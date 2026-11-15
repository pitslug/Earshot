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
