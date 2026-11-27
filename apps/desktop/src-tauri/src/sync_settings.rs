//! The Sync page's commands (T11.3a; docs/sync.md section 7, docs/sync-spec.md sections 6.3
//! and 9) and the app's one sync thread ([`SyncHost`]).
//!
//! No response carries the vault key, the device token, the setup secret, or a pairing code
//! once it is used: the page gets counts, times, fixed copy and a shown code it asked for.
//! Errors reach the window only as fixed copy chosen by the problem's word
//! ([`problem_copy`], pinned to `fixtures/ui/sync_copy_cases.json`, which the UI mock reads).
//!
//! The thread is started at app start when sync is on and set up, after setting up or joining,
//! and when turned on; stopped when turned off (on a thread of its own, so the page does not
//! wait for a request in flight) and before this computer is removed. On quit it is not
//! joined: the process exits under it, which is safe because every write of a tick is one
//! store transaction and a tick replayed after a restart changes nothing.

use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use earshot_runtime::Services;
use earshot_runtime::offload::OffloadHost;
use earshot_runtime::sync::{
    AddressRefused, ChangeListener, Superseder, SyncChanges, SyncClient, SyncOverview, SyncProblem,
    SyncThread, SyncTiming, SyncVault, check_address,
};
use serde::Serialize;
use specta::Type;

use crate::ipc::{AppState, IpcError, blocking};

/// The IPC error kind of a sync problem.
pub const SYNC_ERROR_KIND: &str = "sync";

/// Shown when the last sync left one queued item unsent because it cannot be sealed (T14.43:
/// over the size limit, or holding an audio file's signature); names no content
/// (`refused_note.one` in `fixtures/ui/sync_copy_cases.json`).
pub const REFUSED_NOTE_ONE: &str = "1 item on this computer cannot be synced because it is too large or looks like audio, so it was not sent. Your other computers do not get it. Everything else syncs as usual.";

/// [`REFUSED_NOTE_ONE`] for more than one item; `{count}` is the number (`refused_note.many`).
pub const REFUSED_NOTE_MANY: &str = "{count} items on this computer cannot be synced because they are too large or look like audio, so they were not sent. Your other computers do not get them. Everything else syncs as usual.";

/// The note for `count` queued items the last sync could not seal ([`REFUSED_NOTE_ONE`],
/// [`REFUSED_NOTE_MANY`]); `None` for none. Fixed copy and a count, never what an item holds.
#[must_use]
pub fn refused_note(count: u64) -> Option<String> {
    match count {
        0 => None,
        1 => Some(REFUSED_NOTE_ONE.to_owned()),
        n => Some(REFUSED_NOTE_MANY.replace("{count}", &n.to_string())),
    }
}

/// Fixed copy for each problem (the words in `fixtures/ui/sync_copy_cases.json`).
#[must_use]
pub fn problem_copy(problem: SyncProblem) -> &'static str {
    match problem {
        SyncProblem::Unreachable => "The sync server could not be reached. Earshot will try again.",
        SyncProblem::Server => {
            "The sync server gave an answer Earshot does not understand. Earshot will try again."
        }
        SyncProblem::UnknownDevice => {
            "This computer was removed from sync. Remove it here, then join again with a pairing code."
        }
        SyncProblem::KeyMismatch => {
            "This sync server already belongs to other devices. Join it with a pairing code from one of them."
        }
        SyncProblem::UpdateNeeded => {
            "Another device uses a newer Earshot. Update Earshot on this computer to keep syncing."
        }
        SyncProblem::SlowDown => "Too many tries. Wait a minute, then try again.",
        SyncProblem::TooLarge => {
            "Something was too large for the sync server. Earshot will try again."
        }
        SyncProblem::ClockAhead => {
            "Some changes on this computer are dated more than a day in the future, so they were not sent. Check the date and time on this computer and on the sync server. Earshot will try again."
        }
        SyncProblem::BadSecret => "That setup secret is not the server's.",
        SyncProblem::NoSlot => {
            "That pairing code is not one the server holds. It may be used or out of date: show a new one on the other device."
        }
        SyncProblem::BadCode => "That is not a pairing code. Check it and type it again.",
        SyncProblem::DeviceExists => {
            "This computer is already registered with the server. Set it up with the server's setup secret instead."
        }
        SyncProblem::Address => {
            "The sync server's address is not usable. Remove this computer from sync and set it up again."
        }
        SyncProblem::Vault => {
            "This computer's sync key is missing. Remove this computer from sync and set it up again."
        }
        SyncProblem::Store => {
            "Earshot could not read or save its own data for sync. It will try again."
        }
        SyncProblem::Internal => "Something went wrong inside sync. Restart Earshot to try again.",
    }
}

/// Fixed copy for a button press that failed (the words in the table's `command_problems`).
/// [`problem_copy`] is the status line's, which may say Earshot will try again: nothing retries
/// a button press, so these say what to do instead.
#[must_use]
pub fn command_copy(problem: SyncProblem) -> &'static str {
    match problem {
        SyncProblem::Unreachable => {
            "The sync server could not be reached. Check the address and try again."
        }
        SyncProblem::Server => {
            "The sync server gave an answer Earshot does not understand. Check the address and try again."
        }
        SyncProblem::TooLarge => "Something was too large for the sync server. Try again.",
        SyncProblem::ClockAhead => {
            "Some changes on this computer are dated more than a day in the future, so they were not sent. Check the date and time on this computer and on the sync server, then try again."
        }
        SyncProblem::Store => "Earshot could not read or save its own data for sync. Try again.",
        other => problem_copy(other),
    }
}

fn sync_error(problem: SyncProblem) -> IpcError {
    IpcError::new(SYNC_ERROR_KIND, command_copy(problem))
}

fn address_error(refused: AddressRefused) -> IpcError {
    IpcError::invalid(refused.copy())
}

/// Printed when a start-up step before the sync thread (the ignore copy or the removal repair)
/// panicked; fixed words, and the steps after it still run.
pub const START_STEP_PANICKED: &str = "earshot-sync: a start-up step panicked";

/// Tests: the bit of [`SyncHost::panic_in_start_steps_for_test`] that makes the ignore copy panic.
#[cfg(test)]
pub const START_COPY_PANICS: u8 = 1;

/// Tests: the bit of [`SyncHost::panic_in_start_steps_for_test`] that makes the repair panic.
#[cfg(test)]
pub const START_REPAIR_PANICS: u8 = 2;

/// Runs one start-up step, catching a panic in it (printed as [`START_STEP_PANICKED`]) so the
/// steps after it still run; `None` when it panicked.
fn start_step<T>(step: impl FnOnce() -> T) -> Option<T> {
    let ran = std::panic::catch_unwind(std::panic::AssertUnwindSafe(step));
    if ran.is_err() {
        eprintln!("{START_STEP_PANICKED}");
    }
    ran.ok()
}

/// The app's sync thread and the vault it reads.
pub struct SyncHost {
    vault: Arc<dyn SyncVault>,
    thread: Mutex<Option<SyncThread>>,
    /// Held across deciding and replacing the thread, and across the whole of a removal, so a
    /// start cannot slip in between a removal's stop and its deletions (lock order: this, then
    /// the slot, then the store).
    lifecycle: Mutex<()>,
    timing: SyncTiming,
    /// Who does summary offload's work (T11.3db): set once the app's meeting driver and job
    /// runner exist; a thread started without it runs no offload step.
    offload: Mutex<Option<Arc<dyn OffloadHost>>>,
    /// Who hears what a tick changed here (T12.81): the window, through [`SyncEvent`]. A thread
    /// started without it tells no one.
    changes: Mutex<Option<ChangeListener>>,
    /// Who hears what each tick says of damaged rows here (T14.65): the damage scans
    /// ([`crate::damage::Scans`]). A thread started without it tells no one.
    damage: Mutex<Option<earshot_runtime::sync::DamageListener>>,
    /// The threads that were turned off and may still be finishing (handing back summaries
    /// this computer asked for, for up to half an hour): each is told to stop for good the
    /// moment a new thread starts, which then owns that work (T14.45 review). Taken alone,
    /// never with the slot or the store.
    retired: Mutex<Vec<Superseder>>,
    /// The client the pages ask the server through while no sync thread is in place (sync is
    /// off, or it never started), made at the first such ask and kept (T14.50: the open Sync
    /// page asks every few seconds). Dropped whenever the thread is started or this computer is
    /// removed, so it never outlives the address it was made for. Taken alone, after the slot.
    spare: Mutex<Option<Arc<SyncClient>>>,
    /// Moved on, under the `spare` lock, every time the pages' client is dropped (a start, a
    /// removal). An ask notes it before building a client and keeps the client only when it has
    /// not moved, so a client built across a removal is never kept for the address being left.
    spare_generation: std::sync::atomic::AtomicU64,
    /// Tests: the next threads cannot be started, as when the system has no thread to give.
    #[cfg(test)]
    refuse_start: std::sync::atomic::AtomicBool,
    /// Tests: start-up steps made to panic ([`START_COPY_PANICS`], [`START_REPAIR_PANICS`]).
    #[cfg(test)]
    start_panics: std::sync::atomic::AtomicU8,
    /// Tests: how many clients were made for the pages' asks.
    #[cfg(test)]
    spares_made: std::sync::atomic::AtomicUsize,
    /// Tests: run once a page's client is built and before it is kept.
    #[cfg(test)]
    spare_built: Mutex<Option<SpareBuiltHook>>,
}

/// Tests: what runs between building a page's client and keeping it.
#[cfg(test)]
pub type SpareBuiltHook = Box<dyn FnOnce() + Send>;

/// What sync brought to this computer (T12.81): the meetings one pass of the sync thread (its
/// tick and the offload step after it) changed here, by id only (never a title, a line or a
/// summary), so the window reads them again. Sent only after a pass that changed something;
/// `meetings` is empty when only people, folders or Dictionary words changed, which the meeting
/// list still reads again.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Type, tauri_specta::Event)]
pub struct SyncEvent {
    /// Meeting ids.
    pub meetings: Vec<String>,
    /// A person changed (a rename, a merge, a "Me" repair): names in every meeting resolve
    /// through persons, so the open meeting is read again even when it is not named.
    pub people: bool,
}

/// The window's event for what a tick changed: ids only.
#[must_use]
pub fn sync_event(changes: &SyncChanges) -> SyncEvent {
    SyncEvent {
        meetings: changes.meetings.iter().map(ToString::to_string).collect(),
        people: changes.people,
    }
}

impl std::fmt::Debug for SyncHost {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SyncHost").finish_non_exhaustive()
    }
}

impl SyncHost {
    /// A host over `vault` (Credential Manager in the app).
    #[must_use]
    pub fn new(vault: Arc<dyn SyncVault>) -> Self {
        Self::with_timing(vault, SyncTiming::default())
    }

    /// A host with other thread timings (tests).
    #[must_use]
    pub fn with_timing(vault: Arc<dyn SyncVault>, timing: SyncTiming) -> Self {
        Self {
            vault,
            thread: Mutex::new(None),
            lifecycle: Mutex::new(()),
            timing,
            offload: Mutex::new(None),
            changes: Mutex::new(None),
            damage: Mutex::new(None),
            retired: Mutex::new(Vec::new()),
            spare: Mutex::new(None),
            spare_generation: std::sync::atomic::AtomicU64::new(0),
            #[cfg(test)]
            refuse_start: std::sync::atomic::AtomicBool::new(false),
            #[cfg(test)]
            start_panics: std::sync::atomic::AtomicU8::new(0),
            #[cfg(test)]
            spares_made: std::sync::atomic::AtomicUsize::new(0),
            #[cfg(test)]
            spare_built: Mutex::new(None),
        }
    }

    fn spare(&self) -> std::sync::MutexGuard<'_, Option<Arc<SyncClient>>> {
        self.spare.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Drops the pages' client and moves the generation on, so an ask building one meanwhile
    /// does not keep it.
    fn drop_spare(&self) {
        let mut spare = self.spare();
        *spare = None;
        self.spare_generation
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    }

    /// Whether a client is kept for the pages (tests).
    #[cfg(test)]
    pub fn keeps_page_client_for_test(&self) -> bool {
        self.spare().is_some()
    }

    /// Sets what runs once a page's client is built and before it is kept (tests).
    #[cfg(test)]
    pub fn on_spare_built_for_test(&self, hook: SpareBuiltHook) {
        *self
            .spare_built
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(hook);
    }

    /// How many clients were made for the pages' asks (tests).
    #[cfg(test)]
    pub fn spares_made_for_test(&self) -> usize {
        self.spares_made.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// The client a page asks the server through (T14.50): the sync thread's own when a thread
    /// is in place (running, or ended on a problem: its client still answers), else one kept
    /// for the pages. Never a new client for every ask: each has its own HTTP client and
    /// runtime, and the open Sync page asks every few seconds.
    ///
    /// Using the thread's client beside its ticks is safe: an ask reads the vault and makes its
    /// requests, and takes neither the tick lock nor any lock of the thread's
    /// ([`SyncClient::vault_devices`]). The slot is not held while asking.
    fn page_client(&self, services: &Services) -> Result<Arc<SyncClient>, SyncProblem> {
        let generation = self
            .spare_generation
            .load(std::sync::atomic::Ordering::SeqCst);
        let running = self.slot().as_ref().map(SyncThread::client);
        if let Some(client) = running {
            return Ok(client);
        }
        if let Some(client) = self.spare().as_ref() {
            return Ok(Arc::clone(client));
        }
        let client = Arc::new(SyncClient::new(services.clone(), self.vault())?);
        #[cfg(test)]
        self.spares_made
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        #[cfg(test)]
        {
            let hook = self
                .spare_built
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .take();
            if let Some(hook) = hook {
                hook();
            }
        }
        let mut spare = self.spare();
        // Kept only when nothing dropped the pages' client while this one was built: a start
        // or a removal in between may have changed or left the address it was made for. It
        // still answers this one ask.
        if self
            .spare_generation
            .load(std::sync::atomic::Ordering::SeqCst)
            == generation
        {
            if let Some(kept) = spare.as_ref() {
                return Ok(Arc::clone(kept));
            }
            *spare = Some(Arc::clone(&client));
        }
        Ok(client)
    }

    /// The vault's devices, from the server ([`earshot_runtime::sync::vault_devices`]), asked
    /// through [`Self::page_client`].
    ///
    /// # Errors
    /// [`SyncProblem::Vault`] when this computer is not set up; the relay's problems.
    pub fn list_devices(
        &self,
        services: &Services,
    ) -> Result<Vec<earshot_runtime::sync::SyncDevice>, SyncProblem> {
        if !self.vault.has()? {
            return Err(SyncProblem::Vault);
        }
        self.page_client(services)?.vault_devices()
    }

    fn retired(&self) -> std::sync::MutexGuard<'_, Vec<Superseder>> {
        self.retired.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// What would tell the running thread it has been replaced, when one is in place (tests).
    #[cfg(test)]
    pub fn running_superseder_for_test(&self) -> Option<Superseder> {
        self.slot().as_ref().map(SyncThread::superseder)
    }

    /// Makes every start fail to get a thread, or lets them through again (tests).
    #[cfg(test)]
    pub fn refuse_start_for_test(&self, refuse: bool) {
        self.refuse_start
            .store(refuse, std::sync::atomic::Ordering::SeqCst);
    }

    /// Makes the start-up steps named by `steps` panic (a mask of [`START_COPY_PANICS`] and
    /// [`START_REPAIR_PANICS`]; tests).
    #[cfg(test)]
    pub fn panic_in_start_steps_for_test(&self, steps: u8) {
        self.start_panics
            .store(steps, std::sync::atomic::Ordering::SeqCst);
    }

    /// Panics when a test asked this start-up step to (tests only).
    #[cfg(test)]
    fn panic_if_asked(&self, step: u8) {
        if self.start_panics.load(std::sync::atomic::Ordering::SeqCst) & step != 0 {
            panic!("a start-up step made to panic by a test");
        }
    }

    /// The threads turned off and not yet replaced (tests).
    #[cfg(test)]
    pub fn retired_for_test(&self) -> Vec<Superseder> {
        self.retired().clone()
    }

    /// Sets who hears what each tick says of damaged rows here (T14.65); threads started from
    /// now on tell it after every tick ([`SyncClient::with_damage`]).
    pub fn set_damage_listener(&self, listener: earshot_runtime::sync::DamageListener) {
        *self.damage.lock().unwrap_or_else(PoisonError::into_inner) = Some(listener);
    }

    /// Sets who hears what a tick changed here; threads started from now on tell it after every
    /// tick that changed rows ([`SyncClient::with_changes`]).
    pub fn set_change_listener(&self, listener: ChangeListener) {
        *self.changes.lock().unwrap_or_else(PoisonError::into_inner) = Some(listener);
    }

    /// Sets who does summary offload's work; threads started from now on run its step after
    /// every tick ([`earshot_runtime::offload::Offload::step`]).
    pub fn set_offload_host(&self, host: Arc<dyn OffloadHost>) {
        *self.offload.lock().unwrap_or_else(PoisonError::into_inner) = Some(host);
    }

    /// The vault.
    #[must_use]
    pub fn vault(&self) -> Arc<dyn SyncVault> {
        Arc::clone(&self.vault)
    }

    fn slot(&self) -> std::sync::MutexGuard<'_, Option<SyncThread>> {
        self.thread.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn lifecycle(&self) -> std::sync::MutexGuard<'_, ()> {
        self.lifecycle
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// At app start, before the sync thread: when this device has no vault at all, its legacy
    /// ignores go onto the speaker rows now (T12.93, `earshot_runtime::ignore_copy`), since
    /// nothing can arrive from another computer. With a vault (or when the vault cannot be read)
    /// the sync thread runs the copy after its first completed pull instead, whatever the switch.
    /// Returns whether the copy ran here. A store failure only means it is tried again at the
    /// next start.
    pub fn copy_ignores_without_vault(&self, services: &Services) -> bool {
        #[cfg(test)]
        self.panic_if_asked(START_COPY_PANICS);
        if self.vault.has().unwrap_or(true) {
            return false;
        }
        earshot_runtime::ignore_copy::copy_once(services).is_ok()
    }

    /// At app start, before the sync thread, whether sync is on or off: the one-off removal
    /// repair (docs/sync-spec.md 7A.2, `earshot_runtime::sync::removal_repair`). What it changed
    /// reaches the window as one [`SyncEvent`], through the listener the sync thread tells
    /// ([`Self::set_change_listener`]); a panic in it is caught, as the thread catches one.
    /// Returns what the repair did, `None` when it failed (the next start tries again).
    pub fn repair_removals(
        &self,
        services: &Services,
    ) -> Option<earshot_runtime::sync::RemovalRepaired> {
        #[cfg(test)]
        self.panic_if_asked(START_REPAIR_PANICS);
        let repaired = earshot_runtime::sync::removal_repair(services).ok()?;
        if let Some(changes) = repaired.changes() {
            let listener = self
                .changes
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .clone();
            if let Some(listener) = listener {
                let told =
                    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| listener(&changes)));
                if told.is_err() {
                    eprintln!("{}", earshot_runtime::sync::LISTENER_PANICKED);
                }
            }
        }
        Some(repaired)
    }

    /// Everything the app's `earshot-sync-start` thread does, in this order: the legacy ignore
    /// copy without a vault ([`Self::copy_ignores_without_vault`]), the removal repair whether
    /// sync is on or off ([`Self::repair_removals`]), then the sync thread
    /// ([`Self::start_if_on`]). A panic in the copy or the repair is caught and printed as
    /// [`START_STEP_PANICKED`], so sync still starts at every start; a repair that panicked
    /// counts as failed (`None`) and runs again at the next start. Returns what the repair did.
    pub fn start_up(&self, services: &Services) -> Option<earshot_runtime::sync::RemovalRepaired> {
        start_step(|| self.copy_ignores_without_vault(services));
        let repaired = start_step(|| self.repair_removals(services)).flatten();
        self.start_if_on(services);
        repaired
    }

    /// Starts the thread when sync is on and this device is set up, replacing a running one
    /// (the old one is told and stopped only once the new one exists, and joined on a thread
    /// of its own; when no thread can be started the old one stays in place). A device that
    /// cannot start (no usable address) stays stopped; the page says why.
    pub fn start_if_on(&self, services: &Services) {
        let _lifecycle = self.lifecycle();
        self.start_locked(services);
    }

    /// A new sync thread, held: it exists and has done nothing yet ([`SyncThread::start_held`]).
    /// `None` when the thread could not be started.
    fn spawn_held(
        &self,
        client: SyncClient,
        offload: Option<Arc<dyn OffloadHost>>,
    ) -> Option<SyncThread> {
        #[cfg(test)]
        if self.refuse_start.load(std::sync::atomic::Ordering::SeqCst) {
            return None;
        }
        SyncThread::start_held(client, self.timing, offload).ok()
    }

    /// Remembers a thread that is being stopped and may still be finishing, so the next
    /// thread that really starts can tell it so.
    fn remember_retired(&self, thread: &SyncThread) {
        let mut retired = self.retired();
        retired.retain(|s| !s.is_superseded());
        retired.push(thread.superseder());
    }

    fn start_locked(&self, services: &Services) {
        // Whatever follows, the pages' own client is made anew when next needed: the address
        // may be another (this runs after setting up and joining), and a thread about to start
        // brings its own.
        self.drop_spare();
        let on = {
            let db = services.store();
            earshot_runtime::sync::sync_settings(&db).is_ok_and(|s| s.on)
        };
        let ready = on && self.vault.has().unwrap_or(false);
        let offload = self
            .offload
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        let changes = self
            .changes
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        let damage = self
            .damage
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        let client = if ready {
            SyncClient::new(services.clone(), self.vault()).ok()
        } else {
            None
        };
        let Some(client) = client else {
            // Nothing is to run: sync is off, this computer is not set up, or it has no usable
            // address. A thread still running is stopped (it says nothing and hands nothing
            // back, as a replaced thread always did) and remembered until it has ended, so a
            // thread started later tells it before either works the store.
            let old = self.slot().take();
            if let Some(t) = &old {
                self.remember_retired(t);
            }
            retire(old);
            return;
        };
        let client = match changes {
            Some(l) => client.with_changes(l),
            None => client,
        };
        let client = match damage {
            Some(l) => client.with_damage(l),
            None => client,
        };
        // The names the other computers give themselves, whenever the thread reads presence
        // for its own work (T14.50): a meeting waiting for another computer names it after a
        // restart without anyone opening the Sync page.
        let client = client.with_presence(Arc::new(crate::offload::hear_presence));
        let Some(new) = self.spawn_held(client, offload) else {
            // No thread could be started. Nothing was told and nothing was stopped, so
            // whatever runs keeps what it owns: the running thread stays in the slot and goes
            // on ticking and stepping, and a thread that was turned off goes on handing
            // summaries back. The next start tries again.
            return;
        };
        // The new thread exists and has done nothing yet. Only now are the threads it takes
        // over from told: the running one (at once, not when the thread that joins it gets to
        // run) and any that was turned off and is still handing summaries back. Each starts no
        // further tick and runs no further offload step, so none works the store together
        // with the new thread, which owns the hand-backs from its first offload step (it hands
        // back what is still owed, as after a restart).
        let old = self.slot().take();
        if let Some(t) = &old {
            t.superseder().supersede();
        }
        for earlier in self.retired().drain(..) {
            earlier.supersede();
        }
        new.begin();
        retire(old);
        *self.slot() = Some(new);
    }

    /// Stops the thread without waiting for it.
    pub fn stop_soon(&self) {
        let _lifecycle = self.lifecycle();
        let old = self.slot().take();
        if let Some(t) = &old {
            // It may go on handing summaries back for a while: remembered, so a thread started
            // meanwhile can take that over.
            self.remember_retired(t);
        }
        retire_saying_gone(old);
    }

    /// Stops the thread and waits for it (a request in flight ends at its timeout). The slot
    /// is not held while waiting, so the page's reads go on.
    fn stop_and_wait_locked(&self) {
        let old = self.slot().take();
        if let Some(t) = old {
            t.stop();
        }
    }

    /// Stops the thread and waits for it (tests; the app's removal waits under its own lock).
    #[cfg(test)]
    pub fn stop_and_wait(&self) {
        let _lifecycle = self.lifecycle();
        self.stop_and_wait_locked();
    }

    /// Starts the thread if it should run and does not (it ended on a problem, or never
    /// started), else asks it for a tick now.
    pub fn sync_now(&self, services: &Services) {
        if self.running() {
            self.nudge();
        } else {
            self.start_if_on(services);
        }
    }

    /// Asks the running thread for one tick now, even while it backs off after failures (T14.66:
    /// a Stop's withdrawal). With no thread running it does nothing: the withdrawal waits in the
    /// outbox.
    pub fn tick_now(&self) {
        if let Some(t) = self.slot().as_ref() {
            t.tick_now();
        }
    }

    /// Asks the running thread for a tick now.
    pub fn nudge(&self) {
        if let Some(t) = self.slot().as_ref() {
            t.nudge();
        }
    }

    /// Whether a thread is running.
    #[must_use]
    pub fn running(&self) -> bool {
        self.slot().as_ref().is_some_and(|t| !t.ended())
    }

    /// This computer is going away (the app quits, Windows sleeps; T11.3ca): the running
    /// thread says gone from a thread of its own and stops its beats. Waits at most `wait` for
    /// the send (the slot is not held while waiting); never longer, whatever the network does.
    pub fn say_gone_within(&self, wait: Duration) {
        if let Some(rx) = self.say_gone() {
            let _ = rx.recv_timeout(wait);
        }
    }

    /// [`Self::say_gone_within`] without the wait: the receiver hears once the send has
    /// finished; `None` when nothing is sent (no thread, or never seen online).
    #[must_use]
    pub fn say_gone(&self) -> Option<std::sync::mpsc::Receiver<()>> {
        self.slot().as_ref().and_then(SyncThread::say_gone)
    }

    /// This computer is back from sleep: the running thread says online and ticks now.
    pub fn back(&self) {
        if let Some(t) = self.slot().as_ref() {
            t.back();
        }
    }
}

/// How long quitting waits for the gone presence, at most (inside [`crate::shell::EXIT_SAVE_LIMIT`]).
pub const QUIT_GONE_WAIT: Duration = Duration::from_secs(1);
/// How long a suspend waits for the gone presence: Windows gives a suspend notification about
/// two seconds before it may go ahead, and the network may already be on its way down (a
/// judgement).
pub const SLEEP_GONE_WAIT: Duration = Duration::from_millis(1500);

/// Stops a thread for good on a thread of its own, telling the other devices it is gone.
fn retire_saying_gone(old: Option<SyncThread>) {
    if let Some(t) = old {
        let _ = std::thread::Builder::new()
            .name("earshot-sync-stop".into())
            .spawn(move || t.stop_saying_gone());
    }
}

/// Stops a replaced thread on a thread of its own, so no caller waits for its request.
fn retire(old: Option<SyncThread>) {
    if let Some(t) = old {
        let _ = std::thread::Builder::new()
            .name("earshot-sync-stop".into())
            .spawn(move || t.stop());
    }
}

/// What the Sync page shows. Never a key, token, setup secret or used pairing code.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Type)]
pub struct SyncView {
    /// A key and token are stored: this computer is set up.
    pub set_up: bool,
    /// Sync is on.
    pub on: bool,
    /// The sync thread is running now.
    pub running: bool,
    /// Voiceprints sync.
    pub voiceprints: bool,
    /// Dictionary words sync (T11.3e; on unless turned off).
    pub dictionary: bool,
    /// The server's address, when one is stored and usable.
    pub address: Option<String>,
    /// Why a stored address is not usable (fixed copy).
    pub address_problem: Option<String>,
    /// When sync last finished without a problem (Unix ms).
    pub last_success_unix_ms: Option<u64>,
    /// The last problem (fixed copy).
    pub problem: Option<String>,
    /// Whether that problem stopped sync until something is done.
    pub stopped: bool,
    /// Rows from other devices that could not be read, in all.
    pub unreadable: u64,
    /// Rows changed here and not yet sent (damaged ones the last sync skipped and ones it could
    /// not seal left out).
    pub waiting: u64,
    /// [`refused_note`] when the last sync left queued rows unsent because they cannot be sealed
    /// (too large, or holding an audio file's signature): fixed copy with their count (T14.43).
    pub refused_note: Option<String>,
    /// Rows from other devices held for later.
    pub held: u64,
    /// "Write summaries for my other computers" (T11.3db).
    pub offers_summaries: bool,
    /// This computer's name as the other computers show it (T11.3db).
    pub device_name: String,
}

fn view_of(o: SyncOverview, running: bool) -> SyncView {
    let (address, address_problem) = match o.settings.address {
        Some(Ok(url)) => (Some(url.as_str().trim_end_matches('/').to_owned()), None),
        Some(Err(why)) => (None, Some(why.copy().to_owned())),
        None => (None, None),
    };
    SyncView {
        set_up: o.set_up,
        on: o.settings.on,
        running,
        voiceprints: o.settings.voiceprints,
        dictionary: o.settings.dictionary,
        address,
        address_problem,
        last_success_unix_ms: o.status.last_success_unix_ms,
        problem: o.status.problem.map(|p| problem_copy(p).to_owned()),
        stopped: o.status.problem.is_some_and(SyncProblem::stops),
        unreadable: o.status.unreadable,
        waiting: o.waiting,
        refused_note: refused_note(o.status.refused),
        held: o.held,
        offers_summaries: o.settings.offers_summaries,
        device_name: o.settings.device_name,
    }
}

/// The page's view now.
///
/// # Errors
/// Fixed copy for a store or credential failure.
pub fn view(services: &Services, host: &SyncHost) -> Result<SyncView, IpcError> {
    let overview =
        earshot_runtime::sync::sync_overview(services, &*host.vault).map_err(sync_error)?;
    Ok(view_of(overview, host.running()))
}

/// One device of the vault.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Type)]
pub struct SyncDeviceView {
    /// Whether it is this computer.
    pub this_device: bool,
    /// Its name from its presence (`None` until it sends one this computer can read).
    pub name: Option<String>,
    /// Whether it is online now (its presence is at most 150 s old and does not say gone).
    pub online: bool,
    /// Whether it is recording now.
    pub recording: bool,
    /// Whether it writes summaries for the other computers (T11.3db).
    pub offers_summaries: bool,
    /// The kind of writer it uses, as the page names it (fixed copy,
    /// [`crate::offload::computer_writer_label`]); `None` when it does not offer summaries.
    pub writer: Option<String>,
    /// Its whole id (hyphenated UUID, as the summary helper list gives it): what
    /// [`remove_other_sync_device`] names. Not a secret (the server lists it to every device of
    /// the vault).
    pub id: String,
    /// The first eight hex digits of its id.
    pub short_id: String,
    /// When it joined (Unix seconds, the server's clock).
    pub joined_unix_s: u64,
    /// Seconds since it last reached the server.
    pub seen_s_ago: u64,
}

/// The vault's devices, from the server.
///
/// # Errors
/// Fixed copy of the problem.
pub fn devices(services: &Services, host: &SyncHost) -> Result<Vec<SyncDeviceView>, IpcError> {
    Ok(host
        .list_devices(services)
        .map_err(sync_error)?
        .into_iter()
        .inspect(|d| {
            // The Summaries page's step label reads the same cache (T11.6a review).
            if !d.this_device {
                crate::offload::remember_presence(
                    d.device,
                    d.name.clone(),
                    d.writer_kind,
                    d.model_place,
                );
            }
        })
        .map(|d| SyncDeviceView {
            this_device: d.this_device,
            name: d.name,
            online: d.online,
            recording: d.recording,
            offers_summaries: d.offers_summaries,
            writer: d
                .writer_kind
                .map(|k| crate::offload::computer_writer_label(k, d.model_place).to_owned()),
            id: d.device.to_string(),
            short_id: d.short_id,
            joined_unix_s: d.joined_unix_s,
            seen_s_ago: d.seen_s_ago,
        })
        .collect())
}

/// Sets this computer up as the vault's first device.
///
/// # Errors
/// [`IpcError::invalid`] for an address the rule refuses; fixed copy of the problem.
pub fn set_up(
    services: &Services,
    host: &SyncHost,
    address: &str,
    secret: &str,
) -> Result<SyncView, IpcError> {
    check_address(address).map_err(address_error)?;
    if secret.trim().is_empty() {
        return Err(IpcError::invalid("Enter the server's setup secret."));
    }
    earshot_runtime::sync::set_up_first_device(services, &*host.vault, address, secret)
        .map_err(sync_error)?;
    host.start_if_on(services);
    view(services, host)
}

/// A pairing code to show for adding a device.
#[derive(Clone, PartialEq, Eq, Serialize, Type)]
pub struct SyncCodeView {
    /// The code, in groups.
    pub code: String,
    /// Seconds until it stops working.
    pub expires_in_s: u64,
}

impl std::fmt::Debug for SyncCodeView {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SyncCodeView")
            .field("expires_in_s", &self.expires_in_s)
            .finish_non_exhaustive()
    }
}

/// A new pairing code.
///
/// # Errors
/// Fixed copy of the problem.
pub fn offer_code(services: &Services, host: &SyncHost) -> Result<SyncCodeView, IpcError> {
    let offer =
        earshot_runtime::sync::offer_pairing_code(services, host.vault()).map_err(sync_error)?;
    Ok(SyncCodeView {
        code: offer.code().to_owned(),
        expires_in_s: offer.expires_in_s,
    })
}

/// Joins the vault with a pairing code.
///
/// # Errors
/// [`IpcError::invalid`] for an address the rule refuses; fixed copy of the problem.
pub fn join(
    services: &Services,
    host: &SyncHost,
    address: &str,
    code: &str,
) -> Result<SyncView, IpcError> {
    check_address(address).map_err(address_error)?;
    earshot_runtime::sync::join_with_code(services, &*host.vault, address, code)
        .map_err(sync_error)?;
    host.start_if_on(services);
    view(services, host)
}

/// Turns sync on or off. Off stops the thread and keeps every row, the key and the token.
///
/// # Errors
/// Fixed copy of a store failure.
pub fn set_on(services: &Services, host: &SyncHost, on: bool) -> Result<SyncView, IpcError> {
    earshot_runtime::sync::set_sync_on(services, on).map_err(sync_error)?;
    if on {
        host.start_if_on(services);
    } else {
        host.stop_soon();
    }
    view(services, host)
}

/// Turns voiceprint sync on or off.
///
/// # Errors
/// Fixed copy of a store failure.
pub fn set_voiceprints(
    services: &Services,
    host: &SyncHost,
    on: bool,
) -> Result<SyncView, IpcError> {
    earshot_runtime::sync::set_sync_voiceprints(services, on).map_err(sync_error)?;
    host.nudge();
    view(services, host)
}

/// Turns Dictionary sync on or off (on: rows for the words here, and every earlier row read).
///
/// # Errors
/// Fixed copy of a store failure.
pub fn set_dictionary(
    services: &Services,
    host: &SyncHost,
    on: bool,
) -> Result<SyncView, IpcError> {
    earshot_runtime::sync::set_sync_dictionary(services, on).map_err(sync_error)?;
    host.nudge();
    view(services, host)
}

/// Turns "Write summaries for my other computers" on or off; the next presence says so.
///
/// # Errors
/// Fixed copy of a store failure.
pub fn set_offer_summaries(
    services: &Services,
    host: &SyncHost,
    on: bool,
) -> Result<SyncView, IpcError> {
    earshot_runtime::sync::set_offer_summaries(services, on).map_err(sync_error)?;
    host.nudge();
    view(services, host)
}

/// The longest name the page accepts (characters), as presence keeps it.
pub const MAX_DEVICE_NAME: usize = earshot_runtime::sync::MAX_DEVICE_NAME;

/// Sets this computer's name as the other computers show it; empty goes back to the computer's
/// own name.
///
/// # Errors
/// [`IpcError::invalid`] for a name over [`MAX_DEVICE_NAME`] characters; fixed copy of a store
/// failure.
pub fn set_device_name(
    services: &Services,
    host: &SyncHost,
    name: &str,
) -> Result<SyncView, IpcError> {
    if name.trim().chars().count() > MAX_DEVICE_NAME {
        return Err(IpcError::invalid(DEVICE_NAME_TOO_LONG));
    }
    earshot_runtime::sync::set_device_name(services, name).map_err(sync_error)?;
    host.nudge();
    view(services, host)
}

/// The refusal of a name over [`MAX_DEVICE_NAME`] characters (the table's `device_name_too_long`,
/// `fixtures/ui/sync_copy_cases.json`).
pub const DEVICE_NAME_TOO_LONG: &str = "The name can have at most 64 characters.";

/// Removes this computer from sync (the thread first, then the server and what it kept).
///
/// # Errors
/// Fixed copy of the problem; nothing is deleted when the server cannot be told.
pub fn remove(services: &Services, host: &SyncHost) -> Result<SyncView, IpcError> {
    let removed = {
        // No start may slip in between the stop and the deletions.
        let _lifecycle = host.lifecycle();
        host.stop_and_wait_locked();
        // The pages' own client was made for the address this computer is leaving.
        host.drop_spare();
        let removed = earshot_runtime::sync::remove_this_device(services, host.vault());
        if removed.is_err() {
            // Still set up: carry on syncing.
            host.start_locked(services);
        } else {
            // Again once the address is gone: an ask that began building during the removal
            // keeps nothing either.
            host.drop_spare();
        }
        removed
    };
    removed.map_err(sync_error)?;
    view(services, host)
}

/// The refusal of an id that is not another computer of the vault (the table's
/// `remove_other.not_another`, `fixtures/ui/sync_copy_cases.json`).
pub const NOT_ANOTHER_DEVICE: &str =
    "That is not another of your computers. To remove this computer, use Remove this computer.";

/// A device id as [`SyncDeviceView::id`] gives it (a UUID, as the summary helper list's ids).
fn parse_device_id(id: &str) -> Option<earshot_core::identity::DeviceId> {
    serde_json::from_value(serde_json::Value::String(id.to_owned())).ok()
}

/// Another computer was removed (T11.6c).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Type)]
pub struct SyncRemovedView {
    /// The computers left, or `None` when they could not be listed after the removal (the page
    /// asks again; the removal itself happened).
    pub devices: Option<Vec<SyncDeviceView>>,
}

/// Removes another computer from the vault (T11.6c): the server revokes its token and drops
/// its presence; its synced rows stay. Returns the computers left. This computer leaves only
/// through [`remove`].
///
/// # Errors
/// [`IpcError::invalid`] with [`NOT_ANOTHER_DEVICE`] for this computer's id or one that is not
/// an id, with nothing sent; fixed copy of the problem. Once the server has revoked it, the
/// answer is a success whatever the list does.
pub fn remove_other(
    services: &Services,
    host: &SyncHost,
    id: &str,
) -> Result<SyncRemovedView, IpcError> {
    remove_other_then(services, host, id, || devices(services, host))
}

/// [`remove_other`] with the list read after the removal given (a test seam).
fn remove_other_then(
    services: &Services,
    host: &SyncHost,
    id: &str,
    list: impl FnOnce() -> Result<Vec<SyncDeviceView>, IpcError>,
) -> Result<SyncRemovedView, IpcError> {
    let device = parse_device_id(id)
        .filter(|d| *d != services.store().device())
        .ok_or_else(|| IpcError::invalid(NOT_ANOTHER_DEVICE))?;
    earshot_runtime::sync::remove_other_device(services, host.vault(), device)
        .map_err(sync_error)?;
    Ok(SyncRemovedView {
        devices: list().ok(),
    })
}

fn host(state: &tauri::State<'_, Arc<SyncHost>>) -> Arc<SyncHost> {
    Arc::clone(state)
}

/// The Sync page's view.
#[tauri::command]
#[specta::specta]
pub async fn sync_view(
    state: tauri::State<'_, AppState>,
    sync: tauri::State<'_, Arc<SyncHost>>,
) -> Result<SyncView, IpcError> {
    let services = state.services()?;
    let host = host(&sync);
    blocking(move || view(&services, &host)).await
}

/// The vault's devices (asks the server).
#[tauri::command]
#[specta::specta]
pub async fn sync_devices(
    state: tauri::State<'_, AppState>,
    sync: tauri::State<'_, Arc<SyncHost>>,
) -> Result<Vec<SyncDeviceView>, IpcError> {
    let services = state.services()?;
    let host = host(&sync);
    blocking(move || devices(&services, &host)).await
}

/// Sets this computer up as the first device with the server's setup secret.
#[tauri::command]
#[specta::specta]
pub async fn set_up_sync(
    state: tauri::State<'_, AppState>,
    sync: tauri::State<'_, Arc<SyncHost>>,
    address: String,
    setup_secret: String,
) -> Result<SyncView, IpcError> {
    let services = match state.services() {
        Ok(s) => s,
        Err(e) => {
            crate::credentials::wipe_string(setup_secret);
            return Err(e);
        }
    };
    let host = host(&sync);
    blocking(move || {
        let out = set_up(&services, &host, &address, &setup_secret);
        crate::credentials::wipe_string(setup_secret);
        out
    })
    .await
}

/// A pairing code for adding another device.
#[tauri::command]
#[specta::specta]
pub async fn offer_sync_code(
    state: tauri::State<'_, AppState>,
    sync: tauri::State<'_, Arc<SyncHost>>,
) -> Result<SyncCodeView, IpcError> {
    let services = state.services()?;
    let host = host(&sync);
    blocking(move || offer_code(&services, &host)).await
}

/// Joins the vault with a pairing code shown on another device.
#[tauri::command]
#[specta::specta]
pub async fn join_sync(
    state: tauri::State<'_, AppState>,
    sync: tauri::State<'_, Arc<SyncHost>>,
    address: String,
    code: String,
) -> Result<SyncView, IpcError> {
    let services = match state.services() {
        Ok(s) => s,
        Err(e) => {
            crate::credentials::wipe_string(code);
            return Err(e);
        }
    };
    let host = host(&sync);
    blocking(move || {
        let out = join(&services, &host, &address, &code);
        crate::credentials::wipe_string(code);
        out
    })
    .await
}

/// Turns sync on or off.
#[tauri::command]
#[specta::specta]
pub async fn set_sync_on(
    state: tauri::State<'_, AppState>,
    sync: tauri::State<'_, Arc<SyncHost>>,
    on: bool,
) -> Result<SyncView, IpcError> {
    let services = state.services()?;
    let host = host(&sync);
    blocking(move || set_on(&services, &host, on)).await
}

/// Turns voiceprint sync on or off.
#[tauri::command]
#[specta::specta]
pub async fn set_sync_voiceprints(
    state: tauri::State<'_, AppState>,
    sync: tauri::State<'_, Arc<SyncHost>>,
    on: bool,
) -> Result<SyncView, IpcError> {
    let services = state.services()?;
    let host = host(&sync);
    blocking(move || set_voiceprints(&services, &host, on)).await
}

/// Turns Dictionary sync on or off.
#[tauri::command]
#[specta::specta]
pub async fn set_sync_dictionary(
    state: tauri::State<'_, AppState>,
    sync: tauri::State<'_, Arc<SyncHost>>,
    on: bool,
) -> Result<SyncView, IpcError> {
    let services = state.services()?;
    let host = host(&sync);
    blocking(move || set_dictionary(&services, &host, on)).await
}

/// Turns "Write summaries for my other computers" on or off.
#[tauri::command]
#[specta::specta]
pub async fn set_sync_offer_summaries(
    state: tauri::State<'_, AppState>,
    sync: tauri::State<'_, Arc<SyncHost>>,
    on: bool,
) -> Result<SyncView, IpcError> {
    let services = state.services()?;
    let host = host(&sync);
    blocking(move || set_offer_summaries(&services, &host, on)).await
}

/// Sets this computer's name as the other computers show it.
#[tauri::command]
#[specta::specta]
pub async fn set_sync_device_name(
    state: tauri::State<'_, AppState>,
    sync: tauri::State<'_, Arc<SyncHost>>,
    name: String,
) -> Result<SyncView, IpcError> {
    let services = state.services()?;
    let host = host(&sync);
    blocking(move || set_device_name(&services, &host, &name)).await
}

/// Asks for a sync now.
#[tauri::command]
#[specta::specta]
pub async fn sync_now(
    state: tauri::State<'_, AppState>,
    sync: tauri::State<'_, Arc<SyncHost>>,
) -> Result<SyncView, IpcError> {
    let services = state.services()?;
    let host = host(&sync);
    blocking(move || {
        host.sync_now(&services);
        view(&services, &host)
    })
    .await
}

/// Removes this computer from sync.
#[tauri::command]
#[specta::specta]
pub async fn remove_sync_device(
    state: tauri::State<'_, AppState>,
    sync: tauri::State<'_, Arc<SyncHost>>,
) -> Result<SyncView, IpcError> {
    let services = state.services()?;
    let host = host(&sync);
    blocking(move || remove(&services, &host)).await
}

/// Removes another computer from sync; returns the computers left when they can be listed.
#[tauri::command]
#[specta::specta]
pub async fn remove_other_sync_device(
    state: tauri::State<'_, AppState>,
    sync: tauri::State<'_, Arc<SyncHost>>,
    id: String,
) -> Result<SyncRemovedView, IpcError> {
    let services = state.services()?;
    let host = host(&sync);
    blocking(move || remove_other(&services, &host, &id)).await
}

#[cfg(test)]
#[path = "sync_settings_tests.rs"]
mod tests;
