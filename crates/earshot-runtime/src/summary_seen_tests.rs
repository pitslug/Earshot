//! `summary-seen/<meeting>` tests (T12.76). Every name contains `local`.

use std::sync::Arc;

use earshot_core::model::{MeetingRepo, Stamp, Summary};
use earshot_core::prompt::Template;
use earshot_core::summary::SummaryV1;
use rand_chacha::ChaCha8Rng;
use rand_core::SeedableRng;

use super::*;
use crate::testutil::{TempDir, meeting, services};

/// Stores a new current summary of `meeting`, tombstoning the one before (as a re-summary
/// does), and returns its id.
fn new_summary(svc: &Services, meeting: EntityId, seed: u64) -> EntityId {
    let clock = Arc::clone(&svc.clock);
    let mut rng = ChaCha8Rng::seed_from_u64(seed);
    let mut db = svc.store();
    let (store, hlc) = db.parts();
    let mut stamp = Stamp {
        clock: &*clock,
        hlc,
        rng: &mut rng,
    };
    for mut old in store.summaries(meeting).unwrap() {
        if !old.meta.deleted {
            old.meta.mark_deleted(stamp.hlc, stamp.clock);
            store.put_summary(old).unwrap();
        }
    }
    let body = SummaryV1 {
        title: "Release plan".into(),
        tldr: "Ship on Friday.".into(),
        enhanced_notes: Vec::new(),
        decisions: Vec::new(),
        action_items: Vec::new(),
        open_questions: Vec::new(),
        topics: Vec::new(),
    };
    let s = Summary::new(meeting, Template::General, None, body, &mut stamp);
    let id = s.id;
    store.put_summary(s).unwrap();
    id
}

fn is_new(svc: &Services, meeting: EntityId) -> bool {
    has_new_summary(&svc.store(), meeting).unwrap()
}

#[test]
fn local_summary_seen_record_bytes_are_pinned() {
    let id = EntityId::from_bytes([1; 16]);
    let pinned_json = r#""01010101-0101-0101-0101-010101010101""#;
    let bytes = encode(id).unwrap();
    assert_eq!(bytes[..4], [1, 0, 0, 0]);
    assert_eq!(std::str::from_utf8(&bytes[4..]).unwrap(), pinned_json);
    assert_eq!(decode(Some(&bytes)), Seen::Row(id));
    assert_eq!(decode(None), Seen::Absent);
    let mut other = bytes.clone();
    other[0] = 9;
    assert_eq!(decode(Some(&other)), Seen::Unreadable);
    assert_eq!(decode(Some(b"ab")), Seen::Unreadable);
    assert_eq!(
        decode(Some(&[1, 0, 0, 0, b'"', b'x', b'"'])),
        Seen::Unreadable
    );
}

#[test]
fn local_summary_seen_is_written_and_read() {
    let dir = TempDir::new("summary-seen");
    let (svc, _clock) = services(&dir);
    let m = meeting(&svc, "Release", 1);
    assert!(!is_new(&svc, m), "no summary, nothing new");
    let s = new_summary(&svc, m, 2);
    assert!(is_new(&svc, m), "never opened here");
    assert!(mark_seen(&svc, m, s).unwrap());
    assert!(!is_new(&svc, m));
    assert!(
        !mark_seen(&svc, m, s).unwrap(),
        "seen again changes nothing"
    );
    assert_eq!(
        svc.store()
            .local_value(&format!("summary-seen/{m}"))
            .unwrap()
            .as_deref(),
        Some(encode(s).unwrap().as_slice())
    );
}

#[test]
fn local_summary_seen_a_new_row_is_new_and_an_edit_in_place_is_not() {
    let dir = TempDir::new("summary-seen-new");
    let (svc, _clock) = services(&dir);
    let m = meeting(&svc, "Release", 1);
    let first = new_summary(&svc, m, 2);
    mark_seen(&svc, m, first).unwrap();
    // An edit keeps the row's id (the text before it becomes a tombstone copy).
    assert!(
        crate::edit::edit_summary(
            &svc,
            m,
            first,
            crate::edit::SummaryField::Tldr,
            "Ship on Monday."
        )
        .unwrap()
    );
    assert!(!is_new(&svc, m), "an edit in place is not a new summary");
    // A re-summary is a new row.
    let second = new_summary(&svc, m, 3);
    assert!(is_new(&svc, m));
    // The detail showed the older row (opened before the new one landed): still new.
    assert!(!mark_seen(&svc, m, first).unwrap());
    assert!(is_new(&svc, m));
    assert!(mark_seen(&svc, m, second).unwrap());
    assert!(!is_new(&svc, m));
}

#[test]
fn local_summary_seen_refuses_a_row_of_another_meeting() {
    let dir = TempDir::new("summary-seen-other");
    let (svc, _clock) = services(&dir);
    let m = meeting(&svc, "Release", 1);
    let other = meeting(&svc, "Other", 2);
    new_summary(&svc, m, 3);
    let theirs = new_summary(&svc, other, 4);
    assert!(!mark_seen(&svc, m, theirs).unwrap());
    assert!(!mark_seen(&svc, m, EntityId::from_bytes([7; 16])).unwrap());
    assert_eq!(
        svc.store().local_value(record_key(m).as_str()).unwrap(),
        None
    );
    assert!(is_new(&svc, m));
}

#[test]
fn local_summary_seen_an_unreadable_record_counts_as_seen() {
    let dir = TempDir::new("summary-seen-unreadable");
    let (svc, _clock) = services(&dir);
    let m = meeting(&svc, "Release", 1);
    let s = new_summary(&svc, m, 2);
    crate::local::set(&mut svc.store(), &record_key(m), &[9, 0, 0, 0, b'x']).unwrap();
    assert!(!is_new(&svc, m));
    // Opening it replaces the record.
    assert!(mark_seen(&svc, m, s).unwrap());
    assert!(!is_new(&svc, m));
}

#[test]
fn local_summary_seen_existing_summaries_are_marked_once() {
    let dir = TempDir::new("summary-seen-start");
    let (svc, _clock) = services(&dir);
    let a = meeting(&svc, "A", 1);
    let b = meeting(&svc, "B", 2);
    let none = meeting(&svc, "No summary", 3);
    let sa = new_summary(&svc, a, 4);
    let _sb = new_summary(&svc, b, 5);
    // `a` was already opened (at a newer row than the record says): left as it is.
    let older = EntityId::from_bytes([3; 16]);
    crate::local::set(&mut svc.store(), &record_key(a), &encode(older).unwrap()).unwrap();
    assert!(is_new(&svc, a) && is_new(&svc, b));

    assert_eq!(mark_existing_once(&svc).unwrap(), 1, "only b");
    assert!(is_new(&svc, a), "an existing record is not overwritten");
    assert!(!is_new(&svc, b));
    assert!(!is_new(&svc, none));
    assert_eq!(
        svc.store().local_value(record_key(none).as_str()).unwrap(),
        None
    );
    assert_eq!(
        svc.store().local_value(SEEN_START_KEY).unwrap().as_deref(),
        Some(&b"done"[..])
    );
    mark_seen(&svc, a, sa).unwrap();

    // Once per device: a summary that arrives after the first start is new.
    let sn = new_summary(&svc, none, 6);
    assert_eq!(mark_existing_once(&svc).unwrap(), 0);
    assert!(is_new(&svc, none));
    mark_seen(&svc, none, sn).unwrap();
    assert!(!is_new(&svc, none));
}

#[test]
fn local_summary_seen_is_swept_by_meeting_deletion() {
    let dir = TempDir::new("summary-seen-delete");
    let (svc, _clock) = services(&dir);
    let m = meeting(&svc, "Release", 1);
    let keep = meeting(&svc, "Keep", 2);
    let s = new_summary(&svc, m, 3);
    let k = new_summary(&svc, keep, 4);
    mark_seen(&svc, m, s).unwrap();
    mark_seen(&svc, keep, k).unwrap();
    svc.delete_meeting(m).unwrap();
    assert_eq!(
        svc.store().local_value(record_key(m).as_str()).unwrap(),
        None
    );
    assert!(
        svc.store()
            .local_value(record_key(keep).as_str())
            .unwrap()
            .is_some()
    );
}

#[test]
fn local_summary_seen_key_is_pinned() {
    assert_eq!(
        DeviceRecord::SummarySeenStart.key().as_str(),
        "summary-seen-start"
    );
    let m = EntityId::from_bytes([1; 16]);
    assert_eq!(
        record_key(m).as_str(),
        "summary-seen/01010101-0101-0101-0101-010101010101"
    );
}

#[test]
fn local_summary_seen_is_not_written_for_a_deleted_meeting() {
    let dir = TempDir::new("summary-seen-deleted-meeting");
    let (svc, _clock) = services(&dir);
    let m = meeting(&svc, "Release", 1);
    let s = new_summary(&svc, m, 2);
    {
        // A tombstoned meeting row, as a deletion from another device arrives by sync.
        let clock = Arc::clone(&svc.clock);
        let mut db = svc.store();
        let mut row = db.meeting(m).unwrap().unwrap();
        let (store, hlc) = db.parts();
        row.meta.mark_deleted(hlc, &*clock);
        store.put_meeting(row).unwrap();
    }
    assert!(!mark_seen(&svc, m, s).unwrap());
    assert_eq!(
        svc.store().local_value(record_key(m).as_str()).unwrap(),
        None
    );
}

#[test]
fn local_summary_seen_is_not_written_for_a_tombstoned_summary() {
    let dir = TempDir::new("summary-seen-tombstone");
    let (svc, _clock) = services(&dir);
    let m = meeting(&svc, "Release", 1);
    let s = new_summary(&svc, m, 2);
    {
        let clock = Arc::clone(&svc.clock);
        let mut db = svc.store();
        let (store, hlc) = db.parts();
        for mut row in store.summaries(m).unwrap() {
            row.meta.mark_deleted(&mut *hlc, &*clock);
            store.put_summary(row).unwrap();
        }
    }
    assert!(!mark_seen(&svc, m, s).unwrap());
    assert_eq!(
        svc.store().local_value(record_key(m).as_str()).unwrap(),
        None
    );
    assert!(!is_new(&svc, m), "no live summary, nothing new");
}

#[test]
fn local_summary_seen_reads_the_current_summary_by_the_one_rule() {
    // The cheap id read the list uses is `current_summary` over the rows (T12.76).
    let dir = TempDir::new("summary-seen-rule");
    let (svc, _clock) = services(&dir);
    let m = meeting(&svc, "Release", 1);
    let check = |svc: &Services| {
        let db = svc.store();
        let id = db.current_summary_id(m).unwrap();
        let rule = earshot_core::model::current_summary(db.summaries(m).unwrap()).map(|s| s.id);
        assert_eq!(id, rule);
        id
    };
    assert_eq!(check(&svc), None);
    let first = new_summary(&svc, m, 2);
    assert_eq!(check(&svc), Some(first));
    let second = new_summary(&svc, m, 3);
    assert_eq!(check(&svc), Some(second));
}
