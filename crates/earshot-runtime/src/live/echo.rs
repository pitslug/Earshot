//! Echo cleaning of the live "Me" transcript (T6.4g, PLAN.md 4.0: the transcript must be
//! clean).
//!
//! Far-end speech that leaks into the mic is recognised a second time on the mic track.
//! [`LiveEchoFilter`] applies the core text rule ([`dedupe_echo_segments`]) to every mic
//! final before it is shown or stored, against the reference ("Them") words heard so far:
//!
//! - **Only with a lock.** The filter records every stretch of meeting time in which the
//!   echo canceller held a delay lock ([`EchoLock`], from each stats snapshot; a delay
//!   change of more than [`DELAY_STEP_MS`] starts a new stretch). A mic final is cleaned
//!   with the delay of the stretch it falls in, also when it is released after the lock
//!   was dropped; outside every stretch (headphones, a muted speaker, a canceller that has
//!   not converged) nothing is removed and finals pass at once (T2.4f: no echo cleaning
//!   without a lock). The stretches are stored for the final pass
//!   ([`crate::echo::record_echo_locks`]).
//! - **Held until the reference is in.** Echo lags the far end, but the two tracks are
//!   transcribed independently, so the reference words a mic final repeats may still be
//!   in an open reference segment. A mic final is held while an open reference segment
//!   (seen through its partials) starts early enough to hold words it could repeat
//!   (`segment end - delay + tolerance`), and at most [`MAX_HOLD`] of meeting time.
//!   With live ASR on per-track workers (T6.10a) the reference can be decoded behind
//!   capture: [`LiveEchoFilter::reference_progress`] then tells how far the reference
//!   has been decoded, a final is held until that passes its reference stretch, and
//!   [`MAX_HOLD`] counts from the reference's progress instead of capture's;
//!   finals are released in order, and everything is released at the end. With partial
//!   decodes off (backpressure) no reference segment is seen open, so a final is checked
//!   against the reference finals only; the final pass cleans again with the whole
//!   reference.
//! - **Runs across finals.** The rule needs a run of several repeated words; the last
//!   released mic words ([`CONTEXT_WORDS`]) are checked with each final so a run that
//!   started in the previous final is still found (words already shown stay shown).
//! - **Partials.** A mic partial is shown without the words that already repeat the
//!   reference heard so far (finals and the open segment), with the same rule.
//!
//! Removed words are counted ([`LiveEchoFilter::removed`]) for the meeting's report.
//!
//! **The live draft may still show a single echo word.** The text rule needs a run of
//! `min_run` (3) repeated words, so an isolated echo word between the user's words can
//! survive here. That is accepted for the live draft (coordinator decision for the owner,
//! 2026-10-18): the transcript that must be clean is the stored final pass, where the
//! speaker check ([`crate::post`]) removes such fragments.

use std::collections::VecDeque;
use std::time::Duration;

use earshot_core::asr::{
    EchoDedupConfig, MeetingTime, PartialSegment, Segment, TranscriptEvent, Word,
    dedupe_echo_segments,
};
use earshot_core::identity::EntityId;
use earshot_core::pipeline::PipelineStats;

use crate::echo::EchoLock;

/// Longest a mic final waits for the reference transcript (meeting time).
pub const MAX_HOLD: Duration = Duration::from_secs(10);

/// Released mic words checked again with each new final.
pub const CONTEXT_WORDS: usize = 8;

/// A lock whose delay moves by more than this starts a new [`EchoLock`] stretch (a device
/// change, or slow drift accumulating); within it the first delay holds (the text rule
/// tolerates 1 s).
pub const DELAY_STEP_MS: u64 = 20;

/// Reference words older than this behind the newest audio are dropped.
const REFERENCE_KEEP: Duration = Duration::from_secs(120);

/// Id of the synthetic context segment (never stored or shown).
const CONTEXT_ID: EntityId = EntityId::from_bytes([0; 16]);

fn ms(d: Duration) -> u64 {
    u64::try_from(d.as_millis()).unwrap_or(u64::MAX)
}

fn plus(t: MeetingTime, d: Duration) -> MeetingTime {
    MeetingTime::from_millis(t.as_millis().saturating_add(ms(d)))
}

fn minus(t: MeetingTime, d: Duration) -> MeetingTime {
    MeetingTime::from_millis(t.as_millis().saturating_sub(ms(d)))
}

/// A mic final leaving the filter.
#[derive(Debug, Clone, PartialEq)]
pub enum Released {
    /// Show and store it (echo words removed).
    Kept(Segment),
    /// It was all echo: nothing to store; the segment with this id is discarded.
    Dropped(EntityId),
}

/// See the module docs.
#[derive(Debug, Default)]
pub struct LiveEchoFilter {
    /// Lock stretches so far, in time order.
    locks: Vec<EchoLock>,
    /// The last stretch is still locked (the newest stats showed the lock).
    lock_open: bool,
    /// Meeting time of the previous stats snapshot.
    last_stats: MeetingTime,
    /// Reference final words, in time order.
    reference: Vec<Word>,
    /// The open reference segment: its id and its words so far.
    open: Option<(EntityId, Vec<Word>)>,
    /// Mic finals waiting for the reference.
    held: VecDeque<Segment>,
    /// The last released mic words.
    context: VecDeque<Word>,
    /// Newest meeting time seen (pipeline progress).
    now: MeetingTime,
    /// How far the reference has been decoded, when its live ASR runs behind capture
    /// (a worker thread); `None`: decoded in step with capture.
    reference_done: Option<MeetingTime>,
    /// How far the mic has been decoded, when its live ASR runs behind capture.
    mic_done: Option<MeetingTime>,
    /// The mic's live ASR stopped for the rest of the meeting (failed, cut or crashed): no
    /// mic final is coming, so its frozen progress no longer keeps reference words.
    mic_stopped: bool,
    /// Mic words removed as echo so far.
    removed: usize,
}

impl LiveEchoFilter {
    /// A filter without a lock.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Follows the canceller: records the lock (and its delay) of every stats snapshot as
    /// [`EchoLock`] stretches.
    pub fn stats(&mut self, stats: &PipelineStats) {
        let delay = stats.aec.and_then(|a| {
            a.locked
                .then_some(a.delay_ms)
                .flatten()
                .filter(|d| d.is_finite() && *d >= 0.0)
                // Whole milliseconds: the unit the locks are stored in for the final pass.
                .map(|d| d.round() as u64)
        });
        let (prev, end) = (self.last_stats.as_millis(), stats.end.as_millis());
        match delay {
            Some(d) => match self.locks.last_mut() {
                Some(last) if self.lock_open && last.delay_ms.abs_diff(d) <= DELAY_STEP_MS => {
                    last.to_ms = last.to_ms.max(end);
                }
                _ => {
                    self.locks.push(EchoLock {
                        from_ms: prev.min(end),
                        to_ms: end,
                        delay_ms: d,
                    });
                    self.lock_open = true;
                }
            },
            None => self.lock_open = false,
        }
        self.last_stats = self.last_stats.max(stats.end);
        self.progress(stats.end);
    }

    /// How far the reference's live ASR has decoded (T6.10a): mic finals are held until
    /// it passes the stretch that could hold the words they repeat.
    pub fn reference_progress(&mut self, done: MeetingTime) {
        self.reference_done = Some(self.reference_done.map_or(done, |d| d.max(done)));
    }

    /// How far the mic's live ASR has decoded: reference words a coming mic final may repeat
    /// are kept until it has passed them, however far capture is ahead (a catch-up, T9.1l).
    pub fn mic_progress(&mut self, done: MeetingTime) {
        if self.mic_stopped {
            return;
        }
        self.mic_done = Some(self.mic_done.map_or(done, |d| d.max(done)));
    }

    /// The mic's live ASR stopped for good (its worker failed or crashed): reference words
    /// are kept only for the mic finals still held, so they no longer grow for the rest of
    /// the meeting behind a frozen mic frontier (TODO 51).
    pub fn mic_stopped(&mut self) {
        self.mic_stopped = true;
        self.mic_done = None;
    }

    /// Pipeline progress (level meters and stats).
    pub fn progress(&mut self, now: MeetingTime) {
        self.now = self.now.max(now);
    }

    /// The current delay lock (`None` when the newest stats showed none).
    #[must_use]
    pub fn delay(&self) -> Option<Duration> {
        self.lock_open
            .then(|| self.locks.last().map(EchoLock::delay))
            .flatten()
    }

    /// The lock stretches so far.
    #[must_use]
    pub fn locks(&self) -> &[EchoLock] {
        &self.locks
    }

    /// Mic words removed as echo so far.
    #[must_use]
    pub fn removed(&self) -> usize {
        self.removed
    }

    /// The delay for mic speech in `[start, end]`: the newest lock stretch overlapping it
    /// (the current one reaches to now and beyond); `None` outside every stretch.
    fn delay_for(&self, start: MeetingTime, end: MeetingTime) -> Option<Duration> {
        let n = self.locks.len();
        self.locks
            .iter()
            .enumerate()
            .rev()
            .find(|(i, l)| {
                // The open stretch reaches to now and beyond.
                let open = self.lock_open && *i + 1 == n;
                let lock = if open {
                    EchoLock {
                        to_ms: u64::MAX,
                        ..**l
                    }
                } else {
                    **l
                };
                lock.overlaps(start.as_millis(), end.as_millis())
            })
            .map(|(_, l)| l.delay())
    }

    /// A reference ("Them") transcript event.
    pub fn reference(&mut self, event: &TranscriptEvent) {
        match event {
            TranscriptEvent::Partial(p) => {
                self.open = Some((p.segment.id, p.segment.words.clone()));
            }
            TranscriptEvent::Final(s) => {
                if self.open.as_ref().is_some_and(|(id, _)| *id == s.id) {
                    self.open = None;
                }
                self.reference.extend(s.words.iter().cloned());
                self.reference.sort_by_key(|w| w.start);
                // Behind the slowest of capture and both tracks' decoding (T9.1l).
                let oldest = [self.reference_done, self.mic_done, self.held_from()]
                    .into_iter()
                    .flatten()
                    .fold(self.now, MeetingTime::min);
                let keep_from = minus(oldest, REFERENCE_KEEP);
                self.reference.retain(|w| w.start >= keep_from);
            }
            TranscriptEvent::Discarded { id } => {
                if self.open.as_ref().is_some_and(|(open, _)| open == id) {
                    self.open = None;
                }
            }
        }
    }

    /// Reference words heard so far (finals, then the open segment's).
    fn heard(&self) -> Vec<Word> {
        let mut words = self.reference.clone();
        if let Some((_, open)) = &self.open {
            words.extend(open.iter().cloned());
            words.sort_by_key(|w| w.start);
        }
        words
    }

    /// A mic partial as it may be shown: without the words that already repeat the
    /// reference.
    #[must_use]
    pub fn mic_partial(&self, partial: PartialSegment) -> PartialSegment {
        let Some(delay) = self.delay_for(partial.segment.start, partial.segment.end) else {
            return partial;
        };
        if partial.segment.words.is_empty() {
            return partial;
        }
        let words = &partial.segment.words;
        let Ok((cleaned, _)) = dedupe_echo_segments(
            std::slice::from_ref(&partial.segment),
            &self.heard(),
            &EchoDedupConfig::with_delay(delay),
        ) else {
            return partial;
        };
        let kept: Vec<Word> = cleaned.into_iter().flat_map(|s| s.words).collect();
        // Committed words that survived (kept words are a subsequence of the original).
        let mut committed = 0;
        let mut k = 0;
        for (i, w) in words.iter().enumerate() {
            if k < kept.len() && kept[k] == *w {
                if i < partial.committed_words {
                    committed += 1;
                }
                k += 1;
            }
        }
        let mut segment = Segment::from_words(
            partial.segment.id,
            partial.segment.track,
            kept,
            partial.segment.start,
        );
        segment.speaker_id = partial.segment.speaker_id;
        PartialSegment {
            segment,
            committed_words: committed,
        }
    }

    /// The start of the earliest mic final still held (`None`: none held): nothing before it
    /// waits to be released.
    #[must_use]
    pub fn held_from(&self) -> Option<MeetingTime> {
        self.held.iter().map(|s| s.start).min()
    }

    /// Queues a mic final.
    pub fn mic_final(&mut self, segment: Segment) {
        self.held.push_back(segment);
    }

    /// Whether the held final `s` may be released now.
    fn releasable(&self, s: &Segment) -> bool {
        let Some(delay) = self.delay_for(s.start, s.end) else {
            return true;
        };
        // The reference decoded so far: capture time, or the reference worker's progress
        // when that is behind.
        let now = self.reference_done.map_or(self.now, |d| d.min(self.now));
        if now >= plus(s.end, MAX_HOLD) {
            return true;
        }
        let tolerance = EchoDedupConfig::default().tolerance;
        // Latest reference start that could hold a word this final repeats.
        let latest = plus(minus(s.end, delay), tolerance);
        if self.reference_done.is_some_and(|d| d < latest) {
            // The reference words this final could repeat are not decoded yet.
            return false;
        }
        self.open
            .as_ref()
            .is_none_or(|(_, words)| words.first().is_some_and(|w| w.start > latest))
    }

    /// The held mic finals that may be shown and stored now, cleaned, in order. A final
    /// left without words is [`Released::Dropped`] (its partials were shown: the UI clears
    /// them). With `end`, all of them.
    pub fn release(&mut self, end: bool) -> Vec<Released> {
        let mut out = Vec::new();
        while let Some(s) = self.held.front() {
            if !end && !self.releasable(s) {
                break;
            }
            let Some(s) = self.held.pop_front() else {
                break;
            };
            let id = s.id;
            out.push(match self.clean(s) {
                Some(clean) => Released::Kept(clean),
                None => Released::Dropped(id),
            });
        }
        out
    }

    fn clean(&mut self, s: Segment) -> Option<Segment> {
        let Some(delay) = self.delay_for(s.start, s.end) else {
            self.remember(&s.words);
            return Some(s);
        };
        let context = Segment::from_words(
            CONTEXT_ID,
            s.track,
            self.context.iter().cloned().collect(),
            s.start,
        );
        let input = [context, s.clone()];
        let segments: &[Segment] = if input[0].words.is_empty() {
            &input[1..]
        } else {
            &input
        };
        let config = EchoDedupConfig::with_delay(delay);
        let Ok((cleaned, _)) = dedupe_echo_segments(segments, &self.heard(), &config) else {
            self.remember(&s.words);
            return Some(s);
        };
        let kept = cleaned.into_iter().find(|c| c.id == s.id);
        let kept_words = kept.as_ref().map_or(0, |k| k.words.len());
        self.removed += s.words.len() - kept_words;
        if let Some(k) = &kept {
            self.remember(&k.words);
        }
        kept
    }

    fn remember(&mut self, words: &[Word]) {
        for w in words {
            if self.context.len() == CONTEXT_WORDS {
                self.context.pop_front();
            }
            self.context.push_back(w.clone());
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use earshot_core::pipeline::{AecStats, TrackStats};

    fn words(text: &str, start_ms: u64, step_ms: u64) -> Vec<Word> {
        text.split_whitespace()
            .enumerate()
            .map(|(k, t)| {
                let s = start_ms + k as u64 * step_ms;
                Word::new(t, s, s + step_ms - 20, 0.9)
            })
            .collect()
    }

    fn seg(id: u8, w: Vec<Word>) -> Segment {
        Segment::from_words(
            EntityId::from_bytes([id; 16]),
            EntityId::from_bytes([9; 16]),
            w,
            MeetingTime::ZERO,
        )
    }

    fn stats(end_ms: u64, lock_ms: Option<f64>) -> PipelineStats {
        PipelineStats {
            end: MeetingTime::from_millis(end_ms),
            aec: Some(AecStats {
                delay_ms: lock_ms,
                locked: lock_ms.is_some(),
                double_talk: false,
                vetoed: false,
                suppression_gain: 1.0,
                bypass: false,
                converged: lock_ms.is_some(),
                residual_drift_ppm: None,
                attenuation_db: None,
            }),
            mic: TrackStats::default(),
            reference: Some(TrackStats::default()),
            reference_correction_ppm: 0.0,
        }
    }

    fn texts(s: &[Released]) -> Vec<String> {
        s.iter()
            .map(|r| match r {
                Released::Kept(s) => s.text.clone(),
                Released::Dropped(_) => "<dropped>".into(),
            })
            .collect()
    }

    #[test]
    fn live_echo_final_repeating_the_reference_is_cleaned_with_a_lock() {
        let mut f = LiveEchoFilter::new();
        f.stats(&stats(1_000, Some(150.0)));
        let far = words("we should ship it on Friday", 1_000, 300);
        f.reference(&TranscriptEvent::Final(seg(1, far.clone())));
        let mut mic = words("hello there", 0, 250);
        mic.extend(far.iter().map(|w| w.shifted(MeetingTime::from_millis(150))));
        f.mic_final(seg(2, mic));
        f.progress(MeetingTime::from_millis(4_000));
        let out = f.release(false);
        assert_eq!(texts(&out), ["hello there"]);
        assert_eq!(f.removed(), 6);
    }

    #[test]
    fn live_echo_without_a_lock_removes_nothing_and_holds_nothing() {
        // Headphones: no echo path, no lock. A mic final that repeats the far end at an
        // echo-like offset is the user's own speech.
        let mut f = LiveEchoFilter::new();
        f.stats(&stats(1_000, None));
        let far = words("we should ship it on Friday", 1_000, 300);
        // The reference is still open: without a lock that does not hold the final.
        f.reference(&TranscriptEvent::Partial(PartialSegment {
            segment: seg(1, far.clone()),
            committed_words: 0,
        }));
        let mic: Vec<Word> = far
            .iter()
            .map(|w| w.shifted(MeetingTime::from_millis(150)))
            .collect();
        f.mic_final(seg(2, mic.clone()));
        let out = f.release(false);
        assert_eq!(out, vec![Released::Kept(seg(2, mic))]);
        assert_eq!(f.removed(), 0);
    }

    #[test]
    fn live_echo_final_waits_for_an_open_reference_segment() {
        let mut f = LiveEchoFilter::new();
        f.stats(&stats(1_000, Some(150.0)));
        let far = words("please send the report today", 1_000, 300);
        let partial = PartialSegment {
            segment: seg(1, far[..2].to_vec()),
            committed_words: 0,
        };
        f.reference(&TranscriptEvent::Partial(partial));
        let mic: Vec<Word> = far
            .iter()
            .map(|w| w.shifted(MeetingTime::from_millis(150)))
            .collect();
        f.mic_final(seg(2, mic.clone()));
        assert!(
            f.release(false).is_empty(),
            "held while the reference is open"
        );
        f.reference(&TranscriptEvent::Final(seg(1, far)));
        let out = f.release(false);
        assert_eq!(
            out,
            vec![Released::Dropped(EntityId::from_bytes([2; 16]))],
            "all echo: dropped"
        );
        assert_eq!(f.removed(), 5);
    }

    #[test]
    fn live_echo_hold_is_bounded_and_end_releases_everything() {
        let mut f = LiveEchoFilter::new();
        f.stats(&stats(1_000, Some(150.0)));
        // A reference segment opened before the mic final and never closed.
        f.reference(&TranscriptEvent::Partial(PartialSegment {
            segment: seg(1, words("unrelated far talk", 500, 300)),
            committed_words: 0,
        }));
        let mine = words("my own words here", 1_000, 300);
        f.mic_final(seg(2, mine.clone()));
        f.progress(MeetingTime::from_millis(5_000));
        assert!(f.release(false).is_empty());
        // MAX_HOLD after the final's end (2.18 s) it is released, unchanged.
        f.progress(MeetingTime::from_millis(2_180 + 10_000 - 1));
        assert!(f.release(false).is_empty());
        f.progress(MeetingTime::from_millis(2_180 + 10_000));
        assert_eq!(f.release(false), vec![Released::Kept(seg(2, mine.clone()))]);
        // At the end every held final is released.
        f.mic_final(seg(3, mine.clone()));
        assert_eq!(f.release(true).len(), 1);
    }

    #[test]
    fn live_echo_a_run_split_across_finals_is_found() {
        let mut f = LiveEchoFilter::new();
        f.stats(&stats(1_000, Some(200.0)));
        let far = words("see you all next week then", 1_000, 300);
        f.reference(&TranscriptEvent::Final(seg(1, far.clone())));
        let echo: Vec<Word> = far
            .iter()
            .map(|w| w.shifted(MeetingTime::from_millis(200)))
            .collect();
        // Two echo words end one final (too short a run alone, kept: already shown),
        // the rest come in the next final and are removed with the context.
        f.mic_final(seg(2, echo[..2].to_vec()));
        f.progress(MeetingTime::from_millis(20_000));
        assert_eq!(texts(&f.release(false)), ["see you"]);
        f.mic_final(seg(3, echo[2..].to_vec()));
        assert_eq!(texts(&f.release(false)), ["<dropped>"]);
        assert_eq!(f.removed(), 4);
    }

    #[test]
    fn live_echo_partials_hide_repeated_reference_words() {
        let mut f = LiveEchoFilter::new();
        f.stats(&stats(1_000, Some(150.0)));
        let far = words("we should ship it", 1_000, 300);
        f.reference(&TranscriptEvent::Partial(PartialSegment {
            segment: seg(1, far.clone()),
            committed_words: 2,
        }));
        let mut mic = words("okay", 0, 300);
        mic.extend(far.iter().map(|w| w.shifted(MeetingTime::from_millis(150))));
        let shown = f.mic_partial(PartialSegment {
            segment: seg(2, mic.clone()),
            committed_words: 3,
        });
        assert_eq!(shown.segment.text, "okay");
        assert_eq!(shown.committed_words, 1);
        // Without a lock the partial is unchanged.
        let mut g = LiveEchoFilter::new();
        g.stats(&stats(1_000, None));
        let p = PartialSegment {
            segment: seg(2, mic),
            committed_words: 3,
        };
        assert_eq!(g.mic_partial(p.clone()), p);
    }

    #[test]
    fn live_echo_lock_stretches_are_recorded_and_split_on_delay_changes() {
        let mut f = LiveEchoFilter::new();
        f.stats(&stats(1_000, None));
        f.stats(&stats(2_000, Some(120.0)));
        f.stats(&stats(3_000, Some(131.0)));
        f.stats(&stats(4_000, None));
        f.stats(&stats(5_000, Some(300.0)));
        f.stats(&stats(6_000, Some(340.0)));
        let lock = |from_ms, to_ms, delay_ms| EchoLock {
            from_ms,
            to_ms,
            delay_ms,
        };
        assert_eq!(
            f.locks(),
            [
                lock(1_000, 3_000, 120),
                lock(4_000, 5_000, 300),
                lock(5_000, 6_000, 340)
            ]
        );
        assert_eq!(f.delay(), Some(Duration::from_millis(340)));
    }

    #[test]
    fn live_echo_final_released_after_the_lock_dropped_is_cleaned_with_its_stretch() {
        let mut f = LiveEchoFilter::new();
        f.stats(&stats(1_000, Some(150.0)));
        let far = words("please send the report today", 1_000, 300);
        f.reference(&TranscriptEvent::Partial(PartialSegment {
            segment: seg(1, far[..2].to_vec()),
            committed_words: 0,
        }));
        let mic: Vec<Word> = far
            .iter()
            .map(|w| w.shifted(MeetingTime::from_millis(150)))
            .collect();
        f.mic_final(seg(2, mic));
        f.stats(&stats(2_000, Some(150.0)));
        assert!(
            f.release(false).is_empty(),
            "held while the reference is open"
        );
        // The lock goes (headphones plugged in) before the reference final arrives.
        f.stats(&stats(3_000, None));
        assert_eq!(f.delay(), None);
        f.reference(&TranscriptEvent::Final(seg(1, far)));
        assert_eq!(texts(&f.release(false)), ["<dropped>"]);
        assert_eq!(f.removed(), 5);
        // Speech after the lock is untouched.
        let later = words("we should ship it", 9_000, 300);
        f.mic_final(seg(3, later.clone()));
        assert_eq!(f.release(false), vec![Released::Kept(seg(3, later))]);
    }

    /// Capture is minutes ahead of both transcripts during a catch-up (T9.1l): reference words a
    /// coming mic final may repeat are still there.
    #[test]
    fn meeting_queue_echo_cleaning_holds_during_a_catch_up() {
        let mut f = LiveEchoFilter::new();
        f.stats(&stats(1_000, Some(100.0)));
        f.stats(&stats(600_000, Some(100.0)));
        f.mic_progress(MeetingTime::from_millis(10_000));
        f.reference_progress(MeetingTime::from_millis(10_000));
        let spoken = "one two three four five";
        f.reference(&TranscriptEvent::Final(seg(1, words(spoken, 5_000, 400))));
        f.mic_final(seg(2, words(spoken, 5_100, 400)));
        assert_eq!(texts(&f.release(false)), ["<dropped>"]);
    }

    /// TODO 51: a mic worker that stopped (failed or crashed) left its frontier frozen, so
    /// every later reference word was kept for the rest of the meeting.
    #[test]
    fn live_echo_a_stopped_mic_no_longer_keeps_reference_words() {
        let feed = |f: &mut LiveEchoFilter| {
            for k in 0..60u64 {
                let at = 10_000 * k;
                f.progress(MeetingTime::from_millis(at));
                f.reference_progress(MeetingTime::from_millis(at));
                f.reference(&TranscriptEvent::Final(seg(1, words("one two", at, 400))));
            }
        };
        let mut frozen = LiveEchoFilter::new();
        frozen.mic_progress(MeetingTime::ZERO);
        feed(&mut frozen);
        assert_eq!(
            frozen.reference.len(),
            120,
            "every word kept behind the mic"
        );

        let mut stopped = LiveEchoFilter::new();
        stopped.mic_progress(MeetingTime::ZERO);
        stopped.mic_stopped();
        stopped.mic_progress(MeetingTime::ZERO);
        feed(&mut stopped);
        // Kept: the last REFERENCE_KEEP (120 s) of 590 s, in words two per 10 s.
        assert!(
            stopped.reference.len() <= 2 * 13,
            "{}",
            stopped.reference.len()
        );

        // A mic final still held keeps the reference words it may repeat.
        let mut held = LiveEchoFilter::new();
        held.stats(&stats(1_000, Some(100.0)));
        held.mic_final(seg(2, words("one two", 1_100, 400)));
        held.mic_stopped();
        feed(&mut held);
        assert!(held.reference.iter().any(|w| w.start.as_millis() <= 1_000));
    }

    #[test]
    fn live_echo_lock_follows_the_newest_stats() {
        let mut f = LiveEchoFilter::new();
        assert_eq!(f.delay(), None);
        f.stats(&stats(0, Some(120.0)));
        assert_eq!(f.delay(), Some(Duration::from_millis(120)));
        f.stats(&stats(1_000, None));
        assert_eq!(f.delay(), None, "a released lock stops the cleaning");
        let mut no_ref = stats(2_000, None);
        no_ref.aec = None;
        f.stats(&no_ref);
        assert_eq!(f.delay(), None, "no reference, no canceller");
    }
}
