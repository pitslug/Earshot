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
use earshot_runtime::sync::{MemoryVault, ModelPlace, SyncClient, SyncTiming, SyncVault};
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
    // T11.6a: a summary worker's own local model by where it runs, keyed by the presence word.
    let places = t["local_model_places"].as_object().unwrap();
    assert_eq!(places.len(), ModelPlace::ALL.len());
    for p in ModelPlace::ALL {
        assert_eq!(
            places[&p.word().replace('-', "_")].as_str(),
            Some(local_model_place_label(p)),
            "{p:?}"
        );
        assert_eq!(
            computer_writer_label(WriterKind::Local, Some(p)),
            local_model_place_label(p)
        );
        // A place never labels another writer kind.
        for k in WriterKind::ALL
            .into_iter()
            .filter(|k| *k != WriterKind::Local)
        {
            assert_eq!(computer_writer_label(k, Some(p)), writer_label(k), "{k:?}");
        }
    }
    for k in WriterKind::ALL {
        assert_eq!(computer_writer_label(k, None), writer_label(k), "{k:?}");
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
    let not_asked = t["not_asked"].as_object().unwrap();
    assert_eq!(not_asked.len(), EVERY_UNAVAILABLE.len());
    for why in EVERY_UNAVAILABLE {
        assert_eq!(
            not_asked[unavailable_word(why)].as_str(),
            Some(not_asked_template(why)),
            "{why:?}"
        );
        // Claims nothing about a summary.
        assert!(!not_asked_template(why).contains("summary."), "{why:?}");
    }
    assert_eq!(t["already_waiting"].as_str(), Some(ALREADY_WAITING));
    // The job's first warning is sent before the job runs: it never says the summary was written.
    for why in every_here() {
        assert!(!written_here_template(&why).contains("wrote"), "{why:?}");
    }
    assert_eq!(t["waiting"].as_str(), Some(WAITING_TEMPLATE));
    let after = t["if_no_answer"].as_object().unwrap();
    assert_eq!(after.len(), 3);
    for (word, then) in [
        ("writes_here", IfNoAnswer::WritesHere),
        ("next_computer", IfNoAnswer::NextComputer),
        ("nothing", IfNoAnswer::Nothing),
    ] {
        assert_eq!(
            after[word].as_str(),
            Some(if_no_answer_copy(then)),
            "{word}"
        );
    }
    assert_eq!(
        waiting_banner(Some("Invented desk"), IfNoAnswer::WritesHere),
        "Waiting for Invented desk to write the summary. If it does not answer, this computer writes it."
    );
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
        "Invented desk is recording a meeting, so it was not asked."
    );
    for why in EVERY_UNAVAILABLE {
        let c = not_asked_copy(why, Some("D"));
        assert!(!c.contains("wrote the summary"), "{c}");
    }
}

fn summary_reply() -> earshot_llm_replay::mock::MockResponse {
    let summary = earshot_core::summary::SummaryV1 {
        title: "Invented title".into(),
        tldr: "An invented summary.".into(),
        enhanced_notes: vec![],
        decisions: vec![],
        action_items: vec![],
        open_questions: vec![],
        topics: vec![],
    };
    earshot_llm_replay::mock::MockResponse::completion(
        &serde_json::to_string(&summary).unwrap(),
        "stop",
    )
}

/// The waiting banner says what follows the computer step (T12.81): the first step after it not
/// yet tried, nothing after a named run, and this computer for a request from before the order.
#[test]
fn offload_waiting_banner_says_what_happens_if_the_computer_does_not_answer() {
    use earshot_runtime::summary_order::OrderStep;
    use earshot_runtime::summary_order::run::{
        RunOrigin, RunState, RunView, StepOutcome, SummaryRun,
    };
    let desk = earshot_core::identity::DeviceId::from_bytes([4; 16]);
    let laptop = earshot_core::identity::DeviceId::from_bytes([5; 16]);
    let waiting = |origin, steps: Vec<OrderStep>, cursor, outcomes| {
        RunView::Waiting(SummaryRun {
            run: 1,
            origin,
            steps,
            cursor,
            outcomes,
            waiting_on: None,
            state: RunState::Waiting,
            writer: None,
            stopped: None,
        })
    };
    let order = vec![
        OrderStep::Computer(desk),
        OrderStep::Computer(laptop),
        OrderStep::LOCAL,
    ];
    assert_eq!(
        if_no_answer(&waiting(RunOrigin::Order, order.clone(), 1, vec![])),
        IfNoAnswer::NextComputer
    );
    // Waiting on the second computer: the local model follows.
    assert_eq!(
        if_no_answer(&waiting(RunOrigin::Order, order.clone(), 2, vec![])),
        IfNoAnswer::WritesHere
    );
    // A later step already tried (dropped) is passed over.
    let tried = vec![StepOutcome {
        step: 2,
        word: earshot_runtime::summary_order::run::OutcomeWord::Offline,
    }];
    assert_eq!(
        if_no_answer(&waiting(
            RunOrigin::Order,
            vec![
                OrderStep::Computer(desk),
                OrderStep::LOCAL,
                OrderStep::Computer(laptop)
            ],
            1,
            vec![StepOutcome {
                step: 1,
                word: earshot_runtime::summary_order::run::OutcomeWord::Offline,
            }],
        )),
        IfNoAnswer::NextComputer
    );
    assert_eq!(
        if_no_answer(&waiting(RunOrigin::Order, order.clone(), 3, tried)),
        IfNoAnswer::Nothing
    );
    // The order's only step, and a named computer: nothing follows.
    assert_eq!(
        if_no_answer(&waiting(
            RunOrigin::Order,
            vec![OrderStep::Computer(desk)],
            1,
            vec![]
        )),
        IfNoAnswer::Nothing
    );
    assert_eq!(
        if_no_answer(&waiting(
            RunOrigin::Named,
            vec![OrderStep::Computer(desk)],
            1,
            vec![]
        )),
        IfNoAnswer::Nothing
    );
    // No run waits: a request from before the order, whose hand-back writes here.
    assert_eq!(if_no_answer(&RunView::None), IfNoAnswer::WritesHere);
    // A record that cannot be read promises nothing.
    assert_eq!(if_no_answer(&RunView::Unreadable), IfNoAnswer::Nothing);
}

/// The banner says this computer wrote the summary only when one was stored; for a summary
/// skipped, failed or cancelled it claims nothing (B1).
#[test]
fn offload_banner_is_shown_only_when_this_computer_wrote_instead() {
    let device = earshot_core::identity::DeviceId::from_bytes([4; 16]);
    let dir = TempDir::new("offload-banner");
    let svc = services(&dir);
    let m = meeting_with_final(&svc, 3, 2);
    let server = earshot_llm_replay::mock::MockServer::start(vec![summary_reply()]).unwrap();
    let service = earshot_runtime::summary::SummaryService::new(
        &earshot_runtime::summary::SummaryConfig::new(server.endpoint().as_str(), "test-model"),
        svc.clock.clone(),
    )
    .unwrap();
    let stored = earshot_runtime::host::run_summary(&svc, m, &service, &CancelToken::new());
    assert!(matches!(stored, SummaryResult::Stored { .. }), "{stored:?}");
    let skipped = SummaryResult::Skipped {
        reason: SkipReason::KeptLocalNoModel {
            model: "qwen3.5-4b-q4_k_m",
        },
    };
    let failed = SummaryResult::Failed {
        error: earshot_runtime::RuntimeError::Internal("an invented failure"),
        retryable: false,
    };
    let instead = OffloadChoice::HereInstead {
        device,
        name: Some("Invented desk".into()),
        why: Unavailable::NotOffering,
    };
    for result in [&stored, &skipped, &failed, &SummaryResult::Cancelled] {
        assert_eq!(offload_banner(&OffloadChoice::Here, result), None);
        assert_eq!(
            offload_banner(
                &OffloadChoice::Ask {
                    device,
                    name: "D".into(),
                    writer: WriterKind::Local,
                },
                result
            ),
            None
        );
        let Some(MeetingEvent::Warning { key, message, .. }) = offload_banner(&instead, result)
        else {
            panic!("a banner");
        };
        assert_eq!(key, OFFLOAD_BANNER_KEY);
        let wrote = matches!(result, SummaryResult::Stored { .. });
        assert_eq!(
            message,
            if wrote {
                unavailable_copy(Unavailable::NotOffering, Some("Invented desk"))
            } else {
                not_asked_copy(Unavailable::NotOffering, Some("Invented desk"))
            },
            "{result:?}"
        );
        assert_eq!(message.contains("wrote the summary"), wrote, "{message}");
    }
}

#[test]
fn offload_an_asked_summary_shows_the_waiting_line() {
    let event = crate::meeting_events::summary_result_event(
        &SummaryResult::Offloaded {
            request: EntityId::from_bytes([1; 16]),
            device: earshot_core::identity::DeviceId::from_bytes([2; 16]),
            name: "Invented desk".into(),
            writer: Some(WriterKind::ClaudeCode),
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

/// T11.6a: a summary worker whose presence says where its own local model runs is listed with
/// that place on the Summaries page, the Sync page and the order's step label; the app's own
/// presence never carries one, and a remote endpoint is never labelled with it.
#[test]
fn offload_a_worker_is_listed_with_where_its_local_model_runs() {
    let relay = relay();
    let (a, b) = pair(&relay);
    earshot_runtime::sync::set_offer_summaries(&b.svc, true).unwrap();
    earshot_runtime::sync::set_device_name(&b.svc, "Invented rack").unwrap();
    let local = Arc::new(std::sync::atomic::AtomicBool::new(true));
    let now = Arc::clone(&local);
    let worker = SyncClient::for_address(
        b.svc.clone(),
        Arc::clone(&b.vault) as Arc<dyn SyncVault>,
        &relay.relay.url(),
    )
    .unwrap()
    .as_summary_worker(Arc::new(move || {
        Some(if now.load(std::sync::atomic::Ordering::SeqCst) {
            WriterKind::Local
        } else {
            WriterKind::OpenaiCompatible
        })
    }))
    .with_model_place(Some(ModelPlace::GraphicsCard));
    worker.send_presence(false).unwrap();
    let view = helpers_view(&a.svc, &a.host).unwrap();
    assert_eq!(view.helpers.len(), 1, "{view:?}");
    assert_eq!(
        view.helpers[0].writer.as_deref(),
        Some("local model, graphics card")
    );
    let devices = crate::sync_settings::devices(&a.svc, &a.host).unwrap();
    let rack = devices.iter().find(|d| !d.this_device).unwrap();
    assert_eq!(rack.writer.as_deref(), Some("local model, graphics card"));
    // The order's step label reads what this process heard.
    let cx = crate::summary_order::StepContext::read(&a.svc);
    let step = earshot_runtime::summary_order::OrderStep::Computer(b.id());
    assert_eq!(
        crate::summary_order::step_label(step, &cx),
        "On Invented rack (local model, graphics card)"
    );
    // Its endpoint becomes remote: the place goes with it.
    local.store(false, std::sync::atomic::Ordering::SeqCst);
    worker.send_presence(false).unwrap();
    let view = helpers_view(&a.svc, &a.host).unwrap();
    assert_eq!(view.helpers[0].writer.as_deref(), Some("own server"));
    let cx = crate::summary_order::StepContext::read(&a.svc);
    assert_eq!(
        crate::summary_order::step_label(step, &cx),
        "On Invented rack (own server)"
    );
    // The app's presence (a computer, not a worker) carries no place: "local model" alone.
    b.client().send_presence(false).unwrap();
    let view = helpers_view(&a.svc, &a.host).unwrap();
    assert_eq!(view.helpers[0].writer.as_deref(), Some("local model"));
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
    // The meeting's one banner names the computer and says what follows (T12.81): a named run
    // writes nothing here if it does not answer.
    let detail = crate::detail::meeting_detail(&a.svc, m).unwrap();
    assert_eq!(
        detail.waiting_for.as_deref(),
        Some(
            "Waiting for Invented desk to write the summary. If it does not answer, no summary is written. Re-summarise to try again."
        )
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
    host.continue_run(WriteHere {
        meeting: m,
        request: EntityId::from_bytes([5; 16]),
        helper: earshot_core::identity::DeviceId::from_bytes([6; 16]),
        why: HereReason::NotClaimed,
        continuation: earshot_runtime::offload::Continuation::Legacy,
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

/// A Claude Code probe that must never be asked.
struct NeverClaude;
impl earshot_runtime::claude_code::ClaudeCodeProbe for NeverClaude {
    fn status(&self) -> earshot_runtime::claude_code::ClaudeCodeStatus {
        panic!("the helper asked a writer its presence did not advertise")
    }
}

/// A Codex probe that must never be asked.
struct NeverCodex;
impl earshot_runtime::codex::CodexProbe for NeverCodex {
    fn status(&self) -> earshot_runtime::codex::CodexStatus {
        panic!("the helper asked a writer its presence did not advertise")
    }
}

/// A Claude Code probe that counts its calls and answers "not installed".
#[derive(Default)]
struct CountedClaude(std::sync::atomic::AtomicU32);
impl earshot_runtime::claude_code::ClaudeCodeProbe for CountedClaude {
    fn status(&self) -> earshot_runtime::claude_code::ClaudeCodeStatus {
        self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        earshot_runtime::claude_code::ClaudeCodeStatus::NotInstalled
    }
}

fn store_order(svc: &Services, steps: &[earshot_runtime::summary_order::OrderStep]) {
    earshot_runtime::summary_order::set_summary_order(
        &mut svc.store(),
        &earshot_runtime::summary_order::SummaryOrder::new(steps.to_vec()).unwrap(),
        None,
    )
    .unwrap();
}

/// The app's helper answers another computer's request through `helper_writer` only (T12.63b,
/// docs/summary-order.md 4.6; replaces T11.3db's `writer_of` over the one-writer setup): exactly
/// the writer kind its presence advertised, the first writer step of its own order, and never a
/// later step.
#[test]
fn offload_host_helper_runs_only_the_writer_its_presence_advertised() {
    use earshot_runtime::summary_order::{OrderStep, WriterStep};
    let dir = TempDir::new("offload-helper-writer");
    let svc = services(&dir);
    let models = earshot_runtime::backends::ModelDir::new(dir.0.join("no-models"));
    let never = |models: &earshot_runtime::backends::ModelDir| HostHelperWork {
        probes: crate::summary_order::SummaryProbes {
            keys: Arc::new(crate::credentials::MemoryVault::default()),
            claude: Arc::new(NeverClaude),
            codex: Arc::new(NeverCodex),
        },
        models: Some(models.clone()),
    };
    // Its own order is [local, Claude Code] and no local model is installed: it declines, and
    // Claude Code is never asked.
    store_order(
        &svc,
        &[OrderStep::LOCAL, OrderStep::Writer(WriterStep::ClaudeCode)],
    );
    assert!(matches!(
        never(&models).writer(&svc, SummaryPrivacy::Writer),
        HelperWriter::Decline(reason::NO_WRITER)
    ));
    // A meeting kept on its asker's computer turns a remote kind local: no local model declines
    // `kept_local_no_model`, and Claude Code is never asked.
    store_order(&svc, &[OrderStep::Writer(WriterStep::ClaudeCode)]);
    assert!(matches!(
        never(&models).writer(&svc, SummaryPrivacy::KeptLocal),
        HelperWriter::Decline(reason::KEPT_LOCAL_NO_MODEL)
    ));
    // No writer in its order (a computer only): it declines `no_writer`.
    store_order(
        &svc,
        &[OrderStep::Computer(
            earshot_core::identity::DeviceId::from_bytes([6; 16]),
        )],
    );
    assert!(matches!(
        never(&models).writer(&svc, SummaryPrivacy::Writer),
        HelperWriter::Decline(reason::NO_WRITER)
    ));
    // The negative control: with Claude Code first it is the writer asked, once.
    store_order(&svc, &[OrderStep::Writer(WriterStep::ClaudeCode)]);
    let claude = Arc::new(CountedClaude::default());
    let work = HostHelperWork {
        probes: crate::summary_order::SummaryProbes {
            keys: Arc::new(crate::credentials::MemoryVault::default()),
            claude: Arc::clone(&claude) as Arc<_>,
            codex: Arc::new(NeverCodex),
        },
        models: Some(models),
    };
    assert!(matches!(
        work.writer(&svc, SummaryPrivacy::Writer),
        HelperWriter::Decline(reason::NO_WRITER)
    ));
    assert_eq!(claude.0.load(std::sync::atomic::Ordering::SeqCst), 1);
}

/// A write here the job runner refuses (the meeting is in use) says so, so the step hands it
/// again (A1).
#[test]
fn offload_host_says_when_a_write_here_did_not_start() {
    let dir = TempDir::new("offload-host-refused");
    let svc = services(&dir);
    let m = meeting_with_final(&svc, 11, 1);
    let (runner, _events) = jobs();
    let host = HostOffload::new(
        svc.clone(),
        runner,
        MeetingQueue::new(),
        crate::activity::WorkGate::new(),
        Arc::new(NoWriter),
    );
    let job = || WriteHere {
        meeting: m,
        request: EntityId::from_bytes([5; 16]),
        helper: earshot_core::identity::DeviceId::from_bytes([6; 16]),
        why: HereReason::Withdrawn,
        continuation: earshot_runtime::offload::Continuation::Legacy,
    };
    let lease = svc
        .lease_meeting_for(m, earshot_runtime::MeetingJob::Processing)
        .unwrap();
    assert!(
        host.continue_run(job()) == earshot_runtime::offload::ContinueAnswer::Refused,
        "refused while the meeting is in use"
    );
    drop(lease);
    assert_eq!(
        host.continue_run(job()),
        earshot_runtime::offload::ContinueAnswer::Started,
        "started once free"
    );
}

/// A helper whose work panics declines the request `failed`, so the asker writes it (A2).
#[test]
fn offload_host_declines_when_its_work_panics() {
    struct Panics;
    impl HelperWork for Panics {
        fn writer(&self, _: &Services, _: SummaryPrivacy) -> HelperWriter {
            panic!("an invented bug in the writer");
        }
    }
    let relay = relay();
    let (a, b) = pair(&relay);
    earshot_runtime::sync::set_offer_summaries(&b.svc, true).unwrap();
    let m = meeting_with_final(&a.svc, 12, 2);
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
        Arc::new(Panics),
    ));
    let state = earshot_runtime::offload::Offload::new();
    let r = state.step(&b.svc, &mut || None, &*host).unwrap();
    assert_eq!(r.handed, 1, "{r:?}");
    let deadline = std::time::Instant::now() + Duration::from_secs(60);
    loop {
        let got = b.svc.store().summary_request(request).unwrap().unwrap();
        if got.state == RequestState::Declined {
            assert_eq!(got.reason, reason::FAILED);
            break;
        }
        assert!(std::time::Instant::now() < deadline, "never declined");
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Re-summarise "On <device>" while the meeting already waits: the same computer answers with
/// the open request, another is refused (A8).
#[test]
fn offload_a_meeting_waits_for_one_computer_at_a_time() {
    let relay = relay();
    let (a, b) = pair(&relay);
    earshot_runtime::sync::set_offer_summaries(&b.svc, true).unwrap();
    earshot_runtime::sync::set_device_name(&b.svc, "Invented desk").unwrap();
    b.client().send_presence(false).unwrap();
    let m = meeting_with_final(&a.svc, 13, 2);
    let (mid, bid) = (m.to_string(), b.id().to_string());
    ask_helper(&a.svc, &a.host, &mid, &bid).unwrap();
    let again = ask_helper(&a.svc, &a.host, &mid, &bid).unwrap();
    assert_eq!(again, "Waiting for Invented desk to write the summary.");
    assert_eq!(a.svc.store().summary_requests().unwrap().len(), 1);
    let other = earshot_core::identity::DeviceId::from_bytes([0x45; 16]).to_string();
    let refused = ask_helper(&a.svc, &a.host, &mid, &other).unwrap_err();
    assert_eq!(refused.message, ALREADY_WAITING);
    assert_eq!(a.svc.store().summary_requests().unwrap().len(), 1);
}

/// The name refusal is the shared table's (A8).
#[test]
fn offload_device_name_refusal_is_the_shared_tables() {
    let table: serde_json::Value =
        serde_json::from_str(include_str!("../../../../fixtures/ui/sync_copy_cases.json")).unwrap();
    assert_eq!(
        table["device_name_too_long"].as_str(),
        Some(crate::sync_settings::DEVICE_NAME_TOO_LONG)
    );
}

/// A helper thread that cannot start declines the request `failed` at once, so the asker writes
/// it (re-review item 5; the spawn seam, never real thread exhaustion).
#[test]
fn offload_host_declines_when_its_thread_cannot_start() {
    fn refuse(
        _: &'static str,
        _: Box<dyn FnOnce() + Send>,
    ) -> std::io::Result<std::thread::JoinHandle<()>> {
        Err(std::io::Error::other("an invented spawn failure"))
    }
    let relay = relay();
    let (a, b) = pair(&relay);
    earshot_runtime::sync::set_offer_summaries(&b.svc, true).unwrap();
    let m = meeting_with_final(&a.svc, 14, 2);
    let request = earshot_runtime::offload::ask(&a.svc, m, b.id())
        .unwrap()
        .unwrap();
    let (ca, cb) = (a.client(), b.client());
    ca.tick().unwrap();
    cb.tick().unwrap();
    cb.tick().unwrap();
    let (runner, _events) = jobs();
    let host: Arc<dyn OffloadHost> = Arc::new(
        HostOffload::new(
            b.svc.clone(),
            runner,
            MeetingQueue::new(),
            crate::activity::WorkGate::new(),
            Arc::new(NoWriter),
        )
        .with_spawn(refuse),
    );
    let state = earshot_runtime::offload::Offload::new();
    let r = state.step(&b.svc, &mut || None, &*host).unwrap();
    assert_eq!(r.handed, 1, "{r:?}");
    let got = b.svc.store().summary_request(request).unwrap().unwrap();
    assert_eq!(
        (got.state, got.reason.as_str()),
        (RequestState::Declined, reason::FAILED)
    );
    // T14.66: the write's entry went with the closure the failed spawn dropped.
    assert!(host.writing().is_empty());
}

/// T14.66 (docs/summary-order.md 11.3): the host's one registry outlives a sync thread. A second
/// hand of a request being written (a new thread's `Offload`) starts nothing. Control: the write's
/// entry goes when it ends. (A withdrawal's cancel is the runtime's `Writing`, tested there.)
#[test]
fn offload_host_writes_each_request_once_while_it_runs() {
    static HELD: std::sync::Mutex<Vec<Box<dyn FnOnce() + Send>>> =
        std::sync::Mutex::new(Vec::new());
    fn hold(
        _: &'static str,
        work: Box<dyn FnOnce() + Send>,
    ) -> std::io::Result<std::thread::JoinHandle<()>> {
        HELD.lock().unwrap().push(work);
        Ok(std::thread::spawn(|| {}))
    }
    let relay = relay();
    let (a, b) = pair(&relay);
    earshot_runtime::sync::set_offer_summaries(&b.svc, true).unwrap();
    let m = meeting_with_final(&a.svc, 15, 2);
    let request = earshot_runtime::offload::ask(&a.svc, m, b.id())
        .unwrap()
        .unwrap();
    let (ca, cb) = (a.client(), b.client());
    ca.tick().unwrap();
    cb.tick().unwrap();
    cb.tick().unwrap();
    let (runner, _events) = jobs();
    let host: Arc<dyn OffloadHost> = Arc::new(
        HostOffload::new(
            b.svc.clone(),
            runner,
            MeetingQueue::new(),
            crate::activity::WorkGate::new(),
            Arc::new(NoWriter),
        )
        .with_spawn(hold),
    );
    earshot_runtime::offload::Offload::new()
        .step(&b.svc, &mut || None, &*host)
        .unwrap();
    assert_eq!(host.writing(), vec![request]);
    // A replaced thread's new Offload hands it again: nothing new starts.
    earshot_runtime::offload::Offload::new()
        .step(&b.svc, &mut || None, &*host)
        .unwrap();
    assert_eq!(HELD.lock().unwrap().len(), 1, "one write");
    // The work ends (here with no writer): its entry goes.
    let work = HELD.lock().unwrap().pop().unwrap();
    work();
    assert!(host.writing().is_empty());
}

// ---- T14.50: the names the sync thread hears reach the cache and the waiting meeting ----

#[test]
fn offload_hearing_presence_remembers_each_computer_and_answers_only_new_names() {
    use earshot_runtime::sync::Heard;
    let desk = earshot_core::identity::DeviceId::from_bytes([0xd1; 16]);
    let server = earshot_core::identity::DeviceId::from_bytes([0xd2; 16]);
    let said = |device, name: Option<&str>, writer, place| Heard {
        device,
        name: name.map(str::to_owned),
        writer_kind: writer,
        model_place: place,
    };
    assert!(!known_computers().contains_key(&desk));
    let first = [
        said(
            desk,
            Some("Invented desk"),
            Some(WriterKind::ClaudeCode),
            None,
        ),
        said(
            server,
            Some("Invented server"),
            Some(WriterKind::Local),
            Some(ModelPlace::GraphicsCard),
        ),
    ];
    assert_eq!(hear_presence(&first), vec![desk, server], "both are new");
    let known = known_computers();
    assert_eq!(known[&desk].name.as_deref(), Some("Invented desk"));
    assert_eq!(known[&desk].writer, Some(WriterKind::ClaudeCode));
    assert_eq!(
        (known[&server].writer, known[&server].place),
        (Some(WriterKind::Local), Some(ModelPlace::GraphicsCard))
    );
    // Heard again as it was: nothing is new, so no meeting is read again for it.
    assert_eq!(hear_presence(&first), Vec::new());
    // One renamed, the other without a name this time (its presence could not say): only the
    // renamed one is answered, and the known name of the other is kept.
    let second = [
        said(desk, Some("Invented desk, renamed"), None, None),
        said(server, None, Some(WriterKind::Local), None),
    ];
    assert_eq!(hear_presence(&second), vec![desk]);
    let known = known_computers();
    assert_eq!(known[&desk].name.as_deref(), Some("Invented desk, renamed"));
    assert_eq!(known[&desk].writer, Some(WriterKind::ClaudeCode), "kept");
    assert_eq!(known[&server].name.as_deref(), Some("Invented server"));
    assert_eq!(known[&server].place, None, "the place is as last said");
    // The banner of a meeting waiting for it reads the same cache.
    assert_eq!(
        waiting_copy(known[&desk].name.as_deref()),
        "Waiting for Invented desk, renamed to write the summary."
    );
}

/// After a restart nothing in the process has heard the other computer's name. The sync thread
/// the app starts reads presence for the claimed request it waits on: the name reaches the
/// cache from that read, and the window is told which meeting to read again, so "Waiting for
/// <computer>" names it without the Sync or Summaries page being opened.
#[test]
fn offload_the_sync_thread_names_the_computer_a_waiting_meeting_shows_after_a_restart() {
    struct Idle;
    impl OffloadHost for Idle {
        fn writing(&self) -> Vec<earshot_core::identity::EntityId> {
            Vec::new()
        }
        fn cancel_writing(&self, _request: earshot_core::identity::EntityId) {}
        fn continue_run(&self, job: WriteHere) -> ContinueAnswer {
            panic!("nothing is handed back while the helper is online: {job:?}")
        }
        fn write_for(&self, _: SummaryRequest) {}
    }
    let relay = relay();
    let (a, b) = pair(&relay);
    let b_id = b.id();
    earshot_runtime::sync::set_offer_summaries(&b.svc, true).unwrap();
    earshot_runtime::sync::set_device_name(&b.svc, "Invented laptop").unwrap();
    let (ca, cb) = (a.client(), b.client());
    cb.send_presence(false).unwrap();
    let m = meeting_with_final(&a.svc, 91, 3);
    let elsewhere = meeting_with_final(&a.svc, 92, 2);
    let request = earshot_runtime::offload::ask(&a.svc, m, b_id)
        .unwrap()
        .unwrap();
    // The other computer claims it; everything is synced by hand, so the thread started below
    // has nothing left to apply.
    ca.tick().unwrap();
    cb.tick().unwrap();
    cb.tick().unwrap();
    let claimed = earshot_runtime::offload::Offload::new()
        .step(&b.svc, &mut || None, &Idle)
        .unwrap();
    assert_eq!(claimed.claimed, 1, "{claimed:?}");
    cb.tick().unwrap();
    ca.tick().unwrap();
    let quiet = ca.tick().unwrap();
    assert_eq!(quiet.written, 0, "{quiet:?}");
    let got = a.svc.store().summary_request(request).unwrap().unwrap();
    assert_eq!(got.state, RequestState::Claimed);
    // As after a restart: this process has heard nothing of that computer.
    assert!(!known_computers().contains_key(&b_id));
    assert_eq!(
        waiting_copy(None),
        "Waiting for your other computer to write the summary."
    );

    // The app starts its sync thread, with its offload host and the window's listener.
    let told: Arc<Mutex<Vec<crate::sync_settings::SyncEvent>>> = Arc::default();
    let sink = Arc::clone(&told);
    a.host
        .set_change_listener(Arc::new(move |c: &earshot_runtime::sync::SyncChanges| {
            sink.lock()
                .unwrap()
                .push(crate::sync_settings::sync_event(c));
        }));
    a.host.set_offload_host(Arc::new(Idle));
    a.host.start_if_on(&a.svc);
    assert!(a.host.running());
    let deadline = std::time::Instant::now() + Duration::from_secs(60);
    let named = |told: &Mutex<Vec<crate::sync_settings::SyncEvent>>| {
        told.lock()
            .unwrap()
            .iter()
            .filter(|e| e.meetings.contains(&m.to_string()))
            .count()
    };
    while named(&told) == 0 {
        assert!(
            std::time::Instant::now() < deadline,
            "the window was never told to read the waiting meeting again"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    // The name was in the cache before the window was told, so the read it makes shows it.
    let name = known_computers().get(&b_id).and_then(|n| n.name.clone());
    assert_eq!(name.as_deref(), Some("Invented laptop"));
    assert_eq!(
        waiting_copy(name.as_deref()),
        "Waiting for Invented laptop to write the summary."
    );
    a.host.stop_and_wait();
    assert_eq!(named(&told), 1, "told once: the name did not change again");
    assert!(
        told.lock()
            .unwrap()
            .iter()
            .all(|e| !e.meetings.contains(&elsewhere.to_string())),
        "a meeting that waits for nobody is not named"
    );
}

#[path = "offload_order_tests.rs"]
mod order;
