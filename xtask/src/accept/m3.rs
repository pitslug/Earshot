//! `accept m3`: diarisation and voiceprints (T3.5, PLAN.md 4.0 and 4.2 `dia.*`, `vp.*`).
//!
//! The work runs in a release build of the `diar_eval` example of `earshot-runtime`, built
//! first and then spawned directly under the deadline. It runs the production speaker setup
//! on the CPU (as the final pass does: pyannote segmentation-3.0 and CAM++ through
//! `backends::load_diarisation`, `PostConfig` with the dev-clean calibrated clustering and
//! voiceprint thresholds); every metric definition is in that example's module docs. It
//! prints JSON with exactly one entry per threshold id of M3.
//!
//! This runner does not take the example's headline numbers on trust: it recomputes every
//! measurement from the components the example reports (DER from summed missed, false alarm
//! and confusion seconds; the count-error median from the item list, each item's error from
//! its true and found counts; the large-meeting count error from the synthetic scenes with
//! 4-6 speakers; the voiceprint rates from the match tallies) and fails if the
//! example's value differs. It checks the meetings are the pinned AMI set
//! ([`crate::fixtures::AMI_MEETINGS`]) in both conditions, that the clustering and voiceprint
//! thresholds used are the core calibrations for the reported embedding model (never tuned
//! here), and that ONNX Runtime placed both sessions on the CPU. Per-meeting DER with its
//! missed / false alarm / confusion breakdown, the count-error distribution, the voiceprint
//! tallies and the data used (meetings, speakers, seeds) are kept as checks, because the
//! committed summary drops measurement detail.
//!
//! `--backend` may only be `cpu` (the production speaker models run on the CPU, T6.11).
//! Missing models or fixtures make the example fail with the `cargo xtask fetch-models` /
//! `fetch-fixtures` command to run; nothing is skipped.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result, bail, ensure};
use earshot_core::diar::EmbeddingModelId;
use earshot_core::diar::cluster::calibrated_cluster_threshold;
use earshot_core::voiceprint::calibrated_threshold;
use serde_json::Value;

use super::{Check, Ctx, Measurement, RunOutput};
use crate::ci::cargo_command;
use crate::proc::run_with_timeout;

/// Deadline for building the example.
pub const BUILD_TIMEOUT: Duration = Duration::from_secs(60 * 60);
/// Deadline for the evaluation itself (CPU diarisation of four AMI recordings and the
/// synthetic scenes takes minutes on the desktop).
pub const EVAL_TIMEOUT: Duration = Duration::from_secs(3 * 60 * 60);

/// AMI metric ids and the condition each is measured on.
pub const AMI_METRICS: [(&str, &str); 2] =
    [("dia.der_ami_ihm", "ihm_mix"), ("dia.der_ami_sdm", "sdm")];
/// Every measured id, in report order.
pub const METRICS: [&str; 6] = [
    "dia.der_ami_ihm",
    "dia.der_ami_sdm",
    "dia.count_error",
    "dia.count_error_large",
    "vp.top1",
    "vp.false_accept",
];
/// Synthetic speaker counts (PLAN.md 4.2: 1-6 speaker mixes).
pub const SCENE_COUNTS: std::ops::RangeInclusive<u64> = 1..=6;
/// Speaker counts scored by `dia.count_error_large` (T3.5a: 4, 5 and 6 speaker scenes).
pub const LARGE_COUNTS: std::ops::RangeInclusive<u64> = 4..=6;
/// Voiceprint clips per role over both folds: 40 test-clean speakers x 3 clips, each speaker
/// enrolled in one fold and unknown in the other.
pub const VP_CLIPS_PER_ROLE: u64 = 120;

/// Relative tolerance between a recomputed value and the example's (floating point noise).
const AGREE: f64 = 1e-9;

/// The `cargo build` arguments of the example (CPU only: no features).
#[must_use]
pub fn build_args() -> Vec<&'static str> {
    vec![
        "build",
        "--release",
        "-p",
        "earshot-runtime",
        "--example",
        "diar_eval",
        "--message-format=json-render-diagnostics",
    ]
}

/// The `diar_eval` executable named in cargo's JSON build messages.
#[must_use]
pub fn example_executable(messages: &str) -> Option<PathBuf> {
    messages
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter(|m| m["reason"] == "compiler-artifact" && m["target"]["name"] == "diar_eval")
        .find_map(|m| m["executable"].as_str().map(PathBuf::from))
}

/// Refuses any `--backend` other than `cpu`.
///
/// # Errors
/// Another backend.
pub fn check_backend(requested: Option<&str>) -> Result<()> {
    match requested {
        None | Some("cpu") => Ok(()),
        Some(other) => bail!(
            "accept m3 runs the production speaker models, which run on the CPU (T6.11); \
             --backend {other} is not supported"
        ),
    }
}

pub(super) fn run(ctx: &Ctx) -> Result<RunOutput> {
    check_backend(ctx.backend)?;
    let models = crate::models::cache_dir()?;
    let fixtures = crate::fixtures::cache_dir()?;
    let mut build = cargo_command();
    build.args(build_args()).current_dir(ctx.root);
    eprintln!("accept m3: building diar_eval (release)");
    let out = run_with_timeout(&mut build, BUILD_TIMEOUT)
        .context("building diar_eval did not finish within the deadline")?;
    if !out.success {
        bail!("building diar_eval failed: {}", out.describe());
    }
    out.require_complete("building diar_eval")?;
    let exe = example_executable(&out.stdout).context("cargo reported no diar_eval executable")?;

    let mut cmd = std::process::Command::new(&exe);
    cmd.arg("--root")
        .arg(ctx.root)
        .arg("--models")
        .arg(&models)
        .arg("--fixtures")
        .arg(&fixtures)
        .current_dir(ctx.root);
    eprintln!(
        "accept m3: running {} (this takes several minutes)",
        exe.display()
    );
    let out = run_with_timeout(&mut cmd, EVAL_TIMEOUT)
        .context("diar_eval did not finish within the deadline")?;
    if !out.success {
        bail!("diar_eval failed: {}", out.describe());
    }
    out.require_complete("diar_eval")?;
    let json: Value = serde_json::from_str(out.stdout.trim())
        .with_context(|| format!("diar_eval printed invalid JSON:\n{}", out.stdout))?;
    evaluate(&json)
}

fn num(v: &Value, key: &str, what: &str) -> Result<f64> {
    v[key]
        .as_f64()
        .filter(|x| x.is_finite())
        .with_context(|| format!("diar_eval: {what} has no finite `{key}`"))
}

fn count(v: &Value, key: &str, what: &str) -> Result<u64> {
    v[key]
        .as_u64()
        .with_context(|| format!("diar_eval: {what} has no count `{key}`"))
}

fn text<'a>(v: &'a Value, key: &str, what: &str) -> Result<&'a str> {
    v[key]
        .as_str()
        .with_context(|| format!("diar_eval: {what} has no `{key}`"))
}

/// Whether `ours` (recomputed) agrees with `theirs` (the example's value).
fn agrees(ours: f64, theirs: f64) -> bool {
    (ours - theirs).abs() <= AGREE * ours.abs().max(1.0)
}

/// The example's metric entry for `id` (exactly one).
fn metric<'a>(metrics: &'a [Value], id: &str) -> Result<&'a Value> {
    let found: Vec<&Value> = metrics.iter().filter(|m| m["id"] == id).collect();
    let [m] = found.as_slice() else {
        bail!("diar_eval: {} results for {id}, expected 1", found.len());
    };
    Ok(m)
}

/// The median (mean of the middle two for an even count).
fn median(values: &[f64]) -> f64 {
    let mut v = values.to_vec();
    v.sort_by(f64::total_cmp);
    let n = v.len();
    if n % 2 == 1 {
        v[n / 2]
    } else {
        0.5 * (v[n / 2 - 1] + v[n / 2])
    }
}

/// Turns the example's JSON into one measurement per M3 threshold id plus checks.
///
/// # Errors
/// A missing, duplicate or extra metric, a missing field, a value that disagrees with the
/// one recomputed from its components, other meetings than the pinned AMI set, an incomplete
/// synthetic or voiceprint set, or a backend other than the CPU.
pub fn evaluate(json: &Value) -> Result<RunOutput> {
    ensure!(
        text(json, "backend", "report")? == "cpu",
        "diar_eval ran on {}, expected cpu",
        json["backend"]
    );
    let metrics = json["metrics"]
        .as_array()
        .context("diar_eval: no metrics array")?;
    if let Some(extra) = metrics
        .iter()
        .find(|m| !METRICS.iter().any(|id| m["id"] == *id))
    {
        bail!("diar_eval: unexpected metric {}", extra["id"]);
    }
    let mut measurements = Vec::new();
    let mut checks = Vec::new();
    let mut push = |id: &str, value: f64, unit: &str, theirs: &Value| -> Result<()> {
        let m = metric(metrics, id)?;
        ensure!(
            text(m, "unit", id)? == unit,
            "diar_eval: {id} reported in {}, expected {unit}",
            m["unit"]
        );
        let reported = num(m, "value", id)?;
        ensure!(
            agrees(value, reported),
            "diar_eval: {id} reported {reported}, but its components give {value}"
        );
        let mut measurement = Measurement::value(id, value, unit);
        measurement.backend = Some("cpu".into());
        measurement.detail = Some(theirs.to_string());
        measurements.push(measurement);
        Ok(())
    };

    // Setup: models, thresholds from the calibrations, placement.
    let model = text(json, "embedding_model", "report")?;
    let id = EmbeddingModelId::new(model);
    // The thresholds are f32 in the example and JSON prints the shortest f32 text, so the
    // comparison is in f32.
    let cluster = num(json, "cluster_threshold", "report")? as f32;
    let vp = num(json, "voiceprint_threshold", "report")? as f32;
    let want_cluster = calibrated_cluster_threshold(&id);
    let want_vp = calibrated_threshold(&id).ok();
    let sessions = json["placement"]
        .as_array()
        .context("diar_eval: no placement")?;
    // ONNX Runtime's verdict as the example computed it (`SessionBackend::kind` and
    // `fully_on_requested`), never a substring of the placement text: a mixed placement
    // names the CPU provider too.
    let on_cpu = sessions
        .iter()
        .all(|p| p["kind"] == "cpu" && p["fully_on_requested"] == true);
    let placement: Vec<String> = sessions
        .iter()
        .map(|p| {
            format!(
                "{}: {}",
                p["session"].as_str().unwrap_or("?"),
                p["placement"]
            )
        })
        .collect();
    ensure!(
        placement.len() == 2,
        "diar_eval: expected two ONNX sessions, got {placement:?}"
    );
    checks.push(Check::new(
        "clustering threshold is the dev-clean calibration for the embedding model",
        want_cluster == Some(cluster),
        format!("{model}: used {cluster}, calibrated {want_cluster:?}"),
    ));
    checks.push(Check::new(
        "voiceprint threshold is the dev-clean calibration for the embedding model",
        want_vp == Some(vp),
        format!("{model}: used {vp}, calibrated {want_vp:?}"),
    ));
    checks.push(Check::new(
        "speaker models on the CPU (ONNX Runtime node placement)",
        on_cpu,
        format!(
            "segmentation {}, {} ONNX threads ({} cores); {}",
            text(json, "segmentation_model", "report")?,
            count(json, "speaker_threads", "report")?,
            count(json, "cores", "report")?,
            placement.join("; ")
        ),
    ));

    // AMI DER per condition, recomputed from the per-meeting components.
    let ami = json["ami"].as_array().context("diar_eval: no ami rows")?;
    let mut count_items: Vec<(String, f64)> = Vec::new();
    for (mid, condition) in AMI_METRICS {
        let rows: Vec<&Value> = ami.iter().filter(|r| r["condition"] == condition).collect();
        let mut meetings: Vec<&str> = rows
            .iter()
            .map(|r| text(r, "meeting", condition))
            .collect::<Result<_>>()?;
        meetings.sort_unstable();
        let mut pinned = crate::fixtures::AMI_MEETINGS.to_vec();
        pinned.sort_unstable();
        ensure!(
            meetings == pinned,
            "diar_eval: {condition} scored meetings {meetings:?}, expected the pinned AMI set \
             {pinned:?}"
        );
        let (mut errors, mut total) = (0.0, 0.0);
        let mut lines = Vec::new();
        for r in &rows {
            let meeting = text(r, "meeting", condition)?;
            let what = format!("{meeting} {condition}");
            ensure!(
                r["metric"] == mid,
                "diar_eval: {what} row is for {}",
                r["metric"]
            );
            let d = &r["der"];
            let (m, fa, c, t) = (
                num(d, "missed_s", &what)?,
                num(d, "false_alarm_s", &what)?,
                num(d, "confusion_s", &what)?,
                num(d, "total_s", &what)?,
            );
            ensure!(t > 0.0, "diar_eval: {what} has no scored reference speech");
            errors += m + fa + c;
            total += t;
            let (found, truth) = (
                count(r, "found_speakers", &what)?,
                count(r, "true_speakers", &what)?,
            );
            count_items.push((format!("AMI {what}"), found.abs_diff(truth) as f64));
            lines.push(format!(
                "{meeting}: DER {:.2}% (missed {:.2}%, false alarm {:.2}%, confusion {:.2}%) \
                 over {t:.1} s scored, {found} speakers found of {truth} (audio {})",
                100.0 * (m + fa + c) / t,
                100.0 * m / t,
                100.0 * fa / t,
                100.0 * c / t,
                text(r, "audio", &what)?,
            ));
        }
        let value = 100.0 * errors / total;
        push(
            mid,
            value,
            "%",
            &Value::Array(rows.iter().map(|r| (*r).clone()).collect()),
        )?;
        checks.push(Check::new(
            &format!("{mid} per meeting ({condition}; only_words RTTM, UEM, 0.25 s collar)"),
            true,
            format!(
                "corpus DER {value:.2}% over {total:.1} s; {}",
                lines.join("; ")
            ),
        ));
    }
    ensure!(
        ami.len() == AMI_METRICS.len() * crate::fixtures::AMI_MEETINGS.len(),
        "diar_eval: {} AMI rows, expected one per meeting and condition",
        ami.len()
    );

    // Count error: every synthetic scene plus the AMI diarisations.
    let synthetic = &json["synthetic"];
    let scenes = synthetic["scenes"]
        .as_array()
        .context("diar_eval: no synthetic scenes")?;
    let mut per_k: BTreeMap<u64, Vec<f64>> = BTreeMap::new();
    let mut seeds = Vec::new();
    for s in scenes {
        let k = count(s, "speakers", "synthetic scene")?;
        let what = format!("synthetic scene k={k} s={}", s["scene"]);
        let found = count(s, "found_speakers", &what)?;
        per_k.entry(k).or_default().push(found.abs_diff(k) as f64);
        count_items.push((what.clone(), found.abs_diff(k) as f64));
        seeds.push(count(s, "seed", &what)?);
    }
    let per_count: Vec<usize> = per_k.values().map(Vec::len).collect();
    ensure!(
        per_k.keys().copied().eq(SCENE_COUNTS)
            && per_count.windows(2).all(|w| w[0] == w[1])
            && per_count.first().is_some_and(|&n| n > 0),
        "diar_eval: synthetic scenes must cover every speaker count {SCENE_COUNTS:?} equally, \
         got {per_count:?} for {:?}",
        per_k.keys().collect::<Vec<_>>()
    );
    let cm = metric(metrics, "dia.count_error")?;
    let items = cm["items"]
        .as_array()
        .context("diar_eval: dia.count_error has no items")?;
    ensure!(
        items.len() == count_items.len(),
        "diar_eval: dia.count_error lists {} items, the scenes and AMI rows give {}",
        items.len(),
        count_items.len()
    );
    for it in items {
        let what = text(it, "source", "count item")?;
        let (t, f) = (count(it, "true", what)?, count(it, "found", what)?);
        ensure!(
            agrees(num(it, "error", what)?, t.abs_diff(f) as f64),
            "diar_eval: count item {what} has error {} for {f} found of {t}",
            it["error"]
        );
    }
    let errors: Vec<f64> = count_items.iter().map(|(_, e)| *e).collect();
    let count_error = median(&errors);
    push("dia.count_error", count_error, "count", cm)?;
    // Large meetings: the mean over the 4-6 speaker scenes, from the scenes themselves; the
    // example's item list must name exactly those scenes with the same counts.
    let large: Vec<f64> = per_k
        .iter()
        .filter(|(k, _)| LARGE_COUNTS.contains(k))
        .flat_map(|(_, v)| v.iter().copied())
        .collect();
    let lm = metric(metrics, "dia.count_error_large")?;
    let large_items = lm["items"]
        .as_array()
        .context("diar_eval: dia.count_error_large has no items")?;
    let mut listed = Vec::new();
    for it in large_items {
        let what = text(it, "source", "large count item")?;
        let (t, f) = (count(it, "true", what)?, count(it, "found", what)?);
        ensure!(
            LARGE_COUNTS.contains(&t) && what.starts_with("synthetic"),
            "diar_eval: dia.count_error_large lists {what} with {t} speakers"
        );
        ensure!(
            agrees(num(it, "error", what)?, t.abs_diff(f) as f64),
            "diar_eval: large count item {what} has error {} for {f} found of {t}",
            it["error"]
        );
        listed.push(t.abs_diff(f) as f64);
    }
    let mut want = large.clone();
    want.sort_by(f64::total_cmp);
    listed.sort_by(f64::total_cmp);
    ensure!(
        listed == want,
        "diar_eval: dia.count_error_large lists errors {listed:?}, the 4-6 speaker scenes give \
         {want:?}"
    );
    let count_error_large = large.iter().sum::<f64>() / large.len() as f64;
    push("dia.count_error_large", count_error_large, "count", lm)?;
    let mut histogram: BTreeMap<u64, usize> = BTreeMap::new();
    for e in &errors {
        *histogram.entry(*e as u64).or_default() += 1;
    }
    let ami_counts: Vec<String> = count_items
        .iter()
        .filter(|(w, _)| w.starts_with("AMI"))
        .map(|(w, e)| format!("{w} {e}"))
        .collect();
    checks.push(Check::new(
        "dia.count_error distribution (|found - true| over synthetic scenes and AMI)",
        true,
        format!(
            "median {count_error} over {} items; histogram {}; synthetic mean per k {}; \
             mean over k = 4-6 {count_error_large:.3} ({} scenes); {}",
            errors.len(),
            histogram
                .iter()
                .map(|(e, n)| format!("{e}: {n}"))
                .collect::<Vec<_>>()
                .join(", "),
            per_k
                .iter()
                .map(|(k, v)| format!("{k}: {:.2}", v.iter().sum::<f64>() / v.len() as f64))
                .collect::<Vec<_>>()
                .join(", "),
            large.len(),
            ami_counts.join(", ")
        ),
    ));
    checks.push(Check::new(
        "informational: synthetic scene DER (test-clean speakers, no UEM, 0.25 s collar)",
        true,
        format!(
            "{:.2}% over {} scenes; seeds {seeds:?}; speakers {}",
            num(synthetic, "der_percent", "synthetic")?,
            scenes.len(),
            synthetic["speakers"]
        ),
    ));

    // Voiceprints, recomputed from the tallies.
    let mut vp_detail = String::new();
    for id in ["vp.top1", "vp.false_accept"] {
        let m = metric(metrics, id)?;
        let t = &m["tally"];
        let c = |k: &str| count(t, k, id);
        let (known, correct, wrong, rejected, closed) = (
            c("known")?,
            c("known_correct")?,
            c("known_wrong_person")?,
            c("known_rejected")?,
            c("known_closed_set_top1")?,
        );
        let (unknown, accepted) = (c("unknown")?, c("unknown_accepted")?);
        ensure!(
            known == VP_CLIPS_PER_ROLE && unknown == VP_CLIPS_PER_ROLE,
            "diar_eval: {id} scored {known} enrolled and {unknown} unknown clips, expected \
             {VP_CLIPS_PER_ROLE} each"
        );
        ensure!(
            correct + wrong + rejected == known && closed >= correct && accepted <= unknown,
            "diar_eval: {id} tally is inconsistent: {t}"
        );
        let value = if id == "vp.top1" {
            100.0 * correct as f64 / known as f64
        } else {
            100.0 * accepted as f64 / unknown as f64
        };
        push(id, value, "%", m)?;
        vp_detail = format!(
            "threshold {vp}; enrolled clips matched to their own person {correct}/{known}, \
             to another person {wrong}, rejected as unknown {rejected} (closed-set top-1 \
             {closed}/{known}); unknown clips accepted {accepted}/{unknown}"
        );
    }
    let folds = json["voiceprints"]["folds"]
        .as_array()
        .context("diar_eval: no voiceprint folds")?;
    checks.push(Check::new(
        "vp.* tallies (test-clean, 2 folds of 20 enrolled / 20 unknown, 2.5 s clips)",
        true,
        format!(
            "{vp_detail}; folds {}",
            folds
                .iter()
                .map(|f| format!(
                    "{}: enrolled {} unknown {}",
                    f["fold"], f["enrolled"], f["unknown"]
                ))
                .collect::<Vec<_>>()
                .join("; ")
        ),
    ));
    let ids: Vec<&str> = measurements.iter().map(|m| m.id.as_str()).collect();
    ensure!(ids.len() == METRICS.len(), "diar_eval: measured {ids:?}");
    Ok(RunOutput {
        measurements,
        checks,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn is_example(p: &std::path::Path) -> bool {
        p.file_stem().is_some_and(|s| s == "diar_eval")
    }

    const MODEL: &str = "campplus-en-voxceleb@357a834f702b";

    fn der(m: f64, fa: f64, c: f64, t: f64) -> Value {
        json!({"missed_s": m, "false_alarm_s": fa, "confusion_s": c, "total_s": t,
               "der_percent": 100.0 * (m + fa + c) / t})
    }

    fn ami_row(meeting: &str, condition: &str, metric: &str, d: Value, found: u64) -> Value {
        json!({"metric": metric, "meeting": meeting, "condition": condition,
               "audio": format!("ami/audio/{meeting}.x.wav"), "true_speakers": 4,
               "found_speakers": found, "count_error": found.abs_diff(4), "der": d})
    }

    fn tally(correct: u64, wrong: u64, accepted: u64) -> Value {
        json!({"known": 120, "known_correct": correct, "known_wrong_person": wrong,
               "known_rejected": 120 - correct - wrong, "known_closed_set_top1": correct + 1,
               "unknown": 120, "unknown_accepted": accepted})
    }

    /// A consistent report: IHM DER (10+5+5)/200 + (0+0+10)/100 -> 30/300 = 10%; SDM 20%;
    /// count errors 0 for k = 1..=6 (two scenes each, one with error 1 for k = 6) and AMI
    /// 0, 1, 2, 0 -> median 0; large-meeting mean over the six k = 4-6 scenes 1/6; vp top-1 114/120 = 95%, false accept 6/120 = 5%.
    fn sample() -> Value {
        let mut scenes = Vec::new();
        for k in 1..=6u64 {
            for s in 0..2u64 {
                let found = if k == 6 && s == 1 { 5 } else { k };
                scenes.push(
                    json!({"speakers": k, "scene": s, "seed": 35_000 + 100 * k + s,
                                   "found_speakers": found, "der": der(1.0, 1.0, 1.0, 30.0)}),
                );
            }
        }
        let mut items: Vec<Value> = scenes
            .iter()
            .map(|s| {
                let (k, f) = (
                    s["speakers"].as_u64().unwrap(),
                    s["found_speakers"].as_u64().unwrap(),
                );
                json!({"source": "synthetic", "true": k, "found": f, "error": k.abs_diff(f)})
            })
            .collect();
        let large: Vec<Value> = items
            .iter()
            .filter(|it| LARGE_COUNTS.contains(&it["true"].as_u64().unwrap()))
            .cloned()
            .collect();
        let ami = vec![
            ami_row(
                "IS1009a",
                "ihm_mix",
                "dia.der_ami_ihm",
                der(10.0, 5.0, 5.0, 200.0),
                4,
            ),
            ami_row(
                "ES2004a",
                "ihm_mix",
                "dia.der_ami_ihm",
                der(0.0, 0.0, 10.0, 100.0),
                5,
            ),
            ami_row(
                "IS1009a",
                "sdm",
                "dia.der_ami_sdm",
                der(20.0, 10.0, 10.0, 200.0),
                6,
            ),
            ami_row(
                "ES2004a",
                "sdm",
                "dia.der_ami_sdm",
                der(10.0, 5.0, 5.0, 100.0),
                4,
            ),
        ];
        for r in &ami {
            let f = r["found_speakers"].as_u64().unwrap();
            items.push(json!({"source": "AMI", "true": 4, "found": f, "error": f.abs_diff(4)}));
        }
        json!({
            "backend": "cpu",
            "embedding_model": MODEL,
            "segmentation_model": "pyannote-segmentation-3.0@x",
            "speaker_threads": 4, "cores": 32,
            "placement": [
                {"session": "segmentation", "placement": "requested cpu, placed: CPUExecutionProvider=10",
                 "kind": "cpu", "fully_on_requested": true},
                {"session": "embedding", "placement": "requested cpu, placed: CPUExecutionProvider=20",
                 "kind": "cpu", "fully_on_requested": true}
            ],
            "cluster_threshold": f64::from(earshot_core::diar::cluster::CAMPPLUS_DEV_CLEAN_CLUSTER_THRESHOLD),
            "voiceprint_threshold": f64::from(earshot_core::voiceprint::CAMPPLUS_DEV_CLEAN_THRESHOLD),
            "ami": ami,
            "synthetic": {"der_percent": 10.0, "speakers": [1, 2], "scenes": scenes},
            "voiceprints": {"folds": [{"fold": 0, "enrolled": [1], "unknown": [2]},
                                      {"fold": 1, "enrolled": [2], "unknown": [1]}]},
            "metrics": [
                {"id": "dia.der_ami_ihm", "unit": "%", "value": 10.0},
                {"id": "dia.der_ami_sdm", "unit": "%", "value": 20.0},
                {"id": "dia.count_error", "unit": "count", "value": 0.0, "items": items},
                {"id": "dia.count_error_large", "unit": "count", "value": 1.0 / 6.0,
                 "items": large},
                {"id": "vp.top1", "unit": "%", "value": 95.0, "tally": tally(114, 2, 6)},
                {"id": "vp.false_accept", "unit": "%", "value": 5.0, "tally": tally(114, 2, 6)}
            ]
        })
    }

    fn index(j: &Value, id: &str) -> usize {
        j["metrics"]
            .as_array()
            .unwrap()
            .iter()
            .position(|m| m["id"] == id)
            .unwrap()
    }

    #[test]
    fn m3_evaluate_recomputes_one_measurement_per_threshold() {
        let out = evaluate(&sample()).unwrap();
        let got: Vec<(&str, f64, &str)> = out
            .measurements
            .iter()
            .map(|m| (m.id.as_str(), m.value.unwrap(), m.unit.as_str()))
            .collect();
        assert_eq!(
            got,
            [
                ("dia.der_ami_ihm", 10.0, "%"),
                ("dia.der_ami_sdm", 20.0, "%"),
                ("dia.count_error", 0.0, "count"),
                ("dia.count_error_large", 1.0 / 6.0, "count"),
                ("vp.top1", 95.0, "%"),
                ("vp.false_accept", 5.0, "%"),
            ]
        );
        assert!(
            out.measurements
                .iter()
                .all(|m| m.backend.as_deref() == Some("cpu"))
        );
        assert!(out.checks.iter().all(|c| c.pass), "{:?}", out.checks);
        let detail = |name: &str| {
            out.checks
                .iter()
                .find(|c| c.name.contains(name))
                .and_then(|c| c.detail.clone())
                .unwrap_or_else(|| panic!("no check {name}"))
        };
        let ihm = detail("dia.der_ami_ihm per meeting");
        assert!(
            ihm.contains("IS1009a: DER 10.00% (missed 5.00%, false alarm 2.50%, confusion 2.50%)"),
            "{ihm}"
        );
        assert!(ihm.contains("ES2004a: DER 10.00%"), "{ihm}");
        let dist = detail("count_error distribution");
        assert!(dist.contains("median 0 over 16 items"), "{dist}");
        assert!(dist.contains("histogram 0: 13, 1: 2, 2: 1"), "{dist}");
        assert!(dist.contains("6: 0.50"), "{dist}");
        assert!(
            dist.contains("mean over k = 4-6 0.167 (6 scenes)"),
            "{dist}"
        );
        let vp = detail("vp.* tallies");
        assert!(vp.contains("own person 114/120"), "{vp}");
        assert!(vp.contains("unknown clips accepted 6/120"), "{vp}");
        assert!(detail("informational: synthetic").contains("seeds [35100, 35101"));
    }

    #[test]
    fn m3_der_is_summed_over_meetings_not_averaged() {
        // Per-meeting IHM rates 30/200 = 15% and 10/100 = 10%: their mean is 12.5%, the
        // summed components give 40/300.
        let mut j = sample();
        j["ami"][0]["der"] = der(10.0, 10.0, 10.0, 200.0);
        let k = index(&j, "dia.der_ami_ihm");
        j["metrics"][k]["value"] = json!(12.5);
        assert!(evaluate(&j).is_err());
        j["metrics"][k]["value"] = json!(100.0 * 40.0 / 300.0);
        let out = evaluate(&j).unwrap();
        assert!((out.measurements[0].value.unwrap() - 13.333_333_333_333_334).abs() < 1e-9);
    }

    #[test]
    fn m3_reported_values_must_match_their_components() {
        for (id, bad) in [
            ("dia.der_ami_ihm", 9.0),
            ("dia.der_ami_sdm", 19.0),
            ("dia.count_error", 1.0),
            ("dia.count_error_large", 0.0),
            ("vp.top1", 96.0),
            ("vp.false_accept", 4.0),
        ] {
            let mut j = sample();
            let k = index(&j, id);
            j["metrics"][k]["value"] = json!(bad);
            assert!(evaluate(&j).is_err(), "{id}");
        }
        // An item whose error disagrees with its counts.
        let mut j = sample();
        let k = index(&j, "dia.count_error");
        j["metrics"][k]["items"][0]["error"] = json!(3);
        assert!(evaluate(&j).is_err());
        // An item dropped from the list.
        let mut j = sample();
        j["metrics"][k]["items"].as_array_mut().unwrap().pop();
        assert!(evaluate(&j).is_err());
    }

    #[test]
    fn m3_count_median_uses_every_scene_and_ami_row() {
        // Raise one AMI error to 3: errors 0 x 12, 1 x 2, 2 x 1, 3 x 1 -> 16 items, median
        // of the 8th and 9th smallest = 0.
        let mut j = sample();
        j["ami"][0]["found_speakers"] = json!(7);
        let k = index(&j, "dia.count_error");
        let items = j["metrics"][k]["items"].as_array_mut().unwrap();
        let n = items.len();
        items[n - 4]["found"] = json!(7);
        items[n - 4]["error"] = json!(3);
        assert_eq!(evaluate(&j).unwrap().measurements[2].value, Some(0.0));
        // Make most synthetic scenes miss by 2: the median moves.
        let mut j = sample();
        let scenes = j["synthetic"]["scenes"].as_array_mut().unwrap();
        for s in scenes.iter_mut().take(10) {
            let kk = s["speakers"].as_u64().unwrap();
            s["found_speakers"] = json!(kk + 2);
        }
        let items = j["metrics"][k]["items"].as_array_mut().unwrap();
        for it in items.iter_mut().take(10) {
            let kk = it["true"].as_u64().unwrap();
            it["found"] = json!(kk + 2);
            it["error"] = json!(2);
        }
        // Errors: 2 x 10, 1 (k=6 s=1), 0, then AMI 0, 1, 2, 0 -> sorted median of 16 = 2.
        j["metrics"][k]["value"] = json!(2.0);
        // The large-meeting list follows the same scenes: k = 4, 5 now miss by 2, k = 6 by
        // 0 and 1 -> (2 x 4 + 1) / 6.
        let large: Vec<Value> = j["metrics"][k]["items"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|it| {
                it["source"] == "synthetic" && LARGE_COUNTS.contains(&it["true"].as_u64().unwrap())
            })
            .cloned()
            .collect();
        let m = index(&j, "dia.count_error_large");
        j["metrics"][m]["items"] = json!(large);
        j["metrics"][m]["value"] = json!(9.0 / 6.0);
        let out = evaluate(&j).unwrap();
        assert_eq!(out.measurements[2].value, Some(2.0));
        assert_eq!(out.measurements[3].value, Some(1.5));
    }

    #[test]
    fn m3_large_count_error_sees_merged_large_meetings_the_median_hides() {
        // Every 4-6 speaker scene found two speakers too few: the median over all 16 items
        // stays 0 (10 of them are still exact), the large-meeting mean is 2.
        let mut j = sample();
        for s in j["synthetic"]["scenes"].as_array_mut().unwrap() {
            let k = s["speakers"].as_u64().unwrap();
            if LARGE_COUNTS.contains(&k) {
                s["found_speakers"] = json!(k - 2);
            }
        }
        for id in ["dia.count_error", "dia.count_error_large"] {
            let m = index(&j, id);
            for it in j["metrics"][m]["items"].as_array_mut().unwrap() {
                let k = it["true"].as_u64().unwrap();
                if it["source"] == "synthetic" && LARGE_COUNTS.contains(&k) {
                    it["found"] = json!(k - 2);
                    it["error"] = json!(2);
                }
            }
        }
        let k = index(&j, "dia.count_error");
        j["metrics"][k]["value"] = json!(0.5);
        let m = index(&j, "dia.count_error_large");
        j["metrics"][m]["value"] = json!(2.0);
        let out = evaluate(&j).unwrap();
        let value = |id: &str| {
            out.measurements
                .iter()
                .find(|x| x.id == id)
                .and_then(|x| x.value)
                .unwrap()
        };
        assert_eq!(value("dia.count_error"), 0.5);
        assert_eq!(value("dia.count_error_large"), 2.0);
    }

    #[test]
    fn m3_large_count_items_must_be_exactly_the_large_scenes() {
        let m = index(&sample(), "dia.count_error_large");
        // A scene dropped from the list.
        let mut j = sample();
        j["metrics"][m]["items"].as_array_mut().unwrap().pop();
        assert!(evaluate(&j).is_err());
        // A small scene listed in place of a large one.
        let mut j = sample();
        j["metrics"][m]["items"][0] =
            json!({"source": "synthetic", "true": 2, "found": 2, "error": 0});
        assert!(evaluate(&j).is_err());
        // An AMI diarisation listed in place of a scene.
        let mut j = sample();
        j["metrics"][m]["items"][0] = json!({"source": "AMI", "true": 4, "found": 4, "error": 0});
        assert!(evaluate(&j).is_err());
        // An item whose error disagrees with its counts, and one whose counts disagree with
        // the scenes.
        let mut j = sample();
        j["metrics"][m]["items"][0]["error"] = json!(3);
        assert!(evaluate(&j).is_err());
        let mut j = sample();
        j["metrics"][m]["items"][0]["found"] = json!(1);
        j["metrics"][m]["items"][0]["error"] = json!(3);
        assert!(evaluate(&j).is_err());
    }

    #[test]
    fn m3_rejects_missing_duplicate_extra_metrics_and_bad_data() {
        let mut j = sample();
        j["metrics"].as_array_mut().unwrap().pop();
        assert!(evaluate(&j).is_err());
        let mut j = sample();
        let first = j["metrics"][0].clone();
        j["metrics"].as_array_mut().unwrap().push(first);
        assert!(evaluate(&j).is_err());
        let mut j = sample();
        j["metrics"]
            .as_array_mut()
            .unwrap()
            .push(json!({"id": "dia.extra", "unit": "%", "value": 1.0}));
        assert!(evaluate(&j).is_err());
        // Wrong unit.
        let mut j = sample();
        j["metrics"][2]["unit"] = json!("%");
        assert!(evaluate(&j).is_err());
        // Another meeting than the pinned set, or one missing.
        let mut j = sample();
        j["ami"][1]["meeting"] = json!("ES2004b");
        assert!(evaluate(&j).is_err());
        let mut j = sample();
        j["ami"].as_array_mut().unwrap().remove(3);
        assert!(evaluate(&j).is_err());
        // A speaker count without scenes, or uneven scene counts.
        let mut j = sample();
        j["synthetic"]["scenes"]
            .as_array_mut()
            .unwrap()
            .retain(|s| s["speakers"] != 1);
        assert!(evaluate(&j).is_err());
        let mut j = sample();
        j["synthetic"]["scenes"].as_array_mut().unwrap().remove(0);
        assert!(evaluate(&j).is_err());
        // Voiceprint clips missing, or an inconsistent tally.
        for id in ["vp.top1", "vp.false_accept"] {
            let mut j = sample();
            let k = index(&j, id);
            j["metrics"][k]["tally"]["unknown"] = json!(119);
            assert!(evaluate(&j).is_err(), "{id}");
            let mut j = sample();
            j["metrics"][k]["tally"]["known_rejected"] = json!(0);
            assert!(evaluate(&j).is_err(), "{id}");
        }
        // Another backend.
        let mut j = sample();
        j["backend"] = json!("cuda");
        assert!(evaluate(&j).is_err());
        // Non-finite or missing DER components.
        let mut j = sample();
        j["ami"][0]["der"]["missed_s"] = Value::Null;
        assert!(evaluate(&j).is_err());
    }

    #[test]
    fn m3_thresholds_must_be_the_calibrations() {
        let failing = |j: &Value| -> Vec<String> {
            evaluate(j)
                .unwrap()
                .checks
                .into_iter()
                .filter(|c| !c.pass)
                .map(|c| c.name)
                .collect()
        };
        let mut j = sample();
        j["cluster_threshold"] = json!(0.7);
        assert_eq!(
            failing(&j),
            ["clustering threshold is the dev-clean calibration for the embedding model"]
        );
        let mut j = sample();
        j["voiceprint_threshold"] = json!(0.4);
        assert_eq!(
            failing(&j),
            ["voiceprint threshold is the dev-clean calibration for the embedding model"]
        );
        // An uncalibrated model fails both.
        let mut j = sample();
        j["embedding_model"] = json!("other@1");
        assert_eq!(failing(&j).len(), 2);
        // A session placed elsewhere.
        let mut j = sample();
        j["placement"][1]["placement"] = json!("requested cuda, placed: CUDAExecutionProvider=20");
        j["placement"][1]["kind"] = json!("cuda");
        assert_eq!(
            failing(&j),
            ["speaker models on the CPU (ONNX Runtime node placement)"]
        );
        // A mixed placement names the CPU provider too; the verdict fields decide.
        let mut j = sample();
        j["placement"][0]["placement"] =
            json!("requested cuda, placed: CPUExecutionProvider=4 CUDAExecutionProvider=6");
        j["placement"][0]["fully_on_requested"] = json!(false);
        assert_eq!(
            failing(&j),
            ["speaker models on the CPU (ONNX Runtime node placement)"]
        );
        // A report without the verdict fields fails the check.
        let mut j = sample();
        j["placement"][1]
            .as_object_mut()
            .unwrap()
            .remove("fully_on_requested");
        assert_eq!(
            failing(&j),
            ["speaker models on the CPU (ONNX Runtime node placement)"]
        );
    }

    #[test]
    fn m3_runs_on_the_cpu_only_and_builds_without_features() {
        assert!(check_backend(None).is_ok());
        assert!(check_backend(Some("cpu")).is_ok());
        assert!(check_backend(Some("cuda")).is_err());
        let args = build_args();
        assert!(args.contains(&"--release"));
        assert!(!args.contains(&"--features"));
        assert!(args.windows(2).any(|w| w == ["--example", "diar_eval"]));
    }

    #[test]
    fn m3_example_executable_is_read_from_cargo_messages() {
        let msgs = [
            r#"{"reason":"compiler-artifact","target":{"name":"earshot_core"},"executable":null}"#,
            "not json",
            r#"{"reason":"compiler-artifact","target":{"name":"diar_eval"},"executable":"C:\\t\\release\\examples\\diar_eval.exe"}"#,
        ]
        .join("\n");
        let exe = example_executable(&msgs).unwrap();
        assert!(is_example(&exe), "{}", exe.display());
        assert_eq!(example_executable("{}\n"), None);
    }
}
