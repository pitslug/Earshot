//! Recovering recordings that were interrupted (T9.18).
//!
//! A live session finishes each track's FLAC file and sets the meeting's duration only at its
//! end, so a process that dies during a recording (killed, crashed, the machine lost power)
//! leaves a meeting with Track rows, no duration and no `AudioFile` rows: it reads
//! [`MeetingActivity::Interrupted`](crate::MeetingActivity::Interrupted), and its audio is in
//! `<track>.flac.part` files whose header is still the zeroed placeholder (or, when the end of
//! the session got as far as finishing a file but not its row, a finished file without a row).
//!
//! A session that ended can also leave one track's file unfinished: its finish panicked
//! (T9.19) or failed (T9.17's rename or row), and the meeting still got its duration.
//!
//! [`recover_interrupted`] runs once at the app's start, after the model folder's own
//! recovery and before audio retention's first run. Its candidates are the live (not deleted)
//! meetings with live Track rows that either have no duration or have a live track with
//! something left on disk: no `AudioFile` row and a `.part` or finished file at its layout
//! path, or a row and a `.part` beside its file. A meeting a job of this process holds (a live
//! session of this process first of all) is never touched. For each it:
//! - finishes each track's file with the whole frames it holds
//!   ([`earshot_store::recover_track_file`], no store lock held) and writes its `AudioFile`
//!   row ([`earshot_store::Store::adopt_track_audio`]);
//! - removes a `.part` left beside a finished file once the file and its row agree
//!   ([`earshot_store::remove_stale_part`]); an empty finished file is never given a row;
//! - for a meeting that already has its duration, stops there: the duration is kept and no
//!   notice is stored (the session told the user when it ended);
//! - otherwise, when at least one track now has a file, sets the duration from the shortest
//!   track and
//!   stores a notice (`recovered-recording/<meeting>`, [`MeetingRecord::RecoveredRecording`]:
//!   `version u32 LE | saved ms u64 LE`, no content) for the app to show once
//!   ([`take_notices`]). The meeting then reads Unprocessed: the user can re-transcribe it.
//!   Its retention clock is not started (no final transcript), so its audio is kept.
//! - when no track has a whole frame, leaves the meeting as it is (Interrupted, nothing to
//!   re-transcribe) and the files untouched; a later start looks again.
//!
//! The meeting is held with a [`MeetingJob::Processing`](crate::MeetingJob::Processing) lease
//! while it is recovered, so nothing in this process deletes or re-transcribes it meanwhile. A
//! `.part` another process holds open (a CLI recording on the same data folder) is left alone.

use earshot_core::capture::TrackId;
use earshot_core::identity::EntityId;
use earshot_core::model::{AudioFileRepo, MeetingRepo, Track};
use earshot_store::{AudioProvenance, TrackFileRecovery};

use crate::local::{LocalKey, MeetingRecord};
use crate::{MeetingJob, RuntimeError, Services};

/// The notice record's version, written before its payload.
pub const NOTICE_VERSION: u32 = 1;

/// One meeting [`recover_interrupted`] gave its audio back to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RecoveredMeeting {
    /// The meeting.
    pub meeting: EntityId,
    /// How much was saved: the shortest recovered track, in ms (the meeting's new duration).
    pub saved_ms: u64,
}

/// What [`recover_interrupted`] did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RecoveryReport {
    /// Meetings whose audio is back, with their new duration.
    pub recovered: Vec<RecoveredMeeting>,
    /// Interrupted meetings with no whole frame of audio on any track (left Interrupted), and
    /// meetings with a duration whose leftover files hold nothing to finish.
    pub nothing_saved: Vec<EntityId>,
    /// Meetings that already had their duration whose leftover track files were finished or
    /// tidied (rows written, stale `.part` files removed); their duration is unchanged.
    pub leftovers_finished: Vec<EntityId>,
    /// Interrupted meetings held by a job of this process, or whose audio another process
    /// holds open: not touched.
    pub skipped: Vec<EntityId>,
    /// Meetings whose recovery failed (an I/O or store error): left for a later start. The
    /// error text stays in the process (local paths), it is never shown.
    pub failed: Vec<(EntityId, String)>,
    /// The list of meetings could not be read (nothing was recovered).
    pub list_failed: Option<String>,
}

/// The notice record's key for `meeting`.
pub(crate) fn notice_key(meeting: EntityId) -> LocalKey {
    MeetingRecord::RecoveredRecording.key(&[], meeting, &[])
}

fn encode_notice(saved_ms: u64) -> Vec<u8> {
    let mut v = NOTICE_VERSION.to_le_bytes().to_vec();
    v.extend_from_slice(&saved_ms.to_le_bytes());
    v
}

fn decode_notice(bytes: &[u8]) -> Option<u64> {
    let (version, rest) = bytes.split_first_chunk::<4>()?;
    let saved: [u8; 8] = rest.try_into().ok()?;
    (u32::from_le_bytes(*version) == NOTICE_VERSION).then(|| u64::from_le_bytes(saved))
}

/// Recovers every interrupted meeting (module docs). Never fails as a whole: each meeting's
/// outcome is in the report.
pub fn recover_interrupted(services: &Services) -> RecoveryReport {
    let mut report = RecoveryReport::default();
    let candidates = match interrupted_meetings(services) {
        Ok(c) => c,
        Err(e) => {
            report.list_failed = Some(e.to_string());
            return report;
        }
    };
    for (meeting, tracks) in candidates {
        // Held by a job of this process (a live session first of all): never touched. The
        // lease keeps deletions and re-transcriptions off it while it is recovered.
        if services.leased_meetings().contains(&meeting) {
            report.skipped.push(meeting);
            continue;
        }
        let Ok(_lease) = services.lease_meeting_alone(meeting, MeetingJob::Processing) else {
            report.skipped.push(meeting);
            continue;
        };
        match recover_meeting(services, meeting, &tracks) {
            Ok(Outcome::Recovered(saved_ms)) => report
                .recovered
                .push(RecoveredMeeting { meeting, saved_ms }),
            Ok(Outcome::NothingSaved) => report.nothing_saved.push(meeting),
            Ok(Outcome::LeftoversFinished) => report.leftovers_finished.push(meeting),
            Ok(Outcome::InUse | Outcome::Changed) => report.skipped.push(meeting),
            Err(e) => report.failed.push((meeting, e.to_string())),
        }
    }
    report
}

/// The candidates (module docs), with their live tracks. The store is read under one lock;
/// the files are looked at with it free.
fn interrupted_meetings(services: &Services) -> Result<Vec<(EntityId, Vec<Track>)>, RuntimeError> {
    let mut listed = Vec::new();
    {
        let db = services.store();
        for m in db.meetings()? {
            if m.meta.deleted {
                continue;
            }
            let tracks: Vec<Track> = db
                .tracks(m.id)?
                .into_iter()
                .filter(|t| !t.meta.deleted)
                .collect();
            if tracks.is_empty() {
                continue;
            }
            let rows = db.audio_files(m.id)?;
            let with_row: Vec<bool> = tracks
                .iter()
                .map(|t| rows.iter().any(|f| f.track_id == t.id))
                .collect();
            listed.push((m.id, m.duration_ms.is_none(), tracks, with_row));
        }
    }
    let mut out = Vec::new();
    for (meeting, no_duration, tracks, with_row) in listed {
        let left = no_duration
            || tracks
                .iter()
                .zip(&with_row)
                .any(|(t, &row)| leftover_on_disk(services, meeting, t.kind, row));
        if left {
            out.push((meeting, tracks));
        }
    }
    Ok(out)
}

/// Whether `kind`'s layout path holds something to finish or tidy: a `.part`, or a finished file
/// without its row. An unresolvable path counts as nothing (recovery would refuse it too).
fn leftover_on_disk(services: &Services, meeting: EntityId, kind: TrackId, has_row: bool) -> bool {
    let Ok(path) = services
        .data
        .resolve(&earshot_store::DataDir::audio_rel(meeting, kind))
    else {
        return false;
    };
    let mut part = path.as_os_str().to_owned();
    part.push(".part");
    std::path::Path::new(&part).exists() || (!has_row && path.exists())
}

enum Outcome {
    Recovered(u64),
    NothingSaved,
    /// A meeting with its duration whose leftovers were finished or tidied.
    LeftoversFinished,
    InUse,
    /// The meeting was deleted or got its duration while it was recovered.
    Changed,
}

fn recover_meeting(
    services: &Services,
    meeting: EntityId,
    tracks: &[Track],
) -> Result<Outcome, RuntimeError> {
    let has_reference = tracks.iter().any(|t| t.kind == TrackId::Reference);
    let mut in_use = false;
    let mut changed = false;
    for track in tracks {
        let row = services
            .store()
            .audio_files(meeting)?
            .into_iter()
            .find(|f| f.track_id == track.id);
        if let Some(row) = row {
            changed |= earshot_store::remove_stale_part(&services.data, &row)?;
            continue;
        }
        // File work runs with the store free (it can decode a long recording).
        let found = earshot_store::recover_track_file(
            &services.data,
            meeting,
            track.kind,
            earshot_core::audio::PIPELINE_RATE.hz(),
        )?;
        if found == TrackFileRecovery::InUse {
            in_use = true;
        }
        // An empty finished file is never given a row: the meeting would read Interrupted
        // with a row, and nothing in it can be transcribed.
        let Some(file) = found.finished().filter(|f| f.frames > 0) else {
            continue;
        };
        let provenance = AudioProvenance {
            pipeline_version: crate::live::PIPELINE_VERSION.to_owned(),
            aec_version: (track.kind == TrackId::Mic && has_reference)
                .then(|| crate::live::AEC_VERSION.to_owned()),
        };
        let mut rng = crate::services::os_seeded_rng();
        let clock = std::sync::Arc::clone(&services.clock);
        let row = services.store().adopt_track_audio(
            file,
            &services.data,
            track,
            provenance,
            &*clock,
            &mut *rng,
        )?;
        changed = true;
        // A finished file found beside a `.part`: the part is stale once the row exists.
        earshot_store::remove_stale_part(&services.data, &row)?;
    }
    if in_use {
        return Ok(Outcome::InUse);
    }
    let clock = std::sync::Arc::clone(&services.clock);
    let mut db = services.store();
    // A meeting the session saved keeps its duration: only its leftovers were for us.
    match db.meeting(meeting)? {
        None => return Ok(Outcome::Changed),
        Some(m) if m.meta.deleted => return Ok(Outcome::Changed),
        Some(m) if m.duration_ms.is_some() => {
            return Ok(if changed {
                Outcome::LeftoversFinished
            } else {
                Outcome::NothingSaved
            });
        }
        Some(_) => {}
    }
    let saved_ms = db
        .audio_files(meeting)?
        .iter()
        .filter(|f| f.sample_rate_hz > 0 && f.frames > 0)
        .map(|f| f.frames.saturating_mul(1000) / u64::from(f.sample_rate_hz))
        .min();
    let Some(saved_ms) = saved_ms else {
        return Ok(Outcome::NothingSaved);
    };
    // Re-read: only the duration changes, edits made since are kept.
    let Some(mut m) = db.meeting(meeting)? else {
        return Ok(Outcome::Changed);
    };
    if m.meta.deleted || m.duration_ms.is_some() {
        return Ok(Outcome::Changed);
    }
    let (store, hlc) = db.parts();
    m.duration_ms = Some(saved_ms);
    m.meta.touch(hlc, &*clock);
    store.transaction(|tx| -> Result<(), RuntimeError> {
        tx.put_meeting(m)?;
        crate::local::set_tx(tx, &notice_key(meeting), &encode_notice(saved_ms))?;
        Ok(())
    })?;
    Ok(Outcome::Recovered(saved_ms))
}

/// A recording the app has not yet told the user about ([`take_notices`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RecoveryNotice {
    /// The meeting.
    pub meeting: EntityId,
    /// How much was saved, in ms.
    pub saved_ms: u64,
}

/// The notices [`recover_interrupted`] stored, deleted as they are read, so each is shown
/// once. A notice whose meeting is gone, or that this version cannot read, is deleted
/// without being returned.
///
/// # Errors
/// Store errors (nothing is deleted then).
pub fn take_notices(services: &Services) -> Result<Vec<RecoveryNotice>, RuntimeError> {
    let mut db = services.store();
    let prefix = MeetingRecord::RecoveredRecording.prefix();
    let keys: Vec<String> = db
        .local_keys()?
        .into_iter()
        .filter(|k| k.starts_with(prefix))
        .collect();
    let mut out = Vec::new();
    for key in keys {
        let notice = match (
            crate::local::meeting_of(&key),
            db.local_value(&key)?.as_deref().and_then(decode_notice),
        ) {
            (Some(meeting), Some(saved_ms))
                if db.meeting(meeting)?.is_some_and(|m| !m.meta.deleted) =>
            {
                Some(RecoveryNotice { meeting, saved_ms })
            }
            _ => None,
        };
        db.delete_local_value(&key)?;
        out.extend(notice);
    }
    out.sort_by_key(|n| n.meeting);
    Ok(out)
}

#[cfg(test)]
#[path = "recover_tests.rs"]
mod tests;
