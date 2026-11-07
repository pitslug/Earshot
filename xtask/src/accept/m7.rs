//! `accept m7`: the desktop app, built as it ships, runs a replayed meeting (T7.6).
//!
//! 1. **Builds.** The app's release build with its front end compiled in (what `tauri build
//!    --no-bundle` runs: `--features tauri/custom-protocol`; `bundle.active` is off until T9.1,
//!    and going through cargo gives the executable and llama.cpp's DLLs from its JSON messages).
//!    The host's build always, and the other Windows architecture when `cross_toolchain_present`
//!    says it can build here. x64 is required by the plan, so an ARM64 host without the x64
//!    tools fails the `x64_build` check rather than skipping it; only the ARM64 build on an x64
//!    host may be skipped, and the check says so.
//! 2. **Replay.** The built executable's hidden mode (`crate::headless` in the app), on the
//!    meeting `accept m6` renders from `acceptance/m6_meeting.toml`, with the summary from the
//!    LLM mock on loopback (a shipped binary holds no mock, so `--llm-mock` of the plan's first
//!    wording became a loopback URL) and the export into a temp vault. It runs before Tauri is
//!    touched: no window or webview exists.
//! 3. **Checks.** `m6::inspect` unchanged: what `e2e.replay` checks on the CLI, here on what the
//!    app left behind. Plus `app_replay`, the process's own report.
//!
//! M7 has no row in PLAN.md 4.2, so this runner emits only checks; `accept` fails when any check
//! does.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail, ensure};
use earshot_llm_replay::mock::MockServer;

use super::{Check, Ctx, RunOutput, m6};
use crate::build;
use crate::proc::run_with_timeout;

/// Deadline for the replayed meeting.
const RUN_TIMEOUT: Duration = Duration::from_secs(2 * 60 * 60);

/// How much of a failed build's text the report keeps.
const MAX_DETAIL_CHARS: usize = 4000;

/// The x64 Windows target.
pub const X64_WINDOWS: &str = "x86_64-pc-windows-msvc";

/// The target an architecture's check builds: `None` for the host itself.
#[must_use]
pub fn build_target<'a>(want: &'a str, host: &str) -> Option<&'a str> {
    (want != host).then_some(want)
}

/// Whether the app can be cross-built for `target` on this host, from what doctor found.
#[must_use]
pub fn cross_toolchain_present(target: &str, report: &crate::doctor::DoctorReport) -> bool {
    let ok = |name: &str| {
        report
            .items
            .iter()
            .any(|i| i.name == name && i.status == crate::doctor::Status::Ok)
    };
    if target == build::ARM64_WINDOWS {
        ok("MSVC ARM64 tools") && ok("Ninja") && ok("LLVM/libclang")
    } else {
        ok("MSVC x64 tools")
    }
}

/// The verdict on the non-host architecture.
#[must_use]
pub fn may_skip_cross(target: &str) -> bool {
    // The plan requires x64 and asks for ARM64 "if the toolchain is present".
    target == build::ARM64_WINDOWS
}

/// The kinds the app printed for `m6::inspect`: its lines carry the labels the UI shows, and only
/// the exact CPU label counts as the kind `Cpu`; any other stays as printed, and fails the check.
///
/// # Errors
/// A line missing.
pub fn app_backends(stdout: &str) -> Result<[String; 3]> {
    let kinds = m6::printed_backends(stdout)?;
    Ok(kinds.map(|k| if k == "CPU" { "Cpu".to_owned() } else { k }))
}

/// The arguments of the app's hidden mode.
#[must_use]
pub fn app_args(fixture: &Path, url: &str, data: &Path, vault: &Path) -> Vec<std::ffi::OsString> {
    let mut args: Vec<std::ffi::OsString> = Vec::new();
    let mut push = |flag: &str, value: &std::ffi::OsStr| {
        args.push(flag.into());
        args.push(value.to_os_string());
    };
    push("--replay", fixture.as_os_str());
    push("--llm-url", url.as_ref());
    push("--data", data.as_os_str());
    push("--vault", vault.as_os_str());
    push("--model", m6::MODEL.as_ref());
    args.push("--exit-after-export".into());
    args
}

/// The end of a build's failure text: the report is committed, and the whole cargo log is
/// hundreds of kB. The end names what failed.
#[must_use]
pub fn tail(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    if chars.len() <= MAX_DETAIL_CHARS {
        return text.to_owned();
    }
    let kept: String = chars[chars.len() - MAX_DETAIL_CHARS..].iter().collect();
    format!("... {kept}")
}

/// Builds the app for one architecture.
fn build_check(
    root: &Path,
    name: &str,
    target: Option<&str>,
    host_exe: &mut Option<PathBuf>,
) -> Check {
    match build::app(root, target, &[], None) {
        Ok(exe) => {
            let detail = format!("{}", exe.display());
            if target.is_none() {
                *host_exe = Some(exe);
            }
            Check::new(name, true, detail)
        }
        Err(e) => Check::new(name, false, tail(&format!("{e:#}"))),
    }
}

pub(super) fn run(ctx: &Ctx) -> Result<RunOutput> {
    if let Some(b) = ctx.backend
        && b != "cpu"
    {
        bail!(
            "accept m7 runs the default app build, which transcribes on the CPU; --backend {b} is not supported"
        );
    }
    let mut checks = Vec::new();
    let report = crate::doctor::collect();
    let host = report.host.clone();
    let mut exe = None;
    for (name, want) in [
        ("x64_build", X64_WINDOWS),
        ("arm64_build", build::ARM64_WINDOWS),
    ] {
        let target = build_target(want, &host);
        if let Some(t) = target
            && !cross_toolchain_present(t, &report)
        {
            let skip = may_skip_cross(t);
            checks.push(Check::new(
                name,
                skip,
                if skip {
                    format!("skipped: no {t} toolchain on this host")
                } else {
                    format!("no {t} toolchain on this host, and the plan requires this build")
                },
            ));
            continue;
        }
        checks.push(build_check(ctx.root, name, target, &mut exe));
    }
    let exe = exe.context("the host's app build failed, so nothing can be replayed")?;
    checks.extend(replay(ctx.root, &exe, &|_| {})?);
    Ok(RunOutput {
        measurements: Vec::new(),
        checks,
    })
}

/// Replays the meeting through `exe`'s hidden mode and judges what it left behind. `prepare` may
/// change where the process runs (`bundle-check` runs an installed exe away from the repo).
///
/// # Errors
/// The meeting cannot be rendered, the mock cannot start or the replay misses its deadline.
pub fn replay(
    root: &Path,
    exe: &Path,
    prepare: &dyn Fn(&mut std::process::Command),
) -> Result<Vec<Check>> {
    let mut checks = Vec::new();
    let file = m6::meeting_file(root)?;
    let sim = m6::simulate_meeting(root, &file)?;
    let work = std::env::temp_dir().join(format!("earshot-accept-m7-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&work);
    let (data, vault, fixture) = (work.join("data"), work.join("vault"), work.join("fixture"));
    for dir in [&vault, &fixture] {
        std::fs::create_dir_all(dir).with_context(|| format!("cannot create {}", dir.display()))?;
    }
    m6::write_mono(&fixture.join("mic.wav"), &sim.mic)?;
    m6::write_mono(&fixture.join("loopback.wav"), &sim.reference)?;

    let server = MockServer::start_with(m6::respond).context("cannot start the LLM mock")?;
    let endpoint = server.endpoint();
    let host_name = endpoint.host_str().unwrap_or_default().to_owned();
    let port = endpoint.port().unwrap_or_default();
    ensure!(
        host_name == "127.0.0.1",
        "the mock must listen on loopback, not {host_name}"
    );

    // The executable itself, not cargo: the deadline kills the direct child.
    let mut cmd = std::process::Command::new(exe);
    cmd.args(app_args(&fixture, endpoint.as_str(), &data, &vault))
        .env("EARSHOT_MODELS", crate::models::cache_dir()?)
        .current_dir(root);
    prepare(&mut cmd);
    eprintln!(
        "accept m7: replaying a {:.0} s two-track meeting through {}",
        file.meeting.duration_s,
        exe.display()
    );
    let out = run_with_timeout(&mut cmd, RUN_TIMEOUT)
        .context("the app's replay did not finish within the deadline")?;
    out.require_complete("the app's replay")?;
    let ran = out.success;
    checks.push(Check::new(
        "app_replay",
        ran,
        if ran {
            "the app exited 0 after the export".to_owned()
        } else {
            tail(&format!(
                "work directory {}: {}\n{}",
                work.display(),
                out.stdout,
                out.stderr
            ))
        },
    ));
    let requests = server.requests();
    drop(server);
    if ran {
        checks.extend(m6::inspect(
            root,
            &sim,
            &data,
            &vault,
            &out.stdout,
            app_backends(&out.stdout),
            &requests,
            &host_name,
            port,
        )?);
    }
    if checks.iter().all(|c| c.pass) {
        let _ = std::fs::remove_dir_all(&work);
    } else {
        eprintln!("accept m7: work directory kept at {}", work.display());
    }
    Ok(checks)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn m7_builds_the_other_architecture_as_a_cross_target() {
        assert_eq!(build_target(X64_WINDOWS, X64_WINDOWS), None);
        assert_eq!(
            build_target(X64_WINDOWS, build::ARM64_WINDOWS),
            Some(X64_WINDOWS)
        );
        assert_eq!(
            build_target(build::ARM64_WINDOWS, X64_WINDOWS),
            Some(build::ARM64_WINDOWS)
        );
    }

    #[test]
    fn m7_a_long_failure_keeps_its_end() {
        assert_eq!(tail("short"), "short");
        let long = format!("{}the end", "x".repeat(MAX_DETAIL_CHARS * 2));
        let t = tail(&long);
        assert!(t.starts_with("... ") && t.ends_with("the end"));
        assert_eq!(t.chars().count(), MAX_DETAIL_CHARS + 4);
    }

    #[test]
    fn m7_only_the_arm64_build_may_be_skipped() {
        assert!(may_skip_cross(build::ARM64_WINDOWS));
        assert!(!may_skip_cross(X64_WINDOWS));
    }

    #[test]
    fn m7_only_the_exact_cpu_label_counts_as_the_cpu() {
        let lines = |a: &str, b: &str, c: &str| {
            format!(
                "live asr mic: {a}\nlive asr reference: {b}\npost: final pass ran on {c}\nnetwork: x\n"
            )
        };
        assert_eq!(
            app_backends(&lines("CPU", "CPU", "CPU")).unwrap(),
            ["Cpu", "Cpu", "Cpu"]
        );
        // Another backend stays as printed, so `m6::inspect`'s check fails on it.
        assert_eq!(
            app_backends(&lines("CPU", "GPU (CUDA)", "CPU")).unwrap()[1],
            "GPU"
        );
        assert_eq!(app_backends(&lines("cpu", "CPU", "CPU")).unwrap()[0], "cpu");
        assert!(app_backends("live asr mic: CPU\n").is_err());
    }

    #[test]
    fn m7_app_args_name_every_required_flag() {
        let a = app_args(
            Path::new("f"),
            "http://127.0.0.1:1/v1",
            Path::new("d"),
            Path::new("v"),
        );
        let text: Vec<String> = a.iter().map(|s| s.to_string_lossy().into_owned()).collect();
        assert_eq!(
            text,
            [
                "--replay",
                "f",
                "--llm-url",
                "http://127.0.0.1:1/v1",
                "--data",
                "d",
                "--vault",
                "v",
                "--model",
                m6::MODEL,
                "--exit-after-export",
            ]
        );
    }
}
