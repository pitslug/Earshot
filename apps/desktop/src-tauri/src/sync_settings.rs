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
    AddressRefused, SyncClient, SyncOverview, SyncProblem, SyncThread, SyncTiming, SyncVault,
    check_address,
};
use serde::Serialize;
use specta::Type;

use crate::ipc::{AppState, IpcError, blocking};

/// The IPC error kind of a sync problem.
pub const SYNC_ERROR_KIND: &str = "sync";

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
/// a button press, so these four say what to do instead.
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
        }
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

    /// Starts the thread when sync is on and this device is set up, replacing a running one
    /// (the old one is stopped first, on a thread of its own). A device that cannot start (no
    /// usable address) stays stopped; the page says why.
    pub fn start_if_on(&self, services: &Services) {
        let _lifecycle = self.lifecycle();
        self.start_locked(services);
    }

    fn start_locked(&self, services: &Services) {
        let old = self.slot().take();
        retire(old);
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
        let next = if ready {
            SyncClient::new(services.clone(), self.vault())
                .ok()
                .and_then(|c| match offload {
                    Some(host) => SyncThread::start_offloading(c, self.timing, host).ok(),
                    None => SyncThread::start(c, self.timing).ok(),
                })
        } else {
            None
        };
        *self.slot() = next;
    }

    /// Stops the thread without waiting for it.
    pub fn stop_soon(&self) {
        let _lifecycle = self.lifecycle();
        let old = self.slot().take();
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
    /// Rows changed here and not yet sent.
    pub waiting: u64,
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
        address,
        address_problem,
        last_success_unix_ms: o.status.last_success_unix_ms,
        problem: o.status.problem.map(|p| problem_copy(p).to_owned()),
        stopped: o.status.problem.is_some_and(SyncProblem::stops),
        unreadable: o.status.unreadable,
        waiting: o.waiting,
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
    /// [`crate::offload::writer_label`]); `None` when it does not offer summaries.
    pub writer: Option<String>,
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
    Ok(earshot_runtime::sync::vault_devices(services, host.vault())
        .map_err(sync_error)?
        .into_iter()
        .map(|d| SyncDeviceView {
            this_device: d.this_device,
            name: d.name,
            online: d.online,
            recording: d.recording,
            offers_summaries: d.offers_summaries,
            writer: d
                .writer_kind
                .map(|k| crate::offload::writer_label(k).to_owned()),
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
        let removed = earshot_runtime::sync::remove_this_device(services, host.vault());
        if removed.is_err() {
            // Still set up: carry on syncing.
            host.start_locked(services);
        }
        removed
    };
    removed.map_err(sync_error)?;
    view(services, host)
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

#[cfg(test)]
#[path = "sync_settings_tests.rs"]
mod tests;
