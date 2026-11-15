//! Pairing codes (docs/sync-spec.md section 2.5).
//!
//! A code is 16 random bytes, shown as 26 Crockford base32 characters
//! (<https://www.crockford.com/base32.html>) in groups of 5, 5, 5, 5 and 6. The joining device
//! fetches the wrapped vault key from the slot the code names:
//!
//! ```text
//! slot    = HKDF-SHA256(code, "earshot-sync v1 pair-slot")[0..16]
//! k_wrap  = HKDF-SHA256(code, "earshot-sync v1 pair-wrap")
//! wrapped = 0x01 | nonce (24) | XChaCha20-Poly1305(k_wrap, nonce, vault key, "earshot-pair-v1")
//! ```
//!
//! The code carries 128 bits, so the relay cannot recover the key from the slot and the
//! wrapped bytes, and no password stretching is needed.

use zeroize::Zeroize;

use crate::keys::{VaultKey, derive32};
use crate::seal::{open_bytes, seal_bytes};
use crate::{SealError, Unreadable};

/// Length of a pairing slot id.
pub const SLOT_LEN: usize = 16;

const CODE_BYTES: usize = 16;
/// 128 bits in 5-bit characters, the last carrying 3 bits and 2 zero bits.
const CODE_CHARS: usize = 26;
const GROUPS: [usize; 5] = [5, 5, 5, 5, 6];
const ALPHABET: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";
const INFO_SLOT: &[u8] = b"earshot-sync v1 pair-slot";
const INFO_WRAP: &[u8] = b"earshot-sync v1 pair-wrap";
const WRAP_AAD: &[u8] = b"earshot-pair-v1";

/// Why typed text is not a pairing code. Says nothing about the text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BadCode {
    /// Not 26 characters once dashes and spaces are removed.
    Length,
    /// A character outside Crockford's base32 (`U` included: Crockford leaves it out).
    Character,
    /// The last character's two spare bits are not zero: a mistyped last character.
    Padding,
}

/// A pairing code. `Debug` shows nothing; wiped on drop. Shown to the user with `Display`.
pub struct PairingCode([u8; CODE_BYTES]);

impl PairingCode {
    /// A new code from the operating system's generator.
    ///
    /// # Errors
    /// [`SealError::Random`].
    pub fn generate() -> Result<Self, SealError> {
        let mut b = [0u8; CODE_BYTES];
        crate::random(&mut b)?;
        Ok(Self(b))
    }

    /// The code typed by a person: case does not matter, dashes and spaces are ignored, and
    /// `I`/`L` read as `1` and `O` as `0`, as Crockford decodes them.
    ///
    /// # Errors
    /// [`BadCode`].
    pub fn parse(text: &str) -> Result<Self, BadCode> {
        let mut values = Vec::with_capacity(CODE_CHARS);
        for c in text.chars() {
            if c == '-' || c.is_whitespace() {
                continue;
            }
            let v = match c.to_ascii_uppercase() {
                'I' | 'L' => 1,
                'O' => 0,
                u => ALPHABET
                    .iter()
                    .position(|&a| char::from(a) == u)
                    .ok_or(BadCode::Character)?,
            };
            values.push(u8::try_from(v).expect("an alphabet index fits a byte"));
        }
        if values.len() != CODE_CHARS {
            return Err(BadCode::Length);
        }
        // Big-endian bit stream, 5 bits per character.
        let mut out = [0u8; CODE_BYTES];
        let (mut acc, mut bits, mut n) = (0u32, 0u32, 0usize);
        for (i, v) in values.iter().enumerate() {
            acc = (acc << 5) | u32::from(*v);
            bits += 5;
            if i == CODE_CHARS - 1 {
                // The last character: 3 data bits, then 2 that must be zero.
                if acc & 0b11 != 0 {
                    return Err(BadCode::Padding);
                }
                acc >>= 2;
                bits -= 2;
            }
            while bits >= 8 {
                bits -= 8;
                out[n] = u8::try_from((acc >> bits) & 0xff).expect("masked to a byte");
                n += 1;
            }
            acc &= (1 << bits) - 1;
        }
        values.zeroize();
        debug_assert_eq!(n, CODE_BYTES);
        Ok(Self(out))
    }

    /// The slot the relay keeps the wrapped key in.
    #[must_use]
    pub fn slot(&self) -> [u8; SLOT_LEN] {
        let mut full = derive32(&self.0, INFO_SLOT);
        let mut s = [0u8; SLOT_LEN];
        s.copy_from_slice(&full[..SLOT_LEN]);
        full.zeroize();
        s
    }

    /// `key` wrapped for the joining device.
    ///
    /// # Errors
    /// [`SealError::Random`].
    pub fn wrap(&self, key: &VaultKey) -> Result<Vec<u8>, SealError> {
        let mut k = derive32(&self.0, INFO_WRAP);
        let out = seal_bytes(&k, key.as_bytes(), WRAP_AAD);
        k.zeroize();
        out
    }

    /// The vault key from `wrapped`.
    ///
    /// # Errors
    /// [`Unreadable::Version`], [`Unreadable::Tag`] (another code), or [`Unreadable::Json`]
    /// when the plaintext is not 32 bytes.
    pub fn unwrap_key(&self, wrapped: &[u8]) -> Result<VaultKey, Unreadable> {
        let mut k = derive32(&self.0, INFO_WRAP);
        let opened = open_bytes(&k, wrapped, WRAP_AAD);
        k.zeroize();
        let mut plain = opened?;
        let bytes: Result<[u8; 32], _> = plain.as_slice().try_into();
        plain.zeroize();
        bytes
            .map(VaultKey::from_bytes)
            .map_err(|_| Unreadable::Json)
    }
}

impl std::fmt::Display for PairingCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut chars = Vec::with_capacity(CODE_CHARS);
        let (mut acc, mut bits) = (0u32, 0u32);
        for b in self.0 {
            acc = (acc << 8) | u32::from(b);
            bits += 8;
            while bits >= 5 {
                bits -= 5;
                chars.push(ALPHABET[((acc >> bits) & 0x1f) as usize]);
            }
            acc &= (1 << bits) - 1;
        }
        // 128 = 25 * 5 + 3: the last character holds 3 bits and 2 zero bits.
        chars.push(ALPHABET[((acc << (5 - bits)) & 0x1f) as usize]);
        let mut start = 0;
        for (i, len) in GROUPS.iter().enumerate() {
            if i > 0 {
                f.write_str("-")?;
            }
            let group = std::str::from_utf8(&chars[start..start + len]).expect("ASCII alphabet");
            f.write_str(group)?;
            start += len;
        }
        chars.zeroize();
        Ok(())
    }
}

impl std::fmt::Debug for PairingCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("PairingCode(..)")
    }
}

impl Drop for PairingCode {
    fn drop(&mut self) {
        self.0.zeroize();
    }
}
