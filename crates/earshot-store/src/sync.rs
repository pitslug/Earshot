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
    DictionaryWord, Folder, Meeting, Note, RepoError, RequestState, SegmentRow, Speaker, Summary,
    SummaryRequest, Track,
};
use earshot_core::voiceprint::{Person, Voiceprint, VoiceprintSample};
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
pub type AfterApply<'a> = &'a dyn Fn(&mut StoreTx<'_>) -> Result<(), StoreError>;

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
    /// The meetings a row written here belongs to (a meeting's own row, its tracks, lines,
    /// speakers, notes, summaries and summary requests), including a summary the convergence
    /// rules replaced: ids only, so the window can read again what changed (T12.81). A row
    /// held, ignored or skipped names nothing.
    pub meetings: std::collections::BTreeSet<EntityId>,
    /// Whether a person row was written here or "Me" people merged: names shown in any meeting
    /// resolve through persons, so the window reads the open meeting again (T12.81).
    pub people: bool,
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

/// What [`Store::apply_remote`] does with a pulled summary request over the copy here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Withdrawal {
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
fn withdrawal(
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

/// Whether the person `id`, followed through `merged_into`, ends at a tombstoned person
/// (`Some(true)`), a live one (`Some(false)`), or at nobody known here or a loop (`None`).
fn ends_deleted(tx: &StoreTx<'_>, id: EntityId) -> Result<Option<bool>, StoreError> {
    let mut seen = Vec::new();
    let mut cur = id;
    loop {
        if seen.contains(&cur) {
            return Ok(None);
        }
        seen.push(cur);
        let Some(SyncedRow::Person(p)) = crate::repo::synced_by_id(&tx.conn, "person", cur)? else {
            return Ok(None);
        };
        if p.meta.deleted {
            return Ok(Some(true));
        }
        match p.merged_into {
            Some(next) => cur = next,
            None => return Ok(Some(false)),
        }
    }
}

/// Rule 3a of [`converge`]: live prints and samples of a person tombstoned here are erased,
/// prints zeroed and tombstoned, samples by id in SQL (a damaged row is erased too).
fn dead_person_repair<C: Clock + ?Sized>(
    tx: &StoreTx<'_>,
    hlc: &mut Hlc,
    clock: &C,
    out: &mut Applied,
) -> Result<(), StoreError> {
    // (sample id, person id, meeting id as stored).
    type SampleRow = ([u8; 16], [u8; 16], Option<Vec<u8>>);
    let samples: Vec<SampleRow> = {
        let mut st = tx.conn.prepare(
            "SELECT id, person_id, meeting_id FROM voiceprint_sample \
             WHERE deleted = 0 AND person_id IS NOT NULL",
        )?;
        st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
            .collect::<Result<_, _>>()?
    };
    for (id, person, meeting) in samples {
        if ends_deleted(tx, EntityId::from_bytes(person))? == Some(true) {
            erase_sample_by_id(tx, hlc, clock, id)?;
            erased_sample(out, meeting.as_deref());
        }
    }
    let prints: Vec<([u8; 16], [u8; 16])> = {
        let mut st = tx
            .conn
            .prepare("SELECT id, person_id FROM voiceprint WHERE deleted = 0")?;
        st.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<Result<_, _>>()?
    };
    for (id, person) in prints {
        if ends_deleted(tx, EntityId::from_bytes(person))? != Some(true) {
            continue;
        }
        let Some(SyncedRow::Voiceprint(v)) =
            crate::repo::synced_by_id(&tx.conn, "voiceprint", EntityId::from_bytes(id))?
        else {
            continue;
        };
        let mut meta = *v.meta();
        meta.mark_deleted(hlc, clock);
        let gone = Voiceprint::from_stored(
            v.id(),
            meta,
            v.person_id(),
            v.model().clone(),
            vec![0.0; v.base_sum().len()],
            v.base_count(),
            v.base_speech_ms(),
        );
        crate::repo::put_synced(&tx.conn, &SyncedRow::Voiceprint(gone))?;
        out.biometrics_forgotten += 1;
        out.people = true;
    }
    Ok(())
}

/// Erases the voice sample `id` in place by SQL (body cleared, vector empty, a new stamp), so a
/// row this build cannot read is erased too.
fn erase_sample_by_id<C: Clock + ?Sized>(
    tx: &StoreTx<'_>,
    hlc: &mut Hlc,
    clock: &C,
    id: [u8; 16],
) -> Result<(), StoreError> {
    let at = hlc.now(clock);
    tx.conn.execute(
        "UPDATE voiceprint_sample SET deleted = 1, voiceprint_id = NULL, person_id = NULL, \
         model = NULL, vector = x'', speech_ms = NULL, meeting_id = NULL, speaker_id = NULL, \
         updated_wall = ?1, updated_logical = ?2, updated_node = ?3 \
         WHERE id = ?4 AND deleted = 0",
        params![
            i64_of(at.wall_ms)?,
            at.logical,
            &at.node.as_uuid().as_bytes()[..],
            &id[..],
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
            out.people = true;
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
            out.meetings.insert(replaced.meeting_id);
        }
    }
    // Rule 3a: no live voiceprint or voice sample of a person tombstoned here, directly or at
    // the end of a `merged_into` chain (docs/voice-samples.md 7.4). A person not known here,
    // or a chain that loops, leaves the row alone.
    dead_person_repair(tx, hlc, clock, out)?;
    // Rule 3: no live voice sample of a print tombstoned here (docs/voice-samples.md 7.4).
    let dead: Vec<([u8; 16], Option<Vec<u8>>)> = {
        let mut st = tx.conn.prepare(
            "SELECT s.id, s.meeting_id FROM voiceprint_sample s \
             JOIN voiceprint v ON v.id = s.voiceprint_id WHERE s.deleted = 0 AND v.deleted = 1",
        )?;
        st.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<Result<_, _>>()?
    };
    // Erased by id in SQL, never read back first: a damaged live row (one `from_stored`
    // refuses) is erased too and cannot abort the page.
    for (id, meeting) in dead {
        erase_sample_by_id(tx, hlc, clock, id)?;
        erased_sample(out, meeting.as_deref());
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
/// Every device writes the same content, so the copies converge whichever stamp wins.
fn edits_win<C: Clock + ?Sized>(
    tx: &StoreTx<'_>,
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
        if leased(m)
            || !crate::repo::synced_by_id(&tx.conn, "meeting", m)?
                .is_some_and(|r| !r.meta().deleted)
        {
            continue;
        }
        let rows = crate::repo::segments_of(&tx.conn, m)?;
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
                Some(segment) => new.segment = segment,
                None => new.meta.deleted = true,
            }
            new.meta.touch(hlc, clock);
            crate::repo::put_synced(&tx.conn, &SyncedRow::Segment(new))?;
            out.words_dropped_under_edits += dropped;
            out.lines_rewritten_under_edits += 1;
        }
        let named: BTreeSet<EntityId> =
            edited.iter().filter_map(|e| e.segment.speaker_id).collect();
        let mut speakers = crate::repo::speakers_of(&tx.conn, m)?;
        speakers.sort_by_key(|s| s.id);
        let mut me_live = speakers
            .iter()
            .any(|s| !s.meta.deleted && s.role == SpeakerRole::Me);
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
                back.meta.touch(hlc, clock);
                crate::repo::put_synced(&tx.conn, &SyncedRow::Speaker(back))?;
                out.speakers_revived += 1;
            }
        }
    }
    Ok(())
}

/// Counts a sample a repair erased and names its meeting (read in the repair's own query), so
/// a window showing that meeting reloads.
fn erased_sample(out: &mut Applied, meeting: Option<&[u8]>) {
    out.biometrics_forgotten += 1;
    out.people = true;
    if let Some(m) = meeting.and_then(|b| <[u8; 16]>::try_from(b).ok()) {
        out.meetings.insert(EntityId::from_bytes(m));
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
    pub fn queue_everything(&mut self) -> Result<u64, StoreError> {
        self.immediate(|tx| {
            let queued = tx.queue_every_row()?;
            tx.conn.execute("DELETE FROM sync_pending", [])?;
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
                    // A kind switched off is skipped before anything is held, whether or not
                    // its record decodes: a device that never syncs it keeps nothing of it.
                    if inc.entity.as_deref().is_some_and(|e| !rules.kinds.syncs(e)) {
                        unhold(tx, inc)?;
                        out.skipped += 1;
                        continue;
                    }
                    hold(tx, inc, HoldReason::Format)?;
                    out.hold(HoldReason::Format);
                    continue;
                };
                if !rules.kinds.syncs(row.table()) {
                    unhold(tx, inc)?;
                    out.skipped += 1;
                    continue;
                }
                if inc.stamp.wall_ms > rules.now_ms.saturating_add(MAX_CLOCK_AHEAD_MS) {
                    hold(tx, inc, HoldReason::Clock)?;
                    out.hold(HoldReason::Clock);
                    continue;
                }
                if row.waits_while_busy() && row.meeting().is_some_and(|m| (rules.leased)(m)) {
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
                    // A local meeting row that does not read is judged by its `deleted` column
                    // (T14.26).
                    && match crate::repo::synced_by_id_or_damaged(&tx.conn, "meeting", m)? {
                        Ok(l) => l.is_some_and(|l| l.meta().deleted),
                        Err(()) => crate::repo::row_reads_deleted(&tx.conn, "meeting", m)?,
                    }
                {
                    unhold(tx, inc)?;
                    out.deletions_kept += 1;
                    continue;
                }
                // A local row that does not read (T14.26) is replaced by the pulled one, as an
                // absent row is, except where the readable path below keeps the local state
                // final against the pulled copy: a meeting or voiceprint tombstone, an erased
                // voice sample, and this device's withdrawal of its own summary request against
                // an open one. Those are judged from the columns that still read and the pulled
                // row is ignored (a damaged row cannot be stamped again or sent).
                let local = match crate::repo::synced_by_id_or_damaged(
                    &tx.conn,
                    row.table(),
                    row.id(),
                )? {
                    Ok(local) => local,
                    Err(()) => {
                        let stays_final = match row {
                            SyncedRow::Meeting(_) | SyncedRow::Voiceprint(_) => {
                                !row.meta().deleted
                                    && crate::repo::row_reads_deleted(
                                        &tx.conn,
                                        row.table(),
                                        row.id(),
                                    )?
                            }
                            SyncedRow::VoiceprintSample(there) => {
                                !there.is_erased()
                                    && crate::repo::row_reads_deleted(
                                        &tx.conn,
                                        row.table(),
                                        row.id(),
                                    )?
                            }
                            SyncedRow::SummaryRequest(there) => {
                                there.state.is_open()
                                    && crate::repo::request_reads_withdrawn_by(
                                        &tx.conn,
                                        row.id(),
                                        hlc.node(),
                                    )?
                            }
                            _ => false,
                        };
                        if stays_final {
                            unhold(tx, inc)?;
                            out.ignored += 1;
                            continue;
                        }
                        None
                    }
                };
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
                // A voiceprint deletion is final, as a meeting's (docs/voice-samples.md 7.4): a
                // live copy written elsewhere after it (an older build that still had the print)
                // never brings the voice back. The tombstone is stamped again and goes out.
                if let (Some(SyncedRow::Voiceprint(here)), SyncedRow::Voiceprint(there)) =
                    (&local, row)
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
                        crate::repo::put_synced(&tx.conn, &SyncedRow::Voiceprint(again))?;
                        out.deletions_kept += 1;
                    } else {
                        out.ignored += 1;
                    }
                    unhold(tx, inc)?;
                    continue;
                }
                // A voice sample's erasure is final too: a live copy never brings an erased
                // voice back (docs/voice-samples.md 7.4).
                if let (
                    Some(SyncedRow::VoiceprintSample(here)),
                    SyncedRow::VoiceprintSample(there),
                ) = (&local, row)
                    && here.is_erased()
                    && !there.is_erased()
                {
                    let _ = hlc.observe(inc.stamp, clock);
                    if here.meta().updated_at < row.meta().updated_at {
                        let mut meta = *here.meta();
                        meta.touch(hlc, clock);
                        let again = VoiceprintSample::erased(here.id(), meta)
                            .map_err(|e| StoreError::Repo(RepoError::Storage(e.to_string())))?;
                        crate::repo::put_synced(&tx.conn, &SyncedRow::VoiceprintSample(again))?;
                        out.deletions_kept += 1;
                    } else {
                        out.ignored += 1;
                    }
                    unhold(tx, inc)?;
                    continue;
                }
                // A summary request's withdrawal is final (docs/sync-spec.md section 10.1).
                if let (Some(SyncedRow::SummaryRequest(here)), SyncedRow::SummaryRequest(there)) =
                    (&local, row)
                {
                    match withdrawal(here, there, hlc.node()) {
                        Withdrawal::Lww => {}
                        Withdrawal::TakeIt => {
                            // Written as it came: only the asker writes a withdrawal, so this
                            // device never stamps one of its own here.
                            crate::repo::put_synced(&tx.conn, row)?;
                            tx.conn.execute(
                                "DELETE FROM sync_outbox WHERE tbl = ?1 AND id = ?2",
                                params![row.table(), &row.id().as_uuid().as_bytes()[..]],
                            )?;
                            unhold(tx, inc)?;
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
                                crate::repo::put_synced(
                                    &tx.conn,
                                    &SyncedRow::SummaryRequest(again),
                                )?;
                                out.withdrawals_kept += 1;
                            } else {
                                out.ignored += 1;
                            }
                            unhold(tx, inc)?;
                            continue;
                        }
                    }
                }
                // Rule 4a (docs/sync-spec.md section 7, T12.92): two copies of one final line
                // are compared by the user's edit first, so a re-transcription that did not see
                // an edit never replaces it, whatever the stamps. Only while the meeting is live
                // here. The relay keeps the later `updated_at` alone, so a copy kept here that
                // is stamped below the losing one is stamped again and goes out.
                let mut restamp_written = false;
                if let (Some(SyncedRow::Segment(here)), SyncedRow::Segment(there)) = (&local, row)
                    && let Some(order) = SegmentRow::edit_order(here, there)
                    && crate::repo::synced_by_id(&tx.conn, "meeting", there.meeting_id)?
                        .is_some_and(|m| !m.meta().deleted)
                {
                    if order != std::cmp::Ordering::Greater {
                        let _ = hlc.observe(inc.stamp, clock);
                        if here.meta.updated_at < there.meta.updated_at {
                            let mut again = here.clone();
                            again.meta.touch(hlc, clock);
                            crate::repo::put_synced(&tx.conn, &SyncedRow::Segment(again))?;
                            out.edited_lines_kept += 1;
                            out.meetings.insert(here.meeting_id);
                        } else {
                            out.ignored += 1;
                        }
                        unhold(tx, inc)?;
                        continue;
                    }
                    restamp_written = here.meta.updated_at > there.meta.updated_at;
                }
                if !restamp_written
                    && local
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
                let print_was_live =
                    matches!(&local, Some(SyncedRow::Voiceprint(v)) if !v.meta().deleted);
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
                // Rule 4a: an edit that won over a later-stamped copy here is stamped again
                // (after the incoming stamp was observed), so it keeps its outbox entry and goes
                // out to win at the relay.
                if restamp_written && let SyncedRow::Segment(won) = row {
                    let mut again = won.clone();
                    again.meta.touch(hlc, clock);
                    crate::repo::put_synced(&tx.conn, &SyncedRow::Segment(again))?;
                }
                out.written += 1;
                if local.is_none() && row.has_children() {
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
                        && let Some(earshot_core::voiceprint::SampleOrigin::Meeting {
                            meeting,
                            ..
                        }) = s.origin()
                    {
                        out.meetings.insert(meeting);
                    }
                }
                if matches!(row, SyncedRow::Voiceprint(v) if v.meta().deleted) && print_was_live {
                    out.biometrics_forgotten += 1;
                }
                if matches!(row, SyncedRow::VoiceprintSample(s) if s.is_erased()) && sample_was_live
                {
                    out.biometrics_forgotten += 1;
                }
                if let SyncedRow::Person(p) = row
                    && p.meta.deleted
                    && person_was_live
                {
                    out.biometrics_forgotten +=
                        crate::identities::forget_identity_biometrics(tx, p.id, hlc, clock)?
                            .total();
                }
            }
            converge(tx, hlc, clock, &mut out)?;
            edits_win(tx, hlc, clock, rules.leased, &mut out)?;
            if let Some(after) = rules.after {
                after(tx)?;
            }
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
