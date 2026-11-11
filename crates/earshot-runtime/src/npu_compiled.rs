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
    /// `encoder.onnx` and `encoder.weights`, the QNN package of this architecture, the ONNX Runtime
    /// library loaded and the NPUs `earshot_hardware` reads.
    ///
    /// # Errors
    /// An unreadable `conversion.json` or one without those pins; ONNX Runtime not loaded.
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
        let runtime = earshot_onnx::loaded_library_sha256().ok_or("ONNX Runtime is not loaded")?;
        Ok(Self::from_parts(
            &[("encoder.onnx", &onnx), ("encoder.weights", &weights)],
            &earshot_onnx::qnn::QNN_HOST,
            runtime,
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

/// How [`load`] loaded the encoder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum How {
    /// From the compiled context model.
    Compiled,
    /// The ordinary load, writing the context model for the next one (in process).
    Compiling {
        /// Why a compiled folder for this key was not used (`None`: there was none).
        refused: Option<String>,
        /// Whether what was written is kept (`Err`: why not).
        kept: Result<(), String>,
    },
    /// Compiled by the helper process, then loaded from what it wrote.
    CompiledByHelper {
        /// Why a compiled folder for this key was not used (`None`: there was none).
        refused: Option<String>,
    },
    /// The ordinary load, without writing.
    Ordinary {
        /// Why.
        why: String,
    },
}

impl How {
    /// Whether this load is reported ([`record`]): anything but the compiled model and the first
    /// compile of a key that was kept.
    #[must_use]
    pub fn is_refusal(&self) -> bool {
        match self {
            How::Compiled => false,
            How::Compiling { refused, kept } => refused.is_some() || kept.is_err(),
            How::CompiledByHelper { refused } => refused.is_some(),
            How::Ordinary { .. } => true,
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
}

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
    let mut refused = None;
    if dest.exists() {
        let usable = verify(&dest, key).and_then(|model| match fit(&model) {
            Ok(true) => Ok(model),
            Ok(false) => Err("the NPU asks for a new compile".to_owned()),
            Err(e) => Err(format!("the NPU cannot judge it ({e})")),
        });
        match usable {
            Ok(model) => match compiled(&model) {
                Ok(s) => return Ok((s, How::Compiled)),
                Err(e) => refused = Some(format!("its load failed ({e})")),
            },
            Err(why) => refused = Some(why),
        }
        if let Err(e) = put_aside(&root, &dest) {
            let why = format!(
                "the compiled model was refused ({}) and cannot be replaced ({e})",
                refused.unwrap_or_default()
            );
            return ordinary().map(|s| (s, How::Ordinary { why }));
        }
    }
    let tmp = unique(&root, ".tmp");
    if let Err(e) = std::fs::create_dir_all(&tmp) {
        let why = format!("no folder to compile into ({e})");
        return ordinary().map(|s| (s, How::Ordinary { why }));
    }
    let failed = |refused: Option<String>, e: String| match refused {
        Some(r) => format!("the compiled model was refused ({r}) and compiling failed ({e})"),
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
                let why = failed(refused, e.to_string());
                ordinary().map(|s| (s, How::Ordinary { why }))
            }
        },
        Compile::Helper(helper) => {
            let made = helper(&tmp.join(CONTEXT_MODEL))
                .and_then(|()| write_record(&tmp, key))
                .and_then(|()| publish(&tmp, &dest));
            if let Err(e) = made {
                let _ = std::fs::remove_dir_all(&tmp);
                let why = failed(refused, e);
                return ordinary().map(|s| (s, How::Ordinary { why }));
            }
            sweep(&root, &dest);
            // What another process wrote is checked like any compiled folder before it loads.
            match verify(&dest, key).and_then(|model| compiled(&model).map_err(|e| e.to_string())) {
                Ok(s) => Ok((s, How::CompiledByHelper { refused })),
                Err(e) => {
                    let why = format!("the helper's compiled model did not load ({e})");
                    ordinary().map(|s| (s, How::Ordinary { why }))
                }
            }
        }
    }
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

/// Runs `exe` in [`COMPILE_FLAG`] mode to write the context model of `model`'s NPU conversion in
/// the models folder `models` at `out`, within [`HELPER_DEADLINE`], in a job that ends it with
/// this process.
///
/// # Errors
/// A helper that could not start, ran out of time or did not exit 0 (with the end of what it
/// printed), or wrote no context model.
pub fn helper_compile(exe: &Path, models: &Path, model: &str, out: &Path) -> Result<(), String> {
    let spec = earshot_proc::ChildSpec {
        program: exe.to_path_buf(),
        args: vec![
            COMPILE_FLAG.into(),
            "--models".into(),
            models.as_os_str().to_owned(),
            "--model".into(),
            model.into(),
            "--out".into(),
            out.as_os_str().to_owned(),
        ],
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

/// The [`COMPILE_FLAG`] mode: `None` when `args` (the process's, program first) do not ask for it,
/// else the exit code after compiling (`--models <dir> --model <name> --out <file>`, nothing else).
/// It loads exactly what `backends::load_parakeet` loads on the NPU, writing the context model, and
/// prints only on failure.
#[must_use]
pub fn child_main(args: &[std::ffi::OsString]) -> Option<i32> {
    if args.get(1).is_none_or(|a| a != COMPILE_FLAG) {
        return None;
    }
    Some(match compile_here(&args[2..]) {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("{e}");
            1
        }
    })
}

fn compile_here(args: &[std::ffi::OsString]) -> Result<(), String> {
    let [flag_models, models, flag_model, model, flag_out, out] = args else {
        return Err("expected --models <dir> --model <name> --out <file>".to_owned());
    };
    if flag_models != "--models" || flag_model != "--model" || flag_out != "--out" {
        return Err("expected --models <dir> --model <name> --out <file>".to_owned());
    }
    let model = model.to_str().ok_or("the model name is not text")?;
    crate::backends::compile_npu_model(
        &crate::backends::ModelDir::new(PathBuf::from(models)),
        model,
        Path::new(out),
    )
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

/// Records how a load went: a refusal ([`How::is_refusal`]) makes [`take_note`] answer once, the
/// first time only.
pub fn record(how: &How) {
    *LAST
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(how.clone());
    if how.is_refusal() {
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

#[cfg(test)]
#[path = "npu_compiled_tests.rs"]
mod tests;
