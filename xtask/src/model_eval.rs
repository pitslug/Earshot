//! `cargo xtask model-eval` (T6.10e): builds the `model_eval` example of `earshot-runtime` in
//! release (with `cuda` for `--backend cuda`, `vulkan` for `--backend vulkan`, T6.10d) and runs
//! it on the cached models and fixtures. A Vulkan build uses the short target directory of
//! `cargo xtask build` (ggml's nested Vulkan shader build fails under a deep path).
//! Metric definitions live in that example's module docs. The report goes to
//! `docs/model-eval/<backend>.json` unless `--out` says otherwise.
//!
//! The build goes through `run_with_timeout`; the evaluation itself runs with inherited
//! output (it takes from minutes on a GPU to hours on a CPU and prints its progress and one
//! `RESULT` line per model), so Ctrl+C stops it.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde_json::Value;

use crate::ci::cargo_command;
use crate::proc::run_with_timeout;

/// Deadline for the release build (a cold `cuda` build of whisper.cpp takes about 4 minutes).
const BUILD_TIMEOUT: Duration = Duration::from_secs(60 * 60);

/// What to evaluate; empty lists mean everything.
#[derive(Debug, Default, Clone)]
pub struct Options {
    /// `cpu`, `cuda`, `vulkan` or `qnn`; `None` picks CUDA when doctor reports it, else the
    /// CPU.
    pub backend: Option<String>,
    /// Candidate names (the example's list).
    pub models: Vec<String>,
    /// Corpora: librispeech, ami, gate12, silence (the default), librispeech_dev, smoke,
    /// ami_sdm, speed.
    pub corpora: Vec<String>,
    /// Whisper decodings: greedy, beam.
    pub decodings: Vec<String>,
    /// Report path (default `docs/model-eval/<backend>.json`).
    pub out: Option<PathBuf>,
    /// Build here (a Vulkan build otherwise uses `cargo xtask build`'s short directory).
    pub target_dir: Option<PathBuf>,
}

/// The backend to run: `requested` if given, else CUDA when doctor reports it, else the CPU.
/// Vulkan is never picked by default.
///
/// # Errors
/// An unsupported backend, `cuda` on a host where doctor does not report CUDA, or `vulkan`
/// where it does not report the Vulkan SDK.
pub fn select_backend(requested: Option<&str>, cuda: bool, vulkan: bool) -> Result<&'static str> {
    match requested {
        None if cuda => Ok("cuda"),
        None | Some("cpu") => Ok("cpu"),
        Some("cuda") if cuda => Ok("cuda"),
        // QNN builds with the `qnn` feature on any host: the NPU on Windows ARM64, Qualcomm's
        // HTP simulator on x64 (`cargo xtask fetch-models --set qnn` / `--set qnn-x64`).
        Some("qnn") => Ok("qnn"),
        Some("cuda") => bail!(
            "--backend cuda, but `cargo xtask doctor` does not report the CUDA toolkit and an \
             NVIDIA GPU"
        ),
        Some("vulkan") if vulkan => Ok("vulkan"),
        Some("vulkan") => {
            bail!("--backend vulkan, but `cargo xtask doctor` does not report the Vulkan SDK")
        }
        Some(other) => {
            bail!("model-eval supports --backend cpu, cuda, vulkan or qnn, not {other}")
        }
    }
}

/// The `cargo build` arguments of the example; `target_dir` overrides cargo's.
#[must_use]
pub fn build_args(backend: &str, target_dir: Option<&Path>) -> Vec<String> {
    example_build_args("model_eval", backend, target_dir)
}

/// The `cargo build` arguments of an earshot-runtime example built for `backend`.
#[must_use]
pub fn example_build_args(example: &str, backend: &str, target_dir: Option<&Path>) -> Vec<String> {
    let mut args: Vec<String> = ["build", "--release", "-p", "earshot-runtime"]
        .map(Into::into)
        .to_vec();
    if backend == "cuda" || backend == "vulkan" || backend == "qnn" {
        args.extend(["--features".into(), backend.into()]);
    }
    if let Some(dir) = target_dir {
        args.extend(["--target-dir".into(), dir.display().to_string()]);
    }
    args.extend(
        [
            "--example",
            example,
            "--message-format=json-render-diagnostics",
        ]
        .map(Into::into),
    );
    args
}

/// The example's arguments.
#[must_use]
pub fn eval_args(
    root: &Path,
    backend: &str,
    models: &Path,
    fixtures: &Path,
    out: &Path,
    opts: &Options,
) -> Vec<std::ffi::OsString> {
    let mut args: Vec<std::ffi::OsString> = vec![
        "--root".into(),
        root.into(),
        "--backend".into(),
        backend.into(),
        "--models".into(),
        models.into(),
        "--fixtures".into(),
        fixtures.into(),
        "--out".into(),
        out.into(),
    ];
    for (flag, values) in [
        ("--model", &opts.models),
        ("--corpus", &opts.corpora),
        ("--decoding", &opts.decodings),
    ] {
        for v in values {
            args.push(flag.into());
            args.push(v.into());
        }
    }
    args
}

/// The executable of `example` named in cargo's JSON build messages.
#[must_use]
pub fn example_executable(messages: &str, example: &str) -> Option<PathBuf> {
    messages
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter(|m| m["reason"] == "compiler-artifact" && m["target"]["name"] == example)
        .find_map(|m| m["executable"].as_str().map(PathBuf::from))
}

/// Builds and runs the evaluation.
///
/// # Errors
/// Backend selection, a failed build, or a failed evaluation.
pub fn run(root: &Path, opts: &Options) -> Result<()> {
    let report = crate::doctor::collect();
    // An incomplete CUDA probe is an error here, never a silent CPU build.
    let cuda = crate::doctor::cuda_for_backend_choice(&report, opts.backend.as_deref())?;
    let vulkan = crate::doctor::reports_vulkan_sdk(&report);
    let backend = select_backend(opts.backend.as_deref(), cuda, vulkan)?;
    let models = crate::models::cache_dir()?;
    let fixtures = crate::fixtures::cache_dir()?;
    let out = opts
        .out
        .clone()
        .unwrap_or_else(|| root.join(format!("docs/model-eval/{backend}.json")));
    if let Some(dir) = out.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    }
    let mut build = cargo_command();
    let target_dir = opts.target_dir.clone().or_else(|| {
        (backend == "vulkan").then(|| {
            crate::build::short_target_dir(root, None, std::env::var_os("SystemDrive"), None)
        })
    });
    build
        .args(build_args(backend, target_dir.as_deref()))
        .current_dir(root);
    eprintln!("model-eval: building the model_eval example ({backend}, release)");
    let built = run_with_timeout(&mut build, BUILD_TIMEOUT)
        .context("building model_eval did not finish within the deadline")?;
    if !built.success {
        bail!("building model_eval failed: {}", built.describe());
    }
    // The executable path comes from cargo's JSON on stdout.
    built.require_complete("building model_eval")?;
    let exe =
        example_executable(&built.stdout, "model_eval").context("cargo reported no model_eval")?;
    let status = std::process::Command::new(&exe)
        .args(eval_args(root, backend, &models, &fixtures, &out, opts))
        .current_dir(root)
        .status()
        .with_context(|| format!("start {}", exe.display()))?;
    if !status.success() {
        bail!("model_eval failed ({status})");
    }
    eprintln!("model-eval: report in {}", out.display());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_eval_backend_selection() {
        assert_eq!(select_backend(None, true, false).unwrap(), "cuda");
        assert_eq!(select_backend(None, false, false).unwrap(), "cpu");
        assert_eq!(select_backend(None, false, true).unwrap(), "cpu");
        assert_eq!(select_backend(Some("cpu"), true, true).unwrap(), "cpu");
        assert!(select_backend(Some("cuda"), false, true).is_err());
        assert!(select_backend(Some("vulkan"), true, false).is_err());
        assert_eq!(
            select_backend(Some("vulkan"), false, true).unwrap(),
            "vulkan"
        );
        // T8.3: QNN is the NPU on ARM64 and Qualcomm's HTP simulator on x64, never a default.
        assert_eq!(select_backend(Some("qnn"), false, false).unwrap(), "qnn");
        assert_eq!(select_backend(None, false, false).unwrap(), "cpu");
        assert!(select_backend(Some("npu"), true, true).is_err());
    }

    #[test]
    fn model_eval_builds_with_cuda_only_for_cuda() {
        assert!(
            build_args("cuda", None)
                .windows(2)
                .any(|w| w == ["--features", "cuda"])
        );
        assert!(!build_args("cpu", None).iter().any(|a| a == "cuda"));
        assert!(!build_args("cpu", None).iter().any(|a| a == "--target-dir"));
        assert!(
            build_args("qnn", None)
                .windows(2)
                .any(|w| w == ["--features", "qnn"])
        );
        assert!(!build_args("cpu", None).iter().any(|a| a == "qnn"));
    }

    #[test]
    fn model_eval_builds_vulkan_in_the_given_target_dir() {
        let args = build_args("vulkan", Some(Path::new(r"C:\earshot-build")));
        assert!(args.windows(2).any(|w| w == ["--features", "vulkan"]));
        assert!(
            args.windows(2)
                .any(|w| w == ["--target-dir", r"C:\earshot-build"])
        );
        assert!(!args.iter().any(|a| a == "cuda"));
    }

    #[test]
    fn model_eval_passes_filters_through() {
        let opts = Options {
            models: vec!["base.en".into()],
            corpora: vec!["ami".into(), "silence".into()],
            decodings: vec!["greedy".into()],
            ..Options::default()
        };
        let p = Path::new("x");
        let args: Vec<String> = eval_args(p, "cpu", p, p, p, &opts)
            .into_iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        let joined = args.join(" ");
        assert!(joined.contains("--backend cpu"));
        assert!(joined.contains("--model base.en"));
        assert!(joined.contains("--corpus ami --corpus silence"));
        assert!(joined.contains("--decoding greedy"));
    }

    #[test]
    fn model_eval_finds_the_example_executable() {
        let msgs = concat!(
            r#"{"reason":"compiler-artifact","target":{"name":"earshot_runtime"},"executable":null}"#,
            "\n",
            r#"{"reason":"compiler-artifact","target":{"name":"model_eval"},"executable":"C:/t/model_eval.exe"}"#,
            "\n",
        );
        assert_eq!(
            example_executable(msgs, "model_eval"),
            Some(PathBuf::from("C:/t/model_eval.exe"))
        );
        assert_eq!(example_executable("", "model_eval"), None);
    }
}
