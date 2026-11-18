//! The relay's SQLite database (docs/sync-spec.md section 4). One connection behind the
//! relay's mutex; every multi-step change is one IMMEDIATE transaction.

use std::path::Path;

use earshot_sync::wire::{
    B64, PresenceEntry, PresenceResponse, PullResponse, PulledRow, VaultDevice, VaultResponse,
};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use sha2::{Digest, Sha256};

/// The schema this build writes. A database with a higher `user_version` is refused.
const SCHEMA_VERSION: u32 = 1;

const SCHEMA: &str = "
CREATE TABLE vault (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    format INTEGER NOT NULL,
    key_check BLOB,
    next_seq INTEGER NOT NULL
) STRICT;
INSERT INTO vault VALUES (1, 0, NULL, 1);
CREATE TABLE device (
    device_id BLOB PRIMARY KEY NOT NULL,
    token_sha256 BLOB NOT NULL UNIQUE,
    created_unix_s INTEGER NOT NULL,
    last_seen_unix_s INTEGER NOT NULL,
    presence BLOB,
    presence_unix_s INTEGER,
    revoked INTEGER NOT NULL
) STRICT;
CREATE TABLE row (
    key BLOB PRIMARY KEY NOT NULL,
    wall_ms INTEGER NOT NULL,
    logical INTEGER NOT NULL,
    node BLOB NOT NULL,
    seq INTEGER NOT NULL UNIQUE,
    device_id BLOB NOT NULL,
    body BLOB NOT NULL
) STRICT;
CREATE TABLE pairing (
    slot BLOB PRIMARY KEY NOT NULL,
    wrapped BLOB,
    expires_unix_s INTEGER NOT NULL,
    taken INTEGER NOT NULL
) STRICT;
";

/// A database problem.
#[derive(Debug)]
pub enum DbError {
    /// The data folder.
    Io(std::io::Error),
    /// SQLite.
    Sqlite(rusqlite::Error),
    /// Written by a newer relay: update the container.
    Newer(u32),
}

impl DbError {
    /// A fixed word for logs.
    #[must_use]
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Io(_) => "io",
            Self::Sqlite(_) => "sqlite",
            Self::Newer(_) => "newer",
        }
    }
}

impl std::fmt::Display for DbError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(e) => write!(f, "{e}"),
            Self::Sqlite(e) => write!(f, "{e}"),
            Self::Newer(v) => write!(
                f,
                "the database was written by a newer relay (schema {v}); update the container"
            ),
        }
    }
}

impl std::error::Error for DbError {}

impl From<rusqlite::Error> for DbError {
    fn from(e: rusqlite::Error) -> Self {
        Self::Sqlite(e)
    }
}

/// SHA-256 of a bearer token: all the relay keeps of it.
pub(crate) fn token_hash(token: &str) -> [u8; 32] {
    Sha256::digest(token.as_bytes()).into()
}

/// A new bearer token: 32 bytes from the operating system's generator, as hex.
pub(crate) fn new_token() -> Result<String, ()> {
    let mut b = [0u8; 32];
    getrandom::fill(&mut b).map_err(|_| ())?;
    Ok(b.iter().map(|x| format!("{x:02x}")).collect())
}

/// A row's `updated_at`, ordered as `HlcTimestamp` orders it: wall, logical, node.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct Stamp {
    pub wall_ms: i64,
    pub logical: u32,
    pub node: [u8; 16],
}

/// A row as pushed, checked.
pub(crate) struct NewRow<'a> {
    pub key: [u8; 16],
    pub stamp: Stamp,
    pub body: &'a [u8],
}

/// What a push did.
pub(crate) struct Pushed {
    pub accepted: u32,
    pub superseded: Vec<[u8; 16]>,
    pub seq: u64,
}

/// The outcome of a registration.
pub(crate) enum Registered {
    Yes,
    KeyMismatch,
    /// The slot was used or expired between the check and the write.
    SlotGone,
    /// A slot registration named a live device id (not revoked): a code never takes one over.
    DeviceExists,
}

pub(crate) struct Db(Connection);

fn u64_of(v: i64) -> u64 {
    u64::try_from(v).unwrap_or(0)
}

fn i64_of(v: u64) -> i64 {
    i64::try_from(v).unwrap_or(i64::MAX)
}

fn blob16(v: Vec<u8>) -> [u8; 16] {
    let mut out = [0u8; 16];
    if v.len() == 16 {
        out.copy_from_slice(&v);
    }
    out
}

impl Db {
    /// Opens or creates the database at `path`.
    pub(crate) fn open(path: &Path) -> Result<Self, DbError> {
        let mut c = Connection::open(path)?;
        let mode: String = c.query_row("PRAGMA journal_mode = WAL", [], |r| r.get(0))?;
        debug_assert_eq!(mode.to_ascii_lowercase(), "wal");
        c.pragma_update(None, "synchronous", "NORMAL")?;
        c.busy_timeout(std::time::Duration::from_secs(5))?;
        let tx = c.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let version: u32 = tx.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        match version {
            0 => {
                tx.execute_batch(SCHEMA)?;
                tx.pragma_update(None, "user_version", SCHEMA_VERSION)?;
            }
            SCHEMA_VERSION => {}
            v => return Err(DbError::Newer(v)),
        }
        tx.commit()?;
        Ok(Self(c))
    }

    /// Counts and sizes of what the relay holds (`earshot-sync-server inspect`, `accept m11`).
    /// Nothing it holds is read but lengths.
    pub(crate) fn inspect(&self) -> Result<crate::Inspection, DbError> {
        let (rows, largest_row, rows_over_limit): (i64, i64, i64) = self.0.query_row(
            "SELECT count(*), coalesce(max(length(body)), 0), \
             coalesce(sum(length(body) > ?1), 0) FROM row",
            [i64::try_from(earshot_sync::wire::MAX_ROW_BODY).unwrap_or(i64::MAX)],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )?;
        let (devices, largest_presence, presence_over_limit): (i64, i64, i64) = self.0.query_row(
            "SELECT count(*), coalesce(max(length(presence)), 0), \
             coalesce(sum(length(presence) > ?1), 0) FROM device",
            [i64::try_from(earshot_sync::wire::MAX_PRESENCE).unwrap_or(i64::MAX)],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )?;
        let n = |v: i64| u64::try_from(v).unwrap_or(0);
        Ok(crate::Inspection {
            rows: n(rows),
            largest_row_bytes: n(largest_row),
            rows_over_limit: n(rows_over_limit),
            row_limit_bytes: earshot_sync::wire::MAX_ROW_BODY as u64,
            devices: n(devices),
            largest_presence_bytes: n(largest_presence),
            presence_over_limit: n(presence_over_limit),
        })
    }

    /// The device holding `token_hash`, unless revoked; records that it was seen at `now`.
    pub(crate) fn device_by_token(
        &self,
        token_hash: &[u8; 32],
        now: u64,
    ) -> Result<Option<[u8; 16]>, DbError> {
        let id: Option<Vec<u8>> = self
            .0
            .query_row(
                "SELECT device_id FROM device WHERE token_sha256 = ?1 AND revoked = 0",
                params![&token_hash[..]],
                |r| r.get(0),
            )
            .optional()?;
        let Some(id) = id else { return Ok(None) };
        self.0.execute(
            "UPDATE device SET last_seen_unix_s = ?2 WHERE device_id = ?1",
            params![id, i64_of(now)],
        )?;
        Ok(Some(blob16(id)))
    }

    /// The vault's format.
    pub(crate) fn vault_format(&self) -> Result<u32, DbError> {
        Ok(self
            .0
            .query_row("SELECT format FROM vault WHERE id = 1", [], |r| r.get(0))?)
    }

    /// Whether `slot` has been taken and has not expired: the one registration it allows.
    pub(crate) fn slot_taken_and_live(&self, slot: &[u8; 16], now: u64) -> Result<bool, DbError> {
        Ok(self
            .0
            .query_row(
                "SELECT 1 FROM pairing WHERE slot = ?1 AND taken = 1 AND expires_unix_s > ?2",
                params![&slot[..], i64_of(now)],
                |_| Ok(()),
            )
            .optional()?
            .is_some())
    }

    /// Registers `device` with a new token. The vault's key check is set by the first
    /// registration and must match after that; a slot, when given, is used up.
    pub(crate) fn register(
        &mut self,
        device: &[u8; 16],
        check: &[u8; 32],
        token_hash: &[u8; 32],
        slot: Option<&[u8; 16]>,
        now: u64,
    ) -> Result<Registered, DbError> {
        let tx = self
            .0
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let stored: Option<Vec<u8>> =
            tx.query_row("SELECT key_check FROM vault WHERE id = 1", [], |r| r.get(0))?;
        match stored {
            Some(s) if s.as_slice() != check.as_slice() => return Ok(Registered::KeyMismatch),
            Some(_) => {}
            None => {
                tx.execute(
                    "UPDATE vault SET key_check = ?1 WHERE id = 1",
                    params![&check[..]],
                )?;
            }
        }
        if let Some(slot) = slot {
            let used = tx.execute(
                "DELETE FROM pairing WHERE slot = ?1 AND taken = 1 AND expires_unix_s > ?2",
                params![&slot[..], i64_of(now)],
            )?;
            if used != 1 {
                return Ok(Registered::SlotGone);
            }
            // A pairing code adds a device, or brings back one that was removed (the owner,
            // 2026-11-16: the code was shown on a live device of the vault, which already holds
            // the key the joiner unwrapped). It never takes over a live device: that would hand
            // a live device's id to whoever holds a code. The setup secret registers any id.
            let revoked: Option<bool> = tx
                .query_row(
                    "SELECT revoked FROM device WHERE device_id = ?1",
                    params![&device[..]],
                    |r| r.get::<_, i64>(0).map(|v| v != 0),
                )
                .optional()?;
            if revoked == Some(false) {
                return Ok(Registered::DeviceExists);
            }
        }
        // A device registering again (the setup secret: a lost token, or set up anew; a pairing
        // code: a removed device coming back) keeps its first date, gets the new token and is no
        // longer revoked. A revoked device's presence was cleared by the revoke, so it starts
        // with none.
        tx.execute(
            "INSERT INTO device (device_id, token_sha256, created_unix_s, last_seen_unix_s, revoked)
             VALUES (?1, ?2, ?3, ?3, 0)
             ON CONFLICT(device_id) DO UPDATE SET
                token_sha256 = excluded.token_sha256,
                last_seen_unix_s = excluded.last_seen_unix_s,
                revoked = 0",
            params![&device[..], &token_hash[..], i64_of(now)],
        )?;
        tx.commit()?;
        Ok(Registered::Yes)
    }

    /// Revokes `device` (its token stops working). Unknown devices change nothing.
    pub(crate) fn revoke(&self, device: &[u8; 16]) -> Result<(), DbError> {
        self.0.execute(
            "UPDATE device SET revoked = 1, presence = NULL, presence_unix_s = NULL
             WHERE device_id = ?1",
            params![&device[..]],
        )?;
        Ok(())
    }

    fn sweep_slots(&self, now: u64) -> Result<(), DbError> {
        self.0.execute(
            "DELETE FROM pairing WHERE expires_unix_s <= ?1",
            params![i64_of(now)],
        )?;
        Ok(())
    }

    /// Stores `wrapped` in `slot` until `expires`.
    pub(crate) fn offer_slot(
        &self,
        slot: &[u8; 16],
        wrapped: &[u8],
        expires: u64,
        now: u64,
    ) -> Result<(), DbError> {
        self.sweep_slots(now)?;
        self.0.execute(
            "INSERT OR REPLACE INTO pairing (slot, wrapped, expires_unix_s, taken)
             VALUES (?1, ?2, ?3, 0)",
            params![&slot[..], wrapped, i64_of(expires)],
        )?;
        Ok(())
    }

    /// The wrapped key in `slot`, once: the key is deleted and the slot marked taken, so it
    /// allows one registration for the rest of its life.
    pub(crate) fn take_slot(
        &mut self,
        slot: &[u8; 16],
        now: u64,
    ) -> Result<Option<Vec<u8>>, DbError> {
        self.sweep_slots(now)?;
        let tx = self
            .0
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let wrapped: Option<Vec<u8>> = tx
            .query_row(
                "SELECT wrapped FROM pairing WHERE slot = ?1 AND taken = 0 AND wrapped IS NOT NULL",
                params![&slot[..]],
                |r| r.get(0),
            )
            .optional()?;
        if wrapped.is_some() {
            tx.execute(
                "UPDATE pairing SET wrapped = NULL, taken = 1 WHERE slot = ?1",
                params![&slot[..]],
            )?;
        }
        tx.commit()?;
        Ok(wrapped)
    }

    /// Stores each row whose stamp is later than the one held for its key, numbering it with
    /// the next sequence; raises the vault's format to `format`.
    pub(crate) fn push(
        &mut self,
        device: &[u8; 16],
        format: u32,
        rows: &[NewRow<'_>],
    ) -> Result<Pushed, DbError> {
        let tx = self
            .0
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut next: i64 =
            tx.query_row("SELECT next_seq FROM vault WHERE id = 1", [], |r| r.get(0))?;
        let mut accepted = 0u32;
        let mut superseded = Vec::new();
        for row in rows {
            let held: Option<(i64, u32, Vec<u8>)> = tx
                .query_row(
                    "SELECT wall_ms, logical, node FROM row WHERE key = ?1",
                    params![&row.key[..]],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                )
                .optional()?;
            let newer = held.is_none_or(|(w, l, n)| {
                row.stamp
                    > Stamp {
                        wall_ms: w,
                        logical: l,
                        node: blob16(n),
                    }
            });
            if !newer {
                superseded.push(row.key);
                continue;
            }
            tx.execute(
                "INSERT INTO row (key, wall_ms, logical, node, seq, device_id, body)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
                 ON CONFLICT(key) DO UPDATE SET
                    wall_ms = excluded.wall_ms, logical = excluded.logical,
                    node = excluded.node, seq = excluded.seq,
                    device_id = excluded.device_id, body = excluded.body",
                params![
                    &row.key[..],
                    row.stamp.wall_ms,
                    row.stamp.logical,
                    &row.stamp.node[..],
                    next,
                    &device[..],
                    row.body
                ],
            )?;
            next += 1;
            accepted += 1;
        }
        tx.execute(
            "UPDATE vault SET next_seq = ?1, format = max(format, ?2) WHERE id = 1",
            params![next, format],
        )?;
        tx.commit()?;
        Ok(Pushed {
            accepted,
            superseded,
            seq: u64_of(next - 1),
        })
    }

    /// Rows with a sequence above `after`, ascending: at most `limit` rows and at most
    /// `max_bytes` of bodies (always at least one row when there is one). `None` when
    /// `caller_format` is below the vault's: checked in the one read transaction that reads the
    /// rows, so a push raising the format (which commits with its rows) is seen whole or not at
    /// all.
    pub(crate) fn pull(
        &mut self,
        caller_format: u32,
        after: u64,
        limit: u32,
        max_bytes: usize,
    ) -> Result<Option<PullResponse>, DbError> {
        let tx = self.0.transaction()?;
        let vault_format: u32 =
            tx.query_row("SELECT format FROM vault WHERE id = 1", [], |r| r.get(0))?;
        if caller_format < vault_format {
            return Ok(None);
        }
        let mut st = tx.prepare(
            "SELECT seq, key, wall_ms, logical, node, body FROM row
             WHERE seq > ?1 ORDER BY seq LIMIT ?2",
        )?;
        let mut rows = Vec::new();
        let mut bytes = 0usize;
        let mut more = false;
        let mut q = st.query(params![i64_of(after), i64::from(limit) + 1])?;
        while let Some(r) = q.next()? {
            let body: Vec<u8> = r.get(5)?;
            if rows.len() == limit as usize || (!rows.is_empty() && bytes + body.len() > max_bytes)
            {
                more = true;
                break;
            }
            bytes += body.len();
            rows.push(PulledRow {
                seq: u64_of(r.get(0)?),
                key: B64(r.get(1)?),
                wall_ms: u64_of(r.get(2)?),
                logical: r.get(3)?,
                node: B64(r.get(4)?),
                body: B64(body),
            });
        }
        drop(q);
        drop(st);
        tx.commit()?;
        Ok(Some(PullResponse {
            rows,
            more,
            vault_format,
        }))
    }

    /// Stores `device`'s presence blob, received at `now`.
    pub(crate) fn put_presence(
        &self,
        device: &[u8; 16],
        blob: &[u8],
        now: u64,
    ) -> Result<(), DbError> {
        self.0.execute(
            "UPDATE device SET presence = ?2, presence_unix_s = ?3 WHERE device_id = ?1",
            params![&device[..], blob, i64_of(now)],
        )?;
        Ok(())
    }

    /// Every live device's presence blob with its age.
    pub(crate) fn presence(&self, now: u64) -> Result<PresenceResponse, DbError> {
        let mut st = self.0.prepare(
            "SELECT device_id, presence, presence_unix_s FROM device
             WHERE revoked = 0 AND presence IS NOT NULL ORDER BY device_id",
        )?;
        let devices = st
            .query_map([], |r| {
                let at: i64 = r.get(2)?;
                Ok(PresenceEntry {
                    device_id: B64(r.get(0)?),
                    age_s: now.saturating_sub(u64_of(at)),
                    blob: B64(r.get(1)?),
                })
            })?
            .collect::<Result<_, _>>()?;
        Ok(PresenceResponse { devices })
    }

    /// The vault's format and live devices.
    pub(crate) fn vault(&self, now: u64) -> Result<VaultResponse, DbError> {
        let mut st = self.0.prepare(
            "SELECT device_id, created_unix_s, last_seen_unix_s FROM device
             WHERE revoked = 0 ORDER BY created_unix_s, device_id",
        )?;
        let devices = st
            .query_map([], |r| {
                let seen: i64 = r.get(2)?;
                Ok(VaultDevice {
                    device_id: B64(r.get(0)?),
                    created_unix_s: u64_of(r.get(1)?),
                    age_s: now.saturating_sub(u64_of(seen)),
                })
            })?
            .collect::<Result<_, _>>()?;
        Ok(VaultResponse {
            vault_format: self.vault_format()?,
            devices,
        })
    }
}
