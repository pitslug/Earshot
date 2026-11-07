//! Batch decoding in speech pieces (T6.16): the final pass without repetition loops.
//!
//! One whisper call over a whole track lets whisper.cpp condition every 30 s window on the
//! text of the windows before it (`prompt_past1` in `whisper_full_with_state`, whisper.cpp
//! src/whisper.cpp; `no_context` only clears text left from earlier calls). Across quiet
//! stretches of echo residual that conditioning can lock the decoder into repeating one
//! sentence for minutes (seen on a private recording), and
//! whisper.cpp's entropy-based fallback did not catch it. [`transcribe_in_pieces`] instead:
//!
//! 1. finds speech with a [`Vad`]: a frame starts speech at `threshold` and keeps it above
//!    `threshold - 0.15`; speech ends after `min_silence` below; regions shorter than
//!    `min_speech` are dropped; each region is padded by `speech_pad` (Silero
//!    `get_speech_timestamps` with faster-whisper's `VadOptions` defaults, SYSTRAN/
//!    faster-whisper `vad.py`: threshold 0.5, min_silence_duration_ms 2000, speech_pad_ms
//!    400). Silence is never decoded.
//! 2. plans contiguous pieces of at most `max_piece` (Whisper's 30 s window, so each piece
//!    is one window): regions closer than `merge_gap` share a piece; a longer region is cut
//!    at its least speech-like frame in the second half of the window.
//! 3. transcribes each piece with its own call; the previous piece's text (at most
//!    `prompt_chars`) is the prompt only when that piece was accepted and the pause before
//!    this one is at most `context_gap`; the caller's prompt goes to the first piece only.
//! 4. checks each piece with openai/whisper's compression ratio (`transcribe.py`
//!    `compression_ratio_threshold` 2.4: UTF-8 text length over its zlib-compressed length).
//!    A piece above it is decoded again without a prompt, then with greedy decoding without
//!    a prompt; if it still repeats it is dropped and reported, never stored.
//! 5. for a transcriber that [may skip speech](Transcriber::may_skip_speech) (Parakeet TDT,
//!    whose greedy decoder can stay on blank for the rest of a piece, T2.5d), finds the
//!    stretches between the accepted words (and before the first and after the last) that
//!    hold more than `recover_speech` of VAD speech, and decodes each again without a
//!    prompt: on its own, or, when it is the whole piece (the same input gives the same
//!    answer), as two halves cut at the least speech-like frame of its middle half; then
//!    the same for each part's own uncovered speech, at most [`RECOVER_DEPTH`] levels deep
//!    and never into parts under [`RECOVER_MIN_PART`]. Worst case, when every decode gives
//!    nothing: about 7 calls per uncovered stretch (1, then 2 halves, then 4 quarters; 14
//!    when the stretch is the whole piece, which is halved at once), and since each level
//!    decodes disjoint parts of the level above, at most 3 times the piece's audio again,
//!    so up to about 4 times the piece's audio in all with its first decode. Parts whose text repeats itself
//!    (step 4's ratio) are ignored. The words found come back as their own segments in
//!    time order; a segment they fall inside is split around them. `recover_speech` (2 s)
//!    was fixed on the LibriSpeech dev-clean subset streams with parakeet-tdt-0.6b-v2 and
//!    the 110M TDT branch: no stretch between correctly decoded words held more than
//!    0.64 s of speech, while the failures left 2.5 s, 17 s and 21 s. Reproduced for
//!    parakeet-tdt-0.6b-v2 on the CPU by earshot-runtime's `skip_gaps` example (raw output
//!    `docs/skip-gaps/parakeet-tdt-0.6b-v2-cpu.json`, 2026-11-01: dev-clean largest stretch
//!    at most 2 s 0.64 s, over 2 s 17.31, 2.50 and 20.83 s; test-clean 0.35 s and 23.36 s).
//!
//! This is not in Whisper or faster-whisper (no reference); whisper transcribers do not
//! report skipping speech, so step 5 never runs for them. Expected effect on WER: it
//! removes the skipped stretches' deletions (on the test-clean streams one such piece held
//! 78 of the 0.6B v2 model's 153 errors) and, as it runs only where more than 2 s of speech
//! has no words, nothing else.
//!
//! Deviations from the references, and their expected effect on WER: `min_speech` is
//! Silero's 250 ms rather than faster-whisper's 0 (blips padded to 0.8 s invite short
//! hallucinations such as "Thank you."; expected neutral); pieces keep the audio between
//! merged regions instead of faster-whisper's concatenation of speech only (simpler times;
//! at most `merge_gap` of silence per gap); zlib is miniz_oxide at level 6 rather than
//! Python's zlib (sizes differ by a few bytes, so ratios near 2.4 may decide differently);
//! the openai/whisper rule retries at higher temperatures and finally keeps a repetitive
//! result, here the decoder's own temperature fallback runs inside each call and a piece
//! that still repeats after two more decodes is dropped (a few seconds of words lost in
//! exchange for no invented text).

use std::time::Duration;

use serde::{Deserialize, Serialize};

use super::transcriber::{
    AsrError, CancelToken, DecodingOptions, DecodingStrategy, Language, RawSegment,
    TranscribeOptions, Transcriber, Transcription, check_pipeline_rate,
};
use super::types::{MeetingTime, Word};
use super::vad::Vad;
use crate::audio::{MonoBuffer, PIPELINE_RATE};

/// Settings for [`transcribe_in_pieces`]; see the module docs for the sources.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PieceConfig {
    /// Speech probability that starts speech.
    pub threshold: f32,
    /// Shortest speech region kept.
    pub min_speech: Duration,
    /// Silence that ends a speech region.
    pub min_silence: Duration,
    /// Padding added before and after each region.
    pub speech_pad: Duration,
    /// Longest piece (one whisper window).
    pub max_piece: Duration,
    /// Regions separated by at most this much silence share a piece.
    pub merge_gap: Duration,
    /// The previous piece's text is the prompt only across a pause up to this long.
    pub context_gap: Duration,
    /// At most this many characters of the previous piece's text, from its end.
    pub prompt_chars: usize,
    /// A piece whose text compresses better than this is repetitive.
    pub compression_ratio_threshold: f32,
    /// Speech that no word covers, more than this inside one stretch between words, is
    /// decoded again (module docs, step 5); only for a transcriber that
    /// [may skip speech](Transcriber::may_skip_speech). `None` never decodes again. Records
    /// written before this setting existed read as `None`.
    #[serde(default)]
    pub recover_speech: Option<Duration>,
}

impl Default for PieceConfig {
    fn default() -> Self {
        PieceConfig {
            threshold: 0.5,
            min_speech: Duration::from_millis(250),
            min_silence: Duration::from_millis(2000),
            speech_pad: Duration::from_millis(400),
            max_piece: Duration::from_secs(30),
            merge_gap: Duration::from_millis(1000),
            context_gap: Duration::from_secs(10),
            prompt_chars: 200,
            compression_ratio_threshold: 2.4,
            recover_speech: Some(Duration::from_secs(2)),
        }
    }
}

impl PieceConfig {
    /// Checks the settings.
    ///
    /// # Errors
    /// [`AsrError::Config`] naming the first setting out of range.
    pub fn validate(&self) -> Result<(), AsrError> {
        let fail = |m: &str| Err(AsrError::Config(format!("piece config: {m}")));
        if !(self.threshold > 0.15 && self.threshold <= 1.0) {
            return fail("threshold must be in (0.15, 1]");
        }
        if self.max_piece < Duration::from_secs(1) || self.max_piece > Duration::from_secs(30) {
            return fail("max_piece must be 1-30 s (one whisper window)");
        }
        if self.min_silence.is_zero() {
            return fail("min_silence must be positive");
        }
        if !(self.compression_ratio_threshold.is_finite() && self.compression_ratio_threshold > 1.0)
        {
            return fail("compression_ratio_threshold must be finite and above 1");
        }
        if self.recover_speech.is_some_and(|d| d.is_zero()) {
            return fail("recover_speech must be positive");
        }
        Ok(())
    }
}

/// A sample range of the buffer, `start..end`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Span {
    /// First sample.
    pub start: usize,
    /// One past the last sample.
    pub end: usize,
}

impl Span {
    /// Number of samples.
    #[must_use]
    pub fn len(self) -> usize {
        self.end - self.start
    }

    /// Whether the span has no samples.
    #[must_use]
    pub fn is_empty(self) -> bool {
        self.end <= self.start
    }
}

/// What [`transcribe_in_pieces`] did.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PieceReport {
    /// Pieces transcribed.
    pub pieces: usize,
    /// Pieces decoded again because their first result was repetitive.
    pub retried: usize,
    /// Pieces whose every result was repetitive: nothing was kept for them.
    pub dropped: Vec<(MeetingTime, MeetingTime)>,
    /// Pieces whose words left speech uncovered, decoded again in parts (step 5).
    pub uncovered: usize,
}

pub(super) fn samples(d: Duration) -> usize {
    let n = d.as_secs_f64() * f64::from(PIPELINE_RATE.hz());
    // Durations here are seconds to minutes; the cast cannot lose a meaningful amount.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let n = n.round() as usize;
    n
}

fn at(sample: usize) -> MeetingTime {
    MeetingTime::from_pipeline_frames(u64::try_from(sample).unwrap_or(u64::MAX))
}

/// Speech probability per frame of `vad` over `audio` (a final partial frame is zero
/// padded).
///
/// # Errors
/// As [`Vad::speech_probability`].
pub fn frame_probabilities<V: Vad + ?Sized>(
    vad: &mut V,
    audio: &[f32],
    cancel: &CancelToken,
) -> Result<Vec<f32>, AsrError> {
    let n = vad.frame_len();
    if n == 0 {
        return Err(AsrError::Config("VAD frame length is zero".into()));
    }
    vad.reset();
    let mut out = Vec::with_capacity(audio.len().div_ceil(n));
    let mut padded = vec![0.0; n];
    for chunk in audio.chunks(n) {
        if cancel.is_cancelled() {
            return Err(AsrError::Cancelled);
        }
        let p = if chunk.len() == n {
            vad.speech_probability(chunk)?
        } else {
            padded[..chunk.len()].copy_from_slice(chunk);
            padded[chunk.len()..].fill(0.0);
            vad.speech_probability(&padded)?
        };
        out.push(p);
    }
    Ok(out)
}

/// Unpadded speech regions closed by `min_silence` of silence, and the region still open at
/// the end of `probs` (its start and, when silence has begun, where the silence began).
pub(super) struct RawRegions {
    pub(super) closed: Vec<Span>,
    pub(super) open: Option<(usize, Option<usize>)>,
}

/// The speech state machine of [`speech_regions`] (module docs, step 1) over `probs`.
pub(super) fn raw_regions(probs: &[f32], frame: usize, config: &PieceConfig) -> RawRegions {
    let neg = config.threshold - 0.15;
    let min_silence = samples(config.min_silence);
    let mut raw: Vec<Span> = Vec::new();
    let mut start: Option<usize> = None;
    let mut silence_from: Option<usize> = None;
    for (i, &p) in probs.iter().enumerate() {
        let pos = i * frame;
        match start {
            None if p >= config.threshold => {
                start = Some(pos);
                silence_from = None;
            }
            None => {}
            Some(s) => {
                if p >= neg {
                    silence_from = None;
                } else {
                    let from = *silence_from.get_or_insert(pos);
                    if pos + frame - from >= min_silence {
                        raw.push(Span {
                            start: s,
                            end: from,
                        });
                        start = None;
                        silence_from = None;
                    }
                }
            }
        }
    }
    RawRegions {
        closed: raw,
        open: start.map(|s| (s, silence_from)),
    }
}

/// Padded speech regions of a buffer of `len` samples from per-frame probabilities
/// (`frame` samples each), in order and non-overlapping.
#[must_use]
pub fn speech_regions(probs: &[f32], frame: usize, len: usize, config: &PieceConfig) -> Vec<Span> {
    let RawRegions {
        closed: mut raw,
        open,
    } = raw_regions(probs, frame, config);
    if let Some((s, silence_from)) = open {
        raw.push(Span {
            start: s,
            end: silence_from.unwrap_or(len).min(len),
        });
    }
    pad_and_merge(raw, len, config)
}

/// Step 1's padding: drops regions shorter than `min_speech`, pads the rest by `speech_pad`
/// (clipped to `len`) and merges overlaps.
pub(super) fn pad_and_merge(raw: Vec<Span>, len: usize, config: &PieceConfig) -> Vec<Span> {
    let min_speech = samples(config.min_speech);
    let pad = samples(config.speech_pad);
    let mut out: Vec<Span> = Vec::new();
    for r in raw.into_iter().filter(|r| r.len() >= min_speech) {
        let padded = Span {
            start: r.start.saturating_sub(pad),
            end: (r.end + pad).min(len),
        };
        match out.last_mut() {
            Some(last) if padded.start <= last.end => last.end = last.end.max(padded.end),
            _ => out.push(padded),
        }
    }
    out
}

/// Contiguous pieces of at most `max_piece` covering `regions` (see the module docs).
#[must_use]
pub fn plan_pieces(
    regions: &[Span],
    probs: &[f32],
    frame: usize,
    config: &PieceConfig,
) -> Vec<Span> {
    let max = samples(config.max_piece);
    let merge_gap = samples(config.merge_gap);
    // Split regions longer than one window at their least speech-like frame in the second
    // half of the window.
    let mut parts: Vec<Span> = Vec::new();
    for r in regions {
        let mut s = r.start;
        while r.end - s > max {
            let cut = cut_point(s, probs, frame, max);
            parts.push(Span { start: s, end: cut });
            s = cut;
        }
        parts.push(Span {
            start: s,
            end: r.end,
        });
    }
    merge_parts(&parts, merge_gap, max)
}

/// Where a region longer than `max` samples starting at `s` is cut: its least speech-like
/// frame in the second half of the window (frames `(s + max/2)/frame ..< (s + max)/frame`).
pub(super) fn cut_point(s: usize, probs: &[f32], frame: usize, max: usize) -> usize {
    let lo = (s + max / 2).div_ceil(frame);
    let hi = (s + max) / frame;
    (lo..hi)
        .min_by(|&a, &b| {
            let pa = probs.get(a).copied().unwrap_or(0.0);
            let pb = probs.get(b).copied().unwrap_or(0.0);
            pa.total_cmp(&pb)
        })
        .map_or(s + max, |f| f * frame)
        .clamp(s + 1, s + max)
}

/// Step 2's merging: consecutive parts closer than `merge_gap` share a piece of at most `max`.
pub(super) fn merge_parts(parts: &[Span], merge_gap: usize, max: usize) -> Vec<Span> {
    let mut out: Vec<Span> = Vec::new();
    for &p in parts {
        match out.last_mut() {
            Some(last) if p.start - last.end <= merge_gap && p.end - last.start <= max => {
                last.end = p.end;
            }
            _ => out.push(p),
        }
    }
    out
}

/// openai/whisper's compression ratio of `text`: UTF-8 length over zlib-compressed length
/// (`transcribe.py` `compression_ratio`). Empty text is 0.
#[must_use]
pub fn compression_ratio(text: &str) -> f32 {
    let bytes = text.as_bytes();
    if bytes.is_empty() {
        return 0.0;
    }
    let compressed = miniz_oxide::deflate::compress_to_vec_zlib(bytes, 6);
    // Lengths of one window's text; f32 precision is ample for a ratio.
    #[allow(clippy::cast_precision_loss)]
    let ratio = bytes.len() as f32 / compressed.len().max(1) as f32;
    ratio
}

fn text_of(t: &Transcription) -> String {
    segments_text(&t.segments)
}

/// The text of `segments` as the compression-ratio guard and the prompt see it.
fn segments_text(segments: &[RawSegment]) -> String {
    segments
        .iter()
        .map(|s| s.text.trim())
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

fn shifted(mut seg: RawSegment, offset: MeetingTime) -> RawSegment {
    seg.start = seg.start + offset;
    seg.end = seg.end + offset;
    seg.words = seg.words.iter().map(|w| w.shifted(offset)).collect();
    seg
}

fn tail_chars(text: &str, n: usize) -> String {
    let count = text.chars().count();
    text.chars().skip(count.saturating_sub(n)).collect()
}

/// Transcribes `audio` piece by piece (see the module docs); segment times are relative to
/// the start of `audio`.
///
/// # Errors
/// [`AsrError::Config`] for an invalid config; VAD and transcriber errors; cancellation.
pub fn transcribe_in_pieces<T, V>(
    transcriber: &mut T,
    vad: &mut V,
    audio: &MonoBuffer,
    options: &TranscribeOptions,
    config: &PieceConfig,
    cancel: &CancelToken,
) -> Result<(Transcription, PieceReport), AsrError>
where
    T: Transcriber + ?Sized,
    V: Vad + ?Sized,
{
    config.validate()?;
    check_pipeline_rate(audio)?;
    let frame = vad.frame_len();
    let probs = frame_probabilities(vad, &audio.samples, cancel)?;
    let regions = speech_regions(&probs, frame, audio.samples.len(), config);
    let pieces = plan_pieces(&regions, &probs, frame, config);

    let mut decoder = PieceDecoder::new(options, config, transcriber.backend_info());
    let mut segments = Vec::new();
    let speech = Speech {
        probs: &probs,
        frame,
    };
    for &piece in &pieces {
        segments.extend(decoder.decode(
            transcriber,
            &audio.samples[piece.start..piece.end],
            piece,
            speech,
            cancel,
        )?);
    }
    Ok(decoder.finish(segments))
}

/// Step 5's limits (module docs): at most this many levels of decoding again below a
/// piece, and no part shorter than [`RECOVER_MIN_PART`]. They change what a speech stretch
/// decodes and are not part of [`PieceConfig`], so a change here bumps the stretch record
/// version (earshot-runtime `stretches::STRETCH_RECORD_VERSION`).
pub(super) const RECOVER_DEPTH: usize = 3;
/// The shortest part step 5 cuts uncovered speech into.
pub(super) const RECOVER_MIN_PART: Duration = Duration::from_secs(1);

/// The VAD's view of a stream: per-frame speech probabilities from its first sample, and
/// the frame length in samples.
#[derive(Debug, Clone, Copy)]
pub(super) struct Speech<'a> {
    pub(super) probs: &'a [f32],
    pub(super) frame: usize,
}

impl Speech<'_> {
    fn prob(self, k: usize) -> f32 {
        self.probs.get(k).copied().unwrap_or(0.0)
    }

    /// Samples of speech in `span`: frames at or above `threshold` whose start lies in it.
    fn amount(self, span: Span, threshold: f32) -> usize {
        (span.start.div_ceil(self.frame)..span.end.div_ceil(self.frame))
            .filter(|&k| self.prob(k) >= threshold)
            .count()
            * self.frame
    }

    /// Where `span` is cut in two: at the start of its least speech-like frame in the
    /// middle half, the one nearest the middle on a tie, so that neither part is shorter
    /// than `min`; `None` when no such frame exists.
    fn cut(self, span: Span, min: usize) -> Option<usize> {
        let margin = (span.len() / 4).max(min);
        if span.len() < 2 * margin {
            return None;
        }
        let lo = (span.start + margin).div_ceil(self.frame);
        let hi = (span.end - margin) / self.frame;
        let mid = (span.start + span.end) / 2;
        (lo..=hi).map(|k| k * self.frame).min_by(|&a, &b| {
            self.prob(a / self.frame)
                .total_cmp(&self.prob(b / self.frame))
                .then(a.abs_diff(mid).cmp(&b.abs_diff(mid)))
        })
    }
}

/// Stretches of `span` (stream samples) that no word of `segments` (stream time) covers
/// and that hold more than `min` samples of speech, in order.
fn uncovered_speech(
    segments: &[RawSegment],
    span: Span,
    speech: Speech<'_>,
    threshold: f32,
    min: usize,
) -> Vec<Span> {
    word_gaps(segments, span)
        .into_iter()
        .filter(|g| speech.amount(*g, threshold) > min)
        .collect()
}

/// Every stretch of `span` (stream samples) that no timed word of `segments` (stream time)
/// covers, with its samples of VAD speech by step 5's rule: frames of `probs` (`frame`
/// samples each, from the stream's first sample) at or above `threshold` whose start lies
/// in the stretch. Step 5 decodes again the stretches holding more than
/// [`PieceConfig::recover_speech`]; this is the measurement that setting was fixed on
/// (earshot-runtime `skip_gaps` example).
#[must_use]
pub fn speech_between_words(
    segments: &[RawSegment],
    span: Span,
    probs: &[f32],
    frame: usize,
    threshold: f32,
) -> Vec<(Span, usize)> {
    if frame == 0 {
        return Vec::new();
    }
    let speech = Speech { probs, frame };
    word_gaps(segments, span)
        .into_iter()
        .map(|g| (g, speech.amount(g, threshold)))
        .collect()
}

/// The non-empty stretches of `span` that no timed word of `segments` covers, in order.
fn word_gaps(segments: &[RawSegment], span: Span) -> Vec<Span> {
    let sample = |t: MeetingTime| usize::try_from(t.to_pipeline_frames()).unwrap_or(usize::MAX);
    let mut words: Vec<(usize, usize)> = segments
        .iter()
        .flat_map(RawSegment::timed_words)
        .map(|w| (sample(w.start), sample(w.end)))
        .collect();
    words.sort_unstable();
    let mut gaps = Vec::new();
    let mut from = span.start;
    for (start, end) in words {
        if start > from {
            gaps.push(Span {
                start: from,
                end: start.min(span.end),
            });
        }
        from = from.max(end);
    }
    if span.end > from {
        gaps.push(Span {
            start: from,
            end: span.end,
        });
    }
    gaps.into_iter().filter(|g| !g.is_empty()).collect()
}

/// `segments` (one piece's, in order) with `extra` (decoded from stretches between their
/// words) in time order: a segment with an extra segment starting between two of its words
/// is split there (each part's text from its words), so no segment overlaps another.
fn merge_recovered(segments: Vec<RawSegment>, mut extra: Vec<RawSegment>) -> Vec<RawSegment> {
    if extra.is_empty() {
        return segments;
    }
    let starts: Vec<MeetingTime> = extra.iter().map(|s| s.start).collect();
    let between = |a: &Word, b: &Word| {
        starts
            .iter()
            .any(|&t| t >= a.end.min(b.start) && t < b.start)
    };
    let mut out = Vec::with_capacity(segments.len() + extra.len());
    for seg in segments {
        if !seg.words.windows(2).any(|w| between(&w[0], &w[1])) {
            out.push(seg);
            continue;
        }
        let mut part: Vec<Word> = Vec::new();
        for w in seg.words {
            if part.last().is_some_and(|last| between(last, &w)) {
                out.push(RawSegment::from_words(std::mem::take(&mut part)));
            }
            part.push(w);
        }
        out.push(RawSegment::from_words(part));
    }
    out.append(&mut extra);
    out.sort_by_key(|s| s.start);
    out
}

/// What step 5 decodes from: the piece's audio and span, the stream's speech, and the
/// amount of uncovered speech that is decoded again.
struct Recover<'a> {
    samples: &'a [f32],
    piece: Span,
    speech: Speech<'a>,
    min: usize,
}

/// Steps 3 to 5 for one piece at a time (module docs): the prompt rule, the retry ladder,
/// the compression-ratio guard, decoding uncovered speech again and the time shift. Shared
/// by [`transcribe_in_pieces`] and [`super::PieceStream`], so both decode alike.
pub(super) struct PieceDecoder {
    options: TranscribeOptions,
    greedy: TranscribeOptions,
    context_gap: usize,
    prompt_chars: usize,
    threshold: f32,
    /// Step 5: speech is a VAD probability at or above `vad_threshold`; more uncovered
    /// speech than `recover` samples is decoded again.
    vad_threshold: f32,
    recover: Option<usize>,
    /// Pieces decoded so far (the caller's prompt goes to the first only).
    count: usize,
    previous: Option<(Span, String)>,
    report: PieceReport,
    language: Option<String>,
    backend: super::transcriber::BackendInfo,
}

impl PieceDecoder {
    pub(super) fn new(
        options: &TranscribeOptions,
        config: &PieceConfig,
        backend: super::transcriber::BackendInfo,
    ) -> Self {
        Self {
            options: options.clone(),
            greedy: TranscribeOptions {
                decoding: DecodingOptions {
                    strategy: DecodingStrategy::Greedy,
                    ..options.decoding
                },
                prompt: None,
                ..options.clone()
            },
            context_gap: samples(config.context_gap),
            prompt_chars: config.prompt_chars,
            threshold: config.compression_ratio_threshold,
            vad_threshold: config.threshold,
            recover: config.recover_speech.map(samples),
            count: 0,
            previous: None,
            report: PieceReport::default(),
            language: None,
            backend,
        }
    }

    /// A decoder that continues after `done`, decoded earlier with the same options and
    /// config: the prompt rule, the first-piece rule, the language and the report carry on
    /// as if this decoder had decoded them ([`super::transcribe_rest_in_pieces`]).
    pub(super) fn resume(
        options: &TranscribeOptions,
        config: &PieceConfig,
        backend: super::transcriber::BackendInfo,
        done: &[super::StreamPiece],
    ) -> Self {
        let mut d = Self::new(options, config, backend);
        d.count = done.len();
        for p in done {
            d.report.pieces += 1;
            if p.retried {
                d.report.retried += 1;
            }
            if p.uncovered {
                d.report.uncovered += 1;
            }
            if p.dropped {
                d.report.dropped.push((at(p.span.start), at(p.span.end)));
                d.previous = None;
            } else {
                d.previous = Some((p.span, segments_text(&p.segments)));
            }
            if d.language.is_none() {
                d.language.clone_from(&p.language);
            }
        }
        d
    }

    /// Decodes `samples`, the audio of `piece` (sample positions from the start of the
    /// stream, as in `speech`); the segments come back shifted to stream time, nothing for
    /// a dropped piece.
    pub(super) fn decode<T: Transcriber + ?Sized>(
        &mut self,
        transcriber: &mut T,
        samples: &[f32],
        piece: Span,
        speech: Speech<'_>,
        cancel: &CancelToken,
    ) -> Result<Vec<RawSegment>, AsrError> {
        if cancel.is_cancelled() {
            return Err(AsrError::Cancelled);
        }
        let options = &self.options;
        let buffer = MonoBuffer::new(PIPELINE_RATE, samples.to_vec());
        let prompt = match &self.previous {
            Some((prev, text)) if piece.start - prev.end <= self.context_gap => {
                Some(tail_chars(text, self.prompt_chars))
            }
            _ if self.count == 0 => options.prompt.clone(),
            _ => None,
        };
        self.count += 1;
        let first = TranscribeOptions {
            prompt: prompt.clone(),
            ..options.clone()
        };
        let mut attempts: Vec<TranscribeOptions> = vec![first];
        if prompt.is_some() {
            attempts.push(TranscribeOptions {
                prompt: None,
                ..options.clone()
            });
        }
        if options.decoding.strategy != DecodingStrategy::Greedy {
            attempts.push(self.greedy.clone());
        }
        self.report.pieces += 1;
        let mut accepted = None;
        for (k, opts) in attempts.iter().enumerate() {
            let t = transcriber.transcribe(&buffer, opts, cancel)?;
            self.backend = t.backend.clone();
            if k == 1 {
                self.report.retried += 1;
            }
            let text = text_of(&t);
            if compression_ratio(&text) <= self.threshold {
                accepted = Some((t, text));
                break;
            }
        }
        let offset = at(piece.start);
        Ok(match accepted {
            Some((t, _)) => {
                if self.language.is_none() {
                    self.language = t.language.clone();
                }
                let mut segments: Vec<RawSegment> =
                    t.segments.into_iter().map(|s| shifted(s, offset)).collect();
                if let Some(min) = self.recover.filter(|_| transcriber.may_skip_speech()) {
                    let gaps = uncovered_speech(&segments, piece, speech, self.vad_threshold, min);
                    if !gaps.is_empty() {
                        self.report.uncovered += 1;
                        let ctx = Recover {
                            samples,
                            piece,
                            speech,
                            min,
                        };
                        let mut extra = Vec::new();
                        self.recover_gaps(transcriber, &ctx, piece, gaps, 1, &mut extra, cancel)?;
                        segments = merge_recovered(segments, extra);
                    }
                }
                // The text the prompt rule and a resumed decoder see (`resume`).
                self.previous = Some((piece, segments_text(&segments)));
                segments
            }
            None => {
                self.report.dropped.push((offset, at(piece.end)));
                self.previous = None;
                Vec::new()
            }
        })
    }

    /// Step 5 (module docs): decodes `gaps`, the uncovered speech of `span` (the piece, or a
    /// part decoded at `level - 1`), each on its own, or in two halves when it is all of
    /// `span` (the same input would give the same answer), then the uncovered speech of
    /// each part, down to [`RECOVER_DEPTH`] levels. Accepted parts' segments (stream time)
    /// are appended to `out`; a repetitive part is ignored.
    #[allow(clippy::too_many_arguments)]
    fn recover_gaps<T: Transcriber + ?Sized>(
        &mut self,
        transcriber: &mut T,
        ctx: &Recover<'_>,
        span: Span,
        gaps: Vec<Span>,
        level: usize,
        out: &mut Vec<RawSegment>,
        cancel: &CancelToken,
    ) -> Result<(), AsrError> {
        if level > RECOVER_DEPTH {
            return Ok(());
        }
        let options = TranscribeOptions {
            prompt: None,
            ..self.options.clone()
        };
        for gap in gaps {
            let parts = if gap == span {
                let Some(c) = ctx.speech.cut(gap, samples(RECOVER_MIN_PART)) else {
                    continue;
                };
                vec![
                    Span {
                        start: gap.start,
                        end: c,
                    },
                    Span {
                        start: c,
                        end: gap.end,
                    },
                ]
            } else {
                vec![gap]
            };
            for part in parts {
                if cancel.is_cancelled() {
                    return Err(AsrError::Cancelled);
                }
                let from = part.start - ctx.piece.start;
                let audio = &ctx.samples[from..from + part.len()];
                let buffer = MonoBuffer::new(PIPELINE_RATE, audio.to_vec());
                let t = transcriber.transcribe(&buffer, &options, cancel)?;
                self.backend = t.backend.clone();
                if compression_ratio(&text_of(&t)) > self.threshold {
                    continue;
                }
                let offset = at(part.start);
                let segments: Vec<RawSegment> =
                    t.segments.into_iter().map(|s| shifted(s, offset)).collect();
                let inner =
                    uncovered_speech(&segments, part, ctx.speech, self.vad_threshold, ctx.min);
                // Only segments with something in them: an empty one would split a segment
                // of the piece for nothing.
                out.extend(
                    segments
                        .into_iter()
                        .filter(|s| !s.words.is_empty() || !s.text.trim().is_empty()),
                );
                self.recover_gaps(transcriber, ctx, part, inner, level + 1, out, cancel)?;
            }
        }
        Ok(())
    }

    /// The whole transcription of the pieces decoded (`segments` in order) and the report;
    /// the backend is the last call's (the one given to [`PieceDecoder::new`] when no piece
    /// was decoded).
    pub(super) fn finish(self, segments: Vec<RawSegment>) -> (Transcription, PieceReport) {
        (
            Transcription {
                segments,
                language: self.language.or_else(|| match &self.options.language {
                    Language::Code(c) => Some(c.clone()),
                    Language::Auto => None,
                }),
                backend: self.backend,
            },
            self.report,
        )
    }

    /// The report so far.
    pub(super) fn report(&self) -> &PieceReport {
        &self.report
    }

    /// The language so far: the first detected one.
    pub(super) fn language(&self) -> Option<&String> {
        self.language.as_ref()
    }
}
