//! T2.3 accept: `cargo test -p earshot-onnx vad`.
//!
//! Silero VAD against the call simulator's labels (ground truth is `SimOutput::{far,near}_
//! regions` only, never another VAD). The labelled regions are whole utterance files, which
//! include each LibriSpeech clip's own leading/trailing silence and pauses, so they are exact
//! only for the silent gaps the simulator inserts between utterances. The accept metric
//! (decided by the owner, T2.3) is therefore:
//!
//! 1. per-frame accuracy >= 95% over the labelled silent gap frames between utterances, and
//! 2. a mute guard: every labelled utterance contains detected speech (an always-silent VAD
//!    fails), with
//! 3. full-span frame accuracy over all frames printed as informational output.
//!
//! A frame is labelled by its centre sample; it is speech when the probability is at least
//! the chunker's default `vad_threshold` (0.5).

mod common;

use std::ops::Range;

use earshot_core::asr::{AsrError, ChunkerConfig, Vad};
use earshot_onnx::silero::{SILERO_FRAME, SileroVad};
use earshot_onnx::{CPU_EP, SessionConfig};

/// PLAN.md T2.3 accept criterion on the gap frames.
const MIN_GAP_ACCURACY: f64 = 0.95;

fn silero() -> SileroVad {
    common::init_runtime();
    let config = SessionConfig {
        provider: common::test_provider(),
        intra_threads: Some(1),
        background: None,
    };
    SileroVad::load(&common::model(common::SILERO), &config).expect("load Silero")
}

fn threshold() -> f32 {
    ChunkerConfig::default().vad_threshold
}

fn centre(f: usize) -> usize {
    f * SILERO_FRAME + SILERO_FRAME / 2
}

fn probabilities(vad: &mut SileroVad, x: &[f32]) -> Vec<f32> {
    vad.reset();
    x.as_chunks::<SILERO_FRAME>()
        .0
        .iter()
        .map(|f| vad.speech_probability(f).expect("vad frame"))
        .collect()
}

#[derive(Debug)]
struct Score {
    /// Gap frames (labelled silence between the first and last utterance).
    gap_frames: usize,
    /// Gap frames classified as non-speech.
    gap_correct: usize,
    /// Labelled utterances.
    utterances: usize,
    /// Utterances containing at least one detected speech frame.
    utterances_detected: usize,
    /// All frames and frames whose decision matches the utterance-span labels.
    frames: usize,
    span_correct: usize,
}

impl Score {
    fn gap_accuracy(&self) -> f64 {
        self.gap_correct as f64 / self.gap_frames as f64
    }
    fn span_accuracy(&self) -> f64 {
        self.span_correct as f64 / self.frames as f64
    }
}

fn score(p: &[f32], regions: &[Range<usize>]) -> Score {
    let th = threshold();
    let first = regions.first().expect("regions").start;
    let last = regions.last().expect("regions").end;
    let speech = |f: usize| regions.iter().any(|r| r.contains(&centre(f)));
    let mut s = Score {
        gap_frames: 0,
        gap_correct: 0,
        utterances: regions.len(),
        utterances_detected: 0,
        frames: p.len(),
        span_correct: 0,
    };
    for (f, &pf) in p.iter().enumerate() {
        let detected = pf >= th;
        let labelled = speech(f);
        s.span_correct += usize::from(detected == labelled);
        let c = centre(f);
        if !labelled && c >= first && c < last {
            s.gap_frames += 1;
            s.gap_correct += usize::from(!detected);
        }
    }
    s.utterances_detected = regions
        .iter()
        .filter(|r| {
            p.iter()
                .enumerate()
                .any(|(f, &pf)| pf >= th && r.contains(&centre(f)))
        })
        .count();
    s
}

fn check(name: &str, s: &Score) {
    println!(
        "vad {name}: gap accuracy {:.4} ({}/{} gap frames); utterances detected {}/{}; \
         informational full-span accuracy {:.4} over {} frames",
        s.gap_accuracy(),
        s.gap_correct,
        s.gap_frames,
        s.utterances_detected,
        s.utterances,
        s.span_accuracy(),
        s.frames
    );
    assert!(
        s.gap_frames > 0 && s.utterances > 1,
        "{name}: no gaps to score"
    );
    assert!(
        s.gap_accuracy() >= MIN_GAP_ACCURACY,
        "{name}: gap-frame accuracy {:.4} < {MIN_GAP_ACCURACY}",
        s.gap_accuracy()
    );
    assert_eq!(
        s.utterances_detected, s.utterances,
        "{name}: mute guard: every labelled utterance must contain detected speech"
    );
}

#[test]
fn vad_silero_accept_on_simulated_far_end_concatenation() {
    let sim = common::run_scenario("far-only");
    let mut vad = silero();
    let p = probabilities(&mut vad, &sim.reference);
    check(
        "far-only reference (speaker 61)",
        &score(&p, &sim.far_regions),
    );
}

#[test]
fn vad_silero_accept_on_simulated_near_end_mic() {
    let sim = common::run_scenario("near-only");
    let mut vad = silero();
    let p = probabilities(&mut vad, &sim.mic);
    check(
        "near-only mic (speaker 121, room response)",
        &score(&p, &sim.near_regions),
    );
}

#[test]
fn vad_scoring_fails_an_always_silent_and_an_always_speech_detector() {
    let sim = common::run_scenario("far-only");
    let frames = sim.reference.len() / SILERO_FRAME;
    let silent = score(&vec![0.0; frames], &sim.far_regions);
    assert_eq!(
        silent.utterances_detected, 0,
        "mute guard must catch silence"
    );
    assert!(silent.gap_accuracy() >= MIN_GAP_ACCURACY);
    let always = score(&vec![1.0; frames], &sim.far_regions);
    assert_eq!(always.utterances_detected, always.utterances);
    assert!(
        always.gap_accuracy() < MIN_GAP_ACCURACY,
        "gap accuracy must catch always-speech"
    );
}

#[test]
fn vad_silero_is_stateful_and_reset_restores_the_initial_state() {
    let sim = common::run_scenario("far-only");
    let x = &sim.reference[..SILERO_FRAME * 200];
    let mut vad = silero();
    let first = probabilities(&mut vad, x);
    let again = probabilities(&mut vad, x);
    assert_eq!(first, again, "reset must make runs identical");
    // Without reset the recurrent state carries over, so a frame scored after speech
    // differs from the same frame scored from a cold start.
    let frame = &x[..SILERO_FRAME];
    vad.reset();
    let cold = vad.speech_probability(frame).unwrap();
    for f in x.as_chunks::<SILERO_FRAME>().0 {
        vad.speech_probability(f).unwrap();
    }
    let warm = vad.speech_probability(frame).unwrap();
    assert_ne!(cold, warm, "state must carry across frames");
}

#[test]
fn vad_silero_rejects_wrong_frame_length_and_reports_its_placement() {
    let mut vad = silero();
    assert_eq!(vad.frame_len(), 512);
    assert!(matches!(
        vad.speech_probability(&[0.0; 480]),
        Err(AsrError::Config(_))
    ));
    let b = vad.backend();
    println!("vad backend: {b}");
    assert!(b.kind().is_some(), "placement must be reported: {b}");
    if cfg!(feature = "cuda") {
        assert!(b.placement.nodes_on(earshot_onnx::CUDA_EP) > 0, "{b}");
    } else {
        assert!(b.placement.nodes_on(CPU_EP) > 0, "{b}");
        assert!(b.fully_on_requested(), "{b}");
    }
}

#[test]
fn vad_silero_id_names_the_model_file_by_its_hash() {
    // The manifest pin of silero/silero_vad.onnx starts 1a153a22f450.
    assert_eq!(silero().id(), "silero-vad@1a153a22f450");
    assert_ne!(silero().id(), earshot_core::asr::EnergyVad::default().id());
}

#[test]
fn vad_silero_silence_is_not_speech() {
    let mut vad = silero();
    let p = probabilities(&mut vad, &vec![0.0f32; SILERO_FRAME * 50]);
    assert!(p.iter().all(|&p| p < 0.1), "{p:?}");
}
