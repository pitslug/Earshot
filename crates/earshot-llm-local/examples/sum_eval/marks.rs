//! Real-item and mis-heard marks of blinded judge packets (T12.87, docs/summary-eval.md):
//! `sum_eval --count-marks <packet dir> --meetings <split>=<list> --blind-key <key> --out
//! <file>.json` counts, per model, what the judges wrote in `real.<label>.json` beside each
//! `summary.<label>.json`.
//!
//! A marks file is `{"meeting", "label", "items": [...]}` with exactly one entry for every
//! decision (`S-D<n>`) and action item (`S-A<n>`) of its summary packet:
//!
//! - `id`, `kind` (`decision` or `action`, matching the id);
//! - `mark`: `caught` (with `real`, the fixed real-item ids it catches: `D<n>` decisions,
//!   `A<n>` actions), `not_listed`, `made_up` or `duplicate`;
//! - `owner_right` (a caught action: true or false; otherwise null), `also_made_up` (a caught
//!   item that also states a false part);
//! - `misheard`: null, or `{"written", "said"}` when the item repeats a mis-heard word of the
//!   Earshot transcript that the reference transcript shows was something else;
//!   `misheard_right`: the same shape when the item has the right word where the Earshot
//!   transcript had it wrong (informational);
//! - `reason`: one non-empty line.
//!
//! Counts: real decisions and actions caught (each real id once per summary), made up
//! (`made_up` marks plus `also_made_up`), duplicates, not listed, owners right, mis-heard and
//! mis-heard-right items. The packet set is checked as `--import-verdicts` checks it.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use serde_json::{Value, json};

use crate::judge;

/// The marks a judge may give one summary item.
pub const MARKS: [&str; 4] = ["caught", "not_listed", "made_up", "duplicate"];

/// The counts of one marks file, checked against its summary packet.
///
/// # Errors
/// A missing, repeated or unknown item, a kind that does not match its id, a mark outside
/// [`MARKS`], `real` given for anything but a caught item (or missing for one), a malformed
/// mis-heard entry or an empty or multi-line reason.
pub fn check_marks(
    summary: &Value,
    marks: &Value,
    meeting: &str,
    label: &str,
) -> Result<Value, String> {
    let at = |what: String| format!("{meeting} {label}: {what}");
    if marks["meeting"] != meeting || marks["label"] != label {
        return Err(at("meeting or label does not match the file name".into()));
    }
    let mut want: BTreeMap<String, &str> = BTreeMap::new();
    for (list, kind) in [("decisions", "decision"), ("action_items", "action")] {
        for item in summary[list].as_array().into_iter().flatten() {
            let id = item["id"]
                .as_str()
                .ok_or_else(|| at("summary item without id".into()))?;
            want.insert(id.to_owned(), kind);
        }
    }
    let items = marks["items"]
        .as_array()
        .ok_or_else(|| at("no items list".into()))?;
    let mut seen = BTreeSet::new();
    let mut caught_d = BTreeSet::new();
    let mut caught_a = BTreeSet::new();
    let (mut made_up, mut duplicates, mut not_listed, mut owner_right) = (0u64, 0u64, 0u64, 0u64);
    let (mut misheard, mut misheard_right) = (0u64, 0u64);
    for item in items {
        let id = item["id"]
            .as_str()
            .ok_or_else(|| at("an item without id".into()))?;
        let kind = *want
            .get(id)
            .ok_or_else(|| at(format!("{id} is not in the summary packet")))?;
        if !seen.insert(id.to_owned()) {
            return Err(at(format!("{id} is marked twice")));
        }
        if item["kind"] != kind {
            return Err(at(format!("{id}: kind must be {kind}")));
        }
        let mark = item["mark"].as_str().unwrap_or_default();
        if !MARKS.contains(&mark) {
            return Err(at(format!("{id}: mark {mark:?} is not one of {MARKS:?}")));
        }
        let real: Vec<&str> = item["real"]
            .as_array()
            .map(|r| r.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default();
        if (mark == "caught") == real.is_empty() {
            return Err(at(format!(
                "{id}: `real` lists the caught real items, only for caught"
            )));
        }
        for r in &real {
            let n = r
                .get(1..)
                .filter(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()));
            match (r.as_bytes().first(), n) {
                (Some(b'D'), Some(_)) => caught_d.insert((*r).to_owned()),
                (Some(b'A'), Some(_)) => caught_a.insert((*r).to_owned()),
                _ => return Err(at(format!("{id}: real item {r:?} is not D<n> or A<n>"))),
            };
        }
        match mark {
            "made_up" => made_up += 1,
            "duplicate" => duplicates += 1,
            "not_listed" => not_listed += 1,
            _ => {}
        }
        if item["also_made_up"] == true {
            if mark != "caught" {
                return Err(at(format!("{id}: also_made_up is for caught items")));
            }
            made_up += 1;
        }
        if item["owner_right"] == true {
            if mark != "caught" || kind != "action" {
                return Err(at(format!("{id}: owner_right is for caught actions")));
            }
            owner_right += 1;
        }
        for (field, count) in [
            ("misheard", &mut misheard),
            ("misheard_right", &mut misheard_right),
        ] {
            match &item[field] {
                Value::Null => {}
                v => {
                    let ok = |k: &str| v[k].as_str().is_some_and(|s| !s.trim().is_empty());
                    if !(ok("written") && ok("said")) {
                        return Err(at(format!(
                            "{id}: {field} is null or {{\"written\", \"said\"}}"
                        )));
                    }
                    *count += 1;
                }
            }
        }
        let reason = item["reason"].as_str().unwrap_or_default();
        if reason.trim().is_empty() || reason.contains('\n') {
            return Err(at(format!("{id}: reason must be one non-empty line")));
        }
    }
    if let Some(missing) = want.keys().find(|id| !seen.contains(*id)) {
        return Err(at(format!("{missing} has no mark")));
    }
    Ok(json!({
        "decisions_caught": caught_d.len(),
        "actions_caught": caught_a.len(),
        "made_up": made_up,
        "duplicates": duplicates,
        "not_listed": not_listed,
        "owner_right": owner_right,
        "misheard": misheard,
        "misheard_right": misheard_right,
    }))
}

/// Adds every number of `b` into `a`.
fn add(a: &mut Value, b: &Value) {
    for (k, v) in b.as_object().into_iter().flatten() {
        let sum = a[k].as_u64().unwrap_or(0) + v.as_u64().unwrap_or(0);
        a[k] = json!(sum);
    }
}

/// Counts every marks file of `dir` (module docs) and writes `out` (JSON) and its `.md`
/// table.
///
/// # Errors
/// A packet set that differs from the key's, a summary packet without marks (or marks
/// without a packet), a model set that differs between meetings, or any [`check_marks`]
/// failure.
pub fn count(
    dir: &Path,
    meetings: &[String],
    key: Option<&Value>,
    out: &Path,
) -> Result<(), String> {
    if let Some(note) = judge::check_packet_set(key, dir, meetings)? {
        eprintln!("sum_eval: {note}");
    }
    let read = |p: &Path| -> Result<Value, String> {
        let text = std::fs::read_to_string(p).map_err(|e| format!("{}: {e}", p.display()))?;
        serde_json::from_str(&text).map_err(|e| format!("{}: {e}", p.display()))
    };
    let mut totals: BTreeMap<String, Value> = BTreeMap::new();
    let mut per_meeting = Vec::new();
    let mut models: Option<BTreeSet<String>> = None;
    for meeting in meetings {
        let mdir = dir.join(meeting);
        let labels = judge::packet_labels(&mdir)?;
        for entry in std::fs::read_dir(&mdir).map_err(|e| format!("{}: {e}", mdir.display()))? {
            let name = entry
                .map_err(|e| e.to_string())?
                .file_name()
                .to_string_lossy()
                .into_owned();
            if let Some(l) = name
                .strip_prefix("real.")
                .and_then(|r| r.strip_suffix(".json"))
                && !labels.iter().any(|x| x == l)
            {
                return Err(format!("{meeting}: {name} without a summary packet"));
            }
        }
        let mut here = BTreeSet::new();
        for label in &labels {
            let summary = read(&mdir.join(format!("summary.{label}.json")))?;
            let marks = read(&mdir.join(format!("real.{label}.json")))?;
            let counts = check_marks(&summary, &marks, meeting, label)?;
            let model = match key {
                Some(k) => k[meeting.as_str()][label.as_str()]
                    .as_str()
                    .ok_or_else(|| format!("{meeting}: {label} is not in the blinding key"))?
                    .to_owned(),
                None => label.clone(),
            };
            here.insert(model.clone());
            add(
                totals.entry(model.clone()).or_insert_with(|| json!({})),
                &counts,
            );
            per_meeting.push(
                json!({"meeting": meeting, "label": label, "model": model, "counts": counts}),
            );
        }
        match &models {
            None => models = Some(here),
            Some(m) if *m != here => {
                return Err(format!("{meeting}: models differ from other meetings"));
            }
            Some(_) => {}
        }
    }
    let mut table = String::from(
        "| model | decisions caught | actions caught (owner right) | made up | duplicates | not listed | mis-heard | mis-heard right |\n|---|---|---|---|---|---|---|---|\n",
    );
    for (model, c) in &totals {
        table.push_str(&format!(
            "| {model} | {} | {} ({}) | {} | {} | {} | {} | {} |\n",
            c["decisions_caught"],
            c["actions_caught"],
            c["owner_right"],
            c["made_up"],
            c["duplicates"],
            c["not_listed"],
            c["misheard"],
            c["misheard_right"]
        ));
    }
    let report = json!({
        "marks": dir.display().to_string(),
        "meetings": meetings,
        "packet_set": key.and_then(|k| k["packet_set"].as_str()),
        "models": totals,
        "per_meeting": per_meeting,
    });
    std::fs::write(
        out,
        serde_json::to_vec_pretty(&report).map_err(|e| e.to_string())?,
    )
    .map_err(|e| format!("{}: {e}", out.display()))?;
    let md = out.with_extension("md");
    std::fs::write(&md, &table).map_err(|e| format!("{}: {e}", md.display()))?;
    print!("{table}");
    eprintln!("sum_eval: wrote {} and {}", out.display(), md.display());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn summary() -> Value {
        json!({"decisions": [{"id": "S-D1", "text": "x"}, {"id": "S-D2", "text": "y"}],
               "action_items": [{"id": "S-A1", "owner": "o", "task": "t", "due": null}]})
    }

    fn item(id: &str, kind: &str, mark: &str, real: &[&str]) -> Value {
        json!({"id": id, "kind": kind, "mark": mark, "real": real, "owner_right": null,
               "also_made_up": false, "misheard": null, "misheard_right": null, "reason": "r"})
    }

    fn marks(items: Vec<Value>) -> Value {
        json!({"meeting": "M", "label": "S1", "items": items})
    }

    #[test]
    fn sum_eval_marks_count_real_items_once_and_misheard_items() {
        let mut a = item("S-A1", "action", "caught", &["A2"]);
        a["owner_right"] = json!(true);
        a["misheard"] = json!({"written": "Pat", "said": "pad"});
        let mut d2 = item("S-D2", "decision", "caught", &["D1"]);
        d2["also_made_up"] = json!(true);
        let c = check_marks(
            &summary(),
            &marks(vec![
                item("S-D1", "decision", "caught", &["D1", "D3"]),
                d2,
                a,
            ]),
            "M",
            "S1",
        )
        .unwrap();
        assert_eq!(c["decisions_caught"], 2, "D1 counts once");
        assert_eq!(c["actions_caught"], 1);
        assert_eq!(
            (c["made_up"].as_u64(), c["owner_right"].as_u64()),
            (Some(1), Some(1))
        );
        assert_eq!(c["misheard"], 1);
    }

    #[test]
    fn sum_eval_marks_refuse_missing_extra_and_malformed_items() {
        let ok = || {
            vec![
                item("S-D1", "decision", "made_up", &[]),
                item("S-D2", "decision", "duplicate", &[]),
                item("S-A1", "action", "not_listed", &[]),
            ]
        };
        assert!(check_marks(&summary(), &marks(ok()), "M", "S1").is_ok());
        let refused =
            |items: Vec<Value>| check_marks(&summary(), &marks(items), "M", "S1").is_err();
        assert!(refused(ok()[..2].to_vec()), "S-A1 unmarked");
        let mut extra = ok();
        extra.push(item("S-D9", "decision", "made_up", &[]));
        assert!(refused(extra));
        let mut twice = ok();
        twice.push(item("S-D1", "decision", "made_up", &[]));
        assert!(refused(twice));
        let mut wrong_kind = ok();
        wrong_kind[0] = item("S-D1", "action", "made_up", &[]);
        assert!(refused(wrong_kind));
        let mut caught_empty = ok();
        caught_empty[0] = item("S-D1", "decision", "caught", &[]);
        assert!(refused(caught_empty));
        let mut real_on_made_up = ok();
        real_on_made_up[0] = item("S-D1", "decision", "made_up", &["D1"]);
        assert!(refused(real_on_made_up));
        let mut bad_real = ok();
        bad_real[0] = item("S-D1", "decision", "caught", &["X1"]);
        assert!(refused(bad_real));
        let mut half_misheard = ok();
        half_misheard[0]["misheard"] = json!({"written": "a"});
        assert!(refused(half_misheard));
        let mut bad_mark = ok();
        bad_mark[0]["mark"] = json!("missed");
        assert!(refused(bad_mark));
        let mut two_lines = ok();
        two_lines[0]["reason"] = json!("a\nb");
        assert!(refused(two_lines));
        assert!(
            check_marks(&summary(), &marks(ok()), "M", "S2").is_err(),
            "label"
        );
    }
}
