//! Sync encryption (T11.2a; docs/sync-spec.md section 2, the design in docs/sync.md section 4).
//!
//! Everything a device needs to talk to the sync relay without the relay learning anything:
//! - [`VaultKey`]: the 32 random bytes a vault's devices share, and the [`Keys`] derived from
//!   it with HKDF-SHA256 (RFC 5869);
//! - [`seal_row`] and [`open_row`]: a synced row's [`earshot_core::identity::SyncEnvelope`],
//!   sealed with XChaCha20-Poly1305 under its row key and `updated_at`, so the relay cannot move
//!   a ciphertext to another row or stamp;
//! - [`Keys::row_key`]: the 16 bytes the relay files a row under (HMAC-SHA256 of the entity
//!   id), so it does not learn creation times from UUIDv7 ids;
//! - [`Keys::key_check`]: proof two devices hold the same key, without the key;
//! - [`PairingCode`]: a 128-bit code that wraps the vault key for a joining device;
//! - [`check_plaintext`]: the client half of `sync.no_audio` (no audio, nothing oversized);
//! - [`wire`]: the relay's JSON calls, shared with `earshot-sync-server`.
//!
//! No network, no files, no store. Keys and codes have redacted `Debug` and are wiped on drop.

#![forbid(unsafe_code)]

mod guard;
mod keys;
mod pairing;
mod seal;
pub mod wire;

#[cfg(test)]
mod tests;

pub use guard::{MAX_PLAINTEXT, PlaintextRefused, check_plaintext};
pub use keys::{KEY_CHECK_LEN, Keys, ROW_KEY_LEN, RowKey, VaultKey};
pub use pairing::{BadCode, PairingCode, SLOT_LEN};
pub use seal::{
    ENVELOPE_FORMAT, Opened, OpenedRow, SealedRow, open_presence, open_row, seal_presence, seal_row,
};

/// Why sealed bytes could not be opened. A fixed reason, never the bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unreadable {
    /// Not a format this build reads (the leading byte), or too short to hold a seal.
    Version,
    /// The authentication tag did not match: another key, row key, stamp or a changed byte.
    Tag,
    /// The plaintext is not the JSON this build expects.
    Json,
    /// The envelope's id does not hash to the row key it was filed under.
    Id,
    /// The envelope's `updated_at` is not the stamp it was filed under.
    Stamp,
}

impl Unreadable {
    /// A fixed word for status lines and counts.
    #[must_use]
    pub const fn word(self) -> &'static str {
        match self {
            Self::Version => "version",
            Self::Tag => "tag",
            Self::Json => "json",
            Self::Id => "id",
            Self::Stamp => "stamp",
        }
    }
}

/// Why something could not be sealed or made.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SealError {
    /// The operating system's generator failed.
    Random,
    /// The row could not be serialised.
    Json,
    /// The plaintext was refused before sealing ([`check_plaintext`]).
    Refused(PlaintextRefused),
}

impl std::fmt::Display for SealError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Random => f.write_str("the system's random number generator failed"),
            Self::Json => f.write_str("the row could not be serialised"),
            Self::Refused(r) => write!(f, "the row was refused before sealing: {}", r.word()),
        }
    }
}

impl std::error::Error for SealError {}

impl std::fmt::Display for Unreadable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "sealed data could not be read ({})", self.word())
    }
}

impl std::error::Error for Unreadable {}

/// Fills `buf` from the operating system's generator.
pub(crate) fn random(buf: &mut [u8]) -> Result<(), SealError> {
    getrandom::fill(buf).map_err(|_| SealError::Random)
}
