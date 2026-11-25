//! T14.26 review fixes: rows here that do not read. A queued row whose text is not UTF-8 is
//! counted as unsendable by the sync outbox, as the store's own readers count it, and never
//! fails the page (and so every sync tick); and a local tombstone (or withdrawal) that does
//! not read stays final against a pulled live copy, as a readable one does.
//! `cargo test -p earshot-store sync`.

mod common;

use common::TempDir;
use earshot_core::diar::EmbeddingModelId;
use earshot_core::identity::{DeviceId, EntityId, HlcTimestamp, ManualClock, SyncMeta};
use earshot_core::model::{Meeting, MeetingRepo, RequestState, SummaryRequest, SummaryRequestRepo};
use earshot_core::prompt::Template;
use earshot_core::voiceprint::{
    Person, SampleRecord, Voiceprint, VoiceprintRepo, VoiceprintSample,
};
use earshot_store::{ApplyRules, Incoming, Store, SyncKinds, SyncedRow};

const HERE: DeviceId = DeviceId::from_bytes([0x11; 16]);
const THERE: DeviceId = DeviceId::from_bytes([0x22; 16]);
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

fn stamped(wall_ms: u64, node: DeviceId, deleted: bool) -> SyncMeta {
    SyncMeta {
        created_at: at(T0),
        updated_at: HlcTimestamp {
            wall_ms,
            logical: 0,
            node,
        },
        deleted,
        origin_device_id: node,
    }
}

fn model() -> EmbeddingModelId {
    EmbeddingModelId::new("invented-model")
}

fn print(n: u8, meta: SyncMeta) -> Voiceprint {
    let (sum, count, speech) = if meta.deleted {
        (vec![0.0; 4], 0, 0)
    } else {
        (vec![0.5; 4], 1, 5_000)
    };
    Voiceprint::from_stored(id(n), meta, id(1), model(), sum, count, speech)
}

fn request(state: RequestState, meta: SyncMeta) -> SummaryRequest {
    SummaryRequest {
        id: id(6),
        meta,
        meeting_id: id(2),
        target_device: THERE,
        asking_device: HERE,
        segment_count: 40,
        state,
        reason: String::new(),
    }
}

fn incoming(seq: u64, row: SyncedRow) -> Incoming {
    let stamp = row.meta().updated_at;
    let mut key = [0u8; 16];
    key.copy_from_slice(&row.id().as_uuid().as_bytes()[..]);
    key[0] ^= 0xFF;
    Incoming {
        key,
        seq,
        stamp,
        body: vec![0xA5; 40],
        entity: Some(row.table().into()),
        row: Some(row),
    }
}

fn free(_: EntityId) -> bool {
    false
}

/// For every table whose tombstone (or this device's withdrawal) the readable path keeps final
/// against a pulled live copy, a local row of that state that does not read is kept too: the
/// pulled copy is ignored, so a forgotten voice never comes back.
#[test]
fn store_sync_a_final_local_row_that_does_not_read_is_not_replaced_by_a_live_copy() {
    // Every table is judged before any assertion, so a failure names each table that fails.
    let mut failures = Vec::new();
    for table in ["voiceprint", "voiceprint_sample", "summary_request"] {
        let dir = TempDir::new(&format!("sync-damaged-final-{table}"));
        let path = dir.path().join("earshot.db");
        let here_gone = stamped(T0 + 2, HERE, true);
        let pulled = {
            let mut store = Store::open(&path).unwrap();
            store
                .put_person(Person {
                    id: id(1),
                    meta: stamped(T0 + 1, HERE, false),
                    display_name: "Invented Person".into(),
                    is_me: false,
                    merged_into: None,
                })
                .unwrap();
            store.put_meeting(meeting(2, "Invented", T0 + 1)).unwrap();
            match table {
                "voiceprint" => {
                    store.put_voiceprint(print(3, here_gone)).unwrap();
                    SyncedRow::Voiceprint(print(3, stamped(T0 + 5, THERE, false)))
                }
                "voiceprint_sample" => {
                    store
                        .put_voiceprint(print(5, stamped(T0 + 1, HERE, false)))
                        .unwrap();
                    store
                        .put_sample(VoiceprintSample::erased(id(4), here_gone).unwrap())
                        .unwrap();
                    SyncedRow::VoiceprintSample(
                        VoiceprintSample::from_stored(SampleRecord {
                            id: id(4),
                            meta: stamped(T0 + 5, THERE, false),
                            voiceprint_id: Some(id(5)),
                            person_id: Some(id(1)),
                            model: Some(model()),
                            vector: vec![1.0, 0.0, 0.0, 0.0],
                            speech_ms: Some(6_000),
                            meeting_id: None,
                            speaker_id: None,
                        })
                        .unwrap(),
                    )
                }
                _ => {
                    let mut withdrawn = here_gone;
                    withdrawn.deleted = false;
                    store
                        .put_summary_request(request(RequestState::Withdrawn, withdrawn))
                        .unwrap();
                    SyncedRow::SummaryRequest(request(
                        RequestState::Claimed,
                        stamped(T0 + 5, THERE, false),
                    ))
                }
            }
        };
        // The local row stops reading (a creation time no clock gives).
        {
            let c = rusqlite::Connection::open(&path).unwrap();
            let changed = c
                .execute(
                    &format!("UPDATE {table} SET created_wall = -1 WHERE id = ?1"),
                    rusqlite::params![pulled.id().as_uuid().as_bytes().to_vec()],
                )
                .unwrap();
            assert_eq!(changed, 1, "{table}");
        }
        let mut store = Store::open(&path).unwrap();
        let mut hlc = store.hlc(HERE).unwrap();
        let rules = ApplyRules {
            kinds: SyncKinds::ALL,
            leased: &free,
            now_ms: T0 + 10_000,
            after: None,
        };
        let a = store
            .apply_remote(
                &[incoming(1, pulled.clone())],
                &rules,
                &mut hlc,
                &ManualClock::new(T0 + 10_000),
            )
            .unwrap();
        drop(store);
        // Still the damaged local row, never the pulled copy.
        let c = rusqlite::Connection::open(&path).unwrap();
        let wall: i64 = c
            .query_row(
                &format!("SELECT created_wall FROM {table} WHERE id = ?1"),
                rusqlite::params![pulled.id().as_uuid().as_bytes().to_vec()],
                |r| r.get(0),
            )
            .unwrap();
        if (a.written, a.ignored, wall) != (0, 1, -1) {
            failures.push(format!(
                "{table}: written {}, ignored {}, created_wall {wall}",
                a.written, a.ignored
            ));
        }
    }
    assert!(failures.is_empty(), "{failures:#?}");
}
