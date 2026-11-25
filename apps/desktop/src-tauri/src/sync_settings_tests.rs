//! T11.3a: the Sync page's functions against the real relay, in process. Every name holds
//! `sync` (`cargo test -p earshot-desktop sync`).

use std::sync::Arc;
use std::time::{Duration, Instant};

use earshot_runtime::sync::{MemoryVault, SyncProblem, SyncTiming, SyncVault, check_address};
use earshot_sync_server::{Config, LocalRelay, Relay};

use super::*;
use crate::testutil::{TempDir, services};

const SECRET: &str = "an-invented-setup-secret-for-tests";

fn table() -> serde_json::Value {
    serde_json::from_str(include_str!("../../../../fixtures/ui/sync_copy_cases.json")).unwrap()
}

struct Env {
    relay: LocalRelay,
    _relay_dir: TempDir,
}

fn relay() -> Env {
    let dir = TempDir::new("sync-page-relay");
    let config = Config {
        setup_secret: SECRET.into(),
        data: dir.0.join("relay"),
        listen: "127.0.0.1:0".parse().unwrap(),
    };
    Env {
        relay: LocalRelay::start(Relay::open(&config).unwrap()).unwrap(),
        _relay_dir: dir,
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
        presence: Duration::from_millis(50),
    };
    let host = SyncHost::with_timing(Arc::clone(&vault) as Arc<dyn SyncVault>, timing);
    Computer {
        svc,
        host,
        vault,
        _dir: dir,
    }
}

/// Every string a secret could appear in, from what the page receives.
fn assert_holds_no_secret(json: &str, secrets: &[&str]) {
    for s in secrets {
        assert!(!json.contains(s), "a response carried a secret: {json}");
    }
}

#[test]
fn sync_copy_matches_the_shared_table_for_every_problem() {
    let table = table();
    let problems = table["problems"].as_object().unwrap();
    assert_eq!(problems.len(), SyncProblem::ALL.len());
    for p in SyncProblem::ALL {
        assert_eq!(problems[p.word()].as_str(), Some(problem_copy(p)), "{p:?}");
        assert!(!problem_copy(p).contains("  "), "{p:?}");
    }
    // A button press that failed has its own copy: nothing retries it.
    let commands = table["command_problems"].as_object().unwrap();
    assert_eq!(commands.len(), SyncProblem::ALL.len());
    for p in SyncProblem::ALL {
        assert_eq!(commands[p.word()].as_str(), Some(command_copy(p)), "{p:?}");
        assert!(
            !command_copy(p).contains("will try again"),
            "a refused command must not promise a retry: {p:?}"
        );
    }
    let stops: Vec<&str> = table["stops"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    let expected: Vec<&str> = SyncProblem::ALL
        .into_iter()
        .filter(|p| p.stops())
        .map(SyncProblem::word)
        .collect();
    assert_eq!(
        stops, expected,
        "the table's stopping words are the runtime's"
    );
    let empty = set_up(
        &computer("sync-copy").svc,
        &computer("sync-copy-2").host,
        "https://sync.example.com",
        "  ",
    )
    .unwrap_err();
    assert_eq!(
        Some(empty.message.as_str()),
        table["secret_missing"].as_str()
    );
    assert_eq!(
        Some(super::UNSENDABLE_NOTE),
        table["unsendable_note"].as_str()
    );
}

/// T14.43: the note for rows the last sync could not seal is the table's, with the count, and
/// the page's view carries it only when there are some.
#[test]
fn sync_refused_note_is_the_tables_copy_with_the_count() {
    let table = table();
    let note = &table["refused_note"];
    assert_eq!(refused_note(0), None);
    assert_eq!(refused_note(1).as_deref(), note["one"].as_str());
    let many = note["many"].as_str().unwrap();
    assert!(many.starts_with("{count} items "), "{many}");
    assert_eq!(refused_note(2), Some(many.replace("{count}", "2")));
    assert_eq!(refused_note(1_204), Some(many.replace("{count}", "1204")));
    for text in [refused_note(1).unwrap(), refused_note(7).unwrap()] {
        assert!(!text.contains('{') && !text.contains("  "), "{text}");
    }

    let c = computer("sync-refused-view");
    let overview = |refused: u64| {
        let mut o = earshot_runtime::sync::sync_overview(&c.svc, &*c.vault).unwrap();
        o.status.refused = refused;
        o
    };
    assert_eq!(view_of(overview(0), false).refused_note, None);
    assert_eq!(
        view_of(overview(3), false).refused_note,
        Some(many.replace("{count}", "3"))
    );
    let json = serde_json::to_string(&view_of(overview(1), false)).unwrap();
    assert!(json.contains("\"refused_note\":\"1 item "), "{json}");
}

#[test]
fn sync_address_refusals_match_the_shared_table() {
    for case in table()["address_cases"].as_array().unwrap() {
        let input = case["input"].as_str().unwrap();
        let got = check_address(input).err().map(|r| r.copy());
        assert_eq!(got, case["copy"].as_str(), "{input:?}");
    }
}

#[test]
fn sync_set_up_join_and_remove_through_the_page_carry_no_secret() {
    let env = relay();
    let a = computer("sync-page-a");
    let b = computer("sync-page-b");
    let url = env.relay.url();

    let before = view(&a.svc, &a.host).unwrap();
    assert!(!before.set_up && !before.on && !before.running);
    assert_eq!(before.address, None);

    let first = set_up(&a.svc, &a.host, &url, SECRET).unwrap();
    assert!(first.set_up && first.on && first.running, "{first:?}");
    assert_eq!(first.address.as_deref(), Some(url.as_str()));

    let code = offer_code(&a.svc, &a.host).unwrap();
    assert!(code.expires_in_s > 0);
    let joined = join(&b.svc, &b.host, &url, &code.code).unwrap();
    assert!(joined.set_up && joined.running);

    // The code is used: joining with it again is refused, with fixed copy.
    let c = computer("sync-page-c");
    let again = join(&c.svc, &c.host, &url, &code.code).unwrap_err();
    assert_eq!(again.message, problem_copy(SyncProblem::NoSlot));

    // Both threads reach the server; the device list names this computer first.
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let v = view(&b.svc, &b.host).unwrap();
        if v.last_success_unix_ms.is_some() {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the joined computer never synced"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    let list = devices(&b.svc, &b.host).unwrap();
    assert_eq!(list.len(), 2);
    assert!(list[0].this_device);
    // The other computer is listed by the name in its presence, online.
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let list = devices(&b.svc, &b.host).unwrap();
        let other = list.iter().find(|d| !d.this_device).unwrap();
        if other.online {
            assert_eq!(
                other.name.as_deref(),
                Some(earshot_runtime::sync::computer_name().as_str())
            );
            assert!(!other.recording);
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the other computer never showed online"
        );
        std::thread::sleep(Duration::from_millis(20));
    }

    // No response holds the key, the token, the setup secret, or the code once used.
    let key_hex: String = a
        .vault
        .load()
        .unwrap()
        .unwrap()
        .key
        .as_bytes()
        .iter()
        .map(|x| format!("{x:02x}"))
        .collect();
    let token = a.vault.load().unwrap().unwrap().token.as_str().to_owned();
    let responses = [
        serde_json::to_string(&first).unwrap(),
        serde_json::to_string(&joined).unwrap(),
        serde_json::to_string(&list).unwrap(),
        serde_json::to_string(&view(&a.svc, &a.host).unwrap()).unwrap(),
        serde_json::to_string(&again).unwrap(),
    ];
    for json in &responses {
        assert_holds_no_secret(json, &[SECRET, &token, &key_hex, &code.code]);
    }

    // Off stops the thread and keeps everything; on starts it again.
    let off = set_on(&b.svc, &b.host, false).unwrap();
    assert!(off.set_up && !off.on && !off.running);
    let on = set_on(&b.svc, &b.host, true).unwrap();
    assert!(on.on && on.running);
    let vp = set_voiceprints(&b.svc, &b.host, true).unwrap();
    assert!(vp.voiceprints);
    // Dictionary sync is on by default once sync is on (T11.3e), and turns off and on.
    assert!(vp.dictionary);
    let dict = set_dictionary(&b.svc, &b.host, false).unwrap();
    assert!(!dict.dictionary && dict.voiceprints);
    assert!(set_dictionary(&b.svc, &b.host, true).unwrap().dictionary);

    // Removing it stops the thread and forgets the key, token and address.
    let gone = remove(&b.svc, &b.host).unwrap();
    assert!(
        !gone.set_up && !gone.running && gone.address.is_none(),
        "{gone:?}"
    );
    assert!(
        !gone.voiceprints,
        "joining again asks for voiceprint sync afresh"
    );
    assert!(!b.vault.has().unwrap());
}

#[test]
fn sync_remove_other_copy_matches_the_shared_table() {
    let table = table();
    let copy = table["remove_other"].as_object().unwrap();
    assert_eq!(copy.len(), 5, "every key is read here: {copy:?}");
    assert_eq!(copy["not_another"].as_str(), Some(NOT_ANOTHER_DEVICE));
    for key in ["button", "title"] {
        assert!(copy[key].as_str().unwrap().contains("{device}"), "{key}");
    }
    assert_eq!(copy["confirm"].as_str(), Some("Remove"));
    // The owner's decision (T11.6c): the dialog says plainly that the key is not taken back
    // and that a lost computer should be wiped too.
    let explained = copy["explained"].as_str().unwrap();
    for words in [
        "does not take back the sync key",
        "also wipe it",
        "stay on your other",
    ] {
        assert!(explained.contains(words), "{words}");
    }
    for v in copy.values() {
        let s = v.as_str().unwrap();
        assert!(!s.contains('\u{2014}') && !s.contains("  "), "{s}");
    }
}

#[test]
fn sync_removing_another_computer_from_its_row() {
    let env = relay();
    let a = computer("sync-other-a");
    let b = computer("sync-other-b");
    let url = env.relay.url();
    set_up(&a.svc, &a.host, &url, SECRET).unwrap();
    let code = offer_code(&a.svc, &a.host).unwrap();
    join(&b.svc, &b.host, &url, &code.code).unwrap();
    let list = devices(&a.svc, &a.host).unwrap();
    assert_eq!(list.len(), 2);
    let me = list.iter().find(|d| d.this_device).unwrap().id.clone();
    let other = list.iter().find(|d| !d.this_device).unwrap().id.clone();
    assert_eq!(other.len(), 36, "{other}");
    assert!(other.starts_with(&list[1].short_id));

    // This computer, or an id that is not one, is refused with fixed copy and nothing sent.
    for id in [me.as_str(), "", "not-an-id", &other[..30], "zz"] {
        let e = remove_other(&a.svc, &a.host, id).unwrap_err();
        assert_eq!(
            (e.kind.as_str(), e.message.as_str()),
            ("invalid_setting", NOT_ANOTHER_DEVICE),
            "{id:?}"
        );
    }
    assert_eq!(devices(&a.svc, &a.host).unwrap().len(), 2);

    // The other computer: gone from the list returned, and its thread stops on the answer it
    // gets next, saying this computer was removed.
    let left = remove_other(&a.svc, &a.host, &other.to_uppercase())
        .unwrap()
        .devices
        .unwrap();
    assert_eq!(left.len(), 1);
    assert!(left[0].this_device);
    b.host.nudge();
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let v = view(&b.svc, &b.host).unwrap();
        if v.stopped {
            assert_eq!(
                v.problem.as_deref(),
                Some(problem_copy(SyncProblem::UnknownDevice))
            );
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the removed computer never stopped: {v:?}"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    // It can no longer remove the first one: it hears that it was removed.
    let e = remove_other(&b.svc, &b.host, &me).unwrap_err();
    assert_eq!(e.message, command_copy(SyncProblem::UnknownDevice));
    assert!(view(&a.svc, &a.host).unwrap().set_up);
    a.host.stop_and_wait();
    b.host.stop_and_wait();
}

#[test]
fn sync_removing_another_computer_is_a_success_when_the_list_then_fails() {
    let env = relay();
    let a = computer("sync-other-list-a");
    let b = computer("sync-other-list-b");
    let url = env.relay.url();
    set_up(&a.svc, &a.host, &url, SECRET).unwrap();
    let code = offer_code(&a.svc, &a.host).unwrap();
    join(&b.svc, &b.host, &url, &code.code).unwrap();
    let other = devices(&a.svc, &a.host)
        .unwrap()
        .into_iter()
        .find(|d| !d.this_device)
        .unwrap()
        .id;
    // The revoke happened; the list read after it fails: still a success, with no list.
    let done = remove_other_then(&a.svc, &a.host, &other, || {
        Err(sync_error(SyncProblem::Unreachable))
    })
    .unwrap();
    assert_eq!(done, SyncRemovedView { devices: None });
    let left = devices(&a.svc, &a.host).unwrap();
    assert_eq!(left.len(), 1, "the removal happened");
    // A refused revoke is still an error, and the list is never read.
    let e = remove_other_then(&b.svc, &b.host, &a_id(&a), || -> Result<_, IpcError> {
        panic!("no list after a refused removal")
    })
    .unwrap_err();
    assert_eq!(e.message, command_copy(SyncProblem::UnknownDevice));
    a.host.stop_and_wait();
    b.host.stop_and_wait();
}

fn a_id(c: &Computer) -> String {
    c.svc.store().device().to_string()
}

#[test]
fn sync_page_refusals_are_fixed_copy_and_store_nothing() {
    let env = relay();
    let a = computer("sync-page-refuse");
    let bad = set_up(
        &a.svc,
        &a.host,
        &env.relay.url(),
        "not-the-secret-at-all-here",
    )
    .unwrap_err();
    assert_eq!(bad.kind, SYNC_ERROR_KIND);
    assert_eq!(bad.message, problem_copy(SyncProblem::BadSecret));
    // A server that is down: the refusal says what to do, not that Earshot will retry.
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let down = set_up(&a.svc, &a.host, &format!("http://127.0.0.1:{port}"), SECRET).unwrap_err();
    assert_eq!(down.message, command_copy(SyncProblem::Unreachable));
    let addr = set_up(&a.svc, &a.host, "http://sync.example.com", SECRET).unwrap_err();
    assert_eq!(addr.kind, "invalid_setting");
    let code = join(&a.svc, &a.host, &env.relay.url(), "nope").unwrap_err();
    assert_eq!(code.message, problem_copy(SyncProblem::BadCode));
    let v = view(&a.svc, &a.host).unwrap();
    assert!(!v.set_up && !v.running && v.address.is_none());
    assert!(!a.vault.has().unwrap());
}

#[test]
fn sync_host_starts_only_when_on_and_set_up() {
    let env = relay();
    let a = computer("sync-host");
    a.host.start_if_on(&a.svc);
    assert!(!a.host.running(), "nothing set up: no thread");
    set_up(&a.svc, &a.host, &env.relay.url(), SECRET).unwrap();
    assert!(a.host.running());
    // A fresh host over the same data (the app starting again) starts it from the settings.
    let again = SyncHost::with_timing(
        Arc::clone(&a.vault) as Arc<dyn SyncVault>,
        SyncTiming::default(),
    );
    again.start_if_on(&a.svc);
    assert!(again.running());
    again.stop_and_wait();
    assert!(!again.running());
    // Sync now starts a thread that should run and does not.
    again.sync_now(&a.svc);
    assert!(again.running());
    again.stop_and_wait();
    earshot_runtime::sync::set_sync_on(&a.svc, false).unwrap();
    again.start_if_on(&a.svc);
    assert!(!again.running(), "turned off: no thread at the next start");
    a.host.stop_and_wait();
}

/// T14.45 review: a thread that was turned off may still be finishing; it is told it has been
/// replaced only when a new thread really starts, which then owns what it had left to do.
#[test]
fn sync_host_tells_a_turned_off_thread_when_a_new_one_takes_its_place() {
    let env = relay();
    let a = computer("sync-host-superseded");
    set_up(&a.svc, &a.host, &env.relay.url(), SECRET).unwrap();
    assert!(a.host.retired_for_test().is_empty());
    // Turned off: the thread is remembered, not yet replaced.
    set_on(&a.svc, &a.host, false).unwrap();
    let off = a.host.retired_for_test();
    assert_eq!(off.len(), 1);
    assert!(!off[0].is_superseded());
    // A start that starts nothing (sync is still off) leaves it at its work.
    a.host.start_if_on(&a.svc);
    assert!(!a.host.running());
    assert!(!off[0].is_superseded());
    assert_eq!(a.host.retired_for_test().len(), 1);
    // Turned on, but no thread can be started: nothing runs, so nothing has taken its place.
    // It is still at its work and still remembered (the review's failure path: told too
    // early, nobody would own what it had left to hand back until the next start).
    a.host.refuse_start_for_test(true);
    set_on(&a.svc, &a.host, true).unwrap();
    assert!(!a.host.running(), "the thread could not be started");
    assert!(!off[0].is_superseded());
    assert_eq!(a.host.retired_for_test().len(), 1);
    // The next start gets its thread: a new thread runs, and the old one has been told.
    a.host.refuse_start_for_test(false);
    a.host.start_if_on(&a.svc);
    assert!(a.host.running());
    assert!(off[0].is_superseded());
    assert!(a.host.retired_for_test().is_empty());
    a.host.stop_and_wait();
}

/// T14.45 (third review): a start that cannot get a thread leaves the running thread in
/// place. It is not told it was replaced and not stopped: it goes on syncing, and so goes on
/// owning the hand-backs of what this computer asked for. The next start that gets a thread
/// tells it, and only then.
#[test]
fn sync_host_keeps_the_running_thread_when_a_new_one_cannot_be_started() {
    let env = relay();
    let a = computer("sync-host-kept");
    set_up(&a.svc, &a.host, &env.relay.url(), SECRET).unwrap();
    let running = a.host.running_superseder_for_test().expect("a thread runs");
    let synced = |what: &str| {
        let deadline = Instant::now() + Duration::from_secs(60);
        while a.svc.store().outbox_len().unwrap() != 0 {
            assert!(Instant::now() < deadline, "{what}");
            std::thread::sleep(Duration::from_millis(10));
        }
    };
    synced("the first thread never synced");

    // A restart is asked for and no thread can be had.
    a.host.refuse_start_for_test(true);
    for _ in 0..2 {
        a.host.start_if_on(&a.svc);
        assert!(a.host.running(), "the thread in place is still there");
        assert!(!running.is_superseded(), "and was told nothing");
        assert!(a.host.retired_for_test().is_empty());
    }
    // It still does the work: a meeting recorded now is sent by it.
    titled_meeting(&a.svc, "Recorded while no thread could be started");
    assert!(a.svc.store().outbox_len().unwrap() > 0);
    synced("the thread left in place no longer syncs");
    assert!(!running.is_superseded());

    // The next start gets its thread: the old one is told, and the new one is in place.
    a.host.refuse_start_for_test(false);
    a.host.start_if_on(&a.svc);
    assert!(running.is_superseded());
    assert!(a.host.running());
    let next = a
        .host
        .running_superseder_for_test()
        .expect("the new thread");
    assert!(!next.is_superseded());
    titled_meeting(&a.svc, "Recorded after the restart");
    synced("the new thread never synced");
    a.host.stop_and_wait();
}

/// T12.93: the one-off copy of legacy ignores runs at start only on a computer with no vault;
/// with a vault it waits for the sync thread's first completed pull, even with sync off.
#[test]
fn sync_host_copies_ignores_at_start_only_without_a_vault() {
    let env = relay();
    let lone = computer("sync-ignore-lone");
    assert!(lone.host.copy_ignores_without_vault(&lone.svc));
    assert!(earshot_runtime::ignore_copy::copied(&lone.svc.store()).unwrap());

    let a = computer("sync-ignore-vault");
    set_up(&a.svc, &a.host, &env.relay.url(), SECRET).unwrap();
    a.host.stop_and_wait();
    earshot_runtime::sync::set_sync_on(&a.svc, false).unwrap();
    assert!(
        !a.host.copy_ignores_without_vault(&a.svc),
        "a vault: wait for a pull"
    );
    assert!(!earshot_runtime::ignore_copy::copied(&a.svc.store()).unwrap());
    // Turned on: the thread's first completed pull runs it.
    earshot_runtime::sync::set_sync_on(&a.svc, true).unwrap();
    a.host.start_if_on(&a.svc);
    let deadline = Instant::now() + Duration::from_secs(60);
    while !earshot_runtime::ignore_copy::copied(&a.svc.store()).unwrap() {
        assert!(Instant::now() < deadline, "the copy never ran after a pull");
        std::thread::sleep(Duration::from_millis(10));
    }
    a.host.stop_and_wait();
}

#[test]
fn sync_event_carries_meeting_ids_only() {
    let id = earshot_core::identity::EntityId::from_bytes([7; 16]);
    let e = sync_event(&SyncChanges {
        meetings: vec![id],
        people: false,
    });
    let json = serde_json::to_value(&e).unwrap();
    assert_eq!(
        json,
        serde_json::json!({ "meetings": [id.to_string()], "people": false })
    );
    // A change outside any meeting (people, folders) still says something, naming none.
    let people = serde_json::to_value(sync_event(&SyncChanges {
        meetings: vec![],
        people: true,
    }))
    .unwrap();
    assert_eq!(
        people,
        serde_json::json!({ "meetings": [], "people": true })
    );
}

/// A meeting with a title, made on `svc`'s device.
fn titled_meeting(svc: &Services, title: &str) -> earshot_core::identity::EntityId {
    use earshot_core::model::{Meeting, MeetingRepo, Stamp};
    use earshot_core::prompt::Template;
    use rand_chacha::ChaCha8Rng;
    use rand_core::SeedableRng;
    let clock = Arc::clone(&svc.clock);
    let mut rng = ChaCha8Rng::seed_from_u64(81);
    let mut db = svc.store();
    let (store, hlc) = db.parts();
    let mut stamp = Stamp {
        clock: &*clock,
        hlc,
        rng: &mut rng,
    };
    let mut m = Meeting::new(title, 1_800_000_000_000, 600, Template::General, &mut stamp);
    m.duration_ms = Some(60_000);
    let id = m.id;
    store.put_meeting(m).unwrap();
    id
}

#[test]
fn sync_host_tells_the_window_which_meetings_a_tick_changed() {
    let env = relay();
    let a = computer("sync-changes-a");
    let b = computer("sync-changes-b");
    let heard: Arc<std::sync::Mutex<Vec<String>>> = Arc::default();
    let sink = Arc::clone(&heard);
    b.host.set_change_listener(Arc::new(move |c: &SyncChanges| {
        sink.lock()
            .unwrap()
            .push(serde_json::to_string(&sync_event(c)).unwrap());
    }));
    let url = env.relay.url();
    set_up(&a.svc, &a.host, &url, SECRET).unwrap();
    let code = offer_code(&a.svc, &a.host).unwrap();
    join(&b.svc, &b.host, &url, &code.code).unwrap();

    let title = "An invented planning meeting";
    let m = titled_meeting(&a.svc, title);
    a.host.nudge();
    let want = serde_json::json!({ "meetings": [m.to_string()], "people": false }).to_string();
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        if heard.lock().unwrap().contains(&want) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "never told: {:?}",
            heard.lock().unwrap()
        );
        b.host.nudge();
        std::thread::sleep(Duration::from_millis(20));
    }
    // Every event is ids only: no title, ever.
    for e in heard.lock().unwrap().iter() {
        assert!(!e.contains(title), "{e}");
        let v: serde_json::Value = serde_json::from_str(e).unwrap();
        let keys: Vec<&String> = v.as_object().unwrap().keys().collect();
        assert_eq!(keys, ["meetings", "people"], "{e}");
    }
    a.host.stop_and_wait();
    b.host.stop_and_wait();
}

/// Whether `b` lists the other computer as online, within `limit`.
fn sync_other_online_within(b: &Computer, want: bool, limit: Duration) -> bool {
    let deadline = Instant::now() + limit;
    loop {
        let online = devices(&b.svc, &b.host)
            .unwrap()
            .iter()
            .find(|d| !d.this_device)
            .is_some_and(|d| d.online);
        if online == want {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// T14.50: the open Sync page asks for the computers every few seconds. No ask makes a client
/// of its own (each has its own HTTP client and runtime): with a sync thread in place the
/// thread's client is used, without one a single client is kept for the pages, and it never
/// outlives the address it was made for.
#[test]
fn sync_devices_are_asked_through_the_threads_client_or_one_kept_for_the_pages() {
    let env = relay();
    let a = computer("sync-page-client");
    // Not set up: said before any client is made.
    let not_set_up = devices(&a.svc, &a.host).unwrap_err();
    assert_eq!(not_set_up.message, command_copy(SyncProblem::Vault));
    assert_eq!(a.host.spares_made_for_test(), 0);
    set_up(&a.svc, &a.host, &env.relay.url(), SECRET).unwrap();
    assert!(a.host.running());
    // A thread is in place: its client answers, ask after ask, beside its ticks.
    for _ in 0..5 {
        let list = devices(&a.svc, &a.host).unwrap();
        assert_eq!(list.len(), 1);
        assert!(list[0].this_device);
    }
    assert_eq!(
        a.host.spares_made_for_test(),
        0,
        "the thread's client was used"
    );
    // Turned off: no thread. The first ask makes the pages' client, the later ones use it.
    set_on(&a.svc, &a.host, false).unwrap();
    assert!(!a.host.running());
    for _ in 0..5 {
        assert_eq!(devices(&a.svc, &a.host).unwrap().len(), 1);
    }
    assert_eq!(a.host.spares_made_for_test(), 1, "one client for five asks");
    // The Summaries page's list of computers asks the same way.
    let helpers = crate::offload::helpers_view(&a.svc, &a.host).unwrap();
    assert!(!helpers.sync_ready, "sync is off: nothing is asked");
    // Turned on again: the thread's client again, and the pages' own is dropped with the start.
    set_on(&a.svc, &a.host, true).unwrap();
    assert!(a.host.running());
    assert_eq!(devices(&a.svc, &a.host).unwrap().len(), 1);
    assert!(
        crate::offload::helpers_view(&a.svc, &a.host)
            .unwrap()
            .sync_ready
    );
    assert_eq!(a.host.spares_made_for_test(), 1);
    set_on(&a.svc, &a.host, false).unwrap();
    assert_eq!(devices(&a.svc, &a.host).unwrap().len(), 1);
    assert_eq!(a.host.spares_made_for_test(), 2, "made anew after a start");
    // Removed: the pages' client goes with the address, and the list has nothing to ask.
    remove(&a.svc, &a.host).unwrap();
    let removed = devices(&a.svc, &a.host).unwrap_err();
    assert_eq!(removed.message, command_copy(SyncProblem::Vault));
    assert_eq!(a.host.spares_made_for_test(), 2);
    // Set up again, at another relay, and off: the pages' client is the new address's.
    let other = relay();
    set_up(&a.svc, &a.host, &other.relay.url(), SECRET).unwrap();
    set_on(&a.svc, &a.host, false).unwrap();
    assert_eq!(devices(&a.svc, &a.host).unwrap().len(), 1);
    assert_eq!(a.host.spares_made_for_test(), 3);
    a.host.stop_and_wait();
}

/// T14.50 review: a page's client built while this computer is removed is not kept for the
/// address being left (the removal moves the pages' generation on). Control: built with nothing
/// in between, it is kept.
#[test]
fn sync_a_page_client_built_across_a_removal_is_not_kept() {
    let env = relay();
    let a = computer("sync-page-client-remove");
    set_up(&a.svc, &a.host, &env.relay.url(), SECRET).unwrap();
    set_on(&a.svc, &a.host, false).unwrap();
    assert!(!a.host.running());
    // Control: nothing happens between building and keeping.
    a.host.on_spare_built_for_test(Box::new(|| {}));
    assert_eq!(devices(&a.svc, &a.host).unwrap().len(), 1);
    assert!(a.host.keeps_page_client_for_test(), "the control kept it");
    // A start drops it; the next ask is built while this computer is removed.
    set_on(&a.svc, &a.host, true).unwrap();
    set_on(&a.svc, &a.host, false).unwrap();
    assert!(!a.host.keeps_page_client_for_test());
    let made = a.host.spares_made_for_test();
    let (built_tx, built_rx) = std::sync::mpsc::channel::<()>();
    let (go_tx, go_rx) = std::sync::mpsc::channel::<()>();
    a.host.on_spare_built_for_test(Box::new(move || {
        built_tx.send(()).unwrap();
        go_rx.recv_timeout(Duration::from_secs(60)).unwrap();
    }));
    std::thread::scope(|s| {
        let ask = s.spawn(|| devices(&a.svc, &a.host).map(|d| d.len()));
        built_rx
            .recv_timeout(Duration::from_secs(60))
            .expect("the ask never built a client");
        remove(&a.svc, &a.host).unwrap();
        go_tx.send(()).unwrap();
        // The ask's own answer does not matter: the address it asked is gone.
        let _ = ask.join().unwrap();
    });
    assert_eq!(a.host.spares_made_for_test(), made + 1, "the ask built one");
    assert!(
        !a.host.keeps_page_client_for_test(),
        "a client built across the removal was kept"
    );
    a.host.stop_and_wait();
}

#[cfg(windows)]
#[test]
fn sync_sleep_says_gone_at_once_and_resume_says_online_again() {
    use crate::power_win::{PowerChange, handle};
    let env = relay();
    let a = computer("sync-sleep-a");
    let b = computer("sync-sleep-b");
    let url = env.relay.url();
    set_up(&a.svc, &a.host, &url, SECRET).unwrap();
    let code = offer_code(&a.svc, &a.host).unwrap();
    join(&b.svc, &b.host, &url, &code.code).unwrap();
    assert!(
        sync_other_online_within(&b, true, Duration::from_secs(60)),
        "the first computer never showed online"
    );
    // Suspend: the handler returns once gone is sent (or its wait ends), and the other
    // computer reads it offline at once, not after the 150 s online window (2 s allows for a
    // beat that crossed the suspend and the gone sent after it).
    handle(&a.host, PowerChange::Suspend);
    assert!(
        sync_other_online_within(&b, false, Duration::from_secs(2)),
        "a sleeping computer still reads online"
    );
    // It stays offline across several beat intervals: no beats while asleep.
    std::thread::sleep(Duration::from_millis(300));
    assert!(sync_other_online_within(&b, false, Duration::ZERO));
    handle(&a.host, PowerChange::Resume);
    assert!(
        sync_other_online_within(&b, true, Duration::from_secs(60)),
        "a resumed computer never said it was back"
    );
    a.host.stop_and_wait();
    b.host.stop_and_wait();
}
