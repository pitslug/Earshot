//! The relay's JSON calls (docs/sync-spec.md section 3), shared by the sync client and
//! `earshot-sync-server` so the two can only drift in one commit.
//!
//! Binary fields are standard padded base64 ([`B64`]). Nothing here is secret except
//! [`RegisterRequest::setup_secret`] and [`RegisterResponse::token`], whose `Debug` shows
//! nothing.

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// The header carrying the highest envelope format the calling device writes.
pub const FORMAT_HEADER: &str = "Earshot-Sync-Format";

/// The largest decoded row body the relay accepts (the client seals at most
/// [`crate::MAX_PLAINTEXT`], so a sealed row always fits).
pub const MAX_ROW_BODY: usize = 256 * 1024;

/// The largest request body the relay accepts.
pub const MAX_REQUEST: usize = 4 * 1024 * 1024;

/// The largest decoded presence blob the relay accepts.
pub const MAX_PRESENCE: usize = 16 * 1024;

/// Rows per pull when the client names no limit, and the most it may name.
pub const PULL_DEFAULT: u32 = 200;
/// See [`PULL_DEFAULT`].
pub const PULL_MAX: u32 = 500;

/// How long a pairing slot lives, in seconds.
pub const SLOT_TTL_S: u64 = 600;

/// How far past the relay's own clock a pushed row's stamp may be before the push is refused
/// ([`ErrorWord::ClockAhead`], T14.44). 24 hours, a judgement: wide enough for any time zone
/// or daylight-saving mistake and for clocks that drift, far too narrow for a clock set to the
/// wrong day, month or year. It equals the window a device holds a pulled row by
/// (`earshot_store::MAX_CLOCK_AHEAD_MS`, pinned to this by a runtime test): the relay keeps
/// the copy with the latest stamp, so one row stamped in a far future would shadow every
/// later edit of it from a computer whose clock is right.
pub const MAX_STAMP_AHEAD_MS: u64 = 24 * 60 * 60 * 1000;

/// Bytes sent as standard padded base64.
#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Default)]
pub struct B64(pub Vec<u8>);

impl std::fmt::Debug for B64 {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "B64({} bytes)", self.0.len())
    }
}

impl Serialize for B64 {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&STANDARD.encode(&self.0))
    }
}

impl<'de> Deserialize<'de> for B64 {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        STANDARD
            .decode(s.as_bytes())
            .map(B64)
            .map_err(|_| serde::de::Error::custom("not standard base64"))
    }
}

/// `POST /v1/devices`: register with the setup secret or a taken pairing slot.
#[derive(Clone, Serialize, Deserialize)]
pub struct RegisterRequest {
    /// The device's id (16 bytes).
    pub device_id: B64,
    /// [`crate::Keys::key_check`] (32 bytes).
    pub key_check: B64,
    /// The container's setup secret (the first device, or any device the owner sets up).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub setup_secret: Option<String>,
    /// A pairing slot this device has taken ([`crate::PairingCode::slot`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub slot: Option<B64>,
}

impl std::fmt::Debug for RegisterRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RegisterRequest")
            .field("setup_secret", &self.setup_secret.as_ref().map(|_| ".."))
            .field("slot", &self.slot.as_ref().map(|_| ".."))
            .finish_non_exhaustive()
    }
}

/// The answer to [`RegisterRequest`].
#[derive(Clone, Serialize, Deserialize)]
pub struct RegisterResponse {
    /// The device's bearer token. Shown once; the relay keeps only its SHA-256.
    pub token: String,
}

impl std::fmt::Debug for RegisterResponse {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("RegisterResponse(..)")
    }
}

/// `POST /v1/pairing`: offer the wrapped vault key in a slot.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PairingRequest {
    /// The slot ([`crate::PairingCode::slot`]).
    pub slot: B64,
    /// [`crate::PairingCode::wrap`]'s output.
    pub wrapped: B64,
}

/// The answer to [`PairingRequest`].
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct PairingResponse {
    /// Seconds until the slot expires.
    pub expires_in_s: u64,
}

/// `POST /v1/pairing/take`: fetch a slot's wrapped key, once.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TakeRequest {
    /// The slot.
    pub slot: B64,
}

/// The answer to [`TakeRequest`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TakeResponse {
    /// The wrapped vault key.
    pub wrapped: B64,
}

/// A sealed row as pushed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WireRow {
    /// The row key (16 bytes).
    pub key: B64,
    /// The row's `updated_at`: wall clock.
    pub wall_ms: u64,
    /// The row's `updated_at`: logical counter.
    pub logical: u32,
    /// The row's `updated_at`: node (16 bytes).
    pub node: B64,
    /// The sealed body.
    pub body: B64,
}

/// `POST /v1/rows`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PushRequest {
    /// The highest envelope format the device writes.
    pub format: u32,
    /// The rows.
    pub rows: Vec<WireRow>,
}

/// The answer to [`PushRequest`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PushResponse {
    /// Rows stored.
    pub accepted: u32,
    /// Keys of rows the relay already held at the same or a later stamp.
    pub superseded: Vec<B64>,
    /// The vault's highest sequence number.
    pub seq: u64,
}

/// A row as pulled.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PulledRow {
    /// The relay's sequence number for it.
    pub seq: u64,
    /// The row key.
    pub key: B64,
    /// `updated_at`: wall clock.
    pub wall_ms: u64,
    /// `updated_at`: logical counter.
    pub logical: u32,
    /// `updated_at`: node.
    pub node: B64,
    /// The sealed body.
    pub body: B64,
}

/// The answer to `GET /v1/rows?after=<seq>&limit=<n>`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PullResponse {
    /// Rows after the cursor, ascending.
    pub rows: Vec<PulledRow>,
    /// Whether another page exists.
    pub more: bool,
    /// The vault's format.
    pub vault_format: u32,
}

/// `POST /v1/presence`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PresenceRequest {
    /// The sealed presence blob.
    pub blob: B64,
}

/// One device's presence.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PresenceEntry {
    /// The device.
    pub device_id: B64,
    /// Seconds since the relay received its blob, by the relay's clock.
    pub age_s: u64,
    /// The sealed blob.
    pub blob: B64,
}

/// The answer to `GET /v1/presence`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PresenceResponse {
    /// Every registered device that has sent a blob.
    pub devices: Vec<PresenceEntry>,
}

/// One registered device.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VaultDevice {
    /// The device.
    pub device_id: B64,
    /// When it registered (Unix seconds, the relay's clock).
    pub created_unix_s: u64,
    /// Seconds since it last called the relay.
    pub age_s: u64,
}

/// The answer to `GET /v1/vault`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VaultResponse {
    /// The vault's format.
    pub vault_format: u32,
    /// Registered devices that are not revoked.
    pub devices: Vec<VaultDevice>,
}

/// `POST /v1/devices/revoke`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RevokeRequest {
    /// The device to revoke (the caller itself for "Remove this computer").
    pub device_id: B64,
}

/// The answer to `GET /v1/health` (no auth; for checking a deployment by hand).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HealthResponse {
    /// Always true when the relay answers.
    pub ok: bool,
    /// The relay's version (the workspace version).
    pub version: String,
}

/// An error answer: `{"error": <word>}`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ErrorBody {
    /// [`ErrorWord::word`].
    pub error: String,
}

/// Every error the relay answers with. The client maps each to fixed copy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorWord {
    /// 400: the request is not what the call takes.
    BadRequest,
    /// 401: no token, or a token the relay does not know or has revoked.
    UnknownDevice,
    /// 403: the setup secret is wrong.
    BadSecret,
    /// 404: no such pairing slot (unknown, expired or used).
    NoSlot,
    /// 404: no such call (an unknown path or method).
    NotFound,
    /// 409: the device's key check is not the vault's: join with a pairing code.
    KeyMismatch,
    /// 409: the vault holds rows of a newer format than the device writes: update Earshot.
    UpdateNeeded,
    /// 409: a pairing code cannot register a device id that is registered and not revoked (a
    /// revoked one comes back with a code); only the setup secret re-registers a live one.
    DeviceExists,
    /// 409: a pushed row is stamped more than [`MAX_STAMP_AHEAD_MS`] past the relay's clock
    /// (T14.44); nothing of the push is stored. A client from before this word existed reads
    /// it as an answer it does not understand: a failed push it retries, its rows still queued.
    ClockAhead,
    /// 413: over a size limit.
    TooLarge,
    /// 429: too many failed attempts from this address; wait a minute.
    SlowDown,
    /// 500: the relay's own problem (its database).
    Internal,
}

impl ErrorWord {
    /// Every word, for tests and the client's copy table.
    pub const ALL: [Self; 12] = [
        Self::BadRequest,
        Self::UnknownDevice,
        Self::BadSecret,
        Self::NoSlot,
        Self::NotFound,
        Self::KeyMismatch,
        Self::UpdateNeeded,
        Self::DeviceExists,
        Self::ClockAhead,
        Self::TooLarge,
        Self::SlowDown,
        Self::Internal,
    ];

    /// The word on the wire.
    #[must_use]
    pub const fn word(self) -> &'static str {
        match self {
            Self::BadRequest => "bad_request",
            Self::UnknownDevice => "unknown_device",
            Self::BadSecret => "bad_secret",
            Self::NoSlot => "no_slot",
            Self::NotFound => "not_found",
            Self::KeyMismatch => "key_mismatch",
            Self::UpdateNeeded => "update_needed",
            Self::DeviceExists => "device_exists",
            Self::ClockAhead => "clock_ahead",
            Self::TooLarge => "too_large",
            Self::SlowDown => "slow_down",
            Self::Internal => "internal",
        }
    }

    /// The HTTP status it comes with.
    #[must_use]
    pub const fn status(self) -> u16 {
        match self {
            Self::BadRequest => 400,
            Self::UnknownDevice => 401,
            Self::BadSecret => 403,
            Self::NoSlot | Self::NotFound => 404,
            Self::KeyMismatch | Self::UpdateNeeded | Self::DeviceExists | Self::ClockAhead => 409,
            Self::TooLarge => 413,
            Self::SlowDown => 429,
            Self::Internal => 500,
        }
    }

    /// The word for a wire word, `None` for one this build does not know.
    #[must_use]
    pub fn from_word(word: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|w| w.word() == word)
    }
}
