//! `earshot-cli transcribe`: a WAV or FLAC file through whisper.cpp, in batch mode or through
//! the core streaming chunker with a VAD.
//!
//! - Batch: the whole file at 16 kHz, [`TranscribeOptions::batch`] (beam 5, temperature 0
//!   with Whisper's temperature fallback, PLAN.md 4.0).
//! - `--stream`: [`StreamingChunker`] over the same transcriber with
//!   [`ChunkerConfig::default`] ([`TranscribeOptions::live`]: greedy, temperature 0 with the
//!   fallback), fed in
//!   [`STREAM_PUSH_MS`] pushes like a capture device; only final segments are printed
//!   (partials go to stderr with `--verbose`).
//! - The backend in the output is what ran: [`Transcriber::backend_info`] cross-checked
//!   against whisper.cpp's own `whisper_backend_init_gpu` log (whisper.cpp falls back to
//!   the CPU silently when a GPU cannot initialise, AGENTS.md 11). A mismatch with
//!   `--backend` is an error.
//! - Parakeet models (T6.10e3, `--model parakeet-...`) run on ONNX Runtime through
//!   `earshot_runtime::backends::load_parakeet`, which proves the backend from the node
//!   placement; they always decode greedily (beam settings are ignored, reported in the
//!   options), and a batch call longer than 30 s is decoded in consecutive 30 s chunks
//!   (the app decodes VAD pieces instead).

use std::ffi::OsString;
use std::fs::File;
use std::io::BufReader;
use std::num::NonZeroU16;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail, ensure};
use clap::{Args, ValueEnum};
use earshot_asr_whisper::evidence::{self, LogBackend};
use earshot_asr_whisper::{WhisperConfig, WhisperTranscriber};
use earshot_core::asr::{
    AsrError, BackendInfo, BackendKind, CancelToken, ChunkerConfig, EnergyVad, EnergyVadConfig,
    MeetingTime, RawSegment, Segment, StreamingChunker, StreamingTranscriber, TranscribeOptions,
    Transcriber, TranscriptEvent, Vad, Word, mean_probability,
};
use earshot_core::audio::{MonoBuffer, PIPELINE_RATE, Sample, SampleRate, read_wav, resample_all};
use earshot_core::capture::{TrackBuffer, mix_to_mono};
use earshot_core::identity::EntityId;
use earshot_onnx::silero::SileroVad;
use earshot_onnx::{ORT_CPU_DYLIB, ORT_CPU_DYLIB_SHA256, SessionConfig};
use earshot_runtime::backends::{
    Accelerator, ModelDir, PARAKEET_MODELS, ParakeetModel, init_onnx, load_parakeet,
};
use serde_json::{Value, json};

/// Model used when `--model` is not given (PLAN.md 3.4: base.en is the CPU default).
pub const DEFAULT_MODEL: &str = "base.en";

/// Audio per `push` in `--stream` mode: one 10 ms WASAPI period at the pipeline rate.
pub const STREAM_PUSH_MS: u64 = 10;

/// Silero VAD inside the models directory (manifest id `silero-vad`).
pub const SILERO_MODEL: &str = "silero/silero_vad.onnx";

/// `models/manifest.toml`, embedded so the model names and fetch sets cannot drift from it.
const MODELS_MANIFEST: &str = include_str!("../../../models/manifest.toml");

/// Whisper models of `models/manifest.toml` (artefacts at `whisper/ggml-<name>.bin`) by
/// short name, with the first fetch set that has them, in manifest order.
///
/// # Errors
/// The embedded manifest does not parse, a whisper artefact has no fetch set, or it lists
/// no whisper model: a broken manifest fails loudly instead of rejecting every name.
pub fn known_models() -> Result<Vec<(String, String)>> {
    parse_known_models(MODELS_MANIFEST)
}

fn parse_known_models(manifest: &str) -> Result<Vec<(String, String)>> {
    let doc: toml::Table = manifest
        .parse()
        .context("the embedded models/manifest.toml does not parse")?;
    let mut known = Vec::new();
    for a in doc
        .get("artefact")
        .and_then(toml::Value::as_array)
        .into_iter()
        .flatten()
    {
        let Some(name) = a
            .get("path")
            .and_then(toml::Value::as_str)
            .and_then(|p| p.strip_prefix("whisper/ggml-"))
            .and_then(|p| p.strip_suffix(".bin"))
        else {
            continue;
        };
        let set = a
            .get("sets")
            .and_then(toml::Value::as_array)
            .and_then(|s| s.first())
            .and_then(toml::Value::as_str)
            .with_context(|| format!("whisper model {name} has no fetch set in the manifest"))?;
        known.push((name.to_owned(), set.to_owned()));
    }
    ensure!(
        !known.is_empty(),
        "models/manifest.toml lists no whisper model"
    );
    Ok(known)
}

/// `earshot-cli transcribe` arguments.
#[derive(Debug, Args)]
pub struct TranscribeArgs {
    /// Input audio: WAV (any rate; channels are mixed down) or FLAC.
    pub input: PathBuf,
    /// Model: a whisper name in the models directory (tiny.en, base.en, small.en,
    /// small.en-q5_1, large-v3-turbo, large-v3-turbo-q5_0), a path to a ggml `.bin`, or a
    /// Parakeet name (parakeet-ctc-110m, parakeet-tdt-110m, parakeet-tdt-0.6b-v2,
    /// parakeet-tdt-0.6b-v3; `cargo xtask fetch-models --set ci` fetches the defaults,
    /// `--set choice` and `--set eval` the others).
    #[arg(long, default_value = DEFAULT_MODEL, value_name = "NAME|PATH")]
    pub model: String,
    /// Compute backend for whisper.cpp. Default: cuda in a `--features cuda` build, else cpu.
    #[arg(long, value_enum)]
    pub backend: Option<BackendArg>,
    /// Stream the file through the VAD-driven chunker (greedy decoding) instead of one
    /// batch decode (beam search).
    #[arg(long)]
    pub stream: bool,
    /// Voice activity detector for `--stream`.
    #[arg(long, value_enum, default_value_t = VadArg::Silero, requires = "stream")]
    pub vad: VadArg,
    /// Print machine-readable JSON (segments, words, backend, decoding settings).
    #[arg(long)]
    pub json: bool,
    /// Print whisper.cpp's log and streaming partials to stderr.
    #[arg(long)]
    pub verbose: bool,
}

/// `--backend` values.
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum BackendArg {
    /// whisper.cpp on the CPU.
    Cpu,
    /// whisper.cpp on CUDA (needs a `--features cuda` build).
    Cuda,
    /// whisper.cpp on Vulkan (needs a `--features vulkan` build; the first discrete Vulkan
    /// GPU, else the first integrated one). Parakeet models do not run on Vulkan.
    Vulkan,
    /// Parakeet on the Snapdragon NPU through Qualcomm's QNN plugin (needs a
    /// `--features qnn` build; on x64 the plugin's HTP simulator). whisper has no QNN backend.
    Qnn,
}

impl BackendArg {
    fn name(self) -> &'static str {
        match self {
            BackendArg::Cpu => "cpu",
            BackendArg::Cuda => "cuda",
            BackendArg::Vulkan => "vulkan",
            BackendArg::Qnn => "qnn",
        }
    }

    /// The backend it asks for.
    #[must_use]
    pub fn kind(self) -> BackendKind {
        match self {
            BackendArg::Cpu => BackendKind::Cpu,
            BackendArg::Cuda => BackendKind::Cuda,
            BackendArg::Vulkan => BackendKind::Vulkan,
            BackendArg::Qnn => BackendKind::Qnn,
        }
    }
}

/// `--vad` values.
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum VadArg {
    /// Silero VAD v6 on ONNX Runtime (CPU).
    Silero,
    /// Core energy VAD (model-free).
    Energy,
}

/// The backend used when `--backend` is not given.
#[must_use]
pub fn default_backend() -> BackendArg {
    if cfg!(feature = "cuda") {
        BackendArg::Cuda
    } else {
        BackendArg::Cpu
    }
}

/// Fails for a backend this binary was not built with.
pub fn check_backend_built(backend: BackendArg) -> Result<()> {
    if backend == BackendArg::Cuda && !cfg!(feature = "cuda") {
        bail!(
            "--backend cuda needs a CUDA build of the CLI: \
             `cargo run -p earshot-cli --features cuda -- transcribe ...` \
             (or use `--backend cpu`)"
        );
    }
    if backend == BackendArg::Vulkan && !cfg!(feature = "vulkan") {
        bail!(
            "--backend vulkan needs a Vulkan build of the CLI: \
             `cargo xtask build --features vulkan` (or use `--backend cpu`)"
        );
    }
    if backend == BackendArg::Qnn && !cfg!(feature = "qnn") {
        bail!(
            "--backend qnn needs a QNN build of the CLI: \
             `cargo xtask build --target aarch64-pc-windows-msvc --features qnn` \
             (or use `--backend cpu`)"
        );
    }
    Ok(())
}

/// Models directory: `EARSHOT_MODELS`, else `%LOCALAPPDATA%\Earshot\models` (the rule of
/// `xtask::models::cache_dir`). `var` reads an environment variable.
pub fn models_dir_from(var: impl Fn(&str) -> Option<OsString>) -> Option<PathBuf> {
    if let Some(dir) = var("EARSHOT_MODELS").filter(|v| !v.is_empty()) {
        return Some(PathBuf::from(dir));
    }
    var("LOCALAPPDATA")
        .filter(|v| !v.is_empty())
        .map(|base| PathBuf::from(base).join("Earshot").join("models"))
}

/// [`models_dir_from`] over the process environment.
#[must_use]
pub fn models_dir() -> Option<PathBuf> {
    models_dir_from(|k| std::env::var_os(k))
}

fn no_models_dir() -> anyhow::Error {
    anyhow::anyhow!("no models directory: set EARSHOT_MODELS (or LOCALAPPDATA)")
}

/// Resolves `--model`: a value with a path separator or a `.bin` suffix is a file path;
/// anything else is a short name looked up as `whisper/ggml-<name>.bin` in `models`.
/// A missing model fails with the `cargo xtask fetch-models` command that provides it.
pub fn resolve_model(spec: &str, models: Option<&Path>) -> Result<PathBuf> {
    ensure!(!spec.trim().is_empty(), "--model must not be empty");
    if spec.contains(['/', '\\']) || spec.ends_with(".bin") {
        let path = PathBuf::from(spec);
        ensure!(
            path.is_file(),
            "whisper model file not found: {}",
            path.display()
        );
        return Ok(path);
    }
    let known = known_models()?;
    let Some((_, set)) = known.iter().find(|(name, _)| name == spec) else {
        let names: Vec<&str> = known.iter().map(|(n, _)| n.as_str()).collect();
        bail!(
            "unknown whisper model `{spec}`: use one of {} or a path to a ggml .bin file",
            names.join(", ")
        );
    };
    let rel = format!("whisper/ggml-{spec}.bin");
    let path = models
        .ok_or_else(no_models_dir)?
        .join("whisper")
        .join(format!("ggml-{spec}.bin"));
    ensure!(
        path.is_file(),
        "whisper model `{spec}` not found at {}; run `cargo xtask fetch-models --set {set}` \
         (or point EARSHOT_MODELS at a directory that contains {rel})",
        path.display()
    );
    Ok(path)
}

/// Decoded input audio and the 16 kHz mono buffer derived from it.
#[derive(Debug)]
pub struct InputAudio {
    /// Sample rate of the file.
    pub source_rate: u32,
    /// Channel count of the file (mixed down by averaging).
    pub channels: u16,
    /// Mono audio at [`PIPELINE_RATE`].
    pub pipeline: MonoBuffer,
}

/// Reads a WAV (core reader) or FLAC (claxon) file, mixes it to mono and resamples it to
/// [`PIPELINE_RATE`].
pub fn read_audio(path: &Path) -> Result<InputAudio> {
    let ext = path
        .extension()
        .map(|e| e.to_string_lossy().to_ascii_lowercase());
    let open = || File::open(path).with_context(|| format!("cannot open {}", path.display()));
    let (rate, channels, mono) = match ext.as_deref() {
        Some("wav") => {
            let wav = read_wav(BufReader::new(open()?))
                .with_context(|| format!("cannot read WAV {}", path.display()))?;
            (wav.rate(), wav.channels(), wav.to_mono())
        }
        Some("flac") => {
            let (rate, channels, interleaved) = read_flac(path)?;
            (rate, channels, flac_mono(channels, interleaved))
        }
        _ => bail!(
            "unsupported input {}: expected a .wav or .flac file",
            path.display()
        ),
    };
    let samples = if rate == PIPELINE_RATE {
        mono
    } else {
        resample_all(&mono, rate, PIPELINE_RATE)
            .with_context(|| format!("cannot resample {} Hz to 16 kHz", rate.hz()))?
    };
    Ok(InputAudio {
        source_rate: rate.hz(),
        channels,
        pipeline: MonoBuffer::new(PIPELINE_RATE, samples),
    })
}

fn read_flac(path: &Path) -> Result<(SampleRate, u16, Vec<Sample>)> {
    let ctx = || format!("cannot read FLAC {}", path.display());
    let mut reader = claxon::FlacReader::open(path).with_context(ctx)?;
    let info = reader.streaminfo();
    let rate = SampleRate::new(info.sample_rate).with_context(ctx)?;
    let channels = u16::try_from(info.channels).with_context(ctx)?;
    ensure!(
        channels > 0 && (1..=32).contains(&info.bits_per_sample),
        "{}: unsupported FLAC format",
        path.display()
    );
    let scale = 1.0 / (1_u64 << (info.bits_per_sample - 1)) as f32;
    let interleaved = reader
        .samples()
        .map(|s| s.map(|v| v as f32 * scale))
        .collect::<Result<Vec<f32>, _>>()
        .with_context(ctx)?;
    Ok((rate, channels, interleaved))
}

/// The mean of each frame's channels (`read_flac` checked `channels > 0`).
fn flac_mono(channels: u16, interleaved: Vec<Sample>) -> Vec<Sample> {
    match NonZeroU16::new(channels) {
        Some(ch) if ch.get() > 1 => {
            let mut mono = Vec::with_capacity(interleaved.len() / usize::from(ch.get()));
            mix_to_mono(&interleaved, ch, &mut mono);
            mono
        }
        _ => interleaved,
    }
}

/// Reads a WAV or FLAC file at its own rate, keeping its channels (interleaved), for a
/// replay track ([`TrackBuffer`]).
pub fn read_track(path: &Path) -> Result<TrackBuffer> {
    let ext = path
        .extension()
        .map(|e| e.to_string_lossy().to_ascii_lowercase());
    let bad = |e: earshot_core::capture::ReplayError| {
        anyhow::anyhow!("{}: unusable audio ({e:?})", path.display())
    };
    match ext.as_deref() {
        Some("wav") => {
            let file =
                File::open(path).with_context(|| format!("cannot open {}", path.display()))?;
            let wav = read_wav(BufReader::new(file))
                .with_context(|| format!("cannot read WAV {}", path.display()))?;
            TrackBuffer::try_from(wav).map_err(bad)
        }
        Some("flac") => {
            let (rate, channels, interleaved) = read_flac(path)?;
            let channels = NonZeroU16::new(channels).context("FLAC without channels")?;
            TrackBuffer::new(rate, channels, interleaved).map_err(bad)
        }
        _ => bail!(
            "unsupported input {}: expected a .wav or .flac file",
            path.display()
        ),
    }
}

/// Fails unless `requested`, the transcriber's [`BackendInfo`] and whisper.cpp's log agree
/// (the shared rule in [`earshot_asr_whisper::evidence::check_backend`]).
pub fn check_backend(requested: BackendArg, info: &BackendInfo, log: LogBackend) -> Result<()> {
    evidence::check_backend(requested.kind(), info, log).map_err(|e| {
        anyhow::anyhow!(
            "--backend {} requested, but {e} (run with --verbose to see its log)",
            requested.name()
        )
    })
}

/// The language a run asked for, as printed in `--json` (`"en"`, or `"auto"`).
#[must_use]
pub fn requested_language(options: &TranscribeOptions) -> String {
    match &options.language {
        earshot_core::asr::Language::Code(c) => c.clone(),
        earshot_core::asr::Language::Auto => "auto".to_owned(),
    }
}

/// Handles one streaming event: a final is kept for `--json` and, in text mode, printed to
/// `out` at once (not after the whole file); partials go to stderr with `--verbose`.
///
/// # Errors
/// Writing to `out` failed.
pub fn handle_stream_event(
    event: TranscriptEvent,
    print: Option<&mut dyn std::io::Write>,
    verbose: bool,
    segments: &mut Vec<OutSegment>,
) -> Result<()> {
    match event {
        TranscriptEvent::Final(s) => {
            let seg = OutSegment::from_segment(s);
            if let Some(out) = print {
                writeln!(out, "{}", seg.to_line())?;
                out.flush()?;
            }
            segments.push(seg);
        }
        TranscriptEvent::Partial(p) if verbose => eprintln!(
            "partial [{}] {} | {}",
            format_time(p.segment.start.as_millis()),
            p.committed_text(),
            p.tentative_text()
        ),
        TranscriptEvent::Partial(_) | TranscriptEvent::Discarded { .. } => {}
    }
    Ok(())
}

/// `[mm:ss.mmm]`-style time, with hours when needed.
#[must_use]
pub fn format_time(ms: u64) -> String {
    let (h, m, s, milli) = (ms / 3_600_000, ms / 60_000 % 60, ms / 1000 % 60, ms % 1000);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}.{milli:03}")
    } else {
        format!("{m:02}:{s:02}.{milli:03}")
    }
}

/// One output segment (meeting-relative times; the file starts at 0).
#[derive(Debug, Clone, PartialEq)]
pub struct OutSegment {
    /// Start in ms.
    pub start_ms: u64,
    /// End in ms.
    pub end_ms: u64,
    /// Text.
    pub text: String,
    /// Mean word probability.
    pub confidence: f32,
    /// Timed words.
    pub words: Vec<Word>,
}

impl OutSegment {
    fn from_raw(s: &RawSegment) -> Self {
        let words = s.timed_words();
        OutSegment {
            start_ms: s.start.as_millis(),
            end_ms: s.end.as_millis(),
            text: s.text.trim().to_owned(),
            confidence: mean_probability(&words),
            words,
        }
    }

    fn from_segment(s: Segment) -> Self {
        OutSegment {
            start_ms: s.start.as_millis(),
            end_ms: s.end.as_millis(),
            text: s.text,
            confidence: s.confidence,
            words: s.words,
        }
    }

    fn to_json(&self) -> Value {
        let words: Vec<Value> = self
            .words
            .iter()
            .map(|w| {
                json!({
                    "text": w.text,
                    "start_ms": w.start.as_millis(),
                    "end_ms": w.end.as_millis(),
                    "probability": w.probability,
                })
            })
            .collect();
        json!({
            "start_ms": self.start_ms,
            "end_ms": self.end_ms,
            "text": self.text,
            "confidence": self.confidence,
            "words": words,
        })
    }

    fn to_line(&self) -> String {
        format!(
            "[{} -> {}] {}",
            format_time(self.start_ms),
            format_time(self.end_ms),
            self.text
        )
    }
}

/// The VAD chosen with `--vad`.
enum CliVad {
    Silero(Box<SileroVad>),
    Energy(EnergyVad),
}

impl Vad for CliVad {
    fn id(&self) -> String {
        match self {
            CliVad::Silero(v) => v.id(),
            CliVad::Energy(v) => v.id(),
        }
    }

    fn frame_len(&self) -> usize {
        match self {
            CliVad::Silero(v) => v.frame_len(),
            CliVad::Energy(v) => v.frame_len(),
        }
    }

    fn speech_probability(&mut self, frame: &[Sample]) -> Result<f32, AsrError> {
        match self {
            CliVad::Silero(v) => v.speech_probability(frame),
            CliVad::Energy(v) => v.speech_probability(frame),
        }
    }

    fn reset(&mut self) {
        match self {
            CliVad::Silero(v) => v.reset(),
            CliVad::Energy(v) => v.reset(),
        }
    }
}

/// Loads the chosen VAD; Silero needs ONNX Runtime (the CPU library, loaded here unless
/// `ort_ready` says a Parakeet model already loaded one) and the Silero model from the
/// models directory. Returns the VAD and its JSON description.
fn load_vad(kind: VadArg, ort_ready: bool) -> Result<(CliVad, Value)> {
    match kind {
        VadArg::Energy => {
            let vad = EnergyVad::new(EnergyVadConfig::default()).context("energy VAD")?;
            Ok((
                CliVad::Energy(vad),
                json!({ "kind": "energy", "backend": "cpu" }),
            ))
        }
        VadArg::Silero => {
            let dir = models_dir().ok_or_else(no_models_dir)?;
            let model = dir.join(SILERO_MODEL);
            ensure!(
                model.is_file(),
                "Silero VAD not found at {}; run `cargo xtask fetch-models --set ci` \
                 (or use `--vad energy`)",
                model.display()
            );
            if !ort_ready {
                earshot_onnx::init(&dir.join(ORT_CPU_DYLIB), ORT_CPU_DYLIB_SHA256)
                    .context("cannot load ONNX Runtime for Silero VAD")?;
            }
            let vad = SileroVad::load(&model, &SessionConfig::default())
                .context("cannot load Silero VAD")?;
            let backend = serde_json::to_value(vad.backend().kind())?;
            let desc = json!({
                "kind": "silero",
                "backend": backend,
                "model_path": model.display().to_string(),
            });
            Ok((CliVad::Silero(Box::new(vad)), desc))
        }
    }
}

fn ms(d: std::time::Duration) -> u64 {
    u64::try_from(d.as_millis()).unwrap_or(u64::MAX)
}

/// Segment ids for the CLI's throwaway segments (never stored): 1, 2, 3, ...
fn sequential_ids() -> Box<dyn FnMut() -> EntityId + Send> {
    let mut n: u128 = 0;
    Box::new(move || {
        n += 1;
        EntityId::from_bytes(n.to_be_bytes())
    })
}

/// The Parakeet model named `spec`, if it names one.
#[must_use]
pub fn parakeet_spec(spec: &str) -> Option<&'static ParakeetModel> {
    PARAKEET_MODELS.iter().find(|p| p.name == spec)
}

/// Loads a Parakeet model on `backend` (ONNX Runtime for that backend first) and checks the
/// placement against it: the transcriber, its backend and the placement as evidence.
fn load_parakeet_cli(
    p: &ParakeetModel,
    backend: BackendArg,
) -> Result<(Box<dyn Transcriber + Send>, BackendInfo, Value)> {
    let models = ModelDir::new(models_dir().ok_or_else(no_models_dir)?);
    let accel = Accelerator::of(backend.kind())
        .ok_or_else(|| anyhow::anyhow!("no accelerator for --backend {}", backend.name()))?;
    init_onnx(&models, accel).map_err(|e| anyhow::anyhow!("{e}"))?;
    let t = load_parakeet(&models, p, accel, None).map_err(|e| anyhow::anyhow!("{e}"))?;
    let evidence = json!({
        "onnx_placement": t.backends().iter().map(ToString::to_string).collect::<Vec<_>>(),
    });
    let info = t.backend_info();
    Ok((Box::new(t), info, evidence))
}

/// Loads the whisper model at `model` on `backend` and checks it against whisper.cpp's log
/// from `mark`: the transcriber, its backend and the log as evidence.
fn load_whisper_cli(
    model: &Path,
    backend: BackendArg,
    mark: usize,
) -> Result<(Box<dyn Transcriber + Send>, BackendInfo, Value)> {
    let config = WhisperConfig::for_backend(model, backend.kind())?;
    let transcriber = WhisperTranscriber::load(&config)
        .with_context(|| format!("cannot load {}", model.display()))?;
    let info = transcriber.backend_info();
    let (log_backend, evidence_lines) = evidence::classify_whisper_log(&evidence::logs_since(mark));
    check_backend(backend, &info, log_backend)?;
    let evidence = json!({
        "whisper_log": log_backend.name(),
        "lines": evidence_lines,
    });
    Ok((Box::new(transcriber), info, evidence))
}

/// Runs `earshot-cli transcribe` and prints the result to stdout.
pub fn run(args: &TranscribeArgs) -> Result<()> {
    let backend = args.backend.unwrap_or_else(default_backend);
    check_backend_built(backend)?;
    let parakeet = parakeet_spec(&args.model);
    let model = match parakeet {
        Some(p) => models_dir().ok_or_else(no_models_dir)?.join(p.dir),
        None => resolve_model(&args.model, models_dir().as_deref())?,
    };
    let audio = read_audio(&args.input)?;
    evidence::capture_logs(args.verbose).context("cannot capture whisper.cpp's log")?;
    let mark = evidence::log_mark();
    // A Parakeet model loads ONNX Runtime for its backend, which Silero then shares; for
    // whisper the cheap preconditions (VAD model, ONNX Runtime) come before the model load.
    let (vad, (transcriber, info, backend_evidence)) = match parakeet {
        Some(p) => {
            let loaded = load_parakeet_cli(p, backend)?;
            let vad = args.stream.then(|| load_vad(args.vad, true)).transpose()?;
            (vad, loaded)
        }
        None => {
            ensure!(
                backend != BackendArg::Qnn,
                "whisper.cpp has no QNN backend; --backend qnn runs Parakeet models"
            );
            let vad = args.stream.then(|| load_vad(args.vad, false)).transpose()?;
            (vad, load_whisper_cli(&model, backend, mark)?)
        }
    };
    let want = backend.kind();
    ensure!(
        info.kind == want,
        "{} runs on {:?}, but --backend {} was requested",
        info.model,
        info.kind,
        backend.name()
    );

    let header = |mode: &str, options: &TranscribeOptions| {
        eprintln!(
            "transcribe: {mode}, {} on {:?} ({}), decoding {:?}",
            info.model, info.kind, info.device, options.decoding
        );
    };
    let (mode, options, vad, stream, language, segments) = if let Some((vad, vad_json)) = vad {
        let chunker_config = ChunkerConfig::default();
        let options = chunker_config.options.clone();
        let stream_json = json!({
            "push_ms": STREAM_PUSH_MS,
            "max_window_ms": ms(chunker_config.max_window),
            "overlap_ms": ms(chunker_config.overlap),
            "partial_interval_ms": ms(chunker_config.partial_interval),
            "vad_threshold": chunker_config.vad_threshold,
            "min_speech_ms": ms(chunker_config.min_speech),
            "min_silence_ms": ms(chunker_config.min_silence),
            "speech_pad_ms": ms(chunker_config.speech_pad),
            "prompt_chars": chunker_config.prompt_chars,
        });
        let mut chunker = StreamingChunker::new(
            transcriber,
            vad,
            chunker_config,
            EntityId::from_bytes([0; 16]),
            MeetingTime::ZERO,
            sequential_ids(),
        )
        .context("cannot start the streaming chunker")?;
        let push = usize::try_from(STREAM_PUSH_MS * u64::from(PIPELINE_RATE.hz()) / 1000)?;
        if !args.json {
            header("stream", &options);
        }
        let mut stdout = std::io::stdout();
        let mut segments = Vec::new();
        let mut handle = |events: Vec<TranscriptEvent>| -> Result<()> {
            for event in events {
                let print: Option<&mut dyn std::io::Write> =
                    if args.json { None } else { Some(&mut stdout) };
                handle_stream_event(event, print, args.verbose, &mut segments)?;
            }
            Ok(())
        };
        for chunk in audio.pipeline.samples.chunks(push) {
            handle(
                chunker
                    .push(chunk)
                    .context("streaming transcription failed")?,
            )?;
        }
        handle(chunker.finish().context("streaming transcription failed")?)?;
        // The streaming chunker never detects a language.
        ("stream", options, vad_json, stream_json, None, segments)
    } else {
        let mut transcriber = transcriber;
        let options = TranscribeOptions::batch();
        let call_mark = evidence::log_mark();
        let t = transcriber
            .transcribe(&audio.pipeline, &options, &CancelToken::new())
            .context("transcription failed")?;
        // A beam-search call re-creates the whisper state, which re-runs the GPU
        // initialisation: its log must agree with the backend checked at load.
        if parakeet.is_none() {
            evidence::check_call_log(info.kind, &evidence::logs_since(call_mark))
                .context("the transcription did not run on the checked backend")?;
        }
        let segments = t
            .segments
            .iter()
            .map(OutSegment::from_raw)
            .filter(|s| !s.text.is_empty())
            .collect();
        (
            "batch",
            options,
            Value::Null,
            Value::Null,
            t.language,
            segments,
        )
    };

    if args.json {
        let out = json!({
            "input": args.input.display().to_string(),
            "mode": mode,
            "model_path": model.display().to_string(),
            "requested_backend": backend.name(),
            "backend": info,
            "backend_evidence": backend_evidence,
            "options": options,
            "vad": vad,
            "stream": stream,
            "audio": {
                "source_rate": audio.source_rate,
                "channels": audio.channels,
                "duration_ms": ms(audio.pipeline.duration()),
            },
            "requested_language": requested_language(&options),
            "language": language,
            "segments": segments.iter().map(OutSegment::to_json).collect::<Vec<_>>(),
        });
        println!("{}", serde_json::to_string_pretty(&out)?);
    } else if mode == "batch" {
        header(mode, &options);
        for s in &segments {
            println!("{}", s.to_line());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;
    use earshot_core::audio::{WavFormat, write_wav};

    #[derive(Debug, Parser)]
    struct Wrapper {
        #[command(flatten)]
        args: TranscribeArgs,
    }

    fn parse(args: &[&str]) -> Result<TranscribeArgs, clap::Error> {
        Wrapper::try_parse_from(std::iter::once("t").chain(args.iter().copied())).map(|w| w.args)
    }

    #[test]
    fn transcribe_args_defaults() {
        let a = parse(&["in.wav"]).unwrap();
        assert_eq!(a.input, PathBuf::from("in.wav"));
        assert_eq!(a.model, "base.en");
        assert_eq!(a.backend, None);
        assert!(!a.stream && !a.json && !a.verbose);
        assert_eq!(a.vad, VadArg::Silero);
    }

    #[test]
    fn transcribe_args_parse_every_option() {
        let a = parse(&[
            "in.flac",
            "--model",
            "tiny.en",
            "--backend",
            "cuda",
            "--stream",
            "--vad",
            "energy",
            "--json",
            "--verbose",
        ])
        .unwrap();
        assert_eq!(a.model, "tiny.en");
        assert_eq!(a.backend, Some(BackendArg::Cuda));
        assert!(a.stream && a.json && a.verbose);
        assert_eq!(a.vad, VadArg::Energy);
        // T8.3: `qnn` is a backend (Parakeet on the NPU); an unknown name is refused.
        assert_eq!(
            parse(&["in.wav", "--backend", "qnn"]).unwrap().backend,
            Some(BackendArg::Qnn)
        );
        assert!(parse(&["in.wav", "--backend", "npu"]).is_err());
        assert!(parse(&[]).is_err(), "input is required");
    }

    #[test]
    fn transcribe_vad_requires_stream() {
        assert!(parse(&["in.wav", "--vad", "energy"]).is_err());
    }

    #[test]
    fn transcribe_default_backend_follows_the_build() {
        let expected = if cfg!(feature = "cuda") {
            BackendArg::Cuda
        } else {
            BackendArg::Cpu
        };
        assert_eq!(default_backend(), expected);
        assert!(check_backend_built(BackendArg::Cpu).is_ok());
        assert_eq!(
            check_backend_built(BackendArg::Cuda).is_ok(),
            cfg!(feature = "cuda")
        );
        if !cfg!(feature = "cuda") {
            let e = check_backend_built(BackendArg::Cuda).unwrap_err();
            assert!(e.to_string().contains("--features cuda"), "{e}");
        }
        assert_eq!(
            check_backend_built(BackendArg::Qnn).is_ok(),
            cfg!(feature = "qnn")
        );
        if !cfg!(feature = "qnn") {
            let e = check_backend_built(BackendArg::Qnn).unwrap_err();
            assert!(e.to_string().contains("--features qnn"), "{e}");
        }
    }

    #[test]
    fn transcribe_models_dir_prefers_earshot_models() {
        let env = |pairs: &'static [(&'static str, &'static str)]| {
            move |k: &str| {
                pairs
                    .iter()
                    .find(|(n, _)| *n == k)
                    .map(|(_, v)| OsString::from(*v))
            }
        };
        assert_eq!(
            models_dir_from(env(&[("EARSHOT_MODELS", "m"), ("LOCALAPPDATA", "l")])),
            Some(PathBuf::from("m"))
        );
        assert_eq!(
            models_dir_from(env(&[("EARSHOT_MODELS", ""), ("LOCALAPPDATA", "l")])),
            Some(Path::new("l").join("Earshot").join("models"))
        );
        assert_eq!(models_dir_from(env(&[])), None);
    }

    /// Unique scratch directory, removed on drop (std only, see `record_raw.rs`).
    struct ScratchDir(PathBuf);

    impl ScratchDir {
        fn new() -> Self {
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let p = std::env::temp_dir().join(format!(
                "earshot-cli-transcribe-{}-{nanos}",
                std::process::id()
            ));
            std::fs::create_dir_all(&p).unwrap();
            Self(p)
        }
    }

    impl Drop for ScratchDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn transcribe_known_models_follow_the_manifest() {
        assert!(parse_known_models("not = [toml").is_err());
        assert!(parse_known_models("sets = []").is_err(), "no whisper model");
        let no_set = "[[artefact]]\npath = \"whisper/ggml-x.bin\"\n";
        assert!(parse_known_models(no_set).is_err());
        let known = known_models().unwrap();
        let get = |n: &str| known.iter().find(|(m, _)| m == n).map(|(_, s)| s.as_str());
        // PLAN D22: defaults in ci and desktop, models offered but never tested in choice.
        assert_eq!(get("tiny.en"), Some("choice"));
        assert_eq!(get("base.en"), Some("ci"));
        assert_eq!(get("small.en"), Some("desktop"));
        assert_eq!(get("large-v3-turbo"), Some("choice"));
        assert_eq!(get("large-v3-turbo-q5_0"), Some("choice"));
        let whisper_artefacts = MODELS_MANIFEST
            .lines()
            .filter(|l| l.starts_with("path = \"whisper/ggml-"))
            .count();
        assert_eq!(known.len(), whisper_artefacts, "{known:?}");
    }

    #[test]
    fn transcribe_json_names_the_requested_language() {
        assert_eq!(requested_language(&TranscribeOptions::batch()), "en");
        let mut auto = TranscribeOptions::live();
        auto.language = earshot_core::asr::Language::Auto;
        assert_eq!(requested_language(&auto), "auto");
    }

    #[test]
    fn transcribe_stream_prints_each_final_when_it_arrives() {
        let id = EntityId::from_bytes([1; 16]);
        let words = vec![
            Word::new("hello", 0, 400, 0.9),
            Word::new("there", 400, 900, 0.8),
        ];
        let event = || {
            TranscriptEvent::Final(Segment::from_words(
                id,
                id,
                words.clone(),
                MeetingTime::ZERO,
            ))
        };
        let mut out: Vec<u8> = Vec::new();
        let mut segments = Vec::new();
        handle_stream_event(event(), Some(&mut out), false, &mut segments).unwrap();
        assert_eq!(segments.len(), 1);
        let printed = String::from_utf8(out).unwrap();
        assert_eq!(printed.lines().count(), 1, "{printed}");
        assert!(printed.contains("hello there"), "{printed}");
        // --json collects without printing.
        handle_stream_event(event(), None, false, &mut segments).unwrap();
        assert_eq!(segments.len(), 2);
    }

    #[test]
    fn transcribe_resolves_model_names_and_paths() {
        let dir = ScratchDir::new();
        std::fs::create_dir_all(dir.0.join("whisper")).unwrap();
        let tiny = dir.0.join("whisper").join("ggml-tiny.en.bin");
        std::fs::write(&tiny, b"x").unwrap();
        let resolved = resolve_model("tiny.en", Some(&dir.0)).unwrap();
        assert_eq!(resolved, tiny);
        assert_eq!(resolved.to_str(), tiny.to_str(), "one separator style");
        let as_path = tiny.to_str().unwrap();
        assert_eq!(resolve_model(as_path, None).unwrap(), tiny);

        let e = resolve_model("base.en", Some(&dir.0))
            .unwrap_err()
            .to_string();
        assert!(e.contains("cargo xtask fetch-models --set ci"), "{e}");
        assert!(e.contains("ggml-base.en.bin"), "{e}");
        let e = resolve_model("large-v3-turbo", Some(&dir.0))
            .unwrap_err()
            .to_string();
        assert!(e.contains("cargo xtask fetch-models --set choice"), "{e}");
        let e = resolve_model("huge.en", Some(&dir.0))
            .unwrap_err()
            .to_string();
        assert!(e.contains("unknown whisper model"), "{e}");
        let e = resolve_model("tiny.en", None).unwrap_err().to_string();
        assert!(e.contains("EARSHOT_MODELS"), "{e}");
        let missing = dir.0.join("nope.bin");
        assert!(resolve_model(missing.to_str().unwrap(), None).is_err());
        assert!(resolve_model("", Some(&dir.0)).is_err());
    }

    #[test]
    fn transcribe_reads_wav_mixes_down_and_resamples() {
        let dir = ScratchDir::new();
        let path = dir.0.join("a.WAV");
        // 1 s of 48 kHz stereo: left 0.5, right -0.25 -> mono 0.125.
        let stereo: Vec<f32> = (0..48_000).flat_map(|_| [0.5, -0.25]).collect();
        let f = File::create(&path).unwrap();
        write_wav(f, SampleRate::HZ_48000, 2, &stereo, WavFormat::Float32).unwrap();
        let a = read_audio(&path).unwrap();
        assert_eq!(a.source_rate, 48_000);
        assert_eq!(a.channels, 2);
        assert_eq!(a.pipeline.rate, PIPELINE_RATE);
        assert_eq!(a.pipeline.samples.len(), 16_000);
        let mid = a.pipeline.samples[8_000];
        assert!((mid - 0.125).abs() < 1e-3, "{mid}");
    }

    #[test]
    fn transcribe_reads_16k_wav_unchanged_and_rejects_other_formats() {
        let dir = ScratchDir::new();
        let path = dir.0.join("b.wav");
        let s: Vec<f32> = (0..1600).map(|i| (i as f32 / 1600.0) - 0.5).collect();
        write_wav(
            File::create(&path).unwrap(),
            SampleRate::HZ_16000,
            1,
            &s,
            WavFormat::Float32,
        )
        .unwrap();
        let a = read_audio(&path).unwrap();
        assert_eq!(a.pipeline.samples, s);
        let mp3 = dir.0.join("c.mp3");
        std::fs::write(&mp3, b"x").unwrap();
        let e = read_audio(&mp3).unwrap_err().to_string();
        assert!(e.contains(".wav or .flac"), "{e}");
        assert!(read_audio(&dir.0.join("missing.wav")).is_err());
    }

    #[test]
    fn transcribe_reads_a_smoke_flac() {
        let path =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/smoke/61-70968-0000.flac");
        let a = read_audio(&path).unwrap();
        assert_eq!((a.source_rate, a.channels), (16_000, 1));
        assert!(a.pipeline.samples.len() > 16_000);
        assert!(a.pipeline.samples.iter().all(|s| s.abs() <= 1.0));
        assert!(a.pipeline.samples.iter().any(|s| s.abs() > 0.01));
    }

    fn info(kind: BackendKind) -> BackendInfo {
        BackendInfo {
            kind,
            device: "dev".into(),
            model: "ggml-tiny.en".into(),
        }
    }

    #[test]
    fn transcribe_backend_check_needs_info_and_log_to_agree() {
        assert!(check_backend(BackendArg::Cpu, &info(BackendKind::Cpu), LogBackend::Cpu).is_ok());
        assert!(
            check_backend(BackendArg::Cuda, &info(BackendKind::Cuda), LogBackend::Cuda).is_ok()
        );
        let e = check_backend(BackendArg::Cuda, &info(BackendKind::Cpu), LogBackend::Cpu)
            .unwrap_err()
            .to_string();
        assert!(e.contains("--backend cuda requested"), "{e}");
        assert!(
            check_backend(
                BackendArg::Cuda,
                &info(BackendKind::Cuda),
                LogBackend::Unknown
            )
            .is_err()
        );
        assert!(
            check_backend(
                BackendArg::Cpu,
                &info(BackendKind::Cpu),
                LogBackend::Unknown
            )
            .is_err()
        );
        assert!(check_backend(BackendArg::Cpu, &info(BackendKind::Cuda), LogBackend::Cpu).is_err());
        // Vulkan (T6.10d): the same rule with its own log classification.
        assert!(
            check_backend(
                BackendArg::Vulkan,
                &info(BackendKind::Vulkan),
                LogBackend::Vulkan
            )
            .is_ok()
        );
        for log in [LogBackend::Cpu, LogBackend::Cuda, LogBackend::Unknown] {
            assert!(
                check_backend(BackendArg::Vulkan, &info(BackendKind::Vulkan), log).is_err(),
                "{log:?}"
            );
        }
        let e = check_backend(BackendArg::Vulkan, &info(BackendKind::Cpu), LogBackend::Cpu)
            .unwrap_err()
            .to_string();
        assert!(e.contains("--backend vulkan requested"), "{e}");
    }

    #[test]
    fn transcribe_vulkan_needs_a_vulkan_build() {
        assert_eq!(
            parse(&["in.wav", "--backend", "vulkan"]).unwrap().backend,
            Some(BackendArg::Vulkan)
        );
        assert_eq!(BackendArg::Vulkan.kind(), BackendKind::Vulkan);
        assert_eq!(
            check_backend_built(BackendArg::Vulkan).is_ok(),
            cfg!(feature = "vulkan")
        );
        if !cfg!(feature = "vulkan") {
            let e = check_backend_built(BackendArg::Vulkan).unwrap_err();
            assert!(e.to_string().contains("--features vulkan"), "{e}");
        }
    }

    #[test]
    fn transcribe_formats_times() {
        assert_eq!(format_time(0), "00:00.000");
        assert_eq!(format_time(61_234), "01:01.234");
        assert_eq!(format_time(3_600_000 + 5_007), "1:00:05.007");
    }

    #[test]
    fn transcribe_stream_push_is_ten_ms() {
        assert_eq!(STREAM_PUSH_MS * u64::from(PIPELINE_RATE.hz()) / 1000, 160);
    }

    #[test]
    fn transcribe_segment_json_and_line() {
        let raw = RawSegment {
            start: MeetingTime::from_millis(1_000),
            end: MeetingTime::from_millis(2_500),
            text: " hello world ".into(),
            words: vec![
                Word::new("hello", 1_000, 1_500, 0.75),
                Word::new("world", 1_600, 2_500, 0.25),
            ],
        };
        let s = OutSegment::from_raw(&raw);
        assert_eq!(s.text, "hello world");
        assert!((s.confidence - 0.5).abs() < 1e-6);
        assert_eq!(s.to_line(), "[00:01.000 -> 00:02.500] hello world");
        let j = s.to_json();
        assert_eq!(j["start_ms"], 1_000);
        assert_eq!(j["words"][1]["text"], "world");
        assert_eq!(j["words"][1]["end_ms"], 2_500);
        assert_eq!(j["words"][0]["probability"], 0.75);
    }
}
