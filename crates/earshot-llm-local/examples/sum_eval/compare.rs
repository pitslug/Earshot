//! `--compare-summaries <a.json> <b.json>` (T12.25 part 1): whether two finished result files
//! hold the same summaries, meeting by meeting. Two rows are the same when `ok`, `failure`
//! and the stored `summary` are equal as JSON values (the summary is stored as the validated
//! `SummaryV1`, so equal values are equal bytes once serialised). Prints counts and the
//! labels of differing meetings (AMI names or store labels, never content) plus the prompt
//! token and time sums of both files; exits non-zero on any difference or a meeting only one
//! file holds.

use std::collections::BTreeMap;
use std::path::Path;

use serde_json::{Value, json};

/// Rows of a result file by (split, source, meeting).
fn rows(v: &Value) -> BTreeMap<(String, String, String), Value> {
    let mut out = BTreeMap::new();
    for j in v["jobs"].as_array().into_iter().flatten() {
        let split = j["split"].as_str().unwrap_or_default().to_owned();
        let source = j["source"].as_str().unwrap_or_default().to_owned();
        for m in j["meetings"].as_array().into_iter().flatten() {
            let meeting = m["meeting"].as_str().unwrap_or_default().to_owned();
            out.insert((split.clone(), source.clone(), meeting), m.clone());
        }
    }
    out
}

/// The comparison of result files `a` and `b` (see the module docs): `same`, `differ`,
/// `only_a`, `only_b` (meeting labels), and per file the summed prompt tokens, decoded prompt
/// tokens (`prefix_reuse`, when recorded), generated tokens, requests, prompt and total
/// seconds.
#[must_use]
pub fn compare(a: &Value, b: &Value) -> Value {
    let (ra, rb) = (rows(a), rows(b));
    let mut same = 0;
    let mut differ = Vec::new();
    let mut only_a = Vec::new();
    for (k, row) in &ra {
        let label = format!("{}:{}:{}", k.0, k.1, k.2);
        match rb.get(k) {
            None => only_a.push(label),
            Some(other) => {
                let equal = ["ok", "failure", "summary"]
                    .iter()
                    .all(|f| row[*f] == other[*f]);
                if equal {
                    same += 1;
                } else {
                    differ.push(label);
                }
            }
        }
    }
    let only_b: Vec<String> = rb
        .keys()
        .filter(|k| !ra.contains_key(*k))
        .map(|k| format!("{}:{}:{}", k.0, k.1, k.2))
        .collect();
    let sums = |r: &BTreeMap<(String, String, String), Value>| {
        let f = |path: &[&str]| -> f64 {
            r.values()
                .map(|m| path.iter().fold(m, |v, p| &v[*p]).as_f64().unwrap_or(0.0))
                .sum()
        };
        let reuse_recorded = r.values().any(|m| m["report"]["prefix_reuse"].is_object());
        json!({
            "meetings": r.len(),
            "requests": f(&["report", "requests"]),
            "prompt_tokens": f(&["report", "prompt_tokens"]),
            "decoded_prompt_tokens": if reuse_recorded {
                f(&["report", "prefix_reuse", "decoded_tokens"])
            } else {
                f(&["report", "prompt_tokens"])
            },
            "generated_tokens": f(&["report", "generated_tokens"]),
            "prompt_s": f(&["report", "prompt_s"]),
            "generate_s": f(&["report", "generate_s"]),
            "seconds": f(&["seconds"]),
            "hangup_s": f(&["report", "timing", "hangup_s"]),
        })
    };
    json!({
        "same": same,
        "differ": differ,
        "only_a": only_a,
        "only_b": only_b,
        "identical": differ.is_empty() && only_a.is_empty() && only_b.is_empty() && same > 0,
        "a": sums(&ra),
        "b": sums(&rb),
    })
}

fn read(p: &Path) -> Result<Value, String> {
    let text = std::fs::read_to_string(p).map_err(|e| format!("{}: {e}", p.display()))?;
    serde_json::from_str(&text).map_err(|e| format!("{}: {e}", p.display()))
}

/// `--compare-summaries a b`: prints the comparison as JSON; `Err` unless identical.
pub fn run(a: &Path, b: &Path) -> Result<(), String> {
    let c = compare(&read(a)?, &read(b)?);
    println!(
        "{}",
        serde_json::to_string_pretty(&c).map_err(|e| e.to_string())?
    );
    if c["identical"] == true {
        Ok(())
    } else {
        Err(format!(
            "the summaries differ: {} same, {} differ, {} only in the first, {} only in the second",
            c["same"],
            c["differ"].as_array().map_or(0, Vec::len),
            c["only_a"].as_array().map_or(0, Vec::len),
            c["only_b"].as_array().map_or(0, Vec::len),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(rows: Value) -> Value {
        json!({"jobs": [{"split": "dev", "source": "reference", "meetings": rows}]})
    }

    #[test]
    fn sum_eval_compare_finds_any_difference_in_a_stored_summary() {
        let row = |m: &str, title: &str, decoded: u64| {
            json!({
                "meeting": m, "ok": true, "failure": null, "seconds": 2.0,
                "summary": {"title": title, "decisions": []},
                "report": {"requests": 3, "prompt_tokens": 1000, "generated_tokens": 50,
                           "prompt_s": 1.0, "generate_s": 1.0,
                           "prefix_reuse": {"decoded_tokens": decoded}},
            })
        };
        let a = file(json!([row("m1", "A", 1000), row("m2", "B", 1000)]));
        let b = file(json!([row("m1", "A", 400), row("m2", "B", 600)]));
        let c = compare(&a, &b);
        assert_eq!(c["identical"], true);
        assert_eq!(c["same"], 2);
        assert_eq!(c["a"]["decoded_prompt_tokens"], 2000.0);
        assert_eq!(c["b"]["decoded_prompt_tokens"], 1000.0);
        // One character of one title differs.
        let b2 = file(json!([row("m1", "A", 400), row("m2", "B.", 600)]));
        let c = compare(&a, &b2);
        assert_eq!(c["identical"], false);
        assert_eq!(c["differ"], json!(["dev:reference:m2"]));
        // A meeting only one side holds.
        let b3 = file(json!([row("m1", "A", 400)]));
        let c = compare(&a, &b3);
        assert_eq!(c["identical"], false);
        assert_eq!(c["only_a"], json!(["dev:reference:m2"]));
        // A failure on one side.
        let mut failed = row("m2", "B", 600);
        failed["ok"] = json!(false);
        let c = compare(&a, &file(json!([row("m1", "A", 400), failed])));
        assert_eq!(c["identical"], false);
        // Two empty files prove nothing.
        assert_eq!(
            compare(&file(json!([])), &file(json!([])))["identical"],
            false
        );
    }
}
