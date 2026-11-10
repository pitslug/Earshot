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
use earshot_store::{AudioProvenance, DataDir, MeetingDeletion, PersonRemoval, Store, StoreError};
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
    // People and their voiceprints stay, even Ada's, who was only in this meeting (T9.1u).
    for p in [&ada, &bob, &me] {
        assert!(store.person(p.id).unwrap().unwrap().is_live(), "{p:?}");
        assert!(
            !store.voiceprints_of(p.id).unwrap()[0].meta.deleted,
            "{p:?}"
        );
    }
    assert!(on_disk(&db, &ada_print), "Ada's voiceprint is kept");

    for needle in [title.as_bytes(), said.as_bytes(), typed.as_bytes(), &local] {
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
fn store_remove_person_erases_the_name_and_voice_and_a_meeting_deletion_does_not() {
    // T9.1u: a person named a speaker in M1 and in M2, with a voiceprint and a centroid in M2's
    // identity record. Deleting M1 keeps all of it. Removing the person erases it, and M2's row
    // goes back to "Speaker N"; another person's row and a row the user renamed stay.
    use earshot_store::identities::{
        IdentityRecord, SpeakerIdentity, StoredIdentities, decode, encode, record_key,
    };
    let tmp = TempDir::new("remove-person");
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
    let name = "Dee Invented";
    let dee = enrolled(&mut store, &mut w, name, false, 0.017);
    let cy = enrolled(&mut store, &mut w, "Cy", false, 0.029);
    let dee_print = vector_bytes(&store, dee.id);
    speaker(&mut store, &mut w, m1.id, &dee);
    let linked = speaker(&mut store, &mut w, m2.id, &dee);
    // Someone merged away in M2 keeps no name either.
    let mut merged_away = speaker(&mut store, &mut w, m2.id, &dee);
    merged_away.meta.mark_deleted(&mut w.hlc, &w.clock);
    store.put_speaker(merged_away.clone()).unwrap();
    // The pass linked this row to Dee; the user then unlinked and renamed it.
    let mut guest = speaker(&mut store, &mut w, m2.id, &dee);
    guest.person_id = None;
    guest.display_name = "Speaker 1".into();
    guest.meta.touch(&mut w.hlc, &w.clock);
    store.put_speaker(guest.clone()).unwrap();
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
        ignored: false,
    };
    let dee_entry = SpeakerIdentity {
        speaker: linked.id,
        track: TrackId::Reference,
        centroid: dee_c,
        given_name: name.into(),
        given_person: Some(dee.id),
        given_stamp: None,
        user_edited: false,
        enrolled: false,
        ignored: false,
    };
    let both = IdentityRecord {
        speakers: vec![dee_entry, cy_entry.clone()],
    };
    store
        .set_local_value(&record_key(m2.id), &encode(&both))
        .unwrap();
    store.checkpoint().unwrap();
    assert!(on_disk(&db, &dee_n) && on_disk(&db, &cy_n), "sanity");

    // Another connection stays open across everything and the close.
    let reader = rusqlite::Connection::open(&db).unwrap();
    let m1_id = m1.id.to_string();
    store
        .delete_meeting(&data, m1.id, &mut w.hlc, &w.clock, &mut |k: &str| {
            k.contains(&m1_id)
        })
        .unwrap();
    assert!(store.person(dee.id).unwrap().unwrap().is_live());
    assert!(!store.voiceprints_of(dee.id).unwrap()[0].meta.deleted);
    assert_eq!(
        store
            .local_value(&record_key(m2.id))
            .unwrap()
            .map(|b| decode(&b)),
        Some(StoredIdentities::Record(both)),
        "a meeting deletion leaves the other meeting's record alone"
    );

    // Deleting M2 as well would keep Dee too; instead, the Speakers page removes them.
    let report = store.remove_person(dee.id, &mut w.hlc, &w.clock).unwrap();
    assert_eq!(
        report,
        PersonRemoval {
            persons_removed: 1,
            voiceprints_forgotten: 1,
            speakers_unnamed: 1,
        }
    );
    let gone = store.person(dee.id).unwrap().unwrap();
    assert!(gone.meta.deleted && gone.display_name.is_empty());
    assert!(store.person(cy.id).unwrap().unwrap().is_live());
    assert!(store.voiceprints_of(dee.id).unwrap()[0].meta.deleted);
    assert!(!store.voiceprints_of(cy.id).unwrap()[0].meta.deleted);

    let rows = store.speakers(m2.id).unwrap();
    let row = |id: EntityId| rows.iter().find(|s| s.id == id).unwrap();
    assert_eq!(
        row(linked.id).display_name,
        "Speaker 2",
        "Speaker 1 is taken"
    );
    assert_eq!(row(linked.id).person_id, None);
    assert!(!row(linked.id).meta.deleted);
    assert_eq!(row(merged_away.id).display_name, "");
    assert_eq!(row(merged_away.id).person_id, None);
    assert_eq!(row(guest.id).display_name, "Speaker 1");
    assert_eq!(row(cy_row.id).display_name, "Cy");
    assert_eq!(row(cy_row.id).person_id, Some(cy.id));
    assert_eq!(
        store
            .local_value(&record_key(m2.id))
            .unwrap()
            .map(|b| decode(&b)),
        Some(StoredIdentities::Record(IdentityRecord {
            speakers: vec![cy_entry]
        })),
    );
    assert!(on_disk(&db, &cy_n), "another person's centroid stays");
    for gone in [name.as_bytes(), &dee_print, &dee_n] {
        assert!(!on_disk(&db, gone), "removed data survived on disk");
    }
    drop(store);
    for gone in [name.as_bytes(), &dee_print, &dee_n] {
        assert!(!on_disk(&db, gone), "removed data outlived the session");
    }
    drop(reader);
}

#[test]
fn store_remove_person_takes_everyone_merged_into_them() {
    let tmp = TempDir::new("remove-merged");
    let data = DataDir::new(tmp.path().join("Earshot"));
    data.create().unwrap();
    let mut store = Store::open(&data.db_path()).unwrap();
    let mut w = World::new();
    let m = Meeting::new("One", 1, 0, Template::General, &mut w.stamp());
    store.put_meeting(m.clone()).unwrap();
    let keeper = enrolled(&mut store, &mut w, "Kay", false, 0.041);
    let mut dup = enrolled(&mut store, &mut w, "Kay Too", false, 0.053);
    let row = speaker(&mut store, &mut w, m.id, &dup);
    dup.merged_into = Some(keeper.id);
    dup.meta.touch(&mut w.hlc, &w.clock);
    store.put_person(dup.clone()).unwrap();

    let report = store
        .remove_person(keeper.id, &mut w.hlc, &w.clock)
        .unwrap();
    assert_eq!(
        report,
        PersonRemoval {
            persons_removed: 2,
            voiceprints_forgotten: 2,
            speakers_unnamed: 1,
        }
    );
    assert!(
        store
            .person(dup.id)
            .unwrap()
            .unwrap()
            .display_name
            .is_empty()
    );
    let rows = store.speakers(m.id).unwrap();
    assert_eq!(
        rows.iter().find(|s| s.id == row.id).unwrap().display_name,
        "Speaker 1"
    );
}
