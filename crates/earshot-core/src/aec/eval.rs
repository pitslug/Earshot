//! Scoring of the M1 echo-canceller metrics on simulated calls (PLAN.md 4.0, 4.2 `aec.*`).
//!
//! Pure functions over buffers, shared by the unit tests and `accept m1` (via the
//! `aec_eval` example), so the acceptance numbers are produced by the same code the
//! tests exercise. Thresholds are passed in; nothing here reads files.
//!
//! Scenario classes are derived from the scenario fields, never from names:
//!
//! - **far-end only** (far speech, echo, no near end, no noise, no events): `aec.erle_farend`
//!   (segmental ERLE, median of 1 s windows after 2 s) and `aec.convergence` (time to 15 dB
//!   ERLE from the start); without loudspeaker clipping (`softclip` unset, for example
//!   `far-only-linear`) also `aec.erle_linear` (the same ERLE on the linear stage's output).
//! - **double talk** (far and near speech, echo, no noise, no events):
//!   `aec.doubletalk_sisdr`, SI-SDR of the output against the clean near end over the
//!   labelled near-end speech regions, gated at SER >= 0 dB; at negative SER the same
//!   value is informational (`doubletalk_sisdr_informational`, never a failure). With an
//!   audio high-pass the same output is also scored against the high-passed near end
//!   (`doubletalk_sisdr_highpassed`, informational only, T6.23c).
//! - **no echo** (no far end, or no echo path) and **echo stops** (`echo_until_s`):
//!   `aec.passthrough`, SI-SDR of the output against the mic over the whole call or after
//!   the switch. When the canceller deliberately high-passes the app's audio
//!   ([`HighPass::Audio`]), the mic is scored after that same high-pass ([`score_for`];
//!   The owner's decision, PLAN.md 4.2): passing the input through is then passing the
//!   filtered input through. No other metric changes with the filter.
//! - **echo appears or changes** (`path_change_s`, `delay_jump`, `echo_from_s`):
//!   `aec.path_change`, time to 15 dB ERLE after the event, with near-end speech regions
//!   (plus the room tail) excluded from the ERLE windows.
//!
//! Times that are never reached are `f64::INFINITY`, which fails any `<=` threshold.
//!
//! Recovery and the scenario windows: a recovery of `t` needs scored ERLE windows up to
//! `t + 0.5 s + 1 s` (window plus hold) after the event. Windows that reach into near-end
//! speech after the scenario's `near_silent_s` stretch are scored on their unblanked part
//! only, and fully blanked windows are skipped, so the hold never counts near-end speech;
//! `aec_eval_recovery_is_measurable_within_the_threshold` checks with an oracle canceller
//! that every echo-change scenario can score a recovery inside `aec.path_change`, and
//! `aec_eval_late_recovery_inside_the_bound_is_measurable` that a recovery at 1.9 s (echo
//! left in for 1.35 s) is still scored, with its hold reaching past `near_silent_s`.

use std::ops::Range;

use super::{BLOCK, EchoCanceller, EchoCancellerConfig, HighPass, HighPassFilter};
use crate::metrics::erle::{
    CONVERGED_ERLE_DB, ConvergenceConfig, ErleConfig, segmental_erle, time_to_erle,
};
use crate::metrics::sisdr::si_sdr;
use crate::sim::{Scenario, SimOutput};

/// Excluded tail after a near-end region when scoring echo-only ERLE (room decay).
pub const NEAR_TAIL_S: f64 = 0.3;

/// Output of one canceller run over a simulated call (whole blocks only), with the
/// canceller's output latency already removed (see [`compensate`]).
#[derive(Debug, Clone, PartialEq)]
pub struct Run {
    /// Canceller output, aligned with the mic.
    pub out: Vec<f32>,
    /// Linear-stage output (mic minus echo estimate), for `aec.erle_linear`.
    pub linear: Vec<f32>,
}

/// Runs a fresh canceller over `sim`: with its stereo reference (`reference_lr`) through
/// [`EchoCanceller::process_stereo_with_linear`] when it has one, else the mono reference.
#[must_use]
pub fn run_canceller(sim: &SimOutput, cfg: EchoCancellerConfig) -> Run {
    let n = sim.mic.len() / BLOCK * BLOCK;
    let mut aec = EchoCanceller::new(cfg);
    let mut out = vec![0.0f32; n];
    let mut linear = vec![0.0f32; n];
    match &sim.reference_lr {
        Some([l, r]) => {
            aec.process_stereo_with_linear(&sim.mic[..n], &l[..n], &r[..n], &mut out, &mut linear)
        }
        None => aec.process_with_linear(&sim.mic[..n], &sim.reference[..n], &mut out, &mut linear),
    }
    compensate(out, linear, aec.output_latency())
}

/// Removes the canceller's fixed output latency (PLAN.md 3.3): drops the first `latency`
/// output samples so output sample `n` lines up with mic sample `n`, and trims the
/// zero-latency linear output to the same length. Every mic-derived metric (ERLE windows,
/// recovery times, SI-SDR, passthrough) is then scored on aligned signals.
#[must_use]
pub fn compensate(out: Vec<f32>, mut linear: Vec<f32>, latency: usize) -> Run {
    let latency = latency.min(out.len());
    let out = out[latency..].to_vec();
    linear.truncate(out.len());
    Run { out, linear }
}

/// Informational scenario (`Scenario::informational`, PLAN.md T1.2a): scored and
/// reported, but excluded from every `aec.*` measurement and from `aec.robustness`.
#[must_use]
pub fn is_informational(scn: &Scenario) -> bool {
    scn.informational
}

/// Bulk delay (ms) of a robustness grid point from which a stereo scenario's
/// `aec.convergence` is informational (decided by the owner: the clean-transcript
/// requirement is met by transcript-level de-duplication, T2.4e).
pub const STEREO_CONVERGENCE_INFO_DELAY_MS: f64 = 500.0;

/// Whether `aec.convergence` of a robustness grid point (`scn` simulated at the grid's
/// nominal bulk delay `grid_delay_ms`) is reported but not gated: stereo scenarios
/// (`Scenario::stereo` set) at [`STEREO_CONVERGENCE_INFO_DELAY_MS`] and above. Derived from
/// the scenario's fields and the grid point, never from names; every other metric of the
/// point, and every mono scenario, gates as before.
#[must_use]
pub fn grid_convergence_informational(scn: &Scenario, grid_delay_ms: f64) -> bool {
    scn.stereo.is_some() && grid_delay_ms >= STEREO_CONVERGENCE_INFO_DELAY_MS
}

/// Which metrics a scenario contributes to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Class {
    /// Far end only: ERLE, linear ERLE, convergence.
    FarOnly,
    /// Double talk: SI-SDR against the clean near end.
    DoubleTalk,
    /// No echo path at all: passthrough over the whole call.
    NoEcho,
    /// Echo stops at the given time: passthrough after it.
    EchoStops,
    /// Echo appears or changes at the given time: recovery time after it.
    EchoChanges,
    /// Not scored by a threshold (for example double talk with noise).
    Unscored,
}

/// Class of `scn` and, for event classes, the event time (s).
#[must_use]
pub fn classify(scn: &Scenario) -> (Class, f64) {
    let event = scn
        .path_change_s
        .or(scn.delay_jump.map(|j| j.at_s))
        .or(scn.echo_from_s);
    if let Some(t) = event {
        return (Class::EchoChanges, t);
    }
    if let Some(t) = scn.echo_until_s {
        return (Class::EchoStops, t);
    }
    if !scn.far_speech || !scn.echo {
        return (Class::NoEcho, 0.0);
    }
    if scn.noise_snr_db.is_some() {
        return (Class::Unscored, 0.0);
    }
    if scn.near_speech {
        (Class::DoubleTalk, 0.0)
    } else {
        (Class::FarOnly, 0.0)
    }
}

/// Metric values of one scenario (`None` where the class does not score it).
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Scores {
    /// `aec.erle_farend` (dB).
    pub erle_farend: Option<f64>,
    /// `aec.erle_linear` (dB).
    pub erle_linear: Option<f64>,
    /// `aec.convergence` (s).
    pub convergence: Option<f64>,
    /// `aec.path_change` (s).
    pub path_change: Option<f64>,
    /// `aec.doubletalk_sisdr` (dB).
    pub doubletalk_sisdr: Option<f64>,
    /// Double-talk SI-SDR at negative SER (dB): reported, never gated.
    pub doubletalk_sisdr_informational: Option<f64>,
    /// Double-talk SI-SDR (dB, any SER) against the clean near end passed through the
    /// canceller's own audio high-pass (run from sample 0, as for `aec.passthrough`); only
    /// with [`HighPass::Audio`]. Informational (T6.23c step 1): it shows how
    /// much of the official value's loss under the filter is the removed sub-cutoff near-end
    /// content. Never a measurement, a failure or a baselined value.
    pub doubletalk_sisdr_highpassed: Option<f64>,
    /// `aec.passthrough` (dB).
    pub passthrough: Option<f64>,
}

/// Threshold values (from `acceptance/thresholds.toml`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Limits {
    /// `aec.erle_farend` (>=, dB).
    pub erle_farend: f64,
    /// `aec.erle_linear` (>=, dB).
    pub erle_linear: f64,
    /// `aec.convergence` (<=, s).
    pub convergence: f64,
    /// `aec.path_change` (<=, s).
    pub path_change: f64,
    /// `aec.doubletalk_sisdr` (>=, dB).
    pub doubletalk_sisdr: f64,
    /// `aec.passthrough` (>=, dB).
    pub passthrough: f64,
}

impl Scores {
    /// Descriptions of every scored metric that misses its limit.
    #[must_use]
    pub fn failures(&self, lim: &Limits) -> Vec<String> {
        let mut f = Vec::new();
        // +inf ERLE is a silent (muted) output, never perfect cancellation: it fails.
        for (name, v, min) in [
            ("erle_farend", self.erle_farend, lim.erle_farend),
            ("erle_linear", self.erle_linear, lim.erle_linear),
        ] {
            if let Some(v) = v
                && v == f64::INFINITY
            {
                f.push(format!("{name} inf: a muted output"));
            } else if let Some(v) = v
                && (v.is_nan() || v < min)
            {
                f.push(format!("{name} {v:.2} < {min}"));
            }
        }
        let mut ge = |name: &str, v: Option<f64>, min: f64| {
            if let Some(v) = v
                && (v.is_nan() || v < min)
            {
                f.push(format!("{name} {v:.2} < {min}"));
            }
        };
        ge(
            "doubletalk_sisdr",
            self.doubletalk_sisdr,
            lim.doubletalk_sisdr,
        );
        ge("passthrough", self.passthrough, lim.passthrough);
        let mut le = |name: &str, v: Option<f64>, max: f64| {
            if let Some(v) = v
                && (v.is_nan() || v > max)
            {
                f.push(format!("{name} {v:.2} > {max}"));
            }
        };
        le("convergence", self.convergence, lim.convergence);
        le("path_change", self.path_change, lim.path_change);
        f
    }
}

fn concat(x: &[f32], regions: &[Range<usize>]) -> Vec<f32> {
    let mut v = Vec::new();
    for r in regions {
        let r = r.start.min(x.len())..r.end.min(x.len());
        v.extend_from_slice(&x[r]);
    }
    v
}

/// SI-SDR for canceller output. `metrics::sisdr::si_sdr` owns the degenerate cases
/// (including a silent estimate, which it scores `-inf`); the only case it leaves open is
/// a silent reference (`None`), decided here: a silent estimate of a silent reference is
/// `+inf` (nothing to reproduce), anything else is `-inf`. Whether the estimate is silent
/// is asked of `si_sdr` itself (as a reference), so no second epsilon is defined here.
#[must_use]
pub fn guarded_si_sdr(estimate: &[f32], reference: &[f32]) -> f64 {
    if let Some(v) = si_sdr(estimate, reference) {
        return v;
    }
    let comparable = !estimate.is_empty() && estimate.len() == reference.len();
    let estimate_silent = si_sdr(estimate, estimate).is_none();
    if comparable && estimate_silent {
        f64::INFINITY
    } else {
        f64::NEG_INFINITY
    }
}

fn converge_time(mic: &[f32], out: &[f32], rate: u32, from_s: f64) -> f64 {
    time_to_erle(
        mic,
        out,
        rate,
        from_s,
        CONVERGED_ERLE_DB,
        &ConvergenceConfig::default(),
    )
    .unwrap_or(f64::INFINITY)
}

/// Scores one scenario run of a canceller without an audio high-pass.
#[must_use]
pub fn score(scn: &Scenario, sim: &SimOutput, run: &Run) -> Scores {
    score_for(
        scn,
        sim,
        run,
        &EchoCancellerConfig::default().with_highpass(HighPass::Off),
    )
}

/// Scores one scenario run of a canceller with settings `cfg`: as [`score`], except that
/// with an audio high-pass ([`HighPass::Audio`]) `aec.passthrough` compares the output with
/// the mic after the same high-pass (run from the start of the call, as the canceller runs
/// it).
#[must_use]
pub fn score_for(scn: &Scenario, sim: &SimOutput, run: &Run, cfg: &EchoCancellerConfig) -> Scores {
    let n = run.out.len();
    let rate = sim.rate.hz();
    let mic = &sim.mic[..n];
    let mut s = Scores::default();
    let (class, at) = classify(scn);
    match class {
        Class::FarOnly => {
            let erle = |x: &[f32]| {
                segmental_erle(mic, x, rate, &ErleConfig::default())
                    .map_or(f64::NEG_INFINITY, |r| r.median_db)
            };
            s.erle_farend = Some(erle(&run.out));
            // aec.erle_linear only without loudspeaker clipping (PLAN.md 4.0, T1.4):
            // under clipping the linear stage is held to the best linear canceller instead.
            if scn.softclip.is_none() {
                s.erle_linear = Some(erle(&run.linear));
            }
            s.convergence = Some(converge_time(mic, &run.out, rate, 0.0));
        }
        Class::DoubleTalk => {
            let est = concat(&run.out, &sim.near_regions);
            let clean = concat(&sim.near_clean[..n], &sim.near_regions);
            let v = guarded_si_sdr(&est, &clean);
            // PLAN.md 4.0: aec.doubletalk_sisdr gates at SER 0; below it the value is
            // informational (near-end usefulness there is gated by asr.wer_aec_delta).
            if scn.ser_db >= 0.0 {
                s.doubletalk_sisdr = Some(v);
            } else {
                s.doubletalk_sisdr_informational = Some(v);
            }
            if let HighPass::Audio { cutoff_hz } = cfg.highpass {
                let near = HighPassFilter::new(cutoff_hz, rate).filtered(&sim.near_clean[..n]);
                let clean = concat(&near, &sim.near_regions);
                s.doubletalk_sisdr_highpassed = Some(guarded_si_sdr(&est, &clean));
            }
        }
        Class::NoEcho | Class::EchoStops => {
            let from = ((at * f64::from(rate)) as usize).min(n);
            let input = match cfg.highpass {
                HighPass::Audio { cutoff_hz } => HighPassFilter::new(cutoff_hz, rate).filtered(mic),
                _ => mic.to_vec(),
            };
            s.passthrough = Some(guarded_si_sdr(&run.out[from..], &input[from..]));
        }
        Class::EchoChanges => {
            // Echo-only windows: blank near-end speech (and its room tail) in both signals.
            let tail = (NEAR_TAIL_S * f64::from(rate)) as usize;
            let mut m = mic.to_vec();
            let mut o = run.out.clone();
            for r in &sim.near_regions {
                let r = r.start.min(n)..(r.end + tail).min(n);
                m[r.clone()].iter_mut().for_each(|v| *v = 0.0);
                o[r].iter_mut().for_each(|v| *v = 0.0);
            }
            s.path_change = Some(converge_time(&m, &o, rate, at));
        }
        Class::Unscored => {}
    }
    s
}

/// Folds per-scenario scores into headline values: worst case per metric (lowest dB,
/// longest time). `None` if no scenario scored that metric.
#[must_use]
pub fn worst(all: &[Scores]) -> Scores {
    let min = |f: fn(&Scores) -> Option<f64>| all.iter().filter_map(f).reduce(f64::min);
    let max = |f: fn(&Scores) -> Option<f64>| all.iter().filter_map(f).reduce(f64::max);
    Scores {
        erle_farend: min(|s| s.erle_farend),
        erle_linear: min(|s| s.erle_linear),
        convergence: max(|s| s.convergence),
        path_change: max(|s| s.path_change),
        doubletalk_sisdr: min(|s| s.doubletalk_sisdr),
        doubletalk_sisdr_informational: min(|s| s.doubletalk_sisdr_informational),
        doubletalk_sisdr_highpassed: min(|s| s.doubletalk_sisdr_highpassed),
        passthrough: min(|s| s.passthrough),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{run_scenario, scenarios};

    #[test]
    fn aec_eval_classifies_every_scenario_by_its_fields() {
        let file = scenarios();
        let class = |n: &str| classify(file.get(n).unwrap()).0;
        assert_eq!(class("far-only"), Class::FarOnly);
        assert_eq!(class("far-only-linear"), Class::FarOnly);
        assert_eq!(class("delay-450ms"), Class::FarOnly);
        assert_eq!(class("drift-plus-200ppm"), Class::FarOnly);
        assert_eq!(class("double-talk-ser-0"), Class::DoubleTalk);
        assert_eq!(class("double-talk-ser-10"), Class::DoubleTalk);
        assert_eq!(class("near-only"), Class::NoEcho);
        assert_eq!(class("headphones"), Class::NoEcho);
        assert_eq!(class("switch-speakers-to-headphones"), Class::EchoStops);
        assert_eq!(class("switch-headphones-to-speakers"), Class::EchoChanges);
        assert_eq!(class("path-change"), Class::EchoChanges);
        assert_eq!(class("delay-jump"), Class::EchoChanges);
        assert_eq!(class("noisy-office"), Class::Unscored);
        // Each threshold has at least one base scenario.
        let classes: Vec<Class> = file.scenario.iter().map(|s| classify(s).0).collect();
        for c in [
            Class::FarOnly,
            Class::DoubleTalk,
            Class::NoEcho,
            Class::EchoStops,
            Class::EchoChanges,
        ] {
            assert!(classes.contains(&c), "{c:?}");
        }
    }

    #[test]
    fn aec_eval_identity_and_mute_are_scored_as_expected() {
        // A canceller that passes the mic through: perfect passthrough, 0 dB ERLE.
        let o = run_scenario("near-only");
        let scn = scenarios().get("near-only").unwrap();
        let n = o.mic.len() / BLOCK * BLOCK;
        let ident = Run {
            out: o.mic[..n].to_vec(),
            linear: o.mic[..n].to_vec(),
        };
        assert_eq!(score(scn, &o, &ident).passthrough, Some(f64::INFINITY));

        let o = run_scenario("far-only");
        let scn = scenarios().get("far-only").unwrap();
        let ident = Run {
            out: o.mic[..n].to_vec(),
            linear: o.mic[..n].to_vec(),
        };
        let s = score(scn, &o, &ident);
        assert!(s.erle_farend.unwrap().abs() < 1e-9);
        // Clipped far-only does not score the linear stage; far-only-linear does.
        assert_eq!(s.erle_linear, None);
        let lin = scenarios().get("far-only-linear").unwrap();
        assert!(
            score(lin, &run_scenario("far-only-linear"), &ident)
                .erle_linear
                .is_some()
        );
        assert_eq!(s.convergence, Some(f64::INFINITY));
        // Muting everything gives infinite ERLE but a muted near end in double talk
        // scores badly: the metrics cannot be met by muting (PLAN.md 4.0).
        let o = run_scenario("double-talk-ser-0");
        let scn = scenarios().get("double-talk-ser-0").unwrap();
        let mute = Run {
            out: vec![0.0; n],
            linear: vec![0.0; n],
        };
        let s = score(scn, &o, &mute);
        assert!(s.doubletalk_sisdr.unwrap() == f64::NEG_INFINITY);
        // The same for passthrough: muting the mic is not passing it through.
        let o = run_scenario("near-only");
        let scn = scenarios().get("near-only").unwrap();
        assert_eq!(score(scn, &o, &mute).passthrough, Some(f64::NEG_INFINITY));
    }

    #[test]
    fn aec_eval_guarded_si_sdr_rejects_silence() {
        let x: Vec<f32> = (0..100).map(|i| (i as f32 * 0.3).sin()).collect();
        assert_eq!(guarded_si_sdr(&[0.0; 100], &x), f64::NEG_INFINITY);
        assert_eq!(guarded_si_sdr(&x, &[0.0; 100]), f64::NEG_INFINITY);
        assert_eq!(guarded_si_sdr(&[0.0; 100], &[0.0; 100]), f64::INFINITY);
        assert_eq!(guarded_si_sdr(&x, &x), f64::INFINITY);
        let half: Vec<f32> = x.iter().map(|v| v * 0.5).collect();
        assert_eq!(guarded_si_sdr(&half, &x), f64::INFINITY, "scale invariant");
    }

    #[test]
    fn aec_eval_informational_marker_follows_the_flag() {
        let file = scenarios();
        assert!(!is_informational(file.get("far-only").unwrap()));
        assert!(is_informational(file.get("softclip4-far-only").unwrap()));
        // Informational scenarios are never in the robustness sweep.
        for name in &file.robustness.scenarios {
            assert!(!is_informational(file.get(name).unwrap()), "{name}");
        }
    }

    #[test]
    fn aec_eval_recovery_is_measurable_within_the_threshold() {
        // An oracle canceller (output = mic minus the echo) must score a recovery inside
        // aec.path_change on every echo-change scenario: the near-end blanking and the 1 s
        // hold of time_to_erle fit inside the scenario's echo-only window
        // (near_silent_s, sim::RECOVERY_WINDOW_S).
        let max = crate::testutil::threshold("aec.path_change");
        for scn in &scenarios().scenario {
            if classify(scn).0 != Class::EchoChanges || is_informational(scn) {
                continue;
            }
            let o = crate::testutil::run(scn);
            let n = o.mic.len() / BLOCK * BLOCK;
            let oracle: Vec<f32> = o.mic[..n].iter().zip(&o.echo).map(|(m, e)| m - e).collect();
            let run = Run {
                out: oracle.clone(),
                linear: oracle,
            };
            let t = score(scn, &o, &run).path_change.unwrap();
            eprintln!("{}: oracle recovery {t:.2} s", scn.name);
            assert!(t <= max, "{}: oracle recovery {t:.2} s > {max}", scn.name);
        }
    }

    #[test]
    fn aec_eval_late_recovery_inside_the_bound_is_measurable() {
        // T1.6 review: a recovery close to the aec.path_change bound needs scored windows
        // up to about 1.5 s past it (window plus hold), beyond the near-silent stretch.
        // An oracle that leaves the echo in for `LATE` seconds after the event and then
        // removes it exactly must score a finite recovery no later than the end of the
        // first fully clean window (`want`): the blanking and the hold fit. It cannot score
        // at or before `LATE` (every such window is the uncancelled mic, 0 dB). It may
        // score earlier than `want` when the far end pauses at the end of the uncancelled
        // stretch (ERLE is a power ratio); where the far end talks across it (path-change)
        // the value is exactly `want`.
        const LATE: f64 = 1.35;
        let max = crate::testutil::threshold("aec.path_change");
        let win = ConvergenceConfig::default().window_s;
        let hop = ConvergenceConfig::default().hop_s;
        // First window start at or after LATE on the hop grid, then its end.
        let want = (LATE / hop).ceil() * hop + win;
        assert!(want <= max, "the case must sit inside the bound");
        for scn in &scenarios().scenario {
            let (class, at) = classify(scn);
            if class != Class::EchoChanges || is_informational(scn) {
                continue;
            }
            let o = crate::testutil::run(scn);
            let n = o.mic.len() / BLOCK * BLOCK;
            let rate = f64::from(o.rate.hz());
            let late = (at * rate) as usize..((at + LATE) * rate) as usize;
            let out: Vec<f32> = (0..n)
                .map(|i| {
                    if late.contains(&i) {
                        o.mic[i]
                    } else {
                        o.mic[i] - o.echo[i]
                    }
                })
                .collect();
            let run = Run {
                out: out.clone(),
                linear: out,
            };
            let t = score(scn, &o, &run).path_change.unwrap();
            eprintln!(
                "{}: late oracle recovery {t:.2} s (want {want:.2})",
                scn.name
            );
            assert!(
                t > LATE && t <= want + 1e-6,
                "{}: late oracle recovery {t:.3} s, want ({LATE}, {want:.3}]",
                scn.name
            );
            if scn.path_change_s.is_some() {
                assert!((t - want).abs() < 1e-6, "{}: {t:.3} s", scn.name);
            }
        }
    }

    #[test]
    fn aec_eval_doubletalk_gates_at_ser0_only() {
        let file = scenarios();
        let o = run_scenario("double-talk-ser-10");
        let n = o.mic.len() / BLOCK * BLOCK;
        // Muting would fail a gated double-talk score; at SER -10 it is informational.
        let mute = Run {
            out: vec![0.0; n],
            linear: vec![0.0; n],
        };
        let s = score(file.get("double-talk-ser-10").unwrap(), &o, &mute);
        assert_eq!(s.doubletalk_sisdr, None);
        assert_eq!(s.doubletalk_sisdr_informational, Some(f64::NEG_INFINITY));
        let lim = Limits {
            erle_farend: 25.0,
            erle_linear: 12.0,
            convergence: 1.5,
            path_change: 2.0,
            doubletalk_sisdr: 10.0,
            passthrough: 30.0,
        };
        assert!(s.failures(&lim).is_empty());
        let o0 = run_scenario("double-talk-ser-0");
        let s0 = score(file.get("double-talk-ser-0").unwrap(), &o0, &mute);
        assert_eq!(s0.doubletalk_sisdr, Some(f64::NEG_INFINITY));
        assert_eq!(s0.failures(&lim).len(), 1);
    }

    #[test]
    fn aec_eval_doubletalk_against_the_highpassed_near_end_is_informational() {
        // T6.23c step 1: beside the official value, double talk is also
        // scored against the near end through the canceller's own audio high-pass. The
        // official value, its threshold and the failures do not change.
        let file = scenarios();
        let hp = EchoCancellerConfig::default().with_highpass(HighPass::Audio { cutoff_hz: 30.0 });
        for name in ["double-talk-ser-0", "double-talk-ser-10"] {
            let scn = file.get(name).unwrap();
            let o = run_scenario(name);
            let n = o.mic.len() / BLOCK * BLOCK;
            // An output that is exactly the high-passed near end (from sample 0).
            let filtered = HighPassFilter::new(30.0, o.rate.hz()).filtered(&o.near_clean[..n]);
            let run = Run {
                out: filtered.clone(),
                linear: filtered,
            };
            let off = score(scn, &o, &run);
            assert_eq!(off.doubletalk_sisdr_highpassed, None, "{name}: no filter");
            let on = score_for(scn, &o, &run, &hp);
            assert_eq!(
                on.doubletalk_sisdr_highpassed,
                Some(f64::INFINITY),
                "{name}: the filtered near end is perfect against itself"
            );
            // Every official value is the unfiltered scorer's.
            assert_eq!(
                Scores {
                    doubletalk_sisdr_highpassed: None,
                    ..on
                },
                off,
                "{name}"
            );
            let official = on
                .doubletalk_sisdr
                .or(on.doubletalk_sisdr_informational)
                .unwrap();
            assert!(official.is_finite(), "{name}: {official}");
            // Muting fails the official value only; the informational one is never a failure.
            let mute = Run {
                out: vec![0.0; n],
                linear: vec![0.0; n],
            };
            let m = score_for(scn, &o, &mute, &hp);
            assert_eq!(m.doubletalk_sisdr_highpassed, Some(f64::NEG_INFINITY));
            assert_eq!(m.failures(&lim()), score(scn, &o, &mute).failures(&lim()));
        }
        // Non-double-talk classes never carry it.
        let o = run_scenario("far-only");
        let n = o.mic.len() / BLOCK * BLOCK;
        let ident = Run {
            out: o.mic[..n].to_vec(),
            linear: o.mic[..n].to_vec(),
        };
        let s = score_for(file.get("far-only").unwrap(), &o, &ident, &hp);
        assert_eq!(s.doubletalk_sisdr_highpassed, None);
        // The worst case folds it like the official value (lowest).
        let a = Scores {
            doubletalk_sisdr_highpassed: Some(3.0),
            ..Scores::default()
        };
        let b = Scores {
            doubletalk_sisdr_highpassed: Some(1.0),
            ..Scores::default()
        };
        assert_eq!(worst(&[a, b]).doubletalk_sisdr_highpassed, Some(1.0));
    }

    fn lim() -> Limits {
        Limits {
            erle_farend: 25.0,
            erle_linear: 12.0,
            convergence: 1.5,
            path_change: 2.0,
            doubletalk_sisdr: 10.0,
            passthrough: 30.0,
        }
    }

    #[test]
    fn aec_eval_failures_and_worst_case() {
        let lim = Limits {
            erle_farend: 25.0,
            erle_linear: 12.0,
            convergence: 1.5,
            path_change: 2.0,
            doubletalk_sisdr: 10.0,
            passthrough: 30.0,
        };
        let a = Scores {
            erle_farend: Some(30.0),
            convergence: Some(1.0),
            ..Scores::default()
        };
        let b = Scores {
            erle_farend: Some(20.0),
            convergence: Some(f64::INFINITY),
            doubletalk_sisdr: Some(f64::NAN),
            ..Scores::default()
        };
        assert!(a.failures(&lim).is_empty());
        let f = b.failures(&lim);
        assert_eq!(f.len(), 3, "{f:?}");
        let w = worst(&[a, b]);
        assert_eq!(w.erle_farend, Some(20.0));
        assert_eq!(w.convergence, Some(f64::INFINITY));
        assert_eq!(w.path_change, None);
    }

    #[test]
    fn aec_eval_failures_count_infinite_erle_as_a_muted_output() {
        let lim = Limits {
            erle_farend: 25.0,
            erle_linear: 12.0,
            convergence: 1.5,
            path_change: 2.0,
            doubletalk_sisdr: 10.0,
            passthrough: 30.0,
        };
        // +inf ERLE means a silent output (muted), never perfect cancellation.
        for (far, lin) in [(f64::INFINITY, 20.0), (40.0, f64::INFINITY)] {
            let s = Scores {
                erle_farend: Some(far),
                erle_linear: Some(lin),
                ..Scores::default()
            };
            let f = s.failures(&lim);
            assert_eq!(f.len(), 1, "{f:?}");
            assert!(f[0].contains("inf"), "{f:?}");
        }
        // +inf SI-SDR stays the best possible value (an exact passthrough).
        let p = Scores {
            passthrough: Some(f64::INFINITY),
            doubletalk_sisdr: Some(f64::INFINITY),
            ..Scores::default()
        };
        assert!(p.failures(&lim).is_empty());
    }

    #[test]
    fn aec_eval_only_stereo_convergence_at_500_ms_is_informational() {
        let file = scenarios();
        let stereo = file.get("stereo-image-change").unwrap();
        let mono = file.get("far-only").unwrap();
        assert!(stereo.stereo.is_some() && mono.stereo.is_none());
        for (d, p) in [(500.0, -200.0), (500.0, 0.0), (500.0, 200.0)] {
            // The grid point's nominal delay decides, not the drift-shifted start delay.
            assert!(grid_convergence_informational(
                &stereo.with_delay_and_drift(d, p),
                d
            ));
            assert!(!grid_convergence_informational(
                &mono.with_delay_and_drift(d, p),
                d
            ));
        }
        for d in [0.0, 250.0] {
            assert!(!grid_convergence_informational(
                &stereo.with_delay_and_drift(d, 200.0),
                d
            ));
        }
        // Base scenarios (120 ms) always gate.
        assert!(!grid_convergence_informational(stereo, stereo.delay_ms));
    }
}
