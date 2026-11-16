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

    /// Proves both files can be written where they go, before a pairing code is spent on
    /// joining: each folder is created (`0700`) or tightened to `0700`, and a probe file is
    /// written and removed in it. A read-only folder (a Docker secret's `/run/secrets`) fails.
    ///
    /// # Errors
    /// [`VaultRefused::NotWritable`].
    pub fn probe_writable(&self) -> Result<(), VaultRefused> {
        for path in [&self.key, &self.token] {
            let mut probe = path.as_os_str().to_owned();
            probe.push(".probe");
            let probe = PathBuf::from(probe);
            let written = write_private(&probe, b"probe");
            let _ = fs::remove_file(&probe);
            written.map_err(|_| VaultRefused::NotWritable)?;
        }
        Ok(())
    }

    /// Refuses a key or token file that a group or other users can read (Unix; elsewhere the
    /// folder's own permissions decide). Missing files pass: the sync thread reports those.
    ///
    /// # Errors
    /// [`VaultRefused::ReadableByOthers`].
    pub fn check_private(&self) -> Result<(), VaultRefused> {
        for path in [&self.key, &self.token] {
            if readable_by_others(path) {
                return Err(VaultRefused::ReadableByOthers);
            }
        }
        Ok(())
    }

    /// The join marker: `joining` in the key file's folder, there from just before `join`
    /// spends the code until the device is set up ([`FileVault::begin_join`]).
    #[must_use]
    pub fn marker_path(&self) -> PathBuf {
        folder_of(&self.key).join(MARKER_NAME)
    }

    /// Marks a join as under way at `now_s` (Unix seconds): the marker ([`marker_text`]) is
    /// written, readable by this user only, before anything else of the join happens, so a run
    /// does not start on a vault whose device is not set up yet.
    ///
    /// # Errors
    /// [`VaultRefused::NotWritable`].
    pub fn begin_join(&self, now_s: u64) -> Result<(), VaultRefused> {
        write_private(&self.marker_path(), marker_text(now_s).as_bytes())
            .map_err(|_| VaultRefused::NotWritable)
    }

    /// Ends a join, whether it worked or not: the marker is removed (a missing one is fine).
    /// Only a crash leaves it, which [`FileVault::presence`] then reads as
    /// [`Presence::Interrupted`] once it is [`JOIN_STALE_S`] old; the next `join` overwrites it.
    ///
    /// # Errors
    /// [`VaultRefused::NotWritable`].
    pub fn end_join(&self) -> Result<(), VaultRefused> {
        match fs::remove_file(self.marker_path()) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(_) => Err(VaultRefused::NotWritable),
        }
    }

    /// Whether the worker has joined at `now_s` (Unix seconds).
    ///
    /// A join marker decides first: one at most [`JOIN_STALE_S`] away from now is
    /// [`Presence::Joining`] whatever the files hold (the join is still writing them or setting
    /// up the device); an older one, or one that does not hold a time, is
    /// [`Presence::Interrupted`]: still not joined, and never guessed at. Without a marker only
    /// neither file existing is [`Presence::Absent`] (the worker waits to be joined), and both
    /// being files is [`Presence::Present`]. Anything in between needs the person: one file
    /// without the other, a path that is not a file, or a path whose presence cannot be read.
    ///
    /// # Errors
    /// [`VaultRefused::Incomplete`]; [`VaultRefused::Unreadable`].
    pub fn presence(&self, now_s: u64) -> Result<Presence, VaultRefused> {
        match read(&self.marker_path()) {
            Ok(None) => {}
            Ok(Some(text)) => return Ok(marker_verdict(&text, now_s)),
            Err(_) => return Err(VaultRefused::Unreadable),
        }
        match (file_state(&self.key)?, file_state(&self.token)?) {
            (false, false) => Ok(Presence::Absent),
            (true, true) => Ok(Presence::Present),
            _ => Err(VaultRefused::Incomplete),
        }
    }
}

/// The join marker's file name.
const MARKER_NAME: &str = "joining";

/// The requests to the relay a join makes (`take_slot`, then `register`).
pub const JOIN_REQUESTS: u64 = 2;

/// Room for the join's store writes and the device's set-up after the last request (a
/// judgement).
pub const JOIN_MARGIN_S: u64 = 300;

/// How long a join may take before its marker counts as an interrupted join: longer than the
/// worst a join can take, every request following every redirect earshot-net allows, each hop
/// taking earshot-net's longest per-hop timeout ([`earshot_net::DEFAULT_TIMEOUT`]; the relay
/// client sets a shorter one), plus [`JOIN_MARGIN_S`]. A stale marker only changes what the
/// waiting run reports, so erring long costs nothing but a later `join_interrupted`.
pub const JOIN_STALE_S: u64 = JOIN_REQUESTS
    * (earshot_net::MAX_REDIRECTS as u64 + 1)
    * earshot_net::DEFAULT_TIMEOUT.as_secs()
    + JOIN_MARGIN_S;

/// The join marker's text for `now_s`.
#[must_use]
pub fn marker_text(now_s: u64) -> String {
    format!("joining {now_s}\n")
}

fn marker_verdict(text: &str, now_s: u64) -> Presence {
    let at = text
        .trim()
        .strip_prefix("joining ")
        .and_then(|t| t.parse::<u64>().ok());
    match at {
        Some(at) if at.abs_diff(now_s) <= JOIN_STALE_S => Presence::Joining,
        _ => Presence::Interrupted,
    }
}

/// The folder a vault file goes in (`.` for a bare file name).
fn folder_of(path: &Path) -> &Path {
    path.parent()
        .filter(|d| !d.as_os_str().is_empty())
        .unwrap_or(Path::new("."))
}

/// Whether the vault files are there ([`FileVault::presence`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Presence {
    /// Neither file exists: the worker has not joined.
    Absent,
    /// A join is under way: not joined yet.
    Joining,
    /// A join marker older than [`JOIN_STALE_S`] (or holding no time): a join stopped part way
    /// and the worker is not joined. Running `join` again overwrites the marker.
    Interrupted,
    /// Both are files.
    Present,
}

/// Why the vault files cannot be used. Fixed copy; never a path or a file's contents.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VaultRefused {
    /// A file's folder cannot be written (for `join`).
    NotWritable,
    /// A file can be read by a group or other users.
    ReadableByOthers,
    /// One file is there without the other.
    Incomplete,
    /// A path is not a file, or whether it exists cannot be read.
    Unreadable,
}

impl VaultRefused {
    /// The message printed.
    #[must_use]
    pub const fn copy(self) -> &'static str {
        match self {
            Self::NotWritable => {
                "the vault files' folder cannot be written; join writes them to the data volume \
                 (a Docker secret is read-only: copy them there after joining)"
            }
            Self::ReadableByOthers => {
                "a vault file can be read by other users; make it readable by the worker's user \
                 only (chmod 600, owner uid 10001)"
            }
            Self::Incomplete => {
                "only one of the two vault files is there; delete the worker's volume and join \
                 again with a new code"
            }
            Self::Unreadable => {
                "a vault file is not a file or cannot be read; make both files readable by the \
                 worker's user (owner uid 10001)"
            }
        }
    }
}

/// Whether `path` is a file (`true`) or does not exist (`false`); a path that is something else,
/// or whose metadata cannot be read for any reason but its absence, is
/// [`VaultRefused::Unreadable`]. Follows links, as a Docker secret may be one.
fn file_state(path: &Path) -> Result<bool, VaultRefused> {
    match fs::metadata(path) {
        Ok(m) if m.is_file() => Ok(true),
        Ok(_) => Err(VaultRefused::Unreadable),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(_) => Err(VaultRefused::Unreadable),
    }
}

#[cfg(unix)]
fn readable_by_others(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    fs::metadata(path).is_ok_and(|m| m.permissions().mode() & 0o077 != 0)
}

#[cfg(not(unix))]
fn readable_by_others(_path: &Path) -> bool {
    false
}

/// `bytes` written to `path` readable by this user only: a fresh temporary file beside it, then
/// a rename, so a reader never sees half a file.
fn write_private(path: &Path, bytes: &[u8]) -> io::Result<()> {
    create_private_dir(folder_of(path))?;
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
        // An existing folder is tightened: only the worker's user may list it.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(dir)?.permissions().mode();
            if mode & 0o077 != 0 {
                fs::set_permissions(dir, fs::Permissions::from_mode(mode & 0o700))?;
            }
        }
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

    const NOW: u64 = 1_800_000_000;

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

    /// A key file a group can read (0640) is refused; saving into an existing 0755 folder
    /// tightens it to 0700.
    #[cfg(unix)]
    #[test]
    fn vault_a_group_readable_key_is_refused_and_an_open_folder_is_tightened() {
        use std::os::unix::fs::PermissionsExt;
        let dir = Dir::new("tighten");
        let folder = dir.0.join("vault");
        fs::create_dir_all(&folder).unwrap();
        fs::set_permissions(&folder, fs::Permissions::from_mode(0o755)).unwrap();
        let vault = FileVault::new(folder.join("key"), folder.join("token"));
        vault
            .save(
                &VaultKey::from_bytes([2; 32]),
                &DeviceToken::new("t".into()),
            )
            .unwrap();
        let mode = fs::metadata(&folder).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700, "the folder is tightened by a save");
        assert_eq!(vault.check_private(), Ok(()));
        fs::set_permissions(vault.key_path(), fs::Permissions::from_mode(0o640)).unwrap();
        assert_eq!(vault.check_private(), Err(VaultRefused::ReadableByOthers));
    }

    #[test]
    fn vault_probe_leaves_nothing_behind_and_refuses_a_folder_that_is_a_file() {
        let dir = Dir::new("probe");
        let vault = FileVault::new(dir.0.join("vault/key"), dir.0.join("vault/token"));
        vault.probe_writable().unwrap();
        assert_eq!(fs::read_dir(dir.0.join("vault")).unwrap().count(), 0);
        assert!(!vault.has().unwrap());
        // A folder path that is a file cannot hold them, on every system.
        fs::write(dir.0.join("blocked"), b"").unwrap();
        let blocked = FileVault::new(dir.0.join("blocked/key"), dir.0.join("blocked/token"));
        assert_eq!(blocked.probe_writable(), Err(VaultRefused::NotWritable));
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

    /// Only both files missing is a worker waiting to join; one without the other, or a path
    /// that is a folder, is a vault the person must look at.
    #[test]
    fn vault_presence_waits_only_when_both_files_are_missing() {
        let dir = Dir::new("presence");
        let vault = FileVault::new(dir.0.join("vault/key"), dir.0.join("vault/token"));
        // Not even the folder exists yet.
        assert_eq!(vault.presence(NOW), Ok(Presence::Absent));
        fs::create_dir_all(dir.0.join("vault")).unwrap();
        assert_eq!(vault.presence(NOW), Ok(Presence::Absent));
        fs::write(vault.key_path(), "ab".repeat(32)).unwrap();
        assert_eq!(vault.presence(NOW), Err(VaultRefused::Incomplete));
        fs::remove_file(vault.key_path()).unwrap();
        fs::write(vault.token_path(), "a-token").unwrap();
        assert_eq!(vault.presence(NOW), Err(VaultRefused::Incomplete));
        fs::write(vault.key_path(), "ab".repeat(32)).unwrap();
        assert_eq!(vault.presence(NOW), Ok(Presence::Present));
        // A folder where a file should be.
        fs::remove_file(vault.token_path()).unwrap();
        fs::create_dir(vault.token_path()).unwrap();
        assert_eq!(vault.presence(NOW), Err(VaultRefused::Unreadable));
        fs::remove_file(vault.key_path()).unwrap();
        assert_eq!(vault.presence(NOW), Err(VaultRefused::Unreadable));
        // A path under a file is not missing on Linux (ENOTDIR); Windows reports it as a
        // missing path, which is what it is there.
        #[cfg(unix)]
        {
            fs::write(dir.0.join("blocked"), b"").unwrap();
            let blocked = FileVault::new(dir.0.join("blocked/key"), dir.0.join("blocked/token"));
            assert_eq!(blocked.presence(NOW), Err(VaultRefused::Unreadable));
        }
        // Fixed copy, never a path.
        for r in [VaultRefused::Incomplete, VaultRefused::Unreadable] {
            assert!(!r.copy().contains(&*dir.0.to_string_lossy()));
        }
    }

    /// A fresh join marker is a join under way whatever the files hold; a stale or unreadable
    /// one is an interrupted join, never a guess; a join that ends leaves no marker.
    #[test]
    fn vault_a_join_marker_holds_the_run_until_the_join_ends() {
        let dir = Dir::new("marker");
        let vault = FileVault::new(dir.0.join("vault/key"), dir.0.join("vault/token"));
        assert_eq!(vault.marker_path(), dir.0.join("vault/joining"));
        vault.begin_join(NOW).unwrap();
        assert_eq!(
            fs::read_to_string(vault.marker_path()).unwrap(),
            marker_text(NOW)
        );
        // No files yet, the key alone, then both: still joining.
        assert_eq!(vault.presence(NOW), Ok(Presence::Joining));
        fs::write(vault.key_path(), "ab".repeat(32)).unwrap();
        assert_eq!(vault.presence(NOW + 5), Ok(Presence::Joining));
        fs::write(vault.token_path(), "a-token").unwrap();
        assert_eq!(vault.presence(NOW + JOIN_STALE_S), Ok(Presence::Joining));
        // Left behind by a crash: interrupted after JOIN_STALE_S either way round, still not
        // joined whatever the files hold.
        assert_eq!(
            vault.presence(NOW + JOIN_STALE_S + 1),
            Ok(Presence::Interrupted)
        );
        assert_eq!(
            vault.presence(NOW - JOIN_STALE_S - 1),
            Ok(Presence::Interrupted)
        );
        fs::write(vault.marker_path(), "joining soon").unwrap();
        assert_eq!(vault.presence(NOW), Ok(Presence::Interrupted));
        // A new join overwrites the stale marker: joining again.
        vault.begin_join(NOW).unwrap();
        assert_eq!(vault.presence(NOW), Ok(Presence::Joining));
        // The join ends: the files decide again, and ending twice is fine.
        vault.end_join().unwrap();
        assert!(!vault.marker_path().exists());
        assert_eq!(vault.presence(NOW), Ok(Presence::Present));
        vault.end_join().unwrap();
        // A marker that is a folder cannot be read.
        fs::create_dir(vault.marker_path()).unwrap();
        assert_eq!(vault.presence(NOW), Err(VaultRefused::Unreadable));
    }

    /// The stale limit is longer than the worst a join can take: every request following every
    /// redirect, each hop at earshot-net's longest per-hop timeout, plus the margin.
    #[test]
    fn vault_a_join_marker_goes_stale_only_after_the_longest_join() {
        let hops = JOIN_REQUESTS * (earshot_net::MAX_REDIRECTS as u64 + 1);
        let longest_join = hops * earshot_net::DEFAULT_TIMEOUT.as_secs();
        assert_eq!(JOIN_STALE_S, longest_join + JOIN_MARGIN_S);
        assert!(JOIN_STALE_S > longest_join);
        const { assert!(JOIN_MARGIN_S > 0) };
    }
}
