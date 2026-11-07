//! `cargo xtask accept m<N>`: milestone acceptance framework (PLAN.md section 4).
//!
//! A runner per milestone produces measurements and structural checks. The framework
//! evaluates every threshold of the milestone from `acceptance/thresholds.toml` against
//! the measurement with the same id, writes `target/acceptance/m<N>.json` and a summary
//! in `acceptance/results/m<N>.json`, and fails on any miss, any missing metric, or any
//! measurement that has no threshold. Thresholds are never hardcoded or adjusted here.

mod m0;
pub mod m1;
pub mod m2;
pub mod m3;
mod m4;
mod m5;
pub mod m6;
pub mod m7;

use std::collections::{BTreeSet, HashMap};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::proc::run_with_timeout;
use crate::thresholds::{self, MILESTONES, Op, Threshold};

/// Tolerance for `==` on non-count units (floating point noise only).
pub const EQ_EPSILON: f64 = 1e-9;

/// One value measured by a runner.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Measurement {
    /// Threshold id this measurement answers.
    pub id: String,
    /// Numeric value (for `>=`, `<=`, `==`).
    pub value: Option<f64>,
    /// Boolean outcome (for `pass`).
    pub passed: Option<bool>,
    /// Unit of `value`; must equal the threshold's unit (`""` for `pass`).
    pub unit: String,
    /// Backend that produced it (cpu, cuda, vulkan, qnn).
    pub backend: Option<String>,
    /// Free-form detail kept only in the full report.
    pub detail: Option<String>,
}

impl Measurement {
    /// A numeric measurement in `unit` (one of the units in `thresholds::UNITS`).
    pub fn value(id: &str, value: f64, unit: &str) -> Self {
        Self {
            id: id.into(),
            value: Some(value),
            unit: unit.into(),
            ..Self::default()
        }
    }

    /// A boolean measurement for a `pass` threshold (unit `""`).
    pub fn passed(id: &str, passed: bool) -> Self {
        Self {
            id: id.into(),
            passed: Some(passed),
            ..Self::default()
        }
    }
}

/// A structural check reported by a runner (not tied to a threshold).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Check {
    /// Short name.
    pub name: String,
    /// Whether it passed.
    pub pass: bool,
    /// Explanation (kept in the summary too, it is short by convention).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

impl Check {
    /// Builds a check.
    pub fn new(name: &str, pass: bool, detail: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            pass,
            detail: Some(detail.into()),
        }
    }
}

/// What a runner returns.
#[derive(Debug, Clone, Default)]
pub struct RunOutput {
    /// Measurements, one per threshold id of the milestone.
    pub measurements: Vec<Measurement>,
    /// Extra structural checks.
    pub checks: Vec<Check>,
}

/// Inputs handed to a runner.
#[derive(Debug)]
pub struct Ctx<'a> {
    /// Repository root.
    pub root: &'a Path,
    /// Milestone being accepted (`m0`..`m11`).
    pub milestone: &'a str,
    /// `--backend` value, if given.
    pub backend: Option<&'a str>,
}

/// A milestone runner.
pub type Runner = fn(&Ctx) -> Result<RunOutput>;

struct Entry {
    milestone: &'static str,
    task: &'static str,
    runner: Option<Runner>,
}

/// Runner registry keyed by milestone. A `None` runner is filled in by the named task.
const REGISTRY: &[Entry] = &[
    Entry {
        milestone: "m0",
        task: "T0.2",
        runner: Some(m0::run),
    },
    Entry {
        milestone: "m1",
        task: "T1.6",
        runner: Some(m1::run),
    },
    Entry {
        milestone: "m2",
        task: "T2.5",
        runner: Some(m2::run),
    },
    Entry {
        milestone: "m3",
        task: "T3.5",
        runner: Some(m3::run),
    },
    Entry {
        milestone: "m4",
        task: "T4.5",
        runner: Some(m4::run),
    },
    Entry {
        milestone: "m5",
        task: "T5.5",
        runner: Some(m5::run),
    },
    Entry {
        milestone: "m6",
        task: "T6.6",
        runner: Some(m6::run),
    },
    Entry {
        milestone: "m7",
        task: "T7.6",
        runner: Some(m7::run),
    },
    Entry {
        milestone: "m8",
        task: "none planned in PLAN.md 8",
        runner: None,
    },
    Entry {
        milestone: "m9",
        task: "none planned in PLAN.md 8",
        runner: None,
    },
    Entry {
        milestone: "m10",
        task: "T10.6",
        runner: None,
    },
    Entry {
        milestone: "m11",
        task: "T11.4",
        runner: None,
    },
];

/// Looks up the runner for a milestone.
pub fn runner_for(milestone: &str) -> Result<Runner> {
    let Some(entry) = REGISTRY.iter().find(|e| e.milestone == milestone) else {
        bail!(
            "accept: unknown milestone {milestone:?} (expected one of {})",
            MILESTONES.join(", ")
        );
    };
    match entry.runner {
        Some(r) => Ok(r),
        None => bail!(
            "accept {milestone}: runner not implemented (task {})",
            entry.task
        ),
    }
}

/// One evaluated threshold (or an orphan measurement).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ResultRow {
    /// Threshold id.
    pub id: String,
    /// Metric description from the thresholds file (empty for orphan measurements).
    pub metric: String,
    /// Operator string (empty for orphan measurements).
    pub op: String,
    /// Threshold value (`None` for `pass`).
    pub threshold: Option<f64>,
    /// Unit.
    pub unit: String,
    /// Measured value.
    pub value: Option<f64>,
    /// Measured boolean (for `pass`).
    pub passed: Option<bool>,
    /// Backend that produced the measurement.
    pub backend: Option<String>,
    /// Whether this row passes.
    pub pass: bool,
    /// Why it passed or failed.
    pub reason: String,
    /// Runner detail (full report only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// Evaluates the milestone's thresholds against the measurements. Every threshold yields
/// a row; a threshold with no measurement fails as a missing metric; every measurement
/// without a matching threshold yields a failing row too.
pub fn evaluate(selected: &[&Threshold], measurements: &[Measurement]) -> Vec<ResultRow> {
    let mut by_id: HashMap<&str, Vec<&Measurement>> = HashMap::new();
    for m in measurements {
        by_id.entry(m.id.as_str()).or_default().push(m);
    }

    let mut rows = Vec::new();
    for t in selected {
        let found = by_id.get(t.id.as_str()).map(Vec::as_slice).unwrap_or(&[]);
        let m = found.first().copied();
        let (pass, reason) = match found.len() {
            0 => (false, "missing metric".to_string()),
            1 => judge(t, found[0]),
            n => (false, format!("duplicate metric ({n} measurements)")),
        };
        rows.push(ResultRow {
            id: t.id.clone(),
            metric: t.metric.clone(),
            op: t.op.as_str().into(),
            threshold: t.value,
            unit: t.unit.clone(),
            value: m.and_then(|m| m.value),
            passed: m.and_then(|m| m.passed),
            backend: m.and_then(|m| m.backend.clone()),
            pass,
            reason,
            detail: m.and_then(|m| m.detail.clone()),
        });
    }

    for m in measurements {
        if !selected.iter().any(|t| t.id == m.id) {
            rows.push(ResultRow {
                id: m.id.clone(),
                metric: String::new(),
                op: String::new(),
                threshold: None,
                unit: String::new(),
                value: m.value,
                passed: m.passed,
                backend: m.backend.clone(),
                pass: false,
                reason: "no threshold for this metric in this milestone".into(),
                detail: m.detail.clone(),
            });
        }
    }
    rows
}

fn unit_mismatch(t: &Threshold, m: &Measurement) -> Option<String> {
    (m.unit != t.unit).then(|| format!("unit mismatch: measured {}, threshold {}", m.unit, t.unit))
}

fn judge(t: &Threshold, m: &Measurement) -> (bool, String) {
    if t.op == Op::Pass {
        // `pass` threshold: a boolean, no value, and unit "".
        let b = match (m.passed, m.value) {
            (Some(b), None) => b,
            _ => {
                return (
                    false,
                    "malformed measurement: op pass needs a boolean and no value".into(),
                );
            }
        };
        if let Some(why) = unit_mismatch(t, m) {
            return (false, why);
        }
        return (b, if b { "passed" } else { "check failed" }.into());
    }
    let Some(limit) = t.value else {
        return (false, "malformed threshold: no value".into());
    };
    let v = match (m.value, m.passed) {
        (Some(v), None) => v,
        _ => {
            return (
                false,
                format!(
                    "malformed measurement: op {} needs a value and no boolean",
                    t.op.as_str()
                ),
            );
        }
    };
    if let Some(why) = unit_mismatch(t, m) {
        return (false, why);
    }
    if !v.is_finite() {
        return (false, format!("value {v} is not finite"));
    }
    let ok = match t.op {
        Op::Ge => v >= limit,
        Op::Le => v <= limit,
        Op::Eq if t.unit == "count" => v == limit,
        Op::Eq => (v - limit).abs() <= EQ_EPSILON,
        Op::Pass => unreachable!("handled above"),
    };
    let reason = format!(
        "{v} {} {limit}{}",
        t.op.as_str(),
        if ok { "" } else { " not met" }
    );
    (ok, reason)
}

/// Full acceptance report (`target/acceptance/m<N>.json`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Report {
    /// Milestone (`m0`..`m11`).
    pub milestone: String,
    /// `--backend` as given on the command line (never changes which thresholds apply).
    pub requested_backend: Option<String>,
    /// UTC ISO 8601 time of the run.
    pub timestamp: String,
    /// Host target triple.
    pub host: String,
    /// Short commit hash, with `-dirty` when the tree had changes outside
    /// `acceptance/results`, or `-unknown` when that could not be determined.
    pub commit: String,
    /// Distinct backends that produced measurements.
    pub backends: Vec<String>,
    /// Threshold rows.
    pub results: Vec<ResultRow>,
    /// Structural checks.
    pub checks: Vec<Check>,
    /// Overall result.
    pub pass: bool,
}

/// Assembles a report. Overall pass requires at least one row or check and every row
/// and check to pass (an empty run proves nothing, so it fails).
pub fn build_report(
    milestone: &str,
    timestamp: String,
    host: String,
    commit: String,
    requested_backend: Option<String>,
    results: Vec<ResultRow>,
    checks: Vec<Check>,
) -> Report {
    let backends: BTreeSet<String> = results.iter().filter_map(|r| r.backend.clone()).collect();
    let pass = !(results.is_empty() && checks.is_empty())
        && results.iter().all(|r| r.pass)
        && checks.iter().all(|c| c.pass);
    Report {
        milestone: milestone.into(),
        requested_backend,
        timestamp,
        host,
        commit,
        backends: backends.into_iter().collect(),
        results,
        checks,
        pass,
    }
}

/// The committed summary: the same report without bulky per-row detail.
pub fn summary(report: &Report) -> Report {
    let mut s = report.clone();
    for r in &mut s.results {
        r.detail = None;
    }
    s
}

/// Writes the full report to `<target_dir>/<m>.json` and the summary to
/// `<results_dir>/<m>.json`. Returns both paths.
pub fn write_reports(
    report: &Report,
    target_dir: &Path,
    results_dir: &Path,
) -> Result<(PathBuf, PathBuf)> {
    let name = format!("{}.json", report.milestone);
    let full = target_dir.join(&name);
    let short = results_dir.join(&name);
    for (path, rep) in [(&full, report.clone()), (&short, summary(report))] {
        let dir = path.parent().expect("report path has a parent");
        std::fs::create_dir_all(dir).with_context(|| format!("cannot create {}", dir.display()))?;
        let text = serde_json::to_string_pretty(&rep)? + "\n";
        std::fs::write(path, text).with_context(|| format!("cannot write {}", path.display()))?;
    }
    Ok((full, short))
}

fn fmt_opt(v: Option<f64>) -> String {
    v.map_or_else(|| "-".into(), |v| format!("{v}"))
}

/// Human-readable table of a report.
pub fn render(report: &Report) -> String {
    let mut s = String::new();
    let _ = writeln!(
        s,
        "accept {}  host {}  commit {}  at {}",
        report.milestone, report.host, report.commit, report.timestamp
    );
    if !report.backends.is_empty() {
        let _ = writeln!(s, "backends: {}", report.backends.join(", "));
    }
    if !report.results.is_empty() {
        let _ = writeln!(
            s,
            "\n{:<6} {:<24} {:<4} {:>10} {:>12} {:<7} reason",
            "result", "id", "op", "threshold", "measured", "unit"
        );
        for r in &report.results {
            let measured = match (r.value, r.passed) {
                (Some(v), _) => format!("{v}"),
                (None, Some(b)) => b.to_string(),
                (None, None) => "-".into(),
            };
            let _ = writeln!(
                s,
                "{:<6} {:<24} {:<4} {:>10} {:>12} {:<7} {}",
                if r.pass { "PASS" } else { "FAIL" },
                r.id,
                r.op,
                fmt_opt(r.threshold),
                measured,
                r.unit,
                r.reason
            );
        }
    }
    if !report.checks.is_empty() {
        let _ = writeln!(s, "\n{:<6} check", "result");
        for c in &report.checks {
            let _ = writeln!(
                s,
                "{:<6} {}{}",
                if c.pass { "PASS" } else { "FAIL" },
                c.name,
                c.detail
                    .as_deref()
                    .map(|d| format!(": {d}"))
                    .unwrap_or_default()
            );
        }
    }
    let _ = writeln!(
        s,
        "\naccept {}: {}",
        report.milestone,
        if report.pass { "PASS" } else { "FAIL" }
    );
    s
}

/// Host target triple from `rustc -vV`, or `unknown`.
pub fn host_triple() -> String {
    run_with_timeout(Command::new("rustc").arg("-vV"), Duration::from_secs(30))
        .filter(crate::proc::CmdOutput::complete)
        .and_then(|o| crate::doctor::parse_rustc_host(&o.stdout))
        .unwrap_or_else(|| "unknown".into())
}

/// `git status` arguments for the dirty check. `acceptance/results` is excluded because
/// `accept` itself rewrites the committed summary there on every run.
pub const GIT_STATUS_ARGS: [&str; 5] = ["status", "--porcelain", "--", ".", ":!acceptance/results"];

/// Formats the commit string from `git rev-parse --short HEAD` stdout and the outcome of
/// `git status`: `unknown` without a hash, `<hash>-unknown` when the status check failed
/// or its output was not captured completely (an empty capture must not read as clean),
/// `<hash>-dirty` when it listed changes, otherwise `<hash>`.
pub fn format_commit(head: Option<&str>, status: &Option<crate::proc::CmdOutput>) -> String {
    let Some(head) = head.map(str::trim).filter(|h| !h.is_empty()) else {
        return "unknown".into();
    };
    match status {
        Some(o) if o.success && o.complete() && o.stdout.trim().is_empty() => head.to_string(),
        Some(o) if o.success && o.complete() => format!("{head}-dirty"),
        _ => format!("{head}-unknown"),
    }
}

/// The thresholds of one milestone. Selection depends on the milestone only; `--backend`
/// never changes which thresholds apply.
pub fn select<'a>(all: &'a [Threshold], milestone: &str) -> Vec<&'a Threshold> {
    all.iter().filter(|t| t.milestone == milestone).collect()
}

/// Commit string for the report (see `format_commit`).
pub fn git_commit(root: &Path) -> String {
    let t = Duration::from_secs(30);
    let head = run_with_timeout(
        Command::new("git")
            .args(["rev-parse", "--short", "HEAD"])
            .current_dir(root),
        t,
    )
    .filter(|o| o.success && o.complete())
    .map(|o| o.stdout);
    let status = run_with_timeout(
        Command::new("git").args(GIT_STATUS_ARGS).current_dir(root),
        t,
    );
    format_commit(head.as_deref(), &status)
}

/// Runs acceptance for a milestone, writes the reports and prints the table.
/// Returns the overall pass.
pub fn run(root: &Path, milestone: &str, backend: Option<&str>) -> Result<bool> {
    let runner = runner_for(milestone)?;
    let all = thresholds::load(&thresholds::path_in(root))?;
    let selected = select(&all, milestone);

    let ctx = Ctx {
        root,
        milestone,
        backend,
    };
    let out = runner(&ctx).with_context(|| format!("accept {milestone}: runner failed"))?;

    let commit = git_commit(root);
    let results = evaluate(&selected, &out.measurements);
    let report = build_report(
        milestone,
        crate::timefmt::now_utc(),
        host_triple(),
        commit,
        backend.map(str::to_string),
        results,
        out.checks,
    );
    let (full, short) = write_reports(
        &report,
        &root.join("target").join("acceptance"),
        &root.join("acceptance").join("results"),
    )?;
    print!("{}", render(&report));
    println!("reports: {} and {}", full.display(), short.display());
    Ok(report.pass)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn th(id: &str, op: Op, value: Option<f64>, unit: &str) -> Threshold {
        Threshold {
            id: id.into(),
            milestone: "m1".into(),
            metric: format!("metric {id}"),
            op,
            value,
            unit: unit.into(),
            provisional: false,
        }
    }

    fn eval_one(t: Threshold, m: Measurement) -> ResultRow {
        let rows = evaluate(&[&t], &[m]);
        assert_eq!(rows.len(), 1, "{rows:?}");
        rows.into_iter().next().unwrap()
    }

    #[test]
    fn ge_passes_at_and_above_threshold_and_fails_below() {
        let t = || th("a", Op::Ge, Some(25.0), "dB");
        assert!(eval_one(t(), Measurement::value("a", 25.0, "dB")).pass);
        assert!(eval_one(t(), Measurement::value("a", 30.0, "dB")).pass);
        let r = eval_one(t(), Measurement::value("a", 24.999, "dB"));
        assert!(!r.pass);
        assert_eq!(r.threshold, Some(25.0));
        assert_eq!(r.value, Some(24.999));
        assert_eq!(r.metric, "metric a");
        assert_eq!(r.op, ">=");
    }

    #[test]
    fn le_passes_at_and_below_threshold_and_fails_above() {
        let t = || th("a", Op::Le, Some(1.5), "s");
        assert!(eval_one(t(), Measurement::value("a", 1.5, "s")).pass);
        assert!(eval_one(t(), Measurement::value("a", 0.2, "s")).pass);
        assert!(!eval_one(t(), Measurement::value("a", 1.5000001, "s")).pass);
    }

    #[test]
    fn eq_count_is_exact() {
        let t = || th("a", Op::Eq, Some(0.0), "count");
        assert!(eval_one(t(), Measurement::value("a", 0.0, "count")).pass);
        assert!(!eval_one(t(), Measurement::value("a", 1e-12, "count")).pass);
        assert!(!eval_one(t(), Measurement::value("a", 1.0, "count")).pass);
    }

    #[test]
    fn eq_float_uses_tiny_epsilon() {
        let t = || th("a", Op::Eq, Some(100.0), "%");
        assert!(eval_one(t(), Measurement::value("a", 100.0, "%")).pass);
        assert!(eval_one(t(), Measurement::value("a", 100.0 - 1e-12, "%")).pass);
        assert!(!eval_one(t(), Measurement::value("a", 99.99, "%")).pass);
    }

    #[test]
    fn pass_op_uses_boolean() {
        let t = || th("a", Op::Pass, None, "");
        assert!(eval_one(t(), Measurement::passed("a", true)).pass);
        let r = eval_one(t(), Measurement::passed("a", false));
        assert!(!r.pass);
        assert_eq!(r.passed, Some(false));
    }

    #[test]
    fn pass_op_with_value_is_malformed() {
        let mut m = Measurement::passed("a", true);
        m.value = Some(1.0);
        let r = eval_one(th("a", Op::Pass, None, ""), m);
        assert!(!r.pass);
        assert!(r.reason.contains("malformed"), "{}", r.reason);
        let r = eval_one(
            th("a", Op::Pass, None, ""),
            Measurement::value("a", 1.0, ""),
        );
        assert!(!r.pass);
        assert!(r.reason.contains("malformed"), "{}", r.reason);
    }

    #[test]
    fn comparison_with_boolean_is_malformed() {
        let r = eval_one(
            th("a", Op::Ge, Some(1.0), "dB"),
            Measurement::passed("a", true),
        );
        assert!(!r.pass);
        assert!(r.reason.contains("malformed"), "{}", r.reason);
    }

    #[test]
    fn non_finite_values_fail() {
        for v in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            let r = eval_one(
                th("a", Op::Le, Some(1.0), "s"),
                Measurement::value("a", v, "s"),
            );
            assert!(!r.pass, "{v} passed");
            assert!(r.reason.contains("not finite"), "{}", r.reason);
        }
        // +inf must not satisfy >= either.
        let r = eval_one(
            th("a", Op::Ge, Some(1.0), "dB"),
            Measurement::value("a", f64::INFINITY, "dB"),
        );
        assert!(!r.pass);
    }

    #[test]
    fn missing_metric_fails() {
        let t = th("a", Op::Ge, Some(1.0), "dB");
        let rows = evaluate(&[&t], &[]);
        assert_eq!(rows.len(), 1);
        assert!(!rows[0].pass);
        assert!(rows[0].reason.contains("missing metric"));
    }

    #[test]
    fn extra_metric_fails() {
        let t = th("a", Op::Ge, Some(1.0), "dB");
        let rows = evaluate(
            &[&t],
            &[
                Measurement::value("a", 2.0, "dB"),
                Measurement::value("invented", 1.0, "dB"),
            ],
        );
        assert_eq!(rows.len(), 2);
        assert!(rows[0].pass);
        let extra = rows.iter().find(|r| r.id == "invented").unwrap();
        assert!(!extra.pass);
        assert!(extra.reason.contains("no threshold"), "{}", extra.reason);
    }

    #[test]
    fn duplicate_measurement_fails() {
        let t = th("a", Op::Ge, Some(1.0), "dB");
        let rows = evaluate(
            &[&t],
            &[
                Measurement::value("a", 2.0, "dB"),
                Measurement::value("a", 3.0, "dB"),
            ],
        );
        assert!(
            rows.iter()
                .any(|r| !r.pass && r.reason.contains("duplicate"))
        );
    }

    #[test]
    fn backend_and_detail_are_carried() {
        let mut m = Measurement::value("a", 2.0, "dB");
        m.backend = Some("cuda".into());
        m.detail = Some("big detail".into());
        let r = eval_one(th("a", Op::Ge, Some(1.0), "dB"), m);
        assert_eq!(r.backend.as_deref(), Some("cuda"));
        assert_eq!(r.detail.as_deref(), Some("big detail"));
    }

    fn sample_report(pass_row: bool) -> Report {
        let t = th("a", Op::Ge, Some(1.0), "dB");
        let mut m = Measurement::value("a", if pass_row { 2.0 } else { 0.5 }, "dB");
        m.backend = Some("cpu".into());
        m.detail = Some("per-utterance breakdown".into());
        build_report(
            "m1",
            "2026-10-17T00:00:00Z".into(),
            "x86_64-pc-windows-msvc".into(),
            "abc1234".into(),
            Some("cpu".into()),
            evaluate(&[&t], &[m]),
            vec![Check::new("structural", true, "ok")],
        )
    }

    #[test]
    fn report_overall_pass_and_backends() {
        let r = sample_report(true);
        assert!(r.pass);
        assert_eq!(r.backends, vec!["cpu"]);
        assert!(!sample_report(false).pass);
    }

    #[test]
    fn failing_check_fails_report() {
        let r = build_report(
            "m0",
            "t".into(),
            "h".into(),
            "c".into(),
            None,
            vec![],
            vec![Check::new("a", true, ""), Check::new("b", false, "")],
        );
        assert!(!r.pass);
    }

    #[test]
    fn empty_report_fails() {
        let r = build_report(
            "m0",
            "t".into(),
            "h".into(),
            "c".into(),
            None,
            vec![],
            vec![],
        );
        assert!(!r.pass);
    }

    #[test]
    fn reports_round_trip_through_files() {
        let dir = std::env::temp_dir().join(format!("earshot-xtask-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let report = sample_report(true);
        let (full, short) =
            write_reports(&report, &dir.join("target"), &dir.join("results")).unwrap();
        assert!(full.ends_with("m1.json") && short.ends_with("m1.json"));

        let back: Report = serde_json::from_str(&std::fs::read_to_string(&full).unwrap()).unwrap();
        assert_eq!(back, report);

        let sum: Report = serde_json::from_str(&std::fs::read_to_string(&short).unwrap()).unwrap();
        assert_eq!(sum, summary(&report));
        assert_eq!(sum.results[0].detail, None);

        let v: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&full).unwrap()).unwrap();
        for key in [
            "milestone",
            "requested_backend",
            "timestamp",
            "host",
            "commit",
            "backends",
            "results",
            "checks",
            "pass",
        ] {
            assert!(v.get(key).is_some(), "missing key {key}");
        }
        let row = &v["results"][0];
        for key in [
            "id",
            "metric",
            "op",
            "threshold",
            "unit",
            "value",
            "passed",
            "backend",
            "pass",
            "reason",
        ] {
            assert!(row.get(key).is_some(), "missing row key {key}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn unimplemented_runner_names_its_task() {
        let e = runner_for("m8").err().unwrap().to_string();
        assert_eq!(
            e,
            "accept m8: runner not implemented (task none planned in PLAN.md 8)"
        );
        assert!(runner_for("m12").is_err());
        assert!(runner_for("m0").is_ok());
        assert!(runner_for("m1").is_ok());
        assert!(runner_for("m2").is_ok());
        assert!(runner_for("m3").is_ok());
    }

    #[test]
    fn registry_covers_every_milestone_once() {
        let names: Vec<&str> = REGISTRY.iter().map(|e| e.milestone).collect();
        assert_eq!(names, MILESTONES);
        let unique: BTreeSet<&str> = names.iter().copied().collect();
        assert_eq!(unique.len(), names.len());
    }

    #[test]
    fn unit_mismatch_fails() {
        let r = eval_one(
            th("a", Op::Ge, Some(25.0), "dB"),
            Measurement::value("a", 30.0, "s"),
        );
        assert!(!r.pass);
        assert_eq!(r.reason, "unit mismatch: measured s, threshold dB");
        let r = eval_one(
            th("a", Op::Le, Some(10.0), "%"),
            Measurement::value("a", 0.05, "ratio"),
        );
        assert!(!r.pass);
        assert!(r.reason.starts_with("unit mismatch"), "{}", r.reason);
    }

    #[test]
    fn pass_measurement_must_have_empty_unit() {
        let mut m = Measurement::passed("a", true);
        assert!(eval_one(th("a", Op::Pass, None, ""), m.clone()).pass);
        m.unit = "count".into();
        let r = eval_one(th("a", Op::Pass, None, ""), m);
        assert!(!r.pass);
        assert_eq!(r.reason, "unit mismatch: measured count, threshold ");
    }

    #[test]
    fn matching_unit_passes() {
        assert!(
            eval_one(
                th("a", Op::Le, Some(1.5), "s"),
                Measurement::value("a", 1.0, "s")
            )
            .pass
        );
    }

    #[test]
    fn requested_backend_is_recorded_in_both_reports() {
        let r = sample_report(true);
        assert_eq!(r.requested_backend.as_deref(), Some("cpu"));
        assert_eq!(summary(&r).requested_backend.as_deref(), Some("cpu"));
        let v = serde_json::to_value(summary(&r)).unwrap();
        assert_eq!(v["requested_backend"], "cpu");
    }

    #[test]
    fn selection_depends_on_milestone_only() {
        let mut a = th("a", Op::Ge, Some(1.0), "dB");
        a.milestone = "m2".into();
        let b = th("b", Op::Ge, Some(1.0), "dB");
        let all = [a, b];
        let ids: Vec<&str> = select(&all, "m2").iter().map(|t| t.id.as_str()).collect();
        assert_eq!(ids, ["a"]);
        assert!(select(&all, "m0").is_empty());
    }

    #[test]
    fn commit_formatting() {
        let ok = |out: &str| {
            Some(crate::proc::CmdOutput {
                success: true,
                code: Some(0),
                stdout: out.into(),
                stderr: String::new(),
                stdout_complete: true,
                stderr_complete: true,
            })
        };
        let failed = Some(crate::proc::CmdOutput {
            success: false,
            code: Some(128),
            stdout: String::new(),
            stderr: "fatal".into(),
            stdout_complete: true,
            stderr_complete: true,
        });
        assert_eq!(format_commit(None, &ok("")), "unknown");
        assert_eq!(format_commit(Some("  "), &ok("")), "unknown");
        assert_eq!(
            format_commit(
                Some(
                    "abc1234
"
                ),
                &ok("")
            ),
            "abc1234"
        );
        assert_eq!(
            format_commit(
                Some("abc1234"),
                &ok("
")
            ),
            "abc1234"
        );
        assert_eq!(
            format_commit(
                Some("abc1234"),
                &ok(" M PLAN.md
")
            ),
            "abc1234-dirty"
        );
        assert_eq!(format_commit(Some("abc1234"), &failed), "abc1234-unknown");
        assert_eq!(format_commit(Some("abc1234"), &None), "abc1234-unknown");
        // An empty capture that was cut short is not a clean tree.
        let cut = |out: &str| {
            Some(crate::proc::CmdOutput {
                stdout_complete: false,
                ..ok(out).unwrap()
            })
        };
        assert_eq!(format_commit(Some("abc1234"), &cut("")), "abc1234-unknown");
        assert_eq!(
            format_commit(Some("abc1234"), &cut(" M PLAN.md")),
            "abc1234-unknown"
        );
    }

    #[test]
    fn porcelain_excludes_committed_results() {
        assert_eq!(
            GIT_STATUS_ARGS,
            ["status", "--porcelain", "--", ".", ":!acceptance/results"]
        );
    }
}
