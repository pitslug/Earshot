//! Writing rendered notes into an Obsidian vault (PLAN.md 3.8, T5.4, T5.4a-c).
//!
//! [`export_note`] (one meeting) and [`export_notes`] (export all / changed) find each
//! meeting's existing note, merge the render into it (or create a new note), write
//! atomically and record where it went:
//!
//! 1. The vault is validated ([`validate_vault`]) and the meeting must exist in the store.
//! 2. Finding the note: the recorded path (this device's `ExportRecord`) if it still
//!    carries the meeting's `earshot_id`, then the render's path, then the `<stem>*.md`
//!    files beside it, then every non-hidden folder of the vault. The full search runs with
//!    or without a record, so a note the user renamed or moved is always found and never
//!    duplicated, even after the database was lost. Export is on demand only (the owner,
//!    T5.4b), so the cost is accepted: a single export walks the vault, a batch builds one
//!    `VaultIndex` (one open per note) on first need and shares it. An indexed path is
//!    re-checked before use (a stale one falls back to a walk, whose result goes back into
//!    the index), every note the batch writes is added to it, and a miss rebuilds the
//!    index when it is older than `INDEX_MAX_AGE`, so at most one rebuild per
//!    `INDEX_MAX_AGE` window. (A first-time export-all, where every meeting misses, walks
//!    the vault at most once per 30 s of batch time; one meeting's export takes
//!    milliseconds, so that is rare and accepted.) The one remaining race: a note that
//!    appears in the vault from outside (a sync client, the user) after the index was
//!    built is not seen while the index is younger than `INDEX_MAX_AGE` at its meeting's
//!    turn, so that meeting gets a second note; a note present when the index was built,
//!    or written by the batch, is never duplicated.
//!    A matching note whose path cannot be expressed as a
//!    [`VaultPath`] (for example a file name that is not valid Unicode) is an error, never
//!    a reason to create a second note. The record is written before a new note is renamed
//!    into place (step 4).
//! 3. Content is [`earshot_core::export::merge`] of the render into the existing text; if it
//!    equals the current bytes nothing is written.
//! 4. Otherwise it goes to a hidden temp file in the note's folder, is synced, and then:
//!    - update: the note's length and modification time are checked again just before the
//!      rename; if Obsidian or a sync client changed it since it was read, the export
//!      aborts with the retryable [`StoreError::ConcurrentEdit`] and the note is left as the
//!      user has it;
//!    - create: the record is written with the intended path, then the temp file is moved
//!      with a rename that never replaces ([`crate::fsutil::rename_noreplace`]); if a note
//!      appeared at that name meanwhile, the next free `<stem> (n).md` is used. Before each
//!      attempt the render's path and its `<stem>*.md` variants are checked for a note of
//!      this meeting (another export of the same render created it after this one's
//!      search): then the export stops with the retryable [`StoreError::ConcurrentEdit`]
//!      before writing the record. The record write is a claim: in one store access the
//!      record is re-read and written only if it still names what this export last saw
//!      (the record read before the search, then its own claim), else the export stops
//!      with `ConcurrentEdit` (another export, perhaps of a different render whose note has
//!      another name, recorded its note since). Just before the rename the record is read
//!      once more and must still be this export's claim, else `ConcurrentEdit` (an export
//!      that searched after this claim claimed over it and creates the note). So two
//!      creates of one meeting never both rename a note, except when the second claims in
//!      the instant between the first's last look and its rename; with the same render the
//!      no-replace rename and the sibling check exclude even that.
//!
//!    The folder is synced where the platform needs it. Every export writes its own temp
//!    (`.<note>.<pid>-<n>.earshot-tmp`, created exclusively; a name already taken by
//!    someone else's temp is skipped, never removed), so two exports never truncate or
//!    delete each other's temp. An export that stops before its rename (a concurrent edit, a
//!    store error such as a retryable `Busy`, an I/O error) removes its own temp; only a
//!    crash leaves one. Once per folder per export (or per batch), and in the vault
//!    root, leftover temps of crashed exports are removed when their modification time is
//!    more than [`STALE_TEMP_MS`] before or after the folder's own clock, read from the
//!    modification time of a probe file created there (on a network share that is the
//!    server's clock, the one that stamped the temps; the local clock is never compared
//!    with it). A temp written by a live export is younger than that unless the export
//!    itself takes longer, which a single note write does not. If no probe can be created,
//!    nothing is cleaned. Probes (for the clock and for [`validate_vault`]) are named like
//!    temps (`.earshot-probe-<pid>-<n>.earshot-tmp`), so one left by a crash is swept too.
//! 5. The record (one per meeting and device) is upserted with the final path.
//!
//! # Store access
//!
//! The export reaches the store only through [`ExportDb::with_store`], once per short read
//! or write (meeting check and record read, each create's claim and its last look before
//! the rename, the final record write); the vault walk, merge and file writes run between
//! those calls. A caller that shares the store behind a lock (the
//! runtime) takes the lock inside `with_store` only, so other writers are not held up by a
//! vault walk. Every record write re-reads the record inside the same `with_store` call
//! and keeps its id, so a concurrent export of the same meeting never changes it.
//!
//! # Serialisation
//!
//! Exports of one meeting run one at a time (T6.4c, review A8): each holds the meeting's
//! export lease, a `local_state` row `export-lease/<meeting>` (owner: process id and a
//! per-thread token; the time it was taken by the export clock), from before its record
//! read and search until after its final record write. The lease lives in the database, so
//! it serialises exports across threads and across processes sharing the data directory.
//! A second export waits (polling, at most `LeaseTiming::wait`, 30 s) and then searches,
//! finding the first export's note even when its own render names another path (the title
//! changed between them); past the wait it fails with the retryable
//! [`StoreError::ConcurrentEdit`] and writes nothing. A lease older (or newer) than
//! `LeaseTiming::stale_ms` (2 min) by the export clock was left by a crash and is taken
//! over; a release only removes a lease its owner still holds. Taking and releasing are
//! store accesses of their own (one `with_store` call each). The holder re-stamps its lease
//! with the export clock in each later store access (the claim, the last look before a
//! create's rename, the final record write), so only time spent inside one phase (the vault
//! walk, the merge) counts toward staleness; an access that finds the lease no longer its
//! own (taken over after it went stale) stops with the retryable
//! [`StoreError::ConcurrentEdit`] before writing anything. Each of those accesses, and the
//! meeting check with the record write it guards, is one `IMMEDIATE` transaction, so a
//! deletion by another process commits either before the check or after the write. The
//! optimistic checks above stay as a second line (an update's rename, which no store
//! access guards).

use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use earshot_core::export::{RenderedExport, earshot_id_of, merge};
use earshot_core::identity::{Clock, DeviceId, EntityId};
use earshot_core::model::{ExportRecord, ExportRepo, MeetingRepo, RepoError, VaultPath};
use rand_core::Rng;

use crate::db::{Store, StoreError, StoreTx};
use crate::fsutil::{rename_noreplace, sync_dir};

/// Bytes read from the top of a note when looking for its `earshot_id`.
const ID_SCAN_BYTES: u64 = 64 * 1024;

/// Suffix of the hidden temp files the writer creates
/// (`.<note name>.<pid>-<n>.earshot-tmp`).
const TMP_SUFFIX: &str = ".earshot-tmp";

/// Per-process counter making temp names unique.
static TEMP_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Age (by modification time) after which a leftover temp file is treated as stray.
pub const STALE_TEMP_MS: u64 = 5 * 60 * 1000;

/// How many temp names an export tries before giving up (a name is skipped when another
/// process's temp already has it).
const TEMP_ATTEMPTS: usize = 16;

/// A batch index older than this is rebuilt (once) when it misses a meeting, so notes that
/// arrived during a long export-all are still found.
const INDEX_MAX_AGE: std::time::Duration = std::time::Duration::from_secs(30);

/// How often a create retries with the next free name when a note appears at its name.
const CREATE_ATTEMPTS: usize = 8;

/// Why a vault path is unusable.
#[derive(Debug)]
pub enum VaultError {
    /// The path does not exist.
    Missing(PathBuf),
    /// The path is not a directory.
    NotADirectory(PathBuf),
    /// A file cannot be created in it.
    NotWritable(PathBuf, std::io::Error),
}

impl std::fmt::Display for VaultError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            VaultError::Missing(p) => write!(f, "vault {} does not exist", p.display()),
            VaultError::NotADirectory(p) => write!(f, "vault {} is not a folder", p.display()),
            VaultError::NotWritable(p, e) => {
                write!(f, "vault {} is not writable: {e}", p.display())
            }
        }
    }
}

impl std::error::Error for VaultError {}

/// Checks that `vault` exists, is a directory and is writable (a probe file is created and
/// removed). Used when the vault path is saved in settings and before every export.
///
/// # Errors
/// [`VaultError`].
pub fn validate_vault(vault: &Path) -> Result<(), VaultError> {
    let meta = match std::fs::metadata(vault) {
        Ok(m) => m,
        Err(_) => return Err(VaultError::Missing(vault.to_owned())),
    };
    if !meta.is_dir() {
        return Err(VaultError::NotADirectory(vault.to_owned()));
    }
    probe(vault)
        .map(|_| ())
        .map_err(|e| VaultError::NotWritable(vault.to_owned(), e))
}

/// What an export did to the note.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExportAction {
    /// A new note was written.
    Created,
    /// The existing note was rewritten (merged).
    Updated,
    /// The existing note already had exactly this content; nothing was written.
    Unchanged,
}

/// Result of [`export_note`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExportReport {
    /// Vault-relative path of the note.
    pub path: VaultPath,
    /// What happened.
    pub action: ExportAction,
    /// Time stored on the record.
    pub exported_unix_ms: u64,
}

fn join(vault: &Path, rel: &VaultPath) -> PathBuf {
    let mut p = vault.to_owned();
    p.extend(rel.components());
    p
}

/// The `earshot_id` of the note at `path`, from its first [`ID_SCAN_BYTES`].
fn note_id(path: &Path) -> Option<EntityId> {
    let mut buf = Vec::new();
    let f = File::open(path).ok()?;
    #[cfg(test)]
    tests::NOTE_OPENS.with(|n| n.set(n.get() + 1));
    f.take(ID_SCAN_BYTES).read_to_end(&mut buf).ok()?;
    earshot_id_of(&String::from_utf8_lossy(&buf))
}

fn is_markdown(p: &Path) -> bool {
    p.extension().is_some_and(|x| x.eq_ignore_ascii_case("md"))
}

/// Markdown files under `dir`, skipping hidden entries and symlinks, sorted.
fn markdown_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    let mut entries: Vec<_> = rd.filter_map(Result::ok).collect();
    entries.sort_by_key(std::fs::DirEntry::file_name);
    for e in entries {
        if e.file_name().to_string_lossy().starts_with('.') {
            continue;
        }
        let Ok(ty) = e.file_type() else { continue };
        if ty.is_dir() {
            markdown_files(&e.path(), out);
        } else if ty.is_file() && is_markdown(&e.path()) {
            out.push(e.path());
        }
    }
}

fn rel_of(vault: &Path, path: &Path) -> Option<VaultPath> {
    let rel = path.strip_prefix(vault).ok()?;
    let parts: Vec<String> = rel
        .components()
        .map(|c| c.as_os_str().to_str().map(str::to_owned))
        .collect::<Option<_>>()?;
    VaultPath::new(&parts.join("/")).ok()
}

/// `<stem>*.md` files beside the render's path (its collision variants), sorted.
fn siblings(vault: &Path, rendered: &VaultPath) -> Vec<PathBuf> {
    let full = join(vault, rendered);
    let (Some(dir), Some(stem)) = (full.parent(), full.file_stem()) else {
        return Vec::new();
    };
    let stem = stem.to_string_lossy().into_owned();
    let Ok(rd) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out: Vec<PathBuf> = rd
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_ok_and(|t| t.is_file()))
        .map(|e| e.path())
        .filter(|p| {
            is_markdown(p)
                && p.file_name()
                    .is_some_and(|n| n.to_string_lossy().starts_with(&stem))
        })
        .collect();
    out.sort();
    out
}

/// The vault-relative path of a note found on disk.
///
/// # Errors
/// [`StoreError::UnusableNotePath`] when the path cannot be expressed as a [`VaultPath`]:
/// the note exists, so creating another would duplicate it.
fn found_rel(vault: &Path, path: &Path) -> Result<VaultPath, StoreError> {
    rel_of(vault, path).ok_or_else(|| StoreError::UnusableNotePath(path.to_owned()))
}

/// The notes of a vault by `earshot_id`, built with one walk and one open per note, for
/// exporting many meetings at once ([`export_notes`]). Where several notes carry the same
/// id, the first in sorted path order is kept (the same note a single export's walk finds).
#[derive(Debug, Clone)]
pub(crate) struct VaultIndex {
    by_id: std::collections::BTreeMap<EntityId, PathBuf>,
    built: std::time::Instant,
    /// A miss rebuilds the index once it is this old ([`INDEX_MAX_AGE`]).
    max_age: std::time::Duration,
}

impl VaultIndex {
    /// Walks every non-hidden folder of `vault`.
    fn build(vault: &Path) -> Self {
        let mut all = Vec::new();
        markdown_files(vault, &mut all);
        let mut by_id = std::collections::BTreeMap::new();
        for p in all {
            if let Some(id) = note_id(&p) {
                by_id.entry(id).or_insert(p);
            }
        }
        VaultIndex {
            by_id,
            built: std::time::Instant::now(),
            max_age: INDEX_MAX_AGE,
        }
    }
}

/// How step 2 searches the vault after the cheap checks.
enum Search {
    /// Walk the vault now (single export).
    Walk,
    /// Use the batch's index, built on first need.
    Batch(Option<VaultIndex>),
}

/// Finds the meeting's note (see the module docs, step 2).
fn find_note(
    vault: &Path,
    meeting: EntityId,
    recorded: Option<&VaultPath>,
    rendered: &VaultPath,
    search: &mut Search,
    search_starts: &mut dyn FnMut(),
) -> Result<Option<VaultPath>, StoreError> {
    for rel in recorded.into_iter().chain(std::iter::once(rendered)) {
        if note_id(&join(vault, rel)) == Some(meeting) {
            return Ok(Some(rel.clone()));
        }
    }
    if let Some(p) = siblings(vault, rendered)
        .into_iter()
        .find(|p| note_id(p) == Some(meeting))
    {
        return found_rel(vault, &p).map(Some);
    }
    let walk = |vault: &Path| {
        let mut all = Vec::new();
        markdown_files(vault, &mut all);
        all.into_iter().find(|p| note_id(p) == Some(meeting))
    };
    let hit = match search {
        Search::Walk => {
            search_starts();
            walk(vault)
        }
        Search::Batch(index) => {
            let index = index.get_or_insert_with(|| {
                search_starts();
                VaultIndex::build(vault)
            });
            match index.by_id.get(&meeting) {
                // Still that meeting's note: use it.
                Some(p) if note_id(p) == Some(meeting) => Some(p.clone()),
                // Moved or edited since the index was built: walk, and remember the result.
                Some(_) => {
                    search_starts();
                    let hit = walk(vault);
                    match &hit {
                        Some(p) => index.by_id.insert(meeting, p.clone()),
                        None => index.by_id.remove(&meeting),
                    };
                    hit
                }
                // Not indexed: trust that only while the index is younger than its limit.
                None if index.built.elapsed() >= index.max_age => {
                    search_starts();
                    let max_age = index.max_age;
                    *index = VaultIndex::build(vault);
                    index.max_age = max_age;
                    index.by_id.get(&meeting).cloned()
                }
                None => None,
            }
        }
    };
    hit.map(|p| found_rel(vault, &p)).transpose()
}

/// The render's path, or `<stem> (n).md` for the first `n >= 2` that is free.
fn free_path(vault: &Path, wanted: &VaultPath) -> Result<VaultPath, StoreError> {
    if !join(vault, wanted).exists() {
        return Ok(wanted.clone());
    }
    let s = wanted.as_str();
    let stem = s.strip_suffix(".md").unwrap_or(s);
    for n in 2..10_000 {
        let candidate = VaultPath::new(&format!("{stem} ({n}).md"))
            .map_err(|e| StoreError::Corrupt(e.to_string()))?;
        if !join(vault, &candidate).exists() {
            return Ok(candidate);
        }
    }
    Err(StoreError::Corrupt(format!("no free file name for {s}")))
}

/// Whether a temp modified at `mtime` is stale at `now`: more than [`STALE_TEMP_MS`] in the
/// past, or as far in the future (a stepped clock or a copy from another device would
/// otherwise never age).
fn temp_is_stale(mtime: SystemTime, now: SystemTime) -> bool {
    let limit = std::time::Duration::from_millis(STALE_TEMP_MS);
    match now.duration_since(mtime) {
        Ok(age) => age >= limit,
        Err(ahead) => ahead.duration() >= limit,
    }
}

/// Next temp/probe sequence number of this process.
fn next_seq() -> u64 {
    TEMP_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

/// Prefix of the per-meeting export lease rows in `local_state` (module docs,
/// "Serialisation").
const LEASE_PREFIX: &str = "export-lease/";

/// The `local_state` key of `meeting`'s export lease.
pub(crate) fn lease_key(meeting: EntityId) -> String {
    format!("{LEASE_PREFIX}{meeting}")
}

/// How long an export waits for another export's lease on its meeting, how often it looks,
/// and when a lease counts as left by a crash.
#[derive(Debug, Clone, Copy)]
pub(crate) struct LeaseTiming {
    /// Longest wait for a lease held by someone else, then retryable `ConcurrentEdit`.
    pub(crate) wait: std::time::Duration,
    /// Pause between attempts.
    pub(crate) poll: std::time::Duration,
    /// A lease taken this long ago (by the export clock, either direction) is stale: its
    /// holder crashed or hung, and it is taken over.
    pub(crate) stale_ms: u64,
}

impl LeaseTiming {
    /// Production values: one export of one meeting takes milliseconds to seconds (a vault
    /// walk), so 30 s of waiting and 2 min to staleness leave wide margins.
    pub(crate) const DEFAULT: LeaseTiming = LeaseTiming {
        wait: std::time::Duration::from_secs(30),
        poll: std::time::Duration::from_millis(10),
        stale_ms: 2 * 60 * 1000,
    };
}

thread_local! {
    /// This thread's lease owner token (unique in the process).
    static LEASE_TOKEN: u64 = LEASE_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    /// Leases this thread holds, by meeting, with their nesting depth.
    static LEASES_HELD: std::cell::RefCell<std::collections::BTreeMap<EntityId, usize>> =
        const { std::cell::RefCell::new(std::collections::BTreeMap::new()) };
}

/// Source of [`LEASE_TOKEN`]s.
static LEASE_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

/// Who holds a lease: this process id and this thread's token.
fn lease_owner() -> [u8; 12] {
    let mut o = [0u8; 12];
    o[..4].copy_from_slice(&std::process::id().to_le_bytes());
    o[4..].copy_from_slice(&LEASE_TOKEN.with(|t| *t).to_le_bytes());
    o
}

/// The export lease this export holds (passed to the leased export so it can re-stamp it).
#[derive(Debug, Clone, Copy)]
struct ExportLease {
    meeting: EntityId,
    owner: [u8; 12],
}

impl ExportLease {
    /// Re-stamps the lease at `now_ms` in `tx` (the heartbeat, module docs
    /// "Serialisation"); `ConcurrentEdit` on `note` when the lease is no longer this
    /// owner's (it went stale and another export took it).
    fn restamp(self, tx: &StoreTx<'_>, now_ms: u64, note: &Path) -> Result<(), StoreError> {
        let mut value = self.owner.to_vec();
        value.extend_from_slice(&now_ms.to_le_bytes());
        let n = tx.conn.execute(
            "UPDATE local_state SET value = ?1 WHERE key = ?2 AND substr(value, 1, 12) = ?3",
            rusqlite::params![value, lease_key(self.meeting), self.owner.to_vec()],
        )?;
        if n == 1 {
            Ok(())
        } else {
            Err(StoreError::ConcurrentEdit(note.to_path_buf()))
        }
    }
}

impl Store {
    /// Runs `f` in one `IMMEDIATE` transaction (the write lock taken at the start, so
    /// what `f` reads cannot change before what it writes): committed if `f` returns `Ok`.
    fn immediate<T>(
        &mut self,
        f: impl FnOnce(&mut StoreTx<'_>) -> Result<T, StoreError>,
    ) -> Result<T, StoreError> {
        let tx = self
            .conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let mut tx = StoreTx { conn: tx };
        let out = f(&mut tx)?;
        tx.conn.commit()?;
        Ok(out)
    }

    /// Takes `meeting`'s export lease for `owner` at `now_ms` if it is free or stale;
    /// returns whether it was taken. One transaction, so two connections never both take it.
    fn try_take_export_lease(
        &mut self,
        meeting: EntityId,
        owner: [u8; 12],
        now_ms: u64,
        stale_ms: u64,
    ) -> Result<bool, StoreError> {
        use rusqlite::OptionalExtension;
        let key = lease_key(meeting);
        let tx = self
            .conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let held: Option<Vec<u8>> = tx
            .query_row(
                "SELECT value FROM local_state WHERE key = ?1",
                [&key],
                |r| r.get(0),
            )
            .optional()?;
        let free = match held {
            None => true,
            Some(v) if v.len() == 20 => {
                let since = u64::from_le_bytes(v[12..20].try_into().unwrap_or([0; 8]));
                v[..12] == owner || now_ms.abs_diff(since) >= stale_ms
            }
            // Unreadable: not a lease this build wrote; take it over.
            Some(_) => true,
        };
        if free {
            let mut value = owner.to_vec();
            value.extend_from_slice(&now_ms.to_le_bytes());
            tx.execute(
                "INSERT INTO local_state (key, value) VALUES (?1, ?2) \
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                rusqlite::params![key, value],
            )?;
        }
        tx.commit()?;
        Ok(free)
    }

    /// Releases `meeting`'s export lease if `owner` still holds it (a lease taken over
    /// after it went stale belongs to the new holder).
    fn release_export_lease(
        &mut self,
        meeting: EntityId,
        owner: [u8; 12],
    ) -> Result<(), StoreError> {
        self.conn.execute(
            "DELETE FROM local_state WHERE key = ?1 AND substr(value, 1, 12) = ?2",
            rusqlite::params![lease_key(meeting), owner.to_vec()],
        )?;
        Ok(())
    }
}

/// Takes `meeting`'s export lease (waiting as `timing` says), runs `f`, and releases it on
/// every outcome. A thread that already holds the lease nests (no production export calls
/// another, but tests run one export inside another's commit hook to force races).
fn with_export_lease<D: ExportDb + ?Sized, C: Clock + ?Sized, T>(
    store: &mut D,
    vault: &Path,
    meeting: EntityId,
    rendered: &RenderedExport,
    clock: &C,
    timing: LeaseTiming,
    f: impl FnOnce(&mut D, ExportLease) -> Result<T, StoreError>,
) -> Result<T, StoreError> {
    let nested = LEASES_HELD.with(|h| h.borrow().contains_key(&meeting));
    let owner = lease_owner();
    if !nested {
        let started = std::time::Instant::now();
        loop {
            let now = clock.now_unix_ms();
            if store
                .with_store(|s| s.try_take_export_lease(meeting, owner, now, timing.stale_ms))?
            {
                break;
            }
            if started.elapsed() >= timing.wait {
                return Err(StoreError::ConcurrentEdit(join(vault, &rendered.path)));
            }
            store.export_lease_busy();
            std::thread::sleep(timing.poll);
        }
    }
    // From here the thread-local depth is undone on every exit, a panic in `f` included,
    // so the meeting is never left marked as held by this thread. (A panic leaves the
    // database row, which goes stale.)
    let held = HeldLease::enter(meeting);
    let out = f(store, ExportLease { meeting, owner });
    let last = held.depth == 1;
    drop(held);
    if last {
        let released = store.with_store(|s| s.release_export_lease(meeting, owner));
        // The export's own error wins; a failed release leaves a lease that goes stale.
        let out = out?;
        released?;
        return Ok(out);
    }
    out
}

/// This thread's hold on a meeting's export lease, one per nesting level: entering
/// increments [`LEASES_HELD`], dropping decrements it (removing the meeting at zero).
struct HeldLease {
    meeting: EntityId,
    /// The depth after entering (1: the outermost hold).
    depth: usize,
}

impl HeldLease {
    fn enter(meeting: EntityId) -> Self {
        let depth = LEASES_HELD.with(|h| {
            let mut h = h.borrow_mut();
            let d = h.entry(meeting).or_insert(0);
            *d += 1;
            *d
        });
        HeldLease { meeting, depth }
    }
}

impl Drop for HeldLease {
    fn drop(&mut self) {
        LEASES_HELD.with(|h| {
            let mut h = h.borrow_mut();
            if let Some(d) = h.get_mut(&self.meeting) {
                *d -= 1;
                if *d == 0 {
                    h.remove(&self.meeting);
                }
            }
        });
    }
}

/// Creates a probe file in `dir` and returns the modification time the file system gave it
/// (creating a file stamps it; nothing is written). The probe is created exclusively as
/// `.earshot-probe-<pid>-<n>.earshot-tmp` (a name someone else holds is skipped, never
/// removed) and removed again on every outcome once this call created it. Named like a
/// temp, so a probe left by a crash is swept as a stale temp.
///
/// # Errors
/// The create error (the folder is not writable), or a metadata error.
fn probe(dir: &Path) -> std::io::Result<SystemTime> {
    probe_with(dir, &mut next_seq)
}

/// [`probe`] with the sequence numbers drawn from `seq` (tests).
fn probe_with(dir: &Path, seq: &mut dyn FnMut() -> u64) -> std::io::Result<SystemTime> {
    for _ in 0..TEMP_ATTEMPTS {
        let path = dir.join(format!(
            ".earshot-probe-{}-{}{TMP_SUFFIX}",
            std::process::id(),
            seq()
        ));
        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(f) => {
                // Ours from here: removed whatever happens next.
                drop(f);
                let t = std::fs::metadata(&path).and_then(|m| m.modified());
                let _ = std::fs::remove_file(&path);
                return t;
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e),
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::AlreadyExists,
        format!("no free probe name in {}", dir.display()),
    ))
}

/// The folder's own clock: the modification time of a [`probe`] created in `dir` now (on a
/// network share, the server's clock). `None` if no probe can be created.
fn dir_clock(dir: &Path) -> Option<SystemTime> {
    probe(dir).ok()
}

/// Removes `.*.earshot-tmp` files in `dir` that are stale ([`temp_is_stale`]) against the
/// reference time `now`.
fn clean_stray_temps_at(dir: &Path, now: SystemTime) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for e in rd.filter_map(Result::ok) {
        let name = e.file_name().to_string_lossy().into_owned();
        if !(name.starts_with('.') && name.ends_with(TMP_SUFFIX)) {
            continue;
        }
        let stale = e
            .metadata()
            .and_then(|m| m.modified())
            .is_ok_and(|t| temp_is_stale(t, now));
        if stale {
            let _ = std::fs::remove_file(e.path());
        }
    }
}

/// Temp-file state of one export or one batch: the folders already swept (each is swept
/// once), the clock that judges temp age (production: [`dir_clock`], the folder's own
/// clock) and the source of temp sequence numbers.
pub(crate) struct TempFiles {
    swept: std::collections::BTreeSet<PathBuf>,
    clock: fn(&Path) -> Option<SystemTime>,
    seq: Box<dyn FnMut() -> u64>,
}

impl TempFiles {
    /// Production settings: the folder clock and the process-wide sequence.
    fn new() -> Self {
        TempFiles {
            swept: std::collections::BTreeSet::new(),
            clock: dir_clock,
            seq: Box::new(next_seq),
        }
    }

    /// Removes stale temps in `dir`, once per `TempFiles`; nothing when the folder clock
    /// cannot be read.
    fn sweep(&mut self, dir: &Path) {
        if !self.swept.insert(dir.to_owned()) {
            return;
        }
        if let Some(now) = (self.clock)(dir) {
            clean_stray_temps_at(dir, now);
        }
    }

    /// Writes `content` to a synced hidden temp file beside `path`; returns its path. A
    /// name that already exists belongs to someone else (a reused pid after a crash, a temp
    /// synced from another device): it is skipped, never removed, and the next number is
    /// tried, at most [`TEMP_ATTEMPTS`] times. Only a temp this call created is removed on
    /// failure.
    fn write(&mut self, path: &Path, content: &[u8]) -> Result<PathBuf, StoreError> {
        let dir = path
            .parent()
            .ok_or_else(|| StoreError::Corrupt(format!("{} has no folder", path.display())))?;
        std::fs::create_dir_all(dir)?;
        self.sweep(dir);
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        for _ in 0..TEMP_ATTEMPTS {
            let seq = (self.seq)();
            let tmp = dir.join(format!(".{name}.{}-{seq}{TMP_SUFFIX}", std::process::id()));
            let mut f = match OpenOptions::new().write(true).create_new(true).open(&tmp) {
                Ok(f) => f,
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(e.into()),
            };
            // From here the temp is ours.
            if let Err(e) = f.write_all(content).and_then(|()| f.sync_all()) {
                drop(f);
                let _ = std::fs::remove_file(&tmp);
                return Err(e.into());
            }
            return Ok(tmp);
        }
        Err(StoreError::Io(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            format!("no free temp name for {}", path.display()),
        )))
    }
}

/// How an export reaches the store (module docs, "Store access").
pub trait ExportDb {
    /// Runs `f` with the store for one short read or write. Implementations that guard the
    /// store with a lock hold it for this call only.
    ///
    /// # Errors
    /// Whatever `f` returns.
    fn with_store<T>(
        &mut self,
        f: impl FnOnce(&mut Store) -> Result<T, StoreError>,
    ) -> Result<T, StoreError>;

    /// Called just before the export walks or indexes the vault, outside any
    /// [`ExportDb::with_store`] call. Default: nothing (tests use it to prove the walk runs
    /// without the caller's store lock).
    fn vault_search_starts(&mut self) {}

    /// Called, outside any [`ExportDb::with_store`] call, each time the export finds its
    /// meeting's lease held by another export and waits. Default: nothing (tests use it to
    /// prove the wait).
    fn export_lease_busy(&mut self) {}
}

impl ExportDb for Store {
    fn with_store<T>(
        &mut self,
        f: impl FnOnce(&mut Store) -> Result<T, StoreError>,
    ) -> Result<T, StoreError> {
        f(self)
    }
}

/// A temp file this export created exclusively ([`TempFiles::write`]): removed when the
/// export leaves without renaming it into place, on every early return (a concurrent
/// edit, a store error such as a retryable `Busy`, an I/O error), so no export leaves its
/// temp in the user's vault. Only a test hook's simulated crash
/// ([`OwnedTemp::crashed`]) leaves it, as a real crash would.
struct OwnedTemp {
    path: PathBuf,
    /// Renamed into place, or left by a simulated crash: not removed.
    keep: std::cell::Cell<bool>,
}

impl OwnedTemp {
    fn new(path: PathBuf) -> Self {
        Self {
            path,
            keep: std::cell::Cell::new(false),
        }
    }

    fn path(&self) -> &Path {
        &self.path
    }

    /// The temp became the note: nothing to remove.
    fn renamed(&self) {
        self.keep.set(true);
    }

    /// A test hook simulated a crash here: leave the temp as a crash would.
    fn crashed(&self) {
        self.keep.set(true);
    }
}

impl Drop for OwnedTemp {
    fn drop(&mut self) {
        if !self.keep.get() {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

/// Length and modification time: what "the note changed since it was read" compares.
fn stamp_of(path: &Path) -> std::io::Result<(u64, SystemTime)> {
    let m = std::fs::metadata(path)?;
    Ok((m.len(), m.modified()?))
}

/// Exports `rendered` (the note of `meeting`) into `vault` for `device`; see the module
/// docs for the steps.
///
/// # Errors
/// [`StoreError::Vault`] for an unusable vault; [`StoreError::Repo`] if the meeting does not
/// exist or is deleted (also when it is deleted while the export runs: no record is written);
/// [`StoreError::Export`] when the existing note's region markers are ambiguous;
/// [`StoreError::ConcurrentEdit`] (retryable) when the note changed while it was being
/// merged. In every error case the user's note is left as it was. I/O and SQLite errors.
pub fn export_note<D: ExportDb + ?Sized, C: Clock + ?Sized, R: Rng + ?Sized>(
    store: &mut D,
    vault: &Path,
    device: DeviceId,
    meeting: EntityId,
    rendered: &RenderedExport,
    clock: &C,
    rng: &mut R,
) -> Result<ExportReport, StoreError> {
    validate_vault(vault).map_err(StoreError::Vault)?;
    export_with_hook(
        store,
        vault,
        device,
        meeting,
        rendered,
        clock,
        rng,
        &mut |_| Ok(()),
        &mut Search::Walk,
        &mut TempFiles::new(),
    )
}

/// One meeting of an [`export_notes`] batch.
#[derive(Debug, Clone, Copy)]
pub struct ExportItem<'a> {
    /// The meeting.
    pub meeting: EntityId,
    /// Its rendered note.
    pub rendered: &'a RenderedExport,
}

/// Exports many meetings ("export all" / "export changed"): the vault is validated once and
/// searched through one shared vault index (see the module docs), built only if some
/// meeting's note is not at its recorded or rendered path; each folder's stale temps are
/// swept once for the whole batch. Each meeting gets its own result; one failure does not
/// stop the others.
///
/// # Errors
/// The outer error is [`StoreError::Vault`] for an unusable vault (nothing is exported);
/// per-meeting errors are as for [`export_note`].
pub fn export_notes<D: ExportDb + ?Sized, C: Clock + ?Sized, R: Rng + ?Sized>(
    store: &mut D,
    vault: &Path,
    device: DeviceId,
    items: &[ExportItem<'_>],
    clock: &C,
    rng: &mut R,
) -> Result<Vec<Result<ExportReport, StoreError>>, StoreError> {
    validate_vault(vault).map_err(StoreError::Vault)?;
    let mut search = Search::Batch(None);
    let mut temps = TempFiles::new();
    Ok(items
        .iter()
        .map(|it| {
            export_with_hook(
                store,
                vault,
                device,
                it.meeting,
                it.rendered,
                clock,
                rng,
                &mut |_| Ok(()),
                &mut search,
                &mut temps,
            )
        })
        .collect())
}

/// [`export_note`] with `before_commit` called with the target path just before the
/// final checks and the rename (tests use it to force races and crashes; an error from it
/// is returned as is, leaving the state a crash at that point would leave).
#[allow(clippy::too_many_arguments)]
fn export_with_hook<D: ExportDb + ?Sized, C: Clock + ?Sized, R: Rng + ?Sized>(
    store: &mut D,
    vault: &Path,
    device: DeviceId,
    meeting: EntityId,
    rendered: &RenderedExport,
    clock: &C,
    rng: &mut R,
    before_commit: &mut dyn FnMut(&Path) -> std::io::Result<()>,
    search: &mut Search,
    temps: &mut TempFiles,
) -> Result<ExportReport, StoreError> {
    export_with_timing(
        store,
        vault,
        device,
        meeting,
        rendered,
        clock,
        rng,
        before_commit,
        search,
        temps,
        LeaseTiming::DEFAULT,
    )
}

/// [`export_with_hook`] under the meeting's export lease with `timing` (module docs,
/// "Serialisation"): held from the record read through the search to the final record
/// write.
#[allow(clippy::too_many_arguments)]
fn export_with_timing<D: ExportDb + ?Sized, C: Clock + ?Sized, R: Rng + ?Sized>(
    store: &mut D,
    vault: &Path,
    device: DeviceId,
    meeting: EntityId,
    rendered: &RenderedExport,
    clock: &C,
    rng: &mut R,
    before_commit: &mut dyn FnMut(&Path) -> std::io::Result<()>,
    search: &mut Search,
    temps: &mut TempFiles,
    timing: LeaseTiming,
) -> Result<ExportReport, StoreError> {
    with_export_lease(
        store,
        vault,
        meeting,
        rendered,
        clock,
        timing,
        |store, lease| {
            export_leased(
                store,
                lease,
                vault,
                device,
                meeting,
                rendered,
                clock,
                rng,
                before_commit,
                search,
                temps,
            )
        },
    )
}

/// `MissingParent` unless `meeting` exists and is not a tombstone: a deleted meeting gets no
/// export record (its deletion removed them).
fn live_meeting(s: &impl MeetingRepo, meeting: EntityId) -> Result<(), StoreError> {
    match s.meeting(meeting)? {
        Some(m) if !m.meta.deleted => Ok(()),
        _ => Err(StoreError::Repo(RepoError::MissingParent {
            entity: "meeting",
            id: meeting,
        })),
    }
}

/// The export itself, with the meeting's lease held.
#[allow(clippy::too_many_arguments)]
fn export_leased<D: ExportDb + ?Sized, C: Clock + ?Sized, R: Rng + ?Sized>(
    store: &mut D,
    lease: ExportLease,
    vault: &Path,
    device: DeviceId,
    meeting: EntityId,
    rendered: &RenderedExport,
    clock: &C,
    rng: &mut R,
    before_commit: &mut dyn FnMut(&Path) -> std::io::Result<()>,
    search: &mut Search,
    temps: &mut TempFiles,
) -> Result<ExportReport, StoreError> {
    let record = store.with_store(|s| {
        live_meeting(s, meeting)?;
        Ok(s.export_record(meeting, device)?)
    })?;
    // The vault root holds the validation probes; reclaim any a crash left there.
    temps.sweep(vault);
    let new_id = record
        .as_ref()
        .map_or_else(|| EntityId::generate(clock, rng), |r| r.id);
    let found = find_note(
        vault,
        meeting,
        record.as_ref().map(|r| &r.path),
        &rendered.path,
        search,
        &mut || store.vault_search_starts(),
    )?;
    // Re-read in the same store access: a record another export wrote since keeps its id.
    let put_record = |store: &mut D, rel: &VaultPath| -> Result<u64, StoreError> {
        store.with_store(|s| {
            s.immediate(|tx| {
                // Deleted meanwhile (by another process: an export of this one holds the
                // meeting): no record for it. Checked before the lease, which a deletion
                // removes.
                live_meeting(tx, meeting)?;
                let now = clock.now_unix_ms();
                lease.restamp(tx, now, &join(vault, rel))?;
                let id = tx.export_record(meeting, device)?.map_or(new_id, |r| r.id);
                tx.put_export_record(ExportRecord {
                    id,
                    meeting_id: meeting,
                    device_id: device,
                    path: rel.clone(),
                    exported_unix_ms: now,
                })?;
                Ok(now)
            })
        })
    };

    let (rel, action) = match found {
        Some(rel) => {
            let path = join(vault, &rel);
            let before = stamp_of(&path)?;
            let existing = std::fs::read(&path)?;
            let text = String::from_utf8(existing.clone())
                .map_err(|_| StoreError::Corrupt(format!("{} is not UTF-8", path.display())))?;
            let merged = merge(&text, rendered).map_err(StoreError::Export)?;
            if merged.as_bytes() == existing.as_slice() {
                (rel, ExportAction::Unchanged)
            } else {
                let tmp = OwnedTemp::new(temps.write(&path, merged.as_bytes())?);
                before_commit(&path).inspect_err(|_| tmp.crashed())?;
                // Last look before replacing: a change since the read means a concurrent
                // edit (Obsidian, LiveSync) that the merge did not see.
                if stamp_of(&path).ok() != Some(before) || std::fs::read(&path)? != existing {
                    return Err(StoreError::ConcurrentEdit(path));
                }
                std::fs::rename(tmp.path(), &path)?;
                tmp.renamed();
                sync_dir(path.parent().unwrap_or(vault))?;
                (rel, ExportAction::Updated)
            }
        }
        None => {
            let content = rendered.to_file();
            let mut rel = free_path(vault, &rendered.path)?;
            let tmp = OwnedTemp::new(temps.write(&join(vault, &rel), content.as_bytes())?);
            let mut attempt = 0;
            // The record as this export last saw it: read before the search, then its own
            // claim.
            let mut seen = record.as_ref().map(|r| r.path.clone());
            loop {
                // Another export of this meeting (store access is not held across the
                // search) may have created its note since the search: it can only be the
                // render's path or one of its `<stem> (n).md` variants, and it is checked
                // after this attempt's name was chosen and before the record names it.
                // Never a second note for one earshot_id: retryable, a retry merges.
                if let Some(p) = siblings(vault, &rendered.path)
                    .into_iter()
                    .find(|p| note_id(p) == Some(meeting))
                {
                    return Err(StoreError::ConcurrentEdit(p));
                }
                // The record names the intended note before it exists, so a crash after the
                // rename leaves a recorded note, never an orphan. It is also this export's
                // claim: written only if nobody recorded another path since this export
                // last looked (another export, perhaps of a different render, created or is
                // creating the note), else retryable.
                // `Some(path now recorded)` when another export got there first.
                let conflict = store.with_store(|s| {
                    s.immediate(|tx| {
                        live_meeting(tx, meeting)?;
                        let at = clock.now_unix_ms();
                        lease.restamp(tx, at, &join(vault, &rel))?;
                        let now = tx.export_record(meeting, device)?;
                        if now.as_ref().map(|r| &r.path) != seen.as_ref() {
                            return Ok(Some(now.map(|r| r.path)));
                        }
                        let id = now.map_or(new_id, |r| r.id);
                        tx.put_export_record(ExportRecord {
                            id,
                            meeting_id: meeting,
                            device_id: device,
                            path: rel.clone(),
                            exported_unix_ms: at,
                        })?;
                        Ok(None)
                    })
                })?;
                if let Some(other) = conflict {
                    return Err(StoreError::ConcurrentEdit(match other {
                        Some(p) => join(vault, &p),
                        None => join(vault, &rel),
                    }));
                }
                seen = Some(rel.clone());
                let target = join(vault, &rel);
                before_commit(&target).inspect_err(|_| tmp.crashed())?;
                // Last look before the note appears: an export that claimed after this one
                // (it read this claim as the record) holds the record now.
                let current = store.with_store(|s| {
                    s.immediate(|tx| {
                        lease.restamp(tx, clock.now_unix_ms(), &target)?;
                        Ok(tx.export_record(meeting, device)?)
                    })
                })?;
                if current.as_ref().map(|r| &r.path) != Some(&rel) {
                    return Err(StoreError::ConcurrentEdit(
                        current.map_or_else(|| target.clone(), |r| join(vault, &r.path)),
                    ));
                }
                match rename_noreplace(tmp.path(), &target) {
                    Ok(()) => {
                        tmp.renamed();
                        break;
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                        attempt += 1;
                        if attempt >= CREATE_ATTEMPTS {
                            return Err(e.into());
                        }
                        rel = free_path(vault, &rendered.path)?;
                    }
                    Err(e) => return Err(e.into()),
                }
            }
            sync_dir(join(vault, &rel).parent().unwrap_or(vault))?;
            (rel, ExportAction::Created)
        }
    };

    if let Search::Batch(Some(index)) = search {
        // The vault view changed under the index: remember this meeting's note.
        index.by_id.insert(meeting, join(vault, &rel));
    }
    let now = put_record(store, &rel)?;
    Ok(ExportReport {
        path: rel,
        action,
        exported_unix_ms: now,
    })
}

#[cfg(test)]
mod tests {
    use earshot_core::export::{ExportInput, ExportOptions, render};
    use earshot_core::identity::{Hlc, ManualClock};
    use earshot_core::model::{Meeting, Stamp};
    use earshot_core::prompt::Template;
    use rand_chacha::ChaCha8Rng;
    use rand_core::SeedableRng;

    use super::*;

    const DEVICE: DeviceId = DeviceId::from_bytes([0xd5; 16]);

    thread_local! {
        /// Notes opened by `note_id` on this test's thread.
        pub(super) static NOTE_OPENS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    }

    struct W {
        vault: PathBuf,
        store: Store,
        clock: ManualClock,
        rng: ChaCha8Rng,
        meeting: Meeting,
    }

    impl W {
        fn new(tag: &str) -> W {
            let vault = std::env::temp_dir()
                .join(format!("earshot-export-race-{tag}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&vault);
            std::fs::create_dir_all(&vault).unwrap();
            let clock = ManualClock::new(1_792_209_900_000);
            let mut rng = ChaCha8Rng::seed_from_u64(1);
            let mut hlc = Hlc::new(DEVICE);
            let meeting = Meeting::new(
                "Race",
                1_792_209_900_000,
                0,
                Template::General,
                &mut Stamp {
                    clock: &clock,
                    hlc: &mut hlc,
                    rng: &mut rng,
                },
            );
            let mut store = Store::open_in_memory().unwrap();
            store.put_meeting(meeting.clone()).unwrap();
            W {
                vault,
                store,
                clock,
                rng,
                meeting,
            }
        }

        fn render(&self, title: &str) -> RenderedExport {
            let mut m = self.meeting.clone();
            m.tags = vec![title.to_owned()];
            render(
                &ExportInput {
                    transcript: None,
                    meeting: &m,
                    attendees: &[],
                    speakers: &[],
                    segments: &[],
                    notes: &[],
                    summary: None,
                },
                &ExportOptions::default(),
            )
            .unwrap()
        }

        fn export(
            &mut self,
            r: &RenderedExport,
            hook: &mut dyn FnMut(&Path) -> std::io::Result<()>,
        ) -> Result<ExportReport, StoreError> {
            self.clock.advance_ms(1000);
            export_with_hook(
                &mut self.store,
                &self.vault,
                DEVICE,
                self.meeting.id,
                r,
                &self.clock,
                &mut self.rng,
                hook,
                &mut Search::Walk,
                &mut TempFiles::new(),
            )
        }

        fn md_count(&self) -> usize {
            let mut all = Vec::new();
            markdown_files(&self.vault, &mut all);
            all.len()
        }
    }

    impl Drop for W {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.vault);
        }
    }

    fn no_hook(_: &Path) -> std::io::Result<()> {
        Ok(())
    }

    #[test]
    fn export_aborts_when_the_note_is_edited_during_the_merge() {
        let mut w = W::new("edit");
        let r1 = w.render("one");
        let first = w.export(&r1, &mut no_hook).unwrap();
        let path = join(&w.vault, &first.path);
        // Obsidian saves the user's typing between our read and our rename.
        let mut hook = |p: &Path| {
            let mut s = std::fs::read_to_string(p)?;
            s.push_str("typed during export\n");
            std::fs::write(p, s)
        };
        let err = w.export(&w.render("two"), &mut hook).unwrap_err();
        assert!(matches!(err, StoreError::ConcurrentEdit(_)), "{err}");
        assert!(err.is_retryable());
        let now = std::fs::read_to_string(&path).unwrap();
        assert!(
            now.ends_with("typed during export\n"),
            "the user's edit is not clobbered"
        );
        assert!(!now.contains("  - two\n"));
        assert_eq!(
            std::fs::read_dir(path.parent().unwrap()).unwrap().count(),
            1,
            "temp removed"
        );
        // A retry merges on top of the edit.
        let rep = w.export(&w.render("two"), &mut no_hook).unwrap();
        assert_eq!(rep.action, ExportAction::Updated);
        let now = std::fs::read_to_string(&path).unwrap();
        assert!(now.contains("  - two\n") && now.ends_with("typed during export\n"));
    }

    #[test]
    fn export_aborts_on_a_same_length_edit_with_a_new_mtime() {
        let mut w = W::new("mtime");
        let first = w.export(&w.render("one"), &mut no_hook).unwrap();
        let path = join(&w.vault, &first.path);
        let mut hook = |p: &Path| {
            let s = std::fs::read_to_string(p)?.replace("source: earshot", "source: EARSHOT");
            std::fs::write(p, &s)?;
            let f = OpenOptions::new().write(true).open(p)?;
            f.set_modified(SystemTime::now() + std::time::Duration::from_secs(5))
        };
        let err = w.export(&w.render("two"), &mut hook).unwrap_err();
        assert!(matches!(err, StoreError::ConcurrentEdit(_)), "{err}");
        assert!(
            std::fs::read_to_string(&path)
                .unwrap()
                .contains("source: EARSHOT")
        );
    }

    #[test]
    fn export_create_never_overwrites_a_note_that_appears_at_the_name() {
        let mut w = W::new("late");
        let r = w.render("one");
        let foreign = "---\ntitle: theirs\n---\nCreated by a sync client.\n";
        let mut fired = false;
        let mut hook = |p: &Path| {
            if !fired {
                fired = true;
                std::fs::write(p, foreign)?;
            }
            Ok(())
        };
        let rep = w.export(&r, &mut hook).unwrap();
        assert_eq!(rep.action, ExportAction::Created);
        assert!(rep.path.as_str().ends_with(" (2).md"), "{}", rep.path);
        assert_eq!(
            std::fs::read_to_string(join(&w.vault, &r.path)).unwrap(),
            foreign
        );
        assert_eq!(
            std::fs::read_to_string(join(&w.vault, &rep.path)).unwrap(),
            r.to_file()
        );
        let rec = w
            .store
            .export_record(w.meeting.id, DEVICE)
            .unwrap()
            .unwrap();
        assert_eq!(rec.path, rep.path, "record follows the name actually used");
    }

    #[test]
    fn export_crash_before_the_rename_leaves_a_record_and_recovers() {
        let mut w = W::new("crash");
        let r = w.render("one");
        let mut crash = |_: &Path| Err(std::io::Error::other("simulated crash"));
        assert!(w.export(&r, &mut crash).is_err());
        // The intended path is recorded, the note does not exist, a temp file is left.
        let rec = w
            .store
            .export_record(w.meeting.id, DEVICE)
            .unwrap()
            .unwrap();
        assert_eq!(rec.path, r.path);
        assert_eq!(w.md_count(), 0);
        // The crashed export's temp is left; once it is stale, the next export removes it.
        let dir = join(&w.vault, &r.path).parent().unwrap().to_owned();
        for e in std::fs::read_dir(&dir).unwrap() {
            let f = OpenOptions::new()
                .write(true)
                .open(e.unwrap().path())
                .unwrap();
            f.set_modified(
                SystemTime::now() - std::time::Duration::from_millis(STALE_TEMP_MS + 1000),
            )
            .unwrap();
        }
        let rep = w.export(&r, &mut no_hook).unwrap();
        assert_eq!((rep.action, &rep.path), (ExportAction::Created, &r.path));
        let dir = join(&w.vault, &r.path).parent().unwrap().to_owned();
        let names: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            names,
            vec![r.path.file_name().to_owned()],
            "stray temp cleaned"
        );
        assert_eq!(
            rec.id,
            w.store
                .export_record(w.meeting.id, DEVICE)
                .unwrap()
                .unwrap()
                .id
        );
    }

    #[test]
    fn export_keeps_a_young_temp_of_another_export() {
        let mut w = W::new("youngtmp");
        let r = w.render("one");
        let dir = join(&w.vault, &r.path).parent().unwrap().to_owned();
        std::fs::create_dir_all(&dir).unwrap();
        let live = dir.join(".Other note.md.earshot-tmp");
        std::fs::write(&live, "in flight").unwrap();
        let old = dir.join(".Crashed.md.earshot-tmp");
        std::fs::write(&old, "stale").unwrap();
        let f = OpenOptions::new().write(true).open(&old).unwrap();
        f.set_modified(SystemTime::now() - std::time::Duration::from_millis(STALE_TEMP_MS + 1000))
            .unwrap();
        drop(f);
        w.export(&r, &mut no_hook).unwrap();
        assert!(live.exists(), "a live temp is never removed");
        assert!(!old.exists(), "a stale temp is removed");
    }

    #[test]
    fn export_without_a_record_adopts_a_moved_note_anywhere_in_the_vault() {
        let mut w = W::new("farnote");
        let r = w.render("one");
        std::fs::create_dir_all(w.vault.join("Far").join("Away")).unwrap();
        std::fs::write(w.vault.join("Far").join("Away").join("x.md"), r.to_file()).unwrap();
        let rep = w.export(&r, &mut no_hook).unwrap();
        assert_eq!(rep.action, ExportAction::Unchanged);
        assert_eq!(rep.path.as_str(), "Far/Away/x.md");
        assert_eq!(w.md_count(), 1, "never duplicated");
        let rec = w
            .store
            .export_record(w.meeting.id, DEVICE)
            .unwrap()
            .unwrap();
        assert_eq!(rec.path, rep.path);
    }

    #[test]
    fn export_without_a_record_prefers_the_note_beside_the_target() {
        let mut w = W::new("norecord");
        let r = w.render("one");
        // Copies in two places: the one beside the target wins over the vault walk.
        std::fs::create_dir_all(w.vault.join("Far")).unwrap();
        std::fs::write(w.vault.join("Far").join("x.md"), r.to_file()).unwrap();
        let target = join(&w.vault, &r.path);
        std::fs::create_dir_all(target.parent().unwrap()).unwrap();
        std::fs::write(&target, "---\ntitle: theirs\n---\n").unwrap();
        let stem = target.file_stem().unwrap().to_string_lossy().into_owned();
        let sibling = target.with_file_name(format!("{stem} (2).md"));
        std::fs::write(&sibling, r.to_file()).unwrap();
        let rep = w.export(&r, &mut no_hook).unwrap();
        assert_eq!(rep.action, ExportAction::Unchanged);
        assert!(rep.path.as_str().ends_with(" (2).md"));
    }

    // ---- T5.4c ----

    fn render_for(m: &Meeting) -> RenderedExport {
        render(
            &ExportInput {
                transcript: None,
                meeting: m,
                attendees: &[],
                speakers: &[],
                segments: &[],
                notes: &[],
                summary: None,
            },
            &ExportOptions::default(),
        )
        .unwrap()
    }

    fn opens() -> usize {
        NOTE_OPENS.with(std::cell::Cell::get)
    }

    #[test]
    fn export_batch_walks_the_vault_once() {
        let mut w = W::new("batch");
        // 30 unrelated notes elsewhere in the vault.
        let other = w.vault.join("Other");
        std::fs::create_dir_all(&other).unwrap();
        let foreign = w.render("x").to_file().replace(
            &w.meeting.id.to_string(),
            "01920000-0000-7000-8000-00000000ffff",
        );
        for i in 0..30 {
            std::fs::write(other.join(format!("n{i:02}.md")), &foreign).unwrap();
        }
        // Six meetings, none exported yet; the last one's note was moved by the user.
        let mut items = Vec::new();
        for i in 0..6u64 {
            let mut hlc = Hlc::new(DEVICE);
            let m = Meeting::new(
                format!("Batch {i}"),
                1_792_209_900_000 + i * 3_600_000,
                0,
                Template::General,
                &mut Stamp {
                    clock: &w.clock,
                    hlc: &mut hlc,
                    rng: &mut w.rng,
                },
            );
            w.store.put_meeting(m.clone()).unwrap();
            items.push((m.id, render_for(&m)));
        }
        let moved = w.vault.join("Archive").join("kept.md");
        std::fs::create_dir_all(moved.parent().unwrap()).unwrap();
        std::fs::write(&moved, items[5].1.to_file()).unwrap();
        let batch: Vec<ExportItem<'_>> = items
            .iter()
            .map(|(id, r)| ExportItem {
                meeting: *id,
                rendered: r,
            })
            .collect();

        NOTE_OPENS.with(|n| n.set(0));
        let out =
            export_notes(&mut w.store, &w.vault, DEVICE, &batch, &w.clock, &mut w.rng).unwrap();
        let reports: Vec<ExportReport> = out.into_iter().map(Result::unwrap).collect();
        assert_eq!(
            reports[5].path.as_str(),
            "Archive/kept.md",
            "moved note adopted"
        );
        assert_eq!(reports[5].action, ExportAction::Unchanged);
        assert!(
            reports[..5]
                .iter()
                .all(|r| r.action == ExportAction::Created)
        );
        // One walk (31 notes) plus a re-check of the indexed hit; a walk per meeting would
        // open about 6 x 31 notes.
        assert!(opens() <= 31 + 6, "{} note opens for a batch of 6", opens());
    }

    #[test]
    fn export_single_still_finds_a_moved_note_by_walking() {
        let mut w = W::new("single-walk");
        let r = w.render("one");
        let moved = w.vault.join("A").join("B").join("c.md");
        std::fs::create_dir_all(moved.parent().unwrap()).unwrap();
        std::fs::write(&moved, r.to_file()).unwrap();
        let rep = w.export(&r, &mut no_hook).unwrap();
        assert_eq!(rep.path.as_str(), "A/B/c.md");
    }

    /// A file name the platform allows but a `VaultPath` cannot hold.
    fn unusable_name() -> std::ffi::OsString {
        #[cfg(windows)]
        {
            use std::os::windows::ffi::OsStringExt;
            // An unpaired surrogate: valid on NTFS, not valid Unicode.
            let mut v: Vec<u16> = "note-".encode_utf16().collect();
            v.push(0xD800);
            v.extend(".md".encode_utf16());
            std::ffi::OsString::from_wide(&v)
        }
        #[cfg(not(windows))]
        {
            use std::os::unix::ffi::OsStringExt;
            std::ffi::OsString::from_vec(b"note-\xff.md".to_vec())
        }
    }

    #[test]
    fn export_a_found_note_at_an_unusable_path_is_an_error_not_a_duplicate() {
        let mut w = W::new("unusable");
        let r = w.render("one");
        let dir = w.vault.join("Odd");
        std::fs::create_dir_all(&dir).unwrap();
        let odd = dir.join(unusable_name());
        std::fs::write(&odd, r.to_file()).unwrap();
        let err = w.export(&r, &mut no_hook).unwrap_err();
        assert!(
            matches!(err, StoreError::UnusableNotePath(ref p) if *p == odd),
            "{err}"
        );
        assert_eq!(w.md_count(), 1, "no second note was created");
        assert_eq!(w.store.export_record(w.meeting.id, DEVICE).unwrap(), None);
    }

    #[test]
    fn export_temps_dated_in_the_future_are_stale_too() {
        let now = SystemTime::now();
        let min = std::time::Duration::from_secs(60);
        assert!(
            !temp_is_stale(now - min, now),
            "a 1-minute-old temp is live"
        );
        assert!(!temp_is_stale(now + min, now), "small skew is tolerated");
        assert!(temp_is_stale(now - 6 * min, now));
        assert!(
            temp_is_stale(now + 60 * min, now),
            "a future temp would never age"
        );

        let mut w = W::new("futuretmp");
        let r = w.render("one");
        let dir = join(&w.vault, &r.path).parent().unwrap().to_owned();
        std::fs::create_dir_all(&dir).unwrap();
        let future = dir.join(".Synced copy.md.1-0.earshot-tmp");
        std::fs::write(&future, "x").unwrap();
        let f = OpenOptions::new().write(true).open(&future).unwrap();
        f.set_modified(now + 60 * min).unwrap();
        drop(f);
        w.export(&r, &mut no_hook).unwrap();
        assert!(!future.exists(), "a temp dated an hour ahead is removed");
    }

    #[test]
    fn export_concurrent_temps_of_the_same_note_do_not_clobber_each_other() {
        let w = W::new("sametmp");
        let target = w.vault.join("Meetings").join("same.md");
        let a = TempFiles::new().write(&target, b"export A").unwrap();
        let b = TempFiles::new().write(&target, b"export B").unwrap();
        assert_ne!(a, b, "each export has its own temp");
        assert_eq!(std::fs::read(&a).unwrap(), b"export A", "A's temp intact");
        assert_eq!(std::fs::read(&b).unwrap(), b"export B");
        for t in [&a, &b] {
            let name = t.file_name().unwrap().to_string_lossy().into_owned();
            assert!(
                name.starts_with(".same.md.") && name.ends_with(TMP_SUFFIX),
                "{name}"
            );
        }
    }

    // ---- T5.4d ----

    const HOUR: std::time::Duration = std::time::Duration::from_secs(3_600);

    thread_local! {
        /// What `fake_clock` returns on this test's thread.
        static FAKE_NOW: std::cell::Cell<Option<SystemTime>> = const { std::cell::Cell::new(None) };
        /// How often `fake_clock` was asked on this test's thread.
        static CLOCK_CALLS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    }

    fn fake_clock(_: &Path) -> Option<SystemTime> {
        CLOCK_CALLS.with(|c| c.set(c.get() + 1));
        FAKE_NOW.with(std::cell::Cell::get)
    }

    /// Test temp-file state: an injected clock and fixed sequence numbers.
    fn temps_with(clock: fn(&Path) -> Option<SystemTime>, seqs: Vec<u64>) -> TempFiles {
        let mut it = seqs.into_iter();
        TempFiles {
            swept: std::collections::BTreeSet::new(),
            clock,
            seq: Box::new(move || it.next().expect("enough sequence numbers")),
        }
    }

    fn stamp_file(path: &Path, t: SystemTime) {
        let f = OpenOptions::new().write(true).open(path).unwrap();
        f.set_modified(t).unwrap();
    }

    impl W {
        fn export_with(
            &mut self,
            r: &RenderedExport,
            temps: &mut TempFiles,
        ) -> Result<ExportReport, StoreError> {
            self.clock.advance_ms(1000);
            export_with_hook(
                &mut self.store,
                &self.vault,
                DEVICE,
                self.meeting.id,
                r,
                &self.clock,
                &mut self.rng,
                &mut no_hook,
                &mut Search::Walk,
                temps,
            )
        }
    }

    #[test]
    fn export_a_taken_temp_name_is_skipped_never_removed() {
        let w = W::new("taken-temp");
        let dir = w.vault.join("Meetings");
        std::fs::create_dir_all(&dir).unwrap();
        let target = dir.join("note.md");
        // Another process's young temp holds the name this write draws first.
        let theirs = dir.join(format!(".note.md.{}-7{TMP_SUFFIX}", std::process::id()));
        std::fs::write(&theirs, "theirs").unwrap();
        let mut temps = temps_with(|_| None, vec![7, 8]);
        let ours = temps.write(&target, b"ours").unwrap();
        assert_eq!(
            ours,
            dir.join(format!(".note.md.{}-8{TMP_SUFFIX}", std::process::id()))
        );
        assert_eq!(std::fs::read(&ours).unwrap(), b"ours");
        assert_eq!(std::fs::read_to_string(&theirs).unwrap(), "theirs");
        // Every name taken: an error, and still nothing removed.
        let mut all_taken = temps_with(|_| None, vec![7; TEMP_ATTEMPTS]);
        assert!(all_taken.write(&target, b"x").is_err());
        assert_eq!(std::fs::read_to_string(&theirs).unwrap(), "theirs");
    }

    #[test]
    fn export_skips_another_process_temp_at_the_name_it_draws() {
        let mut w = W::new("young-temp-export");
        let r = w.render("one");
        let dir = join(&w.vault, &r.path).parent().unwrap().to_owned();
        std::fs::create_dir_all(&dir).unwrap();
        let name = r.path.file_name();
        // A young temp at exactly the name the export draws first (same pid, seq 5).
        let theirs = dir.join(format!(".{name}.{}-5{TMP_SUFFIX}", std::process::id()));
        std::fs::write(&theirs, "in flight elsewhere").unwrap();
        let mut temps = temps_with(dir_clock, (5..50).collect());
        let rep = w.export_with(&r, &mut temps).unwrap();
        assert_eq!(rep.action, ExportAction::Created);
        assert_eq!(
            std::fs::read_to_string(join(&w.vault, &rep.path)).unwrap(),
            r.to_file()
        );
        assert_eq!(
            std::fs::read_to_string(&theirs).unwrap(),
            "in flight elsewhere",
            "the other temp is neither truncated nor removed"
        );
    }

    #[test]
    fn export_sweep_judges_temps_by_the_injected_folder_clock() {
        // Production wiring: the folder clock, not the local clock.
        assert!(std::ptr::fn_addr_eq(
            TempFiles::new().clock,
            dir_clock as fn(&Path) -> Option<SystemTime>
        ));

        // (i) A share whose clock runs an hour ahead: another process's live temp is stamped
        // an hour ahead of the local clock. Judged by the share's clock it is young.
        let mut w = W::new("sweep-clock");
        let r = w.render("one");
        let dir = join(&w.vault, &r.path).parent().unwrap().to_owned();
        std::fs::create_dir_all(&dir).unwrap();
        let share_now = SystemTime::now() + HOUR;
        let live = dir.join(format!(".other.md.1-0{TMP_SUFFIX}"));
        std::fs::write(&live, "live").unwrap();
        stamp_file(&live, share_now);
        assert!(
            temp_is_stale(share_now, SystemTime::now()),
            "local clock: stale"
        );
        FAKE_NOW.with(|c| c.set(Some(share_now)));
        w.export_with(&r, &mut temps_with(fake_clock, (0..50).collect()))
            .unwrap();
        assert!(live.exists(), "a live temp on a fast share survives");

        // (ii) No folder clock: nothing is cleaned, not even an obviously old temp.
        let mut w = W::new("sweep-none");
        let r = w.render("one");
        let dir = join(&w.vault, &r.path).parent().unwrap().to_owned();
        std::fs::create_dir_all(&dir).unwrap();
        let old = dir.join(format!(".crashed.md.1-0{TMP_SUFFIX}"));
        std::fs::write(&old, "old").unwrap();
        stamp_file(&old, SystemTime::now() - HOUR);
        FAKE_NOW.with(|c| c.set(None));
        w.export_with(&r, &mut temps_with(fake_clock, (0..50).collect()))
            .unwrap();
        assert!(old.exists(), "no probe, no cleaning");

        // (iii) With a clock, the same stale temp goes.
        let mut w2 = W::new("sweep-now");
        let r2 = w2.render("one");
        let dir2 = join(&w2.vault, &r2.path).parent().unwrap().to_owned();
        std::fs::create_dir_all(&dir2).unwrap();
        let old2 = dir2.join(format!(".crashed.md.1-0{TMP_SUFFIX}"));
        std::fs::write(&old2, "old").unwrap();
        stamp_file(&old2, SystemTime::now() - HOUR);
        FAKE_NOW.with(|c| c.set(Some(SystemTime::now())));
        w2.export_with(&r2, &mut temps_with(fake_clock, (0..50).collect()))
            .unwrap();
        assert!(!old2.exists());
    }

    #[test]
    fn export_sweeps_each_folder_once_per_batch() {
        let mut w = W::new("sweep-once");
        let first = w.render("one");
        let mut hlc = earshot_core::identity::Hlc::new(DEVICE);
        let other = Meeting::new(
            "Second",
            1_792_209_900_000 + 3_600_000,
            0,
            Template::General,
            &mut Stamp {
                clock: &w.clock,
                hlc: &mut hlc,
                rng: &mut w.rng,
            },
        );
        w.store.put_meeting(other.clone()).unwrap();
        let second = render_for(&other);
        FAKE_NOW.with(|c| c.set(Some(SystemTime::now())));
        CLOCK_CALLS.with(|c| c.set(0));
        let mut temps = temps_with(fake_clock, (0..50).collect());
        w.export_with(&first, &mut temps).unwrap();
        let mut search = Search::Walk;
        export_with_hook(
            &mut w.store,
            &w.vault,
            DEVICE,
            other.id,
            &second,
            &w.clock,
            &mut w.rng,
            &mut no_hook,
            &mut search,
            &mut temps,
        )
        .unwrap();
        // Vault root once, the Meetings folder once: two clock reads for two notes.
        assert_eq!(CLOCK_CALLS.with(std::cell::Cell::get), 2);
    }

    #[test]
    fn export_probe_removes_only_what_it_created() {
        let w = W::new("probe");
        let dir = w.vault.join("P");
        std::fs::create_dir_all(&dir).unwrap();
        let theirs = dir.join(format!(
            ".earshot-probe-{}-3{TMP_SUFFIX}",
            std::process::id()
        ));
        std::fs::write(&theirs, "theirs").unwrap();
        let mut seqs = [3u64, 4].into_iter();
        probe_with(&dir, &mut || seqs.next().unwrap()).unwrap();
        let names: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            names,
            vec![theirs.file_name().unwrap().to_string_lossy().into_owned()]
        );
        assert_eq!(std::fs::read_to_string(&theirs).unwrap(), "theirs");
        // Every name taken: an error, nothing removed.
        assert!(probe_with(&dir, &mut || 3).is_err());
        assert!(theirs.exists());
        // The folder clock leaves no probe behind.
        let empty = w.vault.join("Q");
        std::fs::create_dir_all(&empty).unwrap();
        assert!(dir_clock(&empty).is_some());
        assert_eq!(std::fs::read_dir(&empty).unwrap().count(), 0);
    }

    #[test]
    fn export_reclaims_a_probe_left_by_a_crash() {
        let mut w = W::new("probe-crash");
        let r = w.render("one");
        // validate_vault's probe and a folder-clock probe, each left by a crash an hour ago.
        let in_root = w.vault.join(format!(".earshot-probe-1-1{TMP_SUFFIX}"));
        let dir = join(&w.vault, &r.path).parent().unwrap().to_owned();
        std::fs::create_dir_all(&dir).unwrap();
        let in_folder = dir.join(format!(".earshot-probe-1-2{TMP_SUFFIX}"));
        for p in [&in_root, &in_folder] {
            std::fs::write(p, "").unwrap();
            stamp_file(p, SystemTime::now() - HOUR);
        }
        w.export(&r, &mut no_hook).unwrap();
        assert!(
            !in_root.exists() && !in_folder.exists(),
            "stale probes are swept"
        );
    }

    #[test]
    fn export_temp_age_rule_and_local_folder_clock() {
        let w = W::new("dirclock");
        let dir = w.vault.join("Share");
        std::fs::create_dir_all(&dir).unwrap();
        // On a local disk the folder clock is the local clock, and the probe is removed.
        let probe_t = dir_clock(&dir).unwrap();
        let skew = probe_t
            .duration_since(SystemTime::now())
            .unwrap_or_else(|e| e.duration());
        assert!(skew < std::time::Duration::from_secs(5), "{skew:?}");
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 0);
        // The age rule against a reference time, both directions.
        let share_now = SystemTime::now() + HOUR;
        let live = dir.join(format!(".a.md.1-0{TMP_SUFFIX}"));
        std::fs::write(&live, "x").unwrap();
        stamp_file(&live, share_now);
        clean_stray_temps_at(&dir, share_now);
        assert!(live.exists());
        clean_stray_temps_at(&dir, share_now + 2 * HOUR);
        assert!(!live.exists());
    }

    #[test]
    fn export_batch_index_learns_its_own_writes_and_walks() {
        let mut w = W::new("index-learn");
        let r = w.render("one");
        let mut search = Search::Batch(None);
        let rep = export_with_hook(
            &mut w.store,
            &w.vault,
            DEVICE,
            w.meeting.id,
            &r,
            &w.clock,
            &mut w.rng,
            &mut no_hook,
            &mut search,
            &mut TempFiles::new(),
        )
        .unwrap();
        let Search::Batch(Some(index)) = &search else {
            panic!("index built")
        };
        assert_eq!(
            index.by_id.get(&w.meeting.id),
            Some(&join(&w.vault, &rep.path))
        );
        // The user moves it; the stale entry falls back to a walk whose result is kept.
        let moved = w.vault.join("Moved").join("m.md");
        std::fs::create_dir_all(moved.parent().unwrap()).unwrap();
        std::fs::rename(join(&w.vault, &rep.path), &moved).unwrap();
        let found = find_note(
            &w.vault,
            w.meeting.id,
            None,
            &r.path,
            &mut search,
            &mut || {},
        )
        .unwrap();
        assert_eq!(found.unwrap().as_str(), "Moved/m.md");
        let Search::Batch(Some(index)) = &search else {
            panic!("index kept")
        };
        assert_eq!(index.by_id.get(&w.meeting.id), Some(&moved));
    }

    #[test]
    fn export_batch_index_is_rebuilt_when_an_old_index_misses() {
        let w = W::new("index-age");
        let r = w.render("one");
        // The index was built before this meeting's note arrived in the vault.
        let mut index = VaultIndex::build(&w.vault);
        assert_eq!(index.max_age, INDEX_MAX_AGE);
        let far = w.vault.join("Synced").join("arrived.md");
        std::fs::create_dir_all(far.parent().unwrap()).unwrap();
        std::fs::write(&far, r.to_file()).unwrap();
        // Younger than its limit: the documented race, not seen.
        let mut fresh = Search::Batch(Some(index.clone()));
        assert_eq!(
            find_note(
                &w.vault,
                w.meeting.id,
                None,
                &r.path,
                &mut fresh,
                &mut || {}
            )
            .unwrap(),
            None
        );
        // At its limit (here: zero): rebuilt once and found.
        index.max_age = std::time::Duration::ZERO;
        let mut old = Search::Batch(Some(index));
        let found = find_note(&w.vault, w.meeting.id, None, &r.path, &mut old, &mut || {}).unwrap();
        assert_eq!(found.unwrap().as_str(), "Synced/arrived.md");
    }

    /// An [`ExportDb`] that logs each store access and, when the vault search starts,
    /// writes a record for the meeting as a concurrent export would.
    struct Logged<'a> {
        store: &'a mut Store,
        log: Vec<&'static str>,
        concurrent: Option<ExportRecord>,
    }

    impl ExportDb for Logged<'_> {
        fn with_store<T>(
            &mut self,
            f: impl FnOnce(&mut Store) -> Result<T, StoreError>,
        ) -> Result<T, StoreError> {
            self.log.push("store");
            f(self.store)
        }

        fn vault_search_starts(&mut self) {
            self.log.push("search");
            if let Some(r) = self.concurrent.take() {
                self.store.put_export_record(r).unwrap();
            }
        }
    }

    #[test]
    fn export_reaches_the_store_only_around_the_vault_search() {
        let mut w = W::new("logged");
        let r = w.render("logged");
        let concurrent = ExportRecord {
            id: EntityId::from_bytes([7; 16]),
            meeting_id: w.meeting.id,
            device_id: DEVICE,
            path: VaultPath::new("Elsewhere/other.md").unwrap(),
            exported_unix_ms: 1,
        };
        let mut db = Logged {
            store: &mut w.store,
            log: Vec::new(),
            concurrent: Some(concurrent),
        };
        let err = export_note(
            &mut db,
            &w.vault,
            DEVICE,
            w.meeting.id,
            &r,
            &w.clock,
            &mut w.rng,
        )
        .unwrap_err();
        // The meeting's export lease taken, one read (meeting check + record), the walk
        // with no store access in progress, then the claim, which finds the path recorded
        // meanwhile: another export is creating this meeting's note there, so this one
        // stops (retryable) and leaves the record and the vault alone; the lease released.
        assert!(matches!(err, StoreError::ConcurrentEdit(_)), "{err}");
        assert!(err.is_retryable());
        assert_eq!(db.log, ["store", "store", "search", "store", "store"]);
        assert!(markdown_names(&w.vault).is_empty());
        assert!(temps_left(join(&w.vault, &r.path).parent().unwrap()).is_empty());
        let rec = db
            .store
            .export_record(w.meeting.id, DEVICE)
            .unwrap()
            .unwrap();
        assert_eq!(rec.path.as_str(), "Elsewhere/other.md");
        // A retry (the other export never created its note) claims and creates: the lease,
        // the read, the search, the claim, the last look before the rename, the final
        // record write and the lease release.
        db.log.clear();
        let report = export_note(
            &mut db,
            &w.vault,
            DEVICE,
            w.meeting.id,
            &r,
            &w.clock,
            &mut w.rng,
        )
        .unwrap();
        assert_eq!(report.action, ExportAction::Created);
        assert_eq!(
            db.log,
            [
                "store", "store", "search", "store", "store", "store", "store"
            ]
        );
        // The record keeps its id; the path is the note just created.
        let rec = w
            .store
            .export_record(w.meeting.id, DEVICE)
            .unwrap()
            .unwrap();
        assert_eq!(rec.id, EntityId::from_bytes([7; 16]));
        assert_eq!(rec.path, report.path);
    }

    /// A store shared by two exports in one test, borrowed per [`ExportDb::with_store`]
    /// call (as the runtime's lock is taken per call).
    struct Shared<'a>(&'a std::cell::RefCell<Store>);

    impl ExportDb for Shared<'_> {
        fn with_store<T>(
            &mut self,
            f: impl FnOnce(&mut Store) -> Result<T, StoreError>,
        ) -> Result<T, StoreError> {
            f(&mut self.0.borrow_mut())
        }
    }

    /// Runs export B of `w`'s meeting; `during_b` runs while B is about to rename its new
    /// note into place (after its search found nothing and its record was written).
    fn export_b_with(
        w: &mut W,
        cell: &std::cell::RefCell<Store>,
        r: &RenderedExport,
        during_b: &mut dyn FnMut(),
    ) -> Result<ExportReport, StoreError> {
        let mut once = Some(());
        export_with_hook(
            &mut Shared(cell),
            &w.vault,
            DEVICE,
            w.meeting.id,
            r,
            &w.clock,
            &mut w.rng,
            &mut |_| {
                if once.take().is_some() {
                    during_b();
                }
                Ok(())
            },
            &mut Search::Walk,
            &mut TempFiles::new(),
        )
    }

    fn markdown_names(vault: &Path) -> Vec<String> {
        let mut all = Vec::new();
        markdown_files(vault, &mut all);
        all.iter()
            .map(|p| rel_of(vault, p).unwrap().as_str().to_owned())
            .collect()
    }

    fn temps_left(dir: &Path) -> Vec<String> {
        std::fs::read_dir(dir)
            .map(|d| {
                d.flatten()
                    .map(|e| e.file_name().to_string_lossy().into_owned())
                    .filter(|n| n.ends_with(TMP_SUFFIX))
                    .collect()
            })
            .unwrap_or_default()
    }

    #[test]
    fn export_two_concurrent_creates_of_one_meeting_leave_one_note() {
        // Neither export of a never-exported meeting finds a note; A creates it while B is
        // about to rename its own at the same name. B must not pick a free name and write a
        // second note with the same earshot_id.
        let mut w = W::new("concurrent-create");
        let r = w.render("concurrent");
        let store = std::mem::replace(&mut w.store, Store::open_in_memory().unwrap());
        let cell = std::cell::RefCell::new(store);
        let (vault, meeting) = (w.vault.clone(), w.meeting.id);
        let clock = ManualClock::new(1_792_209_990_000);
        let mut a_report = None;
        let err = export_b_with(&mut w, &cell, &r, &mut || {
            a_report = Some(
                export_note(
                    &mut Shared(&cell),
                    &vault,
                    DEVICE,
                    meeting,
                    &r,
                    &clock,
                    &mut ChaCha8Rng::seed_from_u64(9),
                )
                .unwrap(),
            );
        })
        .unwrap_err();
        let a = a_report.expect("export A ran inside B");
        assert_eq!(a.action, ExportAction::Created);
        assert!(matches!(err, StoreError::ConcurrentEdit(_)), "{err}");
        assert!(err.is_retryable());
        assert_eq!(markdown_names(&vault), [a.path.as_str()]);
        let rec = cell
            .borrow()
            .export_record(meeting, DEVICE)
            .unwrap()
            .unwrap();
        assert_eq!(rec.path, a.path);
        let dir = join(&vault, &a.path).parent().unwrap().to_owned();
        assert!(temps_left(&dir).is_empty(), "{:?}", temps_left(&dir));
        // A retry merges into the one note.
        let again = export_b_with(&mut w, &cell, &r, &mut || {}).unwrap();
        assert_eq!(again.path, a.path);
        assert_eq!(markdown_names(&vault), [a.path.as_str()]);
    }

    /// `w`'s meeting rendered under another title (another note path).
    fn retitled(w: &W, title: &str) -> RenderedExport {
        let mut m = w.meeting.clone();
        m.title = title.to_owned();
        render(
            &ExportInput {
                transcript: None,
                meeting: &m,
                attendees: &[],
                speakers: &[],
                segments: &[],
                notes: &[],
                summary: None,
            },
            &ExportOptions::default(),
        )
        .unwrap()
    }

    /// A [`Shared`] store that runs `before` once, just before its `at`-th store access
    /// (1-based), outside the borrow.
    struct BeforeAccess<'a> {
        cell: &'a std::cell::RefCell<Store>,
        calls: usize,
        at: usize,
        before: Option<Box<dyn FnOnce() + 'a>>,
    }

    impl ExportDb for BeforeAccess<'_> {
        fn with_store<T>(
            &mut self,
            f: impl FnOnce(&mut Store) -> Result<T, StoreError>,
        ) -> Result<T, StoreError> {
            self.calls += 1;
            if self.calls == self.at
                && let Some(before) = self.before.take()
            {
                before();
            }
            f(&mut self.cell.borrow_mut())
        }
    }

    #[test]
    fn export_two_creates_of_different_renders_claim_one_note_when_the_later_searched_first() {
        // A searched the vault (nothing there) before B, with another title, created the
        // note and recorded it; A's claim then finds B's path recorded since its search and
        // stops (retryable) instead of creating a second note under its own title.
        let mut w = W::new("claim-searched-first");
        let (r_a, r_b) = (retitled(&w, "Planning"), retitled(&w, "Planning v2"));
        assert_ne!(r_a.path, r_b.path);
        let store = std::mem::replace(&mut w.store, Store::open_in_memory().unwrap());
        let cell = std::cell::RefCell::new(store);
        let (vault, meeting) = (w.vault.clone(), w.meeting.id);
        let clock = ManualClock::new(1_792_209_990_000);
        let b_report = std::cell::RefCell::new(None);
        let mut db = BeforeAccess {
            cell: &cell,
            calls: 0,
            // Access 1 takes A's export lease, access 2 reads the record before the
            // search; access 3 is A's claim. (B runs on this thread inside A's lease, as
            // a nested export, so the lease does not hold it back here.)
            at: 3,
            before: Some(Box::new(|| {
                let b = export_note(
                    &mut Shared(&cell),
                    &vault,
                    DEVICE,
                    meeting,
                    &r_b,
                    &clock,
                    &mut ChaCha8Rng::seed_from_u64(11),
                )
                .unwrap();
                *b_report.borrow_mut() = Some(b);
            })),
        };
        let err = export_note(
            &mut db, &w.vault, DEVICE, meeting, &r_a, &w.clock, &mut w.rng,
        )
        .unwrap_err();
        drop(db);
        let b = b_report.take().expect("export B ran before A's claim");
        assert_eq!(b.action, ExportAction::Created);
        assert_eq!(b.path, r_b.path);
        assert!(matches!(err, StoreError::ConcurrentEdit(_)), "{err}");
        assert!(err.is_retryable());
        assert_eq!(markdown_names(&vault), [b.path.as_str()]);
        let dir = join(&vault, &r_a.path).parent().unwrap().to_owned();
        assert!(temps_left(&dir).is_empty(), "{:?}", temps_left(&dir));
        let rec = cell
            .borrow()
            .export_record(meeting, DEVICE)
            .unwrap()
            .unwrap();
        assert_eq!(rec.path, b.path);
        // A retry merges into B's note.
        let again = export_note(
            &mut Shared(&cell),
            &vault,
            DEVICE,
            meeting,
            &r_a,
            &w.clock,
            &mut w.rng,
        )
        .unwrap();
        assert_eq!(again.path, b.path);
        assert_eq!(markdown_names(&vault), [b.path.as_str()]);
    }

    #[test]
    fn export_two_creates_of_different_renders_leave_one_note_when_the_later_claimed_last() {
        // B claimed its path and is about to rename its note into place; A, with another
        // title, searches (B's note is not there yet), claims over B's record and creates
        // its note. B's last look before the rename finds A's claim and stops (retryable).
        let mut w = W::new("claim-claimed-last");
        let (r_a, r_b) = (retitled(&w, "Planning"), retitled(&w, "Planning v2"));
        assert_ne!(r_a.path, r_b.path);
        let store = std::mem::replace(&mut w.store, Store::open_in_memory().unwrap());
        let cell = std::cell::RefCell::new(store);
        let (vault, meeting) = (w.vault.clone(), w.meeting.id);
        let clock = ManualClock::new(1_792_209_990_000);
        let mut a_report = None;
        let err = export_b_with(&mut w, &cell, &r_b, &mut || {
            a_report = Some(
                export_note(
                    &mut Shared(&cell),
                    &vault,
                    DEVICE,
                    meeting,
                    &r_a,
                    &clock,
                    &mut ChaCha8Rng::seed_from_u64(12),
                )
                .unwrap(),
            );
        })
        .unwrap_err();
        let a = a_report.expect("export A ran inside B");
        assert_eq!(a.action, ExportAction::Created);
        assert_eq!(a.path, r_a.path);
        assert!(matches!(err, StoreError::ConcurrentEdit(_)), "{err}");
        assert!(err.is_retryable());
        assert_eq!(markdown_names(&vault), [a.path.as_str()]);
        let dir = join(&vault, &r_b.path).parent().unwrap().to_owned();
        assert!(temps_left(&dir).is_empty(), "{:?}", temps_left(&dir));
        let rec = cell
            .borrow()
            .export_record(meeting, DEVICE)
            .unwrap()
            .unwrap();
        assert_eq!(rec.path, a.path);
        // A retry of B merges into A's note.
        let again = export_b_with(&mut w, &cell, &r_b, &mut || {}).unwrap();
        assert_eq!(again.path, a.path);
        assert_eq!(markdown_names(&vault), [a.path.as_str()]);
    }

    /// A store whose `fail_at`-th access (1-based) fails with the retryable `Busy`.
    struct BusyAt<'a> {
        store: &'a mut Store,
        calls: usize,
        fail_at: usize,
    }

    impl ExportDb for BusyAt<'_> {
        fn with_store<T>(
            &mut self,
            f: impl FnOnce(&mut Store) -> Result<T, StoreError>,
        ) -> Result<T, StoreError> {
            self.calls += 1;
            if self.calls == self.fail_at {
                return Err(StoreError::Busy("simulated".into()));
            }
            f(self.store)
        }
    }

    #[test]
    fn export_store_errors_after_the_temp_was_written_leave_no_temp_in_the_vault() {
        // Create: access 2 is the claim, 3 the last look before the rename. Update: access
        // 2 is the final record write, after the rename (the temp is the note by then).
        for fail_at in [2, 3] {
            let mut w = W::new(&format!("busy-create-{fail_at}"));
            let r = w.render("busy");
            let dir = join(&w.vault, &r.path).parent().unwrap().to_owned();
            for _ in 0..3 {
                let err = export_note(
                    &mut BusyAt {
                        store: &mut w.store,
                        calls: 0,
                        fail_at,
                    },
                    &w.vault,
                    DEVICE,
                    w.meeting.id,
                    &r,
                    &w.clock,
                    &mut w.rng,
                )
                .unwrap_err();
                assert!(err.is_retryable(), "{err}");
                assert!(
                    temps_left(&dir).is_empty(),
                    "{fail_at}: {:?}",
                    temps_left(&dir)
                );
            }
            assert!(markdown_names(&w.vault).is_empty());
            // A retry without the error creates the note, and nothing else is left.
            let ok = w.export(&r, &mut no_hook).unwrap();
            assert_eq!(ok.action, ExportAction::Created);
            assert_eq!(markdown_names(&w.vault), [r.path.as_str()]);
            assert!(temps_left(&dir).is_empty());
        }
        // An update whose note changed under it removes its temp too.
        let mut w = W::new("edited-update");
        let r1 = w.render("one");
        w.export(&r1, &mut no_hook).unwrap();
        let note = join(&w.vault, &r1.path);
        let r2 = w.render("two");
        let mut edit = |p: &Path| std::fs::write(p, "the user's edit\n");
        let err = w.export(&r2, &mut edit).unwrap_err();
        assert!(matches!(err, StoreError::ConcurrentEdit(_)), "{err}");
        assert!(temps_left(note.parent().unwrap()).is_empty());
    }

    #[test]
    fn export_create_after_another_export_created_the_note_leaves_one_note() {
        // A user's unrelated note holds the render's name, so creates go to `<stem> (n).md`.
        // A finishes while B is about to rename at the same free name: B must not take the
        // next free name.
        let mut w = W::new("concurrent-free-name");
        let r = w.render("concurrent");
        let taken = join(&w.vault, &r.path);
        std::fs::create_dir_all(taken.parent().unwrap()).unwrap();
        std::fs::write(&taken, "a note of the user's\n").unwrap();
        let store = std::mem::replace(&mut w.store, Store::open_in_memory().unwrap());
        let cell = std::cell::RefCell::new(store);
        let (vault, meeting) = (w.vault.clone(), w.meeting.id);
        let clock = ManualClock::new(1_792_209_990_000);
        let mut a_path = None;
        let err = export_b_with(&mut w, &cell, &r, &mut || {
            let a = export_note(
                &mut Shared(&cell),
                &vault,
                DEVICE,
                meeting,
                &r,
                &clock,
                &mut ChaCha8Rng::seed_from_u64(10),
            )
            .unwrap();
            a_path = Some(a.path);
        })
        .unwrap_err();
        assert!(matches!(err, StoreError::ConcurrentEdit(_)), "{err}");
        let a_path = a_path.unwrap();
        assert_ne!(a_path, r.path);
        let mut names = markdown_names(&vault);
        names.sort();
        let mut want = vec![r.path.as_str().to_owned(), a_path.as_str().to_owned()];
        want.sort();
        assert_eq!(names, want, "exactly the user's note and one meeting note");
        let rec = cell
            .borrow()
            .export_record(meeting, DEVICE)
            .unwrap()
            .unwrap();
        assert_eq!(rec.path, a_path);
    }

    /// One connection of a store shared by two exporters (two threads standing in for two
    /// processes); `busy` is signalled the first time the export waits for the lease.
    struct Waiting {
        store: Store,
        busy: Option<std::sync::mpsc::Sender<()>>,
    }

    impl ExportDb for Waiting {
        fn with_store<T>(
            &mut self,
            f: impl FnOnce(&mut Store) -> Result<T, StoreError>,
        ) -> Result<T, StoreError> {
            f(&mut self.store)
        }

        fn export_lease_busy(&mut self) {
            if let Some(b) = self.busy.take() {
                let _ = b.send(());
            }
        }
    }

    /// `w`'s meeting rendered under another title (another note path).
    fn render_titled(w: &W, title: &str) -> RenderedExport {
        let mut m = w.meeting.clone();
        m.title = title.to_owned();
        render_for(&m)
    }

    /// `w`'s store moved to a database file next to the vault, opened twice.
    fn on_disk(w: &mut W) -> (Store, Store, PathBuf) {
        let db = w.vault.with_extension("db");
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{suffix}", db.display()));
        }
        let mut a = Store::open(&db).unwrap();
        a.put_meeting(w.meeting.clone()).unwrap();
        let b = Store::open(&db).unwrap();
        (a, b, db)
    }

    #[test]
    fn export_two_exports_with_different_renders_of_one_meeting_leave_one_note() {
        // T6.4c (review A8): the title changed between two exports of a never-exported
        // meeting, so their renders name different notes and the sibling check of one
        // cannot see the other's note. The per-meeting lease serialises them: B holds it
        // from its search to its rename, A (another connection, as another process would)
        // waits, then finds B's note and merges into it.
        let mut w = W::new("lease-two-renders");
        let (a_store, b_store, db) = on_disk(&mut w);
        let (r_a, r_b) = (render_titled(&w, "Planning"), render_titled(&w, "Roadmap"));
        assert_ne!(r_a.path, r_b.path);
        let (vault, meeting) = (w.vault.clone(), w.meeting.id);
        let (go_tx, go) = std::sync::mpsc::channel::<()>();
        let (busy_tx, busy) = std::sync::mpsc::channel();
        let a = std::thread::spawn(move || {
            go.recv().unwrap();
            let mut db = Waiting {
                store: a_store,
                busy: Some(busy_tx),
            };
            export_note(
                &mut db,
                &vault,
                DEVICE,
                meeting,
                &r_a,
                &ManualClock::new(1_792_209_990_000),
                &mut ChaCha8Rng::seed_from_u64(21),
            )
        });
        let mut b_db = Waiting {
            store: b_store,
            busy: None,
        };
        let mut waited = false;
        let b = export_with_hook(
            &mut b_db,
            &w.vault,
            DEVICE,
            meeting,
            &r_b,
            &w.clock,
            &mut w.rng,
            &mut |_| {
                go_tx.send(()).unwrap();
                waited = busy
                    .recv_timeout(std::time::Duration::from_secs(20))
                    .is_ok();
                Ok(())
            },
            &mut Search::Walk,
            &mut TempFiles::new(),
        )
        .unwrap();
        assert!(waited, "A must wait for B's lease");
        let a = a.join().unwrap().unwrap();
        assert_eq!(b.action, ExportAction::Created);
        assert_eq!(a.path, b.path, "A merged into B's note");
        assert_eq!(markdown_names(&w.vault), [b.path.as_str()]);
        assert_eq!(
            b_db.store.local_value(&lease_key(meeting)).unwrap(),
            None,
            "both leases released"
        );
        drop(b_db);
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{suffix}", db.display()));
        }
    }

    fn foreign_lease(store: &mut Store, meeting: EntityId, since_ms: u64) -> Vec<u8> {
        let mut v = vec![0xEE; 12];
        v.extend_from_slice(&since_ms.to_le_bytes());
        store.set_local_value(&lease_key(meeting), &v).unwrap();
        v
    }

    #[test]
    fn export_lease_of_another_export_is_waited_for_then_refused_retryably() {
        let mut w = W::new("lease-held");
        let r = w.render("held");
        let now = w.clock.now_unix_ms();
        let theirs = foreign_lease(&mut w.store, w.meeting.id, now);
        let short = LeaseTiming {
            wait: std::time::Duration::from_millis(60),
            poll: std::time::Duration::from_millis(5),
            ..LeaseTiming::DEFAULT
        };
        let err = export_with_timing(
            &mut w.store,
            &w.vault,
            DEVICE,
            w.meeting.id,
            &r,
            &w.clock,
            &mut w.rng,
            &mut no_hook,
            &mut Search::Walk,
            &mut TempFiles::new(),
            short,
        )
        .unwrap_err();
        assert!(matches!(err, StoreError::ConcurrentEdit(_)), "{err}");
        assert!(err.is_retryable());
        assert_eq!(w.md_count(), 0, "nothing written");
        assert_eq!(
            w.store.local_value(&lease_key(w.meeting.id)).unwrap(),
            Some(theirs),
            "another export's lease is never released by this one"
        );
    }

    #[test]
    fn export_lease_left_by_a_crash_is_taken_over() {
        let mut w = W::new("lease-stale");
        let r = w.render("stale");
        let now = w.clock.now_unix_ms();
        // Stale in either direction of the export clock.
        let stale = LeaseTiming::DEFAULT.stale_ms + 10_000;
        for since in [now - stale, now + stale] {
            foreign_lease(&mut w.store, w.meeting.id, since);
            w.export(&r, &mut no_hook).unwrap();
            assert_eq!(w.store.local_value(&lease_key(w.meeting.id)).unwrap(), None);
        }
        assert_eq!(w.md_count(), 1);
    }

    #[test]
    fn export_lease_depth_is_undone_when_the_export_panics() {
        let mut w = W::new("lease-panic");
        let r = w.render("panic");
        let meeting = w.meeting.id;
        let vault = w.vault.clone();
        let (store, clock) = (&mut w.store, &w.clock);
        let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            with_export_lease(
                store,
                &vault,
                meeting,
                &r,
                clock,
                LeaseTiming::DEFAULT,
                |_, _| -> Result<(), StoreError> { panic!("export panics") },
            )
        }));
        assert!(caught.is_err());
        assert!(
            !LEASES_HELD.with(|h| h.borrow().contains_key(&meeting)),
            "a panic must not leave the meeting marked as held by this thread"
        );
        // The row it left goes stale; with a fresh clock past it, the next export runs.
        w.clock.advance_ms(LeaseTiming::DEFAULT.stale_ms + 1);
        w.export(&r, &mut no_hook).unwrap();
        assert_eq!(w.md_count(), 1);
    }

    /// `w`'s meeting deleted in `store` as another process would (rows tombstoned, export
    /// records removed).
    fn delete_elsewhere(w: &W, store: &mut Store) {
        let data = crate::layout::DataDir::new(w.vault.with_extension("data"));
        let mut hlc = earshot_core::identity::Hlc::new(DeviceId::from_bytes([0xd6; 16]));
        store
            .delete_meeting(&data, w.meeting.id, &mut hlc, &w.clock, &mut |_: &str| {
                false
            })
            .unwrap();
    }

    #[test]
    fn export_of_a_deleted_meeting_writes_no_record() {
        let mut w = W::new("deleted-before");
        let r = w.render("deleted");
        let mut store = std::mem::replace(&mut w.store, Store::open_in_memory().unwrap());
        delete_elsewhere(&w, &mut store);
        w.store = store;
        let err = w.export(&r, &mut no_hook).unwrap_err();
        assert!(
            matches!(err, StoreError::Repo(RepoError::MissingParent { .. })),
            "{err}"
        );
        assert_eq!(w.store.export_record(w.meeting.id, DEVICE).unwrap(), None);
        assert_eq!(w.md_count(), 0);
    }

    #[test]
    fn export_of_a_meeting_deleted_while_it_runs_leaves_no_record() {
        // Another process deletes the meeting after this export read it (cross-process: no
        // shared lease table), on the create path (after the claim) and on the update path.
        for existing in [false, true] {
            let mut w = W::new(if existing {
                "deleted-update"
            } else {
                "deleted-create"
            });
            let r = w.render("deleted");
            let r2 = w.render("deleted again");
            if existing {
                w.export(&r, &mut no_hook).unwrap();
            }
            let store = std::mem::replace(&mut w.store, Store::open_in_memory().unwrap());
            let cell = std::cell::RefCell::new(store);
            let deleted = std::cell::Cell::new(false);
            let err = {
                let w_ref = &w;
                let during = || {
                    delete_elsewhere(w_ref, &mut cell.borrow_mut());
                    deleted.set(true);
                };
                // `export_b_with` needs `&mut W`; run the same hooked export inline.
                let mut once = Some(());
                export_with_hook(
                    &mut Shared(&cell),
                    &w_ref.vault,
                    DEVICE,
                    w_ref.meeting.id,
                    if existing { &r2 } else { &r },
                    &w_ref.clock,
                    &mut ChaCha8Rng::seed_from_u64(31),
                    &mut |_| {
                        if once.take().is_some() {
                            during();
                        }
                        Ok(())
                    },
                    &mut Search::Walk,
                    &mut TempFiles::new(),
                )
                .unwrap_err()
            };
            assert!(deleted.get());
            assert!(
                matches!(
                    err,
                    StoreError::Repo(RepoError::MissingParent { .. })
                        | StoreError::ConcurrentEdit(_)
                ),
                "{err}"
            );
            assert_eq!(
                cell.borrow().export_record(w.meeting.id, DEVICE).unwrap(),
                None,
                "existing={existing}: no record of a deleted meeting"
            );
            assert!(
                cell.borrow()
                    .local_value(&lease_key(w.meeting.id))
                    .unwrap()
                    .is_none()
            );
        }
    }

    /// An [`ExportDb`] over a shared store that calls `hook(n)` just before its `n`-th
    /// store access (1-based), outside the borrow.
    struct EachAccess<'a> {
        cell: &'a std::cell::RefCell<Store>,
        calls: usize,
        hook: Box<dyn FnMut(usize) + 'a>,
    }

    impl ExportDb for EachAccess<'_> {
        fn with_store<T>(
            &mut self,
            f: impl FnOnce(&mut Store) -> Result<T, StoreError>,
        ) -> Result<T, StoreError> {
            self.calls += 1;
            (self.hook)(self.calls);
            f(&mut self.cell.borrow_mut())
        }
    }

    /// `w`'s store, moved out for an [`EachAccess`].
    fn take_store(w: &mut W) -> std::cell::RefCell<Store> {
        std::cell::RefCell::new(std::mem::replace(
            &mut w.store,
            Store::open_in_memory().unwrap(),
        ))
    }

    fn export_each_access(
        w: &W,
        cell: &std::cell::RefCell<Store>,
        r: &RenderedExport,
        hook: impl FnMut(usize),
    ) -> Result<ExportReport, StoreError> {
        export_with_hook(
            &mut EachAccess {
                cell,
                calls: 0,
                hook: Box::new(hook),
            },
            &w.vault,
            DEVICE,
            w.meeting.id,
            r,
            &w.clock,
            &mut ChaCha8Rng::seed_from_u64(41),
            &mut no_hook,
            &mut Search::Walk,
            &mut TempFiles::new(),
        )
    }

    #[test]
    fn export_of_a_meeting_deleted_just_before_the_claim_leaves_no_record() {
        // Access 1 takes the lease, access 2 reads the record, access 3 is the claim: the
        // meeting is deleted between the search and the claim, so the claim's meeting
        // check refuses and nothing names the deleted meeting.
        let mut w = W::new("deleted-at-claim");
        let r = w.render("deleted");
        let cell = take_store(&mut w);
        let err = export_each_access(&w, &cell, &r, |n| {
            if n == 3 {
                delete_elsewhere(&w, &mut cell.borrow_mut());
            }
        })
        .unwrap_err();
        assert!(
            matches!(err, StoreError::Repo(RepoError::MissingParent { .. })),
            "{err}"
        );
        assert_eq!(
            cell.borrow().export_record(w.meeting.id, DEVICE).unwrap(),
            None
        );
        assert_eq!(w.md_count(), 0, "no note for a deleted meeting");
    }

    #[test]
    fn export_meeting_check_and_record_write_are_one_transaction() {
        // Another process deletes the meeting in a transaction that is open when this
        // export's claim (create) or final record write (update, access 3 too) starts, and
        // commits while it waits for the write lock: the check runs after the deletion
        // committed, in the transaction that writes, so no record names a deleted meeting.
        for existing in [false, true] {
            let mut w = W::new(if existing { "tx-update" } else { "tx-create" });
            let r = w.render("tx");
            let (mut a, b, db) = on_disk(&mut w);
            if existing {
                export_with_hook(
                    &mut a,
                    &w.vault,
                    DEVICE,
                    w.meeting.id,
                    &r,
                    &w.clock,
                    &mut w.rng,
                    &mut no_hook,
                    &mut Search::Walk,
                    &mut TempFiles::new(),
                )
                .unwrap();
            }
            let cell = std::cell::RefCell::new(a);
            let mut b = Some(b);
            let mut deleter = None;
            let id = w.meeting.id.as_uuid().as_bytes().to_vec();
            let err = export_each_access(&w, &cell, &r, |n| {
                if n != 3 {
                    return;
                }
                let b = b.take().unwrap();
                let id = id.clone();
                let (open, opened) = std::sync::mpsc::channel();
                deleter = Some(std::thread::spawn(move || {
                    b.conn.execute_batch("BEGIN IMMEDIATE").unwrap();
                    b.conn
                        .execute("UPDATE meeting SET deleted = 1 WHERE id = ?1", [&id])
                        .unwrap();
                    b.conn
                        .execute("DELETE FROM export_record WHERE meeting_id = ?1", [&id])
                        .unwrap();
                    open.send(()).unwrap();
                    std::thread::sleep(std::time::Duration::from_millis(300));
                    b.conn.execute_batch("COMMIT").unwrap();
                    b
                }));
                opened.recv().unwrap();
            })
            .unwrap_err();
            let b = deleter.take().unwrap().join().unwrap();
            assert!(
                matches!(err, StoreError::Repo(RepoError::MissingParent { .. })),
                "existing={existing}: {err}"
            );
            let a = cell.into_inner();
            assert_eq!(
                a.export_record(w.meeting.id, DEVICE).unwrap(),
                None,
                "existing={existing}: no record of a deleted meeting"
            );
            drop((a, b));
            for suffix in ["", "-wal", "-shm"] {
                let _ = std::fs::remove_file(format!("{}{suffix}", db.display()));
            }
        }
    }

    #[test]
    fn export_lease_holder_restamps_its_lease_so_a_long_export_keeps_it() {
        // The export clock moves 0.6 x stale_ms before the claim and again before the final
        // record write: 1.2 x stale_ms after the lease was taken, but at most 0.6 x since
        // the holder last re-stamped it, so another export cannot take it.
        let mut w = W::new("lease-heartbeat");
        let r = w.render("heartbeat");
        let cell = take_store(&mut w);
        let step = LeaseTiming::DEFAULT.stale_ms * 6 / 10;
        let mut taken_by_other = Vec::new();
        let report = export_each_access(&w, &cell, &r, |n| {
            if n == 3 || n == 5 {
                w.clock.advance_ms(step);
            }
            if n == 5 {
                taken_by_other.push(
                    cell.borrow_mut()
                        .try_take_export_lease(
                            w.meeting.id,
                            [0xEE; 12],
                            w.clock.now_unix_ms(),
                            LeaseTiming::DEFAULT.stale_ms,
                        )
                        .unwrap(),
                );
            }
        })
        .unwrap();
        assert_eq!(report.action, ExportAction::Created);
        assert_eq!(taken_by_other, [false], "a live lease is never taken over");
        assert_eq!(
            cell.borrow().local_value(&lease_key(w.meeting.id)).unwrap(),
            None,
            "released"
        );
    }

    #[test]
    fn export_whose_lease_was_taken_over_stops_before_writing() {
        // A hung export's lease went stale and another export took it: at its next store
        // access the first export finds the lease is no longer its own and stops
        // (retryable), creating no note and leaving the new holder's lease alone.
        let mut w = W::new("lease-lost");
        let r = w.render("lost");
        let cell = take_store(&mut w);
        let mut theirs = None;
        let err = export_each_access(&w, &cell, &r, |n| {
            if n == 3 {
                theirs = Some(foreign_lease(
                    &mut cell.borrow_mut(),
                    w.meeting.id,
                    w.clock.now_unix_ms(),
                ));
            }
        })
        .unwrap_err();
        assert!(matches!(err, StoreError::ConcurrentEdit(_)), "{err}");
        assert!(err.is_retryable());
        assert_eq!(w.md_count(), 0);
        assert_eq!(
            cell.borrow().export_record(w.meeting.id, DEVICE).unwrap(),
            None
        );
        assert_eq!(
            cell.borrow().local_value(&lease_key(w.meeting.id)).unwrap(),
            theirs
        );
    }
}
