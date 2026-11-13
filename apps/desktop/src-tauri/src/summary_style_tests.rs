//! Tests of the summary style IPC (T7.4b). Every name contains `summary_style`.

use super::*;
use crate::testutil::{TempDir, services};

#[test]
fn summary_style_view_starts_empty_and_saves_trimmed_text() {
    let dir = TempDir::new("summary-style");
    let svc = services(&dir);
    let v = style_view(&svc).unwrap();
    assert_eq!(
        v,
        SummaryStyleView {
            style: String::new(),
            unknown: vec![],
            max_chars: 1000,
            template: "standard".into(),
            templates: template_options(),
        }
    );
    let saved = set_style(&svc, "  Short and plain.\r\nSpell out acronyms. ").unwrap();
    assert_eq!(saved.style, "Short and plain.\nSpell out acronyms.");
    assert_eq!(style_view(&svc).unwrap(), saved);
    // Reset to default: empty.
    assert_eq!(set_style(&svc, "").unwrap().style, "");
    assert_eq!(style_view(&svc).unwrap().style, "");
}

#[test]
fn summary_style_refusals_are_plain_sentences_and_store_nothing() {
    let dir = TempDir::new("summary-style");
    let svc = services(&dir);
    set_style(&svc, "Keep it short.").unwrap();
    let err = set_style(&svc, &"a".repeat(1001)).unwrap_err();
    assert_eq!(err.kind, "invalid_setting");
    assert_eq!(
        err.message,
        "The summary style can be at most 1000 characters; this one has 1001."
    );
    let err = set_style(&svc, "a\u{1b}b").unwrap_err();
    assert_eq!(
        err.message,
        "The summary style can hold only text, line breaks and tabs."
    );
    assert_eq!(style_view(&svc).unwrap().style, "Keep it short.");
}

#[test]
fn summary_style_the_view_serialises_for_the_ui() {
    let json = serde_json::to_value(view("Short.", StylePreset::Brief, vec![])).unwrap();
    assert_eq!(json["style"], "Short.");
    assert_eq!(json["unknown"], serde_json::json!([]));
    assert_eq!(json["max_chars"], 1000);
    assert_eq!(json["template"], "brief");
    assert_eq!(
        json["templates"][1],
        serde_json::json!({
            "value": "brief",
            "label": "Brief",
            "description": "A one-sentence summary and at most three note bullets.",
        })
    );
}

#[test]
fn summary_style_follows_the_shared_case_table() {
    let table = crate::testutil::summary_cases();
    let dir = TempDir::new("summary-style");
    let svc = services(&dir);
    assert_eq!(
        table["limits"]["max_style_chars"],
        style_view(&svc).unwrap().max_chars
    );
    let cases = table["style"].as_array().unwrap();
    assert!(cases.len() >= 7, "the table lost cases");
    let mut wrong = Vec::new();
    for case in cases {
        let name = case["name"].as_str().unwrap();
        let text = case["text"].as_str().unwrap();
        match (set_style(&svc, text), case["error"].as_str()) {
            (Err(e), Some(want)) if e.kind == "invalid_setting" && e.message == want => {}
            (Ok(view), None) => {
                if case["stored"].as_str() != Some(view.style.as_str()) {
                    wrong.push(format!("{name}: stored {:?}", view.style));
                }
            }
            (Err(e), _) => wrong.push(format!("{name}: refused {:?}", e.message)),
            (Ok(view), Some(_)) => wrong.push(format!("{name}: stored {:?}", view.style)),
        }
    }
    assert!(wrong.is_empty(), "{wrong:#?}");
}

#[test]
fn summary_style_template_applies_at_once_and_keeps_the_style() {
    let dir = TempDir::new("summary-style");
    let svc = services(&dir);
    set_style(&svc, "Spell out acronyms.").unwrap();
    let v = set_template(&svc, "decisions").unwrap();
    assert_eq!(
        (v.template.as_str(), v.style.as_str()),
        ("decisions", "Spell out acronyms.")
    );
    assert_eq!(style_view(&svc).unwrap(), v);
    // Saving the style keeps the template.
    assert_eq!(set_style(&svc, "Plain.").unwrap().template, "decisions");
    let err = set_template(&svc, "terse").unwrap_err();
    assert_eq!(
        (err.kind.as_str(), err.message.as_str()),
        ("invalid_setting", UNKNOWN_TEMPLATE)
    );
    assert_eq!(style_view(&svc).unwrap().template, "decisions");
    // Every option is one the runtime stores.
    for o in template_options() {
        assert_eq!(set_template(&svc, &o.value).unwrap().template, o.value);
    }
}

#[test]
fn summary_style_templates_follow_the_shared_case_table() {
    let table = crate::testutil::summary_cases();
    let want = &table["templates"];
    assert_eq!(
        serde_json::to_value(template_options()).unwrap(),
        want["options"]
    );
    assert_eq!(want["unknown_error"], UNKNOWN_TEMPLATE);
}
