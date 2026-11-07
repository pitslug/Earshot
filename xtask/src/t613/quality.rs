//! `cargo xtask t613 quality`: the single pass against the whole-meeting pass (T6.13: "quality
//! checked against the whole-meeting pass on fixtures (AMI and the GATE-12 recordings)").
//!
//! Each input is replayed twice through the shipped CLI (as fast as it runs, same model on
//! both): `reuse`, the default cleanup that takes the live speech stretches and speaker
//! windows, and `whole`, `--whole-pass`, which transcribes and diarises every track whole
//! with the same models (the fallback). The replay is deterministic, so both runs record
//! the same audio; the report checks it (SHA-256 of each stored FLAC). Per mode and track:
//!
//! - with a reference (LibriSpeech, AMI): WER of the final transcript (Whisper English
//!   normalisation, one alignment per track) and DER of its speaker turns (NIST collar
//!   0.25 s per side), and the number of speakers found;
//! - always: between the two modes, the word differences of the final transcripts (the
//!   whole pass as the reference) and the DER of the reuse speakers against the whole
//!   pass's.
//!
//! With `--realtime` the replays run at wall-clock speed, so the live speaker windows are
//! computed and reused as in a meeting; as fast as possible, live speaker work falls more
//! than 180 s behind and stops, and the cleanup diarises whole in both modes.
//!
//! Inputs: `librispeech` (the LibriSpeech meeting of `--minutes`), `ami` (IS1009a and
//! ES2004a as calls), `gate12` (the private GATE-12 takes named in `.earshot-private/gate12.toml` in the home
//! folder, replayed with their device timelines; without that file the input is skipped with a
//! visible line). The GATE-12 recordings are private: they are read in place from `--private` (default `<repo>/fixtures/private`),
//! replayed into `target/t613/private-scratch/` (removed at the start and end of every run
//! and after each replay), and only counts reach the report, never transcript text. Word
//! differences are counted after Whisper's English normalisation
//! (`word_differences_normalised`); each reuse track also reports `reused_through_s` and
//! `reused_all` (the live stretches reached the end of the recording). Report: `target/t613/quality.json`
//! (`quality-realtime.json` with `--realtime`).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail, ensure};
use earshot_core::capture::TrackId;
use earshot_core::metrics::der::{AMI_COLLAR_PER_SIDE_S, DerConfig, Turn, der};
use earshot_core::metrics::wer::{WerCounts, word_errors_normalized};
use earshot_core::model::{AudioFileRepo, MeetingRepo, TranscriptPass};
use earshot_store::{DataDir, Store};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use super::meeting::{Meeting, TrackTruth};
use super::{Build, cli_env, host_facts, printed_backends, reuse_lines};
use crate::proc::run_with_timeout;

/// The two cleanups compared.
pub const MODES: [&str; 2] = ["reuse", "whole"];

/// One replay input.
#[derive(Debug, Clone)]
pub struct Input {
    /// Name in the report.
    pub name: String,
    /// Mic recording.
    pub mic: PathBuf,
    /// Loopback recording.
    pub reference: PathBuf,
    /// Device timeline (`--timeline`), for raw recordings.
    pub timeline: Option<PathBuf>,
    /// References, when the input has them.
    pub truth: Option<Meeting>,
    /// Private data: temporary data directory, counts only.
    pub private: bool,
}

/// The stored final transcript of one track: segments `(start s, end s, speaker, text)`
/// in time order.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Stored {
    /// Segments.
    pub segments: Vec<(f64, f64, String, String)>,
    /// SHA-256 of the track's FLAC.
    pub flac_sha256: Option<String>,
    /// Length of the track's recording (s).
    pub duration_s: Option<f64>,
}

impl Stored {
    /// The text, segments in time order.
    #[must_use]
    pub fn text(&self) -> String {
        self.segments
            .iter()
            .map(|s| s.3.as_str())
            .collect::<Vec<_>>()
            .join(" ")
    }

    /// Speaker turns.
    #[must_use]
    pub fn turns(&self) -> Vec<Turn> {
        self.segments
            .iter()
            .map(|(a, b, s, _)| Turn::new(*a, *b, s.clone()))
            .collect()
    }

    /// Distinct speakers.
    #[must_use]
    pub fn speakers(&self) -> usize {
        let mut s: Vec<&String> = self.segments.iter().map(|x| &x.2).collect();
        s.sort();
        s.dedup();
        s.len()
    }
}

/// Reads the only meeting's final transcript per track from data directory `data`.
///
/// # Errors
/// No database, not exactly one meeting, or unreadable files.
pub fn read_stored(data: &Path) -> Result<BTreeMap<&'static str, Stored>> {
    let dir = DataDir::new(data);
    let store = Store::open(&dir.db_path()).context("the replay left no database")?;
    let meetings: Vec<_> = store
        .meetings()?
        .into_iter()
        .filter(|m| !m.meta.deleted)
        .collect();
    ensure!(meetings.len() == 1, "{} meetings stored", meetings.len());
    let meeting = meetings[0].id;
    let tracks = store.tracks(meeting)?;
    let files = store.audio_files(meeting)?;
    let segments = store.segments(meeting)?;
    let mut out = BTreeMap::new();
    for (name, kind) in [("mic", TrackId::Mic), ("reference", TrackId::Reference)] {
        let Some(track) = tracks.iter().find(|t| t.kind == kind) else {
            continue;
        };
        let mut s = Stored::default();
        for r in &segments {
            if r.meta.deleted || r.pass != TranscriptPass::Final || r.segment.track != track.id {
                continue;
            }
            let seg = &r.segment;
            s.segments.push((
                seg.start.as_millis() as f64 / 1_000.0,
                seg.end.as_millis() as f64 / 1_000.0,
                seg.speaker_id.map_or("none".into(), |id| id.to_string()),
                seg.text.clone(),
            ));
        }
        s.segments.sort_by(|a, b| a.0.total_cmp(&b.0));
        if let Some(f) = files.iter().find(|f| f.track_id == track.id) {
            let path = dir.resolve(&f.path).map_err(anyhow::Error::from)?;
            let bytes =
                std::fs::read(&path).with_context(|| format!("cannot read {}", path.display()))?;
            s.flac_sha256 = Some(crate::download::hex(&Sha256::digest(&bytes)));
            s.duration_s = Some(f.frames as f64 / f64::from(f.sample_rate_hz.max(1)));
        }
        out.insert(name, s);
    }
    Ok(out)
}

fn counts_json(c: &WerCounts) -> Value {
    json!({
        "ref_words": c.ref_words,
        "substitutions": c.substitutions,
        "deletions": c.deletions,
        "insertions": c.insertions,
        "rate_percent": c.wer().map(|w| w * 100.0),
    })
}

fn nist() -> DerConfig {
    DerConfig::nist(AMI_COLLAR_PER_SIDE_S)
}

/// The per-track scores of one mode against `truth`.
#[must_use]
pub fn score_track(stored: &Stored, truth: Option<&TrackTruth>) -> Value {
    let mut v = json!({
        "segments": stored.segments.len(),
        "words": stored.text().split_whitespace().count(),
        "speakers": stored.speakers(),
    });
    if let Some(t) = truth {
        let wer = word_errors_normalized(&t.text(), &stored.text());
        let reference: Vec<Turn> = t
            .turns
            .iter()
            .map(|r| Turn::new(r.start_s, r.end_s, r.speaker.clone()))
            .collect();
        let d = der(&reference, &stored.turns(), &nist());
        v["wer"] = counts_json(&wer);
        v["der_percent"] = json!(d.der() * 100.0);
        let mut speakers: Vec<&str> = t.turns.iter().map(|r| r.speaker.as_str()).collect();
        speakers.sort_unstable();
        speakers.dedup();
        v["reference_speakers"] = json!(speakers.len());
    }
    v
}

/// Parses a CLI time (`MM:SS.mmm` or `H:MM:SS.mmm`, `transcribe::format_time`) to seconds.
#[must_use]
pub fn parse_cli_time(t: &str) -> Option<f64> {
    let mut secs = 0.0;
    for part in t.split(':') {
        secs = secs * 60.0 + part.parse::<f64>().ok()?;
    }
    Some(secs)
}

/// How far the cleanup took `track`'s live stretches, from its printed line `post: <track>:
/// took N live speech piece(s) up to <time> ...` (`None`: it transcribed the track whole).
#[must_use]
pub fn reused_through_s(stdout: &str, track: &str) -> Option<f64> {
    let prefix = format!("post: {track}: took ");
    stdout.lines().find_map(|l| {
        let rest = l.strip_prefix(&prefix)?;
        let (_, after) = rest.split_once(" live speech piece(s) up to ")?;
        parse_cli_time(after.split_whitespace().next()?.trim_end_matches(','))
    })
}

/// The reuse fields of one track: how far the stretches reached and whether that is the
/// whole recording (within 10 ms).
#[must_use]
pub fn reuse_json(stdout: &str, track: &str, duration_s: Option<f64>) -> Value {
    let through = reused_through_s(stdout, track);
    json!({
        "reused_through_s": through,
        "reused_all": matches!((through, duration_s), (Some(t), Some(d)) if t >= d - 0.01),
    })
}

/// The two modes of one track compared: word differences (whole pass as the reference) and
/// the DER of the reuse speakers against the whole pass's.
#[must_use]
pub fn agreement(reuse: &Stored, whole: &Stored) -> Value {
    let words = word_errors_normalized(&whole.text(), &reuse.text());
    let d = der(&whole.turns(), &reuse.turns(), &nist());
    json!({
        "word_differences_normalised": counts_json(&words),
        "speaker_der_percent": if whole.segments.is_empty() { Value::Null } else { json!(d.der() * 100.0) },
        "same_recording": reuse.flac_sha256.is_some() && reuse.flac_sha256 == whole.flac_sha256,
    })
}

/// `cargo xtask t613 quality` options.
#[derive(Debug, Clone)]
pub struct QualityArgs {
    /// Inputs: librispeech, ami, gate12.
    pub inputs: Vec<String>,
    /// Length of the LibriSpeech meeting (minutes).
    pub minutes: u32,
    /// Transcription model (default: the model check's answer).
    pub model: Option<String>,
    /// Build and backend.
    pub build: Build,
    /// The private fixtures directory (GATE-12).
    pub private: Option<PathBuf>,
    /// Replay at wall-clock speed: live speaker work runs as in a meeting and its windows
    /// are reused (as fast as possible it falls more than 180 s behind and stops, so the
    /// cleanup diarises whole in both modes).
    pub realtime: bool,
}

/// The inputs named in `args`.
///
/// # Errors
/// An unknown input, missing fixtures or private recordings.
pub fn inputs(root: &Path, args: &QualityArgs) -> Result<Vec<Input>> {
    let mut out = Vec::new();
    let from = |m: Meeting, name: String| Input {
        name,
        mic: m.mic(),
        reference: m.loopback(),
        timeline: None,
        truth: Some(m),
        private: false,
    };
    for i in &args.inputs {
        match i.as_str() {
            "librispeech" => {
                let m = super::meeting::librispeech(root, args.minutes, false)?;
                out.push(from(m.clone(), m.truth.name.clone()));
            }
            "ami" => {
                for meeting in crate::fixtures::AMI_MEETINGS {
                    let m = super::meeting::ami(root, meeting, false)?;
                    out.push(from(m.clone(), m.truth.name.clone()));
                }
            }
            "gate12" => {
                let path = super::gate12_private::default_path();
                let settings =
                    super::gate12_private::load(path.as_deref()).map_err(anyhow::Error::msg)?;
                let Some(settings) = settings else {
                    println!(
                        "t613 quality: gate12 SKIPPED: its private settings (.earshot-private/gate12.toml in the home folder) are not here"
                    );
                    continue;
                };
                let dir = args
                    .private
                    .clone()
                    .unwrap_or_else(|| root.join("fixtures").join("private"));
                for take in &settings.takes {
                    let d = dir.join(&take.dir);
                    for f in ["mic.wav", "loopback.wav", "timestamps.json"] {
                        ensure!(
                            d.join(f).is_file(),
                            "{} is missing (GATE-12 recordings, private; pass --private <dir>)",
                            d.join(f).display()
                        );
                    }
                    out.push(Input {
                        name: take.name.clone(),
                        mic: d.join("mic.wav"),
                        reference: d.join("loopback.wav"),
                        timeline: Some(d.join("timestamps.json")),
                        truth: None,
                        private: true,
                    });
                }
            }
            other => bail!("unknown input {other} (librispeech, ami, gate12)"),
        }
    }
    Ok(out)
}

/// One replay of `input` in `mode` into `data`; returns the CLI's stdout.
fn replay(
    exe: &Path,
    input: &Input,
    model: &str,
    mode: &str,
    realtime: bool,
    data: &Path,
) -> Result<String> {
    let _ = std::fs::remove_dir_all(data);
    let mut cmd = std::process::Command::new(exe);
    cmd.arg("replay")
        .arg("--mic")
        .arg(&input.mic)
        .arg("--ref")
        .arg(&input.reference)
        .arg("--out")
        .arg(data)
        .args(["--model", model])
        // T6.13 compares transcripts; no local summary (T6.21b).
        .arg("--no-summary");
    if let Some(t) = &input.timeline {
        cmd.arg("--timeline").arg(t);
    }
    if mode == "whole" {
        cmd.arg("--whole-pass");
    }
    if realtime {
        cmd.arg("--realtime");
    }
    cli_env(&mut cmd)?;
    eprintln!("t613: quality: {} ({mode})", input.name);
    let out = run_with_timeout(&mut cmd, Duration::from_secs(4 * 60 * 60))
        .context("earshot-cli replay did not finish within 4 hours")?;
    if !out.success {
        // Private runs never print the CLI output (it holds the transcript).
        if input.private {
            bail!(
                "earshot-cli replay of {} ({mode}) failed: {} (output withheld: private recording)",
                input.name,
                out.status_text()
            );
        }
        bail!(
            "earshot-cli replay of {} ({mode}) failed: {}",
            input.name,
            out.describe()
        );
    }
    Ok(out.stdout)
}

/// Removes a directory on drop (the private runs' data).
struct Scratch(PathBuf);

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Runs the comparison (module docs).
///
/// # Errors
/// A failed build or replay, or an unreadable result.
pub fn run(root: &Path, args: &QualityArgs) -> Result<PathBuf> {
    let inputs = inputs(root, args)?;
    let exe = args.build.build(root)?;
    let work = root.join("target").join("t613").join("quality");
    std::fs::create_dir_all(&work).with_context(|| format!("cannot create {}", work.display()))?;
    let model = match &args.model {
        Some(m) => m.clone(),
        None => {
            let data = work.join("benchmark");
            let _ = std::fs::remove_dir_all(&data);
            let mut cmd = std::process::Command::new(&exe);
            cmd.arg("benchmark").arg("--out").arg(&data);
            cli_env(&mut cmd)?;
            let out = run_with_timeout(&mut cmd, Duration::from_secs(30 * 60))
                .context("earshot-cli benchmark did not finish within 30 minutes")?;
            if !out.success {
                bail!("earshot-cli benchmark failed: {}", out.describe());
            }
            super::parse_recommendation(&out.stdout)
                .context("earshot-cli benchmark recommended no model")?
        }
    };
    // Private runs' data (recordings and transcripts of the GATE-12 takes) lives only here:
    // swept at the start (a crashed run's leftovers), per run, and at the end.
    let private_scratch = root.join("target").join("t613").join("private-scratch");
    let _ = std::fs::remove_dir_all(&private_scratch);
    let _sweep = Scratch(private_scratch.clone());
    let mut results = Vec::new();
    for input in &inputs {
        let mut stored = BTreeMap::new();
        let mut runs = serde_json::Map::new();
        for mode in MODES {
            let (data, _scratch) = if input.private {
                let d = private_scratch.join(format!("{}-{mode}", input.name));
                (d.clone(), Some(Scratch(d)))
            } else {
                (work.join(format!("{}-{mode}", input.name)), None)
            };
            let stdout = replay(&exe, input, &model, mode, args.realtime, &data)?;
            if !input.private {
                std::fs::write(work.join(format!("{}-{mode}.txt", input.name)), &stdout)?;
            }
            let s = read_stored(&data)?;
            let backends = printed_backends(&stdout);
            let mut tracks = serde_json::Map::new();
            for (name, st) in &s {
                let truth = input.truth.as_ref().map(|m| {
                    if *name == "mic" {
                        &m.truth.mic
                    } else {
                        &m.truth.reference
                    }
                });
                let mut v = score_track(st, truth);
                if let (Value::Object(o), Value::Object(r)) =
                    (&mut v, reuse_json(&stdout, name, st.duration_s))
                {
                    o.extend(r);
                }
                tracks.insert((*name).to_owned(), v);
            }
            runs.insert(
                mode.to_owned(),
                json!({
                    "backends": { "live_mic": backends[0], "live_reference": backends[1], "final_pass": backends[2] },
                    "cleanup_reuse": reuse_lines(&stdout),
                    "tracks": tracks,
                }),
            );
            stored.insert(mode, s);
        }
        let mut agree = serde_json::Map::new();
        for track in ["mic", "reference"] {
            if let (Some(r), Some(w)) = (stored["reuse"].get(track), stored["whole"].get(track)) {
                agree.insert(track.to_owned(), agreement(r, w));
            }
        }
        let entry = json!({
            "input": input.name,
            "private": input.private,
            "source": input.truth.as_ref().map(|m| m.truth.source.clone()),
            "duration_s": input.truth.as_ref().map(|m| m.truth.duration_s),
            "modes": runs,
            "reuse_vs_whole": agree,
        });
        eprintln!("t613: quality: {}: {}", input.name, summary(&entry));
        results.push(entry);
    }
    let report = json!({
        "task": "T6.13",
        "kind": "quality",
        "host": host_facts(),
        "build": args.build.describe(),
        "model": model,
        "realtime": args.realtime,
        "inputs": results,
    });
    let path = root.join("target").join("t613").join(if args.realtime {
        "quality-realtime.json"
    } else {
        "quality.json"
    });
    std::fs::write(&path, serde_json::to_vec_pretty(&report)?)
        .with_context(|| format!("cannot write {}", path.display()))?;
    eprintln!("t613: quality report {}", path.display());
    Ok(path)
}

/// One line per input: WER/DER per mode and track where scored, and the differences.
#[must_use]
pub fn summary(entry: &Value) -> String {
    let mut parts = Vec::new();
    for track in ["mic", "reference"] {
        let mut p = format!("{track}:");
        for mode in MODES {
            let t = &entry["modes"][mode]["tracks"][track];
            if t.is_null() {
                continue;
            }
            let wer = t["wer"]["rate_percent"]
                .as_f64()
                .map_or(String::new(), |w| format!(" WER {w:.2}%"));
            let der = t["der_percent"]
                .as_f64()
                .map_or(String::new(), |d| format!(" DER {d:.2}%"));
            let reuse = match (t["reused_through_s"].as_f64(), t["reused_all"].as_bool()) {
                (Some(through), Some(true)) => format!(" stretches to the end ({through:.1} s)"),
                (Some(through), _) => format!(" stretches only to {through:.1} s"),
                (None, _) => String::new(),
            };
            p += &format!(" {mode}{wer}{der} {} speakers{reuse};", t["speakers"]);
        }
        let a = &entry["reuse_vs_whole"][track];
        if !a.is_null() {
            let w = &a["word_differences_normalised"];
            p += &format!(
                " differ {} of {} words (S {} D {} I {}), speaker DER {:.2}%, same recording {}",
                w["substitutions"].as_u64().unwrap_or(0)
                    + w["deletions"].as_u64().unwrap_or(0)
                    + w["insertions"].as_u64().unwrap_or(0),
                w["ref_words"],
                w["substitutions"],
                w["deletions"],
                w["insertions"],
                a["speaker_der_percent"].as_f64().unwrap_or(f64::NAN),
                a["same_recording"]
            );
        }
        parts.push(p);
    }
    parts.join(" | ")
}

#[cfg(test)]
mod tests {
    use super::super::meeting::{RefTurn, RefWord};
    use super::*;

    fn stored(segs: &[(f64, f64, &str, &str)], sha: &str) -> Stored {
        Stored {
            segments: segs
                .iter()
                .map(|&(a, b, s, t)| (a, b, s.to_owned(), t.to_owned()))
                .collect(),
            flac_sha256: Some(sha.into()),
            duration_s: None,
        }
    }

    #[test]
    fn t613_quality_scores_wer_and_der_against_the_reference() {
        let truth = TrackTruth {
            turns: vec![
                RefTurn {
                    speaker: "a".into(),
                    start_s: 0.0,
                    end_s: 4.0,
                },
                RefTurn {
                    speaker: "b".into(),
                    start_s: 5.0,
                    end_s: 9.0,
                },
            ],
            words: ["hello", "there", "general", "kenobi"]
                .iter()
                .enumerate()
                .map(|(i, w)| RefWord {
                    start_s: if i < 2 { 0.0 } else { 5.0 },
                    text: (*w).into(),
                })
                .collect(),
        };
        // Perfect words and turns (other speaker labels).
        let good = stored(
            &[
                (0.0, 4.0, "x", "Hello there."),
                (5.0, 9.0, "y", "General Kenobi!"),
            ],
            "1",
        );
        let v = score_track(&good, Some(&truth));
        assert_eq!(v["wer"]["rate_percent"], 0.0);
        assert!(v["der_percent"].as_f64().unwrap() < 1e-9);
        assert_eq!(v["speakers"], 2);
        assert_eq!(v["reference_speakers"], 2);
        // One word wrong, both turns one speaker.
        let bad = stored(
            &[
                (0.0, 4.0, "x", "hello where"),
                (5.0, 9.0, "x", "general kenobi"),
            ],
            "1",
        );
        let v = score_track(&bad, Some(&truth));
        assert_eq!(v["wer"]["substitutions"], 1);
        assert_eq!(v["wer"]["rate_percent"], 25.0);
        assert!(v["der_percent"].as_f64().unwrap() > 30.0);
        // Without a reference only counts.
        let v = score_track(&bad, None);
        assert!(v.get("wer").is_none() && v["words"] == 4);
    }

    #[test]
    fn t613_quality_reports_how_far_the_live_stretches_were_reused() {
        let out = "post: mic: took 23 live speech piece(s) up to 12:30.500 (decoded on Cuda (cuda:0, x)), transcribed only the rest\n\
            post: reference: took 65 live speech piece(s) up to 1:02:03.500, transcribed only the rest\n\
            post: mic: took 32 speaker window(s) from the meeting, computed only the rest\n";
        assert_eq!(reused_through_s(out, "mic"), Some(750.5));
        assert_eq!(reused_through_s(out, "reference"), Some(3_723.5));
        assert_eq!(parse_cli_time("00:05.250"), Some(5.25));
        let all = reuse_json(out, "mic", Some(750.499_687_5));
        assert_eq!(all["reused_all"], true);
        assert_eq!(
            reuse_json(out, "reference", Some(3_800.0))["reused_all"],
            false
        );
        let whole =
            "post: mic: transcribed the whole track (the whole-meeting pass was requested)\n";
        let v = reuse_json(whole, "mic", Some(10.0));
        assert_eq!(v["reused_through_s"], Value::Null);
        assert_eq!(v["reused_all"], false);
        let line = summary(&json!({
            "modes": {"reuse": {"tracks": {"mic": {"speakers": 1, "reused_through_s": 750.5, "reused_all": true}}},
                      "whole": {"tracks": {"mic": {"speakers": 1, "reused_through_s": null, "reused_all": false}}}},
            "reuse_vs_whole": {},
        }));
        assert!(line.contains("stretches to the end (750.5 s)"), "{line}");
    }

    #[test]
    fn t613_quality_agreement_counts_differences_and_checks_the_recording() {
        let whole = stored(
            &[
                (0.0, 2.0, "s1", "alpha bravo charlie"),
                (3.0, 5.0, "s2", "delta"),
            ],
            "abc",
        );
        let same = stored(
            &[
                (0.0, 2.0, "t1", "alpha bravo charlie"),
                (3.0, 5.0, "t2", "delta"),
            ],
            "abc",
        );
        let a = agreement(&same, &whole);
        assert_eq!(a["word_differences_normalised"]["rate_percent"], 0.0);
        assert!(a["speaker_der_percent"].as_f64().unwrap() < 1e-9);
        assert_eq!(a["same_recording"], true);
        let other = stored(
            &[
                (0.0, 2.0, "t1", "alpha charlie"),
                (3.0, 5.0, "t1", "delta echo"),
            ],
            "abd",
        );
        let a = agreement(&other, &whole);
        assert_eq!(a["word_differences_normalised"]["deletions"], 1);
        assert_eq!(a["word_differences_normalised"]["insertions"], 1);
        assert!(a["speaker_der_percent"].as_f64().unwrap() > 0.0);
        assert_eq!(a["same_recording"], false);
        let line = summary(&json!({
            "modes": {"reuse": {"tracks": {"mic": {"speakers": 1}}}, "whole": {"tracks": {"mic": {"speakers": 1}}}},
            "reuse_vs_whole": {"mic": a},
        }));
        assert!(line.contains("differ 2 of 4 words"), "{line}");
    }
}
