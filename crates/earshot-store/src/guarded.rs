//! The guarded paths (T14.57, docs/sync-spec.md section 16): code that must not stop on one
//! damaged row (a sync page and everything around it, every biometric erasure the user starts,
//! the People list they start from, the one-off start-up repairs) reaches the store only
//! through [`GuardedTx`] (inside a transaction) or [`GuardedRead`] (a read under the store
//! lock). Neither implements a repository trait or gives the connection: they offer readers
//! built on `split_rows` (a row that does not read comes back as its rowid), lenient column
//! reads ([`Col`]), named lenient aggregates, the named methods the page loop needs and the
//! named writers of 16.4, so a decoding reader cannot be called on a guarded path.
//!
//! A row that does not read is skipped and counted, once per `(table, rowid)`
//! ([`GuardedTx::damaged_skipped`]); any other store error fails as before. A damaged row is
//! written only by rowid, by a named writer, under the written-columns rule of 16.4. Every SQL
//! statement this module runs is a named constant below, used only by its owner
//! ([`SQL_OWNERS`]); the scan test `sync_damaged_rows_never_stop_the_guarded_paths` in
//! earshot-runtime checks both.

use std::cell::RefCell;
use std::collections::BTreeSet;

use earshot_core::diar::SpeakerRole;
use earshot_core::identity::{Clock, EntityId, Hlc, HlcTimestamp};
use earshot_core::model::{DictionaryWord, MeetingRepo, RepoError, Speaker, Summary};
use earshot_core::voiceprint::{Person, Voiceprint, VoiceprintRepo, VoiceprintSample};
use rusqlite::types::ValueRef;
use rusqlite::{Connection, params};

use crate::sync::{HoldReason, Incoming, SyncedRow};
use crate::{Store, StoreError, StoreTx};

// ---- the SQL of the guarded paths, each used only by its owner (`SQL_OWNERS`) ----

/// One step of a `merged_into` chain: the person's `deleted` and `merged_into`.
const CHAIN_STEP_SQL: &str = "SELECT deleted, merged_into FROM person WHERE id = ?1";
/// Every person row's links, by id (core `identity_of`'s order).
const PERSON_LINKS_SQL: &str =
    "SELECT rowid, id, deleted, is_me, merged_into FROM person ORDER BY id";
/// One meeting's speaker rows' links, in creation order.
const SPEAKER_LINKS_SQL: &str = "SELECT rowid, id, meeting_id, person_id, deleted, \
     display_name, role FROM speaker WHERE meeting_id = ?1 \
     ORDER BY created_wall, created_logical, created_node, id";
/// Every speaker row's links.
const ALL_SPEAKER_LINKS_SQL: &str = "SELECT rowid, id, meeting_id, person_id, deleted, \
     display_name, role FROM speaker ORDER BY rowid";
/// Every meeting row's id and `deleted`, in `all_meetings`' order.
const MEETINGS_LENIENT_SQL: &str =
    "SELECT rowid, id, deleted FROM meeting ORDER BY started_unix_ms, id";
/// One meeting row's `deleted`.
const MEETING_DELETED_SQL: &str = "SELECT deleted FROM meeting WHERE id = ?1";
/// The current summary's id, as `repo::CURRENT_SUMMARY_SQL` chooses it (pinned equal).
const CURRENT_SUMMARY_ID_SQL: &str = "SELECT id FROM summary \
     WHERE meeting_id = ?1 AND deleted = 0 \
     ORDER BY updated_wall DESC, updated_logical DESC, updated_node DESC, \
     created_wall DESC, created_logical DESC, created_node DESC, id DESC LIMIT 1";
/// Rule 2's live summaries' meetings, grouped by [`crowded_meetings`] (a `meeting_id` that
/// does not read is left out by its rowid).
const CROWDED_MEETINGS_SQL: &str =
    "SELECT rowid, meeting_id FROM summary WHERE deleted = 0 ORDER BY meeting_id";
/// The People page's (person, meeting) pairs (`repo::person_meetings`' query).
const PERSON_MEETINGS_SQL: &str = "SELECT DISTINCT s.person_id, s.meeting_id FROM speaker s \
     JOIN meeting m ON m.id = s.meeting_id \
     WHERE s.deleted = 0 AND m.deleted = 0 AND s.person_id IS NOT NULL";
/// Every print's links, whether its vector reads, by id.
const VOICE_PRINTS_SQL: &str = "SELECT rowid, id, person_id, deleted, typeof(sum), \
     length(CAST(sum AS BLOB)) FROM voiceprint ORDER BY id";
/// Every live sample's links, by id.
const VOICE_SAMPLES_SQL: &str = "SELECT rowid, id, person_id, voiceprint_id, meeting_id, \
     speaker_id FROM voiceprint_sample WHERE deleted = 0 ORDER BY id";
/// Rule 3a's samples: live samples naming a person (today's query, with the rowid).
const DEAD_PERSON_SAMPLES_SQL: &str = "SELECT rowid, id, person_id, meeting_id \
     FROM voiceprint_sample WHERE deleted = 0 AND person_id IS NOT NULL";
/// Rule 3a's prints: live prints (today's query, with the rowid).
const DEAD_PERSON_PRINTS_SQL: &str = "SELECT rowid, person_id FROM voiceprint WHERE deleted = 0";
/// Rule 3: live samples of a print tombstoned here.
const DEAD_PRINT_SAMPLES_SQL: &str = "SELECT s.rowid, s.id, s.meeting_id \
     FROM voiceprint_sample s JOIN voiceprint v ON v.id = s.voiceprint_id \
     WHERE s.deleted = 0 AND v.deleted = 1";
/// The keys of every meeting's speaker identity record.
const IDENTITY_RECORD_KEYS_SQL: &str = "SELECT key FROM local_state WHERE substr(key, 1, ?1) = ?2";
/// Erases a live sample whose id reads (every column a constant of the erased form).
const ERASE_SAMPLE_BY_ID_SQL: &str = "UPDATE voiceprint_sample SET deleted = 1, \
     voiceprint_id = NULL, person_id = NULL, model = NULL, vector = x'', speech_ms = NULL, \
     meeting_id = NULL, speaker_id = NULL, \
     updated_wall = ?1, updated_logical = ?2, updated_node = ?3 \
     WHERE id = ?4 AND deleted = 0";
/// Erases a live sample by rowid (its id does not read): the erased form, `id` as stored.
const ERASE_SAMPLE_BY_ROWID_SQL: &str = "UPDATE voiceprint_sample SET deleted = 1, \
     voiceprint_id = NULL, person_id = NULL, model = NULL, vector = x'', speech_ms = NULL, \
     meeting_id = NULL, speaker_id = NULL, \
     updated_wall = ?1, updated_logical = ?2, updated_node = ?3 \
     WHERE rowid = ?4 AND deleted = 0";
/// Tombstones a print by rowid with its vector zeroed whatever it reads as (16.4, the owner's
/// decided case: the length comes from the stored value).
const ERASE_PRINT_BY_ROWID_SQL: &str = "UPDATE voiceprint SET deleted = 1, \
     sum = zeroblob(length(CAST(sum AS BLOB))), \
     updated_wall = ?1, updated_logical = ?2, updated_node = ?3 WHERE rowid = ?4";
/// Unlinks a speaker row by rowid: `person_id` NULL, `ignored` 0, the name only when given
/// (`?5` NULL keeps the stored bytes), the stamp.
const UNLINK_SPEAKER_BY_ROWID_SQL: &str = "UPDATE speaker SET person_id = NULL, ignored = 0, \
     display_name = coalesce(?5, display_name), \
     updated_wall = ?1, updated_logical = ?2, updated_node = ?3 WHERE rowid = ?4";
/// Tombstones a person by rowid with the name cleared.
const TOMBSTONE_PERSON_BY_ROWID_SQL: &str = "UPDATE person SET deleted = 1, display_name = '', \
     updated_wall = ?1, updated_logical = ?2, updated_node = ?3 WHERE rowid = ?4";
/// Holds a pulled row (`sync_pending`).
const HOLD_SQL: &str = "INSERT OR REPLACE INTO sync_pending (key, seq, wall_ms, logical, node, body, reason) \
     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)";
/// Removes a held copy not later than the row.
const UNHOLD_SQL: &str =
    "DELETE FROM sync_pending WHERE key = ?1 AND (wall_ms, logical, node) <= (?2, ?3, ?4)";
/// Removes a row's outbox entry.
const OUTBOX_FORGET_SQL: &str = "DELETE FROM sync_outbox WHERE tbl = ?1 AND id = ?2";

/// Each SQL constant of this module and the one function allowed to use it (the scan test
/// reads this list from the source).
pub const SQL_OWNERS: &[(&str, &str)] = &[
    ("CHAIN_STEP_SQL", "chain_step"),
    ("PERSON_LINKS_SQL", "person_links"),
    ("SPEAKER_LINKS_SQL", "speaker_links"),
    ("ALL_SPEAKER_LINKS_SQL", "speaker_links"),
    ("MEETINGS_LENIENT_SQL", "meetings_lenient"),
    ("MEETING_DELETED_SQL", "meeting_deleted"),
    ("CURRENT_SUMMARY_ID_SQL", "current_summary_id_lenient"),
    ("CROWDED_MEETINGS_SQL", "crowded_meetings"),
    ("PERSON_MEETINGS_SQL", "person_meetings_lenient"),
    ("VOICE_PRINTS_SQL", "voice_prints"),
    ("VOICE_SAMPLES_SQL", "voice_samples"),
    ("DEAD_PRINT_SAMPLES_SQL", "dead_print_samples"),
    ("DEAD_PERSON_SAMPLES_SQL", "dead_person_samples"),
    ("DEAD_PERSON_PRINTS_SQL", "dead_person_prints"),
    ("IDENTITY_RECORD_KEYS_SQL", "identity_record_keys"),
    ("ERASE_SAMPLE_BY_ID_SQL", "erase_sample_by_id"),
    ("ERASE_SAMPLE_BY_ROWID_SQL", "erase_sample_by_rowid"),
    ("ERASE_PRINT_BY_ROWID_SQL", "erase_print_by_rowid"),
    ("UNLINK_SPEAKER_BY_ROWID_SQL", "unlink_speaker_by_rowid"),
    ("TOMBSTONE_PERSON_BY_ROWID_SQL", "tombstone_person_by_rowid"),
    ("HOLD_SQL", "hold"),
    ("UNHOLD_SQL", "unhold"),
    ("OUTBOX_FORGET_SQL", "outbox_forget"),
];

// ---- lenient values ----

/// A column read without failing: its value, or `Unreadable` when the stored value does not
/// convert for this build.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Col<T> {
    /// The value.
    Read(T),
    /// The stored value does not convert.
    Unreadable,
}

impl<T> Col<T> {
    /// The value, `None` when it does not read.
    pub fn read(self) -> Option<T> {
        match self {
            Col::Read(v) => Some(v),
            Col::Unreadable => None,
        }
    }

    /// Whether it reads.
    pub fn reads(&self) -> bool {
        matches!(self, Col::Read(_))
    }
}

/// A row's `deleted` column, three ways.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Deleted {
    /// A tombstone.
    Yes,
    /// Live.
    No,
    /// The column does not read.
    Unreadable,
}

/// A rowid as SQLite gives it (always an integer).
fn col_rowid(v: ValueRef<'_>) -> i64 {
    match v {
        ValueRef::Integer(i) => i,
        _ => -1,
    }
}

fn col_id(v: ValueRef<'_>) -> Col<EntityId> {
    match v {
        ValueRef::Blob(b) => {
            <[u8; 16]>::try_from(b).map_or(Col::Unreadable, |b| Col::Read(EntityId::from_bytes(b)))
        }
        _ => Col::Unreadable,
    }
}

fn col_opt_id(v: ValueRef<'_>) -> Col<Option<EntityId>> {
    match v {
        ValueRef::Null => Col::Read(None),
        other => match col_id(other) {
            Col::Read(id) => Col::Read(Some(id)),
            Col::Unreadable => Col::Unreadable,
        },
    }
}

fn col_bool(v: ValueRef<'_>) -> Col<bool> {
    match v {
        ValueRef::Integer(i) => Col::Read(i != 0),
        _ => Col::Unreadable,
    }
}

fn col_deleted(v: ValueRef<'_>) -> Deleted {
    match col_bool(v) {
        Col::Read(true) => Deleted::Yes,
        Col::Read(false) => Deleted::No,
        Col::Unreadable => Deleted::Unreadable,
    }
}

fn col_text(v: ValueRef<'_>) -> Col<String> {
    match v {
        ValueRef::Text(t) => {
            std::str::from_utf8(t).map_or(Col::Unreadable, |s| Col::Read(s.into()))
        }
        _ => Col::Unreadable,
    }
}

fn col_role(v: ValueRef<'_>) -> Col<SpeakerRole> {
    match col_text(v) {
        Col::Read(s) => {
            serde_json::from_value(serde_json::Value::String(s)).map_or(Col::Unreadable, Col::Read)
        }
        Col::Unreadable => Col::Unreadable,
    }
}

fn stamp_params(at: HlcTimestamp) -> Result<(i64, u32, Vec<u8>), StoreError> {
    Ok((
        i64::try_from(at.wall_ms).map_err(|_| StoreError::Corrupt("a stamp past i64".into()))?,
        at.logical,
        at.node.as_uuid().as_bytes().to_vec(),
    ))
}

// ---- what the lenient readers give ----

/// A speaker row's links, each column read leniently.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpeakerLink {
    /// The SQLite rowid.
    pub rowid: i64,
    /// `id`.
    pub id: Col<EntityId>,
    /// `meeting_id`.
    pub meeting_id: Col<EntityId>,
    /// `person_id`.
    pub person_id: Col<Option<EntityId>>,
    /// `deleted`.
    pub deleted: Deleted,
    /// `display_name`.
    pub display_name: Col<String>,
    /// `role`.
    pub role: Col<SpeakerRole>,
}

/// A person row's links, each column read leniently.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PersonLink {
    /// The SQLite rowid.
    pub rowid: i64,
    /// `id`.
    pub id: Col<EntityId>,
    /// `deleted`.
    pub deleted: Deleted,
    /// `is_me`.
    pub is_me: Col<bool>,
    /// `merged_into`.
    pub merged_into: Col<Option<EntityId>>,
}

/// A meeting row's id and `deleted`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MeetingLink {
    /// The SQLite rowid.
    pub rowid: i64,
    /// `id`.
    pub id: Col<EntityId>,
    /// `deleted`.
    pub deleted: Deleted,
}

/// A print's links ([`GuardedTx::voice_rows_of`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PrintLink {
    /// The SQLite rowid.
    pub rowid: i64,
    /// `id`.
    pub id: Col<EntityId>,
    /// `person_id`.
    pub person_id: Col<EntityId>,
    /// `deleted`.
    pub deleted: Deleted,
    /// Whether the stored vector reads (a blob of whole floats).
    pub sum_reads: bool,
}

/// A live sample's links ([`GuardedTx::voice_rows_of`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SampleLink {
    /// The SQLite rowid.
    pub rowid: i64,
    /// `id`.
    pub id: Col<EntityId>,
    /// `person_id`.
    pub person_id: Col<Option<EntityId>>,
    /// `voiceprint_id`.
    pub voiceprint_id: Col<Option<EntityId>>,
    /// `meeting_id`.
    pub meeting_id: Col<Option<EntityId>>,
    /// `speaker_id`.
    pub speaker_id: Col<Option<EntityId>>,
}

impl SampleLink {
    /// A live sample of the user's own voice ("Your voice", `SampleOrigin::Own`): both origin
    /// columns read as NULL (16.5 item 23a). A sample whose origin does not read is not
    /// evidence.
    #[must_use]
    pub fn is_own(&self) -> bool {
        self.meeting_id == Col::Read(None) && self.speaker_id == Col::Read(None)
    }
}

/// The prints and live samples of an identity ([`GuardedTx::voice_rows_of`]).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct VoiceRows {
    /// Prints whose `person_id` reads as a member, in member order, each member's by id.
    pub prints: Vec<PrintLink>,
    /// Live samples whose `person_id` reads as a member or whose `voiceprint_id` reads as one
    /// of [`VoiceRows::prints`], by id (those whose id does not read last, by rowid).
    pub samples: Vec<SampleLink>,
}

/// Where a `merged_into` chain ends ([`chain_end`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChainEnd {
    /// At a step whose `deleted` reads `Yes`.
    Removed,
    /// At a live step with no `merged_into`.
    Live,
    /// A loop, a person not known here, or a step whose `deleted` or `merged_into` does not
    /// read.
    Unknown,
}

/// An identity as [`identity_tx`] places it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Identity {
    /// The person first, then everyone whose chain reaches it, by id.
    pub members: Vec<EntityId>,
    /// Persons that could not be placed (an `id` or a chain step that does not read).
    pub skipped: usize,
}

type Skips = RefCell<BTreeSet<(&'static str, i64)>>;

/// One chain step's `deleted` and `merged_into`.
type ChainStep = (Deleted, Col<Option<EntityId>>);

/// A transaction on a guarded path (module docs).
pub struct GuardedTx<'g, 'c> {
    tx: &'g mut StoreTx<'c>,
    skipped: Skips,
}

/// A read under the store lock on a guarded path (module docs).
pub struct GuardedRead<'g> {
    store: &'g Store,
    skipped: Skips,
}

impl Store {
    /// A guarded read of this store (docs/sync-spec.md 16.3).
    #[must_use]
    pub fn guarded_read(&self) -> GuardedRead<'_> {
        GuardedRead {
            store: self,
            skipped: Skips::default(),
        }
    }

    /// [`Store::transaction`] for a guarded root.
    ///
    /// # Errors
    /// `f`'s error; SQLite errors.
    pub fn guarded_transaction<T, E: From<StoreError>>(
        &mut self,
        f: impl FnOnce(&mut GuardedTx<'_, '_>) -> Result<T, E>,
    ) -> Result<T, E> {
        self.transaction(|tx| f(&mut tx.guarded()))
    }

    /// An `IMMEDIATE` transaction for a guarded root: the write lock is held before its first
    /// read (docs/sync-spec.md 16.3).
    ///
    /// # Errors
    /// `f`'s error; SQLite errors.
    pub fn guarded_immediate<T>(
        &mut self,
        f: impl FnOnce(&mut GuardedTx<'_, '_>) -> Result<T, StoreError>,
    ) -> Result<T, StoreError> {
        self.immediate(|tx| f(&mut tx.guarded()))
    }
}

impl<'c> StoreTx<'c> {
    /// This transaction as a guarded one, for store code that already holds a transaction.
    pub fn guarded(&mut self) -> GuardedTx<'_, 'c> {
        GuardedTx {
            tx: self,
            skipped: Skips::default(),
        }
    }
}

/// Records a damaged row once.
fn skip(skips: &Skips, table: &'static str, rowid: i64) {
    skips.borrow_mut().insert((table, rowid));
}

fn skip_all(skips: &Skips, table: &'static str, rowids: &[i64]) {
    for &r in rowids {
        skip(skips, table, r);
    }
}

fn repo(e: RepoError) -> StoreError {
    StoreError::Repo(e)
}

fn chain_step(c: &Connection, id: EntityId) -> Result<Option<ChainStep>, StoreError> {
    use rusqlite::OptionalExtension;
    Ok(
        c.query_row(CHAIN_STEP_SQL, params![&id.as_uuid().as_bytes()[..]], |r| {
            Ok((col_deleted(r.get_ref(0)?), col_opt_id(r.get_ref(1)?)))
        })
        .optional()?,
    )
}

fn person_links(c: &Connection) -> Result<Vec<PersonLink>, StoreError> {
    let mut st = c.prepare_cached(PERSON_LINKS_SQL)?;
    let rows = st.query_map([], |r| {
        Ok(PersonLink {
            rowid: col_rowid(r.get_ref(0)?),
            id: col_id(r.get_ref(1)?),
            deleted: col_deleted(r.get_ref(2)?),
            is_me: col_bool(r.get_ref(3)?),
            merged_into: col_opt_id(r.get_ref(4)?),
        })
    })?;
    Ok(rows.collect::<Result<_, _>>()?)
}

fn speaker_links(
    c: &Connection,
    meeting: Option<EntityId>,
) -> Result<Vec<SpeakerLink>, StoreError> {
    let link = |r: &rusqlite::Row<'_>| {
        Ok(SpeakerLink {
            rowid: col_rowid(r.get_ref(0)?),
            id: col_id(r.get_ref(1)?),
            meeting_id: col_id(r.get_ref(2)?),
            person_id: col_opt_id(r.get_ref(3)?),
            deleted: col_deleted(r.get_ref(4)?),
            display_name: col_text(r.get_ref(5)?),
            role: col_role(r.get_ref(6)?),
        })
    };
    Ok(match meeting {
        Some(m) => c
            .prepare_cached(SPEAKER_LINKS_SQL)?
            .query_map(params![&m.as_uuid().as_bytes()[..]], link)?
            .collect::<Result<_, _>>()?,
        None => c
            .prepare_cached(ALL_SPEAKER_LINKS_SQL)?
            .query_map([], link)?
            .collect::<Result<_, _>>()?,
    })
}

fn meetings_lenient(c: &Connection) -> Result<Vec<MeetingLink>, StoreError> {
    let mut st = c.prepare_cached(MEETINGS_LENIENT_SQL)?;
    let rows = st.query_map([], |r| {
        Ok(MeetingLink {
            rowid: col_rowid(r.get_ref(0)?),
            id: col_id(r.get_ref(1)?),
            deleted: col_deleted(r.get_ref(2)?),
        })
    })?;
    Ok(rows.collect::<Result<_, _>>()?)
}

fn meeting_deleted(c: &Connection, meeting: EntityId) -> Result<Option<Deleted>, StoreError> {
    use rusqlite::OptionalExtension;
    Ok(c.query_row(
        MEETING_DELETED_SQL,
        params![&meeting.as_uuid().as_bytes()[..]],
        |r| Ok(col_deleted(r.get_ref(0)?)),
    )
    .optional()?)
}

fn current_summary_id_lenient(
    c: &Connection,
    meeting: EntityId,
) -> Result<Option<Col<EntityId>>, StoreError> {
    use rusqlite::OptionalExtension;
    Ok(c.query_row(
        CURRENT_SUMMARY_ID_SQL,
        params![&meeting.as_uuid().as_bytes()[..]],
        |r| Ok(col_id(r.get_ref(0)?)),
    )
    .optional()?)
}

/// The meetings with more than one live summary (today's `GROUP BY ... HAVING count(*) > 1`,
/// in `meeting_id` order), and the rowids of live summaries whose `meeting_id` does not read.
fn crowded_meetings(c: &Connection) -> Result<(Vec<EntityId>, Vec<i64>), StoreError> {
    let mut st = c.prepare_cached(CROWDED_MEETINGS_SQL)?;
    let rows: Vec<(i64, Col<EntityId>)> = st
        .query_map([], |r| {
            Ok((col_rowid(r.get_ref(0)?), col_id(r.get_ref(1)?)))
        })?
        .collect::<Result<_, _>>()?;
    let mut counts: std::collections::BTreeMap<EntityId, usize> = std::collections::BTreeMap::new();
    let mut bad = Vec::new();
    for (rowid, m) in rows {
        match m {
            Col::Read(m) => *counts.entry(m).or_default() += 1,
            Col::Unreadable => bad.push(rowid),
        }
    }
    Ok((
        counts
            .into_iter()
            .filter(|(_, n)| *n > 1)
            .map(|(m, _)| m)
            .collect(),
        bad,
    ))
}

fn person_meetings_lenient(
    c: &Connection,
) -> Result<(Vec<(EntityId, EntityId)>, usize), StoreError> {
    let mut st = c.prepare_cached(PERSON_MEETINGS_SQL)?;
    let rows: Vec<(Col<EntityId>, Col<EntityId>)> = st
        .query_map([], |r| Ok((col_id(r.get_ref(0)?), col_id(r.get_ref(1)?))))?
        .collect::<Result<_, _>>()?;
    let mut out = Vec::new();
    let mut skipped = 0;
    for (p, m) in rows {
        match (p, m) {
            (Col::Read(p), Col::Read(m)) => out.push((p, m)),
            _ => skipped += 1,
        }
    }
    Ok((out, skipped))
}

fn voice_prints(c: &Connection) -> Result<Vec<PrintLink>, StoreError> {
    let mut st = c.prepare_cached(VOICE_PRINTS_SQL)?;
    let rows = st.query_map([], |r| {
        let blob = matches!(r.get_ref(4)?, ValueRef::Text(t) if t == b"blob");
        let len = match r.get_ref(5)? {
            ValueRef::Integer(n) => Some(n),
            _ => None,
        };
        Ok(PrintLink {
            rowid: col_rowid(r.get_ref(0)?),
            id: col_id(r.get_ref(1)?),
            person_id: col_id(r.get_ref(2)?),
            deleted: col_deleted(r.get_ref(3)?),
            sum_reads: blob && len.is_some_and(|n| n % 4 == 0),
        })
    })?;
    Ok(rows.collect::<Result<_, _>>()?)
}

fn voice_samples(c: &Connection) -> Result<Vec<SampleLink>, StoreError> {
    let mut st = c.prepare_cached(VOICE_SAMPLES_SQL)?;
    let rows = st.query_map([], |r| {
        Ok(SampleLink {
            rowid: col_rowid(r.get_ref(0)?),
            id: col_id(r.get_ref(1)?),
            person_id: col_opt_id(r.get_ref(2)?),
            voiceprint_id: col_opt_id(r.get_ref(3)?),
            meeting_id: col_opt_id(r.get_ref(4)?),
            speaker_id: col_opt_id(r.get_ref(5)?),
        })
    })?;
    Ok(rows.collect::<Result<_, _>>()?)
}

/// Rule 3's rows: (rowid, id, meeting id).
type DeadSample = (i64, Col<EntityId>, Col<Option<EntityId>>);

fn dead_print_samples(c: &Connection) -> Result<Vec<DeadSample>, StoreError> {
    let mut st = c.prepare_cached(DEAD_PRINT_SAMPLES_SQL)?;
    let rows = st.query_map([], |r| {
        Ok((
            col_rowid(r.get_ref(0)?),
            col_id(r.get_ref(1)?),
            col_opt_id(r.get_ref(2)?),
        ))
    })?;
    Ok(rows.collect::<Result<_, _>>()?)
}

/// Rule 3a's samples: (rowid, id, person id, meeting id).
type PersonSample = (
    i64,
    Col<EntityId>,
    Col<Option<EntityId>>,
    Col<Option<EntityId>>,
);

fn dead_person_samples(c: &Connection) -> Result<Vec<PersonSample>, StoreError> {
    let mut st = c.prepare_cached(DEAD_PERSON_SAMPLES_SQL)?;
    let rows = st.query_map([], |r| {
        Ok((
            col_rowid(r.get_ref(0)?),
            col_id(r.get_ref(1)?),
            col_opt_id(r.get_ref(2)?),
            col_opt_id(r.get_ref(3)?),
        ))
    })?;
    Ok(rows.collect::<Result<_, _>>()?)
}

fn dead_person_prints(c: &Connection) -> Result<Vec<(i64, Col<EntityId>)>, StoreError> {
    let mut st = c.prepare_cached(DEAD_PERSON_PRINTS_SQL)?;
    let rows = st.query_map([], |r| {
        Ok((col_rowid(r.get_ref(0)?), col_id(r.get_ref(1)?)))
    })?;
    Ok(rows.collect::<Result<_, _>>()?)
}

fn identity_record_keys(c: &Connection) -> Result<Vec<String>, StoreError> {
    let prefix = crate::identities::RECORD_PREFIX;
    let mut st = c.prepare_cached(IDENTITY_RECORD_KEYS_SQL)?;
    let rows = st.query_map(
        params![
            i64::try_from(prefix.chars().count()).unwrap_or(i64::MAX),
            prefix
        ],
        |r| Ok(col_text(r.get_ref(0)?)),
    )?;
    let keys: Vec<Col<String>> = rows.collect::<Result<_, _>>()?;
    Ok(keys.into_iter().filter_map(Col::read).collect())
}

fn erase_sample_by_id(c: &Connection, at: HlcTimestamp, id: EntityId) -> Result<usize, StoreError> {
    let (wall, logical, node) = stamp_params(at)?;
    Ok(c.prepare_cached(ERASE_SAMPLE_BY_ID_SQL)?.execute(params![
        wall,
        logical,
        node,
        &id.as_uuid().as_bytes()[..]
    ])?)
}

fn erase_sample_by_rowid(
    c: &Connection,
    at: HlcTimestamp,
    rowid: i64,
) -> Result<usize, StoreError> {
    let (wall, logical, node) = stamp_params(at)?;
    Ok(c.prepare_cached(ERASE_SAMPLE_BY_ROWID_SQL)?
        .execute(params![wall, logical, node, rowid])?)
}

fn erase_print_by_rowid(c: &Connection, at: HlcTimestamp, rowid: i64) -> Result<usize, StoreError> {
    let (wall, logical, node) = stamp_params(at)?;
    Ok(c.prepare_cached(ERASE_PRINT_BY_ROWID_SQL)?
        .execute(params![wall, logical, node, rowid])?)
}

fn unlink_speaker_by_rowid(
    c: &Connection,
    at: HlcTimestamp,
    rowid: i64,
    name: Option<&str>,
) -> Result<usize, StoreError> {
    let (wall, logical, node) = stamp_params(at)?;
    Ok(c.prepare_cached(UNLINK_SPEAKER_BY_ROWID_SQL)?
        .execute(params![wall, logical, node, rowid, name])?)
}

fn tombstone_person_by_rowid(
    c: &Connection,
    at: HlcTimestamp,
    rowid: i64,
) -> Result<usize, StoreError> {
    let (wall, logical, node) = stamp_params(at)?;
    Ok(c.prepare_cached(TOMBSTONE_PERSON_BY_ROWID_SQL)?
        .execute(params![wall, logical, node, rowid])?)
}

fn i64_of(v: u64) -> Result<i64, StoreError> {
    i64::try_from(v).map_err(|_| StoreError::Corrupt("a stamp past i64".into()))
}

fn hold(c: &Connection, inc: &Incoming, reason: HoldReason) -> Result<(), StoreError> {
    c.prepare_cached(HOLD_SQL)?.execute(params![
        &inc.key[..],
        i64_of(inc.seq)?,
        i64_of(inc.stamp.wall_ms)?,
        inc.stamp.logical,
        &inc.stamp.node.as_uuid().as_bytes()[..],
        &inc.body,
        reason.word()
    ])?;
    Ok(())
}

fn unhold(c: &Connection, inc: &Incoming) -> Result<(), StoreError> {
    c.prepare_cached(UNHOLD_SQL)?.execute(params![
        &inc.key[..],
        i64_of(inc.stamp.wall_ms)?,
        inc.stamp.logical,
        &inc.stamp.node.as_uuid().as_bytes()[..]
    ])?;
    Ok(())
}

fn outbox_forget(c: &Connection, table: &str, id: EntityId) -> Result<usize, StoreError> {
    Ok(c.prepare_cached(OUTBOX_FORGET_SQL)?
        .execute(params![table, &id.as_uuid().as_bytes()[..]])?)
}

/// What [`GuardedTx::write_or_hold`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Written {
    /// The row is written and its outbox entry removed.
    Written,
    /// A row it belongs to is missing: nothing is written.
    MissingParent,
}

/// The readers shared by [`GuardedTx`] and [`GuardedRead`]: each counts what it skips.
macro_rules! guarded_readers {
    () => {
        /// How many distinct damaged rows the readers skipped so far.
        #[must_use]
        pub fn damaged_skipped(&self) -> usize {
            self.skipped.borrow().len()
        }

        /// Counts the damaged rows `rowids` of `table` (a caller that found them through a
        /// reader of its own kind).
        pub fn count_damaged(&self, table: &'static str, rowids: &[i64]) {
            skip_all(&self.skipped, table, rowids);
        }

        /// Every person row's links (16.3 `person_links`), by id.
        ///
        /// # Errors
        /// Store errors.
        pub fn person_links(&self) -> Result<Vec<PersonLink>, StoreError> {
            let links = person_links(self.c())?;
            for l in &links {
                if !l.id.reads()
                    || l.deleted == Deleted::Unreadable
                    || !l.is_me.reads()
                    || !l.merged_into.reads()
                {
                    skip(&self.skipped, "person", l.rowid);
                }
            }
            Ok(links)
        }

        /// The speaker rows' links of `meeting` (every row's when `None`).
        ///
        /// # Errors
        /// Store errors.
        pub fn speaker_links(&self, meeting: Option<EntityId>) -> Result<Vec<SpeakerLink>, StoreError> {
            speaker_links(self.c(), meeting)
        }

        /// Every meeting row's id and `deleted`, in `all_meetings`' order; a row whose either
        /// does not read is counted.
        ///
        /// # Errors
        /// Store errors.
        pub fn meetings_lenient(&self) -> Result<Vec<MeetingLink>, StoreError> {
            let rows = meetings_lenient(self.c())?;
            for m in &rows {
                if !m.id.reads() || m.deleted == Deleted::Unreadable {
                    skip(&self.skipped, "meeting", m.rowid);
                }
            }
            Ok(rows)
        }

        /// `meeting`'s `deleted`, three ways: `None` when there is no such row.
        ///
        /// # Errors
        /// Store errors.
        pub fn meeting_deleted(&self, meeting: EntityId) -> Result<Option<Deleted>, StoreError> {
            meeting_deleted(self.c(), meeting)
        }

        /// The id of `meeting`'s current summary, read as a [`Col`].
        ///
        /// # Errors
        /// Store errors.
        pub fn current_summary_id_lenient(
            &self,
            meeting: EntityId,
        ) -> Result<Option<Col<EntityId>>, StoreError> {
            current_summary_id_lenient(self.c(), meeting)
        }

        /// `meeting`'s current summary (16.3): `Some(Err(()))` when that row does not read,
        /// never an older summary.
        ///
        /// # Errors
        /// Store errors.
        pub fn current_summary_or_damaged(
            &self,
            meeting: EntityId,
        ) -> Result<Option<Result<Summary, ()>>, StoreError> {
            Ok(match crate::repo::lenient::current_summary(self.c(), meeting).map_err(repo)? {
                None => None,
                Some(Ok(s)) => Some(Ok(s)),
                Some(Err(rowid)) => {
                    skip(&self.skipped, "summary", rowid);
                    Some(Err(()))
                }
            })
        }

        /// `meeting`'s speaker rows that read; the others counted.
        ///
        /// # Errors
        /// Store errors.
        pub fn speakers_or_damaged(&self, meeting: EntityId) -> Result<(Vec<Speaker>, Vec<i64>), StoreError> {
            let (ok, bad) = crate::repo::for_deletion::speakers(self.c(), meeting).map_err(repo)?;
            skip_all(&self.skipped, "speaker", &bad);
            Ok((ok, bad))
        }

        /// `meeting`'s speaker rows in creation order, each with its rowid (`None` when it
        /// does not read; counted).
        ///
        /// # Errors
        /// Store errors.
        pub fn speakers_in_order(&self, meeting: EntityId) -> Result<Vec<(i64, Option<Speaker>)>, StoreError> {
            let rows = crate::repo::lenient::speakers_in_order(self.c(), meeting).map_err(repo)?;
            for (rowid, row) in &rows {
                if row.is_none() {
                    skip(&self.skipped, "speaker", *rowid);
                }
            }
            Ok(rows)
        }

        /// `meeting`'s summaries that read; the others counted.
        ///
        /// # Errors
        /// Store errors.
        pub fn summaries_or_damaged(&self, meeting: EntityId) -> Result<Vec<Summary>, StoreError> {
            let (ok, bad) = crate::repo::for_deletion::summaries(self.c(), meeting).map_err(repo)?;
            skip_all(&self.skipped, "summary", &bad);
            Ok(ok)
        }

        /// `meeting`'s segment rows that read; the others counted.
        ///
        /// # Errors
        /// Store errors.
        pub fn segments_or_damaged(&self, meeting: EntityId) -> Result<Vec<earshot_core::model::SegmentRow>, StoreError> {
            let (ok, bad) = crate::repo::segments_or_damaged(self.c(), meeting).map_err(repo)?;
            skip_all(&self.skipped, "segment", &bad);
            Ok(ok)
        }

        /// Every person row that reads, by id; the others counted.
        ///
        /// # Errors
        /// Store errors.
        pub fn persons_or_damaged(&self) -> Result<Vec<Person>, StoreError> {
            let (ok, bad) = crate::repo::lenient::persons(self.c()).map_err(repo)?;
            skip_all(&self.skipped, "person", &bad);
            Ok(ok)
        }

        /// The person row `id`: `None` when absent, `Some(Err(()))` when it does not read
        /// (counted).
        ///
        /// # Errors
        /// Store errors.
        pub fn person_or_damaged(&self, id: EntityId) -> Result<Option<Result<Person, ()>>, StoreError> {
            Ok(match crate::repo::lenient::person(self.c(), id).map_err(repo)? {
                None => None,
                Some(Ok(p)) => Some(Ok(p)),
                Some(Err(rowid)) => {
                    skip(&self.skipped, "person", rowid);
                    Some(Err(()))
                }
            })
        }

        /// The meeting row `id`: `None` when absent, `Some(Err(()))` when it does not read
        /// (counted).
        ///
        /// # Errors
        /// Store errors.
        pub fn meeting_or_damaged(
            &self,
            id: EntityId,
        ) -> Result<Option<Result<earshot_core::model::Meeting, ()>>, StoreError> {
            Ok(match crate::repo::for_deletion::meeting(self.c(), id).map_err(repo)? {
                None => None,
                Some(Ok(m)) => Some(Ok(m)),
                Some(Err(rowid)) => {
                    skip(&self.skipped, "meeting", rowid);
                    Some(Err(()))
                }
            })
        }

        /// Every Dictionary word row that reads, in creation order, and how many did not
        /// (counted).
        ///
        /// # Errors
        /// Store errors.
        pub fn dictionary_words_or_damaged(&self) -> Result<(Vec<DictionaryWord>, usize), StoreError> {
            let (ok, bad) = crate::repo::lenient::dictionary_words(self.c()).map_err(repo)?;
            skip_all(&self.skipped, "dictionary_word", &bad);
            Ok((ok, bad.len()))
        }

        /// Rule 2's meetings with more than one live summary; a `meeting_id` that does not
        /// read is counted and left out.
        ///
        /// # Errors
        /// Store errors.
        pub fn crowded_meetings(&self) -> Result<Vec<EntityId>, StoreError> {
            let (out, bad) = crowded_meetings(self.c())?;
            skip_all(&self.skipped, "summary", &bad);
            Ok(out)
        }

        /// The People page's (person, meeting) pairs; a pair whose either id does not read is
        /// counted and left out.
        ///
        /// # Errors
        /// Store errors.
        pub fn person_meetings_lenient(&self) -> Result<(Vec<(EntityId, EntityId)>, usize), StoreError> {
            person_meetings_lenient(self.c())
        }

        /// The prints of `members` and their live samples (16.3 `voice_rows_of`): prints whose
        /// `person_id` reads as a member, samples whose `person_id` reads as a member or whose
        /// `voiceprint_id` reads as one of those prints. A row whose deciding columns do not
        /// read is counted.
        ///
        /// # Errors
        /// Store errors.
        pub fn voice_rows_of(&self, members: &[EntityId]) -> Result<VoiceRows, StoreError> {
            let prints = voice_prints(self.c())?;
            let mut out = VoiceRows::default();
            for &m in members {
                for p in prints.iter().filter(|p| p.person_id == Col::Read(m)) {
                    out.prints.push(*p);
                }
            }
            for p in prints.iter().filter(|p| !p.person_id.reads()) {
                skip(&self.skipped, "voiceprint", p.rowid);
            }
            let ids: Vec<EntityId> = out.prints.iter().filter_map(|p| p.id.read()).collect();
            let mut readable = Vec::new();
            let mut by_rowid = Vec::new();
            for s in voice_samples(self.c())? {
                let theirs = matches!(s.person_id, Col::Read(Some(p)) if members.contains(&p))
                    || matches!(s.voiceprint_id, Col::Read(Some(v)) if ids.contains(&v));
                if !s.person_id.reads() && !s.voiceprint_id.reads() {
                    skip(&self.skipped, "voiceprint_sample", s.rowid);
                }
                if theirs {
                    if s.id.reads() {
                        readable.push(s);
                    } else {
                        by_rowid.push(s);
                    }
                }
            }
            readable.sort_by_key(|s| s.id.read());
            out.samples = readable;
            out.samples.extend(by_rowid);
            Ok(out)
        }

        /// Every live sample's links (16.5 item 23a reads the "Your voice" ones).
        ///
        /// # Errors
        /// Store errors.
        pub fn live_sample_links(&self) -> Result<Vec<SampleLink>, StoreError> {
            voice_samples(self.c())
        }

        /// Whether the identity `members` holds a voice: a live print of a member whose
        /// effective count, folded as today (core `effective`) over its live samples that
        /// read, is above 0 (16.3 `has_voice`). A print or sample that does not read decides
        /// nothing and is counted.
        ///
        /// # Errors
        /// Store errors.
        pub fn has_voice(&self, members: &[EntityId]) -> Result<bool, StoreError> {
            for &m in members {
                let (prints, bad): (Vec<Voiceprint>, Vec<i64>) =
                    crate::repo::lenient::voiceprints_of(self.c(), m).map_err(repo)?;
                skip_all(&self.skipped, "voiceprint", &bad);
                for v in prints.iter().filter(|v| !v.meta().deleted) {
                    let (samples, bad): (Vec<VoiceprintSample>, Vec<i64>) =
                        crate::repo::lenient::samples_of(self.c(), v.id()).map_err(repo)?;
                    skip_all(&self.skipped, "voiceprint_sample", &bad);
                    if earshot_core::voiceprint::effective(v, &samples).count() > 0 {
                        return Ok(true);
                    }
                }
            }
            Ok(false)
        }

        /// A device-local value.
        ///
        /// # Errors
        /// SQLite errors.
        pub fn local_value(&self, key: &str) -> Result<Option<Vec<u8>>, StoreError> {
            self.inner_local_value(key)
        }
    };
}

impl GuardedRead<'_> {
    fn c(&self) -> &Connection {
        &self.store.conn
    }

    fn inner_local_value(&self, key: &str) -> Result<Option<Vec<u8>>, StoreError> {
        self.store.local_value(key)
    }

    guarded_readers!();

    /// This store's device id as stored, `None` before one exists.
    ///
    /// # Errors
    /// Store errors.
    pub fn outbox_len_except(
        &self,
        meetings: &[EntityId],
        kinds: crate::SyncKinds,
    ) -> Result<u64, StoreError> {
        self.store.outbox_len_except(meetings, kinds)
    }

    /// [`Store::outbox_page`] (T14.26's lenient outbox read).
    ///
    /// # Errors
    /// Store errors.
    pub fn outbox_page(
        &self,
        limit: usize,
        meetings: &[EntityId],
        kinds: crate::SyncKinds,
    ) -> Result<crate::OutboxPage, StoreError> {
        self.store.outbox_page(limit, meetings, kinds)
    }

    /// [`Store::held_page`].
    ///
    /// # Errors
    /// Store errors.
    pub fn held_page(
        &self,
        reasons: &[HoldReason],
        after: Option<&[u8; 16]>,
        limit: usize,
    ) -> Result<Vec<crate::Held>, StoreError> {
        self.store.held_page(reasons, after, limit)
    }

    /// [`Store::held_count`].
    ///
    /// # Errors
    /// Store errors.
    pub fn held_count(&self, reasons: &[HoldReason]) -> Result<u64, StoreError> {
        self.store.held_count(reasons)
    }
}

impl GuardedTx<'_, '_> {
    fn c(&self) -> &Connection {
        &self.tx.conn
    }

    fn inner_local_value(&self, key: &str) -> Result<Option<Vec<u8>>, StoreError> {
        self.tx.local_value(key)
    }

    guarded_readers!();

    /// Sets a device-local value.
    ///
    /// # Errors
    /// As `StoreTx::set_local_value`.
    pub fn set_local_value(&mut self, key: &str, value: &[u8]) -> Result<(), StoreError> {
        self.tx.set_local_value(key, value)
    }

    /// Deletes a device-local value; returns whether it existed.
    ///
    /// # Errors
    /// As `StoreTx::delete_local_value`.
    pub fn delete_local_value(&mut self, key: &str) -> Result<bool, StoreError> {
        self.tx.delete_local_value(key)
    }

    /// The keys of every meeting's speaker identity record.
    ///
    /// # Errors
    /// Store errors.
    pub fn identity_record_keys(&self) -> Result<Vec<String>, StoreError> {
        identity_record_keys(self.c())
    }

    /// Writes a whole readable person row.
    ///
    /// # Errors
    /// Store errors.
    pub fn put_person(&mut self, p: Person) -> Result<(), StoreError> {
        VoiceprintRepo::put_person(self.tx, p)
            .map_err(|e| StoreError::Repo(RepoError::Storage(e.to_string())))
    }

    /// Writes a whole readable speaker row.
    ///
    /// # Errors
    /// Store errors.
    pub fn put_speaker(&mut self, s: Speaker) -> Result<(), StoreError> {
        MeetingRepo::put_speaker(self.tx, s).map_err(repo)
    }

    /// Writes a whole readable summary row.
    ///
    /// # Errors
    /// Store errors.
    pub fn put_summary(&mut self, s: Summary) -> Result<(), StoreError> {
        MeetingRepo::put_summary(self.tx, s).map_err(repo)
    }

    /// Writes a whole readable Dictionary word row.
    ///
    /// # Errors
    /// Store errors.
    pub fn put_dictionary_word(&mut self, w: DictionaryWord) -> Result<(), StoreError> {
        earshot_core::model::DictionaryRepo::put_dictionary_word(self.tx, w).map_err(repo)
    }

    /// Writes a whole synced row (the page's own writes and the repairs' rewrites).
    ///
    /// # Errors
    /// Store errors.
    pub(crate) fn put_synced(&mut self, row: &SyncedRow) -> Result<(), StoreError> {
        crate::repo::put_synced(self.c(), row).map_err(repo)
    }

    /// Writes `row` and removes its outbox entry, or reports a missing parent with nothing
    /// written (a guarded savepoint).
    ///
    /// # Errors
    /// Store errors other than a missing parent.
    pub(crate) fn write_or_hold(&mut self, row: &SyncedRow) -> Result<Written, StoreError> {
        let sp = self.tx.conn.savepoint()?;
        match crate::repo::put_synced(&sp, row) {
            Ok(()) => {
                outbox_forget(&sp, row.table(), row.id())?;
                sp.commit()?;
                Ok(Written::Written)
            }
            Err(RepoError::MissingParent { .. }) => Ok(Written::MissingParent),
            Err(e) => Err(repo(e)),
        }
    }

    /// The local copy of a pulled row: `Ok(None)` absent, `Err(())` when it does not read.
    ///
    /// # Errors
    /// Store errors.
    pub(crate) fn synced_by_id_or_damaged(
        &self,
        table: &str,
        id: EntityId,
    ) -> Result<Result<Option<SyncedRow>, ()>, StoreError> {
        crate::repo::synced_by_id_or_damaged(self.c(), table, id).map_err(repo)
    }

    /// Whether row `id` of `table` reads as a tombstone from its `deleted` column alone.
    ///
    /// # Errors
    /// Store errors.
    pub(crate) fn row_reads_deleted(&self, table: &str, id: EntityId) -> Result<bool, StoreError> {
        crate::repo::row_reads_deleted(self.c(), table, id).map_err(repo)
    }

    /// Whether the summary request `id` reads as withdrawn by `asker`.
    ///
    /// # Errors
    /// Store errors.
    pub(crate) fn request_reads_withdrawn_by(
        &self,
        id: EntityId,
        asker: earshot_core::identity::DeviceId,
    ) -> Result<bool, StoreError> {
        crate::repo::request_reads_withdrawn_by(self.c(), id, asker).map_err(repo)
    }

    /// Row `id`'s `updated_at` from its three columns: `None` absent, `Some(None)` unreadable.
    ///
    /// # Errors
    /// Store errors.
    pub(crate) fn updated_at_lenient(
        &self,
        table: &str,
        id: EntityId,
    ) -> Result<Option<Option<HlcTimestamp>>, StoreError> {
        crate::repo::lenient::updated_at(self.c(), table, id).map_err(repo)
    }

    /// Holds a pulled row.
    ///
    /// # Errors
    /// Store errors.
    pub(crate) fn hold(&mut self, inc: &Incoming, reason: HoldReason) -> Result<(), StoreError> {
        hold(self.c(), inc, reason)
    }

    /// Removes a held copy of `inc`'s row key not later than `inc`.
    ///
    /// # Errors
    /// Store errors.
    pub(crate) fn unhold(&mut self, inc: &Incoming) -> Result<(), StoreError> {
        unhold(self.c(), inc)
    }

    /// Removes a row's outbox entry.
    ///
    /// # Errors
    /// Store errors.
    pub(crate) fn outbox_forget(&mut self, table: &str, id: EntityId) -> Result<usize, StoreError> {
        outbox_forget(self.c(), table, id)
    }

    /// The row keys H1 and H2 hold (docs/sync-spec.md 15.4).
    ///
    /// # Errors
    /// Store errors.
    pub(crate) fn held_replaced_keys(&self) -> Result<BTreeSet<[u8; 16]>, StoreError> {
        crate::sync::held_replaced_keys(self.c())
    }

    /// Stores the row keys H1 and H2 hold.
    ///
    /// # Errors
    /// Store errors.
    pub(crate) fn put_held_replaced_keys(
        &mut self,
        keys: &BTreeSet<[u8; 16]>,
    ) -> Result<(), StoreError> {
        crate::sync::put_held_replaced_keys(self.c(), keys)
    }

    /// The pulled segment row's plan (docs/sync-spec.md 15.4).
    ///
    /// # Errors
    /// Store errors.
    pub(crate) fn segment_plan(
        &self,
        there: &earshot_core::model::SegmentRow,
        local: Option<&SyncedRow>,
        release: Option<&dyn Fn(EntityId, EntityId) -> bool>,
    ) -> Result<crate::sync::SegmentPlan, StoreError> {
        crate::sync::segment_plan(self.c(), there, local, release)
    }

    /// The segment row `id`'s rowid.
    ///
    /// # Errors
    /// Store errors.
    pub(crate) fn segment_rowid(&self, id: EntityId) -> Result<Option<i64>, StoreError> {
        crate::replaced::rowid_of(self.c(), id)
    }

    /// Clears a segment row to its cleared form (docs/sync-spec.md 15.2).
    ///
    /// # Errors
    /// Store errors.
    pub(crate) fn clear_segment(&mut self, rowid: i64, at: HlcTimestamp) -> Result<(), StoreError> {
        crate::replaced::clear_rowid(self.c(), rowid, at).map(|_| ())
    }

    /// Erases a segment row of a meeting deleted here (docs/sync-spec.md 15.4).
    ///
    /// # Errors
    /// Store errors.
    pub(crate) fn erase_segment(&mut self, rowid: i64, at: HlcTimestamp) -> Result<(), StoreError> {
        crate::replaced::erase_rowid(self.c(), rowid, at).map(|_| ())
    }

    /// Notes a live final line this transaction stamped (docs/sync-spec.md 15.2).
    pub(crate) fn note_line(&self, row: &earshot_core::model::SegmentRow) {
        self.tx.note_line(row);
    }

    /// Lines this transaction stamped go out before what they replace (docs/sync-spec.md
    /// 15.2).
    ///
    /// # Errors
    /// Store errors.
    pub fn send_lines_first<C: Clock + ?Sized>(
        &mut self,
        hlc: &mut Hlc,
        clock: &C,
    ) -> Result<(), StoreError> {
        self.tx.send_lines_first(hlc, clock).map(|_| ())
    }

    /// Erases the live sample `id` (16.4); returns whether one was erased.
    ///
    /// # Errors
    /// Store errors.
    pub fn erase_sample_by_id<C: Clock + ?Sized>(
        &mut self,
        hlc: &mut Hlc,
        clock: &C,
        id: EntityId,
    ) -> Result<bool, StoreError> {
        let at = hlc.now(clock);
        Ok(erase_sample_by_id(self.c(), at, id)? > 0)
    }

    /// Erases the live sample at `rowid` whose `id` does not read (16.4); returns whether one
    /// was erased.
    ///
    /// # Errors
    /// Store errors.
    pub fn erase_sample_by_rowid<C: Clock + ?Sized>(
        &mut self,
        hlc: &mut Hlc,
        clock: &C,
        rowid: i64,
    ) -> Result<bool, StoreError> {
        let at = hlc.now(clock);
        Ok(erase_sample_by_rowid(self.c(), at, rowid)? > 0)
    }

    /// Tombstones the print at `rowid`, its vector zeroed whatever it reads as (16.4).
    ///
    /// # Errors
    /// Store errors.
    pub fn erase_print_by_rowid<C: Clock + ?Sized>(
        &mut self,
        hlc: &mut Hlc,
        clock: &C,
        rowid: i64,
    ) -> Result<(), StoreError> {
        let at = hlc.now(clock);
        erase_print_by_rowid(self.c(), at, rowid).map(|_| ())
    }

    /// Unlinks the speaker row at `rowid` (16.4): `person_id` NULL, `ignored` 0, the name only
    /// when `name` is given.
    ///
    /// # Errors
    /// Store errors.
    pub fn unlink_speaker_by_rowid<C: Clock + ?Sized>(
        &mut self,
        hlc: &mut Hlc,
        clock: &C,
        rowid: i64,
        name: Option<&str>,
    ) -> Result<(), StoreError> {
        let at = hlc.now(clock);
        unlink_speaker_by_rowid(self.c(), at, rowid, name).map(|_| ())
    }

    /// Tombstones the person row at `rowid` with its name cleared (16.4).
    ///
    /// # Errors
    /// Store errors.
    pub fn tombstone_person_by_rowid<C: Clock + ?Sized>(
        &mut self,
        hlc: &mut Hlc,
        clock: &C,
        rowid: i64,
    ) -> Result<(), StoreError> {
        let at = hlc.now(clock);
        tombstone_person_by_rowid(self.c(), at, rowid).map(|_| ())
    }

    /// Rule 3's rows: live samples of a print tombstoned here.
    ///
    /// # Errors
    /// Store errors.
    pub(crate) fn dead_print_samples(&self) -> Result<Vec<DeadSample>, StoreError> {
        dead_print_samples(self.c())
    }

    /// Rule 3a's samples: live samples naming a person, (rowid, id, person id, meeting id).
    ///
    /// # Errors
    /// Store errors.
    pub(crate) fn dead_person_samples(&self) -> Result<Vec<PersonSample>, StoreError> {
        dead_person_samples(self.c())
    }

    /// Rule 3a's prints: live prints, (rowid, person id).
    ///
    /// # Errors
    /// Store errors.
    pub(crate) fn dead_person_prints(&self) -> Result<Vec<(i64, Col<EntityId>)>, StoreError> {
        dead_person_prints(self.c())
    }
}

/// The body of [`Store::apply_remote`]'s transaction (docs/sync-spec.md 5.3 and 16.3): the
/// page's rows in order, the convergence repairs and the caller's `after`. A local row that
/// does not read is judged by the columns that still read and replaced by the pulled copy as
/// an absent row is, except where a tombstone here stays final (T14.26); every reader below
/// skips and counts a row that does not read ([`Applied::damaged_skipped`]).
///
/// # Errors
/// Store errors other than a row that does not read; nothing of the page is written then.
#[allow(clippy::too_many_arguments)]
pub(crate) fn apply_page<C: Clock + ?Sized>(
    g: &mut GuardedTx<'_, '_>,
    page: &[Incoming],
    rules: &crate::ApplyRules<'_>,
    hlc: &mut Hlc,
    clock: &C,
    mut wrote: crate::sync::Wrote,
    release: Option<&dyn Fn(EntityId, EntityId) -> bool>,
) -> Result<crate::Applied, StoreError> {
    use crate::sync::{AfterWrite, SegmentPlan, Withdrawal, withdrawal};
    let mut out = crate::Applied::default();
    // A row H1 or H2 would hold is tried once more after the rest of the page
    // (docs/sync-spec.md 15.4): a page that brings a track's new lines after the rows they
    // replace applies both, as the end-of-tick retry would.
    let mut rows: std::collections::VecDeque<(&Incoming, bool)> =
        page.iter().map(|inc| (inc, false)).collect();
    // The row keys H1 and H2 hold (`HELD_REPLACED_KEY`).
    let mut h_held = g.held_replaced_keys()?;
    let h_held_before = !h_held.is_empty();
    while let Some((inc, deferred)) = rows.pop_front() {
        let Some(row) = &inc.row else {
            // A kind switched off is skipped before anything is held, whether or not its
            // record decodes: a device that never syncs it keeps nothing of it.
            if inc.entity.as_deref().is_some_and(|e| !rules.kinds.syncs(e)) {
                g.unhold(inc)?;
                out.skipped += 1;
                continue;
            }
            g.hold(inc, HoldReason::Format)?;
            out.hold(HoldReason::Format);
            continue;
        };
        if !rules.kinds.syncs(row.table()) {
            g.unhold(inc)?;
            out.skipped += 1;
            continue;
        }
        if inc.stamp.wall_ms > rules.now_ms.saturating_add(crate::MAX_CLOCK_AHEAD_MS) {
            g.hold(inc, HoldReason::Clock)?;
            out.hold(HoldReason::Clock);
            continue;
        }
        if row.waits_while_busy() && row.meeting().is_some_and(|m| (rules.leased)(m)) {
            g.hold(inc, HoldReason::Busy)?;
            out.hold(HoldReason::Busy);
            continue;
        }
        // A live row of a meeting deleted here (edited elsewhere before that device heard of
        // the deletion) is not written: no transcript text of a deleted meeting is kept. The
        // other device deletes its copy when the tombstone reaches it.
        if !matches!(row, SyncedRow::Meeting(_))
            && !row.meta().deleted
            && let Some(m) = row.meeting()
            // A local meeting row that does not read is judged by its `deleted` column
            // (T14.26).
            && match g.synced_by_id_or_damaged("meeting", m)? {
                Ok(l) => l.is_some_and(|l| l.meta().deleted),
                Err(()) => g.row_reads_deleted("meeting", m)?,
            }
        {
            g.unhold(inc)?;
            out.deletions_kept += 1;
            continue;
        }
        // A local row that does not read (T14.26) is replaced by the pulled one, as an absent
        // row is, except where the readable path below keeps the local state final against the
        // pulled copy: a meeting or voiceprint tombstone, an erased voice sample, and this
        // device's withdrawal of its own summary request against an open one. Those are judged
        // from the columns that still read and the pulled row is ignored (a damaged row cannot
        // be stamped again or sent).
        let local = match g.synced_by_id_or_damaged(row.table(), row.id())? {
            Ok(local) => local,
            Err(()) => {
                let stays_final = match row {
                    SyncedRow::Meeting(_) | SyncedRow::Voiceprint(_) => {
                        !row.meta().deleted && g.row_reads_deleted(row.table(), row.id())?
                    }
                    SyncedRow::VoiceprintSample(there) => {
                        !there.is_erased() && g.row_reads_deleted(row.table(), row.id())?
                    }
                    SyncedRow::SummaryRequest(there) => {
                        there.state.is_open()
                            && g.request_reads_withdrawn_by(row.id(), hlc.node())?
                    }
                    _ => false,
                };
                if stays_final {
                    g.unhold(inc)?;
                    out.ignored += 1;
                    continue;
                }
                None
            }
        };
        // A meeting deletion is final: a live copy edited elsewhere after it (pushed before
        // that device heard of the deletion, so the relay kept the edit) never brings the
        // meeting back. The tombstone here is stamped again, after the edit, and goes out to
        // win at the relay; the other device then deletes its copy.
        if let (Some(SyncedRow::Meeting(here)), SyncedRow::Meeting(there)) = (&local, row)
            && here.meta.deleted
            && !there.meta.deleted
        {
            let _ = hlc.observe(inc.stamp, clock);
            if here.meta.updated_at < row.meta().updated_at {
                let mut again = here.clone();
                again.meta.touch(hlc, clock);
                g.put_synced(&SyncedRow::Meeting(again))?;
                out.deletions_kept += 1;
            } else {
                out.ignored += 1;
            }
            g.unhold(inc)?;
            continue;
        }
        // A voiceprint deletion is final, as a meeting's (docs/voice-samples.md 7.4): a live
        // copy written elsewhere after it (an older build that still had the print) never
        // brings the voice back. The tombstone is stamped again and goes out.
        if let (Some(SyncedRow::Voiceprint(here)), SyncedRow::Voiceprint(there)) = (&local, row)
            && here.meta().deleted
            && !there.meta().deleted
        {
            let _ = hlc.observe(inc.stamp, clock);
            if here.meta().updated_at < row.meta().updated_at {
                let mut meta = *here.meta();
                meta.touch(hlc, clock);
                let again = Voiceprint::from_stored(
                    here.id(),
                    meta,
                    here.person_id(),
                    here.model().clone(),
                    here.base_sum().to_vec(),
                    here.base_count(),
                    here.base_speech_ms(),
                );
                g.put_synced(&SyncedRow::Voiceprint(again))?;
                wrote.prints = true;
                out.deletions_kept += 1;
            } else {
                out.ignored += 1;
            }
            g.unhold(inc)?;
            continue;
        }
        // A voice sample's erasure is final too: a live copy never brings an erased voice back
        // (docs/voice-samples.md 7.4).
        if let (Some(SyncedRow::VoiceprintSample(here)), SyncedRow::VoiceprintSample(there)) =
            (&local, row)
            && here.is_erased()
            && !there.is_erased()
        {
            let _ = hlc.observe(inc.stamp, clock);
            if here.meta().updated_at < row.meta().updated_at {
                let mut meta = *here.meta();
                meta.touch(hlc, clock);
                let again = VoiceprintSample::erased(here.id(), meta)
                    .map_err(|e| StoreError::Repo(RepoError::Storage(e.to_string())))?;
                g.put_synced(&SyncedRow::VoiceprintSample(again))?;
                wrote.samples = true;
                out.deletions_kept += 1;
            } else {
                out.ignored += 1;
            }
            g.unhold(inc)?;
            continue;
        }
        // A summary request's withdrawal is final (docs/sync-spec.md section 10.1).
        if let (Some(SyncedRow::SummaryRequest(here)), SyncedRow::SummaryRequest(there)) =
            (&local, row)
        {
            match withdrawal(here, there, hlc.node()) {
                Withdrawal::Lww => {}
                Withdrawal::TakeIt => {
                    // Written as it came: only the asker writes a withdrawal, so this device
                    // never stamps one of its own here.
                    g.put_synced(row)?;
                    let _ = g.outbox_forget(row.table(), row.id())?;
                    g.unhold(inc)?;
                    let _ = hlc.observe(inc.stamp, clock);
                    out.written += 1;
                    out.meetings.insert(there.meeting_id);
                    continue;
                }
                Withdrawal::KeepMine => {
                    let _ = hlc.observe(inc.stamp, clock);
                    if here.meta.updated_at < there.meta.updated_at {
                        let mut again = here.clone();
                        again.meta.touch(hlc, clock);
                        g.put_synced(&SyncedRow::SummaryRequest(again))?;
                        out.withdrawals_kept += 1;
                    } else {
                        out.ignored += 1;
                    }
                    g.unhold(inc)?;
                    continue;
                }
            }
        }
        // Rule 4a (docs/sync-spec.md section 7, T12.92): two copies of one final line are
        // compared by the user's edit first, so a re-transcription that did not see an edit
        // never replaces it, whatever the stamps. Only while the meeting is live here (a
        // meeting whose `deleted` does not read falls back to plain `updated_at` order,
        // 16.5 item 10). The relay keeps the later `updated_at` alone, so a copy kept here
        // that is stamped below the losing one is stamped again and goes out.
        let mut restamp_written = false;
        if let (Some(SyncedRow::Segment(here)), SyncedRow::Segment(there)) = (&local, row)
            && let Some(order) = earshot_core::model::SegmentRow::edit_order(here, there)
            && crate::sync::meeting_live(g, there.meeting_id)? == Some(true)
        {
            if order != std::cmp::Ordering::Greater {
                let _ = hlc.observe(inc.stamp, clock);
                if here.meta.updated_at < there.meta.updated_at {
                    let mut again = here.clone();
                    again.meta.touch(hlc, clock);
                    g.put_synced(&SyncedRow::Segment(again.clone()))?;
                    // Re-queued with a stamp of its own (docs/sync-spec.md 15.2).
                    g.note_line(&again);
                    out.edited_lines_kept += 1;
                    out.meetings.insert(here.meeting_id);
                } else {
                    out.ignored += 1;
                }
                g.unhold(inc)?;
                continue;
            }
            restamp_written = here.meta.updated_at > there.meta.updated_at;
        }
        if !restamp_written
            && local
                .as_ref()
                .is_some_and(|l| l.meta().updated_at >= row.meta().updated_at)
        {
            g.unhold(inc)?;
            out.ignored += 1;
            continue;
        }
        // Replaced transcript text (docs/sync-spec.md 15.4): the two holds, and what is done
        // once the pulled segment row is written.
        let segment_plan = match row {
            SyncedRow::Segment(there) => {
                let plan = g.segment_plan(there, local.as_ref(), release)?;
                if plan.hold && !deferred {
                    rows.push_back((inc, true));
                    continue;
                }
                if plan.hold {
                    g.hold(inc, HoldReason::Parent)?;
                    h_held.insert(inc.key);
                    out.hold(HoldReason::Parent);
                    continue;
                }
                plan
            }
            _ => SegmentPlan::default(),
        };
        let person_was_live = match &local {
            Some(SyncedRow::Person(p)) => !p.meta.deleted,
            None => matches!(row, SyncedRow::Person(_)),
            Some(_) => false,
        };
        let print_was_live = matches!(&local, Some(SyncedRow::Voiceprint(v)) if !v.meta().deleted);
        let sample_was_live =
            matches!(&local, Some(SyncedRow::VoiceprintSample(s)) if !s.is_erased());
        // A pulled print tombstone is stored with its vector zeroed, whatever it holds.
        let zeroed;
        let row = match row {
            SyncedRow::Voiceprint(v) if v.meta().deleted => {
                zeroed = SyncedRow::Voiceprint(Voiceprint::from_stored(
                    v.id(),
                    *v.meta(),
                    v.person_id(),
                    v.model().clone(),
                    vec![0.0; v.base_sum().len()],
                    v.base_count(),
                    v.base_speech_ms(),
                ));
                &zeroed
            }
            other => other,
        };
        // The trigger named it; it came from elsewhere, so it is not sent.
        if g.write_or_hold(row)? == Written::MissingParent {
            g.hold(inc, HoldReason::Parent)?;
            // Held for a missing parent, not by H1 or H2: the release leaves it.
            h_held.remove(&inc.key);
            out.hold(HoldReason::Parent);
            continue;
        }
        g.unhold(inc)?;
        wrote.row(row);
        let _ = hlc.observe(inc.stamp, clock);
        // Rule 4a: an edit that won over a later-stamped copy here is stamped again (after the
        // incoming stamp was observed), so it keeps its outbox entry and goes out to win at the
        // relay.
        if restamp_written && let SyncedRow::Segment(won) = row {
            let mut again = won.clone();
            again.meta.touch(hlc, clock);
            g.put_synced(&SyncedRow::Segment(again.clone()))?;
            g.note_line(&again);
        }
        // A pulled tombstone that holds text leaves this device holding none (15.4): written,
        // then cleared (or, in a meeting deleted here, erased) through the one statement,
        // stamped after the incoming stamp was observed, so it goes out and replaces the
        // relay's copy.
        match segment_plan.after {
            AfterWrite::Clear | AfterWrite::Erase => {
                if let Some(rowid) = g.segment_rowid(row.id())? {
                    let at = hlc.now(clock);
                    if segment_plan.after == AfterWrite::Clear {
                        g.clear_segment(rowid, at)?;
                    } else {
                        g.erase_segment(rowid, at)?;
                    }
                    out.text_emptied += 1;
                }
            }
            AfterWrite::Nothing if segment_plan.emptied => out.text_emptied += 1,
            AfterWrite::Nothing => {}
        }
        out.written += 1;
        if (local.is_none() && row.has_children()) || segment_plan.first_final {
            out.parents_written += 1;
        }
        if let Some(m) = row.meeting() {
            out.meetings.insert(m);
        }
        // Persons, prints and samples decide names and saved voices in any meeting.
        out.people |= matches!(
            row,
            SyncedRow::Person(_) | SyncedRow::Voiceprint(_) | SyncedRow::VoiceprintSample(_)
        );
        // A sample names its meeting; a tombstone (no body) the meeting of the sample it
        // erased here.
        for s in [Some(row), local.as_ref()].into_iter().flatten() {
            if let SyncedRow::VoiceprintSample(s) = s
                && let Some(earshot_core::voiceprint::SampleOrigin::Meeting { meeting, .. }) =
                    s.origin()
            {
                out.meetings.insert(meeting);
            }
        }
        if matches!(row, SyncedRow::Voiceprint(v) if v.meta().deleted) && print_was_live {
            out.biometrics_forgotten += 1;
        }
        if matches!(row, SyncedRow::VoiceprintSample(s) if s.is_erased()) && sample_was_live {
            out.biometrics_forgotten += 1;
        }
        if let SyncedRow::Person(p) = row
            && p.meta.deleted
            && person_was_live
        {
            // Every erased item, identity entries included, so a removal that dropped only
            // centroids still leads the client to checkpoint (docs/sync-spec.md 16.7).
            out.biometrics_forgotten += crate::identities::forget_identity_biometrics(
                g,
                p.id,
                hlc,
                clock,
                crate::identities::Erasure::Pulled,
            )?
            .erased();
            // It tombstones that identity's prints and erases its samples.
            // Belt-and-braces: `forget_identity_biometrics` already erased them.
            wrote.prints = true;
            wrote.samples = true;
        }
    }
    // Kept to the rows still held for a parent: an applied or re-held row leaves it.
    if h_held_before || !h_held.is_empty() {
        g.put_held_replaced_keys(&h_held)?;
    }
    crate::sync::converge(g, hlc, clock, &mut out, wrote)?;
    crate::sync::edits_win(g, hlc, clock, rules.leased, &mut out)?;
    // Lines this page stamped here (rule 4a's re-stamps, rule 4b's rewrites) go out before
    // the track's queued cleared rows (docs/sync-spec.md 15.2).
    g.send_lines_first(hlc, clock)?;
    if let Some(after) = rules.after {
        after(g)?;
    }
    out.damaged_rows = g.skipped.borrow().clone();
    out.damaged_skipped = out.damaged_rows.len();
    Ok(out)
}

/// Where `person`'s `merged_into` chain ends (16.3 `chain_end`), reading each step's `deleted`
/// and `merged_into` only: today's answer of `ends_deleted` on readable rows.
///
/// # Errors
/// Store errors.
pub fn chain_end(g: &GuardedTx<'_, '_>, person: EntityId) -> Result<ChainEnd, StoreError> {
    let mut seen = Vec::new();
    let mut cur = person;
    loop {
        if seen.contains(&cur) {
            return Ok(ChainEnd::Unknown);
        }
        seen.push(cur);
        let Some((deleted, merged_into)) = g.chain_step(cur)? else {
            return Ok(ChainEnd::Unknown);
        };
        match (deleted, merged_into) {
            (Deleted::Yes, _) => return Ok(ChainEnd::Removed),
            (Deleted::Unreadable, _) | (_, Col::Unreadable) => return Ok(ChainEnd::Unknown),
            (Deleted::No, Col::Read(None)) => return Ok(ChainEnd::Live),
            (Deleted::No, Col::Read(Some(next))) => cur = next,
        }
    }
}

impl GuardedTx<'_, '_> {
    fn chain_step(&self, id: EntityId) -> Result<Option<ChainStep>, StoreError> {
        chain_step(self.c(), id)
    }
}

/// The identity `person` (16.3 `identity_tx`): `person` first, then every person whose
/// `merged_into` chain, read through [`GuardedTx::person_links`]' `id` and `merged_into`,
/// reaches it, by id (core `identity_of`'s rule and order). A person whose `id` or a chain
/// step's `merged_into` does not read cannot be placed and is counted in
/// [`Identity::skipped`]; `is_me` is never read.
///
/// # Errors
/// Store errors.
pub fn identity_tx(g: &GuardedRead<'_>, person: EntityId) -> Result<Identity, StoreError> {
    Ok(identity_of_links(&g.person_links()?, person))
}

/// [`identity_tx`] inside a transaction.
///
/// # Errors
/// Store errors.
pub fn identity_in(g: &GuardedTx<'_, '_>, person: EntityId) -> Result<Identity, StoreError> {
    Ok(identity_of_links(&g.person_links()?, person))
}

/// The identity rule over person links (core `chain_reaches`).
#[must_use]
pub fn identity_of_links(links: &[PersonLink], person: EntityId) -> Identity {
    let next_of = |id: EntityId| -> Option<Col<Option<EntityId>>> {
        links
            .iter()
            .find(|l| l.id == Col::Read(id))
            .map(|l| l.merged_into)
    };
    // `Some(true)` reaches, `Some(false)` does not, `None` cannot be placed.
    let reaches = |start: EntityId| -> Option<bool> {
        let mut seen = Vec::new();
        let mut cur = start;
        loop {
            if cur == person {
                return Some(true);
            }
            if seen.contains(&cur) {
                return Some(false);
            }
            seen.push(cur);
            match next_of(cur) {
                None | Some(Col::Read(None)) => return Some(false),
                Some(Col::Read(Some(next))) => cur = next,
                Some(Col::Unreadable) => return None,
            }
        }
    };
    let mut out = Identity {
        members: vec![person],
        skipped: 0,
    };
    for l in links {
        match l.id {
            Col::Read(id) if id == person => {}
            Col::Read(id) => match reaches(id) {
                Some(true) => out.members.push(id),
                Some(false) => {}
                None => out.skipped += 1,
            },
            Col::Unreadable => out.skipped += 1,
        }
    }
    out
}

#[cfg(test)]
mod tests {
    #[test]
    fn guarded_current_summary_query_is_the_repository_one() {
        assert_eq!(
            super::CURRENT_SUMMARY_ID_SQL,
            crate::repo::CURRENT_SUMMARY_SQL
        );
    }

    #[test]
    fn guarded_every_sql_constant_has_one_owner() {
        let src = include_str!("guarded.rs");
        for line in src.lines() {
            let line = line.trim_start();
            if let Some(rest) = line.strip_prefix("const ")
                && let Some((name, _)) = rest.split_once(':')
                && name.ends_with("_SQL")
            {
                assert!(
                    super::SQL_OWNERS.iter().any(|(c, _)| *c == name),
                    "{name} has no owner"
                );
            }
        }
    }
}
