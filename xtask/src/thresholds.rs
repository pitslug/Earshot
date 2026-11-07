//! Shared parser for `acceptance/thresholds.toml` (PLAN.md section 4.2).
//!
//! Used by `cargo xtask accept` and by `tests/thresholds.rs`. It checks shape (ids, ops,
//! units, value presence) and deliberately does not pin numeric values, so tightening a
//! threshold never needs a code change. Thresholds are only ever read from the file.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};
use serde::Serialize;

/// Every threshold id from the table in PLAN.md section 4.2, in table order.
///
/// Adding or removing a row in PLAN.md 4.2 requires changing `acceptance/thresholds.toml`
/// and this list in the same commit.
pub const EXPECTED_IDS: &[&str] = &[
    "aec.erle_farend",
    "aec.erle_linear",
    "aec.convergence",
    "aec.path_change",
    "aec.doubletalk_sisdr",
    "aec.passthrough",
    "aec.robustness",
    "aec.rtf",
    "asr.wer_smoke_base",
    "asr.wer_ls_base",
    "asr.stream_determinism",
    "asr.wer_ls_default",
    "asr.wer_ami_ihm",
    "asr.wer_aec_delta",
    "asr.echo_leak",
    "dia.der_ami_ihm",
    "dia.der_ami_sdm",
    "dia.count_error",
    "dia.count_error_large",
    "vp.top1",
    "vp.false_accept",
    "sum.schema_replay",
    "sum.grounding",
    "exp.golden",
    "exp.idempotent",
    "e2e.replay",
    "asr.wer_ami_sdm",
    "sync.no_audio",
    "sync.convergence",
];

/// Allowed `op` strings.
pub const OPS: &[&str] = &[">=", "<=", "==", "pass"];
/// Allowed `unit` strings.
pub const UNITS: &[&str] = &["dB", "s", "%", "points", "ratio", "count", ""];
/// Units a PLAN.md 4.2 cell without a unit (`<= 0.02`, `<= 1`, `0`) may map to.
pub const DIMENSIONLESS_UNITS: &[&str] = &["ratio", "count"];
/// Allowed milestone names.
pub const MILESTONES: &[&str] = &[
    "m0", "m1", "m2", "m3", "m4", "m5", "m6", "m7", "m8", "m9", "m10", "m11",
];

const ALLOWED_KEYS: &[&str] = &["milestone", "metric", "op", "value", "unit", "provisional"];

/// Comparison operator of a threshold.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum Op {
    /// Measured value must be greater than or equal to the threshold.
    #[serde(rename = ">=")]
    Ge,
    /// Measured value must be less than or equal to the threshold.
    #[serde(rename = "<=")]
    Le,
    /// Measured value must equal the threshold.
    #[serde(rename = "==")]
    Eq,
    /// Boolean check with no value.
    #[serde(rename = "pass")]
    Pass,
}

impl Op {
    /// Parses an `op` string from the file.
    pub fn parse(s: &str) -> Option<Op> {
        match s {
            ">=" => Some(Op::Ge),
            "<=" => Some(Op::Le),
            "==" => Some(Op::Eq),
            "pass" => Some(Op::Pass),
            _ => None,
        }
    }

    /// The `op` string as written in the file.
    pub fn as_str(self) -> &'static str {
        match self {
            Op::Ge => ">=",
            Op::Le => "<=",
            Op::Eq => "==",
            Op::Pass => "pass",
        }
    }
}

/// One `[thresholds."<id>"]` entry.
#[derive(Debug, Clone, PartialEq)]
pub struct Threshold {
    /// Threshold id, for example `aec.erle_farend`.
    pub id: String,
    /// Milestone the threshold belongs to (`m0`..`m11`).
    pub milestone: String,
    /// Human description from PLAN.md 4.2.
    pub metric: String,
    /// Comparison operator.
    pub op: Op,
    /// Threshold value (`None` exactly when `op` is `pass`).
    pub value: Option<f64>,
    /// Unit string (`""` for `pass`).
    pub unit: String,
    /// Whether PLAN.md marks the value provisional.
    pub provisional: bool,
}

/// Path of the thresholds file under a repo root.
pub fn path_in(root: &Path) -> PathBuf {
    root.join("acceptance").join("thresholds.toml")
}

/// Parses and validates thresholds TOML text. Returns every problem found, not just the first.
/// Entries are returned sorted by id.
pub fn parse(text: &str) -> Result<Vec<Threshold>, Vec<String>> {
    let doc: toml::Table = text
        .parse()
        .map_err(|e| vec![format!("not valid TOML: {e}")])?;
    let table = doc
        .get("thresholds")
        .and_then(|v| v.as_table())
        .ok_or_else(|| vec!["missing [thresholds] table".to_string()])?;

    let mut out = Vec::new();
    let mut errors = Vec::new();
    for (id, entry) in table {
        match parse_entry(id, entry) {
            Ok(t) => out.push(t),
            Err(mut e) => errors.append(&mut e),
        }
    }
    if errors.is_empty() {
        out.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(out)
    } else {
        Err(errors)
    }
}

fn parse_entry(id: &str, entry: &toml::Value) -> Result<Threshold, Vec<String>> {
    let Some(e) = entry.as_table() else {
        return Err(vec![format!("{id}: not a table")]);
    };
    let mut errs = Vec::new();
    for key in e.keys() {
        if !ALLOWED_KEYS.contains(&key.as_str()) {
            errs.push(format!("{id}: unknown key {key}"));
        }
    }

    let milestone = e.get("milestone").and_then(|v| v.as_str());
    if !milestone.is_some_and(|m| MILESTONES.contains(&m)) {
        errs.push(format!("{id}: bad milestone {milestone:?}"));
    }
    let metric = e.get("metric").and_then(|v| v.as_str()).unwrap_or("");
    if metric.is_empty() {
        errs.push(format!("{id}: empty metric"));
    }
    let provisional = e.get("provisional").and_then(|v| v.as_bool());
    if provisional.is_none() {
        errs.push(format!("{id}: provisional must be a bool"));
    }

    let op_str = e.get("op").and_then(|v| v.as_str()).unwrap_or("");
    let op = Op::parse(op_str);
    if op.is_none() {
        errs.push(format!("{id}: bad op {op_str:?}"));
    }
    let unit = e.get("unit").and_then(|v| v.as_str());
    let raw_value = e.get("value");

    let mut value = None;
    if op == Some(Op::Pass) {
        if raw_value.is_some() {
            errs.push(format!("{id}: op \"pass\" must not have a value"));
        }
        if !unit.is_none_or(|u| UNITS.contains(&u)) {
            errs.push(format!("{id}: bad unit {unit:?}"));
        }
    } else if op.is_some() {
        match raw_value {
            None => errs.push(format!("{id}: op {op_str} needs a value")),
            Some(v) => match v.as_float().or_else(|| v.as_integer().map(|i| i as f64)) {
                None => errs.push(format!("{id}: value must be a number")),
                Some(n) if !(n.is_finite() && n >= 0.0) => {
                    errs.push(format!("{id}: value {n} out of range"));
                }
                Some(n) => {
                    if unit == Some("%") && n > 100.0 {
                        errs.push(format!("{id}: percent {n} above 100"));
                    }
                    value = Some(n);
                }
            },
        }
        if !unit.is_some_and(|u| UNITS.contains(&u)) {
            errs.push(format!("{id}: bad unit {unit:?}"));
        }
    }

    if !errs.is_empty() {
        return Err(errs);
    }
    Ok(Threshold {
        id: id.to_string(),
        milestone: milestone.unwrap_or_default().to_string(),
        metric: metric.to_string(),
        op: op.expect("checked above"),
        value,
        unit: unit.unwrap_or("").to_string(),
        provisional: provisional.unwrap_or_default(),
    })
}

/// Reads and parses a thresholds file.
pub fn load(path: &Path) -> Result<Vec<Threshold>> {
    let text =
        std::fs::read_to_string(path).with_context(|| format!("cannot read {}", path.display()))?;
    parse(&text).map_err(|errs| anyhow!("{} is invalid:\n  {}", path.display(), errs.join("\n  ")))
}

/// Compares the parsed ids against an expected list. Returns one message per problem
/// (count mismatch, missing id, unexpected id); empty means they match exactly.
pub fn check_ids(thresholds: &[Threshold], expected: &[&str]) -> Vec<String> {
    let mut problems = Vec::new();
    if thresholds.len() != expected.len() {
        problems.push(format!(
            "entry count {} differs from expected count {}",
            thresholds.len(),
            expected.len()
        ));
    }
    for id in expected {
        if !thresholds.iter().any(|t| t.id == *id) {
            problems.push(format!("missing threshold {id}"));
        }
    }
    for t in thresholds {
        if !expected.contains(&t.id.as_str()) {
            problems.push(format!("unexpected threshold {}", t.id));
        }
    }
    problems
}

/// Cells of every data row of the table in PLAN.md section 4.2 (header and separator
/// rows skipped).
fn plan_table_cells(plan_md: &str) -> Vec<Vec<String>> {
    let mut rows = Vec::new();
    let mut in_section = false;
    for line in plan_md.lines() {
        let line = line.trim();
        if line.starts_with('#') {
            if in_section {
                break;
            }
            in_section = line.starts_with("### 4.2 ");
            continue;
        }
        if !in_section || !line.starts_with('|') {
            continue;
        }
        let cells: Vec<String> = line
            .trim_matches('|')
            .split('|')
            .map(|c| c.trim().to_string())
            .collect();
        let first = cells.first().map(String::as_str).unwrap_or("");
        if first.is_empty() || first == "ID" || first.starts_with('-') {
            continue;
        }
        rows.push(cells);
    }
    rows
}

/// Extracts the ids (first column) of the table in PLAN.md section 4.2.
pub fn plan_table_ids(plan_md: &str) -> Vec<String> {
    plan_table_cells(plan_md)
        .into_iter()
        .map(|mut c| c.swap_remove(0))
        .collect()
}

/// One row of the PLAN.md 4.2 table, parsed.
#[derive(Debug, Clone, PartialEq)]
pub struct PlanRow {
    /// Threshold id (column 1).
    pub id: String,
    /// Milestone, lower-cased (`M1` -> `m1`).
    pub milestone: String,
    /// Operator (`100%` and `0` without an operator mean `==`).
    pub op: Op,
    /// Numeric value (`None` for `pass`).
    pub value: Option<f64>,
    /// Unit when the cell states one (`None` for a bare number, `""` for `pass`).
    pub unit: Option<String>,
    /// Whether the cell ends with `(provisional)`.
    pub provisional: bool,
}

/// Parses a Threshold cell such as `>= 25 dB`, `<= 6.0%`, `100%`, `0`, `pass` or
/// `<= 22% (provisional)` into `(op, value, unit, provisional)`.
pub fn parse_threshold_cell(cell: &str) -> Result<(Op, Option<f64>, Option<String>, bool), String> {
    let mut rest = cell.trim();
    let provisional = match rest.strip_suffix("(provisional)") {
        Some(r) => {
            rest = r.trim_end();
            true
        }
        None => false,
    };
    if rest == "pass" {
        return Ok((Op::Pass, None, Some(String::new()), provisional));
    }
    let op = if let Some(r) = rest.strip_prefix(">=") {
        rest = r.trim_start();
        Op::Ge
    } else if let Some(r) = rest.strip_prefix("<=") {
        rest = r.trim_start();
        Op::Le
    } else {
        Op::Eq
    };
    let num_len = rest
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or(rest.len());
    let value: f64 = rest[..num_len]
        .parse()
        .map_err(|_| format!("cannot read a number in threshold {cell:?}"))?;
    let unit = match rest[num_len..].trim() {
        "" => None,
        u @ ("dB" | "s" | "%" | "points") => Some(u.to_string()),
        u => return Err(format!("unknown unit {u:?} in threshold {cell:?}")),
    };
    Ok((op, Some(value), unit, provisional))
}

/// Parses every row of the PLAN.md 4.2 table.
pub fn plan_rows(plan_md: &str) -> Result<Vec<PlanRow>, Vec<String>> {
    let mut rows = Vec::new();
    let mut errors = Vec::new();
    for cells in plan_table_cells(plan_md) {
        let id = cells[0].clone();
        if cells.len() != 4 {
            errors.push(format!(
                "{id}: PLAN.md 4.2 row has {} cells, expected 4",
                cells.len()
            ));
            continue;
        }
        let milestone = cells[1].to_lowercase();
        if !MILESTONES.contains(&milestone.as_str()) {
            errors.push(format!("{id}: bad milestone {:?} in PLAN.md 4.2", cells[1]));
            continue;
        }
        match parse_threshold_cell(&cells[3]) {
            Ok((op, value, unit, provisional)) => rows.push(PlanRow {
                id,
                milestone,
                op,
                value,
                unit,
                provisional,
            }),
            Err(e) => errors.push(format!("{id}: {e}")),
        }
    }
    if errors.is_empty() {
        Ok(rows)
    } else {
        Err(errors)
    }
}

/// Compares thresholds.toml entries with the PLAN.md 4.2 rows (milestone, op, value,
/// unit where the table states one, provisional). Returns one message per difference.
pub fn compare_with_plan(thresholds: &[Threshold], rows: &[PlanRow]) -> Vec<String> {
    let mut problems = Vec::new();
    for row in rows {
        let id = &row.id;
        let Some(t) = thresholds.iter().find(|t| &t.id == id) else {
            problems.push(format!("{id}: in PLAN.md 4.2 but not in thresholds.toml"));
            continue;
        };
        if t.milestone != row.milestone {
            problems.push(format!(
                "{id}: milestone plan {}, toml {}",
                row.milestone, t.milestone
            ));
        }
        if t.op != row.op {
            problems.push(format!(
                "{id}: op plan {}, toml {}",
                row.op.as_str(),
                t.op.as_str()
            ));
        }
        let same_value = match (row.value, t.value) {
            (None, None) => true,
            (Some(a), Some(b)) => (a - b).abs() <= 1e-12,
            _ => false,
        };
        if !same_value {
            problems.push(format!(
                "{id}: value plan {:?}, toml {:?}",
                row.value, t.value
            ));
        }
        match &row.unit {
            Some(u) if *u != t.unit => {
                problems.push(format!("{id}: unit plan {u:?}, toml {:?}", t.unit));
            }
            // A bare number in PLAN.md is dimensionless, so the TOML unit must be one of
            // the dimensionless units.
            None if !DIMENSIONLESS_UNITS.contains(&t.unit.as_str()) => {
                problems.push(format!(
                    "{id}: unit plan is a bare number (one of {DIMENSIONLESS_UNITS:?}), toml {:?}",
                    t.unit
                ));
            }
            _ => {}
        }
        if t.provisional != row.provisional {
            problems.push(format!(
                "{id}: provisional plan {}, toml {}",
                row.provisional, t.provisional
            ));
        }
    }
    for t in thresholds {
        if !rows.iter().any(|r| r.id == t.id) {
            problems.push(format!(
                "{}: in thresholds.toml but not in PLAN.md 4.2",
                t.id
            ));
        }
    }
    problems
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(id: &str, body: &str) -> String {
        format!("[thresholds.\"{id}\"]\n{body}\n")
    }

    const GOOD_GE: &str = "milestone = \"m1\"\nmetric = \"m\"\nop = \">=\"\nvalue = 25.0\nunit = \"dB\"\nprovisional = false";
    const GOOD_PASS: &str =
        "milestone = \"m5\"\nmetric = \"m\"\nop = \"pass\"\nunit = \"\"\nprovisional = false";

    fn errs(text: &str) -> Vec<String> {
        parse(text).expect_err("expected parse to fail")
    }

    #[test]
    fn parses_good_entries() {
        let text = entry("b.ge", GOOD_GE) + &entry("a.pass", GOOD_PASS);
        let t = parse(&text).unwrap();
        assert_eq!(t.len(), 2);
        assert_eq!(t[0].id, "a.pass");
        assert_eq!(t[0].op, Op::Pass);
        assert_eq!(t[0].value, None);
        assert_eq!(t[1].op, Op::Ge);
        assert_eq!(t[1].value, Some(25.0));
        assert_eq!(t[1].unit, "dB");
        assert_eq!(t[1].milestone, "m1");
    }

    #[test]
    fn integer_value_is_accepted() {
        let body = GOOD_GE.replace("25.0", "1");
        assert_eq!(parse(&entry("x", &body)).unwrap()[0].value, Some(1.0));
    }

    #[test]
    fn rejects_invalid_toml() {
        assert!(errs("this is = = not toml")[0].contains("TOML"));
    }

    #[test]
    fn rejects_missing_table() {
        assert!(errs("x = 1")[0].contains("[thresholds]"));
    }

    #[test]
    fn rejects_unknown_key() {
        let e = errs(&entry("x", &format!("{GOOD_GE}\nextra = 1")));
        assert!(e.iter().any(|m| m.contains("unknown key extra")), "{e:?}");
    }

    #[test]
    fn rejects_bad_milestone() {
        let e = errs(&entry("x", &GOOD_GE.replace("\"m1\"", "\"m99\"")));
        assert!(e.iter().any(|m| m.contains("milestone")), "{e:?}");
    }

    #[test]
    fn rejects_empty_metric() {
        let e = errs(&entry("x", &GOOD_GE.replace("\"m\"", "\"\"")));
        assert!(e.iter().any(|m| m.contains("metric")), "{e:?}");
    }

    #[test]
    fn rejects_non_bool_provisional() {
        let e = errs(&entry("x", &GOOD_GE.replace("false", "\"no\"")));
        assert!(e.iter().any(|m| m.contains("provisional")), "{e:?}");
    }

    #[test]
    fn rejects_bad_op() {
        let e = errs(&entry("x", &GOOD_GE.replace("\">=\"", "\">\"")));
        assert!(e.iter().any(|m| m.contains("op")), "{e:?}");
    }

    #[test]
    fn rejects_pass_with_value() {
        let e = errs(&entry("x", &format!("{GOOD_PASS}\nvalue = 1.0")));
        assert!(
            e.iter().any(|m| m.contains("must not have a value")),
            "{e:?}"
        );
    }

    #[test]
    fn rejects_comparison_without_value() {
        let e = errs(&entry("x", &GOOD_GE.replace("value = 25.0\n", "")));
        assert!(e.iter().any(|m| m.contains("needs a value")), "{e:?}");
    }

    #[test]
    fn rejects_non_numeric_value() {
        let e = errs(&entry("x", &GOOD_GE.replace("25.0", "\"25\"")));
        assert!(e.iter().any(|m| m.contains("number")), "{e:?}");
    }

    #[test]
    fn rejects_negative_or_non_finite_value() {
        let e = errs(&entry("x", &GOOD_GE.replace("25.0", "-1.0")));
        assert!(e.iter().any(|m| m.contains("out of range")), "{e:?}");
        let e = errs(&entry("x", &GOOD_GE.replace("25.0", "nan")));
        assert!(e.iter().any(|m| m.contains("out of range")), "{e:?}");
    }

    #[test]
    fn rejects_bad_unit() {
        let e = errs(&entry("x", &GOOD_GE.replace("\"dB\"", "\"dBA\"")));
        assert!(e.iter().any(|m| m.contains("unit")), "{e:?}");
    }

    #[test]
    fn rejects_percent_above_100() {
        let body = GOOD_GE.replace("25.0", "101.0").replace("\"dB\"", "\"%\"");
        let e = errs(&entry("x", &body));
        assert!(e.iter().any(|m| m.contains("above 100")), "{e:?}");
    }

    #[test]
    fn reports_every_problem() {
        let text = entry("a", &GOOD_GE.replace("\">=\"", "\"?\""))
            + &entry("b", &GOOD_GE.replace("\"dB\"", "\"x\""));
        assert_eq!(errs(&text).len(), 2);
    }

    #[test]
    fn check_ids_detects_missing_extra_and_count() {
        let text = entry("a", GOOD_GE) + &entry("c", GOOD_GE);
        let t = parse(&text).unwrap();
        assert!(check_ids(&t, &["a", "c"]).is_empty());
        let problems = check_ids(&t, &["a", "b"]);
        assert!(problems.iter().any(|p| p.contains("missing threshold b")));
        assert!(
            problems
                .iter()
                .any(|p| p.contains("unexpected threshold c"))
        );
        let problems = check_ids(&t, &["a"]);
        assert!(problems.iter().any(|p| p.contains("count")));
    }

    #[test]
    fn plan_table_ids_reads_section_4_2_only() {
        let md = "\
### 4.1 Fixtures

| Set | Source |
|---|---|
| smoke | x |

### 4.2 Initial thresholds (`acceptance/thresholds.toml`)

Some prose.

| ID | Milestone | Metric | Threshold |
|---|---|---|---|
| aec.erle_farend | M1 | ERLE | >= 25 dB |
| sync.convergence | M11 | converge | pass |

Hardware numbers are recorded at gates.

---

## 5. Gates

| ID | x |
|---|---|
| GATE-00 | y |
";
        assert_eq!(
            plan_table_ids(md),
            vec!["aec.erle_farend", "sync.convergence"]
        );
    }

    #[test]
    fn threshold_cells_parse() {
        let c = |s: &str| parse_threshold_cell(s).unwrap();
        let some = |u: &str| Some(u.to_string());
        assert_eq!(c(">= 25 dB"), (Op::Ge, Some(25.0), some("dB"), false));
        assert_eq!(c("<= 1.5 s"), (Op::Le, Some(1.5), some("s"), false));
        assert_eq!(c("<= 0.02"), (Op::Le, Some(0.02), None, false));
        assert_eq!(c("<= 10%"), (Op::Le, Some(10.0), some("%"), false));
        assert_eq!(c("<= 6.0%"), (Op::Le, Some(6.0), some("%"), false));
        assert_eq!(
            c("<= 2.0 points"),
            (Op::Le, Some(2.0), some("points"), false)
        );
        assert_eq!(c(">= 95%"), (Op::Ge, Some(95.0), some("%"), false));
        assert_eq!(c("100%"), (Op::Eq, Some(100.0), some("%"), false));
        assert_eq!(c("0"), (Op::Eq, Some(0.0), None, false));
        assert_eq!(c("<= 1"), (Op::Le, Some(1.0), None, false));
        assert_eq!(c("pass"), (Op::Pass, None, some(""), false));
        assert_eq!(
            c(">= 10 dB (provisional)"),
            (Op::Ge, Some(10.0), some("dB"), true)
        );
        assert_eq!(
            c("<= 22% (provisional)"),
            (Op::Le, Some(22.0), some("%"), true)
        );
        assert!(parse_threshold_cell("about 5").is_err());
        assert!(parse_threshold_cell(">= 5 furlongs").is_err());
        assert!(parse_threshold_cell("").is_err());
    }

    #[test]
    fn plan_rows_parse_milestone_and_threshold() {
        let md = "### 4.2 T

| ID | Milestone | Metric | Threshold |
|---|---|---|---|
| a.x | M1 | some metric | >= 25 dB |
| b.y | M10 | other | pass |
";
        let rows = plan_rows(md).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].id, "a.x");
        assert_eq!(rows[0].milestone, "m1");
        assert_eq!(rows[0].op, Op::Ge);
        assert_eq!(rows[1].milestone, "m10");
        assert_eq!(rows[1].op, Op::Pass);
        let bad = "### 4.2 T

| ID | Milestone | Metric | Threshold |
|---|---|---|---|
| a.x | M99 | m | >= 1 dB |
";
        assert!(plan_rows(bad).is_err());
    }

    fn repo_text(rel: &str) -> String {
        std::fs::read_to_string(crate::repo_root().join(rel)).unwrap()
    }

    /// Replaces the first `from` after the `[thresholds."<id>"]` header.
    fn mutate(toml: &str, id: &str, from: &str, to: &str) -> String {
        let header = format!("[thresholds.\"{id}\"]");
        let start = toml.find(&header).expect("id present");
        let at = start + toml[start..].find(from).expect("field present");
        format!("{}{}{}", &toml[..at], to, &toml[at + from.len()..])
    }

    fn plan_problems(toml: &str) -> Vec<String> {
        let rows = plan_rows(&repo_text("PLAN.md")).unwrap();
        compare_with_plan(&parse(toml).unwrap(), &rows)
    }

    #[test]
    fn real_repo_matches_plan() {
        let p = plan_problems(&repo_text("acceptance/thresholds.toml"));
        assert!(p.is_empty(), "{p:#?}");
    }

    #[test]
    fn mutations_are_caught() {
        let real = repo_text("acceptance/thresholds.toml");
        let cases = [
            (
                "aec.erle_farend",
                "milestone = \"m1\"",
                "milestone = \"m2\"",
                "milestone",
            ),
            ("aec.convergence", "op = \"<=\"", "op = \">=\"", "op"),
            ("aec.erle_farend", "value = 25.0", "value = 20.0", "value"),
            (
                "aec.doubletalk_sisdr",
                "provisional = true",
                "provisional = false",
                "provisional",
            ),
            (
                "asr.wer_ls_base",
                "unit = \"%\"",
                "unit = \"points\"",
                "unit",
            ),
            // Unit-less PLAN cells (`<= 0.02`, `<= 1`, `0`) still pin a dimensionless unit.
            ("aec.rtf", "unit = \"ratio\"", "unit = \"dB\"", "unit"),
            (
                "dia.count_error",
                "unit = \"count\"",
                "unit = \"s\"",
                "unit",
            ),
            ("sum.grounding", "unit = \"count\"", "unit = \"%\"", "unit"),
            ("sum.grounding", "unit = \"count\"", "unit = \"\"", "unit"),
        ];
        for (id, from, to, field) in cases {
            let p = plan_problems(&mutate(&real, id, from, to));
            assert!(
                p.iter().any(|m| m.starts_with(&format!("{id}: {field}"))),
                "{id} {field}: {p:#?}"
            );
        }
    }

    #[test]
    fn missing_and_extra_rows_are_caught() {
        let t = parse(&(entry("a", GOOD_GE))).unwrap();
        let row = |id: &str| PlanRow {
            id: id.into(),
            milestone: "m1".into(),
            op: Op::Ge,
            value: Some(25.0),
            unit: Some("dB".into()),
            provisional: false,
        };
        assert!(compare_with_plan(&t, &[row("a")]).is_empty());
        let p = compare_with_plan(&t, &[row("a"), row("b")]);
        assert!(p.iter().any(|m| m.starts_with("b: ")), "{p:?}");
        let p = compare_with_plan(&t, &[]);
        assert!(p.iter().any(|m| m.starts_with("a: ")), "{p:?}");
    }
}
