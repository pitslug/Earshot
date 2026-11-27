//! Attendees leave out ignored voices (T14.68). Every name contains `view`; every name is
//! invented.

use earshot_core::capture::TrackId;
use earshot_core::diar::{Embedding, EmbeddingModelId, SpeakerRole};
use earshot_core::model::{MeetingRepo, Speaker};

use super::*;
use crate::Services;
use crate::identities::{IdentityRecord, SpeakerIdentity, encode, record_key};
use crate::local::DeviceRecord;
use crate::testutil::{TempDir, add_speaker, meeting, services};

fn named(svc: &Services, m: EntityId) -> MeetingView {
    MeetingView::load_named(&svc.store(), m).unwrap().unwrap()
}

/// A legacy (0.4.2) identity entry for `row` with its ignore flag set.
fn legacy_entry(row: &Speaker) -> SpeakerIdentity {
    SpeakerIdentity {
        speaker: row.id,
        track: TrackId::Reference,
        centroid: Embedding::new(EmbeddingModelId::new("m@1"), vec![0.6, 0.8]),
        given_name: row.display_name.clone(),
        given_person: row.person_id,
        given_stamp: Some(row.meta.updated_at),
        user_edited: false,
        enrolled: false,
        ignored: true,
        linked_before: Vec::new(),
    }
}

fn put_legacy(svc: &Services, m: EntityId, rows: &[&Speaker]) {
    let speakers = rows.iter().map(|r| legacy_entry(r)).collect();
    crate::local::set(
        &mut svc.store(),
        &record_key(m),
        &encode(&IdentityRecord { speakers }),
    )
    .unwrap();
}

fn label_of(view: &MeetingView, id: EntityId) -> String {
    view.labels()
        .into_iter()
        .find(|(s, _)| *s == id)
        .map(|(_, l)| l)
        .unwrap()
}

#[test]
fn view_attendees_leave_out_an_ignored_voice_and_keep_the_others() {
    let dir = TempDir::new("view");
    let (svc, _) = services(&dir);
    let m = meeting(&svc, "Invented planning", 1);
    let ignored = add_speaker(&svc, m, SpeakerRole::Remote, "Speaker 1", false, true, 2);
    let unnamed = add_speaker(&svc, m, SpeakerRole::Remote, "Speaker 2", false, false, 3);
    let linked = add_speaker(&svc, m, SpeakerRole::Remote, "Rowan Pell", true, false, 4);

    let v = named(&svc, m);
    assert_eq!(
        v.attendees(),
        ["Me", "Alice", "Speaker 2", "Rowan Pell"],
        "the ignored voice is left out, the others are kept"
    );
    // The transcript still names the ignored voice as before.
    assert_eq!(label_of(&v, ignored.id), "Speaker 1");
    assert_eq!(label_of(&v, unnamed.id), "Speaker 2");
    assert_eq!(label_of(&v, linked.id), "Rowan Pell");

    // Control: the same row without the flag is an attendee.
    let mut back = ignored.clone();
    back.ignored = false;
    svc.store().put_speaker(back).unwrap();
    assert_eq!(
        named(&svc, m).attendees(),
        ["Me", "Alice", "Speaker 1", "Speaker 2", "Rowan Pell"]
    );
}

#[test]
fn view_attendees_count_a_legacy_ignore_only_before_the_copy() {
    let dir = TempDir::new("view");
    let (svc, _) = services(&dir);
    let m = meeting(&svc, "Invented review", 5);
    let voice = add_speaker(&svc, m, SpeakerRole::Remote, "Speaker 3", false, false, 6);
    let kept = add_speaker(&svc, m, SpeakerRole::Remote, "Speaker 4", false, false, 7);
    put_legacy(&svc, m, &[&voice]);
    assert!(!crate::ignore_copy::copied(&svc.store()).unwrap());

    // Before this device's copy the legacy entry flag counts.
    let before = named(&svc, m);
    assert!(!before.attendees().contains(&"Speaker 3".to_owned()));
    assert!(before.attendees().contains(&"Speaker 4".to_owned()));
    assert_eq!(label_of(&before, voice.id), "Speaker 3");
    assert_eq!(label_of(&before, kept.id), "Speaker 4");

    // After it the entry flag is never read again (the row carries the choice).
    crate::local::set(&mut svc.store(), &DeviceRecord::IgnoreCopied.key(), b"done").unwrap();
    assert!(crate::ignore_copy::copied(&svc.store()).unwrap());
    let after = named(&svc, m);
    assert!(after.attendees().contains(&"Speaker 3".to_owned()));
    assert!(after.attendees().contains(&"Speaker 4".to_owned()));
}

#[test]
fn view_attendees_never_leave_out_the_user() {
    let dir = TempDir::new("view");
    let (svc, _) = services(&dir);
    let m = meeting(&svc, "Invented sync", 8);
    // A forged flag on the Me row (a synced row is a trust boundary) and a legacy entry for it.
    let mut me = svc
        .store()
        .speakers(m)
        .unwrap()
        .into_iter()
        .find(|s| s.role == SpeakerRole::Me)
        .unwrap();
    me.ignored = true;
    svc.store().put_speaker(me.clone()).unwrap();
    put_legacy(&svc, m, &[&me]);
    assert!(named(&svc, m).attendees().contains(&"Me".to_owned()));
    // Control: an unnamed voice with the same flag and entry is left out.
    let voice = add_speaker(&svc, m, SpeakerRole::Remote, "Speaker 5", false, true, 9);
    put_legacy(&svc, m, &[&me, &voice]);
    let v = named(&svc, m);
    assert!(v.attendees().contains(&"Me".to_owned()));
    assert!(!v.attendees().contains(&"Speaker 5".to_owned()));
}

#[test]
fn view_attendees_are_told_apart_only_among_those_kept() {
    let dir = TempDir::new("view");
    let (svc, _) = services(&dir);
    let m = meeting(&svc, "Invented handover", 10);
    // Two people sharing a first name; one is an ignored, unnamed voice the user renamed here.
    let named_row = add_speaker(&svc, m, SpeakerRole::Remote, "Casey Pine", true, false, 11);
    let ignored = add_speaker(&svc, m, SpeakerRole::Remote, "Casey Vale", false, true, 12);
    let v = named(&svc, m);
    // The transcript tells them apart over every row, as before.
    assert_eq!(label_of(&v, named_row.id), "Casey P");
    assert_eq!(label_of(&v, ignored.id), "Casey V");
    // The attendee's only namesake is ignored, so the full name is kept.
    assert_eq!(v.attendees(), ["Me", "Alice", "Casey Pine"]);

    // Control: two kept Caseys are still told apart among the attendees.
    let mut back = ignored.clone();
    back.ignored = false;
    svc.store().put_speaker(back).unwrap();
    assert_eq!(
        named(&svc, m).attendees(),
        ["Me", "Alice", "Casey P", "Casey V"]
    );
}
