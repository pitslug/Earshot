//! Silero VAD v6 ([`SileroVad`]) implementing the core [`Vad`] trait.
//!
//! Reference: snakers4/silero-vad v6.2.3, `src/silero_vad/utils_vad.py`, `OnnxWrapper.__call__`.
//! At 16 kHz the model takes 512 new samples per call (`num_samples = 512`) with the last
//! 64 samples of the previous call prepended (`context_size = 64`), so the `input` tensor is
//! `[1, 576]`; the recurrent `state` is `[2, 1, 128]` and `sr` is an int64 scalar. Output
//! `output` is the speech probability `[1, 1]`, `stateN` the next state. The context and
//! state start as zeros and are cleared by [`Vad::reset`] (`reset_states`).

use std::path::Path;

use earshot_core::asr::{AsrError, Vad};
use earshot_core::audio::{PIPELINE_RATE, Sample};
use ort::value::Tensor;

use crate::runtime::{OnnxError, OnnxSession, SessionBackend, SessionConfig, runtime_err};

/// New samples per call at 16 kHz (`utils_vad.py`: `num_samples = 512 if sr == 16000`).
pub const SILERO_FRAME: usize = 512;
/// Samples of the previous call prepended to each input (`context_size = 64 if sr == 16000`).
pub const SILERO_CONTEXT: usize = 64;
/// Size of the recurrent state (`[2, batch, 128]`).
const STATE_LEN: usize = 2 * 128;

/// Silero VAD v6 on ONNX Runtime: stateful, 16 kHz, 512-sample frames.
#[derive(Debug)]
pub struct SileroVad {
    session: OnnxSession,
    state: Vec<f32>,
    context: Vec<f32>,
    /// `silero-vad@<first 12 hex digits of the model file's SHA-256>` ([`Vad::id`]).
    id: String,
}

impl SileroVad {
    /// Loads `silero_vad.onnx` (models manifest id `silero-vad`).
    ///
    /// # Errors
    /// [`OnnxError::Model`] if the file is missing or invalid, [`OnnxError::Signature`] if it
    /// is not a Silero v5/v6 model, provider errors as from [`OnnxSession::load`].
    pub fn load(model: &Path, config: &SessionConfig) -> Result<Self, OnnxError> {
        let session = OnnxSession::load(model, config)?;
        session.expect_signature(
            "Silero VAD",
            &["input", "state", "sr"],
            &["output", "stateN"],
        )?;
        let sha = crate::runtime::file_sha256(model)
            .map_err(|e| OnnxError::Model(format!("{}: {e}", model.display())))?;
        Ok(SileroVad {
            session,
            state: vec![0.0; STATE_LEN],
            context: vec![0.0; SILERO_CONTEXT],
            id: format!("silero-vad@{}", &sha[..12]),
        })
    }

    /// The provider requested and where ONNX Runtime placed the nodes.
    #[must_use]
    pub fn backend(&self) -> &SessionBackend {
        self.session.backend()
    }

    fn run(&mut self, frame: &[Sample]) -> Result<f32, OnnxError> {
        let mut input = Vec::with_capacity(SILERO_CONTEXT + SILERO_FRAME);
        input.extend_from_slice(&self.context);
        input.extend_from_slice(frame);
        let input = Tensor::from_array(([1usize, SILERO_CONTEXT + SILERO_FRAME], input))
            .map_err(runtime_err)?;
        let state =
            Tensor::from_array(([2usize, 1, 128], self.state.clone())).map_err(runtime_err)?;
        let sr =
            Tensor::from_array(((), vec![i64::from(PIPELINE_RATE.hz())])).map_err(runtime_err)?;
        let outputs = self
            .session
            .session
            .run(ort::inputs!["input" => input, "state" => state, "sr" => sr])
            .map_err(runtime_err)?;
        let (_, prob) = outputs["output"]
            .try_extract_tensor::<f32>()
            .map_err(runtime_err)?;
        let (_, next) = outputs["stateN"]
            .try_extract_tensor::<f32>()
            .map_err(runtime_err)?;
        if prob.len() != 1 || next.len() != STATE_LEN {
            return Err(OnnxError::Signature(format!(
                "Silero VAD returned {} probabilities and a state of {} values",
                prob.len(),
                next.len()
            )));
        }
        let p = prob[0];
        self.state.copy_from_slice(next);
        self.context
            .copy_from_slice(&frame[SILERO_FRAME - SILERO_CONTEXT..]);
        Ok(p)
    }
}

impl Vad for SileroVad {
    fn id(&self) -> String {
        self.id.clone()
    }

    fn frame_len(&self) -> usize {
        SILERO_FRAME
    }

    fn speech_probability(&mut self, frame: &[Sample]) -> Result<f32, AsrError> {
        if frame.len() != SILERO_FRAME {
            return Err(AsrError::Config(format!(
                "Silero VAD frame must be {SILERO_FRAME} samples, got {}",
                frame.len()
            )));
        }
        self.run(frame)
            .map_err(|e| AsrError::Backend(e.to_string()))
    }

    fn reset(&mut self) {
        self.state.fill(0.0);
        self.context.fill(0.0);
    }
}
