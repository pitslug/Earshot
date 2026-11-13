//! The model check child's line protocol (T7.4e), with fixed text: every line kind, a
//! truncated stream, an error line, an unknown kind; and the job mapping over a fake child.

use std::io::Cursor;

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

fn read(text: &str) -> (Result<CheckOutcome, IpcError>, Heard) {
    let mut heard = Heard::default();
    let r = read_lines(Cursor::new(text.as_bytes()), &mut heard);
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
        assert!(e.message.contains("cannot read"), "{line}: {}", e.message);
    }
    // A model the runtime does not know.
    let (r, _) = read(&DONE.replace("large-v3-turbo", "whisper-v9"));
    assert!(r.unwrap_err().message.contains("unknown model"));
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
    let text = [WARNING, TESTING, TESTED, DONE].join("\n");
    let outcome = read_lines(Cursor::new(text.as_bytes()), &mut events).unwrap();
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
