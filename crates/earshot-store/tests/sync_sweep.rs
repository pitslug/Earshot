//! T14.49c (docs/sync-spec.md 7A.2 P5, "The one-off sweep"): once per store, inside
//! `Store::open`, every speaker identity entry whose speaker row is gone (missing, tombstoned, or
//! with a `deleted` that does not read) is dropped, an undecodable record is deleted whole and an
//! empty one deleted; a live row's entry is never touched, linked or not. Done is the device
//! record `identity-sweep`. Damage is made by SQL from a second connection; erasure is checked on
//! disk with a second connection kept open. Every name is invented.
//! `cargo test -p earshot-store sync`.

mod common;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use common::TempDir;
use earshot_core::capture::TrackId;
use earshot_core::diar::{Embedding, EmbeddingModelId, SpeakerRole};
use earshot_core::identity::{DeviceId, EntityId, HlcTimestamp, ManualClock, SyncMeta};
use earshot_core::model::{Meeting, MeetingRepo, Speaker};
use earshot_core::prompt::Template;
use earshot_core::voiceprint::{Person, VoiceprintRepo};
use earshot_store::identities::{
    IdentityRecord, SWEEP_KEY, SpeakerIdentity, StoredIdentities, decode, decode_sweep, encode,
    record_key,
};
use earshot_store::{Store, sweep_identities};

const THERE: DeviceId = DeviceId::from_bytes([0x22; 16]);
const T0: u64 = 1_800_000_000_000;

fn id(n: u8) -> EntityId {
    EntityId::from_bytes([
        0x01, 0x93, 0, 0, 0, 0, 0x70, 0, 0x80, 0, 0, 0, 0, 0, n, 0x5c,
    ])
}

fn blob(n: u8) -> Vec<u8> {
    id(n).as_uuid().as_bytes().to_vec()
}

fn at(dt: u64) -> HlcTimestamp {
    HlcTimestamp {
        wall_ms: T0 + dt,
        logical: 0,
        node: THERE,
    }
}

fn meta(dt: u64, deleted: bool) -> SyncMeta {
    SyncMeta {
        created_at: at(0),
        updated_at: at(dt),
        deleted,
        origin_device_id: THERE,
    }
}

fn meeting(n: u8) -> Meeting {
    Meeting {
        id: id(n),
        meta: meta(0, false),
        title: "An invented sweep".into(),
        started_unix_ms: T0,
        utc_offset_minutes: 600,
        duration_ms: Some(60_000),
        tags: Vec::new(),
        template: Template::General,
        folder_id: None,
        summary_kept_local: false,
    }
}

fn person(n: u8, name: &str, is_me: bool, deleted: bool) -> Person {
    Person {
        id: id(n),
        meta: meta(5, deleted),
        display_name: if deleted { String::new() } else { name.into() },
        is_me,
        merged_into: None,
    }
}

fn speaker(n: u8, meeting: u8, person: Option<u8>, name: &str, deleted: bool) -> Speaker {
    Speaker {
        id: id(n),
        meta: meta(7, deleted),
        meeting_id: id(meeting),
        role: SpeakerRole::Remote,
        display_name: name.into(),
        person_id: person.map(id),
        ignored: false,
    }
}

/// A centroid value no other stored byte holds, by speaker: the on-disk needle is its JSON text.
fn centroid_value(speaker: u8) -> f32 {
    0.271_828_18 + f32::from(speaker) * 0.000_173
}

fn needle(speaker: u8) -> Vec<u8> {
    serde_json::to_string(&centroid_value(speaker))
        .unwrap()
        .into_bytes()
}

/// An entry for `speaker`, its fields varied by `kind` so no entry field can decide a drop.
fn entry(speaker: u8, kind: u8) -> SpeakerIdentity {
    SpeakerIdentity {
        speaker: id(speaker),
        track: if kind.is_multiple_of(2) {
            TrackId::Reference
        } else {
            TrackId::Mic
        },
        centroid: Embedding::new(
            EmbeddingModelId::new("invented-embedder@7"),
            vec![centroid_value(speaker), 0.5],
        ),
        given_name: format!("Invented given {kind}"),
        given_person: (kind % 3 == 1).then(|| id(40)),
        given_stamp: (kind % 3 == 2).then(|| at(1)),
        user_edited: kind.is_multiple_of(4),
        enrolled: false,
        ignored: kind.is_multiple_of(5),
        linked_before: if kind % 2 == 1 {
            vec![id(41)]
        } else {
            Vec::new()
        },
    }
}

/// Whether `needle` is in the database file, its WAL or its journal.
fn on_disk(db: &Path, needle: &[u8]) -> bool {
    let mut found = false;
    for suffix in ["", "-wal", "-journal"] {
        let p = PathBuf::from(format!("{}{suffix}", db.display()));
        if let Ok(bytes) = std::fs::read(&p) {
            found |= bytes.windows(needle.len()).any(|w| w == needle);
        }
    }
    found
}

fn conn(path: &Path) -> rusqlite::Connection {
    rusqlite::Connection::open(path).unwrap()
}

fn value(path: &Path, key: &str) -> Option<Vec<u8>> {
    conn(path)
        .query_row(
            "SELECT value FROM local_state WHERE key = ?1",
            rusqlite::params![key],
            |r| r.get(0),
        )
        .ok()
}

/// The stored entries of `meeting`'s record by speaker, as JSON (byte-for-byte comparison), read
/// through a connection of its own.
fn entries(path: &Path, meeting: u8) -> BTreeMap<EntityId, String> {
    match value(path, &record_key(id(meeting))).map(|b| decode(&b)) {
        None => BTreeMap::new(),
        Some(StoredIdentities::Record(r)) => r
            .speakers
            .into_iter()
            .map(|e| (e.speaker, serde_json::to_string(&e).unwrap()))
            .collect(),
        Some(StoredIdentities::Unreadable(why)) => panic!("unreadable: {why}"),
    }
}

fn json(e: &SpeakerIdentity) -> String {
    serde_json::to_string(e).unwrap()
}

/// A store at `dir`, as an earlier build left it: written, then `identity-sweep` removed, so the
/// next open sweeps.
struct Old {
    path: PathBuf,
    _dir: TempDir,
}

impl Old {
    fn new(tag: &str, build: impl FnOnce(&mut Store)) -> Self {
        let dir = TempDir::new(tag);
        let path = dir.path().join("earshot.db");
        {
            let mut store = Store::open(&path).unwrap();
            build(&mut store);
            assert!(
                store.local_value(SWEEP_KEY).unwrap().is_some(),
                "a new store is swept at its first open"
            );
            store.delete_local_value(SWEEP_KEY).unwrap();
        }
        Self { path, _dir: dir }
    }

    fn sql(&self, sql: &str) {
        let c = conn(&self.path);
        c.execute_batch("PRAGMA foreign_keys = OFF;").unwrap();
        c.execute_batch(sql).unwrap();
    }

    fn swept(&self) -> Option<u64> {
        value(&self.path, SWEEP_KEY).map(|b| decode_sweep(&b).expect("a readable record").dropped)
    }
}

fn put_record(store: &mut Store, meeting: u8, speakers: Vec<SpeakerIdentity>) {
    store
        .set_local_value(
            &record_key(id(meeting)),
            &encode(&IdentityRecord { speakers }),
        )
        .unwrap();
}

/// Meeting 1: entries whose rows are tombstoned (name ""), missing, live and unlinked (an
/// automatic name, ignored), live and linked to a live person, to a person not known here, to an
/// `is_me` tombstone and to a non-`is_me` tombstone. Meeting 2: an undecodable record. Meeting 3:
/// an empty record. Meeting 4 (no meeting row): an entry. And a record whose key names no
/// meeting id.
fn every_kind(store: &mut Store) {
    for m in [1, 2, 3] {
        store.put_meeting(meeting(m)).unwrap();
    }
    store
        .put_person(person(40, "Avery Invented", false, false))
        .unwrap();
    store.put_person(person(42, "", true, true)).unwrap();
    store.put_person(person(43, "", false, true)).unwrap();
    store.put_speaker(speaker(10, 1, None, "", true)).unwrap();
    let mut ignored = speaker(12, 1, None, "Speaker 3", false);
    ignored.ignored = true;
    store.put_speaker(ignored).unwrap();
    store
        .put_speaker(speaker(13, 1, Some(40), "Avery Invented", false))
        .unwrap();
    store
        .put_speaker(speaker(14, 1, Some(41), "Blake Invented", false))
        .unwrap();
    store
        .put_speaker(speaker(15, 1, Some(42), "Me", false))
        .unwrap();
    store
        .put_speaker(speaker(16, 1, Some(43), "Casey Invented", false))
        .unwrap();
    store
        .put_speaker(speaker(17, 2, None, "Speaker 2", false))
        .unwrap();
    put_record(
        store,
        1,
        vec![
            entry(10, 0),
            entry(11, 1),
            entry(12, 2),
            entry(13, 3),
            entry(14, 4),
            entry(15, 5),
            entry(16, 6),
        ],
    );
    let mut undecodable = 2u32.to_le_bytes().to_vec();
    undecodable.extend_from_slice(b"{\"speakers\": not json");
    store
        .set_local_value(&record_key(id(2)), &undecodable)
        .unwrap();
    put_record(store, 3, Vec::new());
    put_record(store, 4, vec![entry(18, 7)]);
    store
        .set_local_value(
            "speaker-identities/not-a-meeting-id",
            &encode(&IdentityRecord {
                speakers: vec![entry(19, 8)],
            }),
        )
        .unwrap();
}

#[test]
fn store_sync_sweep_drops_entries_of_rows_that_are_gone_and_keeps_every_live_rows_entry() {
    let old = Old::new("sweep-kinds", every_kind);
    let before = entries(&old.path, 1);
    // Positive control: the needles are on disk before the sweep (the closing checkpoint folded
    // them into the database file).
    for s in [10, 11, 12, 18, 19] {
        assert!(on_disk(&old.path, &needle(s)), "speaker {s} before");
    }
    // A second connection held open across the open and its checkpoint (AGENTS: SQLite's
    // last-connection checkpoint must not mask a missing one).
    let held = conn(&old.path);
    let store = Store::open(&old.path).unwrap();
    // Gone rows' entries, the undecodable record, the empty one, the meeting-less one: dropped.
    let after = entries(&old.path, 1);
    for s in [10, 11] {
        assert!(!after.contains_key(&id(s)), "speaker {s} kept");
    }
    // Every live row's entry, linked or not, whoever it is linked to: byte for byte as before.
    for s in [12, 13, 14, 15, 16] {
        assert_eq!(after.get(&id(s)), before.get(&id(s)), "speaker {s}");
    }
    assert_eq!(after.len(), 5);
    for m in [2, 3, 4] {
        assert_eq!(value(&old.path, &record_key(id(m))), None, "meeting {m}");
    }
    assert_eq!(
        value(&old.path, "speaker-identities/not-a-meeting-id"),
        None
    );
    // Two entries, the undecodable record as one, the meeting-less entry and the one under a key
    // that names no meeting.
    assert_eq!(old.swept(), Some(5));
    // Off the disk, with the store still open and the second connection held.
    for s in [10, 11, 18, 19] {
        assert!(!on_disk(&old.path, &needle(s)), "speaker {s} on disk");
    }
    // Positive control of the same search: a kept entry's needle is found.
    assert!(on_disk(&old.path, &needle(12)));
    drop(store);
    drop(held);
}

#[test]
fn store_sync_sweep_a_second_open_reads_no_record_and_changes_nothing() {
    let old = Old::new("sweep-twice", every_kind);
    drop(Store::open(&old.path).unwrap());
    assert_eq!(old.swept(), Some(5));
    // An entry of a tombstoned row written after the sweep stays: the sweep ran once.
    {
        let mut store = Store::open(&old.path).unwrap();
        put_record(&mut store, 1, vec![entry(10, 0), entry(12, 2)]);
    }
    let held = conn(&old.path);
    drop(Store::open(&old.path).unwrap());
    assert_eq!(entries(&old.path, 1).len(), 2);
    assert_eq!(old.swept(), Some(5), "the record is written once");
    drop(held);
}

#[test]
fn store_sync_sweep_entries_written_by_any_writer_are_judged_only_by_their_row() {
    // An entry carrying an edit's marks (user_edited, a stamp, a person the pass gave, earlier
    // links, a legacy ignore) and one carrying none: kept when the row is live, dropped when it is
    // tombstoned, whatever the entry says (fifth review B-1).
    let old = Old::new("sweep-fields", |s| {
        s.put_meeting(meeting(1)).unwrap();
        for (n, deleted) in [(20, false), (21, false), (22, true), (23, true)] {
            s.put_speaker(speaker(n, 1, None, "Speaker 2", deleted))
                .unwrap();
        }
        put_record(
            s,
            1,
            vec![entry(20, 0), entry(21, 1), entry(22, 0), entry(23, 1)],
        );
    });
    drop(Store::open(&old.path).unwrap());
    let after = entries(&old.path, 1);
    assert_eq!(
        after.keys().copied().collect::<Vec<_>>(),
        vec![id(20), id(21)]
    );
    assert_eq!(after[&id(20)], json(&entry(20, 0)));
    assert_eq!(after[&id(21)], json(&entry(21, 1)));
    assert_eq!(old.swept(), Some(2));
}

#[test]
fn store_sync_sweep_a_damaged_row_does_not_fail_the_open() {
    let old = Old::new("sweep-damaged", |s| {
        s.put_meeting(meeting(5)).unwrap();
        s.put_speaker(speaker(30, 5, None, "Speaker 2", false))
            .unwrap();
        s.put_speaker(speaker(31, 5, None, "Speaker 3", false))
            .unwrap();
        s.put_speaker(speaker(32, 5, None, "Speaker 4", false))
            .unwrap();
        put_record(s, 5, vec![entry(30, 0), entry(31, 1), entry(33, 2)]);
    });
    // Row 30: its other columns do not decode (`speakers` would fail), its `deleted` reads 0.
    old.sql(&format!(
        "UPDATE speaker SET role = 'bogus' WHERE hex(id) = '{}'",
        hex(&blob(30))
    ));
    // Row 31: its `id` does not read (15 bytes), so entry 31 matches no readable row and goes.
    old.sql(&format!(
        "UPDATE speaker SET id = x'{}' WHERE hex(id) = '{}'",
        &hex(&blob(31))[..30],
        hex(&blob(31))
    ));
    // Under STRICT tables a `deleted` that does not read cannot be stored (16.10a): the case the
    // spec names is refused by SQLite itself, so the sweep's three-way read only ever sees it on a
    // store from outside this build.
    let c = conn(&old.path);
    assert!(
        c.execute(
            "UPDATE speaker SET deleted = 'not a number' WHERE hex(id) = ?1",
            rusqlite::params![hex(&blob(32))],
        )
        .is_err()
    );
    // Control: the damaged row really fails a decoding read.
    {
        let store = Store::open(&old.path).unwrap();
        assert!(store.speakers(id(5)).is_err(), "the role damage decodes");
    }
    let after = entries(&old.path, 5);
    assert_eq!(after.get(&id(30)), Some(&json(&entry(30, 0))), "kept");
    assert!(
        !after.contains_key(&id(31)),
        "an entry of a row whose id does not read"
    );
    assert!(!after.contains_key(&id(33)), "an entry with no row");
    assert_eq!(old.swept(), Some(2));
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02X}")).collect()
}

#[test]
fn store_sync_sweep_best_effort_an_error_rolls_back_and_the_next_open_sweeps() {
    let old = Old::new("sweep-error", every_kind);
    let before = entries(&old.path, 1);
    // An error inside the sweep's transaction, at its last write (the record), after every drop:
    // no seam in shipped code, a trigger in the test's store.
    old.sql(
        "CREATE TRIGGER invented_sweep_failure BEFORE INSERT ON local_state \
         WHEN NEW.key = 'identity-sweep' BEGIN SELECT RAISE(ABORT, 'invented failure'); END;",
    );
    let store = Store::open(&old.path).expect("the open still succeeds");
    drop(store);
    assert_eq!(entries(&old.path, 1), before, "nothing dropped");
    assert!(
        value(&old.path, &record_key(id(2))).is_some(),
        "rolled back"
    );
    assert_eq!(old.swept(), None, "no record");
    // The next open, the failure gone, sweeps.
    old.sql("DROP TRIGGER invented_sweep_failure;");
    drop(Store::open(&old.path).unwrap());
    assert_eq!(old.swept(), Some(5));
    assert_eq!(entries(&old.path, 1).len(), 5);
}

#[test]
fn store_sync_sweep_two_opens_racing_sweep_once() {
    let old = Old::new("sweep-race", every_kind);
    let barrier = std::sync::Barrier::new(4);
    std::thread::scope(|s| {
        for _ in 0..4 {
            s.spawn(|| {
                barrier.wait();
                drop(Store::open(&old.path).unwrap());
            });
        }
    });
    // A second sweep would have written its own count (0) over the first one's.
    assert_eq!(old.swept(), Some(5));
    assert_eq!(entries(&old.path, 1).len(), 5);
}

#[test]
fn store_sync_sweep_rereads_the_record_inside_its_transaction() {
    let old = Old::new("sweep-reread", every_kind);
    let mut store = Store::open(&old.path).unwrap();
    // Entries of gone rows written after the sweep, then the root run again by itself: it reads
    // the record under the write lock and does nothing.
    put_record(&mut store, 1, vec![entry(10, 0), entry(11, 1)]);
    let again = store.guarded_immediate(sweep_identities).unwrap();
    assert_eq!(again, None);
    assert_eq!(entries(&old.path, 1).len(), 2);
    // Control: with the record gone the same call sweeps.
    store.delete_local_value(SWEEP_KEY).unwrap();
    assert_eq!(store.guarded_immediate(sweep_identities).unwrap(), Some(2));
    assert!(entries(&old.path, 1).is_empty());
    assert_eq!(
        value(&old.path, &record_key(id(1))),
        None,
        "an emptied record is deleted"
    );
}

#[test]
fn store_sync_sweep_after_forgetting_a_voice_keeps_entries_of_live_linked_rows() {
    // Forgetting P's voice drops the entry of P's row (and nothing on any row); the sweep at the
    // next open leaves the meeting's other entries of rows linked to live persons in place.
    let old = Old::new("sweep-forget", |s| {
        s.put_meeting(meeting(1)).unwrap();
        s.put_person(person(40, "Avery Invented", false, false))
            .unwrap();
        s.put_person(person(44, "Drew Invented", false, false))
            .unwrap();
        s.put_speaker(speaker(24, 1, Some(40), "Avery Invented", false))
            .unwrap();
        s.put_speaker(speaker(25, 1, Some(44), "Drew Invented", false))
            .unwrap();
        put_record(s, 1, vec![entry(24, 0), entry(25, 2)]);
        let clock = ManualClock::new(T0 + 60_000);
        let mut hlc = s.hlc(DeviceId::from_bytes([0x11; 16])).unwrap();
        s.forget_voiceprints(id(40), &mut hlc, &clock).unwrap();
    });
    let before = entries(&old.path, 1);
    assert_eq!(before.keys().copied().collect::<Vec<_>>(), vec![id(25)]);
    let held = conn(&old.path);
    let store = Store::open(&old.path).unwrap();
    assert_eq!(entries(&old.path, 1), before);
    let rows = store.speakers(id(1)).unwrap();
    assert!(
        rows.iter()
            .any(|r| r.id == id(24) && r.person_id == Some(id(40)) && !r.meta.deleted),
        "the forgotten person's row is untouched"
    );
    assert_eq!(old.swept(), Some(0));
    drop(store);
    drop(held);
}

/// The sweep's real cost (docs/sync-spec.md 7A.2 P5, "Real cost"): a generated store of 2,000
/// meetings, each with four speaker rows (one tombstoned) and a record of four entries with
/// CAM++-sized centroids (512 floats). Prints the time of the open that sweeps and of one that
/// does not; asserts only what was dropped. A measurement, never a gate.
#[test]
#[ignore = "a measurement: cargo test --release -p earshot-store --test sync_sweep cost -- --ignored --nocapture"]
fn store_sync_sweep_cost_on_two_thousand_meetings() {
    const MEETINGS: u16 = 2_000;
    const DIM: usize = 512;
    let wide = |m: u16, k: u8| -> EntityId {
        let [hi, lo] = m.to_be_bytes();
        EntityId::from_bytes([
            0x01, 0x93, 0, 0, 0, 0, 0x70, 0, 0x80, 0, 0, 0, hi, lo, k, 0x5d,
        ])
    };
    let old = Old::new("sweep-cost", |s| {
        s.transaction(|tx| -> Result<(), earshot_store::StoreError> {
            for m in 0..MEETINGS {
                let mut mt = meeting(1);
                mt.id = wide(m, 0);
                tx.put_meeting(mt).unwrap();
                let mut speakers = Vec::new();
                for k in 1..=4u8 {
                    let mut row = speaker(1, 1, None, "Speaker 2", k == 4);
                    row.id = wide(m, k);
                    row.meeting_id = wide(m, 0);
                    tx.put_speaker(row).unwrap();
                    let mut e = entry(k, k);
                    e.speaker = wide(m, k);
                    e.centroid = Embedding::new(
                        EmbeddingModelId::new("campplus-invented@1"),
                        (0..DIM)
                            .map(|i| ((i as f32) * 0.001 + f32::from(k)).sin() * 0.1)
                            .collect(),
                    );
                    speakers.push(e);
                }
                tx.set_local_value(
                    &record_key(wide(m, 0)),
                    &encode(&IdentityRecord { speakers }),
                )
                .unwrap();
            }
            Ok(())
        })
        .unwrap();
    });
    let size = std::fs::metadata(&old.path).unwrap().len();
    let t = std::time::Instant::now();
    drop(Store::open(&old.path).unwrap());
    let sweeping = t.elapsed();
    let t = std::time::Instant::now();
    drop(Store::open(&old.path).unwrap());
    let done = t.elapsed();
    assert_eq!(old.swept(), Some(u64::from(MEETINGS)));
    println!(
        "identity sweep cost: {MEETINGS} meetings, database {size} bytes; open that sweeps \
         (with its checkpoint and close) {sweeping:?}, open after {done:?}"
    );
}
