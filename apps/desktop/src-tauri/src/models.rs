//! What the Models page reads and changes (T7.4e): the hardware facts, the catalogue with
//! install state, where each model would run and what the stored check found, the
//! recommendation, the model settings, and removal. Every decision comes from
//! `earshot_runtime` (`host`, `recommend`, `model_settings`, `catalogue`); this module only
//! shapes it for the UI.

use crate::plain_text::plain;
use earshot_core::asr::BackendKind;
use earshot_runtime::Services;
use earshot_runtime::backends::{Accelerator, ModelDir, accelerator_order};
use earshot_runtime::catalogue::{Catalogue, CatalogueError, InstallState, ModelKind};
use earshot_runtime::host::{self, TranscriptionSource};
use earshot_runtime::model_bench::{StoredCheck, check_fingerprint, hardware_facts, stored_check};
use earshot_runtime::model_settings::{
    ModelPick, ModelSettings, Part, SettingsError, model_settings, set_model_settings,
};
use earshot_runtime::recommend::{self, CheckVerdict, ChoiceRole};
use serde::{Deserialize, Serialize};
use specta::Type;

use crate::ipc::IpcError;
use crate::model_jobs::ModelJobs;

/// A part of the computer a model runs on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Type)]
pub enum RunsOn {
    /// The processor.
    Processor,
    /// An NVIDIA graphics card (CUDA).
    Cuda,
    /// A graphics card through Vulkan.
    Vulkan,
    /// The NPU.
    Npu,
}

impl RunsOn {
    /// The part of a backend (a test backend reads as the processor).
    #[must_use]
    pub fn of(backend: BackendKind) -> Self {
        match backend {
            BackendKind::Cuda => Self::Cuda,
            BackendKind::Vulkan => Self::Vulkan,
            BackendKind::Qnn => Self::Npu,
            BackendKind::Cpu | BackendKind::Fake => Self::Processor,
        }
    }

    fn of_accelerator(a: Accelerator) -> Self {
        Self::of(a.backend())
    }
}

/// What a check found for a model where it would run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Type)]
pub enum Verdict {
    /// It keeps up.
    KeepsUp,
    /// Too slow here (still selectable).
    TooSlow,
    /// Wrong output or it did not run there.
    Fails,
    /// No current check tested it.
    Unknown,
}

impl Verdict {
    /// From the recommendation's verdict.
    #[must_use]
    pub fn of(v: CheckVerdict) -> Self {
        match v {
            CheckVerdict::KeepsUp => Self::KeepsUp,
            CheckVerdict::TooSlow => Self::TooSlow,
            CheckVerdict::Fails => Self::Fails,
            CheckVerdict::Unknown => Self::Unknown,
        }
    }

    /// From a trial's verdict.
    #[must_use]
    pub fn of_trial(v: earshot_runtime::model_check::Verdict) -> Self {
        use earshot_runtime::model_check::Verdict as V;
        match v {
            V::Pass { .. } => Self::KeepsUp,
            V::TooSlow => Self::TooSlow,
            V::Wrong => Self::Fails,
        }
    }
}

/// A display adapter.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Type)]
pub struct GpuView {
    /// Its exact name.
    pub name: String,
    /// Dedicated video memory (bytes), when known.
    pub dedicated_memory: Option<u64>,
    /// Driver version, when known.
    pub driver_version: Option<String>,
    /// Integrated (shares system memory), when known.
    pub integrated: Option<bool>,
}

/// An NPU.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Type)]
pub struct NpuView {
    /// Its exact name.
    pub name: String,
    /// Driver version, when known.
    pub driver_version: Option<String>,
}

/// "This is your hardware": the facts as read (a missing fact is `null`, never a guess) and
/// the parts this build can run models on, best first.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Type)]
pub struct HardwareView {
    /// The processor's brand string.
    pub processor: Option<String>,
    /// Physical cores.
    pub physical_cores: Option<u32>,
    /// Logical cores.
    pub logical_cores: Option<u32>,
    /// Display adapters.
    pub gpus: Vec<GpuView>,
    /// NPUs.
    pub npus: Vec<NpuView>,
    /// Physical memory as Windows reports it (bytes).
    pub total_memory: Option<u64>,
    /// The build's accelerator order (the processor last).
    pub accelerators: Vec<RunsOn>,
}

fn count(n: Option<usize>) -> Option<u32> {
    n.map(|n| u32::try_from(n).unwrap_or(u32::MAX))
}

/// The hardware view of `hw` on a build with accelerator `order`.
#[must_use]
pub fn hardware_view(
    hw: &earshot_runtime::earshot_hardware::Hardware,
    order: &[Accelerator],
) -> HardwareView {
    HardwareView {
        processor: hw.cpu.name.clone(),
        physical_cores: count(hw.cpu.physical_cores),
        logical_cores: count(hw.cpu.logical_cores),
        gpus: hw
            .gpus
            .iter()
            .map(|g| GpuView {
                name: g.name.clone(),
                dedicated_memory: g.dedicated_memory,
                driver_version: g.driver_version.clone(),
                integrated: g.integrated,
            })
            .collect(),
        npus: hw
            .npus
            .iter()
            .map(|n| NpuView {
                name: n.name.clone(),
                driver_version: n.driver_version.clone(),
            })
            .collect(),
        total_memory: hw.total_memory,
        accelerators: order.iter().map(|a| RunsOn::of_accelerator(*a)).collect(),
    }
}

/// Transcription or summaries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Type)]
pub enum ModelUse {
    /// Speech to text.
    Transcription,
    /// A local summary model.
    Summary,
    /// What every meeting needs (the speech helpers).
    Helpers,
    /// The NPU version of a transcription model (ARM64 only): never chosen on its own.
    NpuVersion,
}

/// A model's files on this computer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Type)]
pub enum Installed {
    /// Every file is there.
    Yes,
    /// Some bytes are held (a paused or cut download).
    Partly,
    /// Nothing is held.
    No,
}

/// One model of the catalogue.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Type)]
pub struct ModelItem {
    /// Its name (the manifest id: what the commands take).
    pub name: String,
    /// The name the user sees (`earshot_runtime::catalogue::display_name`); every text the
    /// page shows names models this way.
    pub title: String,
    /// What it is for.
    pub kind: ModelUse,
    /// Download size (bytes).
    pub size: u64,
    /// Language codes it transcribes (empty for a summary model).
    pub languages: Vec<String>,
    /// Install state.
    pub installed: Installed,
    /// Bytes held towards the download.
    pub held: u64,
    /// Where it would run here with "where it runs" on automatic.
    pub runs_on: RunsOn,
    /// What the stored check found for it there.
    pub check: Verdict,
    /// It is chosen or otherwise kept (Remove is refused).
    pub protected: bool,
    /// It is downloading now.
    pub downloading: bool,
}

/// Which of the four transcription choices.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Type)]
pub enum ChoiceKind {
    /// The recommended model.
    Recommended,
    /// The next rung (older or busy machines, less memory).
    Lighter,
    /// Meetings in European languages.
    European,
    /// Meetings in any language.
    EveryLanguage,
}

/// One transcription choice (the model's details are in `ModelsView::transcription`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Type)]
pub struct ChoiceView {
    /// Which choice.
    pub role: ChoiceKind,
    /// The model.
    pub model: String,
}

/// A recommendation and why.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Type)]
pub struct RecommendedView {
    /// The model.
    pub model: String,
    /// Where it would run.
    pub runs_on: RunsOn,
    /// One line for the user.
    pub reason: String,
}

/// The stored model check.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Type)]
pub struct CheckView {
    /// The model it chose.
    pub model: String,
    /// Where it passed.
    pub runs_on: RunsOn,
    /// The model a meeting falls back to when it falls behind.
    pub fallback: Option<String>,
    /// Whether it still holds for this machine, the installed models and the meeting
    /// languages (else it should run again).
    pub current: bool,
}

/// What the app uses now, with the settings applied.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Type)]
pub struct InUseView {
    /// The transcription model.
    pub transcription: String,
    /// Where transcription runs first.
    pub transcription_runs_on: RunsOn,
    /// Why this model (the setting, the check, the recommendation, the fallback).
    pub transcription_source: String,
    /// The local summary model.
    pub summary: String,
    /// Where summaries run when this computer writes them.
    pub summary_runs_on: RunsOn,
    /// Whether this computer writes the summaries; otherwise (an endpoint, Claude Code, Codex, no
    /// summary) the local model and part above are only what a switch back would use.
    pub summaries_here: bool,
    /// What the user is told: a chosen part not used, a stored setting not known.
    pub notes: Vec<String>,
}

/// Everything the Models page lists.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Type)]
pub struct ModelsView {
    /// The meeting languages the advice is for.
    pub languages: Vec<String>,
    /// The transcription model recommended for them (`null`: no model covers them).
    pub recommended: Option<RecommendedView>,
    /// The four transcription choices, recommended first.
    pub choices: Vec<ChoiceView>,
    /// The summary model of the memory rule.
    pub summary_recommended: RecommendedView,
    /// Every transcription model.
    pub transcription: Vec<ModelItem>,
    /// Every local summary model.
    pub summary: Vec<ModelItem>,
    /// What every meeting needs, one item (T9.1f): never chosen, never removed.
    pub helpers: ModelItem,
    /// The NPU versions of the transcription models (T8.4g): listed by an ARM64 build only, empty
    /// on x64.
    pub npu: Vec<ModelItem>,
    /// The stored check, if any.
    pub check: Option<CheckView>,
    /// What the app uses now.
    pub in_use: InUseView,
}

fn internal(e: &CatalogueError) -> IpcError {
    IpcError {
        kind: "model".into(),
        message: plain(e.to_string()),
        retryable: false,
    }
}

fn source_text(source: &TranscriptionSource) -> String {
    match source {
        TranscriptionSource::Setting => "chosen in Settings".into(),
        TranscriptionSource::ModelCheck => "the model check".into(),
        TranscriptionSource::Recommended(why) => format!("recommended ({why})"),
        TranscriptionSource::Fallback(why) => format!("the fallback model ({why})"),
    }
}

fn role(r: ChoiceRole) -> ChoiceKind {
    match r {
        ChoiceRole::Recommended => ChoiceKind::Recommended,
        ChoiceRole::Lighter => ChoiceKind::Lighter,
        ChoiceRole::European => ChoiceKind::European,
        ChoiceRole::EveryLanguage => ChoiceKind::EveryLanguage,
    }
}

/// What a summary on the graphics card runs through in this build (the DLLs beside the exe):
/// `cuda_settings::apply_start` changes it when a CUDA folder is loaded.
pub(crate) fn summary_gpu_part(order: &[Accelerator]) -> RunsOn {
    order
        .iter()
        .find(|a| matches!(a, Accelerator::Cuda | Accelerator::Vulkan))
        .map_or(RunsOn::Processor, |a| RunsOn::of_accelerator(*a))
}

/// The Models page of this machine (`services`' store, `models`, `hw`, this build's
/// `order`), `downloading` saying which models download now.
///
/// # Errors
/// Store errors; a stored check that does not parse; a broken embedded catalogue.
pub fn models_view(
    services: &Services,
    models: &ModelDir,
    hw: &earshot_runtime::earshot_hardware::Hardware,
    order: &[Accelerator],
    downloading: &dyn Fn(&str) -> bool,
) -> Result<ModelsView, IpcError> {
    let catalogue = Catalogue::embedded().map_err(|e| internal(&e))?;
    let (settings, setup, summary, stored, protected, summaries_here) = {
        let db = services.store();
        let (settings, _) = model_settings(&db)?;
        let setup = host::transcription_setup(&db, models, order, hw)?;
        let summary = host::local_summary_placement(&db, order, hw)?;
        (
            settings,
            setup,
            summary,
            stored_check(&db)?,
            host::protected_models(&db, models, order, hw)?,
            earshot_runtime::summary_settings::summary_settings(&db)?
                .0
                .writer
                == earshot_runtime::summary_settings::SummaryWriter::OnThisComputer,
        )
    };
    let langs = settings.language_refs();
    let fingerprint = check_fingerprint(models, &hardware_facts(), &langs);
    let current: Option<&StoredCheck> = stored.as_ref().filter(|c| c.current(&fingerprint));
    let advice = recommend::advise(hw, order, &langs, current)?;
    let summary_choice = host::summary_model_for(&summary, hw)?;
    let summary_on = if summary.gpu {
        summary_gpu_part(order)
    } else {
        RunsOn::Processor
    };
    let item = |m: &earshot_runtime::catalogue::Model| -> Result<ModelItem, IpcError> {
        let install = catalogue
            .install_state(models, &m.name)
            .map_err(|e| internal(&e))?;
        let (kind, runs_on, check) = match m.kind {
            ModelKind::Transcription => {
                // The NPU only while the model's NPU version is installed (T8.4g).
                let on = recommend::runs_on_here(models, &m.name, order);
                (
                    ModelUse::Transcription,
                    RunsOn::of(on),
                    Verdict::of(recommend::check_verdict(current, &m.name, on)),
                )
            }
            ModelKind::Summary => (ModelUse::Summary, summary_on, Verdict::Unknown),
            ModelKind::Helpers => (ModelUse::Helpers, RunsOn::Processor, Verdict::Unknown),
            ModelKind::NpuVersion => (ModelUse::NpuVersion, RunsOn::Npu, Verdict::Unknown),
        };
        Ok(ModelItem {
            name: m.name.clone(),
            title: earshot_runtime::catalogue::display_name(&m.name).to_owned(),
            kind,
            size: install.total,
            languages: m.languages.clone(),
            installed: match install.state {
                InstallState::Installed => Installed::Yes,
                InstallState::Partial => Installed::Partly,
                InstallState::Missing => Installed::No,
            },
            held: install.held,
            runs_on,
            check,
            protected: protected.contains(&m.name),
            downloading: downloading(&m.name),
        })
    };
    let list = |kind| -> Result<Vec<ModelItem>, IpcError> {
        catalogue.models_of(kind).map(item).collect()
    };
    let helpers = catalogue
        .models_of(ModelKind::Helpers)
        .next()
        .ok_or_else(|| {
            internal(&CatalogueError::UnknownModel(
                earshot_runtime::catalogue::HELPERS_MODEL.to_owned(),
            ))
        })
        .and_then(item)?;
    let mut notes = setup.notes.clone();
    notes.extend(summary.note.clone());
    Ok(ModelsView {
        languages: settings.languages.clone(),
        recommended: advice.transcription.map(|r| RecommendedView {
            model: r.model.to_owned(),
            runs_on: RunsOn::of(r.backend),
            reason: r.reason,
        }),
        choices: advice
            .choices
            .iter()
            .map(|c| ChoiceView {
                role: role(c.role),
                model: c.model.to_owned(),
            })
            .collect(),
        summary_recommended: RecommendedView {
            model: advice.summary.model.id.to_owned(),
            runs_on: if advice.summary_on_gpu {
                summary_gpu_part(order)
            } else {
                RunsOn::Processor
            },
            reason: advice.summary.user_reason(),
        },
        transcription: list(ModelKind::Transcription)?,
        summary: list(ModelKind::Summary)?,
        helpers,
        npu: list(ModelKind::NpuVersion)?,
        check: stored.as_ref().map(|c| CheckView {
            model: c.model.clone(),
            runs_on: RunsOn::of(c.backend),
            fallback: c.fallback.clone(),
            current: c.current(&fingerprint),
        }),
        in_use: InUseView {
            transcription: setup.choice.model.clone(),
            transcription_runs_on: setup
                .order
                .first()
                .map_or(RunsOn::Processor, |a| RunsOn::of_accelerator(*a)),
            transcription_source: source_text(&setup.source),
            summary: summary_choice.model.id.to_owned(),
            summary_runs_on: summary_on,
            summaries_here,
            notes,
        },
    })
}

/// Where a task runs, as the settings screen offers it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Type)]
pub enum PartChoice {
    /// The fastest part that passed the check.
    Automatic,
    /// The graphics card.
    GraphicsCard,
    /// The NPU (transcription only).
    Npu,
    /// The processor.
    Processor,
}

impl PartChoice {
    fn of(p: Part) -> Self {
        match p {
            Part::Automatic => Self::Automatic,
            Part::GraphicsCard => Self::GraphicsCard,
            Part::Npu => Self::Npu,
            Part::Processor => Self::Processor,
        }
    }

    fn part(self) -> Part {
        match self {
            Self::Automatic => Part::Automatic,
            Self::GraphicsCard => Part::GraphicsCard,
            Self::Npu => Part::Npu,
            Self::Processor => Part::Processor,
        }
    }
}

/// The Models page settings as the UI sends and reads them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct ModelSettingsValue {
    /// The transcription model (`null`: automatic).
    pub transcription: Option<String>,
    /// Where transcription runs.
    pub transcription_part: PartChoice,
    /// The local summary model (`null`: automatic).
    pub summary: Option<String>,
    /// Where summaries run (never `Npu`).
    pub summary_part: PartChoice,
    /// Meeting language codes.
    pub languages: Vec<String>,
}

impl ModelSettingsValue {
    fn of(s: &ModelSettings) -> Self {
        Self {
            transcription: s.transcription.name().map(str::to_owned),
            transcription_part: PartChoice::of(s.transcription_part),
            summary: s.summary.name().map(str::to_owned),
            summary_part: PartChoice::of(s.summary_part),
            languages: s.languages.clone(),
        }
    }

    fn settings(&self) -> ModelSettings {
        let pick = |m: &Option<String>| m.clone().map_or(ModelPick::Automatic, ModelPick::Named);
        ModelSettings {
            transcription: pick(&self.transcription),
            transcription_part: self.transcription_part.part(),
            summary: pick(&self.summary),
            summary_part: self.summary_part.part(),
            languages: self.languages.clone(),
        }
    }
}

/// The stored settings and the stored values this version does not know.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Type)]
pub struct ModelSettingsView {
    /// The settings in use (defaults for anything absent or unknown).
    pub value: ModelSettingsValue,
    /// One line per stored value this version does not know.
    pub unknown: Vec<String>,
}

/// The settings of `services`' store.
///
/// # Errors
/// Store errors.
pub fn settings_view(services: &Services) -> Result<ModelSettingsView, IpcError> {
    let (s, unknown) = model_settings(&services.store())?;
    Ok(ModelSettingsView {
        value: ModelSettingsValue::of(&s),
        unknown: unknown.iter().map(ToString::to_string).collect(),
    })
}

/// The UI error of a refused setting.
#[must_use]
pub fn settings_error(e: SettingsError) -> IpcError {
    match e {
        SettingsError::Store(e) => IpcError::from(e),
        other => IpcError {
            kind: "invalid_setting".into(),
            message: other.to_string(),
            retryable: false,
        },
    }
}

/// Checks and stores `value`; returns the settings as stored.
///
/// # Errors
/// `invalid_setting` (nothing stored); store errors.
pub fn set_settings(
    services: &Services,
    value: &ModelSettingsValue,
) -> Result<ModelSettingsView, IpcError> {
    let stored = {
        let mut db = services.store();
        set_model_settings(&mut db, &value.settings()).map_err(settings_error)?
    };
    Ok(ModelSettingsView {
        value: ModelSettingsValue::of(&stored),
        unknown: Vec::new(),
    })
}

/// What a removal freed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Type)]
pub struct RemovedView {
    /// Bytes freed.
    pub bytes_freed: u64,
    /// Files kept because another installed model uses them.
    pub kept_shared: Vec<String>,
}

/// The UI error of a refused removal.
#[must_use]
pub fn remove_error(e: &CatalogueError) -> IpcError {
    let kind = match e {
        CatalogueError::Chosen(_) => "model_in_use",
        CatalogueError::UnknownModel(_) => "unknown_model",
        _ => "model",
    };
    IpcError {
        kind: kind.into(),
        message: plain(e.to_string()),
        retryable: false,
    }
}

/// Removes `model`'s files unless it is protected (`host::protected_models` for `hw` and
/// `order`, what a run would see) or downloading. The downloading test and the removal are
/// one critical section with `ModelJobs::start_download` ([`ModelJobs::without_download`]),
/// so a download cannot start between them.
///
/// # Errors
/// `model_in_use` for a chosen or kept model, `busy` while it downloads, `unknown_model`,
/// store and file errors.
pub fn remove(
    services: &Services,
    models: &ModelDir,
    hw: &earshot_runtime::earshot_hardware::Hardware,
    order: &[Accelerator],
    jobs: &ModelJobs,
    model: &str,
) -> Result<RemovedView, IpcError> {
    let protected = host::protected_models(&services.store(), models, order, hw)?;
    jobs.without_download(model, || {
        let removed =
            host::remove_model(models, model, &protected).map_err(|e| remove_error(&e))?;
        Ok(RemovedView {
            bytes_freed: removed.bytes_freed,
            kept_shared: removed.kept_shared,
        })
    })
}

/// [`hardware_view`] of this machine and build.
#[must_use]
pub fn this_hardware() -> HardwareView {
    hardware_view(&earshot_runtime::hardware(), &accelerator_order())
}

#[cfg(test)]
#[path = "models_tests.rs"]
mod tests;
