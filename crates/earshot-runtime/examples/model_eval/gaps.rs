//! Offline evidence for bridging a gap between two lock stretches (T6.23 follow-up),
//! evaluation only: the informational `bridged_gaps_with_echo_path` of the gate12 corpus.
//! The final pass bridges every candidate gap (`earshot_runtime::echo::cleaning_stretches`);
//! on the GATE-12 takes and the simulator the evidence rule did not earn a place there
//! (private notes). Moved here from `earshot_runtime::echo` (T12.51), its only caller.

use earshot_core::aec::delay::{DelayConfig, DelayEstimator, DelayState};
use earshot_core::audio::MonoBuffer;
use earshot_runtime::echo::{EchoLock, MAX_LOCK_GAP_MS};
use earshot_runtime::live::echo::DELAY_STEP_MS;

/// `earshot_runtime::echo::cleaning_stretches` with each candidate gap (one that meets its
/// time and delay conditions and is longer than 0 ms) bridged only when `evidence(prev,
/// next)` holds for the recorded stretches on either side, for example
/// [`gap_has_echo_path`]. With evidence that always holds it equals `cleaning_stretches`
/// (`post_echo_cleaning_stretches_with_ask_only_for_candidate_gaps`).
pub fn cleaning_stretches_with(
    locks: &[EchoLock],
    mut evidence: impl FnMut(&EchoLock, &EchoLock) -> bool,
) -> Vec<EchoLock> {
    let mut out = locks.to_vec();
    for k in 1..out.len() {
        let next = out[k];
        let prev = &mut out[k - 1];
        if next.from_ms > prev.to_ms
            && next.from_ms - prev.to_ms <= MAX_LOCK_GAP_MS
            && prev.delay_ms.abs_diff(next.delay_ms) <= DELAY_STEP_MS
            && evidence(&locks[k - 1], &locks[k])
        {
            prev.to_ms = next.from_ms;
        }
    }
    out
}

/// The gaps between the recorded `locks` that `stretches` (their cleaning stretches, one
/// per lock, as [`cleaning_stretches_with`] returns) bridge, as `(from_ms, to_ms)`.
pub fn gaps_bridged(locks: &[EchoLock], stretches: &[EchoLock]) -> Vec<(u64, u64)> {
    locks
        .iter()
        .zip(stretches)
        .filter(|(recorded, cleaned)| cleaned.to_ms > recorded.to_ms)
        .map(|(recorded, cleaned)| (recorded.to_ms, cleaned.to_ms))
        .collect()
}

/// Whether the stored recordings show the echo path inside the gap between two recorded
/// lock stretches `prev` and `next` (offline evidence for bridging it).
///
/// The rule, written before measuring and with no setting of its own: core's delay
/// estimator ([`DelayEstimator`] with [`DelayConfig::default`], the canceller's own lock
/// rules and thresholds unchanged), seeded with `prev`'s delay as a hint
/// ([`DelayEstimator::set_hint`]: one strong candidate within its agreement tolerance
/// locks, otherwise the usual confirmations), is fed the stored mic and reference of the
/// gap alone; the gap has the echo path when it locks at a hop inside the gap at a delay
/// within [`DELAY_STEP_MS`] of `prev`'s or `next`'s (the rule that splits stretches). Before
/// the gap the estimator sees only the reference (its history primed with the
/// `max_delay` samples before the gap, the mic silent), so no evidence from a locked
/// stretch carries over. A strong candidate needs the canceller's peak confidence and
/// Welch coherence at its delay (`DelayConfig::verify_coherence`), so unrelated speech on
/// the two tracks (a headphones interlude) does not count.
///
/// `mic` is the stored canceller output: in a gap where the canceller still cancels well,
/// the residual echo may be too weak to lock on, and then the gap is not bridged. That
/// keeps the near end safe (nothing is attributed to echo without evidence) at the cost
/// of echo the transcript still shows. Audio at another rate than the estimator's
/// (16 kHz) or a gap outside the recordings gives no evidence.
pub fn gap_has_echo_path(
    mic: &MonoBuffer,
    reference: &MonoBuffer,
    prev: &EchoLock,
    next: &EchoLock,
) -> bool {
    let cfg = DelayConfig::default();
    if mic.rate != reference.rate || mic.rate.hz() != cfg.rate_hz {
        return false;
    }
    let per_ms = (cfg.rate_hz / 1_000) as usize;
    let to_sample = |ms: u64| {
        usize::try_from(ms)
            .unwrap_or(usize::MAX)
            .saturating_mul(per_ms)
    };
    let len = mic.samples.len().min(reference.samples.len());
    let (a, b) = (to_sample(prev.to_ms), to_sample(next.from_ms).min(len));
    if a >= b {
        return false;
    }
    let mut estimator = DelayEstimator::new(cfg);
    estimator.set_hint(to_sample(prev.delay_ms) as f64);
    let prime = cfg.max_delay.min(a);
    estimator.push(&vec![0.0; prime], &reference.samples[a - prime..a]);
    let tolerance = to_sample(DELAY_STEP_MS) as f64;
    let near = |d: f64, lock: &EchoLock| (d - to_sample(lock.delay_ms) as f64).abs() <= tolerance;
    let mut at = a;
    while at < b {
        let end = (at + cfg.hop).min(b);
        estimator.push(&mic.samples[at..end], &reference.samples[at..end]);
        if let DelayState::Locked { delay, .. } = estimator.state()
            && (near(delay, prev) || near(delay, next))
        {
            return true;
        }
        at = end;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use earshot_runtime::echo::cleaning_stretches;

    fn lock(from_ms: u64, to_ms: u64, delay_ms: u64) -> EchoLock {
        EchoLock {
            from_ms,
            to_ms,
            delay_ms,
        }
    }

    const RATE: earshot_core::audio::SampleRate = earshot_core::audio::PIPELINE_RATE;

    fn noise(seed: u64, n: usize, amp: f32) -> Vec<f32> {
        use rand_chacha::ChaCha8Rng;
        use rand_core::{Rng, SeedableRng};
        let mut rng = ChaCha8Rng::seed_from_u64(seed);
        (0..n)
            .map(|_| ((rng.next_u32() >> 8) as f32 / (1u32 << 24) as f32 * 2.0 - 1.0) * amp)
            .collect()
    }

    /// `x` delayed by `delay_ms` and scaled by `gain`, same length.
    fn delayed(x: &[f32], delay_ms: usize, gain: f32) -> Vec<f32> {
        let d = delay_ms * 16;
        (0..x.len())
            .map(|i| if i >= d { x[i - d] * gain } else { 0.0 })
            .collect()
    }

    fn add(a: &[f32], b: &[f32]) -> Vec<f32> {
        a.iter().zip(b).map(|(x, y)| x + y).collect()
    }

    fn buffer(x: Vec<f32>) -> MonoBuffer {
        MonoBuffer::new(RATE, x)
    }

    /// Two stretches at `delay_ms` around a gap from 10 s to 17 s.
    fn gap_locks(delay_ms: u64) -> (EchoLock, EchoLock) {
        (lock(0, 10_000, delay_ms), lock(17_000, 30_000, delay_ms))
    }

    #[test]
    fn post_echo_gap_evidence_needs_the_echo_path_at_the_stretches_delay() {
        let n = 30 * 16_000;
        let far = noise(1, n, 0.3);
        let near = noise(2, n, 0.1);
        let (prev, next) = gap_locks(180);
        let reference = buffer(far.clone());
        // Echo at 180 ms under near-end talk at about 0 dB: the path is there.
        let echo = buffer(add(&delayed(&far, 180, 0.33), &near));
        assert!(gap_has_echo_path(&echo, &reference, &prev, &next));
        // Headphones: the mic hears only the near end.
        assert!(!gap_has_echo_path(
            &buffer(near.clone()),
            &reference,
            &prev,
            &next
        ));
        // An echo path at another delay (400 ms) is not the stretches' path.
        let elsewhere = buffer(add(&delayed(&far, 400, 0.33), &near));
        assert!(!gap_has_echo_path(&elsewhere, &reference, &prev, &next));
        // The far end silent in the gap: nothing to find (and nothing to clean).
        let mut quiet = far.clone();
        quiet[10 * 16_000 - 9_600..17 * 16_000].fill(0.0);
        let quiet_echo = buffer(add(&delayed(&quiet, 180, 0.33), &near));
        assert!(!gap_has_echo_path(
            &quiet_echo,
            &buffer(quiet),
            &prev,
            &next
        ));
        // Echo only before and after the gap (the path goes away inside it, a headphones
        // interlude): the locked stretches lend the gap no evidence.
        let mut interlude = add(&delayed(&far, 180, 0.33), &near);
        interlude[10 * 16_000..17 * 16_000].copy_from_slice(&near[10 * 16_000..17 * 16_000]);
        assert!(!gap_has_echo_path(
            &buffer(interlude),
            &reference,
            &prev,
            &next
        ));
        // Another rate, or a gap outside the recordings, gives no evidence.
        let other = MonoBuffer::new(earshot_core::audio::SampleRate::new(8_000).unwrap(), far);
        assert!(!gap_has_echo_path(&other, &other, &prev, &next));
        let late = (lock(40_000, 41_000, 180), lock(45_000, 50_000, 180));
        assert!(!gap_has_echo_path(&echo, &reference, &late.0, &late.1));
    }

    #[test]
    fn post_echo_cleaning_stretches_with_ask_only_for_candidate_gaps() {
        let locks = [
            lock(0, 6_000, 150),
            lock(9_000, 12_000, 152),
            // Another delay: never a candidate, never asked.
            lock(14_000, 20_000, 400),
            lock(20_000, 30_000, 400),
        ];
        let mut asked = Vec::new();
        let none = cleaning_stretches_with(&locks, |p, n| {
            asked.push((p.to_ms, n.from_ms));
            false
        });
        assert_eq!(none, locks);
        // A 0 ms gap needs no evidence and is not asked about.
        assert_eq!(asked, [(6_000, 9_000)]);
        // With evidence that always holds, the final pass's rule exactly.
        let all = cleaning_stretches_with(&locks, |_, _| true);
        assert_eq!(all, cleaning_stretches(&locks));
        assert_eq!(gaps_bridged(&locks, &all), [(6_000, 9_000)]);
        assert!(gaps_bridged(&locks, &none).is_empty());
        for locks in [
            vec![lock(41_000, 47_000, 200), lock(54_000, 86_000, 202)],
            vec![
                lock(0, 1_000, 150),
                lock(1_000 + MAX_LOCK_GAP_MS, 50_000, 150),
            ],
            vec![
                lock(0, 1_000, 150),
                lock(1_001 + MAX_LOCK_GAP_MS, 50_000, 150),
            ],
            vec![
                lock(0, 6_000, 150),
                lock(12_000, 20_000, 150 + DELAY_STEP_MS + 1),
            ],
            vec![
                lock(0, 6_000, 150),
                lock(12_000, 20_000, 150 + DELAY_STEP_MS),
            ],
            Vec::new(),
        ] {
            assert_eq!(
                cleaning_stretches_with(&locks, |_, _| true),
                cleaning_stretches(&locks),
                "{locks:?}"
            );
        }
    }

    /// Smoke utterances (speaker 61, speaker 121), as the core simulator's tests use them.
    fn smoke_speech() -> (Vec<Vec<f32>>, Vec<Vec<f32>>) {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/smoke");
        let mut names: Vec<String> = std::fs::read_dir(&dir)
            .unwrap_or_else(|e| panic!("{}: {e}", dir.display()))
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .filter(|n| n.ends_with(".flac"))
            .collect();
        names.sort();
        let (mut a, mut b) = (Vec::new(), Vec::new());
        for name in names {
            let (rate, audio) = earshot_store::read_flac(&dir.join(&name)).unwrap();
            assert_eq!(rate, 16_000);
            if name.starts_with("61-") {
                a.push(audio);
            } else if name.starts_with("121-") {
                b.push(audio);
            }
        }
        assert_eq!((a.len(), b.len()), (10, 10), "smoke set");
        (a, b)
    }

    /// What the production canceller makes of `mic` (default config, cleaned sample k at
    /// frame k), as the stored mic FLAC holds it.
    fn cancelled(mic: &[f32], reference: &[f32]) -> Vec<f32> {
        use earshot_core::aec::{BLOCK, EchoCanceller, EchoCancellerConfig};
        let mut aec = EchoCanceller::new(EchoCancellerConfig::default());
        let latency = aec.output_latency();
        let total = (mic.len() + latency).div_ceil(BLOCK) * BLOCK;
        let pad = |x: &[f32]| {
            let mut v = x.to_vec();
            v.resize(total, 0.0);
            v
        };
        let mut out = vec![0.0; total];
        aec.process(&pad(mic), &pad(reference), &mut out);
        earshot_core::audio::quantise_pcm16(&out[latency..latency + mic.len()])
    }

    /// The rule of [`gap_has_echo_path`] on the dev scenarios of
    /// `acceptance/aec_scenarios.toml` (smoke speech, the call simulator): a gap from 10 s to
    /// 17 s, in double talk, between stretches at the true echo delay. Written before
    /// GATE-12 was measured; the expectations are the design's: with no echo path
    /// (headphones) nothing is found on the raw mic or the canceller output; with an echo
    /// path the raw mic shows it in double talk at both SERs. On the canceller output the
    /// path shows only where the residual echo is strong enough to lock on; the measured
    /// outcome is pinned so that a change is noticed.
    #[test]
    fn post_echo_gap_evidence_on_simulated_double_talk() {
        use earshot_core::sim::{ScenarioFile, SpeechPool, simulate};
        use rand_chacha::ChaCha8Rng;
        use rand_core::SeedableRng;
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../acceptance/aec_scenarios.toml");
        let file: ScenarioFile = toml::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        let (far, near) = smoke_speech();
        let pool = SpeechPool {
            far: &far,
            near: &near,
        };
        let mut found = Vec::new();
        for name in [
            "double-talk-ser-0",
            "double-talk-ser-10",
            "noisy-office",
            "headphones",
        ] {
            let scn = file.scenario.iter().find(|s| s.name == name).unwrap();
            let out = simulate(
                scn,
                &file.setup,
                &pool,
                &mut ChaCha8Rng::seed_from_u64(scn.seed),
            )
            .unwrap();
            let delay_ms = (out.delay.delay_at(10 * 16_000) / 16.0).round() as u64;
            let (prev, next) = gap_locks(delay_ms);
            let reference = buffer(out.reference.clone());
            let raw = buffer(earshot_core::audio::quantise_pcm16(&out.mic));
            let clean = buffer(cancelled(&out.mic, &out.reference));
            found.push((
                name,
                gap_has_echo_path(&raw, &reference, &prev, &next),
                gap_has_echo_path(&clean, &reference, &prev, &next),
            ));
        }
        eprintln!("gap evidence (scenario, raw mic, canceller output): {found:?}");
        assert_eq!(found, EXPECTED_SIM_EVIDENCE);
    }

    /// Measured 2026-11-01 (see the test).
    const EXPECTED_SIM_EVIDENCE: [(&str, bool, bool); 4] = [
        ("double-talk-ser-0", true, false),
        ("double-talk-ser-10", true, false),
        ("noisy-office", true, false),
        ("headphones", false, false),
    ];
}
