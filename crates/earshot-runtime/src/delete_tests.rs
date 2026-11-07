//! Meeting deletion tests (temp data directory and vault). Every name contains `delete`.

use earshot_core::asr::FakeTranscriber;
use earshot_core::asr::Transcriber;
use earshot_core::capture::TrackId;
use earshot_core::export::ExportOptions;
use earshot_core::identity::DeviceId;
use earshot_core::model::{AudioFileRepo, ExportRepo, TranscriptPass};
use earshot_store::{AudioProvenance, DataDir};
use rand_chacha::ChaCha8Rng;
use rand_core::SeedableRng;

use super::*;
use crate::local::{LocalKey, MeetingRecord};
use crate::testutil::{TempDir, meeting, services};

/// A key of `record` for `meeting`, built by the production key builder of that record.
/// Exhaustive: a new kind of meeting record must be added here, and is then checked below.
fn production_key(record: MeetingRecord, meeting: EntityId, device: DeviceId) -> LocalKey {
    match record {
        MeetingRecord::LiveStretches => crate::stretches::record_key(meeting, TrackId::Mic),
        MeetingRecord::LiveSpeakers => crate::speakers::record_key(meeting, TrackId::Reference),
        MeetingRecord::EchoLocks => crate::echo::locks_key(meeting),
        MeetingRecord::EchoReport => crate::echo::report_key(meeting, TranscriptPass::Final),
        MeetingRecord::PassBackend => {
            crate::backends::pass_key(meeting, TranscriptPass::Live, Some(TrackId::Mic))
        }
        MeetingRecord::ExportFingerprint => crate::export::fingerprint_key(device, meeting),
        MeetingRecord::SpeakerIdentities => crate::identities::record_key(meeting),
        MeetingRecord::LiveSummaries => crate::live_summaries::record_key(meeting),
        MeetingRecord::SummaryHandovers => crate::claude_code::record_key(meeting),
        MeetingRecord::AudioRetention => crate::retention::clock_key(meeting),
    }
}

fn needle(seed: u8) -> Vec<u8> {
    (0..24u8)
        .map(|i| seed ^ i.wrapping_mul(41) ^ 0x5A)
        .collect()
}

fn on_disk(db: &std::path::Path, needle: &[u8]) -> bool {
    ["", "-wal", "-journal"].iter().any(|suffix| {
        std::fs::read(format!("{}{suffix}", db.display()))
            .is_ok_and(|b| b.windows(needle.len()).any(|w| w == needle))
    })
}

/// Writes a record of every kind for `meeting` (each value carries `needle`).
fn records_of(svc: &Services, meeting: EntityId, needle: &[u8]) -> Vec<LocalKey> {
    let mut db = svc.store();
    let device = db.device();
    MeetingRecord::ALL
        .iter()
        .map(|&r| {
            let key = production_key(r, meeting, device);
            let mut value = b"record:".to_vec();
            value.extend_from_slice(needle);
            crate::local::set(&mut db, &key, &value).unwrap();
            key
        })
        .collect()
}

fn mic_audio(svc: &Services, meeting: EntityId) -> std::path::PathBuf {
    let mut db = svc.store();
    let track = db
        .tracks(meeting)
        .unwrap()
        .into_iter()
        .find(|t| t.kind == TrackId::Mic)
        .unwrap();
    let mut w = svc
        .data
        .track_writer(meeting, TrackId::Mic, 16_000)
        .unwrap();
    w.write(&[0.2; 320]).unwrap();
    let prov = AudioProvenance {
        pipeline_version: "p".into(),
        aec_version: None,
    };
    let clock = Arc::clone(&svc.clock);
    db.finalize_track_audio(
        w,
        &svc.data,
        &track,
        prov,
        &*clock,
        // Distinct seeds per meeting: ids from one seed and clock collide.
        &mut ChaCha8Rng::seed_from_u64(u64::from(meeting.as_uuid().as_bytes()[15])),
    )
    .unwrap();
    svc.data
        .resolve(&DataDir::audio_rel(meeting, TrackId::Mic))
        .unwrap()
}

#[test]
fn delete_meeting_leaves_no_row_file_or_local_value_of_it() {
    let dir = TempDir::new("delete");
    let (svc, _clock) = services(&dir);
    let vault = dir.0.join("vault");
    std::fs::create_dir_all(vault.join(".obsidian")).unwrap();
    // The title is the needle for row content (both meetings share their transcript text).
    let title = "Title-needle-c41d7e";
    let m = meeting(&svc, title, 1);
    let keep = meeting(&svc, "Keep me", 2);
    let audio = mic_audio(&svc, m);
    let kept_audio = mic_audio(&svc, keep);

    // The production writers run too (their keys are the same builders as below).
    crate::export::export_meeting(
        &svc,
        &vault,
        m,
        &ExportOptions::default(),
        &mut ChaCha8Rng::seed_from_u64(3),
    )
    .unwrap();
    crate::echo::record_echo_locks(&mut svc.store(), m, &[]).unwrap();
    crate::backends::record_pass_backend(
        &mut svc.store(),
        m,
        TranscriptPass::Final,
        None,
        &FakeTranscriber::scripted([]).backend_info(),
    )
    .unwrap();
    let mine = records_of(&svc, m, &needle(1));
    let theirs = records_of(&svc, keep, &needle(2));
    // A meeting whose session was withdrawn (tombstoned) without its records ever being
    // cleaned: they are swept by this deletion.
    let withdrawn = meeting(&svc, "Withdrawn", 3);
    let stale = records_of(&svc, withdrawn, &needle(3));
    {
        let clock = Arc::clone(&svc.clock);
        let mut db = svc.store();
        let mut row = db.meeting(withdrawn).unwrap().unwrap();
        let (store, hlc) = db.parts();
        row.meta.mark_deleted(hlc, &*clock);
        store.put_meeting(row).unwrap();
    }
    let device = svc.store().device();
    let note_count = || {
        std::fs::read_dir(vault.join("Meetings"))
            .unwrap()
            .flatten()
            .count()
    };
    assert_eq!(note_count(), 1);
    svc.store().checkpoint().unwrap();
    let db_path = svc.data.db_path();
    assert!(on_disk(&db_path, &needle(1)), "sanity");
    assert!(on_disk(&db_path, title.as_bytes()), "sanity");

    let reader = earshot_store::Store::open(&db_path).unwrap();
    let report = svc.delete_meeting(m).unwrap();
    assert_eq!(report.audio_files_deleted, 1);
    assert_eq!(report.export_records_removed, 1);
    assert_eq!(
        report.local_values_erased,
        mine.len() + 1 + stale.len(),
        "{report:?}"
    );

    let db = svc.store();
    // No local value of the deleted (or withdrawn) meeting remains; others stay.
    let left: Vec<String> = db
        .local_keys()
        .unwrap()
        .into_iter()
        .filter(|k| matches!(crate::local::meeting_of(k), Some(x) if x == m || x == withdrawn))
        .collect();
    assert!(left.is_empty(), "{left:?}");
    for k in &theirs {
        assert!(db.local_value(k.as_str()).unwrap().is_some(), "{k:?}");
    }
    // No live row, no file, no export record; the vault note is the user's and stays.
    assert!(db.meeting(m).unwrap().unwrap().meta.deleted);
    assert!(db.tracks(m).unwrap().iter().all(|t| t.meta.deleted));
    assert!(db.speakers(m).unwrap().iter().all(|s| s.meta.deleted));
    assert!(db.segments(m).unwrap().iter().all(|s| s.meta.deleted));
    assert!(db.notes(m).unwrap().iter().all(|n| n.meta.deleted));
    assert!(db.audio_files(m).unwrap().is_empty());
    assert!(!audio.exists());
    assert!(kept_audio.exists());
    assert_eq!(db.export_record(m, device).unwrap(), None);
    assert!(!db.meeting(keep).unwrap().unwrap().meta.deleted);
    drop(db);
    assert_eq!(note_count(), 1, "the exported note is left alone");
    // Erased on disk too, with another connection open.
    for n in [1, 3] {
        assert!(
            !on_disk(&db_path, &needle(n)),
            "record bytes of meeting {n}"
        );
    }
    assert!(
        !on_disk(&db_path, title.as_bytes()),
        "the deleted meeting's title survived"
    );
    assert!(
        on_disk(&db_path, &needle(2)),
        "the kept meeting's records stay"
    );
    drop(svc);
    drop(reader);
}

#[test]
fn delete_meeting_is_refused_while_a_job_holds_it() {
    let dir = TempDir::new("delete-busy");
    let (svc, _clock) = services(&dir);
    let m = meeting(&svc, "Busy", 1);
    let lease = svc.lease_meeting(m).unwrap();
    let second = svc.lease_meeting(m).unwrap();
    let err = svc.delete_meeting(m).unwrap_err();
    assert!(
        matches!(err, RuntimeError::MeetingBusy(x) if x == m),
        "{err:?}"
    );
    assert!(err.is_retryable());
    drop(lease);
    assert!(
        matches!(svc.delete_meeting(m), Err(RuntimeError::MeetingBusy(_))),
        "every lease counts"
    );
    drop(second);
    assert!(!svc.store().meeting(m).unwrap().unwrap().meta.deleted);

    // While a deletion runs, no job can start on the meeting, and another deletion waits.
    let claim = svc.begin_deletion(m).unwrap();
    assert!(matches!(
        svc.lease_meeting(m),
        Err(RuntimeError::MeetingBusy(_))
    ));
    assert!(matches!(
        svc.delete_meeting(m),
        Err(RuntimeError::MeetingBusy(_))
    ));
    let other = meeting(&svc, "Other", 2);
    drop(svc.lease_meeting(other).unwrap());
    drop(claim);
    svc.delete_meeting(m).unwrap();
    assert!(svc.store().meeting(m).unwrap().unwrap().meta.deleted);
    // Deleting again is a clean no-op; an unknown meeting is an error.
    assert_eq!(svc.delete_meeting(m).unwrap(), Default::default());
    let ghost = EntityId::from_bytes([7; 16]);
    assert!(matches!(
        svc.delete_meeting(ghost),
        Err(RuntimeError::Store(earshot_store::StoreError::Repo(_)))
    ));
}

#[test]
fn delete_meeting_keeps_records_of_a_deleted_meeting_a_job_still_holds() {
    let dir = TempDir::new("delete-held");
    let (svc, _clock) = services(&dir);
    let m = meeting(&svc, "Delete me", 1);
    let held = meeting(&svc, "Held", 2);
    let keys = records_of(&svc, held, &needle(4));
    {
        let clock = Arc::clone(&svc.clock);
        let mut db = svc.store();
        let mut row = db.meeting(held).unwrap().unwrap();
        let (store, hlc) = db.parts();
        row.meta.mark_deleted(hlc, &*clock);
        store.put_meeting(row).unwrap();
    }
    let lease = svc.lease_meeting(held).unwrap();
    svc.delete_meeting(m).unwrap();
    for k in &keys {
        assert!(
            svc.store().local_value(k.as_str()).unwrap().is_some(),
            "{k:?}"
        );
    }
    drop(lease);
    // The next deletion sweeps them.
    svc.delete_meeting(m).unwrap();
    for k in &keys {
        assert_eq!(svc.store().local_value(k.as_str()).unwrap(), None, "{k:?}");
    }
}

#[test]
fn delete_meeting_export_skips_a_meeting_being_deleted() {
    let dir = TempDir::new("delete-export");
    let (svc, _clock) = services(&dir);
    let vault = dir.0.join("vault");
    std::fs::create_dir_all(vault.join(".obsidian")).unwrap();
    let m = meeting(&svc, "Being deleted", 1);
    let claim = svc.begin_deletion(m).unwrap();
    let err = crate::export::export_meeting(
        &svc,
        &vault,
        m,
        &ExportOptions::default(),
        &mut ChaCha8Rng::seed_from_u64(1),
    )
    .unwrap_err();
    assert!(matches!(err, RuntimeError::MeetingBusy(_)), "{err:?}");
    let all = crate::export::export_meetings(
        &svc,
        &vault,
        crate::export::ExportSelection::All,
        &ExportOptions::default(),
        &mut ChaCha8Rng::seed_from_u64(1),
    )
    .unwrap();
    assert!(all.is_empty(), "{all:?}");
    drop(claim);
}
