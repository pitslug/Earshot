//! Repair from sync (T14.65, docs/sync-spec.md 17.4): the pull reads the relay again from row
//! 0, so a damaged row here is replaced by a readable copy another computer pushed, as an
//! absent row is. Nothing is queued or removed; it runs as ordinary ticks.
//!
//! Its progress is the device record `damage-repair` ([`DAMAGE_REPAIR_KEY`]), written only by
//! compare-and-set, each write in its own transaction, the one that changes the sync state it
//! tracks:
//!
//! - arm (absent -> `armed`), with the cursor's restart, by [`start_repair`];
//! - mark (`armed` -> `reading`), before the pull sends a request from row 0
//!   ([`mark_before_request`]): never after a cursor move, so `reading` always means the pull
//!   since then started at row 0;
//! - end (`reading` -> absent), at the end of the first tick whose pull reached the relay's last
//!   row with nothing held ([`end_after_tick`]);
//! - turning sync off, removing this device and setting up or joining (any -> absent,
//!   [`clear_tx`]).

use earshot_store::{StoreTx, encode_versioned, split_version};
use serde::{Deserialize, Serialize};

use super::{SyncProblem, cursor_of, encode_cursor_record, restart_pull_tx};
use crate::local::{self, DeviceRecord};
use crate::services::{Services, ServicesWrite};

/// The device record of a Repair from sync in progress.
pub const DAMAGE_REPAIR_KEY: &str = "damage-repair";

/// The version outside the record's JSON.
pub const DAMAGE_REPAIR_VERSION: u32 = 1;

/// Where a Repair from sync is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RepairStep {
    /// Pressed; the pull has not yet sent a request from row 0.
    Armed,
    /// The pull since the mark started at row 0.
    Reading,
}

#[derive(Serialize, Deserialize)]
struct RepairRecord {
    step: RepairStep,
}

/// The record's bytes for `step`.
#[must_use]
pub fn encode_step(step: RepairStep) -> Vec<u8> {
    encode_versioned(DAMAGE_REPAIR_VERSION, &RepairRecord { step }).expect("a step serialises")
}

/// The step a stored record says. A record that does not decode counts as `Armed`: a Repair
/// is in progress, and the mark then starts it from row 0 again (the safe reading: at worst one
/// more read of the relay).
fn decode_step(bytes: Option<Vec<u8>>) -> Option<RepairStep> {
    let bytes = bytes?;
    Some(
        split_version(&bytes)
            .filter(|(v, _)| *v == DAMAGE_REPAIR_VERSION)
            .and_then(|(_, payload)| serde_json::from_slice::<RepairRecord>(payload).ok())
            .map_or(RepairStep::Armed, |r| r.step),
    )
}

/// The Repair in progress, if any.
///
/// # Errors
/// [`SyncProblem::Store`].
pub fn repair_step(services: &Services) -> Result<Option<RepairStep>, SyncProblem> {
    let r = services.guarded_read();
    Ok(decode_step(r.read().local_value(DAMAGE_REPAIR_KEY)?))
}

/// Where a Repair is, for a progress line: its step and the cursor's numbers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RepairProgress {
    /// The step the record says.
    pub step: RepairStep,
    /// The relay sequence number pulled up to.
    pub pull_seq: u64,
    /// The highest sequence number of the vault seen (0 before any answer named one).
    pub seen_seq: u64,
}

/// The Repair in progress with the cursor's numbers, if any (one read).
///
/// # Errors
/// [`SyncProblem::Store`].
pub fn repair_progress(services: &Services) -> Result<Option<RepairProgress>, SyncProblem> {
    let r = services.guarded_read();
    let g = r.read();
    let Some(step) = decode_step(g.local_value(DAMAGE_REPAIR_KEY)?) else {
        return Ok(None);
    };
    let cursor = cursor_of(&|k| g.local_value(k))?;
    Ok(Some(RepairProgress {
        step,
        pull_seq: cursor.pull_seq,
        seen_seq: cursor.seen_seq,
    }))
}

/// The percentage a Repair shows (docs/sync-spec.md 17.4): `pull_seq * 100 / seen_seq`, at most
/// 99 (it ends only with the closing tick), `None` while `seen_seq` is 0.
#[must_use]
pub fn repair_percent(pull_seq: u64, seen_seq: u64) -> Option<u8> {
    if seen_seq == 0 {
        return None;
    }
    let p = u128::from(pull_seq) * 100 / u128::from(seen_seq);
    Some(u8::try_from(p.min(99)).unwrap_or(99))
}

/// What [`start_repair`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RepairStart {
    /// Armed with the cursor at row 0; the host asks the sync thread for a tick.
    Started,
    /// A Repair is already in progress: nothing was written.
    AlreadyRunning,
}

/// Starts a Repair from sync: in one transaction, the pull's restart (`pull_seq` 0; `seen_seq`,
/// the epoch and the resets kept, so it is no vault reset and nothing is queued) and the arm, a
/// compare-and-set on an absent record. The caller checks sync is set up and on.
///
/// # Errors
/// [`SyncProblem::Store`].
pub fn start_repair(services: &Services) -> Result<RepairStart, SyncProblem> {
    let mut db = services.store();
    db.transaction(|tx| -> Result<RepairStart, SyncProblem> {
        if tx.local_value(DAMAGE_REPAIR_KEY)?.is_some() {
            return Ok(RepairStart::AlreadyRunning);
        }
        restart_pull_tx(tx)?;
        local::set_tx(
            tx,
            &DeviceRecord::DamageRepair.key(),
            &encode_step(RepairStep::Armed),
        )?;
        Ok(RepairStart::Started)
    })
}

/// The mark, before the pull sends a request (docs/sync-spec.md 17.4): while the record says
/// `armed`, in one transaction the cursor goes back to row 0 if it moved (a tick already past 0
/// when Repair was pressed, or a crash between a cursor advance and the mark) and the record
/// says `reading`. Any other record writes nothing. Returns whether it marked.
pub(crate) fn mark_before_request(w: &mut ServicesWrite<'_>) -> Result<bool, SyncProblem> {
    w.transaction(|g, _| -> Result<bool, SyncProblem> {
        if decode_step(g.local_value(DAMAGE_REPAIR_KEY)?) != Some(RepairStep::Armed) {
            return Ok(false);
        }
        let mut cursor = cursor_of(&|k| g.local_value(k))?;
        if cursor.pull_seq != 0 {
            cursor.pull_seq = 0;
            local::set_guarded(
                g,
                &DeviceRecord::SyncCursor.key(),
                &encode_cursor_record(&cursor),
            )?;
        }
        local::set_guarded(
            g,
            &DeviceRecord::DamageRepair.key(),
            &encode_step(RepairStep::Reading),
        )?;
        Ok(true)
    })
}

/// The end, at the end of a tick whose pull reached the relay's last row with nothing held
/// (the caller judges that): a compare-and-set on `reading` deletes the record. Returns whether
/// the Repair ended, so the host scans again.
pub(crate) fn end_after_tick(w: &mut ServicesWrite<'_>) -> Result<bool, SyncProblem> {
    w.transaction(|g, _| -> Result<bool, SyncProblem> {
        if decode_step(g.local_value(DAMAGE_REPAIR_KEY)?) != Some(RepairStep::Reading) {
            return Ok(false);
        }
        g.delete_local_value(DAMAGE_REPAIR_KEY)?;
        Ok(true)
    })
}

/// Inside `tx`: no Repair any more (sync turned off, this device removed, set up or joined).
///
/// # Errors
/// Store errors.
pub(crate) fn clear_tx(tx: &mut StoreTx<'_>) -> Result<(), earshot_store::StoreError> {
    tx.delete_local_value(DAMAGE_REPAIR_KEY)?;
    Ok(())
}

#[cfg(test)]
pub(crate) fn decode_step_for_test(bytes: Option<Vec<u8>>) -> Option<RepairStep> {
    decode_step(bytes)
}
