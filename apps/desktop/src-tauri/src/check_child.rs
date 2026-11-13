//! The model check in a child process (T7.4e). The check loads every
//! candidate model, initialises ONNX Runtime (one library per process) and reads
//! whisper.cpp's process-wide log ring, so it never runs inside the app's own process: the
//! app starts its own exe in a hidden mode,
//!
//! `earshot.exe --model-check --languages en,de --models <dir> [--cuda-transcription]`,
//!
//! which [`child_main`] handles at the very start of `main`, before Tauri is built: it opens
//! no window and no store and never calls llama.cpp (`llama.dll` stays delay-loaded and
//! unloaded). It initialises ONNX Runtime for the check (`host::init_onnx_for_check`), runs
//! `model_bench::run_check` and writes one JSON line per event to stdout ([`CheckLine`]):
//! a `warning` for a library that could not load, `testing` and `tested` per trial, then
//! exactly one final `done` (the facts, every trial and the answer) and exit 0, or one final
//! `error` and a non-zero exit.
//!
//! `--cuda-transcription` is this app's own choice at start (`cuda::transcription_from_folder`,
//! never the setting), made again here before anything reads the order: the check's fingerprint
//! holds the accelerators, so a child that chose differently would record an answer the parent
//! reads as out of date. A child that cannot make the choice ends in `error`. The choice is made
//! by the time this runs: the gate refuses a check while the start runs (`activity`).
//!
//! The parent ([`run_child`]) starts the child with no console window, in a kill-on-close
//! job object (`earshot_proc::KillOnClose`, the workspace's one implementation; the
//! child's lines stream as it runs, so it is not `earshot_proc::run`, which returns the
//! output at the exit), reads its lines ([`read_lines`]) and records the answer itself
//! with `host::record_model_check` (the parent owns the store). A child that ends without a
//! final line, writes a line it cannot read, crashes or is killed is a retryable failure.

use std::ffi::OsString;
use std::io::{BufRead, Write};
use std::path::PathBuf;
use std::time::Duration;

use earshot_core::asr::{BackendKind, CancelToken};
use earshot_runtime::model_bench::{CheckOutcome, CheckProgress, TrialReport};
use earshot_runtime::model_check::{self, HardwareFacts, Recommendation, Trial, Verdict};
use serde::{Deserialize, Serialize};

use crate::ipc::IpcError;

/// The hidden mode's first argument.
pub const MODEL_CHECK_FLAG: &str = "--model-check";
/// The parent chose CUDA for transcription at its start, so the child does too.
pub const CUDA_TRANSCRIPTION_FLAG: &str = "--cuda-transcription";

/// A trial's verdict on the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VerdictLine {
    /// Correct and comfortably fast.
    PassComfortable,
    /// Correct and fast enough.
    Pass,
    /// Correct but too slow.
    TooSlow,
    /// Wrong output or it did not run.
    Wrong,
}

impl VerdictLine {
    fn of(v: Verdict) -> Self {
        match v {
            Verdict::Pass { comfortable: true } => Self::PassComfortable,
            Verdict::Pass { comfortable: false } => Self::Pass,
            Verdict::TooSlow => Self::TooSlow,
            Verdict::Wrong => Self::Wrong,
        }
    }

    /// The trial verdict.
    #[must_use]
    pub fn verdict(self) -> Verdict {
        match self {
            Self::PassComfortable => Verdict::Pass { comfortable: true },
            Self::Pass => Verdict::Pass { comfortable: false },
            Self::TooSlow => Verdict::TooSlow,
            Self::Wrong => Verdict::Wrong,
        }
    }
}

/// One trial on the wire (a measurement that is not a number is `null`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TrialLine {
    /// The model.
    pub model: String,
    /// The backend.
    pub backend: BackendKind,
    /// Decode time over audio time.
    pub rtf: Option<f64>,
    /// Word error rate (percent).
    pub wer_percent: Option<f64>,
    /// Why it did not run.
    pub failure: Option<String>,
    /// Load time (ms).
    pub load_ms: u64,
    /// How the backend was proven.
    pub evidence: Option<String>,
    /// What it means.
    pub verdict: VerdictLine,
}

impl TrialLine {
    fn of(report: &TrialReport, verdict: Verdict) -> Self {
        let finite = |x: f64| x.is_finite().then_some(x);
        Self {
            model: report.trial.model.to_owned(),
            backend: report.trial.backend,
            rtf: finite(report.trial.rtf),
            wer_percent: finite(report.trial.wer_percent),
            failure: report.failure.clone(),
            load_ms: u64::try_from(report.load.as_millis()).unwrap_or(u64::MAX),
            evidence: report.evidence.clone(),
            verdict: VerdictLine::of(verdict),
        }
    }
}

/// An accelerator of the facts on the wire.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AcceleratorLine {
    /// Its backend.
    pub backend: BackendKind,
    /// Free memory, when known.
    pub free_memory: Option<u64>,
}

/// The facts the check ran on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FactsLine {
    /// Accelerators, best first.
    pub accelerators: Vec<AcceleratorLine>,
    /// Logical cores.
    pub cores: usize,
    /// Total memory.
    pub memory: u64,
    /// GPU driver versions (`null`: the read failed).
    pub drivers: Option<Vec<String>>,
}

/// The check's answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AnswerLine {
    /// The model.
    pub model: String,
    /// Where it passed.
    pub backend: BackendKind,
    /// Its fallback.
    pub fallback: Option<String>,
}

/// One line the child writes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "line", rename_all = "snake_case", deny_unknown_fields)]
pub enum CheckLine {
    /// A notice for the user (an accelerator's library could not load).
    Warning {
        /// Plain copy.
        message: String,
    },
    /// A trial starts.
    Testing {
        /// The model.
        model: String,
        /// The backend.
        backend: BackendKind,
    },
    /// A trial ended.
    Tested {
        /// The trial.
        trial: TrialLine,
    },
    /// The check finished (final line).
    Done {
        /// The facts it ran on.
        facts: FactsLine,
        /// Every trial, in order.
        trials: Vec<TrialLine>,
        /// The answer (`null`: no installed model works here).
        answer: Option<AnswerLine>,
    },
    /// The check could not run (final line).
    Error {
        /// What failed.
        message: String,
    },
}

/// The arguments of the hidden mode: meeting languages and the models directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChildArgs {
    /// Meeting language codes.
    pub languages: Vec<String>,
    /// The models directory.
    pub models: PathBuf,
    /// The parent's choice at start: transcription runs on CUDA from the models folder.
    pub cuda_transcription: bool,
}

/// The hidden mode's arguments (after the program name), `None` when the first is not
/// [`MODEL_CHECK_FLAG`] (a normal start).
///
/// # Errors
/// The text of an error line: a missing or unknown argument.
pub fn parse_args(args: &[OsString]) -> Option<Result<ChildArgs, String>> {
    if args.first().is_none_or(|a| a != MODEL_CHECK_FLAG) {
        return None;
    }
    let mut languages = None;
    let mut models = None;
    let mut cuda_transcription = false;
    let mut rest = args[1..].iter();
    while let Some(a) = rest.next() {
        if a == CUDA_TRANSCRIPTION_FLAG {
            cuda_transcription = true;
            continue;
        }
        let value = rest.next();
        match (a.to_str(), value) {
            (Some("--languages"), Some(v)) => {
                languages = Some(
                    v.to_string_lossy()
                        .split(',')
                        .map(str::trim)
                        .filter(|l| !l.is_empty())
                        .map(str::to_owned)
                        .collect::<Vec<_>>(),
                );
            }
            (Some("--models"), Some(v)) => models = Some(PathBuf::from(v)),
            _ => return Some(Err(format!("unknown model check argument {a:?}"))),
        }
    }
    Some(match (languages, models) {
        (Some(l), Some(m)) if !l.is_empty() => Ok(ChildArgs {
            languages: l,
            models: m,
            cuda_transcription,
        }),
        _ => Err("the model check needs --languages and --models".into()),
    })
}

/// The arguments the parent passes for `languages`, `models` and its own choice at start.
#[must_use]
pub fn child_args(
    languages: &[String],
    models: &std::path::Path,
    cuda_transcription: bool,
) -> Vec<OsString> {
    let mut args: Vec<OsString> = vec![
        MODEL_CHECK_FLAG.into(),
        "--languages".into(),
        languages.join(",").into(),
        "--models".into(),
        models.as_os_str().to_owned(),
    ];
    if cuda_transcription {
        args.push(CUDA_TRANSCRIPTION_FLAG.into());
    }
    args
}

/// Writes one line and flushes; a parent that is gone ends the child at once.
fn emit(out: &mut dyn Write, line: &CheckLine) {
    let ok = serde_json::to_string(line)
        .ok()
        .is_some_and(|text| writeln!(out, "{text}").and_then(|()| out.flush()).is_ok());
    if !ok {
        std::process::exit(3);
    }
}

/// The hidden mode, if `args` (the process arguments after the program name) ask for it:
/// runs the check (module docs) and returns the exit code; `None` for a normal start.
#[must_use]
pub fn child_main(args: &[OsString]) -> Option<i32> {
    let parsed = parse_args(args)?;
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    let args = match parsed {
        Ok(a) => a,
        Err(message) => {
            emit(&mut out, &CheckLine::Error { message });
            return Some(2);
        }
    };
    Some(match run_in_child(&args, &mut out) {
        Ok(()) => 0,
        Err(message) => {
            emit(&mut out, &CheckLine::Error { message });
            1
        }
    })
}

fn run_in_child(args: &ChildArgs, out: &mut dyn Write) -> Result<(), String> {
    use earshot_runtime::backends::{ModelDir, accelerator_order};
    use earshot_runtime::host;
    let models = ModelDir::new(args.models.clone());
    let languages: Vec<&str> = args.languages.iter().map(String::as_str).collect();
    // Before anything reads the order. `false` changes nothing.
    earshot_runtime::cuda::use_for_transcription(&models, args.cuda_transcription)
        .map_err(|e| crate::plain_text::runtime_text(&e))?;
    let mut fell_back = false;
    host::init_onnx_for_check(&models, &accelerator_order(), &mut |library, e| {
        fell_back = true;
        emit(
            out,
            &CheckLine::Warning {
                message: crate::model_jobs::check_fallback_message(library, e),
            },
        );
    })
    .map_err(|e| crate::plain_text::runtime_text(&e))?;
    if let Some(message) = choice_not_kept(args.cuda_transcription, fell_back) {
        return Err(message.into());
    }
    let outcome = earshot_runtime::model_bench::run_check(&models, &languages, &mut |p| {
        emit(
            out,
            &match p {
                CheckProgress::Testing { model, backend } => CheckLine::Testing {
                    model: (*model).to_owned(),
                    backend: *backend,
                },
                CheckProgress::Tested { report, verdict } => CheckLine::Tested {
                    trial: TrialLine::of(report, *verdict),
                },
            },
        );
    })
    .map_err(|e| crate::plain_text::runtime_text(&e))?;
    emit(out, &done_line(&outcome));
    Ok(())
}

/// What a child says when it cannot keep the parent's CUDA choice ([`choice_not_kept`]).
pub const CUDA_CHOICE_NOT_KEPT: &str = "The model check could not load the CUDA libraries this \
     start chose for transcription, so it did not run. Restart Earshot to try again.";

/// Why the child runs no check: the parent chose CUDA for transcription (`cuda_transcription`) but
/// the library then fell back to the processor's. Its answer would hold a fingerprint without
/// CUDA, which the parent, holding CUDA, reads as out of date at every start, so the whole check
/// would run again each time; as for a child that cannot make the choice, it runs none.
#[must_use]
pub fn choice_not_kept(cuda_transcription: bool, fell_back: bool) -> Option<&'static str> {
    (cuda_transcription && fell_back).then_some(CUDA_CHOICE_NOT_KEPT)
}

/// The final line of a finished check.
#[must_use]
pub fn done_line(outcome: &CheckOutcome) -> CheckLine {
    CheckLine::Done {
        facts: FactsLine {
            accelerators: outcome
                .facts
                .accelerators
                .iter()
                .map(|a| AcceleratorLine {
                    backend: a.backend,
                    free_memory: a.free_memory,
                })
                .collect(),
            cores: outcome.facts.cores,
            memory: outcome.facts.memory,
            drivers: outcome.facts.drivers.clone(),
        },
        trials: outcome
            .trials
            .iter()
            .map(|(r, v)| TrialLine::of(r, *v))
            .collect(),
        answer: outcome.recommendation.as_ref().map(|r| AnswerLine {
            model: r.model.to_owned(),
            backend: r.backend,
            fallback: r.fallback.map(str::to_owned),
        }),
    }
}

/// What the user is told when they cancelled the check.
pub const CHECK_CANCELLED_MESSAGE: &str = "The model check was cancelled.";

/// The failure of a cancelled check (retryable: it can be started again).
#[must_use]
pub fn cancelled() -> IpcError {
    child_failed(CHECK_CANCELLED_MESSAGE)
}

/// A child failure as the job reports it (retryable: running the check again may work).
fn child_failed(message: impl Into<String>) -> IpcError {
    IpcError {
        kind: "model_check".into(),
        message: message.into(),
        retryable: true,
    }
}

/// A manifest model's name as the runtime's tables hold it.
fn known_model(name: &str) -> Result<&'static str, IpcError> {
    model_check::model_facts(name)
        .map(|f| f.name)
        .ok_or_else(|| child_failed(format!("the model check named an unknown model {name:?}")))
}

fn trial_report(t: &TrialLine) -> Result<(TrialReport, Verdict), IpcError> {
    Ok((
        TrialReport {
            trial: Trial {
                model: known_model(&t.model)?,
                backend: t.backend,
                rtf: t.rtf.unwrap_or(f64::NAN),
                wer_percent: t.wer_percent.unwrap_or(f64::NAN),
            },
            failure: t.failure.clone(),
            load: Duration::from_millis(t.load_ms),
            evidence: t.evidence.clone(),
        },
        t.verdict.verdict(),
    ))
}

/// The check outcome of a `done` line, for `host::record_model_check`.
///
/// # Errors
/// A model name the runtime does not know (retryable failure).
pub fn outcome_of(
    facts: &FactsLine,
    trials: &[TrialLine],
    answer: Option<&AnswerLine>,
) -> Result<CheckOutcome, IpcError> {
    Ok(CheckOutcome {
        facts: HardwareFacts {
            accelerators: facts
                .accelerators
                .iter()
                .map(|a| model_check::Accelerator {
                    backend: a.backend,
                    free_memory: a.free_memory,
                })
                .collect(),
            cores: facts.cores,
            memory: facts.memory,
            drivers: facts.drivers.clone(),
        },
        trials: trials.iter().map(trial_report).collect::<Result<_, _>>()?,
        recommendation: answer
            .map(|a| -> Result<Recommendation, IpcError> {
                Ok(Recommendation {
                    model: known_model(&a.model)?,
                    backend: a.backend,
                    fallback: a.fallback.as_deref().map(known_model).transpose()?,
                    missing: Vec::new(),
                })
            })
            .transpose()?,
    })
}

/// What the parent hears from the child's lines.
pub trait LineEvents {
    /// A `warning` line.
    fn warning(&mut self, message: String);
    /// A `testing` or `tested` line.
    fn trial(&mut self, model: &str, backend: BackendKind, verdict: Option<Verdict>);
}

/// Reads the child's stdout until its final line: `warning`, `testing` and `tested` go to
/// `events`, a `done` line is the outcome, an `error` line a failure (not retryable: the
/// check itself could not run). A stream that ends without a final line, a line that does
/// not parse (an unknown kind included) or a line after the final one is a retryable
/// failure; nothing here panics.
///
/// # Errors
/// As described.
pub fn read_lines(
    reader: impl BufRead,
    events: &mut dyn LineEvents,
) -> Result<CheckOutcome, IpcError> {
    let mut lines = reader.lines();
    while let Some(line) = lines.next() {
        let line = line.map_err(|e| child_failed(format!("the model check's output: {e}")))?;
        if line.trim().is_empty() {
            continue;
        }
        let parsed: CheckLine = serde_json::from_str(&line).map_err(|e| {
            child_failed(format!(
                "the model check wrote a line Earshot cannot read ({e})"
            ))
        })?;
        let end = match parsed {
            CheckLine::Warning { message } => {
                events.warning(message);
                continue;
            }
            CheckLine::Testing { model, backend } => {
                events.trial(&model, backend, None);
                continue;
            }
            CheckLine::Tested { trial } => {
                events.trial(&trial.model, trial.backend, Some(trial.verdict.verdict()));
                continue;
            }
            CheckLine::Done {
                facts,
                trials,
                answer,
            } => outcome_of(&facts, &trials, answer.as_ref()),
            CheckLine::Error { message } => Err(IpcError {
                kind: "model_check".into(),
                message,
                retryable: false,
            }),
        };
        if lines.any(|l| l.is_ok_and(|l| !l.trim().is_empty())) {
            return Err(child_failed(
                "the model check wrote more after its last line",
            ));
        }
        return end;
    }
    Err(child_failed(
        "The model check stopped before it finished (it may have crashed or been stopped).",
    ))
}

/// Runs the check in a child process (module docs) for `languages` and `models`: the
/// app's own exe in the hidden mode, no console window, in a kill-on-close job; `cancel`
/// kills it. Returns the outcome for the parent to record.
///
/// # Errors
/// The child cannot start; any failure of [`read_lines`]; a child that exits with a
/// failure status after its last line; a cancel (retryable).
pub fn run_child(
    languages: &[String],
    models: &std::path::Path,
    cancel: &CancelToken,
    events: &mut dyn LineEvents,
) -> Result<CheckOutcome, IpcError> {
    #[cfg(windows)]
    {
        use std::sync::atomic::{AtomicBool, Ordering};

        /// `CREATE_NO_WINDOW`: the child gets no console window.
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;

        let exe = std::env::current_exe()
            .map_err(|e| child_failed(format!("cannot find Earshot's own program: {e}")))?;
        let job = earshot_proc::KillOnClose::new()
            .map_err(|e| child_failed(format!("cannot set up the model check: {e}")))?;
        let mut command = std::process::Command::new(exe);
        command
            .args(child_args(
                languages,
                models,
                earshot_runtime::cuda::transcription_from_folder(),
            ))
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null());
        // Started suspended and resumed only once in the job: nothing it starts runs outside.
        let mut child = job
            .spawn(&mut command, CREATE_NO_WINDOW)
            .map_err(|e| match e {
                earshot_proc::SpawnError::Start(e) => {
                    child_failed(format!("cannot start the model check: {e}"))
                }
                earshot_proc::SpawnError::Job(e) => {
                    child_failed(format!("cannot set up the model check: {e}"))
                }
            })?;
        let Some(stdout) = child.stdout.take() else {
            drop(job);
            let _ = child.wait();
            return Err(child_failed("the model check has no output"));
        };
        let reading = AtomicBool::new(true);
        let result = std::thread::scope(|scope| {
            // Kills the child when the user cancels; ends with the read.
            scope.spawn(|| {
                while reading.load(Ordering::SeqCst) {
                    if cancel.is_cancelled() {
                        job.terminate();
                        return;
                    }
                    std::thread::sleep(Duration::from_millis(50));
                }
            });
            let r = read_lines(std::io::BufReader::new(stdout), events);
            reading.store(false, Ordering::SeqCst);
            r
        });
        let status = child.wait();
        drop(job);
        if cancel.is_cancelled() {
            return Err(cancelled());
        }
        let outcome = result?;
        match status {
            Ok(s) if s.success() => Ok(outcome),
            Ok(s) => Err(child_failed(format!("the model check ended with {s}"))),
            Err(e) => Err(child_failed(format!("the model check: {e}"))),
        }
    }
    #[cfg(not(windows))]
    {
        let _ = (languages, models, cancel, events);
        Err(IpcError {
            kind: "unsupported".into(),
            message: "the model check runs on Windows only".into(),
            retryable: false,
        })
    }
}

#[cfg(test)]
#[path = "check_child_tests.rs"]
mod tests;
