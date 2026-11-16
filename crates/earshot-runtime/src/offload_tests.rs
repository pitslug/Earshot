//! T11.3da: summary offload between two simulated devices against the in-process relay, with
//! fake presence and a fake clock (no test sleeps for the 90 s or 5 minute rules).
//!
//! Every name holds `offload`, so `cargo test -p earshot-runtime offload` selects them.

use std::sync::{Arc, Mutex};

use earshot_core::asr::CancelToken;
use earshot_core::identity::{Clock, DeviceId, EntityId, ManualClock};
use earshot_core::model::{MeetingRepo, RequestState, SummaryRequest, SummaryRequestRepo};
use earshot_core::summary::SummaryV1;
use earshot_llm_replay::mock::{MockResponse, MockServer};
use earshot_sync_server::{Config, LocalRelay, Relay as RelayServer};

use super::*;
use crate::meeting_queue::{MeetingQueue, SummaryHold};
use crate::summary::{SummaryConfig, SummaryService};
use crate::sync::{
    MemoryVault, Peer, Presence, SyncClient, SyncVault, join_with_code, offer_pairing_code,
    set_offer_summaries, set_up_first_device,
};
use crate::testutil::{TempDir, meeting, services};

const SECRET: &str = "an-invented-setup-secret-for-offload-tests";

struct TestRelay {
    relay: LocalRelay,
    _dir: TempDir,
}

fn relay() -> TestRelay {
    let dir = TempDir::new("offload-relay");
    let server = RelayServer::open(&Config {
        setup_secret: SECRET.into(),
        data: dir.0.join("relay"),
        listen: "127.0.0.1:0".parse().unwrap(),
    })
    .unwrap();
    TestRelay {
        relay: LocalRelay::start(server).unwrap(),
        _dir: dir,
    }
}

/// One simulated device: its own store, clock and vault.
struct Device {
    svc: Services,
    clock: Arc<ManualClock>,
    vault: Arc<MemoryVault>,
    offload: Offload,
    _dir: TempDir,
}

impl Device {
    fn new(tag: &str) -> Self {
        let dir = TempDir::new(tag);
        let (svc, clock) = services(&dir);
        Self {
            svc,
            clock,
            vault: Arc::new(MemoryVault::default()),
            offload: Offload::new(),
            _dir: dir,
        }
    }

    fn id(&self) -> DeviceId {
        self.svc.store().device()
    }

    fn vault(&self) -> Arc<dyn SyncVault> {
        Arc::clone(&self.vault) as Arc<dyn SyncVault>
    }

    fn client(&self) -> SyncClient {
        SyncClient::new(self.svc.clone(), self.vault()).unwrap()
    }

    fn request(&self, id: EntityId) -> SummaryRequest {
        self.svc.store().summary_request(id).unwrap().unwrap()
    }

    fn live_summaries(&self, meeting: EntityId) -> usize {
        self.svc
            .store()
            .summaries(meeting)
            .unwrap()
            .iter()
            .filter(|s| !s.meta.deleted)
            .count()
    }

    /// One offload step with `peers` as the relay's presence.
    fn step(&self, peers: Option<Vec<Peer>>, host: &Recorder) -> StepReport {
        let mut asked = false;
        self.offload
            .step(
                &self.svc,
                &mut || {
                    assert!(!asked, "presence is asked at most once a step");
                    asked = true;
                    peers.clone()
                },
                host,
            )
            .unwrap()
    }
}

/// A first device (the asker) and a second (the helper) joined with a pairing code; the helper
/// offers to write summaries.
fn pair(relay: &TestRelay) -> (Device, Device) {
    let a = Device::new("offload-a");
    let b = Device::new("offload-b");
    set_up_first_device(&a.svc, &*a.vault, &relay.relay.url(), SECRET).unwrap();
    let offer = offer_pairing_code(&a.svc, a.vault()).unwrap();
    join_with_code(&b.svc, &*b.vault, &relay.relay.url(), offer.code()).unwrap();
    set_offer_summaries(&b.svc, true).unwrap();
    (a, b)
}

/// What a host was handed.
#[derive(Default)]
struct Recorder {
    here: Mutex<Vec<WriteHere>>,
    for_others: Mutex<Vec<SummaryRequest>>,
    /// Refuse to start a write here (a job of the meeting runs).
    refuse_here: std::sync::atomic::AtomicBool,
    /// Answer that nothing is left to do.
    settle_here: std::sync::atomic::AtomicBool,
}

impl OffloadHost for Recorder {
    fn continue_run(&self, job: WriteHere) -> ContinueAnswer {
        self.here.lock().unwrap().push(job);
        if self.settle_here.load(std::sync::atomic::Ordering::SeqCst) {
            ContinueAnswer::Settled
        } else if self.refuse_here.load(std::sync::atomic::Ordering::SeqCst) {
            ContinueAnswer::Refused
        } else {
            ContinueAnswer::Started
        }
    }
    fn write_for(&self, request: SummaryRequest) {
        self.for_others.lock().unwrap().push(request);
    }
}

impl Recorder {
    fn here(&self) -> Vec<WriteHere> {
        self.here.lock().unwrap().clone()
    }
    fn for_others(&self) -> Vec<SummaryRequest> {
        self.for_others.lock().unwrap().clone()
    }
}

fn presence(name: &str, offers: bool, writer: Option<WriterKind>, recording: bool) -> Presence {
    Presence {
        v: 1,
        name: name.into(),
        app_version: "0.0.0".into(),
        kind: "app".into(),
        offers_summaries: offers,
        writer_kind: writer.map(|w| w.word().to_owned()),
        recording,
        gone: false,
        pad: String::new(),
    }
}

/// `device` as the relay would report it: `age_s` old, online by the production rule.
fn peer(device: DeviceId, presence: Option<Presence>, age_s: u64) -> Peer {
    let gone = presence.as_ref().is_some_and(|p| p.gone);
    Peer {
        device,
        online: presence.is_some() && crate::sync::online(age_s, gone),
        presence,
        age_s,
    }
}

fn helper_online(device: DeviceId) -> Vec<Peer> {
    vec![peer(
        device,
        Some(presence(
            "Invented desk",
            true,
            Some(WriterKind::OpenaiCompatible),
            false,
        )),
        10,
    )]
}

fn reply(title: &str) -> MockResponse {
    let summary = SummaryV1 {
        title: title.into(),
        tldr: "The team agreed to ship on Friday.".into(),
        enhanced_notes: vec![],
        decisions: vec![],
        action_items: vec![],
        open_questions: vec![],
        topics: vec!["release".into()],
    };
    MockResponse::completion(&serde_json::to_string(&summary).unwrap(), "stop")
}

fn remote_service(svc: &Services, server: &MockServer) -> SummaryService {
    SummaryService::new(
        &SummaryConfig::new(server.endpoint().as_str(), "test-model"),
        svc.clock.clone(),
    )
    .unwrap()
}

/// The asker's summary step over the level fakes' final pass (models load nothing).
fn run_of<'a>(
    models: &'a crate::backends::ModelDir,
    summary: &'a SummaryService,
    summary_cancel: &'a CancelToken,
    offload: &'a dyn Fn() -> OffloadChoice,
    post: bool,
) -> crate::host::AfterMeeting<'a> {
    crate::host::AfterMeeting {
        models,
        model: "base.en",
        order: &[crate::backends::Accelerator::Cpu],
        post,
        whole_pass: false,
        summary: Some(summary),
        no_summary: crate::host::SkipReason::NotRequested,
        post_cancel: summary_cancel,
        summary_cancel,
        offload: Some(offload),
        order_run: None,
    }
}

/// A helper writer over the mock endpoint (it holds nothing of this computer).
fn remote_writer(svc: &Services, server: &MockServer) -> HelperWriter {
    HelperWriter::Write(
        Box::new(remote_service(svc, server)),
        SummaryHold::OffThisComputer,
    )
}

/// Syncs both ways: `a` pushes, `b` pulls twice (rows that came before their parent in one page
/// are held and applied by the next tick, as the sync thread's next tick does) and pushes, `a`
/// pulls.
fn sync(a: &SyncClient, b: &SyncClient) {
    a.tick().unwrap();
    b.tick().unwrap();
    b.tick().unwrap();
    a.tick().unwrap();
}

// ---- the decision, clock-free ----

#[test]
fn offload_decide_asks_only_an_online_offering_idle_helper_with_a_writer() {
    let me = DeviceId::from_bytes([1; 16]);
    let them = DeviceId::from_bytes([2; 16]);
    let writer = Some(WriterKind::ClaudeCode);
    assert_eq!(
        decide(None, None, false),
        OffloadChoice::Here,
        "none chosen"
    );
    assert_eq!(
        decide(None, Some(&helper_online(them)), false),
        OffloadChoice::Here
    );
    let why = |c: OffloadChoice| match c {
        OffloadChoice::HereInstead { device, why, .. } => {
            assert_eq!(device, them);
            Some(why)
        }
        _ => None,
    };
    assert_eq!(
        why(decide(Some(them), None, false)),
        Some(Unavailable::SyncOff)
    );
    assert_eq!(
        why(decide(Some(them), None, true)),
        Some(Unavailable::Unreachable)
    );
    let only_me = vec![peer(me, Some(presence("Here", true, writer, false)), 0)];
    assert_eq!(
        why(decide(Some(them), Some(&only_me), false)),
        Some(Unavailable::Offline)
    );
    let unreadable = vec![peer(them, None, 0)];
    assert_eq!(
        why(decide(Some(them), Some(&unreadable), false)),
        Some(Unavailable::Offline)
    );
    // The online rule is the presence one: 150 s is online, 151 s is not.
    let at = |age| {
        vec![peer(
            them,
            Some(presence("Invented desk", true, writer, false)),
            age,
        )]
    };
    assert!(matches!(
        decide(Some(them), Some(&at(150)), false),
        OffloadChoice::Ask { .. }
    ));
    assert_eq!(
        why(decide(Some(them), Some(&at(151)), false)),
        Some(Unavailable::Offline)
    );
    // Gone (asleep or quit) is offline at once, whatever the age.
    let mut asleep = presence("Invented desk", true, writer, false);
    asleep.gone = true;
    assert_eq!(
        why(decide(
            Some(them),
            Some(&[peer(them, Some(asleep), 1)]),
            false
        )),
        Some(Unavailable::Offline)
    );
    let one = |p: Presence| vec![peer(them, Some(p), 5)];
    assert_eq!(
        why(decide(
            Some(them),
            Some(&one(presence("D", false, writer, false))),
            false
        )),
        Some(Unavailable::NotOffering)
    );
    assert_eq!(
        why(decide(
            Some(them),
            Some(&one(presence("D", true, writer, true))),
            false
        )),
        Some(Unavailable::Recording)
    );
    assert_eq!(
        why(decide(
            Some(them),
            Some(&one(presence("D", true, None, false))),
            false
        )),
        Some(Unavailable::NoWriter)
    );
    let mut unknown = presence("D", true, None, false);
    unknown.writer_kind = Some("a_writer_from_a_newer_version".into());
    assert_eq!(
        why(decide(Some(them), Some(&one(unknown)), false)),
        Some(Unavailable::NoWriter)
    );
    assert_eq!(
        decide(
            Some(them),
            Some(&one(presence("Invented desk", true, writer, false))),
            false
        ),
        OffloadChoice::Ask {
            device: them,
            name: "Invented desk".into(),
            writer: WriterKind::ClaudeCode,
        }
    );
}

#[test]
fn offload_helper_staleness_starts_at_five_minutes_of_silence() {
    let them = DeviceId::from_bytes([2; 16]);
    let p = || Some(presence("D", true, Some(WriterKind::Local), false));
    let seen = |stale| HelperPresence::Seen { stale };
    assert_eq!(
        presence_of(&[peer(them, p(), STALE_AFTER_S - 1)], them),
        seen(false)
    );
    assert_eq!(
        presence_of(&[peer(them, p(), STALE_AFTER_S)], them),
        seen(true)
    );
    // A helper that said gone is stale five minutes after it said so, not at once.
    let mut gone = presence("D", true, Some(WriterKind::Local), false);
    gone.gone = true;
    assert_eq!(
        presence_of(&[peer(them, Some(gone.clone()), 30)], them),
        seen(false)
    );
    assert_eq!(
        presence_of(&[peer(them, Some(gone), STALE_AFTER_S)], them),
        seen(true)
    );
    // Missing or unreadable presence is timed by the asker from when it first found it so.
    assert_eq!(presence_of(&[], them), HelperPresence::Missing);
    assert_eq!(
        presence_of(&[peer(them, None, 0)], them),
        HelperPresence::Missing
    );
}

#[test]
fn offload_writer_kinds_have_stable_words_and_follow_the_summary_writer() {
    for k in WriterKind::ALL {
        assert_eq!(WriterKind::parse(k.word()), Some(k));
        assert_eq!(
            serde_json::to_value(k).unwrap(),
            serde_json::Value::String(k.word().into())
        );
    }
    let dir = TempDir::new("offload-writer");
    let (svc, _) = services(&dir);
    use crate::summary_settings::{
        EndpointProvider, SummarySettings, SummaryWriter, set_summary_settings,
    };
    let set = |writer: SummaryWriter, provider: EndpointProvider| {
        let url = match provider {
            EndpointProvider::Anthropic => "https://api.anthropic.com",
            EndpointProvider::OpenAiCompatible => "https://llm.example.com/v1",
        };
        set_summary_settings(
            &mut svc.store(),
            &SummarySettings {
                writer,
                provider,
                url: url.into(),
                model: "an-invented-model".into(),
                ..SummarySettings::default()
            },
        )
        .unwrap();
    };
    let kind = || WriterKind::of_settings(&svc.store()).unwrap();
    assert_eq!(
        kind(),
        Some(WriterKind::Local),
        "the default writer is the local model"
    );
    for (writer, provider, want) in [
        (
            SummaryWriter::OnThisComputer,
            EndpointProvider::Anthropic,
            Some(WriterKind::Local),
        ),
        (
            SummaryWriter::OwnEndpoint,
            EndpointProvider::Anthropic,
            Some(WriterKind::Anthropic),
        ),
        (
            SummaryWriter::OwnEndpoint,
            EndpointProvider::OpenAiCompatible,
            Some(WriterKind::OpenaiCompatible),
        ),
        (
            SummaryWriter::ClaudeCode,
            EndpointProvider::OpenAiCompatible,
            Some(WriterKind::ClaudeCode),
        ),
        (
            SummaryWriter::Codex,
            EndpointProvider::OpenAiCompatible,
            Some(WriterKind::Codex),
        ),
        (
            SummaryWriter::NoSummary,
            EndpointProvider::OpenAiCompatible,
            None,
        ),
    ] {
        set(writer, provider);
        assert_eq!(kind(), want, "{writer:?} {provider:?}");
    }
}

#[test]
fn offload_chosen_helper_is_device_local_and_never_this_device() {
    let dir = TempDir::new("offload-choice");
    let (svc, _) = services(&dir);
    let them = DeviceId::from_bytes([9; 16]);
    assert_eq!(chosen_helper(&svc.store()).unwrap(), None);
    set_chosen_helper(&svc, Some(them)).unwrap();
    assert_eq!(chosen_helper(&svc.store()).unwrap(), Some(them));
    let me = svc.store().device();
    assert!(set_chosen_helper(&svc, Some(me)).is_err());
    assert_eq!(
        chosen_helper(&svc.store()).unwrap(),
        Some(them),
        "unchanged"
    );
    set_chosen_helper(&svc, None).unwrap();
    assert_eq!(chosen_helper(&svc.store()).unwrap(), None);
    // A stored value that is not a device id is no choice.
    crate::local::set(
        &mut svc.store(),
        &crate::local::DeviceRecord::SummaryOffloadDevice.key(),
        b"not-a-device",
    )
    .unwrap();
    assert_eq!(chosen_helper(&svc.store()).unwrap(), None);
    // Without sync no request is made: the choice is shown as not available.
    set_chosen_helper(&svc, Some(them)).unwrap();
    let vault: Arc<dyn SyncVault> = Arc::new(MemoryVault::default());
    assert!(matches!(
        choose_now(&svc, vault),
        OffloadChoice::HereInstead {
            why: Unavailable::SyncOff,
            ..
        }
    ));
}

/// Only the asker writes requested and withdrawn, only the target the rest; anything else is
/// ignored by every reader.
#[test]
fn offload_a_state_written_by_the_wrong_device_is_ignored() {
    let asker = DeviceId::from_bytes([1; 16]);
    let target = DeviceId::from_bytes([2; 16]);
    let dir = TempDir::new("offload-authored");
    let (svc, _) = services(&dir);
    let m = meeting(&svc, "Invented review", 3);
    let mut r = {
        let mut db = svc.store();
        let clock = Arc::clone(&svc.clock);
        let (_, hlc) = db.parts();
        let mut rng = crate::os_seeded_rng();
        let mut stamp = earshot_core::model::Stamp {
            clock: &*clock,
            hlc,
            rng: &mut *rng,
        };
        SummaryRequest::new(m, target, asker, 2, &mut stamp)
    };
    for (state, writer, ok) in [
        (RequestState::Requested, asker, true),
        (RequestState::Requested, target, false),
        (RequestState::Withdrawn, asker, true),
        (RequestState::Withdrawn, target, false),
        (RequestState::Claimed, target, true),
        (RequestState::Claimed, asker, false),
        (RequestState::Done, target, true),
        (RequestState::Done, asker, false),
        (RequestState::Declined, target, true),
        (RequestState::Declined, asker, false),
    ] {
        r.state = state;
        r.meta.updated_at.node = writer;
        assert_eq!(authored(&r), ok, "{state:?} written by {writer:?}");
    }
}

// ---- two devices against the relay ----

/// The whole path: the asker's summary step asks the helper; the helper claims, waits for the
/// transcript, writes with its own writer through the ordinary path in its meeting queue and
/// marks the request done in the summary's transaction; the summary syncs back.
#[test]
fn offload_the_helper_writes_the_summary_and_it_syncs_back() {
    let relay = relay();
    let (a, b) = pair(&relay);
    let (ca, cb) = (a.client(), b.client());
    set_chosen_helper(&a.svc, Some(b.id())).unwrap();
    let aid = a.id();

    // The asker's summary step, after a real final pass, with the helper online.
    let outcome = crate::host::tests::recorded(&a.svc, 301);
    let meeting = outcome.meeting;
    let models = crate::backends::ModelDir::new(a._dir.0.join("models"));
    let sc = CancelToken::new();
    let never = MockServer::start(vec![reply("Never written here")]).unwrap();
    let own = remote_service(&a.svc, &never);
    let peers = helper_online(b.id());
    let choose = || decide(Some(b.id()), Some(&peers), false);
    let host = crate::host::after_meeting_with(
        &a.svc,
        &outcome,
        &run_of(&models, &own, &sc, &choose, true),
        crate::host::tests::fake_post,
        &mut |_| {},
    );
    let crate::host::SummaryResult::Offloaded {
        request,
        device,
        name,
        writer,
    } = host.summary
    else {
        panic!("{:?}", host.summary);
    };
    assert_eq!(
        (device, name.as_str(), writer),
        (b.id(), "Invented desk", Some(WriterKind::OpenaiCompatible))
    );
    assert!(matches!(host.offload, Some(OffloadChoice::Ask { .. })));
    assert!(
        never.requests().is_empty(),
        "the asker's own writer did not run"
    );
    assert!(host.network.is_empty());
    let asked = a.request(request);
    assert_eq!(asked.state, RequestState::Requested);
    assert_eq!((asked.asking_device, asked.target_device), (a.id(), b.id()));
    let finals = earshot_core::model::live_segments(
        &**a.svc.store(),
        meeting,
        earshot_core::model::TranscriptPass::Final,
    )
    .unwrap()
    .len() as u64;
    assert!(finals > 0);
    assert_eq!(asked.segment_count, finals);
    assert_eq!(
        waiting_for(&a.svc.store(), aid, meeting).unwrap(),
        Some(b.id())
    );

    // The helper claims and is handed the work.
    sync(&ca, &cb);
    let helper = Recorder::default();
    let r = b.step(None, &helper);
    assert_eq!((r.claimed, r.handed), (1, 1), "{r:?}");
    let handed = helper.for_others();
    assert_eq!(handed.len(), 1);
    // Handed once per process.
    let again = b.step(None, &helper);
    assert_eq!(again.handed, 0, "{again:?}");
    sync(&cb, &ca);
    assert_eq!(a.request(request).state, RequestState::Claimed);
    assert_eq!(
        waiting_for(&a.svc.store(), aid, meeting).unwrap(),
        Some(b.id())
    );

    // The helper writes with its own writer, in its meeting queue.
    let server = MockServer::start(vec![reply("Written by the helper")]).unwrap();
    let queue = MeetingQueue::new();
    let done = write_for_request(
        &b.svc,
        &queue,
        &handed[0],
        &|privacy| {
            assert_eq!(privacy, SummaryPrivacy::Writer);
            remote_writer(&b.svc, &server)
        },
        &CancelToken::new(),
    );
    assert!(matches!(done, HelperOutcome::Done { .. }), "{done:?}");
    assert_eq!(server.requests().len(), 1);
    assert_eq!(b.request(request).state, RequestState::Done);
    assert_eq!(b.live_summaries(meeting), 1);

    // It syncs back: the asker holds the summary and has nothing to write itself.
    sync(&cb, &ca);
    assert_eq!(a.request(request).state, RequestState::Done);
    assert_eq!(a.live_summaries(meeting), 1);
    let title = a.svc.store().meeting(meeting).unwrap().unwrap().title;
    assert_eq!(title, "Written by the helper");
    let asker = Recorder::default();
    let r = a.step(None, &asker);
    assert_eq!(r, StepReport::default());
    assert!(asker.here().is_empty());
    assert_eq!(waiting_for(&a.svc.store(), aid, meeting).unwrap(), None);
}

#[test]
fn offload_an_unclaimed_request_is_withdrawn_after_90_seconds_and_written_here() {
    let relay = relay();
    let (a, b) = pair(&relay);
    let (ca, cb) = (a.client(), b.client());
    let m = meeting(&a.svc, "Invented standup", 11);
    let request = ask(&a.svc, m, b.id()).unwrap().unwrap();
    let aid = a.id();
    let asker = Recorder::default();
    a.clock.advance_ms(CLAIM_WITHIN_MS - 1);
    assert_eq!(a.step(None, &asker), StepReport::default(), "not yet");
    a.clock.advance_ms(1);
    // Presence is not needed for an unclaimed request: the relay may be unreachable.
    let r = a.step(None, &asker);
    assert_eq!((r.withdrawn, r.written_here), (1, 1), "{r:?}");
    assert_eq!(
        asker.here(),
        vec![WriteHere {
            meeting: m,
            request,
            helper: b.id(),
            why: HereReason::NotClaimed,
            continuation: Continuation::Legacy,
        }]
    );
    assert_eq!(a.request(request).state, RequestState::Withdrawn);
    assert_eq!(waiting_for(&a.svc.store(), aid, m).unwrap(), None);
    // Handed once in this process.
    assert_eq!(a.step(None, &asker).written_here, 0);
    // A later process hands it again while no summary was written (the write here failed or
    // never ran), and stops once one exists.
    let restarted = Offload::new();
    let rh = Recorder::default();
    restarted.step(&a.svc, &mut || None, &rh).unwrap();
    assert_eq!(rh.here().len(), 1);
    assert_eq!(rh.here()[0].why, HereReason::Withdrawn);

    // The helper hears of the withdrawal and does nothing with it.
    sync(&ca, &cb);
    let helper = Recorder::default();
    assert_eq!(b.step(None, &helper), StepReport::default());
    assert_eq!(b.request(request).state, RequestState::Withdrawn);
}

#[test]
fn offload_a_claimed_request_whose_helper_goes_quiet_is_withdrawn_after_five_minutes() {
    let relay = relay();
    let (a, b) = pair(&relay);
    let (ca, cb) = (a.client(), b.client());
    let m = meeting(&a.svc, "Invented retro", 12);
    let request = ask(&a.svc, m, b.id()).unwrap().unwrap();
    sync(&ca, &cb);
    b.step(None, &Recorder::default());
    sync(&cb, &ca);
    assert_eq!(a.request(request).state, RequestState::Claimed);

    let asker = Recorder::default();
    a.clock.advance_ms(60 * 60_000);
    // Online, or quiet for less than five minutes: the helper keeps it whatever the time.
    let quiet = |age| {
        vec![peer(
            b.id(),
            Some(presence(
                "Invented desk",
                true,
                Some(WriterKind::Local),
                false,
            )),
            age,
        )]
    };
    assert_eq!(
        a.step(Some(quiet(STALE_AFTER_S - 1)), &asker),
        StepReport::default()
    );
    // Presence could not be read: nothing is withdrawn on presence.
    assert_eq!(a.step(None, &asker), StepReport::default());
    let r = a.step(Some(quiet(STALE_AFTER_S)), &asker);
    assert_eq!((r.withdrawn, r.written_here), (1, 1), "{r:?}");
    assert_eq!(asker.here()[0].why, HereReason::HelperStale);
    assert_eq!(a.request(request).state, RequestState::Withdrawn);
}

/// The helper claims after the asker withdrew, before it heard of it: the withdrawal wins on
/// both devices, and the helper's writer is never run.
#[test]
fn offload_a_claim_after_the_withdrawal_never_reopens_the_request() {
    let relay = relay();
    let (a, b) = pair(&relay);
    let (ca, cb) = (a.client(), b.client());
    let m = meeting(&a.svc, "Invented planning", 13);
    let request = ask(&a.svc, m, b.id()).unwrap().unwrap();
    sync(&ca, &cb);
    // The asker withdraws (not claimed in time) and has not pushed yet.
    a.clock.advance_ms(CLAIM_WITHIN_MS);
    a.step(None, &Recorder::default());
    // The helper claims (it has not heard) and pushes; its clock is ahead, so its claim is the
    // later write.
    b.clock.advance_ms(10 * CLAIM_WITHIN_MS);
    let helper = Recorder::default();
    assert_eq!(b.step(None, &helper).claimed, 1);
    cb.tick().unwrap();
    // The asker pulls the later claim: it keeps its withdrawal and stamps it again.
    let r = ca.tick().unwrap();
    assert!(r.repaired >= 1, "{r:?}");
    assert_eq!(a.request(request).state, RequestState::Withdrawn);
    ca.tick().unwrap();
    cb.tick().unwrap();
    assert_eq!(b.request(request).state, RequestState::Withdrawn);
    // The helper's work finds the request no longer claimed and writes nothing.
    let server = MockServer::start(vec![reply("Never stored")]).unwrap();
    let done = write_for_request(
        &b.svc,
        &MeetingQueue::new(),
        &helper.for_others()[0],
        &|_| remote_writer(&b.svc, &server),
        &CancelToken::new(),
    );
    assert_eq!(done, HelperOutcome::NotClaimed);
    assert!(server.requests().is_empty());
    assert_eq!(b.live_summaries(m), 0);
}

/// The asker withdraws while the helper's model is running: the helper re-reads the request in
/// its write transaction and stores nothing.
#[test]
fn offload_a_withdrawal_during_the_helpers_summary_stores_nothing() {
    let relay = relay();
    let (a, b) = pair(&relay);
    let (ca, cb) = (Arc::new(a.client()), Arc::new(b.client()));
    let m = meeting(&a.svc, "Invented budget", 14);
    let request = ask(&a.svc, m, b.id()).unwrap().unwrap();
    sync(&ca, &cb);
    let helper = Recorder::default();
    b.step(None, &helper);
    sync(&cb, &ca);
    // While the helper's endpoint answers, the asker finds the helper gone quiet, withdraws,
    // and the withdrawal reaches the helper.
    let (asvc, a_offload, bid) = (a.svc.clone(), Arc::new(Offload::new()), b.id());
    let (ta, tb) = (Arc::clone(&ca), Arc::clone(&cb));
    let clock = Arc::clone(&a.clock);
    let pending = Mutex::new(Some(()));
    let server = MockServer::start_with(move |_| {
        if pending.lock().unwrap().take().is_some() {
            clock.advance_ms(60 * 60_000);
            let stale = vec![peer(
                bid,
                Some(presence("D", true, Some(WriterKind::Local), false)),
                STALE_AFTER_S,
            )];
            let r = a_offload
                .step(&asvc, &mut || Some(stale.clone()), &Recorder::default())
                .unwrap();
            assert_eq!(r.withdrawn, 1, "{r:?}");
            ta.tick().unwrap();
            tb.tick().unwrap();
        }
        reply("Too late")
    })
    .unwrap();
    let done = write_for_request(
        &b.svc,
        &MeetingQueue::new(),
        &helper.for_others()[0],
        &|_| remote_writer(&b.svc, &server),
        &CancelToken::new(),
    );
    assert_eq!(done, HelperOutcome::NotClaimed);
    assert_eq!(server.requests().len(), 1, "the model ran");
    assert_eq!(
        b.live_summaries(m),
        0,
        "nothing stored for a withdrawn request"
    );
    assert_eq!(b.request(request).state, RequestState::Withdrawn);
}

/// A meeting kept on its computer is written only by a local model on either device: the
/// helper declines without one, the asker gets it back, and a remote writer is refused before
/// the transcript is read even if a host offered one.
#[test]
fn offload_a_kept_local_meeting_never_reaches_a_remote_writer() {
    let relay = relay();
    let (a, b) = pair(&relay);
    let (ca, cb) = (a.client(), b.client());
    let m = meeting(&a.svc, "Invented private", 15);
    {
        let mut db = a.svc.store();
        let clock = Arc::clone(&a.svc.clock);
        let (store, hlc) = db.parts();
        let mut row = store.meeting(m).unwrap().unwrap();
        row.summary_kept_local = true;
        row.meta.touch(hlc, &*clock);
        store.put_meeting(row).unwrap();
    }

    // 1. A host that builds its writer from the privacy declines without a local model.
    let request = ask(&a.svc, m, b.id()).unwrap().unwrap();
    sync(&ca, &cb);
    let helper = Recorder::default();
    b.step(None, &helper);
    let done = write_for_request(
        &b.svc,
        &MeetingQueue::new(),
        &helper.for_others()[0],
        &|privacy| {
            assert_eq!(
                privacy,
                SummaryPrivacy::KeptLocal,
                "the synced field keeps it"
            );
            HelperWriter::Decline(reason::KEPT_LOCAL_NO_MODEL)
        },
        &CancelToken::new(),
    );
    assert_eq!(done, HelperOutcome::Declined(reason::KEPT_LOCAL_NO_MODEL));
    sync(&cb, &ca);
    let declined = a.request(request);
    assert_eq!(
        (declined.state, declined.reason.as_str()),
        (RequestState::Declined, reason::KEPT_LOCAL_NO_MODEL)
    );
    let asker = Recorder::default();
    a.step(None, &asker);
    assert_eq!(
        asker.here()[0].why,
        HereReason::Declined(reason::KEPT_LOCAL_NO_MODEL.into())
    );

    // 2. A host that offered a remote writer anyway: refused before anything is sent.
    a.clock.advance_ms(1_000);
    let second = ask(&a.svc, m, b.id()).unwrap().unwrap();
    sync(&ca, &cb);
    let helper = Recorder::default();
    b.step(None, &helper);
    let server = MockServer::start(vec![reply("Never sent")]).unwrap();
    let handed = helper
        .for_others()
        .into_iter()
        .find(|r| r.id == second)
        .unwrap();
    let done = write_for_request(
        &b.svc,
        &MeetingQueue::new(),
        &handed,
        &|_| remote_writer(&b.svc, &server),
        &CancelToken::new(),
    );
    assert_eq!(done, HelperOutcome::Declined(reason::KEPT_LOCAL_NO_MODEL));
    assert!(server.requests().is_empty(), "the transcript never left");
    assert_eq!(b.live_summaries(m), 0);
}

#[test]
fn offload_a_device_that_no_longer_offers_declines() {
    let relay = relay();
    let (a, b) = pair(&relay);
    let (ca, cb) = (a.client(), b.client());
    set_offer_summaries(&b.svc, false).unwrap();
    let m = meeting(&a.svc, "Invented sync", 16);
    let request = ask(&a.svc, m, b.id()).unwrap().unwrap();
    sync(&ca, &cb);
    let helper = Recorder::default();
    let r = b.step(None, &helper);
    assert_eq!((r.claimed, r.declined, r.handed), (0, 1, 0), "{r:?}");
    sync(&cb, &ca);
    let got = a.request(request);
    assert_eq!(
        (got.state, got.reason.as_str()),
        (RequestState::Declined, reason::NOT_OFFERED)
    );
    let asker = Recorder::default();
    a.step(None, &asker);
    assert_eq!(
        asker.here()[0].why,
        HereReason::Declined(reason::NOT_OFFERED.into())
    );
    // Once a summary is written here it is not handed again (by any later process).
    let server = MockServer::start(vec![reply("Written here")]).unwrap();
    remote_service(&a.svc, &server)
        .summarize(&a.svc, m, &mut *crate::os_seeded_rng(), &CancelToken::new())
        .unwrap();
    let rh = Recorder::default();
    Offload::new().step(&a.svc, &mut || None, &rh).unwrap();
    assert!(rh.here().is_empty());
}

/// The helper waits for the transcript the request counted; a transcript that never arrives
/// is declined after the wait.
#[test]
fn offload_the_helper_waits_for_the_counted_transcript() {
    let relay = relay();
    let (a, b) = pair(&relay);
    let (ca, cb) = (a.client(), b.client());
    let m = meeting(&a.svc, "Invented roadmap", 17);
    sync(&ca, &cb);
    // Asked as if the asker held one more final segment than reached the helper.
    let request = ask(&a.svc, m, b.id()).unwrap().unwrap();
    {
        let mut db = a.svc.store();
        let clock = Arc::clone(&a.svc.clock);
        let (store, hlc) = db.parts();
        let mut r = store.summary_request(request).unwrap().unwrap();
        r.segment_count += 1;
        r.meta.touch(hlc, &*clock);
        store.put_summary_request(r).unwrap();
    }
    sync(&ca, &cb);
    let helper = Recorder::default();
    let r = b.step(None, &helper);
    assert_eq!((r.claimed, r.handed), (1, 0), "claimed, not handed: {r:?}");
    b.clock.advance_ms(TRANSCRIPT_WAIT_MS - 1);
    assert_eq!(b.step(None, &helper), StepReport::default());
    b.clock.advance_ms(1);
    let r = b.step(None, &helper);
    assert_eq!(r.declined, 1, "{r:?}");
    let got = b.request(request);
    assert_eq!(
        (got.state, got.reason.as_str()),
        (RequestState::Declined, reason::NO_TRANSCRIPT)
    );
    assert!(helper.for_others().is_empty());
}

/// The helper's summary takes its turn in its own meeting queue, behind its own work.
#[test]
fn offload_the_helpers_summary_waits_for_its_own_older_work() {
    let relay = relay();
    let (a, b) = pair(&relay);
    let (ca, cb) = (a.client(), b.client());
    let m = meeting(&a.svc, "Invented queue", 18);
    let request = ask(&a.svc, m, b.id()).unwrap().unwrap();
    sync(&ca, &cb);
    let helper = Recorder::default();
    b.step(None, &helper);
    let handed = helper.for_others().remove(0);
    let queue = MeetingQueue::new();
    let older = queue.enter();
    let server = MockServer::start(vec![reply("After my own work")]).unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    let (svc, q, srv) = (b.svc.clone(), queue.clone(), Arc::new(server));
    let srv2 = Arc::clone(&srv);
    let worker = std::thread::spawn(move || {
        write_for_request(
            &svc,
            &q,
            &handed,
            &|_| {
                tx.send(()).unwrap();
                remote_writer(&svc, &srv2)
            },
            &CancelToken::new(),
        )
    });
    // The helper has entered the queue and built its writer; with the older ticket held it
    // cannot reach the model.
    rx.recv().unwrap();
    assert!(srv.requests().is_empty());
    assert_eq!(b.request(request).state, RequestState::Claimed);
    drop(older);
    let done = worker.join().unwrap();
    assert!(matches!(done, HelperOutcome::Done { .. }), "{done:?}");
    assert_eq!(srv.requests().len(), 1);
}

/// A failed summary on the helper declines the request, so the asker writes it.
#[test]
fn offload_a_failed_helper_summary_declines_the_request() {
    let relay = relay();
    let (a, b) = pair(&relay);
    let (ca, cb) = (a.client(), b.client());
    let m = meeting(&a.svc, "Invented outage", 19);
    let request = ask(&a.svc, m, b.id()).unwrap().unwrap();
    sync(&ca, &cb);
    let helper = Recorder::default();
    b.step(None, &helper);
    let server = MockServer::start(vec![MockResponse::status(500, "upstream down")]).unwrap();
    let done = write_for_request(
        &b.svc,
        &MeetingQueue::new(),
        &helper.for_others()[0],
        &|_| remote_writer(&b.svc, &server),
        &CancelToken::new(),
    );
    assert_eq!(done, HelperOutcome::Declined(reason::FAILED));
    assert_eq!(b.request(request).state, RequestState::Declined);
    // The asker hears of the decline; the meeting then has no open request and can be asked anew.
    sync(&cb, &ca);
    // A cancelled one stays claimed for the next start.
    a.clock.advance_ms(1_000);
    let second = ask(&a.svc, m, b.id()).unwrap().unwrap();
    sync(&ca, &cb);
    let helper = Recorder::default();
    b.step(None, &helper);
    let cancel = CancelToken::new();
    cancel.cancel();
    let handed = helper
        .for_others()
        .into_iter()
        .find(|r| r.id == second)
        .unwrap();
    let done = write_for_request(
        &b.svc,
        &MeetingQueue::new(),
        &handed,
        &|_| remote_writer(&b.svc, &server),
        &cancel,
    );
    assert_eq!(done, HelperOutcome::Cancelled);
    assert_eq!(b.request(second).state, RequestState::Claimed);
}

/// The summary step shows a named choice when the chosen helper is not available: this
/// device's writer runs and the outcome says why. A meeting with no final transcript is not
/// offloaded.
#[test]
fn offload_a_helper_not_available_runs_this_devices_writer_with_a_reason() {
    let dir = TempDir::new("offload-here");
    let (svc, _) = services(&dir);
    let them = DeviceId::from_bytes([7; 16]);
    let outcome = crate::host::tests::recorded(&svc, 311);
    let models = crate::backends::ModelDir::new(dir.0.join("models"));
    let sc = CancelToken::new();
    let server = MockServer::start(vec![reply("Written here instead")]).unwrap();
    let own = remote_service(&svc, &server);
    let choose = || decide(Some(them), Some(&[]), false);
    let host = crate::host::after_meeting_with(
        &svc,
        &outcome,
        &run_of(&models, &own, &sc, &choose, true),
        crate::host::tests::fake_post,
        &mut |_| {},
    );
    assert!(
        matches!(host.summary, crate::host::SummaryResult::Stored { .. }),
        "{:?}",
        host.summary
    );
    assert_eq!(
        host.offload,
        Some(OffloadChoice::HereInstead {
            device: them,
            name: None,
            why: Unavailable::Offline,
        })
    );
    assert!(svc.store().summary_requests().unwrap().is_empty());

    // No final transcript (the post pass skipped): nothing to count, written here.
    let bare = crate::host::tests::recorded(&svc, 321);
    let online = helper_online(them);
    let choose = || decide(Some(them), Some(&online), false);
    let server = MockServer::start(vec![reply("Draft summary")]).unwrap();
    let own = remote_service(&svc, &server);
    let host = crate::host::after_meeting_with(
        &svc,
        &bare,
        &run_of(&models, &own, &sc, &choose, false),
        crate::host::tests::fake_post,
        &mut |_| {},
    );
    assert!(matches!(
        host.offload,
        Some(OffloadChoice::HereInstead {
            why: Unavailable::NoFinalTranscript,
            ..
        })
    ));
    assert!(svc.store().summary_requests().unwrap().is_empty());
    // A cancelled summary decides nothing.
    let cancelled = CancelToken::new();
    cancelled.cancel();
    let never: &dyn Fn() -> OffloadChoice = &|| panic!("not asked");
    let host = crate::host::after_meeting_with(
        &svc,
        &bare,
        &run_of(&models, &own, &cancelled, never, false),
        crate::host::tests::fake_post,
        &mut |_| {},
    );
    assert_eq!(host.offload, None);
}

/// The sync thread runs the offload step after its ticks and hands the host the request.
#[test]
fn offload_the_sync_thread_steps_after_each_tick() {
    let relay = relay();
    let (a, b) = pair(&relay);
    let m = meeting(&a.svc, "Invented thread", 20);
    let request = ask(&a.svc, m, b.id()).unwrap().unwrap();
    a.client().tick().unwrap();
    let host = Arc::new(Recorder::default());
    let thread = crate::sync::SyncThread::start_offloading(
        b.client(),
        crate::sync::SyncTiming {
            tick: std::time::Duration::from_millis(20),
            poll: std::time::Duration::from_millis(5),
            presence: std::time::Duration::from_secs(3600),
        },
        Arc::clone(&host) as Arc<dyn OffloadHost>,
    )
    .unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    while host.for_others().is_empty() {
        assert!(std::time::Instant::now() < deadline, "never handed");
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    thread.stop();
    assert_eq!(host.for_others()[0].id, request);
    assert_eq!(b.request(request).state, RequestState::Claimed);
}

/// A device whose sync stops on a problem (here: another device removed it, T11.6c) runs one last
/// step before its thread ends: what it asked and is still open is withdrawn here and written here
/// at once (nothing it sends can reach the helper), and nothing asked of it is claimed.
#[test]
fn offload_a_device_whose_sync_stops_withdraws_what_it_asked_and_writes_here() {
    let relay = relay();
    let (a, b) = pair(&relay);
    set_offer_summaries(&a.svc, true).unwrap();
    let (ca, cb) = (a.client(), b.client());
    let unclaimed = meeting(&a.svc, "Invented planning", 21);
    let claimed = meeting(&a.svc, "Invented review", 22);
    let r_claimed = ask(&a.svc, claimed, b.id()).unwrap().unwrap();
    sync(&ca, &cb);
    b.step(None, &Recorder::default());
    sync(&cb, &ca);
    assert_eq!(a.request(r_claimed).state, RequestState::Claimed);
    // Asked after the helper's step: still unclaimed when sync stops.
    let r_unclaimed = ask(&a.svc, unclaimed, b.id()).unwrap().unwrap();
    // A request the other device asked of this one, before the removal.
    let theirs = meeting(&b.svc, "Invented handover", 23);
    let r_theirs = ask(&b.svc, theirs, a.id()).unwrap().unwrap();
    cb.tick().unwrap();
    ca.tick().unwrap();
    assert_eq!(a.request(r_theirs).state, RequestState::Requested);
    let open_before: Vec<RequestState> = [r_unclaimed, r_claimed]
        .iter()
        .map(|r| a.request(*r).state)
        .collect();
    assert!(open_before.iter().all(|s| s.is_open()), "{open_before:?}");

    crate::sync::remove_other_device(&b.svc, b.vault(), a.id()).unwrap();
    let host = Arc::new(Recorder::default());
    let thread = crate::sync::SyncThread::start_offloading(
        a.client(),
        crate::sync::SyncTiming {
            tick: std::time::Duration::from_millis(20),
            poll: std::time::Duration::from_millis(5),
            presence: std::time::Duration::from_secs(3600),
        },
        Arc::clone(&host) as Arc<dyn OffloadHost>,
    )
    .unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    while !thread.ended() {
        assert!(
            std::time::Instant::now() < deadline,
            "the thread never stopped"
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    thread.stop();
    // No 90 s wait and no presence: both are withdrawn and handed back at once.
    let mut here: Vec<(EntityId, HereReason)> = host
        .here()
        .into_iter()
        .map(|w| (w.request, w.why))
        .collect();
    here.sort_by_key(|(r, _)| *r);
    let mut want = vec![
        (r_unclaimed, HereReason::Withdrawn),
        (r_claimed, HereReason::Withdrawn),
    ];
    want.sort_by_key(|(r, _)| *r);
    assert_eq!(here, want);
    for r in [r_unclaimed, r_claimed] {
        assert_eq!(a.request(r).state, RequestState::Withdrawn);
    }
    // Asked of it: not claimed, not handed.
    assert_eq!(a.request(r_theirs).state, RequestState::Requested);
    assert!(host.for_others().is_empty());
}

/// A stopping thread whose host refuses the hand-back (a job of the meeting runs) keeps handing it
/// back every tick until the host starts it, then ends (T11.6c re-review).
#[test]
fn offload_a_refused_hand_back_after_sync_stops_is_retried_until_it_starts() {
    let relay = relay();
    let (a, b) = pair(&relay);
    let m = meeting(&a.svc, "Invented busy meeting", 25);
    let request = ask(&a.svc, m, b.id()).unwrap().unwrap();
    a.client().tick().unwrap();
    crate::sync::remove_other_device(&b.svc, b.vault(), a.id()).unwrap();
    let host = Arc::new(Recorder::default());
    host.refuse_here
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let thread = crate::sync::SyncThread::start_offloading(
        a.client(),
        crate::sync::SyncTiming {
            tick: std::time::Duration::from_millis(20),
            poll: std::time::Duration::from_millis(5),
            presence: std::time::Duration::from_secs(3600),
        },
        Arc::clone(&host) as Arc<dyn OffloadHost>,
    )
    .unwrap();
    let wait = |what: &str, done: &dyn Fn() -> bool| {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
        while !done() {
            assert!(std::time::Instant::now() < deadline, "{what}");
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    };
    // Refused, and handed again while the thread waits (it has not ended).
    wait("never handed again", &|| host.here().len() >= 3);
    assert!(
        !thread.ended(),
        "a refused hand-back keeps the thread going"
    );
    assert_eq!(a.request(request).state, RequestState::Withdrawn);
    // The job ends: the next hand-back starts, and the thread ends.
    host.refuse_here
        .store(false, std::sync::atomic::Ordering::SeqCst);
    wait("the thread never ended", &|| thread.ended());
    let handed = host.here();
    assert!(
        handed
            .iter()
            .all(|w| w.request == request && w.why == HereReason::Withdrawn)
    );
    let after = handed.len();
    std::thread::sleep(std::time::Duration::from_millis(100));
    assert_eq!(
        host.here().len(),
        after,
        "nothing is handed once it started"
    );
    thread.stop();
}

/// Turning sync off (`stop_saying_gone`) hands back what this device asked, as a stopping
/// problem does; a thread only replaced (`stop`) hands back nothing (T11.6c).
#[test]
fn offload_turning_sync_off_hands_back_what_this_device_asked() {
    let relay = relay();
    let (a, b) = pair(&relay);
    let timing = crate::sync::SyncTiming {
        tick: std::time::Duration::from_millis(20),
        poll: std::time::Duration::from_millis(5),
        presence: std::time::Duration::from_secs(3600),
    };
    let m = meeting(&a.svc, "Invented sync off", 24);
    let request = ask(&a.svc, m, b.id()).unwrap().unwrap();
    // Wait for one tick each time: the manual clock never reaches the 90 s claim window.
    let ticked = |host: &Recorder, thread: &crate::sync::SyncThread| {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
        while crate::sync::sync_status(&a.svc.store())
            .unwrap()
            .last_success_unix_ms
            .is_none()
        {
            assert!(std::time::Instant::now() < deadline, "never ticked");
            assert!(!thread.ended());
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(
            host.here().is_empty(),
            "nothing handed back while sync runs"
        );
    };
    let replaced = Arc::new(Recorder::default());
    let thread = crate::sync::SyncThread::start_offloading(
        a.client(),
        timing,
        Arc::clone(&replaced) as Arc<dyn OffloadHost>,
    )
    .unwrap();
    ticked(&replaced, &thread);
    thread.stop();
    assert!(
        replaced.here().is_empty(),
        "a replaced thread hands back nothing"
    );
    assert_eq!(a.request(request).state, RequestState::Requested);

    let host = Arc::new(Recorder::default());
    let thread = crate::sync::SyncThread::start_offloading(
        a.client(),
        timing,
        Arc::clone(&host) as Arc<dyn OffloadHost>,
    )
    .unwrap();
    ticked(&host, &thread);
    thread.stop_saying_gone();
    let here: Vec<(EntityId, HereReason)> = host
        .here()
        .into_iter()
        .map(|w| (w.request, w.why))
        .collect();
    assert_eq!(here, vec![(request, HereReason::Withdrawn)]);
    assert_eq!(a.request(request).state, RequestState::Withdrawn);
}

/// A claimed request whose helper has no presence at all is given five minutes from when the
/// asker first found it missing, not withdrawn at once (A5).
#[test]
fn offload_a_helper_missing_from_presence_gets_five_minutes() {
    let relay = relay();
    let (a, b) = pair(&relay);
    let (ca, cb) = (a.client(), b.client());
    let m = meeting(&a.svc, "Invented absence", 31);
    let request = ask(&a.svc, m, b.id()).unwrap().unwrap();
    sync(&ca, &cb);
    b.step(None, &Recorder::default());
    sync(&cb, &ca);
    assert_eq!(a.request(request).state, RequestState::Claimed);
    let asker = Recorder::default();
    // First found missing: the wait starts now.
    assert_eq!(a.step(Some(vec![]), &asker), StepReport::default());
    a.clock.advance_ms(STALE_AFTER_S * 1000 - 1);
    assert_eq!(a.step(Some(vec![]), &asker), StepReport::default());
    // Seen again in between resets it.
    let back = vec![peer(
        b.id(),
        Some(presence("D", true, Some(WriterKind::Local), false)),
        5,
    )];
    assert_eq!(a.step(Some(back), &asker), StepReport::default());
    a.clock.advance_ms(2);
    assert_eq!(
        a.step(Some(vec![]), &asker),
        StepReport::default(),
        "missing anew"
    );
    a.clock.advance_ms(STALE_AFTER_S * 1000);
    let r = a.step(Some(vec![]), &asker);
    assert_eq!((r.withdrawn, r.written_here), (1, 1), "{r:?}");
    assert_eq!(asker.here()[0].why, HereReason::HelperStale);
}

/// The 90 s and transcript waits run on the device clock from this device's own readings, not
/// on HLC wall time: a request stamped far ahead (a peer's clock observed) is not withdrawn early
/// (A4).
#[test]
fn offload_waits_run_on_the_device_clock_not_hlc_time() {
    let relay = relay();
    let (a, b) = pair(&relay);
    let (ca, cb) = (a.client(), b.client());
    // The asker's HLC runs an hour ahead of its clock, as after observing a peer's stamp.
    {
        let clock = Arc::clone(&a.svc.clock);
        let mut db = a.svc.store();
        let (_, hlc) = db.parts();
        let ahead = earshot_core::identity::HlcTimestamp {
            wall_ms: clock.now_unix_ms() + 60 * 60_000,
            logical: 0,
            node: DeviceId::from_bytes([0x7f; 16]),
        };
        let _ = hlc.observe(ahead, &*clock);
    }
    let m = meeting(&a.svc, "Invented clocks", 32);
    let request = ask(&a.svc, m, b.id()).unwrap().unwrap();
    assert!(
        a.request(request).meta.created_at.wall_ms > a.clock.now_unix_ms() + 30 * 60_000,
        "the request's HLC time is ahead of the clock"
    );
    let asker = Recorder::default();
    a.clock.advance_ms(CLAIM_WITHIN_MS - 1);
    assert_eq!(a.step(None, &asker), StepReport::default());
    a.clock.advance_ms(1);
    assert_eq!(a.step(None, &asker).withdrawn, 1, "90 s on the clock");

    // The helper's transcript wait, likewise: its claim is stamped by an HLC ahead of its clock.
    let second = meeting(&a.svc, "Invented clocks two", 33);
    sync(&ca, &cb);
    let req = ask(&a.svc, second, b.id()).unwrap().unwrap();
    {
        let mut db = a.svc.store();
        let clock = Arc::clone(&a.svc.clock);
        let (store, hlc) = db.parts();
        let mut r = store.summary_request(req).unwrap().unwrap();
        r.segment_count += 1;
        r.meta.touch(hlc, &*clock);
        store.put_summary_request(r).unwrap();
    }
    sync(&ca, &cb);
    let helper = Recorder::default();
    assert_eq!(b.step(None, &helper).claimed, 1);
    assert!(b.request(req).meta.updated_at.wall_ms > b.clock.now_unix_ms() + 30 * 60_000);
    b.clock.advance_ms(TRANSCRIPT_WAIT_MS - 1);
    assert_eq!(b.step(None, &helper), StepReport::default());
    b.clock.advance_ms(1);
    assert_eq!(b.step(None, &helper).declined, 1);
}

/// A write here the host refuses (a job of the meeting runs) is handed again at the next step
/// (A1).
#[test]
fn offload_a_refused_write_here_is_handed_again() {
    let relay = relay();
    let (a, b) = pair(&relay);
    let m = meeting(&a.svc, "Invented refusal", 34);
    let request = ask(&a.svc, m, b.id()).unwrap().unwrap();
    let asker = Recorder::default();
    asker
        .refuse_here
        .store(true, std::sync::atomic::Ordering::SeqCst);
    a.clock.advance_ms(CLAIM_WITHIN_MS);
    let r = a.step(None, &asker);
    assert_eq!((r.withdrawn, r.written_here), (1, 0), "{r:?}");
    asker
        .refuse_here
        .store(false, std::sync::atomic::Ordering::SeqCst);
    let r = a.step(None, &asker);
    assert_eq!(r.written_here, 1, "{r:?}");
    let here = asker.here();
    assert_eq!(here.len(), 2);
    assert!(here.iter().all(|h| h.request == request));
    assert_eq!(here[1].why, HereReason::Withdrawn);
    // Started: not handed again in this process.
    assert_eq!(a.step(None, &asker).written_here, 0);
}

/// After the summary written here is undone to an older one, a restart hands nothing: the user's
/// choice is kept (A3).
#[test]
fn offload_an_undone_summary_is_not_written_over_after_a_restart() {
    let relay = relay();
    let (a, b) = pair(&relay);
    let m = meeting(&a.svc, "Invented undo", 35);
    let server = MockServer::start(vec![reply("Older summary"), reply("Written here")]).unwrap();
    let service = remote_service(&a.svc, &server);
    let run = |svc: &Services| {
        service
            .summarize(svc, m, &mut *crate::os_seeded_rng(), &CancelToken::new())
            .unwrap()
    };
    run(&a.svc);
    a.clock.advance_ms(1_000);
    let _request = ask(&a.svc, m, b.id()).unwrap().unwrap();
    a.clock.advance_ms(CLAIM_WITHIN_MS);
    let asker = Recorder::default();
    assert_eq!(a.step(None, &asker).written_here, 1);
    // The host writes it here; the user switches back to the older one.
    a.clock.advance_ms(1_000);
    run(&a.svc);
    crate::edit::undo_summary(&a.svc, m).unwrap();
    let live: Vec<_> = a
        .svc
        .store()
        .summaries(m)
        .unwrap()
        .into_iter()
        .filter(|s| !s.meta.deleted)
        .collect();
    assert_eq!(live.len(), 1);
    assert_eq!(live[0].summary.title, "Older summary");
    let rh = Recorder::default();
    Offload::new().step(&a.svc, &mut || None, &rh).unwrap();
    assert!(
        rh.here().is_empty(),
        "a restart writes nothing over the undo"
    );
}

/// A meeting asked twice gets no second request, and an open request is looked at whatever its
/// age (A8).
#[test]
fn offload_a_meeting_is_asked_once_and_open_requests_never_age() {
    let relay = relay();
    let (a, b) = pair(&relay);
    let m = meeting(&a.svc, "Invented once", 36);
    let first = ask(&a.svc, m, b.id()).unwrap().unwrap();
    assert_eq!(ask(&a.svc, m, b.id()).unwrap(), Some(first), "the open one");
    let other = DeviceId::from_bytes([0x44; 16]);
    assert_eq!(
        ask_for(&a.svc, m, other).unwrap(),
        Some(Asked {
            request: first,
            target: b.id(),
        }),
        "one open request per meeting, naming its own device"
    );
    assert_eq!(a.svc.store().summary_requests().unwrap().len(), 1);
    let asker = Recorder::default();
    a.clock.advance_ms(LOOK_BACK_MS + 1);
    assert_eq!(a.step(None, &asker).written_here, 1);
}

/// The look-back runs from this device's own start or settling of the write here, never from the
/// request's HLC time: a request declined while the asker was away a week is still written, and so
/// is one whose write here was refused on waking; one started more than 7 days ago is left.
#[test]
fn offload_closed_requests_age_from_their_start_on_this_device() {
    let relay = relay();
    let (a, b) = pair(&relay);
    let (ca, cb) = (a.client(), b.client());
    set_offer_summaries(&b.svc, false).unwrap();
    let m = meeting(&a.svc, "Invented away", 41);
    let request = ask(&a.svc, m, b.id()).unwrap().unwrap();
    sync(&ca, &cb);
    // The helper declines while the asker is away for eight days.
    assert_eq!(b.step(None, &Recorder::default()).declined, 1);
    cb.tick().unwrap();
    a.clock.advance_ms(LOOK_BACK_MS + 24 * 60 * 60 * 1000);
    ca.tick().unwrap();
    assert_eq!(a.request(request).state, RequestState::Declined);
    // On waking the write here is refused (a job of the meeting runs) ...
    let asker = Recorder::default();
    asker
        .refuse_here
        .store(true, std::sync::atomic::Ordering::SeqCst);
    assert_eq!(a.step(None, &asker).written_here, 0);
    // ... and a week later, never started, it is still handed.
    a.clock.advance_ms(LOOK_BACK_MS + 1);
    asker
        .refuse_here
        .store(false, std::sync::atomic::Ordering::SeqCst);
    assert_eq!(
        a.step(None, &asker).written_here,
        1,
        "declined while away, then refused"
    );
    // Started: a new process within the week hands it again (no summary since) ...
    a.clock.advance_ms(LOOK_BACK_MS - 1);
    let rh = Recorder::default();
    Offload::new().step(&a.svc, &mut || None, &rh).unwrap();
    assert_eq!(rh.here().len(), 1);
    // ... and one more than a week after it started leaves it.
    a.clock.advance_ms(2);
    let late = Recorder::default();
    assert_eq!(
        Offload::new().step(&a.svc, &mut || None, &late).unwrap(),
        StepReport::default()
    );
    assert!(late.here().is_empty());
}

/// `offload-times` is `version u32 LE | JSON`, pinned byte for byte.
#[test]
fn offload_times_bytes_are_pinned() {
    let mut times = Times::default();
    times.requests.insert(
        "01900000-0000-7000-8000-000000000001".into(),
        Since {
            state: "requested".into(),
            since_ms: 5,
            absent_since_ms: None,
        },
    );
    times.closed.insert(
        "01900000-0000-7000-8000-000000000002".into(),
        Closed {
            seen_ms: 6,
            started_ms: Some(7),
            settled_ms: None,
        },
    );
    let mut want = 1u32.to_le_bytes().to_vec();
    want.extend_from_slice(
        br#"{"requests":{"01900000-0000-7000-8000-000000000001":{"state":"requested","since_ms":5,"absent_since_ms":null}},"closed":{"01900000-0000-7000-8000-000000000002":{"seen_ms":6,"started_ms":7,"settled_ms":null}}}"#,
    );
    assert_eq!(encode_times(&times), want);
    assert_eq!(read_times(Some(&want)), times);
    // Unreadable, another version, or nothing: no readings (every wait starts anew, so it grows).
    assert_eq!(read_times(Some(b"junk")), Times::default());
    assert_eq!(read_times(Some(&want[..3])), Times::default());
    let mut other = 2u32.to_le_bytes().to_vec();
    other.extend_from_slice(&want[4..]);
    assert_eq!(read_times(Some(&other)), Times::default());
    assert_eq!(read_times(None), Times::default());
}

/// A step's write merges: a reading an `ask` wrote after the step read is kept, a reading the
/// step pruned goes only when it is still what the step read.
#[test]
fn offload_times_merge_keeps_an_ask_meanwhile() {
    let since = |ms| Since {
        state: "requested".into(),
        since_ms: ms,
        absent_since_ms: None,
    };
    let before: std::collections::BTreeMap<String, Since> =
        [("old".to_owned(), since(1)), ("kept".to_owned(), since(2))].into();
    let after: std::collections::BTreeMap<String, Since> =
        [("kept".to_owned(), since(2)), ("new".to_owned(), since(3))].into();
    let mut stored = before.clone();
    stored.insert("asked".into(), since(4));
    merge(&mut stored, &before, &after);
    let keys: Vec<&str> = stored.keys().map(String::as_str).collect();
    assert_eq!(
        keys,
        ["asked", "kept", "new"],
        "the ask meanwhile is kept, old pruned"
    );
    // A pruned reading that changed meanwhile stays.
    let mut changed = before.clone();
    changed.insert("old".into(), since(9));
    merge(&mut changed, &before, &after);
    assert_eq!(changed["old"], since(9));
}

/// Readings are pruned: an open reading when the request closes, a closed one when the store no
/// longer holds the request; and they survive a new `Offload` on the same store.
#[test]
fn offload_times_are_pruned_and_survive_a_restart() {
    let relay = relay();
    let (a, b) = pair(&relay);
    let m = meeting(&a.svc, "Invented prune", 42);
    let request = ask(&a.svc, m, b.id()).unwrap().unwrap();
    let stored = || {
        read_times(
            a.svc
                .store()
                .local_value(OFFLOAD_TIMES_KEY)
                .unwrap()
                .as_deref(),
        )
    };
    assert!(stored().requests.contains_key(&request.to_string()));
    // A new process 60 s later: the 90 s still run from the ask.
    a.clock.advance_ms(60_000);
    assert_eq!(
        Offload::new()
            .step(&a.svc, &mut || None, &Recorder::default())
            .unwrap(),
        StepReport::default()
    );
    a.clock.advance_ms(CLAIM_WITHIN_MS - 60_000);
    let asker = Recorder::default();
    let fresh = Offload::new();
    assert_eq!(
        fresh.step(&a.svc, &mut || None, &asker).unwrap().withdrawn,
        1
    );
    // Next step: the open reading is pruned; the closed one holds the start.
    fresh.step(&a.svc, &mut || None, &asker).unwrap();
    let t = stored();
    assert!(!t.requests.contains_key(&request.to_string()));
    assert!(t.closed[&request.to_string()].started_ms.is_some());
    // The request tombstoned: its closed reading goes.
    {
        let mut db = a.svc.store();
        let clock = Arc::clone(&a.svc.clock);
        let (store, hlc) = db.parts();
        let mut r = store.summary_request(request).unwrap().unwrap();
        r.meta.mark_deleted(hlc, &*clock);
        store.put_summary_request(r).unwrap();
    }
    Offload::new()
        .step(&a.svc, &mut || None, &Recorder::default())
        .unwrap();
    let t = stored();
    assert!(
        !t.closed.contains_key(&request.to_string()),
        "pruned with the request: {t:?}"
    );
}

/// The summary step names the device its request asks: a meeting that already waited for another
/// device is reported as waiting for that one, not the newly chosen (re-review item 4).
#[test]
fn offload_after_meeting_names_the_device_of_an_open_request() {
    let relay = relay();
    let (a, b) = pair(&relay);
    let outcome = crate::host::tests::recorded(&a.svc, 331);
    let models = crate::backends::ModelDir::new(a._dir.0.join("models"));
    let sc = CancelToken::new();
    let never = MockServer::start(vec![reply("Never")]).unwrap();
    let own = remote_service(&a.svc, &never);
    // The final pass first, then a request to another device before the summary step.
    let earlier = DeviceId::from_bytes([0x46; 16]);
    let peers = helper_online(b.id());
    let asked = std::cell::Cell::new(None);
    let choose = || {
        asked.set(Some(
            ask(&a.svc, outcome.meeting, earlier).unwrap().unwrap(),
        ));
        decide(Some(b.id()), Some(&peers), false)
    };
    let host = crate::host::after_meeting_with(
        &a.svc,
        &outcome,
        &run_of(&models, &own, &sc, &choose, true),
        crate::host::tests::fake_post,
        &mut |_| {},
    );
    let crate::host::SummaryResult::Offloaded {
        request,
        device,
        name,
        writer,
    } = host.summary
    else {
        panic!("{:?}", host.summary);
    };
    assert_eq!(Some(request), asked.get());
    assert_eq!(device, earlier);
    assert_eq!((name.as_str(), writer), ("", None));
    assert!(never.requests().is_empty());
}

// ---- computer steps of the order of summary writers (T12.63a, docs/summary-order.md 4.3) ----

mod order_steps {
    use std::collections::HashMap;
    use std::sync::Mutex;
    use std::sync::atomic::Ordering;

    use super::*;
    use crate::host::{PreparedSummary, SkipReason, SummaryResult};
    use crate::summary_order::run::{
        Built, OutcomeWord, RunOrigin, RunPlan, RunRecord, RunStart, RunState, RunView,
        StepWriters, SummaryRun, run_summary_order, run_view,
    };
    use crate::summary_order::{OrderStep, SummaryOrder, WriterStep, set_summary_order};

    const LOCAL: OrderStep = OrderStep::LOCAL;
    const ENDPOINT: OrderStep = OrderStep::Writer(WriterStep::Endpoint);
    const CLAUDE: OrderStep = OrderStep::Writer(WriterStep::ClaudeCode);
    const DESK: DeviceId = DeviceId::from_bytes([0x51; 16]);
    const LAPTOP: DeviceId = DeviceId::from_bytes([0x52; 16]);

    /// Fake writers: each writer a mock endpoint (or not installed), each computer a decision.
    #[derive(Default)]
    struct Writers {
        mocks: HashMap<WriterStep, String>,
        computers: HashMap<DeviceId, OffloadChoice>,
        asked: Mutex<Vec<OrderStep>>,
    }

    impl Writers {
        fn mock(mut self, step: WriterStep, server: &MockServer) -> Self {
            self.mocks.insert(step, server.endpoint().to_string());
            self
        }

        fn asks(mut self, device: DeviceId) -> Self {
            self.computers.insert(
                device,
                OffloadChoice::Ask {
                    device,
                    name: "Invented desk".into(),
                    writer: WriterKind::Local,
                },
            );
            self
        }

        fn choice(mut self, device: DeviceId, choice: OffloadChoice) -> Self {
            self.computers.insert(device, choice);
            self
        }

        fn asked(&self) -> Vec<OrderStep> {
            self.asked.lock().unwrap().clone()
        }
    }

    impl StepWriters for Writers {
        fn writer(&self, services: &Services, step: WriterStep) -> Built {
            self.asked.lock().unwrap().push(OrderStep::Writer(step));
            match self.mocks.get(&step) {
                Some(url) => Built::Service(Box::new(PreparedSummary {
                    service: SummaryService::new(
                        &SummaryConfig::new(url.as_str(), "test-model"),
                        services.clock.clone(),
                    )
                    .unwrap(),
                    local: None,
                })),
                None => Built::Skip(SkipReason::NotInstalled {
                    model: "qwen3.5-4b",
                    why: "not installed".into(),
                }),
            }
        }

        fn computer(&self, _services: &Services, device: DeviceId) -> OffloadChoice {
            self.asked.lock().unwrap().push(OrderStep::Computer(device));
            self.computers
                .get(&device)
                .cloned()
                .unwrap_or_else(|| decide(Some(device), None, false))
        }
    }

    fn plan(writers: &Writers) -> RunPlan<'_> {
        RunPlan {
            writers,
            local_not_now: &|| false,
        }
    }

    fn order(steps: &[OrderStep]) -> SummaryOrder {
        SummaryOrder::new(steps.to_vec()).unwrap()
    }

    fn record(svc: &Services, m: EntityId) -> SummaryRun {
        match crate::summary_order::run::read(&svc.store(), m).unwrap() {
            RunRecord::Read(r) => r,
            other => panic!("{other:?}"),
        }
    }

    fn start(dev: &Device, m: EntityId, start_as: RunStart, writers: &Writers) -> SummaryResult {
        run_summary_order(&dev.svc, m, start_as, &plan(writers), &CancelToken::new()).summary
    }

    fn offloaded(result: SummaryResult) -> EntityId {
        match result {
            SummaryResult::Offloaded { request, .. } => request,
            other => panic!("{other:?}"),
        }
    }

    /// The asker's step 90 s after its last request: withdrawn unclaimed and handed back.
    fn hand_back(dev: &Device, host: &Recorder) -> StepReport {
        dev.clock.advance_ms(CLAIM_WITHIN_MS + 1);
        dev.step(None, host)
    }

    fn open_requests(dev: &Device, m: EntityId) -> usize {
        dev.svc
            .store()
            .summary_requests()
            .unwrap()
            .into_iter()
            .filter(|r| r.meeting_id == m && r.state.is_open())
            .count()
    }

    #[test]
    fn offload_order_a_computer_step_suspends_the_run_and_names_its_request() {
        let a = Device::new("offload-order-suspend");
        let m = meeting(&a.svc, "Invented planning", 501);
        let writers = Writers::default().asks(DESK);
        let request = offloaded(start(
            &a,
            m,
            RunStart::Order(order(&[OrderStep::Computer(DESK), ENDPOINT])),
            &writers,
        ));
        assert_eq!(
            writers.asked(),
            vec![OrderStep::Computer(DESK)],
            "suspended there"
        );
        let r = record(&a.svc, m);
        assert_eq!(
            (r.state, r.waiting_on, r.cursor),
            (RunState::Waiting, Some(request), 1)
        );
        assert_eq!(a.request(request).target_device, DESK);
        let aid = a.id();
        assert_eq!(waiting_for(&a.svc.store(), aid, m).unwrap(), Some(DESK));
    }

    #[test]
    fn offload_order_the_request_and_waiting_on_are_one_transaction() {
        let a = Device::new("offload-order-fault");
        let m = meeting(&a.svc, "Invented planning", 502);
        let writers = Writers::default().asks(DESK);
        crate::summary_order::run::fault::fail_ask();
        let result = start(
            &a,
            m,
            RunStart::Order(order(&[OrderStep::Computer(DESK)])),
            &writers,
        );
        assert!(
            matches!(result, SummaryResult::Skipped { .. }),
            "{result:?}"
        );
        assert!(
            a.svc.store().summary_requests().unwrap().is_empty(),
            "no request without a record naming it"
        );
        let r = record(&a.svc, m);
        assert_eq!((r.waiting_on, r.state), (None, RunState::Ended));
        assert_eq!(r.outcome_of(0), Some(OutcomeWord::Failed));
    }

    #[test]
    fn offload_order_a_hand_back_continues_the_same_run_from_its_copied_steps() {
        let a = Device::new("offload-order-continue");
        let m = meeting(&a.svc, "Invented planning", 503);
        let request = offloaded(start(
            &a,
            m,
            RunStart::Order(order(&[OrderStep::Computer(DESK), ENDPOINT])),
            &Writers::default().asks(DESK),
        ));
        let host = Recorder::default();
        let r = hand_back(&a, &host);
        assert_eq!((r.withdrawn, r.written_here), (1, 1), "{r:?}");
        let here = host.here();
        assert_eq!(here.len(), 1);
        assert_eq!(
            here[0].continuation,
            Continuation::Order {
                word: OutcomeWord::NoReply
            }
        );
        // The order changes meanwhile: the run goes on with the steps it copied.
        set_summary_order(&mut a.svc.store(), &order(&[LOCAL]), None).unwrap();
        let good = MockServer::start(vec![reply("Written by the endpoint")]).unwrap();
        let writers = Writers::default().mock(WriterStep::Endpoint, &good);
        let result = start(
            &a,
            m,
            RunStart::Continue {
                request,
                word: OutcomeWord::NoReply,
            },
            &writers,
        );
        assert!(matches!(result, SummaryResult::Stored { .. }), "{result:?}");
        assert_eq!(
            writers.asked(),
            vec![ENDPOINT],
            "never the current order's local step"
        );
        let r = record(&a.svc, m);
        assert_eq!((r.state, r.writer), (RunState::Ended, Some(1)));
        assert_eq!(r.outcome_of(0), Some(OutcomeWord::NoReply));
        // A second continuation of the same request finds nothing to continue.
        let again = run_summary_order(
            &a.svc,
            m,
            RunStart::Continue {
                request,
                word: OutcomeWord::NoReply,
            },
            &plan(&Writers::default()),
            &CancelToken::new(),
        );
        assert!(again.settled);
    }

    #[test]
    fn offload_order_a_restart_while_continuing_is_interrupted_and_sends_nothing_again() {
        let a = Device::new("offload-order-restart");
        let m = meeting(&a.svc, "Invented planning", 504);
        let request = offloaded(start(
            &a,
            m,
            RunStart::Order(order(&[OrderStep::Computer(DESK), ENDPOINT])),
            &Writers::default().asks(DESK),
        ));
        hand_back(&a, &Recorder::default());
        // The job's first transaction ran, then the app stopped.
        let mut continuing = record(&a.svc, m);
        continuing.waiting_on = None;
        continuing.state = RunState::Continuing;
        crate::local::set(
            &mut a.svc.store(),
            &crate::summary_order::run::record_key(m),
            &crate::summary_order::run::encode(&continuing),
        )
        .unwrap();
        let restarted = Offload::new();
        let host = Recorder::default();
        restarted.step(&a.svc, &mut || None, &host).unwrap();
        assert!(host.here().is_empty(), "nothing is handed again");
        assert!(matches!(
            run_view(&a.svc.store(), m).unwrap(),
            RunView::Unfinished(_)
        ));
        assert_eq!(a.request(request).state, RequestState::Withdrawn);
    }

    #[test]
    fn offload_order_a_named_hand_back_ends_the_run() {
        let a = Device::new("offload-order-named");
        let m = meeting(&a.svc, "Invented planning", 505);
        offloaded(start(
            &a,
            m,
            RunStart::Named(OrderStep::Computer(DESK)),
            &Writers::default().asks(DESK),
        ));
        let host = Recorder::default();
        let r = hand_back(&a, &host);
        assert_eq!((r.withdrawn, r.written_here), (1, 0), "{r:?}");
        assert!(host.here().is_empty(), "nothing is written here");
        let run = record(&a.svc, m);
        assert_eq!(
            (run.origin, run.state, run.writer),
            (RunOrigin::Named, RunState::Ended, None)
        );
        assert_eq!(run.outcome_of(0), Some(OutcomeWord::NoReply));
        // Settled: not read again, and a restart finds nothing to continue either.
        a.step(None, &host);
        Offload::new().step(&a.svc, &mut || None, &host).unwrap();
        assert!(host.here().is_empty());
        assert!(a.svc.store().summaries(m).unwrap().is_empty());
    }

    #[test]
    fn offload_order_two_computer_steps_in_one_run() {
        let a = Device::new("offload-order-two");
        let m = meeting(&a.svc, "Invented planning", 506);
        let steps = [
            OrderStep::Computer(DESK),
            OrderStep::Computer(LAPTOP),
            LOCAL,
        ];
        let first = offloaded(start(
            &a,
            m,
            RunStart::Order(order(&steps)),
            &Writers::default().asks(DESK),
        ));
        hand_back(&a, &Recorder::default());
        let writers = Writers::default().asks(LAPTOP);
        let second = offloaded(start(
            &a,
            m,
            RunStart::Continue {
                request: first,
                word: OutcomeWord::NoReply,
            },
            &writers,
        ));
        assert_ne!(first, second);
        assert_eq!(writers.asked(), vec![OrderStep::Computer(LAPTOP)]);
        let r = record(&a.svc, m);
        assert_eq!((r.waiting_on, r.cursor), (Some(second), 2));
        assert_eq!(r.outcome_of(0), Some(OutcomeWord::NoReply));
        assert_eq!(a.request(second).target_device, LAPTOP);
        assert_eq!(open_requests(&a, m), 1, "one open request per meeting");
    }

    #[test]
    fn offload_order_a_request_the_record_does_not_name_is_settled() {
        let a = Device::new("offload-order-other");
        let m = meeting(&a.svc, "Invented planning", 507);
        let old = ask(&a.svc, m, DESK).unwrap().unwrap();
        // A new run withdraws it and names none.
        let local_fails = Writers::default();
        start(&a, m, RunStart::Order(order(&[LOCAL])), &local_fails);
        assert_eq!(a.request(old).state, RequestState::Withdrawn);
        let host = Recorder::default();
        a.step(None, &host);
        assert!(host.here().is_empty(), "no job");
        a.clock.advance_ms(LOOK_BACK_MS + 1);
        a.step(None, &host);
        assert!(host.here().is_empty(), "not handed again");
    }

    #[test]
    fn offload_order_an_unreadable_record_on_a_hand_back_writes_and_sends_nothing() {
        let a = Device::new("offload-order-unreadable");
        let m = meeting(&a.svc, "Invented planning", 508);
        offloaded(start(
            &a,
            m,
            RunStart::Named(OrderStep::Computer(DESK)),
            &Writers::default().asks(DESK),
        ));
        crate::local::set(
            &mut a.svc.store(),
            &crate::summary_order::run::record_key(m),
            b"damaged",
        )
        .unwrap();
        let host = Recorder::default();
        let r = hand_back(&a, &host);
        assert_eq!(r.written_here, 0, "{r:?}");
        assert!(host.here().is_empty(), "never the legacy branch");
        assert!(a.svc.store().summaries(m).unwrap().is_empty());
        assert_eq!(run_view(&a.svc.store(), m).unwrap(), RunView::Unreadable);
    }

    #[test]
    fn offload_order_a_request_with_no_record_is_written_here_as_before() {
        let a = Device::new("offload-order-legacy");
        let m = meeting(&a.svc, "Invented planning", 509);
        let request = ask(&a.svc, m, DESK).unwrap().unwrap();
        let host = Recorder::default();
        assert_eq!(hand_back(&a, &host).written_here, 1);
        assert_eq!(host.here()[0].continuation, Continuation::Legacy);
        assert_eq!(host.here()[0].request, request);
        // The legacy job writes with the order's first writer step and records an ended run.
        set_summary_order(&mut a.svc.store(), &order(&[CLAUDE, LOCAL]), None).unwrap();
        let good = MockServer::start(vec![reply("Written here")]).unwrap();
        let writers = Writers::default().mock(WriterStep::ClaudeCode, &good);
        let result = start(&a, m, RunStart::Legacy, &writers);
        assert!(matches!(result, SummaryResult::Stored { .. }), "{result:?}");
        assert_eq!(writers.asked(), vec![CLAUDE], "one writer, no next step");
        let r = record(&a.svc, m);
        assert_eq!(
            (r.origin, r.state, r.writer),
            (RunOrigin::Named, RunState::Ended, Some(0))
        );
    }

    #[test]
    fn offload_order_continue_run_answers_started_refused_and_settled() {
        let a = Device::new("offload-order-answers");
        let m = meeting(&a.svc, "Invented planning", 510);
        offloaded(start(
            &a,
            m,
            RunStart::Order(order(&[OrderStep::Computer(DESK), LOCAL])),
            &Writers::default().asks(DESK),
        ));
        let host = Recorder::default();
        host.refuse_here.store(true, Ordering::SeqCst);
        assert_eq!(hand_back(&a, &host).written_here, 0);
        // Refused: handed again at the next step.
        assert_eq!(a.step(None, &host).written_here, 0);
        assert_eq!(host.here().len(), 2);
        // Settled: not handed again.
        host.refuse_here.store(false, Ordering::SeqCst);
        host.settle_here.store(true, Ordering::SeqCst);
        a.step(None, &host);
        assert_eq!(host.here().len(), 3);
        a.step(None, &host);
        assert_eq!(host.here().len(), 3, "settled");
        // Started: counted, and handed once in the process.
        let b = Device::new("offload-order-answers-b");
        let m2 = meeting(&b.svc, "Invented planning", 511);
        offloaded(start(
            &b,
            m2,
            RunStart::Order(order(&[OrderStep::Computer(DESK), LOCAL])),
            &Writers::default().asks(DESK),
        ));
        let started = Recorder::default();
        assert_eq!(hand_back(&b, &started).written_here, 1);
        assert_eq!(b.step(None, &started).written_here, 0);
        assert_eq!(started.here().len(), 1);
    }

    #[test]
    fn offload_order_an_unavailable_computer_moves_on_and_keeps_its_banner() {
        let a = Device::new("offload-order-unavailable");
        let m = meeting(&a.svc, "Invented planning", 512);
        let good = MockServer::start(vec![reply("Written here")]).unwrap();
        let writers = Writers::default()
            .choice(
                DESK,
                OffloadChoice::HereInstead {
                    device: DESK,
                    name: Some("Invented desk".into()),
                    why: Unavailable::NotOffering,
                },
            )
            .mock(WriterStep::Local, &good);
        let out = run_summary_order(
            &a.svc,
            m,
            RunStart::Order(order(&[OrderStep::Computer(DESK), LOCAL])),
            &plan(&writers),
            &CancelToken::new(),
        );
        assert!(matches!(out.summary, SummaryResult::Stored { .. }));
        assert!(matches!(
            out.offload,
            Some(OffloadChoice::HereInstead {
                why: Unavailable::NotOffering,
                ..
            })
        ));
        assert_eq!(
            record(&a.svc, m).outcome_of(0),
            Some(OutcomeWord::NotOffering)
        );
        assert!(a.svc.store().summary_requests().unwrap().is_empty());
    }

    #[test]
    fn offload_order_the_helpers_summary_ends_the_waiting_run() {
        let relay = relay();
        let (a, b) = pair(&relay);
        let (ca, cb) = (a.client(), b.client());
        let m = meeting(&a.svc, "Invented planning", 513);
        let peers = helper_online(b.id());
        let writers = Writers::default().choice(b.id(), decide(Some(b.id()), Some(&peers), false));
        let request = offloaded(start(
            &a,
            m,
            RunStart::Order(order(&[OrderStep::Computer(b.id()), LOCAL])),
            &writers,
        ));
        sync(&ca, &cb);
        let helper = Recorder::default();
        b.step(None, &helper);
        let handed = helper.for_others();
        assert_eq!(handed.len(), 1);
        let server = MockServer::start(vec![reply("Written by the helper")]).unwrap();
        let done = write_for_request(
            &b.svc,
            &MeetingQueue::new(),
            &handed[0],
            &|_| remote_writer(&b.svc, &server),
            &CancelToken::new(),
        );
        assert!(matches!(done, HelperOutcome::Done { .. }), "{done:?}");
        sync(&cb, &ca);
        assert_eq!(a.request(request).state, RequestState::Done);
        // Before the asker's step the view already reads it as ended.
        assert!(matches!(
            run_view(&a.svc.store(), m).unwrap(),
            RunView::Ended(_)
        ));
        assert_eq!(record(&a.svc, m).state, RunState::Waiting);
        let asker = Recorder::default();
        a.step(None, &asker);
        assert!(asker.here().is_empty());
        let r = record(&a.svc, m);
        assert_eq!(
            (r.state, r.writer, r.waiting_on),
            (RunState::Ended, Some(0), None)
        );
        assert_eq!(a.live_summaries(m), 1);
    }
}
