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
        voiceprints: false,
        leased: &never,
        now_ms: dev.svc.clock.now_unix_ms(),
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
    let key = earshot_sync::VaultKey::generate().unwrap();
    let keys = key.keys();
    let device = earshot_core::identity::DeviceId::from_bytes([7; 16]);
    let widest = "\u{1F600}".repeat(MAX_DEVICE_NAME);
    let mut sizes = std::collections::BTreeSet::new();
    for (name, recording, gone) in [
        ("A", false, false),
        ("A", true, false),
        ("A much longer computer name here", true, true),
        (widest.as_str(), false, false),
    ] {
        let p = presence::presence_now(name.to_owned(), true, recording, gone);
        let blob = presence::blob_of(&p, &keys, device).unwrap();
        sizes.insert(blob.len());
        let back = presence::open_blob(&keys, device, &blob).unwrap();
        assert_eq!((back.recording, back.gone), (recording, gone));
        assert_eq!(back.name, clean_name(name).unwrap());
    }
    assert_eq!(sizes.len(), 1, "every blob is one size: {sizes:?}");
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
