//! The model check child's line protocol (T7.4e), with fixed text: every line kind, a
//! truncated stream, an error line, an unknown kind; the job mapping over a fake child; the
//! line prefix and the stall watch (T14.11).

use std::io::Cursor;
use std::sync::Mutex;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};

use earshot_runtime::backends::ModelDir;
use earshot_runtime::model_bench::stored_check;

use super::*;
use crate::model_jobs::{CheckStep, JobLines, record_outcome};
use crate::models::{RunsOn, Verdict as UiVerdict};
use crate::testutil::{TempDir, services};

/// What a fake reader heard.
#[derive(Default)]
struct Heard {
    warnings: Vec<String>,
    trials: Vec<(String, BackendKind, Option<Verdict>)>,
    damaged: Vec<Vec<String>>,
}

impl LineEvents for Heard {
    fn damaged(&mut self, artefacts: Vec<String>) {
        self.damaged.push(artefacts);
    }
    fn warning(&mut self, message: String) {
        self.warnings.push(message);
    }
    fn trial(&mut self, model: &str, backend: BackendKind, verdict: Option<Verdict>) {
        self.trials.push((model.to_owned(), backend, verdict));
    }
}

const WARNING: &str =
    r#"{"line":"warning","message":"The Qnn library of ONNX Runtime is unavailable."}"#;
const TESTING: &str = r#"{"line":"testing","model":"parakeet-tdt-0.6b-v2","backend":"cpu"}"#;
const TESTED: &str = r#"{"line":"tested","trial":{"model":"parakeet-tdt-0.6b-v2","backend":"cpu","rtf":0.12,"wer_percent":3.5,"failure":null,"load_ms":2100,"evidence":"onnx placement: cpu","verdict":"pass_comfortable"}}"#;
const DONE: &str = r#"{"line":"done","facts":{"accelerators":[{"backend":"qnn","free_memory":null}],"cores":12,"memory":17179869184,"drivers":["a=1.0"]},"trials":[{"model":"parakeet-tdt-0.6b-v2","backend":"cpu","rtf":0.12,"wer_percent":3.5,"failure":null,"load_ms":2100,"evidence":"onnx placement: cpu","verdict":"pass_comfortable"},{"model":"large-v3-turbo","backend":"cpu","rtf":null,"wer_percent":null,"failure":"out of memory","load_ms":0,"evidence":null,"verdict":"wrong"}],"answer":{"model":"parakeet-tdt-0.6b-v2","backend":"cpu","fallback":"parakeet-tdt-110m"}}"#;

/// `text` as the child's stream carries it: every line that is not empty starts with the
/// protocol's prefix.
fn wire(text: &str) -> String {
    text.split('\n')
        .map(|l| {
            if l.is_empty() {
                String::new()
            } else {
                format!("{LINE_PREFIX}{l}")
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Reads `text`, each of its lines a line of the protocol.
fn read(text: &str) -> (Result<CheckOutcome, IpcError>, Heard) {
    read_raw(&wire(text))
}

/// Reads `text` exactly as given.
fn read_raw(text: &str) -> (Result<CheckOutcome, IpcError>, Heard) {
    let mut heard = Heard::default();
    let r = read_lines(Cursor::new(text.as_bytes()), &mut heard, &mut || {});
    (r, heard)
}

#[test]
fn model_check_lines_of_every_kind_are_read_in_order() {
    let (r, heard) = read(&[WARNING, TESTING, TESTED, "", DONE, ""].join("\n"));
    let outcome = r.unwrap();
    assert_eq!(
        heard.warnings,
        ["The Qnn library of ONNX Runtime is unavailable."]
    );
    assert_eq!(
        heard.trials,
        [
            ("parakeet-tdt-0.6b-v2".to_owned(), BackendKind::Cpu, None),
            (
                "parakeet-tdt-0.6b-v2".to_owned(),
                BackendKind::Cpu,
                Some(Verdict::Pass { comfortable: true })
            ),
        ]
    );
    assert_eq!(outcome.facts.cores, 12);
    assert_eq!(outcome.facts.accelerators[0].backend, BackendKind::Qnn);
    assert_eq!(
        outcome.facts.drivers.as_deref(),
        Some(&["a=1.0".to_owned()][..])
    );
    assert_eq!(outcome.trials.len(), 2);
    let (report, verdict) = &outcome.trials[1];
    assert_eq!(report.trial.model, "large-v3-turbo");
    assert!(report.trial.rtf.is_nan() && report.trial.wer_percent.is_nan());
    assert_eq!(report.failure.as_deref(), Some("out of memory"));
    assert_eq!(*verdict, Verdict::Wrong);
    let r = outcome.recommendation.unwrap();
    assert_eq!(
        (r.model, r.backend, r.fallback),
        (
            "parakeet-tdt-0.6b-v2",
            BackendKind::Cpu,
            Some("parakeet-tdt-110m")
        )
    );
}

#[test]
fn model_check_damaged_files_found_in_the_child_reach_the_parent() {
    // A refused library in the child marked nothing in the app: the child reports what it
    // found before its final line, whichever that is.
    let line = CheckLine::Damaged {
        artefacts: vec!["ort".into()],
    };
    let text = serde_json::to_string(&line).unwrap();
    assert_eq!(text, r#"{"line":"damaged","artefacts":["ort"]}"#);
    let (r, heard) = read(&[text.as_str(), DONE].join("\n"));
    assert!(r.is_ok());
    assert_eq!(heard.damaged, [vec!["ort".to_owned()]]);
    let error = r#"{"line":"error","message":"The speech helpers could not load."}"#;
    let (r, heard) = read(&[text.as_str(), error].join("\n"));
    assert_eq!(r.unwrap_err().kind, "model_check");
    assert_eq!(heard.damaged, [vec!["ort".to_owned()]]);
    // After the final line it is a line too many, as any other.
    let (r, _) = read(&[DONE, text.as_str()].join("\n"));
    assert!(r.is_err());
}

#[test]
fn model_check_damaged_line_marks_the_files_in_the_app() {
    // The job's events mark what the child reported in this process's catalogue, so the
    // System page offers the download at once; an id the catalogue does not hold is ignored.
    let dir = TempDir::new("check-damaged");
    let models = ModelDir::new(dir.0.join("models"));
    let catalogue = earshot_runtime::catalogue::Catalogue::embedded().unwrap();
    let known = catalogue.artefacts()[0].id.clone();
    let mut steps = Vec::new();
    let mut warnings = Vec::new();
    let mut events = JobLines {
        models: &models,
        progress: &mut |s: CheckStep| steps.push(s),
        warn: &mut |w: String| warnings.push(w),
    };
    events.damaged(vec![known.clone(), "no-such-artefact".into()]);
    assert_eq!(
        earshot_runtime::catalogue::Catalogue::found_damaged(&models),
        [known]
    );
}

#[test]
fn model_check_done_line_round_trips_the_outcome() {
    let (r, _) = read(DONE);
    let outcome = r.unwrap();
    let text = serde_json::to_string(&done_line(&outcome)).unwrap();
    let (again, _) = read(&text);
    let again = again.unwrap();
    assert_eq!(again.facts, outcome.facts);
    assert_eq!(again.recommendation, outcome.recommendation);
    assert_eq!(again.trials[0], outcome.trials[0]);
    assert_eq!(again.trials[1].1, outcome.trials[1].1);
    assert_eq!(text, DONE);
}

#[test]
fn model_check_a_truncated_stream_is_a_retryable_failure() {
    for text in [
        "",
        WARNING,
        &[WARNING, TESTING, TESTED].join("\n"),
        &DONE[..40],
    ] {
        let (r, _) = read(text);
        let e = r.unwrap_err();
        assert_eq!(e.kind, "model_check", "{text}");
        assert!(e.retryable, "{text}");
    }
}

/// A reader whose first read fails with the operating system's words.
struct BrokenPipe;

impl std::io::Read for BrokenPipe {
    fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
        Err(std::io::Error::other(
            "the pipe has been ended (os error 109)",
        ))
    }
}

/// The child's output that cannot be read says fixed copy, never the operating system's text.
#[test]
fn model_check_output_that_cannot_be_read_reports_fixed_copy() {
    let mut heard = Heard::default();
    let e = read_lines(std::io::BufReader::new(BrokenPipe), &mut heard, &mut || {})
        .expect_err("the read failed");
    assert_eq!(e.kind, "model_check");
    assert_eq!(e.message, crate::plain_text::INTERNAL);
    assert!(e.retryable);
    assert!(!e.message.contains("os error"), "{}", e.message);
}

#[test]
fn model_check_an_error_line_is_a_failure_with_its_message() {
    let (r, heard) = read(
        &[
            WARNING,
            r#"{"line":"error","message":"ONNX Runtime is missing"}"#,
        ]
        .join("\n"),
    );
    let e = r.unwrap_err();
    assert_eq!(
        (e.kind.as_str(), e.message.as_str(), e.retryable),
        ("model_check", "ONNX Runtime is missing", false)
    );
    assert_eq!(heard.warnings.len(), 1);
}

#[test]
fn model_check_an_unreadable_line_is_a_failure_not_a_panic() {
    for line in [
        r#"{"line":"progress","model":"x"}"#,
        r#"{"line":"warning","message":"a","extra":1}"#,
        "not json",
        r#"{"line":"testing","model":"x","backend":"tpu"}"#,
    ] {
        let (r, _) = read(&[TESTING, line, DONE].join("\n"));
        let e = r.unwrap_err();
        assert!(e.retryable, "{line}");
        // Fixed copy: serde's words (which quote the line) never reach the page (T12.56).
        assert_eq!(e.message, UNREADABLE_LINE, "{line}");
    }
    // A model the runtime does not know: fixed copy too, never the name the child wrote (T12.70).
    let (r, _) = read(&DONE.replace("large-v3-turbo", "whisper-v9"));
    let e = r.unwrap_err();
    assert!(e.retryable);
    assert_eq!(e.message, UNREADABLE_LINE);
    assert!(!e.message.contains("whisper-v9"), "{}", e.message);
    // Anything after the final line.
    let (r, _) = read(&[DONE, WARNING].join("\n"));
    assert!(r.unwrap_err().message.contains("after its last line"));
}

/// A child told to use CUDA that cannot ends in its one final error: a check on another order
/// would record a fingerprint the parent reads as out of date. Never a silent run without it.
#[test]
fn model_check_a_child_that_cannot_make_the_parents_choice_runs_no_check() {
    let dir = crate::testutil::TempDir::new("check-child-cuda");
    let args = ChildArgs {
        languages: vec!["en".into()],
        models: dir.0.clone(),
        cuda_transcription: true,
    };
    let mut out = Vec::new();
    let message = run_in_child(&args, &mut out).unwrap_err();
    assert!(
        !crate::plain_text::names_developer_tool(&message),
        "{message}"
    );
    assert!(!message.contains(".dll"), "{message}");
    assert!(out.is_empty(), "no line before the final one");
}

#[test]
fn model_check_hidden_mode_arguments() {
    let os = |v: &[&str]| v.iter().map(OsString::from).collect::<Vec<_>>();
    assert_eq!(parse_args(&os(&[])), None);
    assert_eq!(parse_args(&os(&["--other"])), None);
    let models = std::path::Path::new(r"C:\models dir");
    // The parent's choice reaches the child exactly as it was made.
    for cuda_transcription in [false, true] {
        let args = child_args(&["en".into(), "de".into()], models, cuda_transcription);
        assert_eq!(
            parse_args(&args),
            Some(Ok(ChildArgs {
                languages: vec!["en".into(), "de".into()],
                models: models.to_path_buf(),
                cuda_transcription,
            }))
        );
    }
    assert!(matches!(parse_args(&os(&[MODEL_CHECK_FLAG])), Some(Err(_))));
    assert!(matches!(
        parse_args(&os(&[MODEL_CHECK_FLAG, "--languages", "en", "--models"])),
        Some(Err(_))
    ));
    assert!(matches!(
        parse_args(&os(&[
            MODEL_CHECK_FLAG,
            "--languages",
            "en",
            "--models",
            "m",
            "--x",
            "y"
        ])),
        Some(Err(_))
    ));
}

#[test]
fn model_check_job_maps_a_fake_childs_lines_and_records_in_the_parent() {
    let dir = TempDir::new("check-child");
    let svc = services(&dir);
    let models = ModelDir::new(dir.0.join("models"));
    let (mut steps, mut warnings) = (Vec::new(), Vec::new());
    let mut events = JobLines {
        models: &models,
        progress: &mut |s: CheckStep| steps.push(s),
        warn: &mut |w: String| warnings.push(w),
    };
    let text = wire(&[WARNING, TESTING, TESTED, DONE].join("\n"));
    let outcome = read_lines(Cursor::new(text.as_bytes()), &mut events, &mut || {}).unwrap();
    let done = record_outcome(&svc, &models, &["en".into()], &outcome).unwrap();
    assert_eq!(warnings.len(), 1);
    assert_eq!(
        steps,
        [
            CheckStep {
                model: "parakeet-tdt-0.6b-v2".into(),
                runs_on: RunsOn::Processor,
                verdict: None
            },
            CheckStep {
                model: "parakeet-tdt-0.6b-v2".into(),
                runs_on: RunsOn::Processor,
                verdict: Some(UiVerdict::KeepsUp)
            },
        ]
    );
    assert_eq!(
        done.answer,
        Some(("parakeet-tdt-0.6b-v2".into(), RunsOn::Processor))
    );
    assert_eq!(done.trials, 2);
    let stored = stored_check(&svc.store()).unwrap().unwrap();
    assert_eq!(stored.model, "parakeet-tdt-0.6b-v2");
    assert_eq!(stored.fallback.as_deref(), Some("parakeet-tdt-110m"));
    assert_eq!(stored.trials.len(), 2);
}

#[test]
#[cfg(windows)]
fn model_check_child_job_kills_its_process_when_closed() {
    // The job object `run_child` puts the child in (`earshot_proc::KillOnClose`): a child
    // that would run for two minutes ends at once when the job closes (the exit code of a
    // kill-on-close is not a failure status, so the time is what tells).
    let started = std::time::Instant::now();
    let mut child = std::process::Command::new("ping")
        .args(["-n", "120", "127.0.0.1"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let job = earshot_proc::KillOnClose::new().unwrap();
    job.assign(&child).unwrap();
    assert!(
        child.try_wait().unwrap().is_none(),
        "still running in the job"
    );
    drop(job);
    child.wait().unwrap();
    assert!(
        started.elapsed() < std::time::Duration::from_secs(60),
        "killed by the job, not finished: {:?}",
        started.elapsed()
    );
}

#[test]
fn check_child_that_cannot_keep_the_cuda_choice_runs_no_check() {
    // A child told to use CUDA whose library then fell back would record an answer the parent
    // reads as out of date at every start; it runs none, as when it cannot make the choice.
    assert_eq!(choice_not_kept(true, true), Some(CUDA_CHOICE_NOT_KEPT));
    assert_eq!(choice_not_kept(true, false), None);
    assert_eq!(choice_not_kept(false, true), None, "no choice to keep");
    assert_eq!(choice_not_kept(false, false), None);
    assert!(CUDA_CHOICE_NOT_KEPT.ends_with('.'));
    assert!(
        !CUDA_CHOICE_NOT_KEPT.contains("  "),
        "{CUDA_CHOICE_NOT_KEPT}"
    );
}

/// The child's exit reaches the page as fixed copy, never the exit code or the operating
/// system's words.
#[cfg(windows)]
#[test]
fn model_check_a_failed_exit_reports_fixed_copy() {
    use std::os::windows::process::ExitStatusExt;
    use std::process::ExitStatus;

    assert_eq!(exit_verdict(Ok(ExitStatus::from_raw(0))), Ok(()));
    for status in [
        Ok(ExitStatus::from_raw(1)),
        Ok(ExitStatus::from_raw(0xC000_0409)),
        Err(std::io::Error::other("the handle is invalid (os error 6)")),
    ] {
        let e = exit_verdict(status).unwrap_err();
        assert_eq!(e.kind, "model_check");
        assert_eq!(e.message, crate::plain_text::INTERNAL);
        assert!(e.retryable);
    }
}

/// What an ended stream with no final line is: the failure a stalled child ends in too.
fn no_final_line() -> IpcError {
    read("").0.unwrap_err()
}

/// T14.11: a native library printing to the child's stdout never fails the check. Only lines
/// with the prefix are lines of the protocol.
#[test]
fn check_child_lines_without_the_prefix_are_not_read() {
    let text = [
        "onnxruntime: a notice nobody asked for",
        &format!("{LINE_PREFIX}{WARNING}"),
        "{\"line\":\"error\",\"message\":\"looks like a line, has no prefix\"}",
        "",
        "\u{feff}not even text a parser would like: {{{",
        &format!("{LINE_PREFIX}{TESTING}"),
        &format!("{LINE_PREFIX}{TESTED}"),
        &format!("{LINE_PREFIX}{DONE}"),
        "a library saying goodbye after the last line",
        "",
    ]
    .join("\n");
    let (r, heard) = read_raw(&text);
    let outcome = r.unwrap();
    assert_eq!(outcome.trials.len(), 2);
    assert_eq!(heard.warnings.len(), 1);
    assert_eq!(heard.trials.len(), 2);
    // Bytes that are not text are skipped the same way.
    let mut bytes = b"\xff\xfe native bytes\n".to_vec();
    bytes.extend_from_slice(format!("{LINE_PREFIX}{DONE}\n").as_bytes());
    let mut heard = Heard::default();
    assert!(read_lines(Cursor::new(bytes), &mut heard, &mut || {}).is_ok());
    // The prefix is what makes a line: the same lines without it are no lines at all, so the
    // stream ended without a final one.
    let (r, heard) = read_raw(&[WARNING, TESTING, TESTED, DONE].join("\n"));
    assert_eq!(r.unwrap_err(), no_final_line());
    assert!(heard.warnings.is_empty() && heard.trials.is_empty());
    // A line with the prefix is still held to the protocol.
    let (r, _) = read_raw(&format!("{LINE_PREFIX}not json\n"));
    assert_eq!(r.unwrap_err().message, UNREADABLE_LINE);
    let (r, _) = read_raw(&wire(&[DONE, WARNING].join("\n")));
    assert_eq!(r.unwrap_err().message, MORE_AFTER_LAST_LINE);
}

/// T14.11: what the child writes is what the parent reads, also right after a native library
/// left a line open.
#[test]
fn check_child_written_lines_are_the_lines_the_parent_reads() {
    let mut out: Vec<u8> = Vec::new();
    out.extend_from_slice(b"a native library's progress, no newline yet...");
    let warning: CheckLine = serde_json::from_str(WARNING).unwrap();
    let testing: CheckLine = serde_json::from_str(TESTING).unwrap();
    let done: CheckLine = serde_json::from_str(DONE).unwrap();
    emit(&mut out, &warning);
    out.extend_from_slice(b"more of it");
    emit(&mut out, &testing);
    emit(&mut out, &done);
    let text = String::from_utf8(out.clone()).unwrap();
    assert!(
        text.contains(&format!("\n{LINE_PREFIX}{WARNING}\n")),
        "{text}"
    );
    let mut heard = Heard::default();
    let outcome = read_lines(Cursor::new(out), &mut heard, &mut || {}).unwrap();
    assert_eq!(heard.warnings.len(), 1);
    assert_eq!(heard.trials.len(), 1);
    assert_eq!(outcome.trials.len(), 2);
    // The prefix is fixed text of this protocol, with its separating space.
    assert_eq!(LINE_PREFIX, "earshot-model-check ");
}

/// A line longer than any real one is a child that is not well, with or without the prefix.
#[test]
fn check_child_a_line_too_long_to_read_is_a_retryable_failure() {
    let long = vec![b'x'; usize::try_from(MAX_LINE_BYTES).unwrap() + 10];
    let mut heard = Heard::default();
    let e = read_lines(Cursor::new(long), &mut heard, &mut || {}).unwrap_err();
    assert_eq!((e.message.as_str(), e.retryable), (UNREADABLE_LINE, true));
}

/// A clock the test moves by hand: time awake and time asleep, kept apart. `awake` is what the
/// stall watch is given; `wall` counts both, as `Instant` does on Windows.
///
/// It also tells the test who read it. [`follow`] reads the clock first on the thread that reads
/// the child's lines (the start of the count), then once on that thread for every line of the
/// protocol, and on its watch thread at every look; so reads by the first reader count the lines
/// heard, and the other reads are the watch's looks.
#[derive(Clone, Default)]
struct FakeClock {
    awake_ms: Arc<AtomicU64>,
    asleep_ms: Arc<AtomicU64>,
    /// The thread that read the clock first: the one reading the child's lines.
    reader: Arc<std::sync::OnceLock<std::thread::ThreadId>>,
    reader_reads: Arc<AtomicU64>,
    watch_looks: Arc<AtomicU64>,
}

impl FakeClock {
    fn read_by_this_thread(&self) {
        let me = std::thread::current().id();
        if *self.reader.get_or_init(|| me) == me {
            self.reader_reads.fetch_add(1, Ordering::SeqCst);
        } else {
            self.watch_looks.fetch_add(1, Ordering::SeqCst);
        }
    }

    fn awake(&self) -> impl Fn() -> Duration + Sync + use<> {
        let me = self.clone();
        move || {
            me.read_by_this_thread();
            Duration::from_millis(me.awake_ms.load(Ordering::SeqCst))
        }
    }

    fn wall(&self) -> impl Fn() -> Duration + Sync + use<> {
        let me = self.clone();
        move || {
            me.read_by_this_thread();
            Duration::from_millis(
                me.awake_ms.load(Ordering::SeqCst) + me.asleep_ms.load(Ordering::SeqCst),
            )
        }
    }

    fn stay_awake(&self, d: Duration) {
        self.awake_ms.fetch_add(ms(d), Ordering::SeqCst);
    }

    fn sleep(&self, d: Duration) {
        self.asleep_ms.fetch_add(ms(d), Ordering::SeqCst);
    }

    /// Returns once the watch has looked at the clock twice more: it has seen the time as it is
    /// now and decided. The first call also proves the count has started.
    fn watched(&self) {
        let from = self.watch_looks.load(Ordering::SeqCst);
        wait_for("the watch to look at the clock", || {
            self.watch_looks.load(Ordering::SeqCst) >= from + 2
        });
    }

    /// Returns once `lines` lines of the protocol were heard (each a sign of life).
    fn heard(&self, lines: u64) {
        wait_for("the child's lines to be read", || {
            self.reader_reads.load(Ordering::SeqCst) > lines
        });
    }
}

/// Bounds a failing case only.
const GUARD: Duration = Duration::from_secs(60);

fn wait_for(what: &str, done: impl Fn() -> bool) {
    let started = std::time::Instant::now();
    while !done() {
        assert!(started.elapsed() < GUARD, "waited too long for {what}");
        std::thread::sleep(Duration::from_millis(2));
    }
}

/// T14.11, the rule alone: only time awake since the last line counts.
#[test]
fn check_child_stall_rule_counts_time_awake_since_the_last_line() {
    let limit = CHECK_STALL_LIMIT;
    let clock = FakeClock::default();
    let (awake, wall) = (clock.awake(), clock.wall());
    let heard = awake();
    assert!(!stalled(heard, awake(), limit));
    // A night asleep is no stall by the awake clock, and would be one by a clock that counts it.
    clock.sleep(Duration::from_secs(8 * 3600));
    assert!(!stalled(heard, awake(), limit));
    assert!(
        stalled(heard, wall(), limit),
        "the control: a wall clock stalls"
    );
    clock.stay_awake(limit - Duration::from_millis(1));
    assert!(!stalled(heard, awake(), limit));
    clock.stay_awake(Duration::from_millis(1));
    assert!(stalled(heard, awake(), limit));
    // A line heard now starts the count again.
    let heard = awake();
    assert!(!stalled(heard, awake(), limit));
    // A clock that went backwards never stalls a child.
    assert!(!stalled(
        Duration::from_secs(900),
        Duration::from_secs(5),
        limit
    ));
    // The limit is the documented ten minutes.
    assert_eq!(limit, Duration::from_secs(600));
}

/// A child's stdout the test writes to, ended only by its "kill" (or by the test).
struct FakeChild {
    stdout: Mutex<Option<std::io::PipeWriter>>,
    kills: AtomicU64,
}

impl FakeChild {
    fn start() -> (Arc<Self>, std::io::PipeReader) {
        let (reader, writer) = std::io::pipe().unwrap();
        (
            Arc::new(Self {
                stdout: Mutex::new(Some(writer)),
                kills: AtomicU64::new(0),
            }),
            reader,
        )
    }

    fn write(&self, text: &str) {
        let mut out = self.stdout.lock().unwrap();
        out.as_mut()
            .expect("the child is alive")
            .write_all(text.as_bytes())
            .unwrap();
    }

    /// The child's end: its stdout closes.
    fn exit(&self) {
        self.stdout.lock().unwrap().take();
    }

    fn kill(&self) {
        self.kills.fetch_add(1, Ordering::SeqCst);
        self.exit();
    }

    fn kills(&self) -> u64 {
        self.kills.load(Ordering::SeqCst)
    }

    /// Ends the child when the returned guard is dropped, so a test that fails while a thread
    /// still reads the child's output ends instead of waiting for that read.
    fn ended_on_drop(self: &Arc<Self>) -> EndedOnDrop {
        EndedOnDrop(Arc::clone(self))
    }
}

struct EndedOnDrop(Arc<FakeChild>);

impl Drop for EndedOnDrop {
    fn drop(&mut self) {
        self.0.exit();
    }
}

/// Line events that tell the test thread of each trial line as it is read.
struct Told(Sender<String>);

impl LineEvents for Told {
    fn damaged(&mut self, _: Vec<String>) {}
    fn warning(&mut self, _: String) {}
    fn trial(&mut self, model: &str, _: BackendKind, _: Option<Verdict>) {
        let _ = self.0.send(model.to_owned());
    }
}

fn next_trial(rx: &Receiver<String>) {
    match rx.recv_timeout(GUARD) {
        Ok(_) => {}
        Err(RecvTimeoutError::Timeout | RecvTimeoutError::Disconnected) => {
            panic!("no trial line was read")
        }
    }
}

/// T14.11: a child that stops writing is killed after the limit of time awake, never for time
/// asleep, and ends exactly as a child without a final line does.
#[test]
fn check_child_that_stops_answering_is_killed_and_ends_as_one_without_a_final_line() {
    let limit = CHECK_STALL_LIMIT;
    let clock = FakeClock::default();
    let (child, stdout) = FakeChild::start();
    let (tx, rx) = mpsc::channel();
    let cancel = CancelToken::new();
    let result = std::thread::scope(|scope| {
        let _ended = child.ended_on_drop();
        let (awake, killed) = (clock.awake(), Arc::clone(&child));
        let cancel = &cancel;
        let run = scope.spawn(move || {
            follow(
                std::io::BufReader::new(stdout),
                &mut Told(tx),
                cancel,
                &awake,
                limit,
                &|| killed.kill(),
            )
        });
        // The count has started.
        clock.watched();
        // Quiet, but not for long enough: nothing happens.
        clock.stay_awake(limit - Duration::from_millis(1));
        clock.watched();
        assert_eq!(child.kills(), 0);
        // A line is a sign of life: the count starts again from it.
        child.write(&format!("\n{LINE_PREFIX}{TESTING}\n"));
        next_trial(&rx);
        clock.stay_awake(limit - Duration::from_millis(1));
        clock.watched();
        assert_eq!(child.kills(), 0, "the limit counts from the last line");
        // What a native library prints is no sign of life.
        child.write("a library still talking\n");
        // The computer sleeps through the night: no stall.
        clock.sleep(Duration::from_secs(8 * 3600));
        clock.watched();
        assert_eq!(child.kills(), 0, "time asleep is not counted");
        // Awake for the rest of the limit with no line: the child is killed, once.
        clock.stay_awake(Duration::from_millis(1));
        wait_for("the kill", || child.kills() == 1);
        run.join().unwrap()
    });
    assert_eq!(child.kills(), 1);
    let e = result.unwrap_err();
    assert_eq!(e, no_final_line());
    assert_eq!(
        (e.kind.as_str(), e.message.as_str(), e.retryable),
        ("model_check", STOPPED_BEFORE_FINISHED, true)
    );
}

/// The control of the test above: given a clock that counts sleep, the same night kills the
/// child. So the test would notice a watch that reads the wrong clock.
#[test]
fn check_child_watch_given_a_clock_that_counts_sleep_kills_after_a_night() {
    let clock = FakeClock::default();
    let (child, stdout) = FakeChild::start();
    let (tx, _rx) = mpsc::channel();
    let cancel = CancelToken::new();
    let result = std::thread::scope(|scope| {
        let _ended = child.ended_on_drop();
        let (wall, killed) = (clock.wall(), Arc::clone(&child));
        let cancel = &cancel;
        let run = scope.spawn(move || {
            follow(
                std::io::BufReader::new(stdout),
                &mut Told(tx),
                cancel,
                &wall,
                CHECK_STALL_LIMIT,
                &|| killed.kill(),
            )
        });
        // The count has started, and nothing is wrong yet.
        clock.watched();
        assert_eq!(child.kills(), 0);
        clock.sleep(Duration::from_secs(8 * 3600));
        wait_for("the kill", || child.kills() == 1);
        run.join().unwrap()
    });
    assert_eq!(result.unwrap_err(), no_final_line());
}

/// A child that finishes is never killed, however long each quiet stretch below the limit; a
/// child that wrote its final line and then never exits is stopped like any other.
#[test]
fn check_child_that_finishes_is_not_killed_and_one_that_never_exits_is() {
    let limit = CHECK_STALL_LIMIT;
    for exits in [true, false] {
        let clock = FakeClock::default();
        let (child, stdout) = FakeChild::start();
        let (tx, rx) = mpsc::channel();
        let cancel = CancelToken::new();
        let result = std::thread::scope(|scope| {
            let _ended = child.ended_on_drop();
            let (awake, killed) = (clock.awake(), Arc::clone(&child));
            let cancel = &cancel;
            let run = scope.spawn(move || {
                follow(
                    std::io::BufReader::new(stdout),
                    &mut Told(tx),
                    cancel,
                    &awake,
                    limit,
                    &|| killed.kill(),
                )
            });
            clock.watched();
            for line in [TESTING, TESTED] {
                clock.stay_awake(limit - Duration::from_millis(1));
                clock.watched();
                child.write(&format!("\n{LINE_PREFIX}{line}\n"));
                next_trial(&rx);
            }
            clock.stay_awake(limit - Duration::from_millis(1));
            clock.watched();
            child.write(&format!("\n{LINE_PREFIX}{DONE}\n"));
            if exits {
                child.exit();
            } else {
                // Its final line is read (the count starts again from it), then nothing: no
                // exit. The limit passes with no further line.
                clock.heard(3);
                clock.watched();
                assert_eq!(child.kills(), 0);
                clock.stay_awake(limit - Duration::from_millis(1));
                clock.watched();
                assert_eq!(child.kills(), 0, "the limit counts from the final line too");
                clock.stay_awake(Duration::from_millis(1));
                wait_for("the kill", || child.kills() == 1);
            }
            run.join().unwrap()
        });
        if exits {
            assert_eq!(child.kills(), 0);
            assert_eq!(result.unwrap().trials.len(), 2);
        } else {
            assert_eq!(result.unwrap_err(), no_final_line());
        }
    }
}

/// A cancel kills the child at once, whatever the clock says; the caller names the cancel.
#[test]
fn check_child_cancel_kills_the_child_without_waiting_for_the_limit() {
    let clock = FakeClock::default();
    let (child, stdout) = FakeChild::start();
    let (tx, _rx) = mpsc::channel();
    let cancel = CancelToken::new();
    let result = std::thread::scope(|scope| {
        let _ended = child.ended_on_drop();
        let (awake, killed) = (clock.awake(), Arc::clone(&child));
        let token = &cancel;
        let run = scope.spawn(move || {
            follow(
                std::io::BufReader::new(stdout),
                &mut Told(tx),
                token,
                &awake,
                CHECK_STALL_LIMIT,
                &|| killed.kill(),
            )
        });
        clock.watched();
        assert_eq!(child.kills(), 0);
        cancel.cancel();
        wait_for("the kill", || child.kills() == 1);
        run.join().unwrap()
    });
    assert!(result.is_err());
    assert!(cancel.is_cancelled());
}

/// Model work whose check follows a child that never writes, on a clock already past the limit.
struct SilentChildWork {
    child: Arc<FakeChild>,
    stdout: Mutex<Option<std::io::PipeReader>>,
}

impl crate::model_jobs::ModelWork for SilentChildWork {
    fn download(
        &self,
        _: &earshot_runtime::Services,
        _: &str,
        _: &CancelToken,
        _: &mut dyn FnMut(u64, u64),
    ) -> Result<(), crate::model_jobs::DownloadEnd> {
        unreachable!("no download in this test")
    }

    fn check(
        &self,
        _: &earshot_runtime::Services,
        cancel: &CancelToken,
        progress: &mut dyn FnMut(CheckStep),
        warn: &mut dyn FnMut(String),
    ) -> Result<crate::model_jobs::CheckDone, IpcError> {
        let models = ModelDir::new(std::env::temp_dir().join("earshot-check-stall-none"));
        let mut events = JobLines {
            models: &models,
            progress,
            warn,
        };
        let stdout = self.stdout.lock().unwrap().take().expect("one check");
        let clock = FakeClock::default();
        let awake = clock.awake();
        // The start of the watch, then the limit passes with the computer awake.
        let started = AtomicBool::new(false);
        let late = move || {
            if started.swap(true, Ordering::SeqCst) {
                awake() + CHECK_STALL_LIMIT
            } else {
                awake()
            }
        };
        let child = Arc::clone(&self.child);
        follow(
            std::io::BufReader::new(stdout),
            &mut events,
            cancel,
            &late,
            CHECK_STALL_LIMIT,
            &|| child.kill(),
        )
        .map(|_| unreachable!("a silent child has no outcome"))
    }
}

/// T14.11: a stalled check ends its job with the one retryable `Failed` of a child without a
/// final line and gives its pass back, so a meeting can start again without a cancel.
#[test]
fn check_child_that_stalls_ends_the_job_and_frees_the_gate_for_a_meeting() {
    use crate::model_jobs::{ModelJobEvent, ModelJobs};

    let dir = TempDir::new("check-stall-gate");
    let svc = services(&dir);
    let (child, stdout) = FakeChild::start();
    let gate = crate::activity::WorkGate::new();
    let (tx, rx) = mpsc::channel();
    let jobs = ModelJobs::new(
        Arc::new(SilentChildWork {
            child: Arc::clone(&child),
            stdout: Mutex::new(Some(stdout)),
        }),
        Arc::new(move |e| {
            let _ = tx.send(e);
        }),
        gate.clone(),
    );
    jobs.start_check(&svc).unwrap();
    let event = rx.recv_timeout(GUARD).expect("the check's one event");
    assert_eq!(
        event,
        ModelJobEvent::Failed {
            model: None,
            message: STOPPED_BEFORE_FINISHED.into(),
            retryable: true,
        }
    );
    jobs.join();
    assert_eq!(child.kills(), 1, "killed through the job's kill, once");
    assert!(rx.try_recv().is_err(), "exactly one terminal event");
    // The pass is back: a meeting starts, and the check can be run again.
    drop(gate.enter_meeting().expect("no check holds the gate"));
    drop(gate.enter_check().expect("the check can run again"));
}
