//! NPU encoders loaded from a context model compiled on this computer (T12.12a,
//! docs/live-previews.md).
//!
//! The QNN plugin compiles an NPU conversion's QDQ encoder at every load: about 30 s and about
//! 2 GiB of memory per session for Parakeet 0.6B v2. A context model the plugin wrote once
//! (`earshot_onnx::OnnxSession::load_writing_context`) loads in about a second and costs about
//! 0.13 GiB per extra session, with the same words, times and probabilities (T12.12). This module
//! keeps one per NPU conversion; `backends::load_parakeet` is its only caller.
//!
//! - **Where:** `<conversion>/compiled/<key>/` (`parakeet-qnn/<model>/compiled/...`), so the
//!   catalogue removes it with the model and a new download of the NPU version replaces it.
//!   `<key>` is [`CompileKey::folder_name`], so a load never reads or replaces a folder made for
//!   anything else.
//! - **What makes it current:** [`CompileKey`]: this module's [`FORMAT`], the encoder's
//!   `encoder.onnx` and `encoder.weights` pins from `conversion.json` (checked before every load by
//!   `backends::verify_qnn_conversion`), every pin of the QNN package, the ONNX Runtime library's
//!   pin, and the NPU's name and driver version. A change to any of them is a new folder, compiled
//!   at the next load without a note; the old one is swept.
//! - **What every load checks:** the record [`RECORD`] (the key text, and the size and SHA-256 of
//!   every file the plugin wrote; no other file allowed), each file against it, then the plugin's
//!   own answer (`earshot_onnx::qnn::compiled_model_fit`), then everything an ordinary load checks
//!   (the Parakeet signature and metadata, the static input, the NPU partition).
//! - **What happens otherwise:** with no current folder, the load is the ordinary one and writes
//!   the context model as it compiles. A folder that fails any check is put aside and compiled
//!   again the same way; a compile that fails is the ordinary load without writing. Saving memory
//!   never costs a meeting: only a failure of the ordinary load fails the load. A refusal (any
//!   load slower than the compiled one, except the first compile of a key) is reported once per
//!   process ([`take_note`]).
//! - **Previews** ([`Compile::Never`], T12.12e): only a current folder as it is; a preview never
//!   compiles, never takes the ordinary load and never puts a refused folder aside (the track's
//!   load replaces it).
//! - **After the fact** (T12.12e): every refusal's [`Step`]s, path-free, wait in
//!   [`take_unstored`] for the host to keep in device record `npu-compiled` ([`RefusalRecord`]).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use serde::{Deserialize, Serialize};

/// The folder of compiled models inside an NPU conversion's folder.
pub const COMPILED_DIR: &str = "compiled";
/// The record of a compiled folder.
pub const RECORD: &str = "compiled.json";
/// The context model the plugin writes (its binary beside it is named after it).
pub const CONTEXT_MODEL: &str = "encoder_ctx.onnx";
/// The version of the key and record; a change compiles every model again.
pub const FORMAT: u32 = 1;
/// A temp folder older than this is a crash's leftover (a compile takes about 30 s).
const STALE_TEMP: Duration = Duration::from_secs(60 * 60);

/// The banner text of a refusal, fixed copy ([`take_note`]).
pub const NOTE: &str = "The NPU could not use the copy of the speech model Earshot prepared for \
                        it, so it loaded the slower way this time (about half a minute and more \
                        memory). Earshot prepares it again by itself.";

/// What a compiled folder was made from ([`CompileKey::for_conversion`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompileKey {
    text: String,
}

impl CompileKey {
    /// The key of the given parts: `encoder` the `(file, SHA-256)` pins of the encoder's files,
    /// `qnn` the QNN package, `onnx_runtime` the loaded ONNX Runtime library's SHA-256, `npus` each
    /// NPU's name and driver version.
    #[must_use]
    pub fn from_parts(
        encoder: &[(&str, &str)],
        qnn: &earshot_onnx::qnn::QnnPackage,
        onnx_runtime: &str,
        npus: &[earshot_hardware::Npu],
    ) -> Self {
        let mut lines = vec![format!("format {FORMAT}")];
        for (file, sha) in encoder {
            lines.push(format!("encoder {file} {}", sha.to_ascii_lowercase()));
        }
        lines.push(format!(
            "qnn {} {}",
            earshot_onnx::qnn::QNN_PROVIDER_LIB,
            qnn.provider_sha256
        ));
        lines.push(format!(
            "qnn {} {}",
            earshot_onnx::qnn::QNN_HTP_LIB,
            qnn.htp_sha256
        ));
        for (file, sha) in qnn.libraries {
            lines.push(format!("qnn {file} {sha}"));
        }
        lines.push(format!("onnxruntime {}", onnx_runtime.to_ascii_lowercase()));
        if npus.is_empty() {
            lines.push("npu none".to_owned());
        }
        for npu in npus {
            lines.push(format!(
                "npu {} driver {}",
                npu.name,
                npu.driver_version.as_deref().unwrap_or("unknown")
            ));
        }
        Self {
            text: lines.join("\n"),
        }
    }

    /// The key of the NPU conversion in `dir` on this computer, now: its `conversion.json` pins of
    /// `encoder.onnx` and `encoder.weights`, the QNN package of this architecture, the pin of the
    /// ONNX Runtime library QNN runs in, and the NPUs `earshot_hardware` reads. Nothing needs to be
    /// loaded for it.
    ///
    /// # Errors
    /// An unreadable `conversion.json` or one without those pins.
    pub fn for_conversion(dir: &Path) -> Result<Self, String> {
        let text = std::fs::read_to_string(dir.join("conversion.json"))
            .map_err(|e| format!("conversion.json: {e}"))?;
        let record: serde_json::Value =
            serde_json::from_str(&text).map_err(|e| format!("conversion.json: {e}"))?;
        let pin = |file: &str| {
            record["outputs"][file]["sha256"]
                .as_str()
                .map(str::to_owned)
                .ok_or_else(|| format!("conversion.json has no SHA-256 for {file}"))
        };
        let (onnx, weights) = (pin("encoder.onnx")?, pin("encoder.weights")?);
        Ok(Self::from_parts(
            &[("encoder.onnx", &onnx), ("encoder.weights", &weights)],
            &earshot_onnx::qnn::QNN_HOST,
            // QNN always runs in this library (`backends::init_onnx`), checked against this pin.
            earshot_onnx::ORT_CPU_DYLIB_SHA256,
            &crate::hardware().npus,
        ))
    }

    /// The key's text (one fact per line), as the record stores it.
    #[must_use]
    pub fn text(&self) -> &str {
        &self.text
    }

    /// The folder name: the first 16 hex digits of the text's SHA-256.
    #[must_use]
    pub fn folder_name(&self) -> String {
        use sha2::{Digest, Sha256};
        Sha256::digest(self.text.as_bytes())
            .iter()
            .take(8)
            .map(|b| format!("{b:02x}"))
            .collect()
    }
}

/// The record of a compiled folder ([`RECORD`]).
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Record {
    format: u32,
    key: String,
    files: BTreeMap<String, FilePin>,
}

#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct FilePin {
    size: u64,
    sha256: String,
}

/// Every entry of `folder` but the record, as `(name, is a file)`.
fn entries(folder: &Path) -> Result<Vec<(String, bool)>, String> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir(folder).map_err(|e| e.to_string())? {
        let entry = entry.map_err(|e| e.to_string())?;
        let name = entry
            .file_name()
            .into_string()
            .map_err(|n| format!("{} is not a plain name", n.to_string_lossy()))?;
        if name != RECORD {
            let is_file = entry.file_type().map_err(|e| e.to_string())?.is_file();
            out.push((name, is_file));
        }
    }
    Ok(out)
}

fn sha256_of(path: &Path) -> Result<String, String> {
    use sha2::{Digest, Sha256};
    use std::io::Read;
    let mut file = std::fs::File::open(path).map_err(|e| e.to_string())?;
    let mut hash = Sha256::new();
    let mut buf = vec![0u8; 1 << 16];
    loop {
        let n = file.read(&mut buf).map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        hash.update(&buf[..n]);
    }
    Ok(hash.finalize().iter().map(|b| format!("{b:02x}")).collect())
}

/// Writes the record of what the plugin wrote into `folder`, for `key`.
fn write_record(folder: &Path, key: &CompileKey) -> Result<(), String> {
    let mut files = BTreeMap::new();
    for (name, is_file) in entries(folder)? {
        if !is_file {
            return Err(format!("the compile wrote a folder ({name})"));
        }
        let path = folder.join(&name);
        let size = std::fs::metadata(&path).map_err(|e| e.to_string())?.len();
        let sha256 = sha256_of(&path)?;
        files.insert(name, FilePin { size, sha256 });
    }
    if !files.contains_key(CONTEXT_MODEL) {
        return Err(format!("the compile wrote no {CONTEXT_MODEL}"));
    }
    let record = Record {
        format: FORMAT,
        key: key.text.clone(),
        files,
    };
    let json = serde_json::to_vec_pretty(&record).map_err(|e| e.to_string())?;
    std::fs::write(folder.join(RECORD), json).map_err(|e| format!("{RECORD}: {e}"))
}

/// The context model of `folder` when its record is for `key` and every file matches it.
///
/// # Errors
/// Why the folder cannot be used, in a few words.
fn verify(folder: &Path, key: &CompileKey) -> Result<PathBuf, String> {
    let text = std::fs::read_to_string(folder.join(RECORD))
        .map_err(|e| format!("its record cannot be read ({e})"))?;
    let record: Record =
        serde_json::from_str(&text).map_err(|e| format!("its record is damaged ({e})"))?;
    if record.format != FORMAT || record.key != key.text {
        return Err("its record is for something else".to_owned());
    }
    if !record.files.contains_key(CONTEXT_MODEL) {
        return Err(format!("its record lists no {CONTEXT_MODEL}"));
    }
    for (name, is_file) in entries(folder).map_err(|e| format!("it cannot be listed ({e})"))? {
        if !is_file || !record.files.contains_key(&name) {
            return Err(format!("it holds {name}, which its record does not list"));
        }
    }
    for (name, pin) in &record.files {
        let path = folder.join(name);
        let size = std::fs::metadata(&path)
            .map_err(|e| format!("{name} cannot be read ({e})"))?
            .len();
        if size != pin.size {
            return Err(format!(
                "{name} is {size} bytes, its record says {}",
                pin.size
            ));
        }
        let sha = sha256_of(&path).map_err(|e| format!("{name} cannot be read ({e})"))?;
        if !sha.eq_ignore_ascii_case(&pin.sha256) {
            return Err(format!("{name} differs from its record"));
        }
    }
    Ok(folder.join(CONTEXT_MODEL))
}

static SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// A name in `root` no other load of this process uses.
fn unique(root: &Path, prefix: &str) -> PathBuf {
    root.join(format!(
        "{prefix}-{}-{}",
        std::process::id(),
        SEQUENCE.fetch_add(1, Ordering::Relaxed)
    ))
}

/// Puts `folder` aside under a new name and deletes it there. Only the rename must succeed: a
/// folder whose files another process has mapped refuses it whole, with every file in place.
fn put_aside(root: &Path, folder: &Path) -> Result<(), String> {
    let aside = unique(root, ".old");
    std::fs::rename(folder, &aside).map_err(|e| e.to_string())?;
    let _ = std::fs::remove_dir_all(&aside);
    Ok(())
}

/// Moves the finished `tmp` to `dest`. Another process that published the same key first wins:
/// ours is deleted and that is no failure.
fn publish(tmp: &Path, dest: &Path) -> Result<(), String> {
    match std::fs::rename(tmp, dest) {
        Ok(()) => Ok(()),
        Err(_) if dest.join(RECORD).is_file() => {
            let _ = std::fs::remove_dir_all(tmp);
            Ok(())
        }
        Err(e) => Err(e.to_string()),
    }
}

/// Removes, best effort, every folder in `root` but `keep`: other keys' folders and leftovers put
/// aside (each renamed aside first, so a folder in use elsewhere stays whole), and temp folders
/// older than [`STALE_TEMP`] (a younger one may be another process's compile).
fn sweep(root: &Path, keep: &Path) {
    let Ok(list) = std::fs::read_dir(root) else {
        return;
    };
    for entry in list.flatten() {
        let path = entry.path();
        if path == keep || !path.is_dir() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with(".tmp-") {
            let stale = entry
                .metadata()
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| t.elapsed().ok())
                .is_some_and(|age| age > STALE_TEMP);
            if stale {
                let _ = std::fs::remove_dir_all(&path);
            }
        } else if name.starts_with(".old-") {
            let _ = std::fs::remove_dir_all(&path);
        } else {
            let _ = put_aside(root, &path);
        }
    }
}

/// Which step of a load refused the compiled model, or made a load slower than it (T12.12e). Each
/// has fixed copy and a word ([`Step::copy`], [`Step::word`]) with no path or error text, so it
/// can be kept where a later session reads it ([`RefusalRecord`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Step {
    /// The conversion's record gave no key ([`CompileKey::for_conversion`]).
    NoKey,
    /// There was no compiled model for the key (a preview, which never compiles).
    NotCompiled,
    /// The compiled folder's files did not match its record ([`RECORD`]).
    Verify,
    /// The plugin asked for a new compile or could not judge the compiled model.
    NpuFit,
    /// The plugin did not load the compiled model.
    CompiledLoad,
    /// A refused folder could not be put aside.
    Replace,
    /// There was no folder to compile into.
    NoTempFolder,
    /// Compiling in this process failed.
    Compile,
    /// The helper process did not compile.
    Helper,
    /// The helper's compiled model did not load.
    HelperLoad,
    /// What was compiled could not be kept for the next load.
    Keep,
}

impl Step {
    /// Its word in [`RefusalRecord`].
    #[must_use]
    pub fn word(self) -> &'static str {
        match self {
            Step::NoKey => "no-key",
            Step::NotCompiled => "not-compiled",
            Step::Verify => "verify",
            Step::NpuFit => "npu-fit",
            Step::CompiledLoad => "compiled-load",
            Step::Replace => "replace",
            Step::NoTempFolder => "no-temp-folder",
            Step::Compile => "compile",
            Step::Helper => "helper",
            Step::HelperLoad => "helper-load",
            Step::Keep => "keep",
        }
    }

    /// Fixed copy saying what happened, with no path, file name or error text.
    #[must_use]
    pub fn copy(self) -> &'static str {
        match self {
            Step::NoKey => "the NPU version's record could not be read",
            Step::NotCompiled => "there was no prepared copy yet",
            Step::Verify => "the prepared copy's files did not match its record",
            Step::NpuFit => "the NPU asked for the copy to be prepared again",
            Step::CompiledLoad => "the NPU did not load the prepared copy",
            Step::Replace => "the refused copy could not be replaced",
            Step::NoTempFolder => "there was no folder to prepare a copy in",
            Step::Compile => "preparing a copy in Earshot failed",
            Step::Helper => "the helper that prepares the copy failed",
            Step::HelperLoad => "the copy the helper prepared did not load",
            Step::Keep => "the prepared copy could not be kept",
        }
    }
}

/// Why a compiled folder was not used: its [`Step`], and a few words that may name paths (for
/// reports and tests only: never stored or shown).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refusal {
    /// The step that refused it.
    pub step: Step,
    /// What it said.
    pub why: String,
}

impl Refusal {
    fn new(step: Step, why: impl Into<String>) -> Self {
        Self {
            step,
            why: why.into(),
        }
    }
}

/// How [`load`] loaded the encoder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum How {
    /// From the compiled context model.
    Compiled,
    /// The ordinary load, writing the context model for the next one (in process).
    Compiling {
        /// Why a compiled folder for this key was not used (`None`: there was none).
        refused: Option<Refusal>,
        /// Whether what was written is kept (`Err`: why not).
        kept: Result<(), String>,
    },
    /// Compiled by the helper process, then loaded from what it wrote.
    CompiledByHelper {
        /// Why a compiled folder for this key was not used (`None`: there was none).
        refused: Option<Refusal>,
    },
    /// The ordinary load, without writing.
    Ordinary {
        /// The steps that led to it, first first (never empty).
        steps: Vec<Step>,
        /// Why.
        why: String,
    },
}

impl How {
    /// Whether this load is reported ([`record`]): anything but the compiled model and the first
    /// compile of a key that was kept.
    #[must_use]
    pub fn is_refusal(&self) -> bool {
        !self.steps().is_empty()
    }

    /// The steps that made this load other than the compiled one or a kept first compile, first
    /// first (empty: no refusal).
    #[must_use]
    pub fn steps(&self) -> Vec<Step> {
        match self {
            How::Compiled => Vec::new(),
            How::Compiling { refused, kept } => refused
                .iter()
                .map(|r| r.step)
                .chain(kept.is_err().then_some(Step::Keep))
                .collect(),
            How::CompiledByHelper { refused } => refused.iter().map(|r| r.step).collect(),
            How::Ordinary { steps, .. } => steps.clone(),
        }
    }
}

/// [`Compile::InProcess`]: the load writing the context model at the path given.
pub type CompileInProcess<'a, S, E> = Box<dyn FnOnce(&Path) -> Result<S, E> + 'a>;

/// [`Compile::Helper`]: another process writing the context model at the path given.
pub type CompileInHelper<'a> = Box<dyn FnOnce(&Path) -> Result<(), String> + 'a>;

/// How [`load`] compiles a model it has no current folder for.
pub enum Compile<'a, S, E> {
    /// The ordinary load, writing the context model at the path given; the session that wrote it
    /// is the one returned (and, as every QNN session, never closed: `earshot_onnx::runtime`).
    InProcess(CompileInProcess<'a, S, E>),
    /// Another process writes the context model at the path given and ends; this process then
    /// loads it like any compiled model ([`helper_compile`]).
    Helper(CompileInHelper<'a>),
    /// Only a current compiled folder as it is (a preview, T12.12c, T12.12e): nothing compiles,
    /// nothing is put aside, the ordinary load is never taken; the error is made from the steps
    /// that refused.
    Never(RefuseLoad<'a, E>),
}

/// [`Compile::Never`]: the error of a load that found no usable compiled folder, from its steps.
pub type RefuseLoad<'a, E> = Box<dyn FnOnce(Vec<Step>) -> E + 'a>;

/// One NPU encoder load at a time in this process, so two sessions of a model never compile it
/// together (the second waits about 30 s, then loads the first one's compile in about a second).
static LOADS: Mutex<()> = Mutex::new(());

/// Loads the NPU conversion in `conversion` for `key` (module docs), returning the loaded value
/// and how. `fit` is the plugin's answer for a context model (true: use it); `compiled` loads
/// everything from a context model, `compile` says how a missing one is made, and `ordinary` is
/// the ordinary load; each runs every check of a load, so any refusal is theirs to give.
///
/// # Errors
/// Only `ordinary`'s.
pub fn load<S, E: std::fmt::Display>(
    conversion: &Path,
    key: &CompileKey,
    fit: impl FnOnce(&Path) -> Result<bool, String>,
    mut compiled: impl FnMut(&Path) -> Result<S, E>,
    compile: Compile<'_, S, E>,
    ordinary: impl FnOnce() -> Result<S, E>,
) -> Result<(S, How), E> {
    let _one = LOADS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let root = conversion.join(COMPILED_DIR);
    let dest = root.join(key.folder_name());
    let mut refused: Option<Refusal> = None;
    if dest.exists() {
        let usable = verify(&dest, key)
            .map_err(|why| Refusal::new(Step::Verify, why))
            .and_then(|model| match fit(&model) {
                Ok(true) => Ok(model),
                Ok(false) => Err(Refusal::new(Step::NpuFit, "the NPU asks for a new compile")),
                Err(e) => Err(Refusal::new(
                    Step::NpuFit,
                    format!("the NPU cannot judge it ({e})"),
                )),
            });
        match usable {
            Ok(model) => match compiled(&model) {
                Ok(s) => return Ok((s, How::Compiled)),
                Err(e) => {
                    refused = Some(Refusal::new(
                        Step::CompiledLoad,
                        format!("its load failed ({e})"),
                    ));
                }
            },
            Err(r) => refused = Some(r),
        }
    }
    // A preview changes nothing on disk (T12.12e): a refused folder stays for the track's load.
    let compile = match compile {
        Compile::Never(refuse) => {
            let step = refused.map_or(Step::NotCompiled, |r| r.step);
            return Err(refuse(vec![step]));
        }
        other => other,
    };
    let first: Vec<Step> = refused.iter().map(|r| r.step).collect();
    let ordinary_after = |mut steps: Vec<Step>, step: Step, why: String| {
        steps.push(step);
        ordinary().map(|s| (s, How::Ordinary { steps, why }))
    };
    if refused.is_some()
        && let Err(e) = put_aside(&root, &dest)
    {
        let why = format!(
            "the compiled model was refused ({}) and cannot be replaced ({e})",
            refused.map(|r| r.why).unwrap_or_default()
        );
        return ordinary_after(first, Step::Replace, why);
    }
    let tmp = unique(&root, ".tmp");
    if let Err(e) = std::fs::create_dir_all(&tmp) {
        let why = format!("no folder to compile into ({e})");
        return ordinary_after(first, Step::NoTempFolder, why);
    }
    let failed = |refused: &Option<Refusal>, e: String| match refused {
        Some(r) => format!(
            "the compiled model was refused ({}) and compiling failed ({e})",
            r.why
        ),
        None => format!("compiling failed ({e})"),
    };
    match compile {
        Compile::InProcess(compile) => match compile(&tmp.join(CONTEXT_MODEL)) {
            Ok(s) => {
                let kept = write_record(&tmp, key).and_then(|()| publish(&tmp, &dest));
                if kept.is_ok() {
                    sweep(&root, &dest);
                } else {
                    let _ = std::fs::remove_dir_all(&tmp);
                }
                Ok((s, How::Compiling { refused, kept }))
            }
            Err(e) => {
                let _ = std::fs::remove_dir_all(&tmp);
                let why = failed(&refused, e.to_string());
                ordinary_after(first, Step::Compile, why)
            }
        },
        Compile::Helper(helper) => {
            if let Err(e) = made_by_helper(&root, &tmp, &dest, key, helper) {
                let why = failed(&refused, e);
                return ordinary_after(first, Step::Helper, why);
            }
            // What another process wrote is checked like any compiled folder before it loads.
            match verify(&dest, key).and_then(|model| compiled(&model).map_err(|e| e.to_string())) {
                Ok(s) => Ok((s, How::CompiledByHelper { refused })),
                Err(e) => {
                    let why = format!("the helper's compiled model did not load ({e})");
                    ordinary_after(first, Step::HelperLoad, why)
                }
            }
        }
        Compile::Never(_) => unreachable!("handled above"),
    }
}

/// Has `helper` write into the empty `tmp`, records it for `key` and publishes it at `dest`, then
/// sweeps `root`; on any failure removes `tmp`.
fn made_by_helper(
    root: &Path,
    tmp: &Path,
    dest: &Path,
    key: &CompileKey,
    helper: impl FnOnce(&Path) -> Result<(), String>,
) -> Result<(), String> {
    let made = helper(&tmp.join(CONTEXT_MODEL))
        .and_then(|()| write_record(tmp, key))
        .and_then(|()| publish(tmp, dest));
    match made {
        Ok(()) => {
            sweep(root, dest);
            Ok(())
        }
        Err(e) => {
            let _ = std::fs::remove_dir_all(tmp);
            Err(e)
        }
    }
}

/// What [`prepare`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Prepared {
    /// A current, intact compiled folder was there already.
    AlreadyCompiled,
    /// The helper compiled it now (`replaced`: why a folder for the key was not usable).
    Compiled {
        /// Why a folder for this key was put aside (`None`: there was none).
        replaced: Option<String>,
    },
}

/// Compiles the NPU conversion in `conversion` for `key` ahead of any load (T12.12b, the app's
/// start-up), with `helper` only: this process loads nothing. Under the same lock as [`load`], so
/// a load that starts meanwhile waits and then finds it compiled. A folder already there is checked
/// like a load checks it, without the plugin's answer (which needs the plugin loaded); a load still
/// asks the plugin.
///
/// # Errors
/// Why nothing usable was compiled; a load then compiles as it would have.
pub fn prepare(
    conversion: &Path,
    key: &CompileKey,
    helper: impl FnOnce(&Path) -> Result<(), String>,
) -> Result<Prepared, String> {
    let _one = LOADS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let root = conversion.join(COMPILED_DIR);
    let dest = root.join(key.folder_name());
    let mut replaced = None;
    if dest.exists() {
        match verify(&dest, key) {
            Ok(_) => return Ok(Prepared::AlreadyCompiled),
            Err(why) => replaced = Some(why),
        }
        put_aside(&root, &dest).map_err(|e| format!("the folder cannot be replaced ({e})"))?;
    }
    let tmp = unique(&root, ".tmp");
    std::fs::create_dir_all(&tmp).map_err(|e| format!("no folder to compile into ({e})"))?;
    made_by_helper(&root, &tmp, &dest, key, helper)?;
    Ok(Prepared::Compiled { replaced })
}

/// The hidden mode of an exe that compiles for [`load`] ([`use_this_exe`], [`child_main`]).
pub const COMPILE_FLAG: &str = "--npu-compile";

/// How long a helper may take: a compile takes about 30 s (0.6B v2); a judgement with room for a
/// busy machine.
pub const HELPER_DEADLINE: Duration = Duration::from_secs(10 * 60);

static HELPER: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();

/// Makes this exe the helper of every later compile ([`Compile::Helper`]). The exe must call
/// [`child_main`] first thing in `main`. If the exe cannot find itself nothing changes: compiles
/// stay in process.
pub fn use_this_exe() {
    if let Ok(exe) = std::env::current_exe() {
        let _ = HELPER.set(exe);
    }
}

/// The helper [`use_this_exe`] registered.
#[must_use]
pub fn helper() -> Option<&'static Path> {
    HELPER.get().map(PathBuf::as_path)
}

/// The arguments [`helper_compile`] gives the helper, after the program: [`COMPILE_FLAG`] first, as
/// [`child_main`] reads them.
#[must_use]
pub fn helper_args(models: &Path, model: &str, out: &Path) -> Vec<std::ffi::OsString> {
    vec![
        COMPILE_FLAG.into(),
        "--models".into(),
        models.as_os_str().to_owned(),
        "--model".into(),
        model.into(),
        "--out".into(),
        out.as_os_str().to_owned(),
    ]
}

/// Runs `exe` in [`COMPILE_FLAG`] mode ([`helper_args`]) to write the context model of `model`'s
/// NPU conversion in the models folder `models` at `out`, within [`HELPER_DEADLINE`], in a job that
/// ends it with this process.
///
/// # Errors
/// A helper that could not start, ran out of time or did not exit 0 (with the end of what it
/// printed), or wrote no context model.
pub fn helper_compile(exe: &Path, models: &Path, model: &str, out: &Path) -> Result<(), String> {
    let spec = earshot_proc::ChildSpec {
        program: exe.to_path_buf(),
        args: helper_args(models, model, out),
        current_dir: out.parent().unwrap_or(models).to_path_buf(),
        env_remove: Vec::new(),
        env_set: Vec::new(),
        stdin: Vec::new(),
        deadline: HELPER_DEADLINE,
        max_output: 64 * 1024,
    };
    let output = earshot_proc::run(&spec, &|| false).map_err(|e| format!("the helper: {e}"))?;
    if !output.status.success() {
        let text = String::from_utf8_lossy(&output.stderr);
        let last = text
            .lines()
            .rev()
            .find(|l| !l.trim().is_empty())
            .unwrap_or("");
        return Err(format!("the helper ended with {} ({last})", output.status));
    }
    if !out.is_file() {
        return Err(format!("the helper wrote no {CONTEXT_MODEL}"));
    }
    Ok(())
}

/// The [`COMPILE_FLAG`] mode: `None` when `args` (the process's arguments after the program, as
/// every child mode of the app takes them) do not start with it, else the exit code after
/// compiling (the rest exactly as [`helper_args`] writes them). It loads exactly what
/// `backends::load_parakeet` loads on the NPU, writing the context model, and prints only on
/// failure.
#[must_use]
pub fn child_main(args: &[std::ffi::OsString]) -> Option<i32> {
    if args.first().is_none_or(|a| a != COMPILE_FLAG) {
        return None;
    }
    Some(match compile_here(args) {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("{e}");
            1
        }
    })
}

/// `(models, model, out)` of a [`COMPILE_FLAG`] command line ([`helper_args`]), nothing else.
///
/// # Errors
/// Anything but `--npu-compile --models <dir> --model <name> --out <file>`.
pub(crate) fn parse_helper_args(
    args: &[std::ffi::OsString],
) -> Result<(PathBuf, String, PathBuf), String> {
    const SHAPE: &str = "expected --npu-compile --models <dir> --model <name> --out <file>";
    let [flag, flag_models, models, flag_model, model, flag_out, out] = args else {
        return Err(SHAPE.to_owned());
    };
    if flag != COMPILE_FLAG
        || flag_models != "--models"
        || flag_model != "--model"
        || flag_out != "--out"
    {
        return Err(SHAPE.to_owned());
    }
    let model = model.to_str().ok_or("the model name is not text")?;
    Ok((PathBuf::from(models), model.to_owned(), PathBuf::from(out)))
}

fn compile_here(args: &[std::ffi::OsString]) -> Result<(), String> {
    let (models, model, out) = parse_helper_args(args)?;
    crate::backends::compile_npu_model(&crate::backends::ModelDir::new(models), &model, &out)
        .map_err(|e| e.to_string())
}

/// The last load [`record`]ed in this process ([`last_load`]).
static LAST: Mutex<Option<How>> = Mutex::new(None);

/// How the last NPU encoder load of this process went, for reports and tests (its reasons may
/// name paths: never shown to the user).
#[must_use]
pub fn last_load() -> Option<How> {
    LAST.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone()
}

/// The note state of this process: whether a refusal waits to be shown, and whether one was.
static NOTE_STATE: Mutex<(bool, bool)> = Mutex::new((false, false));

/// Records how a track's load went: a refusal ([`How::is_refusal`]) makes [`take_note`] answer
/// once, the first time only, and waits to be stored ([`take_unstored`]).
pub fn record(how: &How) {
    *LAST
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(how.clone());
    if how.is_refusal() {
        unstored(Refused::new(LoadFor::Track, how.steps()));
        let mut state = NOTE_STATE
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !state.1 {
            state.0 = true;
        }
    }
}

/// [`NOTE`] once per process, after the first refusal [`record`]ed; `None` otherwise.
#[must_use]
pub fn take_note() -> Option<&'static str> {
    let mut state = NOTE_STATE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if state.0 {
        *state = (false, true);
        Some(NOTE)
    } else {
        None
    }
}

/// Whose load a [`Refused`] was.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum LoadFor {
    /// A meeting track's or the final pass's load.
    Track,
    /// A preview's load (it loads only a compiled model).
    Preview,
}

/// One load that did not use the compiled model, path-free ([`RefusalRecord`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Refused {
    /// When, in Unix milliseconds (0: the clock was before 1970).
    pub at_ms: u64,
    /// Whose load.
    pub load_for: LoadFor,
    /// The steps, first first ([`How::steps`]).
    pub steps: Vec<Step>,
}

impl Refused {
    /// A refusal of `load_for`'s load by `steps`, now.
    #[must_use]
    pub fn new(load_for: LoadFor, steps: Vec<Step>) -> Self {
        let at_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX));
        Self {
            at_ms,
            load_for,
            steps,
        }
    }
}

/// Refusals of this process not stored yet ([`take_unstored`]); at most one per kind, the latest.
static UNSTORED: Mutex<Vec<Refused>> = Mutex::new(Vec::new());

fn unstored(r: Refused) {
    let mut list = UNSTORED
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    list.retain(|old| old.load_for != r.load_for);
    list.push(r);
}

/// Records a preview's refusal by `steps` for [`take_unstored`] only: it never takes the banner
/// ([`take_note`]) or replaces [`last_load`], which are the track's (T12.30).
pub fn record_preview_refusal(steps: Vec<Step>) {
    unstored(Refused::new(LoadFor::Preview, steps));
}

/// The refusals [`record`]ed or [`record_preview_refusal`]ed since the last call, for the host to
/// store ([`RefusalRecord::with`]).
#[must_use]
pub fn take_unstored() -> Vec<Refused> {
    std::mem::take(
        &mut *UNSTORED
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner),
    )
}

/// The device record `npu-compiled` (T12.12e): the latest refusal of each kind, with no path, file
/// name or error text, so a refusal can be diagnosed after the fact. Stored as
/// `version u32 LE | JSON`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RefusalRecord {
    /// The latest refusal of a track's load.
    pub track: Option<Refused>,
    /// The latest refusal of a preview's load.
    pub preview: Option<Refused>,
}

/// The device-local key of [`RefusalRecord`].
pub const REFUSALS_KEY: &str = "npu-compiled";
/// The version of [`RefusalRecord`]'s encoding.
pub const REFUSALS_VERSION: u32 = 1;

impl RefusalRecord {
    /// This record with `refused` in it, each replacing the kind's older entry.
    #[must_use]
    pub fn with(mut self, refused: Vec<Refused>) -> Self {
        for r in refused {
            match r.load_for {
                LoadFor::Track => self.track = Some(r),
                LoadFor::Preview => self.preview = Some(r),
            }
        }
        self
    }

    /// `version u32 LE | JSON`.
    ///
    /// # Errors
    /// Never in practice (serde_json on plain data).
    pub fn encode(&self) -> Result<Vec<u8>, String> {
        let mut out = REFUSALS_VERSION.to_le_bytes().to_vec();
        out.extend(serde_json::to_vec(self).map_err(|e| e.to_string())?);
        Ok(out)
    }

    /// The record in `bytes`; `None` for another version or a damaged one (it is then replaced).
    #[must_use]
    pub fn decode(bytes: &[u8]) -> Option<Self> {
        let (version, json) = bytes.split_first_chunk::<4>()?;
        if u32::from_le_bytes(*version) != REFUSALS_VERSION {
            return None;
        }
        serde_json::from_slice(json).ok()
    }
}

#[cfg(test)]
#[path = "npu_compiled_tests.rs"]
mod tests;
