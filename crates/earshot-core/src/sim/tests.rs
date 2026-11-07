//! Simulator tests. All names contain `sim` (module path `sim::tests`).

use rand_chacha::ChaCha8Rng;
use rand_core::SeedableRng;
use rustfft::FftPlanner;
use rustfft::num_complex::Complex;

use super::*;
use crate::dsp::db;
use crate::testutil::{run, run_scenario, scenarios, smoke_speech};

const REQUIRED: &[&str] = &[
    "far-only",
    "near-only",
    "double-talk-ser-0",
    "double-talk-ser-10",
    "path-change",
    "delay-450ms",
    "drift-plus-200ppm",
    "drift-minus-200ppm",
    "noisy-office",
    "headphones",
    "switch-speakers-to-headphones",
    "switch-headphones-to-speakers",
    "delay-jump",
];

/// GCC-PHAT lag (samples, parabolic sub-sample) of `sig` relative to `reference` over
/// `sig[start..start + len]`, searching lags `0..max_lag`.
fn gcc_phat_lag(reference: &[f32], sig: &[f32], start: usize, len: usize, max_lag: usize) -> f64 {
    let n = (len + max_lag).next_power_of_two() * 2;
    let mut planner = FftPlanner::<f64>::new();
    let fwd = planner.plan_fft_forward(n);
    let inv = planner.plan_fft_inverse(n);
    let mut a = vec![Complex::new(0.0, 0.0); n];
    let mut b = vec![Complex::new(0.0, 0.0); n];
    for i in 0..len {
        a[i] = Complex::new(f64::from(sig[start + i]), 0.0);
    }
    // Reference window covering every lag.
    let r0 = start.saturating_sub(max_lag);
    for i in r0..start + len {
        b[i - r0] = Complex::new(f64::from(reference[i]), 0.0);
    }
    fwd.process(&mut a);
    fwd.process(&mut b);
    for (x, y) in a.iter_mut().zip(&b) {
        let c = *x * y.conj();
        let m = c.norm();
        *x = if m > 1e-20 {
            c / m
        } else {
            Complex::new(0.0, 0.0)
        };
    }
    inv.process(&mut a);
    // IFFT(A conj(B))[k] = sum_i sig_win[i + k] ref_win[i] (circular). With
    // sig[m] ~ ref[m - lag]: start + i + k - lag = r0 + i, so k = lag - (start - r0).
    let corr = |lag: usize| -> f64 {
        let k = lag as isize - (start - r0) as isize;
        a[k.rem_euclid(n as isize) as usize].re
    };
    let max_lag = max_lag.min(start - r0);
    let best = (0..max_lag)
        .max_by(|&x, &y| corr(x).total_cmp(&corr(y)))
        .unwrap();
    if best == 0 || best + 1 >= max_lag {
        return best as f64;
    }
    let (ym, y0, yp) = (corr(best - 1), corr(best), corr(best + 1));
    let denom = ym - 2.0 * y0 + yp;
    best as f64
        + if denom.abs() > 1e-12 {
            0.5 * (ym - yp) / denom
        } else {
            0.0
        }
}

fn assert_aligned(out: &SimOutput, at_s: f64, tol: f64) {
    let start = (at_s * 16_000.0) as usize;
    let len = 16_000;
    let lag = gcc_phat_lag(&out.reference, &out.echo, start, len, 12_000);
    let truth = out.delay.delay_at(start + len / 2);
    assert!(
        (lag - truth).abs() <= tol,
        "at {at_s} s: measured lag {lag:.2}, truth {truth:.2} samples"
    );
}

#[test]
fn sim_scenario_file_has_every_required_scenario() {
    let file = scenarios();
    for name in REQUIRED {
        assert!(file.get(name).is_some(), "missing scenario {name}");
    }
    let mut names: Vec<&str> = file.scenario.iter().map(|s| s.name.as_str()).collect();
    names.sort_unstable();
    names.dedup();
    assert_eq!(names.len(), file.scenario.len(), "duplicate scenario names");
    let far = file.get("far-only").unwrap();
    assert!(
        far.noise_snr_db.is_none() && !far.near_speech,
        "far-only is noise-free"
    );
    let rob = &file.robustness;
    let max = rob.delays_ms.iter().copied().fold(f64::MIN, f64::max);
    let min = rob.delays_ms.iter().copied().fold(f64::MAX, f64::min);
    assert!(min <= 0.0 && max >= 500.0, "robustness covers 0-500 ms");
    assert!(rob.drifts_ppm.contains(&200.0) && rob.drifts_ppm.contains(&-200.0));
    for s in &rob.scenarios {
        assert!(
            file.get(s).is_some(),
            "robustness names unknown scenario {s}"
        );
    }
}

/// Gating scenarios with a loudspeaker soft clip and their informational copies at the
/// pre-T1.2a `softclip = 4`. T1.2a measured GATE-10 as at least as severe as softclip 3,
/// which the gating scenarios now use (see the header of `acceptance/aec_scenarios.toml`).
const SOFTCLIP4: &[(&str, &str)] = &[
    ("far-only", "softclip4-far-only"),
    ("double-talk-ser-0", "softclip4-double-talk-ser-0"),
    ("double-talk-ser-10", "softclip4-double-talk-ser-10"),
    ("noisy-office", "softclip4-noisy-office"),
];

#[test]
fn sim_informational_scenarios_copy_their_gating_scenario_at_softclip_4() {
    let file = scenarios();
    let gating_clip = file.get("far-only").unwrap().softclip;
    // Pinned to the T1.2a measurement of the GATE-10 recording (decided by the owner). Changing it needs a new `cargo xtask echo-nl` measurement, logged, and
    // this assertion edited in the same commit.
    assert_eq!(
        gating_clip,
        Some(3.0),
        "gating soft clip is anchored to the T1.2a measurement"
    );
    for (gate, copy) in SOFTCLIP4 {
        let g = file.get(gate).unwrap();
        let s = file
            .get(copy)
            .unwrap_or_else(|| panic!("missing scenario {copy}"));
        assert!(!g.informational, "{gate} gates");
        assert!(s.informational, "{copy} is informational");
        assert_eq!(
            g.softclip, gating_clip,
            "{gate} shares the measured soft clip"
        );
        assert_eq!(s.softclip, Some(4.0), "{copy}");
        // Identical to the gating scenario apart from name, description, flag and clip.
        let same = Scenario {
            name: g.name.clone(),
            description: g.description.clone(),
            informational: false,
            softclip: g.softclip,
            ..s.clone()
        };
        assert_eq!(&same, g, "{copy}");
    }
    for s in &file.scenario {
        assert_eq!(
            s.informational,
            s.name.starts_with("softclip4-"),
            "{}: only the softclip4-* copies are informational",
            s.name
        );
    }
    for name in REQUIRED {
        assert!(!file.get(name).unwrap().informational, "{name} gates");
    }
    for s in &file.robustness.scenarios {
        assert!(!file.get(s).unwrap().informational, "robustness runs {s}");
    }
    let o = run_scenario("softclip4-far-only");
    assert!(db(power(&o.echo)) > -45.0, "echo present");
}

#[test]
fn sim_heldout_runs_are_every_gating_scenario_at_each_offset() {
    let file = scenarios();
    let h = file.heldout.as_ref().expect("[heldout] block");
    assert_eq!(h.seed_offsets, [10_000, 10_001, 10_002, 10_003, 10_004]);
    let runs = file.heldout_runs().unwrap();
    let gating: Vec<&Scenario> = file.scenario.iter().filter(|s| !s.informational).collect();
    assert_eq!(runs.len(), gating.len() * h.seed_offsets.len());
    for (scn, chunk) in gating.iter().zip(runs.chunks(h.seed_offsets.len())) {
        for (run, &offset) in chunk.iter().zip(&h.seed_offsets) {
            assert_eq!(run.base, scn.name);
            assert_eq!(run.offset, offset);
            assert_eq!(run.scenario.seed, scn.seed + offset);
            assert_eq!(run.scenario.name, format!("{}@heldout+{offset}", scn.name));
            // Everything but the seed and name is the gating scenario.
            let back = Scenario {
                name: scn.name.clone(),
                seed: scn.seed,
                ..run.scenario.clone()
            };
            assert_eq!(&back, *scn);
        }
    }
    // Informational scenarios are never held out (selected by the field, not the name).
    assert!(runs.iter().all(|r| !r.scenario.informational));
    assert!(file.scenario.iter().any(|s| s.informational));
}

#[test]
fn sim_heldout_seeds_are_disjoint_from_dev_seeds() {
    let file = scenarios();
    let dev = file.dev_seeds();
    // Base seeds, the second-seed sweep and the dev-wide sweep are dev data.
    for s in &file.scenario {
        assert!(dev.contains(&s.seed) && dev.contains(&(s.seed + SECOND_SEED_OFFSET)));
        assert!(dev.contains(&(s.seed + DEV_WIDE_SEED_OFFSET)));
    }
    let held: std::collections::BTreeSet<u64> = file
        .heldout_runs()
        .unwrap()
        .iter()
        .map(|r| r.scenario.seed)
        .collect();
    assert!(held.is_disjoint(&dev), "{:?}", held.intersection(&dev));
    // An offset that lands on a dev seed is refused, as are a missing block, no offsets
    // and duplicates.
    let with = |offsets: Vec<u64>| ScenarioFile {
        heldout: Some(HeldOut {
            seed_offsets: offsets,
        }),
        ..file.clone()
    };
    for offsets in [
        vec![0],
        vec![SECOND_SEED_OFFSET],
        vec![DEV_WIDE_SEED_OFFSET],
        vec![10_000, 1],
        vec![],
        vec![7, 7],
    ] {
        assert!(with(offsets.clone()).heldout_runs().is_err(), "{offsets:?}");
    }
    let none = ScenarioFile {
        heldout: None,
        ..file.clone()
    };
    assert!(none.heldout_runs().is_err());
    let overflow = with(vec![u64::MAX]);
    assert!(overflow.heldout_runs().is_err());
}

#[test]
fn sim_every_echo_path_event_has_an_echo_only_recovery_window() {
    let file = scenarios();
    let mut checked = 0;
    for scn in &file.scenario {
        if scn.events().is_empty() {
            continue;
        }
        checked += 1;
        let out = run_scenario(&scn.name);
        let bad = recovery_window_violations(scn, &out);
        assert!(bad.is_empty(), "{bad:?}");
        // Where an event brings or keeps the echo, it is there in the window.
        for t in scn.events() {
            let w = (t * 16_000.0) as usize..((t + RECOVERY_WINDOW_S) * 16_000.0) as usize;
            if scn.echo_until_s != Some(t) {
                assert!(
                    db(power(&out.echo[w])) > -50.0,
                    "{}: echo at {t} s",
                    scn.name
                );
            }
        }
    }
    // switch x2, path-change, delay-jump.
    assert!(checked >= 4, "only {checked} event scenarios");
    // Negative control: without its near-silent interval the near end talks across the
    // switch and the rule must catch it.
    let mut scn = file.get("switch-headphones-to-speakers").unwrap().clone();
    scn.near_silent_s.clear();
    let (far, near) = smoke_speech();
    let mut rng = ChaCha8Rng::seed_from_u64(scn.seed);
    let out = simulate(&scn, &file.setup, &SpeechPool { far, near }, &mut rng).unwrap();
    assert!(
        recovery_window_violations(&scn, &out)
            .iter()
            .any(|m| m.contains("near end")),
        "rule must reject near-end talk across the switch"
    );
}

#[test]
fn sim_near_silent_intervals_are_respected_and_validated() {
    let file = scenarios();
    let scn = file.get("switch-headphones-to-speakers").unwrap();
    let out = run_scenario(&scn.name);
    for r in &out.near_regions {
        assert!(
            r.end <= 14 * 16_000 + 8_000 || r.start >= 17 * 16_000 + 8_000,
            "{r:?}"
        );
    }
    let (far, near) = smoke_speech();
    let pool = SpeechPool { far, near };
    let mut rng = ChaCha8Rng::seed_from_u64(1);
    for bad in [[17.5, 14.5], [-1.0, 2.0], [29.0, 31.0]] {
        let s = Scenario {
            near_silent_s: vec![bad],
            ..scn.clone()
        };
        assert!(matches!(
            simulate(&s, &file.setup, &pool, &mut rng),
            Err(SimError::InvalidScenario(_))
        ));
    }
}

#[test]
fn sim_placed_utterances_are_the_speech_regions_with_their_clips() {
    let (far, near) = smoke_speech();
    for name in ["double-talk-ser-0", "far-only", "near-only"] {
        let out = run_scenario(name);
        let spans = |u: &[PlacedUtterance]| u.iter().map(|p| p.span.clone()).collect::<Vec<_>>();
        assert_eq!(spans(&out.far_utterances), out.far_regions, "{name}");
        assert_eq!(spans(&out.near_utterances), out.near_regions, "{name}");
        for (u, pool) in out
            .far_utterances
            .iter()
            .map(|u| (u, far))
            .chain(out.near_utterances.iter().map(|u| (u, near)))
        {
            let clip = &pool[u.clip];
            assert_eq!(u.complete, u.span.len() == clip.len(), "{name}: {u:?}");
            assert!(
                u.span.len() <= clip.len() && u.channel == 0,
                "{name}: {u:?}"
            );
        }
        assert!(out.far_utterances.iter().rev().skip(1).all(|u| u.complete));
    }
    // A stereo far end adds the right channel's own utterances, marked channel 1.
    let base = scenarios().get("far-only").unwrap();
    let stereo = run(&crate::testutil::stereo_variant(base, 0.5, None));
    assert!(stereo.far_utterances.iter().any(|u| u.channel == 1));
    assert!(stereo.far_utterances.iter().any(|u| u.channel == 0));
}

#[test]
fn sim_is_deterministic_per_seed() {
    let a = run_scenario("noisy-office");
    let b = run_scenario("noisy-office");
    assert_eq!(a, b);
    let file = scenarios();
    let mut scn = file.get("noisy-office").unwrap().clone();
    scn.seed += 1;
    let (far, near) = smoke_speech();
    let mut rng = ChaCha8Rng::seed_from_u64(scn.seed);
    let c = simulate(&scn, &file.setup, &SpeechPool { far, near }, &mut rng).unwrap();
    assert_ne!(a.mic, c.mic);
}

#[test]
fn sim_mic_is_the_sum_of_its_components_and_lengths_match() {
    for name in REQUIRED {
        let o = run_scenario(name);
        let n = o.mic.len();
        assert_eq!(n, 30 * 16_000, "{name}");
        for s in [&o.reference, &o.near_clean, &o.echo, &o.noise] {
            assert_eq!(s.len(), n, "{name}");
        }
        for i in 0..n {
            assert_eq!(
                o.mic[i],
                o.echo[i] + o.near_clean[i] + o.noise[i],
                "{name} @ {i}"
            );
        }
    }
}

#[test]
fn sim_levels_match_configuration() {
    let far = run_scenario("far-only");
    let ref_db = 20.0 * active_rms(&far.reference, &far.far_regions).log10();
    assert!((ref_db + 20.0).abs() < 0.1, "reference {ref_db:.2} dBFS");
    assert!(far.near_clean.iter().all(|&v| v == 0.0));
    assert!(far.noise.iter().all(|&v| v == 0.0));
    assert!(db(power(&far.echo)) > -45.0, "echo present");

    for (name, ser) in [("double-talk-ser-0", 0.0), ("double-talk-ser-10", -10.0)] {
        let o = run_scenario(name);
        let base = o.delay.segments[0].base.round() as usize;
        let arrived: Vec<_> = o
            .far_regions
            .iter()
            .map(|r| (r.start + base)..(r.end + base).min(o.mic.len()))
            .collect();
        let e = active_rms(&o.echo, &arrived);
        let s = active_rms(&o.near_clean, &o.near_regions);
        let measured = 20.0 * (s / e).log10();
        assert!((measured - ser).abs() < 0.1, "{name}: SER {measured:.2} dB");
        assert!((20.0 * e.log10() + 30.0).abs() < 0.1, "{name}: echo level");
        assert!(
            o.near_regions.first().unwrap().start >= 5 * 16_000,
            "near end starts at 5 s"
        );
    }

    let o = run_scenario("noisy-office");
    let s = active_rms(&o.near_clean, &o.near_regions);
    let snr = 20.0 * (s / power(&o.noise).sqrt()).log10();
    assert!((snr - 15.0).abs() < 0.1, "SNR {snr:.2} dB");
}

#[test]
fn sim_echo_is_aligned_with_the_delay_truth() {
    let o = run_scenario("far-only");
    for t in [3.0, 12.0, 25.0] {
        assert_aligned(&o, t, 1.0);
    }
    let o = run_scenario("delay-450ms");
    assert!(o.delay.segments[0].base > 7_200.0 && o.delay.segments[0].base < 7_250.0);
    for t in [3.0, 20.0] {
        assert_aligned(&o, t, 1.0);
    }
}

#[test]
fn sim_drift_moves_the_delay_by_the_configured_rate() {
    for (name, ppm) in [("drift-plus-200ppm", 200.0), ("drift-minus-200ppm", -200.0)] {
        let o = run_scenario(name);
        // Render fast (+ppm): the echo carries later reference samples, delay shrinks.
        let d0 = o.delay.delay_at(16_000);
        let d1 = o.delay.delay_at(28 * 16_000);
        let expected = -ppm * 1e-6 * 27.0 * 16_000.0;
        assert!(((d1 - d0) - expected).abs() < 1e-6, "{name}");
        // The delay moves 3.2 samples within each 1 s analysis window, which smears the
        // GCC-PHAT peak; measured error stays under 2.5 samples (0.16 ms).
        for t in [1.0, 14.0, 27.0] {
            assert_aligned(&o, t, 3.0);
        }
    }
}

#[test]
fn sim_delay_jump_and_path_change_move_the_peak() {
    let o = run_scenario("delay-jump");
    assert_eq!(o.delay.segments.len(), 2);
    assert_eq!(o.delay.segments[1].start, 15 * 16_000);
    let jump = o.delay.segments[1].base - o.delay.segments[0].base;
    assert!((jump - 360.0 * 16.0).abs() < 1e-9, "jump {jump}");
    assert_aligned(&o, 5.0, 1.0);
    assert_aligned(&o, 22.0, 1.0);

    let o = run_scenario("path-change");
    assert_eq!(o.delay.segments.len(), 2);
    assert_ne!(o.delay.segments[0].base, o.delay.segments[1].base);
    assert_aligned(&o, 5.0, 1.0);
    assert_aligned(&o, 22.0, 1.0);
}

#[test]
fn sim_headphone_scenarios_switch_the_echo() {
    let o = run_scenario("headphones");
    assert!(o.echo.iter().all(|&v| v == 0.0));
    assert!(o.echo_active.is_empty());
    assert!(db(power(&o.reference)) > -30.0, "far end still talks");

    let cut = 15 * 16_000;
    let o = run_scenario("switch-speakers-to-headphones");
    assert!(o.echo[cut..].iter().all(|&v| v == 0.0));
    assert!(db(power(&o.echo[..cut])) > -45.0);
    assert_eq!(o.echo_active, vec![0..cut]);

    let o = run_scenario("switch-headphones-to-speakers");
    assert!(o.echo[..cut].iter().all(|&v| v == 0.0));
    assert!(db(power(&o.echo[cut..])) > -45.0);
    assert_eq!(o.echo_active, vec![cut..o.mic.len()]);

    let o = run_scenario("near-only");
    assert!(o.reference.iter().all(|&v| v == 0.0));
    assert!(o.echo.iter().all(|&v| v == 0.0));
}

#[test]
fn sim_speech_labels_cover_the_speech() {
    // Outside the labelled regions (allowing the room tail) the near-end is silent.
    let o = run_scenario("near-only");
    let tail = 4_000; // rir_ms = 250
    let mut inside = vec![false; o.mic.len()];
    for r in &o.near_regions {
        for v in &mut inside[r.start..(r.end + tail).min(o.mic.len())] {
            *v = true;
        }
    }
    let (mut pin, mut pout, mut nin, mut nout) = (0.0, 0.0, 0usize, 0usize);
    for (i, &v) in o.near_clean.iter().enumerate() {
        let p = f64::from(v) * f64::from(v);
        if inside[i] {
            pin += p;
            nin += 1;
        } else {
            pout += p;
            nout += 1;
        }
    }
    assert!(nout > 16_000, "there are labelled silences");
    let ratio = db((pout / nout as f64) / (pin / nin as f64));
    assert!(ratio < -60.0, "energy outside labels {ratio:.1} dB");
    // Regions are disjoint, sorted and separated by at least 0.2 s.
    for w in o.near_regions.windows(2) {
        assert!(w[1].start >= w[0].end + 3_200);
    }
}

#[test]
fn sim_rir_has_direct_path_and_plausible_decay() {
    let file = scenarios();
    let s = &file.setup;
    let rate = PIPELINE_RATE;
    let h = image_source_rir(&s.room, s.speaker, s.mic, rate, 8_000);
    let d = direct_delay_samples(s.speaker, s.mic, rate);
    let peak = h
        .iter()
        .enumerate()
        .max_by(|a, b| a.1.abs().total_cmp(&b.1.abs()))
        .unwrap()
        .0;
    assert!(
        (peak as f64 - d).abs() <= 1.0,
        "peak {peak} vs direct {d:.2}"
    );
    let dist = distance(s.speaker, s.mic);
    // A fractional delay spreads the impulse over neighbouring taps but keeps its energy
    // (sum of sinc^2 = 1), so compare the energy around the peak with (1 / 4 pi r)^2. The
    // first reflection (floor) arrives more than 1 m later, outside this window.
    let e_direct: f64 = h[peak - 8..=peak + 8]
        .iter()
        .map(|&v| f64::from(v).powi(2))
        .sum();
    let expected = (1.0 / (4.0 * std::f64::consts::PI * dist)).powi(2);
    assert!(
        (db(e_direct / expected)).abs() < 1.0,
        "direct-path energy {:.2} dB from 1/(4 pi r)",
        db(e_direct / expected)
    );
    // Schroeder backward integration; T20 extrapolated to 60 dB.
    let mut energy: Vec<f64> = h.iter().map(|&v| f64::from(v).powi(2)).collect();
    for i in (0..energy.len() - 1).rev() {
        energy[i] += energy[i + 1];
    }
    let total = energy[0];
    let t_at = |level_db: f64| {
        energy
            .iter()
            .position(|&e| db(e / total) <= level_db)
            .unwrap() as f64
            / 16_000.0
    };
    let rt60 = 3.0 * (t_at(-25.0) - t_at(-5.0));
    let sabine = s.room.sabine_rt60();
    assert!(
        rt60 > 0.5 * sabine && rt60 < 1.5 * sabine,
        "RT60 {rt60:.3} s vs Sabine {sabine:.3} s"
    );
}

#[test]
fn sim_rejects_bad_scenarios() {
    let file = scenarios();
    let (far, near) = smoke_speech();
    let pool = SpeechPool { far, near };
    let mut rng = ChaCha8Rng::seed_from_u64(1);
    let base = file.get("far-only").unwrap().clone();
    for bad in [
        Scenario {
            duration_s: 0.0,
            ..base.clone()
        },
        Scenario {
            delay_ms: -1.0,
            ..base.clone()
        },
        Scenario {
            path_change_s: Some(31.0),
            ..base.clone()
        },
        Scenario {
            drift_ppm: 1e5,
            ..base.clone()
        },
        // Fast render clock from 0 ms: the echo would precede its reference.
        Scenario {
            delay_ms: 0.0,
            drift_ppm: 200.0,
            ..base.clone()
        },
    ] {
        assert!(matches!(
            simulate(&bad, &file.setup, &pool, &mut rng),
            Err(SimError::InvalidScenario(_))
        ));
    }
    let empty = SpeechPool {
        far: &[],
        near: &[],
    };
    assert_eq!(
        simulate(&base, &file.setup, &empty, &mut rng),
        Err(SimError::EmptyPool("far-end"))
    );
}

#[test]
fn sim_robustness_variants_keep_the_requested_minimum_delay() {
    let file = scenarios();
    let base = file.get("far-only").unwrap();
    for &d in &file.robustness.delays_ms {
        for &p in &file.robustness.drifts_ppm {
            let scn = base.with_delay_and_drift(d, p);
            let out = crate::testutil::run(&scn);
            let direct = out.delay.segments[0].base - (scn.delay_ms * 16.0).round();
            let n = out.mic.len();
            let bulk_min = (out.delay.delay_at(0).min(out.delay.delay_at(n - 1)) - direct) / 16.0;
            let bulk_max = (out.delay.delay_at(0).max(out.delay.delay_at(n - 1)) - direct) / 16.0;
            // Minimum bulk delay is the requested one and the span is the drift over the call,
            // to within the 1/16 ms grid plus eps * base (the truth delay at sample 0 is
            // base (1 + eps): 0.1 ms at 500 ms and 200 ppm).
            assert!(
                (bulk_min - d).abs() < 0.15,
                "{}: min {bulk_min:.3} ms",
                scn.name
            );
            assert!(
                (bulk_max - bulk_min - p.abs() * 1e-6 * 30_000.0).abs() < 0.15,
                "{}: span {:.3} ms",
                scn.name,
                bulk_max - bulk_min
            );
        }
    }
}

#[test]
fn sim_stereo_scenario_renders_two_loudspeakers() {
    let file = crate::testutil::scenarios();
    let base = file.get("far-only-linear").unwrap();
    assert!(
        crate::testutil::run_scenario("far-only-linear")
            .reference_lr
            .is_none()
    );
    for c in [0.0, 0.8] {
        let o = crate::testutil::run(&crate::testutil::stereo_variant(base, c, None));
        let [l, r] = o.reference_lr.as_ref().expect("stereo reference");
        // The mono reference is the downmix.
        for ((m, a), b) in o.reference.iter().zip(l).zip(r) {
            assert!((m - 0.5 * (a + b)).abs() < 1e-6);
        }
        // The right channel carries the left one 1 ms (16 samples) later at weight c.
        let lag = 16;
        let dot = |x: &[f32], y: &[f32]| -> f64 {
            x.iter()
                .zip(y)
                .map(|(a, b)| f64::from(*a) * f64::from(*b))
                .sum()
        };
        let corr = dot(&r[lag..], &l[..l.len() - lag])
            / (dot(&r[lag..], &r[lag..]) * dot(&l[..l.len() - lag], &l[..l.len() - lag])).sqrt();
        assert!(
            (corr - c).abs() < 0.15,
            "c {c}: channel correlation {corr:.2}"
        );
        // With independent channels the right loudspeaker's echo is in the mic: the echo
        // correlates with the right channel at its path delay (direct path at
        // base + ~42 samples).
        if c == 0.0 {
            let base = o.delay.segments[0].base.round() as usize;
            let span = 5 * 16_000..10 * 16_000;
            let best = (base..base + 120)
                .map(|d| {
                    let e = &o.echo[span.start + d..span.end + d];
                    let x = &r[span.clone()];
                    dot(e, x) / (dot(e, e) * dot(x, x)).sqrt()
                })
                .fold(0.0f64, f64::max);
            assert!(
                best > 0.1,
                "right loudspeaker missing from the echo ({best:.3})"
            );
        }
    }
}

#[test]
fn sim_stereo_rejects_invalid_parameters() {
    let file = crate::testutil::scenarios();
    let base = file.get("far-only-linear").unwrap();
    for (c, change) in [(1.5, None), (-0.1, None), (0.5, Some(31.0))] {
        let scn = crate::testutil::stereo_variant(base, c, change);
        let (far, near) = crate::testutil::smoke_speech();
        let pool = SpeechPool { far, near };
        let mut rng = ChaCha8Rng::seed_from_u64(1);
        assert!(
            matches!(
                simulate(&scn, &file.setup, &pool, &mut rng),
                Err(SimError::InvalidScenario(_))
            ),
            "c {c}, change {change:?} accepted"
        );
    }
}

#[test]
fn sim_echo_level_holds_on_both_sides_of_a_delay_jump() {
    // T6.6b: the echo is normalised over the far-end regions as they arrive at the mic in
    // each delay segment, so the level after a delay jump is the configured one too
    // (before, the first segment's delay was used for the whole call: -29.95 dBFS).
    let scn = scenarios().get("delay-jump").unwrap();
    let o = run_scenario("delay-jump");
    let n = o.mic.len();
    let arrived = arrived_regions(&o.far_regions, &o.delay, n);
    let target = scn.echo_level_dbfs;
    let level = db(active_rms(&o.echo, &arrived).powi(2));
    eprintln!("echo level over arrived regions {level:.3} dBFS (target {target})");
    assert!(
        (level - target).abs() < 0.01,
        "echo {level:.2} dBFS vs {target}"
    );
    // Arrival follows each segment's delay.
    let jump = o.delay.segments[1].start;
    let late: Vec<_> = arrived
        .iter()
        .filter(|r| r.start >= jump)
        .cloned()
        .collect();
    let shift = o.delay.segments[1].base.round() as usize;
    for r in &late {
        assert!(
            o.far_regions
                .iter()
                .any(|f| f.start + shift == r.start || r.start == jump),
            "{r:?} is not a far region shifted by the second segment's delay"
        );
    }
}

#[test]
fn sim_arrived_regions_follow_each_segment_delay() {
    // T6.6b: far-end speech is counted where it reaches the mic (recovery window and echo
    // level), segment by segment: 100 samples of delay until sample 1000, then 400.
    let delay = DelayTruth {
        drift_ppm: 0.0,
        segments: vec![
            DelaySegment {
                start: 0,
                base: 100.0,
            },
            DelaySegment {
                start: 1_000,
                base: 400.0,
            },
        ],
    };
    let far = vec![0..200, 800..1_200, 1_500..1_600];
    let got = arrived_regions(&far, &delay, 1_900);
    // The second region straddles the jump; the last arrives after the end.
    assert_eq!(got, vec![100..300, 900..1_000, 1_200..1_600]);
}
