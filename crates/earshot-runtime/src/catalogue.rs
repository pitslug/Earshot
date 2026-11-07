//! The model catalogue (T7.4d): what the Models page lists, read from the embedded
//! `models/manifest.toml`, plus the install state and removal of a model's files.
//!
//! - **Catalogue** ([`Catalogue::embedded`]): every artefact (url, cache path, size,
//!   SHA-256, archive extraction), every transcription model (`[[model]]`: engine,
//!   artefacts, languages), summary model (the artefacts of set `summary`, one file
//!   each) and the speech helpers ([`HELPERS_MODEL`]: the artefacts of this architecture's
//!   helpers set), and the ladders (`[[ladder]]`). The manifest is parsed strictly (unknown fields
//!   are refused, as xtask's parser does) and a manifest that does not parse or holds a
//!   path outside the models directory is an error, never a partial catalogue. Tests pin
//!   it to the runtime's own tables (`model_check::LADDERS` and `MODELS`,
//!   `backends::WHISPER_MODELS` and `PARAKEET_MODELS`, `summary_model::SUMMARY_MODELS`).
//! - **Paths** ([`jail_join`]): every cache path (file, its `.part`, extraction directory
//!   and members) must be relative, without `..`, root, prefix or drive; the rule of xtask's
//!   `download::jail_join`, checked at parse and again when a path is joined.
//! - **Install state** ([`Catalogue::install_state`]): from file sizes only (installed when
//!   every artefact has its manifest size, an archive when every pinned member of it does;
//!   partly downloaded when bytes are held in a file or `.part`); the full SHA-256 check is a
//!   separate call ([`Catalogue::verify`]).
//! - **Removal** ([`Catalogue::remove`]): a model's files and `.part`s, refused for a
//!   chosen or protected model and for `host::FALLBACK_MODEL`; a file another installed model also uses stays. The speech
//!   helpers (VAD, segmentation, speaker embeddings, ONNX Runtime) are listed as one item
//!   ([`HELPERS_MODEL`]) and never removed: every meeting needs them.
//! - **Download** ([`Catalogue::download`], T7.4e): every artefact of a model, one after
//!   the other, through a fetch function (production: `download::Downloader::fetch`), with
//!   progress over the whole model; a cancelled or cut transfer keeps its `.part`. An
//!   archive is extracted after its download (`crate::archive`, only its pinned members) and
//!   then deleted; one already extracted costs no request.
//! - **The opt-in CUDA files** (`[[cuda_archive]]`, T8.4ae): read and checked here, never
//!   listed as models; everything done with them is [`crate::cuda`].

use std::collections::BTreeSet;
use std::fmt;
use std::io;
use std::path::{Component, Path, PathBuf};
use std::sync::OnceLock;

use earshot_core::asr::BackendKind;
use serde::Deserialize;

use crate::archive::{self, ExtractError};
use crate::backends::ModelDir;
use crate::cuda::CudaPart;
use earshot_core::asr::CancelToken;
use earshot_net::Url;

use crate::download::{DownloadError, Expected, MODELS_MANIFEST, Outcome, part_path, sha256_file};
use crate::model_check::Scope;

#[cfg(test)]
#[path = "catalogue_tests.rs"]
mod tests;

/// The manifest set of the shipped local summary models.
const SUMMARY_SET: &str = "summary";

/// The catalogue name of the speech helpers: Silero VAD, segmentation, CAM++ and ONNX Runtime,
/// which every meeting needs (T9.1f). One item, because a user has no choice to make about
/// any one of them.
pub const HELPERS_MODEL: &str = "speech-helpers";

/// The manifest set holding the speech helpers of the x64 build.
pub const HELPERS_SET_X64: &str = "helpers-x64";

/// The manifest set holding the speech helpers of the ARM64 build.
pub const HELPERS_SET_ARM64: &str = "helpers-arm64";

/// The manifest set of this build's speech helpers (its ONNX Runtime is that architecture's;
/// `earshot_onnx::ORT_CPU_DYLIB` follows the same test).
pub const HELPERS_SET: &str = if cfg!(target_arch = "aarch64") {
    HELPERS_SET_ARM64
} else {
    HELPERS_SET_X64
};

/// The names the app shows for catalogue models (manifest id, display name). Every text
/// for the user names a model through [`display_name`], never by its manifest id; the
/// catalogue tests pin this list to the catalogue (one entry per model, no other).
pub const DISPLAY_NAMES: &[(&str, &str)] = &[
    ("tiny.en", "Whisper tiny.en"),
    ("base.en", "Whisper base.en"),
    ("small.en", "Whisper small.en"),
    ("small.en-q5_1", "Whisper small.en (compact)"),
    ("large-v3-turbo", "Whisper large-v3-turbo"),
    ("large-v3-turbo-q5_0", "Whisper large-v3-turbo (compact)"),
    ("parakeet-ctc-110m", "Parakeet CTC 110M"),
    ("parakeet-tdt-110m", "Parakeet 110M"),
    ("parakeet-tdt-0.6b-v2", "Parakeet 0.6B v2"),
    ("parakeet-tdt-0.6b-v3", "Parakeet 0.6B v3"),
    ("qwen3.5-9b-q4_k_m", "Qwen 3.5 9B"),
    ("qwen3.5-4b-q4_k_m", "Qwen 3.5 4B"),
    (HELPERS_MODEL, "Speech helpers"),
];

/// The name the user sees for model `id` ([`DISPLAY_NAMES`]); an id the list does not
/// know (a stored value of another version) is shown as it is.
#[must_use]
pub fn display_name(id: &str) -> &str {
    DISPLAY_NAMES
        .iter()
        .find(|(m, _)| *m == id)
        .map_or(id, |(_, name)| name)
}

/// The first catalogue model id that `text` names other than inside a display name
/// (`"Whisper base.en"` names no id; `"base.en is too slow"` names `base.en`): the check
/// that user text never shows a manifest id, including the artefact ids of the speech
/// helpers, which are never named to the user.
#[must_use]
pub fn manifest_id_in(text: &str) -> Option<&'static str> {
    let mut names: Vec<&str> = DISPLAY_NAMES.iter().map(|(_, n)| *n).collect();
    names.sort_by_key(|n| std::cmp::Reverse(n.len()));
    let mut rest = text.to_owned();
    for n in names {
        rest = rest.replace(n, "|");
    }
    let helper_files = Catalogue::embedded()
        .into_iter()
        .flat_map(|c| c.artefacts().iter())
        .filter(|a| a.is_helper())
        .map(|a| a.id.as_str());
    DISPLAY_NAMES
        .iter()
        .map(|(id, _)| *id)
        .chain(helper_files)
        .find(|id| rest.contains(id))
}

/// Why the catalogue, a path or an operation on a model failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CatalogueError {
    /// The manifest does not parse or breaks a rule (a bug in the embedded manifest).
    Manifest(String),
    /// A cache path that would leave the models directory.
    Path {
        /// The path as written.
        path: String,
        /// What is wrong with it.
        why: &'static str,
    },
    /// No catalogue model has this name.
    UnknownModel(String),
    /// The model is the chosen one (transcription or summary): choose another first.
    Chosen(String),
    /// A file could not be read or removed.
    Io {
        /// The file.
        path: PathBuf,
        /// What failed.
        error: String,
    },
}

impl fmt::Display for CatalogueError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Manifest(e) => write!(f, "the embedded models/manifest.toml: {e}"),
            Self::Path { path, why } => write!(f, "manifest path `{path}` {why}"),
            Self::UnknownModel(m) => write!(f, "model {m:?} is not in models/manifest.toml"),
            Self::Chosen(m) => write!(
                f,
                "{} is in use; choose another model first",
                display_name(m)
            ),
            Self::Io { path, error } => write!(f, "{}: {error}", path.display()),
        }
    }
}

impl std::error::Error for CatalogueError {}

fn io_error(path: &Path) -> impl FnOnce(io::Error) -> CatalogueError + '_ {
    move |e| CatalogueError::Io {
        path: path.to_path_buf(),
        error: e.to_string(),
    }
}

/// `root` joined with the manifest path `rel`, refusing anything that could leave `root`:
/// an empty path, a leading `/` or `\`, any `:` (drives, `C:x`, alternate streams), `..`,
/// a root or prefix component, or a path naming `root` itself. The rule of xtask's
/// `download::jail_join`.
///
/// # Errors
/// [`CatalogueError::Path`].
pub fn jail_join(root: &Path, rel: &str) -> Result<PathBuf, CatalogueError> {
    let bad = |why| CatalogueError::Path {
        path: rel.to_owned(),
        why,
    };
    if rel.is_empty() {
        return Err(bad("is empty"));
    }
    if rel.starts_with('/') || rel.starts_with('\\') || rel.contains(':') {
        return Err(bad("must be relative to the models directory"));
    }
    let mut out = root.to_path_buf();
    for comp in Path::new(rel).components() {
        match comp {
            Component::Normal(c) => out.push(c),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(bad("escapes the models directory"));
            }
        }
    }
    if out == root {
        return Err(bad("names the models directory itself"));
    }
    Ok(out)
}

/// The manifest as written (strict: unknown fields are refused, as by xtask's parser).
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawManifest {
    #[allow(dead_code)]
    sets: Vec<String>,
    #[allow(dead_code)]
    hosts: Vec<String>,
    #[serde(default)]
    #[allow(dead_code)]
    redirect_hosts: Vec<String>,
    #[serde(default, rename = "artefact")]
    artefacts: Vec<RawArtefact>,
    #[serde(default, rename = "model")]
    models: Vec<RawModel>,
    #[serde(default, rename = "ladder")]
    ladders: Vec<RawLadder>,
    #[serde(default, rename = "cuda_archive")]
    cuda_archives: Vec<RawCudaArchive>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawArtefact {
    id: String,
    sets: Vec<String>,
    location: String,
    url: String,
    #[serde(default)]
    #[allow(dead_code)]
    source_member: Option<String>,
    path: String,
    sha256: String,
    size: u64,
    #[allow(dead_code)]
    licence: String,
    #[allow(dead_code)]
    attribution: String,
    #[serde(default)]
    #[allow(dead_code)]
    publisher_checksum: Option<String>,
    #[serde(default)]
    extract: Option<RawExtract>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawExtract {
    format: String,
    into: String,
    #[serde(default, rename = "member")]
    members: Vec<Member>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawModel {
    name: String,
    engine: String,
    artefacts: Vec<String>,
    languages: Vec<String>,
    detects_language: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawLadder {
    backend: String,
    scope: String,
    models: Vec<String>,
    #[allow(dead_code)]
    source: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawCudaArchive {
    part: String,
    artefact: String,
    format: String,
    #[serde(default, rename = "file")]
    files: Vec<CudaFile>,
}

/// A downloadable file of the manifest (location `cache`, under the models directory).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Artefact {
    /// Unique id.
    pub id: String,
    /// The `cargo xtask fetch-models` sets that hold it.
    pub sets: Vec<String>,
    /// Its https source.
    pub url: String,
    /// Its path relative to the models directory (jail-checked).
    pub path: String,
    /// Lower-case hex SHA-256.
    pub sha256: String,
    /// Size in bytes.
    pub size: u64,
    /// Archive extraction, if the file is an archive.
    pub extract: Option<Extract>,
}

impl Artefact {
    /// Its file under `models`.
    ///
    /// # Errors
    /// [`CatalogueError::Path`] (the catalogue already refused such a path at parse).
    pub fn file(&self, models: &ModelDir) -> Result<PathBuf, CatalogueError> {
        jail_join(models.root(), &self.path)
    }

    /// Whether it is one of the speech helpers of either architecture.
    fn is_helper(&self) -> bool {
        self.sets
            .iter()
            .any(|s| s == HELPERS_SET_X64 || s == HELPERS_SET_ARM64)
    }
}

impl Extract {
    /// The extraction directory under `models`.
    fn dir(&self, models: &ModelDir) -> Result<PathBuf, CatalogueError> {
        jail_join(models.root(), &self.into)
    }

    /// Whether every pinned member is in `models` with its manifest size (never for an
    /// archive that pins nothing: nothing says what it should hold).
    fn members_present(&self, models: &ModelDir) -> Result<bool, CatalogueError> {
        let dir = self.dir(models)?;
        let mut present = !self.members.is_empty();
        for m in &self.members {
            present &= matches!(
                file_state(&jail_join(&dir, &m.path)?, m.size),
                FileState::Present
            );
        }
        Ok(present)
    }

    /// What is wrong with the pinned members in `models`, reading every byte.
    fn member_problems(&self, models: &ModelDir) -> Result<Vec<String>, CatalogueError> {
        let dir = self.dir(models)?;
        let mut problems = Vec::new();
        if self.members.is_empty() {
            problems.push("the manifest pins no file of this archive".to_owned());
        }
        for m in &self.members {
            let path = jail_join(&dir, &m.path)?;
            if let Some(what) = file_problem(&path, &m.sha256, m.size)? {
                problems.push(format!("{}: {what}", m.path));
            }
        }
        Ok(problems)
    }
}

/// What is wrong with the file at `path` (missing, size, digest; reads every byte).
pub(crate) fn file_problem(
    path: &Path,
    sha256: &str,
    size: u64,
) -> Result<Option<String>, CatalogueError> {
    Ok(match std::fs::metadata(path) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => Some("missing".to_owned()),
        Err(e) => return Err(io_error(path)(e)),
        Ok(m) if m.len() != size => Some(format!("{} bytes, the manifest says {size}", m.len())),
        Ok(_) => {
            let got = sha256_file(path).map_err(io_error(path))?;
            (!got.eq_ignore_ascii_case(sha256))
                .then(|| format!("sha256 {got}, the manifest says {sha256}"))
        }
    })
}

/// Extraction of an archive artefact.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Extract {
    /// `tar.gz`, `tar.bz2` or `zip`.
    pub format: String,
    /// Directory (relative to the models directory) the archive is extracted into.
    pub into: String,
    /// Extracted files whose checksum is pinned (paths relative to `into`).
    pub members: Vec<Member>,
}

/// A pinned file inside an extracted archive.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Member {
    /// Path relative to the extraction directory.
    pub path: String,
    /// Lower-case hex SHA-256.
    pub sha256: String,
    /// Size in bytes.
    pub size: u64,
}

/// What runs a model.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Engine {
    /// whisper.cpp (ggml).
    Whisper,
    /// Parakeet's CTC head (ONNX Runtime).
    ParakeetCtc,
    /// Parakeet's TDT transducer (ONNX Runtime).
    ParakeetTdt,
    /// llama.cpp (a GGUF summary model).
    Llama,
    /// ONNX Runtime and the speech models it runs ([`HELPERS_MODEL`]).
    Helpers,
}

impl Engine {
    fn parse(s: &str) -> Option<Self> {
        match s {
            "whisper" => Some(Self::Whisper),
            "parakeet-ctc" => Some(Self::ParakeetCtc),
            "parakeet-tdt" => Some(Self::ParakeetTdt),
            _ => None,
        }
    }
}

/// What a model is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelKind {
    /// Speech to text (`[[model]]`).
    Transcription,
    /// A local summary model (set `summary`).
    Summary,
    /// What every meeting needs ([`HELPERS_MODEL`]; never chosen, never removed).
    Helpers,
}

/// A model the user can choose, download and remove.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Model {
    /// Its name (`[[model]]` name; a summary model's artefact id).
    pub name: String,
    /// Transcription or summary.
    pub kind: ModelKind,
    /// What runs it.
    pub engine: Engine,
    /// The artefact ids it needs.
    pub artefacts: Vec<String>,
    /// Language codes it transcribes (empty for a summary model).
    pub languages: Vec<String>,
    /// Whether it detects the language itself.
    pub detects_language: bool,
}

/// One backend's transcription models for one scope, best first.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ladder {
    /// The backend.
    pub backend: BackendKind,
    /// English or multilingual meetings.
    pub scope: Scope,
    /// Model names, best first.
    pub models: Vec<String>,
}

/// An archive that fills part of the opt-in CUDA folder (`[[cuda_archive]]`, [`crate::cuda`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CudaArchive {
    /// The part its files belong to.
    pub part: CudaPart,
    /// The artefact that is the archive (its url, cache path, size and SHA-256).
    pub artefact: String,
    /// `zip` or `tar.bz2`.
    pub format: String,
    /// The files taken from it; nothing else of the archive is written.
    pub files: Vec<CudaFile>,
}

/// One file of the CUDA folder, taken from an archive.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CudaFile {
    /// Its name in the folder (no folders: everything lands flat, so a DLL finds its imports).
    pub name: String,
    /// Its path inside the archive.
    pub member: String,
    /// Lower-case hex SHA-256.
    pub sha256: String,
    /// Size in bytes.
    pub size: u64,
}

/// The parsed manifest (module docs).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Catalogue {
    artefacts: Vec<Artefact>,
    models: Vec<Model>,
    ladders: Vec<Ladder>,
    cuda_archives: Vec<CudaArchive>,
}

fn manifest_err(msg: impl Into<String>) -> CatalogueError {
    CatalogueError::Manifest(msg.into())
}

fn is_sha256(s: &str) -> bool {
    s.len() == 64
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// A path checked against a stand-in root (only its shape matters at parse).
fn check_path(rel: &str) -> Result<(), CatalogueError> {
    jail_join(Path::new("models"), rel).map(|_| ())
}

/// Every location an extraction into `into` uses: the folder, its temp and where it is put aside
/// (`archive::tmp_dir`, `archive::aside_dir`). The same list as xtask's `check_cache_overlaps`.
fn extract_cache_paths(into: &str) -> [String; 3] {
    [
        into.to_owned(),
        format!("{into}.tmp"),
        format!("{into}.old"),
    ]
}

/// Whether a manifest path is inside the CUDA folder. The rule of xtask's
/// `manifest::in_cuda_folder`, pinned by `fixtures/models/cuda_folder_cases.toml`.
fn in_cuda_folder(path: &str) -> bool {
    path.split(['/', '\\'])
        .find(|c| !c.is_empty() && *c != ".")
        .is_some_and(|c| c.eq_ignore_ascii_case(crate::cuda::FOLDER))
}

fn backend_of(s: &str) -> Option<BackendKind> {
    match s {
        "cuda" => Some(BackendKind::Cuda),
        "vulkan" => Some(BackendKind::Vulkan),
        "qnn" => Some(BackendKind::Qnn),
        "cpu" => Some(BackendKind::Cpu),
        _ => None,
    }
}

impl Catalogue {
    /// The catalogue of the manifest built into this binary, parsed once.
    ///
    /// # Errors
    /// As [`Catalogue::parse`] (a broken embedded manifest is a bug, reported every call).
    pub fn embedded() -> Result<&'static Self, CatalogueError> {
        static CATALOGUE: OnceLock<Result<Catalogue, CatalogueError>> = OnceLock::new();
        CATALOGUE
            .get_or_init(|| {
                let catalogue = Self::parse(MODELS_MANIFEST)?;
                if catalogue.model(HELPERS_MODEL).is_none() {
                    return Err(manifest_err(format!(
                        "no artefact is in set `{HELPERS_SET}`"
                    )));
                }
                Ok(catalogue)
            })
            .as_ref()
            .map_err(Clone::clone)
    }

    /// Parses and checks a models manifest (module docs).
    ///
    /// # Errors
    /// [`CatalogueError::Manifest`] for text that does not parse or a broken rule (an
    /// unknown engine, backend or scope, a model naming a missing artefact, a ladder
    /// naming an unknown model, a duplicate id, a model file that is not downloaded into
    /// the models directory, a bad URL or digest, a helper archive the app cannot extract);
    /// [`CatalogueError::Path`] for a cache path outside the models directory.
    pub fn parse(text: &str) -> Result<Self, CatalogueError> {
        let raw: RawManifest = toml::from_str(text).map_err(|e| manifest_err(e.to_string()))?;
        let mut ids = BTreeSet::new();
        let mut artefacts = Vec::new();
        for a in raw.artefacts {
            if !ids.insert(a.id.clone()) {
                return Err(manifest_err(format!("artefact id `{}` repeats", a.id)));
            }
            if a.location != "cache" {
                // Committed files (location `repo`) are not models; none is listed today.
                return Err(manifest_err(format!(
                    "artefact `{}` has location `{}`; models are downloaded (`cache`)",
                    a.id, a.location
                )));
            }
            if !a.url.starts_with("https://") {
                return Err(manifest_err(format!(
                    "artefact `{}` url is not https",
                    a.id
                )));
            }
            if !is_sha256(&a.sha256) {
                return Err(manifest_err(format!(
                    "artefact `{}` sha256 is not lower-case hex",
                    a.id
                )));
            }
            check_path(&a.path)?;
            check_path(&format!("{}.part", a.path))?;
            let extract = match a.extract {
                None => None,
                Some(x) => {
                    for path in extract_cache_paths(&x.into) {
                        check_path(&path)?;
                    }
                    for m in &x.members {
                        check_path(&m.path)?;
                        check_path(&format!("{}/{}", x.into, m.path))?;
                        if !is_sha256(&m.sha256) {
                            return Err(manifest_err(format!(
                                "artefact `{}` member `{}` sha256 is not lower-case hex",
                                a.id, m.path
                            )));
                        }
                    }
                    Some(Extract {
                        format: x.format,
                        into: x.into,
                        members: x.members,
                    })
                }
            };
            artefacts.push(Artefact {
                id: a.id,
                sets: a.sets,
                url: a.url,
                path: a.path,
                sha256: a.sha256,
                size: a.size,
                extract,
            });
        }
        let mut names = BTreeSet::new();
        let mut models = Vec::new();
        for m in raw.models {
            let engine = Engine::parse(&m.engine)
                .ok_or_else(|| manifest_err(format!("model `{}` engine `{}`", m.name, m.engine)))?;
            Self::check_model_artefacts(&artefacts, &m.name, &m.artefacts)?;
            if !names.insert(m.name.clone()) {
                return Err(manifest_err(format!("model `{}` repeats", m.name)));
            }
            models.push(Model {
                name: m.name,
                kind: ModelKind::Transcription,
                engine,
                artefacts: m.artefacts,
                languages: m.languages,
                detects_language: m.detects_language,
            });
        }
        for a in artefacts
            .iter()
            .filter(|a| a.sets.iter().any(|s| s == SUMMARY_SET))
        {
            if a.extract.is_some() {
                return Err(manifest_err(format!(
                    "summary model `{}` is an archive",
                    a.id
                )));
            }
            if !names.insert(a.id.clone()) {
                return Err(manifest_err(format!("model `{}` repeats", a.id)));
            }
            models.push(Model {
                name: a.id.clone(),
                kind: ModelKind::Summary,
                engine: Engine::Llama,
                artefacts: vec![a.id.clone()],
                languages: Vec::new(),
                detects_language: false,
            });
        }
        let helpers = Self::helpers_of(&artefacts, HELPERS_SET)?;
        if !helpers.is_empty() {
            if !names.insert(HELPERS_MODEL.to_owned()) {
                return Err(manifest_err(format!("model `{HELPERS_MODEL}` repeats")));
            }
            models.push(Model {
                name: HELPERS_MODEL.to_owned(),
                kind: ModelKind::Helpers,
                engine: Engine::Helpers,
                artefacts: helpers,
                languages: Vec::new(),
                detects_language: false,
            });
        }
        let mut ladders = Vec::new();
        for l in raw.ladders {
            let backend = backend_of(&l.backend)
                .ok_or_else(|| manifest_err(format!("ladder backend `{}`", l.backend)))?;
            let scope = match l.scope.as_str() {
                "english" => Scope::English,
                "multilingual" => Scope::Multilingual,
                other => return Err(manifest_err(format!("ladder scope `{other}`"))),
            };
            for name in &l.models {
                if !models
                    .iter()
                    .any(|m| m.name == *name && m.kind == ModelKind::Transcription)
                {
                    return Err(manifest_err(format!(
                        "ladder {} {} names unknown model `{name}`",
                        l.backend, l.scope
                    )));
                }
            }
            ladders.push(Ladder {
                backend,
                scope,
                models: l.models,
            });
        }
        let cuda_archives = Self::cuda_archives_of(&artefacts, raw.cuda_archives)?;
        Ok(Self {
            artefacts,
            models,
            ladders,
            cuda_archives,
        })
    }

    /// Checks `[[cuda_archive]]` (the rules of xtask's `Manifest::validate_cuda`): a known part
    /// and an extractable format; an artefact that exists, used by one archive only; every file
    /// a plain name, unique without regard to case across both parts, its digest and size given
    /// and equal to the artefact's own pin of that member if it has one; and no artefact stored
    /// inside the CUDA folder, whose contents are the parts' alone.
    fn cuda_archives_of(
        artefacts: &[Artefact],
        raw: Vec<RawCudaArchive>,
    ) -> Result<Vec<CudaArchive>, CatalogueError> {
        for a in artefacts {
            let into = a.extract.as_ref().map_or("", |x| x.into.as_str());
            if in_cuda_folder(&a.path) || in_cuda_folder(into) {
                return Err(manifest_err(format!(
                    "artefact `{}` is stored inside the CUDA folder",
                    a.id
                )));
            }
        }
        let mut used = BTreeSet::new();
        let mut names = BTreeSet::new();
        let mut archives = Vec::new();
        for r in raw {
            let ctx = format!("cuda_archive `{}`", r.artefact);
            let part = CudaPart::parse(&r.part)
                .ok_or_else(|| manifest_err(format!("{ctx}: unknown part `{}`", r.part)))?;
            if archive::Format::parse(&r.format).is_none() {
                return Err(manifest_err(format!(
                    "{ctx}: the app cannot extract format `{}`",
                    r.format
                )));
            }
            let artefact = artefacts
                .iter()
                .find(|a| a.id == r.artefact)
                .ok_or_else(|| manifest_err(format!("{ctx}: unknown artefact")))?;
            if !used.insert(r.artefact.clone()) {
                return Err(manifest_err(format!("{ctx}: listed twice")));
            }
            if r.files.is_empty() {
                return Err(manifest_err(format!("{ctx}: names no file")));
            }
            for f in &r.files {
                let plain = !f.name.is_empty()
                    && f.name != "."
                    && f.name != ".."
                    && !f.name.contains(['/', '\\', ':']);
                if !plain {
                    return Err(manifest_err(format!(
                        "{ctx}: file name `{}` is not a plain name",
                        f.name
                    )));
                }
                if !names.insert(f.name.to_lowercase()) {
                    return Err(manifest_err(format!(
                        "{ctx}: file name `{}` repeats",
                        f.name
                    )));
                }
                check_path(&f.member)?;
                if !is_sha256(&f.sha256) || f.size == 0 {
                    return Err(manifest_err(format!(
                        "{ctx}: file `{}` needs a sha256 and a size",
                        f.name
                    )));
                }
                let pin = artefact
                    .extract
                    .as_ref()
                    .and_then(|x| x.members.iter().find(|m| m.path == f.member));
                if pin.is_some_and(|m| m.sha256 != f.sha256 || m.size != f.size) {
                    return Err(manifest_err(format!(
                        "{ctx}: file `{}` differs from the artefact's own pin",
                        f.name
                    )));
                }
            }
            archives.push(CudaArchive {
                part,
                artefact: r.artefact,
                format: r.format,
                files: r.files,
            });
        }
        Ok(archives)
    }

    /// The archives of the opt-in CUDA folder, in manifest order ([`crate::cuda`]).
    #[must_use]
    pub fn cuda_archives(&self) -> &[CudaArchive] {
        &self.cuda_archives
    }

    /// The artefact ids of helpers set `set`; an archive among them must be one the app can
    /// extract and must pin what it holds (only pinned members are extracted).
    fn helpers_of(artefacts: &[Artefact], set: &str) -> Result<Vec<String>, CatalogueError> {
        let mut ids = Vec::new();
        for a in artefacts.iter().filter(|a| a.sets.iter().any(|s| s == set)) {
            if let Some(x) = &a.extract
                && (archive::Format::parse(&x.format).is_none() || x.members.is_empty())
            {
                return Err(manifest_err(format!(
                    "helper `{}` is an archive the app cannot extract (format `{}`, {} members)",
                    a.id,
                    x.format,
                    x.members.len()
                )));
            }
            ids.push(a.id.clone());
        }
        Ok(ids)
    }

    fn check_model_artefacts(
        artefacts: &[Artefact],
        model: &str,
        ids: &[String],
    ) -> Result<(), CatalogueError> {
        if ids.is_empty() {
            return Err(manifest_err(format!("model `{model}` has no artefacts")));
        }
        for id in ids {
            let a = artefacts.iter().find(|a| a.id == *id).ok_or_else(|| {
                manifest_err(format!("model `{model}` names unknown artefact `{id}`"))
            })?;
            // Removal reads files, not extracted archives; only the speech helpers, which
            // are never removed, are archives.
            if a.extract.is_some() {
                return Err(manifest_err(format!(
                    "model `{model}` artefact `{id}` is an archive"
                )));
            }
        }
        Ok(())
    }

    /// Every artefact, in manifest order.
    #[must_use]
    pub fn artefacts(&self) -> &[Artefact] {
        &self.artefacts
    }

    /// The artefact `id`.
    #[must_use]
    pub fn artefact(&self, id: &str) -> Option<&Artefact> {
        self.artefacts.iter().find(|a| a.id == id)
    }

    /// Every model, transcription models first, in manifest order.
    #[must_use]
    pub fn models(&self) -> &[Model] {
        &self.models
    }

    /// The model `name` (matched exactly).
    #[must_use]
    pub fn model(&self, name: &str) -> Option<&Model> {
        self.models.iter().find(|m| m.name == name)
    }

    /// The model that `path`, a file under any models directory, belongs to: an artefact's
    /// file or a pinned member of its archive. `None` for a file no model holds (a library of
    /// a set the app does not offer, a local NPU conversion).
    #[must_use]
    pub fn model_of_file(&self, path: &Path) -> Option<&Model> {
        let holds = |a: &Artefact| {
            path.ends_with(&a.path)
                || a.extract.as_ref().is_some_and(|x| {
                    x.members
                        .iter()
                        .any(|m| path.ends_with(Path::new(&x.into).join(&m.path)))
                })
        };
        self.models.iter().find(|m| {
            m.artefacts
                .iter()
                .filter_map(|id| self.artefact(id))
                .any(holds)
        })
    }

    /// Whether the speech helpers are installed in the models folder that holds `path`, a file
    /// some model's artefact names; `None` when `path` is no such file. Sizes only.
    #[must_use]
    pub fn helpers_beside(&self, path: &Path) -> Option<bool> {
        let rel = self
            .artefacts
            .iter()
            .find(|a| path.ends_with(&a.path))
            .map(|a| Path::new(&a.path))?;
        let keep = path
            .components()
            .count()
            .checked_sub(rel.components().count())?;
        let root: PathBuf = path.components().take(keep).collect();
        Some(self.is_installed(&ModelDir::new(root), HELPERS_MODEL))
    }

    /// The models of one kind, in manifest order.
    pub fn models_of(&self, kind: ModelKind) -> impl Iterator<Item = &Model> {
        self.models.iter().filter(move |m| m.kind == kind)
    }

    /// Every ladder, in manifest order.
    #[must_use]
    pub fn ladders(&self) -> &[Ladder] {
        &self.ladders
    }

    /// The ladder of `backend` and `scope`, if the manifest has one.
    #[must_use]
    pub fn ladder(&self, backend: BackendKind, scope: Scope) -> Option<&Ladder> {
        self.ladders
            .iter()
            .find(|l| l.backend == backend && l.scope == scope)
    }

    fn require(&self, name: &str) -> Result<&Model, CatalogueError> {
        self.model(name)
            .ok_or_else(|| CatalogueError::UnknownModel(name.to_owned()))
    }

    /// The artefacts model `name` needs.
    ///
    /// # Errors
    /// [`CatalogueError::UnknownModel`].
    pub fn artefacts_of(&self, name: &str) -> Result<Vec<&Artefact>, CatalogueError> {
        let model = self.require(name)?;
        Ok(model
            .artefacts
            .iter()
            .filter_map(|id| self.artefact(id))
            .collect())
    }

    /// The bytes model `name` downloads (all its artefacts).
    ///
    /// # Errors
    /// [`CatalogueError::UnknownModel`].
    pub fn download_size(&self, name: &str) -> Result<u64, CatalogueError> {
        Ok(self.artefacts_of(name)?.iter().map(|a| a.size).sum())
    }

    /// Model `name`'s files in `models`, from sizes only (module docs).
    ///
    /// # Errors
    /// [`CatalogueError::UnknownModel`]; [`CatalogueError::Path`].
    pub fn install_state(&self, models: &ModelDir, name: &str) -> Result<Install, CatalogueError> {
        let mut entries = Vec::new();
        for a in self.artefacts_of(name)? {
            entries.push((a.id.clone(), a.size, artefact_state(a, models)?));
        }
        Ok(Install::from_files(entries))
    }

    /// Puts back every extraction a crash left aside in `models` (`archive::restore_aside`), so
    /// the install needs no new download. Call it at start, before any download can run: it would
    /// otherwise race that download's own swap. Goes on past a failure, which costs only that
    /// download; returns how many came back.
    #[must_use]
    pub fn recover_extractions(&self, models: &ModelDir) -> usize {
        self.artefacts
            .iter()
            .filter_map(|a| a.extract.as_ref()?.dir(models).ok())
            .filter(|into| matches!(archive::restore_aside(into), Ok(true)))
            .count()
    }

    /// Whether every file of model `name` is in `models` with its manifest size.
    #[must_use]
    pub fn is_installed(&self, models: &ModelDir, name: &str) -> bool {
        self.install_state(models, name)
            .is_ok_and(|i| i.state == InstallState::Installed)
    }

    /// The full check of model `name`: every artefact's size and SHA-256, for an archive
    /// every pinned member's instead (reads every byte). Empty when all match.
    ///
    /// # Errors
    /// [`CatalogueError::UnknownModel`]; [`CatalogueError::Path`]; [`CatalogueError::Io`]
    /// for a file that exists but cannot be read.
    pub fn verify(&self, models: &ModelDir, name: &str) -> Result<Vec<Problem>, CatalogueError> {
        let mut problems = Vec::new();
        for a in self.artefacts_of(name)? {
            let found = match &a.extract {
                Some(x) => x.member_problems(models)?,
                None => file_problem(&a.file(models)?, &a.sha256, a.size)?
                    .into_iter()
                    .collect(),
            };
            problems.extend(found.into_iter().map(|what| Problem {
                artefact: a.id.clone(),
                what,
            }));
        }
        Ok(problems)
    }

    /// Removes model `name`'s files and partial downloads from `models` (module docs),
    /// unless it is one of `chosen` (the chosen transcription and summary models, the stored
    /// check's model and fallback: `host::protected_models`),
    /// [`crate::host::FALLBACK_MODEL`] or the speech helpers, which are always kept. A file
    /// that another installed model also uses is kept and listed.
    ///
    /// # Errors
    /// [`CatalogueError::Chosen`] (nothing removed); [`CatalogueError::UnknownModel`];
    /// [`CatalogueError::Path`]; [`CatalogueError::Io`] when a file cannot be removed
    /// (files removed before it stay removed and are not counted).
    pub fn remove(
        &self,
        models: &ModelDir,
        name: &str,
        chosen: &[&str],
    ) -> Result<Removed, CatalogueError> {
        let model = self.require(name)?;
        if chosen.contains(&name)
            || name == crate::host::FALLBACK_MODEL
            || model.kind == ModelKind::Helpers
        {
            return Err(CatalogueError::Chosen(name.to_owned()));
        }
        let shared: BTreeSet<&str> = self
            .models
            .iter()
            .filter(|m| m.name != name && self.is_installed(models, &m.name))
            .flat_map(|m| m.artefacts.iter().map(String::as_str))
            .collect();
        let mut removed = Removed::default();
        for a in self.artefacts_of(name)? {
            if shared.contains(a.id.as_str()) {
                removed.kept_shared.push(a.id.clone());
                continue;
            }
            let file = a.file(models)?;
            for path in [part_path(&file), file] {
                let size = match std::fs::metadata(&path) {
                    Ok(m) if m.is_file() => m.len(),
                    Ok(_) => continue,
                    Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
                    Err(e) => return Err(io_error(&path)(e)),
                };
                match std::fs::remove_file(&path) {
                    Ok(()) => {
                        removed.bytes_freed += size;
                        removed.files.push(path);
                    }
                    Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                    Err(e) => return Err(io_error(&path)(e)),
                }
            }
        }
        Ok(removed)
    }
}

/// Fetches one file: `(url, dest, expected, cancel, progress(done, total))`, as
/// `download::Downloader::fetch`.
pub type FetchFn<'a> = dyn FnMut(
        &Url,
        &Path,
        &Expected<'_>,
        &CancelToken,
        &mut dyn FnMut(u64, u64),
    ) -> Result<Outcome, DownloadError>
    + 'a;

/// Why [`Catalogue::download`] failed.
#[derive(Debug)]
pub enum ModelDownloadError {
    /// An unknown model, a bad path or URL.
    Catalogue(CatalogueError),
    /// An artefact's download failed.
    Download {
        /// The artefact id.
        artefact: String,
        /// What failed.
        error: DownloadError,
    },
    /// A downloaded archive could not be extracted; the archive is kept.
    Extract {
        /// The artefact id.
        artefact: String,
        /// What failed.
        error: ExtractError,
    },
}

impl ModelDownloadError {
    /// A failed extraction of artefact `a`; a cancelled one is a cancelled download (the archive
    /// is kept, so a later run picks up there).
    pub(crate) fn of_extraction(a: &Artefact, error: ExtractError) -> Self {
        match error {
            ExtractError::Cancelled => Self::Download {
                artefact: a.id.clone(),
                error: DownloadError::Cancelled {
                    done: a.size,
                    total: a.size,
                },
            },
            error => Self::Extract {
                artefact: a.id.clone(),
                error,
            },
        }
    }

    /// Whether it was cancelled (the `.part` is kept).
    #[must_use]
    pub fn cancelled(&self) -> bool {
        matches!(
            self,
            Self::Download {
                error: DownloadError::Cancelled { .. },
                ..
            }
        )
    }

    /// Whether starting it again can succeed without the user changing anything (a kept
    /// `.part`, a network or server failure).
    #[must_use]
    pub fn retryable(&self) -> bool {
        match self {
            Self::Catalogue(_) => false,
            Self::Download { error, .. } => {
                error.resumable()
                    || matches!(
                        error,
                        DownloadError::Request { .. }
                            | DownloadError::Status { .. }
                            | DownloadError::Verification { .. }
                            | DownloadError::TooLong { .. }
                    )
            }
            // A full disk can clear; an archive that matched its pin and is refused cannot.
            Self::Extract { error, .. } => {
                matches!(error, ExtractError::Io { .. } | ExtractError::InUse { .. })
            }
        }
    }
}

impl fmt::Display for ModelDownloadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Catalogue(e) => write!(f, "{e}"),
            Self::Download { error, .. } => write!(f, "{error}"),
            Self::Extract { error, .. } => write!(f, "{error}"),
        }
    }
}

impl std::error::Error for ModelDownloadError {}

impl Catalogue {
    /// Downloads model `name` into `models`: each artefact in manifest order through `fetch`
    /// (a file already present with its size and SHA-256 costs no request; the fetch checks
    /// that). `progress(done, total)` counts bytes over the whole model: the artefacts
    /// finished plus the current one's bytes held. Stops at the first failure (a
    /// cancellation is [`DownloadError::Cancelled`]); the artefacts finished before stay. An
    /// archive whose pinned members are already in place, with their SHA-256 (otherwise it is
    /// downloaded again, which repairs them), costs no request; a downloaded one is extracted
    /// and deleted (module docs).
    ///
    /// # Errors
    /// [`ModelDownloadError`].
    pub fn download(
        &self,
        models: &ModelDir,
        name: &str,
        cancel: &CancelToken,
        progress: &mut dyn FnMut(u64, u64),
        fetch: &mut FetchFn<'_>,
    ) -> Result<(), ModelDownloadError> {
        let artefacts = self
            .artefacts_of(name)
            .map_err(ModelDownloadError::Catalogue)?;
        let total: u64 = artefacts.iter().map(|a| a.size).sum();
        let mut finished = 0u64;
        progress(0, total);
        for a in artefacts {
            if let Some(x) = &a.extract {
                // A crash's copy costs no request; a failure here is reported by the extraction.
                if let Ok(into) = x.dir(models) {
                    let _ = archive::restore_aside(&into);
                }
                let problems = x
                    .member_problems(models)
                    .map_err(ModelDownloadError::Catalogue)?;
                if problems.is_empty() {
                    finished += a.size;
                    progress(finished, total);
                    continue;
                }
            }
            let dest = a.file(models).map_err(ModelDownloadError::Catalogue)?;
            let url = Url::parse(&a.url).map_err(|e| {
                ModelDownloadError::Catalogue(CatalogueError::Manifest(format!(
                    "artefact `{}` url: {e}",
                    a.id
                )))
            })?;
            let exp = Expected {
                sha256: &a.sha256,
                size: a.size,
            };
            fetch(&url, &dest, &exp, cancel, &mut |done, _| {
                progress(finished + done.min(a.size), total);
            })
            .map_err(|error| ModelDownloadError::Download {
                artefact: a.id.clone(),
                error,
            })?;
            if let Some(x) = &a.extract {
                extract_download(a, x, &dest, models, cancel)?;
            }
            finished += a.size;
            progress(finished, total);
        }
        Ok(())
    }
}

/// Extracts the archive just verified at `archive`, then deletes it (the members are what a
/// run reads; a failure to delete it only costs disk). A cancel before or during the extraction,
/// or a failed extraction, keeps the archive for the next run.
fn extract_download(
    a: &Artefact,
    x: &Extract,
    archive: &Path,
    models: &ModelDir,
    cancel: &CancelToken,
) -> Result<(), ModelDownloadError> {
    if cancel.is_cancelled() {
        return Err(ModelDownloadError::Download {
            artefact: a.id.clone(),
            error: DownloadError::Cancelled {
                done: a.size,
                total: a.size,
            },
        });
    }
    let into = x.dir(models).map_err(ModelDownloadError::Catalogue)?;
    archive::extract_verified(archive, x, &into, &a.sha256, cancel)
        .map_err(|error| ModelDownloadError::of_extraction(a, error))?;
    let _ = std::fs::remove_file(archive);
    Ok(())
}

/// One artefact's file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileState {
    /// The file has its manifest size.
    Present,
    /// No file, but a `.part` holding this many bytes (at most the manifest size counts).
    Partial(u64),
    /// A file of another size (an interrupted copy): replaced by the next download.
    WrongSize(u64),
    /// Neither the file nor a `.part`.
    Missing,
}

pub(crate) fn file_state(path: &Path, size: u64) -> FileState {
    match std::fs::metadata(path) {
        Ok(m) if m.is_file() && m.len() == size => FileState::Present,
        Ok(m) if m.is_file() => FileState::WrongSize(m.len()),
        _ => match std::fs::metadata(part_path(path)) {
            Ok(m) if m.is_file() => FileState::Partial(m.len()),
            _ => FileState::Missing,
        },
    }
}

/// One artefact on disk: its file; for an archive, present once its pinned members are, and
/// a complete archive that is not extracted yet is partial (the extraction is still to come).
fn artefact_state(a: &Artefact, models: &ModelDir) -> Result<FileState, CatalogueError> {
    let file = file_state(&a.file(models)?, a.size);
    let Some(x) = &a.extract else {
        return Ok(file);
    };
    if x.members_present(models)? {
        return Ok(FileState::Present);
    }
    Ok(match file {
        FileState::Present => FileState::Partial(a.size),
        other => other,
    })
}

/// A model's install state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstallState {
    /// Every file has its manifest size.
    Installed,
    /// Some bytes are held (complete files or `.part`s), not all.
    Partial,
    /// Nothing is held.
    Missing,
}

/// A model's files on disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Install {
    /// The summary.
    pub state: InstallState,
    /// Bytes held towards the download: complete files plus `.part` bytes (each at most
    /// its artefact's size); a file of the wrong size counts nothing.
    pub held: u64,
    /// The download size.
    pub total: u64,
    /// Each artefact's file.
    pub files: Vec<(String, FileState)>,
}

impl Install {
    /// From each artefact's id, manifest size and file state.
    fn from_files(entries: Vec<(String, u64, FileState)>) -> Self {
        let total = entries.iter().map(|(_, size, _)| size).sum();
        let installed = entries.iter().all(|(_, _, f)| *f == FileState::Present);
        let held = entries
            .iter()
            .map(|(_, size, f)| match f {
                FileState::Present => *size,
                FileState::Partial(n) => (*n).min(*size),
                FileState::WrongSize(_) | FileState::Missing => 0,
            })
            .sum();
        let state = if installed {
            InstallState::Installed
        } else if held > 0 {
            InstallState::Partial
        } else {
            InstallState::Missing
        };
        Self {
            state,
            held,
            total,
            files: entries.into_iter().map(|(id, _, f)| (id, f)).collect(),
        }
    }
}

/// One artefact that failed [`Catalogue::verify`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Problem {
    /// The artefact id.
    pub artefact: String,
    /// What is wrong (missing, size, digest).
    pub what: String,
}

/// What [`Catalogue::remove`] did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Removed {
    /// Bytes of the files removed (complete files and `.part`s).
    pub bytes_freed: u64,
    /// The files removed.
    pub files: Vec<PathBuf>,
    /// Artefacts kept because another installed model uses them.
    pub kept_shared: Vec<String>,
    /// What a CUDA part's removal left in the folder that belongs to no part (`crate::cuda`).
    pub left_in_folder: Vec<String>,
}
