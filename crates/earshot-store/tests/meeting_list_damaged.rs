//! T14.64: a list of every meeting reads meeting rows leniently. A meeting row that does not
//! read is left out and counted, never failing the list; a tombstone is left out and never
//! counted, whether or not it reads. Every name is invented. `cargo test -p earshot-store meeting_list`.

mod common;

use common::TempDir;
use earshot_core::identity::{DeviceId, EntityId, HlcTimestamp, SyncMeta};
use earshot_core::model::{Meeting, MeetingRepo};
use earshot_core::prompt::Template;
use earshot_store::Store;

const HERE: DeviceId = DeviceId::from_bytes([0x11; 16]);
const T0: u64 = 1_800_000_000_000;

fn id(n: u8) -> EntityId {
    EntityId::from_bytes([0x01, 0x90, 0, 0, 0, 0, 0x70, 0, 0x80, 0, 0, 0, 0, 0, n, 4])
}

fn meeting(n: u8, title: &str, deleted: bool) -> Meeting {
    let at = HlcTimestamp {
        wall_ms: T0,
        logical: 0,
        node: HERE,
    };
    Meeting {
        id: id(n),
        meta: SyncMeta {
            created_at: at,
            updated_at: at,
            deleted,
            origin_device_id: HERE,
        },
        title: title.into(),
        started_unix_ms: T0 + u64::from(n),
        utc_offset_minutes: 600,
        duration_ms: Some(60_000),
        tags: Vec::new(),
        template: Template::General,
        folder_id: None,
        summary_kept_local: false,
    }
}

fn damage(path: &std::path::Path, sql: &str, n: u8) {
    let c = rusqlite::Connection::open(path).unwrap();
    let changed = c
        .execute(sql, rusqlite::params![id(n).as_uuid().as_bytes().to_vec()])
        .unwrap();
    assert_eq!(changed, 1, "{sql}");
}

/// A damaged live folder row is left out and counted, a damaged folder tombstone is left out
/// and not counted. Control: with no damage the reader equals the strict `folders()` filtered
/// to live rows.
#[test]
fn store_meeting_list_leaves_out_and_counts_damaged_folder_rows() {
    use earshot_core::model::{Folder, FolderRepo};
    for damaged in [true, false] {
        let dir = TempDir::new("meeting-list-damaged-folder");
        let path = dir.path().join("earshot.db");
        let folder = |n: u8, name: &str, deleted: bool| Folder {
            id: id(n),
            meta: meeting(n, "", deleted).meta,
            name: name.into(),
        };
        {
            let mut store = Store::open(&path).unwrap();
            store
                .put_folder(folder(1, "Invented archive", false))
                .unwrap();
            store
                .put_folder(folder(2, "Invented Quillmere", false))
                .unwrap();
            store.put_folder(folder(3, "", true)).unwrap();
        }
        let strict: Vec<Folder> = Store::open(&path)
            .unwrap()
            .folders()
            .unwrap()
            .into_iter()
            .filter(|f| !f.meta.deleted)
            .collect();
        if damaged {
            for n in [2, 3] {
                let c = rusqlite::Connection::open(&path).unwrap();
                let changed = c
                    .execute(
                        "UPDATE folder SET name = CAST(X'FF00FE' AS TEXT) WHERE id = ?1",
                        rusqlite::params![id(n).as_uuid().as_bytes().to_vec()],
                    )
                    .unwrap();
                assert_eq!(changed, 1);
            }
        }
        let store = Store::open(&path).unwrap();
        let (listed, count) = store.live_folders_or_damaged().unwrap();
        if damaged {
            assert!(store.folders().is_err(), "the strict reader fails on them");
            assert_eq!(count, 1, "the tombstone is not counted");
            assert_eq!(listed.iter().map(|f| f.id).collect::<Vec<_>>(), vec![id(1)]);
        } else {
            assert_eq!(count, 0);
            assert_eq!(listed, strict);
            assert_eq!(listed.len(), 2);
        }
    }
}

/// Damaged live rows (a title that is not UTF-8, an unknown template word, a 15-byte id) are
/// left out and counted; a damaged tombstone is left out and not counted; the readable live
/// rows come back in `meetings()`' order. Control: with no damage the reader equals the
/// strict `meetings()` filtered to live rows, with nothing counted.
#[test]
fn store_meeting_list_leaves_out_and_counts_damaged_rows() {
    for damaged in [true, false] {
        let dir = TempDir::new("meeting-list-damaged");
        let path = dir.path().join("earshot.db");
        {
            let mut store = Store::open(&path).unwrap();
            for (n, title, deleted) in [
                (1, "Invented planning", false),
                (2, "Invented review", false),
                (3, "Invented kickoff", false),
                (4, "Invented retro", false),
                (5, "Invented standup", false),
                (6, "Invented removed", true),
            ] {
                store.put_meeting(meeting(n, title, deleted)).unwrap();
            }
        }
        let store = Store::open(&path).unwrap();
        let strict: Vec<Meeting> = store
            .meetings()
            .unwrap()
            .into_iter()
            .filter(|m| !m.meta.deleted)
            .collect();
        drop(store);
        if damaged {
            damage(
                &path,
                "UPDATE meeting SET title = CAST(X'FF00FE' AS TEXT) WHERE id = ?1",
                2,
            );
            damage(
                &path,
                "UPDATE meeting SET template = 'invented-word' WHERE id = ?1",
                3,
            );
            damage(
                &path,
                "UPDATE meeting SET id = X'0102030405060708090A0B0C0D0E0F' WHERE id = ?1",
                4,
            );
            damage(
                &path,
                "UPDATE meeting SET title = CAST(X'FF00FE' AS TEXT) WHERE id = ?1",
                6,
            );
        }
        let store = Store::open(&path).unwrap();
        let (listed, count) = store.live_meetings_or_damaged().unwrap();
        if damaged {
            assert!(store.meetings().is_err(), "the strict reader fails on them");
            assert_eq!(
                count, 3,
                "three live rows do not read; the tombstone is not counted"
            );
            assert_eq!(
                listed.iter().map(|m| m.id).collect::<Vec<_>>(),
                vec![id(1), id(5)]
            );
        } else {
            assert_eq!(count, 0);
            assert_eq!(listed, strict);
            assert_eq!(listed.len(), 5);
        }
    }
}
