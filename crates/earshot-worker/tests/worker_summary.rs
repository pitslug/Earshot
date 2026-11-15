//! T11.6 accept: the worker writes a summary for a simulated device's request. The real relay
//! runs in process (`LocalRelay`), the summary endpoint is the loopback mock
//! (`earshot-llm-replay`), and the worker runs its production code path: `join` with a pairing
//! code into its vault files, `Worker::open_with`, its sync thread with summary offload and its
//! host's writer. Only where the endpoint is (`Locate`) is the test's own, since the mock is on
//! loopback, which the own-network rule never accepts.
//!
//! Every name holds `worker`, so `cargo test -p earshot-worker worker` selects them.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use earshot_core::asr::{MeetingTime, Segment, Word};
use earshot_core::capture::TrackId;
use earshot_core::diar::SpeakerRole;
use earshot_core::identity::{DeviceId, EntityId};
use earshot_core::model::{
    ClockDomain, Meeting, MeetingRepo, RequestState, SegmentRow, Speaker, Stamp,
    SummaryRequestRepo, Track, TranscriptPass,
};
use earshot_core::prompt::Template;
use earshot_core::summary::SummaryV1;
use earshot_llm_replay::mock::{MockResponse, MockServer};
use earshot_net::Url;
use earshot_runtime::sync::{
    KIND_SERVER, MemoryVault, SyncClient, SyncProblem, SyncTiming, SyncVault, offer_pairing_code,
    set_sync_voiceprints, set_up_first_device,
};
use earshot_runtime::{Services, SharedClock, SystemClock};
use earshot_store::{DataDir, Store};
use earshot_sync_server::{Config as RelayConfig, LocalRelay, Relay as RelayServer};
use earshot_worker::config::Config;
use earshot_worker::endpoint::Place;
use earshot_worker::worker::{Worker, WorkerError, join, open_services};
use rand_chacha::ChaCha8Rng;
use rand_core::SeedableRng;

const SECRET: &str = "an-invented-setup-secret-for-worker-tests";
/// A fixed meeting start (any time works: the clocks are the system's).
const T0: u64 = 1_792_294_800_000;

struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "earshot-worker-{tag}-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::SeqCst)
        ));
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

fn relay(dir: &TempDir) -> LocalRelay {
    let server = RelayServer::open(&RelayConfig {
        setup_secret: SECRET.into(),
        data: dir.0.join("relay"),
        listen: "127.0.0.1:0".parse().unwrap(),
    })
    .unwrap();
    LocalRelay::start(server).unwrap()
}

/// The simulated device that asks: its own store and an in-memory vault.
struct Asker {
    svc: Services,
    vault: Arc<MemoryVault>,
}

impl Asker {
    fn new(dir: &Path) -> Self {
        let data = DataDir::new(dir.join("asker"));
        data.create().unwrap();
        let store = Store::open(&data.db_path()).unwrap();
        let clock: SharedClock = Arc::new(SystemClock);
        Self {
            svc: Services::new(store, data, clock).unwrap(),
            vault: Arc::new(MemoryVault::default()),
        }
    }

    fn client(&self) -> SyncClient {
        SyncClient::new(
            self.svc.clone(),
            Arc::clone(&self.vault) as Arc<dyn SyncVault>,
        )
        .unwrap()
    }
}

/// A finished two-track meeting with a final transcript (invented words and names).
fn meeting(svc: &Services, title: &str, kept_local: bool, seed: u64) -> EntityId {
    let clock = Arc::clone(&svc.clock);
    let mut rng = ChaCha8Rng::seed_from_u64(seed);
    let mut db = svc.store();
    let (store, hlc) = db.parts();
    let mut stamp = Stamp {
        clock: &*clock,
        hlc,
        rng: &mut rng,
    };
    let mut m = Meeting::new(title, T0, 600, Template::General, &mut stamp);
    m.duration_ms = Some(60_000);
    m.summary_kept_local = kept_local;
    let mic = Track::new(m.id, TrackId::Mic, None, ClockDomain::Replay, &mut stamp);
    let them = Track::new(
        m.id,
        TrackId::Reference,
        None,
        ClockDomain::Replay,
        &mut stamp,
    );
    let me = Speaker::new(m.id, SpeakerRole::Me, "Me", None, &mut stamp);
    let other = Speaker::new(m.id, SpeakerRole::Remote, "Zephyrine", None, &mut stamp);
    let mut segments = Vec::new();
    for (track, speaker, text, start) in [
        (&mic, &me, "shall we move the launch to thursday", 1_000u64),
        (
            &them,
            &other,
            "thursday works if the checklist is done",
            5_000,
        ),
    ] {
        let words: Vec<Word> = text
            .split(' ')
            .enumerate()
            .map(|(i, w)| Word::new(w, start + 400 * i as u64, start + 400 * i as u64 + 300, 0.9))
            .collect();
        let mut s = Segment::from_words(
            EntityId::generate(stamp.clock, &mut *stamp.rng),
            track.id,
            words,
            MeetingTime::from_millis(start),
        );
        s.speaker_id = Some(speaker.id);
        segments.push(s);
    }
    store.put_meeting(m.clone()).unwrap();
    store.put_track(mic).unwrap();
    store.put_track(them).unwrap();
    store.put_speaker(me).unwrap();
    store.put_speaker(other).unwrap();
    for s in segments {
        let row = SegmentRow::new(m.id, TranscriptPass::Final, s, stamp.hlc, stamp.clock);
        store.put_segment(row).unwrap();
    }
    m.id
}

fn reply(title: &str) -> MockResponse {
    let summary = SummaryV1 {
        title: title.into(),
        tldr: "The launch moves to Thursday once the checklist is done.".into(),
        enhanced_notes: vec![],
        decisions: vec![],
        action_items: vec![],
        open_questions: vec![],
        topics: vec!["launch".into()],
    };
    MockResponse::completion(&serde_json::to_string(&summary).unwrap(), "stop")
}

fn worker_config(dir: &Path, relay: &str, llm: &Url) -> Config {
    let data = dir.join("worker");
    Config {
        key_file: data.join("vault").join("key"),
        token_file: data.join("vault").join("token"),
        data,
        relay: Some(relay.to_owned()),
        llm_url: llm.clone(),
        llm_model: "test-model".into(),
        llm_context: 16_384,
        llm_timeout: Duration::from_secs(60),
        name: "Invented server".into(),
    }
}

fn fast() -> SyncTiming {
    SyncTiming {
        tick: Duration::from_millis(100),
        poll: Duration::from_millis(20),
        presence: Duration::from_millis(100),
    }
}

fn wait_until(what: &str, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(90);
    while !done() {
        assert!(Instant::now() < deadline, "timed out: {what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// The asker's view of `request` after a tick.
fn state_after_tick(asker: &Asker, client: &SyncClient, request: EntityId) -> RequestState {
    let _ = client.tick();
    asker
        .svc
        .store()
        .summary_request(request)
        .unwrap()
        .map_or(RequestState::Requested, |r| r.state)
}

fn live_summaries(svc: &Services, meeting: EntityId) -> Vec<SummaryV1> {
    svc.store()
        .summaries(meeting)
        .unwrap()
        .into_iter()
        .filter(|s| !s.meta.deleted)
        .map(|s| s.summary)
        .collect()
}

fn files_under(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).unwrap().flatten() {
        let path = entry.path();
        if path.is_dir() {
            files_under(&path, out);
        } else {
            out.push(path);
        }
    }
}

#[test]
fn worker_writes_the_summary_a_simulated_device_asks_for() {
    let dir = TempDir::new("summary");
    let relay = relay(&dir);
    let asker = Asker::new(&dir.0);
    set_up_first_device(&asker.svc, &*asker.vault, &relay.url(), SECRET).unwrap();
    let server = MockServer::start(vec![
        reply("Launch moved to Thursday"),
        reply("Kept here, written by the worker's own model"),
    ])
    .unwrap();
    let endpoint = server.endpoint();
    let config = worker_config(&dir.0, &relay.url(), &endpoint);

    // Join with a pairing code, as `earshot-worker join` does.
    let wsvc = open_services(&config.data).unwrap();
    let offer =
        offer_pairing_code(&asker.svc, Arc::clone(&asker.vault) as Arc<dyn SyncVault>).unwrap();
    join(&wsvc, &config, offer.code()).unwrap();
    drop(offer);
    assert!(config.key_file.is_file() && config.token_file.is_file());

    // The endpoint is remote until the test says it is the worker's own model.
    let own = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&own);
    let worker = Worker::open_with(
        wsvc.clone(),
        config.clone(),
        Arc::new(move |url: &Url| {
            if flag.load(Ordering::SeqCst) {
                Place::OwnNetwork { url: url.clone() }
            } else {
                Place::Remote { url: url.clone() }
            }
        }),
    )
    .unwrap();
    let worker_id: DeviceId = wsvc.store().device();
    let thread = worker.start(fast()).unwrap();

    // The asker sees it as a server that writes summaries, by its name, online.
    let ca = asker.client();
    wait_until("the worker's presence", || {
        ca.peers().unwrap().iter().any(|p| {
            p.device == worker_id
                && p.online
                && p.presence.as_ref().is_some_and(|x| x.offers_summaries)
        })
    });
    let peers = ca.peers().unwrap();
    let p = peers
        .iter()
        .find(|p| p.device == worker_id)
        .and_then(|p| p.presence.clone())
        .unwrap();
    assert_eq!(p.kind, KIND_SERVER);
    assert_eq!(p.name, "Invented server");
    assert_eq!(p.writer_kind.as_deref(), Some("openai_compatible"));
    assert!(!p.recording);

    // A meeting asked of the worker is written by it and comes back as an ordinary summary.
    let m = meeting(&asker.svc, "", false, 1);
    ca.tick().unwrap();
    let request = earshot_runtime::offload::ask(&asker.svc, m, worker_id)
        .unwrap()
        .unwrap();
    wait_until("the worker's summary", || {
        state_after_tick(&asker, &ca, request) == RequestState::Done
            && !live_summaries(&asker.svc, m).is_empty()
    });
    let got = live_summaries(&asker.svc, m);
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].title, "Launch moved to Thursday");
    assert_eq!(
        asker.svc.store().meeting(m).unwrap().unwrap().title,
        "Launch moved to Thursday"
    );
    assert_eq!(
        live_summaries(&wsvc, m).len(),
        1,
        "stored by the worker first"
    );
    // Only the transcript text went to the endpoint.
    let sent = server.requests();
    assert_eq!(sent.len(), 1);
    let body = String::from_utf8(sent[0].body.clone()).unwrap();
    assert!(body.contains("thursday works if the checklist is done"));
    assert!(body.contains("Zephyrine"));

    // A kept-local meeting and a remote endpoint: declined before anything is sent.
    let kept = meeting(&asker.svc, "", true, 2);
    ca.tick().unwrap();
    let declined = earshot_runtime::offload::ask(&asker.svc, kept, worker_id)
        .unwrap()
        .unwrap();
    wait_until("the decline", || {
        state_after_tick(&asker, &ca, declined) == RequestState::Declined
    });
    let row = asker
        .svc
        .store()
        .summary_request(declined)
        .unwrap()
        .unwrap();
    assert_eq!(row.reason, "kept_local_no_model");
    assert_eq!(server.requests().len(), 1, "nothing sent for it");
    assert!(live_summaries(&asker.svc, kept).is_empty());

    // The same meeting asked again once the endpoint is the worker's own model: written.
    own.store(true, Ordering::SeqCst);
    let again = earshot_runtime::offload::ask(&asker.svc, kept, worker_id)
        .unwrap()
        .unwrap();
    assert_ne!(again, declined);
    wait_until("the kept-local summary", || {
        state_after_tick(&asker, &ca, again) == RequestState::Done
            && !live_summaries(&asker.svc, kept).is_empty()
    });
    assert_eq!(
        live_summaries(&asker.svc, kept)[0].title,
        "Kept here, written by the worker's own model"
    );
    assert_eq!(server.requests().len(), 2);

    // It says gone when it stops; it holds no audio and voiceprint sync stayed off.
    if let Some(done) = thread.say_gone() {
        let _ = done.recv_timeout(Duration::from_secs(5));
    }
    thread.stop();
    wait_until("gone", || {
        ca.peers()
            .unwrap()
            .iter()
            .any(|p| p.device == worker_id && !p.online)
    });
    assert!(
        !earshot_runtime::sync::sync_settings(&wsvc.store())
            .unwrap()
            .voiceprints
    );
    let mut files = Vec::new();
    files_under(&config.data, &mut files);
    for f in &files {
        let ext = f.extension().and_then(|e| e.to_str()).unwrap_or("");
        assert!(!matches!(ext, "flac" | "wav" | "part"), "{}", f.display());
    }
}

#[test]
fn worker_refuses_to_run_with_voiceprint_sync_on_or_unjoined() {
    let dir = TempDir::new("refuse");
    let relay = relay(&dir);
    let endpoint = Url::parse("http://llama:8080/v1").unwrap();
    let config = worker_config(&dir.0, &relay.url(), &endpoint);
    let wsvc = open_services(&config.data).unwrap();
    let remote = |url: &Url| Place::Remote { url: url.clone() };
    assert_eq!(
        Worker::open_with(wsvc.clone(), config.clone(), Arc::new(remote)).unwrap_err(),
        WorkerError::NotJoined
    );
    // Without a relay address it cannot run at all.
    let no_relay = Config {
        relay: None,
        ..config.clone()
    };
    assert!(matches!(
        Worker::open_with(wsvc.clone(), no_relay, Arc::new(remote)),
        Err(WorkerError::Config(_))
    ));
    // Voiceprint sync on (set by hand, since the worker has no way to): refused to join and to
    // run, before any request.
    set_sync_voiceprints(&wsvc, true).unwrap();
    assert_eq!(
        join(&wsvc, &config, "ABCD-EFGH-JKLM").unwrap_err(),
        WorkerError::VoiceprintsOn
    );
    assert!(!config.key_file.exists());
    assert_eq!(
        Worker::open_with(wsvc, config, Arc::new(remote)).unwrap_err(),
        WorkerError::VoiceprintsOn
    );
}

#[test]
fn worker_join_with_a_wrong_code_stores_nothing() {
    let dir = TempDir::new("wrong-code");
    let relay = relay(&dir);
    let asker = Asker::new(&dir.0);
    set_up_first_device(&asker.svc, &*asker.vault, &relay.url(), SECRET).unwrap();
    let endpoint = Url::parse("http://llama:8080/v1").unwrap();
    let config = worker_config(&dir.0, &relay.url(), &endpoint);
    let wsvc = open_services(&config.data).unwrap();
    let e = join(&wsvc, &config, "not a code").unwrap_err();
    assert_eq!(e, WorkerError::Sync(SyncProblem::BadCode));
    assert!(!e.to_string().contains("not a code"));
    assert!(!config.key_file.exists() && !config.token_file.exists());
}
