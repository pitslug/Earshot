//! T12.77b2: migration 9, voice samples in the store (`cargo test -p earshot-store`;
//! docs/voice-samples.md 2, 4 and 9). Every name is invented.

mod common;

use std::cell::Cell;

use common::TempDir;
use earshot_core::diar::{Embedding, EmbeddingModelId};
use earshot_core::identity::{DeviceId, EntityId, HlcTimestamp, SyncMeta};
use earshot_core::model::repo_contract;
use earshot_core::voiceprint::{
    Person, SampleRecord, Voiceprint, VoiceprintRepo, VoiceprintSample, effective, live_prints,
    live_prints_of, match_embedding,
};
use earshot_store::{ReadOnlyStore, SCHEMA_VERSION, Store, SyncKinds, SyncedRow};

const HERE: DeviceId = DeviceId::from_bytes([0x11; 16]);
const T0: u64 = 1_800_000_000_000;

fn id(n: u8) -> EntityId {
    EntityId::from_bytes([0x01, 0x90, 0, 0, 0, 0, 0x70, 0, 0x80, 0, 0, 0, 0, 0, n, 9])
}

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

fn model() -> EmbeddingModelId {
    EmbeddingModelId::new("invented-embedder@0")
}

fn person(n: u8) -> Person {
    Person {
        id: id(n),
        meta: meta(T0),
        display_name: "An invented colleague".into(),
        is_me: false,
        merged_into: None,
    }
}

/// A zero-base print, as 0.4.0 creates them.
fn empty_print(n: u8, person: EntityId) -> Voiceprint {
    Voiceprint::from_stored(id(n), meta(T0 + 1), person, model(), vec![0.0; 3], 0, 0)
}

fn sample(n: u8, wall_ms: u64, print: &Voiceprint, vector: Vec<f32>) -> VoiceprintSample {
    VoiceprintSample::from_stored(SampleRecord {
        id: id(n),
        meta: meta(wall_ms),
        voiceprint_id: Some(print.id()),
        person_id: Some(print.person_id()),
        model: Some(print.model().clone()),
        vector,
        speech_ms: Some(6_000),
        meeting_id: Some(id(200)),
        speaker_id: Some(id(201)),
    })
    .unwrap()
}

#[test]
fn store_voiceprint_sample_in_memory_passes_the_sample_contract() {
    repo_contract::voiceprint_sample_repo(|| Store::open_in_memory().unwrap());
}

#[test]
fn store_voiceprint_sample_on_disk_passes_the_sample_contract() {
    let dir = TempDir::new("sample-contract");
    let n = Cell::new(0);
    repo_contract::voiceprint_sample_repo(|| {
        n.set(n.get() + 1);
        Store::open(&dir.path().join(format!("c{}.db", n.get()))).unwrap()
    });
}

#[test]
fn store_voiceprint_sample_vector_survives_a_reopen_bit_for_bit() {
    let dir = TempDir::new("sample-bits");
    let path = dir.path().join("earshot.db");
    let p = empty_print(2, id(1));
    let x = 0.1f32.next_up();
    let vector = vec![x, -(1.0f32 / 3.0), (1.0 - x * x - 1.0 / 9.0).sqrt()];
    let s = sample(3, T0 + 2, &p, vector.clone());
    {
        let mut store = Store::open(&path).unwrap();
        store.put_sample(s.clone()).unwrap();
    }
    let store = Store::open(&path).unwrap();
    let back = store.sample(s.id()).unwrap().unwrap();
    let bits = |v: &[f32]| v.iter().map(|x| x.to_bits()).collect::<Vec<_>>();
    assert_eq!(bits(back.vector().unwrap()), bits(&vector));
    assert_eq!(back, s);
}

#[test]
fn store_voiceprint_sample_a_damaged_live_row_is_an_error_not_skipped() {
    let dir = TempDir::new("sample-damaged");
    let path = dir.path().join("earshot.db");
    let p = empty_print(2, id(1));
    let s = sample(3, T0 + 2, &p, vec![1.0, 0.0, 0.0]);
    {
        let mut store = Store::open(&path).unwrap();
        store.put_sample(s.clone()).unwrap();
    }
    {
        // A vector no longer of unit length (as a damaged file or a foreign writer leaves it).
        let c = rusqlite::Connection::open(&path).unwrap();
        let two: Vec<u8> = [2.0f32, 0.0, 0.0]
            .iter()
            .flat_map(|x| x.to_le_bytes())
            .collect();
        c.execute("UPDATE voiceprint_sample SET vector = ?1", [two])
            .unwrap();
    }
    let store = Store::open(&path).unwrap();
    assert!(store.sample(s.id()).is_err());
    assert!(store.samples_of(p.id()).is_err());
}

#[test]
fn store_voiceprint_sample_folds_into_live_prints_and_matching() {
    let mut store = Store::open_in_memory().unwrap();
    let ada = person(1);
    store.put_person(ada.clone()).unwrap();
    let p = empty_print(2, ada.id);
    store.put_voiceprint(p.clone()).unwrap();
    let probe = Embedding::new(model(), vec![0.0, 1.0, 0.0]);
    assert!(matches!(
        match_embedding(&store, &probe, 0.5).unwrap(),
        earshot_core::voiceprint::MatchOutcome::Unknown { best: None }
    ));
    let s = sample(3, T0 + 2, &p, vec![0.0, 1.0, 0.0]);
    store.put_sample(s.clone()).unwrap();
    let (_, eff) = live_prints(&store, &model()).unwrap().pop().unwrap();
    assert_eq!((eff.count(), eff.speech_ms()), (1, 6_000));
    assert_eq!(eff, effective(&p, &[s]));
    assert_eq!(live_prints_of(&store, ada.id).unwrap()[0].1, eff);
    assert!(matches!(
        match_embedding(&store, &probe, 0.5).unwrap(),
        earshot_core::voiceprint::MatchOutcome::Known { person, .. } if person == ada.id
    ));
}

#[test]
fn store_voiceprint_sample_read_only_reads_and_refuses_writes() {
    let dir = TempDir::new("sample-read-only");
    let path = dir.path().join("earshot.db");
    let p = empty_print(2, id(1));
    let s = sample(3, T0 + 2, &p, vec![1.0, 0.0, 0.0]);
    {
        let mut store = Store::open(&path).unwrap();
        store.put_sample(s.clone()).unwrap();
    }
    let mut ro = ReadOnlyStore::open(&path).unwrap();
    assert_eq!(ro.samples(&model()).unwrap(), vec![s.clone()]);
    assert_eq!(ro.samples_of(p.id()).unwrap(), vec![s.clone()]);
    assert_eq!(
        ro.samples_of_person(p.person_id()).unwrap(),
        vec![s.clone()]
    );
    assert_eq!(ro.samples_from(id(200), id(201)).unwrap(), vec![s.clone()]);
    assert_eq!(ro.sample(s.id()).unwrap(), Some(s.clone()));
    assert!(ro.put_sample(s).is_err());
}

#[test]
fn store_voiceprint_sample_rows_are_opt_in_with_voiceprints_and_decode_checked() {
    let p = empty_print(2, id(1));
    let s = sample(3, T0 + 2, &p, vec![1.0, 0.0, 0.0]);
    let row = SyncedRow::VoiceprintSample(s.clone());
    assert_eq!(row.table(), "voiceprint_sample");
    assert_eq!(row.id(), s.id());
    assert_eq!(row.meeting(), None, "never held for its meeting");
    assert!(!row.waits_while_busy());
    let off = SyncKinds {
        voiceprints: false,
        dictionary: true,
    };
    assert!(!off.syncs("voiceprint_sample"));
    assert!(SyncKinds::ALL.syncs("voiceprint_sample"));
    // Sample sync is on (T12.77b8): a pulled sample decodes through its checks, and one that
    // fails them is not a row this build reads (the caller holds it).
    let record = serde_json::to_value(&s).unwrap();
    assert_eq!(
        SyncedRow::decode("voiceprint_sample", record.clone()).unwrap(),
        Some(row)
    );
    let mut bad = record;
    bad["vector"] = serde_json::json!([3.0, 0.0, 0.0]);
    assert!(
        SyncedRow::decode("voiceprint_sample", bad)
            .unwrap_or(None)
            .is_none()
    );
}

#[test]
fn store_voiceprint_sample_outbox_and_its_count_agree_with_samples_present() {
    let mut store = Store::open_in_memory().unwrap();
    let ada = person(1);
    store.put_person(ada.clone()).unwrap();
    let p = empty_print(2, ada.id);
    store.put_voiceprint(p.clone()).unwrap();
    let s = sample(3, T0 + 2, &p, vec![1.0, 0.0, 0.0]);
    store.put_sample(s.clone()).unwrap();
    let mut erased = meta(T0 + 4);
    erased.created_at = meta(T0 + 3).created_at;
    erased.deleted = true;
    store
        .put_sample(VoiceprintSample::erased(id(4), erased).unwrap())
        .unwrap();
    let off = SyncKinds {
        voiceprints: false,
        dictionary: true,
    };
    for kinds in [SyncKinds::ALL, off] {
        let rows = store.outbox_except(10_000, &[], kinds).unwrap();
        let n = store.outbox_len_except(&[], kinds).unwrap();
        assert_eq!(u64::try_from(rows.len()).unwrap(), n, "{kinds:?}");
        let samples = rows
            .iter()
            .filter(|r| r.table() == "voiceprint_sample")
            .count();
        assert_eq!(samples, if kinds.voiceprints { 2 } else { 0 }, "{kinds:?}");
    }
    assert!(
        store
            .outbox(10_000)
            .unwrap()
            .contains(&SyncedRow::VoiceprintSample(s))
    );
}

#[test]
fn store_voiceprint_sample_schema_v8_database_gains_the_sample_table() {
    let dir = TempDir::new("samples-v8");
    let path = dir.path().join("earshot.db");
    let outbox_before: i64 = {
        let c = rusqlite::Connection::open(&path).unwrap();
        c.execute_batch(include_str!("data/schema_v8.sql")).unwrap();
        let v: u32 = c
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(v, 8, "the fixture is a schema-8 database");
        c.query_row("SELECT count(*) FROM sync_outbox", [], |r| r.get(0))
            .unwrap()
    };
    let mut store = Store::open(&path).unwrap();
    assert_eq!(store.schema_version().unwrap(), SCHEMA_VERSION);
    const { assert!(SCHEMA_VERSION >= 9) };
    assert_eq!(
        store.outbox_len().unwrap(),
        u64::try_from(outbox_before).unwrap(),
        "the migration queues nothing"
    );
    // The fixture's print (an enrolment from before samples) reads as its stored base.
    let prints = store
        .voiceprints(&EmbeddingModelId::new("invented-model"))
        .unwrap();
    assert_eq!(prints.len(), 1);
    let old = &prints[0];
    assert!(store.samples_of(old.id()).unwrap().is_empty());
    let (_, eff) = live_prints(&store, old.model()).unwrap().pop().unwrap();
    assert_eq!(eff, effective(old, &[]));
    assert_eq!(
        (eff.count(), eff.speech_ms(), eff.has_base()),
        (1, 5_000, true)
    );
    assert_eq!(eff.sum(), &[0.5; 4]);
    // A sample written now is queued by the new triggers.
    let s = VoiceprintSample::from_stored(SampleRecord {
        id: id(30),
        meta: meta(T0 + 9),
        voiceprint_id: Some(old.id()),
        person_id: Some(old.person_id()),
        model: Some(old.model().clone()),
        vector: vec![0.5; 4],
        speech_ms: Some(1_000),
        meeting_id: None,
        speaker_id: None,
    })
    .unwrap();
    store.put_sample(s.clone()).unwrap();
    assert!(
        store
            .outbox(10_000)
            .unwrap()
            .contains(&SyncedRow::VoiceprintSample(s))
    );
}

/// T12.77b8: a live sample row that cannot be read stays queued and is counted, never sent
/// and never stopping the rows around it.
#[test]
fn store_voiceprint_sample_a_damaged_queued_row_is_counted_and_the_rest_still_read() {
    let dir = TempDir::new("sample-damaged-outbox");
    let path = dir.path().join("earshot.db");
    let ada = person(1);
    let p = empty_print(2, ada.id);
    let good = sample(3, T0 + 2, &p, vec![1.0, 0.0, 0.0]);
    let bad = sample(4, T0 + 3, &p, vec![0.0, 1.0, 0.0]);
    {
        let mut store = Store::open(&path).unwrap();
        store.put_person(ada.clone()).unwrap();
        store.put_voiceprint(p.clone()).unwrap();
        store.put_sample(good.clone()).unwrap();
        store.put_sample(bad.clone()).unwrap();
    }
    {
        let c = rusqlite::Connection::open(&path).unwrap();
        let two: Vec<u8> = [2.0f32, 0.0, 0.0]
            .iter()
            .flat_map(|x| x.to_le_bytes())
            .collect();
        c.execute(
            "UPDATE voiceprint_sample SET vector = ?1 WHERE id = ?2",
            rusqlite::params![two, bad.id().as_uuid().as_bytes().to_vec()],
        )
        .unwrap();
    }
    let store = Store::open(&path).unwrap();
    let page = store.outbox_page(100, &[], SyncKinds::ALL).unwrap();
    assert_eq!(page.unreadable, 1);
    assert_eq!(page.rows.len(), 3, "person, print and the readable sample");
    assert!(page.rows.contains(&SyncedRow::VoiceprintSample(good)));
    assert_eq!(
        store.outbox_except(100, &[], SyncKinds::ALL).unwrap(),
        page.rows
    );
    // Still queued: it counts as waiting until it is repaired or erased.
    assert_eq!(store.outbox_len_except(&[], SyncKinds::ALL).unwrap(), 4);
    let off = SyncKinds {
        voiceprints: false,
        dictionary: true,
    };
    assert_eq!(store.outbox_page(100, &[], off).unwrap().unreadable, 0);
    drop(store);
    // Any other store error (here a column the read needs is gone) fails the page, never
    // counted as a damaged row.
    {
        let c = rusqlite::Connection::open(&path).unwrap();
        c.execute_batch("ALTER TABLE voiceprint_sample RENAME COLUMN speech_ms TO gone")
            .unwrap();
    }
    let store = Store::open(&path).unwrap();
    assert!(store.outbox_page(100, &[], SyncKinds::ALL).is_err());
}
