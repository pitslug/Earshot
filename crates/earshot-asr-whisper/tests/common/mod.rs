//! Shared helpers for the whisper.cpp integration tests.
//!
//! Tests that capture whisper.cpp's log each live in their own integration-test binary,
//! because the log hook is process-wide.

#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use earshot_asr_whisper::{WhisperConfig, WhisperTranscriber, evidence};
use earshot_core::asr::{CancelToken, TranscribeOptions, Transcriber, Transcription};
use earshot_core::audio::{MonoBuffer, PIPELINE_RATE};
use earshot_core::metrics::wer::{WerCounts, corpus_wer, word_errors_normalized};

/// Relative path of base.en (the default whisper model tests load, PLAN D22) inside the models directory (see `models/manifest.toml`).
pub const BASE_EN: &str = "whisper/ggml-base.en.bin";

/// Whisper expects 16 kHz mono.
pub const SAMPLE_RATE: u32 = 16_000;

/// Number of clips in the committed smoke set (PLAN.md 4.1). Scoring fewer is a failure,
/// never a smaller corpus.
pub const SMOKE_CLIPS: usize = 20;

/// Threshold id scored by these tests (PLAN.md 4.2).
pub const WER_SMOKE_BASE: &str = "asr.wer_smoke_base";

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// Models directory: `EARSHOT_MODELS`, else `%LOCALAPPDATA%\Earshot\models`.
///
/// Mirrors `xtask::models::cache_dir` (xtask is not a dependency of this crate); keep the two
/// rules identical.
pub fn models_dir() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("EARSHOT_MODELS").filter(|v| !v.is_empty()) {
        return Some(PathBuf::from(dir));
    }
    std::env::var_os("LOCALAPPDATA")
        .filter(|v| !v.is_empty())
        .map(|base| PathBuf::from(base).join("Earshot").join("models"))
}

/// Path to base.en. Panics with the fetch instruction when it is missing: a missing model is
/// a failed precondition, never a skip.
pub fn base_en() -> PathBuf {
    match models_dir().map(|dir| dir.join(BASE_EN)) {
        Some(p) if p.is_file() => p,
        other => panic!(
            "whisper base.en model not found at {}; run `cargo xtask fetch-models --set ci` \
             (or point EARSHOT_MODELS at a directory that contains {BASE_EN})",
            other.map_or_else(
                || "<no EARSHOT_MODELS or LOCALAPPDATA>".to_string(),
                |p| p.display().to_string()
            )
        ),
    }
}

/// Loads base.en on the GPU (when `use_gpu`) or the CPU.
pub fn load_base_en(use_gpu: bool) -> (WhisperTranscriber, Duration) {
    let mut config = WhisperConfig::new(base_en());
    config.use_gpu = use_gpu;
    let started = Instant::now();
    let transcriber = WhisperTranscriber::load(&config).expect("load base.en");
    (transcriber, started.elapsed())
}

/// Decodes a committed smoke FLAC to f32 mono samples in [-1, 1].
pub fn load_smoke_clip(id: &str) -> Vec<f32> {
    let path = repo_root()
        .join("fixtures/smoke")
        .join(format!("{id}.flac"));
    let mut reader =
        claxon::FlacReader::open(&path).unwrap_or_else(|e| panic!("open {}: {e}", path.display()));
    let info = reader.streaminfo();
    assert_eq!(info.sample_rate, SAMPLE_RATE, "smoke clips are 16 kHz");
    assert_eq!(info.channels, 1, "smoke clips are mono");
    assert_eq!(info.bits_per_sample, 16, "smoke clips are 16-bit");
    let scale = (1_i64 << (info.bits_per_sample - 1)) as f32;
    reader
        .samples()
        .map(|s| s.expect("flac sample") as f32 / scale)
        .collect()
}

/// A smoke clip as a pipeline buffer.
pub fn smoke_buffer(id: &str) -> MonoBuffer {
    MonoBuffer::new(PIPELINE_RATE, load_smoke_clip(id))
}

/// Every `(id, reference)` of the committed smoke set, in `transcripts.txt` order. Panics
/// unless every FLAC has exactly one transcript and there are [`SMOKE_CLIPS`] of them.
pub fn smoke_set() -> Vec<(String, String)> {
    let dir = repo_root().join("fixtures/smoke");
    let text = std::fs::read_to_string(dir.join("transcripts.txt")).expect("read transcripts.txt");
    let set: Vec<(String, String)> = text
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|line| {
            let (id, reference) = line.split_once(' ').expect("`<id> <reference>` line");
            (id.to_string(), reference.to_string())
        })
        .collect();
    let mut flacs: Vec<String> = std::fs::read_dir(&dir)
        .expect("read fixtures/smoke")
        .map(|e| e.expect("dir entry").path())
        .filter(|p| p.extension().is_some_and(|x| x == "flac"))
        .map(|p| p.file_stem().unwrap().to_string_lossy().into_owned())
        .collect();
    flacs.sort();
    let mut ids: Vec<String> = set.iter().map(|(id, _)| id.clone()).collect();
    ids.sort();
    assert_eq!(ids, flacs, "transcripts.txt and the smoke FLACs disagree");
    assert_eq!(
        set.len(),
        SMOKE_CLIPS,
        "the smoke set must have {SMOKE_CLIPS} clips"
    );
    set
}

/// Reference transcript of one smoke clip.
pub fn reference(id: &str) -> String {
    smoke_set()
        .into_iter()
        .find_map(|(i, r)| (i == id).then_some(r))
        .unwrap_or_else(|| panic!("{id} not in the smoke set"))
}

/// The `asr.wer_smoke_base` limit in percent, read from `acceptance/thresholds.toml`.
pub fn wer_smoke_base_limit() -> f64 {
    let path = repo_root().join("acceptance/thresholds.toml");
    let text = std::fs::read_to_string(&path).expect("read acceptance/thresholds.toml");
    let doc: toml::Table = text.parse().expect("parse thresholds.toml");
    let entry = doc["thresholds"][WER_SMOKE_BASE]
        .as_table()
        .unwrap_or_else(|| panic!("{WER_SMOKE_BASE} missing from {}", path.display()));
    assert_eq!(entry["milestone"].as_str(), Some("m2"));
    assert_eq!(entry["op"].as_str(), Some("<="), "{WER_SMOKE_BASE} op");
    assert_eq!(entry["unit"].as_str(), Some("%"), "{WER_SMOKE_BASE} unit");
    entry["value"]
        .as_float()
        .or_else(|| entry["value"].as_integer().map(|v| v as f64))
        .expect("numeric threshold value")
}

/// Corpus WER of one run over the smoke set.
pub struct SmokeRun {
    /// Summed alignment counts (corpus WER, PLAN.md 4.0).
    pub counts: WerCounts,
    /// Total audio duration.
    pub audio: Duration,
    /// Total time inside `transcribe`.
    pub transcribe: Duration,
}

impl SmokeRun {
    /// Corpus WER in percent.
    pub fn wer_percent(&self) -> f64 {
        self.counts.wer().expect("reference words") * 100.0
    }

    /// One-line summary for the test output.
    pub fn summary(&self, label: &str, load: Duration) -> String {
        format!(
            "{label}: {WER_SMOKE_BASE} = {:.2}% ({} errors: S{} D{} I{} / {} words) over {} clips, \
             {:.1} s audio, load {:.2} s, transcribe {:.2} s (RTF {:.3}), decoding {:?}",
            self.wer_percent(),
            self.counts.errors(),
            self.counts.substitutions,
            self.counts.deletions,
            self.counts.insertions,
            self.counts.ref_words,
            SMOKE_CLIPS,
            self.audio.as_secs_f64(),
            load.as_secs_f64(),
            self.transcribe.as_secs_f64(),
            self.transcribe.as_secs_f64() / self.audio.as_secs_f64(),
            TranscribeOptions::batch().decoding,
        )
    }
}

/// Checks the shape of one transcription of `audio`: the reported backend is the
/// transcriber's, words are in order and inside the audio.
pub fn assert_well_formed(t: &Transcription, transcriber: &WhisperTranscriber, audio: &MonoBuffer) {
    assert_eq!(t.backend, transcriber.backend_info());
    assert_eq!(t.language.as_deref(), Some("en"));
    let duration_ms = (audio.samples.len() as u64 * 1000).div_ceil(u64::from(SAMPLE_RATE));
    let words = t.words();
    assert!(!words.is_empty(), "no words in {t:?}");
    for pair in words.windows(2) {
        assert!(
            pair[0].start <= pair[1].start,
            "words out of order: {pair:?}"
        );
    }
    for w in &words {
        assert!(w.start <= w.end, "{w:?}");
        // whisper.cpp timestamps have 10 ms resolution.
        assert!(
            w.end.as_millis() <= duration_ms + 10,
            "{w:?} beyond {duration_ms} ms"
        );
        assert!((0.0..=1.0).contains(&w.probability), "{w:?}");
        assert!(!w.text.trim().is_empty(), "{w:?}");
    }
}

/// Transcribes the whole smoke set with batch decoding (PLAN.md 4.0) and sums WER counts
/// after Whisper normalisation.
pub fn run_smoke_set(transcriber: &mut WhisperTranscriber) -> SmokeRun {
    let options = TranscribeOptions::batch();
    let cancel = CancelToken::new();
    let mut per_clip = Vec::new();
    let (mut audio, mut transcribe) = (Duration::ZERO, Duration::ZERO);
    for (id, reference) in smoke_set() {
        let buffer = smoke_buffer(&id);
        audio += Duration::from_secs_f64(buffer.samples.len() as f64 / f64::from(SAMPLE_RATE));
        let started = Instant::now();
        let t = transcriber
            .transcribe(&buffer, &options, &cancel)
            .unwrap_or_else(|e| panic!("{id}: {e}"));
        transcribe += started.elapsed();
        assert_well_formed(&t, transcriber, &buffer);
        let hypothesis: Vec<&str> = t.segments.iter().map(|s| s.text.as_str()).collect();
        let hypothesis = hypothesis.join(" ");
        let counts = word_errors_normalized(&reference, &hypothesis);
        if counts.errors() > 0 {
            eprintln!("{id}: {counts:?}\n  ref: {reference}\n  hyp: {hypothesis}");
        }
        per_clip.push(counts);
    }
    SmokeRun {
        counts: corpus_wer(per_clip),
        audio,
        transcribe,
    }
}

/// Routes whisper.cpp and ggml logs into the library's capture buffer
/// (`earshot_asr_whisper::evidence`, the same rule the CLI and `accept m2` use).
pub fn capture_whisper_logs() {
    evidence::capture_logs(false).expect("no other logger in this test binary");
}

/// Every whisper.cpp / ggml log line captured so far.
pub fn captured_logs() -> Vec<String> {
    evidence::captured_logs()
}
