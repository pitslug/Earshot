//! Job runner tests (T7.3): one job per meeting, leases, events in order. The work is faked
//! (the real re-transcription and summary run in the runtime's own tests).

use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use earshot_core::asr::{BackendInfo, BackendKind};
use earshot_core::model::{Meeting, MeetingRepo, Stamp};
use earshot_core::prompt::Template;
use earshot_runtime::edit::Retranscribed;
use earshot_runtime::host::{HostPost, HostStep, SkipReason, SummaryResult};
use earshot_runtime::post::{PostOutcome, PostStage, PostWarning};
use earshot_runtime::{MeetingActivity, MeetingJob, Services};
use rand_chacha::ChaCha8Rng;
use rand_core::SeedableRng;

use super::*;
use crate::meeting_events::PostStep;
use crate::testutil::{T0, TempDir, services};

/// Bounds a failing case only.
const GUARD: Duration = Duration::from_secs(30);

fn meeting(svc: &Services, seed: u64) -> EntityId {
    let clock = Arc::clone(&svc.clock);
    let mut rng = ChaCha8Rng::seed_from_u64(seed);
    let mut db = svc.store();
    let (store, hlc) = db.parts();
    let mut stamp = Stamp {
        clock: &*clock,
        hlc,
        rng: &mut rng,
    };
    let mut m = Meeting::new("Sync", T0, 600, Template::General, &mut stamp);
    m.duration_ms = Some(60_000);
    let id = m.id;
    store.put_meeting(m).unwrap();
    id
}

/// Work that waits for `go` before it answers, and reports what it saw.
struct Fake {
    go: Mutex<Receiver<()>>,
    /// The meeting's activity while the work ran.
    seen: Mutex<Vec<Option<MeetingActivity>>>,
    fail: bool,
    /// The work panics (after `go`).
    panic: bool,
    /// What the summary setup tells the user (a "where it runs" note).
    summary_note: Option<String>,
    /// What the final pass warns of; with any, the re-transcription ran instead of finding no
    /// audio.
    post_warnings: Vec<PostWarning>,
    /// The work never looks at its cancel (a model load, which cannot be interrupted).
    deaf: bool,
}

impl Fake {
    fn new(fail: bool) -> (Arc<Self>, Sender<()>) {
        Self::with(fail, false)
    }

    fn with(fail: bool, panic: bool) -> (Arc<Self>, Sender<()>) {
        let (tx, rx) = mpsc::channel();
        (
            Arc::new(Self {
                go: Mutex::new(rx),
                seen: Mutex::new(Vec::new()),
                fail,
                panic,
                summary_note: None,
                post_warnings: Vec::new(),
                deaf: false,
            }),
            tx,
        )
    }

    /// Waits for `go`, or answers `cancel` as the final pass and the summary answer theirs
    /// (true: cancelled). A `go` already sent wins: the work had finished.
    fn wait(&self, cancel: &CancelToken) -> bool {
        let go = self.go.lock().unwrap();
        let until = std::time::Instant::now() + GUARD;
        loop {
            match go.recv_timeout(Duration::from_millis(5)) {
                Ok(()) => return false,
                Err(mpsc::RecvTimeoutError::Timeout) if !self.deaf && cancel.is_cancelled() => {
                    return true;
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    assert!(std::time::Instant::now() < until, "nobody said go");
                }
                Err(e) => panic!("{e}"),
            }
        }
    }
}

impl JobWork for Fake {
    fn retranscribe(
        &self,
        svc: &Services,
        meeting: EntityId,
        cancel: &CancelToken,
        progress: &mut dyn FnMut(HostStep<'_>),
        warn: &mut dyn FnMut(String),
    ) -> Result<Retranscribed, IpcError> {
        // The runtime's re-transcription leases the meeting itself.
        let _lease = svc
            .lease_meeting_alone(meeting, MeetingJob::Processing)
            .map_err(IpcError::from)?;
        self.seen
            .lock()
            .unwrap()
            .push(svc.meeting_activity(meeting).unwrap());
        progress(HostStep::PostLoaded { missing: &[] });
        warn("The Cuda library of ONNX Runtime is unavailable.".into());
        progress(HostStep::Post(PostStage::EchoChecked {
            elapsed: Duration::ZERO,
        }));
        if self.wait(cancel) {
            // As `host::final_pass` answers a cancel: nothing stored.
            return Ok(Retranscribed {
                missing_tracks: Vec::new(),
                post: HostPost::Cancelled {
                    speaker_windows_deleted: Ok(0),
                },
                summary_stale: false,
            });
        }
        assert!(!self.panic, "a bug in the work");
        if self.fail {
            return Err(IpcError {
                kind: "model".into(),
                message: "The model is missing.".into(),
                retryable: false,
            });
        }
        if !self.post_warnings.is_empty() {
            return Ok(Retranscribed {
                missing_tracks: Vec::new(),
                post: HostPost::Done(Box::new(PostOutcome {
                    meeting,
                    speakers: Vec::new(),
                    final_segments: 1,
                    enrolled_me: None,
                    backend: BackendInfo {
                        kind: BackendKind::Cpu,
                        device: "cpu".into(),
                        model: "fake".into(),
                    },
                    warnings: self.post_warnings.clone(),
                    echo: earshot_runtime::echo::EchoReport::default(),
                    stretches: Vec::new(),
                    speaker_windows: Vec::new(),
                })),
                summary_stale: false,
            });
        }
        Ok(Retranscribed {
            missing_tracks: vec![earshot_core::capture::TrackId::Reference],
            post: HostPost::NothingToProcess,
            summary_stale: true,
        })
    }

    fn summarise(
        &self,
        svc: &Services,
        meeting: EntityId,
        cancel: &CancelToken,
        warn: &mut dyn FnMut(String),
    ) -> Result<SummaryResult, IpcError> {
        if let Some(n) = &self.summary_note {
            warn(n.clone());
        }
        self.seen
            .lock()
            .unwrap()
            .push(svc.meeting_activity(meeting).unwrap());
        if self.wait(cancel) {
            // As `host::run_summary` answers a cancel: nothing stored.
            return Ok(SummaryResult::Cancelled);
        }
        assert!(!self.panic, "a bug in the summary");
        Ok(SummaryResult::Skipped {
            reason: SkipReason::NotRequested,
        })
    }
}

fn runner(work: Arc<Fake>) -> (JobRunner, Receiver<JobEvent>) {
    let (tx, rx) = mpsc::channel();
    let tx = Mutex::new(tx);
    let sink: JobSink = Arc::new(move |e| {
        let _ = tx.lock().unwrap().send(e);
    });
    (
        JobRunner::new(
            work,
            sink,
            crate::activity::WorkGate::new(),
            &crate::cuda_settings::CudaLatch::resolved_off(),
        ),
        rx,
    )
}

#[test]
fn jobs_resummarise_holds_the_meeting_and_reports_the_summary() {
    let dir = TempDir::new("jobs-sum");
    let svc = services(&dir);
    let m = meeting(&svc, 1);
    let (fake, go) = Fake::new(false);
    let (jobs, rx) = runner(Arc::clone(&fake));
    jobs.start(&svc, m, JobKind::Resummarise).unwrap();
    // While it runs: a second job is busy and a deletion is refused.
    let e = jobs.start(&svc, m, JobKind::Retranscribe).unwrap_err();
    assert_eq!(e.kind, "busy");
    assert!(e.retryable);
    let e: IpcError = svc.delete_meeting(m).unwrap_err().into();
    assert_eq!(e.kind, "meeting_busy");
    go.send(()).unwrap();
    let event = rx.recv_timeout(GUARD).unwrap();
    assert_eq!(
        event,
        JobEvent::Summary {
            meeting: m.to_string(),
            result: SummaryEvent::Skipped {
                reason: "Summaries are turned off.".into()
            },
            note: None,
        }
    );
    jobs.join();
    assert_eq!(
        *fake.seen.lock().unwrap(),
        [Some(MeetingActivity::Processing)]
    );
    // Afterwards the meeting is free again.
    assert!(!jobs.running(m));
    svc.delete_meeting(m).unwrap();
}

/// A meeting driver with no meeting (its pre-flight is never asked).
fn idle_driver() -> crate::meeting::MeetingDriver {
    struct NoMeeting;
    impl crate::meeting::Prepare for NoMeeting {
        fn prepare(
            &self,
            _: &Services,
            _: &crate::meeting::StartRequest,
            _: &Arc<earshot_runtime::meeting_queue::QueueTicket>,
        ) -> Result<crate::meeting::Prepared, IpcError> {
            unreachable!("no meeting starts here")
        }
    }
    crate::meeting::MeetingDriver::new(
        Arc::new(NoMeeting),
        Arc::new(|_| {}),
        Arc::new(std::time::Instant::now),
        crate::activity::WorkGate::new(),
        &crate::cuda_settings::CudaLatch::resolved_off(),
    )
}

/// Every event up to and including the first terminal one, then checks nothing follows.
fn until_terminal(rx: &Receiver<JobEvent>) -> (Vec<JobEvent>, JobEvent) {
    let mut steps = Vec::new();
    loop {
        let e = rx.recv_timeout(GUARD).unwrap();
        if matches!(e, JobEvent::Progress { .. } | JobEvent::Warning { .. }) {
            steps.push(e);
        } else {
            return (steps, e);
        }
    }
}

/// Waits until the work of `job` has reached its wait (its steps before it were sent).
fn wait_for_the_work(rx: &Receiver<JobEvent>, job: JobKind) {
    if job == JobKind::Retranscribe {
        // Progress, Warning, Progress: the fake waits after its last step.
        for _ in 0..3 {
            rx.recv_timeout(GUARD).unwrap();
        }
    }
}

#[test]
fn jobs_cancel_ends_a_running_job_with_one_cancelled_event_and_frees_the_meeting() {
    // T12.41: the job's own token reaches the work, which answers it as the final pass and the
    // summary do; the runner sends exactly one terminal event and lets the meeting go.
    let dir = TempDir::new("jobs-cancel");
    let svc = services(&dir);
    for (job, seed) in [(JobKind::Retranscribe, 21), (JobKind::Resummarise, 22)] {
        let m = meeting(&svc, seed);
        let (fake, _go) = Fake::new(false);
        let (jobs, rx) = runner(Arc::clone(&fake));
        jobs.start(&svc, m, job).unwrap();
        wait_for_the_work(&rx, job);
        assert_eq!(jobs.cancel(m), Some(job));
        let (_, last) = until_terminal(&rx);
        assert_eq!(
            last,
            JobEvent::Cancelled {
                meeting: m.to_string(),
                job,
            }
        );
        jobs.join();
        assert!(rx.try_recv().is_err(), "exactly one terminal event");
        assert!(!jobs.running(m));
        assert_eq!(jobs.cancel(m), None, "a second press changes nothing");
        // The work held the meeting while it ran, and let it go when it answered the cancel.
        assert_eq!(
            *fake.seen.lock().unwrap(),
            [Some(MeetingActivity::Processing)]
        );
        assert_ne!(
            svc.meeting_activity(m).unwrap(),
            Some(MeetingActivity::Processing)
        );
        svc.delete_meeting(m).unwrap();
    }
}

#[test]
fn jobs_cancel_after_the_job_finished_changes_nothing() {
    let dir = TempDir::new("jobs-cancel-late");
    let svc = services(&dir);
    for (job, seed) in [(JobKind::Retranscribe, 23), (JobKind::Resummarise, 24)] {
        let m = meeting(&svc, seed);
        let (fake, go) = Fake::new(false);
        let (jobs, rx) = runner(fake);
        // No job yet: nothing to cancel.
        assert_eq!(jobs.cancel(m), None);
        jobs.start(&svc, m, job).unwrap();
        go.send(()).unwrap();
        let (_, last) = until_terminal(&rx);
        jobs.join();
        assert_eq!(jobs.cancel(m), None);
        assert!(
            matches!(
                last,
                JobEvent::Retranscribed { .. } | JobEvent::Summary { .. }
            ),
            "{last:?}"
        );
        assert!(rx.try_recv().is_err(), "no second terminal event");
        // On the same runner: a job cancelled while it runs, then a new job, which gets a token
        // of its own (a reused, already-set token would cancel it at once).
        jobs.start(&svc, m, job).unwrap();
        wait_for_the_work(&rx, job);
        assert_eq!(jobs.cancel(m), Some(job));
        let (_, last) = until_terminal(&rx);
        assert!(matches!(last, JobEvent::Cancelled { .. }), "{last:?}");
        jobs.join();
        jobs.start(&svc, m, job).unwrap();
        go.send(()).unwrap();
        let (_, last) = until_terminal(&rx);
        jobs.join();
        assert!(!matches!(last, JobEvent::Cancelled { .. }), "{last:?}");
    }
}

#[test]
fn jobs_a_meeting_with_a_job_running_is_deleted_only_after_the_user_agreed_to_stop_it() {
    // T12.38 then T12.41: without the user's agreement a deletion is refused with the job named;
    // with it, the job is cancelled, ends in its one Cancelled event, and the meeting is deleted.
    use crate::deletion::{DeleteAsk, ask, delete, job_copy};
    let dir = TempDir::new("jobs-delete");
    let svc = services(&dir);
    let driver = idle_driver();
    for (job, asked, seed) in [
        (JobKind::Resummarise, DeleteAsk::Resummarising, 11),
        (JobKind::Retranscribe, DeleteAsk::Retranscribing, 12),
    ] {
        let m = meeting(&svc, seed);
        let (fake, _go) = Fake::new(false);
        let (jobs, rx) = runner(Arc::clone(&fake));
        jobs.start(&svc, m, job).unwrap();
        wait_for_the_work(&rx, job);
        assert_eq!(ask(&svc, &driver, &jobs, m).unwrap(), asked);
        let e = delete(&svc, &driver, &jobs, m, false, GUARD).unwrap_err();
        assert_eq!(e.kind, "meeting_job");
        assert_eq!(e.message, job_copy(job));
        assert!(e.retryable);
        assert!(jobs.running(m), "a refusal stops nothing");

        delete(&svc, &driver, &jobs, m, true, GUARD).unwrap();
        let (_, last) = until_terminal(&rx);
        assert_eq!(
            last,
            JobEvent::Cancelled {
                meeting: m.to_string(),
                job,
            }
        );
        jobs.join();
        assert!(rx.try_recv().is_err(), "exactly one terminal event");
        assert_eq!(svc.meeting_activity(m).unwrap(), None);
        assert!(svc.store().meeting(m).unwrap().unwrap().meta.deleted);
    }
}

#[test]
fn jobs_a_job_that_does_not_answer_its_cancel_in_time_keeps_the_meeting() {
    // A model load cannot be interrupted: past the wait the deletion is refused as retryable,
    // the job already cancelled, and nothing is deleted.
    use crate::deletion::{STILL_STOPPING, delete};
    let dir = TempDir::new("jobs-delete-slow");
    let svc = services(&dir);
    let driver = idle_driver();
    let m = meeting(&svc, 13);
    let (mut fake, go) = Fake::new(false);
    Arc::get_mut(&mut fake).unwrap().deaf = true;
    let (jobs, rx) = runner(Arc::clone(&fake));
    jobs.start(&svc, m, JobKind::Resummarise).unwrap();
    let e = delete(&svc, &driver, &jobs, m, true, Duration::from_millis(200)).unwrap_err();
    assert_eq!(
        (e.kind.as_str(), e.message.as_str(), e.retryable),
        ("meeting_stopping", STILL_STOPPING, true)
    );
    assert!(!svc.store().meeting(m).unwrap().unwrap().meta.deleted);
    go.send(()).unwrap();
    let (_, last) = until_terminal(&rx);
    jobs.join();
    // The work finished before it looked at the token again: its result stands.
    assert!(matches!(last, JobEvent::Summary { .. }), "{last:?}");
}

#[test]
fn jobs_retranscribe_carries_the_final_pass_warnings_on_its_last_event() {
    let dir = TempDir::new("jobs-re-warn");
    let svc = services(&dir);
    let m = meeting(&svc, 12);
    let (mut fake, go) = Fake::new(false);
    let warnings = vec![PostWarning::SkippedVoiceprints(1)];
    let expected: Vec<String> = crate::plain_text::post_warning_copy(&warnings)
        .into_iter()
        .map(|(_, text)| text)
        .collect();
    assert_eq!(expected.len(), 1);
    Arc::get_mut(&mut fake).unwrap().post_warnings = warnings;
    let (jobs, rx) = runner(fake);
    jobs.start(&svc, m, JobKind::Retranscribe).unwrap();
    go.send(()).unwrap();
    let mut seen = Vec::new();
    let last = loop {
        match rx.recv_timeout(GUARD).unwrap() {
            JobEvent::Warning { message, .. } => seen.push(message),
            JobEvent::Progress { .. } => {}
            other => break other,
        }
    };
    jobs.join();
    // The final pass's warning is not sent ahead, where the terminal event would replace it.
    assert!(!seen.iter().any(|m| expected.contains(m)));
    assert_eq!(
        last,
        JobEvent::Retranscribed {
            meeting: m.to_string(),
            missing_tracks: 0,
            no_audio: false,
            summary_stale: false,
            warnings: expected,
        }
    );
}

#[test]
fn jobs_retranscribe_reports_progress_then_its_result() {
    let dir = TempDir::new("jobs-re");
    let svc = services(&dir);
    let m = meeting(&svc, 2);
    let (fake, go) = Fake::new(false);
    let (jobs, rx) = runner(Arc::clone(&fake));
    jobs.start(&svc, m, JobKind::Retranscribe).unwrap();
    go.send(()).unwrap();
    let mut events = Vec::new();
    loop {
        let e = rx.recv_timeout(GUARD).unwrap();
        let last = !matches!(e, JobEvent::Progress { .. } | JobEvent::Warning { .. });
        events.push(e);
        if last {
            break;
        }
    }
    jobs.join();
    let id = m.to_string();
    assert_eq!(
        events,
        [
            JobEvent::Progress {
                meeting: id.clone(),
                step: PostStep::Loading,
                done: 0,
                total: None,
            },
            JobEvent::Warning {
                meeting: id.clone(),
                message: "The Cuda library of ONNX Runtime is unavailable.".into(),
            },
            JobEvent::Progress {
                meeting: id.clone(),
                step: PostStep::EchoChecked,
                done: 1,
                total: None,
            },
            JobEvent::Retranscribed {
                meeting: id,
                missing_tracks: 1,
                no_audio: true,
                summary_stale: true,
                warnings: Vec::new(),
            },
        ]
    );
    // The runner's own check lease was gone before the work took its lease.
    assert_eq!(
        *fake.seen.lock().unwrap(),
        [Some(MeetingActivity::Processing)]
    );
}

#[test]
fn jobs_a_failed_job_is_one_failed_event_and_frees_the_meeting() {
    let dir = TempDir::new("jobs-fail");
    let svc = services(&dir);
    let m = meeting(&svc, 3);
    let (fake, go) = Fake::new(true);
    let (jobs, rx) = runner(fake);
    jobs.start(&svc, m, JobKind::Retranscribe).unwrap();
    go.send(()).unwrap();
    let last = loop {
        let e = rx.recv_timeout(GUARD).unwrap();
        if !matches!(e, JobEvent::Progress { .. } | JobEvent::Warning { .. }) {
            break e;
        }
    };
    assert_eq!(
        last,
        JobEvent::Failed {
            meeting: m.to_string(),
            job: JobKind::Retranscribe,
            message: "The model is missing.".into(),
            retryable: false,
        }
    );
    jobs.join();
    assert!(!jobs.running(m));
}

#[test]
fn jobs_refuse_a_meeting_that_records() {
    let dir = TempDir::new("jobs-rec");
    let svc = services(&dir);
    let m = meeting(&svc, 4);
    let (fake, _go) = Fake::new(false);
    let (jobs, _rx) = runner(fake);
    let lease = svc.lease_meeting_for(m, MeetingJob::Recording).unwrap();
    for job in [JobKind::Retranscribe, JobKind::Resummarise] {
        let e = jobs.start(&svc, m, job).unwrap_err();
        assert_eq!(e.kind, "meeting_busy");
        assert!(e.retryable);
    }
    drop(lease);
    assert!(!jobs.running(m));
}

#[test]
fn jobs_a_panicking_job_ends_with_one_failed_event_and_frees_the_meeting() {
    let dir = TempDir::new("jobs-panic");
    let svc = services(&dir);
    for (seed, job) in [(5, JobKind::Retranscribe), (6, JobKind::Resummarise)] {
        let m = meeting(&svc, seed);
        let (fake, go) = Fake::with(false, true);
        let (jobs, rx) = runner(fake);
        jobs.start(&svc, m, job).unwrap();
        go.send(()).unwrap();
        let mut terminal = Vec::new();
        while let Ok(e) = rx.recv_timeout(GUARD) {
            if !matches!(e, JobEvent::Progress { .. } | JobEvent::Warning { .. }) {
                terminal.push(e);
            }
            if !terminal.is_empty() {
                break;
            }
        }
        jobs.join();
        assert_eq!(
            terminal,
            [JobEvent::Failed {
                meeting: m.to_string(),
                job,
                message: JOB_PANIC_MESSAGE.into(),
                retryable: false,
            }]
        );
        assert_eq!(JOB_PANIC_MESSAGE, "The job stopped unexpectedly.");
        assert!(rx.try_recv().is_err(), "exactly one terminal event");
        assert!(!jobs.running(m));
        // The lease is gone: the meeting can be deleted.
        svc.delete_meeting(m).unwrap();
    }
}

#[test]
fn jobs_resummarise_tells_a_setup_note_before_the_summary() {
    let dir = TempDir::new("jobs-note");
    let svc = services(&dir);
    let m = meeting(&svc, 7);
    let (tx, rx_go) = mpsc::channel();
    let note = "Qwen 3.5 9B is too big for your graphics card, so summaries run on the \
                processor and take longer.";
    let fake = Arc::new(Fake {
        go: Mutex::new(rx_go),
        seen: Mutex::new(Vec::new()),
        fail: false,
        panic: false,
        summary_note: Some(note.into()),
        post_warnings: Vec::new(),
        deaf: false,
    });
    let (jobs, rx) = runner(fake);
    jobs.start(&svc, m, JobKind::Resummarise).unwrap();
    tx.send(()).unwrap();
    let first = rx.recv_timeout(GUARD).unwrap();
    assert_eq!(
        first,
        JobEvent::Warning {
            meeting: m.to_string(),
            message: note.into(),
        }
    );
    assert!(matches!(
        rx.recv_timeout(GUARD).unwrap(),
        JobEvent::Summary { .. }
    ));
    jobs.join();
    assert!(rx.try_recv().is_err());
}

/// Model check work that is never reached (the gate refuses it first).
struct NoCheck;

impl crate::model_jobs::ModelWork for NoCheck {
    fn download(
        &self,
        _: &Services,
        _: &str,
        _: &earshot_core::asr::CancelToken,
        _: &mut dyn FnMut(u64, u64),
    ) -> Result<(), crate::model_jobs::DownloadEnd> {
        Ok(())
    }

    fn check(
        &self,
        _: &Services,
        _: &earshot_core::asr::CancelToken,
        _: &mut dyn FnMut(crate::model_jobs::CheckStep),
        _: &mut dyn FnMut(String),
    ) -> Result<crate::model_jobs::CheckDone, IpcError> {
        Ok(crate::model_jobs::CheckDone {
            answer: None,
            trials: 0,
        })
    }
}

#[test]
fn jobs_and_the_model_check_exclude_each_other() {
    use crate::activity::{CHECK_RUNNING, WORK_RUNNING, WorkGate};
    let dir = TempDir::new("jobs-gate");
    let svc = services(&dir);
    let m = meeting(&svc, 8);
    let gate = WorkGate::new();
    let (fake, go) = Fake::new(false);
    let (tx, rx) = mpsc::channel();
    let tx = Mutex::new(tx);
    let sink: JobSink = Arc::new(move |e| {
        let _ = tx.lock().unwrap().send(e);
    });
    let jobs = JobRunner::new(
        fake,
        sink,
        gate.clone(),
        &crate::cuda_settings::CudaLatch::resolved_off(),
    );
    let checks =
        crate::model_jobs::ModelJobs::new(Arc::new(NoCheck), Arc::new(|_| {}), gate.clone());
    // A check runs: no job starts, and the meeting is not leased.
    let check = gate.enter_check().unwrap();
    for job in [JobKind::Retranscribe, JobKind::Resummarise] {
        let e = jobs.start(&svc, m, job).unwrap_err();
        assert_eq!(
            (e.kind.as_str(), e.message.as_str(), e.retryable),
            ("busy", CHECK_RUNNING, true)
        );
    }
    assert!(!jobs.running(m));
    drop(check);
    // A job runs: the check is refused until the job's thread has ended.
    jobs.start(&svc, m, JobKind::Resummarise).unwrap();
    let e = checks.start_check(&svc).unwrap_err();
    assert_eq!(
        (e.kind.as_str(), e.message.as_str(), e.retryable),
        ("busy", WORK_RUNNING, true)
    );
    go.send(()).unwrap();
    rx.recv_timeout(GUARD).unwrap();
    jobs.join();
    checks.start_check(&svc).unwrap();
    checks.join();
}

/// T14.12: a re-transcription keeps the model the settings gave it in use from the read of
/// the settings until the job lets go of it, also once the settings name another model.
#[test]
fn job_retranscription_keeps_its_model_in_use() {
    use earshot_runtime::backends::{Accelerator, ModelDir};
    use earshot_runtime::host;
    use earshot_runtime::model_settings::{ModelPick, model_settings, set_model_settings};

    let dir = TempDir::new("jobs-retranscribe-in-use");
    let svc = services(&dir);
    let models = ModelDir::new(dir.0.join("models"));
    let choose = |model: &str| {
        let mut db = svc.store();
        let (mut settings, _) = model_settings(&db).unwrap();
        settings.transcription = ModelPick::Named(model.to_owned());
        set_model_settings(&mut db, &settings).unwrap();
    };
    choose("small.en");
    assert!(host::models_in_use(&models).is_empty());
    let (setup, model_use) = retranscribe_setup(&svc, &models, &[Accelerator::Cpu]).unwrap();
    assert_eq!(setup.choice.model, "small.en");
    assert_eq!(model_use.model(), "small.en");
    assert_eq!(host::models_in_use(&models), ["small.en"]);
    // The settings move on while the job runs: its model is still in use.
    choose("tiny.en");
    assert_eq!(host::models_in_use(&models), ["small.en"]);
    drop(model_use);
    assert!(host::models_in_use(&models).is_empty());
}

/// T14.12: a re-summary keeps in use the local summary model a removal keeps for summaries,
/// until the job lets go of it.
#[test]
fn job_resummary_keeps_the_local_summary_model_in_use() {
    use earshot_runtime::backends::ModelDir;
    use earshot_runtime::host;

    let dir = TempDir::new("jobs-resummary-in-use");
    let svc = services(&dir);
    let models = ModelDir::new(dir.0.join("models"));
    let hardware = earshot_runtime::hardware();
    let expected = {
        let db = svc.store();
        let placement = host::local_summary_placement(
            &db,
            &earshot_runtime::backends::accelerator_order(),
            &hardware,
        )
        .unwrap();
        host::summary_model_for(&placement, &hardware)
            .unwrap()
            .model
            .id
            .to_owned()
    };
    let model_use = summary_model_use(&svc, &models).expect("a shipped summary model");
    assert_eq!(model_use.model(), expected);
    assert_eq!(host::models_in_use(&models), [expected.clone()]);
    // It is the model the settings' own protection names for summaries.
    let kept = host::protected_models(
        &svc.store(),
        &models,
        &earshot_runtime::backends::accelerator_order(),
        &hardware,
    )
    .unwrap();
    assert!(kept.contains(&expected), "{kept:?}");
    drop(model_use);
    assert!(host::models_in_use(&models).is_empty());
}
