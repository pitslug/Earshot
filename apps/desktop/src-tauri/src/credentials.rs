//! The Anthropic API key in Windows Credential Manager (T7.4g).
//!
//! The key is pasted once on the Summaries page and kept as a generic credential of the
//! Windows account (encrypted to it, local-machine persistence: never roaming), never in
//! Earshot's database, logs, events or IPC responses. The UI can only store it, ask whether
//! one is stored and remove it ([`KeyVault`]); the runtime reads it when a summary service
//! is built ([`ApiKeyStore`]) as `earshot_llm`'s redacted [`ApiKey`]. The Win32 calls and
//! all their `unsafe` live in `credentials_win.rs`; this file is portable.
//!
//! What is wiped (zeroed before it is freed): the copy of the key handed to `CredWriteW`,
//! the blob `CredReadW` returns (before `CredFree`) and the copy made of it, and the IPC
//! argument of `set_api_key` ([`wipe_string`]). What is not: the [`ApiKey`] built from a
//! read (`earshot_llm::ApiKey` holds a plain `String`, and `ApiKey::new` makes trimmed
//! copies), the request header values the earshot-llm client builds from it, and anything
//! Tauri's IPC layer buffered. Those are ordinary heap memory freed without zeroing.

use earshot_runtime::summary_settings::{ApiKey, ApiKeyError, ApiKeyStore};

/// The Credential Manager target of the Anthropic API key.
pub const ANTHROPIC_KEY_TARGET: &str = "Earshot/anthropic-api-key";
/// The user name stored with it (Credential Manager shows it; not a secret).
const KEY_USER: &str = "Earshot";
/// Longest key accepted, in bytes (Credential Manager allows 2560 for a generic blob).
pub const MAX_KEY_BYTES: usize = 1024;

/// What the Summaries page may do with the key: store it, ask whether one is stored,
/// remove it. Reading it is only [`ApiKeyStore`] (the runtime, at use time).
pub trait KeyVault: ApiKeyStore + Send + Sync {
    /// Stores `key` (replacing an earlier one), after [`checked_key`].
    ///
    /// # Errors
    /// [`ApiKeyError::Invalid`] (nothing stored); the store failing.
    fn save(&self, key: &str) -> Result<(), ApiKeyError>;
    /// Whether a key is stored.
    ///
    /// # Errors
    /// The store failing.
    fn has(&self) -> Result<bool, ApiKeyError>;
    /// Removes the stored key; no key is not an error.
    ///
    /// # Errors
    /// The store failing.
    fn remove(&self) -> Result<(), ApiKeyError>;
}

/// `key` trimmed, or why it cannot be stored: empty, longer than [`MAX_KEY_BYTES`], or with
/// spaces or control characters inside (a paste that took more than the key). The reason
/// never quotes the key.
///
/// # Errors
/// [`ApiKeyError::Invalid`].
pub fn checked_key(key: &str) -> Result<&str, ApiKeyError> {
    let key = key.trim();
    if key.is_empty() {
        return Err(ApiKeyError::Invalid("it is empty"));
    }
    if key.len() > MAX_KEY_BYTES {
        return Err(ApiKeyError::Invalid("it is longer than an API key can be"));
    }
    if key.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err(ApiKeyError::Invalid(
            "it holds spaces or line breaks; paste only the key",
        ));
    }
    Ok(key)
}

/// Zeroes and drops a string that held the key (the IPC argument), where the platform code
/// can wipe; elsewhere it is only dropped.
pub fn wipe_string(s: String) {
    #[cfg(windows)]
    crate::credentials_win::wipe(s.into_bytes());
    #[cfg(not(windows))]
    drop(s);
}

/// The key of a stored credential blob (UTF-8), `None` for an empty one.
///
/// # Errors
/// A blob that is not text.
fn key_of(blob: &[u8]) -> Result<Option<ApiKey>, ApiKeyError> {
    let text = std::str::from_utf8(blob).map_err(|_| {
        ApiKeyError::Store("the saved key is not text; remove it and save it again".into())
    })?;
    Ok(ApiKey::new(text))
}

/// Windows Credential Manager under one target name.
#[derive(Debug, Clone)]
pub struct CredentialManager {
    target: String,
}

impl CredentialManager {
    /// The Anthropic API key ([`ANTHROPIC_KEY_TARGET`]).
    #[must_use]
    pub fn anthropic() -> Self {
        Self::with_target(ANTHROPIC_KEY_TARGET)
    }

    /// Under `target` (tests use their own).
    #[must_use]
    pub fn with_target(target: &str) -> Self {
        Self {
            target: target.to_owned(),
        }
    }
}

#[cfg(windows)]
fn store_error(e: &windows::core::Error) -> ApiKeyError {
    ApiKeyError::Store(e.message())
}

impl ApiKeyStore for CredentialManager {
    #[cfg(windows)]
    fn anthropic_key(&self) -> Result<Option<ApiKey>, ApiKeyError> {
        let Some(blob) = crate::credentials_win::read(&self.target).map_err(|e| store_error(&e))?
        else {
            return Ok(None);
        };
        let key = key_of(&blob);
        crate::credentials_win::wipe(blob);
        key
    }

    #[cfg(not(windows))]
    fn anthropic_key(&self) -> Result<Option<ApiKey>, ApiKeyError> {
        Err(ApiKeyError::Unsupported)
    }
}

impl KeyVault for CredentialManager {
    #[cfg(windows)]
    fn save(&self, key: &str) -> Result<(), ApiKeyError> {
        let key = checked_key(key)?;
        crate::credentials_win::write(&self.target, KEY_USER, key.as_bytes())
            .map_err(|e| store_error(&e))
    }

    #[cfg(windows)]
    fn has(&self) -> Result<bool, ApiKeyError> {
        match crate::credentials_win::read(&self.target).map_err(|e| store_error(&e))? {
            Some(blob) => {
                let some = !blob.is_empty();
                crate::credentials_win::wipe(blob);
                Ok(some)
            }
            None => Ok(false),
        }
    }

    #[cfg(windows)]
    fn remove(&self) -> Result<(), ApiKeyError> {
        crate::credentials_win::delete(&self.target)
            .map(|_| ())
            .map_err(|e| store_error(&e))
    }

    #[cfg(not(windows))]
    fn save(&self, key: &str) -> Result<(), ApiKeyError> {
        checked_key(key)?;
        let _ = (&self.target, KEY_USER);
        Err(ApiKeyError::Unsupported)
    }

    #[cfg(not(windows))]
    fn has(&self) -> Result<bool, ApiKeyError> {
        Err(ApiKeyError::Unsupported)
    }

    #[cfg(not(windows))]
    fn remove(&self) -> Result<(), ApiKeyError> {
        Err(ApiKeyError::Unsupported)
    }
}

/// An in-memory [`KeyVault`] for tests (the key never reaches a real store).
#[cfg(test)]
#[derive(Debug, Default)]
pub struct MemoryVault {
    key: std::sync::Mutex<Option<String>>,
    /// Make every call fail with this store error.
    pub broken: Option<String>,
}

#[cfg(test)]
impl MemoryVault {
    /// A vault whose every call fails with the store error `e`.
    pub fn broken(e: &str) -> Self {
        Self {
            key: std::sync::Mutex::new(None),
            broken: Some(e.to_owned()),
        }
    }

    fn check(&self) -> Result<std::sync::MutexGuard<'_, Option<String>>, ApiKeyError> {
        match &self.broken {
            Some(e) => Err(ApiKeyError::Store(e.clone())),
            None => Ok(self.key.lock().unwrap()),
        }
    }
}

#[cfg(test)]
impl ApiKeyStore for MemoryVault {
    fn anthropic_key(&self) -> Result<Option<ApiKey>, ApiKeyError> {
        Ok(self.check()?.as_deref().and_then(ApiKey::new))
    }
}

#[cfg(test)]
impl KeyVault for MemoryVault {
    fn save(&self, key: &str) -> Result<(), ApiKeyError> {
        let key = checked_key(key)?.to_owned();
        *self.check()? = Some(key);
        Ok(())
    }
    fn has(&self) -> Result<bool, ApiKeyError> {
        Ok(self.check()?.is_some())
    }
    fn remove(&self) -> Result<(), ApiKeyError> {
        *self.check()? = None;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SECRET: &str = "sk-ant-api03-credential-test-0123456789";

    #[test]
    fn credentials_checked_key_trims_and_refuses_what_is_not_a_key() {
        assert_eq!(checked_key(&format!("  {SECRET}\n")).unwrap(), SECRET);
        for bad in [
            String::new(),
            "   ".into(),
            format!("{SECRET} {SECRET}"),
            format!("{SECRET}\n{SECRET}"),
            format!("{SECRET}\u{7}"),
            "k".repeat(MAX_KEY_BYTES + 1),
        ] {
            let e = checked_key(&bad).unwrap_err();
            assert!(matches!(e, ApiKeyError::Invalid(_)), "{e:?}");
            assert!(!e.to_string().contains(SECRET), "{e}");
        }
        assert!(checked_key(&"k".repeat(MAX_KEY_BYTES)).is_ok());
    }

    #[test]
    fn credentials_blob_reads_as_a_redacted_key() {
        let key = key_of(SECRET.as_bytes()).unwrap().unwrap();
        assert_eq!(Some(key.clone()), ApiKey::new(SECRET));
        assert!(!format!("{key:?}").contains(SECRET));
        assert_eq!(key_of(b"").unwrap(), None);
        let e = key_of(&[0xff, 0xfe]).unwrap_err();
        assert!(matches!(e, ApiKeyError::Store(_)), "{e:?}");
    }

    #[test]
    fn credentials_manager_debug_names_only_the_target() {
        let m = CredentialManager::anthropic();
        assert_eq!(
            format!("{m:?}"),
            format!("CredentialManager {{ target: {ANTHROPIC_KEY_TARGET:?} }}")
        );
    }

    /// Deletes the test credential when the test ends, passed or failed.
    #[cfg(windows)]
    struct Cleanup(CredentialManager);

    #[cfg(windows)]
    impl Drop for Cleanup {
        fn drop(&mut self) {
            let _ = self.0.remove();
        }
    }

    #[cfg(windows)]
    #[test]
    fn credentials_round_trip_through_windows_credential_manager() {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        let target = format!("Earshot/test-api-key-{}-{nanos}", std::process::id());
        let vault = CredentialManager::with_target(&target);
        let cleanup = Cleanup(vault.clone());
        assert!(!vault.has().unwrap(), "a fresh target holds nothing");
        assert_eq!(vault.anthropic_key().unwrap(), None);
        vault.remove().expect("removing nothing is not an error");
        vault.save(&format!(" {SECRET} ")).unwrap();
        assert!(vault.has().unwrap());
        assert_eq!(vault.anthropic_key().unwrap(), ApiKey::new(SECRET));
        // A second save replaces the first.
        vault.save("sk-ant-replaced").unwrap();
        assert_eq!(
            vault.anthropic_key().unwrap(),
            ApiKey::new("sk-ant-replaced")
        );
        // A refused key changes nothing.
        assert!(vault.save("two words").is_err());
        assert_eq!(
            vault.anthropic_key().unwrap(),
            ApiKey::new("sk-ant-replaced")
        );
        vault.remove().unwrap();
        assert!(!vault.has().unwrap());
        assert_eq!(vault.anthropic_key().unwrap(), None);
        drop(cleanup);
        assert!(!vault.has().unwrap());
    }
}
