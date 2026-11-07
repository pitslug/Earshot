use std::sync::atomic::{AtomicUsize, Ordering};

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
