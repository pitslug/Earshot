//! Speech recognition interfaces and the streaming chunker (PLAN.md 3.3, 3.4).
//!
//! - [`Segment`], [`Word`], [`MeetingTime`]: transcript data, meeting-relative times.
//! - [`Transcriber`]: batch recogniser (one buffer at [`crate::audio::PIPELINE_RATE`] in,
//!   segments out), with [`TranscribeOptions`] (language, decoding per PLAN.md 4.0) and a
//!   [`CancelToken`]; [`BackendInfo`] says which backend and device ran.
//! - [`StreamingTranscriber`]: push audio, receive [`TranscriptEvent`]s (partial, final).
//! - [`Vad`] and [`EnergyVad`]: voice activity detection.
//! - [`StreamingChunker`]: VAD-driven windows of at most 15 s with overlap and
//!   LocalAgreement-2 stabilisation ([`LocalAgreement`]), over any `Transcriber`.
//! - [`dedupe_echo`]: removes far-end speech that leaked into the mic transcript (T2.4e).
//! - [`transcribe_in_pieces`]: batch decoding in VAD speech pieces with a repetition check
//!   (the final pass, T6.16).
//! - [`drop_non_speech`]: removes Whisper's `[BLANK_AUDIO]`-style annotations (T6.7).
//! - [`Vocab`], [`ctc_greedy`], [`words_from_tokens`]: CTC decoding for Parakeet (T6.10e1).
//! - [`tdt_greedy`] over a [`TdtModel`]: greedy TDT decoding for Parakeet (T6.10e2).
//! - [`FakeTranscriber`]: deterministic test double.
//!
//! Everything here is step-driven and single-threaded; backends block inside
//! [`Transcriber::transcribe`] on whatever thread the runtime chose.

mod agreement;
mod chunker;
mod ctc;
mod echo;
mod fake;
mod nonspeech;
mod pieces;
#[cfg(test)]
mod pieces_tests;
mod stretch;
#[cfg(test)]
mod stretch_tests;
mod tdt;
mod transcriber;
mod types;
mod vad;

#[cfg(test)]
mod tests;

pub use agreement::{LocalAgreement, normalize_token};
pub use chunker::{ChunkerConfig, MAX_WINDOW_LIMIT, SegmentIds, StreamingChunker};
pub use ctc::{BLANK, CtcToken, Vocab, WORD_START, ctc_greedy, words_from_tokens};
pub use echo::{EchoDedup, EchoDedupConfig, dedupe_echo, dedupe_echo_segments};
pub use fake::{FakeCall, FakeTranscriber};
pub use nonspeech::{MAX_ANNOTATION_WORDS, drop_non_speech, is_non_speech};
pub use pieces::{
    PieceConfig, PieceReport, Span, compression_ratio, frame_probabilities, plan_pieces,
    speech_between_words, speech_regions, transcribe_in_pieces,
};
pub use stretch::{
    DecodedPieces, PieceStream, SETTLE_EVERY_FRAMES, StreamPiece, StreamSummary, settled_pieces,
    transcribe_rest_in_pieces,
};
pub use tdt::{TdtConfig, TdtModel, tdt_greedy};
pub use transcriber::{
    AsrError, BackendInfo, BackendKind, CancelToken, DecodingOptions, DecodingStrategy, Language,
    RawSegment, StreamingTranscriber, TemperatureFallback, TranscribeOptions, Transcriber,
    Transcription, check_pipeline_rate,
};
pub use types::{
    MeetingTime, PartialSegment, Segment, TranscriptEvent, Word, join_words, mean_probability,
};
pub use vad::{EnergyVad, EnergyVadConfig, Vad};
