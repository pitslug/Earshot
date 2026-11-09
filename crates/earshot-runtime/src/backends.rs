//! Production backends (PLAN.md 3.4, T6.4d): where models live, which accelerator runs
//! them, and the factories that turn them into the live and post-meeting stages.
//!
//! - **Models**: [`ModelDir`] is `EARSHOT_MODELS` when set (development), else the data
//!   directory's `models\` ([`earshot_store::DataDir::models_dir`]). A missing model is
//!   [`RuntimeError::ModelMissing`] naming the `cargo xtask fetch-models` set that
//!   provides it; nothing is downloaded here (the in-app download is T7.4).
//! - **Accelerators**: [`accelerator_order`] is CUDA (a `cuda` build, or the shipped x64 build
//!   after [`crate::cuda::use_for_transcription`], for Parakeet only), then QNN (the
//!   Snapdragon NPU, an ARM64 `qnn` build, T8.3), then Vulkan (a `vulkan` build on a
//!   machine with a Vulkan device, T6.10d), then the CPU (PLAN.md 3.4, D20). Vulkan runs
//!   whisper.cpp only; ONNX Runtime models refuse it. QNN runs ONNX models only, so it is
//!   offered to Parakeet models alone ([`model_order`]); the x64 QNN package is Qualcomm's HTP
//!   simulator and is never auto-selected. A run whose model came from the model check
//!   starts at the backend the check kept ([`order_from`]): Vulkan is used only where it beat
//!   the CPU.
//! - **ONNX Runtime**: [`init_onnx`] loads Microsoft's official library for the chosen
//!   accelerator from the models directory, checksum-verified by `earshot-onnx`; there is
//!   one ONNX Runtime per process, so choose once ([`onnx_library`]: the CUDA library only
//!   when a Parakeet model may run on CUDA; for QNN the CPU library with Qualcomm's QNN
//!   plugin registered into it, `earshot_onnx::qnn`). Silero VAD ([`load_silero`]), pyannote
//!   segmentation and CAM++ ([`load_diarisation`]) run on the CPU in production (T6.11:
//!   CAM++ on CUDA pays about 110 ms of setup per new input length and is twice as slow as
//!   the CPU even without it, private notes), with ONNX Runtime's CPU threads capped
//!   by [`onnx_cpu_threads`]; a provider that cannot run is an error, never a silent CPU
//!   fallback (`earshot-onnx`). Every speaker-model session, live or final pass, runs with
//!   [`speaker_threads`]: CAM++ output changes with the thread count, and the final pass
//!   reuses the live speaker windows only when it cannot change a result (T6.13d2).
//! - **whisper.cpp**: [`select_whisper`] tries the accelerators in order through a
//!   [`WhisperLoader`], which must verify the backend that actually ran (whisper.cpp falls
//!   back to the CPU silently; AGENTS.md: trusted only when `backend_info()` and the
//!   whisper log agree). The production loader is [`EvidenceLoader`]
//!   (`earshot_asr_whisper::evidence::load_verified`); its [`VerifiedWhisper`] also checks
//!   every call's log (`evidence::check_call_log`), so a per-call GPU fallback shows in
//!   `backend_info()`, the post-meeting warnings and the recorded pass backend.
//! - **Parakeet** (T6.10e3): the NVIDIA Parakeet models of [`PARAKEET_MODELS`] run on
//!   ONNX Runtime ([`load_parakeet`]); the backend is proven from ONNX Runtime's node
//!   placement of every session (a requested GPU that does not run them is a refusal,
//!   never a CPU session). [`select_asr`] loads a whisper or Parakeet model by name
//!   ([`asr_model`]) on the first verified accelerator of an order.
//! - **Factories**: [`live_asr_factory`] (the transcription model + Silero per track for
//!   [`crate::live::start_live`]), [`with_live_speakers`] (adds pyannote and CAM++ per
//!   track for speaker windows during the meeting, T6.13d2) and [`post_processor`]
//!   (final-pass model, pyannote, CAM++).
//! - **Capture** (Windows): [`start_wasapi_session`] opens the default microphone and, in a
//!   call, the loopback source, and starts a live session on them; in the room
//!   (`CaptureMode::InRoom`, T6.27) the session has the mic track only, so the pipeline runs
//!   no echo canceller and the final pass uses the in-person policy. The WASAPI channel holds at most
//!   `earshot_capture_wasapi::LIVE_QUEUE_CAP` (30 s) of audio: the session pumps every
//!   10 ms while idle, so only a stalled consumer reaches it (frames are then dropped and
//!   the gap is marked `DataLost`).

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use earshot_asr_whisper::{WhisperConfig, WhisperTranscriber, evidence};
use earshot_core::asr::{
    AsrError, BackendInfo, BackendKind, CancelToken, ChunkerConfig, PieceConfig, TranscribeOptions,
    Transcriber, Transcription,
};
use earshot_core::audio::MonoBuffer;
use earshot_core::capture::TrackId;
use earshot_core::diar::{DiarizationConfig, Diarizer, SpeakerEmbedder, SpeakerSegmenter};
use earshot_core::identity::EntityId;
use earshot_core::model::TranscriptPass;
use earshot_core::pipeline::LiveAsr;
use earshot_onnx::embed::CamppEmbedder;
use earshot_onnx::parakeet::{ParakeetCtc, ParakeetTdt};
use earshot_onnx::segment::PyannoteSegmenter;
use earshot_onnx::silero::SileroVad;
use earshot_onnx::{Provider, SessionBackend, SessionConfig};
use earshot_store::{DataDir, Store};
use rand_chacha::ChaCha20Rng;
use rand_core::{Rng, SeedableRng};

use crate::live::{LiveAsrFactory, LiveSpeakers, TrackAsr};
use crate::local::{LocalKey, MeetingRecord};
use crate::post::{PostConfig, PostProcessor};
use crate::{BoxRng, RuntimeError, SharedClock};

#[cfg(test)]
#[path = "backends_tests.rs"]
pub(crate) mod tests;

/// Silero VAD, relative to the models directory (manifest set `ci`).
pub const SILERO: &str = "silero/silero_vad.onnx";
/// pyannote segmentation-3.0 (extracted member; set `ci`).
pub const SEGMENTATION: &str =
    "pyannote-segmentation-3.0/sherpa-onnx-pyannote-segmentation-3-0/model.onnx";
/// CAM++ VoxCeleb speaker embedding (set `ci`).
pub const CAMPPLUS: &str = "embedding/3dspeaker_speech_campplus_sv_en_voxceleb_16k.onnx";
/// Windows batch size for pyannote segmentation.
const SEGMENTER_BATCH: usize = 32;

/// The whisper models of `models/manifest.toml` and the fetch set of each (PLAN D22: `ci`
/// and `desktop` hold the defaults, `choice` the models offered but never tested).
pub const WHISPER_MODELS: &[(&str, &str)] = &[
    ("tiny.en", "choice"),
    ("base.en", "ci"),
    ("small.en", "desktop"),
    ("small.en-q5_1", "choice"),
    ("large-v3-turbo", "choice"),
    ("large-v3-turbo-q5_0", "choice"),
];

/// The ggml whisper model `name` (`base.en`, `large-v3-turbo`, ...), relative to the models
/// directory.
///
/// # Errors
/// [`RuntimeError::UnknownModel`] for a name not in the manifest ([`WHISPER_MODELS`]);
/// names are matched exactly, so nothing can leave the models directory.
pub fn whisper_model(name: &str) -> Result<String, RuntimeError> {
    WHISPER_MODELS
        .iter()
        .find(|(n, _)| *n == name)
        .map(|(n, _)| format!("whisper/ggml-{n}.bin"))
        .ok_or_else(|| RuntimeError::UnknownModel(name.to_owned()))
}

/// How a Parakeet model decodes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParakeetDecoder {
    /// The CTC head (`model.onnx`).
    Ctc,
    /// The TDT transducer (`encoder.onnx`, `decoder.onnx`, `joiner.onnx`).
    Tdt,
}

/// A Parakeet model of `models/manifest.toml` (T6.10e3; community conversions from set
/// `eval`, the defaults also in `ci` and `desktop`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ParakeetModel {
    /// The model name (`--model`).
    pub name: &'static str,
    /// Its directory, relative to the models directory.
    pub dir: &'static str,
    /// How it decodes.
    pub decoder: ParakeetDecoder,
    /// Whether the encoder's weights are in `encoder.weights` next to `encoder.onnx`.
    pub external_weights: bool,
    /// English only (otherwise the language is detected).
    pub english_only: bool,
    /// The smallest `cargo xtask fetch-models` set that provides its files (PLAN D22: the
    /// defaults in `ci`, v3 in `choice`, the CTC head only in `eval`).
    pub fetch_set: &'static str,
}

impl ParakeetModel {
    /// The directory of its NPU conversion, relative to the models directory
    /// (`parakeet-qnn/<name>`; TDT models only): a static-shape, QDQ-quantised encoder for
    /// Qualcomm's HTP backend plus the float prediction network and joint, which run on the
    /// CPU. Built by `cargo xtask qnn-convert --model <name>` (T8.3); the conversions of the NPU
    /// ladder's models are also published as their NPU versions (manifest set `qnn-models`,
    /// T8.4g), which the ARM64 app downloads from its Models page.
    #[must_use]
    pub fn qnn_dir(&self) -> Option<String> {
        (self.decoder == ParakeetDecoder::Tdt).then(|| format!("parakeet-qnn/{}", self.name))
    }

    /// The files of its NPU conversion, relative to the models directory (empty without one).
    #[must_use]
    pub fn qnn_files(&self) -> Vec<String> {
        self.qnn_dir().map_or_else(Vec::new, |dir| {
            [
                "encoder.onnx",
                "encoder.weights",
                "decoder.onnx",
                "joiner.onnx",
                "tokens.txt",
                "conversion.json",
            ]
            .iter()
            .map(|f| format!("{dir}/{f}"))
            .collect()
        })
    }

    /// Its files, relative to the models directory.
    #[must_use]
    pub fn files(&self) -> Vec<String> {
        let names: &[&str] = match (self.decoder, self.external_weights) {
            (ParakeetDecoder::Ctc, _) => &["model.onnx", "tokens.txt"],
            (ParakeetDecoder::Tdt, false) => {
                &["encoder.onnx", "decoder.onnx", "joiner.onnx", "tokens.txt"]
            }
            (ParakeetDecoder::Tdt, true) => &[
                "encoder.onnx",
                "encoder.weights",
                "decoder.onnx",
                "joiner.onnx",
                "tokens.txt",
            ],
        };
        names.iter().map(|n| format!("{}/{n}", self.dir)).collect()
    }
}

/// The Parakeet models (evaluation conversions from manifest set `eval`, not shippable yet;
/// the English defaults are also in `ci` and `desktop`, v3 in `choice`).
pub const PARAKEET_MODELS: &[ParakeetModel] = &[
    ParakeetModel {
        name: "parakeet-ctc-110m",
        dir: "parakeet/tdt_ctc-110m",
        decoder: ParakeetDecoder::Ctc,
        external_weights: false,
        english_only: true,
        fetch_set: "eval",
    },
    ParakeetModel {
        name: "parakeet-tdt-110m",
        dir: "parakeet/tdt-110m",
        decoder: ParakeetDecoder::Tdt,
        external_weights: false,
        english_only: true,
        fetch_set: "ci",
    },
    ParakeetModel {
        name: "parakeet-tdt-0.6b-v2",
        dir: "parakeet/tdt-0.6b-v2",
        decoder: ParakeetDecoder::Tdt,
        external_weights: true,
        english_only: true,
        fetch_set: "ci",
    },
    ParakeetModel {
        name: "parakeet-tdt-0.6b-v3",
        dir: "parakeet/tdt-0.6b-v3",
        decoder: ParakeetDecoder::Tdt,
        external_weights: true,
        english_only: false,
        fetch_set: "choice",
    },
];

/// A transcription model by name: a whisper model (its ggml file) or a Parakeet model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AsrModel {
    /// A whisper model; the ggml file relative to the models directory.
    Whisper(String),
    /// A Parakeet model.
    Parakeet(&'static ParakeetModel),
}

impl AsrModel {
    /// Every file the model needs, relative to the models directory.
    #[must_use]
    pub fn files(&self) -> Vec<String> {
        match self {
            Self::Whisper(rel) => vec![rel.clone()],
            Self::Parakeet(p) => p.files(),
        }
    }
}

/// Every transcription model name, whisper first, in manifest order.
#[must_use]
pub fn asr_model_names() -> Vec<&'static str> {
    WHISPER_MODELS
        .iter()
        .map(|(n, _)| *n)
        .chain(PARAKEET_MODELS.iter().map(|p| p.name))
        .collect()
}

/// The transcription model `name` (matched exactly).
///
/// # Errors
/// [`RuntimeError::UnknownModel`] for any other name.
pub fn asr_model(name: &str) -> Result<AsrModel, RuntimeError> {
    if let Some(p) = PARAKEET_MODELS.iter().find(|p| p.name == name) {
        return Ok(AsrModel::Parakeet(p));
    }
    whisper_model(name).map(AsrModel::Whisper)
}

/// The `cargo xtask fetch-models` set that provides every file of the transcription model
/// `name` (PLAN D22: `ci` for the defaults ci loads, `desktop` for the other defaults,
/// `choice` for models offered but never tested, `eval` for evaluation-only ones), `None`
/// for an unknown name.
#[must_use]
pub fn model_fetch_set(name: &str) -> Option<&'static str> {
    if let Some(p) = PARAKEET_MODELS.iter().find(|p| p.name == name) {
        return Some(p.fetch_set);
    }
    WHISPER_MODELS
        .iter()
        .find(|(n, _)| *n == name)
        .map(|(_, set)| *set)
}

/// The `cargo xtask fetch-models` set that provides `rel`, `None` if no manifest entry
/// does (models/manifest.toml).
fn fetch_set(rel: &str) -> Option<&'static str> {
    if let Some(p) = PARAKEET_MODELS
        .iter()
        .find(|p| p.files().iter().any(|f| f == rel))
    {
        return Some(p.fetch_set);
    }
    if rel == earshot_onnx::ORT_CUDA_DYLIB {
        return Some("cuda");
    }
    if rel == earshot_onnx::ORT_CPU_DYLIB_ARM64 {
        return Some("arm64");
    }
    if qnn_files().iter().any(|f| f == rel) {
        return Some(earshot_onnx::qnn::QNN_HOST_SET);
    }
    if crate::summary_model::is_summary_file(rel) {
        return Some(crate::summary_model::SUMMARY_FETCH_SET);
    }
    if npu_conversion_published(rel) {
        return Some(crate::catalogue::NPU_SET);
    }
    if [SILERO, SEGMENTATION, CAMPPLUS, earshot_onnx::ORT_CPU_DYLIB].contains(&rel) {
        return Some("ci");
    }
    WHISPER_MODELS
        .iter()
        .find(|(n, _)| rel == format!("whisper/ggml-{n}.bin"))
        .map(|(_, set)| *set)
}

/// Whether `rel` is a file of an NPU conversion the manifest publishes (set
/// [`crate::catalogue::NPU_SET`], T8.4g; listed in the artefacts on every build).
fn npu_conversion_published(rel: &str) -> bool {
    crate::catalogue::Catalogue::embedded().is_ok_and(|c| {
        c.artefacts()
            .iter()
            .filter(|a| a.sets.iter().any(|s| s == crate::catalogue::NPU_SET))
            .filter_map(|a| a.extract.as_ref())
            .any(|x| {
                x.members
                    .iter()
                    .any(|m| format!("{}/{}", x.into, m.path) == rel)
            })
    })
}

/// The models directory; see the module docs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelDir {
    root: PathBuf,
}

impl ModelDir {
    /// Models under `root`.
    #[must_use]
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// `EARSHOT_MODELS` when set and not empty, else `data`'s models directory.
    #[must_use]
    pub fn from_env(data: &DataDir) -> Self {
        Self::resolve(std::env::var_os("EARSHOT_MODELS"), data)
    }

    /// [`ModelDir::from_env`] with the variable's value given.
    #[must_use]
    pub fn resolve(earshot_models: Option<OsString>, data: &DataDir) -> Self {
        match earshot_models.filter(|v| !v.is_empty()) {
            Some(dir) => Self::new(dir),
            None => Self::new(data.models_dir()),
        }
    }

    /// The directory.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The file at `rel`.
    ///
    /// # Errors
    /// [`RuntimeError::ModelMissing`] (with the fetch command) when it is not a file.
    pub fn require(&self, rel: &str) -> Result<PathBuf, RuntimeError> {
        let path = self.root.join(rel);
        if path.is_file() {
            Ok(path)
        } else {
            Err(RuntimeError::ModelMissing {
                path,
                fetch_set: fetch_set(rel),
            })
        }
    }
}

/// Where models run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Accelerator {
    /// The CPU (always available).
    Cpu,
    /// NVIDIA CUDA (`cuda` builds).
    Cuda,
    /// A Vulkan GPU (`vulkan` builds; whisper.cpp only: ONNX Runtime has no Vulkan provider
    /// here, so Parakeet, Silero and the speaker models refuse it).
    Vulkan,
    /// The Snapdragon NPU through Qualcomm's QNN plugin for ONNX Runtime (`qnn` builds;
    /// ONNX models only, so only Parakeet is offered it, [`model_order`]).
    Qnn,
}

impl Accelerator {
    /// The backend it runs.
    #[must_use]
    pub fn backend(self) -> BackendKind {
        match self {
            Self::Cpu => BackendKind::Cpu,
            Self::Cuda => BackendKind::Cuda,
            Self::Vulkan => BackendKind::Vulkan,
            Self::Qnn => BackendKind::Qnn,
        }
    }

    /// The accelerator of `backend`, if it is one.
    #[must_use]
    pub fn of(backend: BackendKind) -> Option<Self> {
        [Self::Cpu, Self::Cuda, Self::Vulkan, Self::Qnn]
            .into_iter()
            .find(|a| a.backend() == backend)
    }
}

/// Accelerators to try, best first, for this build on this machine: [`order_for`] with CUDA
/// in a `cuda` build or after [`crate::cuda::use_for_transcription`] chose it, and Vulkan in a
/// `vulkan` build when whisper.cpp sees a Vulkan device (a Vulkan build is meant for any machine,
/// and many have none).
#[must_use]
pub fn accelerator_order() -> Vec<Accelerator> {
    let vulkan = cfg!(feature = "vulkan")
        && earshot_asr_whisper::gpu_devices()
            .iter()
            .any(|d| d.backend == BackendKind::Vulkan);
    with_npu(
        order_for(onnx_cuda_usable(), vulkan),
        NPU_BUILD && !qnn_load_failed(),
    )
}

/// Whether this build offers the NPU: an ARM64 `qnn` build (T8.3, T8.4f).
const NPU_BUILD: bool = cfg!(all(feature = "qnn", target_arch = "aarch64"));

/// Set once the QNN library of this process failed to load ([`record_qnn_load_failure`]).
static QNN_LOAD_FAILED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Records that ONNX Runtime with the QNN plugin did not load (no NPU, a library failing its
/// SHA-256, a provider that refuses, missing files). The NPU leaves the order for the rest of the
/// process, as CUDA does after `cuda::record_load_failure`: the Models page, the check's
/// fingerprint and later meetings see what really runs, and only the caller that heard the failure
/// reports it (`host::init_onnx_for`).
pub(crate) fn record_qnn_load_failure() {
    QNN_LOAD_FAILED.store(true, std::sync::atomic::Ordering::SeqCst);
}

/// Whether the QNN library failed to load in this process ([`record_qnn_load_failure`]).
#[must_use]
pub fn qnn_load_failed() -> bool {
    QNN_LOAD_FAILED.load(std::sync::atomic::Ordering::SeqCst)
}

/// Whether this process may run ONNX Runtime's CUDA provider: a developer's `cuda` build, or the
/// choice of [`crate::cuda::use_for_transcription`].
fn onnx_cuda_usable() -> bool {
    cfg!(feature = "cuda") || crate::cuda::transcription_from_folder()
}

/// `order` with the NPU (QNN) inserted after CUDA and before Vulkan when `qnn` (an ARM64
/// `qnn` build; PLAN.md 3.4 and D20: NPU > Vulkan > CPU on the Surface). The x64 QNN package
/// is Qualcomm's HTP simulator, for tests, so x64 builds never pass `qnn`.
#[must_use]
pub fn with_npu(order: Vec<Accelerator>, qnn: bool) -> Vec<Accelerator> {
    if !qnn || order.contains(&Accelerator::Qnn) {
        return order;
    }
    let at = order
        .iter()
        .position(|a| *a != Accelerator::Cuda)
        .unwrap_or(order.len());
    let mut out = order;
    out.insert(at, Accelerator::Qnn);
    out
}

/// The accelerators of `order` that can run `model`: QNN only for Parakeet (ONNX Runtime);
/// whisper.cpp has no QNN backend, so it is never offered it (nothing to refuse or warn
/// about). After [`crate::cuda::use_for_transcription`] the same goes for CUDA, because such a
/// build's whisper.cpp has no CUDA backend ([`model_order_with`]).
#[must_use]
pub fn model_order(model: &AsrModel, order: &[Accelerator]) -> Vec<Accelerator> {
    let order = model_order_with(model, order, !crate::cuda::transcription_from_folder());
    // A setup read before the library failed to load must not try it again: the one warning was
    // the library's.
    let cuda_failed = crate::cuda::load_failed();
    let qnn_failed = qnn_load_failed();
    order
        .into_iter()
        .filter(|a| !(cuda_failed && *a == Accelerator::Cuda))
        .filter(|a| !(qnn_failed && *a == Accelerator::Qnn))
        .collect()
}

/// [`model_order`], with whether whisper.cpp may be offered CUDA given. Asked directly anyway,
/// a whisper.cpp built without CUDA refuses it (`WhisperConfig::for_backend`).
#[must_use]
pub fn model_order_with(
    model: &AsrModel,
    order: &[Accelerator],
    whisper_on_cuda: bool,
) -> Vec<Accelerator> {
    let parakeet = matches!(model, AsrModel::Parakeet(_));
    order
        .iter()
        .copied()
        .filter(|a| *a != Accelerator::Qnn || parakeet)
        .filter(|a| *a != Accelerator::Cuda || parakeet || whisper_on_cuda)
        .collect()
}

/// Whether model `name` can run on `accel` by [`model_order`]. A name that is no model is left to
/// whoever loads it to refuse.
#[must_use]
pub fn model_can_run_on(name: &str, accel: Accelerator) -> bool {
    asr_model(name).map_or(true, |m| !model_order(&m, &[accel]).is_empty())
}

/// The automatic order (PLAN.md 3.4, D20): CUDA, then Vulkan, then the CPU, each only when
/// present.
#[must_use]
pub fn order_for(cuda: bool, vulkan: bool) -> Vec<Accelerator> {
    let mut order = Vec::new();
    if cuda {
        order.push(Accelerator::Cuda);
    }
    if vulkan {
        order.push(Accelerator::Vulkan);
    }
    order.push(Accelerator::Cpu);
    order
}

/// `order` from the backend the model check recommended (T6.10d): a check that kept the CPU
/// over Vulkan (Vulkan did not beat it) must not be overruled by trying Vulkan first. A
/// backend not in `order` leaves it unchanged.
#[must_use]
pub fn order_from(order: &[Accelerator], recommended: BackendKind) -> Vec<Accelerator> {
    match order.iter().position(|a| a.backend() == recommended) {
        Some(i) => order[i..].to_vec(),
        None => order.to_vec(),
    }
}

/// The QNN plugin files this architecture loads, relative to the models directory
/// (`earshot_onnx::qnn::QNN_HOST`, set `qnn` on ARM64, `qnn-x64` on x64): the plugin, the HTP
/// backend and every companion library the package pins.
#[must_use]
pub fn qnn_files() -> Vec<String> {
    let package = earshot_onnx::qnn::QNN_HOST;
    [
        earshot_onnx::qnn::QNN_PROVIDER_LIB,
        earshot_onnx::qnn::QNN_HTP_LIB,
    ]
    .into_iter()
    .chain(package.libraries.iter().map(|(name, _)| *name))
    .map(|f| format!("{}/{f}", package.dir))
    .collect()
}

/// Whether this build takes the QNN plugin only from beside the exe: the shipped ARM64 app
/// (`qnn-installed`, through the desktop's `qnn` feature; T8.4e, T8.4f). The CLI and development
/// builds may use the models folder's extraction.
pub const QNN_INSTALL_ONLY: bool = cfg!(feature = "qnn-installed");

/// The folder the QNN plugin is registered from ([`qnn_package_dir_in`] for this exe and build).
#[must_use]
pub fn qnn_package_dir(models: &ModelDir) -> Option<PathBuf> {
    qnn_package_dir_in(
        models,
        earshot_onnx::qnn::exe_dir().as_deref(),
        QNN_INSTALL_ONLY,
    )
}

/// The folder the QNN plugin is registered from: `exe_dir` when it holds the plugin (a shipped
/// ARM64 install, T8.4e); otherwise the models directory's extraction, unless `install_only` (a
/// shipped build never reads the models folder for it: `None`).
#[must_use]
pub fn qnn_package_dir_in(
    models: &ModelDir,
    exe_dir: Option<&Path>,
    install_only: bool,
) -> Option<PathBuf> {
    let beside_exe = exe_dir.filter(|d| d.join(earshot_onnx::qnn::QNN_PROVIDER_LIB).is_file());
    match beside_exe {
        Some(dir) => Some(dir.to_path_buf()),
        None if install_only => None,
        None => Some(earshot_onnx::qnn::package_dir(
            models.root(),
            &earshot_onnx::qnn::QNN_HOST,
            None,
        )),
    }
}

/// This install holds no QNN libraries beside the exe ([`QNN_INSTALL_ONLY`]).
const QNN_NOT_INSTALLED: RuntimeError =
    RuntimeError::Unavailable("this installation of Earshot holds no NPU libraries beside it");

/// `order` for transcription model `name` given what `models` holds: without the NPU, and the
/// user's note why, when `name` is a Parakeet model whose NPU conversion
/// ([`ParakeetModel::qnn_files`]) is not all there (T8.4f; the conversions are a separate download,
/// T8.4g). Only presence is read here: the files are checked against their `conversion.json` when
/// the model loads ([`require_qnn_model`]), and a refusal there falls back to the CPU, reported. An
/// order without the NPU, or a model never offered it ([`model_order`]), is unchanged.
#[must_use]
pub fn npu_order(
    models: &ModelDir,
    name: &str,
    order: Vec<Accelerator>,
) -> (Vec<Accelerator>, Option<String>) {
    let Ok(AsrModel::Parakeet(model)) = asr_model(name) else {
        return (order, None);
    };
    if !order.contains(&Accelerator::Qnn) {
        return (order, None);
    }
    let files = model.qnn_files();
    if !files.is_empty() && files.iter().all(|f| models.root().join(f).is_file()) {
        return (order, None);
    }
    let note = format!(
        "{} has no NPU version installed, so it runs on the processor.",
        crate::catalogue::display_name(name)
    );
    (
        order
            .into_iter()
            .filter(|a| *a != Accelerator::Qnn)
            .collect(),
        Some(note),
    )
}

/// Silero VAD's ONNX Runtime threads (a small model run on 32 ms frames).
pub const VAD_THREADS: usize = 1;

/// ONNX Runtime CPU threads for each of `jobs` model sessions running side by side on a
/// machine with `cores` logical cores (T6.11): the T6.10b whisper rule, so a meeting app
/// leaves the call app room (at most half the cores in total, at most
/// [`MAX_LIVE_THREADS_PER_TRACK`] per job, at least one).
#[must_use]
pub fn onnx_cpu_threads(cores: usize, jobs: usize) -> usize {
    live_whisper_threads(BackendKind::Cpu, cores, jobs).get()
}

fn cores() -> usize {
    std::thread::available_parallelism().map_or(1, std::num::NonZeroUsize::get)
}

/// The ONNX Runtime library to load for transcription model `model` with whisper's
/// accelerator `order`: the CUDA library whenever CUDA is in the order, whatever the model
/// (replacing T6.11's Parakeet-only rule: ONNX Runtime is one library per
/// process, so an app session that ran a Whisper meeting on the CPU library could not later
/// run Parakeet on CUDA, nor the reverse; the CUDA library also serves the CPU sessions of
/// VAD and the speaker models, at the cost of its load time and memory and of needing cuDNN
/// even for Whisper meetings); the CPU library with the QNN plugin registered when `model`
/// is a Parakeet model and QNN is in the order (T8.3; the same DLL as the CPU library, so
/// no conflict); otherwise the CPU library. After the QNN library failed to load in this process
/// ([`qnn_load_failed`]) the NPU is not asked for again.
#[must_use]
pub fn onnx_library(model: &str, order: &[Accelerator]) -> Accelerator {
    let parakeet = matches!(asr_model(model), Ok(AsrModel::Parakeet(_)));
    if order.contains(&Accelerator::Cuda) {
        Accelerator::Cuda
    } else if parakeet && order.contains(&Accelerator::Qnn) && !qnn_load_failed() {
        Accelerator::Qnn
    } else {
        Accelerator::Cpu
    }
}

/// The ONNX Runtime library for trying every accelerator of `order` (the model check):
/// CUDA's when CUDA is in the order, else the QNN plugin's when QNN is (and has not failed to load
/// in this process), else the CPU's.
#[must_use]
pub fn onnx_library_for_all(order: &[Accelerator]) -> Accelerator {
    [Accelerator::Cuda, Accelerator::Qnn]
        .into_iter()
        .filter(|a| *a != Accelerator::Qnn || !qnn_load_failed())
        .find(|a| order.contains(a))
        .unwrap_or(Accelerator::Cpu)
}

/// The session settings for `accel` with `threads` intra-op threads; never a silent CPU
/// fallback.
///
/// # Errors
/// [`RuntimeError::Unavailable`] for CUDA in a build without `cuda`.
pub fn onnx_session_config(
    accel: Accelerator,
    threads: usize,
) -> Result<SessionConfig, RuntimeError> {
    Ok(SessionConfig {
        intra_threads: Some(threads.max(1)),
        ..session_config(accel)?
    })
}

/// ONNX Runtime models (Parakeet, Silero, pyannote, CAM++) refuse Vulkan: the official
/// ONNX Runtime builds of the manifest have no Vulkan provider.
const VULKAN_ONNX: RuntimeError = RuntimeError::Unavailable(
    "ONNX Runtime has no Vulkan provider: only whisper.cpp runs on Vulkan",
);

/// The session settings for `accel`; never a silent CPU fallback.
fn session_config(accel: Accelerator) -> Result<SessionConfig, RuntimeError> {
    let provider = match accel {
        Accelerator::Cpu => Provider::Cpu,
        Accelerator::Cuda if onnx_cuda_usable() => Provider::Cuda { device_id: 0 },
        Accelerator::Cuda => return Err(crate::cuda::off_reason()),
        Accelerator::Vulkan => return Err(VULKAN_ONNX),
        #[cfg(feature = "qnn")]
        Accelerator::Qnn => Provider::Qnn,
        #[cfg(not(feature = "qnn"))]
        Accelerator::Qnn => {
            return Err(RuntimeError::Unavailable(
                "QNN needs a build with the `qnn` feature",
            ));
        }
    };
    Ok(SessionConfig {
        provider,
        intra_threads: None,
        background: None,
    })
}

/// Loads ONNX Runtime for `accel` from the models directory (once per process). For QNN
/// that is the CPU library with Qualcomm's QNN plugin registered into it from
/// [`qnn_package_dir`] (`earshot_onnx::qnn::register_qnn_in`, every library checksum-verified).
///
/// # Errors
/// [`RuntimeError::Unavailable`] for CUDA or QNN in a build without the feature, and for QNN in a
/// shipped build ([`QNN_INSTALL_ONLY`]) whose install holds no QNN libraries;
/// [`RuntimeError::ModelMissing`]; [`RuntimeError::Onnx`] if a library fails its
/// checksum, another ONNX Runtime library is already loaded, or the QNN plugin offers no
/// device.
pub fn init_onnx(models: &ModelDir, accel: Accelerator) -> Result<PathBuf, RuntimeError> {
    // The one door to ONNX Runtime in a host: the choice of library may still be being made.
    crate::cuda::wait_for_onnx();
    // The shipped build's CUDA library, from the folder of the optional download.
    if accel == Accelerator::Cuda
        && !cfg!(feature = "cuda")
        && crate::cuda::transcription_from_folder()
    {
        let library = crate::cuda::folder(models).join(crate::cuda::ONNX_RUNTIME_DLL);
        let loaded = if library.is_file() {
            // cuDNN before ONNX Runtime, because the provider needs it there. A failure of either
            // leaves the CPU library free to load (`earshot_onnx::init` keeps only a success).
            crate::cuda::preload_for_provider(models)
                .and_then(|()| {
                    earshot_onnx::init(&library, earshot_onnx::ORT_CUDA_DYLIB_SHA256).map(|_| ())
                })
                .map_err(RuntimeError::Onnx)
        } else {
            Err(crate::cuda::FILES_MISSING)
        };
        if loaded.is_err() {
            crate::cuda::record_load_failure();
        }
        return loaded.map(|()| library);
    }
    let (rel, sha) = match accel {
        Accelerator::Cpu => (
            earshot_onnx::ORT_CPU_DYLIB,
            earshot_onnx::ORT_CPU_DYLIB_SHA256,
        ),
        Accelerator::Qnn if cfg!(feature = "qnn") => (
            earshot_onnx::ORT_CPU_DYLIB,
            earshot_onnx::ORT_CPU_DYLIB_SHA256,
        ),
        Accelerator::Qnn => {
            return Err(RuntimeError::Unavailable(
                "QNN needs a build with the `qnn` feature",
            ));
        }
        Accelerator::Cuda if cfg!(feature = "cuda") => (
            earshot_onnx::ORT_CUDA_DYLIB,
            earshot_onnx::ORT_CUDA_DYLIB_SHA256,
        ),
        Accelerator::Cuda => return Err(crate::cuda::off_reason()),
        Accelerator::Vulkan => return Err(VULKAN_ONNX),
    };
    let library = models.require(rel)?;
    // A shipped install holds the plugin beside the exe; only the models folder's copy needs the
    // fetch instruction of a missing file. A shipped build never falls back to that copy.
    let qnn_dir = match accel {
        Accelerator::Qnn => Some(qnn_package_dir(models).ok_or(QNN_NOT_INSTALLED)?),
        _ => None,
    };
    if qnn_dir
        .as_deref()
        .is_some_and(|d| d == models.root().join(earshot_onnx::qnn::QNN_HOST.dir))
    {
        for f in qnn_files() {
            models.require(&f)?;
        }
    }
    earshot_onnx::init(&library, sha).map_err(RuntimeError::Onnx)?;
    if let Some(dir) = qnn_dir {
        earshot_onnx::qnn::register_qnn_in(&dir, &earshot_onnx::qnn::QNN_HOST)
            .map_err(RuntimeError::Onnx)?;
    }
    Ok(library)
}

/// Silero VAD on `accel` (after [`init_onnx`]) with [`VAD_THREADS`].
///
/// # Errors
/// [`RuntimeError::ModelMissing`]; [`RuntimeError::Onnx`].
pub fn load_silero(models: &ModelDir, accel: Accelerator) -> Result<SileroVad, RuntimeError> {
    SileroVad::load(
        &models.require(SILERO)?,
        &onnx_session_config(accel, VAD_THREADS)?,
    )
    .map_err(RuntimeError::Onnx)
}

/// pyannote segmentation and CAM++ on `accel` (after [`init_onnx`]), with
/// [`speaker_threads`].
///
/// # Errors
/// [`RuntimeError::ModelMissing`]; [`RuntimeError::Onnx`].
pub fn load_diarisation(
    models: &ModelDir,
    accel: Accelerator,
) -> Result<(PyannoteSegmenter, CamppEmbedder), RuntimeError> {
    load_diarisation_with(models, accel, speaker_threads(cores()), false)
}

/// pyannote segmentation and CAM++ on `accel` with `threads` ONNX Runtime threads; callers
/// record the same `threads` they pass here (T6.13d2: reuse depends on it). `background`
/// sessions run their pool threads at below-normal priority without spin-waiting
/// (T6.13d3; outputs equal a normal session's, `earshot-onnx`
/// `diar_background_sessions_equal_normal_sessions_bit_for_bit`).
fn load_diarisation_with(
    models: &ModelDir,
    accel: Accelerator,
    threads: usize,
    background: bool,
) -> Result<(PyannoteSegmenter, CamppEmbedder), RuntimeError> {
    let config = SessionConfig {
        background: background.then_some(earshot_onnx::ThreadStart(
            crate::priority::lower_pool_thread,
        )),
        ..onnx_session_config(accel, threads)?
    };
    let segmenter =
        PyannoteSegmenter::load(&models.require(SEGMENTATION)?, &config, SEGMENTER_BATCH)
            .map_err(RuntimeError::Onnx)?;
    let embedder =
        CamppEmbedder::load(&models.require(CAMPPLUS)?, &config).map_err(RuntimeError::Onnx)?;
    Ok((segmenter, embedder))
}

/// ONNX Runtime threads of every speaker-model session (pyannote, CAM++) on a machine with
/// `cores` logical cores, live and in the final pass (T6.13d2): the [`onnx_cpu_threads`]
/// budget of two side-by-side copies (a live session has two tracks; the final pass may
/// diarise them side by side, [`parallel_diarisation`]). One count for both, because CAM++
/// embeddings differ in the last bits between thread counts (measured,
/// `earshot-onnx` `diar_cpu_outputs_are_identical_at_the_same_thread_count`) and the final
/// pass reuses live windows only with the same count. Equal to the one-copy budget on 16
/// cores or more.
#[must_use]
pub fn speaker_threads(cores: usize) -> usize {
    onnx_cpu_threads(cores, 2)
}

/// Whether the final pass diarises two tracks side by side on a machine with `cores`
/// logical cores (T6.12): only when each of the two copies still gets at least two ONNX
/// Runtime threads under [`onnx_cpu_threads`] (8 cores or more).
#[must_use]
pub fn parallel_diarisation(cores: usize) -> bool {
    onnx_cpu_threads(cores, 2) >= 2
}

/// Adds speaker work during the meeting to `inner` (T6.13d2): every track that gets live
/// ASR also gets its own pyannote and CAM++ on `accel` (after [`init_onnx`]) with
/// [`speaker_threads`] and `diarization`'s window settings, which must be the final pass's
/// ([`PostConfig::diarization`]) for its windows to be reused. The sessions are background
/// ones (T6.13d3): the session's one speaker thread and their pool threads run at
/// below-normal priority.
///
/// CPU budget of a two-track live session (T6.10b, T6.11): the speaker thread runs one
/// batch at a time for both tracks, so speaker work keeps at most [`speaker_threads`]
/// cores busy, all at below-normal priority, next to the normal-priority live
/// transcription ([`live_whisper_threads`] or [`onnx_cpu_threads`] per track) and one
/// Silero thread per track ([`VAD_THREADS`]):
/// - 32 logical cores (desktop): transcription 2 x 2 on a GPU (2 x 4 on the CPU) plus 2
///   Silero, at most 10 normal-priority threads; speaker work 4 below normal: at most 14
///   busy, within half the cores (16).
/// - 12 logical cores (Surface): transcription 2 x 3 on the CPU (half the cores) plus 2
///   Silero, 8 normal-priority threads; speaker work 3 below normal: at most 11 busy. The
///   speaker threads go beyond half the cores but take only time that normal-priority
///   threads (transcription and the call app) leave idle.
pub fn with_live_speakers(
    mut inner: LiveAsrFactory,
    models: ModelDir,
    accel: Accelerator,
    diarization: DiarizationConfig,
) -> LiveAsrFactory {
    Box::new(move |track: TrackId, entity: EntityId| {
        let Some(mut asr) = inner(track, entity)? else {
            return Ok(None);
        };
        asr.speakers = Some(live_speakers(models.clone(), accel, diarization.clone()));
        Ok(Some(asr))
    })
}

/// The live speaker work of a track, as [`with_live_speakers`] gives it: the models load only when
/// speaker work runs during the meeting (T6.13d4).
#[must_use]
pub fn live_speakers(
    models: ModelDir,
    accel: Accelerator,
    diarization: DiarizationConfig,
) -> LiveSpeakers {
    // One count, used for the sessions and recorded with the windows.
    let threads = speaker_threads(cores());
    let load = move || {
        let (segmenter, embedder) = load_diarisation_with(&models, accel, threads, true)?;
        Diarizer::new(
            Box::new(segmenter) as Box<dyn SpeakerSegmenter + Send>,
            Box::new(embedder) as Box<dyn SpeakerEmbedder + Send>,
            diarization,
        )
        .map_err(RuntimeError::Diar)
    };
    LiveSpeakers {
        models: Box::new(load),
        threads: Some(threads),
        progress: None,
        background: true,
    }
}

/// Loads a whisper model and proves which backend it runs on.
///
/// Implementations must fail unless the requested backend, the transcriber's
/// `backend_info()` and whisper.cpp's own log agree (AGENTS.md); the production
/// implementation is `earshot_asr_whisper::evidence::load_verified`.
pub trait WhisperLoader {
    /// The transcriber produced.
    type Transcriber: Transcriber + Send + 'static;

    /// Loads `model` on `expected`, verified.
    ///
    /// # Errors
    /// Load errors, or [`AsrError::Backend`] when the backend cannot be verified.
    fn load_verified(
        &self,
        model: &Path,
        expected: BackendKind,
    ) -> Result<Self::Transcriber, AsrError>;
}

/// A whisper model loaded on a verified backend.
#[derive(Debug)]
pub struct WhisperChoice<T> {
    /// The transcriber.
    pub transcriber: T,
    /// The backend it runs on (verified by the loader).
    pub backend: BackendKind,
    /// Better backends that were tried and why they were refused.
    pub rejected: Vec<(BackendKind, AsrError)>,
}

/// Loads `model` on the first accelerator of `order` the loader can verify. QNN is not a
/// whisper backend and is skipped without a refusal ([`model_order`] never offers it).
///
/// # Errors
/// [`RuntimeError::NoBackend`] with every refusal when none verifies.
pub fn select_whisper<L: WhisperLoader + ?Sized>(
    loader: &L,
    model: &Path,
    order: &[Accelerator],
) -> Result<WhisperChoice<L::Transcriber>, RuntimeError> {
    let mut rejected = Vec::new();
    for accel in order.iter().filter(|a| **a != Accelerator::Qnn) {
        let backend = accel.backend();
        match loader.load_verified(model, backend) {
            Ok(transcriber) => {
                return Ok(WhisperChoice {
                    transcriber,
                    backend,
                    rejected,
                });
            }
            Err(e) => rejected.push((backend, e)),
        }
    }
    Err(RuntimeError::NoBackend(rejected))
}

/// A loaded Parakeet model.
#[derive(Debug)]
pub enum ParakeetTranscriber {
    /// A CTC model.
    Ctc(Box<ParakeetCtc>),
    /// A TDT model.
    Tdt(Box<ParakeetTdt>),
}

impl ParakeetTranscriber {
    /// Where ONNX Runtime placed each session's nodes.
    #[must_use]
    pub fn backends(&self) -> Vec<&SessionBackend> {
        match self {
            Self::Ctc(t) => t.backends(),
            Self::Tdt(t) => t.backends(),
        }
    }
}

impl Transcriber for ParakeetTranscriber {
    fn transcribe(
        &mut self,
        audio: &MonoBuffer,
        options: &TranscribeOptions,
        cancel: &CancelToken,
    ) -> Result<Transcription, AsrError> {
        match self {
            Self::Ctc(t) => t.transcribe(audio, options, cancel),
            Self::Tdt(t) => t.transcribe(audio, options, cancel),
        }
    }

    fn backend_info(&self) -> BackendInfo {
        match self {
            Self::Ctc(t) => t.backend_info(),
            Self::Tdt(t) => t.backend_info(),
        }
    }

    fn may_skip_speech(&self) -> bool {
        match self {
            Self::Ctc(t) => t.may_skip_speech(),
            Self::Tdt(t) => t.may_skip_speech(),
        }
    }
}

/// Whether ONNX Runtime ran every session on `expected` (a session counts for a GPU when
/// any of its nodes is placed there, as [`SessionBackend::kind`]; shape nodes stay on the
/// CPU); `Err` names the first session that did not.
///
/// For QNN (T8.3) there is one rule, the documented NPU partition of a Parakeet model
/// ([`check_qnn_parakeet`]): an NPU that takes a fraction of the graph is never reported as
/// the NPU.
///
/// # Errors
/// [`AsrError::Backend`] naming the placement.
pub fn check_placement(
    backends: &[&SessionBackend],
    expected: BackendKind,
) -> Result<(), AsrError> {
    if expected == BackendKind::Qnn {
        // One rule for the NPU: only Parakeet runs there, with its documented partition.
        return check_qnn_parakeet(backends);
    }
    if backends.is_empty() {
        return Err(AsrError::Backend("no ONNX session to verify".into()));
    }
    if let Some(b) = backends.iter().find(|b| b.kind() != Some(expected)) {
        return Err(AsrError::Backend(format!(
            "ONNX Runtime did not run the model on {expected:?} ({b})"
        )));
    }
    Ok(())
}

/// The directory of `model`'s NPU conversion ([`ParakeetModel::qnn_dir`]) when all its files
/// are there.
///
/// # Errors
/// [`AsrError::Backend`] naming the `cargo xtask qnn-convert` command that builds it, or
/// saying that the model has no NPU conversion (CTC).
pub fn require_qnn_model(models: &ModelDir, model: &ParakeetModel) -> Result<PathBuf, AsrError> {
    let Some(dir) = model.qnn_dir() else {
        return Err(AsrError::Backend(format!(
            "{} has no NPU conversion (only the TDT models are converted)",
            model.name
        )));
    };
    for rel in model.qnn_files() {
        let path = models.root().join(&rel);
        if !path.is_file() {
            return Err(AsrError::Backend(format!(
                "{} not found: the NPU conversion of {} is downloaded as its NPU version on the \
                 app's Transcription page (`cargo xtask fetch-models --set {}`) or built with \
                 `cargo xtask qnn-convert --model {}`",
                path.display(),
                model.name,
                crate::catalogue::NPU_SET,
                model.name
            )));
        }
    }
    let dir = models.root().join(dir);
    verify_qnn_conversion(&dir)?;
    Ok(dir)
}

/// Lower-case hex SHA-256 of a file.
fn file_sha256(path: &Path) -> std::io::Result<String> {
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

/// The converter versions (`task` in `conversion.json`) whose NPU conversions the runtime
/// loads. T8.3 conversions (no `recipe`) calibrated on one piece and cut every piece after
/// 16 s (T8.3c, docs/qnn.md), so they are refused.
pub const QNN_CONVERSION_TASKS: [&str; 1] = ["T8.3c"];

/// Checks every model file of the NPU conversion in `dir` against the size and SHA-256
/// `cargo xtask qnn-convert` recorded in its `conversion.json`, so a damaged or swapped file is
/// refused before it reaches the NPU, and refuses a conversion made by a converter version
/// not in [`QNN_CONVERSION_TASKS`] or without a recipe.
///
/// # Errors
/// [`AsrError::Backend`] for a missing or unreadable record, an old converter version or no
/// recipe (with the rebuild command), a file it does not list, or a size or hash that differs.
pub fn verify_qnn_conversion(dir: &Path) -> Result<(), AsrError> {
    let fail = |m: String| AsrError::Backend(format!("{}: {m}", dir.display()));
    let record = std::fs::read_to_string(dir.join("conversion.json"))
        .map_err(|e| fail(format!("conversion.json: {e}")))?;
    let record: serde_json::Value =
        serde_json::from_str(&record).map_err(|e| fail(format!("conversion.json: {e}")))?;
    let task = record["task"].as_str().unwrap_or("none");
    let recipe = record["recipe"].as_str().filter(|r| !r.is_empty());
    if !QNN_CONVERSION_TASKS.contains(&task) || recipe.is_none() {
        let model = dir.file_name().map_or_else(
            || "<model>".to_owned(),
            |n| n.to_string_lossy().into_owned(),
        );
        return Err(fail(format!(
            "conversion.json is from converter {task} with recipe {}, which this runtime does \
             not load (T8.3 conversions lose everything after 16 s of a piece); rebuild it \
             with `cargo xtask qnn-convert --model {model}`",
            recipe.unwrap_or("none")
        )));
    }
    let outputs = record["outputs"]
        .as_object()
        .ok_or_else(|| fail("conversion.json lists no outputs".into()))?;
    for f in [
        "encoder.onnx",
        "encoder.weights",
        "decoder.onnx",
        "joiner.onnx",
        "tokens.txt",
    ] {
        let pin = outputs
            .get(f)
            .ok_or_else(|| fail(format!("conversion.json does not list {f}")))?;
        let (Some(sha), Some(size)) = (pin["sha256"].as_str(), pin["size"].as_u64()) else {
            return Err(fail(format!("conversion.json has no hash or size for {f}")));
        };
        let path = dir.join(f);
        let actual_size = std::fs::metadata(&path)
            .map_err(|e| fail(format!("{f}: {e}")))?
            .len();
        if actual_size != size {
            return Err(fail(format!(
                "{f} is {actual_size} bytes, conversion.json says {size}; rebuild it with \
                 `cargo xtask qnn-convert`"
            )));
        }
        let actual = file_sha256(&path).map_err(|e| fail(format!("{f}: {e}")))?;
        if !actual.eq_ignore_ascii_case(sha) {
            return Err(fail(format!(
                "{f} has SHA-256 {actual}, conversion.json says {sha}; refusing to load it"
            )));
        }
    }
    Ok(())
}

/// Loads the Parakeet model `model` on `accel` (ONNX Runtime must be initialised with a
/// library that has the provider, [`init_onnx`]) with `threads` intra-op threads (`None`:
/// ONNX Runtime's default), and proves the backend from the node placement.
///
/// On QNN (T8.3) the model's NPU conversion is loaded ([`require_qnn_model`]) with its
/// documented partition ([`check_qnn_parakeet`]): the static QDQ encoder runs on the QNN EP
/// with only its float input and output conversions on the CPU (any other node left on the
/// CPU is a refusal), and the prediction network and joint run on the CPU with
/// `threads` (one LSTM step and one joint per decoded token: an NPU round trip each would
/// cost more than it saves).
///
/// # Errors
/// [`AsrError::Backend`] when a file is missing, the model does not load, the provider
/// cannot run it, or the placement does not show `accel` ([`check_placement`]).
pub fn load_parakeet(
    models: &ModelDir,
    model: &ParakeetModel,
    accel: Accelerator,
    threads: Option<usize>,
) -> Result<ParakeetTranscriber, AsrError> {
    load_parakeet_with(models, model, accel, threads, false)
}

/// A machine with less memory than this has no previews by default on the CPU: a meeting with them
/// reaches 5.8 GiB there (measured 2026-11-07, docs/live-previews.md), with the call app beside it.
/// A judgement: a 16 GB machine has room, an 8 GB one little. The user can turn
/// them on anyway (`preview_settings`).
pub const PREVIEW_MIN_MEMORY: u64 = 12 * 1024 * 1024 * 1024;

/// The same on CUDA, where sessions do not share their weights and a meeting reaches 15.4 GiB.
pub const PREVIEW_MIN_MEMORY_CUDA: u64 = 20 * 1024 * 1024 * 1024;

/// A graphics card with less dedicated memory than this runs no previews on CUDA: a third session
/// took a meeting's use of the card from 7.8 to 11.5 GiB (measured, docs/live-previews.md). A
/// judgement: the 11.5 and room for the rest of the desktop.
pub const PREVIEW_MIN_CARD_MEMORY: u64 = 14 * 1024 * 1024 * 1024;

/// Why this computer runs no previews for a track on `backend`, or `None` when it does. The one
/// rule (`live::preview` docs): the user's "off"; the NPU is off until the Surface numbers exist;
/// whisper, on any backend, holds decoder state and sampled retries a preview must not touch and
/// would need a second model that nothing here has measured; a card without room for a third
/// session. The user's "on" or "everyone" lifts only the system memory limit.
#[must_use]
pub fn previews_refused(
    backend: BackendKind,
    hardware: &earshot_hardware::Hardware,
    choice: crate::preview_settings::PreviewChoice,
) -> Option<String> {
    use crate::preview_settings::PreviewChoice;
    if choice == PreviewChoice::Off {
        return Some("turned off in Settings".to_owned());
    }
    let memory_limit = match backend {
        BackendKind::Cpu => PREVIEW_MIN_MEMORY,
        BackendKind::Cuda => {
            let card = crate::model_settings::graphics_memory(hardware);
            if card.is_none_or(|m| m < PREVIEW_MIN_CARD_MEMORY) {
                return Some("the graphics card is too small for a third session".to_owned());
            }
            PREVIEW_MIN_MEMORY_CUDA
        }
        BackendKind::Qnn => return Some("not measured on the NPU yet".to_owned()),
        BackendKind::Vulkan | BackendKind::Fake => {
            return Some("previews run on Parakeet only".to_owned());
        }
    };
    if choice == PreviewChoice::Automatic && hardware.total_memory.is_none_or(|m| m < memory_limit)
    {
        return Some("not enough memory for a second session of the model".to_owned());
    }
    None
}

/// A machine with less memory than this runs no second preview session (the user's own track in a
/// call, T9.14) on the CPU: the same as one session's limit, because the second shares the model's
/// weights and took a meeting's peak from 5.81 to 6.08 GiB (measured 2026-11-08, docs/live-previews.md).
/// It cost about half a core more on a 32-thread host; the slow-preview pause covers smaller ones.
pub const PREVIEW_BOTH_MIN_MEMORY: u64 = 12 * 1024 * 1024 * 1024;

/// Where this computer runs the second preview session, for the user's own track in a call (T9.14),
/// when the track runs on `backend`; `Err` says why it runs none. The one rule: only when the user
/// chose everyone, only where the first session runs ([`previews_refused`]), and only with
/// [`PREVIEW_BOTH_MIN_MEMORY`] known to be there. It always runs on the CPU, with the CPU's shared
/// weights: beside the CPU's own sessions, and beside CUDA too (T12.3a), so that the card holds no
/// more sessions than with "Other people only".
///
/// # Errors
/// The reason no second session runs.
pub fn second_preview_backend(
    backend: BackendKind,
    hardware: &earshot_hardware::Hardware,
    choice: crate::preview_settings::PreviewChoice,
) -> Result<BackendKind, String> {
    use crate::preview_settings::PreviewChoice;
    if choice != PreviewChoice::Everyone {
        return Err("only for the other people".to_owned());
    }
    if let Some(why) = previews_refused(backend, hardware, choice) {
        return Err(why);
    }
    if !matches!(backend, BackendKind::Cpu | BackendKind::Cuda) {
        return Err("previews run on Parakeet on the CPU or CUDA only".to_owned());
    }
    if hardware
        .total_memory
        .is_none_or(|m| m < PREVIEW_BOTH_MIN_MEMORY)
    {
        return Err("not enough memory for a third session of the model".to_owned());
    }
    Ok(BackendKind::Cpu)
}

/// Why this computer runs no second preview session (T9.14), or `None` when it does: the reason
/// [`second_preview_backend`] gives.
#[must_use]
pub fn previews_refused_second(
    backend: BackendKind,
    hardware: &earshot_hardware::Hardware,
    choice: crate::preview_settings::PreviewChoice,
) -> Option<String> {
    second_preview_backend(backend, hardware, choice).err()
}

/// The previews' own session of the model of a track that runs on `backend` (T9.1m): background
/// threads, the thread budget of a live track, and nothing shared with the track. An `Err` says why
/// this meeting has no previews ([`crate::live::PreviewLoader`]).
///
/// # Errors
/// A whisper model (it holds decoder state and sampled retries, and is not measured here); the
/// backend or machine refused by [`previews_refused`]; a failed load.
pub fn load_preview(
    models: &ModelDir,
    backend: &BackendInfo,
    choice: crate::preview_settings::PreviewChoice,
) -> Result<Box<dyn Transcriber + Send>, String> {
    if let Some(why) = previews_refused(backend.kind, &crate::hardware(), choice) {
        return Err(why);
    }
    load_preview_session(models, backend)
}

/// [`load_preview`] for the second session, the user's own track in a call (T9.14), loaded where
/// [`second_preview_backend`] says (the CPU, also beside CUDA: T12.3a) or refused by it. It shares
/// nothing with the first.
///
/// # Errors
/// As [`load_preview`], with the second session's rule.
pub fn load_preview_second(
    models: &ModelDir,
    backend: &BackendInfo,
    choice: crate::preview_settings::PreviewChoice,
) -> Result<Box<dyn Transcriber + Send>, String> {
    load_preview_second_with(backend, &crate::hardware(), choice, |on| {
        load_preview_session(models, on)
    })
}

/// [`load_preview_second`] on `hardware`, loading through `load` with the track's model on the
/// backend the rule chose.
pub(crate) fn load_preview_second_with(
    backend: &BackendInfo,
    hardware: &earshot_hardware::Hardware,
    choice: crate::preview_settings::PreviewChoice,
    load: impl FnOnce(&BackendInfo) -> Result<Box<dyn Transcriber + Send>, String>,
) -> Result<Box<dyn Transcriber + Send>, String> {
    let kind = second_preview_backend(backend.kind, hardware, choice)?;
    load(&BackendInfo {
        kind,
        ..backend.clone()
    })
}

/// A preview session loaded by [`load_preview_unrefused`], with the evidence of where it runs.
pub struct EvaluatedPreview {
    /// The session, as [`load_preview`] would have returned it.
    pub transcriber: Box<dyn Transcriber + Send>,
    /// Each ONNX Runtime session's requested provider and node placement, from ONNX Runtime's own
    /// placement log (the load has already passed [`check_placement`], or [`check_qnn_parakeet`]
    /// on QNN), never from the request.
    pub placement: Vec<String>,
}

/// Evaluation only (T12.4, `live_lag --force-previews`): a preview session loaded exactly as
/// [`load_preview`] and [`load_preview_second`] load one, without asking [`previews_refused`] or
/// [`previews_refused_second`] first. It exists so the NPU can be measured while the app still
/// refuses previews there; the app, the CLI and [`crate::host`] never call it (pinned by
/// `backends_preview_unrefused_load_is_used_by_no_shipped_code`). Every other check stays: a
/// whisper model, a missing file, a failed placement check or a preview thread that cannot lower
/// its priority is still an `Err`.
///
/// # Errors
/// As [`load_preview`], without its refusal.
pub fn load_preview_unrefused(
    models: &ModelDir,
    backend: &BackendInfo,
) -> Result<EvaluatedPreview, String> {
    let session = load_preview_parakeet(models, backend)?;
    let placement = session.backends().iter().map(ToString::to_string).collect();
    Ok(EvaluatedPreview {
        transcriber: Box::new(session),
        placement,
    })
}

fn load_preview_session(
    models: &ModelDir,
    backend: &BackendInfo,
) -> Result<Box<dyn Transcriber + Send>, String> {
    Ok(Box::new(load_preview_parakeet(models, backend)?))
}

fn load_preview_parakeet(
    models: &ModelDir,
    backend: &BackendInfo,
) -> Result<ParakeetTranscriber, String> {
    let Ok(AsrModel::Parakeet(model)) = asr_model(&backend.model) else {
        return Err("previews run on Parakeet only".to_owned());
    };
    let accel = Accelerator::of(backend.kind).ok_or("no accelerator for the backend")?;
    let threads = onnx_cpu_threads(cores(), 2);
    let session =
        load_parakeet_with(models, model, accel, Some(threads), true).map_err(|e| e.to_string())?;
    if crate::priority::preview_pool_failed() {
        return Err("a preview thread could not lower its priority".to_owned());
    }
    Ok(session)
}

/// [`load_parakeet`], on background threads when `background`.
fn load_parakeet_with(
    models: &ModelDir,
    model: &ParakeetModel,
    accel: Accelerator,
    threads: Option<usize>,
    background: bool,
) -> Result<ParakeetTranscriber, AsrError> {
    let config = SessionConfig {
        intra_threads: threads,
        background: background.then_some(earshot_onnx::ThreadStart(
            crate::priority::lower_preview_pool_thread,
        )),
        ..session_config(accel).map_err(|e| AsrError::Backend(e.to_string()))?
    };
    if accel == Accelerator::Qnn {
        return load_parakeet_qnn(models, model, &config, threads);
    }
    for rel in model.files() {
        models
            .require(&rel)
            .map_err(|e| AsrError::Backend(e.to_string()))?;
    }
    let dir = models.root().join(model.dir);
    let onnx = |e: earshot_onnx::OnnxError| AsrError::Backend(format!("{}: {e}", model.name));
    let t = match model.decoder {
        ParakeetDecoder::Ctc => {
            ParakeetTranscriber::Ctc(Box::new(ParakeetCtc::load(&dir, &config).map_err(onnx)?))
        }
        ParakeetDecoder::Tdt => ParakeetTranscriber::Tdt(Box::new(
            ParakeetTdt::load(&dir, &config, model.name, model.english_only).map_err(onnx)?,
        )),
    };
    check_placement(&t.backends(), accel.backend())?;
    Ok(t)
}

/// [`load_parakeet`] on QNN: the NPU conversion with its documented partition.
fn load_parakeet_qnn(
    models: &ModelDir,
    model: &ParakeetModel,
    encoder: &SessionConfig,
    threads: Option<usize>,
) -> Result<ParakeetTranscriber, AsrError> {
    let dir = require_qnn_model(models, model)?;
    let steps = SessionConfig {
        intra_threads: threads,
        ..SessionConfig::default()
    };
    let onnx = |e: earshot_onnx::OnnxError| AsrError::Backend(format!("{}: {e}", model.name));
    let t = ParakeetTdt::load_split(&dir, encoder, &steps, model.name, model.english_only)
        .map_err(onnx)?;
    if !t.is_static() {
        return Err(AsrError::Backend(format!(
            "{}: the encoder in {} has a dynamic input shape; the HTP backend needs the static \
             conversion (`cargo xtask qnn-convert --model {}`)",
            model.name,
            dir.display(),
            model.name
        )));
    }
    let sessions = t.backends();
    check_qnn_parakeet(&sessions)?;
    Ok(ParakeetTranscriber::Tdt(Box::new(t)))
}

/// The NPU partition of a Parakeet TDT model (encoder, prediction network, joint): the
/// encoder on the QNN EP with only its documented float boundary on the CPU
/// (`earshot_onnx::parakeet::QNN_ENCODER_CPU_OPS`), the two step sessions wholly on the CPU
/// ([`earshot_onnx::parakeet::check_qnn_partition`], the rule `qnn_etw_probe` checks too).
///
/// # Errors
/// [`AsrError::Backend`] naming the session and placement that differ.
pub fn check_qnn_parakeet(sessions: &[&SessionBackend]) -> Result<(), AsrError> {
    earshot_onnx::parakeet::check_qnn_partition(sessions).map_err(AsrError::Backend)
}

/// A whisper or Parakeet transcriber ([`select_asr`]).
#[derive(Debug)]
pub enum AnyTranscriber<W> {
    /// A whisper model from a [`WhisperLoader`].
    Whisper(W),
    /// A Parakeet model.
    Parakeet(Box<ParakeetTranscriber>),
}

impl<W: Transcriber> Transcriber for AnyTranscriber<W> {
    fn transcribe(
        &mut self,
        audio: &MonoBuffer,
        options: &TranscribeOptions,
        cancel: &CancelToken,
    ) -> Result<Transcription, AsrError> {
        match self {
            Self::Whisper(t) => t.transcribe(audio, options, cancel),
            Self::Parakeet(t) => t.transcribe(audio, options, cancel),
        }
    }

    fn backend_info(&self) -> BackendInfo {
        match self {
            Self::Whisper(t) => t.backend_info(),
            Self::Parakeet(t) => t.backend_info(),
        }
    }

    fn may_skip_speech(&self) -> bool {
        match self {
            Self::Whisper(t) => t.may_skip_speech(),
            Self::Parakeet(t) => t.may_skip_speech(),
        }
    }
}

/// Loads the transcription model `name` ([`asr_model`]) on the first accelerator of
/// `order` that verifies: whisper through `loader` ([`select_whisper`]), Parakeet through
/// [`load_parakeet`] with `parakeet_threads(backend)` intra-op threads.
///
/// # Errors
/// [`RuntimeError::UnknownModel`]; [`RuntimeError::ModelMissing`] for a missing file;
/// [`RuntimeError::NoBackend`] with every refusal when no accelerator verifies.
pub fn select_asr<L: WhisperLoader + ?Sized>(
    loader: &L,
    models: &ModelDir,
    name: &str,
    order: &[Accelerator],
    parakeet_threads: &dyn Fn(BackendKind) -> Option<usize>,
) -> Result<WhisperChoice<AnyTranscriber<L::Transcriber>>, RuntimeError> {
    let model = asr_model(name)?;
    for rel in model.files() {
        models.require(&rel)?;
    }
    let order = model_order(&model, order);
    let order = order.as_slice();
    match model {
        AsrModel::Whisper(rel) => {
            let choice = select_whisper(loader, &models.require(&rel)?, order)?;
            Ok(WhisperChoice {
                transcriber: AnyTranscriber::Whisper(choice.transcriber),
                backend: choice.backend,
                rejected: choice.rejected,
            })
        }
        AsrModel::Parakeet(p) => {
            let mut rejected = Vec::new();
            for &accel in order {
                let backend = accel.backend();
                match load_parakeet(models, p, accel, parakeet_threads(backend)) {
                    Ok(t) => {
                        return Ok(WhisperChoice {
                            transcriber: AnyTranscriber::Parakeet(Box::new(t)),
                            backend,
                            rejected,
                        });
                    }
                    Err(e) => rejected.push((backend, e)),
                }
            }
            Err(RuntimeError::NoBackend(rejected))
        }
    }
}

/// The production [`WhisperLoader`]: `earshot_asr_whisper::evidence::load_verified`, on
/// the device of the expected backend (`WhisperConfig::for_backend`: the CPU, or the first
/// discrete then integrated CUDA or Vulkan device).
///
/// `load_verified` installs the process-wide log capture (whisper.cpp's backend log is the
/// evidence), so a host that wants verified backends must not install another `log`
/// logger, and loads must not run concurrently with other whisper.cpp loads.
#[derive(Debug, Clone, Default)]
pub struct EvidenceLoader {
    /// CPU threads for whisper.cpp (`None`: `WhisperConfig`'s default).
    pub threads: Option<std::num::NonZeroUsize>,
    /// Live tracks sharing the machine: with `threads` unset, each load gets
    /// [`live_whisper_threads`] for the backend it is verified on.
    pub live_tracks: Option<usize>,
}

impl EvidenceLoader {
    /// A loader for `tracks` live tracks: whisper threads per [`live_whisper_threads`].
    #[must_use]
    pub fn for_live(tracks: usize) -> Self {
        Self {
            threads: None,
            live_tracks: Some(tracks),
        }
    }
}

/// Most whisper CPU threads one live track may use (T6.10b).
pub const MAX_LIVE_THREADS_PER_TRACK: usize = 4;

/// CPU threads for one live whisper worker when `tracks` live tracks share a machine with
/// `cores` logical cores (decided by the owner: a meeting app must leave the call
/// app room, and laptops and phones heat up): 2 when whisper runs on a GPU (the CPU only
/// prepares audio and picks tokens); on the CPU at most half of the cores in total, split
/// between the tracks, never more than [`MAX_LIVE_THREADS_PER_TRACK`] per track, at least
/// one.
#[must_use]
pub fn live_whisper_threads(
    backend: BackendKind,
    cores: usize,
    tracks: usize,
) -> std::num::NonZeroUsize {
    let n = match backend {
        BackendKind::Cuda | BackendKind::Vulkan => 2,
        _ => (cores / 2 / tracks.max(1)).min(MAX_LIVE_THREADS_PER_TRACK),
    };
    std::num::NonZeroUsize::new(n).unwrap_or(std::num::NonZeroUsize::MIN)
}

impl WhisperLoader for EvidenceLoader {
    type Transcriber = VerifiedWhisper;

    fn load_verified(
        &self,
        model: &Path,
        expected: BackendKind,
    ) -> Result<VerifiedWhisper, AsrError> {
        // The device of the expected backend (a build with CUDA and Vulkan lists the same
        // card once per backend); a backend this build or machine lacks is refused here.
        let mut config = WhisperConfig::for_backend(model, expected)?;
        if let Some(threads) = self.threads {
            config.threads = threads;
        } else if let Some(tracks) = self.live_tracks {
            let cores = std::thread::available_parallelism().map_or(1, std::num::NonZeroUsize::get);
            config.threads = live_whisper_threads(expected, cores, tracks);
        }
        let (inner, evidence) = evidence::load_verified(&config, expected)?;
        let reported = inner.backend_info();
        Ok(VerifiedWhisper {
            inner,
            expected,
            reported,
            evidence,
        })
    }
}

/// A whisper model whose backend was verified at load and is re-checked after every call
/// (`evidence::check_call_log`): a beam-search call re-initialises the backend and may
/// fall back to the CPU for that call. Such a call's result is kept (it is a valid
/// transcription, only slower), and from then on [`Transcriber::backend_info`] reports
/// the CPU with the reason, so the fallback is never silent (the post-meeting processor
/// warns and records it).
///
/// The check reads the whisper lines logged during the call; whisper.cpp work of another
/// model in the process at the same time can make it report a fallback that did not
/// happen (the safe direction).
#[derive(Debug)]
pub struct VerifiedWhisper {
    inner: WhisperTranscriber,
    expected: BackendKind,
    reported: BackendInfo,
    evidence: evidence::BackendEvidence,
}

impl VerifiedWhisper {
    /// What whisper.cpp's log of the load proved (the classification and its lines).
    #[must_use]
    pub fn evidence(&self) -> &evidence::BackendEvidence {
        &self.evidence
    }
}

impl AnyTranscriber<VerifiedWhisper> {
    /// How the backend was proven, for reports: whisper.cpp's log classification and the
    /// lines that decided it, or ONNX Runtime's node placement for Parakeet.
    #[must_use]
    pub fn backend_evidence(&self) -> String {
        match self {
            Self::Whisper(w) => format!(
                "whisper.cpp log: {} [{}]",
                w.evidence.log.name(),
                w.evidence
                    .lines
                    .iter()
                    .map(|l| l.split_whitespace().collect::<Vec<_>>().join(" "))
                    .collect::<Vec<_>>()
                    .join("; ")
            ),
            Self::Parakeet(_) => self.onnx_placement().unwrap_or_default(),
        }
    }
}

impl<W> AnyTranscriber<W> {
    /// ONNX Runtime's node placement of every session of a Parakeet model, for reports
    /// (`None` for whisper, whose evidence is whisper.cpp's log).
    #[must_use]
    pub fn onnx_placement(&self) -> Option<String> {
        match self {
            Self::Whisper(_) => None,
            Self::Parakeet(p) => Some(format!(
                "ONNX Runtime placement: {}",
                p.backends()
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join("; ")
            )),
        }
    }
}

/// What one call's log says about where it ran.
///
/// - `Ok(None)`: the log agrees with `expected` (or the call initialised nothing).
/// - `Ok(Some(cpu))`: a GPU call fell back with a positive CPU signal
///   (`whisper_backend_init_gpu: failed to initialize` or `no GPU found`): the call ran on
///   the CPU, reported with the reason.
/// - `Err`: any other contradiction (for example a re-initialisation that names no
///   backend, or a GPU on a CPU model). The backend is unknown, which is an error, never
///   taken as the CPU (AGENTS.md).
///
/// # Errors
/// [`AsrError::Backend`] for an unknown or contradictory call log.
pub fn degraded_backend(
    loaded: &BackendInfo,
    expected: BackendKind,
    call_log: &[String],
) -> Result<Option<BackendInfo>, AsrError> {
    let Err(reason) = evidence::check_call_log(expected, call_log) else {
        return Ok(None);
    };
    let cpu_signal = expected != BackendKind::Cpu
        && call_log.iter().any(|l| {
            l.contains("whisper_backend_init_gpu")
                && (l.contains("failed to initialize") || l.contains("no GPU found"))
        });
    if !cpu_signal {
        return Err(reason);
    }
    Ok(Some(BackendInfo {
        kind: BackendKind::Cpu,
        device: format!("cpu (fell back from {}: {reason})", loaded.device),
        model: loaded.model.clone(),
    }))
}

impl Transcriber for VerifiedWhisper {
    fn transcribe(
        &mut self,
        audio: &MonoBuffer,
        options: &TranscribeOptions,
        cancel: &CancelToken,
    ) -> Result<Transcription, AsrError> {
        let mark = evidence::log_mark();
        let mut result = self.inner.transcribe(audio, options, cancel);
        let lines = evidence::logs_since_complete(mark).ok_or_else(|| {
            AsrError::Backend("the whisper log of this call overflowed the capture".into())
        })?;
        if let Some(actual) = degraded_backend(&self.reported, self.expected, &lines)? {
            self.reported = actual;
        }
        if let Ok(t) = &mut result {
            t.backend = self.reported.clone();
        }
        result
    }

    fn backend_info(&self) -> BackendInfo {
        self.reported.clone()
    }

    fn may_skip_speech(&self) -> bool {
        self.inner.may_skip_speech()
    }
}

/// Live ASR must decode greedily: the live session reads the backend once, at its start
/// ([`crate::live::RuntimeEvent::LiveBackend`]), which stays true only because greedy
/// decoding reuses the loaded state; beam search re-initialises the backend on every call
/// (and could fall back per call unseen by the live view).
///
/// # Errors
/// [`RuntimeError::Unavailable`] for any other decoding strategy.
pub fn check_live_options(config: &ChunkerConfig) -> Result<(), RuntimeError> {
    if config.options.decoding.strategy == earshot_core::asr::DecodingStrategy::Greedy {
        Ok(())
    } else {
        Err(RuntimeError::Unavailable(
            "live transcription must decode greedily (beam search re-initialises the backend per call)",
        ))
    }
}

/// Live ASR for [`crate::live::start_live`]: per track, the live transcription model `model`
/// (whisper or Parakeet, [`select_asr`]) on the best verified accelerator of `order` and a
/// Silero VAD on `vad_accel` (ONNX Runtime must be initialised for it), with the default
/// chunker settings. Parakeet gets the T6.10b live thread budget for a two-track session
/// ([`live_whisper_threads`]). Segment ids
/// are UUIDv7 from `clock` and a generator seeded from `rng` per track
/// ([`EntityId::generate`]); the refused backends go into the [`TrackAsr`].
///
/// The name is checked when the factory runs ([`asr_model`]).
pub fn live_asr_factory<L>(
    loader: Arc<L>,
    models: ModelDir,
    model: String,
    order: Vec<Accelerator>,
    vad_accel: Accelerator,
    clock: SharedClock,
    mut rng: BoxRng,
) -> LiveAsrFactory
where
    L: WhisperLoader + Send + Sync + 'static,
{
    Box::new(move |_track: TrackId, entity: EntityId| {
        let choice = select_asr(&*loader, &models, &model, &order, &|backend| {
            Some(live_whisper_threads(backend, cores(), 2).get())
        })?;
        let vad = load_silero(&models, vad_accel)?;
        let config = ChunkerConfig::default();
        check_live_options(&config)?;
        let mut seed = [0u8; 32];
        rng.fill_bytes(&mut seed);
        let mut track_rng = ChaCha20Rng::from_seed(seed);
        let clock = Arc::clone(&clock);
        let segment_ids = Box::new(move || EntityId::generate(&*clock, &mut track_rng));
        let evidence = choice.transcriber.onnx_placement();
        Ok(Some(TrackAsr {
            asr: LiveAsr {
                transcriber: Box::new(choice.transcriber),
                vad: Box::new(vad),
                config,
                track_entity: entity,
                segment_ids,
            },
            fallbacks: choice.rejected,
            speakers: None,
            evidence,
        }))
    })
}

/// A post-meeting processor and how its final-pass whisper was placed.
#[derive(Debug)]
pub struct PostSetup<T> {
    /// The processor.
    pub processor: PostProcessor<T, PyannoteSegmenter, CamppEmbedder>,
    /// The backend the final-pass transcription model (whisper or Parakeet) was verified on
    /// when it was loaded. A later call may fall back (see [`VerifiedWhisper`]): after a
    /// run, show [`crate::post::PostOutcome::backend`] or the recorded [`pass_backend`],
    /// not this field, especially when the processor is reused for several meetings.
    pub asr_backend: BackendKind,
    /// Better backends refused for it, with the reason.
    pub fallbacks: Vec<(BackendKind, AsrError)>,
}

/// The final-pass transcription model `model` exactly as [`post_processor`] loads it:
/// [`select_asr`] on the first verified accelerator of `order`, Parakeet with the one-job
/// thread budget ([`live_whisper_threads`] for one track). Also what `accept m2`'s
/// production evaluation (the `m2_prod` example) transcribes the final pass with.
///
/// # Errors
/// As [`select_asr`].
pub fn final_pass_asr<L: WhisperLoader + ?Sized>(
    loader: &L,
    models: &ModelDir,
    model: &str,
    order: &[Accelerator],
) -> Result<WhisperChoice<AnyTranscriber<L::Transcriber>>, RuntimeError> {
    select_asr(loader, models, model, order, &|backend| {
        Some(live_whisper_threads(backend, cores(), 1).get())
    })
}

/// The post-meeting processor, with `config`:
/// - the final-pass transcription model `model` (whisper or Parakeet, [`select_asr`];
///   Parakeet with the one-job thread budget) on the best verified accelerator of `order`;
/// - pyannote, CAM++ and Silero (final-pass speech pieces) on `onnx_accel` (ONNX Runtime
///   must be initialised for it); two copies of pyannote and CAM++, so the two tracks are
///   diarised side by side (T6.12), only when the transcription model runs on a GPU (on the
///   CPU it already uses the CPU budget while diarisation runs) and
///   [`parallel_diarisation`] allows.
///
/// # Errors
/// As [`asr_model`], [`select_asr`], [`load_diarisation`] and
/// [`PostProcessor::new`] (an embedder without calibrated thresholds).
pub fn post_processor<L: WhisperLoader + ?Sized>(
    loader: &L,
    models: &ModelDir,
    model: &str,
    order: &[Accelerator],
    onnx_accel: Accelerator,
    config: PostConfig,
) -> Result<PostSetup<AnyTranscriber<L::Transcriber>>, RuntimeError> {
    let choice = final_pass_asr(loader, models, model, order)?;
    // Two tracks are diarised side by side on two copies of the speaker models where the
    // cores allow (T6.12), each copy with half the ONNX thread budget.
    let parallel = choice.backend != BackendKind::Cpu && parallel_diarisation(cores());
    // One count for both speaker-model copies, also what live windows must match.
    let threads = speaker_threads(cores());
    let (segmenter, embedder) = load_diarisation_with(models, onnx_accel, threads, false)?;
    // The final pass decodes in Silero speech pieces, never one whole-track call (T6.16).
    let vad = load_silero(models, onnx_accel)?;
    let mut processor = PostProcessor::new(choice.transcriber, segmenter, embedder, config)?
        .with_pieces(Box::new(vad), PieceConfig::default())?
        .with_speaker_threads(Some(threads));
    if parallel {
        let (segmenter, embedder) = load_diarisation_with(models, onnx_accel, threads, false)?;
        processor = processor.with_second_diarizer(segmenter, embedder)?;
    }
    Ok(PostSetup {
        processor,
        asr_backend: choice.backend,
        fallbacks: choice.rejected,
    })
}

pub(crate) fn pass_key(
    meeting: EntityId,
    pass: TranscriptPass,
    track: Option<TrackId>,
) -> LocalKey {
    let pass = match pass {
        TranscriptPass::Live => "live",
        TranscriptPass::Final => "final",
    };
    let track = match track {
        Some(TrackId::Mic) => "mic",
        Some(TrackId::Reference) => "reference",
        None => "all",
    };
    MeetingRecord::PassBackend.key(&[pass], meeting, &[track])
}

/// Records which backend ran a transcript pass of `meeting` (`track` for one track,
/// `None` for all), in the device-local state: reports must say which backend ran.
///
/// # Errors
/// Store errors.
pub fn record_pass_backend(
    store: &mut Store,
    meeting: EntityId,
    pass: TranscriptPass,
    track: Option<TrackId>,
    backend: &BackendInfo,
) -> Result<(), RuntimeError> {
    let value = serde_json::to_vec(backend)
        .map_err(|_| RuntimeError::Internal("backend info does not serialise"))?;
    crate::local::set(store, &pass_key(meeting, pass, track), &value)?;
    Ok(())
}

/// The backend recorded by [`record_pass_backend`], if any.
///
/// # Errors
/// Store errors; [`RuntimeError::Internal`] for a value that does not parse.
pub fn pass_backend(
    store: &Store,
    meeting: EntityId,
    pass: TranscriptPass,
    track: Option<TrackId>,
) -> Result<Option<BackendInfo>, RuntimeError> {
    store
        .local_value(pass_key(meeting, pass, track).as_str())?
        .map(|v| {
            serde_json::from_slice(&v)
                .map_err(|_| RuntimeError::Internal("stored backend info does not parse"))
        })
        .transpose()
}
/// Starts a live session on the default microphone (`mic_role`) and, in a call, the
/// loopback source of `mode` (Windows). In the room the meeting has one (mic) track. The
/// clock domain is QPC.
///
/// # Errors
/// [`RuntimeError::Capture`] if capture cannot start; as [`crate::live::start_live`].
#[cfg(windows)]
pub fn start_wasapi_session(
    services: crate::Services,
    config: crate::live::LiveConfig,
    mic_role: earshot_capture_wasapi::MicRole,
    mode: &earshot_capture_wasapi::CaptureMode,
    live_asr: Option<LiveAsrFactory>,
    rng: crate::BoxRng,
) -> Result<crate::live::LiveSession, RuntimeError> {
    start_wasapi_session_on(
        services,
        config,
        mic_role,
        mode,
        &earshot_capture_wasapi::DeviceSelection::default(),
        live_asr,
        rng,
    )
}

/// [`start_wasapi_session`] on the endpoints of `devices` (T7.4i, the Audio page): the
/// chosen microphone instead of the default one and the chosen output for a device
/// loopback. A chosen endpoint that cannot be opened is replaced by the default and reported
/// at the start as [`crate::live::RuntimeWarning::ChosenDeviceUnavailable`] (after any
/// Bluetooth hands-free warning).
///
/// # Errors
/// [`RuntimeError::Capture`] if capture cannot start; as [`crate::live::start_live`].
#[cfg(windows)]
pub fn start_wasapi_session_on(
    services: crate::Services,
    config: crate::live::LiveConfig,
    mic_role: earshot_capture_wasapi::MicRole,
    mode: &earshot_capture_wasapi::CaptureMode,
    devices: &earshot_capture_wasapi::DeviceSelection,
    live_asr: Option<LiveAsrFactory>,
    rng: crate::BoxRng,
) -> Result<crate::live::LiveSession, RuntimeError> {
    let source = earshot_capture_wasapi::wasapi::WasapiSource::start_on(mic_role, mode, devices)
        .map_err(|e| RuntimeError::Capture(e.to_string()))?;
    let mut config = crate::live::LiveConfig {
        clock_domain: earshot_core::model::ClockDomain::Qpc,
        ..config
    };
    // The audio check's delay for the pair opened (T7.4j): a warm-start hint only. A store
    // error only loses the hint.
    if config.pipeline.delay_hint.is_none()
        && let (Some(mic), Some(output)) = (
            source.mic_endpoint().and_then(|p| p.id.as_deref()),
            source.output_endpoint_id(),
        )
    {
        let max_delay = config.pipeline.aec.linear.delay.max_delay;
        config.pipeline.delay_hint =
            crate::audio_check::delay_hint_for(&services.store(), mic, output, max_delay)
                .ok()
                .flatten();
    }
    let warnings = source
        .mic_warning()
        .map(hands_free_warning)
        .into_iter()
        .chain(source.fallbacks().iter().map(device_fallback_warning))
        .collect();
    crate::live::start_live_with_warnings(source, services, config, live_asr, rng, warnings)
}

/// The session warning for a chosen endpoint replaced by the default (T7.4i).
#[cfg(windows)]
#[must_use]
pub fn device_fallback_warning(
    f: &earshot_capture_wasapi::DeviceFallback,
) -> crate::live::RuntimeWarning {
    crate::live::RuntimeWarning::ChosenDeviceUnavailable {
        track: f.track,
        chosen: f.chosen_name.clone(),
        used: f.used.clone(),
        error: match &f.cause {
            earshot_capture_wasapi::FallbackCause::NotConnected => None,
            earshot_capture_wasapi::FallbackCause::OpenFailed(e) => Some(e.clone()),
        },
    }
}

/// The session warning for a Bluetooth hands-free mic found by the capture layer (T6.24).
#[cfg(windows)]
#[must_use]
pub fn hands_free_warning(
    mic: earshot_capture_wasapi::HandsFreeMic,
) -> crate::live::RuntimeWarning {
    crate::live::RuntimeWarning::BluetoothHandsFreeMic {
        device_name: mic.device_name,
        sample_rate: mic.sample_rate,
    }
}
