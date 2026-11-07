//! M2 evaluation for `cargo xtask accept m2` (PLAN.md T2.5).
//!
//! `cargo run --release -p earshot-asr-whisper [--features cuda] --example asr_eval --
//!  <repo-root> --backend cpu|cuda --models <dir> --fixtures <dir> [--metric <id>]...`
//!
//! Prints one JSON object on stdout with one entry per `asr.*` threshold of M2 it measures
//! (or only the `--metric` ids given, for development). `asr.stream_determinism` (the old
//! `asr.wer_stream_delta`) and `asr.wer_aec_delta` are measured on the production setup by earshot-runtime's `m2_prod`
//! example since 2026-10-24 (the owner); their earlier base.en definitions stay here as
//! `informational` entries (checks in `accept m2`, never measurements), so the history
//! stays comparable. Since PLAN D22 (tests use only the default models)
//! every model this example loads for a metric is base.en, a default; `asr.wer_ls_default`,
//! `asr.wer_ami_ihm` and the informational AMI SDM number moved to `m2_prod` (the
//! production default model), and tiny.en and large-v3-turbo are no longer loaded (only
//! the opt-in `word_times` evaluation names other models). Every model load goes through
//! `earshot_asr_whisper::evidence::load_verified`: the backend recorded for a metric is the
//! one whisper.cpp's own log proves and `backend_info()` agrees with, otherwise the run
//! fails. `--backend` is the backend for the metrics PLAN.md 4.2 does not pin to the CPU.
//!
//! Metric definitions (PLAN.md 4.0: corpus WER = summed `WerCounts` after Whisper
//! normalisation; batch = `TranscribeOptions::batch()` (beam 5, temperature 0), streaming =
//! `ChunkerConfig::default()` with `TranscribeOptions::live()` (greedy, temperature 0), both
//! with Whisper's temperature fallback (T2.4d); every metric reports how many decoding
//! windows fell back (`fallback`, and `batch_fallback` / `stream_fallback` for the stream
//! delta)):
//!
//! - `asr.wer_smoke_base` (%): base.en on the CPU, batch, the 20 committed smoke clips.
//! - `asr.wer_ls_base` (%): base.en on the CPU, batch, every test-clean utterance of the
//!   pinned LibriSpeech subset (`fixtures/librispeech/subset.toml`, enrolment and test
//!   roles; dev-clean is the voiceprint calibration split and is not scored).
//! - `informational.wer_stream_delta_base_chunker` (points, informational; the definition of
//!   `asr.wer_stream_delta` until 2026-10-24): base.en on `--backend`. The same test-clean
//!   utterances, in subset order, concatenated into streams of about 60 s
//!   (`refs::group_streams`); each stream is transcribed once in batch and once through
//!   the streaming chunker with Silero VAD (CPU ONNX Runtime), fed in 10 ms pushes like a
//!   capture device, each stream on a fresh whisper state (`stream_text`); the value is
//!   streaming corpus WER minus batch corpus WER over the same streams and references.
//! - `informational.wer_aec_delta_base_batch` (points, informational; the definition of
//!   `asr.wer_aec_delta` until 2026-10-24): base.en on `--backend`. The gating double-talk
//!   scenarios at SER 0 and SER -10 of `acceptance/aec_scenarios.toml`
//!   (`double-talk-ser-0`, `double-talk-ser-10`) are simulated exactly as `accept m1` does
//!   and run through the echo canceller (default config, output latency removed); the
//!   renders are shared with `m2_prod` (`aec_renders.rs`). Every
//!   near-end utterance the simulator placed completely is scored with its smoke
//!   transcript, on a window reaching halfway into the gaps to its neighbours (and 0.5 s
//!   before the first and after the last), so residual echo between utterances is scored
//!   too; the last utterance, cut off by the end of the call, is not scored. Each window is
//!   transcribed in batch from the canceller output and from the clean near end (the near
//!   end as it reaches the mic, with its room response). Each scenario is rendered with
//!   several fixed seeds (its own seed, then `seed + k * 1000`) until the scored
//!   utterances hold at least 300 reference words (T2.4g: one seed held 35 words, so one
//!   error was 2.9 points); the seeds follow from the simulations alone. The value is the
//!   SER 0 scenario's canceller corpus WER minus clean corpus WER, with `WerCounts` summed
//!   over all seeds; the per-seed deltas and their spread are reported. SER -10 is
//!   reported as informational (PLAN.md 4.2).
//! - `asr.echo_leak` (%): base.en on `--backend`: far-end words left in the mic ("Me")
//!   transcript after cross-track echo de-duplication (T2.4e), as a share of far-end
//!   words, over far-only and double-talk (SER 0, -10) scenarios chosen by their fields,
//!   mono and stereo; each live stream decoded on a fresh whisper state (`leak_stream`), so
//!   the value does not depend on what was decoded before; see `echo_leak`.
//!
//! Informational, not thresholds (printed under `informational`, never in `metrics`):
//!
//! - `word_times` (T2.2d; only when named with `--metric word_times`, never in a run without
//!   `--metric`): word start error of token-level timestamps against DTW per whisper model
//!   on AMI IHM forced alignments, and their speed; rule in `word_times.rs`. Options:
//!   `--word-time-models a,b` (default: every model with pinned alignment heads),
//!   `--word-time-corpora ami,smoke` (default both).

mod aec_renders;
// Shared with m2_prod, model_eval and others by path: since PLAN D22 the AMI scoring
// (`AmiHeadset::score`, `load_ami_sdm`) runs only in m2_prod; refs_tests still covers it here.
#[allow(dead_code)]
mod refs;
#[cfg(test)]
mod refs_tests;
mod word_times;

use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use aec_renders::{
    AEC_EDGE_S, AEC_MAX_SEEDS, AEC_MIN_WORDS, AEC_SCENARIOS, AEC_SEED_STEP, SmokeClip, aec_pool,
    aec_renders,
};
use earshot_asr_whisper::evidence::{self, BackendEvidence};
use earshot_asr_whisper::{FallbackStats, WhisperConfig, WhisperTranscriber, WordTiming};
use earshot_core::aec::delay::DelayState;
use earshot_core::aec::eval::{Class, classify, compensate};
use earshot_core::aec::{BLOCK, EchoCanceller, EchoCancellerConfig, HighPass};
use earshot_core::asr::{
    BackendKind, CancelToken, ChunkerConfig, EchoDedupConfig, MeetingTime, StreamingChunker,
    StreamingTranscriber, TranscribeOptions, Transcriber, TranscriptEvent, Word, dedupe_echo,
    normalize_token,
};
use earshot_core::audio::{MonoBuffer, PIPELINE_RATE};
use earshot_core::diar::cluster::calibrated_cluster_threshold;
use earshot_core::diar::{
    Diarization, DiarizationConfig, Diarizer, EmbeddingModelId, SpanEmbedding, SpeakerEchoConfig,
    SpeakerEmbedder, calibrated_speaker_echo_margin, echo_span_pieces, remove_speaker_echo,
    speaker_echo_spans,
};
use earshot_core::identity::EntityId;
use earshot_core::metrics::wer::{WerCounts, word_errors_normalized};
use earshot_core::sim::rumble::{RumbleSet, add_rumble};
use earshot_core::sim::{ScenarioFile, SpeechPool, simulate};
use earshot_onnx::embed::CamppEmbedder;
use earshot_onnx::segment::PyannoteSegmenter;
use earshot_onnx::silero::SileroVad;
use earshot_onnx::{ORT_CPU_DYLIB, ORT_CPU_DYLIB_SHA256, SessionConfig};
use rand_chacha::ChaCha8Rng;
use rand_core::SeedableRng;
use serde_json::{Value, json};

/// Threshold ids this example measures (PLAN.md 4.2, milestone m2; `asr.stream_determinism`,
/// `asr.wer_ls_default`, `asr.wer_ami_ihm` and `asr.wer_aec_delta` are measured by
/// earshot-runtime's `m2_prod`).
const METRICS: [&str; 3] = ["asr.wer_smoke_base", "asr.wer_ls_base", "asr.echo_leak"];
/// Informational entries (`informational` in the output, checks in `accept m2`, never
/// measurements): the base.en values of the two deltas under their definitions until
/// 2026-10-24, kept so the history stays comparable.
const STREAM_BASE: &str = "informational.wer_stream_delta_base_chunker";
const AEC_BASE: &str = "informational.wer_aec_delta_base_batch";
/// Informational results, reported under `informational` (never in `metrics`), each also
/// selectable with `--metric`; `word_times` runs only when selected.
const INFORMATIONAL: [&str; 3] = [STREAM_BASE, AEC_BASE, word_times::ID];

/// Target length of a streaming stream (PLAN.md 4.0: about 60 s).
const STREAM_TARGET_S: f64 = 60.0;
/// Audio per `push` in streaming (one 10 ms WASAPI period, as the CLI).
const STREAM_PUSH: usize = 160;
/// Smoke clips in the committed set (PLAN.md 4.1).
const SMOKE_CLIPS: usize = 20;
/// `asr.echo_leak`: a mic word counts as a leaked far-end word when it matches a word of a
/// far-end utterance (in order) and starts within this distance of that utterance's
/// echo at the mic (its span shifted by the true echo delay). Generous on purpose: the
/// metric must not miss leaks because of recogniser word timing.
const LEAK_SLACK_MS: i64 = 1_500;
/// `asr.echo_leak` double-talk scenarios: these SERs (dB) are scored.
const LEAK_SERS: [f64; 2] = [0.0, -10.0];
/// pyannote segmentation-3.0 and CAM++ in the models directory (set `ci`), as the
/// runtime's post processor uses them.
const SEGMENTATION_MODEL: &str =
    "pyannote-segmentation-3.0/sherpa-onnx-pyannote-segmentation-3-0/model.onnx";
const CAMPPLUS_MODEL: &str = "embedding/3dspeaker_speech_campplus_sv_en_voxceleb_16k.onnx";
/// `--leak-dev` seed offset (development split of `asr.echo_leak`).
const LEAK_DEV_SEED: u64 = 1_000_003;

fn fail(msg: impl std::fmt::Display) -> ! {
    eprintln!("asr_eval: {msg}");
    std::process::exit(2);
}

struct Args {
    root: PathBuf,
    backend: BackendKind,
    models: PathBuf,
    fixtures: PathBuf,
    only: Vec<String>,
    /// `--leak-dev`: `asr.echo_leak` on the development split (LibriSpeech dev-clean
    /// speakers, other seeds) for designing the de-duplication; never used by `accept m2`.
    leak_dev: bool,
    /// `--highpass <variant>` (with `--leak-dev` only): the echo canceller's high-pass
    /// (T6.23b variant evaluation on the development split).
    highpass: Option<HighPass>,
    /// `--rumble <set.toml>` (with `--leak-dev` only): add the rumble dev set's measured
    /// low-frequency rumble to every rendered mic (T6.23b; `sim::rumble::add_rumble`).
    rumble: Option<RumbleSet>,
    /// `--word-time-models` / `--word-time-corpora` (`word_times`).
    word_times: word_times::Options,
}

fn parse_args() -> Args {
    let mut it = std::env::args().skip(1);
    let root = PathBuf::from(
        it.next()
            .unwrap_or_else(|| fail("usage: asr_eval <repo-root> ...")),
    );
    let (mut backend, mut models, mut fixtures, mut only) = (None, None, None, Vec::new());
    let mut leak_dev = false;
    let mut highpass = None;
    let mut rumble = None;
    let mut word_time_options = word_times::Options::default();
    while let Some(flag) = it.next() {
        let mut value = || {
            it.next()
                .unwrap_or_else(|| fail(format!("{flag} needs a value")))
        };
        match flag.as_str() {
            "--backend" => {
                backend = Some(match value().as_str() {
                    "cpu" => BackendKind::Cpu,
                    "cuda" => BackendKind::Cuda,
                    other => fail(format!("unsupported backend {other}")),
                });
            }
            "--models" => models = Some(PathBuf::from(value())),
            "--fixtures" => fixtures = Some(PathBuf::from(value())),
            "--metric" => {
                let id = value();
                if !METRICS.contains(&id.as_str()) && !INFORMATIONAL.contains(&id.as_str()) {
                    fail(format!("unknown metric {id}"));
                }
                only.push(id);
            }
            "--leak-dev" => leak_dev = true,
            "--highpass" => highpass = Some(HighPass::parse(&value()).unwrap_or_else(|e| fail(e))),
            "--rumble" => {
                let path = PathBuf::from(value());
                let text = std::fs::read_to_string(&path)
                    .unwrap_or_else(|e| fail(format!("{}: {e}", path.display())));
                rumble = Some(
                    toml::from_str::<RumbleSet>(&text)
                        .unwrap_or_else(|e| fail(format!("{}: {e}", path.display()))),
                );
            }
            "--word-time-models" => {
                word_time_options.models = word_times::Options::parse_models(&value());
            }
            "--word-time-corpora" => {
                word_time_options.corpora = word_times::Options::parse_corpora(&value());
            }
            other => fail(format!("unknown argument {other}")),
        }
    }
    if (highpass.is_some() || rumble.is_some()) && !leak_dev {
        fail("--highpass and --rumble need --leak-dev (development split only)");
    }
    Args {
        root,
        backend: backend.unwrap_or_else(|| fail("--backend is required")),
        models: models.unwrap_or_else(|| fail("--models is required")),
        fixtures: fixtures.unwrap_or_else(|| fail("--fixtures is required")),
        only,
        leak_dev,
        highpass,
        rumble,
        word_times: word_time_options,
    }
}

impl Args {
    fn wants(&self, id: &str) -> bool {
        self.only.is_empty() || self.named(id)
    }

    /// `id` was selected with `--metric` (opt-in results run only then).
    fn named(&self, id: &str) -> bool {
        self.only.iter().any(|o| o == id)
    }
}

/// A model loaded with verified backend evidence.
struct Loaded {
    transcriber: WhisperTranscriber,
    kind: BackendKind,
    evidence: BackendEvidence,
    report: Value,
    /// Fallback counts when the current metric started.
    metric_start: FallbackStats,
}

/// Fallback counts accumulated between two snapshots, as report JSON.
fn fallback_json(before: FallbackStats, after: FallbackStats) -> Value {
    json!({
        "windows": after.windows - before.windows,
        "fallback_windows": after.fallback_windows - before.fallback_windows,
        "retries": after.retries - before.retries,
    })
}

impl Loaded {
    /// Starts counting temperature fallbacks for a new metric.
    fn begin_metric(&mut self) {
        self.metric_start = self.transcriber.fallback_stats();
    }

    /// Fails unless whisper.cpp's log since `mark` agrees with the load's backend: every
    /// beam-search call re-creates the state and re-runs the GPU initialisation.
    fn check_calls_since(&self, mark: usize) {
        evidence::check_call_log(self.kind, &evidence::logs_since(mark))
            .unwrap_or_else(|e| fail(e));
    }
}

fn backend_name(kind: BackendKind) -> &'static str {
    match kind {
        BackendKind::Cpu => "cpu",
        BackendKind::Cuda => "cuda",
        _ => fail(format!("unsupported backend {kind:?}")),
    }
}

fn load(args: &Args, name: &str, kind: BackendKind) -> Loaded {
    load_timed(args, name, kind, WordTiming::TokenTimestamps)
}

/// [`load`] with word times from `timing`.
fn load_timed(args: &Args, name: &str, kind: BackendKind, timing: WordTiming) -> Loaded {
    // The manifest set that holds `name` (PLAN D22: base.en in ci, small.en in desktop, the
    // models only `word_times` evaluates in choice).
    let set = match name {
        "base.en" => "ci",
        "small.en" => "desktop",
        _ => "choice",
    };
    let path = args.models.join("whisper").join(format!("ggml-{name}.bin"));
    if !path.is_file() {
        fail(format!(
            "whisper model {name} not found at {}; run `cargo xtask fetch-models --set {set}`",
            path.display()
        ));
    }
    let mut config = WhisperConfig::new(&path);
    config.use_gpu = kind != BackendKind::Cpu;
    let cores = std::thread::available_parallelism().map_or(1, NonZeroUsize::get);
    config.threads = NonZeroUsize::new(cores.min(8)).unwrap_or(NonZeroUsize::MIN);
    config.word_timing = timing;
    let started = Instant::now();
    let (transcriber, evidence) = evidence::load_verified(&config, kind)
        .unwrap_or_else(|e| fail(format!("load {name} on {kind:?}: {e}")));
    let info = transcriber.backend_info();
    eprintln!(
        "asr_eval: loaded {} on {:?} ({}), whisper.cpp log: {}",
        info.model,
        info.kind,
        info.device,
        evidence.log.name()
    );
    let report = json!({
        "model": name,
        "path": path.display().to_string(),
        "requested_backend": backend_name(kind),
        "backend_info": info,
        "whisper_log": evidence.log.name(),
        "evidence_lines": evidence.lines,
        "threads": config.threads.get(),
        "word_timing": format!("{timing:?}"),
        "load_s": started.elapsed().as_secs_f64(),
    });
    let metric_start = transcriber.fallback_stats();
    Loaded {
        transcriber,
        kind,
        evidence,
        report,
        metric_start,
    }
}

/// Corpus WER accumulator.
#[derive(Default)]
struct Score {
    counts: WerCounts,
    items: usize,
    audio: Duration,
    decode: Duration,
}

impl Score {
    fn add(&mut self, reference: &str, hypothesis: &str, samples: usize, decode: Duration) {
        self.add_counts(
            word_errors_normalized(reference, hypothesis),
            samples,
            decode,
        );
    }

    fn add_counts(&mut self, counts: WerCounts, samples: usize, decode: Duration) {
        self.counts += counts;
        self.items += 1;
        self.audio += Duration::from_secs_f64(samples as f64 / f64::from(PIPELINE_RATE.hz()));
        self.decode += decode;
    }

    fn percent(&self) -> f64 {
        self.counts
            .wer()
            .unwrap_or_else(|| fail("no reference words scored"))
            * 100.0
    }

    /// The counts, with `wer_percent` null when no reference word was scored (a bucket
    /// whose words all normalise to "", or an empty `outside_segments`); a metric value
    /// still fails then (`percent`).
    fn json(&self) -> Value {
        let c = self.counts;
        json!({
            "wer_percent": c.wer().map(|w| w * 100.0),
            "substitutions": c.substitutions,
            "deletions": c.deletions,
            "insertions": c.insertions,
            "ref_words": c.ref_words,
            "items": self.items,
            "audio_s": self.audio.as_secs_f64(),
            "decode_s": self.decode.as_secs_f64(),
        })
    }
}

fn transcribe(t: &mut Loaded, samples: &[f32], options: &TranscribeOptions) -> (String, Duration) {
    let (out, elapsed) = transcribe_full(t, samples, options);
    let text: Vec<&str> = out.segments.iter().map(|s| s.text.as_str()).collect();
    (text.join(" "), elapsed)
}

/// One verified call (backend checked against the call's log), with its decode time.
fn transcribe_full(
    t: &mut Loaded,
    samples: &[f32],
    options: &TranscribeOptions,
) -> (earshot_core::asr::Transcription, Duration) {
    let buffer = MonoBuffer::new(PIPELINE_RATE, samples.to_vec());
    let mark = evidence::log_mark();
    let started = Instant::now();
    let out = t
        .transcriber
        .transcribe(&buffer, options, &CancelToken::new())
        .unwrap_or_else(|e| fail(format!("transcribe: {e}")));
    let elapsed = started.elapsed();
    t.check_calls_since(mark);
    if out.backend != t.transcriber.backend_info() {
        fail(format!("transcription reports {:?}", out.backend));
    }
    (out, elapsed)
}

fn metric(id: &str, value: f64, unit: &str, model: &Loaded, mut extra: Value) -> Value {
    if !value.is_finite() {
        fail(format!("{id}: value {value} is not finite"));
    }
    extra["id"] = json!(id);
    extra["value"] = json!(value);
    extra["unit"] = json!(unit);
    extra["backend"] = json!(model.evidence.log.name());
    extra["model"] = model.report.clone();
    extra["fallback"] = fallback_json(model.metric_start, model.transcriber.fallback_stats());
    extra
}

fn read_flac(path: &Path) -> Vec<f32> {
    let mut reader = claxon::FlacReader::open(path)
        .unwrap_or_else(|e| fail(format!("open {}: {e}", path.display())));
    let info = reader.streaminfo();
    if info.sample_rate != PIPELINE_RATE.hz() || info.channels != 1 {
        fail(format!("{}: expected 16 kHz mono", path.display()));
    }
    let scale = 1.0 / f64::from(1u32 << (info.bits_per_sample - 1));
    reader
        .samples()
        .map(|s| {
            let v = s.unwrap_or_else(|e| fail(format!("{}: {e}", path.display())));
            (f64::from(v) * scale) as f32
        })
        .collect()
}

/// One scored utterance.
struct Utterance {
    id: String,
    reference: String,
    audio: Vec<f32>,
}

fn smoke_set(root: &Path) -> Vec<Utterance> {
    let dir = root.join("fixtures").join("smoke");
    let text = std::fs::read_to_string(dir.join("transcripts.txt"))
        .unwrap_or_else(|e| fail(format!("read smoke transcripts: {e}")));
    let set: Vec<Utterance> = text
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|line| {
            let (id, reference) = line
                .split_once(' ')
                .unwrap_or_else(|| fail(format!("bad transcript line {line}")));
            Utterance {
                id: id.to_owned(),
                reference: reference.to_owned(),
                audio: read_flac(&dir.join(format!("{id}.flac"))),
            }
        })
        .collect();
    if set.len() != SMOKE_CLIPS {
        fail(format!(
            "smoke set has {} clips, expected {SMOKE_CLIPS}",
            set.len()
        ));
    }
    set
}

const FETCH_LIBRISPEECH: &str = "run `cargo xtask fetch-fixtures --set librispeech`";

/// Every test-clean utterance of the pinned subset, in subset order.
fn librispeech_test_clean(args: &Args) -> Vec<Utterance> {
    librispeech_split(args, "test-clean")
}

/// Every utterance of `split` in the pinned subset, in subset order.
fn librispeech_split(args: &Args, split: &str) -> Vec<Utterance> {
    let path = args.root.join("fixtures/librispeech/subset.toml");
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| fail(format!("read {}: {e}", path.display())));
    let doc: toml::Table = text
        .parse()
        .unwrap_or_else(|e| fail(format!("{}: {e}", path.display())));
    let entries = doc["utterance"]
        .as_array()
        .unwrap_or_else(|| fail("subset.toml has no utterances"));
    let mut out = Vec::new();
    for e in entries {
        if e["split"].as_str() != Some(split) {
            continue;
        }
        let id = e["id"]
            .as_str()
            .unwrap_or_else(|| fail("utterance without id"));
        let parts: Vec<&str> = id.split('-').collect();
        let [speaker, chapter, _] = parts.as_slice() else {
            fail(format!("bad LibriSpeech id {id}"));
        };
        let dir = args
            .fixtures
            .join(format!("librispeech/{split}/LibriSpeech/{split}"))
            .join(speaker)
            .join(chapter);
        let audio_path = dir.join(format!("{id}.flac"));
        if !audio_path.is_file() {
            fail(format!(
                "{} is missing; {FETCH_LIBRISPEECH}",
                audio_path.display()
            ));
        }
        let trans = dir.join(format!("{speaker}-{chapter}.trans.txt"));
        let reference = std::fs::read_to_string(&trans)
            .unwrap_or_else(|e| fail(format!("{}: {e}; {FETCH_LIBRISPEECH}", trans.display())))
            .lines()
            .find_map(|l| {
                l.strip_prefix(id)
                    .and_then(|r| r.strip_prefix(' '))
                    .map(str::to_owned)
            })
            .unwrap_or_else(|| fail(format!("no transcript for {id} in {}", trans.display())));
        out.push(Utterance {
            id: id.to_owned(),
            reference,
            audio: read_flac(&audio_path),
        });
    }
    if out.is_empty() {
        fail(format!("subset.toml lists no {split} utterances"));
    }
    out
}

fn batch_corpus(model: &mut Loaded, set: &[Utterance], label: &str) -> Score {
    let options = TranscribeOptions::batch();
    let mut score = Score::default();
    for (i, u) in set.iter().enumerate() {
        let (hyp, dt) = transcribe(model, &u.audio, &options);
        score.add(&u.reference, &hyp, u.audio.len(), dt);
        if (i + 1) % 50 == 0 {
            eprintln!("asr_eval: {label}: {}/{}", i + 1, set.len());
        }
    }
    score
}

fn wer_metric(id: &str, model: &mut Loaded, set: &[Utterance], what: &str) -> Value {
    model.begin_metric();
    let score = batch_corpus(model, set, id);
    eprintln!("asr_eval: {id} = {:.3}%", score.percent());
    metric(
        id,
        score.percent(),
        "%",
        model,
        json!({
            "set": what,
            "options": TranscribeOptions::batch(),
            "score": score.json(),
        }),
    )
}

fn chunker_json(c: &ChunkerConfig) -> Value {
    let ms = |d: Duration| u64::try_from(d.as_millis()).unwrap_or(u64::MAX);
    json!({
        "push_samples": STREAM_PUSH,
        "max_window_ms": ms(c.max_window),
        "overlap_ms": ms(c.overlap),
        "partial_interval_ms": ms(c.partial_interval),
        "vad": "silero",
        "vad_threshold": c.vad_threshold,
        "min_speech_ms": ms(c.min_speech),
        "min_silence_ms": ms(c.min_silence),
        "speech_pad_ms": ms(c.speech_pad),
        "prompt_chars": c.prompt_chars,
    })
}

fn silero(args: &Args) -> SileroVad {
    let model = args.models.join("silero/silero_vad.onnx");
    if !model.is_file() {
        fail(format!(
            "Silero VAD not found at {}; run `cargo xtask fetch-models --set ci`",
            model.display()
        ));
    }
    earshot_onnx::init(&args.models.join(ORT_CPU_DYLIB), ORT_CPU_DYLIB_SHA256).unwrap_or_else(
        |e| {
            fail(format!(
                "ONNX Runtime: {e}; run `cargo xtask fetch-models --set ci`"
            ))
        },
    );
    SileroVad::load(&model, &SessionConfig::default())
        .unwrap_or_else(|e| fail(format!("Silero VAD: {e}")))
}

/// The streamed text of `audio` (the old `asr.wer_stream_delta`), decoded on a fresh whisper state
/// like `leak_stream`: live decoding keeps its state, whose RNG drives sampled fallback
/// retries, so without the reset a stream's text depends on every stream decoded before.
fn stream_text(
    args: &Args,
    model: &mut Loaded,
    audio: &[f32],
    config: &ChunkerConfig,
) -> (String, Duration) {
    model.transcriber.reset_state();
    let (finals, elapsed) = stream_finals(args, model, audio, config);
    let text: Vec<String> = finals.into_iter().map(|s| s.text).collect();
    (text.join(" "), elapsed)
}

/// The final segments of `audio` streamed through the chunker (live path).
fn stream_finals(
    args: &Args,
    model: &mut Loaded,
    audio: &[f32],
    config: &ChunkerConfig,
) -> (Vec<earshot_core::asr::Segment>, Duration) {
    let mut n: u128 = 0;
    let ids = Box::new(move || {
        n += 1;
        EntityId::from_bytes(n.to_be_bytes())
    });
    let mark = evidence::log_mark();
    let mut chunker = StreamingChunker::new(
        &mut model.transcriber,
        silero(args),
        config.clone(),
        EntityId::from_bytes([0; 16]),
        MeetingTime::ZERO,
        ids,
    )
    .unwrap_or_else(|e| fail(format!("chunker: {e}")));
    let started = Instant::now();
    let mut events = Vec::new();
    for chunk in audio.chunks(STREAM_PUSH) {
        events.extend(
            chunker
                .push(chunk)
                .unwrap_or_else(|e| fail(format!("stream: {e}"))),
        );
    }
    events.extend(
        chunker
            .finish()
            .unwrap_or_else(|e| fail(format!("stream: {e}"))),
    );
    let elapsed = started.elapsed();
    drop(chunker);
    model.check_calls_since(mark);
    let finals = events
        .into_iter()
        .filter_map(|e| match e {
            TranscriptEvent::Final(s) => Some(s),
            _ => None,
        })
        .collect();
    (finals, elapsed)
}

fn stream_delta(args: &Args, model: &mut Loaded, set: &[Utterance]) -> Value {
    model.begin_metric();
    let (mut batch_fallback, mut stream_fallback) =
        (FallbackStats::default(), FallbackStats::default());
    let add = |sum: &mut FallbackStats, before: FallbackStats, after: FallbackStats| {
        sum.windows += after.windows - before.windows;
        sum.fallback_windows += after.fallback_windows - before.fallback_windows;
        sum.retries += after.retries - before.retries;
    };
    const ID: &str = STREAM_BASE;
    let rate = f64::from(PIPELINE_RATE.hz());
    let durations: Vec<f64> = set.iter().map(|u| u.audio.len() as f64 / rate).collect();
    let streams = refs::group_streams(&durations, STREAM_TARGET_S);
    let config = ChunkerConfig::default();
    let (mut batch, mut stream) = (Score::default(), Score::default());
    let mut per_stream = Vec::new();
    for (i, r) in streams.iter().enumerate() {
        let audio: Vec<f32> = set[r.clone()]
            .iter()
            .flat_map(|u| u.audio.iter().copied())
            .collect();
        let reference: Vec<&str> = set[r.clone()]
            .iter()
            .map(|u| u.reference.as_str())
            .collect();
        let reference = reference.join(" ");
        let f0 = model.transcriber.fallback_stats();
        let (b, bt) = transcribe(model, &audio, &TranscribeOptions::batch());
        let f1 = model.transcriber.fallback_stats();
        let (s, st) = stream_text(args, model, &audio, &config);
        let f2 = model.transcriber.fallback_stats();
        add(&mut batch_fallback, f0, f1);
        add(&mut stream_fallback, f1, f2);
        let (cb, cs) = (
            word_errors_normalized(&reference, &b),
            word_errors_normalized(&reference, &s),
        );
        batch.add(&reference, &b, audio.len(), bt);
        stream.add(&reference, &s, audio.len(), st);
        per_stream.push(json!({
            "utterances": [set[r.start].id, set[r.end - 1].id],
            "seconds": audio.len() as f64 / rate,
            "ref_words": cb.ref_words,
            "batch_errors": cb.errors(),
            "stream_errors": cs.errors(),
            "reference": reference,
            "batch_hypothesis": b,
            "stream_hypothesis": s,
        }));
        eprintln!(
            "asr_eval: {ID}: stream {}/{} ({:.1} s): batch {} / stream {} errors of {} words",
            i + 1,
            streams.len(),
            audio.len() as f64 / rate,
            cb.errors(),
            cs.errors(),
            cb.ref_words
        );
    }
    let delta = stream.percent() - batch.percent();
    eprintln!(
        "asr_eval: {ID} = {delta:.3} points (stream {:.3}% - batch {:.3}%)",
        stream.percent(),
        batch.percent()
    );
    metric(
        ID,
        delta,
        "points",
        model,
        json!({
            "set": "LibriSpeech subset test-clean utterances concatenated into ~60 s streams",
            "streams": streams.len(),
            "batch_options": TranscribeOptions::batch(),
            "stream_options": config.options,
            "chunker": chunker_json(&config),
            "batch": batch.json(),
            "stream": stream.json(),
            "per_stream": per_stream,
            "batch_fallback": fallback_json(FallbackStats::default(), batch_fallback),
            "stream_fallback": fallback_json(FallbackStats::default(), stream_fallback),
        }),
    )
}

fn aec_delta(args: &Args, model: &mut Loaded) -> Value {
    model.begin_metric();
    const ID: &str = AEC_BASE;
    let path = args.root.join("acceptance/aec_scenarios.toml");
    let text =
        std::fs::read_to_string(&path).unwrap_or_else(|e| fail(format!("{}: {e}", path.display())));
    let file: ScenarioFile =
        toml::from_str(&text).unwrap_or_else(|e| fail(format!("{}: {e}", path.display())));
    // The scenarios, seeds, canceller runs and utterances shared with m2_prod.
    let smoke = smoke_set(&args.root)
        .into_iter()
        .map(|u| SmokeClip {
            id: u.id,
            reference: u.reference,
            audio: u.audio,
        })
        .collect();
    let pool = aec_pool(smoke).unwrap_or_else(|e| fail(e));
    let rate = f64::from(PIPELINE_RATE.hz());
    let edge = (AEC_EDGE_S * rate) as usize;
    let options = TranscribeOptions::batch();
    let mut gated = None;
    let mut scenarios = Vec::new();
    for (name, ser) in AEC_SCENARIOS {
        let renders = aec_renders(&file, name, ser, &pool).unwrap_or_else(|e| fail(e));
        let (mut clean, mut aec) = (Score::default(), Score::default());
        let mut per_seed = Vec::new();
        let mut deltas = Vec::new();
        for (seed, render) in &renders {
            let (sim, spans) = (&render.sim, &render.spans);
            let ids: Vec<&SmokeClip> = render.clips.iter().map(|&c| &pool.near[c]).collect();
            let n = render.out.len();
            if spans.is_empty() {
                // No complete near-end utterance in this render: listed with zero words and
                // no delta (the report names every seed in `seeds`), outside the spread.
                per_seed.push(json!({
                    "seed": seed,
                    "delta_points": null,
                    "clean": {"ref_words": 0},
                    "aec": {"ref_words": 0},
                    "utterances": [],
                    "skipped_cut_off": sim.near_regions.len(),
                }));
                continue;
            }
            let (mut seed_clean, mut seed_aec) = (Score::default(), Score::default());
            let mut items = Vec::new();
            for (w, u) in refs::windows(spans, n, edge).into_iter().zip(ids) {
                let (c, ct) = transcribe(model, &sim.near_clean[w.clone()], &options);
                let (a, at) = transcribe(model, &render.out[w.clone()], &options);
                let (cc, ca) = (
                    word_errors_normalized(&u.reference, &c),
                    word_errors_normalized(&u.reference, &a),
                );
                for (score, hyp, t) in [(&mut clean, &c, ct), (&mut seed_clean, &c, ct)] {
                    score.add(&u.reference, hyp, w.len(), t);
                }
                for (score, hyp, t) in [(&mut aec, &a, at), (&mut seed_aec, &a, at)] {
                    score.add(&u.reference, hyp, w.len(), t);
                }
                items.push(json!({
                    "utterance": u.id,
                    "window_s": [w.start as f64 / rate, w.end as f64 / rate],
                    "ref_words": cc.ref_words,
                    "clean_errors": cc.errors(),
                    "aec_errors": ca.errors(),
                    "reference": u.reference,
                    "clean_hypothesis": c,
                    "aec_hypothesis": a,
                }));
            }
            let delta = seed_aec.percent() - seed_clean.percent();
            deltas.push(delta);
            eprintln!(
                "asr_eval: {ID}: {name} seed {seed}: aec {:.3}% - clean {:.3}% = {delta:.3} \
                 points over {} words",
                seed_aec.percent(),
                seed_clean.percent(),
                seed_clean.counts.ref_words
            );
            per_seed.push(json!({
                "seed": seed,
                "delta_points": delta,
                "clean": seed_clean.json(),
                "aec": seed_aec.json(),
                "utterances": items,
                "skipped_cut_off": sim.near_regions.len() - spans.len(),
            }));
        }
        let delta = aec.percent() - clean.percent();
        let (lo, hi) = deltas
            .iter()
            .fold((f64::INFINITY, f64::NEG_INFINITY), |(l, h), &d| {
                (l.min(d), h.max(d))
            });
        let mean = deltas.iter().sum::<f64>() / deltas.len() as f64;
        let sd = (deltas.iter().map(|d| (d - mean).powi(2)).sum::<f64>()
            / (deltas.len().max(2) - 1) as f64)
            .sqrt();
        eprintln!(
            "asr_eval: {ID}: {name}: aec {:.3}% - clean {:.3}% = {delta:.3} points over {} \
             words, {} seeds (per seed {lo:.2} to {hi:.2}, mean {mean:.2}, sd {sd:.2})",
            aec.percent(),
            clean.percent(),
            clean.counts.ref_words,
            renders.len()
        );
        // PLAN.md 4.2: SER 0 gates, SER -10 is informational.
        if ser == 0.0 {
            gated = Some(delta);
        }
        scenarios.push(json!({
            "scenario": name,
            "ser_db": ser,
            "gating": ser == 0.0,
            "delta_points": delta,
            "clean": clean.json(),
            "aec": aec.json(),
            "seeds": renders.iter().map(|r| r.0).collect::<Vec<u64>>(),
            "spread": {"min": lo, "max": hi, "mean": mean, "sd": sd},
            "per_seed": per_seed,
        }));
    }
    let value = gated.unwrap_or_else(|| fail(format!("{ID}: no SER 0 scenario")));
    eprintln!("asr_eval: {ID} = {value:.3} points (SER 0; SER -10 informational)");
    metric(
        ID,
        value,
        "points",
        model,
        json!({
            "set": "double-talk-ser-0 (gating) and double-talk-ser-10 (informational), each \
                    rendered with seeds seed + k * 1000 until >= 300 near-end reference words, \
                    complete near-end utterances",
            "aggregate": "SER 0 scenario: corpus WER (summed WerCounts over all seeds) after \
                          the canceller minus clean",
            "seed_rule": {"step": AEC_SEED_STEP, "min_words": AEC_MIN_WORDS, "max_seeds": AEC_MAX_SEEDS},
            "canceller": "EchoCancellerConfig::default(), output latency removed",
            "options": options,
            "scenarios": scenarios,
        }),
    )
}

/// One ground-truth word with the mic-time window its echo may appear in (ms).
struct TruthWord {
    token: String,
    from_ms: i64,
    to_ms: i64,
}

/// Which `words` match `truth` in order, each within its window: a time-constrained
/// longest common subsequence (one maximal matching).
fn matched_in_order(words: &[Word], truth: &[TruthWord]) -> Vec<bool> {
    let tokens: Vec<String> = words.iter().map(|w| normalize_token(&w.text)).collect();
    let hit = |i: usize, k: usize| {
        let t = i64::try_from(words[i].start.as_millis()).unwrap_or(i64::MAX);
        !tokens[i].is_empty()
            && tokens[i] == truth[k].token
            && (truth[k].from_ms..=truth[k].to_ms).contains(&t)
    };
    let (n, m) = (words.len(), truth.len());
    let mut table = vec![vec![0usize; m + 1]; n + 1];
    for i in 0..n {
        for k in 0..m {
            table[i + 1][k + 1] = if hit(i, k) {
                table[i][k] + 1
            } else {
                table[i][k + 1].max(table[i + 1][k])
            };
        }
    }
    let mut mask = vec![false; n];
    let (mut i, mut k) = (n, m);
    while i > 0 && k > 0 {
        if hit(i - 1, k - 1) && table[i][k] == table[i - 1][k - 1] + 1 {
            mask[i - 1] = true;
            i -= 1;
            k -= 1;
        } else if table[i - 1][k] >= table[i][k - 1] {
            i -= 1;
        } else {
            k -= 1;
        }
    }
    mask
}

/// Leak counts of one mic transcript ([`count_leaks`]).
#[derive(Debug, Clone, Copy, Default)]
struct LeakCount {
    /// Far-end words left in the mic transcript.
    leaked: usize,
    /// Mic words attributed to the near end.
    near: usize,
    /// Of `near`, words that also match the far-end truth in order within their echo
    /// window: attributed to the near end only by the benefit of the doubt.
    ambiguous: usize,
}

/// Far-end words leaked into `mic`: the mic words that match the near-end truth are
/// near-end speech (the near end gets the benefit of the doubt, and the ones that could
/// equally be echo are counted as ambiguous); of the rest, those that match the far-end
/// truth in order are leaks.
fn count_leaks(mic: &[Word], far: &[TruthWord], near: &[TruthWord]) -> LeakCount {
    let near_mask = matched_in_order(mic, near);
    let far_mask = matched_in_order(mic, far);
    let rest: Vec<Word> = mic
        .iter()
        .zip(&near_mask)
        .filter(|&(_, n)| !n)
        .map(|(w, _)| w.clone())
        .collect();
    LeakCount {
        leaked: matched_in_order(&rest, far).iter().filter(|&&x| x).count(),
        near: near_mask.iter().filter(|&&x| x).count(),
        ambiguous: near_mask
            .iter()
            .zip(&far_mask)
            .filter(|(n, f)| **n && **f)
            .count(),
    }
}

/// Ground truth words of `utterances` (texts from `texts` by clip index), in time order,
/// each with the window in which its echo (or the near-end speech) reaches the mic:
/// `shift(sample)` maps a span sample to mic time in samples.
fn truth_words(
    utterances: &[earshot_core::sim::PlacedUtterance],
    texts: &[&str],
    shift: impl Fn(usize) -> f64,
) -> (Vec<TruthWord>, usize) {
    let mut sorted: Vec<&earshot_core::sim::PlacedUtterance> = utterances.iter().collect();
    sorted.sort_by_key(|u| u.span.start);
    let ms = |samples: f64| (samples * 1000.0 / f64::from(PIPELINE_RATE.hz())) as i64;
    let (mut out, mut complete_words) = (Vec::new(), 0);
    for u in sorted {
        let (from, to) = (ms(shift(u.span.start)), ms(shift(u.span.end)));
        let tokens: Vec<String> = texts[u.clip]
            .split_whitespace()
            .map(normalize_token)
            .filter(|t| !t.is_empty())
            .collect();
        if u.complete {
            complete_words += tokens.len();
        }
        out.extend(tokens.into_iter().map(|token| TruthWord {
            token,
            from_ms: from - LEAK_SLACK_MS,
            to_ms: to + LEAK_SLACK_MS,
        }));
    }
    (out, complete_words)
}

/// One live stream of `asr.echo_leak` on a fresh whisper state. Live decoding keeps its state
/// from call to call, and whisper.cpp seeds the RNG of sampled fallback retries once per
/// state, so without the reset a retry in one stream changes every later sampled retry and
/// the metric depends on the order of everything decoded before (it moved 0.445% -> 0.534%
/// between two runs). The reset makes each stream a function of its own audio, as each
/// scenario is a separate call; decoding itself (greedy, the same fallback) is unchanged,
/// and production live decoding is not touched.
fn leak_stream(
    args: &Args,
    model: &mut Loaded,
    audio: &[f32],
    config: &ChunkerConfig,
) -> Vec<earshot_core::asr::Segment> {
    model.transcriber.reset_state();
    stream_finals(args, model, audio, config).0
}

/// `asr.echo_leak` (T2.4e, PLAN.md 4.0): far-end words that end up in the "Me"
/// transcript after cleaning, as a share of far-end words. The selection rule is fixed
/// here before measuring:
///
/// - Scenarios by their fields, never by name: every non-informational scenario whose
///   class (`aec::eval::classify`) is far-only, or double talk at an SER in
///   [`LEAK_SERS`] ("leak scenarios"), plus every non-informational no-echo scenario with
///   near-end speech ("near-only": they only measure near-end loss; at least one of them
///   must have far-end speech, like `headphones`, so false deletions while the far end
///   talks without an echo path are exercised). A stereo far end
///   (`stereo` set) runs the stereo canceller on both channels, any other the mono one.
/// - Two conditions per scenario: `aec`, the canceller output, and `raw`, the mic with
///   the canceller bypassed ("even without the echo removal, no duplicate
///   transcription"). The canceller always runs for its delay lock.
/// - Both tracks on the live path (chunker, Silero VAD, greedy with fallback), base.en:
///   the condition's mic audio ("Me") and the reference downmix ("Them"). Cleaning:
///   text de-duplication (`asr::dedupe_echo`), then the final-pass speaker check: both
///   tracks diarised (pyannote segmentation-3.0, CAM++ at its calibrated cluster
///   threshold), mic turns cut into pieces of at most 2 s and embedded, and
///   `diar::speaker_echo_spans` / `remove_speaker_echo` with the AEC delay lock and the
///   model's calibrated margin. Without a lock neither rule removes anything (T2.4f). The
///   acoustic gate was rejected for near-end loss (T2.4e) and removed (T2.4f).
/// - A leak is a kept mic word that matches the true far-end text in order within
///   [`LEAK_SLACK_MS`] of its echo at the mic, after mic words matching the near-end truth
///   are attributed to the near end; mic words so attributed that also match the far-end
///   truth are reported as `ambiguous_words`. The denominator is the words of complete
///   far-end utterances. Leak shares are pooled over the leak scenarios per condition. The value
///   is the `aec` condition (coordinator decision for the owner: the canceller
///   always runs in the product and the speaker check needs its echo-path lock); the
///   `raw` condition is reported as an informational check (`informational_raw`), never
///   gating. On the development split the `aec` leak was 2.01% (acceptance 0.45%): real
///   calls may sit near 2%. Near-end loss is reported per condition: near-end words
///   heard (matching the near-end truth) before cleaning, after text de-duplication and
///   after the speaker check, in leak scenarios and in near-only scenarios (and of those,
///   the ones with far-end speech), and pooled over both (`near_recall_all_percent`, which
///   `accept m2` floors for the aec condition).
fn echo_leak(args: &Args, model: &mut Loaded) -> Value {
    model.begin_metric();
    const ID: &str = "asr.echo_leak";
    let path = args.root.join("acceptance/aec_scenarios.toml");
    let text =
        std::fs::read_to_string(&path).unwrap_or_else(|e| fail(format!("{}: {e}", path.display())));
    let file: ScenarioFile =
        toml::from_str(&text).unwrap_or_else(|e| fail(format!("{}: {e}", path.display())));
    // Acceptance: the smoke pool exactly as `accept m1` builds it (speaker 61 far, 121
    // near) with each scenario's seed. Development (`--leak-dev`): the first two LibriSpeech
    // dev-clean speakers of the pinned subset (far, near) and seeds offset by
    // `LEAK_DEV_SEED`, so the de-duplication is designed without the acceptance outputs.
    let (speech, far_prefix, near_prefix, seed_offset) = if args.leak_dev {
        let dev = librispeech_split(args, "dev-clean");
        let mut speakers: Vec<u32> = dev
            .iter()
            .filter_map(|u| u.id.split('-').next()?.parse().ok())
            .collect();
        speakers.sort_unstable();
        speakers.dedup();
        let [a, b, ..] = speakers.as_slice() else {
            fail("dev-clean needs two speakers");
        };
        (dev, format!("{a}-"), format!("{b}-"), LEAK_DEV_SEED)
    } else {
        let mut smoke = smoke_set(&args.root);
        smoke.sort_by(|a, b| a.id.cmp(&b.id));
        (smoke, "61-".to_owned(), "121-".to_owned(), 0)
    };
    let far: Vec<&Utterance> = speech
        .iter()
        .filter(|u| u.id.starts_with(&far_prefix))
        .collect();
    let near: Vec<&Utterance> = speech
        .iter()
        .filter(|u| u.id.starts_with(&near_prefix))
        .collect();
    let far_audio: Vec<Vec<f32>> = far.iter().map(|u| u.audio.clone()).collect();
    let near_audio: Vec<Vec<f32>> = near.iter().map(|u| u.audio.clone()).collect();
    let far_text: Vec<&str> = far.iter().map(|u| u.reference.as_str()).collect();
    let near_text: Vec<&str> = near.iter().map(|u| u.reference.as_str()).collect();
    let pool = SpeechPool {
        far: &far_audio,
        near: &near_audio,
    };
    let rate = f64::from(PIPELINE_RATE.hz());
    let live = ChunkerConfig::default();
    let words_of = |segs: Vec<earshot_core::asr::Segment>| -> Vec<Word> {
        segs.into_iter().flat_map(|s| s.words).collect()
    };
    let join = |w: &[Word]| {
        w.iter()
            .map(|x| x.text.as_str())
            .collect::<Vec<_>>()
            .join(" ")
    };
    let mut diar = DiarModels::load(args);
    let margins = speaker_margins(args, &diar.model);
    // Pooled counts per condition and margin: [far words, leaked raw, after text, after
    // text + speaker check, near heard, near after text, near after both, ambiguous after
    // both]; near-only scenarios: [near heard, near after both, and the same for those
    // with far-end speech].
    let mut totals = vec![vec![[0usize; 8]; margins.len()]; 2];
    let mut near_only_totals = vec![vec![[0usize; 4]; margins.len()]; 2];
    let mut near_only_with_far = 0usize;
    let mut scenarios = Vec::new();
    for scn in &file.scenario {
        let class = classify(scn).0;
        let leak_scenario = match class {
            Class::FarOnly => true,
            Class::DoubleTalk => LEAK_SERS.contains(&scn.ser_db),
            _ => false,
        };
        let near_only = class == Class::NoEcho && scn.near_speech;
        if scn.informational || !(leak_scenario || near_only) {
            continue;
        }
        if near_only && scn.far_speech {
            near_only_with_far += 1;
        }
        let name = scn.name.as_str();
        let mut rng = ChaCha8Rng::seed_from_u64(scn.seed + seed_offset);
        let mut sim = simulate(scn, &file.setup, &pool, &mut rng)
            .unwrap_or_else(|e| fail(format!("{name}: {e}")));
        if let Some(set) = &args.rumble {
            let mut rng =
                ChaCha8Rng::seed_from_u64((scn.seed + seed_offset).wrapping_add(set.seed_offset));
            add_rumble(&mut sim, set, &mut rng);
        }
        let n = sim.mic.len() / BLOCK * BLOCK;
        let mut aec = EchoCanceller::new(EchoCancellerConfig {
            highpass: args
                .highpass
                .unwrap_or(EchoCancellerConfig::default().highpass),
            ..EchoCancellerConfig::default()
        });
        let mut out = vec![0.0f32; n];
        match &sim.reference_lr {
            Some([l, r]) => aec.process_stereo(&sim.mic[..n], &l[..n], &r[..n], &mut out),
            None => aec.process(&sim.mic[..n], &sim.reference[..n], &mut out),
        }
        let stereo = sim.reference_lr.is_some();
        let lock_ms = match aec.status().delay {
            DelayState::Locked { delay, .. } => Some(delay * 1000.0 / rate),
            _ => None,
        };
        let lock = lock_ms.map(|d| Duration::from_secs_f64(d.max(0.0) / 1000.0));
        let run = compensate(out, Vec::new(), aec.output_latency());
        let ref_words = words_of(leak_stream(args, model, &sim.reference[..n], &live));
        let reference_diar = diar.diarize(&sim.reference[..n], name);
        let dedup = lock.map_or_else(EchoDedupConfig::default, EchoDedupConfig::with_delay);
        let truth_delay = |m: usize| m as f64 + sim.delay.delay_at(m);
        let (far_truth, far_words) = truth_words(&sim.far_utterances, &far_text, truth_delay);
        let (near_truth, _) = truth_words(&sim.near_utterances, &near_text, |m| m as f64);
        let near_words = near_truth.len();
        let mut conditions = Vec::new();
        for (c, (label, audio)) in [("aec", &run.out[..]), ("raw", &sim.mic[..n])]
            .into_iter()
            .enumerate()
        {
            let mic_words = words_of(leak_stream(args, model, audio, &live));
            let text = dedupe_echo(&mic_words, &ref_words, &dedup)
                .unwrap_or_else(|e| fail(format!("{name}: {e}")));
            let mic_diar = diar.diarize(audio, name);
            let spans = diar.mic_spans(audio, &mic_diar);
            let raw_count = count_leaks(&mic_words, &far_truth, &near_truth);
            let text_count = count_leaks(&text.kept, &far_truth, &near_truth);
            let (leak_raw, near_raw) = (raw_count.leaked, raw_count.near);
            let (leak_text, near_text_kept) = (text_count.leaked, text_count.near);
            let mut per_margin = Vec::new();
            for (m, &margin) in margins.iter().enumerate() {
                let mut cfg = SpeakerEchoConfig::for_model(&reference_diar.embedding_model)
                    .unwrap_or_else(|e| fail(format!("{name}: {e}")));
                cfg.margin = margin;
                cfg.delay = lock;
                let echo = speaker_echo_spans(&mic_diar, &reference_diar, &spans, &cfg)
                    .unwrap_or_else(|e| fail(format!("{name}: {e}")));
                let clean = remove_speaker_echo(&text.kept, &spans, &echo);
                let full = count_leaks(&clean.kept, &far_truth, &near_truth);
                let (leak_full, near_full) = (full.leaked, full.near);
                if leak_scenario {
                    let row = [
                        far_words,
                        leak_raw,
                        leak_text,
                        leak_full,
                        near_raw,
                        near_text_kept,
                        near_full,
                        full.ambiguous,
                    ];
                    for (k, v) in row.into_iter().enumerate() {
                        totals[c][m][k] += v;
                    }
                } else {
                    near_only_totals[c][m][0] += near_raw;
                    near_only_totals[c][m][1] += near_full;
                    if scn.far_speech {
                        near_only_totals[c][m][2] += near_raw;
                        near_only_totals[c][m][3] += near_full;
                    }
                }
                if m == 0 {
                    eprintln!(
                        "asr_eval: {ID}: {name} [{label}]: leaked {leak_full} of {far_words} far-end \
                         words (raw {leak_raw}, text only {leak_text}); near-end words heard \
                         {near_raw} -> {near_text_kept} after text -> {near_full} kept \
                         ({} ambiguous)",
                        full.ambiguous
                    );
                }
                per_margin.push(json!({
                    "margin": margin,
                    "leaked": leak_full,
                    "near_kept": near_full,
                    "ambiguous": full.ambiguous,
                    "echo_spans": echo.iter().filter(|&&e| e).count(),
                    "kept": join(&clean.kept),
                    "removed_by_speaker_check": join(&clean.removed),
                }));
            }
            conditions.push(json!({
                "condition": label,
                "leaked_raw": leak_raw,
                "leaked_text": leak_text,
                "near_heard": near_raw,
                "near_after_text": near_text_kept,
                "mic_speakers": mic_diar.speakers.len(),
                "spans": spans.len(),
                "mic_transcript": join(&mic_words),
                "removed_by_text": join(&text.removed),
                "margins": per_margin,
            }));
        }
        scenarios.push(json!({
            "scenario": name,
            "role": if leak_scenario { "leak" } else { "near-only" },
            "stereo": stereo,
            "far_speech": scn.far_speech,
            "near_speech": scn.near_speech,
            "ser_db": scn.near_speech.then_some(scn.ser_db),
            "aec_delay_lock_ms": lock_ms,
            "far_words": far_words,
            "near_words": near_words,
            "reference_speakers": reference_diar.speakers.len(),
            "reference_transcript": join(&ref_words),
            "conditions": conditions,
        }));
    }
    if near_only_with_far == 0 {
        fail(format!(
            "{ID}: no near-only scenario with far-end speech (headphones-style) selected"
        ));
    }
    let share = |num: usize, den: usize| {
        if den == 0 {
            0.0
        } else {
            num as f64 * 100.0 / den as f64
        }
    };
    let mut per_condition = Vec::new();
    let mut value = 0.0f64;
    for (c, label) in ["aec", "raw"].into_iter().enumerate() {
        for (m, &margin) in margins.iter().enumerate() {
            let t = totals[c][m];
            if t[0] == 0 {
                fail(format!("{ID}: no leak scenario selected"));
            }
            let leak = share(t[3], t[0]);
            if m == 0 && c == 0 {
                value = leak;
            }
            let o = near_only_totals[c][m];
            let all = share(t[6] + o[1], t[4] + o[0]);
            eprintln!(
                "asr_eval: {ID} [{label}, margin {margin}]: leak {leak:.2}% (raw {:.2}%, text \
                 only {:.2}%, {} ambiguous); near-end words kept {:.1}% (text only {:.1}%) of \
                 heard in leak scenarios, {:.1}% in near-only ({:.1}% with far-end speech), \
                 {all:.1}% overall",
                share(t[1], t[0]),
                share(t[2], t[0]),
                t[7],
                share(t[6], t[4]),
                share(t[5], t[4]),
                share(o[1], o[0]),
                share(o[3], o[2]),
            );
            per_condition.push(json!({
                "condition": label,
                "margin": margin,
                "leak_percent": leak,
                "leak_raw_percent": share(t[1], t[0]),
                "leak_text_only_percent": share(t[2], t[0]),
                "far_words": t[0],
                "leaked_words": t[3],
                "near_heard": t[4],
                "near_kept_text_only": t[5],
                "near_kept": t[6],
                "near_recall_percent": share(t[6], t[4]),
                "near_recall_text_only_percent": share(t[5], t[4]),
                "near_only_heard": o[0],
                "near_only_kept": o[1],
                "near_only_recall_percent": share(o[1], o[0]),
                "near_only_far_speech_heard": o[2],
                "near_only_far_speech_kept": o[3],
                "near_only_far_speech_recall_percent": share(o[3], o[2]),
                "near_recall_all_percent": all,
                "ambiguous_words": t[7],
            }));
        }
    }
    eprintln!("asr_eval: {ID} = {value:.3}% (aec condition; raw informational)");
    metric(
        ID,
        value,
        "%",
        model,
        json!({
            "set": "non-informational far-only and double-talk (SER 0, -10) scenarios, mono and \
                    stereo by scenario fields; conditions: canceller output (aec) and raw mic \
                    (raw); no-echo scenarios with near-end speech report near-end loss only",
            "aggregate": "pooled leak share of the aec condition (first margin); raw is \
                          informational",
            "dev_note": "development split (--leak-dev) aec leak 2.01% at margin 0.5 (T2.4f): real \
                         calls may sit near 2%",
            "gating_aec": per_condition
                .iter()
                .find(|c| c["condition"] == "aec")
                .cloned()
                .unwrap_or(Value::Null),
            "informational_raw": per_condition
                .iter()
                .find(|c| c["condition"] == "raw")
                .cloned()
                .unwrap_or(Value::Null),
            "cleaning": "text de-duplication (asr::dedupe_echo) + speaker check \
                         (diar::speaker_echo_spans / remove_speaker_echo), both only with an \
                         AEC delay lock",
            "options": TranscribeOptions::live(),
            "decoding_state": "a fresh whisper state per stream (leak_stream), so sampled \
                               fallback retries do not depend on earlier streams",
            "dedup": format!("{:?}", EchoDedupConfig::default()),
            "speaker_margins": margins,
            "leak_slack_ms": LEAK_SLACK_MS,
            "dev_split": args.leak_dev,
            "highpass": args.highpass.map(HighPass::label),
            "rumble": args.rumble.is_some(),
            "conditions": per_condition,
            "scenarios": scenarios,
        }),
    )
}

/// Speaker-check margins: the calibrated margin of `model`, or on the development split
/// only, a sweep from `EARSHOT_LEAK_MARGINS=<m1>,<m2>,...` (the first one sets the value).
fn speaker_margins(args: &Args, model: &EmbeddingModelId) -> Vec<f32> {
    if args.leak_dev
        && let Ok(v) = std::env::var("EARSHOT_LEAK_MARGINS")
    {
        let m: Vec<f32> = v.split(',').filter_map(|x| x.trim().parse().ok()).collect();
        if m.is_empty() {
            fail("EARSHOT_LEAK_MARGINS needs numbers");
        }
        return m;
    }
    vec![
        calibrated_speaker_echo_margin(model)
            .unwrap_or_else(|| fail(format!("no calibrated speaker-check margin for {model:?}"))),
    ]
}

/// pyannote segmentation and CAM++ for the speaker check (CPU ONNX Runtime), as the
/// runtime's post processor loads them.
struct DiarModels {
    parts: Option<(PyannoteSegmenter, CamppEmbedder)>,
    config: DiarizationConfig,
    min_samples: usize,
    /// The embedder's model id.
    model: EmbeddingModelId,
}

impl DiarModels {
    fn load(args: &Args) -> Self {
        let need = |rel: &str| {
            let p = args.models.join(rel);
            if !p.is_file() {
                fail(format!(
                    "{} not found; run `cargo xtask fetch-models --set ci`",
                    p.display()
                ));
            }
            p
        };
        earshot_onnx::init(&args.models.join(ORT_CPU_DYLIB), ORT_CPU_DYLIB_SHA256)
            .unwrap_or_else(|e| fail(format!("ONNX Runtime: {e}")));
        let seg = PyannoteSegmenter::load(&need(SEGMENTATION_MODEL), &SessionConfig::default(), 32)
            .unwrap_or_else(|e| fail(format!("segmentation: {e}")));
        let emb = CamppEmbedder::load(&need(CAMPPLUS_MODEL), &SessionConfig::default())
            .unwrap_or_else(|e| fail(format!("CAM++: {e}")));
        let model = emb.info().model.clone();
        let min_samples = emb.info().min_samples;
        let threshold = calibrated_cluster_threshold(&model)
            .unwrap_or_else(|| fail(format!("no calibrated cluster threshold for {model:?}")));
        let mut config = DiarizationConfig::default();
        config.clustering.threshold = threshold;
        DiarModels {
            parts: Some((seg, emb)),
            config,
            min_samples,
            model,
        }
    }

    fn diarize(&mut self, audio: &[f32], name: &str) -> Diarization {
        let (seg, emb) = self
            .parts
            .take()
            .unwrap_or_else(|| fail("diarisation models gone"));
        let mut d = Diarizer::new(seg, emb, self.config.clone())
            .unwrap_or_else(|e| fail(format!("{name}: {e}")));
        let out = d
            .diarize(
                &MonoBuffer::new(PIPELINE_RATE, audio.to_vec()),
                MeetingTime::ZERO,
                &CancelToken::new(),
            )
            .unwrap_or_else(|e| fail(format!("{name}: diarisation: {e}")));
        self.parts = Some(d.into_parts());
        out
    }

    /// The mic turns cut by `diar::echo_span_pieces` (pieces of at most 2 s; a remainder
    /// shorter than the embedder's minimum joins the previous piece), embedded with CAM++.
    fn mic_spans(&mut self, audio: &[f32], mic: &Diarization) -> Vec<SpanEmbedding> {
        // The one cutting rule, shared with the runtime's final pass.
        let pieces = echo_span_pieces(&mic.turns, PIPELINE_RATE, audio.len(), self.min_samples);
        let (_, emb) = self
            .parts
            .as_mut()
            .unwrap_or_else(|| fail("diarisation models gone"));
        let slices: Vec<&[f32]> = pieces.iter().map(|p| &audio[p.samples.clone()]).collect();
        let embeddings = emb
            .embed(&slices, &CancelToken::new())
            .unwrap_or_else(|e| fail(format!("CAM++: {e}")));
        pieces
            .into_iter()
            .zip(embeddings)
            .filter(|(_, e)| e.is_valid())
            .map(|(p, embedding)| SpanEmbedding {
                start: p.start,
                end: p.end,
                embedding,
            })
            .collect()
    }
}

fn main() {
    let args = parse_args();
    evidence::capture_logs(false).unwrap_or_else(|e| fail(e));
    let mut metrics = Vec::new();
    let mut informational = Vec::new();
    let mut loads = Vec::new();
    let selected = args.backend;

    let wants_ls = ["asr.wer_ls_base", STREAM_BASE]
        .iter()
        .any(|id| args.wants(id));
    let ls = if wants_ls {
        librispeech_test_clean(&args)
    } else {
        Vec::new()
    };
    const LS_SET: &str = "LibriSpeech subset, every test-clean utterance";

    // The two CPU-pinned batch metrics share one base.en load (each counts its own
    // fallbacks from `begin_metric`).
    if args.wants("asr.wer_smoke_base") || args.wants("asr.wer_ls_base") {
        let mut base = load(&args, "base.en", BackendKind::Cpu);
        if args.wants("asr.wer_smoke_base") {
            let set = smoke_set(&args.root);
            metrics.push(wer_metric(
                "asr.wer_smoke_base",
                &mut base,
                &set,
                "committed smoke set (20 clips)",
            ));
        }
        if args.wants("asr.wer_ls_base") {
            metrics.push(wer_metric("asr.wer_ls_base", &mut base, &ls, LS_SET));
        }
        loads.push(base.report);
    }

    if args.wants(STREAM_BASE) || args.wants(AEC_BASE) || args.wants("asr.echo_leak") {
        let mut base = load(&args, "base.en", selected);
        if args.wants(STREAM_BASE) {
            informational.push(stream_delta(&args, &mut base, &ls));
        }
        if args.wants(AEC_BASE) {
            informational.push(aec_delta(&args, &mut base));
        }
        if args.wants("asr.echo_leak") {
            metrics.push(echo_leak(&args, &mut base));
        }
        loads.push(base.report);
    }

    if args.named(word_times::ID) {
        informational.push(word_times::word_times(&args, &args.word_times));
    }

    let out = json!({
        "selected_backend": backend_name(selected),
        "loads": loads,
        "metrics": metrics,
        "informational": informational,
    });
    println!("{out}");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn asr_eval_score_json_reports_counts_without_a_rate_when_no_word_was_scored() {
        let mut empty = Score::default();
        // A reference that normalises to nothing ("um" is a filler Whisper's normaliser drops).
        empty.add("um", "", 16_000, Duration::ZERO);
        let v = empty.json();
        assert!(v["wer_percent"].is_null(), "{v}");
        assert_eq!(v["ref_words"], 0);
        assert_eq!(v["items"], 1);
        assert!(Score::default().json()["wer_percent"].is_null());
        let mut some = Score::default();
        some.add("hello world", "hello word", 16_000, Duration::ZERO);
        assert_eq!(some.json()["wer_percent"], 50.0);
        assert_eq!(some.json()["substitutions"], 1);
    }
}
