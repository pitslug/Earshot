//! The Models page settings (T7.4e): which transcription and summary model the app uses,
//! where each runs, and the meeting languages. Device-local (`local_state`, never synced, no
//! migration), one key each, written only through [`set_model_settings`].
//!
//! - **Models** ([`ModelPick`]): `automatic` (transcription: the current model check, else
//!   the recommendation for the meeting languages when it is installed, else
//!   [`crate::host::FALLBACK_MODEL`]; summaries: the T6.21b memory rule) or a model name.
//! - **Where it runs** ([`Part`]): automatic (the build's order from the check's backend
//!   down, as before), the graphics card, the NPU, or the processor. A model forced onto a
//!   part it cannot use (no NPU version, too big for the graphics card, a build or computer
//!   without that part) runs on the processor and the reason is returned with the plan
//!   ([`Placement::note`], [`SummaryPlacement::note`]); the host shows it, never silently.
//! - **Meeting languages**: codes as the ladders list them ([`model_check::language_code`]),
//!   default [`DEFAULT_MEETING_LANGUAGES`]; they decide the stored check's fingerprint and
//!   the recommendation.
//!
//! Each value is stored as text. A value this build does not know (written by a newer one,
//! or a model no longer shipped) reads as the default and is reported ([`UnknownSetting`]),
//! as `speaker_timing::StoredTiming` does. Setting a value checks it first
//! ([`validate`]): an unknown model, a language no model covers, a chosen model that does not
//! transcribe a meeting language, or the NPU for summaries is a [`SettingsError`].

use std::fmt;

use earshot_core::asr::BackendKind;
use earshot_hardware::Hardware;
use earshot_store::Store;

use crate::RuntimeError;
use crate::backends::{Accelerator, AsrModel, asr_model};
use crate::catalogue::{Catalogue, ModelKind, display_name};
use crate::local::DeviceRecord;
use crate::model_check::{self, LADDERS, MEMORY_FACTOR, MODELS};
use crate::summary_model::{self, SUMMARY_MODELS};

#[cfg(test)]
#[path = "model_settings_tests.rs"]
mod tests;

/// `local_state` key of the chosen transcription model.
pub const TRANSCRIPTION_MODEL_KEY: &str = "settings/transcription-model";
/// `local_state` key of where transcription runs.
pub const TRANSCRIPTION_PART_KEY: &str = "settings/transcription-part";
/// `local_state` key of the chosen local summary model.
pub const SUMMARY_MODEL_KEY: &str = "settings/summary-model";
/// `local_state` key of where summaries run.
pub const SUMMARY_PART_KEY: &str = "settings/summary-part";
/// `local_state` key of the meeting languages (comma-separated codes).
pub const MEETING_LANGUAGES_KEY: &str = "settings/meeting-languages";

/// The meeting languages until the user picks some.
pub const DEFAULT_MEETING_LANGUAGES: &[&str] = &["en"];

/// The stored text of [`ModelPick::Automatic`].
const AUTOMATIC: &str = "automatic";

/// A chosen model.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum ModelPick {
    /// Follow the check and the recommendation (module docs).
    #[default]
    Automatic,
    /// This model, by exact manifest name.
    Named(String),
}

impl ModelPick {
    /// The model name, `None` for automatic.
    #[must_use]
    pub fn name(&self) -> Option<&str> {
        match self {
            Self::Automatic => None,
            Self::Named(n) => Some(n),
        }
    }

    fn stored(&self) -> &str {
        self.name().unwrap_or(AUTOMATIC)
    }

    fn parse(text: &str, known: impl Fn(&str) -> bool) -> Option<Self> {
        if text == AUTOMATIC {
            Some(Self::Automatic)
        } else {
            known(text).then(|| Self::Named(text.to_owned()))
        }
    }
}

/// Where a task runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Part {
    /// The fastest part that passed the check (the build's order).
    #[default]
    Automatic,
    /// The graphics card (CUDA, else Vulkan, whichever this build has).
    GraphicsCard,
    /// The NPU (transcription only).
    Npu,
    /// The processor.
    Processor,
}

impl Part {
    /// Every value, in the order a settings screen lists them.
    pub const ALL: [Self; 4] = [
        Self::Automatic,
        Self::GraphicsCard,
        Self::Npu,
        Self::Processor,
    ];

    /// The stored name: `automatic`, `gpu`, `npu`, `processor`.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Automatic => AUTOMATIC,
            Self::GraphicsCard => "gpu",
            Self::Npu => "npu",
            Self::Processor => "processor",
        }
    }

    /// Reads [`Part::as_str`].
    #[must_use]
    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|p| p.as_str() == name)
    }

    /// The part's name in a sentence ("the graphics card").
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Automatic => "the fastest part",
            Self::GraphicsCard => "the graphics card",
            Self::Npu => "the NPU",
            Self::Processor => "the processor",
        }
    }
}

/// Every Models page setting.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelSettings {
    /// The transcription model.
    pub transcription: ModelPick,
    /// Where transcription runs.
    pub transcription_part: Part,
    /// The local summary model.
    pub summary: ModelPick,
    /// Where summaries run (never [`Part::Npu`]).
    pub summary_part: Part,
    /// Meeting language codes, never empty.
    pub languages: Vec<String>,
}

impl Default for ModelSettings {
    fn default() -> Self {
        Self {
            transcription: ModelPick::Automatic,
            transcription_part: Part::Automatic,
            summary: ModelPick::Automatic,
            summary_part: Part::Automatic,
            languages: DEFAULT_MEETING_LANGUAGES
                .iter()
                .map(|l| (*l).to_owned())
                .collect(),
        }
    }
}

impl ModelSettings {
    /// The meeting languages as `&str`s (for `host::choose_model` and the check).
    #[must_use]
    pub fn language_refs(&self) -> Vec<&str> {
        self.languages.iter().map(String::as_str).collect()
    }
}

/// A stored value this build does not know; the default is used instead.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnknownSetting {
    /// Its `local_state` key.
    pub key: &'static str,
    /// What is stored.
    pub stored: String,
}

impl fmt::Display for UnknownSetting {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "the stored setting {} ({:?}) is not known to this version; the default is used",
            self.key, self.stored
        )
    }
}

/// Why a setting was refused (nothing was stored).
#[derive(Debug)]
pub enum SettingsError {
    /// Not a transcription model of the catalogue.
    UnknownModel(String),
    /// Not a shipped local summary model.
    UnknownSummaryModel(String),
    /// No meeting language.
    NoLanguages,
    /// No model transcribes this language.
    UnknownLanguage(String),
    /// The chosen transcription model does not transcribe these meeting languages.
    ModelLanguages {
        /// The model.
        model: String,
        /// The languages it does not cover.
        missing: Vec<String>,
    },
    /// Summaries do not run on the NPU.
    NpuForSummaries,
    /// Reading or writing the store failed.
    Store(RuntimeError),
}

impl fmt::Display for SettingsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownModel(m) => write!(f, "{m} is not a transcription model of this version"),
            Self::UnknownSummaryModel(m) => {
                write!(f, "{m} is not a summary model of this version")
            }
            Self::NoLanguages => write!(f, "choose at least one meeting language"),
            Self::UnknownLanguage(l) => write!(f, "no transcription model understands {l:?}"),
            Self::ModelLanguages { model, missing } => write!(
                f,
                "{} does not transcribe {}; choose a multilingual model",
                display_name(model),
                missing.join(", ")
            ),
            Self::NpuForSummaries => write!(f, "summaries cannot run on the NPU"),
            Self::Store(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for SettingsError {}

impl From<RuntimeError> for SettingsError {
    fn from(e: RuntimeError) -> Self {
        Self::Store(e)
    }
}

/// Whether `name` is a transcription model the app can choose: a catalogue transcription
/// model the runtime can load.
#[must_use]
pub fn is_transcription_model(name: &str) -> bool {
    asr_model(name).is_ok()
        && Catalogue::embedded().is_ok_and(|c| {
            c.model(name)
                .is_some_and(|m| m.kind == ModelKind::Transcription)
        })
}

fn is_summary_model(name: &str) -> bool {
    SUMMARY_MODELS.iter().any(|m| m.id == name)
}

/// The meeting languages of `text` (comma-separated), normalised and without repeats;
/// `None` when empty or when a language no model covers is listed.
fn parse_languages(text: &str) -> Option<Vec<String>> {
    let mut out: Vec<String> = Vec::new();
    for l in text.split(',').map(str::trim).filter(|l| !l.is_empty()) {
        let code = model_check::language_code(l);
        if !MODELS.iter().any(|m| m.languages.contains(&code.as_str())) {
            return None;
        }
        if !out.contains(&code) {
            out.push(code);
        }
    }
    (!out.is_empty()).then_some(out)
}

/// The settings `settings` would be stored as: languages normalised, everything checked
/// (module docs).
///
/// # Errors
/// [`SettingsError`] (never [`SettingsError::Store`]).
pub fn validate(settings: &ModelSettings) -> Result<ModelSettings, SettingsError> {
    let mut languages: Vec<String> = Vec::new();
    for l in settings.languages.iter().map(|l| l.trim()) {
        if l.is_empty() {
            continue;
        }
        let code =
            parse_languages(l).ok_or_else(|| SettingsError::UnknownLanguage(l.to_owned()))?;
        for c in code {
            if !languages.contains(&c) {
                languages.push(c);
            }
        }
    }
    if languages.is_empty() {
        return Err(SettingsError::NoLanguages);
    }
    if let ModelPick::Named(m) = &settings.transcription {
        if !is_transcription_model(m) {
            return Err(SettingsError::UnknownModel(m.clone()));
        }
        let missing: Vec<String> = languages
            .iter()
            .filter(|l| !model_check::covers(m, &[l.as_str()]))
            .cloned()
            .collect();
        if !missing.is_empty() {
            return Err(SettingsError::ModelLanguages {
                model: m.clone(),
                missing,
            });
        }
    }
    if let ModelPick::Named(m) = &settings.summary
        && !is_summary_model(m)
    {
        return Err(SettingsError::UnknownSummaryModel(m.clone()));
    }
    if settings.summary_part == Part::Npu {
        return Err(SettingsError::NpuForSummaries);
    }
    Ok(ModelSettings {
        languages,
        ..settings.clone()
    })
}

/// The stored settings (absent values are the defaults) and every stored value this build
/// does not know (its default is used).
///
/// # Errors
/// Store errors.
pub fn model_settings(store: &Store) -> Result<(ModelSettings, Vec<UnknownSetting>), RuntimeError> {
    let mut unknown = Vec::new();
    let mut out = ModelSettings::default();
    for key in [
        TRANSCRIPTION_MODEL_KEY,
        TRANSCRIPTION_PART_KEY,
        SUMMARY_MODEL_KEY,
        SUMMARY_PART_KEY,
        MEETING_LANGUAGES_KEY,
    ] {
        let Some(bytes) = store.local_value(key)? else {
            continue;
        };
        let text = String::from_utf8_lossy(&bytes).into_owned();
        let known = match key {
            TRANSCRIPTION_MODEL_KEY => ModelPick::parse(&text, is_transcription_model)
                .map(|p| out.transcription = p)
                .is_some(),
            TRANSCRIPTION_PART_KEY => Part::parse(&text)
                .map(|p| out.transcription_part = p)
                .is_some(),
            SUMMARY_MODEL_KEY => ModelPick::parse(&text, is_summary_model)
                .map(|p| out.summary = p)
                .is_some(),
            SUMMARY_PART_KEY => Part::parse(&text)
                .filter(|p| *p != Part::Npu)
                .map(|p| out.summary_part = p)
                .is_some(),
            _ => parse_languages(&text).map(|l| out.languages = l).is_some(),
        };
        if !known {
            unknown.push(UnknownSetting { key, stored: text });
        }
    }
    // A chosen model that no longer covers the stored languages is not used.
    if let ModelPick::Named(m) = &out.transcription
        && !out
            .languages
            .iter()
            .all(|l| model_check::covers(m, &[l.as_str()]))
    {
        unknown.push(UnknownSetting {
            key: TRANSCRIPTION_MODEL_KEY,
            stored: m.clone(),
        });
        out.transcription = ModelPick::Automatic;
    }
    Ok((out, unknown))
}

/// Checks and stores every setting (replacing earlier ones); returns what was stored.
///
/// # Errors
/// [`SettingsError`]: nothing is stored on a refusal.
pub fn set_model_settings(
    store: &mut Store,
    settings: &ModelSettings,
) -> Result<ModelSettings, SettingsError> {
    let s = validate(settings)?;
    let languages = s.languages.join(",");
    let values = [
        (DeviceRecord::TranscriptionModel, s.transcription.stored()),
        (
            DeviceRecord::TranscriptionPart,
            s.transcription_part.as_str(),
        ),
        (DeviceRecord::SummaryModel, s.summary.stored()),
        (DeviceRecord::SummaryPart, s.summary_part.as_str()),
        (DeviceRecord::MeetingLanguages, languages.as_str()),
    ];
    store
        .transaction(|tx| -> Result<(), earshot_store::StoreError> {
            for (record, value) in values {
                crate::local::set_tx(tx, &record.key(), value.as_bytes())?;
            }
            Ok(())
        })
        .map_err(RuntimeError::from)?;
    Ok(s)
}

/// Where transcription runs ([`transcription_placement`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Placement {
    /// The accelerators to try, best first.
    pub order: Vec<Accelerator>,
    /// Why the chosen part is not used (the model then runs on the processor).
    pub note: Option<String>,
}

/// The dedicated memory of the largest discrete (or unknown) graphics card, `None` when
/// the read found none with a known size.
pub(crate) fn graphics_memory(hardware: &Hardware) -> Option<u64> {
    hardware
        .gpus
        .iter()
        .filter(|g| g.integrated != Some(true))
        .filter_map(|g| g.dedicated_memory)
        .max()
}

/// Whether the machine has a discrete GPU: the rule for summaries on Vulkan (D23: anything else
/// stays on the CPU until measured). An adapter of unknown kind counts only with dedicated
/// memory of its own. An unreadable machine lists no adapter, so it stays on the CPU.
#[must_use]
pub fn has_discrete_gpu(hardware: &Hardware) -> bool {
    hardware.gpus.iter().any(|g| match g.integrated {
        Some(integrated) => !integrated,
        None => g.dedicated_memory.is_some(),
    })
}

/// The names of the machine's integrated GPUs: a summary load that names one of these counts as
/// refused ([`crate::summary::LocalSummaryConfig::refuse_gpus`]).
#[must_use]
pub fn integrated_gpu_names(hardware: &Hardware) -> Vec<String> {
    hardware
        .gpus
        .iter()
        .filter(|g| g.integrated == Some(true))
        .map(|g| g.name.clone())
        .collect()
}

/// Whether a model of `size` bytes fits the graphics card: the one rule for transcription
/// and summaries, the model check's [`MEMORY_FACTOR`] (twice the file size: weights plus
/// activations or, for a summary, the context's KV cache) within the card's dedicated
/// memory. A judgement, not a measurement on the cards; unknown memory is not held against
/// a model.
#[must_use]
pub fn fits_graphics_card(size: u64, hardware: &Hardware) -> bool {
    graphics_memory(hardware).is_none_or(|m| size.saturating_mul(MEMORY_FACTOR) <= m)
}

fn on_processor(note: String) -> Placement {
    Placement {
        order: vec![Accelerator::Cpu],
        note: Some(note),
    }
}

/// Where `model` runs for `part`: `automatic` (the order without a forced part, e.g.
/// `ModelChoice::order`) for [`Part::Automatic`], the processor alone for
/// [`Part::Processor`], else the forced part then the processor, or the processor with the
/// reason when the build (`build`, the accelerator order) has no such part, the model has no
/// version for it (Parakeet on Vulkan, a model not converted for the NPU), or the model is
/// too big for the graphics card's memory (`hardware`; unknown memory is not held against
/// it).
#[must_use]
pub fn transcription_placement(
    model: &str,
    part: Part,
    automatic: Vec<Accelerator>,
    build: &[Accelerator],
    hardware: &Hardware,
) -> Placement {
    let forced = |a: Accelerator| Placement {
        order: vec![a, Accelerator::Cpu],
        note: None,
    };
    match part {
        Part::Automatic => Placement {
            order: automatic,
            note: None,
        },
        Part::Processor => Placement {
            order: vec![Accelerator::Cpu],
            note: None,
        },
        Part::GraphicsCard => {
            let Some(gpu) = build
                .iter()
                .copied()
                .filter(|a| crate::backends::model_can_run_on(model, *a))
                .find(|a| matches!(a, Accelerator::Cuda | Accelerator::Vulkan))
            else {
                if build.contains(&Accelerator::Cuda) {
                    return on_processor(format!(
                        "{} has no CUDA version here, so it runs on the processor.",
                        display_name(model)
                    ));
                }
                return on_processor(format!(
                    "This computer has no graphics card Earshot can use, so {} runs on the \
                     processor.",
                    display_name(model)
                ));
            };
            let whisper = matches!(asr_model(model), Ok(AsrModel::Whisper(_)));
            if gpu == Accelerator::Vulkan && !whisper {
                return on_processor(format!(
                    "{} has no graphics card version here, so it runs on the processor.",
                    display_name(model)
                ));
            }
            let size = model_check::model_facts(model).map_or(0, |f| f.size);
            if !fits_graphics_card(size, hardware) {
                return on_processor(format!(
                    "{} is too big for your graphics card, so it runs on the processor.",
                    display_name(model)
                ));
            }
            forced(gpu)
        }
        Part::Npu => {
            if !build.contains(&Accelerator::Qnn) {
                return on_processor(format!(
                    "This computer has no NPU Earshot can use, so {} runs on the processor.",
                    display_name(model)
                ));
            }
            let converted = LADDERS
                .iter()
                .any(|l| l.backend == BackendKind::Qnn && l.models.contains(&model));
            if !converted {
                return on_processor(format!(
                    "{} has no NPU version, so it runs on the processor.",
                    display_name(model)
                ));
            }
            forced(Accelerator::Qnn)
        }
    }
}

/// Where summaries run and with which model ([`summary_placement`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SummaryPlacement {
    /// The model asked for by name (`None`: the memory rule picks).
    pub requested: Option<String>,
    /// Whether the summary may use the graphics card.
    pub gpu: bool,
    /// Why the chosen part is not used.
    pub note: Option<String>,
}

/// Where summaries run for `pick` and `part` on a build with accelerator `order` and this
/// `hardware`: automatic follows `summary_model::summary_gpu_planned`; the processor never
/// uses the graphics card; the graphics card is used only when this build can (else the
/// processor, with the reason) and the model fits its memory ([`fits_graphics_card`], else
/// the processor: "too big for your graphics card").
///
/// # Errors
/// [`RuntimeError::UnknownModel`] for a named model that is not shipped.
pub fn summary_placement(
    pick: &ModelPick,
    part: Part,
    order: &[Accelerator],
    hardware: &Hardware,
) -> Result<SummaryPlacement, RuntimeError> {
    placement_when(
        summary_model::summary_gpu_planned(order),
        pick,
        part,
        &summary_model::llama_order(order),
        hardware,
    )
}

/// [`summary_placement`] with `planned` given (whether this build could run it on a GPU).
fn placement_when(
    planned: bool,
    pick: &ModelPick,
    part: Part,
    order: &[Accelerator],
    hardware: &Hardware,
) -> Result<SummaryPlacement, RuntimeError> {
    let requested = pick.name().map(str::to_owned);
    if let Some(m) = &requested {
        summary_model::summary_model(m)?;
    }
    // Why the card cannot take it, or None. CUDA keeps its own rule: only a forced card is
    // checked for room.
    let refusal = |vulkan_only: bool| -> Result<Option<String>, RuntimeError> {
        let on_vulkan = order.first() == Some(&Accelerator::Vulkan);
        if vulkan_only && !on_vulkan {
            return Ok(None);
        }
        if on_vulkan && !has_discrete_gpu(hardware) {
            return Ok(Some(
                "This computer has no separate graphics card, so summaries run on the processor."
                    .to_owned(),
            ));
        }
        let choice = summary_model::choose_summary_model_on(
            requested.as_deref(),
            hardware.total_memory,
            true,
            graphics_memory(hardware),
        )?;
        Ok((!fits_graphics_card(choice.model.size, hardware)).then(|| {
            format!(
                "{} is too big for your graphics card, so summaries run on the processor and \
                 take longer.",
                display_name(choice.model.id)
            )
        }))
    };
    let (gpu, note) = match part {
        Part::Automatic if !planned => (false, None),
        Part::Automatic => {
            let refused = refusal(true)?;
            // A note only about a model the user named; choosing for them says nothing.
            (
                refused.is_none(),
                refused.filter(|_| requested.is_some() && has_discrete_gpu(hardware)),
            )
        }
        Part::Processor => (false, None),
        Part::Npu => (
            false,
            Some("Summaries cannot run on the NPU, so they run on the processor.".to_owned()),
        ),
        Part::GraphicsCard if !planned => (
            false,
            Some(
                "This version of Earshot cannot run summaries on a graphics card here, so they \
                 run on the processor."
                    .to_owned(),
            ),
        ),
        Part::GraphicsCard => match refusal(false)? {
            Some(note) => (false, Some(note)),
            None => (true, None),
        },
    };
    Ok(SummaryPlacement {
        requested,
        gpu,
        note,
    })
}
