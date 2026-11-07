//! `cargo xtask purity`: the core purity guard (PLAN.md 3.2).
//!
//! Checks, all scoped to the `earshot-core` library target and its normal (and, for
//! native builders, build) dependencies:
//! 1. `cargo build -p earshot-core --lib --target wasm32-unknown-unknown`.
//! 2. [`BANNED_CRATES`] / [`BANNED_PREFIXES`] checked against
//!    `cargo tree -p earshot-core -e normal --target all`.
//! 3. [`NATIVE_BUILDERS`] checked against
//!    `cargo tree -p earshot-core -e normal,build --target all`.
//! 4. `cargo clippy -p earshot-core --lib -- -D warnings -F clippy::disallowed_methods
//!    -F clippy::disallowed_types -F unsafe_code` with `CLIPPY_CONF_DIR` set to the
//!    core crate, which picks up the disallowed methods and types in
//!    `crates/earshot-core/clippy.toml`.
//!
//! Plus: the core crate has no build script, its `lib.rs` declares
//! `#![forbid(unsafe_code)]`, and no source file under `src/` uses the `clippy` cfg
//! (`cfg(clippy)`, `cfg(not(clippy))`, `cfg_attr(clippy, ...)`, `cfg!(clippy)`), which
//! would hide lib code from check 4. Each check is a function over a [`Target`] so the
//! canary tests run the same code against throwaway crates. Every child cargo goes
//! through [`crate::ci::cargo_command`].

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use anyhow::{Result, bail};

use crate::proc::{CmdOutput, run_with_timeout};

/// Package the purity contract applies to.
pub const CORE_PACKAGE: &str = "earshot-core";

/// Crates that must never appear among earshot-core's normal dependencies (exact names).
/// This is the single ban list; extend it here.
pub const BANNED_CRATES: &[&str] = &[
    // OS / platform bindings
    "windows",
    "windows-sys",
    "ndk",
    "ndk-sys",
    "jni",
    "jni-sys",
    "libc",
    "nix",
    "winapi",
    // audio devices, inference runtimes
    "cpal",
    "ort",
    "ort-sys",
    "whisper-rs",
    "whisper-rs-sys",
    // network
    "reqwest",
    "hyper",
    "ureq",
    "socket2",
    "mio",
    // async runtimes and thread pools
    "tokio",
    "async-std",
    "rayon",
    "rayon-core",
    // storage
    "rusqlite",
    "libsqlite3-sys",
    // OS randomness and wasm host bindings
    "getrandom",
    "wasm-bindgen",
    "js-sys",
    "web-sys",
    // native code builders
    "cc",
    "cmake",
    "bindgen",
];

/// Native code builders: banned anywhere in earshot-core's normal and build dependency
/// graph (`cargo tree -e normal,build`), so no dependency's build script compiles or
/// links native code for core. Dev-dependencies are outside the contract.
pub const NATIVE_BUILDERS: &[&str] = &[
    "cc",
    "cmake",
    "bindgen",
    "pkg-config",
    "vcpkg",
    "nasm-rs",
    "autotools",
];

/// Crate-name prefixes that are banned as whole families (for example `windows-core`,
/// `windows_x86_64_msvc`, `tokio-util`).
pub const BANNED_PREFIXES: &[&str] = &["windows-", "windows_", "tokio-", "hyper-", "ndk-", "jni-"];

/// Generous deadline for a cargo build/clippy/tree invocation.
pub const CARGO_TIMEOUT: Duration = Duration::from_secs(30 * 60);

/// Where and on what a purity check runs.
#[derive(Debug, Clone)]
pub struct Target {
    /// Workspace (or standalone crate) directory cargo runs in.
    pub root: PathBuf,
    /// Package to check (`-p`).
    pub package: String,
    /// Directory of the package's manifest (for the build script check).
    pub manifest_dir: PathBuf,
    /// Cargo target dir override; `None` uses the workspace default.
    pub target_dir: Option<PathBuf>,
}

impl Target {
    /// The real target: `earshot-core` in the repo at `root`.
    pub fn core(root: &Path) -> Self {
        Target {
            root: root.to_path_buf(),
            package: CORE_PACKAGE.into(),
            manifest_dir: root.join("crates").join(CORE_PACKAGE),
            target_dir: None,
        }
    }
}

/// The child cargo command for a purity check on `t`: built by
/// [`crate::ci::cargo_command`] (so the running xtask's package variables never reach the
/// child), run in `t.root`, with `t.target_dir` as `CARGO_TARGET_DIR` when set.
pub fn cargo_command(t: &Target, args: &[&str]) -> Command {
    let mut cmd = crate::ci::cargo_command();
    cmd.args(args).current_dir(&t.root);
    if let Some(dir) = &t.target_dir {
        cmd.env("CARGO_TARGET_DIR", dir);
    }
    cmd
}

fn run_cargo(t: &Target, args: &[&str]) -> Result<CmdOutput> {
    run_captured(cargo_command(t, args), args)
}

/// Runs a purity child cargo. Every check decides from what the child printed, so the
/// output must be complete: a capture cut short (a starved reader, a pipe held open) is
/// an error from [`require_success`], never "nothing found".
fn run_captured(mut cmd: Command, args: &[&str]) -> Result<CmdOutput> {
    match run_with_timeout(&mut cmd, CARGO_TIMEOUT) {
        Some(out) => Ok(out),
        None => bail!(
            "purity: `cargo {}` could not start or exceeded {} s",
            args.join(" "),
            CARGO_TIMEOUT.as_secs()
        ),
    }
}

/// `Ok` only for a run whose output was captured completely and that exited 0.
fn require_success(what: &str, args: &[&str], out: &CmdOutput) -> Result<()> {
    out.require_complete(&format!("purity: {what} (cargo {})", args.join(" ")))?;
    if out.success {
        return Ok(());
    }
    bail!(
        "purity: {what} failed (cargo {}): {}",
        args.join(" "),
        out.describe()
    )
}

/// `cargo tree -p <package> --prefix none` output must open with the root package line
/// (`<package> v<version> ...`). Anything else (empty, cut before the root, another root)
/// is an error, so an empty tree can never read as "no banned crates".
pub fn require_tree_root(package: &str, tree: &str) -> Result<()> {
    let first = tree.lines().find(|l| !l.trim().is_empty());
    let rooted = first.is_some_and(|l| {
        let mut words = l.split_whitespace();
        words.next() == Some(package) && words.next().is_some_and(|v| v.starts_with('v'))
    });
    if !rooted {
        bail!(
            "purity: `cargo tree` output does not start with the root package line \
             `{package} v...`; refusing to read it as an empty dependency graph\n\
             --- stdout ---\n{tree}"
        );
    }
    Ok(())
}

/// Check 1: the library compiles for `wasm32-unknown-unknown`.
pub fn check_wasm_build(t: &Target) -> Result<()> {
    let args = [
        "build",
        "-p",
        &t.package,
        "--lib",
        "--target",
        "wasm32-unknown-unknown",
    ];
    let out = run_cargo(t, &args)?;
    require_success("wasm32 build", &args, &out)
}

/// Whether `name` is on the ban list (exact name or banned family prefix).
pub fn is_banned(name: &str) -> bool {
    BANNED_CRATES.contains(&name) || BANNED_PREFIXES.iter().any(|p| name.starts_with(p))
}

/// Returns the banned crate names (sorted, deduplicated) found in `cargo tree --prefix
/// none` output, where each line starts with `<name> v<version>`.
pub fn banned_in_tree(tree: &str) -> Vec<String> {
    let mut found: Vec<String> = tree
        .lines()
        .filter_map(|l| l.split_whitespace().next())
        .filter(|name| is_banned(name))
        .map(str::to_string)
        .collect();
    found.sort();
    found.dedup();
    found
}

/// Arguments of the check 2 `cargo tree` run.
pub fn ban_list_args(package: &str) -> Vec<&str> {
    vec![
        "tree", "-p", package, "-e", "normal", "--target", "all", "--prefix", "none",
    ]
}

/// Check 2: no banned crate among the package's normal dependencies, for any target.
pub fn check_ban_list(t: &Target) -> Result<()> {
    let args = ban_list_args(&t.package);
    let out = run_cargo(t, &args)?;
    evaluate_ban_list(&t.package, &args, &out)
}

/// The verdict of check 2 on a captured `cargo tree` run. Fails closed: incomplete
/// output, a failed run or output without the root line is an error before any crate
/// name is looked at.
pub fn evaluate_ban_list(package: &str, args: &[&str], out: &CmdOutput) -> Result<()> {
    require_success("cargo tree", args, out)?;
    require_tree_root(package, &out.stdout)?;
    let banned = banned_in_tree(&out.stdout);
    if !banned.is_empty() {
        bail!(
            "purity: banned normal dependencies of {package}: {}\n{}",
            banned.join(", "),
            out.stdout
        );
    }
    Ok(())
}

/// Returns the native builder names (sorted, deduplicated) found in `cargo tree --prefix
/// none` output.
pub fn native_builders_in_tree(tree: &str) -> Vec<String> {
    let mut found: Vec<String> = tree
        .lines()
        .filter_map(|l| l.split_whitespace().next())
        .filter(|name| NATIVE_BUILDERS.contains(name))
        .map(str::to_string)
        .collect();
    found.sort();
    found.dedup();
    found
}

/// Check 2b: no native builder among the package's normal or build dependencies
/// (transitively), for any target. `-e build` alone would stop at the first normal edge,
/// so both edge kinds are followed.
pub fn check_build_tree(t: &Target) -> Result<()> {
    let args = build_tree_args(&t.package);
    let out = run_cargo(t, &args)?;
    evaluate_build_tree(&t.package, &args, &out)
}

/// Arguments of the check 2b `cargo tree` run.
pub fn build_tree_args(package: &str) -> Vec<&str> {
    vec![
        "tree",
        "-p",
        package,
        "-e",
        "normal,build",
        "--target",
        "all",
        "--prefix",
        "none",
    ]
}

/// The verdict of check 2b on a captured `cargo tree` run; fails closed like
/// [`evaluate_ban_list`].
pub fn evaluate_build_tree(package: &str, args: &[&str], out: &CmdOutput) -> Result<()> {
    require_success("cargo tree (build edges)", args, out)?;
    require_tree_root(package, &out.stdout)?;
    let found = native_builders_in_tree(&out.stdout);
    if !found.is_empty() {
        bail!(
            "purity: native build tools in the build graph of {package}: {}\n{}",
            found.join(", "),
            out.stdout
        );
    }
    Ok(())
}

/// Arguments of the purity clippy run. The disallowed lints and `unsafe_code` are
/// forbidden (`-F`), so an `#[allow]` or `#[expect]` in lib code is itself an error.
pub fn clippy_args(package: &str) -> Vec<&str> {
    vec![
        "clippy",
        "-p",
        package,
        "--lib",
        "--",
        "-D",
        "warnings",
        "-F",
        "clippy::disallowed_methods",
        "-F",
        "clippy::disallowed_types",
        "-F",
        "unsafe_code",
    ]
}

/// The command of check 3 (see [`check_clippy`]).
pub fn clippy_command(t: &Target) -> Command {
    let mut cmd = cargo_command(t, &clippy_args(&t.package));
    cmd.env("CLIPPY_CONF_DIR", &t.manifest_dir);
    cmd
}

/// Check 3: clippy on the library target with the crate's own `clippy.toml`
/// (`CLIPPY_CONF_DIR` is pinned to the package directory so an inherited value cannot
/// swap the config), `-D warnings` and the forbids from [`clippy_args`].
pub fn check_clippy(t: &Target) -> Result<()> {
    let args = clippy_args(&t.package);
    let out = run_captured(clippy_command(t), &args)?;
    require_success("clippy (disallowed methods/types)", &args, &out)
}

/// Whether a crate root's source carries `#![forbid(unsafe_code)]` as a line of its own.
pub fn has_forbid_unsafe(lib_rs: &str) -> bool {
    lib_rs
        .lines()
        .any(|l| l.trim() == "#![forbid(unsafe_code)]")
}

/// The library root declares `#![forbid(unsafe_code)]`, so no FFI call can compile.
pub fn check_forbid_unsafe(t: &Target) -> Result<()> {
    let lib_rs = t.manifest_dir.join("src").join("lib.rs");
    let src = std::fs::read_to_string(&lib_rs)
        .map_err(|e| anyhow::anyhow!("purity: cannot read {}: {e}", lib_rs.display()))?;
    if !has_forbid_unsafe(&src) {
        bail!(
            "purity: {} must declare `#![forbid(unsafe_code)]`",
            lib_rs.display()
        );
    }
    Ok(())
}

fn is_ident_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

/// Whether a cfg predicate (whitespace removed) names the `clippy` cfg as a whole word.
fn predicate_names_clippy(pred: &str) -> bool {
    pred.match_indices("clippy").any(|(i, _)| {
        let before = pred[..i].chars().next_back();
        let after = pred[i + "clippy".len()..].chars().next();
        !before.is_some_and(is_ident_char) && !after.is_some_and(is_ident_char)
    })
}

/// Every `cfg(..)`, `cfg!(..)` and `cfg_attr(..)` in `src` whose predicate names the
/// `clippy` cfg, returned with whitespace removed. Such a cfg makes code differ between
/// the purity clippy run and real builds (for example `#[cfg(not(clippy))]` hides a
/// `std::fs` call from clippy while the wasm32 build still compiles it), so core must
/// not use one. Matching ignores whitespace; comments and string literals are not
/// exempt (strict on purpose). In `cfg_attr` only the predicate is checked, so
/// `cfg_attr(test, allow(clippy::x))` is fine.
pub fn clippy_cfgs(src: &str) -> Vec<String> {
    let compact: String = src.chars().filter(|c| !c.is_whitespace()).collect();
    let mut found = Vec::new();
    for (start, _) in compact.match_indices("cfg") {
        if compact[..start]
            .chars()
            .next_back()
            .is_some_and(is_ident_char)
        {
            continue;
        }
        let rest = &compact[start + 3..];
        let (open, is_attr) = if rest.starts_with('(') {
            (0, false)
        } else if rest.starts_with("!(") {
            (1, false)
        } else if rest.starts_with("_attr(") {
            (5, true)
        } else {
            continue;
        };
        // Balanced parentheses after `open`; an unbalanced tail is taken whole.
        let body = &rest[open + 1..];
        let mut depth = 1usize;
        let mut end = body.len();
        let mut first_comma = None;
        for (i, c) in body.char_indices() {
            match c {
                '(' => depth += 1,
                ')' => {
                    depth -= 1;
                    if depth == 0 {
                        end = i;
                        break;
                    }
                }
                ',' if depth == 1 && first_comma.is_none() => first_comma = Some(i),
                _ => {}
            }
        }
        let content = &body[..end];
        let pred = if is_attr {
            &content[..first_comma.unwrap_or(content.len())]
        } else {
            content
        };
        if predicate_names_clippy(pred) {
            let close = (open + 1 + end + 1).min(rest.len());
            found.push(format!("cfg{}", &rest[..close]));
        }
    }
    found
}

fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    let entries = std::fs::read_dir(dir)
        .map_err(|e| anyhow::anyhow!("purity: cannot read {}: {e}", dir.display()))?;
    for entry in entries {
        let path = entry?.path();
        if path.is_dir() {
            rust_files(&path, out)?;
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
    Ok(())
}

/// No `.rs` file under the package's `src/` uses the `clippy` cfg (see [`clippy_cfgs`]).
pub fn check_no_clippy_cfg(t: &Target) -> Result<()> {
    let mut files = Vec::new();
    rust_files(&t.manifest_dir.join("src"), &mut files)?;
    files.sort();
    let mut hits = Vec::new();
    for f in files {
        let src = std::fs::read_to_string(&f)
            .map_err(|e| anyhow::anyhow!("purity: cannot read {}: {e}", f.display()))?;
        for cfg in clippy_cfgs(&src) {
            hits.push(format!("{}: {cfg}", f.display()));
        }
    }
    if !hits.is_empty() {
        bail!(
            "purity: the clippy cfg hides code from the purity clippy run; remove it:\n{}",
            hits.join("\n")
        );
    }
    Ok(())
}

/// Every `#[path = ...]` attribute (also inside `cfg_attr`) and `include!(...)` call in
/// `src`, returned with whitespace removed. Either one can pull Rust source from outside
/// `src/`, where the clippy cfg scan does not look; core uses neither, so all of them are
/// refused. `include_str!` and `include_bytes!` embed data, not code, and are allowed.
/// Comments and string literals are not exempt (strict on purpose).
pub fn source_escapes(src: &str) -> Vec<String> {
    let compact: String = src.chars().filter(|c| !c.is_whitespace()).collect();
    let mut found = Vec::new();
    for (i, _) in compact.match_indices("path=") {
        if matches!(compact[..i].chars().next_back(), Some('[' | ',')) {
            let end = compact[i..].find(']').map_or(compact.len(), |e| i + e + 1);
            found.push(compact[i..end].to_string());
        }
    }
    for (i, _) in compact.match_indices("include!(") {
        if !compact[..i].chars().next_back().is_some_and(is_ident_char) {
            let end = compact[i..].find(')').map_or(compact.len(), |e| i + e + 1);
            found.push(compact[i..end].to_string());
        }
    }
    found
}

/// No `.rs` file under the package's `src/` uses `#[path]` or `include!` (see
/// [`source_escapes`]), so every lib source file is under `src/` and covered by
/// [`check_no_clippy_cfg`].
pub fn check_no_source_escapes(t: &Target) -> Result<()> {
    let mut files = Vec::new();
    rust_files(&t.manifest_dir.join("src"), &mut files)?;
    files.sort();
    let mut hits = Vec::new();
    for f in files {
        let src = std::fs::read_to_string(&f)
            .map_err(|e| anyhow::anyhow!("purity: cannot read {}: {e}", f.display()))?;
        for found in source_escapes(&src) {
            hits.push(format!("{}: {found}", f.display()));
        }
    }
    if !hits.is_empty() {
        bail!(
            "purity: #[path] and include! can load source from outside src/; remove them:\n{}",
            hits.join("\n")
        );
    }
    Ok(())
}

/// The package has no build script (no native code compiled for core).
pub fn check_no_build_script(t: &Target) -> Result<()> {
    if t.manifest_dir.join("build.rs").exists() {
        bail!(
            "purity: {} has a build.rs; core must not have a build script",
            t.package
        );
    }
    Ok(())
}

/// Runs every purity check against `t`, stopping at the first failure.
pub fn check(t: &Target) -> Result<()> {
    println!("purity: no build script");
    check_no_build_script(t)?;
    println!("purity: #![forbid(unsafe_code)] in lib.rs");
    check_forbid_unsafe(t)?;
    println!("purity: no #[path] or include! in src/");
    check_no_source_escapes(t)?;
    println!("purity: no clippy cfg in src/");
    check_no_clippy_cfg(t)?;
    println!("purity: wasm32-unknown-unknown lib build");
    check_wasm_build(t)?;
    println!("purity: ban list over normal dependencies");
    check_ban_list(t)?;
    println!("purity: no native build tools over normal and build dependencies");
    check_build_tree(t)?;
    println!(
        "purity: clippy --lib with clippy.toml, -D warnings, -F disallowed lints and unsafe_code"
    );
    check_clippy(t)?;
    println!("purity: all checks passed for {}", t.package);
    Ok(())
}

/// `cargo xtask purity` on the repo at `root`.
pub fn run(root: &Path) -> Result<()> {
    check(&Target::core(root))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    // Canned `cargo tree -e normal --prefix none` output.
    const CLEAN_TREE: &str = "\
earshot-core v0.1.0 (C:\\repo\\crates\\earshot-core)
serde v1.0.228
serde_derive v1.0.228 (proc-macro)
proc-macro2 v1.0.101
uuid v1.18.1
";

    #[test]
    fn clean_tree_has_no_banned_crates() {
        assert!(banned_in_tree(CLEAN_TREE).is_empty());
    }

    #[test]
    fn banned_crates_in_tree_are_reported() {
        let tree = format!(
            "{CLEAN_TREE}getrandom v0.3.3\nlibc v0.2.177\nwindows-targets v0.52.6\ngetrandom v0.3.3 (*)\n"
        );
        assert_eq!(
            banned_in_tree(&tree),
            ["getrandom", "libc", "windows-targets"]
        );
    }

    #[test]
    fn ban_list_covers_the_plan_minimum() {
        for name in [
            "windows",
            "windows-sys",
            "ndk",
            "jni",
            "libc",
            "cpal",
            "ort",
            "whisper-rs",
            "reqwest",
            "hyper",
            "tokio",
            "rusqlite",
            "getrandom",
            "rayon",
            "mio",
            "socket2",
        ] {
            assert!(is_banned(name), "{name} must be banned");
        }
    }

    // Canned `cargo tree -e normal,build --prefix none` output.
    const BUILD_TREE_WITH_CC: &str = "\
earshot-core v0.1.0 (C:\\repo\\crates\\earshot-core)
serde v1.0.228
some-sys v0.1.0
cc v1.2.0
shlex v1.3.0
pkg-config v0.3.31
";

    #[test]
    fn native_builders_in_build_tree_are_reported() {
        assert!(native_builders_in_tree(CLEAN_TREE).is_empty());
        assert_eq!(
            native_builders_in_tree(BUILD_TREE_WITH_CC),
            ["cc", "pkg-config"]
        );
    }

    #[test]
    fn native_builder_list_covers_the_plan_minimum() {
        for name in ["cc", "cmake", "bindgen", "pkg-config"] {
            assert!(NATIVE_BUILDERS.contains(&name), "{name} must be banned");
        }
        assert!(!NATIVE_BUILDERS.contains(&"autocfg"));
    }

    #[test]
    fn purity_clippy_forbids_disallowed_lints_and_unsafe() {
        let args = clippy_args("earshot-core");
        let tail = args.iter().skip_while(|a| **a != "--").collect::<Vec<_>>();
        for pair in [
            ["-D", "warnings"],
            ["-F", "clippy::disallowed_methods"],
            ["-F", "clippy::disallowed_types"],
            ["-F", "unsafe_code"],
        ] {
            assert!(
                tail.windows(2)
                    .any(|w| *w[0] == pair[0] && *w[1] == pair[1]),
                "missing {pair:?} in {args:?}"
            );
        }
        assert!(args.contains(&"--lib"));
    }

    #[test]
    fn real_core_lib_forbids_unsafe_code() {
        check_forbid_unsafe(&Target::core(&crate::repo_root()))
            .expect("core lib.rs forbids unsafe");
    }

    #[test]
    fn forbid_unsafe_source_check() {
        assert!(has_forbid_unsafe(
            "//! doc\n#![forbid(unsafe_code)]\n\npub mod a;\n"
        ));
        assert!(!has_forbid_unsafe("//! doc\npub mod a;\n"));
        assert!(!has_forbid_unsafe(
            "// #![forbid(unsafe_code)]\npub mod a;\n"
        ));
        assert!(!has_forbid_unsafe("#![deny(unsafe_code)]\n"));
    }

    #[test]
    fn ban_list_matches_names_exactly_not_substrings() {
        for name in [
            "serde",
            "uuid",
            "rustfft",
            "rand_core",
            "libcore-thing",
            "ortho",
        ] {
            assert!(!is_banned(name), "{name} must not be banned");
        }
        assert!(is_banned("windows_x86_64_msvc"));
        assert!(is_banned("tokio-util"));
    }

    // ----- fail-closed verdicts on captured `cargo tree` output (no cargo run) -----

    fn tree_output(stdout: &str, success: bool, stdout_complete: bool) -> CmdOutput {
        CmdOutput {
            success,
            code: Some(if success { 0 } else { 101 }),
            stdout: stdout.into(),
            stderr: String::new(),
            stdout_complete,
            stderr_complete: true,
        }
    }

    const CANARY_TREE: &str = "canary v0.1.0 (C:\\tmp\\canary)\nserde v1.0.228\n";

    /// Both tree checks, so every fail-closed case is asserted for each.
    fn tree_verdicts(out: &CmdOutput) -> [Result<()>; 2] {
        [
            evaluate_ban_list("canary", &ban_list_args("canary"), out),
            evaluate_build_tree("canary", &build_tree_args("canary"), out),
        ]
    }

    #[test]
    fn tree_checks_pass_a_complete_rooted_clean_tree() {
        for r in tree_verdicts(&tree_output(CANARY_TREE, true, true)) {
            r.expect("a complete clean tree passes");
        }
    }

    #[test]
    fn tree_checks_fail_closed_on_empty_output() {
        // The fail-open seen under load: cargo exited 0 but nothing was captured.
        for out in [
            tree_output("", true, true),
            tree_output("", true, false),
            tree_output("\n\n", true, true),
        ] {
            for r in tree_verdicts(&out) {
                let err = format!("{:#}", r.unwrap_err());
                assert!(
                    err.contains("not fully captured") || err.contains("root package line"),
                    "{err}"
                );
            }
        }
    }

    #[test]
    fn tree_checks_fail_closed_on_incomplete_output_even_with_a_root_line() {
        // Truncated after the root: a banned crate further down could have been cut off.
        for r in tree_verdicts(&tree_output(
            "canary v0.1.0 (C:\\tmp\\canary)\n",
            true,
            false,
        )) {
            let err = format!("{:#}", r.unwrap_err());
            assert!(err.contains("not fully captured"), "{err}");
            assert!(err.contains("exit code 0"), "{err}");
        }
    }

    #[test]
    fn tree_checks_fail_closed_on_output_without_the_root_line() {
        for tree in [
            "serde v1.0.228\nlibm v0.2.15\n",
            "other v0.1.0 (C:\\tmp\\other)\nserde v1.0.228\n",
            "canary\n",
            "canary-extra v0.1.0\n",
            "warning: something\ncanary v0.1.0\n",
        ] {
            for r in tree_verdicts(&tree_output(tree, true, true)) {
                let err = format!("{:#}", r.unwrap_err());
                assert!(err.contains("root package line `canary v...`"), "{err}");
            }
        }
    }

    #[test]
    fn tree_checks_report_a_failed_run_with_status_and_output() {
        let mut out = tree_output("", false, true);
        out.stderr = "error: package ID specification `canary` did not match".into();
        for r in tree_verdicts(&out) {
            let err = format!("{:#}", r.unwrap_err());
            assert!(err.contains("exit code 101"), "{err}");
            assert!(err.contains("did not match"), "{err}");
        }
    }

    #[test]
    fn tree_checks_still_find_banned_crates_in_complete_output() {
        let tree = "canary v0.1.0 (C:\\tmp\\canary)\nlibc v0.2.0\ncc v1.2.0\n";
        let [ban, build] = tree_verdicts(&tree_output(tree, true, true));
        assert!(format!("{:#}", ban.unwrap_err()).contains("banned normal dependencies"));
        assert!(format!("{:#}", build.unwrap_err()).contains("native build tools"));
    }

    #[test]
    fn real_tree_args_are_the_ones_evaluated() {
        // The verdict helpers are only as good as the args the checks really pass.
        assert_eq!(
            ban_list_args("p"),
            [
                "tree", "-p", "p", "-e", "normal", "--target", "all", "--prefix", "none"
            ]
        );
        assert_eq!(
            build_tree_args("p"),
            [
                "tree",
                "-p",
                "p",
                "-e",
                "normal,build",
                "--target",
                "all",
                "--prefix",
                "none"
            ]
        );
    }

    // ----- canaries: real cargo runs against throwaway crates in a temp dir -----

    /// The `cargo tree` output a check saw, for a canary's failure message when a check
    /// unexpectedly passed.
    fn tree_for_message(t: &Target, args: &[&str]) -> String {
        match run_cargo(t, args) {
            Ok(out) => out.describe(),
            Err(e) => format!("{e:#}"),
        }
    }

    static SEQ: AtomicU32 = AtomicU32::new(0);

    /// A throwaway standalone workspace with one lib crate `canary` and the real
    /// earshot-core `clippy.toml`. Removed on drop.
    struct Canary {
        dir: PathBuf,
    }

    impl Canary {
        fn new(tag: &str, lib_rs: &str, extra_manifest: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "earshot-purity-canary-{}-{}-{tag}",
                std::process::id(),
                SEQ.fetch_add(1, Ordering::Relaxed)
            ));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(dir.join("src")).unwrap();
            std::fs::write(
                dir.join("Cargo.toml"),
                format!(
                    "[package]\nname = \"canary\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n\
                     [workspace]\n\n{extra_manifest}"
                ),
            )
            .unwrap();
            std::fs::write(dir.join("src").join("lib.rs"), lib_rs).unwrap();
            let clippy_toml = crate::repo_root()
                .join("crates")
                .join(CORE_PACKAGE)
                .join("clippy.toml");
            std::fs::copy(clippy_toml, dir.join("clippy.toml")).unwrap();
            Canary { dir }
        }

        /// Adds a local path crate (no network) named `name` under `deps/<name>`.
        fn add_path_crate(&self, name: &str) {
            self.add_path_crate_with(name, "");
        }

        /// As [`Canary::add_path_crate`], appending `extra_manifest` to its Cargo.toml.
        fn add_path_crate_with(&self, name: &str, extra_manifest: &str) {
            let d = self.dir.join("deps").join(name);
            std::fs::create_dir_all(d.join("src")).unwrap();
            std::fs::write(
                d.join("Cargo.toml"),
                format!(
                    "[package]\nname = \"{name}\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n\
                     {extra_manifest}"
                ),
            )
            .unwrap();
            std::fs::write(d.join("src").join("lib.rs"), "").unwrap();
        }

        fn target(&self) -> Target {
            Target {
                root: self.dir.clone(),
                package: "canary".into(),
                manifest_dir: self.dir.clone(),
                target_dir: Some(self.dir.join("target")),
            }
        }
    }

    impl Drop for Canary {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    const PURE_LIB: &str = "//! canary\n#![forbid(unsafe_code)]\n\n/// Adds.\npub fn add(a: u32, b: u32) -> u32 {\n    a + b\n}\n";

    #[test]
    fn canary_pure_crate_passes_every_check() {
        let c = Canary::new("pure", PURE_LIB, "");
        check(&c.target()).expect("a pure crate passes purity");
    }

    #[test]
    fn canary_platform_code_fails_wasm_build() {
        let lib = "use std::os::windows::ffi::OsStrExt;\n\
                   /// Encodes.\n\
                   pub fn wide(s: &std::ffi::OsStr) -> Vec<u16> {\n    s.encode_wide().collect()\n}\n";
        let c = Canary::new("wasm", lib, "");
        let err = check_wasm_build(&c.target()).unwrap_err();
        assert!(
            format!("{err:#}").contains("wasm32 build failed"),
            "{err:#}"
        );
    }

    #[test]
    fn canary_banned_normal_dependency_trips_ban_list() {
        let c = Canary::new(
            "bannednormal",
            PURE_LIB,
            "[dependencies]\nlibc = { path = \"deps/libc\" }\n",
        );
        c.add_path_crate("libc");
        let t = c.target();
        let Err(err) = check_ban_list(&t) else {
            panic!(
                "the ban list passed; a re-run of cargo tree printed:\n{}",
                tree_for_message(&t, &ban_list_args(&t.package))
            );
        };
        assert!(format!("{err:#}").contains("libc"), "{err:#}");
    }

    #[test]
    fn canary_banned_dev_dependency_is_allowed() {
        let c = Canary::new(
            "banneddev",
            PURE_LIB,
            "[dev-dependencies]\nlibc = { path = \"deps/libc\" }\n",
        );
        c.add_path_crate("libc");
        check_ban_list(&c.target()).expect("dev-dependencies are outside the contract");
    }

    #[test]
    fn canary_disallowed_calls_in_lib_fail_clippy() {
        // One call per category the contract names (PLAN.md 3.2).
        let lib = r#"//! canary
/// fs
pub fn a() -> String { std::fs::read_to_string("x").unwrap_or_default() }
/// fs type
pub fn b() -> bool { std::fs::File::open("x").is_ok() }
/// net
pub fn c() -> bool { std::net::TcpStream::connect("127.0.0.1:1").is_ok() }
/// process
pub fn d() -> bool { std::process::Command::new("x").status().is_ok() }
/// thread
pub fn e() { let _ = std::thread::spawn(|| ()); }
/// wall clock
pub fn f() -> std::time::SystemTime { std::time::SystemTime::now() }
/// monotonic clock
pub fn g() -> std::time::Instant { std::time::Instant::now() }
/// env var
pub fn h() -> bool { std::env::var("X").is_ok() }
/// env vars
pub fn i() -> usize { std::env::vars().count() }
/// env var_os
pub fn j() -> bool { std::env::var_os("X").is_some() }
/// path probes
pub fn k(p: &std::path::Path) -> bool {
    p.exists()
        || p.try_exists().is_ok()
        || p.is_file()
        || p.is_dir()
        || p.metadata().is_ok()
        || p.symlink_metadata().is_ok()
        || p.canonicalize().is_ok()
        || p.read_dir().is_ok()
        || p.read_link().is_ok()
}
/// dir entry type
pub fn l(e: &std::fs::DirEntry) -> std::ffi::OsString { e.file_name() }
/// stdio
pub fn m() -> usize {
    let _ = std::io::stdin();
    let _ = std::io::stdout();
    let _ = std::io::stderr();
    0
}
/// env mutation
pub fn n() {
    unsafe { std::env::set_var("X", "1") };
    unsafe { std::env::remove_var("X") };
    let _ = std::env::set_current_dir("x");
}
"#;
        let c = Canary::new("disallowed", lib, "");
        let err = format!("{:#}", check_clippy(&c.target()).unwrap_err());
        for path in [
            "std::fs::read_to_string",
            "std::fs::File",
            "std::net::TcpStream",
            "std::process::Command",
            "std::thread::spawn",
            "std::time::SystemTime::now",
            "std::time::Instant::now",
            "std::env::var`",
            "std::env::vars`",
            "std::env::var_os",
            "std::path::Path::exists",
            "std::path::Path::try_exists",
            "std::path::Path::is_file",
            "std::path::Path::is_dir",
            "std::path::Path::metadata",
            "std::path::Path::symlink_metadata",
            "std::path::Path::canonicalize",
            "std::path::Path::read_dir",
            "std::path::Path::read_link",
            "std::fs::DirEntry",
            "std::io::stdin",
            "std::io::stdout",
            "std::io::stderr",
            "std::env::set_var",
            "std::env::remove_var",
            "std::env::set_current_dir",
        ] {
            assert!(err.contains(path), "clippy did not flag {path}:\n{err}");
        }
    }

    #[test]
    fn canary_disallowed_call_in_test_code_passes_lib_check() {
        let lib = "/// Adds.\npub fn add(a: u32, b: u32) -> u32 {\n    a + b\n}\n\n\
                   #[cfg(test)]\nmod tests {\n    #[test]\n    fn reads_fixture() {\n        \
                   let _ = std::fs::read_to_string(\"fixture.txt\");\n        \
                   let _ = std::env::var(\"EARSHOT_FIXTURES\");\n    }\n}\n";
        let c = Canary::new("testonly", lib, "");
        check_clippy(&c.target()).expect("test-only fs/env use is outside the lib check");
    }

    #[test]
    fn canary_native_builder_build_dependency_trips_build_tree() {
        // canary -> mid (normal) -> cc (build-dependency of mid), all local path crates.
        let c = Canary::new(
            "nativebuild",
            PURE_LIB,
            "[dependencies]\nmid = { path = \"deps/mid\" }\n",
        );
        c.add_path_crate("cc");
        c.add_path_crate_with("mid", "[build-dependencies]\ncc = { path = \"../cc\" }\n");
        std::fs::write(
            c.dir.join("deps").join("mid").join("build.rs"),
            "fn main() {}\n",
        )
        .unwrap();
        // The normal-edge ban list cannot see a build-dependency ...
        check_ban_list(&c.target()).expect("cc is not a normal dependency");
        // ... the build-edge pass does.
        let t = c.target();
        let Err(err) = check_build_tree(&t) else {
            panic!(
                "the build-tree check passed; a re-run of cargo tree printed:\n{}",
                tree_for_message(&t, &build_tree_args(&t.package))
            );
        };
        let err = format!("{err:#}");
        assert!(
            err.contains("native build tools") && err.contains("cc"),
            "{err}"
        );
    }

    #[test]
    fn canary_native_builder_dev_dependency_is_allowed() {
        let c = Canary::new(
            "nativedev",
            PURE_LIB,
            "[dev-dependencies]\ncc = { path = \"deps/cc\" }\n",
        );
        c.add_path_crate("cc");
        check_build_tree(&c.target()).expect("dev-dependencies are outside the contract");
    }

    #[test]
    fn canary_allow_attribute_cannot_silence_disallowed_methods() {
        let lib = "//! canary\n#![forbid(unsafe_code)]\n\n/// Reads.\n\
                   #[allow(clippy::disallowed_methods)]\n\
                   pub fn a() -> Vec<u8> {\n    std::fs::read(\"x\").unwrap_or_default()\n}\n\n\
                   /// Opens.\n#[allow(clippy::disallowed_types)]\n\
                   pub fn b() -> Option<std::fs::File> {\n    None\n}\n";
        let c = Canary::new("allow", lib, "");
        let err = format!("{:#}", check_clippy(&c.target()).unwrap_err());
        assert!(err.contains("clippy::disallowed_methods"), "{err}");
        assert!(err.contains("clippy::disallowed_types"), "{err}");
    }

    #[test]
    fn canary_ffi_call_fails_under_forbid_unsafe() {
        let lib = "//! canary\n#![forbid(unsafe_code)]\n\n\
                   unsafe extern \"C\" {\n    fn abs(x: i32) -> i32;\n}\n\n\
                   /// Calls C.\npub fn a() -> i32 {\n    unsafe { abs(-1) }\n}\n";
        let c = Canary::new("ffi", lib, "");
        let err = format!("{:#}", check_clippy(&c.target()).unwrap_err());
        assert!(err.contains("unsafe_code"), "{err}");
    }

    #[test]
    fn canary_ffi_call_fails_through_the_forbid_flag_alone() {
        // No `#![forbid(unsafe_code)]` in source and an explicit allow: only the
        // `-F unsafe_code` flag of the purity clippy run can reject this (an in-source
        // allow cannot override a command-line forbid).
        let lib = "//! canary\n#![allow(unsafe_code)]\n\n\
                   unsafe extern \"C\" {\n    fn abs(x: i32) -> i32;\n}\n\n\
                   /// Calls C.\npub fn a() -> i32 {\n    unsafe { abs(-1) }\n}\n";
        let c = Canary::new("ffiflag", lib, "");
        let err = format!("{:#}", check_clippy(&c.target()).unwrap_err());
        assert!(err.contains("unsafe_code"), "{err}");
    }

    #[test]
    fn clippy_cfg_variants_are_found() {
        for src in [
            "#[cfg(clippy)]\nfn a() {}\n",
            "#[cfg(not(clippy))]\nfn a() {}\n",
            "#[cfg( not ( clippy ) )]\nfn a() {}\n",
            "#[cfg(any(test,\n    clippy))]\nfn a() {}\n",
            "#[cfg_attr(clippy, path = \"x.rs\")]\nmod x;\n",
            "#[cfg_attr(not(clippy), allow(dead_code))]\nfn a() {}\n",
            "#![cfg_attr(all(clippy, test), allow(unused))]\n",
            "fn a() -> bool { cfg!(clippy) }\n",
            "fn a() -> bool { cfg ! ( not(clippy) ) }\n",
        ] {
            assert_eq!(clippy_cfgs(src).len(), 1, "{src:?}");
        }
        assert_eq!(
            clippy_cfgs("#[cfg(not(clippy))]\nfn a() {}\n"),
            ["cfg(not(clippy))"]
        );
    }

    #[test]
    fn clippy_lint_paths_and_lookalikes_are_not_clippy_cfgs() {
        for src in [
            "#[cfg_attr(test, allow(clippy::unwrap_used))]\nfn a() {}\n",
            "#[allow(clippy::too_many_arguments)]\nfn a() {}\n",
            "#[cfg(test)]\nmod tests {}\n",
            "#[cfg(feature = \"clippy_extra\")]\nfn a() {}\n",
            "#[cfg(not_clippy)]\nfn a() {}\n",
            "fn mycfg(x: u8) {} fn b() { mycfg(clippy_level()) }\n",
            "// clippy is run by purity\n",
        ] {
            assert!(
                clippy_cfgs(src).is_empty(),
                "{src:?}: {:?}",
                clippy_cfgs(src)
            );
        }
    }

    #[test]
    fn real_core_sources_have_no_clippy_cfg() {
        check_no_clippy_cfg(&Target::core(&crate::repo_root())).expect("no clippy cfg in core");
    }

    #[test]
    fn canary_cfg_not_clippy_hides_lib_code_from_clippy_but_fails_the_source_scan() {
        let lib = "//! canary\n#![forbid(unsafe_code)]\n\n\
                   /// Reads.\n#[cfg(not(clippy))]\n\
                   pub fn read() -> Vec<u8> {\n    std::fs::read(\"x\").unwrap_or_default()\n}\n";
        let c = Canary::new("cfgclippy", lib, "");
        // The gap: clippy never sees the read, so check 4 alone passes.
        check_clippy(&c.target()).expect("cfg(not(clippy)) hides the read from clippy");
        let err = format!("{:#}", check_no_clippy_cfg(&c.target()).unwrap_err());
        assert!(err.contains("cfg(not(clippy))"), "{err}");
        assert!(check(&c.target()).is_err(), "full purity run must fail");

        // A clippy cfg in a nested module file is found too.
        std::fs::write(c.dir.join("src").join("lib.rs"), PURE_LIB).unwrap();
        check_no_clippy_cfg(&c.target()).expect("clean again");
        std::fs::create_dir_all(c.dir.join("src").join("inner")).unwrap();
        std::fs::write(
            c.dir.join("src").join("inner").join("m.rs"),
            "#[cfg_attr(clippy, allow(dead_code))]\nfn a() {}\n",
        )
        .unwrap();
        let err = format!("{:#}", check_no_clippy_cfg(&c.target()).unwrap_err());
        assert!(
            err.contains("m.rs") && err.contains("cfg_attr(clippy,"),
            "{err}"
        );
    }

    // The std::os::windows paths in core's clippy.toml resolve only on a Windows host (the
    // only build hosts); elsewhere this canary is a visible skip, not a silent pass.
    #[test]
    #[cfg_attr(
        not(windows),
        ignore = "std::os::windows paths resolve only on Windows hosts"
    )]
    fn canary_windows_only_fs_and_process_calls_fail_clippy() {
        // `cfg(windows)` keeps these out of the wasm32 build; the clippy run on a
        // Windows host must still reject them.
        let lib = r#"//! canary
#![forbid(unsafe_code)]
/// Symlinks a file.
#[cfg(windows)]
pub fn a() -> bool { std::os::windows::fs::symlink_file("a", "b").is_ok() }
/// Symlinks a dir.
#[cfg(windows)]
pub fn b() -> bool { std::os::windows::fs::symlink_dir("a", "b").is_ok() }
/// Uses the process extension trait.
#[cfg(windows)]
pub fn c<T: std::os::windows::process::CommandExt>(_t: &T) {}
"#;
        let c = Canary::new("winos", lib, "");
        check_wasm_build(&c.target()).expect("cfg(windows) code is not in the wasm32 build");
        let err = format!("{:#}", check_clippy(&c.target()).unwrap_err());
        for path in [
            "std::os::windows::fs::symlink_file",
            "std::os::windows::fs::symlink_dir",
            "std::os::windows::process::CommandExt",
        ] {
            assert!(err.contains(path), "clippy did not flag {path}:\n{err}");
        }
    }

    // A non-Windows target (wasm32, the off-Windows check of adapter crates that depend on
    // core) cannot resolve the std::os::windows entries; they must not print config
    // warnings there, while the canary above keeps them enforced on Windows.
    #[test]
    fn canary_core_clippy_toml_has_no_unresolved_paths_on_wasm32() {
        let c = Canary::new("wasmcfg", PURE_LIB, "");
        let t = c.target();
        let args = [
            "clippy",
            "-p",
            "canary",
            "--lib",
            "--target",
            "wasm32-unknown-unknown",
            "--",
            "-D",
            "warnings",
        ];
        let mut cmd = cargo_command(&t, &args);
        cmd.env("CLIPPY_CONF_DIR", &t.manifest_dir);
        let out = run_with_timeout(&mut cmd, CARGO_TIMEOUT).expect("cargo clippy ran");
        require_success("wasm32 clippy", &args, &out).unwrap();
        assert!(
            !out.stderr.contains("does not refer to a reachable"),
            "core clippy.toml has entries that do not resolve on wasm32:\n{}",
            out.stderr
        );
    }

    #[test]
    fn source_escapes_are_found() {
        for src in [
            "#[path = \"../x.rs\"]\nmod x;\n",
            "#[path=\"x.rs\"] mod x;\n",
            "#[cfg_attr(test, path = \"../t.rs\")]\nmod t;\n",
            "include!(\"../gen.rs\");\n",
            "const A: u8 = include ! (concat!(\"a\", \".rs\"));\n",
        ] {
            assert_eq!(source_escapes(src).len(), 1, "{src:?}");
        }
        for src in [
            "const S: &str = include_str!(\"data/english.json\");\n",
            "const B: &[u8] = include_bytes!(\"x.bin\");\n",
            "fn f(path: &str) { let path = path; }\n",
            "macro_rules! my_include { () => {} }\nmy_include!();\n",
            "#[doc = \"a path = b\"]\nfn g() {}\n",
        ] {
            assert!(
                source_escapes(src).is_empty(),
                "{src:?}: {:?}",
                source_escapes(src)
            );
        }
    }

    #[test]
    fn real_core_sources_have_no_source_escapes() {
        check_no_source_escapes(&Target::core(&crate::repo_root()))
            .expect("no #[path] or include! in core");
    }

    #[test]
    fn canary_source_outside_src_evades_the_clippy_cfg_scan_but_not_the_escape_scan() {
        // The lib pulls a module from outside src/ that hides an fs read from clippy.
        let outside = "/// Reads.\n#[cfg(not(clippy))]\n\
                       pub fn read() -> Vec<u8> {\n    std::fs::read(\"x\").unwrap_or_default()\n}\n";
        for (tag, lib) in [
            (
                "pathattr",
                "//! canary\n#![forbid(unsafe_code)]\n\n/// Outside.\n#[path = \"../outside.rs\"]\npub mod outside;\n",
            ),
            (
                "includemacro",
                "//! canary\n#![forbid(unsafe_code)]\n\n/// Outside.\npub mod outside {\n    include!(\"../outside.rs\");\n}\n",
            ),
        ] {
            let c = Canary::new(tag, lib, "");
            std::fs::write(c.dir.join("outside.rs"), outside).unwrap();
            // The gap: neither clippy nor the src/ clippy-cfg scan sees the hidden read.
            check_clippy(&c.target()).expect("cfg(not(clippy)) hides the read from clippy");
            check_no_clippy_cfg(&c.target()).expect("outside.rs is not under src/");
            let err = format!("{:#}", check_no_source_escapes(&c.target()).unwrap_err());
            assert!(err.contains("outside.rs"), "{tag}: {err}");
            assert!(
                check(&c.target()).is_err(),
                "{tag}: full purity run must fail"
            );
        }
    }

    #[test]
    fn purity_child_cargo_scrubs_the_package_env_of_the_running_xtask() {
        let inherited: Vec<std::ffi::OsString> = std::env::vars_os()
            .map(|(k, _)| k)
            .filter(|k| k.to_str().is_some_and(crate::ci::is_package_env))
            .collect();
        // `cargo test` sets CARGO_MANIFEST_DIR (and CARGO_PKG_*) for the test process.
        assert!(
            !inherited.is_empty(),
            "expected cargo to set package variables for this test process"
        );
        let t = Target {
            target_dir: Some(PathBuf::from("tdir")),
            ..Target::core(&crate::repo_root())
        };
        for cmd in [
            cargo_command(&t, &["tree"]),
            clippy_command(&t),
            crate::ci::cargo_command(),
        ] {
            let envs: Vec<_> = cmd.get_envs().collect();
            for k in &inherited {
                assert!(
                    envs.iter().any(|(n, v)| n == k && v.is_none()),
                    "{k:?} not removed from {cmd:?}"
                );
            }
        }
        let clippy = clippy_command(&t);
        let get = |name: &str| {
            clippy
                .get_envs()
                .find(|(k, _)| *k == name)
                .and_then(|(_, v)| v)
                .map(Path::new)
        };
        assert_eq!(get("CLIPPY_CONF_DIR"), Some(t.manifest_dir.as_path()));
        assert_eq!(get("CARGO_TARGET_DIR"), Some(Path::new("tdir")));
    }

    #[test]
    fn canary_missing_forbid_unsafe_is_rejected() {
        let c = Canary::new("noforbid", "/// Adds.\npub fn add() {}\n", "");
        let err = format!("{:#}", check_forbid_unsafe(&c.target()).unwrap_err());
        assert!(err.contains("forbid(unsafe_code)"), "{err}");
    }

    /// A two-level workspace shaped like the repo: a neutral root `clippy.toml` (the
    /// repo's own) and a `core` member carrying earshot-core's `clippy.toml`.
    fn core_shaped_workspace(tag: &str, lib_rs: &str) -> Canary {
        let c = Canary::new(tag, PURE_LIB, "");
        std::fs::write(
            c.dir.join("Cargo.toml"),
            "[workspace]\nresolver = \"3\"\nmembers = [\"core\"]\n",
        )
        .unwrap();
        std::fs::copy(
            crate::repo_root().join("clippy.toml"),
            c.dir.join("clippy.toml"),
        )
        .unwrap();
        let core = c.dir.join("core");
        std::fs::create_dir_all(core.join("src")).unwrap();
        std::fs::create_dir_all(core.join("tests")).unwrap();
        std::fs::write(
            core.join("Cargo.toml"),
            "[package]\nname = \"core\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
        )
        .unwrap();
        std::fs::copy(
            crate::repo_root()
                .join("crates")
                .join(CORE_PACKAGE)
                .join("clippy.toml"),
            core.join("clippy.toml"),
        )
        .unwrap();
        std::fs::write(core.join("src").join("lib.rs"), lib_rs).unwrap();
        std::fs::write(
            core.join("tests").join("reads_fixture.rs"),
            "#[test]\nfn reads_fixture() {\n    let _ = std::fs::read(\"fixture.bin\");\n}\n",
        )
        .unwrap();
        c
    }

    fn core_target(c: &Canary) -> Target {
        Target {
            root: c.dir.clone(),
            package: "core".into(),
            manifest_dir: c.dir.join("core"),
            target_dir: Some(c.dir.join("target")),
        }
    }

    fn run_ci_clippy(c: &Canary) -> CmdOutput {
        let (name, args) = crate::ci::STEPS
            .iter()
            .find(|s| s.0 == "clippy")
            .expect("ci has a clippy step");
        let mut cmd = crate::ci::step_command(&c.dir, name, args);
        cmd.env("CARGO_TARGET_DIR", c.dir.join("target"));
        let out = run_with_timeout(&mut cmd, CARGO_TIMEOUT).expect("cargo clippy ran");
        out.require_complete("ci clippy step").unwrap();
        out
    }

    const CORE_LIB_WITH_TEST_READ: &str = "//! canary\n#![forbid(unsafe_code)]\n\n\
        /// Adds.\npub fn add(a: u32, b: u32) -> u32 {\n    a + b\n}\n\n\
        #[cfg(test)]\nmod tests {\n    #[test]\n    fn reads_fixture() {\n        \
        let _ = std::fs::read(\"fixture.bin\");\n    }\n}\n";

    #[test]
    fn canary_core_test_reading_a_file_passes_ci_clippy_but_lib_read_fails_purity() {
        let c = core_shaped_workspace("cilint", CORE_LIB_WITH_TEST_READ);
        let out = run_ci_clippy(&c);
        assert!(
            out.success,
            "ci clippy must not apply core's clippy.toml to test targets:\n{}",
            out.describe()
        );
        check_clippy(&core_target(&c)).expect("test-only reads are outside the lib check");

        // Control: the same workspace linted with core's own clippy.toml (no neutral
        // CLIPPY_CONF_DIR) does flag the test read, so the split is what lets it pass.
        let mut plain = crate::ci::cargo_command();
        plain
            .args([
                "clippy",
                "--workspace",
                "--all-targets",
                "--",
                "-D",
                "warnings",
            ])
            .current_dir(&c.dir)
            .env_remove("CLIPPY_CONF_DIR")
            .env("CARGO_TARGET_DIR", c.dir.join("target"));
        let out = run_with_timeout(&mut plain, CARGO_TIMEOUT).expect("cargo clippy ran");
        out.require_complete("control clippy run").unwrap();
        assert!(
            !out.success,
            "control run should flag std::fs::read in tests:\n{}",
            out.describe()
        );
        assert!(out.stderr.contains("std::fs::read"), "{}", out.describe());

        // The same call in lib code fails purity.
        let lib = format!(
            "{CORE_LIB_WITH_TEST_READ}\n/// Reads.\npub fn read() -> Vec<u8> {{\n    \
             std::fs::read(\"x\").unwrap_or_default()\n}}\n"
        );
        std::fs::write(c.dir.join("core").join("src").join("lib.rs"), lib).unwrap();
        let err = format!("{:#}", check_clippy(&core_target(&c)).unwrap_err());
        assert!(err.contains("std::fs::read"), "{err}");
    }

    #[test]
    fn canary_build_script_is_rejected() {
        let c = Canary::new("buildrs", PURE_LIB, "");
        std::fs::write(c.dir.join("build.rs"), "fn main() {}\n").unwrap();
        assert!(check_no_build_script(&c.target()).is_err());
    }
}
