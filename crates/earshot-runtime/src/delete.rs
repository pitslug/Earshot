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
//! - its summary requests' offload clock readings in the `offload-times` record (T12.70);
//! - then a truncating checkpoint, so with `secure_delete` none of it survives in free pages
//!   or the WAL.
//!
//! People and their voiceprints are shared across meetings and stay (the owner, T9.1u): only
//! [`crate::edit::remove_person`], from the Speakers page, deletes a name and a voice, and
//! [`crate::edit::forget_person_voices`] a voice alone. The meeting's own record of speaker
//! centroids goes with it.
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
        // Leases are read under the store lock on purpose: a job cannot lease a meeting and
        // write its records between this snapshot and the sweep below. The activity lock is a
        // leaf lock (nothing takes another lock while holding it), so taking it here, inside the
        // store lock, cannot deadlock.
        let leased = self.leased_meetings();
        // Records of other meetings whose rows are deleted or missing and that no job holds.
        let mut gone = BTreeSet::new();
        for key in db.local_keys()? {
            if let Some(m) = crate::local::meeting_of(&key)
                && m != meeting
                && !leased.contains(&m)
                && !gone.contains(&m)
                // A row that does not read is judged by its `deleted` column alone (T14.26), so
                // another meeting's damage never stops this deletion.
                && match db.meeting(m) {
                    Ok(row) => row.is_none_or(|row| row.meta.deleted),
                    Err(_) => db.meeting_reads_deleted(m)?,
                }
            {
                gone.insert(m);
            }
        }
        let (store, hlc) = db.parts();
        let deleted = store.delete_meeting(&self.data, meeting, hlc, &*clock, &mut |key| {
            crate::local::meeting_of(key).is_some_and(|m| m == meeting || gone.contains(&m))
        });
        // Offload's clock readings of its requests live in one device record for every meeting,
        // so they go here, once the meeting reads deleted: after a deletion that worked, or one
        // whose checkpoint was `Busy` after its commit. A deletion that failed before its
        // transaction keeps them: a meeting still there must not lose them (a closed request
        // would be handed again). A step that read them earlier cannot write them back
        // (`offload::store_step_times`).
        if deleted.is_ok() || db.meeting_reads_deleted(meeting)? {
            db.transaction(|tx| crate::offload::forget_meeting_times(tx, meeting))?;
        }
        Ok(deleted?)
    }
}

#[cfg(test)]
#[path = "delete_tests.rs"]
mod tests;
