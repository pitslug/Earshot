//! pyannote segmentation-3.0 ([`PyannoteSegmenter`]) implementing core
//! [`SpeakerSegmenter`].
//!
//! The sherpa-onnx export (models manifest `pyannote-segmentation-3.0`, `model.onnx`) takes
//! `x: [N, 1, 160000]` (10 s at 16 kHz, samples in `[-1, 1]`) and returns
//! `y: [N, 589, 7]` powerset log-probabilities, decoded per frame with core
//! [`Powerset`] (3 speakers, sets of at most 2), as pyannote.audio 3.x `Powerset.to_multilabel`.
//! The geometry is checked against the model metadata at load (`window_size`,
//! `receptive_field_shift`, `receptive_field_size`, `num_speakers`, `powerset_max_classes`,
//! `num_classes`, `sample_rate`).

use std::path::Path;

use earshot_core::asr::CancelToken;
use earshot_core::audio::Sample;
use earshot_core::diar::{DiarError, LocalActivity, Powerset, SegmenterInfo, SpeakerSegmenter};
use ort::value::Tensor;

use crate::runtime::{OnnxError, OnnxSession, SessionBackend, SessionConfig};

/// Model name in [`SegmenterInfo::model`], which is `<SEGMENTATION_MODEL>@<first 12 hex
/// digits of the file's SHA-256>` so that results computed with one model file are never
/// taken for another's (T6.13d2, as the CAM++ id does).
pub const SEGMENTATION_MODEL: &str = "pyannote-segmentation-3.0";

/// pyannote segmentation-3.0 on ONNX Runtime.
#[derive(Debug)]
pub struct PyannoteSegmenter {
    session: OnnxSession,
    info: SegmenterInfo,
    powerset: Powerset,
    max_batch: usize,
}

fn meta_usize(
    meta: &std::collections::BTreeMap<String, String>,
    key: &str,
) -> Result<usize, OnnxError> {
    meta.get(key)
        .and_then(|v| v.trim().parse().ok())
        .ok_or_else(|| OnnxError::Signature(format!("segmentation metadata `{key}` missing")))
}

impl PyannoteSegmenter {
    /// Loads the segmentation model; `max_batch` bounds the windows per ONNX Runtime call.
    ///
    /// # Errors
    /// [`OnnxError::Model`] if the file is missing or invalid, [`OnnxError::Signature`] if its
    /// inputs, outputs or metadata do not describe segmentation-3.0 at 16 kHz.
    pub fn load(model: &Path, config: &SessionConfig, max_batch: usize) -> Result<Self, OnnxError> {
        let session = OnnxSession::load(model, config)?;
        session.expect_signature("pyannote segmentation", &["x"], &["y"])?;
        let meta = session.custom_metadata()?;
        let sha = crate::runtime::file_sha256(model)
            .map_err(|e| OnnxError::Model(format!("{}: {e}", model.display())))?;
        let info = SegmenterInfo::pyannote_segmentation_3_0(format!(
            "{SEGMENTATION_MODEL}@{}",
            &sha[..12]
        ));
        let expected = [
            ("sample_rate", 16_000),
            ("window_size", info.window),
            ("receptive_field_shift", info.frame_step),
            ("receptive_field_size", info.frame_duration),
            ("num_speakers", info.max_local_speakers),
            ("powerset_max_classes", 2),
            ("num_classes", 7),
        ];
        for (key, want) in expected {
            let got = meta_usize(&meta, key)?;
            if got != want {
                return Err(OnnxError::Signature(format!(
                    "segmentation metadata `{key}` is {got}, expected {want}"
                )));
            }
        }
        Ok(PyannoteSegmenter {
            session,
            info,
            powerset: Powerset::new(3, 2),
            max_batch: max_batch.max(1),
        })
    }

    /// The provider requested and where ONNX Runtime placed the nodes.
    #[must_use]
    pub fn backend(&self) -> &SessionBackend {
        self.session.backend()
    }

    fn run(&mut self, windows: &[&[Sample]]) -> Result<Vec<LocalActivity>, DiarError> {
        let w = self.info.window;
        let mut input = Vec::with_capacity(windows.len() * w);
        for win in windows {
            input.extend_from_slice(win);
        }
        let x = Tensor::from_array(([windows.len(), 1, w], input))
            .map_err(|e| DiarError::Backend(e.to_string()))?;
        let outputs = self
            .session
            .session
            .run(ort::inputs!["x" => x])
            .map_err(|e| DiarError::Backend(e.to_string()))?;
        let (shape, y) = outputs["y"]
            .try_extract_tensor::<f32>()
            .map_err(|e| DiarError::Backend(e.to_string()))?;
        let classes = self.powerset.classes().len();
        let dims: Vec<i64> = shape.iter().copied().collect();
        if dims
            != [
                windows.len() as i64,
                self.info.frames as i64,
                classes as i64,
            ]
        {
            return Err(DiarError::Shape(format!(
                "segmentation output {dims:?}, expected [{}, {}, {classes}]",
                windows.len(),
                self.info.frames
            )));
        }
        y.chunks(self.info.frames * classes)
            .map(|scores| self.powerset.to_multilabel(scores))
            .collect()
    }
}

impl SpeakerSegmenter for PyannoteSegmenter {
    fn info(&self) -> &SegmenterInfo {
        &self.info
    }

    fn segment(
        &mut self,
        windows: &[&[Sample]],
        cancel: &CancelToken,
    ) -> Result<Vec<LocalActivity>, DiarError> {
        if let Some(bad) = windows.iter().find(|w| w.len() != self.info.window) {
            return Err(DiarError::Config(format!(
                "segmentation windows must have {} samples, got {}",
                self.info.window,
                bad.len()
            )));
        }
        let mut out = Vec::with_capacity(windows.len());
        for batch in windows.chunks(self.max_batch) {
            if cancel.is_cancelled() {
                return Err(DiarError::Cancelled);
            }
            out.extend(self.run(batch)?);
        }
        Ok(out)
    }
}
