//! `ci` step `net`: no HTTP client outside `earshot-net` in shipped crates (PLAN.md 3.9,
//! decision D14).
//!
//! Runs `cargo tree --workspace --exclude xtask -e normal --all-features --target all
//! --prefix depth --no-dedupe` and fails if any [`HTTP_CLIENT_CRATES`] entry appears in a
//! shipped crate's normal dependency graph other than beneath [`NET_PACKAGE`]. A crate that
//! depends on `earshot-net` (for example `earshot-llm`) gets reqwest only through it, which
//! is allowed; a direct or other indirect dependency is not. xtask, a dev tool, is exempt;
//! dev- and build-dependencies are outside the check.
//!
//! App crates listed in [`SHIPPED_TARGETS`] are left out of that run and checked instead
//! on each target they ship to (`cargo tree -p <package> --target <triple>`), with the same
//! rule and no exemption (Tauri 2 depends on reqwest only on Android and
//! iOS, which the desktop app does not ship to; the Android edge is decided at T10.2).
//!
//! This replaces a crate-level `clippy.toml` `disallowed_types` rule: those are only
//! enforced by dedicated xtask steps (AGENTS.md section 11), and a dependency-graph check
//! also catches clients reached through other crates.

use std::path::Path;

use anyhow::{Result, bail};

use crate::proc::{CmdOutput, run_with_timeout};
use crate::purity::CARGO_TIMEOUT;

/// The one crate allowed to depend on an HTTP client.
pub const NET_PACKAGE: &str = "earshot-net";

/// Workspace members exempt from the check (dev tools, never shipped).
pub const EXEMPT_PACKAGES: &[&str] = &["xtask", "earshot-llm-replay"];

/// Non-shipped test-support members (also in [`EXEMPT_PACKAGES`]). A shipped crate may
/// use them only as a dev-dependency; one in a shipped crate's normal dependency graph is
/// a violation, so test code (a loopback mock server) cannot reach a shipped binary.
pub const TEST_SUPPORT_PACKAGES: &[&str] = &["earshot-llm-replay"];

/// HTTP client (and client-stack) crates that only `earshot-net` may pull in.
pub const HTTP_CLIENT_CRATES: &[&str] = &[
    "reqwest",
    "hyper",
    "hyper-util",
    "ureq",
    "isahc",
    "attohttpc",
    "surf",
    "curl",
    "curl-sys",
    "http-client",
    "minreq",
    "ehttp",
    "wreq",
    "async-h1",
    "awc",
    "hyper-rustls",
    "hyper-tls",
    "reqwest-middleware",
    "tungstenite",
    "tokio-tungstenite",
];

/// Workspace members checked only on the targets they ship to, each target in its own
/// `cargo tree` run ([`shipped_tree_args`]); every other shipped member is checked with
/// `--target all`. Adding a package here narrows its check: it needs the owner's agreement.
pub const SHIPPED_TARGETS: &[(&str, &[&str])] = &[(
    "earshot-desktop",
    &["x86_64-pc-windows-msvc", "aarch64-pc-windows-msvc"],
)];

/// Arguments of the `--target all` `cargo tree` run over the shipped members not in
/// [`SHIPPED_TARGETS`].
pub fn tree_args() -> Vec<&'static str> {
    let mut args = vec!["tree", "--workspace"];
    for p in EXEMPT_PACKAGES {
        args.extend(["--exclude", p]);
    }
    for (p, _) in SHIPPED_TARGETS {
        args.extend(["--exclude", p]);
    }
    args.extend([
        "-e",
        "normal",
        "--all-features",
        "--target",
        "all",
        "--prefix",
        "depth",
        "--no-dedupe",
    ]);
    args
}

/// Arguments of the `cargo tree` run that checks `package` on one shipped `target`.
pub fn shipped_tree_args<'a>(package: &'a str, target: &'a str) -> Vec<&'a str> {
    vec![
        "tree",
        "-p",
        package,
        "-e",
        "normal",
        "--all-features",
        "--target",
        target,
        "--prefix",
        "depth",
        "--no-dedupe",
    ]
}

/// One forbidden dependency: an HTTP client crate found in the normal dependency graph of
/// the workspace member `package` outside `earshot-net`, or a test-support crate
/// ([`TEST_SUPPORT_PACKAGES`]) anywhere in it.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Violation {
    /// Workspace member (tree root).
    pub package: String,
    /// The HTTP client or test-support crate.
    pub client: String,
}

/// Parses `cargo tree --prefix depth --no-dedupe` output (one tree per workspace member,
/// each line `<depth><name> v<version> ...`) and returns the violations, sorted and
/// deduplicated. Subtrees rooted at [`NET_PACKAGE`] are skipped. Errors on lines that do
/// not start with a depth or when no tree root is found, so a format change cannot pass
/// silently.
pub fn violations_in_tree(tree: &str) -> Result<Vec<Violation>> {
    let mut found = Vec::new();
    let mut root: Option<String> = None;
    let mut skip_deeper_than: Option<usize> = None;
    for line in tree.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let digits = line.bytes().take_while(u8::is_ascii_digit).count();
        if digits == 0 {
            bail!("net: unexpected `cargo tree` line (no depth prefix): {line}");
        }
        let depth: usize = line[..digits].parse()?;
        let Some(name) = line[digits..].split_whitespace().next() else {
            bail!("net: unexpected `cargo tree` line (no crate name): {line}");
        };
        if let Some(d) = skip_deeper_than {
            if depth > d {
                continue;
            }
            skip_deeper_than = None;
        }
        if depth == 0 {
            root = Some(name.to_string());
        }
        if name == NET_PACKAGE {
            skip_deeper_than = Some(depth);
            continue;
        }
        if HTTP_CLIENT_CRATES.contains(&name) || TEST_SUPPORT_PACKAGES.contains(&name) {
            let Some(package) = &root else {
                bail!("net: dependency line before any tree root: {line}");
            };
            found.push(Violation {
                package: package.clone(),
                client: name.to_string(),
            });
        }
    }
    if root.is_none() {
        bail!("net: no dependency tree found in `cargo tree` output");
    }
    found.sort();
    found.dedup();
    Ok(found)
}

/// The violations in a captured `cargo tree` run. Fails closed: output that was not
/// captured completely (a cut-off tree could hide a member) or a failed run is an error,
/// and [`violations_in_tree`] rejects output without a tree root.
pub fn evaluate(args: &[&str], out: &CmdOutput) -> Result<Vec<Violation>> {
    out.require_complete(&format!("net: `cargo {}`", args.join(" ")))?;
    if !out.success {
        bail!("net: `cargo {}` failed: {}", args.join(" "), out.describe());
    }
    violations_in_tree(&out.stdout)
}

/// The violations in a captured single-package run ([`shipped_tree_args`]). Fails closed
/// like [`evaluate`], and also unless the output is exactly one tree rooted at `package`
/// ([`crate::purity::require_tree_root`] on the first line without its depth prefix).
pub fn evaluate_shipped(package: &str, args: &[&str], out: &CmdOutput) -> Result<Vec<Violation>> {
    out.require_complete(&format!("net: `cargo {}`", args.join(" ")))?;
    if !out.success {
        bail!("net: `cargo {}` failed: {}", args.join(" "), out.describe());
    }
    let first = out
        .stdout
        .lines()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("");
    crate::purity::require_tree_root(
        package,
        first.trim_start_matches(|c: char| c.is_ascii_digit()),
    )?;
    let roots = out
        .stdout
        .lines()
        .filter(|l| l.starts_with('0') && !l[1..].starts_with(|c: char| c.is_ascii_digit()))
        .count();
    if roots != 1 {
        bail!(
            "net: `cargo {}` printed {roots} dependency trees, expected only {package}'s",
            args.join(" ")
        );
    }
    violations_in_tree(&out.stdout)
}

fn capture(root: &Path, args: &[&str]) -> Result<CmdOutput> {
    let mut cmd = crate::ci::cargo_command();
    cmd.args(args).current_dir(root);
    match run_with_timeout(&mut cmd, CARGO_TIMEOUT) {
        Some(out) => Ok(out),
        None => bail!(
            "net: `cargo {}` could not start or exceeded {} s",
            args.join(" "),
            CARGO_TIMEOUT.as_secs()
        ),
    }
}

/// Runs the check on the workspace at `root`.
pub fn run(root: &Path) -> Result<()> {
    let args = tree_args();
    let mut found = evaluate(&args, &capture(root, &args)?)?;
    for (package, targets) in SHIPPED_TARGETS {
        for target in *targets {
            let args = shipped_tree_args(package, target);
            found.extend(evaluate_shipped(package, &args, &capture(root, &args)?)?);
        }
    }
    found.sort();
    found.dedup();
    if !found.is_empty() {
        let list: Vec<String> = found
            .iter()
            .map(|v| format!("{} -> {}", v.package, v.client))
            .collect();
        bail!(
            "net: HTTP client crates outside {NET_PACKAGE} (all HTTP goes through \
             {NET_PACKAGE}, PLAN.md 3.9) or test-support crates ({}) in shipped crates: {}",
            TEST_SUPPORT_PACKAGES.join(", "),
            list.join(", ")
        );
    }
    println!("net: no HTTP client outside {NET_PACKAGE}, no test-support crate in shipped crates");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // Canned `cargo tree --prefix depth --no-dedupe` output: earshot-net pulls reqwest and
    // hyper, earshot-llm depends on earshot-net only.
    const CLEAN: &str = "\
0earshot-core v0.1.0 (C:\\repo\\crates\\earshot-core)
1serde v1.0.228

0earshot-llm v0.1.0 (C:\\repo\\crates\\earshot-llm)
1earshot-net v0.1.0 (C:\\repo\\crates\\earshot-net)
2reqwest v0.13.5
3hyper v1.12.0
3hyper-util v0.1.21
4hyper v1.12.0
2url v2.5.8
1serde_json v1.0.149

0earshot-net v0.1.0 (C:\\repo\\crates\\earshot-net)
1reqwest v0.13.5
2hyper v1.12.0
1url v2.5.8
";

    fn v(package: &str, client: &str) -> Violation {
        Violation {
            package: package.into(),
            client: client.into(),
        }
    }

    #[test]
    fn net_clients_beneath_earshot_net_are_allowed() {
        assert_eq!(violations_in_tree(CLEAN).unwrap(), []);
    }

    #[test]
    fn direct_http_client_in_another_crate_is_a_violation() {
        let tree =
            format!("{CLEAN}\n0earshot-store v0.1.0 (C:\\repo)\n1ureq v3.4.2\n2rustls v0.23.46\n");
        assert_eq!(
            violations_in_tree(&tree).unwrap(),
            [v("earshot-store", "ureq")]
        );
    }

    #[test]
    fn client_next_to_earshot_net_is_a_violation() {
        // earshot-llm depends on earshot-net *and* directly on reqwest: the pruned subtree
        // must end at the sibling at the same depth.
        let tree = "\
0earshot-llm v0.1.0 (C:\\repo\\crates\\earshot-llm)
1earshot-net v0.1.0 (C:\\repo\\crates\\earshot-net)
2reqwest v0.13.5
3hyper v1.12.0
1reqwest v0.13.5
2hyper v1.12.0
";
        assert_eq!(
            violations_in_tree(tree).unwrap(),
            [v("earshot-llm", "hyper"), v("earshot-llm", "reqwest")]
        );
    }

    #[test]
    fn transitive_client_through_another_crate_is_a_violation() {
        let tree = "\
0earshot-onnx v0.1.0 (C:\\repo\\crates\\earshot-onnx)
1ort v2.0.0
2ureq v3.4.2
1some-sdk v1.0.0
2isahc v1.7.2
3curl v0.4.47
";
        assert_eq!(
            violations_in_tree(tree).unwrap(),
            [
                v("earshot-onnx", "curl"),
                v("earshot-onnx", "isahc"),
                v("earshot-onnx", "ureq")
            ]
        );
    }

    #[test]
    fn every_listed_client_is_caught() {
        for client in HTTP_CLIENT_CRATES {
            let tree = format!("0earshot-cli v0.1.0 (C:\\repo)\n1{client} v1.0.0\n");
            assert_eq!(
                violations_in_tree(&tree).unwrap(),
                [v("earshot-cli", client)],
                "{client}"
            );
        }
        for name in [
            "reqwest",
            "hyper",
            "ureq",
            "isahc",
            "attohttpc",
            "surf",
            "minreq",
            "curl",
            "curl-sys",
            "ehttp",
            "wreq",
            "async-h1",
            "awc",
            "hyper-rustls",
            "hyper-tls",
            "reqwest-middleware",
            "tungstenite",
            "tokio-tungstenite",
        ] {
            assert!(HTTP_CLIENT_CRATES.contains(&name), "{name}");
        }
    }

    #[test]
    fn test_support_crate_in_a_shipped_tree_is_a_violation() {
        let tree = format!(
            "{CLEAN}\n0earshot-runtime v0.1.0 (C:\\repo)\n1earshot-cli v0.1.0 (C:\\repo)\n2earshot-llm-replay v0.1.0 (C:\\repo)\n3earshot-net v0.1.0 (C:\\repo)\n4reqwest v0.13.5\n"
        );
        assert_eq!(
            violations_in_tree(&tree).unwrap(),
            [v("earshot-runtime", "earshot-llm-replay")]
        );
        for p in TEST_SUPPORT_PACKAGES {
            assert!(EXEMPT_PACKAGES.contains(p), "{p} is not a tree root");
        }
    }

    #[test]
    fn unparseable_output_fails_instead_of_passing() {
        assert!(violations_in_tree("").is_err());
        assert!(violations_in_tree("earshot-cli v0.1.0\n└── reqwest v0.13.5\n").is_err());
        assert!(violations_in_tree("1reqwest v0.13.5\n").is_err());
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
    fn evaluate_passes_only_complete_successful_output() {
        let args = tree_args();
        assert_eq!(
            evaluate(&args, &captured(CLEAN, true, true)).unwrap(),
            vec![]
        );
        // A clean-looking tree that was cut off could hide a later member's client.
        let err = format!(
            "{:#}",
            evaluate(&args, &captured(CLEAN, true, false)).unwrap_err()
        );
        assert!(err.contains("not fully captured"), "{err}");
        // Nothing captured from a run that exited 0 is not "no violations".
        assert!(evaluate(&args, &captured("", true, true)).is_err());
        assert!(evaluate(&args, &captured("", true, false)).is_err());
        let err = format!(
            "{:#}",
            evaluate(&args, &captured(CLEAN, false, true)).unwrap_err()
        );
        assert!(err.contains("exit code 101"), "{err}");
    }

    #[test]
    fn tree_args_cover_the_workspace_minus_xtask_with_normal_edges_only() {
        let args = tree_args();
        let joined = args.join(" ");
        assert!(joined.starts_with("tree --workspace --exclude xtask"));
        assert!(joined.contains("--exclude earshot-llm-replay"));
        for flag in [
            "-e normal",
            "--all-features",
            "--target all",
            "--prefix depth",
            "--no-dedupe",
        ] {
            assert!(joined.contains(flag), "{flag}");
        }
    }

    /// The rooted tree of `earshot-desktop` on one Windows target, with Tauri.
    const DESKTOP: &str = "\
0earshot-desktop v0.1.0 (C:\\repo\\apps\\desktop\\src-tauri)
1tauri v2.12.4
2serde v1.0.228
1earshot-runtime v0.1.0 (C:\\repo\\crates\\earshot-runtime)
2earshot-net v0.1.0 (C:\\repo\\crates\\earshot-net)
3reqwest v0.13.5
";

    #[test]
    fn earshot_desktop_is_checked_on_exactly_its_shipped_windows_targets() {
        let (package, targets) = SHIPPED_TARGETS
            .iter()
            .find(|(p, _)| *p == "earshot-desktop")
            .expect("earshot-desktop has shipped targets");
        assert_eq!(*package, "earshot-desktop");
        assert_eq!(
            *targets,
            ["x86_64-pc-windows-msvc", "aarch64-pc-windows-msvc"]
        );
        // Out of the `--target all` run, into one run per shipped target.
        let all = tree_args();
        assert!(
            all.windows(2)
                .any(|w| w == ["--exclude", "earshot-desktop"]),
            "{all:?}"
        );
        for target in *targets {
            let args = shipped_tree_args(package, target);
            let joined = args.join(" ");
            assert!(joined.starts_with("tree -p earshot-desktop"), "{joined}");
            assert!(
                args.windows(2).any(|w| w == ["--target", *target]),
                "{joined}"
            );
            assert!(!joined.contains("--target all"), "{joined}");
            for flag in [
                "-e normal",
                "--all-features",
                "--prefix depth",
                "--no-dedupe",
            ] {
                assert!(joined.contains(flag), "{flag}: {joined}");
            }
        }
    }

    #[test]
    fn every_package_with_shipped_targets_is_a_workspace_member() {
        let text = std::fs::read_to_string(crate::repo_root().join("Cargo.toml")).unwrap();
        let manifest: toml::Table = toml::from_str(&text).unwrap();
        let members: Vec<String> = manifest["workspace"]["members"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m.as_str().unwrap().to_string())
            .collect();
        for (package, targets) in SHIPPED_TARGETS {
            assert!(!targets.is_empty(), "{package}");
            let found = members.iter().any(|m| {
                let path = crate::repo_root().join(m).join("Cargo.toml");
                std::fs::read_to_string(path)
                    .ok()
                    .and_then(|t| toml::from_str::<toml::Table>(&t).ok())
                    .and_then(|t| {
                        t.get("package")?
                            .get("name")?
                            .as_str()
                            .map(|n| n == *package)
                    })
                    .unwrap_or(false)
            });
            assert!(found, "{package} is not a workspace member");
            assert!(!EXEMPT_PACKAGES.contains(package), "{package}");
        }
    }

    #[test]
    fn http_client_on_a_shipped_windows_target_still_fails() {
        let args = shipped_tree_args("earshot-desktop", "x86_64-pc-windows-msvc");
        // Beneath earshot-net is allowed, as everywhere.
        assert_eq!(
            evaluate_shipped("earshot-desktop", &args, &captured(DESKTOP, true, true)).unwrap(),
            vec![]
        );
        // Canary: an HTTP client reached through Tauri on a Windows target.
        let tree =
            format!("{DESKTOP}1tauri-plugin-http v2.0.0\n2reqwest v0.13.5\n3hyper v1.12.0\n");
        assert_eq!(
            evaluate_shipped("earshot-desktop", &args, &captured(&tree, true, true)).unwrap(),
            [
                v("earshot-desktop", "hyper"),
                v("earshot-desktop", "reqwest")
            ]
        );
        for client in HTTP_CLIENT_CRATES {
            let tree =
                format!("0earshot-desktop v0.1.0 (C:\\repo)\n1tauri v2.12.4\n2{client} v1.0.0\n");
            assert_eq!(
                evaluate_shipped("earshot-desktop", &args, &captured(&tree, true, true)).unwrap(),
                [v("earshot-desktop", client)],
                "{client}"
            );
        }
    }

    #[test]
    fn shipped_target_run_fails_closed() {
        let args = shipped_tree_args("earshot-desktop", "aarch64-pc-windows-msvc");
        let p = "earshot-desktop";
        // Cut off, failed, empty or rooted at another package: never "no violations".
        assert!(evaluate_shipped(p, &args, &captured(DESKTOP, true, false)).is_err());
        assert!(evaluate_shipped(p, &args, &captured(DESKTOP, false, true)).is_err());
        assert!(evaluate_shipped(p, &args, &captured("", true, true)).is_err());
        assert!(evaluate_shipped(p, &args, &captured(CLEAN, true, true)).is_err());
        // A second tree root means the run covered more than the package.
        let two = format!("{DESKTOP}\n0earshot-cli v0.1.0 (C:\\repo)\n1serde v1.0.228\n");
        assert!(evaluate_shipped(p, &args, &captured(&two, true, true)).is_err());
    }
}
