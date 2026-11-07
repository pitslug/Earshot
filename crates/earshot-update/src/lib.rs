//! The signed version file of an in-app update (T9.1b, D24): what a release says about itself and
//! how the app knows the owner said it. No network, no files.
//!
//! The file is `{"manifest": "<JSON text>", "signature": "<hex>"}`. The signature is Ed25519 over
//! [`DOMAIN`] followed by the manifest's exact bytes, so nothing is parsed before it is trusted,
//! and the signature cannot be reused for anything else the key might one day sign. Every field
//! of a [`Release`] is checked after that: [`verify`] returns only a release of this product whose
//! installer name follows from its version and architecture, which leaves nothing in the file
//! able to choose a host or a path.

use std::cmp::Ordering;
use std::fmt;
use std::str::FromStr;

use ed25519_dalek::{Signature, VerifyingKey};
use serde::{Deserialize, Serialize};

/// The public key of the owner's release key, built into the app. `None` until one is generated
/// (`cargo xtask update-key`): such a build says updates are not set up and `sign-update` refuses
/// to make a file for it. Changing this leaves installed copies unable to verify later releases.
pub const RELEASE_PUBLIC_KEY: Option<[u8; 32]> = None;

/// What the signature covers before the manifest.
pub const DOMAIN: &[u8] = b"earshot-update-v1\n";
/// [`Release::product`].
pub const PRODUCT: &str = "earshot";
/// The largest version file the app reads.
pub const MAX_FILE_BYTES: u64 = 16 * 1024;
/// The largest installer a release may announce (today's is about 25 MB).
pub const MAX_INSTALLER_BYTES: u64 = 2 * 1024 * 1024 * 1024;

/// What a release says about itself.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Release {
    /// Always [`PRODUCT`].
    pub product: String,
    /// `x64` or `arm64`.
    pub arch: String,
    /// `major.minor.patch`.
    pub version: String,
    /// The installer's file name: [`installer_name`] of the two above.
    pub installer: String,
    /// The installer's size in bytes.
    pub size: u64,
    /// The installer's SHA-256, lower-case hex.
    pub sha256: String,
}

/// The file as it travels.
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SignedFile {
    manifest: String,
    signature: String,
}

/// Why a version file is refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VerifyError {
    /// Not the expected JSON, or a signature that is not 64 bytes of hex.
    Malformed,
    /// The signature does not match, or the key is not a usable key.
    BadSignature,
    /// Signed, but its contents break a rule: which one.
    BadManifest(&'static str),
}

impl fmt::Display for VerifyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Malformed => f.write_str("the version file is malformed"),
            Self::BadSignature => f.write_str("the version file's signature does not match"),
            Self::BadManifest(why) => write!(f, "the signed version file is wrong: {why}"),
        }
    }
}

impl std::error::Error for VerifyError {}

/// A `major.minor.patch` version, ordered by number. Nothing else is one: no prefix, suffix,
/// leading zero or sign, so a string has one reading.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Version(u32, u32, u32);

impl FromStr for Version {
    type Err = ();

    fn from_str(s: &str) -> Result<Self, ()> {
        let number = |part: &str| -> Result<u32, ()> {
            let digits = !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit());
            if !digits || (part.len() > 1 && part.starts_with('0')) {
                return Err(());
            }
            part.parse().map_err(|_| ())
        };
        let mut parts = s.split('.');
        match (parts.next(), parts.next(), parts.next(), parts.next()) {
            (Some(a), Some(b), Some(c), None) => Ok(Self(number(a)?, number(b)?, number(c)?)),
            _ => Err(()),
        }
    }
}

impl PartialOrd for Version {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Version {
    fn cmp(&self, other: &Self) -> Ordering {
        (self.0, self.1, self.2).cmp(&(other.0, other.1, other.2))
    }
}

/// This build's architecture, as a release names it.
#[must_use]
pub const fn this_arch() -> &'static str {
    if cfg!(target_arch = "aarch64") {
        "arm64"
    } else {
        "x64"
    }
}

/// The version file's name for `arch`.
#[must_use]
pub fn version_file_name(arch: &str) -> String {
    format!("earshot-update-{arch}.json")
}

/// The installer's file name (`xtask::bundle::installer_path` makes the same one).
#[must_use]
pub fn installer_name(version: &str, arch: &str) -> String {
    format!("Earshot_{version}_{arch}-setup.exe")
}

/// Lower-case hex.
#[must_use]
pub fn to_hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(out, "{b:02x}");
    }
    out
}

/// Bytes from hex of exactly `N` bytes.
#[must_use]
pub fn from_hex<const N: usize>(text: &str) -> Option<[u8; N]> {
    let raw = text.as_bytes();
    if raw.len() != N * 2 {
        return None;
    }
    let digit = |b: u8| {
        char::from(b)
            .to_digit(16)
            .and_then(|d| u8::try_from(d).ok())
    };
    let mut out = [0u8; N];
    for (slot, [high, low]) in out.iter_mut().zip(raw.as_chunks::<2>().0) {
        *slot = digit(*high)? * 16 + digit(*low)?;
    }
    Some(out)
}

/// The bytes a signature covers.
fn signed_bytes(manifest: &str) -> Vec<u8> {
    [DOMAIN, manifest.as_bytes()].concat()
}

/// The checks after the signature. A release signed by mistake still cannot name another product,
/// another architecture or an installer whose name does not follow from its version.
fn check(release: &Release, arch: &str) -> Result<(), VerifyError> {
    let wrong = VerifyError::BadManifest;
    if release.product != PRODUCT {
        return Err(wrong("another product"));
    }
    if release.arch != arch {
        return Err(wrong("another architecture"));
    }
    if release.version.parse::<Version>().is_err() {
        return Err(wrong("the version is not major.minor.patch"));
    }
    if release.installer != installer_name(&release.version, &release.arch) {
        return Err(wrong(
            "the installer's name does not follow from its version",
        ));
    }
    if release.size == 0 || release.size > MAX_INSTALLER_BYTES {
        return Err(wrong("the installer's size is out of range"));
    }
    let lower_hex = |b: u8| b.is_ascii_digit() || (b'a'..=b'f').contains(&b);
    if release.sha256.len() != 64 || !release.sha256.bytes().all(lower_hex) {
        return Err(wrong("the installer's checksum is not SHA-256 hex"));
    }
    Ok(())
}

/// Verifies a version file against `key` for this `arch`. Nothing in the file is read as a release
/// until its signature matches.
///
/// # Errors
/// [`VerifyError`]: why it is refused.
pub fn verify(bytes: &[u8], key: &[u8; 32], arch: &str) -> Result<Release, VerifyError> {
    let file: SignedFile = serde_json::from_slice(bytes).map_err(|_| VerifyError::Malformed)?;
    let signature = from_hex::<64>(&file.signature).ok_or(VerifyError::Malformed)?;
    let key = VerifyingKey::from_bytes(key).map_err(|_| VerifyError::BadSignature)?;
    key.verify_strict(
        &signed_bytes(&file.manifest),
        &Signature::from_bytes(&signature),
    )
    .map_err(|_| VerifyError::BadSignature)?;
    let release: Release = serde_json::from_str(&file.manifest)
        .map_err(|_| VerifyError::BadManifest("not a release"))?;
    check(&release, arch)?;
    Ok(release)
}

/// The public key of a signing seed.
#[cfg(any(feature = "sign", test))]
#[must_use]
pub fn public_key(seed: &[u8; 32]) -> [u8; 32] {
    ed25519_dalek::SigningKey::from_bytes(seed)
        .verifying_key()
        .to_bytes()
}

/// A signed version file for `release`, which must pass the same checks as [`verify`].
///
/// # Errors
/// The rule `release` breaks.
#[cfg(any(feature = "sign", test))]
pub fn sign(release: &Release, seed: &[u8; 32]) -> Result<Vec<u8>, VerifyError> {
    use ed25519_dalek::Signer as _;
    check(release, &release.arch)?;
    let manifest = serde_json::to_string(release)
        .map_err(|_| VerifyError::BadManifest("cannot be written"))?;
    let signature = ed25519_dalek::SigningKey::from_bytes(seed).sign(&signed_bytes(&manifest));
    let file = SignedFile {
        manifest,
        signature: to_hex(&signature.to_bytes()),
    };
    serde_json::to_vec_pretty(&file).map_err(|_| VerifyError::BadManifest("cannot be written"))
}

#[cfg(test)]
mod tests;
