//! The first-run wizard's state (T9.1d, D21): whether the wizard is still to be shown, and
//! which optional models the user said "never" to. One device-local record ([`FIRST_RUN_KEY`],
//! `version u32 LE | JSON`, never synced, no content), written only here.
//!
//! The rule for showing the wizard exists once, [`first_run_state`]: it shows until it was
//! finished (skipping every step counts) on a store that has never held a meeting. A store
//! with any meeting row, a deleted one's tombstone included, counts as done without a record,
//! so someone who has already used Earshot never sees it. A record this version cannot read
//! also counts as done: it says the wizard ran, and being shown it again is worse than losing
//! the "never" answers.
//!
//! "Never" only records an answer: the wizard shows it again when reopened, and Settings,
//! Models still offers the model.

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
    /// The optional models the user said "never" to, sorted.
    declined_models: Vec<String>,
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

/// The stored record; `None` when there is none. One this version cannot read comes back done,
/// with no answers (module docs).
fn read(store: &Store) -> Result<Option<Record>, RuntimeError> {
    Ok(store.local_value(FIRST_RUN_KEY)?.map(|bytes| {
        decode(&bytes).unwrap_or(Record {
            done: true,
            declined_models: Vec::new(),
        })
    }))
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
    /// The optional models the user said "never" to, sorted.
    pub declined_models: Vec<String>,
}

/// The wizard's state (module docs: the one rule for showing it).
///
/// # Errors
/// Store errors.
pub fn first_run_state(store: &Store) -> Result<FirstRunState, RuntimeError> {
    let record = read(store)?.unwrap_or_default();
    let show = !record.done && !store.has_meeting_row()?;
    Ok(FirstRunState {
        show,
        declined_models: record.declined_models,
    })
}

/// Marks the wizard done, whichever steps were skipped; keeps the "never" answers. Finishing
/// again (after reopening it from Settings) changes nothing else.
///
/// # Errors
/// Store errors.
pub fn finish_first_run(store: &mut Store) -> Result<FirstRunState, RuntimeError> {
    let mut record = read(store)?.unwrap_or_default();
    record.done = true;
    write(store, &record)?;
    first_run_state(store)
}

/// Records or clears "never" for `model`. Callers check the name against the catalogue.
///
/// # Errors
/// Store errors.
pub fn set_model_declined(
    store: &mut Store,
    model: &str,
    declined: bool,
) -> Result<FirstRunState, RuntimeError> {
    let mut record = read(store)?.unwrap_or_default();
    record.declined_models.retain(|m| m != model);
    if declined {
        record.declined_models.push(model.to_owned());
        record.declined_models.sort();
    }
    write(store, &record)?;
    first_run_state(store)
}
