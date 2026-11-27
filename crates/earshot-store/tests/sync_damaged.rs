//! T14.57 (docs/sync-spec.md section 16): one damaged row never stops a sync page, a removal or
//! a Forget voice. Damage is made by SQL from a second connection (a value that does not
//! convert, its other columns left in place). Every name is invented. Each test states its
//! control: the same case without the damage, or the other answer it would give.
//! `cargo test -p earshot-store sync`.

mod common;

use common::TempDir;
use earshot_core::capture::TrackId;
use earshot_core::diar::{Embedding, EmbeddingModelId, SpeakerRole};
use earshot_core::identity::{Clock, DeviceId, EntityId, Hlc, HlcTimestamp, ManualClock, SyncMeta};
use earshot_core::model::{Meeting, MeetingRepo, Speaker, Summary};
use earshot_core::prompt::Template;
use earshot_core::summary::{SUMMARY_SCHEMA_VERSION, SummaryV1};
use earshot_core::voiceprint::{
    Person, SampleRecord, Voiceprint, VoiceprintRepo, VoiceprintSample,
};
use earshot_store::identities::{IdentityRecord, SpeakerIdentity, encode, record_key};
use earshot_store::{ApplyRules, Incoming, Store, StoreError, SyncKinds, SyncedRow};

const HERE: DeviceId = DeviceId::from_bytes([0x11; 16]);
const THERE: DeviceId = DeviceId::from_bytes([0x22; 16]);
const T0: u64 = 1_800_000_000_000;

fn id(n: u8) -> EntityId {
    EntityId::from_bytes([0x01, 0x92, 0, 0, 0, 0, 0x70, 0, 0x80, 0, 0, 0, 0, 0, n, 5])
}

fn blob(n: u8) -> Vec<u8> {
    id(n).as_uuid().as_bytes().to_vec()
}

fn meta(wall: u64, node: DeviceId, deleted: bool) -> SyncMeta {
    let at = HlcTimestamp {
        wall_ms: wall,
        logical: 0,
        node,
    };
    SyncMeta {
        created_at: at,
        updated_at: at,
        deleted,
        origin_device_id: node,
    }
}

fn model() -> EmbeddingModelId {
    EmbeddingModelId::new("invented-embedder@1")
}

fn person(n: u8, name: &str, merged_into: Option<u8>) -> Person {
    Person {
        id: id(n),
        meta: meta(T0 + u64::from(n), HERE, false),
        display_name: name.into(),
        is_me: false,
        merged_into: merged_into.map(id),
    }
}

fn print(n: u8, person: u8) -> Voiceprint {
    Voiceprint::from_stored(
        id(n),
        meta(T0 + u64::from(n), HERE, false),
        id(person),
        model(),
        vec![0.6, 0.8],
        1,
        5_000,
    )
}

fn sample(n: u8, print: u8, person: u8) -> VoiceprintSample {
    VoiceprintSample::from_stored(SampleRecord {
        id: id(n),
        meta: meta(T0 + u64::from(n), HERE, false),
        voiceprint_id: Some(id(print)),
        person_id: Some(id(person)),
        model: Some(model()),
        vector: vec![1.0, 0.0],
        speech_ms: Some(4_000),
        meeting_id: Some(id(1)),
        speaker_id: Some(id(200)),
    })
    .unwrap()
}

fn meeting(n: u8) -> Meeting {
    Meeting {
        id: id(n),
        meta: meta(T0, HERE, false),
        title: "An invented meeting".into(),
        started_unix_ms: T0 + u64::from(n),
        utc_offset_minutes: 600,
        duration_ms: Some(60_000),
        tags: Vec::new(),
        template: Template::General,
        folder_id: None,
        summary_kept_local: false,
    }
}

fn speaker(n: u8, meeting: u8, person: Option<u8>, name: &str) -> Speaker {
    Speaker {
        id: id(n),
        meta: meta(T0 + u64::from(n), HERE, false),
        meeting_id: id(meeting),
        role: SpeakerRole::Remote,
        display_name: name.into(),
        person_id: person.map(id),
        ignored: false,
    }
}

fn entry(speaker: u8, given: Option<u8>) -> SpeakerIdentity {
    SpeakerIdentity {
        speaker: id(speaker),
        track: TrackId::Reference,
        centroid: Embedding::new(model(), vec![0.6, 0.8]),
        given_name: "Invented given".into(),
        given_person: given.map(id),
        given_stamp: None,
        user_edited: false,
        enrolled: false,
        ignored: false,
        linked_before: Vec::new(),
    }
}

fn incoming(seq: u64, row: SyncedRow) -> Incoming {
    let stamp = row.meta().updated_at;
    let mut key = [0u8; 16];
    key.copy_from_slice(&row.id().as_uuid().as_bytes()[..]);
    key[0] ^= 0xFF;
    Incoming {
        key,
        seq,
        stamp,
        body: vec![0xA5; 40],
        entity: Some(row.table().into()),
        row: Some(row),
    }
}

fn free(_: EntityId) -> bool {
    false
}

struct Dev {
    store: Store,
    path: std::path::PathBuf,
    hlc: Hlc,
    clock: ManualClock,
    _dir: TempDir,
}

impl Dev {
    fn new(tag: &str) -> Self {
        let dir = TempDir::new(tag);
        let path = dir.path().join("earshot.db");
        let store = Store::open(&path).unwrap();
        Self {
            hlc: store.hlc(HERE).unwrap(),
            store,
            path,
            clock: ManualClock::new(T0 + 100_000),
            _dir: dir,
        }
    }

    fn sql(&self, sql: &str, n: u8) {
        let c = rusqlite::Connection::open(&self.path).unwrap();
        c.execute_batch("PRAGMA foreign_keys = OFF;").unwrap();
        let changed = c.execute(sql, rusqlite::params![blob(n)]).unwrap();
        assert_eq!(changed, 1, "{sql}");
    }

    fn raw(&self, table: &str, n: u8) -> Vec<String> {
        let c = rusqlite::Connection::open(&self.path).unwrap();
        let mut st = c
            .prepare(&format!("SELECT * FROM {table} WHERE id = ?1"))
            .unwrap();
        let k = st.column_count();
        st.query_row(rusqlite::params![blob(n)], |r| {
            (0..k).map(|i| Ok(format!("{:?}", r.get_ref(i)?))).collect()
        })
        .unwrap()
    }

    fn apply(&mut self, page: &[Incoming]) -> Result<earshot_store::Applied, StoreError> {
        let rules = ApplyRules {
            kinds: SyncKinds::ALL,
            leased: &free,
            now_ms: self.clock.now_unix_ms(),
            after: None,
        };
        self.store
            .apply_remote(page, &rules, &mut self.hlc, &self.clock)
    }

    fn put_record(&mut self, meeting: u8, entries: Vec<SpeakerIdentity>) {
        self.store
            .set_local_value(
                &record_key(id(meeting)),
                &encode(&IdentityRecord { speakers: entries }),
            )
            .unwrap();
    }

    fn record(&self, meeting: u8) -> Vec<EntityId> {
        match self.store.local_value(&record_key(id(meeting))).unwrap() {
            None => Vec::new(),
            Some(b) => match earshot_store::identities::decode(&b) {
                earshot_store::identities::StoredIdentities::Record(r) => {
                    r.speakers.into_iter().map(|e| e.speaker).collect()
                }
                earshot_store::identities::StoredIdentities::Unreadable(_) => panic!("unreadable"),
            },
        }
    }

    fn print_live(&self, n: u8) -> bool {
        let c = rusqlite::Connection::open(&self.path).unwrap();
        c.query_row(
            "SELECT deleted FROM voiceprint WHERE id = ?1",
            rusqlite::params![blob(n)],
            |r| r.get::<_, i64>(0),
        )
        .unwrap()
            == 0
    }

    fn sample_live(&self, rowid_of_id: u8) -> bool {
        let c = rusqlite::Connection::open(&self.path).unwrap();
        c.query_row(
            "SELECT deleted FROM voiceprint_sample WHERE id = ?1",
            rusqlite::params![blob(rowid_of_id)],
            |r| r.get::<_, i64>(0),
        )
        .unwrap()
            == 0
    }
}

/// A person tombstone as another device pushes it.
fn tombstone(n: u8) -> SyncedRow {
    SyncedRow::Person(Person {
        id: id(n),
        meta: meta(T0 + 50_000, THERE, true),
        display_name: String::new(),
        is_me: false,
        merged_into: None,
    })
}

/// A world: persons 10 (to be removed) and 11 (someone else), each with a print and a sample,
/// a meeting with their speaker rows and identity entries.
fn world(tag: &str) -> Dev {
    let mut d = Dev::new(tag);
    let s = &mut d.store;
    s.put_meeting(meeting(1)).unwrap();
    s.put_person(person(10, "Invented Rowan", None)).unwrap();
    s.put_person(person(11, "Invented Sasha", None)).unwrap();
    s.put_voiceprint(print(30, 10)).unwrap();
    s.put_voiceprint(print(31, 11)).unwrap();
    s.put_sample(sample(40, 30, 10)).unwrap();
    s.put_sample(sample(41, 31, 11)).unwrap();
    s.put_speaker(speaker(80, 1, Some(10), "Invented Rowan"))
        .unwrap();
    s.put_speaker(speaker(81, 1, Some(11), "Invented Sasha"))
        .unwrap();
    d.put_record(1, vec![entry(80, Some(10)), entry(81, Some(11))]);
    d
}

/// Test 1: a pulled person tombstone with a damaged person row elsewhere. The page commits, the
/// removed identity's voices and centroids are gone, the damaged row is byte for byte as it
/// was, rule 1 ran over the rows that read, and the skip is counted once. Control: on the code
/// before T14.57 the same page failed in rule 1 (`all_persons`) and was rolled back; here the
/// damage removed gives the same writes and no skip.
#[test]
fn store_sync_damaged_person_elsewhere_never_stops_a_pulled_removal() {
    for damaged in [true, false] {
        let mut d = world("damaged-person-elsewhere");
        d.store
            .put_person(person(12, "Invented Tamsin", None))
            .unwrap();
        if damaged {
            d.sql(
                "UPDATE person SET display_name = CAST(X'FF00FE' AS TEXT) WHERE id = ?1",
                12,
            );
        }
        let before = d.raw("person", 12);
        let out = d.apply(&[incoming(1, tombstone(10))]).unwrap();
        assert!(
            !d.print_live(30) && !d.sample_live(40),
            "the removed voice is gone"
        );
        assert!(
            d.print_live(31) && d.sample_live(41),
            "the other voice stays"
        );
        assert_eq!(d.record(1), vec![id(81)], "the removed centroid is gone");
        assert_eq!(d.raw("person", 12), before, "the damaged row is unchanged");
        assert_eq!(out.damaged_skipped, usize::from(damaged));
        assert!(out.biometrics_forgotten >= 3, "{out:?}");
    }
}

/// Test 1 (second half): a damaged row merged into the removed person whose `merged_into` reads
/// and whose name does not: its voices are erased by rowid, and Remove tombstones it with the
/// name cleared, so it reads again as a sendable tombstone; with a second damaged column it
/// stays unsendable. Control: the readable member gives the same rows.
#[test]
fn store_sync_damaged_member_is_erased_and_tombstoned_by_rowid() {
    for second in [false, true] {
        let mut d = world("damaged-member");
        d.store
            .put_person(person(13, "Invented Ulla", Some(10)))
            .unwrap();
        d.store.put_voiceprint(print(33, 13)).unwrap();
        d.store.put_sample(sample(43, 33, 13)).unwrap();
        d.sql(
            "UPDATE person SET display_name = CAST(X'FF00FE' AS TEXT) WHERE id = ?1",
            13,
        );
        if second {
            d.sql("UPDATE person SET created_wall = -1 WHERE id = ?1", 13);
        }
        let removal = d.store.remove_person(id(10), &mut d.hlc, &d.clock).unwrap();
        assert_eq!(removal.persons_removed, 2);
        assert!(
            !d.print_live(33) && !d.sample_live(43),
            "the member's voice is gone"
        );
        let readable = d.store.person(id(13));
        if second {
            assert!(readable.is_err(), "still damaged");
            assert!(removal.damaged_skipped >= 1);
        } else {
            let p = readable.unwrap().unwrap();
            assert!(p.meta.deleted && p.display_name.is_empty(), "{p:?}");
            let page = d.store.outbox_page(100, &[], SyncKinds::ALL).unwrap();
            assert!(page.rows.iter().any(|r| r.id() == id(13)), "sendable");
        }
    }
}

/// Test 3: identity entries under `forget_identity` with damaged speaker rows. A row (`role`
/// bogus) linked to the removed person loses its entry; one whose `person_id` does not read
/// loses its entry too, counted, its row byte for byte unchanged; one whose `person_id` reads
/// as someone else keeps its entry. Control: with the unreadable `person_id` repaired to name
/// someone else, that entry is kept.
#[test]
fn store_sync_damaged_speaker_rows_under_forget_identity() {
    for repaired in [false, true] {
        let mut d = world("damaged-forget-identity");
        d.store
            .put_speaker(speaker(82, 1, Some(10), "Invented Rowan"))
            .unwrap();
        d.store
            .put_speaker(speaker(83, 1, None, "Speaker 3"))
            .unwrap();
        d.store
            .put_speaker(speaker(84, 1, Some(11), "Invented Sasha"))
            .unwrap();
        d.put_record(
            1,
            vec![
                entry(81, Some(11)),
                entry(82, None),
                entry(83, None),
                entry(84, None),
            ],
        );
        d.sql("UPDATE speaker SET role = 'bogus' WHERE id = ?1", 82);
        if repaired {
            d.sql("UPDATE speaker SET person_id = ?1 WHERE id = ?1", 83);
            d.sql(
                &format!(
                    "UPDATE speaker SET person_id = X'{}' WHERE id = ?1",
                    hex(&blob(11))
                ),
                83,
            );
        } else {
            d.sql(
                "UPDATE speaker SET person_id = X'0102030405060708090A0B0C0D0E0F' WHERE id = ?1",
                83,
            );
        }
        let before = d.raw("speaker", 83);
        d.store
            .forget_voiceprints(id(10), &mut d.hlc, &d.clock)
            .unwrap();
        let left = d.record(1);
        assert!(!left.contains(&id(82)), "the damaged row linked to them");
        assert!(
            left.contains(&id(81)) && left.contains(&id(84)),
            "someone else's"
        );
        assert_eq!(
            left.contains(&id(83)),
            repaired,
            "an unreadable person_id is dropped"
        );
        assert_eq!(d.raw("speaker", 83), before, "no row is written");
    }
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02X}")).collect()
}

/// Test 4 (store half): Remove with damaged speaker rows. (a) a damaged row linked to the
/// person (`role` bogus) is unlinked by rowid with "Speaker N" (its name reads) and stays
/// unsendable; (b) a damaged row elsewhere is untouched; (g) a live linked row whose name does
/// not read keeps its name bytes. Control (g): with the name readable it gets "Speaker N".
#[test]
fn store_sync_damaged_remove_unlinks_damaged_rows_by_rowid() {
    for name_reads in [false, true] {
        let mut d = world("damaged-remove-rows");
        d.store.put_meeting(meeting(2)).unwrap();
        d.store
            .put_speaker(speaker(85, 1, Some(10), "Invented Rowan"))
            .unwrap();
        d.store
            .put_speaker(speaker(86, 2, Some(11), "Invented Sasha"))
            .unwrap();
        d.store
            .put_speaker(speaker(87, 2, Some(10), "Invented Rowan"))
            .unwrap();
        d.sql("UPDATE speaker SET role = 'bogus' WHERE id = ?1", 85);
        d.sql("UPDATE speaker SET role = 'bogus' WHERE id = ?1", 86);
        d.sql("UPDATE speaker SET role = 'bogus' WHERE id = ?1", 87);
        if !name_reads {
            d.sql(
                "UPDATE speaker SET display_name = CAST(X'FF00FE' AS TEXT) WHERE id = ?1",
                87,
            );
        }
        let elsewhere = d.raw("speaker", 86);
        let name_before = d.raw("speaker", 87)[11].clone();
        let _ = &name_before;
        d.store.remove_person(id(10), &mut d.hlc, &d.clock).unwrap();
        let c = rusqlite::Connection::open(&d.path).unwrap();
        let row = |n: u8| -> (Option<Vec<u8>>, i64, String, String) {
            c.query_row(
                "SELECT person_id, ignored, display_name, role FROM speaker WHERE id = ?1",
                rusqlite::params![blob(n)],
                |r| {
                    Ok((
                        r.get(0)?,
                        r.get(1)?,
                        format!("{:?}", r.get_ref(2)?),
                        r.get(3)?,
                    ))
                },
            )
            .unwrap()
        };
        let text = |s: &str| format!("{:?}", rusqlite::types::ValueRef::Text(s.as_bytes()));
        let a = row(85);
        assert_eq!(
            (a.0, a.1, a.3.as_str()),
            (None, 0, "bogus"),
            "(a) unlinked by rowid"
        );
        // Row 80 of the same meeting, created first, took "Speaker 1".
        assert_eq!(a.2, text("Speaker 2"));
        assert_eq!(
            d.raw("speaker", 86),
            elsewhere,
            "(b) a damaged row elsewhere"
        );
        let g = row(87);
        assert_eq!(g.0, None, "(g) unlinked");
        if name_reads {
            assert_eq!(g.2, text("Speaker 1"));
        } else {
            assert_eq!(
                g.2, name_before,
                "(g) the derived name is never written over it"
            );
        }
        let page = d.store.outbox_page(100, &[], SyncKinds::ALL).unwrap();
        assert!(page.unreadable >= 2, "the damaged rows stay unsendable");
    }
}

/// Test 5: rules 3 and 3a over damaged voice rows. A print and a sample of a removed person
/// with a damaged `model` are erased; a sample whose `id` is 15 bytes is erased by rowid; one
/// whose `person_id` and `voiceprint_id` do not read is untouched; a chain through a step whose
/// `merged_into` does not read erases nothing. Control: with the step readable and ending at a
/// tombstone, that chain's print is erased.
#[test]
fn store_sync_damaged_voice_rows_under_rules_3_and_3a() {
    for step_reads in [false, true] {
        let mut d = world("damaged-rules-3");
        // A chain 14 -> 15 -> 10, whose middle step may not read.
        d.store
            .put_person(person(15, "Invented Vito", Some(10)))
            .unwrap();
        d.store
            .put_person(person(14, "Invented Wren", Some(15)))
            .unwrap();
        d.store.put_voiceprint(print(34, 14)).unwrap();
        d.store.put_sample(sample(44, 30, 10)).unwrap();
        d.store.put_sample(sample(45, 31, 11)).unwrap();
        d.sql("UPDATE voiceprint SET count = -1 WHERE id = ?1", 30);
        d.sql(
            "UPDATE voiceprint_sample SET speech_ms = -1 WHERE id = ?1",
            40,
        );
        d.sql(
            "UPDATE voiceprint_sample SET id = X'0102030405060708090A0B0C0D0E0F' WHERE id = ?1",
            44,
        );
        d.sql(
            "UPDATE voiceprint_sample SET person_id = X'01', voiceprint_id = X'02' WHERE id = ?1",
            45,
        );
        if !step_reads {
            d.sql("UPDATE person SET merged_into = X'00' WHERE id = ?1", 15);
        }
        let untouched = d.raw("voiceprint_sample", 45);
        // The tombstone of 10 arrives as a row this device did not tombstone itself: the page
        // writes it and then rules 3 and 3a run.
        d.apply(&[incoming(1, tombstone(10))]).unwrap();
        assert!(!d.print_live(30), "a damaged print of the removed person");
        assert!(!d.sample_live(40), "a damaged sample of the removed person");
        let c = rusqlite::Connection::open(&d.path).unwrap();
        let (deleted, vector): (i64, Vec<u8>) = c
            .query_row(
                "SELECT deleted, vector FROM voiceprint_sample WHERE id = X'0102030405060708090A0B0C0D0E0F'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!((deleted, vector.len()), (1, 0), "erased by rowid");
        assert_eq!(d.raw("voiceprint_sample", 45), untouched);
        assert_eq!(
            !d.print_live(34),
            step_reads,
            "a chain through an unreadable step"
        );
    }
}

/// Test 5a: the owner's decided case. A removed person's print whose `sum` does not read (a
/// blob of 7 bytes, not whole floats) is zeroed with the stored value's length and stays
/// unreadable and unsendable; a readable `sum` of the same person gives today's zeroed
/// tombstone. The tables are STRICT, so the spec's TEXT `sum` of 8 bytes (which would read
/// afterwards) cannot be stored. Control: a live person's print with the same damage is
/// untouched.
#[test]
fn store_sync_damaged_a_removed_persons_unreadable_vector_is_zeroed() {
    for damaged in [true, false] {
        let mut d = world("damaged-vector");
        if damaged {
            d.sql(
                "UPDATE voiceprint SET sum = X'01020304050607' WHERE id = ?1",
                30,
            );
        }
        d.sql(
            "UPDATE voiceprint SET sum = X'01020304050607' WHERE id = ?1",
            31,
        );
        let other = d.raw("voiceprint", 31);
        let n = d
            .store
            .forget_voiceprints(id(10), &mut d.hlc, &d.clock)
            .unwrap();
        assert_eq!((n.voiceprints, n.zeroed), (1, usize::from(damaged)));
        let c = rusqlite::Connection::open(&d.path).unwrap();
        let (sum, deleted): (Vec<u8>, i64) = c
            .query_row(
                "SELECT sum, deleted FROM voiceprint WHERE id = ?1",
                rusqlite::params![blob(30)],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        let len = if damaged { 7 } else { 8 };
        assert_eq!((sum, deleted), (vec![0; len], 1), "no voice left");
        let v = d.store.voiceprints_of(id(10));
        assert_eq!(v.is_ok(), !damaged, "{v:?}");
        if !damaged {
            assert!(v.unwrap()[0].meta().deleted);
        }
        assert_eq!(
            d.raw("voiceprint", 31),
            other,
            "a live person's print is untouched"
        );
    }
}

/// Test 13: any other store error still fails the page, and nothing is written. Control: with
/// the table back, the same page commits.
#[test]
fn store_sync_damaged_other_store_errors_still_fail_the_page() {
    let mut d = world("damaged-other-errors");
    {
        let c = rusqlite::Connection::open(&d.path).unwrap();
        c.execute_batch("ALTER TABLE voiceprint RENAME TO voiceprint_away;")
            .unwrap();
    }
    assert!(d.apply(&[incoming(1, tombstone(10))]).is_err());
    {
        let c = rusqlite::Connection::open(&d.path).unwrap();
        c.execute_batch("ALTER TABLE voiceprint_away RENAME TO voiceprint;")
            .unwrap();
    }
    assert!(
        d.store.person(id(10)).unwrap().unwrap().is_live(),
        "nothing was written"
    );
    d.apply(&[incoming(1, tombstone(10))]).unwrap();
    assert!(!d.store.person(id(10)).unwrap().unwrap().is_live());
}

/// Test 18: an entry matching no readable row while the meeting holds a row whose `id` does not
/// read. A user-started erasure of another identity drops it; under a pulled tombstone it is
/// dropped too unless the unreadable row's `role` reads `Me` (or does not read). Control: with
/// the unreadable row repaired and naming someone else, the entry is kept.
#[test]
fn store_sync_damaged_an_entry_that_cannot_be_placed() {
    // (how, role of the unreadable row, repaired, entry kept)
    let cases = [
        ("forget", "remote", false, false),
        ("forget", "me", false, false),
        ("pulled", "remote", false, false),
        ("pulled", "me", false, true),
        ("pulled", "bogus", false, true),
        ("forget", "remote", true, true),
        ("pulled", "me", true, true),
    ];
    for (how, role, repaired, kept) in cases {
        let mut d = world("damaged-unplaced-entry");
        d.store
            .put_speaker(speaker(88, 1, Some(11), "Invented Sasha"))
            .unwrap();
        d.put_record(1, vec![entry(81, Some(11)), entry(89, None)]);
        d.sql(
            &format!("UPDATE speaker SET role = '{role}' WHERE id = ?1"),
            88,
        );
        if repaired {
            d.sql("UPDATE speaker SET id = ?1 WHERE id = ?1", 88);
        } else {
            d.sql(
                "UPDATE speaker SET id = X'0102030405060708090A0B0C0D0E0F' WHERE id = ?1",
                88,
            );
        }
        match how {
            "forget" => {
                d.store
                    .forget_voiceprints(id(10), &mut d.hlc, &d.clock)
                    .unwrap();
            }
            _ => {
                d.apply(&[incoming(1, tombstone(10))]).unwrap();
            }
        }
        assert_eq!(
            d.record(1).contains(&id(89)),
            kept,
            "{how} role {role} repaired {repaired}"
        );
    }
}

/// Test 19: a centroid-only removal is checkpointed. A pulled tombstone whose identity has an
/// identity entry and no voice counts the entry in `biometrics_forgotten` (through
/// `Forgotten::erased`), so the client flushes. Control: no entry and no voice leaves it at 0.
#[test]
fn store_sync_damaged_a_centroid_only_removal_counts_towards_the_flush() {
    for with_entry in [true, false] {
        let mut d = Dev::new("damaged-centroid-only");
        d.store.put_meeting(meeting(1)).unwrap();
        d.store
            .put_person(person(10, "Invented Rowan", None))
            .unwrap();
        if with_entry {
            d.put_record(1, vec![entry(80, Some(10))]);
        }
        let out = d.apply(&[incoming(1, tombstone(10))]).unwrap();
        assert_eq!(out.biometrics_forgotten > 0, with_entry, "{out:?}");
    }
}

/// Test 21: rule 2's crowded meetings with a live summary whose `meeting_id` is 15 bytes: the
/// page finishes and the row is counted. Control: without the damage the crowded meeting still
/// converges to one live summary.
#[test]
fn store_sync_damaged_a_summary_meeting_id_that_does_not_read() {
    for damaged in [true, false] {
        let mut d = world("damaged-crowded");
        let summary = |n: u8, wall: u64| Summary {
            id: id(n),
            meta: meta(wall, THERE, false),
            meeting_id: id(1),
            schema_version: SUMMARY_SCHEMA_VERSION,
            template: Template::General,
            model: None,
            summary: SummaryV1 {
                tldr: format!("An invented summary {n}"),
                ..SummaryV1::default()
            },
        };
        d.store.put_summary(summary(20, T0 + 1)).unwrap();
        d.store.put_summary(summary(22, T0 + 2)).unwrap();
        if damaged {
            // Two live summaries share a meeting id that does not read, so rule 2's crowded
            // query returns it.
            for n in [20, 22] {
                d.sql(
                    "UPDATE summary SET meeting_id = X'0102030405060708090A0B0C0D0E0F' WHERE id = ?1",
                    n,
                );
            }
        }
        let out = d
            .apply(&[incoming(1, SyncedRow::Summary(summary(21, T0 + 3)))])
            .unwrap();
        assert_eq!(out.damaged_skipped, if damaged { 2 } else { 0 }, "{out:?}");
        let live = d
            .store
            .summaries(id(1))
            .unwrap()
            .into_iter()
            .filter(|s| !s.meta.deleted)
            .count();
        assert_eq!(live, 1);
    }
}

/// Test 9: `outbox_sent` with a row damaged after its send leaves its entry queued and the call
/// succeeds. Control: an undamaged sent row leaves the outbox.
#[test]
fn store_sync_damaged_outbox_sent_keeps_a_row_damaged_after_its_send() {
    let mut d = world("damaged-outbox-sent");
    let page = d.store.outbox_page(100, &[], SyncKinds::ALL).unwrap();
    let sent: Vec<(&str, EntityId, HlcTimestamp)> = page
        .rows
        .iter()
        .filter(|r| r.id() == id(10) || r.id() == id(11))
        .map(|r| (r.table(), r.id(), r.meta().updated_at))
        .collect();
    assert_eq!(sent.len(), 2);
    d.sql("UPDATE person SET updated_wall = -1 WHERE id = ?1", 10);
    let gone = d.store.outbox_sent(&sent).unwrap();
    assert_eq!(gone, 1, "only the undamaged row left the outbox");
    let c = rusqlite::Connection::open(&d.path).unwrap();
    let queued = |n: u8| -> bool {
        c.query_row(
            "SELECT count(*) FROM sync_outbox WHERE id = ?1",
            rusqlite::params![blob(n)],
            |r| r.get::<_, i64>(0),
        )
        .unwrap()
            == 1
    };
    assert!(queued(10) && !queued(11));
}

/// Test 7: rule 4 with a damaged segment and a damaged speaker in the meeting: the page commits.
/// Control: the same page over the meeting without the damaged rows commits with the same
/// answer for the readable rows.
#[test]
fn store_sync_damaged_rule_4_rows_do_not_stop_the_page() {
    for damaged in [true, false] {
        let mut d = world("damaged-rule-4");
        // An edited final line naming a speaker, so rules 4b and 4c read the meeting's lines and
        // speakers.
        d.store
            .put_track(earshot_core::model::Track {
                id: id(60),
                meta: meta(T0, HERE, false),
                meeting_id: id(1),
                kind: TrackId::Reference,
                device_name: None,
                clock_domain: earshot_core::model::ClockDomain::Replay,
            })
            .unwrap();
        let edited = earshot_core::model::SegmentRow {
            meta: meta(T0 + 5, HERE, false),
            meeting_id: id(1),
            pass: earshot_core::model::TranscriptPass::Final,
            segment: earshot_core::asr::Segment {
                id: id(70),
                track: id(60),
                start: earshot_core::asr::MeetingTime::from_millis(0),
                end: earshot_core::asr::MeetingTime::from_millis(2_000),
                text: "An invented line".into(),
                words: Vec::new(),
                confidence: 0.9,
                speaker_id: Some(id(81)),
            },
            text_edited_at: Some(meta(T0 + 5, HERE, false).updated_at),
            pass_run: Some(id(90)),
        };
        let mut other = edited.clone();
        other.segment.id = id(71);
        other.text_edited_at = None;
        d.store.put_segment(edited).unwrap();
        d.store.put_segment(other).unwrap();
        let mut hlc = d.store.hlc(HERE).unwrap();
        d.store
            .transaction(|tx| tx.send_lines_first(&mut hlc, &d.clock).map(|_| ()))
            .unwrap();
        if damaged {
            d.sql("UPDATE speaker SET role = 'bogus' WHERE id = ?1", 80);
            d.sql("UPDATE segment SET start_ms = -1 WHERE id = ?1", 71);
        }
        let mut m = meeting(1);
        m.meta = meta(T0 + 60_000, THERE, false);
        m.title = "An invented meeting, renamed".into();
        let out = d.apply(&[incoming(1, SyncedRow::Meeting(m))]).unwrap();
        assert_eq!(out.written, 1);
        assert_eq!(
            d.store.meeting(id(1)).unwrap().unwrap().title,
            "An invented meeting, renamed"
        );
    }
}
