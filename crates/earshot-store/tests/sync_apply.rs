//! T11.1b: migration 6, the outbox and applying pulled rows
//! (`cargo test -p earshot-store sync`; docs/sync-spec.md sections 5.2 and 5.3).

mod common;

use std::path::PathBuf;

use common::TempDir;
use earshot_core::capture::TrackId;
use earshot_core::diar::EmbeddingModelId;
use earshot_core::identity::{DeviceId, EntityId, HlcTimestamp, ManualClock, SyncMeta};
use earshot_core::model::{ClockDomain, Meeting, MeetingRepo, Track};
use earshot_core::prompt::Template;
use earshot_core::voiceprint::{Person, Voiceprint, VoiceprintRepo};
use earshot_store::{ApplyRules, HoldReason, Incoming, MAX_CLOCK_AHEAD_MS, Store, SyncedRow};

/// This device and another one.
const HERE: DeviceId = DeviceId::from_bytes([0x11; 16]);
const THERE: DeviceId = DeviceId::from_bytes([0x22; 16]);

const T0: u64 = 1_800_000_000_000;

fn id(n: u8) -> EntityId {
    EntityId::from_bytes([0x01, 0x90, 0, 0, 0, 0, 0x70, 0, 0x80, 0, 0, 0, 0, 0, n, 1])
}

fn at(wall_ms: u64, logical: u32, node: DeviceId) -> HlcTimestamp {
    HlcTimestamp {
        wall_ms,
        logical,
        node,
    }
}

fn meta(updated: HlcTimestamp) -> SyncMeta {
    SyncMeta {
        created_at: at(T0, 0, updated.node),
        updated_at: updated,
        deleted: false,
        origin_device_id: updated.node,
    }
}

fn meeting(n: u8, title: &str, updated: HlcTimestamp) -> Meeting {
    Meeting {
        id: id(n),
        meta: meta(updated),
        title: title.into(),
        started_unix_ms: T0,
        utc_offset_minutes: 600,
        duration_ms: Some(60_000),
        tags: Vec::new(),
        template: Template::General,
        folder_id: None,
        summary_kept_local: false,
    }
}

fn track(n: u8, meeting: u8, updated: HlcTimestamp) -> Track {
    Track {
        id: id(n),
        meta: meta(updated),
        meeting_id: id(meeting),
        kind: TrackId::Mic,
        device_name: None,
        clock_domain: ClockDomain::Replay,
    }
}

/// A pulled row with a made-up row key and sealed body.
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
        row: Some(row),
    }
}

struct Env {
    store: Store,
    clock: ManualClock,
    _dir: TempDir,
}

fn env(name: &str) -> Env {
    let dir = TempDir::new(name);
    let store = Store::open(&dir.path().join("earshot.db")).unwrap();
    Env {
        store,
        clock: ManualClock::new(T0 + 10_000),
        _dir: dir,
    }
}

fn free(_: EntityId) -> bool {
    false
}

fn rules(voiceprints: bool) -> ApplyRules<'static> {
    ApplyRules {
        voiceprints,
        leased: &free,
        now_ms: T0 + 10_000,
    }
}

impl Env {
    fn apply(&mut self, page: &[Incoming], rules: &ApplyRules<'_>) -> earshot_store::Applied {
        let mut hlc = self.store.hlc(HERE).unwrap();
        self.store
            .apply_remote(page, rules, &mut hlc, &self.clock)
            .unwrap()
    }

    fn outbox_ids(&self) -> Vec<EntityId> {
        self.store
            .outbox(1000)
            .unwrap()
            .iter()
            .map(SyncedRow::id)
            .collect()
    }
}

fn held(n: usize, reason: HoldReason) -> [usize; 4] {
    let mut h = [0; 4];
    h[HoldReason::ALL.iter().position(|r| *r == reason).unwrap()] = n;
    h
}

#[test]
fn store_sync_schema_v5_database_queues_every_existing_row() {
    let dir = TempDir::new("sync-v5");
    let path: PathBuf = dir.path().join("earshot.db");
    {
        let c = rusqlite::Connection::open(&path).unwrap();
        c.execute_batch(include_str!("data/schema_v5.sql")).unwrap();
    }
    let rows_before: i64 = {
        let c = rusqlite::Connection::open(&path).unwrap();
        [
            "meeting",
            "track",
            "speaker",
            "segment",
            "note",
            "summary",
            "person",
            "voiceprint",
            "folder",
        ]
        .iter()
        .map(|t| {
            c.query_row(&format!("SELECT count(*) FROM {t}"), [], |r| {
                r.get::<_, i64>(0)
            })
            .unwrap()
        })
        .sum()
    };
    assert!(rows_before > 0, "the fixture holds rows");
    let store = Store::open(&path).unwrap();
    // Per table: every synced table holds a row in the fixture, and each is queued.
    {
        let c = rusqlite::Connection::open(&path).unwrap();
        for t in [
            "meeting",
            "track",
            "speaker",
            "segment",
            "note",
            "summary",
            "person",
            "voiceprint",
            "folder",
        ] {
            let rows: i64 = c
                .query_row(&format!("SELECT count(*) FROM {t}"), [], |r| r.get(0))
                .unwrap();
            let queued: i64 = c
                .query_row(
                    "SELECT count(*) FROM sync_outbox WHERE tbl = ?1",
                    [t],
                    |r| r.get(0),
                )
                .unwrap();
            assert!(rows > 0, "the fixture has no {t} row");
            assert_eq!(queued, rows, "{t}");
        }
    }
    // Every row from before sync existed is queued, so turning sync on later sends it.
    assert_eq!(
        store.outbox_len().unwrap(),
        u64::try_from(rows_before).unwrap()
    );
    assert_eq!(
        store.outbox(10_000).unwrap().len(),
        usize::try_from(rows_before).unwrap()
    );
    assert!(store.held_rows().unwrap().is_empty());
}

#[test]
fn store_sync_outbox_names_local_changes_in_stamp_order_until_sent() {
    let mut e = env("sync-outbox");
    assert_eq!(e.store.outbox_len().unwrap(), 0);
    // The outbox reads by updated_at, not by write order: the track, written second but
    // stamped first, comes first.
    e.store
        .put_meeting(meeting(1, "Invented", at(T0 + 5, 0, HERE)))
        .unwrap();
    e.store.put_track(track(2, 1, at(T0 + 2, 0, HERE))).unwrap();
    assert_eq!(e.outbox_ids(), [id(2), id(1)]);
    assert_eq!(e.store.outbox(1).unwrap().len(), 1, "the limit holds");
    // Sent: it leaves only if unchanged since it was read.
    let rows = e.store.outbox(10).unwrap();
    let sent: Vec<(&str, EntityId, HlcTimestamp)> = rows
        .iter()
        .map(|r| (r.table(), r.id(), r.meta().updated_at))
        .collect();
    let mut changed = meeting(1, "Edited on the way", at(T0 + 6, 0, HERE));
    changed.duration_ms = Some(61_000);
    e.store.put_meeting(changed).unwrap();
    assert_eq!(
        e.store.outbox_sent(&sent).unwrap(),
        1,
        "the track left, the meeting stayed"
    );
    assert_eq!(e.outbox_ids(), [id(1)]);
    // An update re-queues a sent row.
    let rows = e.store.outbox(10).unwrap();
    let sent: Vec<_> = rows
        .iter()
        .map(|r| (r.table(), r.id(), r.meta().updated_at))
        .collect();
    assert_eq!(e.store.outbox_sent(&sent).unwrap(), 1);
    assert_eq!(e.store.outbox_len().unwrap(), 0);
    e.store.put_track(track(2, 1, at(T0 + 7, 0, HERE))).unwrap();
    assert_eq!(e.outbox_ids(), [id(2)]);
}

#[test]
fn store_sync_later_stamp_wins_whole_rows_and_sends_nothing_back() {
    let mut e = env("sync-lww");
    e.store
        .put_meeting(meeting(1, "Local title", at(T0 + 5, 0, HERE)))
        .unwrap();
    e.store
        .outbox_sent(&[("meeting", id(1), at(T0 + 5, 0, HERE))])
        .unwrap();
    // Older and equal remote copies change nothing.
    for older in [at(T0 + 4, 9, THERE), at(T0 + 5, 0, HERE)] {
        let a = e.apply(
            &[incoming(1, SyncedRow::Meeting(meeting(1, "Remote", older)))],
            &rules(false),
        );
        assert_eq!((a.written, a.ignored), (0, 1), "{older:?}");
        assert_eq!(
            e.store.meeting(id(1)).unwrap().unwrap().title,
            "Local title"
        );
    }
    // A later one replaces the whole row with its own metadata, and is not queued to send.
    let later = at(T0 + 5, 1, THERE);
    let a = e.apply(
        &[incoming(2, SyncedRow::Meeting(meeting(1, "Remote", later)))],
        &rules(false),
    );
    assert_eq!(a.written, 1);
    let got = e.store.meeting(id(1)).unwrap().unwrap();
    assert_eq!(got.title, "Remote");
    assert_eq!(got.meta.updated_at, later);
    assert_eq!(
        e.store.outbox_len().unwrap(),
        0,
        "a pulled row is not pushed back"
    );
    // The clock observed it: the next local stamp is later than the remote one.
    let mut hlc = e.store.hlc(HERE).unwrap();
    assert!(hlc.now(&e.clock) > later);
}

#[test]
fn store_sync_row_without_its_meeting_is_held_then_applied() {
    let mut e = env("sync-parent");
    let t = incoming(1, SyncedRow::Track(track(2, 1, at(T0 + 2, 0, THERE))));
    let a = e.apply(std::slice::from_ref(&t), &rules(false));
    assert_eq!((a.written, a.held), (0, held(1, HoldReason::Parent)));
    assert!(e.store.is_held(&t.key).unwrap());
    let h = e.store.held_rows().unwrap();
    assert_eq!(h.len(), 1);
    assert_eq!(
        (h[0].reason, h[0].seq, h[0].stamp),
        (HoldReason::Parent, 1, t.stamp)
    );
    assert_eq!(h[0].body, t.body, "held still sealed");
    assert!(
        e.store.tracks(id(1)).unwrap().is_empty(),
        "nothing of the track was written"
    );
    // The meeting arrives; the held track is tried again and applies.
    let m = incoming(
        2,
        SyncedRow::Meeting(meeting(1, "Remote", at(T0 + 1, 0, THERE))),
    );
    let a = e.apply(&[m, t.clone()], &rules(false));
    assert_eq!(a.written, 2);
    assert!(!e.store.is_held(&t.key).unwrap());
    assert_eq!(e.store.tracks(id(1)).unwrap().len(), 1);
    // Rows earlier in a page count as present for later ones (a meeting then its track).
    let mut e = env("sync-parent-page");
    let a = e.apply(
        &[
            incoming(
                1,
                SyncedRow::Meeting(meeting(3, "Remote", at(T0 + 1, 0, THERE))),
            ),
            incoming(2, SyncedRow::Track(track(4, 3, at(T0 + 2, 0, THERE)))),
        ],
        &rules(false),
    );
    assert_eq!((a.written, a.held), (2, [0; 4]));
}

#[test]
fn store_sync_rows_of_a_meeting_in_use_wait_for_it() {
    let mut e = env("sync-busy");
    let m = incoming(
        1,
        SyncedRow::Meeting(meeting(1, "Remote", at(T0 + 1, 0, THERE))),
    );
    let busy = |m: EntityId| m == id(1);
    let leased = ApplyRules {
        voiceprints: false,
        leased: &busy,
        now_ms: T0 + 10_000,
    };
    let a = e.apply(std::slice::from_ref(&m), &leased);
    assert_eq!(a.held, held(1, HoldReason::Busy));
    assert!(e.store.meeting(id(1)).unwrap().is_none());
    // Control: once free it applies.
    let a = e.apply(&[m], &rules(false));
    assert_eq!(a.written, 1);
}

#[test]
fn store_sync_stamp_far_ahead_and_unreadable_rows_are_held() {
    let mut e = env("sync-clock");
    let ahead = at(T0 + 10_000 + MAX_CLOCK_AHEAD_MS + 1, 0, THERE);
    let a = e.apply(
        &[incoming(
            1,
            SyncedRow::Meeting(meeting(1, "Far ahead", ahead)),
        )],
        &rules(false),
    );
    assert_eq!(a.held, held(1, HoldReason::Clock));
    // Exactly at the limit is accepted.
    let edge = at(T0 + 10_000 + MAX_CLOCK_AHEAD_MS, 0, THERE);
    let a = e.apply(
        &[incoming(
            2,
            SyncedRow::Meeting(meeting(2, "At the edge", edge)),
        )],
        &rules(false),
    );
    assert_eq!(a.written, 1);
    // A row a newer Earshot wrote is held, never dropped.
    let newer = Incoming {
        key: [7; 16],
        seq: 3,
        stamp: at(T0, 0, THERE),
        body: vec![1, 2, 3],
        row: None,
    };
    let a = e.apply(&[newer], &rules(false));
    assert_eq!(a.held, held(1, HoldReason::Format));
    let reasons: Vec<HoldReason> = e
        .store
        .held_rows()
        .unwrap()
        .iter()
        .map(|h| h.reason)
        .collect();
    assert_eq!(reasons, [HoldReason::Clock, HoldReason::Format]);
}

fn voiceprint(n: u8, person: u8, updated: HlcTimestamp) -> Voiceprint {
    Voiceprint {
        id: id(n),
        meta: meta(updated),
        person_id: id(person),
        model: EmbeddingModelId::new("invented-model"),
        sum: vec![0.5; 4],
        count: 1,
        speech_ms: 5_000,
    }
}

fn person(n: u8, deleted: bool, updated: HlcTimestamp) -> Person {
    let mut m = meta(updated);
    m.deleted = deleted;
    Person {
        id: id(n),
        meta: m,
        display_name: if deleted {
            String::new()
        } else {
            "Invented Person".into()
        },
        is_me: false,
        merged_into: None,
    }
}

#[test]
fn store_sync_outbox_leaves_out_a_recording_meeting_and_switched_off_voiceprints() {
    let mut e = env("sync-outbox-except");
    // Meeting 1 is being recorded: its rows are the oldest and outnumber the limit.
    e.store
        .put_meeting(meeting(1, "Recording", at(T0 + 1, 0, HERE)))
        .unwrap();
    for n in 10..16 {
        e.store
            .put_track(track(n, 1, at(T0 + 2 + u64::from(n), 0, HERE)))
            .unwrap();
    }
    e.store
        .put_meeting(meeting(2, "Saved", at(T0 + 100, 0, HERE)))
        .unwrap();
    e.store
        .put_track(track(3, 2, at(T0 + 101, 0, HERE)))
        .unwrap();
    e.store
        .put_person(person(4, false, at(T0 + 102, 0, HERE)))
        .unwrap();
    e.store
        .put_voiceprint(voiceprint(5, 4, at(T0 + 103, 0, HERE)))
        .unwrap();
    let ids = |rows: Vec<SyncedRow>| rows.iter().map(SyncedRow::id).collect::<Vec<_>>();
    // A limit of 2 still reaches the saved meeting: the recording's rows are not read at all.
    assert_eq!(
        ids(e.store.outbox_except(2, &[id(1)], true).unwrap()),
        [id(2), id(3)]
    );
    assert_eq!(
        ids(e.store.outbox_except(10, &[id(1)], true).unwrap()),
        [id(2), id(3), id(4), id(5)]
    );
    assert_eq!(
        ids(e.store.outbox_except(10, &[id(1)], false).unwrap()),
        [id(2), id(3), id(4)],
        "voiceprints wait while their switch is off"
    );
    // Nothing left out: every row, oldest first, as `outbox` reads them.
    let all = ids(e.store.outbox_except(100, &[], true).unwrap());
    assert_eq!(all, ids(e.store.outbox(100).unwrap()));
    assert_eq!(all.len(), 11);
    assert_eq!(all[0], id(1));
    // The skipped rows stay queued.
    assert_eq!(e.store.outbox_len().unwrap(), 11);
}

#[test]
fn store_sync_queue_everything_requeues_sent_rows_and_drops_held_ones() {
    let mut e = env("sync-queue-all");
    e.store
        .put_meeting(meeting(1, "Sent before", at(T0 + 1, 0, HERE)))
        .unwrap();
    e.store.put_track(track(2, 1, at(T0 + 2, 0, HERE))).unwrap();
    let sent: Vec<(&str, EntityId, HlcTimestamp)> = e
        .store
        .outbox(10)
        .unwrap()
        .iter()
        .map(|r| (r.table(), r.id(), r.meta().updated_at))
        .collect();
    assert_eq!(e.store.outbox_sent(&sent).unwrap(), 2);
    assert_eq!(e.store.outbox_len().unwrap(), 0);
    // A row held under the old vault.
    let orphan = incoming(9, SyncedRow::Track(track(3, 7, at(T0 + 3, 0, THERE))));
    let a = e.apply(&[orphan], &rules(false));
    assert_eq!(a.held, held(1, HoldReason::Parent));
    assert_eq!(e.store.queue_everything().unwrap(), 2);
    assert_eq!(e.outbox_ids(), [id(1), id(2)]);
    assert!(e.store.held_rows().unwrap().is_empty());
    // Queued once only.
    assert_eq!(e.store.queue_everything().unwrap(), 0);
    assert_eq!(e.store.outbox_len().unwrap(), 2);
}

#[test]
fn store_sync_a_live_meeting_never_overwrites_a_deletion_here() {
    let mut e = env("sync-deletion-final");
    let mut gone = meeting(1, "Deleted here", at(T0 + 5, 0, HERE));
    gone.meta.deleted = true;
    e.store.put_meeting(gone).unwrap();
    let sent: Vec<(&str, EntityId, HlcTimestamp)> = e
        .store
        .outbox(10)
        .unwrap()
        .iter()
        .map(|r| (r.table(), r.id(), r.meta().updated_at))
        .collect();
    e.store.outbox_sent(&sent).unwrap();
    // Edited elsewhere after the deletion: a later stamp, live.
    let edited = incoming(
        1,
        SyncedRow::Meeting(meeting(1, "Renamed there", at(T0 + 9, 0, THERE))),
    );
    let a = e.apply(std::slice::from_ref(&edited), &rules(false));
    assert_eq!((a.written, a.deletions_kept), (0, 1), "{a:?}");
    let here = e.store.meeting(id(1)).unwrap().unwrap();
    assert!(here.meta.deleted, "the meeting stays deleted");
    assert!(
        here.meta.updated_at > at(T0 + 9, 0, THERE),
        "the tombstone is stamped after the edit, so it wins at the relay"
    );
    assert_eq!(e.outbox_ids(), [id(1)], "and it goes out again");
    // The same edit again, now older than the tombstone: nothing to do.
    let a = e.apply(&[edited], &rules(false));
    assert_eq!((a.ignored, a.deletions_kept), (1, 0), "{a:?}");
    // A tombstone pulled over a live row still applies as ever.
    let mut del = meeting(2, "Live here", at(T0 + 1, 0, HERE));
    e.store.put_meeting(del.clone()).unwrap();
    del.meta.deleted = true;
    del.meta.updated_at = at(T0 + 2, 0, THERE);
    let a = e.apply(&[incoming(2, SyncedRow::Meeting(del))], &rules(false));
    assert_eq!(a.written, 1);
    assert!(e.store.meeting(id(2)).unwrap().unwrap().meta.deleted);
}

#[test]
fn store_sync_voiceprints_apply_only_when_switched_on() {
    let mut e = env("sync-voiceprint");
    let v = incoming(
        1,
        SyncedRow::Voiceprint(voiceprint(2, 1, at(T0 + 1, 0, THERE))),
    );
    let a = e.apply(std::slice::from_ref(&v), &rules(false));
    assert_eq!((a.written, a.skipped), (0, 1));
    assert!(e.store.voiceprints_of(id(1)).unwrap().is_empty());
    let a = e.apply(&[v], &rules(true));
    assert_eq!(a.written, 1);
    assert_eq!(e.store.voiceprints_of(id(1)).unwrap().len(), 1);
}

#[test]
fn store_sync_person_deleted_elsewhere_forgets_their_voice_here() {
    let mut e = env("sync-person");
    e.store
        .put_person(person(1, false, at(T0 + 1, 0, HERE)))
        .unwrap();
    e.store
        .put_voiceprint(voiceprint(2, 1, at(T0 + 1, 0, HERE)))
        .unwrap();
    let a = e.apply(
        &[incoming(
            1,
            SyncedRow::Person(person(1, true, at(T0 + 2, 0, THERE))),
        )],
        &rules(false),
    );
    assert_eq!(a.written, 1);
    assert_eq!(a.biometrics_forgotten, 1);
    let v = &e.store.voiceprints_of(id(1)).unwrap()[0];
    assert!(v.meta.deleted);
    assert!(v.sum.iter().all(|x| *x == 0.0), "the print is zeroed");
    // Control: a live person arriving forgets nothing.
    let mut e = env("sync-person-live");
    e.store
        .put_voiceprint(voiceprint(4, 3, at(T0 + 1, 0, HERE)))
        .unwrap();
    let a = e.apply(
        &[incoming(
            1,
            SyncedRow::Person(person(3, false, at(T0 + 2, 0, THERE))),
        )],
        &rules(false),
    );
    assert_eq!(a.biometrics_forgotten, 0);
    assert!(!e.store.voiceprints_of(id(3)).unwrap()[0].meta.deleted);
}

#[test]
fn store_sync_decode_reads_every_synced_entity_and_holds_unknown_ones() {
    let m = meeting(1, "Invented", at(T0, 0, THERE));
    let record = serde_json::to_value(&m).unwrap();
    assert_eq!(
        SyncedRow::decode("meeting", record.clone()).unwrap(),
        Some(SyncedRow::Meeting(m))
    );
    assert_eq!(
        SyncedRow::decode("from_the_future", record.clone()).unwrap(),
        None
    );
    assert!(SyncedRow::decode("track", record).is_err());
}

/// Regression (found writing T11.1b): the store writes with upserts, and SQLite replaces a
/// trigger's own conflict clause with the firing statement's, so an `INSERT OR IGNORE`
/// trigger failed on a row's second change. Four tables are written three times here; the
/// trigger text of all nine is pinned by `store_sync_triggers_cover_every_synced_table`.
#[test]
fn store_sync_a_row_changed_again_and_again_stays_one_outbox_entry() {
    let mut e = env("sync-again");
    for n in 0..3 {
        e.store
            .put_meeting(meeting(1, "Invented", at(T0 + 10 + n, 0, HERE)))
            .unwrap();
        e.store
            .put_track(track(2, 1, at(T0 + 20 + n, 0, HERE)))
            .unwrap();
        e.store
            .put_person(person(3, false, at(T0 + 30 + n, 0, HERE)))
            .unwrap();
        e.store
            .put_voiceprint(voiceprint(4, 3, at(T0 + 40 + n, 0, HERE)))
            .unwrap();
    }
    assert_eq!(e.store.outbox_len().unwrap(), 4);
}

#[test]
fn store_sync_tombstone_of_a_person_never_seen_here_is_kept_and_forgets_nothing() {
    let mut e = env("sync-person-unknown");
    e.store
        .put_voiceprint(voiceprint(6, 5, at(T0 + 1, 0, HERE)))
        .unwrap();
    let a = e.apply(
        &[incoming(
            1,
            SyncedRow::Person(person(7, true, at(T0 + 2, 0, THERE))),
        )],
        &rules(false),
    );
    assert_eq!((a.written, a.biometrics_forgotten), (1, 0));
    assert!(e.store.person(id(7)).unwrap().unwrap().meta.deleted);
    assert!(
        !e.store.voiceprints_of(id(5)).unwrap()[0].meta.deleted,
        "another person's print stays"
    );
    // The same tombstone again is not newer: ignored, nothing runs twice.
    let a = e.apply(
        &[incoming(
            2,
            SyncedRow::Person(person(7, true, at(T0 + 2, 0, THERE))),
        )],
        &rules(false),
    );
    assert_eq!((a.written, a.ignored), (0, 1));
}

#[test]
fn store_sync_a_row_the_store_refuses_rolls_back_the_whole_page() {
    let mut e = env("sync-rollback");
    let good = incoming(
        1,
        SyncedRow::Meeting(meeting(1, "Fine", at(T0 + 1, 0, THERE))),
    );
    let mut bad = meeting(2, "Start time past what SQLite holds", at(T0 + 1, 0, THERE));
    bad.started_unix_ms = u64::MAX;
    let mut hlc = e.store.hlc(HERE).unwrap();
    let r = e.store.apply_remote(
        &[good, incoming(2, SyncedRow::Meeting(bad))],
        &rules(false),
        &mut hlc,
        &e.clock,
    );
    assert!(r.is_err());
    assert!(
        e.store.meeting(id(1)).unwrap().is_none(),
        "the earlier row was rolled back too"
    );
    assert!(e.store.held_rows().unwrap().is_empty());
}

#[test]
fn store_sync_a_held_copy_newer_than_a_retried_row_stays() {
    let mut e = env("sync-unhold");
    // A track whose meeting has not arrived is held at stamp T0 + 5.
    let newer = incoming(1, SyncedRow::Track(track(2, 1, at(T0 + 5, 0, THERE))));
    e.apply(std::slice::from_ref(&newer), &rules(false));
    // An older copy of the same row arrives and is ignored once the meeting and a local copy
    // exist: it must not delete the newer held copy.
    e.store
        .put_meeting(meeting(1, "Invented", at(T0 + 1, 0, HERE)))
        .unwrap();
    e.store.put_track(track(2, 1, at(T0 + 4, 0, HERE))).unwrap();
    let mut older = incoming(2, SyncedRow::Track(track(2, 1, at(T0 + 3, 0, THERE))));
    older.key = newer.key;
    let a = e.apply(&[older], &rules(false));
    assert_eq!(a.ignored, 1);
    assert!(
        e.store.is_held(&newer.key).unwrap(),
        "the newer held copy stays"
    );
    // Retrying the held copy applies it and clears it.
    let a = e.apply(std::slice::from_ref(&newer), &rules(false));
    assert_eq!(a.written, 1);
    assert!(!e.store.is_held(&newer.key).unwrap());
}
