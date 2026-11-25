//! Tests of the user's own words IPC (T12.15a). Every name contains `summary_vocabulary`;
//! every word is invented.

use super::*;
use crate::testutil::{TempDir, services};

fn owned(words: &[&str]) -> Vec<String> {
    words.iter().map(|w| (*w).to_owned()).collect()
}

/// Replaces the whole list through the runtime's whole-list path, with the stored list as the
/// base, and maps a refusal as the page's commands do (the page itself edits one word at a
/// time since T11.3e).
fn set_words(svc: &Services, words: &[String]) -> Result<SummaryVocabularyView, IpcError> {
    let base = vocabulary::vocabulary(&svc.store()).unwrap().0;
    vocabulary::set_vocabulary(svc, base.words(), words).map_err(refusal)?;
    vocabulary_view(svc)
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
            not_in_use: 0,
            not_in_use_note: None,
            people_left_out: 0,
            people_left_out_note: None,
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
    let json = serde_json::to_value(view(&owned(&["QXR-7"]), vec![], 0, 0)).unwrap();
    assert_eq!(
        json,
        serde_json::json!({
            "words": ["QXR-7"],
            "unknown": [],
            "max_words": 100,
            "max_word_chars": 40,
            "max_chars": 1000,
            "not_in_use": 0,
            "not_in_use_note": null,
            "people_left_out": 0,
            "people_left_out_note": null,
        })
    );
}

#[test]
fn summary_vocabulary_saved_people_follow_the_shared_case_table() {
    let table = crate::testutil::summary_cases();
    let cases = table["people"]["cases"].as_array().unwrap();
    assert!(cases.len() >= 8, "the table lost cases");
    for case in cases {
        let strings = |k: &str| -> Vec<String> { serde_json::from_value(case[k].clone()).unwrap() };
        let dictionary = earshot_core::prompt::Vocabulary::new(strings("dictionary")).unwrap();
        let got = earshot_core::prompt::People::new(strings("names"), &dictionary);
        assert_eq!(got.names(), strings("people"), "{}", case["name"]);
        let left_out = earshot_core::prompt::People::left_out(strings("names"));
        assert_eq!(
            u64::try_from(left_out).ok(),
            case["left_out"].as_u64(),
            "{}",
            case["name"]
        );
    }
}

/// The saved names a summary may be given, as the runtime reads them from `svc`'s store.
fn summary_people(svc: &Services) -> Vec<String> {
    let db = svc.store();
    let state = vocabulary::dictionary_state(&db).unwrap();
    earshot_runtime::word_choices::summary_people(&db, &state.words)
        .unwrap()
        .names()
        .to_vec()
}

#[test]
fn summary_vocabulary_lists_no_saved_people_but_summaries_still_get_them() {
    // T12.91: the saved names reach summaries beside the words; T12.97: the Dictionary page
    // lists none of them (the People page holds them), so its view carries no name.
    let dir = TempDir::new("summary-vocabulary");
    let svc = services(&dir);
    // The default "Me" is no one's name.
    assert!(summary_people(&svc).is_empty());
    earshot_runtime::my_voice::set_my_name(&svc, "Tamsin Brevik").unwrap();
    assert_eq!(summary_people(&svc), owned(&["Tamsin Brevik"]));
    let json = serde_json::to_string(&vocabulary_view(&svc).unwrap()).unwrap();
    assert!(
        !json.contains("Tamsin") && !json.contains("Brevik"),
        "{json}"
    );
    // A name the Dictionary already holds (any case) is left to the Dictionary.
    let added = add_word(&svc, "tamsin brevik").unwrap();
    assert_eq!(added.view.words, owned(&["tamsin brevik"]));
    assert!(summary_people(&svc).is_empty());
    remove_word(&svc, "tamsin brevik").unwrap();
    assert_eq!(summary_people(&svc), owned(&["Tamsin Brevik"]));
}

#[test]
fn summary_vocabulary_says_how_many_saved_names_it_left_out_and_why() {
    // T12.91a: a name holding an invisible formatting character (a zero-width non-joiner,
    // legitimate in some scripts) is never given to a summary; the page says how many, in
    // fixed words that hold no name.
    let table = crate::testutil::summary_cases();
    let copy = &table["messages"]["people_left_out"];
    assert_eq!(people_left_out_note(0), None);
    assert_eq!(people_left_out_note(1).as_deref(), copy["one"].as_str());
    let many = copy["many"].as_str().unwrap();
    assert!(many.contains("{n}"));
    assert_eq!(people_left_out_note(4), Some(many.replace("{n}", "4")));
    let dir = TempDir::new("summary-vocabulary");
    let svc = services(&dir);
    earshot_runtime::my_voice::set_my_name(&svc, "Nim\u{200c}a Rostavel").unwrap();
    add_person(&svc, "Vantrel Oskarine");
    let v = vocabulary_view(&svc).unwrap();
    assert_eq!(summary_people(&svc), owned(&["Vantrel Oskarine"]));
    assert_eq!(v.people_left_out, 1);
    assert_eq!(v.people_left_out_note.as_deref(), copy["one"].as_str());
    // The view the window gets carries the count (positive control: the name it counts is
    // stored) and no text of any saved name (T12.97: it lists none).
    let json = serde_json::to_string(&v).unwrap();
    assert!(json.contains("\"people_left_out\":1"), "{json}");
    assert!(
        !json.contains("Rostavel") && !json.contains("Nim") && !json.contains("Vantrel"),
        "{json}"
    );
    earshot_runtime::my_voice::set_my_name(&svc, "Nima Rostavel").unwrap();
    let v = vocabulary_view(&svc).unwrap();
    assert_eq!(
        summary_people(&svc),
        owned(&["Nima Rostavel", "Vantrel Oskarine"])
    );
    assert_eq!((v.people_left_out, v.people_left_out_note), (0, None));
}

/// Saves a person named `name` with no voice, as the People page lists them.
fn add_person(svc: &Services, name: &str) {
    use earshot_core::model::Stamp;
    use earshot_core::voiceprint::{Person, VoiceprintRepo};
    use rand_core::SeedableRng;
    let clock = std::sync::Arc::clone(&svc.clock);
    let mut rng = rand_chacha::ChaCha8Rng::seed_from_u64(name.len() as u64 + 7);
    let mut db = svc.store();
    let (store, hlc) = db.parts();
    let mut stamp = Stamp {
        clock: &*clock,
        hlc,
        rng: &mut rng,
    };
    store
        .put_person(Person::new(name, false, &mut stamp))
        .unwrap();
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

#[test]
fn summary_vocabulary_words_not_in_use_follow_the_shared_table() {
    let table = crate::testutil::summary_cases();
    let copy = &table["messages"]["not_in_use"];
    assert_eq!(not_in_use_note(0), None);
    assert_eq!(not_in_use_note(1).as_deref(), copy["one"].as_str());
    let many = copy["many"].as_str().unwrap();
    assert!(many.contains("{n}"));
    assert_eq!(not_in_use_note(7), Some(many.replace("{n}", "7")));
    // A view counts them and says so.
    let v = view(&owned(&["QXR-7"]), vec![], 3, 0);
    assert_eq!(v.not_in_use, 3);
    assert_eq!(v.not_in_use_note, Some(many.replace("{n}", "3")));
}

#[test]
fn summary_vocabulary_the_page_adds_and_removes_one_word_at_a_time() {
    let dir = TempDir::new("summary-vocabulary");
    let svc = services(&dir);
    let added = add_word(&svc, "  Velmora   Ridge ").unwrap();
    assert!(added.added);
    assert_eq!(added.view.words, owned(&["Velmora Ridge"]));
    // Already there in another case: the result says so, nothing is written.
    let again = add_word(&svc, "velmora ridge").unwrap();
    assert!(!again.added);
    assert_eq!(again.view.words, owned(&["Velmora Ridge"]));
    let err = add_word(&svc, &"q".repeat(41)).unwrap_err();
    assert_eq!(
        err.message,
        "A word can have at most 40 characters; this one has 41."
    );
    assert!(remove_word(&svc, "VELMORA RIDGE").unwrap().words.is_empty());
}

#[test]
fn summary_vocabulary_refusals_of_t11_3e_follow_the_shared_table() {
    let table = crate::testutil::summary_cases();
    let m = &table["messages"];
    let not_in_use = refusal(SetVocabularyError::NotInUse);
    assert_eq!(not_in_use.kind, "invalid_setting");
    assert_eq!(
        Some(not_in_use.message.as_str()),
        m["add_not_in_use"].as_str()
    );
    let stale = refusal(SetVocabularyError::Stale);
    assert_eq!(stale.kind, "invalid_setting");
    assert_eq!(Some(stale.message.as_str()), m["stale_list"].as_str());
}

#[test]
fn summary_vocabulary_quotes_around_a_word_follow_the_shared_dictionary_adds() {
    let table = crate::testutil::summary_cases();
    let cases = table["dictionary_adds"]["cases"].as_array().unwrap();
    assert!(cases.len() >= 12, "the table lost cases");
    let mut wrong = Vec::new();
    for case in cases {
        let add = case["add"].as_str().unwrap();
        let dir = TempDir::new("summary-vocabulary");
        let svc = services(&dir);
        // Stored as it is: the way an earlier version may have stored it.
        let before = serde_json::to_vec(&case["before"]).unwrap();
        svc.store()
            .set_local_value(vocabulary::VOCABULARY_KEY, &before)
            .unwrap();
        match (add_word(&svc, add), case["error"].as_str()) {
            (Err(e), Some(want)) if e.kind == "invalid_setting" && e.message == want => {}
            (Ok(added), None) => {
                let want_added = case["added"].as_bool().unwrap();
                let shown = added.view.words.clone();
                if added.added != want_added
                    || !case["word"]
                        .as_str()
                        .is_some_and(|w| shown.iter().any(|s| s == w))
                {
                    wrong.push(format!("{add:?}: added {} {shown:?}", added.added));
                }
            }
            (Err(e), _) => wrong.push(format!("{add:?}: refused {:?}", e.message)),
            (Ok(_), Some(_)) => wrong.push(format!("{add:?}: not refused")),
        }
        if let Some(remove) = case["remove"].as_str() {
            remove_word(&svc, remove).unwrap();
        }
        let want: Vec<String> = serde_json::from_value(case["stored"].clone()).unwrap();
        let got = vocabulary_view(&svc).unwrap().words;
        if got != want {
            wrong.push(format!("{add:?}: stored {got:?}"));
        }
    }
    assert!(wrong.is_empty(), "{wrong:#?}");
}

#[test]
fn summary_vocabulary_the_shared_table_holds_for_the_pages_one_word_adds() {
    // The page adds one word at a time (T11.3e): each case's words added in turn (blank ones
    // are never sent: the page's Add is off for them) end in the stored list, or the first
    // refusal is the case's message.
    let table = crate::testutil::summary_cases();
    let mut wrong = Vec::new();
    for case in table["vocabulary"].as_array().unwrap() {
        let name = case["name"].as_str().unwrap();
        let dir = TempDir::new("summary-vocabulary");
        let svc = services(&dir);
        let words: Vec<String> = serde_json::from_value(case["words"].clone()).unwrap();
        let mut refused = None;
        for w in words.iter().filter(|w| !w.trim().is_empty()) {
            if let Err(e) = add_word(&svc, w) {
                refused = Some(e);
                break;
            }
        }
        match (refused, case["error"].as_str()) {
            (Some(e), Some(want)) if e.kind == "invalid_setting" && e.message == want => {}
            (None, None) => {
                let want: Vec<String> = serde_json::from_value(case["stored"].clone()).unwrap();
                let got = vocabulary_view(&svc).unwrap().words;
                if got != want {
                    wrong.push(format!("{name}: stored {got:?}"));
                }
            }
            (Some(e), _) => wrong.push(format!("{name}: refused {:?}", e.message)),
            (None, Some(_)) => wrong.push(format!("{name}: not refused")),
        }
    }
    assert!(wrong.is_empty(), "{wrong:#?}");
}
