//! `cargo xtask schema [--write]` and ci step `schema`: `schemas/summary.v1.json` (the
//! stored summary) and `schemas/summary-llm.v1.json` (the reply models write, citing
//! segment numbers; T6.20) are generated from `earshot_core::summary` and committed; ci
//! fails when a committed file drifts from the code.

use std::path::Path;

use anyhow::{Context, Result, bail};
use earshot_core::summary::{
    SUMMARY_LLM_SCHEMA_PATH, SUMMARY_SCHEMA_PATH, json_schema, llm_json_schema,
};
use serde_json::Value;

/// A schema generator.
pub type Generator = fn() -> Value;

/// Every generated schema: committed path and generator.
pub const SCHEMAS: [(&str, Generator); 2] = [
    (SUMMARY_SCHEMA_PATH, json_schema),
    (SUMMARY_LLM_SCHEMA_PATH, llm_json_schema),
];

/// A schema file's content: pretty JSON, LF line endings, one trailing newline.
fn render_value(v: &Value) -> String {
    let mut s = serde_json::to_string_pretty(v).expect("a serde_json::Value always serialises");
    s.push('\n');
    s
}

/// The summary v1 schema file content generated from the types.
pub fn render() -> String {
    render_value(&json_schema())
}

/// The reply schema file content.
pub fn render_llm() -> String {
    render_value(&llm_json_schema())
}

/// Compares committed summary v1 content with [`render`] byte for byte.
pub fn check_content(committed: &str) -> Result<()> {
    check_file_content(SUMMARY_SCHEMA_PATH, committed, &render())
}

fn check_file_content(path: &str, committed: &str, generated: &str) -> Result<()> {
    if committed != generated {
        bail!(
            "schema: {path} is out of date with earshot_core::summary; \
             run `cargo xtask schema --write` and commit the result"
        );
    }
    Ok(())
}

/// ci step: every committed schema matches the code.
pub fn check(root: &Path) -> Result<()> {
    for (rel, generate) in SCHEMAS {
        let path = root.join(rel);
        let committed = std::fs::read_to_string(&path).with_context(|| {
            format!(
                "schema: cannot read {} (run `cargo xtask schema --write`)",
                path.display()
            )
        })?;
        check_file_content(rel, &committed, &render_value(&generate()))?;
        println!("schema: {rel} is up to date");
    }
    Ok(())
}

/// Regenerates the committed schemas.
pub fn write(root: &Path) -> Result<()> {
    for (rel, generate) in SCHEMAS {
        let path = root.join(rel);
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        }
        std::fs::write(&path, render_value(&generate()))
            .with_context(|| format!("writing {}", path.display()))?;
        println!("schema: wrote {rel}");
    }
    Ok(())
}

/// `cargo xtask schema`: check, or regenerate with `write`.
pub fn run(root: &Path, write_file: bool) -> Result<()> {
    if write_file { write(root) } else { check(root) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schema_render_is_deterministic_lf_json_with_one_trailing_newline() {
        let a = render();
        assert_eq!(a, render());
        assert!(!a.contains('\r'));
        assert!(a.ends_with("}\n") && !a.ends_with("\n\n"));
        let v: serde_json::Value = serde_json::from_str(&a).unwrap();
        assert_eq!(v["title"], "Earshot meeting summary v1");
        let l = render_llm();
        assert!(l.ends_with("}\n") && !l.contains('\r'));
        assert!(l.contains("\"integer\"") && !l.contains("uuid"));
    }

    #[test]
    fn schema_drift_is_detected() {
        let good = render();
        assert!(check_content(&good).is_ok());
        let edited = good.replacen("\"title\"", "\"titel\"", 1);
        assert!(check_content(&edited).is_err());
        assert!(check_content(&good.replace('\n', "\r\n")).is_err());
        assert!(check_content(good.trim_end()).is_err());
        assert!(check_content("").is_err());
        let llm = render_llm();
        assert!(check_file_content(SUMMARY_LLM_SCHEMA_PATH, &llm, &render_llm()).is_ok());
        let edited = llm.replacen("\"maxItems\": 3", "\"maxItems\": 4", 1);
        assert_ne!(edited, llm);
        assert!(check_file_content(SUMMARY_LLM_SCHEMA_PATH, &edited, &render_llm()).is_err());
    }

    #[test]
    fn schema_committed_files_are_checked_at_the_planned_paths() {
        assert_eq!(SUMMARY_SCHEMA_PATH, "schemas/summary.v1.json");
        assert_eq!(SUMMARY_LLM_SCHEMA_PATH, "schemas/summary-llm.v1.json");
        let missing = std::env::temp_dir().join("earshot-xtask-schema-missing-root");
        assert!(check(&missing).is_err());
        // The committed files are current.
        assert!(check(&crate::repo_root()).is_ok());
    }
}
