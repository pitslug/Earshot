//! `earshot replay` end to end: the built binary replays a synthesized two-track meeting
//! (committed smoke speech) through the runtime with the production backends, then the
//! test reads the data directory back.
//!
//! These tests need the `ci` model set (whisper base.en, Silero VAD, pyannote
//! segmentation, CAM++, ONNX Runtime CPU). A missing model fails the test with the
//! runtime's `cargo xtask fetch-models` instruction in the output; it is never a skip
//! (AGENTS.md 11). No audio device is used.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};

use earshot_core::asr::BackendKind;
use earshot_core::audio::{
    PIPELINE_RATE, SampleRate, WavFormat, read_wav, resample_all, write_wav,
};
use earshot_core::model::{AudioFileRepo, ExportRepo, MeetingRepo, TranscriptPass};
use earshot_core::summary::SummaryV1;
use earshot_llm_replay::mock::{MockResponse, MockServer};
use earshot_runtime::backends::pass_backend;
use earshot_runtime::{SystemClock, os_seeded_rng};
use earshot_store::{DataDir, Store};

const ME: &str = "61-70968-0000";
const THEM: &str = "121-121726-0002";

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// A temp directory removed on drop (std only, AGENTS.md).
struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        static N: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "earshot-cli-replay-{tag}-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::SeqCst)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A committed smoke clip at 16 kHz mono.
fn smoke(clip: &str) -> Vec<f32> {
    let path = repo_root().join(format!("fixtures/smoke/{clip}.flac"));
    let mut reader = claxon::FlacReader::open(&path).expect("open smoke clip");
    let info = reader.streaminfo();
    assert_eq!((info.sample_rate, info.channels), (16_000, 1));
    let scale = 1.0 / (1_u64 << (info.bits_per_sample - 1)) as f32;
    reader
        .samples()
        .map(|s| s.unwrap() as f32 * scale)
        .collect()
}

/// Two turns: "me" on the mic, then "them" on a 48 kHz stereo loopback (no echo, so the
/// canceller passes the mic through). Returns the mic and reference paths.
fn two_track_meeting(dir: &Path) -> (PathBuf, PathBuf) {
    let me = smoke(ME);
    let them = smoke(THEM);
    let gap = vec![0.0; 8_000];
    let mut mic = me.clone();
    mic.extend_from_slice(&gap);
    mic.extend(std::iter::repeat_n(0.0, them.len()));
    let mut far = vec![0.0; me.len() + gap.len()];
    far.extend_from_slice(&them);
    let far48 = resample_all(&far, PIPELINE_RATE, SampleRate::new(48_000).unwrap()).unwrap();
    let stereo: Vec<f32> = far48.iter().flat_map(|&x| [x, x]).collect();

    let mic_path = dir.join("mic.wav");
    let ref_path = dir.join("loopback.wav");
    write(&mic_path, PIPELINE_RATE, 1, &mic);
    write(&ref_path, SampleRate::new(48_000).unwrap(), 2, &stereo);
    (mic_path, ref_path)
}

/// A float WAV with `channels` interleaved channels, checked by reading it back.
fn write(path: &Path, rate: SampleRate, channels: u16, samples: &[f32]) {
    let file = std::fs::File::create(path).unwrap();
    write_wav(
        std::io::BufWriter::new(file),
        rate,
        channels,
        samples,
        WavFormat::Float32,
    )
    .unwrap();
    let back = read_wav(std::io::BufReader::new(std::fs::File::open(path).unwrap())).unwrap();
    assert_eq!((back.rate(), back.channels()), (rate, channels));
}
fn earshot(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_earshot"))
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

fn s(p: &Path) -> &str {
    p.to_str().expect("utf-8 path")
}

fn summary_reply() -> MockResponse {
    let summary = SummaryV1 {
        title: "Replay check".into(),
        tldr: "Two people spoke in turn.".into(),
        enhanced_notes: vec![],
        decisions: vec![],
        action_items: vec![],
        open_questions: vec![],
        topics: vec!["replay".into()],
    };
    MockResponse::completion(&serde_json::to_string(&summary).unwrap(), "stop")
}

fn open_store(data: &Path) -> Store {
    // Opened only after the CLI process has exited.
    Store::open(&DataDir::new(data).db_path()).expect("the replay left a database")
}

#[test]
fn replay_two_tracks_records_transcribes_summarises_and_exports_on_request() {
    let dir = TempDir::new("full");
    let (mic, reference) = two_track_meeting(&dir.0);
    let data = dir.0.join("data");
    let vault = dir.0.join("vault");
    std::fs::create_dir_all(&vault).unwrap();
    let server = MockServer::start(vec![summary_reply()]).unwrap();
    let endpoint = server.endpoint();

    let out = earshot(&[
        "replay",
        "--mic",
        s(&mic),
        "--ref",
        s(&reference),
        "--out",
        s(&data),
        "--model",
        "base.en",
        "--llm-url",
        endpoint.as_str(),
        "--llm-model",
        "test-model",
        "--export-vault",
        s(&vault),
    ]);
    assert!(out.status.success(), "{}", describe(&out));
    let stdout = String::from_utf8_lossy(&out.stdout);

    // Reports say which backend ran: live per track, then the final pass.
    assert!(stdout.contains("live asr mic: Cpu"), "{}", describe(&out));
    assert!(
        stdout.contains("live asr reference: Cpu"),
        "{}",
        describe(&out)
    );
    assert!(
        stdout.contains("final pass ran on Cpu"),
        "{}",
        describe(&out)
    );
    // AEC stats are printed for a session with a reference.
    assert!(stdout.contains("aec "), "{}", describe(&out));
    // Network: only the configured endpoint, and it is printed.
    let host_port = format!(
        "{}:{}",
        endpoint.host_str().unwrap(),
        endpoint.port().unwrap()
    );
    let net_lines: Vec<&str> = stdout
        .lines()
        .filter(|l| l.starts_with("network:"))
        .collect();
    assert!(!net_lines.is_empty(), "{}", describe(&out));
    for l in &net_lines {
        assert!(l.contains(&host_port), "{l}\n{}", describe(&out));
    }
    assert_eq!(server.requests().len(), 1);
    assert_eq!(server.unscripted(), 0);

    let mut store = open_store(&data);
    let meetings = store.meetings().unwrap();
    assert_eq!(meetings.len(), 1);
    let meeting = meetings[0].id;
    // Both tracks recorded to FLAC.
    let files = store.audio_files(meeting).unwrap();
    assert_eq!(files.len(), 2, "{}", describe(&out));
    for f in &files {
        assert!(
            DataDir::new(&data).resolve(&f.path).unwrap().is_file(),
            "{f:?}"
        );
    }
    // The final pass ran and its backend is recorded.
    let backend = pass_backend(&store, meeting, TranscriptPass::Final, None).unwrap();
    assert_eq!(backend.map(|b| b.kind), Some(BackendKind::Cpu));
    let segments = store.segments(meeting).unwrap();
    assert!(
        segments.iter().any(|r| r.pass == TranscriptPass::Final),
        "no final segments\n{}",
        describe(&out)
    );
    let text = segments
        .iter()
        .filter(|r| r.pass == TranscriptPass::Final)
        .map(|r| r.segment.text.to_lowercase())
        .collect::<Vec<_>>()
        .join(" ");
    // Both turns were transcribed (one word of each clip's reference, loosely).
    let transcripts =
        std::fs::read_to_string(repo_root().join("fixtures/smoke/transcripts.txt")).unwrap();
    for clip in [ME, THEM] {
        let reference = transcripts
            .lines()
            .find_map(|l| l.strip_prefix(clip))
            .unwrap()
            .to_lowercase();
        assert!(
            reference
                .split_whitespace()
                .filter(|w| w.len() > 4)
                .any(|w| text.contains(w)),
            "{clip}: {reference:?} not in {text:?}"
        );
    }
    // The summary was stored and the note exported, because both were asked for.
    assert_eq!(store.summaries(meeting).unwrap().len(), 1);
    let device = store
        .device_id(&SystemClock, &mut *os_seeded_rng())
        .unwrap();
    assert!(store.export_record(meeting, device).unwrap().is_some());
    let notes: Vec<_> = walk(&vault)
        .into_iter()
        .filter(|p| p.extension().is_some_and(|e| e == "md"))
        .collect();
    assert_eq!(notes.len(), 1, "{notes:?}");
}

#[test]
fn replay_without_endpoint_or_vault_stays_offline_and_exports_nothing() {
    // --no-summary: no local model either (T6.21b; otherwise the local summary runs).
    let dir = TempDir::new("offline");
    let (mic, _) = two_track_meeting(&dir.0);
    let data = dir.0.join("data");
    let out = earshot(&[
        "replay",
        "--mic",
        s(&mic),
        "--out",
        s(&data),
        "--model",
        "base.en",
        "--no-post",
        "--no-summary",
    ]);
    assert!(out.status.success(), "{}", describe(&out));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("live asr mic: Cpu"), "{}", describe(&out));
    assert!(
        stdout.contains("summary: skipped (--no-summary)"),
        "{}",
        describe(&out)
    );
    assert!(
        stdout.contains("network: none (summary skipped)"),
        "{}",
        describe(&out)
    );
    assert!(
        stdout.contains("aec off (no reference)"),
        "{}",
        describe(&out)
    );

    let mut store = open_store(&data);
    let meetings = store.meetings().unwrap();
    assert_eq!(meetings.len(), 1);
    let meeting = meetings[0].id;
    assert_eq!(store.audio_files(meeting).unwrap().len(), 1);
    // --no-post: only the live pass.
    let segments = store.segments(meeting).unwrap();
    assert!(!segments.is_empty(), "{}", describe(&out));
    assert!(segments.iter().all(|r| r.pass == TranscriptPass::Live));
    assert!(store.summaries(meeting).unwrap().is_empty());
    let device = store
        .device_id(&SystemClock, &mut *os_seeded_rng())
        .unwrap();
    assert!(store.export_record(meeting, device).unwrap().is_none());
}

#[test]
#[ignore = "needs the summary model set: cargo xtask fetch-models --set summary; run: cargo test -p earshot-cli --test replay replay_summarises_locally -- --include-ignored"]
fn replay_summarises_locally_with_no_network() {
    // T6.21b: no endpoint, so the summary is made on this machine by the named model.
    let dir = TempDir::new("local");
    let (mic, reference) = two_track_meeting(&dir.0);
    let data = dir.0.join("data");
    let out = earshot(&[
        "replay",
        "--mic",
        s(&mic),
        "--ref",
        s(&reference),
        "--out",
        s(&data),
        "--model",
        "base.en",
        "--no-post",
        "--summary-model",
        "qwen3.5-4b-q4_k_m",
    ]);
    assert!(out.status.success(), "{}", describe(&out));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("summary model: qwen3.5-4b-q4_k_m on cpu"),
        "{}",
        describe(&out)
    );
    assert!(
        stdout.contains("network: none (local summary, 0 audit entries)"),
        "{}",
        describe(&out)
    );
    assert!(
        !stdout
            .lines()
            .any(|l| l.starts_with("network: ") && l.contains("://")),
        "{}",
        describe(&out)
    );
    let store = open_store(&data);
    let meeting = store.meetings().unwrap()[0].id;
    let rows = store.summaries(meeting).unwrap();
    assert_eq!(rows.len(), 1, "{}", describe(&out));
    assert_eq!(rows[0].model.as_deref(), Some("qwen3.5-4b-q4_k_m"));
    assert!(!rows[0].summary.title.trim().is_empty());
}

#[test]
fn replay_event_log_times_a_realtime_meeting_and_the_whole_pass_skips_the_live_work() {
    // T6.13: `--events-json` logs the audio origin, every live final (times and word count,
    // never text), the end and each cleanup step; `--whole-pass` transcribes and diarises
    // every track whole although the stretches ran.
    let dir = TempDir::new("events");
    let (mic, reference) = two_track_meeting(&dir.0);
    let data = dir.0.join("data");
    let log = dir.0.join("events.jsonl");
    let out = earshot(&[
        "replay",
        "--mic",
        s(&mic),
        "--ref",
        s(&reference),
        "--out",
        s(&data),
        "--model",
        "base.en",
        "--realtime",
        "--whole-pass",
        "--no-summary",
        "--events-json",
        s(&log),
    ]);
    assert!(out.status.success(), "{}", describe(&out));
    let stdout = String::from_utf8_lossy(&out.stdout);
    for track in ["mic", "reference"] {
        assert!(
            stdout.contains(&format!(
                "post: {track}: transcribed the whole track (the whole-meeting pass was requested)"
            )),
            "{}",
            describe(&out)
        );
        assert!(
            stdout.contains(&format!(
                "post: {track}: found the speakers over the whole track (the whole-meeting pass was requested)"
            )),
            "{}",
            describe(&out)
        );
    }
    let text = std::fs::read_to_string(&log).expect("the event log was written");
    let lines: Vec<serde_json::Value> = text
        .lines()
        .map(|l| serde_json::from_str(l).expect("JSON lines"))
        .collect();
    let kinds: Vec<&str> = lines.iter().filter_map(|l| l["kind"].as_str()).collect();
    for k in [
        "started",
        "audio_origin",
        "live_backend",
        "final",
        "ended",
        "post_start",
    ] {
        assert!(kinds.contains(&k), "{k} missing: {kinds:?}");
    }
    let stored = lines
        .iter()
        .position(|l| l["kind"] == "post_stage" && l["stage"] == "stored")
        .expect("the cleanup stored the final pass");
    let ended = kinds.iter().position(|k| *k == "ended").unwrap();
    assert!(ended < stored, "{kinds:?}");
    let t: Vec<f64> = lines.iter().map(|l| l["t_ms"].as_f64().unwrap()).collect();
    assert!(t.windows(2).all(|w| w[0] <= w[1]), "wall times in order");
    // Realtime: no final is shown before its audio was delivered.
    let origin = lines.iter().find(|l| l["kind"] == "audio_origin").unwrap()["t_ms"]
        .as_f64()
        .unwrap();
    for f in lines.iter().filter(|l| l["kind"] == "final") {
        let shown = f["t_ms"].as_f64().unwrap() - origin;
        assert!(shown >= f["end_ms"].as_f64().unwrap(), "{f}");
        assert!(f["words"].as_u64().unwrap() > 0, "{f}");
    }
    // No transcript text in the log: no two consecutive words of any stored segment (live
    // or final) appear in it.
    let store = open_store(&data);
    let meeting = store.meetings().unwrap()[0].id;
    let clean = |w: &str| {
        w.chars()
            .filter(|c| c.is_alphanumeric() || *c == '\'')
            .collect::<String>()
            .to_lowercase()
    };
    let phrases: Vec<String> = store
        .segments(meeting)
        .unwrap()
        .into_iter()
        .flat_map(|r| {
            let w: Vec<String> = r.segment.words.iter().map(|w| clean(&w.text)).collect();
            w.windows(2)
                .map(|p| format!("{} {}", p[0], p[1]))
                .collect::<Vec<_>>()
        })
        .collect();
    assert!(!phrases.is_empty(), "{}", describe(&out));
    let lower = text.to_lowercase();
    for p in &phrases {
        assert!(
            !lower.contains(p.as_str()),
            "transcript text {p:?} in the event log"
        );
    }
}

#[test]
fn replay_summarises_through_anthropic_with_the_key_from_the_environment() {
    const KEY: &str = "sk-ant-test-SECRET99";
    let dir = TempDir::new("anthropic");
    let (mic, _) = two_track_meeting(&dir.0);
    let data = dir.0.join("data");
    let summary = SummaryV1 {
        title: "Replay check".into(),
        tldr: "Two people spoke in turn.".into(),
        enhanced_notes: vec![],
        decisions: vec![],
        action_items: vec![],
        open_questions: vec![],
        topics: vec!["replay".into()],
    };
    let server = MockServer::start(vec![MockResponse::anthropic_text(
        &serde_json::to_string(&summary).unwrap(),
        "end_turn",
    )])
    .unwrap();
    let base = server.base_url();
    let out = Command::new(env!("CARGO_BIN_EXE_earshot"))
        .args([
            "replay",
            "--mic",
            s(&mic),
            "--out",
            s(&data),
            "--model",
            "base.en",
            "--no-post",
            "--llm-provider",
            "anthropic",
            "--llm-url",
            base.as_str(),
            "--llm-model",
            "claude-test",
        ])
        .env("ANTHROPIC_API_KEY", KEY)
        .output()
        .expect("run earshot");
    assert!(out.status.success(), "{}", describe(&out));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(!describe(&out).contains("SECRET99"), "{}", describe(&out));
    // Network: only the configured host, the Messages API path.
    let target = format!(
        "{}:{}/v1/messages",
        base.host_str().unwrap(),
        base.port().unwrap()
    );
    let net_lines: Vec<&str> = stdout
        .lines()
        .filter(|l| l.starts_with("network:"))
        .collect();
    assert_eq!(net_lines.len(), 1, "{}", describe(&out));
    assert!(net_lines[0].contains(&target), "{}", describe(&out));
    let reqs = server.requests();
    assert_eq!(reqs.len(), 1);
    assert_eq!(reqs[0].header("x-api-key"), Some(KEY));
    assert_eq!(reqs[0].json()["model"], "claude-test");

    let store = open_store(&data);
    let meeting = store.meetings().unwrap()[0].id;
    let rows = store.summaries(meeting).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].model.as_deref(), Some("claude-test"));
}

#[test]
fn replay_refuses_anthropic_without_a_key_before_recording() {
    let dir = TempDir::new("nokey");
    let (mic, _) = two_track_meeting(&dir.0);
    let data = dir.0.join("data");
    let out = Command::new(env!("CARGO_BIN_EXE_earshot"))
        .args([
            "replay",
            "--mic",
            s(&mic),
            "--out",
            s(&data),
            "--model",
            "base.en",
            "--llm-provider",
            "anthropic",
        ])
        .env_remove("ANTHROPIC_API_KEY")
        .output()
        .expect("run earshot");
    assert!(!out.status.success(), "{}", describe(&out));
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("ANTHROPIC_API_KEY"),
        "{}",
        describe(&out)
    );
    let recorded = DataDir::new(&data).db_path().is_file()
        && !open_store(&data).meetings().unwrap().is_empty();
    assert!(!recorded, "{}", describe(&out));
}

#[test]
fn replay_refuses_a_public_plain_http_endpoint_before_recording() {
    let dir = TempDir::new("denied");
    let (mic, _) = two_track_meeting(&dir.0);
    let data = dir.0.join("data");
    let out = earshot(&[
        "replay",
        "--mic",
        s(&mic),
        "--out",
        s(&data),
        "--model",
        "base.en",
        "--llm-url",
        "http://example.com/v1",
        "--llm-model",
        "m",
    ]);
    assert!(!out.status.success(), "{}", describe(&out));
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("summary endpoint"),
        "{}",
        describe(&out)
    );
    // Nothing was recorded.
    let recorded = DataDir::new(&data).db_path().is_file()
        && !open_store(&data).meetings().unwrap().is_empty();
    assert!(!recorded, "{}", describe(&out));
}

fn walk(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for e in std::fs::read_dir(dir).unwrap() {
        let p = e.unwrap().path();
        if p.is_dir() {
            out.extend(walk(&p));
        } else {
            out.push(p);
        }
    }
    out
}
