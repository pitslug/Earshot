//! ONNX Runtime environment, execution-provider selection and honest backend reporting.
//!
//! - ONNX Runtime is a shared library loaded at run time (ort `load-dynamic`) from Microsoft's
//!   official release, fetched by `cargo xtask fetch-models` ([`ORT_CPU_DYLIB`],
//!   [`ORT_CUDA_DYLIB`] relative to the models directory). [`init`] loads it once per process
//!   and must run before any session is created; nothing falls back to a library found on
//!   `PATH`.
//! - One process-wide [`ort::environment::Environment`] (ONNX Runtime allows only one), built
//!   by [`init`] with **telemetry disabled** (ONNX Runtime enables its Windows ETW telemetry
//!   by default; PLAN.md 3.9 forbids telemetry).
//! - [`OnnxSession::load`] registers the requested [`Provider`] with `error_on_failure`, so a
//!   provider that cannot be initialised is an error, never a silent CPU fallback. Falling
//!   back is the caller's decision (PLAN.md 3.4 auto-selection lives in the runtime).
//! - Which provider actually runs the graph is taken from ONNX Runtime's own node-placement
//!   log for the session (`"All nodes placed on [CPUExecutionProvider]"`,
//!   `"Node(s) placed on [CUDAExecutionProvider]"`), captured with a per-session logger, not
//!   inferred from what was requested. See [`Placement`].

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use earshot_core::asr::BackendKind;
use ort::environment::Environment;
use ort::ep::ExecutionProviderDispatch;
use ort::logging::LogLevel;
use ort::session::Session;

/// ONNX Runtime name of the CPU execution provider.
pub const CPU_EP: &str = "CPUExecutionProvider";
/// ONNX Runtime name of the CUDA execution provider.
pub const CUDA_EP: &str = "CUDAExecutionProvider";
/// ONNX Runtime name of the QNN execution provider.
pub const QNN_EP: &str = "QNNExecutionProvider";

/// ONNX Runtime version these adapters are built and pinned against (models manifest
/// `onnxruntime-win-x64`, ort 2.0.0-rc.14 C API 31).
pub const ORT_VERSION: &str = "1.31.0";
/// The official CPU build of ONNX Runtime for Windows x64, relative to the models directory
/// (manifest id `onnxruntime-win-x64`, set `ci`).
pub const ORT_CPU_DYLIB_X64: &str =
    "onnxruntime/win-x64-1.31.0/onnxruntime-win-x64-1.31.0/lib/onnxruntime.dll";
/// SHA-256 of [`ORT_CPU_DYLIB_X64`], as pinned in `models/manifest.toml`.
pub const ORT_CPU_DYLIB_X64_SHA256: &str =
    "0c42dc4e278f0968fdf9ebf1b1e54a74c657a9942e70803e453c91993451cf4c";
/// The official build of ONNX Runtime for Windows ARM64, relative to the models directory
/// (manifest id `onnxruntime-win-arm64`, sets `arm64` and `qnn`; T8.3).
pub const ORT_CPU_DYLIB_ARM64: &str =
    "onnxruntime/win-arm64-1.31.0/onnxruntime-win-arm64-1.31.0/lib/onnxruntime.dll";
/// SHA-256 of [`ORT_CPU_DYLIB_ARM64`], as pinned in `models/manifest.toml`.
pub const ORT_CPU_DYLIB_ARM64_SHA256: &str =
    "b6da671624bce049c13c34a3b48b7ddfd1b521dbf07d20e83ff9af4441110641";
/// The official CPU build of ONNX Runtime for the architecture this binary was built for
/// ([`ORT_CPU_DYLIB_ARM64`] on ARM64, else [`ORT_CPU_DYLIB_X64`]); the QNN plugin is
/// registered into it ([`crate::qnn`]).
pub const ORT_CPU_DYLIB: &str = if cfg!(target_arch = "aarch64") {
    ORT_CPU_DYLIB_ARM64
} else {
    ORT_CPU_DYLIB_X64
};
/// The `cargo xtask fetch-models` set that provides [`ORT_CPU_DYLIB`].
pub const ORT_CPU_SET: &str = if cfg!(target_arch = "aarch64") {
    "arm64"
} else {
    "ci"
};
/// The official GPU (CUDA 13) build of ONNX Runtime, relative to the models directory
/// (manifest id `onnxruntime-win-x64-gpu-cuda13`, set `cuda`).
pub const ORT_CUDA_DYLIB: &str = "onnxruntime/win-x64-gpu_cuda13-1.31.0/onnxruntime-win-x64-gpu_cuda13-1.31.0/lib/onnxruntime.dll";
/// SHA-256 of [`ORT_CPU_DYLIB`], as pinned in `models/manifest.toml` (checked by the
/// `runtime_dylib_pins_match_the_manifest` test).
pub const ORT_CPU_DYLIB_SHA256: &str = if cfg!(target_arch = "aarch64") {
    ORT_CPU_DYLIB_ARM64_SHA256
} else {
    ORT_CPU_DYLIB_X64_SHA256
};
/// SHA-256 of [`ORT_CUDA_DYLIB`], as pinned in `models/manifest.toml`.
pub const ORT_CUDA_DYLIB_SHA256: &str =
    "00a206c08241370fdc363887428238fab1d926e9eef2539751c07c6e26059f01";

/// Errors of the ONNX adapters.
#[derive(Debug, Clone, PartialEq)]
pub enum OnnxError {
    /// [`init`] has not loaded ONNX Runtime yet.
    NotInitialised,
    /// The ONNX Runtime library could not be loaded (missing, wrong version, or [`init`] was
    /// already called with a different library).
    Library(String),
    /// The provider was requested but this build does not include it (cargo feature off).
    ProviderNotBuilt(Provider),
    /// The provider is built in but could not be registered (driver, DLL or device missing).
    ProviderFailed {
        /// The provider requested.
        provider: Provider,
        /// ONNX Runtime's message.
        message: String,
    },
    /// The model file is missing or could not be loaded.
    Model(String),
    /// The model's inputs or outputs are not what the adapter expects.
    Signature(String),
    /// ONNX Runtime failed at run time.
    Runtime(String),
}

impl fmt::Display for OnnxError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            OnnxError::NotInitialised => {
                f.write_str("ONNX Runtime is not loaded; call earshot_onnx::init first")
            }
            OnnxError::Library(m) => write!(f, "ONNX Runtime library: {m}"),
            OnnxError::ProviderNotBuilt(p) => write!(
                f,
                "execution provider {p} is not built into this binary (enable the `{}` feature of earshot-onnx)",
                p.feature()
            ),
            OnnxError::ProviderFailed { provider, message } => {
                write!(
                    f,
                    "execution provider {provider} failed to initialise: {message}"
                )
            }
            OnnxError::Model(m) => write!(f, "ONNX model error: {m}"),
            OnnxError::Signature(m) => write!(f, "unexpected ONNX model signature: {m}"),
            OnnxError::Runtime(m) => write!(f, "ONNX Runtime error: {m}"),
        }
    }
}

impl std::error::Error for OnnxError {}

pub(crate) fn runtime_err(e: impl fmt::Display) -> OnnxError {
    OnnxError::Runtime(e.to_string())
}

/// An execution provider that can be requested for a session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Provider {
    /// ONNX Runtime's default CPU provider (always built).
    Cpu,
    /// NVIDIA CUDA (cargo feature `cuda`).
    Cuda {
        /// CUDA device ordinal.
        device_id: i32,
    },
    /// Qualcomm QNN HTP (cargo feature `qnn`).
    Qnn,
}

impl Provider {
    /// The cargo feature that builds this provider in (`""` for the CPU).
    #[must_use]
    pub fn feature(self) -> &'static str {
        match self {
            Provider::Cpu => "",
            Provider::Cuda { .. } => "cuda",
            Provider::Qnn => "qnn",
        }
    }

    /// ONNX Runtime's name for the provider.
    #[must_use]
    pub fn ep_name(self) -> &'static str {
        match self {
            Provider::Cpu => CPU_EP,
            Provider::Cuda { .. } => CUDA_EP,
            Provider::Qnn => QNN_EP,
        }
    }

    /// Whether this binary was built with the provider (the cargo feature is on). Says
    /// nothing about whether the hardware or driver is present.
    #[must_use]
    pub fn is_built(self) -> bool {
        match self {
            Provider::Cpu => true,
            Provider::Cuda { .. } => cfg!(feature = "cuda"),
            Provider::Qnn => cfg!(feature = "qnn"),
        }
    }

    fn dispatch(self) -> Result<Option<ExecutionProviderDispatch>, OnnxError> {
        if !self.is_built() {
            return Err(OnnxError::ProviderNotBuilt(self));
        }
        Ok(match self {
            // The CPU provider is always appended by ONNX Runtime; nothing to register.
            Provider::Cpu => None,
            #[cfg(feature = "cuda")]
            Provider::Cuda { device_id } => Some(
                ort::ep::CUDA::default()
                    .with_device_id(device_id)
                    .build()
                    .error_on_failure(),
            ),
            // The QNN plugin goes through the device API, not a provider dispatch
            // (`crate::qnn::with_qnn_device`).
            Provider::Qnn => None,
            #[allow(unreachable_patterns)]
            _ => return Err(OnnxError::ProviderNotBuilt(self)),
        })
    }
}

impl fmt::Display for Provider {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Provider::Cpu => f.write_str("cpu"),
            Provider::Cuda { device_id } => write!(f, "cuda:{device_id}"),
            Provider::Qnn => f.write_str("qnn"),
        }
    }
}

/// Session options.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionConfig {
    /// The execution provider to register (CPU nodes always remain available as ONNX
    /// Runtime's fallback for operators the provider does not support).
    pub provider: Provider,
    /// Intra-op threads (`None` = ONNX Runtime's default, one per physical core).
    pub intra_threads: Option<usize>,
    /// Background session (`None`: a normal one): each of ONNX Runtime's intra-op pool
    /// threads runs this first (for example to lower its OS priority), and the pool does
    /// not spin-wait between operators. Neither changes how work is split between the
    /// threads, so outputs equal a normal session's at the same thread count.
    pub background: Option<ThreadStart>,
}

/// A function an intra-op pool thread runs before any work ([`SessionConfig::background`]).
/// Compared by address.
#[derive(Clone, Copy)]
pub struct ThreadStart(pub fn());

impl fmt::Debug for ThreadStart {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ThreadStart(..)")
    }
}

impl PartialEq for ThreadStart {
    fn eq(&self, other: &Self) -> bool {
        std::ptr::fn_addr_eq(self.0, other.0)
    }
}

impl Eq for ThreadStart {}

/// Spawns ONNX Runtime's pool threads as std threads that run a [`ThreadStart`] first.
struct StartingThreads(ThreadStart);

impl ort::environment::ThreadManager for StartingThreads {
    type Thread = std::thread::JoinHandle<()>;

    fn create(&self, work: impl FnOnce() + Send + 'static) -> ort::Result<Self::Thread> {
        let start = self.0.0;
        std::thread::Builder::new()
            .name("earshot-onnx-pool".into())
            .spawn(move || {
                start();
                work();
            })
            .map_err(|e| ort::Error::new(e.to_string()))
    }

    fn join(thread: Self::Thread) -> ort::Result<()> {
        thread
            .join()
            .map_err(|_| ort::Error::new("an ONNX Runtime pool thread panicked"))
    }
}

impl Default for SessionConfig {
    fn default() -> Self {
        SessionConfig {
            provider: Provider::Cpu,
            intra_threads: None,
            background: None,
        }
    }
}

impl SessionConfig {
    /// CPU with `threads` intra-op threads.
    #[must_use]
    pub fn cpu(threads: usize) -> Self {
        SessionConfig {
            provider: Provider::Cpu,
            intra_threads: Some(threads),
            background: None,
        }
    }
}

/// Where ONNX Runtime placed the graph's nodes, from its own session log.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Placement {
    /// Node count per execution-provider name (for example `"CUDAExecutionProvider" -> 41`).
    pub nodes: BTreeMap<String, usize>,
    /// Placed nodes per provider by operator type, from the node lines that follow each
    /// provider's header (a fused accelerator partition is one node).
    pub ops: BTreeMap<String, BTreeMap<String, usize>>,
    /// The provider whose node lines are being read.
    current: Option<String>,
}

/// `(op type, node name)` of an ONNX Runtime node line `  <op type> (<node name>)`.
fn node_line(message: &str) -> Option<(&str, &str)> {
    let line = message.strip_prefix("  ")?.trim_end();
    let (op, rest) = line.split_once(" (")?;
    let name = rest.strip_suffix(')')?;
    let ident = |s: &str| !s.is_empty() && !s.contains(char::is_whitespace);
    (ident(op) && !name.is_empty()).then_some((op, name))
}

impl Placement {
    /// Parses one ONNX Runtime log line; returns true if it was a placement line.
    ///
    /// ONNX Runtime 1.31 (`onnxruntime/core/framework/session_state_utils` /
    /// `inference_session.cc`, VERBOSE) logs either
    /// `All nodes placed on [CPUExecutionProvider]. Number of nodes: 12` or, per provider,
    /// `Node(s) placed on [CUDAExecutionProvider]. Number of nodes: 30`, each followed by one
    /// line per node, `  <op type> (<node name>)` (recorded in [`Placement::ops`]).
    pub fn observe(&mut self, message: &str) -> bool {
        if let (Some(ep), Some((op, _))) = (&self.current, node_line(message)) {
            *self
                .ops
                .entry(ep.clone())
                .or_default()
                .entry(op.to_owned())
                .or_insert(0) += 1;
            return true;
        }
        self.current = None;
        let Some((_, rest)) = message.split_once("placed on [") else {
            return false;
        };
        let Some((ep, tail)) = rest.split_once(']') else {
            return false;
        };
        let count = tail
            .split_once("Number of nodes:")
            .and_then(|(_, n)| n.split_whitespace().next())
            .and_then(|n| n.parse::<usize>().ok());
        match count {
            Some(n) => {
                *self.nodes.entry(ep.to_owned()).or_insert(0) += n;
                self.current = Some(ep.to_owned());
                true
            }
            None => false,
        }
    }

    /// Nodes placed on `ep`.
    #[must_use]
    pub fn nodes_on(&self, ep: &str) -> usize {
        self.nodes.get(ep).copied().unwrap_or(0)
    }

    /// The operator types placed on `ep` and how many nodes of each.
    #[must_use]
    pub fn ops_on(&self, ep: &str) -> BTreeMap<String, usize> {
        self.ops.get(ep).cloned().unwrap_or_default()
    }
}

/// What actually ran a session: the provider requested and ONNX Runtime's placement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionBackend {
    /// The provider that was requested and registered.
    pub requested: Provider,
    /// Node placement reported by ONNX Runtime.
    pub placement: Placement,
}

impl SessionBackend {
    /// The backend family that ran the graph: the accelerator if ONNX Runtime placed any
    /// node on it, otherwise the CPU. `None` if no placement was logged (then nothing is
    /// claimed).
    #[must_use]
    pub fn kind(&self) -> Option<BackendKind> {
        if self.placement.nodes.is_empty() {
            return None;
        }
        Some(if self.placement.nodes_on(CUDA_EP) > 0 {
            BackendKind::Cuda
        } else if self.placement.nodes_on(QNN_EP) > 0 {
            BackendKind::Qnn
        } else {
            BackendKind::Cpu
        })
    }

    /// Checks the placement of a model meant to run on the requested accelerator against its
    /// documented partition: ONNX Runtime must have placed nodes on the requested provider,
    /// at most `allowed_cpu_nodes` on the CPU provider (the documented exceptions; 0 for a
    /// model that runs whole on the accelerator) and none anywhere else. A session without
    /// a placement log is refused, never assumed.
    ///
    /// # Errors
    /// A message naming the placement and what was allowed.
    pub fn check_partition(&self, allowed_cpu_nodes: usize) -> Result<(), String> {
        let ep = self.requested.ep_name();
        let on_ep = self.placement.nodes_on(ep);
        let on_cpu = if ep == CPU_EP {
            0
        } else {
            self.placement.nodes_on(CPU_EP)
        };
        let elsewhere = self
            .placement
            .nodes
            .iter()
            .any(|(k, n)| k != ep && k != CPU_EP && *n > 0);
        if on_ep == 0 || on_cpu > allowed_cpu_nodes || elsewhere {
            return Err(format!(
                "{self}; the model's partition allows {allowed_cpu_nodes} node(s) on the CPU"
            ));
        }
        Ok(())
    }

    /// [`SessionBackend::check_partition`] by operator: nodes on the requested provider,
    /// none on a third provider, and the nodes left on the CPU exactly itemised in the log
    /// and within `allowed` (`(op type, most nodes)`, the model's documented partition).
    ///
    /// # Errors
    /// A message naming the placement and the CPU operators that are not allowed.
    pub fn check_partition_ops(&self, allowed: &[(&str, usize)]) -> Result<(), String> {
        // Requested provider used, no third provider; the CPU nodes are judged by operator.
        self.check_partition(usize::MAX)?;
        let on_cpu = self.placement.nodes_on(CPU_EP);
        if self.requested.ep_name() == CPU_EP || on_cpu == 0 {
            return Ok(());
        }
        let ops = self.placement.ops_on(CPU_EP);
        if ops.values().sum::<usize>() != on_cpu {
            return Err(format!(
                "{self}; the {on_cpu} CPU node(s) were not itemised in ONNX Runtime's log"
            ));
        }
        let extra: Vec<String> = ops
            .iter()
            .filter(|(op, n)| {
                allowed
                    .iter()
                    .find(|(a, _)| a == op)
                    .is_none_or(|(_, most)| *n > most)
            })
            .map(|(op, n)| format!("{op} x{n}"))
            .collect();
        if extra.is_empty() {
            Ok(())
        } else {
            Err(format!(
                "{self}; CPU nodes outside the model's partition: {}",
                extra.join(", ")
            ))
        }
    }

    /// The device name for reports: the requested provider, with what the QNN device is
    /// (`qnn:npu`, or `qnn:htp-simulator` for Qualcomm's simulator on x64).
    #[must_use]
    pub fn device(&self) -> String {
        match (self.requested, crate::qnn::registered_qnn()) {
            (Provider::Qnn, Some(d)) => match d.hardware {
                crate::qnn::QnnHardware::Npu => "qnn:npu".to_owned(),
                crate::qnn::QnnHardware::Simulator => "qnn:htp-simulator".to_owned(),
            },
            (p, _) => p.to_string(),
        }
    }

    /// True if every node runs on the requested provider.
    #[must_use]
    pub fn fully_on_requested(&self) -> bool {
        let ep = self.requested.ep_name();
        !self.placement.nodes.is_empty() && self.placement.nodes.keys().all(|k| k == ep)
    }
}

impl fmt::Display for SessionBackend {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "requested {}, placed:", self.requested)?;
        if self.placement.nodes.is_empty() {
            return f.write_str(" (not reported)");
        }
        for (ep, n) in &self.placement.nodes {
            write!(f, " {ep}={n}")?;
        }
        Ok(())
    }
}

/// The loaded ONNX Runtime: the library path and the process-wide environment.
struct Loaded {
    library: PathBuf,
    env: Environment,
}

static RUNTIME: OnceLock<Loaded> = OnceLock::new();
/// Serialises [`init`], and holds the library ort has bound (ort keeps the first library it loads
/// for the process and ignores any later path, so a second library must be refused here).
static ORT_BOUND: Mutex<Option<PathBuf>> = Mutex::new(None);
/// `ort::init_from` failed after a clean probe, so ort's library slot is unusable ([`init`]).
static ORT_SLOT_BROKEN: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// What every [`init`] answers once the slot is broken.
const SLOT_BROKEN_MESSAGE: &str = "an earlier load failed; ONNX Runtime needs a restart";
/// The oldest ONNX Runtime minor version [`ORT_VERSION`] accepts (ort's `MINOR_VERSION`, api-31).
const ORT_MINOR: u32 = 31;

/// Lower-case hex SHA-256 of a file.
pub(crate) fn file_sha256(path: &Path) -> std::io::Result<String> {
    use sha2::{Digest, Sha256};
    use std::io::Read;
    let mut f = std::fs::File::open(path)?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 1 << 16];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(h.finalize().iter().map(|b| format!("{b:02x}")).collect())
}

fn one_runtime(requested: &Path, loaded: &Path) -> OnnxError {
    OnnxError::Library(format!(
        "{} requested but {} is already loaded (one ONNX Runtime per process)",
        requested.display(),
        loaded.display()
    ))
}

/// Why [`init`] stops before touching `canonical`, given the slot's state and the library bound to
/// it. A broken slot comes first: nothing is loaded then, whatever the binding says.
fn refused_before_load(broken: bool, bound: Option<&Path>, canonical: &Path) -> Option<OnnxError> {
    if broken {
        return Some(OnnxError::Library(SLOT_BROKEN_MESSAGE.to_owned()));
    }
    bound
        .filter(|held| *held != canonical)
        .map(|held| one_runtime(canonical, held))
}

/// Loads the ONNX Runtime shared library at `library` (normally `<models>/`[`ORT_CPU_DYLIB`]
/// with [`ORT_CPU_DYLIB_SHA256`], or [`ORT_CUDA_DYLIB`] with [`ORT_CUDA_DYLIB_SHA256`]) and
/// creates the process-wide environment with telemetry off. The file's SHA-256 must equal
/// `expected_sha256` (the models-manifest pin) before it is loaded. Idempotent for the same
/// (canonical) path.
///
/// # Errors
/// [`OnnxError::Library`] if the file is missing (with the fetch instruction), does not match
/// the pin, cannot be loaded, is older than ONNX Runtime 1.31, or a different library was
/// loaded already. A failure is not kept: another library can be tried after it.
pub fn init(library: &Path, expected_sha256: &str) -> Result<&'static Environment, OnnxError> {
    if !library.is_file() {
        return Err(OnnxError::Library(format!(
            "{} not found; run `cargo xtask fetch-models --set {ORT_CPU_SET}` (CPU build) or \
             `--set cuda` (GPU build)",
            library.display()
        )));
    }
    let canonical = library
        .canonicalize()
        .map_err(|e| OnnxError::Library(format!("{}: {e}", library.display())))?;
    let mut bound = ORT_BOUND
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(l) = RUNTIME.get() {
        return if l.library == canonical {
            Ok(&l.env)
        } else {
            Err(one_runtime(&canonical, &l.library))
        };
    }
    let broken = ORT_SLOT_BROKEN.load(std::sync::atomic::Ordering::SeqCst);
    if let Some(refused) = refused_before_load(broken, bound.as_deref(), &canonical) {
        return Err(refused);
    }
    let actual = file_sha256(&canonical)
        .map_err(|e| OnnxError::Library(format!("{}: {e}", canonical.display())))?;
    if !actual.eq_ignore_ascii_case(expected_sha256) {
        return Err(OnnxError::Library(format!(
            "{} has SHA-256 {actual}, expected {expected_sha256}; refusing to load it \
             (re-run `cargo xtask fetch-models --verify`)",
            canonical.display()
        )));
    }
    // ort 2.0.0-rc.14's `OnceLock` (`util/once_lock_std.rs`) marks its library slot as set even when
    // the load fails, and every later ort call then reads uninitialised memory. So `init_from` runs
    // only once ort's own checks have passed on a throwaway load, and a refusal here leaves the
    // process free to load another library.
    if ort::util::probe(&canonical).is_none_or(|minor| minor < ORT_MINOR) {
        return Err(OnnxError::Library(format!(
            "{} cannot be loaded or is older than ONNX Runtime {ORT_VERSION}",
            canonical.display()
        )));
    }
    // From here a failure (the file changed since the probe) can never be retried: see above.
    *bound = Some(canonical.clone());
    let builder = ort::init_from(&canonical).map_err(|e| {
        ORT_SLOT_BROKEN.store(true, std::sync::atomic::Ordering::SeqCst);
        OnnxError::Library(e.to_string())
    })?;
    let env = builder
        .with_name("earshot")
        .with_telemetry(false)
        .build()
        .map_err(|e| OnnxError::Library(e.to_string()))?;
    Ok(&RUNTIME
        .get_or_init(|| Loaded {
            library: canonical,
            env,
        })
        .env)
}

/// Loads the library at `path` for the life of the process, so a later load by its bare name finds
/// this copy instead of searching PATH (an execution provider's run-time dependency).
///
/// # Errors
/// [`OnnxError::Library`] if it cannot be loaded.
pub fn preload_library(path: &Path) -> Result<(), OnnxError> {
    ort::util::preload_dylib(path)
        .map_err(|e| OnnxError::Library(format!("{}: {e}", path.display())))
}

/// The ONNX Runtime library loaded by [`init`], if any.
#[must_use]
pub fn loaded_library() -> Option<&'static Path> {
    RUNTIME.get().map(|l| l.library.as_path())
}

/// The process-wide ONNX Runtime environment (telemetry off).
///
/// # Errors
/// [`OnnxError::NotInitialised`] before a successful [`init`].
pub fn environment() -> Result<&'static Environment, OnnxError> {
    RUNTIME
        .get()
        .map(|l| &l.env)
        .ok_or(OnnxError::NotInitialised)
}

/// An ONNX Runtime session plus the backend that actually runs it.
pub struct OnnxSession {
    pub(crate) session: Session,
    backend: SessionBackend,
    /// The weights this session uses instead of its own copy, kept alive with it.
    shared_weights: Option<Arc<crate::shared_weights::Weights>>,
}

impl fmt::Debug for OnnxSession {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OnnxSession")
            .field("backend", &self.backend)
            .finish_non_exhaustive()
    }
}

impl OnnxSession {
    /// Loads `model` with `config`.
    ///
    /// # Errors
    /// [`OnnxError::Model`] if the file is missing or invalid, [`OnnxError::ProviderNotBuilt`]
    /// or [`OnnxError::ProviderFailed`] if the provider cannot be used (no silent fallback).
    pub fn load(model: &Path, config: &SessionConfig) -> Result<Self, OnnxError> {
        // A CPU session shares a model's external weights with the others that use them. If that
        // does not load, the ordinary way decides: saving memory never costs a meeting.
        if config.provider == Provider::Cpu
            && model.is_file()
            && let Some(weights) = crate::shared_weights::for_model(model)
            && let Ok(session) = Self::load_with(model, config, Some(weights), &[])
        {
            return Ok(session);
        }
        Self::load_with(model, config, None, &[])
    }

    /// Evaluation only (T12.12): [`OnnxSession::load`] without weight sharing and with extra
    /// ONNX Runtime session config entries (for example `ep.context_enable`, which makes the
    /// QNN plugin write a compiled context model). No shipped code calls this.
    ///
    /// # Errors
    /// As [`OnnxSession::load`], plus a refused config entry.
    #[doc(hidden)]
    pub fn load_with_session_entries(
        model: &Path,
        config: &SessionConfig,
        entries: &[(&str, &str)],
    ) -> Result<Self, OnnxError> {
        Self::load_with(model, config, None, entries)
    }

    fn load_with(
        model: &Path,
        config: &SessionConfig,
        shared_weights: Option<Arc<crate::shared_weights::Weights>>,
        entries: &[(&str, &str)],
    ) -> Result<Self, OnnxError> {
        if !model.is_file() {
            return Err(OnnxError::Model(format!(
                "{} not found; run `cargo xtask fetch-models --set ci`",
                model.display()
            )));
        }
        let env = environment()?;
        let dispatch = config.provider.dispatch()?;
        let placement = Arc::new(Mutex::new(Placement::default()));
        let sink = Arc::clone(&placement);
        let logger: ort::logging::LoggerFunction = Arc::new(
            move |_level: LogLevel, _cat: &str, _id: &str, _loc: &str, msg: &str| {
                if let Ok(mut p) = sink.lock() {
                    p.observe(msg);
                }
            },
        );
        let mut builder = Session::builder(env)
            .map_err(runtime_err)?
            .with_logger(logger)
            .map_err(runtime_err)?
            .with_log_level(LogLevel::Verbose)
            .map_err(runtime_err)?;
        if let Some(n) = config.intra_threads {
            builder = builder.with_intra_threads(n).map_err(runtime_err)?;
        }
        for (key, value) in entries {
            builder = builder.with_config_entry(key, value).map_err(runtime_err)?;
        }
        if let Some(weights) = &shared_weights {
            for (name, value) in weights.initializers() {
                builder = builder
                    .with_initializer(name, Arc::clone(value))
                    .map_err(runtime_err)?;
            }
            // Prepacking would make each session its own second copy of the weights.
            builder = builder.with_prepacking(false).map_err(runtime_err)?;
        }
        if let Some(start) = config.background {
            builder = builder
                .with_thread_manager(StartingThreads(start))
                .map_err(runtime_err)?
                .with_intra_op_spinning(false)
                .map_err(runtime_err)?;
        }
        // ort 2.0.0-rc.14 gives every new session ONNX Runtime's automatic device selection
        // with the "max efficiency" policy (prefer an NPU). Once the QNN plugin is registered
        // that would silently move a CPU session to QNN, so every session names its devices:
        // the CPU EP's own device for the CPU, the plugin's device for QNN (CUDA is appended
        // explicitly below, which replaces the policy).
        match config.provider {
            Provider::Qnn => builder = crate::qnn::with_qnn_device(builder)?,
            Provider::Cpu => {
                let cpu: Vec<_> = env
                    .devices()
                    .filter(|d| d.ep().ok() == Some(CPU_EP))
                    .collect();
                if cpu.is_empty() {
                    return Err(OnnxError::Runtime(
                        "ONNX Runtime reports no CPU execution-provider device".into(),
                    ));
                }
                builder = builder.with_devices(cpu, None).map_err(runtime_err)?;
            }
            Provider::Cuda { .. } => {}
        }
        if let Some(d) = dispatch {
            builder =
                builder
                    .with_execution_providers([d])
                    .map_err(|e| OnnxError::ProviderFailed {
                        provider: config.provider,
                        message: e.to_string(),
                    })?;
        }
        let session = builder.commit_from_file(model).map_err(|e| {
            let message = format!("{}: {e}", model.display());
            // A provider that registered but cannot run the graph (for example CUDA
            // without cuDNN) fails here; report it as the provider's failure.
            if config.provider != Provider::Cpu
                && (message.contains("Execution Provider") || message.contains("ExecutionProvider"))
            {
                OnnxError::ProviderFailed {
                    provider: config.provider,
                    message,
                }
            } else {
                OnnxError::Model(message)
            }
        })?;
        let placement = placement.lock().map(|p| p.clone()).unwrap_or_default();
        Ok(OnnxSession {
            session,
            backend: SessionBackend {
                requested: config.provider,
                placement,
            },
            shared_weights,
        })
    }

    /// Whether this session shares its weights with others of the same model
    /// ([`crate::shared_weights`]) rather than holding its own copy.
    #[must_use]
    pub fn shares_weights(&self) -> bool {
        self.shared_weights.is_some()
    }

    /// The provider requested and where ONNX Runtime placed the nodes.
    #[must_use]
    pub fn backend(&self) -> &SessionBackend {
        &self.backend
    }

    /// Input names in model order.
    #[must_use]
    pub fn input_names(&self) -> Vec<String> {
        self.session
            .inputs()
            .iter()
            .map(|o| o.name().to_owned())
            .collect()
    }

    /// The declared shape of input `name` (`-1` for a dynamic dimension), `None` if there is
    /// no such tensor input.
    #[must_use]
    pub fn input_shape(&self, name: &str) -> Option<Vec<i64>> {
        self.session
            .inputs()
            .iter()
            .find(|o| o.name() == name)
            .and_then(|o| o.dtype().tensor_shape().map(|s| s.to_vec()))
    }

    /// Output names in model order.
    #[must_use]
    pub fn output_names(&self) -> Vec<String> {
        self.session
            .outputs()
            .iter()
            .map(|o| o.name().to_owned())
            .collect()
    }

    /// The model's custom metadata (sherpa-onnx exports record sample rate, window and
    /// feature settings there).
    ///
    /// # Errors
    /// [`OnnxError::Runtime`] if ONNX Runtime cannot read it.
    pub fn custom_metadata(&self) -> Result<BTreeMap<String, String>, OnnxError> {
        let meta = self.session.metadata().map_err(runtime_err)?;
        let mut out = BTreeMap::new();
        for key in meta.custom_keys().map_err(runtime_err)? {
            if let Some(v) = meta.custom(&key) {
                out.insert(key, v);
            }
        }
        Ok(out)
    }

    /// Fails with [`OnnxError::Signature`] unless the model has exactly these inputs and
    /// outputs, in this order.
    pub(crate) fn expect_signature(
        &self,
        what: &str,
        inputs: &[&str],
        outputs: &[&str],
    ) -> Result<(), OnnxError> {
        let (i, o) = (self.input_names(), self.output_names());
        if i != inputs || o != outputs {
            return Err(OnnxError::Signature(format!(
                "{what}: expected inputs {inputs:?} and outputs {outputs:?}, model has {i:?} and {o:?}"
            )));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn onnx_init_after_a_broken_slot_never_says_another_library_is_loaded() {
        let (first, second) = (Path::new("first.dll"), Path::new("second.dll"));
        let text = |e: Option<OnnxError>| e.map(|e| e.to_string());
        assert!(
            text(refused_before_load(true, Some(first), second))
                .is_some_and(|t| t.contains("needs a restart") && !t.contains("already loaded"))
        );
        assert!(
            text(refused_before_load(true, None, first)).is_some_and(|t| t.contains("restart"))
        );
        // Sound slot: the one-runtime rule, and the same library again goes on.
        assert!(
            text(refused_before_load(false, Some(first), second))
                .is_some_and(|t| t.contains("already loaded"))
        );
        assert!(refused_before_load(false, Some(first), first).is_none());
        assert!(refused_before_load(false, None, first).is_none());
    }

    #[test]
    fn vad_runtime_placement_parses_onnx_runtime_log_lines() {
        let mut p = Placement::default();
        assert!(p.observe("All nodes placed on [CPUExecutionProvider]. Number of nodes: 212"));
        assert!(!p.observe("Removing initializer 'x'. It is no longer used by any node."));
        assert!(!p.observe("placed on [CUDAExecutionProvider] but no count"));
        assert_eq!(p.nodes_on(CPU_EP), 212);
        let mut q = Placement::default();
        assert!(q.observe(" Node(s) placed on [CUDAExecutionProvider]. Number of nodes: 30"));
        assert!(q.observe(" Node(s) placed on [CPUExecutionProvider]. Number of nodes: 4"));
        let b = SessionBackend {
            requested: Provider::Cuda { device_id: 0 },
            placement: q,
        };
        assert_eq!(b.kind(), Some(BackendKind::Cuda));
        assert!(!b.fully_on_requested(), "4 nodes run on the CPU");
        let none = SessionBackend {
            requested: Provider::Cpu,
            placement: Placement::default(),
        };
        assert_eq!(
            none.kind(),
            None,
            "nothing is claimed without a placement log"
        );
        assert!(!none.fully_on_requested());
    }

    #[test]
    fn vad_runtime_cpu_only_placement_is_reported_as_cpu() {
        let mut p = Placement::default();
        p.observe("All nodes placed on [CPUExecutionProvider]. Number of nodes: 5");
        let b = SessionBackend {
            requested: Provider::Cuda { device_id: 0 },
            placement: p,
        };
        assert_eq!(b.kind(), Some(BackendKind::Cpu));
        assert!(!b.fully_on_requested());
    }

    #[test]
    fn vad_runtime_missing_model_is_a_model_error_with_fetch_hint() {
        let err = OnnxSession::load(Path::new("does/not/exist.onnx"), &SessionConfig::default())
            .unwrap_err();
        match err {
            OnnxError::Model(m) => assert!(m.contains("cargo xtask fetch-models"), "{m}"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn vad_runtime_missing_library_is_an_error_with_fetch_hint() {
        let err = init(Path::new("does/not/onnxruntime.dll"), ORT_CPU_DYLIB_SHA256).unwrap_err();
        match err {
            OnnxError::Library(m) => assert!(m.contains("cargo xtask fetch-models"), "{m}"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn vad_runtime_refuses_a_library_that_does_not_match_its_pin() {
        let dir = std::env::temp_dir().join(format!("earshot-onnx-pin-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let fake = dir.join("onnxruntime.dll");
        std::fs::write(&fake, b"not onnxruntime").unwrap();
        let err = init(&fake, ORT_CPU_DYLIB_SHA256).unwrap_err();
        match err {
            OnnxError::Library(m) => assert!(m.contains("refusing to load"), "{m}"),
            other => panic!("{other:?}"),
        }
        assert!(loaded_library().is_none(), "nothing was loaded");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(not(feature = "cuda"))]
    #[test]
    fn vad_runtime_cuda_without_feature_is_an_error_not_a_cpu_fallback() {
        let p = Provider::Cuda { device_id: 0 };
        assert!(!p.is_built());
        assert_eq!(p.dispatch().err(), Some(OnnxError::ProviderNotBuilt(p)));
    }

    /// Placement lines recorded on the desktop (2026-10-24): Silero VAD (float, dynamic
    /// shapes) loaded with the QNN plugin 2.7.0 on Qualcomm's HTP simulator in ONNX Runtime
    /// 1.31; the QNN EP took one node of 212.
    const QNN_SILERO_PLACEMENT: [&str; 3] = [
        " Node(s) placed on [CPUExecutionProvider]. Number of nodes: 211",
        " Node(s) placed on [QNNExecutionProvider]. Number of nodes: 1",
        "  QNNExecutionProvider_14446177244157773527_1 (QNNExecutionProvider_QNNExecutionProvider_14446177244157773527_1_0)",
    ];

    fn qnn_backend(lines: &[&str]) -> SessionBackend {
        let mut p = Placement::default();
        for l in lines {
            p.observe(l);
        }
        SessionBackend {
            requested: Provider::Qnn,
            placement: p,
        }
    }

    #[test]
    fn vad_runtime_qnn_partition_refuses_cpu_nodes_beyond_the_documented_ones() {
        let b = qnn_backend(&QNN_SILERO_PLACEMENT);
        assert_eq!(b.placement.nodes_on(QNN_EP), 1);
        assert_eq!(b.placement.nodes_on(CPU_EP), 211);
        assert_eq!(
            b.kind(),
            Some(BackendKind::Qnn),
            "any QNN node makes it QNN"
        );
        let err = b.check_partition(0).unwrap_err();
        assert!(err.contains("CPUExecutionProvider=211"), "{err}");
        assert!(b.check_partition(210).is_err());
        assert!(b.check_partition(211).is_ok(), "a documented partition");
    }

    #[test]
    fn vad_runtime_placement_records_the_operators_of_each_provider() {
        // ONNX Runtime 1.31 logs each placed node as "  <op type> (<node name>)" after its
        // provider's header line.
        let mut p = Placement::default();
        for l in [
            " Node(s) placed on [CPUExecutionProvider]. Number of nodes: 2",
            "  Cast (/Cast_3)",
            "  Gather (/Gather_1)",
            " Node(s) placed on [QNNExecutionProvider]. Number of nodes: 1",
            "  QNNExecutionProvider_14446177244157773527_1 (QNNExecutionProvider_QNNExecutionProvider_14446177244157773527_1_0)",
            "Removing initializer 'x'. It is no longer used by any node.",
            "  Cast (/not_a_placement)",
        ] {
            p.observe(l);
        }
        assert_eq!(p.nodes_on(CPU_EP), 2);
        assert_eq!(
            p.ops_on(CPU_EP),
            BTreeMap::from([("Cast".to_owned(), 1), ("Gather".to_owned(), 1)])
        );
        assert_eq!(p.ops_on(QNN_EP).values().sum::<usize>(), 1);
        assert!(p.ops_on(CUDA_EP).is_empty());
    }

    #[test]
    fn vad_runtime_qnn_partition_by_operator_allows_only_the_documented_ones() {
        let mut b = qnn_backend(&[
            " Node(s) placed on [CPUExecutionProvider]. Number of nodes: 2",
            "  Cast (/Cast_3)",
            "  Gather (/Gather_1)",
            " Node(s) placed on [QNNExecutionProvider]. Number of nodes: 1",
            "  QNNExecutionProvider_1 (QNNExecutionProvider_QNNExecutionProvider_1_0)",
        ]);
        assert!(b.check_partition_ops(&[("Cast", 1), ("Gather", 1)]).is_ok());
        assert!(b.check_partition_ops(&[("Cast", 2), ("Gather", 1)]).is_ok());
        let err = b.check_partition_ops(&[("Cast", 1)]).unwrap_err();
        assert!(err.contains("Gather"), "{err}");
        assert!(b.check_partition_ops(&[]).is_err());
        // A placement whose CPU nodes were not itemised is refused, not assumed allowed.
        b.placement.ops.clear();
        assert!(
            b.check_partition_ops(&[("Cast", 1), ("Gather", 1)])
                .is_err()
        );
    }

    #[test]
    fn vad_runtime_qnn_partition_accepts_a_model_wholly_on_qnn() {
        let b = qnn_backend(&[" Node(s) placed on [QNNExecutionProvider]. Number of nodes: 1"]);
        assert!(b.check_partition(0).is_ok());
        assert!(b.fully_on_requested());
    }

    #[test]
    fn vad_runtime_qnn_partition_refuses_cpu_only_and_unlogged_sessions() {
        let all_cpu =
            qnn_backend(&["All nodes placed on [CPUExecutionProvider]. Number of nodes: 5"]);
        assert!(
            all_cpu.check_partition(usize::MAX).is_err(),
            "nothing on QNN"
        );
        let unlogged = qnn_backend(&[]);
        assert!(unlogged.check_partition(usize::MAX).is_err());
        let mut mixed =
            qnn_backend(&[" Node(s) placed on [QNNExecutionProvider]. Number of nodes: 3"]);
        mixed
            .placement
            .observe(" Node(s) placed on [CUDAExecutionProvider]. Number of nodes: 1");
        assert!(
            mixed.check_partition(usize::MAX).is_err(),
            "a third provider"
        );
    }

    #[test]
    fn vad_runtime_cpu_partition_is_every_node_on_the_cpu() {
        let mut p = Placement::default();
        p.observe("All nodes placed on [CPUExecutionProvider]. Number of nodes: 5");
        let b = SessionBackend {
            requested: Provider::Cpu,
            placement: p,
        };
        assert!(b.check_partition(0).is_ok());
    }

    #[cfg(not(feature = "qnn"))]
    #[test]
    fn vad_runtime_qnn_without_feature_is_an_error() {
        assert_eq!(
            Provider::Qnn.dispatch().err(),
            Some(OnnxError::ProviderNotBuilt(Provider::Qnn))
        );
    }
}
