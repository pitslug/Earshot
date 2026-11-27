//! T14.49b (docs/sync-spec.md 7A.2 P5): a removed identity's centroids are found whatever order
//! the rows arrived in. `SpeakerIdentity::linked_before` remembers the persons a row was linked
//! to before a write moved it away, `identities::relinked` is the one rule, and
//! `forget_identity` drops an entry whose `linked_before` holds a member of the identity.
//! Straight `apply_remote` pages; damage is made by SQL from a second connection; erasure is
//! checked on disk with a second connection kept open. Every name is invented.
//! `cargo test -p earshot-store sync`.

mod common;

use std::collections::BTreeMap;

use common::TempDir;
use earshot_core::asr::{MeetingTime, Segment, Word};
use earshot_core::capture::TrackId;
use earshot_core::diar::{Embedding, EmbeddingModelId, SpeakerRole};
use earshot_core::identity::{DeviceId, EntityId, HlcTimestamp, ManualClock, SyncMeta};
use earshot_core::model::{ClockDomain, Meeting, MeetingRepo, SegmentRow, Speaker, Track};
use earshot_core::prompt::Template;
use earshot_core::voiceprint::{
    Person, SampleRecord, Voiceprint, VoiceprintRepo, VoiceprintSample,
};
use earshot_store::identities::{
    IdentityRecord, RowRelink, SpeakerIdentity, StoredIdentities, decode, encode, record_key,
    relinked,
};
use earshot_store::{Applied, ApplyRules, ChainEnd, Incoming, Store, SyncKinds, SyncedRow};

const HERE: DeviceId = DeviceId::from_bytes([0x11; 16]);
const THERE: DeviceId = DeviceId::from_bytes([0x22; 16]);
const T0: u64 = 1_800_000_000_000;
const NOW: u64 = T0 + 10_000;
const DAY_MS: u64 = 24 * 60 * 60 * 1000;
const MIC: u8 = 70;

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

fn person(n: u8, name: &str, merged_into: Option<u8>, m: SyncMeta) -> Person {
    Person {
        id: id(n),
        meta: m,
        display_name: if m.deleted {
            String::new()
        } else {
            name.into()
        },
        is_me: false,
        merged_into: merged_into.map(id),
    }
}

fn live(n: u8, name: &str, dt: u64, node: DeviceId) -> Person {
    person(n, name, None, meta(dt, node, false))
}

fn tomb(n: u8, dt: u64, node: DeviceId) -> Person {
    person(n, "", None, meta(dt, node, true))
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

/// A centroid value no other stored byte holds, by speaker: the on-disk needle is its JSON text.
fn centroid_value(speaker: u8) -> f32 {
    0.314_159_27 + f32::from(speaker) * 0.000_131
}

fn needle(speaker: u8) -> Vec<u8> {
    serde_json::to_string(&centroid_value(speaker))
        .unwrap()
        .into_bytes()
}

/// An entry a user's link left (`given_person` `None`), with a centroid of its own.
fn entry(speaker: u8, given: Option<u8>) -> SpeakerIdentity {
    SpeakerIdentity {
        speaker: id(speaker),
        track: TrackId::Reference,
        centroid: Embedding::new(model(), vec![centroid_value(speaker), 0.25]),
        given_name: "Invented given".into(),
        given_person: given.map(id),
        given_stamp: None,
        user_edited: true,
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

/// Whether `needle` is in the database file, its WAL or its journal.
fn on_disk(db: &std::path::Path, needle: &[u8]) -> bool {
    let mut found = false;
    for suffix in ["", "-wal", "-journal"] {
        let p = std::path::PathBuf::from(format!("{}{suffix}", db.display()));
        if let Ok(bytes) = std::fs::read(&p) {
            found |= bytes.windows(needle.len()).any(|w| w == needle);
        }
    }
    found
}

struct Dev {
    store: Option<Store>,
    path: std::path::PathBuf,
    seq: u64,
    _dir: TempDir,
}

impl Dev {
    /// A store holding meeting 1 and its mic track, with nothing queued.
    fn new(tag: &str) -> Self {
        let dir = TempDir::new(tag);
        let path = dir.path().join("earshot.db");
        let mut store = Store::open(&path).unwrap();
        store
            .put_meeting(meeting(1, meta(0, THERE, false)))
            .unwrap();
        store
            .put_track(Track {
                id: id(MIC),
                meta: meta(0, THERE, false),
                meeting_id: id(1),
                kind: TrackId::Mic,
                device_name: None,
                clock_domain: ClockDomain::Replay,
            })
            .unwrap();
        let d = Self {
            store: Some(store),
            path,
            seq: 0,
            _dir: dir,
        };
        d.sent();
        d
    }

    fn s(&self) -> &Store {
        self.store.as_ref().unwrap()
    }

    fn sm(&mut self) -> &mut Store {
        self.store.as_mut().unwrap()
    }

    /// Closes and opens the store again (a later tick, after a restart).
    fn reopen(&mut self) {
        self.store = None;
        self.store = Some(Store::open(&self.path).unwrap());
    }

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
        self.apply_at(&page, NOW)
    }

    fn apply_at(&mut self, page: &[Incoming], now_ms: u64) -> Applied {
        let rules = ApplyRules {
            kinds: SyncKinds::ALL,
            leased: &free,
            now_ms,
            after: None,
        };
        let clock = ManualClock::new(now_ms.max(NOW));
        let store = self.store.as_mut().unwrap();
        let mut hlc = store.hlc(HERE).unwrap();
        let out = store.apply_remote(page, &rules, &mut hlc, &clock).unwrap();
        // The client's gate (docs/sync-spec.md 7A.4).
        if out.erased > 0 {
            store.flush_erasures().unwrap();
        }
        out
    }

    fn sql(&self, sql: &str, n: u8) {
        let c = self.conn();
        c.execute_batch("PRAGMA foreign_keys = OFF;").unwrap();
        let changed = c.execute(sql, rusqlite::params![blob(n)]).unwrap();
        assert_eq!(changed, 1, "{sql}");
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

    fn speaker(&self, n: u8) -> Speaker {
        self.s()
            .speakers(id(1))
            .unwrap()
            .into_iter()
            .find(|s| s.id == id(n))
            .unwrap()
    }

    fn put_record(&mut self, entries: Vec<SpeakerIdentity>) {
        self.sm()
            .set_local_value(
                &record_key(id(1)),
                &encode(&IdentityRecord { speakers: entries }),
            )
            .unwrap();
    }

    /// The stored entries by speaker, read through a connection of its own (from disk).
    fn entries(&self) -> BTreeMap<EntityId, SpeakerIdentity> {
        let bytes: Option<Vec<u8>> = self
            .conn()
            .query_row(
                "SELECT value FROM local_state WHERE key = ?1",
                rusqlite::params![record_key(id(1))],
                |r| r.get(0),
            )
            .ok();
        match bytes.map(|b| decode(&b)) {
            None => BTreeMap::new(),
            Some(StoredIdentities::Record(r)) => {
                r.speakers.into_iter().map(|e| (e.speaker, e)).collect()
            }
            Some(StoredIdentities::Unreadable(why)) => panic!("unreadable: {why}"),
        }
    }

    fn linked_before(&self, speaker: u8) -> Vec<EntityId> {
        self.entries()[&id(speaker)].linked_before.clone()
    }
}

/// `relinked`'s cases in their order, with every chain end: `from` removed gives `None` whatever
/// `to` is; `to` live clears `linked_before` without adding `from`; `from` otherwise is added,
/// once; neither leaves the entry. A `to` whose chain is `Removed`, `MeTombstone` or `Unknown`
/// keeps `from` (case 3, review 10 advisory 2); a `from` that is `MeTombstone` or `Unknown` is
/// added, never dropped (D1, P4).
#[test]
fn store_sync_removed_p5_relinked_cases_in_order() {
    let ends: BTreeMap<EntityId, ChainEnd> = [
        (id(10), ChainEnd::Removed),
        (id(11), ChainEnd::Live),
        (id(12), ChainEnd::Unknown),
        (id(13), ChainEnd::MeTombstone),
        (id(14), ChainEnd::Live),
    ]
    .into_iter()
    .collect();
    let end = |p: EntityId| ends[&p];
    let mut e = entry(80, None);
    e.linked_before = vec![id(14)];
    // 1. `from` removed: gone, whatever `to` is.
    for to in [None, Some(id(11)), Some(id(12)), Some(id(10))] {
        assert_eq!(relinked(&e, Some(id(10)), to, end), None, "{to:?}");
    }
    // 2. `to` live: cleared, `from` not added.
    let got = relinked(&e, Some(id(14)), Some(id(11)), end).unwrap();
    assert!(got.linked_before.is_empty());
    let got = relinked(&e, None, Some(id(11)), end).unwrap();
    assert!(got.linked_before.is_empty());
    // 3. `from` a person: added once; also when `to` cannot be shown to be live.
    let got = relinked(&e, Some(id(11)), None, end).unwrap();
    assert_eq!(got.linked_before, vec![id(14), id(11)]);
    let twice = relinked(&got, Some(id(11)), None, end).unwrap();
    assert_eq!(twice.linked_before, vec![id(14), id(11)], "added once");
    for to in [id(10), id(12), id(13)] {
        let got = relinked(&e, Some(id(11)), Some(to), end).unwrap();
        assert_eq!(got.linked_before, vec![id(14), id(11)], "to {to}");
    }
    for from in [id(12), id(13)] {
        let got = relinked(&e, Some(from), None, end).unwrap();
        assert_eq!(got.linked_before, vec![id(14), from], "from {from}");
    }
    // 4. Neither: unchanged.
    assert_eq!(relinked(&e, None, None, end), Some(e.clone()));
    assert_eq!(relinked(&e, None, Some(id(12)), end), Some(e.clone()));
    // Only `linked_before` changes.
    let got = relinked(&e, Some(id(11)), None, end).unwrap();
    assert_eq!(
        SpeakerIdentity {
            linked_before: e.linked_before.clone(),
            ..got
        },
        e
    );
}

/// `linked_before` is left out when empty, and a record holding it reads with the earlier
/// build's entry shape (version 2, the field ignored): the version stays 2.
#[test]
fn store_sync_removed_p5_linked_before_is_read_by_the_earlier_decoder() {
    /// The entry as builds before T14.49b read it (no `deny_unknown_fields`).
    #[derive(serde::Deserialize, Debug)]
    #[allow(dead_code)]
    struct EarlierEntry {
        speaker: EntityId,
        track: TrackId,
        centroid: Embedding,
        given_name: String,
        given_person: Option<EntityId>,
        #[serde(default)]
        given_stamp: Option<HlcTimestamp>,
        #[serde(default)]
        user_edited: bool,
        #[serde(default)]
        enrolled: bool,
        #[serde(default)]
        ignored: bool,
    }
    #[derive(serde::Deserialize, Debug)]
    struct EarlierRecord {
        speakers: Vec<EarlierEntry>,
    }
    let plain = encode(&IdentityRecord {
        speakers: vec![entry(80, None)],
    });
    assert!(!String::from_utf8_lossy(&plain).contains("linked_before"));
    let mut with = entry(80, None);
    with.linked_before = vec![id(10), id(11)];
    let bytes = encode(&IdentityRecord {
        speakers: vec![with.clone()],
    });
    assert_eq!(&bytes[..4], &2u32.to_le_bytes(), "version 2");
    let earlier: EarlierRecord = serde_json::from_slice(&bytes[4..]).unwrap();
    assert_eq!(earlier.speakers.len(), 1);
    assert_eq!(earlier.speakers[0].speaker, id(80));
    assert_eq!(earlier.speakers[0].given_person, None);
    match decode(&bytes) {
        StoredIdentities::Record(r) => assert_eq!(r.speakers, vec![with]),
        StoredIdentities::Unreadable(why) => panic!("{why}"),
    }
}

/// P5 at a pulled speaker row whose person differs from the local row's: unlinked from a live
/// person, the entry holds that person in `linked_before` (the meeting and people reported);
/// linked to a person not known here, nothing changes; unlinked again, that person is added;
/// linked to a live person, `linked_before` is cleared. Control: on the code before T14.49b
/// nothing changed.
#[test]
fn store_sync_removed_p5_a_pulled_row_relinks_its_entry() {
    let mut d = Dev::new("p5-pulled");
    d.sm()
        .put_person(live(10, "Invented Rowan", 1, HERE))
        .unwrap();
    d.sm()
        .put_person(live(11, "Invented Sasha", 1, HERE))
        .unwrap();
    d.sm()
        .put_speaker(speaker(
            80,
            Some(10),
            "Invented Rowan",
            meta(5, HERE, false),
        ))
        .unwrap();
    d.put_record(vec![entry(80, None), entry(81, None)]);
    d.sent();
    let out = d.apply(&[SyncedRow::Speaker(speaker(
        80,
        None,
        "Speaker 1",
        meta(200, THERE, false),
    ))]);
    assert_eq!(d.linked_before(80), vec![id(10)]);
    assert!(out.meetings.contains(&id(1)) && out.people);
    assert_eq!(out.erased, 0);
    assert!(d.linked_before(81).is_empty(), "another row's entry");
    d.apply(&[SyncedRow::Speaker(speaker(
        80,
        Some(12),
        "Invented Toby",
        meta(210, THERE, false),
    ))]);
    assert_eq!(
        d.linked_before(80),
        vec![id(10)],
        "linked to a person not known here"
    );
    d.apply(&[SyncedRow::Speaker(speaker(
        80,
        None,
        "Speaker 1",
        meta(220, THERE, false),
    ))]);
    assert_eq!(d.linked_before(80), vec![id(10), id(12)]);
    d.apply(&[SyncedRow::Speaker(speaker(
        80,
        Some(11),
        "Invented Sasha",
        meta(230, THERE, false),
    ))]);
    assert!(
        d.linked_before(80).is_empty(),
        "a link to a live person clears it"
    );
    // A pulled copy that does not change the person leaves the entry.
    let before = d.entries();
    let out = d.apply(&[SyncedRow::Speaker(speaker(
        80,
        Some(11),
        "Invented Sasha again",
        meta(240, THERE, false),
    ))]);
    assert_eq!(d.entries(), before);
    assert_eq!(out.erased, 0);
}

/// P5 at P3's write: every row P3 unlinks from a person removed here loses its entry
/// (`Applied::erased`, the meeting reported), other rows' entries stay, and a record left empty
/// is deleted. Control: on the code before T14.49b the entry stayed.
#[test]
fn store_sync_removed_p5_p3_drops_the_entry_of_each_row_it_unlinks() {
    let mut d = Dev::new("p5-p3");
    d.sm().put_person(tomb(10, 100, HERE)).unwrap();
    d.sm()
        .put_speaker(speaker(
            80,
            Some(10),
            "Invented Rowan",
            meta(5, HERE, false),
        ))
        .unwrap();
    d.sm()
        .put_speaker(speaker(82, None, "Speaker 1", meta(6, HERE, false)))
        .unwrap();
    d.put_record(vec![entry(80, None), entry(82, None)]);
    d.sent();
    let out = d.apply(&[SyncedRow::Person(live(15, "Invented Wren", 200, THERE))]);
    assert_eq!(d.speaker(80).person_id, None);
    let left = d.entries();
    assert!(!left.contains_key(&id(80)), "P3's row's entry dropped");
    assert!(left.contains_key(&id(82)), "another row's entry kept");
    // "Speaker N" (a name cleared) and the entry.
    assert_eq!(out.erased, 2);
    assert!(out.meetings.contains(&id(1)));
    // The last entry going deletes the record.
    let mut d = Dev::new("p5-p3-last");
    d.sm().put_person(tomb(10, 100, HERE)).unwrap();
    d.sm()
        .put_speaker(speaker(
            80,
            Some(10),
            "Invented Rowan",
            meta(5, HERE, false),
        ))
        .unwrap();
    d.put_record(vec![entry(80, None)]);
    d.sent();
    d.apply(&[SyncedRow::Person(live(15, "Invented Wren", 200, THERE))]);
    let present: i64 = d
        .conn()
        .query_row(
            "SELECT count(*) FROM local_state WHERE key = ?1",
            rusqlite::params![record_key(id(1))],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(present, 0, "no empty record kept");
}

/// A damaged local speaker row (D1): its earlier person is read from its own `person_id` column
/// (so the entry remembers it and a later tombstone drops it); when that column does not read
/// either, the pulled row drops the entry; and P3 over a row whose `person_id` does not read
/// leaves the row byte for byte and drops its entry. Control: on the code before T14.49b every
/// entry stayed.
#[test]
fn store_sync_removed_p5_a_damaged_local_speaker_row() {
    let mut d = Dev::new("p5-damaged");
    d.sm()
        .put_person(live(10, "Invented Rowan", 1, HERE))
        .unwrap();
    d.sm()
        .put_person(live(11, "Invented Sasha", 1, HERE))
        .unwrap();
    d.sm()
        .put_speaker(speaker(
            80,
            Some(10),
            "Invented Rowan",
            meta(5, HERE, false),
        ))
        .unwrap();
    d.sm()
        .put_speaker(speaker(
            81,
            Some(10),
            "Invented Rowan",
            meta(5, HERE, false),
        ))
        .unwrap();
    d.put_record(vec![entry(80, None), entry(81, None)]);
    d.sql("UPDATE speaker SET role = 'bogus' WHERE id = ?1", 80);
    d.sql(
        "UPDATE speaker SET role = 'bogus', person_id = X'01020304' WHERE id = ?1",
        81,
    );
    d.sent();
    let out = d.apply(&[
        SyncedRow::Speaker(speaker(80, None, "Speaker 1", meta(200, THERE, false))),
        SyncedRow::Speaker(speaker(
            81,
            Some(11),
            "Invented Sasha",
            meta(200, THERE, false),
        )),
    ]);
    let left = d.entries();
    assert_eq!(left[&id(80)].linked_before, vec![id(10)], "from its column");
    assert!(!left.contains_key(&id(81)), "neither readable: dropped");
    assert_eq!(out.erased, 1);
    let out = d.apply(&[SyncedRow::Person(tomb(10, 300, THERE))]);
    assert!(
        d.entries().is_empty(),
        "the tombstone finds 80 through linked_before"
    );
    assert!(out.erased > 0);

    // P3 over a row whose `person_id` does not read.
    let mut d = Dev::new("p5-damaged-p3");
    d.sm().put_person(tomb(10, 100, HERE)).unwrap();
    d.sm()
        .put_speaker(speaker(
            82,
            Some(10),
            "Invented Rowan",
            meta(7, HERE, false),
        ))
        .unwrap();
    d.put_record(vec![entry(82, None)]);
    d.sql(
        "UPDATE speaker SET person_id = X'01020304' WHERE id = ?1",
        82,
    );
    d.sent();
    let untouched = d.raw("speaker", 82);
    let out = d.apply(&[SyncedRow::Person(live(15, "Invented Wren", 200, THERE))]);
    assert_eq!(d.raw("speaker", 82), untouched, "not rewritten");
    assert!(d.entries().is_empty(), "its entry dropped");
    assert!(out.erased > 0);
}

/// The rows a removal leaves on B: P live with three speaker rows linked to P by B's user (no
/// `given_person`), each with an entry; A's rewritten rows (unlinked) and A's tombstone of P.
fn split_setup(tag: &str) -> (Dev, Vec<SyncedRow>) {
    let mut d = Dev::new(tag);
    d.sm()
        .put_person(live(10, "Invented Rowan", 1, HERE))
        .unwrap();
    for n in [80, 81, 82] {
        d.sm()
            .put_speaker(speaker(n, Some(10), "Invented Rowan", meta(5, HERE, false)))
            .unwrap();
    }
    d.put_record(vec![entry(80, None), entry(81, None), entry(82, None)]);
    d.sm().checkpoint().unwrap();
    d.sent();
    let rows = [80u8, 81, 82]
        .iter()
        .enumerate()
        .map(|(k, &n)| {
            SyncedRow::Speaker(speaker(
                n,
                None,
                &format!("Speaker {}", k + 1),
                meta(200 + k as u64, THERE, false),
            ))
        })
        .collect();
    (d, rows)
}

/// After the split cases: no entry for any of P's rows, read back from disk, and no centroid on
/// disk (a second connection kept open across the close).
fn assert_no_centroid(mut d: Dev, case: &str) {
    assert!(
        d.entries().is_empty(),
        "{case}: entries left {:?}",
        d.entries()
    );
    let second = d.conn();
    let _: i64 = second
        .query_row("SELECT count(*) FROM speaker", [], |r| r.get(0))
        .unwrap();
    d.sm().flush_erasures().unwrap();
    for n in [80, 81, 82] {
        assert!(
            !on_disk(&d.path, &needle(n)),
            "{case}: centroid of {n} on disk"
        );
    }
    d.store = None;
    for n in [80, 81, 82] {
        assert!(
            !on_disk(&d.path, &needle(n)),
            "{case}: centroid of {n} after close"
        );
    }
    drop(second);
}

/// B1 (docs/sync-spec.md 7A.6, "Speaker identities across pages and ticks"): an identity with
/// more speaker rows than a page. (1) The rewritten rows in page 1 and the tombstone in page 2
/// of one tick; (2) the tick stopped after page 1 and the tombstone applied in a later tick,
/// after a restart; (3) the tombstone held `clock` while page 1 applies, then released; (4) the
/// tombstone first (a holder stamped it again later) and the rows after. In every case B ends
/// with no entry of P's rows and no centroid on disk. Controls: the centroids are on disk before;
/// with `linked_before` taken out of `forget_identity`'s test, (1) to (3) fail (a mutation run).
#[test]
fn store_sync_removed_p5_split_1_one_tick_two_pages() {
    // Sanity: before, every centroid is on disk.
    let (d, _) = split_setup("p5-split-sanity");
    for n in [80, 81, 82] {
        assert!(
            on_disk(&d.path, &needle(n)),
            "centroid of {n} on disk before"
        );
    }
    drop(d);
    let (mut d, rows) = split_setup("p5-split-1");
    d.apply(&rows[..2]);
    d.apply(&[rows[2].clone(), SyncedRow::Person(tomb(10, 203, THERE))]);
    assert_no_centroid(d, "(1)");
}

/// B1 (2): the tombstone in a later tick, after a restart (the record kept on disk).
#[test]
fn store_sync_removed_p5_split_2_tombstone_in_a_later_tick() {
    let (mut d, rows) = split_setup("p5-split-2");
    d.apply(&rows);
    for n in [80, 81, 82] {
        assert_eq!(
            d.linked_before(n),
            vec![id(10)],
            "kept on disk across ticks"
        );
    }
    d.reopen();
    d.apply(&[SyncedRow::Person(tomb(10, 203, THERE))]);
    assert_no_centroid(d, "(2)");
}

/// B1 (3): the tombstone held `clock` while the rows apply, then released.
#[test]
fn store_sync_removed_p5_split_3_tombstone_held_then_released() {
    let (mut d, rows) = split_setup("p5-split-3");
    let ahead = 10_000 + DAY_MS + 60_000;
    let mut page = d.page(&rows);
    page.extend(d.page(&[SyncedRow::Person(tomb(10, ahead, THERE))]));
    let out = d.apply_at(&page, NOW);
    assert_eq!(out.held.iter().sum::<usize>(), 1, "the tombstone is held");
    assert!(!d.s().person(id(10)).unwrap().unwrap().meta.deleted);
    let held = page[3..].to_vec();
    d.apply_at(&held, T0 + ahead);
    assert!(d.s().person(id(10)).unwrap().unwrap().meta.deleted);
    assert_no_centroid(d, "(3)");
}

/// B1 (4): the tombstone first (a holder stamped it again later), the rows after.
#[test]
fn store_sync_removed_p5_split_4_tombstone_first() {
    let (mut d, rows) = split_setup("p5-split-4");
    d.apply(&[SyncedRow::Person(tomb(10, 300, THERE))]);
    d.apply(&rows);
    assert_no_centroid(d, "(4)");
}

/// The removing computer's form (7A.6): B links a row R to P after the removal and before it
/// hears of it. On A (P tombstoned) the pulled R is unlinked by P3 and A's entry for R goes; on
/// B, R's entry goes in both orders of A's unlinked R and the tombstone.
#[test]
fn store_sync_removed_p5_the_removing_computers_form() {
    // A.
    let mut a = Dev::new("p5-removing-a");
    a.sm().put_person(tomb(10, 100, HERE)).unwrap();
    a.sm()
        .put_speaker(speaker(81, None, "Speaker 2", meta(5, HERE, false)))
        .unwrap();
    a.put_record(vec![entry(81, None)]);
    a.sent();
    a.apply(&[SyncedRow::Speaker(speaker(
        81,
        Some(10),
        "Invented Rowan",
        meta(300, THERE, false),
    ))]);
    assert_eq!(a.speaker(81).person_id, None, "P3 unlinked it");
    assert!(a.entries().is_empty(), "A's entry for R dropped");
    // B, both orders.
    for tomb_first in [false, true] {
        let mut b = Dev::new("p5-removing-b");
        b.sm()
            .put_person(live(10, "Invented Rowan", 1, HERE))
            .unwrap();
        b.sm()
            .put_speaker(speaker(
                81,
                Some(10),
                "Invented Rowan",
                meta(300, HERE, false),
            ))
            .unwrap();
        b.put_record(vec![entry(81, None)]);
        b.sent();
        let unlinked = SyncedRow::Speaker(speaker(81, None, "Speaker 2", meta(400, THERE, false)));
        let tombstone = SyncedRow::Person(tomb(10, 401, THERE));
        if tomb_first {
            b.apply(&[tombstone]);
            b.apply(&[unlinked]);
        } else {
            b.apply(&[unlinked]);
            b.apply(&[tombstone]);
        }
        assert!(b.entries().is_empty(), "tombstone first: {tomb_first}");
    }
}

/// A page boundary between P2's merged person and its tombstoned target: C holds X live with a
/// row linked to X; B merged X into P offline and A removed P. Whichever of X's merge and P's
/// tombstone comes first, X ends tombstoned, the row unlinked and its entry gone.
#[test]
fn store_sync_removed_p5_a_page_boundary_between_a_merged_person_and_its_removed_target() {
    for tomb_first in [false, true] {
        let mut c = Dev::new("p5-p2-boundary");
        c.sm()
            .put_person(live(10, "Invented Rowan", 1, HERE))
            .unwrap();
        c.sm()
            .put_person(live(11, "Invented Sasha", 1, HERE))
            .unwrap();
        c.sm()
            .put_speaker(speaker(
                80,
                Some(11),
                "Invented Sasha",
                meta(5, HERE, false),
            ))
            .unwrap();
        c.put_record(vec![entry(80, None)]);
        c.sent();
        let merged = SyncedRow::Person(person(
            11,
            "Invented Sasha",
            Some(10),
            meta(200, THERE, false),
        ));
        let tombstone = SyncedRow::Person(tomb(10, 210, THERE));
        if tomb_first {
            c.apply(&[tombstone]);
            c.apply(&[merged]);
        } else {
            c.apply(&[merged]);
            c.apply(&[tombstone]);
        }
        assert!(
            c.s().person(id(11)).unwrap().unwrap().meta.deleted,
            "{tomb_first}"
        );
        assert_eq!(c.speaker(80).person_id, None, "{tomb_first}");
        assert!(c.entries().is_empty(), "{tomb_first}");
    }
}

/// The transcript as the owner's invariant names it: every speaker row's name, `person_id` and
/// `ignored`, every person row, and every segment's speaker, from disk.
fn transcript(d: &Dev) -> Vec<String> {
    let c = d.conn();
    let mut out = Vec::new();
    for sql in [
        "SELECT id, display_name, person_id, ignored FROM speaker ORDER BY id",
        "SELECT * FROM person ORDER BY id",
        "SELECT id, speaker_id FROM segment ORDER BY id",
    ] {
        let mut st = c.prepare(sql).unwrap();
        let k = st.column_count();
        let rows: Vec<String> = st
            .query_map([], |r| {
                (0..k)
                    .map(|i| Ok(format!("{:?}", r.get_ref(i)?)))
                    .collect::<Result<Vec<_>, _>>()
                    .map(|v| v.join("|"))
            })
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        out.extend(rows);
    }
    out
}

fn line(n: u8, speaker: u8) -> SegmentRow {
    let words = vec![
        Word::new("shall", 1_000, 1_300, 0.9),
        Word::new("we", 1_400, 1_700, 0.9),
    ];
    let mut s = Segment::from_words(id(n), id(MIC), words, MeetingTime::ZERO);
    s.speaker_id = Some(id(speaker));
    SegmentRow {
        meta: meta(5, HERE, false),
        meeting_id: id(1),
        pass: earshot_core::model::TranscriptPass::Final,
        segment: s,
        text_edited_at: None,
        pass_run: None,
    }
}

fn voiced(tag: &str) -> Dev {
    let mut d = Dev::new(tag);
    d.sm()
        .put_person(live(10, "Invented Rowan", 1, HERE))
        .unwrap();
    d.sm()
        .put_speaker(speaker(
            80,
            Some(10),
            "Invented Rowan",
            meta(5, HERE, false),
        ))
        .unwrap();
    let mut ignored = speaker(82, None, "Speaker 1", meta(5, HERE, false));
    ignored.ignored = true;
    d.sm().put_speaker(ignored).unwrap();
    d.sm().put_segment(line(60, 80)).unwrap();
    d.sm().put_segment(line(61, 82)).unwrap();
    d.sm()
        .put_voiceprint(Voiceprint::from_stored(
            id(31),
            meta(1, HERE, false),
            id(10),
            model(),
            vec![0.6, 0.8],
            1,
            5_000,
        ))
        .unwrap();
    d.sm()
        .put_sample(
            VoiceprintSample::from_stored(SampleRecord {
                id: id(41),
                meta: meta(1, HERE, false),
                voiceprint_id: Some(id(31)),
                person_id: Some(id(10)),
                model: Some(model()),
                vector: vec![1.0, 0.0],
                speech_ms: Some(4_000),
                meeting_id: Some(id(1)),
                speaker_id: Some(id(80)),
            })
            .unwrap(),
        )
        .unwrap();
    d.put_record(vec![entry(80, Some(10)), entry(82, None)]);
    d.sm().checkpoint().unwrap();
    d.sent();
    d
}

fn live_print(d: &Dev) -> bool {
    d.conn()
        .query_row(
            "SELECT count(*) FROM voiceprint WHERE deleted = 0",
            [],
            |r| r.get::<_, i64>(0),
        )
        .unwrap()
        > 0
}

/// Forgetting a voice keeps the transcript (the owner's invariant, 7A.2 P5): after
/// `forget_voiceprints(P)` and after a pulled print tombstone, every speaker row's name,
/// `person_id` and `ignored`, every person row and every segment's speaker are byte-equal; the
/// forget also leaves no centroid of P's row on disk. Positive control: the same comparison sees
/// `remove_person(P)`.
#[test]
fn store_sync_removed_p5_forgetting_a_voice_keeps_the_transcript() {
    let mut d = voiced("p5-forget");
    let before = transcript(&d);
    assert!(on_disk(&d.path, &needle(80)), "sanity");
    let second = d.conn();
    let mut hlc = d.s().hlc(HERE).unwrap();
    let clock = ManualClock::new(NOW);
    d.sm().forget_voiceprints(id(10), &mut hlc, &clock).unwrap();
    assert_eq!(transcript(&d), before, "forget_voiceprints");
    assert!(!live_print(&d));
    assert!(!d.entries().contains_key(&id(80)));
    assert!(
        d.entries().contains_key(&id(82)),
        "another voice's entry stays"
    );
    d.store = None;
    assert!(
        !on_disk(&d.path, &needle(80)),
        "the centroid is off the disk"
    );
    drop(second);

    let mut d = voiced("p5-forget-pulled");
    let before = transcript(&d);
    let gone = Voiceprint::from_stored(
        id(31),
        meta(200, THERE, true),
        id(10),
        model(),
        vec![0.0, 0.0],
        1,
        5_000,
    );
    d.apply(&[SyncedRow::Voiceprint(gone)]);
    assert!(!live_print(&d));
    assert_eq!(transcript(&d), before, "a pulled print tombstone");

    // Positive control: a removal changes what the comparison reads.
    let mut d = voiced("p5-forget-control");
    let before = transcript(&d);
    let mut hlc = d.s().hlc(HERE).unwrap();
    d.sm().remove_person(id(10), &mut hlc, &clock).unwrap();
    assert_ne!(transcript(&d), before, "the comparison sees a removal");
}

/// A relink lost to a removal (7A.6, review 7 advisory 6): B relinks a voice from P to Q.
/// Stamped before A's removal, A's unlinked row is the later write: the row ends unlinked, the
/// entry holds Q in `linked_before` and is kept through P's tombstone, and B's sample for Q stays
/// Q's. Stamped after: the row stays linked to Q and the entry is kept.
#[test]
fn store_sync_removed_p5_a_relink_lost_to_a_removal() {
    for relink_later in [false, true] {
        let mut b = Dev::new("p5-relink-lost");
        b.sm()
            .put_person(live(10, "Invented Rowan", 1, HERE))
            .unwrap();
        b.sm()
            .put_person(live(11, "Invented Sasha", 1, HERE))
            .unwrap();
        let link_dt = if relink_later { 500 } else { 150 };
        b.sm()
            .put_speaker(speaker(
                80,
                Some(11),
                "Invented Sasha",
                meta(link_dt, HERE, false),
            ))
            .unwrap();
        b.sm()
            .put_voiceprint(Voiceprint::from_stored(
                id(32),
                meta(link_dt, HERE, false),
                id(11),
                model(),
                vec![0.6, 0.8],
                1,
                5_000,
            ))
            .unwrap();
        b.sm()
            .put_sample(
                VoiceprintSample::from_stored(SampleRecord {
                    id: id(42),
                    meta: meta(link_dt, HERE, false),
                    voiceprint_id: Some(id(32)),
                    person_id: Some(id(11)),
                    model: Some(model()),
                    vector: vec![1.0, 0.0],
                    speech_ms: Some(4_000),
                    meeting_id: Some(id(1)),
                    speaker_id: Some(id(80)),
                })
                .unwrap(),
            )
            .unwrap();
        b.put_record(vec![entry(80, None)]);
        b.sent();
        b.apply(&[
            SyncedRow::Speaker(speaker(80, None, "Speaker 1", meta(200, THERE, false))),
            SyncedRow::Person(tomb(10, 201, THERE)),
        ]);
        let row = b.speaker(80);
        let left = b.entries();
        if relink_later {
            assert_eq!(row.person_id, Some(id(11)), "B's link is the later write");
            assert!(left[&id(80)].linked_before.is_empty());
        } else {
            assert_eq!(row.person_id, None, "A's unlink is the later write");
            assert_eq!(left[&id(80)].linked_before, vec![id(11)]);
        }
        let sample_live: i64 = b
            .conn()
            .query_row(
                "SELECT count(*) FROM voiceprint_sample WHERE deleted = 0 AND person_id = ?1",
                rusqlite::params![blob(11)],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            sample_live, 1,
            "B's sample for Q stays Q's ({relink_later})"
        );
    }
}

/// `forget_identity`'s `linked_before` test, from each erasure: `remove_person(P)` and
/// `forget_voiceprints(P)` drop an entry whose row was once linked to P and is now unlinked
/// (P5's cost for a forget: the entry only, never the row), and forgetting someone else keeps it.
#[test]
fn store_sync_removed_p5_forget_identity_finds_an_entry_by_linked_before() {
    let setup = |tag: &str| {
        let mut d = Dev::new(tag);
        d.sm()
            .put_person(live(10, "Invented Rowan", 1, HERE))
            .unwrap();
        d.sm()
            .put_person(live(11, "Invented Sasha", 1, HERE))
            .unwrap();
        d.sm()
            .put_speaker(speaker(80, None, "Speaker 1", meta(5, HERE, false)))
            .unwrap();
        let mut e = entry(80, None);
        e.linked_before = vec![id(10)];
        d.put_record(vec![e]);
        d
    };
    let clock = ManualClock::new(NOW);
    let mut d = setup("p5-forget-identity-remove");
    let mut hlc = d.s().hlc(HERE).unwrap();
    d.sm().remove_person(id(10), &mut hlc, &clock).unwrap();
    assert!(d.entries().is_empty(), "remove_person");
    let mut d = setup("p5-forget-identity-forget");
    let row_before = d.raw("speaker", 80);
    let mut hlc = d.s().hlc(HERE).unwrap();
    d.sm().forget_voiceprints(id(10), &mut hlc, &clock).unwrap();
    assert!(d.entries().is_empty(), "forget_voiceprints");
    assert_eq!(d.raw("speaker", 80), row_before, "the row is never touched");
    let mut d = setup("p5-forget-identity-other");
    let mut hlc = d.s().hlc(HERE).unwrap();
    d.sm().forget_voiceprints(id(11), &mut hlc, &clock).unwrap();
    assert_eq!(d.entries().len(), 1, "someone else's forget keeps it");
}

/// `row_relinked_tx` (the user edit's entry point) over the same rule: a row moved from a live
/// person to none adds that person; a move to a live person clears it; a move away from a person
/// removed here drops the entry and says so.
#[test]
fn store_sync_removed_p5_row_relinked_tx_applies_the_one_rule() {
    let mut d = Dev::new("p5-tx");
    d.sm()
        .put_person(live(10, "Invented Rowan", 1, HERE))
        .unwrap();
    d.sm()
        .put_person(live(11, "Invented Sasha", 1, HERE))
        .unwrap();
    d.sm().put_person(tomb(12, 1, HERE)).unwrap();
    d.put_record(vec![entry(80, None), entry(81, None)]);
    let got = d
        .sm()
        .transaction(|tx| {
            earshot_store::identities::row_relinked_tx(tx, id(1), id(80), Some(id(10)), None)
        })
        .unwrap();
    assert_eq!(got, RowRelink::Changed);
    assert_eq!(d.linked_before(80), vec![id(10)]);
    let got = d
        .sm()
        .transaction(|tx| {
            earshot_store::identities::row_relinked_tx(tx, id(1), id(80), None, Some(id(11)))
        })
        .unwrap();
    assert_eq!(got, RowRelink::Changed);
    assert!(d.linked_before(80).is_empty());
    let got = d
        .sm()
        .transaction(|tx| {
            earshot_store::identities::row_relinked_tx(tx, id(1), id(81), Some(id(12)), None)
        })
        .unwrap();
    assert!(got.dropped());
    assert!(!d.entries().contains_key(&id(81)));
    let got = d
        .sm()
        .transaction(|tx| {
            earshot_store::identities::row_relinked_tx(tx, id(1), id(99), Some(id(10)), None)
        })
        .unwrap();
    assert_eq!(got, RowRelink::Unchanged, "no entry for that row");
}
