//! The sync client (T11.3; docs/sync.md section 6, docs/sync-spec.md section 6).
//!
//! One background thread ([`SyncThread`]) runs [`SyncClient::tick`]: push this device's
//! changed rows (the store's outbox), then pull what other devices pushed after the stored
//! cursor and apply it ([`earshot_store::Store::apply_remote`]). Every request goes through
//! one `earshot-net` client whose allowlist is exactly the sync server, so each hop is in its
//! audit log. Rows leave sealed ([`earshot_sync::seal_row`]); the relay never holds a key.
//!
//! - A meeting being recorded is not pushed: its rows stay queued until the recording is
//!   saved ([`crate::services::MeetingJob::Recording`]).
//! - A server that cannot be reached is a status ([`SyncStatus`]) and a retry with back-off
//!   ([`backoff`]); it never reaches a meeting.
//! - The vault key and the device token are reached only through [`SyncVault`]: Credential
//!   Manager in the desktop app, a file on a summary worker, memory in tests. Neither, nor a
//!   pairing code after it is used, nor the setup secret, is ever stored elsewhere, logged or
//!   returned to a window.
//! - Voiceprints sync only while `settings/sync-voiceprints` is on (off by default); turning
//!   it on reads every earlier row again ([`set_sync_voiceprints`]).
//! - Dictionary words sync while `settings/sync-dictionary` is on (on by default, T11.3e);
//!   turning it on makes a row for each word this device has and reads every earlier row
//!   again ([`set_sync_dictionary`]). After every applied page `settings/vocabulary` is rebuilt
//!   from the live rows in the page's own transaction (`crate::vocabulary`).
//!
//! Setting up a device is [`set_up_first_device`], [`offer_pairing_code`] and
//! [`join_with_code`]; [`remove_this_device`] undoes it.

mod client;
mod engine;
mod presence;
mod setup;
mod thread;

use std::fmt;
use std::sync::Mutex;

use earshot_net::Url;
use earshot_store::{Store, SyncKinds, encode_versioned, split_version};
use serde::{Deserialize, Serialize};

use crate::local::{self, DeviceRecord};
use crate::services::Services;

/// The vault key type, for hosts that store it ([`SyncVault`]).
pub use earshot_sync::VaultKey;
pub use engine::{Peer, SyncClient, TickReport, WorkerWriter};
pub use presence::{
    Beats, KIND_APP, KIND_SERVER, MAX_DEVICE_NAME, ModelPlace, ONLINE_WITHIN_S, PRESENCE_EVERY,
    Presence, clean_name, computer_name, online,
};
pub use setup::{
    PairingOffer, join_with_code, offer_pairing_code, remove_other_device, remove_this_device,
    set_up_first_device,
};
pub use thread::{SyncThread, SyncTiming, backoff};

/// `settings/sync-server`: the sync server's address.
pub(crate) const SYNC_SERVER_KEY: &str = "settings/sync-server";
/// `settings/sync`: whether sync runs (`on`/`off`; absent is off).
pub(crate) const SYNC_ON_KEY: &str = "settings/sync";
/// `settings/sync-voiceprints`: whether voiceprints sync (`on`/`off`; absent is off).
pub(crate) const SYNC_VOICEPRINTS_KEY: &str = "settings/sync-voiceprints";
/// `settings/sync-dictionary`: whether Dictionary words sync (`on`/`off`; absent is on).
pub(crate) const SYNC_DICTIONARY_KEY: &str = "settings/sync-dictionary";
/// `sync-cursor`: the relay sequence number pulled up to.
pub(crate) const SYNC_CURSOR_KEY: &str = "sync-cursor";
/// `sync-status`: the last tick's outcome (times, a problem word, counts; never content).
pub(crate) const SYNC_STATUS_KEY: &str = "sync-status";
/// `settings/device-name`: this device's name as its other devices show it (absent: the
/// computer's name).
pub(crate) const DEVICE_NAME_KEY: &str = "settings/device-name";
/// `settings/sync-offer-summaries`: whether this device writes summaries for the others
/// (`on`/`off`; absent is off). Sent in presence; summary offload is T11.3d.
pub(crate) const OFFER_SUMMARIES_KEY: &str = "settings/sync-offer-summaries";

/// The longest address accepted.
pub const MAX_ADDRESS: usize = 2048;

const CURSOR_VERSION: u32 = 1;
const STATUS_VERSION: u32 = 1;

/// What went wrong, as a fixed word. Nothing a server or a person typed is carried.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SyncProblem {
    /// The server could not be reached (connection, TLS, timeout).
    Unreachable,
    /// The server answered something a relay would not, or its own error.
    Server,
    /// The relay does not know this device's token: it was removed (stops the loop).
    UnknownDevice,
    /// This device's key is not the vault's: join with a pairing code (stops the loop).
    KeyMismatch,
    /// The vault holds rows from a newer Earshot: update this one (stops the loop).
    UpdateNeeded,
    /// Too many failed attempts at the server: wait a minute.
    SlowDown,
    /// A request was over the server's size limit.
    TooLarge,
    /// The setup secret is not the server's.
    BadSecret,
    /// The pairing code is not one the server holds (wrong, used or expired).
    NoSlot,
    /// The pairing code is mistyped (its own check failed).
    BadCode,
    /// This device is already registered; only the setup secret registers it again.
    DeviceExists,
    /// The stored or typed server address is refused (stops the loop).
    Address,
    /// The vault key or device token is missing or cannot be read (stops the loop).
    Vault,
    /// This device's store failed.
    Store,
    /// Something inside sync failed that should not (a panic, a sealing failure).
    Internal,
}

impl SyncProblem {
    /// Every problem.
    pub const ALL: [Self; 15] = [
        Self::Unreachable,
        Self::Server,
        Self::UnknownDevice,
        Self::KeyMismatch,
        Self::UpdateNeeded,
        Self::SlowDown,
        Self::TooLarge,
        Self::BadSecret,
        Self::NoSlot,
        Self::BadCode,
        Self::DeviceExists,
        Self::Address,
        Self::Vault,
        Self::Store,
        Self::Internal,
    ];

    /// The stored word.
    #[must_use]
    pub const fn word(self) -> &'static str {
        match self {
            Self::Unreachable => "unreachable",
            Self::Server => "server",
            Self::UnknownDevice => "unknown_device",
            Self::KeyMismatch => "key_mismatch",
            Self::UpdateNeeded => "update_needed",
            Self::SlowDown => "slow_down",
            Self::TooLarge => "too_large",
            Self::BadSecret => "bad_secret",
            Self::NoSlot => "no_slot",
            Self::BadCode => "bad_code",
            Self::DeviceExists => "device_exists",
            Self::Address => "address",
            Self::Vault => "vault",
            Self::Store => "store",
            Self::Internal => "internal",
        }
    }

    /// Whether the loop stops until the person acts, rather than retrying.
    #[must_use]
    pub const fn stops(self) -> bool {
        matches!(
            self,
            Self::UnknownDevice
                | Self::KeyMismatch
                | Self::UpdateNeeded
                | Self::Address
                | Self::Vault
        )
    }
}

impl fmt::Display for SyncProblem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.word())
    }
}

impl std::error::Error for SyncProblem {}

impl From<earshot_store::StoreError> for SyncProblem {
    fn from(_: earshot_store::StoreError) -> Self {
        Self::Store
    }
}

impl From<earshot_core::model::RepoError> for SyncProblem {
    fn from(_: earshot_core::model::RepoError) -> Self {
        Self::Store
    }
}

impl From<crate::RuntimeError> for SyncProblem {
    fn from(_: crate::RuntimeError) -> Self {
        Self::Store
    }
}

/// A device token as the relay issued it. `Debug` shows nothing; wiped on drop.
pub struct DeviceToken(String);

impl DeviceToken {
    /// A token as stored.
    #[must_use]
    pub fn new(token: String) -> Self {
        Self(token)
    }

    /// The token, for storing it or sending it as a bearer token. Keep the borrow short.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Drop for DeviceToken {
    fn drop(&mut self) {
        earshot_net::wipe_string(&mut self.0);
    }
}

impl fmt::Debug for DeviceToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("DeviceToken(..)")
    }
}

/// The vault key and this device's token, read from a [`SyncVault`] for one tick.
#[derive(Debug)]
pub struct SyncSecrets {
    /// The vault key.
    pub key: VaultKey,
    /// The device token.
    pub token: DeviceToken,
}

/// Why a [`SyncVault`] failed. Says nothing about what it holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VaultError {
    /// The credential store failed.
    Store,
    /// This host has no credential store.
    Unsupported,
}

/// A credential store that fails to answer is a retry ([`SyncProblem::Store`]); only a
/// missing key or token is [`SyncProblem::Vault`], which stops the loop.
impl From<VaultError> for SyncProblem {
    fn from(_: VaultError) -> Self {
        Self::Store
    }
}

/// Where a device keeps its vault key and token: Credential Manager (`Earshot/sync-key`,
/// `Earshot/sync-token`) in the desktop app. A window never reads either back.
pub trait SyncVault: Send + Sync {
    /// Stores both, replacing what was there.
    ///
    /// # Errors
    /// [`VaultError`].
    fn save(&self, key: &VaultKey, token: &DeviceToken) -> Result<(), VaultError>;
    /// Whether both are stored.
    ///
    /// # Errors
    /// [`VaultError`].
    fn has(&self) -> Result<bool, VaultError>;
    /// Deletes both.
    ///
    /// # Errors
    /// [`VaultError`].
    fn remove(&self) -> Result<(), VaultError>;
    /// Both, for the sync thread; `None` when either is missing.
    ///
    /// # Errors
    /// [`VaultError`].
    fn load(&self) -> Result<Option<SyncSecrets>, VaultError>;
}

/// A [`SyncVault`] in memory (tests, and hosts that set sync up for one run).
#[derive(Default)]
pub struct MemoryVault(Mutex<Option<([u8; 32], DeviceToken)>>);

impl fmt::Debug for MemoryVault {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("MemoryVault(..)")
    }
}

impl MemoryVault {
    fn slot(&self) -> std::sync::MutexGuard<'_, Option<([u8; 32], DeviceToken)>> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl SyncVault for MemoryVault {
    fn save(&self, key: &VaultKey, token: &DeviceToken) -> Result<(), VaultError> {
        *self.slot() = Some((*key.as_bytes(), DeviceToken::new(token.as_str().to_owned())));
        Ok(())
    }

    fn has(&self) -> Result<bool, VaultError> {
        Ok(self.slot().is_some())
    }

    fn remove(&self) -> Result<(), VaultError> {
        *self.slot() = None;
        Ok(())
    }

    fn load(&self) -> Result<Option<SyncSecrets>, VaultError> {
        Ok(self.slot().as_ref().map(|(key, token)| SyncSecrets {
            key: VaultKey::from_bytes(*key),
            token: DeviceToken::new(token.as_str().to_owned()),
        }))
    }
}

/// Why an address cannot be the sync server's. Never names the address.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AddressRefused {
    /// Nothing was entered.
    Empty,
    /// Longer than [`MAX_ADDRESS`].
    TooLong,
    /// Not a web address with a host.
    NotAnAddress,
    /// `http` to a host that is not this computer: only `https` crosses a network.
    NotHttps,
    /// It holds a user name or password.
    Credentials,
    /// It has a path, a query or a fragment: the server is reached at its root.
    NotTheRoot,
}

impl AddressRefused {
    /// Fixed copy for the person.
    #[must_use]
    pub const fn copy(self) -> &'static str {
        match self {
            Self::Empty => "Enter the sync server's address.",
            Self::TooLong => "The address is too long.",
            Self::NotAnAddress => "That is not a web address.",
            Self::NotHttps => {
                "The address must start with https:// (http:// only for this computer)."
            }
            Self::Credentials => "The address must not hold a user name or password.",
            Self::NotTheRoot => {
                "Enter the server's address without a path, for example https://sync.example.com"
            }
        }
    }
}

/// The sync server's address from typed or stored text: `https://<host>[:port]`, or `http`
/// to this computer (`localhost`, `127.0.0.0/8`, `::1`), with no user name, path, query or
/// fragment.
///
/// # Errors
/// [`AddressRefused`].
pub fn check_address(text: &str) -> Result<Url, AddressRefused> {
    let text = text.trim();
    if text.is_empty() {
        return Err(AddressRefused::Empty);
    }
    if text.len() > MAX_ADDRESS {
        return Err(AddressRefused::TooLong);
    }
    let url = Url::parse(text).map_err(|_| AddressRefused::NotAnAddress)?;
    if earshot_net::check_url_shape(&url).is_err() {
        return Err(if !url.username().is_empty() || url.password().is_some() {
            AddressRefused::Credentials
        } else {
            AddressRefused::NotAnAddress
        });
    }
    if url.path() != "/" || url.query().is_some() || url.fragment().is_some() {
        return Err(AddressRefused::NotTheRoot);
    }
    let host = url.host_str().unwrap_or("");
    let loopback = host.eq_ignore_ascii_case("localhost")
        || host
            .trim_start_matches('[')
            .trim_end_matches(']')
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback());
    match url.scheme() {
        "https" => Ok(url),
        "http" if loopback => Ok(url),
        _ => Err(AddressRefused::NotHttps),
    }
}

/// The stored sync settings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncSettings {
    /// The server address; `Err` names why a stored one is refused (never its text).
    pub address: Option<Result<Url, AddressRefused>>,
    /// Whether sync runs.
    pub on: bool,
    /// Whether voiceprints sync.
    pub voiceprints: bool,
    /// Whether Dictionary words sync (on unless turned off).
    pub dictionary: bool,
    /// This device's name as its other devices show it.
    pub device_name: String,
    /// Whether this device writes summaries for the others.
    pub offers_summaries: bool,
}

impl SyncSettings {
    /// The opt-in kinds these settings sync.
    #[must_use]
    pub fn kinds(&self) -> SyncKinds {
        SyncKinds {
            voiceprints: self.voiceprints,
            dictionary: self.dictionary,
        }
    }
}

fn on_setting(store: &Store, key: &str) -> Result<bool, SyncProblem> {
    on_setting_or(store, key, false)
}

/// A stored `on`/`off` switch; absent or unreadable is `default`.
fn on_setting_or(store: &Store, key: &str, default: bool) -> Result<bool, SyncProblem> {
    Ok(store
        .local_value(key)?
        .and_then(|b| local::on_off(&String::from_utf8_lossy(&b)))
        .unwrap_or(default))
}

/// The stored sync settings (an unreadable switch reads as off).
///
/// # Errors
/// [`SyncProblem::Store`].
pub fn sync_settings(store: &Store) -> Result<SyncSettings, SyncProblem> {
    let address = store
        .local_value(SYNC_SERVER_KEY)?
        .map(|b| check_address(&String::from_utf8_lossy(&b)));
    Ok(SyncSettings {
        address,
        on: on_setting(store, SYNC_ON_KEY)?,
        voiceprints: on_setting(store, SYNC_VOICEPRINTS_KEY)?,
        dictionary: on_setting_or(store, SYNC_DICTIONARY_KEY, true)?,
        device_name: store
            .local_value(DEVICE_NAME_KEY)?
            .and_then(|b| clean_name(&String::from_utf8_lossy(&b)))
            .unwrap_or_else(computer_name),
        offers_summaries: on_setting(store, OFFER_SUMMARIES_KEY)?,
    })
}

/// Sets this device's name as its other devices show it; an empty name goes back to the
/// computer's name. Sent with the next presence.
///
/// # Errors
/// [`SyncProblem::Store`].
pub fn set_device_name(services: &Services, name: &str) -> Result<(), SyncProblem> {
    let mut db = services.store();
    match clean_name(name) {
        Some(n) => local::set(&mut db, &DeviceRecord::DeviceName.key(), n.as_bytes())?,
        None => {
            db.delete_local_value(DEVICE_NAME_KEY)?;
        }
    }
    Ok(())
}

/// Turns "Write summaries for my other devices" on or off (sent in presence; the offload it
/// offers is T11.3d).
///
/// # Errors
/// [`SyncProblem::Store`].
pub fn set_offer_summaries(services: &Services, on: bool) -> Result<(), SyncProblem> {
    let mut db = services.store();
    local::set(
        &mut db,
        &DeviceRecord::OfferSummaries.key(),
        on_off_word(on),
    )?;
    Ok(())
}

fn on_off_word(on: bool) -> &'static [u8] {
    if on { b"on" } else { b"off" }
}

/// Turns sync on or off. Off stops nothing by itself (the host stops its [`SyncThread`]);
/// local data, the key and the token stay, so turning it on again resumes.
///
/// # Errors
/// [`SyncProblem::Store`].
pub fn set_sync_on(services: &Services, on: bool) -> Result<(), SyncProblem> {
    crate::vocabulary::with_stamp(services, |tx, stamp| -> Result<(), SyncProblem> {
        local::set_tx(tx, &DeviceRecord::SyncOn.key(), on_off_word(on))?;
        if on {
            // Dictionary words changed while sync was off get rows and the list follows them
            // (T11.3e), as turning the Dictionary switch on does.
            crate::vocabulary::syncing_turned_on_tx(tx, stamp)?;
        }
        Ok(())
    })
}

/// Turns voiceprint sync on or off. Turning it on resets the pull cursor, so voiceprints
/// pushed while it was off are read; rows already here are ignored as not newer.
///
/// # Errors
/// [`SyncProblem::Store`].
pub fn set_sync_voiceprints(services: &Services, on: bool) -> Result<(), SyncProblem> {
    let mut db = services.store();
    let was = on_setting(&db, SYNC_VOICEPRINTS_KEY)?;
    db.transaction(|tx| -> Result<(), SyncProblem> {
        local::set_tx(tx, &DeviceRecord::SyncVoiceprints.key(), on_off_word(on))?;
        if on && !was {
            local::set_tx(tx, &DeviceRecord::SyncCursor.key(), &encode_cursor(0))?;
        }
        Ok(())
    })?;
    Ok(())
}

/// Turns Dictionary sync on or off (docs/sync.md section 9). Turning it on makes a row for each
/// word this device already has and resets the pull cursor, so words pushed while it was off
/// are read; off keeps this device's list as it is and neither sends nor applies word rows.
/// One transaction.
///
/// # Errors
/// [`SyncProblem::Store`].
pub fn set_sync_dictionary(services: &Services, on: bool) -> Result<(), SyncProblem> {
    crate::vocabulary::with_stamp(services, |tx, stamp| -> Result<(), SyncProblem> {
        let was = tx
            .local_value(SYNC_DICTIONARY_KEY)?
            .and_then(|b| local::on_off(&String::from_utf8_lossy(&b)))
            .unwrap_or(true);
        local::set_tx(tx, &DeviceRecord::SyncDictionary.key(), on_off_word(on))?;
        if on && !was {
            crate::vocabulary::syncing_turned_on_tx(tx, stamp)?;
            local::set_tx(tx, &DeviceRecord::SyncCursor.key(), &encode_cursor(0))?;
        }
        Ok(())
    })
}

#[derive(Serialize, Deserialize)]
struct Cursor {
    pull_seq: u64,
}

fn encode_cursor(pull_seq: u64) -> Vec<u8> {
    encode_versioned(CURSOR_VERSION, &Cursor { pull_seq }).expect("a cursor serialises")
}

/// The relay sequence number pulled up to; 0 when none is stored or it cannot be read (a
/// pull from the start applies nothing twice: rows already here are not newer).
///
/// # Errors
/// [`SyncProblem::Store`].
pub fn pull_cursor(store: &Store) -> Result<u64, SyncProblem> {
    Ok(store
        .local_value(SYNC_CURSOR_KEY)?
        .and_then(|b| {
            let (v, payload) = split_version(&b)?;
            (v == CURSOR_VERSION)
                .then(|| serde_json::from_slice::<Cursor>(payload).ok())
                .flatten()
        })
        .map_or(0, |c| c.pull_seq))
}

/// Moves the cursor from `after` to `to` only if it still reads `after` (the caller holds the
/// store lock across both): a reset made while the page was on the way wins. Returns whether
/// it moved.
fn advance_cursor(store: &mut Store, after: u64, to: u64) -> Result<bool, SyncProblem> {
    if pull_cursor(store)? != after {
        return Ok(false);
    }
    local::set(store, &DeviceRecord::SyncCursor.key(), &encode_cursor(to))?;
    Ok(true)
}

/// The last tick's outcome. Times, a problem word and counts; never content.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SyncStatus {
    /// When a tick last finished without a problem (Unix ms).
    pub last_success_unix_ms: Option<u64>,
    /// The last tick's problem; `None` after a tick without one.
    pub problem: Option<SyncProblem>,
    /// Pulled rows that could not be opened or read, in all.
    pub unreadable: u64,
}

/// What the Sync page shows: the settings, the last tick's outcome and the queues. Counts and
/// words only; never an address's text when it is refused, never a key or token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncOverview {
    /// Whether a key and token are stored (this device is set up).
    pub set_up: bool,
    /// The stored settings.
    pub settings: SyncSettings,
    /// The last tick's outcome.
    pub status: SyncStatus,
    /// Rows changed here and not yet sent.
    pub waiting: u64,
    /// Pulled rows held for later (a meeting in use, a missing parent, a newer format, a clock).
    pub held: u64,
}

/// The Sync page's view of this device.
///
/// # Errors
/// [`SyncProblem::Store`]; a failing credential store is [`SyncProblem::Store`] too.
pub fn sync_overview(
    services: &Services,
    vault: &dyn SyncVault,
) -> Result<SyncOverview, SyncProblem> {
    let set_up = vault.has()?;
    let recording = services.recording_meetings();
    let store = services.store();
    let settings = sync_settings(&store)?;
    Ok(SyncOverview {
        set_up,
        status: sync_status(&store)?,
        // What the push would send now: a recording's rows and switched-off kinds are not
        // behind.
        waiting: store.outbox_len_except(&recording, settings.kinds())?,
        held: u64::try_from(store.held_rows()?.len()).unwrap_or(u64::MAX),
        settings,
    })
}

/// One device of the vault, as the server knows it, with its presence when it sent one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncDevice {
    /// Its id.
    pub device: earshot_core::identity::DeviceId,
    /// Whether it is this device.
    pub this_device: bool,
    /// Its name, from its presence (`None` until it sends one this device can read).
    pub name: Option<String>,
    /// Whether it is online now ([`online`]).
    pub online: bool,
    /// Whether it is recording now (from its presence).
    pub recording: bool,
    /// Whether it writes summaries for the other devices (from its presence).
    pub offers_summaries: bool,
    /// The kind of writer it would use, when it offers summaries and this build knows the
    /// kind (from its presence; summary offload names it so the user knows where text goes).
    pub writer_kind: Option<crate::offload::WriterKind>,
    /// Where its own local model runs, when it is a summary worker whose writer kind is its
    /// local model and it said (from its presence, T11.6a); `None` otherwise.
    pub model_place: Option<ModelPlace>,
    /// The first eight hex digits of its id: enough to tell two devices apart on the page.
    pub short_id: String,
    /// When it joined (Unix seconds, the server's clock).
    pub joined_unix_s: u64,
    /// Seconds since it last called the server.
    pub seen_s_ago: u64,
}

/// The vault's devices, from the server, this device first, each with its presence (name,
/// online, recording, offers summaries) when it has sent one this device can read; a device
/// without one is shown by its short id.
///
/// # Errors
/// [`SyncProblem::Vault`] when this device is not set up; the relay's problems.
pub fn vault_devices(
    services: &Services,
    vault: std::sync::Arc<dyn SyncVault>,
) -> Result<Vec<SyncDevice>, SyncProblem> {
    let secrets = vault.load()?.ok_or(SyncProblem::Vault)?;
    let client = SyncClient::new(services.clone(), vault)?;
    let me = *client.device().as_uuid().as_bytes();
    let answer = client.relay().vault(&secrets.token)?;
    let peers = client.peers()?;
    let mut devices: Vec<SyncDevice> = answer
        .devices
        .into_iter()
        .map(|d| {
            let peer = peers
                .iter()
                .find(|p| p.device.as_uuid().as_bytes()[..] == d.device_id.0[..]);
            let presence = peer.and_then(|p| p.presence.as_ref());
            let id: [u8; 16] = d.device_id.0.as_slice().try_into().unwrap_or([0; 16]);
            SyncDevice {
                device: earshot_core::identity::DeviceId::from_bytes(id),
                this_device: d.device_id.0 == me,
                name: presence.map(|p| p.name.clone()).filter(|n| !n.is_empty()),
                online: peer.is_some_and(|p| p.online),
                recording: presence.is_some_and(|p| p.recording),
                offers_summaries: presence.is_some_and(|p| p.offers_summaries),
                writer_kind: presence
                    .filter(|p| p.offers_summaries)
                    .and_then(|p| p.writer_kind.as_deref())
                    .and_then(crate::offload::WriterKind::parse),
                model_place: presence.and_then(model_place_of),
                short_id: d
                    .device_id
                    .0
                    .iter()
                    .take(4)
                    .map(|b| format!("{b:02x}"))
                    .collect(),
                joined_unix_s: d.created_unix_s,
                seen_s_ago: d.age_s,
            }
        })
        .collect();
    devices.sort_by_key(|d| (!d.this_device, d.joined_unix_s));
    Ok(devices)
}

/// Where a device's own local model runs, from its presence: only a summary worker's, only
/// while it offers summaries with its local model, and only a word this build knows.
fn model_place_of(p: &Presence) -> Option<ModelPlace> {
    let local = p.writer_kind.as_deref() == Some(crate::offload::WriterKind::Local.word());
    (p.kind == KIND_SERVER && p.offers_summaries && local)
        .then(|| p.model_place.as_deref().and_then(ModelPlace::parse))
        .flatten()
}

/// The stored status (the default when none is stored or it cannot be read).
///
/// # Errors
/// [`SyncProblem::Store`].
pub fn sync_status(store: &Store) -> Result<SyncStatus, SyncProblem> {
    Ok(store
        .local_value(SYNC_STATUS_KEY)?
        .and_then(|b| {
            let (v, payload) = split_version(&b)?;
            (v == STATUS_VERSION)
                .then(|| serde_json::from_slice(payload).ok())
                .flatten()
        })
        .unwrap_or_default())
}

fn set_sync_status(store: &mut Store, status: &SyncStatus) -> Result<(), SyncProblem> {
    let bytes = encode_versioned(STATUS_VERSION, status).expect("a status serialises");
    local::set(store, &DeviceRecord::SyncStatus.key(), &bytes)?;
    Ok(())
}

/// Records `problem` as the last tick's outcome, keeping the last success and the counts.
fn record_problem(services: &Services, problem: SyncProblem) {
    let mut db = services.store();
    if let Ok(mut status) = sync_status(&db) {
        status.problem = Some(problem);
        let _ = set_sync_status(&mut db, &status);
    }
}

#[cfg(test)]
#[path = "../sync_tests.rs"]
mod tests;
