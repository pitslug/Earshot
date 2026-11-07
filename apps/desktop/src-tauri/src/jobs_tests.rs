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
            }),
            tx,
        )
    }

    fn wait(&self) {
        self.go.lock().unwrap().recv_timeout(GUARD).unwrap();
    }
}

impl JobWork for Fake {
    fn retranscribe(
        &self,
        svc: &Services,
        meeting: EntityId,
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
        self.wait();
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
        warn: &mut dyn FnMut(String),
    ) -> Result<SummaryResult, IpcError> {
        if let Some(n) = &self.summary_note {
            warn(n.clone());
        }
        self.seen
            .lock()
            .unwrap()
            .push(svc.meeting_activity(meeting).unwrap());
        self.wait();
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
        JobRunner::new(work, sink, crate::activity::WorkGate::new()),
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
    let jobs = JobRunner::new(fake, sink, gate.clone());
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
