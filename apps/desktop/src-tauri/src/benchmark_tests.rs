//! Tests of the Benchmark section (T7.4c): the jobs with fake work (never the models), the
//! stored result and the page's words. Every name contains `benchmark`.

use std::path::PathBuf;
use std::sync::mpsc;
use std::time::Duration;

use earshot_runtime::backends::Accelerator;
use earshot_runtime::benchmark::{RecordSettings, record_benchmark};

use super::*;
use crate::activity::{BENCHMARK_ALREADY_RUNNING, BENCHMARK_RUNNING, WORK_RUNNING_FOR_BENCHMARK};
use crate::testutil::{T0, TempDir, services};

fn record(kept_up: bool, summary: RecordSummary) -> BenchmarkRecord {
    BenchmarkRecord {
        ran_at_ms: T0,
        app_version: "0.1.0".into(),
        settings: RecordSettings {
            transcription_model: "parakeet-tdt-0.6b-v2".into(),
            speaker_timing: "during".into(),
            live_summaries: LiveSummariesRun::Off,
        },
        meeting_ms: 150_000,
        live_backends: vec![
            RecordBackend {
                kind: "cpu".into(),
                device: "cpu".into(),
            },
            RecordBackend {
                kind: "cpu".into(),
                device: "cpu".into(),
            },
        ],
        lag_max_ms: Some(31_400),
        lag_median_ms: Some(4_250),
        catch_up_ms: if kept_up { 2_100 } else { 61_000 },
        kept_up,
        transcript_ready_ms: 12_345,
        final_backend: Some(RecordBackend {
            kind: "cuda".into(),
            device: "NVIDIA GeForce RTX 5080".into(),
        }),
        summary,
        check: Some(StoredVerdict::Pass),
        notes: vec!["Parakeet 0.6B v3 has no NPU version, so it runs on the processor.".into()],
    }
}

fn stored(ready_ms: u64) -> RecordSummary {
    RecordSummary::Stored {
        ready_ms,
        model: "qwen3.5-4b-q4_k_m".into(),
        backend: "cpu".into(),
    }
}

/// What a fake run does after its progress.
type Then = Box<
    dyn Fn(&Services, &CancelToken) -> Result<Option<BenchmarkRecord>, RuntimeError> + Send + Sync,
>;

/// Fake work: reports the replay, then does what `then` says.
struct Fake {
    then: Then,
}

impl BenchmarkWork for Fake {
    fn run(
        &self,
        services: &Services,
        cancel: &CancelToken,
        progress: &mut dyn FnMut(BenchmarkProgress),
    ) -> Result<Option<BenchmarkRecord>, RuntimeError> {
        let total = Duration::from_secs(150);
        progress(BenchmarkProgress::Replaying {
            heard: Duration::ZERO,
            total,
        });
        progress(BenchmarkProgress::Replaying {
            heard: Duration::from_secs(1),
            total,
        });
        progress(BenchmarkProgress::Transcribing);
        (self.then)(services, cancel)
    }
}

type Events = Arc<Mutex<Vec<BenchmarkEvent>>>;

fn jobs(then: Fake, gate: &WorkGate) -> (BenchmarkJobs, Events) {
    let events: Events = Arc::default();
    let sink_events = Arc::clone(&events);
    let sink: BenchmarkSink = Arc::new(move |e| {
        sink_events
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(e);
    });
    (
        BenchmarkJobs::new(Arc::new(then), sink, gate.clone()),
        events,
    )
}

fn taken(events: &Events) -> Vec<BenchmarkEvent> {
    std::mem::take(&mut *events.lock().unwrap_or_else(PoisonError::into_inner))
}

fn terminal(e: &BenchmarkEvent) -> bool {
    !matches!(e, BenchmarkEvent::Progress { .. })
}

#[test]
fn benchmark_run_reports_progress_then_the_stored_result() {
    let dir = TempDir::new("benchmark");
    let svc = services(&dir);
    let gate = WorkGate::new();
    let (jobs, events) = jobs(
        Fake {
            then: Box::new(|svc, _| {
                let r = record(true, stored(48_000));
                record_benchmark(&mut svc.store(), &r)?;
                Ok(Some(r))
            }),
        },
        &gate,
    );
    // Nothing stored yet: no result, the meeting's length and its attribution.
    let before = benchmark_view(&svc, None).unwrap();
    assert_eq!(before.last, None);
    assert!(!before.unreadable);
    assert_eq!(before.meeting_ms, 150_000);
    assert_eq!(before.attribution, MEETING_ATTRIBUTION);
    jobs.start(&svc).unwrap();
    jobs.join();
    let events = taken(&events);
    assert_eq!(
        events[..3],
        [
            BenchmarkEvent::Progress {
                phase: BenchmarkPhase::Replaying,
                heard_ms: 0,
                total_ms: 150_000
            },
            BenchmarkEvent::Progress {
                phase: BenchmarkPhase::Replaying,
                heard_ms: 1_000,
                total_ms: 150_000
            },
            BenchmarkEvent::Progress {
                phase: BenchmarkPhase::Transcribing,
                heard_ms: 150_000,
                total_ms: 150_000
            },
        ]
    );
    assert_eq!(events.iter().filter(|e| terminal(e)).count(), 1);
    let Some(BenchmarkEvent::Done { view }) = events.last() else {
        panic!("{events:?}");
    };
    let last = view.last.as_ref().unwrap();
    assert!(last.kept_up);
    assert_eq!(
        view,
        &benchmark_view(&svc, None).unwrap(),
        "the page shows the stored one"
    );
    // The pass is released: a meeting may start.
    drop(gate.enter_meeting().unwrap());
}

#[test]
fn benchmark_cancel_is_one_cancelled_event_and_releases_the_gate() {
    let dir = TempDir::new("benchmark-cancel");
    let svc = services(&dir);
    let gate = WorkGate::new();
    let (started_tx, started_rx) = mpsc::sync_channel::<()>(1);
    let started_tx = Mutex::new(started_tx);
    let (jobs, events) = jobs(
        Fake {
            then: Box::new(move |_, cancel| {
                let _ = started_tx.lock().unwrap().send(());
                // Bounded only for a failing test.
                for _ in 0..60_000 {
                    if cancel.is_cancelled() {
                        return Ok(None);
                    }
                    std::thread::sleep(Duration::from_millis(1));
                }
                panic!("never cancelled");
            }),
        },
        &gate,
    );
    jobs.start(&svc).unwrap();
    started_rx.recv().unwrap();
    // A page opened now sees the run and its step.
    assert_eq!(
        jobs.current(),
        Some(BenchmarkRunning {
            phase: BenchmarkPhase::Transcribing,
            heard_ms: 150_000,
            total_ms: 150_000,
        })
    );
    assert_eq!(
        benchmark_view(&svc, jobs.current()).unwrap().running,
        jobs.current()
    );
    // Busy while it runs: a second run, a meeting, a job.
    let e = jobs.start(&svc).unwrap_err();
    assert_eq!(
        (e.kind.as_str(), e.message.as_str()),
        ("busy", BENCHMARK_ALREADY_RUNNING)
    );
    assert_eq!(gate.enter_meeting().unwrap_err().message, BENCHMARK_RUNNING);
    assert_eq!(gate.enter_work().unwrap_err().message, BENCHMARK_RUNNING);
    jobs.cancel();
    jobs.join();
    let events = taken(&events);
    assert_eq!(events.last(), Some(&BenchmarkEvent::Cancelled));
    assert_eq!(jobs.current(), None, "no run under way any more");
    assert_eq!(events.iter().filter(|e| terminal(e)).count(), 1);
    assert_eq!(
        benchmark_view(&svc, None).unwrap().last,
        None,
        "nothing stored"
    );
    drop(gate.enter_check().unwrap());
}

#[test]
fn benchmark_failure_and_panic_are_one_failed_event() {
    let dir = TempDir::new("benchmark-fail");
    let svc = services(&dir);
    let gate = WorkGate::new();
    let (jobs, events) = jobs(
        Fake {
            then: Box::new(|_, _| Err(RuntimeError::Unavailable("no model here"))),
        },
        &gate,
    );
    jobs.start(&svc).unwrap();
    jobs.join();
    let got = taken(&events);
    assert_eq!(
        got.last(),
        Some(&BenchmarkEvent::Failed {
            message: CANNOT_RUN_HERE.into()
        })
    );
    assert_eq!(got.iter().filter(|e| terminal(e)).count(), 1);

    let (jobs, events) = jobs_panicking(&gate);
    jobs.start(&svc).unwrap();
    jobs.join();
    let got = taken(&events);
    assert_eq!(
        got.last(),
        Some(&BenchmarkEvent::Failed {
            message: BENCHMARK_PANIC_MESSAGE.into()
        })
    );
    assert_eq!(got.iter().filter(|e| terminal(e)).count(), 1);
    drop(gate.enter_benchmark().unwrap());
}

fn jobs_panicking(gate: &WorkGate) -> (BenchmarkJobs, Events) {
    jobs(
        Fake {
            then: Box::new(|_, _| panic!("boom")),
        },
        gate,
    )
}

#[test]
fn benchmark_refused_while_a_meeting_job_runs() {
    let dir = TempDir::new("benchmark-busy");
    let svc = services(&dir);
    let gate = WorkGate::new();
    let (jobs, events) = jobs(
        Fake {
            then: Box::new(|_, _| Ok(None)),
        },
        &gate,
    );
    let job = gate.enter_work().unwrap();
    let e = jobs.start(&svc).unwrap_err();
    assert_eq!(
        (e.kind.as_str(), e.message.as_str(), e.retryable),
        ("busy", WORK_RUNNING_FOR_BENCHMARK, true)
    );
    assert!(taken(&events).is_empty());
    drop(job);
    jobs.start(&svc).unwrap();
    jobs.join();
}

#[test]
fn benchmark_unreadable_result_is_reported() {
    let dir = TempDir::new("benchmark-unreadable");
    let svc = services(&dir);
    svc.store()
        .set_local_value(
            earshot_runtime::benchmark::BENCHMARK_KEY,
            b"\x09\x00\x00\x00{}",
        )
        .unwrap();
    let view = benchmark_view(&svc, None).unwrap();
    assert_eq!((view.last, view.unreadable), (None, true));
}

#[test]
fn benchmark_result_words() {
    let v = result_view(&record(true, stored(48_000)));
    assert_eq!(
        v.headline,
        "Live transcription kept up, and the summary was ready 48 seconds after the meeting ended."
    );
    let settings: Vec<(&str, &str)> = v
        .settings
        .iter()
        .map(|r| (r.label.as_str(), r.value.as_str()))
        .collect();
    assert_eq!(
        settings,
        [
            ("Transcription", "Parakeet 0.6B v2 on the processor"),
            ("Summary", "Qwen 3.5 4B on the processor"),
            ("Summarising during the meeting", "Off"),
            ("Speaker detection", "During the call"),
        ]
    );
    let timings: Vec<(&str, &str)> = v
        .timings
        .iter()
        .map(|r| (r.label.as_str(), r.value.as_str()))
        .collect();
    assert_eq!(
        timings,
        [
            (
                "Transcript lag during the meeting (both tracks)",
                "Usually 4.3 seconds behind, at most 31 seconds"
            ),
            (
                "Live transcript",
                "Kept up: complete 2.1 seconds after the meeting ended"
            ),
            (
                "Final transcript",
                "Ready 12 seconds after the meeting ended on the graphics card (NVIDIA GeForce RTX 5080)"
            ),
            ("Summary", "Ready 48 seconds after the meeting ended"),
            (
                "Model check",
                "Found Parakeet 0.6B v2 fast enough on the processor"
            ),
        ]
    );
    let slow = result_view(&record(
        false,
        RecordSummary::Skipped {
            reason: BenchSkip::NotOnThisComputer {
                writer: "claude-code".into(),
            },
        },
    ));
    assert!(!slow.kept_up);
    assert!(slow.headline.starts_with("Live transcription fell behind."));
    assert_eq!(
        slow.settings[1].value,
        "Not run: summaries are written outside this computer, and the benchmark makes no \
         network connection"
    );
    assert_eq!(
        slow.timings[1].value,
        "Fell behind: complete 61 seconds after the meeting ended"
    );
    assert_eq!(slow.timings[3].value, "Not run");
    let none = result_view(&record(
        true,
        RecordSummary::Skipped {
            reason: BenchSkip::NotInstalled {
                model: "qwen3.5-4b-q4_k_m".into(),
            },
        },
    ));
    assert_eq!(none.headline, "Live transcription kept up.");
    assert_eq!(
        none.settings[1].value,
        "None (Qwen 3.5 4B is not downloaded; see Summaries)"
    );
    // No em dashes or manifest ids in what the page shows.
    for text in [&v, &slow, &none]
        .iter()
        .flat_map(|v| v.settings.iter().chain(&v.timings))
        .flat_map(|r| [&r.label, &r.value])
    {
        assert!(!text.contains('\u{2014}'), "{text}");
        assert!(
            !text.contains("parakeet-tdt") && !text.contains("qwen3.5"),
            "{text}"
        );
    }
}

#[test]
fn benchmark_live_summaries_row_says_what_ran() {
    let mut r = record(true, stored(1_000));
    r.settings.live_summaries = LiveSummariesRun::NotRun;
    assert_eq!(
        result_view(&r).settings[2].value,
        "On, but not run: it needs a summary on this computer"
    );
    r.settings.live_summaries = LiveSummariesRun::Stopped {
        reason: earshot_runtime::live_summaries::ON_BATTERY_REASON.into(),
    };
    assert_eq!(
        result_view(&r).settings[2].value,
        "Summaries wait for the end of the call on battery power, as speaker detection does."
    );
    r.settings.live_summaries = LiveSummariesRun::On;
    assert_eq!(result_view(&r).settings[2].value, "On");
    assert_eq!(result_view(&r).notes, r.notes);
}

#[test]
fn benchmark_failures_are_plain_words_with_no_manifest_id() {
    use earshot_runtime::catalogue::manifest_id_in;
    let id = "parakeet-tdt-0.6b-v2";
    let path = PathBuf::from(format!("models/parakeet/{id}/encoder.onnx"));
    let cases = [
        RuntimeError::UnknownModel(id.into()),
        RuntimeError::ModelMissing {
            path: path.clone(),
            fetch_set: Some("ci"),
        },
        RuntimeError::ModelIncomplete {
            path,
            expected: 2,
            found: 1,
            fetch_set: "ci",
        },
        RuntimeError::Unavailable("QNN needs a build with the `qnn` feature"),
        RuntimeError::NoBackend(Vec::new()),
        RuntimeError::Internal("parakeet-tdt-0.6b-v2 broke"),
        RuntimeError::Capture(format!("{id} capture")),
        RuntimeError::NoTracks,
    ];
    for e in &cases {
        let text = failure_copy(e);
        assert_eq!(manifest_id_in(&text), None, "{text}");
        assert!(!text.contains("models/"), "{text}");
        assert!(text.ends_with('.'), "{text}");
    }
    assert_eq!(failure_copy(&cases[0]), MODEL_MISSING);
    assert_eq!(failure_copy(&cases[3]), CANNOT_RUN_HERE);
    assert_eq!(failure_copy(&cases[5]), OTHER_FAILED);
    // The rows and the headline of a result too.
    let v = result_view(&record(true, stored(1_000)));
    for text in v
        .settings
        .iter()
        .chain(&v.timings)
        .flat_map(|r| [&r.label, &r.value])
    {
        assert_eq!(manifest_id_in(text), None, "{text}");
    }
    assert_eq!(manifest_id_in(&v.headline), None);
    // The notes too, with every library note in plain words.
    let mut r = record(true, stored(1_000));
    r.notes.extend(
        [
            Accelerator::Cpu,
            Accelerator::Cuda,
            Accelerator::Vulkan,
            Accelerator::Qnn,
        ]
        .map(bench::onnx_fallback_note),
    );
    let notes = result_view(&r).notes;
    assert_eq!(notes.len(), 5);
    for text in &notes {
        assert_eq!(manifest_id_in(text), None, "{text}");
        for debug in ["Cpu", "Cuda", "Vulkan", "Qnn"] {
            assert!(!text.contains(debug), "{text}");
        }
    }
}

#[test]
fn benchmark_seconds_text() {
    assert_eq!(seconds_text(0), "0.0 seconds");
    assert_eq!(seconds_text(950), "1 second");
    assert_eq!(seconds_text(4_250), "4.3 seconds");
    assert_eq!(seconds_text(9_949), "9.9 seconds");
    assert_eq!(seconds_text(9_960), "10 seconds");
    assert_eq!(seconds_text(10_000), "10 seconds");
    assert_eq!(seconds_text(119_400), "119 seconds");
    assert_eq!(seconds_text(150_000), "2 min 30 s");
}

#[test]
fn benchmark_bundled_meeting_is_the_attributed_excerpt() {
    let m = bundled_meeting().unwrap();
    assert_eq!(m.duration(), Duration::from_secs(150));
    let notice = include_str!("../resources/benchmark/NOTICE.md");
    for fact in ["ES2004a", "570 s to 720 s", "CC BY 4.0"] {
        assert!(notice.contains(fact), "{fact}");
    }
    for fact in [
        "ES2004a",
        "9:30 to 12:00",
        "CC BY 4.0",
        "AMI Meeting Corpus",
    ] {
        assert!(MEETING_ATTRIBUTION.contains(fact), "{fact}");
    }
}
