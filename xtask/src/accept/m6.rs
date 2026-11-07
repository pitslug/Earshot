//! `accept m6`: a replayed two-track meeting end to end (T6.6, PLAN.md 4.2 `e2e.replay`).
//!
//! 1. **Meeting.** The call simulator (`earshot_core::sim`) renders the meeting described
//!    in `acceptance/m6_meeting.toml` from the committed smoke speech: the far end
//!    (speaker 61) is the loopback, the mic carries the near end ("Me", speaker 121) plus
//!    the far end's echo through the room and room noise. Both are written as 16 kHz WAV
//!    files into a work directory under the system temp dir.
//! 2. **Replay.** The shipped CLI runs it (`cargo run --release -p earshot-cli -- replay`,
//!    default CPU build) through the runtime: live transcription and echo cancellation,
//!    FLAC recording, the post pass (final transcript with whisper [`MODEL`], diarisation,
//!    voiceprint naming), a summary from the `earshot-llm-replay` mock on 127.0.0.1, and
//!    an Obsidian export into a temp vault. Models come from the `cargo xtask
//!    fetch-models` cache; a missing one fails the run with the fetch command (the CLI
//!    checks every model before recording).
//! 3. **Checks** on what the run left behind: the database in the data directory, the
//!    files, the mock's recorded requests and the CLI's printed audit log. `e2e.replay`
//!    passes when every check passes:
//!    - `audio_files`: one FLAC per track, 16 kHz, exactly as long as the meeting, and the
//!      meeting duration matches.
//!    - `transcript`: final-pass segments with text on both tracks; the live pass and the
//!      final pass ran on the CPU (the CLI's backend lines, from the runtime's verified
//!      backends).
//!    - `speakers_separated`: every mic segment belongs to the "Me" speaker (role `Me`),
//!      every loopback segment to a remote speaker, and no speaker spans both tracks.
//!    - `content_separated`: of the words of each track's transcript that only one
//!      talker's smoke transcripts contain, more come from that track's own talker than
//!      from the other one, and at least one: each transcript is its own talker's.
//!    - `echo_leak`: no such word of the other talker at all: zero far-end-only words in
//!      the "Me" transcript (echo leaking into the mic is never attributed to "Me") and
//!      zero near-end-only words in the loopback transcript. The vocabulary sets only see
//!      words unique to one talker, so a leaked shared word goes unnoticed here.
//!    - `summary`: one summary stored for the meeting, valid against the schema rules
//!      with every cited id a final-pass segment of the meeting, citing at least one
//!      segment; the meeting took its title. The mock writes the summary from the prompt
//!      it receives (it cites the transcript lines it finds, see [`mock_summary`]), so a
//!      prompt without transcript lines yields a summary that cites nothing and fails.
//!    - `prompt`: every request is a chat completion whose prompt holds transcript lines
//!      of "Me" and of the remote speaker, and no audio (no audio content parts, bodies
//!      far smaller than the recording).
//!    - `export`: exactly one note in the vault, at the export record's path, with the
//!      meeting's `earshot_id`, the managed region, the summary's tl;dr and both speakers.
//!    - `network`: the CLI printed the `earshot-net` audit log: at least one entry, every
//!      entry a `POST` to the mock's host, port and `/v1/chat/completions` that succeeded,
//!      as many entries as the mock received requests.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail, ensure};
use earshot_core::asr::Segment;
use earshot_core::audio::{PIPELINE_RATE, WavFormat, write_wav};
use earshot_core::capture::TrackId;
use earshot_core::diar::SpeakerRole;
use earshot_core::export::{BEGIN_MARKER, END_MARKER, earshot_id_of};
use earshot_core::identity::{EntityId, ManualClock};
use earshot_core::model::{AudioFileRepo, ExportRepo, MeetingRepo, TranscriptPass};
use earshot_core::sim::{Scenario, Setup, SimOutput, SpeechPool, simulate};
use earshot_core::summary::validate_summary;
use earshot_llm_replay::mock::{MockResponse, MockServer, RecordedRequest};
use earshot_store::{DataDir, Store, read_flac};
use rand_chacha::ChaCha8Rng;
use rand_core::SeedableRng;
use serde::Deserialize;
use serde_json::{Value, json};

use super::{Check, Ctx, Measurement, RunOutput};
use crate::ci::cargo_command;
use crate::proc::run_with_timeout;

/// The threshold id.
pub const ID: &str = "e2e.replay";

/// Whisper model of the live and final passes (PLAN.md T6.6; base.en, a default model, PLAN
/// D22).
pub const MODEL: &str = "base.en";

/// Model name configured for the summary endpoint (the mock ignores it).
const LLM_MODEL: &str = "m6-mock";

/// Deadline for building the CLI (release, including whisper.cpp from cold).
const BUILD_TIMEOUT: Duration = Duration::from_secs(60 * 60);

/// Deadline for replaying the meeting.
const RUN_TIMEOUT: Duration = Duration::from_secs(2 * 60 * 60);

/// A request body this large would carry audio (the meeting is 40 s, 1.3 MB as 16-bit
/// PCM); the transcript prompt is a few kB.
const MAX_REQUEST_BYTES: usize = 256 * 1024;

/// `acceptance/m6_meeting.toml`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MeetingFile {
    /// Room and positions.
    pub setup: Setup,
    /// The call.
    pub meeting: Scenario,
}

/// Parses `root/acceptance/m6_meeting.toml`.
///
/// # Errors
/// Missing or malformed file.
pub fn meeting_file(root: &Path) -> Result<MeetingFile> {
    let path = root.join("acceptance/m6_meeting.toml");
    let text = std::fs::read_to_string(&path)
        .with_context(|| format!("cannot read {}", path.display()))?;
    toml::from_str(&text).with_context(|| format!("{}", path.display()))
}

/// Renders the meeting (deterministic: the scenario seed drives the simulator's RNG).
///
/// # Errors
/// Missing smoke speech or an invalid scenario.
pub fn simulate_meeting(root: &Path, file: &MeetingFile) -> Result<SimOutput> {
    let (far, near) = crate::echo_nl::smoke_speech(root)?;
    let pool = SpeechPool {
        far: &far,
        near: &near,
    };
    let mut rng = ChaCha8Rng::seed_from_u64(file.meeting.seed);
    simulate(&file.meeting, &file.setup, &pool, &mut rng).map_err(|e| anyhow::anyhow!("{e}"))
}

/// A transcript line of a summary prompt: `[<segment number>] <hh:mm:ss> <speaker>: <text>`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptLine {
    /// Segment number (from 1, per meeting), the value a reply cites.
    pub number: u64,
    /// Speaker name.
    pub speaker: String,
}

/// The transcript lines of a prompt (other lines are ignored).
#[must_use]
pub fn prompt_lines(prompt: &str) -> Vec<PromptLine> {
    prompt
        .lines()
        .filter_map(|l| {
            let rest = l.strip_prefix('[')?;
            let (number, rest) = rest.split_once("] ")?;
            let number: u64 = number.parse().ok().filter(|&n| n >= 1)?;
            let (_time, rest) = rest.split_once(' ')?;
            let (speaker, _text) = rest.split_once(": ")?;
            Some(PromptLine {
                number,
                speaker: speaker.to_owned(),
            })
        })
        .collect()
}

/// The user-role text of a chat completion request.
fn user_prompt(body: &Value) -> String {
    body["messages"]
        .as_array()
        .map(|m| {
            m.iter()
                .filter(|m| m["role"] == "user")
                .filter_map(|m| m["content"].as_str())
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default()
}

/// The mock model: a summary written from the transcript lines of the prompt. Each
/// speaker gets a note bullet citing their first lines (at most
/// `earshot_core::summary::MAX_CITATIONS`); the decision cites the first line and the action
/// item the first line of the first speaker other than "Me" (else the first line). Replies
/// cite segment numbers, as the prompt shows them. Without transcript lines it cites
/// nothing (and `accept` fails).
#[must_use]
pub fn mock_summary(prompt: &str) -> Value {
    let lines = prompt_lines(prompt);
    let mut by_speaker: BTreeMap<&str, Vec<u64>> = BTreeMap::new();
    for l in &lines {
        let cites = by_speaker.entry(l.speaker.as_str()).or_default();
        if cites.len() < earshot_core::summary::MAX_CITATIONS {
            cites.push(l.number);
        }
    }
    let bullets: Vec<Value> = by_speaker
        .iter()
        .map(|(speaker, ids)| json!({ "text": format!("{speaker} spoke"), "source_segment_ids": ids }))
        .collect();
    let first = lines.first();
    let owner = lines.iter().find(|l| l.speaker != "Me").or(first);
    let decisions: Vec<Value> = first
        .map(|l| json!({ "text": "Keep the replayed call on record", "source_segment_ids": [l.number] }))
        .into_iter()
        .collect();
    let actions: Vec<Value> = owner
        .map(|l| {
            json!({
                "owner": l.speaker,
                "task": "Follow up on the call",
                "due": null,
                "source_segment_ids": [l.number],
            })
        })
        .into_iter()
        .collect();
    json!({
        "title": "Replayed call",
        "tldr": format!("A call with {} transcript lines from {} speakers.", lines.len(), by_speaker.len()),
        "enhanced_notes": [{ "heading": "Speakers", "bullets": bullets }],
        "decisions": decisions,
        "action_items": actions,
        "open_questions": [],
        "topics": ["call"],
    })
}

pub(super) fn respond(req: &RecordedRequest) -> MockResponse {
    let summary = mock_summary(&user_prompt(&req.json()));
    MockResponse::completion(&summary.to_string(), "stop")
}

/// Words of `text` for vocabulary checks: lower case, letters and inner apostrophes,
/// at least four characters.
#[must_use]
pub fn words(text: &str) -> Vec<String> {
    text.split(|c: char| !(c.is_alphabetic() || c == '\''))
        .map(|w| w.trim_matches('\'').to_lowercase())
        .filter(|w| w.chars().count() >= 4)
        .collect()
}

/// The words only the near talker (speaker 121) and only the far talker (speaker 61) use
/// in `fixtures/smoke/transcripts.txt`.
///
/// # Errors
/// An unreadable file or a speaker without lines.
pub fn exclusive_vocabularies(root: &Path) -> Result<(BTreeSet<String>, BTreeSet<String>)> {
    let path = root.join("fixtures/smoke/transcripts.txt");
    let text = std::fs::read_to_string(&path)
        .with_context(|| format!("cannot read {}", path.display()))?;
    let (mut near, mut far) = (BTreeSet::new(), BTreeSet::new());
    for line in text.lines() {
        let Some((clip, words_of)) = line.split_once(' ') else {
            continue;
        };
        let set = if clip.starts_with("121-") {
            &mut near
        } else if clip.starts_with("61-") {
            &mut far
        } else {
            continue;
        };
        set.extend(words(words_of));
    }
    ensure!(
        !near.is_empty() && !far.is_empty(),
        "smoke transcripts for speakers 121 and 61 missing"
    );
    let near_only = near.difference(&far).cloned().collect();
    let far_only = far.difference(&near).cloned().collect();
    Ok((near_only, far_only))
}

/// How many words of `text` are in `own` and in `other`.
#[must_use]
pub fn vocabulary_hits(
    text: &str,
    own: &BTreeSet<String>,
    other: &BTreeSet<String>,
) -> (usize, usize) {
    let w = words(text);
    (
        w.iter().filter(|w| own.contains(*w)).count(),
        w.iter().filter(|w| other.contains(*w)).count(),
    )
}

/// The words of `text` that are in `other` (the other talker's own vocabulary), in order.
#[must_use]
pub fn leaked_words(text: &str, other: &BTreeSet<String>) -> Vec<String> {
    words(text)
        .into_iter()
        .filter(|w| other.contains(w))
        .collect()
}
/// The track's transcript is its own talker's: some own words, more than the other's.
#[must_use]
pub fn content_is_own(own: usize, other: usize) -> bool {
    own > 0 && own > other
}

/// The backend kinds the CLI printed: live mic, live reference, final pass.
///
/// # Errors
/// A line missing.
pub fn printed_backends(stdout: &str) -> Result<[String; 3]> {
    let kind_after = |prefix: &str| -> Result<String> {
        let line = stdout
            .lines()
            .find_map(|l| l.strip_prefix(prefix))
            .with_context(|| format!("no `{prefix}` line in the CLI output"))?;
        Ok(line
            .split_whitespace()
            .next()
            .unwrap_or_default()
            .to_owned())
    };
    Ok([
        kind_after("live asr mic: ")?,
        kind_after("live asr reference: ")?,
        kind_after("post: final pass ran on ")?,
    ])
}

/// The backend the run proved, lower case, when the CLI''s three backend lines (live mic,
/// live loopback, final pass) agree; `None` when they disagree or are missing.
#[must_use]
pub fn measured_backend(stdout: &str) -> Option<String> {
    let [a, b, c] = printed_backends(stdout).ok()?;
    (a == b && b == c).then(|| a.to_lowercase())
}

/// The executable of `cargo build --release -p earshot-cli` (from cargo''s JSON messages).
///
/// # Errors
/// A failed or timed-out build, or no `earshot` executable reported.
pub fn build_cli(root: &Path) -> Result<PathBuf> {
    let mut cmd = cargo_command();
    cmd.args([
        "build",
        "--release",
        "-p",
        "earshot-cli",
        "--bin",
        "earshot-cli",
        "--message-format=json-render-diagnostics",
    ])
    .current_dir(root);
    eprintln!("accept m6: building earshot-cli (release)");
    let out = run_with_timeout(&mut cmd, BUILD_TIMEOUT)
        .context("building earshot-cli did not finish within the deadline")?;
    if !out.success {
        bail!("building earshot-cli failed: {}", out.describe());
    }
    // The executable path comes from cargo's JSON on stdout.
    out.require_complete("building earshot-cli")?;
    let exe = cli_executable(&out.stdout).context("cargo reported no earshot executable")?;
    // <target dir>/release/earshot-cli.exe: its C/C++ was optimised (T2.2c).
    if let Some(profile) = exe.parent() {
        crate::native_flags::check_optimised(profile, "--release")?;
    }
    // It is spawned directly (not through cargo): llama.cpp's DLLs go beside it (T6.21b).
    crate::llama_dlls::install_beside(&out.stdout, &exe)?;
    Ok(exe)
}

/// The `earshot-cli` executable named in cargo''s JSON build messages.
#[must_use]
pub fn cli_executable(messages: &str) -> Option<PathBuf> {
    executable_named(messages, "earshot-cli")
}

/// The executable of target `name` in cargo''s JSON build messages.
#[must_use]
pub fn executable_named(messages: &str, name: &str) -> Option<PathBuf> {
    messages
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter(|m| m["reason"] == "compiler-artifact" && m["target"]["name"] == name)
        .find_map(|m| m["executable"].as_str().map(PathBuf::from))
}
/// Checks the printed audit log (`network: METHOD scheme://host:port/path status S (O)`):
/// at least one entry, `expected` entries, each a successful `POST http` to
/// `host:port/v1/chat/completions`.
#[must_use]
pub fn audit_check(stdout: &str, host: &str, port: u16, expected: usize) -> Check {
    let entries: Vec<&str> = stdout
        .lines()
        .filter_map(|l| l.strip_prefix("network: "))
        .collect();
    let want = format!("POST http://{host}:{port}/v1/chat/completions status Some(200) (");
    let bad: Vec<&&str> = entries.iter().filter(|e| !e.starts_with(&want)).collect();
    let pass = !entries.is_empty() && bad.is_empty() && entries.len() == expected;
    Check::new(
        "network",
        pass,
        format!(
            "{} audit entries, {expected} requests at the mock, {} not a successful POST to {host}:{port}/v1/chat/completions{}",
            entries.len(),
            bad.len(),
            bad.first()
                .map(|b| format!(" (first: {b})"))
                .unwrap_or_default()
        ),
    )
}

/// The checks on the request bodies the mock received.
#[must_use]
pub fn prompt_check(requests: &[RecordedRequest], me: &str, remote: &[String]) -> Check {
    let mut problems = Vec::new();
    for (i, r) in requests.iter().enumerate() {
        let body = r.json();
        if r.method != "POST" || r.path != "/v1/chat/completions" {
            problems.push(format!("request {i}: {} {}", r.method, r.path));
        }
        if r.body.len() > MAX_REQUEST_BYTES {
            problems.push(format!("request {i}: {} bytes", r.body.len()));
        }
        let audio_part = body["messages"].as_array().is_some_and(|ms| {
            ms.iter().any(|m| {
                m["content"]
                    .as_array()
                    .is_some_and(|parts| parts.iter().any(|p| p["type"] != "text"))
            })
        });
        if audio_part {
            problems.push(format!("request {i}: a non-text content part"));
        }
        let lines = prompt_lines(&user_prompt(&body));
        if !lines.iter().any(|l| l.speaker == me) {
            problems.push(format!("request {i}: no transcript line of {me}"));
        }
        if !lines.iter().any(|l| remote.contains(&l.speaker)) {
            problems.push(format!("request {i}: no transcript line of {remote:?}"));
        }
    }
    if requests.is_empty() {
        problems.push("no request".into());
    }
    Check::new(
        "prompt",
        problems.is_empty(),
        if problems.is_empty() {
            format!(
                "{} request(s): transcript lines of {me} and {remote:?}, text only",
                requests.len()
            )
        } else {
            problems.join("; ")
        },
    )
}

pub(super) fn write_mono(path: &Path, samples: &[f32]) -> Result<()> {
    let file =
        std::fs::File::create(path).with_context(|| format!("cannot create {}", path.display()))?;
    write_wav(
        std::io::BufWriter::new(file),
        PIPELINE_RATE,
        1,
        samples,
        WavFormat::Float32,
    )
    .with_context(|| format!("cannot write {}", path.display()))
}

fn walk(dir: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    for e in std::fs::read_dir(dir).with_context(|| format!("cannot list {}", dir.display()))? {
        let p = e?.path();
        if p.is_dir() {
            walk(&p, out)?;
        } else {
            out.push(p);
        }
    }
    Ok(())
}

pub(super) fn run(ctx: &Ctx) -> Result<RunOutput> {
    if let Some(b) = ctx.backend
        && b != "cpu"
    {
        bail!("accept m6 runs the default (CPU) earshot-cli build; --backend {b} is not supported");
    }
    let file = meeting_file(ctx.root)?;
    let sim = simulate_meeting(ctx.root, &file)?;
    let work = std::env::temp_dir().join(format!("earshot-accept-m6-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&work);
    let (data, vault) = (work.join("data"), work.join("vault"));
    std::fs::create_dir_all(&vault)
        .with_context(|| format!("cannot create {}", vault.display()))?;
    let (mic_wav, ref_wav) = (work.join("mic.wav"), work.join("loopback.wav"));
    write_mono(&mic_wav, &sim.mic)?;
    write_mono(&ref_wav, &sim.reference)?;

    let server = MockServer::start_with(respond).context("cannot start the LLM mock")?;
    let endpoint = server.endpoint();
    let host = endpoint.host_str().unwrap_or_default().to_owned();
    let port = endpoint.port().unwrap_or_default();
    ensure!(
        host == "127.0.0.1",
        "the mock must listen on loopback, not {host}"
    );

    // Build first, then run the executable itself, so the deadline kills earshot (not a
    // cargo parent that would leave it running).
    let exe = build_cli(ctx.root)?;
    let mut cmd = std::process::Command::new(&exe);
    cmd.arg("replay")
        .arg("--mic")
        .arg(&mic_wav)
        .arg("--ref")
        .arg(&ref_wav)
        .arg("--out")
        .arg(&data)
        .args([
            "--model",
            MODEL,
            "--llm-url",
            endpoint.as_str(),
            "--llm-model",
            LLM_MODEL,
        ])
        .arg("--export-vault")
        .arg(&vault)
        .env("EARSHOT_MODELS", crate::models::cache_dir()?)
        .current_dir(ctx.root);
    eprintln!(
        "accept m6: replaying a {:.0} s two-track meeting through {}",
        file.meeting.duration_s,
        exe.display()
    );
    let out = run_with_timeout(&mut cmd, RUN_TIMEOUT)
        .context("earshot-cli replay did not finish within the deadline")?;
    if !out.success {
        bail!(
            "earshot-cli replay failed (work directory {}):\n{}\n{}",
            work.display(),
            out.stdout,
            out.stderr
        );
    }
    // The checks read the printed backend, audit entries and transcript from stdout.
    out.require_complete("earshot-cli replay")?;
    let requests = server.requests();
    drop(server);

    let checks = inspect(
        ctx.root,
        &sim,
        &data,
        &vault,
        &out.stdout,
        printed_backends(&out.stdout),
        &requests,
        &host,
        port,
    )?;
    let passed = checks.iter().all(|c| c.pass);
    if passed {
        let _ = std::fs::remove_dir_all(&work);
    } else {
        eprintln!("accept m6: work directory kept at {}", work.display());
    }
    let mut m = Measurement::passed(ID, passed);
    m.backend = measured_backend(&out.stdout);
    m.detail = Some(format!("earshot-cli replay output:\n{}", out.stdout));
    Ok(RunOutput {
        measurements: vec![m],
        checks,
    })
}

/// Final-pass rows of the meeting (tombstones excluded).
fn final_segments(store: &Store, meeting: EntityId) -> Result<Vec<Segment>> {
    Ok(store
        .segments(meeting)?
        .into_iter()
        .filter(|r| !r.meta.deleted && r.pass == TranscriptPass::Final)
        .map(|r| r.segment)
        .collect())
}

/// What a replay left behind, checked (the module docs' list). `backends` are the kinds the host
/// printed: live mic, live loopback, final pass.
#[allow(clippy::too_many_arguments)]
pub(super) fn inspect(
    root: &Path,
    sim: &SimOutput,
    data: &Path,
    vault: &Path,
    stdout: &str,
    backends: Result<[String; 3]>,
    requests: &[RecordedRequest],
    host: &str,
    port: u16,
) -> Result<Vec<Check>> {
    let mut checks = Vec::new();
    let dir = DataDir::new(data);
    let mut store = Store::open(&dir.db_path()).context("the replay left no database")?;
    let meetings: Vec<_> = store
        .meetings()?
        .into_iter()
        .filter(|m| !m.meta.deleted)
        .collect();
    ensure!(
        meetings.len() == 1,
        "{} meetings stored, expected 1",
        meetings.len()
    );
    let meeting = &meetings[0];
    let tracks = store.tracks(meeting.id)?;
    let track_of = |kind: TrackId| tracks.iter().find(|t| t.kind == kind).map(|t| t.id);
    let (Some(mic), Some(reference)) = (track_of(TrackId::Mic), track_of(TrackId::Reference))
    else {
        bail!(
            "the meeting has tracks {:?}, expected mic and reference",
            tracks.len()
        );
    };

    // Audio files.
    let frames = sim.mic.len() as u64;
    let mut problems = Vec::new();
    let files = store.audio_files(meeting.id)?;
    for (kind, track) in [(TrackId::Mic, mic), (TrackId::Reference, reference)] {
        let Some(f) = files.iter().find(|f| f.track_id == track) else {
            problems.push(format!("{kind:?}: no audio file"));
            continue;
        };
        match dir
            .resolve(&f.path)
            .map_err(anyhow::Error::from)
            .and_then(|p| read_flac(&p).with_context(|| format!("cannot decode {}", p.display())))
        {
            Ok((rate, samples)) => {
                if rate != PIPELINE_RATE.hz()
                    || samples.len() as u64 != frames
                    || f.frames != frames
                {
                    problems.push(format!(
                        "{kind:?}: {rate} Hz, {} frames decoded, {} recorded, expected {frames}",
                        samples.len(),
                        f.frames
                    ));
                }
            }
            Err(e) => problems.push(format!("{kind:?}: {e:#}")),
        }
    }
    let duration_ms = frames * 1000 / u64::from(PIPELINE_RATE.hz());
    if meeting.duration_ms != Some(duration_ms) {
        problems.push(format!(
            "meeting duration {:?} ms, expected {duration_ms}",
            meeting.duration_ms
        ));
    }
    checks.push(Check::new(
        "audio_files",
        problems.is_empty() && files.len() == 2,
        if problems.is_empty() {
            format!("{} FLAC files of {frames} frames at 16 kHz", files.len())
        } else {
            problems.join("; ")
        },
    ));

    // Transcript and backends.
    let segments = final_segments(&store, meeting.id)?;
    let on = |track: EntityId| -> Vec<&Segment> {
        segments
            .iter()
            .filter(|s| s.track == track && !s.text.trim().is_empty())
            .collect()
    };
    let (me_segs, them_segs) = (on(mic), on(reference));
    let cpu = backends
        .as_ref()
        .is_ok_and(|b| b.iter().all(|k| k == "Cpu"));
    checks.push(Check::new(
        "transcript",
        !me_segs.is_empty() && !them_segs.is_empty() && cpu,
        format!(
            "final pass: {} mic and {} loopback segments with text; backends (live mic, live loopback, final) {:?}",
            me_segs.len(),
            them_segs.len(),
            backends.map_err(|e| e.to_string())
        ),
    ));

    // Speakers.
    let speakers = store.speakers(meeting.id)?;
    let role_of = |id: Option<EntityId>| {
        id.and_then(|id| speakers.iter().find(|s| s.id == id))
            .map(|s| (s.role, s.display_name.clone()))
    };
    let distinct = |segs: &[&Segment]| {
        let mut out: Vec<Option<(SpeakerRole, String)>> = Vec::new();
        for r in segs.iter().map(|s| role_of(s.speaker_id)) {
            if !out.contains(&r) {
                out.push(r);
            }
        }
        out
    };
    let mic_roles = distinct(&me_segs);
    let them_roles = distinct(&them_segs);
    let mic_ids: BTreeSet<_> = me_segs.iter().map(|s| s.speaker_id).collect();
    let them_ids: BTreeSet<_> = them_segs.iter().map(|s| s.speaker_id).collect();
    let me_only = mic_roles
        .iter()
        .all(|r| matches!(r, Some((SpeakerRole::Me, _))));
    let remote_only = them_roles
        .iter()
        .all(|r| matches!(r, Some((SpeakerRole::Remote, _))));
    let disjoint = mic_ids.is_disjoint(&them_ids);
    let me_name = mic_roles
        .iter()
        .flatten()
        .map(|(_, n)| n.clone())
        .next()
        .unwrap_or_default();
    let remote_names: Vec<String> = them_roles
        .iter()
        .flatten()
        .map(|(_, n)| n.clone())
        .collect();
    checks.push(Check::new(
        "speakers_separated",
        me_only && remote_only && disjoint && !mic_roles.is_empty() && !them_roles.is_empty(),
        format!(
            "mic speakers {mic_roles:?}; loopback speakers {them_roles:?}; shared: {}",
            !disjoint
        ),
    ));

    // Content.
    let (near_only, far_only) = exclusive_vocabularies(root)?;
    let text = |segs: &[&Segment]| {
        segs.iter()
            .map(|s| s.text.as_str())
            .collect::<Vec<_>>()
            .join(" ")
    };
    let (me_own, me_other) = vocabulary_hits(&text(&me_segs), &near_only, &far_only);
    let (them_own, them_other) = vocabulary_hits(&text(&them_segs), &far_only, &near_only);
    checks.push(Check::new(
        "content_separated",
        content_is_own(me_own, me_other) && content_is_own(them_own, them_other),
        format!(
            "talker-only words: Me transcript {me_own} near-end / {me_other} far-end; loopback transcript {them_own} far-end / {them_other} near-end"
        ),
    ));
    // The product requirement (the owner): far-end speech that leaks into the mic is never
    // attributed to "Me" (and the other way round): no word only the other talker uses.
    let me_leak = leaked_words(&text(&me_segs), &far_only);
    let them_leak = leaked_words(&text(&them_segs), &near_only);
    checks.push(Check::new(
        "echo_leak",
        me_leak.is_empty() && them_leak.is_empty(),
        format!(
            "far-end-only words in the Me transcript: {me_leak:?}; near-end-only words in the loopback transcript: {them_leak:?}"
        ),
    ));

    // Summary.
    let summaries: Vec<_> = store
        .summaries(meeting.id)?
        .into_iter()
        .filter(|s| !s.meta.deleted)
        .collect();
    let ids: BTreeSet<EntityId> = segments.iter().map(|s| s.id).collect();
    let summary_check = match summaries.as_slice() {
        [s] => {
            let errors = validate_summary(&s.summary, &ids);
            let cited = s
                .summary
                .decisions
                .iter()
                .flat_map(|d| &d.source_segment_ids)
                .chain(
                    s.summary
                        .action_items
                        .iter()
                        .flat_map(|a| &a.source_segment_ids),
                )
                .count();
            let titled = meeting.title == s.summary.title;
            Check::new(
                "summary",
                errors.is_empty() && cited > 0 && titled,
                format!(
                    "{} validation errors, {cited} citations, meeting title {:?}",
                    errors.len(),
                    meeting.title
                ),
            )
        }
        other => Check::new(
            "summary",
            false,
            format!("{} summaries stored", other.len()),
        ),
    };
    checks.push(summary_check);
    checks.push(prompt_check(requests, &me_name, &remote_names));

    // Export.
    let device = store.device_id(&ManualClock::new(0), &mut ChaCha8Rng::seed_from_u64(6))?;
    let mut notes = Vec::new();
    walk(vault, &mut notes)?;
    let export_check = match store.export_record(meeting.id, device)? {
        Some(record) => {
            let mut path = vault.to_path_buf();
            path.extend(record.path.components());
            let note = std::fs::read_to_string(&path).unwrap_or_default();
            let tldr = summaries
                .first()
                .map(|s| s.summary.tldr.clone())
                .unwrap_or_default();
            let mut missing = Vec::new();
            if earshot_id_of(&note) != Some(meeting.id) {
                missing.push("earshot_id".to_owned());
            }
            for needle in [BEGIN_MARKER, END_MARKER, tldr.as_str(), me_name.as_str()]
                .into_iter()
                .chain(remote_names.iter().map(String::as_str))
            {
                if needle.is_empty() || !note.contains(needle) {
                    missing.push(format!("{needle:?}"));
                }
            }
            Check::new(
                "export",
                notes.len() == 1 && notes[0] == path && missing.is_empty(),
                format!(
                    "{} file(s) in the vault; note {}; missing {missing:?}",
                    notes.len(),
                    record.path.as_str()
                ),
            )
        }
        None => Check::new("export", false, "no export record"),
    };
    checks.push(export_check);
    checks.push(audit_check(stdout, host, port, requests.len()));
    Ok(checks)
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: &str = "01890a5d-ac96-7000-0000-000000000001";
    const B: &str = "01890a5d-ac96-7000-0000-000000000002";
    const C: &str = "01890a5d-ac96-7000-0000-000000000003";

    fn prompt() -> String {
        format!(
            "Meeting\nTitle: x\n\nTranscript\n[1] 00:00:01 Me: hello there\n[2] 00:00:05 Speaker 1: good morning: all\n[3] 00:00:09 Me: bye\nnot a line\n[bad] 00:00:01 Me: x\n[0] 00:00:01 Me: x\n[{A}] 00:00:01 Me: an id is not a number\n"
        )
    }

    #[test]
    fn m6_prompt_lines_are_parsed_with_numbers_and_speakers() {
        let lines = prompt_lines(&prompt());
        let got: Vec<(u64, &str)> = lines
            .iter()
            .map(|l| (l.number, l.speaker.as_str()))
            .collect();
        assert_eq!(got, vec![(1, "Me"), (2, "Speaker 1"), (3, "Me")]);
    }

    #[test]
    fn m6_mock_summary_cites_the_prompt_and_validates() {
        let v = mock_summary(&prompt());
        let ids: Vec<EntityId> = [A, B, C].iter().map(|s| s.parse().unwrap()).collect();
        let numbers = earshot_core::summary::SegmentNumbers::new(ids.clone());
        let s = earshot_core::summary::validate_reply(&v, &numbers, &(0..3).into())
            .unwrap()
            .summary;
        assert!(validate_summary(&s, &ids.iter().copied().collect::<BTreeSet<_>>()).is_empty());
        assert_eq!(s.decisions[0].source_segment_ids[0].to_string(), A);
        assert_eq!(s.action_items[0].owner, "Speaker 1");
        assert_eq!(s.action_items[0].source_segment_ids[0].to_string(), B);
        // Citing segments outside what the request may cite fails validation.
        assert!(earshot_core::summary::validate_reply(&v, &numbers, &(2..3).into()).is_err());
        // No transcript lines: nothing cited.
        let empty: earshot_core::summary::SummaryV1 =
            serde_json::from_value(mock_summary("no transcript")).unwrap();
        assert!(empty.decisions.is_empty() && empty.action_items.is_empty());
    }

    #[test]
    fn m6_measured_backend_needs_all_three_lines_to_agree() {
        let line = |a: &str, b: &str, c: &str| {
            format!(
                "live asr mic: {a} (x)\nlive asr reference: {b} (x)\npost: final pass ran on {c} (x)\n"
            )
        };
        assert_eq!(
            measured_backend(&line("Cpu", "Cpu", "Cpu")).as_deref(),
            Some("cpu")
        );
        assert_eq!(
            measured_backend(&line("Cuda", "Cuda", "Cuda")).as_deref(),
            Some("cuda")
        );
        assert_eq!(measured_backend(&line("Cuda", "Cpu", "Cuda")), None);
        assert_eq!(measured_backend("live asr mic: Cpu (x)\n"), None);
    }

    #[test]
    fn m6_cli_executable_is_read_from_cargo_messages() {
        let msgs = concat!(
            r#"{"reason":"compiler-artifact","target":{"name":"earshot_core"},"executable":null}"#,
            "\n",
            r#"{"reason":"compiler-artifact","target":{"name":"earshot-cli"},"executable":"C:\\t\\release\\earshot-cli.exe"}"#,
            "\n",
            r#"{"reason":"build-finished","success":true}"#,
            "\n"
        );
        assert_eq!(
            cli_executable(msgs),
            Some(PathBuf::from("C:\\t\\release\\earshot-cli.exe"))
        );
        assert_eq!(cli_executable("not json\n"), None);
    }

    #[test]
    fn m6_leaked_words_are_the_other_talkers_words() {
        let far: BTreeSet<String> = ["wizard".to_owned(), "curtain".to_owned()].into();
        assert_eq!(leaked_words("the wizard went home", &far), vec!["wizard"]);
        assert!(leaked_words("nothing shared here", &far).is_empty());
    }

    #[test]
    fn m6_words_and_content_rule() {
        assert_eq!(
            words("It's the WIZARD's curtain, ok!"),
            vec!["it's", "wizard's", "curtain"]
        );
        let own: BTreeSet<String> = ["wizard".to_owned()].into();
        let other: BTreeSet<String> = ["curtain".to_owned()].into();
        assert_eq!(
            vocabulary_hits("wizard wizard curtain", &own, &other),
            (2, 1)
        );
        assert!(content_is_own(2, 1));
        assert!(!content_is_own(1, 1));
        assert!(!content_is_own(0, 0));
    }

    #[test]
    fn m6_exclusive_vocabularies_come_from_the_smoke_transcripts() {
        let (near, far) = exclusive_vocabularies(&crate::repo_root()).unwrap();
        // Each set holds words of its own talker's first committed transcript line (not
        // guaranteed by construction: a line could use only shared words).
        let text =
            std::fs::read_to_string(crate::repo_root().join("fixtures/smoke/transcripts.txt"))
                .unwrap();
        let line = |prefix: &str| {
            text.lines()
                .find(|l| l.starts_with(prefix))
                .unwrap()
                .to_owned()
        };
        let near_line = words(&line("121-"));
        let far_line = words(&line("61-"));
        assert!(near_line.iter().any(|w| near.contains(w)), "{near_line:?}");
        assert!(far_line.iter().any(|w| far.contains(w)), "{far_line:?}");
    }

    #[test]
    fn m6_backends_are_read_from_the_cli_lines() {
        let out = "live asr mic: Cpu (cpu, ggml-base.en)\nlive asr reference: Cuda (x)\npost: final pass ran on Cpu (cpu, m); 3 segments\n";
        assert_eq!(printed_backends(out).unwrap(), ["Cpu", "Cuda", "Cpu"]);
        assert!(printed_backends("live asr mic: Cpu (x)\n").is_err());
    }

    #[test]
    fn m6_audit_check_needs_every_entry_at_the_mock() {
        let ok = "network: POST http://127.0.0.1:5000/v1/chat/completions status Some(200) (Completed)\n";
        assert!(audit_check(ok, "127.0.0.1", 5000, 1).pass);
        assert!(
            !audit_check(ok, "127.0.0.1", 5000, 2).pass,
            "count mismatch"
        );
        assert!(!audit_check(ok, "127.0.0.1", 5001, 1).pass, "other port");
        assert!(!audit_check("", "127.0.0.1", 5000, 0).pass, "no entry");
        let other = format!(
            "{ok}network: POST http://10.0.0.2:5000/v1/chat/completions status Some(200) (Completed)\n"
        );
        assert!(
            !audit_check(&other, "127.0.0.1", 5000, 2).pass,
            "another host"
        );
        let failed = "network: POST http://127.0.0.1:5000/v1/chat/completions status Some(500) (Completed)\n";
        assert!(!audit_check(failed, "127.0.0.1", 5000, 1).pass);
    }

    fn request(body: Value) -> RecordedRequest {
        RecordedRequest {
            method: "POST".into(),
            path: "/v1/chat/completions".into(),
            headers: vec![],
            body: body.to_string().into_bytes(),
        }
    }

    #[test]
    fn m6_prompt_check_needs_both_speakers_and_text_only() {
        let good = request(json!({"messages": [
            {"role": "system", "content": "rules"},
            {"role": "user", "content": prompt()},
        ]}));
        let remote = vec!["Speaker 1".to_owned()];
        assert!(prompt_check(std::slice::from_ref(&good), "Me", &remote).pass);
        assert!(!prompt_check(&[], "Me", &remote).pass);
        assert!(!prompt_check(std::slice::from_ref(&good), "Me", &["Speaker 2".to_owned()]).pass);
        let audio = request(json!({"messages": [
            {"role": "user", "content": [
                {"type": "text", "text": prompt()},
                {"type": "input_audio", "input_audio": {"data": "AAAA", "format": "wav"}},
            ]},
        ]}));
        assert!(!prompt_check(&[audio], "Me", &remote).pass);
    }

    #[test]
    fn m6_meeting_file_parses_and_simulates() {
        let root = crate::repo_root();
        let file = meeting_file(&root).unwrap();
        assert!(file.meeting.near_speech && file.meeting.echo);
        assert!(!file.meeting.informational);
        let sim = simulate_meeting(&root, &file).unwrap();
        assert_eq!(
            sim.mic.len(),
            (file.meeting.duration_s * f64::from(PIPELINE_RATE.hz())) as usize
        );
        assert_eq!(sim.mic.len(), sim.reference.len());
        assert!(!sim.near_regions.is_empty() && !sim.far_regions.is_empty());
    }
}
