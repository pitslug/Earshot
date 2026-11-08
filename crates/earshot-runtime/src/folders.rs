//! Folders in the side panel (T9.5b, docs/folders-design.md): the one owner of every folder
//! read and edit. One level of named folders; a meeting is in at most one, through
//! [`Meeting::folder_id`].
//!
//! - Reads: [`list`] (live folders by name, case folded per character as search folds),
//!   [`folder_of`] and [`meeting_folders`]. A meeting naming a tombstoned or unknown folder
//!   reads as no folder ([`LiveFolders::resolve`]): sync may deliver a meeting before its
//!   folder, or one device may delete a folder while another moves a meeting into it. Hosts
//!   show the resolved folder, never a raw `folder_id`.
//! - Edits, each one [`crate::edit`] write transaction that re-reads what it changes:
//!   [`create`], [`rename`] (the speaker and person name rule, [`clean_name`]; two live
//!   folders never share a name ignoring case, checked over live rows inside the
//!   transaction, never by a storage index, because every tombstone carries the same cleared
//!   name), [`move_meeting`] (refused while the meeting records; allowed while it finishes
//!   or a job runs on it, because the folder is the user's field and every other writer of
//!   the meeting row re-reads it and changes only its own fields).
//! - Deleting: [`delete_keeping_meetings`] (one transaction: the folder becomes a tombstone
//!   with its name cleared and its meetings lose their folder) and [`delete_with_meetings`]
//!   (each meeting through [`Services::delete_meeting`] with no lock held, then the same
//!   closing transaction, which selects the folder's meetings again inside it, so a meeting
//!   moved in meanwhile is unfiled rather than left naming a tombstone; if any deletion
//!   fails the folder stays with what is left). Both end with `Store::flush_erasures`.
//!
//! A folder name is content: it is never logged, and no error, event or result here carries
//! it ([`EditError`] refusals carry ids at most).

use std::collections::BTreeMap;

use earshot_core::identity::EntityId;
use earshot_core::model::{Folder, FolderRepo, Meeting, MeetingRepo, delete_folder};
use earshot_store::StoreTx;

use crate::edit::{DynStamp, EditError, clean_name, live_meeting, write};
use crate::search::fold_case;
use crate::{MeetingActivity, RuntimeError, Services};

/// A live folder as hosts show it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FolderItem {
    /// The folder's row id.
    pub id: EntityId,
    /// Its name (content: never logged).
    pub name: String,
}

/// The live folders of a store, read once: the one place a `folder_id` is resolved.
#[derive(Debug, Clone, Default)]
pub struct LiveFolders(BTreeMap<EntityId, Folder>);

impl LiveFolders {
    /// The live (not tombstoned) folders of `repo`.
    ///
    /// # Errors
    /// Storage errors.
    pub fn read<R: FolderRepo + ?Sized>(repo: &R) -> Result<Self, RuntimeError> {
        Ok(Self(
            repo.folders()?
                .into_iter()
                .filter(|f| !f.meta.deleted)
                .map(|f| (f.id, f))
                .collect(),
        ))
    }

    /// The folder a meeting with `folder_id` is in: `None` for no folder, and for an id
    /// naming a tombstoned or unknown folder.
    #[must_use]
    pub fn resolve(&self, folder_id: Option<EntityId>) -> Option<&Folder> {
        folder_id.and_then(|id| self.0.get(&id))
    }

    /// The folders sorted by name, case folded per character, then by creation and id (two
    /// live folders can share a name after a sync).
    #[must_use]
    pub fn sorted(&self) -> Vec<FolderItem> {
        let mut folders: Vec<&Folder> = self.0.values().collect();
        folders.sort_by_cached_key(|f| {
            (
                fold_case(&f.name).collect::<String>(),
                f.meta.created_at,
                f.id,
            )
        });
        folders
            .into_iter()
            .map(|f| FolderItem {
                id: f.id,
                name: f.name.clone(),
            })
            .collect()
    }
}

/// Every live folder, sorted by name (case folded per character).
///
/// # Errors
/// Storage errors.
pub fn list(services: &Services) -> Result<Vec<FolderItem>, RuntimeError> {
    Ok(LiveFolders::read(&**services.store())?.sorted())
}

/// The folder `meeting` is in: `None` for no folder, a dangling or tombstoned `folder_id`,
/// or a meeting that does not exist or is deleted.
///
/// # Errors
/// Storage errors.
pub fn folder_of(
    services: &Services,
    meeting: EntityId,
) -> Result<Option<FolderItem>, RuntimeError> {
    let db = services.store();
    let Some(m) = db.meeting(meeting)?.filter(|m| !m.meta.deleted) else {
        return Ok(None);
    };
    let folders = LiveFolders::read(&**db)?;
    Ok(folders.resolve(m.folder_id).map(|f| FolderItem {
        id: f.id,
        name: f.name.clone(),
    }))
}

/// The resolved folder of every live meeting that is in one (meeting id -> folder id), read
/// under one store lock: what a meeting list needs beside [`list`]. A meeting with no
/// folder, or naming a tombstoned or unknown one, is absent.
///
/// # Errors
/// Storage errors.
pub fn meeting_folders(services: &Services) -> Result<BTreeMap<EntityId, EntityId>, RuntimeError> {
    let db = services.store();
    let folders = LiveFolders::read(&**db)?;
    Ok(db
        .meetings()?
        .into_iter()
        .filter(|m| !m.meta.deleted)
        .filter_map(|m| Some((m.id, folders.resolve(m.folder_id)?.id)))
        .collect())
}

/// The live folder `folder` in a transaction.
fn live_folder<R: FolderRepo + ?Sized>(repo: &R, folder: EntityId) -> Result<Folder, RuntimeError> {
    repo.folder(folder)?
        .filter(|f| !f.meta.deleted)
        .ok_or_else(|| EditError::FolderNotFound(folder).into())
}

/// Refuses `name` when a live folder other than `except` has it, ignoring case.
fn check_free<R: FolderRepo + ?Sized>(
    repo: &R,
    name: &str,
    except: Option<EntityId>,
) -> Result<(), RuntimeError> {
    let folded: String = fold_case(name).collect();
    for f in repo.folders()? {
        if !f.meta.deleted && Some(f.id) != except && fold_case(&f.name).eq(folded.chars()) {
            return Err(EditError::FolderNameTaken(f.id).into());
        }
    }
    Ok(())
}

/// The live meetings filed in `folder` (by their stored `folder_id`).
fn members<R: MeetingRepo + ?Sized>(
    repo: &R,
    folder: EntityId,
) -> Result<Vec<Meeting>, RuntimeError> {
    Ok(repo
        .meetings()?
        .into_iter()
        .filter(|m| !m.meta.deleted && m.folder_id == Some(folder))
        .collect())
}

/// A new folder named `name` (trimmed). Returns its id.
///
/// # Errors
/// [`EditError::InvalidName`], [`EditError::FolderNameTaken`]; store errors.
pub fn create(services: &Services, name: &str) -> Result<EntityId, RuntimeError> {
    let name = clean_name(name)?;
    write(services, |tx, stamp| {
        check_free(&*tx, &name, None)?;
        let folder = Folder::new(name, stamp);
        let id = folder.id;
        tx.put_folder(folder)?;
        Ok(id)
    })
}

/// Renames `folder` to `name` (trimmed). Renaming to its own name in another case is allowed.
/// Returns whether the row changed (the same name writes nothing).
///
/// # Errors
/// [`EditError::InvalidName`], [`EditError::FolderNotFound`], [`EditError::FolderNameTaken`];
/// store errors.
pub fn rename(services: &Services, folder: EntityId, name: &str) -> Result<bool, RuntimeError> {
    let name = clean_name(name)?;
    write(services, |tx, stamp| {
        let mut row = live_folder(&*tx, folder)?;
        check_free(&*tx, &name, Some(folder))?;
        if row.name == name {
            return Ok(false);
        }
        row.name = name;
        row.meta.touch(stamp.hlc, stamp.clock);
        tx.put_folder(row)?;
        Ok(true)
    })
}

/// Files `meeting` in `folder` (`None`: no folder). Refused while a live session in this
/// process records the meeting; allowed while it finishes or a job runs on it. The meeting
/// row is re-read in the transaction and only its folder changes. Returns whether the row
/// changed.
///
/// # Errors
/// [`EditError::MeetingRecording`], [`EditError::MeetingNotFound`],
/// [`EditError::FolderNotFound`] (the target is deleted or unknown); store errors.
pub fn move_meeting(
    services: &Services,
    meeting: EntityId,
    folder: Option<EntityId>,
) -> Result<bool, RuntimeError> {
    // The activity lock, then the store lock, each briefly and never nested; a recording
    // meeting holds its lease from before the user can see it.
    match services.meeting_activity(meeting)? {
        None => return Err(EditError::MeetingNotFound(meeting).into()),
        Some(MeetingActivity::Recording) => {
            return Err(EditError::MeetingRecording(meeting).into());
        }
        Some(_) => {}
    }
    write(services, |tx, stamp| {
        let mut m = live_meeting(&*tx, meeting)?;
        if let Some(target) = folder {
            live_folder(&*tx, target)?;
        }
        if m.folder_id == folder {
            return Ok(false);
        }
        m.folder_id = folder;
        m.meta.touch(stamp.hlc, stamp.clock);
        tx.put_meeting(m)?;
        Ok(true)
    })
}

/// What deleting a folder did. Counts only, never names.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FolderDeletion {
    /// Whether the folder is gone (a tombstone with its name cleared). `false` only when a
    /// meeting deletion failed and the folder stays with what is left.
    pub folder_deleted: bool,
    /// Meetings deleted ([`delete_with_meetings`]; always 0 when keeping them).
    pub meetings_deleted: usize,
    /// Meetings still in the folder because their deletion failed (busy, or any error).
    pub meetings_left: usize,
    /// Meetings that lost their folder and stay, with no folder, in "All meetings".
    pub meetings_unfiled: usize,
    /// The deletion is committed but the checkpoint that clears the name from the WAL could
    /// not finish (another connection was reading, or it failed): it leaves at the next
    /// checkpoint.
    pub checkpoint_pending: bool,
}

/// The closing transaction's body: the folder becomes a tombstone with its name cleared and
/// the live meetings naming it, selected here, lose their folder. Returns how many lost it.
fn close_folder(
    tx: &mut StoreTx<'_>,
    stamp: &mut DynStamp<'_, '_>,
    folder: EntityId,
) -> Result<usize, RuntimeError> {
    delete_folder(&mut *tx, folder, stamp.hlc, stamp.clock)?;
    let filed = members(&*tx, folder)?;
    let unfiled = filed.len();
    for mut m in filed {
        m.folder_id = None;
        m.meta.touch(stamp.hlc, stamp.clock);
        tx.put_meeting(m)?;
    }
    Ok(unfiled)
}

/// Deletes `folder` and keeps its meetings (the default): one transaction tombstones it,
/// clears its name and clears the folder of every live meeting in it; meetings and their
/// content are untouched. Then the erasure is checkpointed out of the WAL.
///
/// # Errors
/// [`EditError::FolderNotFound`]; store errors.
pub fn delete_keeping_meetings(
    services: &Services,
    folder: EntityId,
) -> Result<FolderDeletion, RuntimeError> {
    let unfiled = write(services, |tx, stamp| {
        live_folder(&*tx, folder)?;
        close_folder(tx, stamp, folder)
    })?;
    Ok(FolderDeletion {
        folder_deleted: true,
        meetings_unfiled: unfiled,
        checkpoint_pending: services.store().flush_erasures().is_err(),
        ..FolderDeletion::default()
    })
}

/// Deletes `folder` and its meetings: each meeting through [`Services::delete_meeting`]
/// (people and voiceprints stay), with no store or activity lock held across the loop. A
/// deletion that fails (the meeting is busy, [`RuntimeError::MeetingBusy`], or any other
/// error) is counted and the rest go on. Only when every one was deleted does the folder go,
/// in the closing transaction of [`delete_keeping_meetings`] (a meeting moved in during the
/// loop is unfiled, not deleted); otherwise the folder stays with what is left.
///
/// # Errors
/// [`EditError::FolderNotFound`]; store errors outside the meeting deletions.
pub fn delete_with_meetings(
    services: &Services,
    folder: EntityId,
) -> Result<FolderDeletion, RuntimeError> {
    delete_with_meetings_then(services, folder, &mut |_| {})
}

/// [`delete_with_meetings`], calling `after_each` (with no lock held) after each meeting's
/// deletion attempt: the seam tests use to move a meeting in during the loop.
pub(crate) fn delete_with_meetings_then(
    services: &Services,
    folder: EntityId,
    after_each: &mut dyn FnMut(EntityId),
) -> Result<FolderDeletion, RuntimeError> {
    let filed: Vec<EntityId> = {
        let db = services.store();
        live_folder(&**db, folder)?;
        members(&**db, folder)?.iter().map(|m| m.id).collect()
    };
    let mut out = FolderDeletion::default();
    let mut failed = 0;
    for meeting in filed {
        match services.delete_meeting(meeting) {
            Ok(_) => out.meetings_deleted += 1,
            Err(_) => failed += 1,
        }
        after_each(meeting);
    }
    if failed > 0 {
        out.meetings_left = members(&**services.store(), folder)?.len();
        return Ok(out);
    }
    out.meetings_unfiled = write(services, |tx, stamp| close_folder(tx, stamp, folder))?;
    out.folder_deleted = true;
    out.checkpoint_pending = services.store().flush_erasures().is_err();
    Ok(out)
}

#[cfg(test)]
#[path = "folders_tests.rs"]
mod tests;
