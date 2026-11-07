//! Chirp analysis on simulated inputs: the chirps played into the simulator's shoebox room
//! (`sim::image_source_rir`) with a known bulk delay, gain and noise.

use rand_chacha::ChaCha8Rng;
use rand_core::{Rng, SeedableRng};

use super::*;
use crate::dsp::fft_filter;
use crate::sim::{Room, direct_delay_samples, image_source_rir};

const ROOM: Room = Room {
    dims: [4.0, 3.5, 2.7],
    absorption: 0.35,
};
const SPEAKER: [f64; 3] = [1.0, 1.2, 0.9];
const MIC: [f64; 3] = [1.3, 1.25, 0.95];

/// White noise at `rms`.
fn noise(n: usize, rms: f64, seed: u64) -> Vec<f32> {
    let mut rng = ChaCha8Rng::seed_from_u64(seed);
    (0..n)
        .map(|_| {
            // Sum of 12 uniforms minus 6: unit-variance, close to Gaussian.
            let s: f64 = (0..12)
                .map(|_| f64::from(rng.next_u32()) / f64::from(u32::MAX))
                .sum();
            ((s - 6.0) * rms) as f32
        })
        .collect()
}

struct Sim {
    reference: Vec<f32>,
    mic: Vec<f32>,
    echo: Vec<f32>,
    /// Bulk delay plus the direct path (samples).
    truth: f64,
}

/// The test signal through the room: `bulk` samples of device delay, the echo scaled by
/// `gain`, plus noise at `noise_rms`.
fn simulate(bulk: usize, gain: f32, noise_rms: f64, seed: u64) -> Sim {
    let reference = test_signal(PIPELINE_RATE);
    let rir = image_source_rir(&ROOM, SPEAKER, MIC, PIPELINE_RATE, 4000);
    let mut delayed = vec![0.0; reference.len()];
    for (i, v) in reference.iter().enumerate() {
        if i + bulk < delayed.len() {
            delayed[i + bulk] = *v;
        }
    }
    let echo: Vec<f32> = fft_filter(&delayed, &rir)
        .into_iter()
        .map(|v| v * gain)
        .collect();
    let n = noise(reference.len(), noise_rms, seed);
    let mic = echo.iter().zip(&n).map(|(e, v)| e + v).collect();
    Sim {
        reference,
        mic,
        echo,
        truth: bulk as f64 + direct_delay_samples(SPEAKER, MIC, PIPELINE_RATE),
    }
}

fn echo_of(r: &CheckResult) -> (f64, f64, f64) {
    match r.outcome {
        Outcome::Echo {
            delay_samples,
            echo_db,
            echo_dbfs,
        } => (delay_samples, echo_db, echo_dbfs),
        ref other => panic!("expected an echo, got {other:?} ({r:?})"),
    }
}

#[test]
fn audio_check_signal_is_four_chirps_in_the_stated_duration() {
    for rate in [SampleRate::HZ_16000, SampleRate::HZ_48000] {
        let s = test_signal(rate);
        assert_eq!(s.len(), rate.frames_for(test_signal_duration()));
        let peak = s.iter().fold(0.0f32, |m, v| m.max(v.abs()));
        assert!(peak <= CHIRP_PEAK && peak > 0.99 * CHIRP_PEAK, "{peak}");
        // Silent before the first chirp and after the last one.
        let lead = rate.frames_for(LEAD_IN);
        assert!(s[..lead].iter().all(|&v| v == 0.0));
        let end = lead + 3 * rate.frames_for(CHIRP_PERIOD) + rate.frames_for(CHIRP_LEN);
        assert!(s[end..].iter().all(|&v| v == 0.0));
        // Faded: the first and last samples of a chirp are (near) zero.
        for rising in [true, false] {
            let c = chirp(rate, rising);
            assert!(c[0].abs() < 1e-6 && c[c.len() - 1].abs() < 0.01);
        }
    }
}

#[test]
fn audio_check_finds_the_delay_through_a_room() {
    // Device delays from none to past the canceller's 600 ms range.
    for (i, bulk_ms) in [0usize, 37, 120, 250, 590, 900].into_iter().enumerate() {
        let bulk = bulk_ms * 16;
        let sim = simulate(bulk, 0.5, 1e-3, i as u64);
        let r = analyse(&sim.reference, &sim.mic, false);
        let (delay, _, _) = echo_of(&r);
        assert!(
            (delay - sim.truth).abs() <= 16.0,
            "{bulk_ms} ms: measured {delay}, truth {}",
            sim.truth
        );
        assert!(!r.too_quiet && !r.clipping, "{r:?}");
        assert_eq!(r.stretches.len(), CHIRP_COUNT);
        let ms = r.delay_ms().unwrap();
        assert!((ms - sim.truth / 16.0).abs() <= 1.0, "{ms}");
    }
}

#[test]
fn audio_check_echo_level_matches_the_simulated_echo() {
    for (gain, seed) in [(1.0f32, 1u64), (0.3, 2), (0.05, 3)] {
        let sim = simulate(1600, gain, 3e-4, seed);
        let r = analyse(&sim.reference, &sim.mic, false);
        let (delay, echo_db, echo_dbfs) = echo_of(&r);
        // Truth: the clean echo's power in the same windows over the played power.
        let d = delay as usize;
        let w = PIPELINE_RATE.frames_for(CHIRP_LEN);
        let (mut e, mut p) = (0.0, 0.0);
        for s in &r.stretches {
            e += power(&sim.echo[s.start + d..s.start + d + w]);
            p += power(&sim.reference[s.start..s.start + w]);
        }
        let truth = db(e) - db(p);
        assert!(
            (echo_db - truth).abs() <= 1.0,
            "gain {gain}: {echo_db} vs {truth}"
        );
        assert!((echo_dbfs - db(e / 4.0)).abs() <= 1.0, "{echo_dbfs}");
    }
}

#[test]
fn audio_check_hears_an_echo_below_the_noise() {
    // Echo RMS about 10 dB below the noise: the matched filter's gain still finds it.
    let probe = simulate(800, 1.0, 0.0, 0);
    let echo_rms = power(&probe.echo).sqrt();
    let sim = simulate(800, 1.0, echo_rms * 3.16, 7);
    let r = analyse(&sim.reference, &sim.mic, false);
    let (delay, _, _) = echo_of(&r);
    assert!(
        (delay - sim.truth).abs() <= 16.0,
        "{delay} vs {}",
        sim.truth
    );
}

#[test]
fn audio_check_no_echo_is_headphones() {
    // The mic hears only the room's noise: no stretch agrees, nothing is invented.
    for seed in 0..8 {
        let reference = test_signal(PIPELINE_RATE);
        let mic = noise(reference.len(), 3e-3, 100 + seed);
        let r = analyse(&reference, &mic, false);
        assert_eq!(r.outcome, Outcome::NoEcho, "seed {seed}: {r:?}");
        assert_eq!(r.delay_hint(9_600), None);
        assert!(!r.too_quiet);
        assert!((r.noise_dbfs - db(9e-6)).abs() < 2.0, "{}", r.noise_dbfs);
    }
}

#[test]
fn audio_check_unrelated_sound_at_the_mic_is_not_an_echo() {
    // Someone else's chirps at other times (a second device) do not agree on one lag.
    let reference = test_signal(PIPELINE_RATE);
    let mut mic = noise(reference.len(), 1e-3, 9);
    let one = chirp(PIPELINE_RATE, true);
    for (k, at) in [2000usize, 9000, 21000, 30000].into_iter().enumerate() {
        let g = 0.2 + 0.1 * k as f32;
        for (i, v) in one.iter().enumerate() {
            mic[at + i] += v * g;
        }
    }
    let r = analyse(&reference, &mic, false);
    assert_eq!(r.outcome, Outcome::NoEcho, "{r:?}");
}

#[test]
fn audio_check_silent_loopback_is_not_played() {
    let n = PIPELINE_RATE.frames_for(test_signal_duration());
    let reference = noise(n, 1e-4, 1); // -80 dBFS: nothing played.
    let mic = noise(n, 1e-3, 2);
    let r = analyse(&reference, &mic, false);
    assert_eq!(r.outcome, Outcome::NotPlayed);
    let r = analyse(&vec![0.0; n], &mic, false);
    assert_eq!(r.outcome, Outcome::NotPlayed);
    assert_eq!(analyse(&[], &mic, false).outcome, Outcome::NotPlayed);
}

#[test]
fn audio_check_silent_mic_is_too_quiet() {
    let reference = test_signal(PIPELINE_RATE);
    let r = analyse(&reference, &vec![0.0; reference.len()], false);
    assert_eq!(r.outcome, Outcome::MicSilent);
    assert!(r.too_quiet);
    let r = analyse(&reference, &[], false);
    assert_eq!(r.outcome, Outcome::MicSilent);
}

#[test]
fn audio_check_faint_echo_is_too_quiet_and_gives_no_hint() {
    // Echo around -70 dBFS over a very quiet mic: heard, but too faint to trust.
    let sim = simulate(1600, 0.003, 1e-5, 4);
    let r = analyse(&sim.reference, &sim.mic, false);
    let (delay, _, echo_dbfs) = echo_of(&r);
    assert!(echo_dbfs < TOO_QUIET_DBFS, "{echo_dbfs}");
    assert!(r.too_quiet);
    assert!((delay - sim.truth).abs() <= 16.0);
    assert_eq!(r.delay_hint(9_600), None);
    // The same path at a normal level gives the hint.
    let loud = simulate(1600, 0.5, 1e-5, 4);
    let r = analyse(&loud.reference, &loud.mic, false);
    let hint = r.delay_hint(9_600).unwrap();
    assert!((hint - loud.truth).abs() <= 16.0);
}

#[test]
fn audio_check_delay_past_the_search_range_gives_no_hint() {
    let sim = simulate(900 * 16, 0.5, 1e-3, 5);
    let r = analyse(&sim.reference, &sim.mic, false);
    let (delay, _, _) = echo_of(&r);
    assert!(delay > 9_600.0);
    assert_eq!(r.delay_hint(9_600), None);
    assert!(r.delay_hint(20_000).is_some());
}

#[test]
fn audio_check_clipping_is_a_run_at_full_scale() {
    let mut c = ClipDetector::new();
    c.push(&[0.5, 1.0, -1.0, 0.2, 1.0]);
    assert!(!c.clipped(), "two in a row is a peak");
    c.push(&[1.0, 0.9995]);
    assert!(c.clipped(), "a run across pushes");
    let mut c = ClipDetector::new();
    c.push(&[-1.0, -1.0, -1.0]);
    assert!(c.clipped());
    // The host's verdict is carried through whatever the outcome.
    let reference = test_signal(PIPELINE_RATE);
    let mic = noise(reference.len(), 1e-3, 3);
    assert!(analyse(&reference, &mic, true).clipping);
    assert!(!analyse(&reference, &mic, false).clipping);
}

#[test]
fn audio_check_speech_reference_gives_the_delay() {
    // Any played signal with loud stretches works (a recorded call's far end, here a smoke
    // utterance), which is what the recorded-input test relies on.
    let (far, _) = crate::testutil::smoke_speech();
    let speech: Vec<f32> = far.iter().take(2).flatten().copied().collect();
    let rir = image_source_rir(&ROOM, SPEAKER, MIC, PIPELINE_RATE, 4000);
    let bulk = 3200;
    let mut delayed = vec![0.0; speech.len() + bulk];
    delayed[bulk..].copy_from_slice(&speech);
    let mut mic = fft_filter(&delayed, &rir);
    for (m, n) in mic.iter_mut().zip(noise(delayed.len(), 1e-3, 11)) {
        *m = *m * 0.4 + n;
    }
    let mut reference = speech;
    reference.resize(mic.len(), 0.0);
    let r = analyse(&reference, &mic, false);
    let (delay, _, _) = echo_of(&r);
    let truth = bulk as f64 + direct_delay_samples(SPEAKER, MIC, PIPELINE_RATE);
    assert!((delay - truth).abs() <= 16.0, "{delay} vs {truth}");
}
