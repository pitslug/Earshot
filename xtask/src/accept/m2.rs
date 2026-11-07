//! `accept m2`: transcription (T2.5, PLAN.md 4.0 and 4.2 `asr.*`).
//!
//! The work runs in two release builds (with `--features cuda` when `cargo xtask doctor`
//! reports the CUDA toolkit and an NVIDIA GPU, otherwise for the CPU): the `asr_eval`
//! example of `earshot-asr-whisper` (the base.en metrics PLAN.md 4.2 names: the smoke and
//! LibriSpeech CPU batch WERs and `asr.echo_leak`), and the `m2_prod` example of
//! `earshot-runtime`, which measures `asr.stream_determinism`, `asr.wer_ls_default`,
//! `asr.wer_ami_ihm` and `asr.wer_aec_delta` on the production setup (the default
//! transcription model and live mode, through the runtime's own constructors; the owner
//! 2026-10-24, and PLAN D22, 2026-11-02: tests and thresholds use only the default models).
//! Metric definitions live in those examples' module docs; each prints JSON with exactly
//! one entry per threshold it measures ([`METRICS`]), which this runner turns into one
//! measurement each. `asr_eval`'s base.en values of the two deltas under their definitions
//! until 2026-10-24 ([`INFORMATIONAL`]) become checks, never measurements.
//!
//! Backends: `asr.wer_smoke_base` and `asr.wer_ls_base` are defined on the CPU (PLAN.md
//! 4.2) and always run there; every other metric runs on the selected backend (CUDA when
//! doctor reports it, otherwise the CPU; `--backend cpu|cuda` overrides, and asking for
//! CUDA on a host without it fails). The backend recorded for each `asr_eval` measurement
//! is the one whisper.cpp's own log proves for that model load, checked against
//! `backend_info()` by the shared rule in `earshot_asr_whisper::evidence`; each `m2_prod`
//! load is proven by ONNX Runtime's node placement (the examples fail otherwise). This
//! runner checks the reported backends and the production model against the plan again and
//! records each load's evidence lines, every metric's decoding settings and the
//! `asr.wer_aec_delta` seed list, near-end word counts and per-seed spread as checks (the
//! committed summary drops measurement detail, so these must be checks to survive). The
//! far-field AMI number (`ami_sdm`, T2.5c: the `asr.wer_ami_ihm` oracle segments decoded
//! from the single distant microphone, by `m2_prod` on the same default model) is an
//! informational check with the IHM value beside it, never a measurement and not a
//! threshold.
//!
//! Missing models or fixtures make the example fail with the `cargo xtask fetch-models` /
//! `fetch-fixtures` command to run; nothing is skipped.

use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, bail, ensure};
use serde_json::Value;

use super::{Check, Ctx, Measurement, RunOutput};
use crate::ci::cargo_command;
use crate::proc::run_with_timeout;

/// Deadline for building and running each evaluation (a CPU-only host decodes the AMI
/// segments, the LibriSpeech subset and the double-talk renders with Parakeet 0.6B v2 on the
/// CPU, and `asr.echo_leak` with base.en; generous so a slow or loaded host never fails on
/// time alone).
pub const EVAL_TIMEOUT: Duration = Duration::from_secs(8 * 60 * 60);

/// `asr.echo_leak` harness guard (T2.4f): in the gating aec condition, cleaning (text
/// de-duplication + speaker check) must keep at least this share of the near-end words
/// heard before cleaning, both pooled over double-talk and near-only scenarios and in the
/// double-talk scenarios alone (near-only scenarios have no lock and nothing removable, so
/// they would dilute a pooled floor). Both checks must pass. Not a PLAN.md
/// 4.2 threshold: it keeps over-deletion from buying a low leak, and allows one lost word
/// in 50 (recogniser and attribution noise). Fixed before measuring; a failure fails the
/// run.
pub const ECHO_NEAR_RECALL_FLOOR_PERCENT: f64 = 98.0;

/// Which evaluation measures a metric.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// `asr_eval` (earshot-asr-whisper): base.en, the whisper model PLAN.md 4.2 names.
    AsrEval,
    /// `m2_prod` (earshot-runtime): the production setup (the default transcription model
    /// and live mode; the owner).
    Production,
}

/// Measured ids, whether PLAN.md 4.2 pins them to the CPU, and which evaluation measures
/// them.
pub const METRICS: [(&str, bool, Source); 7] = [
    ("asr.wer_smoke_base", true, Source::AsrEval),
    ("asr.wer_ls_base", true, Source::AsrEval),
    ("asr.stream_determinism", false, Source::Production),
    ("asr.wer_ls_default", false, Source::Production),
    ("asr.wer_ami_ihm", false, Source::Production),
    ("asr.wer_aec_delta", false, Source::Production),
    ("asr.echo_leak", false, Source::AsrEval),
];

/// `asr_eval`'s informational entries: the base.en numbers of the old `asr.wer_stream_delta`
/// (chunker) and `asr.wer_aec_delta` (batch per utterance window) as measured until
/// 2026-10-24, kept as checks so the history stays comparable (never measurements).
pub const INFORMATIONAL: [&str; 2] = [
    "informational.wer_stream_delta_base_chunker",
    "informational.wer_aec_delta_base_batch",
];

/// The backend for the metrics not pinned to the CPU: `requested` (`--backend`) if given,
/// else CUDA when doctor reports it, else the CPU.
///
/// # Errors
/// An unsupported `--backend`, or `cuda` on a host where doctor does not report CUDA.
pub fn select_backend(requested: Option<&str>, cuda: bool) -> Result<&'static str> {
    match requested {
        None if cuda => Ok("cuda"),
        None | Some("cpu") => Ok("cpu"),
        Some("cuda") if cuda => Ok("cuda"),
        Some("cuda") => bail!(
            "--backend cuda, but `cargo xtask doctor` does not report the CUDA toolkit and an \
             NVIDIA GPU"
        ),
        Some(other) => bail!("accept m2 supports --backend cpu or cuda, not {other}"),
    }
}

/// The `cargo run` arguments of the evaluation.
pub fn eval_args(
    root: &Path,
    backend: &str,
    models: &Path,
    fixtures: &Path,
) -> Vec<std::ffi::OsString> {
    let mut args: Vec<std::ffi::OsString> =
        ["run", "--release", "--quiet", "-p", "earshot-asr-whisper"]
            .iter()
            .map(Into::into)
            .collect();
    if backend == "cuda" {
        args.extend(["--features", "cuda"].map(Into::into));
    }
    args.extend(["--example", "asr_eval", "--"].map(Into::into));
    args.push(root.into());
    args.extend(["--backend".into(), backend.into()]);
    args.extend(["--models".into(), models.into()]);
    args.extend(["--fixtures".into(), fixtures.into()]);
    args
}

/// The `cargo build` arguments of the production evaluation (`m2_prod` of
/// earshot-runtime), with the same features as `asr_eval`.
#[must_use]
pub fn prod_build_args(backend: &str) -> Vec<String> {
    let mut args: Vec<String> = ["build", "--release", "-p", "earshot-runtime"]
        .map(Into::into)
        .to_vec();
    if backend == "cuda" {
        args.extend(["--features".into(), "cuda".into()]);
    }
    args.extend(
        [
            "--example",
            "m2_prod",
            "--message-format=json-render-diagnostics",
        ]
        .map(Into::into),
    );
    args
}

/// The arguments of the built `m2_prod` executable.
#[must_use]
pub fn prod_eval_args(
    root: &Path,
    backend: &str,
    models: &Path,
    fixtures: &Path,
) -> Vec<std::ffi::OsString> {
    vec![
        root.into(),
        "--backend".into(),
        backend.into(),
        "--models".into(),
        models.into(),
        "--fixtures".into(),
        fixtures.into(),
    ]
}

/// The `m2_prod` executable named in cargo's JSON build messages.
#[must_use]
pub fn prod_executable(messages: &str) -> Option<std::path::PathBuf> {
    messages
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter(|m| m["reason"] == "compiler-artifact" && m["target"]["name"] == "m2_prod")
        .find_map(|m| m["executable"].as_str().map(std::path::PathBuf::from))
}

/// Parses an evaluation's stdout (one JSON object).
fn parse_output(out: &crate::proc::CmdOutput, name: &str) -> Result<Value> {
    if !out.success {
        bail!("{name} failed: {}", out.describe());
    }
    out.require_complete(name)?;
    serde_json::from_str(out.stdout.trim())
        .with_context(|| format!("{name} printed invalid JSON:\n{}", out.stdout))
}

pub(super) fn run(ctx: &Ctx) -> Result<RunOutput> {
    // An incomplete CUDA probe is an error here, never a silent CPU run.
    let cuda = crate::doctor::cuda_for_backend_choice(&crate::doctor::collect(), ctx.backend)?;
    let backend = select_backend(ctx.backend, cuda)?;
    let models = crate::models::cache_dir()?;
    let fixtures = crate::fixtures::cache_dir()?;
    let mut cmd = cargo_command();
    cmd.args(eval_args(ctx.root, backend, &models, &fixtures))
        .current_dir(ctx.root);
    eprintln!("accept m2: running asr_eval on {backend} (release build; this takes a while)");
    let out = run_with_timeout(&mut cmd, EVAL_TIMEOUT)
        .context("asr_eval did not finish within the deadline")?;
    let asr = parse_output(&out, "asr_eval")?;
    // The production setup (default model, default live mode): built first, then the
    // executable itself runs under the deadline (the deadline kills only the direct child).
    let mut build = cargo_command();
    build.args(prod_build_args(backend)).current_dir(ctx.root);
    eprintln!("accept m2: building m2_prod ({backend}, release)");
    let built = run_with_timeout(&mut build, EVAL_TIMEOUT)
        .context("building m2_prod did not finish within the deadline")?;
    if !built.success {
        bail!("building m2_prod failed: {}", built.describe());
    }
    built.require_complete("building m2_prod")?;
    let exe = prod_executable(&built.stdout).context("cargo reported no m2_prod executable")?;
    let mut cmd = std::process::Command::new(&exe);
    cmd.args(prod_eval_args(ctx.root, backend, &models, &fixtures))
        .current_dir(ctx.root);
    eprintln!("accept m2: running m2_prod on {backend}");
    let out = run_with_timeout(&mut cmd, EVAL_TIMEOUT)
        .context("m2_prod did not finish within the deadline")?;
    let prod = parse_output(&out, "m2_prod")?;
    // The release whisper.cpp that produced these numbers was optimised (T2.2c).
    let release = release_dir(ctx.root, std::env::var_os("CARGO_TARGET_DIR").as_deref());
    crate::native_flags::check_optimised(&release, "--release")?;
    evaluate(&asr, &prod, backend)
}

/// The release profile directory both evaluations were built into: `CARGO_TARGET_DIR`
/// (relative to the repo root, as cargo resolves it from there) when set, which a deep
/// worktree path needs for a CUDA build, else `<root>/target`.
#[must_use]
pub fn release_dir(root: &Path, target_dir: Option<&std::ffi::OsStr>) -> std::path::PathBuf {
    match target_dir.filter(|d| !d.is_empty()) {
        Some(d) => root.join(d).join("release"),
        None => root.join("target").join("release"),
    }
}

fn str_of<'a>(v: &'a Value, key: &str) -> Result<&'a str> {
    v[key]
        .as_str()
        .with_context(|| format!("asr_eval: `{key}` missing in {v}"))
}

/// The production default model `m2_prod` reports (`default_model.name`), checked against
/// the model of each of its metrics and loads.
fn production_model(prod: &Value) -> Result<&str> {
    let name = prod["default_model"]["name"]
        .as_str()
        .context("m2_prod: no default_model.name")?;
    str_of(&prod["default_model"], "rule").map(|_| name)
}

/// Turns the two evaluations' JSON (`asr_eval` and `m2_prod`) into one measurement per
/// `asr.*` id plus evidence checks.
///
/// # Errors
/// A missing or duplicate metric, a metric from the wrong evaluation, a non-numeric value,
/// a backend other than the plan's, a production metric on another model than the reported
/// default, or a model load without a matching log classification.
pub fn evaluate(asr: &Value, prod: &Value, selected: &str) -> Result<RunOutput> {
    for (json, name) in [(asr, "asr_eval"), (prod, "m2_prod")] {
        ensure!(
            str_of(json, "selected_backend")? == selected,
            "{name} ran on {}, expected {selected}",
            json["selected_backend"]
        );
    }
    let default_model = production_model(prod)?;
    let metrics = asr["metrics"]
        .as_array()
        .context("asr_eval: no metrics array")?;
    let prod_metrics = prod["metrics"]
        .as_array()
        .context("m2_prod: no metrics array")?;
    let mut measurements = Vec::new();
    let mut decoding = Vec::new();
    for (id, cpu_only, source) in METRICS {
        let (list, name) = match source {
            Source::AsrEval => (metrics, "asr_eval"),
            Source::Production => (prod_metrics, "m2_prod"),
        };
        let found: Vec<&Value> = list.iter().filter(|m| m["id"] == id).collect();
        let [m] = found.as_slice() else {
            bail!("{name}: {} results for {id}, expected 1", found.len());
        };
        if source == Source::Production {
            let model = str_of(&m["model"], "name")?;
            ensure!(
                model == default_model,
                "m2_prod: {id} ran {model}, not the production default {default_model}"
            );
            let value = m["value"]
                .as_f64()
                .with_context(|| format!("m2_prod: {id} has no numeric value"))?;
            let backend = str_of(m, "backend")?;
            ensure!(
                backend == selected,
                "m2_prod: {id} ran on {backend}, --backend requires {selected}"
            );
            let info = &m["model"]["backend_info"];
            ensure!(
                info["kind"] == backend,
                "m2_prod: {id} model reports {}, not {backend}",
                info["kind"]
            );
            let mut measurement = Measurement::value(id, value, str_of(m, "unit")?);
            measurement.backend = Some(backend.to_string());
            measurement.detail = Some(m.to_string());
            measurements.push(measurement);
            let options: Vec<String> = ["options", "live_options", "final_options"]
                .iter()
                .filter(|k| !m[**k].is_null())
                .map(|k| format!("{k} {}", m[*k]))
                .collect();
            ensure!(
                !options.is_empty(),
                "m2_prod: {id} reports no decoding options"
            );
            decoding.push(format!(
                "{id}: production default {model} on {backend} ({}), {}, {}",
                info["device"].as_str().unwrap_or("?"),
                options.join(", "),
                str_of(m, "fallback_note")?
            ));
            continue;
        }
        let value = m["value"]
            .as_f64()
            .with_context(|| format!("asr_eval: {id} has no numeric value"))?;
        let backend = str_of(m, "backend")?;
        let want = if cpu_only { "cpu" } else { selected };
        ensure!(
            backend == want,
            "asr_eval: {id} ran on {backend}, PLAN.md 4.2 / --backend require {want}"
        );
        let info = &m["model"]["backend_info"];
        let mut measurement = Measurement::value(id, value, str_of(m, "unit")?);
        measurement.backend = Some(backend.to_string());
        measurement.detail = Some(m.to_string());
        measurements.push(measurement);
        let options: Vec<String> = ["options", "batch_options", "stream_options"]
            .iter()
            .filter(|k| !m[**k].is_null())
            .map(|k| format!("{k} {}", m[*k]))
            .collect();
        // T2.4d: every metric reports how many decoding windows fell back to a higher
        // temperature.
        let fb = &m["fallback"];
        let count = |k: &str| {
            fb[k]
                .as_u64()
                .with_context(|| format!("asr_eval: {id} reports no fallback {k}"))
        };
        let (windows, fell_back, retries) = (
            count("windows")?,
            count("fallback_windows")?,
            count("retries")?,
        );
        decoding.push(format!(
            "{id}: {} on {} ({}), {}, temperature fallback in {fell_back} of {windows} \
             windows ({retries} retries)",
            info["model"].as_str().unwrap_or("?"),
            backend,
            info["device"].as_str().unwrap_or("?"),
            options.join(", ")
        ));
    }
    // T2.4e/T2.4f: both conditions of asr.echo_leak are always reported with their leak
    // per cleaning stage and near-end recall; the gating aec condition's recall is floored,
    // the raw-mic condition is informational.
    let leak = metrics
        .iter()
        .find(|m| m["id"] == "asr.echo_leak")
        .context("asr_eval: no asr.echo_leak")?;
    let aec = echo_condition(&leak["gating_aec"], "gating_aec")?;
    let raw = echo_condition(&leak["informational_raw"], "informational_raw")?;
    let echo_checks = [
        Check::new(
            "asr.echo_leak aec condition (gating): leak and near-end recall",
            true,
            aec.summary(),
        ),
        Check::new(
            &format!(
                "asr.echo_leak aec condition: cleaning keeps >= \
                 {ECHO_NEAR_RECALL_FLOOR_PERCENT}% of all near-end words heard"
            ),
            aec.near_recall_all >= ECHO_NEAR_RECALL_FLOOR_PERCENT,
            format!("{:.1}% kept", aec.near_recall_all),
        ),
        Check::new(
            &format!(
                "asr.echo_leak aec condition: cleaning keeps >= \
                 {ECHO_NEAR_RECALL_FLOOR_PERCENT}% of near-end words heard in double talk"
            ),
            aec.near_recall >= ECHO_NEAR_RECALL_FLOOR_PERCENT,
            format!("{:.1}% kept", aec.near_recall),
        ),
        Check::new(
            "informational: asr.echo_leak raw mic, canceller bypassed (not a threshold)",
            true,
            raw.summary(),
        ),
    ];
    let aec_delta = prod_metrics
        .iter()
        .find(|m| m["id"] == "asr.wer_aec_delta")
        .context("m2_prod: no asr.wer_aec_delta")?;
    let determinism_check = stream_determinism_check(
        prod_metrics
            .iter()
            .find(|m| m["id"] == STREAM_DETERMINISM)
            .context("m2_prod: no asr.stream_determinism")?,
    )?;
    let seed_check = Check::new(
        "asr.wer_aec_delta per scenario: seeds, near-end word counts and per-seed spread",
        true,
        aec_delta_seeds(aec_delta, true)?,
    );
    for (list, source, name) in [
        (metrics, Source::AsrEval, "asr_eval"),
        (prod_metrics, Source::Production, "m2_prod"),
    ] {
        if let Some(extra) = list.iter().find(|m| {
            !METRICS
                .iter()
                .any(|(id, _, s)| *s == source && m["id"] == *id)
        }) {
            bail!("{name}: unexpected metric {}", extra["id"]);
        }
    }
    let informational = informational_checks(asr, metrics)?;

    let ihm = prod_metrics
        .iter()
        .find(|m| m["id"] == "asr.wer_ami_ihm")
        .and_then(|m| m["value"].as_f64())
        .context("m2_prod: no asr.wer_ami_ihm value")?;
    let sdm_check = Check::new(
        "informational: AMI SDM (Array1-01) far-field WER with the asr.wer_ami_ihm oracle \
         segments (not a threshold)",
        true,
        ami_sdm_summary(prod, selected, default_model, ihm)?,
    );

    let mut checks = Vec::new();
    checks.push(Check::new(
        "production default transcription model (asr.stream_determinism, asr.wer_ls_default, \
         asr.wer_ami_ihm, asr.wer_aec_delta, ami_sdm)",
        true,
        format!(
            "{default_model}: {}",
            str_of(&prod["default_model"], "rule")?
        ),
    ));
    let loads = |json: &'_ Value, name: &str| -> Result<Vec<Value>> {
        json["loads"]
            .as_array()
            .filter(|a| !a.is_empty())
            .cloned()
            .with_context(|| format!("{name} reported no model loads"))
    };
    let (prod_loads, asr_loads) = (loads(prod, "m2_prod")?, loads(asr, "asr_eval")?);
    for load in &prod_loads {
        let requested = str_of(load, "requested_backend")?;
        let kind = load["backend_info"]["kind"].as_str().unwrap_or("?");
        let evidence = str_of(load, "evidence")?;
        let model = str_of(load, "model")?;
        checks.push(Check::new(
            &format!(
                "backend evidence: {model} ({}) on {requested}",
                str_of(load, "role")?
            ),
            requested == kind && model == default_model && placement_names(evidence, requested),
            format!(
                "backend_info {kind} on `{}`: {evidence}",
                load["backend_info"]["device"].as_str().unwrap_or("?")
            ),
        ));
    }
    for load in &asr_loads {
        let requested = str_of(load, "requested_backend")?;
        let log = str_of(load, "whisper_log")?;
        let info = &load["backend_info"];
        let kind = info["kind"].as_str().unwrap_or("?");
        let lines: Vec<&str> = load["evidence_lines"]
            .as_array()
            .map(|a| a.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default();
        checks.push(Check::new(
            &format!(
                "backend evidence: {} on {requested}",
                str_of(load, "model")?
            ),
            requested == log && requested == kind && !lines.is_empty(),
            format!(
                "backend_info {kind} on `{}`, whisper.cpp log {log}: {}",
                info["device"].as_str().unwrap_or("?"),
                lines.join(" | ")
            ),
        ));
    }
    checks.extend(echo_checks);
    checks.push(determinism_check);
    checks.push(seed_check);
    checks.push(sdm_check);
    checks.extend(informational);
    checks.push(Check::new(
        "decoding settings (PLAN.md 4.0)",
        true,
        decoding.join("; "),
    ));
    Ok(RunOutput {
        measurements,
        checks,
    })
}

/// Id of the check that live stretches and the final pass produce one transcript (T2.5e).
pub const STREAM_DETERMINISM: &str = "asr.stream_determinism";

/// The per-stream evidence behind `asr.stream_determinism`, recomputed: the value must be the
/// sum of the streams' differing words, and a stream in which nothing was compared fails,
/// because two empty transcripts do not differ either. Live and final-pass WER go beside it
/// (the committed summary drops measurement detail).
///
/// # Errors
/// A missing field, or a value that is not the sum of the streams.
fn stream_determinism_check(m: &Value) -> Result<Check> {
    let count = |v: &Value, k: &str| {
        v[k].as_u64()
            .with_context(|| format!("m2_prod: {STREAM_DETERMINISM} has no {k} in {v}"))
    };
    let streams = m["per_stream"]
        .as_array()
        .filter(|a| !a.is_empty())
        .with_context(|| format!("m2_prod: {STREAM_DETERMINISM} reports no streams"))?;
    let (mut differing, mut compared, mut empty) = (0, 0, 0);
    for stream in streams {
        differing += count(stream, "words_differing")?;
        let words = count(stream, "compared_words")?;
        compared += words;
        empty += u64::from(words == 0);
    }
    let value = m["value"]
        .as_f64()
        .with_context(|| format!("m2_prod: {STREAM_DETERMINISM} has no numeric value"))?;
    ensure!(
        value == differing as f64 && compared == count(m, "compared_words")?,
        "m2_prod: {STREAM_DETERMINISM} reports {value} words of {} but its streams hold \
         {differing} of {compared}",
        m["compared_words"]
    );
    let wer = |k: &str| {
        m[k]["wer_percent"]
            .as_f64()
            .with_context(|| format!("m2_prod: {STREAM_DETERMINISM} has no {k} WER"))
    };
    Ok(Check::new(
        "asr.stream_determinism: every stream compared words",
        empty == 0,
        format!(
            "{differing} of {compared} words differ over {} streams, {empty} streams with no \
             words; WER against the references: live {:.3}%, final pass {:.3}%",
            streams.len(),
            wer("live")?,
            wer("final_pass")?
        ),
    ))
}

/// Whether `evidence` is ONNX Runtime's node placement with every session placed on
/// `backend`'s execution provider (shape nodes may stay on the CPU).
#[must_use]
pub fn placement_names(evidence: &str, backend: &str) -> bool {
    let provider = match backend {
        "cuda" => "CUDAExecutionProvider=",
        "cpu" => "CPUExecutionProvider=",
        _ => return false,
    };
    let Some(sessions) = evidence.strip_prefix("ONNX Runtime placement: ") else {
        return false;
    };
    let sessions: Vec<&str> = sessions.split("; ").collect();
    !sessions.is_empty() && sessions.iter().all(|s| s.contains(provider))
}

/// The [`INFORMATIONAL`] entries of `asr_eval` (exactly one each, `informational` array, and
/// nothing else: [`AMI_SDM`] comes from `m2_prod` since PLAN D22) as checks that always
/// pass: the base.en values of the two deltas under their definitions until 2026-10-24, so
/// the history stays comparable.
///
/// # Errors
/// A missing, duplicate or unknown entry, or one without its value, scores or seed detail.
fn informational_checks(asr: &Value, metrics: &[Value]) -> Result<Vec<Check>> {
    let list = asr["informational"]
        .as_array()
        .context("asr_eval: no informational array")?;
    if let Some(extra) = list
        .iter()
        .find(|m| !INFORMATIONAL.iter().any(|id| m["id"] == *id))
    {
        bail!("asr_eval: unexpected informational entry {}", extra["id"]);
    }
    if let Some(m) = metrics
        .iter()
        .find(|m| m["id"] == AMI_SDM || INFORMATIONAL.iter().any(|id| m["id"] == *id))
    {
        bail!("asr_eval: informational {} reported as a metric", m["id"]);
    }
    let mut checks = Vec::new();
    for id in INFORMATIONAL {
        let found: Vec<&Value> = list.iter().filter(|m| m["id"] == id).collect();
        let [m] = found.as_slice() else {
            bail!("asr_eval: {} results for {id}, expected 1", found.len());
        };
        let value = m["value"]
            .as_f64()
            .with_context(|| format!("asr_eval: {id} has no numeric value"))?;
        let model = m["model"]["backend_info"]["model"].as_str().unwrap_or("?");
        let backend = str_of(m, "backend")?;
        let wer = |k: &str| {
            m[k]["wer_percent"]
                .as_f64()
                .with_context(|| format!("asr_eval: {id} has no {k} WER"))
        };
        let (name, detail) = if id == INFORMATIONAL[0] {
            (
                "informational: asr.wer_stream_delta on base.en, streaming chunker vs batch \
                 (the definition until 2026-10-24; not a threshold)",
                format!(
                    "{value:+.3} points (stream {:.3}% - batch {:.3}%), {model} on {backend}",
                    wer("stream")?,
                    wer("batch")?
                ),
            )
        } else {
            (
                "informational: asr.wer_aec_delta on base.en, batch per utterance window \
                 (the definition until 2026-10-24; not a threshold)",
                format!(
                    "{value:+.3} points, {model} on {backend}; {}",
                    aec_delta_seeds(m, false)?
                ),
            )
        };
        checks.push(Check::new(name, true, detail));
    }
    Ok(checks)
}

/// Id of the informational far-field AMI result in `m2_prod`'s `informational` list
/// (T2.5c): the `asr.wer_ami_ihm` oracle segments decoded from the single distant microphone.
pub const AMI_SDM: &str = "ami_sdm";

/// The `ami_sdm` result of `m2_prod` as one report line, with the `asr.wer_ami_ihm` value
/// beside it. `informational` must hold exactly that one entry and nothing else, on the
/// selected backend and the production default model, and it must never be a metric.
///
/// # Errors
/// A missing, duplicate or unknown informational entry, one reported as a metric, another
/// backend or model, or a missing field.
fn ami_sdm_summary(json: &Value, selected: &str, default_model: &str, ihm: f64) -> Result<String> {
    let entries = json["informational"]
        .as_array()
        .context("m2_prod: no informational list")?;
    if let Some(extra) = entries.iter().find(|m| m["id"] != AMI_SDM) {
        bail!("m2_prod: unexpected informational entry {}", extra["id"]);
    }
    let in_metrics = json["metrics"]
        .as_array()
        .is_some_and(|a| a.iter().any(|m| m["id"] == AMI_SDM));
    ensure!(
        !in_metrics,
        "m2_prod: informational {AMI_SDM} reported as a metric"
    );
    let found: Vec<&Value> = entries.iter().filter(|e| e["id"] == AMI_SDM).collect();
    let [m] = found.as_slice() else {
        bail!("m2_prod: {} {AMI_SDM} results, expected 1", found.len());
    };
    let backend = str_of(m, "backend")?;
    ensure!(
        backend == selected,
        "m2_prod: {AMI_SDM} ran on {backend}, expected {selected}"
    );
    let model = str_of(&m["model"], "name")?;
    ensure!(
        model == default_model,
        "m2_prod: {AMI_SDM} ran {model}, not the production default {default_model}"
    );
    ensure!(
        m["model"]["backend_info"]["kind"] == backend,
        "m2_prod: {AMI_SDM} model reports {}, not {backend}",
        m["model"]["backend_info"]["kind"]
    );
    let num = |v: &Value, k: &str| {
        v[k].as_f64()
            .with_context(|| format!("m2_prod: {AMI_SDM} has no {k}"))
    };
    let value = num(m, "value")?;
    let score = &m["score"];
    let count = |k: &str| {
        score[k]
            .as_u64()
            .with_context(|| format!("m2_prod: {AMI_SDM} score has no {k}"))
    };
    let meetings = m["meetings"]
        .as_array()
        .filter(|a| !a.is_empty())
        .with_context(|| format!("m2_prod: {AMI_SDM} has no meetings"))?
        .iter()
        .map(|x| {
            Ok(format!(
                "{} {:.2}%",
                str_of(x, "meeting")?,
                num(&x["score"], "wer_percent")?
            ))
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(format!(
        "SDM {value:.2}% (IHM {ihm:.2}%) over {} reference words ({} substitutions, {} \
         deletions, {} insertions); {}; production default {model} on {backend}, {}",
        count("ref_words")?,
        count("substitutions")?,
        count("deletions")?,
        count("insertions")?,
        meetings.join(", "),
        str_of(m, "fallback_note")?,
    ))
}

/// The `asr.wer_aec_delta` seed list, near-end reference word counts and per-seed spread
/// of every scenario, as the evaluation reports them (T2.4g seed rule), in one line per
/// scenario for the report. Exactly one scenario gates (SER 0). With `unscored`
/// (`m2_prod`, which transcribes whole tracks), each seed with words also carries the
/// words that started outside every scoring window, clean and after the canceller.
///
/// # Errors
/// A missing field, seeds that disagree with the per-seed rows, or not exactly one
/// gating scenario.
fn aec_delta_seeds(m: &Value, unscored: bool) -> Result<String> {
    let num = |v: &Value, k: &str, what: &str| {
        v[k].as_f64()
            .with_context(|| format!("asr_eval: asr.wer_aec_delta {what} has no {k}"))
    };
    let words = |v: &Value, what: &str| {
        v["clean"]["ref_words"]
            .as_u64()
            .with_context(|| format!("asr_eval: asr.wer_aec_delta {what} has no clean ref_words"))
    };
    let scenarios = m["scenarios"]
        .as_array()
        .filter(|a| !a.is_empty())
        .context("asr_eval: asr.wer_aec_delta reports no scenarios")?;
    let mut lines = Vec::new();
    let mut gating = 0;
    for scn in scenarios {
        let name = str_of(scn, "scenario")?;
        let gates = scn["gating"]
            .as_bool()
            .with_context(|| format!("asr_eval: asr.wer_aec_delta {name} has no gating flag"))?;
        gating += usize::from(gates);
        let seeds: Vec<u64> = scn["seeds"]
            .as_array()
            .and_then(|a| a.iter().map(Value::as_u64).collect())
            .with_context(|| format!("asr_eval: asr.wer_aec_delta {name} has no seed list"))?;
        let rows = scn["per_seed"]
            .as_array()
            .with_context(|| format!("asr_eval: asr.wer_aec_delta {name} has no per_seed"))?;
        let row_seeds: Vec<Option<u64>> = rows.iter().map(|r| r["seed"].as_u64()).collect();
        ensure!(
            !seeds.is_empty() && row_seeds == seeds.iter().map(|s| Some(*s)).collect::<Vec<_>>(),
            "asr_eval: asr.wer_aec_delta {name}: seeds {seeds:?} disagree with per_seed rows \
             {row_seeds:?}"
        );
        let mut per_seed = Vec::new();
        for (seed, row) in seeds.iter().zip(rows) {
            let what = format!("{name} seed {seed}");
            let n = words(row, &what)?;
            // asr_eval lists a seed whose render holds no complete near-end utterance with
            // zero words and no delta; it is outside the spread.
            if n == 0 {
                ensure!(
                    row["delta_points"].is_null(),
                    "asr_eval: asr.wer_aec_delta {what} has 0 words but a delta"
                );
                per_seed.push(format!("{seed} no complete utterance (0 words)"));
            } else {
                let outside = if unscored {
                    let count = |k: &str| {
                        row["unscored_words"][k].as_u64().with_context(|| {
                            format!("asr.wer_aec_delta {what} has no unscored_words.{k}")
                        })
                    };
                    format!(
                        ", unscored clean {} / aec {}",
                        count("clean")?,
                        count("aec")?
                    )
                } else {
                    String::new()
                };
                per_seed.push(format!(
                    "{seed} {:+.2} ({n} words{outside})",
                    num(row, "delta_points", &what)?
                ));
            }
        }
        let spread = &scn["spread"];
        let what = format!("{name} spread");
        lines.push(format!(
            "{name} ({}): {:+.2} points over {} words, seeds {seeds:?}; per seed {}; spread \
             {:+.2} to {:+.2}, mean {:+.2}, sd {:.2}",
            if gates { "gating" } else { "informational" },
            num(scn, "delta_points", name)?,
            words(scn, name)?,
            per_seed.join(", "),
            num(spread, "min", &what)?,
            num(spread, "max", &what)?,
            num(spread, "mean", &what)?,
            num(spread, "sd", &what)?,
        ));
    }
    ensure!(
        gating == 1,
        "asr_eval: asr.wer_aec_delta has {gating} gating scenarios, expected 1"
    );
    Ok(lines.join("; "))
}

/// One condition of `asr.echo_leak` as `asr_eval` reports it.
struct EchoCondition {
    leak: f64,
    leak_raw: f64,
    leak_text: f64,
    ambiguous: f64,
    near_recall: f64,
    near_recall_text: f64,
    near_only_recall: f64,
    near_only_far_recall: f64,
    near_recall_all: f64,
}

fn echo_condition(v: &Value, key: &str) -> Result<EchoCondition> {
    let num = |k: &str| {
        v[k].as_f64()
            .with_context(|| format!("asr_eval: asr.echo_leak {key} has no {k}"))
    };
    Ok(EchoCondition {
        leak: num("leak_percent")?,
        leak_raw: num("leak_raw_percent")?,
        leak_text: num("leak_text_only_percent")?,
        ambiguous: num("ambiguous_words")?,
        near_recall: num("near_recall_percent")?,
        near_recall_text: num("near_recall_text_only_percent")?,
        near_only_recall: num("near_only_recall_percent")?,
        near_only_far_recall: num("near_only_far_speech_recall_percent")?,
        near_recall_all: num("near_recall_all_percent")?,
    })
}

impl EchoCondition {
    fn summary(&self) -> String {
        format!(
            "leak {:.2}% (no cleaning {:.2}%, text de-dup {:.2}%; {} ambiguous words given to \
             the near end); near-end recall in double talk {:.1}% (text de-dup {:.1}%), \
             no-echo scenarios {:.1}% (with far-end speech {:.1}%), overall {:.1}%",
            self.leak,
            self.leak_raw,
            self.leak_text,
            self.ambiguous,
            self.near_recall,
            self.near_recall_text,
            self.near_only_recall,
            self.near_only_far_recall,
            self.near_recall_all,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const DEFAULT: &str = "parakeet-tdt-0.6b-v2";

    fn load(model: &str, backend: &str, log: &str) -> Value {
        json!({
            "model": model, "requested_backend": backend, "whisper_log": log,
            "backend_info": {"kind": backend, "device": "dev", "model": format!("ggml-{model}")},
            "evidence_lines": ["whisper_backend_init_gpu: x"]
        })
    }

    /// ONNX Runtime's placement report for the three Parakeet TDT sessions on `backend`.
    fn placement(backend: &str) -> String {
        let (dev, provider) = if backend == "cuda" {
            ("cuda:0", "CUDAExecutionProvider")
        } else {
            ("cpu", "CPUExecutionProvider")
        };
        format!(
            "ONNX Runtime placement: requested {dev}, placed: CPUExecutionProvider=482 \
             {provider}=1515; requested {dev}, placed: {provider}=13; requested {dev}, \
             placed: {provider}=10"
        )
    }

    fn prod_load(role: &str, backend: &str) -> Value {
        json!({
            "model": DEFAULT, "role": role, "requested_backend": backend,
            "backend_info": {"kind": backend, "device": "dev", "model": DEFAULT},
            "evidence": placement(backend)
        })
    }

    fn condition(leak: f64, recall_double_talk: f64, recall_all: f64) -> Value {
        json!({
            "leak_percent": leak, "leak_raw_percent": 60.5, "leak_text_only_percent": 23.4,
            "ambiguous_words": 2, "near_recall_percent": recall_double_talk,
            "near_recall_text_only_percent": 95.1, "near_only_recall_percent": 100.0,
            "near_only_far_speech_recall_percent": 100.0, "near_recall_all_percent": recall_all
        })
    }

    fn echo_index(j: &Value) -> usize {
        j["metrics"]
            .as_array()
            .unwrap()
            .iter()
            .position(|m| m["id"] == "asr.echo_leak")
            .unwrap()
    }

    fn seed(seed: u64, delta: f64, words: u64) -> Value {
        json!({
            "seed": seed, "delta_points": delta,
            "unscored_words": {"clean": 1, "aec": 2},
            "clean": {"wer_percent": 5.0, "ref_words": words},
            "aec": {"wer_percent": 5.0 + delta, "ref_words": words}
        })
    }

    fn aec_scenarios() -> Value {
        json!([
            {
                "scenario": "double-talk-ser-0", "ser_db": 0.0, "gating": true,
                "delta_points": 1.25,
                "clean": {"wer_percent": 4.0, "ref_words": 320},
                "aec": {"wer_percent": 5.25, "ref_words": 320},
                "seeds": [7, 1007, 2007],
                "spread": {"min": -0.5, "max": 3.0, "mean": 1.2, "sd": 1.75},
                "per_seed": [seed(7, -0.5, 110), seed(1007, 3.0, 100), seed(2007, 1.1, 110)]
            },
            {
                "scenario": "double-talk-ser-10", "ser_db": -10.0, "gating": false,
                "delta_points": 9.5,
                "clean": {"wer_percent": 4.0, "ref_words": 305},
                "aec": {"wer_percent": 13.5, "ref_words": 305},
                "seeds": [11],
                "spread": {"min": 9.5, "max": 9.5, "mean": 9.5, "sd": 0.0},
                "per_seed": [seed(11, 9.5, 305)]
            }
        ])
    }

    fn aec_delta_index(j: &Value) -> usize {
        j["metrics"]
            .as_array()
            .unwrap()
            .iter()
            .position(|m| m["id"] == "asr.wer_aec_delta")
            .unwrap()
    }

    /// `asr_eval`'s JSON: the metrics it measures plus the informational base.en entries.
    fn sample(selected: &str) -> Value {
        let metrics: Vec<Value> = METRICS
            .iter()
            .filter(|(_, _, s)| *s == Source::AsrEval)
            .map(|(id, cpu, _)| {
                let backend = if *cpu { "cpu" } else { selected };
                json!({
                    "id": id, "value": 1.5, "unit": "%",
                    "backend": backend,
                    "options": {"decoding": {"strategy": "beam_search", "beam_size": 5}},
                    "fallback": {"windows": 10, "fallback_windows": 2, "retries": 3},
                    "gating_aec": condition(0.45, 98.5, 99.0),
                    "informational_raw": condition(6.9, 75.4, 75.4),
                    "model": {"backend_info": {"kind": backend, "device": "dev", "model": "ggml-base.en"}}
                })
            })
            .collect();
        let base =
            json!({"backend_info": {"kind": selected, "device": "dev", "model": "ggml-base.en"}});
        json!({
            "selected_backend": selected,
            "loads": [load("base.en", "cpu", "cpu"), load("base.en", selected, selected)],
            "metrics": metrics,
            "informational": base_deltas(selected, &base)
        })
    }

    fn prod_model(selected: &str) -> Value {
        json!({
            "name": DEFAULT,
            "backend_info": {"kind": selected, "device": "dev", "model": DEFAULT}
        })
    }

    /// `m2_prod`'s JSON: the four production metrics on the default model and the
    /// informational AMI SDM number.
    fn prod_sample(selected: &str) -> Value {
        let model = prod_model(selected);
        let batch = json!({"decoding": {"strategy": {"strategy": "beam_search", "beam_size": 5}}});
        json!({
            "selected_backend": selected,
            "default_model": {"name": DEFAULT, "rule": "first rung of the English ladder"},
            "loads": [prod_load("live", selected), prod_load("final pass", selected)],
            "metrics": [
                {
                    "id": "asr.stream_determinism", "value": 0.0, "unit": "count",
                    "backend": selected, "model": model,
                    "compared_words": 1500,
                    "per_stream": [
                        {"compared_words": 900, "words_differing": 0},
                        {"compared_words": 600, "words_differing": 0}
                    ],
                    "live": {"wer_percent": 1.5}, "final_pass": {"wer_percent": 1.5},
                    "live_options": {"decoding": {"strategy": "greedy"}},
                    "final_options": {"decoding": {"strategy": "greedy"}},
                    "fallback_note": "no temperature fallback (Parakeet decodes greedily)"
                },
                {
                    "id": "asr.wer_ls_default", "value": 1.6, "unit": "%",
                    "backend": selected, "model": model, "options": batch,
                    "fallback_note": "no temperature fallback (Parakeet decodes greedily)"
                },
                {
                    "id": "asr.wer_ami_ihm", "value": 10.5, "unit": "%",
                    "backend": selected, "model": model, "options": batch,
                    "fallback_note": "no temperature fallback (Parakeet decodes greedily)"
                },
                {
                    "id": "asr.wer_aec_delta", "value": 1.25, "unit": "points",
                    "backend": selected, "model": model,
                    "options": {"decoding": {"strategy": "greedy"}},
                    "fallback_note": "no temperature fallback (Parakeet decodes greedily)",
                    "scenarios": aec_scenarios()
                }
            ],
            "informational": [ami_sdm(selected)]
        })
    }

    fn both(selected: &str) -> (Value, Value) {
        (sample(selected), prod_sample(selected))
    }

    fn eval(j: &(Value, Value), selected: &str) -> Result<RunOutput> {
        evaluate(&j.0, &j.1, selected)
    }

    fn check<'a>(out: &'a RunOutput, name: &str) -> &'a Check {
        out.checks
            .iter()
            .find(|c| c.name.contains(name))
            .unwrap_or_else(|| panic!("no check `{name}` in {:?}", out.checks))
    }

    fn base_deltas(selected: &str, base: &Value) -> Value {
        json!([
            {
                "id": INFORMATIONAL[0], "value": 3.952, "unit": "points",
                "backend": selected, "model": base,
                "stream": {"wer_percent": 7.5}, "batch": {"wer_percent": 3.548}
            },
            {
                "id": INFORMATIONAL[1], "value": 20.0, "unit": "points",
                "backend": selected, "model": base,
                "scenarios": aec_scenarios()
            }
        ])
    }

    fn ami_sdm(backend: &str) -> Value {
        let meeting =
            |name: &str, wer: f64| json!({"meeting": name, "score": {"wer_percent": wer}});
        json!({
            "id": "ami_sdm", "value": 41.25, "unit": "%", "backend": backend,
            "score": {"wer_percent": 41.25, "substitutions": 900, "deletions": 1200,
                      "insertions": 200, "ref_words": 5576},
            "meetings": [meeting("IS1009a", 39.5), meeting("ES2004a", 43.0)],
            "fallback_note": "no temperature fallback (Parakeet decodes greedily)",
            "model": prod_model(backend)
        })
    }

    #[test]
    fn m2_ami_sdm_is_an_informational_check_beside_the_ihm_value() {
        let out = eval(&both("cuda"), "cuda").unwrap();
        // Never a measurement: the ids are exactly the thresholds.
        assert!(out.measurements.iter().all(|m| m.id != "ami_sdm"));
        let c = out
            .checks
            .iter()
            .find(|c| c.name.contains("AMI SDM"))
            .expect("ami_sdm check");
        assert!(c.pass && c.name.contains("informational") && c.name.contains("not a threshold"));
        let d = c.detail.as_deref().unwrap();
        for want in [
            "SDM 41.25% (IHM 10.50%)",
            "5576 reference words",
            "900 substitutions, 1200 deletions, 200 insertions",
            "IS1009a 39.50%, ES2004a 43.00%",
            "production default parakeet-tdt-0.6b-v2 on cuda",
            "no temperature fallback",
        ] {
            assert!(d.contains(want), "missing `{want}` in {d}");
        }
        // The decoding settings stay the last check.
        assert!(
            out.checks
                .last()
                .unwrap()
                .name
                .starts_with("decoding settings")
        );
    }

    #[test]
    fn m2_ami_sdm_must_be_reported_once_on_the_selected_backend() {
        // From m2_prod (PLAN D22), exactly once, on the selected backend and default model.
        let mut j = both("cuda");
        j.1["informational"] = json!([]);
        assert!(eval(&j, "cuda").is_err());
        let mut j = both("cuda");
        j.1.as_object_mut().unwrap().remove("informational");
        assert!(eval(&j, "cuda").is_err());
        let mut j = both("cuda");
        j.1["informational"] = json!([ami_sdm("cuda"), ami_sdm("cuda")]);
        assert!(eval(&j, "cuda").is_err());
        let mut j = both("cuda");
        j.1["informational"] = json!([ami_sdm("cpu")]);
        assert!(eval(&j, "cuda").is_err());
        let mut j = both("cuda");
        j.1["informational"][0]["model"]["name"] = json!("large-v3-turbo");
        assert!(eval(&j, "cuda").is_err());
        let mut j = both("cuda");
        j.1["informational"][0]["model"]["backend_info"]["kind"] = json!("cpu");
        assert!(eval(&j, "cuda").is_err());
        let mut j = both("cuda");
        j.1["informational"][0]["value"] = Value::Null;
        assert!(eval(&j, "cuda").is_err());
        let mut j = both("cuda");
        j.1["informational"][0]["score"]["ref_words"] = Value::Null;
        assert!(eval(&j, "cuda").is_err());
        let mut j = both("cuda");
        j.1["informational"][0]["fallback_note"] = Value::Null;
        assert!(eval(&j, "cuda").is_err());
        let mut j = both("cuda");
        j.1["informational"]
            .as_array_mut()
            .unwrap()
            .push(json!({"id": "other", "value": 1.0}));
        assert!(eval(&j, "cuda").is_err());
        // Informational results never enter either metrics list, and asr_eval no longer
        // reports the SDM number.
        let mut j = both("cuda");
        let sdm = ami_sdm("cuda");
        j.1["metrics"].as_array_mut().unwrap().push(sdm);
        assert!(eval(&j, "cuda").is_err());
        let mut j = both("cuda");
        let sdm = ami_sdm("cuda");
        j.0["metrics"].as_array_mut().unwrap().push(sdm);
        assert!(eval(&j, "cuda").is_err());
        let mut j = both("cuda");
        let sdm = ami_sdm("cuda");
        j.0["informational"].as_array_mut().unwrap().push(sdm);
        assert!(eval(&j, "cuda").is_err());
    }

    #[test]
    fn m2_evaluate_emits_one_measurement_per_threshold() {
        let out = eval(&both("cuda"), "cuda").unwrap();
        let ids: Vec<&str> = out.measurements.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids, METRICS.map(|(id, _, _)| id));
        let backends: Vec<&str> = out
            .measurements
            .iter()
            .map(|m| m.backend.as_deref().unwrap())
            .collect();
        assert_eq!(
            backends,
            ["cpu", "cpu", "cuda", "cuda", "cuda", "cuda", "cuda"]
        );
        assert_eq!(out.measurements[2].unit, "count");
        // The production metrics' values come from m2_prod, not asr_eval's 1.5.
        assert_eq!(out.measurements[2].value, Some(0.0));
        assert_eq!(out.measurements[3].value, Some(1.6));
        assert_eq!(out.measurements[4].value, Some(10.5));
        assert_eq!(out.measurements[5].value, Some(1.25));
        assert_eq!(out.measurements[3].unit, "%");
        // PLAN D22: the LibriSpeech and AMI numbers are the production default's.
        assert!(
            ["asr.wer_ls_default", "asr.wer_ami_ihm"]
                .iter()
                .all(|id| METRICS
                    .iter()
                    .any(|(m, cpu, s)| m == id && !cpu && *s == Source::Production)),
            "{METRICS:?}"
        );
        assert!(out.checks.iter().all(|c| c.pass), "{:?}", out.checks);
        let decoding = out.checks.last().unwrap().detail.clone().unwrap();
        assert!(decoding.contains("beam_size"), "{decoding}");
        for id in [
            "asr.stream_determinism",
            "asr.wer_ls_default",
            "asr.wer_ami_ihm",
            "asr.wer_aec_delta",
        ] {
            assert!(
                decoding.contains(&format!(
                    "{id}: production default parakeet-tdt-0.6b-v2 on cuda"
                )),
                "{decoding}"
            );
        }
        assert!(
            decoding.contains("asr.wer_smoke_base: ggml-base.en on cpu"),
            "{decoding}"
        );
        assert!(decoding.contains("no temperature fallback"), "{decoding}");
        let raw = check(&out, "raw mic");
        assert!(raw.pass && raw.name.contains("informational"));
        assert!(raw.detail.as_deref().unwrap().contains("leak 6.90%"));
        let aec = check(&out, "aec condition (gating)");
        let detail = aec.detail.as_deref().unwrap();
        assert!(detail.contains("leak 0.45%"), "{detail}");
        assert!(detail.contains("2 ambiguous"), "{detail}");
        assert!(detail.contains("with far-end speech 100.0%"), "{detail}");
        assert!(
            decoding.contains("temperature fallback in 2 of 10 windows (3 retries)"),
            "{decoding}"
        );
        let model = check(&out, "production default transcription model");
        assert!(
            model
                .detail
                .as_deref()
                .unwrap()
                .contains("parakeet-tdt-0.6b-v2: first rung")
        );
        assert!(check(&out, "parakeet-tdt-0.6b-v2 (live) on cuda").pass);
        assert!(check(&out, "parakeet-tdt-0.6b-v2 (final pass) on cuda").pass);
    }

    #[test]
    fn m2_aec_delta_seeds_word_counts_and_spread_are_reported() {
        let out = eval(&both("cpu"), "cpu").unwrap();
        let check = out
            .checks
            .iter()
            .find(|c| c.name.starts_with("asr.wer_aec_delta per scenario"))
            .expect("aec delta seed check");
        assert!(check.pass);
        let d = check.detail.as_deref().unwrap();
        for want in [
            "double-talk-ser-0 (gating): +1.25 points over 320 words",
            "seeds [7, 1007, 2007]",
            "per seed 7 -0.50 (110 words, unscored clean 1 / aec 2), 1007 +3.00 (100 words, \
             unscored clean 1 / aec 2), 2007 +1.10 (110 words, unscored clean 1 / aec 2)",
            "spread -0.50 to +3.00, mean +1.20, sd 1.75",
            "double-talk-ser-10 (informational): +9.50 points over 305 words",
            "seeds [11]",
        ] {
            assert!(d.contains(want), "missing `{want}` in {d}");
        }
        // The decoding settings stay the last check.
        assert!(
            out.checks
                .last()
                .unwrap()
                .name
                .starts_with("decoding settings")
        );
    }

    #[test]
    fn m2_aec_delta_seed_without_complete_utterances_is_listed_with_zero_words() {
        // The evaluation keeps such a seed in `seeds` and reports it with zero words and no
        // delta.
        let mut j = both("cpu");
        let k = aec_delta_index(&j.1);
        let scn = &mut j.1["metrics"][k]["scenarios"][1];
        scn["seeds"] = json!([11, 1011]);
        scn["per_seed"].as_array_mut().unwrap().push(json!({
            "seed": 1011, "delta_points": null,
            "clean": {"ref_words": 0}, "aec": {"ref_words": 0},
            "utterances": [], "skipped_cut_off": 2
        }));
        let out = eval(&j, "cpu").unwrap();
        let d = out
            .checks
            .iter()
            .find(|c| c.name.starts_with("asr.wer_aec_delta per scenario"))
            .and_then(|c| c.detail.clone())
            .unwrap();
        assert!(
            d.contains(
                "seeds [11, 1011]; per seed 11 +9.50 (305 words, unscored clean 1 / aec 2), 1011 \
                 no complete utterance (0 words)"
            ),
            "{d}"
        );
        // A seed with words still needs its delta; a zero-word seed must not carry one.
        let mut bad = j.clone();
        bad.1["metrics"][k]["scenarios"][1]["per_seed"][1]["delta_points"] = json!(1.0);
        assert!(eval(&bad, "cpu").is_err());
        let mut bad = j.clone();
        bad.1["metrics"][k]["scenarios"][1]["per_seed"][0]["delta_points"] = Value::Null;
        assert!(eval(&bad, "cpu").is_err());
        // Every seed still needs its row.
        let mut bad = j;
        bad.1["metrics"][k]["scenarios"][1]["per_seed"]
            .as_array_mut()
            .unwrap()
            .pop();
        assert!(eval(&bad, "cpu").is_err());
    }

    #[test]
    fn m2_aec_delta_without_seed_detail_fails() {
        for path in [
            vec!["scenarios"],
            vec!["scenarios", "0", "seeds"],
            vec!["scenarios", "0", "spread"],
            vec!["scenarios", "0", "per_seed"],
            vec!["scenarios", "0", "clean"],
            vec!["scenarios", "1", "per_seed", "0", "clean"],
            vec!["scenarios", "1", "per_seed", "0", "delta_points"],
        ] {
            let mut j = both("cpu");
            let k = aec_delta_index(&j.1);
            let mut v = &mut j.1["metrics"][k];
            for p in &path[..path.len() - 1] {
                v = match p.parse::<usize>() {
                    Ok(i) => &mut v[i],
                    Err(_) => &mut v[*p],
                };
            }
            v[*path.last().unwrap()] = Value::Null;
            assert!(eval(&j, "cpu").is_err(), "{path:?}");
        }
        // Seeds and per-seed rows must agree.
        let mut j = both("cpu");
        let k = aec_delta_index(&j.1);
        j.1["metrics"][k]["scenarios"][0]["seeds"] = json!([7, 1007]);
        assert!(eval(&j, "cpu").is_err());
        // Exactly one gating scenario.
        let mut j = both("cpu");
        let k = aec_delta_index(&j.1);
        j.1["metrics"][k]["scenarios"][1]["gating"] = json!(true);
        assert!(eval(&j, "cpu").is_err());
    }

    #[test]
    fn m2_echo_near_recall_floor_fails_the_run() {
        // (pooled check passes, double-talk check passes) for the given recalls.
        let floor = |double_talk: f64, all: f64| {
            let mut j = both("cpu");
            let k = echo_index(&j.0);
            j.0["metrics"][k]["gating_aec"] = condition(0.45, double_talk, all);
            let out = eval(&j, "cpu").unwrap();
            let pass = |what: &str| {
                out.checks
                    .iter()
                    .find(|c| c.name.contains("cleaning keeps") && c.name.contains(what))
                    .unwrap_or_else(|| panic!("floor check {what}"))
                    .pass
            };
            (pass("all near-end words"), pass("in double talk"))
        };
        let f = ECHO_NEAR_RECALL_FLOOR_PERCENT;
        assert_eq!(floor(f, f), (true, true));
        assert_eq!(floor(100.0, 100.0), (true, true));
        assert_eq!(floor(100.0, 97.9), (false, true));
        // Near-only scenarios (nothing removable) dilute the pool: double-talk loss that the
        // pooled recall hides still fails.
        assert_eq!(floor(97.9, 99.0), (true, false));
        assert_eq!(floor(97.0, 97.0), (false, false));
    }

    #[test]
    fn m2_evaluate_rejects_missing_duplicate_and_extra_metrics() {
        let mut j = both("cpu");
        j.0["metrics"].as_array_mut().unwrap().pop();
        assert!(eval(&j, "cpu").is_err());
        let mut j = both("cpu");
        let first = j.0["metrics"][0].clone();
        j.0["metrics"].as_array_mut().unwrap().push(first);
        assert!(eval(&j, "cpu").is_err());
        let mut j = both("cpu");
        j.0["metrics"]
            .as_array_mut()
            .unwrap()
            .push(json!({"id": "asr.extra", "value": 1.0, "unit": "%", "backend": "cpu"}));
        assert!(eval(&j, "cpu").is_err());
        let mut j = both("cpu");
        j.0["metrics"][0]["value"] = Value::Null;
        assert!(eval(&j, "cpu").is_err());
        // Both conditions of asr.echo_leak are mandatory (T2.4e, T2.4f), with every field.
        for key in ["informational_raw", "gating_aec"] {
            let mut j = both("cpu");
            let k = echo_index(&j.0);
            j.0["metrics"][k][key] = Value::Null;
            assert!(eval(&j, "cpu").is_err(), "{key}");
            let mut j = both("cpu");
            j.0["metrics"][k][key]["ambiguous_words"] = Value::Null;
            assert!(eval(&j, "cpu").is_err(), "{key}");
        }
        // Fallback counts are mandatory (T2.4d).
        let mut j = both("cpu");
        j.0["metrics"][2]["fallback"] = Value::Null;
        assert!(eval(&j, "cpu").is_err());
    }

    #[test]
    fn m2_production_metrics_come_only_from_m2_prod_on_the_default_model() {
        // A production metric missing from m2_prod.
        let mut j = both("cuda");
        j.1["metrics"].as_array_mut().unwrap().remove(0);
        assert!(eval(&j, "cuda").is_err());
        // asr_eval may not measure a production metric (base.en is informational only).
        let mut j = both("cuda");
        let stream = j.1["metrics"][0].clone();
        j.0["metrics"].as_array_mut().unwrap().push(stream);
        assert!(eval(&j, "cuda").is_err());
        // m2_prod may not measure an asr_eval metric.
        let mut j = both("cuda");
        let smoke = j.0["metrics"][0].clone();
        j.1["metrics"].as_array_mut().unwrap().push(smoke);
        assert!(eval(&j, "cuda").is_err());
        // A production metric on another model than the reported default (every one of
        // them, the LibriSpeech and AMI WERs included: PLAN D22).
        for k in 0..4 {
            let mut j = both("cuda");
            j.1["metrics"][k]["model"]["name"] = json!("large-v3-turbo");
            assert!(eval(&j, "cuda").is_err(), "{k}");
            let mut j = both("cuda");
            j.1["metrics"][k]["backend"] = json!("cpu");
            assert!(eval(&j, "cuda").is_err(), "{k}");
            let mut j = both("cuda");
            j.1["metrics"][k]["fallback_note"] = Value::Null;
            assert!(eval(&j, "cuda").is_err(), "{k}");
        }
        // The batch metrics report their decoding options.
        for k in [1, 2] {
            let mut j = both("cuda");
            j.1["metrics"][k]["options"] = Value::Null;
            assert!(eval(&j, "cuda").is_err(), "{k}");
        }
        // asr_eval may not measure the LibriSpeech or AMI default-model WERs.
        for k in [1, 2] {
            let mut j = both("cuda");
            let m = j.1["metrics"][k].clone();
            j.0["metrics"].as_array_mut().unwrap().push(m);
            assert!(eval(&j, "cuda").is_err(), "{k}");
        }
        // The default model must be named with its rule.
        for key in ["name", "rule"] {
            let mut j = both("cuda");
            j.1["default_model"][key] = Value::Null;
            assert!(eval(&j, "cuda").is_err(), "{key}");
        }
        // On the selected backend, and the model agrees.
        let mut j = both("cuda");
        j.1["metrics"][0]["backend"] = json!("cpu");
        assert!(eval(&j, "cuda").is_err());
        let mut j = both("cuda");
        j.1["metrics"][0]["model"]["backend_info"]["kind"] = json!("cpu");
        assert!(eval(&j, "cuda").is_err());
        let mut j = both("cuda");
        j.1["selected_backend"] = json!("cpu");
        assert!(eval(&j, "cuda").is_err());
        // Decoding options and the fallback note are mandatory.
        for key in ["live_options", "fallback_note"] {
            let mut j = both("cuda");
            j.1["metrics"][0][key] = Value::Null;
            if key == "live_options" {
                j.1["metrics"][0]["final_options"] = Value::Null;
            }
            assert!(eval(&j, "cuda").is_err(), "{key}");
        }
        // Production loads: required, and a load on another backend or model fails its check.
        let mut j = both("cuda");
        j.1["loads"] = json!([]);
        assert!(eval(&j, "cuda").is_err());
        let mut j = both("cuda");
        j.1["loads"][1]["backend_info"]["kind"] = json!("cpu");
        let out = eval(&j, "cuda").unwrap();
        assert!(!check(&out, "(final pass) on cuda").pass);
        let mut j = both("cuda");
        j.1["loads"][0]["model"] = json!("base.en");
        let out = eval(&j, "cuda").unwrap();
        assert!(!check(&out, "base.en (live) on cuda").pass);
    }

    /// T2.5e: words differing is the value and it is recomputed from the streams; two empty
    /// transcripts do not count as agreeing.
    #[test]
    fn m2_stream_determinism_is_recomputed_and_needs_words_in_every_stream() {
        let out = eval(&both("cuda"), "cuda").unwrap();
        let m = out
            .measurements
            .iter()
            .find(|m| m.id == STREAM_DETERMINISM)
            .unwrap();
        assert_eq!((m.value, m.unit.as_str()), (Some(0.0), "count"));
        let c = check(&out, "asr.stream_determinism: every stream");
        assert!(c.pass);
        let d = c.detail.as_deref().unwrap();
        assert!(d.contains("0 of 1500 words differ over 2 streams"), "{d}");
        assert!(d.contains("live 1.500%, final pass 1.500%"), "{d}");

        let set = |edit: &dyn Fn(&mut Value)| {
            let mut j = both("cuda");
            edit(&mut j.1["metrics"][0]);
            j
        };
        // A difference a stream reports reaches the value, and a value that hides it fails.
        let hidden = set(&|m| m["per_stream"][1]["words_differing"] = json!(2));
        let e = eval(&hidden, "cuda").unwrap_err().to_string();
        assert!(e.contains("streams hold 2 of 1500"), "{e}");
        let reported = set(&|m| {
            m["per_stream"][1]["words_differing"] = json!(2);
            m["value"] = json!(2.0);
        });
        let out = eval(&reported, "cuda").unwrap();
        let m = out
            .measurements
            .iter()
            .find(|m| m.id == STREAM_DETERMINISM)
            .unwrap();
        assert_eq!(m.value, Some(2.0));
        // An empty stream fails the check even though nothing differs.
        let empty = set(&|m| {
            m["per_stream"][1]["compared_words"] = json!(0);
            m["compared_words"] = json!(900);
        });
        let out = eval(&empty, "cuda").unwrap();
        assert!(!check(&out, "asr.stream_determinism: every stream").pass);
        // Missing evidence is an error, never a pass.
        let none = set(&|m| m["per_stream"] = json!([]));
        assert!(eval(&none, "cuda").is_err());
    }

    #[test]
    fn m2_base_en_deltas_are_informational_checks_not_measurements() {
        let out = eval(&both("cuda"), "cuda").unwrap();
        let stream = check(&out, "informational: asr.wer_stream_delta on base.en");
        assert!(stream.pass);
        let d = stream.detail.as_deref().unwrap();
        assert!(
            d.contains("+3.952 points (stream 7.500% - batch 3.548%), ggml-base.en on cuda"),
            "{d}"
        );
        let aec = check(&out, "informational: asr.wer_aec_delta on base.en");
        let d = aec.detail.as_deref().unwrap();
        assert!(d.contains("+20.000 points, ggml-base.en on cuda"), "{d}");
        assert!(d.contains("seeds [7, 1007, 2007]"), "{d}");
        // Never a measurement.
        assert!(
            out.measurements
                .iter()
                .all(|m| !m.id.starts_with("informational"))
        );
        // Each entry is mandatory, once, with its value; nothing unknown; never a metric.
        for i in 0..2 {
            let mut j = both("cuda");
            j.0["informational"].as_array_mut().unwrap().remove(i);
            assert!(eval(&j, "cuda").is_err(), "{i}");
            let mut j = both("cuda");
            let e = j.0["informational"][i].clone();
            j.0["informational"].as_array_mut().unwrap().push(e);
            assert!(eval(&j, "cuda").is_err(), "{i}");
            let mut j = both("cuda");
            j.0["informational"][i]["value"] = Value::Null;
            assert!(eval(&j, "cuda").is_err(), "{i}");
            let mut j = both("cuda");
            let e = j.0["informational"][i].clone();
            j.0["metrics"].as_array_mut().unwrap().push(e);
            assert!(eval(&j, "cuda").is_err(), "{i}");
        }
        let mut j = both("cuda");
        j.0["informational"]
            .as_array_mut()
            .unwrap()
            .push(json!({"id": "informational.other", "value": 1.0}));
        assert!(eval(&j, "cuda").is_err());
        let mut j = both("cuda");
        j.0["informational"] = Value::Null;
        assert!(eval(&j, "cuda").is_err());
        let mut j = both("cuda");
        j.0["informational"][1]["scenarios"] = Value::Null;
        assert!(eval(&j, "cuda").is_err());
    }

    #[test]
    fn m2_evaluate_enforces_the_planned_backends() {
        // A CPU-pinned metric that ran on CUDA.
        let mut j = both("cuda");
        j.0["metrics"][0]["backend"] = json!("cuda");
        assert!(eval(&j, "cuda").is_err());
        // A selected-backend metric (asr.echo_leak) that ran elsewhere.
        let mut j = both("cuda");
        j.0["metrics"][2]["backend"] = json!("cpu");
        assert!(eval(&j, "cuda").is_err());
        // The evaluation ran on another backend than selected.
        assert!(eval(&both("cpu"), "cuda").is_err());
        // A load whose log disagrees fails its check.
        let mut j = both("cuda");
        j.0["loads"][1]["whisper_log"] = json!("unknown");
        let out = eval(&j, "cuda").unwrap();
        assert!(!check(&out, "base.en on cuda").pass);
        assert!(check(&out, "base.en on cpu").pass);
    }

    #[test]
    fn m2_backend_follows_doctor_unless_requested() {
        assert_eq!(select_backend(None, true).unwrap(), "cuda");
        assert_eq!(select_backend(None, false).unwrap(), "cpu");
        assert_eq!(select_backend(Some("cpu"), true).unwrap(), "cpu");
        assert_eq!(select_backend(Some("cuda"), true).unwrap(), "cuda");
        assert!(select_backend(Some("cuda"), false).is_err());
        assert!(select_backend(Some("vulkan"), true).is_err());
    }

    #[test]
    fn m2_eval_builds_cuda_only_for_cuda() {
        let p = Path::new("x");
        let cuda: Vec<String> = eval_args(p, "cuda", p, p)
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert!(cuda.windows(2).any(|w| w == ["--features", "cuda"]));
        assert!(cuda.windows(2).any(|w| w == ["--backend", "cuda"]));
        assert!(cuda.contains(&"--release".to_string()));
        let cpu: Vec<String> = eval_args(p, "cpu", p, p)
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert!(!cpu.contains(&"--features".to_string()));
        // The production evaluation: built (never `cargo run`), with the same features.
        let build = prod_build_args("cuda");
        assert!(build.windows(2).any(|w| w == ["--features", "cuda"]));
        assert!(build.windows(2).any(|w| w == ["--example", "m2_prod"]));
        assert!(build.windows(2).any(|w| w == ["-p", "earshot-runtime"]));
        assert_eq!(build[0], "build");
        assert!(build.contains(&"--release".to_string()));
        assert!(!prod_build_args("cpu").contains(&"--features".to_string()));
        let run: Vec<String> = prod_eval_args(p, "cuda", p, p)
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(run[0], "x");
        assert!(run.windows(2).any(|w| w == ["--backend", "cuda"]));
    }

    #[test]
    fn m2_prod_seed_rows_must_report_unscored_words() {
        // m2_prod transcribes whole tracks: words outside every window are reported per seed.
        for k in ["clean", "aec"] {
            let mut j = both("cpu");
            let a = aec_delta_index(&j.1);
            j.1["metrics"][a]["scenarios"][0]["per_seed"][1]["unscored_words"][k] = Value::Null;
            assert!(eval(&j, "cpu").is_err(), "{k}");
        }
        // asr_eval's informational base.en rows (per-window decodes) do not carry them.
        let mut j = both("cpu");
        for scn in j.0["informational"][1]["scenarios"].as_array_mut().unwrap() {
            for row in scn["per_seed"].as_array_mut().unwrap() {
                row.as_object_mut().unwrap().remove("unscored_words");
            }
        }
        let out = eval(&j, "cpu").unwrap();
        let d = check(&out, "informational: asr.wer_aec_delta on base.en")
            .detail
            .clone()
            .unwrap();
        assert!(d.contains("7 -0.50 (110 words)"), "{d}");
    }

    #[test]
    fn m2_prod_load_evidence_must_place_every_session_on_the_backend() {
        assert!(placement_names(&placement("cuda"), "cuda"));
        assert!(placement_names(&placement("cpu"), "cpu"));
        // A CUDA request whose sessions ran on the CPU only.
        assert!(!placement_names(&placement("cpu"), "cuda"));
        // One session off the GPU.
        let partial = placement("cuda").replace(
            "placed: CUDAExecutionProvider=13",
            "placed: CPUExecutionProvider=13",
        );
        assert!(!placement_names(&partial, "cuda"));
        assert!(!placement_names("", "cuda"));
        assert!(!placement_names("whisper.cpp log: cuda", "cuda"));
        assert!(!placement_names(&placement("cuda"), "vulkan"));
        // The runner's check fails on such evidence.
        let mut j = both("cuda");
        j.1["loads"][0]["evidence"] = json!(partial);
        let out = eval(&j, "cuda").unwrap();
        assert!(!check(&out, "(live) on cuda").pass);
        assert!(check(&out, "(final pass) on cuda").pass);
    }

    #[test]
    #[cfg_attr(not(windows), ignore = "Windows path syntax")]
    fn m2_release_dir_follows_cargo_target_dir() {
        let root = Path::new(r"C:\repo");
        assert_eq!(
            release_dir(root, None),
            Path::new(r"C:\repo\target\release")
        );
        assert_eq!(
            release_dir(root, Some(std::ffi::OsStr::new(""))),
            Path::new(r"C:\repo\target\release")
        );
        assert_eq!(
            release_dir(root, Some(std::ffi::OsStr::new(r"C:\eb-m2"))),
            Path::new(r"C:\eb-m2\release")
        );
        assert_eq!(
            release_dir(root, Some(std::ffi::OsStr::new("short"))),
            Path::new(r"C:\repo\short\release")
        );
    }

    #[test]
    fn m2_prod_executable_is_read_from_cargo_messages() {
        let msgs = concat!(
            r#"{"reason":"compiler-artifact","target":{"name":"model_eval"},"executable":"C:\\t\\model_eval.exe"}"#,
            "\n",
            r#"{"reason":"compiler-artifact","target":{"name":"m2_prod"},"executable":"C:\\t\\release\\examples\\m2_prod.exe"}"#,
            "\n"
        );
        assert_eq!(
            prod_executable(msgs),
            Some(std::path::PathBuf::from(
                r"C:\t\release\examples\m2_prod.exe"
            ))
        );
        assert_eq!(prod_executable("not json\n"), None);
    }
}
