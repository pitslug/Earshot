//! Keeping a meeting's summary on this computer (T12.13).
//!
//! The summary writer is one setting for every meeting (`crate::summary_settings`); a meeting
//! started with "keep the summary on this computer" carries the device-local record
//! `summary-kept-local/<meeting>` ([`crate::local::MeetingRecord::KeptLocal`]), written in the
//! transaction that creates the meeting's rows (`LiveConfig::keep_summary_local`) and never
//! written later (the owner, 2026-11-10: the switch is set only when a meeting starts).
//!
//! Two decision points read it, and both fail closed:
//! - [`crate::host::summary_from_settings`] with [`SummaryPrivacy::KeptLocal`] turns a remote
//!   writer (an endpoint, Claude Code, Codex) into the local summary model, or into no summary
//!   ([`crate::host::SkipReason::KeptLocalNoModel`]); no API key is read and no network client
//!   or agent CLI is built.
//! - [`crate::summary::SummaryService::summarize`] refuses any service that is not local for a
//!   meeting whose record is present ([`crate::RuntimeError::SummaryKeptLocal`]), before any text
//!   leaves the store.
//!
//! The record's presence is the whole answer: a record that cannot be read, or a store that
//! cannot be read, counts as kept local. [`allow_writer`] is the only way back (local to the
//! writer, never the other way), and it sends nothing by itself.

use earshot_core::identity::EntityId;
use earshot_core::model::MeetingRepo;
use earshot_store::Store;

use crate::local::{LocalKey, MeetingRecord};
use crate::{RuntimeError, Services};

/// The record's version, written before its marker.
pub const RECORD_VERSION: u32 = 1;

/// The marker after the version.
const MARKER: &[u8] = b"local";

/// Where a meeting's summary may be written.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SummaryPrivacy {
    /// By the writer the Summaries page names.
    Writer,
    /// Only on this computer: by the local summary model, or not at all.
    KeptLocal,
}

/// The record's key for `meeting`.
pub(crate) fn record_key(meeting: EntityId) -> LocalKey {
    MeetingRecord::KeptLocal.key(&[], meeting, &[])
}

/// The record's value: `version u32 LE | "local"`.
pub(crate) fn encode() -> Vec<u8> {
    let mut v = RECORD_VERSION.to_le_bytes().to_vec();
    v.extend_from_slice(MARKER);
    v
}

/// Marks `meeting` kept local inside `tx` (the transaction that creates its rows).
///
/// # Errors
/// Store errors.
pub(crate) fn mark_kept_local_tx(
    tx: &mut earshot_store::StoreTx<'_>,
    meeting: EntityId,
) -> Result<(), earshot_store::StoreError> {
    crate::local::set_tx(tx, &record_key(meeting), &encode())
}

/// Where `meeting`'s summary may be written: [`SummaryPrivacy::KeptLocal`] whenever its record
/// is present, whatever it holds, and when the store cannot be read (fail closed);
/// [`SummaryPrivacy::Writer`] only when the store says there is no record.
#[must_use]
pub fn summary_privacy(store: &Store, meeting: EntityId) -> SummaryPrivacy {
    match store.local_value(record_key(meeting).as_str()) {
        Ok(None) => SummaryPrivacy::Writer,
        Ok(Some(_)) | Err(_) => SummaryPrivacy::KeptLocal,
    }
}

/// Lets `meeting`'s summary be written by the writer the Summaries page names from now on
/// (local to remote; the owner, 2026-11-10). Deletes the record only: nothing is summarised or
/// sent until the user asks for a summary. Allowing a meeting that is not kept local changes
/// nothing.
///
/// # Errors
/// [`crate::edit::EditError::MeetingNotFound`] for an unknown or deleted meeting; store errors.
pub fn allow_writer(services: &Services, meeting: EntityId) -> Result<(), RuntimeError> {
    let mut db = services.store();
    match db.meeting(meeting)? {
        Some(m) if !m.meta.deleted => {}
        _ => {
            return Err(RuntimeError::Edit(crate::edit::EditError::MeetingNotFound(
                meeting,
            )));
        }
    }
    db.delete_local_value(record_key(meeting).as_str())?;
    Ok(())
}

#[cfg(test)]
#[path = "keep_local_tests.rs"]
mod tests;
