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
    let json = serde_json::to_value(view("Short.", vec![])).unwrap();
    assert_eq!(
        json,
        serde_json::json!({"style": "Short.", "unknown": [], "max_chars": 1000})
    );
}
