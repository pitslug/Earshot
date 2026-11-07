//! The Obsidian page of Settings (T7.4h): the vault folder and the subfolder notes are
//! exported to. The settings and their rules are `earshot_runtime::export_settings`
//! (checked when saved; export reads them, `detail::export`); this module only maps them
//! to the IPC types.

use earshot_runtime::Services;
use earshot_runtime::export_settings::{
    self, DEFAULT_FOLDER, ObsidianSettings, ObsidianSettingsError,
};
use serde::{Deserialize, Serialize};
use specta::Type;

use crate::ipc::IpcError;

/// The Obsidian settings as the UI sends them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct ObsidianSettingsValue {
    /// The vault folder ("" for none).
    pub vault: String,
    /// The subfolder inside the vault ("" for the vault's top folder).
    pub folder: String,
}

/// The stored settings with what the page says about them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Type)]
pub struct ObsidianSettingsView {
    /// The settings in use.
    pub value: ObsidianSettingsValue,
    /// One line per stored value this version cannot use.
    pub unknown: Vec<String>,
    /// The subfolder used when none is given.
    pub default_folder: String,
}

fn view(s: &ObsidianSettings, unknown: Vec<String>) -> ObsidianSettingsView {
    ObsidianSettingsView {
        value: ObsidianSettingsValue {
            vault: s
                .vault
                .as_ref()
                .map(|v| v.display().to_string())
                .unwrap_or_default(),
            folder: s.folder.clone(),
        },
        unknown,
        default_folder: DEFAULT_FOLDER.into(),
    }
}

/// The settings of `services`' store.
///
/// # Errors
/// Store errors.
pub fn settings_view(services: &Services) -> Result<ObsidianSettingsView, IpcError> {
    let (s, unknown) = export_settings::obsidian_settings(&services.store())?;
    Ok(view(&s, unknown.iter().map(ToString::to_string).collect()))
}

/// `text` with its first letter upper-case and a full stop.
fn sentence(text: &str) -> String {
    let mut c = text.chars();
    let mut out: String = c
        .next()
        .map(|f| f.to_uppercase().collect())
        .unwrap_or_default();
    out.push_str(c.as_str());
    if !out.ends_with('.') {
        out.push('.');
    }
    out
}

/// Checks and stores `value` (the vault folder is checked on disk); returns the settings as
/// stored.
///
/// # Errors
/// `invalid_setting` (nothing stored, the reason in plain words); store errors.
pub fn set_settings(
    services: &Services,
    value: &ObsidianSettingsValue,
) -> Result<ObsidianSettingsView, IpcError> {
    match export_settings::set_obsidian_settings(services, &value.vault, &value.folder) {
        Ok(s) => Ok(view(&s, Vec::new())),
        Err(ObsidianSettingsError::Store(e)) => Err(e.into()),
        Err(other) => Err(IpcError {
            kind: "invalid_setting".into(),
            message: sentence(&other.to_string()),
            retryable: false,
        }),
    }
}

/// The Obsidian page settings (and stored values this version cannot use).
#[tauri::command]
#[specta::specta]
pub async fn obsidian_settings(
    state: tauri::State<'_, crate::ipc::AppState>,
) -> Result<ObsidianSettingsView, IpcError> {
    let services = state.services()?;
    crate::ipc::blocking(move || settings_view(&services)).await
}

/// Checks (the vault folder on disk) and stores the Obsidian page settings
/// (`invalid_setting`: nothing stored).
#[tauri::command]
#[specta::specta]
pub async fn set_obsidian_settings(
    state: tauri::State<'_, crate::ipc::AppState>,
    settings: ObsidianSettingsValue,
) -> Result<ObsidianSettingsView, IpcError> {
    let services = state.services()?;
    crate::ipc::blocking(move || set_settings(&services, &settings)).await
}

/// The folder dialog's title.
pub const VAULT_DIALOG_TITLE: &str = "Choose your Obsidian vault folder";

/// Opens the Windows folder dialog over the app window, starting at `start` (the vault field's
/// text, when that folder exists), and returns the folder chosen (`None`: cancelled). Stores
/// nothing: the page checks and stores it on Save.
#[tauri::command]
#[specta::specta]
pub async fn pick_obsidian_vault(
    window: tauri::WebviewWindow,
    start: String,
) -> Result<Option<String>, IpcError> {
    #[cfg(windows)]
    {
        let owner = window.hwnd().map_or(0, |h| h.0 as isize);
        crate::ipc::blocking(move || {
            crate::folder_picker_win::pick_folder(owner, &start, VAULT_DIALOG_TITLE)
                .map_err(|_| folder_dialog_failed())
        })
        .await
    }
    #[cfg(not(windows))]
    {
        let _ = (window, start);
        Err(folder_dialog_failed())
    }
}

/// The refusal when the dialog cannot open (the page keeps the field for typing).
fn folder_dialog_failed() -> IpcError {
    IpcError {
        kind: "folder_dialog".into(),
        message: "The folder window could not open. Type the vault folder instead.".into(),
        retryable: true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{TempDir, services};

    fn value(vault: &str, folder: &str) -> ObsidianSettingsValue {
        ObsidianSettingsValue {
            vault: vault.into(),
            folder: folder.into(),
        }
    }

    #[test]
    fn obsidian_settings_round_trip_through_the_ipc_types() {
        let dir = TempDir::new("obsidian-settings");
        let svc = services(&dir);
        let first = settings_view(&svc).unwrap();
        assert_eq!(first.value, value("", "Meetings"));
        assert_eq!(first.default_folder, "Meetings");
        assert!(first.unknown.is_empty());
        let vault = dir.0.join("vault");
        std::fs::create_dir_all(&vault).unwrap();
        let v = vault.display().to_string();
        let saved = set_settings(&svc, &value(&v, "Work\\Calls\\")).unwrap();
        assert_eq!(saved.value, value(&v, "Work/Calls"));
        assert_eq!(settings_view(&svc).unwrap(), saved);
    }

    #[test]
    fn obsidian_settings_refusals_are_plain_sentences_and_store_nothing() {
        let dir = TempDir::new("obsidian-refuse");
        let svc = services(&dir);
        let missing = dir.0.join("nowhere").display().to_string();
        let e = set_settings(&svc, &value(&missing, "Meetings")).unwrap_err();
        assert_eq!(e.kind, "invalid_setting");
        assert_eq!(e.message, "This folder does not exist.");
        assert!(
            !e.message.contains(&missing),
            "the page shows the path already"
        );
        let e = set_settings(&svc, &value("", "../up")).unwrap_err();
        assert_eq!(e.kind, "invalid_setting");
        assert!(
            e.message
                .starts_with("The subfolder must be a folder inside the vault")
        );
        let e = set_settings(&svc, &value("Notes", "")).unwrap_err();
        assert!(
            e.message
                .starts_with("The vault must be a full folder path")
        );
        assert_eq!(settings_view(&svc).unwrap().value, value("", "Meetings"));
    }

    #[test]
    fn obsidian_folder_dialog_failure_is_retryable_and_says_what_to_do() {
        let e = folder_dialog_failed();
        assert_eq!(e.kind, "folder_dialog");
        assert!(e.retryable);
        assert!(e.message.contains("Type the vault folder"));
    }

    #[test]
    #[cfg(windows)]
    #[ignore = "opens the Windows folder dialog and waits for a person: cargo test -p earshot-desktop --bins obsidian_folder_picker -- --ignored --nocapture"]
    fn obsidian_folder_picker_opens_and_returns_a_folder() {
        let start = std::env::temp_dir().display().to_string();
        let picked = crate::folder_picker_win::pick_folder(0, &start, VAULT_DIALOG_TITLE).unwrap();
        println!("picked: {picked:?}");
        if let Some(p) = picked {
            assert!(std::path::Path::new(&p).is_dir(), "{p}");
        }
    }
}
