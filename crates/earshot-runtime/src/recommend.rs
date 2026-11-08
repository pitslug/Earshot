//! What the Models page recommends before anything is downloaded (T7.4d).
//!
//! - **Transcription** ([`recommend_transcription`]): the model check's own starting rule
//!   ([`model_check::starting_rung`] over [`model_check::candidates`] for the meeting
//!   languages, backends in [`model_check::backend_order`]) without running a trial. As in
//!   the check's search, a Vulkan answer is kept only when its model is more accurate
//!   ([`model_check::QUALITY_ORDER`]) than the next backend's. Meetings in languages no
//!   ladder of this machine covers get the most accurate manifest model that covers them
//!   (Whisper large-v3-turbo for most languages; T7.4). The check, once
//!   run, confirms or steps down.
//! - **The four choices** ([`transcription_choices`], T7.4): recommended (the English
//!   [`recommend_transcription`], so a Vulkan build agrees with it), lighter (the next rung of
//!   that backend's English ladder), multilingual for European
//!   languages ([`EUROPEAN_MODEL`]) and for every language ([`EVERY_LANGUAGE_MODEL`]), each
//!   with where it would run here ([`runs_on`]) and what a stored check found for it
//!   ([`CheckVerdict`]). "Too slow here" comes only from a recorded trial; without one
//!   the verdict is [`CheckVerdict::Unknown`], never a guess.
//! - **Summaries** ([`ModelsAdvice::summary`]): [`summary_model::choose_summary_model`]
//!   over the reported memory and where the summary would run
//!   ([`summary_model::summary_gpu_planned`]).
//!
//! The hardware facts come from `earshot_hardware`; [`facts_for`] turns them into the
//! model check's [`HardwareFacts`] as `model_bench::hardware_facts` does (no free video
//! memory: the trial decides whether a model fits a GPU).

use earshot_core::asr::BackendKind;
use earshot_hardware::Hardware;

use crate::RuntimeError;
use crate::backends::{Accelerator, AsrModel, asr_model};
use crate::catalogue::display_name;
use crate::model_bench::{StoredCheck, StoredVerdict};
use crate::model_check::{
    self, HardwareFacts, LADDERS, MODELS, Scope, backend_order, candidates, quality_rank,
    starting_rung,
};
use crate::summary_model::{self, SummaryModelChoice};

#[cfg(test)]
#[path = "recommend_tests.rs"]
mod tests;

/// The multilingual model for European languages (Parakeet 0.6B v3: 25 languages).
pub const EUROPEAN_MODEL: &str = "parakeet-tdt-0.6b-v3";
/// The multilingual model for every language (Whisper large-v3-turbo: 100 languages).
pub const EVERY_LANGUAGE_MODEL: &str = "large-v3-turbo";

/// The model check's facts for `hardware` and the build's accelerator `order`: every
/// accelerator but the CPU, without free memory (as `model_bench::hardware_facts`);
/// unknown memory is not held against a model and unknown cores count as one. The core
/// count is the hardware read's logical cores, which
/// `hardware_facts_of_this_host_feed_the_recommendation` pins to the check's own count
/// (`model_bench::hardware_facts().cores`), so advice and check start from the same rung.
#[must_use]
pub fn facts_for(hardware: &Hardware, order: &[Accelerator]) -> HardwareFacts {
    HardwareFacts {
        accelerators: order
            .iter()
            .filter(|a| **a != Accelerator::Cpu)
            .map(|a| model_check::Accelerator {
                backend: a.backend(),
                free_memory: None,
            })
            .collect(),
        cores: hardware.cpu.logical_cores.unwrap_or(1),
        memory: hardware.total_memory.unwrap_or(u64::MAX),
        drivers: None,
    }
}

/// A recommended transcription model and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Recommended {
    /// The model.
    pub model: &'static str,
    /// Where it would run.
    pub backend: BackendKind,
    /// One line for the user.
    pub reason: String,
}

impl Recommended {
    /// This recommendation as it would run with what `models` holds: a Parakeet model
    /// recommended for the NPU runs on the processor until its NPU version is installed
    /// ([`runs_on_here`], T8.4g), and the reason says so. Anything else is unchanged.
    #[must_use]
    pub fn here(self, models: &crate::backends::ModelDir, order: &[Accelerator]) -> Self {
        if self.backend != BackendKind::Qnn {
            return self;
        }
        let backend = runs_on_here(models, self.model, order);
        if backend == BackendKind::Qnn {
            return self;
        }
        Self {
            reason: format!(
                "{}; it runs on {} until you download its NPU version",
                self.reason,
                backend_name(backend)
            ),
            backend,
            ..self
        }
    }
}

/// A backend's name for the user.
#[must_use]
pub fn backend_name(backend: BackendKind) -> &'static str {
    match backend {
        BackendKind::Cpu => "the processor",
        BackendKind::Cuda => "the NVIDIA graphics card",
        BackendKind::Vulkan => "the graphics card",
        BackendKind::Qnn => "the NPU",
        BackendKind::Fake => "a test backend",
    }
}

fn scope_of(languages: &[&str]) -> Scope {
    let english = languages
        .iter()
        .all(|l| model_check::language_code(l) == "en");
    if english {
        Scope::English
    } else {
        Scope::Multilingual
    }
}

fn scope_word(scope: Scope) -> &'static str {
    match scope {
        Scope::English => "English",
        Scope::Multilingual => "multilingual",
    }
}

/// One backend's starting rung for `languages`.
fn start_on(
    facts: &HardwareFacts,
    backend: BackendKind,
    languages: &[&str],
) -> Option<(&'static str, usize, Vec<&'static str>)> {
    let models = candidates(backend, languages);
    let start = starting_rung(facts, backend, &models)?;
    Some((models[start], start, models))
}

fn reason_for(backend: BackendKind, scope: Scope, start: usize, ladder: &[&'static str]) -> String {
    let base = format!(
        "the most accurate {} model for {}",
        scope_word(scope),
        backend_name(backend)
    );
    if start == 0 {
        base
    } else {
        format!(
            "{base} that suits this computer ({} needs more memory or processor cores)",
            ladder[..start]
                .iter()
                .map(|m| display_name(m))
                .collect::<Vec<_>>()
                .join(", ")
        )
    }
}

/// The transcription model to recommend for meetings in `languages` (empty: English) on
/// this hardware (module docs). `None` when no manifest model covers the languages.
#[must_use]
pub fn recommend_transcription(
    hardware: &Hardware,
    order: &[Accelerator],
    languages: &[&str],
) -> Option<Recommended> {
    let facts = facts_for(hardware, order);
    let scope = scope_of(languages);
    let mut pending: Option<Recommended> = None;
    for backend in backend_order(&facts) {
        let Some((model, start, ladder)) = start_on(&facts, backend, languages) else {
            continue;
        };
        let here = Recommended {
            model,
            backend,
            reason: reason_for(backend, scope, start, &ladder),
        };
        if let Some(v) = pending.take() {
            // The check keeps Vulkan only for a more accurate model (or the same model
            // faster, which only a trial can show).
            if quality_rank(v.model) < quality_rank(here.model) {
                return Some(v);
            }
            return Some(Recommended {
                reason: format!(
                    "{}; more accurate than {} on {}",
                    here.reason,
                    display_name(v.model),
                    backend_name(v.backend)
                ),
                ..here
            });
        }
        if backend == BackendKind::Vulkan {
            pending = Some(here);
        } else {
            return Some(here);
        }
    }
    if pending.is_some() {
        return pending;
    }
    // No ladder covers the languages: the most accurate model that does.
    let codes: Vec<String> = languages
        .iter()
        .map(|l| model_check::language_code(l))
        .collect();
    let codes: Vec<&str> = codes.iter().map(String::as_str).collect();
    let model = MODELS
        .iter()
        .map(|m| m.name)
        .filter(|m| model_check::covers(m, &codes))
        .min_by_key(|m| quality_rank(m))?;
    Some(Recommended {
        model,
        backend: runs_on(model, order),
        reason: format!(
            "the most accurate model that transcribes {}",
            codes.join(", ")
        ),
    })
}

/// [`runs_on`] with what `models` holds: the processor rather than the NPU for a Parakeet model
/// whose NPU version is not installed (`backends::npu_order`, T8.4g), as a meeting would run.
#[must_use]
pub fn runs_on_here(
    models: &crate::backends::ModelDir,
    model: &str,
    order: &[Accelerator],
) -> BackendKind {
    runs_on(
        model,
        &crate::backends::npu_order(models, model, order.to_vec()).0,
    )
}

/// Where `model` would run on a build with accelerator `order`: the first accelerator
/// that can run it ([`crate::backends::model_order`]; Vulkan runs whisper only) and whose
/// ladder lists it (where it was measured), else the processor.
#[must_use]
pub fn runs_on(model: &str, order: &[Accelerator]) -> BackendKind {
    let Ok(asr) = asr_model(model) else {
        return BackendKind::Cpu;
    };
    let whisper = matches!(asr, AsrModel::Whisper(_));
    crate::backends::model_order(&asr, order)
        .into_iter()
        .filter(|a| *a != Accelerator::Cpu)
        .filter(|a| *a != Accelerator::Vulkan || whisper)
        .map(Accelerator::backend)
        .find(|b| {
            LADDERS
                .iter()
                .any(|l| l.backend == *b && l.models.contains(&model))
        })
        .unwrap_or(BackendKind::Cpu)
}

/// What a stored check says about a model where it would run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckVerdict {
    /// The check found it keeps up.
    KeepsUp,
    /// The check found it too slow here (it stays selectable).
    TooSlow,
    /// The check found its output wrong on that backend.
    Fails,
    /// No current check tested it.
    Unknown,
}

/// `check`'s verdict on `model` on `backend` (module docs). `check` must be current for
/// this machine and the meeting languages ([`StoredCheck::current`]); pass `None` without
/// one.
#[must_use]
pub fn check_verdict(
    check: Option<&StoredCheck>,
    model: &str,
    backend: BackendKind,
) -> CheckVerdict {
    match check.and_then(|c| c.verdict(model, backend)) {
        Some(StoredVerdict::Pass) => CheckVerdict::KeepsUp,
        Some(StoredVerdict::TooSlow) => CheckVerdict::TooSlow,
        Some(StoredVerdict::Wrong) => CheckVerdict::Fails,
        None => CheckVerdict::Unknown,
    }
}

/// Which of the T7.4 choices.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChoiceRole {
    /// The first suitable rung of the English ladder.
    Recommended,
    /// The next rung (older or busy machines, less memory).
    Lighter,
    /// Meetings in European languages.
    European,
    /// Meetings in any language.
    EveryLanguage,
}

/// One transcription choice of the Models page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TranscriptionChoice {
    /// Which choice.
    pub role: ChoiceRole,
    /// The model.
    pub model: &'static str,
    /// Where it would run here ([`runs_on`]).
    pub runs_on: BackendKind,
    /// What a current check found ([`check_verdict`]).
    pub check: CheckVerdict,
    /// Languages it transcribes.
    pub languages: &'static [&'static str],
    /// Bytes to download.
    pub download_size: u64,
}

/// The T7.4 transcription choices (module docs), each model at most once and only models
/// some ladder lists (the multilingual ones only where a ladder has them).
#[must_use]
pub fn transcription_choices(
    hardware: &Hardware,
    order: &[Accelerator],
    check: Option<&StoredCheck>,
) -> Vec<TranscriptionChoice> {
    let mut picks: Vec<(ChoiceRole, &'static str)> = Vec::new();
    // The recommendation's own rule (the Vulkan rule included), then the next rung of its
    // backend's English ladder.
    let english = recommend_transcription(hardware, order, &[]).and_then(|r| {
        let ladder = candidates(r.backend, &[]);
        let start = ladder.iter().position(|m| *m == r.model)?;
        Some((r.model, start, ladder))
    });
    if let Some((model, start, ladder)) = english {
        picks.push((ChoiceRole::Recommended, model));
        if let Some(next) = ladder.get(start + 1) {
            picks.push((ChoiceRole::Lighter, next));
        }
    }
    for (role, model) in [
        (ChoiceRole::European, EUROPEAN_MODEL),
        (ChoiceRole::EveryLanguage, EVERY_LANGUAGE_MODEL),
    ] {
        let in_a_ladder = LADDERS
            .iter()
            .any(|l| l.scope == Scope::Multilingual && l.models.contains(&model));
        if in_a_ladder {
            picks.push((role, model));
        }
    }
    let mut seen = Vec::new();
    picks
        .into_iter()
        .filter(|(_, m)| {
            let new = !seen.contains(m);
            seen.push(m);
            new
        })
        .filter_map(|(role, model)| {
            let facts = model_check::model_facts(model)?;
            let backend = runs_on(model, order);
            Some(TranscriptionChoice {
                role,
                model,
                runs_on: backend,
                check: check_verdict(check, model, backend),
                languages: facts.languages,
                download_size: facts.size,
            })
        })
        .collect()
}

/// What the Models page shows before any download.
#[derive(Debug, Clone, PartialEq)]
pub struct ModelsAdvice {
    /// The transcription model for the meeting languages (`None`: no model covers them).
    pub transcription: Option<Recommended>,
    /// The T7.4 choices.
    pub choices: Vec<TranscriptionChoice>,
    /// The local summary model and why.
    pub summary: SummaryModelChoice,
    /// Whether the summary would run on the graphics card.
    pub summary_on_gpu: bool,
}

/// The whole recommendation (module docs). `check` is the stored model check when it is
/// current for this machine and `languages`, else `None`.
///
/// # Errors
/// [`RuntimeError::UnknownModel`] if the summary rule names a model missing from
/// [`summary_model::SUMMARY_MODELS`] (a bug, pinned by test).
pub fn advise(
    hardware: &Hardware,
    order: &[Accelerator],
    languages: &[&str],
    check: Option<&StoredCheck>,
) -> Result<ModelsAdvice, RuntimeError> {
    let summary_on_gpu = crate::model_settings::summary_placement(
        &crate::model_settings::ModelPick::Automatic,
        crate::model_settings::Part::Automatic,
        order,
        hardware,
    )?
    .gpu;
    let summary = summary_model::choose_summary_model_on(
        None,
        hardware.total_memory,
        summary_on_gpu,
        crate::model_settings::graphics_memory(hardware),
    )?;
    Ok(ModelsAdvice {
        transcription: recommend_transcription(hardware, order, languages),
        choices: transcription_choices(hardware, order, check),
        summary,
        summary_on_gpu,
    })
}
