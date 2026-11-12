//! The Earshot runtime (PLAN.md 3.3, T6.4): owns the threads, drives the sans-IO core
//! pipeline from a capture source, persists the meeting, and reports to the UI through an
//! event stream.
//!
//! - [`live`]: a live session. A capture source (WASAPI, or a replay) is pumped into the
//!   core [`earshot_core::pipeline::Pipeline`] on one session thread; its audio is written
//!   to FLAC, live transcript finals are stored, runtime policies (clock re-sync, device
//!   loss reporting) run on its stats, and the final-pass request is returned at stop.
//! - [`services`]: the shared store, data directory, clock and RNG.
//!
//! Later parts of T6.4 add the post-meeting jobs (final pass, diarisation, voiceprints),
//! the summary (through `earshot-llm` / `earshot-net` only) and the on-demand Obsidian
//! export. Nothing here exports to a vault on its own.

use std::fmt;

use earshot_core::asr::AsrError;
use earshot_core::diar::{DiarError, EmbeddingModelId};
use earshot_core::export::ExportError;
use earshot_core::model::RepoError;
use earshot_core::pipeline::PipelineError;
use earshot_core::voiceprint::VoiceprintError;
use earshot_llm::LlmError;
/// The network errors inside [`RuntimeError`] (and a summary's [`LlmError::Net`]), for hosts
/// that choose their copy by kind.
pub use earshot_net::{DenyReason, NetError, UrlError};
/// The ONNX Runtime error inside [`RuntimeError::Onnx`].
pub use earshot_onnx::OnnxError;
use earshot_store::StoreError;

pub mod archive;
pub mod audio_check;
pub mod audio_settings;
pub mod backends;
pub mod benchmark;
pub mod call_end;
pub mod catalogue;
pub mod claude_code;
pub mod codex;
pub mod cpu;
pub mod cuda;
pub mod cuda_switch;
pub mod delete;
pub mod download;
pub mod echo;
pub mod edit;
pub mod export;
pub mod export_settings;
pub mod first_run;
pub mod folders;
pub mod host;

pub mod identities;
pub mod keep_local;
pub mod live;
pub mod live_summaries;
mod local;
pub mod meeting_queue;
pub mod model_bench;
pub mod model_check;
pub mod model_settings;
pub mod my_voice;
pub mod npu_compiled;
pub mod post;
pub mod power;
pub mod preview_settings;
mod priority;
pub mod recommend;
pub mod replay;
pub mod retention;
pub mod search;
pub mod services;
pub mod speaker_timing;
pub mod speakers;
pub mod stretches;
pub mod summary;
pub mod summary_child;
pub mod summary_model;
pub mod summary_settings;
pub mod summary_style;
pub mod update;
pub mod view;
pub mod vocabulary;
pub mod word_choices;

#[cfg(test)]
mod testutil;

/// The hardware facts' types (T7.4d), for hosts that show them.
pub use earshot_hardware;
pub use post::Calibration;

/// This machine's hardware facts: `earshot_hardware::read()`, the one source of them.
#[must_use]
pub fn hardware() -> earshot_hardware::Hardware {
    earshot_hardware::read()
}
pub use services::{
    BoxRng, Db, MeetingActivity, MeetingJob, MeetingLease, Services, SharedClock, SystemClock,
    os_seeded_rng,
};

/// Runtime failures that end a job (recoverable problems are events instead).
#[derive(Debug)]
pub enum RuntimeError {
    /// The database failed.
    Store(StoreError),
    /// A repository write failed.
    Repo(RepoError),
    /// The pipeline could not be built (configuration, unusable rate, live ASR).
    Pipeline(PipelineError),
    /// A runtime thread panicked, or panicked while holding the store.
    Poisoned,
    /// A runtime thread could not be started.
    Spawn(std::io::Error),
    /// A contract the runtime relies on was broken (a bug, not an input problem).
    Internal(&'static str),
    /// A speech recognition call failed or was cancelled.
    Asr(AsrError),
    /// Diarisation failed or was cancelled.
    Diar(DiarError),
    /// Voiceprint matching or enrolment failed.
    Voiceprint(VoiceprintError),
    /// A post-meeting run was given no tracks (it would only remove data).
    NoTracks,
    /// The embedding model has no calibrated threshold for `purpose`: calibrate it on
    /// dev data first (there is no fallback value).
    Uncalibrated {
        /// Which threshold.
        purpose: Calibration,
        /// The model.
        model: EmbeddingModelId,
    },
    /// The summary endpoint or the model call failed (or was cancelled).
    Llm(LlmError),
    /// A local summary failed: the model did not load, the transcript could not be
    /// chunked, or the run failed, was still invalid after its repairs or was cancelled
    /// ([`summary::LocalSummaryError`]).
    LocalSummary(summary::LocalSummaryError),
    /// The network client could not be built.
    Net(NetError),
    /// The configured endpoint cannot be allowlisted.
    Endpoint(UrlError),
    /// `earshot-net` refuses the configured endpoint (plain `http` to a public host).
    EndpointDenied(DenyReason),
    /// The export options are unusable.
    Export(ExportError),
    /// The data changed while a job was working on it (retry the job).
    Conflict(&'static str),
    /// A model file is missing; `cargo xtask fetch-models --set <fetch_set>` provides it.
    ModelMissing {
        /// Where it was expected.
        path: std::path::PathBuf,
        /// The manifest set that contains it (`None`: not a manifest model).
        fetch_set: Option<&'static str>,
    },
    /// A model file of another size than the manifest's (an interrupted copy);
    /// `cargo xtask fetch-models --set <fetch_set>` replaces it.
    ModelIncomplete {
        /// The file.
        path: std::path::PathBuf,
        /// The manifest size in bytes.
        expected: u64,
        /// Its size.
        found: u64,
        /// The manifest set that contains it.
        fetch_set: &'static str,
    },
    /// A model name that is not in models/manifest.toml.
    UnknownModel(String),
    /// No accelerator could be verified for a model (each refusal, best first).
    NoBackend(Vec<(earshot_core::asr::BackendKind, AsrError)>),
    /// The requested backend is not available in this build.
    Unavailable(&'static str),
    /// ONNX Runtime or one of its models failed.
    Onnx(earshot_onnx::OnnxError),
    /// Audio capture could not start.
    Capture(String),
    /// A WASAPI capture could not start (T9.23): what it means for the user, read from the
    /// Windows error code ([`capture_start_error`]), and the capture's own text for the CLI.
    CaptureStart {
        /// What the user can do about it.
        problem: CaptureProblem,
        /// The capture error's text (developer words; never shown in a window).
        detail: String,
    },
    /// This processor lacks the instructions the whisper.cpp and llama.cpp builds need
    /// ([`cpu`], T9.23): refused instead of an illegal-instruction crash.
    ProcessorTooOld,
    /// A live session, final pass or export is working on the meeting, or it is being
    /// deleted: the deletion (or the job) is refused. Retry when it has finished.
    MeetingBusy(earshot_core::identity::EntityId),
    /// A meeting edit was refused ([`edit::EditError`]); nothing was written.
    Edit(edit::EditError),
    /// The summary endpoint's API key is missing or its store failed
    /// ([`summary_settings::ApiKeyError`]; never the key itself).
    ApiKey(summary_settings::ApiKeyError),
    /// The meeting's summary is kept on this computer ([`keep_local`], T12.13) and the summary
    /// service would send it elsewhere: refused before any text left the store.
    SummaryKeptLocal(earshot_core::identity::EntityId),
}

impl RuntimeError {
    /// Whether retrying the same operation can succeed without user action.
    #[must_use]
    pub fn is_retryable(&self) -> bool {
        match self {
            Self::MeetingBusy(_) | Self::Conflict(_) => true,
            Self::Store(e) => e.is_retryable(),
            Self::Llm(earshot_llm::LlmError::ClaudeCode(e)) => e.is_retryable(),
            Self::Llm(earshot_llm::LlmError::Codex(e)) => e.is_retryable(),
            // A child that died or stalled says nothing about the next try.
            Self::LocalSummary(
                summary::LocalSummaryError::GpuChildDied { .. }
                | summary::LocalSummaryError::GpuChildStalled,
            ) => true,
            _ => false,
        }
    }
}

impl fmt::Display for RuntimeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Store(e) => write!(f, "store: {e}"),
            Self::Repo(e) => write!(f, "store: {e}"),
            Self::Pipeline(e) => write!(f, "pipeline: {e}"),
            Self::Poisoned => write!(f, "a runtime thread panicked"),
            Self::Spawn(e) => write!(f, "cannot start a runtime thread: {e}"),
            Self::Internal(m) => write!(f, "internal error: {m}"),
            Self::Asr(e) => write!(f, "speech recognition: {e}"),
            Self::Diar(e) => write!(f, "diarisation: {e:?}"),
            Self::Voiceprint(e) => write!(f, "voiceprints: {e:?}"),
            Self::NoTracks => write!(f, "no recorded track to process"),
            Self::Uncalibrated { purpose, model } => write!(
                f,
                "no calibrated {purpose} threshold for embedding model {}",
                model.as_str()
            ),
            Self::Llm(e) => write!(f, "summary: {e}"),
            Self::LocalSummary(e) => write!(f, "local summary: {e}"),
            Self::Net(e) => write!(f, "network: {e}"),
            Self::Endpoint(e) => write!(f, "summary endpoint: {e}"),
            Self::EndpointDenied(e) => write!(f, "summary endpoint refused: {e}"),
            Self::Export(e) => write!(f, "export: {e}"),
            Self::Conflict(m) => write!(f, "changed meanwhile: {m}"),
            Self::ModelMissing {
                path,
                fetch_set: Some(set),
            } => write!(
                f,
                "model not found at {}; run `cargo xtask fetch-models --set {set}` (or point EARSHOT_MODELS at a directory that contains it)",
                path.display()
            ),
            Self::ModelMissing {
                path,
                fetch_set: None,
            } => write!(
                f,
                "model not found at {} (it is not in models/manifest.toml)",
                path.display()
            ),
            Self::ModelIncomplete {
                path,
                expected,
                found,
                fetch_set,
            } => write!(
                f,
                "model {} is {found} bytes, not {expected}; run `cargo xtask fetch-models --set {fetch_set}`",
                path.display()
            ),
            Self::UnknownModel(name) => {
                write!(f, "model {name:?} is not in models/manifest.toml")
            }
            Self::NoBackend(rejected) => {
                write!(f, "no backend could be verified:")?;
                for (kind, e) in rejected {
                    write!(f, " {kind:?}: {e};")?;
                }
                Ok(())
            }
            Self::Unavailable(m) => write!(f, "unavailable: {m}"),
            Self::Onnx(e) => write!(f, "onnx runtime: {e}"),
            Self::Capture(m) => write!(f, "capture: {m}"),
            Self::CaptureStart { detail, .. } => write!(f, "capture: {detail}"),
            Self::ProcessorTooOld => write!(
                f,
                "this processor lacks AVX2 or FMA, which the whisper.cpp and llama.cpp builds need"
            ),
            Self::MeetingBusy(m) => write!(
                f,
                "meeting {m} is being recorded, processed, exported or deleted; try again when it has finished"
            ),
            Self::Edit(e) => write!(f, "edit refused: {e}"),
            Self::ApiKey(e) => write!(f, "{e}"),
            Self::SummaryKeptLocal(m) => write!(
                f,
                "meeting {m} keeps its summary on this computer; only a local summary model may summarise it"
            ),
        }
    }
}

impl std::error::Error for RuntimeError {}

/// Why a capture could not start, as far as the user can act on it (T9.23); the app's copy is
/// chosen from this, never from the error's text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaptureProblem {
    /// No microphone is connected.
    NoMicrophone,
    /// No speakers or headphones are connected.
    NoOutput,
    /// Windows' microphone privacy switch refuses desktop apps.
    MicrophoneAccessOff,
    /// Another app holds the device for itself.
    DeviceInUse,
    /// Anything else.
    Other,
}

#[cfg(windows)]
impl From<earshot_capture_wasapi::StartProblem> for CaptureProblem {
    fn from(p: earshot_capture_wasapi::StartProblem) -> Self {
        use earshot_capture_wasapi::StartProblem as S;
        match p {
            S::NoMicrophone => Self::NoMicrophone,
            S::NoOutput => Self::NoOutput,
            S::MicrophoneAccessOff => Self::MicrophoneAccessOff,
            S::DeviceInUse => Self::DeviceInUse,
            S::Other => Self::Other,
        }
    }
}

/// The runtime error of a WASAPI capture that could not start: [`RuntimeError::CaptureStart`]
/// with the problem its Windows error code means.
#[cfg(windows)]
#[must_use]
pub fn capture_start_error(e: &earshot_capture_wasapi::wasapi::CaptureError) -> RuntimeError {
    RuntimeError::CaptureStart {
        problem: e.start_problem().into(),
        detail: e.to_string(),
    }
}

impl From<StoreError> for RuntimeError {
    fn from(e: StoreError) -> Self {
        Self::Store(e)
    }
}

impl From<RepoError> for RuntimeError {
    fn from(e: RepoError) -> Self {
        Self::Repo(e)
    }
}

impl From<PipelineError> for RuntimeError {
    fn from(e: PipelineError) -> Self {
        Self::Pipeline(e)
    }
}
