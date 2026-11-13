//! The host layer (T7.2a): what a front end (the `earshot` CLI, the desktop app) does around
//! a meeting, without printing anything. Every decision here returns data; the host shows
//! it. The CLI's `record` and `replay` are this module plus their printing.
//!
//! A host runs a meeting in this order (each step can fail before anything is recorded,
//! so a bad setting never costs a recorded meeting its post pass):
//!
//! 1. [`model_dir`], then [`choose_model`] (a `--model` request, else the current model
//!    check of this machine, else [`FALLBACK_MODEL`]) and [`require_models`] (every model
//!    file the run loads, the post models only when the post pass runs).
//! 2. [`summary_plan`] (a remote endpoint, the local model this machine's memory picks when
//!    it is installed, or none) and [`prepare_summary`], which builds the service and, for
//!    a local model, first checks that llama.cpp's DLLs are where the host says the exe
//!    finds them ([`missing_llama_dlls`]; the runtime never guesses the exe layout).
//! 3. [`init_onnx_for`] (ONNX Runtime, the CPU library unless a Parakeet model may run on
//!    CUDA or the NPU) and [`choose_speaker_timing`].
//! 4. [`start_meeting`] with a [`Starter`] for the capture mode ([`wasapi_starter`] for a
//!    call or the room on Windows, a replay for tests and `earshot-cli replay`) and the live
//!    transcription of [`live_factory`]. The session's events are the host's to show.
//! 5. [`after_meeting`] with the session's [`LiveOutcome`]: the final pass with progress
//!    ([`HostStep`], including every [`PostStage`]) and a [`CancelToken`], then the summary
//!    per the plan; the result is a [`HostOutcome`]. While it runs the meeting reads as
//!    [`crate::MeetingActivity::Processing`].
//!
//! Cancelling the post pass ([`AfterMeeting::post_cancel`]) keeps the recordings and the
//! live transcript and deletes the speaker windows computed during the meeting (biometric
//! data no final pass will finish from, [`cleanup_cancelled`]); the summary then still
//! runs on the live draft, with its own token ([`AfterMeeting::summary_cancel`]).

use std::path::PathBuf;
use std::sync::Arc;

use earshot_core::asr::{AsrError, BackendKind, CancelToken, Transcriber};
use earshot_core::capture::TrackId;
use earshot_core::diar::{SpeakerEmbedder, SpeakerSegmenter};
use earshot_core::identity::EntityId;
use earshot_net::AuditEntry;
use earshot_store::{DataDir, Store};

use crate::backends::{
    Accelerator, CAMPPLUS, EvidenceLoader, ModelDir, SEGMENTATION, SILERO, asr_model, init_onnx,
    live_asr_factory, onnx_library, order_from, post_processor, with_live_speakers,
};
pub use crate::keep_local::SummaryPrivacy;
use crate::live::{LiveAsrFactory, LiveConfig, LiveOutcome, LiveSession};
use crate::model_bench::{check_fingerprint, hardware_facts, stored_check};
use crate::post::{
    CleanupWork, PostConfig, PostInput, PostOutcome, PostProcessor, PostStage, load_post_input,
};
use crate::speaker_timing::{SpeakerTiming, StoredTiming, set_speaker_timing, speaker_timing};
use crate::summary::{SummaryConfig, SummaryOutcome, SummaryRunReport, SummaryService};
use crate::summary_model::{
    SummaryModelChoice, choose_summary_model_on, installed, local_summary_config, summary_gpu_here,
};
use crate::{BoxRng, MeetingJob, RuntimeError, Services, SharedClock, os_seeded_rng};

/// The transcription model when none is requested and no current model check says
/// otherwise.
pub const FALLBACK_MODEL: &str = "base.en";

/// VAD, segmentation and speaker embeddings run on the CPU (T6.11).
pub const ONNX_ACCELERATOR: Accelerator = Accelerator::Cpu;

/// Puts back any helper extraction a crash left aside in `models`
/// ([`Catalogue::recover_extractions`]). For the app's start, before a download can run.
pub fn recover_extractions(models: &ModelDir) {
    if let Ok(catalogue) = crate::catalogue::Catalogue::embedded() {
        let _ = catalogue.recover_extractions(models);
    }
}

/// The models directory: `EARSHOT_MODELS`, else the per-user data directory's models
/// (`data` when there is no per-user directory).
#[must_use]
pub fn model_dir(data: &DataDir) -> ModelDir {
    let user = DataDir::from_local_app_data().unwrap_or_else(|| data.clone());
    ModelDir::resolve(std::env::var_os("EARSHOT_MODELS"), &user)
}

/// The build's accelerator order once this start's choice of ONNX Runtime library is made
/// ([`crate::cuda::wait_for_onnx`]): what anything about to load ONNX Runtime reads, because an
/// order read before the choice names the wrong library. Pages read
/// [`crate::backends::accelerator_order`], which never waits. Never call it holding the store lock.
#[must_use]
pub fn settled_order() -> Vec<Accelerator> {
    crate::cuda::wait_for_onnx();
    crate::backends::accelerator_order()
}

/// What a held meeting runs on, decided once, when the first of its work takes its turn (T9.1l):
/// after the CUDA start has chosen, so the meeting runs on what the start chose. Capture never waits
/// for it. A meeting that never got that far (stopped during its hold) decides in its turn after the
/// meeting.
pub struct MeetingModels {
    services: Services,
    models: ModelDir,
    /// The build's order the pre-flight read, and what it checked on that order.
    checked: (Vec<Accelerator>, ResolvedModels),
    /// The meeting's hold: where the notes go.
    notes: crate::live::HoldNotes,
    /// The banner of an ONNX Runtime library that did not load.
    library_note: LibraryNote,
    resolved: std::sync::Mutex<Option<Arc<ResolvedModels>>>,
}

/// The banner of an ONNX Runtime library that did not load ([`MeetingModels`]).
pub type LibraryNote = Box<dyn Fn(Accelerator, &RuntimeError) -> String + Send + Sync>;

/// What [`MeetingModels::resolve`] decided.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedModels {
    /// The transcription model.
    pub model: String,
    /// The accelerators to try, best first.
    pub order: Vec<Accelerator>,
}

impl MeetingModels {
    /// The models of a meeting in `services`; `checked` is the build's order the pre-flight read and
    /// what it checked on it; `notes` hears what the loads find.
    #[must_use]
    pub fn new(
        services: &Services,
        models: ModelDir,
        checked: (Vec<Accelerator>, ResolvedModels),
        notes: crate::live::HoldNotes,
        library_note: LibraryNote,
    ) -> Self {
        Self {
            services: services.clone(),
            models,
            checked,
            notes,
            library_note,
            resolved: std::sync::Mutex::new(None),
        }
    }

    /// What the pre-flight checked, as a meeting has always kept the model it started with, unless
    /// the CUDA start changed what this build can run ([`settled_order`], which waits for it): then
    /// the settings are read again on that order and every model file checked. ONNX Runtime is
    /// initialised either way. Only an answer is kept, so a failure is tried again by the next step.
    ///
    /// # Errors
    /// Store errors, a missing model, or an ONNX Runtime CPU library that cannot be loaded.
    pub fn resolve(&self) -> Result<Arc<ResolvedModels>, RuntimeError> {
        let mut slot = self
            .resolved
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(resolved) = &*slot {
            return Ok(Arc::clone(resolved));
        }
        let build = settled_order();
        let (checked_build, checked) = &self.checked;
        let ResolvedModels { model, order } = if build == *checked_build {
            checked.clone()
        } else {
            let setup = {
                let db = self.services.store();
                transcription_setup(&db, &self.models, &build, &crate::hardware())?
            };
            require_models(&self.models, &setup.choice.model, true)?;
            ResolvedModels {
                model: setup.choice.model,
                order: setup.order,
            }
        };
        init_onnx_for(&self.models, &model, &order, &mut |library, e| {
            self.notes
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(crate::live::RuntimeWarning::HostNote {
                    key: "onnx".to_owned(),
                    message: (self.library_note)(library, e),
                });
        })?;
        let resolved = Arc::new(ResolvedModels { model, order });
        *slot = Some(Arc::clone(&resolved));
        Ok(resolved)
    }

    /// [`Self::resolve`] that gives way to `cancel` while it waits for the CUDA start (T9.22):
    /// `None` when `cancel` came first, so a meeting cancelled after its recording never waits for
    /// a start that may not end.
    #[must_use]
    pub fn resolve_unless(
        &self,
        cancel: &earshot_core::asr::CancelToken,
    ) -> Option<Result<Arc<ResolvedModels>, RuntimeError>> {
        if !crate::cuda::wait_for_onnx_unless(cancel) {
            return None;
        }
        Some(self.resolve())
    }

    /// Live transcription of the meeting, on what [`Self::resolve`] decides at its first call.
    pub fn live_factory(self: &Arc<Self>) -> LiveAsrFactory {
        let me = Arc::clone(self);
        let mut factory: Option<LiveAsrFactory> = None;
        Box::new(move |track, entity| {
            let f = match &mut factory {
                Some(f) => f,
                None => {
                    let r = me.resolve()?;
                    let clock = Arc::clone(&me.services.clock);
                    factory.insert(live_factory(&me.models, &r.model, &r.order, clock, false))
                }
            };
            let loaded = f(track, entity);
            store_npu_refusals(&me.services);
            // An NPU load that could not use its compiled model (T12.12a), once per process. One
            // made by a final pass or a preview shows at the next track load.
            if let Some(message) = crate::npu_compiled::take_note() {
                me.notes
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .push(crate::live::RuntimeWarning::HostNote {
                        key: "npu-compiled".to_owned(),
                        message: message.to_owned(),
                    });
            }
            loaded
        })
    }

    /// Live speaker work of the meeting.
    pub fn speaker_factory(self: &Arc<Self>) -> crate::live::HeldSpeakerFactory {
        let me = Arc::clone(self);
        Arc::new(move |_track| {
            me.resolve()?;
            Ok(Some(crate::backends::live_speakers(
                me.models.clone(),
                ONNX_ACCELERATOR,
                PostConfig::default().diarization,
            )))
        })
    }
}

/// The transcription model of a run and, when it came from the model check, the backend the
/// check kept for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelChoice {
    /// The model name.
    pub model: String,
    /// The backend the model check kept (`None`: not from the check, so the build's whole
    /// order is tried).
    pub backend: Option<BackendKind>,
}

impl ModelChoice {
    /// The accelerators to try for this run: from the check's backend when the model came from
    /// the check ([`order_from`]; a check that kept the CPU over Vulkan is not overruled, T6.10d),
    /// else the build's order without Vulkan: transcription uses Vulkan only when the model check
    /// kept it on this machine (D23, T8.4ac).
    #[must_use]
    pub fn order(&self, build: &[Accelerator]) -> Vec<Accelerator> {
        match self.backend {
            Some(b) => order_from(build, b),
            None => build
                .iter()
                .copied()
                .filter(|a| *a != Accelerator::Vulkan)
                .collect(),
        }
    }
}

pub use earshot_llm_local::{
    FolderRefused, LlamaHold, hold_llama, llama_held, load_from_folder as load_llama_folder,
    probe_gpu as probe_llama_gpu, wait_for_llama,
};

/// Whether a local summary being prepared asks for the GPU, `allow_gpu` being the plan
/// ([`local_summary_placement`], decided without llama.cpp). Normally llama.cpp is asked too. During
/// a [`hold_llama`] asking would hold a meeting's start for as long as the hold lasts, and whichever
/// set of DLLs the hold ends on has the GPU backend the plan counted on; the summary itself waits at
/// the gate and reports what really ran.
#[must_use]
pub fn summary_gpu_while_preparing(allow_gpu: bool) -> bool {
    allow_gpu && (llama_held() || summary_gpu_here())
}

/// The smallest shipped summary model installed in `models`: what a trial of a set of llama.cpp
/// DLLs loads (`None`: none is installed).
#[must_use]
pub fn smallest_installed_summary_model(models: &ModelDir) -> Option<PathBuf> {
    let mut shipped: Vec<_> = crate::summary_model::SUMMARY_MODELS.iter().collect();
    shipped.sort_by_key(|m| m.size);
    shipped.into_iter().find_map(|m| installed(models, m).ok())
}

/// Where a [`ModelChoice`] came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelSource {
    /// The model was asked for by name.
    Requested,
    /// The model check recorded on this machine, current for its hardware and models.
    ModelCheck,
    /// [`FALLBACK_MODEL`], because there is no current model check (the reason).
    Fallback(&'static str),
}

/// What the Models page recommends for meetings in `languages` on this machine
/// ([`crate::recommend::advise`]): the hardware read now, this build's accelerator order,
/// and the stored model check only when it is current for this machine, `models` and
/// `languages`.
///
/// # Errors
/// The stored model check does not parse; store errors.
pub fn models_advice(
    store: &Store,
    models: &ModelDir,
    languages: &[&str],
) -> Result<crate::recommend::ModelsAdvice, RuntimeError> {
    let fingerprint = check_fingerprint(models, &hardware_facts(), languages);
    let check = stored_check(store)?.filter(|c| c.current(&fingerprint));
    crate::recommend::advise(
        &earshot_hardware::read(),
        &crate::backends::accelerator_order(),
        languages,
        check.as_ref(),
    )
}

/// The meeting languages of a host that has no language setting yet (T7.4e adds one).
pub const DEFAULT_LANGUAGES: &[&str] = &["en"];

/// The transcription model of a run: `requested`, else the model check recorded in `store`
/// when it is current for this machine, `models` and the meeting `languages` (T6.10c; the
/// check's fingerprint holds the languages it ran for), else [`FALLBACK_MODEL`]. A host
/// without a language setting passes [`DEFAULT_LANGUAGES`].
///
/// # Errors
/// The stored model check does not parse; store errors.
pub fn choose_model(
    requested: Option<&str>,
    store: &Store,
    models: &ModelDir,
    languages: &[&str],
) -> Result<(ModelChoice, ModelSource), RuntimeError> {
    if let Some(m) = requested {
        let choice = ModelChoice {
            model: m.to_owned(),
            backend: None,
        };
        return Ok((choice, ModelSource::Requested));
    }
    let fingerprint = check_fingerprint(models, &hardware_facts(), languages);
    let why = match stored_check(store)? {
        Some(c) if c.current(&fingerprint) => {
            let choice = ModelChoice {
                model: c.model,
                backend: Some(c.backend),
            };
            return Ok((choice, ModelSource::ModelCheck));
        }
        Some(_) => "the model check is out of date for this machine",
        None => "no model check yet",
    };
    let choice = ModelChoice {
        model: FALLBACK_MODEL.to_owned(),
        backend: None,
    };
    Ok((choice, ModelSource::Fallback(why)))
}

/// Where the transcription model of [`transcription_setup`] came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TranscriptionSource {
    /// Chosen in the settings.
    Setting,
    /// The current model check (settings: automatic).
    ModelCheck,
    /// No current check: the installed recommendation for the meeting languages (why).
    Recommended(String),
    /// [`FALLBACK_MODEL`]: no current check and the recommendation is not installed (why).
    Fallback(&'static str),
}

/// Compiles, in the helper process ([`crate::npu_compiled`]), the NPU model a meeting would load now
/// (T12.12b): the model and order [`transcription_setup`] gives on [`settled_order`], when the
/// order holds the NPU (which it does only with the model's NPU version installed). For the app's
/// start-up, on a thread of its own: it loads nothing in this process, and a meeting starting
/// meanwhile waits for it and then loads the compiled model.
///
/// # Errors
/// `Ok(None)` when no meeting would load on the NPU or no helper is registered; an `Err` says why
/// nothing was compiled (a meeting's load then compiles as before).
pub fn prepare_npu_model(
    services: &Services,
    models: &ModelDir,
) -> Result<Option<crate::npu_compiled::Prepared>, RuntimeError> {
    let Some(exe) = crate::npu_compiled::helper() else {
        return Ok(None);
    };
    let setup = {
        let db = services.store();
        transcription_setup(&db, models, &settled_order(), &crate::hardware())?
    };
    if !setup.order.contains(&Accelerator::Qnn) {
        return Ok(None);
    }
    let name = setup.choice.model;
    let Ok(crate::backends::AsrModel::Parakeet(model)) = crate::backends::asr_model(&name) else {
        return Ok(None);
    };
    let dir = crate::backends::require_qnn_model(models, model).map_err(RuntimeError::Asr)?;
    let key = crate::npu_compiled::CompileKey::for_conversion(&dir)
        .map_err(|e| RuntimeError::Asr(earshot_core::asr::AsrError::Backend(e)))?;
    crate::npu_compiled::prepare(&dir, &key, |out| {
        crate::npu_compiled::helper_compile(exe, models.root(), &name, out)
    })
    .map(Some)
    .map_err(|e| RuntimeError::Asr(earshot_core::asr::AsrError::Backend(e)))
}

/// Stores the NPU loads of this process that did not use their compiled model and are not stored
/// yet ([`crate::npu_compiled::take_unstored`]) in device record `npu-compiled` (T12.12e), each
/// replacing the older entry of its kind, so a refusal can be diagnosed after the app has closed:
/// steps and times only, never a path or error text. Best effort: a store that cannot be read or
/// written loses the entry, and never costs a meeting. Called after each live track load, when a
/// meeting's work after recording starts (previews have loaded by then) and after it ends (the
/// final pass's own load).
pub fn store_npu_refusals(services: &Services) {
    store_npu_refusals_from(services, crate::npu_compiled::take_unstored());
}

/// [`store_npu_refusals`] for the refusals given.
pub(crate) fn store_npu_refusals_from(
    services: &Services,
    refused: Vec<crate::npu_compiled::Refused>,
) {
    use crate::npu_compiled::{REFUSALS_KEY, RefusalRecord};
    if refused.is_empty() {
        return;
    }
    let mut db = services.store();
    let old = db
        .local_value(REFUSALS_KEY)
        .ok()
        .flatten()
        .and_then(|bytes| RefusalRecord::decode(&bytes))
        .unwrap_or_default();
    if let Ok(bytes) = old.with(refused).encode() {
        let _ = crate::local::set(
            &mut db,
            &crate::local::DeviceRecord::NpuCompiled.key(),
            &bytes,
        );
    }
}

/// The NPU refusals stored by [`store_npu_refusals`] (`None`: none, or a record this version cannot
/// read).
#[must_use]
pub fn stored_npu_refusals(store: &Store) -> Option<crate::npu_compiled::RefusalRecord> {
    store
        .local_value(crate::npu_compiled::REFUSALS_KEY)
        .ok()
        .flatten()
        .and_then(|bytes| crate::npu_compiled::RefusalRecord::decode(&bytes))
}

/// The transcription of an app run, from the Models page settings
/// ([`crate::model_settings`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TranscriptionSetup {
    /// The model (and the check's backend when it came from the check).
    pub choice: ModelChoice,
    /// Where the model came from.
    pub source: TranscriptionSource,
    /// The accelerators to try, best first (the "where it runs" setting applied).
    pub order: Vec<Accelerator>,
    /// The meeting languages.
    pub languages: Vec<String>,
    /// Things the user is told: the forced part not used (the model runs on the processor)
    /// and stored settings this version does not know.
    pub notes: Vec<String>,
}

/// The transcription model and accelerator order of an app run (meeting start,
/// re-transcription) from the settings in `store`: a chosen model, else the model check
/// when it is current for this machine, `models` and the meeting languages, else the
/// recommendation for those languages ([`crate::recommend::recommend_transcription`] over
/// `hardware` and `build`) when it is installed, else [`FALLBACK_MODEL`]; then "where it
/// runs" ([`crate::model_settings::transcription_placement`]).
///
/// # Errors
/// The stored model check does not parse; store errors.
pub fn transcription_setup(
    store: &Store,
    models: &ModelDir,
    build: &[Accelerator],
    hardware: &earshot_hardware::Hardware,
) -> Result<TranscriptionSetup, RuntimeError> {
    use crate::model_settings::{ModelPick, model_settings, transcription_placement};
    let (settings, unknown) = model_settings(store)?;
    let languages = settings.language_refs();
    let (choice, source) = match &settings.transcription {
        ModelPick::Named(m) => (
            ModelChoice {
                model: m.clone(),
                backend: None,
            },
            TranscriptionSource::Setting,
        ),
        ModelPick::Automatic => match choose_model(None, store, models, &languages)? {
            // Nothing is requested here, so only the check or the fallback answers.
            (choice, ModelSource::ModelCheck | ModelSource::Requested) => {
                (choice, TranscriptionSource::ModelCheck)
            }
            (fallback, ModelSource::Fallback(why)) => {
                let installed = |m: &str| {
                    crate::catalogue::Catalogue::embedded().is_ok_and(|c| c.is_installed(models, m))
                };
                match crate::recommend::recommend_transcription(hardware, build, &languages) {
                    Some(r) if installed(r.model) => (
                        ModelChoice {
                            model: r.model.to_owned(),
                            backend: None,
                        },
                        TranscriptionSource::Recommended(format!("{why}; {}", r.reason)),
                    ),
                    _ => (fallback, TranscriptionSource::Fallback(why)),
                }
            }
        },
    };
    let placement = transcription_placement(
        &choice.model,
        settings.transcription_part,
        choice.order(build),
        build,
        hardware,
    );
    // A Parakeet model whose NPU conversion is not installed runs on the processor, said (T8.4f).
    let (order, npu_note) = crate::backends::npu_order(models, &choice.model, placement.order);
    let mut notes: Vec<String> = placement.note.into_iter().chain(npu_note).collect();
    notes.extend(unknown.iter().map(ToString::to_string));
    Ok(TranscriptionSetup {
        choice,
        source,
        order,
        languages: settings.languages.clone(),
        notes,
    })
}

/// Where a local summary of an app run would run, from the Models page settings in `store`:
/// the chosen local model (or the memory rule) and where it runs
/// ([`crate::model_settings::summary_placement`] over `order` and `hardware`), whoever the
/// Summaries page says writes the summary.
///
/// # Errors
/// Store errors; a stored summary model that is not shipped (refused at set time).
pub fn local_summary_placement(
    store: &Store,
    order: &[Accelerator],
    hardware: &earshot_hardware::Hardware,
) -> Result<crate::model_settings::SummaryPlacement, RuntimeError> {
    let (settings, _) = crate::model_settings::model_settings(store)?;
    crate::model_settings::summary_placement(
        &settings.summary,
        settings.summary_part,
        order,
        hardware,
    )
}

/// The summary step of an app run, from the settings ([`summary_from_settings`]).
#[derive(Debug)]
pub struct SettingsSummary {
    /// The request for [`summary_plan`].
    pub request: SummaryRequest,
    /// Where a local summary runs (pass its `gpu` to [`summary_plan`] and
    /// [`prepare_summary_on`]); its note is in `notes` only when the summary is local.
    pub placement: crate::model_settings::SummaryPlacement,
    /// Who writes the summary: the Summaries page's writer, or this computer for a meeting
    /// kept local whose writer would send it elsewhere ([`SummaryRequest::kept_local`]).
    pub writer: crate::summary_settings::SummaryWriter,
    /// Whether "summarise during the meeting" is on (honoured only with a local summary,
    /// [`live_summaries_for`]).
    pub live: bool,
    /// What the user is told: a "where it runs" choice not used (local only) and every
    /// stored Summaries setting this version does not know.
    pub notes: Vec<String>,
}

/// The summary step of an app run from the settings in `store` (T7.4g): no summary, the
/// local model of [`local_summary_placement`], the user's own endpoint
/// ([`crate::summary_settings::SummarySettings::endpoint_config`]), or Claude through the
/// user's Claude Code (T7.4ga: [`SummaryRequest::claude_code`]; whether it is installed and
/// signed in is checked later, without the store lock, by [`check_claude_code`]), or OpenAI
/// through the user's Codex (T7.4gc: [`SummaryRequest::codex`], checked by [`check_codex`]);
/// for Anthropic the API key
/// is read from `keys` once, now. A missing key or a failed read is
/// [`SummaryRequest::no_key`] (the plan is [`SummaryPlan::NoApiKey`]: no summary, with the
/// reason), never a local model or another provider.
///
/// A meeting kept on this computer (`privacy` [`SummaryPrivacy::KeptLocal`], T12.13) never
/// gets a remote writer: an endpoint, Claude Code or Codex becomes the local summary model
/// ([`SummaryRequest::kept_local`]; no API key is read), and "no summary" stays no summary.
///
/// # Errors
/// Store errors; a stored summary model that is not shipped.
pub fn summary_from_settings(
    store: &Store,
    order: &[Accelerator],
    hardware: &earshot_hardware::Hardware,
    keys: &dyn crate::summary_settings::ApiKeyStore,
    privacy: SummaryPrivacy,
) -> Result<SettingsSummary, RuntimeError> {
    use crate::summary_settings::{ApiKeyError, EndpointProvider, SummaryWriter, summary_settings};
    let (settings, unknown) = summary_settings(store)?;
    let mut placement = local_summary_placement(store, order, hardware)?;
    let mut request = SummaryRequest {
        graphics_memory: crate::model_settings::graphics_memory(hardware),
        ..SummaryRequest::default()
    };
    // The one place a kept-local meeting's writer is decided (T12.13).
    let remote = !matches!(
        settings.writer,
        SummaryWriter::OnThisComputer | SummaryWriter::NoSummary
    );
    let writer = if privacy == SummaryPrivacy::KeptLocal && remote {
        request.kept_local = true;
        SummaryWriter::OnThisComputer
    } else {
        settings.writer
    };
    match writer {
        SummaryWriter::OnThisComputer => request.local_model = placement.requested.clone(),
        SummaryWriter::NoSummary => request.skip = true,
        SummaryWriter::ClaudeCode => {
            request.claude_code = Some(crate::claude_code::ClaudeCodeSummaryConfig {
                model: (!settings.claude_model.is_empty()).then(|| settings.claude_model.clone()),
                effort: settings.claude_effort,
                ..Default::default()
            });
        }
        SummaryWriter::Codex => {
            request.codex = Some(crate::codex::CodexSummaryConfig {
                model: (!settings.codex_model.is_empty()).then(|| settings.codex_model.clone()),
                ..Default::default()
            });
        }
        SummaryWriter::OwnEndpoint => match settings.provider {
            EndpointProvider::OpenAiCompatible => {
                request.remote = Some(settings.endpoint_config(None));
            }
            // Read once; without a key the summary is skipped with the reason (the owner,
            // 2026-11-04: a summary setting never costs a recording).
            EndpointProvider::Anthropic => match keys.anthropic_key() {
                Ok(Some(key)) => request.remote = Some(settings.endpoint_config(Some(key))),
                Ok(None) => request.no_key = Some(ApiKeyError::Missing),
                Err(e) => request.no_key = Some(e),
            },
        },
    }
    if writer != SummaryWriter::OnThisComputer {
        placement.note = None;
    }
    let mut notes: Vec<String> = placement.note.clone().into_iter().collect();
    notes.extend(unknown.iter().map(ToString::to_string));
    // The style and the user's own words are read again when the summary is written
    // (`SummaryService::summarize`); here only a refused stored value is reported.
    if writer != SummaryWriter::NoSummary {
        if let (_, Some(refused)) = crate::summary_style::summary_style(store)? {
            notes.push(refused.to_string());
        }
        if let (_, Some(refused)) = crate::summary_style::summary_template(store)? {
            notes.push(refused.to_string());
        }
        if let (_, Some(refused)) = crate::vocabulary::vocabulary(store)? {
            notes.push(refused.to_string());
        }
    }
    Ok(SettingsSummary {
        request,
        placement,
        writer,
        live: settings.live,
        notes,
    })
}

/// The local summary model a run with `placement` ([`local_summary_placement`]) uses on
/// `hardware`: the requested one, else the memory rule for where it runs (the 4B whenever
/// the summary runs on the processor).
///
/// # Errors
/// [`RuntimeError::UnknownModel`] for a requested model that is not shipped.
pub fn summary_model_for(
    placement: &crate::model_settings::SummaryPlacement,
    hardware: &earshot_hardware::Hardware,
) -> Result<SummaryModelChoice, RuntimeError> {
    choose_summary_model_on(
        placement.requested.as_deref(),
        hardware.total_memory,
        placement.gpu,
        crate::model_settings::graphics_memory(hardware),
    )
}

/// The models a removal must keep, from the same functions that decide what a run uses:
/// the transcription model of [`transcription_setup`] (a chosen model, the current check's,
/// the installed recommendation or the fallback), the summary model of
/// [`local_summary_placement`] and [`summary_model_for`] (whoever writes the summary, so
/// switching back to this computer finds it), the stored check's model and its
/// fallback (current or not), and [`FALLBACK_MODEL`] and the speech helpers always. `models`,
/// `build` and `hardware` are what a run would see.
///
/// # Errors
/// Store errors; a stored check that does not parse.
pub fn protected_models(
    store: &Store,
    models: &ModelDir,
    build: &[Accelerator],
    hardware: &earshot_hardware::Hardware,
) -> Result<Vec<String>, RuntimeError> {
    let mut keep = vec![
        FALLBACK_MODEL.to_owned(),
        crate::catalogue::HELPERS_MODEL.to_owned(),
    ];
    keep.push(
        transcription_setup(store, models, build, hardware)?
            .choice
            .model,
    );
    if let Some(check) = stored_check(store)? {
        keep.push(check.model);
        keep.extend(check.fallback);
    }
    let placement = local_summary_placement(store, build, hardware)?;
    keep.push(summary_model_for(&placement, hardware)?.model.id.to_owned());
    if let Ok(catalogue) = crate::catalogue::Catalogue::embedded() {
        keep = with_npu_versions(catalogue, keep);
    }
    keep.sort();
    keep.dedup();
    Ok(keep)
}

/// `keep` with the NPU version (T8.4g) of every model in it, as `catalogue` lists them: a kept
/// model's NPU files are kept too, because a meeting on the NPU has them open.
#[must_use]
pub fn with_npu_versions(
    catalogue: &crate::catalogue::Catalogue,
    keep: Vec<String>,
) -> Vec<String> {
    let npu: Vec<String> = keep
        .iter()
        .filter_map(|m| catalogue.npu_version_of(m))
        .map(|v| v.name.clone())
        .collect();
    keep.into_iter().chain(npu).collect()
}

/// Removes model `name`'s files from `models` unless it is one of `protected`
/// ([`protected_models`]); the one removal path of the app.
///
/// # Errors
/// [`crate::catalogue::CatalogueError`] (`Chosen` for a protected model, nothing removed).
pub fn remove_model(
    models: &ModelDir,
    name: &str,
    protected: &[String],
) -> Result<crate::catalogue::Removed, crate::catalogue::CatalogueError> {
    let catalogue = crate::catalogue::Catalogue::embedded()?;
    let keep: Vec<&str> = protected.iter().map(String::as_str).collect();
    catalogue.remove(models, name, &keep)
}

/// Downloads every artefact of model `name` into `models` through `downloader`, one after
/// the other ([`crate::catalogue::Catalogue::download`]); `progress` hears the bytes held
/// over the whole model, thinned by each file's download ([`crate::download::download`]).
///
/// # Errors
/// As [`crate::catalogue::Catalogue::download`].
pub fn download_model(
    downloader: &crate::download::Downloader,
    models: &ModelDir,
    name: &str,
    cancel: &CancelToken,
    progress: &mut dyn FnMut(u64, u64),
) -> Result<(), crate::catalogue::ModelDownloadError> {
    let catalogue = crate::catalogue::Catalogue::embedded()
        .map_err(crate::catalogue::ModelDownloadError::Catalogue)?;
    catalogue.download(
        models,
        name,
        cancel,
        progress,
        &mut |url, dest, exp, cancel, p| downloader.fetch(url, dest, exp, cancel, p),
    )
}

/// Downloads what the manifest pins of the CUDA part `part` into `models` through `downloader`
/// ([`crate::cuda::download_part`]); `progress` hears the bytes held over the whole part.
///
/// # Errors
/// As [`crate::cuda::download_part`].
pub fn download_cuda_part(
    downloader: &crate::download::Downloader,
    models: &ModelDir,
    part: crate::cuda::CudaPart,
    cancel: &CancelToken,
    progress: &mut dyn FnMut(u64, u64),
) -> Result<(), crate::catalogue::ModelDownloadError> {
    let catalogue = crate::catalogue::Catalogue::embedded()
        .map_err(crate::catalogue::ModelDownloadError::Catalogue)?;
    crate::cuda::download_part(
        catalogue,
        models,
        part,
        cancel,
        progress,
        &mut |url, dest, exp, cancel, p| downloader.fetch(url, dest, exp, cancel, p),
    )
}

/// Removes the CUDA part `part` from `models` ([`crate::cuda::remove_part`]).
///
/// # Errors
/// As [`crate::cuda::remove_part`].
pub fn remove_cuda_part(
    models: &ModelDir,
    part: crate::cuda::CudaPart,
) -> Result<crate::catalogue::Removed, crate::cuda::CudaError> {
    crate::cuda::remove_part(crate::catalogue::Catalogue::embedded()?, models, part)
}

/// Initialises ONNX Runtime for the model check: the library that serves every accelerator
/// of `order` ([`crate::backends::onnx_library_for_all`]), else (`on_fallback` hears why)
/// the CPU library.
///
/// # Errors
/// The CPU library cannot be loaded.
pub fn init_onnx_for_check(
    models: &ModelDir,
    order: &[Accelerator],
    on_fallback: &mut dyn FnMut(Accelerator, &RuntimeError),
) -> Result<PathBuf, RuntimeError> {
    let library = crate::backends::onnx_library_for_all(order);
    match init_onnx(models, library) {
        Err(e) if library != Accelerator::Cpu => {
            fell_back(library);
            on_fallback(library, &e);
            init_onnx(models, Accelerator::Cpu)
        }
        r => r,
    }
}

/// Records the answer of a finished check for meetings in `languages`
/// ([`crate::model_bench::StoredCheck::new`] with every trial); `None` (nothing recorded)
/// when it found no model.
///
/// # Errors
/// Store errors.
pub fn record_model_check(
    store: &mut Store,
    models: &ModelDir,
    languages: &[&str],
    outcome: &crate::model_bench::CheckOutcome,
) -> Result<Option<crate::model_bench::StoredCheck>, RuntimeError> {
    let Some(r) = &outcome.recommendation else {
        return Ok(None);
    };
    let fingerprint = check_fingerprint(models, &outcome.facts, languages);
    let check = crate::model_bench::StoredCheck::new(fingerprint, r, &outcome.trials);
    crate::model_bench::record_check(store, &check)?;
    Ok(Some(check))
}

/// Fails (with the fetch instruction) unless every model the run will load is present:
/// the transcription model (whisper or Parakeet, every file) and Silero VAD for the live
/// pass, and pyannote segmentation and CAM++ when the post pass runs (`post`).
///
/// # Errors
/// [`RuntimeError::UnknownModel`], or the first missing or incomplete model.
pub fn require_models(models: &ModelDir, model: &str, post: bool) -> Result<(), RuntimeError> {
    let mut needed = asr_model(model)?.files();
    needed.push(SILERO.to_owned());
    if post {
        needed.extend([SEGMENTATION.to_owned(), CAMPPLUS.to_owned()]);
    }
    for rel in &needed {
        models.require(rel)?;
    }
    Ok(())
}

/// Where a run's speaker detection setting came from ([`choose_speaker_timing`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TimingSource {
    /// Asked for this run and saved as the setting.
    RequestedAndSaved,
    /// Asked for this run only.
    Requested,
    /// The stored setting.
    Stored,
    /// Nothing stored: the default.
    Default,
    /// The stored value is not known to this build (as stored): the default.
    UnknownStored(String),
}

/// The speaker detection setting for a run (T6.13d4): `requested` if given (stored as the
/// setting with `save`), else the setting stored in `store`, else the default.
///
/// # Errors
/// Store errors.
pub fn choose_speaker_timing(
    store: &mut Store,
    requested: Option<SpeakerTiming>,
    save: bool,
) -> Result<(SpeakerTiming, TimingSource), RuntimeError> {
    let stored = speaker_timing(store)?;
    Ok(match (requested, stored) {
        (Some(t), _) if save => {
            set_speaker_timing(store, t)?;
            (t, TimingSource::RequestedAndSaved)
        }
        (Some(t), _) => (t, TimingSource::Requested),
        (None, StoredTiming::Set(t)) => (t, TimingSource::Stored),
        (None, s @ StoredTiming::Absent) => (s.timing(), TimingSource::Default),
        (None, StoredTiming::Unknown(name)) => (
            StoredTiming::Unknown(name.clone()).timing(),
            TimingSource::UnknownStored(name),
        ),
    })
}

/// The live summaries setting for a run (T8.3j, [`crate::live_summaries`]): `requested` if
/// given (stored as the setting with `save`), else the stored setting, else off. The
/// source is reported as for [`choose_speaker_timing`].
///
/// # Errors
/// Store errors.
pub fn choose_live_summaries(
    store: &mut Store,
    requested: Option<bool>,
    save: bool,
) -> Result<(bool, TimingSource), RuntimeError> {
    use crate::live_summaries::{StoredSwitch, live_summaries_setting, set_live_summaries};
    let stored = live_summaries_setting(store)?;
    Ok(match (requested, stored) {
        (Some(on), _) if save => {
            set_live_summaries(store, on)?;
            (on, TimingSource::RequestedAndSaved)
        }
        (Some(on), _) => (on, TimingSource::Requested),
        (None, StoredSwitch::Set(on)) => (on, TimingSource::Stored),
        (None, s @ StoredSwitch::Absent) => (s.on(), TimingSource::Default),
        (None, StoredSwitch::Unknown(name)) => (false, TimingSource::UnknownStored(name)),
    })
}

/// Why the live summaries setting is not honoured without a local summary model: a live
/// session makes no connection, so only a local model summarises during the meeting.
pub const LIVE_SUMMARIES_NEED_LOCAL: &str =
    "summarising during the meeting needs a local summary model";

/// The map worker of a run with the live summaries setting on: the prepared local summary
/// model with the meeting's share of the cores ([`crate::live_summaries::live_summary_threads`],
/// beside live transcription on `order`'s first accelerator, two tracks).
///
/// # Errors
/// [`LIVE_SUMMARIES_NEED_LOCAL`] without a local summary model.
pub fn live_summaries_for(
    summary: Option<&PreparedSummary>,
    order: &[Accelerator],
) -> Result<crate::live_summaries::LiveSummaries, &'static str> {
    let service = summary
        .filter(|s| s.local.is_some())
        .map(|s| &s.service)
        .ok_or(LIVE_SUMMARIES_NEED_LOCAL)?;
    let cores = std::thread::available_parallelism().map_or(1, std::num::NonZeroUsize::get);
    let backend = order.first().map_or(BackendKind::Cpu, |a| a.backend());
    let live = 2 * crate::backends::live_whisper_threads(backend, cores, 2).get();
    let threads = crate::live_summaries::live_summary_threads(
        cores,
        live,
        earshot_llm_local::default_threads(),
    );
    let mapper = service
        .live_mapper(threads)
        .ok_or(LIVE_SUMMARIES_NEED_LOCAL)?;
    Ok(crate::live_summaries::LiveSummaries {
        mapper,
        background: true,
    })
}

/// What the user asked of the summary step.
#[derive(Debug, Clone, Default)]
pub struct SummaryRequest {
    /// A remote endpoint (OpenAI-compatible or Anthropic): used whenever given.
    pub remote: Option<SummaryConfig>,
    /// No summary at all (no model loaded, nothing sent).
    pub skip: bool,
    /// The local summary model by id; `None`: the one this machine's memory picks.
    pub local_model: Option<String>,
    /// The graphics card's dedicated memory, which that choice also respects (`None`:
    /// unknown, not held against a model).
    pub graphics_memory: Option<u64>,
    /// The endpoint needs an API key that could not be read: no summary, with this reason
    /// ([`SummaryPlan::NoApiKey`]); never a local model or another provider.
    pub no_key: Option<crate::summary_settings::ApiKeyError>,
    /// Claude through the user's Claude Code CLI (T7.4ga): used whenever given (no endpoint
    /// is then given).
    pub claude_code: Option<crate::claude_code::ClaudeCodeSummaryConfig>,
    /// OpenAI through the user's Codex CLI (T7.4gc): used whenever given (no endpoint and no
    /// Claude Code is then given).
    pub codex: Option<crate::codex::CodexSummaryConfig>,
    /// The meeting is kept on this computer and its writer would have sent it elsewhere
    /// (T12.13, [`summary_from_settings`]): only the local model runs; without it the plan is
    /// [`SummaryPlan::KeptLocalNoModel`]. Any endpoint, Claude Code, Codex or key problem in
    /// the request is ignored.
    pub kept_local: bool,
}

/// What the summary step will do (decided before the session starts).
#[derive(Debug)]
pub enum SummaryPlan {
    /// No summary was asked for.
    Skip,
    /// The configured endpoint.
    Remote(SummaryConfig),
    /// A local model that is installed.
    Local(SummaryModelChoice),
    /// The default local model is not installed: no summary, with this reason (it names
    /// the `cargo xtask fetch-models --set summary` command).
    NotInstalled(SummaryModelChoice, String),
    /// The chosen endpoint needs an API key that could not be read: no summary, with why.
    NoApiKey(crate::summary_settings::ApiKeyError),
    /// Claude through the user's Claude Code CLI (T7.4ga; run [`check_claude_code`] first).
    ClaudeCode(crate::claude_code::ClaudeCodeSummaryConfig),
    /// Claude Code was chosen but cannot run (not installed, not signed in): no summary,
    /// with why ([`check_claude_code`]); never another provider.
    ClaudeCodeUnavailable(earshot_llm::ClaudeCodeError),
    /// OpenAI through the user's Codex CLI (T7.4gc; run [`check_codex`] first).
    Codex(crate::codex::CodexSummaryConfig),
    /// Codex was chosen but cannot run (not installed, not signed in): no summary, with why
    /// ([`check_codex`]); never another provider.
    CodexUnavailable(earshot_llm::CodexError),
    /// The meeting is kept on this computer ([`SummaryRequest::kept_local`]) and its local
    /// summary model is not installed: no summary, never the remote writer.
    KeptLocalNoModel(SummaryModelChoice),
}

/// Why a [`SummaryRequest`] cannot be planned.
#[derive(Debug)]
pub enum SummaryPlanError {
    /// The requested local model is not a shipped summary model.
    UnknownModel(RuntimeError),
    /// The local model asked for by name is not installed.
    NotInstalled(RuntimeError),
}

/// The summary step of `request`: its endpoint when one is given; else, unless it skips
/// the summary, the requested local model or the one `memory` (reported physical bytes) and
/// `gpu` (whether it will run on the GPU, [`crate::summary_model::summary_gpu_planned`])
/// pick, if installed in `models`. A default model that is not installed is
/// [`SummaryPlan::NotInstalled`] (no summary, never a network endpoint).
///
/// # Errors
/// An unknown local model; a local model asked for by name that is not installed.
pub fn summary_plan(
    request: &SummaryRequest,
    memory: Option<u64>,
    gpu: bool,
    models: &ModelDir,
) -> Result<SummaryPlan, SummaryPlanError> {
    // A kept-local request never plans a remote writer, whatever else it carries (T12.13).
    if !request.kept_local {
        if let Some(e) = &request.no_key {
            return Ok(SummaryPlan::NoApiKey(e.clone()));
        }
        if let Some(config) = &request.remote {
            return Ok(SummaryPlan::Remote(config.clone()));
        }
        if let Some(config) = &request.claude_code {
            return Ok(SummaryPlan::ClaudeCode(config.clone()));
        }
        if let Some(config) = &request.codex {
            return Ok(SummaryPlan::Codex(config.clone()));
        }
    }
    if request.skip {
        return Ok(SummaryPlan::Skip);
    }
    let choice = choose_summary_model_on(
        request.local_model.as_deref(),
        memory,
        gpu,
        request.graphics_memory,
    )
    .map_err(SummaryPlanError::UnknownModel)?;
    match installed(models, choice.model) {
        Ok(_) => Ok(SummaryPlan::Local(choice)),
        Err(_) if request.kept_local => Ok(SummaryPlan::KeptLocalNoModel(choice)),
        Err(e) if request.local_model.is_some() => Err(SummaryPlanError::NotInstalled(e)),
        Err(e) => Ok(SummaryPlan::NotInstalled(choice, e.to_string())),
    }
}

/// A Claude Code `plan` checked with `probe` (T7.4ga): not installed or not signed in turns
/// it into [`SummaryPlan::ClaudeCodeUnavailable`] (a meeting then records and skips the
/// summary with the reason; an explicit re-summary fails with it); otherwise the found CLI is
/// kept in the plan. Any other plan is returned unchanged and `probe` is not asked. The probe
/// runs child processes: call this without the store lock.
#[must_use]
pub fn check_claude_code(
    plan: SummaryPlan,
    probe: &dyn crate::claude_code::ClaudeCodeProbe,
) -> SummaryPlan {
    let SummaryPlan::ClaudeCode(mut config) = plan else {
        return plan;
    };
    let status = probe.status();
    if let Some(problem) = status.problem() {
        return SummaryPlan::ClaudeCodeUnavailable(problem);
    }
    config.cli = status.cli().cloned();
    SummaryPlan::ClaudeCode(config)
}

/// A Codex `plan` checked with `probe` (T7.4gc), as [`check_claude_code`] does for Claude
/// Code: not installed or not signed in turns it into [`SummaryPlan::CodexUnavailable`];
/// otherwise the found CLI is kept in the plan. Any other plan is returned unchanged and
/// `probe` is not asked. The probe runs child processes: call this without the store lock.
#[must_use]
pub fn check_codex(plan: SummaryPlan, probe: &dyn crate::codex::CodexProbe) -> SummaryPlan {
    let SummaryPlan::Codex(mut config) = plan else {
        return plan;
    };
    let status = probe.status();
    if let Some(problem) = status.problem() {
        return SummaryPlan::CodexUnavailable(problem);
    }
    config.cli = status.cli().cloned();
    SummaryPlan::Codex(config)
}

/// llama.cpp's DLLs a local summary loads (`cargo xtask build` copies them next to the
/// exe): `llama.dll` (delay-loaded by `earshot-cli.exe`, see its build.rs) and the ggml DLLs it
/// imports.
pub const LLAMA_DLLS: &[&str] = &[
    "llama.dll",
    "ggml.dll",
    "ggml-base.dll",
    "ggml-cpu.dll",
    #[cfg(feature = "cuda")]
    "ggml-cuda.dll",
    #[cfg(feature = "vulkan-summaries")]
    "ggml-vulkan.dll",
];

/// The [`LLAMA_DLLS`] found in none of `dirs` (the exe's directory, then `PATH`); always
/// empty off Windows.
#[must_use]
pub fn missing_llama_dlls(dirs: &[PathBuf]) -> Vec<&'static str> {
    if !cfg!(windows) {
        return Vec::new();
    }
    LLAMA_DLLS
        .iter()
        .copied()
        .filter(|name| !dirs.iter().any(|d| d.join(name).is_file()))
        .collect()
}

/// Where Windows finds `llama.dll` and its DLLs for the running exe: its own directory, then
/// `PATH` (pass to [`prepare_summary`]; the one rule of the CLI and the app).
#[must_use]
pub fn dll_search_dirs() -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = std::env::current_exe()
        .ok()
        .and_then(|e| e.parent().map(PathBuf::from))
        .into_iter()
        .collect();
    if let Some(path) = std::env::var_os("PATH") {
        dirs.extend(std::env::split_paths(&path));
    }
    dirs
}

/// Name prefixes of the CUDA runtime DLLs a `cuda` build's `ggml-cuda.dll` loads
/// (`dumpbin /DEPENDENTS`: `ggml-cuda.dll` imports `cublas64_<major>.dll`, which imports
/// `cublasLt64_<major>.dll`; cudart is linked statically). Without them a local summary
/// would crash in `supports_gpu()`, the first llama call. The major version follows the CUDA
/// toolkit of the build, so only the prefix is checked.
pub const CUDA_RUNTIME_DLL_PREFIXES: &[&str] = &[
    #[cfg(feature = "cuda")]
    "cublas64_",
    #[cfg(feature = "cuda")]
    "cublasLt64_",
];

/// The `prefixes` with no `<prefix>*.dll` file (ASCII case ignored, as Windows does) in any
/// of `dirs`, each reported as `<prefix>*.dll`; always empty off Windows.
#[must_use]
pub fn missing_dll_prefixes(dirs: &[PathBuf], prefixes: &[&str]) -> Vec<String> {
    if !cfg!(windows) {
        return Vec::new();
    }
    let has = |dir: &PathBuf, prefix: &str| {
        let prefix = prefix.to_ascii_lowercase();
        std::fs::read_dir(dir).is_ok_and(|entries| {
            entries.flatten().any(|e| {
                let name = e.file_name().to_string_lossy().to_ascii_lowercase();
                name.starts_with(&prefix)
                    && name.ends_with(".dll")
                    && e.file_type().is_ok_and(|t| t.is_file())
            })
        })
    };
    prefixes
        .iter()
        .filter(|p| !dirs.iter().any(|d| has(d, p)))
        .map(|p| format!("{p}*.dll"))
        .collect()
}

/// A summary service ready for [`after_meeting`].
pub struct PreparedSummary {
    /// The service.
    pub service: SummaryService,
    /// For a local model: the model and whether the GPU is requested.
    pub local: Option<(SummaryModelChoice, bool)>,
}

impl std::fmt::Debug for PreparedSummary {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PreparedSummary")
            .field("model", &self.service.model())
            .field("local", &self.local)
            .finish_non_exhaustive()
    }
}

/// Why [`prepare_summary`] failed.
#[derive(Debug)]
pub enum SummarySetupError {
    /// llama.cpp's DLLs ([`LLAMA_DLLS`]) are not where the host said the exe finds them.
    LlamaDllsMissing(Vec<String>),
    /// A `cuda` build's CUDA runtime DLLs ([`CUDA_RUNTIME_DLL_PREFIXES`]) are not found.
    CudaDllsMissing(Vec<String>),
    /// The service could not be built (endpoint, local model configuration).
    Runtime(RuntimeError),
}

/// The summary service of `plan` (`None` for a plan that runs no summary). For a local
/// model, `dll_dirs` are the directories the host's exe loads DLLs from (its own directory,
/// then `PATH`): llama.cpp's DLLs must be there before any llama.cpp call, the first of
/// which ([`summary_gpu_here`]) decides whether the GPU is requested.
///
/// # Errors
/// [`SummarySetupError`].
pub fn prepare_summary(
    plan: &SummaryPlan,
    models: &ModelDir,
    clock: SharedClock,
    dll_dirs: &[PathBuf],
) -> Result<Option<PreparedSummary>, SummarySetupError> {
    prepare_summary_on(plan, models, clock, dll_dirs, true)
}

/// [`prepare_summary`] with the graphics card allowed only when `allow_gpu` (the Models page
/// "where it runs" setting, [`local_summary_placement`]); without it a local model runs on the
/// processor whatever [`summary_gpu_here`] says.
///
/// # Errors
/// [`SummarySetupError`].
pub fn prepare_summary_on(
    plan: &SummaryPlan,
    models: &ModelDir,
    clock: SharedClock,
    dll_dirs: &[PathBuf],
    allow_gpu: bool,
) -> Result<Option<PreparedSummary>, SummarySetupError> {
    match plan {
        SummaryPlan::Skip
        | SummaryPlan::NotInstalled(..)
        | SummaryPlan::NoApiKey(_)
        | SummaryPlan::ClaudeCodeUnavailable(_)
        | SummaryPlan::CodexUnavailable(_)
        | SummaryPlan::KeptLocalNoModel(_) => Ok(None),
        SummaryPlan::Remote(config) => Ok(Some(PreparedSummary {
            service: SummaryService::new(config, clock).map_err(SummarySetupError::Runtime)?,
            local: None,
        })),
        SummaryPlan::ClaudeCode(config) => Ok(Some(PreparedSummary {
            service: SummaryService::claude_code(config, clock)
                .map_err(SummarySetupError::Runtime)?,
            local: None,
        })),
        SummaryPlan::Codex(config) => Ok(Some(PreparedSummary {
            service: SummaryService::codex(config, clock).map_err(SummarySetupError::Runtime)?,
            local: None,
        })),
        SummaryPlan::Local(choice) => {
            let missing: Vec<String> = missing_llama_dlls(dll_dirs)
                .into_iter()
                .map(String::from)
                .collect();
            if !missing.is_empty() {
                return Err(SummarySetupError::LlamaDllsMissing(missing));
            }
            let missing = missing_dll_prefixes(dll_dirs, CUDA_RUNTIME_DLL_PREFIXES);
            if !missing.is_empty() {
                return Err(SummarySetupError::CudaDllsMissing(missing));
            }
            let gpu = summary_gpu_while_preparing(allow_gpu);
            let mut config =
                local_summary_config(models, choice, gpu).map_err(SummarySetupError::Runtime)?;
            config.refuse_gpus = crate::model_settings::integrated_gpu_names(&crate::hardware());
            Ok(Some(PreparedSummary {
                service: SummaryService::local(&config).map_err(SummarySetupError::Runtime)?,
                local: Some((*choice, gpu)),
            }))
        }
    }
}

/// Initialises ONNX Runtime for a run of `model` on `order`: the CUDA (or QNN) library only
/// for a Parakeet model that may run there ([`onnx_library`]), else the CPU library. If that
/// library cannot be loaded, `on_fallback` hears why and the CPU library is loaded instead
/// (Parakeet then refuses the accelerator and runs on the CPU, reported). Returns the loaded
/// library's path.
///
/// # Errors
/// The CPU library cannot be loaded.
pub fn init_onnx_for(
    models: &ModelDir,
    model: &str,
    order: &[Accelerator],
    on_fallback: &mut dyn FnMut(Accelerator, &RuntimeError),
) -> Result<PathBuf, RuntimeError> {
    let library = onnx_library(model, order);
    match init_onnx(models, library) {
        Err(e) if library != Accelerator::Cpu => {
            fell_back(library);
            on_fallback(library, &e);
            init_onnx(models, Accelerator::Cpu)
        }
        r => r,
    }
}

/// Keeps a library that did not load out of the rest of the process, so the warning just heard is
/// its only one: the NPU's here (T8.4f); CUDA's is recorded by `init_onnx` itself.
fn fell_back(library: Accelerator) {
    if library == Accelerator::Qnn {
        crate::backends::record_qnn_load_failure();
    }
}

/// Live transcription for a session: `model` on the best verified accelerator of `order`
/// per track ([`EvidenceLoader::for_live`] for at most two transcribed tracks) with Silero
/// VAD, and, when `speakers` (the post pass will run), the speaker windows computed during
/// the meeting with the post pass's window settings (T6.13d2).
pub fn live_factory(
    models: &ModelDir,
    model: &str,
    order: &[Accelerator],
    clock: SharedClock,
    speakers: bool,
) -> LiveAsrFactory {
    let loader = Arc::new(EvidenceLoader::for_live(2));
    let factory = live_asr_factory(
        loader,
        models.clone(),
        model.to_owned(),
        order.to_vec(),
        ONNX_ACCELERATOR,
        clock,
        os_seeded_rng(),
    );
    if speakers {
        with_live_speakers(
            factory,
            models.clone(),
            ONNX_ACCELERATOR,
            PostConfig::default().diarization,
        )
    } else {
        factory
    }
}

/// Provisional words for a meeting's session (T9.1m): the model's own session, loaded on the preview
/// thread for the backend the track turned out to run on. Whether and where this computer has them is
/// decided there (`backends::preview_backend`: the track's backend, or the CPU beside a graphics card
/// too small for them, T12.3c), and never costs the meeting anything. With
/// [`PreviewChoice::Everyone`](crate::preview_settings::PreviewChoice::Everyone) the user's own
/// track in a call gets a second session of its own, where `backends::second_preview_backend`
/// says (T9.14; the CPU, also beside CUDA: T12.3a).
#[must_use]
pub fn live_previews(
    models: &ModelDir,
    choice: crate::preview_settings::PreviewChoice,
) -> crate::live::LivePreviews {
    let first = models.clone();
    let mut previews = crate::live::LivePreviews::new(Arc::new(move |backend| {
        crate::backends::load_preview(&first, backend, choice)
    }));
    if choice == crate::preview_settings::PreviewChoice::Everyone {
        let second = models.clone();
        previews.own_voice = Some(Arc::new(move |backend| {
            crate::backends::load_preview_second(&second, backend, choice)
        }));
    }
    previews
}

/// Starts a live session from a capture source (the source differs between hosts and
/// capture modes).
pub type Starter = Box<
    dyn FnOnce(
        Services,
        LiveConfig,
        Option<LiveAsrFactory>,
        BoxRng,
    ) -> Result<LiveSession, RuntimeError>,
>;

/// A [`Starter`] for live capture on Windows: the default microphone (`mic_role`) and, for
/// a call, the loopback source of `mode`; in the room the meeting has one (mic) track
/// ([`crate::backends::start_wasapi_session`]).
#[cfg(windows)]
pub fn wasapi_starter(
    mode: earshot_capture_wasapi::CaptureMode,
    mic_role: earshot_capture_wasapi::MicRole,
) -> Starter {
    wasapi_starter_on(
        mode,
        mic_role,
        earshot_capture_wasapi::DeviceSelection::default(),
    )
}

/// [`wasapi_starter`] on the endpoints of `devices` (T7.4i: the Audio page's choice,
/// [`crate::audio_settings::AudioSettings::devices`]); a chosen endpoint that cannot be
/// opened is replaced by the default with a start warning
/// ([`crate::backends::start_wasapi_session_on`]).
#[cfg(windows)]
pub fn wasapi_starter_on(
    mode: earshot_capture_wasapi::CaptureMode,
    mic_role: earshot_capture_wasapi::MicRole,
    devices: earshot_capture_wasapi::DeviceSelection,
) -> Starter {
    Box::new(move |svc, cfg, asr, rng| {
        crate::backends::start_wasapi_session_on(svc, cfg, mic_role, &mode, &devices, asr, rng)
    })
}

/// The Audio page settings a meeting starts with (T7.4i: the microphone and output, for
/// [`wasapi_starter_on`] through [`crate::audio_settings::AudioSettings::devices`]) and one
/// note per stored value this version cannot use (its default is used).
///
/// # Errors
/// Store errors.
pub fn audio_setup(
    store: &Store,
) -> Result<(crate::audio_settings::AudioSettings, Vec<String>), RuntimeError> {
    let (settings, unknown) = crate::audio_settings::audio_settings(store)?;
    Ok((settings, unknown.iter().map(ToString::to_string).collect()))
}

/// Starts the meeting: `start` with `config` and the live transcription `factory`.
///
/// # Errors
/// As the starter (capture, models, store); nothing is recorded on an error.
pub fn start_meeting(
    services: &Services,
    config: LiveConfig,
    start: Starter,
    factory: LiveAsrFactory,
) -> Result<LiveSession, RuntimeError> {
    start(services.clone(), config, Some(factory), os_seeded_rng())
}

/// Progress of [`after_meeting`], in order.
#[derive(Debug)]
pub enum HostStep<'a> {
    /// The recorded tracks were loaded; these final-pass tracks have no recording.
    PostLoaded {
        /// Tracks without a recording.
        missing: &'a [TrackId],
    },
    /// The final pass is about to run (models loaded). The post cancel token is live from
    /// here on.
    PostStarting {
        /// Better backends refused for the final-pass model, with the reason.
        fallbacks: &'a [(BackendKind, AsrError)],
        /// What is left to transcribe and diarise per track (for an estimate).
        work: &'a [CleanupWork],
        /// The backend the final-pass model was verified on at load.
        asr_backend: BackendKind,
    },
    /// A final-pass step finished.
    Post(PostStage),
    /// The post pass is over (whatever its result); nothing runs between this and the
    /// summary.
    PostFinished(&'a Result<HostPost, RuntimeError>),
    /// The summary is about to run.
    SummaryStarting,
}

/// What the post pass did.
#[derive(Debug)]
pub enum HostPost {
    /// Not asked for ([`AfterMeeting::post`] off).
    Skipped,
    /// No track had a recording: nothing to process.
    NothingToProcess,
    /// Cancelled through [`AfterMeeting::post_cancel`]: the recordings and the live
    /// transcript are kept; the speaker windows of the meeting were deleted (how many
    /// tracks' records, or why not, [`cleanup_cancelled`]).
    Cancelled {
        /// Track records deleted, or the deletion's error.
        speaker_windows_deleted: Result<usize, RuntimeError>,
    },
    /// The final pass ran and stored the final transcript.
    Done(Box<PostOutcome>),
}

/// Why no summary was made.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkipReason {
    /// The plan runs none ([`SummaryPlan::Skip`]).
    NotRequested,
    /// The default local model is not installed ([`SummaryPlan::NotInstalled`]).
    NotInstalled {
        /// Its id.
        model: &'static str,
        /// Why (names the fetch command).
        why: String,
    },
    /// The local model chosen by name is not installed ([`SummaryPlanError::NotInstalled`]): a
    /// meeting records and skips the summary; an explicit re-summary fails.
    ChosenNotInstalled {
        /// Its id.
        model: &'static str,
    },
    /// The post pass failed, so the run stopped there.
    PostFailed,
    /// The chosen endpoint needs an API key that could not be read
    /// ([`SummaryPlan::NoApiKey`]).
    NoApiKey(crate::summary_settings::ApiKeyError),
    /// Claude Code was chosen but is not installed or not signed in
    /// ([`SummaryPlan::ClaudeCodeUnavailable`]).
    ClaudeCodeUnavailable(earshot_llm::ClaudeCodeError),
    /// Codex was chosen but is not installed or not signed in
    /// ([`SummaryPlan::CodexUnavailable`]).
    CodexUnavailable(earshot_llm::CodexError),
    /// The meeting is kept on this computer and no local summary model is installed
    /// ([`SummaryPlan::KeptLocalNoModel`]); the remote writer is never used instead.
    KeptLocalNoModel {
        /// The local model it would have used.
        model: &'static str,
    },
}

/// Where a stored summary was made.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SummaryBackend {
    /// The configured endpoint (its requests are in [`HostOutcome::network`]).
    Remote,
    /// Claude through the user's Claude Code (its runs are in [`HostOutcome::handovers`]).
    ClaudeCode,
    /// OpenAI through the user's Codex (its runs are in [`HostOutcome::handovers`]).
    Codex,
    /// On this machine, where llama.cpp put the model (`LlamaBackend::name`: "cpu", "cuda", ...).
    Local(String),
}

/// What the summary step did.
#[derive(Debug)]
pub enum SummaryResult {
    /// A summary was stored.
    Stored {
        /// The stored `Summary` row.
        summary_id: EntityId,
        /// The model that made it.
        model: String,
        /// Where it ran.
        backend: SummaryBackend,
        /// The meeting had no title and now carries the summary's.
        title_set: bool,
        /// The whole outcome (the summary, the run's report).
        outcome: Box<SummaryOutcome>,
    },
    /// No summary was made.
    Skipped {
        /// Why.
        reason: SkipReason,
    },
    /// The summary failed; nothing was stored.
    Failed {
        /// Why.
        error: RuntimeError,
        /// Whether retrying can succeed without user action ([`RuntimeError::is_retryable`]).
        retryable: bool,
    },
    /// Cancelled through [`AfterMeeting::summary_cancel`]; nothing was stored.
    Cancelled,
}

/// Result of [`after_meeting`].
#[derive(Debug)]
pub struct HostOutcome {
    /// Final-pass tracks without a recording.
    pub missing_tracks: Vec<TrackId>,
    /// The post pass; an error stops the run before the summary.
    pub post: Result<HostPost, RuntimeError>,
    /// The summary.
    pub summary: SummaryResult,
    /// Every network request the summary made (the privacy record; empty for a local
    /// summary or none).
    pub network: Vec<AuditEntry>,
    /// Every time the summary handed text to Claude Code or Codex (the privacy record of
    /// PLAN.md 3.9's exception: when, how many characters, which CLI, never the text; empty for
    /// any other summary).
    pub handovers: Vec<crate::claude_code::HandoverEntry>,
    /// Why storing a handover in the meeting's local record failed (store error text, never
    /// meeting text); the summary's own result is unaffected. Empty when all were stored.
    pub handover_record_failures: Vec<String>,
    /// A stored summary was written by Claude Code at its own default effort although one was
    /// chosen, because its help could not be read ([`claude_effort_unread`], T12.20); false
    /// for any summary that was not stored.
    pub claude_effort_unread: bool,
}

impl HostOutcome {
    /// The outcome of a run that could not start: `error` stops it before the summary.
    #[must_use]
    pub fn not_started(error: RuntimeError) -> Self {
        Self {
            missing_tracks: Vec::new(),
            post: Err(error),
            summary: SummaryResult::Skipped {
                reason: SkipReason::PostFailed,
            },
            network: Vec::new(),
            handovers: Vec::new(),
            handover_record_failures: Vec::new(),
            claude_effort_unread: false,
        }
    }

    /// The outcome of a run cancelled before it started, which takes no model and writes nothing:
    /// the same as a final pass and a summary cancelled at their first look at the token.
    #[must_use]
    pub fn cancelled(services: &Services, meeting: EntityId) -> Self {
        Self {
            missing_tracks: Vec::new(),
            post: Ok(HostPost::Cancelled {
                speaker_windows_deleted: cleanup_cancelled(services, meeting),
            }),
            summary: SummaryResult::Cancelled,
            network: Vec::new(),
            handovers: Vec::new(),
            handover_record_failures: Vec::new(),
            claude_effort_unread: false,
        }
    }
}

/// How [`after_meeting`] runs.
pub struct AfterMeeting<'a> {
    /// Where the models are.
    pub models: &'a ModelDir,
    /// The transcription model (the live pass's).
    pub model: &'a str,
    /// Accelerators to try for the final pass, best first.
    pub order: &'a [Accelerator],
    /// Whether the post pass runs.
    pub post: bool,
    /// Transcribe and diarise every track whole instead of finishing from the live
    /// stretches and speaker windows (for comparison; T6.13).
    pub whole_pass: bool,
    /// The summary service (`None`: the plan runs no summary).
    pub summary: Option<&'a SummaryService>,
    /// What [`SummaryResult::Skipped`] says when `summary` is `None`.
    pub no_summary: SkipReason,
    /// Cancels the post pass.
    pub post_cancel: &'a CancelToken,
    /// Cancels the summary.
    pub summary_cancel: &'a CancelToken,
}

/// The skip reason of a plan that runs no summary (`None` for one that runs a summary).
#[must_use]
pub fn skip_reason(plan: &SummaryPlan) -> Option<SkipReason> {
    match plan {
        SummaryPlan::Skip => Some(SkipReason::NotRequested),
        SummaryPlan::NotInstalled(choice, why) => Some(SkipReason::NotInstalled {
            model: choice.model.id,
            why: why.clone(),
        }),
        SummaryPlan::NoApiKey(e) => Some(SkipReason::NoApiKey(e.clone())),
        SummaryPlan::ClaudeCodeUnavailable(e) => Some(SkipReason::ClaudeCodeUnavailable(e.clone())),
        SummaryPlan::CodexUnavailable(e) => Some(SkipReason::CodexUnavailable(e.clone())),
        SummaryPlan::KeptLocalNoModel(choice) => Some(SkipReason::KeptLocalNoModel {
            model: choice.model.id,
        }),
        SummaryPlan::Remote(_)
        | SummaryPlan::Local(_)
        | SummaryPlan::ClaudeCode(_)
        | SummaryPlan::Codex(_) => None,
    }
}

/// After the session ended (module docs, step 5): the final pass of `outcome` with the
/// production models ([`post_processor`]), then the summary. `progress` hears every
/// [`HostStep`]. The meeting is leased as [`MeetingJob::Processing`] throughout.
pub fn after_meeting(
    services: &Services,
    outcome: &LiveOutcome,
    run: &AfterMeeting<'_>,
    progress: &mut dyn FnMut(HostStep<'_>),
) -> HostOutcome {
    let load = || {
        let setup = post_processor(
            &EvidenceLoader::default(),
            run.models,
            run.model,
            run.order,
            ONNX_ACCELERATOR,
            PostConfig {
                reuse_live: !run.whole_pass,
                ..PostConfig::default()
            },
        )?;
        Ok(PostLoad {
            processor: setup.processor,
            asr_backend: setup.asr_backend,
            fallbacks: setup.fallbacks,
        })
    };
    let done = after_meeting_with(services, outcome, run, load, progress);
    // The final pass's own NPU load, so it is kept even if the app closes after this meeting.
    store_npu_refusals(services);
    done
}

/// A loaded final-pass processor and how its model was verified.
pub struct PostLoad<T, S, E> {
    /// The processor.
    pub processor: PostProcessor<T, S, E>,
    /// The backend the final-pass model was verified on at load.
    pub asr_backend: BackendKind,
    /// Better backends refused for it.
    pub fallbacks: Vec<(BackendKind, AsrError)>,
}

/// [`after_meeting`] with the final-pass processor from `load` (called only when the post
/// pass runs and some track has a recording).
pub fn after_meeting_with<T, S, E>(
    services: &Services,
    outcome: &LiveOutcome,
    run: &AfterMeeting<'_>,
    load: impl FnOnce() -> Result<PostLoad<T, S, E>, RuntimeError>,
    progress: &mut dyn FnMut(HostStep<'_>),
) -> HostOutcome
where
    T: Transcriber + Send,
    S: SpeakerSegmenter + Send,
    E: SpeakerEmbedder + Send,
{
    let meeting = outcome.meeting;
    // The meeting's previews have loaded or given up by now.
    store_npu_refusals(services);
    // Refused while the meeting is being deleted: nothing runs, no step is reported.
    let _lease = match services.lease_meeting_for(meeting, MeetingJob::Processing) {
        Ok(lease) => lease,
        Err(e) => return HostOutcome::not_started(e),
    };
    let mut missing_tracks = Vec::new();
    let post = if run.post {
        run_post(services, outcome, run, load, &mut missing_tracks, progress)
    } else {
        Ok(HostPost::Skipped)
    };
    progress(HostStep::PostFinished(&post));
    let (summary, network, handovers, handover_record_failures) = if post.is_err() {
        (
            SummaryResult::Skipped {
                reason: SkipReason::PostFailed,
            },
            Vec::new(),
            Vec::new(),
            Vec::new(),
        )
    } else {
        match run.summary {
            // Cancelled already: nothing to load or send, whatever the transcript holds.
            Some(_) if run.summary_cancel.is_cancelled() => {
                (SummaryResult::Cancelled, Vec::new(), Vec::new(), Vec::new())
            }
            Some(service) => {
                progress(HostStep::SummaryStarting);
                let result = run_summary(services, meeting, service, run.summary_cancel);
                (
                    result,
                    service.audit_log().entries(),
                    service.handovers(),
                    service.handover_record_failures(),
                )
            }
            None => (
                SummaryResult::Skipped {
                    reason: run.no_summary.clone(),
                },
                Vec::new(),
                Vec::new(),
                Vec::new(),
            ),
        }
    };
    let claude_effort_unread = run
        .summary
        .is_some_and(|s| claude_effort_unread(s, &summary));
    HostOutcome {
        missing_tracks,
        post,
        summary,
        network,
        handovers,
        handover_record_failures,
        claude_effort_unread,
    }
}

/// Whether to say that `service`'s Claude Code wrote the summary of `result` at its own default
/// effort (T12.20): only for a stored summary whose run found the help unreadable. The rule of
/// [`HostOutcome::claude_effort_unread`] and of the app's re-summary job.
#[must_use]
pub fn claude_effort_unread(service: &SummaryService, result: &SummaryResult) -> bool {
    matches!(result, SummaryResult::Stored { .. }) && service.claude_effort_unread()
}

fn run_post<T, S, E>(
    services: &Services,
    outcome: &LiveOutcome,
    run: &AfterMeeting<'_>,
    load: impl FnOnce() -> Result<PostLoad<T, S, E>, RuntimeError>,
    missing_tracks: &mut Vec<TrackId>,
    progress: &mut dyn FnMut(HostStep<'_>),
) -> Result<HostPost, RuntimeError>
where
    T: Transcriber + Send,
    S: SpeakerSegmenter + Send,
    E: SpeakerEmbedder + Send,
{
    final_pass(
        services,
        outcome.meeting,
        &outcome.final_pass,
        run.post_cancel,
        load,
        missing_tracks,
        progress,
    )
}

/// The final pass of `request` on `meeting` with the processor from `load` (called only when
/// some track has a recording), reporting [`HostStep`]s up to the last [`PostStage`]; shared
/// by [`after_meeting`] and [`crate::edit::retranscribe`]. A cancelled pass deletes the
/// meeting's live speaker windows ([`cleanup_cancelled`]).
pub(crate) fn final_pass<T, S, E>(
    services: &Services,
    meeting: EntityId,
    request: &earshot_core::pipeline::FinalPassRequest,
    cancel: &CancelToken,
    load: impl FnOnce() -> Result<PostLoad<T, S, E>, RuntimeError>,
    missing_tracks: &mut Vec<TrackId>,
    progress: &mut dyn FnMut(HostStep<'_>),
) -> Result<HostPost, RuntimeError>
where
    T: Transcriber + Send,
    S: SpeakerSegmenter + Send,
    E: SpeakerEmbedder + Send,
{
    let (input, missing): (PostInput, Vec<TrackId>) = load_post_input(services, meeting, request)?;
    *missing_tracks = missing;
    progress(HostStep::PostLoaded {
        missing: missing_tracks,
    });
    if input.tracks.is_empty() {
        return Ok(HostPost::NothingToProcess);
    }
    if cancel.is_cancelled() {
        return Ok(HostPost::Cancelled {
            speaker_windows_deleted: cleanup_cancelled(services, meeting),
        });
    }
    let mut setup = load()?;
    let work = setup.processor.cleanup_work(&input);
    progress(HostStep::PostStarting {
        fallbacks: &setup.fallbacks,
        work: &work,
        asr_backend: setup.asr_backend,
    });
    let result = setup.processor.run_reporting(
        services,
        input,
        &mut *os_seeded_rng(),
        cancel,
        &mut |stage| progress(HostStep::Post(stage)),
    );
    match result {
        Ok(o) => Ok(HostPost::Done(Box::new(o))),
        Err(e) if is_cancelled_pass(&e, cancel) => Ok(HostPost::Cancelled {
            speaker_windows_deleted: cleanup_cancelled(services, meeting),
        }),
        Err(e) => Err(e),
    }
}

/// Whether `e` is the pass answering `cancel`: from either model (the echo check's embedder
/// answers as a diarisation error), and never a failure that happens to follow a cancel.
fn is_cancelled_pass(e: &RuntimeError, cancel: &CancelToken) -> bool {
    cancel.is_cancelled() && crate::post::is_cancellation(e)
}

/// The post pass was cancelled: no final pass will finish from the speaker windows computed
/// during the meeting (speaker embeddings, biometric data), so they are deleted at once
/// ([`crate::speakers::forget_speakers`]); the recordings and the live transcript are kept.
/// Returns the number of tracks whose records were deleted.
///
/// # Errors
/// Store errors.
pub fn cleanup_cancelled(services: &Services, meeting: EntityId) -> Result<usize, RuntimeError> {
    crate::speakers::forget_speakers(&mut services.store(), meeting)
}

/// Runs `service` on `meeting` ([`SummaryService::summarize`]) and says what happened.
#[must_use]
pub fn run_summary(
    services: &Services,
    meeting: EntityId,
    service: &SummaryService,
    cancel: &CancelToken,
) -> SummaryResult {
    match service.summarize(services, meeting, &mut *os_seeded_rng(), cancel) {
        Ok(outcome) => SummaryResult::Stored {
            summary_id: outcome.summary_id,
            model: service.model().to_owned(),
            backend: match &outcome.report {
                SummaryRunReport::Remote(_) => SummaryBackend::Remote,
                SummaryRunReport::ClaudeCode(_) => SummaryBackend::ClaudeCode,
                SummaryRunReport::Codex(_) => SummaryBackend::Codex,
                SummaryRunReport::Local(r) => SummaryBackend::Local(r.backend.name().to_owned()),
            },
            title_set: outcome.title_set,
            outcome: Box::new(outcome),
        },
        // Only the cancellation itself; any other failure stays one whatever the token says.
        Err(
            RuntimeError::Llm(earshot_llm::LlmError::Cancelled)
            | RuntimeError::LocalSummary(crate::summary::LocalSummaryError::Run(
                earshot_llm_local::summarize::SummaryFailure::Cancelled,
            )),
        ) => SummaryResult::Cancelled,
        Err(error) => SummaryResult::Failed {
            retryable: error.is_retryable(),
            error,
        },
    }
}

#[cfg(test)]
#[path = "host_tests.rs"]
pub(crate) mod tests;
