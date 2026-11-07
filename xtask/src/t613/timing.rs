//! `cargo xtask t613 timing`: how far behind the meeting the single pass lands (T6.13).
//!
//! The shipped CLI replays the LibriSpeech meeting (`meeting::librispeech`) at wall-clock
//! speed (`earshot replay --realtime --events-json`) with the production models: unless
//! `--model` is given, `earshot benchmark` runs first in the same data directory and the
//! replay uses the model check's answer, exactly as `record` does. From the event log:
//!
//! - **stretch delay**: for every live final (one per speech stretch, T6.13b) the wall time
//!   it was stored and shown minus the wall time its last word was delivered
//!   (`audio_origin + end`), per track and pooled (plan target: about 60 s);
//! - **end to final**: the wall time the cleanup stored the final transcript and speakers
//!   (`post_stage` `stored`) minus the wall time the meeting's audio ended (plan target:
//!   60 s); also when the session ended and the cleanup started;
//! - the backends the CLI printed (live mic, live loopback, final pass) and the model.
//!
//! The numbers are reported against the plan's targets, never gated here (there is no
//! PLAN 4.2 threshold for them). The JSON report goes to `target/t613/timing-<N>min.json`;
//! the work directory (`target/t613/timing-<N>min/`, data, event log and CLI output) is kept.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail, ensure};
use serde_json::{Value, json};

use super::{Build, cli_env, host_facts, parse_model_line, printed_backends, reuse_lines};
use crate::proc::run_with_timeout;

/// The plan's target for each stretch (s, "about 60 s").
pub const STRETCH_TARGET_S: f64 = 60.0;

/// The plan's target from the end of the meeting to the stored final transcript (s).
pub const FINAL_TARGET_S: f64 = 60.0;

/// The parsed event log.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Events(pub Vec<Value>);

impl Events {
    /// Parses JSON lines (empty lines skipped).
    ///
    /// # Errors
    /// A line that is not a JSON object with `t_ms` and `kind`.
    pub fn parse(text: &str) -> Result<Self> {
        let mut v = Vec::new();
        for (i, l) in text
            .lines()
            .enumerate()
            .filter(|(_, l)| !l.trim().is_empty())
        {
            let e: Value = serde_json::from_str(l)
                .with_context(|| format!("event log line {}: not JSON", i + 1))?;
            ensure!(
                e["t_ms"].is_number() && e["kind"].is_string(),
                "event log line {}: no t_ms or kind",
                i + 1
            );
            v.push(e);
        }
        Ok(Self(v))
    }

    fn of<'a>(&'a self, kind: &'a str) -> impl Iterator<Item = &'a Value> + 'a {
        self.0.iter().filter(move |e| e["kind"] == kind)
    }

    fn t(e: &Value) -> f64 {
        e["t_ms"].as_f64().unwrap_or(f64::NAN)
    }
}

/// Order statistics of a set of delays (s).
#[derive(Debug, Clone, PartialEq)]
pub struct Spread {
    /// How many.
    pub count: usize,
    /// Smallest.
    pub min: f64,
    /// Median (nearest rank).
    pub median: f64,
    /// 90th percentile (nearest rank).
    pub p90: f64,
    /// 99th percentile (nearest rank).
    pub p99: f64,
    /// Largest.
    pub max: f64,
    /// Mean.
    pub mean: f64,
    /// How many are over the plan's target.
    pub over_target: usize,
}

/// Nearest-rank percentile `p` (0-100) of sorted `x` (not empty).
#[must_use]
pub fn percentile(sorted: &[f64], p: f64) -> f64 {
    let rank = ((p / 100.0) * sorted.len() as f64).ceil() as usize;
    sorted[rank.clamp(1, sorted.len()) - 1]
}

/// The spread of `x` with `target`; `None` for no values.
#[must_use]
pub fn spread(x: &[f64], target: f64) -> Option<Spread> {
    if x.is_empty() {
        return None;
    }
    let mut s = x.to_vec();
    s.sort_by(f64::total_cmp);
    Some(Spread {
        count: s.len(),
        min: s[0],
        median: percentile(&s, 50.0),
        p90: percentile(&s, 90.0),
        p99: percentile(&s, 99.0),
        max: s[s.len() - 1],
        mean: s.iter().sum::<f64>() / s.len() as f64,
        over_target: s.iter().filter(|&&d| d > target).count(),
    })
}

fn spread_json(s: Option<&Spread>) -> Value {
    s.map_or(Value::Null, |s| {
        json!({
            "count": s.count, "min_s": s.min, "median_s": s.median, "p90_s": s.p90,
            "p99_s": s.p99, "max_s": s.max, "mean_s": s.mean, "over_target": s.over_target,
        })
    })
}

/// What the event log says about the single pass's timing.
#[derive(Debug, Clone, PartialEq)]
pub struct Timing {
    /// Meeting length (s), from the `ended` event.
    pub meeting_end_s: f64,
    /// Stretch delays (s) of the mic finals.
    pub mic: Vec<f64>,
    /// Stretch delays (s) of the loopback finals.
    pub reference: Vec<f64>,
    /// Finals stored before the session ended (during the meeting or while the workers
    /// finished their queues).
    pub finals_before_end: usize,
    /// Session ended (all stretches stored, recording finalised), after the audio ended (s).
    pub ended_after_end_s: f64,
    /// Cleanup started, after the audio ended (s).
    pub post_started_after_end_s: Option<f64>,
    /// Final transcript and speakers stored, after the audio ended (s).
    pub stored_after_end_s: Option<f64>,
    /// Each cleanup step: `(stage, track, elapsed s, finished after the audio ended s)`.
    pub stages: Vec<(String, Option<String>, f64, f64)>,
}

/// Times from `events` (module docs).
///
/// # Errors
/// No `audio_origin` (not a `--realtime` run) or no `ended`.
pub fn analyse(events: &Events) -> Result<Timing> {
    let origin = events
        .of("audio_origin")
        .next()
        .map(Events::t)
        .context("the event log has no audio_origin: the replay must run with --realtime")?;
    let ended = events
        .of("ended")
        .next()
        .context("the event log has no ended event: the session did not finish")?;
    let end_s = ended["end_ms"].as_f64().context("ended without end_ms")? / 1_000.0;
    // Wall time after the audio origin (s).
    let wall = |e: &Value| (Events::t(e) - origin) / 1_000.0;
    let after_end = |e: &Value| wall(e) - end_s;
    let (mut mic, mut reference) = (Vec::new(), Vec::new());
    let ended_t = Events::t(ended);
    let mut before = 0;
    for f in events.of("final") {
        let end = f["end_ms"].as_f64().context("final without end_ms")? / 1_000.0;
        let delay = wall(f) - end;
        match f["track"].as_str() {
            Some("mic") => mic.push(delay),
            Some("reference") => reference.push(delay),
            other => bail!("final on track {other:?}"),
        }
        if Events::t(f) <= ended_t {
            before += 1;
        }
    }
    let stages = events
        .of("post_stage")
        .map(|s| {
            (
                s["stage"].as_str().unwrap_or_default().to_owned(),
                s["track"].as_str().map(str::to_owned),
                s["elapsed_ms"].as_f64().unwrap_or(f64::NAN) / 1_000.0,
                after_end(s),
            )
        })
        .collect::<Vec<_>>();
    Ok(Timing {
        meeting_end_s: end_s,
        mic,
        reference,
        finals_before_end: before,
        ended_after_end_s: after_end(ended),
        post_started_after_end_s: events.of("post_start").next().map(after_end),
        stored_after_end_s: stages.iter().find(|s| s.0 == "stored").map(|s| s.3),
        stages,
    })
}

impl Timing {
    /// The report's `timing` object.
    #[must_use]
    pub fn to_json(&self) -> Value {
        let all: Vec<f64> = self.mic.iter().chain(&self.reference).copied().collect();
        json!({
            "meeting_end_s": self.meeting_end_s,
            "stretch_delay": {
                "target_s": STRETCH_TARGET_S,
                "all": spread_json(spread(&all, STRETCH_TARGET_S).as_ref()),
                "mic": spread_json(spread(&self.mic, STRETCH_TARGET_S).as_ref()),
                "reference": spread_json(spread(&self.reference, STRETCH_TARGET_S).as_ref()),
                "finals_before_session_end": self.finals_before_end,
            },
            "end_to_final": {
                "target_s": FINAL_TARGET_S,
                "session_ended_after_end_s": self.ended_after_end_s,
                "cleanup_started_after_end_s": self.post_started_after_end_s,
                "final_stored_after_end_s": self.stored_after_end_s,
                "within_target": self.stored_after_end_s.map(|s| s <= FINAL_TARGET_S),
                "stages": self.stages.iter().map(|(stage, track, elapsed, at)| json!({
                    "stage": stage, "track": track, "elapsed_s": elapsed, "finished_after_end_s": at,
                })).collect::<Vec<_>>(),
            },
        })
    }

    /// Short human summary.
    #[must_use]
    pub fn summary(&self) -> String {
        let all: Vec<f64> = self.mic.iter().chain(&self.reference).copied().collect();
        let mut s = format!("meeting {:.0} s", self.meeting_end_s);
        if let Some(d) = spread(&all, STRETCH_TARGET_S) {
            s += &format!(
                "; {} stretches: delay median {:.1} s, p90 {:.1} s, max {:.1} s, {} over {:.0} s",
                d.count, d.median, d.p90, d.max, d.over_target, STRETCH_TARGET_S
            );
        } else {
            s += "; no stretches";
        }
        s += &format!(
            "; session ended {:.1} s after the audio, final transcript and speakers stored {}",
            self.ended_after_end_s,
            self.stored_after_end_s
                .map_or("never".to_owned(), |t| format!("{t:.1} s after"))
        );
        s
    }
}

/// `cargo xtask t613 timing` options.
#[derive(Debug, Clone)]
pub struct TimingArgs {
    /// Meeting length (minutes).
    pub minutes: u32,
    /// Transcription model (default: the model check's answer).
    pub model: Option<String>,
    /// Build and backend.
    pub build: Build,
    /// Generate the meeting again.
    pub regenerate: bool,
}

/// Runs the timing replay (module docs).
///
/// # Errors
/// A failed build, benchmark or replay, or an incomplete event log.
pub fn run(root: &Path, args: &TimingArgs) -> Result<PathBuf> {
    let meeting = super::meeting::librispeech(root, args.minutes, args.regenerate)?;
    let exe = args.build.build(root)?;
    let work = root
        .join("target")
        .join("t613")
        .join(format!("timing-{}min", args.minutes));
    let _ = std::fs::remove_dir_all(&work);
    std::fs::create_dir_all(&work).with_context(|| format!("cannot create {}", work.display()))?;
    let data = work.join("data");
    let events_path = work.join("events.jsonl");

    let mut benchmark = None;
    if args.model.is_none() {
        let mut cmd = std::process::Command::new(&exe);
        cmd.arg("benchmark").arg("--out").arg(&data);
        cli_env(&mut cmd)?;
        eprintln!(
            "t613: earshot benchmark (the model check) into {}",
            data.display()
        );
        let out = run_with_timeout(&mut cmd, Duration::from_secs(30 * 60))
            .context("earshot benchmark did not finish within 30 minutes")?;
        if !out.success {
            bail!("earshot benchmark failed: {}", out.describe());
        }
        std::fs::write(work.join("benchmark.txt"), &out.stdout)?;
        benchmark = super::parse_recommendation(&out.stdout);
    }
    let mut cmd = std::process::Command::new(&exe);
    cmd.arg("replay")
        .arg("--mic")
        .arg(meeting.mic())
        .arg("--ref")
        .arg(meeting.loopback())
        .arg("--out")
        .arg(&data)
        .arg("--realtime")
        // T6.13 times the cleanup; a local summary (T6.21b) is not part of it.
        .arg("--no-summary")
        .arg("--events-json")
        .arg(&events_path);
    if let Some(m) = &args.model {
        cmd.args(["--model", m]);
    }
    cli_env(&mut cmd)?;
    let deadline = Duration::from_secs_f64(meeting.truth.duration_s * 1.5 + 30.0 * 60.0);
    eprintln!(
        "t613: replaying {} ({:.0} min) at real-time speed; results in {}",
        meeting.truth.name,
        meeting.truth.duration_s / 60.0,
        work.display()
    );
    let started = std::time::Instant::now();
    let out = run_with_timeout(&mut cmd, deadline)
        .context("earshot replay did not finish within its deadline")?;
    std::fs::write(work.join("replay.txt"), &out.stdout)?;
    std::fs::write(work.join("replay.err.txt"), &out.stderr)?;
    if !out.success {
        bail!("earshot replay failed (output in {})", work.display());
    }
    let events = Events::parse(
        &std::fs::read_to_string(&events_path)
            .with_context(|| format!("no event log at {}", events_path.display()))?,
    )?;
    let timing = analyse(&events)?;
    let backends = printed_backends(&out.stdout);
    let report = json!({
        "task": "T6.13",
        "kind": "timing",
        "meeting": {
            "name": meeting.truth.name,
            "source": meeting.truth.source,
            "duration_s": meeting.truth.duration_s,
            "mic_speech_s": meeting.truth.mic.speech_s(),
            "reference_speech_s": meeting.truth.reference.speech_s(),
            "dir": meeting.dir,
        },
        "host": host_facts(),
        "build": args.build.describe(),
        "model": parse_model_line(&out.stdout),
        "benchmark_recommendation": benchmark,
        "backends": { "live_mic": backends[0], "live_reference": backends[1], "final_pass": backends[2] },
        "realtime_lag": out.stdout.lines().find(|l| l.starts_with("realtime: ")),
        "cleanup_reuse": reuse_lines(&out.stdout),
        "wall_s": started.elapsed().as_secs_f64(),
        "timing": timing.to_json(),
        "work_dir": work,
    });
    let path = root
        .join("target")
        .join("t613")
        .join(format!("timing-{}min.json", args.minutes));
    std::fs::write(&path, serde_json::to_vec_pretty(&report)?)
        .with_context(|| format!("cannot write {}", path.display()))?;
    eprintln!("t613: {}", timing.summary());
    eprintln!(
        "t613: backends (live mic, live loopback, final pass) {:?}; report {}",
        backends,
        path.display()
    );
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn log(lines: &[Value]) -> Events {
        let text: String = lines.iter().map(|l| format!("{l}\n")).collect();
        Events::parse(&text).unwrap()
    }

    #[test]
    fn t613_timing_measures_each_stretch_from_its_end_and_the_final_from_the_audio_end() {
        // Audio starts at wall 1.0 s; a mic final ending at meeting 10 s is shown at wall
        // 25 s (14 s late); a loopback final ending at 50 s is shown at wall 52.5 s (1.5 s
        // late); the meeting ends at 60 s (wall 61 s), the session at wall 70 s and the
        // final transcript is stored at wall 100 s (39 s after the end).
        let ev = log(&[
            json!({"t_ms": 0.0, "kind": "started"}),
            json!({"t_ms": 1_000.0, "kind": "audio_origin"}),
            json!({"t_ms": 25_000.0, "kind": "final", "track": "mic", "start_ms": 3_000, "end_ms": 10_000, "words": 9}),
            json!({"t_ms": 52_500.0, "kind": "final", "track": "reference", "start_ms": 41_000, "end_ms": 50_000, "words": 20}),
            json!({"t_ms": 70_000.0, "kind": "ended", "end_ms": 60_000}),
            json!({"t_ms": 71_000.0, "kind": "post_start"}),
            json!({"t_ms": 90_000.0, "kind": "post_stage", "stage": "transcribed", "track": "mic", "elapsed_ms": 500.0}),
            json!({"t_ms": 100_000.0, "kind": "post_stage", "stage": "stored", "elapsed_ms": 250.0}),
        ]);
        let t = analyse(&ev).unwrap();
        assert_eq!(t.meeting_end_s, 60.0);
        assert_eq!(t.mic, vec![14.0]);
        assert_eq!(t.reference, vec![1.5]);
        assert_eq!(t.finals_before_end, 2);
        assert_eq!(t.ended_after_end_s, 9.0);
        assert_eq!(t.post_started_after_end_s, Some(10.0));
        assert_eq!(t.stored_after_end_s, Some(39.0));
        assert_eq!(t.stages.len(), 2);
        assert_eq!(t.stages[0].2, 0.5);
        let j = t.to_json();
        assert_eq!(j["end_to_final"]["within_target"], true);
        assert_eq!(j["stretch_delay"]["all"]["count"], 2);
        assert_eq!(j["stretch_delay"]["all"]["max_s"], 14.0);
    }

    #[test]
    fn t613_timing_needs_a_realtime_log_and_an_end() {
        let no_origin = log(&[json!({"t_ms": 1.0, "kind": "ended", "end_ms": 1})]);
        assert!(analyse(&no_origin).is_err());
        let no_end = log(&[json!({"t_ms": 1.0, "kind": "audio_origin"})]);
        assert!(analyse(&no_end).is_err());
        assert!(Events::parse("{\"kind\":\"x\"}").is_err(), "no t_ms");
        assert!(Events::parse("not json").is_err());
        // A cleanup that never stored anything says so.
        let t = analyse(&log(&[
            json!({"t_ms": 0.0, "kind": "audio_origin"}),
            json!({"t_ms": 5_000.0, "kind": "ended", "end_ms": 4_000}),
        ]))
        .unwrap();
        assert_eq!(t.stored_after_end_s, None);
        assert_eq!(t.to_json()["end_to_final"]["within_target"], Value::Null);
    }

    #[test]
    fn t613_timing_spread_uses_nearest_rank_and_counts_over_target() {
        let x: Vec<f64> = (1..=100).map(f64::from).collect();
        let s = spread(&x, 60.0).unwrap();
        assert_eq!(
            (s.count, s.min, s.median, s.p90, s.p99, s.max),
            (100, 1.0, 50.0, 90.0, 99.0, 100.0)
        );
        assert_eq!(s.over_target, 40);
        assert_eq!(spread(&[7.0], 60.0).unwrap().p99, 7.0);
        assert!(spread(&[], 60.0).is_none());
    }
}
