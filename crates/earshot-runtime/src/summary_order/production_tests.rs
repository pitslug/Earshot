//! T12.63b: the order of summary writers through the production types that T12.63a pinned only
//! with fakes (its review advisories 2 and 3): [`crate::host::SettingsWriters`],
//! [`crate::offload::choose_device_now`], [`crate::host::order_summary_hold`] and
//! [`crate::summary_order::live_summaries_allowed`] over a stored order. Every name holds
//! `summary_order`.

use std::sync::atomic::{AtomicU32, Ordering};

use earshot_core::asr::CancelToken;
use earshot_core::identity::DeviceId;

use super::*;
use crate::host::{SettingsWriters, SkipReason, SummaryResult};
use crate::keep_local::SummaryPrivacy;
use crate::summary_order::{SummaryOrder, set_summary_order};
use crate::summary_settings::{
    ApiKey, ApiKeyError, ApiKeyStore, EndpointProvider, SummarySettings,
};
use crate::testutil::{TempDir, meeting, services};

const DESK: DeviceId = DeviceId::from_bytes([7; 16]);
const LOCAL: OrderStep = OrderStep::LOCAL;
const ENDPOINT: OrderStep = OrderStep::Writer(WriterStep::Endpoint);
const CLAUDE: OrderStep = OrderStep::Writer(WriterStep::ClaudeCode);
const CODEX: OrderStep = OrderStep::Writer(WriterStep::Codex);

/// A key store that counts its reads.
#[derive(Default)]
struct CountedKeys(AtomicU32);

impl ApiKeyStore for CountedKeys {
    fn anthropic_key(&self) -> Result<Option<ApiKey>, ApiKeyError> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(ApiKey::new("sk-ant-an-invented-test-key-0123456789"))
    }
}

/// An agent CLI probe that counts its calls and answers "not installed".
#[derive(Default)]
struct CountedCli(AtomicU32);

impl crate::claude_code::ClaudeCodeProbe for CountedCli {
    fn status(&self) -> crate::claude_code::ClaudeCodeStatus {
        self.0.fetch_add(1, Ordering::SeqCst);
        crate::claude_code::ClaudeCodeStatus::NotInstalled
    }
}

impl crate::codex::CodexProbe for CountedCli {
    fn status(&self) -> crate::codex::CodexStatus {
        self.0.fetch_add(1, Ordering::SeqCst);
        crate::codex::CodexStatus::NotInstalled
    }
}

fn count(n: &AtomicU32) -> u32 {
    n.load(Ordering::SeqCst)
}

fn order(steps: &[OrderStep]) -> SummaryOrder {
    SummaryOrder::new(steps.to_vec()).unwrap()
}

fn kept_local(svc: &crate::Services, m: earshot_core::identity::EntityId) {
    use earshot_core::model::MeetingRepo;
    let mut db = svc.store();
    let mut row = db.meeting(m).unwrap().unwrap();
    row.summary_kept_local = true;
    db.put_meeting(row).unwrap();
}

fn anthropic_endpoint() -> SummarySettings {
    SummarySettings {
        provider: EndpointProvider::Anthropic,
        url: "https://api.anthropic.com".into(),
        model: "invented-model".into(),
        ..SummarySettings::default()
    }
}

#[test]
fn summary_order_production_writers_read_no_key_and_probe_no_cli_for_dropped_steps() {
    let dir = TempDir::new("order-production-kept");
    let (svc, _) = services(&dir);
    let m = meeting(&svc, "Planning", 21);
    kept_local(&svc, m);
    set_summary_order(
        &mut svc.store(),
        &order(&[ENDPOINT, CLAUDE, CODEX]),
        Some(&anthropic_endpoint()),
    )
    .unwrap();
    // An empty models folder: no local model is installed.
    let models = crate::backends::ModelDir::new(dir.0.join("no-models"));
    let (keys, claude, codex) = (
        CountedKeys::default(),
        CountedCli::default(),
        CountedCli::default(),
    );
    let hardware = earshot_hardware::Hardware::default();
    let writers = SettingsWriters {
        models: &models,
        keys: &keys,
        claude: &claude,
        codex: &codex,
        dll_dirs: Vec::new(),
        order: vec![crate::backends::Accelerator::Cpu],
        hardware: &hardware,
        computers: None,
    };
    let plan = RunPlan {
        writers: &writers,
        local_not_now: &|| false,
        order: None,
    };
    let (stored, _) = crate::summary_order::summary_order(&svc.store()).unwrap();
    assert_eq!(stored, order(&[ENDPOINT, CLAUDE, CODEX]));
    let out = run_summary_order(
        &svc,
        m,
        RunStart::Order(stored.clone()),
        &plan,
        &CancelToken::new(),
    );
    assert_eq!(count(&keys.0), 0, "no API key read for a dropped step");
    assert_eq!(count(&claude.0), 0, "Claude Code never probed");
    assert_eq!(count(&codex.0), 0, "Codex never probed");
    assert!(
        matches!(
            out.summary,
            SummaryResult::Skipped {
                reason: SkipReason::KeptLocalNoModel { .. }
            }
        ),
        "{:?}",
        out.summary
    );
    // A named remote writer on the kept meeting is refused before anything is read or probed.
    let named = run_summary_order(&svc, m, RunStart::Named(CLAUDE), &plan, &CancelToken::new());
    assert!(matches!(
        named.summary,
        SummaryResult::Failed {
            error: crate::RuntimeError::SummaryKeptLocal(_),
            ..
        }
    ));
    assert_eq!((count(&keys.0), count(&claude.0)), (0, 0));
    // The negative control: the same order for a meeting not kept here reads the key and probes
    // both CLIs, each when its step is tried.
    let open = meeting(&svc, "Planning", 22);
    let out = run_summary_order(
        &svc,
        open,
        RunStart::Order(stored),
        &plan,
        &CancelToken::new(),
    );
    assert_eq!(count(&keys.0), 1);
    assert_eq!(count(&claude.0), 1);
    assert_eq!(count(&codex.0), 1);
    assert_eq!(out.steps.len(), 3);
}

#[test]
fn summary_order_production_choose_device_now_asks_nothing_with_sync_off() {
    let dir = TempDir::new("order-production-choose");
    let (svc, _) = services(&dir);
    let vault: std::sync::Arc<dyn crate::sync::SyncVault> =
        std::sync::Arc::new(crate::sync::MemoryVault::default());
    // Sync off and no vault: not asked, and nothing reaches a relay.
    assert_eq!(
        crate::offload::choose_device_now(&svc, vault, DESK),
        OffloadChoice::HereInstead {
            device: DESK,
            name: None,
            why: Unavailable::SyncOff,
        }
    );
}

#[test]
fn summary_order_production_hold_and_live_summaries_follow_the_filtered_order() {
    use crate::meeting_queue::SummaryHold;
    let dir = TempDir::new("order-production-hold");
    let (svc, _) = services(&dir);
    let hardware = earshot_hardware::Hardware::default();
    let accel = [crate::backends::Accelerator::Cpu];
    let hold = |steps: &[OrderStep]| {
        crate::host::order_summary_hold(&svc.store(), steps, &accel, &hardware)
    };
    let filtered = |o: &[OrderStep], privacy| order(o).filtered(privacy);
    // No local step: nothing held here.
    assert_eq!(
        hold(&filtered(
            &[OrderStep::Computer(DESK), CLAUDE],
            SummaryPrivacy::Writer
        )),
        SummaryHold::OffThisComputer
    );
    // A local step anywhere: the local model is held.
    for o in [
        &[OrderStep::Computer(DESK), LOCAL][..],
        &[CLAUDE, ENDPOINT, LOCAL][..],
    ] {
        assert!(
            matches!(
                hold(&filtered(o, SummaryPrivacy::Writer)),
                SummaryHold::Local { .. }
            ),
            "{o:?}"
        );
    }
    // A kept meeting's implicit local step counts.
    assert!(matches!(
        hold(&filtered(&[CLAUDE], SummaryPrivacy::KeptLocal)),
        SummaryHold::Local { .. }
    ));
    // Live summaries: local first, or a computer then local.
    let live = |o: &[OrderStep], privacy| {
        crate::summary_order::live_summaries_allowed(&filtered(o, privacy))
    };
    assert!(live(
        &[OrderStep::Computer(DESK), LOCAL],
        SummaryPrivacy::Writer
    ));
    assert!(!live(&[CLAUDE, LOCAL], SummaryPrivacy::Writer));
    assert!(!live(
        &[OrderStep::Computer(DESK), CLAUDE, LOCAL],
        SummaryPrivacy::Writer
    ));
    assert!(live(&[CLAUDE, LOCAL], SummaryPrivacy::KeptLocal));
}
