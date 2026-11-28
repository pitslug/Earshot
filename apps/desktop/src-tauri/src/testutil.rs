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

/// Picks the online service of the online writer in `steps` (the endpoint's by `provider`, else
/// by the stored provider), as the Summaries page's picker does before an online writer can be
/// ticked (T14.72); a `provider` that differs from the stored one is picked first, the only way
/// the provider changes. The first half of the one setup helper of the desktop's summary tests.
pub fn pick_for(
    svc: &Services,
    steps: &[earshot_runtime::summary_order::OrderStep],
    provider: Option<earshot_runtime::summary_settings::EndpointProvider>,
) {
    use earshot_runtime::summary_order::{OrderStep, Service, WriterStep, set_online_service};
    let stored = earshot_runtime::summary_settings::summary_settings(&svc.store())
        .unwrap()
        .0
        .provider;
    let provider = provider.unwrap_or(stored);
    if provider != stored {
        let service = Service::of_step(WriterStep::Endpoint, provider).unwrap();
        set_online_service(&mut svc.store(), service).unwrap();
    }
    for step in steps {
        if let OrderStep::Writer(w) = step
            && let Some(service) = Service::of_step(*w, provider)
        {
            set_online_service(&mut svc.store(), service).unwrap();
        }
    }
}

/// [`pick_for`] the online writer of `steps` (by the provider of `settings`, else the stored
/// one), then stores the order of `steps` with `settings`: how the Summaries page stores an
/// order holding an online writer since T14.72.
pub fn store_order(
    svc: &Services,
    steps: &[earshot_runtime::summary_order::OrderStep],
    settings: Option<&earshot_runtime::summary_settings::SummarySettings>,
) {
    pick_for(svc, steps, settings.map(|s| s.provider));
    earshot_runtime::summary_order::set_summary_order(
        &mut svc.store(),
        &earshot_runtime::summary_order::SummaryOrder::new(steps.to_vec()).unwrap(),
        settings,
    )
    .unwrap();
}

/// Stores `settings` with their writer as the order's only writer step (no summary: none; the
/// order's computers kept, first), picking its online service first: what the Summaries page's
/// one writer did before T14.72 (`set_summary_settings` then changed the order). The one setup
/// helper of desktop tests that set a writer through the settings.
pub fn store_settings(
    svc: &Services,
    settings: &earshot_runtime::summary_settings::SummarySettings,
) {
    use earshot_runtime::summary_order::{OrderStep, WriterStep};
    let (order, _) = earshot_runtime::summary_order::summary_order(&svc.store()).unwrap();
    let mut steps: Vec<OrderStep> = order
        .steps()
        .iter()
        .copied()
        .filter(|s| matches!(s, OrderStep::Computer(_)))
        .collect();
    steps.extend(WriterStep::of_writer(settings.writer).map(OrderStep::Writer));
    store_order(svc, &steps, Some(settings));
}
