//! The one-off copy of legacy ignores onto the synced speaker rows (T12.93).
//!
//! Until 0.4.2 the user's choice to ignore a voice (T12.19) lived only in the voice's entry of
//! the device-local `speaker-identities/<meeting>` record
//! (`earshot_store::identities::SpeakerIdentity::ignored`), which never syncs, so another
//! computer still asked for a name. Since T12.93 the choice is `Speaker::ignored` on the synced
//! row. Each device copies its own legacy ignores onto the rows once ([`copy_once`]), and records
//! that it did in the device record `ignore-copied` ([`COPIED_KEY`],
//! [`crate::local::DeviceRecord::IgnoreCopied`]).
//!
//! **When.** At app start only when this device has no sync vault at all (nothing can arrive
//! from another computer); otherwise only after the sync thread's first completed pull (a tick
//! whose pull ended with `more == false`, `crate::sync::TickReport::pull_complete`, with no
//! problem, no row held in that tick and none waiting in the store for a retry), whatever the
//! sync switch was at start, so an edit another computer made (a name, a link, an un-ignore)
//! is already here and is never overwritten by an older choice. A device with no vault copies
//! at start; if it joins a vault later, its copied flags are ordinary edits stamped then, and
//! can beat a name another computer gave the voice meanwhile (accepted: rare, and visible).
//!
//! **What.** In one transaction, re-reading each row: every live speaker of a live meeting
//! whose entry says ignored and whose row is unnamed, not the user and not yet ignored gets the
//! flag (re-stamped, so it syncs). There is no "row unchanged since the pass" clause: after the
//! pull it protects nothing, and it would drop a voice the user renamed and ignored.
//!
//! **Before the copy** the entry flag is a legacy ignore and is honoured everywhere: the one
//! rule `crate::edit::is_ignored` reads `row.ignored` or the entry flag ([`legacy_of`]), the
//! final pass's `SpeakerIdentity::edited` reads it too, and a final pass that continues the
//! voice carries the flag into its new entry (never onto the row, whose re-stamp could beat a
//! remote name not pulled yet), the one place the flag is still written true. After the copy the entry flag is never
//! read ([`legacy_of`] returns nothing). The worker (a headless paired device) shares the sync
//! thread and holds no entries: there the copy finds nothing and only records that it ran.
//!
//! The module needs no engine (the worker builds the runtime without `engines`).

use std::collections::BTreeSet;

use earshot_core::diar::SpeakerRole;
use earshot_core::identity::EntityId;
use earshot_store::{Store, StoreTx};

use crate::identities::{SpeakerIdentity, StoredIdentities};
use crate::local::DeviceRecord;
use crate::{RuntimeError, Services};

/// `local_state` key of the device record that says the legacy ignores were copied.
pub const COPIED_KEY: &str = "ignore-copied";

/// Whether this device's copy has run (a store the caller has locked).
///
/// # Errors
/// Store errors.
pub fn copied(store: &Store) -> Result<bool, RuntimeError> {
    Ok(store.local_value(COPIED_KEY)?.is_some())
}

/// [`copied`] inside a transaction.
pub(crate) fn copied_tx(tx: &StoreTx<'_>) -> Result<bool, RuntimeError> {
    Ok(tx.local_value(COPIED_KEY)?.is_some())
}

/// The speakers whose entry among `entries` holds a legacy ignore, honoured only while the copy
/// has not run (`copied` false); empty after it, so the entry flag is never read again.
#[must_use]
pub fn legacy_of(copied: bool, entries: &[SpeakerIdentity]) -> BTreeSet<EntityId> {
    if copied {
        return BTreeSet::new();
    }
    entries
        .iter()
        .filter(|i| i.ignored)
        .map(|i| i.speaker)
        .collect()
}

/// Copies this device's legacy ignores onto the speaker rows, once (module docs). Returns how
/// many rows it flagged; 0 when it ran before. The caller decides when (module docs): at app
/// start with no vault, or after the sync thread's first completed pull.
///
/// A row or meeting that does not read is skipped and counted, and the copy still writes
/// `ignore-copied` (the owner, docs/sync-spec.md 16.12 c): its legacy flag is lost for good
/// on this device, since a later run could overwrite an un-ignore pulled meanwhile.
///
/// # Errors
/// Store errors (nothing is written then, and the next call tries again).
pub fn copy_once(services: &Services) -> Result<usize, RuntimeError> {
    services.guarded_write(copy_once_in)
}

/// [`copy_once`]'s root (T14.57).
///
/// # Errors
/// As [`copy_once`].
pub(crate) fn copy_once_in(
    w: &mut crate::services::ServicesWrite<'_>,
) -> Result<usize, RuntimeError> {
    use earshot_store::{Col, Deleted};
    if w.read().local_value(COPIED_KEY)?.is_some() {
        return Ok(0);
    }
    let (flagged, skipped) = w.transaction(|g, stamp| -> Result<(usize, usize), RuntimeError> {
        if g.local_value(COPIED_KEY)?.is_some() {
            return Ok((0, 0));
        }
        let mut flagged = 0;
        for meeting in g.meetings_lenient()? {
            let (Col::Read(id), Deleted::No) = (meeting.id, meeting.deleted) else {
                continue;
            };
            let key = crate::identities::record_key(id);
            let Some(StoredIdentities::Record(record)) = g
                .local_value(key.as_str())?
                .map(|b| crate::identities::decode(&b))
            else {
                continue;
            };
            let ignored: BTreeSet<EntityId> = legacy_of(false, &record.speakers);
            if ignored.is_empty() {
                continue;
            }
            for mut row in g.speakers_or_damaged(id)?.0 {
                if !ignored.contains(&row.id)
                    || row.meta.deleted
                    || row.role == SpeakerRole::Me
                    || row.person_id.is_some()
                    || row.ignored
                {
                    continue;
                }
                row.ignored = true;
                row.meta.touch(stamp.hlc, stamp.clock);
                g.put_speaker(row)?;
                flagged += 1;
            }
        }
        crate::local::set_guarded(g, &DeviceRecord::IgnoreCopied.key(), b"done")?;
        Ok((flagged, g.damaged_skipped()))
    })?;
    if skipped > 0 {
        // A fixed word and the count only (docs/sync-spec.md 16.7).
        eprintln!("ignore copy: damaged rows skipped {skipped}");
    }
    Ok(flagged)
}

#[cfg(test)]
#[path = "ignore_copy_tests.rs"]
mod tests;
