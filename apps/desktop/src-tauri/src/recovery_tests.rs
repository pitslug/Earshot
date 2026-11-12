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
    recover_then(
        &svc,
        |_| panic!("an invented fault in recovery"),
        |_| told.set(true),
        |_| retention.set(true),
    );
    assert!(retention.get() && !told.get());
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
        |_| retention.set(true),
    );
    assert!(retention.get() && !told.get());
    assert!(recovered(&svc).unwrap().is_empty());
}
