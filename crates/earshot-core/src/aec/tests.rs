//! Full-canceller tests (PLAN.md T1.5 accept: `cargo test -p earshot-core aec`).
//! Thresholds come from `acceptance/thresholds.toml`; `aec.doubletalk_sisdr` and the
//! robustness grid are scored by `accept m1` (T1.6).

use super::*;
use crate::metrics::erle::{
    CONVERGED_ERLE_DB, ConvergenceConfig, ErleConfig, segmental_erle, time_to_erle,
};
use crate::metrics::sisdr::si_sdr;
use crate::sim::SimOutput;
use crate::testutil::{run_scenario, threshold};

/// Runs a fresh canceller; the output is returned latency-compensated (sample `n` of the
/// result belongs to mic sample `n`), so it is shorter than the mic by the latency.
fn cancel_with(out: &SimOutput, cfg: EchoCancellerConfig) -> (Vec<f32>, Vec<BlockStatus>) {
    let n = out.mic.len() / BLOCK * BLOCK;
    let mut aec = EchoCanceller::new(cfg);
    let mut e = vec![0.0f32; n];
    let mut status = Vec::with_capacity(n / BLOCK);
    for ((m, r), o) in out.mic[..n]
        .as_chunks::<BLOCK>()
        .0
        .iter()
        .zip(out.reference[..n].as_chunks::<BLOCK>().0)
        .zip(e.as_chunks_mut::<BLOCK>().0)
    {
        aec.process(m, r, o);
        status.push(aec.status());
    }
    (e[aec.output_latency()..].to_vec(), status)
}

/// Largest sample difference accepted where the output must equal the delayed mic (the
/// postfilter's overlap-add reconstructs to float rounding).
const MAX_PASS_DIFF: f32 = 1e-5;

fn assert_passes_through(out: &[f32], mic: &[f32], what: &str) {
    let worst = out
        .iter()
        .zip(mic)
        .map(|(a, b)| (a - b).abs())
        .fold(0.0f32, f32::max);
    assert!(
        worst <= MAX_PASS_DIFF,
        "{what}: output differs from the mic by {worst}"
    );
}

/// The canceller's input as the app's audio defines it: the mic after the default
/// high-pass (T6.23b: with an audio high-pass, passing the mic through
/// means passing the high-passed mic through), run from the first sample as the canceller
/// runs it.
fn app_input(mic: &[f32]) -> Vec<f32> {
    match DEFAULT_HIGHPASS {
        HighPass::Audio { cutoff_hz } => HighPassFilter::new(cutoff_hz, 16_000).filtered(mic),
        _ => mic.to_vec(),
    }
}

fn cancel(out: &SimOutput) -> Vec<f32> {
    cancel_with(out, EchoCancellerConfig::default()).0
}

fn time_to_converge(mic: &[f32], out: &[f32], from_s: f64) -> Option<f64> {
    time_to_erle(
        mic,
        out,
        16_000,
        from_s,
        CONVERGED_ERLE_DB,
        &ConvergenceConfig::default(),
    )
}

#[test]
fn aec_full_far_only_meets_erle_and_convergence() {
    let o = run_scenario("far-only");
    let e = cancel(&o);
    let mic = &o.mic[..e.len()];
    let erle = segmental_erle(mic, &e, 16_000, &ErleConfig::default())
        .unwrap()
        .median_db;
    let conv = time_to_converge(mic, &e, 0.0).expect("never reached 15 dB");
    eprintln!("far-only: ERLE {erle:.1} dB, convergence {conv:.2} s");
    let min_erle = threshold("aec.erle_farend");
    let max_conv = threshold("aec.convergence");
    assert!(
        erle >= min_erle,
        "aec.erle_farend {erle:.2} dB < {min_erle}"
    );
    assert!(conv <= max_conv, "aec.convergence {conv:.2} s > {max_conv}");
}

#[test]
fn aec_full_far_only_meets_the_erle_bar_on_every_dev_wide_voice() {
    // T1.6c: on the dev-wide voices (dev-clean, `fixtures/librispeech/aec_dev.toml`) the
    // loudspeaker clipping of voiced speech leaves a product below 125 Hz that the in-bin
    // residual models cannot predict; before the low-frequency product model it held the
    // far-end-only ERLE at 10.9-18.6 dB on 6 of the 8 pairs.
    let scn = crate::testutil::scenarios()
        .get("far-only")
        .expect("far-only scenario");
    let min_erle = threshold("aec.erle_farend");
    let mut failed = Vec::new();
    for pair in crate::testutil::dev_wide_speech() {
        let o = crate::testutil::run_dev_wide(scn, pair);
        let run = eval::run_canceller(&o, EchoCancellerConfig::default());
        let erle = segmental_erle(
            &o.mic[..run.out.len()],
            &run.out,
            16_000,
            &ErleConfig::default(),
        )
        .expect("scored windows")
        .median_db;
        eprintln!("far-only {}/{}: ERLE {erle:.1} dB", pair.far, pair.near);
        if erle < min_erle {
            failed.push(format!("{}/{} {erle:.1} dB", pair.far, pair.near));
        }
    }
    assert!(
        failed.is_empty(),
        "aec.erle_farend below {min_erle} dB on dev-wide voices: {failed:?}"
    );
}

#[test]
fn aec_full_recovers_after_path_change_and_delay_jump() {
    let max = threshold("aec.path_change");
    for name in ["path-change", "delay-jump"] {
        let o = run_scenario(name);
        let e = cancel(&o);
        let t = time_to_converge(&o.mic[..e.len()], &e, 15.0).expect("never recovered");
        eprintln!("{name}: recovery {t:.2} s");
        assert!(t <= max, "{name}: aec.path_change {t:.2} s > {max}");
    }
}

#[test]
fn aec_full_passes_the_mic_through_without_echo() {
    // No echo path: the output is the mic (after the default high-pass, `app_input`),
    // sample for sample after the fixed latency.
    for name in ["near-only", "headphones"] {
        let o = run_scenario(name);
        let e = cancel(&o);
        assert_passes_through(&e, &app_input(&o.mic)[..e.len()], name);
    }
    // Echo disappears mid-call (switch to headphones): near end passes through after.
    let o = run_scenario("switch-speakers-to-headphones");
    let e = cancel(&o);
    let from = 15 * 16_000;
    let input = app_input(&o.mic);
    let pass = si_sdr(&e[from..], &input[from..e.len()]).unwrap();
    let gain = gain_db(&e[from..], &input[from..e.len()]);
    eprintln!("switch-speakers-to-headphones: passthrough {pass:.1} dB, gain {gain:+.2} dB");
    let min = threshold("aec.passthrough");
    assert!(pass >= min, "aec.passthrough {pass:.2} dB < {min}");
    // SI-SDR is scale-invariant: also bound the level (no attenuation or boost).
    assert!(
        gain.abs() <= MAX_PASS_GAIN_DB,
        "passthrough gain {gain:+.2} dB"
    );
}

/// Largest out/in level change accepted where the mic must pass through.
const MAX_PASS_GAIN_DB: f64 = 0.5;

/// Output-to-input power ratio in dB.
fn gain_db(out: &[f32], input: &[f32]) -> f64 {
    10.0 * (crate::dsp::power(out) / crate::dsp::power(input)).log10()
}

fn cancel_hinted(o: &SimOutput, hint: f64) -> Vec<f32> {
    let n = o.mic.len() / BLOCK * BLOCK;
    let mut aec = EchoCanceller::new(EchoCancellerConfig::default());
    aec.set_delay_hint(hint);
    let out = aec.process_vec(&o.mic[..n], &o.reference[..n]);
    out[aec.output_latency()..].to_vec()
}

#[test]
fn aec_full_hint_never_cancels_without_an_echo_path() {
    // Headphones (no echo) with a plausible warm-start hint: nothing is touched.
    let o = run_scenario("headphones");
    let e = cancel_hinted(&o, o.delay.delay_at(0));
    assert_passes_through(&e, &app_input(&o.mic)[..e.len()], "headphones + hint");

    // Speakers then headphones, echo 15 dB below the near end so the mismatch bypass
    // cannot rescue it: after the switch the near end must pass through.
    let mut scn = crate::testutil::scenarios()
        .get("switch-speakers-to-headphones")
        .unwrap()
        .clone();
    scn.ser_db = 15.0;
    let o = crate::testutil::run(&scn);
    let e = cancel_hinted(&o, o.delay.delay_at(0));
    let from = 15 * 16_000;
    let input = app_input(&o.mic);
    let pass = si_sdr(&e[from..], &input[from..e.len()]).unwrap();
    let gain = gain_db(&e[from..], &input[from..e.len()]);
    eprintln!("hint + switch to headphones (low echo): SI-SDR {pass:.1} dB, gain {gain:+.2} dB");
    assert!(pass >= threshold("aec.passthrough"), "SI-SDR {pass:.2} dB");
    assert!(gain.abs() <= MAX_PASS_GAIN_DB, "gain {gain:+.2} dB");
}

#[test]
fn aec_full_doubletalk_detector_freezes_adaptation() {
    // Double talk is detected and freezes the linear stage; far-end-only speech after
    // convergence is almost never mistaken for it.
    let o = run_scenario("double-talk-ser-0");
    let (_, status) = cancel_with(&o, EchoCancellerConfig::default());
    let near_blocks = status.iter().skip(6 * 125).filter(|s| s.doubletalk).count();
    let frozen = status.iter().skip(6 * 125).filter(|s| s.frozen).count();
    eprintln!("double-talk-ser-0: {near_blocks} double-talk blocks, {frozen} frozen");
    assert!(
        near_blocks > 500,
        "double talk detected in {near_blocks} blocks"
    );
    assert!(frozen > 100, "adaptation frozen in {frozen} blocks");
    let o = run_scenario("far-only");
    let (_, status) = cancel_with(&o, EchoCancellerConfig::default());
    let late: Vec<_> = status.iter().skip(3 * 125).collect();
    // Raw witness calls, vetoed ones included (measured distribution and the assertion
    // that none of them is effective: `aec_full_far_only_double_talk_calls_are_all_vetoed`).
    let false_dt = late.iter().filter(|s| s.doubletalk).count();
    eprintln!("far-only: {false_dt}/{} blocks flagged", late.len());
    assert!(
        (false_dt as f64) < 0.05 * late.len() as f64,
        "far-only flagged as double talk in {false_dt}/{} blocks",
        late.len()
    );
}

#[test]
fn aec_full_frozen_block_leaves_the_filter_unchanged() {
    let o = run_scenario("far-only");
    let n = 3 * 16_000 / BLOCK * BLOCK;
    let mut lin = linear::LinearAec::new(linear::LinearConfig::default());
    let mut e = vec![0.0f32; n];
    lin.process(&o.mic[..n], &o.reference[..n], &mut e);
    let before = lin.filter_energy();
    assert!(before > 0.0);
    let _ = lin.process_block(&o.mic[n..n + BLOCK], &o.reference[n..n + BLOCK], 0.0);
    assert_eq!(lin.filter_energy(), before);
}

#[test]
fn aec_full_comfort_noise_is_optional_and_deterministic() {
    // A scenario with a real noise floor (comfort noise follows the estimated floor).
    let o = run_scenario("noisy-office");
    let tail_power = |cfg| {
        let (e, _) = cancel_with(&o, cfg);
        crate::dsp::power(&e[5 * 16_000..])
    };
    let off = tail_power(EchoCancellerConfig::default());
    let mut seeded = EchoCancellerConfig::default();
    seeded.postfilter.comfort_noise = Some(7);
    let on = tail_power(seeded);
    assert!(on > off, "comfort noise adds energy ({on} vs {off})");
    let (a, _) = cancel_with(&o, seeded);
    let (b, _) = cancel_with(&o, seeded);
    assert_eq!(a, b, "same seed, same output");
}

#[test]
fn aec_full_reports_its_fixed_latency() {
    // 16 ms at 16 kHz, within the 32 ms PLAN.md 3.3 allows; the mic with no echo path comes
    // out exactly that much later.
    let aec = EchoCanceller::new(EchoCancellerConfig::default());
    assert_eq!(aec.output_latency(), 256);
    let o = run_scenario("near-only");
    let n = o.mic.len() / BLOCK * BLOCK;
    let mut aec = EchoCanceller::new(EchoCancellerConfig::default());
    let raw = aec.process_vec(&o.mic[..n], &o.reference[..n]);
    assert_passes_through(
        &raw[256..],
        &app_input(&o.mic[..n])[..n - 256],
        "delayed mic",
    );
    assert!(raw[..256].iter().all(|v| v.abs() < 1e-6));
}

#[test]
#[should_panic(expected = "multiple of BLOCK")]
fn aec_full_rejects_partial_blocks() {
    let mut aec = EchoCanceller::new(EchoCancellerConfig::default());
    let mut out = [0.0f32; 100];
    aec.process(&[0.0; 100], &[0.0; 100], &mut out);
}

/// Largest near-end level change accepted in double talk (PLAN.md T1.5c guard).
const MAX_NEAR_LEVEL_DB: f64 = 1.0;

/// Output, linear-stage output and clean near end over the near-end speech regions
/// (latency compensated).
fn near_regions(o: &SimOutput) -> (Vec<f32>, Vec<f32>, Vec<f32>) {
    near_regions_with(o, EchoCancellerConfig::default())
}

fn near_regions_with(o: &SimOutput, cfg: EchoCancellerConfig) -> (Vec<f32>, Vec<f32>, Vec<f32>) {
    let n = o.mic.len() / BLOCK * BLOCK;
    let mut aec = EchoCanceller::new(cfg);
    let mut out = vec![0.0f32; n];
    let mut lin = vec![0.0f32; n];
    aec.process_with_linear(&o.mic[..n], &o.reference[..n], &mut out, &mut lin);
    let run = eval::compensate(out, lin, aec.output_latency());
    let m = run.out.len();
    let pick = |x: &[f32]| -> Vec<f32> {
        o.near_regions
            .iter()
            .flat_map(|r| x[r.start.min(m)..r.end.min(m)].to_vec())
            .collect()
    };
    (pick(&run.out), pick(&run.linear), pick(&o.near_clean[..m]))
}

#[test]
fn aec_full_postfilter_keeps_the_near_end_level_in_double_talk() {
    // Gated at SER 0 (PLAN.md 4.0, the owner: echo removal is kept over near-end level at
    // negative SER, where transcription quality is gated by M2's asr.wer_aec_delta);
    // SER -10 is reported only.
    let o = run_scenario("double-talk-ser-0");
    let (out, _, near) = near_regions(&o);
    let level = gain_db(&out, &near);
    eprintln!("double-talk-ser-0: near-end level {level:+.2} dB");
    assert!(
        level.abs() <= MAX_NEAR_LEVEL_DB,
        "double-talk-ser-0: near-end level {level:+.2} dB"
    );
    let o = run_scenario("double-talk-ser-10");
    let (out, _, near) = near_regions(&o);
    eprintln!(
        "double-talk-ser-10: near-end level {:+.2} dB (informational)",
        gain_db(&out, &near)
    );
}

#[test]
fn aec_full_postfilter_improves_on_the_linear_stage_in_double_talk() {
    // Held on the unfiltered canceller (`HighPass::Off`). With the default 30 Hz audio
    // high-pass (T6.23b) the postfilter scored 12.65 dB against the linear stage's 13.17 dB
    // on double-talk-ser-0 (3.86 against 4.05 dB on ser-10); the owner accepted that miss on
    // 2026-10-27 as part of the S30 trade. Still open after T6.23c (private notes:
    // the postfilter loses near-end SI-SDR at 150-400 Hz in double talk with or without the
    // filter; unfiltered it passed because the linear stage left a sub-100 Hz residual the
    // postfilter removed). The bound is unchanged.
    for name in ["double-talk-ser-0", "double-talk-ser-10"] {
        let o = run_scenario(name);
        let (out, lin, near) = near_regions_with(&o, with_highpass(HighPass::Off));
        let full = si_sdr(&out, &near).unwrap();
        let linear = si_sdr(&lin, &near).unwrap();
        eprintln!("{name}: SI-SDR linear {linear:.1} dB, with postfilter {full:.1} dB");
        assert!(
            full >= linear,
            "{name}: postfilter {full:.2} < linear {linear:.2}"
        );
    }
}

/// Blocks in which the mismatch bypass passed the mic through after the linear stage
/// first converged (before a lock the status also reports a pass-through).
fn bypass_blocks(o: &SimOutput, cfg: EchoCancellerConfig) -> usize {
    let (_, status) = cancel_with(o, cfg);
    let first = status
        .iter()
        .position(|s| s.converged)
        .expect("never converged");
    status[first..].iter().filter(|s| s.bypass).count()
}

#[test]
fn aec_full_clipped_echo_bursts_do_not_engage_the_bypass() {
    // With the echo path intact, the linear estimate overshooting a clipped echo burst
    // (a few blocks) or the decaying filter output after the far end stops must not be
    // taken for a lost echo path: the bypass would pass the full echo through until the
    // linear stage cancels again, which in double talk can take seconds. These grid
    // points put loud clipped bursts inside near-end speech (T1.6 robustness grid).
    let file = crate::testutil::scenarios();
    let base = file.get("double-talk-ser-0").unwrap();
    for (delay_ms, ppm) in [(250.0, 0.0), (500.0, 0.0)] {
        let scn = base.with_delay_and_drift(delay_ms, ppm);
        let o = crate::testutil::run(&scn);
        let n = bypass_blocks(&o, EchoCancellerConfig::default());
        eprintln!("{}: {n} bypass blocks", scn.name);
        assert_eq!(
            n, 0,
            "{}: bypass engaged with the echo path intact",
            scn.name
        );
    }
}

#[test]
fn aec_full_stereo_with_identical_channels_is_the_mono_canceller() {
    let o = run_scenario("double-talk-ser-0");
    let n = o.mic.len() / BLOCK * BLOCK;
    let mono = EchoCanceller::new(EchoCancellerConfig::default())
        .process_vec(&o.mic[..n], &o.reference[..n]);
    let mut stereo = vec![0.0f32; n];
    EchoCanceller::new(EchoCancellerConfig::default()).process_stereo(
        &o.mic[..n],
        &o.reference[..n],
        &o.reference[..n],
        &mut stereo,
    );
    assert_eq!(mono, stereo);
}

#[test]
fn aec_full_stereo_reference_cancels_two_loudspeakers() {
    // Two loudspeakers with the gating clip level: the stereo canceller meets
    // aec.erle_farend and beats the mono canceller on the downmix. Double talk at SER 0
    // is printed for information (stereo scenarios are not gating).
    let file = crate::testutil::scenarios();
    let min = threshold("aec.erle_farend");
    for (name, c) in [("far-only", 0.8), ("double-talk-ser-0", 0.8)] {
        let o = crate::testutil::run(&crate::testutil::stereo_variant(
            file.get(name).unwrap(),
            c,
            None,
        ));
        let n = o.mic.len() / BLOCK * BLOCK;
        let [l, r] = o.reference_lr.as_ref().unwrap();
        let mut aec = EchoCanceller::new(EchoCancellerConfig::default());
        let mut out = vec![0.0f32; n];
        aec.process_stereo(&o.mic[..n], &l[..n], &r[..n], &mut out);
        let stereo = out[aec.output_latency()..].to_vec();
        let mono = cancel(&o);
        if name == "far-only" {
            let erle = |e: &[f32]| {
                segmental_erle(&o.mic[..e.len()], e, 16_000, &ErleConfig::default())
                    .unwrap()
                    .median_db
            };
            let (es, em) = (erle(&stereo), erle(&mono));
            eprintln!("{name} stereo c {c}: ERLE stereo {es:.1} dB, mono downmix {em:.1} dB");
            assert!(es >= min, "stereo ERLE {es:.2} dB < {min}");
            assert!(es >= em, "stereo {es:.2} dB vs mono downmix {em:.2} dB");
        } else {
            let m = stereo.len().min(mono.len());
            let pick = |x: &[f32]| -> Vec<f32> {
                o.near_regions
                    .iter()
                    .flat_map(|r| x[r.start.min(m)..r.end.min(m)].to_vec())
                    .collect()
            };
            let near = pick(&o.near_clean[..m]);
            eprintln!(
                "{name} stereo c {c} (informational): SI-SDR stereo {:.1} dB, mono downmix {:.1} dB",
                si_sdr(&pick(&stereo), &near).unwrap(),
                si_sdr(&pick(&mono), &near).unwrap()
            );
        }
    }
}

#[test]
fn aec_full_survives_non_finite_input() {
    // Canceller state (detector powers, postfilter) never sees a non-finite sample:
    // output stays finite, a later stretch without echo passes the mic through again, and
    // a cancelling stretch keeps cancelling.
    for name in ["near-only", "far-only"] {
        let o = run_scenario(name);
        let at = 5 * 16_000;
        let bad = crate::testutil::with_non_finite(&[&o.mic, &o.reference], at);
        let poisoned = SimOutput {
            mic: bad[0].clone(),
            reference: bad[1].clone(),
            ..o.clone()
        };
        for stereo in [false, true] {
            let n = o.mic.len() / BLOCK * BLOCK;
            let mut aec = EchoCanceller::new(EchoCancellerConfig::default());
            let mut out = vec![0.0f32; n];
            if stereo {
                aec.process_stereo(
                    &poisoned.mic[..n],
                    &poisoned.reference[..n],
                    &poisoned.reference[..n],
                    &mut out,
                );
            } else {
                aec.process(&poisoned.mic[..n], &poisoned.reference[..n], &mut out);
            }
            let e = &out[aec.output_latency()..];
            assert!(
                e.iter().all(|v| v.is_finite()),
                "{name} stereo {stereo}: non-finite output"
            );
            let w = at + 16_000..at + 10 * 16_000;
            if name == "near-only" {
                // The default high-pass of the poisoned mic (non-finite samples are zero
                // before the filter, so its state recovers within the 1 s gap).
                let input = app_input(&o.mic);
                assert_passes_through(&e[w.clone()], &input[w], name);
            } else {
                let (clean, _) = cancel_with(&o, EchoCancellerConfig::default());
                let erle = |x: &[f32]| {
                    10.0 * (crate::dsp::power(&o.mic[w.clone()]) / crate::dsp::power(&x[w.clone()]))
                        .log10()
                };
                let (got, want) = (erle(e), erle(&clean));
                eprintln!(
                    "{name} stereo {stereo}: ERLE after bad samples {got:.1} dB (clean {want:.1})"
                );
                assert!(
                    got >= want - 3.0,
                    "{name} stereo {stereo}: {got:.2} vs {want:.2} dB"
                );
            }
        }
    }
}

#[test]
fn aec_full_event_scenarios_hold_at_zero_delay_with_drift() {
    // Grid points of the event scenarios that failed when T6.6a swept them (M1 review):
    // a delay jump from a short delay (the pre-delay grows with the jump, so the kept
    // filter must move with it) and a switch to headphones right after a clipped burst
    // that triggered double talk (the vanished echo must bypass at once).
    let file = crate::testutil::scenarios();
    let max_pc = threshold("aec.path_change");
    let min_pass = threshold("aec.passthrough");
    for (name, ppm) in [("delay-jump", 0.0), ("delay-jump", 200.0)] {
        let scn = file.get(name).unwrap().with_delay_and_drift(0.0, ppm);
        let o = crate::testutil::run(&scn);
        let sc = eval::score_for(
            &scn,
            &o,
            &eval::run_canceller(&o, EchoCancellerConfig::default()),
            &EchoCancellerConfig::default(),
        );
        let t = sc.path_change.expect("event scenario");
        eprintln!("{}: path change {t:.2} s", scn.name);
        assert!(
            t <= max_pc,
            "{}: aec.path_change {t:.2} s > {max_pc}",
            scn.name
        );
    }
    let scn = file
        .get("switch-speakers-to-headphones")
        .unwrap()
        .with_delay_and_drift(0.0, -200.0);
    let o = crate::testutil::run(&scn);
    let sc = eval::score_for(
        &scn,
        &o,
        &eval::run_canceller(&o, EchoCancellerConfig::default()),
        &EchoCancellerConfig::default(),
    );
    let pass = sc.passthrough.expect("echo stops");
    eprintln!("{}: passthrough {pass:.1} dB", scn.name);
    assert!(
        pass >= min_pass,
        "{}: aec.passthrough {pass:.2} dB < {min_pass}",
        scn.name
    );
}

#[test]
fn aec_full_a_new_lock_epoch_ends_the_mismatch_bypass() {
    // T6.23c: a jump to a longer delay leaves the mic without echo for the difference
    // (360 ms here) while the filter, still on the old alignment, predicts it: the echo-gone
    // rule fires and the bypass sticks, suspending the cold-phase model, until the linear
    // stage re-converges (with the 30 Hz high-pass, recovery 2.0 s on this grid point). The
    // re-lock on the new delay is fresh evidence of an echo path, so the bypass, a verdict
    // about the old alignment, ends with it.
    let file = crate::testutil::scenarios();
    let scn = file
        .get("delay-jump")
        .unwrap()
        .with_delay_and_drift(0.0, 0.0);
    let o = crate::testutil::run(&scn);
    let n = o.mic.len() / BLOCK * BLOCK;
    let mut aec = EchoCanceller::new(EchoCancellerConfig::default());
    let mut out = [0.0f32; BLOCK];
    let mut trace = Vec::with_capacity(n / BLOCK);
    for (m, r) in o.mic[..n]
        .as_chunks::<BLOCK>()
        .0
        .iter()
        .zip(o.reference[..n].as_chunks::<BLOCK>().0)
    {
        aec.process(m, r, &mut out);
        trace.push((aec.linear().alignment_epoch(), aec.status().bypass));
    }
    let jump = 15 * 16_000 / BLOCK;
    let old = trace[jump].0;
    let relock = (jump..trace.len())
        .find(|&b| trace[b].0 != old)
        .expect("no re-lock after the jump");
    // Control: the case this test is about, the bypass engaged by the jump before the re-lock.
    assert!(
        trace[jump..relock].iter().any(|&(_, bypass)| bypass),
        "control: the jump did not engage the bypass"
    );
    let new = trace[relock].0;
    // From the re-lock block itself: the new epoch ends the bypass before that block's
    // output is chosen.
    let held = trace[relock..]
        .iter()
        .take_while(|&&(epoch, _)| epoch == new)
        .filter(|&&(_, bypass)| bypass)
        .count();
    let sc = eval::score_for(
        &scn,
        &o,
        &eval::run_canceller(&o, EchoCancellerConfig::default()),
        &EchoCancellerConfig::default(),
    );
    let path_change = sc.path_change.expect("event scenario");
    eprintln!("{}: path change {path_change:.2} s", scn.name);
    // The benefit, not only the mechanism (private notes, T6.23c option A at this
    // grid point: 2.0 s with the bypass held past the re-lock, 1.1 s with it ended): a
    // bound between the two, with margin on both sides.
    assert!(
        path_change <= MAX_DELAY_JUMP_RECOVERY_S,
        "{}: path change {path_change:.2} s > {MAX_DELAY_JUMP_RECOVERY_S} s",
        scn.name
    );
    assert_eq!(held, 0, "bypass held for {held} blocks after the re-lock");
}

/// The `delay-jump@0ms+0ppm` recovery bound of
/// `aec_full_a_new_lock_epoch_ends_the_mismatch_bypass`: 2.0 s measured with the bypass
/// held past the re-lock, 1.1 s with it ended (private notes, T6.23c).
const MAX_DELAY_JUMP_RECOVERY_S: f64 = 1.6;

#[test]
fn aec_full_stereo_double_talk_meets_the_target_without_the_linear_witness() {
    // T1.5d: stereo double talk is detected by the coherence statistic, so the canceller
    // keeps the near end where the two-channel linear stage has not converged. These grid
    // points scored 2.7-6.2 dB with the detector that used the linear estimate.
    let file = crate::testutil::scenarios();
    let min = threshold("aec.doubletalk_sisdr");
    let base = file.get("stereo-double-talk-ser-0").unwrap();
    for (d, ppm) in [(0.0, 200.0), (500.0, -200.0)] {
        let scn = base.with_delay_and_drift(d, ppm);
        let o = crate::testutil::run(&scn);
        let sc = eval::score(
            &scn,
            &o,
            &eval::run_canceller(&o, EchoCancellerConfig::default()),
        );
        let v = sc.doubletalk_sisdr.expect("double talk at SER 0");
        eprintln!("{}: SI-SDR {v:.2} dB", scn.name);
        assert!(
            v >= min,
            "{}: aec.doubletalk_sisdr {v:.2} dB < {min}",
            scn.name
        );
    }
}

#[test]
fn aec_full_coherence_flags_double_talk_and_not_far_end_speech() {
    // The statistic itself on the canceller: low during near-end speech, high while only
    // the far end talks (median over blocks after 3 s), mono and stereo.
    for (dt, far) in [
        ("double-talk-ser-0", "far-only"),
        ("stereo-double-talk-ser-0", "stereo-far-only"),
    ] {
        let median = |name: &str, near: bool| {
            let o = run_scenario(name);
            let n = o.mic.len() / BLOCK * BLOCK;
            let mut aec = EchoCanceller::new(EchoCancellerConfig::default());
            let mut v = Vec::new();
            for b in 0..n / BLOCK {
                let s = b * BLOCK..(b + 1) * BLOCK;
                let mut out = [0.0f32; BLOCK];
                match &o.reference_lr {
                    Some([l, r]) => aec.process_stereo(
                        &o.mic[s.clone()],
                        &l[s.clone()],
                        &r[s.clone()],
                        &mut out,
                    ),
                    None => aec.process(&o.mic[s.clone()], &o.reference[s.clone()], &mut out),
                }
                let mid = b * BLOCK + BLOCK / 2;
                let in_near = o.near_regions.iter().any(|r| r.contains(&mid));
                let echo = o.echo[s].iter().map(|x| x * x).sum::<f32>() > 1e-7 * BLOCK as f32;
                if b >= 3 * 125 && echo && in_near == near {
                    v.push(aec.status().coherence);
                }
            }
            v.sort_by(f32::total_cmp);
            v[v.len() / 2]
        };
        let (d, f) = (median(dt, true), median(far, false));
        eprintln!("{dt}: median xi^2 {d:.2} in near-end speech, {far}: {f:.2}");
        let th = EchoCancellerConfig::default().coherence_threshold;
        assert!(d < th, "{dt}: median {d:.2} not below {th}");
        assert!(f > th + 0.1, "{far}: median {f:.2} too close to {th}");
    }
}

#[test]
fn aec_full_unrelated_speech_never_locks_or_suppresses() {
    // T1.5e: different talkers on the mic and the reference and no echo path (two
    // unrelated clips, as the CLI replay did). GCC-PHAT whitens both signals, so unrelated
    // speech can give repeated strong peaks; the canceller must still not lock, cancel or
    // suppress: the output is the mic (level within 0.5 dB, SI-SDR at the passthrough bar).
    let file = crate::testutil::scenarios();
    let base = file.get("headphones").unwrap();
    let min_pass = threshold("aec.passthrough");
    let seeds: Vec<u64> = std::env::var("T15E_SEEDS")
        .ok()
        .map(|v| v.split(',').filter_map(|x| x.parse().ok()).collect())
        .unwrap_or_else(|| (300..312).collect());
    assert!(!seeds.is_empty(), "T15E_SEEDS names no seed");
    for seed in seeds {
        let scn = crate::sim::Scenario {
            name: format!("unrelated-speech-{seed}"),
            seed,
            near_start_s: 0.0,
            ..base.clone()
        };
        let o = crate::testutil::run(&scn);
        let (out, status) = cancel_with(&o, EchoCancellerConfig::default());
        let locked = status.iter().filter(|s| s.delay.is_locked()).count();
        let input = app_input(&o.mic);
        let mic = &input[..out.len()];
        let gain = gain_db(&out, mic);
        let pass = si_sdr(&out, mic).unwrap();
        eprintln!(
            "{}: {locked} locked blocks, gain {gain:+.2} dB, SI-SDR {pass:.1} dB",
            scn.name
        );
        assert_eq!(locked, 0, "{}: locked on unrelated speech", scn.name);
        assert!(
            gain.abs() <= MAX_PASS_GAIN_DB,
            "{}: gain {gain:+.2} dB",
            scn.name
        );
        assert!(
            pass >= min_pass,
            "{}: SI-SDR {pass:.1} dB < {min_pass}",
            scn.name
        );
    }
}

#[test]
fn aec_full_config_validation_rejects_out_of_range_settings() {
    // T6.6b: settings are checked by validate() (and the constructors panic with its
    // message) instead of scattered asserts.
    let d = EchoCancellerConfig::default();
    assert_eq!(d.validate(), Ok(()));
    let mut bad = Vec::new();
    let mut c = d;
    c.linear.partitions = 0;
    bad.push(c);
    let mut c = d;
    c.linear.delay.fft_len = c.linear.delay.max_delay;
    bad.push(c);
    let mut c = d;
    c.linear.delay.hop = 0;
    bad.push(c);
    let mut c = d;
    c.linear.delay.max_delay = 40_000;
    c.linear.delay.fft_len = 65_536;
    bad.push(c);
    let mut c = d;
    c.linear.delay.lock_confidence = 1.0;
    bad.push(c);
    let mut c = d;
    c.linear.step = f32::NAN;
    bad.push(c);
    let mut c = d;
    c.postfilter.min_gain = 2.0;
    bad.push(c);
    let mut c = d;
    c.coherence.band_hz = (6_000.0, 200.0);
    bad.push(c);
    let mut c = d;
    c.coherence_threshold = 1.5;
    bad.push(c);
    let mut c = d;
    c.mono_coherence_veto = -0.1;
    bad.push(c);
    let mut c = d;
    c.freeze_ratio = 1.0;
    bad.push(c);
    let mut c = d;
    c.mismatch_blocks = 0;
    bad.push(c);
    for highpass in [
        HighPass::Internal { cutoff_hz: 5.0 },
        HighPass::Audio { cutoff_hz: 2_000.0 },
        HighPass::Internal {
            cutoff_hz: f32::NAN,
        },
        HighPass::Decisions {
            cutoff_hz: 0.0,
            postfilter: true,
        },
    ] {
        bad.push(EchoCancellerConfig { highpass, ..d });
    }
    for c in bad {
        let e = c.validate().expect_err("out of range");
        let msg = e.to_string();
        let panicked = std::panic::catch_unwind(|| EchoCanceller::new(c)).expect_err("new panics");
        let text = panicked
            .downcast_ref::<String>()
            .cloned()
            .unwrap_or_default();
        assert_eq!(text, msg, "constructor panics with the validation message");
    }
}

#[test]
fn aec_full_config_validation_names_the_single_offending_setting() {
    // T1.5f review: the message names the setting that is out of range, not a pair.
    let d = EchoCancellerConfig::default();
    let msg = |c: EchoCancellerConfig| c.validate().expect_err("out of range").to_string();
    let m = msg(EchoCancellerConfig {
        coherence_threshold: 1.5,
        ..d
    });
    assert!(
        m.contains("coherence_threshold") && !m.contains("mono_coherence_veto"),
        "{m}"
    );
    let m = msg(EchoCancellerConfig {
        mono_coherence_veto: -0.1,
        ..d
    });
    assert!(
        m.contains("mono_coherence_veto") && !m.contains("coherence_threshold"),
        "{m}"
    );
}

/// Output-to-echo power ratio (dB) of the loudest 32 ms window (hop one block) in which the
/// near end is silent (clean near-end power more than 30 dB below the echo) while echo is
/// present, from `from_s` on, and the pooled ratio over all such windows.
fn near_pause_echo_leak_db(o: &SimOutput, out: &[f32], from_s: f64) -> (f64, f64) {
    let p = |x: &[f32]| crate::dsp::power(x);
    let w = 4 * BLOCK;
    let (mut worst, mut sum_out, mut sum_echo) = (f64::NEG_INFINITY, 0.0f64, 0.0f64);
    let mut i = (from_s * 16_000.0) as usize;
    while i + w <= out.len() {
        let r = i..i + w;
        let (near, echo) = (p(&o.near_clean[r.clone()]), p(&o.echo[r.clone()]));
        if near < 1e-3 * echo && echo > 1e-6 {
            let o = p(&out[r]);
            worst = worst.max(10.0 * (o / echo).log10());
            sum_out += o;
            sum_echo += echo;
        }
        i += BLOCK;
    }
    (worst, 10.0 * (sum_out / sum_echo).log10())
}

/// Largest output-to-echo ratio accepted in a 32 ms near-end pause during far-end speech
/// (T1.5f): the false mono double-talk calls passed echo bursts at the echo's own level
/// (+0.2 dB on `double-talk-ser-0`); the older leak at 9.38 s, outside double talk, stays
/// at about -9 dB (loudspeaker-distortion residue above the level class's learned ratio).
const MAX_NEAR_PAUSE_LEAK_DB: f64 = -6.0;

#[test]
fn aec_full_mono_doubletalk_is_not_called_in_near_end_pauses() {
    // T1.5f: late reverberation at far-end offsets that the linear stage has not cancelled
    // made the mono linear witness call double talk in near-end pauses (6.37 s and 10.78 s
    // on double-talk-ser-0); the call lowered the suppression and froze the filter, and the
    // echo burst passed. While the reference explains the mic (coherence veto) the call
    // neither freezes the filter nor softens the suppression.
    //
    // Held on the unfiltered canceller (`HighPass::Off`). With the default 30 Hz audio
    // high-pass (T6.23b) the worst pause leak was -5.9 dB against the -6 dB bound; the owner
    // accepted that miss on 2026-10-27 as part of the S30 trade. Still open after T6.23c
    // (private notes: a 41-63 Hz linear-estimate overshoot in the last far-end tail
    // that S30's small learned low-bin ratios leave unsuppressed). The bound is unchanged.
    let o = run_scenario("double-talk-ser-0");
    let from_s = o.near_regions[0].start as f64 / 16_000.0;
    let unfiltered = with_highpass(HighPass::Off);
    let (out, _) = cancel_with(&o, unfiltered);
    let (worst, pooled) = near_pause_echo_leak_db(&o, &out, from_s);
    eprintln!("double-talk-ser-0 near-end pauses: worst {worst:.1} dB, pooled {pooled:.1} dB");
    assert!(
        worst <= MAX_NEAR_PAUSE_LEAK_DB,
        "echo burst {worst:.1} dB in a near-end pause"
    );
    // Negative control: with the veto at 1.0 the leak is there, so the bound measures it.
    // 1.0 is "almost off", not off: `xi^2` reads 1 while it is not measured, right after
    // every realignment, so a few vetoes remain. Once the false calls are fixed at the root
    // this control must be replaced by an assertion on the detector itself (no call in
    // these pauses), never relaxed.
    let off = EchoCancellerConfig {
        mono_coherence_veto: 1.0,
        ..unfiltered
    };
    let (out, _) = cancel_with(&o, off);
    let (worst_off, pooled_off) = near_pause_echo_leak_db(&o, &out, from_s);
    eprintln!("without the veto: worst {worst_off:.1} dB, pooled {pooled_off:.1} dB");
    assert!(
        worst_off > MAX_NEAR_PAUSE_LEAK_DB,
        "control: no leak without the veto ({worst_off:.1} dB)"
    );
    assert!(
        pooled < pooled_off,
        "veto raised the pooled leak ({pooled:.1} vs {pooled_off:.1} dB)"
    );
}

#[test]
fn aec_full_far_only_double_talk_calls_are_all_vetoed() {
    // T1.5 review (fragile 5% margin): every false double-talk call of the mono linear
    // witness in far-end-only speech after 3 s is overridden by the coherence veto
    // (T1.5f), so none freezes the linear stage or softens the suppression; they only
    // stop the models from learning. Measured 2026-10-24 (raw calls per scenario):
    // far-only 4.62%, seeds +1000..+5000 2.67-4.68%, robustness grid 2.96-6.07%
    // (0 ms +200 ppm the highest); vetoed and frozen counts below were 100% and 0 on
    // every one of them. The raw 5% bound in
    // `aec_full_doubletalk_detector_freezes_adaptation` stays as it is.
    let file = crate::testutil::scenarios();
    let base = file.get("far-only").unwrap();
    let mut variants = vec![base.clone()];
    for &d in &file.robustness.delays_ms {
        for &p in &file.robustness.drifts_ppm {
            variants.push(base.with_delay_and_drift(d, p));
        }
    }
    for scn in variants {
        let o = crate::testutil::run(&scn);
        let (_, status) = cancel_with(&o, EchoCancellerConfig::default());
        let late = &status[3 * 125..];
        let calls = late.iter().filter(|s| s.doubletalk).count();
        let effective = late.iter().filter(|s| s.doubletalk && !s.vetoed).count();
        let frozen = late.iter().filter(|s| s.frozen).count();
        eprintln!(
            "{}: {calls}/{} blocks called, {effective} not vetoed, {frozen} frozen",
            scn.name,
            late.len()
        );
        assert_eq!(
            effective, 0,
            "{}: far-end speech taken for double talk",
            scn.name
        );
        assert_eq!(
            frozen, 0,
            "{}: far-end speech froze the linear stage",
            scn.name
        );
    }
    // Negative control: with the veto at 1.0 ("almost off") the calls are effective. It
    // runs on the unfiltered canceller (`HighPass::Off`), the condition the veto was built
    // for: with the default 30 Hz audio high-pass (T6.23b) the false far-only calls do not
    // occur at all (0 effective calls with the veto at 1.0), so the control would prove
    // nothing there.
    let o = run_scenario("far-only");
    let off = EchoCancellerConfig {
        mono_coherence_veto: 1.0,
        ..with_highpass(HighPass::Off)
    };
    let (_, status) = cancel_with(&o, off);
    let effective = status[3 * 125..]
        .iter()
        .filter(|s| s.doubletalk && !s.vetoed)
        .count();
    eprintln!("far-only, veto at 1.0: {effective} calls not vetoed");
    assert!(effective > 0, "control: no effective call without the veto");
}

// --- High-pass around the canceller (T6.23b) ---

fn with_highpass(highpass: HighPass) -> EchoCancellerConfig {
    EchoCancellerConfig {
        highpass,
        ..EchoCancellerConfig::default()
    }
}

/// Low-frequency room rumble (not echo): seeded noise through four one-pole low-passes at
/// `corner_hz`, scaled to `power` mean square.
fn rumble(n: usize, corner_hz: f64, power: f64) -> Vec<f32> {
    let a = (-std::f64::consts::TAU * corner_hz / 16_000.0).exp();
    let mut state = 0x2545_f491_u64;
    let mut z = [0.0f64; 4];
    let mut x: Vec<f64> = (0..n)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            let mut v = (state >> 11) as f64 / (1u64 << 53) as f64 - 0.5;
            for s in &mut z {
                *s = a * *s + (1.0 - a) * v;
                v = *s;
            }
            v
        })
        .collect();
    let ms = x.iter().map(|v| v * v).sum::<f64>() / n as f64;
    let g = (power / ms).sqrt();
    x.iter_mut().for_each(|v| *v *= g);
    x.into_iter().map(|v| v as f32).collect()
}

fn mean_square(x: &[f32]) -> f64 {
    x.iter().map(|v| f64::from(*v).powi(2)).sum::<f64>() / x.len().max(1) as f64
}

/// Far-only call whose mic also holds rumble at twice the echo's power (as on a real
/// recording, where most of the mic's power on far-end-only blocks was below 100 Hz and not
/// echo).
fn far_only_with_rumble() -> (SimOutput, Vec<f32>) {
    let mut o = run_scenario("far-only");
    let r = rumble(o.mic.len(), 40.0, 2.0 * mean_square(&o.echo));
    for (m, v) in o.mic.iter_mut().zip(&r) {
        *m += v;
    }
    (o, r)
}

fn converged_share(status: &[BlockStatus], from_block: usize) -> f64 {
    let s = &status[from_block..];
    s.iter().filter(|b| b.converged).count() as f64 / s.len() as f64
}

#[test]
fn aec_full_highpass_internal_converges_under_low_frequency_rumble() {
    // T6.23b: rumble below 100 Hz keeps the broadband linear-stage ERLE under the
    // `converged_ratio` bar (the mechanism a private recording showed), so double talk is never detected; the
    // internal and decision high-passes judge the canceller on the band the echo lives in.
    let (o, _) = far_only_with_rumble();
    let skip = 5 * 16_000 / BLOCK;
    let (_, off) = cancel_with(&o, with_highpass(HighPass::Off));
    let off = converged_share(&off, skip);
    assert!(
        off < 0.2,
        "negative control: without the filter converged {off:.2}"
    );
    // The internal filter also keeps the rumble out of adaptation; the decision filter only
    // out of the decisions, and rumble at twice the echo's power still slows the unfiltered
    // linear stage's own step control, so its in-band ERLE crosses the bar less often
    // (measured: I80 0.99, D80 0.27, D80w 0.27, D100w 0.26 of the blocks).
    for (label, min) in [("I80", 0.9), ("D80", 0.2), ("D80w", 0.2), ("D100w", 0.2)] {
        let (_, st) = cancel_with(&o, with_highpass(HighPass::parse(label).unwrap()));
        let share = converged_share(&st, skip);
        println!("{label}: converged {share:.2} (unfiltered {off:.2})");
        assert!(share > min && share > off, "{label}: converged {share:.2}");
    }
}

#[test]
fn aec_full_highpass_internal_keeps_the_rumble_and_removes_the_echo() {
    let (o, r) = far_only_with_rumble();
    let (out, _) = cancel_with(&o, with_highpass(HighPass::Internal { cutoff_hz: 80.0 }));
    let n = out.len();
    // Echo above the cutoff: ERLE after a 150 Hz high-pass of both signals meets the
    // far-end ERLE threshold and beats the unfiltered canceller on the same call.
    let hp = |x: &[f32]| HighPassFilter::new(150.0, 16_000).filtered(x);
    let erle = |mic: &[f32], out: &[f32]| {
        segmental_erle(
            &hp(&mic[..out.len()]),
            &hp(out),
            16_000,
            &ErleConfig::default(),
        )
        .unwrap()
        .median_db
    };
    let with = erle(&o.mic, &out);
    let off = erle(&o.mic, &cancel_with(&o, with_highpass(HighPass::Off)).0);
    let min = threshold("aec.erle_farend");
    assert!(
        with >= min && with > off,
        "ERLE above 150 Hz {with:.1} dB, unfiltered {off:.1} dB"
    );
    // The rumble (not echo) passes where no echo is predicted: output power below 40 Hz
    // in the far end's pauses (after the first 5 s) within 1 dB of the rumble's. While the
    // far end talks the low-frequency product model (T1.6c), learned while only the far
    // end talks, also predicts part of the stationary rumble; there the internal filter
    // keeps about as much of it as the unfiltered canceller (within 0.5 dB; measured
    // -8.30 dB against -8.16 dB).
    let lp = |x: &[f32]| {
        let a = (-std::f64::consts::TAU * 40.0 / 16_000.0).exp() as f32;
        let mut z = [0.0f32; 2];
        x.iter()
            .map(|&v| {
                z[0] = a * z[0] + (1.0 - a) * v;
                z[1] = a * z[1] + (1.0 - a) * z[0];
                z[1]
            })
            .collect::<Vec<f32>>()
    };
    let from = 5 * 16_000;
    let pause: Vec<bool> = (0..n)
        .map(|i| i >= from && !o.far_regions.iter().any(|r| r.contains(&i)))
        .collect();
    let level = |x: &[f32], sel: &dyn Fn(usize) -> bool| {
        let l = lp(x);
        let v: Vec<f32> = (0..n).filter(|&i| sel(i)).map(|i| l[i]).collect();
        mean_square(&v)
    };
    let db = |a: f64, b: f64| 10.0 * (a / b).log10();
    let in_pause = |i: usize| pause[i];
    let talking = |i: usize| i >= from && !pause[i];
    let off_out = cancel_with(&o, with_highpass(HighPass::Off)).0;
    let kept_pause = db(level(&out, &in_pause), level(&r[..n], &in_pause));
    let kept_talk = db(level(&out, &talking), level(&r[..n], &talking));
    let off_talk = db(level(&off_out, &talking), level(&r[..n], &talking));
    println!(
        "rumble kept: pauses {kept_pause:.2} dB, far end talking {kept_talk:.2} dB (unfiltered {off_talk:.2} dB), pause samples {}",
        pause.iter().filter(|p| **p).count()
    );
    assert!(
        kept_pause.abs() < 1.0,
        "rumble in pauses changed by {kept_pause:.2} dB"
    );
    assert!(
        kept_talk >= off_talk - 0.5,
        "rumble while the far end talks {kept_talk:.2} dB, unfiltered {off_talk:.2} dB"
    );
}

#[test]
fn aec_full_highpass_without_an_echo_path_passes_the_mic_through() {
    // Internal: the all-pass of the mic (unit gain at every frequency), delayed. Audio: the
    // high-passed mic, delayed. Decisions: the mic itself, delayed.
    let o = run_scenario("near-only");
    let n = o.mic.len() / BLOCK * BLOCK;
    let mut mic = o.mic[..n].to_vec();
    for (m, v) in mic.iter_mut().zip(rumble(n, 40.0, 1e-4)) {
        *m += v;
    }
    let silent = vec![0.0f32; n];
    for (hp, expect) in [
        (
            HighPass::Internal { cutoff_hz: 80.0 },
            AllPassFilter::matching(80.0, 16_000).filtered(&mic),
        ),
        (
            HighPass::Audio { cutoff_hz: 80.0 },
            HighPassFilter::new(80.0, 16_000).filtered(&mic),
        ),
        (HighPass::parse("D80").unwrap(), mic.clone()),
        (HighPass::parse("D80w").unwrap(), mic.clone()),
    ] {
        let mut aec = EchoCanceller::new(with_highpass(hp));
        let out = aec.process_vec(&mic, &silent);
        assert_passes_through(&out[aec.output_latency()..], &expect, &hp.label());
    }
}

#[test]
fn aec_full_highpass_stereo_with_identical_channels_is_the_mono_canceller() {
    let o = run_scenario("double-talk-ser-0");
    let n = o.mic.len() / BLOCK * BLOCK;
    for hp in [
        HighPass::Internal { cutoff_hz: 80.0 },
        HighPass::Audio { cutoff_hz: 100.0 },
        HighPass::parse("D80").unwrap(),
        HighPass::parse("D100w").unwrap(),
    ] {
        let mono =
            EchoCanceller::new(with_highpass(hp)).process_vec(&o.mic[..n], &o.reference[..n]);
        let mut stereo = vec![0.0f32; n];
        EchoCanceller::new(with_highpass(hp)).process_stereo(
            &o.mic[..n],
            &o.reference[..n],
            &o.reference[..n],
            &mut stereo,
        );
        assert_eq!(mono, stereo, "{}", hp.label());
    }
}

#[test]
fn aec_full_highpass_audio_is_the_canceller_on_filtered_inputs() {
    // Audio mode is exactly the unfiltered canceller fed the filtered mic and reference.
    let o = run_scenario("double-talk-ser-0");
    let n = o.mic.len() / BLOCK * BLOCK;
    let f = |x: &[f32]| HighPassFilter::new(100.0, 16_000).filtered(x);
    let a = EchoCanceller::new(with_highpass(HighPass::Audio { cutoff_hz: 100.0 }))
        .process_vec(&o.mic[..n], &o.reference[..n]);
    let b = EchoCanceller::new(with_highpass(HighPass::Off))
        .process_vec(&f(&o.mic[..n]), &f(&o.reference[..n]));
    assert_eq!(a, b);
}

#[test]
fn aec_full_highpass_decisions_keep_the_unfiltered_linear_output() {
    // Design D: adaptation is the unfiltered canceller's, so the linear output is identical
    // until the first block where a decision (freeze) differs; on a far-only call the
    // whole-call linear-stage ERLE stays within 0.5 dB of the unfiltered canceller's.
    let o = run_scenario("far-only");
    let n = o.mic.len() / BLOCK * BLOCK;
    let lin = |hp: HighPass| {
        let mut aec = EchoCanceller::new(with_highpass(hp));
        let mut out = vec![0.0f32; n];
        let mut l = vec![0.0f32; n];
        aec.process_with_linear(&o.mic[..n], &o.reference[..n], &mut out, &mut l);
        l
    };
    let off = lin(HighPass::Off);
    let erle = |l: &[f32]| 10.0 * (mean_square(&o.mic[..n]) / mean_square(l)).log10();
    for label in ["D80", "D80w"] {
        let d = lin(HighPass::parse(label).unwrap());
        let first_diff = off.iter().zip(&d).position(|(a, b)| a != b).unwrap_or(n);
        let (a, b) = (erle(&off), erle(&d));
        println!(
            "{label}: linear output equal for {first_diff} samples; linear ERLE {b:.2} dB, unfiltered {a:.2} dB"
        );
        assert!(
            first_diff > 16_000,
            "{label}: linear output differs from {first_diff}"
        );
        assert!(
            (a - b).abs() < 0.5,
            "{label}: linear ERLE {b:.2} dB, unfiltered {a:.2} dB"
        );
    }
}

#[test]
fn aec_full_highpass_audio_passthrough_is_scored_against_the_filtered_input() {
    // The owner (PLAN.md 4.2): with an audio high-pass, aec.passthrough compares the
    // output with the high-passed input. The same output against the unfiltered input
    // scores as before (below the threshold). The Off scorer (`eval::score`) is unchanged.
    let min = threshold("aec.passthrough");
    let file = crate::testutil::scenarios();
    for name in ["near-only", "switch-speakers-to-headphones"] {
        let scn = file.get(name).unwrap();
        let o = run_scenario(name);
        let cfg = with_highpass(HighPass::Audio { cutoff_hz: 30.0 });
        let run = eval::run_canceller(&o, cfg);
        let filtered = eval::score_for(scn, &o, &run, &cfg).passthrough.unwrap();
        let unfiltered = eval::score(scn, &o, &run).passthrough.unwrap();
        println!(
            "{name}: passthrough {filtered:.2} dB against the filtered input, {unfiltered:.2} dB against the mic"
        );
        assert!(
            filtered >= min,
            "{name}: {filtered:.2} dB against the filtered input"
        );
        assert!(
            unfiltered < min,
            "{name}: {unfiltered:.2} dB against the unfiltered mic"
        );
        // With the high-pass off (not the default since S30): `score_for` equals `score`.
        let off = EchoCancellerConfig::default().with_highpass(HighPass::Off);
        let prod = eval::run_canceller(&o, off);
        assert_eq!(
            eval::score_for(scn, &o, &prod, &off).passthrough,
            eval::score(scn, &o, &prod).passthrough
        );
    }
}
