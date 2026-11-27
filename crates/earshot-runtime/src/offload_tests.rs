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
    /// What `writing()` answers (T14.66).
    writing: Mutex<Vec<EntityId>>,
    /// Requests the step asked to cancel.
    cancelled: Mutex<Vec<EntityId>>,
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
    fn writing(&self) -> Vec<EntityId> {
        self.writing.lock().unwrap().clone()
    }
    fn cancel_writing(&self, request: EntityId) {
        self.cancelled.lock().unwrap().push(request);
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
        model_place: None,
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
    // The window hears of it (T12.81): the withdrawal and hand-back name the meeting.
    assert_eq!(r.meetings.iter().copied().collect::<Vec<_>>(), vec![m]);
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

/// T12.70: a deleted meeting's readings go with it, open and closed, whether it was deleted here or
/// its tombstone was pulled from another device; other meetings' readings stay.
#[test]
fn offload_times_of_a_deleted_meeting_go_with_its_deletion() {
    let relay = relay();
    let (a, b) = pair(&relay);
    let (ca, cb) = (a.client(), b.client());
    let stored = || {
        read_times(
            a.svc
                .store()
                .local_value(OFFLOAD_TIMES_KEY)
                .unwrap()
                .as_deref(),
        )
    };
    let gone = meeting(&a.svc, "Invented gone", 43);
    let kept = meeting(&a.svc, "Invented kept", 44);
    let (gone_r, kept_r) = (
        ask(&a.svc, gone, b.id()).unwrap().unwrap().to_string(),
        ask(&a.svc, kept, b.id()).unwrap().unwrap().to_string(),
    );
    // Both withdrawn unclaimed, then closed readings with their start.
    a.clock.advance_ms(CLAIM_WITHIN_MS + 1);
    let asker = Recorder::default();
    let fresh = Offload::new();
    assert_eq!(
        fresh.step(&a.svc, &mut || None, &asker).unwrap().withdrawn,
        2
    );
    fresh.step(&a.svc, &mut || None, &asker).unwrap();
    let t = stored();
    assert!(t.closed.contains_key(&gone_r) && t.closed.contains_key(&kept_r));
    a.svc.delete_meeting(gone).unwrap();
    let t = stored();
    assert!(!t.closed.contains_key(&gone_r), "deleted here: {t:?}");
    assert!(t.closed.contains_key(&kept_r), "{t:?}");
    // An open reading of a meeting deleted on the other device goes when the tombstone arrives.
    let pulled = meeting(&a.svc, "Invented pulled", 45);
    let pulled_r = ask(&a.svc, pulled, b.id()).unwrap().unwrap().to_string();
    assert!(stored().requests.contains_key(&pulled_r));
    sync(&ca, &cb);
    b.svc.delete_meeting(pulled).unwrap();
    cb.tick().unwrap();
    ca.tick().unwrap();
    assert!(
        a.svc.store().meeting(pulled).unwrap().unwrap().meta.deleted,
        "the tombstone was pulled"
    );
    let t = stored();
    assert!(!t.requests.contains_key(&pulled_r), "pulled: {t:?}");
    assert!(t.closed.contains_key(&kept_r), "{t:?}");
}

/// T12.70 (review): a deletion that fails before its transaction (an audio file that cannot be
/// removed) keeps the meeting and its readings, so a closed request is not handed again; the
/// deletion that then works drops them.
#[test]
fn offload_times_survive_a_deletion_that_fails_before_it_commits() {
    let relay = relay();
    let (a, b) = pair(&relay);
    let stored = || {
        read_times(
            a.svc
                .store()
                .local_value(OFFLOAD_TIMES_KEY)
                .unwrap()
                .as_deref(),
        )
    };
    let m = meeting(&a.svc, "Invented stuck", 46);
    let request = ask(&a.svc, m, b.id()).unwrap().unwrap().to_string();
    a.clock.advance_ms(CLAIM_WITHIN_MS + 1);
    let fresh = Offload::new();
    assert_eq!(
        fresh
            .step(&a.svc, &mut || None, &Recorder::default())
            .unwrap()
            .withdrawn,
        1
    );
    fresh
        .step(&a.svc, &mut || None, &Recorder::default())
        .unwrap();
    assert!(stored().closed.contains_key(&request));
    // A mic recording whose file is a non-empty folder: removing it fails.
    let audio = {
        let mut db = a.svc.store();
        let track = db
            .tracks(m)
            .unwrap()
            .into_iter()
            .find(|t| t.kind == earshot_core::capture::TrackId::Mic)
            .unwrap();
        let mut w = a
            .svc
            .data
            .track_writer(m, earshot_core::capture::TrackId::Mic, 16_000)
            .unwrap();
        w.write(&[0.2; 320]).unwrap();
        let clock = Arc::clone(&a.svc.clock);
        db.finalize_track_audio(
            w,
            &a.svc.data,
            &track,
            earshot_store::AudioProvenance {
                pipeline_version: "p".into(),
                aec_version: None,
            },
            &*clock,
            &mut <rand_chacha::ChaCha8Rng as rand_core::SeedableRng>::seed_from_u64(46),
        )
        .unwrap();
        a.svc
            .data
            .resolve(&earshot_store::DataDir::audio_rel(
                m,
                earshot_core::capture::TrackId::Mic,
            ))
            .unwrap()
    };
    std::fs::remove_file(&audio).unwrap();
    std::fs::create_dir_all(audio.join("held")).unwrap();
    assert!(a.svc.delete_meeting(m).is_err());
    assert!(!a.svc.store().meeting(m).unwrap().unwrap().meta.deleted);
    assert!(
        stored().closed.contains_key(&request),
        "kept: {:?}",
        stored()
    );
    std::fs::remove_dir_all(&audio).unwrap();
    a.svc.delete_meeting(m).unwrap();
    assert!(!stored().closed.contains_key(&request), "{:?}", stored());
}

/// T12.70 (review): a step that read the readings before a meeting's deletion and changed one of
/// that meeting's never writes it back.
#[test]
fn offload_a_step_merging_after_a_deletion_does_not_bring_back_its_readings() {
    let relay = relay();
    let (a, b) = pair(&relay);
    let stored = || {
        read_times(
            a.svc
                .store()
                .local_value(OFFLOAD_TIMES_KEY)
                .unwrap()
                .as_deref(),
        )
    };
    let gone = meeting(&a.svc, "Invented merged", 47);
    let kept = meeting(&a.svc, "Invented beside", 48);
    let gone_r = ask(&a.svc, gone, b.id()).unwrap().unwrap().to_string();
    let kept_r = ask(&a.svc, kept, b.id()).unwrap().unwrap().to_string();
    // What a step read, and what it would write: both readings changed.
    let before = stored();
    let mut after = before.clone();
    for since in after.requests.values_mut() {
        since.absent_since_ms = Some(7);
    }
    a.svc.delete_meeting(gone).unwrap();
    assert!(!stored().requests.contains_key(&gone_r));
    a.svc
        .store()
        .transaction(|tx| store_step_times(tx, &before, &after))
        .unwrap();
    let t = stored();
    assert!(!t.requests.contains_key(&gone_r), "brought back: {t:?}");
    assert_eq!(t.requests[&kept_r].absent_since_ms, Some(7), "{t:?}");
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

// ---- presence the sync thread reads names the computers (T14.50) ----

#[test]
fn offload_what_a_presence_says_of_itself_is_its_name_writer_and_model_place() {
    use crate::sync::{Heard, KIND_SERVER, ModelPlace};
    let device = DeviceId::from_bytes([0x61; 16]);
    let heard = |p: Presence| peer(device, Some(p), 10).heard().unwrap();
    // An app that offers summaries: its name and writer kind, never a place.
    let mut app = presence("Invented desk", true, Some(WriterKind::Local), false);
    app.model_place = Some(ModelPlace::GraphicsCard.word().to_owned());
    assert_eq!(
        heard(app),
        Heard {
            device,
            name: Some("Invented desk".into()),
            writer_kind: Some(WriterKind::Local),
            model_place: None,
        }
    );
    // A summary worker writing with its own local model says where that runs.
    let mut worker = presence("Invented server", true, Some(WriterKind::Local), false);
    worker.kind = KIND_SERVER.into();
    worker.model_place = Some(ModelPlace::Processor.word().to_owned());
    assert_eq!(
        heard(worker.clone()).model_place,
        Some(ModelPlace::Processor)
    );
    // Not while its writer is another service, and a word this build does not know is none.
    let mut remote = worker.clone();
    remote.writer_kind = Some(WriterKind::OpenaiCompatible.word().to_owned());
    assert_eq!(
        (heard(remote.clone()).writer_kind, heard(remote).model_place),
        (Some(WriterKind::OpenaiCompatible), None)
    );
    let mut odd = worker.clone();
    odd.model_place = Some("an invented place".into());
    assert_eq!(heard(odd).model_place, None);
    // A device that does not offer summaries names no writer; an empty name is no name.
    let quiet = presence("", false, Some(WriterKind::Codex), false);
    assert_eq!(
        heard(quiet),
        Heard {
            device,
            name: None,
            writer_kind: None,
            model_place: None,
        }
    );
    // A presence that cannot be read says nothing.
    assert_eq!(peer(device, None, 10).heard(), None);
}

#[test]
fn offload_meetings_waiting_on_a_device_are_those_with_an_open_request_asked_of_it() {
    let relay = relay();
    let (a, b) = pair(&relay);
    let elsewhere = DeviceId::from_bytes([0x62; 16]);
    let waiting = meeting(&a.svc, "Invented planning", 41);
    let other = meeting(&a.svc, "Invented review", 42);
    let withdrawn = meeting(&a.svc, "Invented retro", 43);
    ask(&a.svc, waiting, b.id()).unwrap().unwrap();
    ask(&a.svc, other, elsewhere).unwrap().unwrap();
    let gone = ask(&a.svc, withdrawn, b.id()).unwrap().unwrap();
    assert!(
        move_request(
            &a.svc,
            gone,
            RequestState::Requested,
            RequestState::Withdrawn,
            ""
        )
        .unwrap()
    );
    // Read before any store guard is taken: a device's id comes from its store.
    let (asker, helper) = (a.id(), b.id());
    let on = |devices: &[DeviceId]| {
        meetings_waiting_on(&a.svc.store(), asker, devices)
            .unwrap()
            .into_iter()
            .collect::<Vec<_>>()
    };
    assert_eq!(on(&[b.id()]), vec![waiting]);
    assert_eq!(on(&[elsewhere]), vec![other]);
    assert_eq!(on(&[]), Vec::<EntityId>::new());
    let mut both = on(&[b.id(), elsewhere]);
    both.sort();
    let mut want = vec![waiting, other];
    want.sort();
    assert_eq!(both, want);
    // Only what this device asked: the helper waits for nothing.
    assert!(
        meetings_waiting_on(&a.svc.store(), helper, &[helper])
            .unwrap()
            .is_empty()
    );
}

/// After a restart nothing in the process knows the other computers' names. The sync thread
/// reads presence for a claimed request it waits on; the host's listener hears the names from
/// that same read, and the meeting that waits is named to the window once, so its banner can
/// show the name. No request is made for it.
#[test]
fn offload_presence_the_sync_thread_reads_names_the_computer_a_meeting_waits_for() {
    use crate::sync::{Heard, SyncChanges};
    let relay = relay();
    let (a, b) = pair(&relay);
    let (ca, cb) = (a.client(), b.client());
    crate::sync::set_device_name(&b.svc, "Invented laptop").unwrap();
    cb.send_presence(false).unwrap();
    ca.send_presence(false).unwrap();
    let m = meeting(&a.svc, "Invented planning", 44);
    let other = meeting(&a.svc, "Invented review", 45);
    let request = ask(&a.svc, m, b.id()).unwrap().unwrap();
    sync(&ca, &cb);
    b.step(None, &Recorder::default());
    sync(&cb, &ca);
    assert_eq!(a.request(request).state, RequestState::Claimed);

    // The host's side, as the app's: names kept by device, a device answered only when its
    // name is new or changed.
    let heard: Arc<Mutex<Vec<Heard>>> = Arc::default();
    let told: Arc<Mutex<Vec<SyncChanges>>> = Arc::default();
    let client = {
        let (heard, told) = (Arc::clone(&heard), Arc::clone(&told));
        let known: Mutex<std::collections::BTreeMap<DeviceId, String>> = Mutex::default();
        a.client()
            .with_presence(Arc::new(move |said: &[Heard]| {
                heard.lock().unwrap().extend(said.iter().cloned());
                let mut known = known.lock().unwrap();
                said.iter()
                    .filter(|h| {
                        h.name
                            .as_ref()
                            .is_some_and(|n| known.insert(h.device, n.clone()).as_ref() != Some(n))
                    })
                    .map(|h| h.device)
                    .collect()
            }))
            .with_changes(Arc::new(move |c: &SyncChanges| {
                told.lock().unwrap().push(c.clone());
            }))
    };
    let requests = client.audit_log();
    assert!(requests.entries().is_empty());
    let thread = crate::sync::SyncThread::start_offloading(
        client,
        crate::sync::SyncTiming {
            tick: std::time::Duration::from_millis(20),
            poll: std::time::Duration::from_millis(5),
            presence: std::time::Duration::from_secs(3600),
        },
        Arc::new(Recorder::default()) as Arc<dyn OffloadHost>,
    )
    .unwrap();
    let named = |told: &Mutex<Vec<SyncChanges>>| {
        told.lock()
            .unwrap()
            .iter()
            .filter(|c| c.meetings.contains(&m))
            .count()
    };
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    while named(&told) == 0 {
        assert!(
            std::time::Instant::now() < deadline,
            "the waiting meeting was never named to the window"
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    // A few more passes: the name is the same, so the meeting is not named again.
    let passes = heard.lock().unwrap().len();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    while heard.lock().unwrap().len() < passes + 3 {
        assert!(std::time::Instant::now() < deadline, "the thread stopped");
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    thread.stop();
    assert_eq!(named(&told), 1, "{:?}", told.lock().unwrap());
    assert!(
        told.lock()
            .unwrap()
            .iter()
            .all(|c| !c.meetings.contains(&other)),
        "a meeting that waits for nobody is not named"
    );
    let heard = heard.lock().unwrap();
    assert!(!heard.is_empty());
    for h in heard.iter() {
        assert_eq!(h.device, b.id(), "this device's own presence is left out");
        assert_eq!(h.name.as_deref(), Some("Invented laptop"));
    }
    // Each hearing is a read of presence the step made for its own work (one a step, while it
    // waits on the claimed request): no request was made to hear the names.
    let presence_reads = requests
        .entries()
        .iter()
        .filter(|e| e.method == "GET" && e.path == "/v1/presence" && e.status == Some(200))
        .count();
    assert_eq!(presence_reads, heard.len());
    // Still waiting: hearing a name changes nothing of the request.
    assert_eq!(a.request(request).state, RequestState::Claimed);
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
            order: None,
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

    // ---- the step after a tick reads no request while nothing can have changed (T14.50) ----

    fn scans(offload: &Offload) -> usize {
        offload.scans.load(Ordering::SeqCst)
    }

    /// The step as the sync thread runs it after a tick that says `rows_changed`.
    fn after_tick(dev: &Device, host: &Recorder, rows_changed: bool) -> StepReport {
        let mut report = StepReport::default();
        dev.offload
            .step_after_tick_into(&dev.svc, &mut || None, host, rows_changed, &mut report)
            .unwrap();
        report
    }

    #[test]
    fn offload_a_step_after_a_tick_reads_no_request_while_nothing_can_have_changed() {
        let relay = relay();
        let (a, b) = pair(&relay);
        let host = Recorder::default();
        // The first step of a process always reads.
        assert_eq!(after_tick(&a, &host, false), StepReport::default());
        assert_eq!(scans(&a.offload), 1);
        // It found nothing to do or wait for: the next ones read no request.
        for _ in 0..3 {
            a.clock.advance_ms(30_000);
            assert_eq!(after_tick(&a, &host, false), StepReport::default());
        }
        assert_eq!(scans(&a.offload), 1);
        // A tick that changed rows here: read again.
        after_tick(&a, &host, true);
        assert_eq!(scans(&a.offload), 2);
        after_tick(&a, &host, false);
        assert_eq!(scans(&a.offload), 2);
        // The clock went back: read again (a closed request's age is judged by it).
        a.clock.set_ms(a.clock.now_unix_ms() - 60_000);
        after_tick(&a, &host, false);
        assert_eq!(scans(&a.offload), 3);
        after_tick(&a, &host, false);
        assert_eq!(scans(&a.offload), 3);
        // A step that is not the thread's after-tick step always reads.
        a.step(None, &host);
        assert_eq!(scans(&a.offload), 4);
        after_tick(&a, &host, false);
        assert_eq!(scans(&a.offload), 4);

        // This device asks: its own write is seen without any tick saying so, and while the
        // request is open every step reads (its 90 seconds run on the clock).
        let m = meeting(&a.svc, "Invented planning", 521);
        let request = ask(&a.svc, m, b.id()).unwrap().unwrap();
        assert_eq!(after_tick(&a, &host, false), StepReport::default());
        assert_eq!(scans(&a.offload), 5);
        a.clock.advance_ms(CLAIM_WITHIN_MS - 1);
        assert_eq!(after_tick(&a, &host, false), StepReport::default());
        assert_eq!(scans(&a.offload), 6);
        a.clock.advance_ms(1);
        let r = after_tick(&a, &host, false);
        assert_eq!((r.withdrawn, r.written_here), (1, 1), "{r:?}");
        assert_eq!(scans(&a.offload), 7);
        assert_eq!(a.request(request).state, RequestState::Withdrawn);
        // Handed back and not yet summarised: still looked at, step after step.
        after_tick(&a, &host, false);
        after_tick(&a, &host, false);
        assert_eq!(scans(&a.offload), 9);
        // Left alone for its age: one more read finds nothing, and the steps after it skip.
        a.clock.advance_ms(LOOK_BACK_MS + 1);
        after_tick(&a, &host, false);
        assert_eq!(scans(&a.offload), 10);
        after_tick(&a, &host, false);
        after_tick(&a, &host, false);
        assert_eq!(scans(&a.offload), 10);
        // The last step of a thread whose sync stopped always reads, and leaves nothing behind
        // for a later step to skip on.
        a.offload.step_sync_stopped(&a.svc, &host).unwrap();
        assert_eq!(scans(&a.offload), 11);
        after_tick(&a, &host, false);
        assert_eq!(scans(&a.offload), 12);
        assert_eq!(host.here().len(), 1, "handed once in all");
    }

    /// The pages are varied and the same on every run.
    struct Dice(u64);

    impl Dice {
        fn roll(&mut self, below: u64) -> u64 {
            self.0 = self
                .0
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            (self.0 >> 33) % below
        }
    }

    /// One device of the varied run: its offload state (replaced on a "restart"), its host, and
    /// whether a tick changed rows here since its last step.
    struct Side<'d> {
        dev: &'d Device,
        offload: Offload,
        host: Recorder,
        rows_changed: bool,
    }

    #[derive(Default, Debug)]
    struct Tally {
        skipped: usize,
        scanned: usize,
        acted: usize,
    }

    /// Everything a step may change, as text.
    fn everything(side: &Side<'_>, meetings: &[EntityId]) -> Vec<String> {
        let db = side.dev.svc.store();
        let mut out: Vec<String> = db
            .summary_requests()
            .unwrap()
            .iter()
            .map(|r| format!("{r:?}"))
            .collect();
        out.push(format!("{:?}", db.local_value(OFFLOAD_TIMES_KEY).unwrap()));
        for m in meetings {
            out.push(format!("{:?}", crate::summary_order::run::read(&db, *m)));
            out.push(format!("{:?}", db.summaries(*m).map(|s| s.len())));
        }
        out.push(format!(
            "{:?} {:?}",
            side.host.here(),
            side.host
                .for_others()
                .iter()
                .map(|r| r.id)
                .collect::<Vec<_>>()
        ));
        out
    }

    /// One step. When the thread's step after a tick would skip the scan, the scan is run all
    /// the same and must have done nothing at all; the skipped step's state is then put back,
    /// so a run of skips is judged against the scan they follow, as in the thread.
    fn checked_step(
        side: &mut Side<'_>,
        peers: Option<Vec<Peer>>,
        meetings: &[EntityId],
        tally: &mut Tally,
        at: &str,
    ) {
        let would_skip = !side.rows_changed && side.offload.nothing_to_do(&side.dev.svc);
        side.rows_changed = false;
        let idle = *side.offload.idle.lock().unwrap();
        let before = everything(side, meetings);
        let report = side
            .offload
            .step(&side.dev.svc, &mut || peers.clone(), &side.host)
            .unwrap();
        let after = everything(side, meetings);
        if would_skip {
            assert_eq!(report, StepReport::default(), "{at}: a skipped step acted");
            assert_eq!(after, before, "{at}: a skipped step changed something");
            *side.offload.idle.lock().unwrap() = idle;
            tally.skipped += 1;
        } else {
            tally.scanned += 1;
            if report != StepReport::default() || after != before {
                tally.acted += 1;
            }
        }
    }

    /// T14.50: over a varied run of two devices (asks with and without a run of the order,
    /// syncs, clocks moving on and back, the helper writing or declining, summaries stored,
    /// meetings deleted, hosts refusing, restarts), the step after a tick never skips a scan
    /// that would have done anything.
    #[test]
    fn offload_a_step_skipped_after_a_tick_is_one_a_scan_would_have_done_nothing_in() {
        use rand_core::SeedableRng;
        let mut tally = Tally::default();
        for seed in 0..5u64 {
            let relay = relay();
            let (da, db) = pair(&relay);
            let (ca, cb) = (da.client(), db.client());
            let mut dice = Dice(0x0FF1_0AD0 + seed);
            let mut a = Side {
                dev: &da,
                offload: Offload::new(),
                host: Recorder::default(),
                rows_changed: false,
            };
            let mut b = Side {
                dev: &db,
                offload: Offload::new(),
                host: Recorder::default(),
                rows_changed: false,
            };
            let mut meetings: Vec<EntityId> = Vec::new();
            let mut written_for = 0;
            for turn in 0..90u64 {
                let at = format!("seed {seed} turn {turn}");
                match dice.roll(13) {
                    // This device asks, by itself or as a computer step of a run of the order.
                    0 | 1 => {
                        let m = meeting(&da.svc, "Invented meeting", 600 + seed * 100 + turn);
                        meetings.push(m);
                        if dice.roll(2) == 0 {
                            ask(&da.svc, m, db.id()).unwrap();
                        } else {
                            let peers = helper_online(db.id());
                            let writers = Writers::default()
                                .choice(db.id(), decide(Some(db.id()), Some(&peers), false));
                            start(
                                &da,
                                m,
                                RunStart::Order(order(&[OrderStep::Computer(db.id()), ENDPOINT])),
                                &writers,
                            );
                        }
                    }
                    // A sync both ways; each device hears whether its ticks changed rows.
                    2..=4 => {
                        a.rows_changed |= ca.tick().unwrap().changed();
                        b.rows_changed |= cb.tick().unwrap().changed();
                        b.rows_changed |= cb.tick().unwrap().changed();
                        a.rows_changed |= ca.tick().unwrap().changed();
                    }
                    // Time passes on one device: a second, most of the claim window, past it,
                    // past the stale and transcript waits, past the look-back.
                    5 | 6 => {
                        let by = [
                            1_000,
                            45_000,
                            CLAIM_WITHIN_MS + 1,
                            6 * 60_000,
                            LOOK_BACK_MS + 1,
                        ][usize::try_from(dice.roll(5)).unwrap()];
                        if dice.roll(2) == 0 {
                            da.clock.advance_ms(by);
                        } else {
                            db.clock.advance_ms(by);
                        }
                    }
                    // The helper's host does the next piece of work it was handed: it writes
                    // the summary, or declines.
                    7 | 8 => {
                        let handed = b.host.for_others();
                        if let Some(request) = handed.get(written_for) {
                            written_for += 1;
                            if dice.roll(2) == 0 {
                                let server =
                                    MockServer::start(vec![reply("Written by the helper")])
                                        .unwrap();
                                write_for_request(
                                    &db.svc,
                                    &MeetingQueue::new(),
                                    request,
                                    &|_| remote_writer(&db.svc, &server),
                                    &CancelToken::new(),
                                );
                            } else {
                                let _ = decline(&db.svc, request.id, reason::FAILED);
                            }
                        }
                    }
                    // The asker's host refuses, settles or starts what it is handed.
                    9 => {
                        let mode = dice.roll(3);
                        a.host.refuse_here.store(mode == 1, Ordering::SeqCst);
                        a.host.settle_here.store(mode == 2, Ordering::SeqCst);
                    }
                    // A summary is stored on the asker (a write here that finished).
                    10 => {
                        if let Some(m) = meetings
                            .get(usize::try_from(dice.roll(meetings.len().max(1) as u64)).unwrap())
                        {
                            let view =
                                crate::view::MeetingView::load_named(&da.svc.store(), *m).unwrap();
                            if let Some(view) = view {
                                let summary = SummaryV1 {
                                    title: "Invented meeting".into(),
                                    tldr: "An invented summary.".into(),
                                    ..SummaryV1::default()
                                };
                                let _ = crate::summary::store_summary(
                                    &da.svc,
                                    &view,
                                    "test-model".into(),
                                    &summary,
                                    &mut rand_chacha::ChaCha8Rng::seed_from_u64(turn),
                                );
                            }
                        }
                    }
                    // A meeting is deleted on the asker.
                    11 => {
                        if let Some(m) = meetings
                            .get(usize::try_from(dice.roll(meetings.len().max(1) as u64)).unwrap())
                        {
                            let _ = da.svc.delete_meeting(*m);
                        }
                    }
                    // Rarer things: the helper stops or starts offering, the asker's clock is
                    // set back, either process restarts.
                    _ => match dice.roll(5) {
                        0 => set_offer_summaries(&db.svc, false).unwrap(),
                        1 => set_offer_summaries(&db.svc, true).unwrap(),
                        2 => da
                            .clock
                            .set_ms(da.clock.now_unix_ms().saturating_sub(10 * 60_000)),
                        3 => a.offload = Offload::new(),
                        _ => {
                            b.offload = Offload::new();
                            written_for = b.host.for_others().len();
                        }
                    },
                }
                // Most turns end with a step on each device, the asker with the relay's
                // presence as it may be: unread, the helper online, stale, or missing.
                if dice.roll(4) != 0 {
                    let peers = match dice.roll(4) {
                        0 => None,
                        1 => Some(helper_online(db.id())),
                        2 => Some(vec![peer(
                            db.id(),
                            Some(presence(
                                "Invented desk",
                                true,
                                Some(WriterKind::Local),
                                false,
                            )),
                            STALE_AFTER_S + 1,
                        )]),
                        _ => Some(Vec::new()),
                    };
                    checked_step(&mut a, peers, &meetings, &mut tally, &at);
                }
                if dice.roll(4) != 0 {
                    checked_step(&mut b, None, &meetings, &mut tally, &at);
                }
            }
        }
        // Steps were skipped, scans ran, and scans did act: the run is not one where nothing
        // ever happens or nothing is ever skipped.
        assert!(tally.skipped > 60, "{tally:?}");
        assert!(tally.scanned > 150, "{tally:?}");
        assert!(tally.acted > 40, "{tally:?}");
    }

    // ---- Stop (T14.66, docs/summary-order.md 11.2) ----

    use crate::summary_order::run::{StopAnswer, stop_stands, stop_waiting};

    /// A summary of `m` stored here after `clock` moved on (as one arriving by sync would be).
    fn store_a_summary(dev: &Device, m: EntityId, title: &str) {
        dev.clock.advance_ms(1000);
        let view = crate::view::MeetingView::load(&**dev.svc.store(), m)
            .unwrap()
            .unwrap();
        crate::summary::store_summary(
            &dev.svc,
            &view,
            "other".into(),
            &SummaryV1 {
                title: title.into(),
                tldr: "Done.".into(),
                enhanced_notes: vec![],
                decisions: vec![],
                action_items: vec![],
                open_questions: vec![],
                topics: vec![],
            },
            &mut *crate::os_seeded_rng(),
        )
        .unwrap();
    }

    fn asked_of_desk(dev: &Device, m: EntityId) -> EntityId {
        offloaded(start(
            dev,
            m,
            RunStart::Order(order(&[OrderStep::Computer(DESK), ENDPOINT])),
            &Writers::default().asks(DESK),
        ))
    }

    #[test]
    fn offload_order_stop_withdraws_and_ends_the_run_in_one_transaction() {
        let a = Device::new("offload-stop-one");
        let m = meeting(&a.svc, "Invented planning", 601);
        let request = asked_of_desk(&a, m);
        // A fault between the withdrawal and the record leaves neither.
        crate::summary_order::run::fault::fail_stop();
        assert!(stop_waiting(&a.svc, m).is_err());
        assert_eq!(a.request(request).state, RequestState::Requested);
        assert_eq!(record(&a.svc, m).state, RunState::Waiting);
        // Stop.
        assert_eq!(
            stop_waiting(&a.svc, m).unwrap(),
            StopAnswer::Stopped { device: DESK }
        );
        assert_eq!(a.request(request).state, RequestState::Withdrawn);
        assert!(authored(&a.request(request)));
        let r = record(&a.svc, m);
        assert_eq!(
            (r.state, r.waiting_on, r.stopped, r.writer),
            (RunState::Ended, None, Some(request), None)
        );
        assert_eq!(r.outcome_of(0), Some(OutcomeWord::Stopped));
        assert_eq!(r.outcome_of(1), None, "no later step ran");
        assert!(stop_stands(&a.svc.store(), m, &r).unwrap());
        // Control: a meeting with nothing waiting.
        let other = meeting(&a.svc, "Invented retro", 602);
        assert_eq!(stop_waiting(&a.svc, other).unwrap(), StopAnswer::NotWaiting);
    }

    #[test]
    fn offload_order_no_continuation_after_a_stop() {
        let a = Device::new("offload-stop-no-continuation");
        let m = meeting(&a.svc, "Invented planning", 603);
        asked_of_desk(&a, m);
        stop_waiting(&a.svc, m).unwrap();
        let host = Recorder::default();
        hand_back(&a, &host);
        assert!(host.here().is_empty(), "the stopped run is not continued");
        // Nor after a restart.
        let restarted = Offload::new();
        let rh = Recorder::default();
        restarted.step(&a.svc, &mut || None, &rh).unwrap();
        assert!(rh.here().is_empty());
        // Control: a run withdrawn by the 90 s rule is continued.
        let other = meeting(&a.svc, "Invented retro", 604);
        asked_of_desk(&a, other);
        let host = Recorder::default();
        hand_back(&a, &host);
        assert_eq!(host.here().len(), 1);
        assert_eq!(host.here()[0].meeting, other);
    }

    #[test]
    fn offload_order_stop_on_a_legacy_request_writes_an_ended_record() {
        let a = Device::new("offload-stop-legacy");
        let m = meeting(&a.svc, "Invented planning", 605);
        let request = ask(&a.svc, m, DESK).unwrap().unwrap();
        assert_eq!(
            crate::summary_order::run::read(&a.svc.store(), m).unwrap(),
            RunRecord::Absent
        );
        assert_eq!(
            stop_waiting(&a.svc, m).unwrap(),
            StopAnswer::Stopped { device: DESK }
        );
        let r = record(&a.svc, m);
        assert_eq!(
            (r.origin, r.steps.clone(), r.state, r.stopped),
            (
                RunOrigin::Named,
                vec![OrderStep::Computer(DESK)],
                RunState::Ended,
                Some(request)
            )
        );
        assert_eq!(r.outcome_of(0), Some(OutcomeWord::Stopped));
        let host = Recorder::default();
        hand_back(&a, &host);
        assert!(host.here().is_empty(), "never written here");
        // Control: a legacy withdrawal without Stop is written here.
        let other = meeting(&a.svc, "Invented retro", 606);
        ask(&a.svc, other, DESK).unwrap().unwrap();
        let host = Recorder::default();
        hand_back(&a, &host);
        assert_eq!(host.here().len(), 1);
        assert_eq!(host.here()[0].continuation, Continuation::Legacy);
    }

    #[test]
    fn offload_order_stop_after_the_summary_arrived_is_already_written() {
        for done in [false, true] {
            let a = Device::new("offload-stop-written");
            let m = meeting(&a.svc, "Invented planning", 607);
            let request = asked_of_desk(&a, m);
            if done {
                // The summary row arrives before the request's Done (a different page).
                store_a_summary(&a, m, "From the desk");
            }
            let answer = stop_waiting(&a.svc, m).unwrap();
            if done {
                assert_eq!(answer, StopAnswer::AlreadyWritten { device: DESK });
                // Withdrawn too, so a third device's summary leaves the helper nothing to write.
                assert_eq!(a.request(request).state, RequestState::Withdrawn);
                let r = record(&a.svc, m);
                assert_eq!(
                    (r.state, r.writer, r.stopped),
                    (RunState::Ended, Some(0), None)
                );
            } else {
                // Control: no summary yet.
                assert_eq!(answer, StopAnswer::Stopped { device: DESK });
            }
        }
    }

    #[test]
    fn offload_order_a_summary_after_the_stop_is_kept_and_the_note_falls() {
        let a = Device::new("offload-stop-later-summary");
        let m = meeting(&a.svc, "Invented planning", 608);
        asked_of_desk(&a, m);
        stop_waiting(&a.svc, m).unwrap();
        let r = record(&a.svc, m);
        // Control: no summary, the note stands.
        assert!(stop_stands(&a.svc.store(), m, &r).unwrap());
        store_a_summary(&a, m, "Written anyway");
        assert_eq!(a.live_summaries(m), 1, "kept");
        assert!(!stop_stands(&a.svc.store(), m, &r).unwrap());
    }

    #[test]
    fn offload_order_stop_ends_a_run_withdrawn_but_not_yet_continued() {
        for stopped in [true, false] {
            let a = Device::new("offload-stop-case-four");
            let m = meeting(&a.svc, "Invented planning", 609);
            let request = asked_of_desk(&a, m);
            // Withdrawn by the 90 s rule and handed to the host, whose job has not begun.
            let host = Recorder::default();
            hand_back(&a, &host);
            assert_eq!(host.here().len(), 1);
            assert_eq!(record(&a.svc, m).state, RunState::Waiting);
            if stopped {
                assert_eq!(
                    stop_waiting(&a.svc, m).unwrap(),
                    StopAnswer::Stopped { device: DESK }
                );
                let r = record(&a.svc, m);
                assert_eq!((r.state, r.stopped), (RunState::Ended, Some(request)));
            }
            // The continuation the host started now runs (or finds nothing).
            let good = MockServer::start(vec![reply("Written by the endpoint")]).unwrap();
            let writers = Writers::default().mock(WriterStep::Endpoint, &good);
            let out = run_summary_order(
                &a.svc,
                m,
                RunStart::Continue {
                    request,
                    word: OutcomeWord::NoReply,
                },
                &plan(&writers),
                &CancelToken::new(),
            );
            if stopped {
                assert!(out.settled, "nothing run after a stop");
                assert!(writers.asked().is_empty());
            } else {
                // Control: without Stop the next step writes it.
                assert!(matches!(out.summary, SummaryResult::Stored { .. }));
            }
        }
    }

    #[test]
    fn offload_order_stop_and_a_new_run_in_either_order() {
        // Stop, then a new run: the new run replaces the ended record and finds nothing open.
        let a = Device::new("offload-stop-then-start");
        let m = meeting(&a.svc, "Invented planning", 610);
        let first = asked_of_desk(&a, m);
        stop_waiting(&a.svc, m).unwrap();
        let second = asked_of_desk(&a, m);
        assert_ne!(first, second);
        assert_eq!(a.request(first).state, RequestState::Withdrawn);
        assert_eq!(record(&a.svc, m).waiting_on, Some(second));
        // A new run, then Stop: Stop acts only on the new run's request.
        let b = Device::new("offload-start-then-stop");
        let m = meeting(&b.svc, "Invented planning", 611);
        let first = asked_of_desk(&b, m);
        let good = MockServer::start(vec![reply("Written by the endpoint")]).unwrap();
        let out = run_summary_order(
            &b.svc,
            m,
            RunStart::Order(order(&[ENDPOINT])),
            &plan(&Writers::default().mock(WriterStep::Endpoint, &good)),
            &CancelToken::new(),
        );
        assert!(matches!(out.summary, SummaryResult::Stored { .. }));
        assert_eq!(b.request(first).state, RequestState::Withdrawn);
        assert_eq!(stop_waiting(&b.svc, m).unwrap(), StopAnswer::NotWaiting);
        assert_eq!(
            record(&b.svc, m).writer,
            Some(0),
            "the new run is untouched"
        );
    }

    // ---- the meeting list's "written elsewhere" (T14.66, docs/summary-order.md 11.4) ----

    fn shown_set(dev: &Device) -> (std::collections::BTreeSet<EntityId>, usize) {
        let store = dev.svc.store();
        let read = store.guarded_read();
        meetings_waiting_shown(&read, store.device())
    }

    fn damage(dev: &Device, sql: &str, id: EntityId) {
        let c = rusqlite::Connection::open(dev.svc.data.db_path()).unwrap();
        let changed = c
            .execute(sql, rusqlite::params![id.as_uuid().as_bytes().to_vec()])
            .unwrap();
        assert_eq!(changed, 1);
    }

    #[test]
    fn offload_meetings_waiting_shown_is_waiting_shown_meeting_by_meeting() {
        let a = Device::new("offload-list-shown");
        // An empty store: nothing.
        assert!(shown_set(&a).0.is_empty());
        let requested = meeting(&a.svc, "Invented one", 701);
        asked_of_desk(&a, requested);
        let withdrawn = meeting(&a.svc, "Invented two", 702);
        let w = asked_of_desk(&a, withdrawn);
        super::withdraw(&a, w);
        let stopped = meeting(&a.svc, "Invented three", 703);
        asked_of_desk(&a, stopped);
        stop_waiting(&a.svc, stopped).unwrap();
        let summarised = meeting(&a.svc, "Invented four", 704);
        asked_of_desk(&a, summarised);
        store_a_summary(&a, summarised, "Arrived");
        // A request whose last write is not its asker's (a claim written here as if by the
        // target, which is not this device): ignored by both.
        let forged = meeting(&a.svc, "Invented five", 705);
        let f = asked_of_desk(&a, forged);
        let mut row = a.request(f);
        crate::edit::write(&a.svc, |tx, stamp| {
            row.state = RequestState::Claimed;
            row.meta.touch(stamp.hlc, stamp.clock);
            tx.put_summary_request(row.clone())?;
            Ok(())
        })
        .unwrap();
        let none = meeting(&a.svc, "Invented six", 706);
        let (set, damaged) = shown_set(&a);
        assert_eq!(damaged, 0);
        let me = a.id();
        for m in [requested, withdrawn, stopped, summarised, forged, none] {
            assert_eq!(
                set.contains(&m),
                waiting_shown(&a.svc.store(), me, m).unwrap().is_some(),
                "{m}"
            );
        }
        assert_eq!(set.into_iter().collect::<Vec<_>>(), vec![requested]);
    }

    #[test]
    fn offload_meetings_waiting_shown_skips_damaged_rows_and_counts_them() {
        for damaged in [true, false] {
            let a = Device::new("offload-list-damaged");
            let by_request = meeting(&a.svc, "Invented one", 711);
            let r = asked_of_desk(&a, by_request);
            let by_summary = meeting(&a.svc, "Invented two", 712);
            asked_of_desk(&a, by_summary);
            // An earlier summary, before the request (the meeting still waits).
            let earlier = meeting(&a.svc, "Invented three", 713);
            store_a_summary(&a, earlier, "Earlier");
            let s = a.svc.store().summaries(earlier).unwrap()[0].id;
            a.clock.advance_ms(1000);
            asked_of_desk(&a, earlier);
            if damaged {
                damage(
                    &a,
                    "UPDATE summary_request SET state = 'not-a-state' WHERE id = ?1",
                    r,
                );
                damage(
                    &a,
                    "UPDATE summary SET template = CAST(X'FF00FE' AS TEXT) WHERE id = ?1",
                    s,
                );
            }
            let (set, count) = shown_set(&a);
            if damaged {
                // Neither meeting with a damaged row is marked; both rows are counted.
                assert_eq!(count, 2);
                assert_eq!(set.into_iter().collect::<Vec<_>>(), vec![by_summary]);
            } else {
                // Control: undamaged, all three wait.
                assert_eq!(count, 0);
                assert_eq!(set.len(), 3);
            }
        }
    }
}

// ---- T14.66: a withdrawal stops the helper's write (docs/summary-order.md 11.3) ----

/// A host that writes for others as a real host does: one [`Writing`] entry per write, the write
/// on a thread of its own over `writer`, its outcome sent back.
struct WritingHost {
    svc: Services,
    queue: MeetingQueue,
    writing: Arc<Writing>,
    writer: Arc<dyn Fn(&Services) -> HelperWriter + Send + Sync>,
    started: std::sync::atomic::AtomicUsize,
    done: Mutex<std::sync::mpsc::Sender<HelperOutcome>>,
}

type WriterFn = Arc<dyn Fn(&Services) -> HelperWriter + Send + Sync>;

impl WritingHost {
    fn new(
        svc: &Services,
        writer: WriterFn,
    ) -> (Arc<Self>, std::sync::mpsc::Receiver<HelperOutcome>) {
        let (tx, rx) = std::sync::mpsc::channel();
        (
            Arc::new(Self {
                svc: svc.clone(),
                queue: MeetingQueue::new(),
                writing: Writing::new(),
                writer,
                started: std::sync::atomic::AtomicUsize::new(0),
                done: Mutex::new(tx),
            }),
            rx,
        )
    }

    fn started(&self) -> usize {
        self.started.load(std::sync::atomic::Ordering::SeqCst)
    }
}

impl OffloadHost for WritingHost {
    fn continue_run(&self, _job: WriteHere) -> ContinueAnswer {
        ContinueAnswer::Refused
    }
    fn write_for(&self, request: SummaryRequest) {
        let Some((token, entry)) = self.writing.begin(request.id) else {
            return;
        };
        self.started
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let (svc, queue, writer) = (
            self.svc.clone(),
            self.queue.clone(),
            Arc::clone(&self.writer),
        );
        let done = self.done.lock().unwrap().clone();
        std::thread::spawn(move || {
            let _entry = entry;
            let out = write_for_request(&svc, &queue, &request, &|_| writer(&svc), &token);
            let _ = done.send(out);
        });
    }
    fn writing(&self) -> Vec<EntityId> {
        self.writing.ids()
    }
    fn cancel_writing(&self, request: EntityId) {
        self.writing.cancel(request);
    }
}

/// The asker withdraws its open `request` (as Stop does), authored by the asker.
fn withdraw(a: &Device, request: EntityId) {
    let state = a.request(request).state;
    assert!(move_request(&a.svc, request, state, RequestState::Withdrawn, "").unwrap());
}

/// A request asked of `b`, claimed by it and synced back: (meeting, request).
fn claimed(a: &Device, b: &Device, ca: &SyncClient, cb: &SyncClient) -> (EntityId, EntityId) {
    let m = meeting(&a.svc, "Invented planning", 13);
    let request = ask(&a.svc, m, b.id()).unwrap().unwrap();
    sync(ca, cb);
    b.step(None, &Recorder::default());
    sync(cb, ca);
    assert_eq!(a.request(request).state, RequestState::Claimed);
    (m, request)
}

/// An endpoint that answers only once released (or after a 60 s guard), with a rendezvous when
/// a summary request arrives.
fn held_endpoint() -> (
    Arc<MockServer>,
    std::sync::mpsc::Receiver<()>,
    std::sync::mpsc::Sender<()>,
) {
    let (arrived_tx, arrived_rx) = std::sync::mpsc::channel::<()>();
    let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
    let arrived_tx = Mutex::new(arrived_tx);
    let release_rx = Mutex::new(release_rx);
    let server = Arc::new(
        MockServer::start_with(move |_| {
            let _ = arrived_tx.lock().unwrap().send(());
            let _ = release_rx
                .lock()
                .unwrap()
                .recv_timeout(std::time::Duration::from_secs(60));
            reply("Written by the helper")
        })
        .unwrap(),
    );
    (server, arrived_rx, release_tx)
}

const GUARD: std::time::Duration = std::time::Duration::from_secs(60);

#[test]
fn offload_a_pulled_withdrawal_cancels_the_helpers_write_and_only_its_asker_can() {
    let relay = relay();
    let (a, b) = pair(&relay);
    let (ca, cb) = (a.client(), b.client());
    let (_, request) = claimed(&a, &b, &ca, &cb);
    let host = Recorder::default();
    *host.writing.lock().unwrap() = vec![request];

    // Control: still claimed, nothing to cancel.
    b.step(None, &host);
    assert!(host.cancelled.lock().unwrap().is_empty());

    // Control: a withdrawal written by a device that is not the asker (the helper itself here)
    // is ignored.
    let mut forged = b.request(request);
    crate::edit::write(&b.svc, |tx, stamp| {
        forged.state = RequestState::Withdrawn;
        forged.meta.touch(stamp.hlc, stamp.clock);
        tx.put_summary_request(forged.clone())?;
        Ok(())
    })
    .unwrap();
    b.step(None, &host);
    assert!(host.cancelled.lock().unwrap().is_empty());

    // The asker's withdrawal, pulled, cancels the write.
    withdraw(&a, request);
    sync(&ca, &cb);
    assert_eq!(b.request(request).state, RequestState::Withdrawn);
    b.step(None, &host);
    assert_eq!(*host.cancelled.lock().unwrap(), vec![request]);
}

#[test]
fn offload_no_store_lock_is_held_while_a_write_is_cancelled() {
    struct Probe {
        svc: Services,
        request: EntityId,
        free: Mutex<Option<bool>>,
    }
    impl OffloadHost for Probe {
        fn continue_run(&self, _job: WriteHere) -> ContinueAnswer {
            ContinueAnswer::Refused
        }
        fn write_for(&self, _request: SummaryRequest) {}
        fn writing(&self) -> Vec<EntityId> {
            vec![self.request]
        }
        fn cancel_writing(&self, _request: EntityId) {
            // The store, taken from another thread, within a bounded wait.
            let (tx, rx) = std::sync::mpsc::channel();
            let svc = self.svc.clone();
            std::thread::spawn(move || {
                let _store = svc.store();
                let _ = tx.send(());
            });
            let free = rx.recv_timeout(std::time::Duration::from_secs(10)).is_ok();
            *self.free.lock().unwrap() = Some(free);
        }
    }
    let relay = relay();
    let (a, b) = pair(&relay);
    let (ca, cb) = (a.client(), b.client());
    let (_, request) = claimed(&a, &b, &ca, &cb);
    withdraw(&a, request);
    sync(&ca, &cb);
    let probe = Probe {
        svc: b.svc.clone(),
        request,
        free: Mutex::new(None),
    };
    b.offload.step(&b.svc, &mut || None, &probe).unwrap();
    assert_eq!(*probe.free.lock().unwrap(), Some(true));
}

#[test]
fn offload_a_withdrawal_reaching_a_running_write_cancels_it_and_nothing_is_stored() {
    for withdrawn in [true, false] {
        let relay = relay();
        let (a, b) = pair(&relay);
        let (ca, cb) = (a.client(), b.client());
        let m = meeting(&a.svc, "Invented roadmap", 15);
        let request = ask(&a.svc, m, b.id()).unwrap().unwrap();
        sync(&ca, &cb);
        let (server, arrived, release) = held_endpoint();
        let s = Arc::clone(&server);
        let (host, outcomes) =
            WritingHost::new(&b.svc, Arc::new(move |svc| remote_writer(svc, &s)));
        let helper = Offload::new();
        helper.step(&b.svc, &mut || None, &*host).unwrap();
        assert_eq!(host.writing.ids(), vec![request]);
        // The rendezvous: the endpoint has the summary request and holds its answer.
        arrived.recv_timeout(GUARD).expect("the endpoint is asked");
        if withdrawn {
            withdraw(&a, request);
            ca.tick().unwrap();
            cb.tick().unwrap();
            helper.step(&b.svc, &mut || None, &*host).unwrap();
            let out = outcomes
                .recv_timeout(GUARD)
                .expect("the write ends without the endpoint answering");
            assert_eq!(out, HelperOutcome::Cancelled);
            let _ = release.send(());
            assert_eq!(b.live_summaries(m), 0, "nothing stored");
            assert_eq!(
                b.request(request).state,
                RequestState::Withdrawn,
                "no decline"
            );
        } else {
            // Control: the write has not ended while the endpoint holds its answer, and ends
            // Done once released.
            assert!(host.writing.ids() == vec![request]);
            let _ = release.send(());
            let out = outcomes.recv_timeout(GUARD).unwrap();
            assert!(matches!(out, HelperOutcome::Done { .. }), "{out:?}");
        }
        // The entry goes when the write ends (bounded wait).
        let until = std::time::Instant::now() + GUARD;
        while !host.writing.ids().is_empty() && std::time::Instant::now() < until {
            std::thread::yield_now();
        }
        assert!(host.writing.ids().is_empty());
    }
}

#[test]
fn offload_a_new_sync_threads_second_hand_starts_no_second_write_and_still_cancels_it() {
    let relay = relay();
    let (a, b) = pair(&relay);
    let (ca, cb) = (a.client(), b.client());
    let m = meeting(&a.svc, "Invented hiring", 16);
    let request = ask(&a.svc, m, b.id()).unwrap().unwrap();
    sync(&ca, &cb);
    let (server, arrived, release) = held_endpoint();
    let s = Arc::clone(&server);
    let (host, outcomes) = WritingHost::new(&b.svc, Arc::new(move |svc| remote_writer(svc, &s)));
    // The first thread's step starts the write.
    Offload::new().step(&b.svc, &mut || None, &*host).unwrap();
    arrived.recv_timeout(GUARD).unwrap();
    assert_eq!(host.started(), 1);
    // A replaced thread's new Offload hands the same request again: nothing new starts.
    let second = Offload::new();
    second.step(&b.svc, &mut || None, &*host).unwrap();
    assert_eq!(host.started(), 1);
    assert_eq!(host.writing.ids(), vec![request]);
    // The withdrawal, heard by the new thread, cancels the running write.
    withdraw(&a, request);
    sync(&ca, &cb);
    second.step(&b.svc, &mut || None, &*host).unwrap();
    let out = outcomes.recv_timeout(GUARD).unwrap();
    assert_eq!(out, HelperOutcome::Cancelled);
    assert_eq!(b.live_summaries(m), 0);
    let _ = release.send(());
    // Control: another request handed by the new thread starts its own write.
    let other_m = meeting(&a.svc, "Invented offsite", 17);
    let other = ask(&a.svc, other_m, b.id()).unwrap().unwrap();
    sync(&ca, &cb);
    second.step(&b.svc, &mut || None, &*host).unwrap();
    assert_eq!(host.started(), 2);
    arrived.recv_timeout(GUARD).unwrap();
    let _ = release.send(());
    let out = outcomes.recv_timeout(GUARD).unwrap();
    assert!(matches!(out, HelperOutcome::Done { .. }), "{out:?}");
    assert_eq!(b.request(other).state, RequestState::Done);
}

#[test]
fn offload_a_restarted_helper_never_writes_a_request_withdrawn_before_its_first_step() {
    // (a) The withdrawal is pulled before the first step: never handed, nothing written.
    let relay = relay();
    let (a, b) = pair(&relay);
    let (ca, cb) = (a.client(), b.client());
    let (m, request) = claimed(&a, &b, &ca, &cb);
    withdraw(&a, request);
    sync(&ca, &cb);
    let host = Recorder::default();
    let restarted = Offload::new();
    restarted.step(&b.svc, &mut || None, &host).unwrap();
    assert!(host.for_others().is_empty());
    assert!(host.cancelled.lock().unwrap().is_empty());
    assert_eq!(b.live_summaries(m), 0);
}

#[test]
fn offload_a_restarted_helper_whose_first_tick_failed_is_cancelled_at_the_next_one() {
    for withdrawn in [true, false] {
        let relay = relay();
        let (a, b) = pair(&relay);
        let (ca, cb) = (a.client(), b.client());
        let (m, request) = claimed(&a, &b, &ca, &cb);
        let (server, arrived, release) = held_endpoint();
        let s = Arc::clone(&server);
        let (host, outcomes) =
            WritingHost::new(&b.svc, Arc::new(move |svc| remote_writer(svc, &s)));
        // (b) A fresh process: its first tick failed (no pull), the step runs anyway and hands
        // the request it holds claimed.
        let restarted = Offload::new();
        restarted.step(&b.svc, &mut || None, &*host).unwrap();
        assert_eq!(host.writing.ids(), vec![request]);
        arrived.recv_timeout(GUARD).unwrap();
        if withdrawn {
            withdraw(&a, request);
            ca.tick().unwrap();
            cb.tick().unwrap();
            restarted.step(&b.svc, &mut || None, &*host).unwrap();
            let out = outcomes.recv_timeout(GUARD).unwrap();
            assert_eq!(out, HelperOutcome::Cancelled);
            let _ = release.send(());
            assert_eq!(b.live_summaries(m), 0);
        } else {
            // Control: with no withdrawal the write runs to Done.
            let _ = release.send(());
            let out = outcomes.recv_timeout(GUARD).unwrap();
            assert!(matches!(out, HelperOutcome::Done { .. }), "{out:?}");
        }
    }
}

#[test]
fn offload_writing_starts_each_request_once_and_stopping_refuses_new_writes() {
    let w = Writing::new();
    let (r1, r2) = (EntityId::from_bytes([1; 16]), EntityId::from_bytes([2; 16]));
    let (t1, e1) = w.begin(r1).unwrap();
    assert!(w.begin(r1).is_none(), "a second hand starts nothing");
    let (t2, e2) = w.begin(r2).unwrap();
    // A withdrawal cancels only its own write.
    w.cancel(r1);
    assert!(t1.is_cancelled());
    assert!(!t2.is_cancelled());
    // The entry goes with its write.
    drop(e1);
    assert_eq!(w.ids(), vec![r2]);
    // Stopping cancels the rest and starts nothing new.
    w.cancel_all();
    assert!(t2.is_cancelled());
    assert!(w.begin(r1).is_none());
    drop(e2);
    assert!(w.ids().is_empty());
}

#[test]
fn offload_writings_lock_is_a_leaf_taken_with_the_store_locked_elsewhere() {
    // Every Writing call completes while this thread holds the store: none of them waits for the
    // store (or anything else) under its own lock.
    let dir = TempDir::new("offload-writing-leaf");
    let (svc, _) = services(&dir);
    let store = svc.store();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let w = Writing::new();
        let r = EntityId::from_bytes([3; 16]);
        let entry = w.begin(r);
        w.cancel(r);
        let _ = w.ids();
        drop(entry);
        w.cancel_all();
        let _ = tx.send(());
    });
    assert!(rx.recv_timeout(std::time::Duration::from_secs(10)).is_ok());
    drop(store);
}
