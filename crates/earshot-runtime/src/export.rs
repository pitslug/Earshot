//! Obsidian export, on demand only (PLAN.md 3.8, decided by the owner): nothing in the
//! runtime exports by itself; these are the explicit commands the UI calls.
//!
//! - [`export_meeting`]: one meeting.
//! - [`export_meetings`]: "export all" or "export changed" in one batch through
//!   [`earshot_store::export_notes`] (one vault validation, one shared vault index).
//!
//! A meeting is rendered from its [`MeetingView`] (final transcript when there is one,
//! else the live draft; speakers; notes; current summary). Deleted meetings are never
//! exported. The vault is only ever written through `earshot-store`'s export writer
//! (atomic, merge-preserving).
//!
//! # "Changed"
//!
//! A meeting is changed when the note it renders to now differs from the note last
//! exported from this device: each successful export stores a SHA-256 fingerprint of the
//! rendered note (path and content) in the store's device-local state
//! ([`earshot_store::Store::set_local_value`]). This compares like with like: any edit
//! that changes the note counts, whether it happened in the same millisecond as the last
//! export, arrived by sync with an older clock, or is a tombstone (a deleted note);
//! edits that do not change the note (audio, voiceprints) do not. A meeting never
//! exported from this device is changed. A crash between writing the note and storing
//! its fingerprint only exports that meeting again.
//!
//! # Locking
//!
//! Neither export holds the store lock across vault work. Each meeting's view is loaded
//! and its fingerprint read under a short lock; rendering runs without it; the store
//! writer reaches the store through `LockedStore`, which takes the lock for one record
//! read or write at a time ([`earshot_store::ExportDb`]), so the vault walk, merges and file
//! writes never block a live session's segment writes. A fingerprint is written under the
//! lock only after its note was written.
//!
//! What this does and does not exclude, now that exports do not serialise on the lock:
//! - Each store read or write is atomic (one lock hold), and a record write re-reads the
//!   record and keeps its id.
//! - Two exports of one meeting at once (`export_meeting` during an `export_meetings`
//!   batch, two batches, another process), whatever their renders, never give it a second
//!   note: the store writer holds the meeting's export lease (a database row, so across
//!   processes too; `earshot_store` export docs, "Serialisation") from its search to its
//!   final record write, so the later export waits and then merges into the first one's
//!   note. It waits outside the store lock. Past the wait it fails for that meeting with the
//!   retryable `StoreError::ConcurrentEdit` (its fingerprint is not written, so it stays
//!   "changed"). An edit of the note by Obsidian or a sync client during the merge is also
//!   `ConcurrentEdit`. Behind the lease, each create still claims the meeting's export
//!   record and checks the claim before its rename (`earshot_store` export docs, step 4),
//!   for a lease taken over from a hung export.
//! - A meeting cannot be deleted while it is exported: each export holds a
//!   [`crate::MeetingLease`] on it, and a meeting being deleted is skipped by a batch (and
//!   refused by `export_meeting` with the retryable `RuntimeError::MeetingBusy`).
//! - Not excluded: rows changing between a meeting's render and its write (the note shows
//!   the rows as rendered; the next "changed" export picks the edit up).

use std::path::Path;
use std::sync::Arc;

use earshot_core::export::{ExportInput, ExportOptions, RenderedExport, render};
use earshot_core::identity::{DeviceId, EntityId};
use earshot_core::model::MeetingRepo;
use earshot_store::{
    ExportDb, ExportItem, ExportReport, Store, StoreError, export_note, export_notes,
};
use rand_core::Rng;
use sha2::{Digest, Sha256};

use crate::local::{LocalKey, MeetingRecord};
use crate::view::MeetingView;
use crate::{RuntimeError, Services};

#[cfg(test)]
#[path = "export_tests.rs"]
mod tests;

/// Which meetings [`export_meetings`] writes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExportSelection {
    /// Every meeting.
    All,
    /// Meetings whose note differs from the one last exported from this device (module
    /// docs).
    Changed,
}

fn rendered(view: &MeetingView, options: &ExportOptions) -> Result<RenderedExport, RuntimeError> {
    let attendees = view.attendees();
    render(
        &ExportInput {
            meeting: &view.meeting,
            attendees: &attendees,
            speakers: &view.speakers,
            segments: &view.segments,
            notes: &view.notes,
            summary: view.summary.as_ref().map(|s| &s.summary),
            transcript: view.pass,
        },
        options,
    )
    .map_err(RuntimeError::Export)
}

/// SHA-256 of the note as it would be written (path and whole file).
fn fingerprint(note: &RenderedExport) -> Vec<u8> {
    let path = note.path.to_string();
    let file = note.to_file();
    let mut h = Sha256::new();
    h.update((path.len() as u64).to_le_bytes());
    h.update(path.as_bytes());
    h.update(file.as_bytes());
    h.finalize().to_vec()
}

/// The services' store for the export writer: the lock is taken for each
/// [`ExportDb::with_store`] call only (module docs, "Locking").
struct LockedStore<'a> {
    services: &'a Services,
    /// Called when the writer starts a vault search (tests: a rendezvous).
    on_search: &'a mut dyn FnMut(),
}

impl ExportDb for LockedStore<'_> {
    fn with_store<T>(
        &mut self,
        f: impl FnOnce(&mut Store) -> Result<T, StoreError>,
    ) -> Result<T, StoreError> {
        f(&mut self.services.store())
    }

    fn vault_search_starts(&mut self) {
        (self.on_search)();
    }
}

pub(crate) fn fingerprint_key(device: DeviceId, meeting: EntityId) -> LocalKey {
    MeetingRecord::ExportFingerprint.key(&[&device.to_string()], meeting, &[])
}

fn record_fingerprint(
    store: &mut Store,
    device: DeviceId,
    meeting: EntityId,
    note: &RenderedExport,
) -> Result<(), StoreError> {
    crate::local::set(store, &fingerprint_key(device, meeting), &fingerprint(note))
}

/// Exports one meeting into `vault` (explicit command).
///
/// # Errors
/// [`RuntimeError::Internal`] for an unknown or deleted meeting;
/// [`RuntimeError::MeetingBusy`] (retryable) while the meeting is being deleted;
/// [`RuntimeError::Export`] for unusable options; store errors (unusable vault,
/// concurrent edit: retryable).
pub fn export_meeting<R: Rng + ?Sized>(
    services: &Services,
    vault: &Path,
    meeting: EntityId,
    options: &ExportOptions,
    rng: &mut R,
) -> Result<ExportReport, RuntimeError> {
    let clock = Arc::clone(&services.clock);
    let _lease = services.lease_meeting(meeting)?;
    let (view, device) = {
        let db = services.store();
        (MeetingView::load(&**db, meeting)?, db.device())
    };
    let view = view.ok_or(RuntimeError::Internal("no such meeting"))?;
    let note = rendered(&view, options)?;
    let mut locked = LockedStore {
        services,
        on_search: &mut || {},
    };
    let report = export_note(&mut locked, vault, device, meeting, &note, &*clock, rng)?;
    record_fingerprint(&mut services.store(), device, meeting, &note)?;
    Ok(report)
}

/// Exports every meeting, or the changed ones, into `vault` in one batch (explicit
/// command). Returns one result per exported meeting, in meeting order; one meeting's
/// failure does not stop the others (and keeps it "changed").
///
/// # Errors
/// [`RuntimeError::Export`] for unusable options; store errors reading the meetings or
/// an unusable vault (then nothing is exported).
#[allow(clippy::type_complexity)]
pub fn export_meetings<R: Rng + ?Sized>(
    services: &Services,
    vault: &Path,
    selection: ExportSelection,
    options: &ExportOptions,
    rng: &mut R,
) -> Result<Vec<(EntityId, Result<ExportReport, StoreError>)>, RuntimeError> {
    export_meetings_observed(services, vault, selection, options, rng, &mut || {})
}

/// [`export_meetings`] with `on_search` called when the writer starts a vault search,
/// with the store lock free (tests).
#[allow(clippy::type_complexity)]
fn export_meetings_observed<R: Rng + ?Sized>(
    services: &Services,
    vault: &Path,
    selection: ExportSelection,
    options: &ExportOptions,
    rng: &mut R,
    on_search: &mut dyn FnMut(),
) -> Result<Vec<(EntityId, Result<ExportReport, StoreError>)>, RuntimeError> {
    let clock = Arc::clone(&services.clock);
    let (device, meetings) = {
        let db = services.store();
        (db.device(), db.meetings()?)
    };
    let mut chosen = Vec::new();
    let mut leases = Vec::new();
    for m in meetings {
        // A meeting being deleted is skipped, as a deleted one is.
        let Ok(lease) = services.lease_meeting(m.id) else {
            continue;
        };
        // One short lock per meeting; rendering runs without it.
        let Some(view) = MeetingView::load(&**services.store(), m.id)? else {
            continue;
        };
        let note = rendered(&view, options)?;
        let wanted = match selection {
            ExportSelection::All => true,
            ExportSelection::Changed => {
                services
                    .store()
                    .local_value(fingerprint_key(device, m.id).as_str())?
                    .as_deref()
                    != Some(fingerprint(&note).as_slice())
            }
        };
        if wanted {
            chosen.push((m.id, note));
            leases.push(lease);
        }
    }
    let items: Vec<ExportItem<'_>> = chosen
        .iter()
        .map(|(meeting, rendered)| ExportItem {
            meeting: *meeting,
            rendered,
        })
        .collect();
    let mut locked = LockedStore {
        services,
        on_search,
    };
    let results = export_notes(&mut locked, vault, device, &items, &*clock, rng)?;
    for ((meeting, note), result) in chosen.iter().zip(&results) {
        if result.is_ok() {
            record_fingerprint(&mut services.store(), device, *meeting, note)?;
        }
    }
    // The chosen meetings could not be deleted until their fingerprints were written.
    drop(leases);
    Ok(chosen.iter().map(|(m, _)| *m).zip(results).collect())
}
