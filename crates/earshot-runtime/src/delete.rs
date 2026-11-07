//! Deleting a meeting (T6.26), the explicit command the UI calls.
//!
//! [`Services::delete_meeting`] removes, through [`earshot_store::Store::delete_meeting`]:
//! - the audio files (each file before its row) and any stray `.part` in the meeting's
//!   audio folder;
//! - every syncable row of the meeting, as tombstones with their content cleared (the
//!   deletion syncs; transcript text, names and summaries do not survive it);
//! - its export records (the Obsidian note itself is the user's and stays in the vault);
//! - every device-local record keyed by it (any [`crate::local::MeetingRecord`]: live speech
//!   stretches, live speaker windows, echo locks and reports, pass backends, export
//!   fingerprints), and the same records of other meetings that are already deleted or
//!   gone (a withdrawn session, a meeting deleted before its cleanup ran) unless a job holds
//!   them;
//! - then a truncating checkpoint, so with `secure_delete` none of it survives in free pages
//!   or the WAL.
//!
//! Persons are shared across meetings and are kept. A voiceprint has no per-meeting
//! provenance (it is a running sum), so it is kept unless no live speaker of any other live
//! meeting refers to its person any more (then it was learnt only from deleted meetings and
//! is forgotten); the user's own voiceprint is always kept (it is deleted explicitly with
//! `Store::forget_voiceprints`). See [`earshot_core::model::forget_unreferenced_voiceprints`].
//!
//! # Concurrency
//!
//! Live sessions, final passes and exports hold a [`crate::MeetingLease`] on their meeting;
//! a deletion while one is held is refused with the retryable
//! [`RuntimeError::MeetingBusy`], and a job cannot start on a meeting while its deletion
//! runs. Across processes (no shared lease table) the store is the guard: the final pass's
//! write transaction refuses a deleted meeting, the summary's already does, and an export
//! skips one; a device-local record written by another process just after a deletion is
//! swept by the next deletion.

use std::collections::BTreeSet;
use std::sync::Arc;

use earshot_core::identity::EntityId;
use earshot_core::model::MeetingRepo;
use earshot_store::MeetingDeletion;

use crate::{RuntimeError, Services};

impl Services {
    /// Deletes `meeting` (module docs). Deleting a meeting that is already deleted cleans
    /// up anything left behind (a zero report when nothing was).
    ///
    /// # Errors
    /// [`RuntimeError::MeetingBusy`] (retryable) while a live session, final pass or export
    /// holds the meeting or another deletion of it runs; [`RuntimeError::Store`] for an
    /// unknown meeting (`Repo(MissingParent)`), a file that cannot be removed, or a
    /// checkpoint blocked by another connection (`Busy`, retryable: the deletion is
    /// committed).
    pub fn delete_meeting(&self, meeting: EntityId) -> Result<MeetingDeletion, RuntimeError> {
        let _claim = self.begin_deletion(meeting)?;
        let clock = Arc::clone(&self.clock);
        let mut db = self.store();
        let leased = self.leased_meetings();
        // Records of other meetings whose rows are deleted or missing and that no job holds.
        let mut gone = BTreeSet::new();
        for key in db.local_keys()? {
            if let Some(m) = crate::local::meeting_of(&key)
                && m != meeting
                && !leased.contains(&m)
                && !gone.contains(&m)
                && db.meeting(m)?.is_none_or(|row| row.meta.deleted)
            {
                gone.insert(m);
            }
        }
        let (store, hlc) = db.parts();
        let report = store.delete_meeting(&self.data, meeting, hlc, &*clock, &mut |key| {
            crate::local::meeting_of(key).is_some_and(|m| m == meeting || gone.contains(&m))
        })?;
        Ok(report)
    }
}

#[cfg(test)]
#[path = "delete_tests.rs"]
mod tests;
