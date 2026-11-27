//! T14.49e (docs/sync-spec.md 7A.2, "The one-off removal repair, sync on or off", 7A.4 and
//! 7A.11): the repair that runs once per device at start, outside any page. Every name and id
//! is invented.

use earshot_core::identity::{EntityId, SyncMeta};
use earshot_core::voiceprint::{Person, VoiceprintRepo};

use super::*;
use crate::edit::PersonChoice;

const NAME: &str = "Robin Invented";
const MERGED: &str = "Merged Invented";

fn speaker_named(dev: &Device, m: EntityId, name: &str) -> EntityId {
    dev.svc
        .store()
        .speakers(m)
        .unwrap()
        .into_iter()
        .find(|s| s.display_name == name)
        .unwrap_or_else(|| panic!("no speaker {name}"))
        .id
}

/// A speaker row's `person_id`, read by SQL (the row may sit beside a damaged one).
fn person_id_by_sql(dev: &Device, speaker: EntityId) -> Option<Vec<u8>> {
    let c = rusqlite::Connection::open(db_path(dev)).unwrap();
    c.query_row(
        "SELECT person_id FROM speaker WHERE id = ?1",
        rusqlite::params![speaker.as_uuid().as_bytes().to_vec()],
        |r| r.get(0),
    )
    .unwrap()
}

fn removed(dev: &Device, p: EntityId) -> bool {
    dev.svc
        .store()
        .person(p)
        .unwrap()
        .is_some_and(|r| r.meta.deleted && r.display_name.is_empty())
}

fn summarise(dev: &Device, m: EntityId, tldr: &str, seed: u64) {
    use rand_core::SeedableRng;
    let v = crate::view::MeetingView::load_named(&dev.svc.store(), m)
        .unwrap()
        .unwrap();
    let mut s = summary_of("Planning");
    s.tldr = tldr.into();
    crate::summary::store_summary(
        &dev.svc,
        &v,
        "test-model".into(),
        &s,
        &mut rand_chacha::ChaCha8Rng::seed_from_u64(seed),
    )
    .unwrap();
}

fn marks(dev: &Device, m: EntityId) -> Vec<String> {
    let mut names: Vec<String> = crate::summary_names::read(&dev.svc.store(), m)
        .unwrap()
        .into_values()
        .flatten()
        .collect();
    names.sort();
    names
}

fn record(dev: &Device) -> Option<Vec<u8>> {
    dev.svc.store().local_value(REMOVAL_REPAIR_KEY).unwrap()
}

/// The `(table, id)` of every queued row.
fn outbox(dev: &Device) -> Vec<(String, Vec<u8>)> {
    let c = rusqlite::Connection::open(db_path(dev)).unwrap();
    let mut q = c.prepare("SELECT tbl, id FROM sync_outbox").unwrap();
    q.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .map(Result::unwrap)
        .collect()
}

fn clear_outbox(dev: &Device) {
    let c = rusqlite::Connection::open(db_path(dev)).unwrap();
    c.execute("DELETE FROM sync_outbox", []).unwrap();
}

fn queued(dev: &Device, tbl: &str, id: EntityId) -> bool {
    outbox(dev)
        .iter()
        .any(|(t, i)| t == tbl && i.as_slice() == id.as_uuid().as_bytes())
}

/// Meeting `m`'s Alice linked to a new person `NAME`; with `summary`, a summary that used the
/// name is stored after. Returns (Alice, the person).
fn linked(dev: &Device, m: EntityId, summary: bool) -> (EntityId, EntityId) {
    let alice = speaker_named(dev, m, "Alice");
    let p = crate::edit::link_speaker(&dev.svc, m, alice, &PersonChoice::New { name: NAME.into() })
        .unwrap()
        .person;
    if summary {
        summarise(dev, m, "Robin Invented will send the plan.", 31);
    }
    (alice, p)
}

/// [`linked`] then [`left_behind`]: (Alice, the removed person, the merged one).
fn leftover(dev: &Device, m: EntityId, summary: bool) -> (EntityId, EntityId, EntityId) {
    let (alice, p) = linked(dev, m, summary);
    (alice, p, left_behind(dev, p))
}

/// What an earlier build left on `dev` after a removal of `p` it pulled: `p` tombstoned here
/// (name cleared) with its rows still linked, and a live person `MERGED` merged into it.
/// Returns the merged person.
fn left_behind(dev: &Device, p: EntityId) -> EntityId {
    let merged = EntityId::from_bytes([0x5a; 16]);
    let clock = Arc::clone(&dev.svc.clock);
    let mut db = dev.svc.store();
    let (store, hlc) = db.parts();
    let mut row = store.person(p).unwrap().unwrap();
    row.display_name = String::new();
    row.meta.mark_deleted(hlc, &*clock);
    store.put_person(row).unwrap();
    store
        .put_person(Person {
            id: merged,
            meta: SyncMeta::new(hlc, &*clock),
            display_name: MERGED.into(),
            is_me: false,
            merged_into: Some(p),
        })
        .unwrap();
    merged
}

#[test]
fn sync_removal_repair_record_key_is_pinned() {
    assert_eq!(
        DeviceRecord::RemovalRepair.key().as_str(),
        REMOVAL_REPAIR_KEY
    );
    assert_eq!(REMOVAL_REPAIR_KEY, "removal-repair");
}

/// The one-off removal repair (7A.11): with sync off both leftovers are mended, a summary that
/// used the removed name is marked, `removal-repair` holds counts only, the rewritten rows wait
/// in the outbox and go when sync is turned on; a second run writes nothing.
#[test]
fn sync_removal_repair_mends_both_with_sync_off_and_sends_them_when_sync_is_on() {
    let relay = relay();
    let (a, b) = pair(&relay);
    let m = meeting(&a.svc, "Planning", 30);
    let (alice, p) = linked(&a, m, true);
    trade(&a, &b);
    let merged = left_behind(&a, p);
    set_sync_on(&a.svc, false).unwrap();
    clear_outbox(&a);
    assert!(record(&a).is_none());
    assert_eq!(marks(&a, m), Vec::<String>::new(), "the control");

    let r = removal_repair(&a.svc).unwrap();
    assert!(r.ran, "{r:?}");
    assert_eq!(
        (r.persons_removed_with, r.speakers_unlinked),
        (1, 1),
        "{r:?}"
    );
    assert!(r.meetings.contains(&m) && r.people, "{r:?}");
    assert!(
        r.erased >= 2,
        "a name cleared and a speaker name rewritten: {r:?}"
    );
    assert_eq!(r.names_recorded, 1, "{r:?}");
    assert_eq!(person_id_by_sql(&a, alice), None);
    assert!(removed(&a, merged));
    assert!(removed(&a, p));
    assert_eq!(marks(&a, m), [NAME]);
    let bytes = record(&a).expect("the record is written");
    let (version, json) = earshot_store::split_version(&bytes).unwrap();
    assert_eq!(version, REMOVAL_REPAIR_VERSION);
    let v: serde_json::Value = serde_json::from_slice(json).unwrap();
    assert_eq!(v["persons_removed_with"], 1);
    assert_eq!(v["speakers_unlinked"], 1);
    assert!(
        !String::from_utf8_lossy(json).contains("Invented"),
        "counts only"
    );
    assert!(queued(&a, "speaker", alice), "{:?}", outbox(&a));
    assert!(queued(&a, "person", merged), "{:?}", outbox(&a));
    assert_eq!(r.changes().unwrap().meetings, vec![m]);

    // A second run writes nothing.
    let rows = a.rows();
    let queue = outbox(&a);
    let again = removal_repair(&a.svc).unwrap();
    assert_eq!(again, RemovalRepaired::default());
    assert_eq!(a.rows(), rows);
    assert_eq!(outbox(&a), queue);
    assert_eq!(record(&a).unwrap(), bytes);

    // Turned on, the rows go: B ends with Alice unlinked and the merged person removed.
    assert_eq!(
        b.svc
            .store()
            .speakers(m)
            .unwrap()
            .iter()
            .find(|s| s.id == alice)
            .unwrap()
            .person_id,
        Some(p),
        "the control on B"
    );
    set_sync_on(&a.svc, true).unwrap();
    a.client().tick().unwrap();
    b.client().tick().unwrap();
    assert_eq!(person_id_by_sql(&b, alice), None);
    assert!(removed(&b, merged));
}

/// An injected store error at the repair's last write (the record) leaves nothing written and no
/// record; the caller carries on and the next run repairs. No seam in shipped code: a trigger in
/// the test's store.
#[test]
fn sync_removal_repair_a_store_error_leaves_nothing_and_the_next_run_repairs() {
    let a = Device::new("sync-repair-error");
    let m = meeting(&a.svc, "Planning", 32);
    let (alice, _p, merged) = leftover(&a, m, true);
    clear_outbox(&a);
    let c = rusqlite::Connection::open(db_path(&a)).unwrap();
    c.execute_batch(
        "CREATE TRIGGER invented_repair_failure BEFORE INSERT ON local_state \
         WHEN NEW.key = 'removal-repair' BEGIN SELECT RAISE(ABORT, 'invented failure'); END;",
    )
    .unwrap();
    let rows = a.rows();
    assert!(removal_repair(&a.svc).is_err());
    assert_eq!(a.rows(), rows, "rolled back");
    assert!(person_id_by_sql(&a, alice).is_some());
    assert!(!removed(&a, merged));
    assert!(record(&a).is_none());
    assert!(outbox(&a).is_empty(), "{:?}", outbox(&a));
    assert_eq!(marks(&a, m), Vec::<String>::new());
    c.execute_batch("DROP TRIGGER invented_repair_failure")
        .unwrap();
    drop(c);
    let r = removal_repair(&a.svc).unwrap();
    assert!(r.ran && r.speakers_unlinked == 1, "{r:?}");
    assert_eq!(person_id_by_sql(&a, alice), None);
    assert!(removed(&a, merged));
    assert!(record(&a).is_some());
    assert_eq!(marks(&a, m), [NAME]);
}

/// A repair and a tick started together (7A.11): one runs after the other under the store lock,
/// so the rows end as either alone leaves them: Alice unlinked, the merged person removed, the
/// summary marked once, and afterwards neither another repair nor another tick writes anything.
#[test]
fn sync_removal_repair_and_a_tick_started_together_leave_the_same_rows_as_either_alone() {
    let relay = relay();
    let (a, _b) = pair(&relay);
    let m = meeting(&a.svc, "Planning", 33);
    let (alice, _p, merged) = leftover(&a, m, true);
    let client = a.client();
    let gate = std::sync::Barrier::new(2);
    std::thread::scope(|s| {
        s.spawn(|| {
            gate.wait();
            removal_repair(&a.svc).unwrap();
        });
        s.spawn(|| {
            gate.wait();
            client.tick().unwrap();
        });
    });
    assert_eq!(person_id_by_sql(&a, alice), None);
    assert!(removed(&a, merged));
    assert_eq!(marks(&a, m), [NAME]);
    assert!(record(&a).is_some());
    let rows = a.rows();
    let again = removal_repair(&a.svc).unwrap();
    assert!(!again.ran);
    let r = client.tick().unwrap();
    assert_eq!((r.written, r.repaired), (0, 0), "{r:?}");
    assert_eq!(a.rows(), rows);
}

/// A repair that cleared names runs `flush_erasures` (7A.4): the merged person's name and the
/// rewritten speaker name leave the database file and its write-ahead log, with a second
/// connection kept open across the check.
#[test]
fn sync_removal_repair_erases_the_old_names_from_the_file_and_log() {
    let a = Device::new("sync-repair-erase");
    let m = meeting(&a.svc, "Planning", 34);
    let (_alice, _p, _merged) = leftover(&a, m, false);
    let path = db_path(&a);
    let other = rusqlite::Connection::open(&path).unwrap();
    let holds = |name: &str| {
        let mut wal = path.as_os_str().to_owned();
        wal.push("-wal");
        [path.clone(), std::path::PathBuf::from(wal)]
            .iter()
            .filter_map(|f| std::fs::read(f).ok())
            .any(|bytes| bytes.windows(name.len()).any(|w| w == name.as_bytes()))
    };
    assert!(
        holds(NAME) && holds(MERGED),
        "the control: both are there before"
    );
    let r = removal_repair(&a.svc).unwrap();
    assert!(r.erased > 0, "{r:?}");
    assert!(!holds(NAME), "the speaker name left the file and log");
    assert!(
        !holds(MERGED),
        "the merged person's name left the file and log"
    );
    drop(other);
}

/// Damaged rows under section 16 and a widened `names_before` (7A.11): the repair commits past a
/// damaged person row elsewhere and a damaged speaker row in a meeting with a summary, counting
/// them; a damaged current summary costs the meeting its mark. Control: nothing damaged marks it.
#[test]
fn sync_removal_repair_commits_past_damaged_rows_and_counts_them() {
    for case in ["none", "person", "speaker", "summary"] {
        let a = Device::new("sync-repair-damaged");
        let m = meeting(&a.svc, "Planning", 35);
        let (alice, _p, merged) = leftover(&a, m, true);
        let other = meeting(&a.svc, "Review", 36);
        let bob = speaker_named(&a, other, "Alice");
        let elsewhere = crate::edit::link_speaker(
            &a.svc,
            other,
            bob,
            &PersonChoice::New {
                name: "Kestrel Invented".into(),
            },
        )
        .unwrap()
        .person;
        let me = speaker_named(&a, m, "Me");
        match case {
            "person" => damage(
                &a,
                "UPDATE person SET id = X'0102030405060708090A0B0C0D0E0F' WHERE id = ?1",
                elsewhere,
            ),
            "speaker" => damage(&a, "UPDATE speaker SET role = 'bogus' WHERE id = ?1", me),
            "summary" => damage(
                &a,
                "UPDATE summary SET template = 'bogus' WHERE meeting_id = ?1",
                m,
            ),
            _ => {}
        }
        let r = removal_repair(&a.svc).unwrap();
        assert!(r.ran, "case {case}: {r:?}");
        assert_eq!(person_id_by_sql(&a, alice), None, "case {case}: {r:?}");
        assert!(removed(&a, merged), "case {case}");
        assert!(record(&a).is_some(), "case {case}");
        assert_eq!(r.damaged_skipped > 0, case != "none", "case {case}: {r:?}");
        let expected: &[&str] = match case {
            "summary" => &[],
            _ => &[NAME],
        };
        assert_eq!(marks(&a, m), expected, "case {case}: {r:?}");
    }
}
