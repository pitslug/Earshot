//! Diarisation model interfaces: [`SpeakerSegmenter`], [`SpeakerEmbedder`], their
//! descriptions and outputs, embeddings with a model id, errors and the powerset decoder.

use core::fmt;

use serde::{Deserialize, Serialize};

use crate::asr::CancelToken;
use crate::audio::Sample;

/// Diarisation errors.
#[derive(Debug, Clone, PartialEq)]
pub enum DiarError {
    /// Audio was not at [`crate::audio::PIPELINE_RATE`].
    WrongSampleRate {
        /// The rate required.
        expected: u32,
        /// The rate given.
        actual: u32,
    },
    /// The [`CancelToken`] was cancelled.
    Cancelled,
    /// Invalid configuration or model description.
    Config(String),
    /// A model returned output of the wrong shape or count.
    Shape(String),
    /// Two embeddings from different models were compared.
    ModelMismatch {
        /// Model of the left-hand embedding.
        left: EmbeddingModelId,
        /// Model of the right-hand embedding.
        right: EmbeddingModelId,
    },
    /// The backend failed.
    Backend(String),
    /// Stored or streamed per-window results do not fit this diarizer or this audio
    /// ([`super::WindowResults`]).
    State(String),
}

impl fmt::Display for DiarError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DiarError::WrongSampleRate { expected, actual } => {
                write!(f, "audio must be {expected} Hz, got {actual} Hz")
            }
            DiarError::Cancelled => f.write_str("diarisation cancelled"),
            DiarError::Config(m) => write!(f, "invalid diarisation configuration: {m}"),
            DiarError::Shape(m) => write!(f, "diarisation model output has the wrong shape: {m}"),
            DiarError::ModelMismatch { left, right } => write!(
                f,
                "embeddings from different models cannot be compared ({left} vs {right})"
            ),
            DiarError::Backend(m) => write!(f, "diarisation backend error: {m}"),
            DiarError::State(m) => write!(f, "diarisation window results unusable: {m}"),
        }
    }
}

impl std::error::Error for DiarError {}

/// Identifies the speaker-embedding model (and version) that produced an [`Embedding`].
///
/// Embeddings from different models live in different spaces; they are never compared
/// (PLAN.md T3.4). Use a string that changes whenever the model file changes, for example
/// `"wespeaker-resnet34-lm@<sha256 prefix>"`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct EmbeddingModelId(String);

impl EmbeddingModelId {
    /// Wraps a model identifier.
    #[must_use]
    pub fn new(id: impl Into<String>) -> Self {
        EmbeddingModelId(id.into())
    }

    /// The identifier.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for EmbeddingModelId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// A speaker embedding tagged with the model that produced it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Embedding {
    /// The model that produced `vector`.
    pub model: EmbeddingModelId,
    /// The embedding vector (any scale; comparisons use cosine similarity).
    pub vector: Vec<f32>,
}

impl Embedding {
    /// An embedding of `model`.
    #[must_use]
    pub fn new(model: EmbeddingModelId, vector: Vec<f32>) -> Self {
        Embedding { model, vector }
    }

    /// True if every component is finite and the vector is not all zeros. Invalid
    /// embeddings (a model's NaN output for too little speech) are ignored by the pipeline.
    #[must_use]
    pub fn is_valid(&self) -> bool {
        !self.vector.is_empty()
            && self.vector.iter().all(|x| x.is_finite())
            && self.vector.iter().any(|&x| x != 0.0)
    }

    /// Cosine similarity in `[-1, 1]` (0 if either vector is all zeros).
    ///
    /// # Errors
    /// [`DiarError::ModelMismatch`] if the embeddings come from different models, and
    /// [`DiarError::Shape`] if the dimensions differ.
    pub fn cosine_similarity(&self, other: &Embedding) -> Result<f32, DiarError> {
        if self.model != other.model {
            return Err(DiarError::ModelMismatch {
                left: self.model.clone(),
                right: other.model.clone(),
            });
        }
        if self.vector.len() != other.vector.len() {
            return Err(DiarError::Shape(format!(
                "embedding dimensions differ ({} vs {})",
                self.vector.len(),
                other.vector.len()
            )));
        }
        Ok(super::cluster::cosine_similarity(
            &self.vector,
            &other.vector,
        ))
    }
}

/// What a [`SpeakerSegmenter`] consumes and produces. All lengths are in samples at
/// [`crate::audio::PIPELINE_RATE`].
///
/// Output frame `i` of a window is centred at `i * frame_step + frame_duration / 2` from
/// the window start (the pyannote `SlidingWindow` convention for the model's receptive
/// field) and stands for the `frame_step` samples around that centre.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SegmenterInfo {
    /// Model identifier, for reports.
    pub model: String,
    /// Window length in samples; every window passed to the model has exactly this length.
    pub window: usize,
    /// Output frames per window.
    pub frames: usize,
    /// Distance between output frames in samples.
    pub frame_step: usize,
    /// Receptive field of one output frame in samples.
    pub frame_duration: usize,
    /// Number of local speakers (output columns) per window.
    pub max_local_speakers: usize,
}

impl SegmenterInfo {
    /// pyannote segmentation-3.0 at 16 kHz: 10 s windows (160 000 samples), 589 frames
    /// with a step of 270 samples and a receptive field of 991 samples, 3 local speakers
    /// (decoded from the 7-class powerset output with [`Powerset`]).
    #[must_use]
    pub fn pyannote_segmentation_3_0(model: impl Into<String>) -> Self {
        SegmenterInfo {
            model: model.into(),
            window: 160_000,
            frames: 589,
            frame_step: 270,
            frame_duration: 991,
            max_local_speakers: 3,
        }
    }

    /// Checks that the description is internally consistent.
    ///
    /// # Errors
    /// [`DiarError::Config`] for zero sizes or frames that do not fit in the window.
    pub fn validate(&self) -> Result<(), DiarError> {
        if self.window == 0
            || self.frames == 0
            || self.frame_step == 0
            || self.max_local_speakers == 0
        {
            return Err(DiarError::Config(
                "segmenter window, frames, frame_step and max_local_speakers must be > 0"
                    .to_owned(),
            ));
        }
        if self.frame_duration < self.frame_step {
            return Err(DiarError::Config(
                "segmenter frame_duration must be >= frame_step".to_owned(),
            ));
        }
        if (self.frames - 1) * self.frame_step + self.frame_duration > self.window {
            return Err(DiarError::Config(
                "segmenter frames do not fit in the window".to_owned(),
            ));
        }
        Ok(())
    }

    /// Offset (samples from the window start) of the first sample that frame `i` stands
    /// for: its centre minus half a step.
    #[must_use]
    pub fn frame_start(&self, i: usize) -> usize {
        i * self.frame_step + self.frame_duration / 2 - self.frame_step / 2
    }
}

/// Frame-level activity of the local speakers of one window: `frames x speakers` values
/// in `[0, 1]`, row-major. Local speaker `j` of one window has no relation to local
/// speaker `j` of another; the pipeline stitches them with embeddings.
#[derive(Debug, Clone, PartialEq)]
pub struct LocalActivity {
    frames: usize,
    speakers: usize,
    data: Vec<f32>,
}

impl LocalActivity {
    /// Wraps row-major `frames x speakers` values.
    ///
    /// # Errors
    /// [`DiarError::Shape`] if `data.len() != frames * speakers`.
    pub fn new(frames: usize, speakers: usize, data: Vec<f32>) -> Result<Self, DiarError> {
        if data.len() != frames * speakers {
            return Err(DiarError::Shape(format!(
                "activity has {} values, expected {frames} x {speakers}",
                data.len()
            )));
        }
        Ok(LocalActivity {
            frames,
            speakers,
            data,
        })
    }

    /// Number of frames.
    #[must_use]
    pub fn frames(&self) -> usize {
        self.frames
    }

    /// Number of local speakers.
    #[must_use]
    pub fn speakers(&self) -> usize {
        self.speakers
    }

    /// Activity of local speaker `speaker` at frame `frame`.
    ///
    /// # Panics
    /// If either index is out of range.
    #[must_use]
    pub fn get(&self, frame: usize, speaker: usize) -> f32 {
        assert!(frame < self.frames && speaker < self.speakers);
        self.data[frame * self.speakers + speaker]
    }
}

/// Frame-level multi-speaker activity model (pyannote segmentation-3.0 shaped, PLAN.md
/// 3.4). Implementations live in adapter crates (`earshot-onnx`); [`super::FakeSegmenter`]
/// is the test double.
///
/// Calls are blocking; the caller chooses the thread. Implementations poll `cancel`.
pub trait SpeakerSegmenter {
    /// The window and frame geometry. Must not change between calls.
    fn info(&self) -> &SegmenterInfo;

    /// Runs the model on a batch of windows. Every window has exactly
    /// [`SegmenterInfo::window`] samples at 16 kHz (the last window of a short recording is
    /// zero-padded). Returns one [`LocalActivity`] per window with [`SegmenterInfo::frames`]
    /// frames and [`SegmenterInfo::max_local_speakers`] speakers.
    ///
    /// # Errors
    /// [`DiarError::Cancelled`] if `cancel` fires, [`DiarError::Backend`] on model failure.
    fn segment(
        &mut self,
        windows: &[&[Sample]],
        cancel: &CancelToken,
    ) -> Result<Vec<LocalActivity>, DiarError>;
}

/// A boxed segmenter (for example `Box<dyn SpeakerSegmenter + Send>`) is a segmenter.
impl<T: SpeakerSegmenter + ?Sized> SpeakerSegmenter for Box<T> {
    fn info(&self) -> &SegmenterInfo {
        (**self).info()
    }

    fn segment(
        &mut self,
        windows: &[&[Sample]],
        cancel: &CancelToken,
    ) -> Result<Vec<LocalActivity>, DiarError> {
        (**self).segment(windows, cancel)
    }
}

/// What a [`SpeakerEmbedder`] produces.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EmbedderInfo {
    /// Model id, copied into every [`Embedding`].
    pub model: EmbeddingModelId,
    /// Embedding dimension.
    pub dimension: usize,
    /// Least speech (samples at 16 kHz) the model can embed; shorter regions get no
    /// embedding.
    pub min_samples: usize,
}

/// Speaker-embedding model (CAM++ or WeSpeaker, PLAN.md 3.4). Implementations live in
/// adapter crates; [`super::FakeEmbedder`] is the test double.
///
/// Calls are blocking; the caller chooses the thread. Implementations poll `cancel`.
pub trait SpeakerEmbedder {
    /// Model id, dimension and minimum input length. Must not change between calls.
    fn info(&self) -> &EmbedderInfo;

    /// Embeds a batch of speech regions (16 kHz samples of one speaker, each at least
    /// [`EmbedderInfo::min_samples`] long). Returns one embedding per region, tagged with
    /// [`EmbedderInfo::model`]. A region the model cannot embed may yield a non-finite or
    /// all-zero vector ([`Embedding::is_valid`] is false); the pipeline then ignores it.
    ///
    /// # Errors
    /// [`DiarError::Cancelled`] if `cancel` fires, [`DiarError::Backend`] on model failure.
    fn embed(
        &mut self,
        speech: &[&[Sample]],
        cancel: &CancelToken,
    ) -> Result<Vec<Embedding>, DiarError>;
}

/// A boxed embedder (for example `Box<dyn SpeakerEmbedder + Send>`) is an embedder.
impl<T: SpeakerEmbedder + ?Sized> SpeakerEmbedder for Box<T> {
    fn info(&self) -> &EmbedderInfo {
        (**self).info()
    }

    fn embed(
        &mut self,
        speech: &[&[Sample]],
        cancel: &CancelToken,
    ) -> Result<Vec<Embedding>, DiarError> {
        (**self).embed(speech, cancel)
    }
}

/// Decoder for powerset segmentation output (pyannote.audio 3.x `Powerset`,
/// `pyannote/audio/utils/powerset.py`): each class is a set of at most `max_set_size` of
/// `num_speakers` local speakers, ordered by set size and then lexicographically. For
/// segmentation-3.0 (`3, 2`) the 7 classes are `{}, {0}, {1}, {2}, {0,1}, {0,2}, {1,2}`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Powerset {
    num_speakers: usize,
    classes: Vec<Vec<usize>>,
}

impl Powerset {
    /// The powerset of `num_speakers` speakers with sets of at most `max_set_size`.
    #[must_use]
    pub fn new(num_speakers: usize, max_set_size: usize) -> Self {
        let mut classes = Vec::new();
        for size in 0..=max_set_size.min(num_speakers) {
            combinations(num_speakers, size, 0, &mut Vec::new(), &mut classes);
        }
        Powerset {
            num_speakers,
            classes,
        }
    }

    /// The speaker sets, in class order.
    #[must_use]
    pub fn classes(&self) -> &[Vec<usize>] {
        &self.classes
    }

    /// Hard multilabel activity from per-frame class scores (`frames x classes`,
    /// row-major, logits or probabilities): each frame takes its highest-scoring class
    /// (the first on ties), as pyannote's `Powerset.to_multilabel`.
    ///
    /// # Errors
    /// [`DiarError::Shape`] if `scores.len()` is not a multiple of the class count.
    pub fn to_multilabel(&self, scores: &[f32]) -> Result<LocalActivity, DiarError> {
        let n = self.classes.len();
        if n == 0 || !scores.len().is_multiple_of(n) {
            return Err(DiarError::Shape(format!(
                "{} powerset scores is not a multiple of {n} classes",
                scores.len()
            )));
        }
        let frames = scores.len() / n;
        let mut data = vec![0.0f32; frames * self.num_speakers];
        for (f, row) in scores.chunks(n).enumerate() {
            let mut best = 0;
            for (c, &v) in row.iter().enumerate() {
                if v > row[best] {
                    best = c;
                }
            }
            for &s in &self.classes[best] {
                data[f * self.num_speakers + s] = 1.0;
            }
        }
        LocalActivity::new(frames, self.num_speakers, data)
    }
}

fn combinations(n: usize, k: usize, from: usize, cur: &mut Vec<usize>, out: &mut Vec<Vec<usize>>) {
    if cur.len() == k {
        out.push(cur.clone());
        return;
    }
    for i in from..n {
        cur.push(i);
        combinations(n, k, i + 1, cur, out);
        cur.pop();
    }
}
