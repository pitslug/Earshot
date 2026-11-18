//! T11.3da: migration 7, summary offload requests in the store and their sync rules
//! (`cargo test -p earshot-store offload`; docs/sync-spec.md section 10.1).

mod common;

use std::path::PathBuf;

use common::TempDir;
use earshot_core::identity::{DeviceId, EntityId, HlcTimestamp, ManualClock, SyncMeta};
use earshot_core::model::{
    Meeting, MeetingRepo, RequestState, SummaryRequest, SummaryRequestRepo, repo_contract,
};
use earshot_core::prompt::Template;
use earshot_store::{ApplyRules, Incoming, SCHEMA_VERSION, Store, SyncKinds, SyncedRow};

/// The asker and the target of the requests here.
const ASKER: DeviceId = DeviceId::from_bytes([0x11; 16]);
const TARGET: DeviceId = DeviceId::from_bytes([0x22; 16]);

const T0: u64 = 1_800_000_000_000;

/// The opt-in switches with voiceprints `on` and Dictionary words on (T11.3e).
fn vp(on: bool) -> SyncKinds {
    SyncKinds {
        voiceprints: on,
        dictionary: true,
    }
}

fn id(n: u8) -> EntityId {
    EntityId::from_bytes([0x01, 0x90, 0, 0, 0, 0, 0x70, 0, 0x80, 0, 0, 0, 0, 0, n, 2])
}

fn at(wall_ms: u64, node: DeviceId) -> HlcTimestamp {
    HlcTimestamp {
        wall_ms,
        logical: 0,
        node,
    }
}

fn meeting(n: u8) -> Meeting {
    let stamp = at(T0, ASKER);
    Meeting {
        id: id(n),
        meta: SyncMeta {
            created_at: stamp,
            updated_at: stamp,
            deleted: false,
            origin_device_id: ASKER,
        },
        title: "Invented planning".into(),
        started_unix_ms: T0,
        utc_offset_minutes: 600,
        duration_ms: Some(60_000),
        tags: Vec::new(),
        template: Template::General,
        folder_id: None,
        summary_kept_local: false,
    }
}

fn request(n: u8, meeting: u8, state: RequestState, updated: HlcTimestamp) -> SummaryRequest {
    SummaryRequest {
        id: id(n),
        meta: SyncMeta {
            created_at: at(T0 + 1, ASKER),
            updated_at: updated,
            deleted: false,
            origin_device_id: ASKER,
        },
        meeting_id: id(meeting),
        target_device: TARGET,
        asking_device: ASKER,
        segment_count: 40,
        state,
        reason: String::new(),
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
        body: vec![0x5A; 32],
        entity: Some(row.table().into()),
        row: Some(row),
    }
}

/// A store on disk that applies pages as device `me`.
struct Env {
    store: Store,
    clock: ManualClock,
    me: DeviceId,
    _dir: TempDir,
}

fn env(name: &str, me: DeviceId) -> Env {
    let dir = TempDir::new(name);
    let store = Store::open(&dir.path().join("earshot.db")).unwrap();
    Env {
        store,
        clock: ManualClock::new(T0 + 60_000),
        me,
        _dir: dir,
    }
}

impl Env {
    fn apply(
        &mut self,
        page: &[Incoming],
        leased: &dyn Fn(EntityId) -> bool,
    ) -> earshot_store::Applied {
        let mut hlc = self.store.hlc(self.me).unwrap();
        let rules = ApplyRules {
            kinds: vp(false),
            leased,
            now_ms: T0 + 60_000,
            after: None,
        };
        self.store
            .apply_remote(page, &rules, &mut hlc, &self.clock)
            .unwrap()
    }

    fn state(&self, n: u8) -> SummaryRequest {
        self.store.summary_request(id(n)).unwrap().unwrap()
    }
}

fn free(_: EntityId) -> bool {
    false
}

#[test]
fn store_offload_schema_v6_database_gains_the_request_table() {
    let dir = TempDir::new("offload-v6");
    let path: PathBuf = dir.path().join("earshot.db");
    let meetings_before: i64 = {
        let c = rusqlite::Connection::open(&path).unwrap();
        c.execute_batch(include_str!("data/schema_v6.sql")).unwrap();
        let v: u32 = c
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(v, 6, "the fixture is a schema-6 database");
        c.query_row("SELECT count(*) FROM meeting", [], |r| r.get(0))
            .unwrap()
    };
    let outbox_before = {
        let c = rusqlite::Connection::open(&path).unwrap();
        c.query_row("SELECT count(*) FROM sync_outbox", [], |r| {
            r.get::<_, i64>(0)
        })
        .unwrap()
    };
    let mut store = Store::open(&path).unwrap();
    assert_eq!(store.schema_version().unwrap(), SCHEMA_VERSION);
    const { assert!(SCHEMA_VERSION >= 7) };
    assert!(store.summary_requests().unwrap().is_empty());
    // The migration queues nothing new and keeps every meeting.
    assert_eq!(
        store.outbox_len().unwrap(),
        u64::try_from(outbox_before).unwrap()
    );
    let meetings = store.meetings().unwrap();
    assert_eq!(i64::try_from(meetings.len()).unwrap(), meetings_before);

    // A request written now is queued by the new triggers and sent like any row.
    let m = meetings.first().unwrap().id;
    let mut r = request(1, 0, RequestState::Requested, at(T0 + 5, ASKER));
    r.meeting_id = m;
    store.put_summary_request(r.clone()).unwrap();
    let out = store.outbox(10_000).unwrap();
    assert!(
        out.iter()
            .any(|row| matches!(row, SyncedRow::SummaryRequest(q) if *q == r)),
        "the request is in the outbox"
    );
    assert_eq!(
        store.outbox_len().unwrap(),
        u64::try_from(outbox_before).unwrap() + 1
    );
}

#[test]
fn store_offload_in_memory_passes_the_summary_request_contract() {
    repo_contract::summary_request_repo(|| Store::open_in_memory().unwrap());
}

#[test]
fn store_offload_on_disk_passes_the_summary_request_contract() {
    let dir = TempDir::new("offload-contract");
    let n = std::cell::Cell::new(0);
    repo_contract::summary_request_repo(|| {
        n.set(n.get() + 1);
        Store::open(&dir.path().join(format!("c{}.db", n.get()))).unwrap()
    });
}

#[test]
fn store_offload_request_decodes_and_seals_as_its_own_entity() {
    let r = request(1, 9, RequestState::Declined, at(T0 + 5, TARGET));
    let record = serde_json::to_value(&r).unwrap();
    assert_eq!(
        SyncedRow::decode("summary_request", record.clone()).unwrap(),
        Some(SyncedRow::SummaryRequest(r.clone()))
    );
    let row = SyncedRow::SummaryRequest(r);
    assert_eq!(row.table(), "summary_request");
    assert_eq!(row.meeting(), Some(id(9)));
    assert!(SyncedRow::decode("meeting", record).is_err());
}

/// A request holds no content and must reach a device whose summary job holds the meeting, so
/// it is never held as busy; the meeting's other rows still are.
#[test]
fn store_offload_a_request_applies_while_its_meeting_is_in_use() {
    let mut e = env("offload-busy", TARGET);
    e.store.put_meeting(meeting(5)).unwrap();
    let leased = |m: EntityId| m == id(5);
    let mut edited = meeting(5);
    edited.title = "Invented planning, renamed".into();
    edited.meta.updated_at = at(T0 + 9, ASKER);
    let page = [
        incoming(
            1,
            SyncedRow::SummaryRequest(request(1, 5, RequestState::Requested, at(T0 + 5, ASKER))),
        ),
        incoming(2, SyncedRow::Meeting(edited)),
    ];
    let applied = e.apply(&page, &leased);
    assert_eq!(applied.written, 1, "{applied:?}");
    assert_eq!(applied.held[2], 1, "the meeting row waits: {applied:?}");
    assert_eq!(e.state(1).state, RequestState::Requested);
}

/// Rule 1: a pulled withdrawal replaces an open request whatever the stamps (here the target
/// claimed after the asker withdrew, before it heard of it).
#[test]
fn store_offload_a_withdrawal_replaces_a_later_claim() {
    let mut e = env("offload-take", TARGET);
    e.store.put_meeting(meeting(5)).unwrap();
    e.store
        .put_summary_request(request(1, 5, RequestState::Claimed, at(T0 + 20, TARGET)))
        .unwrap();
    let withdrawn = request(1, 5, RequestState::Withdrawn, at(T0 + 10, ASKER));
    let applied = e.apply(
        &[incoming(1, SyncedRow::SummaryRequest(withdrawn.clone()))],
        &free,
    );
    assert_eq!(applied.written, 1, "{applied:?}");
    assert_eq!(
        e.state(1),
        withdrawn,
        "written as it came: the target stamps no withdrawal"
    );
    assert!(
        !e.store
            .outbox(100)
            .unwrap()
            .iter()
            .any(|r| r.table() == "summary_request"),
        "an applied withdrawal is not sent back, and the claim it replaced is not sent"
    );
    // A later done answer stays last writer wins: withdrawal and summary can both happen.
    let done = request(1, 5, RequestState::Done, at(T0 + 30, TARGET));
    e.apply(
        &[incoming(2, SyncedRow::SummaryRequest(done.clone()))],
        &free,
    );
    assert_eq!(e.state(1), done);
}

/// Rule 2: on the asker a withdrawn request never opens again; a later open copy is refused and
/// the withdrawal stamped again here so it wins at the relay. An older copy changes nothing.
#[test]
fn store_offload_the_asker_keeps_its_withdrawal() {
    let mut e = env("offload-keep", ASKER);
    e.store.put_meeting(meeting(5)).unwrap();
    let mine = request(1, 5, RequestState::Withdrawn, at(T0 + 10, ASKER));
    e.store.put_summary_request(mine.clone()).unwrap();
    e.store
        .outbox_sent(&[("summary_request", mine.id, mine.meta.updated_at)])
        .unwrap();

    let older = request(1, 5, RequestState::Claimed, at(T0 + 5, TARGET));
    let applied = e.apply(&[incoming(1, SyncedRow::SummaryRequest(older))], &free);
    assert_eq!(
        (applied.ignored, applied.withdrawals_kept),
        (1, 0),
        "{applied:?}"
    );
    assert_eq!(e.state(1), mine);

    let later = request(1, 5, RequestState::Claimed, at(T0 + 20, TARGET));
    let applied = e.apply(&[incoming(2, SyncedRow::SummaryRequest(later))], &free);
    assert_eq!(applied.withdrawals_kept, 1, "{applied:?}");
    let now = e.state(1);
    assert_eq!(now.state, RequestState::Withdrawn);
    assert_eq!(now.meta.updated_at.node, ASKER, "the asker stamped it");
    assert!(now.meta.updated_at > at(T0 + 20, TARGET), "{now:?}");
    let out = e.store.outbox(100).unwrap();
    assert!(
        out.iter().any(
            |r| matches!(r, SyncedRow::SummaryRequest(q) if q.state == RequestState::Withdrawn)
        ),
        "the withdrawal goes out again"
    );

    // A device that is not the asker takes the later open copy by last writer wins.
    let mut other = env("offload-third", DeviceId::from_bytes([0x33; 16]));
    other.store.put_meeting(meeting(5)).unwrap();
    other.store.put_summary_request(mine.clone()).unwrap();
    let later = request(1, 5, RequestState::Claimed, at(T0 + 20, TARGET));
    other.apply(
        &[incoming(1, SyncedRow::SummaryRequest(later.clone()))],
        &free,
    );
    assert_eq!(other.state(1), later);
}

/// A meeting being recorded leaves its requests in the outbox with its other rows.
#[test]
fn store_offload_a_recording_meetings_requests_wait_in_the_outbox() {
    let mut e = env("offload-outbox", ASKER);
    e.store.put_meeting(meeting(5)).unwrap();
    e.store
        .put_summary_request(request(1, 5, RequestState::Requested, at(T0 + 5, ASKER)))
        .unwrap();
    let except = e.store.outbox_except(100, &[id(5)], vp(false)).unwrap();
    assert!(except.is_empty(), "{except:?}");
    assert_eq!(e.store.outbox_len_except(&[id(5)], vp(false)).unwrap(), 0);
    assert_eq!(e.store.outbox_len_except(&[], vp(false)).unwrap(), 2);
}
