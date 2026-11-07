//! The live speech stretches kept for the final pass (T6.13c), device-locally (never
//! synced, no migration): per track, what a live session's stretch worker decoded
//! ([`StretchRecord`]) so the final pass can take those words instead of transcribing the
//! track again.
//!
//! A record says what the stretches cover and how they were made:
//! - the pieces exactly as decoded (raw model segments in meeting time, before the live
//!   echo filter or any other host filtering), with the pieces dropped as loops;
//! - the decoded frontier: every speech piece before it is among the pieces;
//! - the backend that decoded them ([`earshot_core::asr::BackendInfo`]: its exact model,
//!   and the kind and device, reported as the pass backend when the final pass decodes
//!   nothing itself), the decoding options, the piece settings, and the VAD's identity
//!   ([`earshot_core::asr::Vad::id`]) and frame length;
//! - why live transcription stopped early, if it did (a failed or cancelled model, a
//!   crashed worker, a queue over its cap); the frontier then is where it stopped.
//!
//! The stored value is `version (u32 LE) | payload (JSON)`, with the version outside the
//! payload, so a record of another version or with a damaged payload is recognised
//! without parsing it and comes back as [`StoredStretches::Unreadable`], never as an error:
//! the final pass then transcribes that track whole and warns. Records of version 1 (plain
//! JSON with the version inside, written before 2026-10-24) are read as unreadable: a
//! meeting whose cleanup was still pending when this build was installed is transcribed
//! whole once, which is the result it would have had without stretches.
//!
//! It is written once, when the session ends. A session without stretches (the live
//! chunker, no live transcription) or an older meeting has none, and the final pass
//! transcribes those tracks whole. The final pass reuses a record only when it would
//! transcribe that track the same way (`crate::post`, step 1).
//!
//! **The record is transcript data** (the raw words, including echo words the live filter
//! removed), so it is transient: it exists only between the end of the session and the
//! final pass, which deletes it in its write transaction on every path and checkpoints
//! the WAL (`earshot_store::Store::flush_erasures`). A failed or cancelled final pass keeps
//! it for a retry. A future meeting deletion must also sweep `live-stretches/<meeting>/*`
//! (and `live-speakers/<meeting>/*`, [`crate::speakers`]).

use earshot_core::asr::{BackendInfo, DecodedPieces, PieceConfig, TranscribeOptions};
use earshot_core::capture::TrackId;
use earshot_core::identity::EntityId;
use earshot_store::Store;
use serde::{Deserialize, Serialize};

use crate::RuntimeError;
use crate::local::{LocalKey, MeetingRecord};

/// The format of the stored record; a record of another version is not reused.
pub const STRETCH_RECORD_VERSION: u32 = 2;

/// What a track's live speech stretches decoded (module docs).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StretchRecord {
    /// The backend that decoded the stretches, as the transcriber reported it after its
    /// last decode (`LiveTrack::stretches_backend`: a per-call GPU fallback reports the
    /// CPU); the start-time backend when nothing was decoded.
    pub backend: BackendInfo,
    /// The decoding options of every piece.
    pub options: TranscribeOptions,
    /// The piece settings.
    pub pieces: PieceConfig,
    /// The VAD's identity ([`earshot_core::asr::Vad::id`]).
    pub vad: String,
    /// The VAD's frame length (samples).
    pub vad_frame: usize,
    /// The decoded pieces and the frontier (samples at the pipeline rate from meeting time
    /// zero).
    pub decoded: DecodedPieces,
    /// Why live transcription of the track stopped before the end of the meeting; `None`
    /// when it finished.
    pub stopped: Option<String>,
}

/// A stored record as read back: usable, or why not.
#[derive(Debug, Clone, PartialEq)]
pub enum StoredStretches {
    /// The record.
    Record(Box<StretchRecord>),
    /// A record of another version or that does not parse.
    Unreadable(String),
}

/// The local_state key of `track`'s record in `meeting`.
pub(crate) fn record_key(meeting: EntityId, track: TrackId) -> LocalKey {
    let track = match track {
        TrackId::Mic => "mic",
        TrackId::Reference => "reference",
    };
    MeetingRecord::LiveStretches.key(&[], meeting, &[track])
}

/// The stored form of `record` (module docs).
///
/// # Errors
/// [`RuntimeError::Internal`] if the record does not serialise (never expected).
pub fn encode(record: &StretchRecord) -> Result<Vec<u8>, RuntimeError> {
    let payload = serde_json::to_vec(record)
        .map_err(|_| RuntimeError::Internal("a stretch record does not serialise"))?;
    let mut out = Vec::with_capacity(4 + payload.len());
    out.extend_from_slice(&STRETCH_RECORD_VERSION.to_le_bytes());
    out.extend_from_slice(&payload);
    Ok(out)
}

/// Reads [`encode`]: the record, or why it cannot be used.
#[must_use]
pub fn decode(bytes: &[u8]) -> StoredStretches {
    // Version 1 was the JSON object alone (module docs).
    if bytes.first() == Some(&b'{') {
        return StoredStretches::Unreadable(format!(
            "stretch record version 1, this build reads {STRETCH_RECORD_VERSION}"
        ));
    }
    let Some(head) = bytes.get(..4) else {
        return StoredStretches::Unreadable("the stretch record is truncated".into());
    };
    let version = u32::from_le_bytes([head[0], head[1], head[2], head[3]]);
    if version != STRETCH_RECORD_VERSION {
        return StoredStretches::Unreadable(format!(
            "stretch record version {version}, this build reads {STRETCH_RECORD_VERSION}"
        ));
    }
    match serde_json::from_slice(&bytes[4..]) {
        Ok(record) => StoredStretches::Record(Box::new(record)),
        Err(e) => StoredStretches::Unreadable(format!("the stretch record: {e}")),
    }
}

/// Stores `record` for `track` of `meeting` (replacing an earlier one).
///
/// # Errors
/// Store errors.
pub fn record_stretches(
    store: &mut Store,
    meeting: EntityId,
    track: TrackId,
    record: &StretchRecord,
) -> Result<(), RuntimeError> {
    crate::local::set(store, &record_key(meeting, track), &encode(record)?)?;
    Ok(())
}

/// The record stored by [`record_stretches`], if any; a value that cannot be read comes
/// back as [`StoredStretches::Unreadable`], never as an error.
///
/// # Errors
/// Store errors.
pub fn stretch_record(
    store: &Store,
    meeting: EntityId,
    track: TrackId,
) -> Result<Option<StoredStretches>, RuntimeError> {
    Ok(store
        .local_value(record_key(meeting, track).as_str())?
        .map(|v| decode(&v)))
}

#[cfg(test)]
mod tests {
    use earshot_core::asr::{BackendKind, RawSegment, Span, StreamPiece, Word};

    use super::*;

    fn record() -> StretchRecord {
        StretchRecord {
            backend: BackendInfo {
                kind: BackendKind::Cuda,
                device: "gpu".into(),
                model: "ggml-base.en".into(),
            },
            options: TranscribeOptions::batch(),
            pieces: PieceConfig::default(),
            vad: "silero-vad@0123456789ab".into(),
            vad_frame: 512,
            decoded: DecodedPieces {
                pieces: vec![StreamPiece {
                    span: Span {
                        start: 16_000,
                        end: 32_000,
                    },
                    segments: vec![RawSegment::from_words(vec![Word::new("hi", 0, 300, 0.9)])],
                    dropped: false,
                    retried: false,
                    uncovered: false,
                    language: Some("en".into()),
                }],
                through: 40_000,
            },
            stopped: None,
        }
    }

    #[test]
    fn stretches_record_round_trips_with_its_version_outside_the_payload() {
        let r = record();
        let bytes = encode(&r).unwrap();
        assert_eq!(bytes[..4], STRETCH_RECORD_VERSION.to_le_bytes());
        assert_eq!(decode(&bytes), StoredStretches::Record(Box::new(r)));
    }

    #[test]
    fn stretches_record_of_another_version_or_damaged_is_unreadable_not_an_error() {
        let mut bytes = encode(&record()).unwrap();
        // A future version, even with a payload this build could parse.
        bytes[..4].copy_from_slice(&(STRETCH_RECORD_VERSION + 1).to_le_bytes());
        let StoredStretches::Unreadable(why) = decode(&bytes) else {
            panic!("another version must not be read");
        };
        assert!(why.contains("version 3"), "{why}");
        // A damaged payload.
        let mut bytes = encode(&record()).unwrap();
        bytes.truncate(bytes.len() - 3);
        assert!(matches!(decode(&bytes), StoredStretches::Unreadable(_)));
        // A payload whose fields this build does not know.
        let mut bytes = STRETCH_RECORD_VERSION.to_le_bytes().to_vec();
        bytes.extend_from_slice(br#"{"something":"else"}"#);
        assert!(matches!(decode(&bytes), StoredStretches::Unreadable(_)));
        // Too short for a version.
        assert!(matches!(decode(&[2, 0]), StoredStretches::Unreadable(_)));
    }

    #[test]
    fn stretches_record_from_before_uncovered_speech_recovery_reads_but_does_not_match() {
        // T2.5d: a version 2 record stored before `PieceConfig::recover_speech` and
        // `StreamPiece::uncovered` existed. It still reads (both default), with the setting
        // off, so the final pass's piece-settings check (`post::reusable`, "the stretches
        // used other piece settings") refuses it under today's default instead of reusing
        // stretches that were decoded without the recovery; that refusal itself is tested
        // through `post::reusable` in
        // `post_stretch_record_from_before_uncovered_speech_recovery_is_refused_by_reusable`.
        let mut json = serde_json::to_value(record()).unwrap();
        json["pieces"]
            .as_object_mut()
            .unwrap()
            .remove("recover_speech")
            .unwrap();
        json["decoded"]["pieces"][0]
            .as_object_mut()
            .unwrap()
            .remove("uncovered")
            .unwrap();
        let mut bytes = STRETCH_RECORD_VERSION.to_le_bytes().to_vec();
        bytes.extend_from_slice(&serde_json::to_vec(&json).unwrap());
        let StoredStretches::Record(old) = decode(&bytes) else {
            panic!("an older version 2 record must still read");
        };
        assert_eq!(old.pieces.recover_speech, None);
        assert!(!old.decoded.pieces[0].uncovered);
        assert_ne!(old.pieces, PieceConfig::default());
        assert!(PieceConfig::default().recover_speech.is_some());
    }

    #[test]
    fn stretches_record_of_version_1_is_unreadable() {
        // As the first format stored it: the JSON object with the version inside.
        let v1 = br#"{"version":1,"model":"ggml-base.en","options":{},"pieces":{},"vad_frame":512,"decoded":{"pieces":[],"through":0},"stopped":null}"#;
        let StoredStretches::Unreadable(why) = decode(v1) else {
            panic!("a version 1 record must not be read");
        };
        assert!(why.contains("version 1"), "{why}");
    }
}
