//! `cargo xtask ci` and `cargo xtask purity`.

use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result, bail};

/// xtask checks run by `ci` first, before the cargo [`STEPS`]: the desktop app's crate
/// needs the built front end (`apps/desktop/dist`) to compile (T7.1).
pub const PRE_CHECKS: &[(&str, Check)] = &[("ui", crate::ui::run)];

/// Cargo steps run by `ci`, in order, after the [`PRE_CHECKS`] and before the [`CHECKS`].
pub const STEPS: &[(&str, &[&str])] = &[
    ("fmt", &["fmt", "--all", "--", "--check"]),
    (
        "clippy",
        &[
            "clippy",
            "--workspace",
            "--all-targets",
            "--",
            "-D",
            "warnings",
        ],
    ),
    ("test", &["test", "--workspace"]),
];

fn cargo() -> String {
    std::env::var("CARGO").unwrap_or_else(|_| "cargo".into())
}

/// True for variables that `cargo run` sets for the xtask package itself. They must not leak
/// into child cargo builds: build scripts that watch them (ring watches `CARGO_PKG_NAME` and
/// `CARGO_MANIFEST_DIR`) would rebuild, relinking the running `xtask.exe`, which Windows
/// refuses ("Access is denied").
pub fn is_package_env(name: &str) -> bool {
    name.starts_with("CARGO_PKG_")
        || name.starts_with("CARGO_MANIFEST_")
        || matches!(
            name,
            "CARGO_CRATE_NAME" | "CARGO_BIN_NAME" | "CARGO_PRIMARY_PACKAGE" | "OUT_DIR"
        )
}

/// A `cargo` command without the package variables of the running xtask (see
/// [`is_package_env`]). On an ARM64 Windows host it also carries the ARM64 native build
/// environment ([`arm64_host_env`]), so `ci`, `accept` and every other child cargo build
/// whisper.cpp and llama.cpp the way `cargo xtask build` does.
pub fn cargo_command() -> Command {
    let mut cmd = Command::new(cargo());
    for (name, _) in std::env::vars_os() {
        if name.to_str().is_some_and(is_package_env) {
            cmd.env_remove(&name);
        }
    }
    if cfg!(all(windows, target_arch = "aarch64")) {
        let llvm = crate::build::llvm_bin(
            std::env::var_os("LIBCLANG_PATH"),
            std::env::var_os("ProgramFiles"),
        );
        for (k, v) in arm64_host_env(llvm, std::env::var_os("PATH")) {
            cmd.env(k, v);
        }
    }
    cmd
}

/// The environment for building natively on an ARM64 Windows host: ggml refuses MSVC for
/// ARM64 CPU code ("MSVC on ARM requires GGML_NATIVE=ON, use clang", first seen on the
/// Surface's first `cargo xtask ci`, 2026-11-02), so whisper.cpp and llama.cpp build with
/// clang-cl through Ninja ([`crate::build::target_env`]), and LLVM's `bin` (`llvm`, when
/// found) goes first on PATH for `ring`.
pub fn arm64_host_env(
    llvm: Option<std::path::PathBuf>,
    path: Option<std::ffi::OsString>,
) -> Vec<(std::ffi::OsString, std::ffi::OsString)> {
    let mut env: Vec<(std::ffi::OsString, std::ffi::OsString)> =
        crate::build::target_env(Some(crate::build::ARM64_WINDOWS))
            .into_iter()
            .map(|(k, v)| (k.into(), v.into()))
            .collect();
    if let Some(bin) = llvm {
        let mut dirs = vec![bin];
        dirs.extend(std::env::split_paths(&path.unwrap_or_default()));
        if let Ok(joined) = std::env::join_paths(dirs) {
            env.push(("PATH".into(), joined));
        }
    }
    env
}

/// The command for one CI step, run in `root`.
///
/// The workspace `clippy` step sets `CLIPPY_CONF_DIR` to `root`, whose `clippy.toml` is
/// the neutral workspace-wide config. Without it, clippy would apply
/// `crates/earshot-core/clippy.toml` to core's test targets too, but PLAN.md 3.2 lets
/// core tests read fixtures. Core's lib target is held to its own config by `purity`.
pub fn step_command(root: &Path, name: &str, args: &[&str]) -> Command {
    let mut cmd = cargo_command();
    cmd.args(args).current_dir(root);
    if name == "clippy" {
        cmd.env("CLIPPY_CONF_DIR", root);
    }
    cmd
}

/// An xtask check over the repo at the given root.
pub type Check = fn(&Path) -> Result<()>;

/// xtask checks run by `ci` after the cargo [`STEPS`], in order.
pub const CHECKS: &[(&str, Check)] = &[
    ("purity", purity),
    ("net", crate::net_guard::run),
    ("llama", crate::llama_guard::run),
    ("schema", crate::schema::check),
    ("public-guard", crate::public_guard::run),
];

/// Every step name in the order [`run`] runs them.
pub fn step_order() -> Vec<&'static str> {
    PRE_CHECKS
        .iter()
        .map(|c| c.0)
        .chain(STEPS.iter().map(|s| s.0))
        .chain(CHECKS.iter().map(|c| c.0))
        .collect()
}

/// Runs every CI step in order ([`step_order`]), stopping at the first failure.
pub fn run(root: &Path) -> Result<()> {
    run_checks(root, PRE_CHECKS)?;
    for (name, args) in STEPS {
        println!("ci: {name}: cargo {}", args.join(" "));
        let status = step_command(root, name, args)
            .status()
            .with_context(|| format!("ci: cannot start cargo for step {name}"))?;
        if !status.success() {
            bail!(
                "ci: step `{name}` failed (cargo {}); stopping",
                args.join(" ")
            );
        }
    }
    run_checks(root, CHECKS)?;
    println!("ci: all steps passed");
    Ok(())
}

fn run_checks(root: &Path, checks: &[(&str, Check)]) -> Result<()> {
    for (name, check) in checks {
        println!("ci: {name}");
        check(root).with_context(|| format!("ci: step `{name}` failed; stopping"))?;
    }
    Ok(())
}

/// Core purity guard (PLAN.md 3.2); see [`crate::purity`].
pub fn purity(root: &Path) -> Result<()> {
    crate::purity::run(root)
}

#[cfg(test)]
mod tests {
    use super::{CHECKS, PRE_CHECKS, STEPS, arm64_host_env, is_package_env, step_order};

    #[test]
    fn ci_arm64_host_builds_ggml_with_clang_through_ninja() {
        let env = arm64_host_env(
            Some(r"C:\LLVM\bin".into()),
            Some(r"C:\Windows;C:\tools".into()),
        );
        let get = |k: &str| {
            env.iter()
                .find(|(n, _)| n == k)
                .map(|(_, v)| v.to_string_lossy().into_owned())
        };
        assert_eq!(get("CMAKE_GENERATOR").as_deref(), Some("Ninja"));
        assert_eq!(get("CMAKE_C_COMPILER").as_deref(), Some("clang-cl"));
        assert_eq!(get("CMAKE_CXX_COMPILER").as_deref(), Some("clang-cl"));
        assert_eq!(
            get("GGML_CPU_ARM_ARCH").as_deref(),
            Some(crate::build::ARM64_CPU_ARCH)
        );
        assert_eq!(
            get("CXXFLAGS_aarch64_pc_windows_msvc").as_deref(),
            Some("/EHsc")
        );
        let path = get("PATH").expect("LLVM goes on PATH");
        assert!(path.starts_with(r"C:\LLVM\bin;"), "{path}");
        assert!(path.ends_with(r"C:\Windows;C:\tools"), "{path}");
        // Without LLVM the PATH is left alone (doctor reports the missing install).
        assert!(
            arm64_host_env(None, Some("x".into()))
                .iter()
                .all(|(n, _)| n != "PATH")
        );
    }

    #[test]
    fn xtask_checks_run_after_the_cargo_steps_in_order() {
        let names: Vec<&str> = CHECKS.iter().map(|c| c.0).collect();
        assert_eq!(names, ["purity", "net", "llama", "schema", "public-guard"]);
    }

    /// The front end is built before anything compiles the desktop app's crate.
    #[test]
    fn ui_runs_first_then_cargo_steps_then_xtask_checks() {
        let names: Vec<&str> = PRE_CHECKS.iter().map(|c| c.0).collect();
        assert_eq!(names, ["ui"]);
        assert_eq!(
            step_order(),
            [
                "ui",
                "fmt",
                "clippy",
                "test",
                "purity",
                "net",
                "llama",
                "schema",
                "public-guard"
            ]
        );
    }

    #[test]
    fn package_env_of_the_running_xtask_is_scrubbed_but_cargo_config_is_kept() {
        for v in [
            "CARGO_PKG_NAME",
            "CARGO_PKG_VERSION_MAJOR",
            "CARGO_MANIFEST_DIR",
            "CARGO_MANIFEST_PATH",
            "CARGO_CRATE_NAME",
            "OUT_DIR",
        ] {
            assert!(is_package_env(v), "{v}");
        }
        for v in [
            "CARGO",
            "CARGO_HOME",
            "CARGO_TARGET_DIR",
            "CARGO_BUILD_JOBS",
            "PATH",
        ] {
            assert!(!is_package_env(v), "{v}");
        }
    }

    #[test]
    fn steps_are_fmt_clippy_test_in_order() {
        let names: Vec<&str> = STEPS.iter().map(|s| s.0).collect();
        assert_eq!(names, ["fmt", "clippy", "test"]);
        assert!(STEPS[0].1.ends_with(&["--check"]));
        assert!(STEPS[1].1.ends_with(&["-D", "warnings"]));
        assert!(STEPS[1].1.contains(&"--all-targets"));
        assert!(STEPS[2].1.contains(&"--workspace"));
    }

    /// `ci` runs `cargo test --workspace`, which builds xtask with the workspace's unified
    /// features into `target/`. The `cargo xtask` alias must build the running xtask
    /// into a different target dir, or a feature change would relink the running
    /// `xtask.exe`, which Windows refuses ("Access is denied").
    #[test]
    fn xtask_alias_runs_from_its_own_target_dir() {
        let text = std::fs::read_to_string(crate::repo_root().join(".cargo").join("config.toml"))
            .expect("read .cargo/config.toml");
        let config: toml::Table = toml::from_str(&text).expect("parse .cargo/config.toml");
        let alias = config["alias"]["xtask"]
            .as_str()
            .expect("xtask alias string");
        let args: Vec<&str> = alias.split_whitespace().collect();
        let dir = args
            .windows(2)
            .find(|w| w[0] == "--target-dir")
            .map(|w| w[1])
            .expect("xtask alias sets --target-dir");
        let dir = std::path::Path::new(dir);
        assert_ne!(dir, std::path::Path::new("target"), "{alias}");
        assert!(
            dir.starts_with("target"),
            "keep it under the ignored target/: {alias}"
        );
        assert!(
            args.contains(&"--package") && args.contains(&"xtask"),
            "{alias}"
        );
        // The test step itself must not override the target dir back to the alias's.
        assert!(!STEPS[2].1.contains(&"--target-dir"));
    }

    #[test]
    fn workspace_clippy_step_uses_the_neutral_root_config() {
        let root = crate::repo_root();
        let cmd = super::step_command(&root, "clippy", STEPS[1].1);
        let conf = cmd
            .get_envs()
            .find(|(k, _)| *k == "CLIPPY_CONF_DIR")
            .and_then(|(_, v)| v)
            .expect("clippy step sets CLIPPY_CONF_DIR");
        assert_eq!(std::path::Path::new(conf), root);
        assert!(root.join("clippy.toml").is_file());
        for name in ["fmt", "test"] {
            let cmd = super::step_command(&root, name, &[]);
            assert!(cmd.get_envs().all(|(k, _)| k != "CLIPPY_CONF_DIR"));
        }
    }
}
