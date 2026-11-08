//! Folders in the side panel (T9.5c, docs/folders-design.md): the IPC commands over
//! `earshot_runtime::folders`, which owns every folder read and edit. This module only maps
//! ids and results to the IPC types. Refusals reach the UI as `detail::edit_error`'s fixed
//! copy; a folder name is content and never goes into a log or an error.

use earshot_runtime::Services;
use earshot_runtime::folders::{self, FolderDeletion, FolderItem};
use serde::Serialize;
use specta::Type;
use tauri::State;

use crate::detail::parse_id;
use crate::ipc::{AppState, IpcError, blocking};

#[cfg(test)]
#[path = "folders_tests.rs"]
mod tests;

/// A live folder as the side panel shows it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Type)]
pub struct FolderListItem {
    /// Folder id (hyphenated UUID).
    pub id: String,
    /// Its name.
    pub name: String,
}

impl From<FolderItem> for FolderListItem {
    fn from(f: FolderItem) -> Self {
        Self {
            id: f.id.to_string(),
            name: f.name,
        }
    }
}

/// What deleting a folder did: counts only.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Type)]
pub struct FolderDeleted {
    /// Whether the folder is gone. `false` only when some of its meetings could not be deleted:
    /// the folder then stays with them.
    pub folder_deleted: bool,
    /// Meetings deleted with the folder.
    pub meetings_deleted: u32,
    /// Meetings still in the folder because their deletion failed (busy).
    pub meetings_left: u32,
    /// Meetings that lost their folder and now show only in "All meetings".
    pub meetings_unfiled: u32,
}

fn count(n: usize) -> u32 {
    u32::try_from(n).unwrap_or(u32::MAX)
}

impl From<FolderDeletion> for FolderDeleted {
    fn from(d: FolderDeletion) -> Self {
        Self {
            folder_deleted: d.folder_deleted,
            meetings_deleted: count(d.meetings_deleted),
            meetings_left: count(d.meetings_left),
            meetings_unfiled: count(d.meetings_unfiled),
        }
    }
}

/// Every live folder, by name.
///
/// # Errors
/// Store errors.
pub fn folder_list(services: &Services) -> Result<Vec<FolderListItem>, IpcError> {
    Ok(folders::list(services)?
        .into_iter()
        .map(FolderListItem::from)
        .collect())
}

/// Creates a folder named `name`; returns its id.
///
/// # Errors
/// `invalid_name`, `folder_name_taken`; store errors.
pub fn create(services: &Services, name: &str) -> Result<String, IpcError> {
    Ok(folders::create(services, name)?.to_string())
}

/// Renames `folder` to `name`.
///
/// # Errors
/// `invalid_id`, `invalid_name`, `folder_not_found`, `folder_name_taken`; store errors.
pub fn rename(services: &Services, folder: &str, name: &str) -> Result<(), IpcError> {
    folders::rename(services, parse_id(folder)?, name)?;
    Ok(())
}

/// Files `meeting` in `folder` (`None`: no folder).
///
/// # Errors
/// `invalid_id`, `meeting_not_found`, `folder_not_found`, `meeting_recording`; store errors.
pub fn move_meeting(
    services: &Services,
    meeting: &str,
    folder: Option<&str>,
) -> Result<(), IpcError> {
    let meeting = parse_id(meeting)?;
    let folder = folder.map(parse_id).transpose()?;
    folders::move_meeting(services, meeting, folder)?;
    Ok(())
}

/// Deletes `folder`, keeping its meetings (`with_meetings` false, the default the dialog
/// offers) or deleting them too.
///
/// # Errors
/// `invalid_id`, `folder_not_found`; store errors.
pub fn delete(
    services: &Services,
    folder: &str,
    with_meetings: bool,
) -> Result<FolderDeleted, IpcError> {
    let folder = parse_id(folder)?;
    let done = if with_meetings {
        folders::delete_with_meetings(services, folder)?
    } else {
        folders::delete_keeping_meetings(services, folder)?
    };
    Ok(done.into())
}

/// Every live folder, by name.
#[tauri::command]
#[specta::specta]
pub async fn list_folders(state: State<'_, AppState>) -> Result<Vec<FolderListItem>, IpcError> {
    let services = state.services()?;
    blocking(move || folder_list(&services)).await
}

/// Creates a folder; returns its id. Refuses an unusable name or one another folder has.
#[tauri::command]
#[specta::specta]
pub async fn create_folder(state: State<'_, AppState>, name: String) -> Result<String, IpcError> {
    let services = state.services()?;
    blocking(move || create(&services, &name)).await
}

/// Renames a folder. Refuses an unusable name or one another folder has.
#[tauri::command]
#[specta::specta]
pub async fn rename_folder(
    state: State<'_, AppState>,
    folder: String,
    name: String,
) -> Result<(), IpcError> {
    let services = state.services()?;
    blocking(move || rename(&services, &folder, &name)).await
}

/// Moves a meeting into a folder, or out of any (`None`). Refused while it records.
#[tauri::command]
#[specta::specta]
pub async fn move_to_folder(
    state: State<'_, AppState>,
    meeting: String,
    folder: Option<String>,
) -> Result<(), IpcError> {
    let services = state.services()?;
    blocking(move || move_meeting(&services, &meeting, folder.as_deref())).await
}

/// Deletes a folder; its meetings stay and show in "All meetings".
#[tauri::command]
#[specta::specta]
pub async fn delete_folder_keeping_meetings(
    state: State<'_, AppState>,
    folder: String,
) -> Result<FolderDeleted, IpcError> {
    let services = state.services()?;
    blocking(move || delete(&services, &folder, false)).await
}

/// Deletes a folder and its meetings. A meeting that is busy stays, and the folder stays with
/// it: the result says how many were deleted and how many are left.
#[tauri::command]
#[specta::specta]
pub async fn delete_folder_with_meetings(
    state: State<'_, AppState>,
    folder: String,
) -> Result<FolderDeleted, IpcError> {
    let services = state.services()?;
    blocking(move || delete(&services, &folder, true)).await
}
