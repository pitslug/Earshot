//! The worker's vault key and device token in files (T11.6, docs/sync.md section 10).
//!
//! `join` writes them under the data volume (`<data>/vault/key` and `<data>/vault/token` by
//! default), each through a temporary file renamed into place, the folder `0700` and the files
//! `0600` on Unix, so only the container's user can read them. The run reads them from the same
//! paths or from paths the configuration names, so a Docker secret
//! (`/run/secrets/<name>`, read-only) can hold them instead. They are never an environment
//! variable, a command-line argument or part of an image, and never logged: `Debug` shows the
//! paths only.

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use earshot_runtime::sync::{DeviceToken, SyncSecrets, SyncVault, VaultError, VaultKey};

/// A [`SyncVault`] over two files.
#[derive(Debug, Clone)]
pub struct FileVault {
    key: PathBuf,
    token: PathBuf,
}

impl FileVault {
    /// The vault at these two paths.
    #[must_use]
    pub fn new(key: impl Into<PathBuf>, token: impl Into<PathBuf>) -> Self {
        Self {
            key: key.into(),
            token: token.into(),
        }
    }

    /// The key file's path.
    #[must_use]
    pub fn key_path(&self) -> &Path {
        &self.key
    }

    /// The token file's path.
    #[must_use]
    pub fn token_path(&self) -> &Path {
        &self.token
    }
}

/// `bytes` written to `path` readable by this user only: a fresh temporary file beside it, then
/// a rename, so a reader never sees half a file.
fn write_private(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let dir = path
        .parent()
        .filter(|d| !d.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    create_private_dir(dir)?;
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    let _ = fs::remove_file(&tmp);
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let written = (|| {
        let mut file = options.open(&tmp)?;
        file.write_all(bytes)?;
        file.sync_all()
    })();
    match written.and_then(|()| fs::rename(&tmp, path)) {
        Ok(()) => Ok(()),
        Err(e) => {
            let _ = fs::remove_file(&tmp);
            Err(e)
        }
    }
}

fn create_private_dir(dir: &Path) -> io::Result<()> {
    if dir.is_dir() {
        return Ok(());
    }
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(dir)
}

/// `path`'s text, `None` when it does not exist.
fn read(path: &Path) -> Result<Option<String>, VaultError> {
    match fs::read_to_string(path) {
        Ok(text) => Ok(Some(text)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(_) => Err(VaultError::Store),
    }
}

/// The key as 64 lower-case hex digits.
fn hex_of(key: &VaultKey) -> String {
    let mut out = String::with_capacity(65);
    for b in key.as_bytes() {
        out.push_str(&format!("{b:02x}"));
    }
    out.push('\n');
    out
}

/// 64 hex digits (surrounding white space ignored) as a key; `None` for anything else.
fn key_of(text: &str) -> Option<VaultKey> {
    let hex = text.trim().as_bytes();
    if hex.len() != 64 {
        return None;
    }
    let digit = |c: u8| (c as char).to_digit(16);
    let mut bytes = [0u8; 32];
    let (pairs, _) = hex.as_chunks::<2>();
    for (i, [h, l]) in pairs.iter().enumerate() {
        let (hi, lo) = (digit(*h)?, digit(*l)?);
        bytes[i] = u8::try_from(hi * 16 + lo).ok()?;
    }
    Some(VaultKey::from_bytes(bytes))
}

impl SyncVault for FileVault {
    fn save(&self, key: &VaultKey, token: &DeviceToken) -> Result<(), VaultError> {
        let mut hex = hex_of(key);
        let mut token_text = format!("{}\n", token.as_str());
        let written = write_private(&self.key, hex.as_bytes())
            .and_then(|()| write_private(&self.token, token_text.as_bytes()));
        earshot_net::wipe_string(&mut hex);
        earshot_net::wipe_string(&mut token_text);
        written.map_err(|_| VaultError::Store)
    }

    fn has(&self) -> Result<bool, VaultError> {
        Ok(self.key.is_file() && self.token.is_file())
    }

    fn remove(&self) -> Result<(), VaultError> {
        for path in [&self.key, &self.token] {
            match fs::remove_file(path) {
                Ok(()) => {}
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(_) => return Err(VaultError::Store),
            }
        }
        Ok(())
    }

    /// A file that is missing, or does not hold a key or a token, is `None` (the sync thread
    /// then stops with the `vault` problem, for the person to join again); a file that cannot be
    /// read is a [`VaultError::Store`] (retried).
    fn load(&self) -> Result<Option<SyncSecrets>, VaultError> {
        let (Some(mut key_text), Some(mut token_text)) = (read(&self.key)?, read(&self.token)?)
        else {
            return Ok(None);
        };
        let key = key_of(&key_text);
        let token = token_text.trim().to_owned();
        earshot_net::wipe_string(&mut key_text);
        earshot_net::wipe_string(&mut token_text);
        Ok(match key {
            Some(key) if !token.is_empty() => Some(SyncSecrets {
                key,
                token: DeviceToken::new(token),
            }),
            _ => {
                let mut token = token;
                earshot_net::wipe_string(&mut token);
                None
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Dir(PathBuf);
    impl Dir {
        fn new(tag: &str) -> Self {
            let dir = std::env::temp_dir()
                .join(format!("earshot-worker-vault-{tag}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&dir);
            Self(dir)
        }
    }
    impl Drop for Dir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn vault_files_round_trip_and_hold_only_the_key_and_token() {
        let dir = Dir::new("round");
        let vault = FileVault::new(dir.0.join("vault/key"), dir.0.join("vault/token"));
        assert!(!vault.has().unwrap());
        assert!(vault.load().unwrap().is_none());
        let key = VaultKey::from_bytes([0xab; 32]);
        vault
            .save(&key, &DeviceToken::new("an-invented-token".into()))
            .unwrap();
        assert!(vault.has().unwrap());
        let loaded = vault.load().unwrap().unwrap();
        assert_eq!(loaded.key.as_bytes(), &[0xab; 32]);
        assert_eq!(loaded.token.as_str(), "an-invented-token");
        assert_eq!(
            fs::read_to_string(vault.key_path()).unwrap(),
            format!("{}\n", "ab".repeat(32))
        );
        assert_eq!(
            fs::read_to_string(vault.token_path()).unwrap(),
            "an-invented-token\n"
        );
        // No temporary file is left beside them.
        let names: Vec<_> = fs::read_dir(dir.0.join("vault"))
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        assert_eq!(names.len(), 2, "{names:?}");
        // Debug names the paths, never what they hold.
        let shown = format!("{vault:?} {loaded:?}");
        assert!(
            !shown.contains("an-invented-token") && !shown.contains("abab"),
            "{shown}"
        );
        vault.remove().unwrap();
        assert!(!vault.has().unwrap());
        vault.remove().unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn vault_files_are_readable_by_their_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = Dir::new("mode");
        let vault = FileVault::new(dir.0.join("vault/key"), dir.0.join("vault/token"));
        vault
            .save(
                &VaultKey::from_bytes([1; 32]),
                &DeviceToken::new("t".into()),
            )
            .unwrap();
        for path in [vault.key_path(), vault.token_path()] {
            let mode = fs::metadata(path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "{}", path.display());
        }
        let mode = fs::metadata(dir.0.join("vault"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o700);
    }

    #[test]
    fn vault_a_damaged_or_half_present_vault_reads_as_none() {
        let dir = Dir::new("damaged");
        fs::create_dir_all(&dir.0).unwrap();
        let vault = FileVault::new(dir.0.join("key"), dir.0.join("token"));
        fs::write(vault.key_path(), "not hex").unwrap();
        fs::write(vault.token_path(), "a-token").unwrap();
        assert!(vault.load().unwrap().is_none());
        fs::write(vault.key_path(), "cd".repeat(32)).unwrap();
        fs::write(vault.token_path(), "  \n").unwrap();
        assert!(vault.load().unwrap().is_none(), "an empty token");
        fs::write(vault.token_path(), "a-token").unwrap();
        // A Docker secret file may end without a newline or with white space.
        let loaded = vault.load().unwrap().unwrap();
        assert_eq!(loaded.key.as_bytes(), &[0xcd; 32]);
        fs::remove_file(vault.token_path()).unwrap();
        assert!(vault.load().unwrap().is_none());
        assert!(!vault.has().unwrap());
        // Wrong length, odd digits.
        assert!(key_of(&"a".repeat(63)).is_none());
        assert!(key_of(&format!("{}zz", "a".repeat(62))).is_none());
    }
}
