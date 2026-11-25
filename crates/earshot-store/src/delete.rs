//! Deleting a meeting (T6.26): its audio files (each file before its row), its syncable rows
//! (tombstoned and cleared, [`earshot_core::model::delete_meeting`]), its export records and the
//! device-local records the caller names; then a truncating checkpoint so, with `secure_delete`,
//! nothing of it survives in the database pages, free pages or the WAL. The Obsidian note is the
//! user's and is never touched. People and their voiceprints stay (the owner, T9.1u): only
//! [`Store::remove_person`] deletes them.

use earshot_core::identity::{Clock, EntityId, Hlc};
use earshot_core::identity::{DeviceId, HlcTimestamp, SyncMeta};
use earshot_core::model::{
    AudioFileRepo, Meeting, MeetingRepo, Note, RepoError, SegmentRow, Speaker, Summary, Track,
    delete_meeting,
};
use earshot_core::prompt::Template;
use earshot_core::summary::SummaryV1;

use crate::repo::for_deletion;
use earshot_core::voiceprint::{VoiceprintRepo, identity_of};
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
    /// Rows of the meeting that could not be read (T14.26): each was erased in SQL by its rowid
    /// (an audio row deleted), as a readable one is. Syncable ones written are in
    /// `rows_tombstoned` too.
    pub unreadable_rows: usize,
}

/// What [`Store::remove_person`] did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PersonRemoval {
    /// Person rows removed (the person and anyone merged into them).
    pub persons_removed: usize,
    /// Voiceprints forgotten.
    pub voiceprints_forgotten: usize,
    /// Voice samples forgotten (T12.77b3).
    pub samples_forgotten: usize,
    /// Speakers in kept meetings that went back to "Speaker N".
    pub speakers_unnamed: usize,
}

impl Store {
    /// Deletes `meeting` (module docs). `erase_local` is asked about every device-local key
    /// (never `device_id`) and returns whether to erase it; it may also name keys of other
    /// meetings (a sweep of records whose cleanup never ran). Persons and their voiceprints are
    /// kept, whichever meetings they came from.
    ///
    /// Order and crash safety: audio files go first, each file before its row (as retention
    /// does); then one transaction writes the tombstones, removes the export records and the
    /// named local values; then the WAL is checkpointed. A crash in between leaves rows whose files are missing (tolerated by every reader) and
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
        // Read row by row (T14.26): a meeting must always be deletable, so a row that does not
        // read never stops it.
        if for_deletion::meeting(&self.conn, meeting)?.is_none() {
            return Err(StoreError::Repo(RepoError::MissingParent {
                entity: "meeting",
                id: meeting,
            }));
        }
        let mut report = MeetingDeletion::default();
        let (files, damaged_files) = for_deletion::audio(&self.conn, meeting)?;
        // An audio row that does not read cannot name its file: the row goes, and the file is
        // removed with the rest of the meeting's folder below.
        for rowid in damaged_files {
            report.unreadable_rows += self
                .conn
                .execute("DELETE FROM audio_file WHERE rowid = ?1", [rowid])?;
        }
        for f in files {
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
            // A row that does not read is erased in SQL by its rowid first (T14.26), so it can
            // neither keep its content nor stop the deletion; the core rule erases the rest.
            let damaged = erase_damaged_rows(tx, meeting, hlc, clock)?;
            let erased = delete_meeting(
                &mut ReadableRows {
                    tx,
                    meeting,
                    meeting_damaged: damaged.meeting_row,
                },
                meeting,
                hlc,
                clock,
            )?;
            report.rows_tombstoned = erased.rows_written + damaged.written;
            report.unreadable_rows += damaged.written;
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

    /// Removes `person` and anyone merged into them, the one way a name or a voiceprint goes
    /// (T9.1u): in one transaction their voiceprints and speaker centroids
    /// ([`crate::identities::forget_identity_biometrics`]), their rows (name cleared,
    /// tombstoned), and every speaker row linked to them, which goes back to the first free
    /// "Speaker N" of its meeting, unlinked, so it asks for a name again. A tombstoned speaker
    /// row keeps no name either. Then a checkpoint, as [`Store::forget_voiceprints`]. Text already
    /// written (transcript words, summaries, notes) is not rewritten. The caller refuses the
    /// user's own person.
    ///
    /// # Errors
    /// As [`Store::forget_voiceprints`]: [`StoreError::Busy`] after the commit means everything
    /// is removed. SQLite errors.
    pub fn remove_person<C: Clock + ?Sized>(
        &mut self,
        person: EntityId,
        hlc: &mut Hlc,
        clock: &C,
    ) -> Result<PersonRemoval, StoreError> {
        let storage = |e: earshot_core::voiceprint::VoiceprintError| {
            StoreError::Repo(RepoError::Storage(e.to_string()))
        };
        let report = self.transaction(|tx| -> Result<PersonRemoval, StoreError> {
            let mut report = PersonRemoval::default();
            let identity = identity_of(tx, person).map_err(storage)?;
            // Before the rows are unlinked: it finds a centroid through the row's link too.
            let forgotten = crate::identities::forget_identity_biometrics(tx, person, hlc, clock)?;
            report.voiceprints_forgotten = forgotten.voiceprints;
            report.samples_forgotten = forgotten.samples;
            for meeting in tx.meetings().map_err(StoreError::Repo)? {
                let rows = tx.speakers(meeting.id).map_err(StoreError::Repo)?;
                let theirs = |p: Option<EntityId>| p.is_some_and(|p| identity.contains(&p));
                let mut taken: Vec<String> = rows
                    .iter()
                    .filter(|s| !s.meta.deleted && !theirs(s.person_id))
                    .map(|s| s.display_name.clone())
                    .collect();
                for mut row in rows.into_iter().filter(|s| theirs(s.person_id)) {
                    row.person_id = None;
                    // A linked row is never ignored; a flag a pulled row carried must not turn
                    // the unlinked voice into an ignored one (T12.93).
                    row.ignored = false;
                    if row.meta.deleted {
                        row.display_name.clear();
                    } else {
                        row.display_name = first_free_speaker_name(&taken);
                        taken.push(row.display_name.clone());
                        report.speakers_unnamed += 1;
                    }
                    row.meta.touch(hlc, clock);
                    tx.put_speaker(row).map_err(StoreError::Repo)?;
                }
            }
            for id in identity {
                let Some(mut row) = tx.person(id).map_err(storage)? else {
                    continue;
                };
                row.display_name.clear();
                if row.meta.deleted {
                    row.meta.touch(hlc, clock);
                } else {
                    row.meta.mark_deleted(hlc, clock);
                }
                tx.put_person(row).map_err(storage)?;
                report.persons_removed += 1;
            }
            Ok(report)
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

impl Store {
    /// Whether `meeting`'s row is a tombstone, read from that one column, so a row whose other
    /// values do not read still answers (T14.26: after a deletion whose checkpoint was
    /// `Busy`). `false` for an unknown meeting.
    ///
    /// # Errors
    /// SQLite errors.
    pub fn meeting_reads_deleted(&self, meeting: EntityId) -> Result<bool, StoreError> {
        let mut st = self
            .conn
            .prepare_cached("SELECT deleted FROM meeting WHERE id = ?1")?;
        let mut rows = st.query([meeting.as_uuid().as_bytes().as_slice()])?;
        Ok(match rows.next()? {
            Some(r) => r.get::<_, i64>(0).is_ok_and(|d| d != 0),
            None => false,
        })
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

/// What [`erase_damaged_rows`] did.
struct DamagedRows {
    /// Rows written (erased now; one erased earlier is not written again).
    written: usize,
    /// The meeting row itself does not read: the deletion sees it as an erased tombstone.
    meeting_row: bool,
}

/// One synced table of a meeting: the SQL that erases a damaged row by its rowid as
/// [`delete_meeting`] erases a readable one (the content it clears, a tombstone, `?1`..`?3` this
/// device's stamp, `?4` the rowid), skipping a row already erased so. The outbox triggers fire on
/// the update, as on any write.
const ERASE_BY_ROWID: [(&str, &str); 6] = [
    (
        "segment",
        "UPDATE segment SET text = '', words = '[]', confidence = 0.0, speaker_id = NULL, \
         deleted = 1, updated_wall = ?1, updated_logical = ?2, updated_node = ?3 \
         WHERE rowid = ?4 AND NOT (deleted = 1 AND text = '' AND words = '[]' \
         AND confidence = 0.0 AND speaker_id IS NULL)",
    ),
    (
        "speaker",
        "UPDATE speaker SET display_name = '', person_id = NULL, ignored = 0, \
         deleted = 1, updated_wall = ?1, updated_logical = ?2, updated_node = ?3 \
         WHERE rowid = ?4 AND NOT (deleted = 1 AND display_name = '' AND person_id IS NULL \
         AND ignored = 0)",
    ),
    (
        "note",
        "UPDATE note SET text = '', \
         deleted = 1, updated_wall = ?1, updated_logical = ?2, updated_node = ?3 \
         WHERE rowid = ?4 AND NOT (deleted = 1 AND text = '')",
    ),
    (
        "summary",
        "UPDATE summary SET body = ?5, model = NULL, \
         deleted = 1, updated_wall = ?1, updated_logical = ?2, updated_node = ?3 \
         WHERE rowid = ?4 AND NOT (deleted = 1 AND body = ?5 AND model IS NULL)",
    ),
    (
        "track",
        "UPDATE track SET device_name = NULL, \
         deleted = 1, updated_wall = ?1, updated_logical = ?2, updated_node = ?3 \
         WHERE rowid = ?4 AND NOT (deleted = 1 AND device_name IS NULL)",
    ),
    (
        "meeting",
        "UPDATE meeting SET title = '', tags = '[]', folder_id = NULL, \
         deleted = 1, updated_wall = ?1, updated_logical = ?2, updated_node = ?3 \
         WHERE rowid = ?4 AND NOT (deleted = 1 AND title = '' AND tags = '[]' \
         AND folder_id IS NULL)",
    ),
];

/// Erases every row of `meeting` whose stored values do not convert (T14.26), in each synced
/// table the deletion reads and the meeting row last, by SQLite rowid, as [`delete_meeting`]
/// erases a readable row (the `ERASE_BY_ROWID` statements). Each written row gets its own stamp
/// from the device HLC.
fn erase_damaged_rows<C: Clock + ?Sized>(
    tx: &StoreTx<'_>,
    meeting: EntityId,
    hlc: &mut Hlc,
    clock: &C,
) -> Result<DamagedRows, StoreError> {
    let c = &tx.conn;
    let meeting_row = for_deletion::meeting(c, meeting)?;
    let damaged: [Vec<i64>; 6] = [
        for_deletion::segments(c, meeting)?.1,
        for_deletion::speakers(c, meeting)?.1,
        for_deletion::notes(c, meeting)?.1,
        for_deletion::summaries(c, meeting)?.1,
        for_deletion::tracks(c, meeting)?.1,
        meeting_row
            .iter()
            .filter_map(|r| r.as_ref().err().copied())
            .collect(),
    ];
    let cleared_body = serde_json::to_string(&SummaryV1::default())
        .map_err(|e| StoreError::Corrupt(e.to_string()))?;
    let mut written = 0;
    for ((table, sql), rowids) in ERASE_BY_ROWID.iter().zip(damaged) {
        for rowid in rowids {
            let at = hlc.now(clock);
            let wall = i64::try_from(at.wall_ms)
                .map_err(|_| StoreError::Corrupt("a clock reading past the SQLite range".into()))?;
            let node = at.node.as_uuid().as_bytes().to_vec();
            let logical = i64::from(at.logical);
            written += if *table == "summary" {
                c.execute(
                    sql,
                    rusqlite::params![wall, logical, node, rowid, cleared_body],
                )?
            } else {
                c.execute(sql, rusqlite::params![wall, logical, node, rowid])?
            };
        }
    }
    Ok(DamagedRows {
        written,
        meeting_row: matches!(meeting_row, Some(Err(_))),
    })
}

/// The deletion's view of the transaction: every read as [`StoreTx`]'s, except that rows that
/// do not read are left out (they were erased in SQL first, [`erase_damaged_rows`]); a damaged
/// meeting row reads as an erased tombstone, so the core rule writes nothing over it.
struct ReadableRows<'a, 'b> {
    tx: &'a mut StoreTx<'b>,
    meeting: EntityId,
    meeting_damaged: bool,
}

impl MeetingRepo for ReadableRows<'_, '_> {
    fn meeting(&self, id: EntityId) -> Result<Option<Meeting>, RepoError> {
        if self.meeting_damaged && id == self.meeting {
            // Already erased in SQL; these values are never written (the core rule finds
            // nothing to change in an erased tombstone).
            let stamp = HlcTimestamp {
                wall_ms: 0,
                logical: 0,
                node: DeviceId::from_bytes([0; 16]),
            };
            return Ok(Some(Meeting {
                id,
                meta: SyncMeta {
                    created_at: stamp,
                    updated_at: stamp,
                    deleted: true,
                    origin_device_id: stamp.node,
                },
                title: String::new(),
                started_unix_ms: 0,
                utc_offset_minutes: 0,
                duration_ms: None,
                tags: Vec::new(),
                template: Template::General,
                folder_id: None,
                summary_kept_local: false,
            }));
        }
        self.tx.meeting(id)
    }
    fn meetings(&self) -> Result<Vec<Meeting>, RepoError> {
        self.tx.meetings()
    }
    fn put_meeting(&mut self, meeting: Meeting) -> Result<(), RepoError> {
        self.tx.put_meeting(meeting)
    }
    fn tracks(&self, meeting: EntityId) -> Result<Vec<Track>, RepoError> {
        Ok(for_deletion::tracks(&self.tx.conn, meeting)?.0)
    }
    fn put_track(&mut self, track: Track) -> Result<(), RepoError> {
        self.tx.put_track(track)
    }
    fn segments(&self, meeting: EntityId) -> Result<Vec<SegmentRow>, RepoError> {
        Ok(for_deletion::segments(&self.tx.conn, meeting)?.0)
    }
    fn put_segment(&mut self, segment: SegmentRow) -> Result<(), RepoError> {
        self.tx.put_segment(segment)
    }
    fn speakers(&self, meeting: EntityId) -> Result<Vec<Speaker>, RepoError> {
        Ok(for_deletion::speakers(&self.tx.conn, meeting)?.0)
    }
    fn put_speaker(&mut self, speaker: Speaker) -> Result<(), RepoError> {
        self.tx.put_speaker(speaker)
    }
    fn notes(&self, meeting: EntityId) -> Result<Vec<Note>, RepoError> {
        Ok(for_deletion::notes(&self.tx.conn, meeting)?.0)
    }
    fn put_note(&mut self, note: Note) -> Result<(), RepoError> {
        self.tx.put_note(note)
    }
    fn summaries(&self, meeting: EntityId) -> Result<Vec<Summary>, RepoError> {
        Ok(for_deletion::summaries(&self.tx.conn, meeting)?.0)
    }
    fn put_summary(&mut self, summary: Summary) -> Result<(), RepoError> {
        self.tx.put_summary(summary)
    }
}

/// "Speaker N" for the smallest N no name of `taken` uses, as the final pass numbers them.
#[must_use]
pub fn first_free_speaker_name(taken: &[String]) -> String {
    (1u32..)
        .map(|n| format!("Speaker {n}"))
        .find(|name| !taken.contains(name))
        .unwrap_or_default()
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
