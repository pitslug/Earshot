//! M2 on the production setup (`cargo xtask accept m2`, PLAN.md 4.2; the owner, and
//! PLAN D22, 2026-11-02): `asr.stream_determinism`, `asr.wer_aec_delta`, `asr.wer_ls_default`
//! and `asr.wer_ami_ihm` (plus the informational `ami_sdm`) measured with the transcription
//! model and the live mode real meetings use, through the runtime's own constructors.
//!
//! ```text
//! cargo build --release -p earshot-runtime [--features cuda] --example m2_prod
//! m2_prod <repo-root> --backend cpu|cuda --models <dir> --fixtures <dir> [--metric <id>]...
//!     [--max-piece <s>]
//! ```
//!
//! Prints one JSON object on stdout (`default_model`, `loads`, `metrics`, `informational`);
//! progress on stderr. `--metric` selects metrics or the informational `ami_sdm` (for
//! development; `accept m2` runs everything). The other M2 metrics (`asr.wer_smoke_base`,
//! `asr.wer_ls_base`, `asr.echo_leak`, on base.en, and the base.en values of the two deltas
//! under their earlier definitions, as informational entries) come from `asr_eval` in
//! earshot-asr-whisper.
//!
//! Setup (fixed before measuring):
//!
//! - **Model** ([`default_model`]): the production default for meetings in English on
//!   `--backend`: the first rung of the model check's English ladder for that backend
//!   (`model_check::candidates`, mirrored from `models/manifest.toml` `[[ladder]]` and
//!   ordered by `model_check::QUALITY_ORDER`), named by `backends::asr_model`. On CUDA and on
//!   the CPU that is `parakeet-tdt-0.6b-v2`. The timed model check (T6.10c) starts at or
//!   below that rung and moves up only when it is comfortably fast, so on a host that runs
//!   it in real time it is the check's answer; the real-time factor of every decode is
//!   reported. A whisper default fails the run: this example cannot report whisper's
//!   temperature-fallback counts through the runtime's transcriber (T2.4d).
//! - **Backend**: `--backend` alone (`backends::Accelerator::of`), so a model that does not
//!   verify there is an error, never a run elsewhere; ONNX Runtime's library is chosen by
//!   `backends::onnx_library` and loaded by `backends::init_onnx` as the CLI does; Silero
//!   VAD runs on the CPU (`backends::load_silero`, as in production, T6.11). The backend is
//!   proven by the runtime's loaders (ONNX Runtime node placement for Parakeet,
//!   `backends::check_placement`; whisper.cpp's log for whisper) and re-checked on every
//!   result's `BackendInfo`.
//! - **Live transcription**: the track a live session builds, from the runtime's own
//!   constructors: `backends::live_asr_factory` (the model on the live thread budget for
//!   two tracks, Silero, `ChunkerConfig::default()` whose decoding options the stretches
//!   use) and `LiveTrack::with_stretches` with the piece config of `LiveConfig::default()`
//!   (`LiveTranscription::Stretches`, the production default live mode, T6.13b). It is fed
//!   the audio quantised to 16-bit PCM (`audio::quantise_pcm16`) as the stretch worker
//!   feeds it (T6.13e), in 10 ms pushes as a capture device delivers them, with `run`
//!   after every push under the default pipeline's partial policy and the runtime's
//!   backlog cap (`live::live_asr_backlog`), then `finish`. Unlike a live session it runs
//!   on the evaluation's own thread rather than a `LiveWorker`, and without the capture
//!   pipeline (no canceller or resampling: the corpus audio is already 16 kHz mono); for
//!   `asr.wer_aec_delta` the canceller output stands in for the pipeline's. The live
//!   transcript is the words of every live final.
//! - **Batch transcription** (`asr.wer_ls_default`, `asr.wer_ami_ihm`, `ami_sdm`): one
//!   call of the final-pass transcriber (`backends::final_pass_asr`, as below) per utterance
//!   or oracle segment with `TranscribeOptions::batch()` (Parakeet decodes greedily whatever
//!   the options say; the options are reported anyway), no VAD pieces; the hypothesis is
//!   the words production keeps (`refs::kept_words`) joined by `refs::hypothesis_text`.
//! - **Final pass without stretch reuse**: the transcriber `backends::post_processor`
//!   would load (`backends::final_pass_asr`) and `asr::transcribe_in_pieces` with Silero
//!   and `PieceConfig::default()` (as `post_processor` sets it), with the options a live
//!   stretch track hands the final pass (`LiveTrack::final_pass`: the stretches' own,
//!   greedy, D20), on the same 16-bit-quantised audio (the final pass reads the meeting's
//!   16-bit FLAC). This is the path the final pass takes when nothing is reusable. The
//!   final transcript is the words production keeps (`refs::kept_words`: non-speech
//!   annotations dropped, as `Transcription::into_segments`).
//!
//! Metrics (WER: corpus WER over summed `WerCounts` after Whisper's English normalisation,
//! PLAN.md 4.0; hypothesis text `refs::hypothesis_text`):
//!
//! - `asr.stream_determinism` (count; `asr.wer_stream_delta` until T2.5e):
//!   the test-clean utterances of the pinned LibriSpeech subset, in subset order, concatenated
//!   into streams of about 60 s (`refs::group_streams`, as `asr_eval` groups them). Each stream
//!   is transcribed by the production live transcription above (a fresh live track per
//!   stream, as each meeting track starts one) and by the final pass without stretch reuse on
//!   the same audio. The value is the number of words that differ between the two, after
//!   Whisper's English normalisation: substitutions, deletions and insertions of the live
//!   text aligned to the final-pass text, summed over the streams. By D20 the two decode the
//!   same pieces, so anything but 0 is a determinism or stretch-equality regression, not an
//!   accuracy loss; accuracy is `asr.wer_ami_ihm` and `asr.wer_ls_default`. The live and
//!   final-pass WER against the references are reported beside it. It is not the final pass
//!   with reuse against the live stretches (that is the same transcript by construction).
//! - `asr.wer_aec_delta` (points): the double-talk renders of `asr_eval` (`aec_renders.rs`,
//!   shared by path: `double-talk-ser-0` gating and `double-talk-ser-10` informational, the
//!   T2.4g seed rule `refs::render_seeds` with at least 300 near-end reference words per
//!   scenario, the default canceller with its latency removed). Each render is transcribed
//!   the way production transcribes the mic track for its final transcript: the whole track,
//!   once from the canceller output and once from the clean near end (as it reaches the mic,
//!   with its room response), each by the final pass without stretch reuse (by D20 the live
//!   stretches a meeting reuses are the same transcript). Echo cleaning (text
//!   de-duplication and the speaker check against the reference track) is not applied: it
//!   is not part of what the canceller does to near-end words, and its near-end loss is
//!   floored by `accept m2`'s `asr.echo_leak` recall checks. Every complete near-end
//!   utterance is scored with its smoke transcript against the words whose start lies in its
//!   scoring window (`refs::windows`: halfway into the gaps to its neighbours, 0.5 s before
//!   the first and after the last); words outside every window are counted as `unscored`
//!   and reported. The value is the SER 0 scenario's canceller corpus WER minus its clean
//!   corpus WER, summed over all seeds; per-seed deltas, their spread, and per utterance
//!   the canceller SI-SDR against the clean near end over the window (diagnostic) are
//!   reported.
//! - `asr.wer_ls_default` (%): every test-clean utterance of the pinned LibriSpeech subset
//!   (`fixtures/librispeech/subset.toml`), each one batch call (above); corpus WER.
//! - `asr.wer_ami_ihm` (%): the AMI IHM condition with oracle segmentation, scored the
//!   Kaldi/ESPnet way (T2.5b; rule in `ami.rs`, moved from `asr_eval` unchanged), each oracle
//!   segment one batch call on its speaker's headset.
//!
//! Informational (under `informational`, never in `metrics`): `ami_sdm` (T2.5c), the same
//! segments, references and scoring decoded from the single distant microphone `Array1-01`.

mod ami;

#[allow(dead_code)] // shared with asr_eval; this example uses the stream and window helpers
#[path = "../../../earshot-asr-whisper/examples/asr_eval/refs.rs"]
mod refs;

#[path = "../../../earshot-asr-whisper/examples/asr_eval/aec_renders.rs"]
mod aec_renders;

use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use aec_renders::{
    AEC_EDGE_S, AEC_MAX_SEEDS, AEC_MIN_WORDS, AEC_SCENARIOS, AEC_SEED_STEP, SmokeClip, aec_pool,
    aec_renders,
};
use earshot_core::asr::{
    BackendInfo, BackendKind, CancelToken, PieceConfig, TranscribeOptions, Transcriber,
    TranscriptEvent, Word, transcribe_in_pieces,
};
use earshot_core::audio::{MonoBuffer, PIPELINE_RATE, quantise_pcm16};
use earshot_core::capture::TrackId;
use earshot_core::identity::EntityId;
use earshot_core::metrics::sisdr::si_sdr;
use earshot_core::metrics::wer::{WerCounts, word_errors, word_errors_normalized};
use earshot_core::pipeline::{LiveTrack, PartialPolicy, PipelineEvent};
use earshot_core::sim::ScenarioFile;
use earshot_onnx::silero::SileroVad;
use earshot_runtime::backends::{
    Accelerator, AnyTranscriber, AsrModel, EvidenceLoader, ModelDir, VerifiedWhisper, asr_model,
    final_pass_asr, init_onnx, live_asr_factory, load_silero, onnx_library,
};
use earshot_runtime::live::{LiveConfig, LiveTranscription, live_asr_backlog};
use earshot_runtime::model_check;
use earshot_runtime::{SystemClock, os_seeded_rng};
use serde_json::{Value, json};

/// The metrics this example measures (PLAN.md 4.2, milestone m2).
const METRICS: [&str; 4] = [
    "asr.stream_determinism",
    "asr.wer_ls_default",
    "asr.wer_ami_ihm",
    "asr.wer_aec_delta",
];
/// Informational results (under `informational`, never in `metrics`), selectable with
/// `--metric`.
const INFORMATIONAL: [&str; 1] = [ami::INFO_AMI_SDM];
/// Target length of a stream (PLAN.md 4.0: about 60 s), as `asr_eval`.
const STREAM_TARGET_S: f64 = 60.0;
/// Audio per live push (one 10 ms WASAPI period).
const STREAM_PUSH: usize = 160;
/// The decoders this example accepts as the default and what their decoding is.
const FALLBACK_NOTE: &str =
    "no temperature fallback: Parakeet decodes greedily whatever the options ask";
const FETCH_LIBRISPEECH: &str = "run `cargo xtask fetch-fixtures --set librispeech`";

fn fail(msg: impl std::fmt::Display) -> ! {
    eprintln!("m2_prod: {msg}");
    std::process::exit(2);
}

struct Args {
    root: PathBuf,
    backend: BackendKind,
    models: ModelDir,
    fixtures: PathBuf,
    only: Vec<String>,
    /// Evaluation only (T12.24): `PieceConfig::max_piece` in seconds for the live stretches and
    /// the final pass alike; `None` (always, for `accept m2`) is production's config.
    max_piece: Option<u64>,
}

fn parse_args() -> Args {
    let mut it = std::env::args().skip(1);
    let root = PathBuf::from(
        it.next()
            .unwrap_or_else(|| fail("usage: m2_prod <repo-root> ...")),
    );
    let (mut backend, mut models, mut fixtures, mut only) = (None, None, None, Vec::new());
    let mut max_piece = None;
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
            "--models" => models = Some(ModelDir::new(value())),
            "--fixtures" => fixtures = Some(PathBuf::from(value())),
            "--max-piece" => {
                let s: u64 = value()
                    .parse()
                    .unwrap_or_else(|e| fail(format!("--max-piece: {e}")));
                max_piece = Some(s);
            }
            "--metric" => {
                let id = value();
                if !METRICS.contains(&id.as_str()) && !INFORMATIONAL.contains(&id.as_str()) {
                    fail(format!("unknown metric {id}"));
                }
                only.push(id);
            }
            other => fail(format!("unknown argument {other}")),
        }
    }
    Args {
        root,
        backend: backend.unwrap_or_else(|| fail("--backend is required")),
        models: models.unwrap_or_else(|| fail("--models is required")),
        fixtures: fixtures.unwrap_or_else(|| fail("--fixtures is required")),
        only,
        max_piece,
    }
}

impl Args {
    fn wants(&self, id: &str) -> bool {
        self.only.is_empty() || self.only.iter().any(|o| o == id)
    }
}

fn backend_name(kind: BackendKind) -> &'static str {
    match kind {
        BackendKind::Cpu => "cpu",
        BackendKind::Cuda => "cuda",
        _ => fail(format!("unsupported backend {kind:?}")),
    }
}

/// The production default transcription model for English meetings on `backend` (module
/// docs): the first rung of the model check's English ladder.
fn default_model(backend: BackendKind) -> Option<&'static str> {
    model_check::candidates(backend, &[]).first().copied()
}

/// The words of a stream whose start lies in `window` (samples), in order.
fn words_in(words: &[Word], window: &Range<usize>) -> Vec<Word> {
    words
        .iter()
        .filter(|w| {
            let at = w.start.to_pipeline_frames();
            usize::try_from(at).is_ok_and(|at| window.contains(&at))
        })
        .cloned()
        .collect()
}

/// How many `words` start outside every window.
fn unscored(words: &[Word], windows: &[Range<usize>]) -> usize {
    words
        .iter()
        .filter(|w| {
            let at = usize::try_from(w.start.to_pipeline_frames()).unwrap_or(usize::MAX);
            !windows.iter().any(|r| r.contains(&at))
        })
        .count()
}

/// Corpus WER accumulator.
#[derive(Default)]
struct Score {
    counts: WerCounts,
    items: usize,
    audio_s: f64,
    decode_s: f64,
}

impl Score {
    fn add(&mut self, counts: WerCounts, samples: usize, decode: Duration) {
        self.counts += counts;
        self.items += 1;
        self.audio_s += samples as f64 / f64::from(PIPELINE_RATE.hz());
        self.decode_s += decode.as_secs_f64();
    }

    fn percent(&self) -> f64 {
        self.counts
            .wer()
            .unwrap_or_else(|| fail("no reference words scored"))
            * 100.0
    }

    fn json(&self) -> Value {
        let c = self.counts;
        json!({
            "wer_percent": c.wer().map(|w| w * 100.0),
            "substitutions": c.substitutions,
            "deletions": c.deletions,
            "insertions": c.insertions,
            "ref_words": c.ref_words,
            "items": self.items,
            "audio_s": self.audio_s,
            "decode_s": self.decode_s,
            "rtf": self.decode_s / self.audio_s.max(1e-9),
        })
    }
}

/// The production pieces: the live session's stretch config and partial policy.
struct LiveSetup {
    pieces: PieceConfig,
    policy: PartialPolicy,
    cap: u64,
}

impl LiveSetup {
    fn production() -> Self {
        let cfg = LiveConfig::default();
        let LiveTranscription::Stretches(pieces) = cfg.transcription.clone() else {
            fail("the default live mode is no longer speech stretches; update m2_prod");
        };
        Self {
            pieces,
            policy: cfg.pipeline.partials,
            cap: live_asr_backlog(&cfg),
        }
    }
}

/// `pieces` with `max_piece` set to `seconds` when given (T12.24, evaluation only); a value
/// the config refuses ends the run.
fn pieces_with_max(mut pieces: PieceConfig, seconds: Option<u64>) -> PieceConfig {
    if let Some(s) = seconds {
        pieces.max_piece = Duration::from_secs(s);
        pieces
            .validate()
            .unwrap_or_else(|e| fail(format!("--max-piece: {e}")));
    }
    pieces
}

impl LiveSetup {
    /// The same setup with the stretches' `max_piece` overridden (`--max-piece`).
    fn with_max_piece(mut self, seconds: Option<u64>) -> Self {
        self.pieces = pieces_with_max(self.pieces, seconds);
        self
    }
}

/// Everything loaded once: the live factory, the final-pass transcriber and its VAD.
struct Prod {
    name: &'static str,
    backend: BackendKind,
    factory: earshot_runtime::live::LiveAsrFactory,
    live: LiveSetup,
    final_t: AnyTranscriber<VerifiedWhisper>,
    final_vad: SileroVad,
    /// The final pass's piece config: `PieceConfig::default()` unless `--max-piece`.
    final_pieces: PieceConfig,
    /// The options a live stretch track hands the final pass (`LiveTrack::final_pass`).
    final_options: TranscribeOptions,
    /// The live transcription's decoding options (`asr.config.options`).
    live_options: TranscribeOptions,
    loads: Vec<Value>,
    /// The last final-pass piece report (diagnostic).
    last_report: Value,
}

/// A fresh production live track (module docs), with its options and backend.
/// The track, its decoding options, backend and backend evidence (ONNX placement).
fn live_track(prod: &mut Prod) -> (LiveTrack, TranscribeOptions, BackendInfo, String) {
    let track = (prod.factory)(TrackId::Mic, EntityId::from_bytes([1; 16]))
        .unwrap_or_else(|e| fail(format!("live ASR: {e}")))
        .unwrap_or_else(|| fail("the live factory gave no transcription"));
    if !track.fallbacks.is_empty() {
        fail(format!("live ASR refused backends: {:?}", track.fallbacks));
    }
    let evidence = track
        .evidence
        .clone()
        .unwrap_or_else(|| fail("the live factory reported no ONNX placement"));
    let options = track.asr.config.options.clone();
    let backend = track.asr.transcriber.backend_info();
    if backend.kind != prod.backend {
        fail(format!("live ASR runs on {:?}", backend.kind));
    }
    let live = LiveTrack::with_stretches(TrackId::Mic, track.asr, &prod.live.pieces)
        .unwrap_or_else(|e| fail(format!("live stretches: {e:?}")));
    (live, options, backend, evidence)
}

fn load(args: &Args) -> Prod {
    let name = default_model(args.backend)
        .unwrap_or_else(|| fail(format!("no English ladder for {:?}", args.backend)));
    match asr_model(name).unwrap_or_else(|e| fail(e)) {
        AsrModel::Parakeet(_) => {}
        AsrModel::Whisper(_) => fail(format!(
            "the default model {name} is a whisper model; m2_prod reports Parakeet only (module docs)"
        )),
    }
    let accel = Accelerator::of(args.backend)
        .unwrap_or_else(|| fail(format!("no accelerator for {:?}", args.backend)));
    let order = vec![accel];
    let library = onnx_library(name, &order);
    let ort =
        init_onnx(&args.models, library).unwrap_or_else(|e| fail(format!("ONNX Runtime: {e}")));
    eprintln!(
        "m2_prod: default model {name} on {:?}; ONNX Runtime {}",
        args.backend,
        ort.display()
    );
    let started = Instant::now();
    let factory = live_asr_factory(
        Arc::new(EvidenceLoader::for_live(2)),
        args.models.clone(),
        name.to_owned(),
        order.clone(),
        Accelerator::Cpu,
        Arc::new(SystemClock),
        os_seeded_rng(),
    );
    let choice = final_pass_asr(&EvidenceLoader::default(), &args.models, name, &order)
        .unwrap_or_else(|e| fail(format!("final-pass model: {e}")));
    if choice.backend != args.backend || !choice.rejected.is_empty() {
        fail(format!(
            "final-pass model on {:?} (refused {:?})",
            choice.backend, choice.rejected
        ));
    }
    let final_vad = load_silero(&args.models, Accelerator::Cpu)
        .unwrap_or_else(|e| fail(format!("Silero: {e}")));
    let final_load_s = started.elapsed().as_secs_f64();
    let final_info = choice.transcriber.backend_info();
    let final_evidence = choice.transcriber.backend_evidence();
    let mut prod = Prod {
        name,
        backend: args.backend,
        factory,
        live: LiveSetup::production().with_max_piece(args.max_piece),
        final_t: choice.transcriber,
        final_vad,
        final_pieces: pieces_with_max(PieceConfig::default(), args.max_piece),
        final_options: TranscribeOptions::live(),
        live_options: TranscribeOptions::live(),
        loads: Vec::new(),
        last_report: Value::Null,
    };
    // One live track to read the options production uses (the final pass of a stretch
    // track decodes with the stretches' own options).
    let started = Instant::now();
    let (track, live_options, live_info, live_evidence) = live_track(&mut prod);
    let live_load_s = started.elapsed().as_secs_f64();
    prod.final_options = track.final_pass().options;
    prod.live_options = live_options;
    drop(track);
    let requested = backend_name(args.backend);
    prod.loads = vec![
        json!({
            "model": name,
            "role": "live",
            "requested_backend": requested,
            "backend_info": live_info,
            "evidence": live_evidence,
            "load_s": live_load_s,
        }),
        json!({
            "model": name,
            "role": "final pass",
            "requested_backend": requested,
            "backend_info": final_info,
            "evidence": final_evidence,
            "load_s": final_load_s,
        }),
    ];
    prod
}

/// The production live transcript of `audio` (module docs) and its decode time.
fn live_words(prod: &mut Prod, audio: &[f32]) -> (Vec<Word>, Duration) {
    let (mut track, _, _, _) = live_track(prod);
    let mut events = Vec::new();
    let started = Instant::now();
    for chunk in audio.chunks(STREAM_PUSH) {
        track.feed(chunk, prod.live.cap, &mut events);
        track.run(prod.live.policy, &mut events);
    }
    track.finish(prod.live.policy, &mut events);
    let elapsed = started.elapsed();
    let backend = track
        .stretches_backend()
        .cloned()
        .unwrap_or_else(|| fail("live track has no stretches backend"));
    if backend.kind != prod.backend {
        fail(format!("live stretches ran on {:?}", backend.kind));
    }
    let mut words = Vec::new();
    for e in events {
        match e {
            PipelineEvent::Transcript {
                event: TranscriptEvent::Final(s),
                ..
            } => words.extend(s.words),
            PipelineEvent::Transcript { .. } => fail("a stretch track emitted a partial"),
            PipelineEvent::Error(e) => fail(format!("live ASR: {e:?}")),
            _ => {}
        }
    }
    (words, elapsed)
}

/// The final pass without stretch reuse over `audio` (module docs): kept words and decode
/// time.
fn final_words(prod: &mut Prod, audio: &[f32]) -> (Vec<Word>, Duration) {
    let buffer = MonoBuffer::new(PIPELINE_RATE, audio.to_vec());
    let started = Instant::now();
    let (t, report) = transcribe_in_pieces(
        &mut prod.final_t,
        &mut prod.final_vad,
        &buffer,
        &prod.final_options,
        &prod.final_pieces,
        &CancelToken::new(),
    )
    .unwrap_or_else(|e| fail(format!("final pass: {e}")));
    let elapsed = started.elapsed();
    if t.backend.kind != prod.backend || prod.final_t.backend_info().kind != prod.backend {
        fail(format!("final pass ran on {:?}", t.backend.kind));
    }
    // Diagnostic: pieces decoded, retried and dropped as repetitive (PostWarning in
    // production), and those whose uncovered speech was decoded again (T2.5d).
    prod.last_report = json!({
        "pieces": report.pieces,
        "retried": report.retried,
        "uncovered": report.uncovered,
        "dropped_ms": report
            .dropped
            .iter()
            .map(|(a, b)| [a.as_millis(), b.as_millis()])
            .collect::<Vec<_>>(),
    });
    (refs::kept_words(&t), elapsed)
}

fn model_json(prod: &Prod) -> Value {
    json!({
        "name": prod.name,
        "backend_info": prod.final_t.backend_info(),
    })
}

fn read_flac(path: &Path) -> Vec<f32> {
    let mut reader = claxon::FlacReader::open(path)
        .unwrap_or_else(|e| fail(format!("open {}: {e}", path.display())));
    let info = reader.streaminfo();
    if info.sample_rate != PIPELINE_RATE.hz() || info.channels != 1 {
        fail(format!("{}: expected 16 kHz mono", path.display()));
    }
    // As asr_eval reads them.
    let scale = 1.0 / f64::from(1u32 << (info.bits_per_sample - 1));
    reader
        .samples()
        .map(|s| {
            let v = s.unwrap_or_else(|e| fail(format!("{}: {e}", path.display())));
            (f64::from(v) * scale) as f32
        })
        .collect()
}

/// Every test-clean utterance of the pinned LibriSpeech subset, in subset order (as
/// `asr_eval`).
fn librispeech_test_clean(args: &Args) -> Vec<SmokeClip> {
    let path = args.root.join("fixtures/librispeech/subset.toml");
    let text =
        std::fs::read_to_string(&path).unwrap_or_else(|e| fail(format!("{}: {e}", path.display())));
    let doc: toml::Table = text
        .parse()
        .unwrap_or_else(|e| fail(format!("{}: {e}", path.display())));
    let entries = doc["utterance"]
        .as_array()
        .unwrap_or_else(|| fail("subset.toml has no utterances"));
    let mut out = Vec::new();
    for e in entries
        .iter()
        .filter(|e| e["split"].as_str() == Some("test-clean"))
    {
        let id = e["id"]
            .as_str()
            .unwrap_or_else(|| fail("utterance without id"));
        let parts: Vec<&str> = id.split('-').collect();
        let [speaker, chapter, _] = parts.as_slice() else {
            fail(format!("bad LibriSpeech id {id}"));
        };
        let dir = args
            .fixtures
            .join("librispeech/test-clean/LibriSpeech/test-clean")
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
            .unwrap_or_else(|| fail(format!("no transcript for {id}")));
        out.push(SmokeClip {
            id: id.to_owned(),
            reference,
            audio: read_flac(&audio_path),
        });
    }
    if out.is_empty() {
        fail("subset.toml lists no test-clean utterances");
    }
    out
}

/// The committed smoke clips with their transcripts.
fn smoke_set(root: &Path) -> Vec<SmokeClip> {
    let dir = root.join("fixtures").join("smoke");
    let text = std::fs::read_to_string(dir.join("transcripts.txt"))
        .unwrap_or_else(|e| fail(format!("read smoke transcripts: {e}")));
    text.lines()
        .filter(|l| !l.trim().is_empty())
        .map(|line| {
            let (id, reference) = line
                .split_once(' ')
                .unwrap_or_else(|| fail(format!("bad transcript line {line}")));
            SmokeClip {
                id: id.to_owned(),
                reference: reference.to_owned(),
                audio: read_flac(&dir.join(format!("{id}.flac"))),
            }
        })
        .collect()
}

/// `asr.stream_determinism` (module docs).
fn stream_determinism(args: &Args, prod: &mut Prod) -> Value {
    const ID: &str = "asr.stream_determinism";
    let set = librispeech_test_clean(args);
    let rate = f64::from(PIPELINE_RATE.hz());
    let durations: Vec<f64> = set.iter().map(|u| u.audio.len() as f64 / rate).collect();
    let streams = refs::group_streams(&durations, STREAM_TARGET_S);
    let (mut live, mut fin) = (Score::default(), Score::default());
    let mut differing = WerCounts::default();
    let mut raw_differing = WerCounts::default();
    let mut per_stream = Vec::new();
    for (i, r) in streams.iter().enumerate() {
        // 16-bit PCM, as the stretch worker and the meeting's FLAC hold it (T6.13e).
        let audio = quantise_pcm16(
            &set[r.clone()]
                .iter()
                .flat_map(|u| u.audio.iter().copied())
                .collect::<Vec<f32>>(),
        );
        let reference: Vec<&str> = set[r.clone()]
            .iter()
            .map(|u| u.reference.as_str())
            .collect();
        let reference = reference.join(" ");
        let (lw, lt) = live_words(prod, &audio);
        let (fw, ft) = final_words(prod, &audio);
        let (lh, fh) = (refs::hypothesis_text(&lw), refs::hypothesis_text(&fw));
        let (cl, cf) = (
            word_errors_normalized(&reference, &lh),
            word_errors_normalized(&reference, &fh),
        );
        let diff = word_errors_normalized(&fh, &lh);
        // Beside it, informational: the same comparison on the raw tokens, so a flap the
        // normaliser maps away (case, punctuation, spelling) still shows (TODO 47).
        let raw = word_errors(&fh, &lh);
        raw_differing += raw;
        differing += diff;
        live.add(cl, audio.len(), lt);
        fin.add(cf, audio.len(), ft);
        eprintln!(
            "m2_prod: {ID}: stream {}/{} ({:.1} s): {} of {} words differ; live {} / final {} \
             errors of {} reference words",
            i + 1,
            streams.len(),
            audio.len() as f64 / rate,
            diff.errors(),
            diff.ref_words,
            cl.errors(),
            cf.errors(),
            cl.ref_words
        );
        per_stream.push(json!({
            "utterances": [set[r.start].id, set[r.end - 1].id],
            "seconds": audio.len() as f64 / rate,
            "ref_words": cl.ref_words,
            "compared_words": diff.ref_words,
            "words_differing": diff.errors(),
            "raw_compared_tokens": raw.ref_words,
            "raw_tokens_differing": raw.errors(),
            "live_errors": cl.errors(),
            "final_errors": cf.errors(),
            "final_pieces": prod.last_report.clone(),
            "reference": reference,
            "live_hypothesis": lh,
            "final_hypothesis": fh,
        }));
    }
    eprintln!(
        "m2_prod: {ID} = {} of {} words differ (WER: live {:.3}%, final pass {:.3}%)",
        differing.errors(),
        differing.ref_words,
        live.percent(),
        fin.percent()
    );
    json!({
        "id": ID,
        "value": differing.errors() as f64,
        "unit": "count",
        "backend": backend_name(prod.backend),
        "model": model_json(prod),
        "set": "LibriSpeech subset test-clean utterances concatenated into ~60 s streams",
        "definition": "words differing, after Whisper normalisation, between production live \
                       transcription (live_asr_factory + LiveTrack::with_stretches, \
                       LiveConfig::default() pieces, 10 ms pushes) and the final pass without \
                       stretch reuse (final_pass_asr + transcribe_in_pieces, \
                       PieceConfig::default(), the stretch track's final-pass options) on the \
                       same audio",
        "streams": streams.len(),
        "compared_words": differing.ref_words,
        "raw_compared_tokens": raw_differing.ref_words,
        "raw_tokens_differing": raw_differing.errors(),
        "differing": {
            "substitutions": differing.substitutions,
            "deletions": differing.deletions,
            "insertions": differing.insertions,
        },
        "live_options": prod.live_options,
        "final_options": prod.final_options,
        "pieces": format!("{:?}", prod.live.pieces),
        "final_piece_config": format!("{:?}", prod.final_pieces),
        "fallback_note": FALLBACK_NOTE,
        "live": live.json(),
        "final_pass": fin.json(),
        "per_stream": per_stream,
    })
}

/// One batch call of the final-pass transcriber on `audio` (module docs): the kept
/// hypothesis text and the decode time; the result's backend is checked.
fn batch_text(prod: &mut Prod, audio: &[f32]) -> (String, Duration) {
    let buffer = MonoBuffer::new(PIPELINE_RATE, audio.to_vec());
    let started = Instant::now();
    let t = prod
        .final_t
        .transcribe(&buffer, &TranscribeOptions::batch(), &CancelToken::new())
        .unwrap_or_else(|e| fail(format!("transcribe: {e}")));
    let elapsed = started.elapsed();
    if t.backend.kind != prod.backend || prod.final_t.backend_info().kind != prod.backend {
        fail(format!("batch transcription ran on {:?}", t.backend.kind));
    }
    (refs::hypothesis_text(&refs::kept_words(&t)), elapsed)
}

/// `asr.wer_ls_default` (module docs).
fn ls_default(args: &Args, prod: &mut Prod) -> Value {
    const ID: &str = "asr.wer_ls_default";
    let set = librispeech_test_clean(args);
    let mut score = Score::default();
    for (i, u) in set.iter().enumerate() {
        let (hyp, d) = batch_text(prod, &u.audio);
        score.add(word_errors_normalized(&u.reference, &hyp), u.audio.len(), d);
        if (i + 1) % 50 == 0 {
            eprintln!("m2_prod: {ID}: {}/{}", i + 1, set.len());
        }
    }
    eprintln!("m2_prod: {ID} = {:.3}%", score.percent());
    json!({
        "id": ID,
        "value": score.percent(),
        "unit": "%",
        "backend": backend_name(prod.backend),
        "model": model_json(prod),
        "set": "LibriSpeech subset, every test-clean utterance",
        "definition": "one batch call of the final-pass transcriber (final_pass_asr) per \
                       utterance, hypothesis refs::kept_words",
        "options": TranscribeOptions::batch(),
        "fallback_note": FALLBACK_NOTE,
        "score": score.json(),
    })
}

/// `asr.wer_ami_ihm`, or the informational `ami_sdm` (module docs, `ami.rs`).
fn ami_metric(args: &Args, prod: &mut Prod, mic: refs::AmiMic) -> Value {
    let set = match mic {
        refs::AmiMic::Ihm => refs::load_ami_ihm(&args.fixtures, &refs::AMI_MEETINGS),
        refs::AmiMic::Sdm => refs::load_ami_sdm(&args.fixtures, &refs::AMI_MEETINGS),
    }
    .unwrap_or_else(|e| fail(e));
    let (value, mut out) = ami::score(mic, &set, |audio| batch_text(prod, audio));
    out["id"] = json!(match mic {
        refs::AmiMic::Ihm => "asr.wer_ami_ihm",
        refs::AmiMic::Sdm => ami::INFO_AMI_SDM,
    });
    out["value"] = json!(value);
    out["unit"] = json!("%");
    out["backend"] = json!(backend_name(prod.backend));
    out["model"] = model_json(prod);
    out["options"] = json!(TranscribeOptions::batch());
    out["fallback_note"] = json!(FALLBACK_NOTE);
    out
}

fn aec_delta(args: &Args, prod: &mut Prod) -> Value {
    const ID: &str = "asr.wer_aec_delta";
    let path = args.root.join("acceptance/aec_scenarios.toml");
    let text =
        std::fs::read_to_string(&path).unwrap_or_else(|e| fail(format!("{}: {e}", path.display())));
    let file: ScenarioFile =
        toml::from_str(&text).unwrap_or_else(|e| fail(format!("{}: {e}", path.display())));
    let pool = aec_pool(smoke_set(&args.root)).unwrap_or_else(|e| fail(e));
    let rate = f64::from(PIPELINE_RATE.hz());
    let edge = (AEC_EDGE_S * rate) as usize;
    let mut gated = None;
    let mut scenarios = Vec::new();
    for (name, ser) in AEC_SCENARIOS {
        let renders = aec_renders(&file, name, ser, &pool).unwrap_or_else(|e| fail(e));
        let (mut clean, mut aec) = (Score::default(), Score::default());
        let (mut per_seed, mut deltas) = (Vec::new(), Vec::new());
        for (seed, render) in &renders {
            let n = render.out.len();
            if render.spans.is_empty() {
                per_seed.push(json!({
                    "seed": seed,
                    "delta_points": null,
                    "clean": {"ref_words": 0},
                    "aec": {"ref_words": 0},
                    "utterances": [],
                    "skipped_cut_off": render.sim.near_regions.len(),
                }));
                continue;
            }
            // The whole mic track, as production transcribes it (module docs).
            let near_clean = &render.sim.near_clean[..n];
            // Transcribed as 16-bit PCM, as production holds the mic track (T6.13e);
            // SI-SDR below is the canceller's, on the unquantised signals.
            let (near_pcm, out_pcm) = (quantise_pcm16(near_clean), quantise_pcm16(&render.out));
            let (cw, ct) = final_words(prod, &near_pcm);
            let (aw, at) = final_words(prod, &out_pcm);
            let windows = refs::windows(&render.spans, n, edge);
            let (mut seed_clean, mut seed_aec) = (Score::default(), Score::default());
            let mut items = Vec::new();
            for (k, (w, &clip)) in windows.iter().zip(&render.clips).enumerate() {
                let u = &pool.near[clip];
                let (ch, ah) = (
                    refs::hypothesis_text(&words_in(&cw, w)),
                    refs::hypothesis_text(&words_in(&aw, w)),
                );
                let (cc, ca) = (
                    word_errors_normalized(&u.reference, &ch),
                    word_errors_normalized(&u.reference, &ah),
                );
                // Decode time is per track; it is booked on the first window.
                let (dc, da) = if k == 0 {
                    (ct, at)
                } else {
                    (Duration::ZERO, Duration::ZERO)
                };
                clean.add(cc, w.len(), dc);
                seed_clean.add(cc, w.len(), dc);
                aec.add(ca, w.len(), da);
                seed_aec.add(ca, w.len(), da);
                let sisdr = si_sdr(&render.out[w.clone()], &near_clean[w.clone()]);
                items.push(json!({
                    "utterance": u.id,
                    "window_s": [w.start as f64 / rate, w.end as f64 / rate],
                    "ref_words": cc.ref_words,
                    "clean_errors": cc.errors(),
                    "aec_errors": ca.errors(),
                    "aec_counts": {"sub": ca.substitutions, "del": ca.deletions, "ins": ca.insertions},
                    "canceller_sisdr_db": sisdr.filter(|v| v.is_finite()),
                    "reference": u.reference,
                    "clean_hypothesis": ch,
                    "aec_hypothesis": ah,
                }));
            }
            let delta = seed_aec.percent() - seed_clean.percent();
            deltas.push(delta);
            eprintln!(
                "m2_prod: {ID}: {name} seed {seed}: aec {:.3}% - clean {:.3}% = {delta:.3} \
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
                "unscored_words": {"clean": unscored(&cw, &windows), "aec": unscored(&aw, &windows)},
                "utterances": items,
                "skipped_cut_off": render.sim.near_regions.len() - render.spans.len(),
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
            "m2_prod: {ID}: {name}: aec {:.3}% - clean {:.3}% = {delta:.3} points over {} \
             words, {} seeds (per seed {lo:.2} to {hi:.2}, mean {mean:.2}, sd {sd:.2})",
            aec.percent(),
            clean.percent(),
            clean.counts.ref_words,
            renders.len()
        );
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
    eprintln!("m2_prod: {ID} = {value:.3} points (SER 0; SER -10 informational)");
    json!({
        "id": ID,
        "value": value,
        "unit": "points",
        "backend": backend_name(prod.backend),
        "model": model_json(prod),
        "set": "double-talk-ser-0 (gating) and double-talk-ser-10 (informational), each \
                rendered with seeds seed + k * 1000 until >= 300 near-end reference words, \
                complete near-end utterances (renders shared with asr_eval)",
        "definition": "whole mic track transcribed by the final pass without stretch reuse \
                       (final_pass_asr + transcribe_in_pieces, PieceConfig::default(), the \
                       stretch track's final-pass options), canceller output vs clean near \
                       end; words scored per utterance window by start time; no echo cleaning",
        "aggregate": "SER 0 scenario: corpus WER (summed WerCounts over all seeds) after the \
                      canceller minus clean",
        "seed_rule": {"step": AEC_SEED_STEP, "min_words": AEC_MIN_WORDS, "max_seeds": AEC_MAX_SEEDS},
        "canceller": "EchoCancellerConfig::default(), output latency removed",
        "options": prod.final_options,
        "fallback_note": FALLBACK_NOTE,
        "scenarios": scenarios,
    })
}

fn main() {
    let args = parse_args();
    let mut prod = load(&args);
    let mut metrics = Vec::new();
    if args.wants("asr.stream_determinism") {
        metrics.push(stream_determinism(&args, &mut prod));
    }
    if args.wants("asr.wer_ls_default") {
        metrics.push(ls_default(&args, &mut prod));
    }
    if args.wants("asr.wer_ami_ihm") {
        metrics.push(ami_metric(&args, &mut prod, refs::AmiMic::Ihm));
    }
    if args.wants("asr.wer_aec_delta") {
        metrics.push(aec_delta(&args, &mut prod));
    }
    let mut informational = Vec::new();
    if args.wants(ami::INFO_AMI_SDM) {
        informational.push(ami_metric(&args, &mut prod, refs::AmiMic::Sdm));
    }
    let rule = format!(
        "the model check's first English rung for {} (model_check::candidates; ladder {:?}, \
         QUALITY_ORDER rank {}): the check's answer on a host that runs it in real time (the \
         timed check itself is not run here); without a recorded check the CLI falls back to \
         base.en",
        backend_name(args.backend),
        model_check::candidates(args.backend, &[]),
        model_check::quality_rank(prod.name)
    );
    let out = json!({
        "selected_backend": backend_name(args.backend),
        "max_piece_override_s": args.max_piece,
        "default_model": {"name": prod.name, "rule": rule},
        "loads": prod.loads,
        "metrics": metrics,
        "informational": informational,
    });
    println!("{out}");
}

#[cfg(test)]
mod tests {
    use super::*;
    use earshot_core::asr::MeetingTime;

    fn word(text: &str, start_ms: u64) -> Word {
        Word {
            text: text.into(),
            start: MeetingTime::from_millis(start_ms),
            end: MeetingTime::from_millis(start_ms + 100),
            probability: 1.0,
        }
    }

    #[test]
    fn m2_prod_default_model_is_the_first_english_rung() {
        assert_eq!(
            default_model(BackendKind::Cuda),
            Some("parakeet-tdt-0.6b-v2")
        );
        assert_eq!(
            default_model(BackendKind::Cpu),
            Some("parakeet-tdt-0.6b-v2")
        );
        // The top of the ladder is the most accurate model the ladder holds.
        let ladder = model_check::candidates(BackendKind::Cuda, &[]);
        let best = ladder
            .iter()
            .min_by_key(|m| model_check::quality_rank(m))
            .copied();
        assert_eq!(default_model(BackendKind::Cuda), best);
        assert!(matches!(
            asr_model(default_model(BackendKind::Cuda).unwrap()),
            Ok(AsrModel::Parakeet(_))
        ));
    }

    #[test]
    fn m2_prod_words_are_scored_in_the_window_their_start_lies_in() {
        // 16 samples per ms: windows [0, 8000) = 0-500 ms and [8000, 16000) = 500-1000 ms.
        let words = [
            word("a", 0),
            word("b", 499),
            word("c", 500),
            word("d", 999),
            word("e", 1000),
        ];
        let windows = [0..8_000, 8_000..16_000];
        let texts = |w: &Range<usize>| {
            words_in(&words, w)
                .into_iter()
                .map(|w| w.text)
                .collect::<Vec<_>>()
        };
        assert_eq!(texts(&windows[0]), ["a", "b"]);
        assert_eq!(texts(&windows[1]), ["c", "d"]);
        // A word starting after the last window is counted, never dropped silently.
        assert_eq!(unscored(&words, &windows), 1);
        assert_eq!(unscored(&words, &[]), words.len());
    }

    #[test]
    fn m2_prod_max_piece_changes_only_max_piece_and_none_is_production() {
        assert_eq!(
            pieces_with_max(PieceConfig::default(), None),
            PieceConfig::default()
        );
        let p = pieces_with_max(PieceConfig::default(), Some(15));
        assert_eq!(p.max_piece, Duration::from_secs(15));
        assert_eq!(
            PieceConfig {
                max_piece: PieceConfig::default().max_piece,
                ..p
            },
            PieceConfig::default()
        );
        assert_eq!(
            LiveSetup::production().with_max_piece(None).pieces,
            LiveSetup::production().pieces
        );
    }

    #[test]
    fn m2_prod_live_setup_is_the_runtime_default() {
        let s = LiveSetup::production();
        assert_eq!(
            LiveConfig::default().transcription,
            LiveTranscription::Stretches(s.pieces.clone())
        );
    }
}
