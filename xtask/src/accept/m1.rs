//! `accept m1`: echo canceller (T1.6, PLAN.md 4.0 and 4.2 `aec.*`).
//!
//! The work runs in a release build of the `aec_eval` example of `earshot-core`
//! (`aec.rtf` is defined on a release build, one core): it simulates every scenario in
//! `acceptance/aec_scenarios.toml` with the committed smoke speech, scores them with
//! `earshot_core::aec::eval` against `acceptance/thresholds.toml`, re-runs the robustness
//! grid and prints JSON, which this runner turns into exactly one measurement per `aec.*`
//! threshold:
//!
//! - `aec.erle_farend`, `aec.erle_linear`, `aec.doubletalk_sisdr`, `aec.passthrough`: the
//!   worst (lowest) value over the base scenarios of the matching class;
//! - `aec.convergence`, `aec.path_change`: the worst (longest) time;
//! - `aec.robustness`: every scored metric of every base scenario and every
//!   delay x drift variant meets its threshold;
//! - `aec.rtf`: canceller processing time over audio duration for the base scenarios.
//!
//! Informational scenarios (`Scenario::informational`, see
//! `earshot_core::aec::eval::is_informational`) are scored and reported as checks only:
//! they never feed a measurement or robustness. Likewise `aec.convergence` of stereo
//! scenarios at the 500 ms robustness grid points
//! (`earshot_core::aec::eval::grid_convergence_informational`, decided by the owner): reported
//! with its value as a check, every other metric of those points gating as before.
//!
//! The GATE-10 recording (optional; `aec_eval` finds its folder through `gate10.dir` of the
//! private settings file, `.earshot-private/gate12.toml` in the home folder) is
//! informational only: its real-world ERLE is reported as a check whose detail says so,
//! or that it was skipped because the settings or the files are absent. It is never a
//! measurement. The same holds for the GATE-10b stereo recording (the same folder with a
//! `-stereo` suffix), run through the stereo canceller and, for comparison, the mono one on
//! the downmix (T1.5b).
//!
//! The held-out set (T1.6b; `[heldout]` in `acceptance/aec_scenarios.toml`, speech pinned
//! in `fixtures/librispeech/aec_heldout.toml`) is never a measurement: one informational
//! check per gating scenario reports its worst value per metric over the held-out seeds
//! next to the dev value, and one gating check fails when any of those values is worse than
//! the committed [`HELDOUT_BASELINE`] by more than [`HELDOUT_TOLERANCE`]. The baseline is
//! written only by `cargo xtask aec-heldout-baseline` ([`write_heldout_baseline`]).
//!
//! The dev-wide sweep (T1.6c; every gating scenario at seed + 2000 with each voice pair of
//! `fixtures/librispeech/aec_dev.toml`, dev-clean speakers) is dev data: one informational
//! check ([`devwide_check`]) reports its worst values and misses; it never gates.

use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, bail, ensure};
use serde_json::{Value, json};

use super::{Check, Ctx, Measurement, RunOutput};
use crate::ci::cargo_command;
use crate::fixtures;
use crate::proc::run_with_timeout;

/// Deadline for building and running the evaluation.
pub const EVAL_TIMEOUT: Duration = Duration::from_secs(60 * 60);

pub(super) fn run(ctx: &Ctx) -> Result<RunOutput> {
    let json = run_eval(ctx.root, &[])?;
    evaluate(&json, load_baseline(ctx.root).as_ref())
}

/// Committed held-out baseline, relative to the repository root (T1.6b).
pub const HELDOUT_BASELINE: &str = "acceptance/aec_heldout_baseline.json";

/// How much worse than [`HELDOUT_BASELINE`] a held-out value may be: nothing. The simulator
/// and the canceller are deterministic (seeded ChaCha8, single-threaded, no clock or OS
/// input), and on the build host two `aec-heldout-baseline` runs and the held-out pass of
/// `accept m1` gave bit-identical values (T1.6b), so any change comes from the code.
pub const HELDOUT_TOLERANCE: f64 = 0.0;

/// Builds and runs the `aec_eval` example with the held-out and the dev-wide speech (their
/// checksums are verified first; missing files fail with the fetch instruction) and returns
/// its JSON.
fn run_eval(root: &Path, extra: &[&OsStr]) -> Result<Value> {
    let cache = fixtures::cache_dir()?;
    fixtures::verified_aec_heldout(root, &cache)?;
    fixtures::verified_aec_dev(root, &cache)?;
    let mut cmd = cargo_command();
    cmd.args([
        "run",
        "--release",
        "--quiet",
        "-p",
        "earshot-core",
        "--example",
        "aec_eval",
        "--",
    ])
    .arg(root)
    .arg("--heldout")
    .arg(&cache)
    .arg("--devwide")
    .arg(&cache)
    .args(extra)
    .current_dir(root);
    let out = run_with_timeout(&mut cmd, EVAL_TIMEOUT)
        .context("aec_eval did not finish within the deadline")?;
    if !out.success {
        bail!("aec_eval failed: {}", out.describe());
    }
    out.require_complete("aec_eval")?;
    serde_json::from_str(out.stdout.trim())
        .with_context(|| format!("aec_eval printed invalid JSON:\n{}", out.stdout))
}

/// The committed baseline, or why it cannot be used.
fn load_baseline(root: &Path) -> Result<Value, String> {
    let p = root.join(HELDOUT_BASELINE);
    let text = std::fs::read_to_string(&p).map_err(|e| {
        format!(
            "cannot read {}: {e}; generate it with `cargo xtask aec-heldout-baseline` and log that in LOG.md",
            p.display()
        )
    })?;
    parse_baseline(&text).map_err(|e| format!("{}: {e}", p.display()))
}

/// Parses a baseline file and checks its `sha256` (over the definition and the worst
/// values), so a hand edit is refused rather than compared against.
fn parse_baseline(text: &str) -> Result<Value, String> {
    let v: Value = serde_json::from_str(text).map_err(|e| e.to_string())?;
    let want = baseline_digest(&v["definition"], &v["worst"]);
    if v["sha256"].as_str() != Some(want.as_str()) {
        return Err(format!(
            "sha256 {} does not match its contents ({want}): the baseline was edited by hand; regenerate it with `cargo xtask aec-heldout-baseline` and log that in LOG.md",
            v["sha256"]
        ));
    }
    Ok(v)
}

/// SHA-256 of the compact JSON of `{definition, worst}` (keys sorted).
fn baseline_digest(definition: &Value, worst: &Value) -> String {
    let body = json!({ "definition": definition, "worst": worst }).to_string();
    crate::download::hex(&<sha2::Sha256 as sha2::Digest>::digest(body.as_bytes()))
}

/// `cargo xtask aec-heldout-baseline`: runs the held-out set with the current canceller and
/// writes [`HELDOUT_BASELINE`]. The only way the baseline changes; every use is logged.
pub fn write_heldout_baseline(root: &Path) -> Result<()> {
    let json = run_eval(root, &[OsStr::new("--heldout-only")])?;
    let text = baseline_json(&json["heldout"])?;
    let p = root.join(HELDOUT_BASELINE);
    std::fs::write(&p, &text).with_context(|| format!("write {}", p.display()))?;
    println!(
        "aec-heldout-baseline: wrote {}. Log this regeneration and its reason in LOG.md.",
        p.display()
    );
    Ok(())
}

/// Whether a lower value of the metric is better (times) rather than a higher one (dB).
fn lower_is_better(id: &str) -> bool {
    matches!(id, "aec.convergence" | "aec.path_change")
}

/// Held-out metric ids (the `aec.*` threshold ids `aec_eval` scores per run).
pub const HELDOUT_METRICS: [&str; 6] = [
    "aec.erle_farend",
    "aec.erle_linear",
    "aec.convergence",
    "aec.path_change",
    "aec.doubletalk_sisdr",
    "aec.passthrough",
];

/// A value that fails whatever it is compared with: NaN, or +inf ERLE (a silent, muted
/// output, never perfect cancellation; AGENTS.md, `accept m1` caps +inf only for SI-SDR).
fn unusable(id: &str, x: f64) -> bool {
    x.is_nan() || (x == f64::INFINITY && matches!(id, "aec.erle_farend" | "aec.erle_linear"))
}

/// Per gating scenario and metric: the worst value over its held-out runs (the `worst`
/// fold of `aec::eval`, except that an [`unusable`] value (NaN, +inf ERLE) on any seed is
/// the worst value, never skipped). A metric that does not apply (`null` on every run) is
/// absent.
pub fn heldout_worst(heldout: &Value) -> Result<BTreeMap<String, BTreeMap<String, f64>>> {
    let runs = heldout["runs"]
        .as_array()
        .context("aec_eval reported no held-out runs")?;
    ensure!(!runs.is_empty(), "aec_eval reported no held-out runs");
    let mut out: BTreeMap<String, BTreeMap<String, f64>> = BTreeMap::new();
    for r in runs {
        let base = r["base"].as_str().context("held-out run without base")?;
        let entry = out.entry(base.to_string()).or_default();
        for id in HELDOUT_METRICS {
            let v = &r["scores"][id];
            if v.is_null() {
                continue;
            }
            let x = number(v).with_context(|| format!("{base}: {id} is not a number: {v}"))?;
            let w = entry.entry(id.to_string()).or_insert(x);
            *w = if w.is_nan() || x.is_nan() {
                f64::NAN
            } else if unusable(id, *w) || unusable(id, x) {
                f64::INFINITY
            } else if lower_is_better(id) {
                w.max(x)
            } else {
                w.min(x)
            };
        }
    }
    Ok(out)
}

/// A value for a message: the number, or inf / -inf / nan without JSON quotes.
fn show(x: f64) -> String {
    match value_json(x) {
        Value::String(s) => s,
        v => v.to_string(),
    }
}

fn value_json(x: f64) -> Value {
    if x.is_finite() {
        json!(x)
    } else if x > 0.0 {
        json!("inf")
    } else if x < 0.0 {
        json!("-inf")
    } else {
        json!("nan")
    }
}

/// The baseline file for an `aec_eval` held-out object: the held-out definition (seed
/// offsets, speech) and the worst value per scenario and metric. Keys are sorted, so the
/// bytes depend only on the values. An [`unusable`] value (NaN, +inf ERLE) is refused: a
/// failure must never become the bar.
pub fn baseline_json(heldout: &Value) -> Result<String> {
    let worst = heldout_worst(heldout)?;
    let mut scenarios = serde_json::Map::new();
    for (scn, metrics) in &worst {
        let mut m = serde_json::Map::new();
        for (id, &x) in metrics {
            ensure!(
                !unusable(id, x),
                "held-out {scn} {id} is {} (a failure); refusing to baseline it",
                show(x)
            );
            m.insert(id.clone(), value_json(x));
        }
        scenarios.insert(scn.clone(), Value::Object(m));
    }
    let def = definition(heldout);
    let worst = Value::Object(scenarios);
    let doc = json!({
        "generated_by": "cargo xtask aec-heldout-baseline (T1.6b); never edit by hand, log every regeneration in LOG.md",
        "tolerance": HELDOUT_TOLERANCE,
        "sha256": baseline_digest(&def, &worst),
        "definition": def,
        "worst": worst,
    });
    Ok(serde_json::to_string_pretty(&doc)? + "\n")
}

/// What the held-out values depend on besides the code: seed offsets, speech, and a digest
/// of the room setup and every gating scenario's parameters (`aec_eval`'s `scenarios`).
fn definition(heldout: &Value) -> Value {
    let scenarios = heldout["scenarios"].as_str().unwrap_or_default();
    json!({
        "seed_offsets": heldout["seed_offsets"],
        "speech": heldout["speech"],
        "scenarios_sha256": crate::download::hex(
            &<sha2::Sha256 as sha2::Digest>::digest(scenarios.as_bytes())
        ),
    })
}

/// Every way the held-out results are worse than `baseline` (empty when none is).
pub fn heldout_regressions(heldout: &Value, baseline: &Value) -> Result<Vec<String>> {
    let mut bad = Vec::new();
    let def = definition(heldout);
    if baseline["definition"] != def {
        bad.push(format!(
            "held-out definition differs from the baseline's (baseline {}, now {def})",
            baseline["definition"]
        ));
    }
    let now = heldout_worst(heldout)?;
    let base = baseline["worst"]
        .as_object()
        .context("baseline has no worst values")?;
    for (scn, metrics) in &now {
        let Some(b) = base.get(scn).and_then(Value::as_object) else {
            bad.push(format!("{scn}: not in the baseline"));
            continue;
        };
        for (id, &x) in metrics {
            let Some(bv) = b.get(id) else {
                bad.push(format!("{scn} {id}: not in the baseline"));
                continue;
            };
            let bx = number(bv).with_context(|| format!("baseline {scn} {id}: {bv}"))?;
            ensure!(!unusable(id, bx), "baseline {scn} {id} is {}", show(bx));
            let worse = if unusable(id, x) {
                true
            } else if lower_is_better(id) {
                x > bx + HELDOUT_TOLERANCE
            } else {
                x < bx - HELDOUT_TOLERANCE
            };
            if worse {
                bad.push(format!("{scn} {id}: {} vs baseline {}", show(x), show(bx)));
            }
        }
        for id in b.keys().filter(|id| !metrics.contains_key(*id)) {
            bad.push(format!("{scn} {id}: in the baseline, not measured now"));
        }
    }
    for scn in base.keys().filter(|s| !now.contains_key(*s)) {
        bad.push(format!("{scn}: in the baseline, not measured now"));
    }
    Ok(bad)
}

/// The gating no-regression check: fails when the held-out results are missing, the
/// baseline is missing or unusable, or any held-out value is worse than the baseline.
pub fn heldout_regression_check(heldout: &Value, baseline: Result<&Value, &String>) -> Check {
    let name = format!(
        "held-out no-regression against {HELDOUT_BASELINE} (tolerance {HELDOUT_TOLERANCE})"
    );
    if heldout.is_null() {
        return Check::new(
            &name,
            false,
            "not measured: aec_eval reported no held-out set",
        );
    }
    let baseline = match baseline {
        Ok(b) => b,
        Err(e) => return Check::new(&name, false, e.clone()),
    };
    match heldout_regressions(heldout, baseline) {
        Ok(bad) if bad.is_empty() => {
            let n: usize =
                heldout_worst(heldout).map_or(0, |w| w.values().map(BTreeMap::len).sum());
            Check::new(
                &name,
                true,
                format!("{n} values, none worse than the baseline"),
            )
        }
        Ok(bad) => Check::new(
            &name,
            false,
            format!(
                "{} worse (fix on dev data; regenerate only with `cargo xtask aec-heldout-baseline` for a logged reason): {}",
                bad.len(),
                bad.join("; ")
            ),
        ),
        Err(e) => Check::new(&name, false, format!("cannot compare: {e:#}")),
    }
}

/// One informational line per gating scenario: its worst held-out value per metric next to
/// the dev value, and the threshold misses of its held-out runs. Never gates.
pub fn heldout_scenario_checks(json: &Value) -> Vec<Check> {
    let heldout = &json["heldout"];
    let Ok(worst) = heldout_worst(heldout) else {
        return Vec::new();
    };
    let seeds = heldout["seed_offsets"].as_array().map_or(0, Vec::len);
    let dev = |scn: &str, id: &str| -> String {
        let key = id.trim_start_matches("aec.");
        json["scenarios"]
            .as_array()
            .and_then(|a| a.iter().find(|s| s["name"] == scn))
            .map_or_else(|| "n/a".into(), |s| s["scores"][key].to_string())
    };
    worst
        .iter()
        .map(|(scn, metrics)| {
            let mut values: Vec<String> = metrics
                .iter()
                .map(|(id, x)| format!("{id}={} (dev {})", value_json(*x), dev(scn, id)))
                .collect();
            // Double-talk SI-SDR below SER 0: lowest over the seeds, never gated (PLAN 4.0).
            let below: Vec<f64> = heldout["runs"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|r| r["base"] == scn.as_str())
                .filter_map(|r| number(&r["doubletalk_sisdr_below_ser0"]))
                .collect();
            if !below.is_empty() {
                let low = below.iter().copied().fold(f64::INFINITY, |a, b| {
                    if b.is_nan() || a.is_nan() {
                        f64::NAN
                    } else {
                        a.min(b)
                    }
                });
                values.push(format!(
                    "doubletalk_sisdr below SER 0={} (informational, not baselined)",
                    show(low)
                ));
            }
            let misses: Vec<String> = heldout["runs"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|r| r["base"] == scn.as_str())
                .flat_map(|r| {
                    let off = r["offset"].clone();
                    r["would_fail"]
                        .as_array()
                        .cloned()
                        .unwrap_or_default()
                        .into_iter()
                        .filter_map(move |m| m.as_str().map(|m| format!("+{off}: {m}")))
                })
                .collect();
            Check::new(
                &format!("informational: held-out {scn}, worst of {seeds} seeds (not a threshold)"),
                true,
                format!(
                    "{}; would miss: {}",
                    if values.is_empty() {
                        "no scored metric".to_string()
                    } else {
                        values.join(", ")
                    },
                    if misses.is_empty() {
                        "none".to_string()
                    } else {
                        misses.join("; ")
                    }
                ),
            )
        })
        .collect()
}

/// Value recorded for a `>=` dB metric that is +inf (no error at all): far above any
/// threshold and any finite f32 signal ratio.
pub const INF_CAP_DB: f64 = 300.0;

/// Reads a number that the example may have written as "inf" / "-inf".
fn number(v: &Value) -> Option<f64> {
    match v {
        Value::Number(n) => n.as_f64(),
        Value::String(s) if s == "inf" => Some(f64::INFINITY),
        Value::String(s) if s == "-inf" => Some(f64::NEG_INFINITY),
        Value::String(s) if s == "nan" => Some(f64::NAN),
        _ => None,
    }
}

/// Turns the example's JSON into measurements and checks; `baseline` is the committed
/// held-out baseline or why it could not be read.
pub fn evaluate(json: &Value, baseline: Result<&Value, &String>) -> Result<RunOutput> {
    let head = &json["headline"];
    let mut measurements = Vec::new();
    for (id, key, unit, cap_inf) in [
        ("aec.erle_farend", "erle_farend", "dB", false),
        ("aec.erle_linear", "erle_linear", "dB", false),
        ("aec.convergence", "convergence", "s", false),
        ("aec.path_change", "path_change", "s", false),
        ("aec.doubletalk_sisdr", "doubletalk_sisdr", "dB", true),
        ("aec.passthrough", "passthrough", "dB", true),
    ] {
        let Some(v) = number(&head[key]) else {
            bail!("aec_eval: no headline value for {key}");
        };
        let mut detail = per_scenario_detail(json, key);
        // An SI-SDR at +inf (output identical to the target: an exact passthrough or a
        // perfect near end) is the best possible result: record it as the cap instead of
        // a non-finite failure. ERLE is not capped: +inf there means a silent (muted)
        // output, which must not pass as perfect cancellation; it stays non-finite and
        // fails, as does a `<=` time at +inf (never reached).
        let v = if cap_inf && v == f64::INFINITY {
            detail = format!("+inf, recorded as the cap {INF_CAP_DB} dB; {detail}");
            INF_CAP_DB
        } else {
            v
        };
        let mut m = Measurement::value(id, v, unit);
        m.backend = Some("cpu".into());
        m.detail = Some(detail);
        measurements.push(m);
    }

    let rob = &json["robustness"];
    let Some(pass) = rob["pass"].as_bool() else {
        bail!("aec_eval: no robustness result");
    };
    let failures: Vec<String> = rob["failures"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|f| f.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    let mut m = Measurement::passed("aec.robustness", pass);
    m.backend = Some("cpu".into());
    m.detail = Some(format!(
        "{} base + {} grid runs; {} failing: {}",
        rob["base_runs"],
        rob["grid_runs"],
        failures.len(),
        failures.join("; ")
    ));
    measurements.push(m);

    let Some(rtf) = number(&json["rtf"]["value"]) else {
        bail!("aec_eval: no rtf value");
    };
    let mut m = Measurement::value("aec.rtf", rtf, "ratio");
    m.backend = Some("cpu".into());
    m.detail = Some(format!(
        "release build, one thread: {} s of processing for {} s of audio",
        json["rtf"]["cpu_s"], json["rtf"]["audio_s"]
    ));
    measurements.push(m);

    let mut checks = vec![gate10_check(&json["gate10"])];
    checks.extend(informational_checks(&json["informational"]));
    checks.push(below_ser0_check(&json["doubletalk_below_ser0"]));
    checks.push(gate10b_check(&json["gate10b"]));
    checks.push(robustness_informational_check(
        &json["robustness"]["informational"],
    ));
    checks.push(second_seed_check(&json["second_seed"]));
    checks.push(devwide_check(&json["devwide"]));
    checks.push(doubletalk_highpassed_check(json));
    checks.extend(heldout_scenario_checks(json));
    checks.push(heldout_regression_check(&json["heldout"], baseline));
    Ok(RunOutput {
        measurements,
        checks,
    })
}

/// Name of [`doubletalk_highpassed_check`].
pub const DOUBLETALK_HIGHPASSED: &str =
    "informational: double-talk SI-SDR against the high-passed near end (not a threshold)";

/// Double-talk SI-SDR scored against the clean near end (the official `aec.doubletalk_sisdr`
/// scoring, at any SER) beside the same output scored against the near end passed through the
/// canceller's audio high-pass (T6.23c step 1): dev base scenarios, the
/// dev-wide and the held-out worst per scenario. Shows how much of the official value's loss
/// under the filter is the removed sub-cutoff near-end content. Never gates, never baselined.
pub fn doubletalk_highpassed_check(json: &Value) -> Check {
    let item = |set: &str, scn: &str, v: &Value| -> Option<String> {
        if v.is_null() || v["highpassed_near"].is_null() {
            return None;
        }
        let below = if v["gated"].as_bool() == Some(false) {
            " (below SER 0)"
        } else {
            ""
        };
        let val = |x: &Value| number(x).map_or_else(|| x.to_string(), show);
        Some(format!(
            "{set} {scn}{below}: {} (high-passed near end {})",
            val(&v["official"]),
            val(&v["highpassed_near"])
        ))
    };
    let mut items: Vec<String> = json["scenarios"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|s| {
            item(
                "dev",
                s["name"].as_str().unwrap_or("?"),
                &s["doubletalk_highpassed"],
            )
        })
        .collect();
    for (set, key) in [("dev-wide", "devwide"), ("held-out", "heldout")] {
        if let Some(m) = json[key]["doubletalk_highpassed_by_scenario"].as_object() {
            items.extend(m.iter().filter_map(|(scn, v)| item(set, scn, v)));
        }
    }
    Check::new(
        DOUBLETALK_HIGHPASSED,
        true,
        if items.is_empty() {
            "not measured: no double-talk run scored against a high-passed near end".to_string()
        } else {
            format!(
                "official value (high-passed near end), worst per scenario: {}",
                items.join(", ")
            )
        },
    )
}

/// One line per informational scenario: always passes, excluded from every measurement,
/// shows the scores and which thresholds it would miss.
pub fn informational_checks(scenarios: &Value) -> Vec<Check> {
    scenarios
        .as_array()
        .map(|a| {
            a.iter()
                .map(|s| {
                    let name = s["name"].as_str().unwrap_or("?");
                    let scores: Vec<String> = s["scores"]
                        .as_object()
                        .map(|o| {
                            o.iter()
                                .filter(|(_, v)| !v.is_null())
                                .map(|(k, v)| format!("{k}={v}"))
                                .collect()
                        })
                        .unwrap_or_default();
                    let misses: Vec<String> = s["would_fail"]
                        .as_array()
                        .map(|m| {
                            m.iter()
                                .filter_map(|x| x.as_str().map(str::to_string))
                                .collect()
                        })
                        .unwrap_or_default();
                    Check::new(
                        &format!("informational: scenario {name} (not a threshold)"),
                        true,
                        format!(
                            "{}; would miss: {}",
                            scores.join(", "),
                            if misses.is_empty() {
                                "none".into()
                            } else {
                                misses.join("; ")
                            }
                        ),
                    )
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Double-talk SI-SDR below SER 0 (base scenarios and robustness variants): reported,
/// never gated (PLAN.md 4.0; transcription quality there is gated by M2).
pub fn below_ser0_check(v: &Value) -> Check {
    let items: Vec<String> = v
        .as_array()
        .map(|a| {
            a.iter()
                .map(|x| {
                    format!(
                        "{}={}",
                        x["name"].as_str().unwrap_or("?"),
                        x["doubletalk_sisdr"]
                    )
                })
                .collect()
        })
        .unwrap_or_default();
    Check::new(
        "informational: double-talk SI-SDR below SER 0 (not a threshold)",
        true,
        if items.is_empty() {
            "none scored".to_string()
        } else {
            items.join(", ")
        },
    )
}

fn per_scenario_detail(json: &Value, key: &str) -> String {
    json["scenarios"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter(|s| !s["scores"][key].is_null())
                .map(|s| format!("{}={}", s["name"].as_str().unwrap_or("?"), s["scores"][key]))
                .collect::<Vec<_>>()
                .join(", ")
        })
        .unwrap_or_default()
}

/// Robustness grid points whose `aec.convergence` is informational (stereo scenarios at
/// 500 ms bulk delay; PLAN.md `aec.robustness`): always passes, lists every value.
pub fn robustness_informational_check(v: &Value) -> Check {
    const NAME: &str =
        "informational: stereo convergence at 500 ms bulk delay (aec.robustness, not gated)";
    let Some(a) = v.as_array() else {
        return Check::new(NAME, true, "not measured: aec_eval reported no such points");
    };
    let items: Vec<String> = a
        .iter()
        .map(|x| {
            format!(
                "{}={} s{}",
                x["name"].as_str().unwrap_or("?"),
                x["convergence"],
                if x["would_fail"].as_bool() == Some(true) {
                    " (over the bar)"
                } else {
                    ""
                }
            )
        })
        .collect();
    Check::new(
        NAME,
        true,
        if items.is_empty() {
            "none".to_string()
        } else {
            items.join(", ")
        },
    )
}

/// The informational second-seed sweep (T6.6a): every gating scenario re-simulated with
/// another seed, to show the tuning has not overfit the fixed seeds. Never gates.
pub fn second_seed_check(runs: &Value) -> Check {
    const NAME: &str = "informational: second-seed sweep of the gating scenarios (not a threshold)";
    let Some(a) = runs.as_array() else {
        return Check::new(
            NAME,
            true,
            "not measured: aec_eval reported no second-seed sweep",
        );
    };
    let misses: Vec<String> = a
        .iter()
        .flat_map(|s| {
            let name = s["name"].as_str().unwrap_or("?").to_string();
            s["would_fail"]
                .as_array()
                .cloned()
                .unwrap_or_default()
                .into_iter()
                .filter_map(move |m| m.as_str().map(|m| format!("{name}: {m}")))
        })
        .collect();
    Check::new(
        NAME,
        true,
        format!(
            "{} runs; would miss: {}",
            a.len(),
            if misses.is_empty() {
                "none".to_string()
            } else {
                misses.join("; ")
            }
        ),
    )
}

/// The informational dev-wide sweep (T1.6c): every gating scenario with each dev-wide voice
/// pair (`fixtures/librispeech/aec_dev.toml`). Dev data, for tuning and diagnosis: reported
/// with the worst value per scenario and metric and every threshold the runs would miss,
/// never gated.
pub fn devwide_check(v: &Value) -> Check {
    const NAME: &str = "informational: dev-wide voice sweep (dev data, not a threshold)";
    let (Some(runs), Some(worst)) = (v["runs"].as_array(), v["worst_by_scenario"].as_object())
    else {
        return Check::new(
            NAME,
            true,
            "not measured: aec_eval reported no dev-wide sweep",
        );
    };
    let pairs = v["pairs"].as_array().map_or(0, Vec::len);
    let values: Vec<String> = worst
        .iter()
        .filter_map(|(scn, m)| {
            let items: Vec<String> = m
                .as_object()?
                .iter()
                .filter(|(_, x)| !x.is_null())
                .map(|(id, x)| format!("{id}={x}"))
                .collect();
            (!items.is_empty()).then(|| format!("{scn}: {}", items.join(" ")))
        })
        .collect();
    let misses: Vec<String> = runs
        .iter()
        .flat_map(|r| {
            let at = format!(
                "{}@{}",
                r["base"].as_str().unwrap_or("?"),
                r["pair"].as_str().unwrap_or("?")
            );
            r["would_fail"]
                .as_array()
                .cloned()
                .unwrap_or_default()
                .into_iter()
                .filter_map(move |m| m.as_str().map(|m| format!("{at}: {m}")))
        })
        .collect();
    Check::new(
        NAME,
        true,
        format!(
            "{} runs over {pairs} voice pairs at seed + {}; worst per scenario: {}; would miss ({}): {}",
            runs.len(),
            v["seed_offset"],
            values.join(", "),
            misses.len(),
            if misses.is_empty() {
                "none".to_string()
            } else {
                misses.join("; ")
            }
        ),
    )
}

/// The informational GATE-10 line: always passes (it is not a threshold) and always says
/// what happened, including a skip.
pub fn gate10_check(g: &Value) -> Check {
    const NAME: &str = "informational: GATE-10 real-world ERLE (not a threshold)";
    let detail = match g["status"].as_str() {
        Some("measured") => format!(
            "{} dB over {} s, final delay {} ms ({})",
            g["erle_db"],
            g["seconds"],
            g["final_delay_ms"],
            g["detail"].as_str().unwrap_or("")
        ),
        Some("skipped") => format!(
            "skipped: {}",
            g["detail"].as_str().unwrap_or("recording absent")
        ),
        _ => format!(
            "not measured: {}",
            g["detail"].as_str().unwrap_or("unknown error")
        ),
    };
    Check::new(NAME, true, detail)
}

/// The informational GATE-10b line (stereo recording through the stereo canceller,
/// T1.5b): always passes and always says what happened, including a skip.
pub fn gate10b_check(g: &Value) -> Check {
    const NAME: &str = "informational: GATE-10b stereo real-world ERLE (not a threshold)";
    let detail = match g["status"].as_str() {
        Some("measured") => format!(
            "stereo canceller {} dB, mono canceller on the downmix {} dB, over {} s, final delay {} ms ({})",
            g["erle_db"],
            g["erle_db_mono_downmix"],
            g["seconds"],
            g["final_delay_ms"],
            g["detail"].as_str().unwrap_or("")
        ),
        Some("skipped") => format!(
            "skipped: {}",
            g["detail"].as_str().unwrap_or("recording absent")
        ),
        Some(_) => format!(
            "not measured: {}",
            g["detail"].as_str().unwrap_or("unknown error")
        ),
        None => "not measured: aec_eval reported no gate10b result".to_string(),
    };
    Check::new(NAME, true, detail)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const NO_BASELINE: &str = "no baseline in this test";

    fn ev(j: &Value) -> RunOutput {
        evaluate(j, Err(&NO_BASELINE.to_string())).unwrap()
    }

    /// A held-out object as `aec_eval` prints it: two scenarios, two seeds each.
    fn heldout_sample() -> Value {
        json!({
            "seed_offsets": [10000, 10001],
            "speech": {"far_speaker": 260, "near_speaker": 237, "far": ["260-1-0001"], "near": ["237-1-0001"]},
            "scenarios": "Setup { rir_ms: 250.0 }\n[Scenario { name: \"far-only\" }]",
            "runs": [
                {"base": "far-only", "offset": 10000, "seed": 10101, "would_fail": [],
                 "scores": {"aec.erle_farend": 27.123456789012345, "aec.erle_linear": null,
                            "aec.convergence": 0.9, "aec.path_change": null,
                            "aec.doubletalk_sisdr": null, "aec.passthrough": null}},
                {"base": "far-only", "offset": 10001, "seed": 10102, "would_fail": ["convergence 2.90 > 1.5"],
                 "scores": {"aec.erle_farend": 26.0, "aec.erle_linear": null,
                            "aec.convergence": 2.8999999999999986, "aec.path_change": null,
                            "aec.doubletalk_sisdr": null, "aec.passthrough": null}},
                {"base": "near-only", "offset": 10000, "seed": 10102, "would_fail": [],
                 "scores": {"aec.erle_farend": null, "aec.erle_linear": null,
                            "aec.convergence": null, "aec.path_change": null,
                            "aec.doubletalk_sisdr": null, "aec.passthrough": "inf"}},
                {"base": "near-only", "offset": 10001, "seed": 10103, "would_fail": [],
                 "scores": {"aec.erle_farend": null, "aec.erle_linear": null,
                            "aec.convergence": null, "aec.path_change": null,
                            "aec.doubletalk_sisdr": null, "aec.passthrough": 139.5},
                 "doubletalk_sisdr_below_ser0": 2.5}
            ]
        })
    }

    fn baseline_of(h: &Value) -> Value {
        parse_baseline(&baseline_json(h).unwrap()).unwrap()
    }

    fn set_score(h: &mut Value, run: usize, id: &str, v: Value) {
        h["runs"][run]["scores"][id] = v;
    }

    #[test]
    fn m1_doubletalk_highpassed_check_reports_both_values_and_never_gates() {
        let dt = |official: Value, gated: bool, hp: Value| json!({"official": official, "gated": gated, "highpassed_near": hp});
        let j = json!({
            "scenarios": [
                {"name": "double-talk-ser-0", "doubletalk_highpassed": dt(json!(12.5), true, json!(13.25))},
                {"name": "far-only", "doubletalk_highpassed": null}
            ],
            "devwide": {"doubletalk_highpassed_by_scenario": {
                "double-talk-ser-0": dt(json!(-3.1), true, json!(4.5))
            }},
            "heldout": {"doubletalk_highpassed_by_scenario": {
                "double-talk-ser-0": dt(json!(9.25), true, json!(9.75)),
                "double-talk-ser-10": dt(json!("-inf"), false, json!("-inf"))
            }}
        });
        let c = doubletalk_highpassed_check(&j);
        assert!(c.pass, "informational only");
        let detail = c.detail.unwrap_or_default();
        for want in [
            "dev double-talk-ser-0: 12.5 (high-passed near end 13.25)",
            "dev-wide double-talk-ser-0: -3.1 (high-passed near end 4.5)",
            "held-out double-talk-ser-0: 9.25 (high-passed near end 9.75)",
            "held-out double-talk-ser-10 (below SER 0): -inf (high-passed near end -inf)",
        ] {
            assert!(detail.contains(want), "{want} not in {detail}");
        }
        assert!(!detail.contains("far-only"), "{detail}");
        // Without any high-passed value (canceller without an audio high-pass) it says so.
        let none = doubletalk_highpassed_check(&json!({"scenarios": []}));
        let nd = none.detail.unwrap_or_default();
        assert!(none.pass && nd.contains("not measured"), "{nd}");
        // It is one of m1's checks.
        let names: Vec<String> = ev(&sample(json!({"status": "skipped", "detail": "absent"})))
            .checks
            .into_iter()
            .map(|c| c.name)
            .collect();
        assert!(
            names.contains(&DOUBLETALK_HIGHPASSED.to_string()),
            "{names:?}"
        );
    }

    #[test]
    fn m1_heldout_worst_folds_each_metric_in_its_direction() {
        let w = heldout_worst(&heldout_sample()).unwrap();
        assert_eq!(w["far-only"]["aec.erle_farend"], 26.0);
        assert_eq!(w["far-only"]["aec.convergence"], 2.8999999999999986);
        assert_eq!(w["near-only"]["aec.passthrough"], 139.5);
        assert!(!w["far-only"].contains_key("aec.passthrough"));
        // A NaN on any seed is the worst value, never skipped (unlike f64::min).
        let mut h = heldout_sample();
        set_score(&mut h, 0, "aec.erle_farend", json!("nan"));
        assert!(heldout_worst(&h).unwrap()["far-only"]["aec.erle_farend"].is_nan());
        assert!(heldout_worst(&json!({"runs": []})).is_err());
        assert!(heldout_worst(&Value::Null).is_err());
    }

    #[test]
    fn m1_heldout_check_passes_on_equal_and_fails_on_any_worse_value() {
        let h = heldout_sample();
        let base = baseline_of(&h);
        let check = |now: &Value| heldout_regression_check(now, Ok(&base));
        assert!(check(&h).pass, "{:?}", check(&h).detail);
        assert!(heldout_regressions(&h, &base).unwrap().is_empty());

        // Worse by the smallest step, in each direction.
        let mut lower = h.clone();
        set_score(&mut lower, 1, "aec.erle_farend", json!(25.999999999999996));
        assert!(!check(&lower).pass);
        assert!(
            check(&lower)
                .detail
                .unwrap()
                .contains("far-only aec.erle_farend")
        );
        let mut slower = h.clone();
        set_score(&mut slower, 1, "aec.convergence", json!(2.9));
        assert!(!check(&slower).pass);
        let mut muted = h.clone();
        set_score(&mut muted, 2, "aec.passthrough", json!(139.0));
        assert!(!check(&muted).pass, "the worst seed got worse");
        let mut nan = h.clone();
        set_score(&mut nan, 0, "aec.convergence", json!("nan"));
        assert!(!check(&nan).pass);
        // +inf ERLE is a muted output: the worst value, on any seed, never "better".
        for run in [0, 1] {
            let mut muted_erle = h.clone();
            set_score(&mut muted_erle, run, "aec.erle_farend", json!("inf"));
            assert_eq!(
                heldout_worst(&muted_erle).unwrap()["far-only"]["aec.erle_farend"],
                f64::INFINITY
            );
            let c = check(&muted_erle);
            assert!(!c.pass, "run {run}: {:?}", c.detail);
            assert!(c.detail.unwrap().contains("far-only aec.erle_farend: inf"));
        }

        // Better values pass; only the worst seed counts.
        let mut better = h.clone();
        set_score(&mut better, 1, "aec.erle_farend", json!(30.0));
        set_score(&mut better, 1, "aec.convergence", json!(0.5));
        set_score(&mut better, 3, "aec.passthrough", json!("inf"));
        assert!(check(&better).pass, "{:?}", check(&better).detail);
        let mut other_seed = h.clone();
        set_score(&mut other_seed, 0, "aec.erle_farend", json!(26.5));
        assert!(check(&other_seed).pass);

        // A metric or scenario that appears or disappears, or a changed definition, fails.
        let mut gone = h.clone();
        for r in 0..2 {
            set_score(&mut gone, r, "aec.convergence", Value::Null);
        }
        assert!(!check(&gone).pass);
        let mut extra = h.clone();
        for r in 0..2 {
            set_score(&mut extra, r, "aec.erle_linear", json!(20.0));
        }
        assert!(!check(&extra).pass);
        let mut fewer = h.clone();
        fewer["runs"].as_array_mut().unwrap().truncate(2);
        assert!(!check(&fewer).pass);
        let mut seeds = h.clone();
        seeds["seed_offsets"] = json!([10000, 10002]);
        assert!(!check(&seeds).pass);
        let mut voice = h.clone();
        voice["speech"]["near_speaker"] = json!(121);
        assert!(!check(&voice).pass);
        // Any change to the setup or a gating scenario's parameters forces a regeneration.
        let mut scn = h.clone();
        scn["scenarios"] = json!("Setup { rir_ms: 251.0 }\n[Scenario { name: \"far-only\" }]");
        let c = check(&scn);
        assert!(!c.pass && c.detail.unwrap().contains("definition differs"));

        // No results or no baseline is a failure, never a pass.
        assert!(!heldout_regression_check(&Value::Null, Ok(&base)).pass);
        let missing = "no file".to_string();
        let c = heldout_regression_check(&h, Err(&missing));
        assert!(!c.pass && c.detail.unwrap().contains("no file"));
    }

    #[test]
    fn m1_heldout_baseline_generator_is_deterministic_and_round_trips_exactly() {
        let h = heldout_sample();
        let a = baseline_json(&h).unwrap();
        assert_eq!(a, baseline_json(&h).unwrap());
        // Run order does not change a byte.
        let mut shuffled = h.clone();
        shuffled["runs"].as_array_mut().unwrap().reverse();
        assert_eq!(a, baseline_json(&shuffled).unwrap());
        assert!(a.ends_with("}\n"));
        let b = baseline_of(&h);
        assert_eq!(b["worst"]["near-only"]["aec.passthrough"], json!(139.5));
        assert_eq!(b["definition"]["seed_offsets"], json!([10000, 10001]));
        // Values survive the file exactly (tolerance 0 depends on it): awkward f64 bit
        // patterns compare equal after a round trip, and one ulp worse fails.
        let mut x: u64 = 0x9E37_79B9_7F4A_7C15;
        for _ in 0..2000 {
            x = x.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
            let v = f64::from_bits((x >> 12) | 0x4000_0000_0000_0000) * 7.0;
            let mut hv = h.clone();
            set_score(&mut hv, 0, "aec.erle_farend", json!(v));
            set_score(&mut hv, 1, "aec.erle_farend", json!(v));
            let bv = baseline_of(&hv);
            assert!(heldout_regressions(&hv, &bv).unwrap().is_empty(), "{v:e}");
            let mut worse = hv.clone();
            set_score(
                &mut worse,
                1,
                "aec.erle_farend",
                json!(f64::from_bits(v.to_bits() - 1)),
            );
            assert!(
                !heldout_regressions(&worse, &bv).unwrap().is_empty(),
                "{v:e}"
            );
        }
        // A NaN or a muted (+inf ERLE) value is never baselined; +inf SI-SDR is.
        let mut nan = h.clone();
        set_score(&mut nan, 0, "aec.erle_farend", json!("nan"));
        assert!(baseline_json(&nan).is_err());
        for id in ["aec.erle_farend", "aec.erle_linear"] {
            let mut muted = h.clone();
            set_score(&mut muted, 0, id, json!("inf"));
            set_score(&mut muted, 1, id, json!(20.0));
            assert!(baseline_json(&muted).is_err(), "{id}");
        }
        let mut perfect = h.clone();
        set_score(&mut perfect, 3, "aec.passthrough", json!("inf"));
        assert!(baseline_json(&perfect).is_ok());
    }

    #[test]
    fn m1_heldout_baseline_refuses_hand_edits() {
        let text = baseline_json(&heldout_sample()).unwrap();
        assert!(parse_baseline(&text).is_ok());
        let edited = text.replacen("26.0", "25.0", 1);
        assert_ne!(edited, text);
        let e = parse_baseline(&edited).unwrap_err();
        assert!(e.contains("edited by hand"), "{e}");
        let mut v: Value = serde_json::from_str(&text).unwrap();
        v["definition"]["seed_offsets"] = json!([1]);
        assert!(parse_baseline(&v.to_string()).is_err());
        v["sha256"] = Value::Null;
        assert!(parse_baseline(&v.to_string()).is_err());
    }

    #[test]
    fn m1_committed_heldout_baseline_matches_the_heldout_definition() {
        let root = crate::repo_root();
        let base = load_baseline(&root).unwrap();
        assert_eq!(base["tolerance"], json!(HELDOUT_TOLERANCE));
        let file: toml::Table = toml::from_str(
            &std::fs::read_to_string(root.join("acceptance/aec_scenarios.toml")).unwrap(),
        )
        .unwrap();
        let offsets: Vec<i64> = file["heldout"]["seed_offsets"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_integer().unwrap())
            .collect();
        let committed: Vec<i64> = base["definition"]["seed_offsets"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_i64().unwrap())
            .collect();
        assert_eq!(offsets, committed);
        let speech = fixtures::load_aec_heldout(&root).unwrap();
        let ids = |end: fixtures::CallEnd| -> Vec<Value> {
            speech
                .iter()
                .filter(|p| p.end == end)
                .map(|p| json!(p.id))
                .collect()
        };
        assert_eq!(
            base["definition"]["speech"]["far"],
            json!(ids(fixtures::CallEnd::Far))
        );
        assert_eq!(
            base["definition"]["speech"]["near"],
            json!(ids(fixtures::CallEnd::Near))
        );
        // One entry per gating scenario.
        let gating = file["scenario"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|s| s.get("informational").and_then(toml::Value::as_bool) != Some(true))
            .count();
        assert_eq!(base["worst"].as_object().unwrap().len(), gating);
    }

    #[test]
    fn m1_heldout_results_are_informational_checks_plus_one_regression_check() {
        let mut j = sample(json!({"status": "skipped", "detail": "absent"}));
        j["heldout"] = heldout_sample();
        let base = baseline_of(&j["heldout"]);
        let out = evaluate(&j, Ok(&base)).unwrap();
        assert_eq!(out.measurements.len(), 8, "held-out adds no measurement");
        let held: Vec<&Check> = out
            .checks
            .iter()
            .filter(|c| c.name.contains("held-out"))
            .collect();
        assert_eq!(held.len(), 3);
        let far = held.iter().find(|c| c.name.contains("far-only")).unwrap();
        assert!(far.pass && far.name.contains("informational"));
        let d = far.detail.clone().unwrap();
        assert!(d.contains("aec.erle_farend=26.0 (dev 27.0)"), "{d}");
        assert!(d.contains("+10001: convergence 2.90 > 1.5"), "{d}");
        let near = held.iter().find(|c| c.name.contains("near-only")).unwrap();
        let d = near.detail.clone().unwrap();
        assert!(
            d.contains("doubletalk_sisdr below SER 0=2.5 (informational, not baselined)"),
            "{d}"
        );
        let reg = held.last().unwrap();
        assert!(reg.pass && reg.name.contains("no-regression"));
        // A worse held-out value fails the run through that check alone.
        set_score(&mut j["heldout"], 1, "aec.erle_farend", json!(25.0));
        let out = evaluate(&j, Ok(&base)).unwrap();
        assert!(!out.checks.last().unwrap().pass);
        assert!(out.checks.iter().filter(|c| !c.pass).count() == 1);
        // Without held-out results the regression check fails.
        let out = ev(&sample(json!({"status": "skipped", "detail": "absent"})));
        assert!(!out.checks.last().unwrap().pass);
    }

    fn sample(gate10: Value) -> Value {
        json!({
            "headline": {
                "erle_farend": 27.0, "erle_linear": 12.5, "convergence": 0.9,
                "path_change": "inf", "doubletalk_sisdr": 9.0, "passthrough": "inf"
            },
            "scenarios": [
                {"name": "far-only", "scores": {"erle_farend": 27.0}},
                {"name": "near-only", "scores": {"erle_farend": null}}
            ],
            "robustness": {"pass": false, "base_runs": 13, "grid_runs": 36,
                           "failures": ["a: x"]},
            "rtf": {"value": 0.003, "cpu_s": 1.0, "audio_s": 390.0},
            "gate10": gate10
        })
    }

    #[test]
    fn m1_evaluate_emits_one_measurement_per_threshold() {
        let out = ev(&sample(json!({"status": "skipped", "detail": "absent"})));
        let ids: Vec<&str> = out.measurements.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(
            ids,
            [
                "aec.erle_farend",
                "aec.erle_linear",
                "aec.convergence",
                "aec.path_change",
                "aec.doubletalk_sisdr",
                "aec.passthrough",
                "aec.robustness",
                "aec.rtf"
            ]
        );
        // "inf" is carried through so a never-reached time fails the framework's
        // finiteness check instead of disappearing.
        assert_eq!(out.measurements[3].value, Some(f64::INFINITY));
        assert_eq!(out.measurements[6].passed, Some(false));
        assert_eq!(out.measurements[6].unit, "");
        assert_eq!(out.measurements[7].unit, "ratio");
        assert!(
            out.measurements[0]
                .detail
                .as_deref()
                .unwrap()
                .contains("far-only=27.0")
        );
    }

    #[test]
    fn m1_inf_on_a_higher_is_better_metric_is_capped_not_failed() {
        let out = ev(&sample(json!({"status": "skipped", "detail": "absent"})));
        let pass = out
            .measurements
            .iter()
            .find(|m| m.id == "aec.passthrough")
            .unwrap();
        assert_eq!(pass.value, Some(INF_CAP_DB));
        assert!(
            pass.detail
                .as_deref()
                .unwrap()
                .starts_with("+inf, recorded as the cap")
        );
        // A never-reached time stays infinite (and fails in the framework).
        let pc = out
            .measurements
            .iter()
            .find(|m| m.id == "aec.path_change")
            .unwrap();
        assert_eq!(pc.value, Some(f64::INFINITY));
        // ERLE at +inf is a muted output, never capped.
        let mut j = sample(json!({"status": "skipped", "detail": "absent"}));
        j["headline"]["erle_farend"] = json!("inf");
        let out = ev(&j);
        let erle = out
            .measurements
            .iter()
            .find(|m| m.id == "aec.erle_farend")
            .unwrap();
        assert_eq!(erle.value, Some(f64::INFINITY));
    }

    #[test]
    fn m1_stereo_convergence_points_are_reported_not_gated() {
        let c = robustness_informational_check(&json!([
            {"name": "stereo-image-change@500ms+0ppm", "convergence": 1.6, "would_fail": true},
            {"name": "stereo-far-only@500ms+0ppm", "convergence": 1.4, "would_fail": false}
        ]));
        assert!(c.pass && c.name.contains("informational"));
        let d = c.detail.unwrap();
        assert!(
            d.contains("stereo-image-change@500ms+0ppm=1.6 s (over the bar)"),
            "{d}"
        );
        assert!(d.contains("stereo-far-only@500ms+0ppm=1.4 s"), "{d}");
        // A failing robustness grid still fails the measurement: the check adds nothing.
        let mut j = sample(json!({"status": "skipped", "detail": "absent"}));
        j["robustness"]["informational"] = json!([]);
        let out = ev(&j);
        let rob = out
            .measurements
            .iter()
            .find(|m| m.id == "aec.robustness")
            .unwrap();
        assert_eq!(rob.passed, Some(false));
    }

    #[test]
    fn m1_second_seed_sweep_is_informational() {
        let c = second_seed_check(&json!([
            {"name": "far-only", "would_fail": []},
            {"name": "double-talk-ser-0", "would_fail": ["doubletalk_sisdr 9.00 < 10"]}
        ]));
        assert!(c.pass && c.name.contains("informational"));
        let d = c.detail.unwrap();
        assert!(d.starts_with("2 runs"), "{d}");
        assert!(
            d.contains("double-talk-ser-0: doubletalk_sisdr 9.00 < 10"),
            "{d}"
        );
        let missing = second_seed_check(&Value::Null);
        assert!(missing.pass);
        assert!(missing.detail.unwrap().starts_with("not measured"));
    }

    #[test]
    fn m1_devwide_sweep_is_informational() {
        let c = devwide_check(&json!({
            "seed_offset": 2000,
            "pairs": [{"far": 1, "near": 2}, {"far": 2, "near": 1}],
            "worst_by_scenario": {
                "far-only": {"aec.erle_farend": 20.5, "aec.convergence": 1.2, "aec.passthrough": null},
                "near-only": {"aec.erle_farend": null}
            },
            "runs": [
                {"base": "far-only", "pair": "1/2", "would_fail": ["erle_farend 20.50 < 25"]},
                {"base": "far-only", "pair": "2/1", "would_fail": []}
            ]
        }));
        assert!(c.pass && c.name.contains("informational"));
        let d = c.detail.unwrap();
        assert!(
            d.starts_with("2 runs over 2 voice pairs at seed + 2000"),
            "{d}"
        );
        assert!(
            d.contains("far-only: aec.convergence=1.2 aec.erle_farend=20.5"),
            "{d}"
        );
        assert!(!d.contains("near-only"), "{d}");
        assert!(
            d.contains("would miss (1): far-only@1/2: erle_farend 20.50 < 25"),
            "{d}"
        );
        let missing = devwide_check(&Value::Null);
        assert!(missing.pass);
        assert!(missing.detail.unwrap().starts_with("not measured"));
    }

    #[test]
    fn m1_gate10b_is_informational_and_reports_skips() {
        let detail = |c: &Check| c.detail.clone().unwrap_or_default();
        let skipped = gate10b_check(&json!({"status": "skipped", "detail": "no files"}));
        assert!(skipped.pass);
        assert!(detail(&skipped).starts_with("skipped: no files"));
        let missing = gate10b_check(&Value::Null);
        assert!(missing.pass);
        assert!(detail(&missing).starts_with("not measured"));
        let measured = gate10b_check(&json!({
            "status": "measured", "erle_db": 12.5, "erle_db_mono_downmix": 9.0,
            "seconds": 120.0, "final_delay_ms": 20.0, "detail": "real call"
        }));
        assert!(measured.pass && measured.name.contains("informational"));
        assert!(detail(&measured).contains("stereo canceller 12.5 dB"));
        assert!(detail(&measured).contains("downmix 9.0 dB"));
    }

    #[test]
    fn m1_gate10_is_informational_and_reports_skips() {
        let skipped = gate10_check(&json!({"status": "skipped", "detail": "no files"}));
        assert!(skipped.pass);
        assert!(
            skipped
                .detail
                .as_deref()
                .unwrap()
                .starts_with("skipped: no files")
        );
        let measured = gate10_check(&json!({
            "status": "measured", "erle_db": 3.4, "seconds": 120.0,
            "final_delay_ms": 216.5, "detail": "informational"
        }));
        assert!(measured.name.contains("informational"));
        assert!(measured.detail.as_deref().unwrap().contains("3.4 dB"));
        // Never a measurement: the framework rejects measurements without a threshold.
        let out = ev(&sample(json!({"status": "measured", "erle_db": 3.0})));
        assert!(
            out.measurements
                .iter()
                .all(|m| m.id.starts_with("aec.") && m.id != "aec.gate10")
        );
    }

    #[test]
    fn m1_informational_scenarios_are_checks_only() {
        let mut j = sample(json!({"status": "skipped", "detail": "absent"}));
        j["informational"] = json!([{
            "name": "softclip4-double-talk", "scores": {"doubletalk_sisdr": 1.0, "erle_farend": null},
            "would_fail": ["doubletalk_sisdr 1.00 < 10"]
        }]);
        let out = ev(&j);
        assert_eq!(
            out.measurements.len(),
            8,
            "informational adds no measurement"
        );
        let c = &out.checks[1];
        assert!(c.pass);
        assert!(c.name.contains("softclip4-double-talk") && c.name.contains("informational"));
        assert!(
            c.detail
                .as_deref()
                .unwrap()
                .contains("doubletalk_sisdr 1.00 < 10")
        );
    }

    #[test]
    fn m1_evaluate_rejects_incomplete_output() {
        let mut j = sample(json!({"status": "skipped"}));
        j["headline"]["erle_farend"] = Value::Null;
        assert!(evaluate(&j, Err(&NO_BASELINE.to_string())).is_err());
    }
}
