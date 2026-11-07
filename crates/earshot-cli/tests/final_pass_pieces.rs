//! The final pass never stores a whisper repetition loop (T6.16): `earshot-cli replay` decodes
//! each track in speech pieces, and the stored final transcript is read back.
//!
//! `final_pass_pieces_quiet_stretches_between_speech` needs the `ci` model set (whisper
//! base.en, Silero VAD, pyannote segmentation, CAM++, ONNX Runtime CPU); a missing model
//! fails with the runtime's `cargo xtask fetch-models` instruction (AGENTS.md 11).
//! `final_pass_pieces_gate12_take2` replays a private recording (never committed), named with
//! the words it must keep in `gate12.toml` outside the repo, and is ignored; see its reason for
//! the command.

#[path = "../../earshot-asr-whisper/examples/asr_eval/gate12_private.rs"]
mod gate12_private;

use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};

use earshot_core::asr::compression_ratio;
use earshot_core::audio::{PIPELINE_RATE, WavFormat, write_wav};
use earshot_core::capture::TrackId;
use earshot_core::model::{MeetingRepo, TranscriptPass};
use earshot_store::{DataDir, Store};

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// A temp directory removed on drop (std only, AGENTS.md).
struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        static N: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "earshot-cli-pieces-{tag}-{}-{}",
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

/// `seconds` of quiet seeded noise (about -55 dBFS), like an echo residual.
fn quiet(seconds: usize, seed: &mut u64) -> Vec<f32> {
    (0..seconds * 16_000)
        .map(|_| {
            *seed = seed.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
            ((*seed >> 33) as f32 / (1_u64 << 31) as f32 - 0.5) * 0.004
        })
        .collect()
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

fn s(p: &Path) -> &str {
    p.to_str().expect("utf-8 path")
}

/// The stored final transcript of the mic track, one string per segment.
fn final_mic_segments(data: &Path) -> Vec<String> {
    let store = Store::open(&DataDir::new(data).db_path()).expect("the replay left a database");
    let meeting = store.meetings().unwrap()[0].id;
    let mic = store
        .tracks(meeting)
        .unwrap()
        .into_iter()
        .find(|t| t.kind == TrackId::Mic)
        .expect("a mic track")
        .id;
    store
        .segments(meeting)
        .unwrap()
        .into_iter()
        .filter(|r| r.pass == TranscriptPass::Final && r.segment.track == mic)
        .map(|r| r.segment.text)
        .collect()
}

/// The longest run of the same sentence said back to back.
fn longest_repeat(text: &str) -> usize {
    let sentences: Vec<String> = text
        .split(['.', '?', '!'])
        .map(|s| {
            s.chars()
                .filter(|c| c.is_alphanumeric() || c.is_whitespace())
                .collect::<String>()
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ")
                .to_lowercase()
        })
        .filter(|s| !s.is_empty())
        .collect();
    let mut best = usize::from(!sentences.is_empty());
    let mut run = 1;
    for w in sentences.windows(2) {
        run = if w[0] == w[1] { run + 1 } else { 1 };
        best = best.max(run);
    }
    best
}

/// No sentence said three times running, and no segment as repetitive as a whisper loop.
fn assert_no_loop(segments: &[String]) {
    let text = segments.join(" ");
    assert!(longest_repeat(&text) < 3, "a repeated sentence: {text:?}");
    for s in segments {
        assert!(
            compression_ratio(s) <= 2.4,
            "a repetitive segment ({}): {s:?}",
            compression_ratio(s)
        );
    }
}

#[test]
fn final_pass_pieces_helpers_see_a_loop() {
    assert_eq!(
        longest_repeat("I checked the valve. I checked the valve. I checked the valve."),
        3
    );
    assert_eq!(
        longest_repeat("Reading them again. The numbers came in. The numbers came in."),
        2
    );
    assert_eq!(longest_repeat(""), 0);
}

#[test]
fn final_pass_pieces_quiet_stretches_between_speech() {
    let dir = TempDir::new("quiet");
    let clips = ["61-70968-0000", "61-70968-0003", "61-70968-0008"];
    let mut seed = 7;
    let mut mic = quiet(2, &mut seed);
    for (i, clip) in clips.iter().enumerate() {
        mic.extend(smoke(clip));
        mic.extend(quiet(if i == 0 { 60 } else { 30 }, &mut seed));
    }
    let path = dir.0.join("mic.wav");
    let file = std::fs::File::create(&path).unwrap();
    write_wav(
        std::io::BufWriter::new(file),
        PIPELINE_RATE,
        1,
        &mic,
        WavFormat::Float32,
    )
    .unwrap();
    let data = dir.0.join("data");
    let out = earshot(&[
        "replay",
        "--mic",
        s(&path),
        "--out",
        s(&data),
        "--model",
        "base.en",
        "--no-summary",
    ]);
    assert!(out.status.success(), "{}", describe(&out));
    // T6.18: the cleanup is announced with a rough duration and reports each step.
    let stdout = String::from_utf8_lossy(&out.stdout);
    for line in [
        "post: cleaning up the transcript",
        "post: re-transcribed the mic track",
        "speaker(s) on the mic track",
        "post: echo check done",
        "post: speakers named and transcript stored",
    ] {
        assert!(
            stdout.contains(line),
            "{line:?} missing\n{}",
            describe(&out)
        );
    }
    let segments = final_mic_segments(&data);
    assert_no_loop(&segments);
    // Every clip was transcribed (one longer word of each reference, loosely).
    let text = segments.join(" ").to_lowercase();
    let transcripts =
        std::fs::read_to_string(repo_root().join("fixtures/smoke/transcripts.txt")).unwrap();
    for clip in clips {
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
}

#[test]
#[ignore = "needs the private GATE-12 recording and settings (.earshot-private/gate12.toml) \
            and a cuda build with cuDNN on PATH: cargo test --release -p earshot-cli \
            --features cuda --target-dir target/cuda --test final_pass_pieces -- --include-ignored"]
fn final_pass_pieces_gate12_take2() {
    let path = gate12_private::default_path();
    let settings = gate12_private::load(path.as_deref())
        .unwrap_or_else(|e| panic!("gate12 settings: {e}"))
        .expect("this test needs .earshot-private/gate12.toml in the home folder");
    let take2 = repo_root().join(&settings.model_eval_dir);
    let dir = TempDir::new("gate12");
    let data = dir.0.join("data");
    let out = earshot(&[
        "replay",
        "--mic",
        s(&take2.join("mic.wav")),
        "--ref",
        s(&take2.join("loopback.wav")),
        "--out",
        s(&data),
        "--no-summary",
    ]);
    assert!(out.status.success(), "{}", describe(&out));
    let segments = final_mic_segments(&data);
    assert_no_loop(&segments);
    // The later parts of the script are in the stored "Me" transcript (the whole-track decode
    // looped and lost them).
    let text = segments.join(" ").to_lowercase();
    for word in &settings.script_words {
        assert!(
            text.contains(word.as_str()),
            "{word:?} missing from {text:?}"
        );
    }
}
