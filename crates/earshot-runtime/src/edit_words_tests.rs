//! Suggestions beside words (T12.15ec): the word list, the stored choices and the edits.
//! Every name contains `edit`; every word is invented.

use earshot_core::asr::CancelToken;
use earshot_llm_replay::mock::{MockResponse, MockServer};

use super::*;
use crate::word_choices::{
    ChangedWord, LineWords, WordChoice, WordSuggestion, decode, record_key, word_choices,
    word_list, word_marks,
};

fn set_words(svc: &Services, words: &[&str]) {
    crate::vocabulary::set_vocabulary(&mut svc.store(), words).unwrap();
}

/// Puts `text` into line `id` as the recogniser would (no edit mark).
fn decoded(svc: &Services, meeting: EntityId, id: EntityId, text: &str) {
    let clock = Arc::clone(&svc.clock);
    let mut db = svc.store();
    let mut row = db
        .segments(meeting)
        .unwrap()
        .into_iter()
        .find(|r| r.segment.id == id)
        .unwrap();
    let (store, hlc) = db.parts();
    row.segment.text = text.into();
    row.meta.touch(hlc, &*clock);
    store.put_segment(row).unwrap();
}

fn text_of(svc: &Services, meeting: EntityId, id: EntityId) -> String {
    view(svc, meeting)
        .segments
        .into_iter()
        .find(|s| s.id == id)
        .unwrap()
        .text
}

fn marks(svc: &Services, meeting: EntityId) -> crate::word_choices::WordMarks {
    let db = svc.store();
    let v = MeetingView::load(&**db, meeting).unwrap().unwrap();
    word_marks(&db, &v).unwrap()
}

fn line(svc: &Services, meeting: EntityId, id: EntityId) -> LineWords {
    marks(svc, meeting).lines.remove(&id).unwrap_or_default()
}

fn suggestion(token: usize, decoded: &str, listed: &str) -> WordSuggestion {
    WordSuggestion {
        token,
        decoded: decoded.into(),
        listed: listed.into(),
    }
}

fn changed(token: usize, decoded: &str, written: &str) -> ChangedWord {
    ChangedWord {
        token,
        decoded: decoded.into(),
        written: written.into(),
    }
}

fn stored(svc: &Services, meeting: EntityId) -> Vec<WordChoice> {
    word_choices(&svc.store(), meeting).unwrap()
}

/// A meeting whose second line reads "ask sorbeck, then ship" as decoded, with "Zorbek" on
/// the Dictionary: one suggestion at token 1.
fn suggesting(svc: &Services, seed: u64) -> (EntityId, EntityId) {
    let m = meeting(svc, "Planning", seed);
    let id = view(svc, m).segments[1].id;
    decoded(svc, m, id, "ask sorbeck, then ship");
    set_words(svc, &["Zorbek"]);
    (m, id)
}

#[test]
fn edit_word_list_is_the_dictionary_then_saved_names_without_repeats() {
    let dir = TempDir::new("edit");
    let (svc, _) = services(&dir);
    let m = meeting(&svc, "Planning", 900);
    set_words(&svc, &["Zorbek", "Quillan  Marsh", "vantrel"]);
    // The default "Me" is not listed.
    assert_eq!(
        word_list(&svc.store()).unwrap(),
        ["Zorbek", "Quillan", "Marsh", "vantrel"]
    );
    let alice = speaker(&svc, m, "Alice");
    let person = link_speaker(
        &svc,
        m,
        alice.id,
        &PersonChoice::New {
            name: "Vantrel Oskarine".into(),
        },
    )
    .unwrap();
    crate::my_voice::set_my_name(&svc, "Tamsin Brevik").unwrap();
    // Dictionary first; a saved name's word that repeats one in another case is dropped;
    // people by name; the user's own name once they set one.
    assert_eq!(
        word_list(&svc.store()).unwrap(),
        [
            "Zorbek", "Quillan", "Marsh", "vantrel", "Tamsin", "Brevik", "Oskarine"
        ]
    );
    // A removed person is no longer listed.
    remove_person(&svc, person).unwrap();
    assert_eq!(
        word_list(&svc.store()).unwrap(),
        ["Zorbek", "Quillan", "Marsh", "vantrel", "Tamsin", "Brevik"]
    );
}

#[test]
fn edit_word_marks_suggest_on_the_final_transcript_only() {
    let dir = TempDir::new("edit");
    let (svc, _) = services(&dir);
    let (m, id) = suggesting(&svc, 901);
    let got = marks(&svc, m);
    assert_eq!(got.list, ["Zorbek"]);
    assert_eq!(
        got.lines.get(&id).unwrap().suggestions,
        [suggestion(1, "sorbeck", "Zorbek")]
    );
    assert_eq!(got.lines.len(), 1, "no other line has a mark");

    // A token already equal to a listed word (any case) is not suggested.
    decoded(&svc, m, id, "ask zorbek, then ship");
    assert!(marks(&svc, m).lines.is_empty());

    // A draft transcript has no marks and no list.
    decoded(&svc, m, id, "ask sorbeck, then ship");
    {
        let clock = Arc::clone(&svc.clock);
        let mut db = svc.store();
        let rows = db.segments(m).unwrap();
        let (store, hlc) = db.parts();
        for mut r in rows {
            r.pass = TranscriptPass::Live;
            r.meta.touch(hlc, &*clock);
            store.put_segment(r).unwrap();
        }
    }
    assert_eq!(marks(&svc, m), crate::word_choices::WordMarks::default());
    assert_eq!(
        refused(keep_word(&svc, m, id, 1)),
        EditError::DraftTranscript(m)
    );
    assert_eq!(
        refused(use_word(&svc, m, id, 1, "Zorbek")),
        EditError::DraftTranscript(m)
    );
}

#[test]
fn edit_use_word_then_put_back_gives_the_original_text() {
    let dir = TempDir::new("edit");
    let (svc, _) = services(&dir);
    let (m, id) = suggesting(&svc, 902);
    let summary = crate::summary::store_summary(
        &svc,
        &view(&svc, m),
        "test-model".into(),
        &summary_citing("A summary before the change.", id),
        &mut ChaCha8Rng::seed_from_u64(903),
    )
    .unwrap()
    .0;
    assert!(!view(&svc, m).summary_out_of_date());

    // Any case of a listed word; the listed form is written, the punctuation kept.
    assert!(use_word(&svc, m, id, 1, " zorbek ").unwrap());
    assert_eq!(text_of(&svc, m, id), "ask Zorbek, then ship");
    let v = view(&svc, m);
    assert!(v.edited.contains_key(&id), "the line is marked edited");
    assert!(v.summary_out_of_date(), "a summary older than the edit");
    assert_eq!(v.summary.as_ref().unwrap().id, summary, "nothing replaced");
    assert_eq!(
        line(&svc, m, id),
        LineWords {
            suggestions: vec![],
            changed: vec![changed(1, "sorbeck", "Zorbek")],
        }
    );
    // The same word again changes nothing.
    assert!(!use_word(&svc, m, id, 1, "Zorbek").unwrap());

    assert!(put_back_word(&svc, m, id, 1).unwrap());
    assert_eq!(text_of(&svc, m, id), "ask sorbeck, then ship");
    assert!(stored(&svc, m).is_empty());
    assert_eq!(
        svc.store().local_value(record_key(m).as_str()).unwrap(),
        None,
        "no choice left: the record is erased"
    );
    // Back to a suggestion; nothing left to put back.
    assert_eq!(
        line(&svc, m, id).suggestions,
        [suggestion(1, "sorbeck", "Zorbek")]
    );
    assert_eq!(
        refused(put_back_word(&svc, m, id, 1)),
        EditError::NoWordChoice(id)
    );
}

#[test]
fn edit_use_word_another_listed_word_keeps_the_decoded_word() {
    let dir = TempDir::new("edit");
    let (svc, _) = services(&dir);
    let (m, id) = suggesting(&svc, 904);
    set_words(&svc, &["Zorbek", "Sorbelle"]);
    assert!(use_word(&svc, m, id, 1, "Zorbek").unwrap());
    assert!(use_word(&svc, m, id, 1, "Sorbelle").unwrap());
    assert_eq!(text_of(&svc, m, id), "ask Sorbelle, then ship");
    assert_eq!(
        line(&svc, m, id).changed,
        [changed(1, "sorbeck", "Sorbelle")]
    );
    assert_eq!(stored(&svc, m).len(), 1);
    assert!(put_back_word(&svc, m, id, 1).unwrap());
    assert_eq!(text_of(&svc, m, id), "ask sorbeck, then ship");
}

#[test]
fn edit_use_word_refuses_an_unlisted_word_and_a_missing_word() {
    let dir = TempDir::new("edit");
    let (svc, _) = services(&dir);
    let (m, id) = suggesting(&svc, 905);
    decoded(&svc, m, id, "ask sorbeck - then ship");
    let e = use_word(&svc, m, id, 1, "Glimmerax").unwrap_err();
    assert!(
        matches!(e, RuntimeError::Edit(EditError::WordNotListed)),
        "{e:?}"
    );
    // The refusal names no word.
    assert!(!e.to_string().contains("Glimmerax"), "{e}");
    assert!(!format!("{e:?}").contains("Glimmerax"));
    for token in [2, 99] {
        let e = refused(use_word(&svc, m, id, token, "Zorbek"));
        assert_eq!(e, EditError::WordNotFound(id), "token {token}");
        assert!(!e.to_string().contains("sorbeck"));
        assert_eq!(
            refused(keep_word(&svc, m, id, token)),
            EditError::WordNotFound(id)
        );
    }
    let other = meeting(&svc, "Other", 906);
    let theirs = view(&svc, other).segments[0].id;
    assert_eq!(
        refused(use_word(&svc, m, theirs, 0, "Zorbek")),
        EditError::SegmentNotFound(theirs)
    );
    assert_eq!(text_of(&svc, m, id), "ask sorbeck - then ship");
    assert!(stored(&svc, m).is_empty(), "nothing recorded on a refusal");
    for e in [
        EditError::WordNotFound(id),
        EditError::WordNotListed,
        EditError::NoWordChoice(id),
    ] {
        let shown = e.to_string();
        assert!(
            !shown.contains("sorbeck") && !shown.contains("Zorbek"),
            "{shown}"
        );
    }
}

#[test]
fn edit_keep_word_hides_the_suggestion_until_the_line_changes_under_it() {
    let dir = TempDir::new("edit");
    let (svc, _) = services(&dir);
    let (m, id) = suggesting(&svc, 907);
    let before = view(&svc, m);
    assert!(keep_word(&svc, m, id, 1).unwrap());
    assert!(!keep_word(&svc, m, id, 1).unwrap(), "kept already");
    assert_eq!(line(&svc, m, id), LineWords::default());
    let after = view(&svc, m);
    assert_eq!(after.edited, before.edited, "the line is not written");
    assert_eq!(text_of(&svc, m, id), "ask sorbeck, then ship");
    assert_eq!(stored(&svc, m).len(), 1);
    // A kept word cannot be put back, and a used word at a kept token is allowed (it was
    // the user's own choice to make).
    assert_eq!(
        refused(put_back_word(&svc, m, id, 1)),
        EditError::NoWordChoice(id)
    );

    // A hand edit that moves the word: the choice no longer reads as recorded, so it is
    // ignored, and the next write drops it.
    assert!(edit_segment(&svc, m, id, "please ask sorbeck, then ship").unwrap());
    assert_eq!(
        line(&svc, m, id).suggestions,
        [suggestion(2, "sorbeck", "Zorbek")]
    );
    assert!(keep_word(&svc, m, id, 2).unwrap());
    let left = stored(&svc, m);
    assert_eq!(left.len(), 1);
    assert_eq!(left[0].token, 2);
}

#[test]
fn edit_keep_word_refuses_a_word_with_no_suggestion() {
    let dir = TempDir::new("edit");
    let (svc, _) = services(&dir);
    let (m, id) = suggesting(&svc, 911);
    // "ask sorbeck, then ship": only token 1 is suggested.
    for token in [0, 2, 3] {
        assert_eq!(
            refused(keep_word(&svc, m, id, token)),
            EditError::WordNotFound(id),
            "token {token}"
        );
    }
    assert!(stored(&svc, m).is_empty(), "nothing recorded on a refusal");
    // With the word off the list, the suggestion is gone too.
    set_words(&svc, &["Quillan"]);
    assert_eq!(
        refused(keep_word(&svc, m, id, 1)),
        EditError::WordNotFound(id)
    );
    set_words(&svc, &["Zorbek"]);
    assert!(keep_word(&svc, m, id, 1).unwrap());
    assert_eq!(stored(&svc, m).len(), 1);
}

#[test]
fn edit_word_choices_unreadable_record_reads_as_none() {
    let dir = TempDir::new("edit");
    let (svc, _) = services(&dir);
    let (m, id) = suggesting(&svc, 908);
    crate::local::set(&mut svc.store(), &record_key(m), b"\x07\0\0\0[damaged").unwrap();
    assert!(stored(&svc, m).is_empty());
    assert_eq!(
        line(&svc, m, id).suggestions,
        [suggestion(1, "sorbeck", "Zorbek")]
    );
    assert!(decode(b"").is_empty() && decode(b"\x01\0\0\0not json").is_empty());
    // The next choice replaces it.
    assert!(keep_word(&svc, m, id, 1).unwrap());
    assert_eq!(stored(&svc, m).len(), 1);
}

#[test]
fn edit_word_choices_are_erased_with_the_meeting() {
    let dir = TempDir::new("edit");
    let (svc, _) = services(&dir);
    let (m, id) = suggesting(&svc, 909);
    assert!(use_word(&svc, m, id, 1, "Zorbek").unwrap());
    assert!(
        svc.store()
            .local_value(record_key(m).as_str())
            .unwrap()
            .is_some()
    );
    delete(&svc, m);
    assert_eq!(
        svc.store().local_value(record_key(m).as_str()).unwrap(),
        None
    );
}

#[test]
fn edit_retranscribe_keeps_a_changed_word_and_drops_a_kept_one() {
    let dir = TempDir::new("edit");
    let (svc, _) = services(&dir);
    let m = processed(&svc, 910);
    let v = view(&svc, m);
    // The fakes decode "first" and "second" on the far end, every run.
    let id_of = |t: &str| v.segments.iter().find(|s| s.text == t).unwrap().id;
    let (first, second) = (id_of("first"), id_of("second"));
    set_words(&svc, &["Fyrstan", "Sekondo"]);
    assert!(use_word(&svc, m, first, 0, "Fyrstan").unwrap());
    // Only a suggested word can be kept, and "second" is a common word: the line reads as
    // the recogniser might have heard the listed word.
    decoded(&svc, m, second, "sekonda");
    assert_eq!(
        line(&svc, m, second).suggestions,
        [suggestion(0, "sekonda", "Sekondo")]
    );
    assert!(keep_word(&svc, m, second, 0).unwrap());
    assert_eq!(stored(&svc, m).len(), 2);

    let again = retranscribe_with(&svc, m, &CancelToken::new(), fake_post, &mut |_| {}).unwrap();
    assert!(matches!(again.post, HostPost::Done(_)), "{:?}", again.post);
    let v = view(&svc, m);
    let texts: Vec<&str> = v.segments.iter().map(|s| s.text.as_str()).collect();
    assert_eq!(texts, ["me", "Fyrstan", "second"]);
    assert!(
        !v.segments.iter().any(|s| s.id == second),
        "the line was replaced"
    );
    // The final pass drops the replaced line's choice (transcript words) in its own write.
    let left = stored(&svc, m);
    assert_eq!(left.len(), 1, "{left:?}");
    assert_eq!((left[0].segment, left[0].token), (first, 0));
    // The changed word survives with its line; the kept one went with its line, harmlessly.
    assert_eq!(
        line(&svc, m, first).changed,
        [changed(0, "first", "Fyrstan")]
    );
    assert!(put_back_word(&svc, m, first, 0).unwrap());
    assert_eq!(text_of(&svc, m, first), "first");
    assert!(
        stored(&svc, m).is_empty(),
        "the stale kept choice is dropped"
    );
}

#[test]
fn edit_word_list_never_reaches_a_summary_request() {
    let dir = TempDir::new("edit");
    let (svc, _) = services(&dir);
    let m = meeting(&svc, "", 911);
    // A saved person who is not in this meeting, from another meeting.
    let elsewhere = meeting(&svc, "Elsewhere", 912);
    let alice = speaker(&svc, elsewhere, "Alice");
    link_speaker(
        &svc,
        elsewhere,
        alice.id,
        &PersonChoice::New {
            name: "Vantrel Oskarine".into(),
        },
    )
    .unwrap();
    let list = word_list(&svc.store()).unwrap();
    assert!(list.iter().any(|w| w == "Oskarine"), "{list:?}");

    let reply = earshot_core::summary::SummaryV1 {
        title: "Release planning".into(),
        tldr: "The team agreed to ship on Friday.".into(),
        enhanced_notes: vec![],
        decisions: vec![],
        action_items: vec![],
        open_questions: vec![],
        topics: vec![],
    };
    let server = MockServer::start(vec![MockResponse::completion(
        &serde_json::to_string(&reply).unwrap(),
        "stop",
    )])
    .unwrap();
    let service = crate::summary::SummaryService::new(
        &crate::summary::SummaryConfig::new(server.endpoint().as_str(), "test-model"),
        svc.clock.clone(),
    )
    .unwrap();
    service
        .summarize(
            &svc,
            m,
            &mut ChaCha8Rng::seed_from_u64(913),
            &CancelToken::new(),
        )
        .unwrap();
    let requests = server.requests();
    assert!(!requests.is_empty());
    for r in requests {
        let body = String::from_utf8(r.body.clone()).unwrap();
        assert!(
            body.contains("friday works for the release"),
            "a real request"
        );
        assert!(!body.contains("Oskarine") && !body.contains("Vantrel"));
    }
}

#[test]
fn edit_segment_offering_vocabulary_offers_the_new_unlisted_words_and_adds_nothing() {
    let dir = TempDir::new("edit");
    let (svc, _) = services(&dir);
    let (m, id) = suggesting(&svc, 914);
    let alice = speaker(&svc, m, "Alice");
    link_speaker(
        &svc,
        m,
        alice.id,
        &PersonChoice::New {
            name: "Vantrel Oskarine".into(),
        },
    )
    .unwrap();
    // "ask sorbeck, then ship" -> Zorbek (Dictionary) and Oskarine (a saved name) are listed,
    // "the" is common, "Quillmarsh" and "Brevmoor" are new.
    let offer = edit_segment_offering(
        &svc,
        m,
        id,
        "  ask Zorbek, then ship the Quillmarsh kit to Oskarine (Brevmoor). ",
    )
    .unwrap();
    assert_eq!(offer, ["Quillmarsh", "Brevmoor"]);
    assert_eq!(
        text_of(&svc, m, id),
        "ask Zorbek, then ship the Quillmarsh kit to Oskarine (Brevmoor)."
    );
    // Nothing joined the Dictionary.
    assert_eq!(
        crate::vocabulary::vocabulary(&svc.store())
            .unwrap()
            .0
            .words(),
        ["Zorbek"]
    );
    // The same text again writes nothing and offers nothing.
    assert!(
        edit_segment_offering(
            &svc,
            m,
            id,
            "ask Zorbek, then ship the Quillmarsh kit to Oskarine (Brevmoor)."
        )
        .unwrap()
        .is_empty()
    );
    // The refusals are edit_segment's.
    assert_eq!(
        refused(edit_segment_offering(&svc, m, id, "  ")),
        refused(edit_segment(&svc, m, id, "  "))
    );
}
