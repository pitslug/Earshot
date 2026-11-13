//! `cargo xtask vulkan-parity` (T8.2): WER parity of whisper.cpp on Vulkan with the CPU, the
//! plan's rule: within [`MAX_POINTS`] points on the same corpus.
//!
//! The measuring is `cargo xtask model-eval` (`--corpus librispeech --decoding greedy`, the
//! pinned test-clean subset, one piece run per utterance, the production decoding path) on
//! every model of the manifest's Vulkan ladders, once per backend, into
//! [`REPORT_DIR`]; a Vulkan report per device is allowed (`vulkan-<device>.json`, measured
//! with `GGML_VK_VISIBLE_DEVICES` by `--device <device>=<n>`). Judging reads those reports and
//! nothing else, so it can
//! be repeated (`--skip-run`) and the committed reports stay pinned to the rule by a plain
//! test (`vulkan_parity_committed_reports_hold_the_rule`).
//!
//! The backend comes from whisper.cpp's log as the report recorded it, never from what was
//! asked for: `backend_info` and the log's classification must both say the row's backend,
//! and a Vulkan row must hold `using Vulkan<n> backend` and `Vulkan<n> total size` for one
//! device (the rule of `earshot_asr_whisper::evidence`). A missing row, a report the run did
//! not finish, or corpora of different sizes fail: a parity not measured is not a pass.
//! WER does not depend on host load, so unlike a timing this holds on a shared host.
//!
//! A host with no Vulkan device fails the Vulkan run; that host's check is compile-only:
//! `cargo xtask build --features vulkan`.
//!
//! Only WER is judged. The reports also hold the evaluation's speed fields, which say nothing
//! about the host's load: the committed ones were measured on a shared host (docs/asr-models.md),
//! so read them as upper bounds, never as a figure or baseline (the T8.1 rule).

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail, ensure};
use serde_json::Value;

use crate::manifest::Manifest;
use crate::model_eval;

/// The plan's limit (T8.2), in WER points.
pub const MAX_POINTS: f64 = 1.0;
/// Where the reports live, from the repo root.
pub const REPORT_DIR: &str = "docs/model-eval/vulkan-parity";
/// The corpus.
const CORPUS: &str = "librispeech";
/// The decoding (the live and single-pass decoding, D20).
const DECODING: &str = "greedy";

/// One model's comparison.
#[derive(Debug, Clone, PartialEq)]
pub struct Row {
    /// The model.
    pub model: String,
    /// The Vulkan device whisper.cpp reported.
    pub device: String,
    /// CPU WER (%).
    pub cpu: f64,
    /// Vulkan WER (%).
    pub vulkan: f64,
    /// Reference words of each.
    pub ref_words: u64,
}

impl Row {
    /// Vulkan minus the CPU, in points.
    #[must_use]
    pub fn delta(&self) -> f64 {
        self.vulkan - self.cpu
    }

    /// Whether it holds the rule.
    #[must_use]
    pub fn holds(&self) -> bool {
        self.delta().abs() <= MAX_POINTS
    }
}

/// The models to measure: every model of the Vulkan ladders in `manifest`, in ladder order.
#[must_use]
pub fn ladder_models(manifest: &Manifest) -> Vec<String> {
    let mut models = Vec::new();
    for ladder in manifest.ladders.iter().filter(|l| l.backend == "vulkan") {
        for m in &ladder.models {
            if !models.contains(m) {
                models.push(m.clone());
            }
        }
    }
    models
}

/// `model`'s greedy row in `report`.
fn row_of<'a>(report: &'a Value, model: &str, what: &str) -> Result<&'a Value> {
    let rows = report["rows"]
        .as_array()
        .with_context(|| format!("the {what} report has no rows"))?;
    let mut found = rows
        .iter()
        .filter(|r| r["model"] == model && r["decoding"] == DECODING);
    let row = found
        .next()
        .with_context(|| format!("the {what} report has no {DECODING} row for {model}"))?;
    ensure!(
        found.next().is_none(),
        "the {what} report has two {DECODING} rows for {model}"
    );
    Ok(row)
}

/// The proof in `row` that it ran on `kind`, from the recorded whisper.cpp log.
fn check_backend(row: &Value, kind: &str, model: &str) -> Result<()> {
    let info = &row["backend_info"]["kind"];
    let log = &row["evidence"]["whisper_log"];
    ensure!(
        info == kind && log == kind,
        "{model}: asked for {kind}; backend_info says {info}, whisper.cpp's log {log}"
    );
    if kind == "vulkan" {
        let lines: Vec<&str> = row["evidence"]["lines"]
            .as_array()
            .map(|l| l.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default();
        let device = lines
            .iter()
            .find_map(|l| {
                let rest = l.strip_prefix("whisper_backend_init_gpu: using ")?;
                rest.strip_suffix(" backend")
            })
            .with_context(|| format!("{model}: no `using Vulkan<n> backend` line in its log"))?;
        ensure!(
            device.starts_with("Vulkan"),
            "{model}: whisper.cpp used {device}"
        );
        ensure!(
            lines.iter().any(|l| l.starts_with("whisper_model_load:")
                && l.contains(&format!("{device} total size"))),
            "{model}: no `{device} total size` line in its log"
        );
    }
    Ok(())
}

fn wer_of(row: &Value, model: &str) -> Result<(f64, u64)> {
    let wer = &row["corpora"][CORPUS]["wer"];
    let percent = wer["wer_percent"]
        .as_f64()
        .with_context(|| format!("{model}: no {CORPUS} WER"))?;
    let words = wer["ref_words"]
        .as_u64()
        .with_context(|| format!("{model}: no {CORPUS} reference words"))?;
    Ok((percent, words))
}

/// Compares `vulkan` with `cpu` on `models`.
///
/// # Errors
/// An unfinished report, a missing row or WER, a row without proof of its backend, or corpora
/// of different sizes. A model over the limit is not an error: see [`Row::holds`].
pub fn judge(cpu: &Value, vulkan: &Value, models: &[String]) -> Result<Vec<Row>> {
    ensure!(!models.is_empty(), "no Vulkan ladder to judge");
    for (report, what) in [(cpu, "cpu"), (vulkan, "vulkan")] {
        ensure!(
            report["complete"] == true,
            "the {what} report did not finish"
        );
        ensure!(
            report["backend"] == what,
            "the {what} report is {}",
            report["backend"]
        );
    }
    models
        .iter()
        .map(|model| {
            let (c, v) = (row_of(cpu, model, "cpu")?, row_of(vulkan, model, "vulkan")?);
            check_backend(c, "cpu", model)?;
            check_backend(v, "vulkan", model)?;
            let (cpu_wer, cpu_words) = wer_of(c, model)?;
            let (vulkan_wer, vulkan_words) = wer_of(v, model)?;
            ensure!(
                cpu_words == vulkan_words && cpu_words > 0,
                "{model}: {cpu_words} reference words on the CPU, {vulkan_words} on Vulkan"
            );
            Ok(Row {
                model: model.clone(),
                device: v["backend_info"]["device"]
                    .as_str()
                    .unwrap_or_default()
                    .to_owned(),
                cpu: cpu_wer,
                vulkan: vulkan_wer,
                ref_words: cpu_words,
            })
        })
        .collect()
}

fn read_report(path: &Path) -> Result<Value> {
    let text = std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    serde_json::from_str(&text).with_context(|| format!("parse {}", path.display()))
}

/// The Vulkan reports in `dir`: `vulkan.json` and `vulkan-<device>.json`, sorted.
///
/// # Errors
/// A folder that cannot be read.
pub fn vulkan_reports(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut found = Vec::new();
    for entry in std::fs::read_dir(dir).with_context(|| format!("read {}", dir.display()))? {
        let path = entry?.path();
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default();
        if name == "vulkan.json" || (name.starts_with("vulkan-") && name.ends_with(".json")) {
            found.push(path);
        }
    }
    found.sort();
    Ok(found)
}

/// Judges every Vulkan report in `dir` against its `cpu.json`.
///
/// # Errors
/// As [`judge`]; no CPU or Vulkan report.
pub fn judge_dir(dir: &Path, models: &[String]) -> Result<Vec<Row>> {
    let cpu = read_report(&dir.join("cpu.json"))?;
    let reports = vulkan_reports(dir)?;
    ensure!(!reports.is_empty(), "no Vulkan report in {}", dir.display());
    let mut rows = Vec::new();
    for path in reports {
        rows.extend(
            judge(&cpu, &read_report(&path)?, models)
                .with_context(|| format!("judging {}", path.display()))?,
        );
    }
    Ok(rows)
}

/// `--device <name>=<n>`: the report name (`vulkan-<name>.json`) and the `Vulkan<n>` index.
///
/// # Errors
/// Another shape, a name that is not lower-case letters, digits and `-`, or an index that is
/// not a number.
pub fn device_run(arg: &str) -> Result<(String, u32)> {
    let Some((name, index)) = arg.split_once('=') else {
        bail!("--device {arg:?} is not <name>=<n>, for example igpu=0");
    };
    ensure!(
        !name.is_empty()
            && name
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-'),
        "--device name {name:?} must be lower-case letters, digits or '-'"
    );
    let index = index
        .parse()
        .with_context(|| format!("--device index {index:?} is not a number"))?;
    Ok((format!("vulkan-{name}.json"), index))
}

/// Measures unless `skip_run` (with `device`, only that Vulkan device, [`device_run`]), then
/// judges and prints the table. Whether every model holds.
///
/// # Errors
/// A failed evaluation; a report that cannot be judged.
pub fn run(
    root: &Path,
    skip_run: bool,
    device: Option<&str>,
    target_dir: Option<PathBuf>,
) -> Result<bool> {
    let manifest = Manifest::load(&crate::models::manifest_path(root))?;
    let models = ladder_models(&manifest);
    let dir = root.join(REPORT_DIR);
    let device = device.map(device_run).transpose()?;
    ensure!(
        !(skip_run && device.is_some()),
        "--device measures a device; --skip-run measures nothing"
    );
    if let Some((report, index)) = &device {
        model_eval::run(
            root,
            &model_eval::Options {
                backend: Some("vulkan".to_owned()),
                models: models.clone(),
                corpora: vec![CORPUS.to_owned()],
                decodings: vec![DECODING.to_owned()],
                out: Some(dir.join(report)),
                target_dir: target_dir.clone(),
                vulkan_device: Some(*index),
            },
        )?;
    } else if !skip_run {
        // Vulkan first: a host without a device fails in minutes, not after the CPU's hours.
        for backend in ["vulkan", "cpu"] {
            model_eval::run(
                root,
                &model_eval::Options {
                    backend: Some(backend.to_owned()),
                    models: models.clone(),
                    corpora: vec![CORPUS.to_owned()],
                    decodings: vec![DECODING.to_owned()],
                    out: Some(dir.join(format!("{backend}.json"))),
                    target_dir: target_dir.clone(),
                    vulkan_device: None,
                },
            )?;
        }
    }
    let rows = judge_dir(&dir, &models)?;
    println!("model            device                     cpu %  vulkan %   delta  rule");
    for r in &rows {
        println!(
            "{:<16} {:<25} {:>6.2} {:>9.2} {:>+7.2}  {}",
            r.model,
            r.device,
            r.cpu,
            r.vulkan,
            r.delta(),
            if r.holds() { "holds" } else { "FAILS" }
        );
    }
    println!(
        "{} reference words; limit {MAX_POINTS} point; speed fields are not judged",
        rows[0].ref_words
    );
    if rows.iter().any(|r| r.ref_words != rows[0].ref_words) {
        bail!("the reports do not share one corpus");
    }
    Ok(rows.iter().all(Row::holds))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn repo() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("repo root")
            .to_path_buf()
    }

    fn row(model: &str, kind: &str, wer: f64) -> Value {
        let lines = if kind == "vulkan" {
            json!([
                "whisper_model_load:      Vulkan1 total size =   147.37 MB",
                "whisper_backend_init_gpu: using Vulkan1 backend"
            ])
        } else {
            json!(["whisper_backend_init_gpu: no GPU found"])
        };
        json!({
            "model": model,
            "decoding": "greedy",
            "backend_info": { "kind": kind, "device": "dev" },
            "evidence": { "whisper_log": kind, "lines": lines },
            "corpora": { "librispeech": { "wer": { "wer_percent": wer, "ref_words": 100 } } },
        })
    }

    fn report(kind: &str, wer: f64) -> Value {
        json!({ "backend": kind, "complete": true, "rows": [row("m", kind, wer)] })
    }

    fn models() -> Vec<String> {
        vec!["m".to_owned()]
    }

    #[test]
    fn vulkan_parity_device_runs_name_their_report_and_index() {
        assert_eq!(
            device_run("igpu=0").unwrap(),
            ("vulkan-igpu.json".to_owned(), 0)
        );
        assert_eq!(
            device_run("rtx-5080=1").unwrap(),
            ("vulkan-rtx-5080.json".to_owned(), 1)
        );
        for bad in [
            "igpu", "=0", "IGPU=0", "i/gpu=0", "igpu=x", "igpu=-1", "igpu=",
        ] {
            assert!(device_run(bad).is_err(), "{bad}");
        }
        // The committed device report is one such name.
        let reports = vulkan_reports(&repo().join(REPORT_DIR)).unwrap();
        assert!(
            reports
                .iter()
                .any(|p| p.file_name().is_some_and(|n| n == "vulkan-igpu.json")),
            "{reports:?}"
        );
    }

    #[test]
    fn vulkan_parity_a_device_reaches_the_evaluation_only_on_vulkan() {
        let opts = |vulkan_device| model_eval::Options {
            backend: None,
            models: Vec::new(),
            corpora: Vec::new(),
            decodings: Vec::new(),
            out: None,
            target_dir: None,
            vulkan_device,
        };
        assert!(
            model_eval::eval_env("vulkan", &opts(None))
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            model_eval::eval_env("vulkan", &opts(Some(0))).unwrap(),
            [(model_eval::VULKAN_DEVICES_ENV, "0".to_owned())]
        );
        assert!(model_eval::eval_env("cpu", &opts(Some(0))).is_err());
        assert!(model_eval::eval_env("cpu", &opts(None)).unwrap().is_empty());
    }

    #[test]
    fn vulkan_parity_holds_within_one_point_either_way() {
        for (vulkan, holds) in [
            (5.0, true),
            (6.0, true),
            (4.0, true),
            (6.5, false),
            (3.5, false),
        ] {
            let rows = judge(&report("cpu", 5.0), &report("vulkan", vulkan), &models()).unwrap();
            assert_eq!(rows[0].holds(), holds, "{vulkan}");
        }
    }

    #[test]
    fn vulkan_parity_needs_the_log_to_prove_the_backend() {
        let cpu = report("cpu", 5.0);
        let judged = |edit: &dyn Fn(&mut Value)| {
            let mut v = report("vulkan", 5.0);
            edit(&mut v["rows"][0]);
            judge(&cpu, &v, &models())
        };
        assert!(judged(&|_| {}).is_ok());
        // The request alone proves nothing: a CPU log under a Vulkan report.
        assert!(judged(&|r| r["evidence"]["whisper_log"] = json!("cpu")).is_err());
        assert!(judged(&|r| r["backend_info"]["kind"] = json!("cpu")).is_err());
        assert!(judged(&|r| r["evidence"]["lines"] = json!([])).is_err());
        // Weights on another device than the one selected.
        assert!(
            judged(&|r| {
                r["evidence"]["lines"] = json!([
                    "whisper_model_load:      Vulkan0 total size =   147.37 MB",
                    "whisper_backend_init_gpu: using Vulkan1 backend"
                ]);
            })
            .is_err()
        );
        assert!(
            judged(&|r| {
                r["evidence"]["lines"] = json!([
                    "whisper_model_load:      CUDA0 total size =   147.37 MB",
                    "whisper_backend_init_gpu: using CUDA0 backend"
                ]);
            })
            .is_err()
        );
    }

    #[test]
    fn vulkan_parity_fails_what_was_not_measured() {
        let (cpu, vulkan) = (report("cpu", 5.0), report("vulkan", 5.0));
        assert!(judge(&cpu, &vulkan, &["other".to_owned()]).is_err());
        assert!(judge(&cpu, &vulkan, &[]).is_err());
        let mut unfinished = vulkan.clone();
        unfinished["complete"] = json!(false);
        assert!(judge(&cpu, &unfinished, &models()).is_err());
        // The two reports swapped.
        assert!(judge(&vulkan, &cpu, &models()).is_err());
        // Different corpora.
        let mut small = vulkan.clone();
        small["rows"][0]["corpora"]["librispeech"]["wer"]["ref_words"] = json!(50);
        assert!(judge(&cpu, &small, &models()).is_err());
        // A beam row is not a greedy row.
        let mut beam = vulkan;
        beam["rows"][0]["decoding"] = json!("beam");
        assert!(judge(&cpu, &beam, &models()).is_err());
    }

    #[test]
    fn vulkan_parity_ladder_comes_from_the_manifest() {
        let manifest = Manifest::load(&crate::models::manifest_path(&repo())).unwrap();
        let models = ladder_models(&manifest);
        assert!(!models.is_empty());
        for ladder in manifest.ladders.iter().filter(|l| l.backend == "vulkan") {
            for m in &ladder.models {
                assert!(models.contains(m), "{m}");
            }
        }
    }

    #[test]
    fn vulkan_parity_committed_reports_hold_the_rule() {
        let root = repo();
        let manifest = Manifest::load(&crate::models::manifest_path(&root)).unwrap();
        let models = ladder_models(&manifest);
        let rows = judge_dir(&root.join(REPORT_DIR), &models).unwrap();
        assert!(rows.len() >= models.len());
        for r in &rows {
            assert!(r.holds(), "{r:?}");
        }
    }
}
