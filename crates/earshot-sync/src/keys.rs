//! The vault key and the keys derived from it (docs/sync-spec.md sections 2.1, 2.2 and 2.4).

use earshot_core::identity::EntityId;
use hkdf::Hkdf;
use hmac::{Hmac, Mac};
use sha2::Sha256;
use zeroize::Zeroize;

use crate::SealError;

/// Length of a [`RowKey`].
pub const ROW_KEY_LEN: usize = 16;

/// Length of a key check ([`Keys::key_check`]).
pub const KEY_CHECK_LEN: usize = 32;

/// HKDF `info` labels (section 2.1). Changing one makes every vault unreadable.
pub(crate) const INFO_ROW: &[u8] = b"earshot-sync v1 row";
pub(crate) const INFO_ID: &[u8] = b"earshot-sync v1 row-key";
pub(crate) const INFO_PRESENCE: &[u8] = b"earshot-sync v1 presence";
pub(crate) const INFO_CHECK: &[u8] = b"earshot-sync v1 check";

/// The message the key check is computed over.
const CHECK_MESSAGE: &[u8] = b"earshot-vault";

type HmacSha256 = Hmac<Sha256>;

/// HKDF-SHA256 with an empty salt: 32 bytes for `info` from `ikm`.
pub(crate) fn derive32(ikm: &[u8], info: &[u8]) -> [u8; 32] {
    let mut out = [0u8; 32];
    Hkdf::<Sha256>::new(None, ikm)
        .expand(info, &mut out)
        .expect("32 bytes is a valid HKDF-SHA256 length");
    out
}

/// HMAC-SHA256 of `message` under `key`.
pub(crate) fn hmac(key: &[u8; 32], message: &[u8]) -> [u8; 32] {
    let mut mac = HmacSha256::new_from_slice(key).expect("HMAC takes a key of any length");
    mac.update(message);
    mac.finalize().into_bytes().into()
}

/// The 32 random bytes a vault's devices share. Never sent to the relay unwrapped.
///
/// Not `Clone`; `Debug` shows no bytes; wiped on drop.
pub struct VaultKey([u8; 32]);

impl VaultKey {
    /// A new key from the operating system's generator.
    ///
    /// # Errors
    /// [`SealError::Random`] when the generator fails.
    pub fn generate() -> Result<Self, SealError> {
        let mut k = [0u8; 32];
        crate::random(&mut k)?;
        Ok(Self(k))
    }

    /// A key from stored bytes (Credential Manager, or unwrapped from a pairing slot).
    #[must_use]
    pub fn from_bytes(mut bytes: [u8; 32]) -> Self {
        let key = Self(bytes);
        bytes.zeroize();
        key
    }

    /// The key's bytes, for storing it. Keep the borrow short.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    /// The keys derived from this one.
    #[must_use]
    pub fn keys(&self) -> Keys {
        Keys {
            row: derive32(&self.0, INFO_ROW),
            id: derive32(&self.0, INFO_ID),
            presence: derive32(&self.0, INFO_PRESENCE),
            check: derive32(&self.0, INFO_CHECK),
        }
    }
}

impl Drop for VaultKey {
    fn drop(&mut self) {
        self.0.zeroize();
    }
}

impl std::fmt::Debug for VaultKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("VaultKey(..)")
    }
}

/// The keys derived from a [`VaultKey`] (section 2.1). `Debug` shows no bytes; wiped on drop.
pub struct Keys {
    pub(crate) row: [u8; 32],
    pub(crate) id: [u8; 32],
    pub(crate) presence: [u8; 32],
    check: [u8; 32],
}

impl Keys {
    /// The key the relay files `id`'s row under: HMAC-SHA256 of the id's bytes, first 16
    /// (section 2.2). Entity ids are unique across tables, so the entity name is not part of it.
    #[must_use]
    pub fn row_key(&self, id: EntityId) -> RowKey {
        let mac = hmac(&self.id, id.as_uuid().as_bytes());
        let mut k = [0u8; ROW_KEY_LEN];
        k.copy_from_slice(&mac[..ROW_KEY_LEN]);
        RowKey(k)
    }

    /// Proof of holding the vault key, without it: HMAC-SHA256 of `"earshot-vault"` under the
    /// check key (section 2.4). The relay stores the first and refuses a device with another.
    #[must_use]
    pub fn key_check(&self) -> [u8; KEY_CHECK_LEN] {
        hmac(&self.check, CHECK_MESSAGE)
    }
}

impl Drop for Keys {
    fn drop(&mut self) {
        self.row.zeroize();
        self.id.zeroize();
        self.presence.zeroize();
        self.check.zeroize();
    }
}

impl std::fmt::Debug for Keys {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Keys(..)")
    }
}

/// The 16 bytes the relay files a row under. Not secret: the relay sees it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct RowKey(pub [u8; ROW_KEY_LEN]);
