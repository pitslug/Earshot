//! The relay's SQLite database (docs/sync-spec.md section 4). One connection behind the
//! relay's mutex; every multi-step change is one IMMEDIATE transaction.

use std::path::Path;

use earshot_sync::wire::{
    B64, PresenceEntry, PresenceResponse, PullResponse, PulledRow, VaultDevice, VaultResponse,
};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

/// The schema this build writes. A database with a higher `user_version` is refused.
///
/// 2 (T14.45): `vault.epoch`, the vault's epoch. A relay from before it refuses a version 2
/// database ("written by a newer relay"), so a relay cannot go back to an older image once
/// this one has opened its data.
const SCHEMA_VERSION: u32 = 2;

/// Version 1, as every relay up to 0.4.2 created it. Never edited: a later version is a step
/// in [`Db::open`].
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

/// Version 2 (T14.45): the vault's epoch (absent until [`Db::open`] gives the vault one), the
/// vault's highest sequence number when that epoch was taken after a restore was noticed
/// (`epoch_floor`; NULL while this database has noticed none), when an epoch was last taken
/// (`claim_unix_s`), and for each device the epoch its numbers are known to belong to
/// (`served_epoch`, `hear_claim`). Columns added with no default to compute: the step costs
/// the same however many rows the relay holds, and rewrites none of them.
const SCHEMA_2: &str = "
ALTER TABLE vault ADD COLUMN epoch BLOB;
ALTER TABLE vault ADD COLUMN epoch_floor INTEGER;
ALTER TABLE vault ADD COLUMN claim_unix_s INTEGER NOT NULL DEFAULT 0;
ALTER TABLE device ADD COLUMN served_epoch BLOB;
";

/// How often, at most, the vault takes a new epoch: once in ten minutes, a judgement. A new
/// epoch makes every device that knows of epochs send its whole store again, a restore is one
/// event, and this is what keeps a device that sends ever higher claims (a fault, or a token
/// in the wrong hands) from doing that to the others more than six times an hour. Inside the
/// limit such a claim changes nothing and is answered like any call: a device of this epoch
/// that saw more still starts over by itself (it reads the vault's highest number below its
/// own), and a client from before the epoch claims again at its next pull. Filing rows again
/// for a client from before the epoch is not under this limit: it makes nobody start over, and
/// is bounded for each device by itself (once per epoch).
pub const CLAIM_EVERY_S: u64 = 10 * 60;

/// The most rows filed again in one go for a client from before the epoch (`hear_claim`):
/// 20,000, a judgement. They are renumbered one by one in the transaction of the pull that
/// asked, with the database held, so the number bounds how long one call can hold every
/// other. Past it the rows are left where they are: that old client misses them, and gets what
/// is filed from then on.
pub const REFILE_MAX: i64 = 20_000;

/// The furthest above the vault's own highest number a claim may move its numbering: about a
/// thousand million (2^30), a judgement. A real cursor is a number this relay once gave out,
/// so it is above what the relay holds now by no more than the rows lost with a restore; a
/// restored relay cannot know that figure, but no vault loses a thousand million rows (each is
/// a sealed row of hundreds of bytes). A claim further out is taken to be at this distance,
/// never refused and never followed: one call naming a number near the top of what the
/// database can count to would otherwise put every later row there, and the vault could not
/// be used again. At this step it would take thousands of millions of such claims, one every
/// ten minutes, to run out of numbers.
pub const MAX_CLAIM_JUMP: i64 = 1 << 30;

/// A database problem.
#[derive(Debug)]
pub enum DbError {
    /// The data folder.
    Io(std::io::Error),
    /// SQLite.
    Sqlite(rusqlite::Error),
    /// Written by a newer relay: update the container.
    Newer(u32),
    /// The operating system's random number generator failed (a new epoch could not be made).
    Random,
}

impl DbError {
    /// A fixed word for logs.
    #[must_use]
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Io(_) => "io",
            Self::Sqlite(_) => "sqlite",
            Self::Newer(_) => "newer",
            Self::Random => "random",
        }
    }
}

impl std::fmt::Display for DbError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(e) => write!(f, "{e}"),
            Self::Sqlite(e) => write!(f, "{e}"),
            Self::Random => f.write_str("the system's random number generator failed"),
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
    /// The vault's epoch after the push.
    pub epoch: [u8; 16],
    /// The vault's highest sequence number before the push (and before its claim was heard).
    pub max_seq_before: u64,
}

/// What a device says it has seen of the vault (T14.45).
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct Claim {
    /// The highest sequence number it has seen.
    pub seen: Option<u64>,
    /// The vault epoch it last saw.
    pub epoch: Option<[u8; 16]>,
    /// The relay's clock when the call came (Unix seconds).
    pub now_s: u64,
    /// The device.
    pub device: [u8; 16],
}

/// A new vault epoch: 16 bytes from the operating system's generator. Not a secret; it only has
/// to differ from every earlier one.
fn new_epoch() -> Result<[u8; 16], DbError> {
    let mut b = [0u8; 16];
    getrandom::fill(&mut b).map_err(|_| DbError::Random)?;
    Ok(b)
}

/// Hears `claim` inside the caller's transaction (T14.45) and returns the vault's highest
/// sequence number before it and the vault's epoch after it.
///
/// A device that has seen a sequence number this vault has not reached proves the relay lost
/// rows: it was restored from an older copy. One restore is one new epoch. The relay keeps,
/// for each device, the epoch its numbers are known to belong to (`device.served_epoch`): the
/// vault's epoch once the device has named it, or has been dealt with below. What the relay
/// does depends on who calls:
///
/// - **A device whose numbers are this epoch's** (it names the epoch, or names none and was
///   served in it), or any device that names none while this database has noticed no restore:
///   if it has seen more than the vault holds, the vault takes a new epoch (at most once in
///   [`CLAIM_EVERY_S`]), so every device that knows of epochs sends its rows again and reads
///   from the start. The vault's highest number at that moment is kept (`epoch_floor`): rows
///   numbered above it were filed in the new epoch. The numbering moves past what the device
///   saw, so rows filed from now on are above its old cursor.
/// - **A device that names no epoch and has not been served in this epoch, after a restore
///   was noticed** (a client from before the epoch, at its first pull since: its cursor only
///   ever moves forward, so it would never see rows numbered at or below it): no new epoch.
///   The rows filed since the epoch was taken that are numbered at or below its cursor, those
///   in `(epoch_floor, cursor]`, are filed again above everything, in their order, so its pull
///   brings them with the rest. This is done whether or not its cursor is above what the
///   vault holds: rows filed meanwhile may have passed it. It makes nobody start over (devices
///   of the epoch see the same epoch and a higher highest number, and one that had pulled
///   those rows pulls them once more and changes nothing), so it is not under the ten-minute
///   limit: each old device is served at its own first pull, once per epoch. More than
///   [`REFILE_MAX`] such rows are left where they are.
/// - **A device that names another epoch**: nothing. It saw another history and starts over by
///   itself when it reads this vault's epoch in the answer.
///
/// A claim never moves the numbering more than [`MAX_CLAIM_JUMP`] above the vault's rows.
///
/// `epoch_floor` is kept until the vault next takes an epoch; nothing clears it sooner. It
/// costs nothing as the vault grows: only rows numbered at or below the calling device's own
/// cursor count towards [`REFILE_MAX`], and rows filed later are numbered above every cursor.
fn hear_claim(tx: &rusqlite::Transaction<'_>, claim: &Claim) -> Result<(u64, [u8; 16]), DbError> {
    let (next, epoch, floor, acted_at): (i64, Vec<u8>, Option<i64>, i64) = tx.query_row(
        "SELECT next_seq, epoch, epoch_floor, claim_unix_s FROM vault WHERE id = 1",
        [],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
    )?;
    let highest = next - 1;
    let max = u64_of(highest);
    let epoch = blob16(epoch);
    if claim.epoch.is_some_and(|theirs| theirs != epoch) {
        return Ok((max, epoch));
    }
    let served = tx
        .query_row(
            "SELECT served_epoch FROM device WHERE device_id = ?1",
            params![&claim.device[..]],
            |r| r.get::<_, Option<Vec<u8>>>(0),
        )
        .optional()?
        .flatten()
        .is_some_and(|theirs| theirs == epoch);
    let serve = |epoch: &[u8; 16]| {
        tx.execute(
            "UPDATE device SET served_epoch = ?1 WHERE device_id = ?2",
            params![&epoch[..], &claim.device[..]],
        )
    };
    // What the device has seen, taken to be at most MAX_CLAIM_JUMP above what the vault holds.
    let seen = claim
        .seen
        .map(|seen| i64_of(seen).min(highest.saturating_add(MAX_CLAIM_JUMP)));
    let Some(floor) = floor.filter(|_| claim.epoch.is_none() && !served) else {
        // The device's numbers are this epoch's (it names the epoch, or was served in it), or
        // this database has noticed no restore. Having seen more than the vault holds, it
        // proves a restore: a new epoch, unless one was taken a moment ago. (A clock that
        // reads earlier than the last time does not hold one back.)
        let Some(seen) = seen.filter(|seen| *seen > highest) else {
            if !served {
                serve(&epoch)?;
            }
            return Ok((max, epoch));
        };
        let taken_at = u64_of(acted_at);
        if taken_at != 0 && claim.now_s >= taken_at && claim.now_s - taken_at < CLAIM_EVERY_S {
            return Ok((max, epoch));
        }
        let epoch = new_epoch()?;
        tx.execute(
            "UPDATE vault SET next_seq = ?1, epoch = ?2, epoch_floor = ?3, claim_unix_s = ?4
             WHERE id = 1",
            params![
                seen.saturating_add(1),
                &epoch[..],
                highest,
                i64_of(claim.now_s)
            ],
        )?;
        serve(&epoch)?;
        return Ok((max, epoch));
    };
    // A client from before the epoch, at its first call since the epoch was taken. A push
    // says nothing of its cursor: its pull is waited for.
    let Some(cursor) = seen else {
        return Ok((max, epoch));
    };
    let behind: i64 = tx.query_row(
        "SELECT count(*) FROM row WHERE seq > ?1 AND seq <= ?2",
        params![floor, cursor],
        |r| r.get(0),
    )?;
    let above = cursor.max(highest);
    let mut next_seq = above.saturating_add(1);
    if (1..=REFILE_MAX).contains(&behind) {
        let keys: Vec<Vec<u8>> = {
            let mut st =
                tx.prepare("SELECT key FROM row WHERE seq > ?1 AND seq <= ?2 ORDER BY seq")?;
            st.query_map(params![floor, cursor], |r| r.get(0))?
                .collect::<Result<_, _>>()?
        };
        // Every new number is above every number in use, so none meets another on the way.
        for key in keys {
            tx.execute(
                "UPDATE row SET seq = ?1 WHERE key = ?2",
                params![next_seq, key],
            )?;
            next_seq = next_seq.saturating_add(1);
        }
    }
    if next_seq > next {
        tx.execute(
            "UPDATE vault SET next_seq = ?1 WHERE id = 1",
            params![next_seq],
        )?;
    }
    serve(&epoch)?;
    Ok((max, epoch))
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
        if version > SCHEMA_VERSION {
            return Err(DbError::Newer(version));
        }
        // Each step runs once, in this one transaction with the version it leaves: a database
        // is at a whole version or the one before, never between.
        if version < 1 {
            tx.execute_batch(SCHEMA)?;
        }
        if version < 2 {
            // The epoch of a vault that already holds rows starts here: its devices have seen
            // none yet and take the first they are given without reading anything again.
            tx.execute_batch(SCHEMA_2)?;
            tx.execute(
                "UPDATE vault SET epoch = ?1 WHERE id = 1",
                params![&new_epoch()?[..]],
            )?;
        }
        if version < SCHEMA_VERSION {
            tx.pragma_update(None, "user_version", SCHEMA_VERSION)?;
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
            // Compared in constant time (T14.46): a device that does not hold the vault's key
            // learns nothing of its check from how long a refusal took.
            Some(s) if !bool::from(s.as_slice().ct_eq(check.as_slice())) => {
                return Ok(Registered::KeyMismatch);
            }
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
    /// the next sequence; raises the vault's format to `format`. `None`, with nothing stored
    /// and nothing changed, when `format` is below the vault's: checked here, in the
    /// transaction that stores the rows (T14.46), so a device that passed an earlier check
    /// cannot store rows of an older format after another device raised the vault's. The
    /// device's `claim` is heard next, in the same transaction (`hear_claim`).
    pub(crate) fn push(
        &mut self,
        device: &[u8; 16],
        format: u32,
        rows: &[NewRow<'_>],
        claim: &Claim,
    ) -> Result<Option<Pushed>, DbError> {
        let tx = self
            .0
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let vault_format: u32 =
            tx.query_row("SELECT format FROM vault WHERE id = 1", [], |r| r.get(0))?;
        if format < vault_format {
            return Ok(None);
        }
        let (max_seq_before, epoch) = hear_claim(&tx, claim)?;
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
        Ok(Some(Pushed {
            accepted,
            superseded,
            seq: u64_of(next - 1),
            epoch,
            max_seq_before,
        }))
    }

    /// Rows with a sequence above `after`, ascending: at most `limit` rows and at most
    /// `max_bytes` of bodies (always at least one row when there is one). `None` when
    /// `caller_format` is below the vault's: checked in the one read transaction that reads the
    /// rows, so a push raising the format (which commits with its rows) is seen whole or not at
    /// all. The device's `claim` is heard in the same transaction, once the format has let it
    /// in (`hear_claim`); a device below the vault's format changes nothing.
    pub(crate) fn pull(
        &mut self,
        caller_format: u32,
        after: u64,
        limit: u32,
        max_bytes: usize,
        claim: &Claim,
    ) -> Result<Option<PullResponse>, DbError> {
        let tx = self.0.transaction()?;
        let vault_format: u32 =
            tx.query_row("SELECT format FROM vault WHERE id = 1", [], |r| r.get(0))?;
        if caller_format < vault_format {
            return Ok(None);
        }
        let (max_seq_before, epoch) = hear_claim(&tx, claim)?;
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
            epoch: Some(B64(epoch.to_vec())),
            max_seq: Some(max_seq_before),
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

#[cfg(test)]
mod tests {
    use super::*;

    fn row(key: u8) -> NewRow<'static> {
        NewRow {
            key: [key; 16],
            stamp: Stamp {
                wall_ms: 5,
                logical: 0,
                node: [9; 16],
            },
            body: b"sealed",
        }
    }

    /// The vault's format, next sequence number, epoch and row count.
    fn state(db: &Db) -> (u32, i64, Vec<u8>, i64) {
        let (format, next, epoch) =
            db.0.query_row(
                "SELECT format, next_seq, epoch FROM vault WHERE id = 1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        let rows =
            db.0.query_row("SELECT count(*) FROM row", [], |r| r.get(0))
                .unwrap();
        (format, next, epoch, rows)
    }

    /// T14.46: the push itself refuses a format below the vault's, in the transaction that
    /// would store its rows. The call's earlier check reads the vault's format under another
    /// lock hold, so a device can pass it just before another device raises the vault; its
    /// push then reaches the database with the old format and must store nothing and change
    /// nothing (not the numbering, not the epoch, whatever it says it has seen).
    #[test]
    fn relay_push_refuses_an_older_format_inside_its_own_transaction() {
        static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "earshot-relay-db-format-{}-{}",
            std::process::id(),
            N.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let mut db = Db::open(&dir.join("relay.db")).unwrap();
        let device = [1u8; 16];
        // A format-1 device passes the early check: the vault is still at 0.
        assert!(1 >= db.vault_format().unwrap());
        // Another device's format-2 push raises the vault first.
        let pushed = db
            .push(&device, 2, &[row(1)], &Claim::default())
            .unwrap()
            .expect("a push at the vault's format or above is stored");
        assert_eq!((pushed.accepted, pushed.seq), (1, 1));
        let before = state(&db);
        assert_eq!((before.0, before.1, before.3), (2, 2, 1));
        // The format-1 push arrives now: refused, with everything as it was.
        assert!(
            db.push(&device, 1, &[row(2)], &Claim::default())
                .unwrap()
                .is_none()
        );
        let claim = Claim {
            seen: Some(50),
            epoch: None,
            now_s: 1_800_000_000,
            device,
        };
        assert!(db.push(&device, 1, &[row(3)], &claim).unwrap().is_none());
        assert_eq!(state(&db), before);
        // At the vault's format it is stored.
        let pushed = db
            .push(&device, 2, &[row(2)], &Claim::default())
            .unwrap()
            .unwrap();
        assert_eq!((pushed.accepted, pushed.seq), (1, 2));
        drop(db);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// T14.45 review: filing rows again for a client from before the epoch is bounded. Up to
    /// [`REFILE_MAX`] rows filed since the epoch was taken, at or below its cursor, are
    /// renumbered above it; one more than that and none is moved (the call must not hold the
    /// database for long), only the numbering moves on, so what is filed from then on still
    /// reaches that client. (The device here is not registered, so each of its pulls is a first
    /// one.)
    #[test]
    fn relay_files_at_most_a_bounded_number_of_rows_again() {
        static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "earshot-relay-db-refile-{}-{}",
            std::process::id(),
            N.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let mut db = Db::open(&dir.join("relay.db")).unwrap();
        let device = [1u8; 16];
        let keyed = |n: i64| {
            let mut key = [0u8; 16];
            key[..8].copy_from_slice(&n.to_be_bytes());
            NewRow {
                key,
                stamp: Stamp {
                    wall_ms: 5,
                    logical: 0,
                    node: [9; 16],
                },
                body: b"sealed",
            }
        };
        let at = |now_s: u64, seen: u64| Claim {
            seen: Some(seen),
            epoch: None,
            now_s,
            device: [7; 16],
        };
        let t = 1_800_000_000;
        // A restore is noticed (the first claim above the rows): a new epoch, floor 0.
        let first = state(&db).2;
        let pushed = db.push(&device, 1, &[], &at(t, 10)).unwrap().unwrap();
        assert_ne!(pushed.epoch.to_vec(), first);
        let epoch = pushed.epoch;
        // Exactly the limit is filed since: all of it is filed again above an old cursor.
        let limit = REFILE_MAX;
        for batch in 0..limit / 1_000 {
            let rows: Vec<NewRow<'_>> = (0..1_000).map(|i| keyed(batch * 1_000 + i)).collect();
            db.push(&device, 1, &rows, &Claim::default()).unwrap();
        }
        let before = state(&db);
        assert_eq!((before.1, before.3), (11 + limit, limit));
        let far = 1_000_000;
        let page = db
            .pull(1, far, 3, usize::MAX, &at(t + CLAIM_EVERY_S, far))
            .unwrap()
            .unwrap();
        assert_eq!(
            page.epoch.as_ref().map(|e| e.0.clone()),
            Some(epoch.to_vec())
        );
        assert_eq!(
            page.rows.iter().map(|r| r.seq).collect::<Vec<_>>(),
            [far + 1, far + 2, far + 3],
            "filed again above the cursor, in order"
        );
        assert_eq!(page.rows[0].key.0[..8], 0i64.to_be_bytes());
        let far_i = i64::try_from(far).unwrap();
        assert_eq!(state(&db).1, far_i + limit + 1);
        let lowest: i64 =
            db.0.query_row("SELECT min(seq) FROM row", [], |r| r.get(0))
                .unwrap();
        assert_eq!(lowest, far_i + 1, "every one of them moved");
        // One more row than the limit: a later old cursor moves none, only the numbering.
        db.push(&device, 1, &[keyed(limit)], &Claim::default())
            .unwrap();
        let farther = 5_000_000;
        let page = db
            .pull(
                1,
                farther,
                3,
                usize::MAX,
                &at(t + 2 * CLAIM_EVERY_S, farther),
            )
            .unwrap()
            .unwrap();
        assert!(page.rows.is_empty(), "too many to file again");
        let after = state(&db);
        assert_eq!(after.1, i64::try_from(farther).unwrap() + 1);
        assert_eq!(after.2, epoch.to_vec(), "and still no new epoch");
        let highest: i64 =
            db.0.query_row("SELECT max(seq) FROM row", [], |r| r.get(0))
                .unwrap();
        assert_eq!(
            highest,
            far_i + limit + 1,
            "the rows stayed where they were"
        );
        // What is filed from now on is above that cursor.
        let pushed = db
            .push(&device, 1, &[keyed(limit + 1)], &Claim::default())
            .unwrap()
            .unwrap();
        assert_eq!(pushed.seq, farther + 1);
        drop(db);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
