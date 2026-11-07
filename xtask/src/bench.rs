//! `cargo xtask bench asr|diar|aec --backend <b>` (T8.1): builds earshot-runtime's `bench`
//! example in release for the backend, runs one part on the benchmark meeting and records its
//! real-time factor, live chunk latency and peak memory in
//! `acceptance/results/bench-<host>-<backend>.json` (`<host>` is the target triple: no machine
//! name goes into the repo). The definitions are in the example's module docs.
//!
//! A results file keeps one entry per part; a run replaces its own part and keeps the others.
//! Each entry has its commit, time and `note`: `--note` says what else the host was doing, and
//! a timing with a note saying the host was shared is not a performance figure. Nothing here
//! gates: PLAN.md 4.2 has no threshold on these. `diar` and `aec` run only on the CPU, as they
//! do in production. Other benchmarks answer other questions: `model-eval` compares models,
//! `t613 timing` measures a whole CLI replay, Settings > Benchmark the user's own settings.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail, ensure};
use serde_json::{Map, Value, json};

use crate::ci::cargo_command;
use crate::model_eval::{example_build_args, example_executable, select_backend};
use crate::proc::run_with_timeout;

/// Deadline for the release build (a cold GPU build of whisper.cpp and llama.cpp).
const BUILD_TIMEOUT: Duration = Duration::from_secs(60 * 60);
/// Deadline for one part (the meeting is 150 s; a CPU decode of a slow model takes minutes).
const RUN_TIMEOUT: Duration = Duration::from_secs(60 * 60);
/// The example.
const EXAMPLE: &str = "bench";
/// The parts, in file order.
pub const PARTS: [&str; 3] = ["asr", "diar", "aec"];

/// One run.
#[derive(Debug, Clone, Default)]
pub struct Options {
    /// `asr`, `diar` or `aec`.
    pub part: String,
    /// `cpu`, `cuda`, `vulkan` or `qnn`; `None` picks as `model-eval` does.
    pub backend: Option<String>,
    /// `asr` only: another model than the production default.
    pub model: Option<String>,
    /// What else the host was doing.
    pub note: Option<String>,
    /// Build here (a Vulkan build otherwise uses `cargo xtask build`'s short directory).
    pub target_dir: Option<PathBuf>,
}

/// The results file of `host` and `backend`.
#[must_use]
pub fn results_path(root: &Path, host: &str, backend: &str) -> PathBuf {
    root.join("acceptance/results")
        .join(format!("bench-{host}-{backend}.json"))
}

/// Refuses what production never runs.
///
/// # Errors
/// An unknown part; `diar` or `aec` on anything but the CPU; `--model` on them.
pub fn check_part(opts: &Options, backend: &str) -> Result<()> {
    ensure!(
        PARTS.contains(&opts.part.as_str()),
        "bench runs asr, diar or aec, not {}",
        opts.part
    );
    if opts.part != "asr" {
        ensure!(
            backend == "cpu",
            "{} runs on the CPU only in production (T6.11); use --backend cpu",
            opts.part
        );
        ensure!(opts.model.is_none(), "{} has no --model", opts.part);
    }
    Ok(())
}

/// `existing` (a results file's JSON, if any) with `part` replaced by `entry`.
///
/// # Errors
/// An existing file that is not this host's and backend's, or not an object.
pub fn merged(
    existing: Option<&str>,
    host: &str,
    backend: &str,
    part: &str,
    entry: Value,
) -> Result<Value> {
    let mut root: Map<String, Value> = match existing {
        Some(text) => {
            serde_json::from_str(text).context("the results file is not a JSON object")?
        }
        None => Map::new(),
    };
    for (key, want) in [("host", host), ("backend", backend)] {
        match root.get(key) {
            Some(v) if v != want => bail!("the results file says {key} {v}, this run {want}"),
            _ => {
                root.insert(key.into(), json!(want));
            }
        }
    }
    root.insert(part.into(), entry);
    Ok(Value::Object(root))
}

/// The example's arguments.
#[must_use]
pub fn run_args(
    opts: &Options,
    root: &Path,
    backend: &str,
    models: &Path,
    scratch: &Path,
) -> Vec<std::ffi::OsString> {
    let mut args: Vec<std::ffi::OsString> = vec![
        opts.part.clone().into(),
        "--root".into(),
        root.into(),
        "--backend".into(),
        backend.into(),
        "--models".into(),
        models.into(),
        "--scratch".into(),
        scratch.into(),
    ];
    if let Some(m) = &opts.model {
        args.extend(["--model".into(), m.into()]);
    }
    args
}

/// Builds and runs one part and updates the results file.
///
/// # Errors
/// A refused backend or part, a failed build or run, output that was cut off, or a results
/// file that cannot be read or written.
pub fn run(root: &Path, opts: &Options) -> Result<()> {
    let report = crate::doctor::collect();
    let cuda = crate::doctor::cuda_for_backend_choice(&report, opts.backend.as_deref())?;
    let vulkan = crate::doctor::reports_vulkan_sdk(&report);
    let backend = select_backend(opts.backend.as_deref(), cuda, vulkan)?;
    check_part(opts, backend)?;
    let target_dir = opts.target_dir.clone().or_else(|| {
        (backend == "vulkan").then(|| {
            crate::build::short_target_dir(root, None, std::env::var_os("SystemDrive"), None)
        })
    });
    let mut build = cargo_command();
    build
        .args(example_build_args(EXAMPLE, backend, target_dir.as_deref()))
        .current_dir(root);
    eprintln!("bench: building the bench example ({backend}, release)");
    let built = run_with_timeout(&mut build, BUILD_TIMEOUT)
        .context("building the bench example did not finish within the deadline")?;
    if !built.success {
        bail!("building the bench example failed: {}", built.describe());
    }
    built.require_complete("building the bench example")?;
    let exe = example_executable(&built.stdout, EXAMPLE).context("cargo reported no bench")?;
    // The session code links llama.cpp's DLLs (the live summary thread's path); the exe is
    // spawned directly, so cargo's PATH does not find them.
    crate::llama_dlls::install_beside(&built.stdout, &exe)?;
    let models = crate::models::cache_dir()?;
    let scratch = root.join("target/bench");
    std::fs::create_dir_all(&scratch).with_context(|| format!("create {}", scratch.display()))?;
    let mut command = std::process::Command::new(&exe);
    command
        .args(run_args(opts, root, backend, &models, &scratch))
        .current_dir(root);
    eprintln!("bench: running {} on {backend}", opts.part);
    let ran = run_with_timeout(&mut command, RUN_TIMEOUT)
        .context("the bench run did not finish within the deadline")?;
    eprint!("{}", ran.stderr);
    if !ran.success {
        bail!("the bench run failed: {}", ran.describe());
    }
    ran.require_complete("the bench run")?;
    let mut entry: Value =
        serde_json::from_str(ran.stdout.trim()).context("the bench run printed no JSON")?;
    entry["commit"] = json!(crate::accept::git_commit(root));
    entry["timestamp"] = json!(crate::timefmt::now_utc());
    entry["note"] = json!(opts.note);
    let host = crate::accept::host_triple();
    let path = results_path(root, &host, backend);
    let existing = match std::fs::read_to_string(&path) {
        Ok(text) => Some(text),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(e).with_context(|| format!("read {}", path.display())),
    };
    let merged = merged(existing.as_deref(), &host, backend, &opts.part, entry)?;
    let mut text = serde_json::to_string_pretty(&merged)?;
    text.push('\n');
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    }
    std::fs::write(&path, text).with_context(|| format!("write {}", path.display()))?;
    let part = &merged[opts.part.as_str()];
    println!(
        "bench {} on {backend}: rtf {:.3}, chunk latency p50 {:.2} ms p95 {:.2} ms, peak {} MB",
        opts.part,
        part["rtf"].as_f64().unwrap_or(f64::NAN),
        part["live"]["latency_ms"]["p50"]
            .as_f64()
            .unwrap_or(f64::NAN),
        part["live"]["latency_ms"]["p95"]
            .as_f64()
            .unwrap_or(f64::NAN),
        part["peak_memory_mb"]["working_set"],
    );
    println!("bench: {}", path.display());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn options(part: &str) -> Options {
        Options {
            part: part.into(),
            ..Options::default()
        }
    }

    #[test]
    fn bench_results_file_is_named_by_host_triple_and_backend() {
        assert_eq!(
            results_path(Path::new("r"), "x86_64-pc-windows-msvc", "vulkan"),
            Path::new("r/acceptance/results/bench-x86_64-pc-windows-msvc-vulkan.json")
        );
    }

    #[test]
    fn bench_diar_and_aec_run_on_the_cpu_only() {
        assert!(check_part(&options("asr"), "vulkan").is_ok());
        for part in ["diar", "aec"] {
            assert!(check_part(&options(part), "cpu").is_ok());
            assert!(check_part(&options(part), "cuda").is_err());
            let with_model = Options {
                model: Some("base.en".into()),
                ..options(part)
            };
            assert!(check_part(&with_model, "cpu").is_err());
        }
        assert!(check_part(&options("summary"), "cpu").is_err());
    }

    #[test]
    fn bench_a_run_replaces_its_part_and_keeps_the_others() {
        let first = merged(None, "h", "cpu", "asr", json!({"rtf": 1.0})).unwrap();
        let second = merged(
            Some(&first.to_string()),
            "h",
            "cpu",
            "aec",
            json!({"rtf": 2.0}),
        )
        .unwrap();
        assert_eq!(second["asr"]["rtf"], 1.0);
        assert_eq!(second["aec"]["rtf"], 2.0);
        let third = merged(
            Some(&second.to_string()),
            "h",
            "cpu",
            "asr",
            json!({"rtf": 3.0}),
        )
        .unwrap();
        assert_eq!(third["asr"]["rtf"], 3.0);
        assert_eq!(third["host"], "h");
    }

    #[test]
    fn bench_refuses_a_results_file_of_another_host_or_backend() {
        let text = merged(None, "h", "cpu", "asr", json!({}))
            .unwrap()
            .to_string();
        assert!(merged(Some(&text), "other", "cpu", "asr", json!({})).is_err());
        assert!(merged(Some(&text), "h", "cuda", "asr", json!({})).is_err());
        assert!(merged(Some("[]"), "h", "cpu", "asr", json!({})).is_err());
    }

    #[test]
    fn bench_passes_the_model_through() {
        let opts = Options {
            model: Some("base.en".into()),
            ..options("asr")
        };
        let p = Path::new("x");
        let joined = run_args(&opts, p, "vulkan", p, p)
            .into_iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join(" ");
        assert!(
            joined.starts_with("asr --root x --backend vulkan"),
            "{joined}"
        );
        assert!(joined.ends_with("--model base.en"), "{joined}");
    }

    #[test]
    fn bench_is_built_through_the_shared_example_arguments() {
        let args = example_build_args(EXAMPLE, "vulkan", Some(Path::new(r"C:\eb\x")));
        assert!(args.windows(2).any(|w| w == ["--example", "bench"]));
        assert!(args.windows(2).any(|w| w == ["--features", "vulkan"]));
        assert!(args.windows(2).any(|w| w == ["--target-dir", r"C:\eb\x"]));
        assert_eq!(
            example_executable(
                r#"{"reason":"compiler-artifact","target":{"name":"bench"},"executable":"C:/t/bench.exe"}"#,
                EXAMPLE
            ),
            Some(PathBuf::from("C:/t/bench.exe"))
        );
    }
}
