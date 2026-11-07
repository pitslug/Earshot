//! Transcript data types: meeting-relative time, words, segments and streaming events.

use core::fmt;
use core::ops::Add;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::audio::PIPELINE_RATE;
use crate::identity::{EntityId, SyncClass, SyncableClass};

/// A point in time measured from the start of the meeting, in milliseconds.
///
/// Transcripts, notes and exports anchor to meeting time, never wall-clock time
/// (PLAN.md 3.5). Millisecond resolution is finer than any ASR timestamp we consume
/// (whisper.cpp reports 10 ms steps).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default, Serialize, Deserialize)]
#[serde(transparent)]
pub struct MeetingTime(u64);

impl MeetingTime {
    /// The start of the meeting.
    pub const ZERO: Self = MeetingTime(0);

    /// `ms` milliseconds after the start of the meeting.
    #[must_use]
    pub const fn from_millis(ms: u64) -> Self {
        MeetingTime(ms)
    }

    /// Milliseconds since the start of the meeting.
    #[must_use]
    pub const fn as_millis(self) -> u64 {
        self.0
    }

    /// The time of frame `frames` at [`PIPELINE_RATE`], rounded to the nearest millisecond.
    /// Exact for every `u64` (u128 intermediate; the result is smaller than `frames`), so
    /// untrusted positions cannot overflow.
    #[must_use]
    pub fn from_pipeline_frames(frames: u64) -> Self {
        let hz = u128::from(PIPELINE_RATE.hz());
        let ms = (u128::from(frames) * 1000 + hz / 2) / hz;
        MeetingTime(u64::try_from(ms).unwrap_or(u64::MAX))
    }

    /// The frame index at [`PIPELINE_RATE`] nearest to this time (saturating).
    #[must_use]
    pub fn to_pipeline_frames(self) -> u64 {
        let hz = u128::from(PIPELINE_RATE.hz());
        u64::try_from((u128::from(self.0) * hz + 500) / 1000).unwrap_or(u64::MAX)
    }

    /// `self - earlier`, or zero if `earlier` is later.
    #[must_use]
    pub const fn saturating_sub(self, earlier: MeetingTime) -> Duration {
        Duration::from_millis(self.0.saturating_sub(earlier.0))
    }

    /// `self - d`, clamped at the start of the meeting.
    #[must_use]
    pub fn saturating_sub_duration(self, d: Duration) -> MeetingTime {
        MeetingTime(self.0.saturating_sub(duration_ms(d)))
    }
}

fn duration_ms(d: Duration) -> u64 {
    u64::try_from(d.as_millis()).unwrap_or(u64::MAX)
}

impl Add<Duration> for MeetingTime {
    type Output = MeetingTime;
    fn add(self, rhs: Duration) -> MeetingTime {
        MeetingTime(self.0.saturating_add(duration_ms(rhs)))
    }
}

/// Shifts a time relative to some buffer by that buffer's start time.
impl Add<MeetingTime> for MeetingTime {
    type Output = MeetingTime;
    fn add(self, rhs: MeetingTime) -> MeetingTime {
        MeetingTime(self.0.saturating_add(rhs.0))
    }
}

impl fmt::Debug for MeetingTime {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{:03}s", self.0 / 1000, self.0 % 1000)
    }
}

/// One recognised word with its time span and the recogniser's probability.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Word {
    /// The word as the recogniser produced it, including attached punctuation and without
    /// surrounding whitespace.
    pub text: String,
    /// Start of the word.
    pub start: MeetingTime,
    /// End of the word (`>= start`).
    pub end: MeetingTime,
    /// Recogniser probability in `[0, 1]`.
    pub probability: f32,
}

impl Word {
    /// A word spanning `start_ms..end_ms`.
    #[must_use]
    pub fn new(text: impl Into<String>, start_ms: u64, end_ms: u64, probability: f32) -> Self {
        Word {
            text: text.into(),
            start: MeetingTime::from_millis(start_ms),
            end: MeetingTime::from_millis(end_ms.max(start_ms)),
            probability,
        }
    }

    /// Midpoint of the word's span. Used to assign a word to exactly one side of a chunk
    /// boundary.
    #[must_use]
    pub fn midpoint(&self) -> MeetingTime {
        MeetingTime((self.start.0 + self.end.0) / 2)
    }

    /// The same word shifted later by `offset`.
    #[must_use]
    pub fn shifted(&self, offset: MeetingTime) -> Word {
        Word {
            text: self.text.clone(),
            start: self.start + offset,
            end: self.end + offset,
            probability: self.probability,
        }
    }
}

/// Joins word texts with single spaces.
#[must_use]
pub fn join_words(words: &[Word]) -> String {
    let mut out = String::new();
    for w in words {
        let t = w.text.trim();
        if t.is_empty() {
            continue;
        }
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(t);
    }
    out
}

/// Mean word probability, or 0 for no words.
#[must_use]
pub fn mean_probability(words: &[Word]) -> f32 {
    if words.is_empty() {
        return 0.0;
    }
    let sum: f64 = words.iter().map(|w| f64::from(w.probability)).sum();
    (sum / words.len() as f64) as f32
}

/// A transcript segment of one track (PLAN.md 3.5).
///
/// `Segment` is `Syncable`. Its sync metadata ([`crate::identity::SyncMeta`]) is attached
/// when the segment is persisted (the data model, T5.1); ASR produces the content only.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Segment {
    /// Stable id. A streaming segment keeps the same id from its first partial to its final.
    pub id: EntityId,
    /// The track (mic or loopback) this segment was transcribed from.
    pub track: EntityId,
    /// Start of the segment.
    pub start: MeetingTime,
    /// End of the segment.
    pub end: MeetingTime,
    /// Full text (the words joined by single spaces).
    pub text: String,
    /// Words with timestamps.
    pub words: Vec<Word>,
    /// Mean word probability in `[0, 1]`.
    pub confidence: f32,
    /// Speaker, assigned after diarisation; `None` until then.
    pub speaker_id: Option<EntityId>,
}

impl Segment {
    /// A segment built from words: text, span and confidence are derived from them.
    /// With no words the span is `fallback_start..fallback_start`.
    #[must_use]
    pub fn from_words(
        id: EntityId,
        track: EntityId,
        words: Vec<Word>,
        fallback_start: MeetingTime,
    ) -> Self {
        let start = words.first().map_or(fallback_start, |w| w.start);
        let end = words.last().map_or(start, |w| w.end.max(start));
        Segment {
            id,
            track,
            start,
            end,
            text: join_words(&words),
            confidence: mean_probability(&words),
            words,
            speaker_id: None,
        }
    }
}

impl SyncClass for Segment {
    type Class = SyncableClass;
}

/// A segment that is still being recognised.
///
/// `segment.words[..committed_words]` are stable: later partials and the final keep them
/// unchanged (LocalAgreement, see [`super::StreamingChunker`]). The rest is a tentative
/// hypothesis that may still change.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PartialSegment {
    /// The segment so far (text covers committed and tentative words).
    pub segment: Segment,
    /// Number of leading words that are committed.
    pub committed_words: usize,
}

impl PartialSegment {
    /// The committed (stable) words joined.
    #[must_use]
    pub fn committed_text(&self) -> String {
        join_words(&self.segment.words[..self.committed_words])
    }

    /// The tentative words joined.
    #[must_use]
    pub fn tentative_text(&self) -> String {
        join_words(&self.segment.words[self.committed_words..])
    }
}

/// Output of a [`super::StreamingTranscriber`].
///
/// Every segment id appears in zero or more `Partial` events followed by exactly one
/// `Final` or `Discarded`, unless the stream is cancelled (then open ids are abandoned).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TranscriptEvent {
    /// Updated hypothesis for an open segment.
    Partial(PartialSegment),
    /// The segment is complete and will not change.
    Final(Segment),
    /// A segment that had partials produced no text in the end (for example a cough).
    Discarded {
        /// The id previously used by `Partial` events.
        id: EntityId,
    },
}
