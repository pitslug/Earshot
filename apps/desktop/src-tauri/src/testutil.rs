//! Test helpers shared by the crate's tests (std only, AGENTS.md).

use std::path::PathBuf;
use std::sync::Arc;

use earshot_core::identity::ManualClock;
use earshot_runtime::Services;
use earshot_store::DataDir;

/// A directory under the system temp dir, removed on drop.
pub struct TempDir(pub PathBuf);

impl TempDir {
    pub fn new(tag: &str) -> Self {
        static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("earshot-desktop-{tag}-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// The clock of test services (2026-10-15 05:00 UTC).
pub const T0: u64 = 1_792_294_800_000;

/// Services over a fresh store in `dir`, on a manual clock at [`T0`].
pub fn services(dir: &TempDir) -> Services {
    let data = DataDir::new(dir.0.join("data"));
    data.create().unwrap();
    let store = data.open_store().unwrap();
    Services::new(store, data, Arc::new(ManualClock::new(T0))).unwrap()
}
