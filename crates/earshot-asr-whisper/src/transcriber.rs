//! [`WhisperTranscriber`]: `earshot_core::asr::Transcriber` on whisper.cpp.

use std::ffi::{c_int, c_void};
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use earshot_core::asr::{
    AsrError, BackendInfo, BackendKind, CancelToken, MeetingTime, RawSegment, TranscribeOptions,
    Transcriber, Transcription, check_pipeline_rate,
};
use earshot_core::audio::{MonoBuffer, PIPELINE_RATE};
use whisper_rs::{
    DtwMode, DtwParameters, FullParams, SamplingStrategy, WhisperContext, WhisperContextParameters,
    WhisperState,
};
use whisper_rs_sys as sys;

use crate::backend;
use crate::decode::{self, DecodePlan, Strategy, Token};
use crate::dtw::{self, WordTiming};

/// How to load a whisper.cpp model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WhisperConfig {
    /// Path to a ggml model file (for example `whisper/ggml-base.en.bin` in the models
    /// directory).
    pub model: PathBuf,
    /// Run on a GPU when this build has a GPU backend (CUDA, Vulkan) and the machine has a
    /// matching device; otherwise whisper.cpp runs on the CPU.
    /// [`WhisperTranscriber::backend_info`] reports which one was used.
    pub use_gpu: bool,
    /// Index among the GPU devices whisper.cpp sees (0 = first).
    pub gpu_device: u32,
    /// CPU threads for whisper.cpp (mel spectrogram, CPU compute, decoder sampling).
    pub threads: NonZeroUsize,
    /// How words are timed. [`WordTiming::Dtw`] needs alignment heads pinned for the model
    /// ([`crate::dtw::ALIGNMENT_HEADS`]).
    pub word_timing: WordTiming,
}

impl WhisperConfig {
    /// Defaults for `model`: GPU if available, device 0, whisper.cpp's default thread
    /// count, `min(4, available cores)`, and token-timestamp word times.
    #[must_use]
    pub fn new(model: impl Into<PathBuf>) -> Self {
        let cores = std::thread::available_parallelism().map_or(1, NonZeroUsize::get);
        WhisperConfig {
            model: model.into(),
            use_gpu: true,
            gpu_device: 0,
            threads: NonZeroUsize::new(cores.min(4)).unwrap_or(NonZeroUsize::MIN),
            word_timing: WordTiming::TokenTimestamps,
        }
    }

    /// [`WhisperConfig::new`] asking for `backend`: the CPU without a GPU, or the first
    /// device of that GPU backend in ggml's registry (in a build with CUDA and Vulkan the
    /// same card is listed once per backend, so the device index picks the backend). What
    /// actually ran is proven only by [`crate::evidence`].
    ///
    /// # Errors
    /// [`AsrError::Backend`] when this build and machine have no device of `backend` (for
    /// example Vulkan in a build without the `vulkan` feature) or `backend` is not a
    /// whisper.cpp backend.
    pub fn for_backend(model: impl Into<PathBuf>, backend: BackendKind) -> Result<Self, AsrError> {
        let mut config = Self::new(model);
        match backend {
            BackendKind::Cpu => config.use_gpu = false,
            BackendKind::Cuda | BackendKind::Vulkan => {
                config.use_gpu = true;
                config.gpu_device =
                    backend::gpu_index(&backend::registry(), backend).ok_or_else(|| {
                        AsrError::Backend(format!(
                            "whisper.cpp has no {backend:?} device in this build on this machine"
                        ))
                    })?;
            }
            other => {
                return Err(AsrError::Backend(format!(
                    "{other:?} is not a whisper.cpp backend"
                )));
            }
        }
        Ok(config)
    }
}

/// A key for the cached [`FullParams`] templates: what cannot be changed on a clone
/// (`set_language` leaks a C string; the sampling strategy is fixed by `FullParams::new`).
/// Temperatures are set on each call's clone, so a temperature sweep adds no template.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct TemplateKey {
    language: &'static str,
    strategy: Strategy,
}

impl TemplateKey {
    fn for_call(language: &'static str, plan: &DecodePlan) -> Self {
        TemplateKey {
            language,
            strategy: plan.strategy,
        }
    }
}

/// How often whisper.cpp's temperature fallback ran (T2.4d), counted since the transcriber
/// was loaded. A window is one encoder pass (up to 30 s of audio); every decoding attempt
/// after the first of a window is a retry at a higher temperature.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FallbackStats {
    /// Windows decoded.
    pub windows: u64,
    /// Windows decoded more than once (the first attempt failed a fallback check).
    pub fallback_windows: u64,
    /// Decoding attempts after the first of their window.
    pub retries: u64,
}

/// Counters updated from whisper.cpp callbacks during `whisper_full` (atomics: the
/// logits callback also runs on whisper.cpp's decoder worker threads).
#[derive(Debug, Default)]
struct FallbackCounter {
    windows: AtomicU64,
    window_attempts: AtomicU64,
    fallback_windows: AtomicU64,
    retries: AtomicU64,
}

impl FallbackCounter {
    /// Closes the current window: its attempts after the first are retries, counted per
    /// window (a window with no counted attempt adds nothing).
    fn close_window(&self) {
        let attempts = self.window_attempts.swap(0, Ordering::Relaxed);
        if attempts > 1 {
            self.fallback_windows.fetch_add(1, Ordering::Relaxed);
            self.retries.fetch_add(attempts - 1, Ordering::Relaxed);
        }
    }

    fn stats(&self) -> FallbackStats {
        FallbackStats {
            windows: self.windows.load(Ordering::Relaxed),
            fallback_windows: self.fallback_windows.load(Ordering::Relaxed),
            retries: self.retries.load(Ordering::Relaxed),
        }
    }
}

/// whisper.cpp encoder-begin callback: one call per window (before each 30 s encoder pass,
/// whisper.cpp 1.8.3 `whisper_full_with_state`), on the calling thread. Never aborts.
unsafe extern "C" fn window_started(
    _ctx: *mut sys::whisper_context,
    _state: *mut sys::whisper_state,
    user_data: *mut c_void,
) -> bool {
    // SAFETY: `user_data` is the boxed `FallbackCounter` of the transcriber running
    // `whisper_full`; the box outlives the call and is only read through atomics.
    let counter = unsafe { &*user_data.cast::<FallbackCounter>() };
    counter.close_window();
    counter.windows.fetch_add(1, Ordering::Relaxed);
    true
}

/// whisper.cpp logits-filter callback. Every decoding attempt of a window (the first one
/// and each temperature retry) starts with exactly one call for decoder 0 with no tokens
/// yet, on the calling thread (`whisper_full_with_state`, before the decoding loop); every
/// later call has at least one token. So calls with `n_tokens == 0` count attempts.
/// The logits are not changed.
unsafe extern "C" fn attempt_step(
    _ctx: *mut sys::whisper_context,
    _state: *mut sys::whisper_state,
    _tokens: *const sys::whisper_token_data,
    n_tokens: c_int,
    _logits: *mut f32,
    user_data: *mut c_void,
) {
    if n_tokens == 0 {
        // SAFETY: as in `window_started`.
        let counter = unsafe { &*user_data.cast::<FallbackCounter>() };
        counter.window_attempts.fetch_add(1, Ordering::Relaxed);
    }
}

/// A loaded whisper.cpp model with one decoding state, implementing
/// [`Transcriber`].
///
/// - Word times follow [`WhisperConfig::word_timing`]: whisper.cpp's token-level timestamps
///   (`token_timestamps = true`, `whisper_exp_compute_token_level_timestamps`; each word
///   spans its first token's `t0` to its last token's `t1`, 10 ms resolution), or DTW over
///   the model's alignment heads ([`crate::dtw`]; `dtw_token_timestamps`, 20 ms resolution,
///   flash attention off). Token-level timestamps are computed in both modes (DTW falls
///   back to them for a token it did not reach). Neither changes the decoded text.
/// - Every call is independent: `no_context` is set, so the only text context is
///   [`TranscribeOptions::prompt`] (passed as prompt tokens), and beam-search calls run on
///   a fresh decoding state (whisper.cpp's beam search draws candidates from an RNG that a
///   reused state carries from call to call).
/// - Decoding follows [`TranscribeOptions::decoding`] exactly, including its temperature
///   fallback, which runs inside whisper.cpp per 30 s window (see `decode::decode_plan`);
///   [`WhisperTranscriber::fallback_stats`] counts the windows that fell back. A retry at a
///   temperature above 0 samples with decoder 0's RNG, which whisper.cpp seeds once per
///   state: batch calls run on a fresh state, so their retries are reproducible; greedy
///   (live) calls keep the state (the live backend check relies on that), so once a live
///   window has fallen back, later sampled live retries depend on the earlier ones.
///   Attempts at temperature 0 take the arg max and never depend on history.
/// - Input shorter than [`MIN_INPUT_SAMPLES`] (which whisper.cpp would skip and return no
///   text for) is padded with trailing silence to it; that call drops words and segments
///   starting in the padding and clamps the rest to the end of the real input; empty input
///   returns no segments.
/// - A failed state creation leaves no state; the next call creates one.
/// - Cancellation is polled through whisper.cpp's abort callback (checked after every
///   encoder and decoder graph); a cancelled call returns [`AsrError::Cancelled`].
pub struct WhisperTranscriber {
    ctx: WhisperContext,
    /// `None` only between dropping a used state and creating its replacement.
    state: Option<WhisperState>,
    info: BackendInfo,
    threads: i32,
    multilingual: bool,
    word_timing: WordTiming,
    // `FullParams::set_language` allocates a C string it never frees, so params are built
    // once per (language, strategy) and cloned per call (a clone copies the pointer).
    templates: Vec<(TemplateKey, FullParams<'static, 'static>)>,
    // Boxed: whisper.cpp callbacks hold its address during `whisper_full`.
    fallback: Box<FallbackCounter>,
}

impl std::fmt::Debug for WhisperTranscriber {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WhisperTranscriber")
            .field("info", &self.info)
            .field("threads", &self.threads)
            .field("multilingual", &self.multilingual)
            .field("word_timing", &self.word_timing)
            .finish_non_exhaustive()
    }
}

impl WhisperTranscriber {
    /// Loads the model and initialises its backend and decoding state.
    ///
    /// # Errors
    /// [`AsrError::Config`] if the model file is missing, the thread count is too large, or
    /// DTW word times are asked for a model without pinned alignment heads or whose shape
    /// differs from the one its heads are for; [`AsrError::Backend`] if whisper.cpp cannot
    /// load the model or create its state, or it picked a GPU backend Earshot does not
    /// support.
    pub fn load(config: &WhisperConfig) -> Result<Self, AsrError> {
        let heads = match config.word_timing {
            WordTiming::TokenTimestamps => None,
            WordTiming::Dtw => Some(dtw::alignment_heads(&model_id(&config.model)).ok_or_else(
                || {
                    AsrError::Config(format!(
                        "no DTW alignment heads are pinned for {} (dtw::ALIGNMENT_HEADS)",
                        model_id(&config.model)
                    ))
                },
            )?),
        };
        if !config.model.is_file() {
            return Err(AsrError::Config(format!(
                "whisper model not found: {}",
                config.model.display()
            )));
        }
        let threads = i32::try_from(config.threads.get())
            .map_err(|_| AsrError::Config(format!("too many threads: {}", config.threads)))?;
        let gpu_device = i32::try_from(config.gpu_device).map_err(|_| {
            AsrError::Config(format!("invalid GPU device index {}", config.gpu_device))
        })?;

        let mut params = WhisperContextParameters::default();
        // Flash attention off in both modes (whisper-rs's default; whisper.cpp would drop
        // DTW with only a warning).
        params
            .use_gpu(config.use_gpu)
            .gpu_device(gpu_device)
            .flash_attn(false);
        if let Some(heads) = heads {
            params.dtw_parameters(DtwParameters {
                mode: DtwMode::ModelPreset {
                    model_preset: heads.preset.whisper_rs(),
                },
                ..DtwParameters::default()
            });
        }
        let ctx = WhisperContext::new_with_params(&config.model, params)
            .map_err(|e| AsrError::Backend(format!("load {}: {e}", config.model.display())))?;
        if let Some(heads) = heads {
            dtw::check_shape(
                heads,
                ctx.model_n_text_layer(),
                ctx.model_n_text_head(),
                ctx.is_multilingual(),
            )
            .map_err(|e| AsrError::Config(format!("{}: {e}", config.model.display())))?;
        }
        // whisper.cpp initialises its compute backends per state (`whisper_init_state`).
        let state = ctx
            .create_state()
            .map_err(|e| AsrError::Backend(format!("create whisper state: {e}")))?;

        let devices = backend::registry();
        let (kind, device) = match backend::select_gpu(&devices, config.use_gpu, config.gpu_device)
        {
            Some(gpu) => (backend::gpu_kind(gpu)?, gpu.description.clone()),
            None => (BackendKind::Cpu, "cpu".to_owned()),
        };
        let info = BackendInfo {
            kind,
            device,
            model: model_id(&config.model),
        };
        let multilingual = ctx.is_multilingual();
        Ok(WhisperTranscriber {
            ctx,
            state: Some(state),
            info,
            threads,
            multilingual,
            word_timing: config.word_timing,
            templates: Vec::new(),
            fallback: Box::default(),
        })
    }

    /// How this transcriber times words ([`WhisperConfig::word_timing`]).
    #[must_use]
    pub fn word_timing(&self) -> WordTiming {
        self.word_timing
    }

    /// Temperature-fallback counts over every call since [`WhisperTranscriber::load`].
    #[must_use]
    pub fn fallback_stats(&self) -> FallbackStats {
        self.fallback.stats()
    }

    /// Frees the decoding state; the next call starts on a fresh one, so its sampled
    /// fallback retries no longer depend on earlier calls (whisper.cpp seeds decoder 0's RNG
    /// once per state). For evaluations that must be a function of each input; the runtime
    /// never calls it (live decoding keeps its state).
    pub fn reset_state(&mut self) {
        self.state = None;
    }

    fn template(&mut self, key: TemplateKey) -> FullParams<'static, 'static> {
        if let Some((_, params)) = self.templates.iter().find(|(k, _)| *k == key) {
            return params.clone();
        }
        let strategy = match key.strategy {
            Strategy::Greedy { best_of } => SamplingStrategy::Greedy { best_of },
            Strategy::BeamSearch { beam_size } => SamplingStrategy::BeamSearch {
                beam_size,
                // whisper.cpp's default; patience is not implemented in whisper.cpp.
                patience: -1.0,
            },
        };
        let mut params = FullParams::new(strategy);
        params.set_language(Some(key.language));
        params.set_translate(false);
        params.set_no_context(true);
        params.set_token_timestamps(true);
        params.set_print_special(false);
        params.set_print_progress(false);
        params.set_print_realtime(false);
        params.set_print_timestamps(false);
        self.templates.push((key, params.clone()));
        params
    }

    fn prompt_tokens(&self, prompt: Option<&str>) -> Result<Vec<i32>, AsrError> {
        let Some(prompt) = prompt.filter(|p| !p.trim().is_empty()) else {
            return Ok(Vec::new());
        };
        // Every BPE token covers at least one byte, so this buffer is always large enough
        // (whisper-rs mishandles whisper_tokenize's "buffer too small" result).
        let mut tokens = self
            .ctx
            .tokenize(prompt, prompt.len() + 16)
            .map_err(|e| AsrError::Config(format!("prompt cannot be tokenised: {e}")))?;
        // whisper.cpp uses at most n_text_ctx / 2 prompt tokens, the most recent ones.
        let keep = usize::try_from(self.ctx.n_text_ctx() / 2).unwrap_or(0);
        if tokens.len() > keep {
            tokens.drain(..tokens.len() - keep);
        }
        Ok(tokens)
    }

    fn state(&self) -> Result<&WhisperState, AsrError> {
        self.state
            .as_ref()
            .ok_or_else(|| AsrError::Backend("no whisper state (a re-creation failed)".into()))
    }

    fn collect_segments(&self) -> Result<Vec<RawSegment>, AsrError> {
        let eot = self.ctx.token_eot();
        let mut segments = Vec::new();
        // DTW time of the last text token so far in this call (`decode::Timing::Dtw`).
        let mut preceding = -1;
        for segment in self.state()?.as_iter() {
            let (seg_t0, seg_t1) = (segment.start_timestamp(), segment.end_timestamp());
            let mut raw = Vec::new();
            for i in 0..segment.n_tokens() {
                let Some(token) = segment.get_token(i) else {
                    continue;
                };
                let data = token.token_data();
                let text = token
                    .to_bytes()
                    .map_err(|e| AsrError::Backend(format!("token text: {e}")))?;
                raw.push(Token {
                    text,
                    t0: data.t0,
                    t1: data.t1,
                    t_dtw: data.t_dtw,
                    p: data.p,
                    special: token.token_id() >= eot,
                });
            }
            let timing = decode::Timing::of(self.word_timing, preceding);
            let words = decode::assemble_words(&raw, seg_t0, seg_t1, timing);
            // A segment without text tokens keeps the earlier one.
            if raw.iter().any(|t| !t.special) {
                preceding = decode::last_dtw(&raw);
            }
            let text = segment
                .to_str_lossy()
                .map_err(|e| AsrError::Backend(format!("segment text: {e}")))?
                .trim()
                .to_owned();
            let start = MeetingTime::from_millis(decode::centis_to_millis(seg_t0));
            let end = MeetingTime::from_millis(decode::centis_to_millis(seg_t1)).max(start);
            segments.push(RawSegment {
                start,
                end,
                text,
                words,
            });
        }
        Ok(segments)
    }
}

/// Model id for reports: the file name without `.bin` (for example `ggml-tiny.en`).
fn model_id(path: &Path) -> String {
    path.file_stem().map_or_else(
        || path.display().to_string(),
        |s| s.to_string_lossy().into_owned(),
    )
}

/// The decoding state for a call: the one in `slot`, unless `fresh` asks for a new one or
/// the slot is empty (an earlier creation failed); then the old state is dropped first and
/// `create` makes the new one. A failed creation leaves the slot empty, so the next call
/// tries again.
fn ensure_state<S, E>(
    slot: &mut Option<S>,
    fresh: bool,
    create: impl FnOnce() -> Result<S, E>,
) -> Result<&mut S, E> {
    let state = match slot.take() {
        Some(state) if !fresh => state,
        old => {
            drop(old);
            create()?
        }
    };
    Ok(slot.insert(state))
}

/// Mel hop and FFT window of whisper.cpp (`WHISPER_HOP_LENGTH`, `WHISPER_N_FFT`).
const MEL_HOP: usize = 160;
const MEL_FFT: usize = 400;
/// Fewest mel frames whisper.cpp 1.8.3 `whisper_full_with_state` decodes. With
/// `delta_min = 10` it returns early below 10 frames ("input is too short", a warning only),
/// and its decoding loop starts a window only while `seek + delta_min < seek_end`, so exactly
/// 10 frames also return no text, silently (measured: 1,640 samples started no window).
const MIN_MEL_FRAMES: usize = 11;

/// Mel frames whisper.cpp 1.8.3 counts for `n` samples (`log_mel_spectrogram`:
/// `n_len_org = 1 + (n_samples + stage_2_pad - frame_size) / frame_step`, with
/// `stage_2_pad = frame_size / 2`), for `n >= MEL_FFT / 2`.
const fn mel_frames(n: usize) -> usize {
    1 + (n + MEL_FFT / 2 - MEL_FFT) / MEL_HOP
}

/// Shortest input (16 kHz samples, 112.5 ms) whisper.cpp 1.8.3 decodes: the first length
/// with [`MIN_MEL_FRAMES`] mel frames. [`WhisperTranscriber`] pads shorter input with
/// trailing silence to this length, so every caller (for example a short VAD piece or an
/// AMI segment) is decoded instead of silently returning no text. It also keeps whisper.cpp's
/// 200-sample reflective pad (`std::reverse_copy(samples + 1, samples + 201, ...)`) inside
/// the buffer.
pub const MIN_INPUT_SAMPLES: usize = (MIN_MEL_FRAMES - 1) * MEL_HOP + MEL_FFT / 2;
const _: () = assert!(mel_frames(MIN_INPUT_SAMPLES) == MIN_MEL_FRAMES);

/// `samples`, extended with trailing silence to [`MIN_INPUT_SAMPLES`] when shorter.
fn pad_to_min_input(samples: &[f32]) -> std::borrow::Cow<'_, [f32]> {
    if samples.len() >= MIN_INPUT_SAMPLES {
        return std::borrow::Cow::Borrowed(samples);
    }
    let mut padded = samples.to_vec();
    padded.resize(MIN_INPUT_SAMPLES, 0.0);
    std::borrow::Cow::Owned(padded)
}

/// End of `samples` 16 kHz samples, rounded up to the next millisecond.
fn input_end(samples: usize) -> MeetingTime {
    let ms = (samples as u64 * 1000).div_ceil(u64::from(PIPELINE_RATE.hz()));
    MeetingTime::from_millis(ms)
}

/// Keeps a padded call ([`pad_to_min_input`]) inside the input the caller gave (`end`):
/// words and segments that start at or after `end` lie wholly in the added silence and are
/// dropped (clamping them would make `start == end`, which LocalAgreement reads as an
/// untimed word); a word or segment that straddles `end` is clamped to it. A segment that
/// loses words gets its text rebuilt from the words it keeps, and one left without words is
/// dropped (`RawSegment::timed_words` would otherwise re-create words from its text).
fn clamp_to_input(segments: &mut Vec<RawSegment>, end: MeetingTime) {
    segments.retain_mut(|s| {
        if s.start >= end {
            return false;
        }
        s.end = s.end.min(end);
        if s.words.is_empty() {
            return true;
        }
        let before = s.words.len();
        s.words.retain(|w| w.start < end);
        for w in &mut s.words {
            w.end = w.end.min(end);
        }
        if s.words.len() != before {
            s.text = s
                .words
                .iter()
                .map(|w| w.text.as_str())
                .collect::<Vec<_>>()
                .join(" ");
        }
        !s.words.is_empty()
    });
}

/// whisper.cpp abort callback: returns true once the [`CancelToken`] is cancelled.
unsafe extern "C" fn abort_requested(user_data: *mut c_void) -> bool {
    // SAFETY: `user_data` is the `&CancelToken` set in `transcribe`, which outlives the
    // `WhisperState::full` call during which whisper.cpp invokes this callback.
    let token = unsafe { &*user_data.cast::<CancelToken>() };
    token.is_cancelled()
}

impl Transcriber for WhisperTranscriber {
    fn transcribe(
        &mut self,
        audio: &MonoBuffer,
        options: &TranscribeOptions,
        cancel: &CancelToken,
    ) -> Result<Transcription, AsrError> {
        check_pipeline_rate(audio)?;
        if cancel.is_cancelled() {
            return Err(AsrError::Cancelled);
        }
        let plan = decode::decode_plan(&options.decoding)?;
        let language = decode::whisper_language(&options.language, self.multilingual, |l| {
            whisper_rs::get_lang_id(l).and_then(whisper_rs::get_lang_str)
        })?;
        let prompt = self.prompt_tokens(options.prompt.as_deref())?;
        let auto = language == "auto";
        if audio.samples.is_empty() {
            return Ok(Transcription {
                segments: Vec::new(),
                language: (!auto).then(|| language.to_owned()),
                backend: self.info.clone(),
            });
        }

        // Shortened from 'static (FullParams is covariant) so it can borrow `prompt`.
        let mut params: FullParams<'_, '_> = self.template(TemplateKey::for_call(language, &plan));
        params.set_temperature(plan.temperature);
        params.set_temperature_inc(plan.temperature_inc);
        params.set_entropy_thold(plan.entropy_thold);
        params.set_logprob_thold(plan.logprob_thold);
        params.set_n_threads(self.threads);
        params.set_tokens(&prompt);
        let token: *const CancelToken = cancel;
        // SAFETY: `abort_requested` only reads the `CancelToken` behind `user_data`; the
        // token is borrowed for this whole call and whisper.cpp only calls back during
        // `full` below, so the pointer never dangles.
        unsafe {
            params.set_abort_callback(Some(abort_requested));
            params.set_abort_callback_user_data(token.cast_mut().cast::<c_void>());
        }
        let counter: *const FallbackCounter = &*self.fallback;
        // SAFETY: both callbacks only touch the `FallbackCounter` behind `user_data` through
        // atomics; the box lives as long as `self`, and whisper.cpp only calls back during
        // `full` below.
        unsafe {
            params.set_start_encoder_callback(Some(window_started));
            params.set_start_encoder_callback_user_data(counter.cast_mut().cast::<c_void>());
            params.set_filter_logits_callback(Some(attempt_step));
            params.set_filter_logits_callback_user_data(counter.cast_mut().cast::<c_void>());
        }
        // whisper.cpp's beam search samples its candidates with decoder 0's RNG, which
        // `whisper_init_state` seeds once and `whisper_full` never re-seeds (1.8.3), so on a
        // reused state the result depends on every earlier beam decode. A fresh state makes
        // each batch call a function of its input. Greedy decoding at temperature 0 takes the
        // arg max and keeps the state. The old state (KV cache, compute buffers) is freed
        // before the new one is allocated, so GPU memory never holds both. Creating a state
        // re-runs `whisper_backend_init_gpu`; callers that report a backend check each call's
        // log with `evidence::check_call_log`.
        // Beam calls get a fresh state; a call after a failed creation creates one, so one
        // failure never disables the transcriber.
        let fresh = matches!(plan.strategy, Strategy::BeamSearch { .. });
        let ctx = &self.ctx;
        let state = ensure_state(&mut self.state, fresh, || {
            ctx.create_state()
                .map_err(|e| AsrError::Backend(format!("create whisper state: {e}")))
        })?;
        let samples = pad_to_min_input(&audio.samples);
        let result = state.full(params, &samples);
        self.fallback.close_window();
        if cancel.is_cancelled() {
            return Err(AsrError::Cancelled);
        }
        result.map_err(|e| AsrError::Backend(format!("whisper_full failed: {e}")))?;

        let language = if auto {
            whisper_rs::get_lang_str(self.state()?.full_lang_id_from_state()).map(str::to_owned)
        } else {
            Some(language.to_owned())
        };
        let mut segments = self.collect_segments()?;
        if samples.len() > audio.samples.len() {
            clamp_to_input(&mut segments, input_end(audio.samples.len()));
        }
        Ok(Transcription {
            segments,
            language,
            backend: self.info.clone(),
        })
    }

    fn backend_info(&self) -> BackendInfo {
        self.info.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transcriber_is_send() {
        fn assert_send<T: Send>() {}
        assert_send::<WhisperTranscriber>();
    }

    #[test]
    fn default_config_uses_whisper_thread_default() {
        let config = WhisperConfig::new("m.bin");
        assert!((1..=4).contains(&config.threads.get()));
        assert!(config.use_gpu);
        assert_eq!(config.gpu_device, 0);
    }

    #[test]
    fn config_for_backend_follows_the_build() {
        let cpu = WhisperConfig::for_backend("m.bin", BackendKind::Cpu).unwrap();
        assert!(!cpu.use_gpu);
        // A GPU backend this build lacks is refused, never a CPU config.
        if !cfg!(feature = "vulkan") {
            let e = WhisperConfig::for_backend("m.bin", BackendKind::Vulkan).unwrap_err();
            assert!(e.to_string().contains("no Vulkan device"), "{e}");
        }
        if !cfg!(feature = "cuda") {
            assert!(WhisperConfig::for_backend("m.bin", BackendKind::Cuda).is_err());
        }
        for kind in [BackendKind::Qnn, BackendKind::Fake] {
            assert!(WhisperConfig::for_backend("m.bin", kind).is_err());
        }
    }

    /// Drives the counter as whisper.cpp's callbacks do (`window_started`, then one
    /// zero-token logits call per attempt, then later steps with tokens).
    fn window(counter: &FallbackCounter, attempts: u64) {
        let ud = std::ptr::from_ref(counter).cast_mut().cast::<c_void>();
        // SAFETY: `ud` points at a live `FallbackCounter` for the duration of the calls;
        // the callbacks never dereference the other pointers.
        unsafe {
            window_started(std::ptr::null_mut(), std::ptr::null_mut(), ud);
            for _ in 0..attempts {
                attempt_step(
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    std::ptr::null(),
                    0,
                    std::ptr::null_mut(),
                    ud,
                );
                attempt_step(
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    std::ptr::null(),
                    3,
                    std::ptr::null_mut(),
                    ud,
                );
            }
        }
    }

    #[test]
    fn fallback_retries_are_counted_per_window() {
        let counter = FallbackCounter::default();
        window(&counter, 1);
        // A window that ends before its first logits call (for example cancelled) must not
        // absorb another window's retries.
        window(&counter, 0);
        window(&counter, 3);
        counter.close_window();
        assert_eq!(
            counter.stats(),
            FallbackStats {
                windows: 3,
                fallback_windows: 1,
                retries: 2,
            }
        );
    }

    #[test]
    fn template_key_ignores_temperature_but_not_strategy_or_language() {
        let plan = |strategy, temperature| DecodePlan {
            strategy,
            temperature,
            temperature_inc: 0.0,
            entropy_thold: 2.4,
            logprob_thold: -1.0,
        };
        let greedy = Strategy::Greedy { best_of: 1 };
        let beam = Strategy::BeamSearch { beam_size: 5 };
        let key = |l, p: DecodePlan| TemplateKey::for_call(l, &p);
        assert_eq!(key("en", plan(greedy, 0.0)), key("en", plan(greedy, 0.4)));
        assert_ne!(key("en", plan(greedy, 0.0)), key("en", plan(beam, 0.0)));
        assert_ne!(key("en", plan(greedy, 0.0)), key("de", plan(greedy, 0.0)));
    }

    #[test]
    fn model_id_is_the_file_stem() {
        assert_eq!(
            model_id(Path::new("models/whisper/ggml-tiny.en.bin")),
            "ggml-tiny.en"
        );
    }

    /// A state that records when it is dropped, for `ensure_state`.
    struct Probe {
        id: u32,
        drops: std::rc::Rc<std::cell::RefCell<Vec<u32>>>,
    }

    impl Drop for Probe {
        fn drop(&mut self) {
            self.drops.borrow_mut().push(self.id);
        }
    }

    #[test]
    fn a_failed_state_creation_is_retried_by_the_next_call() {
        let drops = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let mut created = 0;
        let mut create = |ok: bool| {
            let drops = drops.clone();
            let id = created;
            created += 1;
            move || {
                if ok {
                    Ok(Probe { id, drops })
                } else {
                    Err("create failed")
                }
            }
        };
        let mut slot = Some(Probe {
            id: 100,
            drops: drops.clone(),
        });
        // A beam call needs a fresh state: the old one is freed before the new one is
        // created, so a failed creation leaves the slot empty.
        assert!(ensure_state(&mut slot, true, create(false)).is_err());
        assert!(slot.is_none());
        assert_eq!(*drops.borrow(), [100]);
        // The next call (greedy, so it would reuse a state) creates one instead of failing
        // with "no whisper state" for the rest of the transcriber's life.
        assert_eq!(ensure_state(&mut slot, false, create(true)).unwrap().id, 1);
        // A greedy call keeps the state (the creator is not called).
        let unused = || -> Result<Probe, &str> { panic!("a kept state was re-created") };
        assert_eq!(ensure_state(&mut slot, false, unused).unwrap().id, 1);
        // A fresh state drops the old one first (GPU memory never holds both).
        let drops2 = drops.clone();
        let fresh = move || {
            assert_eq!(drops2.borrow().last(), Some(&1), "old state still alive");
            Ok::<_, &str>(Probe {
                id: 2,
                drops: drops2.clone(),
            })
        };
        assert_eq!(ensure_state(&mut slot, true, fresh).unwrap().id, 2);
        assert_eq!(*drops.borrow(), [100, 1]);
    }

    #[test]
    fn times_of_a_padded_call_stay_inside_the_real_input() {
        // 1,000 samples = 62.5 ms, so the input ends at 63 ms (rounded up).
        let end = input_end(1_000);
        assert_eq!(end, MeetingTime::from_millis(63));
        assert_eq!(input_end(MIN_INPUT_SAMPLES), MeetingTime::from_millis(113));
        let mut segments = vec![RawSegment {
            start: MeetingTime::from_millis(0),
            end: MeetingTime::from_millis(110),
            text: "hi there".into(),
            words: vec![
                earshot_core::asr::Word::new("hi".to_owned(), 10, 50, 0.9),
                earshot_core::asr::Word::new("there".to_owned(), 50, 110, 0.8),
            ],
        }];
        clamp_to_input(&mut segments, end);
        let s = &segments[0];
        assert_eq!((s.start, s.end), (MeetingTime::from_millis(0), end));
        assert_eq!(s.words[0].start, MeetingTime::from_millis(10));
        assert_eq!(s.words[0].end, MeetingTime::from_millis(50));
        assert_eq!(s.words[1].start, MeetingTime::from_millis(50));
        assert_eq!(s.words[1].end, end);
        // A segment wholly in the padding is dropped, never turned into untimed words.
        let mut late = vec![RawSegment {
            start: MeetingTime::from_millis(80),
            end: MeetingTime::from_millis(100),
            text: "x".into(),
            words: vec![earshot_core::asr::Word::new("x".to_owned(), 80, 100, 0.5)],
        }];
        clamp_to_input(&mut late, end);
        assert!(late.is_empty(), "{late:?}");
        // A word starting exactly at the input end is in the padding too; the segment keeps
        // its earlier words and its text follows them.
        let mut mixed = vec![RawSegment {
            start: MeetingTime::from_millis(0),
            end: MeetingTime::from_millis(100),
            text: "hi there you".into(),
            words: vec![
                earshot_core::asr::Word::new("hi".to_owned(), 0, 40, 0.9),
                earshot_core::asr::Word::new("there".to_owned(), 40, 70, 0.9),
                earshot_core::asr::Word::new("you".to_owned(), 63, 100, 0.9),
            ],
        }];
        clamp_to_input(&mut mixed, end);
        let s = &mixed[0];
        assert_eq!(s.text, "hi there");
        assert_eq!(s.words.len(), 2);
        assert_eq!(
            (s.words[1].start, s.words[1].end),
            (MeetingTime::from_millis(40), end)
        );
        assert!(s.words.iter().all(|w| w.start < w.end));
        // A segment whose words all lie in the padding is dropped, not re-timed from text.
        let mut empty_after = vec![RawSegment {
            start: MeetingTime::from_millis(50),
            end: MeetingTime::from_millis(100),
            text: "late".into(),
            words: vec![earshot_core::asr::Word::new(
                "late".to_owned(),
                70,
                100,
                0.9,
            )],
        }];
        clamp_to_input(&mut empty_after, end);
        assert!(empty_after.is_empty());
        // A segment without word times is kept and clamped.
        let mut untimed = vec![RawSegment {
            start: MeetingTime::from_millis(0),
            end: MeetingTime::from_millis(100),
            text: "hello".into(),
            words: Vec::new(),
        }];
        clamp_to_input(&mut untimed, end);
        assert_eq!(untimed[0].end, end);
    }

    #[test]
    fn input_shorter_than_whisper_minimum_is_padded_with_trailing_silence() {
        // whisper.cpp 1.8.3 decodes nothing up to 10 mel frames (`delta_min`): the minimum is
        // the shortest input with 11 frames, and one sample less has 10.
        assert_eq!(mel_frames(MIN_INPUT_SAMPLES), MIN_MEL_FRAMES);
        assert_eq!(mel_frames(MIN_INPUT_SAMPLES - 1), MIN_MEL_FRAMES - 1);
        assert_eq!(
            MIN_INPUT_SAMPLES, 1_800,
            "100 ms (1,600 samples) is not enough"
        );
        let short = vec![0.5f32; 1000];
        let padded = pad_to_min_input(&short);
        assert_eq!(padded.len(), MIN_INPUT_SAMPLES);
        assert!(padded[..1000].iter().all(|&x| x == 0.5));
        assert!(padded[1000..].iter().all(|&x| x == 0.0));
        // Long enough already: the input itself, not a copy.
        let long = vec![0.25f32; MIN_INPUT_SAMPLES];
        assert!(matches!(
            pad_to_min_input(&long),
            std::borrow::Cow::Borrowed(s) if s.as_ptr() == long.as_ptr()
        ));
    }

    #[test]
    fn dtw_without_pinned_alignment_heads_is_a_config_error() {
        let mut config = WhisperConfig::new("models/whisper/ggml-large-v2.bin");
        config.word_timing = WordTiming::Dtw;
        let err = WhisperTranscriber::load(&config).unwrap_err();
        assert!(
            matches!(&err, AsrError::Config(m) if m.contains("no DTW alignment heads") && m.contains("ggml-large-v2")),
            "{err:?}"
        );
    }

    #[test]
    fn missing_model_is_a_config_error() {
        let err = WhisperTranscriber::load(&WhisperConfig::new("does/not/exist.bin")).unwrap_err();
        assert!(matches!(err, AsrError::Config(_)), "{err:?}");
    }
}
