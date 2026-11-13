//! The owner's own meetings as a sum-eval source (T12.25a, evaluation only).
//!
//! - `cargo xtask sum-eval --store-db <earshot.db> --store-run <name> [--store-count 6]
//!   [--store-min-minutes 15] [--store-exclude <id,...>]` checks that
//!   `fixtures/private/real-meetings/<name>/` is git-ignored, then builds and runs
//!   earshot-runtime's `store_inputs` example, which copies the database there, opens the
//!   copy read-only, picks meetings by a rule that reads no text and writes their summary
//!   inputs and numbered transcripts beside it (the example's module docs). It prints counts.
//! - `cargo xtask sum-eval --source store --store-run <name> --model <m> ...` summarises
//!   those meetings (split `real`, labels `r1`, `r2`, ... from the run's `picked.json`) with
//!   any candidate, the agent CLIs included, under today's plan; results, the report and the
//!   table go to `<run>/results` (or an `--out` inside the run folder), never `target/`.
//!   Remote endpoints never see them.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use anyhow::{Context, Result, bail, ensure};
use serde_json::Value;

use super::{Options, build, build_args, executable};
use crate::proc::run_with_timeout;

/// The source name (`--source store`, `--job real:store`).
pub const SOURCE: &str = "store";

/// The split name of the owner's meetings.
pub const SPLIT: &str = "real";

/// How long `git check-ignore` may take.
const GIT_TIMEOUT: Duration = Duration::from_secs(60);

/// How long the store copy and its inputs may take.
const PREPARE_TIMEOUT: Duration = Duration::from_secs(30 * 60);

/// The run folder `fixtures/private/real-meetings/<name>` under `root`. A name is ASCII
/// letters, digits, `-`, `_` and `.`, not starting with `.`, so it can never leave the tree.
///
/// # Errors
/// Any other name.
pub fn run_dir(root: &Path, name: &str) -> Result<PathBuf> {
    ensure!(
        !name.is_empty()
            && !name.starts_with('.')
            && name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.')),
        "--store-run is a plain folder name (letters, digits, '-', '_' and '.')"
    );
    Ok(root
        .join("fixtures")
        .join("private")
        .join("real-meetings")
        .join(name))
}

/// Refuses a run folder git would track: `git check-ignore -q` must exit 0 for it.
///
/// # Errors
/// Git did not answer, or the folder is not ignored.
pub fn check_ignored(root: &Path, dir: &Path) -> Result<()> {
    let out = run_with_timeout(
        Command::new("git")
            .arg("-C")
            .arg(root)
            .args(["check-ignore", "-q"])
            .arg(dir),
        GIT_TIMEOUT,
    )
    .context("git check-ignore did not finish")?;
    ensure!(
        out.success,
        "{} is not git-ignored; the owner's meetings must never be tracked (check the \
         \"Private files\" block of .gitignore)",
        dir.display()
    );
    Ok(())
}

/// The labels of the picked meetings in `<run>/picked.json`, in order.
///
/// # Errors
/// A missing or malformed file.
pub fn labels(run: &Path) -> Result<Vec<String>> {
    let p = run.join("picked.json");
    let v: Value = serde_json::from_slice(&std::fs::read(&p).with_context(|| {
        format!(
            "{}: run `cargo xtask sum-eval --store-db <earshot.db> --store-run <name>` first",
            p.display()
        )
    })?)
    .with_context(|| p.display().to_string())?;
    let labels: Vec<String> = v["meetings"]
        .as_array()
        .with_context(|| format!("{}: no meetings", p.display()))?
        .iter()
        .map(|m| m["label"].as_str().map(str::to_owned))
        .collect::<Option<_>>()
        .with_context(|| format!("{}: a meeting without a label", p.display()))?;
    ensure!(!labels.is_empty(), "{}: no meeting was picked", p.display());
    Ok(labels)
}

/// The meetings of a store run: the run's labels narrowed to `wanted` (each must be one) and
/// cut to the first `limit`.
///
/// # Errors
/// As [`labels`], or a wanted label that is not in the run.
pub fn meetings(
    run: &Path,
    wanted: &[String],
    limit: Option<usize>,
) -> Result<Vec<(String, Vec<String>)>> {
    let all = vec![(SPLIT.to_owned(), labels(run)?)];
    Ok(super::select_meetings(all, wanted)?
        .into_iter()
        .map(|(split, list)| match limit {
            Some(n) => (split, list.into_iter().take(n).collect()),
            None => (split, list),
        })
        .collect())
}

/// The output folder of a store run: `out` (default `<run>/results`), created, which must
/// lie inside the run folder.
///
/// # Errors
/// An output outside the run folder, or a folder that cannot be created.
pub fn out_dir(run: &Path, out: Option<&Path>) -> Result<PathBuf> {
    let out = out.map_or_else(|| run.join("results"), Path::to_path_buf);
    std::fs::create_dir_all(&out).with_context(|| out.display().to_string())?;
    let (o, r) = (
        std::fs::canonicalize(&out).with_context(|| out.display().to_string())?,
        std::fs::canonicalize(run).with_context(|| run.display().to_string())?,
    );
    ensure!(
        o.starts_with(&r) && o != r,
        "--out must lie inside the run folder {} for the owner's meetings",
        run.display()
    );
    Ok(out)
}

/// The `sum_eval` arguments that point it at the run folder.
#[must_use]
pub fn example_args(run: &Path) -> Vec<OsString> {
    vec!["--store-inputs".into(), run.into()]
}

/// The arguments of the `store_inputs` example.
#[must_use]
pub fn prepare_args(db: &Path, into: &Path, opts: &Options) -> Vec<OsString> {
    let mut a: Vec<OsString> = vec![
        "--db".into(),
        db.into(),
        "--into".into(),
        into.into(),
        "--count".into(),
        opts.store_count.to_string().into(),
        "--min-minutes".into(),
        opts.store_min_minutes.to_string().into(),
    ];
    if !opts.store_exclude.is_empty() {
        a.extend(["--exclude".into(), opts.store_exclude.join(",").into()]);
    }
    a
}

/// Refuses a store preparation mixed with anything that summarises or replays.
///
/// # Errors
/// What the preparation must not be combined with.
pub fn check_prepare(opts: &Options) -> Result<()> {
    ensure!(
        opts.store_run.is_some(),
        "--store-db needs --store-run <name>"
    );
    ensure!(
        opts.models.is_empty()
            && !opts.rescore
            && !opts.stored_only()
            && !opts.replay_only
            && opts.out.is_none(),
        "--store-db only prepares the run folder: no --model, --rescore, --replay-only, \
         --export-prompts, --import-replies or --out"
    );
    ensure!(opts.store_count > 0, "--store-count must be at least 1");
    Ok(())
}

/// `--store-db`: builds `store_inputs` and fills the run folder ([`self`]).
///
/// # Errors
/// A refused option, a run folder git would track, a failed build or preparation.
pub fn prepare(root: &Path, opts: &Options) -> Result<()> {
    check_prepare(opts)?;
    let db = opts.store_db.as_deref().context("--store-db missing")?;
    let run = run_dir(root, opts.store_run.as_deref().unwrap_or_default())?;
    check_ignored(root, &run)?;
    let target_dir = crate::build::short_target_dir(
        root,
        opts.target_dir.as_deref(),
        std::env::var_os("SystemDrive"),
        None,
    );
    let msgs = build(
        root,
        &build_args(
            "earshot-runtime",
            &["--example", "store_inputs"],
            None,
            &target_dir,
        ),
        "the store_inputs example",
    )?;
    let exe = executable(&msgs, "store_inputs").context("cargo reported no store_inputs")?;
    let out = run_with_timeout(
        Command::new(&exe).args(prepare_args(db, &run, opts)),
        PREPARE_TIMEOUT,
    )
    .context("store_inputs did not finish")?;
    out.require_complete("store_inputs")?;
    // Its output is counts and the folder only (the example's rule); passed through.
    print!("{}", out.stdout);
    eprint!("{}", out.stderr);
    if !out.success {
        bail!("store_inputs failed ({:?})", out.code);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sum_eval_store_run_names_stay_inside_the_private_tree() {
        let root = Path::new("repo");
        assert_eq!(
            run_dir(root, "2026-11-13").unwrap(),
            root.join("fixtures/private/real-meetings/2026-11-13")
        );
        for bad in [
            "", ".", "..", "../x", "a/b", "a\\b", ".hidden", "c:x", "a b",
        ] {
            assert!(run_dir(root, bad).is_err(), "{bad}");
        }
    }

    fn temp_root(name: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!(
            "xtask-sum-eval-store-{name}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn sum_eval_store_labels_outputs_and_preparation_follow_the_run_folder() {
        let root = temp_root("labels");
        let run = run_dir(&root, "run-1").unwrap();
        std::fs::create_dir_all(&run).unwrap();
        assert!(labels(&run).is_err());
        std::fs::write(
            run.join("picked.json"),
            r#"{"meetings": [{"label": "r1"}, {"label": "r2"}, {"label": "r3"}]}"#,
        )
        .unwrap();
        assert_eq!(labels(&run).unwrap(), ["r1", "r2", "r3"]);
        assert_eq!(
            meetings(&run, &[], Some(2)).unwrap(),
            [("real".to_owned(), vec!["r1".to_owned(), "r2".to_owned()])]
        );
        assert_eq!(
            meetings(&run, &["r3".to_owned()], None).unwrap(),
            [("real".to_owned(), vec!["r3".to_owned()])]
        );
        assert!(meetings(&run, &["r9".to_owned()], None).is_err());
        // Results stay in the run folder.
        assert_eq!(out_dir(&run, None).unwrap(), run.join("results"));
        assert!(out_dir(&run, Some(&run.join("arm-b"))).is_ok());
        assert!(out_dir(&run, Some(&root.join("target"))).is_err());
        assert!(out_dir(&run, Some(&run)).is_err());
        // The preparation runs alone.
        let opts = Options {
            store_db: Some("e.db".into()),
            store_run: Some("run-1".into()),
            ..Options::default()
        };
        check_prepare(&opts).unwrap();
        let args = prepare_args(Path::new("e.db"), &run, &opts);
        assert!(args.iter().any(|a| a == "--count") && !args.iter().any(|a| a == "--exclude"));
        let excluded = Options {
            store_exclude: vec!["a".into(), "b".into()],
            ..opts.clone()
        };
        let args = prepare_args(Path::new("e.db"), &run, &excluded);
        assert!(args.iter().any(|a| a == "a,b"));
        for bad in [
            Options {
                models: vec!["qwen3.5-4b-q4_k_m".into()],
                ..opts.clone()
            },
            Options {
                out: Some("o".into()),
                ..opts.clone()
            },
            Options {
                store_run: None,
                ..opts.clone()
            },
            Options {
                store_count: 0,
                ..opts
            },
        ] {
            assert!(check_prepare(&bad).is_err());
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn sum_eval_store_the_private_tree_is_git_ignored_here() {
        // The repository's own rule: a store run folder is never tracked.
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
        check_ignored(root, &run_dir(root, "any-run").unwrap()).unwrap();
        assert!(check_ignored(root, &root.join("docs").join("x.md")).is_err());
    }
}
