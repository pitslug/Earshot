//! Delay and drift estimator tests on the simulator scenarios (PLAN.md T1.3 accept):
//! with echo present, delay within +/-2 ms of the truth and drift within +/-20 ppm; with no
//! echo, `NoLock`.

use super::*;
use crate::sim::SimOutput;
use crate::testutil::{run, run_scenario, scenarios};

const FS: f64 = 16_000.0;
/// +/-2 ms at 16 kHz.
const DELAY_TOL: f64 = 32.0;
const DRIFT_TOL_PPM: f64 = 20.0;
/// Time allowed to (re)acquire after the echo appears or changes.
const SETTLE_S: f64 = 3.0;

struct Trace {
    /// (time s, state, drift) after every 10 ms chunk.
    points: Vec<(f64, DelayState, Option<f64>)>,
    epochs: u64,
}

fn track(out: &SimOutput, hint: Option<f64>) -> Trace {
    let mut est = DelayEstimator::new(DelayConfig::default());
    if let Some(h) = hint {
        est.set_hint(h);
    }
    let mut points = Vec::new();
    for (m, r) in out.mic.chunks(160).zip(out.reference.chunks(160)) {
        est.push(m, r);
        points.push((est.samples() as f64 / FS, est.state(), est.drift_ppm()));
    }
    Trace {
        points,
        epochs: est.epoch(),
    }
}

/// Times at which the echo appears or changes (first far speech reaching the mic, path
/// change, delay jump, echo switched on).
fn events(scn: &crate::sim::Scenario, out: &SimOutput) -> Vec<f64> {
    let first_far = out.far_regions.first().map_or(0.0, |r| r.start as f64 / FS);
    let mut ev = vec![first_far];
    ev.extend(scn.path_change_s);
    ev.extend(scn.delay_jump.map(|j| j.at_s));
    ev.extend(scn.echo_from_s);
    ev
}

fn echo_present_at(out: &SimOutput, t: f64) -> bool {
    let m = (t * FS) as usize;
    out.echo_active.iter().any(|r| r.contains(&m))
}

fn check_echo_scenario(name: &str) {
    let scn = scenarios().get(name).unwrap();
    check_echo(scn, &run_scenario(name));
}

fn check_echo(scn: &crate::sim::Scenario, out: &SimOutput) {
    let name = scn.name.as_str();
    let trace = track(out, None);
    let ev = events(scn, out);
    let mut worst: f64 = 0.0;
    let mut locked_checks = 0usize;
    for &(t, state, _) in &trace.points {
        if !echo_present_at(out, t) {
            continue;
        }
        let last_event = ev
            .iter()
            .copied()
            .filter(|&e| e <= t)
            .fold(f64::MIN, f64::max);
        if t - last_event < SETTLE_S {
            continue;
        }
        // Echo present and settled: must be locked on the true delay.
        let DelayState::Locked { delay, .. } = state else {
            panic!("{name}: not locked at {t:.2} s ({state:?})");
        };
        let truth = out.delay.delay_at((t * FS) as usize);
        worst = worst.max((delay - truth).abs());
        locked_checks += 1;
        assert!(
            (delay - truth).abs() <= DELAY_TOL,
            "{name} at {t:.2} s: delay {delay:.1} vs truth {truth:.1} samples"
        );
    }
    assert!(locked_checks > 500, "{name}: too few settled points");
    // Drift at the last moment the echo was present (the lock ends with the echo).
    let (_, _, drift) = trace
        .points
        .iter()
        .rev()
        .find(|p| echo_present_at(out, p.0))
        .unwrap();
    let drift = drift.unwrap_or_else(|| panic!("{name}: no drift estimate"));
    eprintln!(
        "{name}: worst delay error {:.2} ms, drift {drift:+.1} ppm (truth {:+.0}), epochs {}",
        worst / 16.0,
        scn.drift_ppm,
        trace.epochs
    );
    assert!(
        (drift - scn.drift_ppm).abs() <= DRIFT_TOL_PPM,
        "{name}: drift {drift:.1} ppm vs {} ppm",
        scn.drift_ppm
    );
}

#[test]
fn aec_delay_far_only() {
    check_echo_scenario("far-only");
}

#[test]
fn aec_delay_double_talk() {
    check_echo_scenario("double-talk-ser-0");
    check_echo_scenario("double-talk-ser-10");
}

#[test]
fn aec_delay_long_delay_and_noise() {
    check_echo_scenario("delay-450ms");
    check_echo_scenario("noisy-office");
}

#[test]
fn aec_delay_drift_both_signs() {
    check_echo_scenario("drift-plus-200ppm");
    check_echo_scenario("drift-minus-200ppm");
}

#[test]
fn aec_delay_tracks_path_change_and_delay_jump() {
    check_echo_scenario("path-change");
    check_echo_scenario("delay-jump");
}

#[test]
fn aec_delay_acquires_echo_that_appears_mid_call() {
    check_echo_scenario("switch-headphones-to-speakers");
    let out = run_scenario("switch-headphones-to-speakers");
    let trace = track(&out, None);
    for &(t, state, _) in &trace.points {
        if t < 15.0 {
            assert_eq!(
                state,
                DelayState::NoLock,
                "locked before the echo at {t:.2} s"
            );
        }
    }
}

#[test]
fn aec_delay_reports_no_lock_without_echo() {
    for name in ["near-only", "headphones"] {
        let out = run_scenario(name);
        let trace = track(&out, None);
        for &(t, state, _) in &trace.points {
            assert_eq!(state, DelayState::NoLock, "{name}: {state:?} at {t:.2} s");
        }
        assert_eq!(trace.epochs, 0, "{name}");
    }
}

#[test]
fn aec_delay_loses_lock_when_echo_stops() {
    let name = "switch-speakers-to-headphones";
    check_echo_scenario(name);
    let out = run_scenario(name);
    let trace = track(&out, None);
    for &(t, state, _) in &trace.points {
        if t >= 15.0 + SETTLE_S {
            assert_eq!(state, DelayState::NoLock, "{name}: {state:?} at {t:.2} s");
        }
    }
}

#[test]
fn aec_delay_warm_start_hint_is_used_then_overridden() {
    let out = run_scenario("far-only");
    let truth = out.delay.delay_at(0);
    // A wrong hint is reported until live tracking locks, then replaced.
    let trace = track(&out, Some(3_000.0));
    assert_eq!(trace.points[0].1, DelayState::Hint { delay: 3_000.0 });
    let locked_at = trace
        .points
        .iter()
        .find(|p| p.1.is_locked())
        .expect("locks")
        .0;
    assert!(locked_at < SETTLE_S, "locked at {locked_at:.2} s");
    let end = trace.points.last().unwrap().1.delay().unwrap();
    assert!((end - truth).abs() <= DELAY_TOL, "hint must not win: {end}");

    // Without far-end speech the hint stays in force (NoLock would mean no delay at all).
    let silent = run_scenario("near-only");
    let hinted = track(&silent, Some(1_234.0));
    assert!(
        hinted
            .points
            .iter()
            .all(|p| p.1 == DelayState::Hint { delay: 1_234.0 })
    );
}

#[test]
fn aec_delay_hint_seeds_the_search_and_is_discarded_on_release() {
    // A correct hint locks no later than no hint.
    let out = run_scenario("far-only");
    let first_lock = |t: &Trace| t.points.iter().find(|p| p.1.is_locked()).map(|p| p.0);
    let plain = first_lock(&track(&out, None)).unwrap();
    let hinted = first_lock(&track(&out, Some(out.delay.delay_at(0)))).unwrap();
    eprintln!("lock at {plain:.3} s without hint, {hinted:.3} s with a correct hint");
    assert!(hinted <= plain, "hint slowed lock: {hinted} > {plain}");

    // When the echo stops, the released lock goes to NoLock, not back to the hint.
    let out = run_scenario("switch-speakers-to-headphones");
    let t = track(&out, Some(out.delay.delay_at(0)));
    let last = t.points.last().unwrap().1;
    assert_eq!(last, DelayState::NoLock, "hint survived the release");
    assert!(
        t.points
            .iter()
            .filter(|p| p.0 > 15.0 + SETTLE_S)
            .all(|p| p.1 == DelayState::NoLock)
    );
}

#[test]
fn aec_delay_state_accessors() {
    assert_eq!(DelayState::NoLock.delay(), None);
    assert_eq!(DelayState::Hint { delay: 5.0 }.delay(), Some(5.0));
    assert!(!DelayState::Hint { delay: 5.0 }.is_locked());
    let l = DelayState::Locked {
        delay: 7.0,
        confidence: 20.0,
    };
    assert_eq!(l.delay(), Some(7.0));
    assert!(l.is_locked());
}

#[test]
fn aec_delay_robustness_grid() {
    // PLAN.md 4.0 robustness: bulk delay 0-500 ms and drift +/-200 ppm.
    let file = scenarios();
    for name in ["far-only", "double-talk-ser-10"] {
        let base = file.get(name).unwrap();
        for &d in &file.robustness.delays_ms {
            for &p in &file.robustness.drifts_ppm {
                let scn = base.with_delay_and_drift(d, p);
                check_echo(&scn, &run(&scn));
            }
        }
    }
}

#[test]
fn aec_delay_ignores_non_finite_samples() {
    // An infinite mic sample must not become the recent mic peak (which would gate every
    // later hop), and NaN must not reach the correlation: the estimator still locks on
    // delay-jump and follows its jump. The estimator is fed directly (the canceller
    // sanitises its input before it gets here).
    let scn = scenarios().get("delay-jump").unwrap();
    let out = run_scenario("delay-jump");
    let at = 5 * 16_000;
    // Infinity alone in the mic (its frame power is +inf, not NaN), then the mixed set in
    // both signals a second later.
    let mut mic = out.mic.clone();
    mic[at] = f32::INFINITY;
    let bad = crate::testutil::with_non_finite(&[&mic, &out.reference], at + 16_000);
    let poisoned = SimOutput {
        mic: bad[0].clone(),
        reference: bad[1].clone(),
        ..out.clone()
    };
    check_echo(scn, &poisoned);
}

/// Noise with speech-like gaps: 0.6 s on, 0.4 s off, seeded.
fn bursty_noise(n: usize, seed: u64) -> Vec<f32> {
    let mut s = seed;
    (0..n)
        .map(|i| {
            s = s
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            let x = ((s >> 33) as f32 / (1u64 << 31) as f32) - 0.5;
            if (i / 1_600) % 10 < 6 { x * 0.5 } else { 0.0 }
        })
        .collect()
}

fn locks_seen(delay: usize) -> Vec<f64> {
    let reference = bursty_noise(16_000 * 20, 77);
    let mut mic = vec![0.0f32; reference.len()];
    for (i, m) in mic.iter_mut().enumerate().skip(delay) {
        *m = 0.3 * reference[i - delay];
    }
    let mut est = DelayEstimator::new(DelayConfig::default());
    let mut seen = Vec::new();
    for (m, r) in mic.chunks(160).zip(reference.chunks(160)) {
        est.push(m, r);
        if let DelayState::Locked { delay, .. } = est.state() {
            seen.push(delay);
        }
    }
    seen
}

#[test]
fn aec_delay_never_locks_at_the_edge_of_the_search_range() {
    // T6.17: a live session reported a lock at exactly 600 ms (`max_delay`) while the far
    // end was paused. A peak at the edge of the searched lags means the true delay may lie
    // outside the window, so it is never a lock.
    let max = DelayConfig::default().max_delay;
    for delay in [max, max + 200] {
        let seen = locks_seen(delay);
        assert!(
            seen.is_empty(),
            "locked at {:?} for a {delay}-sample echo",
            seen.first()
        );
    }
    // Inside the range a long delay still locks, accurately.
    let inside = max - 600;
    let seen = locks_seen(inside);
    assert!(!seen.is_empty(), "no lock for a {inside}-sample echo");
    assert!(
        seen.iter().all(|d| (d - inside as f64).abs() <= DELAY_TOL),
        "{seen:?}"
    );
}
