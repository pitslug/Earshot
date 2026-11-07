//! NVIDIA Parakeet on ONNX Runtime behind the core [`Transcriber`]: [`ParakeetCtc`], the CTC
//! branch of parakeet-tdt_ctc-110m (T6.10e1), and [`ParakeetTdt`], the TDT models (the 110M
//! TDT branch, 0.6B v2 and v3; T6.10e2).
//!
//! The model is the sherpa-onnx export (`scripts/nemo/fast-conformer-hybrid-transducer-ctc/
//! export-onnx-ctc-non-streaming.py`, "Only the CTC branch is exported"): input `audio_signal`
//! `[1, 80, T]` NeMo log-mel features ([`NemoMel`]) and `length` `[1]` (valid frames), output
//! `logprobs` `[1, T', vocab + 1]` (log-softmax), one frame per 80 ms (8x subsampling; valid
//! output frames `(valid - 1) / 8 + 1`, onnx-asr `asr.py`). Decoding is greedy CTC with word
//! timings in core ([`earshot_core::asr::ctc_greedy`], [`earshot_core::asr::words_from_tokens`]).
//!
//! Deviations from a whisper [`Transcriber`], reported rather than hidden: English only (another
//! requested language is an error); no prompt (ignored); decoding is always greedy CTC whatever
//! [`earshot_core::asr::DecodingOptions`] asks (beam search is not part of this backend); inputs
//! longer than [`MAX_CHUNK`] are decoded in consecutive chunks without overlap (callers pass VAD
//! pieces or chunker windows of at most 30 s, so this only affects whole-track calls).
//!
//! [`OnnxNemoMel`] runs a NeMo preprocessor exported to ONNX (onnx-asr's `nemo128.onnx`); it
//! exists as the reference for [`NemoMel`] in the tests.

use std::path::Path;
use std::time::Duration;

use earshot_core::asr::{
    AsrError, BackendInfo, BackendKind, CancelToken, Language, RawSegment, TdtConfig, TdtModel,
    TranscribeOptions, Transcriber, Transcription, Vocab, check_pipeline_rate, ctc_greedy,
    tdt_greedy, words_from_tokens,
};
use earshot_core::audio::{MonoBuffer, PIPELINE_RATE};
use ort::value::Tensor;

use crate::nemo_mel::NemoMel;
use crate::runtime::{OnnxError, OnnxSession, SessionBackend, SessionConfig, runtime_err};

/// Encoder frame (FastConformer 8x subsampling of 10 ms frames).
pub const FRAME: Duration = Duration::from_millis(80);
/// Longest audio decoded in one model call (module docs).
pub const MAX_CHUNK: Duration = Duration::from_secs(30);
/// Model name reported in [`BackendInfo::model`].
pub const MODEL_NAME: &str = "parakeet-tdt_ctc-110m (CTC)";
/// The documented partition of an NPU-converted (QDQ) Parakeet encoder on the QNN EP: the
/// only nodes left on the CPU are the graph's input `QuantizeLinear` (`audio_signal`, float
/// to the 16-bit activation type) and output `DequantizeLinear` (`outputs` back to float),
/// the conversions at the boundary between the application's float tensors and the
/// quantised graph. Recorded on the HTP simulator (2026-10-24, plugin 2.7.0): everything else
/// is one fused QNN node.
pub const QNN_ENCODER_CPU_OPS: [(&str, usize); 2] =
    [("QuantizeLinear", 1), ("DequantizeLinear", 1)];

/// The NPU partition of a Parakeet TDT model's sessions ([`ParakeetTdt::backends`]: encoder,
/// prediction network, joint): the encoder on the QNN EP with only [`QNN_ENCODER_CPU_OPS`]
/// left on the CPU, the two step sessions wholly on the CPU. The one rule for the runtime
/// (`earshot_runtime::backends::check_qnn_parakeet`) and the `qnn_etw_probe` example.
///
/// # Errors
/// A message naming the session and placement that differ.
pub fn check_qnn_partition(sessions: &[&SessionBackend]) -> Result<(), String> {
    let [encoder, steps @ ..] = sessions else {
        return Err("no ONNX session to verify".into());
    };
    if steps.len() != 2 {
        return Err(format!(
            "a Parakeet TDT model has three sessions, {} were given",
            sessions.len()
        ));
    }
    encoder
        .check_partition_ops(&QNN_ENCODER_CPU_OPS)
        .map_err(|e| format!("the encoder did not run on Qnn: {e}"))?;
    if let Some(b) = steps.iter().find(|b| b.kind() != Some(BackendKind::Cpu)) {
        return Err(format!(
            "ONNX Runtime did not run the model on {:?} ({b})",
            BackendKind::Cpu
        ));
    }
    Ok(())
}

/// Feature frames of one [`MAX_CHUNK`] piece (`1 + 480_000 / 160`): the fixed time
/// dimension of an encoder converted for the NPU (T8.3; Qualcomm's HTP backend needs static
/// shapes), matching the piece planner's 30 s `max_piece`.
pub const STATIC_FRAMES: usize = 3001;

/// The encoder input of a static-shape encoder for `samples` (at most one [`MAX_CHUNK`]):
/// the [`NemoMel`] features of `samples`, exactly as for a dynamic encoder, followed by zero
/// frames up to [`STATIC_FRAMES`] (`[n_mels][STATIC_FRAMES]` row-major), and the valid frame
/// count for the encoder's `length` input. The features are computed on the piece's own
/// length, so the per-feature normalisation never sees the padding, and the encoder masks
/// everything past `length` (NeMo's `ConformerEncoder` length masks), so the padding does not
/// change the valid output frames.
///
/// # Errors
/// [`OnnxError::Signature`] for more than one piece of audio.
pub fn static_features(mel: &NemoMel, samples: &[f32]) -> Result<(Vec<f32>, usize), OnnxError> {
    let piece = PIPELINE_RATE.frames_for(MAX_CHUNK);
    if samples.len() > piece {
        return Err(OnnxError::Signature(format!(
            "a static-shape encoder takes at most {piece} samples (one piece), {} were given",
            samples.len()
        )));
    }
    let (frames, valid) = NemoMel::frames(samples.len());
    let n_mels = mel.n_mels();
    let dynamic = mel.compute(samples);
    let mut out = vec![0.0f32; n_mels * STATIC_FRAMES];
    for m in 0..n_mels {
        out[m * STATIC_FRAMES..m * STATIC_FRAMES + frames]
            .copy_from_slice(&dynamic[m * frames..(m + 1) * frames]);
    }
    Ok((out, valid))
}

/// The fixed time dimension of an encoder's `audio_signal` input `shape` for `n_mels`
/// features: `None` when the time is dynamic (the float exports), [`STATIC_FRAMES`] for an
/// encoder converted for the NPU.
///
/// # Errors
/// [`OnnxError::Signature`] for any other fixed shape.
fn static_input(shape: Option<Vec<i64>>, n_mels: usize) -> Result<Option<usize>, OnnxError> {
    let Some(dims) = shape else {
        return Ok(None);
    };
    let want = |d: i64, n: usize| usize::try_from(d).is_ok_and(|d| d == n);
    match dims[..] {
        [_, _, t] if t < 0 => Ok(None),
        [b, m, t] if want(b, 1) && want(m, n_mels) && want(t, STATIC_FRAMES) => {
            Ok(Some(STATIC_FRAMES))
        }
        _ => Err(OnnxError::Signature(format!(
            "Parakeet TDT encoder input {dims:?}: a static encoder takes [1, {n_mels}, {STATIC_FRAMES}]"
        ))),
    }
}

/// The 110M Parakeet CTC model (module docs).
#[derive(Debug)]
pub struct ParakeetCtc {
    session: OnnxSession,
    mel: NemoMel,
    vocab: Vocab,
}

impl ParakeetCtc {
    /// Loads `model.onnx` and `tokens.txt` from `dir` (manifest ids
    /// `parakeet-tdt_ctc-110m-ctc` and `-tokens`).
    ///
    /// # Errors
    /// [`OnnxError::Model`] for missing or invalid files, [`OnnxError::Signature`] if the model
    /// is not a NeMo CTC export matching the vocabulary, provider errors as from
    /// [`OnnxSession::load`].
    pub fn load(dir: &Path, config: &SessionConfig) -> Result<Self, OnnxError> {
        let tokens = dir.join("tokens.txt");
        let text = std::fs::read_to_string(&tokens).map_err(|e| {
            OnnxError::Model(format!(
                "{}: {e}; run `cargo xtask fetch-models --set eval`",
                tokens.display()
            ))
        })?;
        let vocab = Vocab::parse(&text).map_err(|e| OnnxError::Model(e.to_string()))?;
        let session = OnnxSession::load(&dir.join("model.onnx"), config)?;
        session.expect_signature("Parakeet CTC", &["audio_signal", "length"], &["logprobs"])?;
        let meta = session.custom_metadata()?;
        let want = |key: &str, value: &str| -> Result<(), OnnxError> {
            match meta.get(key) {
                Some(v) if v == value => Ok(()),
                other => Err(OnnxError::Signature(format!(
                    "Parakeet CTC: metadata {key} is {other:?}, expected {value}"
                ))),
            }
        };
        want("normalize_type", "per_feature")?;
        want("subsampling_factor", "8")?;
        want("vocab_size", &(vocab.len() - 1).to_string())?;
        if vocab.blank() != vocab.len() - 1 {
            return Err(OnnxError::Signature(
                "Parakeet CTC: the blank must be the last token".into(),
            ));
        }
        let n_mels = meta
            .get("feat_dim")
            .map_or(Ok(80), |v| v.parse::<usize>())
            .map_err(|e| OnnxError::Signature(format!("Parakeet CTC: feat_dim: {e}")))?;
        Ok(Self {
            session,
            mel: NemoMel::new(n_mels),
            vocab,
        })
    }

    /// Where ONNX Runtime placed the model's nodes (the backend evidence).
    #[must_use]
    pub fn backends(&self) -> Vec<&SessionBackend> {
        vec![self.session.backend()]
    }

    fn info(&self) -> BackendInfo {
        let kind = self.session.backend().kind().unwrap_or(BackendKind::Cpu);
        BackendInfo {
            kind,
            device: self.session.backend().device(),
            model: MODEL_NAME.to_owned(),
        }
    }

    /// Log-probabilities of one chunk: `(values, valid frames, classes)`.
    fn log_probs(&mut self, samples: &[f32]) -> Result<(Vec<f32>, usize, usize), OnnxError> {
        let features = self.mel.compute(samples);
        let (frames, valid) = NemoMel::frames(samples.len());
        let n_mels = self.mel.n_mels();
        let audio =
            Tensor::from_array(([1usize, n_mels, frames], features)).map_err(runtime_err)?;
        let length = Tensor::from_array(([1usize], vec![i64::try_from(valid).unwrap_or(i64::MAX)]))
            .map_err(runtime_err)?;
        let outputs = self
            .session
            .session
            .run(ort::inputs!["audio_signal" => audio, "length" => length])
            .map_err(runtime_err)?;
        let (shape, values) = outputs["logprobs"]
            .try_extract_tensor::<f32>()
            .map_err(runtime_err)?;
        let dims: Vec<usize> = shape
            .iter()
            .map(|d| usize::try_from(*d).unwrap_or(0))
            .collect();
        let [1, t, classes] = dims[..] else {
            return Err(OnnxError::Signature(format!(
                "Parakeet CTC: logprobs shape {dims:?}"
            )));
        };
        if classes != self.vocab.len() {
            return Err(OnnxError::Signature(format!(
                "Parakeet CTC: {classes} classes for a vocabulary of {}",
                self.vocab.len()
            )));
        }
        let out_valid = if valid == 0 {
            0
        } else {
            ((valid - 1) / 8 + 1).min(t)
        };
        Ok((values[..out_valid * classes].to_vec(), out_valid, classes))
    }
}

fn english(language: &Language) -> bool {
    match language {
        Language::Auto => true,
        Language::Code(c) => c.eq_ignore_ascii_case("en"),
    }
}

impl Transcriber for ParakeetCtc {
    fn transcribe(
        &mut self,
        audio: &MonoBuffer,
        options: &TranscribeOptions,
        cancel: &CancelToken,
    ) -> Result<Transcription, AsrError> {
        check_pipeline_rate(audio)?;
        if !english(&options.language) {
            return Err(AsrError::Config(format!(
                "{MODEL_NAME} is English only, {:?} was requested",
                options.language
            )));
        }
        let chunk = PIPELINE_RATE.frames_for(MAX_CHUNK);
        let mut segments = Vec::new();
        for (i, piece) in audio.samples.chunks(chunk).enumerate() {
            if cancel.is_cancelled() {
                return Err(AsrError::Cancelled);
            }
            let (lp, frames, classes) = self
                .log_probs(piece)
                .map_err(|e| AsrError::Backend(e.to_string()))?;
            let tokens = ctc_greedy(&lp, frames, classes, self.vocab.blank())?;
            let offset = earshot_core::asr::MeetingTime::from_pipeline_frames(
                u64::try_from(i * chunk).unwrap_or(u64::MAX),
            );
            let words: Vec<_> = words_from_tokens(&tokens, &self.vocab, FRAME)
                .into_iter()
                .map(|w| w.shifted(offset))
                .collect();
            if !words.is_empty() {
                segments.push(RawSegment::from_words(words));
            }
        }
        if cancel.is_cancelled() {
            return Err(AsrError::Cancelled);
        }
        Ok(Transcription {
            segments,
            language: Some("en".to_owned()),
            backend: self.info(),
        })
    }

    fn backend_info(&self) -> BackendInfo {
        self.info()
    }
}

/// A NeMo mel preprocessor exported to ONNX (onnx-asr `nemo80.onnx` / `nemo128.onnx`: inputs
/// `waveforms` `[1, N]` and `waveforms_lens` `[1]`, outputs `features` `[1, n_mels, T]` and
/// `features_lens`); the reference for [`NemoMel`].
#[derive(Debug)]
pub struct OnnxNemoMel {
    session: OnnxSession,
}

impl OnnxNemoMel {
    /// Loads the preprocessor at `model`.
    ///
    /// # Errors
    /// As [`OnnxSession::load`]; [`OnnxError::Signature`] for another graph.
    pub fn load(model: &Path, config: &SessionConfig) -> Result<Self, OnnxError> {
        let session = OnnxSession::load(model, config)?;
        session.expect_signature(
            "NeMo preprocessor",
            &["waveforms", "waveforms_lens"],
            &["features", "features_lens"],
        )?;
        Ok(Self { session })
    }

    /// Features of `samples`: `(values [n_mels][T], n_mels, T, valid frames)`.
    ///
    /// # Errors
    /// Runtime errors; [`OnnxError::Signature`] for an unexpected output shape.
    pub fn compute(
        &mut self,
        samples: &[f32],
    ) -> Result<(Vec<f32>, usize, usize, usize), OnnxError> {
        let wave =
            Tensor::from_array(([1usize, samples.len()], samples.to_vec())).map_err(runtime_err)?;
        let lens = Tensor::from_array(([1usize], vec![i64::try_from(samples.len()).unwrap_or(0)]))
            .map_err(runtime_err)?;
        let outputs = self
            .session
            .session
            .run(ort::inputs!["waveforms" => wave, "waveforms_lens" => lens])
            .map_err(runtime_err)?;
        let (shape, values) = outputs["features"]
            .try_extract_tensor::<f32>()
            .map_err(runtime_err)?;
        let dims: Vec<usize> = shape
            .iter()
            .map(|d| usize::try_from(*d).unwrap_or(0))
            .collect();
        let [1, n_mels, t] = dims[..] else {
            return Err(OnnxError::Signature(format!(
                "NeMo preprocessor: shape {dims:?}"
            )));
        };
        let (_, lens) = outputs["features_lens"]
            .try_extract_tensor::<i64>()
            .map_err(runtime_err)?;
        let valid = lens.first().map_or(0, |v| usize::try_from(*v).unwrap_or(0));
        Ok((values.to_vec(), n_mels, t, valid))
    }
}

/// Parakeet TDT models as exported by sherpa-onnx (`scripts/nemo/parakeet-tdt-0.6b-v2/
/// export_onnx.py` and `fast-conformer-hybrid-transducer-ctc/export-onnx-transducer-non-
/// streaming.py`): `encoder.onnx` (`audio_signal` `[1, n_mels, T]`, `length` -> `outputs`
/// `[1, D, T']`, `encoded_lengths`), `decoder.onnx` (`targets` int32 `[1, 1]`,
/// `target_length` int32 `[1]` and two LSTM states `[layers, 1, hidden]` -> output
/// `[1, hidden, 1]`, length and the next states; the state tensors are addressed by position
/// because their names are generated by the export), `joiner.onnx` (`encoder_outputs`
/// `[1, D, 1]`, `decoder_outputs` `[1, hidden, 1]` -> `outputs`: vocabulary logits then five
/// duration logits). Greedy TDT decoding is core's [`earshot_core::asr::tdt_greedy`].
///
/// The English models (110M TDT branch, 0.6B v2) refuse another requested language; the
/// multilingual 0.6B v3 detects the language itself, so a requested language cannot be forced
/// and is not reported back (`Transcription::language` is `None`).
#[derive(Debug)]
pub struct ParakeetTdt {
    encoder: OnnxSession,
    decoder: OnnxSession,
    joiner: OnnxSession,
    mel: NemoMel,
    vocab: Vocab,
    layers: usize,
    hidden: usize,
    decoder_inputs: Vec<String>,
    decoder_outputs: Vec<String>,
    name: String,
    english_only: bool,
    /// `Some(STATIC_FRAMES)` for an encoder with a fixed input shape (converted for the NPU).
    static_frames: Option<usize>,
}

impl ParakeetTdt {
    /// Loads `encoder.onnx` (with `encoder.weights` when the export has external data),
    /// `decoder.onnx`, `joiner.onnx` and `tokens.txt` from `dir`; `name` is reported in
    /// [`BackendInfo::model`].
    ///
    /// # Errors
    /// [`OnnxError::Model`] for missing or invalid files, [`OnnxError::Signature`] for graphs or
    /// metadata that are not a NeMo TDT export, provider errors as from [`OnnxSession::load`].
    pub fn load(
        dir: &Path,
        config: &SessionConfig,
        name: &str,
        english_only: bool,
    ) -> Result<Self, OnnxError> {
        Self::load_split(dir, config, config, name, english_only)
    }

    /// [`ParakeetTdt::load`] with the encoder on `encoder_config` and the prediction network
    /// and joint (one small call per decoded token) on `step_config`: the NPU partition of
    /// T8.3, where the encoder runs on QNN and the per-token steps on the CPU. An encoder
    /// whose `audio_signal` input has a fixed time dimension (a model converted for the NPU)
    /// must take exactly [`STATIC_FRAMES`] frames; each piece is then padded to it
    /// ([`static_features`]).
    ///
    /// # Errors
    /// As [`ParakeetTdt::load`]; [`OnnxError::Signature`] for a fixed shape other than
    /// `[1, n_mels, STATIC_FRAMES]`.
    pub fn load_split(
        dir: &Path,
        encoder_config: &SessionConfig,
        step_config: &SessionConfig,
        name: &str,
        english_only: bool,
    ) -> Result<Self, OnnxError> {
        let tokens = dir.join("tokens.txt");
        let text = std::fs::read_to_string(&tokens).map_err(|e| {
            OnnxError::Model(format!(
                "{}: {e}; run `cargo xtask fetch-models --set ci` (the default Parakeet \
                 models) or `--set choice` (Parakeet 0.6B v3)",
                tokens.display()
            ))
        })?;
        let vocab = Vocab::parse(&text).map_err(|e| OnnxError::Model(e.to_string()))?;
        let encoder = OnnxSession::load(&dir.join("encoder.onnx"), encoder_config)?;
        let decoder = OnnxSession::load(&dir.join("decoder.onnx"), step_config)?;
        let joiner = OnnxSession::load(&dir.join("joiner.onnx"), step_config)?;
        encoder.expect_signature(
            "Parakeet TDT encoder",
            &["audio_signal", "length"],
            &["outputs", "encoded_lengths"],
        )?;
        joiner.expect_signature(
            "Parakeet TDT joiner",
            &["encoder_outputs", "decoder_outputs"],
            &["outputs"],
        )?;
        let (decoder_inputs, decoder_outputs) = (decoder.input_names(), decoder.output_names());
        if decoder_inputs.len() != 4
            || decoder_inputs[..2] != ["targets", "target_length"]
            || decoder_outputs.len() != 4
        {
            return Err(OnnxError::Signature(format!(
                "Parakeet TDT decoder: inputs {decoder_inputs:?}, outputs {decoder_outputs:?}"
            )));
        }
        let meta = encoder.custom_metadata()?;
        let get = |key: &str| -> Result<usize, OnnxError> {
            meta.get(key)
                .and_then(|v| v.parse().ok())
                .ok_or_else(|| OnnxError::Signature(format!("Parakeet TDT: metadata {key}")))
        };
        if meta.get("normalize_type").map(String::as_str) != Some("per_feature")
            || get("subsampling_factor")? != 8
            || get("vocab_size")? + 1 != vocab.len()
            || vocab.blank() + 1 != vocab.len()
        {
            return Err(OnnxError::Signature(format!(
                "Parakeet TDT: unexpected metadata {meta:?} for {} tokens",
                vocab.len()
            )));
        }
        let n_mels = match meta.get("feat_dim") {
            None => 80,
            Some(v) => v
                .parse()
                .map_err(|e| OnnxError::Signature(format!("Parakeet TDT: feat_dim: {e}")))?,
        };
        let static_frames = static_input(encoder.input_shape("audio_signal"), n_mels)?;
        Ok(Self {
            encoder,
            decoder,
            joiner,
            mel: NemoMel::new(n_mels),
            vocab,
            layers: get("pred_rnn_layers")?,
            hidden: get("pred_hidden")?,
            decoder_inputs,
            decoder_outputs,
            name: name.to_owned(),
            english_only,
            static_frames,
        })
    }

    /// Whether the encoder takes a fixed [`STATIC_FRAMES`] input (a model converted for the
    /// NPU).
    #[must_use]
    pub fn is_static(&self) -> bool {
        self.static_frames.is_some()
    }

    /// Where ONNX Runtime placed the nodes of the encoder, decoder and joiner (the backend
    /// evidence).
    #[must_use]
    pub fn backends(&self) -> Vec<&SessionBackend> {
        vec![
            self.encoder.backend(),
            self.decoder.backend(),
            self.joiner.backend(),
        ]
    }

    fn info(&self) -> BackendInfo {
        BackendInfo {
            kind: self.encoder.backend().kind().unwrap_or(BackendKind::Cpu),
            device: self.encoder.backend().device(),
            model: self.name.clone(),
        }
    }

    /// Encoder frames of one chunk: `(values [T'][D], T', D)`.
    fn encode(&mut self, samples: &[f32]) -> Result<(Vec<f32>, usize, usize), OnnxError> {
        let (features, frames, valid) = if self.static_frames.is_some() {
            let (features, valid) = static_features(&self.mel, samples)?;
            (features, STATIC_FRAMES, valid)
        } else {
            let (frames, valid) = NemoMel::frames(samples.len());
            (self.mel.compute(samples), frames, valid)
        };
        let audio = Tensor::from_array(([1usize, self.mel.n_mels(), frames], features))
            .map_err(runtime_err)?;
        let length = Tensor::from_array(([1usize], vec![i64::try_from(valid).unwrap_or(0)]))
            .map_err(runtime_err)?;
        let outputs = self
            .encoder
            .session
            .run(ort::inputs!["audio_signal" => audio, "length" => length])
            .map_err(runtime_err)?;
        let (shape, values) = outputs["outputs"]
            .try_extract_tensor::<f32>()
            .map_err(runtime_err)?;
        let dims: Vec<usize> = shape
            .iter()
            .map(|d| usize::try_from(*d).unwrap_or(0))
            .collect();
        let [1, d, t] = dims[..] else {
            return Err(OnnxError::Signature(format!("TDT encoder output {dims:?}")));
        };
        let (_, lens) = outputs["encoded_lengths"]
            .try_extract_tensor::<i64>()
            .map_err(runtime_err)?;
        let valid_t = lens
            .first()
            .map_or(t, |v| usize::try_from(*v).unwrap_or(0).min(t));
        // [D][T] -> [T][D] for per-frame joint calls.
        let mut out = vec![0.0f32; valid_t * d];
        for f in 0..valid_t {
            for k in 0..d {
                out[f * d + k] = values[k * t + f];
            }
        }
        Ok((out, valid_t, d))
    }
}

/// One chunk's encoder frames plus the model, as a core [`TdtModel`].
struct TdtRun<'a> {
    model: &'a mut ParakeetTdt,
    frames: &'a [f32],
    dim: usize,
}

type LstmState = (Vec<f32>, Vec<f32>);

fn backend_err(e: OnnxError) -> AsrError {
    AsrError::Backend(e.to_string())
}

impl TdtModel for TdtRun<'_> {
    type State = LstmState;

    fn initial_state(&mut self) -> Result<LstmState, AsrError> {
        let n = self.model.layers * self.model.hidden;
        Ok((vec![0.0; n], vec![0.0; n]))
    }

    fn predict(
        &mut self,
        token: usize,
        state: &LstmState,
    ) -> Result<(Vec<f32>, LstmState), AsrError> {
        let m = &mut *self.model;
        let shape = [m.layers, 1usize, m.hidden];
        let targets = Tensor::from_array(([1usize, 1], vec![i32::try_from(token).unwrap_or(0)]))
            .map_err(runtime_err)
            .map_err(backend_err)?;
        let len = Tensor::from_array(([1usize], vec![1i32]))
            .map_err(runtime_err)
            .map_err(backend_err)?;
        let h = Tensor::from_array((shape, state.0.clone()))
            .map_err(runtime_err)
            .map_err(backend_err)?;
        let c = Tensor::from_array((shape, state.1.clone()))
            .map_err(runtime_err)
            .map_err(backend_err)?;
        let names = m.decoder_inputs.clone();
        let outputs = m
            .decoder
            .session
            .run(ort::inputs![
                names[0].as_str() => targets,
                names[1].as_str() => len,
                names[2].as_str() => h,
                names[3].as_str() => c
            ])
            .map_err(runtime_err)
            .map_err(backend_err)?;
        let get = |i: usize| -> Result<Vec<f32>, AsrError> {
            let (_, v) = outputs[m.decoder_outputs[i].as_str()]
                .try_extract_tensor::<f32>()
                .map_err(runtime_err)
                .map_err(backend_err)?;
            Ok(v.to_vec())
        };
        let out = get(0)?;
        if out.len() != m.hidden {
            return Err(AsrError::Backend(format!(
                "TDT decoder output of {} values, expected {}",
                out.len(),
                m.hidden
            )));
        }
        let next = (get(2)?, get(3)?);
        Ok((out, next))
    }

    fn joint(&mut self, frame: usize, pred: &[f32]) -> Result<Vec<f32>, AsrError> {
        let m = &mut *self.model;
        let f = self.frames[frame * self.dim..(frame + 1) * self.dim].to_vec();
        let enc = Tensor::from_array(([1usize, self.dim, 1], f))
            .map_err(runtime_err)
            .map_err(backend_err)?;
        let dec = Tensor::from_array(([1usize, m.hidden, 1], pred.to_vec()))
            .map_err(runtime_err)
            .map_err(backend_err)?;
        let outputs = m
            .joiner
            .session
            .run(ort::inputs!["encoder_outputs" => enc, "decoder_outputs" => dec])
            .map_err(runtime_err)
            .map_err(backend_err)?;
        let (_, v) = outputs["outputs"]
            .try_extract_tensor::<f32>()
            .map_err(runtime_err)
            .map_err(backend_err)?;
        Ok(v.to_vec())
    }
}

impl Transcriber for ParakeetTdt {
    fn transcribe(
        &mut self,
        audio: &MonoBuffer,
        options: &TranscribeOptions,
        cancel: &CancelToken,
    ) -> Result<Transcription, AsrError> {
        check_pipeline_rate(audio)?;
        if self.english_only && !english(&options.language) {
            return Err(AsrError::Config(format!(
                "{} is English only, {:?} was requested",
                self.name, options.language
            )));
        }
        let chunk = PIPELINE_RATE.frames_for(MAX_CHUNK);
        let config = TdtConfig::parakeet(self.vocab.len(), self.vocab.blank());
        let mut segments = Vec::new();
        for (i, piece) in audio.samples.chunks(chunk).enumerate() {
            if cancel.is_cancelled() {
                return Err(AsrError::Cancelled);
            }
            let (frames, t, dim) = self.encode(piece).map_err(backend_err)?;
            let tokens = tdt_greedy(
                &mut TdtRun {
                    model: self,
                    frames: &frames,
                    dim,
                },
                t,
                &config,
                cancel,
            )?;
            let offset = earshot_core::asr::MeetingTime::from_pipeline_frames(
                u64::try_from(i * chunk).unwrap_or(u64::MAX),
            );
            let words: Vec<_> = words_from_tokens(&tokens, &self.vocab, FRAME)
                .into_iter()
                .map(|w| w.shifted(offset))
                .collect();
            if !words.is_empty() {
                segments.push(RawSegment::from_words(words));
            }
        }
        Ok(Transcription {
            segments,
            language: self.english_only.then(|| "en".to_owned()),
            backend: self.info(),
        })
    }

    fn backend_info(&self) -> BackendInfo {
        self.info()
    }

    /// True (T2.5d): on some inputs the 0.6B models' greedy TDT decoder stays on blank
    /// (duration 4) for every frame from some point on, so whole pieces or their tails of
    /// clear speech come back without words. Measured on the LibriSpeech subset streams
    /// (2026-10-25, CPU and CUDA alike): v2 left 17-23 s of speech uncovered in one piece of
    /// test-clean and two of dev-clean (and 2.5 s in a third), v3 failed on the same
    /// test-clean piece; outside those pieces no stretch between words held more than
    /// 0.64 s of VAD speech. The input features match NeMo's front end and the decoder
    /// follows NeMo's greedy rule, and the same audio cut a little differently decodes
    /// fully, so this is the model's own behaviour. The 110M TDT branch did not fail there
    /// but shares the decoder, so every TDT model reports it.
    fn may_skip_speech(&self) -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn placed(lines: &[&str], requested: crate::Provider) -> SessionBackend {
        let mut placement = crate::runtime::Placement::default();
        for l in lines {
            placement.observe(l);
        }
        SessionBackend {
            requested,
            placement,
        }
    }

    #[test]
    fn parakeet_qnn_partition_is_the_encoder_on_qnn_with_its_boundary_and_the_steps_on_the_cpu() {
        use crate::Provider;
        let encoder_lines = [
            " Node(s) placed on [CPUExecutionProvider]. Number of nodes: 2",
            "  QuantizeLinear (audio_signal_QuantizeLinear)",
            "  DequantizeLinear (outputs_DequantizeLinear)",
            " Node(s) placed on [QNNExecutionProvider]. Number of nodes: 1",
            "  QNNExecutionProvider_9_1 (QNNExecutionProvider_QNNExecutionProvider_9_1_0)",
        ];
        let encoder = placed(&encoder_lines, Provider::Qnn);
        let step = placed(
            &["All nodes placed on [CPUExecutionProvider]. Number of nodes: 7"],
            Provider::Cpu,
        );
        assert_eq!(check_qnn_partition(&[&encoder, &step, &step]), Ok(()));
        // Another operator left on the CPU.
        let mut lines = encoder_lines.to_vec();
        lines[0] = " Node(s) placed on [CPUExecutionProvider]. Number of nodes: 3";
        lines.insert(3, "  LayerNormalization (/layers.0/norm)");
        let leaky = placed(&lines, Provider::Qnn);
        let e = check_qnn_partition(&[&leaky, &step, &step]).unwrap_err();
        assert!(e.contains("LayerNormalization"), "{e}");
        // An encoder requested on QNN that ran on the CPU (what the probe printed as `qnn`
        // from the request alone).
        let on_cpu = placed(
            &["All nodes placed on [CPUExecutionProvider]. Number of nodes: 900"],
            Provider::Qnn,
        );
        assert!(check_qnn_partition(&[&on_cpu, &step, &step]).is_err());
        // The steps stay on the CPU; all three sessions are needed.
        assert!(check_qnn_partition(&[&encoder, &encoder, &step]).is_err());
        assert!(check_qnn_partition(&[&encoder]).is_err());
        assert!(check_qnn_partition(&[]).is_err());
    }

    #[test]
    fn parakeet_static_frames_hold_exactly_one_30_s_piece() {
        let piece = PIPELINE_RATE.frames_for(MAX_CHUNK);
        assert_eq!(STATIC_FRAMES, NemoMel::frames(piece).0);
        assert_eq!(STATIC_FRAMES, 3001);
    }

    #[test]
    fn parakeet_static_features_pad_the_dynamic_ones_with_zeros() {
        let mel = NemoMel::new(80);
        let x: Vec<f32> = (0..70_559)
            .map(|i| ((i as f32) * 0.031).sin() * 0.2 + ((i as f32) * 0.0071).cos() * 0.1)
            .collect();
        let dynamic = mel.compute(&x);
        let (frames, valid) = NemoMel::frames(x.len());
        let (fixed, fixed_valid) = static_features(&mel, &x).unwrap();
        assert_eq!(fixed_valid, valid, "the length input is unchanged");
        assert_eq!(fixed.len(), 80 * STATIC_FRAMES);
        for m in 0..80 {
            let row = &fixed[m * STATIC_FRAMES..(m + 1) * STATIC_FRAMES];
            assert_eq!(
                &row[..frames],
                &dynamic[m * frames..(m + 1) * frames],
                "bin {m}"
            );
            assert!(row[frames..].iter().all(|v| *v == 0.0), "bin {m} padding");
        }
    }

    #[test]
    fn parakeet_static_input_is_read_from_the_encoder_shape() {
        // The sherpa-onnx exports: batch and time dynamic.
        assert_eq!(static_input(Some(vec![-1, 128, -1]), 128).unwrap(), None);
        assert_eq!(static_input(None, 80).unwrap(), None);
        // A converted encoder: exactly one 30 s piece.
        assert_eq!(
            static_input(Some(vec![1, 80, 3001]), 80).unwrap(),
            Some(STATIC_FRAMES)
        );
        for bad in [
            vec![1, 80, 1500],
            vec![1, 128, 3001],
            vec![2, 80, 3001],
            vec![1, 80],
        ] {
            assert!(
                matches!(
                    static_input(Some(bad.clone()), 80),
                    Err(OnnxError::Signature(_))
                ),
                "{bad:?}"
            );
        }
    }

    #[test]
    fn parakeet_static_features_refuse_more_than_one_piece() {
        let mel = NemoMel::new(128);
        let piece = PIPELINE_RATE.frames_for(MAX_CHUNK);
        let (full, valid) = static_features(&mel, &vec![0.01; piece]).unwrap();
        assert_eq!(full.len(), 128 * STATIC_FRAMES);
        assert_eq!(valid, STATIC_FRAMES - 1);
        assert!(matches!(
            static_features(&mel, &vec![0.01; piece + 1]),
            Err(OnnxError::Signature(_))
        ));
        let (empty, valid) = static_features(&mel, &[]).unwrap();
        assert_eq!(valid, 0);
        assert!(empty.iter().all(|v| *v == 0.0));
    }
}
