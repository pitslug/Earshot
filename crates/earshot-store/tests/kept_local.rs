//! T11.1a: migration 5, a meeting kept on this computer as a synced meeting field
//! (`cargo test -p earshot-store kept_local`; docs/sync-spec.md section 5.1).

mod common;

use std::path::{Path, PathBuf};

use common::TempDir;
use earshot_core::model::MeetingRepo;
use earshot_store::{SCHEMA_VERSION, Store};

/// A schema-4 database as SQL text: the schema-3 fixture with migration 4 applied and two
/// device-local records added (the first meeting kept on this computer, an unrelated record for
/// the second), dumped with Python's sqlite3 `iterdump`. The lib test
/// `schema::tests::store_kept_local_v4_fixture_has_exactly_the_schema_of_migrations_1_to_4`
/// keeps its DDL identical to migrations 1 to 4.
const SCHEMA_V4_SQL: &str = include_str!("data/schema_v4.sql");

/// The record key of the fixture's first meeting.
const KEPT_KEY: &str = "summary-kept-local/01a3185c-5000-7d88-aae9-af321bc447ec";

fn build_v4(dir: &TempDir) -> PathBuf {
    let to = dir.path().join("earshot.db");
    let c = rusqlite::Connection::open(&to).unwrap();
    c.execute_batch(SCHEMA_V4_SQL).unwrap();
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
fn store_kept_local_schema_v4_database_moves_the_record_into_the_meeting_row() {
    let dir = TempDir::new("kept-local-v4");
    let path = build_v4(&dir);
    assert_eq!(user_version(&path), 4, "the fixture is a schema-4 database");

    let store = Store::open(&path).unwrap();
    assert_eq!(store.schema_version().unwrap(), SCHEMA_VERSION);
    const { assert!(SCHEMA_VERSION >= 5) };
    let meetings = store.meetings().unwrap();
    assert_eq!(meetings.len(), 2, "{meetings:?}");
    let kept = meetings
        .iter()
        .find(|m| m.id.to_string() == "01a3185c-5000-7d88-aae9-af321bc447ec")
        .unwrap();
    let other = meetings.iter().find(|m| m.id != kept.id).unwrap();
    assert!(kept.summary_kept_local, "the record became the field");
    // A different record of another meeting says nothing about keeping it.
    assert!(!other.summary_kept_local);
    // The backfill is not an edit: no row was re-stamped, so nothing reads as changed here.
    assert_eq!(kept.meta.updated_at, kept.meta.created_at);
    assert_eq!(other.meta.updated_at, other.meta.created_at);
    // The old record stays: until every reader is past it, either one keeps the meeting.
    assert!(store.local_value(KEPT_KEY).unwrap().is_some());
    drop(store);
    assert_eq!(user_version(&path), SCHEMA_VERSION);
    // Opening an up-to-date database again changes nothing.
    let store = Store::open(&path).unwrap();
    assert!(
        store
            .meetings()
            .unwrap()
            .iter()
            .any(|m| m.summary_kept_local)
    );
}

#[test]
fn store_kept_local_field_round_trips_through_the_meeting_row() {
    let dir = TempDir::new("kept-local-row");
    let path = build_v4(&dir);
    let mut store = Store::open(&path).unwrap();
    let mut meetings = store.meetings().unwrap();
    let mut m = meetings.pop().unwrap();
    for want in [true, false, true] {
        m.summary_kept_local = want;
        store.put_meeting(m.clone()).unwrap();
        assert_eq!(
            store.meeting(m.id).unwrap().unwrap().summary_kept_local,
            want
        );
    }
}
