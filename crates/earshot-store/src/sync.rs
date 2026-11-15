//! The store's side of sync (T11.1b; docs/sync-spec.md sections 5.2 to 5.4).
//!
//! - The outbox ([`Store::outbox`], [`Store::outbox_sent`]): every synced row changed on this
//!   device, named by the triggers of store migration 6, read in `updated_at` order so a
//!   meeting goes out before its tracks and a deletion's cleared rows before its meeting.
//! - Applying pulled rows ([`Store::apply_remote`]): one transaction per page, whole-row last
//!   writer wins on `updated_at`, and a row that cannot be applied yet is held, still sealed,
//!   in `sync_pending` ([`Store::held_rows`]) with a reason, and tried again later.
//!
//! The store never sees a vault key: the sync client opens rows (`earshot-sync`) and hands
//! the store decoded [`SyncedRow`]s with their sealed bodies, which a held row keeps.
//!
//! What a pulled meeting deletion means for this device's audio and device-local records is
//! the client's: it runs the ordinary meeting deletion before it applies the tombstone, so
//! files go before rows exactly as a local deletion does. A pulled person tombstone forgets
//! that identity's biometrics here, in the same transaction.

use earshot_core::identity::{Clock, EntityId, Hlc, HlcTimestamp, SyncMeta, SyncSerialize};
use earshot_core::model::{Folder, Meeting, Note, RepoError, SegmentRow, Speaker, Summary, Track};
use earshot_core::voiceprint::{Person, Voiceprint};
use rusqlite::{OptionalExtension, params};

use crate::db::{Store, StoreError, StoreTx};
use crate::schema::SYNCED_TABLES;

/// How far past this device's clock a pulled row's stamp may be before it is held
/// ([`HoldReason::Clock`]): one bad clock must not win every later edit (24 h, a judgement).
pub const MAX_CLOCK_AHEAD_MS: u64 = 24 * 60 * 60 * 1000;

/// A row of any synced table.
#[derive(Debug, Clone, PartialEq)]
pub enum SyncedRow {
    /// `meeting`.
    Meeting(Meeting),
    /// `track`.
    Track(Track),
    /// `segment`.
    Segment(SegmentRow),
    /// `speaker`.
    Speaker(Speaker),
    /// `note`.
    Note(Note),
    /// `summary`.
    Summary(Summary),
    /// `person`.
    Person(Person),
    /// `voiceprint` (synced only when the user turns voiceprint sync on).
    Voiceprint(Voiceprint),
    /// `folder`.
    Folder(Folder),
}

macro_rules! each {
    ($self:expr, $r:ident => $e:expr) => {
        match $self {
            SyncedRow::Meeting($r) => $e,
            SyncedRow::Track($r) => $e,
            SyncedRow::Segment($r) => $e,
            SyncedRow::Speaker($r) => $e,
            SyncedRow::Note($r) => $e,
            SyncedRow::Summary($r) => $e,
            SyncedRow::Person($r) => $e,
            SyncedRow::Voiceprint($r) => $e,
            SyncedRow::Folder($r) => $e,
        }
    };
}

impl SyncedRow {
    /// Its table, which is also its `SyncSerialize::ENTITY`.
    #[must_use]
    pub fn table(&self) -> &'static str {
        fn of<T: SyncSerialize>(_: &T) -> &'static str {
            T::ENTITY
        }
        each!(self, r => of(r))
    }

    /// Its id.
    #[must_use]
    pub fn id(&self) -> EntityId {
        each!(self, r => r.sync_id())
    }

    /// Its sync metadata.
    #[must_use]
    pub fn meta(&self) -> &SyncMeta {
        each!(self, r => r.sync_meta())
    }

    /// The meeting it belongs to: its own id for a meeting, `None` for people, voiceprints
    /// and folders.
    #[must_use]
    pub fn meeting(&self) -> Option<EntityId> {
        match self {
            Self::Meeting(m) => Some(m.id),
            Self::Track(r) => Some(r.meeting_id),
            Self::Segment(r) => Some(r.meeting_id),
            Self::Speaker(r) => Some(r.meeting_id),
            Self::Note(r) => Some(r.meeting_id),
            Self::Summary(r) => Some(r.meeting_id),
            Self::Person(_) | Self::Voiceprint(_) | Self::Folder(_) => None,
        }
    }

    /// The row an opened envelope's `record` holds for `entity`; `Ok(None)` for an entity
    /// this build does not know (a newer Earshot's), which is held, never dropped.
    ///
    /// # Errors
    /// The record is not that entity's JSON.
    pub fn decode(
        entity: &str,
        record: serde_json::Value,
    ) -> Result<Option<Self>, serde_json::Error> {
        use serde_json::from_value as v;
        Ok(Some(match entity {
            "meeting" => Self::Meeting(v(record)?),
            "track" => Self::Track(v(record)?),
            "segment" => Self::Segment(v(record)?),
            "speaker" => Self::Speaker(v(record)?),
            "note" => Self::Note(v(record)?),
            "summary" => Self::Summary(v(record)?),
            "person" => Self::Person(v(record)?),
            "voiceprint" => Self::Voiceprint(v(record)?),
            "folder" => Self::Folder(v(record)?),
            _ => return Ok(None),
        }))
    }
}

/// Why a pulled row waits in `sync_pending`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum HoldReason {
    /// Written by a newer Earshot (an envelope format or an entity this build does not
    /// read): applied after an update.
    Format,
    /// Stamped more than [`MAX_CLOCK_AHEAD_MS`] past this device's clock.
    Clock,
    /// Its meeting is recording, finishing or running a job here.
    Busy,
    /// A row it belongs to (its meeting, track or speaker) has not arrived.
    Parent,
}

impl HoldReason {
    /// Every reason.
    pub const ALL: [Self; 4] = [Self::Format, Self::Clock, Self::Busy, Self::Parent];

    /// The word stored in `sync_pending.reason`.
    #[must_use]
    pub const fn word(self) -> &'static str {
        match self {
            Self::Format => "format",
            Self::Clock => "clock",
            Self::Busy => "busy",
            Self::Parent => "parent",
        }
    }

    fn of_word(w: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|r| r.word() == w)
    }
}

/// A pulled row, opened by the sync client. `Debug` names the row, never its content.
#[derive(Clone, PartialEq)]
pub struct Incoming {
    /// The row key the relay filed it under.
    pub key: [u8; 16],
    /// The relay's sequence number.
    pub seq: u64,
    /// Its `updated_at` as filed (the client checked it equals the row's).
    pub stamp: HlcTimestamp,
    /// The sealed body, kept if the row is held.
    pub body: Vec<u8>,
    /// The decoded row; `None` for a row this build cannot read ([`HoldReason::Format`]).
    pub row: Option<SyncedRow>,
}

impl std::fmt::Debug for Incoming {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Incoming")
            .field("seq", &self.seq)
            .field("stamp", &self.stamp)
            .field("body", &self.body.len())
            .field(
                "row",
                &self.row.as_ref().map(|r| (r.table(), r.id().to_string())),
            )
            .finish_non_exhaustive()
    }
}

/// A held row, as stored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Held {
    /// The row key.
    pub key: [u8; 16],
    /// The relay's sequence number when it was pulled.
    pub seq: u64,
    /// Its stamp as filed.
    pub stamp: HlcTimestamp,
    /// The sealed body.
    pub body: Vec<u8>,
    /// Why it waits.
    pub reason: HoldReason,
}

/// What may be applied now.
pub struct ApplyRules<'a> {
    /// Whether this device syncs voiceprints (`settings/sync-voiceprints`).
    pub voiceprints: bool,
    /// Whether a meeting is in use on this device (a job lease of this process).
    pub leased: &'a dyn Fn(EntityId) -> bool,
    /// This device's wall clock, Unix ms.
    pub now_ms: u64,
}

/// What a page did. Counts only.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Applied {
    /// Rows written (later than the local copy, or new).
    pub written: usize,
    /// Rows not later than the local copy: nothing changed.
    pub ignored: usize,
    /// Voiceprint rows skipped because voiceprint sync is off.
    pub skipped: usize,
    /// Rows held, per reason, in [`HoldReason::ALL`] order.
    pub held: [usize; 4],
    /// Voiceprints and centroids forgotten because a person was deleted elsewhere.
    pub biometrics_forgotten: usize,
    /// Live meeting rows refused because the meeting is deleted here (its tombstone was
    /// stamped again, so it goes out and wins at the relay).
    pub deletions_kept: usize,
    /// "Me" people merged into the earliest one (two devices each made their own).
    pub mes_merged: usize,
    /// Live summaries tombstoned because a later-created one of the same meeting stays.
    pub summaries_replaced: usize,
}

impl Applied {
    fn hold(&mut self, reason: HoldReason) {
        self.held[HoldReason::ALL
            .iter()
            .position(|r| *r == reason)
            .expect("every reason is in ALL")] += 1;
    }
}

/// A `sync_pending` row as read: key, seq, wall_ms, logical, node, body, reason.
type HeldColumns = (Vec<u8>, i64, i64, u32, Vec<u8>, Vec<u8>, String);

fn i64_of(v: u64) -> Result<i64, StoreError> {
    i64::try_from(v).map_err(|_| StoreError::Corrupt("a stamp past i64".into()))
}

fn hold(tx: &StoreTx<'_>, inc: &Incoming, reason: HoldReason) -> Result<(), StoreError> {
    tx.conn.execute(
        "INSERT OR REPLACE INTO sync_pending (key, seq, wall_ms, logical, node, body, reason)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![
            &inc.key[..],
            i64_of(inc.seq)?,
            i64_of(inc.stamp.wall_ms)?,
            inc.stamp.logical,
            &inc.stamp.node.as_uuid().as_bytes()[..],
            &inc.body,
            reason.word()
        ],
    )?;
    Ok(())
}

/// Removes a held copy of `inc`'s row key that is not later than `inc` (a newer held copy
/// stays, whatever order rows arrive in; the relay keeps one row per key and only ever a later
/// one, so today a held copy is never newer).
fn unhold(tx: &StoreTx<'_>, inc: &Incoming) -> Result<(), StoreError> {
    tx.conn.execute(
        "DELETE FROM sync_pending WHERE key = ?1 AND (wall_ms, logical, node) <= (?2, ?3, ?4)",
        params![
            &inc.key[..],
            i64_of(inc.stamp.wall_ms)?,
            inc.stamp.logical,
            &inc.stamp.node.as_uuid().as_bytes()[..]
        ],
    )?;
    Ok(())
}

/// The rules that keep devices converged (docs/sync-spec.md section 7), run inside
/// [`Store::apply_remote`]'s transaction after its writes. Each depends only on synced rows,
/// so every device settles on the same rows whatever order pages arrive in. Each repair points
/// a row at an earlier-created one, so repairs made on two devices from different subsets
/// (three "Me" people seen two at a time) form no cycle; whole-row last writer wins picks one
/// copy of each repaired row and applying it repairs nothing more.
///
/// 1. One "Me": of the live people marked `is_me` (not deleted, not merged), the one with the
///    earliest `created_at` (id as the tie-break) stays and each other is merged into it, as
///    `merge_persons` does (`merged_into` set; speaker rows and voiceprints follow the chain).
///    When the kept one still has the default name and a merged one was named by its user,
///    the kept one takes the name of the earliest-created such one, so pairing a computer
///    whose user set a name never loses it.
/// 2. One live summary per meeting: the latest-created stays and each other is tombstoned
///    with its body, as a replaced summary is (it becomes the undo). A summary and its
///    edit's pre-edit copy share a `created_at`; the id breaks that tie, deterministically,
///    and the other stays reachable as the newest tombstone.
///
/// Two live folders with one name both stay (the user decides). Run after every page, so a
/// repair also covers rows that arrived in earlier pages.
fn converge<C: Clock + ?Sized>(
    tx: &StoreTx<'_>,
    hlc: &mut Hlc,
    clock: &C,
    out: &mut Applied,
) -> Result<(), StoreError> {
    let mut mes: Vec<Person> = crate::repo::all_persons(&tx.conn)?
        .into_iter()
        .filter(|p| p.is_me && p.is_live())
        .collect();
    mes.sort_by_key(|p| (p.meta.created_at, p.id));
    if let Some((keep, rest)) = mes.split_first() {
        let default_name = earshot_core::voiceprint::DEFAULT_ME_NAME;
        let named = rest
            .iter()
            .find(|p| !p.display_name.is_empty() && p.display_name != default_name);
        if keep.display_name == default_name
            && let Some(named) = named
        {
            let mut kept = keep.clone();
            kept.display_name.clone_from(&named.display_name);
            kept.meta.touch(hlc, clock);
            crate::repo::put_person(&tx.conn, &kept)?;
        }
        for p in rest {
            let mut merged = p.clone();
            merged.merged_into = Some(keep.id);
            merged.meta.touch(hlc, clock);
            crate::repo::put_person(&tx.conn, &merged)?;
            out.mes_merged += 1;
        }
    }
    let crowded: Vec<[u8; 16]> = {
        let mut st = tx.conn.prepare(
            "SELECT meeting_id FROM summary WHERE deleted = 0 GROUP BY meeting_id HAVING count(*) > 1",
        )?;
        st.query_map([], |r| r.get(0))?.collect::<Result<_, _>>()?
    };
    for meeting in crowded {
        let mut live: Vec<Summary> =
            crate::repo::summaries_of(&tx.conn, EntityId::from_bytes(meeting))?
                .into_iter()
                .filter(|s| !s.meta.deleted)
                .collect();
        live.sort_by_key(|s| (s.meta.created_at, s.id));
        live.pop();
        for s in live {
            let mut replaced = s;
            replaced.meta.deleted = true;
            replaced.meta.touch(hlc, clock);
            crate::repo::put_summary(&tx.conn, &replaced)?;
            out.summaries_replaced += 1;
        }
    }
    Ok(())
}

impl Store {
    /// Up to `limit` rows changed on this device and not yet sent, oldest `updated_at` first,
    /// across every synced table.
    ///
    /// # Errors
    /// Store errors.
    pub fn outbox(&self, limit: usize) -> Result<Vec<SyncedRow>, StoreError> {
        self.outbox_except(limit, &[], true)
    }

    /// [`Store::outbox`] leaving out, in the query itself, the rows of the meetings in
    /// `meetings` (a meeting being recorded: its rows wait until it is saved) and, when
    /// `voiceprints` is false, every voiceprint. Left out in SQL rather than after reading, so
    /// a long recording's rows cannot fill every page and keep other rows from going.
    ///
    /// # Errors
    /// Store errors.
    pub fn outbox_except(
        &self,
        limit: usize,
        meetings: &[EntityId],
        voiceprints: bool,
    ) -> Result<Vec<SyncedRow>, StoreError> {
        let mut rows = Vec::new();
        let limit_sql = i64::try_from(limit).unwrap_or(i64::MAX);
        let skip: Vec<Vec<u8>> = meetings
            .iter()
            .map(|m| m.as_uuid().as_bytes().to_vec())
            .collect();
        let marks = vec!["?"; skip.len()].join(", ");
        for table in SYNCED_TABLES {
            if table == "voiceprint" && !voiceprints {
                continue;
            }
            let meeting_col = match table {
                "meeting" => Some("id"),
                "track" | "segment" | "speaker" | "note" | "summary" => Some("meeting_id"),
                _ => None,
            };
            let except = match meeting_col {
                Some(col) if !skip.is_empty() => format!(" AND r.{col} NOT IN ({marks})"),
                _ => String::new(),
            };
            let sql = format!(
                "SELECT o.id FROM sync_outbox o JOIN {table} r ON r.id = o.id WHERE o.tbl = ?{except} \
                 ORDER BY r.updated_wall, r.updated_logical, r.updated_node LIMIT ?"
            );
            let mut values: Vec<&dyn rusqlite::ToSql> = vec![&table];
            if !except.is_empty() {
                values.extend(skip.iter().map(|b| b as &dyn rusqlite::ToSql));
            }
            values.push(&limit_sql);
            let mut st = self.conn.prepare(&sql)?;
            let ids: Vec<[u8; 16]> = st
                .query_map(values.as_slice(), |r| r.get(0))?
                .collect::<Result<_, _>>()?;
            for id in ids {
                if let Some(row) =
                    crate::repo::synced_by_id(&self.conn, table, EntityId::from_bytes(id))?
                {
                    rows.push(row);
                }
            }
        }
        rows.sort_by_key(|r| r.meta().updated_at);
        rows.truncate(limit);
        Ok(rows)
    }

    /// Queues every synced row to be sent and drops every held row, in one transaction: for a
    /// device joining a vault (its first, or a new one after leaving another), whose earlier
    /// sends went elsewhere and whose held rows were sealed under another key.
    ///
    /// # Errors
    /// Store errors; nothing changes then.
    pub fn queue_everything(&mut self) -> Result<u64, StoreError> {
        self.immediate(|tx| {
            let mut queued = 0;
            for table in SYNCED_TABLES {
                queued += tx.conn.execute(
                    &format!(
                        "INSERT INTO sync_outbox (tbl, id) SELECT ?1, r.id FROM {table} r \
                         WHERE NOT EXISTS (SELECT 1 FROM sync_outbox o WHERE o.tbl = ?1 AND o.id = r.id)"
                    ),
                    params![table],
                )?;
            }
            tx.conn.execute("DELETE FROM sync_pending", [])?;
            Ok(u64::try_from(queued).unwrap_or(u64::MAX))
        })
    }

    /// How many rows wait to be sent.
    ///
    /// # Errors
    /// Store errors.
    pub fn outbox_len(&self) -> Result<u64, StoreError> {
        let n: i64 = self
            .conn
            .query_row("SELECT count(*) FROM sync_outbox", [], |r| r.get(0))?;
        Ok(u64::try_from(n).unwrap_or(0))
    }

    /// How many rows [`Store::outbox_except`] would send with the same exclusions: what a
    /// device shows as waiting to send (a recording's rows and switched-off voiceprints wait
    /// for a reason of their own and are not behind).
    ///
    /// # Errors
    /// Store errors.
    pub fn outbox_len_except(
        &self,
        meetings: &[EntityId],
        voiceprints: bool,
    ) -> Result<u64, StoreError> {
        let skip: Vec<Vec<u8>> = meetings
            .iter()
            .map(|m| m.as_uuid().as_bytes().to_vec())
            .collect();
        let marks = vec!["?"; skip.len()].join(", ");
        let mut total: u64 = 0;
        for table in SYNCED_TABLES {
            if table == "voiceprint" && !voiceprints {
                continue;
            }
            let meeting_col = match table {
                "meeting" => Some("id"),
                "track" | "segment" | "speaker" | "note" | "summary" => Some("meeting_id"),
                _ => None,
            };
            let except = match meeting_col {
                Some(col) if !skip.is_empty() => format!(" AND r.{col} NOT IN ({marks})"),
                _ => String::new(),
            };
            let sql = format!(
                "SELECT count(*) FROM sync_outbox o JOIN {table} r ON r.id = o.id WHERE o.tbl = ?{except}"
            );
            let mut values: Vec<&dyn rusqlite::ToSql> = vec![&table];
            if !except.is_empty() {
                values.extend(skip.iter().map(|b| b as &dyn rusqlite::ToSql));
            }
            let n: i64 = self.conn.query_row(&sql, values.as_slice(), |r| r.get(0))?;
            total += u64::try_from(n).unwrap_or(0);
        }
        Ok(total)
    }

    /// Marks rows as sent: each `(table, id, stamp)` leaves the outbox only if the row still
    /// has the `updated_at` that was sent, so a change made while it was on the way is sent
    /// next time. Returns how many left.
    ///
    /// # Errors
    /// Store errors.
    pub fn outbox_sent(
        &mut self,
        sent: &[(&str, EntityId, HlcTimestamp)],
    ) -> Result<usize, StoreError> {
        self.immediate(|tx| {
            let mut gone = 0;
            for (table, id, stamp) in sent {
                let now =
                    crate::repo::synced_by_id(&tx.conn, table, *id)?.map(|r| r.meta().updated_at);
                if now == Some(*stamp) {
                    gone += tx.conn.execute(
                        "DELETE FROM sync_outbox WHERE tbl = ?1 AND id = ?2",
                        params![table, &id.as_uuid().as_bytes()[..]],
                    )?;
                }
            }
            Ok(gone)
        })
    }

    /// Every held row, in sequence order.
    ///
    /// # Errors
    /// Store errors; [`StoreError::Corrupt`] for a row this build did not write.
    pub fn held_rows(&self) -> Result<Vec<Held>, StoreError> {
        let mut st = self.conn.prepare(
            "SELECT key, seq, wall_ms, logical, node, body, reason FROM sync_pending ORDER BY seq",
        )?;
        let raw: Vec<HeldColumns> = st
            .query_map([], |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                    r.get(6)?,
                ))
            })?
            .collect::<Result<_, _>>()?;
        raw.into_iter()
            .map(|(key, seq, wall, logical, node, body, reason)| {
                let bad = || StoreError::Corrupt("a held sync row".into());
                Ok(Held {
                    key: key.try_into().map_err(|_| bad())?,
                    seq: u64::try_from(seq).map_err(|_| bad())?,
                    stamp: HlcTimestamp {
                        wall_ms: u64::try_from(wall).map_err(|_| bad())?,
                        logical,
                        node: earshot_core::identity::DeviceId::from_bytes(
                            node.try_into().map_err(|_| bad())?,
                        ),
                    },
                    body,
                    reason: HoldReason::of_word(&reason).ok_or_else(bad)?,
                })
            })
            .collect()
    }

    /// Applies a page of pulled rows in one transaction, in order (docs/sync-spec.md
    /// section 5.3). Each row is, in this order of checks: held when this build cannot read
    /// it, skipped when it is a voiceprint and voiceprint sync is off, held when its stamp is
    /// more than [`MAX_CLOCK_AHEAD_MS`] ahead, held when its meeting is in use here, ignored
    /// when the local copy's `updated_at` is not earlier, else written whole with its own
    /// metadata (held when a row it belongs to is missing). A written row leaves no outbox
    /// entry of its own and is observed by `hlc`; a row applied or found not newer leaves
    /// `sync_pending`. A person tombstone forgets that identity's biometrics; when
    /// [`Applied::biometrics_forgotten`] is not zero the caller runs [`Store::flush_erasures`],
    /// as for any biometric deletion. One IMMEDIATE transaction: what it reads cannot change
    /// before it writes.
    ///
    /// # Errors
    /// Store errors; nothing of the page is written then.
    pub fn apply_remote<C: Clock + ?Sized>(
        &mut self,
        page: &[Incoming],
        rules: &ApplyRules<'_>,
        hlc: &mut Hlc,
        clock: &C,
    ) -> Result<Applied, StoreError> {
        self.immediate(|tx| {
            let mut out = Applied::default();
            for inc in page {
                let Some(row) = &inc.row else {
                    hold(tx, inc, HoldReason::Format)?;
                    out.hold(HoldReason::Format);
                    continue;
                };
                if matches!(row, SyncedRow::Voiceprint(_)) && !rules.voiceprints {
                    unhold(tx, inc)?;
                    out.skipped += 1;
                    continue;
                }
                if inc.stamp.wall_ms > rules.now_ms.saturating_add(MAX_CLOCK_AHEAD_MS) {
                    hold(tx, inc, HoldReason::Clock)?;
                    out.hold(HoldReason::Clock);
                    continue;
                }
                if row.meeting().is_some_and(|m| (rules.leased)(m)) {
                    hold(tx, inc, HoldReason::Busy)?;
                    out.hold(HoldReason::Busy);
                    continue;
                }
                // A live row of a meeting deleted here (edited elsewhere before that device heard
                // of the deletion) is not written: no transcript text of a deleted meeting is
                // kept. The other device deletes its copy when the tombstone reaches it.
                if !matches!(row, SyncedRow::Meeting(_))
                    && !row.meta().deleted
                    && let Some(m) = row.meeting()
                    && crate::repo::synced_by_id(&tx.conn, "meeting", m)?
                        .is_some_and(|l| l.meta().deleted)
                {
                    unhold(tx, inc)?;
                    out.deletions_kept += 1;
                    continue;
                }
                let local = crate::repo::synced_by_id(&tx.conn, row.table(), row.id())?;
                // A meeting deletion is final: a live copy edited elsewhere after it (pushed
                // before that device heard of the deletion, so the relay kept the edit) never
                // brings the meeting back. The tombstone here is stamped again, after the edit,
                // and goes out to win at the relay; the other device then deletes its copy.
                if let (Some(SyncedRow::Meeting(here)), SyncedRow::Meeting(there)) = (&local, row)
                    && here.meta.deleted
                    && !there.meta.deleted
                {
                    let _ = hlc.observe(inc.stamp, clock);
                    if here.meta.updated_at < row.meta().updated_at {
                        let mut again = here.clone();
                        again.meta.touch(hlc, clock);
                        crate::repo::put_synced(&tx.conn, &SyncedRow::Meeting(again))?;
                        out.deletions_kept += 1;
                    } else {
                        out.ignored += 1;
                    }
                    unhold(tx, inc)?;
                    continue;
                }
                if local
                    .as_ref()
                    .is_some_and(|l| l.meta().updated_at >= row.meta().updated_at)
                {
                    unhold(tx, inc)?;
                    out.ignored += 1;
                    continue;
                }
                let person_was_live = match &local {
                    Some(SyncedRow::Person(p)) => !p.meta.deleted,
                    None => matches!(row, SyncedRow::Person(_)),
                    Some(_) => false,
                };
                {
                    let sp = tx.conn.savepoint()?;
                    match crate::repo::put_synced(&sp, row) {
                        Ok(()) => {
                            // The trigger named it; it came from elsewhere, so it is not sent.
                            sp.execute(
                                "DELETE FROM sync_outbox WHERE tbl = ?1 AND id = ?2",
                                params![row.table(), &row.id().as_uuid().as_bytes()[..]],
                            )?;
                            sp.commit()?;
                        }
                        Err(RepoError::MissingParent { .. }) => {
                            drop(sp);
                            hold(tx, inc, HoldReason::Parent)?;
                            out.hold(HoldReason::Parent);
                            continue;
                        }
                        Err(e) => return Err(e.into()),
                    }
                }
                unhold(tx, inc)?;
                let _ = hlc.observe(inc.stamp, clock);
                out.written += 1;
                if let SyncedRow::Person(p) = row
                    && p.meta.deleted
                    && person_was_live
                {
                    out.biometrics_forgotten +=
                        crate::identities::forget_identity_biometrics(tx, p.id, hlc, clock)?;
                }
            }
            converge(tx, hlc, clock, &mut out)?;
            Ok(out)
        })
    }

    /// Whether a held row with `key` exists (tests and status lines).
    ///
    /// # Errors
    /// Store errors.
    pub fn is_held(&self, key: &[u8; 16]) -> Result<bool, StoreError> {
        Ok(self
            .conn
            .query_row(
                "SELECT 1 FROM sync_pending WHERE key = ?1",
                params![&key[..]],
                |_| Ok(()),
            )
            .optional()?
            .is_some())
    }
}
