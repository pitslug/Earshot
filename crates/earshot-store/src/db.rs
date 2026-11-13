//! The SQLite connection: opening, pragmas, migrations, transactions.

use std::fmt;
use std::path::Path;

use earshot_core::asr::Segment;
use earshot_core::identity::{Clock, DeviceId, EntityId, Hlc, HlcTimestamp};
use earshot_core::model::{RepoError, TranscriptPass};
use rand_core::Rng;
use rusqlite::{Connection, OptionalExtension, Transaction};

use crate::schema;

/// Busy timeout of the connection (waits for other writers).
const BUSY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// A WAL checkpoint after forgetting voiceprints is tried this many times ...
const CHECKPOINT_ATTEMPTS: u32 = 5;
/// ... each waiting at most this long for readers to finish.
const CHECKPOINT_WAIT: std::time::Duration = std::time::Duration::from_millis(100);

/// Schema version this build writes (the number of migrations).
pub const SCHEMA_VERSION: u32 = 4;

/// Store errors.
#[derive(Debug)]
pub enum StoreError {
    /// SQLite failure.
    Sqlite(rusqlite::Error),
    /// File system failure.
    Io(std::io::Error),
    /// The database was written by a newer Earshot; it is not touched.
    SchemaTooNew {
        /// `user_version` found.
        found: u32,
        /// Highest version this build knows.
        supported: u32,
    },
    /// A stored value is out of range or does not parse.
    Corrupt(String),
    /// A repository rule was violated (missing parent).
    Repo(RepoError),
    /// FLAC encoding or decoding failed.
    Audio(String),
    /// The vault path is unusable.
    Vault(crate::export::VaultError),
    /// The existing note cannot be merged (ambiguous region markers).
    Export(earshot_core::export::ExportError),
    /// SQLite could not finish an operation because another connection holds the
    /// database (for example a WAL checkpoint with an active reader). Retry.
    Busy(String),
    /// A note carrying the meeting's `earshot_id` exists at a path that cannot be stored as
    /// a vault path (for example a file name that is not valid Unicode). Nothing is written,
    /// so the note is not duplicated; rename it in the vault and export again.
    UnusableNotePath(std::path::PathBuf),
    /// The note was changed by someone else (Obsidian, a sync client) while it was being
    /// merged; nothing was written. Retry the export.
    ConcurrentEdit(std::path::PathBuf),
}

impl StoreError {
    /// Whether retrying the same operation can succeed without user action.
    #[must_use]
    pub fn is_retryable(&self) -> bool {
        matches!(self, StoreError::ConcurrentEdit(_) | StoreError::Busy(_))
    }

    /// Whether this is a file system error that says the disk (or the user's quota on it) is
    /// full, so a message can say so in plain words (T9.17).
    #[must_use]
    pub fn is_disk_full(&self) -> bool {
        // Windows: ERROR_HANDLE_DISK_FULL (39), ERROR_DISK_FULL (112), ERROR_DISK_QUOTA_EXCEEDED
        // (1295), in case std ever stops mapping them to `StorageFull` / `QuotaExceeded`.
        const FULL_CODES: [i32; 3] = [39, 112, 1295];
        match self {
            StoreError::Io(e) => {
                matches!(
                    e.kind(),
                    std::io::ErrorKind::StorageFull | std::io::ErrorKind::QuotaExceeded
                ) || (cfg!(windows) && e.raw_os_error().is_some_and(|c| FULL_CODES.contains(&c)))
            }
            _ => false,
        }
    }
}

impl fmt::Display for StoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StoreError::Sqlite(e) => write!(f, "sqlite: {e}"),
            StoreError::Io(e) => write!(f, "io: {e}"),
            StoreError::SchemaTooNew { found, supported } => write!(
                f,
                "database schema {found} is newer than this build supports ({supported})"
            ),
            StoreError::Corrupt(m) => write!(f, "corrupt store: {m}"),
            StoreError::Repo(e) => write!(f, "{e}"),
            StoreError::Audio(m) => write!(f, "audio: {m}"),
            StoreError::Vault(e) => write!(f, "{e}"),
            StoreError::Export(e) => write!(f, "export: {e}"),
            StoreError::Busy(m) => write!(f, "database busy: {m}"),
            StoreError::UnusableNotePath(p) => write!(
                f,
                "the meeting's note at {} has a name Earshot cannot record; rename it",
                p.display()
            ),
            StoreError::ConcurrentEdit(p) => write!(
                f,
                "{} changed while it was being exported; try again",
                p.display()
            ),
        }
    }
}

impl std::error::Error for StoreError {}

impl From<rusqlite::Error> for StoreError {
    fn from(e: rusqlite::Error) -> Self {
        StoreError::Sqlite(e)
    }
}

impl From<std::io::Error> for StoreError {
    fn from(e: std::io::Error) -> Self {
        StoreError::Io(e)
    }
}

impl From<RepoError> for StoreError {
    fn from(e: RepoError) -> Self {
        StoreError::Repo(e)
    }
}

/// The Earshot database (`earshot.db`, PLAN.md 3.6). Implements the core repository traits
/// ([`earshot_core::model::MeetingRepo`], [`earshot_core::model::FolderRepo`],
/// [`earshot_core::model::AudioFileRepo`],
/// [`earshot_core::model::ExportRepo`], [`earshot_core::voiceprint::VoiceprintRepo`]).
#[derive(Debug)]
pub struct Store {
    pub(crate) conn: Connection,
}

/// A transaction on the [`Store`]; implements the same repository traits. Commits when
/// the closure given to [`Store::transaction`] returns `Ok`.
#[derive(Debug)]
pub struct StoreTx<'a> {
    pub(crate) conn: Transaction<'a>,
}

/// A database opened read-only, for evaluation tools that read a copy of a user's store
/// (T12.25a, `sum_eval --source store`): SQLite's read-only open (`SQLITE_OPEN_READ_ONLY`,
/// what a `mode=ro` URI asks for) plus `PRAGMA query_only`, no migration, no checkpoint and
/// no pragma that writes. It implements [`MeetingRepo`](earshot_core::model::MeetingRepo)
/// for reading; every `put_*` is refused. Never use it on a store a running Earshot owns:
/// open a copy (in WAL mode SQLite may create the copy's `-shm` file beside it).
#[derive(Debug)]
pub struct ReadOnlyStore {
    pub(crate) conn: Connection,
}

impl ReadOnlyStore {
    /// Opens the database at `path` read-only. It must exist and hold exactly this build's
    /// schema ([`SCHEMA_VERSION`]): an older one would need migrations, which write.
    ///
    /// # Errors
    /// [`StoreError::SchemaTooNew`] for a newer schema, [`StoreError::Corrupt`] for an older
    /// one; SQLite errors (a missing file included).
    pub fn open(path: &Path) -> Result<ReadOnlyStore, StoreError> {
        let conn = Connection::open_with_flags(
            path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        conn.busy_timeout(BUSY_TIMEOUT)?;
        conn.pragma_update(None, "query_only", true)?;
        let found: u32 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        if found > SCHEMA_VERSION {
            return Err(StoreError::SchemaTooNew {
                found,
                supported: SCHEMA_VERSION,
            });
        }
        if found < SCHEMA_VERSION {
            return Err(StoreError::Corrupt(format!(
                "database schema {found} is older than this build reads ({SCHEMA_VERSION}); \
                 a read-only open cannot migrate it"
            )));
        }
        Ok(ReadOnlyStore { conn })
    }
}

impl Store {
    /// Opens (creating if needed) the database at `path`: WAL journal, foreign keys on,
    /// migrations applied.
    ///
    /// # Errors
    /// [`StoreError::SchemaTooNew`] for a database from a newer build; SQLite errors.
    pub fn open(path: &Path) -> Result<Store, StoreError> {
        Self::init(Connection::open(path)?, true)
    }

    /// A private in-memory database (tests).
    ///
    /// # Errors
    /// SQLite errors.
    pub fn open_in_memory() -> Result<Store, StoreError> {
        Self::init(Connection::open_in_memory()?, false)
    }

    fn init(conn: Connection, wal: bool) -> Result<Store, StoreError> {
        conn.busy_timeout(BUSY_TIMEOUT)?;
        conn.pragma_update(None, "foreign_keys", true)?;
        // Deleted and overwritten content is zeroed, so forgotten biometric data does not
        // linger in free pages (PLAN.md 3.9 #4).
        conn.pragma_update(None, "secure_delete", true)?;
        if wal {
            let mode: String =
                conn.pragma_update_and_check(None, "journal_mode", "WAL", |r| r.get(0))?;
            if !mode.eq_ignore_ascii_case("wal") {
                return Err(StoreError::Corrupt(format!(
                    "journal_mode WAL not accepted (got {mode})"
                )));
            }
            // In WAL mode NORMAL is durable against application crashes; a power loss
            // may lose the last commits but never corrupts the database.
            conn.pragma_update(None, "synchronous", "NORMAL")?;
        }
        let mut store = Store { conn };
        store.migrate()?;
        // Fold any WAL left by an earlier session (for example one whose checkpoint after
        // forgetting voiceprints was blocked) into the database. Best effort: another
        // connection may legitimately hold it.
        let _ = store.checkpoint_with_retry(1);
        Ok(store)
    }

    fn migrate(&mut self) -> Result<(), StoreError> {
        let migrations = schema::migrations();
        debug_assert_eq!(migrations.len(), SCHEMA_VERSION as usize);
        let too_new = |found| StoreError::SchemaTooNew {
            found,
            supported: SCHEMA_VERSION,
        };
        // Up to date: no write lock taken.
        let found = self.schema_version()?;
        if found > SCHEMA_VERSION {
            return Err(too_new(found));
        }
        if found == SCHEMA_VERSION {
            return Ok(());
        }
        // Each step takes the write lock first (IMMEDIATE) and re-reads the version under
        // it, so another connection opening the same file at the same time either finished
        // the step already (skipped here) or waits for this one; never both apply it.
        loop {
            let tx = self
                .conn
                .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
            let at: u32 = tx.query_row("PRAGMA user_version", [], |r| r.get(0))?;
            if at > SCHEMA_VERSION {
                return Err(too_new(at));
            }
            let Some(sql) = migrations.get(at as usize) else {
                return Ok(());
            };
            tx.execute_batch(sql)?;
            tx.pragma_update(None, "user_version", at + 1)?;
            tx.commit()?;
        }
    }

    /// The database's `user_version`.
    ///
    /// # Errors
    /// SQLite errors.
    pub fn schema_version(&self) -> Result<u32, StoreError> {
        Ok(self
            .conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))?)
    }

    /// The journal mode (`wal` for an on-disk store).
    ///
    /// # Errors
    /// SQLite errors.
    pub fn journal_mode(&self) -> Result<String, StoreError> {
        Ok(self
            .conn
            .query_row("PRAGMA journal_mode", [], |r| r.get(0))?)
    }

    /// Whether foreign keys are enforced.
    ///
    /// # Errors
    /// SQLite errors.
    pub fn foreign_keys(&self) -> Result<bool, StoreError> {
        Ok(self
            .conn
            .query_row("PRAGMA foreign_keys", [], |r| r.get(0))?)
    }

    /// Runs `f` in a transaction: committed if it returns `Ok`, rolled back otherwise.
    ///
    /// # Errors
    /// `f`'s error, or a SQLite error converted into `E`.
    pub fn transaction<T, E: From<StoreError>>(
        &mut self,
        f: impl FnOnce(&mut StoreTx<'_>) -> Result<T, E>,
    ) -> Result<T, E> {
        let tx = self
            .conn
            .transaction()
            .map_err(|e| E::from(StoreError::from(e)))?;
        let mut tx = StoreTx { conn: tx };
        let out = f(&mut tx)?;
        tx.conn.commit().map_err(|e| E::from(StoreError::from(e)))?;
        Ok(out)
    }

    /// This install's device id: created from `clock` and `rng` on first use, then stable.
    ///
    /// # Errors
    /// SQLite errors, or a stored id that is not 16 bytes.
    pub fn device_id<C: Clock + ?Sized, R: Rng + ?Sized>(
        &mut self,
        clock: &C,
        rng: &mut R,
    ) -> Result<DeviceId, StoreError> {
        // Insert-if-absent and read back in one transaction: two first starts racing on
        // the same database agree on one id.
        let candidate = DeviceId::generate(clock, rng);
        let tx = self.conn.transaction()?;
        tx.execute(
            "INSERT OR IGNORE INTO local_state (key, value) VALUES ('device_id', ?1)",
            [candidate.as_uuid().as_bytes().as_slice()],
        )?;
        let stored: Vec<u8> = tx.query_row(
            "SELECT value FROM local_state WHERE key = 'device_id'",
            [],
            |r| r.get(0),
        )?;
        tx.commit()?;
        let bytes: [u8; 16] = stored
            .try_into()
            .map_err(|_| StoreError::Corrupt("device_id is not 16 bytes".into()))?;
        Ok(DeviceId::from_bytes(bytes))
    }

    /// A device-local value (never synced): `local_state[key]`.
    ///
    /// # Errors
    /// SQLite errors.
    pub fn local_value(&self, key: &str) -> Result<Option<Vec<u8>>, StoreError> {
        get_local(&self.conn, key)
    }

    /// Whether the store holds any meeting row, a deleted meeting's tombstone included. Reads one
    /// row at most.
    ///
    /// # Errors
    /// Store errors.
    pub fn has_meeting_row(&self) -> Result<bool, RepoError> {
        crate::repo::any_meeting(&self.conn)
    }

    /// Sets a device-local value (never synced). `device_id` is reserved
    /// ([`Store::device_id`] owns it).
    ///
    /// # Errors
    /// [`StoreError::Corrupt`] for the reserved key; SQLite errors.
    pub fn set_local_value(&mut self, key: &str, value: &[u8]) -> Result<(), StoreError> {
        set_local(&self.conn, key, value)
    }

    /// Deletes a device-local value; returns whether it existed. With `secure_delete` its
    /// bytes are zeroed in the database pages; they leave the WAL at the next
    /// [`Store::flush_erasures`] (or the closing checkpoint). `device_id` is reserved.
    ///
    /// # Errors
    /// [`StoreError::Corrupt`] for the reserved key; SQLite errors.
    pub fn delete_local_value(&mut self, key: &str) -> Result<bool, StoreError> {
        delete_local(&self.conn, key)
    }

    /// A truncating WAL checkpoint with brief retries (as [`Store::forget_voiceprints`]
    /// runs), so data deleted with `secure_delete` (biometric or transcript records) is
    /// gone from the WAL as well as the database pages.
    ///
    /// # Errors
    /// [`StoreError::Busy`] (retryable) if another connection blocks the checkpoint (the
    /// deletion is committed; the closing checkpoint retries); SQLite errors.
    pub fn flush_erasures(&self) -> Result<(), StoreError> {
        self.checkpoint_with_retry(CHECKPOINT_ATTEMPTS)
    }

    /// Whether `secure_delete` is on (it is for every store this crate opens).
    ///
    /// # Errors
    /// SQLite errors.
    pub fn secure_delete(&self) -> Result<bool, StoreError> {
        Ok(self
            .conn
            .query_row("PRAGMA secure_delete", [], |r| r.get(0))?)
    }

    /// This device's hybrid logical clock, resumed after the greatest `updated_at` stored in
    /// any syncable table (any node), so timestamps never go backwards across restarts even
    /// when the wall clock was stepped back. The stored rows are the high-water mark.
    ///
    /// # Errors
    /// SQLite errors.
    pub fn hlc(&self, node: DeviceId) -> Result<Hlc, StoreError> {
        let mut best: Option<HlcTimestamp> = None;
        for table in crate::schema::SYNCED_TABLES {
            let sql = format!(
                "SELECT updated_wall, updated_logical, updated_node FROM {table} \
                 ORDER BY updated_wall DESC, updated_logical DESC, updated_node DESC LIMIT 1"
            );
            let row: Option<(i64, u32, [u8; 16])> = self
                .conn
                .query_row(&sql, [], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
                .optional()?;
            if let Some((wall, logical, n)) = row {
                let wall_ms = u64::try_from(wall)
                    .map_err(|_| StoreError::Corrupt(format!("negative timestamp in {table}")))?;
                let t = HlcTimestamp {
                    wall_ms,
                    logical,
                    node: DeviceId::from_bytes(n),
                };
                best = best.max(Some(t));
            }
        }
        Ok(best.map_or_else(|| Hlc::new(node), |hw| Hlc::resume(node, hw)))
    }

    /// [`earshot_core::voiceprint::forget_voiceprints`] in one transaction, followed by a
    /// truncating WAL checkpoint, so with `secure_delete` the forgotten vectors are gone from
    /// the database file, its free pages and the WAL (PLAN.md 3.9 #4). Returns how many
    /// voiceprints were deleted.
    ///
    /// In the same transaction it drops that identity's speaker centroids from every
    /// meeting's speaker identity record ([`crate::identities`], T6.25): they are the same
    /// biometric data in another place.
    ///
    /// # Errors
    /// [`StoreError::Busy`] (retryable) if another connection blocks the checkpoint (the
    /// deletion itself is committed); SQLite errors.
    pub fn forget_voiceprints<C: Clock + ?Sized>(
        &mut self,
        person: EntityId,
        hlc: &mut Hlc,
        clock: &C,
    ) -> Result<usize, StoreError> {
        self.forget_voiceprints_of(&[person], hlc, clock)
    }

    /// [`Store::forget_voiceprints`] for every person of `persons` in one transaction (all
    /// or nothing), followed by one truncating checkpoint. Returns how many voiceprints
    /// were deleted.
    ///
    /// # Errors
    /// As [`Store::forget_voiceprints`]: [`StoreError::Busy`] after the commit means every
    /// deletion is committed.
    pub fn forget_voiceprints_of<C: Clock + ?Sized>(
        &mut self,
        persons: &[EntityId],
        hlc: &mut Hlc,
        clock: &C,
    ) -> Result<usize, StoreError> {
        let n = self.transaction(|tx| {
            let mut n = 0;
            for &person in persons {
                n += crate::identities::forget_identity_biometrics(tx, person, hlc, clock)?;
            }
            Ok::<usize, StoreError>(n)
        })?;
        self.checkpoint_with_retry(CHECKPOINT_ATTEMPTS)?;
        Ok(n)
    }

    /// [`Store::checkpoint`] up to `attempts` times, each waiting at most
    /// [`CHECKPOINT_WAIT`] for readers, then the normal busy timeout is restored.
    fn checkpoint_with_retry(&self, attempts: u32) -> Result<(), StoreError> {
        self.conn.busy_timeout(CHECKPOINT_WAIT)?;
        let mut last = Ok(());
        for _ in 0..attempts.max(1) {
            last = self.checkpoint();
            if !matches!(last, Err(StoreError::Busy(_))) {
                break;
            }
        }
        self.conn.busy_timeout(BUSY_TIMEOUT)?;
        last
    }

    /// `PRAGMA wal_checkpoint(TRUNCATE)`: copies the WAL into the database and empties it.
    /// A no-op for an in-memory store.
    ///
    /// # Errors
    /// [`StoreError::Busy`] if a reader or writer blocked it; SQLite errors.
    pub fn checkpoint(&self) -> Result<(), StoreError> {
        let (busy, log, done): (i64, i64, i64) =
            self.conn
                .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |r| {
                    Ok((r.get(0)?, r.get(1)?, r.get(2)?))
                })?;
        if busy != 0 || log != done {
            return Err(StoreError::Busy(format!(
                "wal checkpoint incomplete ({done} of {log} frames)"
            )));
        }
        Ok(())
    }

    /// [`earshot_core::model::replace_segments`] in one transaction: either the whole
    /// replacement is applied or nothing is.
    ///
    /// # Errors
    /// As the core function; nothing is written on error.
    pub fn replace_segments<C: Clock + ?Sized>(
        &mut self,
        meeting: EntityId,
        pass: TranscriptPass,
        segments: Vec<Segment>,
        hlc: &mut Hlc,
        clock: &C,
    ) -> Result<usize, StoreError> {
        self.transaction(|tx| {
            Ok(earshot_core::model::replace_segments(
                tx, meeting, pass, segments, hlc, clock,
            )?)
        })
    }
}

impl StoreTx<'_> {
    /// [`Store::local_value`] inside the transaction.
    ///
    /// # Errors
    /// SQLite errors.
    pub fn local_value(&self, key: &str) -> Result<Option<Vec<u8>>, StoreError> {
        get_local(&self.conn, key)
    }

    /// [`Store::set_local_value`] inside the transaction.
    ///
    /// # Errors
    /// [`StoreError::Corrupt`] for the reserved key; SQLite errors.
    pub fn set_local_value(&mut self, key: &str, value: &[u8]) -> Result<(), StoreError> {
        set_local(&self.conn, key, value)
    }

    /// [`Store::delete_local_value`] inside the transaction.
    ///
    /// # Errors
    /// [`StoreError::Corrupt`] for the reserved key; SQLite errors.
    pub fn delete_local_value(&mut self, key: &str) -> Result<bool, StoreError> {
        delete_local(&self.conn, key)
    }
}

fn get_local(conn: &Connection, key: &str) -> Result<Option<Vec<u8>>, StoreError> {
    use rusqlite::OptionalExtension;
    Ok(conn
        .query_row("SELECT value FROM local_state WHERE key = ?1", [key], |r| {
            r.get(0)
        })
        .optional()?)
}

fn set_local(conn: &Connection, key: &str, value: &[u8]) -> Result<(), StoreError> {
    if key == "device_id" {
        return Err(StoreError::Corrupt(
            "local_state key device_id is reserved".into(),
        ));
    }
    conn.execute(
        "INSERT INTO local_state (key, value) VALUES (?1, ?2) \
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        rusqlite::params![key, value],
    )?;
    Ok(())
}

fn delete_local(conn: &Connection, key: &str) -> Result<bool, StoreError> {
    if key == "device_id" {
        return Err(StoreError::Corrupt(
            "local_state key device_id is reserved".into(),
        ));
    }
    Ok(conn.execute("DELETE FROM local_state WHERE key = ?1", [key])? > 0)
}

/// Closing the store runs a final TRUNCATE checkpoint (best effort), so a forgotten
/// vector whose checkpoint was blocked during the session does not outlive it in the WAL.
impl Drop for Store {
    fn drop(&mut self) {
        let _ = self.checkpoint_with_retry(CHECKPOINT_ATTEMPTS);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schema_version_matches_the_migration_count() {
        assert_eq!(schema::migrations().len(), SCHEMA_VERSION as usize);
    }
}
