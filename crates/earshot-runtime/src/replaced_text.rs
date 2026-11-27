//! The one-off sweep of replaced transcript text (T14.47, docs/sync-spec.md 15.6).
//!
//! Every meeting stored before this rule holds its whole draft with text, and each
//! re-transcription left its replaced lines as tombstones with their text. [`sweep`] clears them
//! once per store through the one function, `Store::clear_replaced_text`: P1 (tombstones that
//! hold text) in every meeting, P2 (drafts of tracks whose final lines are shown) only in the
//! meetings this device owns. It runs on its own thread, in the app and in the worker's run.
//!
//! - **The record.** Device record `replaced-text-swept` ([`SWEPT_KEY`],
//!   `DeviceRecord::ReplacedTextSwept`), `encode_versioned` at version 1, payload
//!   `{"done": true}`, written only by the sweep, in its own transaction, at the end of a pass
//!   with nothing left, nothing skipped and nothing failed. Absent means the sweep is still to
//!   run; an unreadable value means the same, is reported as a count (never its bytes) and is
//!   replaced when the sweep ends.
//! - **Start.** With no vault or sync off: at once. With sync running: after the first tick
//!   that completed a pull, or [`START_CAP`] after start, whichever is first.
//! - **Work.** Meetings in an order of this device's own (a hash of the device id and the
//!   meeting id), at most [`BATCH`] rows a transaction, then `flush_erasures`, then a pause
//!   ([`PAUSE`]) after a transaction that wrote anything. Nothing is written while any meeting
//!   records; a meeting leased here is skipped and tried again every [`LEASED_RETRY`] after the
//!   rest. No lease and no `WorkPass`: no job reads the rows it writes for their text.
//! - **Giving way to sync.** While the sync thread runs and its last tick succeeded, the sweep
//!   waits while more than [`OUTBOX_WAIT`] rows wait to send (counted as the push counts them)
//!   and while the last tick had `text_emptied > 0`.
//! - **End.** A meeting that fails with a store error is counted and left; a pass with a failure
//!   does not write the record. Stopping anywhere is safe.
//!
//! The module needs no engine (the worker builds the runtime without `engines`).

use std::time::Duration;

use earshot_core::identity::{DeviceId, EntityId};
use earshot_store::{SyncKinds, encode_versioned, split_version};

use crate::local::DeviceRecord;
use crate::{RuntimeError, Services};

/// `local_state` key of the device record that says the sweep ran to its end.
pub const SWEPT_KEY: &str = "replaced-text-swept";

/// The record's version.
pub const SWEPT_VERSION: u32 = 1;

/// Rows cleared in one transaction.
pub const BATCH: usize = 500;

/// The pause after a transaction that wrote anything (a judgement).
pub const PAUSE: Duration = Duration::from_secs(2);

/// How long after its start the sweep waits for a completed pull before it starts anyway (a
/// judgement).
pub const START_CAP: Duration = Duration::from_secs(10 * 60);

/// How often a meeting leased here is tried again.
pub const LEASED_RETRY: Duration = Duration::from_secs(60);

/// The sweep waits while more rows than this wait to send.
pub const OUTBOX_WAIT: u64 = 5_000;

/// How often the sweep looks again while it waits.
pub const WAIT_STEP: Duration = Duration::from_secs(1);

/// The record's bytes: `encode_versioned` at [`SWEPT_VERSION`] of `{"done": true}`.
#[must_use]
pub fn record_bytes() -> Vec<u8> {
    #[derive(serde::Serialize)]
    struct Done {
        done: bool,
    }
    // A struct of one bool always serialises.
    encode_versioned(SWEPT_VERSION, &Done { done: true }).unwrap_or_default()
}

/// What the record says.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Swept {
    /// No record: the sweep is still to run.
    Absent,
    /// The sweep ran to its end.
    Done,
    /// A value this build does not read: the sweep runs again and replaces it.
    Unreadable,
}

/// What the store's record says.
///
/// # Errors
/// Store errors.
pub fn swept(services: &Services) -> Result<Swept, RuntimeError> {
    let Some(bytes) = services.store().local_value(SWEPT_KEY)? else {
        return Ok(Swept::Absent);
    };
    #[derive(serde::Deserialize)]
    struct Done {
        done: bool,
    }
    Ok(match split_version(&bytes) {
        Some((SWEPT_VERSION, json)) => match serde_json::from_slice::<Done>(json) {
            Ok(Done { done: true }) => Swept::Done,
            _ => Swept::Unreadable,
        },
        _ => Swept::Unreadable,
    })
}

/// What the sweep reads of this store's sync ([`crate::sync::tick_seen`] in production).
pub trait SweepSync {
    /// What the sync of this store has done so far.
    fn seen(&self) -> crate::sync::TickSeen;
    /// How many rows wait to send, counted as the push counts them.
    ///
    /// # Errors
    /// Store errors.
    fn waiting_to_send(&self) -> Result<u64, RuntimeError>;
}

/// The production [`SweepSync`] of `services`' store.
pub struct StoreSync {
    services: Services,
    device: DeviceId,
}

impl StoreSync {
    /// The sync of `services`' store.
    #[must_use]
    pub fn of(services: &Services) -> Self {
        let device = services.store().device();
        Self {
            services: services.clone(),
            device,
        }
    }
}

impl SweepSync for StoreSync {
    fn seen(&self) -> crate::sync::TickSeen {
        let mut seen = crate::sync::tick_seen(self.device);
        // At start the sync thread may not have started yet: sync switched on with an address
        // counts as running for the start gate (a vault that never answers waits for the cap).
        if !seen.running {
            let db = self.services.store();
            seen.running = crate::sync::sync_settings(&db)
                .is_ok_and(|s| s.on && matches!(s.address, Some(Ok(_))));
        }
        seen
    }

    fn waiting_to_send(&self) -> Result<u64, RuntimeError> {
        let recording = self.services.recording_meetings();
        let db = self.services.store();
        let kinds = crate::sync::sync_settings(&db)
            .map(|s| s.kinds())
            .unwrap_or(SyncKinds::ALL);
        Ok(db.outbox_len_except(&recording, kinds)?)
    }
}

/// The sweep's clock and pause, injected (tests run without waiting).
pub trait SweepWait {
    /// Time since the sweep started.
    fn elapsed(&self) -> Duration;
    /// Waits `d`; `false` when the sweep is to stop.
    fn pause(&self, d: Duration) -> bool;
}

/// The production [`SweepWait`]: the monotonic clock and a sleep, stopped by a flag.
pub struct RealWait {
    started: std::time::Instant,
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl RealWait {
    /// Starts now; `stop` set ends the sweep at its next pause.
    #[must_use]
    pub fn new(stop: std::sync::Arc<std::sync::atomic::AtomicBool>) -> Self {
        Self {
            started: std::time::Instant::now(),
            stop,
        }
    }
}

impl SweepWait for RealWait {
    fn elapsed(&self) -> Duration {
        self.started.elapsed()
    }

    fn pause(&self, d: Duration) -> bool {
        let until = std::time::Instant::now() + d;
        while std::time::Instant::now() < until {
            if self.stop.load(std::sync::atomic::Ordering::Relaxed) {
                return false;
            }
            std::thread::sleep(Duration::from_millis(100).min(d));
        }
        !self.stop.load(std::sync::atomic::Ordering::Relaxed)
    }
}

/// What [`sweep`] did. Counts only.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SweepReport {
    /// Rows written (cleared, or erased in a meeting deleted here).
    pub rows: usize,
    /// Meetings looked at.
    pub meetings: usize,
    /// Meetings that failed with a store error (left as they were).
    pub failed: usize,
    /// Whether an unreadable record was found (and, if the sweep ended, replaced).
    pub unreadable_record: bool,
    /// Whether the record was written: the sweep ran to its end.
    pub done: bool,
    /// Whether it stopped before its end.
    pub stopped: bool,
}

/// The order of the sweep: a stable hash (FNV-1a) of this device's id and the meeting's id.
fn order_key(device: DeviceId, meeting: EntityId) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in device
        .as_uuid()
        .as_bytes()
        .iter()
        .chain(meeting.as_uuid().as_bytes())
    {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// Whether the sweep gives way to sync now (module docs).
fn sync_busy(sync: &dyn SweepSync) -> Result<bool, RuntimeError> {
    let seen = sync.seen();
    if !seen.running || seen.last_ok != Some(true) {
        return Ok(false);
    }
    Ok(seen.text_emptied > 0 || sync.waiting_to_send()? > OUTBOX_WAIT)
}

/// Clears what one meeting holds of replaced text, batch by batch. `Ok(None)`: stopped.
fn sweep_meeting(
    services: &Services,
    meeting: EntityId,
    sync: &dyn SweepSync,
    wait: &dyn SweepWait,
) -> Result<Option<usize>, RuntimeError> {
    let clock = std::sync::Arc::clone(&services.clock);
    let mut rows = 0;
    loop {
        // Nothing is written while any meeting records.
        while !services.recording_meetings().is_empty() || sync_busy(sync)? {
            if !wait.pause(WAIT_STEP) {
                return Ok(None);
            }
        }
        let done = {
            let mut db = services.store();
            let (store, hlc) = db.parts();
            let done = store.clear_replaced_text(meeting, BATCH, hlc, &*clock)?;
            if done.rows > 0 {
                let _ = store.flush_erasures();
            }
            done
        };
        rows += done.rows;
        if done.rows > 0 && !wait.pause(PAUSE) {
            return Ok(None);
        }
        if !done.more {
            return Ok(Some(rows));
        }
    }
}

/// The sweep (module docs). Returns at once when the record says it ran.
///
/// # Errors
/// Store errors reading the meetings or writing the record; a meeting's own failure is counted.
pub fn sweep(
    services: &Services,
    sync: &dyn SweepSync,
    wait: &dyn SweepWait,
) -> Result<SweepReport, RuntimeError> {
    let mut report = SweepReport::default();
    match swept(services)? {
        Swept::Done => {
            report.done = true;
            return Ok(report);
        }
        Swept::Unreadable => report.unreadable_record = true,
        Swept::Absent => {}
    }
    // Start: at once without a running sync; with one, after its first completed pull or at
    // the cap.
    loop {
        let seen = sync.seen();
        if !seen.running || seen.pull_completed || wait.elapsed() >= START_CAP {
            break;
        }
        if !wait.pause(WAIT_STEP) {
            report.stopped = true;
            return Ok(report);
        }
    }
    let device = services.store().device();
    let mut meetings = services.store().meeting_ids()?;
    meetings.sort_by_key(|m| (order_key(device, *m), *m));
    let mut later: Vec<EntityId> = Vec::new();
    for meeting in meetings {
        if services.leased_meetings().contains(&meeting) {
            later.push(meeting);
            continue;
        }
        report.meetings += 1;
        match sweep_meeting(services, meeting, sync, wait) {
            Ok(Some(rows)) => report.rows += rows,
            Ok(None) => {
                report.stopped = true;
                return Ok(report);
            }
            Err(_) => report.failed += 1,
        }
    }
    while !later.is_empty() {
        if !wait.pause(LEASED_RETRY) {
            report.stopped = true;
            return Ok(report);
        }
        let leased = services.leased_meetings();
        let mut still = Vec::new();
        for meeting in later {
            if leased.contains(&meeting) {
                still.push(meeting);
                continue;
            }
            report.meetings += 1;
            match sweep_meeting(services, meeting, sync, wait) {
                Ok(Some(rows)) => report.rows += rows,
                Ok(None) => {
                    report.stopped = true;
                    return Ok(report);
                }
                Err(_) => report.failed += 1,
            }
        }
        later = still;
    }
    if report.failed == 0 {
        services.store().transaction(|tx| {
            crate::local::set_tx(tx, &DeviceRecord::ReplacedTextSwept.key(), &record_bytes())
        })?;
        report.done = true;
    }
    Ok(report)
}

/// Starts [`sweep`] on its own thread (`earshot-replaced-text`) with the production sync view and
/// wait; `stop` set ends it at its next pause. A failure or a panic is the next start's to retry.
///
/// # Errors
/// [`RuntimeError::Spawn`] when the thread cannot start.
pub fn spawn(
    services: &Services,
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
) -> Result<std::thread::JoinHandle<Option<SweepReport>>, RuntimeError> {
    let services = services.clone();
    std::thread::Builder::new()
        .name("earshot-replaced-text".into())
        .spawn(move || {
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let sync = StoreSync::of(&services);
                sweep(&services, &sync, &RealWait::new(stop)).ok()
            }))
            .ok()
            .flatten()
        })
        .map_err(RuntimeError::Spawn)
}

#[cfg(test)]
#[path = "replaced_text_tests.rs"]
mod tests;
