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
//! `error` and a non-zero exit. Native libraries may print to the same stream, so every line is
//! written after a newline with [`LINE_PREFIX`], and the parent reads no line without it (T14.11,
//! as `earshot_runtime::summary_child` does).
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
//!
//! A child that writes no line of the protocol for [`CHECK_STALL_LIMIT`] of time awake (a trial
//! hung in a driver) is killed through its job and ends as a child without a final line does
//! ([`follow`], T14.11): the check holds a pass that refuses every meeting start, so it never
//! waits for the user to cancel it.

use std::ffi::OsString;
use std::io::{BufRead, Read, Write};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
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

/// What starts every line of the protocol (module docs). Parent and child are one exe, so they
/// change together.
pub const LINE_PREFIX: &str = "earshot-model-check ";

/// The longest line read: a `done` line with every trial is a few kilobytes; past this the
/// child is not well.
const MAX_LINE_BYTES: u64 = 4 * 1024 * 1024;

/// How long the child may write no line of the protocol, in time awake, before it is stopped.
/// A judgement, not a measurement: the longest quiet stretch of a healthy check is one trial
/// (a model load and the bundled clip's decode) or the start before the first trial (the CUDA
/// files' verification and ONNX Runtime's start), each a few minutes at worst on a slow
/// computer, and ten minutes is what a model load is given elsewhere (the summary's
/// `GPU_LOAD_LIMIT`, the work after a meeting). A trial that is merely slow is cut too, and the
/// check can be run again.
pub const CHECK_STALL_LIMIT: Duration = Duration::from_secs(600);

/// How often the parent looks at the cancel token and the stall limit.
const WATCH_POLL: Duration = Duration::from_millis(50);

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
    /// Files a refused load in the child found damaged (`Catalogue::found_damaged`), written
    /// just before the final line, so the app marks them too.
    Damaged {
        /// The artefact ids.
        artefacts: Vec<String>,
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

/// One line of the protocol as the child writes it: the newline first, because a native
/// library may have left a line open, then [`LINE_PREFIX`] and the JSON.
fn wire_line(line: &CheckLine) -> Option<String> {
    let text = serde_json::to_string(line).ok()?;
    Some(format!("\n{LINE_PREFIX}{text}\n"))
}

/// Writes one line (in one write) and flushes; a parent that is gone ends the child at once.
fn emit(out: &mut dyn Write, line: &CheckLine) {
    let ok = wire_line(line).is_some_and(|whole| {
        out.write_all(whole.as_bytes())
            .and_then(|()| out.flush())
            .is_ok()
    });
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
    let result = run_in_child(&args, &mut out);
    // A refused load marked these in this process only: the app hears them before the end.
    let artefacts = earshot_runtime::catalogue::Catalogue::found_damaged(
        &earshot_runtime::backends::ModelDir::new(args.models.clone()),
    );
    if !artefacts.is_empty() {
        emit(&mut out, &CheckLine::Damaged { artefacts });
    }
    Some(match result {
        Ok(done) => {
            emit(&mut out, &done);
            0
        }
        Err(message) => {
            emit(&mut out, &CheckLine::Error { message });
            1
        }
    })
}

/// Runs the check and returns its final `done` line (written by the caller).
fn run_in_child(args: &ChildArgs, out: &mut dyn Write) -> Result<CheckLine, String> {
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
    Ok(done_line(&outcome))
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

/// What the page says when the check's child writes a line this version cannot read.
pub const UNREADABLE_LINE: &str = "The model check wrote a line Earshot cannot read.";

/// What the page says when the check's child ended without its final line, also when it was
/// stopped for writing nothing for [`CHECK_STALL_LIMIT`].
pub const STOPPED_BEFORE_FINISHED: &str =
    "The model check stopped before it finished (it may have crashed or been stopped).";

/// What the page says when the check's child wrote a line after its final one.
pub const MORE_AFTER_LAST_LINE: &str = "The model check wrote more after its last line.";

/// The failure of a child that ended without its final line (retryable).
#[must_use]
pub fn stopped_before_finished() -> IpcError {
    child_failed(STOPPED_BEFORE_FINISHED)
}

/// A child failure as the job reports it (retryable: running the check again may work).
fn child_failed(message: impl Into<String>) -> IpcError {
    IpcError {
        kind: "model_check".into(),
        message: message.into(),
        retryable: true,
    }
}

/// A manifest model's name as the runtime's tables hold it. A name this version does not know is
/// a line it cannot read: fixed copy, never the name (T12.70).
fn known_model(name: &str) -> Result<&'static str, IpcError> {
    model_check::model_facts(name)
        .map(|f| f.name)
        .ok_or_else(|| child_failed(UNREADABLE_LINE))
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
    /// A `damaged` line: files the child found damaged, to be marked in this process.
    fn damaged(&mut self, artefacts: Vec<String>);
}

/// Reads the child's stdout to its end: lines without [`LINE_PREFIX`] (a native library's
/// output, or nothing) are not read and not kept; `warning`, `testing` and `tested` go to
/// `events`, a `done` line is the outcome, an `error` line a failure (not retryable: the
/// check itself could not run). A stream that ends without a final line, a line of the protocol
/// that does not parse (an unknown kind included) or one after the final line is a retryable
/// failure; nothing here panics. `heard` is called for every line of the protocol as it arrives:
/// the child's sign of life for [`follow`]. A line without the prefix is no sign of life.
///
/// # Errors
/// As described.
pub fn read_lines(
    mut reader: impl BufRead,
    events: &mut dyn LineEvents,
    heard: &mut dyn FnMut(),
) -> Result<CheckOutcome, IpcError> {
    let mut end: Option<Result<CheckOutcome, IpcError>> = None;
    let mut line = Vec::new();
    loop {
        line.clear();
        let read = reader
            .by_ref()
            .take(MAX_LINE_BYTES)
            .read_until(b'\n', &mut line);
        match read {
            Ok(0) => break,
            Ok(_) => {}
            // Only the child's exit is left after its final line.
            Err(_) if end.is_some() => break,
            // The operating system's words never reach the page (T12.53).
            Err(_) => return Err(child_failed(crate::plain_text::INTERNAL)),
        }
        if line.last() != Some(&b'\n') && line.len() as u64 >= MAX_LINE_BYTES {
            return Err(child_failed(UNREADABLE_LINE));
        }
        let Some(text) = line.strip_prefix(LINE_PREFIX.as_bytes()) else {
            continue;
        };
        heard();
        if end.is_some() {
            return Err(child_failed(MORE_AFTER_LAST_LINE));
        }
        // serde's words name the line's contents and never reach the page (T12.56).
        let parsed: CheckLine =
            serde_json::from_slice(text).map_err(|_| child_failed(UNREADABLE_LINE))?;
        match parsed {
            CheckLine::Warning { message } => events.warning(message),
            CheckLine::Testing { model, backend } => events.trial(&model, backend, None),
            CheckLine::Tested { trial } => {
                events.trial(&trial.model, trial.backend, Some(trial.verdict.verdict()));
            }
            CheckLine::Damaged { artefacts } => events.damaged(artefacts),
            CheckLine::Done {
                facts,
                trials,
                answer,
            } => end = Some(outcome_of(&facts, &trials, answer.as_ref())),
            CheckLine::Error { message } => {
                end = Some(Err(IpcError::new("model_check", message)));
            }
        }
    }
    end.unwrap_or_else(|| Err(stopped_before_finished()))
}

/// Where the parent reads time awake for the stall rule: a monotonic count from any origin that
/// leaves out time the computer slept ([`awake_clock`]), or a fake in tests.
pub type AwakeClock = Arc<dyn Fn() -> Duration + Send + Sync>;

/// The production awake clock: `earshot_proc::awake_time` (Windows' unbiased interrupt time) when
/// it can be read, else time since this clock was made by `Instant`, which counts a sleep.
/// Chosen once, so one clock never mixes the two (T12.62).
#[must_use]
pub fn awake_clock() -> AwakeClock {
    if earshot_proc::awake_time().is_some() {
        Arc::new(|| earshot_proc::awake_time().unwrap_or_default())
    } else {
        let origin = std::time::Instant::now();
        Arc::new(move || origin.elapsed())
    }
}

/// Whether a child last heard at `heard` (time awake) has stalled by `now`: `limit` of time
/// awake has passed with no line of the protocol. A clock that went backwards never stalls it.
#[must_use]
pub fn stalled(heard: Duration, now: Duration, limit: Duration) -> bool {
    now.saturating_sub(heard) >= limit
}

fn ms(d: Duration) -> u64 {
    u64::try_from(d.as_millis()).unwrap_or(u64::MAX)
}

/// Reads the child's lines ([`read_lines`]) while watching it: `kill` is called once, and the
/// reading then ends with the child's output, when `cancel` is set or when the child has written
/// no line of the protocol for `limit` of time awake (`awake`; counted from this call, then from
/// each line, the final one included, so a child that never exits is stopped too). A stalled
/// child ends as one that wrote no final line: [`stopped_before_finished`], retryable. The
/// caller says what a cancel ends in.
///
/// # Errors
/// As [`read_lines`]; a stall as described.
pub fn follow(
    reader: impl BufRead,
    events: &mut dyn LineEvents,
    cancel: &CancelToken,
    awake: &(dyn Fn() -> Duration + Sync),
    limit: Duration,
    kill: &(dyn Fn() + Sync),
) -> Result<CheckOutcome, IpcError> {
    let reading = AtomicBool::new(true);
    let gave_up = AtomicBool::new(false);
    let heard_ms = AtomicU64::new(ms(awake()));
    let result = std::thread::scope(|scope| {
        // Ends with the read, or kills the child, which ends the read.
        scope.spawn(|| {
            while reading.load(Ordering::SeqCst) {
                if cancel.is_cancelled() {
                    kill();
                    return;
                }
                let heard = Duration::from_millis(heard_ms.load(Ordering::SeqCst));
                if stalled(heard, awake(), limit) {
                    gave_up.store(true, Ordering::SeqCst);
                    kill();
                    return;
                }
                std::thread::sleep(WATCH_POLL);
            }
        });
        let r = read_lines(reader, events, &mut || {
            heard_ms.store(ms(awake()), Ordering::SeqCst);
        });
        reading.store(false, Ordering::SeqCst);
        r
    });
    if gave_up.load(Ordering::SeqCst) {
        return Err(stopped_before_finished());
    }
    result
}

/// How the child's exit reaches the page: a success is nothing, anything else fixed copy, never
/// the exit code's or the operating system's words (T12.53).
///
/// # Errors
/// A failed exit or wait, retryable.
#[cfg(windows)]
fn exit_verdict(status: std::io::Result<std::process::ExitStatus>) -> Result<(), IpcError> {
    match status {
        Ok(s) if s.success() => Ok(()),
        Ok(_) | Err(_) => Err(child_failed(crate::plain_text::INTERNAL)),
    }
}

/// Runs the check in a child process (module docs) for `languages` and `models`: the
/// app's own exe in the hidden mode, no console window, in a kill-on-close job; `cancel`
/// kills it, and so does [`CHECK_STALL_LIMIT`] with no line ([`follow`]). Returns the outcome
/// for the parent to record.
///
/// # Errors
/// The child cannot start; any failure of [`read_lines`]; a child that exits with a
/// failure status after its last line; a cancel or a stall (retryable).
pub fn run_child(
    languages: &[String],
    models: &std::path::Path,
    cancel: &CancelToken,
    events: &mut dyn LineEvents,
) -> Result<CheckOutcome, IpcError> {
    #[cfg(windows)]
    {
        /// `CREATE_NO_WINDOW`: the child gets no console window.
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;

        // A child that cannot start says fixed copy, never the operating system's words
        // (T12.53).
        let exe = std::env::current_exe().map_err(|_| child_failed(crate::plain_text::INTERNAL))?;
        let job = earshot_proc::KillOnClose::new()
            .map_err(|_| child_failed(crate::plain_text::INTERNAL))?;
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
            .map_err(|_| child_failed(crate::plain_text::INTERNAL))?;
        let Some(stdout) = child.stdout.take() else {
            drop(job);
            let _ = child.wait();
            return Err(child_failed("the model check has no output"));
        };
        let awake = awake_clock();
        // A cancel or a stall kills everything in the job; the pipe then ends, and so the read.
        let result = follow(
            std::io::BufReader::new(stdout),
            events,
            cancel,
            &*awake,
            CHECK_STALL_LIMIT,
            &|| job.terminate(),
        );
        let status = child.wait();
        drop(job);
        if cancel.is_cancelled() {
            return Err(cancelled());
        }
        let outcome = result?;
        exit_verdict(status).map(|()| outcome)
    }
    #[cfg(not(windows))]
    {
        let _ = (languages, models, cancel, events);
        Err(IpcError::new(
            "unsupported",
            "the model check runs on Windows only",
        ))
    }
}

#[cfg(test)]
#[path = "check_child_tests.rs"]
mod tests;
