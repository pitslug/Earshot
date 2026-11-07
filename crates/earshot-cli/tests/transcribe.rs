//! `earshot-cli transcribe` end to end: the built binary on a committed smoke clip.
//!
//! These tests need the `ci` model set (whisper base.en, Silero VAD, ONNX Runtime CPU). A
//! missing model fails the test with the CLI's own `cargo xtask fetch-models` instruction in
//! the output; it is never a skip (AGENTS.md 11).

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use earshot_core::audio::{SampleRate, WavFormat, resample_all, write_wav};
use earshot_core::metrics::wer::word_errors_normalized;
use serde_json::Value;

/// A short smoke clip (about 6 s, one sentence).
const CLIP: &str = "61-70968-0000";

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn clip_path() -> PathBuf {
    repo_root()
        .join("fixtures/smoke")
        .join(format!("{CLIP}.flac"))
}

fn reference() -> String {
    let text = std::fs::read_to_string(repo_root().join("fixtures/smoke/transcripts.txt"))
        .expect("read transcripts.txt");
    text.lines()
        .find_map(|l| l.strip_prefix(CLIP).map(|r| r.trim().to_owned()))
        .unwrap_or_else(|| panic!("{CLIP} not in transcripts.txt"))
}

fn earshot(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_earshot-cli"))
        .args(args)
        .output()
        .expect("run earshot")
}

fn describe(out: &Output) -> String {
    format!(
        "status {:?}\n--- stdout ---\n{}\n--- stderr ---\n{}",
        out.status,
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

/// Runs `earshot-cli transcribe` and parses its JSON output; panics with the CLI's output
/// (which names the fetch command when a model is missing) on failure.
fn transcribe_json(input: &Path, extra: &[&str]) -> Value {
    let input = input.to_str().expect("utf-8 path");
    let mut args = vec!["transcribe", input, "--model", "base.en", "--json"];
    args.extend_from_slice(extra);
    let out = earshot(&args);
    assert!(out.status.success(), "{}", describe(&out));
    serde_json::from_slice(&out.stdout)
        .unwrap_or_else(|e| panic!("stdout is not JSON ({e})\n{}", describe(&out)))
}

/// Segments are non-empty, timed, in order, carry words, and transcribe this clip.
fn assert_segments(json: &Value) -> String {
    let segments = json["segments"].as_array().expect("segments array");
    assert!(!segments.is_empty(), "no segments: {json:#}");
    let mut prev_start = 0;
    let mut text = Vec::new();
    for s in segments {
        let start = s["start_ms"].as_u64().expect("start_ms");
        let end = s["end_ms"].as_u64().expect("end_ms");
        assert!(start <= end, "{s}");
        assert!(start >= prev_start, "segments out of order: {json:#}");
        prev_start = start;
        let t = s["text"].as_str().expect("text");
        assert!(!t.trim().is_empty(), "{s}");
        text.push(t.to_owned());
        let words = s["words"].as_array().expect("words array");
        assert!(!words.is_empty(), "segment without words: {s}");
        for w in words {
            assert!(
                w["text"].as_str().is_some_and(|t| !t.trim().is_empty()),
                "{w}"
            );
            let (ws, we) = (w["start_ms"].as_u64(), w["end_ms"].as_u64());
            assert!(ws.zip(we).is_some_and(|(a, b)| a <= b), "{w}");
            let p = w["probability"].as_f64().expect("probability");
            assert!((0.0..=1.0).contains(&p), "{w}");
        }
        let c = s["confidence"].as_f64().expect("confidence");
        assert!((0.0..=1.0).contains(&c), "{s}");
    }
    let hypothesis = text.join(" ");
    // A sanity bound that the output is this clip's speech, not an acceptance metric
    // (asr.* WER is scored by `cargo xtask accept m2`).
    let counts = word_errors_normalized(&reference(), &hypothesis);
    let wer = counts.wer().expect("reference words");
    assert!(
        wer <= 0.5,
        "transcript does not match {CLIP}: WER {wer:.2}\n  ref: {}\n  hyp: {hypothesis}",
        reference()
    );
    hypothesis
}

fn assert_cpu_backend(json: &Value) {
    let b = &json["backend"];
    assert_eq!(b["kind"], "cpu", "{json:#}");
    assert_eq!(b["device"], "cpu", "{json:#}");
    assert_eq!(b["model"], "ggml-base.en", "{json:#}");
    assert_eq!(json["requested_backend"], "cpu", "{json:#}");
    assert_eq!(json["backend_evidence"]["whisper_log"], "cpu", "{json:#}");
    let lines = json["backend_evidence"]["lines"].as_array().expect("lines");
    assert!(
        lines.iter().any(|l| l
            .as_str()
            .is_some_and(|l| l.contains("whisper_backend_init_gpu"))),
        "{json:#}"
    );
}

#[test]
fn batch_json_has_segments_words_backend_and_batch_decoding() {
    let json = transcribe_json(&clip_path(), &["--backend", "cpu"]);
    assert_segments(&json);
    assert_cpu_backend(&json);
    assert_eq!(json["mode"], "batch");
    assert_eq!(json["language"], "en", "detected");
    assert_eq!(json["requested_language"], "en", "{json:#}");
    // PLAN.md 4.0: batch is beam search with 5 beams at temperature 0.
    let decoding = &json["options"]["decoding"];
    assert_eq!(decoding["strategy"]["strategy"], "beam_search", "{json:#}");
    assert_eq!(decoding["strategy"]["beam_size"], 5, "{json:#}");
    assert_eq!(decoding["temperature"], 0.0, "{json:#}");
    assert_eq!(json["options"]["language"]["code"], "en", "{json:#}");
    assert!(json["vad"].is_null(), "batch mode has no VAD: {json:#}");
    let audio = &json["audio"];
    assert_eq!(audio["source_rate"], 16_000);
    assert_eq!(audio["channels"], 1);
    assert!(audio["duration_ms"].as_u64().is_some_and(|d| d > 1000));
    assert!(
        json["model_path"]
            .as_str()
            .is_some_and(|p| p.ends_with("ggml-base.en.bin")),
        "{json:#}"
    );
}

#[test]
fn stream_json_with_silero_has_segments_and_live_decoding() {
    let json = transcribe_json(&clip_path(), &["--backend", "cpu", "--stream"]);
    assert_segments(&json);
    assert_cpu_backend(&json);
    assert_eq!(json["mode"], "stream");
    assert_eq!(json["requested_language"], "en", "{json:#}");
    assert!(
        json["language"].is_null(),
        "the chunker detects no language: {json:#}"
    );
    assert_eq!(json["vad"]["kind"], "silero", "silero is the default VAD");
    assert_eq!(json["vad"]["backend"], "cpu", "{json:#}");
    assert_eq!(json["stream"]["push_ms"], 10, "{json:#}");
    // PLAN.md 4.0: live/streaming is greedy at temperature 0.
    let decoding = &json["options"]["decoding"];
    assert_eq!(decoding["strategy"]["strategy"], "greedy", "{json:#}");
    assert_eq!(decoding["temperature"], 0.0, "{json:#}");
}

#[test]
fn stream_json_with_energy_vad_has_segments() {
    let json = transcribe_json(
        &clip_path(),
        &["--backend", "cpu", "--stream", "--vad", "energy"],
    );
    assert_segments(&json);
    assert_eq!(json["mode"], "stream");
    assert_eq!(json["vad"]["kind"], "energy", "{json:#}");
}

#[test]
fn text_output_prints_timed_segments() {
    let input = clip_path();
    let out = earshot(&[
        "transcribe",
        input.to_str().unwrap(),
        "--model",
        "base.en",
        "--backend",
        "cpu",
    ]);
    assert!(out.status.success(), "{}", describe(&out));
    let stdout = String::from_utf8_lossy(&out.stdout);
    let lines: Vec<&str> = stdout.lines().filter(|l| !l.trim().is_empty()).collect();
    assert!(!lines.is_empty(), "{}", describe(&out));
    for l in &lines {
        assert!(
            l.starts_with('[') && l.contains(" -> ") && l.contains("] "),
            "{l}"
        );
    }
}

/// Unique scratch directory under the system temp dir, removed on drop (std only, see
/// `record_raw.rs`).
struct ScratchDir(PathBuf);

impl ScratchDir {
    fn new(tag: &str) -> Self {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let p =
            std::env::temp_dir().join(format!("earshot-cli-{tag}-{}-{nanos}", std::process::id()));
        std::fs::create_dir_all(&p).unwrap();
        Self(p)
    }
}

impl Drop for ScratchDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn smoke_samples() -> Vec<f32> {
    let mut reader = claxon::FlacReader::open(clip_path()).expect("open smoke clip");
    let scale = (1_i64 << (reader.streaminfo().bits_per_sample - 1)) as f32;
    reader
        .samples()
        .map(|s| s.expect("flac sample") as f32 / scale)
        .collect()
}

#[test]
fn stereo_48k_wav_is_mixed_down_and_resampled() {
    let dir = ScratchDir::new("wav");
    let mono = resample_all(&smoke_samples(), SampleRate::HZ_16000, SampleRate::HZ_48000)
        .expect("resample");
    let stereo: Vec<f32> = mono.iter().flat_map(|&s| [s, s]).collect();
    let path = dir.0.join("clip.wav");
    let file = std::fs::File::create(&path).unwrap();
    write_wav(
        std::io::BufWriter::new(file),
        SampleRate::HZ_48000,
        2,
        &stereo,
        WavFormat::Pcm16,
    )
    .expect("write wav");

    let json = transcribe_json(&path, &["--backend", "cpu"]);
    assert_segments(&json);
    assert_eq!(json["audio"]["source_rate"], 48_000);
    assert_eq!(json["audio"]["channels"], 2);
}

#[test]
fn missing_model_fails_with_the_fetch_instruction() {
    let dir = ScratchDir::new("models");
    let out = Command::new(env!("CARGO_BIN_EXE_earshot-cli"))
        .args([
            "transcribe",
            clip_path().to_str().unwrap(),
            "--json",
            "--backend",
            "cpu",
        ])
        .env("EARSHOT_MODELS", &dir.0)
        .output()
        .expect("run earshot");
    assert!(!out.status.success(), "{}", describe(&out));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("cargo xtask fetch-models --set ci"),
        "{}",
        describe(&out)
    );
    assert!(stderr.contains("ggml-base.en.bin"), "{}", describe(&out));
    assert!(
        out.stdout.is_empty(),
        "no JSON on failure: {}",
        describe(&out)
    );
}

#[test]
fn missing_silero_model_fails_with_the_fetch_instruction() {
    // The whisper model exists (an explicit path), the models dir has no Silero.
    let base = match std::env::var_os("EARSHOT_MODELS").filter(|v| !v.is_empty()) {
        Some(d) => PathBuf::from(d),
        None => PathBuf::from(std::env::var_os("LOCALAPPDATA").expect("LOCALAPPDATA"))
            .join("Earshot")
            .join("models"),
    }
    .join("whisper/ggml-base.en.bin");
    let dir = ScratchDir::new("models");
    let out = Command::new(env!("CARGO_BIN_EXE_earshot-cli"))
        .args([
            "transcribe",
            clip_path().to_str().unwrap(),
            "--model",
            base.to_str().unwrap(),
            "--stream",
            "--backend",
            "cpu",
            "--verbose",
        ])
        .env("EARSHOT_MODELS", &dir.0)
        .output()
        .expect("run earshot");
    assert!(!out.status.success(), "{}", describe(&out));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("cargo xtask fetch-models --set ci"),
        "{}",
        describe(&out)
    );
    // The cheap precondition fails before the whisper model is loaded (--verbose would
    // echo whisper.cpp's load log).
    assert!(
        !stderr.contains("whisper_model_load") && !stderr.contains("whisper_init"),
        "{}",
        describe(&out)
    );
}

#[cfg(not(feature = "cuda"))]
#[test]
fn cuda_backend_without_the_cuda_feature_is_a_clear_error() {
    let out = earshot(&[
        "transcribe",
        clip_path().to_str().unwrap(),
        "--backend",
        "cuda",
    ]);
    assert!(!out.status.success(), "{}", describe(&out));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("--features cuda"), "{}", describe(&out));
}

#[cfg(feature = "cuda")]
#[test]
fn cuda_backend_reports_cuda_from_the_whisper_log() {
    let input = clip_path();
    let out = earshot(&[
        "transcribe",
        input.to_str().unwrap(),
        "--model",
        "base.en",
        "--backend",
        "cuda",
        "--json",
    ]);
    assert!(out.status.success(), "{}", describe(&out));
    let json: Value = serde_json::from_slice(&out.stdout).expect("json");
    assert_segments(&json);
    assert_eq!(json["backend"]["kind"], "cuda", "{json:#}");
    assert_eq!(json["backend_evidence"]["whisper_log"], "cuda", "{json:#}");
}
