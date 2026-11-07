//! Store hardening (T5.2c): HLC high-water mark, atomic re-transcription, biometric
//! deletion leaving no trace on disk, orphan audio sweep and stale `.part` cleanup.

mod common;

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use common::TempDir;
use earshot_core::asr::{MeetingTime, Segment, Word};
use earshot_core::capture::TrackId;
use earshot_core::diar::{Embedding, EmbeddingModelId};
use earshot_core::identity::{DeviceId, EntityId, Hlc, HlcTimestamp, ManualClock, SyncMeta};
use earshot_core::model::{
    ClockDomain, Meeting, MeetingRepo, Note, Stamp, Track, TranscriptPass, live_segments,
};
use earshot_core::prompt::Template;
use earshot_core::voiceprint::{EnrolmentConfig, Person, VoiceprintRepo, enroll};
use earshot_store::{
    AudioProvenance, DataDir, ORPHAN_GRACE_MS, RetentionPolicy, Store, run_retention,
};
use rand_chacha::ChaCha8Rng;
use rand_core::SeedableRng;

const DAY_MS: u64 = 86_400_000;

struct World {
    clock: ManualClock,
    hlc: Hlc,
    rng: ChaCha8Rng,
}

impl World {
    fn new() -> Self {
        World {
            clock: ManualClock::new(1_800_000_000_000),
            hlc: Hlc::new(DeviceId::from_bytes([4; 16])),
            rng: ChaCha8Rng::seed_from_u64(11),
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

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

fn age(path: &std::path::Path, by: Duration) {
    let f = std::fs::OpenOptions::new().write(true).open(path).unwrap();
    f.set_modified(SystemTime::now() - by).unwrap();
}

#[test]
fn store_hlc_resumes_after_the_highest_stored_timestamp() {
    let mut store = Store::open_in_memory().unwrap();
    let me = DeviceId::from_bytes([1; 16]);
    let clock = ManualClock::new(1_000);
    // Empty store: a fresh clock.
    assert!(store.hlc(me).unwrap().now(&clock).wall_ms == 1_000);

    let mut w = World::new();
    let m = Meeting::new("", 1, 0, Template::General, &mut w.stamp());
    store.put_meeting(m.clone()).unwrap();
    // A note synced from another device, stamped far ahead of this device's clock.
    let other = DeviceId::from_bytes([2; 16]);
    let high = HlcTimestamp {
        wall_ms: 9_000_000_000_000,
        logical: 3,
        node: other,
    };
    let mut n = Note::new(m.id, MeetingTime::ZERO, "x", &mut w.stamp());
    n.meta = SyncMeta {
        created_at: high,
        updated_at: high,
        deleted: false,
        origin_device_id: other,
    };
    store.put_note(n).unwrap();
    let t = store.hlc(me).unwrap().now(&clock);
    assert!(t > high, "{t:?} must follow the stored {high:?}");
    assert_eq!(t.node, me);
}

#[test]
fn store_replace_segments_is_one_transaction() {
    let mut w = World::new();
    let mut store = Store::open_in_memory().unwrap();
    let m = Meeting::new("", 1, 0, Template::General, &mut w.stamp());
    store.put_meeting(m.clone()).unwrap();
    let t = Track::new(m.id, TrackId::Mic, None, ClockDomain::Qpc, &mut w.stamp());
    store.put_track(t.clone()).unwrap();
    let seg = |w: &mut World, start: u64| {
        Segment::from_words(
            EntityId::generate(&w.clock, &mut w.rng),
            t.id,
            vec![Word::new("w", start, start + 50, 1.0)],
            MeetingTime::ZERO,
        )
    };
    let first = vec![seg(&mut w, 0), seg(&mut w, 100)];
    store
        .replace_segments(
            m.id,
            TranscriptPass::Final,
            first.clone(),
            &mut w.hlc,
            &w.clock,
        )
        .unwrap();
    // The second segment names a speaker that does not exist: the insert fails after the
    // old rows were tombstoned, and the whole replacement must roll back.
    let mut bad = seg(&mut w, 300);
    bad.speaker_id = Some(EntityId::from_bytes([9; 16]));
    let err = store
        .replace_segments(
            m.id,
            TranscriptPass::Final,
            vec![seg(&mut w, 200), bad],
            &mut w.hlc,
            &w.clock,
        )
        .unwrap_err();
    assert!(err.to_string().contains("speaker"), "{err}");
    let live: Vec<EntityId> = live_segments(&store, m.id, TranscriptPass::Final)
        .unwrap()
        .iter()
        .map(|r| r.segment.id)
        .collect();
    assert_eq!(live, first.iter().map(|s| s.id).collect::<Vec<_>>());
    assert_eq!(
        store.segments(m.id).unwrap().len(),
        2,
        "nothing new inserted"
    );
}

/// Whether `needle` occurs in the database file or its WAL.
fn on_disk(db: &std::path::Path, needle: &[u8]) -> bool {
    let mut found = false;
    for suffix in ["", "-wal", "-journal"] {
        let p = std::path::PathBuf::from(format!("{}{suffix}", db.display()));
        if let Ok(bytes) = std::fs::read(&p) {
            found |= bytes.windows(needle.len()).any(|w| w == needle);
        }
    }
    found
}

#[test]
fn store_forget_voiceprints_leaves_no_trace_on_disk() {
    let dir = TempDir::new("forget");
    let db = dir.path().join("earshot.db");
    let mut w = World::new();
    let mut store = Store::open(&db).unwrap();
    assert!(store.secure_delete().unwrap());
    let model = EmbeddingModelId::new("test-model@1");
    let ada = Person::new("Ada", false, &mut w.stamp());
    store.put_person(ada.clone()).unwrap();
    let v: Vec<f32> = (0..64).map(|i| 0.01 + i as f32 * 0.0137).collect();
    enroll(
        &mut store,
        ada.id,
        &[Embedding::new(model.clone(), v)],
        Duration::from_secs(12),
        &EnrolmentConfig::default(),
        &mut w.stamp(),
    )
    .unwrap();
    let sum = store.voiceprints_of(ada.id).unwrap()[0].sum.clone();
    let needle: Vec<u8> = sum[10..16].iter().flat_map(|x| x.to_le_bytes()).collect();
    assert!(
        on_disk(&db, &needle),
        "sanity: the vector is on disk before forgetting"
    );

    let n = store
        .forget_voiceprints(ada.id, &mut w.hlc, &w.clock)
        .unwrap();
    assert_eq!(n, 1);
    assert!(
        !on_disk(&db, &needle),
        "the biometric vector must not survive in free pages or the WAL"
    );
    drop(store);
    assert!(!on_disk(&db, &needle));
}

#[test]
fn store_forgotten_voiceprints_do_not_outlive_the_session_when_a_reader_blocks() {
    let dir = TempDir::new("forget-busy");
    let db = dir.path().join("earshot.db");
    let mut w = World::new();
    let mut store = Store::open(&db).unwrap();
    let model = EmbeddingModelId::new("test-model@1");
    let ada = Person::new("Ada", false, &mut w.stamp());
    store.put_person(ada.clone()).unwrap();
    let v: Vec<f32> = (0..64).map(|i| 0.02 + i as f32 * 0.0111).collect();
    enroll(
        &mut store,
        ada.id,
        &[Embedding::new(model.clone(), v)],
        Duration::from_secs(12),
        &EnrolmentConfig::default(),
        &mut w.stamp(),
    )
    .unwrap();
    let sum = store.voiceprints_of(ada.id).unwrap()[0].sum.clone();
    let needle: Vec<u8> = sum[10..16].iter().flat_map(|x| x.to_le_bytes()).collect();
    // Fold the enrolment into the main file so the vector sits in a database page.
    store.checkpoint().unwrap();

    // Another connection holds a read snapshot: the checkpoint cannot complete.
    let reader = rusqlite::Connection::open(&db).unwrap();
    reader.execute_batch("BEGIN").unwrap();
    let _: i64 = reader
        .query_row("SELECT count(*) FROM voiceprint", [], |r| r.get(0))
        .unwrap();
    let started = std::time::Instant::now();
    let err = store
        .forget_voiceprints(ada.id, &mut w.hlc, &w.clock)
        .unwrap_err();
    assert!(err.is_retryable(), "{err}");
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "retries are brief"
    );
    assert!(
        store.voiceprints_of(ada.id).unwrap()[0].meta.deleted,
        "deletion committed"
    );
    assert!(
        on_disk(&db, &needle),
        "sanity: the old page is still in the main file"
    );

    // The reader ends its transaction but stays connected (another tool or a second
    // Earshot window), so SQLite's own last-connection checkpoint does not run: only the
    // store's closing checkpoint removes the vector.
    reader.execute_batch("COMMIT").unwrap();
    drop(store);
    assert!(
        !on_disk(&db, &needle),
        "the forgotten vector outlived the session"
    );
    drop(reader);
}

#[test]
fn store_open_folds_a_leftover_wal_into_the_database() {
    let dir = TempDir::new("open-ckpt");
    let db = dir.path().join("earshot.db");
    drop(Store::open(&db).unwrap());
    // A session that ended without its closing checkpoint (crash): WAL frames are left.
    {
        let c = rusqlite::Connection::open(&db).unwrap();
        c.pragma_update(None, "journal_mode", "WAL").unwrap();
        c.execute(
            "INSERT INTO local_state (key, value) VALUES ('probe', x'01')",
            [],
        )
        .unwrap();
        // Leak the connection so SQLite does not checkpoint on close.
        std::mem::forget(c);
    }
    let wal = std::path::PathBuf::from(format!("{}-wal", db.display()));
    assert!(
        std::fs::metadata(&wal).unwrap().len() > 0,
        "sanity: WAL has frames"
    );
    let _store = Store::open(&db).unwrap();
    assert_eq!(
        std::fs::metadata(&wal).unwrap().len(),
        0,
        "WAL truncated at open"
    );
}

fn record(data: &DataDir, store: &mut Store, w: &mut World) -> earshot_core::model::AudioFile {
    let m = Meeting::new("", 1, 0, Template::General, &mut w.stamp());
    store.put_meeting(m.clone()).unwrap();
    let t = Track::new(m.id, TrackId::Mic, None, ClockDomain::Qpc, &mut w.stamp());
    store.put_track(t.clone()).unwrap();
    let mut wr = data.track_writer(m.id, TrackId::Mic, 16_000).unwrap();
    wr.write(&[0.1; 100]).unwrap();
    let prov = AudioProvenance {
        pipeline_version: "p".into(),
        aec_version: None,
    };
    store
        .finalize_track_audio(wr, data, &t, prov, &w.clock, &mut w.rng)
        .unwrap()
}

#[test]
fn retention_sweeps_orphan_audio_older_than_the_grace_period() {
    let tmp = TempDir::new("orphans");
    let data = DataDir::new(tmp.path().join("Earshot"));
    data.create().unwrap();
    let mut store = Store::open(&data.db_path()).unwrap();
    let mut w = World::new();
    let kept = record(&data, &mut store, &mut w);
    let kept_path = data.resolve(&kept.path).unwrap();
    age(&kept_path, Duration::from_millis(30 * DAY_MS));

    let audio = data.root().join("audio");
    let mk = |dir: &str, name: &str, old: bool| {
        let d = audio.join(dir);
        std::fs::create_dir_all(&d).unwrap();
        let p = d.join(name);
        std::fs::write(&p, b"fLaC").unwrap();
        if old {
            age(&p, Duration::from_millis(ORPHAN_GRACE_MS + DAY_MS));
        }
        p
    };
    let old_flac = mk("gone-a", "mic.flac", true);
    let old_part = mk("gone-b", "loopback.flac.part", true);
    let new_part = mk("recording", "mic.flac.part", false);
    let other = mk("gone-c", "notes.txt", true);

    let r = run_retention(&mut store, &data, RetentionPolicy::KeepForever, now_ms()).unwrap();
    assert_eq!(r.orphans_removed, 2);
    assert_eq!((r.rows_removed, r.kept), (0, 1));
    assert!(!old_flac.exists() && !old_part.exists());
    assert!(
        !audio.join("gone-a").exists(),
        "empty orphan folder removed"
    );
    assert!(
        new_part.exists(),
        "a recent .part may be a recording in progress"
    );
    assert!(other.exists(), "only audio files are swept");
    assert!(kept_path.exists(), "a file with a row is never an orphan");
}

#[test]
fn data_dir_startup_cleanup_removes_only_stale_part_files() {
    let tmp = TempDir::new("parts");
    let data = DataDir::new(tmp.path().join("Earshot"));
    data.create().unwrap();
    let d = data.root().join("audio").join("m");
    std::fs::create_dir_all(&d).unwrap();
    let stale = d.join("mic.flac.part");
    let fresh = d.join("loopback.flac.part");
    let finished = d.join("x.flac");
    for p in [&stale, &fresh, &finished] {
        std::fs::write(p, b"x").unwrap();
    }
    age(&stale, Duration::from_millis(ORPHAN_GRACE_MS + DAY_MS));
    age(&finished, Duration::from_millis(ORPHAN_GRACE_MS + DAY_MS));
    assert_eq!(
        data.cleanup_stale_parts(now_ms(), ORPHAN_GRACE_MS).unwrap(),
        1
    );
    assert!(!stale.exists() && fresh.exists() && finished.exists());
}

#[test]
fn store_deleted_local_values_leave_no_trace_on_disk() {
    // T6.13d2: transient device-local records (live speaker embeddings, live transcripts)
    // are deleted after the final pass; their bytes must not survive in the database, its
    // free pages or the WAL, also with another connection open when the store closes.
    let dir = TempDir::new("local-erase");
    let db = dir.path().join("earshot.db");
    let mut store = Store::open(&db).unwrap();
    assert!(store.secure_delete().unwrap());
    let needle: Vec<u8> = (0..24u8).map(|i| 0xA0 ^ i.wrapping_mul(37)).collect();
    let mut value = vec![0x11; 300];
    value.extend_from_slice(&needle);
    store
        .set_local_value("live-speakers/x/mic", &value)
        .unwrap();
    store.set_local_value("keep", b"other").unwrap();
    // Fold it into the main file so it sits in a database page, not only in the WAL.
    store.checkpoint().unwrap();
    assert!(on_disk(&db, &needle), "sanity: the value is on disk");

    // Deleted inside a transaction, as the final pass does.
    let existed = store
        .transaction(|tx| tx.delete_local_value("live-speakers/x/mic"))
        .unwrap();
    assert!(existed);
    assert!(
        !store
            .transaction(|tx| tx.delete_local_value("live-speakers/x/mic"))
            .unwrap(),
        "a second delete finds nothing"
    );
    store.flush_erasures().unwrap();
    assert!(
        !on_disk(&db, &needle),
        "the deleted value must not survive in free pages or the WAL"
    );
    assert_eq!(store.local_value("live-speakers/x/mic").unwrap(), None);
    assert_eq!(store.local_value("keep").unwrap(), Some(b"other".to_vec()));
    assert!(matches!(
        store.delete_local_value("device_id"),
        Err(earshot_store::StoreError::Corrupt(_))
    ));

    // A second value deleted while another connection stays open across the close (so
    // SQLite's last-connection checkpoint cannot hide a missing one).
    let needle2: Vec<u8> = (0..24u8).map(|i| 0x5C ^ i.wrapping_mul(53)).collect();
    store
        .set_local_value("live-stretches/x/mic", &needle2)
        .unwrap();
    let reader = rusqlite::Connection::open(&db).unwrap();
    assert!(store.delete_local_value("live-stretches/x/mic").unwrap());
    drop(store);
    assert!(
        !on_disk(&db, &needle2),
        "the deleted value outlived the session"
    );
    drop(reader);
}

#[test]
fn store_forget_voiceprints_drops_the_identitys_speaker_centroids_from_every_meeting() {
    // T6.25: the speaker identity records hold speaker centroids (biometric). Forgetting a
    // person's voiceprints also drops, from every meeting's record, the entries of that
    // identity (rows linked to the person or to someone merged into it, or that the pass
    // linked to them), leaving no trace on disk with another connection open.
    use earshot_core::diar::SpeakerRole;
    use earshot_core::model::Speaker;
    use earshot_store::identities::{
        IdentityRecord, SpeakerIdentity, StoredIdentities, decode, encode, record_key,
    };
    let dir = TempDir::new("forget-identities");
    let db = dir.path().join("earshot.db");
    let mut w = World::new();
    let mut store = Store::open(&db).unwrap();
    assert!(store.secure_delete().unwrap());
    let model = EmbeddingModelId::new("test-model@1");
    let ada = Person::new("Ada", false, &mut w.stamp());
    let mut dup = Person::new("Ada (dup)", false, &mut w.stamp());
    dup.merged_into = Some(ada.id);
    let cy = Person::new("Cy", false, &mut w.stamp());
    for p in [&ada, &dup, &cy] {
        store.put_person(p.clone()).unwrap();
    }
    let centroid = |seed: f32| -> Embedding {
        Embedding::new(
            model.clone(),
            (0..16).map(|i| seed + i as f32 * 0.000_137).collect(),
        )
    };
    let needle = |e: &Embedding| serde_json::to_vec(&e.vector).unwrap();
    let entry = |s: &Speaker, e: Embedding, given: Option<EntityId>| SpeakerIdentity {
        speaker: s.id,
        track: TrackId::Reference,
        centroid: e,
        given_name: s.display_name.clone(),
        given_person: given,
        given_stamp: Some(s.meta.updated_at),
        user_edited: false,
        enrolled: false,
    };

    // Meeting 1: a row the user linked to Ada, one linked to Ada's merged duplicate, and an
    // unrelated one. Meeting 2: only a row the pass linked to Ada (since unlinked by hand).
    // Meeting 3: an unreadable record.
    let m1 = Meeting::new("", 1, 0, Template::General, &mut w.stamp());
    let m2 = Meeting::new("", 2, 0, Template::General, &mut w.stamp());
    let m3 = Meeting::new("", 3, 0, Template::General, &mut w.stamp());
    for m in [&m1, &m2, &m3] {
        store.put_meeting(m.clone()).unwrap();
    }
    let speaker = |w: &mut World, m: &Meeting, name: &str, person: Option<EntityId>| {
        Speaker::new(m.id, SpeakerRole::Remote, name, person, &mut w.stamp())
    };
    let a = speaker(&mut w, &m1, "Ada", Some(ada.id));
    let d = speaker(&mut w, &m1, "Ada D", Some(dup.id));
    let c = speaker(&mut w, &m1, "Cy", Some(cy.id));
    let gone = speaker(&mut w, &m2, "Speaker 1", None);
    for s in [&a, &d, &c, &gone] {
        store.put_speaker(s.clone()).unwrap();
    }
    let (ea, ed, ec, eg) = (
        centroid(0.111_1),
        centroid(0.222_2),
        centroid(0.333_3),
        centroid(0.444_4),
    );
    let (na, nd, nc, ng) = (needle(&ea), needle(&ed), needle(&ec), needle(&eg));
    let keep_c = entry(&c, ec, Some(cy.id));
    store
        .set_local_value(
            &record_key(m1.id),
            &encode(&IdentityRecord {
                speakers: vec![
                    entry(&a, ea, None),
                    entry(&d, ed, Some(dup.id)),
                    keep_c.clone(),
                ],
            }),
        )
        .unwrap();
    store
        .set_local_value(
            &record_key(m2.id),
            &encode(&IdentityRecord {
                speakers: vec![entry(&gone, eg, Some(ada.id))],
            }),
        )
        .unwrap();
    store
        .set_local_value(&record_key(m3.id), &[9, 0, 0, 0, b'{'])
        .unwrap();
    store.checkpoint().unwrap();
    for n in [&na, &nd, &nc, &ng] {
        assert!(on_disk(&db, n), "sanity: the centroid is on disk");
    }

    // Another connection stays open (another tool, a second window).
    let reader = rusqlite::Connection::open(&db).unwrap();
    store
        .forget_voiceprints(ada.id, &mut w.hlc, &w.clock)
        .unwrap();
    for n in [&na, &nd, &ng] {
        assert!(
            !on_disk(&db, n),
            "a forgotten centroid survived in a page or the WAL"
        );
    }
    assert!(on_disk(&db, &nc), "another person's centroid is kept");
    let read = |m: &Meeting| {
        store
            .local_value(&record_key(m.id))
            .unwrap()
            .map(|b| decode(&b))
    };
    assert_eq!(
        read(&m1),
        Some(StoredIdentities::Record(IdentityRecord {
            speakers: vec![keep_c]
        }))
    );
    assert_eq!(read(&m2), None, "a record left empty is deleted");
    assert_eq!(read(&m3), None, "an unreadable record is deleted");
    drop(store);
    for n in [&na, &nd, &ng] {
        assert!(
            !on_disk(&db, n),
            "a forgotten centroid outlived the session"
        );
    }
    drop(reader);
}
