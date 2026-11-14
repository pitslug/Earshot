//! The client half of `sync.no_audio` (PLAN 4.2, docs/sync-spec.md section 2.3): every
//! plaintext is checked before it is sealed, because the relay cannot read what it stores.
//!
//! The type system is the main guard: only `SyncPermitted` classes implement `SyncSerialize`,
//! so audio rows cannot reach [`crate::seal_row`] at all. This check is the second line, for
//! audio smuggled into a field of a synced row. JSON text cannot hold raw audio bytes (a NUL
//! must be escaped), so audio would arrive encoded: as a number array (serde's encoding of
//! `Vec<u8>`), as base64, or as the raw signature when a field holds text. Each form of each
//! container's magic is refused, plus anything over [`MAX_PLAINTEXT`].
//!
//! One deliberate narrowing of the spec's "`RIFF` at any offset": a raw `RIFF` counts only
//! with `WAVE` four bytes later, as a WAV header has it, because "RIFF" is an English word a
//! transcript can hold, and a refused row never syncs.

/// The largest plaintext sealed (192 KiB): the relay's 256 KiB row limit always holds the
/// seal. No synced row comes near it; audio would.
pub const MAX_PLAINTEXT: usize = 192 * 1024;

/// Why a plaintext was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlaintextRefused {
    /// Over [`MAX_PLAINTEXT`].
    TooLarge,
    /// It holds an audio container's signature in one of the forms in the module docs.
    AudioSignature,
}

impl PlaintextRefused {
    /// A fixed word for status lines and counts.
    #[must_use]
    pub const fn word(self) -> &'static str {
        match self {
            Self::TooLarge => "too_large",
            Self::AudioSignature => "audio_signature",
        }
    }
}

/// Raw signatures (FLAC, Ogg) refused anywhere.
const RAW: &[&[u8]] = &[b"fLaC", b"OggS"];

/// The same magics as serde_json writes a `Vec<u8>`: comma-separated decimal bytes.
const NUMBER_ARRAYS: &[&[u8]] = &[
    b"102,76,97,67",  // fLaC
    b"79,103,103,83", // OggS
    b"82,73,70,70",   // RIFF
];

/// The same magics base64-encoded at the start of an encoded value (the first five
/// characters depend only on the first four bytes when they start a 3-byte group).
const BASE64: &[&[u8]] = &[b"ZkxhQ", b"T2dnU", b"UklGR"];

/// Refuses a plaintext that is too large or holds an audio signature (module docs).
///
/// # Errors
/// The reason.
pub fn check_plaintext(plaintext: &[u8]) -> Result<(), PlaintextRefused> {
    if plaintext.len() > MAX_PLAINTEXT {
        return Err(PlaintextRefused::TooLarge);
    }
    let holds = |needle: &[u8]| plaintext.windows(needle.len()).any(|w| w == needle);
    let wav = plaintext
        .windows(12)
        .any(|w| &w[..4] == b"RIFF" && &w[8..] == b"WAVE");
    if wav
        || RAW
            .iter()
            .chain(NUMBER_ARRAYS)
            .chain(BASE64)
            .any(|n| holds(n))
    {
        return Err(PlaintextRefused::AudioSignature);
    }
    Ok(())
}
