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
use crate::testutil::{TempDir, meeting, services};

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
    let mut s = store(&dir);
    let saved = set_vocabulary(&mut s, &[" Quillmarsh ", "", "QXR-7", "quillmarsh"]).unwrap();
    assert_eq!(saved.words(), ["Quillmarsh", "QXR-7"]);
    assert_eq!(vocabulary(&s).unwrap(), (saved, None));
    assert_eq!(
        s.local_value(VOCABULARY_KEY).unwrap().as_deref(),
        Some(br#"["Quillmarsh","QXR-7"]"#.as_slice())
    );
    let cleared = set_vocabulary(&mut s, &["  ", ""]).unwrap();
    assert!(cleared.is_empty());
    assert_eq!(vocabulary(&s).unwrap(), (Vocabulary::default(), None));
    assert!(set_vocabulary::<&str>(&mut s, &[]).unwrap().is_empty());
}

#[test]
fn summary_vocabulary_refusals_store_nothing() {
    let dir = TempDir::new("summary-vocabulary");
    let mut s = store(&dir);
    set_vocabulary(&mut s, &["Quillmarsh"]).unwrap();
    let long = "q".repeat(MAX_VOCABULARY_WORD_CHARS + 1);
    let err = set_vocabulary(&mut s, &["QXR-7", long.as_str()]).unwrap_err();
    assert!(matches!(
        err,
        SetVocabularyError::Refused(VocabularyError::WordTooLong { line: 2, .. })
    ));
    assert_eq!(
        err.to_string(),
        "a word can have at most 40 characters; this one has 41"
    );
    let err = set_vocabulary(&mut s, &["a\nb"]).unwrap_err();
    assert!(matches!(
        err,
        SetVocabularyError::Refused(VocabularyError::ControlCharacter { line: 1 })
    ));
    assert_eq!(vocabulary(&s).unwrap().0.words(), ["Quillmarsh"]);
}

#[test]
fn summary_vocabulary_a_stored_value_this_version_refuses_reads_as_none_and_is_reported() {
    let dir = TempDir::new("summary-vocabulary");
    let mut s = store(&dir);
    let private_ish = serde_json::to_vec(&[format!("Quillmarsh{}", "x".repeat(60))]).unwrap();
    s.set_local_value(VOCABULARY_KEY, &private_ish).unwrap();
    let (words, refused) = vocabulary(&s).unwrap();
    assert!(words.is_empty());
    let refused = refused.expect("reported");
    assert_eq!(refused.key, VOCABULARY_KEY);
    assert!(!refused.to_string().contains("Quillmarsh"), "{refused}");
    assert!(refused.to_string().contains("40"), "{refused}");

    for bad in [&b"\xff\xfe"[..], b"\"Quillmarsh\"", b"[1, 2]", b"{}"] {
        s.set_local_value(VOCABULARY_KEY, bad).unwrap();
        let (words, refused) = vocabulary(&s).unwrap();
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
    let mut s = store(&dir);
    s.set_local_value(VOCABULARY_KEY, b"[\"bad\\u0007\"]")
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
    let setup =
        crate::host::summary_from_settings(&s, &cpu, &hw, &crate::summary_settings::NoApiKeys)
            .unwrap();
    assert!(
        setup.notes.iter().any(|n| n.contains(VOCABULARY_KEY)),
        "{:?}",
        setup.notes
    );
    // A readable list is no note.
    set_vocabulary(&mut s, &["QXR-7"]).unwrap();
    let setup =
        crate::host::summary_from_settings(&s, &cpu, &hw, &crate::summary_settings::NoApiKeys)
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
    set_vocabulary(&mut svc.store(), &["Quillmarsh", "QXR-7"]).unwrap();
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
    set_vocabulary(&mut svc.store(), &["Ostrelby Ridge"]).unwrap();
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
