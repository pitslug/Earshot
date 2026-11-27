//! No text the app shows names a Settings page that no longer exists (T12.5b): Models and
//! Benchmark were dissolved into System, Transcription and Summaries, Speakers became People,
//! and Obsidian and Storage became Notes and storage. The UI has the same guard
//! (`packages/ui/src/settings/pageNames.test.ts`); both take the gone pages and the forms from
//! `fixtures/ui/page_name_cases.json` and run its cases, so a rule change edits that file.
//!
//! The scan reads every source of this app and of `earshot-runtime` (whose text reaches the app
//! through `plain_text` or as a warning), with comments left out and Rust's string line
//! continuations joined, so a page name split across two source lines is still one phrase. It
//! also reads README.md and PRIVACY.md at the repo root whole (T14.53).

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

/// README.md and PRIVACY.md name Settings pages for people reading them (T14.53): read whole,
/// every line counts, as in the UI's guard.
#[test]
fn page_names_readme_and_privacy_name_no_gone_page() {
    let readme = include_str!("../../../../README.md");
    let privacy = include_str!("../../../../PRIVACY.md");
    assert!(readme.contains("General page"));
    assert!(privacy.contains("General page"));
    for (name, text) in [("README.md", readme), ("PRIVACY.md", privacy)] {
        assert!(
            stale_page_names(text).is_empty(),
            "{name}: {:?}",
            stale_page_names(text)
        );
    }
}

/// The voice sample copy sends the user to "the People page" to start a voice over (T12.77b7):
/// that page exists in Settings and holds the Remove button the copy means.
#[test]
fn page_names_the_voice_sample_copy_names_the_people_page_that_removes_a_person() {
    let copy: serde_json::Value = serde_json::from_str(include_str!(
        "../../../../fixtures/ui/voice_sample_copy_cases.json"
    ))
    .unwrap();
    let text = copy["not_saved_before_samples"].as_str().unwrap();
    assert!(text.contains("remove them on the People page"), "{text}");
    assert!(stale_page_names(text).is_empty(), "{text}");
    let settings = include_str!("../../../../packages/ui/src/settings/SettingsView.tsx");
    let sections = settings
        .split_once("export const SETTINGS_SECTIONS = [")
        .and_then(|(_, rest)| rest.split_once(']'))
        .unwrap()
        .0;
    assert!(sections.contains("\"People\""), "{sections}");
    let people = include_str!("../../../../packages/ui/src/settings/PeoplePage.tsx");
    assert!(people.contains("Remove {removing?.name}?"));
}

/// The Dictionary page says names on "the People page" are used too and links to it (T12.91,
/// T12.97): that page exists in Settings, and the link switches to it.
#[test]
fn page_names_the_dictionary_page_names_the_people_page_its_names_come_from() {
    let page = include_str!("../../../../packages/ui/src/settings/DictionaryPage.tsx");
    let constant = |name: &str| {
        page.split_once(&format!("export const {name} ="))
            .and_then(|(_, rest)| rest.split_once(';'))
            .unwrap()
            .0
    };
    let text = constant("SAVED_PEOPLE_NOTE");
    assert!(text.contains("Names on the People page"), "{text}");
    assert!(stale_page_names(text).is_empty(), "{text}");
    let link = constant("OPEN_PEOPLE_LABEL");
    assert!(link.contains("the People page"), "{link}");
    assert!(stale_page_names(link).is_empty(), "{link}");
    let settings = include_str!("../../../../packages/ui/src/settings/SettingsView.tsx");
    let sections = settings
        .split_once("export const SETTINGS_SECTIONS = [")
        .and_then(|(_, rest)| rest.split_once(']'))
        .unwrap()
        .0;
    assert!(sections.contains("\"People\""), "{sections}");
    assert!(
        settings.contains("<DictionaryPage onOpenPeople={() => setSection(\"People\")} />"),
        "the Dictionary page's link opens the People page"
    );
}

/// Details names the Sync page for a damaged kind whose own switch is off (T14.65,
/// `kind_off_voice` and `kind_off_words`): that page exists in Settings and holds both switches.
#[test]
fn page_names_details_kind_off_sentences_name_the_sync_page_with_its_switches() {
    let copy: serde_json::Value = serde_json::from_str(include_str!(
        "../../../../fixtures/ui/damaged_copy_cases.json"
    ))
    .unwrap();
    for key in ["kind_off_voice", "kind_off_words"] {
        let text = copy[key].as_str().unwrap();
        assert!(text.contains("on the Sync page"), "{key}: {text}");
        assert!(stale_page_names(text).is_empty(), "{key}: {text}");
    }
    // Each names the option by its switch's own label on that page.
    let voice = copy["kind_off_voice"].as_str().unwrap();
    assert!(voice.contains("Turning on Sync voiceprints"), "{voice}");
    let words = copy["kind_off_words"].as_str().unwrap();
    assert!(words.contains("Turning on Dictionary sync"), "{words}");
    let settings = include_str!("../../../../packages/ui/src/settings/SettingsView.tsx");
    let sections = settings
        .split_once("export const SETTINGS_SECTIONS = [")
        .and_then(|(_, rest)| rest.split_once(']'))
        .unwrap()
        .0;
    assert!(sections.contains("\"Sync\""), "{sections}");
    let sync = include_str!("../../../../packages/ui/src/settings/SyncPage.tsx");
    assert!(sync.contains("label=\"Sync voiceprints\""));
    assert!(sync.contains("label=\"Sync Dictionary words\""));
}

/// The local summary failures that name "Settings, Summaries" (T14.66): that page exists, holds
/// the list of writers and the summary model's download.
#[test]
fn page_names_local_failure_copy_names_the_summaries_page_with_writers_and_its_model() {
    let copy: serde_json::Value = serde_json::from_str(include_str!(
        "../../../../fixtures/ui/summary_order_cases.json"
    ))
    .unwrap();
    for key in ["too_long", "load_failed"] {
        let text = copy["local_failures"][key].as_str().unwrap();
        assert!(text.contains("in Settings, Summaries"), "{key}: {text}");
        assert!(stale_page_names(text).is_empty(), "{key}: {text}");
    }
    let settings = include_str!("../../../../packages/ui/src/settings/SettingsView.tsx");
    let sections = settings
        .split_once("export const SETTINGS_SECTIONS = [")
        .and_then(|(_, rest)| rest.split_once(']'))
        .unwrap()
        .0;
    assert!(sections.contains("\"Summaries\""), "{sections}");
    let page = include_str!("../../../../packages/ui/src/settings/SummariesPage.tsx");
    assert!(page.contains("<OrderSection"), "the writers' list");
    assert!(
        page.contains("<SummaryModelChoices"),
        "the summary model's download"
    );
}
