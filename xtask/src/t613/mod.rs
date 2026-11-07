//! `cargo xtask t613`: the acceptance measurements of T6.13 (transcription during the
//! meeting, D20) that are not unit tests.
//!
//! - `meeting`: generates the reproducible two-track meetings into the fixture cache
//!   ([`meeting`]): the LibriSpeech call of any length (the 60-minute timing meeting) and
//!   the pinned AMI meetings as calls.
//! - `timing`: replays the LibriSpeech meeting at wall-clock speed through the shipped CLI
//!   with the production models and reports how long after its end each speech stretch was
//!   stored and shown, and how long after the end of the meeting the final transcript and
//!   speakers were stored ([`timing`]).
//! - `quality`: replays each input twice, the cleanup reusing the live work (the default)
//!   and with `--whole-pass` (the whole-meeting pass, same models), and compares them:
//!   WER and DER against the references where they exist (LibriSpeech, AMI), and word and
//!   speaker agreement between the two passes on the private GATE-12 recordings, as counts
//!   only ([`quality`]).
//!
//! Every replay builds the CLI first (release, `--features cuda` exactly when `cargo xtask
//! doctor` reports CUDA, unless `--backend cpu`) and runs the built executable under
//! [`run_with_timeout`] (never `cargo run`). Nothing here gates: there is no PLAN 4.2
//! threshold for T6.13; the reports state the plan's targets beside the numbers. Reports go
//! to `target/t613/`.

pub mod meeting;
pub mod quality;
pub mod timing;

/// The `asr_eval` reference rules (`refs::parse_ami_words`), compiled here too so the AMI
/// references of both are one rule.
#[allow(dead_code, clippy::pedantic)]
#[path = "../../../crates/earshot-asr-whisper/examples/asr_eval/refs.rs"]
mod ami_refs;
#[path = "../../../crates/earshot-asr-whisper/examples/asr_eval/gate12_private.rs"]
pub(crate) mod gate12_private;

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

use crate::ci::cargo_command;
use crate::proc::run_with_timeout;

/// Deadline for building the CLI (release, whisper.cpp and CUDA kernels from cold).
const BUILD_TIMEOUT: Duration = Duration::from_secs(90 * 60);

/// How to build the CLI.
#[derive(Debug, Clone, Default)]
pub struct Build {
    /// `cpu` or `cuda` (default: `cuda` when doctor reports it).
    pub backend: Option<String>,
    /// Cargo target directory (a CUDA build in a deep path fails CMake's compiler probe on
    /// the desktop: use a short one).
    pub target_dir: Option<PathBuf>,
}

impl Build {
    /// The backend of the build: `cuda` or `cpu`.
    ///
    /// # Errors
    /// As `accept m2`'s backend choice.
    pub fn backend(&self) -> Result<&'static str> {
        let cuda = crate::doctor::cuda_for_backend_choice(
            &crate::doctor::collect(),
            self.backend.as_deref(),
        )?;
        crate::accept::m2::select_backend(self.backend.as_deref(), cuda)
    }

    /// The `cargo build` arguments.
    #[must_use]
    pub fn args(&self, backend: &str) -> Vec<String> {
        let mut a: Vec<String> = [
            "build",
            "--release",
            "-p",
            "earshot-cli",
            "--bin",
            "earshot",
            "--message-format=json-render-diagnostics",
        ]
        .map(Into::into)
        .to_vec();
        if backend == "cuda" {
            a.extend(["--features".into(), "cuda".into()]);
        }
        if let Some(d) = &self.target_dir {
            a.extend(["--target-dir".into(), d.display().to_string()]);
        }
        a
    }

    /// Builds the CLI and returns the executable.
    ///
    /// # Errors
    /// A failed or timed-out build, or one whose whisper.cpp was not optimised.
    pub fn build(&self, root: &Path) -> Result<PathBuf> {
        let backend = self.backend()?;
        let mut cmd = cargo_command();
        cmd.args(self.args(backend)).current_dir(root);
        eprintln!("t613: building earshot-cli (release, {backend})");
        let out = run_with_timeout(&mut cmd, BUILD_TIMEOUT)
            .context("building earshot-cli did not finish within the deadline")?;
        if !out.success {
            bail!("building earshot-cli failed: {}", out.describe());
        }
        out.require_complete("building earshot-cli")?;
        let exe = crate::accept::m6::cli_executable(&out.stdout)
            .context("cargo reported no earshot executable")?;
        if let Some(profile) = exe.parent() {
            crate::native_flags::check_optimised(profile, "--release")?;
        }
        // Spawned directly: llama.cpp's DLLs go beside it (T6.21b).
        crate::llama_dlls::install_beside(&out.stdout, &exe)?;
        Ok(exe)
    }

    /// For the report.
    #[must_use]
    pub fn describe(&self) -> Value {
        json!({
            "requested_backend": self.backend,
            "built_backend": self.backend().ok(),
            "target_dir": self.target_dir,
        })
    }
}

/// Sets the models directory of a CLI run (the `cargo xtask fetch-models` cache).
///
/// # Errors
/// No models directory.
pub fn cli_env(cmd: &mut Command) -> Result<()> {
    cmd.env("EARSHOT_MODELS", crate::models::cache_dir()?);
    Ok(())
}

/// Host facts for a report.
#[must_use]
pub fn host_facts() -> Value {
    json!({
        "os": std::env::consts::OS,
        "arch": std::env::consts::ARCH,
        "threads": std::thread::available_parallelism().map(std::num::NonZero::get).ok(),
    })
}

/// The `model: ...` line the CLI printed (which model and why).
#[must_use]
pub fn parse_model_line(stdout: &str) -> Option<String> {
    stdout
        .lines()
        .find_map(|l| l.strip_prefix("model: "))
        .map(str::to_owned)
}

/// The model of `earshot benchmark`'s `recommendation: <model> on <Backend>; ...` line.
#[must_use]
pub fn parse_recommendation(stdout: &str) -> Option<String> {
    let rest = stdout
        .lines()
        .find_map(|l| l.strip_prefix("recommendation: "))?;
    let (model, _) = rest.split_once(" on ")?;
    (!model.contains(' ')).then(|| model.to_owned())
}

/// The backend lines the CLI printed: live mic, live loopback, final pass (the rest of the
/// line after the prefix; `None` when missing).
#[must_use]
pub fn printed_backends(stdout: &str) -> [Option<String>; 3] {
    let after = |prefix: &str| {
        stdout
            .lines()
            .find_map(|l| l.strip_prefix(prefix))
            .map(str::to_owned)
    };
    [
        after("live asr mic: "),
        after("live asr reference: "),
        after("post: final pass ran on "),
    ]
}

/// The cleanup's lines saying, per track, whether it took the live stretches and speaker
/// windows or did the whole track (no transcript text in them).
#[must_use]
pub fn reuse_lines(stdout: &str) -> Vec<String> {
    stdout
        .lines()
        .filter(|l| {
            (l.starts_with("post: mic: ") || l.starts_with("post: reference: "))
                && (l.contains(" took ")
                    || l.contains("transcribed the whole track")
                    || l.contains("found the speakers over the whole track"))
        })
        .map(str::to_owned)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn t613_cli_lines_are_parsed() {
        let out = "model: parakeet-tdt-0.6b-v2 on Cuda (from the model check)\n\
            live asr mic: Cuda (CUDA0, parakeet-tdt-0.6b-v2)\n\
            live asr reference: Cuda (CUDA0, parakeet-tdt-0.6b-v2)\n\
            [00:00:01] Me: hello there\n\
            post: final pass ran on Cuda (CUDA0, parakeet-tdt-0.6b-v2); 3 segments, 2 speakers\n\
            post: mic: took 4 live speech piece(s) up to 00:01:00, transcribed only the rest\n\
            post: reference: transcribed the whole track (the whole-meeting pass was requested)\n\
            post: mic: found the speakers over the whole track (no speaker record)\n\
            post: mic: took 32 speaker window(s) from the meeting, computed only the rest\n";
        assert_eq!(
            parse_model_line(out).as_deref(),
            Some("parakeet-tdt-0.6b-v2 on Cuda (from the model check)")
        );
        let b = printed_backends(out);
        assert!(b[0].as_deref().unwrap().starts_with("Cuda"));
        assert!(b[2].as_deref().unwrap().starts_with("Cuda (CUDA0"));
        assert_eq!(printed_backends("")[1], None);
        let r = reuse_lines(out);
        assert_eq!(r.len(), 4, "{r:?}");
        assert!(r.iter().all(|l| !l.contains("hello")));
        assert_eq!(
            parse_recommendation(
                "recommendation: parakeet-tdt-0.6b-v2 on Cuda; fallback when a meeting falls behind: parakeet-tdt-110m"
            )
            .as_deref(),
            Some("parakeet-tdt-0.6b-v2")
        );
        assert_eq!(
            parse_recommendation("recommendation: none (no installed model works)"),
            None
        );
    }

    #[test]
    fn t613_build_uses_cuda_only_when_chosen_and_the_given_target_dir() {
        let b = Build {
            backend: Some("cpu".into()),
            target_dir: Some(PathBuf::from(r"C:\eb")),
        };
        let cpu = b.args("cpu");
        assert!(!cpu.contains(&"--features".to_owned()));
        assert!(cpu.windows(2).any(|w| w == ["--target-dir", r"C:\eb"]));
        let cuda = Build::default().args("cuda");
        assert!(cuda.windows(2).any(|w| w == ["--features", "cuda"]));
        assert!(!cuda.contains(&"--target-dir".to_owned()));
    }
}
