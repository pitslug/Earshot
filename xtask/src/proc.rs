//! Running external commands with a hard deadline, so probes never hang.
//!
//! Output capture is honest: a stream that was not read to its end is reported as
//! incomplete ([`CmdOutput::stdout_complete`], [`CmdOutput::stderr_complete`]) instead
//! of being replaced by an empty string. Anything that decides pass/fail from captured
//! output must check [`CmdOutput::require_complete`] first, because an empty or truncated
//! `cargo tree` would otherwise read as "nothing found".

use std::io::Read;
use std::process::{Command, ExitStatus, Stdio};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Result, bail};

/// Shortest window the output readers get after the child exits, even when the command
/// used up (nearly) all of its deadline. A loaded host can starve the reader threads for
/// seconds (the old fixed 2 s window lost whole `cargo` outputs in `cargo xtask ci`).
pub const MIN_DRAIN: Duration = Duration::from_secs(10);

/// Longest window the output readers get after the child exits. Only a grandchild that
/// inherited a pipe and outlives the child (for example an MSBuild node with node reuse
/// under a build) can hold a stream open this long; its output is then incomplete.
pub const MAX_DRAIN: Duration = Duration::from_secs(120);

/// Captured result of a command that exited before its deadline.
#[derive(Debug, Clone)]
pub struct CmdOutput {
    /// Whether the process exited with status 0.
    pub success: bool,
    /// Exit code, when the platform reported one.
    pub code: Option<i32>,
    /// Standard output, lossily decoded as UTF-8 (possibly partial, see `stdout_complete`).
    pub stdout: String,
    /// Standard error, lossily decoded as UTF-8 (possibly partial, see `stderr_complete`).
    pub stderr: String,
    /// Whether stdout was read to its end.
    pub stdout_complete: bool,
    /// Whether stderr was read to its end.
    pub stderr_complete: bool,
}

impl CmdOutput {
    /// Whether both streams were read to their end.
    pub fn complete(&self) -> bool {
        self.stdout_complete && self.stderr_complete
    }

    /// The exit status in words, for error messages.
    pub fn status_text(&self) -> String {
        match self.code {
            Some(c) => format!("exit code {c}"),
            None => "no exit code".into(),
        }
    }

    /// Exit status, stream completeness and both captured streams, for error messages.
    pub fn describe(&self) -> String {
        let mark = |complete: bool| if complete { "" } else { " (INCOMPLETE)" };
        format!(
            "{}\n--- stdout{} ---\n{}\n--- stderr{} ---\n{}",
            self.status_text(),
            mark(self.stdout_complete),
            self.stdout,
            mark(self.stderr_complete),
            self.stderr
        )
    }

    /// `Ok` only when both streams were read to their end; otherwise an error naming
    /// `what` with [`CmdOutput::describe`].
    pub fn require_complete(&self, what: &str) -> Result<&Self> {
        if self.complete() {
            return Ok(self);
        }
        bail!(
            "{what}: the command's output was not fully captured within the drain window \
             (a pipe was still open after the process exited)\n{}",
            self.describe()
        )
    }
}

/// Outcome of waiting on something with a deadline.
#[derive(Debug, PartialEq, Eq)]
pub enum Wait<S> {
    /// `poll` returned a value before the deadline.
    Done(S),
    /// The deadline passed first.
    TimedOut,
    /// `poll` failed.
    Failed,
}

/// Polls until `poll` yields a value or `elapsed()` reaches `timeout`, calling `sleep`
/// between polls. The clock and sleep are injected so the deadline logic is testable.
pub fn wait_with_deadline<S>(
    mut poll: impl FnMut() -> std::io::Result<Option<S>>,
    mut elapsed: impl FnMut() -> Duration,
    mut sleep: impl FnMut(Duration),
    timeout: Duration,
) -> Wait<S> {
    const STEP: Duration = Duration::from_millis(10);
    loop {
        match poll() {
            Ok(Some(s)) => return Wait::Done(s),
            Ok(None) => {}
            Err(_) => return Wait::Failed,
        }
        let now = elapsed();
        if now >= timeout {
            return Wait::TimedOut;
        }
        sleep(STEP.min(timeout - now));
    }
}

/// How long the output readers may take after the child exited `elapsed` into a command
/// with deadline `timeout`: the rest of the deadline, but at least [`MIN_DRAIN`] and at
/// most [`MAX_DRAIN`].
pub fn drain_window(timeout: Duration, elapsed: Duration) -> Duration {
    timeout.saturating_sub(elapsed).clamp(MIN_DRAIN, MAX_DRAIN)
}

/// What a reader thread sends: data as it arrives, then `End` once the stream hit EOF.
/// A read error sends no `End`, so the stream counts as incomplete.
#[derive(Debug, PartialEq, Eq)]
pub enum Chunk {
    /// Bytes read from the stream.
    Data(Vec<u8>),
    /// The stream reached its end.
    End,
}

/// Which output stream a [`Chunk`] came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stream {
    /// Standard output.
    Stdout,
    /// Standard error.
    Stderr,
}

/// Reads `r` to its end, sending each read as [`Chunk::Data`] and a final
/// [`Chunk::End`], tagged with `stream`; stops without `End` on a read error.
pub fn pump(mut r: impl Read, stream: Stream, tx: &mpsc::Sender<(Stream, Chunk)>) {
    let mut buf = [0u8; 8192];
    loop {
        match r.read(&mut buf) {
            Ok(0) => {
                let _ = tx.send((stream, Chunk::End));
                return;
            }
            Ok(n) => {
                if tx.send((stream, Chunk::Data(buf[..n].to_vec()))).is_err() {
                    return;
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(_) => return,
        }
    }
}

/// Both streams as collected by [`drain`].
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Drained {
    /// Bytes received from stdout.
    pub stdout: Vec<u8>,
    /// Bytes received from stderr.
    pub stderr: Vec<u8>,
    /// Whether stdout's [`Chunk::End`] arrived.
    pub stdout_complete: bool,
    /// Whether stderr's [`Chunk::End`] arrived.
    pub stderr_complete: bool,
}

/// Collects both streams' chunks from the one channel `rx` concurrently, in arrival
/// order, until both have sent [`Chunk::End`], every sender is gone, or `remaining()`
/// reports no time left. A stream without its `End` is incomplete. Neither stream can
/// use up the shared deadline at the other's expense: whatever either reader queued is
/// taken before the clock is consulted. The clock is injected so the rule is testable
/// without waiting.
pub fn drain(
    rx: &mpsc::Receiver<(Stream, Chunk)>,
    mut remaining: impl FnMut() -> Duration,
) -> Drained {
    let mut d = Drained::default();
    while !(d.stdout_complete && d.stderr_complete) {
        let next = match rx.try_recv() {
            Ok(c) => Ok(c),
            Err(mpsc::TryRecvError::Disconnected) => Err(RecvTimeoutError::Disconnected),
            Err(mpsc::TryRecvError::Empty) => {
                let left = remaining();
                if left.is_zero() {
                    Err(RecvTimeoutError::Timeout)
                } else {
                    rx.recv_timeout(left)
                }
            }
        };
        let Ok((stream, chunk)) = next else {
            break;
        };
        let (buf, complete) = match stream {
            Stream::Stdout => (&mut d.stdout, &mut d.stdout_complete),
            Stream::Stderr => (&mut d.stderr, &mut d.stderr_complete),
        };
        match chunk {
            Chunk::Data(bytes) => buf.extend_from_slice(&bytes),
            Chunk::End => *complete = true,
        }
    }
    d
}

/// Runs `cmd` with stdin closed and stdout/stderr captured. Returns `None` if the command
/// cannot be started, fails while waiting, or does not exit within `timeout` (it is
/// killed in that case). After the exit, each stream is read until its end for at most
/// [`drain_window`]; a stream still open after that (a grandchild holding the pipe) is
/// returned partial and flagged incomplete, never silently empty.
pub fn run_with_timeout(cmd: &mut Command, timeout: Duration) -> Option<CmdOutput> {
    run_with_drain(cmd, timeout, |elapsed| drain_window(timeout, elapsed), None)
}

/// The kill-on-close job a command runs in ([`run_in_job_with_timeout`]): earshot-proc's,
/// the workspace's one job object (Windows only).
#[cfg(windows)]
type Job = earshot_proc::KillOnClose;
/// No job object off Windows.
#[cfg(not(windows))]
type Job = std::convert::Infallible;

/// As [`run_with_timeout`], but `cmd` and everything it starts run in a kill-on-close job
/// (Windows: started suspended and resumed once inside, `earshot_proc::KillOnClose::spawn`),
/// so a timeout ends the whole tree, not only the direct child, and nothing outlives the
/// return. Off Windows it is [`run_with_timeout`]. `None` also when the job cannot be made.
pub fn run_in_job_with_timeout(cmd: &mut Command, timeout: Duration) -> Option<CmdOutput> {
    #[cfg(windows)]
    {
        let job = earshot_proc::KillOnClose::new().ok()?;
        run_with_drain(
            cmd,
            timeout,
            |elapsed| drain_window(timeout, elapsed),
            Some(&job),
        )
    }
    #[cfg(not(windows))]
    {
        run_with_timeout(cmd, timeout)
    }
}

/// As [`run_with_timeout`], with the drain window chosen by `window(elapsed at exit)`, in
/// `job` when one is given.
fn run_with_drain(
    cmd: &mut Command,
    timeout: Duration,
    window: impl FnOnce(Duration) -> Duration,
    job: Option<&Job>,
) -> Option<CmdOutput> {
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = match job {
        #[cfg(windows)]
        Some(job) => job.spawn(cmd, 0).ok()?,
        #[cfg(not(windows))]
        Some(never) => match *never {},
        None => cmd.spawn().ok()?,
    };
    let stdout = child.stdout.take()?;
    let stderr = child.stderr.take()?;
    // One channel for both streams, so the drain below serves them concurrently.
    let (tx, rx) = mpsc::channel();
    let err_tx = tx.clone();
    thread::spawn(move || pump(stdout, Stream::Stdout, &tx));
    thread::spawn(move || pump(stderr, Stream::Stderr, &err_tx));

    let start = Instant::now();
    let status: ExitStatus = match wait_with_deadline(
        || child.try_wait(),
        || start.elapsed(),
        thread::sleep,
        timeout,
    ) {
        Wait::Done(s) => s,
        Wait::TimedOut | Wait::Failed => {
            // Without a job, kill() terminates only the direct child, so a grandchild
            // such as node under `cmd /c pnpm` may outlive it. Acceptable for short
            // version probes; the reader threads are never joined, so the caller does
            // not hang on them.
            #[cfg(windows)]
            if let Some(job) = job {
                job.terminate();
            }
            let _ = child.kill();
            let _ = child.wait();
            return None;
        }
    };

    // Both streams are drained together against one deadline.
    let drain_end = Instant::now() + window(start.elapsed());
    let d = drain(&rx, || drain_end.saturating_duration_since(Instant::now()));
    Some(CmdOutput {
        success: status.success(),
        code: status.code(),
        stdout: String::from_utf8_lossy(&d.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&d.stderr).into_owned(),
        stdout_complete: d.stdout_complete,
        stderr_complete: d.stderr_complete,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    #[test]
    fn deadline_returns_value_when_ready() {
        let polls = Cell::new(0);
        let t = Cell::new(Duration::ZERO);
        let r = wait_with_deadline(
            || {
                polls.set(polls.get() + 1);
                Ok((polls.get() == 3).then_some(42))
            },
            || t.get(),
            |d| t.set(t.get() + d),
            Duration::from_secs(1),
        );
        assert_eq!(r, Wait::Done(42));
        assert_eq!(polls.get(), 3);
    }

    #[test]
    fn deadline_times_out_with_fake_clock() {
        let t = Cell::new(Duration::ZERO);
        let r: Wait<()> = wait_with_deadline(
            || Ok(None),
            || t.get(),
            |d| t.set(t.get() + d),
            Duration::from_secs(2),
        );
        assert_eq!(r, Wait::TimedOut);
        assert!(t.get() >= Duration::from_secs(2));
        assert!(t.get() < Duration::from_secs(3), "overshot: {:?}", t.get());
    }

    #[test]
    fn deadline_reports_poll_failure() {
        let r: Wait<()> = wait_with_deadline(
            || Err(std::io::Error::other("boom")),
            || Duration::ZERO,
            |_| {},
            Duration::from_secs(1),
        );
        assert_eq!(r, Wait::Failed);
    }

    #[test]
    fn drain_window_is_the_rest_of_the_deadline_within_bounds() {
        let s = Duration::from_secs;
        // Plenty of deadline left: capped at MAX_DRAIN.
        assert_eq!(drain_window(s(30 * 60), s(5)), MAX_DRAIN);
        // Part of the deadline left: exactly that.
        assert_eq!(drain_window(s(20), s(5)), s(15));
        // Deadline (nearly) used up, or overrun by the poll step: still MIN_DRAIN.
        assert_eq!(drain_window(s(20), s(19)), MIN_DRAIN);
        assert_eq!(drain_window(s(20), s(25)), MIN_DRAIN);
        // Far longer than the old fixed 2 s grace that lost output under load.
        assert!(MIN_DRAIN >= s(10));
        assert!(MIN_DRAIN <= MAX_DRAIN);
    }

    use Stream::{Stderr, Stdout};

    fn data(s: Stream, b: &[u8]) -> (Stream, Chunk) {
        (s, Chunk::Data(b.to_vec()))
    }

    #[test]
    fn drain_collects_every_chunk_up_to_the_end() {
        let (tx, rx) = mpsc::channel();
        tx.send(data(Stdout, b"ab")).unwrap();
        tx.send(data(Stdout, b"c")).unwrap();
        tx.send((Stdout, Chunk::End)).unwrap();
        tx.send((Stderr, Chunk::End)).unwrap();
        // No time left at all: queued output still counts, and End makes it complete.
        let d = drain(&rx, || Duration::ZERO);
        assert_eq!(d.stdout, b"abc");
        assert!(d.stdout_complete && d.stderr_complete);
    }

    #[test]
    fn drain_without_end_by_the_deadline_is_incomplete_and_keeps_partial_output() {
        // A slow reader: some output arrived, the end did not, and time is up.
        let (tx, rx) = mpsc::channel();
        tx.send(data(Stdout, b"earshot-core v0.1.0\n")).unwrap();
        tx.send((Stderr, Chunk::End)).unwrap();
        let d = drain(&rx, || Duration::ZERO);
        assert!(!d.stdout_complete);
        assert!(d.stderr_complete);
        assert_eq!(d.stdout, b"earshot-core v0.1.0\n");
        drop(tx);
    }

    #[test]
    fn drain_of_a_reader_that_never_sent_anything_is_incomplete_not_empty_success() {
        let (_tx, rx) = mpsc::channel::<(Stream, Chunk)>();
        assert_eq!(drain(&rx, || Duration::ZERO), Drained::default());
    }

    #[test]
    fn drain_of_a_vanished_reader_without_end_is_incomplete() {
        let (tx, rx) = mpsc::channel();
        tx.send(data(Stdout, b"x")).unwrap();
        tx.send((Stderr, Chunk::End)).unwrap();
        drop(tx);
        let d = drain(&rx, || Duration::from_secs(60));
        assert_eq!(d.stdout, b"x");
        assert!(!d.stdout_complete && d.stderr_complete);
    }

    /// A large stdout arriving over most of the shared deadline does not starve stderr:
    /// stderr's chunks and End, interleaved with stdout's, are collected as they come, and
    /// the run is complete as soon as both ends arrived, before the deadline.
    #[test]
    fn drain_serves_both_streams_concurrently_against_one_deadline() {
        let (tx, rx) = mpsc::channel();
        tx.send(data(Stderr, b"warning: a\n")).unwrap();
        for _ in 0..100 {
            tx.send(data(Stdout, &[b'x'; 1000])).unwrap();
        }
        tx.send((Stderr, Chunk::End)).unwrap();
        for _ in 0..100 {
            tx.send(data(Stdout, &[b'y'; 1000])).unwrap();
        }
        tx.send((Stdout, Chunk::End)).unwrap();
        // Fake clock: each look at the clock uses up a tenth of a 1 s window; the queue
        // never needs it, so the clock is never read.
        let t = Cell::new(Duration::from_secs(1));
        let reads = Cell::new(0);
        let d = drain(&rx, || {
            reads.set(reads.get() + 1);
            t.set(t.get().saturating_sub(Duration::from_millis(100)));
            t.get()
        });
        assert_eq!(d.stdout.len(), 200_000);
        assert_eq!(d.stderr, b"warning: a\n");
        assert!(d.stdout_complete && d.stderr_complete);
        assert_eq!(
            reads.get(),
            0,
            "queued chunks are taken before the clock is read"
        );
        // Done after both ends: a chunk sent later is not waited for.
        tx.send(data(Stdout, b"late")).unwrap();
        assert!(rx.try_recv().is_ok());
    }

    /// stderr's End arrives while stdout is still streaming and the deadline then passes:
    /// stderr stays complete, only stdout is flagged.
    #[test]
    fn drain_deadline_flags_only_the_stream_that_did_not_end() {
        let (tx, rx) = mpsc::channel();
        tx.send(data(Stdout, b"part")).unwrap();
        tx.send((Stderr, Chunk::End)).unwrap();
        let d = drain(&rx, || Duration::ZERO);
        assert!(d.stderr_complete);
        assert!(!d.stdout_complete);
        assert_eq!(d.stdout, b"part");
        drop(tx);
    }

    struct FailingRead {
        first: Option<&'static [u8]>,
    }

    impl Read for FailingRead {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            match self.first.take() {
                Some(b) => {
                    buf[..b.len()].copy_from_slice(b);
                    Ok(b.len())
                }
                None => Err(std::io::Error::other("pipe broke")),
            }
        }
    }

    #[test]
    fn pump_sends_end_only_at_eof() {
        let (tx, rx) = mpsc::channel();
        pump(&b"hello"[..], Stdout, &tx);
        pump(&b""[..], Stderr, &tx);
        drop(tx);
        let d = drain(&rx, || Duration::ZERO);
        assert_eq!(d.stdout, b"hello");
        assert!(d.stdout_complete && d.stderr_complete);

        let (tx, rx) = mpsc::channel();
        pump(FailingRead { first: Some(b"he") }, Stderr, &tx);
        pump(&b"ok"[..], Stdout, &tx);
        drop(tx);
        let d = drain(&rx, || Duration::ZERO);
        assert_eq!(d.stderr, b"he");
        assert!(!d.stderr_complete, "a read error is not an end");
        assert!(d.stdout_complete);
    }

    fn output(stdout: &str, stdout_complete: bool) -> CmdOutput {
        CmdOutput {
            success: true,
            code: Some(0),
            stdout: stdout.into(),
            stderr: "warning: x".into(),
            stdout_complete,
            stderr_complete: true,
        }
    }

    #[test]
    fn incomplete_capture_is_an_error_naming_status_and_streams() {
        assert!(output("a", true).require_complete("probe").is_ok());
        let err = format!(
            "{:#}",
            output("partial tree", false)
                .require_complete("cargo tree")
                .unwrap_err()
        );
        for want in [
            "cargo tree",
            "not fully captured",
            "exit code 0",
            "stdout (INCOMPLETE)",
            "partial tree",
            "warning: x",
        ] {
            assert!(err.contains(want), "missing {want:?} in:\n{err}");
        }
    }

    #[test]
    fn missing_program_is_none() {
        let r = run_with_timeout(
            &mut Command::new("earshot-definitely-not-a-program"),
            Duration::from_secs(5),
        );
        assert!(r.is_none());
    }

    #[test]
    fn slow_command_is_killed() {
        let mut cmd = if cfg!(windows) {
            let mut c = Command::new("powershell");
            c.args(["-NoProfile", "-Command", "Start-Sleep 60"]);
            c
        } else {
            let mut c = Command::new("sleep");
            c.arg("60");
            c
        };
        let start = Instant::now();
        let r = run_with_timeout(&mut cmd, Duration::from_millis(300));
        assert!(r.is_none());
        // A wide margin that still tells "killed" from "ran its 60 s to the end" (process
        // creation is slow on a loaded host).
        assert!(start.elapsed() < Duration::from_secs(30));
    }

    #[test]
    fn captures_stdout() {
        let r = run_with_timeout(
            Command::new(env!("CARGO")).arg("--version"),
            Duration::from_secs(30),
        )
        .expect("cargo --version runs");
        assert!(r.success);
        assert!(r.complete());
        assert_eq!(r.code, Some(0));
        assert!(r.stdout.starts_with("cargo "), "{:?}", r.stdout);
    }

    /// The original fail-open: the child exits 0 at once while a grandchild keeps its
    /// stdout open. The old runner returned `success` with an empty stdout; now the
    /// stream is flagged incomplete and `require_complete` refuses it.
    #[test]
    fn a_grandchild_holding_stdout_open_gives_incomplete_output_not_empty_success() {
        let mut cmd = if cfg!(windows) {
            let mut c = Command::new("cmd");
            c.args([
                "/c",
                "echo before&& start /b powershell -NoProfile -Command Start-Sleep 30",
            ]);
            c
        } else {
            let mut c = Command::new("sh");
            c.args(["-c", "echo before; sleep 30 &"]);
            c
        };
        let start = Instant::now();
        let r = run_with_drain(
            &mut cmd,
            Duration::from_secs(60),
            |_| Duration::from_secs(3),
            None,
        )
        .expect("the shell runs");
        assert!(r.success, "{}", r.describe());
        assert!(!r.stdout_complete, "{}", r.describe());
        assert!(r.stdout.contains("before"), "{}", r.describe());
        assert!(r.require_complete("probe").is_err());
        // The drain is bounded: the 30 s grandchild is not waited for.
        assert!(
            start.elapsed() < Duration::from_secs(20),
            "{:?}",
            start.elapsed()
        );
    }

    /// Where the helper writes its grandchild's process id.
    const GRANDCHILD_PID_FILE: &str = "EARSHOT_XTASK_PROC_GRANDCHILD_PID_FILE";

    /// The helper's own name, as libtest's `--exact` wants it.
    const HELPER: &str = "proc::tests::proc_job_helper_starts_a_grandchild_with_no_console";

    /// Run by the job tests as the direct child of a timed-out command (the test binary run
    /// again): starts a grandchild with no console at all (`DETACHED_PROCESS`), so nothing
    /// about cmd's or this binary's console can end it, writes its process id and waits.
    /// Run by hand, without the variable, it does nothing.
    #[test]
    #[ignore = "helper: run as a child process by the proc_job tests"]
    fn proc_job_helper_starts_a_grandchild_with_no_console() {
        let Some(file) = std::env::var_os(GRANDCHILD_PID_FILE) else {
            return;
        };
        let mut cmd = Command::new("ping");
        cmd.args(["-n", "60", "127.0.0.1"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            /// `DETACHED_PROCESS` (winbase.h).
            const DETACHED_PROCESS: u32 = 0x0000_0008;
            cmd.creation_flags(DETACHED_PROCESS);
        }
        let mut grandchild = cmd.spawn().expect("ping starts");
        std::fs::write(&file, grandchild.id().to_string()).expect("pid file");
        // Waits as long as the grandchild runs (60 s), well past the 15 s deadline.
        let _ = grandchild.wait();
    }

    /// Whether process `pid` still runs (a query by id only).
    #[cfg(windows)]
    fn running(pid: u32) -> bool {
        let out = Command::new("tasklist")
            .args(["/FI", &format!("PID eq {pid}"), "/NH", "/FO", "CSV"])
            .output()
            .expect("tasklist runs");
        String::from_utf8_lossy(&out.stdout).contains(&format!("\"{pid}\""))
    }

    /// Runs the helper under a 15 s deadline (`in_job` or not), which it outlives; returns
    /// its grandchild's process id.
    #[cfg(windows)]
    fn time_out_the_helper(in_job: bool) -> u32 {
        let file = std::env::temp_dir().join(format!(
            "earshot-xtask-proc-{}-{in_job}.pid",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&file);
        let mut cmd = Command::new(std::env::current_exe().unwrap());
        cmd.args(["--exact", HELPER, "--ignored", "--nocapture"])
            .env(GRANDCHILD_PID_FILE, &file);
        let deadline = Duration::from_secs(15);
        let out = if in_job {
            run_in_job_with_timeout(&mut cmd, deadline)
        } else {
            run_with_timeout(&mut cmd, deadline)
        };
        assert!(out.is_none(), "the helper waits 60 s: {out:?}");
        let text = std::fs::read_to_string(&file)
            .unwrap_or_else(|e| panic!("the helper did not start a grandchild in 15 s: {e}"));
        let _ = std::fs::remove_file(&file);
        text.trim().parse().unwrap()
    }

    #[test]
    #[cfg_attr(not(windows), ignore = "job objects are Windows only")]
    fn proc_job_a_timeout_ends_the_grandchildren_too() {
        #[cfg(windows)]
        {
            let pid = time_out_the_helper(true);
            let until = Instant::now() + Duration::from_secs(10);
            while running(pid) {
                assert!(Instant::now() < until, "grandchild {pid} outlived the job");
                thread::sleep(Duration::from_millis(200));
            }
        }
    }

    #[test]
    #[cfg_attr(not(windows), ignore = "job objects are Windows only")]
    fn proc_job_negative_control_without_a_job_the_grandchild_outlives_the_timeout() {
        #[cfg(windows)]
        {
            let pid = time_out_the_helper(false);
            let alive = running(pid);
            // Stop the one process this test started, by its id.
            let _ = Command::new("taskkill")
                .args(["/PID", &pid.to_string(), "/F"])
                .output();
            assert!(
                alive,
                "grandchild {pid} ended without a job: the control proves nothing"
            );
        }
    }
}
