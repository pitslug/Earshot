//! Repository traits and generic operations over them.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use super::entities::{
    AudioFile, ExportRecord, Meeting, Note, SegmentRow, Speaker, Summary, Track, TranscriptPass,
};
use crate::asr::Segment;
use crate::identity::{Clock, DeviceId, EntityId, Hlc};
use crate::summary::SummaryV1;
use crate::voiceprint::{VoiceprintError, VoiceprintRepo, forget_voiceprints, resolve_person};

/// Repository errors.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RepoError {
    /// A row references a parent that does not exist (or belongs to another meeting).
    MissingParent {
        /// Kind of the missing parent (`"meeting"`, `"track"`, `"speaker"`).
        entity: &'static str,
        /// Its id.
        id: EntityId,
    },
    /// The row would duplicate a value that must be unique (for example an audio file
    /// path); nothing was written.
    Conflict {
        /// Table or entity (`"audio_file"`).
        entity: &'static str,
        /// The unique field (`"path"`).
        field: &'static str,
    },
    /// Storage failure (I/O, SQL, corrupt row).
    Storage(String),
}

impl fmt::Display for RepoError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RepoError::MissingParent { entity, id } => write!(f, "missing {entity} {id}"),
            RepoError::Conflict { entity, field } => {
                write!(f, "another {entity} already has this {field}")
            }
            RepoError::Storage(m) => write!(f, "storage: {m}"),
        }
    }
}

impl std::error::Error for RepoError {}

/// Meetings and their syncable children. `put_*` inserts or replaces by id. Reads return
/// tombstoned rows too (deletions sync); list orders are part of the contract
/// ([`super::repo_contract::meeting_repo`]).
pub trait MeetingRepo {
    /// The meeting with `id`.
    ///
    /// # Errors
    /// [`RepoError::Storage`].
    fn meeting(&self, id: EntityId) -> Result<Option<Meeting>, RepoError>;
    /// All meetings, by `started_unix_ms`, then id.
    ///
    /// # Errors
    /// [`RepoError::Storage`].
    fn meetings(&self) -> Result<Vec<Meeting>, RepoError>;
    /// Inserts or replaces a meeting.
    ///
    /// # Errors
    /// [`RepoError::Storage`].
    fn put_meeting(&mut self, meeting: Meeting) -> Result<(), RepoError>;

    /// Tracks of `meeting`, by kind (mic first), then id.
    ///
    /// # Errors
    /// [`RepoError::Storage`].
    fn tracks(&self, meeting: EntityId) -> Result<Vec<Track>, RepoError>;
    /// Inserts or replaces a track.
    ///
    /// # Errors
    /// [`RepoError::MissingParent`] if its meeting does not exist.
    fn put_track(&mut self, track: Track) -> Result<(), RepoError>;

    /// Segments of `meeting` (both passes), by start, then id.
    ///
    /// # Errors
    /// [`RepoError::Storage`].
    fn segments(&self, meeting: EntityId) -> Result<Vec<SegmentRow>, RepoError>;
    /// Inserts or replaces a segment.
    ///
    /// # Errors
    /// [`RepoError::MissingParent`] if its meeting does not exist, its track is not a track
    /// of that meeting, or its speaker is set and not a speaker of that meeting.
    fn put_segment(&mut self, segment: SegmentRow) -> Result<(), RepoError>;

    /// Speakers of `meeting` in creation order: by `meta.created_at` (HLC), then id.
    /// (Id order alone is not creation order within a millisecond; see `EntityId`.)
    ///
    /// # Errors
    /// [`RepoError::Storage`].
    fn speakers(&self, meeting: EntityId) -> Result<Vec<Speaker>, RepoError>;
    /// Inserts or replaces a speaker.
    ///
    /// # Errors
    /// [`RepoError::MissingParent`] if its meeting does not exist.
    fn put_speaker(&mut self, speaker: Speaker) -> Result<(), RepoError>;

    /// Notes of `meeting`, by anchor, then id.
    ///
    /// # Errors
    /// [`RepoError::Storage`].
    fn notes(&self, meeting: EntityId) -> Result<Vec<Note>, RepoError>;
    /// Inserts or replaces a note.
    ///
    /// # Errors
    /// [`RepoError::MissingParent`] if its meeting does not exist.
    fn put_note(&mut self, note: Note) -> Result<(), RepoError>;

    /// Summaries of `meeting`, by `meta.created_at`, then id (the last live one is current).
    ///
    /// # Errors
    /// [`RepoError::Storage`].
    fn summaries(&self, meeting: EntityId) -> Result<Vec<Summary>, RepoError>;
    /// Inserts or replaces a summary.
    ///
    /// # Errors
    /// [`RepoError::MissingParent`] if its meeting does not exist.
    fn put_summary(&mut self, summary: Summary) -> Result<(), RepoError>;
}

/// Audio file rows (NeverSync; hard-deleted, no tombstones).
pub trait AudioFileRepo {
    /// Audio files of `meeting`, by id.
    ///
    /// # Errors
    /// [`RepoError::Storage`].
    fn audio_files(&self, meeting: EntityId) -> Result<Vec<AudioFile>, RepoError>;
    /// Every audio file, by `created_unix_ms`, then id (for retention).
    ///
    /// # Errors
    /// [`RepoError::Storage`].
    fn all_audio_files(&self) -> Result<Vec<AudioFile>, RepoError>;
    /// Inserts or replaces an audio file row. Call only after the file is finalised.
    ///
    /// # Errors
    /// [`RepoError::MissingParent`] if its meeting does not exist or its track is not a
    /// track of that meeting; [`RepoError::Conflict`] if another row has the same path.
    fn put_audio_file(&mut self, file: AudioFile) -> Result<(), RepoError>;
    /// Deletes the row; returns whether it existed.
    ///
    /// # Errors
    /// [`RepoError::Storage`].
    fn delete_audio_file(&mut self, id: EntityId) -> Result<bool, RepoError>;
}

/// Export records (NeverSync): one per (meeting, device).
pub trait ExportRepo {
    /// The record of `meeting` on `device`.
    ///
    /// # Errors
    /// [`RepoError::Storage`].
    fn export_record(
        &self,
        meeting: EntityId,
        device: DeviceId,
    ) -> Result<Option<ExportRecord>, RepoError>;
    /// Inserts the record, replacing any record of the same (meeting, device).
    ///
    /// # Errors
    /// [`RepoError::MissingParent`] if its meeting does not exist.
    fn put_export_record(&mut self, record: ExportRecord) -> Result<(), RepoError>;
}

/// What [`delete_meeting`] did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MeetingErasure {
    /// Rows written (tombstoned and cleared); 0 when the meeting was already erased.
    pub rows_written: usize,
    /// Persons the meeting's speakers were linked to. Persons are shared across meetings
    /// and are kept; the caller decides about their voiceprints
    /// ([`forget_unreferenced_voiceprints`]).
    pub persons: BTreeSet<EntityId>,
}

/// Deletes `meeting`: the meeting row and every syncable child (tracks, speakers, segments
/// of both passes, notes, summaries) become tombstones, so the deletion syncs, and their
/// content is cleared (title, tags, device names, speaker names and person links, segment
/// text and words, note text, summary body and model), so only ids, parent links, times and
/// the sync metadata remain. Rows that were already tombstones (an earlier final pass's
/// segments) are cleared too. Rows already erased are not written again, so a repeated call
/// writes nothing. Audio files, export records and device-local records are not syncable
/// rows: the storage adapter removes them.
///
/// # Errors
/// [`RepoError::MissingParent`] if the meeting does not exist; storage errors.
pub fn delete_meeting<R: MeetingRepo + ?Sized, C: Clock + ?Sized>(
    repo: &mut R,
    meeting: EntityId,
    hlc: &mut Hlc,
    clock: &C,
) -> Result<MeetingErasure, RepoError> {
    let Some(m) = repo.meeting(meeting)? else {
        return Err(RepoError::MissingParent {
            entity: "meeting",
            id: meeting,
        });
    };
    let mut out = MeetingErasure::default();
    // Clears a row in place; returns it, re-stamped as a tombstone, only if it changed.
    fn erased<T: Clone + PartialEq, C: Clock + ?Sized>(
        row: &T,
        meta: impl Fn(&mut T) -> &mut crate::identity::SyncMeta,
        clear: impl FnOnce(&mut T),
        hlc: &mut Hlc,
        clock: &C,
    ) -> Option<T> {
        let mut new = row.clone();
        meta(&mut new).deleted = true;
        clear(&mut new);
        if new == *row {
            return None;
        }
        meta(&mut new).mark_deleted(hlc, clock);
        Some(new)
    }
    // Children first, the meeting row last.
    for s in repo.segments(meeting)? {
        let clear = |r: &mut SegmentRow| {
            r.segment.text.clear();
            r.segment.words.clear();
            r.segment.confidence = 0.0;
            r.segment.speaker_id = None;
        };
        if let Some(new) = erased(&s, |r| &mut r.meta, clear, hlc, clock) {
            repo.put_segment(new)?;
            out.rows_written += 1;
        }
    }
    for s in repo.speakers(meeting)? {
        out.persons.extend(s.person_id);
        let clear = |r: &mut Speaker| {
            r.display_name.clear();
            r.person_id = None;
        };
        if let Some(new) = erased(&s, |r| &mut r.meta, clear, hlc, clock) {
            repo.put_speaker(new)?;
            out.rows_written += 1;
        }
    }
    for n in repo.notes(meeting)? {
        if let Some(new) = erased(&n, |r| &mut r.meta, |r| r.text.clear(), hlc, clock) {
            repo.put_note(new)?;
            out.rows_written += 1;
        }
    }
    for s in repo.summaries(meeting)? {
        let clear = |r: &mut Summary| {
            r.summary = SummaryV1::default();
            r.model = None;
        };
        if let Some(new) = erased(&s, |r| &mut r.meta, clear, hlc, clock) {
            repo.put_summary(new)?;
            out.rows_written += 1;
        }
    }
    for t in repo.tracks(meeting)? {
        if let Some(new) = erased(&t, |r| &mut r.meta, |r| r.device_name = None, hlc, clock) {
            repo.put_track(new)?;
            out.rows_written += 1;
        }
    }
    let clear = |r: &mut Meeting| {
        r.title.clear();
        r.tags.clear();
    };
    if let Some(new) = erased(&m, |r| &mut r.meta, clear, hlc, clock) {
        repo.put_meeting(new)?;
        out.rows_written += 1;
    }
    Ok(out)
}

/// After a meeting deletion: forgets (tombstones and zeroes, [`forget_voiceprints`]) the
/// voiceprints of each identity among `persons` that no live speaker of a live meeting refers
/// to any more, so a voiceprint learnt only from deleted meetings goes with them. The user's
/// own identity (`is_me`) is always kept: it is built over many meetings, enrolment is opt-in
/// and it is deleted explicitly. Persons themselves are kept. An identity is the end of a
/// person's `merged_into` chain ([`resolve_person`]); a person whose chain is broken is its
/// own identity. Returns how many voiceprints were forgotten.
///
/// A voiceprint is a running sum with no per-meeting provenance, so the contribution of one
/// meeting to a voiceprint that other meetings still use cannot be removed; it is kept.
///
/// # Errors
/// Storage errors.
pub fn forget_unreferenced_voiceprints<R, C>(
    repo: &mut R,
    persons: &BTreeSet<EntityId>,
    hlc: &mut Hlc,
    clock: &C,
) -> Result<usize, RepoError>
where
    R: MeetingRepo + VoiceprintRepo + ?Sized,
    C: Clock + ?Sized,
{
    let storage = |e: VoiceprintError| RepoError::Storage(e.to_string());
    let mut forgotten = 0;
    for id in unreferenced_identities(repo, persons)? {
        forgotten += forget_voiceprints(repo, id, hlc, clock).map_err(storage)?;
    }
    Ok(forgotten)
}

/// The identities among `persons` whose biometric data a meeting deletion forgets
/// ([`forget_unreferenced_voiceprints`], which has the rule): each identity's root id, once,
/// in `persons` order, without the user's own and those a live speaker of a live meeting
/// still refers to. A store with other biometric data per identity forgets it for exactly
/// these.
///
/// # Errors
/// Storage errors.
pub fn unreferenced_identities<R>(
    repo: &R,
    persons: &BTreeSet<EntityId>,
) -> Result<Vec<EntityId>, RepoError>
where
    R: MeetingRepo + VoiceprintRepo + ?Sized,
{
    let storage = |e: VoiceprintError| RepoError::Storage(e.to_string());
    if persons.is_empty() {
        return Ok(Vec::new());
    }
    let identity = |repo: &R, p: EntityId| -> Result<(EntityId, bool), RepoError> {
        let own = repo.person(p).map_err(storage)?;
        let root = resolve_person(repo, p).map_err(storage)?;
        let is_me = own.is_some_and(|x| x.is_me) || root.as_ref().is_some_and(|r| r.is_me);
        Ok((root.map_or(p, |r| r.id), is_me))
    };
    let mut referenced = BTreeSet::new();
    for m in repo.meetings()? {
        if m.meta.deleted {
            continue;
        }
        for s in repo.speakers(m.id)? {
            if let (false, Some(p)) = (s.meta.deleted, s.person_id) {
                referenced.insert(identity(repo, p)?.0);
            }
        }
    }
    let mut out = Vec::new();
    for &p in persons {
        let (id, is_me) = identity(repo, p)?;
        if is_me || referenced.contains(&id) || out.contains(&id) {
            continue;
        }
        out.push(id);
    }
    Ok(out)
}

/// Live (not tombstoned) segments of `meeting` from `pass`, by start, then id.
///
/// # Errors
/// [`RepoError::Storage`].
pub fn live_segments<R: MeetingRepo + ?Sized>(
    repo: &R,
    meeting: EntityId,
    pass: TranscriptPass,
) -> Result<Vec<SegmentRow>, RepoError> {
    Ok(repo
        .segments(meeting)?
        .into_iter()
        .filter(|r| r.pass == pass && !r.meta.deleted)
        .collect())
}

/// Replaces the `pass` transcript of `meeting` with `segments` (final pass at stop, or a
/// re-transcription): the previous live rows of that pass become tombstones, the new ones
/// are inserted. Returns how many rows were tombstoned. Every segment's track is checked
/// before anything is written.
///
/// # Errors
/// [`RepoError::MissingParent`] if the meeting is missing or a segment's track is not one
/// of its tracks (nothing is written then); storage errors.
pub fn replace_segments<R: MeetingRepo + ?Sized, C: Clock + ?Sized>(
    repo: &mut R,
    meeting: EntityId,
    pass: TranscriptPass,
    segments: Vec<Segment>,
    hlc: &mut Hlc,
    clock: &C,
) -> Result<usize, RepoError> {
    if repo.meeting(meeting)?.is_none() {
        return Err(RepoError::MissingParent {
            entity: "meeting",
            id: meeting,
        });
    }
    let tracks: BTreeSet<EntityId> = repo.tracks(meeting)?.iter().map(|t| t.id).collect();
    if let Some(s) = segments.iter().find(|s| !tracks.contains(&s.track)) {
        return Err(RepoError::MissingParent {
            entity: "track",
            id: s.track,
        });
    }
    let new_ids: BTreeSet<EntityId> = segments.iter().map(|s| s.id).collect();
    let existing: BTreeMap<EntityId, SegmentRow> = repo
        .segments(meeting)?
        .into_iter()
        .map(|r| (r.segment.id, r))
        .collect();
    let mut n = 0;
    for old in existing.values() {
        if old.pass != pass || old.meta.deleted || new_ids.contains(&old.segment.id) {
            continue;
        }
        let mut old = old.clone();
        old.meta.mark_deleted(hlc, clock);
        repo.put_segment(old)?;
        n += 1;
    }
    for s in segments {
        let row = match existing.get(&s.id) {
            // Same id again: keep its creation time, record the change.
            Some(prev) => {
                let mut meta = prev.meta;
                meta.deleted = false;
                meta.touch(hlc, clock);
                SegmentRow {
                    meta,
                    meeting_id: meeting,
                    pass,
                    segment: s,
                }
            }
            None => SegmentRow::new(meeting, pass, s, hlc, clock),
        };
        repo.put_segment(row)?;
    }
    Ok(n)
}
