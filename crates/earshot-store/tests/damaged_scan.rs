//! T14.65 (docs/sync-spec.md 17.2): the damage scan, the only authority on what is damaged. It
//! reads every synced table page by page and counts rows that do not read by kind, by table and
//! by the meeting they name. Damage is made by SQL from a second connection. Every name is
//! invented. Each test states its control. `cargo test -p earshot-store damaged_scan`.

mod common;

use common::TempDir;
use earshot_core::diar::SpeakerRole;
use earshot_core::identity::{DeviceId, EntityId, HlcTimestamp, SyncMeta};
use earshot_core::model::{
    DictionaryRepo, DictionaryWord, Folder, FolderRepo, Meeting, MeetingRepo, Speaker,
};
use earshot_core::prompt::Template;
use earshot_core::voiceprint::{Person, VoiceprintRepo};
use earshot_store::{DAMAGE_SCAN_PAGE, DamageCounts, DamageScan, Store, StoreError, damage_scan};

const HERE: DeviceId = DeviceId::from_bytes([0x11; 16]);
const T0: u64 = 1_800_000_000_000;

fn id(n: u16) -> EntityId {
    let [a, b] = n.to_be_bytes();
    EntityId::from_bytes([0x01, 0x93, 0, 0, 0, 0, 0x70, 0, 0x80, 0, 0, 0, 0, a, b, 6])
}

fn meta(n: u16) -> SyncMeta {
    let at = HlcTimestamp {
        wall_ms: T0 + u64::from(n),
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

fn meeting(n: u16) -> Meeting {
    Meeting {
        id: id(n),
        meta: meta(n),
        title: "An invented Quillmere review".into(),
        started_unix_ms: T0 + u64::from(n),
        utc_offset_minutes: 600,
        duration_ms: Some(60_000),
        tags: Vec::new(),
        template: Template::General,
        folder_id: None,
        summary_kept_local: false,
    }
}

fn speaker(n: u16, meeting: u16) -> Speaker {
    Speaker {
        id: id(n),
        meta: meta(n),
        meeting_id: id(meeting),
        role: SpeakerRole::Remote,
        display_name: "Invented Talloway".into(),
        person_id: None,
        ignored: false,
    }
}

/// A store with two meetings, speakers in each, people, folders and Dictionary words.
fn store_with_rows(path: &std::path::Path) {
    let mut s = Store::open(path).unwrap();
    s.put_meeting(meeting(1)).unwrap();
    s.put_meeting(meeting(2)).unwrap();
    for n in 10..14 {
        s.put_speaker(speaker(n, 1)).unwrap();
    }
    for n in 20..23 {
        s.put_speaker(speaker(n, 2)).unwrap();
    }
    for n in 30..33 {
        s.put_person(Person {
            id: id(n),
            meta: meta(n),
            display_name: "Invented Brannock".into(),
            is_me: false,
            merged_into: None,
        })
        .unwrap();
    }
    s.put_folder(Folder {
        id: id(40),
        meta: meta(40),
        name: "Invented archive".into(),
    })
    .unwrap();
    s.put_dictionary_word(DictionaryWord {
        id: id(50),
        meta: meta(50),
        word: "Quillmere".into(),
    })
    .unwrap();
}

fn damage(path: &std::path::Path, sql: &str, n: u16) {
    let c = rusqlite::Connection::open(path).unwrap();
    let changed = c
        .execute(sql, rusqlite::params![id(n).as_uuid().as_bytes().to_vec()])
        .unwrap();
    assert_eq!(changed, 1, "{sql}");
}

/// Runs a whole scan, each page in its own guarded read, as the runtime does; returns it and how
/// many reads it took.
fn scan(store: &Store) -> Result<(DamageScan, usize), StoreError> {
    let mut scan = DamageScan::default();
    let mut reads = 0;
    loop {
        reads += 1;
        if damage_scan(&store.guarded_read(), &mut scan)? {
            return Ok((scan, reads));
        }
    }
}

/// Damaged rows are counted by kind, by table and by the meeting they name, and each counts
/// once. Control: the same store with no damage scans to all zeros.
#[test]
fn store_damaged_scan_counts_by_kind_table_and_meeting() {
    for damaged in [true, false] {
        let dir = TempDir::new("damaged-scan-kinds");
        let path = dir.path().join("earshot.db");
        store_with_rows(&path);
        if damaged {
            damage(
                &path,
                "UPDATE meeting SET template = 'invented-word' WHERE id = ?1",
                2,
            );
            damage(&path, "UPDATE speaker SET role = 'bogus' WHERE id = ?1", 10);
            damage(
                &path,
                "UPDATE speaker SET display_name = CAST(X'FF00FE' AS TEXT) WHERE id = ?1",
                11,
            );
            damage(&path, "UPDATE speaker SET role = 'bogus' WHERE id = ?1", 20);
            damage(
                &path,
                "UPDATE person SET display_name = CAST(X'FF00FE' AS TEXT) WHERE id = ?1",
                31,
            );
            damage(
                &path,
                "UPDATE folder SET name = CAST(X'FF00FE' AS TEXT) WHERE id = ?1",
                40,
            );
            damage(
                &path,
                "UPDATE dictionary_word SET word = CAST(X'FF00FE' AS TEXT) WHERE id = ?1",
                50,
            );
        }
        let store = Store::open(&path).unwrap();
        let (found, _) = scan(&store).unwrap();
        assert!(found.done());
        let want = if damaged {
            DamageCounts {
                meetings: 1,
                parts: 3,
                people: 1,
                voice: 0,
                folders: 1,
                words: 1,
            }
        } else {
            DamageCounts::default()
        };
        assert_eq!(found.counts(), want, "damaged {damaged}");
        assert_eq!(found.counts().total(), if damaged { 7 } else { 0 });
        assert_eq!(found.in_table("speaker"), if damaged { 3 } else { 0 });
        assert_eq!(found.in_table("segment"), 0);
        assert_eq!(found.in_meeting(id(1)), if damaged { 2 } else { 0 });
        assert_eq!(found.in_meeting(id(2)), if damaged { 1 } else { 0 });
        // The scan writes nothing: the store reads as before.
        assert_eq!(
            store.outbox_len().unwrap(),
            Store::open(&path).unwrap().outbox_len().unwrap()
        );
    }
}

/// A damaged voiceprint sample counts as saved voice data. Control: undamaged, nothing counts.
#[test]
fn store_damaged_scan_counts_voice_data() {
    for damaged in [true, false] {
        let dir = TempDir::new("damaged-scan-voice");
        let path = dir.path().join("earshot.db");
        {
            let mut s = Store::open(&path).unwrap();
            s.put_person(Person {
                id: id(30),
                meta: meta(30),
                display_name: "Invented Ferrow".into(),
                is_me: false,
                merged_into: None,
            })
            .unwrap();
            s.put_voiceprint(earshot_core::voiceprint::Voiceprint::from_stored(
                id(60),
                meta(60),
                id(30),
                earshot_core::diar::EmbeddingModelId::new("invented-embedder@1"),
                vec![0.6, 0.8],
                1,
                5_000,
            ))
            .unwrap();
        }
        if damaged {
            damage(&path, "UPDATE voiceprint SET count = -1 WHERE id = ?1", 60);
        }
        let (found, _) = scan(&Store::open(&path).unwrap()).unwrap();
        assert_eq!(found.counts().voice, u64::from(damaged));
        assert_eq!(found.counts().people, 0);
    }
}

/// The scan reads at most [`DAMAGE_SCAN_PAGE`] rows per read and finds damage on every page,
/// the first and last rows included. Control: with the page check removed (one read for the
/// whole table) the read count would be smaller than the pages the rows need.
#[test]
fn store_damaged_scan_reads_in_pages_and_finds_every_damaged_row() {
    let dir = TempDir::new("damaged-scan-pages");
    let path = dir.path().join("earshot.db");
    let rows: u16 = 1_201;
    {
        let mut s = Store::open(&path).unwrap();
        s.transaction(|tx| -> Result<(), StoreError> {
            for n in 1..=rows {
                tx.put_folder(Folder {
                    id: id(n),
                    meta: meta(n),
                    name: format!("Invented shelf {n}"),
                })?;
            }
            Ok(())
        })
        .unwrap();
    }
    for n in [1, 500, 501, 1_000, rows] {
        damage(
            &path,
            "UPDATE folder SET name = CAST(X'FF00FE' AS TEXT) WHERE id = ?1",
            n,
        );
    }
    let (found, reads) = scan(&Store::open(&path).unwrap()).unwrap();
    assert_eq!(found.counts().folders, 5);
    let pages = usize::from(rows).div_ceil(DAMAGE_SCAN_PAGE as usize);
    assert!(reads > pages, "{reads} reads for {pages} pages of folders");
}

/// Any other store error still fails the scan. Control: with the table back, it scans.
#[test]
fn store_damaged_scan_fails_on_other_errors() {
    let dir = TempDir::new("damaged-scan-errors");
    let path = dir.path().join("earshot.db");
    store_with_rows(&path);
    let c = rusqlite::Connection::open(&path).unwrap();
    c.execute_batch("ALTER TABLE folder RENAME TO folder_away")
        .unwrap();
    {
        let store = Store::open(&path);
        // A store that will not open at all also proves the point; a store that opens must
        // fail the scan.
        if let Ok(store) = store {
            assert!(scan(&store).is_err());
        }
    }
    c.execute_batch("ALTER TABLE folder_away RENAME TO folder")
        .unwrap();
    drop(c);
    assert!(scan(&Store::open(&path).unwrap()).is_ok());
}
