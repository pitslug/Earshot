//! T12.30: migration 4, the index that answers whether a meeting has a live segment of a pass
//! (`cargo test -p earshot-store segment_live`).

mod common;

use std::path::{Path, PathBuf};

use common::TempDir;
use earshot_core::model::{MeetingRepo, TranscriptPass};
use earshot_store::{SCHEMA_VERSION, Store};

/// A schema-3 database as SQL text: the schema-2 fixture with migration 3 applied, dumped with
/// Python's sqlite3 `iterdump`, then `PRAGMA user_version = 3`. The lib test
/// `schema::tests::store_segment_live_v3_fixture_has_exactly_the_schema_of_migrations_1_to_3`
/// keeps its DDL identical to migrations 1 to 3.
const SCHEMA_V3_SQL: &str = include_str!("data/schema_v3.sql");

fn build_v3(dir: &TempDir) -> PathBuf {
    let to = dir.path().join("earshot.db");
    let c = rusqlite::Connection::open(&to).unwrap();
    c.execute_batch(SCHEMA_V3_SQL).unwrap();
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

fn has_index(path: &Path, name: &str) -> bool {
    let c = rusqlite::Connection::open(path).unwrap();
    c.query_row(
        "SELECT count(*) FROM sqlite_master WHERE type = 'index' AND name = ?1",
        [name],
        |r| r.get::<_, i64>(0),
    )
    .unwrap()
        == 1
}

#[test]
fn store_segment_live_schema_v3_database_migrates_to_the_index() {
    let dir = TempDir::new("segment-live-v3");
    let path = build_v3(&dir);
    assert_eq!(user_version(&path), 3, "the fixture is a schema-3 database");
    assert!(!has_index(&path, "segment_live"));

    let store = Store::open(&path).unwrap();
    assert_eq!(store.schema_version().unwrap(), SCHEMA_VERSION);
    const { assert!(SCHEMA_VERSION >= 4) };
    let meetings = store.meetings().unwrap();
    assert_eq!(meetings.len(), 2, "{meetings:?}");
    let first = meetings
        .iter()
        .find(|m| m.title == "Invented planning chat")
        .unwrap();
    // The rows are untouched, and the existence answer agrees with them.
    let segments = store.segments(first.id).unwrap();
    assert_eq!(segments.len(), 1);
    assert_eq!(segments[0].pass, TranscriptPass::Final);
    assert!(
        store
            .has_live_segment(first.id, TranscriptPass::Final)
            .unwrap()
    );
    assert!(
        !store
            .has_live_segment(first.id, TranscriptPass::Live)
            .unwrap()
    );
    drop(store);
    assert_eq!(user_version(&path), SCHEMA_VERSION);
    assert!(has_index(&path, "segment_live"));
    // Opening an up-to-date database again changes nothing.
    let store = Store::open(&path).unwrap();
    assert_eq!(store.schema_version().unwrap(), SCHEMA_VERSION);
}
