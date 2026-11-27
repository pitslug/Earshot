//! Deleting a meeting (T6.26): its audio files (each file before its row), its syncable rows
//! (tombstoned and cleared, [`earshot_core::model::delete_meeting`]), its export records and the
//! device-local records the caller names; then a truncating checkpoint so, with `secure_delete`,
//! nothing of it survives in the database pages, free pages or the WAL. The Obsidian note is the
//! user's and is never touched. People and their voiceprints stay (the owner, T9.1u): only
//! [`Store::remove_person`] deletes them.
//!
//! A meeting is always deletable (T14.26): a row of it that does not read is erased by its
//! rowid in SQL, given the values of a fresh tombstone where its own do not convert, and sent
//! like any other; an audio row that does not read goes after the meeting's folder is emptied.
//! A row whose own `meeting_id` does not read names no meeting, so no deletion finds it: a known
//! limit.

use earshot_core::identity::{Clock, EntityId, Hlc};
use earshot_core::identity::{DeviceId, HlcTimestamp, SyncMeta};
use earshot_core::model::{
    AudioFileRepo, Folder, Meeting, MeetingRepo, Note, RepoError, SegmentRow, Speaker, Summary,
    Track, delete_meeting,
};
use earshot_core::prompt::Template;
use earshot_core::summary::SummaryV1;

use crate::repo::for_deletion;
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
    /// Rows here that did not read and were left as they are, each `(table, rowid)` once
    /// (docs/sync-spec.md 16.7). Counts only.
    pub damaged_skipped: usize,
}

/// One speaker row [`unlink_speakers_tx`] unlinked, as written.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UnlinkedRow {
    /// The SQLite rowid.
    pub rowid: i64,
    /// The meeting it belongs to.
    pub meeting: EntityId,
    /// Its id, `None` when that does not read.
    pub speaker: Option<EntityId>,
    /// The person it was linked to.
    pub from: EntityId,
    /// Whether its stored name changed (a live row's to "Speaker N", a tombstone's to none).
    pub name_erased: bool,
}

/// [`Store::remove_person`]'s transaction (docs/sync-spec.md 16.5 item 21): the identity
/// placed inside it ([`crate::identity_in`]), its biometrics erased
/// ([`crate::identities::forget_identity_biometrics`]'s erasure), its speaker rows unlinked
/// ([`unlink_speakers_tx`]) and its person rows tombstoned with the name cleared, a row that
/// does not read by rowid. Erasures write in today's order, stamps taken in that order.
///
/// # Errors
/// Store errors other than a row that does not read.
pub fn remove_person_in<C: Clock + ?Sized>(
    g: &mut crate::GuardedTx<'_, '_>,
    person: EntityId,
    hlc: &mut Hlc,
    clock: &C,
) -> Result<PersonRemoval, StoreError> {
    let mut report = PersonRemoval::default();
    let identity = crate::identity_in(g, person)?;
    // Before the rows are unlinked: it finds a centroid through the row's link too.
    let forgotten = crate::identities::forget_identity_voices(
        g,
        &identity,
        hlc,
        clock,
        crate::identities::Erasure::User,
    )?;
    report.voiceprints_forgotten = forgotten.voiceprints;
    report.samples_forgotten = forgotten.samples;
    // No per-row work: the identity's entries went above, before the rows were unlinked.
    report.speakers_unnamed =
        unlink_speakers_tx(g, &identity.members, hlc, clock, &mut |_, _| Ok(()))?;
    for &id in &identity.members {
        match g.person_or_damaged(id)? {
            None => continue,
            Some(Ok(mut row)) => {
                row.display_name.clear();
                if row.meta.deleted {
                    row.meta.touch(hlc, clock);
                } else {
                    row.meta.mark_deleted(hlc, clock);
                }
                g.put_person(row)?;
            }
            Some(Err(())) => {
                // A member was placed by its `id` and `merged_into`, so both read: tombstoned by
                // rowid with the name cleared (16.4).
                let rowid = g
                    .person_links()?
                    .into_iter()
                    .find(|l| l.id == crate::Col::Read(id))
                    .map(|l| l.rowid);
                let Some(rowid) = rowid else { continue };
                g.tombstone_person_by_rowid(hlc, clock, rowid, Some(id))?;
            }
        }
        report.persons_removed += 1;
    }
    report.damaged_skipped = g.damaged_skipped();
    Ok(report)
}

/// Unlinks every speaker row linked to one of `members` (docs/sync-spec.md 16.5 item 21): a
/// readable row is rewritten as before (unlinked, not ignored, the first free "Speaker N" of
/// its meeting when live, no name when a tombstone); a damaged row whose `person_id` reads as
/// a member is unlinked by rowid under the written-columns rule of 16.4 ("Speaker N" only when
/// its name and `deleted` read and it is live, `''` when `deleted` reads `Yes`, else the name
/// as stored); a damaged row whose `person_id` or `meeting_id` does not read is left as it is
/// and counted. `on_row` is called at each row's write, with what was written
/// ([`UnlinkedRow`]; docs/sync-spec.md 7A.2 P5 drops the row's identity entry there). Rows are
/// written meeting by meeting in `all_meetings`' order, by creation within a meeting, and only
/// meetings holding such a row are read. Returns how many live rows went back to "Speaker N".
///
/// # Errors
/// Store errors, and `on_row`'s.
pub fn unlink_speakers_tx<C: Clock + ?Sized>(
    g: &mut crate::GuardedTx<'_, '_>,
    members: &[EntityId],
    hlc: &mut Hlc,
    clock: &C,
    on_row: &mut dyn FnMut(&mut crate::GuardedTx<'_, '_>, &UnlinkedRow) -> Result<(), StoreError>,
) -> Result<usize, StoreError> {
    use crate::{Col, Deleted};
    let theirs = |p: Option<EntityId>| p.is_some_and(|p| members.contains(&p));
    let mut unnamed = 0;
    let mut holding: std::collections::BTreeSet<EntityId> = std::collections::BTreeSet::new();
    for l in g.speaker_links(None)? {
        if !l.meeting_id.reads() || !l.person_id.reads() {
            g.count_damaged("speaker", &[l.rowid]);
        }
        if let (Col::Read(m), Col::Read(p)) = (l.meeting_id, l.person_id)
            && theirs(p)
        {
            holding.insert(m);
        }
    }
    for meeting in g.meetings_lenient()? {
        let Col::Read(m) = meeting.id else { continue };
        if !holding.contains(&m) {
            continue;
        }
        let rows = g.speakers_in_order(m)?;
        let links = g.speaker_links(Some(m))?;
        let link_of = |rowid: i64| links.iter().find(|l| l.rowid == rowid);
        // Names held by the meeting's other live rows (a name that does not read is held by
        // no row).
        let mut taken: Vec<String> = Vec::new();
        for (rowid, row) in &rows {
            match row {
                Some(s) if !s.meta.deleted && !theirs(s.person_id) => {
                    taken.push(s.display_name.clone());
                }
                Some(_) => {}
                None => {
                    if let Some(l) = link_of(*rowid)
                        && l.deleted == Deleted::No
                        && !matches!(l.person_id, Col::Read(p) if theirs(p))
                        && let Col::Read(name) = &l.display_name
                    {
                        taken.push(name.clone());
                    }
                }
            }
        }
        for (rowid, row) in rows {
            match row {
                Some(mut row) => {
                    let Some(from) = row.person_id.filter(|p| members.contains(p)) else {
                        continue;
                    };
                    let before = row.display_name.clone();
                    row.person_id = None;
                    // A linked row is never ignored; a flag a pulled row carried must not turn
                    // the unlinked voice into an ignored one (T12.93).
                    row.ignored = false;
                    if row.meta.deleted {
                        row.display_name.clear();
                    } else {
                        row.display_name = crate::first_free_speaker_name(&taken);
                        taken.push(row.display_name.clone());
                        unnamed += 1;
                    }
                    row.meta.touch(hlc, clock);
                    let done = UnlinkedRow {
                        rowid,
                        meeting: m,
                        speaker: Some(row.id),
                        from,
                        name_erased: before != row.display_name,
                    };
                    g.note_stamped(row.id);
                    g.put_speaker(row)?;
                    on_row(g, &done)?;
                }
                None => {
                    let Some(l) = link_of(rowid).cloned() else {
                        continue;
                    };
                    let Col::Read(Some(from)) = l.person_id else {
                        continue;
                    };
                    if !members.contains(&from) || !l.meeting_id.reads() {
                        continue;
                    }
                    let name = match (l.deleted, &l.display_name) {
                        (Deleted::Yes, _) => Some(String::new()),
                        (Deleted::No, Col::Read(_)) => {
                            let n = crate::first_free_speaker_name(&taken);
                            taken.push(n.clone());
                            unnamed += 1;
                            Some(n)
                        }
                        _ => None,
                    };
                    g.unlink_speaker_by_rowid(hlc, clock, rowid, l.id.read(), name.as_deref())?;
                    let name_erased = match (&name, &l.display_name) {
                        (Some(new), Col::Read(old)) => new != old,
                        (Some(_), Col::Unreadable) => true,
                        (None, _) => false,
                    };
                    let done = UnlinkedRow {
                        rowid,
                        meeting: m,
                        speaker: l.id.read(),
                        from,
                        name_erased,
                    };
                    on_row(g, &done)?;
                }
            }
        }
    }
    Ok(unnamed)
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
        // An audio row that does not read cannot name its file, but its file is in the
        // meeting's folder, emptied above: file before row still holds (T14.26).
        for rowid in damaged_files {
            self.conn
                .execute("DELETE FROM audio_file WHERE rowid = ?1", [rowid])?;
        }

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
        let report = self.guarded_transaction(|g| remove_person_in(g, person, hlc, clock))?;
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
    /// `meeting`'s row: `None` when absent, `Some(Err(()))` when it exists but its values do not
    /// read (T14.26: what a deletion asks before it runs, so a damaged row stays deletable).
    ///
    /// # Errors
    /// SQLite errors.
    pub fn meeting_or_damaged(
        &self,
        meeting: EntityId,
    ) -> Result<Option<Result<Meeting, ()>>, StoreError> {
        Ok(for_deletion::meeting(&self.conn, meeting)?.map(|r| r.map_err(|_| ())))
    }

    /// Every live meeting row that reads, in [`MeetingRepo::meetings`]' order, and how many
    /// meeting rows that are not tombstones do not read (T14.64): what a list of every meeting
    /// reads, so one damaged row is left out and counted, never failing the list.
    ///
    /// # Errors
    /// SQLite errors.
    pub fn live_meetings_or_damaged(&self) -> Result<(Vec<Meeting>, usize), StoreError> {
        let (ok, bad) = crate::repo::lenient::live_meetings(&self.conn)?;
        Ok((ok, bad.len()))
    }

    /// Every live folder row that reads, in `FolderRepo::folders`' order, and how many folder
    /// rows that are not tombstones do not read (T14.64): a damaged folder row is left out and
    /// counted, never failing the meeting list or the folder list.
    ///
    /// # Errors
    /// SQLite errors.
    pub fn live_folders_or_damaged(&self) -> Result<(Vec<Folder>, usize), StoreError> {
        let (ok, bad) = crate::repo::lenient::live_folders(&self.conn)?;
        Ok((ok, bad.len()))
    }

    /// Whether `meeting`'s row is a tombstone, read from that one column, so a row whose other
    /// values do not read still answers (T14.26: after a deletion whose checkpoint was
    /// `Busy`). `false` for an unknown meeting.
    ///
    /// # Errors
    /// SQLite errors.
    pub fn meeting_reads_deleted(&self, meeting: EntityId) -> Result<bool, StoreError> {
        Ok(crate::repo::row_reads_deleted(
            &self.conn, "meeting", meeting,
        )?)
    }
}

impl StoreTx<'_> {
    /// [`Store::live_meetings_or_damaged`] inside the transaction.
    ///
    /// # Errors
    /// SQLite errors.
    pub fn live_meetings_or_damaged(&self) -> Result<(Vec<Meeting>, usize), StoreError> {
        let (ok, bad) = crate::repo::lenient::live_meetings(&self.conn)?;
        Ok((ok, bad.len()))
    }

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
            // Values outside the content that do not convert get a fresh tombstone's, so the
            // row reads and its tombstone syncs (else the outbox keeps it unsendable for good).
            if !for_deletion::reads(c, table, rowid)? {
                for_deletion::canonicalise(c, table, rowid)?;
            }
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
