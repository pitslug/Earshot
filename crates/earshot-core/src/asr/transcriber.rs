//! The batch and streaming transcriber traits, their options and errors.
//!
//! Backends (whisper.cpp in `earshot-asr-whisper`, QNN in `earshot-asr-qnn`) implement
//! [`Transcriber`]. Calls are blocking and synchronous: the runtime decides which thread
//! runs them, core never spawns one (PLAN.md 3.2). A long decode is stopped from another
//! thread through a [`CancelToken`].

use core::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use serde::{Deserialize, Serialize};

use super::types::{MeetingTime, Segment, TranscriptEvent, Word, join_words};
use crate::audio::{MonoBuffer, PIPELINE_RATE, Sample};
use crate::identity::EntityId;

/// Spoken language to decode.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Language {
    /// Let the model detect the language.
    Auto,
    /// An ISO 639-1 code such as `"en"`.
    Code(String),
}

impl Default for Language {
    /// English, the default for every shipped model.
    fn default() -> Self {
        Language::Code("en".to_owned())
    }
}

/// Search strategy for the decoder.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "strategy", rename_all = "snake_case")]
pub enum DecodingStrategy {
    /// Greedy search (one candidate).
    Greedy,
    /// Beam search with `beam_size` beams.
    BeamSearch {
        /// Number of beams (>= 1).
        beam_size: u32,
    },
}

/// Whisper's temperature fallback (PLAN.md 4.0, T2.4d): a decoding window whose output is
/// too repetitive or too unsure is decoded again at a higher temperature, up to 1.0.
///
/// Reference: openai/whisper `transcribe.py` (`temperature=(0.0, 0.2, 0.4, 0.6, 0.8, 1.0)`,
/// `compression_ratio_threshold=2.4`, `logprob_threshold=-1.0`, a window counted as silence
/// when its no-speech probability is high is not retried). whisper.cpp implements the same
/// recipe (`temperature_inc`, `entropy_thold`, `logprob_thold`) and differs in two ways.
/// First, it measures repetition by the entropy of the last 32 tokens instead of the zlib
/// compression ratio, with the same default threshold 2.4 (whisper.cpp 1.8.3
/// `whisper_full_default_params`). Second, at a temperature above 0 the whisper.cpp backend
/// samples one candidate (a single sampled decoder) on both paths; for batch the reference
/// samples `best_of=5` candidates and keeps the most likely, but whisper-rs 0.16 offers no
/// way to set `greedy.best_of` for a beam-search call, so whisper.cpp keeps its -1 (one
/// decoder).
/// Expected effect: a retried batch window is noisier than the reference's best of five,
/// so fallback windows may carry a few more errors; windows that do not fall back are
/// unaffected (0 of 197 LibriSpeech windows fell back in T2.4d's run).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct TemperatureFallback {
    /// Temperature added per retry, up to 1.0 (reference: 0.2).
    pub increment: f32,
    /// Output more repetitive than this is retried (reference: compression ratio above 2.4;
    /// whisper.cpp: token entropy below 2.4).
    pub repetition_threshold: f32,
    /// Output whose average token log-probability is below this is retried (reference:
    /// -1.0), unless the window is silence.
    pub logprob_threshold: f32,
}

impl TemperatureFallback {
    /// The reference settings (openai/whisper `transcribe.py`, whisper.cpp defaults).
    pub const WHISPER: Self = TemperatureFallback {
        increment: 0.2,
        repetition_threshold: 2.4,
        logprob_threshold: -1.0,
    };
}

/// Decoder settings. PLAN.md 4.0: greedy for live/streaming, beam size 5 for batch and the
/// final pass, temperature 0 with Whisper's temperature fallback for both (T2.4d);
/// acceptance reports record the settings used.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct DecodingOptions {
    /// Search strategy at the first temperature.
    pub strategy: DecodingStrategy,
    /// Sampling temperature of the first attempt (0 = deterministic).
    pub temperature: f32,
    /// Retry repetitive or unsure windows at higher temperatures (`None`: never retry).
    #[serde(default)]
    pub fallback: Option<TemperatureFallback>,
}

impl DecodingOptions {
    /// Live and streaming decoding: greedy, temperature 0, Whisper's fallback.
    pub const LIVE: Self = DecodingOptions {
        strategy: DecodingStrategy::Greedy,
        temperature: 0.0,
        fallback: Some(TemperatureFallback::WHISPER),
    };
    /// Batch and final-pass decoding: beam size 5, temperature 0, Whisper's fallback.
    pub const BATCH: Self = DecodingOptions {
        strategy: DecodingStrategy::BeamSearch { beam_size: 5 },
        temperature: 0.0,
        fallback: Some(TemperatureFallback::WHISPER),
    };
}

/// Options for one [`Transcriber::transcribe`] call.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TranscribeOptions {
    /// Language to decode.
    pub language: Language,
    /// Decoder settings.
    pub decoding: DecodingOptions,
    /// Text that precedes the audio (previous transcript), given to the model as context.
    pub prompt: Option<String>,
}

impl TranscribeOptions {
    /// Live/streaming defaults: English, [`DecodingOptions::LIVE`], no prompt.
    #[must_use]
    pub fn live() -> Self {
        TranscribeOptions {
            language: Language::default(),
            decoding: DecodingOptions::LIVE,
            prompt: None,
        }
    }

    /// Batch/final-pass defaults: English, [`DecodingOptions::BATCH`], no prompt.
    #[must_use]
    pub fn batch() -> Self {
        TranscribeOptions {
            language: Language::default(),
            decoding: DecodingOptions::BATCH,
            prompt: None,
        }
    }
}

/// Compute backend family that ran a model.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BackendKind {
    /// CPU.
    Cpu,
    /// NVIDIA CUDA.
    Cuda,
    /// Vulkan GPU.
    Vulkan,
    /// Qualcomm NPU through the ONNX Runtime QNN execution provider.
    Qnn,
    /// A test fake; never used for reported metrics.
    Fake,
}

/// Which backend, device and model actually ran a transcription. Acceptance reports must
/// say which backend ran (AGENTS.md 9), so this is what a backend observed, not what was
/// requested.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct BackendInfo {
    /// Backend family.
    pub kind: BackendKind,
    /// Device name as reported by the backend (for example the GPU name), or `"cpu"`.
    pub device: String,
    /// Model identifier (for example `"ggml-base.en"`).
    pub model: String,
}

/// Cooperative cancellation flag shared between the thread running a decode and the
/// thread that wants it stopped. Cloning shares the flag.
#[derive(Debug, Clone, Default)]
pub struct CancelToken(Arc<AtomicBool>);

impl CancelToken {
    /// A token that is not cancelled.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Requests cancellation. Idempotent.
    pub fn cancel(&self) {
        self.0.store(true, Ordering::SeqCst);
    }

    /// Whether cancellation was requested.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}

/// ASR and VAD errors.
#[derive(Debug, Clone, PartialEq)]
pub enum AsrError {
    /// Audio was not at [`PIPELINE_RATE`].
    WrongSampleRate {
        /// The rate required.
        expected: u32,
        /// The rate given.
        actual: u32,
    },
    /// The [`CancelToken`] was cancelled.
    Cancelled,
    /// Invalid configuration.
    Config(String),
    /// The backend or VAD failed.
    Backend(String),
}

impl fmt::Display for AsrError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AsrError::WrongSampleRate { expected, actual } => {
                write!(f, "audio must be {expected} Hz, got {actual} Hz")
            }
            AsrError::Cancelled => f.write_str("transcription cancelled"),
            AsrError::Config(m) => write!(f, "invalid ASR configuration: {m}"),
            AsrError::Backend(m) => write!(f, "ASR backend error: {m}"),
        }
    }
}

impl std::error::Error for AsrError {}

/// Fails unless `audio` is at [`PIPELINE_RATE`]. Backends call this first.
///
/// # Errors
/// [`AsrError::WrongSampleRate`].
pub fn check_pipeline_rate(audio: &MonoBuffer) -> Result<(), AsrError> {
    if audio.rate == PIPELINE_RATE {
        Ok(())
    } else {
        Err(AsrError::WrongSampleRate {
            expected: PIPELINE_RATE.hz(),
            actual: audio.rate.hz(),
        })
    }
}

/// One segment as a backend returns it. Times are relative to the start of the buffer
/// that was transcribed.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RawSegment {
    /// Start relative to the buffer start.
    pub start: MeetingTime,
    /// End relative to the buffer start.
    pub end: MeetingTime,
    /// Segment text.
    pub text: String,
    /// Words with buffer-relative timestamps. May be empty if the backend has no word
    /// timestamps; [`RawSegment::timed_words`] then interpolates.
    pub words: Vec<Word>,
}

impl RawSegment {
    /// A segment whose text and span come from `words` (buffer-relative).
    #[must_use]
    pub fn from_words(words: Vec<Word>) -> Self {
        let start = words.first().map_or(MeetingTime::ZERO, |w| w.start);
        let end = words.last().map_or(start, |w| w.end);
        RawSegment {
            start,
            end,
            text: join_words(&words),
            words,
        }
    }

    /// The segment's words. If the backend gave none, the text is split on whitespace and
    /// the span shared out in proportion to character count, with probability 0.5.
    #[must_use]
    pub fn timed_words(&self) -> Vec<Word> {
        if !self.words.is_empty() {
            return self.words.clone();
        }
        let tokens: Vec<&str> = self.text.split_whitespace().collect();
        let total: u64 = tokens.iter().map(|t| t.chars().count() as u64).sum();
        if total == 0 {
            return Vec::new();
        }
        let span = self.end.as_millis().saturating_sub(self.start.as_millis());
        let mut acc = 0u64;
        tokens
            .iter()
            .map(|t| {
                let s = self.start.as_millis() + span * acc / total;
                acc += t.chars().count() as u64;
                let e = self.start.as_millis() + span * acc / total;
                Word::new(*t, s, e, 0.5)
            })
            .collect()
    }
}

/// Result of a [`Transcriber::transcribe`] call.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Transcription {
    /// Segments in time order, buffer-relative.
    pub segments: Vec<RawSegment>,
    /// The language decoded (detected when [`Language::Auto`] was requested).
    pub language: Option<String>,
    /// What actually ran.
    pub backend: BackendInfo,
}

impl Transcription {
    /// All words in time order, buffer-relative.
    #[must_use]
    pub fn words(&self) -> Vec<Word> {
        self.segments
            .iter()
            .flat_map(RawSegment::timed_words)
            .collect()
    }

    /// Converts to domain segments of `track`, shifting times by `offset` (the meeting
    /// time of the buffer's first sample). Empty segments are dropped; `next_id` is called
    /// once per segment kept.
    #[must_use]
    pub fn into_segments(
        self,
        track: EntityId,
        offset: MeetingTime,
        next_id: &mut dyn FnMut() -> EntityId,
    ) -> Vec<Segment> {
        self.segments
            .iter()
            .filter_map(|raw| {
                // Whisper's non-speech annotations are not words (T6.7).
                let words: Vec<Word> = super::nonspeech::drop_non_speech(raw.timed_words())
                    .iter()
                    .map(|w| w.shifted(offset))
                    .collect();
                if words.is_empty() {
                    return None;
                }
                Some(Segment::from_words(
                    next_id(),
                    track,
                    words,
                    raw.start + offset,
                ))
            })
            .collect()
    }
}

/// Batch speech recogniser: one buffer in, segments out.
///
/// Implementations block for the whole decode (possibly on a GPU or NPU) and must poll
/// `cancel` regularly (whisper.cpp: its abort callback), returning
/// [`AsrError::Cancelled`] once it is set.
pub trait Transcriber {
    /// Transcribes `audio`, which must be mono at [`PIPELINE_RATE`].
    ///
    /// # Errors
    /// [`AsrError::WrongSampleRate`], [`AsrError::Cancelled`] or [`AsrError::Backend`].
    fn transcribe(
        &mut self,
        audio: &MonoBuffer,
        options: &TranscribeOptions,
        cancel: &CancelToken,
    ) -> Result<Transcription, AsrError>;

    /// The backend this transcriber runs on.
    fn backend_info(&self) -> BackendInfo;

    /// Whether this backend is known to leave stretches of speech without any words: a
    /// transducer whose decoder stays on blank once it has fallen there (Parakeet TDT 0.6B,
    /// T2.5d). The piece decoders ([`super::transcribe_in_pieces`] and
    /// [`super::PieceStream`]) then decode speech that no word covers again in parts
    /// ([`super::PieceConfig::recover_speech`]). Wrappers forward it.
    fn may_skip_speech(&self) -> bool {
        false
    }
}

impl<T: Transcriber + ?Sized> Transcriber for &mut T {
    fn transcribe(
        &mut self,
        audio: &MonoBuffer,
        options: &TranscribeOptions,
        cancel: &CancelToken,
    ) -> Result<Transcription, AsrError> {
        (**self).transcribe(audio, options, cancel)
    }

    fn backend_info(&self) -> BackendInfo {
        (**self).backend_info()
    }

    fn may_skip_speech(&self) -> bool {
        (**self).may_skip_speech()
    }
}

impl<T: Transcriber + ?Sized> Transcriber for Box<T> {
    fn transcribe(
        &mut self,
        audio: &MonoBuffer,
        options: &TranscribeOptions,
        cancel: &CancelToken,
    ) -> Result<Transcription, AsrError> {
        (**self).transcribe(audio, options, cancel)
    }

    fn backend_info(&self) -> BackendInfo {
        (**self).backend_info()
    }

    fn may_skip_speech(&self) -> bool {
        (**self).may_skip_speech()
    }
}

/// Streaming speech recogniser: audio chunks in, partial and final segment events out.
///
/// Step-driven: each call does its work (including any blocking decodes) on the caller's
/// thread and returns the events it produced. [`super::StreamingChunker`] implements this
/// over any [`Transcriber`] and [`super::Vad`]; a backend with native streaming can
/// implement it directly.
pub trait StreamingTranscriber {
    /// Appends `audio` (mono, [`PIPELINE_RATE`], contiguous with the previous push).
    ///
    /// # Errors
    /// [`AsrError::Cancelled`] once cancelled; backend or VAD errors otherwise.
    fn push(&mut self, audio: &[Sample]) -> Result<Vec<TranscriptEvent>, AsrError>;

    /// End of stream: finalises every open segment.
    ///
    /// # Errors
    /// As for [`StreamingTranscriber::push`].
    fn finish(&mut self) -> Result<Vec<TranscriptEvent>, AsrError>;

    /// A token that cancels this stream from any thread. After cancellation `push` and
    /// `finish` return [`AsrError::Cancelled`] and open segments are abandoned.
    fn cancel_token(&self) -> CancelToken;

    /// The backend decoding this stream.
    fn backend_info(&self) -> BackendInfo;
}
