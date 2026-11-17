//! Tests of the user's own words (T12.15a): stored values, validation, and the words reaching
//! every summary request. Every name contains `summary_vocabulary`; every word is invented.

use earshot_core::asr::CancelToken;
use earshot_core::prompt::MAX_VOCABULARY_WORD_CHARS;
use earshot_core::summary::SummaryV1;
use earshot_llm_replay::mock::{MockResponse, MockServer};
use rand_chacha::ChaCha8Rng;
use rand_core::SeedableRng;

use super::*;
use crate::summary::{SummaryConfig, SummaryService};
use crate::testutil::{TempDir, meeting, replace_words, services};

fn store(dir: &TempDir) -> Store {
    Store::open(&dir.0.join("earshot.db")).unwrap()
}

#[test]
fn summary_vocabulary_is_empty_until_one_is_saved() {
    let dir = TempDir::new("summary-vocabulary");
    let s = store(&dir);
    assert_eq!(vocabulary(&s).unwrap(), (Vocabulary::default(), None));
}

#[test]
fn summary_vocabulary_saves_checked_words_and_empty_clears_them() {
    let dir = TempDir::new("summary-vocabulary");
    let (svc, _clock) = services(&dir);
    let saved = replace_words(&svc, &[" Quillmarsh ", "", "QXR-7", "quillmarsh"]).unwrap();
    assert_eq!(saved.words(), ["Quillmarsh", "QXR-7"]);
    assert_eq!(vocabulary(&svc.store()).unwrap(), (saved, None));
    assert_eq!(
        svc.store().local_value(VOCABULARY_KEY).unwrap().as_deref(),
        Some(br#"["Quillmarsh","QXR-7"]"#.as_slice())
    );
    let cleared = replace_words(&svc, &["  ", ""]).unwrap();
    assert!(cleared.is_empty());
    assert_eq!(
        vocabulary(&svc.store()).unwrap(),
        (Vocabulary::default(), None)
    );
    assert!(replace_words::<&str>(&svc, &[]).unwrap().is_empty());
}

#[test]
fn summary_vocabulary_refusals_store_nothing() {
    let dir = TempDir::new("summary-vocabulary");
    let (svc, _clock) = services(&dir);
    replace_words(&svc, &["Quillmarsh"]).unwrap();
    let long = "q".repeat(MAX_VOCABULARY_WORD_CHARS + 1);
    let err = replace_words(&svc, &["QXR-7", long.as_str()]).unwrap_err();
    assert!(matches!(
        err,
        SetVocabularyError::Refused(VocabularyError::WordTooLong { line: 2, .. })
    ));
    assert_eq!(
        err.to_string(),
        "a word can have at most 40 characters; this one has 41"
    );
    let err = replace_words(&svc, &["a\nb"]).unwrap_err();
    assert!(matches!(
        err,
        SetVocabularyError::Refused(VocabularyError::ControlCharacter { line: 1 })
    ));
    assert_eq!(vocabulary(&svc.store()).unwrap().0.words(), ["Quillmarsh"]);
}

#[test]
fn summary_vocabulary_a_stored_value_this_version_refuses_reads_as_none_and_is_reported() {
    let dir = TempDir::new("summary-vocabulary");
    let (svc, _clock) = services(&dir);
    let private_ish = serde_json::to_vec(&[format!("Quillmarsh{}", "x".repeat(60))]).unwrap();
    svc.store()
        .set_local_value(VOCABULARY_KEY, &private_ish)
        .unwrap();
    let (words, refused) = vocabulary(&svc.store()).unwrap();
    assert!(words.is_empty());
    let refused = refused.expect("reported");
    assert_eq!(refused.key, VOCABULARY_KEY);
    assert!(!refused.to_string().contains("Quillmarsh"), "{refused}");
    assert!(refused.to_string().contains("40"), "{refused}");

    for bad in [&b"\xff\xfe"[..], b"\"Quillmarsh\"", b"[1, 2]", b"{}"] {
        svc.store().set_local_value(VOCABULARY_KEY, bad).unwrap();
        let (words, refused) = vocabulary(&svc.store()).unwrap();
        assert!(words.is_empty());
        let refused = refused.unwrap().to_string();
        assert!(refused.contains("not a list of words"), "{refused}");
        assert!(!refused.contains("Quillmarsh"), "{refused}");
    }
}

#[test]
fn summary_vocabulary_a_refused_stored_value_is_a_meeting_note() {
    use earshot_hardware::{Cpu, Hardware};
    let dir = TempDir::new("summary-vocabulary");
    let (svc, _clock) = services(&dir);
    svc.store()
        .set_local_value(VOCABULARY_KEY, b"[\"bad\\u0007\"]")
        .unwrap();
    let hw = Hardware {
        cpu: Cpu {
            name: Some("Test CPU".into()),
            physical_cores: Some(8),
            logical_cores: Some(8),
        },
        total_memory: Some(16 << 30),
        ..Hardware::default()
    };
    let cpu = [crate::backends::Accelerator::Cpu];
    let setup = crate::host::summary_from_settings(
        &svc.store(),
        &cpu,
        &hw,
        &crate::summary_settings::NoApiKeys,
        crate::keep_local::SummaryPrivacy::Writer,
    )
    .unwrap();
    assert!(
        setup.notes.iter().any(|n| n.contains(VOCABULARY_KEY)),
        "{:?}",
        setup.notes
    );
    // A readable list is no note.
    replace_words(&svc, &["QXR-7"]).unwrap();
    let setup = crate::host::summary_from_settings(
        &svc.store(),
        &cpu,
        &hw,
        &crate::summary_settings::NoApiKeys,
        crate::keep_local::SummaryPrivacy::Writer,
    )
    .unwrap();
    assert!(
        !setup.notes.iter().any(|n| n.contains(VOCABULARY_KEY)),
        "{:?}",
        setup.notes
    );
}

fn reply(title: &str) -> MockResponse {
    let summary = SummaryV1 {
        title: title.into(),
        tldr: "The team agreed to ship the release on Friday.".into(),
        enhanced_notes: vec![],
        decisions: vec![],
        action_items: vec![],
        open_questions: vec![],
        topics: vec!["release".into()],
    };
    MockResponse::completion(&serde_json::to_string(&summary).unwrap(), "stop")
}

#[test]
fn summary_vocabulary_reaches_the_summary_request_and_a_resummarise_uses_the_current_list() {
    let dir = TempDir::new("summary-vocabulary");
    let (svc, _) = services(&dir);
    let id = meeting(&svc, "", 1);
    assert!(
        SummaryService::input(&svc, id)
            .unwrap()
            .vocabulary
            .is_empty()
    );
    replace_words(&svc, &["Quillmarsh", "QXR-7"]).unwrap();
    assert_eq!(
        SummaryService::input(&svc, id).unwrap().vocabulary.words(),
        ["Quillmarsh", "QXR-7"]
    );

    let server = MockServer::start(vec![reply("Release planning"), reply("Again")]).unwrap();
    let endpoint = server.endpoint();
    let service = SummaryService::new(
        &SummaryConfig::new(endpoint.as_str(), "test-model"),
        svc.clock.clone(),
    )
    .unwrap();
    let run = |svc: &crate::Services| {
        service
            .summarize(
                svc,
                id,
                &mut ChaCha8Rng::seed_from_u64(2),
                &CancelToken::new(),
            )
            .unwrap()
    };
    run(&svc);
    replace_words(&svc, &["Ostrelby Ridge"]).unwrap();
    run(&svc);
    let bodies: Vec<String> = server
        .requests()
        .iter()
        .map(|r| String::from_utf8(r.body.clone()).unwrap())
        .collect();
    assert_eq!(bodies.len(), 2);
    assert!(bodies[0].contains("BEGIN USER WORDS"));
    assert!(bodies[0].contains("| Quillmarsh"));
    assert!(!bodies[1].contains("Quillmarsh"));
    assert!(bodies[1].contains("| Ostrelby Ridge"));
    for e in service.audit_log().entries() {
        assert_eq!(
            (e.host.as_str(), e.port),
            (endpoint.host_str().unwrap(), endpoint.port().unwrap())
        );
    }
}

#[test]
fn summary_vocabulary_a_refused_stored_list_still_summarises_without_words() {
    let dir = TempDir::new("summary-vocabulary");
    let (svc, _) = services(&dir);
    let id = meeting(&svc, "", 1);
    svc.store()
        .set_local_value(VOCABULARY_KEY, b"not json")
        .unwrap();
    let input = SummaryService::input(&svc, id).unwrap();
    assert!(input.vocabulary.is_empty());
}

// Adding words from the transcript (T12.15f). Every name contains `vocabulary`.

#[test]
fn vocabulary_dictionary_word_follows_the_shared_case_table() {
    let table: serde_json::Value = serde_json::from_str(include_str!(
        "../../../fixtures/ui/dictionary_word_cases.json"
    ))
    .unwrap();
    let cases = table["cases"].as_array().unwrap();
    assert!(cases.len() >= 10);
    for case in cases {
        let token = case["token"].as_str().unwrap();
        assert_eq!(
            dictionary_word(token),
            case["word"].as_str(),
            "token {token:?}"
        );
    }
}

#[test]
fn vocabulary_an_entry_stored_with_quotes_reads_as_its_stripped_form() {
    let dir = TempDir::new("summary-vocabulary");
    let (svc, _clock) = services(&dir);
    // As a word added from the transcript before T12.45 was stored (written raw).
    svc.store()
        .set_local_value(
            VOCABULARY_KEY,
            br#"["'Zorbek'", "zorbek", "''", "(Velmora)", "Quill 'n' Marsh"]"#,
        )
        .unwrap();
    let (words, refused) = vocabulary(&svc.store()).unwrap();
    assert_eq!(refused, None);
    assert_eq!(words.words(), ["Zorbek", "(Velmora)", "Quill 'n' Marsh"]);
    // It matches the stripped word: not added twice, and removable.
    let added = add_word(&svc, "'Zorbek'").unwrap();
    assert_eq!(
        added,
        AddedWord {
            word: "Zorbek".into(),
            added: false
        }
    );
    assert!(remove_word(&svc, "Zorbek").unwrap());
    assert_eq!(
        vocabulary(&svc.store()).unwrap().0.words(),
        ["(Velmora)", "Quill 'n' Marsh"]
    );
}

#[test]
fn vocabulary_words_to_offer_follows_the_shared_case_table() {
    let table: serde_json::Value = serde_json::from_str(include_str!(
        "../../../fixtures/ui/words_to_offer_cases.json"
    ))
    .unwrap();
    let cases = table["cases"].as_array().unwrap();
    assert!(cases.len() >= 8);
    let strings = |v: &serde_json::Value| -> Vec<String> {
        v.as_array()
            .unwrap()
            .iter()
            .map(|s| s.as_str().unwrap().to_owned())
            .collect()
    };
    for case in cases {
        let after = case["after"].as_str().unwrap();
        let listed = strings(&case["listed"]);
        assert_eq!(
            words_to_offer(case["before"].as_str().unwrap(), after, &listed),
            strings(&case["offer"]),
            "after {after:?}"
        );
    }
}

#[test]
fn vocabulary_add_word_appends_the_core_and_remove_takes_exactly_it_back() {
    let dir = TempDir::new("summary-vocabulary");
    let (svc, _clock) = services(&dir);
    replace_words(&svc, &["QXR-7", "Velmora Ridge"]).unwrap();
    let added = add_word(&svc, "(Quillmarsh),").unwrap();
    assert_eq!(
        added,
        AddedWord {
            word: "Quillmarsh".into(),
            added: true
        }
    );
    assert_eq!(
        vocabulary(&svc.store()).unwrap().0.words(),
        ["QXR-7", "Velmora Ridge", "Quillmarsh"]
    );
    // Already there in another case: nothing written, the stored spelling reported.
    let again = add_word(&svc, "quillmarsh").unwrap();
    assert_eq!(
        again,
        AddedWord {
            word: "Quillmarsh".into(),
            added: false
        }
    );
    assert_eq!(vocabulary(&svc.store()).unwrap().0.words().len(), 3);
    // Undo: exactly that entry goes, case ignored; the others keep their order.
    assert!(remove_word(&svc, " QUILLMARSH ").unwrap());
    assert_eq!(
        vocabulary(&svc.store()).unwrap().0.words(),
        ["QXR-7", "Velmora Ridge"]
    );
    // A word only inside a phrase is not an entry: nothing goes.
    assert!(!remove_word(&svc, "Velmora").unwrap());
    assert!(!remove_word(&svc, "Quillmarsh").unwrap());
    assert!(!remove_word(&svc, "   ").unwrap());
    assert_eq!(
        vocabulary(&svc.store()).unwrap().0.words(),
        ["QXR-7", "Velmora Ridge"]
    );
    // A phrase entry is removed whole when named whole.
    assert!(remove_word(&svc, "velmora  ridge").unwrap());
    assert_eq!(vocabulary(&svc.store()).unwrap().0.words(), ["QXR-7"]);
}

#[test]
fn vocabulary_add_word_refuses_a_token_that_is_not_a_word_and_stores_nothing() {
    let dir = TempDir::new("summary-vocabulary");
    let (svc, _clock) = services(&dir);
    replace_words(&svc, &["QXR-7"]).unwrap();
    for token in ["42", "--", "", "  "] {
        let err = add_word(&svc, token).unwrap_err();
        assert!(matches!(err, SetVocabularyError::NotAWord), "{token:?}");
    }
    assert_eq!(vocabulary(&svc.store()).unwrap().0.words(), ["QXR-7"]);
}

#[test]
fn vocabulary_add_word_keeps_the_dictionary_page_limits_and_messages() {
    let dir = TempDir::new("summary-vocabulary");
    let (svc, _clock) = services(&dir);
    let full: Vec<String> = (0..100).map(|i| format!("w{i}")).collect();
    replace_words(&svc, &full).unwrap();
    let err = add_word(&svc, "Quillmarsh").unwrap_err();
    assert!(matches!(
        err,
        SetVocabularyError::Refused(VocabularyError::TooManyWords(101))
    ));
    assert_eq!(
        err.to_string(),
        "the list can hold at most 100 words; this one has 101"
    );
    assert_eq!(vocabulary(&svc.store()).unwrap().0.words(), full.as_slice());

    // A word too long, and a list too long in all: the page's own messages.
    let long = "q".repeat(MAX_VOCABULARY_WORD_CHARS + 1);
    replace_words(&svc, &["QXR-7"]).unwrap();
    let err = add_word(&svc, &long).unwrap_err();
    assert_eq!(
        err.to_string(),
        "a word can have at most 40 characters; this one has 41"
    );
    let near: Vec<String> = (0..25)
        .map(|i| format!("{i:02}{}", "q".repeat(38)))
        .collect();
    replace_words(&svc, &near).unwrap();
    let err = add_word(&svc, "Quillmarsh").unwrap_err();
    assert_eq!(
        err.to_string(),
        "the list can have at most 1000 characters in all; this one has 1010"
    );
    assert_eq!(vocabulary(&svc.store()).unwrap().0.words(), near.as_slice());
}

#[test]
fn vocabulary_add_word_over_a_refused_stored_list_holds_the_word_alone() {
    let dir = TempDir::new("summary-vocabulary");
    let (svc, _clock) = services(&dir);
    svc.store().set_local_value(VOCABULARY_KEY, b"{}").unwrap();
    assert!(add_word(&svc, "Quillmarsh").unwrap().added);
    assert_eq!(
        vocabulary(&svc.store()).unwrap(),
        (Vocabulary::new(["Quillmarsh"]).unwrap(), None)
    );
}

#[test]
fn vocabulary_words_to_offer_are_the_new_uncommon_unlisted_words_of_the_edit() {
    let listed = ["Zorbek", "Tamsin"];
    // Written: "Quillmarsh" (new), "zorbek" (listed, case ignored), "the" (common),
    // "42" (not a word), "Ridge" twice in another case (once).
    let offer = words_to_offer(
        "ask the quill marsh team, then ship",
        "ask the Quillmarsh team (Ridge), zorbek, 42 then ridge ship",
        &listed,
    );
    assert_eq!(offer, ["Quillmarsh", "Ridge"]);
    // A word already in the line before the edit is not offered, wherever it moved, when only
    // the punctuation around it changed.
    assert!(words_to_offer("Velmora said yes", "yes, said Velmora.", &listed).is_empty());
    // Changing only a word's case writes a new word.
    assert_eq!(
        words_to_offer("velmora said yes", "Velmora said yes", &listed),
        ["Velmora"]
    );
    // Nothing written: nothing offered.
    assert!(words_to_offer("ask Velmora", "ask Velmora", &listed).is_empty());
    assert!(words_to_offer("ask Velmora", "ask", &listed).is_empty());
}

#[test]
fn vocabulary_writes_keep_one_synced_row_per_word_with_sync_off() {
    use earshot_core::model::DictionaryRepo;
    let dir = TempDir::new("summary-vocabulary");
    let (svc, _clock) = services(&dir);
    let rows = || -> Vec<(String, bool)> {
        svc.store()
            .dictionary_words()
            .unwrap()
            .into_iter()
            .map(|w| (w.word, w.meta.deleted))
            .collect()
    };
    replace_words(&svc, &["Quillmarsh", "QXR-7"]).unwrap();
    assert_eq!(
        rows(),
        [("Quillmarsh".into(), false), ("QXR-7".into(), false)]
    );
    add_word(&svc, "Velmora").unwrap();
    assert!(remove_word(&svc, "qxr-7").unwrap());
    // A removal is a tombstone with its word cleared; the list is the device's own, as written.
    assert_eq!(
        rows(),
        [
            ("Quillmarsh".into(), false),
            (String::new(), true),
            ("Velmora".into(), false)
        ]
    );
    assert_eq!(
        vocabulary(&svc.store()).unwrap().0.words(),
        ["Quillmarsh", "Velmora"]
    );
    // Writing the same words again writes no row; every row waits to be sent.
    replace_words(&svc, &["Velmora", "Quillmarsh"]).unwrap();
    assert_eq!(rows().len(), 3);
    assert_eq!(
        vocabulary(&svc.store()).unwrap().0.words(),
        ["Velmora", "Quillmarsh"],
        "with sync off the user's order is kept"
    );
    assert_eq!(svc.store().outbox_len().unwrap(), 3);
    assert_eq!(dictionary_state(&svc.store()).unwrap().not_in_use, 0);
}

// Quotes around a Dictionary word (T12.78): one rule ([`without_end_quotes`]) for every path.

/// The Dictionary page's adds and removals, with sync off and on.
fn quoted_adds_take_the_word_inside(sync: bool) {
    let dir = TempDir::new("summary-vocabulary");
    let (svc, _clock) = services(&dir);
    if sync {
        crate::sync::set_sync_on(&svc, true).unwrap();
        assert!(dictionary_syncs(&svc.store()).unwrap());
    }
    let words = |svc: &Services| vocabulary(&svc.store()).unwrap().0.words().to_vec();
    for (typed, word) in [
        ("'test'", "test"),
        ("\"Quillmarsh\"", "Quillmarsh"),
        ("\u{2018}Velmora\u{2019}", "Velmora"),
        ("\u{201c}Zorbek\u{201d}", "Zorbek"),
        ("  \"'QXR-7'\"  ", "QXR-7"),
        ("O'Brien", "O'Brien"),
        ("'rock'n'roll'", "rock'n'roll"),
        ("\"Velmora Ridge\"", "Velmora Ridge"),
    ] {
        let added = add_entry(&svc, typed).unwrap();
        assert_eq!(
            (added.word.as_str(), added.added),
            (word, true),
            "sync {sync}: {typed:?}"
        );
    }
    assert_eq!(
        words(&svc),
        [
            "test",
            "Quillmarsh",
            "Velmora",
            "Zorbek",
            "QXR-7",
            "O'Brien",
            "rock'n'roll",
            "Velmora Ridge"
        ],
        "sync {sync}"
    );
    // The same word in other quotes is already there.
    for typed in ["test", "\u{201c}TEST\u{201d}", "'Test'"] {
        let again = add_entry(&svc, typed).unwrap();
        assert_eq!((again.word.as_str(), again.added), ("test", false));
    }
    // An entry that is only quotes is not a word.
    for typed in ["''", "\"\"", "\u{2018}\u{2019}", " \u{201c} \u{201d} "] {
        assert!(
            matches!(add_entry(&svc, typed), Err(SetVocabularyError::NotAWord)),
            "sync {sync}: {typed:?}"
        );
    }
    // A removal named in quotes takes the word.
    assert!(remove_entry(&svc, "\u{2018}test\u{2019}").unwrap());
    assert!(remove_entry(&svc, "\"o'brien\"").unwrap());
    assert_eq!(
        words(&svc),
        [
            "Quillmarsh",
            "Velmora",
            "Zorbek",
            "QXR-7",
            "rock'n'roll",
            "Velmora Ridge"
        ]
    );
    // Rows hold the word without its quotes, and the list joined from them keeps every word.
    let rows: Vec<String> = svc
        .store()
        .dictionary_words()
        .unwrap()
        .into_iter()
        .filter(DictionaryWord::is_live)
        .map(|r| r.word)
        .collect();
    assert_eq!(rows, words(&svc));
    assert_eq!(dictionary_state(&svc.store()).unwrap().not_in_use, 0);
}

#[test]
fn vocabulary_quotes_around_a_typed_word_are_dropped_with_sync_off() {
    quoted_adds_take_the_word_inside(false);
}

#[test]
fn vocabulary_quotes_around_a_typed_word_are_dropped_with_sync_on() {
    quoted_adds_take_the_word_inside(true);
}

#[test]
fn vocabulary_a_word_stored_in_double_quotes_reads_matches_and_is_removed() {
    for sync in [false, true] {
        let dir = TempDir::new("summary-vocabulary");
        let (svc, _clock) = services(&dir);
        // As 0.3.11 stored a typed `"test"` (quotes kept), and a curly-quoted one.
        svc.store()
            .set_local_value(
                VOCABULARY_KEY,
                "[\"\\\"test\\\"\", \"\u{201c}Zorbek\u{201d}\", \"QXR-7\"]".as_bytes(),
            )
            .unwrap();
        if sync {
            // Turning sync on makes rows for the stored words and rebuilds the list from them.
            crate::sync::set_sync_on(&svc, true).unwrap();
        }
        let (words, refused) = vocabulary(&svc.store()).unwrap();
        assert_eq!(refused, None);
        assert_eq!(words.words(), ["test", "Zorbek", "QXR-7"], "sync {sync}");
        assert_eq!(dictionary_state(&svc.store()).unwrap().not_in_use, 0);
        // A typed `test` matches it: not added twice.
        let again = add_entry(&svc, "test").unwrap();
        assert_eq!((again.word.as_str(), again.added), ("test", false));
        // It is removed as `test`, its row with it.
        assert!(remove_entry(&svc, "test").unwrap(), "sync {sync}");
        assert_eq!(
            vocabulary(&svc.store()).unwrap().0.words(),
            ["Zorbek", "QXR-7"],
            "sync {sync}"
        );
        let live: Vec<String> = svc
            .store()
            .dictionary_words()
            .unwrap()
            .into_iter()
            .filter(DictionaryWord::is_live)
            .map(|r| r.word)
            .collect();
        assert!(
            live.iter().all(|w| !w.to_lowercase().contains("test")),
            "sync {sync}: {live:?}"
        );
        // And it can be added again.
        assert!(add_entry(&svc, "'test'").unwrap().added, "sync {sync}");
    }
}

#[test]
fn vocabulary_a_stored_entry_still_reads_through_a_leading_tab() {
    let dir = TempDir::new("summary-vocabulary");
    let (svc, _clock) = services(&dir);
    // Written raw: readers trim every white space before the quotes, as before T12.78.
    svc.store()
        .set_local_value(VOCABULARY_KEY, b"[\"\\tQuillmarsh\", \"'QXR-7'\\t\"]")
        .unwrap();
    let (words, refused) = vocabulary(&svc.store()).unwrap();
    assert_eq!(refused, None);
    assert_eq!(words.words(), ["Quillmarsh", "QXR-7"]);
    // A row stored the same way is read and matched too.
    with_stamp(&svc, |tx, stamp| -> Result<(), StoreError> {
        Ok(tx.put_dictionary_word(DictionaryWord::new("\tVelmora", stamp))?)
    })
    .unwrap();
    crate::sync::set_sync_on(&svc, true).unwrap();
    assert_eq!(
        vocabulary(&svc.store()).unwrap().0.words(),
        ["Velmora", "Quillmarsh", "QXR-7"]
    );
    assert!(!add_entry(&svc, "velmora").unwrap().added);
}

#[test]
fn vocabulary_a_row_of_only_quotes_from_0_3_11_is_tombstoned() {
    for sync in [false, true] {
        let dir = TempDir::new("summary-vocabulary");
        let (svc, _clock) = services(&dir);
        replace_words(&svc, &["Quillmarsh", "QXR-7"]).unwrap();
        // As 0.3.11 made a row of a typed `"`.
        with_stamp(&svc, |tx, stamp| -> Result<(), StoreError> {
            tx.put_dictionary_word(DictionaryWord::new("\"", stamp))?;
            Ok(tx.put_dictionary_word(DictionaryWord::new(" \u{201c}' ", stamp))?)
        })
        .unwrap();
        if sync {
            crate::sync::set_sync_on(&svc, true).unwrap();
        } else {
            reconcile(&svc).unwrap();
        }
        let rows: Vec<(String, bool)> = svc
            .store()
            .dictionary_words()
            .unwrap()
            .into_iter()
            .map(|r| (r.word, r.meta.deleted))
            .collect();
        assert_eq!(
            rows,
            [
                ("Quillmarsh".into(), false),
                ("QXR-7".into(), false),
                (String::new(), true),
                (String::new(), true)
            ],
            "sync {sync}"
        );
        assert_eq!(
            vocabulary(&svc.store()).unwrap().0.words(),
            ["Quillmarsh", "QXR-7"],
            "sync {sync}"
        );
        assert_eq!(dictionary_state(&svc.store()).unwrap().not_in_use, 0);
    }
}

#[test]
fn vocabulary_without_end_quotes_keeps_inner_quotes_and_control_characters() {
    assert_eq!(without_end_quotes("'test'"), "test");
    assert_eq!(without_end_quotes(" \"' test '\" "), "test");
    assert_eq!(without_end_quotes("\u{2018}\u{201c}x\u{201d}\u{2019}"), "x");
    assert_eq!(without_end_quotes("O'Brien"), "O'Brien");
    assert_eq!(without_end_quotes("rock'n'roll"), "rock'n'roll");
    assert_eq!(without_end_quotes("say \"hi\" now"), "say \"hi\" now");
    assert_eq!(without_end_quotes("''"), "");
    assert_eq!(without_end_quotes("\u{201c}\u{201d}"), "");
    // A control character is never trimmed away, so the list's own check still refuses it.
    assert_eq!(without_end_quotes("\tword'"), "\tword");
    let dir = TempDir::new("summary-vocabulary");
    let (svc, _clock) = services(&dir);
    assert!(matches!(
        add_entry(&svc, "'\tword'"),
        Err(SetVocabularyError::Refused(
            VocabularyError::ControlCharacter { .. }
        ))
    ));
}
