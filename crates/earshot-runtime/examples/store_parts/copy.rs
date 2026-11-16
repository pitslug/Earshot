//! Copying the owner's store for an evaluation tool (T12.25a, shared with T12.71's
//! `vp_readout`): the database (and its `-wal`, never the `-shm`) is copied once as files into
//! a run folder and the copy is opened read-only. The original is never opened. Included by
//! `store_inputs` and `vp_readout` with `#[path]`.

use std::path::{Component, Path, PathBuf};

use earshot_store::ReadOnlyStore;

/// The folder names a run folder must lie under (case-insensitive), as `sum_eval`'s
/// `check_private` requires: git-ignored through `fixtures/private/`.
const PRIVATE_TREE: [&str; 3] = ["fixtures", "private", "real-meetings"];

/// Whether `into` lies strictly inside a `fixtures/private/real-meetings/` folder. Read from
/// the path as given and, when it exists, from its resolved form. The rule only reads names:
/// nothing on disk changes.
pub fn inside_private_tree(into: &Path) -> bool {
    let check = |p: &Path| {
        let names: Vec<String> = p
            .components()
            .filter_map(|c| match c {
                Component::Normal(n) => Some(n.to_string_lossy().to_ascii_lowercase()),
                _ => None,
            })
            .collect();
        names
            .windows(PRIVATE_TREE.len())
            .enumerate()
            .any(|(i, w)| w == PRIVATE_TREE && i + PRIVATE_TREE.len() < names.len())
    };
    let given = if into.is_absolute() {
        into.to_path_buf()
    } else {
        std::env::current_dir()
            .map(|d| d.join(into))
            .unwrap_or_else(|_| into.to_path_buf())
    };
    check(&given) && std::fs::canonicalize(&given).map_or(true, |r| check(&r))
}

/// Whether some process holds `shm` open. On Windows: an open for reading that shares
/// nothing fails while any other handle exists (it never writes the file). Elsewhere: whether
/// it exists, which can refuse a stale one.
pub fn shm_in_use(shm: &Path) -> bool {
    if !shm.exists() {
        return false;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        std::fs::OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(shm)
            .is_err()
    }
    #[cfg(not(windows))]
    {
        true
    }
}

/// Copies `db` (and its `-wal`) into `into/earshot.db` and opens the copy read-only. Refuses
/// a folder that already holds a copy and a database some process has open. `subdirs` are
/// created inside `into` first. Errors name the run folder at most, never a row.
pub fn copy_and_open(db: &Path, into: &Path, subdirs: &[&str]) -> Result<ReadOnlyStore, String> {
    let copy = into.join("earshot.db");
    if copy.exists() {
        return Err(format!(
            "{} already holds a copy; give the run another name",
            into.display()
        ));
    }
    if !db.is_file() {
        return Err("--db is not a file".into());
    }
    // A copy taken while a connection is open could be a torn database and WAL pair. Every
    // open connection holds the `-shm` file; one left behind by an app that exited without a
    // clean close (Earshot's quit ends the process) is held by nobody and is no reason to
    // refuse.
    let mut shm = db.to_path_buf().into_os_string();
    shm.push("-shm");
    if shm_in_use(Path::new(&shm)) {
        return Err(
            "the database is open (a process holds its -shm file); quit Earshot and \
                    try again"
                .into(),
        );
    }
    std::fs::create_dir_all(into).map_err(|e| format!("{}: {e}", into.display()))?;
    for sub in subdirs {
        let d = into.join(sub);
        std::fs::create_dir_all(&d).map_err(|e| format!("{}: {e}", d.display()))?;
    }
    std::fs::copy(db, &copy).map_err(|e| format!("copying the database: {e}"))?;
    let mut wal = db.to_path_buf().into_os_string();
    wal.push("-wal");
    let wal = PathBuf::from(wal);
    if wal.is_file() {
        std::fs::copy(&wal, into.join("earshot.db-wal"))
            .map_err(|e| format!("copying the database's WAL: {e}"))?;
    }
    ReadOnlyStore::open(&copy).map_err(|e| format!("the copy: {e}"))
}
