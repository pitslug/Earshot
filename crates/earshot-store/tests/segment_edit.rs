//! The user's edit mark on a transcript segment (T12.16): migration 3 and the round trip of
//! `SegmentRow::text_edited_at` (`cargo test -p earshot-store segment_edit`).

mod common;

use std::path::{Path, PathBuf};

use common::TempDir;
use earshot_core::identity::{DeviceId, HlcTimestamp};
use earshot_core::model::MeetingRepo;
use earshot_store::{SCHEMA_VERSION, Store};

/// A schema-2 database as SQL text: the schema-1 fixture with migration 2 applied and an
/// invented folder holding its first meeting, dumped with Python's sqlite3 `iterdump`, then
/// `PRAGMA user_version = 2`. The lib test
/// `schema::tests::store_segment_edit_v2_fixture_has_exactly_the_schema_of_migrations_1_and_2`
/// keeps its DDL identical to migrations 1 and 2.
const SCHEMA_V2_SQL: &str = include_str!("data/schema_v2.sql");

fn build_v2(dir: &TempDir) -> PathBuf {
    let to = dir.path().join("earshot.db");
    let c = rusqlite::Connection::open(&to).unwrap();
    c.execute_batch(SCHEMA_V2_SQL).unwrap();
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

#[test]
fn store_segment_edit_schema_v2_database_migrates_to_unedited_segments() {
    let dir = TempDir::new("segment-edit-v2");
    let path = build_v2(&dir);
    assert_eq!(user_version(&path), 2, "the fixture is a schema-2 database");

    let mut store = Store::open(&path).unwrap();
    assert_eq!(store.schema_version().unwrap(), SCHEMA_VERSION);
    const { assert!(SCHEMA_VERSION >= 3) };
    let meetings = store.meetings().unwrap();
    assert_eq!(meetings.len(), 2, "{meetings:?}");
    let first = meetings
        .iter()
        .find(|m| m.title == "Invented planning chat")
        .unwrap();
    assert!(first.folder_id.is_some(), "the folder survives");
    let segments = store.segments(first.id).unwrap();
    assert_eq!(segments.len(), 1);
    assert_eq!(
        segments[0].text_edited_at, None,
        "existing segments read as the recogniser's"
    );
    assert_eq!(segments[0].segment.text, "Invented words.");
    assert_eq!(segments[0].segment.words.len(), 2);

    // An edit mark is stored and read back, also after reopening.
    let mut edited = segments[0].clone();
    let at = HlcTimestamp {
        wall_ms: 1_800_000_000_123,
        logical: 4,
        node: DeviceId::from_bytes([9; 16]),
    };
    edited.segment.text = "Invented and corrected words.".into();
    edited.segment.words.clear();
    edited.meta.updated_at = at;
    edited.text_edited_at = Some(at);
    store.put_segment(edited.clone()).unwrap();
    drop(store);
    assert_eq!(user_version(&path), SCHEMA_VERSION);
    let store = Store::open(&path).unwrap();
    assert_eq!(store.segments(first.id).unwrap(), vec![edited]);
}
