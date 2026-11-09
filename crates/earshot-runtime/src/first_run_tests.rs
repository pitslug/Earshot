//! Tests of the first-run state (T9.1d). Every name contains `first_run`.

use super::*;
use crate::testutil::{TempDir, meeting, services};

#[test]
fn first_run_shows_on_a_store_that_never_held_a_meeting() {
    let dir = TempDir::new("first-run");
    let (svc, _) = services(&dir);
    let db = svc.store();
    assert_eq!(first_run_state(&db).unwrap(), FirstRunState { show: true });
    assert_eq!(
        db.local_value(FIRST_RUN_KEY).unwrap(),
        None,
        "reading writes nothing"
    );
}

/// The rule that keeps the wizard away from an existing install (the owner's data): any meeting in
/// the store counts as done, with no record written and nothing finished.
#[test]
fn first_run_existing_store_with_meetings_counts_as_done_without_a_record() {
    let dir = TempDir::new("first-run");
    let (svc, _) = services(&dir);
    meeting(&svc, "Earlier meeting", 1);
    let db = svc.store();
    assert!(!first_run_state(&db).unwrap().show);
    assert_eq!(db.local_value(FIRST_RUN_KEY).unwrap(), None);
}

#[test]
fn first_run_a_deleted_meeting_still_counts_as_done() {
    let dir = TempDir::new("first-run");
    let (svc, _) = services(&dir);
    let id = meeting(&svc, "Earlier meeting", 1);
    svc.delete_meeting(id).unwrap();
    assert!(!first_run_state(&svc.store()).unwrap().show);
}

#[test]
fn first_run_finish_is_stored_and_shown_once() {
    let dir = TempDir::new("first-run");
    let (svc, _) = services(&dir);
    let mut db = svc.store();
    // Skipping every step is the same call as finishing: nothing else is asked for.
    assert!(!finish_first_run(&mut db).unwrap().show);
    assert!(!first_run_state(&db).unwrap().show);
    assert!(
        !finish_first_run(&mut db).unwrap().show,
        "finishing again is harmless"
    );
}

#[test]
fn first_run_finish_survives_reopening_the_store() {
    let dir = TempDir::new("first-run");
    let path = dir.0.join("earshot.db");
    finish_first_run(&mut Store::open(&path).unwrap()).unwrap();
    assert!(!first_run_state(&Store::open(&path).unwrap()).unwrap().show);
}

/// A record from before T12.5a holds the old "never" answers: it still reads for what it says
/// about the wizard, and the answers go at the next write.
#[test]
fn first_run_an_old_record_with_declined_models_still_reads() {
    let dir = TempDir::new("first-run");
    let (svc, _) = services(&dir);
    let mut db = svc.store();
    let key = DeviceRecord::FirstRun.key();

    let not_done = b"\x01\x00\x00\x00{\"done\":false,\"declined_models\":[\"tiny.en\"]}";
    crate::local::set(&mut db, &key, not_done).unwrap();
    assert!(first_run_state(&db).unwrap().show, "not finished stays so");

    let done = b"\x01\x00\x00\x00{\"done\":true,\"declined_models\":[\"base.en\",\"tiny.en\"]}";
    crate::local::set(&mut db, &key, done).unwrap();
    assert!(!first_run_state(&db).unwrap().show, "finished stays so");

    crate::local::set(&mut db, &key, not_done).unwrap();
    assert!(!finish_first_run(&mut db).unwrap().show);
    let bytes = db.local_value(FIRST_RUN_KEY).unwrap().unwrap();
    assert_eq!(decode(&bytes), Some(Record { done: true }));
    assert!(
        !bytes.windows(8).any(|w| w == b"declined"),
        "the answers are not written back"
    );
}

#[test]
fn first_run_a_record_this_version_cannot_read_counts_as_done() {
    let dir = TempDir::new("first-run");
    let (svc, _) = services(&dir);
    let mut db = svc.store();
    let key = DeviceRecord::FirstRun.key();
    for bad in [
        &b""[..],
        &b"\x01\x00"[..],
        &b"\x02\x00\x00\x00{\"done\":false,\"declined_models\":[]}"[..],
        &b"\x01\x00\x00\x00not json"[..],
    ] {
        crate::local::set(&mut db, &key, bad).unwrap();
        assert_eq!(
            first_run_state(&db).unwrap(),
            FirstRunState { show: false },
            "{bad:?}"
        );
    }
}

#[test]
fn first_run_record_is_the_registry_key_with_its_version_outside_the_payload() {
    assert_eq!(DeviceRecord::FirstRun.key().as_str(), FIRST_RUN_KEY);
    let dir = TempDir::new("first-run");
    let (svc, _) = services(&dir);
    let mut db = svc.store();
    finish_first_run(&mut db).unwrap();
    let bytes = db.local_value(FIRST_RUN_KEY).unwrap().unwrap();
    assert_eq!(bytes[..4], RECORD_VERSION.to_le_bytes());
    assert_eq!(decode(&bytes), Some(Record { done: true }));
}
