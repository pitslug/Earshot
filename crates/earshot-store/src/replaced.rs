//! Replaced transcript text is erased (T14.47, docs/sync-spec.md section 15).
//!
//! - The cleared form (15.1): one SQL statement, by SQLite rowid, writes a segment row as a
//!   tombstone with no text, words or speaker and confidence
//!   [`earshot_core::model::CLEARED_CONFIDENCE`], with this device's stamp. No typed read is
//!   involved, so a row whose values do not convert is cleared like any other; one that still
//!   does not read afterwards is given a canonical tombstone's values, as a deletion does.
//! - The predicate and the one function (15.2): [`Store::clear_replaced_text`] (and the
//!   transaction's [`StoreTx::clear_replaced_text`]) is the only code that applies it. A row is
//!   replaced when it is a tombstone that holds text (P1), or, on the meeting's owner only, a
//!   live draft row of a live track of the meeting that has a live final row (P2). In a meeting
//!   that reads deleted here the erasure form is written instead, to every row that holds text.
//! - "Lines go out before what they replace" (15.2): a transaction that wrote a live final row
//!   of a track checks, before it commits, that the track's queued cleared rows go out only
//!   after a copy of a live final line of the track reached the relay or goes before them;
//!   otherwise they are stamped again, above every row the transaction wrote
//!   ([`StoreTx::send_lines_first`]).

use std::sync::Arc;

use earshot_core::identity::{Clock, EntityId, Hlc, HlcTimestamp};
use rusqlite::{Connection, OptionalExtension, params};

use crate::db::{Store, StoreError, StoreTx};
use crate::sync::SyncedRow;

/// Whether a segment row holds text, in SQL: pinned to `SegmentRow::holds_text` by a test.
pub(crate) const HOLDS_TEXT: &str = "(text <> '' OR words <> '[]')";

/// The cleared form (15.1): `?1`..`?3` this device's stamp, `?4` the rowid.
const CLEAR_BY_ROWID: &str = "UPDATE segment SET deleted = 1, text = '', words = '[]', \
     confidence = 1.0, speaker_id = NULL, \
     updated_wall = ?1, updated_logical = ?2, updated_node = ?3 WHERE rowid = ?4";

/// A meeting deletion's erasure of one segment row, as `delete.rs` writes it.
const ERASE_BY_ROWID: &str = "UPDATE segment SET text = '', words = '[]', confidence = 0.0, \
     speaker_id = NULL, deleted = 1, \
     updated_wall = ?1, updated_logical = ?2, updated_node = ?3 WHERE rowid = ?4";

/// Whether a queued row would be sealed and sent: the push's own guard, given by the sync
/// client ([`Store::set_push_guard`]). `true` for a row the push sends, `false` for one it
/// refuses before sealing.
#[derive(Clone)]
pub struct PushGuard(Arc<dyn Fn(&SyncedRow) -> bool + Send + Sync>);

impl PushGuard {
    /// The guard `sendable`.
    pub fn new(sendable: impl Fn(&SyncedRow) -> bool + Send + Sync + 'static) -> Self {
        Self(Arc::new(sendable))
    }

    /// Whether `row` would be sent.
    #[must_use]
    pub fn sendable(&self, row: &SyncedRow) -> bool {
        (self.0)(row)
    }
}

impl std::fmt::Debug for PushGuard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("PushGuard")
    }
}

/// What [`Store::clear_replaced_text`] did. Counts only.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Cleared {
    /// Rows written in the cleared form (or, in a meeting deleted here, the erasure form).
    pub rows: usize,
    /// Whether more replaced rows remain (the call stopped at its limit).
    pub more: bool,
}

/// Whether a query matches anything.
fn any(c: &Connection, sql: &str, p: &[&dyn rusqlite::ToSql]) -> Result<bool, StoreError> {
    Ok(c.query_row(sql, p, |_| Ok(())).optional()?.is_some())
}

fn stamp_params(at: HlcTimestamp) -> Result<(i64, i64, Vec<u8>), StoreError> {
    let wall = i64::try_from(at.wall_ms)
        .map_err(|_| StoreError::Corrupt("a clock reading past the SQLite range".into()))?;
    Ok((
        wall,
        i64::from(at.logical),
        at.node.as_uuid().as_bytes().to_vec(),
    ))
}

/// Writes the row `rowid` in the cleared form at `at`; one that does not read afterwards gets a
/// canonical tombstone's values, so it reads and syncs. Returns whether a row was written.
pub(crate) fn clear_rowid(
    c: &Connection,
    rowid: i64,
    at: HlcTimestamp,
) -> Result<bool, StoreError> {
    let (wall, logical, node) = stamp_params(at)?;
    let n = c.execute(CLEAR_BY_ROWID, params![wall, logical, node, rowid])?;
    if n > 0 && !crate::repo::for_deletion::reads(c, "segment", rowid)? {
        crate::repo::for_deletion::canonicalise(c, "segment", rowid)?;
    }
    Ok(n > 0)
}

/// Writes the row `rowid` in a meeting deletion's erasure form at `at` (canonicalised as
/// [`clear_rowid`]).
pub(crate) fn erase_rowid(
    c: &Connection,
    rowid: i64,
    at: HlcTimestamp,
) -> Result<bool, StoreError> {
    let (wall, logical, node) = stamp_params(at)?;
    let n = c.execute(ERASE_BY_ROWID, params![wall, logical, node, rowid])?;
    if n > 0 && !crate::repo::for_deletion::reads(c, "segment", rowid)? {
        crate::repo::for_deletion::canonicalise(c, "segment", rowid)?;
    }
    Ok(n > 0)
}

/// The rowid of the segment row `id`.
pub(crate) fn rowid_of(c: &Connection, id: EntityId) -> Result<Option<i64>, StoreError> {
    Ok(c.query_row(
        "SELECT rowid FROM segment WHERE id = ?1",
        params![&id.as_uuid().as_bytes()[..]],
        |r| r.get(0),
    )
    .optional()?)
}

/// Whether `track` is a Track row of `meeting` here (a row naming another meeting's track is
/// never matched, 15.2 and 15.4).
pub(crate) fn track_of_meeting(
    c: &Connection,
    meeting: EntityId,
    track: EntityId,
) -> Result<bool, StoreError> {
    any(
        c,
        "SELECT 1 FROM track WHERE id = ?1 AND meeting_id = ?2",
        &[
            &&track.as_uuid().as_bytes()[..],
            &&meeting.as_uuid().as_bytes()[..],
        ],
    )
}

/// Whether `meeting`'s `track` has a live final row here that reads, other than `except`
/// (15.4: a local row that does not read is treated as absent).
pub(crate) fn has_readable_live_final(
    c: &Connection,
    meeting: EntityId,
    track: EntityId,
    except: Option<EntityId>,
) -> Result<bool, StoreError> {
    let except = except.map(|e| e.as_uuid().as_bytes().to_vec());
    let rowids: Vec<i64> = {
        let mut st = c.prepare(
            "SELECT rowid FROM segment WHERE meeting_id = ?1 AND track_id = ?2 \
             AND pass = 'final' AND deleted = 0 AND (?3 IS NULL OR id <> ?3) ORDER BY rowid",
        )?;
        st.query_map(
            params![
                &meeting.as_uuid().as_bytes()[..],
                &track.as_uuid().as_bytes()[..],
                except
            ],
            |r| r.get(0),
        )?
        .collect::<Result<_, _>>()?
    };
    for rowid in rowids {
        if crate::repo::for_deletion::reads(c, "segment", rowid)? {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Whether this store's device is `meeting`'s owner: the device its row's origin names (15.2).
pub(crate) fn is_owner(c: &Connection, meeting: EntityId) -> Result<bool, StoreError> {
    let Some(me) = crate::db::stored_device(c)? else {
        return Ok(false);
    };
    any(
        c,
        "SELECT 1 FROM meeting WHERE id = ?1 AND origin_device = ?2",
        &[
            &&meeting.as_uuid().as_bytes()[..],
            &&me.as_uuid().as_bytes()[..],
        ],
    )
}

/// The rowids of `meeting`'s replaced rows (15.2), at most `limit`, in rowid order: P1 always,
/// P2 when `owner`.
fn replaced_rowids(
    c: &Connection,
    meeting: EntityId,
    owner: bool,
    limit: usize,
) -> Result<Vec<i64>, StoreError> {
    // P1, then P2 (15.2): the owner test is `?2`; the final row is matched on meeting and
    // track together, and the track through the `track` table on its id and meeting.
    let sql = "SELECT s.rowid FROM segment s WHERE s.meeting_id = ?1 AND ( \
           (s.deleted = 1 AND (s.text <> '' OR s.words <> '[]')) \
           OR (?2 AND s.pass = 'live' AND s.deleted = 0 \
               AND EXISTS (SELECT 1 FROM track t WHERE t.id = s.track_id \
                           AND t.meeting_id = s.meeting_id AND t.deleted = 0) \
               AND EXISTS (SELECT 1 FROM segment f WHERE f.meeting_id = s.meeting_id \
                           AND f.track_id = s.track_id AND f.pass = 'final' AND f.deleted = 0))) \
         ORDER BY s.rowid LIMIT ?3";
    let limit = i64::try_from(limit).unwrap_or(i64::MAX);
    let mut st = c.prepare(sql)?;
    let rows = st
        .query_map(
            params![&meeting.as_uuid().as_bytes()[..], owner, limit],
            |r| r.get(0),
        )?
        .collect::<Result<Vec<i64>, _>>()?;
    Ok(rows)
}

/// The rowids of `meeting`'s rows that hold text, for a meeting that reads deleted here.
fn text_rowids(c: &Connection, meeting: EntityId, limit: usize) -> Result<Vec<i64>, StoreError> {
    let limit = i64::try_from(limit).unwrap_or(i64::MAX);
    let mut st = c.prepare(&format!(
        "SELECT rowid FROM segment WHERE meeting_id = ?1 AND {HOLDS_TEXT} ORDER BY rowid LIMIT ?2"
    ))?;
    let rows = st
        .query_map(params![&meeting.as_uuid().as_bytes()[..], limit], |r| {
            r.get(0)
        })?
        .collect::<Result<Vec<i64>, _>>()?;
    Ok(rows)
}

/// [`StoreTx::clear_replaced_text`] over a connection.
fn clear_in<C: Clock + ?Sized>(
    c: &Connection,
    meeting: EntityId,
    limit: Option<usize>,
    hlc: &mut Hlc,
    clock: &C,
) -> Result<Cleared, StoreError> {
    let wanted = limit.map_or(usize::MAX, |l| l.saturating_add(1));
    let deleted = crate::repo::row_reads_deleted(c, "meeting", meeting)?;
    let rowids = if deleted {
        text_rowids(c, meeting, wanted)?
    } else {
        replaced_rowids(c, meeting, is_owner(c, meeting)?, wanted)?
    };
    let take = limit.unwrap_or(usize::MAX);
    let more = rowids.len() > take;
    let mut rows = 0;
    for rowid in rowids.into_iter().take(take) {
        let at = hlc.now(clock);
        let written = if deleted {
            erase_rowid(c, rowid, at)?
        } else {
            clear_rowid(c, rowid, at)?
        };
        rows += usize::from(written);
    }
    Ok(Cleared { rows, more })
}

impl StoreTx<'_> {
    /// Writes `meeting`'s replaced rows (module docs) in the cleared form, at most `limit` of
    /// them (`None`: all), each with its own stamp from `hlc`; in a meeting that reads deleted
    /// here, every row that holds text in the erasure form. Idempotent: a second call writes
    /// nothing. On a device that is not the meeting's owner only P1 can match.
    ///
    /// # Errors
    /// Store errors.
    pub fn clear_replaced_text<C: Clock + ?Sized>(
        &mut self,
        meeting: EntityId,
        limit: Option<usize>,
        hlc: &mut Hlc,
        clock: &C,
    ) -> Result<Cleared, StoreError> {
        clear_in(&self.conn, meeting, limit, hlc, clock)
    }

    /// `meeting`'s segment rows that read, in the order of `segments`, and how many do not read
    /// (T14.26): for readers on the apply path, where one damaged row must not fail the page.
    ///
    /// # Errors
    /// Store errors other than a row that does not read.
    pub fn segments_that_read(
        &self,
        meeting: EntityId,
    ) -> Result<(Vec<earshot_core::model::SegmentRow>, usize), StoreError> {
        let (rows, damaged) = crate::repo::segments_or_damaged(&self.conn, meeting)?;
        Ok((rows, damaged.len()))
    }

    /// What `put_segment` does after writing `row` in the transaction: [`StoreTx::note_line`].
    #[allow(clippy::unnecessary_wraps)]
    pub(crate) fn after_put_segment(
        &mut self,
        row: &earshot_core::model::SegmentRow,
    ) -> Result<(), earshot_core::model::RepoError> {
        self.note_line(row);
        Ok(())
    }

    /// Notes that the transaction wrote `row`: a live final row is checked before the commit
    /// ("lines go out before what they replace", 15.2). Any other row is not noted.
    pub(crate) fn note_line(&self, row: &earshot_core::model::SegmentRow) {
        if row.pass == earshot_core::model::TranscriptPass::Final && !row.meta.deleted {
            self.noted
                .borrow_mut()
                .insert((row.meeting_id, row.segment.track));
        }
    }

    /// Notes every meeting and track with queued cleared rows (a re-queue, 15.2).
    pub(crate) fn note_every_cleared_track(&self) -> Result<(), StoreError> {
        let mut st = self.conn.prepare(
            "SELECT DISTINCT s.meeting_id, s.track_id FROM segment s \
             JOIN sync_outbox o ON o.tbl = 'segment' AND o.id = s.id \
             WHERE s.deleted = 1 AND s.text = '' AND s.words = '[]' AND s.confidence = 1.0",
        )?;
        let pairs: Vec<([u8; 16], [u8; 16])> = st
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
            .filter_map(Result::ok)
            .collect();
        let mut noted = self.noted.borrow_mut();
        for (m, t) in pairs {
            noted.insert((EntityId::from_bytes(m), EntityId::from_bytes(t)));
        }
        Ok(())
    }

    /// "Lines go out before what they replace" (15.2) for every track noted so far, with the
    /// caller's HLC: where it does not hold, the track's queued cleared rows are stamped again
    /// with `hlc`, above every row the transaction wrote. Returns how many rows were stamped
    /// again. Run by every writer of a live final row before its commit, with the device HLC
    /// (`Db::parts()` in the runtime): the store has no stamp source of its own, and the commit
    /// only checks that nothing was left to stamp ([`StoreTx::assert_lines_checked`]).
    ///
    /// # Errors
    /// Store errors.
    pub fn send_lines_first<C: Clock + ?Sized>(
        &mut self,
        hlc: &mut Hlc,
        clock: &C,
    ) -> Result<usize, StoreError> {
        let noted = std::mem::take(&mut *self.noted.borrow_mut());
        let mut stamped = 0;
        for (meeting, track) in noted {
            if !lines_go_first(&self.conn, self.push_guard.as_ref(), meeting, track)? {
                stamped += restamp_cleared(&self.conn, meeting, track, &mut || hlc.now(clock))?;
            }
        }
        Ok(stamped)
    }

    /// Run at every commit: a track noted and not checked by [`StoreTx::send_lines_first`] whose
    /// cleared rows would need stamping again is a writer that forgot the check, a programming
    /// error: a debug assertion. In a release build the rows stay as they are (the next
    /// writer of a line of the track, or a re-queue, runs the check). A noted track that already
    /// meets the invariant needs nothing.
    pub(crate) fn assert_lines_checked(&mut self) -> Result<(), StoreError> {
        let noted = std::mem::take(&mut *self.noted.borrow_mut());
        for (meeting, track) in noted {
            let holds = lines_go_first(&self.conn, self.push_guard.as_ref(), meeting, track)?;
            debug_assert!(
                holds,
                "a live final line was written without `send_lines_first` and its track's \
                 cleared rows now go out before it (docs/sync-spec.md 15.2)"
            );
        }
        Ok(())
    }
}

/// The lowest stamp of the queued cleared rows of `meeting`'s `track`.
fn lowest_queued_cleared(
    c: &Connection,
    meeting: EntityId,
    track: EntityId,
) -> Result<Option<(i64, i64, Vec<u8>)>, StoreError> {
    Ok(c.query_row(
        "SELECT s.updated_wall, s.updated_logical, s.updated_node FROM segment s \
         JOIN sync_outbox o ON o.tbl = 'segment' AND o.id = s.id \
         WHERE s.meeting_id = ?1 AND s.track_id = ?2 AND s.deleted = 1 AND s.text = '' \
         AND s.words = '[]' AND s.confidence = 1.0 \
         ORDER BY s.updated_wall, s.updated_logical, s.updated_node LIMIT 1",
        params![
            &meeting.as_uuid().as_bytes()[..],
            &track.as_uuid().as_bytes()[..]
        ],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    )
    .optional()?)
}

/// The check of 15.2: whether `meeting`'s `track` meets "lines go out before what they
/// replace". It holds when the track has no queued cleared row, or when some live final row of
/// the track is not queued (it came from the relay or was pushed), or is queued, sendable (it
/// reads and the push's guard would seal it) and stamped below the lowest queued cleared row.
pub(crate) fn lines_go_first(
    c: &Connection,
    guard: Option<&PushGuard>,
    meeting: EntityId,
    track: EntityId,
) -> Result<bool, StoreError> {
    let Some((wall, logical, node)) = lowest_queued_cleared(c, meeting, track)? else {
        return Ok(true);
    };
    let (m, t) = (
        meeting.as_uuid().as_bytes().to_vec(),
        track.as_uuid().as_bytes().to_vec(),
    );
    if any(
        c,
        "SELECT 1 FROM segment s WHERE s.meeting_id = ?1 AND s.track_id = ?2 \
         AND s.pass = 'final' AND s.deleted = 0 \
         AND NOT EXISTS (SELECT 1 FROM sync_outbox o WHERE o.tbl = 'segment' AND o.id = s.id)",
        &[&m, &t],
    )? {
        return Ok(true);
    }
    let ids: Vec<[u8; 16]> = {
        let mut st = c.prepare(
            "SELECT s.id FROM segment s JOIN sync_outbox o ON o.tbl = 'segment' AND o.id = s.id \
             WHERE s.meeting_id = ?1 AND s.track_id = ?2 AND s.pass = 'final' AND s.deleted = 0 \
             AND (s.updated_wall, s.updated_logical, s.updated_node) < (?3, ?4, ?5) \
             ORDER BY s.updated_wall, s.updated_logical, s.updated_node",
        )?;
        st.query_map(params![m, t, wall, logical, node], |r| r.get(0))?
            .collect::<Result<_, _>>()?
    };
    for id in ids {
        if let Ok(Some(row)) =
            crate::repo::synced_by_id_or_damaged(c, "segment", EntityId::from_bytes(id))?
            && guard.is_none_or(|g| g.sendable(&row))
        {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Stamps every queued cleared row of `meeting`'s `track` again with `next` (one stamp per
/// row); its content is unchanged. Returns how many.
fn restamp_cleared(
    c: &Connection,
    meeting: EntityId,
    track: EntityId,
    next: &mut dyn FnMut() -> HlcTimestamp,
) -> Result<usize, StoreError> {
    let rowids: Vec<i64> = {
        let mut st = c.prepare(
            "SELECT s.rowid FROM segment s JOIN sync_outbox o ON o.tbl = 'segment' AND o.id = s.id \
             WHERE s.meeting_id = ?1 AND s.track_id = ?2 AND s.deleted = 1 AND s.text = '' \
             AND s.words = '[]' AND s.confidence = 1.0 \
             ORDER BY s.updated_wall, s.updated_logical, s.updated_node",
        )?;
        st.query_map(
            params![
                &meeting.as_uuid().as_bytes()[..],
                &track.as_uuid().as_bytes()[..]
            ],
            |r| r.get(0),
        )?
        .collect::<Result<_, _>>()?
    };
    for rowid in &rowids {
        let (wall, logical, node) = stamp_params(next())?;
        c.execute(
            "UPDATE segment SET updated_wall = ?1, updated_logical = ?2, updated_node = ?3 \
             WHERE rowid = ?4",
            params![wall, logical, node, rowid],
        )?;
    }
    Ok(rowids.len())
}

impl Store {
    /// [`StoreTx::clear_replaced_text`] in one IMMEDIATE transaction of its own (the sweep,
    /// 15.6), followed by nothing: the caller flushes the erasures from the WAL.
    ///
    /// # Errors
    /// Store errors; nothing is written then.
    pub fn clear_replaced_text<C: Clock + ?Sized>(
        &mut self,
        meeting: EntityId,
        limit: usize,
        hlc: &mut Hlc,
        clock: &C,
    ) -> Result<Cleared, StoreError> {
        self.immediate(|tx| {
            let out = tx.clear_replaced_text(meeting, Some(limit), hlc, clock)?;
            tx.send_lines_first(hlc, clock)?;
            Ok(out)
        })
    }

    /// Whether this store's device is `meeting`'s owner (15.2).
    ///
    /// # Errors
    /// Store errors.
    pub fn owns_meeting(&self, meeting: EntityId) -> Result<bool, StoreError> {
        is_owner(&self.conn, meeting)
    }

    /// Whether "lines go out before what they replace" holds for `meeting`'s `track` now
    /// (15.2; tests and the sync page's counts).
    ///
    /// # Errors
    /// Store errors.
    pub fn lines_go_first(&self, meeting: EntityId, track: EntityId) -> Result<bool, StoreError> {
        lines_go_first(&self.conn, self.push_guard.as_ref(), meeting, track)
    }

    /// The ids of every meeting row, tombstones included, for the sweep (15.6).
    ///
    /// # Errors
    /// Store errors.
    pub fn meeting_ids(&self) -> Result<Vec<EntityId>, StoreError> {
        let mut st = self.conn.prepare("SELECT id FROM meeting ORDER BY id")?;
        let ids = st
            .query_map([], |r| r.get::<_, Vec<u8>>(0))?
            .filter_map(Result::ok)
            .filter_map(|b| <[u8; 16]>::try_from(b).ok())
            .map(EntityId::from_bytes)
            .collect();
        Ok(ids)
    }

    /// Whether the stored row `id` holds text by the SQL form of [`HOLDS_TEXT`] (tests pin it
    /// to `SegmentRow::holds_text`).
    ///
    /// # Errors
    /// Store errors.
    pub fn segment_holds_text_in_sql(&self, id: EntityId) -> Result<Option<bool>, StoreError> {
        Ok(self
            .conn
            .query_row(
                &format!("SELECT {HOLDS_TEXT} FROM segment WHERE id = ?1"),
                params![&id.as_uuid().as_bytes()[..]],
                |r| r.get(0),
            )
            .optional()?)
    }

    /// What `put_segment` does after writing `row` outside a transaction.
    pub(crate) fn after_put_segment(
        &mut self,
        row: &earshot_core::model::SegmentRow,
    ) -> Result<(), earshot_core::model::RepoError> {
        self.line_written_alone(row)
            .map_err(|e| earshot_core::model::RepoError::Storage(e.to_string()))
    }

    /// [`StoreTx::assert_lines_checked`] for a single row this store wrote outside a transaction
    /// (an autocommit `put_segment`, which has no HLC to stamp with).
    pub(crate) fn line_written_alone(
        &mut self,
        row: &earshot_core::model::SegmentRow,
    ) -> Result<(), StoreError> {
        if row.pass != earshot_core::model::TranscriptPass::Final || row.meta.deleted {
            return Ok(());
        }
        let holds = lines_go_first(
            &self.conn,
            self.push_guard.as_ref(),
            row.meeting_id,
            row.segment.track,
        )?;
        debug_assert!(
            holds,
            "a live final line was written outside a transaction and its track's cleared rows \
             now go out before it (docs/sync-spec.md 15.2)"
        );
        Ok(())
    }
}
