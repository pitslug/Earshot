//! One child process run with a deadline, cancellation and complete-output rules (T7.4ga):
//! the summary provider that runs the user's Claude Code CLI uses it, and anything else that
//! must never leave a child running can. `KillOnClose` is the workspace's one job object
//! implementation: the desktop app's model-check child, whose lines stream while it runs,
//! uses it directly.
//!
//! [`run`]:
//! - starts the program with every stream piped, no console window on Windows
//!   (`CREATE_NO_WINDOW`), in the given folder and with the given environment changes;
//! - on Windows starts it inside a kill-on-close job object ([`KillOnClose::spawn`]:
//!   suspended, assigned, then resumed), so the child and everything it starts are killed
//!   when the run ends, on a timeout or a cancel, and if this process dies;
//! - with [`run_with`], asks the caller once the child is in the job and before any input is
//!   written (`on_started` with the child's pid); a `false` answer stops the child unread
//!   ([`ChildError::Aborted`]);
//! - writes the input on its own thread and closes it, and reads both outputs to the end on
//!   their own threads, keeping at most [`ChildSpec::max_output`] bytes of each;
//! - polls the child, the deadline and `cancelled` every [`POLL`];
//! - after the child exits waits at most [`DRAIN_TIMEOUT`] for both outputs to end, then
//!   kills the job (a grandchild holding a pipe open) and reports what was read. An output
//!   that did not end, or went past the cap, is marked incomplete: callers never parse an
//!   incomplete output ([`ChildOutput::require_complete`]).
//!
//! Off Windows there is no job object: only the child itself is killed.

#![deny(unsafe_code)]

#[cfg(windows)]
#[allow(unsafe_code)]
mod job_win;

use std::ffi::OsString;
use std::fmt;
use std::io::{self, Read, Write};
use std::path::PathBuf;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

#[cfg(windows)]
pub use job_win::{KillOnClose, SpawnError, peak_memory};

/// A process's peak memory so far ([`peak_memory`]; `cargo xtask bench`, T8.1), in bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PeakMemory {
    /// Peak working set: the most physical memory it held.
    pub working_set: u64,
    /// Peak private committed memory.
    pub committed: u64,
}

/// Not measured off Windows.
#[cfg(not(windows))]
#[must_use]
pub fn peak_memory() -> Option<PeakMemory> {
    None
}

/// How often the run checks the child, the deadline and the cancel flag.
pub const POLL: Duration = Duration::from_millis(10);

/// How long the outputs may stay open after the child exited.
pub const DRAIN_TIMEOUT: Duration = Duration::from_secs(5);

/// `CREATE_NO_WINDOW` (winbase.h): the child gets no console window.
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// What to run.
#[derive(Debug, Clone)]
pub struct ChildSpec {
    /// The executable.
    pub program: PathBuf,
    /// Its arguments, in order.
    pub args: Vec<OsString>,
    /// Its working directory.
    pub current_dir: PathBuf,
    /// Variables removed from the inherited environment.
    pub env_remove: Vec<OsString>,
    /// Variables set (after the removals).
    pub env_set: Vec<(OsString, OsString)>,
    /// Written to its standard input, which is then closed.
    pub stdin: Vec<u8>,
    /// The whole run (start to exit) must end within this.
    pub deadline: Duration,
    /// At most this many bytes of each output are kept; more makes it incomplete.
    pub max_output: usize,
}

/// What a run that exited produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChildOutput {
    /// How it exited.
    pub status: ExitStatus,
    /// Standard output (at most [`ChildSpec::max_output`] bytes).
    pub stdout: Vec<u8>,
    /// Standard error (at most [`ChildSpec::max_output`] bytes).
    pub stderr: Vec<u8>,
    /// Standard output was read to its end, within the cap.
    pub stdout_complete: bool,
    /// Standard error was read to its end, within the cap.
    pub stderr_complete: bool,
}

impl ChildOutput {
    /// Whether both outputs were read to the end.
    #[must_use]
    pub fn complete(&self) -> bool {
        self.stdout_complete && self.stderr_complete
    }

    /// `self` when both outputs are complete.
    ///
    /// # Errors
    /// [`ChildError::OutputIncomplete`].
    pub fn require_complete(self) -> Result<Self, ChildError> {
        if self.complete() {
            Ok(self)
        } else {
            Err(ChildError::OutputIncomplete {
                stdout: self.stdout_complete,
                stderr: self.stderr_complete,
            })
        }
    }
}

/// Why a run produced no [`ChildOutput`] (or one that may not be read).
#[derive(Debug)]
pub enum ChildError {
    /// The program could not be started (missing, not executable).
    Start(io::Error),
    /// The job object could not be created or the child put into it (the child was
    /// killed).
    Job(io::Error),
    /// The deadline passed; the child (and its job) was killed.
    TimedOut(Duration),
    /// `cancelled` returned true; the child (and its job) was killed.
    Cancelled,
    /// `on_started` refused the run; the child was killed before any input was written.
    Aborted,
    /// Waiting for the child failed (it was killed).
    Wait(io::Error),
    /// An output did not end within [`DRAIN_TIMEOUT`] of the exit or went past the cap
    /// (which streams were complete).
    OutputIncomplete {
        /// Standard output was complete.
        stdout: bool,
        /// Standard error was complete.
        stderr: bool,
    },
}

impl fmt::Display for ChildError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Start(e) => write!(f, "the program could not be started: {e}"),
            Self::Job(e) => write!(f, "the program could not be put into a job object: {e}"),
            Self::TimedOut(d) => write!(f, "the program did not finish within {} s", d.as_secs()),
            Self::Cancelled => f.write_str("cancelled"),
            Self::Aborted => f.write_str("stopped before its input was written"),
            Self::Wait(e) => write!(f, "waiting for the program failed: {e}"),
            Self::OutputIncomplete { stdout, stderr } => write!(
                f,
                "the program's output was cut off (standard output {}, standard error {})",
                if *stdout { "complete" } else { "incomplete" },
                if *stderr { "complete" } else { "incomplete" },
            ),
        }
    }
}

impl std::error::Error for ChildError {}

/// The job the child runs in (Windows), nothing elsewhere.
struct Job {
    #[cfg(windows)]
    job: KillOnClose,
}

impl Job {
    fn new() -> io::Result<Self> {
        Ok(Self {
            #[cfg(windows)]
            job: KillOnClose::new()?,
        })
    }

    /// Starts `cmd` inside the job (Windows: no console window), or plainly elsewhere.
    fn spawn(&self, cmd: &mut Command) -> Result<Child, ChildError> {
        #[cfg(windows)]
        {
            self.job.spawn(cmd, CREATE_NO_WINDOW).map_err(|e| match e {
                SpawnError::Start(e) => ChildError::Start(e),
                SpawnError::Job(e) => ChildError::Job(e),
            })
        }
        #[cfg(not(windows))]
        {
            cmd.spawn().map_err(ChildError::Start)
        }
    }

    fn terminate(&self) {
        #[cfg(windows)]
        self.job.terminate();
    }
}

/// Kills the job and the child and reaps it.
fn kill(job: &Job, child: &mut Child) {
    job.terminate();
    let _ = child.kill();
    let _ = child.wait();
}

/// Which output a reader thread read.
enum Stream {
    Out,
    Err,
}

/// Reads `r` to its end, keeping at most `cap` bytes (the rest is drained and dropped);
/// `true` when it ended within the cap.
fn read_capped(mut r: impl Read, cap: usize) -> (Vec<u8>, bool) {
    let mut kept = Vec::new();
    let mut buf = [0u8; 16 * 1024];
    let mut over = false;
    loop {
        match r.read(&mut buf) {
            Ok(0) => return (kept, !over),
            Ok(n) => {
                let room = cap.saturating_sub(kept.len());
                if n > room {
                    over = true;
                }
                kept.extend_from_slice(&buf[..n.min(room)]);
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(_) => return (kept, false),
        }
    }
}

/// Starts `program` and leaves it running with nothing attached: no job object, so it outlives
/// this process, which is the point (an update's installer replaces this exe after it exits).
/// Everything else here keeps a child on a short lead; use this only for what must survive.
///
/// # Errors
/// The program could not start.
pub fn spawn_detached(program: &std::path::Path, args: &[&str]) -> io::Result<()> {
    Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map(drop)
}

/// Runs `spec` (module docs). `cancelled` is polled every [`POLL`].
///
/// # Errors
/// [`ChildError`]: start, job, deadline, cancel, wait. An exit with incomplete output is
/// returned as a [`ChildOutput`] with the flags cleared (see
/// [`ChildOutput::require_complete`]).
pub fn run(spec: &ChildSpec, cancelled: &dyn Fn() -> bool) -> Result<ChildOutput, ChildError> {
    run_with(spec, cancelled, &mut |_| true)
}

/// [`run`], asking `on_started` (with the child's pid) once the child is in its job and
/// before any input is written: `false` kills it unread.
///
/// # Errors
/// As [`run`]; [`ChildError::Aborted`] when `on_started` refused.
pub fn run_with(
    spec: &ChildSpec,
    cancelled: &dyn Fn() -> bool,
    on_started: &mut dyn FnMut(u32) -> bool,
) -> Result<ChildOutput, ChildError> {
    if cancelled() {
        return Err(ChildError::Cancelled);
    }
    let started = Instant::now();
    // The job exists before the child, so a job failure never leaves a child behind.
    let job = Job::new().map_err(ChildError::Job)?;
    let mut cmd = Command::new(&spec.program);
    cmd.args(&spec.args)
        .current_dir(&spec.current_dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for name in &spec.env_remove {
        cmd.env_remove(name);
    }
    for (name, value) in &spec.env_set {
        cmd.env(name, value);
    }
    let mut child = job.spawn(&mut cmd)?;
    let (tx, rx) = mpsc::channel::<(Stream, Vec<u8>, bool)>();
    if let Some(out) = child.stdout.take() {
        let tx = tx.clone();
        let cap = spec.max_output;
        std::thread::spawn(move || {
            let (bytes, complete) = read_capped(out, cap);
            let _ = tx.send((Stream::Out, bytes, complete));
        });
    }
    if let Some(err) = child.stderr.take() {
        let tx = tx.clone();
        let cap = spec.max_output;
        std::thread::spawn(move || {
            let (bytes, complete) = read_capped(err, cap);
            let _ = tx.send((Stream::Err, bytes, complete));
        });
    }
    drop(tx);
    if !on_started(child.id()) {
        kill(&job, &mut child);
        return Err(ChildError::Aborted);
    }
    if let Some(mut input) = child.stdin.take() {
        let bytes = spec.stdin.clone();
        // A child that exits without reading breaks the pipe: not an error of the run.
        std::thread::spawn(move || {
            let _ = input.write_all(&bytes);
        });
    }
    let status = loop {
        if cancelled() {
            kill(&job, &mut child);
            return Err(ChildError::Cancelled);
        }
        if started.elapsed() >= spec.deadline {
            kill(&job, &mut child);
            return Err(ChildError::TimedOut(spec.deadline));
        }
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => std::thread::sleep(POLL),
            Err(e) => {
                kill(&job, &mut child);
                return Err(ChildError::Wait(e));
            }
        }
    };
    let mut out = ChildOutput {
        status,
        stdout: Vec::new(),
        stderr: Vec::new(),
        stdout_complete: false,
        stderr_complete: false,
    };
    let collect = |until: Instant, out: &mut ChildOutput| {
        while let Some(left) = until.checked_duration_since(Instant::now()) {
            match rx.recv_timeout(left) {
                Ok((Stream::Out, bytes, complete)) => {
                    out.stdout = bytes;
                    out.stdout_complete = complete;
                }
                Ok((Stream::Err, bytes, complete)) => {
                    out.stderr = bytes;
                    out.stderr_complete = complete;
                }
                // Both readers reported (their senders are gone).
                Err(mpsc::RecvTimeoutError::Disconnected) => return true,
                Err(mpsc::RecvTimeoutError::Timeout) => return false,
            }
        }
        false
    };
    if !collect(Instant::now() + DRAIN_TIMEOUT, &mut out) {
        // A process left in the job holds an output open: end it. What arrives after that
        // was cut off by the kill, so it is never complete.
        job.terminate();
        let mut late = ChildOutput {
            stdout: Vec::new(),
            stderr: Vec::new(),
            ..out.clone()
        };
        late.stdout_complete = false;
        late.stderr_complete = false;
        collect(Instant::now() + Duration::from_secs(1), &mut late);
        if !late.stdout.is_empty() {
            out.stdout = late.stdout;
            out.stdout_complete = false;
        }
        if !late.stderr.is_empty() {
            out.stderr = late.stderr;
            out.stderr_complete = false;
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests;
