//! The double-talk renders of `asr.wer_aec_delta` (T2.4g), shared by path between
//! `asr_eval` (the informational base.en value) and earshot-runtime's `m2_prod` (the metric
//! on the production setup), so both score exactly the same scenarios,
//! seeds, canceller runs and utterances.
//!
//! - Speech: the smoke pool exactly as `accept m1` (aec_eval) builds it: smoke clips in name
//!   order, speaker 61 far end, speaker 121 near end ([`aec_pool`]). Near-end utterances are
//!   identified by exact clip length, so the near-end clips must have distinct lengths.
//! - Scenarios ([`AEC_SCENARIOS`]): the gating double-talk scenarios at SER 0 and SER -10 of
//!   `acceptance/aec_scenarios.toml`, simulated as `accept m1` does and run through the echo
//!   canceller (`EchoCancellerConfig::default()`, output latency removed by `run_canceller`).
//! - Seeds ([`aec_renders`], `refs::render_seeds`): each scenario's own seed, then
//!   `seed + k * AEC_SEED_STEP`, until the complete near-end utterances hold at least
//!   [`AEC_MIN_WORDS`] reference words, counted from the simulations before any
//!   transcription. An utterance cut off by the end of the call is not scored.

use std::ops::Range;

use earshot_core::aec::EchoCancellerConfig;
use earshot_core::aec::eval::run_canceller;
use earshot_core::metrics::wer::word_errors_normalized;
use earshot_core::sim::{ScenarioFile, SimOutput, SpeechPool, simulate};
use rand_chacha::ChaCha8Rng;
use rand_core::SeedableRng;

/// Double-talk scenarios of `asr.wer_aec_delta` with their required SER (dB); SER 0 gates,
/// SER -10 is informational (PLAN.md 4.2).
pub const AEC_SCENARIOS: [(&str, f64); 2] =
    [("double-talk-ser-0", 0.0), ("double-talk-ser-10", -10.0)];
/// Seed rule (T2.4g, fixed before measuring): see the module docs; more than
/// `AEC_MAX_SEEDS` seeds is an error. One seed held 35 words, so one recognition error was
/// 2.9 points.
pub const AEC_SEED_STEP: u64 = 1_000;
/// Reference words the renders of one scenario must hold.
pub const AEC_MIN_WORDS: usize = 300;
/// Most seeds per scenario.
pub const AEC_MAX_SEEDS: usize = 32;
/// Scoring window margin before the first and after the last near-end utterance (s).
pub const AEC_EDGE_S: f64 = 0.5;

/// One smoke clip with its transcript.
pub struct SmokeClip {
    /// LibriSpeech utterance id.
    pub id: String,
    /// Reference transcript.
    pub reference: String,
    /// 16 kHz mono audio.
    pub audio: Vec<f32>,
}

/// The speech pool of the double-talk scenarios.
pub struct AecPool {
    /// Far-end utterances (speaker 61), in name order.
    pub far: Vec<Vec<f32>>,
    /// Near-end utterances (speaker 121), in name order.
    pub near: Vec<SmokeClip>,
}

/// The pool exactly as `accept m1` builds it (module docs).
///
/// # Errors
/// Two near-end clips of the same length (utterances could not be identified).
pub fn aec_pool(mut smoke: Vec<SmokeClip>) -> Result<AecPool, String> {
    smoke.sort_by(|a, b| a.id.cmp(&b.id));
    let far = smoke
        .iter()
        .filter(|u| u.id.starts_with("61-"))
        .map(|u| u.audio.clone())
        .collect();
    let near: Vec<SmokeClip> = smoke
        .into_iter()
        .filter(|u| u.id.starts_with("121-"))
        .collect();
    for (i, u) in near.iter().enumerate() {
        if near
            .iter()
            .skip(i + 1)
            .any(|v| v.audio.len() == u.audio.len())
        {
            return Err(format!(
                "near-end clip {} has the same length as another; cannot identify utterances",
                u.id
            ));
        }
    }
    Ok(AecPool { far, near })
}

/// One render of a scenario: the simulation, the canceller output and the complete
/// near-end utterances it holds.
pub struct AecRender {
    /// The simulation (`mic`, `near_clean`, `near_regions`, ...).
    pub sim: SimOutput,
    /// Canceller output, aligned with the mic (a whole number of blocks long).
    pub out: Vec<f32>,
    /// Spans of the complete near-end utterances, in time order.
    pub spans: Vec<Range<usize>>,
    /// For each span, its clip (index into [`AecPool::near`]).
    pub clips: Vec<usize>,
}

/// The renders of scenario `name` (a gating double-talk scenario at SER `ser` dB) by the
/// seed rule (module docs), with their seeds.
///
/// # Errors
/// A missing or non-gating scenario, a simulation error, a near-end region that matches no
/// clip, or too few words within [`AEC_MAX_SEEDS`] seeds.
pub fn aec_renders(
    file: &ScenarioFile,
    name: &str,
    ser: f64,
    pool: &AecPool,
) -> Result<Vec<(u64, AecRender)>, String> {
    let scn = file
        .get(name)
        .ok_or_else(|| format!("scenario {name} missing"))?;
    if scn.informational || !scn.near_speech || !scn.far_speech || !scn.echo || scn.ser_db != ser {
        return Err(format!(
            "scenario {name} is not a gating double-talk scenario at SER {ser} dB"
        ));
    }
    let near_audio: Vec<Vec<f32>> = pool.near.iter().map(|u| u.audio.clone()).collect();
    let speech = SpeechPool {
        far: &pool.far,
        near: &near_audio,
    };
    let mut error = None;
    let renders = super::refs::render_seeds(
        scn.seed,
        AEC_SEED_STEP,
        AEC_MIN_WORDS,
        AEC_MAX_SEEDS,
        |seed| {
            let mut rng = ChaCha8Rng::seed_from_u64(seed);
            let sim = match simulate(scn, &file.setup, &speech, &mut rng) {
                Ok(sim) => sim,
                Err(e) => {
                    error.get_or_insert(format!("{name} seed {seed}: {e}"));
                    return (None, AEC_MIN_WORDS);
                }
            };
            let run = run_canceller(&sim, EchoCancellerConfig::default());
            let n = run.out.len();
            let (mut spans, mut clips) = (Vec::new(), Vec::new());
            for r in &sim.near_regions {
                if r.end >= sim.mic.len() || r.end > n {
                    continue; // cut off by the end of the call
                }
                let Some(clip) = pool.near.iter().position(|u| u.audio.len() == r.len()) else {
                    error.get_or_insert(format!(
                        "{name} seed {seed}: near-end region {r:?} matches no smoke clip"
                    ));
                    return (None, AEC_MIN_WORDS);
                };
                spans.push(r.clone());
                clips.push(clip);
            }
            let words = clips
                .iter()
                .map(|&c| {
                    let reference = &pool.near[c].reference;
                    word_errors_normalized(reference, reference).ref_words
                })
                .sum::<usize>();
            let render = AecRender {
                sim,
                out: run.out,
                spans,
                clips,
            };
            (Some(render), words)
        },
    );
    if let Some(e) = error {
        return Err(e);
    }
    renders
        .map_err(|e| format!("{name}: {e}"))?
        .into_iter()
        .map(|(seed, r)| {
            r.map(|r| (seed, r))
                .ok_or_else(|| format!("{name}: no render"))
        })
        .collect()
}
