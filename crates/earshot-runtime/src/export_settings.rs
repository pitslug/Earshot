//! The Obsidian page settings (T7.4h, PLAN.md 3.8): the vault folder exported notes go to
//! and the subfolder inside it. Device-local (`local_state`, never synced, no migration),
//! one key each, written only through [`set_obsidian_settings`] in one transaction.
//!
//! - **The vault** ([`VAULT_KEY`]): an absolute folder path, checked when saved with the
//!   store's own rule (`earshot_store::validate_vault`: it exists, is a folder, and a file
//!   can be created in it), which every export applies again. Empty: no vault, so export
//!   is refused with "set your vault". A stored vault is not re-checked when read (it may be
//!   on a drive that is not attached now); the export then fails with the store's reason.
//! - **The subfolder** ([`FOLDER_KEY`]): vault-relative, the rule of the export itself
//!   (`VaultPath::new`: no absolute path, drive, `.` or `..` component or control
//!   character; backslashes become `/`), with leading and trailing separators dropped;
//!   empty is the vault's own top folder. Default [`DEFAULT_FOLDER`] (the export default).
//!
//! A stored value this build cannot use (not text, a relative vault, a subfolder the rule
//! refuses) reads as the default and is reported ([`UnknownSetting`]), as the other
//! settings pages do.

use std::fmt;
use std::path::{Path, PathBuf};

use earshot_core::export::ExportOptions;
use earshot_core::model::VaultPath;
use earshot_store::{Store, VaultError, validate_vault};

use crate::local::DeviceRecord;
pub use crate::model_settings::UnknownSetting;
use crate::{RuntimeError, Services};

#[cfg(test)]
#[path = "export_settings_tests.rs"]
mod tests;

/// `local_state` key of the vault folder.
pub const VAULT_KEY: &str = "settings/obsidian-vault";
/// `local_state` key of the subfolder inside the vault.
pub const FOLDER_KEY: &str = "settings/obsidian-folder";
/// The subfolder when none is stored (`ExportOptions::default().folder`).
pub const DEFAULT_FOLDER: &str = "Meetings";
/// Longest vault path or subfolder accepted (characters).
pub const MAX_PATH_CHARS: usize = 1024;

/// The Obsidian settings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObsidianSettings {
    /// The vault folder (absolute); `None` until one is set.
    pub vault: Option<PathBuf>,
    /// The subfolder inside the vault, `/`-separated ("" for the vault's top folder).
    pub folder: String,
}

impl Default for ObsidianSettings {
    fn default() -> Self {
        Self {
            vault: None,
            folder: DEFAULT_FOLDER.to_owned(),
        }
    }
}

impl ObsidianSettings {
    /// The export options of these settings (the other options keep their defaults).
    #[must_use]
    pub fn export_options(&self) -> ExportOptions {
        ExportOptions {
            folder: self.folder.clone(),
            ..ExportOptions::default()
        }
    }
}

/// Why the settings were not saved (nothing was written).
#[derive(Debug)]
pub enum ObsidianSettingsError {
    /// The vault path is not a full path (it must start with a drive or `\\server`).
    VaultNotAbsolute,
    /// The vault path or subfolder is longer than [`MAX_PATH_CHARS`].
    TooLong,
    /// The vault folder is unusable (missing, not a folder, not writable).
    Vault(VaultError),
    /// The subfolder is not a path inside the vault.
    Folder(&'static str),
    /// The store failed.
    Store(RuntimeError),
}

impl fmt::Display for ObsidianSettingsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::VaultNotAbsolute => f.write_str(
                "the vault must be a full folder path, for example C:\\Users\\you\\Notes",
            ),
            Self::TooLong => write!(f, "the path is longer than {MAX_PATH_CHARS} characters"),
            Self::Vault(VaultError::Missing(_)) => f.write_str("this folder does not exist"),
            Self::Vault(VaultError::NotADirectory(_)) => f.write_str("this is not a folder"),
            Self::Vault(VaultError::NotWritable(_, e)) => {
                write!(f, "Earshot cannot write in this folder ({e})")
            }
            Self::Folder(why) => {
                write!(f, "the subfolder must be a folder inside the vault ({why})")
            }
            Self::Store(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for ObsidianSettingsError {}

/// The subfolder rule (module docs): `Ok` with the normalised subfolder.
///
/// # Errors
/// [`ObsidianSettingsError::TooLong`], [`ObsidianSettingsError::Folder`].
pub fn check_folder(folder: &str) -> Result<String, ObsidianSettingsError> {
    let trimmed = folder.trim().trim_matches(['/', '\\']);
    if trimmed.chars().count() > MAX_PATH_CHARS {
        return Err(ObsidianSettingsError::TooLong);
    }
    if trimmed.is_empty() {
        return Ok(String::new());
    }
    VaultPath::new(trimmed)
        .map(|p| p.as_str().to_owned())
        .map_err(|e| ObsidianSettingsError::Folder(e.reason))
}

/// The vault path's shape (absolute, not too long); `None` for an empty one. Does not
/// touch the disk.
fn vault_path(vault: &str) -> Result<Option<PathBuf>, ObsidianSettingsError> {
    let vault = vault.trim();
    if vault.is_empty() {
        return Ok(None);
    }
    if vault.chars().count() > MAX_PATH_CHARS {
        return Err(ObsidianSettingsError::TooLong);
    }
    let path = Path::new(vault);
    if !path.is_absolute() {
        return Err(ObsidianSettingsError::VaultNotAbsolute);
    }
    Ok(Some(path.to_owned()))
}

/// Checks `vault` (empty: none) and `folder` as typed (module docs). The vault check
/// creates and removes a probe file in the folder, so no store lock may be held.
///
/// # Errors
/// [`ObsidianSettingsError`] (not `Store`).
pub fn validate(vault: &str, folder: &str) -> Result<ObsidianSettings, ObsidianSettingsError> {
    let folder = check_folder(folder)?;
    let vault = vault_path(vault)?;
    if let Some(v) = &vault {
        validate_vault(v).map_err(ObsidianSettingsError::Vault)?;
    }
    Ok(ObsidianSettings { vault, folder })
}

/// The stored settings (one read each) and the stored values this build cannot use.
///
/// # Errors
/// Store errors.
pub fn obsidian_settings(
    store: &Store,
) -> Result<(ObsidianSettings, Vec<UnknownSetting>), RuntimeError> {
    let mut out = ObsidianSettings::default();
    let mut unknown = Vec::new();
    if let Some(bytes) = store.local_value(VAULT_KEY)? {
        match String::from_utf8(bytes) {
            Ok(text) => match vault_path(&text) {
                Ok(v) => out.vault = v,
                Err(_) => unknown.push(UnknownSetting {
                    key: VAULT_KEY,
                    stored: text,
                }),
            },
            Err(e) => unknown.push(UnknownSetting {
                key: VAULT_KEY,
                stored: String::from_utf8_lossy(e.as_bytes()).into_owned(),
            }),
        }
    }
    if let Some(bytes) = store.local_value(FOLDER_KEY)? {
        let text = String::from_utf8_lossy(&bytes).into_owned();
        match std::str::from_utf8(&bytes).map(check_folder) {
            Ok(Ok(f)) => out.folder = f,
            _ => unknown.push(UnknownSetting {
                key: FOLDER_KEY,
                stored: text,
            }),
        }
    }
    Ok((out, unknown))
}

/// Writes checked settings in one transaction.
///
/// # Errors
/// Store errors.
fn store_settings(store: &mut Store, s: &ObsidianSettings) -> Result<(), RuntimeError> {
    let vault = s
        .vault
        .as_ref()
        .map(|v| v.to_string_lossy().into_owned())
        .unwrap_or_default();
    store
        .transaction(|tx| -> Result<(), earshot_store::StoreError> {
            crate::local::set_tx(tx, &DeviceRecord::ObsidianVault.key(), vault.as_bytes())?;
            crate::local::set_tx(tx, &DeviceRecord::ObsidianFolder.key(), s.folder.as_bytes())?;
            Ok(())
        })
        .map_err(RuntimeError::from)
}

/// Checks `vault` and `folder` ([`validate`], with the store lock free) and stores them
/// together. Returns what was stored.
///
/// # Errors
/// [`ObsidianSettingsError`]; nothing is stored then.
pub fn set_obsidian_settings(
    services: &Services,
    vault: &str,
    folder: &str,
) -> Result<ObsidianSettings, ObsidianSettingsError> {
    let checked = validate(vault, folder)?;
    store_settings(&mut services.store(), &checked).map_err(ObsidianSettingsError::Store)?;
    Ok(checked)
}
