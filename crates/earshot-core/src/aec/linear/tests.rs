//! Linear-stage tests (PLAN.md T1.4 accept: aec.erle_linear and aec.convergence on
//! far-only). Thresholds come from `acceptance/thresholds.toml`.

use super::*;
use crate::metrics::erle::{
    CONVERGED_ERLE_DB, ConvergenceConfig, ErleConfig, segmental_erle, time_to_erle,
};
use crate::sim::{Scenario, SimOutput};
use crate::testutil::{run_scenario, threshold};

fn cancel(out: &SimOutput) -> (Vec<f32>, LinearAec) {
    let n = out.mic.len() / BLOCK * BLOCK;
    let mut aec = LinearAec::new(LinearConfig::default());
    let mut e = vec![0.0f32; n];
    aec.process(&out.mic[..n], &out.reference[..n], &mut e);
    (e, aec)
}

fn erle_linear(name: &str) -> (f64, Option<f64>) {
    let out = run_scenario(name);
    let (e, aec) = cancel(&out);
    let mic = &out.mic[..e.len()];
    let erle = segmental_erle(mic, &e, 16_000, &ErleConfig::default())
        .unwrap()
        .median_db;
    let conv = time_to_erle(
        mic,
        &e,
        16_000,
        0.0,
        CONVERGED_ERLE_DB,
        &ConvergenceConfig::default(),
    );
    eprintln!(
        "{name}: linear ERLE {erle:.1} dB, time to {CONVERGED_ERLE_DB} dB {conv:?} s, resets {}",
        aec.resets()
    );
    (erle, conv)
}

#[test]
fn aec_linear_far_only_meets_erle_linear() {
    // aec.erle_linear is the linear stage alone, scored without loudspeaker clipping
    // (far-only-linear; PLAN.md 4.0 as decided for T1.4). Under clipping the guard is
    // aec_linear_reaches_the_best_linear_canceller_under_clipping. aec.convergence is a
    // property of the whole canceller and is asserted on the EchoCanceller.
    let (erle, conv) = erle_linear("far-only-linear");
    let min_erle = threshold("aec.erle_linear");
    assert!(
        erle >= min_erle,
        "aec.erle_linear {erle:.2} dB < {min_erle}"
    );
    eprintln!("far-only-linear time to {CONVERGED_ERLE_DB} dB: {conv:?} s (informational)");
}

#[test]
fn aec_linear_handles_long_delay_and_drift() {
    let min_erle = threshold("aec.erle_linear");
    for name in [
        "delay-450ms",
        "drift-plus-200ppm",
        "drift-minus-200ppm",
        "delay-jump",
    ] {
        let (erle, _) = erle_linear(name);
        assert!(
            erle >= min_erle,
            "{name}: linear ERLE {erle:.2} dB < {min_erle}"
        );
    }
}

#[test]
fn aec_linear_does_not_adapt_without_an_echo_path() {
    // NoLock (no far end, or far end on headphones): no adaptation, output is the mic,
    // sample for sample (no added latency).
    for name in ["near-only", "headphones"] {
        let out = run_scenario(name);
        let (e, aec) = cancel(&out);
        assert_eq!(aec.delay_state(), DelayState::NoLock, "{name}");
        assert_eq!(aec.filter_energy(), 0.0, "{name}: filter adapted");
        assert_eq!(e[..], out.mic[..e.len()], "{name}: output differs from mic");
    }
}

#[test]
fn aec_linear_frozen_filter_does_not_change() {
    let out = run_scenario("far-only");
    let mut aec = LinearAec::new(LinearConfig::default());
    let n = 3 * 16_000 / BLOCK * BLOCK;
    let mut e = vec![0.0f32; n];
    aec.process(&out.mic[..n], &out.reference[..n], &mut e);
    let before = aec.filter_energy();
    assert!(before > 0.0, "adapted during the first 3 s");
    for (m, r) in out.mic[n..2 * n]
        .as_chunks::<BLOCK>()
        .0
        .iter()
        .zip(out.reference[n..2 * n].as_chunks::<BLOCK>().0)
    {
        let b = aec.process_block(m, r, 0.0);
        assert!(b.active);
    }
    assert_eq!(aec.filter_energy(), before);
}

#[test]
fn aec_linear_interpolator_is_accurate() {
    // A 3 kHz tone read at fractional positions matches the analytic value (the drift
    // path depends on this).
    let mut aec = LinearAec::new(LinearConfig::default());
    let w = 2.0 * std::f64::consts::PI * 3_000.0 / 16_000.0;
    for i in 0..4_096u64 {
        aec.hist[i as usize] = (0.5 * (w * i as f64).sin()) as f32;
    }
    aec.ref_count = 4_096;
    let mut worst = 0.0f64;
    for k in 0..500 {
        let pos = 1_000.0 + k as f64 * 3.37;
        let got = f64::from(aec.read_ref(pos));
        worst = worst.max((got - 0.5 * (w * pos).sin()).abs());
    }
    // -60 dB relative to the tone.
    assert!(worst < 0.5e-3, "interpolation error {worst}");
}

#[test]
#[should_panic(expected = "multiple of BLOCK")]
fn aec_linear_rejects_partial_blocks() {
    let mut aec = LinearAec::new(LinearConfig::default());
    let mut out = [0.0f32; 100];
    aec.process(&[0.0; 100], &[0.0; 100], &mut out);
}

#[test]
fn aec_linear_filter_shift_moves_the_impulse_response_exactly() {
    let mut aec = LinearAec::new(LinearConfig::default());
    // Unit impulse at tap 300 (partition 2, offset 44), built in the frequency domain.
    let tap = 300usize;
    let part = tap / BLOCK;
    for (i, b) in aec.buf.iter_mut().enumerate() {
        *b = Complex::new(if i == tap % BLOCK { 1.0 } else { 0.0 }, 0.0);
    }
    aec.fft.process(&mut aec.buf);
    let ws = part * BINS;
    let spec: Vec<Complex<f32>> = aec.buf[..BINS].to_vec();
    aec.w[ws..ws + BINS].copy_from_slice(&spec);
    aec.shift_filter_left(28);
    // Read back the time-domain filter.
    let mut h = vec![0.0f32; aec.cfg.partitions * BLOCK];
    for p in 0..aec.cfg.partitions {
        aec.y_spec.copy_from_slice(&aec.w[p * BINS..(p + 1) * BINS]);
        aec.load_half_spectrum();
        aec.ifft.process(&mut aec.buf);
        for t in 0..BLOCK {
            h[p * BLOCK + t] = aec.buf[t].re / FFT_LEN as f32;
        }
    }
    for (t, &v) in h.iter().enumerate() {
        let want = if t == tap - 28 { 1.0 } else { 0.0 };
        assert!((v - want).abs() < 1e-5, "tap {t}: {v}");
    }
}

#[test]
fn aec_linear_short_delay_with_drift_keeps_the_echo_in_place() {
    // Bulk delay down to 0 ms with +/-200 ppm drift: the read offset would hit the
    // interpolator limit; the per-epoch pre-delay (and its re-plan once the drift is
    // known) keeps the echo still in the filter. No loudspeaker clipping here, so the
    // linear stage must reach the aec.erle_linear threshold.
    let min = threshold("aec.erle_linear");
    for (name, ppm) in [("drift-plus-200ppm", 200.0), ("drift-minus-200ppm", -200.0)] {
        let scn = crate::testutil::scenarios()
            .get(name)
            .unwrap()
            .with_delay_and_drift(0.0, ppm);
        let out = crate::testutil::run(&scn);
        let (e, _) = cancel(&out);
        let erle = segmental_erle(&out.mic[..e.len()], &e, 16_000, &ErleConfig::default())
            .unwrap()
            .median_db;
        eprintln!("{}: linear ERLE {erle:.1} dB", scn.name);
        assert!(
            erle >= min,
            "{}: linear ERLE {erle:.2} dB < {min}",
            scn.name
        );
    }
}

#[test]
fn aec_linear_reaches_the_best_linear_canceller_under_clipping() {
    // Oracle: the far-only echo without loudspeaker clipping (same seed), scaled by the
    // best single gain, subtracted from the clipped echo. No linear canceller can do
    // better on the clipped residue; the adaptive stage must come within 3 dB of it.
    let base = crate::testutil::scenarios()
        .get("far-only")
        .unwrap()
        .clone();
    let clipped = crate::testutil::run(&base);
    let unclipped = crate::testutil::run(&Scenario {
        softclip: None,
        ..base
    });
    let (mut gxy, mut gxx) = (0.0f64, 0.0f64);
    for (a, b) in clipped.echo.iter().zip(&unclipped.echo) {
        gxy += f64::from(*a) * f64::from(*b);
        gxx += f64::from(*b) * f64::from(*b);
    }
    let g = gxy / gxx;
    let resid: Vec<f32> = clipped
        .echo
        .iter()
        .zip(&unclipped.echo)
        .map(|(a, b)| (f64::from(*a) - g * f64::from(*b)) as f32)
        .collect();
    let oracle = segmental_erle(&clipped.mic, &resid, 16_000, &ErleConfig::default())
        .unwrap()
        .median_db;
    let (e, _) = cancel(&clipped);
    let got = segmental_erle(&clipped.mic[..e.len()], &e, 16_000, &ErleConfig::default())
        .unwrap()
        .median_db;
    eprintln!("far-only linear ERLE {got:.1} dB, best linear canceller {oracle:.1} dB");
    assert!(got >= oracle - 3.0, "{got:.2} dB vs oracle {oracle:.2} dB");
}

/// Linear output for a stereo reference (left, right).
fn cancel_stereo(out: &SimOutput) -> (Vec<f32>, LinearAec) {
    let n = out.mic.len() / BLOCK * BLOCK;
    let [l, r] = out.reference_lr.as_ref().expect("stereo scenario");
    let mut aec = LinearAec::new(LinearConfig::default());
    let mut e = vec![0.0f32; n];
    for (b, o) in e.as_chunks_mut::<BLOCK>().0.iter_mut().enumerate() {
        let s = b * BLOCK..(b + 1) * BLOCK;
        let out = aec.process_block_stereo(&out.mic[s.clone()], &l[s.clone()], &r[s], 1.0);
        o.copy_from_slice(out.error);
    }
    (e, aec)
}

fn median_erle(mic: &[f32], e: &[f32]) -> f64 {
    segmental_erle(&mic[..e.len()], e, 16_000, &ErleConfig::default())
        .unwrap()
        .median_db
}

#[test]
fn aec_linear_stereo_with_identical_channels_is_the_mono_canceller() {
    // Mono fallback: identical channels (side exactly zero) run the mono canceller,
    // sample for sample.
    let o = run_scenario("far-only");
    let n = o.mic.len() / BLOCK * BLOCK;
    let (mono, _) = cancel(&o);
    let mut aec = LinearAec::new(LinearConfig::default());
    for (b, m) in mono
        .as_chunks::<BLOCK>()
        .0
        .iter()
        .enumerate()
        .take(n / BLOCK)
    {
        let s = b * BLOCK..(b + 1) * BLOCK;
        let r = &o.reference[s.clone()];
        let out = aec.process_block_stereo(&o.mic[s], r, r, 1.0);
        assert_eq!(out.error, m.as_slice(), "block {b} differs from mono");
    }
    assert!(!aec.is_stereo());
}

#[test]
fn aec_linear_stereo_converges_on_two_independent_channels() {
    // Exactness of the two-channel filter: independent white channels through different
    // short paths (the right one 40 samples later) are cancelled deeply.
    let n = 20 * 16_000;
    let mut seed = 7u64;
    let mut noise = || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        ((seed >> 11) as f64 / (1u64 << 53) as f64 - 0.5) as f32 * 0.3
    };
    let l: Vec<f32> = (0..n).map(|_| noise()).collect();
    let r: Vec<f32> = (0..n).map(|_| noise()).collect();
    let d = 1600;
    let mic: Vec<f32> = (0..n)
        .map(|i| {
            let a = if i >= d + 5 {
                0.5 * l[i - d] - 0.2 * l[i - d - 5]
            } else {
                0.0
            };
            let b = if i >= d + 60 {
                0.3 * r[i - d - 40] + 0.1 * r[i - d - 60]
            } else {
                0.0
            };
            a + b
        })
        .collect();
    let mut aec = LinearAec::new(LinearConfig::default());
    let mut e = vec![0.0f32; n];
    for (b, o) in e.as_chunks_mut::<BLOCK>().0.iter_mut().enumerate() {
        let s = b * BLOCK..(b + 1) * BLOCK;
        let out = aec.process_block_stereo(&mic[s.clone()], &l[s.clone()], &r[s], 1.0);
        o.copy_from_slice(out.error);
    }
    let tail = n - 2 * 16_000..n;
    let erle = 10.0 * (crate::dsp::power(&mic[tail.clone()]) / crate::dsp::power(&e[tail])).log10();
    eprintln!("stereo white-noise ERLE over the last 2 s: {erle:.1} dB");
    assert!(aec.is_stereo());
    assert!(erle >= 30.0, "stereo ERLE {erle:.1} dB");
}

#[test]
fn aec_linear_stereo_reference_beats_the_mono_downmix() {
    // Two loudspeakers with partly decorrelated content: the mono canceller on the
    // downmix cannot model both paths; the stereo canceller does.
    let base = crate::testutil::scenarios().get("far-only-linear").unwrap();
    let min_erle = threshold("aec.erle_linear");
    for c in [0.5, 0.8] {
        let o = crate::testutil::run(&crate::testutil::stereo_variant(base, c, None));
        let (mono, _) = cancel(&o);
        let (stereo, aec) = cancel_stereo(&o);
        let (em, es) = (median_erle(&o.mic, &mono), median_erle(&o.mic, &stereo));
        eprintln!("c {c}: linear ERLE mono downmix {em:.1} dB, stereo {es:.1} dB");
        assert!(
            es >= min_erle,
            "c {c}: stereo linear ERLE {es:.2} dB < {min_erle}"
        );
        assert!(
            es >= em + 6.0,
            "c {c}: stereo {es:.2} dB vs mono downmix {em:.2} dB"
        );
        // The estimator's peak moves between the two loudspeakers' paths; those moves
        // keep the alignment instead of realigning the filter each time.
        let (est, aligned) = (aec.delay_estimator().epoch(), aec.alignment_epoch());
        eprintln!("c {c}: estimator epochs {est}, alignment epochs {aligned}");
        assert!(aligned <= 3, "c {c}: realigned {aligned} times");
    }
}

#[test]
fn aec_linear_stereo_keeps_cancelling_when_the_far_end_image_changes() {
    // Non-uniqueness (Sondhi, Morgan, Hall 1995): with strongly correlated channels a
    // filter that only fits the current inter-channel relation stops cancelling when the
    // far-end stereo image changes. Measured 1-4 s after the change at 15 s.
    let base = crate::testutil::scenarios().get("far-only-linear").unwrap();
    let o = crate::testutil::run(&crate::testutil::stereo_variant(base, 0.95, Some(15.0)));
    let (mono, _) = cancel(&o);
    let (stereo, _) = cancel_stereo(&o);
    let w = 16 * 16_000..19 * 16_000;
    let after = |e: &[f32]| median_erle(&o.mic[w.clone()], &e[w.clone()]);
    let (em, es) = (after(&mono), after(&stereo));
    eprintln!("after the image change: linear ERLE mono downmix {em:.1} dB, stereo {es:.1} dB");
    assert!(es >= 10.0, "stereo linear ERLE after the change {es:.2} dB");
    assert!(
        es >= em + 6.0,
        "stereo {es:.2} dB vs mono downmix {em:.2} dB"
    );
}

#[test]
fn aec_linear_survives_non_finite_input() {
    // One NaN or infinite sample (a driver glitch) must not poison the filter: output stays
    // finite, cancellation continues, and the delay estimator still follows the later
    // 40 -> 400 ms jump of delay-jump. Bad samples in the mic, then in the reference
    // (left channel for stereo), mono and stereo.
    let scn = crate::testutil::scenarios().get("delay-jump").unwrap();
    let at = 5 * 16_000;
    let tol = 32.0;
    for stereo in [false, true] {
        let o = if stereo {
            crate::testutil::run(&crate::testutil::stereo_variant(scn, 0.8, None))
        } else {
            run_scenario("delay-jump")
        };
        let n = o.mic.len() / BLOCK * BLOCK;
        let (l, r) = match &o.reference_lr {
            Some([l, r]) => (l.clone(), r.clone()),
            None => (o.reference.clone(), o.reference.clone()),
        };
        let run = |mic: &[f32], left: &[f32]| -> (Vec<f32>, LinearAec) {
            let mut aec = LinearAec::new(LinearConfig::default());
            let mut e = vec![0.0f32; n];
            for (b, out) in e.as_chunks_mut::<BLOCK>().0.iter_mut().enumerate() {
                let s = b * BLOCK..(b + 1) * BLOCK;
                let res = if stereo {
                    aec.process_block_stereo(&mic[s.clone()], &left[s.clone()], &r[s], 1.0)
                } else {
                    aec.process_block(&mic[s.clone()], &left[s], 1.0)
                };
                out.copy_from_slice(res.error);
            }
            (e, aec)
        };
        let (clean, clean_aec) = run(&o.mic, &l);
        let bad_mic = crate::testutil::with_non_finite(&[&o.mic], at).remove(0);
        let bad_ref = crate::testutil::with_non_finite(&[&l], at + 16_000).remove(0);
        for (what, mic, left) in [("mic", &bad_mic, &l), ("reference", &o.mic, &bad_ref)] {
            let (e, aec) = run(mic, left);
            let what = format!("{} {what}", if stereo { "stereo" } else { "mono" });
            assert!(e.iter().all(|v| v.is_finite()), "{what}: non-finite output");
            // Cancelling again within 0.5 s of the bad blocks (the second bad position is
            // 1 s later for the reference).
            let w = at + 3 * 16_000..at + 8 * 16_000;
            let erle = |x: &[f32]| {
                10.0 * (crate::dsp::power(&o.mic[w.clone()]) / crate::dsp::power(&x[w.clone()]))
                    .log10()
            };
            let (got, want) = (erle(&e), erle(&clean));
            eprintln!("{what}: ERLE after the bad samples {got:.1} dB (clean run {want:.1} dB)");
            assert!(
                got >= want - 3.0,
                "{what}: ERLE {got:.2} dB vs clean {want:.2} dB"
            );
            // Still tracking the delay after the jump at 15 s.
            let truth = o.delay.delay_at(n - 1);
            let d = aec.delay_state().delay().expect("locked at the end");
            let dc = clean_aec.delay_state().delay().unwrap();
            assert!(
                (d - dc).abs() <= 8.0,
                "{what}: delay {d:.1} vs clean run {dc:.1}"
            );
            if !stereo {
                assert!(
                    (d - truth).abs() <= tol,
                    "{what}: delay {d:.1} vs truth {truth:.1}"
                );
            }
        }
    }
}

#[test]
fn aec_linear_recovers_from_absurd_finite_input() {
    // T6.6b: finite but absurd samples (|x| = 1e20, far beyond full scale) in the
    // reference must not leave the NLMS normaliser infinite or full of rounding error (no
    // or wild adaptation until the periodic re-sum, about 8 s; measured -270 dB here
    // before). The block makes the output diverge and the filter resets, so the check is
    // that the filter re-converges (at least 6 dB over 3-8 s after the block; measured
    // 12.1 dB, clean run 31.9 dB), not that it matches the clean run.
    let o = run_scenario("far-only-linear");
    let n = o.mic.len() / BLOCK * BLOCK;
    let at = 5 * 16_000;
    let run = |reference: &[f32]| {
        let mut aec = LinearAec::new(LinearConfig::default());
        let mut e = vec![0.0f32; n];
        aec.process(&o.mic[..n], &reference[..n], &mut e);
        e
    };
    let clean = run(&o.reference);
    let mut bad = o.reference.clone();
    for v in &mut bad[at..at + BLOCK] {
        *v = 1e20;
    }
    let e = run(&bad);
    assert!(e.iter().all(|v| v.is_finite()), "non-finite output");
    let w = at + 3 * 16_000..at + 8 * 16_000;
    let erle = |x: &[f32]| {
        10.0 * (crate::dsp::power(&o.mic[w.clone()]) / crate::dsp::power(&x[w.clone()])).log10()
    };
    let (got, want) = (erle(&e), erle(&clean));
    eprintln!("ERLE 3-8 s after the absurd block: {got:.1} dB (clean run {want:.1} dB)");
    assert!(got >= 6.0, "ERLE {got:.2} dB (clean run {want:.2} dB)");
}

#[test]
fn aec_linear_stereo_switching_is_hysteretic() {
    // T6.6c: near-identical channels whose difference hovers around the side threshold
    // (-50 dB of the mid power) must not switch the stereo path (and with it the
    // double-talk detector) on and off block by block.
    let o = run_scenario("far-only-linear");
    let n = o.mic.len() / BLOCK * BLOCK;
    let mut aec = LinearAec::new(LinearConfig::default());
    let mut flips = 0usize;
    let mut last = false;
    let mut seed = 11u64;
    for b in 0..n / BLOCK {
        let s = b * BLOCK..(b + 1) * BLOCK;
        let l = &o.reference[s.clone()];
        // Side level swings between -47 and -53 dB of the channel (a slow wobble plus
        // jitter), so the summed side power crosses -50 dB over and over.
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        let jitter = ((seed >> 11) as f64 / (1u64 << 53) as f64 - 0.5) * 2.0;
        let db = -50.0 + 3.0 * ((b as f64 / 40.0).sin() * 0.7 + jitter * 0.3);
        let g = 2.0 * 10f64.powf(db / 20.0) as f32;
        let r: Vec<f32> = l.iter().map(|v| v * (1.0 - g)).collect();
        aec.process_block_stereo(&o.mic[s], l, &r, 1.0);
        if aec.is_stereo() != last {
            flips += 1;
            last = aec.is_stereo();
        }
    }
    eprintln!("stereo path switched {flips} times");
    assert!(flips <= 4, "stereo path switched {flips} times");
}
