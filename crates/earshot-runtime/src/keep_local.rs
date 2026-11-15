//! Keeping a meeting's summary on this computer (T12.13).
//!
//! The summary writer is one setting for every meeting (`crate::summary_settings`); a meeting
//! started with "keep the summary on this computer" carries the synced field
//! `Meeting::summary_kept_local` (T11.1a, docs/sync-spec.md section 5.1), set in the
//! transaction that creates the meeting's rows (`LiveConfig::keep_summary_local`) and never
//! set later (the owner, 2026-11-10: the switch is set only when a meeting starts). Every
//! synced copy of the meeting carries it, so another device keeps it too. Meetings from before
//! T11.1a carry T12.13's device-local record `summary-kept-local/<meeting>`
//! ([`crate::local::MeetingRecord::KeptLocal`]), which store migration 5 copied into the
//! field; the record is still read, and either one keeps the meeting.
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
//! The field set, or the record present whatever it holds, keeps the meeting; a meeting row
//! or a record that cannot be read counts as kept local. [`allow_writer`] is the only way back
//! (local to the writer, never the other way): it clears both in one transaction and re-stamps
//! the meeting, so the choice wins over an older synced copy; it sends nothing by itself. A
//! stale synced copy can only put the field back to kept, the safe direction.

use earshot_core::identity::EntityId;
use earshot_core::model::MeetingRepo;
use earshot_store::Store;

use crate::local::{LocalKey, MeetingRecord};
use crate::{MeetingActivity, RuntimeError, Services};

/// The record's version, written before its marker.
pub const RECORD_VERSION: u32 = 1;

/// The marker after the version.
#[cfg(test)]
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

/// The record's value: `version u32 LE | "local"`. Tests only: nothing writes the record
/// since T11.1a; tests write it to stand for a meeting from before store migration 5.
#[cfg(test)]
pub(crate) fn encode() -> Vec<u8> {
    let mut v = RECORD_VERSION.to_le_bytes().to_vec();
    v.extend_from_slice(MARKER);
    v
}

/// Where `meeting`'s summary may be written: [`SummaryPrivacy::KeptLocal`] when its meeting
/// row has [`earshot_core::model::Meeting::summary_kept_local`] set, when its T12.13 record is
/// present whatever it holds, and when either cannot be read (fail closed);
/// [`SummaryPrivacy::Writer`] only when the store says neither keeps it. An unknown meeting has
/// no field, so only the record answers for it.
#[must_use]
pub fn summary_privacy(store: &Store, meeting: EntityId) -> SummaryPrivacy {
    match store.meeting(meeting) {
        Ok(Some(m)) if m.summary_kept_local => return SummaryPrivacy::KeptLocal,
        Ok(_) => {}
        Err(_) => return SummaryPrivacy::KeptLocal,
    }
    match store.local_value(record_key(meeting).as_str()) {
        Ok(None) => SummaryPrivacy::Writer,
        Ok(Some(_)) | Err(_) => SummaryPrivacy::KeptLocal,
    }
}

/// Lets `meeting`'s summary be written by the writer the Summaries page names from now on
/// (local to remote; the owner, 2026-11-10). Clears the field and deletes the record in one
/// transaction, re-stamping the meeting only when the field was set: nothing is summarised or
/// sent until the user asks for a summary. Allowing a meeting that is not kept local changes
/// nothing.
///
/// # Errors
/// [`crate::edit::EditError::MeetingNotFound`] for an unknown or deleted meeting;
/// [`crate::edit::EditError::MeetingStillWorking`] while it records, waits, is finished or has a
/// re-transcribe or re-summarise running (nothing changes); store errors.
pub fn allow_writer(services: &Services, meeting: EntityId) -> Result<(), RuntimeError> {
    // The work after a meeting chose its summary writer when the meeting started, and a
    // re-summarise when it started: allowing now would change nothing for it (T12.45). Read
    // before the store lock (`meeting_activity` takes the activity lock, then the store's).
    if matches!(
        services.meeting_activity(meeting)?,
        Some(MeetingActivity::Recording | MeetingActivity::Waiting | MeetingActivity::Processing)
    ) {
        return Err(RuntimeError::Edit(
            crate::edit::EditError::MeetingStillWorking(meeting),
        ));
    }
    crate::edit::write(services, |tx, stamp| {
        let mut m = crate::edit::live_meeting(&*tx, meeting)?;
        if m.summary_kept_local {
            m.summary_kept_local = false;
            m.meta.touch(stamp.hlc, stamp.clock);
            tx.put_meeting(m)?;
        }
        tx.delete_local_value(record_key(meeting).as_str())?;
        Ok(())
    })
}

#[cfg(test)]
#[path = "keep_local_tests.rs"]
mod tests;
