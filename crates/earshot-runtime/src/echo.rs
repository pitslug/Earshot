//! Echo-cleaning provenance (T6.4g), kept device-locally next to the pass backends: the
//! stretches of a live session in which the echo canceller held a delay lock, with the
//! delay ([`EchoLock`]; the final pass cleans each stretch with its own delay, also when it
//! is re-run later), and, per pass, what the cleaning removed and whether the final
//! pass's speaker check ran (and why not). The final pass cleans in
//! [`cleaning_stretches`]: the recorded stretches with short gaps at the same delay
//! bridged (T6.23); outside them nothing is cleaned.

use std::time::Duration;

use earshot_core::aec::delay::{DelayConfig, DelayEstimator, DelayState};
use earshot_core::asr::{AsrError, EchoDedupConfig, Word, dedupe_echo};
use earshot_core::audio::MonoBuffer;
use earshot_core::identity::EntityId;
use earshot_core::model::TranscriptPass;
use earshot_store::Store;
use serde::{Deserialize, Serialize};

use crate::RuntimeError;
use crate::live::echo::DELAY_STEP_MS;
use crate::local::{LocalKey, MeetingRecord};

/// A stretch of meeting time in which the echo canceller held a delay lock.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct EchoLock {
    /// Start (meeting time, ms): the stats snapshot before the lock was first seen.
    pub from_ms: u64,
    /// End (meeting time, ms): the last snapshot that still showed the lock.
    pub to_ms: u64,
    /// The echo delay (mic behind reference), ms.
    pub delay_ms: u64,
}

impl EchoLock {
    /// The delay.
    #[must_use]
    pub fn delay(&self) -> Duration {
        Duration::from_millis(self.delay_ms)
    }

    /// Whether `[start_ms, end_ms]` overlaps the lock.
    #[must_use]
    pub fn overlaps(&self, start_ms: u64, end_ms: u64) -> bool {
        start_ms <= self.to_ms && self.from_ms <= end_ms
    }

    /// Whether `ms` lies in the lock.
    #[must_use]
    pub fn covers(&self, ms: u64) -> bool {
        self.from_ms <= ms && ms <= self.to_ms
    }
}

/// Longest gap between two lock stretches of the same delay that [`cleaning_stretches`]
/// bridges (30 s). A judgement made before measuring (T6.23), not derived: "a few
/// sentences of talk-over". The canceller drops its lock after 1.5 s of weak estimates
/// while the reference plays (`DelayConfig::release_hops`) and re-locks only after fresh
/// agreeing estimates, which double talk withholds. The bridge cannot tell such a gap
/// from a real loss of the echo path: a headphones interlude shorter than 30 s between
/// two loudspeaker stretches at the same delay is bridged too. Near-end safety in a
/// bridged gap rests on each rule's own evidence: the text rule needs 3 or more in-order
/// repeated words within +-1 s of the delay, the speaker check its calibrated margin and
/// the `min_solo` rule for "Me".
pub const MAX_LOCK_GAP_MS: u64 = 30_000;

/// The stretches the final pass cleans in (T6.23): the recorded lock stretches, each
/// extended over a gap of at most [`MAX_LOCK_GAP_MS`] to the next stretch when both have
/// the same delay (within [`DELAY_STEP_MS`], the rule that splits stretches). On a real
/// recording the canceller lost its lock for seconds of talk-over between stretches at about
/// one delay, and an echo run in that gap was kept: the delay estimator losing
/// confidence in double talk is not the echo path going away, and the path is the same
/// on both sides. A gap is still only a precondition: both rules need their own evidence
/// inside it (a run at the delay, a voice that sounds like the far end while it talks).
/// A gap at another delay, a longer gap, and the time before the first or after the last
/// stretch are not bridged. Stretches are in time order, as recorded.
#[must_use]
pub fn cleaning_stretches(locks: &[EchoLock]) -> Vec<EchoLock> {
    cleaning_stretches_with(locks, |_, _| true)
}

/// [`cleaning_stretches`] with each candidate gap (one that meets its time and delay
/// conditions and is longer than 0 ms) bridged only when `evidence(prev, next)` holds for
/// the recorded stretches on either side, for example [`gap_has_echo_path`]. Evaluation
/// only: the final pass bridges every candidate gap ([`cleaning_stretches`]); on the
/// GATE-12 takes and the simulator the evidence rule did not earn a place there
/// (private notes).
#[must_use]
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

/// The gaps between `locks` that [`cleaning_stretches`] bridges, as `(from_ms, to_ms)`.
#[must_use]
pub fn bridged_gaps(locks: &[EchoLock]) -> Vec<(u64, u64)> {
    gaps_bridged(locks, &cleaning_stretches(locks))
}

/// The gaps between the recorded `locks` that `stretches` (their cleaning stretches, one
/// per lock, as [`cleaning_stretches_with`] returns) bridge, as `(from_ms, to_ms)`.
#[must_use]
pub fn gaps_bridged(locks: &[EchoLock], stretches: &[EchoLock]) -> Vec<(u64, u64)> {
    locks
        .iter()
        .zip(stretches)
        .filter(|(recorded, cleaned)| cleaned.to_ms > recorded.to_ms)
        .map(|(recorded, cleaned)| (recorded.to_ms, cleaned.to_ms))
        .collect()
}

/// Whether the stored recordings show the echo path inside the gap between two recorded
/// lock stretches `prev` and `next` (offline evidence for bridging it, T6.23 follow-up;
/// evaluation only, see [`cleaning_stretches_with`]).
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
#[must_use]
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

/// What echo cleaning did to one pass of a meeting's "Me" transcript.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EchoReport {
    /// The locks the cleaning used; empty: no lock, nothing was removed.
    pub locks: Vec<EchoLock>,
    /// Mic words removed by the text rule (`asr::dedupe_echo`).
    pub text_removed: usize,
    /// Mic words removed by the speaker check (final pass only).
    pub speaker_removed: usize,
    /// Why the speaker check did not run, when it did not (no lock, a single track, or an
    /// embedding model without a calibrated margin); `None` when it ran or does not apply
    /// (live pass).
    pub speaker_check_skipped: Option<String>,
    /// Gaps between recorded locks that the final pass bridged and cleaned in
    /// ([`bridged_gaps`]), as `(from_ms, to_ms)`; empty for the live pass and for reports
    /// stored before T6.23.
    #[serde(default)]
    pub bridged_gaps: Vec<(u64, u64)>,
    /// Mic words removed because their cluster is a mostly-echo cluster that sounds like
    /// the far end (final pass only, T6.23a: the words that cluster kept after the text
    /// rule and the speaker check); 0 for the live pass and for reports stored before
    /// T6.23a.
    #[serde(default)]
    pub cluster_removed: usize,
}

impl EchoReport {
    /// Words removed in total.
    #[must_use]
    pub fn removed(&self) -> usize {
        self.text_removed + self.speaker_removed + self.cluster_removed
    }
}

/// The final pass's text rule (T6.4g): which `mic` words are echo of `reference` words,
/// by [`dedupe_echo`] run once per cleaning stretch ([`cleaning_stretches`] of the
/// recorded `locks`) with that stretch's delay; a word is echo only if a stretch covering
/// its start removed it. Outside every cleaning stretch nothing is echo.
///
/// # Errors
/// [`AsrError::Config`] from [`dedupe_echo`].
pub fn text_echo_mask(
    mic: &[Word],
    reference: &[Word],
    locks: &[EchoLock],
) -> Result<Vec<bool>, AsrError> {
    let mut echo = vec![false; mic.len()];
    for lock in &cleaning_stretches(locks) {
        let out = dedupe_echo(mic, reference, &EchoDedupConfig::with_delay(lock.delay()))?;
        for (i, removed) in removed_mask(mic, &out.kept).into_iter().enumerate() {
            if removed && lock.covers(mic[i].start.as_millis()) {
                echo[i] = true;
            }
        }
    }
    Ok(echo)
}

/// Which of `words` are not in `kept` (an in-order subsequence of `words`).
fn removed_mask(words: &[Word], kept: &[Word]) -> Vec<bool> {
    let mut k = 0;
    words
        .iter()
        .map(|w| {
            if k < kept.len() && kept[k] == *w {
                k += 1;
                false
            } else {
                true
            }
        })
        .collect()
}

pub(crate) fn locks_key(meeting: EntityId) -> LocalKey {
    MeetingRecord::EchoLocks.key(&[], meeting, &[])
}

pub(crate) fn report_key(meeting: EntityId, pass: TranscriptPass) -> LocalKey {
    let pass = match pass {
        TranscriptPass::Live => "live",
        TranscriptPass::Final => "final",
    };
    MeetingRecord::EchoReport.key(&[pass], meeting, &[])
}

fn json<T: Serialize>(v: &T) -> Result<Vec<u8>, RuntimeError> {
    serde_json::to_vec(v).map_err(|_| RuntimeError::Internal("echo state does not serialise"))
}

fn parse<T: for<'de> Deserialize<'de>>(v: &[u8]) -> Result<T, RuntimeError> {
    serde_json::from_slice(v)
        .map_err(|_| RuntimeError::Internal("stored echo state does not parse"))
}

/// Records the lock stretches of `meeting`'s live session (empty: never locked).
///
/// # Errors
/// Store errors.
pub fn record_echo_locks(
    store: &mut Store,
    meeting: EntityId,
    locks: &[EchoLock],
) -> Result<(), RuntimeError> {
    crate::local::set(store, &locks_key(meeting), &json(&locks)?)?;
    Ok(())
}

/// The lock stretches recorded by [`record_echo_locks`]; empty when there were none or
/// nothing was recorded (an older meeting: no cleaning).
///
/// # Errors
/// Store errors; [`RuntimeError::Internal`] for a value that does not parse.
pub fn echo_locks(store: &Store, meeting: EntityId) -> Result<Vec<EchoLock>, RuntimeError> {
    match store.local_value(locks_key(meeting).as_str())? {
        Some(v) => parse(&v),
        None => Ok(Vec::new()),
    }
}
/// Records what echo cleaning did in `pass` of `meeting`.
///
/// # Errors
/// Store errors.
pub fn record_echo_report(
    store: &mut Store,
    meeting: EntityId,
    pass: TranscriptPass,
    report: &EchoReport,
) -> Result<(), RuntimeError> {
    crate::local::set(store, &report_key(meeting, pass), &json(report)?)?;
    Ok(())
}

/// The report recorded by [`record_echo_report`], if any.
///
/// # Errors
/// Store errors; [`RuntimeError::Internal`] for a value that does not parse.
pub fn echo_report(
    store: &Store,
    meeting: EntityId,
    pass: TranscriptPass,
) -> Result<Option<EchoReport>, RuntimeError> {
    store
        .local_value(report_key(meeting, pass).as_str())?
        .map(|v| parse(&v))
        .transpose()
}

#[cfg(test)]
mod tests {
    use super::*;
    use earshot_core::asr::MeetingTime;

    fn lock(from_ms: u64, to_ms: u64, delay_ms: u64) -> EchoLock {
        EchoLock {
            from_ms,
            to_ms,
            delay_ms,
        }
    }

    /// Three words every 300 ms from `t`.
    fn run(texts: [&str; 3], t: u64) -> Vec<Word> {
        (0..3u64)
            .map(|k| Word::new(texts[k as usize], t + 300 * k, t + 300 * k + 250, 0.9))
            .collect()
    }

    #[test]
    fn post_echo_cleaning_stretches_bridge_a_short_gap_at_the_same_delay() {
        // The canceller dropped its lock during talk-over between two stretches at about
        // one delay; the gap goes to the earlier stretch.
        let locks = [lock(41_000, 47_000, 200), lock(54_000, 86_000, 202)];
        assert_eq!(
            cleaning_stretches(&locks),
            [lock(41_000, 54_000, 200), lock(54_000, 86_000, 202)]
        );
        // A gap of exactly the limit is bridged.
        let at_limit = [
            lock(0, 1_000, 150),
            lock(1_000 + MAX_LOCK_GAP_MS, 50_000, 150),
        ];
        assert_eq!(
            cleaning_stretches(&at_limit)[0].to_ms,
            1_000 + MAX_LOCK_GAP_MS
        );
    }

    #[test]
    fn post_echo_cleaning_stretches_keep_long_gaps_and_delay_changes() {
        let moved = 150 + DELAY_STEP_MS + 1;
        let locks = [
            lock(0, 6_000, 150),
            // Another delay (a device change): not the same echo path.
            lock(12_000, 20_000, moved),
            // A gap longer than the limit: no evidence for it.
            lock(20_001 + MAX_LOCK_GAP_MS, 400_000, moved),
        ];
        assert_eq!(cleaning_stretches(&locks), locks);
        assert!(cleaning_stretches(&[]).is_empty());
        let one = [lock(5, 10, 100)];
        assert_eq!(cleaning_stretches(&one), one);
        // Applying it twice changes nothing more.
        let bridged = cleaning_stretches(&[lock(0, 1_000, 150), lock(5_000, 9_000, 160)]);
        assert_eq!(cleaning_stretches(&bridged), bridged);
    }

    #[test]
    fn post_echo_text_mask_cleans_a_bridged_gap_and_nothing_in_a_long_one() {
        let long_gap_run = 20_000 + MAX_LOCK_GAP_MS / 2;
        let reference: Vec<Word> = [
            run(["alpha", "beta", "gamma"], 8_000),
            run(["delta", "echo", "foxtrot"], long_gap_run),
        ]
        .concat();
        let mic: Vec<Word> = reference
            .iter()
            .map(|w| w.shifted(MeetingTime::from_millis(180)))
            .collect();
        // The first run lies in a 6 s gap between two stretches at 180 and 182 ms, the
        // second in a gap longer than the limit.
        let locks = [
            lock(0, 5_000, 180),
            lock(11_000, 20_000, 182),
            lock(20_001 + MAX_LOCK_GAP_MS, 20_001 + 3 * MAX_LOCK_GAP_MS, 182),
        ];
        let echo = text_echo_mask(&mic, &reference, &locks).unwrap();
        assert_eq!(echo, [true, true, true, false, false, false]);
        // A single stretch is not bridged to anything: the run after it stays.
        assert_eq!(
            text_echo_mask(&mic, &reference, &locks[..1]).unwrap(),
            [false; 6]
        );
    }

    // ---- Offline evidence for bridging a gap (gap_has_echo_path) ----------------------

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
        let all = cleaning_stretches_with(&locks, |_, _| true);
        assert_eq!(all, cleaning_stretches(&locks));
        assert_eq!(gaps_bridged(&locks, &all), [(6_000, 9_000)]);
        assert!(gaps_bridged(&locks, &none).is_empty());
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
