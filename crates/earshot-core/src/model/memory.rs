//! In-memory repository (tests, and the pipeline before storage exists).

use std::collections::BTreeMap;

use super::entities::{
    AudioFile, ExportRecord, Folder, Meeting, Note, SegmentRow, Speaker, Summary, Track,
};
use super::repo::{AudioFileRepo, ExportRepo, FolderRepo, MeetingRepo, RepoError};
use crate::identity::{DeviceId, EntityId};

/// In-memory implementation of [`MeetingRepo`], [`FolderRepo`], [`AudioFileRepo`] and
/// [`ExportRepo`] with the same parent checks and orders as the SQLite store.
#[derive(Debug, Clone, Default)]
pub struct InMemoryRepo {
    meetings: BTreeMap<EntityId, Meeting>,
    tracks: BTreeMap<EntityId, Track>,
    segments: BTreeMap<EntityId, SegmentRow>,
    speakers: BTreeMap<EntityId, Speaker>,
    notes: BTreeMap<EntityId, Note>,
    summaries: BTreeMap<EntityId, Summary>,
    audio: BTreeMap<EntityId, AudioFile>,
    exports: BTreeMap<(EntityId, DeviceId), ExportRecord>,
    folders: BTreeMap<EntityId, Folder>,
}

impl InMemoryRepo {
    /// An empty repository.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    fn need_meeting(&self, id: EntityId) -> Result<(), RepoError> {
        if self.meetings.contains_key(&id) {
            Ok(())
        } else {
            Err(RepoError::MissingParent {
                entity: "meeting",
                id,
            })
        }
    }

    fn need_track(&self, meeting: EntityId, track: EntityId) -> Result<(), RepoError> {
        match self.tracks.get(&track) {
            Some(t) if t.meeting_id == meeting => Ok(()),
            _ => Err(RepoError::MissingParent {
                entity: "track",
                id: track,
            }),
        }
    }
}

fn of_meeting<'a, T: Clone + 'a>(
    rows: impl Iterator<Item = &'a T>,
    meeting: EntityId,
    meeting_of: impl Fn(&T) -> EntityId,
) -> Vec<T> {
    rows.filter(|r| meeting_of(r) == meeting).cloned().collect()
}

impl MeetingRepo for InMemoryRepo {
    fn meeting(&self, id: EntityId) -> Result<Option<Meeting>, RepoError> {
        Ok(self.meetings.get(&id).cloned())
    }

    fn meetings(&self) -> Result<Vec<Meeting>, RepoError> {
        let mut v: Vec<Meeting> = self.meetings.values().cloned().collect();
        v.sort_by_key(|m| (m.started_unix_ms, m.id));
        Ok(v)
    }

    fn put_meeting(&mut self, meeting: Meeting) -> Result<(), RepoError> {
        self.meetings.insert(meeting.id, meeting);
        Ok(())
    }

    fn tracks(&self, meeting: EntityId) -> Result<Vec<Track>, RepoError> {
        let mut v = of_meeting(self.tracks.values(), meeting, |t| t.meeting_id);
        v.sort_by_key(|t| (t.kind, t.id));
        Ok(v)
    }

    fn put_track(&mut self, track: Track) -> Result<(), RepoError> {
        self.need_meeting(track.meeting_id)?;
        self.tracks.insert(track.id, track);
        Ok(())
    }

    fn segments(&self, meeting: EntityId) -> Result<Vec<SegmentRow>, RepoError> {
        let mut v = of_meeting(self.segments.values(), meeting, |s| s.meeting_id);
        v.sort_by_key(|s| (s.segment.start, s.segment.id));
        Ok(v)
    }

    fn put_segment(&mut self, row: SegmentRow) -> Result<(), RepoError> {
        self.need_meeting(row.meeting_id)?;
        self.need_track(row.meeting_id, row.segment.track)?;
        if let Some(sp) = row.segment.speaker_id {
            match self.speakers.get(&sp) {
                Some(s) if s.meeting_id == row.meeting_id => {}
                _ => {
                    return Err(RepoError::MissingParent {
                        entity: "speaker",
                        id: sp,
                    });
                }
            }
        }
        self.segments.insert(row.segment.id, row);
        Ok(())
    }

    fn speakers(&self, meeting: EntityId) -> Result<Vec<Speaker>, RepoError> {
        let mut v = of_meeting(self.speakers.values(), meeting, |s| s.meeting_id);
        v.sort_by_key(|s| (s.meta.created_at, s.id));
        Ok(v)
    }

    fn put_speaker(&mut self, speaker: Speaker) -> Result<(), RepoError> {
        self.need_meeting(speaker.meeting_id)?;
        self.speakers.insert(speaker.id, speaker);
        Ok(())
    }

    fn notes(&self, meeting: EntityId) -> Result<Vec<Note>, RepoError> {
        let mut v = of_meeting(self.notes.values(), meeting, |n| n.meeting_id);
        v.sort_by_key(|n| (n.at, n.id));
        Ok(v)
    }

    fn put_note(&mut self, note: Note) -> Result<(), RepoError> {
        self.need_meeting(note.meeting_id)?;
        self.notes.insert(note.id, note);
        Ok(())
    }

    fn summaries(&self, meeting: EntityId) -> Result<Vec<Summary>, RepoError> {
        let mut v = of_meeting(self.summaries.values(), meeting, |s| s.meeting_id);
        v.sort_by_key(|s| (s.meta.created_at, s.id));
        Ok(v)
    }

    fn put_summary(&mut self, summary: Summary) -> Result<(), RepoError> {
        self.need_meeting(summary.meeting_id)?;
        self.summaries.insert(summary.id, summary);
        Ok(())
    }
}

impl AudioFileRepo for InMemoryRepo {
    fn audio_files(&self, meeting: EntityId) -> Result<Vec<AudioFile>, RepoError> {
        Ok(of_meeting(self.audio.values(), meeting, |f| f.meeting_id))
    }

    fn all_audio_files(&self) -> Result<Vec<AudioFile>, RepoError> {
        let mut v: Vec<AudioFile> = self.audio.values().cloned().collect();
        v.sort_by_key(|f| (f.created_unix_ms, f.id));
        Ok(v)
    }

    fn put_audio_file(&mut self, file: AudioFile) -> Result<(), RepoError> {
        self.need_meeting(file.meeting_id)?;
        self.need_track(file.meeting_id, file.track_id)?;
        if self
            .audio
            .values()
            .any(|f| f.path == file.path && f.id != file.id)
        {
            return Err(RepoError::Conflict {
                entity: "audio_file",
                field: "path",
            });
        }
        self.audio.insert(file.id, file);
        Ok(())
    }

    fn delete_audio_file(&mut self, id: EntityId) -> Result<bool, RepoError> {
        Ok(self.audio.remove(&id).is_some())
    }
}

impl ExportRepo for InMemoryRepo {
    fn export_record(
        &self,
        meeting: EntityId,
        device: DeviceId,
    ) -> Result<Option<ExportRecord>, RepoError> {
        Ok(self.exports.get(&(meeting, device)).cloned())
    }

    fn put_export_record(&mut self, record: ExportRecord) -> Result<(), RepoError> {
        self.need_meeting(record.meeting_id)?;
        self.exports
            .insert((record.meeting_id, record.device_id), record);
        Ok(())
    }
}

impl FolderRepo for InMemoryRepo {
    fn folder(&self, id: EntityId) -> Result<Option<Folder>, RepoError> {
        Ok(self.folders.get(&id).cloned())
    }

    fn folders(&self) -> Result<Vec<Folder>, RepoError> {
        let mut v: Vec<Folder> = self.folders.values().cloned().collect();
        v.sort_by_key(|f| (f.meta.created_at, f.id));
        Ok(v)
    }

    fn put_folder(&mut self, folder: Folder) -> Result<(), RepoError> {
        self.folders.insert(folder.id, folder);
        Ok(())
    }
}
