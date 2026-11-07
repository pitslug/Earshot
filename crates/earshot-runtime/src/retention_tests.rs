//! Audio retention tests (T7.4k). Every name contains `retention`.

use earshot_core::asr::CancelToken;
use earshot_core::capture::TrackId;
use earshot_core::identity::Clock;
use earshot_core::model::{MeetingRepo, TranscriptPass, live_segments};
use earshot_core::summary::SummaryV1;
use earshot_llm_replay::mock::{MockResponse, MockServer};
use earshot_store::{DAY_MS, DataDir, HOUR_MS, RetentionClock, retention_clock};
use rand_chacha::ChaCha8Rng;
use rand_core::SeedableRng;

use super::*;
use crate::backends::{Accelerator, ModelDir};
use crate::host::tests::{fake_post, recorded};
use crate::host::{AfterMeeting, HostPost, SkipReason, SummaryResult, after_meeting_with};
use crate::summary::{SummaryConfig, SummaryService};
use crate::testutil::{TempDir, services};
use crate::view::MeetingView;

fn clock_of(svc: &Services, meeting: EntityId) -> RetentionClock {
    retention_clock(&svc.store(), meeting).unwrap()
}

fn audio_paths(svc: &Services, meeting: EntityId) -> Vec<std::path::PathBuf> {
    [TrackId::Mic, TrackId::Reference]
        .into_iter()
        .map(|t| svc.data.resolve(&DataDir::audio_rel(meeting, t)).unwrap())
        .collect()
}

fn summary(title: &str) -> SummaryV1 {
    SummaryV1 {
        title: title.into(),
        tldr: "The team agreed to ship on Friday.".into(),
        enhanced_notes: vec![],
        decisions: vec![],
        action_items: vec![],
        open_questions: vec![],
        topics: vec![],
    }
}

fn store_a_summary(svc: &Services, meeting: EntityId, seed: u64) {
    let view = MeetingView::load(&**svc.store(), meeting).unwrap().unwrap();
    crate::summary::store_summary(
        svc,
        &view,
        "test-model".into(),
        &summary("Release"),
        &mut ChaCha8Rng::seed_from_u64(seed),
    )
    .unwrap();
}

/// The cleanup after `outcome`'s meeting with the level fakes and the given summary step.
fn after(
    svc: &Services,
    outcome: &crate::live::LiveOutcome,
    service: Option<&SummaryService>,
    no_summary: SkipReason,
) -> crate::host::HostOutcome {
    let models = ModelDir::new(svc.data.root().join("models"));
    let (a, b) = (CancelToken::new(), CancelToken::new());
    let run = AfterMeeting {
        models: &models,
        model: "base.en",
        order: &[Accelerator::Cpu],
        post: true,
        whole_pass: false,
        summary: service,
        no_summary,
        post_cancel: &a,
        summary_cancel: &b,
    };
    after_meeting_with(svc, outcome, &run, fake_post, &mut |_| {})
}

#[test]
fn retention_choices_are_the_list_with_names_labels_and_policies() {
    let labels: Vec<&str> = AudioRetention::ALL.iter().map(|r| r.label()).collect();
    assert_eq!(
        labels,
        [
            "1 hour", "4 hours", "12 hours", "1 day", "3 days", "7 days", "14 days", "28 days",
            "3 months", "6 months", "1 year", "Forever"
        ]
    );
    let ms: Vec<Option<u64>> = AudioRetention::ALL
        .iter()
        .map(|r| match r.policy() {
            RetentionPolicy::KeepFor { ms } => Some(ms),
            RetentionPolicy::KeepForever => None,
        })
        .collect();
    assert_eq!(
        ms,
        [
            Some(HOUR_MS),
            Some(4 * HOUR_MS),
            Some(12 * HOUR_MS),
            Some(DAY_MS),
            Some(3 * DAY_MS),
            Some(7 * DAY_MS),
            Some(14 * DAY_MS),
            Some(28 * DAY_MS),
            Some(90 * DAY_MS),
            Some(180 * DAY_MS),
            Some(365 * DAY_MS),
            None
        ]
    );
    for r in AudioRetention::ALL {
        assert_eq!(AudioRetention::parse(r.as_str()), Some(r));
    }
    assert_eq!(AudioRetention::default(), AudioRetention::SevenDays);
    assert_eq!(
        AudioRetention::default().policy(),
        RetentionPolicy::default()
    );
    assert_eq!(AudioRetention::parse("7 days"), None);
}

#[test]
fn retention_setting_defaults_to_seven_days_and_reports_an_unknown_value() {
    let dir = TempDir::new("retention");
    let (svc, _) = services(&dir);
    assert_eq!(
        audio_retention(&svc.store()).unwrap(),
        (AudioRetention::SevenDays, None)
    );
    set_audio_retention(&mut svc.store(), AudioRetention::OneHour).unwrap();
    assert_eq!(
        audio_retention(&svc.store()).unwrap(),
        (AudioRetention::OneHour, None)
    );
    assert_eq!(
        svc.store()
            .local_value(AUDIO_RETENTION_KEY)
            .unwrap()
            .unwrap(),
        b"1h"
    );
    svc.store()
        .set_local_value(AUDIO_RETENTION_KEY, b"2 weeks")
        .unwrap();
    let (choice, unknown) = audio_retention(&svc.store()).unwrap();
    assert_eq!(choice, AudioRetention::SevenDays);
    let unknown = unknown.unwrap();
    assert_eq!(
        (unknown.key, unknown.stored.as_str()),
        (AUDIO_RETENTION_KEY, "2 weeks")
    );
    // A run with an unknown stored value applies the default and says so.
    let r = run(&svc).unwrap();
    assert_eq!(r.choice, AudioRetention::SevenDays);
    assert_eq!(r.unknown_setting, Some(unknown));
}

#[test]
fn retention_clock_key_is_the_stores_key() {
    let m = EntityId::from_bytes([0x01, 0x90, 0, 0, 0, 0, 0x70, 0, 0x80, 0, 0, 0, 0, 0, 0, 7]);
    assert_eq!(clock_key(m).as_str(), earshot_store::retention_clock_key(m));
    assert_eq!(crate::local::meeting_of(clock_key(m).as_str()), Some(m));
}

#[test]
fn retention_clock_starts_with_the_final_transcript_and_never_restarts() {
    let dir = TempDir::new("retention");
    let (svc, clock) = services(&dir);
    let outcome = recorded(&svc, 1);
    let m = outcome.meeting;
    // Recorded, no final pass yet: no clock.
    assert_eq!(clock_of(&svc, m), RetentionClock::NotStarted);
    clock.advance_ms(5_000);
    let first = clock.now_unix_ms();
    let r = after(&svc, &outcome, None, SkipReason::NotRequested);
    assert!(matches!(r.post, Ok(HostPost::Done(_))), "{:?}", r.post);
    assert_eq!(clock_of(&svc, m), RetentionClock::Since(first));
    // A summary, a re-summary, an undo and a re-transcription leave it.
    clock.advance_ms(DAY_MS);
    store_a_summary(&svc, m, 2);
    store_a_summary(&svc, m, 3);
    crate::edit::undo_summary(&svc, m).unwrap();
    crate::edit::retranscribe_with(&svc, m, &CancelToken::new(), fake_post, &mut |_| {}).unwrap();
    assert_eq!(clock_of(&svc, m), RetentionClock::Since(first));
}

#[test]
fn retention_clock_starts_with_the_transcript_whatever_the_summary_does() {
    let dir = TempDir::new("retention");
    let (svc, clock) = services(&dir);
    clock.advance_ms(60_000);
    let now = clock.now_unix_ms();
    // Summaries turned off.
    let off = recorded(&svc, 11);
    let r = after(&svc, &off, None, SkipReason::NotRequested);
    assert!(matches!(r.post, Ok(HostPost::Done(_))), "{:?}", r.post);
    assert_eq!(clock_of(&svc, off.meeting), RetentionClock::Since(now));
    // A summary that could not run because its model is missing.
    let skipped = recorded(&svc, 21);
    let r = after(
        &svc,
        &skipped,
        None,
        SkipReason::NotInstalled {
            model: "qwen3.5-4b-q4_k_m",
            why: "not here".into(),
        },
    );
    assert!(matches!(r.post, Ok(HostPost::Done(_))));
    assert_eq!(clock_of(&svc, skipped.meeting), RetentionClock::Since(now));
    // A summary that failed.
    let failed = recorded(&svc, 31);
    let server = MockServer::start(vec![MockResponse::status(500, "upstream down")]).unwrap();
    let service = SummaryService::new(
        &SummaryConfig::new(server.endpoint().as_str(), "test-model"),
        svc.clock.clone(),
    )
    .unwrap();
    let r = after(&svc, &failed, Some(&service), SkipReason::NotRequested);
    assert!(
        matches!(r.summary, SummaryResult::Failed { .. }),
        "{:?}",
        r.summary
    );
    assert_eq!(clock_of(&svc, failed.meeting), RetentionClock::Since(now));
    // All three go once their time is up.
    clock.advance_ms(HOUR_MS);
    let r = run_choice(&svc, AudioRetention::OneHour).unwrap();
    assert_eq!((r.meetings, r.deleted_files), (3, 6), "{r:?}");
    for m in [off.meeting, skipped.meeting, failed.meeting] {
        assert!(audio_paths(&svc, m).iter().all(|p| !p.exists()));
    }
}

#[test]
fn retention_clock_waits_for_a_meeting_without_a_final_transcript() {
    let dir = TempDir::new("retention");
    let (svc, clock) = services(&dir);
    // Recorded but never cleaned up (the final pass did not run or did not succeed), and a
    // meeting with only a summary of its live draft.
    let raw = recorded(&svc, 41);
    let drafted = recorded(&svc, 42);
    store_a_summary(&svc, drafted.meeting, 43);
    clock.advance_ms(400 * DAY_MS);
    set_audio_retention(&mut svc.store(), AudioRetention::OneHour).unwrap();
    let r = run(&svc).unwrap();
    assert_eq!((r.meetings, r.deleted_files, r.clocks_started), (0, 0, 0));
    for m in [raw.meeting, drafted.meeting] {
        assert_eq!(clock_of(&svc, m), RetentionClock::NotStarted);
        assert!(audio_paths(&svc, m).iter().all(|p| p.exists()));
    }
    // A re-transcription that succeeds stores the final transcript and starts the clock.
    crate::edit::retranscribe_with(
        &svc,
        raw.meeting,
        &CancelToken::new(),
        fake_post,
        &mut |_| {},
    )
    .unwrap();
    assert_eq!(
        clock_of(&svc, raw.meeting),
        RetentionClock::Since(clock.now_unix_ms())
    );
}

#[test]
fn retention_run_starts_the_clock_of_an_older_transcribed_meeting_without_one() {
    let dir = TempDir::new("retention");
    let (svc, clock) = services(&dir);
    let outcome = recorded(&svc, 45);
    let m = outcome.meeting;
    after(&svc, &outcome, None, SkipReason::NotRequested);
    // As a meeting finished before 2026-11-05 with a skipped summary: no clock.
    svc.store()
        .delete_local_value(clock_key(m).as_str())
        .unwrap();
    assert_eq!(clock_of(&svc, m), RetentionClock::NotStarted);
    clock.advance_ms(30 * DAY_MS);
    // The run starts its clock now and deletes nothing yet.
    let r = run_choice(&svc, AudioRetention::OneHour).unwrap();
    assert_eq!((r.clocks_started, r.meetings), (1, 0), "{r:?}");
    assert_eq!(
        clock_of(&svc, m),
        RetentionClock::Since(clock.now_unix_ms())
    );
    assert!(audio_paths(&svc, m).iter().all(|p| p.exists()));
    // A later run neither restarts it nor counts it again, and deletes it once due.
    clock.advance_ms(HOUR_MS);
    let r = run_choice(&svc, AudioRetention::OneHour).unwrap();
    assert_eq!(
        (r.clocks_started, r.meetings, r.deleted_files),
        (0, 1, 2),
        "{r:?}"
    );
}

#[test]
fn retention_run_deletes_only_audio_when_it_is_due() {
    let dir = TempDir::new("retention");
    let (svc, clock) = services(&dir);
    let outcome = recorded(&svc, 51);
    let m = outcome.meeting;
    after(&svc, &outcome, None, SkipReason::NotRequested);
    let segments = live_segments(&**svc.store(), m, TranscriptPass::Final)
        .unwrap()
        .len();
    assert!(segments > 0);
    // Not yet due on the default 7 days.
    clock.advance_ms(7 * DAY_MS - 1);
    assert_eq!(
        preview(&svc, AudioRetention::SevenDays).unwrap().meetings,
        0
    );
    assert_eq!(run(&svc).unwrap().meetings, 0);
    assert!(audio_paths(&svc, m).iter().all(|p| p.exists()));
    // A shorter choice would delete it now; the preview deletes nothing.
    assert_eq!(preview(&svc, AudioRetention::OneDay).unwrap().meetings, 1);
    assert!(audio_paths(&svc, m).iter().all(|p| p.exists()));
    clock.advance_ms(1);
    assert_eq!(
        preview(&svc, AudioRetention::SevenDays).unwrap().meetings,
        1
    );
    assert_eq!(preview(&svc, AudioRetention::Forever).unwrap().meetings, 0);
    let r = run(&svc).unwrap();
    assert_eq!((r.meetings, r.deleted_files, r.busy), (1, 2, 0), "{r:?}");
    assert!(audio_paths(&svc, m).iter().all(|p| !p.exists()));
    assert!(svc.store().audio_files(m).unwrap().is_empty());
    // Transcript, speakers and the meeting stay.
    let db = svc.store();
    assert_eq!(
        live_segments(&**db, m, TranscriptPass::Final)
            .unwrap()
            .len(),
        segments
    );
    assert!(!db.speakers(m).unwrap().is_empty());
    assert!(db.meeting(m).unwrap().is_some_and(|x| !x.meta.deleted));
    drop(db);
    assert_eq!(preview(&svc, AudioRetention::OneHour).unwrap().meetings, 0);
}

#[test]
fn retention_run_skips_a_meeting_a_job_holds_until_it_is_released() {
    let dir = TempDir::new("retention");
    let (svc, clock) = services(&dir);
    let outcome = recorded(&svc, 61);
    let m = outcome.meeting;
    after(&svc, &outcome, None, SkipReason::NotRequested);
    clock.advance_ms(2 * HOUR_MS);
    assert_eq!(
        preview(&svc, AudioRetention::OneHour).unwrap(),
        RetentionPreview {
            meetings: 1,
            busy: 0
        }
    );
    let lease = svc.lease_meeting(m).unwrap();
    // The preview plans as the run does: a held meeting is counted apart.
    assert_eq!(
        preview(&svc, AudioRetention::OneHour).unwrap(),
        RetentionPreview {
            meetings: 0,
            busy: 1
        }
    );
    let r = run_choice(&svc, AudioRetention::OneHour).unwrap();
    assert_eq!((r.meetings, r.busy), (0, 1));
    assert!(r.some_left());
    assert!(audio_paths(&svc, m).iter().all(|p| p.exists()));
    drop(lease);
    let r = run_choice(&svc, AudioRetention::OneHour).unwrap();
    assert_eq!((r.meetings, r.deleted_files, r.busy), (1, 2, 0));
    // After the audio is gone a re-transcription has nothing to process; it is no error.
    let again =
        crate::edit::retranscribe_with(&svc, m, &CancelToken::new(), fake_post, &mut |_| {})
            .unwrap();
    assert!(
        matches!(again.post, HostPost::NothingToProcess),
        "{:?}",
        again.post
    );
    assert_eq!(again.missing_tracks.len(), 2);
}

#[test]
fn retention_clock_is_erased_with_the_meeting() {
    let dir = TempDir::new("retention");
    let (svc, _) = services(&dir);
    let outcome = recorded(&svc, 81);
    after(&svc, &outcome, None, SkipReason::NotRequested);
    assert!(matches!(
        clock_of(&svc, outcome.meeting),
        RetentionClock::Since(_)
    ));
    svc.delete_meeting(outcome.meeting).unwrap();
    assert_eq!(clock_of(&svc, outcome.meeting), RetentionClock::NotStarted);
}

#[test]
fn retention_run_counts_a_meeting_it_cannot_delete_and_goes_on() {
    let dir = TempDir::new("retention");
    let (svc, clock) = services(&dir);
    let stuck = recorded(&svc, 91);
    after(&svc, &stuck, None, SkipReason::NotRequested);
    let other = recorded(&svc, 92);
    after(&svc, &other, None, SkipReason::NotRequested);
    // Where the mic recording should be, a folder that a file removal cannot delete.
    let paths = audio_paths(&svc, stuck.meeting);
    std::fs::remove_file(&paths[0]).unwrap();
    std::fs::create_dir(&paths[0]).unwrap();
    std::fs::write(paths[0].join("keep"), b"x").unwrap();
    clock.advance_ms(2 * HOUR_MS);
    let r = run_choice(&svc, AudioRetention::OneHour).unwrap();
    assert_eq!((r.failed, r.meetings), (1, 1), "{r:?}");
    assert!(r.some_left());
    assert!(audio_paths(&svc, other.meeting).iter().all(|p| !p.exists()));
    assert!(!svc.store().audio_files(stuck.meeting).unwrap().is_empty());
    // Once the obstacle is gone a later run finishes it.
    std::fs::remove_dir_all(&paths[0]).unwrap();
    let r = run_choice(&svc, AudioRetention::OneHour).unwrap();
    assert_eq!((r.failed, r.meetings), (0, 1), "{r:?}");
    assert!(svc.store().audio_files(stuck.meeting).unwrap().is_empty());
}

/// A stray recording without a row, older than the grace period, that cannot be removed
/// while the returned handle is open (no sharing, so Windows refuses the removal).
#[cfg(windows)]
fn unremovable_orphan(data: &DataDir) -> std::fs::File {
    use std::os::windows::fs::OpenOptionsExt;
    let dir = data.root().join("audio").join("stray");
    std::fs::create_dir_all(&dir).unwrap();
    let stray = dir.join("mic.flac");
    std::fs::write(&stray, b"x").unwrap();
    let f = std::fs::File::options().write(true).open(&stray).unwrap();
    f.set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_secs(1))
        .unwrap();
    drop(f);
    std::fs::File::options()
        .read(true)
        .share_mode(0)
        .open(&stray)
        .unwrap()
}

#[cfg(not(windows))]
fn unremovable_orphan(_: &DataDir) -> std::fs::File {
    unreachable!("ignored off Windows")
}

#[test]
#[cfg_attr(
    not(windows),
    ignore = "a file held open without sharing blocks its removal only on Windows: run on Windows"
)]
fn retention_run_reports_deletions_made_before_a_failed_orphan_sweep() {
    let dir = TempDir::new("retention");
    let (svc, clock) = services(&dir);
    let outcome = recorded(&svc, 101);
    after(&svc, &outcome, None, SkipReason::NotRequested);
    let held = unremovable_orphan(&svc.data);
    clock.advance_ms(2 * HOUR_MS);
    let r = run_choice(&svc, AudioRetention::OneHour);
    drop(held);
    let r = r.unwrap();
    assert_eq!((r.meetings, r.deleted_files), (1, 2), "{r:?}");
    assert!(r.sweep_failed && r.some_left(), "{r:?}");
    assert!(
        audio_paths(&svc, outcome.meeting)
            .iter()
            .all(|p| !p.exists())
    );
    // Once removable, a later run sweeps it.
    let r = run_choice(&svc, AudioRetention::OneHour).unwrap();
    assert!(!r.sweep_failed && r.orphans_removed == 1, "{r:?}");
}
