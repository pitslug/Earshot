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

/// The one rule (T12.93): a flag counts only on an unnamed voice that is not the user, in the
/// final pass's edit test too, so a pulled linked or "Me" row carrying a flag is not an edit
/// by it.
#[test]
fn store_speaker_ignore_counts_only_on_an_unnamed_voice_that_is_not_the_user() {
    use earshot_core::capture::TrackId;
    use earshot_core::diar::{Embedding, EmbeddingModelId, SpeakerRole};
    use earshot_core::identity::{DeviceId, HlcTimestamp, SyncMeta};
    use earshot_core::model::Speaker;
    use earshot_store::identities::{SpeakerIdentity, counts_as_ignored};
    let at = HlcTimestamp {
        wall_ms: 1_800_000_000_000,
        logical: 0,
        node: DeviceId::from_bytes([3; 16]),
    };
    let mut row = Speaker {
        id: EntityId::from_bytes([1; 16]),
        meta: SyncMeta {
            created_at: at,
            updated_at: at,
            deleted: false,
            origin_device_id: at.node,
        },
        meeting_id: EntityId::from_bytes([2; 16]),
        role: SpeakerRole::Remote,
        display_name: "Speaker 2".into(),
        person_id: None,
        ignored: false,
    };
    let entry = |row: &Speaker, ignored: bool| SpeakerIdentity {
        speaker: row.id,
        track: TrackId::Reference,
        centroid: Embedding::new(EmbeddingModelId::new("m@1"), vec![0.6, 0.8]),
        given_name: row.display_name.clone(),
        given_person: row.person_id,
        given_stamp: Some(row.meta.updated_at),
        user_edited: false,
        enrolled: false,
        ignored,
        linked_before: Vec::new(),
    };
    assert!(!entry(&row, false).edited(&row, true));
    assert!(
        entry(&row, true).edited(&row, true),
        "a legacy flag before the copy"
    );
    assert!(
        !entry(&row, true).edited(&row, false),
        "never read after the copy"
    );
    row.ignored = true;
    assert!(counts_as_ignored(&row, false));
    assert!(entry(&row, false).edited(&row, false));
    // Linked, as the pass gave it: the flag is not an ignore and not an edit.
    row.person_id = Some(EntityId::from_bytes([4; 16]));
    assert!(!counts_as_ignored(&row, true));
    assert!(!entry(&row, true).edited(&row, true));
    row.person_id = None;
    row.role = SpeakerRole::Me;
    assert!(!counts_as_ignored(&row, true));
    assert!(!entry(&row, true).edited(&row, true));
}
