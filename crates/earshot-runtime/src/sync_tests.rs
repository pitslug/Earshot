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
    assert_eq!(client.audit_log().dropped(), 0, "the whole record");
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

/// T14.26: a meeting whose rows here do not read is still deleted, and its tombstones reach the
/// other device, which erases its copy; nothing is left unsendable.
#[test]
fn sync_a_meeting_with_rows_that_do_not_read_is_deleted_on_the_other_device_too() {
    let relay = relay();
    let (a, b) = pair(&relay);
    let (ca, cb) = (a.client(), b.client());
    let m = meeting(&a.svc, "Damaged here", 6);
    ca.tick().unwrap();
    cb.tick().unwrap();
    assert!(b.title(m).is_some());
    let db = earshot_store::DataDir::new(a._dir.0.join("data")).db_path();
    {
        let c = rusqlite::Connection::open(&db).unwrap();
        let key = m.as_uuid().as_bytes().to_vec();
        c.execute(
            "UPDATE meeting SET started_unix_ms = -1 WHERE id = ?1",
            [&key],
        )
        .unwrap();
        c.execute(
            "UPDATE segment SET start_ms = -1 WHERE meeting_id = ?1",
            [&key],
        )
        .unwrap();
    }
    a.clock.advance_ms(1_000);
    a.svc.delete_meeting(m).unwrap();
    let r = ca.tick().unwrap();
    assert_eq!(r.unsendable, 0, "{r:?}");
    let r = cb.tick().unwrap();
    assert_eq!(r.deleted_meetings, 1, "{r:?}");
    assert_eq!(b.title(m), None);
    let row = b.svc.store().meeting(m).unwrap().unwrap();
    assert!(row.meta.deleted && row.title.is_empty());
    assert!(
        b.svc
            .store()
            .segments(m)
            .unwrap()
            .iter()
            .all(|s| s.meta.deleted && s.segment.text.is_empty())
    );
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
        unsendable: 0,
        refused: 0,
    };
    set_sync_status(&mut db, &status).unwrap();
    assert_eq!(
        db.local_value(SYNC_STATUS_KEY).unwrap().unwrap(),
        b"\x01\x00\x00\x00{\"last_success_unix_ms\":5,\"problem\":\"unreachable\",\"unreadable\":2}"
    );
    assert_eq!(sync_status(&db).unwrap(), status);
    let damaged = SyncStatus {
        unsendable: 3,
        ..status.clone()
    };
    set_sync_status(&mut db, &damaged).unwrap();
    assert_eq!(
        db.local_value(SYNC_STATUS_KEY).unwrap().unwrap(),
        b"\x01\x00\x00\x00{\"last_success_unix_ms\":5,\"problem\":\"unreachable\",\"unreadable\":2,\"unsendable\":3}"
    );
    assert_eq!(sync_status(&db).unwrap(), damaged);
    // T14.43: rows that could not be sealed are in the record only when there are some, so a
    // status without any reads and writes as before (and an older build ignores the field).
    let refused = SyncStatus {
        refused: 4,
        ..status.clone()
    };
    set_sync_status(&mut db, &refused).unwrap();
    assert_eq!(
        db.local_value(SYNC_STATUS_KEY).unwrap().unwrap(),
        b"\x01\x00\x00\x00{\"last_success_unix_ms\":5,\"problem\":\"unreachable\",\"unreadable\":2,\"refused\":4}"
    );
    assert_eq!(sync_status(&db).unwrap(), refused);
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
            entity: Some(r.table().into()),
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
        let v = Voiceprint::from_stored(
            vid,
            meta,
            p.id,
            earshot_core::diar::EmbeddingModelId::new("invented-model"),
            vec![0.5; 4],
            1,
            5_000,
        );
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

/// A person and a voiceprint of theirs written on `dev` (an enrolment of today's shape).
fn print_on(
    dev: &Device,
    seed: u64,
) -> (
    earshot_core::identity::EntityId,
    earshot_core::identity::EntityId,
) {
    use earshot_core::voiceprint::{Person, Voiceprint};
    use rand_core::SeedableRng;
    let clock = Arc::clone(&dev.svc.clock);
    let mut rng = rand_chacha::ChaCha8Rng::seed_from_u64(seed);
    let mut db = dev.svc.store();
    let (store, hlc) = db.parts();
    let mut stamp = earshot_core::model::Stamp {
        clock: &*clock,
        hlc,
        rng: &mut rng,
    };
    let p = Person::new("An invented colleague", false, &mut stamp);
    let vid = earshot_core::identity::EntityId::generate(stamp.clock, &mut *stamp.rng);
    let meta = earshot_core::identity::SyncMeta::new(stamp.hlc, stamp.clock);
    let v = Voiceprint::from_stored(
        vid,
        meta,
        p.id,
        earshot_core::diar::EmbeddingModelId::new("invented-model"),
        vec![0.5; 4],
        1,
        5_000,
    );
    store.put_person(p.clone()).unwrap();
    store.put_voiceprint(v).unwrap();
    (p.id, vid)
}

/// What an older build that still had the print does: writes it live again, later.
fn write_print_live_again(dev: &Device, print: earshot_core::identity::EntityId) {
    let clock = Arc::clone(&dev.svc.clock);
    let mut db = dev.svc.store();
    let (store, hlc) = db.parts();
    let v = store
        .voiceprints(&earshot_core::diar::EmbeddingModelId::new("invented-model"))
        .unwrap()
        .into_iter()
        .find(|v| v.id() == print)
        .unwrap();
    assert!(!v.meta().deleted, "still live there");
    let mut meta = *v.meta();
    meta.touch(hlc, &*clock);
    let again = earshot_core::voiceprint::Voiceprint::from_stored(
        v.id(),
        meta,
        v.person_id(),
        v.model().clone(),
        vec![0.5; 4],
        1,
        5_000,
    );
    store.put_voiceprint(again).unwrap();
}

fn print_state(dev: &Device, print: earshot_core::identity::EntityId) -> (bool, bool) {
    let v = dev
        .svc
        .store()
        .voiceprints(&earshot_core::diar::EmbeddingModelId::new("invented-model"))
        .unwrap()
        .into_iter()
        .find(|v| v.id() == print)
        .unwrap();
    let zero = earshot_core::voiceprint::effective(&v, &[])
        .sum()
        .iter()
        .all(|x| *x == 0.0);
    (v.meta().deleted, zero)
}

/// T12.77b3 (docs/voice-samples.md 7.4, traced sequence 1): a Forget here, then a computer
/// that still had the print writes it live with a later stamp. The live copy arrives, the
/// tombstone stays and is stamped again, the next push is accepted and the other computer
/// applies it.
#[test]
fn sync_a_forgotten_voiceprint_stays_forgotten_when_a_later_live_copy_arrives() {
    let relay = relay();
    let (a, b) = pair(&relay);
    set_sync_voiceprints(&a.svc, true).unwrap();
    set_sync_voiceprints(&b.svc, true).unwrap();
    let (ca, cb) = (a.client(), b.client());
    let (person, print) = print_on(&a, 41);
    ca.tick().unwrap();
    cb.tick().unwrap();
    assert_eq!(print_state(&b, print), (false, false), "B has the print");
    a.clock.advance_ms(1_000);
    crate::edit::forget_person_voices(&a.svc, person).unwrap();
    ca.tick().unwrap();
    // B writes it live afterwards by its own clock and pushes first: the relay keeps it.
    b.clock.advance_ms(5_000);
    write_print_live_again(&b, print);
    cb.tick().unwrap();
    // A refuses the live copy and stamps its tombstone again; the next tick sends it.
    ca.tick().unwrap();
    assert_eq!(
        print_state(&a, print),
        (true, true),
        "the Forget stays on A"
    );
    let r = ca.tick().unwrap();
    assert_eq!(r.superseded, 0, "{r:?}");
    cb.tick().unwrap();
    assert_eq!(print_state(&b, print), (true, true), "B applies the Forget");
    ca.tick().unwrap();
    assert_eq!(print_state(&a, print), (true, true));
}

/// T12.77b3 (7.4): a tombstone pushed while the relay holds a later live copy is answered
/// superseded; the pull brings the live copy, finality stamps the tombstone again, and the
/// next push wins.
#[test]
fn sync_a_superseded_voiceprint_tombstone_wins_on_the_next_push() {
    let relay = relay();
    let (a, b) = pair(&relay);
    set_sync_voiceprints(&a.svc, true).unwrap();
    set_sync_voiceprints(&b.svc, true).unwrap();
    let (ca, cb) = (a.client(), b.client());
    let (person, print) = print_on(&a, 42);
    ca.tick().unwrap();
    cb.tick().unwrap();
    a.clock.advance_ms(1_000);
    crate::edit::forget_person_voices(&a.svc, person).unwrap();
    // Before A pushes, B writes the print live later and pushes it.
    b.clock.advance_ms(5_000);
    write_print_live_again(&b, print);
    cb.tick().unwrap();
    let r = ca.tick().unwrap();
    assert!(
        r.superseded >= 1,
        "the relay holds a later live copy: {r:?}"
    );
    assert_eq!(
        print_state(&a, print),
        (true, true),
        "the Forget stays on A"
    );
    let r = ca.tick().unwrap();
    assert!(r.pushed >= 1 && r.superseded == 0, "{r:?}");
    cb.tick().unwrap();
    assert_eq!(print_state(&b, print), (true, true));
}

/// T12.77b3 (7.4, traced sequence 2): a print tombstone pulled while this computer holds
/// samples of that print erases them in the same apply.
#[test]
fn sync_a_pulled_voiceprint_tombstone_erases_this_computers_samples_of_it() {
    use earshot_core::voiceprint::{SampleOrigin, Voiceprint, VoiceprintSample};
    use rand_core::SeedableRng;
    let relay = relay();
    let (a, b) = pair(&relay);
    set_sync_voiceprints(&a.svc, true).unwrap();
    set_sync_voiceprints(&b.svc, true).unwrap();
    let (ca, cb) = (a.client(), b.client());
    let (person, print) = print_on(&a, 43);
    ca.tick().unwrap();
    cb.tick().unwrap();
    // B holds two samples of that print; its next tick sends them too.
    let samples: Vec<VoiceprintSample> = {
        let clock = Arc::clone(&b.svc.clock);
        let mut rng = rand_chacha::ChaCha8Rng::seed_from_u64(44);
        let mut db = b.svc.store();
        let (store, hlc) = db.parts();
        let mut stamp = earshot_core::model::Stamp {
            clock: &*clock,
            hlc,
            rng: &mut rng,
        };
        let v: Voiceprint = store
            .voiceprints_of(person)
            .unwrap()
            .into_iter()
            .find(|v| v.id() == print)
            .unwrap();
        let mut out = Vec::new();
        for x in [[1.0f32, 0.0, 0.0, 0.0], [0.0, 1.0, 0.0, 0.0]] {
            let s = VoiceprintSample::new(&v, &x, 2_000, SampleOrigin::Own, &mut stamp).unwrap();
            store.put_sample(s.clone()).unwrap();
            out.push(s);
        }
        out
    };
    a.clock.advance_ms(1_000);
    crate::edit::forget_person_voices(&a.svc, person).unwrap();
    ca.tick().unwrap();
    b.clock.advance_ms(1_000);
    cb.tick().unwrap();
    assert_eq!(print_state(&b, print), (true, true));
    let db = b.svc.store();
    assert!(db.samples_of(print).unwrap().is_empty());
    for s in &samples {
        assert!(db.sample(s.id()).unwrap().unwrap().is_erased());
    }
}

/// Voice rows on `dev`: a live print with two samples of it, and a second
/// person's print forgotten (a tombstone). Returns (live print, forgotten print).
fn held_rows_on(
    dev: &Device,
) -> (
    earshot_core::identity::EntityId,
    earshot_core::identity::EntityId,
) {
    use earshot_core::voiceprint::{SampleOrigin, VoiceprintSample};
    use rand_core::SeedableRng;
    let (_, live) = print_on(dev, 51);
    let (gone_person, gone) = print_on(dev, 52);
    {
        let clock = Arc::clone(&dev.svc.clock);
        let mut rng = rand_chacha::ChaCha8Rng::seed_from_u64(53);
        let mut db = dev.svc.store();
        let (store, hlc) = db.parts();
        let mut stamp = earshot_core::model::Stamp {
            clock: &*clock,
            hlc,
            rng: &mut rng,
        };
        let v = store
            .voiceprints(&earshot_core::diar::EmbeddingModelId::new("invented-model"))
            .unwrap()
            .into_iter()
            .find(|v| v.id() == live)
            .unwrap();
        for x in [[1.0f32, 0.0, 0.0, 0.0], [0.0, 0.0, 1.0, 0.0]] {
            let s = VoiceprintSample::new(&v, &x, 1_000, SampleOrigin::Own, &mut stamp).unwrap();
            store.put_sample(s).unwrap();
        }
    }
    dev.clock.advance_ms(1_000);
    crate::edit::forget_person_voices(&dev.svc, gone_person).unwrap();
    (live, gone)
}

fn holds_print(dev: &Device, print: earshot_core::identity::EntityId) -> Option<bool> {
    dev.svc
        .store()
        .voiceprints(&earshot_core::diar::EmbeddingModelId::new("invented-model"))
        .unwrap()
        .into_iter()
        .find(|v| v.id() == print)
        .map(|v| v.meta().deleted)
}

/// T12.77b8 (docs/voice-samples.md 7.2): rows written before sync could send them (live
/// prints, samples and a print tombstone) push in the first tick after the raise, oldest
/// first, raise the vault's format, and give the other computer the same effective print.
#[test]
fn sync_voice_samples_written_before_the_raise_push_oldest_first_and_give_the_same_print() {
    let relay = relay();
    let (a, b) = pair(&relay);
    set_sync_voiceprints(&a.svc, true).unwrap();
    set_sync_voiceprints(&b.svc, true).unwrap();
    let (live, gone) = held_rows_on(&a);
    let kinds = sync_settings(&a.svc.store()).unwrap().kinds();
    let waiting = a.svc.store().outbox_len_except(&[], kinds).unwrap();
    let r = a.client().tick().unwrap();
    assert_eq!(
        u64::try_from(r.pushed).unwrap(),
        waiting,
        "every waiting row went: {r:?}"
    );
    assert_eq!(a.svc.store().outbox_len_except(&[], kinds).unwrap(), 0);
    // At the relay, in push order, the stamps never go backwards: oldest first.
    let db = rusqlite::Connection::open(relay._dir.0.join("relay").join("relay.db")).unwrap();
    let format: u32 = db
        .query_row("SELECT format FROM vault WHERE id = 1", [], |r| r.get(0))
        .unwrap();
    assert_eq!(format, earshot_sync::ENVELOPE_FORMAT);
    let stamps: Vec<(i64, i64)> = db
        .prepare("SELECT wall_ms, logical FROM row ORDER BY seq")
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert!(stamps.len() >= 6, "{stamps:?}");
    assert!(stamps.is_sorted(), "{stamps:?}");
    let rb = b.client().tick().unwrap();
    assert_eq!(rb.held, 0, "{rb:?}");
    assert_eq!(holds_print(&b, gone), Some(true), "the Forget arrived");
    let effective_of = |dev: &Device| {
        let person = dev
            .svc
            .store()
            .voiceprints(&earshot_core::diar::EmbeddingModelId::new("invented-model"))
            .unwrap()
            .into_iter()
            .find(|v| v.id() == live)
            .unwrap()
            .person_id();
        earshot_core::voiceprint::live_prints_of(&**dev.svc.store(), person).unwrap()
    };
    let (on_a, on_b) = (effective_of(&a), effective_of(&b));
    assert_eq!(on_a[0].1.count(), 3, "the base and two samples");
    assert_eq!(on_a, on_b, "the same effective print on both");
}

/// T12.77b8: a damaged live sample row stays queued and is reported as refused; every other
/// row still goes, and the next tick does the same (never stopping every push).
#[test]
fn sync_a_damaged_sample_row_is_kept_and_reported_never_stopping_the_push() {
    let relay = relay();
    let (a, b) = pair(&relay);
    set_sync_voiceprints(&a.svc, true).unwrap();
    set_sync_voiceprints(&b.svc, true).unwrap();
    let (live, _) = held_rows_on(&a);
    let db_path = earshot_store::DataDir::new(a._dir.0.join("data")).db_path();
    let samples = a.svc.store().samples_of(live).unwrap();
    assert_eq!(samples.len(), 2);
    {
        let c = rusqlite::Connection::open(&db_path).unwrap();
        let two: Vec<u8> = [2.0f32, 0.0, 0.0, 0.0]
            .iter()
            .flat_map(|x| x.to_le_bytes())
            .collect();
        c.execute(
            "UPDATE voiceprint_sample SET vector = ?1 WHERE id = ?2",
            rusqlite::params![two, samples[0].id().as_uuid().as_bytes().to_vec()],
        )
        .unwrap();
    }
    let ca = a.client();
    let r = ca.tick().unwrap();
    assert_eq!((r.unsendable, r.refused), (1, 0), "{r:?}");
    assert!(r.pushed >= 5, "the rest went: {r:?}");
    let kinds = sync_settings(&a.svc.store()).unwrap().kinds();
    assert_eq!(a.svc.store().outbox_len_except(&[], kinds).unwrap(), 1);
    // The page counts it as damaged, not as waiting.
    let view = sync_overview(&a.svc, &*a.vault).unwrap();
    assert_eq!((view.waiting, view.status.unsendable), (0, 1));
    let r = ca.tick().unwrap();
    assert_eq!((r.pushed, r.unsendable), (0, 1), "{r:?}");
    b.client().tick().unwrap();
    let on_b = b.svc.store().samples_of(live).unwrap();
    assert_eq!(on_b, vec![samples[1].clone()], "only the readable sample");
}

/// T12.77b8: a pulled sample whose envelope opens but whose record fails its check is not a
/// row, its entity named, so the store holds it as `Format`.
#[test]
fn sync_a_malformed_pulled_sample_opens_as_no_row_with_its_entity_named() {
    use earshot_core::identity::{
        EntityId, HlcTimestamp, SyncClass, SyncMeta, SyncSerialize, SyncableOptInClass,
    };
    #[derive(serde::Serialize)]
    struct Malformed(earshot_core::voiceprint::SampleRecord);
    impl SyncClass for Malformed {
        type Class = SyncableOptInClass;
    }
    impl SyncSerialize for Malformed {
        const ENTITY: &'static str = "voiceprint_sample";
        fn sync_id(&self) -> EntityId {
            self.0.id
        }
        fn sync_meta(&self) -> &SyncMeta {
            &self.0.meta
        }
    }
    let node = earshot_core::identity::DeviceId::from_bytes([9; 16]);
    let stamp = HlcTimestamp {
        wall_ms: crate::testutil::T0,
        logical: 0,
        node,
    };
    let meta = SyncMeta {
        created_at: stamp,
        updated_at: stamp,
        deleted: false,
        origin_device_id: node,
    };
    let id =
        |n: u8| EntityId::from_bytes([0x01, 0x90, 0, 0, 0, 0, 0x70, 0, 0x80, 0, 0, 0, 0, 0, 2, n]);
    let good = earshot_core::voiceprint::SampleRecord {
        id: id(1),
        meta,
        voiceprint_id: Some(id(2)),
        person_id: Some(id(3)),
        model: Some(earshot_core::diar::EmbeddingModelId::new("invented-model")),
        vector: vec![1.0, 0.0, 0.0, 0.0],
        speech_ms: Some(2_000),
        meeting_id: None,
        speaker_id: None,
    };
    let keys = earshot_sync::VaultKey::from_bytes([3; 32]).keys();
    let open = |r: earshot_core::voiceprint::SampleRecord| {
        let sealed = earshot_sync::seal_row(&keys, &Malformed(r)).unwrap();
        engine::open(&keys, 1, &sealed.key.0, sealed.stamp, sealed.body).unwrap()
    };
    let inc = open(good.clone());
    assert!(inc.row.is_some(), "sanity: a good sample decodes");
    let malformed = [
        earshot_core::voiceprint::SampleRecord {
            vector: vec![2.0, 0.0, 0.0, 0.0],
            ..good.clone()
        },
        earshot_core::voiceprint::SampleRecord {
            vector: Vec::new(),
            ..good.clone()
        },
        earshot_core::voiceprint::SampleRecord {
            vector: vec![f32::NAN, 0.0, 0.0, 0.0],
            ..good.clone()
        },
        earshot_core::voiceprint::SampleRecord {
            person_id: None,
            ..good.clone()
        },
        earshot_core::voiceprint::SampleRecord {
            meeting_id: Some(id(4)),
            ..good
        },
    ];
    for r in malformed {
        let inc = open(r);
        assert!(inc.row.is_none(), "not decoded");
        assert_eq!(inc.entity.as_deref(), Some("voiceprint_sample"));
    }
}

// ---- T12.93: an ignored voice is ignored on every computer ----

/// A meeting recorded and processed on `dev` (audio, a final transcript, identity entries).
fn processed_on(dev: &Device, seed: u64) -> earshot_core::identity::EntityId {
    use crate::host::tests::{fake_post, recorded};
    let outcome = recorded(&dev.svc, seed);
    let models = crate::backends::ModelDir::new(dev.svc.data.root().join("models"));
    let (post_cancel, summary_cancel) = (
        earshot_core::asr::CancelToken::new(),
        earshot_core::asr::CancelToken::new(),
    );
    let run = crate::host::AfterMeeting {
        models: &models,
        model: "base.en",
        order: &[crate::backends::Accelerator::Cpu],
        post: true,
        whole_pass: false,
        summary: None,
        no_summary: crate::host::SkipReason::NotRequested,
        post_cancel: &post_cancel,
        summary_cancel: &summary_cancel,
        offload: None,
        order_run: None,
    };
    let result = crate::host::after_meeting_with(&dev.svc, &outcome, &run, fake_post, &mut |_| {});
    assert!(
        matches!(result.post, Ok(crate::host::HostPost::Done(_))),
        "{:?}",
        result.post
    );
    outcome.meeting
}

fn retranscribed_on(dev: &Device, m: earshot_core::identity::EntityId) -> crate::post::PostOutcome {
    let cancel = earshot_core::asr::CancelToken::new();
    let again = crate::edit::retranscribe_with(
        &dev.svc,
        m,
        &cancel,
        crate::host::tests::fake_post,
        &mut |_| {},
    )
    .unwrap();
    match again.post {
        crate::host::HostPost::Done(outcome) => *outcome,
        other => panic!("{other:?}"),
    }
}

fn speaker_row(
    dev: &Device,
    m: earshot_core::identity::EntityId,
    id: earshot_core::identity::EntityId,
) -> earshot_core::model::Speaker {
    dev.svc
        .store()
        .speakers(m)
        .unwrap()
        .into_iter()
        .find(|s| s.id == id)
        .unwrap()
}

fn voices(dev: &Device, m: earshot_core::identity::EntityId) -> Vec<earshot_core::model::Speaker> {
    dev.svc
        .store()
        .speakers(m)
        .unwrap()
        .into_iter()
        .filter(|s| !s.meta.deleted && s.role != earshot_core::diar::SpeakerRole::Me)
        .collect()
}

/// Whether `dev` reads voice `id` of `m` as ignored, by the one rule.
fn ignored_on(
    dev: &Device,
    m: earshot_core::identity::EntityId,
    id: earshot_core::identity::EntityId,
) -> bool {
    let legacy = crate::edit::legacy_ignores(&dev.svc.store(), m).unwrap();
    crate::edit::is_ignored(&speaker_row(dev, m, id), &legacy)
}

/// What 0.4.2 wrote for an ignore: the entry's flag on `dev`, the row untouched.
fn legacy_ignore_on(
    dev: &Device,
    m: earshot_core::identity::EntityId,
    id: earshot_core::identity::EntityId,
) {
    let key = crate::identities::record_key(m);
    let mut db = dev.svc.store();
    let bytes = db.local_value(key.as_str()).unwrap().unwrap();
    let crate::identities::StoredIdentities::Record(mut record) = crate::identities::decode(&bytes)
    else {
        panic!("unreadable record");
    };
    // "Me" has no entry from a pass: give it one (a voice once marked in the room has one).
    if !record.speakers.iter().any(|i| i.speaker == id) {
        let mut copy = record.speakers[0].clone();
        copy.speaker = id;
        record.speakers.push(copy);
    }
    record
        .speakers
        .iter_mut()
        .find(|i| i.speaker == id)
        .unwrap()
        .ignored = true;
    crate::local::set(&mut db, &key, &crate::identities::encode(&record)).unwrap();
}

/// Each device pushes, then each pulls what the other sent.
fn trade(a: &Device, b: &Device) {
    a.client().tick().unwrap();
    b.client().tick().unwrap();
    a.client().tick().unwrap();
}

#[test]
fn sync_an_ignore_on_one_computer_is_read_on_the_other_and_kept_through_a_retranscribe() {
    let relay = relay();
    let (a, b) = pair(&relay);
    // Recorded and processed on A, which holds the identity entries; B holds none.
    let m = processed_on(&a, 930);
    trade(&a, &b);
    let v = voices(&b, m)[0].id;
    assert!(
        crate::edit::identities_in(&b.svc.store(), m)
            .unwrap()
            .is_empty()
    );
    assert!(crate::edit::can_ignore(&speaker_row(&b, m, v)));

    // Ignored on B (no entry there), read as ignored on A.
    b.clock.advance_ms(1_000);
    assert!(crate::edit::ignore_speaker(&b.svc, m, v).unwrap());
    trade(&b, &a);
    assert!(ignored_on(&a, m, v), "A reads B's ignore");
    // A re-transcription on A keeps it: same row, unlinked, still ignored, and it goes back.
    let outcome = retranscribed_on(&a, m);
    assert!(outcome.speakers.iter().find(|p| p.id == v).unwrap().ignored);
    assert!(ignored_on(&a, m, v));
    trade(&a, &b);
    assert!(ignored_on(&b, m, v));

    // Un-ignored on B: A reads it as needing a name again.
    b.clock.advance_ms(1_000);
    assert!(crate::edit::unignore_speaker(&b.svc, m, v).unwrap());
    trade(&b, &a);
    assert!(!ignored_on(&a, m, v), "A reads B's un-ignore");
    // An un-ignore made elsewhere does not re-stamp A's entry: A keeps the voice as a user
    // edit (its label, unlinked), a kept name never a lost one.
    let outcome = retranscribed_on(&a, m);
    let carried = outcome.speakers.iter().find(|p| p.id == v).unwrap();
    assert!(!carried.ignored && carried.person.is_none());

    // "Not <name>" on B (no entry there either): A reads an unlinked, ignored "Speaker N".
    let w = voices(&b, m).into_iter().find(|s| s.id != v).unwrap().id;
    b.clock.advance_ms(1_000);
    crate::edit::link_speaker(
        &b.svc,
        m,
        w,
        &crate::edit::PersonChoice::New {
            name: "Rowan Invented".into(),
        },
    )
    .unwrap();
    // Read the row first: `speaker_row` takes the store lock itself.
    let linked_w = speaker_row(&b, m, w);
    assert!(crate::edit::can_not_this_person(&**b.svc.store(), &linked_w).unwrap());
    crate::edit::not_this_person(&b.svc, m, w).unwrap();
    trade(&b, &a);
    let on_a = speaker_row(&a, m, w);
    assert!(on_a.person_id.is_none() && on_a.display_name.starts_with("Speaker "));
    assert!(ignored_on(&a, m, w), "A reads B's Not as an ignore");
    assert_eq!(a.rows(), b.rows());
}

/// The one-off copy runs in the sync thread only after a pull that reached the end, so a name
/// another computer gave a 0.4.2-ignored voice, still on the relay when the thread starts, is
/// pulled first and never overwritten. A pulls one row per page here, so a copy after any
/// earlier page, or before the pull, would run while the name is still unpulled; A's clock is
/// ahead of B's, so such a copy's write would be newer and win (checked by mutation). The copy
/// runs once, honours the rule (linked, Me, tombstoned and deleted-meeting rows skipped) and its
/// flags reach the other computer.
#[test]
fn sync_ignore_copy_runs_after_the_first_completed_pull_and_keeps_a_pulled_name() {
    let relay = relay();
    let (a, b) = pair(&relay);
    let m = processed_on(&a, 931);
    let gone_meeting = processed_on(&a, 932);
    trade(&a, &b);
    let remote = voices(&a, m);
    let (named, kept) = (remote[0].id, remote[1].id);
    let me = a
        .svc
        .store()
        .speakers(m)
        .unwrap()
        .into_iter()
        .find(|s| s.role == earshot_core::diar::SpeakerRole::Me)
        .unwrap()
        .id;
    // 0.4.2 on A: three ignores in this meeting kept only in entries, one in a meeting that is
    // deleted before the copy.
    for id in [named, kept, me] {
        legacy_ignore_on(&a, m, id);
    }
    let in_gone = voices(&a, gone_meeting)[0].id;
    legacy_ignore_on(&a, gone_meeting, in_gone);
    a.svc.delete_meeting(gone_meeting).unwrap();
    assert!(ignored_on(&a, m, named) && ignored_on(&a, m, kept));
    // Meanwhile B (already updated) writes other rows, then names one of the voices last, and
    // pushes; A has pulled none of it when its thread starts.
    b.clock.advance_ms(1_000);
    for seed in 0..3 {
        meeting(&b.svc, "Invented later meeting", 940 + seed);
    }
    b.clock.advance_ms(1_000);
    crate::edit::link_speaker(
        &b.svc,
        m,
        named,
        &crate::edit::PersonChoice::New {
            name: "Avery Invented".into(),
        },
    )
    .unwrap();
    b.client().tick().unwrap();
    a.clock.advance_ms(60_000);
    assert!(!crate::ignore_copy::copied(&a.svc.store()).unwrap());

    let thread = SyncThread::start(a.client().with_pull_pages(1, 1), quick()).unwrap();
    wait_for("the copy after the first completed pull", || {
        crate::ignore_copy::copied(&a.svc.store()).unwrap()
    });
    thread.stop();
    let pulled = speaker_row(&a, m, named);
    assert_eq!(
        pulled.display_name, "Avery Invented",
        "the pulled name stays"
    );
    assert!(pulled.person_id.is_some() && !pulled.ignored);
    assert!(!ignored_on(&a, m, named));
    assert!(speaker_row(&a, m, kept).ignored, "copied onto the row");
    assert!(!speaker_row(&a, m, me).ignored, "never the user");
    // After the copy the entry flag is never read again.
    assert!(
        crate::edit::legacy_ignores(&a.svc.store(), m)
            .unwrap()
            .is_empty()
    );
    // Once only.
    assert_eq!(crate::ignore_copy::copy_once(&a.svc).unwrap(), 0);
    // The copied flag reaches B, which never had the entry, and B keeps its name.
    trade(&a, &b);
    assert!(ignored_on(&b, m, kept));
    assert_eq!(speaker_row(&b, m, named).display_name, "Avery Invented");
}

/// The copy does not run after a tick that failed, after a tick whose pull stopped with
/// `more == true`, or while a pulled row is held (its meeting in use here): any of those may
/// hold the edit another computer made. Legacy ignores stay honoured meanwhile; once the held
/// row is applied the copy runs and keeps B's name.
#[test]
fn sync_ignore_copy_waits_for_a_failed_tick_a_partial_pull_and_a_held_row() {
    let relay = relay();
    let (a, b) = pair(&relay);
    let m = processed_on(&a, 934);
    trade(&a, &b);
    let named = voices(&a, m)[0].id;
    legacy_ignore_on(&a, m, named);

    // A failed tick: the relay is somewhere nothing listens.
    {
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let down = Device::new("sync-ignore-down");
        {
            let mut db = down.svc.store();
            crate::local::set(
                &mut db,
                &crate::local::DeviceRecord::SyncServer.key(),
                format!("http://127.0.0.1:{port}").as_bytes(),
            )
            .unwrap();
        }
        let key = earshot_sync::VaultKey::generate().unwrap();
        down.vault
            .save(&key, &DeviceToken::new("invented-token".into()))
            .unwrap();
        let c = down.client();
        let (report, outcome) = c.tick_reporting();
        assert!(outcome.is_err());
        let ticked: Result<_, ()> = Ok(outcome);
        assert!(!super::thread::copy_after_tick(&c, &ticked, Some(&report)));
        assert!(!crate::ignore_copy::copied(&down.svc.store()).unwrap());
    }

    // B names the voice and writes more rows; A pulls one row per page.
    b.clock.advance_ms(1_000);
    crate::edit::link_speaker(
        &b.svc,
        m,
        named,
        &crate::edit::PersonChoice::New {
            name: "Blair Invented".into(),
        },
    )
    .unwrap();
    for seed in 0..2 {
        meeting(&b.svc, "Invented later meeting", 950 + seed);
    }
    b.client().tick().unwrap();
    a.clock.advance_ms(60_000);

    // The meeting is in use here for the next ticks, so B's speaker row is held when pulled.
    let lease = a.svc.lease_meeting_for(m, MeetingJob::Processing).unwrap();

    // A partial pull: one page of one row, more left on the relay.
    let partial = a.client().with_pull_pages(1, 1);
    let (report, outcome) = partial.tick_reporting();
    assert!(outcome.is_ok() && !report.pull_complete, "{report:?}");
    let ticked: Result<_, ()> = Ok(outcome);
    assert!(!super::thread::copy_after_tick(
        &partial,
        &ticked,
        Some(&report)
    ));
    assert!(!crate::ignore_copy::copied(&a.svc.store()).unwrap());

    // A complete pull while B's speaker row is held: still no copy.
    let c = a.client();
    let (report, outcome) = c.tick_reporting();
    assert!(outcome.is_ok() && report.pull_complete, "{report:?}");
    assert!(c.holds_rows_for_retry().unwrap(), "{report:?}");
    let ticked: Result<_, ()> = Ok(outcome);
    assert!(!super::thread::copy_after_tick(&c, &ticked, Some(&report)));
    assert!(!crate::ignore_copy::copied(&a.svc.store()).unwrap());
    assert!(ignored_on(&a, m, named), "the legacy ignore still counts");
    assert_eq!(speaker_row(&a, m, named).person_id, None);

    // Free again: the held row is applied, then the copy runs and the name stays.
    drop(lease);
    let (report, outcome) = c.tick_reporting();
    assert!(!c.holds_rows_for_retry().unwrap(), "{report:?}");
    let ticked: Result<_, ()> = Ok(outcome);
    assert!(super::thread::copy_after_tick(&c, &ticked, Some(&report)));
    let after = speaker_row(&a, m, named);
    assert_eq!(after.display_name, "Blair Invented");
    assert!(after.person_id.is_some() && !after.ignored);
    assert!(!ignored_on(&a, m, named));
}

/// Before the copy, a re-transcription honours a 0.4.2 entry flag and carries it into its new
/// entry, never onto the row (which would sync it before the copy's completed pull); the copy
/// then puts it on the row, from where it reaches the other computer. Un-ignore clears it.
#[test]
fn sync_ignore_legacy_flag_is_honoured_before_the_copy_and_travels_after_it() {
    let relay = relay();
    let (a, b) = pair(&relay);
    let m = processed_on(&a, 933);
    trade(&a, &b);
    let v = voices(&a, m)[0].id;
    legacy_ignore_on(&a, m, v);
    assert!(!crate::ignore_copy::copied(&a.svc.store()).unwrap());
    assert!(ignored_on(&a, m, v) && !ignored_on(&b, m, v));
    let outcome = retranscribed_on(&a, m);
    assert!(outcome.speakers.iter().find(|p| p.id == v).unwrap().ignored);
    assert!(!speaker_row(&a, m, v).ignored, "not written onto the row");
    assert!(
        ignored_on(&a, m, v),
        "still honoured through the carried entry flag"
    );
    trade(&a, &b);
    assert!(!ignored_on(&b, m, v), "nothing synced before the copy");
    assert_eq!(crate::ignore_copy::copy_once(&a.svc).unwrap(), 1);
    trade(&a, &b);
    assert!(ignored_on(&b, m, v));
    // Un-ignore on A clears the row; B follows.
    assert!(crate::edit::unignore_speaker(&a.svc, m, v).unwrap());
    assert!(!ignored_on(&a, m, v));
    trade(&a, &b);
    assert!(!ignored_on(&b, m, v));
}

// ---- T14.42: rows pulled before their parents apply in the tick that brings the parents ----

/// Stamps `m`'s tracks and speakers again and pushes them, then its meeting row: the relay
/// files a row it accepts at the end, so it then holds the meeting's lines first, its tracks
/// and speakers after them and the meeting row last (as for any meeting edited after its rows
/// were first sent).
fn file_children_before_parents(dev: &Device, m: earshot_core::identity::EntityId) {
    dev.clock.advance_ms(1_000);
    {
        let clock = Arc::clone(&dev.svc.clock);
        let mut db = dev.svc.store();
        let (store, hlc) = db.parts();
        for mut t in store.tracks(m).unwrap() {
            t.meta.touch(hlc, &*clock);
            store.put_track(t).unwrap();
        }
        for mut s in store.speakers(m).unwrap() {
            s.meta.touch(hlc, &*clock);
            store.put_speaker(s).unwrap();
        }
    }
    dev.client().tick().unwrap();
    dev.clock.advance_ms(1_000);
    dev.rename(m, "Renamed after its rows were sent");
    dev.client().tick().unwrap();
}

/// A first device with two meetings, one of them filed children first, and a second device
/// that has joined and not yet pulled.
fn vault_filed_children_first(relay: &TestRelay) -> (Device, Device) {
    let a = Device::new("sync-parents-a");
    set_up_first_device(&a.svc, &*a.vault, &relay.relay.url(), SECRET).unwrap();
    let m = meeting(&a.svc, "Planning", 1);
    meeting(&a.svc, "Review", 2);
    a.client().tick().unwrap();
    file_children_before_parents(&a, m);
    let b = Device::new("sync-parents-b");
    let offer = offer_pairing_code(&a.svc, a.vault()).unwrap();
    join_with_code(&b.svc, &*b.vault, &relay.relay.url(), offer.code()).unwrap();
    (a, b)
}

fn assert_filled_in_one_tick(a: &Device, b: &Device, client: &SyncClient) {
    let (report, outcome) = client.tick_reporting();
    assert!(outcome.is_ok() && report.pull_complete, "{report:?}");
    assert_eq!(
        b.svc.store().held_rows().unwrap().len(),
        0,
        "no row waits for a parent this tick brought: {report:?}"
    );
    assert_eq!(report.held, 0, "{report:?}");
    assert!(!client.holds_rows_for_retry().unwrap());
    assert_eq!(a.rows(), b.rows(), "one tick fills the new computer in");
}

/// A new computer pulls a meeting's lines, then its tracks and speakers, then the meeting row,
/// each in a later page: every row is applied in the tick that brings its parents, not held
/// for a later tick.
#[test]
fn sync_rows_pulled_pages_before_their_parents_apply_in_the_same_tick() {
    let relay = relay();
    let (a, b) = vault_filed_children_first(&relay);
    let client = b.client().with_pull_pages(200, 2);
    assert_filled_in_one_tick(&a, &b, &client);
}

/// The same with the whole vault in one page: a row earlier in the page than its parent is
/// applied in that tick too.
#[test]
fn sync_rows_pulled_before_their_parents_in_one_page_apply_in_the_same_tick() {
    let relay = relay();
    let (a, b) = vault_filed_children_first(&relay);
    assert_filled_in_one_tick(&a, &b, &b.client());
}

/// The retries cost work in proportion to the rows, not to the pages: a new computer pulling
/// several meetings filed children first, two rows a page, opens each held row at most three
/// times in the tick (a line waits for its track, which waits for its meeting), however many
/// pages bring a parent. Twice the meetings is about twice the work, not four times.
#[test]
fn sync_rows_held_for_a_parent_are_opened_again_in_proportion_to_their_number() {
    let filled = |meetings: u64| {
        let relay = relay();
        let a = Device::new("sync-parents-linear-a");
        set_up_first_device(&a.svc, &*a.vault, &relay.relay.url(), SECRET).unwrap();
        let ids: Vec<_> = (0..meetings)
            .map(|n| meeting(&a.svc, "Invented meeting", 300 + n))
            .collect();
        a.client().tick().unwrap();
        for m in ids {
            file_children_before_parents(&a, m);
        }
        let b = Device::new("sync-parents-linear-b");
        let offer = offer_pairing_code(&a.svc, a.vault()).unwrap();
        join_with_code(&b.svc, &*b.vault, &relay.relay.url(), offer.code()).unwrap();
        let client = b.client().with_pull_pages(500, 2);
        let (report, outcome) = client.tick_reporting();
        assert!(outcome.is_ok() && report.pull_complete, "{report:?}");
        assert_eq!(report.held, 0, "{report:?}");
        assert_eq!(a.rows(), b.rows());
        assert_eq!(report.pulled as u64, meetings * 8, "{report:?}");
        report
    };
    let four = filled(4);
    let eight = filled(8);
    for r in [&four, &eight] {
        assert!(
            r.retried <= 3 * r.pulled,
            "each held row is opened at most once per level: {r:?}"
        );
    }
    assert!(
        eight.retried <= 2 * four.retried + 8,
        "twice the rows, about twice the work: {} then {}",
        four.retried,
        eight.retried
    );
}

/// A row whose parent never arrives stays held and is counted; rows held for other reasons
/// are not touched by the retries a new parent sets off.
#[test]
fn sync_a_row_whose_parent_never_arrives_stays_held_and_is_counted() {
    let relay = relay();
    let (a, b) = pair(&relay);
    let m = meeting(&a.svc, "Planning", 1);
    a.client().tick().unwrap();
    // A track of a meeting no device holds is applied on B as a pulled page: held for its
    // parent.
    let first = {
        let db = a.svc.store();
        db.tracks(m).unwrap()[0].clone()
    };
    let orphan = earshot_store::SyncedRow::Track(earshot_core::model::Track {
        id: earshot_core::identity::EntityId::from_bytes([0x5a; 16]),
        meeting_id: earshot_core::identity::EntityId::from_bytes([0x5b; 16]),
        ..first
    });
    let applied = apply_page(&b, &[orphan]);
    assert_eq!(applied.held[3], 1, "{applied:?}");
    let client = b.client();
    let (report, outcome) = client.tick_reporting();
    assert!(outcome.is_ok(), "{report:?}");
    assert_eq!(report.held, 1, "the orphan is still held: {report:?}");
    assert!(client.holds_rows_for_retry().unwrap());
    assert_eq!(a.rows(), b.rows(), "everything else applied");
}

// ---- T14.43: rows refused before sealing are counted and shown ----

/// A row that cannot be sealed (here a meeting row over the size limit) stays queued and never
/// goes. It is counted once per tick, however many push pages read it, kept in the status the
/// Sync page reads and left out of "waiting to send"; everything else is sent. Once the row
/// fits again it goes and the count clears.
#[test]
fn sync_rows_that_cannot_be_sealed_are_counted_once_and_are_not_waiting() {
    let relay = relay();
    let (a, b) = pair(&relay);
    let big = meeting(&a.svc, "Planning", 1);
    a.client().tick().unwrap();
    a.clock.advance_ms(1_000);
    a.rename(big, &"x".repeat(earshot_sync::MAX_PLAINTEXT + 1));
    // More rows than one push page, all stamped after it: two pages read the refused row.
    a.clock.advance_ms(1_000);
    for seed in 0..30 {
        meeting(&a.svc, "Invented later meeting", 100 + seed);
    }
    let ca = a.client();
    let (r, outcome) = ca.tick_reporting();
    assert!(outcome.is_ok(), "a refused row is not a problem: {r:?}");
    assert!(r.pushed >= 240, "the rest went, past the first page: {r:?}");
    assert_eq!(r.refused, 1, "once, though two push pages read it: {r:?}");
    assert_eq!(sync_status(&a.svc.store()).unwrap().refused, 1);
    assert_eq!(a.svc.store().outbox_len().unwrap(), 1, "it stays queued");
    let view = sync_overview(&a.svc, &*a.vault).unwrap();
    assert_eq!((view.waiting, view.status.refused), (0, 1));
    // The next tick refuses it again and says the same.
    let r = ca.tick().unwrap();
    assert_eq!((r.pushed, r.refused), (0, 1), "{r:?}");
    assert_eq!(sync_status(&a.svc.store()).unwrap().refused, 1);
    // The other computer has every other meeting and still the title from before.
    b.client().tick().unwrap();
    assert_eq!(b.title(big).as_deref(), Some("Planning"));
    assert_eq!(b.svc.store().meetings().unwrap().len(), 31);
    // Shortened, it goes; nothing is counted or left behind.
    a.clock.advance_ms(1_000);
    a.rename(big, "Planning, shorter");
    let r = ca.tick().unwrap();
    assert_eq!((r.pushed, r.refused), (1, 0), "{r:?}");
    let view = sync_overview(&a.svc, &*a.vault).unwrap();
    assert_eq!((view.waiting, view.status.refused), (0, 0));
    b.client().tick().unwrap();
    assert_eq!(b.title(big).as_deref(), Some("Planning, shorter"));
}

/// A tick that ends on a problem keeps the count the last completed tick found, as it keeps
/// the damaged count.
#[test]
fn sync_a_failed_tick_keeps_the_refused_count_of_the_last_completed_one() {
    let a = Device::new("sync-refused-kept");
    let mut db = a.svc.store();
    let before = SyncStatus {
        refused: 2,
        unsendable: 1,
        ..SyncStatus::default()
    };
    set_sync_status(&mut db, &before).unwrap();
    drop(db);
    // No address is stored: the client cannot be made, and the thread records the problem.
    record_problem(&a.svc, SyncProblem::Unreachable);
    let after = sync_status(&a.svc.store()).unwrap();
    assert_eq!((after.refused, after.unsendable), (2, 1));
    assert_eq!(after.problem, Some(SyncProblem::Unreachable));
}

/// The retry at the start of each tick reads at most a page of held rows and goes on from its
/// last key next tick, so more held rows than one page are each tried in turn; rows held for a
/// newer format or a far clock are read too until one pass from the first key has reached the
/// end, then only rows held for a meeting in use or a parent. Rows held for a far clock are
/// read again by the first pass that starts an hour or more after the last one that read them
/// began, or after this device's clock was set back (T14.44); rows held for a newer format
/// never again.
#[test]
fn sync_held_row_retries_take_turns_read_format_once_and_far_clocks_hourly() {
    use earshot_store::HoldReason;
    const HOUR: u64 = super::engine::CLOCK_RETRY_MS;
    assert_eq!(HOUR, 60 * 60 * 1000);
    let t = 1_792_294_800_000;
    let mut retry = super::engine::HeldRetry::default();
    let quick: &[HoldReason] = &[HoldReason::Busy, HoldReason::Parent];
    let with_clock: &[HoldReason] = &[HoldReason::Clock, HoldReason::Busy, HoldReason::Parent];
    // The first pass reads every reason from the first key, a full page at a time.
    assert_eq!(retry.begin(t), (&HoldReason::ALL[..], None));
    retry.read(3, 3, Some([1; 16]));
    assert_eq!(retry.begin(t + 1), (&HoldReason::ALL[..], Some([1; 16])));
    retry.read(3, 3, Some([2; 16]));
    assert_eq!(retry.begin(t + 2), (&HoldReason::ALL[..], Some([2; 16])));
    // A short page is the end: the slow reasons are done, and the next retry starts over.
    retry.read(1, 3, Some([3; 16]));
    assert_eq!(retry.begin(t + 3), (quick, None));
    retry.read(3, 3, Some([1; 16]));
    assert_eq!(
        retry.begin(t + 4),
        (quick, Some([1; 16])),
        "on from the last key"
    );
    retry.read(0, 3, None);
    assert_eq!(retry.begin(t + 5), (quick, None), "and round again");
    retry.read(0, 3, None);

    // Just under an hour after the pass that read them began: not yet.
    assert_eq!(retry.begin(t + HOUR - 1), (quick, None));
    retry.read(0, 3, None);
    // An hour after it began: the rows held for a far clock are read again, format rows not.
    assert_eq!(retry.begin(t + HOUR), (with_clock, None));
    // The pass keeps its reasons until it ends, however long that takes.
    retry.read(3, 3, Some([4; 16]));
    assert_eq!(retry.begin(t + HOUR + 1), (with_clock, Some([4; 16])));
    retry.read(2, 3, Some([5; 16]));
    // The hour counts from when that pass began.
    assert_eq!(retry.begin(t + 2 * HOUR - 1), (quick, None));
    retry.read(0, 3, None);
    assert_eq!(retry.begin(t + 2 * HOUR), (with_clock, None));
    retry.read(0, 3, None);
    // The clock was set back (it reads earlier than when they were last read): read them now,
    // since a corrected clock is what clears them.
    assert_eq!(retry.begin(t + HOUR), (with_clock, None));
    retry.read(0, 3, None);
    assert_eq!(retry.begin(t + HOUR + 1), (quick, None));
    // A pass that is under way when the hour comes round is not changed; the next one reads
    // them.
    retry.read(3, 3, Some([6; 16]));
    assert_eq!(retry.begin(t + 3 * HOUR), (quick, Some([6; 16])));
    retry.read(0, 3, None);
    assert_eq!(retry.begin(t + 3 * HOUR), (with_clock, None));
}

// ---- T14.44: a clock far ahead cannot poison a vault ----

/// A relay whose clock is the test's own (Unix ms in `now`), so "a day past the relay's clock"
/// is exact.
fn relay_on_clock(now: &Arc<std::sync::atomic::AtomicU64>) -> TestRelay {
    let dir = TempDir::new("sync-relay-clock");
    let now = Arc::clone(now);
    let clock: earshot_sync_server::Clock =
        Arc::new(move || now.load(std::sync::atomic::Ordering::SeqCst) / 1000);
    let server = RelayServer::open_with_clock(&relay_config(&dir), clock).unwrap();
    TestRelay {
        relay: LocalRelay::start(server).unwrap(),
        _dir: dir,
    }
}

const DAY_MS: u64 = 24 * 60 * 60 * 1000;

/// The relay's window and the window a device holds pulled rows by are one figure: a row the
/// relay accepts is one every device with a right clock applies.
#[test]
fn sync_the_relay_and_the_devices_hold_far_stamps_by_the_same_window() {
    assert_eq!(
        earshot_sync::wire::MAX_STAMP_AHEAD_MS,
        earshot_store::MAX_CLOCK_AHEAD_MS
    );
    assert_eq!(earshot_store::MAX_CLOCK_AHEAD_MS, DAY_MS);
}

/// A computer whose clock jumped three days ahead and was put right: what it wrote meanwhile,
/// and everything it writes after (its stamps go on from the jump), is not sent. The tick
/// still sends the older rows, pulls, counts as a sync and says why rows wait. The vault is
/// not poisoned: the other computer's later edit of the same meeting is the relay's copy and
/// reaches a third computer.
#[test]
fn sync_rows_stamped_far_ahead_are_not_sent_and_the_vault_keeps_later_edits() {
    let now = Arc::new(std::sync::atomic::AtomicU64::new(crate::testutil::T0));
    let relay = relay_on_clock(&now);
    let (a, b) = pair(&relay);
    let shared = meeting(&a.svc, "Planning", 1);
    trade(&a, &b);
    assert_eq!(b.title(shared).as_deref(), Some("Planning"));

    // A's clock jumps, A renames the meeting and records another, and the clock is put right.
    a.clock.advance_ms(3 * DAY_MS);
    a.rename(shared, "Renamed while the clock was wrong");
    let late = meeting(&a.svc, "Recorded while the clock was wrong", 2);
    a.clock.set_ms(crate::testutil::T0 + 1_000);
    // A healthy-looking edit made after the correction is stamped past the jump too.
    let after = meeting(&a.svc, "Recorded after the clock was put right", 3);
    let queued = a.svc.store().outbox_len().unwrap();
    assert!(queued >= 17, "{queued}");

    let ca = a.client();
    let (r, outcome) = ca.tick_reporting();
    assert!(outcome.is_ok(), "the tick itself did not fail: {r:?}");
    assert_eq!((r.pushed, r.superseded), (0, 0), "{r:?}");
    assert_eq!(r.clock_ahead as u64, queued, "{r:?}");
    assert!(r.pull_complete, "it still pulled: {r:?}");
    let status = sync_status(&a.svc.store()).unwrap();
    assert_eq!(status.problem, Some(SyncProblem::ClockAhead));
    assert_eq!(
        status.last_success_unix_ms,
        Some(crate::testutil::T0 + 1_000)
    );
    assert!(!SyncProblem::ClockAhead.stops(), "it is retried");
    assert_eq!(
        a.svc.store().outbox_len().unwrap(),
        queued,
        "all still queued"
    );

    // Nothing of it reached the relay: B has the old title and neither new meeting.
    b.clock.advance_ms(5_000);
    now.store(
        crate::testutil::T0 + 5_000,
        std::sync::atomic::Ordering::SeqCst,
    );
    b.client().tick().unwrap();
    assert_eq!(b.title(shared).as_deref(), Some("Planning"));
    assert_eq!(b.title(late), None);
    assert_eq!(b.title(after), None);

    // B edits the meeting later, by a right clock: the relay takes it (no far stamp shadows
    // it) and a third computer gets B's title.
    b.rename(shared, "Renamed by a computer whose clock is right");
    let rb = b.client().tick().unwrap();
    assert_eq!((rb.pushed, rb.superseded), (1, 0), "{rb:?}");
    let c = Device::new("sync-clock-c");
    let offer = offer_pairing_code(&b.svc, b.vault()).unwrap();
    join_with_code(&c.svc, &*c.vault, &relay.relay.url(), offer.code()).unwrap();
    c.client().tick().unwrap();
    assert_eq!(
        c.title(shared).as_deref(),
        Some("Renamed by a computer whose clock is right")
    );

    // Once real time has passed the far stamps, A's rows go.
    a.clock.set_ms(crate::testutil::T0 + 3 * DAY_MS);
    now.store(
        crate::testutil::T0 + 3 * DAY_MS,
        std::sync::atomic::Ordering::SeqCst,
    );
    let (r, outcome) = ca.tick_reporting();
    assert!(outcome.is_ok(), "{r:?}");
    assert_eq!((r.clock_ahead, r.pushed as u64), (0, queued), "{r:?}");
    assert_eq!(sync_status(&a.svc.store()).unwrap().problem, None);
}

/// A computer whose clock is wrong now (three days ahead) cannot tell by itself: the relay
/// refuses its push by its own clock, with a word of its own, and stores nothing. The rows stay
/// queued and are sent once the clocks agree. A row at the edge of the window is accepted.
/// The refused computer is not cut off: the same tick goes on to pull what the other computer
/// sent, counts as a sync and leaves the word in the status.
#[test]
fn sync_a_push_the_relay_refuses_for_its_far_stamp_keeps_the_rows_queued_and_still_pulls() {
    let t0 = crate::testutil::T0;
    let now = Arc::new(std::sync::atomic::AtomicU64::new(t0));
    let relay = relay_on_clock(&now);
    let (a, b) = pair(&relay);
    // The other computer, whose clock is right, has sent a meeting.
    let theirs = meeting(&b.svc, "Recorded by a clock that is right", 2);
    b.client().tick().unwrap();
    a.clock.advance_ms(3 * DAY_MS);
    let m = meeting(&a.svc, "Recorded by a clock that is wrong now", 1);
    let queued = a.svc.store().outbox_len().unwrap();
    let ca = a.client();
    let (r, outcome) = ca.tick_reporting();
    assert!(outcome.is_ok(), "the tick went on: {r:?}");
    assert!(r.clock_refused, "{r:?}");
    assert_eq!((r.pushed, r.clock_ahead), (0, 0), "{r:?}");
    assert_eq!(
        a.svc.store().outbox_len().unwrap(),
        queued,
        "nothing was lost"
    );
    // It pulled in the same tick.
    assert!(r.pull_complete && r.written == 8, "{r:?}");
    assert_eq!(
        a.title(theirs).as_deref(),
        Some("Recorded by a clock that is right")
    );
    let status = sync_status(&a.svc.store()).unwrap();
    assert_eq!(status.problem, Some(SyncProblem::ClockAhead));
    assert!(status.last_success_unix_ms.is_some());
    b.client().tick().unwrap();
    assert_eq!(b.title(m), None, "the relay stored none of it");
    // The relay's clock at exactly a day behind the stamps: accepted.
    let newest = {
        let db = a.svc.store();
        db.outbox(1_000)
            .unwrap()
            .iter()
            .map(|r| r.meta().updated_at.wall_ms)
            .max()
            .unwrap()
    };
    assert_eq!(newest % 1000, 0, "the relay's clock counts whole seconds");
    now.store(newest - DAY_MS - 1_000, std::sync::atomic::Ordering::SeqCst);
    let (r, outcome) = ca.tick_reporting();
    assert!(
        outcome.is_ok() && r.clock_refused && r.pushed == 0,
        "a second short: {r:?}"
    );
    now.store(newest - DAY_MS, std::sync::atomic::Ordering::SeqCst);
    let (r, outcome) = ca.tick_reporting();
    assert!(outcome.is_ok(), "{r:?}");
    assert_eq!(r.pushed as u64, queued, "{r:?}");
    assert_eq!(a.svc.store().outbox_len().unwrap(), 0);
    assert_eq!(sync_status(&a.svc.store()).unwrap().problem, None);
}

/// The word the relay refuses a far stamp with is not one a 0.4.2 client knows (its list is
/// copied here from `v0.4.2:crates/earshot-sync/src/wire.rs`), and that client reads any
/// answer with an unknown word as `server`: a failed push that it retries with its rows still
/// queued (`v0.4.2:crates/earshot-runtime/src/sync/client.rs`, `call_within`). This build's
/// own handling of an unknown word is the same, pinned here.
#[test]
fn sync_the_clock_word_is_a_failed_push_to_a_client_from_before_it() {
    const V042_WORDS: [&str; 11] = [
        "bad_request",
        "unknown_device",
        "bad_secret",
        "no_slot",
        "not_found",
        "key_mismatch",
        "update_needed",
        "device_exists",
        "too_large",
        "slow_down",
        "internal",
    ];
    let word = earshot_sync::wire::ErrorWord::ClockAhead.word();
    assert_eq!(word, "clock_ahead");
    assert!(!V042_WORDS.contains(&word));
    // Every word 0.4.2 knows is still a word, with the meaning it had.
    for w in V042_WORDS {
        assert!(earshot_sync::wire::ErrorWord::from_word(w).is_some(), "{w}");
    }
    assert_eq!(
        earshot_sync::wire::ErrorWord::ALL.len(),
        V042_WORDS.len() + 1
    );
    assert_eq!(
        earshot_sync::wire::ErrorWord::from_word("a_word_from_a_later_relay"),
        None
    );
    assert_eq!(
        client::problem_of_word(earshot_sync::wire::ErrorWord::ClockAhead),
        SyncProblem::ClockAhead
    );
    assert!(!SyncProblem::Server.stops() && !SyncProblem::ClockAhead.stops());
}

/// Rows a computer holds because their stamps are far ahead of its clock (here its own clock is
/// days behind) are applied by the same running client once its clock is right, not only after
/// a restart: they are tried again when an hour has passed by its clock.
#[test]
fn sync_rows_held_for_a_far_clock_apply_once_this_clock_is_right() {
    let t0 = crate::testutil::T0;
    let now = Arc::new(std::sync::atomic::AtomicU64::new(t0 + 3 * DAY_MS));
    let relay = relay_on_clock(&now);
    let (a, b) = pair(&relay);
    // A's clock is right (the relay's); B's is three days behind.
    a.clock.set_ms(t0 + 3 * DAY_MS);
    let m = meeting(&a.svc, "Planning", 1);
    a.client().tick().unwrap();
    let cb = b.client();
    let r = cb.tick().unwrap();
    assert_eq!((r.written, r.held), (0, 8), "all held for the clock: {r:?}");
    assert_eq!(b.title(m), None);
    assert_eq!(
        b.svc
            .store()
            .held_count(&[earshot_store::HoldReason::Clock])
            .unwrap(),
        8
    );
    // Half an hour later by B's clock, still days behind: nothing changes.
    b.clock.advance_ms(30 * 60 * 1000);
    let r = cb.tick().unwrap();
    assert_eq!(r.written, 0, "{r:?}");
    // B's clock is put right. The same client applies the held rows at its next tick.
    b.clock.set_ms(t0 + 3 * DAY_MS + 60_000);
    let r = cb.tick().unwrap();
    assert_eq!(r.written, 8, "{r:?}");
    assert_eq!(b.title(m).as_deref(), Some("Planning"));
    assert_eq!(
        b.svc
            .store()
            .held_count(&earshot_store::HoldReason::ALL)
            .unwrap(),
        0
    );
    assert_eq!(a.rows(), b.rows());
}

// ---- T14.45 review: one tick at a time on a store; a replaced thread stops ----

/// Two ticks on one store never run at once: while one client's tick is in progress (here,
/// from inside it), a second client's tick is refused, and once the first has ended it runs.
/// What a tick decides from the cursor record (a changed vault, T14.45) relies on this.
#[test]
fn sync_ticks_on_one_store_never_run_at_once() {
    let relay = relay();
    let (a, b) = pair(&relay);
    meeting(&a.svc, "Planning", 1);
    let second = Arc::new(a.client());
    let refused: Arc<std::sync::Mutex<Vec<bool>>> = Arc::default();
    let first = {
        let (second, refused) = (Arc::clone(&second), Arc::clone(&refused));
        a.client().with_in_tick(Arc::new(move || {
            // Inside the first tick: the second client asks for one of its own.
            refused.lock().unwrap().push(second.try_tick().is_none());
        }))
    };
    let r = first.tick().unwrap();
    assert_eq!(r.pushed, 8, "{r:?}");
    assert_eq!(
        *refused.lock().unwrap(),
        vec![true],
        "a second tick was refused while the first ran"
    );
    // Over: the second client's tick runs, and finds the work done.
    let (r, outcome) = second.try_tick().expect("no tick is in progress now");
    assert!(outcome.is_ok() && r.pushed == 0, "{r:?}");
    // Another store's tick is not held up by this one's: each store has its own lock.
    let other = b.client();
    let free: Arc<std::sync::Mutex<Vec<bool>>> = Arc::default();
    let watching = {
        let free = Arc::clone(&free);
        a.client().with_in_tick(Arc::new(move || {
            free.lock().unwrap().push(other.try_tick().is_some());
        }))
    };
    watching.tick().unwrap();
    assert_eq!(*free.lock().unwrap(), vec![true]);
}

/// A client that has been stopped does no tick: nothing is pushed, pulled or recorded, however
/// it comes to tick (a thread that was replaced while it waited its turn).
#[test]
fn sync_a_stopped_client_does_no_tick() {
    let relay = relay();
    let (a, _b) = pair(&relay);
    meeting(&a.svc, "Planning", 1);
    let client = a.client();
    client
        .stop_flag()
        .store(true, std::sync::atomic::Ordering::Relaxed);
    let (r, outcome) = client.tick_reporting();
    assert!(outcome.is_ok());
    assert_eq!(r, TickReport::default());
    assert_eq!(a.svc.store().outbox_len().unwrap(), 8, "nothing was sent");
    assert_eq!(
        sync_status(&a.svc.store()).unwrap(),
        SyncStatus::default(),
        "nothing was recorded"
    );
    assert!(
        client.audit_log().entries().is_empty(),
        "no request was made"
    );
}

/// A host that sends each hand-back it is asked for down a channel and refuses it.
struct RefusesAndTells(std::sync::Mutex<std::sync::mpsc::Sender<()>>);

impl crate::offload::OffloadHost for RefusesAndTells {
    fn continue_run(&self, _: crate::offload::WriteHere) -> crate::offload::ContinueAnswer {
        let _ = self.0.lock().unwrap().send(());
        crate::offload::ContinueAnswer::Refused
    }
    fn write_for(&self, _: earshot_core::model::SummaryRequest) {}
}

/// A thread that was turned off goes on handing back what this computer asked for while the
/// host refuses, for up to half an hour. From the moment another thread takes its place it
/// stops: it hands nothing more back (the new thread does), and the stop that was waiting for
/// it returns.
#[test]
fn sync_a_thread_that_was_turned_off_stops_when_another_takes_its_place() {
    let relay = relay();
    let (a, b) = pair(&relay);
    let m = meeting(&a.svc, "Asked of the other computer", 60);
    crate::offload::ask(&a.svc, m, b.svc.store().device())
        .unwrap()
        .unwrap();
    let (tx, handed) = std::sync::mpsc::channel();
    let host: Arc<dyn crate::offload::OffloadHost> =
        Arc::new(RefusesAndTells(std::sync::Mutex::new(tx)));
    let long = Duration::from_secs(60);
    let thread = SyncThread::start_offloading(a.client(), quick(), Arc::clone(&host)).unwrap();
    let superseder = thread.superseder();
    assert!(!superseder.is_superseded());
    // Turned off: the stop waits while the thread hands the meeting back and is refused.
    let (stopped_tx, stopped) = std::sync::mpsc::channel();
    let stopping = std::thread::spawn(move || {
        thread.stop_saying_gone();
        let _ = stopped_tx.send(());
    });
    handed.recv_timeout(long).expect("the hand-back was tried");
    handed
        .recv_timeout(long)
        .expect("refused, it is tried again: the thread is still at it");
    assert!(
        stopped.try_recv().is_err(),
        "the stop is still waiting for the hand-backs"
    );
    // Another thread takes its place.
    superseder.supersede();
    assert!(superseder.is_superseded());
    stopped
        .recv_timeout(long)
        .expect("the superseded thread ended");
    stopping.join().unwrap();
    // The old thread has ended, so it hands nothing more back. What it sent before it ended is
    // taken off the channel here, so the hand-back heard below can only be the new thread's.
    while handed.try_recv().is_ok() {}
    // The thread that took its place owns the hand-back: its first step hands the meeting
    // back again, as a thread does after a restart.
    let next = SyncThread::start_offloading(a.client(), quick(), host).unwrap();
    handed
        .recv_timeout(long)
        .expect("the new thread handed the meeting back");
    // Superseded while it runs, it ends without being asked to hand anything back for good.
    let superseder = next.superseder();
    superseder.supersede();
    wait_for("the superseded thread ended", || next.ended());
    superseder.supersede();
}

/// Nothing that signals the sync thread waits for a presence send (T14.45 review). The thread
/// took up a resume while a gone was on the wire (the presence lock held, here by the test),
/// and used to wait for that lock with its signal lock still held: a nudge, a resume, a stop
/// request or a superseding then waited as long as the send did.
#[test]
fn sync_thread_signals_never_wait_for_a_presence_send() {
    let relay = relay();
    let (a, _b) = pair(&relay);
    let thread = SyncThread::start(a.client(), quick()).unwrap();
    wait_for("the first tick", || {
        sync_status(&a.svc.store())
            .unwrap()
            .last_success_unix_ms
            .is_some()
    });
    let beats = thread.beats_for_test();
    let superseder = thread.superseder();
    let (tx, signalled) = std::sync::mpsc::channel();
    let in_time = std::thread::scope(|scope| {
        // A presence send in flight holds the lock.
        let sending = beats.lock().unwrap();
        thread.back();
        scope.spawn(|| {
            // The thread takes the resume up, then waits for the presence lock.
            while thread.back_pending() {
                std::thread::sleep(Duration::from_millis(1));
            }
            std::thread::sleep(Duration::from_millis(50));
            thread.nudge();
            thread.back();
            superseder.supersede();
            let _ = tx.send(superseder.is_superseded());
        });
        let in_time = signalled.recv_timeout(Duration::from_secs(5));
        drop(sending);
        in_time
    });
    assert_eq!(in_time, Ok(true), "a signal waited for the presence lock");
    wait_for("the superseded thread ended", || thread.ended());
}

/// A thread started held is a thread that exists and has done nothing: a host knows it can be
/// started before it lets the threads it replaces go (T14.45 review). Stopped as it is, it ends
/// without having read or sent anything; begun, it ticks at once.
#[test]
fn sync_a_thread_started_held_does_nothing_until_it_is_begun() {
    let relay = relay();
    let (a, b) = pair(&relay);
    let m = meeting(&a.svc, "Recorded before the thread", 61);
    let queued = a.svc.store().outbox_len().unwrap();
    assert!(queued > 0);
    let untouched = |what: &str| {
        assert_eq!(a.svc.store().outbox_len().unwrap(), queued, "{what}");
        assert_eq!(
            sync_status(&a.svc.store()).unwrap(),
            SyncStatus::default(),
            "{what}"
        );
    };

    // Stopped while held: joined, and nothing was ever done.
    let held = SyncThread::start_held(a.client(), quick(), None).unwrap();
    assert!(!held.ended());
    held.stop();
    untouched("a held thread that was stopped ticked");
    // Superseded while held: the same.
    let held = SyncThread::start_held(a.client(), quick(), None).unwrap();
    held.superseder().supersede();
    wait_for("the superseded held thread ended", || held.ended());
    untouched("a held thread that was superseded ticked");
    drop(held);

    // Begun: it ticks at once.
    let thread = SyncThread::start_held(a.client(), quick(), None).unwrap();
    untouched("a held thread ticked before it was begun");
    thread.begin();
    wait_for("the begun thread sent the meeting", || {
        a.svc.store().outbox_len().unwrap() == 0
    });
    thread.begin();
    thread.stop();
    b.client().tick().unwrap();
    assert_eq!(b.title(m).as_deref(), Some("Recorded before the thread"));
}

// ---- T14.45: a relay restored from a backup is noticed ----

/// A relay whose data can be copied aside and put back, which is what restoring it from a
/// backup does: the same address and tokens, an older set of rows.
struct RestorableRelay {
    relay: Option<LocalRelay>,
    dir: TempDir,
    port: u16,
    /// The relay's clock, Unix seconds: the devices' own start, moved on by the test.
    clock: Arc<std::sync::atomic::AtomicU64>,
}

impl RestorableRelay {
    fn start() -> Self {
        let dir = TempDir::new("sync-restorable-relay");
        let clock = Arc::new(std::sync::atomic::AtomicU64::new(
            crate::testutil::T0 / 1000,
        ));
        let relay = LocalRelay::start(Self::open(&dir, &clock)).unwrap();
        let port = relay.addr().port();
        Self {
            relay: Some(relay),
            dir,
            port,
            clock,
        }
    }

    fn open(dir: &TempDir, clock: &Arc<std::sync::atomic::AtomicU64>) -> RelayServer {
        let clock = Arc::clone(clock);
        let now: earshot_sync_server::Clock =
            Arc::new(move || clock.load(std::sync::atomic::Ordering::SeqCst));
        RelayServer::open_with_clock(&relay_config(dir), now).unwrap()
    }

    /// Moves the relay's clock on by `s` seconds.
    fn advance(&self, s: u64) {
        self.clock.fetch_add(s, std::sync::atomic::Ordering::SeqCst);
    }

    fn url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    fn data(&self) -> std::path::PathBuf {
        self.dir.0.join("relay")
    }

    fn stop(&mut self) {
        self.relay = None;
    }

    fn start_again(&mut self) {
        let server = Self::open(&self.dir, &self.clock);
        self.relay = Some(LocalRelay::start_on(server, self.port).unwrap());
    }

    fn copy(from: &std::path::Path, to: &std::path::Path) {
        std::fs::create_dir_all(to).unwrap();
        for entry in std::fs::read_dir(from).unwrap() {
            let entry = entry.unwrap();
            std::fs::copy(entry.path(), to.join(entry.file_name())).unwrap();
        }
    }

    /// A copy of the relay's data as it is now.
    fn backup(&mut self) -> TempDir {
        self.stop();
        let backup = TempDir::new("sync-relay-backup");
        Self::copy(&self.data(), &backup.0);
        self.start_again();
        backup
    }

    /// The relay's data replaced by `backup`.
    fn restore(&mut self, backup: &TempDir) {
        self.stop();
        std::fs::remove_dir_all(self.data()).unwrap();
        Self::copy(&backup.0, &self.data());
        self.start_again();
    }

    /// Gives the vault a new epoch behind the relay's back, as a relay that answers oddly
    /// would (two relays behind one address).
    fn change_epoch(&self, to: u8) {
        let c = rusqlite::Connection::open(self.data().join("relay.db")).unwrap();
        c.busy_timeout(Duration::from_secs(5)).unwrap();
        c.execute("UPDATE vault SET epoch = ?1 WHERE id = 1", [vec![to; 16]])
            .unwrap();
    }
}

fn pair_on(relay: &RestorableRelay) -> (Device, Device) {
    let a = Device::new("sync-restore-a");
    let b = Device::new("sync-restore-b");
    set_up_first_device(&a.svc, &*a.vault, &relay.url(), SECRET).unwrap();
    let offer = offer_pairing_code(&a.svc, a.vault()).unwrap();
    join_with_code(&b.svc, &*b.vault, &relay.url(), offer.code()).unwrap();
    (a, b)
}

fn stored_cursor(dev: &Device) -> Cursor {
    cursor_record(&dev.svc.store()).unwrap()
}

/// Ticks `dev` until a tick neither pushes, writes nor resets (at most ten).
fn settle(dev: &Device) -> Vec<TickReport> {
    let mut reports = Vec::new();
    for _ in 0..10 {
        let r = dev.client().tick().unwrap();
        let quiet = r.pushed == 0 && r.written == 0 && !r.vault_reset;
        reports.push(r);
        if quiet {
            return reports;
        }
    }
    panic!("never settled: {reports:?}");
}

/// The relay is restored from a copy made before two computers went on syncing. Their tokens
/// still work and nothing fails, but the relay holds fewer rows than they have seen. Each
/// notices at its next tick, queues every row again and reads the vault from the start; the
/// relay ends up holding everything again, and a computer that joins afterwards gets it all.
#[test]
fn sync_a_relay_restored_from_an_older_copy_is_noticed_and_filled_again() {
    let mut relay = RestorableRelay::start();
    let (a, b) = pair_on(&relay);
    let first = meeting(&a.svc, "Before the backup", 1);
    trade(&a, &b);
    let epoch = stored_cursor(&a)
        .epoch
        .expect("the first epoch was taken in");
    assert_eq!(epoch.0.len(), 16);
    assert_eq!(stored_cursor(&b).epoch.as_ref(), Some(&epoch));
    assert!(stored_cursor(&a).resets.is_empty() && stored_cursor(&b).resets.is_empty());

    let backup = relay.backup();
    // Both go on: a new meeting on A, a rename on B, each reaching the other.
    let second = meeting(&a.svc, "After the backup", 2);
    trade(&a, &b);
    b.clock.advance_ms(5_000);
    b.rename(first, "Renamed after the backup");
    trade(&b, &a);
    assert_eq!(a.rows(), b.rows());
    let before = a.rows();
    assert_eq!(
        stored_cursor(&a).epoch.as_ref(),
        Some(&epoch),
        "a restart changes nothing"
    );
    assert!(stored_cursor(&a).resets.is_empty());

    relay.restore(&backup);

    // A's next tick: nothing fails; it notices and starts over.
    let r = a.client().tick().unwrap();
    assert!(r.vault_reset, "{r:?}");
    let after_reset = stored_cursor(&a);
    assert_eq!(after_reset.pull_seq, 0);
    assert_ne!(
        after_reset.epoch.as_ref(),
        Some(&epoch),
        "the relay took a new epoch"
    );
    assert_eq!(after_reset.resets.len(), 1);
    assert_eq!(
        a.svc.store().outbox_len().unwrap(),
        16,
        "every row of both meetings is queued again"
    );
    // B holds the old epoch and so does the same, by itself.
    let r = b.client().tick().unwrap();
    assert!(r.vault_reset, "{r:?}");
    assert_eq!(stored_cursor(&b).epoch, after_reset.epoch);

    for dev in [&a, &b, &a, &b] {
        let reports = settle(dev);
        assert!(reports.iter().all(|r| !r.vault_reset), "{reports:?}");
    }
    assert_eq!(a.rows(), before, "nothing was lost or changed here");
    assert_eq!(b.rows(), before);
    assert_eq!(stored_cursor(&a).resets.len(), 1, "once");
    assert_eq!(stored_cursor(&b).resets.len(), 1);
    assert_eq!(sync_status(&a.svc.store()).unwrap().problem, None);

    // The relay holds everything again: a computer joining now gets what the copy lacked.
    let c = Device::new("sync-restore-c");
    let offer = offer_pairing_code(&a.svc, a.vault()).unwrap();
    join_with_code(&c.svc, &*c.vault, &relay.url(), offer.code()).unwrap();
    settle(&c);
    assert_eq!(c.title(second).as_deref(), Some("After the backup"));
    assert_eq!(c.title(first).as_deref(), Some("Renamed after the backup"));
    assert_eq!(c.rows(), before);
}

/// The relay takes a new epoch at most once in ten minutes, and a restore inside that time is
/// still filled again: each computer that saw more than the relay now holds starts over by
/// itself, in the epoch it has. Past the ten minutes a restore is a new epoch again.
#[test]
fn sync_a_restore_soon_after_another_is_filled_again_without_a_new_epoch() {
    let mut relay = RestorableRelay::start();
    let (a, b) = pair_on(&relay);
    meeting(&a.svc, "Before the first copy", 1);
    trade(&a, &b);
    let first_copy = relay.backup();
    meeting(&a.svc, "After the first copy", 2);
    trade(&a, &b);
    // The first restore: one new epoch, both start over.
    relay.restore(&first_copy);
    for dev in [&a, &b] {
        let r = dev.client().tick().unwrap();
        assert!(r.vault_reset, "{r:?}");
    }
    for dev in [&a, &b, &a] {
        settle(dev);
    }
    let second = stored_cursor(&a)
        .epoch
        .expect("the epoch taken for the restore");
    assert_eq!(stored_cursor(&b).epoch.as_ref(), Some(&second));

    // A copy made now, more rows, and a minute later the relay is put back to it.
    let second_copy = relay.backup();
    b.clock.advance_ms(5_000);
    meeting(&b.svc, "After the second copy", 3);
    trade(&b, &a);
    let before = a.rows();
    assert_eq!(b.rows(), before);
    relay.advance(60);
    relay.restore(&second_copy);
    for dev in [&a, &b] {
        let r = dev.client().tick().unwrap();
        assert!(r.vault_reset, "by the numbers alone: {r:?}");
        assert_eq!(
            stored_cursor(dev).epoch.as_ref(),
            Some(&second),
            "no further epoch inside the ten minutes"
        );
    }
    for dev in [&a, &b, &a] {
        let reports = settle(dev);
        assert!(reports.iter().all(|r| !r.vault_reset), "{reports:?}");
    }
    assert_eq!((a.rows(), b.rows()), (before.clone(), before.clone()));
    let c = Device::new("sync-restore-twice-c");
    let offer = offer_pairing_code(&a.svc, a.vault()).unwrap();
    join_with_code(&c.svc, &*c.vault, &relay.url(), offer.code()).unwrap();
    settle(&c);
    assert_eq!(c.rows(), before, "the relay holds everything again");

    // The same copy put back once the ten minutes are over: a new epoch.
    relay.advance(earshot_sync_server::CLAIM_EVERY_S);
    relay.restore(&second_copy);
    let r = a.client().tick().unwrap();
    assert!(r.vault_reset, "{r:?}");
    let third = stored_cursor(&a).epoch.expect("an epoch");
    assert_ne!(third, second);
    let r = b.client().tick().unwrap();
    assert!(r.vault_reset, "{r:?}");
    assert_eq!(stored_cursor(&b).epoch.as_ref(), Some(&third));
    for dev in [&a, &b, &a] {
        settle(dev);
    }
    assert_eq!((a.rows(), b.rows()), (before.clone(), before));
    assert_eq!(stored_cursor(&a).resets.len(), MAX_VAULT_RESETS);
}

/// A computer updated from a build before the epoch, whose record holds only its cursor, still
/// notices a relay restored while it was away: its cursor is past everything the relay holds.
#[test]
fn sync_a_computer_from_before_the_epoch_notices_a_restored_relay_by_its_cursor() {
    let mut relay = RestorableRelay::start();
    let (a, b) = pair_on(&relay);
    meeting(&a.svc, "Before the backup", 1);
    trade(&a, &b);
    let backup = relay.backup();
    let second = meeting(&a.svc, "After the backup", 2);
    trade(&a, &b);
    let before = b.rows();
    // B's record as a 0.4.2 build left it: the cursor alone.
    let cursor = stored_cursor(&b).pull_seq;
    {
        let mut db = b.svc.store();
        crate::local::set(
            &mut db,
            &DeviceRecord::SyncCursor.key(),
            &encode_cursor(cursor),
        )
        .unwrap();
    }
    relay.restore(&backup);
    let r = b.client().tick().unwrap();
    assert!(r.vault_reset, "{r:?}");
    assert_eq!(b.svc.store().outbox_len().unwrap(), 16);
    settle(&b);
    // The relay has B's copy of everything again: a new computer gets the later meeting.
    let c = Device::new("sync-restore-upgraded-c");
    let offer = offer_pairing_code(&b.svc, b.vault()).unwrap();
    join_with_code(&c.svc, &*c.vault, &relay.url(), offer.code()).unwrap();
    settle(&c);
    assert_eq!(c.title(second).as_deref(), Some("After the backup"));
    assert_eq!(c.rows(), before);
}

/// What the restore handling rests on (T14.45, third review), through the real relay: a 0.4.2
/// client announces envelope format 2, and this release pushes format 4. From the first push of
/// a device of this release the 0.4.2 client is refused `update_needed` on every pull and
/// push, wherever its cursor is, and nothing it says is heard: the devices of this release see
/// the same epoch and the same numbering afterwards. The same holds after the relay is put back
/// to a copy from before that push: a device of this release notices, starts over and pushes,
/// which raises the format again, and the 0.4.2 client is refused as before. So no row a device
/// of this release files after a restore is ever owed to a 0.4.2 client.
#[test]
fn sync_a_0_4_2_client_is_locked_out_once_this_release_has_pushed_restore_or_not() {
    use earshot_sync::wire::{B64, PushRequest, WireRow};
    let mut relay = RestorableRelay::start();
    let (a, b) = pair_on(&relay);
    let c = Device::new("sync-locked-out-c");
    let offer = offer_pairing_code(&a.svc, a.vault()).unwrap();
    join_with_code(&c.svc, &*c.vault, &relay.url(), offer.code()).unwrap();
    // C's calls as 0.4.2 makes them (`v0.4.2:crates/earshot-runtime/src/sync/client.rs` and
    // `engine.rs`): format 2 in the header and in a push, a pull that names its cursor and a
    // limit and nothing else, a push that names no epoch and nothing seen. A client for each
    // call, as each tick has (the relay is stopped and started between them here).
    let url = relay.url();
    let old = || {
        client::Relay::new(check_address(&url).unwrap(), Arc::clone(&c.svc.clock))
            .unwrap()
            .announcing(2)
    };
    let token = c.vault.load().unwrap().unwrap().token;
    let old_pull = |after: u64| {
        let record = Cursor {
            pull_seq: after,
            ..Cursor::default()
        };
        assert_eq!(client::pull_query(after, 200, &record).len(), 2);
        old().pull(&token, after, 200, &record)
    };
    let old_push = || {
        old().push(
            &token,
            &PushRequest {
                format: 2,
                rows: vec![WireRow {
                    key: B64(vec![0x42; 16]),
                    wall_ms: crate::testutil::T0,
                    logical: 0,
                    node: B64(vec![9; 16]),
                    body: B64(b"an invented sealed row".to_vec()),
                }],
                seen_seq: None,
                epoch: None,
            },
        )
    };
    let refused = |what: &str| {
        for after in [0, 1, 8, 16, 17, 10_000, u64::MAX] {
            assert_eq!(
                old_pull(after).err(),
                Some(SyncProblem::UpdateNeeded),
                "{what}: a pull after {after}"
            );
        }
        assert_eq!(
            old_push().err(),
            Some(SyncProblem::UpdateNeeded),
            "{what}: a push"
        );
    };

    // No device of this release has pushed a row: the 0.4.2 client is let in.
    let answer = old_pull(0).expect("let in while the vault's format is its own");
    assert!(answer.vault_format <= 2, "{}", answer.vault_format);
    let backup = relay.backup();

    // A, of this release, records and syncs: the vault's format is 4.
    meeting(&a.svc, "Recorded on this release", 1);
    trade(&a, &b);
    let before = a.rows();
    assert_eq!(b.rows(), before);
    let settled = (stored_cursor(&a), stored_cursor(&b));
    refused("after the first push of this release");
    // Nothing it said was heard: the same epoch and the same numbering for A and B.
    for dev in [&a, &b] {
        let r = dev.client().tick().unwrap();
        assert!(!r.vault_reset, "{r:?}");
        assert_eq!((r.pushed, r.written), (0, 0), "{r:?}");
    }
    assert_eq!((stored_cursor(&a), stored_cursor(&b)), settled);

    // The relay is put back to the copy from before that push. A notices and starts over; its
    // push raises the format again.
    relay.restore(&backup);
    let r = a.client().tick().unwrap();
    assert!(r.vault_reset, "{r:?}");
    settle(&a);
    let r = b.client().tick().unwrap();
    assert!(r.vault_reset, "{r:?}");
    settle(&b);
    settle(&a);
    let settled = (stored_cursor(&a), stored_cursor(&b));
    assert_ne!(settled.0.epoch, None);
    assert_eq!(settled.0.epoch, settled.1.epoch, "one restore, one epoch");
    refused("after the restore and the next push of this release");
    for dev in [&a, &b] {
        let r = dev.client().tick().unwrap();
        assert!(!r.vault_reset, "{r:?}");
        assert_eq!((r.pushed, r.written), (0, 0), "{r:?}");
    }
    assert_eq!((stored_cursor(&a), stored_cursor(&b)), settled);
    assert_eq!((a.rows(), b.rows()), (before.clone(), before));
}

/// Reading the vault again from a restored relay never brings back what was deleted here: the
/// copy still holds a meeting both computers deleted afterwards, live, with its lines. It stays
/// deleted on both, and the relay is told again.
#[test]
fn sync_a_restored_relay_does_not_bring_back_a_meeting_deleted_since() {
    let mut relay = RestorableRelay::start();
    let (a, b) = pair_on(&relay);
    let gone = meeting(&a.svc, "Deleted after the backup", 1);
    let kept = meeting(&a.svc, "Kept", 2);
    trade(&a, &b);
    let backup = relay.backup();
    // The meeting as the copy holds it: live, with its lines.
    let live_rows = meeting_rows(&a, gone);
    a.clock.advance_ms(5_000);
    a.svc.delete_meeting(gone).unwrap();
    trade(&a, &b);
    assert_eq!((a.title(gone), b.title(gone)), (None, None));

    relay.restore(&backup);
    for dev in [&b, &a, &b, &a] {
        settle(dev);
    }
    assert_eq!((a.title(gone), b.title(gone)), (None, None));
    // The copy's live rows as a pulled page of their own (a tick pushes before it pulls, so
    // through the relay they arrive after this computer's tombstones went out again): none is
    // written on either computer.
    for dev in [&a, &b] {
        let applied = apply_page(dev, &live_rows);
        assert_eq!(applied.written, 0, "{applied:?}");
    }
    assert_eq!((a.title(gone), b.title(gone)), (None, None));
    for dev in [&a, &b] {
        let db = dev.svc.store();
        assert!(
            db.segments(gone)
                .unwrap()
                .iter()
                .all(|s| s.meta.deleted && s.segment.text.is_empty()),
            "no line of the deleted meeting came back"
        );
        assert!(db.meeting(gone).unwrap().unwrap().meta.deleted);
    }
    assert_eq!(a.title(kept).as_deref(), Some("Kept"));
    assert_eq!(a.rows(), b.rows());
    // A computer that joins now is given the deletion, not the copy's live meeting.
    let c = Device::new("sync-restore-deleted-c");
    let offer = offer_pairing_code(&a.svc, a.vault()).unwrap();
    join_with_code(&c.svc, &*c.vault, &relay.url(), offer.code()).unwrap();
    settle(&c);
    assert_eq!(c.title(gone), None);
    assert_eq!(c.title(kept).as_deref(), Some("Kept"));
}

/// A relay that keeps naming another epoch (two relays behind one address) makes this computer
/// start over a bounded number of times: three in a day, then the tick fails with a word it
/// retries on, and nothing is queued or reset until a day has passed.
#[test]
fn sync_a_relay_that_keeps_changing_its_epoch_resets_this_computer_a_bounded_number_of_times() {
    let relay = RestorableRelay::start();
    let (a, _b) = pair_on(&relay);
    meeting(&a.svc, "Planning", 1);
    settle(&a);
    assert_eq!(a.svc.store().outbox_len().unwrap(), 0);
    for n in 0..MAX_VAULT_RESETS {
        relay.change_epoch(0x10 + u8::try_from(n).unwrap());
        a.clock.advance_ms(60_000);
        let r = a.client().tick().unwrap();
        assert!(r.vault_reset, "reset {n}: {r:?}");
        settle(&a);
    }
    assert_eq!(stored_cursor(&a).resets.len(), MAX_VAULT_RESETS);
    let settled = stored_cursor(&a);

    // A fourth and a fifth change inside the day: reported, and nothing else happens.
    for n in 0..2u8 {
        relay.change_epoch(0x40 + n);
        a.clock.advance_ms(60_000);
        let (r, outcome) = a.client().tick_reporting();
        assert_eq!(outcome, Err(SyncProblem::Server), "{r:?}");
        assert!(!r.vault_reset);
        assert_eq!(
            a.svc.store().outbox_len().unwrap(),
            0,
            "nothing queued again"
        );
        assert_eq!(stored_cursor(&a), settled, "the record is as it was");
    }
    assert_eq!(
        sync_status(&a.svc.store()).unwrap().problem,
        Some(SyncProblem::Server)
    );
    assert!(!SyncProblem::Server.stops(), "it goes on trying");

    // A day after the first of the three, one more is allowed.
    a.clock.advance_ms(VAULT_RESET_WINDOW_MS);
    let r = a.client().tick().unwrap();
    assert!(r.vault_reset, "{r:?}");
    assert!(stored_cursor(&a).resets.len() <= MAX_VAULT_RESETS);
}

/// A computer updated from a build before the epoch has a cursor and no epoch. It takes the
/// first epoch the relay names without reading anything again or queueing anything, keeps its
/// cursor, and its record is then more than the old one; a record without any of the new parts
/// is byte for byte the old one.
#[test]
fn sync_a_computer_from_before_the_epoch_takes_the_first_one_without_starting_over() {
    let relay = RestorableRelay::start();
    let (a, b) = pair_on(&relay);
    meeting(&a.svc, "Planning", 1);
    trade(&a, &b);
    let now = stored_cursor(&b);
    assert!(now.pull_seq > 0 && now.epoch.is_some());
    // B's record as a 0.4.2 build wrote it: the cursor alone.
    {
        let mut db = b.svc.store();
        crate::local::set(
            &mut db,
            &DeviceRecord::SyncCursor.key(),
            &encode_cursor(now.pull_seq),
        )
        .unwrap();
        assert_eq!(
            db.local_value(SYNC_CURSOR_KEY).unwrap().unwrap(),
            format!("\u{1}\0\0\0{{\"pull_seq\":{}}}", now.pull_seq).into_bytes()
        );
    }
    let r = b.client().tick().unwrap();
    assert!(!r.vault_reset, "{r:?}");
    assert_eq!((r.pushed, r.written), (0, 0), "{r:?}");
    assert_eq!(b.svc.store().outbox_len().unwrap(), 0);
    let after = stored_cursor(&b);
    assert_eq!(after.pull_seq, now.pull_seq, "the cursor is kept");
    assert_eq!(after.epoch, now.epoch, "the epoch is taken in");
    assert!(after.resets.is_empty());
    // The full record's bytes, pinned.
    let full = Cursor {
        pull_seq: 7,
        seen_seq: 9,
        epoch: Some(earshot_sync::wire::B64(vec![1; 16])),
        resets: vec![5, 6],
    };
    assert_eq!(
        encode_cursor_record(&full),
        b"\x01\x00\x00\x00{\"pull_seq\":7,\"seen_seq\":9,\"epoch\":\"AQEBAQEBAQEBAQEBAQEBAQ==\",\"resets\":[5,6]}"
    );
    assert_eq!(decode_cursor(Some(encode_cursor_record(&full))), full);
    // Turning a switch on starts the pull over and keeps what is known of the vault.
    set_sync_voiceprints(&b.svc, true).unwrap();
    let restarted = stored_cursor(&b);
    assert_eq!(restarted.pull_seq, 0);
    assert_eq!(
        (restarted.epoch, restarted.seen_seq),
        (after.epoch, after.seen_seq)
    );
}

/// The decision itself, without a relay (T14.45): what each kind of answer means against each
/// stored record. A relay from before the epoch names none and no number from before the
/// request; the client then goes by the push answer's own highest number.
#[test]
fn sync_vault_answers_are_judged_by_epoch_and_by_what_was_seen() {
    let b64 = |n: u8| earshot_sync::wire::B64(vec![n; 16]);
    let stored = Cursor {
        pull_seq: 40,
        seen_seq: 50,
        epoch: Some(b64(1)),
        resets: Vec::new(),
    };
    let now = 1_000_000;
    const SAME: &[u8] = &[1u8; 16];
    const OTHER: &[u8] = &[2u8; 16];
    let (same, other) = (SAME, OTHER);
    let seen = |epoch: Option<&&'static [u8]>, before: Option<u64>, now: Option<u64>| VaultSeen {
        epoch: epoch.copied(),
        before,
        now,
    };
    // The same epoch and at least as many rows as seen: nothing to do, or a higher number.
    assert_eq!(
        judge_vault(&stored, seen(Some(&same), Some(50), Some(50)), now),
        VaultVerdict::Same(None)
    );
    assert_eq!(
        judge_vault(&stored, seen(Some(&same), Some(50), Some(53)), now),
        VaultVerdict::Same(Some(Cursor {
            seen_seq: 53,
            ..stored.clone()
        }))
    );
    // Another epoch: start over with it, whatever the numbers say.
    assert_eq!(
        judge_vault(&stored, seen(Some(&other), Some(90), Some(91)), now),
        VaultVerdict::Reset(Cursor {
            pull_seq: 0,
            seen_seq: 91,
            epoch: Some(b64(2)),
            resets: vec![now],
        })
    );
    // The same epoch but fewer rows than seen: start over.
    assert_eq!(
        judge_vault(&stored, seen(Some(&same), Some(49), Some(49)), now),
        VaultVerdict::Reset(Cursor {
            pull_seq: 0,
            seen_seq: 49,
            epoch: Some(b64(1)),
            resets: vec![now],
        })
    );
    // A relay from before the epoch (0.4.2): no epoch in its answers. A pull says nothing; a
    // push answer's highest number does (what `send` derives `before` from).
    assert_eq!(
        judge_vault(&stored, seen(None, None, None), now),
        VaultVerdict::Same(None)
    );
    assert_eq!(
        judge_vault(&stored, seen(None, Some(60), Some(62)), now),
        VaultVerdict::Same(Some(Cursor {
            seen_seq: 62,
            ..stored.clone()
        }))
    );
    assert_eq!(
        judge_vault(&stored, seen(None, Some(10), Some(12)), now),
        VaultVerdict::Reset(Cursor {
            pull_seq: 0,
            seen_seq: 12,
            epoch: Some(b64(1)),
            resets: vec![now],
        })
    );
    // Never seen an epoch: the first is taken in without starting over,
    let fresh = Cursor {
        pull_seq: 40,
        seen_seq: 0,
        ..Cursor::default()
    };
    assert_eq!(
        judge_vault(&fresh, seen(Some(&other), Some(44), Some(44)), now),
        VaultVerdict::Same(Some(Cursor {
            pull_seq: 40,
            seen_seq: 44,
            epoch: Some(b64(2)),
            resets: Vec::new(),
        }))
    );
    // unless the numbers already say rows were lost. A record from before the epoch holds only
    // its cursor, and the cursor is itself a number this device has seen.
    let upgraded = Cursor {
        pull_seq: 40,
        ..Cursor::default()
    };
    assert_eq!(upgraded.seen(), 40);
    assert_eq!(
        judge_vault(&upgraded, seen(Some(&other), Some(20), Some(40)), now),
        VaultVerdict::Reset(Cursor {
            pull_seq: 0,
            seen_seq: 40,
            epoch: Some(b64(2)),
            resets: vec![now],
        })
    );
    // Three resets inside a day are the limit; older ones, and ones dated after now (the clock
    // was set back), do not count.
    let day = VAULT_RESET_WINDOW_MS;
    let busy = Cursor {
        resets: vec![now + day - 3, now + day - 2, now + day - 1],
        ..stored.clone()
    };
    assert_eq!(
        judge_vault(&busy, seen(Some(&other), Some(90), Some(91)), now + day),
        VaultVerdict::TooOften
    );
    let VaultVerdict::Reset(next) = judge_vault(
        &busy,
        seen(Some(&other), Some(90), Some(91)),
        now + 2 * day - 3,
    ) else {
        panic!("a day after the first of three, one more is allowed");
    };
    assert_eq!(
        next.resets,
        vec![now + day - 2, now + day - 1, now + 2 * day - 3]
    );
    let VaultVerdict::Reset(next) = judge_vault(&busy, seen(Some(&other), Some(90), Some(91)), now)
    else {
        panic!("resets dated after now do not count");
    };
    assert_eq!(next.resets, vec![now]);
    assert_eq!(MAX_VAULT_RESETS, 3);
}

/// T14.50: the convergence repairs run for the kinds a page wrote, and a client's first page
/// runs them all. Two live "Me" people are written here by hand (no shipped code leaves them),
/// so the difference shows.
#[test]
fn sync_a_clients_first_page_runs_every_repair_and_later_pages_only_their_own() {
    let relay = relay();
    let (a, b) = pair(&relay);
    let c = Device::new("sync-c");
    let offer = offer_pairing_code(&a.svc, a.vault()).unwrap();
    join_with_code(&c.svc, &*c.vault, &relay.relay.url(), offer.code()).unwrap();
    let (ca, cb, cc) = (a.client(), b.client(), c.client());
    let put_me = |dev: &Device, n: u8| {
        let clock = Arc::clone(&dev.svc.clock);
        let mut db = dev.svc.store();
        let (store, hlc) = db.parts();
        let at = hlc.now(&*clock);
        store
            .put_person(earshot_core::voiceprint::Person {
                id: earshot_core::identity::EntityId::from_bytes([
                    0x01, 0x90, 0, 0, 0, 0, 0x70, 0, 0x80, 0, 0, 0, 0, 0, n, 0x5e,
                ]),
                meta: earshot_core::identity::SyncMeta {
                    created_at: at,
                    updated_at: at,
                    deleted: false,
                    origin_device_id: at.node,
                },
                display_name: "An invented user".into(),
                is_me: true,
                merged_into: None,
            })
            .unwrap();
    };
    let live_mes = |dev: &Device| {
        dev.svc
            .store()
            .persons()
            .unwrap()
            .into_iter()
            .filter(|p| p.is_me && p.is_live())
            .count()
    };
    let shared = meeting(&b.svc, "Planning", 1);
    cb.tick().unwrap();
    let third = meeting(&c.svc, "Review", 2);
    cc.tick().unwrap();
    put_me(&a, 1);
    a.clock.advance_ms(10);
    put_me(&a, 2);
    assert_eq!(live_mes(&a), 2);
    // This client's first page holds the other computer's meeting and no person row written
    // here: every repair runs all the same.
    let first = ca.tick().unwrap();
    assert!(first.written > 0, "{first:?}");
    assert_eq!(first.repaired, 1, "{first:?}");
    assert_eq!(live_mes(&a), 1);
    // A later page of the same client that writes no person row runs no "Me" repair.
    a.clock.advance_ms(10);
    put_me(&a, 3);
    b.clock.advance_ms(1_000);
    b.rename(shared, "Planning, renamed");
    cb.tick().unwrap();
    let later = ca.tick().unwrap();
    assert!(later.written > 0, "{later:?}");
    assert_eq!(later.repaired, 0, "{later:?}");
    assert_eq!(a.title(shared).as_deref(), Some("Planning, renamed"));
    assert_eq!(live_mes(&a), 2, "left for a page that writes a person row");
    // A new client (the next start of sync) goes over everything on its first page. That page
    // holds a rename from the third computer and no person row newer than the copy here: a
    // tick pushes before it pulls, so neither other computer has sent a repair of its own.
    c.clock.advance_ms(1_000);
    c.rename(third, "Review, renamed");
    cc.tick().unwrap();
    let restarted = a.client();
    let again = restarted.tick().unwrap();
    assert!(again.written > 0, "{again:?}");
    assert_eq!(again.repaired, 1, "{again:?}");
    assert_eq!(live_mes(&a), 1);
    // Everything settles the same on both.
    for _ in 0..3 {
        restarted.tick().unwrap();
        cb.tick().unwrap();
        cc.tick().unwrap();
    }
    assert_eq!(a.rows(), b.rows());
    assert_eq!(a.rows(), c.rows());
    assert_eq!(live_mes(&b), 1);
}

/// T14.50: the list of computers is asked through a client the host already has (the sync
/// thread's), the same list as a client made for the ask gives, also while that client's tick
/// is in progress and holds the store's tick lock, and with ticks and asks at once.
#[test]
fn sync_the_device_list_is_asked_through_a_client_beside_its_ticks() {
    let relay = relay();
    let (a, b) = pair(&relay);
    set_device_name(&b.svc, "Invented laptop").unwrap();
    b.client().send_presence(false).unwrap();
    let seen_ignored = |mut list: Vec<SyncDevice>| {
        for d in &mut list {
            d.seen_s_ago = 0;
        }
        list
    };
    let afresh = seen_ignored(vault_devices(&a.svc, a.vault()).unwrap());
    assert_eq!(afresh.len(), 2);
    assert_eq!(afresh[1].name.as_deref(), Some("Invented laptop"));

    // The client's own tick asks for the list from another thread, as a page's command does,
    // while the tick is in progress.
    let this: Arc<std::sync::OnceLock<std::sync::Weak<SyncClient>>> = Arc::default();
    let inside: Arc<std::sync::Mutex<Vec<Result<Vec<SyncDevice>, SyncProblem>>>> = Arc::default();
    let client = {
        let (this, inside) = (Arc::clone(&this), Arc::clone(&inside));
        Arc::new(a.client().with_in_tick(Arc::new(move || {
            let client = this.get().and_then(std::sync::Weak::upgrade);
            let Some(client) = client else { return };
            let asked = std::thread::spawn(move || client.vault_devices())
                .join()
                .expect("the ask did not panic");
            inside.lock().unwrap().push(asked);
        })))
    };
    this.set(Arc::downgrade(&client)).unwrap();
    assert_eq!(seen_ignored(client.vault_devices().unwrap()), afresh);
    meeting(&a.svc, "Planning", 1);
    client.tick().unwrap();
    let asked_inside: Vec<_> = inside
        .lock()
        .unwrap()
        .drain(..)
        .map(|r| r.map(seen_ignored))
        .collect();
    assert_eq!(
        asked_inside,
        vec![Ok(afresh.clone())],
        "asked while a tick held the store's tick lock"
    );

    // Ticks on one thread and asks on another, at once, through the one client.
    let ticking = {
        let client = Arc::clone(&client);
        std::thread::spawn(move || {
            for _ in 0..15 {
                client.tick().unwrap();
            }
        })
    };
    for _ in 0..15 {
        assert_eq!(seen_ignored(client.vault_devices().unwrap()), afresh);
    }
    ticking.join().unwrap();
    // The asks inside those ticks all answered too.
    assert_eq!(inside.lock().unwrap().len(), 15);
    assert!(inside.lock().unwrap().iter().all(Result::is_ok));
    assert_only_the_relay(&client, &relay);
}

#[path = "sync_edit_tests.rs"]
mod edits;
