//! T14.49b (docs/sync-spec.md 7A.2 P5, 7A.3): a user's speaker edit relinks the row's identity
//! entry through `set_row_person`, and every writer refuses a removed person. Every name
//! contains `edit`; the people are invented.

use super::*;

/// The stored entry of `speaker` in `meeting` (from the store).
fn entry_of(
    svc: &Services,
    meeting: EntityId,
    speaker: EntityId,
) -> Option<crate::identities::SpeakerIdentity> {
    identities_in(&svc.store(), meeting)
        .unwrap()
        .into_iter()
        .find(|i| i.speaker == speaker)
}

/// Tombstones `person` directly (test set-up: a removal whose rows this device has not
/// rewritten, so a row can still link to the person).
fn tombstone_directly(svc: &Services, person: EntityId) {
    let clock = Arc::clone(&svc.clock);
    let mut db = svc.store();
    let mut p = db.person(person).unwrap().unwrap();
    p.display_name.clear();
    let (store, hlc) = db.parts();
    p.meta.mark_deleted(hlc, &*clock);
    store.put_person(p).unwrap();
}

/// Whether `needle` is in the database file, its WAL or its journal.
fn on_disk(db: &std::path::Path, needle: &[u8]) -> bool {
    ["", "-wal", "-journal"].iter().any(|suffix| {
        std::fs::read(format!("{}{suffix}", db.display()))
            .is_ok_and(|bytes| bytes.windows(needle.len()).any(|w| w == needle))
    })
}

/// The JSON text of the centroid's vector, as the record stores it.
fn needle_of(e: &crate::identities::SpeakerIdentity) -> Vec<u8> {
    serde_json::to_string(&e.centroid.vector)
        .unwrap()
        .into_bytes()
}

/// P5 from a user's link and from Not: a link to a live person clears `linked_before`; Not adds
/// the person it unlinks; removing that person then drops the entry. Control: on the code before
/// T14.49b the entry stayed after the removal.
#[test]
fn edit_p5_a_user_link_and_not_relink_the_entry() {
    let dir = TempDir::new("edit-p5");
    let (svc, _) = services(&dir);
    let m = processed(&svc, 901);
    let s = remote_speakers(&svc, m)[0].id;
    let dana = link_speaker(
        &svc,
        m,
        s,
        &PersonChoice::New {
            name: "Invented Dana".into(),
        },
    )
    .unwrap()
    .person;
    assert!(entry_of(&svc, m, s).unwrap().linked_before.is_empty());
    let eli = link_speaker(
        &svc,
        m,
        s,
        &PersonChoice::New {
            name: "Invented Eli".into(),
        },
    )
    .unwrap()
    .person;
    assert!(
        entry_of(&svc, m, s).unwrap().linked_before.is_empty(),
        "a link to a live person adds nothing ({dana})"
    );
    not_this_person(&svc, m, s).unwrap();
    assert_eq!(entry_of(&svc, m, s).unwrap().linked_before, vec![eli]);
    remove_person(&svc, eli).unwrap();
    assert!(
        entry_of(&svc, m, s).is_none(),
        "the removal finds the entry through linked_before"
    );
}

/// A user's edit whose helper dropped an entry (the row was linked to a person removed here)
/// checkpoints after its commit, and the centroid is off the disk with a second connection kept
/// open. Control: the centroid was on disk before.
#[test]
fn edit_p5_an_edit_that_drops_an_entry_checkpoints_and_the_centroid_leaves_the_disk() {
    let dir = TempDir::new("edit-p5-disk");
    let (svc, _) = services(&dir);
    let m = processed(&svc, 902);
    let s = remote_speakers(&svc, m)[0].id;
    let dana = link_speaker(
        &svc,
        m,
        s,
        &PersonChoice::New {
            name: "Invented Dana".into(),
        },
    )
    .unwrap()
    .person;
    let needle = needle_of(&entry_of(&svc, m, s).unwrap());
    let db = svc.data.db_path();
    svc.store().checkpoint().unwrap();
    assert!(on_disk(&db, &needle), "sanity: the centroid is on disk");
    tombstone_directly(&svc, dana);
    let second = rusqlite::Connection::open(&db).unwrap();
    let linked = link_speaker(
        &svc,
        m,
        s,
        &PersonChoice::New {
            name: "Invented Eli".into(),
        },
    )
    .unwrap();
    assert!(!linked.took_back.checkpoint_pending);
    assert!(entry_of(&svc, m, s).is_none(), "the entry was dropped");
    assert!(!on_disk(&db, &needle), "the centroid left the disk");
    drop(second);
}

/// `set_row_person` resolves the person inside its transaction and refuses a removed one,
/// directly and through a merge chain (docs/sync-spec.md 7A.3); the row is left as it was.
#[test]
fn edit_set_row_person_refuses_a_removed_person() {
    let dir = TempDir::new("edit-p5-refuse");
    let (svc, _) = services(&dir);
    let m = processed(&svc, 903);
    let s = remote_speakers(&svc, m)[0].id;
    let gone = new_person(&svc, "Invented Fern", false, 904);
    let merged = new_person(&svc, "Invented Gale", false, 905);
    crate::edit::write(&svc, |tx, stamp| {
        merge_persons(tx, merged, gone, stamp.hlc, stamp.clock).map_err(voiceprint_err)
    })
    .unwrap();
    tombstone_directly(&svc, gone);
    let before = remote_speakers(&svc, m)[0].clone();
    for p in [gone, merged] {
        let got = crate::edit::write(&svc, |tx, stamp| {
            let mut row = live_speaker(&*tx, m, s)?;
            set_row_person(tx, stamp, m, &mut row, Some(p)).map(|_| ())
        });
        assert_eq!(refused(got), EditError::PersonNotFound(p));
    }
    assert_eq!(remote_speakers(&svc, m)[0], before, "nothing written");
}

/// Every writer of 7A.3 refuses a removed person, through a chain too: linking, saving a voice,
/// renaming and merging into the user.
#[test]
fn edit_writers_refuse_a_removed_person() {
    let dir = TempDir::new("edit-p5-writers");
    let (svc, _) = services(&dir);
    let m = processed(&svc, 906);
    let s = remote_speakers(&svc, m)[0].id;
    let gone = new_person(&svc, "Invented Fern", false, 907);
    let merged = new_person(&svc, "Invented Gale", false, 908);
    crate::edit::write(&svc, |tx, stamp| {
        merge_persons(tx, merged, gone, stamp.hlc, stamp.clock).map_err(voiceprint_err)
    })
    .unwrap();
    tombstone_directly(&svc, gone);
    for p in [gone, merged] {
        assert!(matches!(
            refused(link_speaker(&svc, m, s, &PersonChoice::Existing(p))),
            EditError::PersonNotFound(_)
        ));
        assert!(matches!(
            refused(save_voice(&svc, m, s, &PersonChoice::Existing(p))),
            EditError::PersonNotFound(_)
        ));
        assert_eq!(
            refused(rename_person(&svc, p, "Invented Hale")),
            EditError::PersonNotFound(p)
        );
        assert_eq!(
            refused(merge_into_me(&svc, p)),
            EditError::PersonNotFound(p)
        );
    }
    assert_eq!(remote_speakers(&svc, m)[0].person_id, None);
}
