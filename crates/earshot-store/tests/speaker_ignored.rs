//! T12.93: migration 10, an ignored voice on its synced speaker row (`cargo test -p
//! earshot-store`). Every name is invented.

mod common;

use common::TempDir;
use earshot_core::identity::EntityId;
use earshot_core::model::MeetingRepo;
use earshot_store::{SCHEMA_VERSION, Store, SyncedRow};

#[test]
fn store_speaker_schema_v9_database_gains_the_ignored_column() {
    let dir = TempDir::new("ignored-v9");
    let path = dir.path().join("earshot.db");
    let outbox_before: i64 = {
        let c = rusqlite::Connection::open(&path).unwrap();
        c.execute_batch(include_str!("data/schema_v9.sql")).unwrap();
        let v: u32 = c
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(v, 9, "the fixture is a schema-9 database");
        c.query_row("SELECT count(*) FROM sync_outbox", [], |r| r.get(0))
            .unwrap()
    };
    let mut store = Store::open(&path).unwrap();
    assert_eq!(store.schema_version().unwrap(), SCHEMA_VERSION);
    const { assert!(SCHEMA_VERSION >= 10) };
    assert_eq!(
        store.outbox_len().unwrap(),
        u64::try_from(outbox_before).unwrap(),
        "the migration queues nothing"
    );
    let meeting = EntityId::from_bytes([
        0x01, 0xA3, 0x18, 0x5C, 0x50, 0x00, 0x74, 0x05, 0x95, 0x98, 0xDD, 0x75, 0x06, 0xCE, 0xB8,
        0x29,
    ]);
    let speakers = store.speakers(meeting).unwrap();
    assert_eq!(speakers.len(), 1, "{speakers:?}");
    let mut row = speakers[0].clone();
    assert!(!row.ignored, "an existing row reads not ignored");
    assert_eq!(row.display_name, "Speaker 9");
    // The column round-trips and the write is queued by the existing triggers.
    row.ignored = true;
    row.meta.updated_at.logical += 1;
    store.put_speaker(row.clone()).unwrap();
    assert_eq!(store.speakers(meeting).unwrap(), vec![row.clone()]);
    assert!(
        store
            .outbox(10_000)
            .unwrap()
            .contains(&SyncedRow::Speaker(row))
    );
}
