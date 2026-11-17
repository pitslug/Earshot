//! Which backend ran a transcript pass (device-local provenance): written by the live session
//! and the final pass, read by the meeting's activity. Outside [`crate::backends`] so a build
//! without the in-process engines (feature `engines`, T11.6b) still reads it; `backends`
//! re-exports it under its old paths.

use earshot_core::asr::BackendInfo;
use earshot_core::capture::TrackId;
use earshot_core::identity::EntityId;
use earshot_core::model::TranscriptPass;
use earshot_store::Store;

use crate::RuntimeError;
use crate::local::{LocalKey, MeetingRecord};

pub(crate) fn pass_key(
    meeting: EntityId,
    pass: TranscriptPass,
    track: Option<TrackId>,
) -> LocalKey {
    let pass = match pass {
        TranscriptPass::Live => "live",
        TranscriptPass::Final => "final",
    };
    let track = match track {
        Some(TrackId::Mic) => "mic",
        Some(TrackId::Reference) => "reference",
        None => "all",
    };
    MeetingRecord::PassBackend.key(&[pass], meeting, &[track])
}

/// Records which backend ran a transcript pass of `meeting` (`track` for one track,
/// `None` for all), in the device-local state: reports must say which backend ran.
///
/// # Errors
/// Store errors.
pub fn record_pass_backend(
    store: &mut Store,
    meeting: EntityId,
    pass: TranscriptPass,
    track: Option<TrackId>,
    backend: &BackendInfo,
) -> Result<(), RuntimeError> {
    let value = serde_json::to_vec(backend)
        .map_err(|_| RuntimeError::Internal("backend info does not serialise"))?;
    crate::local::set(store, &pass_key(meeting, pass, track), &value)?;
    Ok(())
}

/// The backend recorded by [`record_pass_backend`], if any; a value this build cannot read is
/// `None`, as if nothing was recorded ([`pass_backend_record`] says which, T12.56).
///
/// # Errors
/// Store errors.
pub fn pass_backend(
    store: &Store,
    meeting: EntityId,
    pass: TranscriptPass,
    track: Option<TrackId>,
) -> Result<Option<BackendInfo>, RuntimeError> {
    Ok(pass_backend_record(store, meeting, pass, track)?.value())
}

/// The record behind [`pass_backend`]: absent, read, or stored but unreadable (a pass ran even
/// when its backend cannot be read).
///
/// # Errors
/// Store errors.
pub fn pass_backend_record(
    store: &Store,
    meeting: EntityId,
    pass: TranscriptPass,
    track: Option<TrackId>,
) -> Result<crate::StoredRecord<BackendInfo>, RuntimeError> {
    crate::local::json_record(store, &pass_key(meeting, pass, track))
}
