//! The opt-in CUDA files of an x64 install (T8.4ad to T8.4af, docs/cuda-install.md): what the
//! download, the loaders and the Settings page share, and the choice to run transcription from
//! them.
//!
//! Every file of both parts lives in one folder, [`folder`], because a DLL's imports are found
//! beside it: ONNX Runtime's CUDA provider and llama.cpp's `ggml-cuda.dll` both import cuBLAS.
//! So [`CudaPart::Transcription`] needs [`CudaPart::Summaries`]'s files, and no file name is in
//! both parts.
//!
//! - **What a part holds** comes only from `[[cuda_archive]]` in `models/manifest.toml`
//!   ([`part_files`]). The names each part must have to run are a fixed list here
//!   ([`CudaPart::required_files`]); a required name the manifest does not pin is reported by
//!   [`not_available_yet`], and that part is never installed. Both parts are complete in the
//!   embedded manifest (tests pin it); the rule is what catches a manifest that loses a file.
//! - **Install state** ([`install_state`]) reads sizes only, for a page to poll. **A load always
//!   goes through the full check**, [`verify_part`] or [`load_verified`], because the folder is
//!   writable by the user: every file's SHA-256, the first failure named. A `.dll` no part pins
//!   refuses the load too ([`unexpected_libraries`]): Windows resolves imports from this folder,
//!   so it could otherwise be loaded with no digest check.
//! - **A part's `needs()`**: its pinned files must be there too. A file the needed part's
//!   manifest does not pin cannot be what this part imports, so it does not count against it.
//! - **Downloads** ([`download_part`]) go through a fetch function (production:
//!   `download::Downloader::fetch`, through `host::download_cuda_part`), take only the pinned
//!   files from each archive and then delete the archive. A cancelled or cut transfer keeps its
//!   `.part`; an archive whose files are all right already costs no request.
//! - **Removal** ([`remove_part`]): refused for [`CudaPart::Summaries`] while
//!   [`CudaPart::Transcription`] is installed. A DLL this process or a meeting has loaded cannot
//!   be deleted, and it stays loaded until the process ends. Every file is tried for exclusive
//!   access first, so a held file is [`CudaError::InUse`], naming it, with nothing removed; a
//!   retry after a restart works. A read-only file is simply removed. Files no part pins are never
//!   deleted: [`Removed::left_in_folder`] names them, and the folder stays while they are there.
//!
//! ONNX Runtime is one library per process, so transcription's choice is made once, at start and
//! before ONNX Runtime loads: [`use_for_transcription`], which runs the full check. An app that
//! makes that choice on a thread takes [`hold_onnx`] first, and everything that may load ONNX
//! Runtime waits for it ([`wait_for_onnx`]). A setting never costs a recording: a meeting that
//! starts meanwhile records at once and holds its models until the choice is made, so it runs on
//! what the choice decides (T9.1l, [`crate::meeting_queue`]).
//! Whisper never runs on CUDA in the shipped
//! app, so after that choice only Parakeet is offered CUDA
//! ([`crate::backends::model_order`]); a Whisper meeting and the speech helpers still run on the
//! CUDA library, on its CPU provider.

use std::collections::BTreeSet;
use std::fmt;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU8, AtomicUsize, Ordering};
use std::sync::{Condvar, Mutex, PoisonError};

use earshot_core::asr::CancelToken;
use earshot_net::Url;

use crate::RuntimeError;
use crate::archive;
use crate::backends::ModelDir;
use crate::catalogue::{
    Artefact, Catalogue, CatalogueError, CudaArchive, CudaFile, FetchFn, FileState, InstallState,
    ModelDownloadError, Removed, file_problem, file_state, jail_join,
};
use crate::download::{DownloadError, Expected, part_path};

/// The folder under the models directory that holds both parts.
pub const FOLDER: &str = "cuda";

/// What the runtime reports when a CUDA request finds the files missing or failing their check.
pub const NOT_READY: &str = "the CUDA files are not all installed and verified";

/// One separately offered, separately removable part of the download.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CudaPart {
    /// llama.cpp's CUDA set (built by this project) and NVIDIA's cuBLAS.
    Summaries,
    /// ONNX Runtime's CUDA library and NVIDIA's cuDNN, for Parakeet. Needs [`Self::Summaries`].
    Transcription,
}

impl CudaPart {
    /// Every part, in the order it is offered.
    pub const ALL: [Self; 2] = [Self::Summaries, Self::Transcription];

    /// The part this one cannot run without.
    #[must_use]
    pub fn needs(self) -> Option<Self> {
        match self {
            Self::Summaries => None,
            Self::Transcription => Some(Self::Summaries),
        }
    }

    /// The part that cannot run without this one.
    #[must_use]
    pub fn needed_by(self) -> Option<Self> {
        Self::ALL.into_iter().find(|p| p.needs() == Some(self))
    }

    /// The manifest spelling; `None` for a part this version does not know.
    pub(crate) fn parse(s: &str) -> Option<Self> {
        match s {
            "summaries" => Some(Self::Summaries),
            "transcription" => Some(Self::Transcription),
            _ => None,
        }
    }

    /// The files this part cannot run without, whichever archive brings them. Summaries: cuBLAS
    /// and the set `cargo xtask build --cuda-set` builds. Transcription: what ONNX Runtime's CUDA
    /// provider imports or loads (docs/cuda-libraries.md).
    #[must_use]
    pub fn required_files(self) -> &'static [&'static str] {
        match self {
            Self::Summaries => &[
                "cublas64_13.dll",
                "cublasLt64_13.dll",
                "llama.dll",
                "ggml.dll",
                "ggml-base.dll",
                "ggml-cpu.dll",
                "ggml-cuda.dll",
            ],
            Self::Transcription => &[
                "onnxruntime.dll",
                "onnxruntime_providers_cuda.dll",
                "onnxruntime_providers_shared.dll",
                "cudnn64_9.dll",
                "cudnn_adv64_9.dll",
                "cudnn_cnn64_9.dll",
                "cudnn_engines_precompiled64_9.dll",
                "cudnn_engines_runtime_compiled64_9.dll",
                "cudnn_engines_tensor_ir64_9.dll",
                "cudnn_ext64_9.dll",
                "cudnn_graph64_9.dll",
                "cudnn_heuristic64_9.dll",
                "cudnn_ops64_9.dll",
            ],
        }
    }
}

/// The folder that holds both parts.
#[must_use]
pub fn folder(models: &ModelDir) -> PathBuf {
    models.root().join(FOLDER)
}

/// Why a part is not usable or was not removed.
#[derive(Debug)]
pub enum CudaError {
    /// The manifest cannot be read, or holds a broken path (a bug).
    Catalogue(CatalogueError),
    /// The manifest does not pin these required files yet: nothing can be downloaded for them.
    NotAvailableYet {
        /// The part.
        part: CudaPart,
        /// The files.
        files: Vec<&'static str>,
    },
    /// A file is not there.
    MissingFile {
        /// The part it belongs to.
        part: CudaPart,
        /// Its name.
        name: String,
    },
    /// A file differs from the manifest (its size or SHA-256). It is never loaded.
    FailedCheck {
        /// The part it belongs to.
        part: CudaPart,
        /// Its name.
        name: String,
        /// What differs.
        what: String,
    },
    /// A `.dll` no part pins is in the folder, where a load could pick it up unchecked.
    UnexpectedLibrary {
        /// Its name.
        name: String,
        /// The folder to delete it from.
        folder: PathBuf,
    },
    /// A file could not be read.
    Unreadable {
        /// Its name.
        name: String,
        /// What failed.
        error: String,
    },
    /// Summaries cannot go while Transcription, which imports its cuBLAS, is installed.
    NeededBy {
        /// The part asked to go.
        part: CudaPart,
        /// The installed part that needs it.
        by: CudaPart,
    },
    /// A file is held open or loaded (most often by this app, until it restarts). Retryable.
    InUse {
        /// Its name.
        name: String,
        /// What the system said.
        error: String,
    },
    /// A file could not be removed for another reason.
    Remove {
        /// Its path.
        path: PathBuf,
        /// What failed.
        error: String,
    },
}

impl fmt::Display for CudaError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Catalogue(e) => write!(f, "{e}"),
            Self::NotAvailableYet { part, files } => write!(
                f,
                "the {part:?} part is not available yet: this version has no download for {}",
                files.join(", ")
            ),
            Self::MissingFile { part, name } => {
                write!(f, "{name} of the {part:?} part is missing")
            }
            Self::FailedCheck { part, name, what } => {
                write!(f, "{name} of the {part:?} part failed its check: {what}")
            }
            Self::UnexpectedLibrary { name, folder } => write!(
                f,
                "{name} in {} is not one of Earshot's files, so nothing was loaded; delete it \
                 and try again",
                folder.display()
            ),
            Self::Unreadable { name, error } => write!(f, "{name} cannot be read: {error}"),
            Self::NeededBy { part, by } => write!(
                f,
                "the {part:?} part cannot be removed while the {by:?} part is installed"
            ),
            Self::InUse { name, error } => write!(
                f,
                "{name} is in use or not writable, so nothing was removed; restart Earshot and \
                 try again ({error})"
            ),
            Self::Remove { path, error } => write!(f, "{}: {error}", path.display()),
        }
    }
}

impl std::error::Error for CudaError {}

impl From<CatalogueError> for CudaError {
    fn from(e: CatalogueError) -> Self {
        Self::Catalogue(e)
    }
}

impl From<CudaError> for RuntimeError {
    /// A CUDA request without its files never runs on something else (T8.4ae).
    fn from(_: CudaError) -> Self {
        Self::Unavailable(NOT_READY)
    }
}

fn archives_of(c: &Catalogue, part: CudaPart) -> impl Iterator<Item = &CudaArchive> {
    c.cuda_archives().iter().filter(move |a| a.part == part)
}

fn artefact_of<'a>(c: &'a Catalogue, a: &CudaArchive) -> Result<&'a Artefact, CatalogueError> {
    // The catalogue refused a manifest where this fails.
    c.artefact(&a.artefact)
        .ok_or_else(|| CatalogueError::Manifest(format!("unknown artefact `{}`", a.artefact)))
}

/// Every file `part` holds, with its size and SHA-256, in manifest order.
#[must_use]
pub fn part_files(c: &Catalogue, part: CudaPart) -> Vec<&CudaFile> {
    archives_of(c, part).flat_map(|a| a.files.iter()).collect()
}

/// The required files of `part` the manifest does not pin (module docs). A part with any is
/// never installed.
#[must_use]
pub fn not_available_yet(c: &Catalogue, part: CudaPart) -> Vec<&'static str> {
    let pinned: BTreeSet<String> = part_files(c, part)
        .iter()
        .map(|f| f.name.to_lowercase())
        .collect();
    part.required_files()
        .iter()
        .copied()
        .filter(|name| !pinned.contains(&name.to_lowercase()))
        .collect()
}

/// The bytes `part` downloads, as far as the manifest pins them.
///
/// # Errors
/// [`CatalogueError`] for a broken manifest (a bug).
pub fn download_size(c: &Catalogue, part: CudaPart) -> Result<u64, CatalogueError> {
    let mut total = 0;
    for a in archives_of(c, part) {
        total += artefact_of(c, a)?.size;
    }
    Ok(total)
}

/// The bytes `part` takes on disk once installed, as far as the manifest pins them.
#[must_use]
pub fn disk_size(c: &Catalogue, part: CudaPart) -> u64 {
    part_files(c, part).iter().map(|f| f.size).sum()
}

/// A part's files on disk, from sizes only.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PartInstall {
    /// The part.
    pub part: CudaPart,
    /// Installed only when every required file is pinned and every pinned file has its size.
    pub state: InstallState,
    /// Bytes held towards the download.
    pub held: u64,
    /// The download size, as far as the manifest pins it.
    pub total: u64,
    /// Each pinned file.
    pub files: Vec<(String, FileState)>,
    /// [`not_available_yet`].
    pub not_available_yet: Vec<&'static str>,
    /// A pinned file of the part it [`CudaPart::needs`] is not there.
    pub needs_missing: bool,
    /// [`unexpected_libraries`] (empty when the folder cannot be read: the load's check says so).
    pub unexpected_libraries: Vec<String>,
}

impl PartInstall {
    /// Whether a load is worth trying. This reads sizes: the load itself still goes through
    /// [`verify_part`].
    #[must_use]
    pub fn looks_usable(&self) -> bool {
        self.state == InstallState::Installed
            && !self.needs_missing
            && self.unexpected_libraries.is_empty()
    }
}

/// Whether `files` are all in `models` with their sizes (never for no files at all).
fn all_present(models: &ModelDir, files: &[&CudaFile]) -> bool {
    let dir = folder(models);
    !files.is_empty()
        && files
            .iter()
            .all(|f| file_state(&dir.join(&f.name), f.size) == FileState::Present)
}

/// `part`'s files in `models`, from sizes only (module docs).
///
/// # Errors
/// [`CatalogueError`] for a broken manifest (a bug).
pub fn install_state(
    c: &Catalogue,
    models: &ModelDir,
    part: CudaPart,
) -> Result<PartInstall, CatalogueError> {
    let dir = folder(models);
    let mut files = Vec::new();
    let (mut held, mut total) = (0u64, 0u64);
    for a in archives_of(c, part) {
        let artefact = artefact_of(c, a)?;
        total += artefact.size;
        let states: Vec<(String, FileState)> = a
            .files
            .iter()
            .map(|f| (f.name.clone(), file_state(&dir.join(&f.name), f.size)))
            .collect();
        held += if states.iter().all(|(_, s)| *s == FileState::Present) {
            artefact.size
        } else {
            match file_state(&artefact.file(models)?, artefact.size) {
                // Downloaded and not extracted yet.
                FileState::Present => artefact.size,
                FileState::Partial(n) => n.min(artefact.size),
                FileState::WrongSize(_) | FileState::Missing => 0,
            }
        };
        files.extend(states);
    }
    let gap = not_available_yet(c, part);
    let present = files.iter().filter(|(_, s)| *s == FileState::Present);
    let state = if gap.is_empty() && !files.is_empty() && present.clone().count() == files.len() {
        InstallState::Installed
    } else if held > 0 || present.count() > 0 {
        InstallState::Partial
    } else {
        InstallState::Missing
    };
    let needs_missing = part
        .needs()
        .is_some_and(|needed| !all_present(models, &part_files(c, needed)));
    Ok(PartInstall {
        part,
        state,
        held,
        total,
        files,
        not_available_yet: gap,
        needs_missing,
        unexpected_libraries: unexpected_libraries(c, models).unwrap_or_default(),
    })
}

fn check_file(models: &ModelDir, part: CudaPart, f: &CudaFile) -> Result<(), CudaError> {
    let path = folder(models).join(&f.name);
    match file_problem(&path, &f.sha256, f.size) {
        Ok(None) => Ok(()),
        Ok(Some(what)) if what == "missing" => Err(CudaError::MissingFile {
            part,
            name: f.name.clone(),
        }),
        Ok(Some(what)) => Err(CudaError::FailedCheck {
            part,
            name: f.name.clone(),
            what,
        }),
        Err(e) => Err(CudaError::Unreadable {
            name: f.name.clone(),
            error: e.to_string(),
        }),
    }
}

/// The names (lower case) every part pins.
fn pinned_names(c: &Catalogue) -> BTreeSet<String> {
    c.cuda_archives()
        .iter()
        .flat_map(|a| a.files.iter().map(|f| f.name.to_lowercase()))
        .collect()
}

/// The `.dll` files directly in the folder that no part pins, sorted. The loader lets Windows
/// resolve imports from this folder, so such a file could be loaded with no digest check.
/// Anything else, and anything inside a staging folder, cannot be reached that way.
///
/// # Errors
/// The folder cannot be read (no folder is no files).
pub fn unexpected_libraries(c: &Catalogue, models: &ModelDir) -> io::Result<Vec<String>> {
    let pinned = pinned_names(c);
    let entries = match std::fs::read_dir(folder(models)) {
        Ok(entries) => entries,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    let mut found = Vec::new();
    for entry in entries {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        let lower = name.to_lowercase();
        if lower.ends_with(".dll") && !entry.file_type()?.is_dir() && !pinned.contains(&lower) {
            found.push(name);
        }
    }
    found.sort();
    Ok(found)
}

/// What is wrong with the folder of llama.cpp's DLLs a summary's child would load (T9.3), `None`
/// when it may: no [`unexpected_libraries`] and [`CudaPart::Summaries`] installed by size. It runs
/// per summary, so it reads no file's bytes: the start's [`verify_part`] and its trial cover those.
/// Without `models` nothing can be checked, so it refuses. A folder other than the CUDA folder is
/// the replay's `--llama-dir`, a developer's own choice with no manifest to check it against.
#[must_use]
pub(crate) fn child_folder_problem(
    loaded: &Path,
    models: Option<&ModelDir>,
) -> Option<&'static str> {
    let Some(models) = models else {
        return Some("the folder cannot be checked");
    };
    if loaded != folder(models) {
        return None;
    }
    let Ok(c) = Catalogue::embedded() else {
        return Some("the folder cannot be checked");
    };
    match unexpected_libraries(c, models) {
        Err(_) => return Some("the folder cannot be read"),
        Ok(found) if !found.is_empty() => return Some("a library that was not installed is there"),
        Ok(_) => {}
    }
    match install_state(c, models, CudaPart::Summaries) {
        Ok(install) if install.looks_usable() => None,
        Ok(_) => Some("a file is missing or has another size"),
        Err(_) => Some("the folder cannot be checked"),
    }
}

/// The full check of `part` in `c`: every required file pinned, no [`unexpected_libraries`] in
/// the folder, then every byte of each of its files and of the pinned files of the part it needs.
///
/// # Errors
/// [`CudaError`], the first problem found, naming the file.
pub fn verify_part_in(c: &Catalogue, models: &ModelDir, part: CudaPart) -> Result<(), CudaError> {
    let gap = not_available_yet(c, part);
    if !gap.is_empty() {
        return Err(CudaError::NotAvailableYet { part, files: gap });
    }
    let dir = folder(models);
    let unexpected = unexpected_libraries(c, models).map_err(|e| CudaError::Unreadable {
        name: dir.display().to_string(),
        error: e.to_string(),
    })?;
    if let Some(name) = unexpected.into_iter().next() {
        return Err(CudaError::UnexpectedLibrary { name, folder: dir });
    }
    if let Some(needed) = part.needs() {
        for f in part_files(c, needed) {
            check_file(models, needed, f)?;
        }
    }
    for f in part_files(c, part) {
        check_file(models, part, f)?;
    }
    Ok(())
}

/// The full check of `part` against the manifest built into this binary: what every load goes
/// through (module docs). It reads about a gigabyte per part.
///
/// # Errors
/// [`CudaError`], the first problem found, naming the file.
pub fn verify_part(models: &ModelDir, part: CudaPart) -> Result<(), CudaError> {
    verify_part_in(Catalogue::embedded()?, models, part)
}

/// Runs `load` on the folder once [`verify_part_in`] has passed; a file failing its check means
/// `load` never runs. What is on disk can still change after the check returns (the folder is
/// the user's), so this guards against damage and leftovers, not against a program of the
/// user's replacing a file in that moment.
///
/// # Errors
/// [`CudaError`]; `load`'s own result comes back as it is.
pub fn load_verified_in<T>(
    c: &Catalogue,
    models: &ModelDir,
    part: CudaPart,
    load: impl FnOnce(&Path) -> T,
) -> Result<T, CudaError> {
    verify_part_in(c, models, part)?;
    Ok(load(&folder(models)))
}

/// [`load_verified_in`] with the manifest built into this binary: the way a loader reaches the
/// folder (`earshot_llm_local::load_from_folder`, ONNX Runtime's library).
///
/// # Errors
/// As [`load_verified_in`].
pub fn load_verified<T>(
    models: &ModelDir,
    part: CudaPart,
    load: impl FnOnce(&Path) -> T,
) -> Result<T, CudaError> {
    load_verified_in(Catalogue::embedded()?, models, part, load)
}

fn catalogue_err(e: CatalogueError) -> ModelDownloadError {
    ModelDownloadError::Catalogue(e)
}

/// Downloads what the manifest pins of `part` into `models`: each archive through `fetch`,
/// `progress(done, total)` over the whole part (module docs). It does not bring the part's
/// [`not_available_yet`] files, so check that before calling the part installed.
///
/// # Errors
/// [`ModelDownloadError`]; the archives finished before stay.
pub fn download_part(
    c: &Catalogue,
    models: &ModelDir,
    part: CudaPart,
    cancel: &CancelToken,
    progress: &mut dyn FnMut(u64, u64),
    fetch: &mut FetchFn<'_>,
) -> Result<(), ModelDownloadError> {
    let total = download_size(c, part).map_err(catalogue_err)?;
    let dir = folder(models);
    let mut finished = 0u64;
    progress(0, total);
    for a in archives_of(c, part) {
        let artefact = artefact_of(c, a).map_err(catalogue_err)?;
        let all_right = a.files.iter().all(|f| {
            matches!(
                file_problem(&dir.join(&f.name), &f.sha256, f.size),
                Ok(None)
            )
        });
        if !all_right {
            fetch_and_extract(a, artefact, models, cancel, fetch, &mut |done| {
                progress(finished + done.min(artefact.size), total);
            })?;
        }
        finished += artefact.size;
        progress(finished, total);
    }
    Ok(())
}

fn fetch_and_extract(
    a: &CudaArchive,
    artefact: &Artefact,
    models: &ModelDir,
    cancel: &CancelToken,
    fetch: &mut FetchFn<'_>,
    progress: &mut dyn FnMut(u64),
) -> Result<(), ModelDownloadError> {
    let dest = artefact.file(models).map_err(catalogue_err)?;
    let url = Url::parse(&artefact.url).map_err(|e| {
        catalogue_err(CatalogueError::Manifest(format!(
            "artefact `{}` url: {e}",
            artefact.id
        )))
    })?;
    let exp = Expected {
        sha256: &artefact.sha256,
        size: artefact.size,
    };
    fetch(&url, &dest, &exp, cancel, &mut |done, _| progress(done)).map_err(|error| {
        ModelDownloadError::Download {
            artefact: artefact.id.clone(),
            error,
        }
    })?;
    if cancel.is_cancelled() {
        return Err(ModelDownloadError::Download {
            artefact: artefact.id.clone(),
            error: DownloadError::Cancelled {
                done: artefact.size,
                total: artefact.size,
            },
        });
    }
    archive::extract_flat(
        &dest,
        &a.format,
        &a.files,
        &folder(models),
        &staging(artefact),
        cancel,
    )
    .map_err(|error| ModelDownloadError::of_extraction(artefact, error))?;
    // The files are what a run reads; a failure to delete it only costs disk.
    let _ = std::fs::remove_file(&dest);
    Ok(())
}

/// The name of an archive's staging folder inside the CUDA folder.
fn staging(artefact: &Artefact) -> String {
    format!("{STAGING_PREFIX}{}", artefact.id)
}

/// How an archive's staging folder's name starts.
const STAGING_PREFIX: &str = ".staging-";

/// Whether nothing else holds file `name` in `dir`, by asking for it exclusively. The system
/// refuses a file that is mapped as a loaded image or open elsewhere; deleting it would fail the
/// same way, but only after the files before it had gone. A read-only file refuses this too, and
/// no restart would change that, so the question is asked again with the attribute cleared, and
/// the attribute put back: a later file may stop the removal, and `remove_file` takes a read-only
/// file as it is. A read-only open would not do: it succeeds on a mapped image, because the
/// loader's own handle is closed by then.
fn exclusive(dir: &Path, name: &str) -> Result<(), CudaError> {
    let path = dir.join(name);
    let ask = || {
        let mut open = std::fs::OpenOptions::new();
        open.write(true);
        #[cfg(windows)]
        std::os::windows::fs::OpenOptionsExt::share_mode(&mut open, 0);
        open.open(&path).map(drop)
    };
    let other = |e: io::Error| CudaError::Remove {
        path: path.clone(),
        error: e.to_string(),
    };
    let metadata = match std::fs::metadata(&path) {
        Ok(m) => m,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(other(e)),
    };
    if !metadata.is_file() {
        return Ok(());
    }
    let mut answer = ask();
    if answer.is_err() && metadata.permissions().readonly() {
        archive::clear_read_only(&path).map_err(other)?;
        answer = ask();
        archive::restore_read_only(&path);
    }
    match answer {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) if archive::held_by_another(&e) => Err(CudaError::InUse {
            name: name.to_owned(),
            error: e.to_string(),
        }),
        Err(e) => Err(other(e)),
    }
}

/// What is in the folder that no part pins: what a removal reports it left. Sorted. A staging
/// folder is Earshot's own leftover and its name holds a manifest id, so it is not reported.
fn left_in_folder(c: &Catalogue, dir: &Path) -> Vec<String> {
    let pinned = pinned_names(c);
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut left: Vec<String> = entries
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|name| !pinned.contains(&name.to_lowercase()) && !name.starts_with(STAGING_PREFIX))
        .collect();
    left.sort();
    left
}

/// Removes `part` from `models` (module docs): its files and its archives' downloads, whole or
/// partial.
///
/// # Errors
/// [`CudaError::NeededBy`] and [`CudaError::InUse`], nothing removed; [`CudaError::Remove`] when
/// a removal fails after that check (the files before it stay removed and a retry finishes).
pub fn remove_part(c: &Catalogue, models: &ModelDir, part: CudaPart) -> Result<Removed, CudaError> {
    if let Some(by) = part.needed_by()
        && all_present(models, &part_files(c, by))
    {
        return Err(CudaError::NeededBy { part, by });
    }
    let dir = folder(models);
    let files = part_files(c, part);
    for f in &files {
        exclusive(&dir, &f.name)?;
    }
    let mut removed = Removed::default();
    let remove = |path: PathBuf, removed: &mut Removed| -> Result<(), CudaError> {
        let size = match std::fs::metadata(&path) {
            Ok(m) if m.is_file() => m.len(),
            _ => return Ok(()),
        };
        match std::fs::remove_file(&path) {
            Ok(()) => {
                removed.bytes_freed += size;
                removed.files.push(path);
                Ok(())
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(CudaError::Remove {
                path,
                error: e.to_string(),
            }),
        }
    };
    for f in &files {
        remove(jail_join(&dir, &f.name)?, &mut removed)?;
    }
    for a in archives_of(c, part) {
        let artefact = artefact_of(c, a)?;
        let archive = artefact.file(models)?;
        remove(part_path(&archive), &mut removed)?;
        remove(archive, &mut removed)?;
        let _ = std::fs::remove_dir_all(dir.join(staging(artefact)));
    }
    // The folder goes with its last file; a folder with anything left in it stays, and what no
    // part pins is reported, never deleted: it is not ours to delete.
    removed.left_in_folder = left_in_folder(c, &dir);
    let _ = std::fs::remove_dir(&dir);
    Ok(removed)
}

/// ONNX Runtime's library in [`folder`]: the official GPU build, the same file as
/// `earshot_onnx::ORT_CUDA_DYLIB`, so it is held to the same pin.
pub const ONNX_RUNTIME_DLL: &str = "onnxruntime.dll";

/// NVIDIA's cuBLAS: `onnxruntime_providers_cuda.dll` imports both.
pub const CUBLAS_DLLS: [&str; 2] = ["cublasLt64_13.dll", "cublas64_13.dll"];

/// ONNX Runtime's three DLLs. The provider DLL is found beside `onnxruntime.dll` by ONNX Runtime
/// itself, and imports the shared one.
pub const ONNX_DLLS: [&str; 3] = [
    ONNX_RUNTIME_DLL,
    "onnxruntime_providers_shared.dll",
    "onnxruntime_providers_cuda.dll",
];

/// NVIDIA's cuDNN: nothing imports `cudnn64_9.dll`, which loads the rest at run time.
pub const CUDNN_DLLS: [&str; 10] = [
    "cudnn64_9.dll",
    "cudnn_adv64_9.dll",
    "cudnn_cnn64_9.dll",
    "cudnn_engines_precompiled64_9.dll",
    "cudnn_engines_runtime_compiled64_9.dll",
    "cudnn_engines_tensor_ir64_9.dll",
    "cudnn_ext64_9.dll",
    "cudnn_graph64_9.dll",
    "cudnn_heuristic64_9.dll",
    "cudnn_ops64_9.dll",
];

/// [`CUDNN_DLLS`], each after what it imports (read from their import tables, pinned by
/// `tests/cuda_load_order.rs`): a load by full path does not look beside the file for its imports,
/// only among what is already loaded.
pub const CUDNN_LOAD_ORDER: [&str; 10] = [
    "cudnn_graph64_9.dll",
    "cudnn_ops64_9.dll",
    "cudnn_adv64_9.dll",
    "cudnn_cnn64_9.dll",
    "cudnn_engines_precompiled64_9.dll",
    "cudnn_engines_runtime_compiled64_9.dll",
    "cudnn_engines_tensor_ir64_9.dll",
    "cudnn_ext64_9.dll",
    "cudnn_heuristic64_9.dll",
    "cudnn64_9.dll",
];

/// Every file Parakeet on CUDA loads from [`folder`] (T8.4ae pins these in the manifest).
#[must_use]
pub fn transcription_files() -> Vec<&'static str> {
    let mut files = CUBLAS_DLLS.to_vec();
    files.extend(ONNX_DLLS);
    files.extend(CUDNN_DLLS);
    files
}

/// Which of [`transcription_files`] are not in `dir`.
#[must_use]
pub fn missing_transcription_files(dir: &Path) -> Vec<&'static str> {
    transcription_files()
        .into_iter()
        .filter(|f| !dir.join(f).is_file())
        .collect()
}

/// A build with no ONNX Runtime CUDA provider was asked for one.
pub(crate) const NOT_BUILT: RuntimeError =
    RuntimeError::Unavailable("this version of Earshot cannot run transcription on CUDA");
/// A developer's `cuda` build runs on its own toolkit and never uses the folder.
const DEVELOPER_BUILD: RuntimeError =
    RuntimeError::Unavailable("this developer build runs on its own CUDA toolkit");
/// The chosen library did not load ([`record_load_failure`]).
const LOAD_FAILED: RuntimeError = RuntimeError::Unavailable(
    "the CUDA files did not load, so CUDA stays off until the next start",
);
/// ONNX Runtime has loaded, so the choice can no longer be made.
const TOO_LATE: RuntimeError = RuntimeError::Unavailable(
    "ONNX Runtime is already loaded: CUDA is chosen when the app starts, before it loads",
);
/// A file of the set is not there.
pub(crate) const FILES_MISSING: RuntimeError =
    RuntimeError::Unavailable("the CUDA transcription files are not all installed");

/// The one decision about this process's ONNX Runtime library. Open until the start decides; a
/// meeting that starts meanwhile records and holds its models until then (T9.1l).
#[derive(Debug)]
pub struct Choice(AtomicU8);

const OPEN: u8 = 0;
const CHOSEN: u8 = 1;
const LOAD_FAILED_STATE: u8 = 2;

impl Choice {
    /// Nothing decided.
    #[must_use]
    pub const fn new() -> Self {
        Self(AtomicU8::new(OPEN))
    }

    /// Takes the folder, unless its library has failed. Choosing twice is one choice.
    pub fn choose(&self) -> bool {
        match self
            .0
            .compare_exchange(OPEN, CHOSEN, Ordering::SeqCst, Ordering::SeqCst)
        {
            Ok(_) | Err(CHOSEN) => true,
            Err(_) => false,
        }
    }

    /// Whether nothing is decided yet.
    #[must_use]
    pub fn is_open(&self) -> bool {
        self.0.load(Ordering::SeqCst) == OPEN
    }

    /// Whether the folder was taken.
    #[must_use]
    pub fn chosen(&self) -> bool {
        self.0.load(Ordering::SeqCst) == CHOSEN
    }

    fn load_failed(&self) {
        self.0.store(LOAD_FAILED_STATE, Ordering::SeqCst);
    }

    fn has_load_failed(&self) -> bool {
        self.0.load(Ordering::SeqCst) == LOAD_FAILED_STATE
    }
}

impl Default for Choice {
    fn default() -> Self {
        Self::new()
    }
}

/// This process's decision. The chosen library failing to load is its last state: CUDA is off for
/// the rest of the process, and the caller that heard the failure was the only one to report it.
static CHOICE: Choice = Choice::new();

/// Says, once per process and before ONNX Runtime loads, whether this run uses CUDA for
/// transcription. The decision comes from the caller; nothing here reads a setting. `false`
/// changes nothing. With `true`, this process's accelerator order leads with CUDA
/// ([`crate::backends::accelerator_order`]), and ONNX Runtime loads from [`folder`]
/// ([`crate::backends::init_onnx`]).
///
/// A developer's `cuda` build already runs on CUDA, from its toolkit on PATH, so the folder is
/// never used there: `true` is refused, so nothing says it was chosen.
///
/// # Errors
/// [`RuntimeError::Unavailable`] in a developer `cuda` build; when this build has no provider, ONNX
/// Runtime has already loaded, or the files fail [`verify_part`] (each file's SHA-256). Nothing is
/// recorded then: this process stays off CUDA and the caller says so.
pub fn use_for_transcription(models: &ModelDir, use_cuda: bool) -> Result<(), RuntimeError> {
    let catalogue = Catalogue::embedded().map_err(CudaError::from)?;
    use_for_transcription_in(catalogue, models, use_cuda)
}

/// [`use_for_transcription`] against the manifest `c`.
///
/// # Errors
/// As [`use_for_transcription`].
pub fn use_for_transcription_in(
    c: &Catalogue,
    models: &ModelDir,
    use_cuda: bool,
) -> Result<(), RuntimeError> {
    if !use_cuda {
        return Ok(());
    }
    if cfg!(feature = "cuda") {
        return Err(DEVELOPER_BUILD);
    }
    if !provider_built() {
        return Err(NOT_BUILT);
    }
    if load_failed() {
        return Err(LOAD_FAILED);
    }
    if earshot_onnx::loaded_library().is_some() {
        return Err(TOO_LATE);
    }
    verify_part_in(c, models, CudaPart::Transcription)?;
    if CHOICE.choose() {
        Ok(())
    } else {
        Err(off_reason())
    }
}

/// Whether this build has ONNX Runtime's CUDA provider at all.
#[must_use]
pub fn provider_built() -> bool {
    cfg!(any(feature = "onnx-cuda", feature = "cuda"))
}

/// The check of [`folder`] before anything in it loads, without loading it: [`verify_part`] of
/// the transcription part, which reads every byte of about 1.4 GB.
///
/// # Errors
/// [`RuntimeError::Unavailable`] when it fails, naming no file.
pub fn verify_transcription_folder(models: &ModelDir) -> Result<(), RuntimeError> {
    verify_part(models, CudaPart::Transcription).map_err(RuntimeError::from)
}

static ONNX_HOLDS: AtomicUsize = AtomicUsize::new(0);
static ONNX_LOCK: Mutex<()> = Mutex::new(());
static ONNX_RELEASED: Condvar = Condvar::new();
/// Callers that had to sleep at the gate, each counted once.
static ONNX_SLEPT: AtomicUsize = AtomicUsize::new(0);

/// While this lives, nothing loads ONNX Runtime ([`wait_for_onnx`]). A panic on the holder's
/// thread drops it like any other value, so ONNX Runtime is never shut off for good.
#[derive(Debug)]
#[must_use = "dropping the hold opens the gate"]
pub struct OnnxHold(());

/// Closes ONNX Runtime's gate until the hold is dropped, as `earshot_llm_local::hold_llama` does for
/// llama.cpp: the choice of [`use_for_transcription`] may take a minute (hashing the files, a trial
/// in a child), and it decides which library loads. Take it before anything can load ONNX Runtime.
pub fn hold_onnx() -> OnnxHold {
    let _lock = ONNX_LOCK.lock().unwrap_or_else(PoisonError::into_inner);
    ONNX_HOLDS.fetch_add(1, Ordering::AcqRel);
    OnnxHold(())
}

impl Drop for OnnxHold {
    fn drop(&mut self) {
        let _lock = ONNX_LOCK.lock().unwrap_or_else(PoisonError::into_inner);
        ONNX_HOLDS.fetch_sub(1, Ordering::AcqRel);
        ONNX_RELEASED.notify_all();
    }
}

/// Whether a hold is on now and the choice is still to be made: once it is made, either way, the
/// gate has nothing left to protect.
#[must_use]
pub fn onnx_held() -> bool {
    ONNX_HOLDS.load(Ordering::Acquire) > 0 && CHOICE.is_open()
}

/// How many callers have slept at the gate in this process (for tests: a clock cannot say it).
#[doc(hidden)]
#[must_use]
pub fn callers_slept_at_the_onnx_gate() -> usize {
    ONNX_SLEPT.load(Ordering::Acquire)
}

/// Waits until no hold is on. [`crate::backends::init_onnx`] calls it, and a caller that reads the
/// accelerator order first calls it before that ([`crate::host::settled_order`]): an order read
/// before the choice would name the wrong library. Never call it holding the store lock.
pub fn wait_for_onnx() {
    if !onnx_held() {
        return;
    }
    let mut lock = ONNX_LOCK.lock().unwrap_or_else(PoisonError::into_inner);
    if onnx_held() {
        ONNX_SLEPT.fetch_add(1, Ordering::AcqRel);
    }
    while onnx_held() {
        lock = ONNX_RELEASED
            .wait(lock)
            .unwrap_or_else(PoisonError::into_inner);
    }
}

/// [`wait_for_onnx`] that gives way to `cancel` (T9.22): returns false, without waiting longer, once
/// `cancel` is cancelled while the gate is still closed; true once the gate is open. Looks at the
/// token every [`ONNX_CANCEL_POLL`].
#[must_use]
pub fn wait_for_onnx_unless(cancel: &earshot_core::asr::CancelToken) -> bool {
    if !onnx_held() {
        return true;
    }
    let mut lock = ONNX_LOCK.lock().unwrap_or_else(PoisonError::into_inner);
    if onnx_held() {
        ONNX_SLEPT.fetch_add(1, Ordering::AcqRel);
    }
    while onnx_held() {
        if cancel.is_cancelled() {
            return false;
        }
        lock = ONNX_RELEASED
            .wait_timeout(lock, ONNX_CANCEL_POLL)
            .unwrap_or_else(PoisonError::into_inner)
            .0;
    }
    true
}

/// How often [`wait_for_onnx_unless`] looks at its token.
pub const ONNX_CANCEL_POLL: std::time::Duration = std::time::Duration::from_millis(50);

/// Tries the chosen set in this process, for a throwaway process only, because a bad set can end
/// the process that loads it (docs/cuda-install.md): chooses CUDA, loads ONNX
/// Runtime from [`folder`], loads the smallest installed Parakeet model on CUDA and decodes
/// [`TRIAL_SECONDS`] of a generated tone. Returns ONNX Runtime's node placement, only when every
/// session has nodes on CUDA.
///
/// # Errors
/// [`RuntimeError::Unavailable`] for a refused choice or no Parakeet model to try it with; what a
/// load or the decode failed with.
pub fn trial_on_cuda(models: &ModelDir) -> Result<String, RuntimeError> {
    use earshot_core::asr::{CancelToken, TranscribeOptions, Transcriber};
    use earshot_core::audio::{MonoBuffer, SampleRate};

    use crate::backends::{Accelerator, AsrModel, asr_model, init_onnx, load_parakeet};

    use_for_transcription(models, true)?;
    let model = trial_model(models).ok_or(NO_TRIAL_MODEL)?;
    let AsrModel::Parakeet(model) = asr_model(model)? else {
        return Err(NO_TRIAL_MODEL);
    };
    init_onnx(models, Accelerator::Cuda)?;
    let backend = |e: earshot_core::asr::AsrError| {
        RuntimeError::NoBackend(vec![(earshot_core::asr::BackendKind::Cuda, e)])
    };
    let mut parakeet =
        load_parakeet(models, model, Accelerator::Cuda, Some(TRIAL_THREADS)).map_err(backend)?;
    let samples = TRIAL_SECONDS * SampleRate::HZ_16000.hz() as usize;
    let tone: Vec<f32> = (0..samples)
        .map(|i| (i as f32 * 0.05).sin() * 0.1)
        .collect();
    parakeet
        .transcribe(
            &MonoBuffer::new(SampleRate::HZ_16000, tone),
            &TranscribeOptions::live(),
            &CancelToken::new(),
        )
        .map_err(backend)?;
    let sessions = parakeet.backends();
    if sessions
        .iter()
        .any(|s| s.placement.nodes_on(earshot_onnx::CUDA_EP) == 0)
    {
        return Err(TRIAL_OFF_CUDA);
    }
    Ok(sessions
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("; "))
}

/// Seconds of the trial's tone: enough for the encoder and a few decoder steps.
const TRIAL_SECONDS: usize = 2;
/// ONNX Runtime threads of the trial, as a live worker on a GPU.
const TRIAL_THREADS: usize = 2;
const NO_TRIAL_MODEL: RuntimeError =
    RuntimeError::Unavailable("no Parakeet model is installed to try CUDA with");
const TRIAL_OFF_CUDA: RuntimeError =
    RuntimeError::Unavailable("the trial's model did not run on CUDA");

/// Whether a Parakeet model is installed to try CUDA with. Without one there is nothing CUDA could
/// run either.
#[must_use]
pub fn has_trial_model(models: &ModelDir) -> bool {
    trial_model(models).is_some()
}

/// The smallest installed Parakeet model of the CUDA ladders.
fn trial_model(models: &ModelDir) -> Option<&'static str> {
    use crate::model_check::{LADDERS, model_facts};
    LADDERS
        .iter()
        .filter(|l| l.backend == earshot_core::asr::BackendKind::Cuda)
        .flat_map(|l| l.models.iter().copied())
        .filter(|m| {
            crate::backends::asr_model(m).is_ok_and(|a| {
                matches!(a, crate::backends::AsrModel::Parakeet(_))
                    && a.files().iter().all(|f| models.require(f).is_ok())
            })
        })
        .min_by_key(|m| model_facts(m).map_or(u64::MAX, |f| f.size))
}

/// Records that the chosen library did not load. CUDA leaves the order for the rest of the process
/// (as `summary::GPU_STALLED` does for a stalled GPU): the Settings page, the check's fingerprint
/// and later meetings see what really runs, and no later meeting repeats the warning.
pub(crate) fn record_load_failure() {
    CHOICE.load_failed();
}

/// Whether the chosen library failed to load in this process.
#[must_use]
pub fn load_failed() -> bool {
    CHOICE.has_load_failed()
}

/// Why this process cannot use ONNX Runtime's CUDA provider, in a build whose ONNX Runtime is not
/// a developer's.
pub(crate) fn off_reason() -> RuntimeError {
    if load_failed() {
        LOAD_FAILED
    } else {
        NOT_BUILT
    }
}

/// Loads what the provider cannot find by itself, so nothing goes on PATH. Measured (T8.4ad, this
/// folder not on PATH): cuBLAS is imported by the provider DLL and found beside it; without cuDNN
/// the provider refuses every Parakeet session ("cuDNN is unavailable"), and with only
/// `cudnn64_9.dll` loaded the process ends (0xC0000409), so all of it is loaded, or none reaches
/// the provider.
pub(crate) fn preload_for_provider(models: &ModelDir) -> Result<(), earshot_onnx::OnnxError> {
    let dir = folder(models);
    for file in CUDNN_LOAD_ORDER {
        earshot_onnx::preload_library(&dir.join(file))?;
    }
    Ok(())
}

/// Whether [`use_for_transcription`] chose [`folder`] for this process.
#[must_use]
pub fn transcription_from_folder() -> bool {
    CHOICE.chosen()
}

#[cfg(test)]
#[path = "cuda_tests.rs"]
mod tests;
