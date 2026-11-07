//! The measurement behind `PieceConfig::recover_speech` (T2.5d): VAD speech left between
//! the words of a transcriber that may skip speech, with step 5 off.
//!
//! ```text
//! cargo run --release -p earshot-runtime --example skip_gaps -- <repo-root> \
//!     --models <dir> [--model parakeet-tdt-0.6b-v2] [--split dev-clean|test-clean] \
//!     [--out <file.json>]
//! ```
//!
//! For each split (both by default) the pinned LibriSpeech subset's utterances, in subset
//! order, are concatenated into ~60 s streams (`refs::group_streams`, as `asr.stream_determinism`
//! groups test-clean), quantised to 16-bit PCM as the recording stores them, and decoded on
//! the CPU through `transcribe_in_pieces` with the stretch track's options
//! (`TranscribeOptions::live()`) and `PieceConfig::default()` with `recover_speech: None`.
//! Silero's probabilities over the stream then give, for every stretch no word covers,
//! its VAD speech by step 5's own rule (`earshot_core::asr::speech_between_words`).
//!
//! Reported per stream: utterance range, length, reference words and word errors (Whisper
//! normalisation), the largest speech left between words, and every stretch with more than
//! `REPORT_MIN_S` of speech. Counts and times only, no transcript text. The 2 s setting was
//! fixed on dev-clean (module docs of `earshot_core::asr::pieces`); test-clean is reported
//! beside it, never used to choose.

#[allow(dead_code)] // shared with asr_eval; only `group_streams` is used here
#[path = "../../earshot-asr-whisper/examples/asr_eval/refs.rs"]
mod refs;

use std::path::{Path, PathBuf};

use earshot_core::asr::{
    CancelToken, PieceConfig, Span, TranscribeOptions, Vad, frame_probabilities,
    speech_between_words, transcribe_in_pieces,
};
use earshot_core::audio::{MonoBuffer, PIPELINE_RATE, quantise_pcm16};
use earshot_core::metrics::wer::word_errors_normalized;
use earshot_runtime::backends::{
    Accelerator, EvidenceLoader, ModelDir, final_pass_asr, init_onnx, load_silero, onnx_library,
};
use serde_json::{Value, json};

/// The production default English model on the desktop.
const DEFAULT_MODEL: &str = "parakeet-tdt-0.6b-v2";
/// Target stream length, as `asr.stream_determinism`.
const STREAM_TARGET_S: f64 = 60.0;
/// Stretches with more speech than this are listed.
const REPORT_MIN_S: f64 = 0.25;
const FETCH_LIBRISPEECH: &str = "run `cargo xtask fetch-fixtures --set librispeech`";

fn fail(msg: impl std::fmt::Display) -> ! {
    eprintln!("skip_gaps: {msg}");
    std::process::exit(1);
}

struct Args {
    root: PathBuf,
    models: ModelDir,
    model: String,
    splits: Vec<String>,
    out: Option<PathBuf>,
}

fn parse_args() -> Args {
    let mut it = std::env::args().skip(1);
    let root = PathBuf::from(
        it.next()
            .unwrap_or_else(|| fail("usage: skip_gaps <repo-root> --models <dir>")),
    );
    let (mut models, mut out, mut split) = (None, None, None);
    let mut model = DEFAULT_MODEL.to_owned();
    while let Some(flag) = it.next() {
        let mut value = || {
            it.next()
                .unwrap_or_else(|| fail(format!("{flag} needs a value")))
        };
        match flag.as_str() {
            "--models" => models = Some(ModelDir::new(value())),
            "--model" => model = value(),
            "--split" => split = Some(value()),
            "--out" => out = Some(PathBuf::from(value())),
            other => fail(format!("unknown argument {other}")),
        }
    }
    let splits = match split.as_deref() {
        None => vec!["dev-clean".to_owned(), "test-clean".to_owned()],
        Some(s @ ("dev-clean" | "test-clean")) => vec![s.to_owned()],
        Some(other) => fail(format!("unknown split {other}")),
    };
    Args {
        root,
        models: models.unwrap_or_else(|| fail("--models is required")),
        model,
        splits,
        out,
    }
}

/// `EARSHOT_FIXTURES`, else `%LOCALAPPDATA%\earshot-dev\fixtures`.
fn fixtures_dir() -> PathBuf {
    if let Some(d) = std::env::var_os("EARSHOT_FIXTURES").filter(|v| !v.is_empty()) {
        return PathBuf::from(d);
    }
    PathBuf::from(
        std::env::var_os("LOCALAPPDATA")
            .unwrap_or_else(|| fail("set EARSHOT_FIXTURES or LOCALAPPDATA")),
    )
    .join("earshot-dev")
    .join("fixtures")
}

struct Utterance {
    id: String,
    reference: String,
    audio: Vec<f32>,
}

/// The subset's utterances of `split`, in subset order.
fn utterances(root: &Path, split: &str) -> Vec<Utterance> {
    let path = root.join("fixtures/librispeech/subset.toml");
    let text =
        std::fs::read_to_string(&path).unwrap_or_else(|e| fail(format!("{}: {e}", path.display())));
    let doc: toml::Table = text
        .parse()
        .unwrap_or_else(|e| fail(format!("{}: {e}", path.display())));
    let entries = doc["utterance"]
        .as_array()
        .unwrap_or_else(|| fail("subset.toml has no utterances"));
    let base = fixtures_dir()
        .join("librispeech")
        .join(split)
        .join("LibriSpeech")
        .join(split);
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
        let dir = base.join(speaker).join(chapter);
        let flac = dir.join(format!("{id}.flac"));
        if !flac.is_file() {
            fail(format!(
                "{} is missing; {FETCH_LIBRISPEECH}",
                flac.display()
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
        let (rate, audio) = earshot_store::read_flac(&flac)
            .unwrap_or_else(|e| fail(format!("{}: {e}", flac.display())));
        if rate != PIPELINE_RATE.hz() {
            fail(format!("{} is {rate} Hz", flac.display()));
        }
        out.push(Utterance {
            id: id.to_owned(),
            reference,
            audio,
        });
    }
    if out.is_empty() {
        fail(format!("subset.toml lists no {split} utterances"));
    }
    out
}

fn secs(samples: usize) -> f64 {
    (samples as f64 / f64::from(PIPELINE_RATE.hz()) * 100.0).round() / 100.0
}

fn main() {
    let args = parse_args();
    let order = vec![Accelerator::Cpu];
    init_onnx(&args.models, onnx_library(&args.model, &order))
        .unwrap_or_else(|e| fail(format!("ONNX Runtime: {e}")));
    let choice = final_pass_asr(
        &EvidenceLoader::default(),
        &args.models,
        &args.model,
        &order,
    )
    .unwrap_or_else(|e| fail(format!("{}: {e}", args.model)));
    let mut model = choice.transcriber;
    if !earshot_core::asr::Transcriber::may_skip_speech(&model) {
        fail(format!("{} does not report skipping speech", args.model));
    }
    let mut vad = load_silero(&args.models, Accelerator::Cpu)
        .unwrap_or_else(|e| fail(format!("Silero: {e}")));
    let config = PieceConfig {
        recover_speech: None,
        ..PieceConfig::default()
    };
    let options = TranscribeOptions::live();
    let cancel = CancelToken::new();

    let mut splits = Vec::new();
    for split in &args.splits {
        let set = utterances(&args.root, split);
        let durations: Vec<f64> = set
            .iter()
            .map(|u| u.audio.len() as f64 / f64::from(PIPELINE_RATE.hz()))
            .collect();
        let streams = refs::group_streams(&durations, STREAM_TARGET_S);
        let mut per_stream = Vec::new();
        let mut over: Vec<Value> = Vec::new();
        let mut max_all = 0usize;
        for (i, r) in streams.iter().enumerate() {
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
            let buffer = MonoBuffer::new(PIPELINE_RATE, audio.clone());
            let (t, report) =
                transcribe_in_pieces(&mut model, &mut vad, &buffer, &options, &config, &cancel)
                    .unwrap_or_else(|e| fail(format!("{split} stream {}: {e}", i + 1)));
            if t.backend.kind != choice.backend {
                fail(format!("decoded on {:?}", t.backend.kind));
            }
            let errors = word_errors_normalized(
                &reference.join(" "),
                &earshot_core::asr::join_words(&t.words()),
            );
            let probs = frame_probabilities(&mut vad, &audio, &cancel)
                .unwrap_or_else(|e| fail(format!("vad: {e}")));
            let gaps = speech_between_words(
                &t.segments,
                Span {
                    start: 0,
                    end: audio.len(),
                },
                &probs,
                vad.frame_len(),
                config.threshold,
            );
            let max = gaps.iter().map(|&(_, s)| s).max().unwrap_or(0);
            max_all = max_all.max(max);
            let listed: Vec<Value> = gaps
                .iter()
                .filter(|&&(_, s)| secs(s) > REPORT_MIN_S)
                .map(|&(g, s)| {
                    json!({"start_s": secs(g.start), "end_s": secs(g.end), "speech_s": secs(s)})
                })
                .collect();
            for &(g, s) in &gaps {
                if secs(s) > 2.0 {
                    over.push(json!({
                        "stream": i + 1,
                        "start_s": secs(g.start),
                        "end_s": secs(g.end),
                        "speech_s": secs(s),
                    }));
                }
            }
            eprintln!(
                "skip_gaps: {split} stream {}/{}: {} errors of {} words, largest uncovered speech {:.2} s",
                i + 1,
                streams.len(),
                errors.errors(),
                errors.ref_words,
                secs(max)
            );
            per_stream.push(json!({
                "stream": i + 1,
                "utterances": [set[r.start].id, set[r.end - 1].id],
                "seconds": secs(audio.len()),
                "ref_words": errors.ref_words,
                "errors": errors.errors(),
                "pieces": report.pieces,
                "max_uncovered_speech_s": secs(max),
                "uncovered_over_report_min": listed,
            }));
        }
        // The largest stretch outside the ones over 2 s (the failures step 5 decodes again).
        let max_below: f64 = per_stream
            .iter()
            .flat_map(|s| {
                s["uncovered_over_report_min"]
                    .as_array()
                    .cloned()
                    .unwrap_or_default()
            })
            .filter_map(|g| g["speech_s"].as_f64())
            .filter(|&s| s <= 2.0)
            .fold(0.0, f64::max);
        splits.push(json!({
            "split": split,
            "streams": per_stream.len(),
            "max_uncovered_speech_s": secs(max_all),
            "max_uncovered_speech_at_most_2_s": max_below,
            "uncovered_over_2_s": over,
            "per_stream": per_stream,
        }));
    }
    let out = json!({
        "model": args.model,
        "backend": format!("{:?}", choice.backend),
        "options": "TranscribeOptions::live()",
        "pieces": format!("{config:?}"),
        "rule": "speech_between_words: frames at or above the piece threshold whose start lies in a stretch no word covers",
        "report_min_s": REPORT_MIN_S,
        "splits": splits,
    });
    let text = serde_json::to_string_pretty(&out).unwrap_or_else(|e| fail(e));
    if let Some(path) = &args.out {
        std::fs::write(path, &text).unwrap_or_else(|e| fail(format!("{}: {e}", path.display())));
    }
    println!("{text}");
}
