//! Tests of the user's own words IPC (T12.15a). Every name contains `summary_vocabulary`;
//! every word is invented.

use super::*;
use crate::testutil::{TempDir, services};

fn owned(words: &[&str]) -> Vec<String> {
    words.iter().map(|w| (*w).to_owned()).collect()
}

#[test]
fn summary_vocabulary_view_starts_empty_and_saves_checked_words() {
    let dir = TempDir::new("summary-vocabulary");
    let svc = services(&dir);
    let v = vocabulary_view(&svc).unwrap();
    assert_eq!(
        v,
        SummaryVocabularyView {
            words: vec![],
            unknown: vec![],
            max_words: 100,
            max_word_chars: 40,
            max_chars: 1000,
        }
    );
    let saved = set_words(&svc, &owned(&[" Quillmarsh ", "", "QXR-7", "qxr-7"])).unwrap();
    assert_eq!(saved.words, owned(&["Quillmarsh", "QXR-7"]));
    assert_eq!(vocabulary_view(&svc).unwrap(), saved);
    // Clearing: no words.
    assert!(set_words(&svc, &[]).unwrap().words.is_empty());
    assert!(vocabulary_view(&svc).unwrap().words.is_empty());
}

#[test]
fn summary_vocabulary_refusals_are_plain_sentences_and_store_nothing() {
    let dir = TempDir::new("summary-vocabulary");
    let svc = services(&dir);
    set_words(&svc, &owned(&["Quillmarsh"])).unwrap();
    let long = "q".repeat(41);
    let err = set_words(&svc, &owned(&["QXR-7", "", &long])).unwrap_err();
    assert_eq!(err.kind, "invalid_setting");
    assert_eq!(
        err.message,
        "A word can have at most 40 characters; this one has 41."
    );
    let many: Vec<String> = (0..101).map(|i| format!("w{i}")).collect();
    let err = set_words(&svc, &many).unwrap_err();
    assert_eq!(
        err.message,
        "The list can hold at most 100 words; this one has 101."
    );
    let err = set_words(&svc, &owned(&["a\tb"])).unwrap_err();
    assert_eq!(
        err.message,
        "A word can hold only text, with no line breaks or tabs."
    );
    assert_eq!(vocabulary_view(&svc).unwrap().words, owned(&["Quillmarsh"]));
}

#[test]
fn summary_vocabulary_the_view_serialises_for_the_ui() {
    let json = serde_json::to_value(view(&owned(&["QXR-7"]), vec![])).unwrap();
    assert_eq!(
        json,
        serde_json::json!({
            "words": ["QXR-7"],
            "unknown": [],
            "max_words": 100,
            "max_word_chars": 40,
            "max_chars": 1000,
        })
    );
}

#[test]
fn summary_vocabulary_follows_the_shared_case_table() {
    let table = crate::testutil::summary_cases();
    let limits = &table["limits"];
    let dir = TempDir::new("summary-vocabulary");
    let svc = services(&dir);
    let v = vocabulary_view(&svc).unwrap();
    assert_eq!(limits["max_vocabulary_words"], v.max_words);
    assert_eq!(limits["max_vocabulary_word_chars"], v.max_word_chars);
    assert_eq!(limits["max_vocabulary_chars"], v.max_chars);
    let cases = table["vocabulary"].as_array().unwrap();
    assert!(cases.len() >= 13, "the table lost cases");
    let mut wrong = Vec::new();
    for case in cases {
        let name = case["name"].as_str().unwrap();
        let words: Vec<String> = serde_json::from_value(case["words"].clone()).unwrap();
        match (set_words(&svc, &words), case["error"].as_str()) {
            (Err(e), Some(want)) if e.kind == "invalid_setting" && e.message == want => {}
            (Ok(view), None) => {
                let want: Vec<String> = serde_json::from_value(case["stored"].clone()).unwrap();
                if view.words != want {
                    wrong.push(format!("{name}: stored {:?}", view.words));
                }
            }
            (Err(e), _) => wrong.push(format!("{name}: refused {:?}", e.message)),
            (Ok(view), Some(_)) => wrong.push(format!("{name}: stored {:?}", view.words)),
        }
    }
    assert!(wrong.is_empty(), "{wrong:#?}");
}
