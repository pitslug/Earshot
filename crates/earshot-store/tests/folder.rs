//! Folders (T9.5a): the folder table, `meeting.folder_id` and the schema-2 migration
//! (`cargo test -p earshot-store folder`).

mod common;

use std::path::{Path, PathBuf};
use std::sync::{Arc, Barrier};

use common::TempDir;
use earshot_core::identity::{DeviceId, EntityId, Hlc, HlcTimestamp, ManualClock, SyncMeta};
use earshot_core::model::{
    Folder, FolderRepo, Meeting, MeetingRepo, Stamp, delete_folder, repo_contract,
};
use earshot_core::prompt::Template;
use earshot_store::{SCHEMA_VERSION, Store};
use rand_chacha::ChaCha8Rng;
use rand_core::SeedableRng;

/// A schema-1 database as SQL text: the tables and indexes migration 1 creates (copied from
/// `sqlite_master`) and two invented meetings (one with a track, a final segment and a note,
/// one still recording) written once by the schema-1 store, dumped with Python's sqlite3
/// `iterdump`, then `PRAGMA user_version = 1`. Text rather than a `.db` file because
/// `public_guard` refuses tracked database files. The lib test
/// `schema::tests::store_folder_v1_fixture_has_exactly_the_schema_of_migration_1` keeps its
/// DDL identical to migration 1.
const SCHEMA_V1_SQL: &str = include_str!("data/schema_v1.sql");

struct World {
    clock: ManualClock,
    hlc: Hlc,
    rng: ChaCha8Rng,
}

impl World {
    fn new() -> Self {
        World {
            clock: ManualClock::new(1_800_000_000_000),
            hlc: Hlc::new(DeviceId::from_bytes([5; 16])),
            rng: ChaCha8Rng::seed_from_u64(17),
        }
    }
    fn stamp(&mut self) -> Stamp<'_, ManualClock, ChaCha8Rng> {
        self.clock.advance_ms(1);
        Stamp {
            clock: &self.clock,
            hlc: &mut self.hlc,
            rng: &mut self.rng,
        }
    }
}

/// Builds the schema-1 database in `dir` with raw SQLite (never `Store::open`).
fn build_v1(dir: &TempDir) -> PathBuf {
    let to = dir.path().join("earshot.db");
    let c = rusqlite::Connection::open(&to).unwrap();
    c.execute_batch(SCHEMA_V1_SQL).unwrap();
    // An installed schema-1 database is in WAL mode (the store sets it at every open), and
    // the journal mode is stored in the file.
    let mode: String = c
        .query_row("PRAGMA journal_mode = WAL", [], |r| r.get(0))
        .unwrap();
    assert_eq!(mode, "wal");
    to
}

fn user_version(path: &Path) -> u32 {
    let c = rusqlite::Connection::open(path).unwrap();
    c.query_row("PRAGMA user_version", [], |r| r.get(0))
        .unwrap()
}

fn columns(store_path: &Path, table: &str) -> Vec<String> {
    let c = rusqlite::Connection::open(store_path).unwrap();
    let mut st = c
        .prepare(&format!("SELECT name FROM pragma_table_info('{table}')"))
        .unwrap();
    st.query_map([], |r| r.get(0))
        .unwrap()
        .collect::<rusqlite::Result<Vec<String>>>()
        .unwrap()
}

#[test]
fn store_in_memory_passes_the_folder_repo_contract() {
    repo_contract::folder_repo(|| Store::open_in_memory().unwrap());
}

#[test]
fn store_on_disk_passes_the_folder_repo_contract() {
    let dir = TempDir::new("folder-contract");
    let n = std::cell::Cell::new(0);
    repo_contract::folder_repo(|| {
        n.set(n.get() + 1);
        Store::open(&dir.path().join(format!("f{}.db", n.get()))).unwrap()
    });
}

#[test]
fn store_folder_schema_v1_database_migrates_to_no_folders() {
    let dir = TempDir::new("folder-v1");
    let path = build_v1(&dir);
    assert_eq!(user_version(&path), 1, "the fixture is a schema-1 database");
    assert!(
        !columns(&path, "meeting").contains(&"folder_id".to_owned()),
        "the fixture predates folder_id"
    );

    let before = {
        let store = Store::open(&path).unwrap();
        assert_eq!(store.schema_version().unwrap(), 2);
        assert_eq!(SCHEMA_VERSION, 2);
        let meetings = store.meetings().unwrap();
        assert_eq!(meetings.len(), 2, "{meetings:?}");
        assert!(
            meetings.iter().all(|m| m.folder_id.is_none()),
            "existing meetings read as no folder: {meetings:?}"
        );
        assert_eq!(meetings[0].title, "Invented planning chat");
        assert_eq!(meetings[0].duration_ms, Some(120_000));
        assert_eq!(meetings[1].duration_ms, None);
        assert_eq!(store.tracks(meetings[0].id).unwrap().len(), 1);
        assert_eq!(store.segments(meetings[0].id).unwrap().len(), 1);
        assert_eq!(store.notes(meetings[0].id).unwrap().len(), 1);
        assert!(store.folders().unwrap().is_empty(), "nothing is backfilled");
        meetings
    };
    assert_eq!(user_version(&path), 2);
    assert!(columns(&path, "meeting").contains(&"folder_id".to_owned()));
    assert_eq!(
        columns(&path, "folder").last().map(String::as_str),
        Some("name")
    );

    // Reopening changes nothing.
    let mut store = Store::open(&path).unwrap();
    assert_eq!(store.schema_version().unwrap(), 2);
    assert_eq!(store.meetings().unwrap(), before);

    // A migrated meeting can be filed.
    let mut w = World::new();
    let folder = Folder::new("Projects", &mut w.stamp());
    store.put_folder(folder.clone()).unwrap();
    let mut filed = before[0].clone();
    filed.folder_id = Some(folder.id);
    filed.meta.touch(&mut w.hlc, &w.clock);
    store.put_meeting(filed.clone()).unwrap();
    drop(store);
    let store = Store::open(&path).unwrap();
    assert_eq!(store.meeting(filed.id).unwrap(), Some(filed));
    assert_eq!(store.folders().unwrap(), vec![folder]);
}

#[test]
fn store_folder_deleting_two_folders_in_a_row_leaves_two_cleared_tombstones() {
    let dir = TempDir::new("folder-two");
    let path = dir.path().join("earshot.db");
    let mut w = World::new();
    let mut store = Store::open(&path).unwrap();
    let a = Folder::new("Clients", &mut w.stamp());
    let b = Folder::new("Projects", &mut w.stamp());
    store.put_folder(a.clone()).unwrap();
    store.put_folder(b.clone()).unwrap();
    for id in [a.id, b.id] {
        store
            .transaction(|tx| {
                delete_folder(tx, id, &mut w.hlc, &w.clock).map_err(earshot_store::StoreError::Repo)
            })
            .unwrap();
    }
    drop(store);
    let store = Store::open(&path).unwrap();
    let all = store.folders().unwrap();
    assert_eq!(
        all.iter().map(|f| f.id).collect::<Vec<_>>(),
        vec![a.id, b.id]
    );
    assert!(
        all.iter().all(|f| f.meta.deleted && f.name.is_empty()),
        "{all:?}"
    );
}

#[test]
fn store_folder_meeting_may_name_a_folder_that_is_not_stored() {
    // No foreign key: sync may deliver a meeting before its folder.
    let mut w = World::new();
    let mut store = Store::open_in_memory().unwrap();
    let mut m = Meeting::new("", 1, 0, Template::General, &mut w.stamp());
    m.folder_id = Some(EntityId::generate(&w.clock, &mut w.rng));
    store.put_meeting(m.clone()).unwrap();
    assert_eq!(store.meeting(m.id).unwrap(), Some(m));
}

#[test]
fn store_folder_rows_count_for_the_resumed_hlc() {
    let mut store = Store::open_in_memory().unwrap();
    let me = DeviceId::from_bytes([1; 16]);
    let clock = ManualClock::new(1_000);
    let other = DeviceId::from_bytes([2; 16]);
    let high = HlcTimestamp {
        wall_ms: 9_000_000_000_000,
        logical: 3,
        node: other,
    };
    let mut w = World::new();
    let mut f = Folder::new("Synced", &mut w.stamp());
    f.meta = SyncMeta {
        created_at: high,
        updated_at: high,
        deleted: false,
        origin_device_id: other,
    };
    store.put_folder(f).unwrap();
    let t = store.hlc(me).unwrap().now(&clock);
    assert!(t > high, "{t:?} must follow the folder's {high:?}");
}

#[test]
fn store_folder_two_connections_opening_one_v1_database_both_migrate_it_once() {
    let dir = TempDir::new("folder-v1-race");
    let path = build_v1(&dir);
    let start = Arc::new(Barrier::new(2));
    let threads: Vec<_> = (0..2)
        .map(|_| {
            let (path, start) = (path.clone(), Arc::clone(&start));
            std::thread::spawn(move || {
                start.wait();
                let store = Store::open(&path).unwrap();
                (store.schema_version().unwrap(), store.meetings().unwrap())
            })
        })
        .collect();
    let results: Vec<_> = threads.into_iter().map(|t| t.join().unwrap()).collect();
    for (version, meetings) in &results {
        assert_eq!(*version, 2);
        assert_eq!(meetings.len(), 2, "{meetings:?}");
        assert!(meetings.iter().all(|m| m.folder_id.is_none()));
    }
    assert_eq!(results[0].1, results[1].1);
    assert_eq!(user_version(&path), 2);
    assert_eq!(
        columns(&path, "meeting")
            .iter()
            .filter(|c| *c == "folder_id")
            .count(),
        1
    );
}
