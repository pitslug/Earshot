//! Behaviour every repository implementation must have, as plain functions that panic on a
//! violation. [`super::InMemoryRepo`]'s tests and `earshot-store`'s SQLite tests both run
//! them, so the two implementations cannot drift apart.
//!
//! Each function takes a constructor of empty repositories.

use crate::asr::{MeetingTime, Segment, Word};
use crate::capture::TrackId;
use crate::diar::SpeakerRole;
use crate::identity::{DeviceId, EntityId, Hlc, ManualClock, SyncMeta};
use crate::prompt::Template;
use crate::summary::{ActionItem, Decision, NoteBullet, NoteSection, SummaryV1};

use super::entities::{
    AudioFile, AudioFormat, ClockDomain, ExportRecord, Meeting, Note, SegmentRow, Speaker, Summary,
    Track, TranscriptPass, VaultPath,
};
use super::repo::{AudioFileRepo, ExportRepo, MeetingRepo, RepoError};

/// Deterministic row factory (ids from a counter, metadata from a manual clock).
struct Rows {
    clock: ManualClock,
    hlc: Hlc,
    next: u128,
}

impl Rows {
    fn new() -> Self {
        Rows {
            clock: ManualClock::new(1_800_000_000_000),
            hlc: Hlc::new(DeviceId::from_bytes([0xd1; 16])),
            next: 1,
        }
    }

    fn id(&mut self) -> EntityId {
        self.next += 1;
        // Version-7 / variant bits set so the value is a valid UUIDv7 shape.
        let mut b = (self.next | (0x0190_0000_0000_7000_8000_0000_0000_0000)).to_be_bytes();
        b[6] = 0x70 | (b[6] & 0x0f);
        b[8] = 0x80 | (b[8] & 0x3f);
        EntityId::from_bytes(b)
    }

    fn meta(&mut self) -> SyncMeta {
        self.clock.advance_ms(1);
        SyncMeta::new(&mut self.hlc, &self.clock)
    }

    fn meeting(&mut self, started: u64) -> Meeting {
        Meeting {
            id: self.id(),
            meta: self.meta(),
            title: "Weekly sync".into(),
            started_unix_ms: started,
            utc_offset_minutes: -300,
            duration_ms: Some(3_600_000),
            tags: vec!["work".into(), "sync".into()],
            template: Template::OneOnOne,
        }
    }

    fn track(&mut self, meeting: EntityId, kind: TrackId) -> Track {
        Track {
            id: self.id(),
            meta: self.meta(),
            meeting_id: meeting,
            kind,
            device_name: Some("Headset (USB)".into()),
            clock_domain: ClockDomain::Qpc,
        }
    }

    fn segment(&mut self, meeting: EntityId, track: EntityId, start: u64) -> SegmentRow {
        let seg = Segment::from_words(
            self.id(),
            track,
            vec![
                Word::new("Hello,", start, start + 310, 0.912_345_7),
                Word::new("world.", start + 320, start + 700, 0.1),
            ],
            MeetingTime::ZERO,
        );
        SegmentRow {
            meta: self.meta(),
            meeting_id: meeting,
            pass: TranscriptPass::Live,
            segment: seg,
        }
    }

    fn speaker(&mut self, meeting: EntityId) -> Speaker {
        Speaker {
            id: self.id(),
            meta: self.meta(),
            meeting_id: meeting,
            role: SpeakerRole::Remote,
            display_name: "Speaker 2".into(),
            person_id: Some(self.id()),
        }
    }

    fn note(&mut self, meeting: EntityId, at: u64) -> Note {
        Note {
            id: self.id(),
            meta: self.meta(),
            meeting_id: meeting,
            at: MeetingTime::from_millis(at),
            text: "line one\nline two".into(),
        }
    }

    fn summary(&mut self, meeting: EntityId, cite: EntityId) -> Summary {
        Summary {
            id: self.id(),
            meta: self.meta(),
            meeting_id: meeting,
            schema_version: 1,
            template: Template::Standup,
            model: Some("local-model".into()),
            summary: SummaryV1 {
                title: "Plan".into(),
                tldr: "We planned.".into(),
                enhanced_notes: vec![NoteSection {
                    heading: "Plan".into(),
                    bullets: vec![NoteBullet {
                        text: "Ship it".into(),
                        source_segment_ids: vec![cite],
                    }],
                }],
                decisions: vec![Decision {
                    text: "Ship Friday".into(),
                    source_segment_ids: vec![cite],
                }],
                action_items: vec![ActionItem {
                    owner: "Me".into(),
                    task: "Write notes".into(),
                    due: None,
                    source_segment_ids: vec![cite],
                }],
                open_questions: vec!["Who?".into()],
                topics: vec!["planning".into()],
            },
        }
    }

    fn audio(&mut self, meeting: EntityId, track: EntityId, created: u64) -> AudioFile {
        AudioFile {
            id: self.id(),
            meeting_id: meeting,
            track_id: track,
            path: format!("audio/{meeting}/mic.flac"),
            format: AudioFormat::Flac,
            sample_rate_hz: 16_000,
            frames: 48_000,
            size_bytes: 12_345,
            pipeline_version: "earshot-0.1.0".into(),
            aec_version: Some("aec-1".into()),
            created_unix_ms: created,
        }
    }
}

fn missing(r: Result<(), RepoError>, entity: &str, what: &str) {
    match r {
        Err(RepoError::MissingParent { entity: e, .. }) if e == entity => {}
        other => panic!("{what}: expected MissingParent({entity}), got {other:?}"),
    }
}

/// [`MeetingRepo`]: round trips, replace-by-id, tombstones kept, list orders, parent checks.
///
/// # Panics
/// On any violation.
pub fn meeting_repo<R: MeetingRepo>(make: impl Fn() -> R) {
    let mut f = Rows::new();
    let mut repo = make();

    // Meetings: round trip, order by start then id, replace by id.
    let late = f.meeting(2_000);
    let early = f.meeting(1_000);
    assert_eq!(repo.meeting(late.id).unwrap(), None, "empty repo");
    repo.put_meeting(late.clone()).unwrap();
    repo.put_meeting(early.clone()).unwrap();
    assert_eq!(repo.meeting(late.id).unwrap().as_ref(), Some(&late));
    assert_eq!(repo.meetings().unwrap(), vec![early.clone(), late.clone()]);
    let mut renamed = late.clone();
    renamed.title = "Renamed".into();
    renamed.duration_ms = None;
    renamed.tags.clear();
    renamed.meta.touch(&mut f.hlc, &f.clock);
    repo.put_meeting(renamed.clone()).unwrap();
    assert_eq!(repo.meeting(late.id).unwrap().as_ref(), Some(&renamed));
    assert_eq!(repo.meetings().unwrap().len(), 2, "replace, not insert");
    let m = renamed.id;
    let other = early.id;

    // Tracks.
    let ghost = f.id();
    missing(
        repo.put_track(f.track(ghost, TrackId::Mic)),
        "meeting",
        "track of a missing meeting",
    );
    let reference = f.track(m, TrackId::Reference);
    let mic = f.track(m, TrackId::Mic);
    let other_mic = f.track(other, TrackId::Mic);
    repo.put_track(reference.clone()).unwrap();
    repo.put_track(mic.clone()).unwrap();
    repo.put_track(other_mic.clone()).unwrap();
    assert_eq!(
        repo.tracks(m).unwrap(),
        vec![mic.clone(), reference.clone()],
        "tracks of one meeting, mic first"
    );

    // Speakers.
    missing(
        repo.put_speaker(f.speaker(ghost)),
        "meeting",
        "speaker of a missing meeting",
    );
    let sp1 = f.speaker(m);
    let mut sp2 = f.speaker(m);
    sp2.role = SpeakerRole::Me;
    sp2.person_id = None;
    let other_sp = f.speaker(other);
    repo.put_speaker(sp1.clone()).unwrap();
    repo.put_speaker(sp2.clone()).unwrap();
    repo.put_speaker(other_sp.clone()).unwrap();
    assert_eq!(repo.speakers(m).unwrap(), vec![sp1.clone(), sp2.clone()]);
    // Creation order is `meta.created_at` (HLC), not id order: UUIDv7 ids created in the
    // same millisecond sort by their random bits. A later speaker with a lower id comes
    // last; equal timestamps fall back to id order.
    let mut low = [0u8; 16];
    low[6] = 0x70;
    low[8] = 0x80;
    let mut sp3 = f.speaker(m);
    low[15] = 2;
    sp3.id = EntityId::from_bytes(low);
    let mut sp4 = f.speaker(m);
    low[15] = 1;
    sp4.id = EntityId::from_bytes(low);
    sp4.meta = sp3.meta;
    assert!(
        sp4.id < sp3.id && sp3.id < sp1.id,
        "ids sort against creation order"
    );
    repo.put_speaker(sp3.clone()).unwrap();
    repo.put_speaker(sp4.clone()).unwrap();
    assert_eq!(
        repo.speakers(m).unwrap(),
        vec![sp1.clone(), sp2.clone(), sp4.clone(), sp3.clone()],
        "speakers are in creation (HLC) order, id as tiebreak"
    );

    // Segments: exact round trip (words, f32 probabilities), order, parent checks.
    missing(
        repo.put_segment(f.segment(ghost, mic.id, 0)),
        "meeting",
        "segment of a missing meeting",
    );
    missing(
        repo.put_segment(f.segment(m, ghost, 0)),
        "track",
        "segment of a missing track",
    );
    missing(
        repo.put_segment(f.segment(m, other_mic.id, 0)),
        "track",
        "segment on another meeting's track",
    );
    let mut bad_sp = f.segment(m, mic.id, 0);
    bad_sp.segment.speaker_id = Some(other_sp.id);
    missing(
        repo.put_segment(bad_sp),
        "speaker",
        "segment with another meeting's speaker",
    );
    let s_late = f.segment(m, reference.id, 5_000);
    let mut s_early = f.segment(m, mic.id, 1_000);
    s_early.segment.speaker_id = Some(sp1.id);
    s_early.pass = TranscriptPass::Final;
    repo.put_segment(s_late.clone()).unwrap();
    repo.put_segment(s_early.clone()).unwrap();
    assert_eq!(
        repo.segments(m).unwrap(),
        vec![s_early.clone(), s_late.clone()]
    );
    assert!(repo.segments(other).unwrap().is_empty());
    // Tombstones are kept and returned.
    let mut gone = s_late.clone();
    gone.meta.mark_deleted(&mut f.hlc, &f.clock);
    repo.put_segment(gone.clone()).unwrap();
    assert_eq!(repo.segments(m).unwrap(), vec![s_early.clone(), gone]);

    // Notes: order by anchor.
    missing(
        repo.put_note(f.note(ghost, 0)),
        "meeting",
        "note of a missing meeting",
    );
    let n2 = f.note(m, 90_000);
    let n1 = f.note(m, 1_250);
    repo.put_note(n2.clone()).unwrap();
    repo.put_note(n1.clone()).unwrap();
    assert_eq!(repo.notes(m).unwrap(), vec![n1, n2]);

    // Summaries: order by creation.
    missing(
        repo.put_summary(f.summary(ghost, s_early.segment.id)),
        "meeting",
        "summary of a missing meeting",
    );
    let first = f.summary(m, s_early.segment.id);
    let second = f.summary(m, s_early.segment.id);
    repo.put_summary(second.clone()).unwrap();
    repo.put_summary(first.clone()).unwrap();
    assert_eq!(repo.summaries(m).unwrap(), vec![first, second]);
    assert!(repo.summaries(other).unwrap().is_empty());
}

/// [`AudioFileRepo`]: round trip, parent checks, order, hard delete.
///
/// # Panics
/// On any violation.
pub fn audio_file_repo<R: MeetingRepo + AudioFileRepo>(make: impl Fn() -> R) {
    let mut f = Rows::new();
    let mut repo = make();
    let a = f.meeting(1);
    let b = f.meeting(2);
    repo.put_meeting(a.clone()).unwrap();
    repo.put_meeting(b.clone()).unwrap();
    let ta = f.track(a.id, TrackId::Mic);
    let tb = f.track(b.id, TrackId::Mic);
    repo.put_track(ta.clone()).unwrap();
    repo.put_track(tb.clone()).unwrap();

    let ghost = f.id();
    missing(
        repo.put_audio_file(f.audio(ghost, ta.id, 1)),
        "meeting",
        "audio of a missing meeting",
    );
    missing(
        repo.put_audio_file(f.audio(a.id, tb.id, 1)),
        "track",
        "audio on another meeting's track",
    );

    let newer = f.audio(a.id, ta.id, 500);
    let mut older = f.audio(b.id, tb.id, 100);
    older.aec_version = None;
    repo.put_audio_file(newer.clone()).unwrap();
    repo.put_audio_file(older.clone()).unwrap();
    assert_eq!(repo.audio_files(a.id).unwrap(), vec![newer.clone()]);
    assert_eq!(
        repo.all_audio_files().unwrap(),
        vec![older.clone(), newer.clone()]
    );
    // Paths are unique: another row naming the same file is a conflict and changes
    // nothing; replacing a row by its own id keeps its path.
    let mut dup = f.audio(a.id, ta.id, 600);
    dup.path = newer.path.clone();
    match repo.put_audio_file(dup) {
        Err(RepoError::Conflict {
            entity: "audio_file",
            field: "path",
        }) => {}
        other => panic!("duplicate audio path: expected Conflict, got {other:?}"),
    }
    let mut same = newer.clone();
    same.size_bytes += 1;
    repo.put_audio_file(same.clone()).unwrap();
    repo.put_audio_file(newer.clone()).unwrap();
    assert_eq!(
        repo.all_audio_files().unwrap(),
        vec![older.clone(), newer.clone()]
    );
    assert!(repo.delete_audio_file(older.id).unwrap());
    assert!(
        !repo.delete_audio_file(older.id).unwrap(),
        "second delete finds nothing"
    );
    assert_eq!(repo.all_audio_files().unwrap(), vec![newer]);
}

/// [`super::delete_meeting`]: every syncable row of the meeting (live or already a
/// tombstone) is read back as a tombstone with its content cleared; another meeting is
/// untouched; a second run writes nothing.
///
/// # Panics
/// On any violation.
pub fn meeting_deletion<R: MeetingRepo>(make: impl Fn() -> R) {
    let mut f = Rows::new();
    let mut repo = make();
    let m = f.meeting(1_000);
    let keep = f.meeting(2_000);
    repo.put_meeting(m.clone()).unwrap();
    repo.put_meeting(keep.clone()).unwrap();
    let mut rows_of = |repo: &mut R, meeting: EntityId| {
        let mic = f.track(meeting, TrackId::Mic);
        repo.put_track(mic.clone()).unwrap();
        let sp = f.speaker(meeting);
        repo.put_speaker(sp.clone()).unwrap();
        let mut seg = f.segment(meeting, mic.id, 0);
        seg.segment.speaker_id = Some(sp.id);
        repo.put_segment(seg.clone()).unwrap();
        // An earlier final pass's tombstone still holds its text until the meeting goes.
        let mut old = f.segment(meeting, mic.id, 2_000);
        old.pass = TranscriptPass::Final;
        old.meta.mark_deleted(&mut f.hlc, &f.clock);
        repo.put_segment(old).unwrap();
        repo.put_note(f.note(meeting, 10)).unwrap();
        repo.put_summary(f.summary(meeting, seg.segment.id))
            .unwrap();
        sp.person_id.unwrap()
    };
    let person = rows_of(&mut repo, m.id);
    rows_of(&mut repo, keep.id);
    let snapshot = |repo: &R, id| {
        (
            repo.meeting(id).unwrap(),
            repo.tracks(id).unwrap(),
            repo.speakers(id).unwrap(),
            repo.segments(id).unwrap(),
            repo.notes(id).unwrap(),
            repo.summaries(id).unwrap(),
        )
    };
    let kept_before = snapshot(&repo, keep.id);
    let before = snapshot(&repo, m.id);

    let out = super::delete_meeting(&mut repo, m.id, &mut f.hlc, &f.clock).unwrap();
    assert_eq!(
        out.rows_written,
        1 + 1 + 1 + 2 + 1 + 1,
        "every row of the meeting"
    );
    assert_eq!(
        out.persons.into_iter().collect::<Vec<_>>(),
        vec![person],
        "the persons its speakers named (persons are kept)"
    );
    assert_eq!(
        snapshot(&repo, keep.id),
        kept_before,
        "another meeting is untouched"
    );

    let gone = repo
        .meeting(m.id)
        .unwrap()
        .expect("the tombstone stays (it syncs)");
    assert!(gone.meta.deleted);
    assert!(gone.meta.updated_at > before.0.as_ref().unwrap().meta.updated_at);
    assert!(gone.title.is_empty() && gone.tags.is_empty(), "{gone:?}");
    for t in repo.tracks(m.id).unwrap() {
        assert!(t.meta.deleted && t.device_name.is_none(), "{t:?}");
    }
    for s in repo.speakers(m.id).unwrap() {
        assert!(s.meta.deleted, "{s:?}");
        assert!(s.display_name.is_empty() && s.person_id.is_none(), "{s:?}");
    }
    let segments = repo.segments(m.id).unwrap();
    assert_eq!(segments.len(), 2);
    for s in segments {
        assert!(s.meta.deleted, "{s:?}");
        assert!(
            s.segment.text.is_empty() && s.segment.words.is_empty(),
            "{s:?}"
        );
        assert_eq!(s.segment.speaker_id, None);
        assert_eq!(s.segment.confidence.to_bits(), 0f32.to_bits());
    }
    for n in repo.notes(m.id).unwrap() {
        assert!(n.meta.deleted && n.text.is_empty(), "{n:?}");
    }
    for s in repo.summaries(m.id).unwrap() {
        assert!(s.meta.deleted && s.model.is_none(), "{s:?}");
        assert_eq!(s.summary, SummaryV1::default(), "{s:?}");
    }

    let erased = snapshot(&repo, m.id);
    let again = super::delete_meeting(&mut repo, m.id, &mut f.hlc, &f.clock).unwrap();
    assert_eq!(
        again.rows_written, 0,
        "an erased meeting is not written again"
    );
    assert_eq!(snapshot(&repo, m.id), erased);

    let ghost = f.id();
    missing(
        super::delete_meeting(&mut repo, ghost, &mut f.hlc, &f.clock).map(|_| ()),
        "meeting",
        "deleting a missing meeting",
    );
}

/// [`ExportRepo`]: one record per (meeting, device), parent check.
///
/// # Panics
/// On any violation.
pub fn export_repo<R: MeetingRepo + ExportRepo>(make: impl Fn() -> R) {
    let mut f = Rows::new();
    let mut repo = make();
    let m = f.meeting(1);
    repo.put_meeting(m.clone()).unwrap();
    let here = DeviceId::from_bytes([1; 16]);
    let there = DeviceId::from_bytes([2; 16]);
    let rec = |f: &mut Rows, device, path: &str, at| ExportRecord {
        id: f.id(),
        meeting_id: m.id,
        device_id: device,
        path: VaultPath::new(path).unwrap(),
        exported_unix_ms: at,
    };
    let ghost = f.id();
    let mut orphan = rec(&mut f, here, "a.md", 1);
    orphan.meeting_id = ghost;
    missing(
        repo.put_export_record(orphan),
        "meeting",
        "export of a missing meeting",
    );

    assert_eq!(repo.export_record(m.id, here).unwrap(), None);
    let first = rec(&mut f, here, "Meetings/a.md", 10);
    repo.put_export_record(first.clone()).unwrap();
    assert_eq!(
        repo.export_record(m.id, here).unwrap().as_ref(),
        Some(&first)
    );
    let elsewhere = rec(&mut f, there, "Other/a.md", 11);
    repo.put_export_record(elsewhere.clone()).unwrap();
    let moved = rec(&mut f, here, "Meetings/Sub/b.md", 20);
    repo.put_export_record(moved.clone()).unwrap();
    assert_eq!(
        repo.export_record(m.id, here).unwrap().as_ref(),
        Some(&moved),
        "a second record for the same device replaces the first"
    );
    assert_eq!(
        repo.export_record(m.id, there).unwrap().as_ref(),
        Some(&elsewhere)
    );
}
