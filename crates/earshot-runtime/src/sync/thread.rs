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
    /// A new sync thread has started on this store ([`Superseder`]): this one does nothing
    /// more, whatever it was asked to finish.
    superseded: bool,
    /// Started held ([`SyncThread::start_held`]): the thread exists and does nothing until
    /// [`SyncThread::begin`] or a stop.
    held: bool,
}

/// Ends a sync thread from the moment another takes its place, without waiting for it (T14.45
/// review). A host keeps one for each thread it has retired and not yet seen end, and calls
/// [`Superseder::supersede`] on all of them once the new thread exists and before it begins
/// ([`SyncThread::start_held`], [`SyncThread::begin`]): a start that gets no thread leaves
/// them at their work.
///
/// A thread that is stopped still finishes what it was told to finish: a thread turned off
/// goes on handing back the summaries this device asked for, for up to
/// [`STOPPED_HANDBACK_LIMIT`]. A superseded thread does not. It starts no further tick (one in
/// progress ends at its next page, and the new thread's first tick waits for it), sends no
/// gone presence (the new thread's announcement would be undone) and runs no further offload
/// step: from then on the new thread owns the hand-backs, and its first step hands back what
/// is still owed, as a thread does after a restart. Each step of either thread runs under one
/// lock per store, and the superseded thread looks under that lock before it steps, so the two
/// never step at once and the old one never steps after the new one has.
#[derive(Clone)]
pub struct Superseder {
    signal: Arc<(Mutex<Signal>, Condvar)>,
    stop: Arc<AtomicBool>,
}

impl std::fmt::Debug for Superseder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Superseder").finish_non_exhaustive()
    }
}

impl Superseder {
    /// Tells the thread a new one has started. Never blocks (it takes only the signal lock,
    /// which the thread never holds while it waits for anything else); safe to call more than
    /// once and after the thread has ended.
    pub fn supersede(&self) {
        self.stop.store(true, Ordering::Relaxed);
        let (lock, cv) = &*self.signal;
        {
            let mut s = lock.lock().unwrap_or_else(PoisonError::into_inner);
            s.stop = true;
            s.superseded = true;
        }
        cv.notify_all();
    }

    /// Whether [`Self::supersede`] was called.
    #[must_use]
    pub fn is_superseded(&self) -> bool {
        self.signal
            .0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .superseded
    }
}

fn superseded(signal: &(Mutex<Signal>, Condvar)) -> bool {
    signal
        .0
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .superseded
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
        Self::start_with(client, timing, None, false)
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
        Self::start_with(
            client,
            timing,
            Some((Arc::new(Offload::new()), host)),
            false,
        )
    }

    /// [`SyncThread::start`] or, with a `host`, [`SyncThread::start_offloading`], except that
    /// the thread waits: it reads nothing, ticks nothing and runs no offload step until
    /// [`Self::begin`]. A stop or a superseding ends it as it is.
    ///
    /// For a host that replaces threads (T14.45 review): whether a thread can be started at
    /// all is known once this returns, so the threads it takes over from are superseded only
    /// then, and the new one begun after. A start that fails leaves them at their work.
    ///
    /// # Errors
    /// [`RuntimeError::Spawn`].
    pub fn start_held(
        client: SyncClient,
        timing: SyncTiming,
        host: Option<Arc<dyn OffloadHost>>,
    ) -> Result<Self, RuntimeError> {
        let offload = host.map(|host| (Arc::new(Offload::new()), host));
        Self::start_with(client, timing, offload, true)
    }

    /// Lets a thread started with [`Self::start_held`] go: its first tick is at once. Does
    /// nothing to a thread that was not held. Never blocks.
    pub fn begin(&self) {
        let (lock, cv) = &*self.signal;
        lock.lock().unwrap_or_else(PoisonError::into_inner).held = false;
        cv.notify_all();
    }

    fn start_with(
        client: SyncClient,
        timing: SyncTiming,
        offload: Option<(Arc<Offload>, Arc<dyn OffloadHost>)>,
        held: bool,
    ) -> Result<Self, RuntimeError> {
        let signal = Arc::new((
            Mutex::new(Signal {
                held,
                ..Signal::default()
            }),
            Condvar::new(),
        ));
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

    /// What ends this thread when another takes its place ([`Superseder`]).
    #[must_use]
    pub fn superseder(&self) -> Superseder {
        Superseder {
            signal: Arc::clone(&self.signal),
            stop: Arc::clone(&self.stop),
        }
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

    /// Stops the thread for good (sync turned off) and waits for it; on the way out it tells
    /// the other devices it is gone (best effort, bounded by the presence timeout), so they stop
    /// showing it online at once rather than after the online window, and an offloading thread
    /// hands back the summaries this device asked for ([`Offload::step_sync_stopped`]).
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

    /// The presence lock, for a test that holds it as a presence send in flight does.
    #[cfg(test)]
    pub(crate) fn beats_for_test(&self) -> Arc<Mutex<Beats>> {
        Arc::clone(&self.beats)
    }

    /// Whether a [`Self::back`] has not been taken up by the thread yet.
    #[cfg(test)]
    pub(crate) fn back_pending(&self) -> bool {
        self.signal
            .0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .back
    }
}

impl Drop for SyncThread {
    fn drop(&mut self) {
        self.stop_and_join(false);
    }
}

/// How long a stopping sync thread keeps handing back meetings its host refused to start writing
/// (a job of the meeting runs, the meeting is in use) before it ends anyway (30 minutes, a
/// judgement: longer than a re-transcription or summary job, short enough that a thread left
/// behind does not outlive the reason). A meeting still refused then is handed again by the next
/// thread's first step, as after a restart.
pub const STOPPED_HANDBACK_LIMIT: Duration = Duration::from_secs(30 * 60);

/// The last offload step of a thread that runs no more steps (T11.6c), repeated every `tick`
/// while the host refuses a hand-back, until none is refused or `limit` has passed. With
/// `honour_stop` (a stopping problem) a stop asked meanwhile ends it at once, so a removal or a
/// replacement never waits for it; turning sync off asked the stop before this began, and the
/// host retires that thread on a thread of its own.
fn hand_back_until_started(
    client: &SyncClient,
    services: &Services,
    offload: &(Arc<Offload>, Arc<dyn OffloadHost>),
    signal: &(Mutex<Signal>, Condvar),
    tick: Duration,
    limit: Duration,
    honour_stop: bool,
) {
    let (state, host) = offload;
    let deadline = Instant::now() + limit;
    let steps = super::engine::step_lock(client.device());
    loop {
        // The report is kept whatever the step ends with: what it wrote before an error or a
        // panic still reaches the window (T12.81).
        let mut step = crate::offload::StepReport::default();
        {
            // A new thread on this store owns the hand-backs from the moment it starts: seen
            // under the lock every step holds, so this thread never steps after it.
            let _stepping = steps.lock().unwrap_or_else(PoisonError::into_inner);
            if superseded(signal) {
                return;
            }
            let _ = catch_unwind(AssertUnwindSafe(|| {
                state.step_sync_stopped_into(services, &**host, &mut step)
            }));
        }
        if let Some(changes) = super::SyncChanges::after(None, Some(&step)) {
            client.tell_changes(&changes);
        }
        let refused = step.refused_here > 0;
        let now = Instant::now();
        if !refused || now >= deadline {
            return;
        }
        let (lock, cv) = signal;
        let s = lock.lock().unwrap_or_else(PoisonError::into_inner);
        let s = cv
            .wait_timeout_while(s, tick.min(deadline - now), |s| {
                !(s.superseded || (honour_stop && s.stop))
            })
            .unwrap_or_else(PoisonError::into_inner)
            .0;
        if s.superseded || (honour_stop && s.stop) {
            return;
        }
    }
}

fn outbox_len(services: &Services) -> Option<u64> {
    services.store().outbox_len().ok()
}

/// After a tick, the one-off copy of legacy ignores (T12.93, [`crate::ignore_copy`]) runs only
/// when the tick ended with no problem, its pull reached the end (`more == false`), it held no
/// row and no earlier held row waits in the store for a retry: any of those may be the edit
/// another computer made. Returns whether the copy ran (and succeeded) after this tick.
pub(crate) fn copy_after_tick<P>(
    client: &SyncClient,
    ticked: &Result<Result<(), SyncProblem>, P>,
    report: Option<&super::TickReport>,
) -> bool {
    matches!(ticked, Ok(Ok(())))
        && report.is_some_and(|r| r.pull_complete && r.held == 0)
        && client.holds_rows_for_retry() == Ok(false)
        && crate::ignore_copy::copy_once(client.services()).is_ok()
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
    // Started held: nothing is read or done until the host says so, or stops this thread (the
    // loop below then takes the stop as any other).
    {
        let (lock, cv) = signal;
        let s = lock.lock().unwrap_or_else(PoisonError::into_inner);
        drop(
            cv.wait_while(s, |s| s.held && !s.stop)
                .unwrap_or_else(PoisonError::into_inner),
        );
    }
    let services = client.services().clone();
    let steps = super::engine::step_lock(client.device());
    let mut failures: u32 = 0;
    let mut next = Instant::now();
    let mut seen = outbox_len(&services);
    let mut changed = false;
    // Presence goes out after the first tick without a problem, then every `presence`.
    let mut next_presence: Option<Instant> = None;
    loop {
        // Wait until the next tick, a poll, a nudge or a stop. Lock order: the signal lock is a
        // leaf. It is never held while another lock is waited for (the presence lock can be
        // held for as long as a presence send takes), so nothing that signals this thread
        // (a nudge, a resume, a stop, a superseding) ever waits for more than these few lines.
        let back = {
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
                // Saying gone is turning sync off (the one caller of `stop_saying_gone`). A
                // thread another has taken the place of says nothing and hands nothing back:
                // the new thread announces itself and owns the hand-backs ([`Superseder`]).
                let say_gone = s.gone && !s.superseded;
                drop(s);
                // Best effort: the other devices see this one go at once, not after the
                // online window.
                let b = beats_of(beats);
                if b.announced() && say_gone {
                    let _ = catch_unwind(AssertUnwindSafe(|| client.send_presence(true)));
                }
                drop(b);
                // Turned off: no step runs until it is turned on, so what this device asked is
                // handed back now rather than left waiting (T11.6c).
                if say_gone && let Some(pair) = offload {
                    hand_back_until_started(
                        client,
                        &services,
                        pair,
                        signal,
                        timing.tick,
                        STOPPED_HANDBACK_LIMIT,
                        false,
                    );
                }
                return;
            }
            if std::mem::take(&mut s.soon) && failures == 0 {
                next = next.min(Instant::now());
            }
            std::mem::take(&mut s.back)
        };
        // A resume: tick at once whatever the back-off, and beat after the first tick that
        // reaches the relay (the tick's success arm sets the beat due). Behind any presence
        // send in flight, with the signal lock released.
        if back && beats_of(beats).come_back() {
            next = Instant::now();
            next_presence = None;
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
            // presence lock, so a going away either stops this beat or queues behind it. A
            // summary worker's writer kind is found first, outside the lock (its locator may
            // look a name up), so a gone waiting for the lock is held up only by the send itself
            // (bounded by its request timeout), as before; the kind is at most one lookup old.
            // Only a worker's locator can panic: it then names no writer.
            let worker =
                catch_unwind(AssertUnwindSafe(|| client.worker_writer_now())).unwrap_or(Some(None));
            let mut b = beats_of(beats);
            if b.beat_due()
                && let Ok(Ok(())) = catch_unwind(AssertUnwindSafe(|| {
                    client.send_presence_with(false, worker)
                }))
            {
                b.sent_online();
            }
            drop(b);
            next_presence = Some(Instant::now() + timing.presence);
        }
        if Instant::now() < next {
            continue;
        }
        let reported = catch_unwind(AssertUnwindSafe(|| client.tick_reporting()));
        let (tick_report, ticked) = match reported {
            Ok((report, outcome)) => (Some(report), Ok(outcome)),
            Err(panic) => (None, Err(panic)),
        };
        // After the first completed pull, so an edit another computer made is already here
        // (T12.93): this device's legacy ignores go onto the speaker rows, once. Not while any
        // pulled row is held (a meeting in use, a missing parent): it may be that edit. A
        // failure or a panic is the next completed pull's to retry; it never ends sync.
        let _ = catch_unwind(AssertUnwindSafe(|| {
            copy_after_tick(client, &ticked, tick_report.as_ref())
        }));
        let mut step_report = crate::offload::StepReport::default();
        if let Some(pair) = offload {
            // A store failure or a panic here is the next step's to retry; it never ends sync.
            // A problem that stops sync gets a last step that withdraws and writes here what
            // this device asked (T11.6c), repeated while the host refuses one: no later step
            // runs until the user acts.
            if matches!(&ticked, Ok(Err(p)) if p.stops()) {
                // The tick's own changes first; the hand-back tells its own.
                if let Some(changes) = super::SyncChanges::after(tick_report.as_ref(), None) {
                    client.tell_changes(&changes);
                }
                hand_back_until_started(
                    client,
                    &services,
                    pair,
                    signal,
                    timing.tick,
                    STOPPED_HANDBACK_LIMIT,
                    true,
                );
                return;
            }
            let (state, host) = pair;
            // One step at a time on a store, and none once another thread has taken this one's
            // place (looked at under the lock, so the new thread's first step comes after this
            // thread's last).
            let _stepping = steps.lock().unwrap_or_else(PoisonError::into_inner);
            if !superseded(signal) {
                // Whether the tick may have changed a summary request here: it wrote, deleted
                // or repaired a row, or it panicked and cannot say. With nothing changed and
                // nothing to do or wait for at the last step, the step reads no request (T14.50).
                let rows_changed = tick_report.as_ref().is_none_or(super::TickReport::changed);
                // Kept whatever the step ends with, as the tick's report is (T12.81).
                let _ = catch_unwind(AssertUnwindSafe(|| {
                    state.step_after_tick_into(
                        &services,
                        &mut || client.peers().ok(),
                        &**host,
                        rows_changed,
                        &mut step_report,
                    )
                }));
            }
        }
        // Once per pass, after the step (T12.81): what the tick applied and what the step wrote
        // as an asker reach the window together, so a page never reads a request that settled
        // before the run record the step writes for it.
        if let Some(changes) = super::SyncChanges::after(tick_report.as_ref(), Some(&step_report)) {
            client.tell_changes(&changes);
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
