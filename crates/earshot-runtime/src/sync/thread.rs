//! The sync thread (docs/sync-spec.md section 6.2): a tick every 30 s, sooner after a local
//! write, and a back-off after a failure.
//!
//! Local writes are seen without any writer knowing about sync: the thread reads the outbox's
//! length every [`SyncTiming::poll`] and ticks once it has stopped changing for one poll (a
//! debounce). During a recording that is mostly the regular tick, plus one after each pause in
//! speech of a poll or more; the recording's own rows stay queued either way
//! ([`super::SyncClient::tick`]).
//!
//! Stopping ends a tick in progress at its next page, so a stop waits for at most the request
//! on the wire (each is bounded by its timeout). A host on its quit path should not wait for
//! even that: drop the thread off the quit path or bound the wait.

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use super::{SyncClient, SyncProblem, record_problem};
use crate::RuntimeError;
use crate::services::Services;

/// The regular tick.
pub const TICK: Duration = Duration::from_secs(30);
/// The longest back-off after failures.
pub const MAX_BACKOFF: Duration = Duration::from_secs(15 * 60);

/// The wait before the next tick after `failures` failed ticks in a row: [`TICK`] with none,
/// then doubling from it (30 s, 1 min, 2 min, ...) up to [`MAX_BACKOFF`].
#[must_use]
pub fn backoff(failures: u32) -> Duration {
    if failures == 0 {
        return TICK;
    }
    let factor = 1u32.checked_shl(failures - 1).unwrap_or(u32::MAX);
    TICK.checked_mul(factor)
        .map_or(MAX_BACKOFF, |d| d.min(MAX_BACKOFF))
}

/// The thread's timings (the defaults in production; short ones in tests).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SyncTiming {
    /// The wait after a tick without a problem.
    pub tick: Duration,
    /// How often the outbox's length is read (also the debounce).
    pub poll: Duration,
}

impl Default for SyncTiming {
    fn default() -> Self {
        Self {
            tick: TICK,
            poll: Duration::from_secs(2),
        }
    }
}

#[derive(Debug, Default)]
struct Signal {
    stop: bool,
    soon: bool,
}

/// The running sync thread (`earshot-sync`). Stopped and joined by [`SyncThread::stop`] or
/// on drop; a tick in progress finishes first (each request is bounded by its timeout).
pub struct SyncThread {
    signal: Arc<(Mutex<Signal>, Condvar)>,
    stop: Arc<std::sync::atomic::AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl std::fmt::Debug for SyncThread {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SyncThread").finish_non_exhaustive()
    }
}

impl SyncThread {
    /// Starts the thread: a tick at once, then as [`SyncTiming`] says. A problem that
    /// [`SyncProblem::stops`] ends the thread (the person must act); a panic is caught,
    /// recorded as [`SyncProblem::Internal`] and ends it.
    ///
    /// # Errors
    /// [`RuntimeError::Spawn`].
    pub fn start(client: SyncClient, timing: SyncTiming) -> Result<Self, RuntimeError> {
        let signal = Arc::new((Mutex::new(Signal::default()), Condvar::new()));
        let stop = client.stop_flag();
        let theirs = Arc::clone(&signal);
        let handle = std::thread::Builder::new()
            .name("earshot-sync".into())
            .spawn(move || run(&client, timing, &theirs))
            .map_err(RuntimeError::Spawn)?;
        Ok(Self {
            signal,
            stop,
            handle: Some(handle),
        })
    }

    /// Asks for a tick soon (after a local change a host knows of; polling finds the rest).
    pub fn nudge(&self) {
        let (lock, cv) = &*self.signal;
        lock.lock().unwrap_or_else(PoisonError::into_inner).soon = true;
        cv.notify_all();
    }

    /// Stops the thread and waits for it.
    pub fn stop(mut self) {
        self.stop_and_join();
    }

    fn stop_and_join(&mut self) {
        let (lock, cv) = &*self.signal;
        self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
        lock.lock().unwrap_or_else(PoisonError::into_inner).stop = true;
        cv.notify_all();
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }

    /// Whether the thread has ended (a problem that stops it, or a panic).
    #[must_use]
    pub fn ended(&self) -> bool {
        self.handle.as_ref().is_none_or(JoinHandle::is_finished)
    }
}

impl Drop for SyncThread {
    fn drop(&mut self) {
        self.stop_and_join();
    }
}

fn outbox_len(services: &Services) -> Option<u64> {
    services.store().outbox_len().ok()
}

fn run(client: &SyncClient, timing: SyncTiming, signal: &(Mutex<Signal>, Condvar)) {
    let services = client.services().clone();
    let mut failures: u32 = 0;
    let mut next = Instant::now();
    let mut seen = outbox_len(&services);
    let mut changed = false;
    loop {
        // Wait until the next tick, a poll, a nudge or a stop.
        {
            let (lock, cv) = signal;
            let mut s = lock.lock().unwrap_or_else(PoisonError::into_inner);
            let wait = next
                .saturating_duration_since(Instant::now())
                .min(timing.poll);
            if !s.stop && !s.soon && !wait.is_zero() {
                s = cv
                    .wait_timeout(s, wait)
                    .unwrap_or_else(PoisonError::into_inner)
                    .0;
            }
            if s.stop {
                return;
            }
            if std::mem::take(&mut s.soon) && failures == 0 {
                next = next.min(Instant::now());
            }
        }
        // A local write: tick once the outbox stops changing (only between failures).
        let len = outbox_len(&services);
        if len != seen {
            seen = len;
            changed = true;
        } else if changed {
            changed = false;
            if failures == 0 && len.is_some_and(|n| n > 0) {
                next = next.min(Instant::now());
            }
        }
        if Instant::now() < next {
            continue;
        }
        match catch_unwind(AssertUnwindSafe(|| client.tick())) {
            Ok(Ok(_)) => {
                failures = 0;
                next = Instant::now() + timing.tick;
            }
            Ok(Err(p)) if p.stops() => return,
            Ok(Err(_)) => {
                failures = failures.saturating_add(1);
                next = Instant::now() + scaled(backoff(failures), timing.tick);
            }
            Err(_) => {
                record_problem(&services, SyncProblem::Internal);
                return;
            }
        }
        seen = outbox_len(&services);
        changed = false;
    }
}

/// `wait` scaled by `tick / TICK`: the back-off as production has it, shortened with the
/// tick in tests.
fn scaled(wait: Duration, tick: Duration) -> Duration {
    let nanos = wait.as_nanos() * tick.as_nanos() / TICK.as_nanos();
    Duration::from_nanos(u64::try_from(nanos).unwrap_or(u64::MAX))
}
