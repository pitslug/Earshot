//! Which final transcript lines diarisation left without a speaker (T12.65), so they are named
//! [`earshot_core::diar::UNKNOWN_SPEAKER`] and the user can give them one.
//!
//! A segment row with no speaker cannot say why: a track with no speaker turns at all leaves
//! every line without one (named by the track, "Me" or "Them", as before T12.65), and a kept
//! edited line may sit beside them. So the final pass records the lines it left unassigned
//! (`label_segment_speakers_unassigned`) in the device-local record `unknown-lines/<meeting>`
//! ([`crate::local::MeetingRecord::UnknownLines`]): `version u32 LE | JSON list` of segment ids.
//! Each final pass rewrites it in its write transaction: the ids of this run's unassigned
//! lines plus the earlier ids that are still live final rows (lines it kept, such as edited
//! ones). A line given a speaker (`crate::edit::assign_line_speaker`) has a speaker, so its id
//! no longer matters.
//!
//! The record holds row ids only, no text. It is loss-tolerant: a record this version cannot
//! read is no lines, so those lines are named by their track as before, never an error. Meeting
//! deletion erases it with the meeting's other records. It is device-local: on another device
//! the same lines are named by their track.

use std::collections::BTreeSet;

use earshot_core::identity::EntityId;
use earshot_core::model::MeetingRepo;
use earshot_store::Store;

use crate::RuntimeError;
use crate::local::{LocalKey, MeetingRecord};

/// The record's version, written before its JSON payload.
pub const RECORD_VERSION: u32 = 1;

/// The record's key for `meeting`.
pub(crate) fn record_key(meeting: EntityId) -> LocalKey {
    MeetingRecord::UnknownLines.key(&[], meeting, &[])
}

/// The record's bytes for `ids`.
///
/// # Errors
/// [`RuntimeError::Internal`] if the list does not serialise.
pub fn encode(ids: &BTreeSet<EntityId>) -> Result<Vec<u8>, RuntimeError> {
    let text: Vec<String> = ids.iter().map(ToString::to_string).collect();
    earshot_store::encode_versioned(RECORD_VERSION, &text)
        .map_err(|_| RuntimeError::Internal("unknown lines encoding"))
}

/// The ids of a stored record; one this version cannot read is no ids.
fn decode(bytes: &[u8]) -> BTreeSet<EntityId> {
    let Some((version, body)) = earshot_store::split_version(bytes) else {
        return BTreeSet::new();
    };
    if version != RECORD_VERSION {
        return BTreeSet::new();
    }
    serde_json::from_slice::<Vec<String>>(body)
        .unwrap_or_default()
        .iter()
        .filter_map(|s| s.parse().ok())
        .collect()
}

/// Rewrites the record inside the final pass's transaction (module doc): `new` plus the
/// earlier ids still among the meeting's live final rows; none erases it.
///
/// # Errors
/// Store errors.
pub(crate) fn record_tx(
    tx: &mut earshot_store::StoreTx<'_>,
    meeting: EntityId,
    new: &BTreeSet<EntityId>,
) -> Result<(), RuntimeError> {
    let key = record_key(meeting);
    let mut ids = tx
        .local_value(key.as_str())?
        .map(|b| decode(&b))
        .unwrap_or_default();
    let live: BTreeSet<EntityId> = tx
        .segments(meeting)?
        .iter()
        .filter(|r| r.pass == earshot_core::model::TranscriptPass::Final && !r.meta.deleted)
        .map(|r| r.segment.id)
        .collect();
    ids.retain(|id| live.contains(id));
    ids.extend(new.iter().copied());
    if ids.is_empty() {
        tx.delete_local_value(key.as_str())?;
    } else {
        crate::local::set_tx(tx, &key, &encode(&ids)?)?;
    }
    Ok(())
}

/// The lines of `meeting` the final pass left without a speaker. A record this version cannot
/// read counts as none.
///
/// # Errors
/// Store errors.
pub fn read(store: &Store, meeting: EntityId) -> Result<BTreeSet<EntityId>, RuntimeError> {
    Ok(store
        .local_value(record_key(meeting).as_str())?
        .map(|b| decode(&b))
        .unwrap_or_default())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_lines_record_round_trips_and_refuses_another_version() {
        let ids: BTreeSet<EntityId> =
            [EntityId::from_bytes([1; 16]), EntityId::from_bytes([2; 16])]
                .into_iter()
                .collect();
        let bytes = encode(&ids).unwrap();
        assert_eq!(decode(&bytes), ids);
        let mut other = bytes.clone();
        other[0] = 9;
        assert!(decode(&other).is_empty());
        assert!(decode(b"ab").is_empty());
        assert!(decode(&[1, 0, 0, 0, b'{']).is_empty());
    }

    #[test]
    fn unknown_lines_record_bytes_are_pinned() {
        let ids: BTreeSet<EntityId> = [EntityId::from_bytes([1; 16])].into_iter().collect();
        let mut want = 1u32.to_le_bytes().to_vec();
        want.extend_from_slice(br#"["01010101-0101-0101-0101-010101010101"]"#);
        assert_eq!(encode(&ids).unwrap(), want);
    }
}
