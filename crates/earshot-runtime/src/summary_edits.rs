//! Which of a meeting's summaries hold text the user edited (T12.43), so the meeting header
//! can say so.
//!
//! A summary edit ([`crate::edit::edit_summary`]) changes the current row in place and keeps
//! the text before the edit as a tombstone copy with a new id, and undo swaps the two
//! ([`crate::edit::undo_summary`]). The rows alone cannot tell the writer's text from an
//! edited one after an undo, so the edit records it: the device-local record
//! `summary-edits/<meeting>` ([`crate::local::MeetingRecord::SummaryEdits`]) is
//! `version u32 LE | JSON list` of the ids of the meeting's summary rows whose text the user
//! changed. An edit adds the edited row, and the copy of the text before it when that text was
//! itself edited; undo changes nothing here, because each flag travels with its row. Ids of
//! rows the meeting no longer has are dropped at the next edit.
//!
//! The record holds row ids only, no text. It is loss-tolerant: a record this version cannot
//! read is no edits, never an error (losing it loses only the label, never a summary), and the
//! next edit replaces it. Meeting deletion erases it with the meeting's other records. It is
//! device-local: a future sync carries the rows, not this label.

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
    MeetingRecord::SummaryEdits.key(&[], meeting, &[])
}

fn encode(ids: &BTreeSet<EntityId>) -> Result<Vec<u8>, RuntimeError> {
    let mut out = RECORD_VERSION.to_le_bytes().to_vec();
    let text: Vec<String> = ids.iter().map(ToString::to_string).collect();
    serde_json::to_writer(&mut out, &text)
        .map_err(|_| RuntimeError::Internal("summary edits encoding"))?;
    Ok(out)
}

/// The ids of a stored record; one this version cannot read is no ids.
fn decode(bytes: &[u8]) -> BTreeSet<EntityId> {
    let Some((head, body)) = bytes.split_first_chunk::<4>() else {
        return BTreeSet::new();
    };
    if u32::from_le_bytes(*head) != RECORD_VERSION {
        return BTreeSet::new();
    }
    serde_json::from_slice::<Vec<String>>(body)
        .unwrap_or_default()
        .iter()
        .filter_map(|s| s.parse().ok())
        .collect()
}

/// Records inside `tx` that row `edited` of `meeting` now holds edited text and that
/// `before`, the copy of the text before the edit, holds edited text exactly when the row did
/// before this edit (module doc).
///
/// # Errors
/// Store errors.
pub(crate) fn record_edit_tx(
    tx: &mut earshot_store::StoreTx<'_>,
    meeting: EntityId,
    edited: EntityId,
    before: EntityId,
) -> Result<(), RuntimeError> {
    let key = record_key(meeting);
    let mut ids = tx
        .local_value(key.as_str())?
        .map(|b| decode(&b))
        .unwrap_or_default();
    if ids.contains(&edited) {
        ids.insert(before);
    }
    ids.insert(edited);
    let rows: BTreeSet<EntityId> = tx.summaries(meeting)?.iter().map(|s| s.id).collect();
    ids.retain(|id| rows.contains(id));
    crate::local::set_tx(tx, &key, &encode(&ids)?)?;
    Ok(())
}

/// Whether summary row `summary` of `meeting` holds text the user edited. A record this
/// version cannot read counts as no edits.
///
/// # Errors
/// Store errors.
pub fn is_edited(
    store: &Store,
    meeting: EntityId,
    summary: EntityId,
) -> Result<bool, RuntimeError> {
    Ok(store
        .local_value(record_key(meeting).as_str())?
        .is_some_and(|b| decode(&b).contains(&summary)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn summary_edits_record_round_trips_and_refuses_another_version() {
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
}
