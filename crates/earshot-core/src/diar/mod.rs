//! Speaker diarisation (PLAN.md 3.3, 3.4, T3.1), sans-IO.
//!
//! - [`SpeakerSegmenter`]: frame-level activity of up to N local speakers per fixed
//!   window (pyannote segmentation-3.0 shape: 10 s windows, 3 local speakers, powerset
//!   output decoded with [`Powerset`]).
//! - [`SpeakerEmbedder`]: one [`Embedding`] per speech region, tagged with the model's
//!   [`EmbeddingModelId`] so embeddings of different models are never compared.
//! - [`Diarizer`]: the pyannote.audio 3.x pipeline glue (sliding windows, local-speaker
//!   embeddings, clustering, stitching, overlap-aware reconstruction) producing
//!   [`SpeakerTurn`]s ([`pipeline`] has the algorithm and references).
//! - [`cluster_embeddings`]: agglomerative clustering with cosine distance, a threshold
//!   and an optional known speaker count.
//! - [`assign_words`], [`label_segments`] and [`label_segment_speakers`]: words and transcript
//!   segments to speakers.
//! - [`policy`]: the two-track (mic "Me" + loopback remote speakers) and in-person track
//!   policies of PLAN.md 3.3.
//! - [`FakeSegmenter`], [`FakeEmbedder`], [`scene_audio`]: test doubles that read the
//!   ground truth out of synthetic audio.
//!
//! Model backends (ONNX) live in adapter crates; everything here is single-threaded:
//! [`Diarizer::diarize`] runs over a whole track after the meeting ends, or the per-window
//! half runs during the meeting ([`DiarStream`], [`Diarizer::advance`]) and
//! [`Diarizer::diarize_from`] finishes at the end ([`stream`]).

mod assign;
pub mod cluster;
pub mod echo;
mod fake;
pub mod pipeline;
pub mod policy;
pub mod stream;
mod types;

#[cfg(test)]
mod stream_tests;
#[cfg(test)]
mod tests;

pub use assign::{assign_words, label_segment_speakers, label_segments};
pub use cluster::{Clustering, ClusteringConfig, cluster_embeddings};
pub use echo::{
    CALIBRATED_SPEAKER_ECHO_MARGINS, CAMPPLUS_SPEAKER_ECHO_MARGIN, CalibratedSpeakerEchoMargin,
    ECHO_SPAN_PIECE_MS, EchoSpanPiece, SpanEmbedding, SpeakerEchoConfig,
    calibrated_speaker_echo_margin, echo_span_pieces, me_speakers, remove_speaker_echo,
    speaker_echo_spans,
};
pub use fake::{
    FakeEmbedder, FakeSegmenter, MAX_SCENE_SPEAKERS, SceneTurn, sample_mask, scene_audio,
};
pub use pipeline::{DiarSpeaker, Diarization, DiarizationConfig, Diarizer, SpeakerTurn};
pub use policy::{
    MeetingDiarization, MeetingSpeaker, MeetingTurn, SpeakerRole, in_person, two_track,
};
pub use stream::{DiarStream, WINDOW_RESULTS_FORMAT, WindowParams, WindowResults};
pub use types::{
    DiarError, EmbedderInfo, Embedding, EmbeddingModelId, LocalActivity, Powerset, SegmenterInfo,
    SpeakerEmbedder, SpeakerSegmenter,
};
