//! Tests of the Storage page IPC (T7.4k). Every name contains `storage`.

use std::sync::Arc;

use earshot_core::capture::TrackId;
use earshot_core::identity::{Clock, EntityId};
use earshot_core::model::{AudioFileRepo, ClockDomain, Meeting, MeetingRepo, Stamp, Track};
use earshot_core::prompt::Template;
use earshot_store::{AudioProvenance, DAY_MS, HOUR_MS};
use rand_chacha::ChaCha8Rng;
use rand_core::SeedableRng;

use super::*;
use crate::testutil::{T0, TempDir, services};

/// A two-track meeting with both recordings stored; its retention clock started at `since`
/// (`None`: not started).
pub(crate) fn meeting_with_audio(svc: &Services, seed: u64, since: Option<u64>) -> EntityId {
    let clock: Arc<dyn Clock + Send + Sync> = Arc::clone(&svc.clock);
    let mut rng = ChaCha8Rng::seed_from_u64(seed);
    let mut db = svc.store();
    let (store, hlc) = db.parts();
    let mut stamp = Stamp {
        clock: &*clock,
        hlc,
        rng: &mut rng,
    };
    let mut m = Meeting::new("Review", T0, 600, Template::General, &mut stamp);
    m.duration_ms = Some(1_000);
    let tracks: Vec<Track> = [TrackId::Mic, TrackId::Reference]
        .into_iter()
        .map(|k| Track::new(m.id, k, None, ClockDomain::Replay, &mut stamp))
        .collect();
    store.put_meeting(m.clone()).unwrap();
    for t in &tracks {
        store.put_track(t.clone()).unwrap();
    }
    for t in &tracks {
        let mut w = svc.data.track_writer(m.id, t.kind, 16_000).unwrap();
        w.write(&[0.1; 1_600]).unwrap();
        store
            .finalize_track_audio(
                w,
                &svc.data,
                t,
                AudioProvenance {
                    pipeline_version: "test".into(),
                    aec_version: None,
                },
                &*clock,
                &mut rng,
            )
            .unwrap();
    }
    if let Some(since) = since {
        // Test set-up only: production starts the clock through the runtime's registry.
        store
            .set_local_value(
                &earshot_store::retention_clock_key(m.id),
                &earshot_store::encode_retention_clock(since),
            )
            .unwrap();
    }
    m.id
}

fn has_audio(svc: &Services, m: EntityId) -> bool {
    !svc.store().audio_files(m).unwrap().is_empty()
}

/// Puts a non-empty folder where `m`'s mic recording is, so deleting it fails on every
/// platform (a file in use on Windows fails the same way).
fn block_deletion(svc: &Services, m: EntityId) {
    let path = svc
        .data
        .resolve(&earshot_store::DataDir::audio_rel(m, TrackId::Mic))
        .unwrap();
    std::fs::remove_file(&path).unwrap();
    std::fs::create_dir(&path).unwrap();
    std::fs::write(path.join("keep"), b"x").unwrap();
}

fn ok() -> RetentionStatus {
    RetentionStatus::default()
}

#[test]
fn storage_view_lists_every_choice_with_the_default_selected() {
    let dir = TempDir::new("storage");
    let svc = services(&dir);
    let v = storage_view(&svc, &ok()).unwrap();
    assert_eq!(v.retention, "7d");
    let labels: Vec<&str> = v.options.iter().map(|o| o.label.as_str()).collect();
    assert_eq!(
        labels,
        [
            "1 hour", "4 hours", "12 hours", "1 day", "3 days", "7 days", "14 days", "28 days",
            "3 months", "6 months", "1 year", "Forever"
        ]
    );
    assert_eq!(v.options[0].keep_ms, Some(HOUR_MS));
    assert_eq!(v.options[11].keep_ms, None);
    assert!(v.unknown.is_empty());
    assert_eq!(v.problem, None);
    svc.store()
        .set_local_value(retention::AUDIO_RETENTION_KEY, b"2w")
        .unwrap();
    let v = storage_view(&svc, &ok()).unwrap();
    assert_eq!(v.retention, "7d");
    assert_eq!(v.unknown.len(), 1, "{:?}", v.unknown);
}

fn ui_choices_path() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../../packages/ui/src/ipc/retentionChoices.ts")
}

/// The UI mock's copy of the choices, generated from [`retention_options`].
fn ui_choices() -> String {
    format!(
        "// Generated from `storage_settings::retention_options` by\n\
         // `EARSHOT_UPDATE_BINDINGS=1 cargo test -p earshot-desktop bindings`; do not edit.\n\n\
         import type {{ RetentionOption }} from \"./bindings\";\n\n\
         /** The retention choices as the app sends them (for the mock). */\n\
         export const RETENTION_CHOICES: RetentionOption[] = {};\n",
        serde_json::to_string_pretty(&retention_options()).unwrap()
    )
}

#[test]
fn storage_choices_bindings_are_up_to_date() {
    let path = ui_choices_path();
    let fresh = ui_choices();
    if std::env::var_os("EARSHOT_UPDATE_BINDINGS").is_some_and(|v| v == "1") {
        std::fs::write(&path, &fresh).unwrap();
        return;
    }
    let committed = std::fs::read_to_string(&path).unwrap_or_default();
    assert!(
        committed.replace("\r\n", "\n") == fresh,
        "{} is out of date: run `EARSHOT_UPDATE_BINDINGS=1 cargo test -p earshot-desktop \
         bindings` and commit the result",
        path.display()
    );
}

#[test]
fn storage_preview_counts_what_a_shorter_choice_deletes_and_deletes_nothing() {
    let dir = TempDir::new("storage");
    let svc = services(&dir);
    let old = meeting_with_audio(&svc, 1, Some(T0 - 2 * DAY_MS));
    let recent = meeting_with_audio(&svc, 2, Some(T0 - 2 * HOUR_MS));
    let open = meeting_with_audio(&svc, 3, None);
    let n = |v| retention_preview(&svc, v).unwrap();
    let c = |meetings, in_use| RetentionCount { meetings, in_use };
    assert_eq!(n("7d"), c(0, 0));
    assert_eq!(n("1d"), c(1, 0));
    assert_eq!(n("1h"), c(2, 0));
    assert_eq!(n("forever"), c(0, 0));
    // A meeting a job holds is counted apart, as the run skips it.
    let lease = svc.lease_meeting(recent).unwrap();
    assert_eq!(n("1h"), c(1, 1));
    drop(lease);
    for m in [old, recent, open] {
        assert!(has_audio(&svc, m));
    }
    assert_eq!(
        retention_preview(&svc, "7 days").unwrap_err().kind,
        "invalid_setting"
    );
}

#[test]
fn storage_saving_a_choice_applies_it_to_existing_meetings_at_once() {
    let dir = TempDir::new("storage");
    let svc = services(&dir);
    let old = meeting_with_audio(&svc, 11, Some(T0 - 2 * DAY_MS));
    let recent = meeting_with_audio(&svc, 12, Some(T0 - 2 * HOUR_MS));
    let saved = set_storage(&svc, &ok(), "1d").unwrap();
    assert_eq!((saved.deleted, saved.some_left.as_deref()), (1, None));
    assert_eq!(saved.view.retention, "1d");
    assert!(!has_audio(&svc, old));
    assert!(has_audio(&svc, recent));
    assert!(
        svc.store().meeting(old).unwrap().is_some(),
        "only audio goes"
    );
    assert_eq!(storage_view(&svc, &ok()).unwrap().retention, "1d");
    // A refused value stores nothing.
    assert_eq!(
        set_storage(&svc, &ok(), "").unwrap_err().kind,
        "invalid_setting"
    );
    assert_eq!(storage_view(&svc, &ok()).unwrap().retention, "1d");
}

#[test]
fn storage_saving_says_saved_when_some_recordings_cannot_be_deleted_now() {
    let dir = TempDir::new("storage");
    let svc = services(&dir);
    let stuck = meeting_with_audio(&svc, 31, Some(T0 - 2 * DAY_MS));
    let gone = meeting_with_audio(&svc, 32, Some(T0 - 2 * DAY_MS));
    block_deletion(&svc, stuck);
    let saved = set_storage(&svc, &ok(), "1d").unwrap();
    assert_eq!(saved.view.retention, "1d");
    assert_eq!(saved.deleted, 1);
    assert_eq!(saved.some_left.as_deref(), Some(SOME_LEFT_MESSAGE));
    assert!(!SOME_LEFT_MESSAGE.contains(&*svc.data.root().to_string_lossy()));
    // The choice is stored, so the page shows it on reload.
    assert_eq!(storage_view(&svc, &ok()).unwrap().retention, "1d");
    assert!(has_audio(&svc, stuck));
    assert!(!has_audio(&svc, gone));
}

#[test]
fn storage_loop_runs_at_once_and_after_every_wait() {
    let dir = TempDir::new("storage");
    let svc = services(&dir);
    let m = meeting_with_audio(&svc, 21, Some(T0 - 8 * DAY_MS));
    let status = ok();
    let mut waits = 0;
    retention_loop(&svc, &status, retention::run, || {
        // The start-up run (default 7 days) happened before the first wait.
        assert!(!has_audio(&svc, m), "wait {waits}");
        waits += 1;
        waits < 3
    });
    assert_eq!(waits, 3);
    assert_eq!(status.problem(), None);
}

#[test]
fn storage_loop_keeps_going_after_a_panicking_or_failing_run_and_reports_it() {
    let dir = TempDir::new("storage");
    let svc = services(&dir);
    let status = ok();
    let calls = std::cell::Cell::new(0);
    let mut seen = Vec::new();
    retention_loop(
        &svc,
        &status,
        |s| {
            calls.set(calls.get() + 1);
            match calls.get() {
                1 => panic!("a bug in a run"),
                2 => Err(RuntimeError::Internal("store failed")),
                _ => retention::run(s),
            }
        },
        || {
            seen.push(status.problem());
            calls.get() < 3
        },
    );
    assert_eq!(calls.get(), 3);
    assert_eq!(
        seen,
        [
            Some(RUN_STOPPED_MESSAGE.to_owned()),
            Some(SOME_LEFT_MESSAGE.to_owned()),
            None
        ]
    );
    // The page shows the last problem.
    let blocked = meeting_with_audio(&svc, 41, Some(T0 - 8 * DAY_MS));
    block_deletion(&svc, blocked);
    retention_loop(&svc, &status, retention::run, || false);
    assert_eq!(
        storage_view(&svc, &status).unwrap().problem.as_deref(),
        Some(SOME_LEFT_MESSAGE)
    );
}

#[test]
#[cfg(windows)] // The desktop app is Windows-only; a file held open without sharing blocks removal there.
fn storage_saving_counts_deletions_made_before_a_failed_orphan_sweep() {
    use std::os::windows::fs::OpenOptionsExt;
    let dir = TempDir::new("storage");
    let svc = services(&dir);
    let m = meeting_with_audio(&svc, 51, Some(T0 - 2 * DAY_MS));
    // A stray recording without a row, older than the grace period, held open without
    // sharing, so the sweep cannot remove it.
    let stray_dir = svc.data.root().join("audio").join("stray");
    std::fs::create_dir_all(&stray_dir).unwrap();
    let stray = stray_dir.join("mic.flac");
    std::fs::write(&stray, b"x").unwrap();
    let f = std::fs::File::options().write(true).open(&stray).unwrap();
    f.set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_secs(1))
        .unwrap();
    drop(f);
    let held = std::fs::File::options()
        .read(true)
        .share_mode(0)
        .open(&stray)
        .unwrap();
    let saved = set_storage(&svc, &ok(), "1d");
    drop(held);
    let saved = saved.unwrap();
    assert_eq!(saved.deleted, 1);
    assert_eq!(saved.some_left.as_deref(), Some(SOME_LEFT_MESSAGE));
    assert!(!has_audio(&svc, m));
}
