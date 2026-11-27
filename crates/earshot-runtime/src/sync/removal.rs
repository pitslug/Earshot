//! The one-off removal repair, sync on or off (T14.49e, docs/sync-spec.md 7A.2, "The one-off
//! removal repair, sync on or off", and 7A.10).
//!
//! A client's first page runs P2 and P3 with every repair, but a device that turned sync off
//! after updating never applies a page, so rows an earlier sync left (a speaker row linked to a
//! person tombstoned here, a live person merged into one) would stay. So P2 and P3 also run once
//! per device, with P5's entry drops, outside any page ([`removal_repair`]): in the app on the
//! `earshot-sync-start` thread before sync starts, whether sync is on or off, and in the worker
//! before it opens. Not inside `Store::open`: P3 rewrites speaker names, and only the runtime
//! reads `names_before` and runs `names_changed_tx`, so a summary that used a name is marked.
//!
//! Its presence in the device record `removal-repair` ([`REMOVAL_REPAIR_KEY`],
//! [`DeviceRecord::RemovalRepair`]; `version u32 LE | JSON`, counts only) means done. Best
//! effort: an error rolls the whole repair back, the caller carries on and the next start tries
//! again. Racing a tick is safe: both hold the services store lock, so one runs after the other,
//! and P2 and P3 find nothing left the second time. Its writes carry this device's stamp and
//! wait in the outbox like any edit.

use std::collections::BTreeSet;

use earshot_core::identity::EntityId;
use earshot_store::encode_versioned;
use serde::Serialize;

use crate::RuntimeError;
use crate::local::DeviceRecord;
use crate::services::{Services, ServicesWrite};

/// `local_state` key of the device record that says the removal repair ran.
pub const REMOVAL_REPAIR_KEY: &str = "removal-repair";

/// The record's version.
pub const REMOVAL_REPAIR_VERSION: u32 = 1;

/// What the removal repair did. Counts and ids only.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RemovalRepaired {
    /// Whether it ran now (false when the record was already there).
    pub ran: bool,
    /// Live persons tombstoned because their `merged_into` chain ends at a person removed here
    /// (P2).
    pub persons_removed_with: usize,
    /// Speaker rows unlinked from a person removed here (P3).
    pub speakers_unlinked: usize,
    /// Items erased (docs/sync-spec.md 7A.4): names cleared, speaker names rewritten, identity
    /// entries dropped and biometric rows.
    pub erased: usize,
    /// Earlier names recorded against a summary that used them (`summary-names`).
    pub names_recorded: usize,
    /// Rows that did not read and were skipped (docs/sync-spec.md 16.7).
    pub damaged_skipped: usize,
    /// The meetings whose speaker rows or identity records it changed.
    pub meetings: BTreeSet<EntityId>,
    /// Whether a person row or a link to one changed.
    pub people: bool,
}

impl RemovalRepaired {
    /// What the window reads again, as sync tells it; `None` when nothing changed.
    #[must_use]
    pub fn changes(&self) -> Option<super::SyncChanges> {
        (self.people || !self.meetings.is_empty()).then(|| super::SyncChanges {
            meetings: self.meetings.iter().copied().collect(),
            people: self.people,
        })
    }
}

/// The record's payload: counts only.
#[derive(Serialize)]
struct Record {
    persons_removed_with: u64,
    speakers_unlinked: u64,
    erased: u64,
    names_recorded: u64,
    damaged_skipped: u64,
}

/// The record's bytes for `r`.
fn record_bytes(r: &RemovalRepaired) -> Vec<u8> {
    let n = |v: usize| u64::try_from(v).unwrap_or(u64::MAX);
    // A struct of integers always serialises.
    encode_versioned(
        REMOVAL_REPAIR_VERSION,
        &Record {
            persons_removed_with: n(r.persons_removed_with),
            speakers_unlinked: n(r.speakers_unlinked),
            erased: n(r.erased),
            names_recorded: n(r.names_recorded),
            damaged_skipped: n(r.damaged_skipped),
        },
    )
    .unwrap_or_default()
}

/// Runs the removal repair once per device (module docs).
///
/// # Errors
/// Store errors: nothing is written then, and the next call tries again.
pub fn removal_repair(services: &Services) -> Result<RemovalRepaired, RuntimeError> {
    services.guarded_write(removal_repair_in)
}

/// [`removal_repair`]'s root (docs/sync-spec.md 16.3).
///
/// # Errors
/// As [`removal_repair`].
pub(crate) fn removal_repair_in(
    w: &mut ServicesWrite<'_>,
) -> Result<RemovalRepaired, RuntimeError> {
    if w.read().local_value(REMOVAL_REPAIR_KEY)?.is_some() {
        return Ok(RemovalRepaired::default());
    }
    // The names P3 can rewrite, read under the lock this repair holds throughout (as a page's
    // are, `apply_page_in`): every live speaker row linked to a person removed here or whose
    // chain ends at one (7A.9, "Names"). A row that does not read is skipped and counted.
    let (names_before, skipped_before) = {
        let r = w.read();
        let rows = crate::summary_names::rows_removal_may_rename(&r, &[], true)?;
        (
            crate::summary_names::names_before(&r, &rows)?,
            r.damaged_rows(),
        )
    };
    let result = w.transaction(|g, stamp| -> Result<RemovalRepaired, RuntimeError> {
        if g.local_value(REMOVAL_REPAIR_KEY)?.is_some() {
            return Ok(RemovalRepaired::default());
        }
        let applied = earshot_store::removal_repair_tx(g, stamp.hlc, stamp.clock)?;
        let names_recorded = crate::summary_names::names_changed_tx(g, &names_before)?;
        let mut damaged = applied.damaged_rows.clone();
        damaged.extend(g.damaged_rows());
        damaged.extend(skipped_before.iter().copied());
        let out = RemovalRepaired {
            ran: true,
            persons_removed_with: applied.persons_removed_with,
            speakers_unlinked: applied.speakers_unlinked,
            erased: applied.erased,
            names_recorded,
            damaged_skipped: damaged.len(),
            meetings: applied.meetings,
            people: applied.people,
        };
        crate::local::set_guarded(g, &DeviceRecord::RemovalRepair.key(), &record_bytes(&out))?;
        Ok(out)
    });
    let out = match result {
        Ok(out) => out,
        Err(e) => {
            // A fixed word only: the next start tries again.
            eprintln!("removal repair: not done");
            return Err(e);
        }
    };
    if out.erased > 0 {
        // A Busy here means the erasure is committed; the next checkpoint erases it (7A.4).
        let _ = w.flush_erasures();
    }
    if out.ran {
        // A fixed word and counts only (docs/sync-spec.md 16.7).
        eprintln!(
            "removal repair: persons {} speakers {} erased {} names {} damaged rows skipped {}",
            out.persons_removed_with,
            out.speakers_unlinked,
            out.erased,
            out.names_recorded,
            out.damaged_skipped
        );
    }
    Ok(out)
}
