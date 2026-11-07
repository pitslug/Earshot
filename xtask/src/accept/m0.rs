//! `accept m0`: structural checks on `acceptance/thresholds.toml` (T0.2).
//!
//! No threshold rows belong to m0, so this runner returns only checks.

use anyhow::Result;

use super::{Check, Ctx, RunOutput};
use crate::thresholds::{self, EXPECTED_IDS};

pub(super) fn run(ctx: &Ctx) -> Result<RunOutput> {
    let path = thresholds::path_in(ctx.root);
    let mut checks = Vec::new();

    let parsed = std::fs::read_to_string(&path)
        .map_err(|e| vec![format!("cannot read {}: {e}", path.display())])
        .and_then(|text| thresholds::parse(&text));
    match &parsed {
        Ok(t) => checks.push(Check::new(
            "thresholds.toml parses and every entry is well-formed",
            true,
            format!("{} entries", t.len()),
        )),
        Err(errs) => checks.push(Check::new(
            "thresholds.toml parses and every entry is well-formed",
            false,
            errs.join("; "),
        )),
    }

    let ids_problems = match &parsed {
        Ok(t) => thresholds::check_ids(t, EXPECTED_IDS),
        Err(_) => vec!["not checked: file did not parse".into()],
    };
    checks.push(Check::new(
        "threshold ids equal the PLAN.md 4.2 id list",
        ids_problems.is_empty(),
        if ids_problems.is_empty() {
            format!("{} ids match", EXPECTED_IDS.len())
        } else {
            ids_problems.join("; ")
        },
    ));

    let plan_path = ctx.root.join("PLAN.md");
    let plan_md = std::fs::read_to_string(&plan_path)
        .map_err(|e| format!("cannot read {}: {e}", plan_path.display()));

    const IDS_NAME: &str = "PLAN.md 4.2 table matches the id list";
    checks.push(match &plan_md {
        Ok(md) => {
            let plan_ids = thresholds::plan_table_ids(md);
            let matches = plan_ids
                .iter()
                .map(String::as_str)
                .eq(EXPECTED_IDS.iter().copied());
            let detail = if matches {
                format!("{} rows match", plan_ids.len())
            } else {
                format!(
                    "PLAN.md 4.2 has {} rows {:?}, list has {}",
                    plan_ids.len(),
                    plan_ids,
                    EXPECTED_IDS.len()
                )
            };
            Check::new(IDS_NAME, matches, detail)
        }
        Err(e) => Check::new(IDS_NAME, false, e.clone()),
    });

    const ROWS_NAME: &str =
        "thresholds.toml matches PLAN.md 4.2 rows (milestone, op, value, unit, provisional)";
    let problems = match (&plan_md, &parsed) {
        (Err(e), _) => vec![e.clone()],
        (Ok(_), Err(_)) => vec!["not checked: thresholds.toml did not parse".into()],
        (Ok(md), Ok(t)) => match thresholds::plan_rows(md) {
            Ok(rows) => thresholds::compare_with_plan(t, &rows),
            Err(errs) => errs,
        },
    };
    checks.push(Check::new(
        ROWS_NAME,
        problems.is_empty(),
        if problems.is_empty() {
            "every row matches".to_string()
        } else {
            problems.join("; ")
        },
    ));

    Ok(RunOutput {
        measurements: Vec::new(),
        checks,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn m0_passes_on_this_repo() {
        let root = crate::repo_root();
        let out = run(&Ctx {
            root: &root,
            milestone: "m0",
            backend: None,
        })
        .unwrap();
        assert!(out.measurements.is_empty());
        assert_eq!(out.checks.len(), 4);
        for c in &out.checks {
            assert!(c.pass, "{c:?}");
        }
    }

    #[test]
    fn m0_fails_when_file_missing() {
        let dir = std::env::temp_dir().join(format!("earshot-m0-empty-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let out = run(&Ctx {
            root: &dir,
            milestone: "m0",
            backend: None,
        })
        .unwrap();
        assert!(out.checks.iter().all(|c| !c.pass), "{:?}", out.checks);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
