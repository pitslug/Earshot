//! The read-only open for evaluation tools (T12.25a, `cargo test -p earshot-store read_only`):
//! a copy of a store read without a write. Invented meetings only.

mod common;

use common::TempDir;
use earshot_core::identity::{DeviceId, Hlc, ManualClock};
use earshot_core::model::{Meeting, MeetingRepo, Stamp};
use earshot_core::prompt::Template;
use earshot_store::{ReadOnlyStore, SCHEMA_VERSION, Store, StoreError};
use rand_chacha::ChaCha8Rng;
use rand_core::SeedableRng;

fn meeting(title: &str, n: u64) -> Meeting {
    let clock = ManualClock::new(1_800_000_000_000 + n);
    let mut hlc = Hlc::new(DeviceId::from_bytes([3; 16]));
    let mut rng = ChaCha8Rng::seed_from_u64(n);
    let mut stamp = Stamp {
        clock: &clock,
        hlc: &mut hlc,
        rng: &mut rng,
    };
    Meeting::new(
        title,
        1_800_000_000_000 + n,
        0,
        Template::General,
        &mut stamp,
    )
}

fn files(dir: &std::path::Path) -> Vec<(String, Vec<u8>)> {
    let mut out: Vec<(String, Vec<u8>)> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| {
            let e = e.unwrap();
            (
                e.file_name().to_string_lossy().into_owned(),
                std::fs::read(e.path()).unwrap(),
            )
        })
        .collect();
    out.sort();
    out
}

#[test]
fn read_only_open_reads_rows_and_refuses_writes_without_changing_the_file() {
    let dir = TempDir::new("read-only");
    let path = dir.path().join("earshot.db");
    let first = meeting("An invented planning meeting", 1);
    {
        let mut store = Store::open(&path).unwrap();
        store.put_meeting(first.clone()).unwrap();
    }
    let before = files(dir.path());
    {
        let mut ro = ReadOnlyStore::open(&path).unwrap();
        assert_eq!(ro.meetings().unwrap(), vec![first.clone()]);
        assert_eq!(ro.meeting(first.id).unwrap(), Some(first.clone()));
        assert!(ro.put_meeting(meeting("Another", 2)).is_err());
        assert_eq!(ro.meetings().unwrap().len(), 1);
    }
    // The database file is byte for byte what the writer left (a `-shm` may appear in WAL
    // mode; the database itself never changes).
    let after = files(dir.path());
    let db = |v: &[(String, Vec<u8>)]| v.iter().find(|(n, _)| n == "earshot.db").cloned();
    assert_eq!(db(&after), db(&before));
}

#[test]
fn read_only_open_sees_what_is_still_in_the_wal_of_a_copy() {
    // A copy taken while the writer still holds its connection: the rows are in the WAL.
    let dir = TempDir::new("read-only-wal");
    let path = dir.path().join("earshot.db");
    let copy_dir = TempDir::new("read-only-wal-copy");
    let copy = copy_dir.path().join("earshot.db");
    let m = meeting("An invented review", 3);
    let mut store = Store::open(&path).unwrap();
    store.put_meeting(m.clone()).unwrap();
    std::fs::copy(&path, &copy).unwrap();
    let wal = dir.path().join("earshot.db-wal");
    assert!(wal.is_file(), "the writer has a WAL");
    std::fs::copy(&wal, copy_dir.path().join("earshot.db-wal")).unwrap();
    drop(store);
    let ro = ReadOnlyStore::open(&copy).unwrap();
    assert_eq!(ro.meetings().unwrap(), vec![m]);
}

#[test]
fn read_only_open_refuses_another_schema_version_and_a_missing_file() {
    let dir = TempDir::new("read-only-version");
    assert!(ReadOnlyStore::open(&dir.path().join("none.db")).is_err());
    for (version, newer) in [(SCHEMA_VERSION + 1, true), (SCHEMA_VERSION - 1, false)] {
        let path = dir.path().join(format!("v{version}.db"));
        let c = rusqlite::Connection::open(&path).unwrap();
        c.pragma_update(None, "user_version", version).unwrap();
        drop(c);
        let err = ReadOnlyStore::open(&path).unwrap_err();
        if newer {
            assert!(matches!(err, StoreError::SchemaTooNew { .. }), "{err}");
        } else {
            assert!(matches!(err, StoreError::Corrupt(_)), "{err}");
        }
        // Nothing was migrated.
        let c = rusqlite::Connection::open(&path).unwrap();
        let v: u32 = c
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(v, version);
    }
}

#[test]
fn read_only_open_reads_persons_voiceprints_and_local_values_and_refuses_their_writes() {
    use earshot_core::diar::{Embedding, EmbeddingModelId};
    use earshot_core::voiceprint::{
        EnrolmentConfig, Person, Stamp as VpStamp, VoiceprintRepo, enroll,
    };
    let dir = TempDir::new("read-only-voices");
    let path = dir.path().join("earshot.db");
    let model = EmbeddingModelId::new("invented-embedder@0");
    let clock = ManualClock::new(1_800_000_000_000);
    let mut hlc = Hlc::new(DeviceId::from_bytes([4; 16]));
    let mut rng = ChaCha8Rng::seed_from_u64(71);
    let (person, print) = {
        let mut store = Store::open(&path).unwrap();
        let mut stamp = VpStamp {
            clock: &clock,
            hlc: &mut hlc,
            rng: &mut rng,
        };
        let person = Person::new("An invented colleague", false, &mut stamp);
        store.put_person(person.clone()).unwrap();
        let e = Embedding::new(model.clone(), vec![0.6, 0.8, 0.0]);
        let print = enroll(
            &mut store,
            person.id,
            &[e],
            std::time::Duration::from_secs(20),
            &EnrolmentConfig::default(),
            &mut stamp,
        )
        .unwrap();
        store
            .set_local_value("speaker-identities/invented", b"\x02\x00\x00\x00{}")
            .unwrap();
        (person, print)
    };
    let before = files(dir.path());
    {
        let mut ro = ReadOnlyStore::open(&path).unwrap();
        assert_eq!(ro.persons().unwrap(), vec![person.clone()]);
        assert_eq!(ro.person(person.id).unwrap(), Some(person.clone()));
        let prints = ro.voiceprints(&model).unwrap();
        assert_eq!(prints.len(), 1);
        assert_eq!(prints[0].id(), print);
        assert_eq!(ro.voiceprints_of(person.id).unwrap(), prints);
        assert!(
            ro.voiceprints(&EmbeddingModelId::new("another@0"))
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            ro.local_value("speaker-identities/invented").unwrap(),
            Some(b"\x02\x00\x00\x00{}".to_vec())
        );
        assert_eq!(ro.local_value("speaker-identities/none").unwrap(), None);
        assert!(ro.put_person(person.clone()).is_err());
        assert!(ro.put_voiceprint(prints[0].clone()).is_err());
    }
    let after = files(dir.path());
    let db = |v: &[(String, Vec<u8>)]| v.iter().find(|(n, _)| n == "earshot.db").cloned();
    assert_eq!(db(&after), db(&before));
}

#[test]
fn read_only_open_reads_audio_file_rows_and_refuses_their_writes() {
    use earshot_core::capture::TrackId;
    use earshot_core::model::{AudioFileRepo, ClockDomain, Track};
    use earshot_store::{AudioProvenance, DataDir};
    let dir = TempDir::new("read-only-audio");
    let data = DataDir::new(dir.path());
    let clock = ManualClock::new(1_800_000_000_000);
    let mut hlc = Hlc::new(DeviceId::from_bytes([6; 16]));
    let mut rng = ChaCha8Rng::seed_from_u64(73);
    let m = meeting("An invented call", 4);
    let file = {
        let mut store = Store::open(&data.db_path()).unwrap();
        store.put_meeting(m.clone()).unwrap();
        let mut stamp = Stamp {
            clock: &clock,
            hlc: &mut hlc,
            rng: &mut rng,
        };
        let t = Track::new(m.id, TrackId::Reference, None, ClockDomain::Qpc, &mut stamp);
        store.put_track(t.clone()).unwrap();
        let mut w = data.track_writer(m.id, t.kind, 16_000).unwrap();
        w.write(&[0.1; 1600]).unwrap();
        let provenance = AudioProvenance {
            pipeline_version: "earshot-0.1.0".into(),
            aec_version: None,
        };
        store
            .finalize_track_audio(w, &data, &t, provenance, &clock, &mut rng)
            .unwrap()
    };
    let before = std::fs::read(data.db_path()).unwrap();
    {
        let mut ro = ReadOnlyStore::open(&data.db_path()).unwrap();
        assert_eq!(ro.audio_files(m.id).unwrap(), vec![file.clone()]);
        assert_eq!(ro.all_audio_files().unwrap(), vec![file.clone()]);
        assert!(ro.put_audio_file(file.clone()).is_err());
        assert!(ro.delete_audio_file(file.id).is_err());
        assert_eq!(ro.audio_files(m.id).unwrap().len(), 1);
    }
    assert_eq!(std::fs::read(data.db_path()).unwrap(), before);
}
