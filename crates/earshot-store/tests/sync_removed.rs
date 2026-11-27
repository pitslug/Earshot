//! T14.49a (docs/sync-spec.md section 7A): a removed person or folder stays removed in
//! `apply_remote`. P1 and F1 make a person or folder tombstone final both ways, P4 keeps the user,
//! P2 takes who was merged into a removed person with them, P3 unlinks every speaker row linked
//! to a removed person, rule 4c names a nameless revived row, and `chain_end` answers
//! `MeTombstone` at a tombstoned "Me". Straight `apply_remote` pages; damage is made by SQL from
//! a second connection. Every name is invented. `cargo test -p earshot-store sync`.

mod common;

use common::TempDir;
use earshot_core::asr::{MeetingTime, Segment, Word};
use earshot_core::capture::TrackId;
use earshot_core::diar::{Embedding, EmbeddingModelId, SpeakerRole};
use earshot_core::identity::{DeviceId, EntityId, HlcTimestamp, ManualClock, SyncMeta};
use earshot_core::model::{
    ClockDomain, Folder, FolderRepo, Meeting, MeetingRepo, SegmentRow, Speaker, Track,
    TranscriptPass,
};
use earshot_core::prompt::Template;
use earshot_core::voiceprint::{
    Person, SampleRecord, Voiceprint, VoiceprintRepo, VoiceprintSample,
};
use earshot_store::identities::{IdentityRecord, SpeakerIdentity, encode, record_key};
use earshot_store::{Applied, ApplyRules, Incoming, Store, SyncKinds, SyncedRow};

const HERE: DeviceId = DeviceId::from_bytes([0x11; 16]);
const THERE: DeviceId = DeviceId::from_bytes([0x22; 16]);
const T0: u64 = 1_800_000_000_000;

fn id(n: u8) -> EntityId {
    EntityId::from_bytes([0x01, 0x93, 0, 0, 0, 0, 0x70, 0, 0x80, 0, 0, 0, 0, 0, n, 9])
}

fn blob(n: u8) -> Vec<u8> {
    id(n).as_uuid().as_bytes().to_vec()
}

fn at(dt: u64, node: DeviceId) -> HlcTimestamp {
    HlcTimestamp {
        wall_ms: T0 + dt,
        logical: 0,
        node,
    }
}

fn meta(dt: u64, node: DeviceId, deleted: bool) -> SyncMeta {
    SyncMeta {
        created_at: at(0, node),
        updated_at: at(dt, node),
        deleted,
        origin_device_id: node,
    }
}

fn model() -> EmbeddingModelId {
    EmbeddingModelId::new("invented-embedder@7")
}

fn person(n: u8, name: &str, is_me: bool, merged_into: Option<u8>, m: SyncMeta) -> Person {
    Person {
        id: id(n),
        meta: m,
        display_name: if m.deleted {
            String::new()
        } else {
            name.into()
        },
        is_me,
        merged_into: merged_into.map(id),
    }
}

fn live(n: u8, name: &str, dt: u64, node: DeviceId) -> Person {
    person(n, name, false, None, meta(dt, node, false))
}

fn tomb(n: u8, dt: u64, node: DeviceId) -> Person {
    person(n, "", false, None, meta(dt, node, true))
}

fn me_live(n: u8, dt: u64, node: DeviceId) -> Person {
    person(n, "Invented Kim", true, None, meta(dt, node, false))
}

fn me_tomb(n: u8, dt: u64, node: DeviceId) -> Person {
    person(n, "", true, None, meta(dt, node, true))
}

fn print(n: u8, person: u8) -> Voiceprint {
    Voiceprint::from_stored(
        id(n),
        meta(1, HERE, false),
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
        meta: meta(1, HERE, false),
        voiceprint_id: Some(id(print)),
        person_id: Some(id(person)),
        model: Some(model()),
        vector: vec![1.0, 0.0],
        speech_ms: Some(4_000),
        meeting_id: Some(id(1)),
        speaker_id: Some(id(80)),
    })
    .unwrap()
}

fn meeting(n: u8, m: SyncMeta) -> Meeting {
    Meeting {
        id: id(n),
        meta: m,
        title: "An invented meeting".into(),
        started_unix_ms: T0,
        utc_offset_minutes: 600,
        duration_ms: Some(60_000),
        tags: Vec::new(),
        template: Template::General,
        folder_id: None,
        summary_kept_local: false,
    }
}

fn speaker(n: u8, person: Option<u8>, name: &str, m: SyncMeta) -> Speaker {
    Speaker {
        id: id(n),
        meta: m,
        meeting_id: id(1),
        role: SpeakerRole::Remote,
        display_name: name.into(),
        person_id: person.map(id),
        ignored: false,
    }
}

fn folder(n: u8, m: SyncMeta) -> Folder {
    Folder {
        id: id(n),
        meta: m,
        name: if m.deleted {
            String::new()
        } else {
            "An invented folder".into()
        },
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

fn incoming(seq: u64, row: &SyncedRow) -> Incoming {
    let mut key = [0u8; 16];
    key.copy_from_slice(&row.id().as_uuid().as_bytes()[..]);
    key[0] ^= 0xFF;
    Incoming {
        key,
        seq,
        stamp: row.meta().updated_at,
        body: vec![0xA5; 40],
        entity: Some(row.table().into()),
        row: Some(row.clone()),
    }
}

fn free(_: EntityId) -> bool {
    false
}

struct Dev {
    store: Store,
    path: std::path::PathBuf,
    clock: ManualClock,
    seq: u64,
    _dir: TempDir,
}

impl Dev {
    /// A store holding meeting 1, with nothing queued.
    fn new(tag: &str) -> Self {
        let dir = TempDir::new(tag);
        let path = dir.path().join("earshot.db");
        let mut store = Store::open(&path).unwrap();
        store
            .put_meeting(meeting(1, meta(0, THERE, false)))
            .unwrap();
        let d = Self {
            store,
            path,
            clock: ManualClock::new(T0 + 10_000),
            seq: 0,
            _dir: dir,
        };
        d.sent();
        d
    }

    /// Everything queued so far counts as sent.
    fn sent(&self) {
        self.conn().execute("DELETE FROM sync_outbox", []).unwrap();
    }

    fn conn(&self) -> rusqlite::Connection {
        rusqlite::Connection::open(&self.path).unwrap()
    }

    fn page(&mut self, rows: &[SyncedRow]) -> Vec<Incoming> {
        rows.iter()
            .map(|r| {
                self.seq += 1;
                incoming(self.seq, r)
            })
            .collect()
    }

    fn apply(&mut self, rows: &[SyncedRow]) -> Applied {
        let page = self.page(rows);
        self.apply_incoming(&page, false)
    }

    fn apply_incoming(&mut self, page: &[Incoming], every: bool) -> Applied {
        let rules = ApplyRules {
            kinds: SyncKinds::ALL,
            leased: &free,
            now_ms: T0 + 10_000,
            after: None,
        };
        let mut hlc = self.store.hlc(HERE).unwrap();
        if every {
            self.store
                .apply_remote_every_repair(page, &rules, &mut hlc, &self.clock)
                .unwrap()
        } else {
            self.store
                .apply_remote(page, &rules, &mut hlc, &self.clock)
                .unwrap()
        }
    }

    fn sql(&self, sql: &str, n: u8) {
        let c = self.conn();
        c.execute_batch("PRAGMA foreign_keys = OFF;").unwrap();
        let changed = c.execute(sql, rusqlite::params![blob(n)]).unwrap();
        assert_eq!(changed, 1, "{sql}");
    }

    fn rowid(&self, table: &str, n: u8) -> i64 {
        self.conn()
            .query_row(
                &format!("SELECT rowid FROM {table} WHERE id = ?1"),
                rusqlite::params![blob(n)],
                |r| r.get(0),
            )
            .unwrap()
    }

    fn raw(&self, table: &str, n: u8) -> Vec<String> {
        let c = self.conn();
        let mut st = c
            .prepare(&format!("SELECT * FROM {table} WHERE id = ?1"))
            .unwrap();
        let k = st.column_count();
        st.query_row(rusqlite::params![blob(n)], |r| {
            (0..k).map(|i| Ok(format!("{:?}", r.get_ref(i)?))).collect()
        })
        .unwrap()
    }

    fn person(&self, n: u8) -> Person {
        self.store.person(id(n)).unwrap().unwrap()
    }

    fn speaker(&self, n: u8) -> Speaker {
        self.store
            .speakers(id(1))
            .unwrap()
            .into_iter()
            .find(|s| s.id == id(n))
            .unwrap()
    }

    fn queued(&self, n: u8) -> bool {
        self.conn()
            .query_row(
                "SELECT count(*) FROM sync_outbox WHERE id = ?1",
                rusqlite::params![blob(n)],
                |r| r.get::<_, i64>(0),
            )
            .unwrap()
            > 0
    }

    fn live_row(&self, table: &str, n: u8) -> bool {
        self.conn()
            .query_row(
                &format!("SELECT deleted FROM {table} WHERE id = ?1"),
                rusqlite::params![blob(n)],
                |r| r.get::<_, i64>(0),
            )
            .unwrap()
            == 0
    }

    fn put_record(&mut self, entries: Vec<SpeakerIdentity>) {
        self.store
            .set_local_value(
                &record_key(id(1)),
                &encode(&IdentityRecord { speakers: entries }),
            )
            .unwrap();
    }

    fn record(&self) -> Vec<EntityId> {
        match self.store.local_value(&record_key(id(1))).unwrap() {
            None => Vec::new(),
            Some(b) => match earshot_store::identities::decode(&b) {
                earshot_store::identities::StoredIdentities::Record(r) => {
                    r.speakers.into_iter().map(|e| e.speaker).collect()
                }
                earshot_store::identities::StoredIdentities::Unreadable(_) => panic!("unreadable"),
            },
        }
    }

    /// Every row of every table, read through a connection of its own.
    fn dump(&self) -> Vec<(String, Vec<Vec<rusqlite::types::Value>>)> {
        let c = self.conn();
        let tables: Vec<String> = c
            .prepare("SELECT name FROM sqlite_master WHERE type = 'table' ORDER BY name")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        tables
            .into_iter()
            .map(|t| {
                let mut st = c.prepare(&format!("SELECT * FROM \"{t}\"")).unwrap();
                let n = st.column_count();
                let mut rows: Vec<Vec<rusqlite::types::Value>> = st
                    .query_map([], |r| (0..n).map(|i| r.get(i)).collect())
                    .unwrap()
                    .collect::<Result<_, _>>()
                    .unwrap();
                rows.sort_by_key(|r| format!("{r:?}"));
                (t, rows)
            })
            .collect()
    }
}

fn text(s: &str) -> String {
    format!("{:?}", rusqlite::types::ValueRef::Text(s.as_bytes()))
}

/// P1, first case: a person tombstone here is never replaced by a live copy, whatever the stamps.
/// A later live copy has the tombstone stamped again after it, queued to go out
/// (`tombstones_restamped`, `stamped_here`); an earlier one is ignored. Control: on the code
/// before T14.49 the later live copy was written over the tombstone.
#[test]
fn store_sync_removed_p1_a_person_tombstone_here_stays_final_against_a_live_copy() {
    for later in [true, false] {
        let mut d = Dev::new("removed-p1-here");
        d.store.put_person(tomb(10, 100, HERE)).unwrap();
        d.sent();
        let dt = if later { 200 } else { 50 };
        let out = d.apply(&[SyncedRow::Person(live(10, "Invented Rowan", dt, THERE))]);
        let p = d.person(10);
        assert!(p.meta.deleted && p.display_name.is_empty(), "{p:?}");
        if later {
            assert_eq!(p.meta.updated_at.node, HERE, "stamped again here");
            assert!(p.meta.updated_at > at(200, THERE));
            assert!(d.queued(10), "it goes out");
            assert_eq!(out.tombstones_restamped, 1);
            assert!(out.stamped_here.contains(&id(10)));
        } else {
            assert_eq!(p.meta.updated_at, at(100, HERE), "left as it was");
            assert!(!d.queued(10));
            assert_eq!((out.tombstones_restamped, out.ignored), (0, 1));
            assert!(out.stamped_here.is_empty());
        }
        assert_eq!(out.written, 0);
    }
}

/// P1, second case: a pulled person tombstone replaces a live copy here whatever the stamps. When
/// the copy here is the later, the tombstone is written with this device's stamp and queued;
/// otherwise as pulled, unqueued. Either way it is a removal: the voice and centroid go, the
/// speaker row is unlinked (P3), and the cleared bytes count in `erased`. Control: on the code
/// before T14.49 a pulled tombstone older than the live copy here was ignored.
#[test]
fn store_sync_removed_p1_a_pulled_tombstone_replaces_a_live_copy_here() {
    for here_later in [true, false] {
        let mut d = Dev::new("removed-p1-there");
        d.store
            .put_person(live(10, "Invented Rowan", 100, HERE))
            .unwrap();
        d.store.put_voiceprint(print(30, 10)).unwrap();
        d.store
            .put_speaker(speaker(
                80,
                Some(10),
                "Invented Rowan",
                meta(5, HERE, false),
            ))
            .unwrap();
        d.put_record(vec![entry(80, None)]);
        d.sent();
        let dt = if here_later { 50 } else { 200 };
        let out = d.apply(&[SyncedRow::Person(tomb(10, dt, THERE))]);
        let p = d.person(10);
        assert!(p.meta.deleted && p.display_name.is_empty(), "{p:?}");
        if here_later {
            assert_eq!(p.meta.updated_at.node, HERE);
            assert!(p.meta.updated_at > at(100, HERE));
            assert!(d.queued(10));
            assert_eq!(out.tombstones_restamped, 1);
            assert!(out.stamped_here.contains(&id(10)));
        } else {
            assert_eq!(p.meta.updated_at, at(200, THERE), "written as pulled");
            assert!(!d.queued(10));
            assert_eq!(out.tombstones_restamped, 0);
            assert!(!out.stamped_here.contains(&id(10)));
        }
        assert!(!d.live_row("voiceprint", 30), "the voice is gone");
        assert_eq!(d.record(), Vec::<EntityId>::new(), "the centroid is gone");
        let s = d.speaker(80);
        assert_eq!((s.person_id, s.display_name.as_str()), (None, "Speaker 1"));
        assert_eq!(s.meta.updated_at.node, HERE);
        assert!(d.queued(80));
        assert_eq!(out.speakers_unlinked, 1);
        assert!(out.stamped_here.contains(&id(80)));
        assert!(out.meetings.contains(&id(1)) && out.people);
        // The name, the speaker's name, the print and the entry.
        assert!(out.erased >= 4, "{out:?}");
    }
}

/// F1: a folder tombstone is final both ways, as P1. Control: on the code before T14.49 the
/// later live copy was written over the tombstone and the earlier tombstone ignored.
#[test]
fn store_sync_removed_f1_a_folder_tombstone_is_final_both_ways() {
    for (tomb_here, here_later) in [(true, true), (true, false), (false, true), (false, false)] {
        let mut d = Dev::new("removed-f1");
        let here = if tomb_here {
            folder(60, meta(100, HERE, true))
        } else {
            folder(60, meta(100, HERE, false))
        };
        d.store.put_folder(here).unwrap();
        d.sent();
        let dt = if here_later { 50 } else { 200 };
        let out = d.apply(&[SyncedRow::Folder(folder(60, meta(dt, THERE, !tomb_here)))]);
        let f = d.store.folder(id(60)).unwrap().unwrap();
        assert!(
            f.meta.deleted && f.name.is_empty(),
            "{tomb_here} {here_later}: {f:?}"
        );
        // The tombstone is stamped here whenever the live copy is the later.
        let stamped = tomb_here != here_later;
        assert_eq!(
            f.meta.updated_at.node == HERE,
            stamped || (tomb_here && here_later)
        );
        assert_eq!(out.tombstones_restamped, usize::from(stamped));
        if !tomb_here {
            assert_eq!(out.erased, 1, "the folder's name is cleared");
        }
        let queued = out.tombstones_restamped == 1;
        assert_eq!(d.queued(60), queued);
        assert_eq!(out.stamped_here.contains(&id(60)), queued);
    }
}

/// P4, both directions: two copies of the user's person keep the live one, whatever the stamps.
/// A later "Me" tombstone has the live copy stamped again (not counted as a tombstone); a live
/// copy over a "Me" tombstone here is written, stamped again when the tombstone was the later.
/// No voice is forgotten. Control: on the code before T14.49 a later "Me" tombstone removed the
/// user and an earlier live copy was ignored.
#[test]
fn store_sync_removed_p4_sync_never_removes_the_user() {
    for tomb_later in [true, false] {
        let mut d = Dev::new("removed-p4-live-here");
        d.store.put_person(me_live(10, 100, HERE)).unwrap();
        d.store.put_voiceprint(print(30, 10)).unwrap();
        d.sent();
        let dt = if tomb_later { 200 } else { 50 };
        let out = d.apply(&[SyncedRow::Person(me_tomb(10, dt, THERE))]);
        let p = d.person(10);
        assert!(!p.meta.deleted && p.display_name == "Invented Kim", "{p:?}");
        assert_eq!(
            d.queued(10),
            tomb_later,
            "stamped again only when the tombstone is later"
        );
        if tomb_later {
            assert!(p.meta.updated_at > at(200, THERE));
        }
        assert!(d.live_row("voiceprint", 30));
        assert_eq!((out.biometrics_forgotten, out.tombstones_restamped), (0, 0));
        assert!(out.stamped_here.is_empty());
    }
    for here_later in [true, false] {
        let mut d = Dev::new("removed-p4-tomb-here");
        d.store.put_person(me_tomb(10, 100, HERE)).unwrap();
        d.sent();
        let dt = if here_later { 50 } else { 200 };
        let out = d.apply(&[SyncedRow::Person(me_live(10, dt, THERE))]);
        let p = d.person(10);
        assert!(!p.meta.deleted, "{p:?}");
        if here_later {
            assert_eq!(p.meta.updated_at.node, HERE);
            assert!(d.queued(10));
        } else {
            assert_eq!(p.meta.updated_at, at(200, THERE));
            assert!(!d.queued(10));
        }
        assert_eq!(out.tombstones_restamped, 0);
    }
}

/// P4 (A1): a "Me" tombstone that is the first copy this device holds is written with no side
/// effect: no voice, centroid or speaker link goes, so when the live copy arrives everything is
/// as before. Control: on the code before T14.49 the absent row counted as live and the
/// identity's biometrics were forgotten.
#[test]
fn store_sync_removed_p4_a_me_tombstone_as_the_first_copy_has_no_side_effects() {
    let mut d = Dev::new("removed-p4-first");
    d.store.put_voiceprint(print(30, 10)).unwrap();
    d.store.put_sample(sample(40, 30, 10)).unwrap();
    d.store
        .put_speaker(speaker(80, Some(10), "Invented Kim", meta(5, HERE, false)))
        .unwrap();
    d.put_record(vec![entry(80, Some(10))]);
    d.sent();
    let speaker_before = d.raw("speaker", 80);
    let out = d.apply(&[SyncedRow::Person(me_tomb(10, 200, THERE))]);
    assert!(d.person(10).meta.deleted, "written");
    assert!(d.live_row("voiceprint", 30) && d.live_row("voiceprint_sample", 40));
    assert_eq!(d.record(), vec![id(80)]);
    assert_eq!(d.raw("speaker", 80), speaker_before);
    assert_eq!((out.biometrics_forgotten, out.speakers_unlinked), (0, 0));
    let out = d.apply(&[SyncedRow::Person(me_live(10, 300, THERE))]);
    assert!(!d.person(10).meta.deleted);
    assert!(d.live_row("voiceprint", 30) && d.live_row("voiceprint_sample", 40));
    assert_eq!(d.record(), vec![id(80)]);
    assert_eq!(d.raw("speaker", 80), speaker_before);
    assert_eq!(out.biometrics_forgotten, 0);
}

/// P4: a "Me" tombstone over a damaged local person row is written (the local `is_me` cannot be
/// trusted), and costs only the person row: the voice stays. Control: on the code before T14.49
/// the damaged row counted as live and the voice was forgotten.
#[test]
fn store_sync_removed_p4_a_me_tombstone_over_a_damaged_local_row() {
    let mut d = Dev::new("removed-p4-damaged");
    d.store.put_person(me_live(10, 100, HERE)).unwrap();
    d.store.put_voiceprint(print(30, 10)).unwrap();
    d.sql(
        "UPDATE person SET display_name = CAST(X'FF00FE' AS TEXT) WHERE id = ?1",
        10,
    );
    let out = d.apply(&[SyncedRow::Person(me_tomb(10, 200, THERE))]);
    assert!(
        d.person(10).meta.deleted,
        "the pulled tombstone replaced it"
    );
    assert!(d.live_row("voiceprint", 30));
    assert_eq!(out.biometrics_forgotten, 0);
}

/// P4, whose `is_me` decides: the copy here when it reads, else the pulled copy. A pulled live
/// copy claiming `is_me` cannot bring back a person removed here; a pulled tombstone that drops
/// `is_me` cannot remove the user; with the local row damaged the pulled copy decides both ways.
#[test]
fn store_sync_removed_p4_copies_that_disagree_on_is_me() {
    let mut d = Dev::new("removed-p4-disagree");
    d.store.put_person(tomb(10, 100, HERE)).unwrap();
    d.store.put_person(me_live(11, 100, HERE)).unwrap();
    d.store.put_voiceprint(print(31, 11)).unwrap();
    // 12: a live "Me" row here whose name does not read; 13: a non-"Me" tombstone whose name
    // does not read; 14: the same as 13, the control.
    d.store.put_person(me_live(12, 100, HERE)).unwrap();
    d.store.put_voiceprint(print(32, 12)).unwrap();
    d.store.put_person(tomb(13, 100, HERE)).unwrap();
    d.store.put_person(tomb(14, 100, HERE)).unwrap();
    for n in [12, 13, 14] {
        d.sql(
            "UPDATE person SET display_name = CAST(X'FF00FE' AS TEXT) WHERE id = ?1",
            n,
        );
    }
    d.sent();
    d.apply(&[
        SyncedRow::Person(person(
            10,
            "Invented Rowan",
            true,
            None,
            meta(200, THERE, false),
        )),
        SyncedRow::Person(tomb(11, 200, THERE)),
        SyncedRow::Person(tomb(12, 200, THERE)),
        SyncedRow::Person(person(
            13,
            "Invented Kim",
            true,
            None,
            meta(200, THERE, false),
        )),
        SyncedRow::Person(live(14, "Invented Sasha", 200, THERE)),
    ]);
    assert!(d.person(10).meta.deleted, "stays removed");
    assert!(
        !d.person(11).meta.deleted && d.live_row("voiceprint", 31),
        "stays the user"
    );
    assert!(
        d.person(12).meta.deleted,
        "the pulled copy decides: removed"
    );
    assert!(!d.live_row("voiceprint", 32), "and its voice goes");
    assert!(
        !d.person(13).meta.deleted,
        "the pulled copy decides: the user, live"
    );
    assert!(
        d.store.person(id(14)).is_err(),
        "control: a damaged tombstone stays final"
    );
}

/// P2: everyone whose `merged_into` chain ends at a removed person goes with them, through a chain
/// of two merges: tombstoned with the name cleared (`merged_into` kept), their voices and
/// centroids forgotten, their speaker rows unlinked (P3). A chain that ends at a person not known
/// here, loops, or ends at a "Me" tombstone changes nothing. Control: on the code before T14.49
/// the merged persons stayed live.
#[test]
fn store_sync_removed_p2_who_was_merged_into_a_removed_person_goes_with_them() {
    let mut d = Dev::new("removed-p2");
    d.store.put_person(tomb(10, 100, HERE)).unwrap();
    d.store.put_voiceprint(print(31, 11)).unwrap();
    d.store
        .put_speaker(speaker(
            82,
            Some(12),
            "Invented Tamsin",
            meta(5, HERE, false),
        ))
        .unwrap();
    d.put_record(vec![entry(82, None)]);
    d.sent();
    let merged = |n: u8, into: u8, name: &str| {
        SyncedRow::Person(person(n, name, false, Some(into), meta(200, THERE, false)))
    };
    let out = d.apply(&[
        merged(11, 10, "Invented Sasha"),
        merged(12, 11, "Invented Tamsin"),
    ]);
    for n in [11, 12] {
        let p = d.person(n);
        assert!(p.meta.deleted && p.display_name.is_empty(), "{p:?}");
        assert_eq!(p.meta.updated_at.node, HERE);
        assert!(d.queued(n) && out.stamped_here.contains(&id(n)));
    }
    assert_eq!(d.person(12).merged_into, Some(id(11)), "merged_into kept");
    assert!(!d.live_row("voiceprint", 31));
    assert_eq!(d.record(), Vec::<EntityId>::new());
    assert_eq!(d.speaker(82).person_id, None);
    assert_eq!(out.persons_removed_with, 2);

    // Nothing written: an unknown end, a loop, a "Me" tombstone at the end.
    let mut d = Dev::new("removed-p2-nothing");
    d.store.put_person(me_tomb(20, 100, HERE)).unwrap();
    d.sent();
    let out = d.apply(&[
        merged(11, 99, "Invented Sasha"),
        merged(12, 13, "Invented Tamsin"),
        merged(13, 12, "Invented Ulla"),
        merged(14, 20, "Invented Vere"),
    ]);
    for n in [11, 12, 13, 14] {
        assert!(!d.person(n).meta.deleted, "{n}");
    }
    assert_eq!((out.persons_removed_with, out.chains_unknown), (0, 0));
}

/// P3: no speaker row stays linked to a removed person. A live row goes to the first free
/// "Speaker N" of its meeting, unlinked, not ignored, stamped here; a tombstoned row loses its
/// name; a role-"Me" row linked to a "Me" tombstone is left linked. Control: on the code before
/// T14.49 the rows kept the removed person.
#[test]
fn store_sync_removed_p3_no_speaker_row_stays_linked_to_a_removed_person() {
    let mut d = Dev::new("removed-p3");
    d.store.put_person(tomb(10, 100, HERE)).unwrap();
    d.store.put_person(me_tomb(20, 100, HERE)).unwrap();
    let mut ignored = speaker(80, Some(10), "Invented Rowan", meta(5, HERE, false));
    ignored.ignored = true;
    d.store.put_speaker(ignored).unwrap();
    d.store
        .put_speaker(speaker(81, Some(10), "Invented Rowan", meta(6, HERE, true)))
        .unwrap();
    d.store
        .put_speaker(speaker(82, None, "Speaker 1", meta(7, HERE, false)))
        .unwrap();
    let mut mine = speaker(83, Some(20), "Me", meta(8, HERE, false));
    mine.role = SpeakerRole::Me;
    d.store.put_speaker(mine).unwrap();
    d.sent();
    let mine_before = d.raw("speaker", 83);
    let out = d.apply(&[SyncedRow::Person(live(15, "Invented Wren", 200, THERE))]);
    let s = d.speaker(80);
    assert_eq!(
        (s.person_id, s.display_name.as_str(), s.ignored),
        (None, "Speaker 2", false)
    );
    assert_eq!(s.meta.updated_at.node, HERE);
    let t = d.speaker(81);
    assert_eq!((t.person_id, t.display_name.as_str()), (None, ""));
    assert!(t.meta.deleted);
    assert_eq!(d.raw("speaker", 83), mine_before, "left linked to the user");
    assert_eq!(out.speakers_unlinked, 2);
    assert!(out.meetings.contains(&id(1)) && out.people);
    assert!(d.queued(80) && d.queued(81) && !d.queued(83));
    assert!(out.stamped_here.contains(&id(80)) && out.stamped_here.contains(&id(81)));
}

/// P3 on pulled rows: a pulled speaker row naming a person removed here is written and unlinked
/// on the same page; one naming a person not known here is written and left, then unlinked when
/// that person's tombstone arrives.
#[test]
fn store_sync_removed_p3_pulled_rows_naming_a_removed_or_unknown_person() {
    let mut d = Dev::new("removed-p3-pulled");
    d.store.put_person(tomb(10, 100, HERE)).unwrap();
    d.sent();
    let out = d.apply(&[
        SyncedRow::Speaker(speaker(
            80,
            Some(10),
            "Invented Rowan",
            meta(200, THERE, false),
        )),
        SyncedRow::Speaker(speaker(
            81,
            Some(11),
            "Invented Sasha",
            meta(201, THERE, false),
        )),
    ]);
    let s = d.speaker(80);
    assert_eq!((s.person_id, s.display_name.as_str()), (None, "Speaker 1"));
    assert!(s.meta.updated_at > at(200, THERE));
    assert_eq!(d.speaker(81).person_id, Some(id(11)), "left");
    assert_eq!(out.speakers_unlinked, 1);
    let out = d.apply(&[SyncedRow::Person(tomb(11, 300, THERE))]);
    let s = d.speaker(81);
    assert_eq!((s.person_id, s.display_name.as_str()), (None, "Speaker 2"));
    assert_eq!(out.speakers_unlinked, 1);
}

/// Tombstone against tombstone is plain last writer wins: nothing is stamped again.
#[test]
fn store_sync_removed_tombstone_against_tombstone_is_last_writer_wins() {
    for later in [true, false] {
        let mut d = Dev::new("removed-tomb-tomb");
        d.store.put_person(tomb(10, 100, HERE)).unwrap();
        d.sent();
        let dt = if later { 200 } else { 50 };
        let out = d.apply(&[SyncedRow::Person(tomb(10, dt, THERE))]);
        let p = d.person(10);
        assert!(p.meta.deleted);
        assert_eq!(
            p.meta.updated_at,
            if later { at(200, THERE) } else { at(100, HERE) }
        );
        assert!(!d.queued(10));
        assert_eq!(out.tombstones_restamped, 0);
    }
}

/// A page applied twice writes nothing more: P1 ignores a copy that is not later, P2 and P3 find
/// nothing left.
#[test]
fn store_sync_removed_a_page_applied_twice_writes_nothing_more() {
    let mut d = Dev::new("removed-twice");
    d.store.put_person(tomb(10, 300, HERE)).unwrap();
    d.store.put_voiceprint(print(31, 11)).unwrap();
    d.sent();
    let page = d.page(&[
        SyncedRow::Person(live(10, "Invented Rowan", 400, THERE)),
        SyncedRow::Person(person(
            11,
            "Invented Sasha",
            false,
            Some(10),
            meta(200, THERE, false),
        )),
        SyncedRow::Speaker(speaker(
            80,
            Some(10),
            "Invented Rowan",
            meta(200, THERE, false),
        )),
    ]);
    let first = d.apply_incoming(&page, false);
    assert_eq!(
        (
            first.tombstones_restamped,
            first.persons_removed_with,
            first.speakers_unlinked
        ),
        (1, 1, 1)
    );
    let before = d.dump();
    let again = d.apply_incoming(&page, false);
    assert_eq!(d.dump(), before, "nothing written");
    assert_eq!(
        (
            again.written,
            again.tombstones_restamped,
            again.persons_removed_with,
            again.speakers_unlinked,
            again.erased
        ),
        (0, 0, 0, 0, 0)
    );
    assert!(again.stamped_here.is_empty());
}

/// P3 over damaged speaker rows (D1): a live row with a damaged `role` linked to a removed person
/// is unlinked by rowid ("Speaker N", `ignored` 0, this device's stamp) and stays unreadable and
/// unsendable; a tombstoned row whose only damage is its name gets the constant `''` and becomes
/// a readable, sendable tombstone; a row whose `person_id` does not read is left byte for byte.
/// Control: on the code before T14.49 none was written.
#[test]
fn store_sync_removed_p3_over_damaged_speaker_rows() {
    let mut d = Dev::new("removed-p3-damaged");
    d.store.put_person(tomb(10, 100, HERE)).unwrap();
    d.store
        .put_speaker(speaker(
            80,
            Some(10),
            "Invented Rowan",
            meta(5, HERE, false),
        ))
        .unwrap();
    d.store
        .put_speaker(speaker(81, Some(10), "Invented Rowan", meta(6, HERE, true)))
        .unwrap();
    d.store
        .put_speaker(speaker(
            82,
            Some(10),
            "Invented Rowan",
            meta(7, HERE, false),
        ))
        .unwrap();
    d.sql("UPDATE speaker SET role = 'bogus' WHERE id = ?1", 80);
    d.sql(
        "UPDATE speaker SET display_name = CAST(X'FF00FE' AS TEXT) WHERE id = ?1",
        81,
    );
    d.sql(
        "UPDATE speaker SET person_id = X'0102030405060708090A0B0C0D0E0F' WHERE id = ?1",
        82,
    );
    d.sent();
    let untouched = d.raw("speaker", 82);
    let out = d.apply(&[SyncedRow::Person(live(15, "Invented Wren", 200, THERE))]);
    let c = d.conn();
    let row = |n: u8| -> (Option<Vec<u8>>, i64, String) {
        c.query_row(
            "SELECT person_id, ignored, display_name FROM speaker WHERE id = ?1",
            rusqlite::params![blob(n)],
            |r| Ok((r.get(0)?, r.get(1)?, format!("{:?}", r.get_ref(2)?))),
        )
        .unwrap()
    };
    assert_eq!(row(80), (None, 0, text("Speaker 1")));
    assert!(d.store.speakers(id(1)).is_err(), "80 still does not read");
    assert_eq!(row(81), (None, 0, text("")));
    assert_eq!(
        d.raw("speaker", 82),
        untouched,
        "a person_id that does not read"
    );
    let page = d.store.outbox_page(100, &[], SyncKinds::ALL).unwrap();
    assert!(
        page.rows.iter().any(|r| r.id() == id(81)),
        "81 reads and is sendable"
    );
    assert!(!page.rows.iter().any(|r| r.id() == id(80)));
    assert_eq!(page.unreadable, 1, "80 is unsendable");
    assert_eq!(out.speakers_unlinked, 2);
    assert!(
        out.damaged_rows
            .contains(&("speaker", d.rowid("speaker", 82)))
    );
}

/// `chain_end` (D1): a chain through a step whose `merged_into` does not read is `Unknown`,
/// counted once in `chains_unknown` with the step in `damaged_skipped`, and neither rule 3a nor P2
/// changes anything. Controls: the readable chain erases and tombstones with `chains_unknown` 0;
/// a loop and a person not known here are `Unknown` with `chains_unknown` 0; a chain ending at a
/// "Me" tombstone erases nothing (`MeTombstone`).
#[test]
fn store_sync_removed_chain_end_unknown_is_counted_and_changes_nothing() {
    #[derive(Clone, Copy, PartialEq, Debug)]
    enum Case {
        Unreadable,
        Readable,
        Loop,
        NotKnown,
        MeTombstone,
    }
    for case in [
        Case::Unreadable,
        Case::Readable,
        Case::Loop,
        Case::NotKnown,
        Case::MeTombstone,
    ] {
        let mut d = Dev::new("removed-chain-end");
        if case == Case::MeTombstone {
            d.store.put_person(me_tomb(10, 100, HERE)).unwrap();
        } else {
            d.store.put_person(tomb(10, 100, HERE)).unwrap();
        }
        let into = match case {
            Case::Loop => 12,
            Case::NotKnown => 99,
            _ => 10,
        };
        d.store
            .put_person(person(
                11,
                "Invented Sasha",
                false,
                Some(into),
                meta(100, HERE, false),
            ))
            .unwrap();
        if case == Case::Loop {
            d.store
                .put_person(person(
                    12,
                    "Invented Tamsin",
                    false,
                    Some(11),
                    meta(100, HERE, false),
                ))
                .unwrap();
        }
        d.store.put_voiceprint(print(31, 11)).unwrap();
        if case == Case::Unreadable {
            d.sql(
                "UPDATE person SET merged_into = X'0102030405060708090A0B0C0D0E0F' WHERE id = ?1",
                11,
            );
        }
        d.sent();
        let out = d.apply(&[SyncedRow::Person(live(15, "Invented Wren", 200, THERE))]);
        let erased = case == Case::Readable;
        assert_eq!(!d.live_row("voiceprint", 31), erased, "{case:?}");
        assert_eq!(!d.live_row("person", 11), erased, "{case:?}");
        assert_eq!(out.persons_removed_with, usize::from(erased), "{case:?}");
        let unknown = case == Case::Unreadable;
        assert_eq!(out.chains_unknown, usize::from(unknown), "{case:?}");
        assert_eq!(
            out.damaged_rows
                .contains(&("person", d.rowid("person", 11))),
            unknown,
            "{case:?}"
        );
    }
}

/// The counts (7A.4, 7A.9): a removal of a person with no voice erases names only (`erased > 0`,
/// `biometrics_forgotten` 0); a skipped live child of a deleted meeting is kept
/// (`deletions_kept`) but not stamped again; a meeting tombstone stamped again counts in
/// `tombstones_restamped` and `stamped_here`.
#[test]
fn store_sync_removed_counts_erased_and_restamped_tombstones() {
    let mut d = Dev::new("removed-counts");
    d.store
        .put_person(live(10, "Invented Rowan", 100, HERE))
        .unwrap();
    d.store
        .put_speaker(speaker(
            80,
            Some(10),
            "Invented Rowan",
            meta(5, HERE, false),
        ))
        .unwrap();
    d.sent();
    let out = d.apply(&[SyncedRow::Person(tomb(10, 200, THERE))]);
    assert_eq!(out.biometrics_forgotten, 0);
    assert_eq!(out.erased, 2, "the person's name and the speaker's");

    let mut d = Dev::new("removed-counts-meeting");
    d.store
        .put_meeting(meeting(1, meta(100, HERE, true)))
        .unwrap();
    d.sent();
    let out = d.apply(&[SyncedRow::Speaker(speaker(
        80,
        None,
        "Speaker 1",
        meta(200, THERE, false),
    ))]);
    assert_eq!((out.deletions_kept, out.tombstones_restamped), (1, 0));
    assert!(out.stamped_here.is_empty());
    let out = d.apply(&[SyncedRow::Meeting(meeting(1, meta(300, THERE, false)))]);
    assert_eq!((out.deletions_kept, out.tombstones_restamped), (1, 1));
    assert!(out.stamped_here.contains(&id(1)));
    assert!(d.queued(1));
}

/// The first page after a start runs every repair even with no rows: a speaker row linked to a
/// person removed here and a live person merged into one, left by an earlier build, are mended
/// once; a second run writes nothing. Control: a plain empty page mends nothing.
#[test]
fn store_sync_removed_every_repair_with_no_rows_mends_what_an_earlier_build_left() {
    let mut d = Dev::new("removed-every-repair");
    d.store.put_person(tomb(10, 100, HERE)).unwrap();
    d.store
        .put_person(person(
            11,
            "Invented Sasha",
            false,
            Some(10),
            meta(100, HERE, false),
        ))
        .unwrap();
    d.store
        .put_speaker(speaker(
            80,
            Some(10),
            "Invented Rowan",
            meta(5, HERE, false),
        ))
        .unwrap();
    d.sent();
    let before = d.dump();
    let out = d.apply_incoming(&[], false);
    assert_eq!(d.dump(), before, "a plain empty page mends nothing");
    assert_eq!((out.persons_removed_with, out.speakers_unlinked), (0, 0));
    let out = d.apply_incoming(&[], true);
    assert!(d.person(11).meta.deleted);
    assert_eq!(d.speaker(80).person_id, None);
    assert_eq!((out.persons_removed_with, out.speakers_unlinked), (1, 1));
    let before = d.dump();
    let out = d.apply_incoming(&[], true);
    assert_eq!(d.dump(), before);
    assert_eq!((out.persons_removed_with, out.speakers_unlinked), (0, 0));
}

// ---- rule 4c and P3 after it (the meeting of docs/sync-spec.md section 7 rule 4) ----

const MIC: u8 = 2;
const LINE: u8 = 5;
const OLD_SPEAKER: u8 = 6;
const NEW_SPEAKER: u8 = 7;
const RUN: u8 = 8;

fn line(n: u8, speaker: u8, m: SyncMeta, run: Option<u8>) -> SegmentRow {
    let words = vec![
        Word::new("shall", 1_000, 1_300, 0.9),
        Word::new("we", 1_400, 1_700, 0.9),
    ];
    let mut s = Segment::from_words(id(n), id(MIC), words, MeetingTime::ZERO);
    s.speaker_id = Some(id(speaker));
    SegmentRow {
        meta: m,
        meeting_id: id(1),
        pass: TranscriptPass::Final,
        segment: s,
        text_edited_at: None,
        pass_run: run.map(id),
    }
}

/// A meeting whose old speaker a re-transcription took out, its tombstone holding `name` and
/// `person`, and a new live "Speaker 1"; then the user's edited line naming the old speaker.
fn revived(tag: &str, name: &str, person: Option<u8>) -> (Dev, Applied) {
    let mut d = Dev::new(tag);
    d.store
        .put_track(Track {
            id: id(MIC),
            meta: meta(0, THERE, false),
            meeting_id: id(1),
            kind: TrackId::Mic,
            device_name: None,
            clock_domain: ClockDomain::Replay,
        })
        .unwrap();
    d.store.put_person(tomb(10, 100, HERE)).unwrap();
    d.store
        .put_speaker(speaker(OLD_SPEAKER, person, name, meta(150, THERE, true)))
        .unwrap();
    d.store
        .put_speaker(speaker(
            NEW_SPEAKER,
            None,
            "Speaker 1",
            meta(150, THERE, false),
        ))
        .unwrap();
    let mut old = line(LINE, OLD_SPEAKER, meta(150, THERE, true), None);
    old.segment.text = String::new();
    old.segment.words.clear();
    d.store.put_segment(old).unwrap();
    d.store
        .put_segment(line(11, NEW_SPEAKER, meta(150, THERE, false), Some(RUN)))
        .unwrap();
    d.sent();
    let mut edited = line(LINE, OLD_SPEAKER, meta(120, THERE, false), None);
    edited.segment.text = "shall we go".into();
    edited.segment.words.clear();
    edited.text_edited_at = Some(at(120, THERE));
    let out = d.apply(&[SyncedRow::Segment(edited)]);
    (d, out)
}

/// Rule 4c: a tombstoned speaker row whose stored name is empty, named by a live edited line,
/// comes back as the first free "Speaker N", never nameless. Control: on the code before T14.49
/// it came back with no name.
#[test]
fn store_sync_removed_rule_4c_gives_a_nameless_revived_row_a_speaker_name() {
    let (d, out) = revived("removed-4c-nameless", "", None);
    assert_eq!(out.speakers_revived, 1);
    let s = d.speaker(OLD_SPEAKER);
    assert!(!s.meta.deleted);
    assert_eq!(s.display_name, "Speaker 2");
}

/// Order (A5): a row rule 4c revives with its stored person, a person removed here, is unlinked
/// by P3 in the same page. Control: on the code before T14.49 it came back linked.
#[test]
fn store_sync_removed_p3_unlinks_a_row_rule_4c_revived_on_the_same_page() {
    let (d, out) = revived("removed-4c-p3", "Invented Rowan", Some(10));
    assert_eq!((out.speakers_revived, out.speakers_unlinked), (1, 1));
    let s = d.speaker(OLD_SPEAKER);
    assert!(!s.meta.deleted);
    assert_eq!((s.person_id, s.display_name.as_str()), (None, "Speaker 2"));
}
