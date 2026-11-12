//! Start-up recovery tests (temp data directories, fakes). Every name contains `recover`.

use std::sync::Arc;

use earshot_core::asr::CancelToken;
use earshot_core::model::{ClockDomain, Meeting, Stamp, TranscriptPass};
use earshot_core::prompt::Template;
use rand_chacha::ChaCha8Rng;
use rand_core::SeedableRng;

use super::*;
use crate::MeetingActivity;
use crate::host::HostPost;
use crate::host::tests::fake_post;
use crate::post::tests::levels;
use crate::testutil::{T0, TempDir, services};

/// A meeting whose process died while recording: rows as a live session writes them at its
/// start (no duration), and each track's writer dropped unfinished after `audio`.
fn crashed(svc: &Services, audio: &[(TrackId, &[f32])], seed: u64) -> EntityId {
    let clock = Arc::clone(&svc.clock);
    let mut rng = ChaCha8Rng::seed_from_u64(seed);
    let id = {
        let mut db = svc.store();
        let (store, hlc) = db.parts();
        let mut stamp = Stamp {
            clock: &*clock,
            hlc,
            rng: &mut rng,
        };
        let m = Meeting::new("", T0, 600, Template::General, &mut stamp);
        let tracks: Vec<Track> = audio
            .iter()
            .map(|(kind, _)| Track::new(m.id, *kind, None, ClockDomain::Replay, &mut stamp))
            .collect();
        let id = m.id;
        store.put_meeting(m).unwrap();
        for t in tracks {
            store.put_track(t).unwrap();
        }
        id
    };
    for (kind, samples) in audio {
        let mut w = svc.data.track_writer(id, *kind, 16_000).unwrap();
        w.write(samples).unwrap();
        drop(w); // killed: never finished
    }
    id
}

fn part(svc: &Services, meeting: EntityId, kind: TrackId) -> std::path::PathBuf {
    let mut p = svc
        .data
        .resolve(&earshot_store::DataDir::audio_rel(meeting, kind))
        .unwrap()
        .into_os_string();
    p.push(".part");
    p.into()
}

fn activity(svc: &Services, meeting: EntityId) -> MeetingActivity {
    svc.meeting_activity(meeting).unwrap().unwrap()
}

fn quiet(n: usize) -> Vec<f32> {
    (0..n).map(|i| 0.1 * (i as f32 * 0.05).sin()).collect()
}

#[test]
fn recover_gives_an_interrupted_meeting_its_audio_its_duration_and_one_notice() {
    let dir = TempDir::new("recover");
    let (svc, _) = services(&dir);
    // The mic stopped three blocks in, the loopback five: the shorter one is what was saved.
    let (mic, them) = (quiet(4096 * 3 + 500), quiet(4096 * 5 + 10));
    let m = crashed(
        &svc,
        &[(TrackId::Mic, &mic), (TrackId::Reference, &them)],
        1,
    );
    assert_eq!(activity(&svc, m), MeetingActivity::Interrupted);

    let report = recover_interrupted(&svc);
    let saved_ms = 4096 * 3 * 1000 / 16_000;
    assert_eq!(
        report.recovered,
        [RecoveredMeeting {
            meeting: m,
            saved_ms
        }]
    );
    assert!(
        report.failed.is_empty() && report.skipped.is_empty() && report.nothing_saved.is_empty()
    );
    assert_eq!(activity(&svc, m), MeetingActivity::Unprocessed);
    let meeting = svc.store().meeting(m).unwrap().unwrap();
    assert_eq!(meeting.duration_ms, Some(saved_ms));
    let rows = svc.store().audio_files(m).unwrap();
    assert_eq!(rows.len(), 2);
    let mic_row = rows.iter().find(|f| f.path.ends_with("mic.flac")).unwrap();
    assert_eq!(mic_row.frames, 4096 * 3);
    assert_eq!(
        mic_row.aec_version.as_deref(),
        Some(crate::live::AEC_VERSION)
    );
    assert!(!part(&svc, m, TrackId::Mic).exists());
    // No retention clock: the audio is kept whatever the policy.
    assert_eq!(
        earshot_store::retention_clock(&svc.store(), m).unwrap(),
        earshot_store::RetentionClock::NotStarted
    );

    // Shown once.
    assert_eq!(
        take_notices(&svc).unwrap(),
        [RecoveryNotice {
            meeting: m,
            saved_ms
        }]
    );
    assert!(take_notices(&svc).unwrap().is_empty());
    // A second start finds nothing to do.
    assert_eq!(recover_interrupted(&svc), RecoveryReport::default());
}

#[test]
fn recover_never_touches_a_meeting_a_job_of_this_process_holds() {
    let dir = TempDir::new("recover");
    let (svc, _) = services(&dir);
    let mic = quiet(4096 * 2);
    let m = crashed(&svc, &[(TrackId::Mic, &mic)], 2);
    let before = std::fs::read(part(&svc, m, TrackId::Mic)).unwrap();
    {
        let _recording = svc.lease_meeting_for(m, MeetingJob::Recording).unwrap();
        let report = recover_interrupted(&svc);
        assert_eq!(report.skipped, [m]);
        assert!(report.recovered.is_empty());
        assert_eq!(std::fs::read(part(&svc, m, TrackId::Mic)).unwrap(), before);
        assert_eq!(svc.store().meeting(m).unwrap().unwrap().duration_ms, None);
        assert!(take_notices(&svc).unwrap().is_empty());
    }
    // Once nothing holds it, the next start recovers it.
    assert_eq!(recover_interrupted(&svc).recovered.len(), 1);
}

#[test]
fn recover_leaves_a_meeting_with_no_whole_frame_interrupted_and_its_files_alone() {
    let dir = TempDir::new("recover");
    let (svc, _) = services(&dir);
    // Less than one block: the writer never wrote a frame, only the placeholder header.
    let mic = quiet(1000);
    let m = crashed(&svc, &[(TrackId::Mic, &mic)], 3);
    let before = std::fs::read(part(&svc, m, TrackId::Mic)).unwrap();
    assert_eq!(before.len(), 42);
    let report = recover_interrupted(&svc);
    assert_eq!(report.nothing_saved, [m]);
    assert!(report.recovered.is_empty());
    assert_eq!(activity(&svc, m), MeetingActivity::Interrupted);
    assert_eq!(std::fs::read(part(&svc, m, TrackId::Mic)).unwrap(), before);
    assert!(svc.store().audio_files(m).unwrap().is_empty());
    assert!(take_notices(&svc).unwrap().is_empty());
}

#[test]
fn recover_skips_finished_and_deleted_meetings() {
    let dir = TempDir::new("recover");
    let (svc, _) = services(&dir);
    let finished = crate::testutil::meeting(&svc, "Finished", 4);
    let mic = quiet(4096 * 2);
    let deleted = crashed(&svc, &[(TrackId::Mic, &mic)], 5);
    svc.delete_meeting(deleted).unwrap();
    let report = recover_interrupted(&svc);
    assert_eq!(report, RecoveryReport::default());
    assert_eq!(
        svc.store().meeting(finished).unwrap().unwrap().duration_ms,
        Some(60_000)
    );
}

#[test]
fn recover_drops_a_notice_whose_meeting_was_deleted() {
    let dir = TempDir::new("recover");
    let (svc, _) = services(&dir);
    let mic = quiet(4096 * 2);
    let m = crashed(&svc, &[(TrackId::Mic, &mic)], 6);
    assert_eq!(recover_interrupted(&svc).recovered.len(), 1);
    svc.delete_meeting(m).unwrap();
    assert!(take_notices(&svc).unwrap().is_empty());
    assert!(
        svc.store()
            .local_value(notice_key(m).as_str())
            .unwrap()
            .is_none()
    );
}

#[test]
fn recover_then_retranscribe_gives_a_final_transcript() {
    let dir = TempDir::new("recover");
    let (svc, _) = services(&dir);
    // The level scene the final-pass fakes know (host tests), cut by a crash after 19 s.
    let mic = levels(20_000, &[(0.1, 1_000, 6_000)], 70);
    let them = levels(20_000, &[(0.4, 7_000, 12_000), (0.7, 13_000, 18_000)], 80);
    let m = crashed(
        &svc,
        &[(TrackId::Mic, &mic), (TrackId::Reference, &them)],
        7,
    );
    assert_eq!(recover_interrupted(&svc).recovered.len(), 1);
    let cancel = CancelToken::new();
    let done = crate::edit::retranscribe_with(&svc, m, &cancel, fake_post, &mut |_| {}).unwrap();
    assert!(done.missing_tracks.is_empty(), "{:?}", done.missing_tracks);
    assert!(matches!(done.post, HostPost::Done(_)), "{:?}", done.post);
    let finals =
        earshot_core::model::live_segments(&**svc.store(), m, TranscriptPass::Final).unwrap();
    assert!(!finals.is_empty());
    assert_eq!(activity(&svc, m), MeetingActivity::Ready);
}
