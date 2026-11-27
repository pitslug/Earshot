//! Whether this device has opened a meeting's current summary (T12.76), so the meeting list can
//! mark a summary the user has not read.
//!
//! The device-local record `summary-seen/<meeting>`
//! ([`crate::local::MeetingRecord::SummarySeen`]) is `version u32 LE | JSON string`: the id of
//! the summary row the meeting's detail last showed, written only by [`mark_seen`] (and once by
//! [`mark_existing_once`]). A meeting has a new summary ([`has_new_summary`]) when its current
//! summary (`MeetingRepo::current_summary_id`, the rule of
//! `earshot_core::model::current_summary`) is not that row. So a new summary row (a re-summary,
//! a summary that arrives by sync) is new again, while an edit in place keeps the row's id and is
//! not. Undoing a summary brings back an older row, which the detail that undid it shows at once.
//!
//! The record holds a row id only, no text. It is loss-tolerant: a record this version cannot
//! read counts as seen (no mark), because a mark that stays wrongly is worse than one missed, and
//! the next opening replaces it. Meeting deletion erases it with the meeting's other records. It
//! is device-local: each device says what it has opened.

use earshot_core::identity::EntityId;
use earshot_core::model::MeetingRepo;
use earshot_store::Store;

use crate::local::{DeviceRecord, LocalKey, MeetingRecord};
use crate::{RuntimeError, Services};

/// The record's version, written before its JSON payload.
pub const RECORD_VERSION: u32 = 1;

/// `local_state` key of the device record that says existing summaries were marked seen.
pub const SEEN_START_KEY: &str = "summary-seen-start";

/// The record's key for `meeting`.
pub(crate) fn record_key(meeting: EntityId) -> LocalKey {
    MeetingRecord::SummarySeen.key(&[], meeting, &[])
}

fn encode(summary: EntityId) -> Result<Vec<u8>, RuntimeError> {
    earshot_store::encode_versioned(RECORD_VERSION, &summary.to_string())
        .map_err(|_| RuntimeError::Internal("summary seen encoding"))
}

/// A stored record as read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Seen {
    /// Nothing stored: no summary of the meeting was opened here.
    Absent,
    /// The row last shown.
    Row(EntityId),
    /// A record this version cannot read (module docs: counts as seen).
    Unreadable,
}

fn decode(bytes: Option<&[u8]>) -> Seen {
    let Some(bytes) = bytes else {
        return Seen::Absent;
    };
    earshot_store::split_version(bytes)
        .filter(|(version, _)| *version == RECORD_VERSION)
        .and_then(|(_, body)| serde_json::from_slice::<String>(body).ok())
        .and_then(|s| s.parse().ok())
        .map_or(Seen::Unreadable, Seen::Row)
}

/// Whether `meeting` has a current summary this device has not opened (module docs). Reads one
/// summary id, never a summary body.
///
/// # Errors
/// Store errors.
pub fn has_new_summary(store: &Store, meeting: EntityId) -> Result<bool, RuntimeError> {
    let Some(current) = store.current_summary_id(meeting)? else {
        return Ok(false);
    };
    Ok(
        match decode(store.local_value(record_key(meeting).as_str())?.as_deref()) {
            Seen::Absent => true,
            Seen::Row(seen) => seen != current,
            Seen::Unreadable => false,
        },
    )
}

/// Records that the meeting's detail showed summary row `summary` of `meeting`. Returns whether
/// the record changed. Nothing is written unless the meeting is live and `summary` is its live
/// summary (the current one): checked and written in one transaction under the store lock, so a
/// deletion or a newer summary cannot slip in between.
///
/// # Errors
/// Store errors.
pub fn mark_seen(
    services: &Services,
    meeting: EntityId,
    summary: EntityId,
) -> Result<bool, RuntimeError> {
    let mut db = services.store();
    db.transaction(|tx| -> Result<bool, RuntimeError> {
        if !tx.meeting(meeting)?.is_some_and(|m| !m.meta.deleted) {
            return Ok(false);
        }
        if tx.current_summary_id(meeting)? != Some(summary) {
            return Ok(false);
        }
        let key = record_key(meeting);
        if decode(tx.local_value(key.as_str())?.as_deref()) == Seen::Row(summary) {
            return Ok(false);
        }
        crate::local::set_tx(tx, &key, &encode(summary)?)?;
        Ok(true)
    })
}

/// Marks every live meeting's current summary as seen, once per device: the first start of a
/// version with this record, so an upgrade lights no mark. One transaction, which also writes
/// the device record that it ran; returns how many meetings were marked (0 when it ran before).
///
/// Bounded: it reads the meeting rows once and, per live meeting, one local value and one summary
/// id (`MeetingRepo::current_summary_id`, no summary body), so it costs about what one read of the
/// meeting list does, and only on that first start.
///
/// # Errors
/// Store errors.
pub fn mark_existing_once(services: &Services) -> Result<usize, RuntimeError> {
    services.guarded_write(mark_existing_once_in)
}

/// [`mark_existing_once`]'s root (T14.57, docs/sync-spec.md 16.5 item 25): meetings read by
/// their id and `deleted` alone and each current summary by its id alone, so a meeting whose
/// current summary's id does not read is left unmarked (it may show "new summary" once) and the
/// start goes on.
///
/// # Errors
/// Store errors.
pub(crate) fn mark_existing_once_in(
    w: &mut crate::services::ServicesWrite<'_>,
) -> Result<usize, RuntimeError> {
    use earshot_store::{Col, Deleted};
    w.transaction(|g, _| -> Result<usize, RuntimeError> {
        if g.local_value(SEEN_START_KEY)?.is_some() {
            return Ok(0);
        }
        let mut marked = 0;
        for m in g.meetings_lenient()? {
            let (Col::Read(id), Deleted::No) = (m.id, m.deleted) else {
                continue;
            };
            let key = record_key(id);
            if g.local_value(key.as_str())?.is_some() {
                continue;
            }
            if let Some(Col::Read(current)) = g.current_summary_id_lenient(id)? {
                crate::local::set_guarded(g, &key, &encode(current)?)?;
                marked += 1;
            }
        }
        crate::local::set_guarded(g, &DeviceRecord::SummarySeenStart.key(), b"done")?;
        Ok(marked)
    })
}

#[cfg(test)]
#[path = "summary_seen_tests.rs"]
mod tests;
