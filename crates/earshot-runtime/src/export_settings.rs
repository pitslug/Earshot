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
//!   empty is the vault's own top folder. Each folder name must also be one Windows can
//!   create (T14.29: no `< > " | ? *`, no trailing dot or space, no device name such as
//!   CON or COM1). Default [`DEFAULT_FOLDER`] (the export default). The rule is pinned to
//!   the UI mock's `checkFolder` by `fixtures/ui/vault_folder_cases.json`, which both
//!   suites run: a rule or copy change edits both and the table.
//!
//! A stored value this build cannot use (not text, a relative vault, a subfolder the rule
//! refuses) reads as the default and is reported ([`UnknownSetting`]), as the other
//! settings pages do. One exception keeps old exports where they were: a subfolder stored
//! before T14.29 whose names end in dots or spaces reads as the folder Windows made of it
//! (those dropped), when that passes the rule.

use std::fmt;
use std::path::{Path, PathBuf};

use earshot_core::export::ExportOptions;
use earshot_core::model::VaultPath;
use earshot_store::{Store, VaultError, validate_vault};

pub use crate::local::UnknownSetting;
use crate::local::{DeviceRecord, setting_of};
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
    /// A folder name in the subfolder is one Windows refuses (fixed copy, the whole reason).
    FolderName(&'static str),
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
            Self::FolderName(why) => f.write_str(why),
            Self::Store(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for ObsidianSettingsError {}

impl ObsidianSettingsError {
    /// The refusal as the page shows it: the text as a sentence (first letter upper case, a
    /// full stop at the end). The messages of `fixtures/ui/vault_folder_cases.json`.
    #[must_use]
    pub fn message(&self) -> String {
        let text = self.to_string();
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
}

/// A subfolder stored by an earlier version, as exports used it: [`check_folder`], or, when
/// that refuses only for trailing dots or spaces, the folder Windows made of it (each
/// name without them) if that passes. Saving stays strict ([`check_folder`]); a value still
/// refused (a reserved character or device name, which could never have exported) is
/// `None`.
fn stored_folder(folder: &str) -> Option<String> {
    if let Ok(f) = check_folder(folder) {
        return Some(f);
    }
    let windows_made: Vec<&str> = folder
        .trim()
        .trim_matches(['/', '\\'])
        .split(['/', '\\'])
        .map(|name| name.trim_end_matches(['.', ' ']))
        .collect();
    // A name of only dots or spaces is not guessed at.
    if windows_made.iter().any(|n| n.is_empty()) {
        return None;
    }
    check_folder(&windows_made.join("/")).ok()
}

/// The subfolder rule (module docs): `Ok` with the normalised subfolder.
///
/// # Errors
/// [`ObsidianSettingsError::TooLong`], [`ObsidianSettingsError::Folder`],
/// [`ObsidianSettingsError::FolderName`].
pub fn check_folder(folder: &str) -> Result<String, ObsidianSettingsError> {
    let trimmed = folder.trim().trim_matches(['/', '\\']);
    if trimmed.chars().count() > MAX_PATH_CHARS {
        return Err(ObsidianSettingsError::TooLong);
    }
    if trimmed.is_empty() {
        return Ok(String::new());
    }
    let path = VaultPath::new(trimmed).map_err(|e| ObsidianSettingsError::Folder(e.reason))?;
    for name in path.components() {
        if let Some(why) = windows_refuses(name) {
            return Err(ObsidianSettingsError::FolderName(why));
        }
    }
    Ok(path.as_str().to_owned())
}

/// Characters Windows refuses in a file or folder name, beyond the separators and `:`
/// that `VaultPath::new` already handles.
pub const WINDOWS_RESERVED_CHARS: [char; 6] = ['<', '>', '"', '|', '?', '*'];

/// Why Windows cannot create a folder called `name` (one component, already through
/// `VaultPath::new`, so no separator, `:` or control character), or `None`. The rules of
/// Microsoft's "Naming Files, Paths, and Namespaces": no reserved character, no trailing
/// dot or space (Windows would drop them and use another folder), and not a device name
/// (CON, PRN, AUX, NUL, CONIN$, CONOUT$, COM0 to COM9, LPT0 to LPT9, also with the
/// superscript digits ¹ ² ³, in any case, with or without an extension).
fn windows_refuses(name: &str) -> Option<&'static str> {
    if name.contains(WINDOWS_RESERVED_CHARS) {
        return Some(RESERVED_CHARACTER);
    }
    if name.ends_with(['.', ' ']) {
        return Some(TRAILING_DOT_OR_SPACE);
    }
    let base = name.split('.').next().unwrap_or(name).trim_end_matches(' ');
    // COM or LPT and then exactly one digit 0 to 9 or a superscript 1, 2 or 3.
    let numbered = |prefix: &str| {
        base.get(..prefix.len())
            .is_some_and(|p| p.eq_ignore_ascii_case(prefix))
            && {
                let mut rest = base[prefix.len()..].chars();
                matches!(
                    (rest.next(), rest.next()),
                    (Some('0'..='9' | '\u{b9}' | '\u{b2}' | '\u{b3}'), None)
                )
            }
    };
    let reserved = ["CON", "PRN", "AUX", "NUL", "CONIN$", "CONOUT$"]
        .iter()
        .any(|d| base.eq_ignore_ascii_case(d))
        || ["COM", "LPT"].iter().any(|d| numbered(d));
    reserved.then_some(DEVICE_NAME)
}

/// The refusal for a reserved character in the subfolder.
pub const RESERVED_CHARACTER: &str = "a folder name cannot contain < > \" | ? or *";
/// The refusal for a folder name ending with a dot or a space.
pub const TRAILING_DOT_OR_SPACE: &str = "a folder name cannot end with a dot or a space";
/// The refusal for a Windows device name.
pub const DEVICE_NAME: &str = "CON, PRN, AUX, NUL, CONIN$, CONOUT$, COM0 to COM9 and LPT0 to LPT9 are names Windows keeps for itself";

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
    // Both are read strictly: a stored value that is not UTF-8 is reported (lossily), never used.
    let lossy = |bytes: &[u8]| String::from_utf8_lossy(bytes).into_owned();
    if let Some(v) = setting_of(
        VAULT_KEY,
        store.local_value(VAULT_KEY)?.as_deref(),
        |bytes| match std::str::from_utf8(bytes) {
            Ok(text) => vault_path(text).map_err(|_| text.to_owned()),
            Err(_) => Err(lossy(bytes)),
        },
    )
    .known(&mut unknown)
    {
        out.vault = v;
    }
    if let Some(f) = setting_of(
        FOLDER_KEY,
        store.local_value(FOLDER_KEY)?.as_deref(),
        |bytes| match std::str::from_utf8(bytes).ok().and_then(stored_folder) {
            Some(f) => Ok(f),
            None => Err(lossy(bytes)),
        },
    )
    .known(&mut unknown)
    {
        out.folder = f;
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
