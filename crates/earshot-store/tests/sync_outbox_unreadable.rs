//! T14.26 review fix: a queued row whose text is not UTF-8 is counted as unsendable by the
//! sync outbox, as the store's own readers count it, and never fails the page (and so every
//! sync tick). `cargo test -p earshot-store sync`.

mod common;

use common::TempDir;
use earshot_core::identity::{DeviceId, EntityId, HlcTimestamp, SyncMeta};
use earshot_core::model::{Meeting, MeetingRepo};
use earshot_core::prompt::Template;
use earshot_store::{Store, SyncKinds, SyncedRow};

const HERE: DeviceId = DeviceId::from_bytes([0x11; 16]);
const T0: u64 = 1_800_000_000_000;

fn id(n: u8) -> EntityId {
    EntityId::from_bytes([0x01, 0x90, 0, 0, 0, 0, 0x70, 0, 0x80, 0, 0, 0, 0, 0, n, 1])
}

fn at(wall_ms: u64) -> HlcTimestamp {
    HlcTimestamp {
        wall_ms,
        logical: 0,
        node: HERE,
    }
}

fn meeting(n: u8, title: &str, updated: u64) -> Meeting {
    Meeting {
        id: id(n),
        meta: SyncMeta {
            created_at: at(T0),
            updated_at: at(updated),
            deleted: false,
            origin_device_id: HERE,
        },
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

#[test]
fn store_sync_outbox_counts_a_row_whose_text_is_not_utf8_as_unsendable() {
    let dir = TempDir::new("sync-outbox-not-utf8");
    let path = dir.path().join("earshot.db");
    let good = meeting(1, "Invented planning", T0 + 1);
    let bad = meeting(2, "Invented review", T0 + 2);
    {
        let mut store = Store::open(&path).unwrap();
        store.put_meeting(good.clone()).unwrap();
        store.put_meeting(bad.clone()).unwrap();
    }
    {
        let c = rusqlite::Connection::open(&path).unwrap();
        c.execute(
            "UPDATE meeting SET title = CAST(X'FF00FE' AS TEXT) WHERE id = ?1",
            rusqlite::params![bad.id.as_uuid().as_bytes().to_vec()],
        )
        .unwrap();
    }
    let store = Store::open(&path).unwrap();
    let page = store.outbox_page(100, &[], SyncKinds::ALL).unwrap();
    assert_eq!(page.unreadable, 1, "the row that is not UTF-8 is counted");
    assert_eq!(
        page.rows,
        vec![SyncedRow::Meeting(good)],
        "the rest still read"
    );
    assert_eq!(
        store.outbox_except(100, &[], SyncKinds::ALL).unwrap(),
        page.rows
    );
    // Still queued: it waits until it is repaired or erased.
    assert_eq!(store.outbox_len_except(&[], SyncKinds::ALL).unwrap(), 2);
}
