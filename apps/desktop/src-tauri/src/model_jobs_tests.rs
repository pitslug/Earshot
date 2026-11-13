//! Model job tests (T7.4e): one download per model, one check at a time, events in order,
//! exactly one terminal event (also on a panic). The work is faked: no network, no models.

use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use super::*;
use crate::testutil::{TempDir, services};

/// Bounds a failing case only.
const GUARD: Duration = Duration::from_secs(30);

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Succeed,
    Fail,
    Panic,
}

/// Work that waits for `go` (or a cancel) before it ends.
struct Fake {
    go: Mutex<Receiver<()>>,
    mode: Mode,
    /// The check's library fallback, told before the trials.
    fallback: Option<String>,
    /// A transcription model is installed, so the speech helpers' arrival starts the check.
    model_installed: bool,
}

impl Fake {
    fn new(mode: Mode) -> (Arc<Self>, Sender<()>) {
        let (tx, rx) = mpsc::channel();
        (
            Arc::new(Self {
                go: Mutex::new(rx),
                mode,
                fallback: None,
                model_installed: false,
            }),
            tx,
        )
    }

    /// As [`Fake::new`] on a machine with a transcription model installed.
    fn with_a_model_installed(mode: Mode) -> (Arc<Self>, Sender<()>) {
        let (tx, rx) = mpsc::channel();
        (
            Arc::new(Self {
                go: Mutex::new(rx),
                mode,
                fallback: None,
                model_installed: true,
            }),
            tx,
        )
    }

    /// Waits for `go`; `false` when `cancel` was set first.
    fn wait(&self, cancel: Option<&CancelToken>) -> bool {
        let start = Instant::now();
        let go = self.go.lock().unwrap();
        loop {
            if cancel.is_some_and(CancelToken::is_cancelled) {
                return false;
            }
            match go.recv_timeout(Duration::from_millis(5)) {
                Ok(()) => return true,
                Err(RecvTimeoutError::Timeout) => {
                    assert!(start.elapsed() < GUARD, "the test never said go");
                }
                Err(RecvTimeoutError::Disconnected) => panic!("go dropped"),
            }
        }
    }
}

impl ModelWork for Fake {
    fn download(
        &self,
        _: &Services,
        model: &str,
        cancel: &CancelToken,
        progress: &mut dyn FnMut(u64, u64),
    ) -> Result<(), DownloadEnd> {
        progress(0, 10);
        progress(4, 10);
        if !self.wait(Some(cancel)) {
            return Err(DownloadEnd::Cancelled);
        }
        match self.mode {
            Mode::Succeed => {
                progress(10, 10);
                Ok(())
            }
            Mode::Fail => Err(DownloadEnd::Failed(IpcError {
                kind: "download".into(),
                message: format!("GET {model}: connection lost; run again to resume"),
                retryable: true,
            })),
            Mode::Panic => panic!("a bug in the download"),
        }
    }

    fn check(
        &self,
        _: &Services,
        cancel: &CancelToken,
        progress: &mut dyn FnMut(CheckStep),
        warn: &mut dyn FnMut(String),
    ) -> Result<CheckDone, IpcError> {
        if let Some(m) = &self.fallback {
            warn(m.clone());
        }
        progress(CheckStep {
            model: "parakeet-tdt-0.6b-v2".into(),
            runs_on: RunsOn::Npu,
            verdict: None,
        });
        if !self.wait(Some(cancel)) {
            // What run_child returns once the user cancelled (the child is killed).
            return Err(crate::check_child::cancelled());
        }
        progress(CheckStep {
            model: "parakeet-tdt-0.6b-v2".into(),
            runs_on: RunsOn::Npu,
            verdict: Some(Verdict::KeepsUp),
        });
        match self.mode {
            Mode::Succeed => Ok(CheckDone {
                answer: Some(("parakeet-tdt-0.6b-v2".into(), RunsOn::Npu)),
                trials: 1,
            }),
            Mode::Fail => Err(IpcError {
                kind: "backend".into(),
                message: "ONNX Runtime is missing".into(),
                retryable: false,
            }),
            Mode::Panic => panic!("a bug in the check"),
        }
    }

    // Never the real models folder: what is installed on this machine must not decide a test.
    fn check_follows(&self, _: &Services, model: &str) -> bool {
        is_kind(model, ModelKind::Transcription)
            || (is_kind(model, ModelKind::Helpers) && self.model_installed)
    }
}

fn runner(work: Arc<Fake>) -> (ModelJobs, Receiver<ModelJobEvent>) {
    let (tx, rx) = mpsc::channel();
    let tx = Mutex::new(tx);
    let sink: ModelSink = Arc::new(move |e| {
        let _ = tx.lock().unwrap().send(e);
    });
    (
        ModelJobs::new(work, sink, crate::activity::WorkGate::new()),
        rx,
    )
}

fn is_terminal(e: &ModelJobEvent) -> bool {
    !matches!(
        e,
        ModelJobEvent::Progress { .. }
            | ModelJobEvent::CheckProgress { .. }
            | ModelJobEvent::Warning { .. }
    )
}

/// Events until (and with) the first terminal one; then checks nothing else arrives.
fn until_end(rx: &Receiver<ModelJobEvent>, jobs: &ModelJobs) -> Vec<ModelJobEvent> {
    let mut out = Vec::new();
    loop {
        let e = rx.recv_timeout(GUARD).expect("a terminal event");
        let end = is_terminal(&e);
        out.push(e);
        if end {
            break;
        }
    }
    jobs.join();
    assert!(rx.try_recv().is_err(), "exactly one terminal event");
    out
}

const MODEL: &str = "base.en";
/// Summary models: their downloads start no check, so download-only tests use them.
const SUMMARY: &str = earshot_runtime::summary_model::LOW_MEMORY_SUMMARY_MODEL;
const OTHER_SUMMARY: &str = earshot_runtime::summary_model::DEFAULT_SUMMARY_MODEL;

#[test]
fn model_job_download_reports_progress_then_one_downloaded() {
    let dir = TempDir::new("model-jobs");
    let svc = services(&dir);
    let (fake, go) = Fake::new(Mode::Succeed);
    let (jobs, rx) = runner(fake);
    jobs.start_download(&svc, SUMMARY).unwrap();
    assert!(jobs.downloading(SUMMARY));
    go.send(()).unwrap();
    let progress = |done| ModelJobEvent::Progress {
        model: SUMMARY.into(),
        done,
        total: 10,
    };
    assert_eq!(
        until_end(&rx, &jobs),
        [
            progress(0),
            progress(4),
            progress(10),
            ModelJobEvent::Downloaded {
                model: SUMMARY.into()
            }
        ]
    );
    assert!(!jobs.downloading(SUMMARY));
}

#[test]
fn model_job_one_download_per_model() {
    let dir = TempDir::new("model-jobs");
    let svc = services(&dir);
    let (fake, go) = Fake::new(Mode::Succeed);
    let (jobs, rx) = runner(fake);
    jobs.start_download(&svc, SUMMARY).unwrap();
    let e = jobs.start_download(&svc, SUMMARY).unwrap_err();
    assert_eq!(e.kind, "busy");
    assert!(e.retryable);
    // Another model downloads beside it.
    jobs.start_download(&svc, OTHER_SUMMARY).unwrap();
    go.send(()).unwrap();
    go.send(()).unwrap();
    let mut ends = Vec::new();
    while ends.len() < 2 {
        let e = rx.recv_timeout(GUARD).unwrap();
        if is_terminal(&e) {
            ends.push(e);
        }
    }
    jobs.join();
    ends.sort_by_key(|e| format!("{e:?}"));
    assert_eq!(
        ends,
        [
            ModelJobEvent::Downloaded {
                model: SUMMARY.into()
            },
            ModelJobEvent::Downloaded {
                model: OTHER_SUMMARY.into()
            }
        ]
    );
    // Finished: the model can be downloaded again.
    jobs.start_download(&svc, SUMMARY).unwrap();
    go.send(()).unwrap();
    assert!(matches!(
        until_end(&rx, &jobs).last(),
        Some(ModelJobEvent::Downloaded { .. })
    ));
}

#[test]
fn model_job_cancel_ends_with_one_cancelled() {
    let dir = TempDir::new("model-jobs");
    let svc = services(&dir);
    let (fake, _go) = Fake::new(Mode::Succeed);
    let (jobs, rx) = runner(fake);
    assert_eq!(
        jobs.cancel_download(MODEL).unwrap_err().kind,
        "not_downloading"
    );
    jobs.start_download(&svc, MODEL).unwrap();
    jobs.cancel_download(MODEL).unwrap();
    let events = until_end(&rx, &jobs);
    assert_eq!(
        events.last(),
        Some(&ModelJobEvent::Cancelled {
            model: MODEL.into()
        })
    );
    assert!(
        events[..events.len() - 1]
            .iter()
            .all(|e| matches!(e, ModelJobEvent::Progress { .. }))
    );
}

#[test]
fn model_job_failed_and_panicking_downloads_end_with_one_failed() {
    let dir = TempDir::new("model-jobs");
    let svc = services(&dir);
    for (mode, message, retryable) in [
        (
            Mode::Fail,
            "GET base.en: connection lost; run again to resume",
            true,
        ),
        (Mode::Panic, MODEL_JOB_PANIC_MESSAGE, false),
    ] {
        let (fake, go) = Fake::new(mode);
        let (jobs, rx) = runner(fake);
        jobs.start_download(&svc, MODEL).unwrap();
        go.send(()).unwrap();
        assert_eq!(
            until_end(&rx, &jobs).last(),
            Some(&ModelJobEvent::Failed {
                model: Some(MODEL.into()),
                message: message.into(),
                retryable,
            })
        );
        // The model is free again.
        assert!(!jobs.downloading(MODEL));
    }
}

#[test]
fn model_job_unknown_model_is_refused() {
    let dir = TempDir::new("model-jobs");
    let svc = services(&dir);
    let (fake, _go) = Fake::new(Mode::Succeed);
    let (jobs, rx) = runner(fake);
    for name in ["whisper-v9", "silero"] {
        assert_eq!(
            jobs.start_download(&svc, name).unwrap_err().kind,
            "unknown_model"
        );
    }
    assert!(rx.try_recv().is_err());
    // Summary models are catalogue models too.
    require_catalogue_model(earshot_runtime::summary_model::LOW_MEMORY_SUMMARY_MODEL).unwrap();
}

#[test]
fn model_job_check_reports_trials_then_one_checked_and_is_busy_meanwhile() {
    let dir = TempDir::new("model-jobs");
    let svc = services(&dir);
    let (fake, go) = Fake::new(Mode::Succeed);
    let (jobs, rx) = runner(fake);
    jobs.start_check(&svc).unwrap();
    let e = jobs.start_check(&svc).unwrap_err();
    assert_eq!((e.kind.as_str(), e.retryable), ("busy", true));
    go.send(()).unwrap();
    let step = |verdict| ModelJobEvent::CheckProgress {
        model: "parakeet-tdt-0.6b-v2".into(),
        runs_on: RunsOn::Npu,
        verdict,
    };
    assert_eq!(
        until_end(&rx, &jobs),
        [
            step(None),
            step(Some(Verdict::KeepsUp)),
            ModelJobEvent::Checked {
                model: Some("parakeet-tdt-0.6b-v2".into()),
                runs_on: Some(RunsOn::Npu),
                trials: 1
            }
        ]
    );
    // Over: it can run again.
    jobs.start_check(&svc).unwrap();
    go.send(()).unwrap();
    assert!(matches!(
        until_end(&rx, &jobs).last(),
        Some(ModelJobEvent::Checked { .. })
    ));
}

#[test]
fn model_job_failed_and_panicking_checks_end_with_one_failed() {
    let dir = TempDir::new("model-jobs");
    let svc = services(&dir);
    for (mode, message) in [
        (Mode::Fail, "ONNX Runtime is missing"),
        (Mode::Panic, MODEL_JOB_PANIC_MESSAGE),
    ] {
        let (fake, go) = Fake::new(mode);
        let (jobs, rx) = runner(fake);
        jobs.start_check(&svc).unwrap();
        go.send(()).unwrap();
        assert_eq!(
            until_end(&rx, &jobs).last(),
            Some(&ModelJobEvent::Failed {
                model: None,
                message: message.into(),
                retryable: false,
            })
        );
    }
}

#[test]
fn model_job_check_library_fallback_is_one_warning_before_the_trials() {
    let dir = TempDir::new("model-jobs");
    let svc = services(&dir);
    let message = check_fallback_message(
        earshot_runtime::backends::Accelerator::Qnn,
        &earshot_runtime::RuntimeError::Unavailable("QNN needs a build with the `qnn` feature"),
    );
    assert_eq!(
        message,
        "The Qnn library of ONNX Runtime is unavailable, so the model check tests Parakeet \
         models on the processor only (unavailable: QNN needs a build with the `qnn` feature)."
    );
    for mode in [Mode::Succeed, Mode::Fail, Mode::Panic] {
        let (tx, rx_go) = mpsc::channel();
        let fake = Arc::new(Fake {
            go: Mutex::new(rx_go),
            mode,
            fallback: Some(message.clone()),
            model_installed: false,
        });
        let (jobs, rx) = runner(fake);
        jobs.start_check(&svc).unwrap();
        tx.send(()).unwrap();
        let events = until_end(&rx, &jobs);
        assert_eq!(
            events[0],
            ModelJobEvent::Warning {
                model: None,
                message: message.clone()
            }
        );
        let warnings = events
            .iter()
            .filter(|e| matches!(e, ModelJobEvent::Warning { .. }))
            .count();
        assert_eq!(warnings, 1);
        assert!(matches!(
            events[1],
            ModelJobEvent::CheckProgress { verdict: None, .. }
        ));
        let terminal: Vec<&ModelJobEvent> = events.iter().filter(|e| is_terminal(e)).collect();
        assert_eq!(terminal.len(), 1, "{events:?}");
        assert!(matches!(
            (mode, terminal[0]),
            (Mode::Succeed, ModelJobEvent::Checked { .. })
                | (
                    Mode::Fail | Mode::Panic,
                    ModelJobEvent::Failed { model: None, .. }
                )
        ));
    }
}

fn runner_with_gate(
    work: Arc<Fake>,
    gate: crate::activity::WorkGate,
) -> (ModelJobs, Receiver<ModelJobEvent>) {
    let (tx, rx) = mpsc::channel();
    let tx = Mutex::new(tx);
    let sink: ModelSink = Arc::new(move |e| {
        let _ = tx.lock().unwrap().send(e);
    });
    (ModelJobs::new(work, sink, gate), rx)
}

/// Every event until `n` terminal ones; then checks nothing else arrives.
fn until_ends(rx: &Receiver<ModelJobEvent>, jobs: &ModelJobs, n: usize) -> Vec<ModelJobEvent> {
    let mut out = Vec::new();
    let mut ends = 0;
    while ends < n {
        let e = rx.recv_timeout(GUARD).expect("a terminal event");
        ends += usize::from(is_terminal(&e));
        out.push(e);
    }
    jobs.join();
    assert!(
        rx.try_recv().is_err(),
        "nothing after the last terminal event"
    );
    out
}

#[test]
fn model_job_transcription_download_is_followed_by_the_check() {
    let dir = TempDir::new("model-jobs");
    let svc = services(&dir);
    let (fake, go) = Fake::new(Mode::Succeed);
    let (jobs, rx) = runner(fake);
    jobs.start_download(&svc, MODEL).unwrap();
    // One go for the download, one for the check it starts.
    go.send(()).unwrap();
    go.send(()).unwrap();
    let progress = |done| ModelJobEvent::Progress {
        model: MODEL.into(),
        done,
        total: 10,
    };
    let step = |verdict| ModelJobEvent::CheckProgress {
        model: "parakeet-tdt-0.6b-v2".into(),
        runs_on: RunsOn::Npu,
        verdict,
    };
    assert_eq!(
        until_ends(&rx, &jobs, 2),
        [
            progress(0),
            progress(4),
            progress(10),
            ModelJobEvent::Downloaded {
                model: MODEL.into()
            },
            step(None),
            step(Some(Verdict::KeepsUp)),
            ModelJobEvent::Checked {
                model: Some("parakeet-tdt-0.6b-v2".into()),
                runs_on: Some(RunsOn::Npu),
                trials: 1
            }
        ]
    );
}

#[test]
fn model_job_download_during_a_meeting_warns_that_the_check_did_not_start() {
    let dir = TempDir::new("model-jobs");
    let svc = services(&dir);
    let gate = crate::activity::WorkGate::new();
    let meeting = gate.enter_work().unwrap();
    let (fake, go) = Fake::new(Mode::Succeed);
    let (jobs, rx) = runner_with_gate(fake, gate);
    jobs.start_download(&svc, MODEL).unwrap();
    go.send(()).unwrap();
    let mut events = Vec::new();
    while !events.iter().any(is_terminal) {
        events.push(rx.recv_timeout(GUARD).expect("a terminal event"));
    }
    let warning = rx.recv_timeout(GUARD).expect("the warning");
    jobs.join();
    assert!(rx.try_recv().is_err(), "the warning is the last event");
    assert_eq!(
        events.last(),
        Some(&ModelJobEvent::Downloaded {
            model: MODEL.into()
        })
    );
    assert_eq!(
        warning,
        ModelJobEvent::Warning {
            model: Some(MODEL.into()),
            message: check_not_started_message(crate::activity::WORK_RUNNING),
        }
    );
    assert_eq!(
        check_not_started_message(crate::activity::WORK_RUNNING),
        "The model check did not start after the download. A meeting or a meeting job is \
         running; run the model check when it has finished."
    );
    // No check ran; once the meeting is over it can.
    drop(meeting);
    jobs.start_check(&svc).unwrap();
    go.send(()).unwrap();
    assert!(matches!(
        until_end(&rx, &jobs).last(),
        Some(ModelJobEvent::Checked { .. })
    ));
}

#[test]
fn model_job_summary_download_starts_no_check() {
    let dir = TempDir::new("model-jobs");
    let svc = services(&dir);
    let (fake, go) = Fake::new(Mode::Succeed);
    let (jobs, rx) = runner(fake);
    jobs.start_download(&svc, SUMMARY).unwrap();
    go.send(()).unwrap();
    // `until_end` joins every job and fails on any event after the `Downloaded`.
    assert_eq!(
        until_end(&rx, &jobs).last(),
        Some(&ModelJobEvent::Downloaded {
            model: SUMMARY.into()
        })
    );
}

#[test]
fn model_job_cancel_check_ends_with_one_failed() {
    let dir = TempDir::new("model-jobs");
    let svc = services(&dir);
    let (fake, _go) = Fake::new(Mode::Succeed);
    let (jobs, rx) = runner(fake);
    let e = jobs.cancel_check().unwrap_err();
    assert_eq!(e.kind, "not_checking");
    jobs.start_check(&svc).unwrap();
    // The first trial has started: the check is waiting inside its work.
    assert!(matches!(
        rx.recv_timeout(GUARD).unwrap(),
        ModelJobEvent::CheckProgress { verdict: None, .. }
    ));
    jobs.cancel_check().unwrap();
    assert_eq!(
        until_end(&rx, &jobs),
        [ModelJobEvent::Failed {
            model: None,
            message: crate::check_child::CHECK_CANCELLED_MESSAGE.into(),
            retryable: true,
        }]
    );
    let e = jobs.cancel_check().unwrap_err();
    assert_eq!(e.kind, "not_checking");
}

#[test]
fn model_job_download_and_removal_are_refused_while_the_check_runs() {
    let dir = TempDir::new("model-jobs");
    let svc = services(&dir);
    let (fake, go) = Fake::new(Mode::Succeed);
    let (jobs, rx) = runner(fake);
    jobs.start_check(&svc).unwrap();
    let e = jobs.start_download(&svc, SUMMARY).unwrap_err();
    assert_eq!(
        (e.kind.as_str(), e.message.as_str(), e.retryable),
        ("busy", CHECK_RUNNING_MESSAGE, true)
    );
    assert!(!jobs.downloading(SUMMARY));
    let mut removed = false;
    let e = jobs
        .without_download(SUMMARY, || {
            removed = true;
            Ok(())
        })
        .unwrap_err();
    assert_eq!(
        (e.kind.as_str(), e.message.as_str(), e.retryable),
        ("busy", CHECK_RUNNING_MESSAGE, true)
    );
    assert!(!removed, "nothing is removed while the check runs");
    go.send(()).unwrap();
    assert!(matches!(
        until_end(&rx, &jobs).last(),
        Some(ModelJobEvent::Checked { .. })
    ));

    // Once the check is over both run; a removal holds the check slot, so no check starts
    // between its test and the removal.
    jobs.without_download(SUMMARY, || {
        assert!(jobs.check_slot_locked() && jobs.downloads_locked());
        Ok(())
    })
    .unwrap();
    jobs.start_download(&svc, SUMMARY).unwrap();
    go.send(()).unwrap();
    assert!(matches!(
        until_end(&rx, &jobs).last(),
        Some(ModelJobEvent::Downloaded { .. })
    ));
}

#[test]
fn model_job_check_is_refused_while_a_download_runs() {
    let dir = TempDir::new("model-jobs");
    let svc = services(&dir);
    let (fake, go) = Fake::new(Mode::Succeed);
    let (jobs, rx) = runner(fake);
    jobs.start_download(&svc, MODEL).unwrap();
    let e = jobs.start_check(&svc).unwrap_err();
    assert_eq!(
        (e.kind.as_str(), e.message.as_str(), e.retryable),
        ("busy", DOWNLOADING_MESSAGE, true)
    );
    // One go for the download, one for the check its end starts.
    go.send(()).unwrap();
    go.send(()).unwrap();
    assert!(matches!(
        until_ends(&rx, &jobs, 2).last(),
        Some(ModelJobEvent::Checked { .. })
    ));
    // Once the download is over a manual check starts as well.
    jobs.start_check(&svc).unwrap();
    go.send(()).unwrap();
    assert!(matches!(
        until_end(&rx, &jobs).last(),
        Some(ModelJobEvent::Checked { .. })
    ));
}

#[test]
fn model_job_two_transcription_downloads_run_one_check_after_both() {
    let dir = TempDir::new("model-jobs");
    let svc = services(&dir);
    let (fake, go) = Fake::new(Mode::Succeed);
    let (jobs, rx) = runner(fake);
    jobs.start_download(&svc, MODEL).unwrap();
    jobs.start_download(&svc, "tiny.en").unwrap();
    // The first download to end finds the other still writing: it only warns.
    go.send(()).unwrap();
    let mut events = Vec::new();
    while !events.iter().any(is_terminal) {
        events.push(rx.recv_timeout(GUARD).expect("a terminal event"));
    }
    let first = match events.last() {
        Some(ModelJobEvent::Downloaded { model }) => model.clone(),
        other => panic!("expected Downloaded, got {other:?}"),
    };
    // The second download may still report progress in between: the warning is the next
    // event that is not a Progress.
    let next = loop {
        match rx.recv_timeout(GUARD).expect("the warning") {
            ModelJobEvent::Progress { .. } => {}
            other => break other,
        }
    };
    assert_eq!(
        next,
        ModelJobEvent::Warning {
            model: Some(first),
            message: check_not_started_message(DOWNLOADING_MESSAGE),
        }
    );
    // The second ends, starts the one check; one go for the download, one for the check.
    go.send(()).unwrap();
    go.send(()).unwrap();
    let rest = until_ends(&rx, &jobs, 2);
    let count = |f: fn(&ModelJobEvent) -> bool| rest.iter().filter(|e| f(e)).count();
    assert_eq!(count(|e| matches!(e, ModelJobEvent::Downloaded { .. })), 1);
    assert_eq!(count(|e| matches!(e, ModelJobEvent::Checked { .. })), 1);
    assert_eq!(count(|e| matches!(e, ModelJobEvent::Warning { .. })), 0);
}

#[test]
fn model_job_summary_download_does_not_block_the_check() {
    let dir = TempDir::new("model-jobs");
    let svc = services(&dir);
    let (fake, go) = Fake::new(Mode::Succeed);
    let (jobs, rx) = runner(fake);
    jobs.start_download(&svc, SUMMARY).unwrap();
    // A manual check starts while the summary download is still writing.
    jobs.start_check(&svc).unwrap();
    go.send(()).unwrap();
    go.send(()).unwrap();
    let events = until_ends(&rx, &jobs, 2);
    assert!(
        events
            .iter()
            .any(|e| matches!(e, ModelJobEvent::Downloaded { .. }))
    );
    assert!(
        events
            .iter()
            .any(|e| matches!(e, ModelJobEvent::Checked { .. }))
    );
}

#[test]
fn model_job_transcription_download_ending_during_a_summary_download_starts_the_check() {
    let dir = TempDir::new("model-jobs");
    let svc = services(&dir);
    let (fake, go) = Fake::new(Mode::Succeed);
    let (jobs, rx) = runner(fake);
    jobs.start_download(&svc, SUMMARY).unwrap();
    jobs.start_download(&svc, MODEL).unwrap();
    // Three gos: two downloads and the check. Whichever download takes a go first ends;
    // the transcription one's end must start the check whatever the summary one does.
    go.send(()).unwrap();
    go.send(()).unwrap();
    go.send(()).unwrap();
    let events = until_ends(&rx, &jobs, 3);
    let n = |f: fn(&ModelJobEvent) -> bool| events.iter().filter(|e| f(e)).count();
    assert_eq!(n(|e| matches!(e, ModelJobEvent::Downloaded { .. })), 2);
    assert_eq!(n(|e| matches!(e, ModelJobEvent::Checked { .. })), 1);
    assert_eq!(n(|e| matches!(e, ModelJobEvent::Warning { .. })), 0);
}

#[test]
fn model_job_an_npu_version_download_is_waited_for_and_followed_by_the_check() {
    // The ARM64 catalogue, where NPU versions are offered (on x64 they are not downloadable).
    let arm64 =
        Catalogue::parse_with(include_str!("../../../../models/manifest.toml"), true).unwrap();
    let dir = TempDir::new("model-jobs");
    let models = earshot_runtime::backends::ModelDir::new(dir.0.join("models"));
    let npu: Vec<String> = arm64
        .models_of(ModelKind::NpuVersion)
        .map(|m| m.name.clone())
        .collect();
    assert!(!npu.is_empty(), "the manifest holds NPU versions");
    for name in &npu {
        assert!(
            check_waits_for(&arm64, name),
            "{name}: the check waits for it"
        );
        assert!(
            check_follows_in(&arm64, &models, name),
            "{name}: its download starts the check"
        );
    }
    // The other kinds as before.
    assert!(check_waits_for(&arm64, MODEL) && check_follows_in(&arm64, &models, MODEL));
    assert!(!check_waits_for(&arm64, SUMMARY) && !check_follows_in(&arm64, &models, SUMMARY));
    let helpers = earshot_runtime::catalogue::HELPERS_MODEL;
    assert!(check_waits_for(&arm64, helpers));
    assert!(
        !check_follows_in(&arm64, &models, helpers),
        "no model to check yet"
    );
}

#[test]
fn model_job_the_helpers_are_a_download_like_any_other_and_only_a_check_follows_them_with_a_model()
{
    use earshot_runtime::catalogue::HELPERS_MODEL;
    let dir = TempDir::new("model-jobs");
    let models = earshot_runtime::backends::ModelDir::new(dir.0.join("models"));
    // Nothing to check yet: the helpers alone start no check; a transcription model does.
    assert!(!check_follows(&models, HELPERS_MODEL));
    assert!(check_follows(&models, MODEL));
    assert!(!check_follows(&models, SUMMARY));
    // With a transcription model in place (by size, as the install state reads it), the
    // helpers' arrival starts the check that could not run before.
    let catalogue = Catalogue::embedded().unwrap();
    for a in catalogue.artefacts_of("tiny.en").unwrap() {
        let path = a.file(&models).unwrap();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::File::create(&path)
            .unwrap()
            .set_len(a.size)
            .unwrap();
    }
    assert!(check_follows(&models, HELPERS_MODEL));
    assert!(!check_follows(&models, SUMMARY));
    assert!(require_catalogue_model(HELPERS_MODEL).is_ok());
}

#[test]
fn model_job_a_download_failure_is_fixed_copy_without_a_url_or_a_manifest_id() {
    use earshot_runtime::archive::ExtractError;
    use earshot_runtime::catalogue::{CatalogueError, manifest_id_in};
    use earshot_runtime::download::DownloadError as D;
    let url = "https://github.com/microsoft/onnxruntime/releases/onnxruntime-win-x64-1.31.0.zip";
    let download = |error| ModelDownloadError::Download {
        artefact: "onnxruntime-win-x64".into(),
        error,
    };
    let extract = |error| ModelDownloadError::Extract {
        artefact: "onnxruntime-win-x64".into(),
        error,
    };
    let io = || std::io::Error::other("the disk is full at C:\\Users\\someone");
    for (e, words) in [
        (
            download(D::Status {
                url: url.into(),
                status: 503,
            }),
            "HTTP 503",
        ),
        (
            download(D::Interrupted {
                url: url.into(),
                done: 1,
                total: 2,
                reason: "connection lost".into(),
            }),
            "network problem",
        ),
        (
            download(D::Verification {
                url: url.into(),
                detail: "sha256 abc".into(),
            }),
            "did not match",
        ),
        (
            download(D::Io {
                what: "write onnxruntime-win-x64".into(),
                error: io(),
            }),
            "free space",
        ),
        (
            extract(ExtractError::Io {
                what: "write".into(),
                error: io(),
            }),
            "free space",
        ),
        (
            extract(ExtractError::InUse {
                file: "C:/m/cuda/ggml.dll".into(),
                error: io(),
            }),
            "Restart Earshot",
        ),
        (
            extract(ExtractError::Refused("archive entry `x`".into())),
            "cannot use this download",
        ),
        (
            ModelDownloadError::Catalogue(CatalogueError::UnknownModel("x".into())),
            "cannot use this download",
        ),
    ] {
        let copy = download_failure_copy(&e);
        assert!(copy.contains(words), "{copy}");
        assert!(!copy.contains("http"), "{copy}");
        assert!(!copy.contains('\\'), "{copy}");
        assert_eq!(manifest_id_in(&copy), None, "{copy}");
        assert!(!crate::plain_text::names_developer_tool(&copy), "{copy}");
    }
}

#[test]
fn model_job_the_check_waits_for_the_helpers_and_they_alone_start_none() {
    use earshot_runtime::catalogue::HELPERS_MODEL;
    let dir = TempDir::new("model-jobs");
    let svc = services(&dir);
    let (fake, go) = Fake::new(Mode::Succeed);
    let (jobs, rx) = runner(fake);
    jobs.start_download(&svc, HELPERS_MODEL).unwrap();
    let e = jobs.start_check(&svc).unwrap_err();
    assert_eq!(e.kind, "busy");
    assert_eq!(e.message, DOWNLOADING_MESSAGE);
    go.send(()).unwrap();
    // `until_end` also pins that nothing follows: no model to check, so no check.
    let events = until_end(&rx, &jobs);
    assert!(
        matches!(events.last(), Some(ModelJobEvent::Downloaded { model }) if model == HELPERS_MODEL),
        "{events:?}"
    );
}

#[test]
fn model_job_the_helpers_arriving_start_the_check_a_waiting_model_needs() {
    use earshot_runtime::catalogue::HELPERS_MODEL;
    let dir = TempDir::new("model-jobs");
    let svc = services(&dir);
    let (fake, go) = Fake::with_a_model_installed(Mode::Succeed);
    let (jobs, rx) = runner(fake);
    jobs.start_download(&svc, HELPERS_MODEL).unwrap();
    go.send(()).unwrap();
    go.send(()).unwrap();
    let events = until_ends(&rx, &jobs, 2);
    let at = |f: fn(&ModelJobEvent) -> bool| events.iter().position(f).unwrap();
    assert!(
        at(|e| matches!(e, ModelJobEvent::Downloaded { .. }))
            < at(|e| matches!(e, ModelJobEvent::Checked { .. })),
        "{events:?}"
    );
}

/// T9.23: a download that reaches no server says it may be a network that needs a proxy, which
/// Earshot does not use; one a server answered never says so.
#[test]
fn model_job_a_download_that_reaches_no_server_mentions_a_proxy_network() {
    use earshot_core::asr::CancelToken;
    use earshot_core::identity::ManualClock;
    use earshot_runtime::download::{Allowlist, Downloader, Expected, Url};
    // A port that was free a moment ago: the connection is refused.
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let url = Url::parse(&format!("http://127.0.0.1:{port}/file.bin")).unwrap();
    let mut list = Allowlist::new();
    list.allow_url(&url).unwrap();
    let d = Downloader::new(list, Arc::new(ManualClock::new(0))).unwrap();
    let dir = TempDir::new("model-jobs-noserver");
    let sha = "0".repeat(64);
    let error = d
        .fetch(
            &url,
            &dir.0.join("f.bin"),
            &Expected {
                sha256: &sha,
                size: 4,
            },
            &CancelToken::new(),
            &mut |_, _| {},
        )
        .unwrap_err();
    assert!(error.reached_no_server(), "{error:?}");
    let copy = download_failure_copy(&ModelDownloadError::Download {
        artefact: "x".into(),
        error,
    });
    assert_eq!(copy, NO_SERVER_ANSWERED);
    assert!(copy.contains("proxy"), "{copy}");
    assert!(!copy.contains("127.0.0.1"), "{copy}");
    assert!(!crate::plain_text::names_developer_tool(&copy));
    // A server that answered is not a proxy problem.
    let answered = download_failure_copy(&ModelDownloadError::Download {
        artefact: "x".into(),
        error: earshot_runtime::download::DownloadError::Status {
            url: "x".into(),
            status: 502,
        },
    });
    assert!(!answered.contains("proxy"), "{answered}");
}
