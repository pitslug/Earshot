//! SQLite store: migrations, WAL, repository contracts (`cargo test -p earshot-store`).

mod common;

use std::time::Duration;

use common::TempDir;
use earshot_core::diar::{Embedding, EmbeddingModelId};
use earshot_core::identity::{DeviceId, Hlc, ManualClock};
use earshot_core::model::{MeetingRepo, repo_contract};
use earshot_core::prompt::Template;
use earshot_core::voiceprint::{
    EnrolmentConfig, Person, Stamp, VoiceprintRepo, enroll, forget_voiceprints, merge_persons,
};
use earshot_store::{SCHEMA_VERSION, Store, StoreError};
use rand_chacha::ChaCha8Rng;
use rand_core::SeedableRng;

struct Seeded {
    clock: ManualClock,
    hlc: Hlc,
    rng: ChaCha8Rng,
}

impl Seeded {
    fn new() -> Self {
        Seeded {
            clock: ManualClock::new(1_800_000_000_000),
            hlc: Hlc::new(DeviceId::from_bytes([3; 16])),
            rng: ChaCha8Rng::seed_from_u64(1),
        }
    }
    fn stamp(&mut self) -> Stamp<'_, ManualClock, ChaCha8Rng> {
        self.clock.advance_ms(1);
        Stamp {
            clock: &self.clock,
            hlc: &mut self.hlc,
            rng: &mut self.rng,
        }
    }
}

#[test]
fn store_in_memory_passes_the_meeting_repo_contract() {
    repo_contract::meeting_repo(|| Store::open_in_memory().unwrap());
}

#[test]
fn store_in_memory_passes_the_audio_file_repo_contract() {
    repo_contract::audio_file_repo(|| Store::open_in_memory().unwrap());
}

#[test]
fn store_in_memory_passes_the_export_repo_contract() {
    repo_contract::export_repo(|| Store::open_in_memory().unwrap());
}

#[test]
fn store_on_disk_passes_the_repo_contracts() {
    let dir = TempDir::new("contract");
    let n = std::cell::Cell::new(0);
    let make = || {
        n.set(n.get() + 1);
        Store::open(&dir.path().join(format!("c{}.db", n.get()))).unwrap()
    };
    repo_contract::meeting_repo(make);
    repo_contract::audio_file_repo(make);
    repo_contract::export_repo(make);
}

#[test]
fn store_opens_in_wal_mode_with_foreign_keys_and_the_latest_schema() {
    let dir = TempDir::new("wal");
    let store = Store::open(&dir.path().join("earshot.db")).unwrap();
    assert_eq!(store.journal_mode().unwrap(), "wal");
    assert!(store.foreign_keys().unwrap());
    assert_eq!(store.schema_version().unwrap(), SCHEMA_VERSION);
}

/// TODO 69 (T12.36): written before the fix, it reproduced the failure against the pre-fix
/// `Store::open` ("round 0: sqlite: database is locked"), and passed 8 runs of 8 after it.
#[test]
fn store_opens_racing_on_a_brand_new_file_all_succeed_in_wal_mode() {
    // A brand-new file starts in rollback mode; every opener switches it to WAL, which
    // needs an exclusive lock that SQLite's busy handler does not always wait for.
    const OPENERS: usize = 8;
    let dir = TempDir::new("wal-race");
    for round in 0..20 {
        let path = dir.path().join(format!("race{round}.db"));
        let start = std::sync::Arc::new(std::sync::Barrier::new(OPENERS));
        let threads: Vec<_> = (0..OPENERS)
            .map(|_| {
                let (path, start) = (path.clone(), std::sync::Arc::clone(&start));
                std::thread::spawn(move || {
                    start.wait();
                    let store = Store::open(&path)?;
                    Ok::<_, StoreError>((store.journal_mode()?, store.schema_version()?))
                })
            })
            .collect();
        for t in threads {
            let (mode, version) = t
                .join()
                .unwrap()
                .unwrap_or_else(|e| panic!("round {round}: {e}"));
            assert_eq!(mode, "wal");
            assert_eq!(version, SCHEMA_VERSION);
        }
    }
}

#[test]
fn store_reopen_keeps_data_and_does_not_reapply_migrations() {
    let dir = TempDir::new("reopen");
    let path = dir.path().join("earshot.db");
    let mut w = Seeded::new();
    let m = earshot_core::model::Meeting::new("Kept", 1, 60, Template::General, &mut w.stamp());
    {
        let mut store = Store::open(&path).unwrap();
        store.put_meeting(m.clone()).unwrap();
    }
    let store = Store::open(&path).unwrap();
    assert_eq!(store.schema_version().unwrap(), SCHEMA_VERSION);
    assert_eq!(store.meeting(m.id).unwrap(), Some(m));
}

#[test]
fn store_refuses_a_database_from_a_newer_schema() {
    let dir = TempDir::new("newer");
    let path = dir.path().join("earshot.db");
    {
        let c = rusqlite::Connection::open(&path).unwrap();
        c.pragma_update(None, "user_version", SCHEMA_VERSION + 1)
            .unwrap();
    }
    match Store::open(&path) {
        Err(StoreError::SchemaTooNew { found, supported }) => {
            assert_eq!((found, supported), (SCHEMA_VERSION + 1, SCHEMA_VERSION));
        }
        Err(e) => panic!("unexpected error {e}"),
        Ok(_) => panic!("a newer schema must be refused"),
    }
}

#[test]
fn store_transaction_rolls_back_on_error() {
    let mut store = Store::open_in_memory().unwrap();
    let mut w = Seeded::new();
    let m = earshot_core::model::Meeting::new("", 1, 0, Template::General, &mut w.stamp());
    let id = m.id;
    let r: Result<(), StoreError> = store.transaction(|tx| {
        tx.put_meeting(m)?;
        Err(StoreError::Corrupt("abort".into()))
    });
    assert!(r.is_err());
    assert_eq!(store.meeting(id).unwrap(), None, "rolled back");
    let m2 = earshot_core::model::Meeting::new("", 2, 0, Template::General, &mut w.stamp());
    store
        .transaction(|tx| -> Result<(), StoreError> { Ok(tx.put_meeting(m2.clone())?) })
        .unwrap();
    assert_eq!(store.meeting(m2.id).unwrap(), Some(m2));
}

#[test]
fn store_device_id_is_created_once_and_stable_across_reopen() {
    let dir = TempDir::new("device");
    let path = dir.path().join("earshot.db");
    let mut w = Seeded::new();
    let a = Store::open(&path)
        .unwrap()
        .device_id(&w.clock, &mut w.rng)
        .unwrap();
    let b = Store::open(&path)
        .unwrap()
        .device_id(&w.clock, &mut w.rng)
        .unwrap();
    assert_eq!(a, b);
}

#[test]
fn store_voiceprints_round_trip_and_forget_is_per_identity_and_durable() {
    let dir = TempDir::new("vp");
    let path = dir.path().join("earshot.db");
    let mut w = Seeded::new();
    let model = EmbeddingModelId::new("test-model@1");
    let emb = |v: &[f32]| Embedding::new(model.clone(), v.to_vec());
    let (ada, dup) = {
        let mut store = Store::open(&path).unwrap();
        let ada = Person::new("Ada", false, &mut w.stamp());
        let dup = Person::new("Ada (laptop)", false, &mut w.stamp());
        store.put_person(ada.clone()).unwrap();
        store.put_person(dup.clone()).unwrap();
        let cfg = EnrolmentConfig::default();
        enroll(
            &mut store,
            ada.id,
            &[emb(&[1.0, 0.0, 0.0]), emb(&[0.9, 0.1, 0.0])],
            Duration::from_secs(12),
            &cfg,
            &mut w.stamp(),
        )
        .unwrap();
        enroll(
            &mut store,
            dup.id,
            &[emb(&[0.0, 1.0, 0.0])],
            Duration::from_secs(12),
            &cfg,
            &mut w.stamp(),
        )
        .unwrap();
        merge_persons(&mut store, dup.id, ada.id, &mut w.hlc, &w.clock).unwrap();
        (ada, dup)
    };
    let mut store = Store::open(&path).unwrap();
    let vps = store.voiceprints(&model).unwrap();
    assert_eq!(vps.len(), 2);
    let a = store.voiceprints_of(ada.id).unwrap();
    assert_eq!(a.len(), 1);
    assert_eq!(a[0].count, 2);
    assert_eq!(a[0].speech_ms, 12_000);
    assert_eq!(
        store.person(dup.id).unwrap().unwrap().merged_into,
        Some(ada.id)
    );

    let n = forget_voiceprints(&mut store, ada.id, &mut w.hlc, &w.clock).unwrap();
    assert_eq!(n, 2, "the merged duplicate's voiceprint is deleted too");
    drop(store);
    let store = Store::open(&path).unwrap();
    for v in store.voiceprints(&model).unwrap() {
        assert!(v.meta.deleted, "tombstoned after reopen");
        assert!(v.sum.iter().all(|&x| x == 0.0), "vector zeroed on disk");
    }
}

#[test]
fn store_forget_voiceprints_of_several_persons_forgets_all_of_them_at_once() {
    let dir = TempDir::new("vp-several");
    let path = dir.path().join("earshot.db");
    let mut w = Seeded::new();
    let model = EmbeddingModelId::new("test-model@1");
    let emb = |v: &[f32]| Embedding::new(model.clone(), v.to_vec());
    let mut store = Store::open(&path).unwrap();
    let cfg = EnrolmentConfig::default();
    let mut people = Vec::new();
    for (name, v) in [("Me", [1.0, 0.0, 0.0]), ("Me too", [0.0, 1.0, 0.0])] {
        let p = Person::new(name, true, &mut w.stamp());
        store.put_person(p.clone()).unwrap();
        enroll(
            &mut store,
            p.id,
            &[emb(&v)],
            Duration::from_secs(12),
            &cfg,
            &mut w.stamp(),
        )
        .unwrap();
        people.push(p.id);
    }
    let other = Person::new("Ada", false, &mut w.stamp());
    store.put_person(other.clone()).unwrap();
    enroll(
        &mut store,
        other.id,
        &[emb(&[0.0, 0.0, 1.0])],
        Duration::from_secs(12),
        &cfg,
        &mut w.stamp(),
    )
    .unwrap();
    let n = store
        .forget_voiceprints_of(&people, &mut w.hlc, &w.clock)
        .unwrap();
    assert_eq!(n, 2);
    drop(store);
    let store = Store::open(&path).unwrap();
    for p in &people {
        for v in store.voiceprints_of(*p).unwrap() {
            assert!(v.meta.deleted && v.sum.iter().all(|&x| x == 0.0));
        }
    }
    assert!(
        !store.voiceprints_of(other.id).unwrap()[0].meta.deleted,
        "others are kept"
    );
}

#[test]
fn store_local_values_are_device_local_key_value_pairs() {
    let dir = TempDir::new("local-values");
    let path = dir.path().join("earshot.db");
    {
        let mut store = Store::open(&path).unwrap();
        assert_eq!(store.local_value("export/x").unwrap(), None);
        store.set_local_value("export/x", b"one").unwrap();
        store.set_local_value("export/x", b"two").unwrap();
        assert_eq!(
            store.local_value("export/x").unwrap().as_deref(),
            Some(&b"two"[..])
        );
        // The device id is not writable through this API.
        assert!(store.set_local_value("device_id", b"x").is_err());
    }
    // Persistent across reopen.
    let store = Store::open(&path).unwrap();
    assert_eq!(
        store.local_value("export/x").unwrap().as_deref(),
        Some(&b"two"[..])
    );
}

#[test]
fn store_local_values_inside_a_transaction_commit_or_roll_back_with_it() {
    let dir = TempDir::new("local-values-tx");
    let mut store = Store::open(&dir.path().join("earshot.db")).unwrap();
    store.set_local_value("k", b"before").unwrap();
    // A failed transaction leaves the value as it was.
    let failed: Result<(), StoreError> = store.transaction(|tx| {
        assert_eq!(
            tx.local_value("k").unwrap().as_deref(),
            Some(&b"before"[..])
        );
        tx.set_local_value("k", b"lost").unwrap();
        assert_eq!(tx.local_value("k").unwrap().as_deref(), Some(&b"lost"[..]));
        Err(StoreError::Corrupt("abort".into()))
    });
    assert!(failed.is_err());
    assert_eq!(
        store.local_value("k").unwrap().as_deref(),
        Some(&b"before"[..])
    );
    store
        .transaction(|tx| -> Result<(), StoreError> {
            assert!(tx.set_local_value("device_id", b"x").is_err());
            tx.set_local_value("k", b"after")
        })
        .unwrap();
    assert_eq!(
        store.local_value("k").unwrap().as_deref(),
        Some(&b"after"[..])
    );
}
