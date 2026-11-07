//! Meeting deletion (T6.26): tombstoned and cleared rows, audio files removed before their
//! rows, export records and device-local records erased, voiceprints learnt only from the
//! meeting forgotten, and nothing of it left on disk while another connection is open.

mod common;

use std::time::Duration;

use common::TempDir;
use earshot_core::asr::{MeetingTime, Segment, Word};
use earshot_core::capture::TrackId;
use earshot_core::diar::{Embedding, EmbeddingModelId, SpeakerRole};
use earshot_core::identity::{DeviceId, EntityId, Hlc, ManualClock};
use earshot_core::model::{
    AudioFileRepo, ClockDomain, ExportRecord, ExportRepo, Meeting, MeetingRepo, Note, SegmentRow,
    Speaker, Stamp, Track, TranscriptPass, VaultPath, repo_contract,
};
use earshot_core::prompt::Template;
use earshot_core::voiceprint::{EnrolmentConfig, Person, VoiceprintRepo, enroll};
use earshot_store::{AudioProvenance, DataDir, MeetingDeletion, Store, StoreError};
use rand_chacha::ChaCha8Rng;
use rand_core::SeedableRng;

struct World {
    clock: ManualClock,
    hlc: Hlc,
    rng: ChaCha8Rng,
}

impl World {
    fn new() -> Self {
        World {
            clock: ManualClock::new(1_800_000_000_000),
            hlc: Hlc::new(DeviceId::from_bytes([6; 16])),
            rng: ChaCha8Rng::seed_from_u64(26),
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
fn store_meeting_deletion_contract() {
    repo_contract::meeting_deletion(|| Store::open_in_memory().unwrap());
}

fn track_with_audio(
    data: &DataDir,
    store: &mut Store,
    w: &mut World,
    meeting: EntityId,
    kind: TrackId,
) -> Track {
    let t = Track::new(
        meeting,
        kind,
        Some("USB".into()),
        ClockDomain::Qpc,
        &mut w.stamp(),
    );
    store.put_track(t.clone()).unwrap();
    let mut wr = data.track_writer(meeting, kind, 16_000).unwrap();
    wr.write(&[0.1; 160]).unwrap();
    let prov = AudioProvenance {
        pipeline_version: "p".into(),
        aec_version: None,
    };
    store
        .finalize_track_audio(wr, data, &t, prov, &w.clock, &mut w.rng)
        .unwrap();
    t
}

fn enrolled(store: &mut Store, w: &mut World, name: &str, is_me: bool, seed: f32) -> Person {
    let p = Person::new(name, is_me, &mut w.stamp());
    store.put_person(p.clone()).unwrap();
    let v: Vec<f32> = (0..64).map(|i| seed + i as f32 * 0.0131).collect();
    enroll(
        store,
        p.id,
        &[Embedding::new(EmbeddingModelId::new("test-model@1"), v)],
        Duration::from_secs(12),
        &EnrolmentConfig::default(),
        &mut w.stamp(),
    )
    .unwrap();
    p
}

fn speaker(store: &mut Store, w: &mut World, meeting: EntityId, person: &Person) -> Speaker {
    let role = if person.is_me {
        SpeakerRole::Me
    } else {
        SpeakerRole::Remote
    };
    let s = Speaker::new(
        meeting,
        role,
        &person.display_name,
        Some(person.id),
        &mut w.stamp(),
    );
    store.put_speaker(s.clone()).unwrap();
    s
}

fn vector_bytes(store: &Store, person: EntityId) -> Vec<u8> {
    let sum = store.voiceprints_of(person).unwrap()[0].sum.clone();
    sum[10..16].iter().flat_map(|x| x.to_le_bytes()).collect()
}

#[test]
fn store_delete_meeting_leaves_no_row_file_or_local_value_of_it() {
    let tmp = TempDir::new("delete-meeting");
    let data = DataDir::new(tmp.path().join("Earshot"));
    data.create().unwrap();
    let db = data.db_path();
    let mut store = Store::open(&db).unwrap();
    let mut w = World::new();

    let title = "Title-needle-7f3a9c";
    let m = Meeting::new(title, 1, 0, Template::General, &mut w.stamp());
    let keep = Meeting::new("Kept meeting", 2, 0, Template::General, &mut w.stamp());
    store.put_meeting(m.clone()).unwrap();
    store.put_meeting(keep.clone()).unwrap();
    let mic = track_with_audio(&data, &mut store, &mut w, m.id, TrackId::Mic);
    track_with_audio(&data, &mut store, &mut w, m.id, TrackId::Reference);
    let kept_track = track_with_audio(&data, &mut store, &mut w, keep.id, TrackId::Mic);
    // A crashed recording's leftover in the meeting's folder (fresh, so retention's grace
    // period would keep it; the deletion removes it).
    let meeting_dir = data.root().join("audio").join(m.id.to_string());
    std::fs::write(meeting_dir.join("mic.flac.part"), b"fLaC").unwrap();

    // Persons: one heard only in this meeting, one also heard in the kept meeting, the user.
    let ada = enrolled(&mut store, &mut w, "Ada", false, 0.011);
    let bob = enrolled(&mut store, &mut w, "Bob", false, 0.023);
    let me = enrolled(&mut store, &mut w, "Me", true, 0.037);
    let ada_print = vector_bytes(&store, ada.id);
    let sp = speaker(&mut store, &mut w, m.id, &ada);
    speaker(&mut store, &mut w, m.id, &bob);
    speaker(&mut store, &mut w, m.id, &me);
    speaker(&mut store, &mut w, keep.id, &bob);

    let said = "Transcript-needle-41be02";
    let mut seg = Segment::from_words(
        EntityId::generate(&w.clock, &mut w.rng),
        mic.id,
        vec![Word::new(said, 0, 400, 0.9)],
        MeetingTime::ZERO,
    );
    seg.speaker_id = Some(sp.id);
    let row = SegmentRow::new(m.id, TranscriptPass::Live, seg, &mut w.hlc, &w.clock);
    store.put_segment(row).unwrap();
    let typed = "Note-needle-90d1e5";
    store
        .put_note(Note::new(m.id, MeetingTime::ZERO, typed, &mut w.stamp()))
        .unwrap();
    let device = DeviceId::from_bytes([9; 16]);
    for meeting in [m.id, keep.id] {
        store
            .put_export_record(ExportRecord {
                id: EntityId::generate(&w.clock, &mut w.rng),
                meeting_id: meeting,
                device_id: device,
                path: VaultPath::new(&format!("Meetings/{meeting}.md")).unwrap(),
                exported_unix_ms: 3,
            })
            .unwrap();
    }

    let local: Vec<u8> = (0..24u8).map(|i| 0x3C ^ i.wrapping_mul(29)).collect();
    let mine = [
        format!("live-stretches/{}/mic", m.id),
        format!("pass-backend/final/{}/-", m.id),
    ];
    let theirs = format!("live-stretches/{}/mic", keep.id);
    for k in &mine {
        store.set_local_value(k, &local).unwrap();
    }
    store.set_local_value(&theirs, b"kept").unwrap();
    store
        .set_local_value("model-check", b"device-wide")
        .unwrap();
    store.checkpoint().unwrap();
    for needle in [
        title.as_bytes(),
        said.as_bytes(),
        typed.as_bytes(),
        &local,
        &ada_print,
    ] {
        assert!(on_disk(&db, needle), "sanity: on disk before the deletion");
    }

    // Another connection stays open throughout, so SQLite's last-connection checkpoint
    // cannot hide a missing one.
    let reader = rusqlite::Connection::open(&db).unwrap();
    let id = m.id.to_string();
    let mut asked = Vec::new();
    let report = store
        .delete_meeting(&data, m.id, &mut w.hlc, &w.clock, &mut |k: &str| {
            asked.push(k.to_owned());
            k.contains(&id)
        })
        .unwrap();
    assert_eq!(
        report,
        MeetingDeletion {
            rows_tombstoned: 1 + 2 + 3 + 1 + 1,
            audio_files_deleted: 2,
            audio_files_missing: 0,
            stray_audio_removed: 1,
            export_records_removed: 1,
            local_values_erased: 2,
            voiceprints_forgotten: 1,
        }
    );
    assert!(
        !asked.iter().any(|k| k == "device_id"),
        "the reserved key is never offered: {asked:?}"
    );

    // Files first, rows after; the folder goes when empty.
    assert!(!meeting_dir.exists(), "audio folder of the meeting removed");
    assert!(store.audio_files(m.id).unwrap().is_empty());
    assert_eq!(store.audio_files(keep.id).unwrap().len(), 1);
    assert!(
        data.resolve(&DataDir::audio_rel(keep.id, kept_track.kind))
            .unwrap()
            .exists()
    );
    // Rows: tombstones only (cleared content is the contract's business).
    assert!(store.meeting(m.id).unwrap().unwrap().meta.deleted);
    assert!(store.tracks(m.id).unwrap().iter().all(|t| t.meta.deleted));
    assert!(store.speakers(m.id).unwrap().iter().all(|s| s.meta.deleted));
    assert!(store.segments(m.id).unwrap().iter().all(|s| s.meta.deleted));
    assert!(store.notes(m.id).unwrap().iter().all(|n| n.meta.deleted));
    assert!(!store.meeting(keep.id).unwrap().unwrap().meta.deleted);
    assert_eq!(store.export_record(m.id, device).unwrap(), None);
    assert!(store.export_record(keep.id, device).unwrap().is_some());
    // Local values of the meeting erased, others kept.
    for k in &mine {
        assert_eq!(store.local_value(k).unwrap(), None, "{k}");
    }
    assert_eq!(
        store.local_value(&theirs).unwrap().as_deref(),
        Some(&b"kept"[..])
    );
    assert!(store.local_value("model-check").unwrap().is_some());
    // Persons stay; Ada's voiceprint (only this meeting) goes; Bob's (also the kept meeting)
    // and the user's stay.
    for p in [&ada, &bob, &me] {
        assert!(store.person(p.id).unwrap().unwrap().is_live(), "{p:?}");
    }
    assert!(store.voiceprints_of(ada.id).unwrap()[0].meta.deleted);
    assert!(!store.voiceprints_of(bob.id).unwrap()[0].meta.deleted);
    assert!(!store.voiceprints_of(me.id).unwrap()[0].meta.deleted);

    for needle in [
        title.as_bytes(),
        said.as_bytes(),
        typed.as_bytes(),
        &local,
        &ada_print,
    ] {
        assert!(
            !on_disk(&db, needle),
            "deleted content survived in the database, its free pages or the WAL"
        );
    }

    // Idempotent: a second run finds nothing to do.
    let again = store
        .delete_meeting(&data, m.id, &mut w.hlc, &w.clock, &mut |k: &str| {
            k.contains(&id)
        })
        .unwrap();
    assert_eq!(again, MeetingDeletion::default());
    let ghost = EntityId::generate(&w.clock, &mut w.rng);
    assert!(matches!(
        store.delete_meeting(&data, ghost, &mut w.hlc, &w.clock, &mut |_: &str| true),
        Err(StoreError::Repo(_))
    ));
    drop(store);
    assert!(!on_disk(&db, said.as_bytes()));
    drop(reader);
}

#[test]
fn store_delete_meeting_tolerates_audio_already_gone() {
    let tmp = TempDir::new("delete-missing-audio");
    let data = DataDir::new(tmp.path().join("Earshot"));
    data.create().unwrap();
    let mut store = Store::open(&data.db_path()).unwrap();
    let mut w = World::new();
    let m = Meeting::new("", 1, 0, Template::General, &mut w.stamp());
    store.put_meeting(m.clone()).unwrap();
    let t = track_with_audio(&data, &mut store, &mut w, m.id, TrackId::Mic);
    std::fs::remove_file(data.resolve(&DataDir::audio_rel(m.id, t.kind)).unwrap()).unwrap();
    let r = store
        .delete_meeting(&data, m.id, &mut w.hlc, &w.clock, &mut |_: &str| false)
        .unwrap();
    assert_eq!((r.audio_files_deleted, r.audio_files_missing), (0, 1));
    assert!(store.audio_files(m.id).unwrap().is_empty());
    assert!(!data.root().join("audio").join(m.id.to_string()).exists());
}

#[test]
fn store_delete_meeting_forgets_the_speaker_centroids_of_the_voiceprints_it_forgets() {
    // T6.25: a person created in M1 (with a voiceprint) named a speaker in M2's final pass,
    // and the user then unlinked that M2 row by hand. Deleting M1 forgets the person's
    // voiceprint (no live speaker refers to them any more), and with it the centroid M2's
    // speaker identity record tagged with them; another person's entry stays.
    use earshot_store::identities::{
        IdentityRecord, SpeakerIdentity, StoredIdentities, decode, encode, record_key,
    };
    let tmp = TempDir::new("delete-identities");
    let data = DataDir::new(tmp.path().join("Earshot"));
    data.create().unwrap();
    let db = data.db_path();
    let mut store = Store::open(&db).unwrap();
    assert!(store.secure_delete().unwrap());
    let mut w = World::new();

    let m1 = Meeting::new("One", 1, 0, Template::General, &mut w.stamp());
    let m2 = Meeting::new("Two", 2, 0, Template::General, &mut w.stamp());
    store.put_meeting(m1.clone()).unwrap();
    store.put_meeting(m2.clone()).unwrap();
    let dee = enrolled(&mut store, &mut w, "Speaker 1", false, 0.017);
    let cy = enrolled(&mut store, &mut w, "Cy", false, 0.029);
    speaker(&mut store, &mut w, m1.id, &dee);
    // M2: the pass linked a row to Dee; the user then unlinked and renamed it. Cy's row
    // stays linked.
    let mut named = speaker(&mut store, &mut w, m2.id, &dee);
    named.person_id = None;
    named.display_name = "Guest".into();
    named.meta.touch(&mut w.hlc, &w.clock);
    store.put_speaker(named.clone()).unwrap();
    let cy_row = speaker(&mut store, &mut w, m2.id, &cy);

    let model = EmbeddingModelId::new("test-model@1");
    let centroid = |seed: f32| -> Embedding {
        Embedding::new(
            model.clone(),
            (0..16).map(|i| seed + i as f32 * 0.000_173).collect(),
        )
    };
    let (dee_c, cy_c) = (centroid(0.515_1), centroid(0.626_2));
    let needle = |e: &Embedding| serde_json::to_vec(&e.vector).unwrap();
    let (dee_n, cy_n) = (needle(&dee_c), needle(&cy_c));
    let cy_entry = SpeakerIdentity {
        speaker: cy_row.id,
        track: TrackId::Reference,
        centroid: cy_c,
        given_name: "Cy".into(),
        given_person: Some(cy.id),
        given_stamp: None,
        user_edited: false,
        enrolled: false,
    };
    store
        .set_local_value(
            &record_key(m2.id),
            &encode(&IdentityRecord {
                speakers: vec![
                    SpeakerIdentity {
                        speaker: named.id,
                        track: TrackId::Reference,
                        centroid: dee_c,
                        given_name: "Speaker 1".into(),
                        given_person: Some(dee.id),
                        given_stamp: None,
                        user_edited: false,
                        enrolled: false,
                    },
                    cy_entry.clone(),
                ],
            }),
        )
        .unwrap();
    store.checkpoint().unwrap();
    assert!(on_disk(&db, &dee_n) && on_disk(&db, &cy_n), "sanity");

    // Another connection stays open across the deletion and the close.
    let reader = rusqlite::Connection::open(&db).unwrap();
    let m1_id = m1.id.to_string();
    let report = store
        .delete_meeting(&data, m1.id, &mut w.hlc, &w.clock, &mut |k: &str| {
            k.contains(&m1_id)
        })
        .unwrap();
    assert_eq!(report.voiceprints_forgotten, 1, "{report:?}");
    assert!(store.voiceprints_of(dee.id).unwrap()[0].meta.deleted);
    assert!(!store.voiceprints_of(cy.id).unwrap()[0].meta.deleted);
    assert_eq!(
        store
            .local_value(&record_key(m2.id))
            .unwrap()
            .map(|b| decode(&b)),
        Some(StoredIdentities::Record(IdentityRecord {
            speakers: vec![cy_entry]
        })),
        "M2 keeps no centroid tagged with the forgotten person"
    );
    assert!(
        !on_disk(&db, &dee_n),
        "the forgotten centroid survived on disk"
    );
    assert!(on_disk(&db, &cy_n));
    drop(store);
    assert!(
        !on_disk(&db, &dee_n),
        "the forgotten centroid outlived the session"
    );
    drop(reader);
}
