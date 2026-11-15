//! T11.3db: summary offload in the app: its copy against the shared table, the Summaries
//! page's list and choice, re-summarise "On <device>", the meeting's waiting line and banner,
//! and the host the sync thread hands work to. Every name holds `offload`
//! (`cargo test -p earshot-desktop offload`).

use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use earshot_core::asr::{CancelToken, MeetingTime, Segment, Word};
use earshot_core::capture::TrackId;
use earshot_core::identity::EntityId;
use earshot_core::model::{
    ClockDomain, Meeting, MeetingRepo, RequestState, SegmentRow, Stamp, SummaryRequestRepo, Track,
    TranscriptPass,
};
use earshot_core::prompt::Template;
use earshot_runtime::host::{HostPost, SkipReason, SummaryResult};
use earshot_runtime::offload::{HereReason, OffloadChoice, Unavailable, WriterKind};
use earshot_runtime::sync::{MemoryVault, SyncClient, SyncTiming, SyncVault};
use earshot_sync_server::{Config, LocalRelay, Relay};
use rand_chacha::ChaCha8Rng;
use rand_core::SeedableRng;

use super::*;
use crate::jobs::{JobEvent, JobKind, JobRunner, JobSink, JobWork};
use crate::testutil::{T0, TempDir, services};

const SECRET: &str = "an-invented-setup-secret-for-offload-tests";

fn table() -> serde_json::Value {
    serde_json::from_str(include_str!(
        "../../../../fixtures/ui/offload_copy_cases.json"
    ))
    .unwrap()
}

const EVERY_UNAVAILABLE: [Unavailable; 8] = [
    Unavailable::SyncOff,
    Unavailable::Unreachable,
    Unavailable::Offline,
    Unavailable::NotOffering,
    Unavailable::NoWriter,
    Unavailable::Recording,
    Unavailable::NoFinalTranscript,
    Unavailable::NotAsked,
];

fn every_here() -> [HereReason; 4] {
    [
        HereReason::NotClaimed,
        HereReason::HelperStale,
        HereReason::Declined("failed".into()),
        HereReason::Withdrawn,
    ]
}

#[test]
fn offload_copy_matches_the_shared_table() {
    let t = table();
    let writers = t["writers"].as_object().unwrap();
    assert_eq!(writers.len(), WriterKind::ALL.len());
    for k in WriterKind::ALL {
        assert_eq!(writers[k.word()].as_str(), Some(writer_label(k)), "{k:?}");
    }
    let unavailable = t["unavailable"].as_object().unwrap();
    assert_eq!(unavailable.len(), EVERY_UNAVAILABLE.len());
    for why in EVERY_UNAVAILABLE {
        assert_eq!(
            unavailable[unavailable_word(why)].as_str(),
            Some(unavailable_template(why)),
            "{why:?}"
        );
    }
    let here = t["written_here"].as_object().unwrap();
    assert_eq!(here.len(), every_here().len());
    for why in every_here() {
        assert_eq!(
            here[here_word(&why)].as_str(),
            Some(written_here_template(&why)),
            "{why:?}"
        );
    }
    assert_eq!(t["waiting"].as_str(), Some(WAITING_TEMPLATE));
    assert_eq!(t["unnamed"].as_str(), Some(UNNAMED));
    assert_eq!(t["unnamed_inline"].as_str(), Some(UNNAMED_INLINE));
    // A name fills the sentence; without one the copy says "your other computer".
    assert_eq!(
        unavailable_copy(Unavailable::Offline, Some("Invented desk")),
        "Invented desk is not online, so this computer wrote the summary."
    );
    assert_eq!(
        unavailable_copy(Unavailable::Offline, None),
        "Your other computer is not online, so this computer wrote the summary."
    );
    assert_eq!(
        waiting_copy(None),
        "Waiting for your other computer to write the summary."
    );
    assert_eq!(
        not_asked_copy(Unavailable::Recording, Some("Invented desk")),
        "Invented desk is recording a meeting."
    );
    for why in EVERY_UNAVAILABLE {
        let c = not_asked_copy(why, Some("D"));
        assert!(!c.contains("wrote the summary"), "{c}");
    }
}

#[test]
fn offload_banner_is_shown_only_when_this_computer_wrote_instead() {
    let device = earshot_core::identity::DeviceId::from_bytes([4; 16]);
    assert_eq!(offload_banner(&OffloadChoice::Here), None);
    assert_eq!(
        offload_banner(&OffloadChoice::Ask {
            device,
            name: "D".into(),
            writer: WriterKind::Local,
        }),
        None
    );
    let Some(MeetingEvent::Warning { key, message, .. }) =
        offload_banner(&OffloadChoice::HereInstead {
            device,
            name: Some("Invented desk".into()),
            why: Unavailable::NotOffering,
        })
    else {
        panic!("a banner");
    };
    assert_eq!(key, OFFLOAD_BANNER_KEY);
    assert_eq!(
        message,
        unavailable_copy(Unavailable::NotOffering, Some("Invented desk"))
    );
}

#[test]
fn offload_an_asked_summary_shows_the_waiting_line() {
    let event = crate::meeting_events::summary_result_event(
        &SummaryResult::Offloaded {
            request: EntityId::from_bytes([1; 16]),
            device: earshot_core::identity::DeviceId::from_bytes([2; 16]),
            name: "Invented desk".into(),
            writer: WriterKind::ClaudeCode,
        },
        0,
    );
    assert_eq!(
        event,
        crate::meeting_events::SummaryEvent::Offloaded {
            message: "Waiting for Invented desk to write the summary.".into()
        }
    );
    let _ = HostPost::Skipped;
    let _ = SkipReason::NotRequested;
}

// ---- against the relay ----

struct TestRelay {
    relay: LocalRelay,
    _dir: TempDir,
}

fn relay() -> TestRelay {
    let dir = TempDir::new("offload-page-relay");
    let config = Config {
        setup_secret: SECRET.into(),
        data: dir.0.join("relay"),
        listen: "127.0.0.1:0".parse().unwrap(),
    };
    TestRelay {
        relay: LocalRelay::start(Relay::open(&config).unwrap()).unwrap(),
        _dir: dir,
    }
}

struct Computer {
    svc: Services,
    host: SyncHost,
    vault: Arc<MemoryVault>,
    _dir: TempDir,
}

fn computer(tag: &str) -> Computer {
    let dir = TempDir::new(tag);
    let svc = services(&dir);
    let vault = Arc::new(MemoryVault::default());
    let timing = SyncTiming {
        tick: Duration::from_millis(100),
        poll: Duration::from_millis(10),
        presence: Duration::from_secs(3600),
    };
    let host = SyncHost::with_timing(Arc::clone(&vault) as Arc<dyn SyncVault>, timing);
    Computer {
        svc,
        host,
        vault,
        _dir: dir,
    }
}

impl Computer {
    fn client(&self) -> SyncClient {
        SyncClient::new(
            self.svc.clone(),
            Arc::clone(&self.vault) as Arc<dyn SyncVault>,
        )
        .unwrap()
    }
    fn id(&self) -> earshot_core::identity::DeviceId {
        self.svc.store().device()
    }
}

/// Two computers in one vault (no sync thread: the tests tick by hand), sync on.
fn pair(relay: &TestRelay) -> (Computer, Computer) {
    let a = computer("offload-page-a");
    let b = computer("offload-page-b");
    let url = relay.relay.url();
    earshot_runtime::sync::set_up_first_device(&a.svc, &*a.vault, &url, SECRET).unwrap();
    let code = earshot_runtime::sync::offer_pairing_code(
        &a.svc,
        Arc::clone(&a.vault) as Arc<dyn SyncVault>,
    )
    .unwrap();
    earshot_runtime::sync::join_with_code(&b.svc, &*b.vault, &url, code.code()).unwrap();
    for c in [&a, &b] {
        earshot_runtime::sync::set_sync_on(&c.svc, true).unwrap();
    }
    (a, b)
}

/// A finished meeting with `n` final segments.
fn meeting_with_final(svc: &Services, seed: u64, n: usize) -> EntityId {
    let clock = Arc::clone(&svc.clock);
    let mut rng = ChaCha8Rng::seed_from_u64(seed);
    let mut db = svc.store();
    let (store, hlc) = db.parts();
    let mut stamp = Stamp {
        clock: &*clock,
        hlc,
        rng: &mut rng,
    };
    let mut m = Meeting::new("Invented review", T0, 600, Template::General, &mut stamp);
    m.duration_ms = Some(60_000);
    let track = Track::new(m.id, TrackId::Mic, None, ClockDomain::Replay, &mut stamp);
    let rows: Vec<SegmentRow> = (0..n)
        .map(|i| {
            let start = 1_000 * i as u64;
            let seg = Segment::from_words(
                EntityId::generate(stamp.clock, &mut *stamp.rng),
                track.id,
                vec![Word::new("invented", start, start + 300, 0.9)],
                MeetingTime::ZERO,
            );
            SegmentRow::new(m.id, TranscriptPass::Final, seg, stamp.hlc, stamp.clock)
        })
        .collect();
    let id = m.id;
    store.put_meeting(m).unwrap();
    store.put_track(track).unwrap();
    for r in rows {
        store.put_segment(r).unwrap();
    }
    id
}

#[test]
fn offload_helpers_list_offering_computers_and_keep_the_choice() {
    let relay = relay();
    let (a, b) = pair(&relay);
    // Without presence from the other computer it is not listed (it offers nothing yet).
    let view = helpers_view(&a.svc, &a.host).unwrap();
    assert!(view.sync_ready);
    assert!(view.helpers.is_empty(), "{view:?}");
    // It offers summaries and says so.
    earshot_runtime::sync::set_offer_summaries(&b.svc, true).unwrap();
    b.client().send_presence(false).unwrap();
    let view = helpers_view(&a.svc, &a.host).unwrap();
    assert_eq!(view.helpers.len(), 1, "{view:?}");
    let h = &view.helpers[0];
    assert_eq!(h.id, b.id().to_string());
    assert_eq!(
        h.writer.as_deref(),
        Some("local model"),
        "the default writer"
    );
    assert!(h.available && h.online && h.offers && !h.chosen);
    // Chosen: it stays listed as chosen, also after it stops offering.
    let chosen = set_helper(&a.svc, &a.host, Some(&h.id)).unwrap();
    assert_eq!(chosen.chosen.as_deref(), Some(h.id.as_str()));
    earshot_runtime::sync::set_offer_summaries(&b.svc, false).unwrap();
    b.client().send_presence(false).unwrap();
    let view = helpers_view(&a.svc, &a.host).unwrap();
    assert_eq!(view.helpers.len(), 1);
    assert!(view.helpers[0].chosen && !view.helpers[0].available && !view.helpers[0].offers);
    assert_eq!(view.helpers[0].writer, None);
    // This computer cannot be its own helper; an id that is not a device's is refused.
    assert!(set_helper(&a.svc, &a.host, Some(&a.id().to_string())).is_err());
    assert!(set_helper(&a.svc, &a.host, Some("not-an-id")).is_err());
    // None goes back to this computer's writer.
    let none = set_helper(&a.svc, &a.host, None).unwrap();
    assert_eq!(none.chosen, None);
    // Sync off: nothing is asked of the server; the chosen one still shows.
    set_helper(&a.svc, &a.host, Some(&b.id().to_string())).unwrap();
    earshot_runtime::sync::set_sync_on(&a.svc, false).unwrap();
    let off = helpers_view(&a.svc, &a.host).unwrap();
    assert!(!off.sync_ready);
    assert_eq!(off.helpers.len(), 1);
    assert!(off.helpers[0].chosen && off.helpers[0].name.is_none());
}

#[test]
fn offload_resummarise_on_another_computer_asks_it_only_when_it_would_be_asked() {
    let relay = relay();
    let (a, b) = pair(&relay);
    let m = meeting_with_final(&a.svc, 7, 3);
    let (mid, bid) = (m.to_string(), b.id().to_string());
    // Not offering: refused with the reason, nothing written.
    let refused = ask_helper(&a.svc, &a.host, &mid, &bid).unwrap_err();
    assert_eq!(refused.kind, OFFLOAD_ERROR_KIND);
    assert!(
        refused.message.contains("not online"),
        "{}",
        refused.message
    );
    assert!(a.svc.store().summary_requests().unwrap().is_empty());
    // Offering: asked, and the meeting shows it waits.
    earshot_runtime::sync::set_offer_summaries(&b.svc, true).unwrap();
    earshot_runtime::sync::set_device_name(&b.svc, "Invented desk").unwrap();
    b.client().send_presence(false).unwrap();
    let line = ask_helper(&a.svc, &a.host, &mid, &bid).unwrap();
    assert_eq!(line, "Waiting for Invented desk to write the summary.");
    let requests = a.svc.store().summary_requests().unwrap();
    assert_eq!(requests.len(), 1);
    assert_eq!(
        (
            requests[0].state,
            requests[0].segment_count,
            requests[0].target_device
        ),
        (RequestState::Requested, 3, b.id())
    );
    let detail = crate::detail::meeting_detail(&a.svc, m).unwrap();
    assert_eq!(
        detail.waiting_for.as_deref(),
        Some("Waiting for your other computer to write the summary.")
    );
    // A meeting in use is refused (its own summary step decides).
    let lease = a
        .svc
        .lease_meeting_for(m, earshot_runtime::MeetingJob::Processing)
        .unwrap();
    assert!(ask_helper(&a.svc, &a.host, &mid, &bid).is_err());
    drop(lease);
    // A meeting with no final transcript is not sent.
    let bare = meeting_with_final(&a.svc, 8, 0);
    let none = ask_helper(&a.svc, &a.host, &bare.to_string(), &bid).unwrap_err();
    assert!(
        none.message.contains("no final transcript"),
        "{}",
        none.message
    );
}

#[test]
fn offload_sync_page_offers_summaries_and_names_the_computer() {
    let relay = relay();
    let (a, _b) = pair(&relay);
    let v = crate::sync_settings::view(&a.svc, &a.host).unwrap();
    assert!(!v.offers_summaries, "off by default");
    assert_eq!(v.device_name, earshot_runtime::sync::computer_name());
    let v = crate::sync_settings::set_offer_summaries(&a.svc, &a.host, true).unwrap();
    assert!(v.offers_summaries);
    let v = crate::sync_settings::set_device_name(&a.svc, &a.host, "  Invented laptop ").unwrap();
    assert_eq!(v.device_name, "Invented laptop");
    let long = "x".repeat(crate::sync_settings::MAX_DEVICE_NAME + 1);
    let refused = crate::sync_settings::set_device_name(&a.svc, &a.host, &long).unwrap_err();
    assert_eq!(refused.message, crate::sync_settings::DEVICE_NAME_TOO_LONG);
    let v = crate::sync_settings::set_device_name(&a.svc, &a.host, "").unwrap();
    assert_eq!(
        v.device_name,
        earshot_runtime::sync::computer_name(),
        "empty: the computer's own"
    );
}

// ---- the host the sync thread hands work to ----

struct QuickJobs;

impl JobWork for QuickJobs {
    fn retranscribe(
        &self,
        _: &Services,
        _: EntityId,
        _: &CancelToken,
        _: &mut dyn FnMut(earshot_runtime::host::HostStep<'_>),
        _: &mut dyn FnMut(String),
    ) -> Result<earshot_runtime::edit::Retranscribed, IpcError> {
        unreachable!("only re-summaries here")
    }

    fn summarise(
        &self,
        _: &Services,
        _: EntityId,
        _: &CancelToken,
        _: &mut dyn FnMut(String),
    ) -> Result<SummaryResult, IpcError> {
        Ok(SummaryResult::Skipped {
            reason: SkipReason::NotRequested,
        })
    }
}

/// A helper with no writer: every request declines `no_writer`.
struct NoWriter;

impl HelperWork for NoWriter {
    fn writer(&self, _: &Services, _: SummaryPrivacy) -> HelperWriter {
        HelperWriter::Decline(reason::NO_WRITER)
    }
}

fn jobs() -> (JobRunner, Receiver<JobEvent>) {
    let (tx, rx) = mpsc::channel();
    let tx = Mutex::new(tx);
    let sink: JobSink = Arc::new(move |e| {
        let _ = tx.lock().unwrap().send(e);
    });
    (
        JobRunner::new(
            Arc::new(QuickJobs),
            sink,
            crate::activity::WorkGate::new(),
            &crate::cuda_settings::CudaLatch::resolved_off(),
        ),
        rx,
    )
}

#[test]
fn offload_host_writes_a_handed_back_meeting_here_with_its_reason() {
    let dir = TempDir::new("offload-host-here");
    let svc = services(&dir);
    let m = meeting_with_final(&svc, 9, 1);
    let (runner, events) = jobs();
    let host = HostOffload::new(
        svc.clone(),
        runner,
        MeetingQueue::new(),
        crate::activity::WorkGate::new(),
        Arc::new(NoWriter),
    );
    host.write_here(WriteHere {
        meeting: m,
        request: EntityId::from_bytes([5; 16]),
        helper: earshot_core::identity::DeviceId::from_bytes([6; 16]),
        why: HereReason::NotClaimed,
    });
    let first = events.recv_timeout(Duration::from_secs(30)).unwrap();
    assert_eq!(
        first,
        JobEvent::Warning {
            meeting: m.to_string(),
            message: written_here_copy(&HereReason::NotClaimed, None),
        }
    );
    let last = events.recv_timeout(Duration::from_secs(30)).unwrap();
    assert!(
        matches!(last, JobEvent::Summary { .. }),
        "the re-summary ran: {last:?}"
    );
    let _ = JobKind::Resummarise;
}

#[test]
fn offload_host_declines_a_request_it_has_no_writer_for() {
    let relay = relay();
    let (a, b) = pair(&relay);
    earshot_runtime::sync::set_offer_summaries(&b.svc, true).unwrap();
    let m = meeting_with_final(&a.svc, 10, 2);
    let request = earshot_runtime::offload::ask(&a.svc, m, b.id())
        .unwrap()
        .unwrap();
    let (ca, cb) = (a.client(), b.client());
    ca.tick().unwrap();
    cb.tick().unwrap();
    cb.tick().unwrap();
    let (runner, _events) = jobs();
    let host: Arc<dyn OffloadHost> = Arc::new(HostOffload::new(
        b.svc.clone(),
        runner,
        MeetingQueue::new(),
        crate::activity::WorkGate::new(),
        Arc::new(NoWriter),
    ));
    let state = earshot_runtime::offload::Offload::new();
    let r = state.step(&b.svc, &mut || None, &*host).unwrap();
    assert_eq!((r.claimed, r.handed), (1, 1), "{r:?}");
    // The write runs on its own thread; it ends declined.
    let deadline = std::time::Instant::now() + Duration::from_secs(60);
    loop {
        let got = b.svc.store().summary_request(request).unwrap().unwrap();
        if got.state == RequestState::Declined {
            assert_eq!(got.reason, reason::NO_WRITER);
            break;
        }
        assert!(std::time::Instant::now() < deadline, "never declined");
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn offload_writer_of_a_setup_declines_without_a_service() {
    let setup = |no_summary| crate::meeting::SummarySetup {
        summary: None,
        no_summary,
        notes: Vec::new(),
        live: false,
        local_chosen: false,
        chosen_missing: None,
    };
    assert!(matches!(
        writer_of(Ok(setup(SkipReason::KeptLocalNoModel {
            model: "qwen3.5-4b-q4_k_m"
        }))),
        HelperWriter::Decline(reason::KEPT_LOCAL_NO_MODEL)
    ));
    assert!(matches!(
        writer_of(Ok(setup(SkipReason::NotRequested))),
        HelperWriter::Decline(reason::NO_WRITER)
    ));
    assert!(matches!(
        writer_of(Err(IpcError::internal())),
        HelperWriter::Decline(reason::NO_WRITER)
    ));
}
