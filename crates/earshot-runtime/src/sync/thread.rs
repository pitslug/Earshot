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
//!
//! Going away (T11.3ca): when the app quits or Windows sleeps the host calls
//! [`SyncThread::say_gone`], which sends gone presence from a thread of its own (a tick in
//! progress cannot hold it up) and stops the regular beats; [`SyncThread::back`] after a resume
//! makes the thread tick at once and beat after the first tick that reaches the relay.
//! [`super::presence::Beats`] decides what is sent, behind one lock held across every presence
//! send, so a gone and a beat never overtake each other.
//!
//! Summary offload (T11.3da): a thread started with [`SyncThread::start_offloading`] runs
//! [`crate::offload::Offload::step`] after every tick, whatever its outcome (an unclaimed
//! request is withdrawn on time even while the relay cannot be reached), handing work to the
//! host's [`OffloadHost`].

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, sync_channel};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use super::presence::Beats;
use super::{SyncClient, SyncProblem, record_problem};
use crate::RuntimeError;
use crate::offload::{Offload, OffloadHost};
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
    /// How often this device's presence is sent ([`super::PRESENCE_EVERY`]).
    pub presence: Duration,
}

impl Default for SyncTiming {
    fn default() -> Self {
        Self {
            tick: TICK,
            poll: Duration::from_secs(2),
            presence: super::PRESENCE_EVERY,
        }
    }
}

#[derive(Debug, Default)]
struct Signal {
    stop: bool,
    soon: bool,
    /// Say gone on the way out (sync turned off). A thread replaced by a new one does not,
    /// or its gone could land after the new thread's announcement.
    gone: bool,
    /// Back from sleep: send a beat and tick now.
    back: bool,
}

/// The running sync thread (`earshot-sync`). Stopped and joined by [`SyncThread::stop`] or
/// on drop; a tick in progress finishes first (each request is bounded by its timeout).
pub struct SyncThread {
    signal: Arc<(Mutex<Signal>, Condvar)>,
    stop: Arc<std::sync::atomic::AtomicBool>,
    handle: Option<JoinHandle<()>>,
    client: Arc<SyncClient>,
    /// The presence decision, and the lock every presence send holds.
    beats: Arc<Mutex<Beats>>,
    /// Set by [`Self::say_gone`], cleared by [`Self::back`]: a gone thread sends only while it
    /// is still set, so a resume that overtook it is not undone.
    leaving: Arc<AtomicBool>,
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
        Self::start_with(client, timing, None)
    }

    /// [`SyncThread::start`], and summary offload after every tick
    /// ([`crate::offload::Offload::step`], work handed to `host`).
    ///
    /// # Errors
    /// [`RuntimeError::Spawn`].
    pub fn start_offloading(
        client: SyncClient,
        timing: SyncTiming,
        host: Arc<dyn OffloadHost>,
    ) -> Result<Self, RuntimeError> {
        Self::start_with(client, timing, Some((Arc::new(Offload::new()), host)))
    }

    fn start_with(
        client: SyncClient,
        timing: SyncTiming,
        offload: Option<(Arc<Offload>, Arc<dyn OffloadHost>)>,
    ) -> Result<Self, RuntimeError> {
        let signal = Arc::new((Mutex::new(Signal::default()), Condvar::new()));
        let stop = client.stop_flag();
        let client = Arc::new(client);
        let beats = Arc::new(Mutex::new(Beats::default()));
        let theirs = Arc::clone(&signal);
        let (c, b) = (Arc::clone(&client), Arc::clone(&beats));
        let handle = std::thread::Builder::new()
            .name("earshot-sync".into())
            .spawn(move || run(&c, timing, &theirs, &b, offload.as_ref()))
            .map_err(RuntimeError::Spawn)?;
        Ok(Self {
            signal,
            stop,
            handle: Some(handle),
            client,
            beats,
            leaving: Arc::new(AtomicBool::new(false)),
        })
    }

    /// This device is going away (the app quits, Windows sleeps): a thread of its own
    /// (`earshot-sync-gone`) takes the presence lock, behind any beat in flight, stops the
    /// regular beats and, when the other devices have seen this one online, sends gone (best
    /// effort, bounded by the presence timeout). Returns a receiver that hears once that thread
    /// is done, for a caller that may wait a bounded time; `None` when it is already leaving or
    /// the thread cannot start. Never blocks.
    #[must_use]
    pub fn say_gone(&self) -> Option<Receiver<()>> {
        if self.leaving.swap(true, Ordering::SeqCst) {
            return None;
        }
        let (client, beats, leaving) = (
            Arc::clone(&self.client),
            Arc::clone(&self.beats),
            Arc::clone(&self.leaving),
        );
        let (tx, rx) = sync_channel(1);
        std::thread::Builder::new()
            .name("earshot-sync-gone".into())
            .spawn(move || {
                {
                    let mut b = beats_of(&beats);
                    // A resume that came first wins: nothing to take back.
                    if leaving.load(Ordering::SeqCst) && b.go_away() {
                        let _ = catch_unwind(AssertUnwindSafe(|| client.send_presence(true)));
                    }
                }
                let _ = tx.send(());
            })
            .ok()
            .map(|_| rx)
    }

    /// This device is back (Windows resumed): the thread ticks at once whatever the back-off,
    /// and beats again after the first tick that reaches the relay (the network may still be
    /// coming up). Never blocks.
    pub fn back(&self) {
        self.leaving.store(false, Ordering::SeqCst);
        let (signal, cv) = &*self.signal;
        signal.lock().unwrap_or_else(PoisonError::into_inner).back = true;
        cv.notify_all();
    }

    /// Asks for a tick soon (after a local change a host knows of; polling finds the rest).
    pub fn nudge(&self) {
        let (lock, cv) = &*self.signal;
        lock.lock().unwrap_or_else(PoisonError::into_inner).soon = true;
        cv.notify_all();
    }

    /// Stops the thread and waits for it, saying nothing to the other devices (it is being
    /// replaced, or the device is being removed, which the relay records itself).
    pub fn stop(mut self) {
        self.stop_and_join(false);
    }

    /// Stops the thread for good and waits for it; on the way out it tells the other devices
    /// it is gone (best effort, bounded by the presence timeout), so they stop showing it
    /// online at once rather than after the online window.
    pub fn stop_saying_gone(mut self) {
        self.stop_and_join(true);
    }

    fn stop_and_join(&mut self, gone: bool) {
        let (lock, cv) = &*self.signal;
        self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
        {
            let mut s = lock.lock().unwrap_or_else(PoisonError::into_inner);
            s.stop = true;
            s.gone |= gone;
        }
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
        self.stop_and_join(false);
    }
}

fn outbox_len(services: &Services) -> Option<u64> {
    services.store().outbox_len().ok()
}

fn beats_of(beats: &Mutex<Beats>) -> MutexGuard<'_, Beats> {
    beats.lock().unwrap_or_else(PoisonError::into_inner)
}

fn run(
    client: &SyncClient,
    timing: SyncTiming,
    signal: &(Mutex<Signal>, Condvar),
    beats: &Mutex<Beats>,
    offload: Option<&(Arc<Offload>, Arc<dyn OffloadHost>)>,
) {
    let services = client.services().clone();
    let mut failures: u32 = 0;
    let mut next = Instant::now();
    let mut seen = outbox_len(&services);
    let mut changed = false;
    // Presence goes out after the first tick without a problem, then every `presence`.
    let mut next_presence: Option<Instant> = None;
    loop {
        // Wait until the next tick, a poll, a nudge or a stop.
        {
            let (lock, cv) = signal;
            let mut s = lock.lock().unwrap_or_else(PoisonError::into_inner);
            let due = next_presence.map_or(next, |p| p.min(next));
            let wait = due
                .saturating_duration_since(Instant::now())
                .min(timing.poll);
            if !s.stop && !s.soon && !s.back && !wait.is_zero() {
                s = cv
                    .wait_timeout(s, wait)
                    .unwrap_or_else(PoisonError::into_inner)
                    .0;
            }
            if s.stop {
                let say_gone = s.gone;
                drop(s);
                // Best effort: the other devices see this one go at once, not after the
                // online window.
                let b = beats_of(beats);
                if b.announced() && say_gone {
                    let _ = catch_unwind(AssertUnwindSafe(|| client.send_presence(true)));
                }
                drop(b);
                return;
            }
            if std::mem::take(&mut s.soon) && failures == 0 {
                next = next.min(Instant::now());
            }
            if std::mem::take(&mut s.back) {
                // A resume: tick at once whatever the back-off, and beat after the first tick
                // that reaches the relay (the tick's success arm sets the beat due).
                if beats_of(beats).come_back() {
                    next = Instant::now();
                    next_presence = None;
                }
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
        if let Some(at) = next_presence
            && Instant::now() >= at
        {
            // Advisory: a failure is the next tick's to report. Decided and sent under the
            // presence lock, so a going away either stops this beat or queues behind it.
            let mut b = beats_of(beats);
            if b.beat_due()
                && let Ok(Ok(())) = catch_unwind(AssertUnwindSafe(|| client.send_presence(false)))
            {
                b.sent_online();
            }
            drop(b);
            next_presence = Some(Instant::now() + timing.presence);
        }
        if Instant::now() < next {
            continue;
        }
        let ticked = catch_unwind(AssertUnwindSafe(|| client.tick()));
        if let Some((state, host)) = offload
            && !matches!(&ticked, Ok(Err(p)) if p.stops())
        {
            // A store failure or a panic here is the next step's to retry; it never ends sync.
            let _ = catch_unwind(AssertUnwindSafe(|| {
                state.step(&services, &mut || client.peers().ok(), &**host)
            }));
        }
        match ticked {
            Ok(Ok(_)) => {
                failures = 0;
                next = Instant::now() + timing.tick;
                if next_presence.is_none() {
                    next_presence = Some(Instant::now());
                }
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
