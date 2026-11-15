//! T11.3e: migration 8, the user's Dictionary words in the store and their sync rules
//! (`cargo test -p earshot-store dictionary`; docs/sync-spec.md section 10.2). Every word is
//! invented.

mod common;

use std::cell::Cell;

use common::TempDir;
use earshot_core::identity::{DeviceId, EntityId, HlcTimestamp, ManualClock, SyncMeta};
use earshot_core::model::{DictionaryRepo, DictionaryWord, repo_contract};
use earshot_store::{
    ApplyRules, Incoming, SCHEMA_VERSION, Store, StoreError, SyncKinds, SyncedRow,
};

const HERE: DeviceId = DeviceId::from_bytes([0x11; 16]);
const THERE: DeviceId = DeviceId::from_bytes([0x22; 16]);
const T0: u64 = 1_800_000_000_000;

fn id(n: u8) -> EntityId {
    EntityId::from_bytes([0x01, 0x90, 0, 0, 0, 0, 0x70, 0, 0x80, 0, 0, 0, 0, 0, n, 3])
}

fn at(wall_ms: u64, node: DeviceId) -> HlcTimestamp {
    HlcTimestamp {
        wall_ms,
        logical: 0,
        node,
    }
}

fn word(n: u8, text: &str, stamp: HlcTimestamp) -> DictionaryWord {
    DictionaryWord {
        id: id(n),
        meta: SyncMeta {
            created_at: stamp,
            updated_at: stamp,
            deleted: false,
            origin_device_id: stamp.node,
        },
        word: text.into(),
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
        body: vec![0x5A; 32],
        row: Some(row),
    }
}

fn free(_: EntityId) -> bool {
    false
}

fn kinds(dictionary: bool) -> SyncKinds {
    SyncKinds {
        voiceprints: false,
        dictionary,
    }
}

fn apply(
    store: &mut Store,
    page: &[Incoming],
    rules: &ApplyRules<'_>,
) -> Result<earshot_store::Applied, StoreError> {
    let clock = ManualClock::new(T0 + 60_000);
    let mut hlc = store.hlc(HERE).unwrap();
    store.apply_remote(page, rules, &mut hlc, &clock)
}

fn rules(dictionary: bool) -> ApplyRules<'static> {
    ApplyRules {
        kinds: kinds(dictionary),
        leased: &free,
        now_ms: T0 + 60_000,
        after: None,
    }
}

#[test]
fn store_dictionary_schema_v7_database_gains_the_word_table() {
    let dir = TempDir::new("dictionary-v7");
    let path = dir.path().join("earshot.db");
    let outbox_before: i64 = {
        let c = rusqlite::Connection::open(&path).unwrap();
        c.execute_batch(include_str!("data/schema_v7.sql")).unwrap();
        let v: u32 = c
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(v, 7, "the fixture is a schema-7 database");
        c.query_row("SELECT count(*) FROM sync_outbox", [], |r| r.get(0))
            .unwrap()
    };
    let mut store = Store::open(&path).unwrap();
    assert_eq!(store.schema_version().unwrap(), SCHEMA_VERSION);
    const { assert!(SCHEMA_VERSION >= 8) };
    assert!(store.dictionary_words().unwrap().is_empty());
    assert_eq!(
        store.outbox_len().unwrap(),
        u64::try_from(outbox_before).unwrap(),
        "the migration queues nothing"
    );
    // A word written now is queued by the new triggers and sent like any row.
    let w = word(1, "Quillmarsh", at(T0 + 5, HERE));
    store.put_dictionary_word(w.clone()).unwrap();
    assert!(
        store
            .outbox(10_000)
            .unwrap()
            .iter()
            .any(|row| matches!(row, SyncedRow::DictionaryWord(q) if *q == w))
    );
}

#[test]
fn store_dictionary_in_memory_passes_the_dictionary_contract() {
    repo_contract::dictionary_repo(|| Store::open_in_memory().unwrap());
}

#[test]
fn store_dictionary_on_disk_passes_the_dictionary_contract() {
    let dir = TempDir::new("dictionary-contract");
    let n = Cell::new(0);
    repo_contract::dictionary_repo(|| {
        n.set(n.get() + 1);
        Store::open(&dir.path().join(format!("c{}.db", n.get()))).unwrap()
    });
}

#[test]
fn store_dictionary_word_rows_round_trip_through_the_sync_record() {
    let w = word(1, "QXR-7", at(T0, THERE));
    let record = serde_json::to_value(&w).unwrap();
    let row = SyncedRow::decode("dictionary_word", record)
        .unwrap()
        .unwrap();
    assert_eq!(row, SyncedRow::DictionaryWord(w));
    assert_eq!(row.table(), "dictionary_word");
    assert_eq!(row.meeting(), None);
    assert!(!row.waits_while_busy());
}

#[test]
fn store_dictionary_rows_wait_in_the_outbox_while_the_switch_is_off() {
    let mut store = Store::open_in_memory().unwrap();
    store
        .put_dictionary_word(word(1, "Quillmarsh", at(T0, HERE)))
        .unwrap();
    store
        .put_dictionary_word(word(2, "Velmora", at(T0 + 1, HERE)))
        .unwrap();
    assert_eq!(store.outbox_except(100, &[], kinds(true)).unwrap().len(), 2);
    assert!(
        store
            .outbox_except(100, &[], kinds(false))
            .unwrap()
            .is_empty()
    );
    assert_eq!(store.outbox_len_except(&[], kinds(true)).unwrap(), 2);
    assert_eq!(store.outbox_len_except(&[], kinds(false)).unwrap(), 0);
    // The rows stay queued for when the switch is turned on.
    assert_eq!(store.outbox_len().unwrap(), 2);
}

#[test]
fn store_dictionary_rows_are_skipped_while_the_switch_is_off_and_applied_while_on() {
    let mut store = Store::open_in_memory().unwrap();
    let page = [
        incoming(
            1,
            SyncedRow::DictionaryWord(word(1, "Quillmarsh", at(T0, THERE))),
        ),
        incoming(
            2,
            SyncedRow::DictionaryWord(word(2, "Velmora", at(T0 + 1, THERE))),
        ),
    ];
    let off = apply(&mut store, &page, &rules(false)).unwrap();
    assert_eq!((off.written, off.skipped), (0, 2));
    assert!(store.dictionary_words().unwrap().is_empty());
    assert!(store.held_rows().unwrap().is_empty(), "skipped, not held");

    let on = apply(&mut store, &page, &rules(true)).unwrap();
    assert_eq!((on.written, on.skipped), (2, 0));
    let words: Vec<String> = store
        .dictionary_words()
        .unwrap()
        .into_iter()
        .map(|w| w.word)
        .collect();
    assert_eq!(words, ["Quillmarsh", "Velmora"]);
    assert_eq!(
        store.outbox_len().unwrap(),
        0,
        "pulled rows are not sent back"
    );

    // A removal elsewhere is a later tombstone with the word cleared; it replaces the row.
    let mut gone = word(1, "", at(T0, THERE));
    gone.meta.updated_at = at(T0 + 9, THERE);
    gone.meta.deleted = true;
    let a = apply(
        &mut store,
        &[incoming(3, SyncedRow::DictionaryWord(gone.clone()))],
        &rules(true),
    )
    .unwrap();
    assert_eq!(a.written, 1);
    assert_eq!(store.dictionary_word(id(1)).unwrap(), Some(gone));
}

#[test]
fn store_dictionary_after_apply_runs_inside_the_page_and_its_error_rolls_it_back() {
    let mut store = Store::open_in_memory().unwrap();
    let page = [incoming(
        1,
        SyncedRow::DictionaryWord(word(1, "Quillmarsh", at(T0, THERE))),
    )];
    // The hook sees the page's rows before the commit.
    let seen = Cell::new(0usize);
    let count = |tx: &mut earshot_store::StoreTx<'_>| {
        seen.set(tx.dictionary_words().map_err(StoreError::from)?.len());
        Ok(())
    };
    let with_hook = ApplyRules {
        after: Some(&count),
        ..rules(true)
    };
    apply(&mut store, &page, &with_hook).unwrap();
    assert_eq!(seen.get(), 1);

    // A failing hook rolls the whole page back.
    let mut store = Store::open_in_memory().unwrap();
    let fail = |_: &mut earshot_store::StoreTx<'_>| Err(StoreError::Corrupt("refused".into()));
    let failing = ApplyRules {
        after: Some(&fail),
        ..rules(true)
    };
    assert!(apply(&mut store, &page, &failing).is_err());
    assert!(store.dictionary_words().unwrap().is_empty());
}
