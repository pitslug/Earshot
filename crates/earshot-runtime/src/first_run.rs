//! The first-run wizard's state (T9.1d, D21): whether the wizard is still to be shown. One
//! device-local record ([`FIRST_RUN_KEY`], `version u32 LE | JSON`, never synced, no content),
//! written only here.
//!
//! The rule for showing the wizard exists once, [`first_run_state`]: it shows until it was
//! finished (skipping every step counts) on a store that has never held a meeting. A store
//! with any meeting row, a deleted one's tombstone included, counts as done without a record,
//! so someone who has already used Earshot never sees it. A record this version cannot read
//! also counts as done: it says the wizard ran, and being shown it again is worse.
//!
//! A record from before T12.5a also holds `declined_models`, the wizard's old "never" answers.
//! It still reads and the answers are ignored; the next write leaves them out. An older version
//! reading a new record finds that field missing and so counts the wizard as done, which is
//! what a new record says: only finishing writes one.

use earshot_store::Store;
use serde::{Deserialize, Serialize};

use crate::RuntimeError;
use crate::local::DeviceRecord;

#[cfg(test)]
#[path = "first_run_tests.rs"]
mod tests;

/// `local_state` key of the first-run record.
pub const FIRST_RUN_KEY: &str = "first-run";

/// The record's version, stored outside the payload.
const RECORD_VERSION: u32 = 1;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
struct Record {
    /// The wizard was finished or skipped.
    done: bool,
}

fn encode(record: &Record) -> Result<Vec<u8>, RuntimeError> {
    let json = serde_json::to_vec(record)
        .map_err(|_| RuntimeError::Internal("the first-run record could not be written"))?;
    let mut out = RECORD_VERSION.to_le_bytes().to_vec();
    out.extend_from_slice(&json);
    Ok(out)
}

fn decode(bytes: &[u8]) -> Option<Record> {
    let (version, json) = bytes.split_first_chunk::<4>()?;
    if u32::from_le_bytes(*version) != RECORD_VERSION {
        return None;
    }
    serde_json::from_slice(json).ok()
}

/// The stored record; `None` when there is none. One this version cannot read comes back done
/// (module docs).
fn read(store: &Store) -> Result<Option<Record>, RuntimeError> {
    Ok(store
        .local_value(FIRST_RUN_KEY)?
        .map(|bytes| decode(&bytes).unwrap_or(Record { done: true })))
}

fn write(store: &mut Store, record: &Record) -> Result<(), RuntimeError> {
    crate::local::set(store, &DeviceRecord::FirstRun.key(), &encode(record)?)?;
    Ok(())
}

/// What the wizard needs to know.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FirstRunState {
    /// The app opens on the wizard.
    pub show: bool,
}

/// The wizard's state (module docs: the one rule for showing it).
///
/// # Errors
/// Store errors.
pub fn first_run_state(store: &Store) -> Result<FirstRunState, RuntimeError> {
    let record = read(store)?.unwrap_or_default();
    let show = !record.done && !store.has_meeting_row()?;
    Ok(FirstRunState { show })
}

/// Marks the wizard done, whichever steps were skipped. Finishing again (after reopening it from
/// Settings) changes nothing else.
///
/// # Errors
/// Store errors.
pub fn finish_first_run(store: &mut Store) -> Result<FirstRunState, RuntimeError> {
    write(store, &Record { done: true })?;
    first_run_state(store)
}
