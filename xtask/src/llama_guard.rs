//! `ci` step `llama`: only `earshot-llm-local` depends on `llama-cpp-sys-2`.
//!
//! earshot-llm-local guards the rule that keeps llama.cpp's ggml apart from whisper.cpp's
//! (`ffi_calls_only_llama_functions`: Rust names no `ggml_*`/`gguf_*` item, which would bind
//! to whisper's static copy). That guard only scans its own crate, so any other crate with a
//! direct dependency on the sys crate could name ggml items unguarded. Rust can name items
//! only of direct dependencies, so the rule is on direct dependents: this runs
//! `cargo tree --workspace -e normal,build,dev --all-features --target all -i llama-cpp-sys-2 --prefix
//! depth` and fails unless every direct dependent (depth 1 of the inverted tree) is
//! [`OWNER`]. Crates depending on `earshot-llm-local` itself are fine.

use std::path::Path;

use anyhow::{Result, bail};

use crate::proc::{CmdOutput, run_with_timeout};
use crate::purity::CARGO_TIMEOUT;

/// The guarded sys crate.
pub const SYS_CRATE: &str = "llama-cpp-sys-2";

/// The one crate allowed to depend on [`SYS_CRATE`] directly.
pub const OWNER: &str = "earshot-llm-local";

/// Arguments of the `cargo tree` run the check parses. Every dependency kind counts: a dev-
/// or build-dependency could name ggml items in tests or a build script too. (`-e all`
/// would add feature nodes to the tree.)
pub fn tree_args() -> Vec<&'static str> {
    vec![
        "tree",
        "--workspace",
        "-e",
        "normal,build,dev",
        "--all-features",
        "--target",
        "all",
        "-i",
        SYS_CRATE,
        "--prefix",
        "depth",
    ]
}

/// Direct dependents of [`SYS_CRATE`] other than [`OWNER`], sorted and deduplicated, from
/// inverted `cargo tree --prefix depth` output (`0llama-cpp-sys-2 v...`, then its dependents
/// at depth 1, theirs at 2, ...). Fails closed: a line without a depth prefix, a root that
/// is not [`SYS_CRATE`], more than one root, or no dependent at all (so [`OWNER`] must be
/// among them) is an error.
pub fn foreign_dependents(tree: &str) -> Result<Vec<String>> {
    let mut roots = 0;
    let mut dependents = Vec::new();
    for line in tree.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let digits = line.bytes().take_while(u8::is_ascii_digit).count();
        if digits == 0 {
            bail!("llama: unexpected `cargo tree` line (no depth prefix): {line}");
        }
        let depth: usize = line[..digits].parse()?;
        let Some(name) = line[digits..].split_whitespace().next() else {
            bail!("llama: unexpected `cargo tree` line (no crate name): {line}");
        };
        match depth {
            0 => {
                if name != SYS_CRATE {
                    bail!("llama: inverted tree rooted at {name}, not {SYS_CRATE}: {line}");
                }
                roots += 1;
            }
            1 => {
                if roots == 0 {
                    bail!("llama: dependent before the tree root: {line}");
                }
                dependents.push(name.to_string());
            }
            _ => {}
        }
    }
    if roots != 1 {
        bail!("llama: expected one {SYS_CRATE} tree root, found {roots}");
    }
    if !dependents.iter().any(|d| d == OWNER) {
        bail!("llama: {OWNER} is not among the dependents of {SYS_CRATE}: {dependents:?}");
    }
    dependents.retain(|d| d != OWNER);
    dependents.sort();
    dependents.dedup();
    Ok(dependents)
}

/// The foreign dependents in a captured `cargo tree` run. Fails closed on output that was
/// not captured completely or a failed run (for example when no workspace crate depends on
/// [`SYS_CRATE`] any more: `-i` then matches no package).
pub fn evaluate(args: &[&str], out: &CmdOutput) -> Result<Vec<String>> {
    out.require_complete(&format!("llama: `cargo {}`", args.join(" ")))?;
    if !out.success {
        bail!(
            "llama: `cargo {}` failed: {}",
            args.join(" "),
            out.describe()
        );
    }
    foreign_dependents(&out.stdout)
}

/// Runs the check on the workspace at `root`.
pub fn run(root: &Path) -> Result<()> {
    let args = tree_args();
    let mut cmd = crate::ci::cargo_command();
    cmd.args(&args).current_dir(root);
    let Some(out) = run_with_timeout(&mut cmd, CARGO_TIMEOUT) else {
        bail!(
            "llama: `cargo {}` could not start or exceeded {} s",
            args.join(" "),
            CARGO_TIMEOUT.as_secs()
        );
    };
    let foreign = evaluate(&args, &out)?;
    if !foreign.is_empty() {
        bail!(
            "llama: {} depend directly on {SYS_CRATE}; only {OWNER} may (its ggml guard is \
             the only one; docs/local-llm.md)",
            foreign.join(", ")
        );
    }
    println!("llama: only {OWNER} depends on {SYS_CRATE}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // Recorded shape of the inverted tree: the sys crate, its one dependent, and a crate
    // depending on that dependent.
    const CLEAN: &str = "\
0llama-cpp-sys-2 v0.1.160
1earshot-llm-local v0.1.0 (C:\\repo\\crates\\earshot-llm-local)
2earshot-runtime v0.1.0 (C:\\repo\\crates\\earshot-runtime)
3earshot-cli v0.1.0 (C:\\repo\\crates\\earshot-cli)
";

    #[test]
    fn llama_sys_beneath_the_owner_only_is_allowed() {
        assert_eq!(foreign_dependents(CLEAN).unwrap(), Vec::<String>::new());
    }

    #[test]
    fn llama_sys_used_directly_by_another_crate_is_a_violation() {
        let tree = format!(
            "{CLEAN}1earshot-runtime v0.1.0 (C:\\repo)\n2earshot-cli v0.1.0 (C:\\repo)\n1earshot-cli v0.1.0 (C:\\repo) (*)\n"
        );
        assert_eq!(
            foreign_dependents(&tree).unwrap(),
            ["earshot-cli", "earshot-runtime"]
        );
    }

    #[test]
    fn llama_recorded_canary_with_a_second_dependent_is_a_violation() {
        // Recorded 2026-10-24 from a two-crate canary workspace (`a` and earshot-llm-local
        // both depending on the sys crate) with this check's `tree_args` and `--offline`.
        let tree = "\
0llama-cpp-sys-2 v0.1.160
1a v0.1.0 (C:\\Temp\\canary\\a)
1earshot-llm-local v0.1.0 (C:\\Temp\\canary\\earshot-llm-local)
";
        assert_eq!(foreign_dependents(tree).unwrap(), ["a"]);
    }

    #[test]
    fn llama_tree_without_the_owner_or_root_fails_closed() {
        assert!(foreign_dependents("").is_err());
        assert!(foreign_dependents("0llama-cpp-sys-2 v0.1.160\n").is_err());
        assert!(
            foreign_dependents("0llama-cpp-sys-2 v0.1.160\n1earshot-cli v0.1.0 (C:\\repo)\n")
                .is_err()
        );
        assert!(foreign_dependents("1earshot-llm-local v0.1.0 (C:\\repo)\n").is_err());
        assert!(
            foreign_dependents("0whisper-rs-sys v0.15.0\n1earshot-llm-local v0.1.0\n").is_err()
        );
        assert!(foreign_dependents(&format!("{CLEAN}{CLEAN}")).is_err());
        assert!(foreign_dependents("llama-cpp-sys-2 v0.1.160\n└── earshot-llm-local\n").is_err());
    }

    fn captured(stdout: &str, success: bool, stdout_complete: bool) -> CmdOutput {
        CmdOutput {
            success,
            code: Some(if success { 0 } else { 101 }),
            stdout: stdout.into(),
            stderr: String::new(),
            stdout_complete,
            stderr_complete: true,
        }
    }

    #[test]
    fn llama_evaluate_passes_only_complete_successful_output() {
        let args = tree_args();
        assert!(
            evaluate(&args, &captured(CLEAN, true, true))
                .unwrap()
                .is_empty()
        );
        let err = format!(
            "{:#}",
            evaluate(&args, &captured(CLEAN, true, false)).unwrap_err()
        );
        assert!(err.contains("not fully captured"), "{err}");
        assert!(evaluate(&args, &captured("", true, true)).is_err());
        let err = format!(
            "{:#}",
            evaluate(&args, &captured(CLEAN, false, true)).unwrap_err()
        );
        assert!(err.contains("exit code 101"), "{err}");
    }

    #[test]
    fn llama_tree_args_invert_on_the_sys_crate_with_every_edge_kind() {
        let joined = tree_args().join(" ");
        for flag in [
            "--workspace",
            "-e normal,build,dev",
            "--all-features",
            "--target all",
            "-i llama-cpp-sys-2",
            "--prefix depth",
        ] {
            assert!(joined.contains(flag), "{flag}");
        }
    }

    /// The real workspace passes (needs only cargo metadata, no build or network beyond
    /// the lock file already resolved).
    #[test]
    fn llama_workspace_has_one_direct_dependent() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap();
        run(root).unwrap();
    }
}
