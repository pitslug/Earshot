//! Evaluation only (T12.23, docs/live-previews.md): NVIDIA Nemotron Speech Streaming 0.6B, a
//! cache-aware FastConformer RNNT, as exported to ONNX by the k2-fsa sherpa-onnx project (manifest
//! set `stream-eval`, the 560 ms int8 export). Nothing shipped loads it: the licence is agreed
//! for evaluation only, and the runtime names it only in its evaluation loader.
//!
//! A port of sherpa-onnx v2.0.1's streaming NeMo transducer path, step for step:
//!
//! - **Features** ([`OnlineFbank`]): kaldi-native-fbank v1.22.4's `OnlineFbank` with the options
//!   `OnlineRecognizerTransducerNeMoImpl::PostInit` sets (`sherpa-onnx/csrc/
//!   online-recognizer-transducer-nemo-impl.h`): 16 kHz, 25 ms frames every 10 ms, `snip_edges`
//!   false (frames centred, the edges reflected), no dither, no DC removal, pre-emphasis 0.97, a
//!   periodic Hann window, a 512-point FFT, power spectrum, librosa's Slaney mel filters (`norm =
//!   "slaney"`) from 0 to 8000 Hz with `feat_dim` bins, `log(max(energy, FLT_EPSILON))`. The
//!   export's `normalize_type` is empty, so no normalisation follows (checked at load).
//! - **Encoder** ([`NemotronStreaming::decode_chunk`]): `window_size` feature frames starting at
//!   the processed count, which then moves by `chunk_shift`; a chunk is ready only when more than
//!   `processed + window_size` frames exist (`IsReady`). The three cache tensors carry over from
//!   chunk to chunk, zero at the start (`online-transducer-nemo-model.cc`).
//! - **Greedy RNNT** (`online-transducer-greedy-search-nemo-decoder.cc`): per encoder frame up to
//!   10 symbols, the first maximum of the joiner's logits, blank the last class; the prediction
//!   network starts from blank, and its output and LSTM state carry over between chunks.
//! - **Text**: tokens grouped into words at SentencePiece's U+2581
//!   ([`earshot_core::asr::words_from_tokens`]), each token at its encoder frame (80 ms).
//!
//! The `nemotron_probe` example checks the words against sherpa-onnx's own Python package on the
//! smoke clips.

use std::collections::VecDeque;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use earshot_core::asr::{BackendInfo, BackendKind, Word};
use earshot_core::asr::{CtcToken, Vocab, words_from_tokens};
use ort::value::Tensor;
use rustfft::num_complex::Complex32;
use rustfft::{Fft, FftPlanner};

use crate::runtime::{OnnxError, OnnxSession, SessionBackend, SessionConfig, runtime_err};

/// Samples per second the model takes.
pub const SAMPLE_RATE: usize = 16_000;
/// Samples per feature frame (25 ms).
const FRAME_LENGTH: usize = 400;
/// Samples between feature frames (10 ms).
const FRAME_SHIFT: usize = 160;
/// FFT size (the frame rounded up to a power of two).
const FFT_SIZE: usize = 512;
/// kaldi-native-fbank's pre-emphasis coefficient.
const PREEMPH: f32 = 0.97;
/// Encoder frames are this many feature frames (FastConformer's 8x subsampling).
pub const SUBSAMPLING: usize = 8;
/// Most symbols the greedy search emits on one encoder frame (sherpa-onnx's `max_symbols_per_frame`).
const MAX_SYMBOLS_PER_FRAME: usize = 10;
/// One encoder frame.
pub const ENCODER_FRAME: Duration = Duration::from_millis(80);

/// A port of kaldi-native-fbank's online log-mel filterbank with sherpa-onnx's NeMo options
/// (module docs). Frames depend only on the samples, never on how they were pushed.
pub struct OnlineFbank {
    bins: Vec<(usize, Vec<f32>)>,
    window: Vec<f32>,
    fft: Arc<dyn Fft<f32>>,
    /// Samples from `offset` on (the ones later frames still need).
    wave: VecDeque<f32>,
    /// Sample index of `wave[0]`.
    offset: usize,
    /// Every sample received.
    received: usize,
    finished: bool,
    /// Frames computed and not yet popped, from `first_frame` on.
    frames: VecDeque<Vec<f32>>,
    first_frame: usize,
}

impl std::fmt::Debug for OnlineFbank {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OnlineFbank")
            .field("bins", &self.bins.len())
            .field("received", &self.received)
            .field("frames_ready", &self.frames_ready())
            .finish_non_exhaustive()
    }
}

/// Slaney's mel scale (kaldi-native-fbank `MelBanks::MelScaleSlaney`, f32 as there).
fn mel_slaney(hz: f32) -> f32 {
    if hz <= 1000.0 {
        hz * 3.0 / 200.0
    } else {
        15.0 + 14.545_078_f32 * (hz / 1000.0).ln()
    }
}

/// The inverse (`InverseMelScaleSlaney`).
fn hz_slaney(mel: f32) -> f32 {
    if mel <= 15.0 {
        200.0 / 3.0 * mel
    } else {
        1000.0 * ((mel - 15.0) * 0.068_751_78_f32).exp()
    }
}

/// librosa's mel filters as kaldi-native-fbank builds them (`InitLibrosaMelBanks`): per bin the
/// first FFT bin and the weights from there.
fn librosa_bins(num_bins: usize, low: f32, high: f32) -> Vec<(usize, Vec<f32>)> {
    #[allow(clippy::cast_precision_loss)]
    let fft_bin_width = SAMPLE_RATE as f32 / FFT_SIZE as f32;
    let (mel_low, mel_high) = (mel_slaney(low), mel_slaney(high));
    #[allow(clippy::cast_precision_loss)]
    let delta = (mel_high - mel_low) / (num_bins + 1) as f32;
    let mut out = Vec::with_capacity(num_bins);
    for bin in 0..num_bins {
        #[allow(clippy::cast_precision_loss)]
        let b = bin as f32;
        let left = hz_slaney(mel_low + b * delta);
        let center = hz_slaney(mel_low + (b + 1.0) * delta);
        let right = hz_slaney(mel_low + (b + 2.0) * delta);
        let mut first = None;
        let mut weights = Vec::new();
        for i in 0..=FFT_SIZE / 2 {
            #[allow(clippy::cast_precision_loss)]
            let hz = fft_bin_width * i as f32;
            if hz > left && hz < right {
                let mut w = if hz <= center {
                    (hz - left) / (center - left)
                } else {
                    (right - hz) / (right - center)
                };
                w *= 2.0 / (right - left);
                first.get_or_insert(i);
                weights.push(w);
            } else if first.is_some() {
                break;
            }
        }
        out.push((first.unwrap_or(0), weights));
    }
    out
}

/// kaldi-native-fbank's `FirstSampleOfFrame` with `snip_edges` false (may be negative).
fn first_sample(frame: usize) -> i64 {
    let f = i64::try_from(frame).unwrap_or(i64::MAX / 2);
    f * FRAME_SHIFT as i64 + FRAME_SHIFT as i64 / 2 - FRAME_LENGTH as i64 / 2
}

impl OnlineFbank {
    /// A filterbank of `num_bins` mel bins (the encoder's `feat_dim`).
    #[must_use]
    pub fn new(num_bins: usize) -> Self {
        #[allow(clippy::cast_precision_loss)]
        let a = 2.0 * std::f64::consts::PI / FRAME_LENGTH as f64;
        let window = (0..FRAME_LENGTH)
            .map(|i| {
                #[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]
                let w = (0.5 - 0.5 * (a * i as f64).cos()) as f32;
                w
            })
            .collect();
        Self {
            bins: librosa_bins(num_bins, 0.0, 8000.0),
            window,
            fft: FftPlanner::new().plan_fft_forward(FFT_SIZE),
            wave: VecDeque::new(),
            offset: 0,
            received: 0,
            finished: false,
            frames: VecDeque::new(),
            first_frame: 0,
        }
    }

    /// A finished filterbank holding `frames` as they are: checks the decoder apart from this
    /// front end, on another implementation's features (the `nemotron_probe` example).
    #[must_use]
    pub fn finished_with(frames: Vec<Vec<f32>>) -> Self {
        let num_bins = frames.first().map_or(0, Vec::len);
        let mut f = Self::new(num_bins);
        f.finished = true;
        f.frames = frames.into();
        f
    }

    /// Mel bins per frame.
    #[must_use]
    pub fn dim(&self) -> usize {
        self.bins.len()
    }

    /// Frames that exist so far (popped ones included).
    #[must_use]
    pub fn frames_ready(&self) -> usize {
        self.first_frame + self.frames.len()
    }

    /// kaldi-native-fbank's `NumFrames` for what was received.
    fn frames_possible(&self) -> usize {
        let n = (self.received + FRAME_SHIFT / 2) / FRAME_SHIFT;
        if self.finished {
            return n;
        }
        let mut n = n;
        while n > 0 {
            let end = first_sample(n - 1) + FRAME_LENGTH as i64;
            if end <= i64::try_from(self.received).unwrap_or(i64::MAX) {
                break;
            }
            n -= 1;
        }
        n
    }

    /// Adds samples in `[-1, 1]` and computes every frame they complete.
    pub fn accept(&mut self, samples: &[f32]) {
        if self.finished {
            return;
        }
        self.wave.extend(samples.iter().copied());
        self.received += samples.len();
        self.compute();
    }

    /// No more samples: the last frames are computed with the end reflected.
    pub fn input_finished(&mut self) {
        self.finished = true;
        self.compute();
    }

    fn compute(&mut self) {
        let target = self.frames_possible();
        let mut frame = vec![0.0f32; FRAME_LENGTH];
        while self.frames_ready() < target {
            let f = self.frames_ready();
            let start = first_sample(f);
            let wave_start = start - i64::try_from(self.offset).unwrap_or(0);
            let dim = i64::try_from(self.wave.len()).unwrap_or(i64::MAX);
            for (s, slot) in frame.iter_mut().enumerate() {
                let mut i = wave_start + s as i64;
                // Reflection around the beginning or end of what is held (only at the very
                // start of the stream and after the input finished).
                while i < 0 || i >= dim {
                    i = if i < 0 { -i - 1 } else { 2 * dim - 1 - i };
                }
                *slot = self.wave[usize::try_from(i).unwrap_or(0)];
            }
            let features = self.frame_features(&mut frame);
            self.frames.push_back(features);
            // Samples before the next frame's first one are no longer needed (frame 0's
            // reflection reads only samples a later frame also reads).
            let next = first_sample(f + 1).max(0);
            let drop = usize::try_from(next)
                .unwrap_or(0)
                .saturating_sub(self.offset)
                .min(self.wave.len());
            if !self.finished {
                self.wave.drain(..drop);
                self.offset += drop;
            }
        }
    }

    /// One frame's log-mel features (`ProcessWindow` then `FbankComputer::Compute`).
    fn frame_features(&self, frame: &mut [f32]) -> Vec<f32> {
        for i in (1..FRAME_LENGTH).rev() {
            frame[i] -= PREEMPH * frame[i - 1];
        }
        frame[0] -= PREEMPH * frame[0];
        let mut buf = vec![Complex32::new(0.0, 0.0); FFT_SIZE];
        for (i, (x, w)) in frame.iter().zip(&self.window).enumerate() {
            buf[i] = Complex32::new(x * w, 0.0);
        }
        self.fft.process(&mut buf);
        let power: Vec<f32> = buf[..=FFT_SIZE / 2]
            .iter()
            .map(Complex32::norm_sqr)
            .collect();
        self.bins
            .iter()
            .map(|(first, weights)| {
                let energy: f32 = weights
                    .iter()
                    .zip(&power[*first..])
                    .map(|(w, p)| w * p)
                    .sum();
                energy.max(f32::EPSILON).ln()
            })
            .collect()
    }

    /// Frames `from..from + n` as `[n][dim]`; frames before `from` are dropped.
    ///
    /// # Panics
    /// If a frame in the range does not exist yet or was dropped.
    #[must_use]
    pub fn frames(&mut self, from: usize, n: usize) -> Vec<f32> {
        assert!(from >= self.first_frame && from + n <= self.frames_ready());
        let drop = from - self.first_frame;
        self.frames.drain(..drop);
        self.first_frame = from;
        self.frames.iter().take(n).flatten().copied().collect()
    }
}

/// The streaming model (module docs): three sessions, the token list and the export's settings.
#[derive(Debug)]
pub struct NemotronStreaming {
    encoder: OnnxSession,
    decoder: OnnxSession,
    joiner: OnnxSession,
    vocab: Vocab,
    feat_dim: usize,
    window: usize,
    shift: usize,
    cache_channel: [usize; 3],
    cache_time: [usize; 3],
    pred_layers: usize,
    pred_hidden: usize,
}

/// One stream's state: features, encoder caches, the prediction network's state and the tokens
/// emitted so far.
#[derive(Debug)]
pub struct NemotronStream {
    fbank: OnlineFbank,
    processed: usize,
    cache_channel: Vec<f32>,
    cache_time: Vec<f32>,
    cache_len: i64,
    lstm: (Vec<f32>, Vec<f32>),
    /// The prediction network's last output and its shape.
    pred: Option<(Vec<i64>, Vec<f32>)>,
    last: Option<usize>,
    /// Encoder frames decoded.
    frame_offset: usize,
    tokens: Vec<CtcToken>,
}

impl NemotronStream {
    /// Adds samples in `[-1, 1]` at 16 kHz.
    pub fn accept(&mut self, samples: &[f32]) {
        self.fbank.accept(samples);
    }

    /// No more audio: the last frames are computed (decode what is ready afterwards).
    pub fn input_finished(&mut self) {
        self.fbank.input_finished();
    }

    /// Every token emitted so far, at its encoder frame.
    #[must_use]
    pub fn tokens(&self) -> &[CtcToken] {
        &self.tokens
    }

    /// Encoder frames decoded so far.
    #[must_use]
    pub fn frames_decoded(&self) -> usize {
        self.frame_offset
    }
}

fn meta_usize(
    meta: &std::collections::BTreeMap<String, String>,
    key: &str,
) -> Result<usize, OnnxError> {
    meta.get(key)
        .and_then(|v| v.trim().parse::<usize>().ok())
        .ok_or_else(|| OnnxError::Signature(format!("Nemotron streaming: metadata {key} missing")))
}

fn first_max(values: &[f32]) -> usize {
    let mut best = 0;
    for (i, v) in values.iter().enumerate() {
        if *v > values[best] {
            best = i;
        }
    }
    best
}

fn log_softmax_at(values: &[f32], i: usize) -> f32 {
    let max = values.iter().copied().fold(f32::NEG_INFINITY, f32::max);
    let sum: f32 = values.iter().map(|v| (v - max).exp()).sum();
    values[i] - max - sum.ln()
}

impl NemotronStreaming {
    /// Loads `encoder.int8.onnx`, `decoder.int8.onnx`, `joiner.int8.onnx` and `tokens.txt` from
    /// `dir` (manifest set `stream-eval`), each session with `config`.
    ///
    /// # Errors
    /// [`OnnxError::Model`] for missing or invalid files, [`OnnxError::Signature`] if the export
    /// is not the streaming NeMo transducer this port follows, provider errors as from
    /// [`OnnxSession::load`].
    pub fn load(dir: &Path, config: &SessionConfig) -> Result<Self, OnnxError> {
        let tokens = dir.join("tokens.txt");
        let text = std::fs::read_to_string(&tokens).map_err(|e| {
            OnnxError::Model(format!(
                "{}: {e}; run `cargo xtask fetch-models --set stream-eval`",
                tokens.display()
            ))
        })?;
        let vocab = Vocab::parse(&text).map_err(|e| OnnxError::Model(e.to_string()))?;
        let encoder = OnnxSession::load(&dir.join("encoder.int8.onnx"), config)?;
        encoder.expect_signature(
            "Nemotron streaming encoder",
            &[
                "audio_signal",
                "length",
                "cache_last_channel",
                "cache_last_time",
                "cache_last_channel_len",
            ],
            &[
                "outputs",
                "encoded_lengths",
                "cache_last_channel_next",
                "cache_last_time_next",
                "cache_last_channel_next_len",
            ],
        )?;
        let decoder = OnnxSession::load(&dir.join("decoder.int8.onnx"), config)?;
        if decoder.input_names().len() != 4 || decoder.output_names().len() != 4 {
            return Err(OnnxError::Signature(format!(
                "Nemotron streaming decoder: inputs {:?}, outputs {:?}",
                decoder.input_names(),
                decoder.output_names()
            )));
        }
        let joiner = OnnxSession::load(&dir.join("joiner.int8.onnx"), config)?;
        joiner.expect_signature(
            "Nemotron streaming joiner",
            &["encoder_outputs", "decoder_outputs"],
            &["outputs"],
        )?;
        let meta = encoder.custom_metadata()?;
        let normalize = meta.get("normalize_type").map_or("", String::as_str);
        if !(normalize.is_empty() || normalize == "NA") {
            return Err(OnnxError::Signature(format!(
                "Nemotron streaming: normalize_type {normalize:?} is not ported"
            )));
        }
        if meta_usize(&meta, "subsampling_factor")? != SUBSAMPLING {
            return Err(OnnxError::Signature(
                "Nemotron streaming: subsampling_factor is not 8".into(),
            ));
        }
        if meta_usize(&meta, "vocab_size")? + 1 != vocab.len() || vocab.blank() != vocab.len() - 1 {
            return Err(OnnxError::Signature(
                "Nemotron streaming: tokens.txt does not match vocab_size + blank".into(),
            ));
        }
        let three = |p: &str| -> Result<[usize; 3], OnnxError> {
            Ok([
                meta_usize(&meta, &format!("{p}_dim1"))?,
                meta_usize(&meta, &format!("{p}_dim2"))?,
                meta_usize(&meta, &format!("{p}_dim3"))?,
            ])
        };
        let feat_dim = encoder
            .input_shape("audio_signal")
            .and_then(|s| s.get(1).copied())
            .and_then(|d| usize::try_from(d).ok())
            .ok_or_else(|| OnnxError::Signature("Nemotron streaming: feat_dim".into()))?;
        Ok(Self {
            feat_dim,
            window: meta_usize(&meta, "window_size")?,
            shift: meta_usize(&meta, "chunk_shift")?,
            cache_channel: three("cache_last_channel")?,
            cache_time: three("cache_last_time")?,
            pred_layers: meta_usize(&meta, "pred_rnn_layers")?,
            pred_hidden: meta_usize(&meta, "pred_hidden")?,
            encoder,
            decoder,
            joiner,
            vocab,
        })
    }

    /// The token list.
    #[must_use]
    pub fn vocab(&self) -> &Vocab {
        &self.vocab
    }

    /// Feature frames per chunk and between chunks.
    #[must_use]
    pub fn chunk(&self) -> (usize, usize) {
        (self.window, self.shift)
    }

    /// Where ONNX Runtime placed the three sessions.
    #[must_use]
    pub fn backends(&self) -> Vec<&SessionBackend> {
        vec![
            self.encoder.backend(),
            self.decoder.backend(),
            self.joiner.backend(),
        ]
    }

    /// The backend the encoder runs on.
    #[must_use]
    pub fn info(&self) -> BackendInfo {
        BackendInfo {
            kind: self.encoder.backend().kind().unwrap_or(BackendKind::Cpu),
            device: self.encoder.backend().device(),
            model: "nemotron-streaming-0.6b-560ms-int8".into(),
        }
    }

    /// A new stream at time zero.
    #[must_use]
    pub fn stream(&self) -> NemotronStream {
        self.stream_on(OnlineFbank::new(self.feat_dim))
    }

    /// A new stream reading its features from `fbank` (for example
    /// [`OnlineFbank::finished_with`]).
    #[must_use]
    pub fn stream_on(&self, fbank: OnlineFbank) -> NemotronStream {
        let n = self.pred_layers * self.pred_hidden;
        NemotronStream {
            fbank,
            processed: 0,
            cache_channel: vec![0.0; self.cache_channel.iter().product()],
            cache_time: vec![0.0; self.cache_time.iter().product()],
            cache_len: 0,
            lstm: (vec![0.0; n], vec![0.0; n]),
            pred: None,
            last: None,
            frame_offset: 0,
            tokens: Vec::new(),
        }
    }

    /// Whether `stream` holds a whole chunk not yet decoded (sherpa-onnx `IsReady`).
    #[must_use]
    pub fn is_ready(&self, stream: &NemotronStream) -> bool {
        stream.processed + self.window < stream.fbank.frames_ready()
    }

    /// Decodes every ready chunk; returns how many tokens they emitted.
    ///
    /// # Errors
    /// ONNX Runtime errors, or [`OnnxError::Signature`] for an output of an unexpected shape.
    pub fn decode_ready(&mut self, stream: &mut NemotronStream) -> Result<usize, OnnxError> {
        let before = stream.tokens.len();
        while self.is_ready(stream) {
            self.decode_chunk(stream)?;
        }
        Ok(stream.tokens.len() - before)
    }

    /// Decodes one chunk (module docs).
    ///
    /// # Errors
    /// As [`NemotronStreaming::decode_ready`].
    ///
    /// # Panics
    /// If no chunk is ready ([`NemotronStreaming::is_ready`]).
    pub fn decode_chunk(&mut self, stream: &mut NemotronStream) -> Result<(), OnnxError> {
        let (window, dim) = (self.window, self.feat_dim);
        let frames = stream.fbank.frames(stream.processed, window);
        stream.processed += self.shift;
        // [T][F] -> [F][T]
        let mut features = vec![0.0f32; window * dim];
        for t in 0..window {
            for f in 0..dim {
                features[f * window + t] = frames[t * dim + f];
            }
        }
        let audio = Tensor::from_array(([1usize, dim, window], features)).map_err(runtime_err)?;
        let length = Tensor::from_array(([1usize], vec![i64::try_from(window).unwrap_or(0)]))
            .map_err(runtime_err)?;
        let [c1, c2, c3] = self.cache_channel;
        let channel = Tensor::from_array((
            [1usize, c1, c2, c3],
            std::mem::take(&mut stream.cache_channel),
        ))
        .map_err(runtime_err)?;
        let [t1, t2, t3] = self.cache_time;
        let time =
            Tensor::from_array(([1usize, t1, t2, t3], std::mem::take(&mut stream.cache_time)))
                .map_err(runtime_err)?;
        let len = Tensor::from_array(([1usize], vec![stream.cache_len])).map_err(runtime_err)?;
        let outputs = self
            .encoder
            .session
            .run(ort::inputs![
                "audio_signal" => audio,
                "length" => length,
                "cache_last_channel" => channel,
                "cache_last_time" => time,
                "cache_last_channel_len" => len
            ])
            .map_err(runtime_err)?;
        let f32s = |name: &str| -> Result<(Vec<i64>, Vec<f32>), OnnxError> {
            let (shape, v) = outputs[name]
                .try_extract_tensor::<f32>()
                .map_err(runtime_err)?;
            Ok((shape.iter().copied().collect(), v.to_vec()))
        };
        let (shape, encoded) = f32s("outputs")?;
        stream.cache_channel = f32s("cache_last_channel_next")?.1;
        stream.cache_time = f32s("cache_last_time_next")?.1;
        let (_, next_len) = outputs["cache_last_channel_next_len"]
            .try_extract_tensor::<i64>()
            .map_err(runtime_err)?;
        stream.cache_len = next_len.first().copied().unwrap_or(0);
        drop(outputs);
        let dims: Vec<usize> = shape
            .iter()
            .map(|d| usize::try_from(*d).unwrap_or(0))
            .collect();
        let [1, d, t] = dims[..] else {
            return Err(OnnxError::Signature(format!(
                "Nemotron streaming encoder output {dims:?}"
            )));
        };
        if stream.cache_channel.len() != c1 * c2 * c3 || stream.cache_time.len() != t1 * t2 * t3 {
            return Err(OnnxError::Signature(
                "Nemotron streaming: cache outputs of another size".into(),
            ));
        }
        self.greedy(stream, &encoded, d, t)
    }

    /// The prediction network on `token` from `state`: its output (shape and values) and next state.
    #[allow(clippy::type_complexity)]
    fn predict(
        &mut self,
        token: usize,
        state: &(Vec<f32>, Vec<f32>),
    ) -> Result<((Vec<i64>, Vec<f32>), (Vec<f32>, Vec<f32>)), OnnxError> {
        let shape = [self.pred_layers, 1usize, self.pred_hidden];
        let targets = Tensor::from_array(([1usize, 1], vec![i32::try_from(token).unwrap_or(0)]))
            .map_err(runtime_err)?;
        let len = Tensor::from_array(([1usize], vec![1i32])).map_err(runtime_err)?;
        let h = Tensor::from_array((shape, state.0.clone())).map_err(runtime_err)?;
        let c = Tensor::from_array((shape, state.1.clone())).map_err(runtime_err)?;
        let names = self.decoder.input_names();
        let out_names = self.decoder.output_names();
        let outputs = self
            .decoder
            .session
            .run(ort::inputs![
                names[0].as_str() => targets,
                names[1].as_str() => len,
                names[2].as_str() => h,
                names[3].as_str() => c
            ])
            .map_err(runtime_err)?;
        let get = |i: usize| -> Result<(Vec<i64>, Vec<f32>), OnnxError> {
            let (shape, v) = outputs[out_names[i].as_str()]
                .try_extract_tensor::<f32>()
                .map_err(runtime_err)?;
            Ok((shape.iter().copied().collect(), v.to_vec()))
        };
        let out = get(0)?;
        let next = (get(2)?.1, get(3)?.1);
        Ok((out, next))
    }

    fn joint(&mut self, enc: Vec<f32>, pred: &(Vec<i64>, Vec<f32>)) -> Result<Vec<f32>, OnnxError> {
        let d = enc.len();
        let enc = Tensor::from_array(([1usize, d, 1], enc)).map_err(runtime_err)?;
        let shape: Vec<usize> = pred
            .0
            .iter()
            .map(|v| usize::try_from(*v).unwrap_or(0))
            .collect();
        let dec = Tensor::from_array((shape, pred.1.clone())).map_err(runtime_err)?;
        let outputs = self
            .joiner
            .session
            .run(ort::inputs!["encoder_outputs" => enc, "decoder_outputs" => dec])
            .map_err(runtime_err)?;
        let (_, v) = outputs["outputs"]
            .try_extract_tensor::<f32>()
            .map_err(runtime_err)?;
        Ok(v.to_vec())
    }

    /// Greedy search over one chunk's encoder output `[1][d][t]` (module docs).
    fn greedy(
        &mut self,
        stream: &mut NemotronStream,
        encoded: &[f32],
        d: usize,
        t: usize,
    ) -> Result<(), OnnxError> {
        let blank = self.vocab.blank();
        let classes = self.vocab.len();
        let mut pred = match stream.pred.take() {
            Some(p) => p,
            None => {
                let token = stream.last.unwrap_or(blank);
                let (out, next) = self.predict(token, &stream.lstm)?;
                stream.lstm = next;
                out
            }
        };
        for frame in 0..t {
            let enc: Vec<f32> = (0..d).map(|k| encoded[k * t + frame]).collect();
            for _ in 0..MAX_SYMBOLS_PER_FRAME {
                let logits = self.joint(enc.clone(), &pred)?;
                if logits.len() < classes {
                    return Err(OnnxError::Signature(format!(
                        "Nemotron streaming joiner gave {} logits for {classes} classes",
                        logits.len()
                    )));
                }
                let logits = &logits[..classes];
                let y = first_max(logits);
                if y == blank {
                    break;
                }
                stream.tokens.push(CtcToken {
                    id: y,
                    first_frame: stream.frame_offset + frame,
                    end_frame: stream.frame_offset + frame + 1,
                    log_prob: log_softmax_at(logits, y),
                });
                stream.last = Some(y);
                let (out, next) = self.predict(y, &stream.lstm)?;
                stream.lstm = next;
                pred = out;
            }
        }
        stream.pred = Some(pred);
        stream.frame_offset += t;
        Ok(())
    }

    /// The stream's words so far, times from the stream's start.
    #[must_use]
    pub fn words(&self, stream: &NemotronStream) -> Vec<Word> {
        words_from_tokens(&stream.tokens, &self.vocab, ENCODER_FRAME)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nemotron_fbank_frames_do_not_depend_on_push_sizes() {
        let samples: Vec<f32> = (0..16_000 * 2)
            .map(|i| {
                #[allow(clippy::cast_precision_loss)]
                let t = i as f32 / 16_000.0;
                0.3 * (2.0 * std::f32::consts::PI * 440.0 * t).sin()
                    + 0.1 * (2.0 * std::f32::consts::PI * 1700.0 * t).sin()
            })
            .collect();
        let mut whole = OnlineFbank::new(128);
        whole.accept(&samples);
        whole.input_finished();
        let mut pieces = OnlineFbank::new(128);
        let mut at = 0;
        for (k, n) in [1usize, 7, 160, 399, 401, 1000, 3]
            .iter()
            .cycle()
            .enumerate()
        {
            if at >= samples.len() || k > 10_000 {
                break;
            }
            let end = (at + n).min(samples.len());
            pieces.accept(&samples[at..end]);
            at = end;
        }
        pieces.input_finished();
        let n = whole.frames_ready();
        // kaldi-native-fbank's flushed frame count: round(samples / shift).
        assert_eq!(n, (samples.len() + 80) / 160);
        assert_eq!(pieces.frames_ready(), n);
        assert_eq!(whole.frames(0, n), pieces.frames(0, n));
    }

    #[test]
    fn nemotron_fbank_holds_back_frames_that_reach_past_the_input_until_it_ends() {
        let mut f = OnlineFbank::new(128);
        f.accept(&vec![0.01; 1000]);
        // Frame k ends at 160k + 280: frames 0..=4 fit in 1000 samples, frame 5 does not.
        assert_eq!(f.frames_ready(), 5);
        f.input_finished();
        assert_eq!(f.frames_ready(), (1000 + 80) / 160);
    }

    #[test]
    fn nemotron_mel_bins_cover_the_spectrum_with_slaney_weights() {
        let bins = librosa_bins(128, 0.0, 8000.0);
        assert_eq!(bins.len(), 128);
        // kaldi-native-fbank refuses a bin with no FFT bin under it; none is empty here.
        for (first, w) in &bins {
            assert!(!w.is_empty() && *first > 0);
        }
        assert!(bins.iter().all(|(_, w)| w.iter().all(|x| *x >= 0.0)));
    }
}
