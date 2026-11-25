//! T12.92: migration 11, the final-pass run on a segment row (`cargo test -p earshot-store`),
//! and the whole-pass replacement kept out of shipped code. Every name is invented.

mod common;

use std::path::{Path, PathBuf};

use common::TempDir;
use earshot_core::identity::EntityId;
use earshot_core::model::MeetingRepo;
use earshot_store::{SCHEMA_VERSION, Store, SyncedRow};

#[test]
fn store_segment_schema_v10_database_gains_the_pass_run_column() {
    let dir = TempDir::new("pass-run-v10");
    let path = dir.path().join("earshot.db");
    let outbox_before: i64 = {
        let c = rusqlite::Connection::open(&path).unwrap();
        c.execute_batch(include_str!("data/schema_v10.sql"))
            .unwrap();
        let v: u32 = c
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(v, 10, "the fixture is a schema-10 database");
        c.query_row("SELECT count(*) FROM sync_outbox", [], |r| r.get(0))
            .unwrap()
    };
    let mut store = Store::open(&path).unwrap();
    assert_eq!(store.schema_version().unwrap(), SCHEMA_VERSION);
    const { assert!(SCHEMA_VERSION >= 11) };
    assert_eq!(
        store.outbox_len().unwrap(),
        u64::try_from(outbox_before).unwrap(),
        "the migration queues nothing"
    );
    let meeting = EntityId::from_bytes([
        0x01, 0xA3, 0x18, 0x5C, 0x50, 0x00, 0x7D, 0x88, 0xAA, 0xE9, 0xAF, 0x32, 0x1B, 0xC4, 0x47,
        0xEC,
    ]);
    let segments = store.segments(meeting).unwrap();
    assert_eq!(segments.len(), 1, "{segments:?}");
    let mut row = segments[0].clone();
    assert_eq!(row.pass_run, None, "an existing row has no run");
    assert_eq!(row.segment.text, "Invented words.");
    // The column round-trips and the write is queued by the existing triggers.
    row.pass_run = Some(EntityId::from_bytes([
        0x01, 0x90, 0, 0, 0, 0, 0x70, 0, 0x80, 0, 0, 0, 0, 0, 0x77, 1,
    ]));
    row.meta.updated_at.logical += 1;
    store.put_segment(row.clone()).unwrap();
    assert_eq!(store.segments(meeting).unwrap(), vec![row.clone()]);
    assert!(
        store
            .outbox(10_000)
            .unwrap()
            .contains(&SyncedRow::Segment(row))
    );
}

/// Files under `dir` (recursively) whose code calls `name`, leaving out test modules and
/// test folders.
fn shipped_callers(dir: &Path, name: &str, found: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let file = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if path.is_dir() {
            if !matches!(file, "tests" | "target") {
                shipped_callers(&path, name, found);
            }
        } else if path.extension().is_some_and(|e| e == "rs")
            && !file.ends_with("tests.rs")
            && std::fs::read_to_string(&path).is_ok_and(|t| t.contains(name))
        {
            found.push(path);
        }
    }
}

/// T12.92: replacing a whole pass (`replace_segments`) tombstones edited rows, which a
/// re-transcription must keep (docs/sync-spec.md section 7 rule 4), so no shipped code calls
/// it: only the store's transaction around core's function names it.
#[test]
fn store_sync_replace_segments_is_used_by_no_shipped_code() {
    let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let name = concat!("replace_", "segments(");
    let mut found = Vec::new();
    for dir in ["crates", "apps"] {
        let dir = repo.join(dir);
        assert!(dir.is_dir(), "{} is not a folder", dir.display());
        shipped_callers(&dir, name, &mut found);
    }
    let found: Vec<String> = found
        .iter()
        .map(|p| {
            p.strip_prefix(&repo)
                .unwrap_or(p)
                .display()
                .to_string()
                .replace('\\', "/")
        })
        .filter(|p| p.contains("/src/"))
        .collect();
    let mut found = found;
    found.sort();
    // Core's generic definition (`replace_segments<..>(`) does not match; the store's
    // transaction around it calls it.
    assert_eq!(
        found,
        ["crates/earshot-store/src/db.rs"],
        "only the store's wrapper names it"
    );
}
