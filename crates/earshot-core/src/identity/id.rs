//! Entity and device identifiers.

use core::fmt;
use core::str::FromStr;

use rand_core::Rng;
use serde::{Deserialize, Serialize};
use uuid::{Builder, Uuid};

use super::clock::Clock;

/// Identifier of a syncable row: a UUIDv7 (RFC 9562), so ids sort by creation time to the
/// millisecond.
///
/// Built with `uuid::Builder::from_unix_timestamp_millis` from the injected [`Clock`] and
/// 10 bytes from a caller-supplied RNG; the `uuid` crate's `v7`/`rng` features are not
/// used because they pull in `getrandom` (PLAN.md 3.2).
///
/// **Ordering:** ids created in the same millisecond are ordered by their random bits, not
/// by creation (RFC 9562 method 3; no 12-bit counter, because `generate` is stateless and
/// ids from several devices interleave anyway). Creation order is the row's
/// `SyncMeta::created_at` (a hybrid logical clock, strictly increasing per device); sort by
/// that, with the id only as a tiebreak. Ordering by id alone is a stable, arbitrary order
/// within a millisecond.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct EntityId(Uuid);

impl EntityId {
    /// A new UUIDv7 stamped with `clock`'s current time; the remaining 74 bits come from
    /// `rng` (a seeded RNG in tests, an OS-seeded one in adapters).
    pub fn generate<C: Clock + ?Sized, R: Rng + ?Sized>(clock: &C, rng: &mut R) -> Self {
        let mut bytes = [0u8; 10];
        rng.fill_bytes(&mut bytes);
        EntityId(Builder::from_unix_timestamp_millis(clock.now_unix_ms(), &bytes).into_uuid())
    }

    /// Wraps raw bytes (for decoding stored ids). Does not check the version.
    pub const fn from_bytes(bytes: [u8; 16]) -> Self {
        EntityId(Uuid::from_bytes(bytes))
    }

    /// The underlying UUID.
    pub const fn as_uuid(&self) -> &Uuid {
        &self.0
    }

    /// The Unix millisecond timestamp in the first 48 bits.
    pub fn timestamp_ms(&self) -> u64 {
        let b = self.0.as_bytes();
        b[..6]
            .iter()
            .fold(0u64, |acc, &x| (acc << 8) | u64::from(x))
    }
}

impl fmt::Debug for EntityId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "EntityId({})", self.0)
    }
}

impl fmt::Display for EntityId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.0.hyphenated(), f)
    }
}

impl FromStr for EntityId {
    type Err = uuid::Error;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Uuid::parse_str(s).map(EntityId)
    }
}

/// JSON Schema of the serialised form: a UUID string (used by `schemas/summary.v1.json`).
impl schemars::JsonSchema for EntityId {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "EntityId".into()
    }

    fn inline_schema() -> bool {
        true
    }

    fn json_schema(_generator: &mut schemars::SchemaGenerator) -> schemars::Schema {
        schemars::json_schema!({
            "type": "string",
            "format": "uuid"
        })
    }
}

/// Identifier of a device (one per install), the `origin_device_id` of rows it creates
/// and the tiebreak of its [`super::HlcTimestamp`]s.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct DeviceId(Uuid);

impl DeviceId {
    /// Wraps raw bytes.
    pub const fn from_bytes(bytes: [u8; 16]) -> Self {
        DeviceId(Uuid::from_bytes(bytes))
    }

    /// A new random device id from `rng` (UUIDv7 stamped with `clock`).
    pub fn generate<C: Clock + ?Sized, R: Rng + ?Sized>(clock: &C, rng: &mut R) -> Self {
        DeviceId(*EntityId::generate(clock, rng).as_uuid())
    }

    /// The underlying UUID.
    pub const fn as_uuid(&self) -> &Uuid {
        &self.0
    }
}

impl fmt::Debug for DeviceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "DeviceId({})", self.0)
    }
}

impl fmt::Display for DeviceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.0.hyphenated(), f)
    }
}
