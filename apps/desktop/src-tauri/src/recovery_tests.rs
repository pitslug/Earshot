//! Start-up recovery in the app (T9.18): its order before retention and the notices the window
//! reads once. Every name contains `recovery`.

use std::cell::{Cell, RefCell};
use std::sync::Arc;

use earshot_core::capture::TrackId;
use earshot_core::identity::{Clock, EntityId};
use earshot_core::model::{AudioFileRepo, ClockDomain, Meeting, MeetingRepo, Stamp, Track};
use earshot_core::prompt::Template;
use earshot_runtime::retention::{AudioRetention, set_audio_retention};
use rand_chacha::ChaCha8Rng;
use rand_core::SeedableRng;

use super::*;
use crate::testutil::{TempDir, services};

/// A meeting whose app was killed while recording: rows with no duration and an unfinished
/// mic file of two whole frames.
fn killed_meeting(svc: &Services, seed: u64) -> EntityId {
    let clock: Arc<dyn Clock + Send + Sync> = Arc::clone(&svc.clock);
    let mut rng = ChaCha8Rng::seed_from_u64(seed);
    let id = {
        let mut db = svc.store();
        let (store, hlc) = db.parts();
        let mut stamp = Stamp {
            clock: &*clock,
            hlc,
            rng: &mut rng,
        };
        let m = Meeting::new("", 1_000, 600, Template::General, &mut stamp);
        let t = Track::new(m.id, TrackId::Mic, None, ClockDomain::Replay, &mut stamp);
        let id = m.id;
        store.put_meeting(m).unwrap();
        store.put_track(t).unwrap();
        id
    };
    let mut w = svc.data.track_writer(id, TrackId::Mic, 16_000).unwrap();
    let samples: Vec<f32> = (0..4096 * 2 + 77)
        .map(|i| 0.2 * (i as f32 * 0.03).sin())
        .collect();
    w.write(&samples).unwrap();
    drop(w);
    id
}

#[test]
fn recovery_runs_before_retention_and_its_notice_is_read_once() {
    let dir = TempDir::new("recovery");
    let svc = services(&dir);
    set_audio_retention(&mut svc.store(), AudioRetention::ALL[0]).unwrap();
    let m = killed_meeting(&svc, 1);
    let told = Cell::new(0);
    let retention_saw_rows = RefCell::new(None);
    recover_then(
        &svc,
        earshot_runtime::recover::recover_interrupted,
        |report| {
            told.set(told.get() + 1);
            assert_eq!(report.recovered.len(), 1);
        },
        |p| panic!("no problem: {p}"),
        |svc| {
            *retention_saw_rows.borrow_mut() = Some(svc.store().audio_files(m).unwrap().len());
            // The app's first retention run: the recovered audio stays.
            let _ = earshot_runtime::retention::run(svc);
        },
    );
    assert_eq!(told.get(), 1);
    assert_eq!(
        *retention_saw_rows.borrow(),
        Some(1),
        "rows exist before retention"
    );
    assert_eq!(svc.store().audio_files(m).unwrap().len(), 1);
    assert_eq!(
        svc.store().meeting(m).unwrap().unwrap().duration_ms,
        Some(4096 * 2 * 1000 / 16_000)
    );
    let first = recovered(&svc).unwrap();
    assert_eq!(
        first,
        [RecoveredRecording {
            meeting: m.to_string(),
            saved_ms: 512,
        }]
    );
    assert!(recovered(&svc).unwrap().is_empty(), "shown once");
}

#[test]
fn recovery_that_panics_still_starts_retention_and_tells_nothing() {
    let dir = TempDir::new("recovery");
    let svc = services(&dir);
    let told = Cell::new(false);
    let retention = Cell::new(false);
    let problem = Cell::new(None);
    recover_then(
        &svc,
        |_| panic!("an invented fault in recovery"),
        |_| told.set(true),
        |p| problem.set(Some(p)),
        |_| retention.set(true),
    );
    assert!(retention.get() && !told.get());
    assert_eq!(
        problem.get(),
        Some(RECOVERY_DID_NOT_RUN),
        "said beside retention's problems"
    );
}

#[test]
fn recovery_with_nothing_to_recover_tells_nothing() {
    let dir = TempDir::new("recovery");
    let svc = services(&dir);
    let told = Cell::new(false);
    let retention = Cell::new(false);
    recover_then(
        &svc,
        earshot_runtime::recover::recover_interrupted,
        |_| told.set(true),
        |p| panic!("no problem: {p}"),
        |_| retention.set(true),
    );
    assert!(retention.get() && !told.get());
    assert!(recovered(&svc).unwrap().is_empty());
}

#[test]
fn recovery_that_fails_says_so_and_still_tells_what_it_recovered() {
    let dir = TempDir::new("recovery");
    let svc = services(&dir);
    let id = EntityId::from_bytes([3; 16]);
    let failing = |report: RecoveryReport| {
        let told = Cell::new(false);
        let problem = Cell::new(None);
        recover_then(
            &svc,
            move |_| report,
            |_| told.set(true),
            |p| problem.set(Some(p)),
            |_| {},
        );
        (told.get(), problem.get())
    };
    let failed = RecoveryReport {
        failed: vec![(id, "an invented error".into())],
        ..RecoveryReport::default()
    };
    assert_eq!(failing(failed.clone()), (false, Some(RECOVERY_FAILED)));
    let list_failed = RecoveryReport {
        list_failed: Some("an invented error".into()),
        ..RecoveryReport::default()
    };
    assert_eq!(failing(list_failed), (false, Some(RECOVERY_DID_NOT_RUN)));
    // One meeting back and one not: both are said.
    let both = RecoveryReport {
        recovered: vec![earshot_runtime::recover::RecoveredMeeting {
            meeting: EntityId::from_bytes([4; 16]),
            saved_ms: 1_000,
        }],
        ..failed
    };
    assert_eq!(failing(both), (true, Some(RECOVERY_FAILED)));
    // Fixed copy: no error text, no run of spaces.
    for copy in [RECOVERY_FAILED, RECOVERY_DID_NOT_RUN] {
        assert!(!copy.contains("invented") && !copy.contains("  "), "{copy}");
        assert!(copy.ends_with('.'), "{copy}");
    }
}

#[test]
fn recovery_problem_stays_on_the_storage_page_past_retention_runs() {
    let status = RetentionStatus::default();
    status.set_recovery(RECOVERY_FAILED);
    assert_eq!(status.problem().as_deref(), Some(RECOVERY_FAILED));
    // A retention run that went well clears only its own problem.
    status.set(None);
    assert_eq!(status.problem().as_deref(), Some(RECOVERY_FAILED));
    status.set(Some("Retention left some audio."));
    assert_eq!(
        status.problem().as_deref(),
        Some(format!("Retention left some audio. {RECOVERY_FAILED}").as_str())
    );
}
