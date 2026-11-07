//! The per-device data directory (PLAN.md 3.6).
//!
//! ```text
//! <root>\                       %LOCALAPPDATA%\Earshot on Windows, app-private dir on Android
//!   earshot.db                  SQLite, WAL
//!   audio\<meeting-id>\mic.flac       16 kHz mono, post-AEC
//!   audio\<meeting-id>\loopback.flac  16 kHz mono (Windows only)
//!   models\  logs\
//! ```

use std::path::{Path, PathBuf};

use earshot_core::capture::TrackId;
use earshot_core::identity::EntityId;

use crate::db::{Store, StoreError};
use crate::flac::FlacTrackWriter;

/// The data directory of one install.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DataDir {
    root: PathBuf,
}

impl DataDir {
    /// A data directory at `root` (nothing is created).
    pub fn new(root: impl Into<PathBuf>) -> Self {
        DataDir { root: root.into() }
    }

    /// `%LOCALAPPDATA%\Earshot`, when `LOCALAPPDATA` is set (Windows).
    #[must_use]
    pub fn from_local_app_data() -> Option<Self> {
        std::env::var_os("LOCALAPPDATA").map(|d| DataDir::new(PathBuf::from(d).join("Earshot")))
    }

    /// The root.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// `earshot.db`.
    #[must_use]
    pub fn db_path(&self) -> PathBuf {
        self.root.join("earshot.db")
    }

    /// `models/`.
    #[must_use]
    pub fn models_dir(&self) -> PathBuf {
        self.root.join("models")
    }

    /// `logs/` (local only, no telemetry).
    #[must_use]
    pub fn logs_dir(&self) -> PathBuf {
        self.root.join("logs")
    }

    /// Creates the root and its `audio`, `models` and `logs` directories.
    ///
    /// # Errors
    /// I/O errors.
    pub fn create(&self) -> std::io::Result<()> {
        for d in [self.root.join("audio"), self.models_dir(), self.logs_dir()] {
            std::fs::create_dir_all(d)?;
        }
        Ok(())
    }

    /// Opens (creating) the database.
    ///
    /// # Errors
    /// See [`Store::open`].
    pub fn open_store(&self) -> Result<Store, StoreError> {
        Store::open(&self.db_path())
    }

    /// The data-dir-relative path of a track's audio file (`audio/<meeting>/mic.flac` or
    /// `loopback.flac`), as stored in `AudioFile::path`.
    #[must_use]
    pub fn audio_rel(meeting: EntityId, kind: TrackId) -> String {
        let name = match kind {
            TrackId::Mic => "mic.flac",
            TrackId::Reference => "loopback.flac",
        };
        format!("audio/{meeting}/{name}")
    }

    /// Joins a stored relative path to the root. Rejects anything that could leave the data
    /// directory (absolute, drive prefix, `.`/`..` or empty components).
    ///
    /// # Errors
    /// [`StoreError::Corrupt`] for such a path.
    pub fn resolve(&self, rel: &str) -> Result<PathBuf, StoreError> {
        let ok = !rel.is_empty()
            && !rel.starts_with(['/', '\\'])
            && !rel.contains(':')
            && rel
                .split(['/', '\\'])
                .all(|c| !c.is_empty() && c != "." && c != "..");
        if !ok {
            return Err(StoreError::Corrupt(format!(
                "path {rel:?} is not inside the data directory"
            )));
        }
        let mut p = self.root.clone();
        p.extend(rel.split(['/', '\\']));
        Ok(p)
    }

    /// Startup cleanup: removes `audio/*/*.flac.part` files (recordings that crashed before
    /// they were finalised) last modified at least `grace_ms` before `now_unix_ms`. Use
    /// [`crate::ORPHAN_GRACE_MS`]; younger `.part` files stay recoverable. Returns how many
    /// were removed.
    ///
    /// # Errors
    /// I/O errors other than "not found".
    pub fn cleanup_stale_parts(&self, now_unix_ms: u64, grace_ms: u64) -> std::io::Result<usize> {
        let Ok(dirs) = std::fs::read_dir(self.root.join("audio")) else {
            return Ok(0);
        };
        let mut removed = 0;
        for d in dirs.filter_map(Result::ok) {
            let Ok(files) = std::fs::read_dir(d.path()) else {
                continue;
            };
            for f in files.filter_map(Result::ok) {
                let p = f.path();
                if !f.file_name().to_string_lossy().ends_with(".flac.part")
                    || !f.file_type().is_ok_and(|t| t.is_file())
                {
                    continue;
                }
                if crate::audio::mtime_ms(&p)
                    .is_some_and(|m| now_unix_ms.saturating_sub(m) >= grace_ms)
                {
                    match std::fs::remove_file(&p) {
                        Ok(()) => removed += 1,
                        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                        Err(e) => return Err(e),
                    }
                }
            }
        }
        Ok(removed)
    }

    /// A FLAC writer for a track of `meeting`, at its layout path.
    ///
    /// # Errors
    /// See [`FlacTrackWriter::create`].
    pub fn track_writer(
        &self,
        meeting: EntityId,
        kind: TrackId,
        sample_rate: u32,
    ) -> Result<FlacTrackWriter, StoreError> {
        FlacTrackWriter::create(&self.resolve(&Self::audio_rel(meeting, kind))?, sample_rate)
    }
}
