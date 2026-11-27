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
use earshot_core::model::{
    DictionaryWord, Folder, Meeting, Note, RequestState, SegmentRow, Speaker, Summary,
    SummaryRequest, Track,
};
use earshot_core::voiceprint::{Person, Voiceprint, VoiceprintSample};
use rusqlite::{OptionalExtension, params};

use crate::db::{Store, StoreError, StoreTx};
use crate::guarded::{ChainEnd, Col, Deleted, GuardedTx, chain_end};
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
    /// `summary_request` (summary offload, T11.3da).
    SummaryRequest(SummaryRequest),
    /// `dictionary_word` (synced only while the device's Dictionary switch is on, T11.3e).
    DictionaryWord(DictionaryWord),
    /// `voiceprint_sample` (an opt-in kind with voiceprints, T12.77b2; on the wire since
    /// T12.77b8).
    VoiceprintSample(VoiceprintSample),
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
            SyncedRow::SummaryRequest($r) => $e,
            SyncedRow::DictionaryWord($r) => $e,
            SyncedRow::VoiceprintSample($r) => $e,
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

    /// Whether other rows belong to a row of this table, so that one arriving for the first
    /// time can free rows held for it ([`HoldReason::Parent`]): a meeting (its tracks,
    /// speakers, lines, notes, summaries and summary requests), a track and a speaker (lines).
    #[must_use]
    pub fn has_children(&self) -> bool {
        matches!(self, Self::Meeting(_) | Self::Track(_) | Self::Speaker(_))
    }

    /// Its sync metadata.
    #[must_use]
    pub fn meta(&self) -> &SyncMeta {
        each!(self, r => r.sync_meta())
    }

    /// The meeting it belongs to: its own id for a meeting, `None` for people, voiceprints,
    /// voice samples (never held for their meeting), folders and Dictionary words.
    #[must_use]
    pub fn meeting(&self) -> Option<EntityId> {
        match self {
            Self::Meeting(m) => Some(m.id),
            Self::Track(r) => Some(r.meeting_id),
            Self::Segment(r) => Some(r.meeting_id),
            Self::Speaker(r) => Some(r.meeting_id),
            Self::Note(r) => Some(r.meeting_id),
            Self::Summary(r) => Some(r.meeting_id),
            Self::SummaryRequest(r) => Some(r.meeting_id),
            Self::Person(_)
            | Self::Voiceprint(_)
            | Self::VoiceprintSample(_)
            | Self::Folder(_)
            | Self::DictionaryWord(_) => None,
        }
    }

    /// Whether the row waits while its meeting is in use here ([`HoldReason::Busy`]): every row
    /// of a meeting but a summary request, which holds no content and must reach a device
    /// whose summary job holds the meeting (a helper re-reads it before storing, the asker
    /// sees the helper's claim).
    #[must_use]
    pub fn waits_while_busy(&self) -> bool {
        !matches!(self, Self::SummaryRequest(_)) && self.meeting().is_some()
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
            "summary_request" => Self::SummaryRequest(v(record)?),
            "dictionary_word" => Self::DictionaryWord(v(record)?),
            // Through `VoiceprintSample::from_stored`: a live row that fails its check does not
            // decode (held as `Format`), a tombstone keeps no body.
            "voiceprint_sample" => Self::VoiceprintSample(v(record)?),
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
    /// The envelope's entity name when the row opened, even if its record did not decode
    /// (`None` for a newer format): a row of a kind switched off is skipped by it before it
    /// could be held (docs/voice-samples.md 7.2).
    pub entity: Option<String>,
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

/// One page of the outbox ([`Store::outbox_page`]).
#[derive(Debug, Clone, PartialEq)]
pub struct OutboxPage {
    /// Rows to push, oldest `updated_at` first.
    pub rows: Vec<SyncedRow>,
    /// Queued rows that could not be read: left queued and reported, never sent.
    pub unreadable: usize,
}

/// The opt-in kinds this device syncs (docs/sync.md section 9): a row of a kind switched off
/// is neither pushed nor applied here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SyncKinds {
    /// Voiceprints (`settings/sync-voiceprints`, off by default).
    pub voiceprints: bool,
    /// Dictionary words (`settings/sync-dictionary`, on by default, T11.3e).
    pub dictionary: bool,
}

impl SyncKinds {
    /// Every opt-in kind on.
    pub const ALL: Self = Self {
        voiceprints: true,
        dictionary: true,
    };

    /// Whether rows of `table` go out and come in under these switches (`true` for every
    /// table that is not opt-in).
    #[must_use]
    pub fn syncs(self, table: &str) -> bool {
        match table {
            "voiceprint" | "voiceprint_sample" => self.voiceprints,
            "dictionary_word" => self.dictionary,
            _ => true,
        }
    }
}

/// Work the caller does inside [`Store::apply_remote`]'s transaction after the page's writes
/// and the convergence rules (the runtime rebuilds `settings/vocabulary` from the live
/// Dictionary rows there, so the list never differs from the rows it was built from).
pub type AfterApply<'a> = &'a dyn Fn(&mut crate::GuardedTx<'_, '_>) -> Result<(), StoreError>;

/// What may be applied now.
pub struct ApplyRules<'a> {
    /// The opt-in kinds this device syncs.
    pub kinds: SyncKinds,
    /// Whether a meeting is in use on this device (a job lease of this process).
    pub leased: &'a dyn Fn(EntityId) -> bool,
    /// This device's wall clock, Unix ms.
    pub now_ms: u64,
    /// Run last inside the page's transaction, when given; its error rolls the page back.
    pub after: Option<AfterApply<'a>>,
}

/// What a page did. Counts only.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Applied {
    /// Rows written (later than the local copy, or new).
    pub written: usize,
    /// Of those, rows that were not here before and that other rows belong to
    /// ([`SyncedRow::has_children`]): a row held for its parent may apply now, so the client
    /// tries those again in the same tick (T14.42).
    pub parents_written: usize,
    /// Rows not later than the local copy: nothing changed.
    pub ignored: usize,
    /// Voiceprint or Dictionary rows skipped because that kind's sync is off.
    pub skipped: usize,
    /// Rows held, per reason, in [`HoldReason::ALL`] order.
    pub held: [usize; 4],
    /// Biometric rows erased by the page: voiceprints, voice samples and centroids forgotten
    /// because a person was deleted elsewhere, live prints a pulled print tombstone replaced,
    /// and samples of a print tombstoned here (the dead-print repair).
    pub biometrics_forgotten: usize,
    /// Live meeting rows refused because the meeting is deleted here, and live voiceprints
    /// refused because the print is deleted here (T12.77b3): the tombstone was stamped again,
    /// so it goes out and wins at the relay.
    pub deletions_kept: usize,
    /// "Me" people merged into the earliest one (two devices each made their own).
    pub mes_merged: usize,
    /// Live summaries tombstoned because a later-created one of the same meeting stays.
    pub summaries_replaced: usize,
    /// Summary requests this device withdrew that came back open (claimed elsewhere before
    /// that device heard of the withdrawal): the withdrawal was stamped again here.
    pub withdrawals_kept: usize,
    /// Words of a re-transcription dropped because a user's edited line of another run covers
    /// them (docs/sync-spec.md section 7 rule 4b, T12.92).
    pub words_dropped_under_edits: usize,
    /// Pulled copies of a final line that lost to the user's edit here although they were
    /// stamped later (rule 4a): the edit was stamped again, so it goes out and wins at the
    /// relay.
    pub edited_lines_kept: usize,
    /// Lines rule 4b rewrote or tombstoned (the rows behind
    /// [`Applied::words_dropped_under_edits`]).
    pub lines_rewritten_under_edits: usize,
    /// Speaker rows made live again because a user's edited line still names them (rule 4c).
    pub speakers_revived: usize,
    /// Segment rows whose copy here held text before the page and holds none after it
    /// (docs/sync-spec.md 15.4): a pulled tombstone that held text, cleared or erased here; a
    /// pulled copy that holds no text over one that did; a line rule 4b took out. Never a row
    /// whose text only changed. Rows, never content. A tick with any runs `flush_erasures`.
    pub text_emptied: usize,
    /// The meetings a row written here belongs to (a meeting's own row, its tracks, lines,
    /// speakers, notes, summaries and summary requests), including a summary the convergence
    /// rules replaced: ids only, so the window can read again what changed (T12.81). A row
    /// held, ignored or skipped names nothing.
    pub meetings: std::collections::BTreeSet<EntityId>,
    /// Whether a person row was written here or "Me" people merged: names shown in any meeting
    /// resolve through persons, so the window reads the open meeting again (T12.81).
    pub people: bool,
    /// Rows here that did not read and that the page and its rules skipped, each
    /// `(table, rowid)` once (docs/sync-spec.md 16.7). Counts only.
    pub damaged_skipped: usize,
    /// The `(table, rowid)` of each row behind [`Applied::damaged_skipped`], so a caller can
    /// count the rows of several pages once (SQLite rowids only: never logged, shown or stored).
    pub damaged_rows: std::collections::BTreeSet<(&'static str, i64)>,
    /// Tombstones the page stamped again and kept queued (docs/sync-spec.md 7A.2 C1): a meeting,
    /// voiceprint or voice sample tombstone kept against a later live copy, and a person or folder
    /// tombstone kept or written with this device's stamp (P1, F1). Not a skipped live child of a
    /// deleted meeting, which is stamped nowhere ([`Applied::deletions_kept`]).
    pub tombstones_restamped: usize,
    /// The ids of the rows the page wrote with this device's stamp that C1's extra push sends
    /// (docs/sync-spec.md 7A.2): the tombstones of [`Applied::tombstones_restamped`] and what P2,
    /// P3, a pulled person tombstone's erasure, rule 3a and the voice rules wrote. Ids only.
    pub stamped_here: std::collections::BTreeSet<EntityId>,
    /// Items the page erased (docs/sync-spec.md 7A.4): person and folder names cleared, speaker
    /// names P3 rewrote, and [`Applied::biometrics_forgotten`] (voices and identity entries). A
    /// page with any is followed by `flush_erasures`.
    pub erased: usize,
    /// Live persons tombstoned because their `merged_into` chain ends at a person removed here
    /// (P2).
    pub persons_removed_with: usize,
    /// Speaker rows unlinked from a person removed here (P3).
    pub speakers_unlinked: usize,
    /// Distinct chain steps a `merged_into` walk could not read (docs/sync-spec.md 7A.2 D1),
    /// each also in [`Applied::damaged_rows`]. A loop or a person not known here counts nothing.
    pub chains_unknown: usize,
}

impl Applied {
    pub(crate) fn hold(&mut self, reason: HoldReason) {
        self.held[HoldReason::ALL
            .iter()
            .position(|r| *r == reason)
            .expect("every reason is in ALL")] += 1;
    }

    /// What P5 did to a row's identity entry in `meeting` (docs/sync-spec.md 7A.9): a changed
    /// record names its meeting and the people; a dropped entry is an erasure (7A.4).
    pub(crate) fn relinked(&mut self, meeting: EntityId, change: crate::identities::RowRelink) {
        use crate::identities::RowRelink;
        if change == RowRelink::Unchanged {
            return;
        }
        self.meetings.insert(meeting);
        self.people = true;
        if change == RowRelink::Dropped {
            self.erased += 1;
        }
    }
}

/// The kinds of row a page wrote here that a convergence repair reads ([`converge`]): each
/// repair runs only for a page that wrote one of its kinds (T14.50). Marked for every write
/// the page makes to such a table, a kept copy stamped again included.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct Wrote {
    /// A person row (rule 1, and the `merged_into` chains rule 3a follows).
    pub(crate) persons: bool,
    /// A summary row (rule 2).
    pub(crate) summaries: bool,
    /// A voiceprint row (rules 3 and 3a).
    pub(crate) prints: bool,
    /// A voice sample row (rules 3 and 3a).
    pub(crate) samples: bool,
    /// A speaker row (P3, docs/sync-spec.md 7A.2).
    pub(crate) speakers: bool,
}

impl Wrote {
    /// Every kind: every repair runs.
    pub(crate) const EVERYTHING: Self = Self {
        persons: true,
        summaries: true,
        prints: true,
        samples: true,
        speakers: true,
    };

    pub(crate) fn row(&mut self, row: &SyncedRow) {
        match row {
            SyncedRow::Person(_) => self.persons = true,
            SyncedRow::Summary(_) => self.summaries = true,
            SyncedRow::Voiceprint(_) => self.prints = true,
            SyncedRow::VoiceprintSample(_) => self.samples = true,
            SyncedRow::Speaker(_) => self.speakers = true,
            SyncedRow::Meeting(_)
            | SyncedRow::Track(_)
            | SyncedRow::Segment(_)
            | SyncedRow::Note(_)
            | SyncedRow::Folder(_)
            | SyncedRow::SummaryRequest(_)
            | SyncedRow::DictionaryWord(_) => {}
        }
    }
}

/// A mark of the `summary_request` table ([`Store::summary_requests_mark`], T14.50): how many
/// rows it holds and the latest `updated_at` among them. A request this device writes changes
/// it: a new request adds a row, and any write here is stamped after every stamp this device
/// has written or applied. A pulled copy may change a row without changing the mark (its stamp
/// can be below the latest), so a caller that also needs to hear of pulled rows asks the sync
/// tick ([`Applied::written`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RequestsMark {
    /// Rows in the table, tombstones included.
    pub rows: u64,
    /// The latest `updated_at` of any row; `None` for an empty table.
    pub latest: Option<HlcTimestamp>,
}

/// A `sync_pending` row as read: key, seq, wall_ms, logical, node, body, reason.
type HeldColumns = (Vec<u8>, i64, i64, u32, Vec<u8>, Vec<u8>, String);

fn held_columns(r: &rusqlite::Row<'_>) -> rusqlite::Result<HeldColumns> {
    Ok((
        r.get(0)?,
        r.get(1)?,
        r.get(2)?,
        r.get(3)?,
        r.get(4)?,
        r.get(5)?,
        r.get(6)?,
    ))
}

fn held_of((key, seq, wall, logical, node, body, reason): HeldColumns) -> Result<Held, StoreError> {
    let bad = || StoreError::Corrupt("a held sync row".into());
    Ok(Held {
        key: key.try_into().map_err(|_| bad())?,
        seq: u64::try_from(seq).map_err(|_| bad())?,
        stamp: HlcTimestamp {
            wall_ms: u64::try_from(wall).map_err(|_| bad())?,
            logical,
            node: earshot_core::identity::DeviceId::from_bytes(node.try_into().map_err(|_| bad())?),
        },
        body,
        reason: HoldReason::of_word(&reason).ok_or_else(bad)?,
    })
}

/// `local_state` key of the row keys held by H1 or H2 (docs/sync-spec.md 15.4): the release
/// retries only those. A list of 16-byte row keys. The hold word stays `parent` (0.9.0 refuses
/// a word it does not know), so which `parent` rows are the two holds' is kept here.
pub const HELD_REPLACED_KEY: &str = "sync-held-replaced";

/// The H1 and H2 row keys as stored (`held_replaced_keys`).
const HELD_REPLACED_KEYS_SQL: &str = "SELECT value FROM local_state WHERE key = ?1";
/// Whether a row is held for a parent (`held_for_parent`).
const HELD_FOR_PARENT_SQL: &str = "SELECT 1 FROM sync_pending WHERE key = ?1 AND reason = ?2";
/// The H1 and H2 row keys removed (`put_held_replaced_keys`).
const DELETE_HELD_REPLACED_SQL: &str = "DELETE FROM local_state WHERE key = ?1";
/// The H1 and H2 row keys stored (`put_held_replaced_keys`).
const PUT_HELD_REPLACED_SQL: &str = "INSERT INTO local_state (key, value) VALUES (?1, ?2) \
     ON CONFLICT(key) DO UPDATE SET value = excluded.value";

/// C1's id restriction (docs/sync-spec.md 7A.2): the temporary table `outbox_page_only` joins,
/// made and filled inside the read's own transaction (`fill_push_only`).
const PUSH_ONLY_CREATE_SQL: &str =
    "CREATE TEMP TABLE IF NOT EXISTS push_only (id BLOB PRIMARY KEY)";
/// One id of the restriction (`fill_push_only`).
const PUSH_ONLY_INSERT_SQL: &str = "INSERT OR IGNORE INTO temp.push_only (id) VALUES (?1)";
/// The restriction dropped after the read (`drop_push_only`).
const PUSH_ONLY_DROP_SQL: &str = "DROP TABLE IF EXISTS temp.push_only";
/// The join that restricts an outbox read to the table's ids (`push_only_join`).
const PUSH_ONLY_JOIN_SQL: &str = " JOIN temp.push_only p ON p.id = o.id";

/// Makes `temp.push_only` and fills it with `ids`. The table never outlives its transaction:
/// it is dropped before the commit, and a transaction that fails is rolled back with it.
fn fill_push_only(
    c: &rusqlite::Connection,
    ids: &std::collections::BTreeSet<EntityId>,
) -> Result<(), StoreError> {
    c.execute(PUSH_ONLY_CREATE_SQL, [])?;
    let mut st = c.prepare_cached(PUSH_ONLY_INSERT_SQL)?;
    for id in ids {
        st.execute(params![&id.as_uuid().as_bytes()[..]])?;
    }
    Ok(())
}

/// Drops `temp.push_only`.
fn drop_push_only(c: &rusqlite::Connection) -> Result<(), StoreError> {
    c.execute(PUSH_ONLY_DROP_SQL, [])?;
    Ok(())
}

/// The join of [`PUSH_ONLY_JOIN_SQL`], for the outbox read's query.
const fn push_only_join() -> &'static str {
    PUSH_ONLY_JOIN_SQL
}

pub(crate) fn held_replaced_keys(
    c: &rusqlite::Connection,
) -> Result<std::collections::BTreeSet<[u8; 16]>, StoreError> {
    let bytes: Option<Vec<u8>> = c
        .query_row(HELD_REPLACED_KEYS_SQL, params![HELD_REPLACED_KEY], |r| {
            r.get(0)
        })
        .optional()?;
    Ok(bytes
        .unwrap_or_default()
        .as_chunks::<16>()
        .0
        .iter()
        .copied()
        .collect())
}

/// Writes the set, keeping only keys still held for a parent.
pub(crate) fn put_held_replaced_keys(
    c: &rusqlite::Connection,
    keys: &std::collections::BTreeSet<[u8; 16]>,
) -> Result<(), StoreError> {
    let mut value = Vec::with_capacity(keys.len() * 16);
    for k in keys {
        if held_for_parent(c, k)? {
            value.extend_from_slice(k);
        }
    }
    if value.is_empty() {
        c.execute(DELETE_HELD_REPLACED_SQL, params![HELD_REPLACED_KEY])?;
    } else {
        c.execute(PUT_HELD_REPLACED_SQL, params![HELD_REPLACED_KEY, value])?;
    }
    Ok(())
}

/// Whether the row `key` is held for a parent.
pub(crate) fn held_for_parent(
    c: &rusqlite::Connection,
    key: &[u8; 16],
) -> Result<bool, StoreError> {
    Ok(c.query_row(
        HELD_FOR_PARENT_SQL,
        params![&key[..], HoldReason::Parent.word()],
        |_| Ok(()),
    )
    .optional()?
    .is_some())
}

/// What [`Store::apply_remote`] does with a pulled summary request over the copy here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Withdrawal {
    /// Whole-row last writer wins, as for any row.
    Lww,
    /// Write the pulled withdrawal whatever the stamps.
    TakeIt,
    /// Keep the withdrawal here, re-stamped when the pulled copy is later.
    KeepMine,
}

/// The withdrawal rule of summary offload (docs/sync-spec.md section 10.1): once the asker takes
/// a request back it writes the summary itself, so the request must never open again.
///
/// 1. A pulled withdrawal replaces an open request here (requested or claimed) whatever the two
///    stamps: the target may have claimed it after the asker withdrew, before it heard of that.
/// 2. On the asker (`me`), a withdrawn request is never replaced by an open one: the pulled copy
///    is refused, and when it is later the withdrawal is stamped again here, so it goes out and
///    wins at the relay (only the asker writes a withdrawal, and here the asker writes it).
///
/// Anything else (a done or declined answer, two copies of one state) is last writer wins: a
/// withdrawal and a stored summary may both happen, and the summary rule keeps the later.
pub(crate) fn withdrawal(
    here: &SummaryRequest,
    there: &SummaryRequest,
    me: earshot_core::identity::DeviceId,
) -> Withdrawal {
    let withdrawn = RequestState::Withdrawn;
    if there.state == withdrawn && here.state.is_open() {
        if here.meta.updated_at >= there.meta.updated_at {
            Withdrawal::TakeIt
        } else {
            Withdrawal::Lww
        }
    } else if here.state == withdrawn && there.state.is_open() && here.asking_device == me {
        Withdrawal::KeepMine
    } else {
        Withdrawal::Lww
    }
}

/// Rule 3a of [`converge`]: live prints and samples of a person tombstoned here are erased,
/// prints zeroed and tombstoned by rowid, samples by id (or by rowid when their id does not
/// read). A row whose `person_id` does not read is skipped and counted; a chain through a step
/// that does not read is [`ChainEnd::Unknown`] and erases nothing (docs/sync-spec.md 16.5
/// items 1 to 3), and a chain ending at the user's tombstone is [`ChainEnd::MeTombstone`],
/// never a removal (7A.2 P4).
pub(crate) fn dead_person_repair<C: Clock + ?Sized>(
    g: &mut GuardedTx<'_, '_>,
    hlc: &mut Hlc,
    clock: &C,
    out: &mut Applied,
) -> Result<(), StoreError> {
    for (rowid, id, person, meeting) in g.dead_person_samples()? {
        let Col::Read(Some(person)) = person else {
            g.count_damaged("voiceprint_sample", &[rowid]);
            continue;
        };
        if chain_end(g, person)? == ChainEnd::Removed {
            let erased = match id {
                Col::Read(id) => g.erase_sample_by_id(hlc, clock, id)?,
                Col::Unreadable => g.erase_sample_by_rowid(hlc, clock, rowid)?,
            };
            if erased {
                erased_sample(out, meeting.read().flatten());
            }
        }
    }
    for (rowid, id, person) in g.dead_person_prints()? {
        let Col::Read(person) = person else {
            g.count_damaged("voiceprint", &[rowid]);
            continue;
        };
        if chain_end(g, person)? != ChainEnd::Removed {
            continue;
        }
        g.erase_print_by_rowid(hlc, clock, rowid, id.read())?;
        out.biometrics_forgotten += 1;
        out.people = true;
    }
    Ok(())
}

/// P2 of docs/sync-spec.md 7A.2: every live person whose `merged_into` chain ends at a person
/// tombstoned here that is not the user ([`ChainEnd::Removed`]) is tombstoned with its name
/// cleared (`merged_into` kept), with this device's stamp, its biometrics forgotten as for a
/// pulled person tombstone; a damaged row by rowid (16.4). A chain that ends at the user's
/// tombstone, at a person not known here, or loops, changes nothing; the user is never
/// tombstoned; a person whose own `is_me` does not read is skipped and counted (16.5 item 18).
///
/// # Errors
/// Store errors.
pub(crate) fn merged_into_removed_repair<C: Clock + ?Sized>(
    g: &mut GuardedTx<'_, '_>,
    hlc: &mut Hlc,
    clock: &C,
    out: &mut Applied,
) -> Result<(), StoreError> {
    for l in g.person_links()? {
        let Col::Read(person) = l.id else { continue };
        if l.deleted != Deleted::No || l.merged_into == Col::Read(None) {
            continue;
        }
        if chain_end(g, person)? != ChainEnd::Removed {
            continue;
        }
        match l.is_me {
            Col::Read(false) => {}
            Col::Read(true) => continue,
            Col::Unreadable => {
                g.count_damaged("person", &[l.rowid]);
                continue;
            }
        }
        out.biometrics_forgotten += crate::identities::forget_identity_biometrics(
            g,
            person,
            hlc,
            clock,
            crate::identities::Erasure::Pulled,
        )?
        .erased();
        match g.person_or_damaged(person)? {
            None => continue,
            Some(Ok(mut p)) => {
                if !p.display_name.is_empty() {
                    out.erased += 1;
                }
                p.display_name.clear();
                p.meta.mark_deleted(hlc, clock);
                g.put_person(p)?;
                g.note_stamped(person);
            }
            Some(Err(())) => {
                // Placed by its `id` and `merged_into`, both read: tombstoned by rowid with the
                // name cleared whatever it read as (16.4).
                g.tombstone_person_by_rowid(hlc, clock, l.rowid, Some(person))?;
                out.erased += 1;
            }
        }
        out.persons_removed_with += 1;
        out.people = true;
    }
    Ok(())
}

/// P3 of docs/sync-spec.md 7A.2: no speaker row stays linked to a person tombstoned here that
/// is not the user. Every such row is rewritten as `Store::remove_person` rewrites it, through
/// [`crate::delete::unlink_speakers_tx`] (a damaged row by rowid under the written-columns rule
/// of 16.4). A row linked to the user's tombstone is left linked (P4). Run after rule 4c and
/// before the host's `after`.
///
/// # Errors
/// Store errors.
pub(crate) fn unlink_removed_speakers<C: Clock + ?Sized>(
    g: &mut GuardedTx<'_, '_>,
    hlc: &mut Hlc,
    clock: &C,
    out: &mut Applied,
) -> Result<(), StoreError> {
    // After P2 a chain never ends at such a tombstone through a live person, so the direct link
    // is the whole test.
    let removed: Vec<EntityId> = g
        .person_links()?
        .into_iter()
        .filter(|l| l.deleted == Deleted::Yes && l.is_me == Col::Read(false))
        .filter_map(|l| l.id.read())
        .collect();
    if removed.is_empty() {
        return Ok(());
    }
    // A row whose `person_id` does not read is not written, but its earlier person cannot be
    // read either, so P5 drops its entry (docs/sync-spec.md 7A.2 D1: errs towards erasing).
    for l in g.speaker_links(None)? {
        if let (Col::Read(meeting), Col::Read(speaker), Col::Unreadable) =
            (l.meeting_id, l.id, l.person_id)
        {
            let change = crate::identities::drop_row_entry_guarded(g, meeting, speaker)?;
            out.relinked(meeting, change);
        }
    }
    let mut on_row = |g: &mut GuardedTx<'_, '_>, row: &crate::delete::UnlinkedRow| {
        // P5 at this row's write: unlinked from a removed person, its entry goes.
        if let Some(speaker) = row.speaker {
            let change = crate::identities::row_relinked_guarded(
                g,
                row.meeting,
                speaker,
                Some(row.from),
                None,
            )?;
            out.relinked(row.meeting, change);
        }
        out.speakers_unlinked += 1;
        out.meetings.insert(row.meeting);
        out.people = true;
        if row.name_erased {
            out.erased += 1;
        }
        Ok(())
    };
    crate::delete::unlink_speakers_tx(g, &removed, hlc, clock, &mut on_row)?;
    Ok(())
}

/// The store half of the one-off removal repair (docs/sync-spec.md 7A.2, "The one-off removal
/// repair, sync on or off"): P2 ([`merged_into_removed_repair`]) then P3
/// ([`unlink_removed_speakers`]) with P5's entry drops, in the caller's guarded transaction and
/// outside any page, with this device's stamp, so what it writes waits in the outbox like any
/// edit. Counted as a page's are: [`Applied::persons_removed_with`],
/// [`Applied::speakers_unlinked`], [`Applied::erased`] (biometrics included), the rows skipped
/// ([`Applied::damaged_skipped`]) and [`Applied::meetings`] / [`Applied::people`] for the window.
/// A second run finds nothing left (7A.8).
///
/// # Errors
/// Store errors (the caller rolls back).
pub fn removal_repair_tx<C: Clock + ?Sized>(
    g: &mut GuardedTx<'_, '_>,
    hlc: &mut Hlc,
    clock: &C,
) -> Result<Applied, StoreError> {
    let mut out = Applied::default();
    merged_into_removed_repair(g, hlc, clock, &mut out)?;
    unlink_removed_speakers(g, hlc, clock, &mut out)?;
    out.erased += out.biometrics_forgotten;
    out.stamped_here = g.stamped_ids();
    out.chains_unknown = g.chains_unknown();
    out.damaged_rows = g.damaged_rows();
    out.damaged_skipped = out.damaged_rows.len();
    Ok(out)
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
/// repair also covers rows that arrived in earlier pages. A row that does not read takes no
/// part in the decision it would have fed and is counted (docs/sync-spec.md 16.2 R3).
///
/// Each repair runs only when the page wrote a row of a kind it reads (`wrote`, T14.50): a
/// repair's answer depends only on the rows of those kinds, every earlier page left them
/// repaired, and nothing this device writes itself leaves them otherwise (a second "Me" is
/// never made while one is live, a summary replaces the one before it in its own transaction,
/// a voice is saved only for a live person and a removal forgets the voices with the person).
/// So a page that wrote none of a repair's kinds would find nothing for it to do. With
/// [`Wrote::EVERYTHING`] every repair runs, as before T14.50
/// ([`Store::apply_remote_every_repair`]).
pub(crate) fn converge<C: Clock + ?Sized>(
    g: &mut GuardedTx<'_, '_>,
    hlc: &mut Hlc,
    clock: &C,
    out: &mut Applied,
    wrote: Wrote,
) -> Result<(), StoreError> {
    // Rule 1 reads persons.
    let mut mes: Vec<Person> = if wrote.persons {
        g.persons_or_damaged()?
            .into_iter()
            .filter(|p| p.is_me && p.is_live())
            .collect()
    } else {
        Vec::new()
    };
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
            g.put_person(kept)?;
        }
        for p in rest {
            let mut merged = p.clone();
            merged.merged_into = Some(keep.id);
            merged.meta.touch(hlc, clock);
            g.put_person(merged)?;
            out.mes_merged += 1;
            out.people = true;
        }
    }
    // P2 reads persons (docs/sync-spec.md 7A.2): after rule 1, before rule 3a, so 3a and the
    // voice rules see the persons it tombstoned.
    if wrote.persons {
        merged_into_removed_repair(g, hlc, clock, out)?;
    }
    // Rule 2 reads summaries.
    let crowded: Vec<EntityId> = if wrote.summaries {
        g.crowded_meetings()?
    } else {
        Vec::new()
    };
    for meeting in crowded {
        let mut live: Vec<Summary> = g
            .summaries_or_damaged(meeting)?
            .into_iter()
            .filter(|s| !s.meta.deleted)
            .collect();
        live.sort_by_key(|s| (s.meta.created_at, s.id));
        live.pop();
        for s in live {
            let mut replaced = s;
            replaced.meta.deleted = true;
            replaced.meta.touch(hlc, clock);
            out.meetings.insert(replaced.meeting_id);
            g.put_summary(replaced)?;
            out.summaries_replaced += 1;
        }
    }
    // Rule 3a: no live voiceprint or voice sample of a person tombstoned here, directly or at
    // the end of a `merged_into` chain (docs/voice-samples.md 7.4). A person not known here,
    // or a chain that loops, leaves the row alone. It reads persons (rule 1's merges are
    // person rows too, and run only when the page wrote one), prints and samples.
    let forgotten_before = out.biometrics_forgotten;
    if wrote.persons || wrote.prints || wrote.samples {
        dead_person_repair(g, hlc, clock, out)?;
    }
    // Rule 3: no live voice sample of a print tombstoned here (docs/voice-samples.md 7.4). It
    // reads prints and samples, and rule 3a may just have tombstoned a print.
    let dead = if wrote.prints || wrote.samples || out.biometrics_forgotten != forgotten_before {
        g.dead_print_samples()?
    } else {
        Vec::new()
    };
    // Erased by id in SQL, never read back first: a damaged live row (one `from_stored`
    // refuses) is erased too and cannot abort the page; one whose id does not read, by rowid.
    for (rowid, id, meeting) in dead {
        let erased = match id {
            Col::Read(id) => g.erase_sample_by_id(hlc, clock, id)?,
            Col::Unreadable => g.erase_sample_by_rowid(hlc, clock, rowid)?,
        };
        if erased {
            erased_sample(out, meeting.read().flatten());
        }
    }
    Ok(())
}

/// Rule 4b and 4c (docs/sync-spec.md section 7, T12.92), run after the page's writes for each
/// meeting the page wrote that is live here and not leased by this process:
///
/// - (b) on each track, every word of a live final line without an edit whose `pass_run` is
///   set and differs from that of a live edited final line of the same track, and that lies
///   under it (core `drop_words_under_edits`), is dropped; a line that loses some words is
///   rewritten with the rest (same id, speaker and run), one that loses all is tombstoned, each
///   with this device's stamp. Lines of the edited line's own run are never compared, rows
///   without a run are never dropped, edited lines are never changed, and a line is written
///   only when its words change, so a second run writes nothing.
/// - (c) every tombstoned speaker row a live edited final line names is made live again, with
///   its stored name and person, with this device's stamp; a "Me" row comes back as a room
///   voice when the meeting already has a live "Me" (one "Me" per meeting), in id order.
///
/// Every device writes the same content, so the copies converge whichever stamp wins. A row
/// that does not read takes no part and is counted (docs/sync-spec.md 16.5 items 7 to 9): a
/// meeting whose `deleted` does not read is skipped, and a damaged speaker row that may be a
/// live "Me" counts as one, so a revival never makes a second.
pub(crate) fn edits_win<C: Clock + ?Sized>(
    g: &mut GuardedTx<'_, '_>,
    hlc: &mut Hlc,
    clock: &C,
    leased: &dyn Fn(EntityId) -> bool,
    out: &mut Applied,
) -> Result<(), StoreError> {
    use earshot_core::asr::{Segment, drop_words_under_edits};
    use earshot_core::diar::SpeakerRole;
    use earshot_core::model::TranscriptPass;
    use std::collections::{BTreeMap, BTreeSet};
    let meetings: Vec<EntityId> = out.meetings.iter().copied().collect();
    for m in meetings {
        if leased(m) || meeting_live(g, m)? != Some(true) {
            continue;
        }
        let rows = g.segments_or_damaged(m)?;
        let finals: Vec<&SegmentRow> = rows
            .iter()
            .filter(|r| r.pass == TranscriptPass::Final && !r.meta.deleted)
            .collect();
        let edited: Vec<&SegmentRow> = finals
            .iter()
            .copied()
            .filter(|r| r.text_edited_at.is_some())
            .collect();
        if edited.is_empty() {
            continue;
        }
        // The edits a line of (track, run) gives way to, built once per pair the meeting has.
        let mut edits_for: BTreeMap<(EntityId, EntityId), Vec<Segment>> = BTreeMap::new();
        for r in finals.iter().filter(|r| r.text_edited_at.is_none()) {
            let Some(run) = r.pass_run else {
                continue;
            };
            let edits = edits_for.entry((r.segment.track, run)).or_insert_with(|| {
                edited
                    .iter()
                    .filter(|e| e.segment.track == r.segment.track && e.pass_run != Some(run))
                    .map(|e| e.segment.clone())
                    .collect()
            });
            if edits.is_empty() {
                continue;
            }
            let (kept, dropped) = drop_words_under_edits(vec![r.segment.clone()], edits);
            if dropped == 0 {
                continue;
            }
            let mut new = (*r).clone();
            match kept.into_iter().next() {
                Some(segment) => {
                    new.segment = segment;
                    new.meta.touch(hlc, clock);
                    g.put_synced(&SyncedRow::Segment(new.clone()))?;
                    // A line this device stamped: lines go out before what they replace
                    // (docs/sync-spec.md 15.2).
                    g.note_line(&new);
                }
                // A line that lost all its words is taken out in the cleared form, through the
                // one statement (docs/sync-spec.md 15.2), so its words leave this device too.
                None => {
                    if let Some(rowid) = g.segment_rowid(r.segment.id)? {
                        g.clear_segment(rowid, hlc.now(clock))?;
                        out.text_emptied += 1;
                    }
                }
            }
            out.words_dropped_under_edits += dropped;
            out.lines_rewritten_under_edits += 1;
        }
        let named: BTreeSet<EntityId> =
            edited.iter().filter_map(|e| e.segment.speaker_id).collect();
        let (mut speakers, damaged) = g.speakers_or_damaged(m)?;
        speakers.sort_by_key(|s| s.id);
        // A damaged row whose role may be "Me" and that may be live counts as a live "Me".
        let links = g.speaker_links(Some(m))?;
        let maybe_me = links.iter().any(|l| {
            damaged.contains(&l.rowid)
                && matches!(l.role, Col::Read(SpeakerRole::Me) | Col::Unreadable)
                && l.deleted != Deleted::Yes
        });
        let mut me_live = maybe_me
            || speakers
                .iter()
                .any(|s| !s.meta.deleted && s.role == SpeakerRole::Me);
        // Names the meeting's live rows hold (a damaged live row's name when it reads), for a
        // revived row whose stored name is empty (docs/sync-spec.md 7A.2, rule 4c and a
        // nameless row).
        let mut taken: Vec<String> = speakers
            .iter()
            .filter(|s| !s.meta.deleted)
            .map(|s| s.display_name.clone())
            .collect();
        for l in links.iter().filter(|l| damaged.contains(&l.rowid)) {
            if l.deleted == Deleted::No
                && let Col::Read(name) = &l.display_name
            {
                taken.push(name.clone());
            }
        }
        for s in speakers {
            if s.meta.deleted && named.contains(&s.id) {
                let mut back = s;
                back.meta.deleted = false;
                if back.role == SpeakerRole::Me {
                    if me_live {
                        back.role = SpeakerRole::Room;
                    }
                    me_live = true;
                }
                if back.display_name.is_empty() {
                    back.display_name = crate::first_free_speaker_name(&taken);
                }
                taken.push(back.display_name.clone());
                back.meta.touch(hlc, clock);
                g.put_synced(&SyncedRow::Speaker(back))?;
                out.speakers_revived += 1;
            }
        }
    }
    Ok(())
}

/// Whether `meeting` is live here (docs/sync-spec.md 16.5 items 7 and 10): `Some(true)` live,
/// `Some(false)` a tombstone or absent, `None` when its `deleted` does not read (counted).
pub(crate) fn meeting_live(
    g: &GuardedTx<'_, '_>,
    meeting: EntityId,
) -> Result<Option<bool>, StoreError> {
    Ok(match g.meeting_or_damaged(meeting)? {
        None => Some(false),
        Some(Ok(m)) => Some(!m.meta.deleted),
        Some(Err(())) => match g.meeting_deleted(meeting)? {
            Some(Deleted::No) => Some(true),
            Some(Deleted::Yes) | None => Some(false),
            Some(Deleted::Unreadable) => None,
        },
    })
}

/// What [`Store::apply_remote`] does with a pulled segment row once it is written
/// (docs/sync-spec.md 15.4).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum AfterWrite {
    /// Nothing more.
    #[default]
    Nothing,
    /// It is a tombstone that holds text: clear it here.
    Clear,
    /// It is a tombstone that holds text of a meeting deleted here: erase it.
    Erase,
}

/// The pulled segment row's part of the apply ([`segment_plan`]).
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct SegmentPlan {
    /// H1 or H2 holds it as `parent`.
    pub(crate) hold: bool,
    /// What follows the write.
    pub(crate) after: AfterWrite,
    /// It holds no text over a copy here that did ([`Applied::text_emptied`]).
    pub(crate) emptied: bool,
    /// It is a live final row and its track had no live final row here: rows held for it may
    /// apply now ([`Applied::parents_written`]).
    pub(crate) first_final: bool,
}

/// The holds and the clearing of docs/sync-spec.md 15.4 for the pulled segment row `there`,
/// which is the copy to write over `local` (a local row that does not read is `None`, as an
/// absent one). `release` is the release's `blocked`: in the release, a row is held only when
/// its track is blocked. Reads no local row whole but `local`, which the page read already.
pub(crate) fn segment_plan(
    c: &rusqlite::Connection,
    there: &SegmentRow,
    local: Option<&SyncedRow>,
    release: Option<&dyn Fn(EntityId, EntityId) -> bool>,
) -> Result<SegmentPlan, StoreError> {
    use earshot_core::model::TranscriptPass;
    let (meeting, track) = (there.meeting_id, there.segment.track);
    let here = match local {
        Some(SyncedRow::Segment(l)) => Some(l),
        _ => None,
    };
    let meeting_deleted = crate::repo::row_reads_deleted(c, "meeting", meeting)?;
    let mut plan = SegmentPlan {
        emptied: here.is_some_and(SegmentRow::holds_text) && !there.holds_text(),
        ..SegmentPlan::default()
    };
    if there.meta.deleted && there.holds_text() {
        plan.after = if meeting_deleted {
            AfterWrite::Erase
        } else {
            AfterWrite::Clear
        };
        // Cleared here, so it counts through the clearing.
        plan.emptied = false;
    }
    let meeting_live = !meeting_deleted
        && crate::repo::synced_by_id_or_damaged(c, "meeting", meeting)?
            .is_ok_and(|m| m.is_some_and(|m| !m.meta().deleted));
    if !meeting_live || !crate::replaced::track_of_meeting(c, meeting, track)? {
        return Ok(plan);
    }
    if there.pass == TranscriptPass::Final && !there.meta.deleted {
        plan.first_final = !crate::replaced::has_readable_live_final(c, meeting, track, None)?;
    }
    let may_hold = release.is_none_or(|blocked| blocked(meeting, track));
    if !there.meta.deleted || there.is_erasure() || !may_hold {
        return Ok(plan);
    }
    let Some(here) = here.filter(|h| !h.meta.deleted) else {
        return Ok(plan);
    };
    plan.hold = match (there.pass, here.pass) {
        // H1, a cleared draft over a live draft whose track shows no final line here.
        (TranscriptPass::Live, TranscriptPass::Live) => {
            !there.holds_text()
                && !crate::replaced::has_readable_live_final(c, meeting, track, None)?
        }
        // H2, a final line taken out over the last live final line of its track here.
        (TranscriptPass::Final, TranscriptPass::Final) => {
            !crate::replaced::has_readable_live_final(c, meeting, track, Some(here.segment.id))?
        }
        _ => false,
    };
    Ok(plan)
}

/// [`Store::outbox_sent`]'s transaction (docs/sync-spec.md 16.5 item 16): each row's stamp is
/// read from its three `updated_*` columns alone, so a row damaged after its send leaves its
/// entry queued (it is unsendable anyway) instead of failing the tick.
///
/// # Errors
/// Store errors.
pub(crate) fn outbox_sent_in(
    g: &mut GuardedTx<'_, '_>,
    sent: &[(&str, EntityId, HlcTimestamp)],
) -> Result<usize, StoreError> {
    let mut gone = 0;
    for (table, id, stamp) in sent {
        if SYNCED_TABLES.contains(table) && g.updated_at_lenient(table, *id)? == Some(Some(*stamp))
        {
            gone += g.outbox_forget(table, *id)?;
        }
    }
    Ok(gone)
}

/// Counts a sample a repair erased and names its meeting (read in the repair's own query), so
/// a window showing that meeting reloads.
pub(crate) fn erased_sample(out: &mut Applied, meeting: Option<EntityId>) {
    out.biometrics_forgotten += 1;
    out.people = true;
    if let Some(m) = meeting {
        out.meetings.insert(m);
    }
}

impl StoreTx<'_> {
    /// Queues every synced row to be sent, tombstones included, inside the caller's
    /// transaction; returns how many were not already queued. Held rows are left alone: for a
    /// device whose relay lost rows (it was restored from an older copy, T14.45), which sends
    /// everything again to the same vault and whose held rows, sealed under the same key, the
    /// relay may no longer hold. A device joining a vault uses [`Store::queue_everything`].
    ///
    /// # Errors
    /// Store errors.
    pub fn queue_every_row(&self) -> Result<u64, StoreError> {
        let mut queued = 0;
        for table in SYNCED_TABLES {
            queued += self.conn.execute(
                &format!(
                    "INSERT INTO sync_outbox (tbl, id) SELECT ?1, r.id FROM {table} r \
                     WHERE NOT EXISTS (SELECT 1 FROM sync_outbox o WHERE o.tbl = ?1 AND o.id = r.id)"
                ),
                params![table],
            )?;
        }
        // Lines go out before what they replace (docs/sync-spec.md 15.2): every track with
        // queued cleared rows is checked before this transaction commits.
        self.note_every_cleared_track()?;
        Ok(u64::try_from(queued).unwrap_or(u64::MAX))
    }
}

impl Store {
    /// Up to `limit` rows changed on this device and not yet sent, oldest `updated_at` first,
    /// across every synced table.
    ///
    /// # Errors
    /// Store errors.
    pub fn outbox(&self, limit: usize) -> Result<Vec<SyncedRow>, StoreError> {
        self.outbox_except(limit, &[], SyncKinds::ALL)
    }

    /// [`Store::outbox`] leaving out, in the query itself, the rows of the meetings in
    /// `meetings` (a meeting being recorded: its rows wait until it is saved) and every row of a
    /// kind switched off in `kinds` (voiceprints, Dictionary words). Left out in SQL rather than
    /// after reading, so
    /// a long recording's rows cannot fill every page and keep other rows from going.
    ///
    /// # Errors
    /// Store errors.
    pub fn outbox_except(
        &self,
        limit: usize,
        meetings: &[EntityId],
        kinds: SyncKinds,
    ) -> Result<Vec<SyncedRow>, StoreError> {
        Ok(self.outbox_page(limit, meetings, kinds)?.rows)
    }

    /// [`Store::outbox_except`], also counting the queued rows that could not be read (a damaged
    /// voice sample row): they stay queued and are left out of the page, so one damaged row
    /// never stops every push; the push reports them.
    ///
    /// # Errors
    /// Store errors.
    pub fn outbox_page(
        &self,
        limit: usize,
        meetings: &[EntityId],
        kinds: SyncKinds,
    ) -> Result<OutboxPage, StoreError> {
        self.outbox_page_only(limit, meetings, kinds, None)
    }

    /// [`Store::outbox_page`], restricted to the rows whose ids are in `only` when given
    /// (docs/sync-spec.md 7A.2 C1, the extra push): the restriction is applied in the same
    /// query, after the same filters and in the same oldest-stamp-first order, through a
    /// temporary table (`temp.push_only`) filled and joined inside the read's own transaction
    /// and dropped after it, never a bound `IN` list (`only` may be larger than SQLite's limit
    /// on bound parameters). An id that is no longer queued is simply not found.
    ///
    /// # Errors
    /// Store errors.
    pub fn outbox_page_only(
        &self,
        limit: usize,
        meetings: &[EntityId],
        kinds: SyncKinds,
        only: Option<&std::collections::BTreeSet<EntityId>>,
    ) -> Result<OutboxPage, StoreError> {
        let Some(only) = only else {
            return self.outbox_page_in(limit, meetings, kinds, "");
        };
        let tx = self.conn.unchecked_transaction()?;
        fill_push_only(&tx, only)?;
        let page = self.outbox_page_in(limit, meetings, kinds, push_only_join());
        drop_push_only(&tx)?;
        tx.commit()?;
        page
    }

    fn outbox_page_in(
        &self,
        limit: usize,
        meetings: &[EntityId],
        kinds: SyncKinds,
        join: &str,
    ) -> Result<OutboxPage, StoreError> {
        let mut rows = Vec::new();
        let mut unreadable = 0;
        let limit_sql = i64::try_from(limit).unwrap_or(i64::MAX);
        let skip: Vec<Vec<u8>> = meetings
            .iter()
            .map(|m| m.as_uuid().as_bytes().to_vec())
            .collect();
        let marks = vec!["?"; skip.len()].join(", ");
        for table in SYNCED_TABLES {
            if !kinds.syncs(table) {
                continue;
            }
            let meeting_col = match table {
                "meeting" => Some("id"),
                "track" | "segment" | "speaker" | "note" | "summary" | "summary_request" => {
                    Some("meeting_id")
                }
                _ => None,
            };
            let except = match meeting_col {
                Some(col) if !skip.is_empty() => format!(" AND r.{col} NOT IN ({marks})"),
                _ => String::new(),
            };
            let sql = format!(
                "SELECT o.id FROM sync_outbox o JOIN {table} r ON r.id = o.id{join} WHERE o.tbl = ?{except} \
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
                // Only a row whose stored values do not convert counts as unreadable; any
                // other store error fails the page (and the tick).
                match crate::repo::synced_by_id_or_damaged(
                    &self.conn,
                    table,
                    EntityId::from_bytes(id),
                )? {
                    Ok(Some(row)) => rows.push(row),
                    Ok(None) => {}
                    Err(()) => unreadable += 1,
                }
            }
        }
        rows.sort_by_key(|r| r.meta().updated_at);
        rows.truncate(limit);
        Ok(OutboxPage { rows, unreadable })
    }

    /// Queues every synced row to be sent and drops every held row, in one transaction: for a
    /// device joining a vault (its first, or a new one after leaving another), whose earlier
    /// sends went elsewhere and whose held rows were sealed under another key.
    ///
    /// # Errors
    /// Store errors; nothing changes then.
    pub fn queue_everything<C: Clock + ?Sized>(
        &mut self,
        hlc: &mut Hlc,
        clock: &C,
    ) -> Result<u64, StoreError> {
        self.immediate(|tx| {
            let queued = tx.queue_every_row()?;
            tx.conn.execute("DELETE FROM sync_pending", [])?;
            tx.conn.execute(
                "DELETE FROM local_state WHERE key = ?1",
                params![HELD_REPLACED_KEY],
            )?;
            // Re-queued cleared rows go out after a line of their track (docs/sync-spec.md
            // 15.2), stamped with the device HLC.
            tx.send_lines_first(hlc, clock)?;
            Ok(queued)
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
    /// device shows as waiting to send (a recording's rows and switched-off kinds wait
    /// for a reason of their own and are not behind).
    ///
    /// # Errors
    /// Store errors.
    pub fn outbox_len_except(
        &self,
        meetings: &[EntityId],
        kinds: SyncKinds,
    ) -> Result<u64, StoreError> {
        let skip: Vec<Vec<u8>> = meetings
            .iter()
            .map(|m| m.as_uuid().as_bytes().to_vec())
            .collect();
        let marks = vec!["?"; skip.len()].join(", ");
        let mut total: u64 = 0;
        for table in SYNCED_TABLES {
            if !kinds.syncs(table) {
                continue;
            }
            let meeting_col = match table {
                "meeting" => Some("id"),
                "track" | "segment" | "speaker" | "note" | "summary" | "summary_request" => {
                    Some("meeting_id")
                }
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
        self.guarded_immediate(|g| outbox_sent_in(g, sent))
    }

    /// Every held row, in sequence order. Reads every sealed body: the sync client reads held
    /// rows a page at a time by reason ([`Store::held_page`]) and counts them in SQL
    /// ([`Store::held_count`]).
    ///
    /// # Errors
    /// Store errors; [`StoreError::Corrupt`] for a row this build did not write.
    pub fn held_rows(&self) -> Result<Vec<Held>, StoreError> {
        let mut st = self.conn.prepare(
            "SELECT key, seq, wall_ms, logical, node, body, reason FROM sync_pending ORDER BY seq",
        )?;
        let raw: Vec<HeldColumns> = st.query_map([], held_columns)?.collect::<Result<_, _>>()?;
        raw.into_iter().map(held_of).collect()
    }

    /// How many rows are held for one of `reasons`, counted in SQL (no row is read into
    /// memory).
    ///
    /// # Errors
    /// Store errors.
    pub fn held_count(&self, reasons: &[HoldReason]) -> Result<u64, StoreError> {
        if reasons.is_empty() {
            return Ok(0);
        }
        let marks = vec!["?"; reasons.len()].join(", ");
        let words: Vec<&'static str> = reasons.iter().map(|r| r.word()).collect();
        let n: i64 = self.conn.query_row(
            &format!("SELECT count(*) FROM sync_pending WHERE reason IN ({marks})"),
            rusqlite::params_from_iter(words.iter()),
            |r| r.get(0),
        )?;
        Ok(u64::try_from(n).unwrap_or(0))
    }

    /// Up to `limit` rows held for one of `reasons`, selected by reason in SQL, in row-key
    /// order starting after the key `after` (`None`: from the first). Row-key order is the
    /// table's own, so a caller reads every such row once by passing each page's last key,
    /// whatever is applied or held again meanwhile; it says nothing about the order the rows
    /// were pulled in ([`Held::seq`] does).
    ///
    /// # Errors
    /// Store errors; [`StoreError::Corrupt`] for a row this build did not write.
    pub fn held_page(
        &self,
        reasons: &[HoldReason],
        after: Option<&[u8; 16]>,
        limit: usize,
    ) -> Result<Vec<Held>, StoreError> {
        if reasons.is_empty() || limit == 0 {
            return Ok(Vec::new());
        }
        let marks = vec!["?"; reasons.len()].join(", ");
        let sql = format!(
            "SELECT key, seq, wall_ms, logical, node, body, reason FROM sync_pending \
             WHERE key > ? AND reason IN ({marks}) ORDER BY key LIMIT ?"
        );
        // Every row key sorts after the empty blob.
        let from: &[u8] = after.map_or(&[][..], |k| &k[..]);
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        let words: Vec<&'static str> = reasons.iter().map(|r| r.word()).collect();
        let mut values: Vec<&dyn rusqlite::ToSql> = vec![&from];
        values.extend(words.iter().map(|w| w as &dyn rusqlite::ToSql));
        values.push(&limit);
        let mut st = self.conn.prepare(&sql)?;
        let raw: Vec<HeldColumns> = st
            .query_map(values.as_slice(), held_columns)?
            .collect::<Result<_, _>>()?;
        raw.into_iter().map(held_of).collect()
    }

    /// Applies a page of pulled rows in one transaction, in order (docs/sync-spec.md
    /// section 5.3). Each row is, in this order of checks: held when this build cannot read
    /// it, skipped when its kind is switched off ([`SyncKinds`]), held when its stamp is
    /// more than [`MAX_CLOCK_AHEAD_MS`] ahead, held when its meeting is in use here, ignored
    /// when the local copy's `updated_at` is not earlier, else written whole with its own
    /// metadata (held when a row it belongs to is missing). A written row leaves no outbox
    /// entry of its own and is observed by `hlc`; a row applied or found not newer leaves
    /// `sync_pending`. A person tombstone forgets that identity's biometrics; when
    /// [`Applied::biometrics_forgotten`] is not zero the caller runs [`Store::flush_erasures`],
    /// as for any biometric deletion. A summary request is never held as busy, and its
    /// withdrawal is final (`withdrawal`, docs/sync-spec.md section 10.1). One IMMEDIATE
    /// transaction: what it reads cannot change before it writes.
    ///
    /// After the page's writes the convergence repairs run (docs/sync-spec.md section 7), each
    /// only when the page wrote a row of a kind that repair reads (T14.50; `converge` says why
    /// that loses nothing). [`Store::apply_remote_every_repair`] runs them all.
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
        self.apply_remote_with(page, rules, hlc, clock, Wrote::default(), None)
    }

    /// [`Store::apply_remote`], running every convergence repair whatever the page wrote, as
    /// every page did before T14.50. The sync client applies the first page after it starts
    /// this way, so anything a repair would mend that no page of this run caused (rows left by
    /// an earlier build, say) is mended once per start; the result for a store the repairs have
    /// already been over is the same as [`Store::apply_remote`]'s.
    ///
    /// # Errors
    /// As [`Store::apply_remote`].
    pub fn apply_remote_every_repair<C: Clock + ?Sized>(
        &mut self,
        page: &[Incoming],
        rules: &ApplyRules<'_>,
        hlc: &mut Hlc,
        clock: &C,
    ) -> Result<Applied, StoreError> {
        self.apply_remote_with(page, rules, hlc, clock, Wrote::EVERYTHING, None)
    }

    /// [`Store::apply_remote`] for the release of the two holds (docs/sync-spec.md 15.4, "The
    /// release"): run by the sync client at the end of a tick whose pull completed, after the
    /// retry, over the rows held for a parent. A row that H1 or H2 would hold is applied
    /// whatever its track then shows, unless `blocked(meeting, track)`: a row that could still
    /// give the track a live final row is held too (a live final row of that meeting and track
    /// held as busy, parent or clock), so it stays held.
    ///
    /// # Errors
    /// As [`Store::apply_remote`].
    pub fn apply_remote_releasing<C: Clock + ?Sized>(
        &mut self,
        page: &[Incoming],
        rules: &ApplyRules<'_>,
        blocked: &dyn Fn(EntityId, EntityId) -> bool,
        hlc: &mut Hlc,
        clock: &C,
    ) -> Result<Applied, StoreError> {
        self.apply_remote_with(page, rules, hlc, clock, Wrote::default(), Some(blocked))
    }

    /// `wrote` starts as the kinds whose repairs run whatever the page writes; `release` is the
    /// release's `blocked` ([`Store::apply_remote_releasing`]). One IMMEDIATE transaction whose
    /// body is the guarded root `apply_page` (docs/sync-spec.md 16.3).
    fn apply_remote_with<C: Clock + ?Sized>(
        &mut self,
        page: &[Incoming],
        rules: &ApplyRules<'_>,
        hlc: &mut Hlc,
        clock: &C,
        wrote: Wrote,
        release: Option<&dyn Fn(EntityId, EntityId) -> bool>,
    ) -> Result<Applied, StoreError> {
        self.guarded_immediate(|g| {
            crate::guarded::apply_page(g, page, rules, hlc, clock, wrote, release)
        })
    }

    /// A mark of the summary requests this store holds ([`RequestsMark`]): two aggregate reads,
    /// no row is read into memory.
    ///
    /// # Errors
    /// Store errors; [`StoreError::Corrupt`] for a stamp this build did not write.
    pub fn summary_requests_mark(&self) -> Result<RequestsMark, StoreError> {
        let rows: i64 = self
            .conn
            .query_row("SELECT count(*) FROM summary_request", [], |r| r.get(0))?;
        let latest: Option<(i64, u32, Vec<u8>)> = self
            .conn
            .query_row(
                "SELECT updated_wall, updated_logical, updated_node FROM summary_request \
                 ORDER BY updated_wall DESC, updated_logical DESC, updated_node DESC LIMIT 1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;
        let bad = || StoreError::Corrupt("a summary request's stamp".into());
        let latest = latest
            .map(
                |(wall, logical, node)| -> Result<HlcTimestamp, StoreError> {
                    Ok(HlcTimestamp {
                        wall_ms: u64::try_from(wall).map_err(|_| bad())?,
                        logical,
                        node: earshot_core::identity::DeviceId::from_bytes(
                            node.try_into().map_err(|_| bad())?,
                        ),
                    })
                },
            )
            .transpose()?;
        Ok(RequestsMark {
            rows: u64::try_from(rows).unwrap_or(0),
            latest,
        })
    }

    /// The row keys held for a parent by H1 or H2 (docs/sync-spec.md 15.4), the rows the
    /// release retries.
    ///
    /// # Errors
    /// Store errors.
    pub fn held_by_replaced_text(
        &self,
    ) -> Result<std::collections::BTreeSet<[u8; 16]>, StoreError> {
        let keys = held_replaced_keys(&self.conn)?;
        let mut out = std::collections::BTreeSet::new();
        for k in keys {
            if held_for_parent(&self.conn, &k)? {
                out.insert(k);
            }
        }
        Ok(out)
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
