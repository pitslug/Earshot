use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};

use super::*;
use crate::plain_text::INTERNAL;

/// Sets its flag when dropped (stands for a gate pass).
struct Pass(Arc<AtomicBool>);

impl Drop for Pass {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

const OS_TEXT: &str = "no threads left (os error 8)";

fn no_threads(_: &'static str, _: Box<dyn FnOnce() + Send>) -> std::io::Result<JoinHandle<()>> {
    Err(std::io::Error::other(OS_TEXT))
}

#[test]
fn slot_job_a_thread_that_cannot_start_reports_fixed_copy_and_drops_the_pass() {
    let job = SlotJob::new("earshot-test-job", "Already running.").with_spawn(no_threads);
    let dropped = Arc::new(AtomicBool::new(false));
    let pass = Pass(Arc::clone(&dropped));
    let sent = Arc::new(AtomicBool::new(false));
    let sent_flag = Arc::clone(&sent);
    let err = job
        .start(
            move || Ok(pass),
            |_| "done",
            || "panicked",
            move |_| sent_flag.store(true, Ordering::SeqCst),
        )
        .expect_err("the thread could not start");
    assert_eq!(err, IpcError::internal());
    assert_eq!(err.kind, "internal");
    assert_eq!(err.message, INTERNAL);
    assert!(!err.retryable);
    assert!(!err.message.contains("os error"), "{}", err.message);
    assert!(
        dropped.load(Ordering::SeqCst),
        "the pass went with the thread"
    );
    assert!(
        !sent.load(Ordering::SeqCst),
        "no event for a job that never ran"
    );
}

#[test]
fn slot_job_a_failed_start_leaves_the_slot_free() {
    let job = SlotJob::new("earshot-test-job", "Already running.").with_spawn(no_threads);
    assert!(job.start(|| Ok(()), |_| (), || (), |()| ()).is_err());
    let job = job.with_spawn(spawn_named);
    let (tx, rx) = mpsc::channel();
    job.start(|| Ok(()), |_| 7, || 0, move |e| tx.send(e).unwrap())
        .expect("a later start runs");
    job.join();
    assert_eq!(rx.recv().unwrap(), 7);
}

#[test]
fn slot_job_refuses_a_second_start_busy_while_one_runs() {
    let job = SlotJob::new("earshot-test-job", "Already running.");
    let (release_tx, release_rx) = mpsc::channel::<()>();
    let entered = Arc::new(AtomicBool::new(false));
    job.start(
        || Ok(()),
        move |_| {
            let _ = release_rx.recv();
        },
        || (),
        |()| (),
    )
    .unwrap();
    let flag = Arc::clone(&entered);
    let err = job
        .start(
            move || {
                flag.store(true, Ordering::SeqCst);
                Ok(())
            },
            |_| (),
            || (),
            |()| (),
        )
        .expect_err("one at a time");
    assert_eq!(err, IpcError::busy("Already running."));
    assert!(err.retryable);
    assert!(
        !entered.load(Ordering::SeqCst),
        "no pass taken for a refused start"
    );
    release_tx.send(()).unwrap();
    job.join();
}

#[test]
fn slot_job_a_panic_sends_the_panic_event_once_after_the_pass_is_dropped() {
    let job = SlotJob::new("earshot-test-job", "Already running.");
    let dropped = Arc::new(AtomicBool::new(false));
    let pass = Pass(Arc::clone(&dropped));
    let seen = Arc::new(Mutex::new(Vec::new()));
    let (seen2, dropped2) = (Arc::clone(&seen), Arc::clone(&dropped));
    job.start(
        move || Ok(pass),
        |_| -> &'static str { panic!("the work panicked") },
        || "panicked",
        move |e| {
            seen2
                .lock()
                .unwrap()
                .push((e, dropped2.load(Ordering::SeqCst)));
        },
    )
    .unwrap();
    job.join();
    assert_eq!(*seen.lock().unwrap(), vec![("panicked", true)]);
}

#[test]
fn slot_job_cancel_reaches_the_running_work() {
    let job = SlotJob::new("earshot-test-job", "Already running.");
    let (started_tx, started_rx) = mpsc::channel();
    let (tx, rx) = mpsc::channel();
    job.start(
        || Ok(()),
        move |token: &CancelToken| {
            started_tx.send(()).unwrap();
            while !token.is_cancelled() {
                std::thread::yield_now();
            }
            "cancelled"
        },
        || "panicked",
        move |e| tx.send(e).unwrap(),
    )
    .unwrap();
    started_rx.recv().unwrap();
    job.cancel();
    job.join();
    assert_eq!(rx.recv().unwrap(), "cancelled");
}

#[test]
fn slot_job_progress_reports_once_per_step_capped_at_the_total() {
    let mut seen = Vec::new();
    {
        let mut progress = progress_every(
            Duration::from_millis(500),
            Duration::from_millis(1200),
            |done, total| seen.push((done, total)),
        );
        for ms in [0, 100, 499, 500, 900, 1100, 1600] {
            progress(Duration::from_millis(ms));
        }
    }
    assert_eq!(
        seen,
        vec![(0, 1200), (500, 1200), (1100, 1200), (1200, 1200)]
    );
}
