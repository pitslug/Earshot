//! ci step `ui` (T7.1): the pnpm workspace (`apps/desktop`, `packages/ui`).
//!
//! Runs first in `cargo xtask ci`, because the desktop app's `tauri::generate_context!`
//! needs the built front end (`apps/desktop/dist`) when the workspace compiles. Installs
//! from the committed lockfile, builds every package (type-check, Vite build) and runs
//! every package's tests. Each command goes through [`run_in_job_with_timeout`]; a result counts
//! only with exit 0 and completely captured output.

use std::path::Path;
use std::process::Command;
use std::time::Duration;

use anyhow::{Result, bail};

use crate::proc::{CmdOutput, run_in_job_with_timeout};

/// One pnpm command of the step: name, pnpm arguments, deadline.
pub type UiStep = (&'static str, &'static [&'static str], Duration);

/// The pnpm commands, in order.
pub const UI_STEPS: &[UiStep] = &[
    (
        "install",
        &["install", "--frozen-lockfile"],
        Duration::from_secs(10 * 60),
    ),
    ("build", &["-r", "build"], Duration::from_secs(10 * 60)),
    ("test", &["-r", "test"], Duration::from_secs(10 * 60)),
];

/// A `pnpm` command in `root`. On Windows pnpm is a `.cmd` shim, run as `cmd /c pnpm`
/// (AGENTS.md conventions). `CI=true` keeps pnpm from prompting; `NO_COLOR` keeps logs plain.
pub fn pnpm_command(root: &Path, args: &[&str]) -> Command {
    let mut cmd = if cfg!(windows) {
        let mut c = Command::new("cmd");
        c.args(["/c", "pnpm"]);
        c
    } else {
        Command::new("pnpm")
    };
    cmd.args(args)
        .current_dir(root)
        .env("CI", "true")
        .env("NO_COLOR", "1");
    cmd
}

/// The verdict on one captured pnpm run: complete output and exit 0, else an error with
/// everything captured.
pub fn evaluate(args: &[&str], out: &CmdOutput) -> Result<()> {
    let what = format!("ui: `pnpm {}`", args.join(" "));
    out.require_complete(&what)?;
    if !out.success {
        bail!("{what} failed: {}", out.describe());
    }
    Ok(())
}

/// Runs the step on the repo at `root`.
pub fn run(root: &Path) -> Result<()> {
    for (name, args, deadline) in UI_STEPS {
        println!("ui: {name}: pnpm {}", args.join(" "));
        let mut cmd = pnpm_command(root, args);
        // In a kill-on-close job: a timeout stops pnpm and node too, not only `cmd`.
        let Some(out) = run_in_job_with_timeout(&mut cmd, *deadline) else {
            bail!(
                "ui: `pnpm {}` could not start or exceeded {} s (is pnpm on PATH?)",
                args.join(" "),
                deadline.as_secs()
            );
        };
        evaluate(args, &out)?;
        if *name == "test" {
            print!("{}", out.stdout);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ui_installs_from_the_lockfile_then_builds_then_tests() {
        let names: Vec<&str> = UI_STEPS.iter().map(|s| s.0).collect();
        assert_eq!(names, ["install", "build", "test"]);
        assert_eq!(UI_STEPS[0].1, ["install", "--frozen-lockfile"]);
        assert_eq!(UI_STEPS[1].1, ["-r", "build"]);
        assert_eq!(UI_STEPS[2].1, ["-r", "test"]);
        for (name, _, deadline) in UI_STEPS {
            assert!(*deadline >= Duration::from_secs(5 * 60), "{name}");
            assert!(*deadline <= Duration::from_secs(10 * 60), "{name}");
        }
    }

    #[test]
    fn ui_pnpm_runs_through_cmd_on_windows_in_the_repo_root() {
        let root = crate::repo_root();
        let cmd = pnpm_command(&root, &["-r", "test"]);
        let args: Vec<String> = cmd
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        if cfg!(windows) {
            assert_eq!(cmd.get_program(), "cmd");
            assert_eq!(args, ["/c", "pnpm", "-r", "test"]);
        } else {
            assert_eq!(cmd.get_program(), "pnpm");
            assert_eq!(args, ["-r", "test"]);
        }
        assert_eq!(cmd.get_current_dir(), Some(root.as_path()));
    }

    fn captured(success: bool, complete: bool) -> CmdOutput {
        CmdOutput {
            success,
            code: Some(if success { 0 } else { 1 }),
            stdout: "Tests 18 passed".into(),
            stderr: String::new(),
            stdout_complete: complete,
            stderr_complete: true,
        }
    }

    #[test]
    fn ui_passes_only_complete_successful_runs() {
        let args = ["-r", "test"];
        assert!(evaluate(&args, &captured(true, true)).is_ok());
        let err = format!("{:#}", evaluate(&args, &captured(true, false)).unwrap_err());
        assert!(err.contains("not fully captured"), "{err}");
        let err = format!("{:#}", evaluate(&args, &captured(false, true)).unwrap_err());
        assert!(err.contains("pnpm -r test"), "{err}");
        assert!(evaluate(&args, &captured(false, false)).is_err());
    }

    #[test]
    fn ui_workspace_files_are_committed_where_the_step_expects_them() {
        let root = crate::repo_root();
        for file in [
            "package.json",
            "pnpm-workspace.yaml",
            "pnpm-lock.yaml",
            "apps/desktop/package.json",
            "packages/ui/package.json",
        ] {
            assert!(root.join(file).is_file(), "{file}");
        }
        let workspace = std::fs::read_to_string(root.join("pnpm-workspace.yaml")).unwrap();
        assert!(workspace.contains("apps/desktop"), "{workspace}");
        assert!(workspace.contains("packages/ui"), "{workspace}");
    }
}
