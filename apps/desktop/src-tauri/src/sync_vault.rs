//! The sync vault key and device token in Windows Credential Manager (T11.3a,
//! docs/sync-spec.md section 6.1): generic credentials `Earshot/sync-key` and
//! `Earshot/sync-token` of the Windows account, local-machine persistence, written only when a
//! device sets up or joins sync and deleted when it is removed. The UI never reads either back:
//! the runtime's sync thread reads them through [`SyncVault::load`] for each tick. The Win32
//! calls and their `unsafe` live in `credentials_win.rs`; this file is portable.
//!
//! Wiped: the copies handed to and read from Credential Manager. Not wiped: the runtime's
//! `VaultKey` is wiped on drop by `earshot-sync`, and its `DeviceToken` by the runtime.

use earshot_runtime::sync::VaultKey;
use earshot_runtime::sync::{DeviceToken, SyncSecrets, SyncVault, VaultError};

/// The Credential Manager target of the vault key.
pub const SYNC_KEY_TARGET: &str = "Earshot/sync-key";
/// The Credential Manager target of the device token.
pub const SYNC_TOKEN_TARGET: &str = "Earshot/sync-token";
/// The user name stored with them (Credential Manager shows it; not a secret).
#[cfg(windows)]
const USER: &str = "Earshot";
/// Longest token accepted when read back (the relay's are 44 characters).
#[cfg(windows)]
const MAX_TOKEN_BYTES: usize = 256;

/// The vault key and token in Credential Manager, under two targets.
#[derive(Debug, Clone)]
pub struct SyncCredentials {
    key_target: String,
    token_target: String,
}

impl SyncCredentials {
    /// The app's own targets.
    #[must_use]
    pub fn app() -> Self {
        Self::with_targets(SYNC_KEY_TARGET, SYNC_TOKEN_TARGET)
    }

    /// Other targets (the round-trip test uses a unique pair per run).
    #[must_use]
    pub fn with_targets(key_target: &str, token_target: &str) -> Self {
        Self {
            key_target: key_target.to_owned(),
            token_target: token_target.to_owned(),
        }
    }
}

#[cfg(windows)]
impl SyncVault for SyncCredentials {
    fn save(&self, key: &VaultKey, token: &DeviceToken) -> Result<(), VaultError> {
        use crate::credentials_win as win;
        win::write(&self.key_target, USER, key.as_bytes()).map_err(|_| VaultError::Store)?;
        if win::write(&self.token_target, USER, token.as_str().as_bytes()).is_err() {
            // Never leave a key without its token.
            let _ = win::delete(&self.key_target);
            return Err(VaultError::Store);
        }
        Ok(())
    }

    fn has(&self) -> Result<bool, VaultError> {
        Ok(self.load()?.is_some())
    }

    fn remove(&self) -> Result<(), VaultError> {
        use crate::credentials_win as win;
        let key = win::delete(&self.key_target);
        let token = win::delete(&self.token_target);
        key.and(token).map(|_| ()).map_err(|_| VaultError::Store)
    }

    fn load(&self) -> Result<Option<SyncSecrets>, VaultError> {
        use crate::credentials_win as win;
        let Some(key) = win::read(&self.key_target).map_err(|_| VaultError::Store)? else {
            return Ok(None);
        };
        let token = match win::read(&self.token_target) {
            Ok(Some(t)) => t,
            Ok(None) => {
                win::wipe(key);
                return Ok(None);
            }
            Err(_) => {
                win::wipe(key);
                return Err(VaultError::Store);
            }
        };
        let vault_key = <[u8; 32]>::try_from(key.as_slice())
            .ok()
            .map(VaultKey::from_bytes);
        win::wipe(key);
        // A token that is not UTF-8 is handed back by `from_utf8` and wiped too.
        let token_text = if token.len() <= MAX_TOKEN_BYTES {
            String::from_utf8(token)
                .map_err(|e| win::wipe(e.into_bytes()))
                .ok()
        } else {
            win::wipe(token);
            None
        };
        match (vault_key, token_text) {
            (Some(key), Some(t)) => Ok(Some(SyncSecrets {
                key,
                token: DeviceToken::new(t),
            })),
            // Something else wrote these targets: treated as not set up.
            (_, t) => {
                if let Some(t) = t {
                    crate::credentials::wipe_string(t);
                }
                Ok(None)
            }
        }
    }
}

#[cfg(not(windows))]
impl SyncVault for SyncCredentials {
    fn save(&self, _: &VaultKey, _: &DeviceToken) -> Result<(), VaultError> {
        Err(VaultError::Unsupported)
    }

    fn has(&self) -> Result<bool, VaultError> {
        Err(VaultError::Unsupported)
    }

    fn remove(&self) -> Result<(), VaultError> {
        Err(VaultError::Unsupported)
    }

    fn load(&self) -> Result<Option<SyncSecrets>, VaultError> {
        Err(VaultError::Unsupported)
    }
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;

    /// Deletes the test's targets however the test ends.
    struct Cleanup(SyncCredentials);

    impl Drop for Cleanup {
        fn drop(&mut self) {
            let _ = self.0.remove();
        }
    }

    #[test]
    fn sync_vault_round_trips_through_credential_manager() {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        let tag = format!("{}-{nanos}", std::process::id());
        let vault = SyncCredentials::with_targets(
            &format!("Earshot/test-sync-key-{tag}"),
            &format!("Earshot/test-sync-token-{tag}"),
        );
        let _cleanup = Cleanup(vault.clone());
        assert!(!vault.has().unwrap());
        assert!(vault.load().unwrap().is_none());
        let key = VaultKey::from_bytes([0x5A; 32]);
        vault
            .save(&key, &DeviceToken::new("invented-sync-token".into()))
            .unwrap();
        assert!(vault.has().unwrap());
        let back = vault.load().unwrap().unwrap();
        assert_eq!(back.key.as_bytes(), &[0x5A; 32]);
        assert_eq!(back.token.as_str(), "invented-sync-token");
        // A key of the wrong length (something else wrote the target) reads as not set up.
        crate::credentials_win::write(&vault.key_target, USER, b"short").unwrap();
        assert!(vault.load().unwrap().is_none());
        vault.remove().unwrap();
        assert!(!vault.has().unwrap());
        // Removing what is not there is not an error.
        vault.remove().unwrap();
    }
}
