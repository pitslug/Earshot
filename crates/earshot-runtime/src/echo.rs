//! Echo-cleaning provenance (T6.4g), kept device-locally next to the pass backends: the
//! stretches of a live session in which the echo canceller held a delay lock, with the
//! delay ([`EchoLock`]; the final pass cleans each stretch with its own delay, also when it
//! is re-run later), and, per pass, what the cleaning removed and whether the final
//! pass's speaker check ran (and why not). The final pass cleans in
//! [`cleaning_stretches`]: the recorded stretches with short gaps at the same delay
//! bridged (T6.23); outside them nothing is cleaned.

use std::time::Duration;

use earshot_core::asr::{AsrError, EchoDedupConfig, Word, dedupe_echo};
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
    let mut out = locks.to_vec();
    for k in 1..out.len() {
        let next = out[k];
        let prev = &mut out[k - 1];
        if next.from_ms > prev.to_ms
            && next.from_ms - prev.to_ms <= MAX_LOCK_GAP_MS
            && prev.delay_ms.abs_diff(next.delay_ms) <= DELAY_STEP_MS
        {
            prev.to_ms = next.from_ms;
        }
    }
    out
}

/// The gaps between `locks` that [`cleaning_stretches`] bridges, as `(from_ms, to_ms)`.
#[must_use]
pub fn bridged_gaps(locks: &[EchoLock]) -> Vec<(u64, u64)> {
    locks
        .iter()
        .zip(cleaning_stretches(locks))
        .filter(|(recorded, cleaned)| cleaned.to_ms > recorded.to_ms)
        .map(|(recorded, cleaned)| (recorded.to_ms, cleaned.to_ms))
        .collect()
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
        assert_eq!(bridged_gaps(&locks), [(47_000, 54_000)]);
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
        assert!(bridged_gaps(&locks).is_empty());
        assert!(cleaning_stretches(&[]).is_empty());
        // A 0 ms gap (a stretch split by a delay step) is not a bridged gap.
        assert!(bridged_gaps(&[lock(0, 6_000, 150), lock(6_000, 9_000, 152)]).is_empty());
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
}
