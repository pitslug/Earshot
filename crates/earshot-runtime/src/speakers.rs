//! The live speaker windows kept for the final pass (T6.13d2), device-locally (never
//! synced, no migration): per track, the diarisation windows a live session's worker
//! segmented and embedded while the meeting ran
//! ([`earshot_core::diar::WindowResults`]), so the final pass only finishes the tail and
//! clusters ([`earshot_core::diar::Diarizer::diarize_from`]).
//!
//! The stored value is `version (u32 LE) | header length (u32 LE) | header (JSON) |
//! payload`, with the version and the header outside the payload, so a record of another
//! version or with a damaged payload is recognised without parsing it. A track whose live
//! speaker work was off from the start (the speaker detection setting, T6.13d4) gets the
//! reason-only form `0 (u32 LE) | reason (UTF-8)` instead (an empty or non-UTF-8 reason is
//! unreadable; [`SpeakerRecord`]s are never version 0): its models were never loaded, so
//! there are no windows and no window settings to record. The header names everything
//! that decides the windows: [`WindowParams`] (the segmentation model with the first 12
//! hex digits of its file's SHA-256, the embedding model id, the window grid, onset,
//! overlap rule and batch size) and the speaker models' ONNX Runtime thread count (CAM++
//! output changes with it, T6.13d2), plus why live speaker work stopped early, if it did.
//! The payload is [`WindowResults::to_bytes`].
//!
//! It is written once, when the session ends. The final pass reuses it only when the
//! header matches its own diarizer; otherwise, and for a record it cannot read, it
//! diarises the whole track and says why (`crate::post`, step 2). A record that cannot be
//! read is never an error.
//!
//! **The record is biometric data** (per-window speaker embeddings of everyone heard,
//! directly comparable with voiceprints, PLAN.md 3.9), so it is transient: it exists only
//! between the end of the session and the final pass, which deletes it in its write
//! transaction on every path (reused or not) and checkpoints the WAL
//! (`earshot_store::Store::flush_erasures`; `secure_delete` zeroes the pages). A failed or
//! cancelled final pass keeps it for a retry. A future meeting deletion must also sweep
//! `live-speakers/<meeting>/*` (and `live-stretches/<meeting>/*`, [`crate::stretches`]).

use earshot_core::capture::TrackId;
use earshot_core::diar::{WindowParams, WindowResults};
use earshot_core::identity::EntityId;
use earshot_store::Store;
use serde::{Deserialize, Serialize};

use crate::RuntimeError;
use crate::local::{LocalKey, MeetingRecord};

/// The format of the stored record; a record of another version is not reused.
pub const SPEAKER_RECORD_VERSION: u32 = 1;

/// What decides a track's live speaker windows, and why the work stopped early.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SpeakerHeader {
    /// The settings and models the windows were computed with.
    pub params: WindowParams,
    /// ONNX Runtime intra-op threads of the speaker models (`None` for models without a
    /// thread setting, such as test fakes).
    pub threads: Option<usize>,
    /// Why live speaker work stopped before the end of the meeting (it fell behind, a
    /// model failed); `None` when it ran to the end. The windows before it are still valid.
    pub stopped: Option<String>,
}

/// A track's live speaker windows (module docs).
#[derive(Debug, Clone, PartialEq)]
pub struct SpeakerRecord {
    /// What decides the windows.
    pub header: SpeakerHeader,
    /// The windows.
    pub results: WindowResults,
}

/// The version word of the reason-only form (module docs).
const OFF_TAG: u32 = 0;

/// A stored record as read back: usable, or why not.
#[derive(Debug, Clone, PartialEq)]
pub enum StoredSpeakers {
    /// The record.
    Record(Box<SpeakerRecord>),
    /// Live speaker work was off from the start, for this reason ([`record_speakers_off`]).
    Off(String),
    /// A record of another version or that does not parse.
    Unreadable(String),
}

/// The local_state key of `track`'s record in `meeting`.
pub(crate) fn record_key(meeting: EntityId, track: TrackId) -> LocalKey {
    let track = match track {
        TrackId::Mic => "mic",
        TrackId::Reference => "reference",
    };
    MeetingRecord::LiveSpeakers.key(&[], meeting, &[track])
}

/// The stored form of `record` (module docs).
#[must_use]
pub fn encode(record: &SpeakerRecord) -> Vec<u8> {
    // The header is plain data; serialising it cannot fail.
    let header = serde_json::to_vec(&record.header).unwrap_or_default();
    let mut out = Vec::with_capacity(8 + header.len());
    out.extend_from_slice(&SPEAKER_RECORD_VERSION.to_le_bytes());
    out.extend_from_slice(
        &u32::try_from(header.len())
            .unwrap_or(u32::MAX)
            .to_le_bytes(),
    );
    out.extend_from_slice(&header);
    out.extend_from_slice(&record.results.to_bytes());
    out
}

/// Reads [`encode`]: the record, or why it cannot be used.
#[must_use]
pub fn decode(bytes: &[u8]) -> StoredSpeakers {
    let word = |at: usize| -> Option<u32> {
        let b = bytes.get(at..at + 4)?;
        Some(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    };
    let Some(version) = word(0) else {
        return StoredSpeakers::Unreadable("the speaker record is truncated".into());
    };
    if version == OFF_TAG {
        // A reason is always written; an empty or non-UTF-8 one is a damaged value.
        return match std::str::from_utf8(&bytes[4..]) {
            Ok(why) if !why.is_empty() => StoredSpeakers::Off(why.to_owned()),
            _ => StoredSpeakers::Unreadable(
                "the speaker record's off reason is empty or not UTF-8".into(),
            ),
        };
    }
    if version != SPEAKER_RECORD_VERSION {
        return StoredSpeakers::Unreadable(format!(
            "speaker record version {version}, this build reads {SPEAKER_RECORD_VERSION}"
        ));
    }
    let Some(len) = word(4).and_then(|n| usize::try_from(n).ok()) else {
        return StoredSpeakers::Unreadable("the speaker record is truncated".into());
    };
    let Some(header) = bytes.get(8..8usize.saturating_add(len)) else {
        return StoredSpeakers::Unreadable("the speaker record is truncated".into());
    };
    let header: SpeakerHeader = match serde_json::from_slice(header) {
        Ok(h) => h,
        Err(e) => {
            return StoredSpeakers::Unreadable(format!("the speaker record header: {e}"));
        }
    };
    match WindowResults::from_bytes(&bytes[8 + len..]) {
        Ok(results) if *results.params() == header.params => {
            StoredSpeakers::Record(Box::new(SpeakerRecord { header, results }))
        }
        Ok(_) => {
            StoredSpeakers::Unreadable("the speaker record's header and windows disagree".into())
        }
        Err(e) => StoredSpeakers::Unreadable(format!("the speaker record's windows: {e}")),
    }
}

/// Stores `record` for `track` of `meeting` (replacing an earlier one).
///
/// # Errors
/// Store errors.
pub fn record_speakers(
    store: &mut Store,
    meeting: EntityId,
    track: TrackId,
    record: &SpeakerRecord,
) -> Result<(), RuntimeError> {
    crate::local::set(store, &record_key(meeting, track), &encode(record))?;
    Ok(())
}

/// Stores that `track`'s live speaker work in `meeting` was off from the start because of
/// `why` (module docs; replacing an earlier record).
///
/// # Errors
/// Store errors.
pub fn record_speakers_off(
    store: &mut Store,
    meeting: EntityId,
    track: TrackId,
    why: &str,
) -> Result<(), RuntimeError> {
    let mut value = OFF_TAG.to_le_bytes().to_vec();
    value.extend_from_slice(why.as_bytes());
    crate::local::set(store, &record_key(meeting, track), &value)?;
    Ok(())
}

/// The record stored by [`record_speakers`] or [`record_speakers_off`], if any; a value that cannot be read comes
/// back as [`StoredSpeakers::Unreadable`], never as an error.
///
/// # Errors
/// Store errors.
pub fn speaker_record(
    store: &Store,
    meeting: EntityId,
    track: TrackId,
) -> Result<Option<StoredSpeakers>, RuntimeError> {
    Ok(store
        .local_value(record_key(meeting, track).as_str())?
        .map(|v| decode(&v)))
}

/// Deletes the live speaker windows of every track of `meeting` and checkpoints the WAL,
/// for when no final pass will run (the user cancelled the cleanup): the windows hold
/// speaker embeddings and must not outlive the meeting's processing. Returns how many
/// records were deleted. A checkpoint blocked by another connection is not an error (the
/// deletion is committed; the store's closing checkpoint removes the bytes).
///
/// # Errors
/// Store errors.
pub fn forget_speakers(store: &mut Store, meeting: EntityId) -> Result<usize, RuntimeError> {
    let mut deleted = 0;
    for track in [TrackId::Mic, TrackId::Reference] {
        if store.delete_local_value(record_key(meeting, track).as_str())? {
            deleted += 1;
        }
    }
    match store.flush_erasures() {
        Err(e) if e.is_retryable() => {}
        other => other?,
    }
    Ok(deleted)
}

/// Overwrites the stored value with raw bytes (tests of damaged or older records).
#[cfg(test)]
pub(crate) fn store_raw(
    store: &mut Store,
    meeting: EntityId,
    track: TrackId,
    bytes: &[u8],
) -> Result<(), RuntimeError> {
    crate::local::set(store, &record_key(meeting, track), bytes)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn speakers_off_record_round_trips_and_is_never_a_window_record() {
        let dir = crate::testutil::TempDir::new("speakers-off");
        let mut store = Store::open(&dir.0.join("earshot.db")).unwrap();
        let meeting = EntityId::from_bytes([3; 16]);
        record_speakers_off(&mut store, meeting, TrackId::Mic, "set to after the call").unwrap();
        assert_eq!(
            speaker_record(&store, meeting, TrackId::Mic).unwrap(),
            Some(StoredSpeakers::Off("set to after the call".into()))
        );
        assert_eq!(
            speaker_record(&store, meeting, TrackId::Reference).unwrap(),
            None
        );
        // Deleted with the window records.
        assert_eq!(forget_speakers(&mut store, meeting).unwrap(), 1);
        assert_eq!(speaker_record(&store, meeting, TrackId::Mic).unwrap(), None);
        assert_ne!(SPEAKER_RECORD_VERSION, OFF_TAG);
    }

    #[test]
    fn speakers_off_record_with_an_empty_or_damaged_reason_is_unreadable() {
        assert!(matches!(
            decode(&[0, 0, 0, 0]),
            StoredSpeakers::Unreadable(_)
        ));
        assert!(matches!(
            decode(&[0, 0, 0, 0, 0xff]),
            StoredSpeakers::Unreadable(_)
        ));
        let mut ok = OFF_TAG.to_le_bytes().to_vec();
        ok.extend_from_slice("on battery".as_bytes());
        assert_eq!(decode(&ok), StoredSpeakers::Off("on battery".into()));
    }
}
