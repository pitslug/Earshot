//! The persisted entities (PLAN.md 3.5).

use core::fmt;

use rand_core::Rng;
use serde::{Deserialize, Serialize};

use crate::asr::{MeetingTime, Segment};
use crate::capture::TrackId;
use crate::diar::SpeakerRole;
use crate::identity::{
    Clock, DeviceId, EntityId, Hlc, HlcTimestamp, NeverSyncClass, SyncClass, SyncMeta,
    SyncSerialize, SyncableClass,
};
use crate::prompt::Template;
use crate::summary::{SUMMARY_SCHEMA_VERSION, SummaryV1};
use crate::voiceprint::Stamp;

fn new_row<C: Clock + ?Sized, R: Rng + ?Sized>(
    stamp: &mut Stamp<'_, C, R>,
) -> (EntityId, SyncMeta) {
    (
        EntityId::generate(stamp.clock, stamp.rng),
        SyncMeta::new(stamp.hlc, stamp.clock),
    )
}

/// Implements [`SyncClass`] as `Syncable` and [`SyncSerialize`] for a row with `id` and
/// `meta` fields.
macro_rules! syncable_row {
    ($ty:ty, $entity:literal) => {
        impl SyncClass for $ty {
            type Class = SyncableClass;
        }
        impl SyncSerialize for $ty {
            const ENTITY: &'static str = $entity;
            fn sync_id(&self) -> EntityId {
                self.id
            }
            fn sync_meta(&self) -> &SyncMeta {
                &self.meta
            }
        }
    };
}

/// A recorded meeting.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Meeting {
    /// Row id.
    pub id: EntityId,
    /// Sync metadata.
    pub meta: SyncMeta,
    /// Title the user gave the meeting, or the summary's title; empty until either exists.
    pub title: String,
    /// Wall-clock start (Unix ms, from the injected [`Clock`]). Only the meeting itself is
    /// anchored in wall-clock time; everything inside it uses [`MeetingTime`].
    pub started_unix_ms: u64,
    /// The local UTC offset at the start, in minutes (core has no time zone database; the
    /// adapter records it so exports show the local date and time).
    pub utc_offset_minutes: i32,
    /// Length of the meeting; `None` while recording.
    pub duration_ms: Option<u64>,
    /// User tags (exported as Obsidian tags).
    pub tags: Vec<String>,
    /// Summary template.
    pub template: Template,
    /// The [`Folder`] the user filed the meeting in; `None` for no folder. Not a foreign key:
    /// sync may deliver a meeting before its folder, and an id naming a tombstoned or
    /// unknown folder reads as no folder (docs/folders-design.md). `serde(default)` so JSON
    /// written before folders existed still reads.
    #[serde(default)]
    pub folder_id: Option<EntityId>,
}

syncable_row!(Meeting, "meeting");

impl Meeting {
    /// A new meeting starting at `started_unix_ms`.
    pub fn new<C: Clock + ?Sized, R: Rng + ?Sized>(
        title: impl Into<String>,
        started_unix_ms: u64,
        utc_offset_minutes: i32,
        template: Template,
        stamp: &mut Stamp<'_, C, R>,
    ) -> Self {
        let (id, meta) = new_row(stamp);
        Meeting {
            id,
            meta,
            title: title.into(),
            started_unix_ms,
            utc_offset_minutes,
            duration_ms: None,
            tags: Vec::new(),
            template,
            folder_id: None,
        }
    }
}

/// A named folder the user files meetings in (one level; a meeting is in at most one, through
/// [`Meeting::folder_id`]). The name is content: a tombstone has it cleared. Name rules and
/// uniqueness among live folders are the runtime's (T9.5b), never a storage constraint,
/// because every tombstone carries the same cleared name and sync may deliver duplicates.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Folder {
    /// Row id.
    pub id: EntityId,
    /// Sync metadata.
    pub meta: SyncMeta,
    /// The name the user gave the folder; empty once deleted.
    pub name: String,
}

syncable_row!(Folder, "folder");

impl Folder {
    /// A new folder named `name` (not checked here).
    pub fn new<C: Clock + ?Sized, R: Rng + ?Sized>(
        name: impl Into<String>,
        stamp: &mut Stamp<'_, C, R>,
    ) -> Self {
        let (id, meta) = new_row(stamp);
        Folder {
            id,
            meta,
            name: name.into(),
        }
    }
}

/// Which clock a track's device timestamps came from (for diagnosing alignment; transcript
/// times are always [`MeetingTime`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClockDomain {
    /// Windows QueryPerformanceCounter (WASAPI, 100 ns units).
    Qpc,
    /// Android `CLOCK_MONOTONIC` (AudioRecord timestamps).
    AndroidMonotonic,
    /// A replayed or imported recording (no live device clock).
    Replay,
}

impl ClockDomain {
    /// Stable name (`qpc`, `android_monotonic`, `replay`).
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            ClockDomain::Qpc => "qpc",
            ClockDomain::AndroidMonotonic => "android_monotonic",
            ClockDomain::Replay => "replay",
        }
    }

    /// Parses [`ClockDomain::name`].
    #[must_use]
    pub fn from_name(s: &str) -> Option<Self> {
        [Self::Qpc, Self::AndroidMonotonic, Self::Replay]
            .into_iter()
            .find(|c| c.name() == s)
    }
}

/// One audio track of a meeting (metadata only; the audio itself is an [`AudioFile`] and
/// never syncs).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Track {
    /// Row id. Created before transcription starts, so segments can reference it.
    pub id: EntityId,
    /// Sync metadata.
    pub meta: SyncMeta,
    /// The meeting.
    pub meeting_id: EntityId,
    /// Mic or far-end reference.
    pub kind: TrackId,
    /// Endpoint name, when known.
    pub device_name: Option<String>,
    /// Device clock the capture timestamps came from.
    pub clock_domain: ClockDomain,
}

syncable_row!(Track, "track");

impl Track {
    /// A new track of `meeting_id`.
    pub fn new<C: Clock + ?Sized, R: Rng + ?Sized>(
        meeting_id: EntityId,
        kind: TrackId,
        device_name: Option<String>,
        clock_domain: ClockDomain,
        stamp: &mut Stamp<'_, C, R>,
    ) -> Self {
        let (id, meta) = new_row(stamp);
        Track {
            id,
            meta,
            meeting_id,
            kind,
            device_name,
            clock_domain,
        }
    }
}

/// Which transcription pass produced a segment (PLAN.md D4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TranscriptPass {
    /// The live draft during the meeting.
    Live,
    /// The high-accuracy pass at stop, or a later re-transcription (which replaces it).
    Final,
}

impl TranscriptPass {
    /// Stable name (`live`, `final`).
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            TranscriptPass::Live => "live",
            TranscriptPass::Final => "final",
        }
    }

    /// Parses [`TranscriptPass::name`].
    #[must_use]
    pub fn from_name(s: &str) -> Option<Self> {
        match s {
            "live" => Some(TranscriptPass::Live),
            "final" => Some(TranscriptPass::Final),
            _ => None,
        }
    }
}

/// A persisted transcript segment: the ASR [`Segment`] (whose id is the row id) with sync
/// metadata, its meeting and the pass that produced it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SegmentRow {
    /// Sync metadata.
    pub meta: SyncMeta,
    /// The meeting (the segment's track belongs to it).
    pub meeting_id: EntityId,
    /// Live draft or final pass.
    pub pass: TranscriptPass,
    /// The segment; `segment.id` is the row id.
    pub segment: Segment,
    /// When the user last changed this segment's text (T12.16); `None` for text exactly as
    /// the recogniser wrote it. An edited segment keeps its id, track, start and end (so
    /// summary citations still find it and its audio is the same stretch) and has no words:
    /// per-word times described the recogniser's words, which the user's text no longer is,
    /// so they are dropped rather than kept beside text they do not match. `confidence`
    /// stays the recogniser's for the replaced text. A re-transcription keeps an edited
    /// segment (`earshot_runtime::post`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text_edited_at: Option<HlcTimestamp>,
}

impl SyncClass for SegmentRow {
    type Class = SyncableClass;
}

impl SyncSerialize for SegmentRow {
    const ENTITY: &'static str = "segment";
    fn sync_id(&self) -> EntityId {
        self.segment.id
    }
    fn sync_meta(&self) -> &SyncMeta {
        &self.meta
    }
}

impl SegmentRow {
    /// Wraps `segment` (keeping its id) as a new row of `meeting_id`.
    pub fn new<C: Clock + ?Sized>(
        meeting_id: EntityId,
        pass: TranscriptPass,
        segment: Segment,
        hlc: &mut Hlc,
        clock: &C,
    ) -> Self {
        SegmentRow {
            meta: SyncMeta::new(hlc, clock),
            meeting_id,
            pass,
            segment,
            text_edited_at: None,
        }
    }
}

/// A speaker of one meeting.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Speaker {
    /// Row id (the `speaker_id` of segments).
    pub id: EntityId,
    /// Sync metadata.
    pub meta: SyncMeta,
    /// The meeting.
    pub meeting_id: EntityId,
    /// Role under the track policy.
    pub role: SpeakerRole,
    /// Name shown in the transcript ("Me", "Speaker 2", or the person's name).
    pub display_name: String,
    /// The recurring person, when identified or named.
    pub person_id: Option<EntityId>,
}

syncable_row!(Speaker, "speaker");

impl Speaker {
    /// A new speaker of `meeting_id`.
    pub fn new<C: Clock + ?Sized, R: Rng + ?Sized>(
        meeting_id: EntityId,
        role: SpeakerRole,
        display_name: impl Into<String>,
        person_id: Option<EntityId>,
        stamp: &mut Stamp<'_, C, R>,
    ) -> Self {
        let (id, meta) = new_row(stamp);
        Speaker {
            id,
            meta,
            meeting_id,
            role,
            display_name: display_name.into(),
            person_id,
        }
    }
}

/// A note the user typed during the meeting.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Note {
    /// Row id.
    pub id: EntityId,
    /// Sync metadata.
    pub meta: SyncMeta,
    /// The meeting.
    pub meeting_id: EntityId,
    /// Anchor: milliseconds from the meeting start, never wall-clock time (PLAN.md 3.5).
    pub at: MeetingTime,
    /// The note text (may span lines).
    pub text: String,
}

syncable_row!(Note, "note");

impl Note {
    /// A new note of `meeting_id` anchored at `at`.
    pub fn new<C: Clock + ?Sized, R: Rng + ?Sized>(
        meeting_id: EntityId,
        at: MeetingTime,
        text: impl Into<String>,
        stamp: &mut Stamp<'_, C, R>,
    ) -> Self {
        let (id, meta) = new_row(stamp);
        Note {
            id,
            meta,
            meeting_id,
            at,
            text: text.into(),
        }
    }
}

/// A validated summary of a meeting. A meeting may have several (re-summarised with
/// another template or model); the newest live one is current.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Summary {
    /// Row id.
    pub id: EntityId,
    /// Sync metadata.
    pub meta: SyncMeta,
    /// The meeting.
    pub meeting_id: EntityId,
    /// Schema version of `summary` ([`SUMMARY_SCHEMA_VERSION`] when created).
    pub schema_version: u32,
    /// Template used.
    pub template: Template,
    /// LLM model name reported by the endpoint, when known.
    pub model: Option<String>,
    /// The summary.
    pub summary: SummaryV1,
}

syncable_row!(Summary, "summary");

impl Summary {
    /// A new summary of `meeting_id`.
    pub fn new<C: Clock + ?Sized, R: Rng + ?Sized>(
        meeting_id: EntityId,
        template: Template,
        model: Option<String>,
        summary: SummaryV1,
        stamp: &mut Stamp<'_, C, R>,
    ) -> Self {
        let (id, meta) = new_row(stamp);
        Summary {
            id,
            meta,
            meeting_id,
            schema_version: SUMMARY_SCHEMA_VERSION,
            template,
            model,
            summary,
        }
    }
}

/// Audio container format of an [`AudioFile`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AudioFormat {
    /// FLAC, 16-bit mono (PLAN.md 3.6, D9).
    Flac,
}

impl AudioFormat {
    /// Stable name (`flac`).
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            AudioFormat::Flac => "flac",
        }
    }

    /// Parses [`AudioFormat::name`].
    #[must_use]
    pub fn from_name(s: &str) -> Option<Self> {
        (s == "flac").then_some(AudioFormat::Flac)
    }
}

/// A finalised audio file of one track (NeverSync: audio never leaves the device).
///
/// The row is written only after the file is finalised (PLAN.md 3.6); the file may later
/// be removed by retention, so readers tolerate a row whose file is missing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AudioFile {
    /// Row id.
    pub id: EntityId,
    /// The meeting.
    pub meeting_id: EntityId,
    /// The track whose audio this is.
    pub track_id: EntityId,
    /// Path relative to the data directory, with `/` separators (`audio/<meeting>/mic.flac`).
    pub path: String,
    /// Container format.
    pub format: AudioFormat,
    /// Sample rate.
    pub sample_rate_hz: u32,
    /// Number of frames.
    pub frames: u64,
    /// File size.
    pub size_bytes: u64,
    /// Version of the pipeline that produced the audio.
    pub pipeline_version: String,
    /// Version of the echo canceller applied, or `None` when none ran (reference track,
    /// in-person recording).
    pub aec_version: Option<String>,
    /// When the file was finalised (Unix ms, injected clock); retention ages from this.
    pub created_unix_ms: u64,
}

impl SyncClass for AudioFile {
    type Class = NeverSyncClass;
}

impl AudioFile {
    /// Length in milliseconds (rounded down); 0 for a zero sample rate.
    #[must_use]
    pub fn duration_ms(&self) -> u64 {
        if self.sample_rate_hz == 0 {
            return 0;
        }
        self.frames.saturating_mul(1000) / u64::from(self.sample_rate_hz)
    }
}

/// Why a vault-relative path was rejected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VaultPathError {
    /// The rejected input.
    pub path: String,
    /// Reason.
    pub reason: &'static str,
}

impl fmt::Display for VaultPathError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "invalid vault path {:?}: {}", self.path, self.reason)
    }
}

impl std::error::Error for VaultPathError {}

/// A path inside an Obsidian vault: relative, `/`-separated, no `.`/`..`/empty components,
/// no drive or root prefix and no control characters, so joining it to the vault root
/// cannot leave the vault.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct VaultPath(String);

impl VaultPath {
    /// Validates `path` (backslashes are treated as separators and normalised to `/`).
    ///
    /// # Errors
    /// [`VaultPathError`] when the path is empty, absolute, has a drive prefix, or has an
    /// empty, `.` or `..` component or a control character.
    pub fn new(path: &str) -> Result<Self, VaultPathError> {
        let err = |reason| VaultPathError {
            path: path.to_owned(),
            reason,
        };
        if path.is_empty() {
            return Err(err("empty"));
        }
        if path.chars().any(char::is_control) {
            return Err(err("control character"));
        }
        let norm = path.replace('\\', "/");
        if norm.starts_with('/') {
            return Err(err("absolute"));
        }
        if norm.contains(':') {
            return Err(err("drive or stream prefix"));
        }
        for c in norm.split('/') {
            match c {
                "" => return Err(err("empty component")),
                "." | ".." => return Err(err("dot component")),
                _ => {}
            }
        }
        Ok(VaultPath(norm))
    }

    /// The path with `/` separators.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The last component.
    #[must_use]
    pub fn file_name(&self) -> &str {
        self.0.rsplit('/').next().unwrap_or(&self.0)
    }

    /// The components.
    pub fn components(&self) -> impl Iterator<Item = &str> {
        self.0.split('/')
    }
}

impl TryFrom<String> for VaultPath {
    type Error = VaultPathError;
    fn try_from(s: String) -> Result<Self, Self::Error> {
        VaultPath::new(&s)
    }
}

impl From<VaultPath> for String {
    fn from(p: VaultPath) -> String {
        p.0
    }
}

impl fmt::Display for VaultPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Where a meeting was last exported on one device (NeverSync: vault layout is
/// device-local). One record per (meeting, device).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExportRecord {
    /// Row id.
    pub id: EntityId,
    /// The meeting.
    pub meeting_id: EntityId,
    /// The device that wrote the file.
    pub device_id: DeviceId,
    /// Vault-relative path of the written file.
    pub path: VaultPath,
    /// When it was written (Unix ms, injected clock).
    pub exported_unix_ms: u64,
}

impl SyncClass for ExportRecord {
    type Class = NeverSyncClass;
}
