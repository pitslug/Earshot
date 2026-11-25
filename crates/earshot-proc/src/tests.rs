use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use super::*;

fn spec(program: &str, args: &[&str]) -> ChildSpec {
    ChildSpec {
        program: program.into(),
        args: args.iter().map(OsString::from).collect(),
        current_dir: std::env::temp_dir(),
        env_remove: Vec::new(),
        env_set: Vec::new(),
        stdin: Vec::new(),
        deadline: Duration::from_secs(60),
        max_output: 1 << 20,
    }
}

fn never() -> impl Fn() -> bool {
    || false
}

#[test]
#[cfg_attr(not(windows), ignore = "uses cmd.exe and findstr")]
fn proc_run_feeds_stdin_and_reads_both_outputs_to_the_end() {
    let mut s = spec("findstr", &["/r", "^"]);
    s.stdin = b"first line\r\nsecond line\r\n".to_vec();
    let out = run(&s, &never()).unwrap();
    assert!(out.status.success());
    assert!(out.complete());
    assert_eq!(out.stdout, b"first line\r\nsecond line\r\n");
    let out = run(&spec("cmd", &["/c", "echo oops 1>&2 & exit 3"]), &never()).unwrap();
    assert_eq!(out.status.code(), Some(3));
    assert!(String::from_utf8_lossy(&out.stderr).contains("oops"));
    assert!(out.clone().require_complete().is_ok());
}

#[test]
#[cfg_attr(not(windows), ignore = "uses cmd.exe")]
fn proc_run_sets_and_removes_environment_variables() {
    let mut s = spec("cmd", &["/c", "echo [%EARSHOT_PROC_SET%][%PATHEXT_GONE%]"]);
    s.env_set.push(("EARSHOT_PROC_SET".into(), "here".into()));
    s.env_set.push(("PATHEXT_GONE".into(), "present".into()));
    s.env_remove.push("PATHEXT_GONE".into());
    // Set after the removal wins (documented order); a removal of an unset name is fine.
    let out = run(&s, &never()).unwrap();
    assert_eq!(
        String::from_utf8_lossy(&out.stdout).trim(),
        "[here][present]"
    );
    let mut s = spec("cmd", &["/c", "echo [%USERNAME%]"]);
    s.env_remove.push("USERNAME".into());
    let out = run(&s, &never()).unwrap();
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "[%USERNAME%]");
}

#[test]
#[cfg_attr(not(windows), ignore = "uses ping.exe")]
fn proc_run_kills_a_child_past_its_deadline() {
    let mut s = spec("ping", &["-n", "120", "127.0.0.1"]);
    s.deadline = Duration::from_millis(300);
    let started = Instant::now();
    let e = run(&s, &never()).unwrap_err();
    assert!(matches!(e, ChildError::TimedOut(_)), "{e}");
    assert!(
        started.elapsed() < Duration::from_secs(30),
        "{:?}",
        started.elapsed()
    );
}

/// A clock the test moves by hand, keeping time awake and time asleep apart (T12.62's fake).
#[derive(Clone, Default)]
struct HandClock {
    awake_ms: Arc<AtomicU64>,
    asleep_ms: Arc<AtomicU64>,
    reads: Arc<AtomicU64>,
}

impl HandClock {
    fn ms(counter: &AtomicU64) -> u64 {
        counter.load(Ordering::SeqCst)
    }

    /// What the code under test is given: time awake only.
    fn awake(&self) -> AwakeClock {
        let me = self.clone();
        AwakeClock::from_fn(move || {
            me.reads.fetch_add(1, Ordering::SeqCst);
            Duration::from_millis(Self::ms(&me.awake_ms))
        })
    }

    /// What `Instant` reads on Windows: time awake and time asleep (the negative control).
    fn wall(&self) -> AwakeClock {
        let me = self.clone();
        AwakeClock::from_fn(move || {
            me.reads.fetch_add(1, Ordering::SeqCst);
            Duration::from_millis(Self::ms(&me.awake_ms) + Self::ms(&me.asleep_ms))
        })
    }

    /// Time passes with the computer awake.
    fn advance(&self, by: Duration) {
        self.awake_ms
            .fetch_add(u64::try_from(by.as_millis()).unwrap(), Ordering::SeqCst);
    }

    /// Time passes with the computer asleep.
    fn sleep(&self, by: Duration) {
        self.asleep_ms
            .fetch_add(u64::try_from(by.as_millis()).unwrap(), Ordering::SeqCst);
    }

    /// Waits until the code under test has read the clock twice more: it has then judged the
    /// time as it is now at least once. The bound is only for a failing case.
    fn read_twice_more(&self) {
        let from = Self::ms(&self.reads);
        let until = Instant::now() + Duration::from_secs(60);
        while Self::ms(&self.reads) < from + 2 {
            assert!(Instant::now() < until, "the clock is no longer read");
            std::thread::sleep(Duration::from_millis(1));
        }
    }
}

/// A child that runs for two minutes unless it is killed, under `clock`, with a deadline of an
/// hour; its result when the run ends.
#[cfg(windows)]
fn long_run_on(clock: AwakeClock) -> mpsc::Receiver<Result<ChildOutput, ChildError>> {
    let (done, result) = mpsc::channel();
    std::thread::spawn(move || {
        let mut s = spec("ping", &["-n", "120", "127.0.0.1"]);
        s.deadline = Duration::from_secs(3600);
        let _ = done.send(run_on(&s, &|| false, &mut |_| true, &clock));
    });
    result
}

#[test]
#[cfg_attr(not(windows), ignore = "uses ping.exe")]
fn proc_run_deadline_counts_time_awake_not_time_asleep() {
    // T14.8: a child running when the computer goes to sleep is not killed for the hours it
    // slept; the deadline is time awake.
    #[cfg(windows)]
    {
        let timed_out = |r: Result<ChildOutput, ChildError>| {
            assert!(
                matches!(r, Err(ChildError::TimedOut(d)) if d == Duration::from_secs(3600)),
                "{r:?}"
            );
        };
        let guard = Duration::from_secs(60);
        let clock = HandClock::default();
        let result = long_run_on(clock.awake());
        clock.read_twice_more();
        clock.sleep(Duration::from_secs(8 * 3600));
        clock.read_twice_more();
        assert!(result.try_recv().is_err(), "killed for the time asleep");
        clock.advance(Duration::from_secs(3599));
        clock.read_twice_more();
        assert!(result.try_recv().is_err(), "killed short of its deadline");
        clock.advance(Duration::from_secs(1));
        timed_out(result.recv_timeout(guard).expect("the deadline never came"));

        // The negative control: on a clock that counts the sleep, as `Instant` does on Windows,
        // the same night kills the child.
        let clock = HandClock::default();
        let result = long_run_on(clock.wall());
        clock.read_twice_more();
        clock.sleep(Duration::from_secs(8 * 3600));
        timed_out(
            result
                .recv_timeout(guard)
                .expect("the sleep was not counted"),
        );
    }
}

#[test]
fn proc_awake_clock_reads_forward_and_never_trips_on_a_clock_that_went_back() {
    let clock = AwakeClock::system();
    let first = clock.now();
    assert!(clock.now() >= first);
    assert!(clock.since(first) < Duration::from_secs(60));
    // A reading from the future (a clock that went backwards since) is no time at all.
    assert_eq!(
        clock.since(first + Duration::from_secs(3600)),
        Duration::ZERO
    );
    let hand = HandClock::default();
    let (awake, wall) = (hand.awake(), hand.wall());
    hand.advance(Duration::from_secs(5));
    hand.sleep(Duration::from_secs(100));
    assert_eq!(awake.now(), Duration::from_secs(5));
    assert_eq!(wall.now(), Duration::from_secs(105));
    assert_eq!(awake.since(Duration::from_secs(2)), Duration::from_secs(3));
}

#[test]
#[cfg_attr(not(windows), ignore = "uses ping.exe")]
fn proc_run_kills_a_cancelled_child() {
    let flag = Arc::new(AtomicBool::new(false));
    let setter = Arc::clone(&flag);
    let t = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(200));
        setter.store(true, Ordering::SeqCst);
    });
    let started = Instant::now();
    let e = run(&spec("ping", &["-n", "120", "127.0.0.1"]), &|| {
        flag.load(Ordering::SeqCst)
    })
    .unwrap_err();
    t.join().unwrap();
    assert!(matches!(e, ChildError::Cancelled), "{e}");
    assert!(started.elapsed() < Duration::from_secs(30));
    // Already cancelled: nothing starts.
    let e = run(&spec("no-such-program-earshot", &[]), &|| true).unwrap_err();
    assert!(matches!(e, ChildError::Cancelled));
}

#[test]
#[cfg_attr(not(windows), ignore = "uses cmd.exe")]
fn proc_run_marks_output_past_the_cap_incomplete() {
    let mut s = spec("cmd", &["/c", "echo 0123456789"]);
    s.max_output = 4;
    let out = run(&s, &never()).unwrap();
    assert_eq!(out.stdout, b"0123");
    assert!(!out.stdout_complete);
    assert!(out.stderr_complete);
    let e = out.require_complete().unwrap_err();
    assert!(
        matches!(
            e,
            ChildError::OutputIncomplete {
                stdout: false,
                stderr: true
            }
        ),
        "{e}"
    );
}

/// Processes whose parent was `pid` that still run (a query only, PowerShell CIM).
#[cfg(windows)]
fn children_of(pid: u32) -> Vec<u32> {
    let out = std::process::Command::new("powershell")
        .args([
            "-NoProfile",
            "-Command",
            &format!(
                "Get-CimInstance Win32_Process -Filter 'ParentProcessId={pid}' | ForEach-Object {{ $_.ProcessId }}"
            ),
        ])
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| l.trim().parse().ok())
        .collect()
}

#[test]
#[cfg_attr(not(windows), ignore = "uses cmd.exe, ping.exe and PowerShell")]
fn proc_run_kills_a_grandchild_that_holds_the_output_open() {
    // cmd exits at once; the ping it started in the background inherits standard output and
    // keeps it open, so the output never ends by itself: the job is killed after the drain
    // wait, the output is not complete, and no child of cmd is left running.
    #[cfg(windows)]
    {
        let started = Instant::now();
        let mut cmd_pid = 0;
        let out = run_with(
            &spec(
                "cmd",
                &["/c", "start /b ping -n 60 127.0.0.1 >nul & echo done"],
            ),
            &never(),
            &mut |pid| {
                cmd_pid = pid;
                true
            },
        )
        .unwrap();
        assert!(started.elapsed() < Duration::from_secs(30));
        assert!(!out.complete(), "{out:?}");
        assert_ne!(cmd_pid, 0);
        let until = Instant::now() + Duration::from_secs(10);
        loop {
            let left = children_of(cmd_pid);
            if left.is_empty() {
                break;
            }
            assert!(Instant::now() < until, "still running: {left:?}");
            std::thread::sleep(Duration::from_millis(200));
        }
    }
}

#[test]
#[cfg_attr(not(windows), ignore = "uses findstr")]
fn proc_run_with_a_refusal_writes_no_input_and_kills_the_child() {
    let mut s = spec("findstr", &["/r", "^"]);
    s.stdin = b"never read\r\n".to_vec();
    let mut asked = 0;
    let e = run_with(&s, &never(), &mut |_| {
        asked += 1;
        false
    })
    .unwrap_err();
    assert!(matches!(e, ChildError::Aborted), "{e}");
    assert_eq!(asked, 1);
}
#[test]
fn proc_run_reports_a_program_that_cannot_start() {
    let e = run(&spec("no-such-program-earshot-proc", &[]), &never()).unwrap_err();
    assert!(matches!(e, ChildError::Start(_)), "{e}");
}

#[test]
fn proc_read_capped_keeps_the_cap_and_drains_the_rest() {
    let (kept, complete) = read_capped(&b"abcdef"[..], 10);
    assert_eq!((kept.as_slice(), complete), (&b"abcdef"[..], true));
    let (kept, complete) = read_capped(&b"abcdef"[..], 6);
    assert_eq!((kept.as_slice(), complete), (&b"abcdef"[..], true));
    let (kept, complete) = read_capped(&b"abcdefg"[..], 6);
    assert_eq!((kept.as_slice(), complete), (&b"abcdef"[..], false));
}
