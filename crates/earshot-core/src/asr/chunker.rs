//! VAD-driven streaming chunker: [`StreamingChunker`] turns any [`Transcriber`] and [`Vad`]
//! into a [`StreamingTranscriber`].
//!
//! The approach follows whisper_streaming (Machacek, Dabre, Bojar 2023; see
//! [`super::LocalAgreement`]): the open audio window is re-decoded every
//! `partial_interval`, and words are committed once two consecutive hypotheses agree.
//! Unlike the reference, which trims its buffer at sentence ends, windows here are opened
//! and closed by the VAD (one utterance per window) and cut at `max_window` with an
//! overlap when an utterance runs longer.

use std::mem;
use std::time::Duration;

use super::agreement::LocalAgreement;
use super::nonspeech::drop_non_speech;
use super::transcriber::{
    AsrError, BackendInfo, CancelToken, StreamingTranscriber, TranscribeOptions, Transcriber,
};
use super::types::{MeetingTime, PartialSegment, Segment, TranscriptEvent, Word};
use super::vad::Vad;
use crate::audio::{MonoBuffer, PIPELINE_RATE, Sample};
use crate::identity::EntityId;

/// Upper bound for [`ChunkerConfig::max_window`]: whisper models see at most 30 s.
pub const MAX_WINDOW_LIMIT: Duration = Duration::from_secs(30);

/// Settings for [`StreamingChunker`].
#[derive(Debug, Clone, PartialEq)]
pub struct ChunkerConfig {
    /// Longest audio window ever passed to the transcriber (default 15 s, at most
    /// [`MAX_WINDOW_LIMIT`]).
    pub max_window: Duration,
    /// When an utterance runs past `max_window`, the next window starts this much before
    /// the cut, so a word cut in half is decoded whole in the next window. Words are
    /// assigned to the side of the cut that holds their midpoint relative to the middle
    /// of the overlap. Must be less than half of `max_window`. Default 1 s.
    pub overlap: Duration,
    /// Re-decode the open window after this much new audio (default 1 s, must be > 0).
    pub partial_interval: Duration,
    /// Speech probability at or above which a VAD frame counts as speech (default 0.5, in
    /// `(0, 1]`).
    pub vad_threshold: f32,
    /// Speech must last this long to open an utterance (default 90 ms; at least one frame).
    pub min_speech: Duration,
    /// Silence this long closes an utterance (default 500 ms, must be > 0).
    pub min_silence: Duration,
    /// Audio kept before the first and after the last speech frame (default 200 ms). At
    /// most half of `min_silence`, so consecutive utterances never overlap.
    pub speech_pad: Duration,
    /// Decode options (default [`TranscribeOptions::live`]). The prompt is managed by the
    /// chunker when `prompt_chars > 0`.
    pub options: TranscribeOptions,
    /// Pass the most recent whole words of already final text, at most this many
    /// characters, as the prompt (default 200; 0 disables prompting and leaves
    /// `options.prompt` as given).
    pub prompt_chars: usize,
}

impl Default for ChunkerConfig {
    fn default() -> Self {
        ChunkerConfig {
            max_window: Duration::from_secs(15),
            overlap: Duration::from_secs(1),
            partial_interval: Duration::from_secs(1),
            vad_threshold: 0.5,
            min_speech: Duration::from_millis(90),
            min_silence: Duration::from_millis(500),
            speech_pad: Duration::from_millis(200),
            options: TranscribeOptions::live(),
            prompt_chars: 200,
        }
    }
}

impl ChunkerConfig {
    /// Checks the constraints documented on each field.
    ///
    /// # Errors
    /// [`AsrError::Config`] naming the first violated constraint.
    pub fn validate(&self) -> Result<(), AsrError> {
        let fail = |m: &str| Err(AsrError::Config(m.to_owned()));
        if self.max_window.is_zero() || self.max_window > MAX_WINDOW_LIMIT {
            return fail("max_window must be in (0, 30 s]");
        }
        if self.overlap * 2 >= self.max_window {
            return fail("overlap must be less than half of max_window");
        }
        if self.partial_interval.is_zero() {
            return fail("partial_interval must be > 0");
        }
        if !(self.vad_threshold > 0.0 && self.vad_threshold <= 1.0) {
            return fail("vad_threshold must be in (0, 1]");
        }
        if self.min_silence.is_zero() {
            return fail("min_silence must be > 0");
        }
        if self.speech_pad * 2 > self.min_silence {
            return fail("speech_pad must be at most half of min_silence");
        }
        Ok(())
    }
}

/// Source of segment ids (for example a closure over a clock and an RNG calling
/// [`EntityId::generate`]).
pub type SegmentIds = Box<dyn FnMut() -> EntityId + Send>;

/// Positions are absolute sample indices at [`PIPELINE_RATE`] since the first push.
#[derive(Debug, Clone, Copy)]
enum State {
    /// No open utterance; `speech_frames` consecutive speech frames seen so far.
    Idle { speech_frames: usize },
    /// An utterance is open.
    Active {
        window_start: u64,
        last_speech_end: u64,
        silence: u64,
        last_decode_end: u64,
    },
}

/// The segment currently being built.
#[derive(Debug, Default)]
struct OpenSegment {
    /// Assigned when the first event for the segment is emitted.
    id: Option<EntityId>,
    committed: Vec<Word>,
    /// Texts and committed count of the last partial, to suppress repeats.
    last_partial: Option<(Vec<String>, usize)>,
}

/// Window lengths in samples.
#[derive(Debug, Clone, Copy)]
struct Lengths {
    frame: usize,
    max_window: u64,
    overlap: u64,
    partial_interval: u64,
    min_speech_frames: usize,
    min_silence: u64,
    pad: u64,
}

/// The longest run of whole trailing words of `text` that fits in `limit` characters.
fn last_words_within(text: &str, limit: usize) -> String {
    let chars: Vec<char> = text.chars().collect();
    if chars.len() <= limit {
        return text.trim().to_owned();
    }
    let cut = chars.len() - limit;
    let rest: String = chars[cut..].iter().collect();
    if chars[cut - 1].is_whitespace() {
        return rest.trim().to_owned();
    }
    // The cut fell inside a word: drop that partial word.
    match rest.find(char::is_whitespace) {
        Some(i) => rest[i..].trim().to_owned(),
        None => String::new(),
    }
}

fn samples(d: Duration) -> u64 {
    PIPELINE_RATE.frames_for(d) as u64
}

/// VAD-driven chunker with LocalAgreement-2 stabilisation.
///
/// Feed it audio with [`StreamingTranscriber::push`]; it runs the VAD on fixed frames,
/// opens a window when speech starts (with `speech_pad` of pre-roll), re-decodes the window
/// every `partial_interval` (during the silence that closes an utterance only up to the
/// padded speech end, the range the final decodes, so a partial never shows or commits a
/// word from audio no final covers) and emits [`TranscriptEvent::Partial`]s whose committed prefix
/// only grows, and emits one [`TranscriptEvent::Final`] per segment when the utterance ends
/// (after `min_silence`) or the window reaches `max_window`. Every decoded window is at most
/// `max_window` long. Decodes run synchronously inside `push`/`finish`.
///
/// Output is a pure function of the audio, independent of how it is split into pushes
/// (for a fixed [`set_partial_decodes`](Self::set_partial_decodes) setting; toggling it
/// changes which partials are emitted and, with a real decoder, may change final text
/// slightly, since LocalAgreement then sees fewer hypotheses).
/// After an error other than cancellation the stream state is unspecified; drop it.
pub struct StreamingChunker<T, V> {
    transcriber: T,
    vad: V,
    config: ChunkerConfig,
    len: Lengths,
    track: EntityId,
    origin: MeetingTime,
    ids: SegmentIds,
    cancel: CancelToken,
    agreement: LocalAgreement,
    /// Samples not yet making up a whole VAD frame.
    pending: Vec<Sample>,
    /// Retained audio; `buf[0]` is sample `buf_start`.
    buf: Vec<Sample>,
    buf_start: u64,
    state: State,
    /// End of the last closed utterance window; new windows never start before it.
    prev_window_end: u64,
    segment: OpenSegment,
    /// Most recent final text, at most `prompt_chars` characters.
    context: String,
    /// Partial decodes are run (see [`StreamingChunker::set_partial_decodes`]).
    partials: bool,
    /// The range of the last partial decode: one with the same range (later frames of the
    /// closing silence) would decode the same audio again and is skipped.
    last_partial: Option<(u64, u64)>,
}

impl<T: Transcriber, V: Vad> StreamingChunker<T, V> {
    /// A chunker for `track` whose first pushed sample is at meeting time `origin`.
    ///
    /// # Errors
    /// [`AsrError::Config`] if `config` is invalid, the VAD frame length is 0, or one VAD
    /// frame plus the overlap does not fit in `max_window`.
    pub fn new(
        transcriber: T,
        vad: V,
        config: ChunkerConfig,
        track: EntityId,
        origin: MeetingTime,
        ids: SegmentIds,
    ) -> Result<Self, AsrError> {
        config.validate()?;
        let frame = vad.frame_len();
        let len = Lengths {
            frame,
            max_window: samples(config.max_window),
            overlap: samples(config.overlap),
            partial_interval: samples(config.partial_interval).max(1),
            min_speech_frames: (samples(config.min_speech) as usize)
                .div_ceil(frame.max(1))
                .max(1),
            min_silence: samples(config.min_silence),
            pad: samples(config.speech_pad),
        };
        if frame == 0 || frame as u64 + len.overlap >= len.max_window {
            return Err(AsrError::Config(
                "VAD frame length must be > 0 and frame + overlap < max_window".to_owned(),
            ));
        }
        Ok(StreamingChunker {
            transcriber,
            vad,
            config,
            len,
            track,
            origin,
            ids,
            cancel: CancelToken::new(),
            agreement: LocalAgreement::new(),
            pending: Vec::with_capacity(frame),
            buf: Vec::new(),
            buf_start: 0,
            state: State::Idle { speech_frames: 0 },
            prev_window_end: 0,
            segment: OpenSegment::default(),
            context: String::new(),
            partials: true,
            last_partial: None,
        })
    }

    /// Turns partial decodes on or off (default on); a backpressure valve for a host
    /// whose decoder falls behind. Off, only the decodes that finalise text run (cuts
    /// and closes), so no [`TranscriptEvent::Partial`] is emitted. Every word still
    /// reaches a final (with an exact decoder the finals are unchanged). Skipped partials
    /// are not owed: once re-enabled, the next VAD frame of an open utterance decodes at
    /// once, one partial for everything skipped.
    pub fn set_partial_decodes(&mut self, enabled: bool) {
        self.partials = enabled;
    }

    /// The wrapped transcriber (for example to inspect a fake's calls).
    pub fn transcriber(&self) -> &T {
        &self.transcriber
    }

    fn buf_end(&self) -> u64 {
        self.buf_start + self.buf.len() as u64
    }

    fn time(&self, sample: u64) -> MeetingTime {
        self.origin + MeetingTime::from_pipeline_frames(sample)
    }

    fn check_cancel(&self) -> Result<(), AsrError> {
        if self.cancel.is_cancelled() {
            Err(AsrError::Cancelled)
        } else {
            Ok(())
        }
    }

    /// Marks the stream cancelled when an error is a cancellation.
    fn guard<R>(&self, r: Result<R, AsrError>) -> Result<R, AsrError> {
        if matches!(r, Err(AsrError::Cancelled)) {
            self.cancel.cancel();
        }
        r
    }

    /// Drops retained audio before `from`.
    fn trim_to(&mut self, from: u64) {
        let from = from.clamp(self.buf_start, self.buf_end());
        self.buf.drain(..(from - self.buf_start) as usize);
        self.buf_start = from;
    }

    /// Idle: keep only what a new utterance could need as pre-roll.
    fn trim_idle(&mut self) {
        let keep = self.len.pad + (self.len.min_speech_frames * self.len.frame) as u64;
        let from = self
            .buf_end()
            .saturating_sub(keep)
            .max(self.prev_window_end);
        self.trim_to(from);
    }

    /// Decodes `[start, end)` and returns meeting-time words.
    fn decode(&mut self, start: u64, end: u64) -> Result<Vec<Word>, AsrError> {
        self.check_cancel()?;
        let a = (start - self.buf_start) as usize;
        let b = (end - self.buf_start) as usize;
        let audio = MonoBuffer::new(PIPELINE_RATE, self.buf[a..b].to_vec());
        let mut options = self.config.options.clone();
        if self.config.prompt_chars > 0 {
            options.prompt = (!self.context.is_empty()).then(|| self.context.clone());
        }
        let t = self
            .transcriber
            .transcribe(&audio, &options, &self.cancel)?;
        let offset = self.time(start);
        // Whisper's non-speech annotations are not words (T6.7).
        Ok(drop_non_speech(t.words())
            .iter()
            .map(|w| w.shifted(offset))
            .collect())
    }

    fn segment_id(&mut self) -> EntityId {
        *self.segment.id.get_or_insert_with(&mut self.ids)
    }

    fn emit_partial(&mut self, window_start: u64, events: &mut Vec<TranscriptEvent>) {
        let mut words = self.segment.committed.clone();
        words.extend_from_slice(self.agreement.tentative());
        if words.is_empty() {
            return;
        }
        let key = (
            words.iter().map(|w| w.text.clone()).collect::<Vec<_>>(),
            self.segment.committed.len(),
        );
        if self.segment.last_partial.as_ref() == Some(&key) {
            return;
        }
        self.segment.last_partial = Some(key);
        let id = self.segment_id();
        let fallback = self.time(window_start);
        events.push(TranscriptEvent::Partial(PartialSegment {
            segment: Segment::from_words(id, self.track, words, fallback),
            committed_words: self.segment.committed.len(),
        }));
    }

    /// Closes the open segment with `words` (Final, or Discarded if empty after partials).
    fn finalize(
        &mut self,
        words: Vec<Word>,
        fallback: MeetingTime,
        events: &mut Vec<TranscriptEvent>,
    ) {
        let open = mem::take(&mut self.segment);
        if words.is_empty() {
            if let Some(id) = open.id {
                events.push(TranscriptEvent::Discarded { id });
            }
            return;
        }
        let id = open.id.unwrap_or_else(&mut self.ids);
        let segment = Segment::from_words(id, self.track, words, fallback);
        self.push_context(&segment.text);
        events.push(TranscriptEvent::Final(segment));
    }

    fn push_context(&mut self, text: &str) {
        let limit = self.config.prompt_chars;
        if limit == 0 {
            return;
        }
        if !self.context.is_empty() {
            self.context.push(' ');
        }
        self.context.push_str(text);
        self.context = last_words_within(&self.context, limit);
    }

    /// Re-decodes the open window up to `end` and emits a partial.
    fn partial(
        &mut self,
        window_start: u64,
        end: u64,
        events: &mut Vec<TranscriptEvent>,
    ) -> Result<(), AsrError> {
        let hyp = self.decode(window_start, end)?;
        let tail = self.agreement.accept(&hyp);
        let newly = self.agreement.agree(tail);
        self.segment.committed.extend(newly);
        self.emit_partial(window_start, events);
        Ok(())
    }

    /// The utterance ended at `end`: final decode, Final event, back to idle.
    ///
    /// `end` is clamped to `[window_start, window_start + max_window]`. It can lie before
    /// the window start when the window was cut shortly before the speech ended (the cut
    /// moves the start to `cut - overlap`); then nothing is left to decode and the segment
    /// is finalised from its committed words plus the words the cut carried over as
    /// tentative (they were decoded, only not yet confirmed). The closed window then
    /// extends to the end of those carried words (at most to the audio seen), so the next
    /// window's pre-roll cannot decode the audio under them a second time.
    fn close(
        &mut self,
        window_start: u64,
        end: u64,
        events: &mut Vec<TranscriptEvent>,
    ) -> Result<(), AsrError> {
        let mut end = end.clamp(window_start, window_start + self.len.max_window);
        let tail = if end > window_start {
            let hyp = self.decode(window_start, end)?;
            self.agreement.accept(&hyp)
        } else {
            let carried = self.agreement.tentative().to_vec();
            let origin = self.origin.to_pipeline_frames();
            let carried_end = carried
                .iter()
                .map(|w| w.end.to_pipeline_frames().saturating_sub(origin))
                .max()
                .unwrap_or(0);
            end = end.max(carried_end.min(self.buf_end()));
            carried
        };
        self.agreement.commit(&tail);
        let mut words = mem::take(&mut self.segment.committed);
        words.extend(tail);
        let fallback = self.time(window_start);
        // finalize takes the segment (id, partial state); committed was moved out above.
        self.finalize(words, fallback, events);
        self.agreement.clear_history();
        self.prev_window_end = end;
        self.state = State::Idle { speech_frames: 0 };
        self.trim_idle();
        Ok(())
    }

    /// The window `[window_start, cut)` reached `max_window`: finalise the words before the
    /// middle of the overlap and continue with a window starting `overlap` before the cut.
    fn cut(
        &mut self,
        window_start: u64,
        cut: u64,
        events: &mut Vec<TranscriptEvent>,
    ) -> Result<(), AsrError> {
        let hyp = self.decode(window_start, cut)?;
        let split = self
            .time(cut)
            .saturating_sub_duration(self.config.overlap / 2);
        let tail = self.agreement.accept(&hyp);
        let (head, carry): (Vec<Word>, Vec<Word>) =
            tail.into_iter().partition(|w| w.midpoint() < split);
        self.agreement.commit(&head);
        let mut words = mem::take(&mut self.segment.committed);
        words.extend(head);
        if !words.is_empty() {
            let fallback = self.time(window_start);
            self.finalize(words, fallback, events);
        }
        // Otherwise nothing is stable yet: the segment (and its id) stays open.
        // No time floor at the split: whisper re-times words between contexts, so the
        // carried words are matched by text in the next window (LocalAgreement anchors).
        self.agreement.set_tentative(carry);
        let next_start = cut - self.len.overlap;
        if let State::Active {
            window_start: ws,
            last_decode_end,
            ..
        } = &mut self.state
        {
            *ws = next_start;
            *last_decode_end = cut;
        }
        self.trim_to(next_start);
        Ok(())
    }

    /// Processes one whole VAD frame.
    fn step(
        &mut self,
        frame: &[Sample],
        events: &mut Vec<TranscriptEvent>,
    ) -> Result<(), AsrError> {
        let p = self.vad.speech_probability(frame)?;
        let speech = p >= self.config.vad_threshold;
        let frame_len = frame.len() as u64;
        // Decide whether this frame ends the utterance before deciding to cut: an ending
        // utterance is closed (its window clamped to max_window), never cut first.
        if let State::Active {
            window_start,
            silence,
            ..
        } = self.state
        {
            let ending = !speech && silence + frame_len >= self.len.min_silence;
            if !ending && self.buf_end() + frame_len - window_start > self.len.max_window {
                let cut = self.buf_end();
                self.cut(window_start, cut, events)?;
            }
        }
        self.buf.extend_from_slice(frame);
        let end = self.buf_end();
        match self.state {
            State::Idle { speech_frames } => {
                let n = if speech { speech_frames + 1 } else { 0 };
                if n >= self.len.min_speech_frames {
                    let speech_start = end - n as u64 * frame_len;
                    let ws = speech_start
                        .saturating_sub(self.len.pad)
                        .max(self.buf_start)
                        .max(self.prev_window_end);
                    self.agreement.clear_history();
                    self.segment = OpenSegment::default();
                    self.state = State::Active {
                        window_start: ws,
                        last_speech_end: end,
                        silence: 0,
                        last_decode_end: ws,
                    };
                } else {
                    self.state = State::Idle { speech_frames: n };
                    self.trim_idle();
                }
            }
            State::Active {
                window_start,
                mut last_speech_end,
                mut silence,
                last_decode_end,
            } => {
                if speech {
                    last_speech_end = end;
                    silence = 0;
                } else {
                    silence += frame_len;
                }
                self.state = State::Active {
                    window_start,
                    last_speech_end,
                    silence,
                    last_decode_end,
                };
                if silence >= self.len.min_silence {
                    let close_at = (last_speech_end + self.len.pad).min(end);
                    self.close(window_start, close_at, events)?;
                } else if self.partials && end - last_decode_end >= self.len.partial_interval {
                    if let State::Active {
                        last_decode_end, ..
                    } = &mut self.state
                    {
                        *last_decode_end = end;
                    }
                    // A partial decodes no audio the close would not: during the closing
                    // silence only up to the padded speech end (`close`'s range). Audio
                    // past it belongs to no final; a partial word there would vanish, or be
                    // committed and then decoded again by the next window's pre-roll.
                    // After a cut in that silence the window can start past it: then there
                    // is nothing a partial may show.
                    let to = end.min(last_speech_end + self.len.pad);
                    if to > window_start && self.last_partial != Some((window_start, to)) {
                        self.last_partial = Some((window_start, to));
                        self.partial(window_start, to, events)?;
                    }
                }
            }
        }
        Ok(())
    }

    fn push_inner(&mut self, mut audio: &[Sample]) -> Result<Vec<TranscriptEvent>, AsrError> {
        self.check_cancel()?;
        let mut events = Vec::new();
        while !audio.is_empty() {
            let take = (self.len.frame - self.pending.len()).min(audio.len());
            self.pending.extend_from_slice(&audio[..take]);
            audio = &audio[take..];
            if self.pending.len() == self.len.frame {
                let frame = mem::take(&mut self.pending);
                self.step(&frame, &mut events)?;
                self.pending = frame;
                self.pending.clear();
            }
        }
        Ok(events)
    }

    fn finish_inner(&mut self) -> Result<Vec<TranscriptEvent>, AsrError> {
        self.check_cancel()?;
        let mut events = Vec::new();
        let tail = mem::take(&mut self.pending);
        self.buf.extend_from_slice(&tail);
        if let State::Active { window_start, .. } = self.state {
            let end = self.buf_end();
            let mut ws = window_start;
            if end - ws > self.len.max_window {
                let cut = ws + self.len.max_window;
                self.cut(ws, cut, &mut events)?;
                ws = cut - self.len.overlap;
            }
            self.close(ws, end, &mut events)?;
        }
        self.state = State::Idle { speech_frames: 0 };
        self.vad.reset();
        Ok(events)
    }
}

impl<T: Transcriber, V: Vad> StreamingTranscriber for StreamingChunker<T, V> {
    fn push(&mut self, audio: &[Sample]) -> Result<Vec<TranscriptEvent>, AsrError> {
        let r = self.push_inner(audio);
        self.guard(r)
    }

    fn finish(&mut self) -> Result<Vec<TranscriptEvent>, AsrError> {
        let r = self.finish_inner();
        self.guard(r)
    }

    fn cancel_token(&self) -> CancelToken {
        self.cancel.clone()
    }

    fn backend_info(&self) -> BackendInfo {
        self.transcriber.backend_info()
    }
}
