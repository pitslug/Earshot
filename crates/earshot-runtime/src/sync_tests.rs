//! T11.3: the sync client against the real relay, in process (`LocalRelay`).
//!
//! Every name holds `sync`, so `cargo test -p earshot-runtime sync` selects them.

use std::sync::Arc;
use std::time::{Duration, Instant};

use earshot_core::identity::ManualClock;
use earshot_core::model::MeetingRepo;
use earshot_core::voiceprint::VoiceprintRepo;
use earshot_net::AuditOutcome;
use earshot_sync_server::{Config, LocalRelay, Relay as RelayServer};

use super::*;
use crate::local::DeviceRecord;
use crate::services::{MeetingJob, Services};
use crate::testutil::{TempDir, meeting, services};

const SECRET: &str = "an-invented-setup-secret-for-tests";

/// A relay with its own data folder, serving on loopback.
struct TestRelay {
    relay: LocalRelay,
    _dir: TempDir,
}

fn relay_config(dir: &TempDir) -> Config {
    Config {
        setup_secret: SECRET.into(),
        data: dir.0.join("relay"),
        listen: "127.0.0.1:0".parse().unwrap(),
    }
}

fn relay() -> TestRelay {
    let dir = TempDir::new("sync-relay");
    let server = RelayServer::open(&relay_config(&dir)).unwrap();
    TestRelay {
        relay: LocalRelay::start(server).unwrap(),
        _dir: dir,
    }
}

/// Waits up to a minute for `done`, failing with `what`.
fn wait_for(what: &str, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(60);
    while !done() {
        assert!(Instant::now() < deadline, "{what}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// One simulated device: its own store, clock and vault.
struct Device {
    svc: Services,
    clock: Arc<ManualClock>,
    vault: Arc<MemoryVault>,
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
            _dir: dir,
        }
    }

    fn vault(&self) -> Arc<dyn SyncVault> {
        Arc::clone(&self.vault) as Arc<dyn SyncVault>
    }

    fn client(&self) -> SyncClient {
        SyncClient::new(self.svc.clone(), self.vault()).unwrap()
    }

    /// Every synced row this device holds, in a fixed order, as text.
    fn rows(&self) -> Vec<String> {
        let db = self.svc.store();
        let mut out = Vec::new();
        let mut meetings = db.meetings().unwrap();
        meetings.sort_by_key(|m| m.id);
        for m in meetings {
            out.push(format!("{m:?}"));
            let mut parts: Vec<String> = Vec::new();
            parts.extend(db.tracks(m.id).unwrap().iter().map(|r| format!("{r:?}")));
            parts.extend(db.speakers(m.id).unwrap().iter().map(|r| format!("{r:?}")));
            parts.extend(db.segments(m.id).unwrap().iter().map(|r| format!("{r:?}")));
            parts.extend(db.notes(m.id).unwrap().iter().map(|r| format!("{r:?}")));
            parts.extend(db.summaries(m.id).unwrap().iter().map(|r| format!("{r:?}")));
            parts.sort();
            out.extend(parts);
        }
        let mut people: Vec<String> = db
            .persons()
            .unwrap()
            .iter()
            .map(|p| format!("{p:?}"))
            .collect();
        people.sort();
        out.extend(people);
        out
    }

    fn rename(&self, id: earshot_core::identity::EntityId, title: &str) {
        let clock = Arc::clone(&self.svc.clock);
        let mut db = self.svc.store();
        let (store, hlc) = db.parts();
        let mut m = store.meeting(id).unwrap().unwrap();
        m.title = title.into();
        m.meta.touch(hlc, &*clock);
        store.put_meeting(m).unwrap();
    }

    fn title(&self, id: earshot_core::identity::EntityId) -> Option<String> {
        self.svc
            .store()
            .meeting(id)
            .unwrap()
            .filter(|m| !m.meta.deleted)
            .map(|m| m.title)
    }
}

/// A first device and a second joined with a pairing code.
fn pair(relay: &TestRelay) -> (Device, Device) {
    let a = Device::new("sync-a");
    let b = Device::new("sync-b");
    set_up_first_device(&a.svc, &*a.vault, &relay.relay.url(), SECRET).unwrap();
    let offer = offer_pairing_code(&a.svc, a.vault()).unwrap();
    join_with_code(&b.svc, &*b.vault, &relay.relay.url(), offer.code()).unwrap();
    (a, b)
}

fn assert_only_the_relay(client: &SyncClient, relay: &TestRelay) {
    let entries = client.audit_log().entries();
    assert!(!entries.is_empty());
    let addr = relay.relay.addr();
    for e in entries {
        assert_eq!(
            (e.scheme.as_str(), e.host.as_str(), e.port),
            ("http", "127.0.0.1", addr.port()),
            "a request went somewhere other than the relay: {e:?}"
        );
        assert!(e.path.starts_with("/v1/"), "{e:?}");
        assert!(matches!(e.outcome, AuditOutcome::Completed), "{e:?}");
    }
}

#[test]
fn sync_two_devices_converge_after_concurrent_edits() {
    let relay = relay();
    let (a, b) = pair(&relay);
    let (ca, cb) = (a.client(), b.client());

    // Each device records a meeting of its own.
    let shared = meeting(&a.svc, "Planning", 1);
    let own_b = meeting(&b.svc, "Review", 2);
    ca.tick().unwrap();
    cb.tick().unwrap();
    ca.tick().unwrap();
    assert_eq!(a.rows(), b.rows(), "both devices hold both meetings");
    assert_eq!(b.title(shared).as_deref(), Some("Planning"));
    assert_eq!(a.title(own_b).as_deref(), Some("Review"));

    // Both rename the same meeting before either syncs; the later edit wins on both.
    a.clock.advance_ms(1_000);
    a.rename(shared, "Planning, edited here");
    b.clock.advance_ms(2_000);
    b.rename(shared, "Planning, edited there");
    ca.tick().unwrap();
    cb.tick().unwrap();
    ca.tick().unwrap();
    assert_eq!(a.title(shared).as_deref(), Some("Planning, edited there"));
    assert_eq!(a.rows(), b.rows());

    // Nothing is left to send, and nothing came back as a change of its own.
    assert_eq!(a.svc.store().outbox_len().unwrap(), 0);
    assert_eq!(b.svc.store().outbox_len().unwrap(), 0);
    let quiet = ca.tick().unwrap();
    assert_eq!((quiet.pushed, quiet.written), (0, 0), "{quiet:?}");

    assert_only_the_relay(&ca, &relay);
    assert_only_the_relay(&cb, &relay);
    let status = sync_status(&a.svc.store()).unwrap();
    assert_eq!(status.problem, None);
    assert!(status.last_success_unix_ms.is_some());
}

#[test]
fn sync_a_name_the_summary_used_changed_elsewhere_marks_the_summary_here() {
    use rand_core::SeedableRng;
    let relay = relay();
    let (a, b) = pair(&relay);
    let (ca, cb) = (a.client(), b.client());
    let named = |dev: &Device, m| {
        crate::view::MeetingView::load_named(&dev.svc.store(), m)
            .unwrap()
            .unwrap()
    };
    let used = meeting(&a.svc, "Planning", 1);
    let unused = meeting(&a.svc, "Review", 2);
    for (m, tldr, seed) in [
        (used, "Alice will send the plan.", 3),
        (unused, "The plan was agreed.", 4),
    ] {
        let v = named(&a, m);
        let mut s = summary_of("Planning");
        s.tldr = tldr.into();
        crate::summary::store_summary(
            &a.svc,
            &v,
            "test-model".into(),
            &s,
            &mut rand_chacha::ChaCha8Rng::seed_from_u64(seed),
        )
        .unwrap();
    }
    ca.tick().unwrap();
    cb.tick().unwrap();
    assert!(!named(&b, used).summary_out_of_date());

    // The name changes on the first computer; the second applies the row and judges it by
    // the same rule against the name its own copy had.
    for m in [used, unused] {
        let alice = named(&a, m)
            .speakers
            .into_iter()
            .find(|s| s.display_name == "Alice")
            .unwrap();
        a.clock.advance_ms(1_000);
        crate::edit::rename_speaker(&a.svc, m, alice.id, "Alicia").unwrap();
    }
    assert!(named(&a, used).summary_names_changed());
    ca.tick().unwrap();
    cb.tick().unwrap();
    let here = named(&b, used);
    assert_eq!(
        here.summary.as_ref().map(|s| s.id),
        named(&a, used).summary.as_ref().map(|s| s.id)
    );
    assert!(
        here.summary_names_changed(),
        "the name the summary used changed"
    );
    assert!(!named(&b, unused).summary_out_of_date());
    // Applying the same rows again marks nothing new and changes nothing.
    let quiet = cb.tick().unwrap();
    assert_eq!(quiet.written, 0, "{quiet:?}");
    assert!(named(&b, used).summary_names_changed());
}

#[test]
fn sync_a_rename_and_a_new_summary_in_one_page_leave_the_new_summary_current() {
    use rand_core::SeedableRng;
    let relay = relay();
    let (a, b) = pair(&relay);
    let (ca, cb) = (a.client(), b.client());
    let named = |dev: &Device, m| {
        crate::view::MeetingView::load_named(&dev.svc.store(), m)
            .unwrap()
            .unwrap()
    };
    let summarise = |m, tldr: &str, seed| {
        let mut s = summary_of("Planning");
        s.tldr = tldr.into();
        crate::summary::store_summary(
            &a.svc,
            &named(&a, m),
            "test-model".into(),
            &s,
            &mut rand_chacha::ChaCha8Rng::seed_from_u64(seed),
        )
        .unwrap()
        .0
    };
    let m = meeting(&a.svc, "Planning", 1);
    summarise(m, "Alice will send the plan.", 2);
    ca.tick().unwrap();
    cb.tick().unwrap();

    // Renamed and summarised again before the second computer hears of either; the new name
    // holds the old one, so only the summary the rename found may be judged.
    let alice = named(&a, m)
        .speakers
        .into_iter()
        .find(|s| s.display_name == "Alice")
        .unwrap();
    a.clock.advance_ms(1_000);
    crate::edit::rename_speaker(&a.svc, m, alice.id, "Alice Smith").unwrap();
    a.clock.advance_ms(1_000);
    let fresh = summarise(m, "Alice Smith will send the plan.", 3);
    assert!(!named(&a, m).summary_out_of_date());
    ca.tick().unwrap();
    cb.tick().unwrap();
    let here = named(&b, m);
    assert_eq!(here.summary.as_ref().map(|s| s.id), Some(fresh));
    assert!(!here.summary_out_of_date(), "the new summary is current");
}

#[test]
fn sync_a_tick_that_changed_rows_names_their_meetings_and_only_then() {
    let relay = relay();
    let (a, b) = pair(&relay);
    let (ca, cb) = (a.client(), b.client());
    let changes = |r: &TickReport| SyncChanges::after(Some(r), None);

    // Rows of its own pushed, nothing pulled: nothing changed here, nothing is said.
    let own = meeting(&b.svc, "Pushed only", 6);
    let r = cb.tick().unwrap();
    assert_eq!(changes(&r), None, "{r:?}");

    // Rows pulled and written: their meeting, by id only.
    let m = meeting(&a.svc, "From the other computer", 7);
    ca.tick().unwrap();
    let r = cb.tick().unwrap();
    assert!(r.written > 0, "{r:?}");
    let c = changes(&r).expect("rows were written");
    assert_eq!(c.meetings, vec![m], "not this device's own ({own:?})");

    // A quiet tick says nothing.
    assert_eq!(changes(&cb.tick().unwrap()), None);

    // A rename there names the meeting again; a deletion there names it too.
    a.clock.advance_ms(1_000);
    a.rename(m, "Renamed there");
    ca.tick().unwrap();
    assert_eq!(changes(&cb.tick().unwrap()).unwrap().meetings, vec![m]);
    a.clock.advance_ms(1_000);
    a.svc.delete_meeting(m).unwrap();
    ca.tick().unwrap();
    let r = cb.tick().unwrap();
    assert_eq!(r.deleted_meetings, 1, "{r:?}");
    assert_eq!(changes(&r).unwrap().meetings, vec![m]);
}

#[test]
fn sync_changes_merge_the_tick_and_the_offload_step_once() {
    use earshot_core::identity::EntityId;
    let (m1, m2) = (EntityId::from_bytes([1; 16]), EntityId::from_bytes([2; 16]));
    let quiet = TickReport::default();
    let step_none = crate::offload::StepReport::default();
    assert_eq!(SyncChanges::after(Some(&quiet), Some(&step_none)), None);
    assert_eq!(SyncChanges::after(None, None), None);
    // A step that withdrew or handed back a request of this device names its meeting alone.
    let mut step = crate::offload::StepReport::default();
    step.meetings.insert(m2);
    assert_eq!(
        SyncChanges::after(Some(&quiet), Some(&step)),
        Some(SyncChanges {
            meetings: vec![m2],
            people: false
        })
    );
    // Both: one set, in id order.
    let mut tick = TickReport {
        written: 2,
        ..TickReport::default()
    };
    tick.meetings.extend([m2, m1]);
    assert_eq!(
        SyncChanges::after(Some(&tick), Some(&step)),
        Some(SyncChanges {
            meetings: vec![m1, m2],
            people: false
        })
    );
    // A person changed: no meeting named, people said.
    let person = TickReport {
        written: 1,
        people: true,
        ..TickReport::default()
    };
    assert_eq!(
        SyncChanges::after(Some(&person), None),
        Some(SyncChanges {
            meetings: vec![],
            people: true
        })
    );
}

#[test]
fn sync_a_panicking_change_listener_never_ends_sync() {
    let relay = relay();
    let (a, b) = pair(&relay);
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let seen = Arc::clone(&calls);
    let client = b.client().with_changes(Arc::new(move |_: &SyncChanges| {
        seen.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        panic!("an invented listener failure");
    }));
    assert!(!client.tell_changes(&SyncChanges::default()));
    let timing = SyncTiming {
        tick: Duration::from_millis(100),
        poll: Duration::from_millis(10),
        presence: Duration::from_secs(3600),
    };
    let thread = SyncThread::start(client, timing).unwrap();
    let ca = a.client();
    // Two changes, each heard (and panicked on) by the thread, which keeps syncing.
    for (n, title) in [(1_usize, "First"), (2, "Second")] {
        let m = meeting(&a.svc, title, 40 + n as u64);
        ca.tick().unwrap();
        wait_for("the thread pulled the meeting", || b.title(m).is_some());
        wait_for("the listener was told", || {
            calls.load(std::sync::atomic::Ordering::SeqCst) > n
        });
        assert!(!thread.ended(), "a listener panic ended sync");
    }
    assert_eq!(sync_status(&b.svc.store()).unwrap().problem, None);
    thread.stop();
}

/// A host that answers every hand-back with one answer and counts them (each one is a pass of the
/// thread's offload step that reached the meeting).
struct Answers {
    answer: crate::offload::ContinueAnswer,
    handed: std::sync::Mutex<Vec<earshot_core::identity::EntityId>>,
}

impl Answers {
    fn new(answer: crate::offload::ContinueAnswer) -> Arc<Self> {
        Arc::new(Self {
            answer,
            handed: std::sync::Mutex::default(),
        })
    }
    fn count(&self) -> usize {
        self.handed.lock().unwrap().len()
    }
}

impl crate::offload::OffloadHost for Answers {
    fn continue_run(&self, job: crate::offload::WriteHere) -> crate::offload::ContinueAnswer {
        self.handed.lock().unwrap().push(job.meeting);
        self.answer
    }
    fn write_for(&self, _: earshot_core::model::SummaryRequest) {}
}

/// A listener that records every call.
fn recorder() -> (ChangeListener, Arc<std::sync::Mutex<Vec<SyncChanges>>>) {
    let heard: Arc<std::sync::Mutex<Vec<SyncChanges>>> = Arc::default();
    let sink = Arc::clone(&heard);
    (
        Arc::new(move |c: &SyncChanges| sink.lock().unwrap().push(c.clone())),
        heard,
    )
}

fn quick() -> SyncTiming {
    SyncTiming {
        tick: Duration::from_millis(50),
        poll: Duration::from_millis(10),
        presence: Duration::from_secs(3600),
    }
}

/// T12.81: what the offload step writes reaches the window in the pass's one call, though the
/// tick itself changed nothing here (the original bug: only the tick's rows were told), and a
/// hand-back the host keeps refusing is not told again every pass.
#[test]
fn sync_thread_tells_the_window_what_the_offload_step_wrote_once() {
    let relay = relay();
    let (a, b) = pair(&relay);
    let m = meeting(&a.svc, "Asked of the other computer", 50);
    crate::offload::ask(&a.svc, m, b.svc.store().device())
        .unwrap()
        .unwrap();
    let host = Answers::new(crate::offload::ContinueAnswer::Refused);
    // A first step reads when the request was asked, on this device's clock.
    crate::offload::Offload::new()
        .step(&a.svc, &mut || None, &*host)
        .unwrap();
    a.clock.advance_ms(crate::offload::CLAIM_WITHIN_MS);
    let (listener, heard) = recorder();
    let calls = || heard.lock().unwrap().clone();
    let thread = SyncThread::start_offloading(
        a.client().with_changes(listener),
        quick(),
        Arc::clone(&host) as Arc<dyn crate::offload::OffloadHost>,
    )
    .unwrap();
    // The first pass withdraws the request (nothing pulled): told once, naming the meeting.
    wait_for("the step's withdrawal was told", || !calls().is_empty());
    // Refused every pass since: the host is asked again each pass, the window is not told again.
    wait_for("three more passes handed it back", || host.count() >= 4);
    assert_eq!(
        calls(),
        vec![SyncChanges {
            meetings: vec![m],
            people: false
        }]
    );
    assert_eq!(calls().len(), 1, "one call, in the withdrawing pass only");
    thread.stop();
}

/// T12.81: a step that panics after writing keeps what it wrote in the caller's report, as the
/// tick's report survives a problem, so the thread still tells the window.
#[test]
fn sync_a_step_that_panics_after_writing_keeps_its_report() {
    struct Panics;
    impl crate::offload::OffloadHost for Panics {
        fn continue_run(&self, _: crate::offload::WriteHere) -> crate::offload::ContinueAnswer {
            panic!("an invented host failure")
        }
        fn write_for(&self, _: earshot_core::model::SummaryRequest) {}
    }
    let relay = relay();
    let (a, b) = pair(&relay);
    let m = meeting(&a.svc, "Asked, then the host fails", 52);
    crate::offload::ask(&a.svc, m, b.svc.store().device())
        .unwrap()
        .unwrap();
    let state = crate::offload::Offload::new();
    state.step(&a.svc, &mut || None, &Panics).unwrap();
    a.clock.advance_ms(crate::offload::CLAIM_WITHIN_MS);
    let mut report = crate::offload::StepReport::default();
    let ran = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        state.step_into(&a.svc, &mut || None, &Panics, &mut report)
    }));
    assert!(ran.is_err(), "the host panicked");
    assert_eq!(report.withdrawn, 1);
    assert_eq!(
        SyncChanges::after(None, Some(&report)).map(|c| c.meetings),
        Some(vec![m])
    );
}

/// T12.81: a thread that stops on a problem the user must fix still tells the window what its
/// last offload step withdrew and handed back.
#[test]
fn sync_thread_stopping_on_a_problem_tells_what_its_last_step_wrote() {
    let relay = relay();
    let (a, b) = pair(&relay);
    let m = meeting(&a.svc, "Asked before the removal", 51);
    crate::offload::ask(&a.svc, m, b.svc.store().device())
        .unwrap()
        .unwrap();
    let host = Answers::new(crate::offload::ContinueAnswer::Started);
    let (listener, heard) = recorder();
    let a_id = a.svc.store().device();
    remove_other_device(&b.svc, b.vault(), a_id).unwrap();
    let thread = SyncThread::start_offloading(
        a.client().with_changes(listener),
        quick(),
        Arc::clone(&host) as Arc<dyn crate::offload::OffloadHost>,
    )
    .unwrap();
    wait_for("the thread stopped on the problem", || thread.ended());
    assert_eq!(host.count(), 1, "handed back to write here");
    assert_eq!(
        heard.lock().unwrap().clone(),
        vec![SyncChanges {
            meetings: vec![m],
            people: false
        }]
    );
}

#[test]
fn sync_a_meeting_being_recorded_is_not_pushed_until_it_is_saved() {
    let relay = relay();
    let (a, b) = pair(&relay);
    let (ca, cb) = (a.client(), b.client());
    let saved = meeting(&a.svc, "Saved earlier", 3);
    let live = meeting(&a.svc, "Recording now", 4);
    let lease = a
        .svc
        .lease_meeting_for(live, MeetingJob::Recording)
        .unwrap();
    ca.tick().unwrap();
    cb.tick().unwrap();
    assert!(b.title(saved).is_some(), "the saved meeting went");
    assert_eq!(b.title(live), None, "the recording did not");
    assert!(a.svc.store().outbox_len().unwrap() > 0, "its rows wait");
    // Saved: it goes on the next tick.
    drop(lease);
    ca.tick().unwrap();
    cb.tick().unwrap();
    assert_eq!(b.title(live).as_deref(), Some("Recording now"));
    assert_eq!(a.rows(), b.rows());
}

#[test]
fn sync_a_meeting_deleted_on_one_device_is_deleted_on_the_other() {
    let relay = relay();
    let (a, b) = pair(&relay);
    let (ca, cb) = (a.client(), b.client());
    let m = meeting(&a.svc, "To delete", 5);
    ca.tick().unwrap();
    cb.tick().unwrap();
    assert!(b.title(m).is_some());
    a.clock.advance_ms(1_000);
    a.svc.delete_meeting(m).unwrap();
    ca.tick().unwrap();
    let r = cb.tick().unwrap();
    assert_eq!(r.deleted_meetings, 1, "{r:?}");
    assert_eq!(b.title(m), None);
    let row = b.svc.store().meeting(m).unwrap().unwrap();
    assert!(row.meta.deleted);
    cb.tick().unwrap();
    ca.tick().unwrap();
    assert_eq!(a.rows(), b.rows());
}

#[test]
fn sync_a_meeting_in_use_holds_its_pulled_rows_until_it_is_free() {
    let relay = relay();
    let (a, b) = pair(&relay);
    let (ca, cb) = (a.client(), b.client());
    let m = meeting(&a.svc, "Shared", 6);
    ca.tick().unwrap();
    cb.tick().unwrap();
    a.clock.advance_ms(1_000);
    a.rename(m, "Renamed while busy there");
    ca.tick().unwrap();
    // A job on the other device holds the meeting: the rename waits.
    let job = b.svc.lease_meeting_for(m, MeetingJob::Processing).unwrap();
    let r = cb.tick().unwrap();
    assert_eq!(r.held, 1, "{r:?}");
    assert_eq!(b.title(m).as_deref(), Some("Shared"));
    drop(job);
    cb.tick().unwrap();
    assert_eq!(b.title(m).as_deref(), Some("Renamed while busy there"));
}

#[test]
fn sync_setup_problems_keep_nothing() {
    let relay = relay();
    let a = Device::new("sync-bad");
    assert_eq!(
        set_up_first_device(
            &a.svc,
            &*a.vault,
            &relay.relay.url(),
            "not-the-secret-at-all-no"
        ),
        Err(SyncProblem::BadSecret)
    );
    assert_eq!(
        join_with_code(&a.svc, &*a.vault, &relay.relay.url(), "not a code"),
        Err(SyncProblem::BadCode)
    );
    // A well-formed code the relay never offered.
    let unknown = earshot_sync::PairingCode::generate().unwrap().to_string();
    assert_eq!(
        join_with_code(&a.svc, &*a.vault, &relay.relay.url(), &unknown),
        Err(SyncProblem::NoSlot)
    );
    assert_eq!(
        set_up_first_device(&a.svc, &*a.vault, "ftp://example.com", SECRET),
        Err(SyncProblem::Address)
    );
    assert!(!a.vault.has().unwrap());
    let settings = sync_settings(&a.svc.store()).unwrap();
    assert_eq!(settings.address, None);
    assert!(!settings.on);
}

#[test]
fn sync_a_second_vault_cannot_claim_the_server() {
    let relay = relay();
    let (a, _b) = pair(&relay);
    let other = Device::new("sync-other");
    // The setup secret alone does not join: this device's new key is not the vault's.
    assert_eq!(
        set_up_first_device(&other.svc, &*other.vault, &relay.relay.url(), SECRET),
        Err(SyncProblem::KeyMismatch)
    );
    assert!(!other.vault.has().unwrap());
    a.client().tick().unwrap();
}

#[test]
fn sync_a_revoked_device_stops_and_says_so() {
    let relay = relay();
    let (a, b) = pair(&relay);
    let cb = b.client();
    cb.tick().unwrap();
    // The first device revokes the second.
    let secrets = a.vault.load().unwrap().unwrap();
    let ca = a.client();
    ca.relay()
        .revoke(
            &secrets.token,
            &earshot_sync::wire::RevokeRequest {
                device_id: earshot_sync::wire::B64(
                    b.svc.store().device().as_uuid().as_bytes().to_vec(),
                ),
            },
        )
        .unwrap();
    assert_eq!(cb.tick(), Err(SyncProblem::UnknownDevice));
    assert!(SyncProblem::UnknownDevice.stops());
    assert_eq!(
        sync_status(&b.svc.store()).unwrap().problem,
        Some(SyncProblem::UnknownDevice)
    );
    // Removing it here then forgets everything it kept, with nothing left to revoke.
    remove_this_device(&b.svc, b.vault()).unwrap();
    assert!(!b.vault.has().unwrap());
    assert_eq!(sync_settings(&b.svc.store()).unwrap().address, None);
    assert_eq!(sync_status(&b.svc.store()).unwrap(), SyncStatus::default());
}

#[test]
fn sync_removing_a_device_revokes_its_token() {
    let relay = relay();
    let (a, b) = pair(&relay);
    let kept = b.vault.load().unwrap().unwrap();
    remove_this_device(&b.svc, b.vault()).unwrap();
    assert!(!b.vault.has().unwrap());
    // The token it had no longer works.
    let stale = Device::new("sync-stale");
    {
        let mut db = stale.svc.store();
        crate::local::set(
            &mut db,
            &crate::local::DeviceRecord::SyncServer.key(),
            relay.relay.url().as_bytes(),
        )
        .unwrap();
    }
    stale.vault.save(&kept.key, &kept.token).unwrap();
    assert_eq!(stale.client().tick(), Err(SyncProblem::UnknownDevice));
    a.client().tick().unwrap();
}

#[test]
fn sync_removing_another_device_stops_it_and_keeps_its_rows() {
    let relay = relay();
    let (a, b) = pair(&relay);
    let cb = b.client();
    let from_b = meeting(&b.svc, "Kept after removal", 3);
    cb.tick().unwrap();
    let b_id = b.svc.store().device();
    remove_other_device(&a.svc, a.vault(), b_id).unwrap();
    // The removed device's next request is refused as for a removed self, and it stops.
    assert_eq!(cb.tick(), Err(SyncProblem::UnknownDevice));
    assert_eq!(
        sync_status(&b.svc.store()).unwrap().problem,
        Some(SyncProblem::UnknownDevice)
    );
    // It leaves the list; this device keeps syncing.
    let listed: Vec<_> = vault_devices(&a.svc, a.vault())
        .unwrap()
        .into_iter()
        .map(|d| (d.device, d.this_device))
        .collect();
    assert_eq!(listed, [(a.svc.store().device(), true)]);
    let ca = a.client();
    ca.tick().unwrap();
    assert_eq!(a.title(from_b).as_deref(), Some("Kept after removal"));
    // Its rows stay on the server: a computer joining now still receives them.
    let offer = offer_pairing_code(&a.svc, a.vault()).unwrap();
    let c = Device::new("sync-c");
    join_with_code(&c.svc, &*c.vault, &relay.relay.url(), offer.code()).unwrap();
    c.client().tick().unwrap();
    assert_eq!(c.title(from_b).as_deref(), Some("Kept after removal"));
    // Removing it again changes nothing.
    remove_other_device(&a.svc, a.vault(), b_id).unwrap();
    assert_only_the_relay(&ca, &relay);
}

/// The recovery the removed computer's page offers (T11.6c, the owner's decision 2026-11-16):
/// remove it here, then join again with a pairing code from a live computer; it syncs again
/// under its own device id, both ways, with no request to the relay while forgetting.
#[test]
fn sync_a_removed_device_follows_the_copy_and_syncs_again() {
    let relay = relay();
    let (a, b) = pair(&relay);
    let (ca, cb) = (a.client(), b.client());
    let before = meeting(&a.svc, "Before the removal", 4);
    ca.tick().unwrap();
    cb.tick().unwrap();
    let b_id = b.svc.store().device();
    remove_other_device(&a.svc, a.vault(), b_id).unwrap();
    assert_eq!(cb.tick(), Err(SyncProblem::UnknownDevice));

    // "Remove it here": forgetting asks the server nothing (it would only hear 401 again), so
    // it works even with the server out of reach (a port nothing listens on).
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    {
        let mut db = b.svc.store();
        crate::local::set(
            &mut db,
            &DeviceRecord::SyncServer.key(),
            format!("http://127.0.0.1:{port}").as_bytes(),
        )
        .unwrap();
    }
    remove_this_device(&b.svc, b.vault()).unwrap();
    assert!(!b.vault.has().unwrap());

    // "Then join again with a pairing code": shown on the live computer.
    let during = meeting(&a.svc, "While it was removed", 5);
    ca.tick().unwrap();
    let offer = offer_pairing_code(&a.svc, a.vault()).unwrap();
    join_with_code(&b.svc, &*b.vault, &relay.relay.url(), offer.code()).unwrap();
    assert_eq!(b.svc.store().device(), b_id, "the same device id came back");
    let cb = b.client();
    cb.tick().unwrap();
    assert_eq!(b.title(before).as_deref(), Some("Before the removal"));
    assert_eq!(b.title(during).as_deref(), Some("While it was removed"));
    // Both ways again.
    let after = meeting(&b.svc, "After rejoining", 6);
    cb.tick().unwrap();
    ca.tick().unwrap();
    assert_eq!(a.title(after).as_deref(), Some("After rejoining"));
    let listed: Vec<_> = vault_devices(&a.svc, a.vault())
        .unwrap()
        .into_iter()
        .map(|d| d.device)
        .collect();
    assert_eq!(listed.len(), 2);
    assert!(listed.contains(&b_id));
    assert_only_the_relay(&cb, &relay);
}

/// A credential store that refuses every save.
struct RefusingVault;

impl SyncVault for RefusingVault {
    fn save(&self, _: &VaultKey, _: &DeviceToken) -> Result<(), VaultError> {
        Err(VaultError::Store)
    }
    fn has(&self) -> Result<bool, VaultError> {
        Ok(false)
    }
    fn remove(&self) -> Result<(), VaultError> {
        Ok(())
    }
    fn load(&self) -> Result<Option<SyncSecrets>, VaultError> {
        Ok(None)
    }
}

/// The residual the spec names (T11.6c "Coming back"): a rejoin the relay registered but this
/// computer could not keep leaves a live id no one holds a token for; the next code is
/// `device_exists` until another computer removes it, and then the join works.
#[test]
fn sync_a_rejoin_that_could_not_be_kept_is_recovered_by_removing_it_again() {
    let relay = relay();
    let (a, b) = pair(&relay);
    let b_id = b.svc.store().device();
    remove_other_device(&a.svc, a.vault(), b_id).unwrap();
    assert_eq!(b.client().tick(), Err(SyncProblem::UnknownDevice));
    remove_this_device(&b.svc, b.vault()).unwrap();
    let offer = offer_pairing_code(&a.svc, a.vault()).unwrap();
    assert_eq!(
        join_with_code(&b.svc, &RefusingVault, &relay.relay.url(), offer.code()),
        Err(SyncProblem::Store)
    );
    let offer = offer_pairing_code(&a.svc, a.vault()).unwrap();
    assert_eq!(
        join_with_code(&b.svc, &*b.vault, &relay.relay.url(), offer.code()),
        Err(SyncProblem::DeviceExists)
    );
    remove_other_device(&a.svc, a.vault(), b_id).unwrap();
    let offer = offer_pairing_code(&a.svc, a.vault()).unwrap();
    join_with_code(&b.svc, &*b.vault, &relay.relay.url(), offer.code()).unwrap();
    b.client().tick().unwrap();
}

#[test]
fn sync_removing_another_device_never_takes_this_one_or_works_once_removed() {
    let relay = relay();
    let (a, b) = pair(&relay);
    // This device goes only through remove_this_device: the other path refuses it and sends
    // nothing.
    let me = a.svc.store().device();
    assert_eq!(
        remove_other_device(&a.svc, a.vault(), me),
        Err(SyncProblem::Internal)
    );
    a.client().tick().unwrap();
    // A removed device cannot remove another: it hears that it was removed.
    remove_other_device(&a.svc, a.vault(), b.svc.store().device()).unwrap();
    assert_eq!(
        remove_other_device(&b.svc, b.vault(), me),
        Err(SyncProblem::UnknownDevice)
    );
    a.client().tick().unwrap();
    // A device that is not set up has nothing to ask with.
    let lone = Device::new("sync-lone");
    assert_eq!(
        remove_other_device(&lone.svc, lone.vault(), me),
        Err(SyncProblem::Vault)
    );
}

#[test]
fn sync_server_unreachable_is_a_status_and_a_retry() {
    let a = Device::new("sync-down");
    // A loopback port nothing listens on.
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let address = format!("http://127.0.0.1:{port}");
    {
        let mut db = a.svc.store();
        crate::local::set(
            &mut db,
            &crate::local::DeviceRecord::SyncServer.key(),
            address.as_bytes(),
        )
        .unwrap();
    }
    let key = earshot_sync::VaultKey::generate().unwrap();
    a.vault
        .save(&key, &DeviceToken::new("invented-token".into()))
        .unwrap();
    let c = a.client();
    assert_eq!(c.tick(), Err(SyncProblem::Unreachable));
    assert!(!SyncProblem::Unreachable.stops());
    let status = sync_status(&a.svc.store()).unwrap();
    assert_eq!(status.problem, Some(SyncProblem::Unreachable));
    assert_eq!(status.last_success_unix_ms, None);
}

#[test]
fn sync_overview_and_device_list_show_counts_and_short_ids_only() {
    let relay = relay();
    let (a, b) = pair(&relay);
    meeting(&a.svc, "Waiting to go", 13);
    let view = sync_overview(&a.svc, &*a.vault).unwrap();
    assert!(view.set_up);
    assert!(view.settings.on);
    assert!(matches!(view.settings.address, Some(Ok(_))));
    assert!(view.waiting > 0);
    assert_eq!(view.held, 0);
    a.client().tick().unwrap();
    assert_eq!(sync_overview(&a.svc, &*a.vault).unwrap().waiting, 0);
    let devices = vault_devices(&a.svc, a.vault()).unwrap();
    assert_eq!(devices.len(), 2);
    assert!(devices[0].this_device && !devices[1].this_device);
    let me = a.svc.store().device();
    let hex: String = me.as_uuid().as_bytes()[..4]
        .iter()
        .map(|x| format!("{x:02x}"))
        .collect();
    assert_eq!(devices[0].short_id, hex);
    assert_eq!(devices[0].short_id.len(), 8);
    // A device that is not set up has no list to show.
    let lone = Device::new("sync-lone");
    assert!(!sync_overview(&lone.svc, &*lone.vault).unwrap().set_up);
    assert_eq!(
        vault_devices(&lone.svc, lone.vault()),
        Err(SyncProblem::Vault)
    );
    drop(b);
}

#[test]
fn sync_backoff_doubles_from_the_tick_to_its_cap() {
    assert_eq!(backoff(0), Duration::from_secs(30));
    assert_eq!(backoff(1), Duration::from_secs(30));
    assert_eq!(backoff(2), Duration::from_secs(60));
    assert_eq!(backoff(3), Duration::from_secs(120));
    assert_eq!(backoff(5), Duration::from_secs(480));
    assert_eq!(backoff(6), Duration::from_secs(900));
    assert_eq!(backoff(7), Duration::from_secs(900));
    assert_eq!(backoff(u32::MAX), Duration::from_secs(900));
}

#[test]
fn sync_addresses_follow_the_rule() {
    for ok in [
        "https://sync.example.com",
        "https://sync.example.com/",
        "https://sync.example.com:8443",
        "http://127.0.0.1:8787",
        "http://localhost:8787",
        "http://[::1]:8787",
        "  https://sync.example.com  ",
    ] {
        assert!(check_address(ok).is_ok(), "{ok}");
    }
    for (bad, why) in [
        ("", AddressRefused::Empty),
        ("   ", AddressRefused::Empty),
        ("http://sync.example.com", AddressRefused::NotHttps),
        ("http://192.168.1.10:8787", AddressRefused::NotHttps),
        ("ftp://sync.example.com", AddressRefused::NotAnAddress),
        ("sync.example.com", AddressRefused::NotAnAddress),
        (
            "https://user:pw@sync.example.com",
            AddressRefused::Credentials,
        ),
        (
            "https://sync.example.com/earshot",
            AddressRefused::NotTheRoot,
        ),
        ("https://sync.example.com/?a=1", AddressRefused::NotTheRoot),
        ("https://sync.example.com/#x", AddressRefused::NotTheRoot),
    ] {
        assert_eq!(check_address(bad), Err(why), "{bad:?}");
    }
    let long = format!("https://{}.example.com", "a".repeat(MAX_ADDRESS));
    assert_eq!(check_address(&long), Err(AddressRefused::TooLong));
}

#[test]
fn sync_turning_voiceprints_on_reads_from_the_start() {
    let relay = relay();
    let (_a, b) = pair(&relay);
    let cb = b.client();
    meeting(&b.svc, "Anything", 7);
    cb.tick().unwrap();
    assert!(pull_cursor(&b.svc.store()).unwrap() > 0);
    set_sync_voiceprints(&b.svc, true).unwrap();
    assert!(sync_settings(&b.svc.store()).unwrap().voiceprints);
    assert_eq!(pull_cursor(&b.svc.store()).unwrap(), 0);
    // Already on: the cursor is left alone.
    cb.tick().unwrap();
    let at = pull_cursor(&b.svc.store()).unwrap();
    set_sync_voiceprints(&b.svc, true).unwrap();
    assert_eq!(pull_cursor(&b.svc.store()).unwrap(), at);
    set_sync_voiceprints(&b.svc, false).unwrap();
    assert!(!sync_settings(&b.svc.store()).unwrap().voiceprints);
}

#[test]
fn sync_secrets_never_show_in_debug_output() {
    let token = DeviceToken::new("invented-token-value".into());
    assert_eq!(format!("{token:?}"), "DeviceToken(..)");
    let vault = MemoryVault::default();
    let key = earshot_sync::VaultKey::from_bytes([0xAB; 32]);
    vault.save(&key, &token).unwrap();
    assert_eq!(format!("{vault:?}"), "MemoryVault(..)");
    let secrets = vault.load().unwrap().unwrap();
    let shown = format!("{secrets:?}");
    for leak in ["invented", "171", "AB", "ab", "q6ur"] {
        assert!(!shown.contains(leak), "{shown}");
    }
    assert_eq!(
        shown,
        "SyncSecrets { key: VaultKey(..), token: DeviceToken(..) }"
    );
    let relay = relay();
    let (a, _b) = pair(&relay);
    let offer = offer_pairing_code(&a.svc, a.vault()).unwrap();
    assert!(!format!("{offer:?}").contains(offer.code()));
    assert!(offer.expires_in_s > 0);
}

#[test]
fn sync_problem_words_are_distinct_and_round_trip() {
    let mut words = std::collections::BTreeSet::new();
    for p in SyncProblem::ALL {
        assert!(words.insert(p.word()), "{p:?}");
        let json = serde_json::to_string(&p).unwrap();
        assert_eq!(json, format!("\"{}\"", p.word()));
        assert_eq!(serde_json::from_str::<SyncProblem>(&json).unwrap(), p);
    }
    // Only these relay words stop the loop; every other one is a retry or a setup answer.
    let stopping: Vec<_> = earshot_sync::wire::ErrorWord::ALL
        .into_iter()
        .filter(|w| client::problem_of_word(*w).stops())
        .collect();
    assert_eq!(
        stopping,
        [
            earshot_sync::wire::ErrorWord::UnknownDevice,
            earshot_sync::wire::ErrorWord::KeyMismatch,
            earshot_sync::wire::ErrorWord::UpdateNeeded,
        ]
    );
}

#[test]
fn sync_thread_sends_a_local_write_and_stops_when_asked() {
    let relay = relay();
    let (a, b) = pair(&relay);
    let timing = SyncTiming {
        tick: Duration::from_millis(200),
        poll: Duration::from_millis(20),
        presence: Duration::from_millis(50),
    };
    let thread = SyncThread::start(a.client(), timing).unwrap();
    let m = meeting(&a.svc, "Written while running", 8);
    let cb = b.client();
    let deadline = Instant::now() + Duration::from_secs(60);
    while b.title(m).is_none() {
        assert!(
            Instant::now() < deadline,
            "the write never reached the other device"
        );
        std::thread::sleep(Duration::from_millis(20));
        cb.tick().unwrap();
    }
    assert!(!thread.ended());
    thread.stop();
}

#[test]
fn sync_thread_ends_on_a_problem_the_person_must_fix() {
    let a = Device::new("sync-ends");
    {
        let mut db = a.svc.store();
        crate::local::set(
            &mut db,
            &crate::local::DeviceRecord::SyncServer.key(),
            b"https://sync.invalid",
        )
        .unwrap();
    }
    // No key or token stored: the first tick stops the thread.
    let thread = SyncThread::start(a.client(), SyncTiming::default()).unwrap();
    let deadline = Instant::now() + Duration::from_secs(60);
    while !thread.ended() {
        assert!(Instant::now() < deadline, "the thread did not end");
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(
        sync_status(&a.svc.store()).unwrap().problem,
        Some(SyncProblem::Vault)
    );
}

#[test]
fn sync_a_meeting_edited_here_after_it_was_deleted_elsewhere_is_deleted_everywhere() {
    let relay = relay();
    let (a, b) = pair(&relay);
    let (ca, cb) = (a.client(), b.client());
    let m = meeting(&a.svc, "Deleted there, renamed here", 9);
    ca.tick().unwrap();
    cb.tick().unwrap();
    // A deletes; B renames afterwards by its own clock, before it hears of the deletion.
    a.clock.advance_ms(1_000);
    a.svc.delete_meeting(m).unwrap();
    b.clock.advance_ms(5_000);
    b.rename(m, "Renamed after the deletion");
    // A pushes the deletion. B pushes its rename first (the relay keeps the later row), then
    // pulls the deleted children.
    ca.tick().unwrap();
    cb.tick().unwrap();
    // A refuses the live copy and stamps its tombstone again; its next tick sends it.
    let r = ca.tick().unwrap();
    assert_eq!(a.title(m), None, "the deletion stays on A: {r:?}");
    ca.tick().unwrap();
    // B hears of it and runs its own deletion.
    let r = cb.tick().unwrap();
    assert_eq!(r.deleted_meetings, 1, "{r:?}");
    assert_eq!(b.title(m), None, "no live meeting over cleared rows");
    cb.tick().unwrap();
    ca.tick().unwrap();
    assert_eq!(a.title(m), None);
    assert_eq!(a.rows(), b.rows());
}

#[test]
fn sync_cursor_moves_only_from_where_the_page_was_pulled() {
    let a = Device::new("sync-cursor");
    let mut db = a.svc.store();
    assert_eq!(pull_cursor(&db).unwrap(), 0);
    assert!(advance_cursor(&mut db, 0, 500).unwrap());
    assert_eq!(pull_cursor(&db).unwrap(), 500);
    // A reset while a page after 500 was on the way: the page does not undo it.
    crate::local::set(&mut db, &DeviceRecord::SyncCursor.key(), &encode_cursor(0)).unwrap();
    assert!(!advance_cursor(&mut db, 500, 700).unwrap());
    assert_eq!(pull_cursor(&db).unwrap(), 0);
}

#[test]
fn sync_cursor_and_status_bytes_are_pinned() {
    assert_eq!(encode_cursor(7), b"\x01\x00\x00\x00{\"pull_seq\":7}");
    let a = Device::new("sync-pins");
    let mut db = a.svc.store();
    let status = SyncStatus {
        last_success_unix_ms: Some(5),
        problem: Some(SyncProblem::Unreachable),
        unreadable: 2,
    };
    set_sync_status(&mut db, &status).unwrap();
    assert_eq!(
        db.local_value(SYNC_STATUS_KEY).unwrap().unwrap(),
        b"\x01\x00\x00\x00{\"last_success_unix_ms\":5,\"problem\":\"unreachable\",\"unreadable\":2}"
    );
    assert_eq!(sync_status(&db).unwrap(), status);
    // Another version, or bytes that do not read, are the defaults.
    db.set_local_value(SYNC_STATUS_KEY, b"\x02\x00\x00\x00{}")
        .unwrap();
    assert_eq!(sync_status(&db).unwrap(), SyncStatus::default());
    db.set_local_value(SYNC_CURSOR_KEY, b"\x01\x00").unwrap();
    assert_eq!(pull_cursor(&db).unwrap(), 0);
}

#[test]
fn sync_joining_sends_rows_that_went_to_no_vault_yet() {
    let relay = relay();
    let a = Device::new("sync-join-a");
    let b = Device::new("sync-join-b");
    // B's meeting was sent once before (its outbox emptied), to no vault this one knows.
    let earlier = meeting(&b.svc, "From before joining", 10);
    {
        let mut db = b.svc.store();
        let rows = db.outbox(10_000).unwrap();
        let sent: Vec<_> = rows
            .iter()
            .map(|r| (r.table(), r.id(), r.meta().updated_at))
            .collect();
        db.outbox_sent(&sent).unwrap();
        assert_eq!(db.outbox_len().unwrap(), 0);
    }
    set_up_first_device(&a.svc, &*a.vault, &relay.relay.url(), SECRET).unwrap();
    let offer = offer_pairing_code(&a.svc, a.vault()).unwrap();
    join_with_code(&b.svc, &*b.vault, &relay.relay.url(), offer.code()).unwrap();
    b.client().tick().unwrap();
    a.client().tick().unwrap();
    assert_eq!(a.title(earlier).as_deref(), Some("From before joining"));
}

#[test]
fn sync_thread_retries_an_unreachable_server_until_it_answers() {
    let dir = TempDir::new("sync-retry-relay");
    let server = RelayServer::open(&relay_config(&dir)).unwrap();
    let first = LocalRelay::start(server).unwrap();
    let port = first.addr().port();
    let url = first.url();
    let a = Device::new("sync-retry");
    set_up_first_device(&a.svc, &*a.vault, &url, SECRET).unwrap();
    meeting(&a.svc, "Waits for the server", 11);
    // The server goes away; the thread keeps trying.
    drop(first);
    let timing = SyncTiming {
        tick: Duration::from_millis(100),
        poll: Duration::from_millis(10),
        presence: Duration::from_millis(50),
    };
    let thread = SyncThread::start(a.client(), timing).unwrap();
    wait_for("no unreachable status", || {
        sync_status(&a.svc.store()).unwrap().problem == Some(SyncProblem::Unreachable)
    });
    assert!(
        !thread.ended(),
        "an unreachable server is a retry, not a stop"
    );
    assert!(a.svc.store().outbox_len().unwrap() > 0);
    // Back on the same port with the same data: the next try succeeds and sends the meeting.
    let again =
        LocalRelay::start_on(RelayServer::open(&relay_config(&dir)).unwrap(), port).unwrap();
    wait_for("the thread never recovered", || {
        let status = sync_status(&a.svc.store()).unwrap();
        status.problem.is_none() && status.last_success_unix_ms.is_some()
    });
    wait_for("the queued meeting was never sent", || {
        a.svc.store().outbox_len().unwrap() == 0
    });
    assert!(!thread.ended());
    thread.stop();
    drop(again);
}

// T11.3b: the convergence rules (docs/sync-spec.md section 7).

/// A summary saying `title`.
fn summary_of(title: &str) -> earshot_core::summary::SummaryV1 {
    earshot_core::summary::SummaryV1 {
        title: title.into(),
        tldr: "An invented summary.".into(),
        enhanced_notes: vec![],
        decisions: vec![],
        action_items: vec![],
        open_questions: vec![],
        topics: vec![],
    }
}

/// A "Me" person and a summary of `meeting`, made on `dev` with its own clock and HLC, not
/// stored there.
fn me_and_summary(
    dev: &Device,
    meeting: earshot_core::identity::EntityId,
    seed: u64,
) -> Vec<earshot_store::SyncedRow> {
    use rand_core::SeedableRng;
    let clock = Arc::clone(&dev.svc.clock);
    let mut rng = rand_chacha::ChaCha8Rng::seed_from_u64(seed);
    let mut db = dev.svc.store();
    let (_, hlc) = db.parts();
    let mut stamp = earshot_core::model::Stamp {
        clock: &*clock,
        hlc,
        rng: &mut rng,
    };
    let me = earshot_core::voiceprint::Person::new("Me", true, &mut stamp);
    let summary = earshot_core::model::Summary::new(
        meeting,
        earshot_core::prompt::Template::General,
        None,
        summary_of(&format!("Written on device {seed}")),
        &mut stamp,
    );
    vec![
        earshot_store::SyncedRow::Person(me),
        earshot_store::SyncedRow::Summary(summary),
    ]
}

/// Every synced row of `meeting` on `dev`, as a page.
fn meeting_rows(
    dev: &Device,
    m: earshot_core::identity::EntityId,
) -> Vec<earshot_store::SyncedRow> {
    use earshot_store::SyncedRow as R;
    let db = dev.svc.store();
    let mut rows = vec![R::Meeting(db.meeting(m).unwrap().unwrap())];
    rows.extend(db.tracks(m).unwrap().into_iter().map(R::Track));
    rows.extend(db.speakers(m).unwrap().into_iter().map(R::Speaker));
    rows.extend(db.segments(m).unwrap().into_iter().map(R::Segment));
    rows.extend(db.notes(m).unwrap().into_iter().map(R::Note));
    rows
}

/// Applies `rows` on `dev` as one page, as the client does (bodies are not opened here).
fn apply_page(dev: &Device, rows: &[earshot_store::SyncedRow]) -> earshot_store::Applied {
    let page: Vec<earshot_store::Incoming> = rows
        .iter()
        .enumerate()
        .map(|(i, r)| earshot_store::Incoming {
            key: *r.id().as_uuid().as_bytes(),
            seq: u64::try_from(i).unwrap() + 1,
            stamp: r.meta().updated_at,
            body: vec![0xA5; 8],
            row: Some(r.clone()),
        })
        .collect();
    let never = |_: earshot_core::identity::EntityId| false;
    let rules = earshot_store::ApplyRules {
        kinds: earshot_store::SyncKinds {
            voiceprints: false,
            dictionary: true,
        },
        leased: &never,
        now_ms: dev.svc.clock.now_unix_ms(),
        after: None,
    };
    let clock = Arc::clone(&dev.svc.clock);
    let mut db = dev.svc.store();
    let (store, hlc) = db.parts();
    store.apply_remote(&page, &rules, hlc, &*clock).unwrap()
}

/// What must agree across devices: each person's id, mark, merge and deletion, and each
/// summary's id, meeting and deletion. Stamps differ, as each device repairs with its own.
fn converged_rows(dev: &Device, m: earshot_core::identity::EntityId) -> Vec<String> {
    let db = dev.svc.store();
    let mut out: Vec<String> = db
        .persons()
        .unwrap()
        .iter()
        .map(|p| {
            format!(
                "person {} me={} merged={:?} deleted={}",
                p.id, p.is_me, p.merged_into, p.meta.deleted
            )
        })
        .collect();
    out.extend(db.summaries(m).unwrap().iter().map(|s| {
        format!(
            "summary {} of {} deleted={} title={}",
            s.id, s.meeting_id, s.meta.deleted, s.summary.title
        )
    }));
    out.sort();
    out
}

#[test]
fn sync_converge_three_devices_reach_the_same_rows_in_any_order() {
    use earshot_core::identity::Clock as _;
    // Two devices each made their own "Me" and wrote a summary of the same meeting.
    let x = Device::new("sync-converge-x");
    let y = Device::new("sync-converge-y");
    let m = meeting(&x.svc, "Shared meeting", 21);
    x.clock.advance_ms(1_000);
    let mut first = meeting_rows(&x, m);
    first.extend(me_and_summary(&x, m, 1));
    y.clock.set_ms(x.clock.now_unix_ms() + 5_000);
    let second = me_and_summary(&y, m, 2);
    let earliest_me = first[first.len() - 2].id();
    let latest_summary = second[1].id();

    // Every order of the two pages, and both in one page each way.
    let orders: Vec<Vec<Vec<earshot_store::SyncedRow>>> = vec![
        vec![first.clone(), second.clone()],
        vec![second.clone(), first.clone()],
        vec![[first.clone(), second.clone()].concat()],
        vec![[second.clone(), first.clone()].concat()],
    ];
    let mut results = Vec::new();
    for (i, order) in orders.iter().enumerate() {
        let dev = Device::new(&format!("sync-converge-{i}"));
        dev.clock.set_ms(y.clock.now_unix_ms() + 1_000);
        for page in order {
            apply_page(&dev, page);
        }
        // Rows held for a missing parent are tried again, as the next tick does.
        apply_page(&dev, &[first.clone(), second.clone()].concat());
        let rows = converged_rows(&dev, m);
        let db = dev.svc.store();
        // One live "Me": the earliest-created; one live summary: the latest-created.
        let live_mes: Vec<_> = db
            .persons()
            .unwrap()
            .into_iter()
            .filter(|p| p.is_me && p.is_live())
            .collect();
        assert_eq!(live_mes.len(), 1, "order {i}: {rows:?}");
        assert_eq!(live_mes[0].id, earliest_me, "order {i}");
        let live_summaries: Vec<_> = db
            .summaries(m)
            .unwrap()
            .into_iter()
            .filter(|s| !s.meta.deleted)
            .collect();
        assert_eq!(live_summaries.len(), 1, "order {i}: {rows:?}");
        assert_eq!(live_summaries[0].id, latest_summary, "order {i}");
        // The replaced summary keeps its body: it is the undo.
        assert!(
            db.summaries(m)
                .unwrap()
                .iter()
                .any(|s| s.meta.deleted && s.summary.title == "Written on device 1"),
            "order {i}"
        );
        drop(db);
        results.push(rows);
    }
    for (i, r) in results.iter().enumerate().skip(1) {
        assert_eq!(r, &results[0], "order {i} reached other rows than order 0");
    }
}

#[test]
fn sync_converge_repairs_travel_and_settle_through_the_relay() {
    // Through the real relay: each device makes its own "Me" and summary after pairing.
    let relay = relay();
    let (a, b) = pair(&relay);
    let m = meeting(&a.svc, "Shared", 22);
    let (ca, cb) = (a.client(), b.client());
    ca.tick().unwrap();
    cb.tick().unwrap();
    for (dev, seed) in [(&a, 3), (&b, 4)] {
        dev.clock.advance_ms(1_000 * seed);
        let rows = me_and_summary(dev, m, seed);
        let mut db = dev.svc.store();
        for r in rows {
            match r {
                earshot_store::SyncedRow::Person(p) => db.put_person(p).unwrap(),
                earshot_store::SyncedRow::Summary(s) => db.put_summary(s).unwrap(),
                _ => unreachable!(),
            }
        }
    }
    let mut repaired = 0;
    for _ in 0..3 {
        repaired += ca.tick().unwrap().repaired;
        repaired += cb.tick().unwrap().repaired;
    }
    assert!(
        repaired >= 2,
        "a Me merge and a summary replacement were made: {repaired}"
    );
    // The repaired state, the same on both: one live Me and one live summary.
    for dev in [&a, &b] {
        let db = dev.svc.store();
        let mes: Vec<_> = db
            .persons()
            .unwrap()
            .into_iter()
            .filter(|p| p.is_me && p.is_live())
            .map(|p| p.id)
            .collect();
        assert_eq!(mes.len(), 1, "{mes:?}");
        let live = db
            .summaries(m)
            .unwrap()
            .into_iter()
            .filter(|s| !s.meta.deleted)
            .count();
        assert_eq!(live, 1);
    }
    assert_eq!(converged_rows(&a, m), converged_rows(&b, m));
    assert_eq!(a.rows(), b.rows(), "stamps settle too");
    // Settled: neither device has anything left to push, write or repair.
    for c in [&ca, &cb] {
        let quiet = c.tick().unwrap();
        assert_eq!(
            (quiet.pushed, quiet.written, quiet.repaired),
            (0, 0, 0),
            "{quiet:?}"
        );
    }
}

#[test]
fn sync_converge_keeps_the_users_name_when_the_named_me_is_merged_away() {
    use earshot_store::SyncedRow;
    let x = Device::new("sync-converge-name-x");
    let y = Device::new("sync-converge-name-y");
    let m = meeting(&x.svc, "Any", 23);
    // The earlier "Me" was never named; the later one was.
    let mut first = me_and_summary(&x, m, 5);
    first.truncate(1);
    y.clock.advance_ms(10_000);
    let mut second = me_and_summary(&y, m, 6);
    second.truncate(1);
    if let SyncedRow::Person(p) = &mut second[0] {
        p.display_name = "Casey".into();
    }
    let dev = Device::new("sync-converge-name");
    dev.clock.advance_ms(20_000);
    let applied = apply_page(&dev, &[first.clone(), second].concat());
    assert_eq!(applied.mes_merged, 1);
    let db = dev.svc.store();
    let me = db
        .persons()
        .unwrap()
        .into_iter()
        .find(|p| p.is_me && p.is_live())
        .unwrap();
    assert_eq!(me.id, first[0].id(), "the earliest Me stays");
    assert_eq!(me.display_name, "Casey", "and takes the name its user set");
}

// T11.3c: presence (docs/sync-spec.md section 8).

#[test]
fn presence_online_is_a_recent_blob_that_does_not_say_gone() {
    assert!(online(0, false));
    assert!(online(ONLINE_WITHIN_S, false));
    assert!(!online(ONLINE_WITHIN_S + 1, false));
    assert!(!online(0, true), "a device that said gone is not online");
    assert_eq!(ONLINE_WITHIN_S, 150);
    assert_eq!(PRESENCE_EVERY, Duration::from_secs(60));
}

#[test]
fn presence_names_are_cleaned_and_bounded() {
    assert_eq!(clean_name("  Study PC  ").as_deref(), Some("Study PC"));
    assert_eq!(clean_name("Desk\u{7}top").as_deref(), Some("Desktop"));
    assert_eq!(clean_name("   "), None);
    let long = "x".repeat(MAX_DEVICE_NAME + 10);
    assert_eq!(clean_name(&long).unwrap().chars().count(), MAX_DEVICE_NAME);
    assert!(!computer_name().is_empty());
}

#[test]
fn presence_round_trip_names_each_device_and_says_who_is_online() {
    let relay = relay();
    let (a, b) = pair(&relay);
    set_device_name(&a.svc, "Study desktop").unwrap();
    set_offer_summaries(&a.svc, true).unwrap();
    let live = meeting(&a.svc, "Recording now", 30);
    let lease = a
        .svc
        .lease_meeting_for(live, MeetingJob::Recording)
        .unwrap();
    let ca = a.client();
    // Before A says anything, B lists it by its short id only.
    let before = vault_devices(&b.svc, b.vault()).unwrap();
    let other = before.iter().find(|d| !d.this_device).unwrap();
    assert_eq!(other.name, None);
    assert!(!other.online);
    ca.send_presence(false).unwrap();
    let listed = vault_devices(&b.svc, b.vault()).unwrap();
    let other = listed.iter().find(|d| !d.this_device).unwrap();
    assert_eq!(other.name.as_deref(), Some("Study desktop"));
    assert!(
        other.online && other.recording && other.offers_summaries,
        "{other:?}"
    );
    // Going away: gone at once, without waiting out the online window.
    drop(lease);
    ca.send_presence(true).unwrap();
    let after = vault_devices(&b.svc, b.vault()).unwrap();
    let other = after.iter().find(|d| !d.this_device).unwrap();
    assert!(!other.online && !other.recording, "{other:?}");
    assert_eq!(other.name.as_deref(), Some("Study desktop"));
    // An empty name goes back to the computer's name.
    set_device_name(&a.svc, "  ").unwrap();
    assert_eq!(
        sync_settings(&a.svc.store()).unwrap().device_name,
        computer_name()
    );
}

/// T11.6: a summary worker's presence says kind `server` and names the writer it was given,
/// not its Summaries settings; the app's says `app`. Its client may name its relay itself.
#[test]
fn presence_a_summary_worker_says_server_and_its_own_writer() {
    use crate::offload::WriterKind;
    let relay = relay();
    let (a, b) = pair(&relay);
    set_device_name(&b.svc, "Invented server").unwrap();
    set_offer_summaries(&b.svc, true).unwrap();
    // Where the worker's model is, as its locator would say now.
    let local = Arc::new(std::sync::atomic::AtomicBool::new(true));
    let now = Arc::clone(&local);
    let asked = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counted = Arc::clone(&asked);
    let worker = SyncClient::for_address(b.svc.clone(), b.vault(), &relay.relay.url())
        .unwrap()
        .as_summary_worker(Arc::new(move || {
            counted.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Some(if now.load(std::sync::atomic::Ordering::SeqCst) {
                WriterKind::Local
            } else {
                WriterKind::OpenaiCompatible
            })
        }))
        .with_model_place(Some(ModelPlace::GraphicsCard));
    worker.send_presence(false).unwrap();
    a.client().send_presence(false).unwrap();
    let peers = a.client().peers().unwrap();
    let theirs = peers
        .iter()
        .find(|p| p.device == b.svc.store().device())
        .unwrap();
    let p = theirs.presence.as_ref().unwrap();
    assert_eq!(p.kind, KIND_SERVER);
    assert_eq!(p.writer_kind.as_deref(), Some("local"));
    // T11.6a: with its own local model it says where that model runs.
    assert_eq!(p.model_place.as_deref(), Some("graphics-card"));
    assert!(
        theirs.online && p.offers_summaries && !p.recording,
        "{theirs:?}"
    );
    let mine = peers
        .iter()
        .find(|p| p.device == a.svc.store().device())
        .unwrap();
    assert_eq!(mine.presence.as_ref().unwrap().kind, KIND_APP);
    assert_eq!(mine.presence.as_ref().unwrap().model_place, None);
    // The Sync page lists it by its name, online.
    let listed = vault_devices(&a.svc, a.vault()).unwrap();
    let other = listed.iter().find(|d| !d.this_device).unwrap();
    assert_eq!(other.name.as_deref(), Some("Invented server"));
    assert!(other.online && other.offers_summaries);
    assert_eq!(other.writer_kind, Some(WriterKind::Local));
    assert_eq!(other.model_place, Some(ModelPlace::GraphicsCard));
    let me = listed.iter().find(|d| d.this_device).unwrap();
    assert_eq!(me.model_place, None);
    // The model moves (its container was not up yet, or is gone): the next presence says so.
    local.store(false, std::sync::atomic::Ordering::SeqCst);
    worker.send_presence(false).unwrap();
    let peers = a.client().peers().unwrap();
    let theirs = peers
        .iter()
        .find(|p| p.device == b.svc.store().device())
        .unwrap();
    assert_eq!(
        theirs.presence.as_ref().unwrap().writer_kind.as_deref(),
        Some("openai_compatible")
    );
    // A remote endpoint is never labelled with the profile's processor or graphics card.
    assert_eq!(theirs.presence.as_ref().unwrap().model_place, None);
    let listed = vault_devices(&a.svc, a.vault()).unwrap();
    assert_eq!(
        listed.iter().find(|d| !d.this_device).unwrap().model_place,
        None
    );
    local.store(true, std::sync::atomic::Ordering::SeqCst);
    worker.send_presence(false).unwrap();
    let peers = a.client().peers().unwrap();
    let theirs = peers
        .iter()
        .find(|p| p.device == b.svc.store().device())
        .unwrap();
    assert_eq!(
        theirs.presence.as_ref().unwrap().writer_kind.as_deref(),
        Some("local")
    );
    assert_eq!(
        theirs.presence.as_ref().unwrap().model_place.as_deref(),
        Some("graphics-card")
    );
    // Not offering: no writer is named, whatever it was given.
    set_offer_summaries(&b.svc, false).unwrap();
    worker.send_presence(false).unwrap();
    let peers = a.client().peers().unwrap();
    let theirs = peers
        .iter()
        .find(|p| p.device == b.svc.store().device())
        .unwrap();
    assert_eq!(theirs.presence.as_ref().unwrap().writer_kind, None);
    assert_eq!(theirs.presence.as_ref().unwrap().model_place, None);
    // A gone blob does not ask the locator (it may look a name up) and names no writer.
    let before = asked.load(std::sync::atomic::Ordering::SeqCst);
    set_offer_summaries(&b.svc, true).unwrap();
    worker.send_presence(true).unwrap();
    assert_eq!(asked.load(std::sync::atomic::Ordering::SeqCst), before);
    let peers = a.client().peers().unwrap();
    let theirs = peers
        .iter()
        .find(|p| p.device == b.svc.store().device())
        .unwrap();
    let p = theirs.presence.as_ref().unwrap();
    assert!(
        p.gone && p.writer_kind.is_none() && p.model_place.is_none() && p.kind == KIND_SERVER,
        "{p:?}"
    );
    // A refused address is refused as a stored one would be.
    assert_eq!(
        SyncClient::for_address(b.svc.clone(), b.vault(), "http://relay.example.com").err(),
        Some(SyncProblem::Address)
    );
}

#[test]
fn presence_from_another_vault_is_listed_without_a_name() {
    // A blob sealed under another key cannot be opened: no name, not online.
    let relay = relay();
    let (a, b) = pair(&relay);
    let secrets = a.vault.load().unwrap().unwrap();
    let stranger = earshot_sync::VaultKey::generate().unwrap();
    let blob = earshot_sync::seal_presence(
        &stranger.keys(),
        a.svc.store().device(),
        br#"{"v":1,"name":"Not readable","app_version":"0","kind":"app","offers_summaries":false,"writer_kind":null,"recording":false}"#,
    )
    .unwrap();
    a.client()
        .relay()
        .put_presence(
            &secrets.token,
            &earshot_sync::wire::PresenceRequest {
                blob: earshot_sync::wire::B64(blob),
            },
        )
        .unwrap();
    let listed = vault_devices(&b.svc, b.vault()).unwrap();
    let other = listed.iter().find(|d| !d.this_device).unwrap();
    assert_eq!(other.name, None);
    assert!(!other.online);
}

#[test]
fn presence_the_thread_announces_itself_and_says_gone_when_stopped() {
    let relay = relay();
    let (a, b) = pair(&relay);
    set_device_name(&a.svc, "Laptop").unwrap();
    let timing = SyncTiming {
        tick: Duration::from_millis(100),
        poll: Duration::from_millis(10),
        presence: Duration::from_millis(50),
    };
    let thread = SyncThread::start(a.client(), timing).unwrap();
    let online_now = || {
        vault_devices(&b.svc, b.vault())
            .unwrap()
            .into_iter()
            .find(|d| !d.this_device)
            .is_some_and(|d| d.online && d.name.as_deref() == Some("Laptop"))
    };
    wait_for("the thread never sent its presence", online_now);
    thread.stop_saying_gone();
    assert!(!online_now(), "a thread stopped for good says gone");
    // A thread replaced by another stays quiet: its device is still online.
    let replaced = SyncThread::start(a.client(), timing).unwrap();
    wait_for("the second thread never sent its presence", online_now);
    replaced.stop();
    assert!(online_now(), "a replaced thread must not say gone");
}

// T11.3ca: gone on quit and on sleep.

#[test]
fn presence_beats_say_gone_once_and_online_again_only_after_an_announcement() {
    // Nothing announced yet: going away and coming back send nothing.
    let mut b = Beats::default();
    assert!(b.beat_due());
    assert!(!b.go_away(), "never seen online: nothing to take back");
    assert!(!b.beat_due(), "no beats while away");
    assert!(b.come_back(), "it had gone away, though nothing was said");
    assert!(b.beat_due());
    // Announced: gone once per going away; beats resume when back.
    b.sent_online();
    assert!(b.announced());
    assert!(b.go_away(), "seen online: say gone");
    assert!(!b.go_away(), "not twice for one going away");
    assert!(!b.beat_due());
    assert!(b.come_back(), "it had gone away");
    assert!(!b.come_back(), "and is back only once");
    assert!(b.beat_due());
}

#[test]
fn presence_the_thread_says_gone_when_going_away_and_online_when_back() {
    let relay = relay();
    let (a, b) = pair(&relay);
    set_device_name(&a.svc, "Laptop").unwrap();
    let timing = SyncTiming {
        tick: Duration::from_millis(100),
        poll: Duration::from_millis(10),
        presence: Duration::from_millis(50),
    };
    let online_now = || {
        vault_devices(&b.svc, b.vault())
            .unwrap()
            .into_iter()
            .find(|d| !d.this_device)
            .is_some_and(|d| d.online && d.name.as_deref() == Some("Laptop"))
    };
    let thread = SyncThread::start(a.client(), timing).unwrap();
    wait_for("the thread never sent its presence", online_now);
    let sent = thread.say_gone().expect("the gone thread starts");
    sent.recv_timeout(Duration::from_secs(10))
        .expect("the gone send never finished");
    assert!(!online_now(), "a device going away says gone at once");
    assert!(thread.say_gone().is_none(), "not twice for one going away");
    // Several beat intervals later it still reads gone: the beats stopped.
    std::thread::sleep(timing.presence * 6);
    assert!(!online_now(), "no beat while away");
    thread.back();
    wait_for("the thread never said it was back", online_now);
    thread.stop();
}

#[test]
fn presence_back_beats_after_the_first_tick_not_the_next_interval() {
    // A beat interval far longer than the test: only `back` can bring the device online again
    // in time.
    let relay = relay();
    let (a, b) = pair(&relay);
    set_device_name(&a.svc, "Laptop").unwrap();
    let timing = SyncTiming {
        tick: Duration::from_millis(100),
        poll: Duration::from_millis(10),
        presence: Duration::from_secs(3600),
    };
    let online_now = || {
        vault_devices(&b.svc, b.vault())
            .unwrap()
            .into_iter()
            .find(|d| !d.this_device)
            .is_some_and(|d| d.online)
    };
    let thread = SyncThread::start(a.client(), timing).unwrap();
    wait_for("the first beat never came", online_now);
    thread
        .say_gone()
        .expect("the gone thread starts")
        .recv_timeout(Duration::from_secs(10))
        .expect("the gone send never finished");
    assert!(!online_now());
    thread.back();
    wait_for("back never beat before the next interval", online_now);
    thread.stop();
}

#[test]
fn presence_blobs_are_one_size_whatever_they_say() {
    use crate::offload::WriterKind;
    let key = earshot_sync::VaultKey::generate().unwrap();
    let keys = key.keys();
    let device = earshot_core::identity::DeviceId::from_bytes([7; 16]);
    let widest = "\u{1F600}".repeat(MAX_DEVICE_NAME);
    let mut sizes = std::collections::BTreeSet::new();
    let longest = WriterKind::OpenaiCompatible;
    let longest_place = ModelPlace::ALL
        .into_iter()
        .max_by_key(|p| p.word().len())
        .unwrap();
    for (kind, name, recording, gone, writer, place) in [
        (presence::KIND_APP, "A", false, false, None, None),
        (
            presence::KIND_APP,
            "A",
            true,
            false,
            Some(WriterKind::Local),
            None,
        ),
        (
            presence::KIND_APP,
            "A much longer computer name here",
            true,
            true,
            Some(longest),
            None,
        ),
        (
            presence::KIND_APP,
            widest.as_str(),
            false,
            false,
            Some(longest),
            None,
        ),
        // T11.6a: the longest blob there can be, a worker's with every field at its longest
        // (the place is sent only with `local`, but the size rule must hold for any blob).
        (
            presence::KIND_SERVER,
            widest.as_str(),
            true,
            true,
            Some(longest),
            Some(longest_place),
        ),
        (
            presence::KIND_SERVER,
            "Invented server",
            false,
            false,
            Some(WriterKind::Local),
            Some(ModelPlace::Processor),
        ),
    ] {
        let p = presence::presence_now(kind, name.to_owned(), true, writer, place, recording, gone);
        let blob = presence::blob_of(&p, &keys, device).unwrap();
        sizes.insert(blob.len());
        let back = presence::open_blob(&keys, device, &blob).unwrap();
        assert_eq!((back.recording, back.gone), (recording, gone));
        assert_eq!(back.name, clean_name(name).unwrap());
        assert_eq!(back.model_place.as_deref(), place.map(ModelPlace::word));
    }
    assert_eq!(sizes.len(), 1, "every blob is one size: {sizes:?}");
}

/// T11.6a: an app's presence holds no `model_place` key at all (the same bytes as before the
/// field existed), and a reader built before the field existed reads a worker's blob that has
/// one: presence has no `deny_unknown_fields`.
#[test]
fn presence_model_place_is_left_out_for_the_app_and_ignored_by_older_readers() {
    use crate::offload::WriterKind;
    // The presence struct as it was before T11.6a (no `model_place`).
    #[derive(serde::Deserialize)]
    #[allow(dead_code)]
    struct Older {
        v: u32,
        name: String,
        app_version: String,
        kind: String,
        offers_summaries: bool,
        writer_kind: Option<String>,
        recording: bool,
        #[serde(default)]
        gone: bool,
        #[serde(default)]
        pad: String,
    }
    let app = presence::presence_now(
        presence::KIND_APP,
        "Invented desk".to_owned(),
        true,
        Some(WriterKind::Local),
        None,
        false,
        false,
    );
    let json = serde_json::to_string(&app).unwrap();
    assert_eq!(
        json,
        format!(
            r#"{{"v":1,"name":"Invented desk","app_version":"{}","kind":"app","offers_summaries":true,"writer_kind":"local","recording":false,"gone":false,"pad":""}}"#,
            env!("CARGO_PKG_VERSION")
        )
    );
    let worker = presence::presence_now(
        presence::KIND_SERVER,
        "Invented server".to_owned(),
        true,
        Some(WriterKind::Local),
        Some(ModelPlace::GraphicsCard),
        false,
        false,
    );
    let json = serde_json::to_vec(&worker).unwrap();
    assert!(String::from_utf8_lossy(&json).contains(r#""model_place":"graphics-card""#));
    let older: Older = serde_json::from_slice(&json).unwrap();
    assert_eq!(
        (older.kind.as_str(), older.writer_kind.as_deref()),
        (presence::KIND_SERVER, Some("local"))
    );
    // Every word round-trips; anything else is no place.
    for p in ModelPlace::ALL {
        assert_eq!(ModelPlace::parse(p.word()), Some(p));
    }
    for other in ["", "gpu", "cpu", "Processor", "graphics card", "npu"] {
        assert_eq!(ModelPlace::parse(other), None, "{other}");
    }
}

#[test]
fn sync_waiting_leaves_out_voiceprints_while_their_switch_is_off() {
    use earshot_core::voiceprint::{Person, Voiceprint};
    use rand_core::SeedableRng;
    let relay = relay();
    let (a, _b) = pair(&relay);
    {
        let clock = Arc::clone(&a.svc.clock);
        let mut rng = rand_chacha::ChaCha8Rng::seed_from_u64(31);
        let mut db = a.svc.store();
        let (store, hlc) = db.parts();
        let mut stamp = earshot_core::model::Stamp {
            clock: &*clock,
            hlc,
            rng: &mut rng,
        };
        let p = Person::new("Someone invented", false, &mut stamp);
        let vid = earshot_core::identity::EntityId::generate(stamp.clock, &mut *stamp.rng);
        let meta = earshot_core::identity::SyncMeta::new(stamp.hlc, stamp.clock);
        let v = Voiceprint {
            id: vid,
            meta,
            person_id: p.id,
            model: earshot_core::diar::EmbeddingModelId::new("invented-model"),
            sum: vec![0.5; 4],
            count: 1,
            speech_ms: 5_000,
        };
        store.put_person(p).unwrap();
        store.put_voiceprint(v).unwrap();
    }
    a.client().tick().unwrap();
    let view = sync_overview(&a.svc, &*a.vault).unwrap();
    assert!(!view.settings.voiceprints);
    assert_eq!(
        view.waiting, 0,
        "a switched-off voiceprint is not waiting to send"
    );
    assert!(
        a.svc.store().outbox_len().unwrap() > 0,
        "it stays queued for the switch"
    );
}

/// A row of an entity this build does not know (as `summary_request` is to Earshot 0.3.7, which
/// syncs at the same envelope format) is held as `format` by the client, never dropped or
/// applied, and opens again after an update (T11.3da kept the format at 1).
#[test]
fn sync_a_row_of_an_entity_this_build_does_not_know_is_held() {
    use earshot_core::identity::{
        Clock, EntityId, HlcTimestamp, SyncClass, SyncMeta, SyncSerialize, SyncableClass,
    };
    #[derive(serde::Serialize)]
    struct FromANewerBuild {
        id: EntityId,
        meta: SyncMeta,
        anything: String,
    }
    impl SyncClass for FromANewerBuild {
        type Class = SyncableClass;
    }
    impl SyncSerialize for FromANewerBuild {
        const ENTITY: &'static str = "an_entity_from_a_newer_build";
        fn sync_id(&self) -> EntityId {
            self.id
        }
        fn sync_meta(&self) -> &SyncMeta {
            &self.meta
        }
    }
    let node = earshot_core::identity::DeviceId::from_bytes([9; 16]);
    let stamp = HlcTimestamp {
        wall_ms: crate::testutil::T0,
        logical: 0,
        node,
    };
    let row = FromANewerBuild {
        id: EntityId::from_bytes([0x01, 0x90, 0, 0, 0, 0, 0x70, 0, 0x80, 0, 0, 0, 0, 0, 1, 9]),
        meta: SyncMeta {
            created_at: stamp,
            updated_at: stamp,
            deleted: false,
            origin_device_id: node,
        },
        anything: "new".into(),
    };
    let keys = earshot_sync::VaultKey::from_bytes([3; 32]).keys();
    let sealed = earshot_sync::seal_row(&keys, &row).unwrap();
    let inc = engine::open(&keys, 1, &sealed.key.0, sealed.stamp, sealed.body).unwrap();
    assert!(inc.row.is_none(), "not decoded: held for a newer build");
    let dir = TempDir::new("sync-unknown-entity");
    let (svc, clock) = services(&dir);
    let mut db = svc.store();
    let (store, hlc) = db.parts();
    let free = |_: EntityId| false;
    let rules = earshot_store::ApplyRules {
        kinds: earshot_store::SyncKinds {
            voiceprints: false,
            dictionary: true,
        },
        leased: &free,
        now_ms: clock.now_unix_ms(),
        after: None,
    };
    let applied = store.apply_remote(&[inc], &rules, hlc, &*clock).unwrap();
    assert_eq!(applied.held[0], 1, "{applied:?}");
    assert_eq!(
        store.held_rows().unwrap()[0].reason,
        earshot_store::HoldReason::Format
    );
}

// ---- Dictionary sync (T11.3e, docs/sync.md section 9). Every name holds `dictionary_sync`;
// every word is invented. ----

/// The words a device's summaries read now.
fn words(dev: &Device) -> Vec<String> {
    crate::vocabulary::vocabulary(&dev.svc.store())
        .unwrap()
        .0
        .words()
        .to_vec()
}

/// The live word rows a device holds, in creation order.
fn live_rows(dev: &Device) -> Vec<String> {
    use earshot_core::model::DictionaryRepo;
    dev.svc
        .store()
        .dictionary_words()
        .unwrap()
        .into_iter()
        .filter(|w| w.is_live())
        .map(|w| w.word)
        .collect()
}

fn not_in_use(dev: &Device) -> usize {
    crate::vocabulary::dictionary_state(&dev.svc.store())
        .unwrap()
        .not_in_use
}

/// Ticks `a`, `b`, then `a` again, so each has the other's changes.
fn exchange(a: &Device, b: &Device) {
    a.client().tick().unwrap();
    b.client().tick().unwrap();
    a.client().tick().unwrap();
}

#[test]
fn dictionary_sync_is_on_by_default_once_sync_is_on() {
    let relay = relay();
    let (a, _b) = pair(&relay);
    let settings = sync_settings(&a.svc.store()).unwrap();
    assert!(settings.dictionary && !settings.voiceprints);
    assert!(crate::vocabulary::dictionary_syncs(&a.svc.store()).unwrap());
    // A device that never set sync up keeps its own list.
    let alone = Device::new("sync-dictionary-alone");
    assert!(!crate::vocabulary::dictionary_syncs(&alone.svc.store()).unwrap());
}

#[test]
fn dictionary_sync_two_devices_adding_different_words_keep_both() {
    let relay = relay();
    let (a, b) = pair(&relay);
    crate::vocabulary::add_word(&a.svc, "Quillmarsh").unwrap();
    b.clock.advance_ms(5);
    crate::vocabulary::add_word(&b.svc, "Velmora").unwrap();
    exchange(&a, &b);
    assert_eq!(words(&a), ["Quillmarsh", "Velmora"], "creation order");
    assert_eq!(words(&b), words(&a));
    assert_eq!((not_in_use(&a), not_in_use(&b)), (0, 0));
    let ca = a.client();
    ca.tick().unwrap();
    assert_only_the_relay(&ca, &relay);
}

#[test]
fn dictionary_sync_a_removal_on_one_device_reaches_the_other() {
    let relay = relay();
    let (a, b) = pair(&relay);
    crate::testutil::replace_words(&a.svc, &["Quillmarsh", "QXR-7"]).unwrap();
    exchange(&a, &b);
    assert_eq!(words(&b), ["Quillmarsh", "QXR-7"]);
    b.clock.advance_ms(10);
    assert!(crate::vocabulary::remove_word(&b.svc, "quillmarsh").unwrap());
    exchange(&b, &a);
    assert_eq!(words(&a), ["QXR-7"]);
    assert_eq!(words(&b), ["QXR-7"]);
    // The removal is a tombstone with its word cleared: the word is gone from every row.
    use earshot_core::model::DictionaryRepo;
    let rows = a.svc.store().dictionary_words().unwrap();
    assert_eq!(rows.len(), 2);
    assert!(rows.iter().all(|r| r.is_live() || r.word.is_empty()));
}

#[test]
fn dictionary_sync_the_same_word_twice_counts_once_with_the_earliest_spelling() {
    let relay = relay();
    let (a, b) = pair(&relay);
    crate::vocabulary::add_word(&a.svc, "quillmarsh").unwrap();
    b.clock.advance_ms(50);
    crate::vocabulary::add_word(&b.svc, "QuillMarsh").unwrap();
    exchange(&a, &b);
    assert_eq!(words(&a), ["quillmarsh"]);
    assert_eq!(words(&b), ["quillmarsh"]);
    // Both rows stay stored; the repeat is neither in use nor counted as not in use.
    assert_eq!(live_rows(&b).len(), 2);
    assert_eq!(not_in_use(&b), 0);
    // Removing the word removes every spelling of it, everywhere.
    assert!(crate::vocabulary::remove_word(&b.svc, "QUILLMARSH").unwrap());
    exchange(&b, &a);
    assert!(words(&a).is_empty() && live_rows(&a).is_empty());
}

#[test]
fn dictionary_sync_a_joined_list_past_the_limits_keeps_every_word_and_counts_the_rest() {
    let relay = relay();
    let (a, b) = pair(&relay);
    let mine: Vec<String> = (0..60).map(|i| format!("alpha{i:02}")).collect();
    let theirs: Vec<String> = (0..60).map(|i| format!("beta{i:02}")).collect();
    crate::testutil::replace_words(&a.svc, &mine).unwrap();
    b.clock.advance_ms(100);
    crate::testutil::replace_words(&b.svc, &theirs).unwrap();
    exchange(&a, &b);
    let mut want = mine.clone();
    want.extend(theirs[..40].iter().cloned());
    for dev in [&a, &b] {
        assert_eq!(words(dev), want, "taken in creation order up to 100 words");
        assert_eq!(not_in_use(dev), 20);
        assert_eq!(live_rows(dev).len(), 120, "nothing deleted to make room");
    }
    // Removing a word in use lets the next stored word in.
    assert!(crate::vocabulary::remove_word(&a.svc, "alpha00").unwrap());
    assert_eq!(not_in_use(&a), 19);
    assert!(words(&a).contains(&"beta40".to_owned()));
    exchange(&a, &b);
    assert_eq!(words(&b), words(&a));
    assert_eq!(live_rows(&b).len(), 119);
}

#[test]
fn dictionary_sync_off_keeps_this_devices_list_and_neither_sends_nor_applies() {
    let relay = relay();
    let (a, b) = pair(&relay);
    set_sync_dictionary(&b.svc, false).unwrap();
    assert!(!sync_settings(&b.svc.store()).unwrap().dictionary);
    crate::vocabulary::add_word(&a.svc, "Quillmarsh").unwrap();
    b.clock.advance_ms(5);
    crate::vocabulary::add_word(&b.svc, "Velmora").unwrap();
    exchange(&a, &b);
    assert_eq!(words(&a), ["Quillmarsh"], "b sent nothing");
    assert_eq!(words(&b), ["Velmora"], "b applied nothing");
    // b's row waits in its outbox, and is not counted as behind.
    let overview = sync_overview(&b.svc, &*b.vault).unwrap();
    assert_eq!(overview.waiting, 0);
    assert!(b.svc.store().outbox_len().unwrap() > 0);

    // Turning it on reads every earlier row and sends b's own.
    set_sync_dictionary(&b.svc, true).unwrap();
    assert_eq!(pull_cursor(&b.svc.store()).unwrap(), 0);
    exchange(&b, &a);
    b.client().tick().unwrap();
    assert_eq!(words(&a), ["Quillmarsh", "Velmora"]);
    assert_eq!(words(&b), ["Quillmarsh", "Velmora"]);
}

#[test]
fn dictionary_sync_words_stored_before_rows_existed_get_rows_and_are_kept() {
    let relay = relay();
    let (a, b) = pair(&relay);
    // As an older Earshot stored them: the list alone, no rows.
    local::set(
        &mut a.svc.store(),
        &DeviceRecord::Vocabulary.key(),
        br#"["Ostrelby","QXR-7"]"#,
    )
    .unwrap();
    assert!(live_rows(&a).is_empty());
    exchange(&a, &b);
    assert_eq!(live_rows(&a), ["Ostrelby", "QXR-7"]);
    assert_eq!(words(&a), ["Ostrelby", "QXR-7"]);
    assert_eq!(words(&b), ["Ostrelby", "QXR-7"]);
}

#[test]
fn dictionary_sync_turning_it_on_makes_rows_for_the_words_already_here() {
    let relay = relay();
    let (a, _b) = pair(&relay);
    set_sync_dictionary(&a.svc, false).unwrap();
    local::set(
        &mut a.svc.store(),
        &DeviceRecord::Vocabulary.key(),
        br#"["Velmora"]"#,
    )
    .unwrap();
    assert!(live_rows(&a).is_empty());
    set_sync_dictionary(&a.svc, true).unwrap();
    assert_eq!(live_rows(&a), ["Velmora"]);
    // Already on: nothing more is made and the cursor is left alone.
    a.client().tick().unwrap();
    let at = pull_cursor(&a.svc.store()).unwrap();
    set_sync_dictionary(&a.svc, true).unwrap();
    assert_eq!(pull_cursor(&a.svc.store()).unwrap(), at);
    assert_eq!(live_rows(&a), ["Velmora"]);
}

#[test]
fn dictionary_sync_words_reach_the_relay_only_sealed() {
    let relay = relay();
    let (a, _b) = pair(&relay);
    crate::vocabulary::add_word(&a.svc, "Zentravelk").unwrap();
    let ca = a.client();
    ca.tick().unwrap();
    // Nothing the relay stored holds the word in the clear.
    let mut stack = vec![relay._dir.0.clone()];
    let mut files = 0;
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                stack.push(path);
            } else {
                files += 1;
                let bytes = std::fs::read(&path).unwrap();
                assert!(
                    !bytes.windows(10).any(|w| w == b"Zentravelk"),
                    "a relay file holds the word"
                );
            }
        }
    }
    assert!(files > 0);
    assert_only_the_relay(&ca, &relay);
}

/// The stored `settings/vocabulary` bytes of a device.
fn stored_list(dev: &Device) -> Option<Vec<u8>> {
    dev.svc
        .store()
        .local_value(crate::vocabulary::VOCABULARY_KEY)
        .unwrap()
}

#[test]
fn dictionary_sync_a_stale_page_cannot_delete_a_word_another_device_added() {
    let relay = relay();
    let (a, b) = pair(&relay);
    // b's page read its list (empty) before a's word arrived.
    let seen = words(&b);
    crate::vocabulary::add_word(&a.svc, "Velmora").unwrap();
    exchange(&a, &b);
    assert_eq!(words(&b), ["Velmora"]);
    // A whole-list save from that page is refused; nothing is written.
    let err = crate::vocabulary::set_vocabulary(&b.svc, &seen, &["Quillmarsh"]).unwrap_err();
    assert!(matches!(err, crate::vocabulary::SetVocabularyError::Stale));
    assert_eq!(err.to_string(), crate::vocabulary::STALE_LIST);
    // The page edits one word at a time instead: adding keeps the word that arrived.
    b.clock.advance_ms(5);
    crate::vocabulary::add_entry(&b.svc, "Quillmarsh").unwrap();
    exchange(&b, &a);
    assert_eq!(words(&a), ["Velmora", "Quillmarsh"]);
    assert_eq!(words(&b), ["Velmora", "Quillmarsh"]);
}

#[test]
fn dictionary_sync_an_apply_between_read_and_write_is_never_a_removal() {
    let relay = relay();
    let (a, b) = pair(&relay);
    crate::vocabulary::add_word(&b.svc, "Quillmarsh").unwrap();
    exchange(&b, &a);
    // b's page shows its list; meanwhile a's word arrives and is applied on b.
    let page = words(&b);
    assert_eq!(page, ["Quillmarsh"]);
    a.clock.advance_ms(5);
    crate::vocabulary::add_word(&a.svc, "Velmora").unwrap();
    exchange(&a, &b);
    assert_eq!(words(&b), ["Quillmarsh", "Velmora"]);
    // The page's Delete names one word: the word applied in between stays, everywhere.
    assert!(crate::vocabulary::remove_entry(&b.svc, &page[0]).unwrap());
    assert_eq!(words(&b), ["Velmora"]);
    exchange(&b, &a);
    assert_eq!(words(&a), ["Velmora"]);
    assert_eq!(live_rows(&a), ["Velmora"]);
}

#[test]
fn dictionary_sync_add_is_refused_while_words_are_not_in_use() {
    let relay = relay();
    let (a, b) = pair(&relay);
    let mine: Vec<String> = (0..60).map(|i| format!("alpha{i:02}")).collect();
    let theirs: Vec<String> = (0..60).map(|i| format!("beta{i:02}")).collect();
    crate::testutil::replace_words(&a.svc, &mine).unwrap();
    b.clock.advance_ms(100);
    crate::testutil::replace_words(&b.svc, &theirs).unwrap();
    exchange(&a, &b);
    assert_eq!(not_in_use(&a), 20);
    let before = live_rows(&a);
    let err = crate::vocabulary::add_entry(&a.svc, "Quillmarsh").unwrap_err();
    assert!(matches!(
        err,
        crate::vocabulary::SetVocabularyError::NotInUse
    ));
    assert_eq!(err.to_string(), crate::vocabulary::NOT_IN_USE_REFUSAL);
    let err = crate::vocabulary::add_word(&a.svc, "Quillmarsh").unwrap_err();
    assert!(matches!(
        err,
        crate::vocabulary::SetVocabularyError::NotInUse
    ));
    assert_eq!(live_rows(&a), before, "nothing stored");
    // A word already in use is still reported as there, not refused.
    let there = crate::vocabulary::add_entry(&a.svc, "ALPHA00").unwrap();
    assert!(!there.added);
}

#[test]
fn dictionary_sync_a_list_this_version_refuses_is_kept_over_a_pulled_page() {
    let relay = relay();
    let (a, b) = pair(&relay);
    local::set(&mut a.svc.store(), &DeviceRecord::Vocabulary.key(), b"{}").unwrap();
    crate::vocabulary::add_word(&b.svc, "Velmora").unwrap();
    exchange(&b, &a);
    // The page still reports it by its reason; the join did not replace it.
    assert_eq!(stored_list(&a).as_deref(), Some(&b"{}"[..]));
    let state = crate::vocabulary::dictionary_state(&a.svc.store()).unwrap();
    assert!(state.unknown.is_some() && state.words.is_empty());
    // Turning the switch off and on keeps it too.
    set_sync_dictionary(&a.svc, false).unwrap();
    set_sync_dictionary(&a.svc, true).unwrap();
    assert_eq!(stored_list(&a).as_deref(), Some(&b"{}"[..]));
    // The user's own add replaces it with the joined list.
    crate::vocabulary::add_entry(&a.svc, "Quillmarsh").unwrap();
    assert_eq!(words(&a), ["Velmora", "Quillmarsh"]);
}

#[test]
fn dictionary_sync_switched_off_inside_the_apply_leaves_the_list_alone() {
    let relay = relay();
    let (a, _b) = pair(&relay);
    crate::vocabulary::add_word(&a.svc, "Velmora").unwrap();
    local::set(
        &mut a.svc.store(),
        &DeviceRecord::Vocabulary.key(),
        br#"["Ostrelby"]"#,
    )
    .unwrap();
    set_sync_dictionary(&a.svc, false).unwrap();
    // The hook the apply runs reads the switch inside the transaction.
    let rebuilt = a
        .svc
        .store()
        .transaction(|tx| crate::vocabulary::rebuild_tx(tx, crate::vocabulary::OverRefused::Keep))
        .unwrap();
    assert_eq!(rebuilt, None);
    assert_eq!(words(&a), ["Ostrelby"]);
}

#[test]
fn dictionary_sync_an_unchanged_rebuild_keeps_the_list_byte_for_byte() {
    // Live summary windows are reused only with exactly the words they were mapped with
    // (`live_summaries::reusable`): a tick that changes no row must leave the list as it was,
    // so a sync during a meeting maps nothing again unless the words themselves changed.
    let relay = relay();
    let (a, b) = pair(&relay);
    crate::vocabulary::add_word(&a.svc, "Velmora").unwrap();
    b.clock.advance_ms(5);
    crate::vocabulary::add_word(&b.svc, "Quillmarsh").unwrap();
    exchange(&a, &b);
    let before = stored_list(&b);
    exchange(&a, &b);
    b.client().tick().unwrap();
    assert_eq!(stored_list(&b), before);
}

#[test]
fn dictionary_sync_a_row_from_another_build_is_matched_by_its_normalised_word() {
    use earshot_core::model::{DictionaryRepo, DictionaryWord};
    let relay = relay();
    let (a, _b) = pair(&relay);
    // A row as another build might write it: quotes and spaces around the word.
    {
        let clock = Arc::clone(&a.svc.clock);
        let mut rng = crate::services::os_seeded_rng();
        let mut db = a.svc.store();
        let (store, hlc) = db.parts();
        let mut stamp = earshot_core::model::Stamp {
            clock: &*clock,
            hlc,
            rng: &mut *rng,
        };
        store
            .put_dictionary_word(DictionaryWord::new("  'zorbek' ", &mut stamp))
            .unwrap();
    }
    a.client().tick().unwrap();
    assert_eq!(words(&a), ["zorbek"]);
    // Adding it again finds it; removing it removes that row.
    assert!(
        !crate::vocabulary::add_entry(&a.svc, "Zorbek")
            .unwrap()
            .added
    );
    assert!(crate::vocabulary::remove_entry(&a.svc, "Zorbek").unwrap());
    assert!(live_rows(&a).is_empty());
    assert!(words(&a).is_empty());
}

#[test]
fn dictionary_sync_an_add_is_checked_against_the_rebuilt_list() {
    // A refused stored list is kept over pulls, so the stored list (empty) lags the rows: with
    // exactly 100 live rows the quick check passes and only the rebuilt list shows the new word
    // would not be in use. The add rolls back whole.
    let relay = relay();
    let (a, _b) = pair(&relay);
    let full: Vec<String> = (0..100).map(|i| format!("w{i:03}")).collect();
    crate::testutil::replace_words(&a.svc, &full).unwrap();
    local::set(&mut a.svc.store(), &DeviceRecord::Vocabulary.key(), b"{}").unwrap();
    assert_eq!(not_in_use(&a), 0);
    let outbox = a.svc.store().outbox_len().unwrap();
    let err = crate::vocabulary::add_entry(&a.svc, "Quillmarsh").unwrap_err();
    assert!(matches!(
        err,
        crate::vocabulary::SetVocabularyError::NotInUse
    ));
    assert_eq!(live_rows(&a).len(), 100, "no row stored");
    assert_eq!(
        stored_list(&a).as_deref(),
        Some(&b"{}"[..]),
        "nothing written"
    );
    assert_eq!(
        a.svc.store().outbox_len().unwrap(),
        outbox,
        "nothing to sync"
    );
    let err = crate::vocabulary::add_word(&a.svc, "Quillmarsh").unwrap_err();
    assert!(matches!(
        err,
        crate::vocabulary::SetVocabularyError::NotInUse
    ));
}

#[test]
fn dictionary_sync_turning_sync_on_again_brings_the_list_back_to_the_rows() {
    let relay = relay();
    let (a, b) = pair(&relay);
    let mine: Vec<String> = (0..60).map(|i| format!("alpha{i:02}")).collect();
    let theirs: Vec<String> = (0..60).map(|i| format!("beta{i:02}")).collect();
    crate::testutil::replace_words(&a.svc, &mine).unwrap();
    b.clock.advance_ms(100);
    crate::testutil::replace_words(&b.svc, &theirs).unwrap();
    exchange(&a, &b);
    assert_eq!(not_in_use(&a), 20);
    // With sync off the list is this computer's own: a deletion writes it directly.
    set_sync_on(&a.svc, false).unwrap();
    assert!(crate::vocabulary::remove_entry(&a.svc, "alpha00").unwrap());
    assert_eq!(words(&a).len(), 99);
    // Turning sync on again rebuilds the list from the rows, as the Dictionary switch does,
    // and an add is decided on that list.
    set_sync_on(&a.svc, true).unwrap();
    assert_eq!(words(&a).len(), 100);
    assert!(words(&a).contains(&"beta40".to_owned()));
    assert_eq!(not_in_use(&a), 19);
    let err = crate::vocabulary::add_entry(&a.svc, "Quillmarsh").unwrap_err();
    assert!(matches!(
        err,
        crate::vocabulary::SetVocabularyError::NotInUse
    ));
    assert_eq!(live_rows(&a).len(), 119);
}

#[test]
fn dictionary_sync_setting_up_makes_rows_and_rebuilds_the_list() {
    // Dictionary sync is on by default: setting up reconciles and rebuilds at once.
    let relay = relay();
    let a = Device::new("sync-dictionary-setup");
    local::set(
        &mut a.svc.store(),
        &DeviceRecord::Vocabulary.key(),
        br#"["Velmora","'Ostrelby'"]"#,
    )
    .unwrap();
    set_up_first_device(&a.svc, &*a.vault, &relay.relay.url(), SECRET).unwrap();
    assert_eq!(live_rows(&a), ["Velmora", "Ostrelby"]);
    assert_eq!(
        stored_list(&a).as_deref(),
        Some(&br#"["Velmora","Ostrelby"]"#[..])
    );
}

#[test]
fn dictionary_sync_an_edit_holds_the_store_lock_from_its_read_to_its_write() {
    // A sync apply runs only under the store lock (`SyncClient::apply`), so it cannot fall
    // between an edit's read of the stored list and its write while the lock is held: the seam
    // runs at that point and asks another thread whether it could take the lock.
    let relay = relay();
    let (a, _b) = pair(&relay);
    crate::vocabulary::add_entry(&a.svc, "Quillmarsh").unwrap();
    let probe = a.svc.clone();
    let seen: Arc<std::sync::Mutex<Vec<bool>>> = Arc::default();
    let record = Arc::clone(&seen);
    crate::vocabulary::BETWEEN_READ_AND_WRITE.with(|seam| {
        *seam.borrow_mut() = Some(Box::new(move || {
            let svc = probe.clone();
            let held = std::thread::spawn(move || svc.store_is_held())
                .join()
                .unwrap();
            record.lock().unwrap().push(held);
        }));
    });
    crate::vocabulary::add_entry(&a.svc, "Velmora").unwrap();
    assert!(crate::vocabulary::remove_entry(&a.svc, "Quillmarsh").unwrap());
    crate::vocabulary::BETWEEN_READ_AND_WRITE.with(|seam| *seam.borrow_mut() = None);
    assert_eq!(
        *seen.lock().unwrap(),
        [true, true],
        "the seam ran in both edits, lock held"
    );
    assert_eq!(words(&a), ["Velmora"]);
    // Once the edit has committed, the lock is free for an apply.
    assert!(!a.svc.store_is_held());
}
