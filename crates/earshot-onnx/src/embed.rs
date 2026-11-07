//! CAM++ speaker embeddings ([`CamppEmbedder`]) implementing core [`SpeakerEmbedder`].
//!
//! Model: 3D-Speaker CAM++ trained on VoxCeleb (ModelScope
//! `iic/speech_campplus_sv_en_voxceleb_16k`), ONNX export by sherpa-onnx (models manifest
//! `campplus-en-voxceleb`): input `x: [N, T, 80]` mean-normalised Kaldi fbank
//! ([`crate::fbank`]), output `embedding: [N, 512]`. The metadata is checked at load
//! (`sample_rate=16000`, `normalize_samples=1`, `feature_normalize_type=global-mean`,
//! `output_dim=512`).
//!
//! Each region is embedded on its own (regions differ in length; no padding, which would change
//! the statistics pooling). [`EmbedderInfo::min_samples`] is probed at load as the smallest
//! input the model embeds to a finite vector, as pyannote.audio 3.1 does for its embedding
//! models (`pyannote/audio/pipelines/speaker_verification.py`,
//! `PyannoteAudioPretrainedSpeakerEmbedding.min_num_samples`, a binary search).
//!
//! The [`EmbeddingModelId`] is `campplus-en-voxceleb@` plus the first 12 hex digits of the
//! model file's SHA-256, so a changed file never shares an id. Clustering thresholds tuned for
//! WeSpeaker (pyannote's 0.7046) do not transfer to CAM++; calibrate on dev data (T3.5).

use std::path::Path;

use earshot_core::asr::CancelToken;
use earshot_core::audio::Sample;
use earshot_core::diar::{DiarError, EmbedderInfo, Embedding, EmbeddingModelId, SpeakerEmbedder};
use ort::value::Tensor;

use crate::fbank::{FRAME_LEN, KaldiFbank, NUM_MELS};
use crate::runtime::{
    OnnxError, OnnxSession, SessionBackend, SessionConfig, file_sha256, runtime_err,
};

/// Model name used as the prefix of the [`EmbeddingModelId`].
pub const CAMPPLUS_MODEL: &str = "campplus-en-voxceleb";
/// Embedding dimension of CAM++.
pub const CAMPPLUS_DIM: usize = 512;

/// CAM++ on ONNX Runtime.
#[derive(Debug)]
pub struct CamppEmbedder {
    session: OnnxSession,
    fbank: KaldiFbank,
    info: EmbedderInfo,
}

impl CamppEmbedder {
    /// Loads the CAM++ model.
    ///
    /// # Errors
    /// [`OnnxError::Model`] if the file is missing or invalid, [`OnnxError::Signature`] if its
    /// inputs, outputs or metadata are not those of the CAM++ export.
    pub fn load(model: &Path, config: &SessionConfig) -> Result<Self, OnnxError> {
        let session = OnnxSession::load(model, config)?;
        session.expect_signature("CAM++", &["x"], &["embedding"])?;
        let meta = session.custom_metadata()?;
        let expected = [
            ("sample_rate", "16000"),
            ("normalize_samples", "1"),
            ("feature_normalize_type", "global-mean"),
            ("output_dim", "512"),
        ];
        for (key, want) in expected {
            if meta.get(key).map(String::as_str) != Some(want) {
                return Err(OnnxError::Signature(format!(
                    "CAM++ metadata `{key}` is {:?}, expected {want:?}",
                    meta.get(key)
                )));
            }
        }
        let sha = file_sha256(model)
            .map_err(|e| OnnxError::Model(format!("{}: {e}", model.display())))?;
        let id = EmbeddingModelId::new(format!("{CAMPPLUS_MODEL}@{}", &sha[..12]));
        let mut e = CamppEmbedder {
            session,
            fbank: KaldiFbank::new(),
            info: EmbedderInfo {
                model: id,
                dimension: CAMPPLUS_DIM,
                min_samples: FRAME_LEN,
            },
        };
        e.info.min_samples = e.probe_min_samples()?;
        Ok(e)
    }

    /// The provider requested and where ONNX Runtime placed the nodes.
    #[must_use]
    pub fn backend(&self) -> &SessionBackend {
        self.session.backend()
    }

    /// Smallest input length (samples) that yields a finite embedding: binary search between
    /// one fbank frame and 1 s, on a deterministic noise signal.
    fn probe_min_samples(&mut self) -> Result<usize, OnnxError> {
        let mut state = 0x2545_f491_4f6c_dd1du64;
        let noise: Vec<f32> = (0..16_000)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                ((state >> 40) as f32 / (1u64 << 24) as f32 - 0.5) * 0.2
            })
            .collect();
        let ok = |s: &mut Self, n: usize| {
            s.embed_one(&noise[..n])
                .is_ok_and(|v| v.iter().all(|x| x.is_finite()))
        };
        let (mut lo, mut hi) = (FRAME_LEN, noise.len());
        if !ok(self, hi) {
            return Err(OnnxError::Signature(
                "CAM++ cannot embed 1 s of audio".to_owned(),
            ));
        }
        if ok(self, lo) {
            return Ok(lo);
        }
        while hi - lo > 1 {
            let mid = lo + (hi - lo) / 2;
            if ok(self, mid) {
                hi = mid;
            } else {
                lo = mid;
            }
        }
        Ok(hi)
    }

    fn embed_one(&mut self, samples: &[f32]) -> Result<Vec<f32>, OnnxError> {
        let feats = self.fbank.compute_mean_normalized(samples);
        let frames = feats.len() / NUM_MELS;
        if frames == 0 {
            return Err(OnnxError::Runtime(
                "input shorter than one frame".to_owned(),
            ));
        }
        let x = Tensor::from_array(([1usize, frames, NUM_MELS], feats)).map_err(runtime_err)?;
        let outputs = self
            .session
            .session
            .run(ort::inputs!["x" => x])
            .map_err(runtime_err)?;
        let (_, v) = outputs["embedding"]
            .try_extract_tensor::<f32>()
            .map_err(runtime_err)?;
        if v.len() != CAMPPLUS_DIM {
            return Err(OnnxError::Signature(format!(
                "CAM++ returned {} values, expected {CAMPPLUS_DIM}",
                v.len()
            )));
        }
        Ok(v.to_vec())
    }
}

impl SpeakerEmbedder for CamppEmbedder {
    fn info(&self) -> &EmbedderInfo {
        &self.info
    }

    fn embed(
        &mut self,
        speech: &[&[Sample]],
        cancel: &CancelToken,
    ) -> Result<Vec<Embedding>, DiarError> {
        let mut out = Vec::with_capacity(speech.len());
        for region in speech {
            if cancel.is_cancelled() {
                return Err(DiarError::Cancelled);
            }
            if region.len() < self.info.min_samples {
                return Err(DiarError::Config(format!(
                    "speech region of {} samples is shorter than min_samples {}",
                    region.len(),
                    self.info.min_samples
                )));
            }
            let v = self.embed_one(region).map_err(|e| match e {
                OnnxError::Signature(m) => DiarError::Shape(m),
                other => DiarError::Backend(other.to_string()),
            })?;
            out.push(Embedding::new(self.info.model.clone(), v));
        }
        Ok(out)
    }
}
