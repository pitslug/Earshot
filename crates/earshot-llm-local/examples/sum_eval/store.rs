//! The owner's own meetings as a source (T12.25a, evaluation only): `--job real:store
//! --store-inputs <run>`, where `<run>` is a folder `cargo xtask sum-eval --store-db` filled
//! through earshot-runtime's `store_inputs` example under `fixtures/private/real-meetings/`
//! (git-ignored): `inputs/<label>.json` (the production `SummaryInput` of each picked
//! meeting, labels `r1`, `r2`, ...) and `transcripts/<label>.txt` (the numbered transcript as
//! a model reads it).
//!
//! Nothing of these meetings may leave that private tree: a store run's result file, its
//! checkpoint, judge packets and blinding key must all lie inside a
//! `fixtures/private/real-meetings/` folder ([`check_private`]), a store run is scored by
//! length counts only (there is no annotation to compare with, `score::store_score`), and
//! what the example prints for a store meeting is its label and counts. Remote endpoints,
//! prompt export and import, and the AMI meaning judge are refused for store jobs.

use std::path::{Component, Path, PathBuf};

use earshot_core::prompt::SummaryInput;

/// The source name in `--job <split>:store`.
pub const SOURCE: &str = "store";

/// The private tree every store output must lie in: `fixtures/private/real-meetings`.
const PRIVATE_TREE: [&str; 3] = ["fixtures", "private", "real-meetings"];

/// `path` resolved by the file system as far as it exists (the nearest existing ancestor
/// canonicalised, the rest appended), so `..` and links cannot leave the tree unnoticed.
fn resolved(path: &Path) -> Result<PathBuf, String> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(|e| format!("current directory: {e}"))?
            .join(path)
    };
    let mut existing = absolute.as_path();
    let mut rest = Vec::new();
    loop {
        if existing.exists() {
            break;
        }
        match (existing.parent(), existing.file_name()) {
            (Some(parent), Some(name)) => {
                rest.push(name.to_owned());
                existing = parent;
            }
            _ => return Err(format!("{}: no existing ancestor", path.display())),
        }
    }
    let mut out =
        std::fs::canonicalize(existing).map_err(|e| format!("{}: {e}", existing.display()))?;
    for part in rest.iter().rev() {
        if Path::new(part)
            .components()
            .any(|c| !matches!(c, Component::Normal(_)))
        {
            return Err(format!("{}: not a plain path", path.display()));
        }
        out.push(part);
    }
    Ok(out)
}

/// Refuses `path` (named `what` in the error) unless it lies inside a
/// `fixtures/private/real-meetings/` folder (compared ignoring ASCII case, as Windows does),
/// strictly below it.
pub fn check_private(path: &Path, what: &str) -> Result<(), String> {
    let p = resolved(path)?;
    let names: Vec<String> = p
        .components()
        .filter_map(|c| match c {
            Component::Normal(n) => Some(n.to_string_lossy().to_ascii_lowercase()),
            _ => None,
        })
        .collect();
    let inside = names
        .windows(PRIVATE_TREE.len())
        .enumerate()
        .any(|(i, w)| w == PRIVATE_TREE && i + PRIVATE_TREE.len() < names.len());
    if inside {
        Ok(())
    } else {
        Err(format!(
            "{what} must lie inside a fixtures/private/real-meetings/ folder for store \
             meetings (it is {})",
            p.display()
        ))
    }
}

/// The summary input of `label` in run folder `run`.
pub fn input(run: &Path, label: &str) -> Result<(SummaryInput, usize), String> {
    let p = run.join("inputs").join(format!("{label}.json"));
    let text = std::fs::read_to_string(&p).map_err(|e| {
        format!(
            "{}: {e} (written by `cargo xtask sum-eval --store-db`)",
            p.display()
        )
    })?;
    let input: SummaryInput =
        serde_json::from_str(&text).map_err(|e| format!("{}: {e}", p.display()))?;
    let words = input
        .segments
        .iter()
        .map(|s| s.text.split_whitespace().count())
        .sum();
    Ok((input, words))
}

/// The numbered transcript of `label` in run folder `run` (for the judges).
pub fn transcript(run: &Path, label: &str) -> Result<String, String> {
    let p = run.join("transcripts").join(format!("{label}.txt"));
    std::fs::read_to_string(&p).map_err(|e| format!("{}: {e}", p.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_root(name: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!(
            "earshot-sum-eval-store-{name}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn sum_eval_store_outputs_must_stay_in_the_private_tree() {
        let root = temp_root("private");
        let run = root
            .join("fixtures")
            .join("private")
            .join("real-meetings")
            .join("run-1");
        std::fs::create_dir_all(&run).unwrap();
        // Inside, existing or not yet.
        assert!(check_private(&run.join("results").join("a.json"), "--out").is_ok());
        assert!(check_private(&run.join("packets"), "--judge-packets").is_ok());
        // Case does not matter.
        let upper = root
            .join("Fixtures")
            .join("PRIVATE")
            .join("Real-Meetings")
            .join("x.json");
        assert!(check_private(&upper, "--out").is_ok());
        // Outside, or the tree folder itself, or walking out with `..`.
        assert!(check_private(&root.join("target").join("a.json"), "--out").is_err());
        assert!(
            check_private(
                &root.join("fixtures").join("private").join("real-meetings"),
                "--out"
            )
            .is_err()
        );
        let escape = run
            .join("..")
            .join("..")
            .join("..")
            .join("..")
            .join("a.json");
        assert!(check_private(&escape, "--out").is_err());
        assert!(
            check_private(
                &root
                    .join("fixtures")
                    .join("private")
                    .join("other")
                    .join("a.json"),
                "--out"
            )
            .is_err()
        );
        let _ = std::fs::remove_dir_all(&root);
    }
}
