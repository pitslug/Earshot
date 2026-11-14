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
