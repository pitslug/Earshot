//! T14.49d (docs/sync-spec.md 7A.2 C1): the outbox read restricted to the rows a tick stamped
//! again. The restriction goes through a temporary table in the read's own transaction, so a set
//! larger than SQLite's limit on bound parameters is read whole, oldest stamp first, after every
//! filter a push has. Every name is invented. `cargo test -p earshot-store sync`.

mod common;

use std::collections::BTreeSet;

use common::TempDir;
use earshot_core::diar::{EmbeddingModelId, SpeakerRole};
use earshot_core::identity::{DeviceId, EntityId, HlcTimestamp, SyncMeta};
use earshot_core::model::{Folder, FolderRepo, Meeting, MeetingRepo, Speaker};
use earshot_core::prompt::Template;
use earshot_core::voiceprint::{Person, Voiceprint, VoiceprintRepo};
use earshot_store::{Store, StoreError, SyncKinds, SyncedRow};

const HERE: DeviceId = DeviceId::from_bytes([0x11; 16]);
const T0: u64 = 1_800_000_000_000;

fn meta(wall_ms: u64) -> SyncMeta {
    let at = HlcTimestamp {
        wall_ms,
        logical: 0,
        node: HERE,
    };
    SyncMeta {
        created_at: at,
        updated_at: at,
        deleted: false,
        origin_device_id: HERE,
    }
}

/// An id whose order is the reverse of `n`'s, so id order never passes for stamp order.
fn id_of(n: u32) -> EntityId {
    let mut b = [0u8; 16];
    b[..4].copy_from_slice(&(u32::MAX - n).to_be_bytes());
    b[6] = 0x70;
    b[8] = 0x80;
    EntityId::from_bytes(b)
}

/// 40,000 queued folders (more than SQLite's bound parameters), the folder `n` stamped `T0 + n`:
/// the extra push's id set holds them all and the page is the 200 oldest by stamp, in that
/// order. A second read with a smaller set gets only it (the table never outlives its read), and
/// an id that is not queued is simply not found.
#[test]
fn store_sync_a_large_id_set_is_read_through_the_temporary_table_in_stamp_order() {
    const N: u32 = 40_000;
    let dir = TempDir::new("sync-outbox-only-large");
    let mut store = Store::open(&dir.path().join("earshot.db")).unwrap();
    store
        .transaction(|tx| -> Result<(), StoreError> {
            for n in (0..N).rev() {
                tx.put_folder(Folder {
                    id: id_of(n),
                    meta: meta(T0 + u64::from(n)),
                    name: format!("Box {n}"),
                })?;
            }
            Ok(())
        })
        .unwrap();
    let all: BTreeSet<EntityId> = (0..N).map(id_of).collect();
    let page = store
        .outbox_page_only(200, &[], SyncKinds::ALL, Some(&all))
        .unwrap();
    let got: Vec<EntityId> = page.rows.iter().map(SyncedRow::id).collect();
    let want: Vec<EntityId> = (0..200).map(id_of).collect();
    assert_eq!(got, want, "the 200 oldest, oldest first");
    let few: BTreeSet<EntityId> = [id_of(30_000), id_of(7), id_of(12_345), id_of(N + 5)]
        .into_iter()
        .collect();
    let page = store
        .outbox_page_only(200, &[], SyncKinds::ALL, Some(&few))
        .unwrap();
    let got: Vec<EntityId> = page.rows.iter().map(SyncedRow::id).collect();
    assert_eq!(got, vec![id_of(7), id_of(12_345), id_of(30_000)]);
    // Unrestricted, the read is today's.
    assert_eq!(
        store
            .outbox_page_only(5, &[], SyncKinds::ALL, None)
            .unwrap(),
        store.outbox_page(5, &[], SyncKinds::ALL).unwrap()
    );
}

/// The restriction keeps every filter: a voiceprint in the set is not read while voiceprint sync
/// is off, and a recording meeting's rows in the set are not read; the rest of the set is.
#[test]
fn store_sync_an_id_set_keeps_the_kind_and_recording_filters() {
    let dir = TempDir::new("sync-outbox-only-filters");
    let mut store = Store::open(&dir.path().join("earshot.db")).unwrap();
    let person = Person {
        id: id_of(1),
        meta: meta(T0 + 1),
        display_name: String::new(),
        is_me: false,
        merged_into: None,
    };
    store.put_person(person).unwrap();
    store
        .put_voiceprint(Voiceprint::from_stored(
            id_of(2),
            meta(T0 + 2),
            id_of(1),
            EmbeddingModelId::new("invented-model"),
            vec![0.6, 0.8],
            1,
            5_000,
        ))
        .unwrap();
    let recording = Meeting {
        id: id_of(3),
        meta: meta(T0 + 3),
        title: "Invented recording".into(),
        started_unix_ms: T0,
        utc_offset_minutes: 600,
        duration_ms: None,
        tags: Vec::new(),
        template: Template::General,
        folder_id: None,
        summary_kept_local: false,
    };
    store.put_meeting(recording).unwrap();
    store
        .put_speaker(Speaker {
            id: id_of(4),
            meta: meta(T0 + 4),
            meeting_id: id_of(3),
            role: SpeakerRole::Remote,
            display_name: "Speaker 2".into(),
            person_id: None,
            ignored: false,
        })
        .unwrap();
    let set: BTreeSet<EntityId> = (1..=4).map(id_of).collect();
    let kinds = SyncKinds {
        voiceprints: false,
        ..SyncKinds::ALL
    };
    let page = store
        .outbox_page_only(200, &[id_of(3)], kinds, Some(&set))
        .unwrap();
    let got: Vec<EntityId> = page.rows.iter().map(SyncedRow::id).collect();
    assert_eq!(got, vec![id_of(1)], "only the person");
    // The same set with nothing filtered reads all four.
    let page = store
        .outbox_page_only(200, &[], SyncKinds::ALL, Some(&set))
        .unwrap();
    assert_eq!(page.rows.len(), 4);
}
