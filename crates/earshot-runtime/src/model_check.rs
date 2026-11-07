//! The first-run model check's rules (T6.10c1): which transcription model a machine should
//! use, decided by a short timed test rather than by reputation. Pure: the hardware read,
//! the test clip and the timed run are T6.10c2; this module only decides.
//!
//! - **Data**: the T6.10e ladders (per backend, best first, each the fallback of the one
//!   before; English and multilingual) and each model's language coverage and download size,
//!   mirrored from `models/manifest.toml` (`[[ladder]]`, `[[model]]`) and pinned to it by a
//!   test.
//! - **Languages** ([`candidates`]): English-only meetings use the English ladder; any other
//!   set of meeting languages uses the multilingual ladder's models that cover all of them.
//! - **Where to start** ([`starting_rung`]): the hardware facts only choose the first rung to
//!   test; the test decides.
//! - **Verdict** ([`judge`]): a two-track run of the bundled clip is too slow above
//!   [`RTF_LIMIT`], wrong above [`WER_LIMIT_PERCENT`], and comfortably fast at or below
//!   [`RTF_COMFORTABLE`]. All three were fixed before any measurement.
//! - **Search** ([`ModelSearch`]): comfortably fast and correct -> try one rung up; passing
//!   but not comfortable -> settled; too slow -> one rung down; wrong output -> the next
//!   backend (garbage from a GPU or NPU means the backend is broken, and a smaller model on
//!   it would be no better); a backend with no passing rung -> the next backend. Stops as
//!   soon as the answer is clear. Vulkan (T6.10d; whisper only, D20) is kept only if it beats
//!   the CPU: after Vulkan settles, the CPU is searched too, and the CPU's answer wins unless
//!   Vulkan's model is more accurate ([`QUALITY_ORDER`]) or the same model measured faster.
//!   The CPU is not tested when none of its installed models could beat the Vulkan answer.
//! - **Recommendation** ([`Recommendation`]): the best passing model, the next installed
//!   rung below it on the same backend as the automatic fallback when a meeting falls behind,
//!   and the better models of the ladder that are not installed, with their download size.

use earshot_core::asr::BackendKind;

use crate::backends::live_whisper_threads;

/// A two-track run slower than this (decode time over audio time, the slower track) is too
/// slow: a meeting needs headroom for diarisation and for the call app itself.
pub const RTF_LIMIT: f64 = 0.5;
/// A run at or below this is comfortably fast: the next rung up is worth a test.
pub const RTF_COMFORTABLE: f64 = 0.2;
/// A run with a higher word error rate on the clip (both tracks pooled) is wrong output: a
/// working model of the ladders scores far below it on clean read speech, so above it the
/// backend produces garbage.
pub const WER_LIMIT_PERCENT: f64 = 25.0;

const _: () = assert!(
    RTF_COMFORTABLE < RTF_LIMIT,
    "comfortable must be faster than the limit"
);

/// Which meetings a ladder serves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    /// English meetings.
    English,
    /// Meetings in other languages (or several).
    Multilingual,
}

/// The models of one backend and scope, best first.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ladder {
    /// The backend.
    pub backend: BackendKind,
    /// The scope.
    pub scope: Scope,
    /// Model names, best first.
    pub models: &'static [&'static str],
}

/// The T6.10e ladders (`models/manifest.toml` `[[ladder]]`, docs/asr-models.md).
pub const LADDERS: &[Ladder] = &[
    Ladder {
        backend: BackendKind::Cuda,
        scope: Scope::English,
        models: &["parakeet-tdt-0.6b-v2", "parakeet-tdt-110m", "base.en"],
    },
    Ladder {
        backend: BackendKind::Cuda,
        scope: Scope::Multilingual,
        models: &["parakeet-tdt-0.6b-v3", "large-v3-turbo"],
    },
    // Vulkan runs whisper.cpp only (ONNX Runtime has no Vulkan provider here), so its ladders
    // are the best whisper models of T6.10e that invent nothing on silence (English) and the
    // multilingual GPU model (T6.10d).
    Ladder {
        backend: BackendKind::Vulkan,
        scope: Scope::English,
        models: &["small.en", "base.en"],
    },
    Ladder {
        backend: BackendKind::Vulkan,
        scope: Scope::Multilingual,
        models: &["large-v3-turbo"],
    },
    // The Surface NPU (T8.3e, GATE-03a): the converted Parakeet models only (whisper.cpp has
    // no QNN backend); both ran at rtf 0.02 with WER within 0.61 points of the float model on
    // the CPU (docs/model-eval/qnn-surface.json, cpu-surface.json). English only: no
    // multilingual model is converted.
    Ladder {
        backend: BackendKind::Qnn,
        scope: Scope::English,
        models: &["parakeet-tdt-0.6b-v2", "parakeet-tdt-110m"],
    },
    Ladder {
        backend: BackendKind::Cpu,
        scope: Scope::English,
        models: &["parakeet-tdt-0.6b-v2", "parakeet-tdt-110m", "base.en"],
    },
    Ladder {
        backend: BackendKind::Cpu,
        scope: Scope::Multilingual,
        models: &["parakeet-tdt-0.6b-v3"],
    },
];

/// A transcription model's coverage and download size (`models/manifest.toml` `[[model]]`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModelFacts {
    /// The model name.
    pub name: &'static str,
    /// Language codes it transcribes.
    pub languages: &'static [&'static str],
    /// Bytes of all its files.
    pub size: u64,
}

const EN: &[&str] = &["en"];
const PARAKEET_V3: &[&str] = &[
    "bg", "hr", "cs", "da", "nl", "en", "et", "fi", "fr", "de", "el", "hu", "it", "lv", "lt", "mt",
    "pl", "pt", "ro", "sk", "sl", "es", "sv", "ru", "uk",
];
const WHISPER: &[&str] = &[
    "en", "zh", "de", "es", "ru", "ko", "fr", "ja", "pt", "tr", "pl", "ca", "nl", "ar", "sv", "it",
    "id", "hi", "fi", "vi", "he", "uk", "el", "ms", "cs", "ro", "da", "hu", "ta", "no", "th", "ur",
    "hr", "bg", "lt", "la", "mi", "ml", "cy", "sk", "te", "fa", "lv", "bn", "sr", "az", "sl", "kn",
    "et", "mk", "br", "eu", "is", "hy", "ne", "mn", "bs", "kk", "sq", "sw", "gl", "mr", "pa", "si",
    "km", "sn", "yo", "so", "af", "oc", "ka", "be", "tg", "sd", "gu", "am", "yi", "lo", "uz", "fo",
    "ht", "ps", "tk", "nn", "mt", "sa", "lb", "my", "bo", "tl", "mg", "as", "tt", "haw", "ln",
    "ha", "ba", "jw", "su", "yue",
];

/// Every transcription model of the manifest.
pub const MODELS: &[ModelFacts] = &[
    ModelFacts {
        name: "tiny.en",
        languages: EN,
        size: 77_704_715,
    },
    ModelFacts {
        name: "base.en",
        languages: EN,
        size: 147_964_211,
    },
    ModelFacts {
        name: "small.en",
        languages: EN,
        size: 487_614_201,
    },
    ModelFacts {
        name: "small.en-q5_1",
        languages: EN,
        size: 190_098_681,
    },
    ModelFacts {
        name: "large-v3-turbo",
        languages: WHISPER,
        size: 1_624_555_275,
    },
    ModelFacts {
        name: "large-v3-turbo-q5_0",
        languages: WHISPER,
        size: 574_041_195,
    },
    ModelFacts {
        name: "parakeet-ctc-110m",
        languages: EN,
        size: 458_170_974,
    },
    ModelFacts {
        name: "parakeet-tdt-110m",
        languages: EN,
        size: 477_410_591,
    },
    ModelFacts {
        name: "parakeet-tdt-0.6b-v2",
        languages: EN,
        size: 2_512_987_040,
    },
    ModelFacts {
        name: "parakeet-tdt-0.6b-v3",
        languages: PARAKEET_V3,
        size: 2_549_800_429,
    },
];

/// Every manifest model, most accurate first: the greedy word error rate on meeting audio
/// (`docs/model-eval/cuda.json` `corpora.ami`: AMI IHM with oracle segments, the `accept m2`
/// rule, measured 2026-10-24; accuracy does not depend on the backend), pinned to that report
/// by a test. It decides whether a Vulkan answer beats the CPU's ([`ModelSearch`]), where the
/// two ladders hold different models. Until 2026-10-24 it followed the merged Mix-Headset
/// stream (T6.10e), which ranked large-v3-turbo second and small.en above small.en-q5_1.
pub const QUALITY_ORDER: &[&str] = &[
    "parakeet-tdt-0.6b-v2",
    "parakeet-tdt-0.6b-v3",
    "parakeet-tdt-110m",
    "parakeet-ctc-110m",
    "small.en-q5_1",
    "small.en",
    "large-v3-turbo",
    "large-v3-turbo-q5_0",
    "base.en",
    "tiny.en",
];

/// The position of `model` in [`QUALITY_ORDER`] (lower is better), `usize::MAX` if unranked.
#[must_use]
pub fn quality_rank(model: &str) -> usize {
    QUALITY_ORDER
        .iter()
        .position(|m| *m == model)
        .unwrap_or(usize::MAX)
}

/// The facts of model `name`, if it is a manifest model.
#[must_use]
pub fn model_facts(name: &str) -> Option<&'static ModelFacts> {
    MODELS.iter().find(|m| m.name == name)
}

/// Whether model `name` transcribes every language in `languages`.
#[must_use]
pub fn covers(name: &str, languages: &[&str]) -> bool {
    model_facts(name).is_some_and(|m| languages.iter().all(|l| m.languages.contains(l)))
}

/// A meeting language as the ladders list it: lower case, the region dropped (`en-US` ->
/// `en`).
#[must_use]
pub fn language_code(language: &str) -> String {
    language
        .split(['-', '_'])
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase()
}

/// The models to consider on `backend` for meetings in `languages` (empty: English; codes
/// normalised by [`language_code`]), best first: the English ladder for English-only
/// meetings, else the multilingual ladder's models that cover every language. Empty when the
/// backend has no ladder.
#[must_use]
pub fn candidates(backend: BackendKind, languages: &[&str]) -> Vec<&'static str> {
    let codes: Vec<String> = languages.iter().map(|l| language_code(l)).collect();
    let languages: Vec<&str> = codes.iter().map(String::as_str).collect();
    let languages = languages.as_slice();
    let english = languages.is_empty() || languages.iter().all(|l| *l == "en");
    let scope = if english {
        Scope::English
    } else {
        Scope::Multilingual
    };
    let wanted: &[&str] = if english { EN } else { languages };
    LADDERS
        .iter()
        .filter(|l| l.backend == backend && l.scope == scope)
        .flat_map(|l| l.models.iter().copied())
        .filter(|m| covers(m, wanted))
        // Whisper on CUDA in the shipped build: nothing to try (`crate::cuda`).
        .filter(|m| {
            crate::backends::Accelerator::of(backend)
                .is_none_or(|a| crate::backends::model_can_run_on(m, a))
        })
        .collect()
}

/// A GPU (or NPU) the hardware read found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Accelerator {
    /// Its backend.
    pub backend: BackendKind,
    /// Free memory on the device (bytes); `None` when the device does not report it (an
    /// NPU, or integrated graphics sharing system memory).
    pub free_memory: Option<u64>,
}

/// What the hardware read found (T6.10c2), without any network.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HardwareFacts {
    /// GPUs and NPUs this build can use, best backend first.
    pub accelerators: Vec<Accelerator>,
    /// Logical CPU cores.
    pub cores: usize,
    /// Total system memory (bytes).
    pub memory: u64,
    /// The GPU drivers' versions, sorted (`crate::model_bench::gpu_drivers`); empty where
    /// they are not read, `None` when reading them failed. Only the fingerprint uses them:
    /// a driver update re-runs the check, a failed read does not.
    pub drivers: Option<Vec<String>>,
}

/// A model needs this many times its file size in free memory to load and run with room to
/// spare (weights, activations, the second track).
pub const MEMORY_FACTOR: u64 = 2;
/// CPU threads per track a model over [`LARGE_MODEL`] needs to be worth starting at.
pub const LARGE_MODEL_THREADS: usize = 3;
/// A model this large (bytes) counts as large for [`starting_rung`].
pub const LARGE_MODEL: u64 = 1_000_000_000;

/// The backends to try, best first: every accelerator with a ladder, then the CPU.
#[must_use]
pub fn backend_order(facts: &HardwareFacts) -> Vec<BackendKind> {
    let mut order: Vec<BackendKind> = Vec::new();
    for a in &facts.accelerators {
        let has_ladder = LADDERS.iter().any(|l| l.backend == a.backend);
        if has_ladder && a.backend != BackendKind::Cpu && !order.contains(&a.backend) {
            order.push(a.backend);
        }
    }
    order.push(BackendKind::Cpu);
    order
}

/// The first rung of `models` (best first) worth testing on `backend`: the first model whose
/// size fits the device's free memory (or system memory on the CPU) with
/// [`MEMORY_FACTOR`], and on the CPU, for a model over [`LARGE_MODEL`], only with at least
/// [`LARGE_MODEL_THREADS`] threads per track (the T6.10b budget for two tracks). The last
/// rung when nothing fits; `None` for an empty list.
#[must_use]
pub fn starting_rung(
    facts: &HardwareFacts,
    backend: BackendKind,
    models: &[&'static str],
) -> Option<usize> {
    if models.is_empty() {
        return None;
    }
    let device_memory = facts
        .accelerators
        .iter()
        .find(|a| a.backend == backend)
        .and_then(|a| a.free_memory);
    let per_track = live_whisper_threads(BackendKind::Cpu, facts.cores, 2).get();
    let fits = |m: &str| {
        let size = model_facts(m).map_or(u64::MAX, |f| f.size);
        let need = size.saturating_mul(MEMORY_FACTOR);
        match backend {
            BackendKind::Cpu => {
                need <= facts.memory && (size < LARGE_MODEL || per_track >= LARGE_MODEL_THREADS)
            }
            _ => need <= device_memory.unwrap_or(facts.memory),
        }
    };
    Some(
        models
            .iter()
            .position(|m| fits(m))
            .unwrap_or(models.len() - 1),
    )
}

/// One timed run of the test clip.
#[derive(Debug, Clone, PartialEq)]
pub struct Trial {
    /// The model.
    pub model: &'static str,
    /// The backend it ran on (proven by the backend evidence).
    pub backend: BackendKind,
    /// Decode time over audio time of the slower track, both tracks running at once.
    pub rtf: f64,
    /// Word error rate on the clip, both tracks pooled (percent).
    pub wer_percent: f64,
}

/// What a trial says.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// Correct and fast enough; `comfortable` when there is room for a bigger model.
    Pass {
        /// At or below [`RTF_COMFORTABLE`].
        comfortable: bool,
    },
    /// Correct but slower than [`RTF_LIMIT`].
    TooSlow,
    /// More than [`WER_LIMIT_PERCENT`] wrong (or not a number): the backend is broken.
    Wrong,
}

/// The verdict on `trial` (module docs).
#[must_use]
pub fn judge(trial: &Trial) -> Verdict {
    if !trial.wer_percent.is_finite() || trial.wer_percent > WER_LIMIT_PERCENT {
        Verdict::Wrong
    } else if !trial.rtf.is_finite() || trial.rtf > RTF_LIMIT {
        Verdict::TooSlow
    } else {
        Verdict::Pass {
            comfortable: trial.rtf <= RTF_COMFORTABLE,
        }
    }
}

/// A better model of the ladder that is not installed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Missing {
    /// The model.
    pub model: &'static str,
    /// Its download size (bytes).
    pub size: u64,
}

/// What the check recommends.
#[derive(Debug, Clone, PartialEq)]
pub struct Recommendation {
    /// The model to use.
    pub model: &'static str,
    /// The backend it passed on.
    pub backend: BackendKind,
    /// The automatic fallback when a meeting falls behind: the next installed rung below on
    /// the same backend.
    pub fallback: Option<&'static str>,
    /// Better models of that ladder that are not installed (offered for download).
    pub missing: Vec<Missing>,
}

/// The next step of the search.
#[derive(Debug, Clone, PartialEq)]
pub enum Step {
    /// Run the test clip with this model on this backend and [`ModelSearch::record`] it.
    Test {
        /// The model.
        model: &'static str,
        /// The backend.
        backend: BackendKind,
    },
    /// Settled.
    Done(Option<Recommendation>),
}

/// One backend's plan: its ladder for the meeting languages (model, installed) and the
/// starting rung.
type Plan = (BackendKind, Vec<(&'static str, bool)>, Option<usize>);

/// What one backend's plan says on its own.
enum PlanStep {
    /// Nothing to test and nothing passed: the next backend.
    Skip,
    /// This test next.
    Test(Step),
    /// This backend's answer.
    Settled(Recommendation),
}

/// Whether `plan` has an installed model at least as accurate as `model` (only such a
/// model, or the same one faster, can beat it).
fn could_beat((_, ladder, _): &Plan, model: &str) -> bool {
    let rank = quality_rank(model);
    ladder
        .iter()
        .any(|(m, installed)| *installed && quality_rank(m) <= rank)
}

/// The result for `model` on `backend` among `trials` (`(model, backend, result)` in the
/// order they ran): the first trial of that pair, `None` when it was never tested.
///
/// The one rule for a pair tested twice, used by [`ModelSearch`] (verdicts and speeds)
/// and [`crate::model_bench::StoredCheck::verdict`]. The search never tests a pair again
/// once it has a verdict, so its decisions rest on the first trial; a later trial of the
/// same pair (only if a caller records one) is ignored by both, so a stored check reports
/// the verdict its answer was based on.
pub(crate) fn pair_result<'a, R>(
    trials: impl IntoIterator<Item = (&'a str, BackendKind, R)>,
    model: &str,
    backend: BackendKind,
) -> Option<R> {
    trials
        .into_iter()
        .find(|(m, b, _)| *m == model && *b == backend)
        .map(|(_, _, r)| r)
}

/// The search over backends and rungs (module docs).
#[derive(Debug, Clone)]
pub struct ModelSearch {
    /// Per backend, best first: the full ladder for the meeting languages and which of its
    /// models are installed.
    plans: Vec<Plan>,
    trials: Vec<(Trial, Verdict)>,
}

impl ModelSearch {
    /// A search for `facts`, meetings in `languages`, with `installed` saying which models are
    /// on the machine.
    #[must_use]
    pub fn new(
        facts: &HardwareFacts,
        languages: &[&str],
        installed: &dyn Fn(&str) -> bool,
    ) -> Self {
        let plans = backend_order(facts)
            .into_iter()
            .map(|backend| {
                let ladder: Vec<(&'static str, bool)> = candidates(backend, languages)
                    .into_iter()
                    .map(|m| (m, installed(m)))
                    .collect();
                let models: Vec<&'static str> = ladder.iter().map(|(m, _)| *m).collect();
                let start = starting_rung(facts, backend, &models);
                (backend, ladder, start)
            })
            .collect();
        Self {
            plans,
            trials: Vec::new(),
        }
    }

    /// Records a finished trial.
    pub fn record(&mut self, trial: Trial) {
        let verdict = judge(&trial);
        self.trials.push((trial, verdict));
    }

    /// The trials so far with their verdicts.
    #[must_use]
    pub fn trials(&self) -> &[(Trial, Verdict)] {
        &self.trials
    }

    fn verdict(&self, backend: BackendKind, model: &str) -> Option<Verdict> {
        pair_result(
            self.trials.iter().map(|(t, v)| (t.model, t.backend, *v)),
            model,
            backend,
        )
    }

    /// The next test, or the answer.
    ///
    /// The first backend that settles gives the answer, except Vulkan (T6.10d): a Vulkan
    /// answer is kept only if it beats the CPU's ([`ModelSearch::beats`]), so the backends
    /// after it are searched too, unless none of their installed models could beat it.
    #[must_use]
    pub fn next(&self) -> Step {
        let mut pending: Option<Recommendation> = None;
        for plan in &self.plans {
            if let Some(v) = &pending
                && !could_beat(plan, v.model)
            {
                continue;
            }
            match self.plan_step(plan) {
                PlanStep::Skip => {}
                PlanStep::Test(step) => return step,
                PlanStep::Settled(r) => {
                    if let Some(v) = pending.take() {
                        let keep = self.beats(&v, &r);
                        return Step::Done(Some(if keep { v } else { r }));
                    }
                    if r.backend == BackendKind::Vulkan {
                        pending = Some(r);
                    } else {
                        return Step::Done(Some(r));
                    }
                }
            }
        }
        Step::Done(pending)
    }

    /// Whether answer `a` beats answer `b`: a more accurate model ([`QUALITY_ORDER`]), or
    /// the same model measured strictly faster. A tie keeps `b`.
    #[must_use]
    pub fn beats(&self, a: &Recommendation, b: &Recommendation) -> bool {
        let (ra, rb) = (quality_rank(a.model), quality_rank(b.model));
        if ra != rb {
            return ra < rb;
        }
        let rtf = |r: &Recommendation| {
            pair_result(
                self.trials.iter().map(|(t, _)| (t.model, t.backend, t.rtf)),
                r.model,
                r.backend,
            )
        };
        a.model == b.model && matches!((rtf(a), rtf(b)), (Some(x), Some(y)) if x < y)
    }

    /// One backend's next step on its own.
    fn plan_step(&self, (backend, ladder, start): &Plan) -> PlanStep {
        let backend = *backend;
        // Only installed models can be tested, in ladder order.
        let installed: Vec<(usize, &'static str)> = ladder
            .iter()
            .enumerate()
            .filter(|(_, (_, ok))| *ok)
            .map(|(i, (m, _))| (i, *m))
            .collect();
        let Some(start) = *start else {
            return PlanStep::Skip;
        };
        if installed.is_empty() {
            return PlanStep::Skip;
        }
        let verdicts: Vec<Option<Verdict>> = installed
            .iter()
            .map(|(_, m)| self.verdict(backend, m))
            .collect();
        // The best passing installed rung, if any (a wrong result one rung up, for
        // example a bigger model overflowing the GPU, still settles on it).
        if let Some(p) = verdicts
            .iter()
            .position(|v| matches!(v, Some(Verdict::Pass { .. })))
        {
            let comfortable = verdicts[p] == Some(Verdict::Pass { comfortable: true });
            if comfortable && p > 0 && verdicts[p - 1].is_none() {
                return PlanStep::Test(Step::Test {
                    model: installed[p - 1].1,
                    backend,
                });
            }
            return PlanStep::Settled(self.recommend(backend, ladder, &installed, p));
        }
        // Nothing passed: wrong output means a broken backend, so go to the next one.
        if verdicts.contains(&Some(Verdict::Wrong)) {
            return PlanStep::Skip;
        }
        // Otherwise the first untested installed rung at or below the start, stepping
        // down after each failure.
        let first = installed
            .iter()
            .position(|(i, _)| *i >= start)
            .unwrap_or(installed.len() - 1);
        match (first..installed.len()).find(|&k| verdicts[k].is_none()) {
            Some(k) => PlanStep::Test(Step::Test {
                model: installed[k].1,
                backend,
            }),
            None => PlanStep::Skip,
        }
    }

    fn recommend(
        &self,
        backend: BackendKind,
        ladder: &[(&'static str, bool)],
        installed: &[(usize, &'static str)],
        p: usize,
    ) -> Recommendation {
        let (rung, model) = installed[p];
        let missing = ladder[..rung]
            .iter()
            .filter(|(_, ok)| !ok)
            .map(|(m, _)| Missing {
                model: m,
                size: model_facts(m).map_or(0, |f| f.size),
            })
            .collect();
        Recommendation {
            model,
            backend,
            fallback: installed.get(p + 1).map(|(_, m)| *m),
            missing,
        }
    }
}

#[cfg(test)]
#[path = "model_check_tests.rs"]
mod tests;
