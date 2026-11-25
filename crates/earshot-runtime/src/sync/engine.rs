//! One sync tick (docs/sync-spec.md section 6.2): push the outbox, try held rows again, pull
//! after the cursor and apply.
//!
//! The store lock is taken only for a read, a [`Store::apply_remote`] transaction or a record
//! write, and never across a request. A pulled meeting deletion runs this device's own
//! deletion first ([`Services::delete_meeting`]), so its audio goes before its rows as a local
//! deletion's does.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use earshot_core::identity::{DeviceId, EntityId, HlcTimestamp};
use earshot_net::{AuditLog, Url};
use earshot_store::{
    ApplyRules, HoldReason, Incoming, MAX_CLOCK_AHEAD_MS, Store, StoreError, StoreTx, SyncKinds,
    SyncedRow,
};
use earshot_sync::wire::{B64, PullResponse, PulledRow, PushRequest, WireRow};
use earshot_sync::{
    ENVELOPE_FORMAT, Keys, Opened, ROW_KEY_LEN, RowKey, SealError, SealedRow, open_row, seal_row,
};

use super::client::Relay;
use super::{
    SyncProblem, SyncVault, VaultSeen, advance_cursor, check_address, cursor_record, observe_vault,
    set_sync_status, sync_settings, sync_status,
};
use crate::services::Services;

/// Outbox rows read per push request.
const PUSH_PAGE: usize = 200;
/// The most push requests in one tick, so a tick ends even when rows keep changing.
const MAX_PUSH_PAGES: usize = 50;
/// The most pull pages in one tick.
const MAX_PULL_PAGES: usize = 50;
/// Held rows read, opened and applied at a time, and the most the retry at the start of a
/// tick tries (it goes on from there next tick, so every held row gets its turn).
const MAX_RETRY: usize = 500;
/// Sealed bytes per push request, under the relay's 4 MiB request limit after base64 and JSON.
const PUSH_BYTES: usize = 2 * 1024 * 1024;

/// What one tick did: counts, and the ids of the meetings it changed here.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TickReport {
    /// Rows sent and stored by the relay.
    pub pushed: usize,
    /// Rows sent that the relay already held at the same or a later stamp.
    pub superseded: usize,
    /// Rows left queued because they could not be sealed (over the size limit, or audio): each
    /// such row once, however many push pages read it. Shown on the Sync page through
    /// [`super::SyncStatus::refused`] (T14.43).
    pub refused: usize,
    /// Queued rows this device could not read to send (a damaged voice sample row), left
    /// queued and skipped; the most found in one push page.
    pub unsendable: usize,
    /// Queued rows stamped more than [`MAX_CLOCK_AHEAD_MS`] past this device's clock (its
    /// clock was far ahead when they were written), left queued and not sent (T14.44): the
    /// relay would keep such a row over every later edit of it. A tick that finds any ends
    /// with [`SyncProblem::ClockAhead`] in the status, though it pushed the rest and pulled.
    /// Counted in the push page that reached them (rows are read oldest stamp first, so
    /// every later one is ahead too and is not read).
    pub clock_ahead: usize,
    /// The relay refused a push because a row in it is stamped too far past the relay's clock
    /// (this device's clock is wrong now, or the relay's): the rows of that push and every
    /// later one stay queued, nothing more was pushed this tick, and the tick went on to pull.
    /// It ends with [`SyncProblem::ClockAhead`] in the status, like [`Self::clock_ahead`].
    pub clock_refused: bool,
    /// Rows pulled (held rows tried again not included).
    pub pulled: usize,
    /// Rows written here.
    pub written: usize,
    /// Rows not newer than the copy here.
    pub ignored: usize,
    /// Rows this tick left held for later ([`earshot_store::HoldReason`]): each row it held
    /// because its meeting is in use, its stamp is far ahead or a newer Earshot wrote it, and
    /// every row that still waits for a parent when the tick ends (a row whose parent a later
    /// page of the same tick brought is applied in that tick and not counted, T14.42).
    pub held: usize,
    /// Held rows read from the store and opened again in this tick, each time it happened
    /// (the retry at the start, and the retry of rows waiting for a parent at the end): the
    /// work the retries cost, which stays in proportion to the rows held, not to the pages
    /// pulled.
    pub retried: usize,
    /// Rows that could not be opened or read.
    pub unreadable: usize,
    /// Meetings deleted here because they were deleted on another device.
    pub deleted_meetings: usize,
    /// Repairs made by the convergence rules: "Me" people merged, summaries replaced,
    /// withdrawals kept, and a user's line edits kept over a re-transcription (edited lines
    /// stamped again, lines rewritten under them, their speakers revived; T12.92). Rows, not
    /// words.
    pub repaired: usize,
    /// The meetings whose rows this tick wrote or deleted here (ids only;
    /// [`earshot_store::Applied::meetings`] plus the meetings deleted here).
    pub meetings: BTreeSet<EntityId>,
    /// Whether a person row was written here or "Me" people merged.
    pub people: bool,
    /// The relay's history changed (it was restored from an older copy, or replaced): this
    /// device queued every synced row again and set its cursor to the start, and the tick ended
    /// there (T14.45). The next tick sends and reads everything.
    pub vault_reset: bool,
    /// The pull reached the end of what the relay holds: its last page said `more == false`
    /// (T12.93: with no problem in the tick, no row held and none waiting for a retry, the
    /// one-off copy of legacy ignores may run, [`crate::ignore_copy`]).
    pub pull_complete: bool,
}

impl TickReport {
    /// Whether the tick changed anything here: rows written, a meeting deleted or a repair.
    #[must_use]
    pub fn changed(&self) -> bool {
        self.written > 0 || self.deleted_meetings > 0 || self.repaired > 0
    }
}

/// What one pass of the sync thread changed here, as the window hears it (T12.81): its tick and
/// the offload step after it. The ids of the meetings it touched, never anything they hold.
/// `meetings` is empty when only rows outside any meeting changed (people, folders, Dictionary
/// words), which still changes what the meeting list shows.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SyncChanges {
    /// The meetings whose rows were written or deleted here, or whose waiting for another
    /// computer the offload step changed (withdrawn, handed back, ended), in id order.
    pub meetings: Vec<EntityId>,
    /// A person changed (a rename, a merge, a "Me" repair): names in every meeting resolve
    /// through persons, so an open meeting is read again whether or not it is named.
    pub people: bool,
}

impl SyncChanges {
    /// What a tick and the offload step after it changed, merged; `None` when neither changed
    /// anything here (a tick that only pushed, or pulled nothing newer, and a step that wrote
    /// nothing as an asker).
    #[must_use]
    pub fn after(
        tick: Option<&TickReport>,
        step: Option<&crate::offload::StepReport>,
    ) -> Option<Self> {
        let ticked = tick.is_some_and(TickReport::changed);
        let stepped = step.is_some_and(|s| !s.meetings.is_empty());
        if !ticked && !stepped {
            return None;
        }
        let mut meetings: BTreeSet<EntityId> = BTreeSet::new();
        if let Some(t) = tick {
            meetings.extend(t.meetings.iter().copied());
        }
        if let Some(s) = step {
            meetings.extend(s.meetings.iter().copied());
        }
        Some(Self {
            meetings: meetings.into_iter().collect(),
            people: tick.is_some_and(|t| t.people),
        })
    }
}

/// Told once per pass of the sync thread that changed anything here
/// ([`SyncClient::with_changes`], [`SyncChanges::after`]), on the sync thread, with no store lock
/// held. It must not block: a host forwards the ids and returns. A panic in it is caught and
/// never ends sync ([`SyncClient::tell_changes`]).
pub type ChangeListener = Arc<dyn Fn(&SyncChanges) + Send + Sync>;

/// What is printed when a change listener panics: fixed words, nothing it was told.
pub const LISTENER_PANICKED: &str = "earshot-sync: the change listener panicked";

/// Another device's presence, as this device reads it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Peer {
    /// The device.
    pub device: DeviceId,
    /// Its presence; `None` when its blob cannot be opened or read.
    pub presence: Option<super::Presence>,
    /// Seconds since the relay received it, by the relay's clock.
    pub age_s: u64,
    /// Whether it counts as online ([`super::online`]).
    pub online: bool,
}

/// The writer kind a summary worker names in its presence, asked again at every presence send
/// (where its model is can change while it runs).
pub type WorkerWriter =
    std::sync::Arc<dyn Fn() -> Option<crate::offload::WriterKind> + Send + Sync>;

/// The sync client of one device: the relay at the stored address and the device's vault.
pub struct SyncClient {
    services: Services,
    vault: std::sync::Arc<dyn SyncVault>,
    relay: Relay,
    device: DeviceId,
    /// Set by the sync thread's stop: a tick ends at its next page.
    stop: Arc<AtomicBool>,
    /// Where the retry of held rows at the start of each tick is ([`HeldRetry`]).
    retry: std::sync::Mutex<HeldRetry>,
    /// A summary worker's presence ([`SyncClient::as_summary_worker`]): kind `server` and the
    /// writer it names, asked at every send; `None` for the app.
    worker: Option<WorkerWriter>,
    /// Where a summary worker's own local model runs ([`SyncClient::with_model_place`],
    /// T11.6a); sent only by a worker whose writer kind is `local` now.
    model_place: Option<super::presence::ModelPlace>,
    /// Told after every tick that changed rows here ([`SyncClient::with_changes`]).
    changes: Option<ChangeListener>,
    /// Pull pages per tick and rows per page: `MAX_PULL_PAGES` and `PULL_DEFAULT`, smaller only
    /// in tests (a tick that ends with `more == true`).
    pull_pages: usize,
    pull_limit: u32,
}

impl std::fmt::Debug for SyncClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SyncClient").finish_non_exhaustive()
    }
}

/// Where the retry of held rows at the start of each tick is. At most [`MAX_RETRY`] rows are
/// tried per tick, in row-key order from where the last tick stopped, so a store holding more
/// than that still tries each of them in turn (never the same first rows every tick).
///
/// The retry goes through the held rows in passes: a pass starts at the first key and ends
/// when a read comes back short. Rows held because their meeting is in use or a parent is
/// missing are read by every pass. A pass also reads the rows held for a newer format if no
/// pass of this client has yet (they clear only after an update), and the rows held for a far
/// clock if none has, or the last one that did began [`CLOCK_RETRY_MS`] or more ago by this
/// device's clock, or that clock now reads earlier than it did then (it was set).
#[derive(Debug, Default)]
pub(super) struct HeldRetry {
    /// The row key the next retry starts after; `None`: from the first.
    after: Option<[u8; ROW_KEY_LEN]>,
    /// The pass under way; `None` before the first and between passes.
    pass: Option<RetryPass>,
    /// Whether a pass of this client has read the rows held for a newer format.
    format_done: bool,
    /// When (this device's clock, Unix ms) the last finished pass that read the rows held for
    /// a far clock began.
    clock_at: Option<u64>,
}

/// One pass of [`HeldRetry`].
#[derive(Debug, Clone, Copy)]
struct RetryPass {
    format: bool,
    clock: bool,
    began_ms: u64,
}

/// How often rows held because their stamp is far ahead of this device's clock are tried again
/// (T14.44; before it, once per app start). An hour, a judgement: such a row clears when this
/// device's clock is put right or simply with time, the limit it waits for is 24 hours wide, and
/// a retry opens every such row, so more often buys little.
pub(super) const CLOCK_RETRY_MS: u64 = 60 * 60 * 1000;

impl HeldRetry {
    /// The reasons this tick's retry reads and the key it starts after; `now_ms` is this
    /// device's clock.
    pub(super) fn begin(
        &mut self,
        now_ms: u64,
    ) -> (&'static [HoldReason], Option<[u8; ROW_KEY_LEN]>) {
        use HoldReason::{Busy, Clock, Format, Parent};
        let pass = match self.pass {
            Some(pass) => pass,
            None => {
                let clock = self
                    .clock_at
                    .is_none_or(|at| now_ms < at || now_ms - at >= CLOCK_RETRY_MS);
                let pass = RetryPass {
                    format: !self.format_done,
                    clock,
                    began_ms: now_ms,
                };
                self.pass = Some(pass);
                self.after = None;
                pass
            }
        };
        let reasons: &'static [HoldReason] = match (pass.format, pass.clock) {
            (true, true) => &HoldReason::ALL,
            (true, false) => &[Format, Busy, Parent],
            (false, true) => &[Clock, Busy, Parent],
            (false, false) => &[Busy, Parent],
        };
        (reasons, self.after)
    }

    /// The retry read `read` rows of at most `limit`, the last with key `last`.
    pub(super) fn read(&mut self, read: usize, limit: usize, last: Option<[u8; ROW_KEY_LEN]>) {
        if read < limit {
            // The end of the held rows: the pass is over and the next retry starts a new one.
            self.after = None;
            if let Some(pass) = self.pass.take() {
                self.format_done |= pass.format;
                if pass.clock {
                    self.clock_at = Some(pass.began_ms);
                }
            }
        } else {
            self.after = last;
        }
    }
}

/// The sealed row of any synced table.
fn seal(keys: &Keys, row: &SyncedRow) -> Result<SealedRow, SealError> {
    match row {
        SyncedRow::Meeting(r) => seal_row(keys, r),
        SyncedRow::Track(r) => seal_row(keys, r),
        SyncedRow::Segment(r) => seal_row(keys, r),
        SyncedRow::Speaker(r) => seal_row(keys, r),
        SyncedRow::Note(r) => seal_row(keys, r),
        SyncedRow::Summary(r) => seal_row(keys, r),
        SyncedRow::Person(r) => seal_row(keys, r),
        SyncedRow::Voiceprint(r) => seal_row(keys, r),
        SyncedRow::Folder(r) => seal_row(keys, r),
        SyncedRow::SummaryRequest(r) => seal_row(keys, r),
        SyncedRow::DictionaryWord(r) => seal_row(keys, r),
        SyncedRow::VoiceprintSample(r) => seal_row(keys, r),
    }
}

fn wire_row(sealed: SealedRow) -> WireRow {
    WireRow {
        key: B64(sealed.key.0.to_vec()),
        wall_ms: sealed.stamp.wall_ms,
        logical: sealed.stamp.logical,
        node: B64(sealed.stamp.node.as_uuid().as_bytes().to_vec()),
        body: B64(sealed.body),
    }
}

/// A pulled row opened with `keys`: `Ok(Incoming)` to apply or hold, `Err(())` when it
/// cannot be opened or read (counted and skipped).
pub(super) fn open(
    keys: &Keys,
    seq: u64,
    key: &[u8],
    stamp: HlcTimestamp,
    body: Vec<u8>,
) -> Result<Incoming, ()> {
    let key: [u8; ROW_KEY_LEN] = key.try_into().map_err(|_| ())?;
    let (row, entity) = match open_row(keys, RowKey(key), stamp, &body).map_err(|_| ())? {
        Opened::NewerFormat(_) => (None, None),
        // A known entity whose fields this build cannot read was written by a newer Earshot
        // that did not raise the format (or is a damaged voice sample): held like a newer
        // format, never skipped past, unless its kind is switched off here.
        Opened::Row(row) => {
            let entity = row.entity.clone();
            (
                SyncedRow::decode(&row.entity, row.record).unwrap_or(None),
                Some(entity),
            )
        }
    };
    Ok(Incoming {
        key,
        seq,
        stamp,
        body,
        row,
        entity,
    })
}

fn stamp_of(row: &PulledRow) -> Option<HlcTimestamp> {
    let node: [u8; 16] = row.node.0.as_slice().try_into().ok()?;
    Some(HlcTimestamp {
        wall_ms: row.wall_ms,
        logical: row.logical,
        node: DeviceId::from_bytes(node),
    })
}

impl SyncClient {
    /// The client for the address stored in the settings.
    ///
    /// # Errors
    /// [`SyncProblem::Address`] when no usable address is stored; [`SyncProblem::Store`];
    /// [`SyncProblem::Internal`] when the HTTP client cannot be built.
    pub fn new(
        services: Services,
        vault: std::sync::Arc<dyn SyncVault>,
    ) -> Result<Self, SyncProblem> {
        let (address, device) = {
            let db = services.store();
            let settings = sync_settings(&db)?;
            let address = match settings.address {
                Some(Ok(url)) => url,
                _ => return Err(SyncProblem::Address),
            };
            (address, db.device())
        };
        Self::at(services, vault, address, device)
    }

    pub(crate) fn at(
        services: Services,
        vault: std::sync::Arc<dyn SyncVault>,
        address: Url,
        device: DeviceId,
    ) -> Result<Self, SyncProblem> {
        let address = check_address(address.as_str()).map_err(|_| SyncProblem::Address)?;
        let relay = Relay::new(address, std::sync::Arc::clone(&services.clock))?;
        Ok(Self {
            services,
            vault,
            relay,
            device,
            stop: Arc::new(AtomicBool::new(false)),
            retry: std::sync::Mutex::new(HeldRetry::default()),
            worker: None,
            model_place: None,
            changes: None,
            pull_pages: MAX_PULL_PAGES,
            pull_limit: earshot_sync::wire::PULL_DEFAULT,
        })
    }

    /// Test only: pull at most `pages` pages of `limit` rows per tick.
    #[cfg(test)]
    pub(crate) fn with_pull_pages(mut self, pages: usize, limit: u32) -> Self {
        self.pull_pages = pages;
        self.pull_limit = limit;
        self
    }

    /// Whether the store holds pulled rows that every tick tries again (held because their
    /// meeting was in use or a parent was missing): until they are applied, a completed pull
    /// has not brought everything here (T12.93, [`crate::ignore_copy`]).
    ///
    /// # Errors
    /// Store errors.
    pub fn holds_rows_for_retry(&self) -> Result<bool, SyncProblem> {
        Ok(self
            .services
            .store()
            .held_count(&[HoldReason::Busy, HoldReason::Parent])?
            > 0)
    }

    /// The client for the relay at `address` rather than the stored one: the summary worker
    /// (T11.6), whose configuration names its relay. The address is checked as a stored one is
    /// ([`super::check_address`]).
    ///
    /// # Errors
    /// [`SyncProblem::Address`] for a refused address; [`SyncProblem::Internal`] when the HTTP
    /// client cannot be built.
    pub fn for_address(
        services: Services,
        vault: std::sync::Arc<dyn SyncVault>,
        address: &str,
    ) -> Result<Self, SyncProblem> {
        let address = check_address(address).map_err(|_| SyncProblem::Address)?;
        let device = services.store().device();
        Self::at(services, vault, address, device)
    }

    /// This client's presence says it is a summary worker (T11.6, docs/sync.md section 10):
    /// kind `server`, and the kind of writer `writer` answers at each presence send (sent while
    /// it offers summaries), in place of the app's kind and its Summaries settings. Asked at
    /// every send, because the label is what tells the user where a transcript goes, and where
    /// the worker's model is can change while it runs (its container starting late).
    #[must_use]
    pub fn as_summary_worker(mut self, writer: WorkerWriter) -> Self {
        self.worker = Some(writer);
        self
    }

    /// A summary worker's presence also says where its own local model runs (T11.6a, the
    /// compose profile it was started with): sent only while this client is a summary worker
    /// ([`Self::as_summary_worker`]) and the writer kind it names at that send is `local`, so
    /// "processor" or "graphics card" never labels a remote endpoint. `None` sends nothing.
    #[must_use]
    pub const fn with_model_place(mut self, place: Option<super::presence::ModelPlace>) -> Self {
        self.model_place = place;
        self
    }

    /// After every pass of the sync thread that changed anything here (its tick, whatever the
    /// tick's outcome, and the offload step after it), `listener` hears the ids of the meetings
    /// it touched, once per pass (T12.81): the app tells its window, which reads them again.
    #[must_use]
    pub fn with_changes(mut self, listener: ChangeListener) -> Self {
        self.changes = Some(listener);
        self
    }

    /// Tells the listener ([`Self::with_changes`]) what a pass changed. A panic in it is caught
    /// and printed as [`LISTENER_PANICKED`]; returns whether it was told without one.
    pub fn tell_changes(&self, changes: &SyncChanges) -> bool {
        let Some(listener) = &self.changes else {
            return true;
        };
        let told = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| listener(changes)));
        if told.is_err() {
            eprintln!("{LISTENER_PANICKED}");
        }
        told.is_ok()
    }

    /// Every request this client made, one entry per hop.
    #[must_use]
    pub fn audit_log(&self) -> AuditLog {
        self.relay.audit_log()
    }

    /// This device's id.
    #[must_use]
    pub fn device(&self) -> DeviceId {
        self.device
    }

    pub(crate) fn services(&self) -> &Services {
        &self.services
    }

    pub(crate) fn relay(&self) -> &Relay {
        &self.relay
    }

    /// Sends this device's presence (its name, version, whether it offers summaries, whether
    /// it is recording; `gone` when it is going away).
    ///
    /// # Errors
    /// [`SyncProblem::Vault`] when not set up; the relay's problems.
    pub fn send_presence(&self, gone: bool) -> Result<(), SyncProblem> {
        // A gone blob names no writer, so a worker's locator (which may look a name up) is not
        // asked for one.
        let worker = if gone {
            self.worker.as_ref().map(|_| None)
        } else {
            self.worker_writer_now()
        };
        self.send_presence_with(gone, worker)
    }

    /// A summary worker's writer kind now (its locator asked, which may look a name up; no lock
    /// is held), or `None` for the app. The sync thread asks before it takes the presence lock
    /// and passes the answer to [`Self::send_presence_with`].
    pub(crate) fn worker_writer_now(&self) -> Option<Option<crate::offload::WriterKind>> {
        self.worker.as_ref().map(|worker| worker())
    }

    /// [`Self::send_presence`] with a worker's writer kind already found (`worker`: `None` for
    /// the app, whose writer comes from its Summaries settings).
    pub(crate) fn send_presence_with(
        &self,
        gone: bool,
        worker: Option<Option<crate::offload::WriterKind>>,
    ) -> Result<(), SyncProblem> {
        let secrets = self.vault.load()?.ok_or(SyncProblem::Vault)?;
        let keys = secrets.key.keys();
        let (settings, writer) = {
            let db = self.services.store();
            let settings = sync_settings(&db)?;
            // The writer kind travels only while this device offers to write: the asker's
            // label names it, so the user knows where the transcript goes.
            let writer = match worker {
                _ if !settings.offers_summaries => None,
                Some(worker) => worker,
                None => {
                    crate::offload::WriterKind::of_order(&db).map_err(|_| SyncProblem::Store)?
                }
            };
            (settings, writer)
        };
        let recording = !self.services.recording_meetings().is_empty();
        let model_place = self
            .model_place
            .filter(|_| self.worker.is_some() && writer == Some(crate::offload::WriterKind::Local));
        let presence = super::presence::presence_now(
            if self.worker.is_some() {
                super::presence::KIND_SERVER
            } else {
                super::presence::KIND_APP
            },
            settings.device_name,
            settings.offers_summaries,
            writer,
            model_place,
            recording,
            gone,
        );
        let blob = super::presence::blob_of(&presence, &keys, self.device)?;
        self.relay
            .put_presence(&secrets.token, &super::presence::request_of(blob))
    }

    /// Every device's presence, from the relay.
    ///
    /// # Errors
    /// [`SyncProblem::Vault`] when not set up; the relay's problems.
    pub fn peers(&self) -> Result<Vec<Peer>, SyncProblem> {
        let secrets = self.vault.load()?.ok_or(SyncProblem::Vault)?;
        let keys = secrets.key.keys();
        let answer = self.relay.presence(&secrets.token)?;
        Ok(answer
            .devices
            .into_iter()
            .filter_map(|e| {
                let id: [u8; 16] = e.device_id.0.as_slice().try_into().ok()?;
                let device = DeviceId::from_bytes(id);
                let presence = super::presence::open_blob(&keys, device, &e.blob.0);
                let gone = presence.as_ref().is_some_and(|p| p.gone);
                Some(Peer {
                    device,
                    online: presence.is_some() && super::online(e.age_s, gone),
                    presence,
                    age_s: e.age_s,
                })
            })
            .collect())
    }

    /// The flag that ends a tick at its next page.
    pub(crate) fn stop_flag(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.stop)
    }

    fn stopping(&self) -> bool {
        self.stop.load(Ordering::Relaxed)
    }

    /// One tick: push, then pull and apply. Records the outcome in `sync-status`.
    ///
    /// # Errors
    /// The first [`SyncProblem`]; what was pushed or applied before it stays done.
    pub fn tick(&self) -> Result<TickReport, SyncProblem> {
        let (report, outcome) = self.tick_reporting();
        outcome.map(|()| report)
    }

    /// [`Self::tick`], keeping what it did even when it ends on a problem (rows applied before
    /// the problem stay applied, and the window must hear of them).
    pub fn tick_reporting(&self) -> (TickReport, Result<(), SyncProblem>) {
        let mut report = TickReport::default();
        let outcome = self.tick_into(&mut report);
        {
            let mut db = self.services.store();
            // Rows still waiting for a parent when the tick ends, whenever they were pulled.
            report.held = report.held.saturating_add(
                db.held_count(&[HoldReason::Parent])
                    .ok()
                    .and_then(|n| usize::try_from(n).ok())
                    .unwrap_or(0),
            );
            let mut status = sync_status(&db).unwrap_or_default();
            status.unreadable = status
                .unreadable
                .saturating_add(u64::try_from(report.unreadable).unwrap_or(u64::MAX));
            match &outcome {
                Ok(()) => {
                    // A tick that left rows unsent for their far stamps did everything else,
                    // so it counts as a sync; the status still says why those rows wait.
                    status.problem = (report.clock_ahead > 0 || report.clock_refused)
                        .then_some(SyncProblem::ClockAhead);
                    status.last_success_unix_ms = Some(self.services.clock.now_unix_ms());
                    status.unsendable = u64::try_from(report.unsendable).unwrap_or(u64::MAX);
                    status.refused = u64::try_from(report.refused).unwrap_or(u64::MAX);
                }
                Err(p) => status.problem = Some(*p),
            }
            let _ = set_sync_status(&mut db, &status);
        }
        (report, outcome)
    }

    fn tick_into(&self, report: &mut TickReport) -> Result<(), SyncProblem> {
        let secrets = self.vault.load()?.ok_or(SyncProblem::Vault)?;
        let keys = secrets.key.keys();
        let kinds = sync_settings(&self.services.store())?.kinds();
        if kinds.dictionary {
            // Words this device had before its rows existed (an older Earshot) get rows before
            // anything is pulled, so the list rebuilt after a page never loses them.
            crate::vocabulary::reconcile(&self.services)?;
        }
        match self.push(&keys, &secrets.token, kinds, report) {
            // The relay refused the push for a far stamp: this device's clock is wrong now, or
            // the relay's is. The rows stay queued, as when this device finds such rows itself,
            // and the tick goes on to receive: a wrong clock must not cut a device off from
            // what the others send (T14.44).
            Err(SyncProblem::ClockAhead) => report.clock_refused = true,
            other => other?,
        }
        if report.vault_reset {
            // Everything is queued again and the cursor is at the start: the next tick sends
            // and reads it all (it comes soon, the queue having grown).
            return Ok(());
        }
        let mut parents = self.retry_held(&keys, kinds, report)?;
        let pulled = self.pull(&keys, &secrets.token, report, &mut parents);
        // Rows pulled before a parent this tick brought apply now, not next tick (T14.42):
        // once, after the last page, also when the pull ended on a problem part of the way.
        let retried = if parents > 0 {
            self.retry_parents(&keys, kinds, report)
        } else {
            Ok(())
        };
        pulled.and(retried)
    }

    fn push(
        &self,
        keys: &Keys,
        token: &super::DeviceToken,
        kinds: SyncKinds,
        report: &mut TickReport,
    ) -> Result<(), SyncProblem> {
        // Queued rows that cannot be read (a damaged voice sample row) stay queued and are
        // reported, never sent: one damaged row never stops every push.
        let mut unreadable = 0;
        // Rows that cannot be sealed stay queued too, so a later page of this tick reads them
        // again: each is counted once.
        let mut refused: BTreeSet<EntityId> = BTreeSet::new();
        // Rows stamped further past this device's clock than any device applies (T14.44) are
        // not sent: the relay keeps the latest stamp, so such a row would shadow every later
        // edit of it made where the clock is right. They stay queued and go once time has
        // passed them. (The relay refuses them too: this device's clock may itself be wrong.)
        let mut ahead: BTreeSet<EntityId> = BTreeSet::new();
        let latest_ms = self
            .services
            .clock
            .now_unix_ms()
            .saturating_add(MAX_CLOCK_AHEAD_MS);
        let result = (|| {
            for _ in 0..MAX_PUSH_PAGES {
                if self.stopping() {
                    return Ok(());
                }
                let recording = self.services.recording_meetings();
                let page = self
                    .services
                    .store()
                    .outbox_page(PUSH_PAGE, &recording, kinds)?;
                unreadable = unreadable.max(page.unreadable);
                let mut rows = page.rows;
                if rows.is_empty() {
                    return Ok(());
                }
                let full = rows.len() + page.unreadable >= PUSH_PAGE;
                // A recording that started while the page was read: its rows wait too.
                let now_recording = self.services.recording_meetings();
                rows.retain(|r| r.meeting().is_none_or(|m| !now_recording.contains(&m)));
                let mut sent: Vec<(&'static str, EntityId, HlcTimestamp)> = Vec::new();
                let mut batch = Vec::new();
                let mut bytes = 0;
                for row in &rows {
                    if row.meta().updated_at.wall_ms > latest_ms {
                        ahead.insert(row.id());
                        continue;
                    }
                    let sealed = match seal(keys, row) {
                        Ok(s) => s,
                        Err(SealError::Refused(_)) => {
                            refused.insert(row.id());
                            continue;
                        }
                        Err(_) => return Err(SyncProblem::Internal),
                    };
                    if bytes + sealed.body.len() > PUSH_BYTES && !batch.is_empty() {
                        self.send(token, std::mem::take(&mut batch), &mut sent, report)?;
                        if report.vault_reset {
                            return Ok(());
                        }
                        bytes = 0;
                    }
                    bytes += sealed.body.len();
                    batch.push((row.table(), row.id(), wire_row(sealed)));
                }
                if !batch.is_empty() {
                    self.send(token, batch, &mut sent, report)?;
                }
                if report.vault_reset {
                    return Ok(());
                }
                // A page is read oldest stamp first, so every row after one that is too far
                // ahead is too far ahead as well: nothing more to send this tick.
                if sent.is_empty() || !full || !ahead.is_empty() {
                    return Ok(());
                }
            }
            Ok(())
        })();
        report.unsendable = unreadable;
        report.refused = refused.len();
        report.clock_ahead = ahead.len();
        result
    }

    /// Sends one request and marks its rows sent (a superseded row too: the newer copy comes
    /// back in the pull).
    fn send(
        &self,
        token: &super::DeviceToken,
        batch: Vec<(&'static str, EntityId, WireRow)>,
        sent: &mut Vec<(&'static str, EntityId, HlcTimestamp)>,
        report: &mut TickReport,
    ) -> Result<(), SyncProblem> {
        let mut marks = Vec::with_capacity(batch.len());
        let mut rows = Vec::with_capacity(batch.len());
        for (table, id, row) in batch {
            let node: [u8; 16] = row
                .node
                .0
                .as_slice()
                .try_into()
                .map_err(|_| SyncProblem::Internal)?;
            marks.push((
                table,
                id,
                HlcTimestamp {
                    wall_ms: row.wall_ms,
                    logical: row.logical,
                    node: DeviceId::from_bytes(node),
                },
            ));
            rows.push(row);
        }
        let keys: BTreeSet<Vec<u8>> = rows.iter().map(|r| r.key.0.clone()).collect();
        // What this device has seen of the vault goes with the push (T14.45): a relay that
        // holds less was restored from an older copy, and says so in its answer.
        let seen = cursor_record(&self.services.store())?;
        let resp = self.relay.push(
            token,
            &PushRequest {
                format: ENVELOPE_FORMAT,
                rows,
                seen_seq: (seen.seen() > 0).then_some(seen.seen()),
                epoch: seen.epoch,
            },
        )?;
        // Every row sent is stored or superseded; any other answer is not a relay's, and the
        // rows stay queued.
        let accepted = usize::try_from(resp.accepted).unwrap_or(usize::MAX);
        if accepted.checked_add(resp.superseded.len()) != Some(marks.len())
            || resp.superseded.iter().any(|k| !keys.contains(&k.0))
        {
            return Err(SyncProblem::Server);
        }
        report.superseded += resp.superseded.len();
        report.pushed += accepted;
        self.services.store().outbox_sent(&marks)?;
        sent.extend(marks);
        // The relay's history, as this answer shows it. A relay from before T14.45 names no
        // epoch and no number from before the request; its highest number after storing the
        // rows it accepted gives that one.
        let before = resp
            .max_seq
            .unwrap_or_else(|| resp.seq.saturating_sub(u64::from(resp.accepted)));
        if observe_vault(
            &self.services,
            VaultSeen {
                epoch: resp.epoch.as_ref().map(|e| e.0.as_slice()),
                before: Some(before),
                now: Some(resp.seq),
            },
        )? {
            report.vault_reset = true;
        }
        Ok(())
    }

    /// Opens held rows again and applies them as one page, at most [`MAX_RETRY`] a tick, going
    /// on next tick from where this one stopped ([`HeldRetry`]): rows held because their
    /// meeting was in use or a parent was missing, every tick; rows held for a newer format,
    /// once per client (they clear only after an update); rows held for a clock far ahead,
    /// once per client and then every hour (they clear with time, or when this device's clock
    /// is put right). The rows are selected by reason in SQL, never all read into memory. A
    /// held row that no longer opens (sealed under a vault this device left) is not retried.
    ///
    /// Returns how many rows other rows belong to it wrote for the first time (a track whose
    /// meeting came meanwhile): the tick's last step tries the rows waiting for them.
    fn retry_held(
        &self,
        keys: &Keys,
        kinds: SyncKinds,
        report: &mut TickReport,
    ) -> Result<usize, SyncProblem> {
        let now_ms = self.services.clock.now_unix_ms();
        let (reasons, after) = self.retry_state().begin(now_ms);
        let held = self
            .services
            .store()
            .held_page(reasons, after.as_ref(), MAX_RETRY)?;
        self.retry_state()
            .read(held.len(), MAX_RETRY, held.last().map(|h| h.key));
        self.apply_held(keys, held, kinds, report)
    }

    fn retry_state(&self) -> std::sync::MutexGuard<'_, HeldRetry> {
        self.retry
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Opens `held` and applies it as one page in the order it was pulled; returns how many
    /// rows other rows belong to it wrote for the first time.
    fn apply_held(
        &self,
        keys: &Keys,
        held: Vec<earshot_store::Held>,
        kinds: SyncKinds,
        report: &mut TickReport,
    ) -> Result<usize, SyncProblem> {
        report.retried += held.len();
        let mut page: Vec<Incoming> = held
            .into_iter()
            .filter_map(|h| open(keys, h.seq, &h.key, h.stamp, h.body).ok())
            .collect();
        page.sort_by_key(|inc| inc.seq);
        self.apply(&page, kinds, report)
    }

    /// Applies again, in this tick, every row held because a row it belongs to had not arrived
    /// (T14.42): called once, after the tick's last page, when the tick wrote such a row for
    /// the first time (a meeting, a track, a speaker). A pass reads every row held for a
    /// parent, [`MAX_RETRY`] at a time; passes repeat until one frees no further parent (a line
    /// waits for its track, which waits for its meeting), so a held row is opened at most once
    /// per level it waits on, whatever the number of pages: the work is in proportion to the
    /// rows held. (Run after every page that brought a parent, as it first was, it opened every
    /// held row once per such page.) The held table does not record which row a held row waits
    /// for, so the rows to try cannot be picked out in SQL. Each pass that goes on wrote a held
    /// row, so the passes end. Without this a new computer, which pulls most rows before the
    /// meeting row they belong to (the relay files a row at the end each time it changes),
    /// filled in a tick at a time.
    fn retry_parents(
        &self,
        keys: &Keys,
        kinds: SyncKinds,
        report: &mut TickReport,
    ) -> Result<(), SyncProblem> {
        loop {
            let mut after: Option<[u8; ROW_KEY_LEN]> = None;
            let mut freed = 0;
            loop {
                if self.stopping() {
                    return Ok(());
                }
                let held = self.services.store().held_page(
                    &[HoldReason::Parent],
                    after.as_ref(),
                    MAX_RETRY,
                )?;
                let Some(last) = held.last() else { break };
                after = Some(last.key);
                let full = held.len() >= MAX_RETRY;
                freed += self.apply_held(keys, held, kinds, report)?;
                if !full {
                    break;
                }
            }
            if freed == 0 {
                return Ok(());
            }
        }
    }

    /// Pulls pages after the cursor and applies them. `parents` gains the rows other rows
    /// belong to that a page wrote for the first time.
    fn pull(
        &self,
        keys: &Keys,
        token: &super::DeviceToken,
        report: &mut TickReport,
        parents: &mut usize,
    ) -> Result<(), SyncProblem> {
        for _ in 0..self.pull_pages {
            if self.stopping() {
                return Ok(());
            }
            // The cursor and the kind switches are read together: turning a switch on resets
            // the cursor, and the page is applied under the switches it was pulled for.
            let (cursor, kinds) = {
                let db = self.services.store();
                (cursor_record(&db)?, sync_settings(&db)?.kinds())
            };
            let after = cursor.pull_seq;
            let PullResponse {
                rows,
                more,
                vault_format,
                epoch,
                max_seq,
            } = self.relay.pull(token, after, self.pull_limit, &cursor)?;
            if vault_format > ENVELOPE_FORMAT {
                return Err(SyncProblem::UpdateNeeded);
            }
            let last = rows.iter().map(|r| r.seq).max();
            // The relay's history, as this answer shows it (T14.45). If it changed, this
            // page was read from a cursor that means nothing in it: nothing of the page is
            // applied, and the next tick reads the vault from the start.
            if observe_vault(
                &self.services,
                VaultSeen {
                    epoch: epoch.as_ref().map(|e| e.0.as_slice()),
                    before: max_seq,
                    now: max_seq.max(last),
                },
            )? {
                report.vault_reset = true;
                return Ok(());
            }
            report.pulled += rows.len();
            let mut page = Vec::with_capacity(rows.len());
            for r in rows {
                let opened = stamp_of(&r)
                    .ok_or(())
                    .and_then(|stamp| open(keys, r.seq, &r.key.0, stamp, r.body.0));
                match opened {
                    Ok(inc) => page.push(inc),
                    Err(()) => report.unreadable += 1,
                }
            }
            *parents += self.apply(&page, kinds, report)?;
            if let Some(last) = last
                && last > after
                && !advance_cursor(&mut self.services.store(), after, last)?
            {
                // The cursor was reset meanwhile (a switch turned on): read from there next tick.
                return Ok(());
            }
            if !more {
                report.pull_complete = true;
                return Ok(());
            }
            if last.is_none() {
                return Ok(());
            }
        }
        Ok(())
    }

    /// Runs this device's deletion of every meeting the page deletes, then applies the page.
    /// Returns how many rows other rows belong to the page wrote for the first time
    /// ([`earshot_store::Applied::parents_written`]).
    fn apply(
        &self,
        page: &[Incoming],
        kinds: SyncKinds,
        report: &mut TickReport,
    ) -> Result<usize, SyncProblem> {
        if page.is_empty() {
            return Ok(0);
        }
        for meeting in self.meetings_deleted_elsewhere(page)? {
            match self.services.delete_meeting(meeting) {
                Ok(_) => {
                    report.deleted_meetings += 1;
                    report.meetings.insert(meeting);
                }
                // In use here: the apply holds its rows until it is free.
                Err(crate::RuntimeError::MeetingBusy(_)) => {}
                Err(_) => return Err(SyncProblem::Store),
            }
        }
        let leased: BTreeSet<EntityId> = self.services.leased_meetings();
        let is_leased = |m: EntityId| leased.contains(&m);
        let clock = std::sync::Arc::clone(&self.services.clock);
        let mut db = self.services.store();
        // Speaker rows' names and each meeting's current summary before the page, so a name
        // the summary used is judged here as on the computer that changed it, and a summary
        // the page brings in is never judged (T12.82, `crate::summary_names`).
        let mut speakers: std::collections::BTreeMap<EntityId, BTreeSet<EntityId>> =
            std::collections::BTreeMap::new();
        for inc in page {
            if let Some(SyncedRow::Speaker(s)) = &inc.row {
                speakers.entry(s.meeting_id).or_default().insert(s.id);
            }
        }
        let names_before = crate::summary_names::names_before(&**db, &speakers)?;
        // With Dictionary sync on, `settings/vocabulary` is rebuilt from the live word rows in
        // the page's own transaction, so the list never differs from the rows it came from.
        let after = |tx: &mut StoreTx<'_>| -> Result<(), StoreError> {
            crate::summary_names::names_changed_tx(tx, &names_before)?;
            if kinds.dictionary {
                crate::vocabulary::rebuild_tx(tx, crate::vocabulary::OverRefused::Keep)?;
            }
            Ok(())
        };
        let rules = ApplyRules {
            kinds,
            leased: &is_leased,
            now_ms: self.services.clock.now_unix_ms(),
            after: Some(&after),
        };
        let (store, hlc) = db.parts();
        let applied = store.apply_remote(page, &rules, hlc, &*clock)?;
        if applied.biometrics_forgotten > 0 {
            // A Busy here means the forgetting is committed; the next checkpoint erases it.
            let _ = store.flush_erasures();
        }
        report.written += applied.written;
        report.ignored += applied.ignored + applied.skipped;
        // Rows held for a parent are counted once, from the store, when the tick ends: a later
        // page of this tick may bring the parent.
        report.held += HoldReason::ALL
            .iter()
            .zip(applied.held)
            .filter(|(reason, _)| **reason != HoldReason::Parent)
            .map(|(_, n)| n)
            .sum::<usize>();
        report.repaired += applied.mes_merged
            + applied.summaries_replaced
            + applied.withdrawals_kept
            + applied.edited_lines_kept
            + applied.lines_rewritten_under_edits
            + applied.speakers_revived;
        report.meetings.extend(applied.meetings);
        report.people |= applied.people;
        Ok(applied.parents_written)
    }

    /// Meetings this page deletes that are live here, whatever the stamps: a meeting row
    /// edited here after the deletion would otherwise stay live over children the deletion
    /// cleared. The deletion here is stamped now, so it wins everywhere. A tombstone stamped
    /// further ahead than [`MAX_CLOCK_AHEAD_MS`] deletes nothing (its row is held, as any).
    fn meetings_deleted_elsewhere(&self, page: &[Incoming]) -> Result<Vec<EntityId>, SyncProblem> {
        use earshot_core::model::MeetingRepo;
        let now = self.services.clock.now_unix_ms();
        let db = self.services.store();
        let store: &Store = &db;
        let mut out = Vec::new();
        for inc in page {
            if let Some(SyncedRow::Meeting(m)) = &inc.row
                && m.meta.deleted
                && let Some(local) = store.meeting(m.id)?
                && !local.meta.deleted
                && m.meta.updated_at.wall_ms <= now.saturating_add(MAX_CLOCK_AHEAD_MS)
            {
                out.push(m.id);
            }
        }
        Ok(out)
    }
}
