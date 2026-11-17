//! A naming change marks the summary out of date when the summary uses the old name (T12.82).
//! Every name contains `edit`.

use super::*;

/// The meeting as the detail reads it, with the summaries that use a changed name.
fn named(svc: &Services, meeting: EntityId) -> MeetingView {
    MeetingView::load_named(&svc.store(), meeting)
        .unwrap()
        .unwrap()
}

#[test]
fn edit_names_a_rename_of_a_name_the_summary_uses_marks_it_and_a_resummarise_clears_it() {
    let dir = TempDir::new("edit");
    let (svc, _) = services(&dir);
    let m = meeting(&svc, "Release", 820);
    let alice = speaker(&svc, m, "Alice").id;
    let first = store(&svc, m, "Alice will send the release plan.", 821);
    assert!(!named(&svc, m).summary_out_of_date());

    assert!(rename_speaker(&svc, m, alice, "Alicia").unwrap());
    let v = named(&svc, m);
    assert!(v.summary_out_of_date());
    assert!(v.summary_names_changed() && !v.summary_transcript_changed());
    assert_eq!(
        v.summary.as_ref().unwrap().id,
        first,
        "nothing was replaced"
    );

    // A re-summarise is current again.
    let second = store(&svc, m, "Alicia will send the release plan.", 822);
    assert!(!named(&svc, m).summary_out_of_date());
    // An undo brings back the summary that used the old name, still marked.
    assert_eq!(undo_summary(&svc, m).unwrap(), first);
    assert!(named(&svc, m).summary_names_changed());
    assert_eq!(undo_summary(&svc, m).unwrap(), second);
    assert!(!named(&svc, m).summary_out_of_date());

    // Deleting the meeting erases the record.
    delete(&svc, m);
    assert!(
        svc.store()
            .local_value(crate::summary_names::record_key(m).as_str())
            .unwrap()
            .is_none()
    );
}

#[test]
fn edit_names_a_rename_of_a_name_the_summary_does_not_use_leaves_it() {
    let dir = TempDir::new("edit");
    let (svc, _) = services(&dir);
    let m = meeting(&svc, "Release", 823);
    let alice = speaker(&svc, m, "Alice").id;
    store(&svc, m, "The team agreed to ship on Friday.", 824);
    assert!(rename_speaker(&svc, m, alice, "Alicia").unwrap());
    assert!(!named(&svc, m).summary_out_of_date());
    // A change of case alone is not a change of name.
    let other = meeting(&svc, "Review", 825);
    let a = speaker(&svc, other, "Alice").id;
    store(&svc, other, "Alice will check the figures.", 826);
    assert!(rename_speaker(&svc, other, a, "ALICE").unwrap());
    assert!(!named(&svc, other).summary_out_of_date());
    // Case folded: the summary's "ALICE" is the old name.
    assert!(rename_speaker(&svc, other, a, "Alison").unwrap());
    let v = named(&svc, other);
    assert!(v.summary_names_changed());
}

#[test]
fn edit_names_a_name_inside_another_word_does_not_count() {
    let dir = TempDir::new("edit");
    let (svc, _) = services(&dir);
    let m = meeting(&svc, "Release", 827);
    let alice = speaker(&svc, m, "Alice").id;
    store(
        &svc,
        m,
        "Alicetown and the Malice report were discussed.",
        828,
    );
    assert!(rename_speaker(&svc, m, alice, "Alicia").unwrap());
    assert!(!named(&svc, m).summary_out_of_date());
}

#[test]
fn edit_names_link_rename_person_and_not_this_person_each_mark_a_summary_using_the_old_name() {
    let dir = TempDir::new("edit");
    let (svc, _) = services(&dir);
    let m = processed(&svc, 829);
    let s = remote_speakers(&svc, m)[0].clone();
    let old = s.display_name.clone();
    assert!(old.starts_with("Speaker "), "{old}");

    store(&svc, m, &format!("{old} will book the venue."), 830);
    let rowan = link_speaker(
        &svc,
        m,
        s.id,
        &PersonChoice::New {
            name: "Rowan".into(),
        },
    )
    .unwrap();
    assert!(named(&svc, m).summary_names_changed(), "link");

    store(&svc, m, "Rowan will book the venue.", 831);
    assert!(!named(&svc, m).summary_out_of_date());
    assert_eq!(rename_person(&svc, rowan, "Rowena").unwrap(), 1);
    assert!(named(&svc, m).summary_names_changed(), "rename person");

    store(&svc, m, "Rowena will book the venue.", 832);
    assert!(!named(&svc, m).summary_out_of_date());
    not_this_person(&svc, m, s.id).unwrap();
    assert!(named(&svc, m).summary_names_changed(), "Not Rowena");

    // A summary that never named the voice is left alone by every change.
    store(&svc, m, "The venue was booked.", 833);
    link_speaker(&svc, m, s.id, &PersonChoice::Existing(rowan)).unwrap();
    assert!(!named(&svc, m).summary_out_of_date());
    assert_eq!(rename_person(&svc, rowan, "Rowan").unwrap(), 1);
    assert!(!named(&svc, m).summary_out_of_date());
}
