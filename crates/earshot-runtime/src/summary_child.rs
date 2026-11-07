//! GPU summaries in a child process (T9.3).
//!
//! llama.cpp cannot be interrupted inside a GPU call, and a driver fault or a ggml abort
//! (`0xC0000409`, docs/cuda-install.md) ends the process with no error Rust could catch. So when
//! the app has registered its own exe ([`use_this_exe`]), a local summary on the graphics card
//! runs in a child, the same way as the model check's (`check_child` in the desktop app): the exe
//! in the hidden mode [`SUMMARY_CHILD_FLAG`], handled first in `main` before anything of the app
//! is built ([`child_main`]). A stalled child is killed, which frees its graphics memory; a dead
//! child costs only that summary, which moves to the processor in the parent and says so. The
//! recording and the transcript are in the parent's store, which the child never opens.
//!
//! # Privacy
//!
//! The command line holds the flag and nothing else. The job, the transcript text included, goes
//! to the child only on its standard input, and the summary comes back only on its standard
//! output; both are pipes to this process. The child logs nothing and makes no connection, and the
//! only file it can write is the crash log a panic leaves, the same location-only file as the app's
//! (`crash` in the desktop app). Its standard error goes nowhere, and output that is not one of its
//! lines is dropped unread. Only the child's own failure `Display` text enters an error, the same
//! text an in-process failure would give, which holds no transcript text; a dead child is described
//! by fixed words and its exit code.
//!
//! # The protocol
//!
//! The parent writes one JSON document ([`ChildJob`]) and closes the pipe. The child answers with
//! [`ChildLine`]s: `loaded` when the model is on the GPU, a `beat` whenever llama.cpp's heartbeat
//! moved (a prompt batch or a reply token), and exactly one final line, `done`, `load_refused` or
//! `failed`, then exit 0. Native libraries may print to the same stream, so every line is written
//! after a newline with [`LINE_PREFIX`]; anything without the prefix is not a line of the
//! protocol. A prefixed line that does not parse, a line after the final one, a stream that ends
//! without a final line or an exit other than 0 is a dead child, also after a `done` line (as
//! `llama_probe`: a crash after the line still is a crash).
//!
//! # Limits
//!
//! The limits of [`crate::summary::LocalSummaryConfig`] apply: `load_stall_limit` until `loaded`,
//! then `stall_limit` since the last `beat`. A stalled child is killed and its exit confirmed. A
//! child still there after [`KILL_WAIT`] is stuck in the driver and may hold the card, so only then
//! the card stays off for the session ([`crate::summary::LocalSummaryError::GpuStalled`]). Cancelling
//! kills the child at once: a child holds nothing worth saving.
//!
//! # Who does not use it
//!
//! The CPU path never does: a slow processor is not a stall and its library does not touch a
//! driver. `earshot.exe` (a developer tool) does not register an exe, so it keeps the in-process
//! watch of T8.4ab. The child uses the same set of llama.cpp's DLLs as this process
//! ([`earshot_llm_local::loaded_folder`]), read after llama.cpp's gate has opened. Before a child
//! loads the CUDA folder the parent checks its names and sizes ([`crate::cuda`]); the start's full
//! verify and trial cover the bytes. A folder that fails runs the summary on the CPU with a note.

use std::ffi::OsString;
use std::io::{BufRead, Read, Write};
use std::ops::Range;
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::time::{Duration, Instant};

use earshot_core::asr::CancelToken;
use earshot_core::prompt::SummaryInput;
use earshot_core::summary::SummaryV1;
use earshot_llm_local::summarize::{
    SummaryReport, SummaryRun, map_last_chunk, summarize_chunks_with,
};
use earshot_llm_local::{Heartbeat, LlamaBackend};
use serde::{Deserialize, Serialize};

use crate::summary::{LlamaLoader, LoadLocal, LoadedModel, LocalSummaryConfig, options_of};

#[cfg(all(test, windows))]
#[path = "summary_child_tests.rs"]
mod tests;

/// The hidden mode's only argument.
pub const SUMMARY_CHILD_FLAG: &str = "--summary-child";

/// What starts every line of the protocol (module docs).
const LINE_PREFIX: &str = "earshot-summary-child ";

/// The longest line read: a summary is far smaller; past this the child is not well.
const MAX_LINE_BYTES: u64 = 64 * 1024 * 1024;

/// How long a killed child may take to go, and a finished one to exit, a judgement.
pub(crate) const KILL_WAIT: Duration = Duration::from_secs(10);

/// How often the parent looks at the child, and the child at its heartbeat.
const POLL: Duration = Duration::from_millis(50);

/// A child exit that means the job could not be read.
const EXIT_BAD_JOB: i32 = 2;

/// A child exit that means the parent is gone.
const EXIT_NO_PARENT: i32 = 3;

/// The program that runs GPU summaries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GpuChild {
    /// The executable.
    pub program: PathBuf,
    /// Its arguments: fixed words, never meeting text.
    pub args: Vec<OsString>,
    /// Variables set for it (tests only; empty in production).
    pub env: Vec<(OsString, OsString)>,
}

static THIS_EXE: OnceLock<GpuChild> = OnceLock::new();

/// Makes this exe the child of every later GPU summary. The exe must call [`child_main`] first
/// thing in `main`. If the exe cannot find itself nothing changes: summaries stay in process.
pub fn use_this_exe() {
    if let Some(child) = this_exe() {
        let _ = THIS_EXE.set(child);
    }
}

/// This exe in the hidden mode.
fn this_exe() -> Option<GpuChild> {
    std::env::current_exe().ok().map(|program| GpuChild {
        program,
        args: vec![OsString::from(SUMMARY_CHILD_FLAG)],
        env: Vec::new(),
    })
}

/// The child [`use_this_exe`] registered.
#[must_use]
pub fn registered() -> Option<GpuChild> {
    THIS_EXE.get().cloned()
}

/// What the child is asked to do.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum ChildWork {
    /// A whole summary, with the windows already mapped during the meeting.
    Whole {
        /// The time chunks.
        ranges: Vec<Range<usize>>,
        /// One entry per chunk.
        done: Vec<Option<SummaryV1>>,
    },
    /// A live window: map the last of these chunks.
    LastChunk {
        /// The time chunks.
        ranges: Vec<Range<usize>>,
    },
}

/// What the child reads from its standard input.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ChildJob {
    /// The GGUF file.
    pub(crate) model_path: PathBuf,
    /// The model's name.
    pub(crate) model_id: String,
    /// CPU threads.
    pub(crate) threads: i32,
    /// Context size.
    pub(crate) context_tokens: u32,
    /// Largest reply budget.
    pub(crate) max_output_tokens: usize,
    /// Where the reply starts.
    pub(crate) assistant_prefix: String,
    /// GPUs the load must not end up on.
    pub(crate) refuse_gpus: Vec<String>,
    /// The folder of llama.cpp's DLLs this process runs from; `None`: the set beside the exe.
    pub(crate) llama_folder: Option<PathBuf>,
    /// What to do.
    pub(crate) work: ChildWork,
    /// The meeting's text.
    pub(crate) input: SummaryInput,
}

impl ChildJob {
    /// The settings of the load in the child.
    fn config(&self) -> LocalSummaryConfig {
        LocalSummaryConfig {
            gpu: true,
            threads: self.threads,
            context_tokens: self.context_tokens,
            max_output_tokens: self.max_output_tokens,
            assistant_prefix: self.assistant_prefix.clone(),
            refuse_gpus: self.refuse_gpus.clone(),
            ..LocalSummaryConfig::new(&self.model_path, &self.model_id)
        }
    }
}

/// Where the model is on the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum BackendLine {
    Cpu,
    Cuda,
    Vulkan,
}

impl BackendLine {
    fn of(backend: LlamaBackend) -> Self {
        match backend {
            LlamaBackend::Cpu => Self::Cpu,
            LlamaBackend::Cuda => Self::Cuda,
            LlamaBackend::Vulkan => Self::Vulkan,
        }
    }

    fn backend(self) -> LlamaBackend {
        match self {
            Self::Cpu => LlamaBackend::Cpu,
            Self::Cuda => LlamaBackend::Cuda,
            Self::Vulkan => LlamaBackend::Vulkan,
        }
    }
}

/// A [`earshot_llm_local::summarize::StepTime`] on the wire.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct StepLine {
    step: String,
    wall: Duration,
    requests: usize,
}

/// A step's name as the report holds it; a name this list lacks reads as `other`.
fn known_step(name: &str) -> &'static str {
    match name {
        "single" => "single",
        "map" => "map",
        "map-actions" => "map-actions",
        "map-rest" => "map-rest",
        "reduce" => "reduce",
        _ => "other",
    }
}

/// A [`SummaryReport`] on the wire: numbers and step names, no text.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ReportLine {
    requests: usize,
    repairs: usize,
    truncated: usize,
    looping: usize,
    invalid_replies: usize,
    duplicates_dropped: usize,
    joined_parts: usize,
    map_parts: usize,
    reused_parts: usize,
    reduce_steps: usize,
    planner_window: usize,
    replans: usize,
    max_prompt_tokens: usize,
    max_reply_budget: usize,
    prompt_tokens: usize,
    generated_tokens: usize,
    prompt_time: Duration,
    generate_time: Duration,
    steps: Vec<StepLine>,
}

impl ReportLine {
    fn of(r: &SummaryReport) -> Self {
        Self {
            requests: r.requests,
            repairs: r.repairs,
            truncated: r.truncated,
            looping: r.looping,
            invalid_replies: r.invalid_replies,
            duplicates_dropped: r.duplicates_dropped,
            joined_parts: r.joined_parts,
            map_parts: r.map_parts,
            reused_parts: r.reused_parts,
            reduce_steps: r.reduce_steps,
            planner_window: r.planner_window,
            replans: r.replans,
            max_prompt_tokens: r.max_prompt_tokens,
            max_reply_budget: r.max_reply_budget,
            prompt_tokens: r.prompt_tokens,
            generated_tokens: r.generated_tokens,
            prompt_time: r.prompt_time,
            generate_time: r.generate_time,
            steps: r
                .steps
                .iter()
                .map(|s| StepLine {
                    step: s.step.to_owned(),
                    wall: s.wall,
                    requests: s.requests,
                })
                .collect(),
        }
    }

    fn report(self) -> SummaryReport {
        SummaryReport {
            requests: self.requests,
            repairs: self.repairs,
            truncated: self.truncated,
            looping: self.looping,
            invalid_replies: self.invalid_replies,
            duplicates_dropped: self.duplicates_dropped,
            joined_parts: self.joined_parts,
            map_parts: self.map_parts,
            reused_parts: self.reused_parts,
            reduce_steps: self.reduce_steps,
            planner_window: self.planner_window,
            replans: self.replans,
            max_prompt_tokens: self.max_prompt_tokens,
            max_reply_budget: self.max_reply_budget,
            prompt_tokens: self.prompt_tokens,
            generated_tokens: self.generated_tokens,
            prompt_time: self.prompt_time,
            generate_time: self.generate_time,
            steps: self
                .steps
                .into_iter()
                .map(|s| earshot_llm_local::summarize::StepTime {
                    step: known_step(&s.step),
                    wall: s.wall,
                    requests: s.requests,
                })
                .collect(),
        }
    }
}

/// One line the child writes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "line", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum ChildLine {
    /// The model is loaded on `backend`, by its load's own evidence.
    Loaded {
        /// Where it runs.
        backend: BackendLine,
    },
    /// llama.cpp made progress.
    Beat,
    /// The summary (final line).
    Done {
        /// The validated summary.
        summary: Box<SummaryV1>,
        /// Where it ran.
        backend: BackendLine,
        /// The model's load time.
        load_time: Duration,
        /// What the run did.
        report: Box<ReportLine>,
    },
    /// The GPU load was refused (final line): the processor may still do it.
    LoadRefused {
        /// The load's error.
        message: String,
    },
    /// The run itself failed (final line): another processor would fail alike.
    Failed {
        /// The run's error.
        message: String,
    },
}

impl ChildLine {
    fn is_final(&self) -> bool {
        !matches!(self, Self::Loaded { .. } | Self::Beat)
    }
}

/// Writes one line and flushes; a parent that is gone ends the child at once.
fn emit(out: &Mutex<impl Write>, line: &ChildLine) {
    let ok = serde_json::to_string(line).ok().is_some_and(|text| {
        // One write: the newline first, because a native library may have left a line open.
        let whole = format!("\n{LINE_PREFIX}{text}\n");
        let mut out = out
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        out.write_all(whole.as_bytes())
            .and_then(|()| out.flush())
            .is_ok()
    });
    if !ok {
        std::process::exit(EXIT_NO_PARENT);
    }
}

/// The hidden mode, if `args` (the process arguments after the program name) ask for it: runs
/// the job on standard input and returns the exit code; `None` for a normal start.
#[must_use]
pub fn child_main(args: &[OsString]) -> Option<i32> {
    if args.first().is_none_or(|a| a != SUMMARY_CHILD_FLAG) {
        return None;
    }
    Some(serve(
        std::io::stdin().lock(),
        std::io::stdout(),
        &LlamaLoader,
        &|folder| earshot_llm_local::load_from_folder(folder).map_err(|e| e.to_string()),
    ))
}

/// The child's work (module docs), with its loader and DLL folder step passed in.
fn serve(
    mut input: impl Read,
    out: impl Write + Send,
    loader: &dyn LoadLocal,
    load_folder: &dyn Fn(&std::path::Path) -> Result<(), String>,
) -> i32 {
    // Nothing about a job that cannot be read is printed: its text may be the meeting's.
    let mut bytes = Vec::new();
    if input.read_to_end(&mut bytes).is_err() {
        return EXIT_BAD_JOB;
    }
    let Ok(job) = serde_json::from_slice::<ChildJob>(&bytes) else {
        return EXIT_BAD_JOB;
    };
    drop(bytes);
    let out = Mutex::new(out);
    if let Some(folder) = &job.llama_folder
        && let Err(why) = load_folder(folder)
    {
        let message = format!("llama.cpp's folder could not load: {why}");
        emit(&out, &ChildLine::LoadRefused { message });
        return 0;
    }
    let config = job.config();
    let loaded = match loader.load(&config) {
        Ok(loaded) => loaded,
        Err(e) => {
            let message = e.to_string();
            emit(&out, &ChildLine::LoadRefused { message });
            return 0;
        }
    };
    let LoadedModel {
        heartbeat,
        mut model,
        backend,
        load_time,
    } = loaded;
    let backend = BackendLine::of(backend);
    emit(&out, &ChildLine::Loaded { backend });
    let stop = AtomicBool::new(false);
    let run: SummaryRun = std::thread::scope(|scope| {
        scope.spawn(|| beat_while_running(&heartbeat, &stop, &out));
        // Nothing here cancels: the parent kills the child instead.
        let options = options_of(&config, &CancelToken::new());
        let run = match job.work {
            ChildWork::Whole { ranges, done } => {
                summarize_chunks_with(model.as_mut(), &job.input, &options, &ranges, done)
            }
            ChildWork::LastChunk { ranges } => {
                map_last_chunk(model.as_mut(), &job.input, &options, &ranges)
            }
        };
        stop.store(true, Ordering::SeqCst);
        run
    });
    // Freed before the final line, so a crash in the free is a dead child.
    drop(model);
    emit(
        &out,
        &match run.result {
            Ok(summary) => ChildLine::Done {
                summary: Box::new(summary),
                backend,
                load_time,
                report: Box::new(ReportLine::of(&run.report)),
            },
            Err(e) => ChildLine::Failed {
                message: e.to_string(),
            },
        },
    );
    0
}

/// Writes a `beat` whenever `heartbeat` moved, until `stop`; progress made before this thread
/// first ran counts, and it looks once more after `stop`.
fn beat_while_running(heartbeat: &Heartbeat, stop: &AtomicBool, out: &Mutex<impl Write>) {
    let mut seen = 0;
    loop {
        let stopping = stop.load(Ordering::SeqCst);
        let count = heartbeat.count();
        if count != seen {
            seen = count;
            emit(out, &ChildLine::Beat);
        }
        if stopping {
            return;
        }
        std::thread::sleep(POLL);
    }
}

/// What the parent reads from the child's output.
#[derive(Debug, PartialEq)]
enum Heard {
    /// A line of the protocol.
    Line(ChildLine),
    /// Something that cannot be a healthy child's output: fixed words.
    Bad(&'static str),
    /// The output ended.
    End,
}

/// Reads the child's output to its end (module docs). Ends when the child's end of the pipe
/// closes, which a kill does; never parks the caller, because it runs on a thread of its own.
fn read_lines(mut reader: impl BufRead, to: &Sender<Heard>) {
    let mut line = Vec::new();
    let mut ended = false;
    loop {
        line.clear();
        let read = reader
            .by_ref()
            .take(MAX_LINE_BYTES)
            .read_until(b'\n', &mut line);
        let heard = match read {
            Ok(0) => Heard::End,
            Err(_) => Heard::Bad("its output could not be read"),
            Ok(_) if line.last() != Some(&b'\n') && line.len() as u64 >= MAX_LINE_BYTES => {
                Heard::Bad("it wrote a line too long to read")
            }
            Ok(_) => {
                let Some(text) = line.strip_prefix(LINE_PREFIX.as_bytes()) else {
                    // A native library's output, or nothing: not read, not kept.
                    continue;
                };
                match serde_json::from_slice::<ChildLine>(text) {
                    Err(_) => Heard::Bad("it wrote a line Earshot cannot read"),
                    Ok(_) if ended => Heard::Bad("it wrote more after its last line"),
                    Ok(parsed) => {
                        ended = parsed.is_final();
                        Heard::Line(parsed)
                    }
                }
            }
        };
        let last = matches!(heard, Heard::End | Heard::Bad(_));
        if to.send(heard).is_err() || last {
            return;
        }
    }
}

/// A summary a child finished.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ChildSummary {
    /// The validated summary.
    pub(crate) summary: SummaryV1,
    /// Where it ran.
    pub(crate) backend: LlamaBackend,
    /// The load time.
    pub(crate) load_time: Duration,
    /// What the run did.
    pub(crate) report: SummaryReport,
}

/// How a child's run ended.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum ChildEnd {
    /// The summary.
    Finished(Box<ChildSummary>),
    /// The GPU load was refused.
    LoadRefused(String),
    /// The run failed.
    RunFailed(String),
    /// The child did not finish: fixed words and at most an exit code.
    Died(String),
    /// No progress for the limit: the child was killed.
    Stalled {
        /// Its exit was confirmed.
        gone: bool,
    },
    /// The token was cancelled: the child was killed.
    Cancelled,
}

/// What the parent reads about a run; the input of [`judge_end`].
struct Watched {
    last: Option<ChildLine>,
    status: Option<std::process::ExitStatus>,
}

/// The end of a child whose output ended: only a final line and exit 0 count.
fn judge_end(watched: Watched) -> ChildEnd {
    let Watched { last, status } = watched;
    match status {
        None => return ChildEnd::Died("it did not exit".to_owned()),
        Some(s) if !s.success() => {
            return ChildEnd::Died(match s.code() {
                Some(code) => format!("it ended with exit code {:#x}", code.cast_unsigned()),
                None => "it was stopped".to_owned(),
            });
        }
        Some(_) => {}
    }
    match last {
        Some(ChildLine::Done {
            summary,
            backend,
            load_time,
            report,
        }) => ChildEnd::Finished(Box::new(ChildSummary {
            summary: *summary,
            backend: backend.backend(),
            load_time,
            report: report.report(),
        })),
        Some(ChildLine::LoadRefused { message }) => ChildEnd::LoadRefused(message),
        Some(ChildLine::Failed { message }) => ChildEnd::RunFailed(message),
        Some(ChildLine::Loaded { .. } | ChildLine::Beat) | None => {
            ChildEnd::Died("it stopped before it finished".to_owned())
        }
    }
}

/// How the parent treats a run.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Watch {
    /// The limit until `loaded`.
    pub(crate) load_limit: Duration,
    /// The limit since the last progress.
    pub(crate) run_limit: Duration,
    /// Runs the whole child below normal priority (a live window during a meeting).
    pub(crate) background: bool,
}

/// The parent's end of one run: `job` to the child on its stdin, its lines watched. Blocks until
/// the child ended; never panics on anything the child does. Off Windows there is no job object
/// and no child runs: a dead child, so the summary moves to the processor.
pub(crate) fn run_job(
    child: &GpuChild,
    job: &ChildJob,
    watch: Watch,
    cancel: &CancelToken,
) -> ChildEnd {
    if cancel.is_cancelled() {
        return ChildEnd::Cancelled;
    }
    let Ok(bytes) = serde_json::to_vec(job) else {
        return ChildEnd::Died("its job could not be written".to_owned());
    };
    #[cfg(windows)]
    {
        run_windows(child, bytes, watch, cancel)
    }
    #[cfg(not(windows))]
    {
        let _ = (child, bytes, watch);
        ChildEnd::Died("it runs on Windows only".to_owned())
    }
}

#[cfg(windows)]
fn run_windows(child: &GpuChild, bytes: Vec<u8>, watch: Watch, cancel: &CancelToken) -> ChildEnd {
    use std::os::windows::process::CommandExt;
    use std::process::Stdio;

    /// `CREATE_NO_WINDOW`: the child gets no console window.
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    /// `BELOW_NORMAL_PRIORITY_CLASS`.
    const BELOW_NORMAL_PRIORITY_CLASS: u32 = 0x0000_4000;

    let Ok(job) = earshot_proc::KillOnClose::new() else {
        return ChildEnd::Died("it could not be set up".to_owned());
    };
    let flags = if watch.background {
        CREATE_NO_WINDOW | BELOW_NORMAL_PRIORITY_CLASS
    } else {
        CREATE_NO_WINDOW
    };
    let mut process = match std::process::Command::new(&child.program)
        .args(&child.args)
        .envs(child.env.iter().map(|(k, v)| (k, v)))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .creation_flags(flags)
        .spawn()
    {
        Ok(process) => process,
        Err(_) => return ChildEnd::Died("it could not start".to_owned()),
    };
    if job.assign(&process).is_err() {
        let _ = process.kill();
        let _ = process.wait();
        return ChildEnd::Died("it could not be set up".to_owned());
    }
    let (Some(mut stdin), Some(stdout)) = (process.stdin.take(), process.stdout.take()) else {
        let _ = process.kill();
        let _ = process.wait();
        return ChildEnd::Died("it has no pipes".to_owned());
    };
    // A child that dies without reading breaks the pipe: its exit says so.
    std::thread::spawn(move || {
        let _ = stdin.write_all(&bytes);
    });
    let (to, heard) = std::sync::mpsc::channel();
    std::thread::spawn(move || read_lines(std::io::BufReader::new(stdout), &to));

    let kill = |process: &mut std::process::Child| -> bool {
        job.terminate();
        let _ = process.kill();
        exited_within(process, KILL_WAIT).is_some()
    };
    match watch_lines(&heard, watch, cancel) {
        Heading::Exit(last) => {
            let status = exited_within(&mut process, KILL_WAIT);
            if status.is_none() {
                kill(&mut process);
            }
            judge_end(Watched { last, status })
        }
        Heading::Cancel => {
            kill(&mut process);
            ChildEnd::Cancelled
        }
        Heading::Stall => ChildEnd::Stalled {
            gone: kill(&mut process),
        },
        Heading::Bad(why) => {
            kill(&mut process);
            ChildEnd::Died(why.to_owned())
        }
    }
}

/// How [`watch_lines`] ended.
#[derive(Debug, PartialEq)]
enum Heading {
    /// The output ended, after this last line.
    Exit(Option<ChildLine>),
    /// The token was cancelled.
    Cancel,
    /// No progress for the limit.
    Stall,
    /// The output is not a healthy child's.
    Bad(&'static str),
}

/// Follows the child's lines until its output ends or it must be killed.
fn watch_lines(heard: &Receiver<Heard>, watch: Watch, cancel: &CancelToken) -> Heading {
    let mut limit = watch.load_limit;
    let mut since = Instant::now();
    let mut last = None;
    loop {
        if cancel.is_cancelled() {
            return Heading::Cancel;
        }
        match heard.recv_timeout(POLL) {
            Ok(Heard::Line(line)) => {
                since = Instant::now();
                if line.is_final() {
                    // Only its exit is left.
                    limit = KILL_WAIT;
                } else {
                    limit = watch.run_limit;
                }
                last = Some(line);
            }
            Ok(Heard::Bad(why)) => return Heading::Bad(why),
            Ok(Heard::End) | Err(RecvTimeoutError::Disconnected) => return Heading::Exit(last),
            Err(RecvTimeoutError::Timeout) => {}
        }
        if since.elapsed() >= limit {
            return match last {
                Some(l) if l.is_final() => Heading::Bad("it did not exit after its last line"),
                _ => Heading::Stall,
            };
        }
    }
}

/// The child's exit within `wait`, `None` when it is still there.
fn exited_within(
    process: &mut std::process::Child,
    wait: Duration,
) -> Option<std::process::ExitStatus> {
    let started = Instant::now();
    loop {
        match process.try_wait() {
            Ok(Some(status)) => return Some(status),
            Ok(None) if started.elapsed() < wait => std::thread::sleep(POLL),
            Ok(None) | Err(_) => return None,
        }
    }
}
