//! No text the app shows names a Settings page that no longer exists (T12.5b): Models and
//! Benchmark were dissolved into System, Transcription and Summaries, Speakers became People,
//! and Obsidian and Storage became Notes and storage. The UI has the same guard
//! (`packages/ui/src/settings/pageNames.test.ts`); both take the gone pages and the forms from
//! `fixtures/ui/page_name_cases.json` and run its cases, so a rule change edits that file.
//!
//! The scan reads every source of this app and of `earshot-runtime` (whose text reaches the app
//! through `plain_text` or as a warning), with comments left out and Rust's string line
//! continuations joined, so a page name split across two source lines is still one phrase.

use std::path::{Path, PathBuf};

/// The shared table of gone pages, forms and cases.
fn table() -> serde_json::Value {
    serde_json::from_str(include_str!("../../../../fixtures/ui/page_name_cases.json")).unwrap()
}

fn strings(value: &serde_json::Value) -> Vec<String> {
    value
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_owned())
        .collect()
}

/// The ways text names a gone page: "the Models page", "Settings, Models", "see Models".
fn forms() -> Vec<String> {
    let table = table();
    let forms = strings(&table["forms"]);
    strings(&table["gone_pages"])
        .iter()
        .flat_map(|page| forms.iter().map(move |f| f.replace("{page}", page)))
        .collect()
}

/// Whether `text` holds `form` as whole words.
fn holds(text: &str, form: &str) -> bool {
    text.match_indices(form).any(|(at, _)| {
        let after = text[at + form.len()..].chars().next();
        !after.is_some_and(|c| c.is_alphanumeric() || c == '_')
    })
}

/// The gone page names `text` uses, in the forms above.
fn stale_page_names(text: &str) -> Vec<String> {
    forms()
        .into_iter()
        .filter(|form| holds(text, form))
        .collect()
}

/// `source` with comment lines left out and string line continuations (`\` at a line end and
/// the next line's leading spaces) joined.
fn code(source: &str) -> String {
    let kept: Vec<&str> = source
        .lines()
        .filter(|line| !line.trim_start().starts_with("//"))
        .collect();
    let mut out = String::new();
    let mut joining = false;
    for line in kept {
        let line = if joining { line.trim_start() } else { line };
        if let Some(head) = line.strip_suffix('\\') {
            out.push_str(head);
            joining = true;
        } else {
            out.push_str(line);
            out.push('\n');
            joining = false;
        }
    }
    out
}

fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            rust_files(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

#[test]
fn page_names_the_rule_finds_a_gone_page_and_passes_the_pages_that_exist() {
    let table = table();
    let cases = table["cases"].as_array().unwrap();
    assert_eq!(table["forms"].as_array().unwrap().len(), 3);
    assert!(cases.len() >= 7, "the table lost cases");
    for case in cases {
        let text = case["text"].as_str().unwrap();
        assert_eq!(stale_page_names(text), strings(&case["found"]), "{text}");
    }
    for page in [
        "General",
        "Audio",
        "System",
        "Transcription",
        "Summaries",
        "Dictionary",
        "People",
        "Notes and storage",
    ] {
        let text = format!("Open Settings, {page}. It is on the {page} page; see {page}.");
        assert!(stale_page_names(&text).is_empty(), "{text}");
    }
    // A continuation joins the two halves of a phrase; a comment is left out.
    let joined =
        code("let a = \"Open Settings, \\\n         Models to see.\";\n// the Models page\n");
    assert_eq!(joined, "let a = \"Open Settings, Models to see.\";\n");
    assert_eq!(stale_page_names(&joined), ["Settings, Models"]);
    // Words that only start like a page name are not one.
    assert!(stale_page_names("Settings, Modelsville").is_empty());
}

#[test]
fn page_names_no_shown_text_of_the_app_or_the_runtime_names_a_gone_page() {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut files = Vec::new();
    rust_files(&manifest.join("src"), &mut files);
    rust_files(
        &manifest.join("../../../crates/earshot-runtime/src"),
        &mut files,
    );
    let this = Path::new(file!()).file_name().unwrap();
    assert!(files.iter().any(|f| f.ends_with("plain_text.rs")));
    assert!(files.iter().any(|f| f.ends_with("backends.rs")));
    let found: Vec<String> = files
        .iter()
        .filter(|f| f.file_name() != Some(this))
        .flat_map(|f| {
            let text = code(&std::fs::read_to_string(f).unwrap());
            stale_page_names(&text)
                .into_iter()
                .map(move |form| format!("{}: {form}", f.display()))
        })
        .collect();
    assert!(found.is_empty(), "{found:#?}");
}
