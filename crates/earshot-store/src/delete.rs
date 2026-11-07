//! Deleting a meeting (T6.26): its audio files (each file before its row), its syncable rows
//! (tombstoned and cleared, [`earshot_core::model::delete_meeting`]), its export records, the
//! device-local records the caller names, and voiceprints learnt only from it; then a
//! truncating checkpoint so, with `secure_delete`, nothing of it survives in the database
//! pages, free pages or the WAL. The Obsidian note is the user's and is never touched.

use earshot_core::identity::{Clock, EntityId, Hlc};
use earshot_core::model::{
    AudioFileRepo, MeetingRepo, RepoError, delete_meeting, unreferenced_identities,
};
use rusqlite::Connection;

use crate::db::{Store, StoreError, StoreTx};
use crate::layout::DataDir;

/// What [`Store::delete_meeting`] did. All zero when the meeting was already deleted.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MeetingDeletion {
    /// Syncable rows tombstoned or cleared (meeting, tracks, speakers, segments, notes,
    /// summaries).
    pub rows_tombstoned: usize,
    /// Audio files removed (each before its row).
    pub audio_files_deleted: usize,
    /// Audio rows whose file was already gone (the row is removed).
    pub audio_files_missing: usize,
    /// Audio files in the meeting's folder without a row (a crashed recording's `.part`).
    pub stray_audio_removed: usize,
    /// Export records removed (the notes themselves stay in the vault).
    pub export_records_removed: usize,
    /// Device-local values erased.
    pub local_values_erased: usize,
    /// Voiceprints forgotten because only this meeting referred to their person.
    pub voiceprints_forgotten: usize,
}

impl Store {
    /// Deletes `meeting` (module docs). `erase_local` is asked about every device-local key
    /// (never `device_id`) and returns whether to erase it; it may also name keys of other
    /// meetings (a sweep of records whose cleanup never ran). Persons are kept.
    ///
    /// Order and crash safety: audio files go first, each file before its row (as retention
    /// does); then one transaction writes the tombstones, removes the export records and the
    /// named local values and forgets orphaned voiceprints; then the WAL is checkpointed. A
    /// crash in between leaves rows whose files are missing (tolerated by every reader) and
    /// running the deletion again finishes it. A meeting that is already deleted is cleaned
    /// again (nothing left to do is a zero report).
    ///
    /// # Errors
    /// [`StoreError::Repo`] (`MissingParent`) for an unknown meeting; I/O errors other than
    /// "not found" removing a file (that row is kept and nothing else is written);
    /// [`StoreError::Busy`] (retryable) if another connection blocks the checkpoint (the
    /// deletion is committed; the closing checkpoint retries); SQLite errors.
    pub fn delete_meeting<C: Clock + ?Sized>(
        &mut self,
        data: &DataDir,
        meeting: EntityId,
        hlc: &mut Hlc,
        clock: &C,
        erase_local: &mut dyn FnMut(&str) -> bool,
    ) -> Result<MeetingDeletion, StoreError> {
        if self.meeting(meeting)?.is_none() {
            return Err(StoreError::Repo(RepoError::MissingParent {
                entity: "meeting",
                id: meeting,
            }));
        }
        let mut report = MeetingDeletion::default();
        for f in self.audio_files(meeting)? {
            match std::fs::remove_file(data.resolve(&f.path)?) {
                Ok(()) => report.audio_files_deleted += 1,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    report.audio_files_missing += 1;
                }
                Err(e) => return Err(e.into()),
            }
            self.delete_audio_file(f.id)?;
        }
        report.stray_audio_removed = remove_meeting_audio_dir(data, meeting)?;

        self.transaction(|tx| -> Result<(), StoreError> {
            let erased = delete_meeting(tx, meeting, hlc, clock)?;
            report.rows_tombstoned = erased.rows_written;
            // The rule of `forget_unreferenced_voiceprints`, through the store's one path
            // for an identity's biometric data, so its speaker centroids in other meetings'
            // identity records go too (T6.25).
            for id in unreferenced_identities(tx, &erased.persons)? {
                report.voiceprints_forgotten +=
                    crate::identities::forget_identity_biometrics(tx, id, hlc, clock)?;
            }
            report.export_records_removed = tx.conn.execute(
                "DELETE FROM export_record WHERE meeting_id = ?1",
                [meeting.as_uuid().as_bytes().as_slice()],
            )?;
            // A lease an export left behind (a crash) names the meeting too.
            if tx.delete_local_value(&crate::export::lease_key(meeting))? {
                report.local_values_erased += 1;
            }
            for key in local_keys(&tx.conn)? {
                if erase_local(&key) && tx.delete_local_value(&key)? {
                    report.local_values_erased += 1;
                }
            }
            Ok(())
        })?;
        self.flush_erasures()?;
        Ok(report)
    }

    /// Every device-local key except the reserved `device_id`, in key order.
    ///
    /// # Errors
    /// SQLite errors.
    pub fn local_keys(&self) -> Result<Vec<String>, StoreError> {
        local_keys(&self.conn)
    }
}

impl StoreTx<'_> {
    /// [`Store::local_keys`] inside the transaction.
    ///
    /// # Errors
    /// SQLite errors.
    pub fn local_keys(&self) -> Result<Vec<String>, StoreError> {
        local_keys(&self.conn)
    }
}

fn local_keys(conn: &Connection) -> Result<Vec<String>, StoreError> {
    let mut stmt =
        conn.prepare("SELECT key FROM local_state WHERE key <> 'device_id' ORDER BY key")?;
    let keys = stmt
        .query_map([], |r| r.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(keys)
}

/// Removes the audio files left in `audio/<meeting>/` without a row (any age: the meeting is
/// being deleted), then the folder if it is empty. Other files stay.
fn remove_meeting_audio_dir(data: &DataDir, meeting: EntityId) -> Result<usize, StoreError> {
    let dir = data.resolve(&format!("audio/{meeting}"))?;
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Ok(0);
    };
    let mut removed = 0;
    for e in entries.filter_map(Result::ok) {
        let name = e.file_name().to_string_lossy().into_owned();
        if !e.file_type().is_ok_and(|t| t.is_file()) || !crate::audio::is_audio_name(&name) {
            continue;
        }
        match std::fs::remove_file(e.path()) {
            Ok(()) => removed += 1,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => return Err(err.into()),
        }
    }
    // Only succeeds when empty.
    let _ = std::fs::remove_dir(&dir);
    Ok(removed)
}
