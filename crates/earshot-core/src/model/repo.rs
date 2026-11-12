//! Repository traits and generic operations over them.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use super::entities::{
    AudioFile, ExportRecord, Folder, Meeting, Note, SegmentRow, Speaker, Summary, Track,
    TranscriptPass,
};
use crate::asr::Segment;
use crate::identity::{Clock, DeviceId, EntityId, Hlc};
use crate::summary::SummaryV1;

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
    /// Whether `meeting` has a live (not tombstoned) segment of `pass`: the answer of
    /// `!live_segments(repo, meeting, pass).is_empty()`. A repository may answer it without
    /// reading any segment (the SQLite store asks for one row, T9.24a).
    ///
    /// # Errors
    /// [`RepoError::Storage`].
    fn has_live_segment(&self, meeting: EntityId, pass: TranscriptPass) -> Result<bool, RepoError> {
        Ok(self
            .segments(meeting)?
            .iter()
            .any(|r| r.pass == pass && !r.meta.deleted))
    }
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

/// Folders (syncable). `put_folder` inserts or replaces by id; reads return tombstoned rows
/// too (deletions sync). No name is unique here: tombstones all carry the cleared name and
/// sync may deliver two live folders with one name ([`super::repo_contract::folder_repo`]).
pub trait FolderRepo {
    /// The folder with `id`.
    ///
    /// # Errors
    /// [`RepoError::Storage`].
    fn folder(&self, id: EntityId) -> Result<Option<Folder>, RepoError>;
    /// Every folder, tombstones included, in creation order (`meta.created_at`, then id).
    /// Display order (by name) is the caller's.
    ///
    /// # Errors
    /// [`RepoError::Storage`].
    fn folders(&self) -> Result<Vec<Folder>, RepoError>;
    /// Inserts or replaces a folder by id.
    ///
    /// # Errors
    /// [`RepoError::Storage`].
    fn put_folder(&mut self, folder: Folder) -> Result<(), RepoError>;
}

/// The erasure rule of [`delete_meeting`] and [`delete_folder`]: clears a copy of `row`
/// (marked deleted, then `clear`) and returns it re-stamped as a tombstone, or `None` when
/// that changes nothing (already erased), so a repeated deletion writes nothing.
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

/// Deletes `folder`: the row becomes a tombstone (so the deletion syncs) with its name
/// cleared. Returns whether anything was written; an already erased folder is not written
/// again. Meetings filed in it are not touched here: the caller clears their
/// [`Meeting::folder_id`] in the same transaction (T9.5b), and a meeting still naming a
/// tombstoned folder reads as no folder.
///
/// # Errors
/// [`RepoError::MissingParent`] (`"folder"`) if the folder does not exist; storage errors.
pub fn delete_folder<R: FolderRepo + ?Sized, C: Clock + ?Sized>(
    repo: &mut R,
    folder: EntityId,
    hlc: &mut Hlc,
    clock: &C,
) -> Result<bool, RepoError> {
    let Some(old) = repo.folder(folder)? else {
        return Err(RepoError::MissingParent {
            entity: "folder",
            id: folder,
        });
    };
    let Some(new) = erased(&old, |r| &mut r.meta, |r| r.name.clear(), hlc, clock) else {
        return Ok(false);
    };
    repo.put_folder(new)?;
    Ok(true)
}

/// What [`delete_meeting`] did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MeetingErasure {
    /// Rows written (tombstoned and cleared); 0 when the meeting was already erased.
    pub rows_written: usize,
}

/// Deletes `meeting`: the meeting row and every syncable child (tracks, speakers, segments
/// of both passes, notes, summaries) become tombstones, so the deletion syncs, and their
/// content is cleared (title, tags, folder, device names, speaker names and person links, segment
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
        r.folder_id = None;
    };
    if let Some(new) = erased(&m, |r| &mut r.meta, clear, hlc, clock) {
        repo.put_meeting(new)?;
        out.rows_written += 1;
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
                    // New recogniser text: no edit of the user's applies to it.
                    text_edited_at: None,
                }
            }
            None => SegmentRow::new(meeting, pass, s, hlc, clock),
        };
        repo.put_segment(row)?;
    }
    Ok(n)
}
