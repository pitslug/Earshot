//! Sealed rows and presence blobs (docs/sync-spec.md section 2.3, section 8).
//!
//! ```text
//! sealed    = 0x01 | nonce (24) | XChaCha20-Poly1305(key, nonce, plaintext, aad)
//! row aad   = "earshot-row-v1" | row key (16) | wall_ms (u64 BE) | logical (u32 BE) | node (16)
//! presence  = "earshot-presence-v1" | device id (16)
//! plaintext = JSON of the row's SyncEnvelope plus "format"
//! ```

use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};
use earshot_core::identity::{
    DeviceId, EntityId, HlcTimestamp, SyncClassKind, SyncMeta, SyncSerialize, sync_envelope,
};
use serde::Deserialize;

use crate::keys::{Keys, RowKey};
use crate::{SealError, Unreadable, check_plaintext};

/// The envelope format this build writes and reads. A row with a higher one is kept unopened
/// ([`Opened::NewerFormat`]) until an update. A new entity does not raise it: an older build
/// holds a row of an entity it does not know (T11.3da kept 1 for `summary_request`). It rises
/// when an existing entity's shape changes, or when older builds must not hold the new rows at
/// all; the relay raises a vault to the highest format pushed to it, so every older build and
/// worker in that vault must update. 2: voice samples (T12.77b8, docs/voice-samples.md 7.3; a
/// build of format 1 would misread a voiceprint whose voice lives in samples). 3: a speaker
/// row carries the user's ignore (T12.93; a build of format 2 would write a speaker row back
/// without it and undo the ignore on every computer). 4: a final segment row carries its
/// final-pass run (`pass_run`, T12.92, docs/sync-spec.md section 7 rule 4; a build of format 3
/// would write a line back without it and keep no edit order). Rows of formats 1 to 3 still
/// open.
pub const ENVELOPE_FORMAT: u32 = 4;

/// The leading byte of every sealed value this build writes.
const SEAL_VERSION: u8 = 0x01;
const NONCE_LEN: usize = 24;
const TAG_LEN: usize = 16;
const ROW_AAD: &[u8] = b"earshot-row-v1";
const PRESENCE_AAD: &[u8] = b"earshot-presence-v1";

/// A sealed row, ready to push.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SealedRow {
    /// The key the relay files it under.
    pub key: RowKey,
    /// The row's `updated_at`, sent beside the body (the relay keeps the newest per key).
    pub stamp: HlcTimestamp,
    /// The sealed bytes.
    pub body: Vec<u8>,
}

/// An opened row.
#[derive(Debug, Clone, PartialEq)]
pub struct OpenedRow {
    /// The envelope format it was written in (at most [`ENVELOPE_FORMAT`]).
    pub format: u32,
    /// The entity name (`SyncSerialize::ENTITY`).
    pub entity: String,
    /// Its sync class.
    pub class: SyncClassKind,
    /// The row id.
    pub id: EntityId,
    /// Its sync metadata.
    pub meta: SyncMeta,
    /// The row's fields as JSON, for the store to decode by `entity`.
    pub record: serde_json::Value,
}

/// What opening a sealed row gave.
#[derive(Debug, Clone, PartialEq)]
pub enum Opened {
    /// A row this build reads.
    Row(OpenedRow),
    /// A row written by a newer Earshot: keep the sealed body and try again after an update.
    NewerFormat(u32),
}

/// The plaintext's shape on the wire.
#[derive(Deserialize)]
struct Wire {
    format: u32,
    entity: String,
    class: SyncClassKind,
    id: EntityId,
    meta: SyncMeta,
    record: serde_json::Value,
}

/// Only the format, read first so a newer envelope's shape cannot make it unreadable.
#[derive(Deserialize)]
struct FormatOnly {
    format: u32,
}

/// The associated data of a row filed under `key` at `stamp`.
pub(crate) fn row_aad(key: &RowKey, stamp: &HlcTimestamp) -> Vec<u8> {
    let mut aad = Vec::with_capacity(ROW_AAD.len() + 16 + 8 + 4 + 16);
    aad.extend_from_slice(ROW_AAD);
    aad.extend_from_slice(&key.0);
    aad.extend_from_slice(&stamp.wall_ms.to_be_bytes());
    aad.extend_from_slice(&stamp.logical.to_be_bytes());
    aad.extend_from_slice(stamp.node.as_uuid().as_bytes());
    aad
}

/// The associated data of `device`'s presence blob.
pub(crate) fn presence_aad(device: DeviceId) -> Vec<u8> {
    let mut aad = PRESENCE_AAD.to_vec();
    aad.extend_from_slice(device.as_uuid().as_bytes());
    aad
}

/// `0x01 | nonce | ciphertext` of `plaintext` under `key` with `aad`.
pub(crate) fn seal_bytes(
    key: &[u8; 32],
    plaintext: &[u8],
    aad: &[u8],
) -> Result<Vec<u8>, SealError> {
    let mut nonce = [0u8; NONCE_LEN];
    crate::random(&mut nonce)?;
    let cipher = XChaCha20Poly1305::new(key.into());
    let ct = cipher
        .encrypt(
            XNonce::from_slice(&nonce),
            Payload {
                msg: plaintext,
                aad,
            },
        )
        // Encryption fails only for inputs far past any limit here.
        .map_err(|_| SealError::Json)?;
    let mut out = Vec::with_capacity(1 + NONCE_LEN + ct.len());
    out.push(SEAL_VERSION);
    out.extend_from_slice(&nonce);
    out.extend_from_slice(&ct);
    Ok(out)
}

/// The plaintext of `sealed` under `key` with `aad`.
pub(crate) fn open_bytes(key: &[u8; 32], sealed: &[u8], aad: &[u8]) -> Result<Vec<u8>, Unreadable> {
    if sealed.len() < 1 + NONCE_LEN + TAG_LEN || sealed[0] != SEAL_VERSION {
        return Err(Unreadable::Version);
    }
    let (nonce, ct) = sealed[1..].split_at(NONCE_LEN);
    XChaCha20Poly1305::new(key.into())
        .decrypt(XNonce::from_slice(nonce), Payload { msg: ct, aad })
        .map_err(|_| Unreadable::Tag)
}

/// Seals `row` for the relay: its envelope as JSON with `"format"`, checked by
/// [`check_plaintext`], under its row key and `updated_at`.
///
/// # Errors
/// [`SealError::Refused`] for a plaintext [`check_plaintext`] refuses; [`SealError::Json`];
/// [`SealError::Random`].
pub fn seal_row<T: SyncSerialize>(keys: &Keys, row: &T) -> Result<SealedRow, SealError> {
    let mut value = serde_json::to_value(sync_envelope(row)).map_err(|_| SealError::Json)?;
    value
        .as_object_mut()
        .ok_or(SealError::Json)?
        .insert("format".into(), ENVELOPE_FORMAT.into());
    let plaintext = serde_json::to_vec(&value).map_err(|_| SealError::Json)?;
    let key = keys.row_key(row.sync_id());
    let stamp = row.sync_meta().updated_at;
    seal_plaintext(keys, key, stamp, &plaintext)
}

/// Seals an envelope's JSON as filed under `key` at `stamp` (also used by tests to build
/// rows whose contents disagree with where they are filed).
pub(crate) fn seal_plaintext(
    keys: &Keys,
    key: RowKey,
    stamp: HlcTimestamp,
    plaintext: &[u8],
) -> Result<SealedRow, SealError> {
    check_plaintext(plaintext).map_err(SealError::Refused)?;
    let body = seal_bytes(&keys.row, plaintext, &row_aad(&key, &stamp))?;
    Ok(SealedRow { key, stamp, body })
}

/// Opens a row the relay filed under `key` at `stamp`. Checks, in order: the leading byte, the
/// tag (key, row key, stamp and every byte), the format (a newer one is
/// [`Opened::NewerFormat`], not an error), the JSON, that the id hashes to `key`, and that
/// `meta.updated_at` is `stamp`.
///
/// # Errors
/// The first check that failed, as an [`Unreadable`].
pub fn open_row(
    keys: &Keys,
    key: RowKey,
    stamp: HlcTimestamp,
    body: &[u8],
) -> Result<Opened, Unreadable> {
    let plaintext = open_bytes(&keys.row, body, &row_aad(&key, &stamp))?;
    let format = serde_json::from_slice::<FormatOnly>(&plaintext)
        .map_err(|_| Unreadable::Json)?
        .format;
    if format > ENVELOPE_FORMAT {
        return Ok(Opened::NewerFormat(format));
    }
    let w: Wire = serde_json::from_slice(&plaintext).map_err(|_| Unreadable::Json)?;
    if keys.row_key(w.id) != key {
        return Err(Unreadable::Id);
    }
    if w.meta.updated_at != stamp {
        return Err(Unreadable::Stamp);
    }
    Ok(Opened::Row(OpenedRow {
        format: w.format,
        entity: w.entity,
        class: w.class,
        id: w.id,
        meta: w.meta,
        record: w.record,
    }))
}

/// Seals `device`'s presence blob (its JSON is defined by the sync client, section 8). The
/// associated data holds no time: the relay can hand back an old blob as fresh, so presence
/// is advisory (who is likely online), never a fact a decision rests on alone.
///
/// # Errors
/// [`SealError::Random`].
pub fn seal_presence(
    keys: &Keys,
    device: DeviceId,
    plaintext: &[u8],
) -> Result<Vec<u8>, SealError> {
    seal_bytes(&keys.presence, plaintext, &presence_aad(device))
}

/// Opens `device`'s presence blob.
///
/// # Errors
/// [`Unreadable::Version`] or [`Unreadable::Tag`].
pub fn open_presence(keys: &Keys, device: DeviceId, sealed: &[u8]) -> Result<Vec<u8>, Unreadable> {
    open_bytes(&keys.presence, sealed, &presence_aad(device))
}
