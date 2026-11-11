use std::sync::atomic::{AtomicU64, AtomicUsize};

use earshot_update::VerifyError;

use super::*;

/// What a scripted check answers.
type CheckAnswer = Result<Option<(String, u64)>, UpdateError>;

/// A scripted update host, counting what it was asked.
#[derive(Default)]
struct Fake {
    checks: AtomicUsize,
    downloads: AtomicUsize,
    launches: AtomicUsize,
    check_answer: Mutex<Option<CheckAnswer>>,
    download_answer: Mutex<Option<Result<(), UpdateError>>>,
    launch_answer: Mutex<Option<Result<(), UpdateError>>>,
    panic_in_check: bool,
    wait_for_cancel: bool,
}

impl UpdateWork for Fake {
    fn check(&self) -> Result<Option<(String, u64)>, UpdateError> {
        self.checks.fetch_add(1, Ordering::SeqCst);
        assert!(!self.panic_in_check, "a panic in the check");
        self.check_answer
            .lock()
            .unwrap()
            .take()
            .unwrap_or(Ok(Some(("9.9.9".into(), 100))))
    }

    fn download(
        &self,
        cancel: &CancelToken,
        progress: &mut dyn FnMut(u64, u64),
    ) -> Result<(), UpdateError> {
        self.downloads.fetch_add(1, Ordering::SeqCst);
        progress(40, 100);
        if self.wait_for_cancel {
            while !cancel.is_cancelled() {
                std::thread::yield_now();
            }
            return Err(UpdateError::Unreadable("cut".into()));
        }
        self.download_answer
            .lock()
            .unwrap()
            .take()
            .unwrap_or(Ok(()))
    }

    fn launch(&self) -> Result<(), UpdateError> {
        self.launches.fetch_add(1, Ordering::SeqCst);
        self.launch_answer.lock().unwrap().take().unwrap_or(Ok(()))
    }
}

/// Jobs over `fake`, and every phase they announced.
fn jobs(fake: Arc<Fake>) -> (UpdateJobs, Arc<Mutex<Vec<UpdatePhase>>>) {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = Arc::clone(&seen);
    let sink: UpdateSink = Arc::new(move |e: UpdateEvent| log.lock().unwrap().push(e.phase));
    (UpdateJobs::new(fake, sink), seen)
}

fn available(j: &UpdateJobs) {
    j.start_check(false).unwrap().join().unwrap();
    assert!(matches!(j.phase(), UpdatePhase::Available { .. }));
}

fn ready(j: &UpdateJobs) {
    available(j);
    j.start_download().unwrap().join().unwrap();
    assert!(matches!(j.phase(), UpdatePhase::Ready { .. }));
}

#[test]
fn update_nothing_is_asked_until_someone_presses_a_button() {
    let fake = Arc::new(Fake::default());
    let (j, seen) = jobs(Arc::clone(&fake));
    assert_eq!(j.phase(), UpdatePhase::Idle);
    assert_eq!(fake.checks.load(Ordering::SeqCst), 0);
    assert!(seen.lock().unwrap().is_empty());
}

#[test]
fn update_a_check_then_a_download_ends_ready_and_starts_nothing_by_itself() {
    let fake = Arc::new(Fake::default());
    let (j, seen) = jobs(Arc::clone(&fake));
    ready(&j);
    assert_eq!(
        fake.launches.load(Ordering::SeqCst),
        0,
        "only after the user confirms"
    );
    let version = || "9.9.9".to_owned();
    assert_eq!(
        *seen.lock().unwrap(),
        [
            UpdatePhase::Checking,
            UpdatePhase::Available {
                version: version(),
                size: 100
            },
            UpdatePhase::Downloading {
                version: version(),
                done: 0,
                total: 100
            },
            UpdatePhase::Downloading {
                version: version(),
                done: 40,
                total: 100
            },
            UpdatePhase::Ready { version: version() },
        ]
    );
}

#[test]
fn update_a_check_cannot_replace_the_offer_a_ready_install_will_run() {
    let fake = Arc::new(Fake::default());
    let (j, _) = jobs(Arc::clone(&fake));
    ready(&j);
    assert_eq!(j.start_check(false).unwrap_err().kind, "busy");
    assert_eq!(fake.checks.load(Ordering::SeqCst), 1);
    assert!(matches!(j.phase(), UpdatePhase::Ready { .. }));
}

#[test]
fn update_a_check_does_not_download() {
    let fake = Arc::new(Fake::default());
    let (j, _) = jobs(Arc::clone(&fake));
    available(&j);
    assert_eq!(fake.downloads.load(Ordering::SeqCst), 0);
}

#[test]
fn update_up_to_date_is_said_and_offers_nothing() {
    let fake = Arc::new(Fake::default());
    *fake.check_answer.lock().unwrap() = Some(Ok(None));
    let (j, _) = jobs(Arc::clone(&fake));
    j.start_check(false).unwrap().join().unwrap();
    assert_eq!(j.phase(), UpdatePhase::UpToDate);
    assert_eq!(j.start_download().unwrap_err().kind, "update_not_ready");
}

#[test]
fn update_a_failure_shows_fixed_copy_when_asked_and_nothing_when_automatic() {
    let failure = || UpdateError::Network("https://secret.example/path?token=abc".into());
    let fake = Arc::new(Fake::default());
    *fake.check_answer.lock().unwrap() = Some(Err(failure()));
    let (j, _) = jobs(Arc::clone(&fake));
    j.start_check(false).unwrap().join().unwrap();
    assert_eq!(
        j.phase(),
        UpdatePhase::Failed {
            message: NO_CONNECTION.into()
        }
    );

    *fake.check_answer.lock().unwrap() = Some(Err(failure()));
    j.start_check(true).unwrap().join().unwrap();
    assert_eq!(
        j.phase(),
        UpdatePhase::Idle,
        "nobody asked, so nothing to say"
    );
}

#[test]
fn update_every_failure_has_copy_and_none_quotes_the_error() {
    let needle = "NEEDLE";
    let errors = [
        UpdateError::NoKey,
        UpdateError::Setup(needle.into()),
        UpdateError::Network(needle.into()),
        UpdateError::NotPublished,
        UpdateError::Status(503),
        UpdateError::Refused(VerifyError::BadSignature),
        UpdateError::InstallerChanged,
        UpdateError::Unreadable(needle.into()),
    ];
    for error in &errors {
        let copy = failure_copy(error);
        assert!(!copy.contains(needle), "{copy}");
        assert!(copy.ends_with('.'), "{copy}");
    }
}

#[test]
fn update_a_panic_ends_in_a_phase_the_page_can_leave() {
    let fake = Arc::new(Fake {
        panic_in_check: true,
        ..Fake::default()
    });
    let (j, _) = jobs(fake);
    j.start_check(false).unwrap().join().unwrap();
    assert_eq!(
        j.phase(),
        UpdatePhase::Failed {
            message: SOMETHING_WENT_WRONG.into()
        }
    );
}

#[test]
fn update_two_steps_never_run_together() {
    let fake = Arc::new(Fake {
        wait_for_cancel: true,
        ..Fake::default()
    });
    let (j, _) = jobs(Arc::clone(&fake));
    available(&j);
    let running = j.start_download().unwrap();
    assert_eq!(j.start_check(false).unwrap_err().kind, "busy");
    j.cancel();
    running.join().unwrap();
}

#[test]
fn update_a_cancelled_download_goes_back_to_the_offer() {
    let fake = Arc::new(Fake {
        wait_for_cancel: true,
        ..Fake::default()
    });
    let (j, _) = jobs(fake);
    available(&j);
    let running = j.start_download().unwrap();
    j.cancel();
    running.join().unwrap();
    assert!(
        matches!(j.phase(), UpdatePhase::Available { .. }),
        "{:?}",
        j.phase()
    );
}

#[test]
fn update_install_needs_a_verified_download() {
    let fake = Arc::new(Fake::default());
    let (j, _) = jobs(Arc::clone(&fake));
    let gate = WorkGate::new();
    assert_eq!(
        j.install(&gate, false).unwrap_err().kind,
        "update_not_ready"
    );
    available(&j);
    assert_eq!(
        j.install(&gate, false).unwrap_err().kind,
        "update_not_ready"
    );
    assert_eq!(fake.launches.load(Ordering::SeqCst), 0);
}

#[test]
fn update_install_is_refused_while_a_meeting_or_a_download_runs() {
    let fake = Arc::new(Fake::default());
    let (j, _) = jobs(Arc::clone(&fake));
    ready(&j);
    let gate = WorkGate::new();

    let meeting = gate.enter_meeting().unwrap();
    assert_eq!(j.install(&gate, false).unwrap_err().kind, "busy");
    drop(meeting);
    let job = gate.enter_work().unwrap();
    assert_eq!(j.install(&gate, false).unwrap_err().kind, "busy");
    drop(job);
    let voice = gate.enter_voice().unwrap();
    assert_eq!(j.install(&gate, false).unwrap_err().kind, "busy");
    drop(voice);
    assert_eq!(j.install(&gate, true).unwrap_err().kind, "busy");

    assert_eq!(fake.launches.load(Ordering::SeqCst), 0, "nothing ran");
    assert!(
        matches!(j.phase(), UpdatePhase::Ready { .. }),
        "still ready"
    );

    j.install(&gate, false).unwrap();
    assert_eq!(fake.launches.load(Ordering::SeqCst), 1);
    assert!(gate.enter_meeting().is_err(), "closed until the app exits");
}

#[test]
fn update_a_refused_start_leaves_the_app_open_for_work() {
    let fake = Arc::new(Fake::default());
    let (j, _) = jobs(Arc::clone(&fake));
    ready(&j);
    let gate = WorkGate::new();
    *fake.launch_answer.lock().unwrap() = Some(Err(UpdateError::InstallerChanged));
    assert_eq!(j.install(&gate, false).unwrap_err().kind, "update_failed");
    assert_eq!(
        j.phase(),
        UpdatePhase::Failed {
            message: INSTALLER_CHANGED.into()
        }
    );
    drop(gate.enter_meeting().unwrap());
}

#[test]
fn update_folders_newer_database_still_offers_the_check_by_hand() {
    let dir = crate::testutil::TempDir::new("update-newer");
    let state = AppState::open(crate::testutil::newer_database(&dir));
    let (j, _) = jobs(Arc::new(Fake::default()));
    let v = view(&state, &j).unwrap();
    assert!(!v.auto_check);
    assert_eq!(v.phase, UpdatePhase::Idle);
    // Any other failure to open the store is still an error.
    let broken = dir.0.join("broken");
    std::fs::create_dir_all(broken.join("earshot.db")).unwrap();
    let state = AppState::open(earshot_store::DataDir::new(broken));
    let e = view(&state, &j).unwrap_err();
    assert_ne!(e.kind, crate::ipc::NEWER_DATA_KIND);
}

const HOUR_MS: u64 = 60 * 60 * 1000;
const T0: u64 = crate::testutil::T0;

type Notify = Arc<dyn Fn(&str) -> bool + Send + Sync>;

/// Jobs over `fake` whose recorder does what the app's does, with `notify` as the window.
fn scheduled(
    fake: Arc<Fake>,
    services: &Services,
    now: Arc<AtomicU64>,
    notify: Notify,
) -> UpdateJobs {
    let services = services.clone();
    let recorder: CheckRecorder = Arc::new(move |end: &CheckEnd| {
        let n = Arc::clone(&notify);
        after_check(&services, end, now.load(Ordering::SeqCst), |v| n(v)).unwrap();
    });
    UpdateJobs::with_recorder(fake, Arc::new(|_| {}), recorder)
}

fn auto_on(services: &Services, schedule: UpdateSchedule) {
    let mut store = services.store();
    earshot_runtime::update::set_auto_check(&mut store, true).unwrap();
    earshot_runtime::update::set_update_schedule(&mut store, schedule).unwrap();
}

fn last(services: &Services) -> Option<u64> {
    earshot_runtime::update::last_check(&services.store()).unwrap()
}

fn never_notify() -> Notify {
    Arc::new(|_| false)
}

fn at(ms: u64) -> Arc<AtomicU64> {
    Arc::new(AtomicU64::new(ms))
}

#[test]
fn update_schedule_off_makes_no_request_however_overdue() {
    let dir = crate::testutil::TempDir::new("update-schedule-off");
    let services = crate::testutil::services(&dir);
    let fake = Arc::new(Fake::default());
    let j = scheduled(Arc::clone(&fake), &services, at(T0), never_notify());
    // A schedule chosen, never checked, a week late: still off.
    earshot_runtime::update::set_update_schedule(&mut services.store(), UpdateSchedule::Hourly)
        .unwrap();
    let mut ran = None;
    for t in [T0, T0 + 7 * 24 * HOUR_MS] {
        assert_eq!(
            schedule_tick(&services, &j, t, &mut ran).unwrap(),
            SCHEDULE_STEP
        );
    }
    assert_eq!(fake.checks.load(Ordering::SeqCst), 0);
    assert_eq!(j.phase(), UpdatePhase::Idle);
    assert_eq!(last(&services), None);
}

#[test]
fn update_schedule_checks_when_due_and_stores_the_end() {
    let dir = crate::testutil::TempDir::new("update-schedule-due");
    let services = crate::testutil::services(&dir);
    auto_on(&services, UpdateSchedule::FourHours);
    let fake = Arc::new(Fake::default());
    *fake.check_answer.lock().unwrap() = Some(Ok(None));
    let now = at(T0);
    let j = scheduled(
        Arc::clone(&fake),
        &services,
        Arc::clone(&now),
        never_notify(),
    );
    let mut ran = None;
    // Never checked: due at once.
    assert_eq!(
        schedule_tick(&services, &j, T0, &mut ran).unwrap(),
        SCHEDULE_STEP
    );
    assert_eq!(fake.checks.load(Ordering::SeqCst), 1);
    assert_eq!(j.phase(), UpdatePhase::UpToDate);
    assert_eq!(last(&services), Some(T0));
    // An hour on: not due, and the sleep is bounded by the step.
    let sleep = schedule_tick(&services, &j, T0 + HOUR_MS, &mut ran).unwrap();
    assert!(sleep <= SCHEDULE_STEP, "{sleep:?}");
    assert_eq!(fake.checks.load(Ordering::SeqCst), 1);
    // Close to due: sleeps only until then.
    let sleep = schedule_tick(&services, &j, T0 + 4 * HOUR_MS - 1_000, &mut ran).unwrap();
    assert_eq!(sleep, Duration::from_secs(1));
    // Four hours on: due again.
    *fake.check_answer.lock().unwrap() = Some(Ok(None));
    now.store(T0 + 4 * HOUR_MS, Ordering::SeqCst);
    schedule_tick(&services, &j, T0 + 4 * HOUR_MS, &mut ran).unwrap();
    assert_eq!(fake.checks.load(Ordering::SeqCst), 2);
    assert_eq!(last(&services), Some(T0 + 4 * HOUR_MS));
}

#[test]
fn update_schedule_wake_from_sleep_checks_once_not_in_a_burst() {
    let dir = crate::testutil::TempDir::new("update-schedule-wake");
    let services = crate::testutil::services(&dir);
    auto_on(&services, UpdateSchedule::Hourly);
    earshot_runtime::update::record_check(&mut services.store(), T0).unwrap();
    let fake = Arc::new(Fake::default());
    *fake.check_answer.lock().unwrap() = Some(Ok(None));
    let woke = T0 + 10 * HOUR_MS;
    let j = scheduled(Arc::clone(&fake), &services, at(woke), never_notify());
    let mut ran = None;
    for t in [woke, woke + 1_000, woke + 60_000, woke + 30 * 60_000] {
        schedule_tick(&services, &j, t, &mut ran).unwrap();
    }
    assert_eq!(fake.checks.load(Ordering::SeqCst), 1);
}

#[test]
fn update_schedule_clock_moved_back_waits_no_more_than_one_interval() {
    let dir = crate::testutil::TempDir::new("update-schedule-back");
    let services = crate::testutil::services(&dir);
    auto_on(&services, UpdateSchedule::Hourly);
    // The last check is stamped a year ahead of the clock now.
    earshot_runtime::update::record_check(&mut services.store(), T0 + 365 * 24 * HOUR_MS).unwrap();
    let fake = Arc::new(Fake::default());
    *fake.check_answer.lock().unwrap() = Some(Ok(None));
    let j = scheduled(Arc::clone(&fake), &services, at(T0), never_notify());
    let mut ran = None;
    schedule_tick(&services, &j, T0, &mut ran).unwrap();
    assert_eq!(fake.checks.load(Ordering::SeqCst), 0);
    schedule_tick(&services, &j, T0 + HOUR_MS, &mut ran).unwrap();
    assert_eq!(
        fake.checks.load(Ordering::SeqCst),
        1,
        "one interval from now"
    );
}

#[test]
fn update_schedule_a_changed_setting_applies_at_the_next_wake() {
    let dir = crate::testutil::TempDir::new("update-schedule-change");
    let services = crate::testutil::services(&dir);
    auto_on(&services, UpdateSchedule::Weekly);
    earshot_runtime::update::record_check(&mut services.store(), T0).unwrap();
    let fake = Arc::new(Fake::default());
    *fake.check_answer.lock().unwrap() = Some(Ok(None));
    let t = T0 + 2 * 24 * HOUR_MS;
    let j = scheduled(Arc::clone(&fake), &services, at(t), never_notify());
    let mut ran = None;
    schedule_tick(&services, &j, t, &mut ran).unwrap();
    assert_eq!(fake.checks.load(Ordering::SeqCst), 0);
    earshot_runtime::update::set_update_schedule(&mut services.store(), UpdateSchedule::Daily)
        .unwrap();
    schedule_tick(&services, &j, t, &mut ran).unwrap();
    assert_eq!(fake.checks.load(Ordering::SeqCst), 1);
    // Turned off: nothing more, even when due.
    earshot_runtime::update::set_auto_check(&mut services.store(), false).unwrap();
    schedule_tick(&services, &j, t + 30 * 24 * HOUR_MS, &mut ran).unwrap();
    assert_eq!(fake.checks.load(Ordering::SeqCst), 1);
}

#[test]
fn update_schedule_a_check_whose_end_is_not_stored_is_not_repeated_each_wake() {
    let dir = crate::testutil::TempDir::new("update-schedule-unstored");
    let services = crate::testutil::services(&dir);
    auto_on(&services, UpdateSchedule::Hourly);
    let fake = Arc::new(Fake::default());
    *fake.check_answer.lock().unwrap() = Some(Ok(None));
    // A recorder that stores nothing.
    let (j, _) = jobs(Arc::clone(&fake));
    let mut ran = None;
    for t in [T0, T0 + 5 * 60_000, T0 + 30 * 60_000] {
        schedule_tick(&services, &j, t, &mut ran).unwrap();
    }
    assert_eq!(fake.checks.load(Ordering::SeqCst), 1);
    *fake.check_answer.lock().unwrap() = Some(Ok(None));
    schedule_tick(&services, &j, T0 + HOUR_MS, &mut ran).unwrap();
    assert_eq!(fake.checks.load(Ordering::SeqCst), 2);
}

#[test]
fn update_schedule_never_checks_once_an_update_installs() {
    let dir = crate::testutil::TempDir::new("update-schedule-install");
    let services = crate::testutil::services(&dir);
    auto_on(&services, UpdateSchedule::Hourly);
    let fake = Arc::new(Fake::default());
    let j = scheduled(Arc::clone(&fake), &services, at(T0), never_notify());
    ready(&j);
    let checks = fake.checks.load(Ordering::SeqCst);
    let gate = WorkGate::new();
    j.install(&gate, false).unwrap();
    let mut ran = None;
    for t in [T0, T0 + 2 * HOUR_MS, T0 + 30 * 24 * HOUR_MS] {
        schedule_tick(&services, &j, t, &mut ran).unwrap();
    }
    assert_eq!(j.start_check(false).unwrap_err().kind, "busy");
    assert_eq!(fake.checks.load(Ordering::SeqCst), checks);
}

#[test]
fn update_an_install_that_did_not_start_opens_checks_again() {
    let fake = Arc::new(Fake::default());
    let (j, _) = jobs(Arc::clone(&fake));
    ready(&j);
    let gate = WorkGate::new();
    let meeting = gate.enter_meeting().unwrap();
    assert_eq!(j.install(&gate, false).unwrap_err().kind, "busy");
    drop(meeting);
    *fake.launch_answer.lock().unwrap() = Some(Err(UpdateError::InstallerChanged));
    assert_eq!(j.install(&gate, false).unwrap_err().kind, "update_failed");
    j.start_check(false).unwrap().join().unwrap();
    assert!(matches!(j.phase(), UpdatePhase::Available { .. }));
}

#[test]
fn update_schedule_loop_sleeps_first_and_survives_a_panicking_check() {
    let dir = crate::testutil::TempDir::new("update-schedule-loop");
    let services = crate::testutil::services(&dir);
    auto_on(&services, UpdateSchedule::Hourly);
    let fake = Arc::new(Fake {
        panic_in_check: true,
        ..Fake::default()
    });
    let now = at(T0);
    let j = scheduled(
        Arc::clone(&fake),
        &services,
        Arc::clone(&now),
        never_notify(),
    );
    let clock = earshot_core::identity::ManualClock::new(T0);
    // Stopped before the first wake: nothing at all.
    schedule_loop(&services, &j, &clock, |_| false);
    assert_eq!(fake.checks.load(Ordering::SeqCst), 0);
    // Three wakes an hour apart: each check panics, the thread goes on, and a panicking check
    // still counts as the last one.
    let mut wakes = 0u64;
    schedule_loop(&services, &j, &clock, |sleep| {
        assert!(sleep <= SCHEDULE_STEP, "{sleep:?}");
        wakes += 1;
        let t = T0 + (wakes - 1) * HOUR_MS;
        clock.set_ms(t);
        now.store(t, Ordering::SeqCst);
        wakes <= 3
    });
    assert_eq!(fake.checks.load(Ordering::SeqCst), 3);
    assert_eq!(last(&services), Some(T0 + 2 * HOUR_MS));
}

#[test]
fn update_every_finished_check_is_stored_the_button_too() {
    let dir = crate::testutil::TempDir::new("update-record-button");
    let services = crate::testutil::services(&dir);
    let fake = Arc::new(Fake::default());
    *fake.check_answer.lock().unwrap() = Some(Err(UpdateError::NotPublished));
    let j = scheduled(fake, &services, at(T0 + 5), never_notify());
    j.start_check(false).unwrap().join().unwrap();
    assert_eq!(last(&services), Some(T0 + 5), "a failed check counts too");
}

#[test]
fn update_an_automatic_offer_notifies_once_per_version_and_only_unwatched() {
    let dir = crate::testutil::TempDir::new("update-notify");
    let services = crate::testutil::services(&dir);
    let shown = Arc::new(Mutex::new(Vec::<String>::new()));
    let watched = Arc::new(AtomicBool::new(true));
    let (s, w) = (Arc::clone(&shown), Arc::clone(&watched));
    let notify: Notify = Arc::new(move |v: &str| {
        if w.load(Ordering::SeqCst) {
            return false;
        }
        s.lock().unwrap().push(v.to_owned());
        true
    });
    let fake = Arc::new(Fake::default());
    let j = scheduled(Arc::clone(&fake), &services, at(T0), notify);
    let found = |v: &str| *fake.check_answer.lock().unwrap() = Some(Ok(Some((v.into(), 1))));

    // Window watched: the page shows it, no notification, nothing remembered.
    found("9.0.0");
    j.start_check(true).unwrap().join().unwrap();
    assert!(shown.lock().unwrap().is_empty());
    // Not watched: one notification, and not again for the same version.
    watched.store(false, Ordering::SeqCst);
    for _ in 0..2 {
        found("9.0.0");
        j.start_check(true).unwrap().join().unwrap();
    }
    // The button never notifies.
    found("9.1.0");
    j.start_check(false).unwrap().join().unwrap();
    // A newer version does.
    found("9.1.0");
    j.start_check(true).unwrap().join().unwrap();
    assert_eq!(*shown.lock().unwrap(), ["9.0.0", "9.1.0"]);
    assert!(matches!(j.phase(), UpdatePhase::Available { .. }));
}

#[test]
fn update_a_quiet_failure_keeps_the_offer_the_page_shows() {
    let fake = Arc::new(Fake::default());
    let (j, _) = jobs(Arc::clone(&fake));
    available(&j);
    *fake.check_answer.lock().unwrap() = Some(Err(UpdateError::Network("x".into())));
    j.start_check(true).unwrap().join().unwrap();
    assert_eq!(
        j.phase(),
        UpdatePhase::Available {
            version: "9.9.9".into(),
            size: 100
        }
    );
}

#[test]
fn update_view_reads_the_schedule_and_reports_an_unknown_one() {
    let dir = crate::testutil::TempDir::new("update-view-schedule");
    let data = earshot_store::DataDir::new(dir.0.join("data"));
    data.create().unwrap();
    let state = AppState::open(data);
    let (j, _) = jobs(Arc::new(Fake::default()));
    let v = view(&state, &j).unwrap();
    assert_eq!(
        (v.schedule, v.schedule_unknown),
        (ScheduleChoice::Daily, None)
    );
    let services = state.services().unwrap();
    earshot_runtime::update::set_update_schedule(&mut services.store(), UpdateSchedule::Weekly)
        .unwrap();
    assert_eq!(view(&state, &j).unwrap().schedule, ScheduleChoice::Weekly);
    services
        .store()
        .set_local_value(earshot_runtime::update::SCHEDULE_KEY, b"fortnightly")
        .unwrap();
    let v = view(&state, &j).unwrap();
    assert_eq!(v.schedule, ScheduleChoice::Daily);
    assert_eq!(v.schedule_unknown.as_deref(), Some("fortnightly"));
}

#[test]
fn update_schedule_choices_round_trip() {
    for s in UpdateSchedule::ALL {
        assert_eq!(UpdateSchedule::from(ScheduleChoice::from(s)), s);
    }
    assert_eq!(
        serde_json::to_string(&ScheduleChoice::FourHours).unwrap(),
        "\"four_hours\""
    );
}

#[test]
fn update_toast_is_fixed_copy_and_the_version() {
    let t = crate::shell::update_toast("9.1.0");
    assert_eq!(t.title, "An update is available");
    assert_eq!(
        t.body,
        "Earshot 9.1.0 can be downloaded from Settings, General."
    );
    assert!(t.buttons.is_empty() && !t.stays);
}
