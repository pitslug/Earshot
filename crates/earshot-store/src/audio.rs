//! Audio file rows: finalising, retention and loading for re-transcription (PLAN.md 3.6).

use std::collections::{BTreeMap, BTreeSet};

use earshot_core::identity::{Clock, EntityId};
use earshot_core::model::{AudioFile, AudioFileRepo, AudioFormat, MeetingRepo, RepoError, Track};
use rand_core::Rng;

use crate::db::{Store, StoreError};
use crate::flac::{FlacTrackWriter, read_flac};
use crate::layout::DataDir;

/// One day in milliseconds.
pub const DAY_MS: u64 = 86_400_000;

/// Age (by modification time) after which an audio file without a row, or a `.part` file,
/// is removed by [`run_retention`] and [`DataDir::cleanup_stale_parts`]: 7 days, so a crashed
/// recording stays recoverable for a week and a recording in progress (its `.part` is
/// written continuously) is never touched.
pub const ORPHAN_GRACE_MS: u64 = 7 * DAY_MS;

/// What produced a track's audio (stored on the `AudioFile` row).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioProvenance {
    /// Pipeline version.
    pub pipeline_version: String,
    /// Echo canceller version, `None` when none ran on this track.
    pub aec_version: Option<String>,
}

impl Store {
    /// Finalises `writer` and only then inserts its `AudioFile` row (PLAN.md 3.6). The
    /// writer must be the layout writer of `track` ([`DataDir::track_writer`]).
    ///
    /// The meeting and track are checked first: if either is missing the writer is dropped
    /// unfinished, so the audio stays on disk as `<path>.part` and no row is written. If the
    /// row insert fails after the rename, the finished file is kept without a row.
    ///
    /// # Errors
    /// [`StoreError::Repo`] for a missing meeting or track; [`StoreError::Corrupt`] if the
    /// writer is not at the track's layout path; encoding, I/O or SQLite errors.
    pub fn finalize_track_audio<C: Clock + ?Sized, R: Rng + ?Sized>(
        &mut self,
        writer: FlacTrackWriter,
        data: &DataDir,
        track: &Track,
        provenance: AudioProvenance,
        clock: &C,
        rng: &mut R,
    ) -> Result<AudioFile, StoreError> {
        finalize_track_audio_outside_lock(writer, data, track, provenance, clock, rng, &mut |f| {
            f(self)
        })
    }
}

/// Runs one store step for [`finalize_track_audio_outside_lock`]: the caller takes its lock,
/// calls the step on the store and releases the lock.
pub type WithStore<'a> =
    dyn FnMut(&mut dyn FnMut(&mut Store) -> Result<(), StoreError>) -> Result<(), StoreError> + 'a;

/// [`Store::finalize_track_audio`] for a caller whose store sits behind a lock (T9.17): the store
/// is reached only through `with_store`, once for the meeting and track check and once for the
/// row, so the file work between them (encoding the rest, header, sync, rename) runs without the
/// lock. The same rules: the check comes first (a missing track leaves the `.part` unfinished and
/// writes no row), and the row only after the rename.
///
/// # Errors
/// As [`Store::finalize_track_audio`].
pub fn finalize_track_audio_outside_lock<C: Clock + ?Sized, R: Rng + ?Sized>(
    writer: FlacTrackWriter,
    data: &DataDir,
    track: &Track,
    provenance: AudioProvenance,
    clock: &C,
    rng: &mut R,
    with_store: &mut WithStore<'_>,
) -> Result<AudioFile, StoreError> {
    let rel = DataDir::audio_rel(track.meeting_id, track.kind);
    if writer.path() != data.resolve(&rel)? {
        return Err(StoreError::Corrupt(format!(
            "writer at {} is not the layout path {rel}",
            writer.path().display()
        )));
    }
    let mut present = false;
    with_store(&mut |s| {
        present = s.tracks(track.meeting_id)?.iter().any(|t| t.id == track.id);
        Ok(())
    })?;
    if !present {
        return Err(StoreError::Repo(RepoError::MissingParent {
            entity: "track",
            id: track.id,
        }));
    }
    let done = writer.finish()?;
    let file = AudioFile {
        id: EntityId::generate(clock, rng),
        meeting_id: track.meeting_id,
        track_id: track.id,
        path: rel,
        format: AudioFormat::Flac,
        sample_rate_hz: done.sample_rate,
        frames: done.frames,
        size_bytes: done.size_bytes,
        pipeline_version: provenance.pipeline_version,
        aec_version: provenance.aec_version,
        created_unix_ms: clock.now_unix_ms(),
    };
    let row = file.clone();
    with_store(&mut |s| s.put_audio_file(row.clone()).map_err(StoreError::from))?;
    Ok(file)
}

/// One hour in milliseconds.
pub const HOUR_MS: u64 = 3_600_000;

/// The `local_state` key prefix of a meeting's retention clock ([`retention_clock_key`]).
pub const RETENTION_CLOCK_PREFIX: &str = "audio-retention/";

/// The version written before a retention clock's payload.
pub const RETENTION_CLOCK_VERSION: u32 = 1;

/// The `local_state` key of `meeting`'s retention clock: `audio-retention/<meeting>`.
#[must_use]
pub fn retention_clock_key(meeting: EntityId) -> String {
    format!("{RETENTION_CLOCK_PREFIX}{meeting}")
}

/// A retention clock record: `version u32 LE | since u64 LE` (Unix ms, the clock of the
/// process that started it).
#[must_use]
pub fn encode_retention_clock(since_unix_ms: u64) -> Vec<u8> {
    let mut out = RETENTION_CLOCK_VERSION.to_le_bytes().to_vec();
    out.extend_from_slice(&since_unix_ms.to_le_bytes());
    out
}

/// Reads [`encode_retention_clock`]; `None` for another version or a damaged record.
#[must_use]
pub fn decode_retention_clock(bytes: &[u8]) -> Option<u64> {
    let (version, since) = bytes.split_first_chunk::<4>()?;
    let since: [u8; 8] = since.try_into().ok()?;
    (u32::from_le_bytes(*version) == RETENTION_CLOCK_VERSION).then(|| u64::from_le_bytes(since))
}

/// When `meeting`'s retention clock started, as stored.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RetentionClock {
    /// Not started: no summary was stored and no final pass ended with summaries off.
    NotStarted,
    /// Started at this Unix ms.
    Since(u64),
    /// A record this version cannot read (kept: audio cannot be recovered).
    Unreadable,
}

/// `meeting`'s retention clock.
///
/// # Errors
/// SQLite errors.
pub fn retention_clock(store: &Store, meeting: EntityId) -> Result<RetentionClock, StoreError> {
    Ok(match store.local_value(&retention_clock_key(meeting))? {
        None => RetentionClock::NotStarted,
        Some(bytes) => {
            decode_retention_clock(&bytes).map_or(RetentionClock::Unreadable, RetentionClock::Since)
        }
    })
}

/// How long audio is kept (PLAN.md 3.6, T7.4k). The default keeps it 7 days.
///
/// Every limited policy is summary-gated: a meeting's audio ages from its retention clock
/// ([`retention_clock_key`]), which the runtime starts when the meeting's first summary is
/// stored, or when its final pass ends with summaries turned off. A meeting whose clock has
/// not started (recording, no final pass yet, a failed or cancelled summary) keeps its audio
/// whatever the policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RetentionPolicy {
    /// Never delete audio.
    KeepForever,
    /// Delete a meeting's audio once its retention clock started at least this long ago.
    KeepFor {
        /// How long, in milliseconds.
        ms: u64,
    },
}

impl Default for RetentionPolicy {
    fn default() -> Self {
        Self::days(7)
    }
}

impl RetentionPolicy {
    /// Keep audio for `n` hours after the clock starts.
    #[must_use]
    pub const fn hours(n: u64) -> Self {
        Self::KeepFor {
            ms: n.saturating_mul(HOUR_MS),
        }
    }

    /// Keep audio for `n` days after the clock starts.
    #[must_use]
    pub const fn days(n: u64) -> Self {
        Self::KeepFor {
            ms: n.saturating_mul(DAY_MS),
        }
    }
}

/// The audio a policy deletes now ([`due_audio`]).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct DueAudio {
    /// Per meeting, its audio rows to delete.
    pub meetings: BTreeMap<EntityId, Vec<AudioFile>>,
    /// Meetings with audio whose retention clock could not be read (their audio is kept).
    pub unreadable_clocks: usize,
}

/// The one retention rule: every audio row of a meeting whose retention clock started at
/// least the policy's time before `now_unix_ms`. Nothing is deleted; [`run_retention`] and a
/// settings preview both use this.
///
/// # Errors
/// SQLite errors.
pub fn due_audio(
    store: &Store,
    policy: RetentionPolicy,
    now_unix_ms: u64,
) -> Result<DueAudio, StoreError> {
    let mut due = DueAudio::default();
    let RetentionPolicy::KeepFor { ms } = policy else {
        return Ok(due);
    };
    let mut by_meeting: BTreeMap<EntityId, Vec<AudioFile>> = BTreeMap::new();
    for f in store.all_audio_files()? {
        by_meeting.entry(f.meeting_id).or_default().push(f);
    }
    for (meeting, files) in by_meeting {
        match retention_clock(store, meeting)? {
            RetentionClock::Since(since) if now_unix_ms.saturating_sub(since) >= ms => {
                due.meetings.insert(meeting, files);
            }
            RetentionClock::Unreadable => due.unreadable_clocks += 1,
            RetentionClock::Since(_) | RetentionClock::NotStarted => {}
        }
    }
    Ok(due)
}

/// What [`remove_audio`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RemovedAudio {
    /// Files deleted.
    pub deleted_files: usize,
    /// Rows whose file was already gone (tolerated).
    pub missing_files: usize,
    /// Rows removed (deleted plus missing).
    pub rows_removed: usize,
}

/// Deletes each row's file, then the row (a crash in between leaves a row whose file is
/// missing, which every reader tolerates); then the emptied `audio/<meeting>` folders. Only
/// audio goes: transcripts, summaries, speakers and export records stay.
///
/// # Errors
/// SQLite errors, or an I/O error other than "not found" deleting a file (that row is kept
/// and the run stops).
pub fn remove_audio(
    store: &mut Store,
    data: &DataDir,
    files: &[AudioFile],
) -> Result<RemovedAudio, StoreError> {
    let mut report = RemovedAudio::default();
    let mut dirs = BTreeSet::new();
    let result = (|| {
        for f in files {
            let path = data.resolve(&f.path)?;
            match std::fs::remove_file(&path) {
                Ok(()) => report.deleted_files += 1,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => report.missing_files += 1,
                Err(e) => return Err(StoreError::from(e)),
            }
            store.delete_audio_file(f.id)?;
            report.rows_removed += 1;
            if let Some(dir) = path.parent() {
                dirs.insert(dir.to_owned());
            }
        }
        Ok(())
    })();
    for d in dirs {
        // Only succeeds when empty; anything else (another track, a .part file) stays.
        let _ = std::fs::remove_dir(d);
    }
    result.map(|()| report)
}

/// What a retention run did.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RetentionReport {
    /// Files deleted.
    pub deleted_files: usize,
    /// Selected rows whose file was already gone (tolerated).
    pub missing_files: usize,
    /// Rows removed (deleted plus missing).
    pub rows_removed: usize,
    /// Rows kept.
    pub kept: usize,
    /// Meetings with audio whose retention clock could not be read (kept).
    pub unreadable_clocks: usize,
    /// Audio files without a row (`.flac`, `.flac.part`) older than [`ORPHAN_GRACE_MS`],
    /// removed.
    pub orphans_removed: usize,
}

/// Applies `policy` at `now_unix_ms`: the rows [`due_audio`] selects are removed with
/// [`remove_audio`]. Then audio files no row refers to are swept ([`sweep_orphan_audio`]
/// with [`ORPHAN_GRACE_MS`], under every policy: no row means the app cannot reach them).
/// A host whose jobs may be using a meeting's audio removes per meeting instead, skipping
/// those meetings (`earshot_runtime::retention`).
///
/// # Errors
/// SQLite errors, or an I/O error other than "not found" deleting a file (that row is kept
/// and the run stops).
pub fn run_retention(
    store: &mut Store,
    data: &DataDir,
    policy: RetentionPolicy,
    now_unix_ms: u64,
) -> Result<RetentionReport, StoreError> {
    let total = store.all_audio_files()?.len();
    let due = due_audio(store, policy, now_unix_ms)?;
    let files: Vec<AudioFile> = due.meetings.into_values().flatten().collect();
    let removed = remove_audio(store, data, &files)?;
    Ok(RetentionReport {
        deleted_files: removed.deleted_files,
        missing_files: removed.missing_files,
        rows_removed: removed.rows_removed,
        kept: total.saturating_sub(removed.rows_removed),
        unreadable_clocks: due.unreadable_clocks,
        orphans_removed: sweep_orphan_audio(store, data, now_unix_ms, ORPHAN_GRACE_MS)?,
    })
}

/// Modification time of `path` in Unix ms (`None` if unknown).
pub(crate) fn mtime_ms(path: &std::path::Path) -> Option<u64> {
    let t = std::fs::metadata(path).ok()?.modified().ok()?;
    let d = t.duration_since(std::time::UNIX_EPOCH).ok()?;
    u64::try_from(d.as_millis()).ok()
}

/// Whether a file name is one of the audio files the store writes.
pub(crate) fn is_audio_name(name: &str) -> bool {
    name.ends_with(".flac") || name.ends_with(".flac.part")
}

/// Removes `audio/<dir>/*.flac` and `*.flac.part` files that no `AudioFile` row refers to and
/// that were last modified at least `grace_ms` before `now_unix_ms`; emptied folders go too.
/// Returns how many files were removed.
///
/// # Errors
/// SQLite errors; I/O errors other than "not found" removing a file.
pub fn sweep_orphan_audio(
    store: &Store,
    data: &DataDir,
    now_unix_ms: u64,
    grace_ms: u64,
) -> Result<usize, StoreError> {
    let mut known = BTreeSet::new();
    for f in store.all_audio_files()? {
        known.insert(data.resolve(&f.path)?);
    }
    let Ok(dirs) = std::fs::read_dir(data.root().join("audio")) else {
        return Ok(0);
    };
    let mut removed = 0;
    for d in dirs.filter_map(Result::ok) {
        if !d.file_type().is_ok_and(|t| t.is_dir()) {
            continue;
        }
        let Ok(files) = std::fs::read_dir(d.path()) else {
            continue;
        };
        for f in files.filter_map(Result::ok) {
            let p = f.path();
            let name = f.file_name().to_string_lossy().into_owned();
            if !f.file_type().is_ok_and(|t| t.is_file())
                || !is_audio_name(&name)
                || known.contains(&p)
            {
                continue;
            }
            if !mtime_ms(&p).is_some_and(|m| now_unix_ms.saturating_sub(m) >= grace_ms) {
                continue;
            }
            match std::fs::remove_file(&p) {
                Ok(()) => removed += 1,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e.into()),
            }
        }
        // Only succeeds when empty.
        let _ = std::fs::remove_dir(d.path());
    }
    Ok(removed)
}

/// A meeting's stored audio for re-transcription.
#[derive(Debug, Clone, PartialEq)]
pub struct MeetingAudio {
    /// Tracks whose file is present, with the decoded samples.
    pub tracks: Vec<(Track, AudioFile, Vec<f32>)>,
    /// Rows whose file is missing (deleted by retention or by hand): skipped, not an error.
    pub missing: Vec<AudioFile>,
}

/// Loads every audio file of `meeting` for re-transcription, tolerating missing files.
///
/// # Errors
/// SQLite errors, a row whose track is missing, or a present file that cannot be decoded.
pub fn load_meeting_audio(
    store: &Store,
    data: &DataDir,
    meeting: EntityId,
) -> Result<MeetingAudio, StoreError> {
    let tracks: BTreeMap<EntityId, Track> = store
        .tracks(meeting)?
        .into_iter()
        .map(|t| (t.id, t))
        .collect();
    let mut out = MeetingAudio {
        tracks: Vec::new(),
        missing: Vec::new(),
    };
    for f in store.audio_files(meeting)? {
        let track = tracks
            .get(&f.track_id)
            .cloned()
            .ok_or(RepoError::MissingParent {
                entity: "track",
                id: f.track_id,
            })?;
        let path = data.resolve(&f.path)?;
        if !path.exists() {
            out.missing.push(f);
            continue;
        }
        let (_, samples) = read_flac(&path)?;
        out.tracks.push((track, f, samples));
    }
    out.tracks.sort_by_key(|(t, _, _)| (t.kind, t.id));
    Ok(out)
}
