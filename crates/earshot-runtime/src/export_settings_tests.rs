//! Tests of the Obsidian page settings (T7.4h). Every name contains `export_settings`. Vaults
//! are temp folders, never a real vault.

use super::*;
use crate::testutil::{TempDir, services};

#[test]
fn export_settings_default_to_no_vault_and_the_export_folder() {
    let dir = TempDir::new("export-settings-default");
    let (svc, _) = services(&dir);
    let (s, unknown) = obsidian_settings(&svc.store()).unwrap();
    assert_eq!(s, ObsidianSettings::default());
    assert_eq!(s.vault, None);
    assert_eq!(s.folder, DEFAULT_FOLDER);
    assert_eq!(DEFAULT_FOLDER, ExportOptions::default().folder);
    assert_eq!(s.export_options(), ExportOptions::default());
    assert!(unknown.is_empty());
}

#[test]
fn export_settings_store_a_checked_vault_and_folder_together() {
    let dir = TempDir::new("export-settings-store");
    let (svc, _) = services(&dir);
    let vault = dir.0.join("My vault");
    std::fs::create_dir_all(&vault).unwrap();
    let saved = set_obsidian_settings(
        &svc,
        &format!("  {}  ", vault.display()),
        "\\Work\\Meetings\\",
    )
    .unwrap();
    assert_eq!(saved.vault.as_deref(), Some(vault.as_path()));
    assert_eq!(saved.folder, "Work/Meetings");
    let (read, unknown) = obsidian_settings(&svc.store()).unwrap();
    assert_eq!(read, saved);
    assert!(unknown.is_empty());
    assert_eq!(read.export_options().folder, "Work/Meetings");
    // The vault's top folder.
    let top = set_obsidian_settings(&svc, &vault.display().to_string(), " / ").unwrap();
    assert_eq!(top.folder, "");
    assert_eq!(obsidian_settings(&svc.store()).unwrap().0.folder, "");
    // Clearing the vault keeps the subfolder rule.
    let cleared = set_obsidian_settings(&svc, "", "Notes").unwrap();
    assert_eq!(cleared.vault, None);
    assert_eq!(obsidian_settings(&svc.store()).unwrap().0, cleared);
    // The probe file of the check is gone.
    let left: Vec<_> = std::fs::read_dir(&vault).unwrap().collect();
    assert!(left.is_empty(), "{left:?}");
}

#[test]
fn export_settings_refuse_unusable_vaults_and_folders_and_store_nothing() {
    let dir = TempDir::new("export-settings-refuse");
    let (svc, _) = services(&dir);
    let vault = dir.0.join("vault");
    std::fs::create_dir_all(&vault).unwrap();
    let file = dir.0.join("note.md");
    std::fs::write(&file, "x").unwrap();
    let good = vault.display().to_string();
    let missing = dir.0.join("missing").display().to_string();
    let long = "x".repeat(MAX_PATH_CHARS + 1);
    let cases: [(String, &str, &str); 8] = [
        ("Notes".into(), "Meetings", "absolute"),
        (missing, "Meetings", "missing"),
        (file.display().to_string(), "Meetings", "not a folder"),
        (good.clone(), "../outside", "folder"),
        (good.clone(), "a/./b", "folder"),
        (good.clone(), "C:\\Elsewhere", "folder"),
        (good.clone(), "a\u{7}b", "folder"),
        (good.clone(), &long, "too long"),
    ];
    let kind = |e: &ObsidianSettingsError| match e {
        ObsidianSettingsError::VaultNotAbsolute => "absolute",
        ObsidianSettingsError::Vault(VaultError::Missing(_)) => "missing",
        ObsidianSettingsError::Vault(VaultError::NotADirectory(_)) => "not a folder",
        ObsidianSettingsError::Vault(VaultError::NotWritable(..)) => "not writable",
        ObsidianSettingsError::Folder(_) => "folder",
        ObsidianSettingsError::TooLong => "too long",
        ObsidianSettingsError::Store(_) => "store",
    };
    for (vault, folder, want) in cases {
        let e = set_obsidian_settings(&svc, &vault, folder).unwrap_err();
        assert_eq!(kind(&e), want, "{vault:?} {folder:?}: {e:?}");
        // Plain text for the page, no em dash.
        assert!(!e.to_string().contains('\u{2014}'));
        assert_eq!(
            obsidian_settings(&svc.store()).unwrap().0,
            ObsidianSettings::default(),
            "{vault:?} {folder:?} stored something"
        );
    }
    // An absolute path to a drive's own folder is accepted when it is usable.
    set_obsidian_settings(&svc, &good, "Meetings").unwrap();
}

#[test]
fn export_settings_report_stored_values_they_cannot_use() {
    let dir = TempDir::new("export-settings-unknown");
    let (svc, _) = services(&dir);
    {
        let mut db = svc.store();
        db.set_local_value(VAULT_KEY, b"relative\\vault").unwrap();
        db.set_local_value(FOLDER_KEY, b"../up").unwrap();
    }
    let (s, unknown) = obsidian_settings(&svc.store()).unwrap();
    assert_eq!(s, ObsidianSettings::default());
    let keys: Vec<&str> = unknown.iter().map(|u| u.key).collect();
    assert_eq!(keys, [VAULT_KEY, FOLDER_KEY]);
    assert_eq!(unknown[1].stored, "../up");
    {
        let mut db = svc.store();
        db.set_local_value(VAULT_KEY, &[0xff, 0xfe]).unwrap();
        db.set_local_value(FOLDER_KEY, &[0xff]).unwrap();
    }
    let (s, unknown) = obsidian_settings(&svc.store()).unwrap();
    assert_eq!(s, ObsidianSettings::default());
    assert_eq!(unknown.len(), 2);
}

#[test]
fn export_settings_keys_are_in_the_registry() {
    assert_eq!(DeviceRecord::ObsidianVault.key().as_str(), VAULT_KEY);
    assert_eq!(DeviceRecord::ObsidianFolder.key().as_str(), FOLDER_KEY);
}
