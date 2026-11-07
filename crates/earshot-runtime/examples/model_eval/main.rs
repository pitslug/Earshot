//! Transcription model evaluation (T6.10e): every candidate model measured the same way,
//! so the per-backend ladder is ordered by measurement, not reputation.
//!
//! ```text
//! cargo run --release -p earshot-runtime [--features cuda|vulkan|qnn] --example model_eval -- //!     --root <repo> --backend cpu|cuda|vulkan|qnn --models <dir> --fixtures <dir> //!     --out <file.json> [--model <name>]... //!     [--corpus librispeech|smoke|ami|ami_sdm|gate12|silence|speed]... [--decoding greedy|beam]...
//!     [--gate12-dir <raw recording dir, relative to --root>]
//! ```
//!
//! NPU conversions (T8.3): the `*-qnn` candidates are the static-shape QDQ encoders built by
//! `cargo xtask qnn-convert`; they run only when named with `--model`. On `--backend cpu`
//! they measure what quantisation costs against the float model; on `--backend qnn` (a
//! `qnn` build: the NPU on Windows ARM64, Qualcomm's HTP simulator on x64) the encoder must
//! run wholly on the QNN EP and the prediction network and joint on the CPU, proven from the
//! node placement.
//!
//! Run through `cargo xtask model-eval`. Prints progress on stderr and writes one JSON report.
//!
//! Every candidate runs the production decoding path: Silero VAD speech pieces
//! (`asr::transcribe_in_pieces` with `PieceConfig::default()`, as the final pass since T6.16
//! and the single pass of D20), on `--backend`, with the T6.10b thread budget for a two-track
//! meeting (`backends::live_whisper_threads`: 2 on a GPU; on the CPU half the cores split over
//! two tracks, at most 4 per track; ONNX Runtime gets the same intra-op count). Whisper runs
//! greedy (`TranscribeOptions::live()`) and beam 5 (`TranscribeOptions::batch()`); Parakeet
//! decodes greedily whatever is asked (its only mode), reported as `greedy`. The backend is
//! proven per model: whisper through the runtime's `EvidenceLoader` (whisper.cpp's log, checked
//! after every call; a call that fell back to the CPU fails the run), Parakeet through ONNX
//! Runtime's node placement of every session.
//!
//! `--backend vulkan` (T6.10d) runs whisper models only: Parakeet runs on ONNX Runtime, which
//! has no Vulkan provider here, so without `--model` the Parakeet candidates are skipped (said
//! on stderr and in the report) and naming one is an error. The device is the runtime's
//! choice (the first discrete Vulkan GPU, else the first integrated one); ggml's
//! `GGML_VK_VISIBLE_DEVICES` restricts the devices it sees, for measuring another one.
//!
//! Corpora (all definitions fixed before measuring):
//! - `librispeech`: every test-clean utterance of the pinned subset, one piece run per
//!   utterance; corpus WER (Whisper English normalisation, summed `WerCounts`).
//! - `librispeech_dev` (only when asked for; T8.3c's dev set): every dev-clean utterance of
//!   the pinned subset, scored as `librispeech`. NPU conversions never calibrate on it
//!   (`xtask::qnn_convert::calibration_plan` leaves the pinned subset out).
//! - `smoke` (only when asked for): the 20 committed `fixtures/smoke` clips, scored as
//!   `librispeech` (T8.3's quantisation check).
//! - `ami`: the AMI IHM condition of `accept m2`, by the same code (`asr_eval/refs.rs`, shared
//!   by path: `refs::load_ami_ihm`, `AmiHeadset::score`): IS1009a and ES2004a, every oracle
//!   segment of a speaker (clipped to the UEM) decoded by one call on the speaker's own
//!   headset, with the row's decoding and no VAD (the oracle segment is the piece), aligned
//!   against the speaker's words starting inside it; words outside every segment count as
//!   deletions; corpus WER summed over segments. Word-timestamp quality per headset:
//!   hypothesis words (segment start plus word time) matched in order to the speaker's
//!   in-segment reference words by normalised text (longest common subsequence), absolute
//!   start-time error median, 90th percentile and share within 250 ms (AMI word times are
//!   forced alignments). Until 2026-10-24 this corpus decoded the merged Mix-Headset stream
//!   of each meeting in VAD pieces against all speakers' words (T2.5b review).
//! - `ami_sdm` (only when asked for; informational, T2.5c): the `ami` rule and timing with
//!   the recording changed (`refs::load_ami_sdm`): every oracle segment decoded from the
//!   meeting's single distant microphone `Array1-01` (the first microphone of the first
//!   array, the Kaldi/ESPnet `sdm1` condition) instead of the speaker's headset; overlapping
//!   segments of different speakers are each decoded on their own.
//! - `gate12`: the a private recording raw recording (`fixtures/private`, informational; a visible
//!   skip without it) replayed through the core pipeline with the runtime's live settings, so
//!   the mic is the canceller output the app transcribes. Script recall: share of the
//!   near-end script words (gates/GATE-12.md) found in order in the mic transcript. Far-end
//!   words under "Me": mic words not matched to the script that match, in order and within
//!   1.5 s, the far-end reference, which is the loopback transcribed once by large-v3-turbo
//!   (beam) for every candidate (fixed, so a model that mishears the far end gains nothing);
//!   reported as a share of the reference's far-end words, before and after production echo
//!   cleaning: the final pass's own function (`post::clean_mic_echo`: the text rule against
//!   the candidate's own far-end transcript and the speaker check, per cleaning stretch, i.e.
//!   the canceller lock stretches recorded during the replay as a live session records them
//!   with short gaps bridged, then the echo-cluster drop), on both tracks quantised to 16 bit
//!   as the FLACs store them and diarised once by the production speaker models on the CPU
//!   with the final pass's settings. Since 2026-11-01; before, only the text rule per recorded
//!   stretch ran. Script recall and echo residue are scored after cleaning. Echo residue: mic
//!   words in the windows the private settings file names. `bridged_gaps_with_echo_path` (informational) counts the bridged lock
//!   gaps in which the two quantised tracks show the echo path (`echo::gap_has_echo_path`,
//!   not used by the final pass). Counts only: the report never carries mic transcript
//!   text. `--gate12-dir`
//!   replays another GATE-12 take instead (by its folder);
//!   the script is the same, the residue windows are take 2's, so residue is null for
//!   another take.
//! - `speed` (only when asked for, for CPU and NPU runs where accuracy is already known from
//!   a GPU run: accuracy does not depend on the backend): the first 10 minutes of AMI
//!   IS1009a, real-time factor only.
//! - `silence`: three 30 s synthetic clips decoded directly, without VAD (what a model invents
//!   when non-speech passes the VAD): dither at -90 dBFS, white noise at -60 and -40 dBFS;
//!   words emitted.
//!
//! Every corpus scores the words production keeps (Whisper's non-speech annotations such as
//! "(buzzing)" dropped, T6.7); `silence` also reports the raw text.
//!
//! Speed: decode seconds over audio seconds (real-time factor) per corpus and in total, model
//! load time, on the stated backend and thread count.

#[path = "../../../earshot-asr-whisper/examples/asr_eval/gate12_private.rs"]
mod gate12_private;
#[allow(dead_code)] // shared with asr_eval; only the AMI functions are used here
#[path = "../../../earshot-asr-whisper/examples/asr_eval/refs.rs"]
mod refs;

use std::fs::File;
use std::io::BufReader;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use earshot_core::asr::{
    BackendKind, CancelToken, PieceConfig, TranscribeOptions, Transcriber, Transcription, Word,
    normalize_token, transcribe_in_pieces,
};
use earshot_core::asr::{MeetingTime, Segment};
use earshot_core::audio::{MonoBuffer, PIPELINE_RATE, quantise_pcm16, read_wav};
use earshot_core::capture::{
    AudioSource, RawTimeline, ReplayBuilder, SourceStatus, TrackBuffer, TrackId,
};
use earshot_core::diar::{Diarization, Diarizer, SpeakerEmbedder};
use earshot_core::identity::EntityId;
use earshot_core::metrics::wer::{WerCounts, word_errors_normalized};
use earshot_core::pipeline::{Pipeline, PipelineEvent};
use earshot_onnx::embed::CamppEmbedder;
use earshot_onnx::parakeet::{ParakeetCtc, ParakeetTdt};
use earshot_onnx::segment::PyannoteSegmenter;
use earshot_onnx::silero::SileroVad;
use earshot_onnx::{Provider, SessionBackend, SessionConfig};
use earshot_runtime::backends::{
    Accelerator, EvidenceLoader, ModelDir, SILERO, WhisperLoader, check_qnn_parakeet, init_onnx,
    live_whisper_threads, load_diarisation, whisper_model,
};
use earshot_runtime::echo::{EchoLock, cleaning_stretches_with, gap_has_echo_path, gaps_bridged};
use earshot_runtime::live::LiveConfig;
use earshot_runtime::live::echo::LiveEchoFilter;
use earshot_runtime::post::{MicEcho, PostConfig, clean_mic_echo};
use rand_chacha::ChaCha8Rng;
use rand_core::{Rng, SeedableRng};
use serde_json::{Value, json};

/// Corpora in run order; all but `smoke`, `ami_sdm` and `speed` run by default.
const CORPORA: [&str; 8] = [
    "librispeech",
    "librispeech_dev",
    "smoke",
    "ami",
    "ami_sdm",
    "gate12",
    "silence",
    "speed",
];
const DEFAULT_CORPORA: [&str; 4] = ["librispeech", "ami", "gate12", "silence"];
/// `set` of the `ami` corpus.
const AMI_IHM_SET: &str = "AMI IS1009a + ES2004a, IHM with oracle segmentation (the accept m2 \
    rule, asr_eval/refs.rs AmiHeadset): each annotated segment of a speaker, clipped to the \
    UEM, decoded by one call on the speaker's headset and aligned against the speaker's words \
    starting inside it; words outside every segment count as deletions; references: AMI manual \
    annotations v1.6.2. Since 2026-10-24; before, this corpus was the merged Mix-Headset stream \
    decoded in VAD pieces";
/// `set` of the `ami_sdm` corpus (T2.5c).
const AMI_SDM_SET: &str = "AMI IS1009a + ES2004a, SDM (Array1-01, the first microphone of the \
    first far-field array) with the oracle segments, references and scoring of the ami corpus \
    (asr_eval/refs.rs load_ami_sdm): each annotated segment of a speaker, clipped to the UEM, \
    decoded by one call on the shared Array1-01 recording (overlapping segments of different \
    speakers each decoded on their own) and aligned against that speaker's words starting \
    inside it; words outside every segment count as deletions; references: AMI manual \
    annotations v1.6.2. Informational (T2.5c)";
/// `speed`: the first this many seconds of AMI IS1009a (inside its UEM).
const SPEED_S: f64 = 600.0;
/// A hypothesis word start within this of its reference word counts as well timed.
const TIMING_OK_MS: u64 = 250;
/// A mic word matches a far-end reference word starting at most this far away.
const LEAK_SLACK_MS: u64 = 1_500;
/// The fixed far-end reference model and decoding of `gate12`.
const FAR_REFERENCE_MODEL: &str = "large-v3-turbo";
/// Synthetic non-speech clips: (name, RMS in dBFS of uniform white noise).
const SILENCE_CLIPS: [(&str, f64); 3] = [
    ("dither-90dBFS", -90.0),
    ("noise-60dBFS", -60.0),
    ("noise-40dBFS", -40.0),
];
const SILENCE_CLIP_S: usize = 30;
const SILENCE_SEED: u64 = 0x6e0_15e;

const FETCH_LIBRISPEECH: &str = "run `cargo xtask fetch-fixtures --set librispeech`";

/// A candidate model.
struct Candidate {
    name: &'static str,
    engine: Engine,
    /// Language coverage, from the model card.
    languages: &'static str,
}

#[derive(Clone, Copy)]
enum Engine {
    Whisper,
    ParakeetCtc(&'static str),
    ParakeetTdt {
        dir: &'static str,
        model: &'static str,
        english_only: bool,
    },
}

/// The NPU conversions (T8.3, `cargo xtask qnn-convert`): static-shape QDQ encoders, run on
/// `--backend qnn` (encoder on the NPU, prediction network and joint on the CPU) or on the
/// CPU EP to measure what quantisation costs. Only run when named with `--model`.
const QNN_CANDIDATES: [&str; 3] = [
    "parakeet-tdt-110m-qnn",
    "parakeet-tdt-0.6b-v2-qnn",
    "parakeet-tdt-0.6b-v3-qnn",
];

const CANDIDATES: [Candidate; 13] = [
    Candidate {
        name: "parakeet-tdt-110m-qnn",
        engine: Engine::ParakeetTdt {
            dir: "parakeet-qnn/parakeet-tdt-110m",
            model: "parakeet-tdt_ctc-110m (TDT, NPU conversion)",
            english_only: true,
        },
        languages: "English",
    },
    Candidate {
        name: "parakeet-tdt-0.6b-v2-qnn",
        engine: Engine::ParakeetTdt {
            dir: "parakeet-qnn/parakeet-tdt-0.6b-v2",
            model: "parakeet-tdt-0.6b-v2 (NPU conversion)",
            english_only: true,
        },
        languages: "English",
    },
    Candidate {
        name: "parakeet-tdt-0.6b-v3-qnn",
        engine: Engine::ParakeetTdt {
            dir: "parakeet-qnn/parakeet-tdt-0.6b-v3",
            model: "parakeet-tdt-0.6b-v3 (NPU conversion)",
            english_only: false,
        },
        languages: "25 European languages (as parakeet-tdt-0.6b-v3)",
    },
    Candidate {
        name: "parakeet-ctc-110m",
        engine: Engine::ParakeetCtc("parakeet/tdt_ctc-110m"),
        languages: "English",
    },
    Candidate {
        name: "parakeet-tdt-110m",
        engine: Engine::ParakeetTdt {
            dir: "parakeet/tdt-110m",
            model: "parakeet-tdt_ctc-110m (TDT)",
            english_only: true,
        },
        languages: "English",
    },
    Candidate {
        name: "parakeet-tdt-0.6b-v2",
        engine: Engine::ParakeetTdt {
            dir: "parakeet/tdt-0.6b-v2",
            model: "parakeet-tdt-0.6b-v2",
            english_only: true,
        },
        languages: "English",
    },
    Candidate {
        name: "parakeet-tdt-0.6b-v3",
        engine: Engine::ParakeetTdt {
            dir: "parakeet/tdt-0.6b-v3",
            model: "parakeet-tdt-0.6b-v3",
            english_only: false,
        },
        languages: "25 European languages (bg hr cs da nl en et fi fr de el hu it lv lt mt pl pt ro sk sl es sv ru uk), detected automatically",
    },
    Candidate {
        name: "tiny.en",
        engine: Engine::Whisper,
        languages: "English",
    },
    Candidate {
        name: "base.en",
        engine: Engine::Whisper,
        languages: "English",
    },
    Candidate {
        name: "small.en",
        engine: Engine::Whisper,
        languages: "English",
    },
    Candidate {
        name: "small.en-q5_1",
        engine: Engine::Whisper,
        languages: "English",
    },
    Candidate {
        name: "large-v3-turbo",
        engine: Engine::Whisper,
        languages: "99 languages (Whisper), selectable or detected",
    },
    Candidate {
        name: "large-v3-turbo-q5_0",
        engine: Engine::Whisper,
        languages: "99 languages (Whisper), selectable or detected",
    },
];

fn fail(msg: impl std::fmt::Display) -> ! {
    eprintln!("model_eval: {msg}");
    std::process::exit(1);
}

struct Args {
    root: PathBuf,
    /// The GATE-12 raw recording, relative to `root`.
    /// `--gate12-dir`; without it the private settings name the recording.
    gate12_dir: Option<String>,
    backend: BackendKind,
    models: ModelDir,
    fixtures: PathBuf,
    out: PathBuf,
    names: Vec<String>,
    corpora: Vec<String>,
    decodings: Vec<String>,
}

fn parse_args() -> Args {
    let mut it = std::env::args().skip(1);
    let (mut root, mut backend, mut models, mut fixtures, mut out) = (None, None, None, None, None);
    let (mut names, mut corpora, mut decodings) = (Vec::new(), Vec::new(), Vec::new());
    let mut gate12_dir = None;
    while let Some(flag) = it.next() {
        let mut value = || {
            it.next()
                .unwrap_or_else(|| fail(format!("{flag} needs a value")))
        };
        match flag.as_str() {
            "--root" => root = Some(PathBuf::from(value())),
            "--gate12-dir" => gate12_dir = Some(value()),
            "--backend" => {
                backend = Some(match value().as_str() {
                    "cpu" => BackendKind::Cpu,
                    "cuda" => BackendKind::Cuda,
                    "vulkan" => BackendKind::Vulkan,
                    "qnn" => BackendKind::Qnn,
                    other => fail(format!("--backend cpu|cuda|vulkan|qnn, not {other}")),
                });
            }
            "--models" => models = Some(ModelDir::new(value())),
            "--fixtures" => fixtures = Some(PathBuf::from(value())),
            "--out" => out = Some(PathBuf::from(value())),
            "--model" => {
                let v = value();
                if !CANDIDATES.iter().any(|c| c.name == v) {
                    fail(format!("unknown model {v}"));
                }
                names.push(v);
            }
            "--corpus" => {
                let v = value();
                if !CORPORA.contains(&v.as_str()) {
                    fail(format!("unknown corpus {v}"));
                }
                corpora.push(v);
            }
            "--decoding" => {
                let v = value();
                if v != "greedy" && v != "beam" {
                    fail(format!("--decoding greedy|beam, not {v}"));
                }
                decodings.push(v);
            }
            other => fail(format!("unknown argument {other}")),
        }
    }
    let or_all = |v: Vec<String>, all: &[&str]| {
        if v.is_empty() {
            all.iter().map(|s| (*s).to_owned()).collect()
        } else {
            v
        }
    };
    let backend = backend.unwrap_or_else(|| fail("--backend is required"));
    let whisper_only = backend == BackendKind::Vulkan;
    let is_whisper = |n: &str| {
        CANDIDATES
            .iter()
            .any(|c| c.name == n && matches!(c.engine, Engine::Whisper))
    };
    if whisper_only && let Some(n) = names.iter().find(|n| !is_whisper(n)) {
        fail(format!(
            "{n} runs on ONNX Runtime, which has no Vulkan provider: --backend vulkan evaluates whisper models only"
        ));
    }
    if whisper_only && names.is_empty() {
        let skipped: Vec<&str> = CANDIDATES
            .iter()
            .filter(|c| !matches!(c.engine, Engine::Whisper))
            .map(|c| c.name)
            .collect();
        eprintln!(
            "model_eval: --backend vulkan skips the ONNX Runtime models (no Vulkan provider): {}",
            skipped.join(", ")
        );
        names = CANDIDATES
            .iter()
            .filter(|c| matches!(c.engine, Engine::Whisper))
            .map(|c| c.name.to_owned())
            .collect();
    }
    Args {
        root: root.unwrap_or_else(|| fail("--root is required")),
        gate12_dir,
        backend,
        models: models.unwrap_or_else(|| fail("--models is required")),
        fixtures: fixtures.unwrap_or_else(|| fail("--fixtures is required")),
        out: out.unwrap_or_else(|| fail("--out is required")),
        names: or_all(
            names,
            &CANDIDATES
                .map(|c| c.name)
                .into_iter()
                .filter(|n| !QNN_CANDIDATES.contains(n))
                .collect::<Vec<_>>(),
        ),
        corpora: or_all(corpora, &DEFAULT_CORPORA),
        decodings: or_all(decodings, &["greedy", "beam"]),
    }
}

fn backend_name(kind: BackendKind) -> &'static str {
    match kind {
        BackendKind::Cpu => "cpu",
        BackendKind::Cuda => "cuda",
        BackendKind::Vulkan => "vulkan",
        BackendKind::Qnn => "qnn",
        _ => fail(format!("unsupported backend {kind:?}")),
    }
}

/// CPU threads per model under the T6.10b budget for a two-track meeting.
fn thread_budget(backend: BackendKind) -> usize {
    let cores = std::thread::available_parallelism().map_or(1, std::num::NonZeroUsize::get);
    live_whisper_threads(backend, cores, 2).get()
}

/// A loaded candidate and the evidence of where it runs.
struct Loaded {
    transcriber: Box<dyn Transcriber>,
    evidence: Value,
    load_s: f64,
}

fn onnx_evidence(backends: &[&SessionBackend], want: BackendKind, name: &str) -> Value {
    let encoder = backends[0];
    if encoder.kind() != Some(want) {
        fail(format!(
            "{name}: ONNX Runtime placed the encoder as {encoder}, expected {want:?}"
        ));
    }
    if want == BackendKind::Qnn {
        // The NPU partition (T8.3): the encoder on QNN with only its documented float
        // boundary on the CPU, the per-token steps on the CPU.
        check_qnn_parakeet(backends).unwrap_or_else(|e| fail(format!("{name}: {e}")));
    }
    json!({
        "source": "ONNX Runtime node placement",
        "sessions": backends.iter().map(|b| b.to_string()).collect::<Vec<_>>(),
        "fully_on_requested": backends.iter().map(|b| b.fully_on_requested()).collect::<Vec<_>>(),
    })
}

fn load(args: &Args, c: &Candidate) -> Loaded {
    let threads = thread_budget(args.backend);
    let started = Instant::now();
    let session = SessionConfig {
        provider: match args.backend {
            BackendKind::Cuda => Provider::Cuda { device_id: 0 },
            BackendKind::Qnn => Provider::Qnn,
            _ => Provider::Cpu,
        },
        intra_threads: Some(threads),
        background: None,
    };
    // On QNN the prediction network and joint run on the CPU (the NPU partition, T8.3).
    let steps = if args.backend == BackendKind::Qnn {
        SessionConfig::cpu(threads)
    } else {
        session.clone()
    };
    let eval_dir = |rel: &str| {
        let dir = args.models.root().join(rel);
        if !dir.is_dir() {
            let how = if QNN_CANDIDATES.contains(&c.name) {
                let base = c.name.trim_end_matches("-qnn");
                format!("cargo xtask qnn-convert --model {base}")
            } else {
                "cargo xtask fetch-models --set eval".to_owned()
            };
            fail(format!("{} not found; run `{how}`", dir.display()));
        }
        dir
    };
    let (transcriber, evidence): (Box<dyn Transcriber>, Value) = match c.engine {
        Engine::Whisper => {
            let rel = whisper_model(c.name).unwrap_or_else(|e| fail(e));
            let path = args.models.require(&rel).unwrap_or_else(|e| fail(e));
            let loader = EvidenceLoader {
                threads: std::num::NonZeroUsize::new(threads),
                live_tracks: None,
            };
            let t = loader
                .load_verified(&path, args.backend)
                .unwrap_or_else(|e| fail(format!("{}: {e}", c.name)));
            let ev = json!({
                "source": "whisper.cpp log (EvidenceLoader, re-checked after every call)",
                "backend_info": t.backend_info(),
                "whisper_log": t.evidence().log.name(),
                "lines": t.evidence().lines,
            });
            (Box::new(t), ev)
        }
        Engine::ParakeetCtc(dir) => {
            let t = ParakeetCtc::load(&eval_dir(dir), &session)
                .unwrap_or_else(|e| fail(format!("{}: {e}", c.name)));
            let ev = onnx_evidence(&t.backends(), args.backend, c.name);
            (Box::new(t), ev)
        }
        Engine::ParakeetTdt {
            dir,
            model,
            english_only,
        } => {
            let t = ParakeetTdt::load_split(&eval_dir(dir), &session, &steps, model, english_only)
                .unwrap_or_else(|e| fail(format!("{}: {e}", c.name)));
            if QNN_CANDIDATES.contains(&c.name) != t.is_static() {
                fail(format!(
                    "{}: static encoder {}, expected the opposite",
                    c.name,
                    t.is_static()
                ));
            }
            let ev = onnx_evidence(&t.backends(), args.backend, c.name);
            (Box::new(t), ev)
        }
    };
    Loaded {
        transcriber,
        evidence,
        load_s: started.elapsed().as_secs_f64(),
    }
}

fn options(decoding: &str) -> TranscribeOptions {
    if decoding == "beam" {
        TranscribeOptions::batch()
    } else {
        TranscribeOptions::live()
    }
}

/// Words of a piece run with meeting-relative times, and the decode time.
fn run_pieces(
    model: &mut dyn Transcriber,
    vad: &mut SileroVad,
    samples: &[f32],
    opts: &TranscribeOptions,
    want: BackendKind,
) -> (Vec<Word>, Duration, usize) {
    let audio = MonoBuffer::new(PIPELINE_RATE, samples.to_vec());
    let started = Instant::now();
    let (t, report) = transcribe_in_pieces(
        model,
        vad,
        &audio,
        opts,
        &PieceConfig::default(),
        &CancelToken::new(),
    )
    .unwrap_or_else(|e| fail(format!("transcribe: {e}")));
    let elapsed = started.elapsed();
    check_backend(&t, model, want);
    (words_of(&t), elapsed, report.dropped.len())
}

/// Fails unless the call and the model still report the requested backend (a whisper call
/// that fell back to the CPU is reported as CPU by `VerifiedWhisper`).
fn check_backend(t: &Transcription, model: &dyn Transcriber, want: BackendKind) {
    let now = model.backend_info();
    if t.backend.kind != want || now.kind != want {
        fail(format!(
            "{}: ran on {:?} (model reports {:?}), expected {want:?}",
            now.model, t.backend.kind, now.kind
        ));
    }
}

/// The words production keeps (`refs::kept_words`, shared with `asr_eval`'s AMI metric).
fn words_of(t: &Transcription) -> Vec<Word> {
    refs::kept_words(t)
}

/// The scored hypothesis text (`refs::hypothesis_text`).
fn text_of(words: &[Word]) -> String {
    refs::hypothesis_text(words)
}

fn secs(samples: usize) -> f64 {
    samples as f64 / f64::from(PIPELINE_RATE.hz())
}

/// Decode time and audio of one corpus.
#[derive(Default, Clone, Copy)]
struct Speed {
    audio_s: f64,
    decode_s: f64,
}

impl Speed {
    fn add(&mut self, samples: usize, d: Duration) {
        self.audio_s += secs(samples);
        self.decode_s += d.as_secs_f64();
    }

    fn json(self) -> Value {
        json!({
            "audio_s": self.audio_s,
            "decode_s": self.decode_s,
            "rtf": self.decode_s / self.audio_s.max(1e-9),
        })
    }
}

fn wer_json(c: WerCounts) -> Value {
    json!({
        "wer_percent": c.wer().map(|w| w * 100.0),
        "substitutions": c.substitutions,
        "deletions": c.deletions,
        "insertions": c.insertions,
        "ref_words": c.ref_words,
    })
}

/// Index pairs of a longest common subsequence of `a` and `b` under `same`.
fn lcs_pairs(a: usize, b: usize, same: impl Fn(usize, usize) -> bool) -> Vec<(usize, usize)> {
    // Directions: 0 diagonal (match), 1 up (skip a), 2 left (skip b).
    let mut dir = vec![0u8; (a + 1) * (b + 1)];
    let mut prev = vec![0u32; b + 1];
    let mut cur = vec![0u32; b + 1];
    for i in 1..=a {
        cur[0] = 0;
        for j in 1..=b {
            let (v, d) = if same(i - 1, j - 1) {
                (prev[j - 1] + 1, 0)
            } else if prev[j] >= cur[j - 1] {
                (prev[j], 1)
            } else {
                (cur[j - 1], 2)
            };
            cur[j] = v;
            dir[i * (b + 1) + j] = d;
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    let (mut i, mut j, mut out) = (a, b, Vec::new());
    while i > 0 && j > 0 {
        match dir[i * (b + 1) + j] {
            0 => {
                out.push((i - 1, j - 1));
                i -= 1;
                j -= 1;
            }
            1 => i -= 1,
            _ => j -= 1,
        }
    }
    out.reverse();
    out
}

fn tokens(words: &[Word]) -> Vec<String> {
    words.iter().map(|w| normalize_token(&w.text)).collect()
}

fn same_token(a: &[String], b: &[String]) -> impl Fn(usize, usize) -> bool {
    |i, j| !a[i].is_empty() && a[i] == b[j]
}

// ---- LibriSpeech -----------------------------------------------------------------------

struct Utterance {
    reference: String,
    audio: Vec<f32>,
}

fn read_flac(path: &Path) -> Vec<f32> {
    let (rate, samples) =
        earshot_store::read_flac(path).unwrap_or_else(|e| fail(format!("{}: {e}", path.display())));
    if rate != PIPELINE_RATE.hz() {
        fail(format!("{}: expected 16 kHz", path.display()));
    }
    samples
}

/// The pinned subset's utterances of `split` (`test-clean` or `dev-clean`).
fn librispeech(args: &Args, split: &str) -> Vec<Utterance> {
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
        .filter(|e| e["split"].as_str() == Some(split))
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
            .unwrap_or_else(|| fail(format!("no transcript for {id}")));
        out.push(Utterance {
            reference,
            audio: read_flac(&audio_path),
        });
    }
    if out.is_empty() {
        fail(format!("subset.toml lists no {split} utterances"));
    }
    out
}

/// The committed smoke set (`fixtures/smoke`: 20 LibriSpeech test-clean clips and their
/// transcripts).
fn smoke(args: &Args) -> Vec<Utterance> {
    let dir = args.root.join("fixtures/smoke");
    let text = std::fs::read_to_string(dir.join("transcripts.txt"))
        .unwrap_or_else(|e| fail(format!("{}: {e}", dir.display())));
    let out: Vec<Utterance> = text
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| {
            let (id, reference) = l
                .split_once(' ')
                .unwrap_or_else(|| fail(format!("bad smoke transcript line {l:?}")));
            Utterance {
                reference: reference.to_owned(),
                audio: read_flac(&dir.join(format!("{id}.flac"))),
            }
        })
        .collect();
    if out.is_empty() {
        fail("fixtures/smoke/transcripts.txt is empty");
    }
    out
}

fn eval_librispeech(
    set_name: &str,
    set: &[Utterance],
    m: &mut dyn Transcriber,
    vad: &mut SileroVad,
    opts: &TranscribeOptions,
    want: BackendKind,
) -> (Value, Speed) {
    let (mut counts, mut speed, mut dropped) = (WerCounts::default(), Speed::default(), 0);
    for u in set {
        let (words, dt, d) = run_pieces(m, vad, &u.audio, opts, want);
        counts += word_errors_normalized(&u.reference, &text_of(&words));
        speed.add(u.audio.len(), dt);
        dropped += d;
    }
    let v = json!({
        "set": set_name,
        "utterances": set.len(),
        "wer": wer_json(counts),
        "pieces_dropped": dropped,
        "speed": speed.json(),
    });
    (v, speed)
}

// ---- AMI -------------------------------------------------------------------------------

/// Start-time errors (ms) of hypothesis words matched in order to reference words.
fn timing_errors(hyp: &[(String, u64)], reference: &[(String, u64)]) -> Vec<u64> {
    let h: Vec<String> = hyp.iter().map(|(t, _)| t.clone()).collect();
    let r: Vec<String> = reference.iter().map(|(t, _)| t.clone()).collect();
    lcs_pairs(h.len(), r.len(), same_token(&h, &r))
        .into_iter()
        .map(|(i, j)| hyp[i].1.abs_diff(reference[j].1))
        .collect()
}

fn percentile(sorted: &[u64], p: f64) -> Option<u64> {
    if sorted.is_empty() {
        return None;
    }
    let k = ((sorted.len() - 1) as f64 * p).round() as usize;
    Some(sorted[k])
}

fn ms(seconds: f64) -> u64 {
    (seconds * 1000.0).round().max(0.0) as u64
}

/// `ami`: the AMI IHM rule of `accept m2` (`refs::AmiHeadset`, shared with `asr_eval`):
/// every oracle segment decoded by one call on its speaker's headset, scored against the
/// speaker's words inside it, words outside every segment as deletions. `ami_sdm` (T2.5c)
/// is the same rule on `refs::load_ami_sdm`'s set: every segment decoded from the meeting's
/// single distant microphone (`Array1-01`) instead of the speaker's headset.
fn eval_ami(
    (name, set_doc): (&str, &str),
    set: &[refs::AmiHeadset],
    m: &mut dyn Transcriber,
    opts: &TranscribeOptions,
    want: BackendKind,
) -> (Value, Speed) {
    let (mut counts, mut outside, mut speed) =
        (WerCounts::default(), WerCounts::default(), Speed::default());
    let mut errors = Vec::new();
    let mut headsets = Vec::new();
    let mut meetings: Vec<(String, WerCounts)> = Vec::new();
    for h in set {
        eprintln!(
            "model_eval:   {name} {} {} (speaker {} on Headset-{}, {} segments)",
            h.meeting,
            h.channel,
            h.speaker,
            h.headset,
            h.ranges.len()
        );
        let mut hyps = Vec::with_capacity(h.ranges.len());
        // Hypothesis words (normalised token, start ms from the start of the recording).
        let mut timed_hyp = Vec::new();
        let mut decode = Duration::ZERO;
        for (range, audio) in h.ranges.iter().zip(h.segment_audio()) {
            let buffer = MonoBuffer::new(PIPELINE_RATE, audio.to_vec());
            let started = Instant::now();
            let t = m
                .transcribe(&buffer, opts, &CancelToken::new())
                .unwrap_or_else(|e| fail(format!("transcribe: {e}")));
            let dt = started.elapsed();
            check_backend(&t, m, want);
            decode += dt;
            speed.add(audio.len(), dt);
            let words = words_of(&t);
            let offset = ms(secs(range.start));
            timed_hyp.extend(
                words
                    .iter()
                    .map(|w| (normalize_token(&w.text), offset + w.start.as_millis())),
            );
            hyps.push(text_of(&words));
        }
        let score = h.score(&hyps).unwrap_or_else(|e| fail(e));
        let total = score.total();
        counts += total;
        outside += score.outside;
        match meetings.iter_mut().find(|(name, _)| *name == h.meeting) {
            Some((_, c)) => *c += total,
            None => meetings.push((h.meeting.clone(), total)),
        }
        let reference: Vec<(String, u64)> = h
            .segment_words()
            .iter()
            .map(|w| (normalize_token(&w.text), ms(w.start)))
            .collect();
        let mut e = timing_errors(&timed_hyp, &reference);
        headsets.push(json!({
            "meeting": h.meeting,
            "headset": h.headset,
            "channel": h.channel,
            "speaker": h.speaker,
            "segments": h.ranges.len(),
            "wer": wer_json(total),
            "outside_words": score.outside.ref_words,
            "matched_words": e.len(),
            "decode_s": decode.as_secs_f64(),
        }));
        errors.append(&mut e);
    }
    errors.sort_unstable();
    let within = errors.iter().filter(|&&e| e <= TIMING_OK_MS).count();
    let v = json!({
        "set": set_doc,
        "wer": wer_json(counts),
        "outside_segments": wer_json(outside),
        "timing": {
            "matched_words": errors.len(),
            "start_error_median_ms": percentile(&errors, 0.5),
            "start_error_p90_ms": percentile(&errors, 0.9),
            "within_250ms_percent": 100.0 * within as f64 / errors.len().max(1) as f64,
        },
        "meetings": meetings
            .iter()
            .map(|(name, c)| json!({ "meeting": name, "wer": wer_json(*c) }))
            .collect::<Vec<_>>(),
        "headsets": headsets,
        "speed": speed.json(),
    });
    (v, speed)
}

/// `speed`: the first `SPEED_S` of AMI IS1009a's Mix-Headset stream inside its UEM.
struct SpeedClip {
    name: &'static str,
    audio: Vec<f32>,
}

fn speed_clip(args: &Args) -> SpeedClip {
    let name = refs::AMI_MEETINGS[0];
    let uem_path = args.fixtures.join(format!("ami/uem/{name}.uem"));
    let uem = std::fs::read(&uem_path)
        .unwrap_or_else(|e| fail(format!("{}: {e}; {}", uem_path.display(), refs::FETCH_AMI)));
    let (from, to) = refs::parse_uem(&refs::latin1(&uem), name).unwrap_or_else(|e| fail(e));
    let wav_path = args
        .fixtures
        .join(format!("ami/audio/{name}.Mix-Headset.wav"));
    let file = File::open(&wav_path)
        .unwrap_or_else(|e| fail(format!("{}: {e}; {}", wav_path.display(), refs::FETCH_AMI)));
    let wav = read_wav(BufReader::new(file))
        .unwrap_or_else(|e| fail(format!("{}: {e}", wav_path.display())));
    if wav.rate() != PIPELINE_RATE {
        fail(format!("{}: expected 16 kHz", wav_path.display()));
    }
    let mono = wav.to_mono();
    let rate = f64::from(PIPELINE_RATE.hz());
    let a = ((from * rate).round() as usize).min(mono.len());
    let b = ((to * rate).round() as usize)
        .min(mono.len())
        .min(a + (SPEED_S * rate) as usize);
    SpeedClip {
        name,
        audio: mono[a..b].to_vec(),
    }
}

fn eval_speed(
    clip: &SpeedClip,
    m: &mut dyn Transcriber,
    vad: &mut SileroVad,
    opts: &TranscribeOptions,
    want: BackendKind,
) -> (Value, Speed) {
    let n = clip.audio.len();
    let mut speed = Speed::default();
    let (words, dt, _) = run_pieces(m, vad, &clip.audio, opts, want);
    speed.add(n, dt);
    let v = json!({
        "set": format!("AMI {} Mix-Headset, first {SPEED_S} s of the UEM span; speed only", clip.name),
        "words": words.len(),
        "speed": speed.json(),
    });
    (v, speed)
}

// ---- GATE-12 ---------------------------------------------------------------------------

struct Gate12 {
    /// Stretches where only the far end played; only for the settings' own recording.
    residue_ms: Option<Vec<[u64; 2]>>,
    /// The cleaned mic, quantised as the FLAC stores it.
    mic: Vec<f32>,
    /// The reference, quantised as the FLAC stores it.
    far: Vec<f32>,
    /// The canceller's lock stretches, as the live session records them.
    locks: Vec<EchoLock>,
    /// Informational (T6.23 follow-up, evaluation only): the bridged gaps in which the
    /// stored recordings show the echo path (`echo::gap_has_echo_path`). It depends only
    /// on the tracks and the locks, so it is computed once when the corpus loads.
    bridged_gaps_with_echo_path: usize,
    script: String,
    /// The fixed far-end reference words (FAR_REFERENCE_MODEL, beam).
    far_reference: Vec<Word>,
    /// The production speaker models (the speaker check embeds mic spans with them).
    diarizer: Diarizer<PyannoteSegmenter, CamppEmbedder>,
    /// The embedding model's voiceprint threshold (the echo-cluster drop).
    voiceprint_threshold: f32,
    mic_diar: Diarization,
    far_diar: Diarization,
    /// The recording, relative to the root.
    dir: String,
}

fn read_track(path: &Path) -> TrackBuffer {
    let file = File::open(path).unwrap_or_else(|e| fail(format!("{}: {e}", path.display())));
    let wav =
        read_wav(BufReader::new(file)).unwrap_or_else(|e| fail(format!("{}: {e}", path.display())));
    TrackBuffer::try_from(wav).unwrap_or_else(|e| fail(format!("{}: {e:?}", path.display())))
}

/// The cleaned mic, the mono reference and the canceller's lock stretches, as the live
/// pipeline produces them (stretches by `LiveEchoFilter::stats`, as a live session).
fn replay(dir: &Path) -> (Vec<f32>, Vec<f32>, Vec<EchoLock>) {
    let bytes = std::fs::read(dir.join("timestamps.json"))
        .unwrap_or_else(|e| fail(format!("{}: {e}", dir.display())));
    let timeline: RawTimeline =
        serde_json::from_slice(&bytes).unwrap_or_else(|e| fail(format!("timestamps.json: {e}")));
    let mut source = ReplayBuilder::from_raw_timeline(
        &timeline,
        read_track(&dir.join("mic.wav")),
        read_track(&dir.join("loopback.wav")),
    )
    .and_then(ReplayBuilder::build)
    .unwrap_or_else(|e| fail(format!("replay: {e:?}")));
    let mut pipeline = Pipeline::new(source.session().clone(), LiveConfig::default().pipeline)
        .unwrap_or_else(|e| fail(format!("pipeline: {e:?}")));
    let (mut mic, mut far) = (Vec::new(), Vec::new());
    let mut locks = LiveEchoFilter::new();
    loop {
        let status = source
            .pump(&mut pipeline)
            .unwrap_or_else(|e| fail(format!("replay pump: {e:?}")));
        for event in pipeline.poll() {
            if let PipelineEvent::Stats(stats) = &event {
                locks.stats(stats);
            }
            if let PipelineEvent::Audio(a) = event {
                let track = if a.track == TrackId::Mic {
                    &mut mic
                } else {
                    &mut far
                };
                let start = usize::try_from(a.start).unwrap_or(usize::MAX);
                if track.len() < start + a.samples.len() {
                    track.resize(start + a.samples.len(), 0.0);
                }
                track[start..start + a.samples.len()].copy_from_slice(&a.samples);
            }
        }
        if status == SourceStatus::Ended && pipeline.is_ended() {
            break;
        }
    }
    (mic, far, locks.locks().to_vec())
}

/// The near-end script lines of gates/GATE-12.md.
fn gate12_script(root: &Path) -> String {
    let path = root.join("gates/GATE-12.md");
    let text =
        std::fs::read_to_string(&path).unwrap_or_else(|e| fail(format!("{}: {e}", path.display())));
    let section = text
        .split("## Near-end script")
        .nth(1)
        .and_then(|s| s.split("\n## ").next())
        .unwrap_or_else(|| fail("GATE-12.md has no near-end script"));
    let lines: Vec<&str> = section
        .lines()
        .filter_map(|l| l.trim().strip_prefix("- "))
        .collect();
    if lines.len() != 10 {
        fail(format!(
            "GATE-12 script: {} lines, expected 10",
            lines.len()
        ));
    }
    lines.join(" ")
}

fn gate12(args: &Args, vad: &mut SileroVad) -> Option<Gate12> {
    // The recording's folder and residue stretches were read off it, so they live outside the
    // repo (T9.1kb).
    let path = gate12_private::default_path();
    let settings = gate12_private::load(path.as_deref())
        .unwrap_or_else(|e| panic!("model_eval: gate12 settings: {e}"));
    let Some(rel) = args
        .gate12_dir
        .clone()
        .or_else(|| settings.as_ref().map(|s| s.model_eval_dir.clone()))
    else {
        eprintln!(
            "model_eval: gate12 SKIPPED: no --gate12-dir, and its private settings (.earshot-private/gate12.toml in the home folder) are not here"
        );
        return None;
    };
    // The residue stretches are those of the settings' own recording.
    let residue_ms = settings
        .filter(|s| rel == s.model_eval_dir)
        .map(|s| s.residue_ms);
    let dir = args.root.join(&rel);
    if !dir.join("timestamps.json").is_file() {
        eprintln!(
            "model_eval: gate12 SKIPPED: {} not present (private recording)",
            dir.display()
        );
        return None;
    }
    eprintln!("model_eval: gate12: replaying the recording through the pipeline");
    let (mic, far, locks) = replay(&dir);
    // What the final pass reads back from the FLACs.
    let (mic, far) = (quantise_pcm16(&mic), quantise_pcm16(&far));
    eprintln!("model_eval: gate12: canceller lock stretches {locks:?}");
    let bridged_gaps_with_echo_path = {
        let mic_audio = MonoBuffer::new(PIPELINE_RATE, mic.clone());
        let far_audio = MonoBuffer::new(PIPELINE_RATE, far.clone());
        gaps_bridged(
            &locks,
            &cleaning_stretches_with(&locks, |prev, next| {
                gap_has_echo_path(&mic_audio, &far_audio, prev, next)
            }),
        )
        .len()
    };
    eprintln!("model_eval: gate12: diarising both tracks (production speaker models, CPU)");
    let (diarizer, voiceprint_threshold, mic_diar, far_diar) = gate12_speakers(args, &mic, &far);
    let reference = CANDIDATES
        .iter()
        .find(|c| c.name == FAR_REFERENCE_MODEL)
        .unwrap_or_else(|| fail("no far reference model"));
    eprintln!("model_eval: gate12: far-end reference by {FAR_REFERENCE_MODEL} (beam)");
    let mut m = load(args, reference);
    let (far_reference, _, _) = run_pieces(
        m.transcriber.as_mut(),
        vad,
        &far,
        &options("beam"),
        args.backend,
    );
    Some(Gate12 {
        mic,
        far,
        locks,
        bridged_gaps_with_echo_path,
        script: gate12_script(&args.root),
        far_reference,
        diarizer,
        voiceprint_threshold,
        mic_diar,
        far_diar,
        dir: rel,
        residue_ms,
    })
}

/// The final pass's speaker models and settings (`backends::load_diarisation` on the CPU,
/// `PostConfig` default thresholds, as `PostProcessor::new` builds its diarizer) and the
/// diarisation of both tracks.
fn gate12_speakers(
    args: &Args,
    mic: &[f32],
    far: &[f32],
) -> (
    Diarizer<PyannoteSegmenter, CamppEmbedder>,
    f32,
    Diarization,
    Diarization,
) {
    let (segmenter, embedder) =
        load_diarisation(&args.models, Accelerator::Cpu).unwrap_or_else(|e| fail(e));
    let model = embedder.info().model.clone();
    let post = PostConfig::default();
    let mut config = post.diarization.clone();
    config.clustering.threshold = post
        .clustering_threshold(&model)
        .unwrap_or_else(|e| fail(e));
    let voiceprint_threshold = post
        .voiceprint_threshold(&model)
        .unwrap_or_else(|e| fail(e));
    let mut diarizer = Diarizer::new(segmenter, embedder, config).unwrap_or_else(|e| fail(e));
    let mut diarize = |samples: &[f32]| {
        diarizer
            .diarize(
                &MonoBuffer::new(PIPELINE_RATE, samples.to_vec()),
                MeetingTime::ZERO,
                &CancelToken::new(),
            )
            .unwrap_or_else(|e| fail(format!("diarize: {e}")))
    };
    let (mic_diar, far_diar) = (diarize(mic), diarize(far));
    (diarizer, voiceprint_threshold, mic_diar, far_diar)
}

fn eval_gate12(
    g: &mut Gate12,
    m: &mut dyn Transcriber,
    vad: &mut SileroVad,
    opts: &TranscribeOptions,
    want: BackendKind,
) -> (Value, Speed) {
    let mut speed = Speed::default();
    let (raw_mic, dt, dropped) = run_pieces(m, vad, &g.mic, opts, want);
    speed.add(g.mic.len(), dt);
    // Production echo cleaning, the final pass's own function: the text rule against this
    // model's own transcript of the far end and the speaker check, per cleaning stretch,
    // then the echo-cluster drop.
    let (own_far, far_dt, _) = run_pieces(m, vad, &g.far, opts, want);
    speed.add(g.far.len(), far_dt);
    let mut segments = vec![Segment::from_words(
        EntityId::from_bytes([1; 16]),
        EntityId::from_bytes([2; 16]),
        raw_mic.clone(),
        MeetingTime::ZERO,
    )];
    let mut mic_diar = g.mic_diar.clone();
    let mic_audio = MonoBuffer::new(PIPELINE_RATE, g.mic.clone());
    let cleaning = clean_mic_echo(
        MicEcho {
            locks: &g.locks,
            mic: Some(&mut segments),
            reference: Some(&own_far),
            mic_diar: Some(&mut mic_diar),
            ref_diar: Some(&g.far_diar),
            mic_audio: Some(&mic_audio),
            voiceprint_threshold: g.voiceprint_threshold,
        },
        g.diarizer.embedder_mut(),
        &CancelToken::new(),
    )
    .unwrap_or_else(|e| fail(format!("echo cleaning: {e}")));
    if let Some(w) = &cleaning.warning {
        fail(format!("echo cleaning: {w:?}"));
    }
    let echo = cleaning.report;
    let mic: Vec<Word> = segments.into_iter().flat_map(|s| s.words).collect();
    let leaked_raw = far_leaks(&raw_mic, g);
    let script_counts = word_errors_normalized(&g.script, &text_of(&mic));
    let found = script_counts.ref_words - script_counts.substitutions - script_counts.deletions;
    let leaked = far_leaks(&mic, g);
    let residue = g.residue_ms.as_ref().map(|stretches| {
        mic.iter()
            .filter(|w| {
                let t = w.start.as_millis();
                stretches.iter().any(|&[a, b]| t >= a && t < b)
            })
            .count()
    });
    let far_words = g.far_reference.len();
    let v = json!({
        "set": format!("GATE-12 {}, replayed through the pipeline (live settings)", g.dir),
        "script_words": script_counts.ref_words,
        "script_found": found,
        "script_recall_percent": 100.0 * found as f64 / script_counts.ref_words.max(1) as f64,
        "far_reference": format!("{FAR_REFERENCE_MODEL} beam, {far_words} words"),
        "lock_stretches": g.locks.len(),
        "bridged_gaps": echo.bridged_gaps.len(),
        "bridged_gaps_with_echo_path": g.bridged_gaps_with_echo_path,
        "echo_cleaning_removed": raw_mic.len() - mic.len(),
        "echo_text_removed": echo.text_removed,
        "echo_speaker_removed": echo.speaker_removed,
        "echo_cluster_removed": echo.cluster_removed,
        "speaker_check_skipped": echo.speaker_check_skipped,
        "far_words_under_me_before_cleaning": leaked_raw.len(),
        "far_words_under_me_before_cleaning_percent": 100.0 * leaked_raw.len() as f64 / far_words.max(1) as f64,
        "far_words_under_me": leaked.len(),
        "far_words_under_me_percent": 100.0 * leaked.len() as f64 / far_words.max(1) as f64,
        "residue_words": residue,
        "mic_words": mic.len(),
        "far_audio_s": secs(g.far.len()),
        "pieces_dropped": dropped,
        "speed": speed.json(),
    });
    (v, speed)
}

/// Indices of `mic` words that are far-end words: not matched (in order) to the script,
/// but matched in order, within `LEAK_SLACK_MS`, to the fixed far-end reference.
fn far_leaks(mic: &[Word], g: &Gate12) -> Vec<usize> {
    // Mic words that are script words (in order) are the owner's.
    let script_words: Vec<String> = g.script.split_whitespace().map(normalize_token).collect();
    let mic_tokens = tokens(mic);
    let mut is_script = vec![false; mic.len()];
    for (i, _) in lcs_pairs(
        mic.len(),
        script_words.len(),
        same_token(&mic_tokens, &script_words),
    ) {
        is_script[i] = true;
    }
    let rest: Vec<usize> = (0..mic.len()).filter(|&i| !is_script[i]).collect();
    let far_tokens = tokens(&g.far_reference);
    lcs_pairs(rest.len(), far_tokens.len(), |a, b| {
        let w = &mic[rest[a]];
        !mic_tokens[rest[a]].is_empty()
            && mic_tokens[rest[a]] == far_tokens[b]
            && w.start
                .as_millis()
                .abs_diff(g.far_reference[b].start.as_millis())
                <= LEAK_SLACK_MS
    })
    .into_iter()
    .map(|(a, _)| rest[a])
    .collect()
}

// ---- Silence ---------------------------------------------------------------------------

fn silence_clips() -> Vec<(&'static str, Vec<f32>)> {
    let mut rng = ChaCha8Rng::seed_from_u64(SILENCE_SEED);
    let n = SILENCE_CLIP_S * PIPELINE_RATE.hz() as usize;
    SILENCE_CLIPS
        .iter()
        .map(|&(name, dbfs)| {
            // Uniform on [-a, a] has RMS a / sqrt(3).
            let a = 10f64.powf(dbfs / 20.0) * 3f64.sqrt();
            let clip = (0..n)
                .map(|_| {
                    let u = f64::from(rng.next_u32()) / f64::from(u32::MAX);
                    ((2.0 * u - 1.0) * a) as f32
                })
                .collect();
            (name, clip)
        })
        .collect()
}

fn eval_silence(
    clips: &[(&'static str, Vec<f32>)],
    m: &mut dyn Transcriber,
    opts: &TranscribeOptions,
    want: BackendKind,
) -> (Value, Speed) {
    let mut speed = Speed::default();
    let mut out = Vec::new();
    let mut total = 0;
    for (name, clip) in clips {
        let audio = MonoBuffer::new(PIPELINE_RATE, clip.clone());
        let started = Instant::now();
        let t = m
            .transcribe(&audio, opts, &CancelToken::new())
            .unwrap_or_else(|e| fail(format!("silence {name}: {e}")));
        speed.add(clip.len(), started.elapsed());
        check_backend(&t, m, want);
        let words = words_of(&t);
        let raw: Vec<Word> = t.segments.iter().flat_map(|s| s.timed_words()).collect();
        total += words.len();
        out.push(json!({
            "clip": name,
            "words": words.len(),
            "text": text_of(&words),
            "raw_text": text_of(&raw),
        }));
    }
    let v = json!({
        "set": "30 s synthetic clips decoded without VAD",
        "words": total,
        "clips": out,
        "speed": speed.json(),
    });
    (v, speed)
}

fn main() {
    let args = parse_args();
    let accel = match args.backend {
        BackendKind::Cuda => Accelerator::Cuda,
        BackendKind::Qnn => Accelerator::Qnn,
        _ => Accelerator::Cpu,
    };
    init_onnx(&args.models, accel).unwrap_or_else(|e| fail(format!("ONNX Runtime: {e}")));
    let mut vad = SileroVad::load(
        &args.models.require(SILERO).unwrap_or_else(|e| fail(e)),
        &SessionConfig::cpu(1),
    )
    .unwrap_or_else(|e| fail(format!("Silero: {e}")));
    let wants = |c: &str| args.corpora.iter().any(|x| x == c);
    let ls = wants("librispeech").then(|| librispeech(&args, "test-clean"));
    let ls_dev = wants("librispeech_dev").then(|| librispeech(&args, "dev-clean"));
    let smoke_set = wants("smoke").then(|| smoke(&args));
    let ihm = wants("ami").then(|| {
        refs::load_ami_ihm(&args.fixtures, &refs::AMI_MEETINGS).unwrap_or_else(|e| fail(e))
    });
    let sdm = wants("ami_sdm").then(|| {
        refs::load_ami_sdm(&args.fixtures, &refs::AMI_MEETINGS).unwrap_or_else(|e| fail(e))
    });
    let speed = wants("speed").then(|| speed_clip(&args));
    let mut g12 = if wants("gate12") {
        gate12(&args, &mut vad)
    } else {
        None
    };
    let silence = wants("silence").then(silence_clips);
    let threads = thread_budget(args.backend);
    let mut rows = Vec::new();
    for c in CANDIDATES
        .iter()
        .filter(|c| args.names.iter().any(|n| n == c.name))
    {
        let whisper = matches!(c.engine, Engine::Whisper);
        let decodings: Vec<&str> = if whisper {
            args.decodings.iter().map(String::as_str).collect()
        } else {
            vec!["greedy"]
        };
        let mut m = load(&args, c);
        eprintln!(
            "model_eval: {} loaded in {:.1} s on {:?} ({threads} threads)",
            c.name, m.load_s, args.backend
        );
        for decoding in decodings {
            let opts = options(decoding);
            let mut total = Speed::default();
            let mut corpora = serde_json::Map::new();
            let mut add = |name: &str, (v, s): (Value, Speed)| {
                total.audio_s += s.audio_s;
                total.decode_s += s.decode_s;
                corpora.insert(name.to_owned(), v);
            };
            let t = m.transcriber.as_mut();
            eprintln!("model_eval: {} {decoding}", c.name);
            if let Some(set) = &ls {
                add(
                    "librispeech",
                    eval_librispeech(
                        "LibriSpeech test-clean, pinned subset",
                        set,
                        t,
                        &mut vad,
                        &opts,
                        args.backend,
                    ),
                );
            }
            if let Some(set) = &ls_dev {
                add(
                    "librispeech_dev",
                    eval_librispeech(
                        "LibriSpeech dev-clean, pinned subset (dev set: never calibration data)",
                        set,
                        t,
                        &mut vad,
                        &opts,
                        args.backend,
                    ),
                );
            }
            if let Some(set) = &smoke_set {
                add(
                    "smoke",
                    eval_librispeech(
                        "fixtures/smoke (20 committed clips)",
                        set,
                        t,
                        &mut vad,
                        &opts,
                        args.backend,
                    ),
                );
            }
            if let Some(set) = &ihm {
                add(
                    "ami",
                    eval_ami(("ami", AMI_IHM_SET), set, t, &opts, args.backend),
                );
            }
            if let Some(set) = &sdm {
                add(
                    "ami_sdm",
                    eval_ami(("ami_sdm", AMI_SDM_SET), set, t, &opts, args.backend),
                );
            }
            if let Some(g) = &mut g12 {
                add("gate12", eval_gate12(g, t, &mut vad, &opts, args.backend));
            }
            if let Some(clips) = &silence {
                add("silence", eval_silence(clips, t, &opts, args.backend));
            }
            if let Some(clip) = &speed {
                add("speed", eval_speed(clip, t, &mut vad, &opts, args.backend));
            }
            let row = json!({
                "model": c.name,
                "decoding": decoding,
                "languages": c.languages,
                "backend": backend_name(args.backend),
                "threads": threads,
                "load_s": m.load_s,
                "evidence": m.evidence,
                "backend_info": t.backend_info(),
                "corpora": corpora,
                "speed": total.json(),
            });
            summary(&row);
            rows.push(row);
            // Saved after every row, so a long run that is stopped keeps what it measured.
            write_report(&args, threads, g12.is_some(), &rows, false);
        }
    }
    write_report(&args, threads, g12.is_some(), &rows, true);
}

fn write_report(args: &Args, threads: usize, gate12: bool, rows: &[Value], complete: bool) {
    let report = json!({
        "task": "T6.10e",
        "backend": backend_name(args.backend),
        "onnx_models": if args.backend == BackendKind::Vulkan {
            "skipped: ONNX Runtime has no Vulkan provider (whisper models only)"
        } else {
            "evaluated when selected"
        },
        "threads": threads,
        "cores": std::thread::available_parallelism().map_or(1, std::num::NonZeroUsize::get),
        "pieces": format!("{:?}", PieceConfig::default()),
        "corpora": args.corpora,
        "gate12": if !args.corpora.iter().any(|c| c == "gate12") {
            "not requested"
        } else if gate12 {
            "measured"
        } else {
            "skipped (private recording not present)"
        },
        "complete": complete,
        "rows": rows,
    });
    let text = serde_json::to_string_pretty(&report).unwrap_or_else(|e| fail(e));
    std::fs::write(&args.out, text + "\n")
        .unwrap_or_else(|e| fail(format!("{}: {e}", args.out.display())));
    if complete {
        eprintln!("model_eval: report written to {}", args.out.display());
    }
}

/// One line per row on stderr.
fn summary(row: &Value) {
    let c = &row["corpora"];
    let f = |v: &Value| v.as_f64().map_or("-".to_owned(), |x| format!("{x:.2}"));
    eprintln!(
        "model_eval: RESULT {} {}: ls {}% ls-dev {}% smoke {}% ami {}% ami_sdm {}% timing p50 {} ms script {}% far-under-me {}% (before cleaning {}%) residue {} silence {} rtf {}",
        row["model"].as_str().unwrap_or_default(),
        row["decoding"].as_str().unwrap_or_default(),
        f(&c["librispeech"]["wer"]["wer_percent"]),
        f(&c["librispeech_dev"]["wer"]["wer_percent"]),
        f(&c["smoke"]["wer"]["wer_percent"]),
        f(&c["ami"]["wer"]["wer_percent"]),
        f(&c["ami_sdm"]["wer"]["wer_percent"]),
        c["ami"]["timing"]["start_error_median_ms"],
        f(&c["gate12"]["script_recall_percent"]),
        f(&c["gate12"]["far_words_under_me_percent"]),
        f(&c["gate12"]["far_words_under_me_before_cleaning_percent"]),
        c["gate12"]["residue_words"],
        c["silence"]["words"],
        f(&row["speed"]["rtf"]),
    );
}
