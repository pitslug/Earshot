//! [`FakeTranscriber`]: a deterministic, scriptable [`Transcriber`] for tests.

use std::collections::VecDeque;
use std::time::Duration;

use super::transcriber::check_pipeline_rate;
use super::transcriber::{
    AsrError, BackendInfo, BackendKind, CancelToken, RawSegment, TranscribeOptions, Transcriber,
    Transcription,
};
use super::types::{MeetingTime, Word};
use crate::audio::{MonoBuffer, Sample};

/// One recorded call to a [`FakeTranscriber`].
#[derive(Debug, Clone, PartialEq)]
pub struct FakeCall {
    /// Number of samples transcribed.
    pub samples: usize,
    /// Options passed.
    pub options: TranscribeOptions,
    /// Timeline mode: where the buffer starts in the timeline audio, in samples.
    pub timeline_offset: Option<usize>,
}

#[derive(Debug, Clone)]
enum Mode {
    Scripted(VecDeque<Result<Vec<RawSegment>, AsrError>>),
    Timeline(Timeline),
}

#[derive(Debug, Clone)]
struct Timeline {
    audio: Vec<Sample>,
    words: Vec<Word>,
    unstable_tail: Duration,
    jitter: Duration,
    /// Calls `(offset, samples)` that return no words.
    empty_calls: Vec<(usize, usize)>,
}

/// A fake recogniser.
///
/// - **Scripted** ([`FakeTranscriber::scripted`]): returns the given results in order, one
///   per call; an exhausted script is an [`AsrError::Backend`] error.
/// - **Timeline** ([`FakeTranscriber::timeline`]): holds the whole test stream and its
///   reference words (times from the start of `audio`). Each call finds where the buffer
///   sits in the stream by exact sample match (so `audio` must not repeat, for example
///   seeded noise) and returns the words whose midpoint falls inside the buffer, with
///   buffer-relative times. With [`FakeTranscriber::with_unstable_tail`], words ending
///   within that distance of the buffer end get a call-specific suffix, simulating the
///   unstable last words of a real streaming hypothesis. With
///   [`FakeTranscriber::with_timestamp_jitter`], every word of a call is shifted by the same
///   deterministic per-call offset in `[-jitter, +jitter]` (clamped to the buffer),
///   simulating whisper's word times moving between decoding contexts. With
///   [`FakeTranscriber::with_empty_calls`], a call on exactly one of the given buffers
///   returns no words, as Parakeet TDT 0.6B does on some inputs (T2.5d).
///
/// [`FakeTranscriber::skipping_speech`] makes it report
/// [`Transcriber::may_skip_speech`].
///
/// Both modes check the sample rate and the cancel token first, and record every call.
#[derive(Debug, Clone)]
pub struct FakeTranscriber {
    mode: Mode,
    calls: Vec<FakeCall>,
    skips_speech: bool,
}

impl FakeTranscriber {
    /// Returns `results` in order.
    pub fn scripted(results: impl IntoIterator<Item = Result<Vec<RawSegment>, AsrError>>) -> Self {
        FakeTranscriber {
            mode: Mode::Scripted(results.into_iter().collect()),
            calls: Vec::new(),
            skips_speech: false,
        }
    }

    /// Transcribes buffers cut from `audio` using the reference `words`.
    #[must_use]
    pub fn timeline(audio: Vec<Sample>, words: Vec<Word>) -> Self {
        FakeTranscriber {
            mode: Mode::Timeline(Timeline {
                audio,
                words,
                unstable_tail: Duration::ZERO,
                jitter: Duration::ZERO,
                empty_calls: Vec::new(),
            }),
            calls: Vec::new(),
            skips_speech: false,
        }
    }

    /// Timeline mode: words ending within `tail` of the buffer end are unstable.
    #[must_use]
    pub fn with_unstable_tail(mut self, tail: Duration) -> Self {
        if let Mode::Timeline(t) = &mut self.mode {
            t.unstable_tail = tail;
        }
        self
    }

    /// Timeline mode: shift each call's word times by a deterministic offset in
    /// `[-jitter, +jitter]` (millisecond resolution).
    #[must_use]
    pub fn with_timestamp_jitter(mut self, jitter: Duration) -> Self {
        if let Mode::Timeline(t) = &mut self.mode {
            t.jitter = jitter;
        }
        self
    }

    /// Timeline mode: a call on exactly the buffer at `offset` (samples into the timeline
    /// audio) of `samples` samples, for any of `calls`, returns no words.
    #[must_use]
    pub fn with_empty_calls(mut self, calls: impl IntoIterator<Item = (usize, usize)>) -> Self {
        if let Mode::Timeline(t) = &mut self.mode {
            t.empty_calls.extend(calls);
        }
        self
    }

    /// Reports [`Transcriber::may_skip_speech`].
    #[must_use]
    pub fn skipping_speech(mut self) -> Self {
        self.skips_speech = true;
        self
    }

    /// Calls made so far.
    #[must_use]
    pub fn calls(&self) -> &[FakeCall] {
        &self.calls
    }
}

impl Transcriber for FakeTranscriber {
    fn transcribe(
        &mut self,
        audio: &MonoBuffer,
        options: &TranscribeOptions,
        cancel: &CancelToken,
    ) -> Result<Transcription, AsrError> {
        check_pipeline_rate(audio)?;
        if cancel.is_cancelled() {
            return Err(AsrError::Cancelled);
        }
        let call_index = self.calls.len();
        let mut call = FakeCall {
            samples: audio.len(),
            options: options.clone(),
            timeline_offset: None,
        };
        let result = match &mut self.mode {
            Mode::Scripted(queue) => queue
                .pop_front()
                .unwrap_or_else(|| Err(AsrError::Backend("fake script exhausted".to_owned()))),
            Mode::Timeline(t) => match locate(&t.audio, &audio.samples) {
                Some(offset) => {
                    call.timeline_offset = Some(offset);
                    if t.empty_calls.contains(&(offset, audio.len())) {
                        Ok(Vec::new())
                    } else {
                        Ok(timeline_words(t, offset, audio.len(), call_index))
                    }
                }
                None => Err(AsrError::Backend(
                    "buffer not found in the fake's timeline audio".to_owned(),
                )),
            },
        };
        self.calls.push(call);
        let segments = result?;
        Ok(Transcription {
            segments,
            language: Some("en".to_owned()),
            backend: self.backend_info(),
        })
    }

    fn may_skip_speech(&self) -> bool {
        self.skips_speech
    }

    fn backend_info(&self) -> BackendInfo {
        BackendInfo {
            kind: BackendKind::Fake,
            device: "fake".to_owned(),
            model: "fake".to_owned(),
        }
    }
}

/// Start of `needle` inside `haystack` by exact match of its first samples (the whole of
/// `needle` is then verified).
fn locate(haystack: &[Sample], needle: &[Sample]) -> Option<usize> {
    if needle.is_empty() || needle.len() > haystack.len() {
        return None;
    }
    let probe = &needle[..needle.len().min(64)];
    (0..=haystack.len() - needle.len()).find(|&i| {
        haystack[i..i + probe.len()] == *probe && haystack[i..i + needle.len()] == *needle
    })
}

/// Words with their midpoint inside the buffer, buffer-relative and clamped to it, one
/// [`RawSegment`] per word. Words ending within `unstable_tail` of the buffer end get a
/// suffix unique to this call. Times are then shifted by the call's jitter offset.
fn timeline_words(t: &Timeline, offset: usize, len: usize, call_index: usize) -> Vec<RawSegment> {
    let start = MeetingTime::from_pipeline_frames(offset as u64);
    let end = MeetingTime::from_pipeline_frames((offset + len) as u64);
    let len_ms = end.as_millis() - start.as_millis();
    let unstable_from = end.saturating_sub_duration(t.unstable_tail);
    let shift = jitter_ms(t.jitter, call_index);
    t.words
        .iter()
        .filter(|w| w.midpoint() >= start && w.midpoint() < end)
        .map(|w| {
            let rel = |x: MeetingTime| {
                let r = x.min(end).as_millis().saturating_sub(start.as_millis());
                let r = i64::try_from(r).unwrap_or(i64::MAX).saturating_add(shift);
                MeetingTime::from_millis(
                    r.clamp(0, i64::try_from(len_ms).unwrap_or(i64::MAX)) as u64
                )
            };
            let mut text = w.text.clone();
            if !t.unstable_tail.is_zero() && w.end > unstable_from {
                text = format!("{text}~x{call_index}");
            }
            RawSegment::from_words(vec![Word {
                text,
                start: rel(w.start),
                end: rel(w.end),
                probability: w.probability,
            }])
        })
        .collect()
}

/// Deterministic offset in `[-jitter, +jitter]` ms for call `call_index` (splitmix64).
fn jitter_ms(jitter: Duration, call_index: usize) -> i64 {
    let j = i64::try_from(jitter.as_millis()).unwrap_or(i64::MAX / 4);
    if j == 0 {
        return 0;
    }
    let mut z = (call_index as u64).wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^= z >> 31;
    (z % (2 * j as u64 + 1)) as i64 - j
}
