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
