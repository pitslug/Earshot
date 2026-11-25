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
    services_with_clock(dir).0
}

/// [`services`], and its clock, for tests that move time.
pub fn services_with_clock(dir: &TempDir) -> (Services, Arc<ManualClock>) {
    let data = DataDir::new(dir.0.join("data"));
    data.create().unwrap();
    let store = data.open_store().unwrap();
    let clock = Arc::new(ManualClock::new(T0));
    let svc = Services::new(store, data, Arc::clone(&clock) as _).unwrap();
    (svc, clock)
}

/// A clock the test moves by hand, keeping time awake and time asleep apart (T12.62's fake,
/// T14.8): what a stall rule that must not count a system sleep is tested on.
#[derive(Clone, Default)]
pub struct HandClock {
    awake_ms: Arc<std::sync::atomic::AtomicU64>,
    asleep_ms: Arc<std::sync::atomic::AtomicU64>,
    reads: Arc<std::sync::atomic::AtomicU64>,
}

impl HandClock {
    fn ms(counter: &std::sync::atomic::AtomicU64) -> u64 {
        counter.load(std::sync::atomic::Ordering::SeqCst)
    }

    fn add(counter: &std::sync::atomic::AtomicU64, by: std::time::Duration) {
        counter.fetch_add(
            u64::try_from(by.as_millis()).unwrap(),
            std::sync::atomic::Ordering::SeqCst,
        );
    }

    /// What the code under test is given: time awake only.
    pub fn awake(&self) -> earshot_proc::AwakeClock {
        let me = self.clone();
        earshot_proc::AwakeClock::from_fn(move || {
            Self::add(&me.reads, std::time::Duration::from_millis(1));
            std::time::Duration::from_millis(Self::ms(&me.awake_ms))
        })
    }

    /// What `Instant` reads on Windows: time awake and time asleep (the negative control).
    pub fn wall(&self) -> earshot_proc::AwakeClock {
        let me = self.clone();
        earshot_proc::AwakeClock::from_fn(move || {
            Self::add(&me.reads, std::time::Duration::from_millis(1));
            std::time::Duration::from_millis(Self::ms(&me.awake_ms) + Self::ms(&me.asleep_ms))
        })
    }

    /// Time passes with the computer awake.
    pub fn advance(&self, by: std::time::Duration) {
        Self::add(&self.awake_ms, by);
    }

    /// Time passes with the computer asleep.
    pub fn sleep(&self, by: std::time::Duration) {
        Self::add(&self.asleep_ms, by);
    }

    /// Waits until the code under test has read the clock twice more: it has then judged the
    /// time as it is now at least once. The bound is only for a failing case.
    pub fn read_twice_more(&self) {
        let from = Self::ms(&self.reads);
        let until = std::time::Instant::now() + std::time::Duration::from_secs(60);
        while Self::ms(&self.reads) < from + 2 {
            assert!(
                std::time::Instant::now() < until,
                "the clock is no longer read"
            );
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
    }
}

/// A data folder in `dir` whose database a newer Earshot saved: a fresh store, closed, with
/// SQLite's `user_version` (header bytes 60..64, big-endian) set past this build's.
pub fn newer_database(dir: &TempDir) -> DataDir {
    let data = DataDir::new(dir.0.join("newer"));
    data.create().unwrap();
    drop(data.open_store().unwrap());
    let db = data.root().join("earshot.db");
    let mut bytes = std::fs::read(&db).unwrap();
    bytes[60..64].copy_from_slice(&9_999u32.to_be_bytes());
    std::fs::write(&db, bytes).unwrap();
    data
}

/// The shared table of Summaries and Dictionary refusals and messages (T12.20,
/// `fixtures/ui/summary_settings_cases.json`), which the UI mock's tests run too.
pub fn summary_cases() -> serde_json::Value {
    serde_json::from_str(include_str!(
        "../../../../fixtures/ui/summary_settings_cases.json"
    ))
    .unwrap()
}
