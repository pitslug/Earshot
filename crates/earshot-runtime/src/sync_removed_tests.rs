//! T14.49d (docs/sync-spec.md 7A.2 C1, 7A.4, 7A.9, 7A.11): a removed person or folder stays
//! removed, as the client sees it through the real relay. Every name and id is invented; push
//! requests are counted from the client's own audit log (`POST /v1/rows`), one entry per request.

use std::sync::atomic::Ordering;

use earshot_core::identity::{Clock, EntityId, SyncMeta};
use earshot_core::model::{FolderRepo, MeetingRepo};
use earshot_core::voiceprint::{Person, VoiceprintRepo};

use super::*;
use crate::edit::PersonChoice;
use crate::sync::engine::{MAX_PUSH_PAGES, PUSH_PAGE};

/// Push requests `c` has made so far.
fn pushes(c: &SyncClient) -> usize {
    c.audit_log()
        .entries()
        .iter()
        .filter(|e| e.method == "POST" && e.path == "/v1/rows")
        .count()
}

/// The ticks' push requests: `f`'s, counted at `c`.
fn pushes_in<T>(c: &SyncClient, f: impl FnOnce() -> T) -> (T, usize) {
    let before = pushes(c);
    let out = f();
    (out, pushes(c) - before)
}

fn speaker_named(dev: &Device, m: EntityId, name: &str) -> EntityId {
    dev.svc
        .store()
        .speakers(m)
        .unwrap()
        .into_iter()
        .find(|s| s.display_name == name)
        .unwrap_or_else(|| panic!("no speaker {name}"))
        .id
}

fn speaker_row(dev: &Device, m: EntityId, id: EntityId) -> earshot_core::model::Speaker {
    dev.svc
        .store()
        .speakers(m)
        .unwrap()
        .into_iter()
        .find(|s| s.id == id)
        .unwrap()
}

/// A speaker row's `person_id`, read by SQL (the row may sit beside a damaged one).
fn person_id_by_sql(dev: &Device, speaker: EntityId) -> Option<Vec<u8>> {
    let c = rusqlite::Connection::open(db_path(dev)).unwrap();
    c.query_row(
        "SELECT person_id FROM speaker WHERE id = ?1",
        rusqlite::params![speaker.as_uuid().as_bytes().to_vec()],
        |r| r.get(0),
    )
    .unwrap()
}

fn person_row(dev: &Device, p: EntityId) -> Option<Person> {
    dev.svc.store().person(p).unwrap()
}

fn removed(dev: &Device, p: EntityId) -> bool {
    person_row(dev, p).is_some_and(|r| r.meta.deleted && r.display_name.is_empty())
}

fn folder_removed(dev: &Device, f: EntityId) -> bool {
    dev.svc
        .store()
        .folder(f)
        .unwrap()
        .is_some_and(|r| r.meta.deleted && r.name.is_empty())
}

/// A meeting on `a` whose Alice is linked to a new person `name`; both devices hold it.
fn named_person(a: &Device, b: &Device, name: &str, seed: u64) -> (EntityId, EntityId, EntityId) {
    let m = meeting(&a.svc, "Planning", seed);
    let alice = speaker_named(a, m, "Alice");
    let p = crate::edit::link_speaker(&a.svc, m, alice, &PersonChoice::New { name: name.into() })
        .unwrap()
        .person;
    trade(a, b);
    (m, alice, p)
}

/// A device joining the vault through `first`, its clock where `first`'s is, and syncing from
/// the start to the end of what the relay holds.
fn join(relay: &TestRelay, first: &Device, tag: &str) -> Device {
    let c = Device::new(tag);
    c.clock.set_ms(first.clock.now_unix_ms());
    let offer = offer_pairing_code(&first.svc, first.vault()).unwrap();
    join_with_code(&c.svc, &*c.vault, &relay.relay.url(), offer.code()).unwrap();
    let client = c.client();
    for _ in 0..10 {
        if client.tick().unwrap().pull_complete {
            return c;
        }
    }
    panic!("the joining device never read to the end");
}

/// C1, the person case, through a superseded tombstone: B's later rename reaches the relay
/// first, A's earlier tombstone is answered `superseded` and leaves A's outbox, A's pull stamps
/// it again and the same tick's extra push stores it. One tick of A is enough: B ends removed,
/// its speaker unlinked. A's tick made exactly two push requests and left nothing queued.
#[test]
fn sync_removed_c1_a_superseded_person_tombstone_is_stamped_again_and_sent_in_the_same_tick() {
    let relay = relay();
    let (a, b) = pair(&relay);
    let (ca, cb) = (a.client(), b.client());
    let (m, alice, p) = named_person(&a, &b, "Robin Invented", 1);
    a.clock.advance_ms(1_000);
    crate::edit::remove_person(&a.svc, p).unwrap();
    b.clock.advance_ms(5_000);
    crate::edit::rename_person(&b.svc, p, "Robin Renamed").unwrap();
    cb.tick().unwrap();
    let (r, n) = pushes_in(&ca, || ca.tick().unwrap());
    assert_eq!(n, 2, "the first push and the extra push: {r:?}");
    assert!(r.extra_push && r.tombstones_restamped == 1, "{r:?}");
    assert_eq!(a.svc.store().outbox_len().unwrap(), 0, "the tombstone went");
    assert!(removed(&a, p));
    cb.tick().unwrap();
    assert!(removed(&b, p), "B ends removed after one tick of A");
    assert_eq!(speaker_row(&b, m, alice).person_id, None);
    // Nothing loops: further ticks stamp nothing again.
    for c in [&ca, &cb, &ca] {
        let r = c.tick().unwrap();
        assert_eq!((r.tombstones_restamped, r.extra_push), (0, false), "{r:?}");
    }
}

/// C1 for a folder tombstone (F1).
#[test]
fn sync_removed_c1_a_folder_tombstone_is_stamped_again_and_sent_in_the_same_tick() {
    let relay = relay();
    let (a, b) = pair(&relay);
    let (ca, cb) = (a.client(), b.client());
    let f = crate::folders::create(&a.svc, "Invented projects").unwrap();
    trade(&a, &b);
    a.clock.advance_ms(1_000);
    crate::folders::delete_keeping_meetings(&a.svc, f).unwrap();
    b.clock.advance_ms(5_000);
    crate::folders::rename(&b.svc, f, "Invented projects, renamed").unwrap();
    cb.tick().unwrap();
    let (r, n) = pushes_in(&ca, || ca.tick().unwrap());
    assert_eq!(n, 2, "{r:?}");
    assert_eq!(a.svc.store().outbox_len().unwrap(), 0);
    cb.tick().unwrap();
    assert!(folder_removed(&b, f));
}

/// C1 for a meeting tombstone: B renamed the meeting after A deleted it; one tick of A deletes
/// it on B too.
#[test]
fn sync_removed_c1_a_meeting_tombstone_is_stamped_again_and_sent_in_the_same_tick() {
    let relay = relay();
    let (a, b) = pair(&relay);
    let (ca, cb) = (a.client(), b.client());
    let m = meeting(&a.svc, "Planning", 3);
    trade(&a, &b);
    a.clock.advance_ms(1_000);
    a.svc.delete_meeting(m).unwrap();
    b.clock.advance_ms(5_000);
    b.rename(m, "Planning, renamed later");
    cb.tick().unwrap();
    let (r, n) = pushes_in(&ca, || ca.tick().unwrap());
    assert_eq!(n, 2, "{r:?}");
    assert!(r.tombstones_restamped >= 1, "{r:?}");
    assert_eq!(a.svc.store().outbox_len().unwrap(), 0);
    cb.tick().unwrap();
    assert_eq!(b.title(m), None, "deleted on B");
}

/// Several tombstones stamped again in one tick still make exactly two push requests; a tick
/// that stamped nothing again makes exactly one.
#[test]
fn sync_removed_c1_push_requests_are_one_or_exactly_two() {
    let relay = relay();
    let (a, b) = pair(&relay);
    let (ca, cb) = (a.client(), b.client());
    let (_, _, p) = named_person(&a, &b, "Robin Invented", 4);
    let f = crate::folders::create(&a.svc, "Invented projects").unwrap();
    trade(&a, &b);
    // Nothing stamped again: one request for one queued change.
    meeting(&a.svc, "Another", 5);
    let (r, n) = pushes_in(&ca, || ca.tick().unwrap());
    assert_eq!(
        (n, r.extra_push, r.tombstones_restamped),
        (1, false, 0),
        "{r:?}"
    );
    cb.tick().unwrap();
    a.clock.advance_ms(1_000);
    crate::edit::remove_person(&a.svc, p).unwrap();
    crate::folders::delete_keeping_meetings(&a.svc, f).unwrap();
    b.clock.advance_ms(5_000);
    crate::edit::rename_person(&b.svc, p, "Robin Renamed").unwrap();
    crate::folders::rename(&b.svc, f, "Renamed").unwrap();
    cb.tick().unwrap();
    let (r, n) = pushes_in(&ca, || ca.tick().unwrap());
    assert_eq!((n, r.tombstones_restamped), (2, 2), "{r:?}");
    cb.tick().unwrap();
    assert!(removed(&b, p) && folder_removed(&b, f));
}

/// A page that only skipped a live child of a meeting deleted here (`deletions_kept` above 0,
/// nothing stamped again) makes no extra push.
#[test]
fn sync_removed_c1_a_skipped_live_child_of_a_deleted_meeting_makes_no_extra_push() {
    let relay = relay();
    let (a, b) = pair(&relay);
    let (ca, cb) = (a.client(), b.client());
    let m = meeting(&a.svc, "Planning", 6);
    trade(&a, &b);
    let alice = speaker_named(&a, m, "Alice");
    a.clock.advance_ms(1_000);
    a.svc.delete_meeting(m).unwrap();
    ca.tick().unwrap();
    b.clock.advance_ms(5_000);
    crate::edit::rename_speaker(&b.svc, m, alice, "Alicia").unwrap();
    cb.tick().unwrap();
    let (r, n) = pushes_in(&ca, || ca.tick().unwrap());
    assert!(r.pulled > 0, "the live child came: {r:?}");
    assert_eq!(
        (n, r.extra_push, r.tombstones_restamped),
        (0, false, 0),
        "{r:?}"
    );
}

/// A7, the start-of-tick retry page and P1's second case: a person tombstone held `clock` on B
/// (A's clock was more than a day ahead), B renames meanwhile and its rename replaces the
/// tombstone at the relay; once B's clock has come within a day of the stamp the hold applies,
/// P1 writes it with B's stamp above the rename, and C1's extra push of that same tick sends it.
/// Nothing applied before that tick. A device that joins afterwards gets the tombstone.
#[test]
fn sync_removed_held_tombstone_applies_and_its_new_stamp_leaves_in_that_tick() {
    let relay = relay();
    let (a, b) = pair(&relay);
    let (ca, cb) = (a.client(), b.client());
    let (m, alice, p) = named_person(&a, &b, "Robin Invented", 7);
    a.clock.advance_ms(DAY_MS + 3_600_000);
    crate::edit::remove_person(&a.svc, p).unwrap();
    ca.tick().unwrap();
    let r = cb.tick().unwrap();
    assert!(r.held > 0, "{r:?}");
    assert!(
        !removed(&b, p),
        "a held row defends nothing until it applies"
    );
    b.clock.advance_ms(DAY_MS + 2 * 3_600_000);
    crate::edit::rename_person(&b.svc, p, "Robin Renamed").unwrap();
    let (r, n) = pushes_in(&cb, || cb.tick().unwrap());
    assert!(removed(&b, p), "{r:?}");
    assert_eq!((n, r.extra_push), (2, true), "{r:?}");
    assert_eq!(b.svc.store().outbox_len().unwrap(), 0, "{r:?}");
    assert_eq!(speaker_row(&b, m, alice).person_id, None);
    let c = join(&relay, &a, "sync-removed-c");
    assert!(removed(&c, p), "the relay holds the tombstone");
}

/// C1 with a longer outbox than one tick's pushes carry (`MAX_PUSH_PAGES` x `PUSH_PAGE`): the
/// first push sends the oldest 10,000 rows, the pull stamps the tombstone again and the extra
/// push sends it by id though older rows are still queued.
#[test]
fn sync_removed_c1_a_long_outbox_still_sends_the_tombstone_stamped_again() {
    let relay = relay();
    let (a, b) = pair(&relay);
    let (ca, cb) = (a.client(), b.client());
    let (_, _, p) = named_person(&a, &b, "Robin Invented", 8);
    a.clock.advance_ms(1_000);
    crate::edit::remove_person(&a.svc, p).unwrap();
    ca.tick().unwrap();
    b.clock.advance_ms(5_000);
    crate::edit::rename_person(&b.svc, p, "Robin Renamed").unwrap();
    cb.tick().unwrap();
    let extra = MAX_PUSH_PAGES * PUSH_PAGE + 50;
    crate::edit::write(&a.svc, |tx, stamp| {
        for i in 0..extra {
            tx.put_folder(earshot_core::model::Folder::new(format!("Box {i}"), stamp))?;
        }
        Ok(())
    })
    .unwrap();
    let (r, n) = pushes_in(&ca, || ca.tick().unwrap());
    assert_eq!(n, MAX_PUSH_PAGES + 1, "{r:?}");
    assert!(r.extra_push, "{r:?}");
    assert_eq!(a.svc.store().outbox_len().unwrap(), 50, "the rest waits");
    let c = join(&relay, &a, "sync-removed-long-c");
    assert!(removed(&c, p), "the relay holds the tombstone");
}

/// No extra push after the relay refused the first push for a far stamp: the tombstone stamped
/// again stays queued, the tick says `clock_refused`, and it goes once the clocks agree.
#[test]
fn sync_removed_c1_no_extra_push_after_a_refused_push_and_the_next_tick_sends_it() {
    let t0 = crate::testutil::T0;
    let now = Arc::new(std::sync::atomic::AtomicU64::new(t0));
    let relay = relay_on_clock(&now);
    let (a, b) = pair(&relay);
    let (ca, cb) = (a.client(), b.client());
    let (_, _, p) = named_person(&a, &b, "Robin Invented", 9);
    a.clock.advance_ms(1_000);
    crate::edit::remove_person(&a.svc, p).unwrap();
    ca.tick().unwrap();
    b.clock.advance_ms(5_000);
    crate::edit::rename_person(&b.svc, p, "Robin Renamed").unwrap();
    cb.tick().unwrap();
    // A's clock is wrong now: its queued meeting is refused, the pull still stamps again.
    a.clock.advance_ms(3 * DAY_MS);
    meeting(&a.svc, "Recorded by a clock that is wrong now", 10);
    let (r, n) = pushes_in(&ca, || ca.tick().unwrap());
    assert!(r.clock_refused && r.tombstones_restamped == 1, "{r:?}");
    assert_eq!((n, r.extra_push), (1, false), "{r:?}");
    cb.tick().unwrap();
    assert!(!removed(&b, p), "not closed in that tick");
    now.store(t0 + 3 * DAY_MS, Ordering::SeqCst);
    // B's clock too, or it would hold A's stamps as far ahead.
    b.clock.advance_ms(3 * DAY_MS);
    ca.tick().unwrap();
    cb.tick().unwrap();
    assert!(removed(&b, p), "the next tick sent it");
}

/// No extra push when the client is stopping; and an extra push that fails (the relay refuses
/// it) leaves the tombstone queued for the next tick, which sends it.
#[test]
fn sync_removed_c1_a_stop_or_a_failing_extra_push_leaves_the_tombstone_queued() {
    for case in ["stop", "refused"] {
        let t0 = crate::testutil::T0;
        let now = Arc::new(std::sync::atomic::AtomicU64::new(t0 + DAY_MS));
        let relay = relay_on_clock(&now);
        let (a, b) = pair(&relay);
        let cb = b.client();
        let (_, _, p) = named_person(&a, &b, "Robin Invented", 11);
        a.clock.advance_ms(1_000);
        crate::edit::remove_person(&a.svc, p).unwrap();
        a.client().tick().unwrap();
        b.clock.advance_ms(5_000);
        crate::edit::rename_person(&b.svc, p, "Robin Renamed").unwrap();
        cb.tick().unwrap();
        let hooked = a.client();
        let stop = hooked.stop_flag();
        let now2 = Arc::clone(&now);
        let hooked = hooked.with_before_extra_push(Arc::new(move || match case {
            "stop" => stop.store(true, Ordering::SeqCst),
            // The relay's clock goes back two days: the stamp is now too far ahead for it.
            _ => now2.store(t0 - DAY_MS, Ordering::SeqCst),
        }));
        let (r, n) = pushes_in(&hooked, || hooked.tick_reporting().0);
        assert_eq!(r.tombstones_restamped, 1, "case {case}: {r:?}");
        let sent_extra = usize::from(case == "refused");
        assert_eq!(n, sent_extra, "case {case}: {r:?}");
        assert_eq!(r.extra_push, case == "refused", "case {case}: {r:?}");
        assert!(a.svc.store().outbox_len().unwrap() >= 1, "case {case}");
        cb.tick().unwrap();
        assert!(!removed(&b, p), "case {case}");
        now.store(t0 + DAY_MS, Ordering::SeqCst);
        a.client().tick().unwrap();
        cb.tick().unwrap();
        assert!(removed(&b, p), "case {case}: the next tick sent it");
    }
}

/// The limit of 7A.1, pinned on purpose so a later fix changes this test deliberately, never by
/// accident: A removes P, pushes and is never used again; B's offline rename, stamped after the
/// removal, replaces the tombstone at the relay; B and a device C that joins afterwards show P
/// live with B's name. (B's newly saved voice is not built here: saving one needs an identity
/// entry from a final pass, which this harness has no models for; voices A erased stay erased
/// through rule 3a, covered by the store's tests.)
#[test]
fn sync_known_limit_a_removal_whose_holders_never_sync_again_is_undone_by_a_later_offline_rename() {
    let relay = relay();
    let (a, b) = pair(&relay);
    let (ca, cb) = (a.client(), b.client());
    let (_, _, p) = named_person(&a, &b, "Robin Invented", 12);
    a.clock.advance_ms(1_000);
    crate::edit::remove_person(&a.svc, p).unwrap();
    ca.tick().unwrap();
    b.clock.advance_ms(5_000);
    crate::edit::rename_person(&b.svc, p, "Robin Renamed").unwrap();
    cb.tick().unwrap();
    let c = join(&relay, &b, "sync-removed-limit-c");
    for dev in [&b, &c] {
        let row = person_row(dev, p).unwrap();
        assert!(!row.meta.deleted, "the limit: P is live");
        assert_eq!(row.display_name, "Robin Renamed");
    }
    // A holder closes it: one tick of A and every device ends removed.
    ca.tick().unwrap();
    for dev in [&b, &c] {
        dev.client().tick().unwrap();
        assert!(removed(dev, p), "closed by the holder");
    }
    assert!(removed(&a, p));
}

/// A holder closes it when the holder is C, which pulled the tombstone before B's push, not A.
#[test]
fn sync_removed_a_holder_that_pulled_the_tombstone_closes_it_too() {
    let relay = relay();
    let (a, b) = pair(&relay);
    let ca = a.client();
    let (_, _, p) = named_person(&a, &b, "Robin Invented", 13);
    let c = join(&relay, &a, "sync-removed-holder-c");
    a.clock.advance_ms(1_000);
    crate::edit::remove_person(&a.svc, p).unwrap();
    ca.tick().unwrap();
    c.client().tick().unwrap();
    assert!(removed(&c, p));
    b.clock.advance_ms(5_000);
    crate::edit::rename_person(&b.svc, p, "Robin Renamed").unwrap();
    b.client().tick().unwrap();
    let r = c.client().tick().unwrap();
    assert!(r.extra_push, "{r:?}");
    b.client().tick().unwrap();
    assert!(removed(&b, p));
}

/// Three devices, two holding the tombstone, one sending a later live copy: each holder stamps
/// once, then every device settles removed and a further round writes nothing.
#[test]
fn sync_removed_three_devices_settle_with_one_stamp_each() {
    let relay = relay();
    let (a, b) = pair(&relay);
    let (_, _, p) = named_person(&a, &b, "Robin Invented", 14);
    let c = join(&relay, &a, "sync-removed-three-c");
    a.clock.advance_ms(1_000);
    crate::edit::remove_person(&a.svc, p).unwrap();
    a.client().tick().unwrap();
    c.client().tick().unwrap();
    b.clock.advance_ms(5_000);
    crate::edit::rename_person(&b.svc, p, "Robin Renamed").unwrap();
    b.client().tick().unwrap();
    let mut stamps = 0;
    for dev in [&a, &c, &b, &a, &c, &b] {
        stamps += dev.client().tick().unwrap().tombstones_restamped;
    }
    assert!(stamps <= 2, "at most one stamp per holder: {stamps}");
    for dev in [&a, &b, &c] {
        assert!(removed(dev, p));
    }
    for dev in [&a, &b, &c] {
        let r = dev.client().tick().unwrap();
        assert_eq!((r.written, r.tombstones_restamped), (0, 0), "{r:?}");
    }
}

/// The arrival orders of 7A.6 for a rename of a person and of a folder: whoever reaches the
/// relay first and whichever stamp is later, once the holder has synced every device ends
/// removed.
#[test]
fn sync_removed_every_arrival_order_ends_removed_for_persons_and_folders() {
    for (tombstone_first, b_later) in [(true, true), (true, false), (false, true), (false, false)] {
        let relay = relay();
        let (a, b) = pair(&relay);
        let (_, _, p) = named_person(&a, &b, "Robin Invented", 15);
        let f = crate::folders::create(&a.svc, "Invented projects").unwrap();
        trade(&a, &b);
        let (first, later) = if b_later { (&a, &b) } else { (&b, &a) };
        first.clock.advance_ms(1_000);
        later.clock.advance_ms(5_000);
        crate::edit::remove_person(&a.svc, p).unwrap();
        crate::folders::delete_keeping_meetings(&a.svc, f).unwrap();
        crate::edit::rename_person(&b.svc, p, "Robin Renamed").unwrap();
        crate::folders::rename(&b.svc, f, "Renamed").unwrap();
        if tombstone_first {
            a.client().tick().unwrap();
            b.client().tick().unwrap();
        } else {
            b.client().tick().unwrap();
            a.client().tick().unwrap();
        }
        for dev in [&a, &b, &a, &b] {
            dev.client().tick().unwrap();
        }
        let case = format!("tombstone first {tombstone_first}, B later {b_later}");
        for dev in [&a, &b] {
            assert!(removed(dev, p), "{case}");
            assert!(folder_removed(dev, f), "{case}");
        }
    }
}

/// A6: a person with no saved voice removed on A leads B to checkpoint after the page (a cleared
/// name and rewritten speaker names, no biometric item): the old name is gone from B's database
/// file and write-ahead log, with a second connection kept open across the check.
#[test]
fn sync_removed_erasure_a_removal_with_no_voice_is_checkpointed_on_the_other_device() {
    const NAME: &str = "Quillon Ferraby";
    let relay = relay();
    let (a, b) = pair(&relay);
    let (ca, cb) = (a.client(), b.client());
    let (_, _, p) = named_person(&a, &b, NAME, 16);
    let path = db_path(&b);
    let other = rusqlite::Connection::open(&path).unwrap();
    let holds = |path: &std::path::Path| {
        let mut wal = path.as_os_str().to_owned();
        wal.push("-wal");
        [path.to_path_buf(), std::path::PathBuf::from(wal)]
            .iter()
            .filter_map(|f| std::fs::read(f).ok())
            .any(|bytes| bytes.windows(NAME.len()).any(|w| w == NAME.as_bytes()))
    };
    assert!(holds(&path), "the control: B holds the name before");
    a.clock.advance_ms(1_000);
    crate::edit::remove_person(&a.svc, p).unwrap();
    ca.tick().unwrap();
    cb.tick().unwrap();
    assert!(removed(&b, p));
    assert!(!holds(&path), "the old name left B's file and log");
    drop(other);
}

/// First start repairs (7A.2's last paragraph): a store holding what an earlier build left (a
/// speaker row linked to a person tombstoned here, a live person merged into one) and a pull
/// with nothing in it: the first tick mends both, the second writes nothing.
#[test]
fn sync_removed_the_first_tick_with_an_empty_pull_still_repairs() {
    let relay = relay();
    let (a, _b) = pair(&relay);
    let m = meeting(&a.svc, "Planning", 17);
    let alice = speaker_named(&a, m, "Alice");
    let (gone, merged) = (EntityId::from_bytes([7; 16]), EntityId::from_bytes([8; 16]));
    {
        let clock = Arc::clone(&a.svc.clock);
        let mut db = a.svc.store();
        let (store, hlc) = db.parts();
        let mut tomb = SyncMeta::new(hlc, &*clock);
        tomb.deleted = true;
        store
            .put_person(Person {
                id: gone,
                meta: tomb,
                display_name: String::new(),
                is_me: false,
                merged_into: None,
            })
            .unwrap();
        store
            .put_person(Person {
                id: merged,
                meta: SyncMeta::new(hlc, &*clock),
                display_name: "Merged Invented".into(),
                is_me: false,
                merged_into: Some(gone),
            })
            .unwrap();
        let mut s = store
            .speakers(m)
            .unwrap()
            .into_iter()
            .find(|s| s.id == alice)
            .unwrap();
        s.person_id = Some(gone);
        s.meta.touch(hlc, &*clock);
        store.put_speaker(s).unwrap();
    }
    // Nothing queued and nothing on the relay: the first tick pushes and pulls nothing.
    let c = rusqlite::Connection::open(db_path(&a)).unwrap();
    c.execute("DELETE FROM sync_outbox", []).unwrap();
    drop(c);
    let fresh = a.client();
    let r = fresh.tick().unwrap();
    assert_eq!((r.pulled, r.pushed), (0, 0), "an empty pull: {r:?}");
    assert_eq!(speaker_row(&a, m, alice).person_id, None, "{r:?}");
    assert!(removed(&a, merged), "{r:?}");
    assert!(r.repaired >= 2, "P2 and P3 counted: {r:?}");
    // The second tick sends what the repair wrote and writes nothing.
    let r2 = fresh.tick().unwrap();
    assert_eq!((r2.written, r2.repaired), (0, 0), "{r2:?}");
}

/// A summary on `dev`'s meeting `m` whose short summary is `tldr`.
fn summarise(dev: &Device, m: EntityId, tldr: &str, seed: u64) {
    use rand_core::SeedableRng;
    let v = crate::view::MeetingView::load_named(&dev.svc.store(), m)
        .unwrap()
        .unwrap();
    let mut s = summary_of("Planning");
    s.tldr = tldr.into();
    crate::summary::store_summary(
        &dev.svc,
        &v,
        "test-model".into(),
        &s,
        &mut rand_chacha::ChaCha8Rng::seed_from_u64(seed),
    )
    .unwrap();
}

fn marks(dev: &Device, m: EntityId) -> Vec<String> {
    let mut names: Vec<String> = crate::summary_names::read(&dev.svc.store(), m)
        .unwrap()
        .into_values()
        .flatten()
        .collect();
    names.sort();
    names
}

/// A widened `names_before` never stops a page (review 9 B1): a speaker row linked to a person
/// tombstoned here (left by an earlier build) in a meeting the first page does not write, with a
/// damaged speaker row and, separately, a damaged current summary in that meeting. The first page
/// applies and P3 unlinks the leftover; a damaged summary costs the meeting its mark, a damaged
/// speaker row only its own. Control: with nothing damaged the summary is marked.
#[test]
fn sync_removed_a_widened_names_before_never_stops_a_page() {
    for case in ["none", "speaker", "summary"] {
        let relay = relay();
        let (a, _b) = pair(&relay);
        let m = meeting(&a.svc, "Planning", 18);
        let alice = speaker_named(&a, m, "Alice");
        let p = crate::edit::link_speaker(
            &a.svc,
            m,
            alice,
            &PersonChoice::New {
                name: "Robin Invented".into(),
            },
        )
        .unwrap()
        .person;
        summarise(&a, m, "Robin Invented will send the plan.", 19);
        a.client().tick().unwrap();
        // An earlier build left P tombstoned and the row linked.
        {
            let clock = Arc::clone(&a.svc.clock);
            let mut db = a.svc.store();
            let (store, hlc) = db.parts();
            let mut row = store.person(p).unwrap().unwrap();
            row.display_name = String::new();
            row.meta.mark_deleted(hlc, &*clock);
            store.put_person(row).unwrap();
        }
        let me = speaker_named(&a, m, "Me");
        match case {
            "speaker" => damage(&a, "UPDATE speaker SET role = 'bogus' WHERE id = ?1", me),
            "summary" => damage(
                &a,
                "UPDATE summary SET template = 'bogus' WHERE meeting_id = ?1",
                m,
            ),
            _ => {}
        }
        let r = a.client().tick().unwrap();
        assert_eq!(
            person_id_by_sql(&a, alice),
            None,
            "case {case}: P3 unlinked it: {r:?}"
        );
        let expected: &[&str] = match case {
            "summary" => &[],
            _ => &["Robin Invented"],
        };
        assert_eq!(marks(&a, m), expected, "case {case}: {r:?}");
        assert_eq!(r.damaged_skipped > 0, case != "none", "case {case}: {r:?}");
    }
}

/// Order (A5): a page whose P3 renames a speaker that a summary used marks the summary in that
/// same page. B linked a voice of a second meeting to P after A removed P; the page that brings
/// the tombstone holds no row of that meeting (one row per page), so only the widened
/// `names_before` reads its name. Names pin (7A.5): afterwards neither device holds the name or
/// the id in the fields this section owns; the same check finds them before.
#[test]
fn sync_removed_p3_renames_a_speaker_a_summary_used_and_marks_it_in_the_same_page() {
    let relay = relay();
    let (a, b) = pair(&relay);
    let (_m1, _alice, p) = named_person(&a, &b, "Robin Invented", 20);
    let m2 = meeting(&a.svc, "Review", 21);
    summarise(&a, m2, "Robin Invented will send the plan.", 22);
    trade(&a, &b);
    let other = speaker_named(&b, m2, "Alice");
    crate::edit::link_speaker(&b.svc, m2, other, &PersonChoice::Existing(p)).unwrap();
    assert!(!traces(&b, "Robin Invented", p).is_empty(), "the control");
    a.clock.advance_ms(1_000);
    crate::edit::remove_person(&a.svc, p).unwrap();
    a.client().tick().unwrap();
    b.client().with_pull_pages(50, 1).tick().unwrap();
    assert_eq!(speaker_row(&b, m2, other).person_id, None);
    assert_eq!(marks(&b, m2), ["Robin Invented"]);
    trade(&a, &b);
    for dev in [&a, &b] {
        assert_eq!(traces(dev, "Robin Invented", p), Vec::<String>::new());
    }
}

/// Where `name` or `p` shows in the fields 7A.5 owns: person and folder names, speaker names and
/// links, and the speaker identity records.
fn traces(dev: &Device, name: &str, p: EntityId) -> Vec<String> {
    let db = dev.svc.store();
    let mut out = Vec::new();
    for row in db.persons().unwrap() {
        if row.display_name == name || (row.id == p && !row.display_name.is_empty()) {
            out.push(format!("person {}", row.id));
        }
    }
    for f in db.folders().unwrap() {
        if f.name == name {
            out.push(format!("folder {}", f.id));
        }
    }
    for m in db.meetings().unwrap() {
        for s in db.speakers(m.id).unwrap() {
            if s.display_name == name || s.person_id == Some(p) {
                out.push(format!("speaker {}", s.id));
            }
        }
        let key = format!("speaker-identities/{}", m.id);
        if let Some(bytes) = db.local_value(&key).unwrap() {
            let text = String::from_utf8_lossy(&bytes);
            if text.contains(name) || text.contains(&p.to_string()) {
                out.push(format!("identities {}", m.id));
            }
        }
    }
    out
}

/// C1 and the filters: with voiceprint sync off, the extra push sends the person tombstone and
/// none of the voiceprint rows the same page tombstoned (B's own print of P); they stay queued.
#[test]
fn sync_removed_c1_sends_no_voiceprint_while_their_sync_is_off() {
    let relay = relay();
    let (a, b) = pair(&relay);
    let (_, _, p) = named_person(&a, &b, "Robin Invented", 23);
    {
        let clock = Arc::clone(&b.svc.clock);
        let mut db = b.svc.store();
        let (store, hlc) = db.parts();
        store
            .put_voiceprint(earshot_core::voiceprint::Voiceprint::from_stored(
                EntityId::from_bytes([9; 16]),
                SyncMeta::new(hlc, &*clock),
                p,
                earshot_core::diar::EmbeddingModelId::new("invented-model"),
                vec![0.6, 0.8],
                1,
                5_000,
            ))
            .unwrap();
    }
    a.clock.advance_ms(DAY_MS + 3_600_000);
    crate::edit::remove_person(&a.svc, p).unwrap();
    a.client().tick().unwrap();
    let cb = b.client();
    cb.tick().unwrap();
    b.clock.advance_ms(DAY_MS + 2 * 3_600_000);
    crate::edit::rename_person(&b.svc, p, "Robin Renamed").unwrap();
    let (r, n) = pushes_in(&cb, || cb.tick().unwrap());
    assert_eq!((n, r.extra_push), (2, true), "{r:?}");
    assert!(removed(&b, p));
    assert_eq!(
        b.svc.store().outbox_len().unwrap(),
        1,
        "the print's tombstone waits for its switch: {r:?}"
    );
    let view = sync_overview(&b.svc, &*b.vault).unwrap();
    assert_eq!(view.waiting, 0);
}

/// C1 and the filters: a speaker row of a meeting being recorded that P3 rewrote in the page
/// stays queued through the extra push and goes after the recording ends.
#[test]
fn sync_removed_c1_keeps_a_recording_meetings_rows_queued() {
    let relay = relay();
    let (a, b) = pair(&relay);
    let (_, _, p) = named_person(&a, &b, "Robin Invented", 24);
    a.clock.advance_ms(DAY_MS + 3_600_000);
    crate::edit::remove_person(&a.svc, p).unwrap();
    a.client().tick().unwrap();
    let cb = b.client();
    cb.tick().unwrap();
    // B's copy of Alice is still linked (the held tombstone and A's row wait), and B links
    // another voice of a meeting it is recording now.
    let live = meeting(&b.svc, "Recording now", 25);
    let them = speaker_named(&b, live, "Alice");
    crate::edit::link_speaker(&b.svc, live, them, &PersonChoice::Existing(p)).unwrap();
    let lease = b
        .svc
        .lease_meeting_for(live, MeetingJob::Recording)
        .unwrap();
    b.clock.advance_ms(DAY_MS + 2 * 3_600_000);
    crate::edit::rename_person(&b.svc, p, "Robin Renamed").unwrap();
    let r = cb.tick().unwrap();
    assert!(r.extra_push, "{r:?}");
    assert_eq!(
        speaker_row(&b, live, them).person_id,
        None,
        "P3 unlinked it"
    );
    a.client().tick().unwrap();
    assert!(
        a.svc.store().meeting(live).unwrap().is_none(),
        "nothing of the recording went"
    );
    drop(lease);
    cb.tick().unwrap();
    a.client().tick().unwrap();
    assert_eq!(
        speaker_row(&a, live, them).person_id,
        None,
        "it goes after the recording"
    );
}

/// C1 for a voice sample tombstone (voiceprint sync on): A forgets P's voices; B, which had not
/// heard, writes the sample live again later and pushes first; A's pull stamps its erased sample
/// again and the same tick's extra push stores it, so B ends with the sample erased.
#[test]
fn sync_removed_c1_a_voice_sample_tombstone_is_stamped_again_and_sent_in_the_same_tick() {
    use earshot_core::voiceprint::{SampleRecord, Voiceprint, VoiceprintSample};
    let relay = relay();
    let (a, b) = pair(&relay);
    set_sync_voiceprints(&a.svc, true).unwrap();
    set_sync_voiceprints(&b.svc, true).unwrap();
    let (ca, cb) = (a.client(), b.client());
    let (person, print, sample) = (
        EntityId::from_bytes([0x21; 16]),
        EntityId::from_bytes([0x22; 16]),
        EntityId::from_bytes([0x23; 16]),
    );
    let record = |meta: SyncMeta| SampleRecord {
        id: sample,
        meta,
        voiceprint_id: Some(print),
        person_id: Some(person),
        model: Some(earshot_core::diar::EmbeddingModelId::new("invented-model")),
        vector: vec![1.0, 0.0, 0.0, 0.0],
        speech_ms: Some(6_000),
        meeting_id: None,
        speaker_id: None,
    };
    {
        let clock = Arc::clone(&a.svc.clock);
        let mut db = a.svc.store();
        let (store, hlc) = db.parts();
        store
            .put_person(Person {
                id: person,
                meta: SyncMeta::new(hlc, &*clock),
                display_name: "An invented colleague".into(),
                is_me: false,
                merged_into: None,
            })
            .unwrap();
        store
            .put_voiceprint(Voiceprint::from_stored(
                print,
                SyncMeta::new(hlc, &*clock),
                person,
                earshot_core::diar::EmbeddingModelId::new("invented-model"),
                vec![0.0; 4],
                0,
                0,
            ))
            .unwrap();
        store
            .put_sample(VoiceprintSample::from_stored(record(SyncMeta::new(hlc, &*clock))).unwrap())
            .unwrap();
    }
    ca.tick().unwrap();
    cb.tick().unwrap();
    a.clock.advance_ms(1_000);
    crate::edit::forget_person_voices(&a.svc, person).unwrap();
    b.clock.advance_ms(5_000);
    {
        let clock = Arc::clone(&b.svc.clock);
        let mut db = b.svc.store();
        let (store, hlc) = db.parts();
        let mut meta = *store.sample(sample).unwrap().unwrap().meta();
        meta.touch(hlc, &*clock);
        store
            .put_sample(VoiceprintSample::from_stored(record(meta)).unwrap())
            .unwrap();
    }
    cb.tick().unwrap();
    let (r, n) = pushes_in(&ca, || ca.tick().unwrap());
    assert!(r.extra_push && r.tombstones_restamped >= 1, "{r:?}");
    assert_eq!(n, 2, "{r:?}");
    assert_eq!(a.svc.store().outbox_len().unwrap(), 0, "{r:?}");
    cb.tick().unwrap();
    let gone = b
        .svc
        .store()
        .sample(sample)
        .unwrap()
        .is_none_or(|s| s.is_erased());
    assert!(gone, "B ends with the sample erased");
}

/// The tick's counts (C1, advisory 5): a first push that refused r1 and r2, held a1 too far
/// ahead and found 2 unsendable, then an extra push that refused r2 and r3 and found 1, report
/// exactly `refused == 3`, `clock_ahead == 1` and `unsendable == 2`; with nothing found by the
/// extra push the first push's counts stand exactly. (The rows themselves cannot be made through
/// the relay here: a refused row needs a body past the sealing limit, an unsendable one a damaged
/// voice sample; the push folds through this one function.)
#[test]
fn sync_removed_push_counts_fold_over_the_extra_push() {
    let id = |n: u8| EntityId::from_bytes([n; 16]);
    let set = |ids: &[u8]| {
        ids.iter()
            .map(|n| id(*n))
            .collect::<std::collections::BTreeSet<_>>()
    };
    let mut r = TickReport::default();
    r.fold_push(2, set(&[1, 2]), set(&[9]));
    r.fold_push(1, set(&[2, 3]), set(&[]));
    assert_eq!((r.refused, r.clock_ahead, r.unsendable), (3, 1, 2), "{r:?}");
    let mut r = TickReport::default();
    r.fold_push(2, set(&[1, 2]), set(&[9]));
    r.fold_push(0, set(&[]), set(&[]));
    assert_eq!((r.refused, r.clock_ahead, r.unsendable), (2, 1, 2), "{r:?}");
}

/// Holders that cannot send (7A.1): a holder whose tombstone row does not read keeps it final
/// here but cannot stamp it again or send it, so the limit is not closed in that tick, and the
/// tick says so through `unsendable`.
#[test]
fn sync_removed_a_holder_with_a_damaged_tombstone_cannot_close_the_limit() {
    let relay = relay();
    let (a, b) = pair(&relay);
    let (ca, cb) = (a.client(), b.client());
    let (_, _, p) = named_person(&a, &b, "Robin Invented", 27);
    a.clock.advance_ms(1_000);
    crate::edit::remove_person(&a.svc, p).unwrap();
    ca.tick().unwrap();
    b.clock.advance_ms(5_000);
    crate::edit::rename_person(&b.svc, p, "Robin Renamed").unwrap();
    cb.tick().unwrap();
    damage(
        &a,
        "UPDATE person SET display_name = CAST(X'FF00FE' AS TEXT) WHERE id = ?1",
        p,
    );
    let (r, n) = pushes_in(&ca, || ca.tick().unwrap());
    assert_eq!(
        r.unsendable, 1,
        "the damaged tombstone is never sent: {r:?}"
    );
    assert_eq!(
        (n, r.tombstones_restamped, r.extra_push),
        (0, 0, false),
        "{r:?}"
    );
    cb.tick().unwrap();
    assert!(!removed(&b, p), "not closed");
    let deleted: i64 = rusqlite::Connection::open(db_path(&a))
        .unwrap()
        .query_row(
            "SELECT deleted FROM person WHERE id = ?1",
            rusqlite::params![p.as_uuid().as_bytes().to_vec()],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(deleted, 1, "it stays final on A");
}

/// Sets `p`'s `merged_into` on `dev` as core `merge_persons` writes it (the user merged one
/// person into another while offline).
fn merge(dev: &Device, p: EntityId, into: EntityId) {
    let clock = Arc::clone(&dev.svc.clock);
    let mut db = dev.svc.store();
    let (store, hlc) = db.parts();
    let mut row = store.person(p).unwrap().unwrap();
    row.merged_into = Some(into);
    row.meta.touch(hlc, &*clock);
    store.put_person(row).unwrap();
}

/// Every live speaker row on `dev` linked to `p`, by name.
fn linked_to(dev: &Device, p: EntityId) -> Vec<String> {
    let db = dev.svc.store();
    db.meetings()
        .unwrap()
        .iter()
        .flat_map(|m| db.speakers(m.id).unwrap())
        .filter(|s| !s.meta.deleted && s.person_id == Some(p))
        .map(|s| s.display_name)
        .collect()
}

/// The person rows of 7A.6's table, each with the removal reaching the relay first and with B's
/// write first, B's write stamped after the removal: once the devices have synced, every device
/// ends with P a tombstone with no name and no speaker row linked to it; a merge of X into P
/// removes X with P; a merge of P into Q leaves Q as it is; a relink to Q that is the later write
/// keeps the row linked to Q.
#[test]
fn sync_removed_7a6_what_b_did_before_it_heard_ends_as_the_table_says() {
    for action in [
        "link",
        "not",
        "relink",
        "merge_x_into_p",
        "merge_p_into_q",
        "remove",
    ] {
        for tombstone_first in [true, false] {
            let relay = relay();
            let (a, b) = pair(&relay);
            let (m, alice, p) = named_person(&a, &b, "Robin Invented", 28);
            let m2 = meeting(&a.svc, "Review", 29);
            let q = crate::edit::link_speaker(
                &a.svc,
                m2,
                speaker_named(&a, m2, "Alice"),
                &PersonChoice::New {
                    name: "Quinn Invented".into(),
                },
            )
            .unwrap()
            .person;
            trade(&a, &b);
            let other = speaker_named(&b, m2, "Quinn Invented");
            a.clock.advance_ms(1_000);
            b.clock.advance_ms(5_000);
            crate::edit::remove_person(&a.svc, p).unwrap();
            match action {
                "link" => {
                    let m3 = meeting(&b.svc, "Offline", 30);
                    let s = speaker_named(&b, m3, "Alice");
                    crate::edit::link_speaker(&b.svc, m3, s, &PersonChoice::Existing(p)).unwrap();
                }
                "not" => {
                    crate::edit::not_this_person(&b.svc, m, alice).unwrap();
                }
                "relink" => {
                    crate::edit::link_speaker(&b.svc, m, alice, &PersonChoice::Existing(q))
                        .unwrap();
                }
                "merge_x_into_p" => merge(&b, q, p),
                "merge_p_into_q" => merge(&b, p, q),
                _ => {
                    crate::edit::remove_person(&b.svc, p).unwrap();
                }
            }
            let order = if tombstone_first { [&a, &b] } else { [&b, &a] };
            for dev in order {
                dev.client().tick().unwrap();
            }
            for dev in [&a, &b, &a, &b] {
                dev.client().tick().unwrap();
            }
            let case = format!("{action}, tombstone first {tombstone_first}");
            for dev in [&a, &b] {
                assert!(removed(dev, p), "{case}");
                assert_eq!(linked_to(dev, p), Vec::<String>::new(), "{case}");
                if action == "merge_x_into_p" {
                    assert!(removed(dev, q), "{case}: X goes with P");
                } else {
                    let q_row = person_row(dev, q).unwrap();
                    assert!(
                        !q_row.meta.deleted && q_row.display_name == "Quinn Invented",
                        "{case}: Q stays"
                    );
                    assert_eq!(speaker_row(dev, m2, other).person_id, Some(q), "{case}");
                }
                if action == "relink" {
                    assert_eq!(
                        speaker_row(dev, m, alice).person_id,
                        Some(q),
                        "{case}: the later link to Q stays"
                    );
                }
            }
        }
    }
}

/// The folder rows of 7A.6's table: a meeting moved into F on B before it heard of the deletion
/// stays live and shows in no folder; a deletion on both ends in one tombstone. (A folder made on
/// B with F's name cannot be made while B still holds F live: the name is taken there.)
#[test]
fn sync_removed_7a6_folder_rows_end_as_the_table_says() {
    for action in ["move", "delete"] {
        let relay = relay();
        let (a, b) = pair(&relay);
        let f = crate::folders::create(&a.svc, "Invented projects").unwrap();
        let m = meeting(&a.svc, "Planning", 31);
        trade(&a, &b);
        a.clock.advance_ms(1_000);
        b.clock.advance_ms(5_000);
        crate::folders::delete_keeping_meetings(&a.svc, f).unwrap();
        if action == "move" {
            crate::folders::move_meeting(&b.svc, m, Some(f)).unwrap();
        } else {
            crate::folders::delete_keeping_meetings(&b.svc, f).unwrap();
        }
        for dev in [&a, &b, &a, &b] {
            dev.client().tick().unwrap();
        }
        for dev in [&a, &b] {
            assert!(folder_removed(dev, f), "{action}");
            assert!(dev.title(m).is_some(), "{action}: the meeting stays");
            assert!(
                crate::folders::folder_of(&dev.svc, m).unwrap().is_none(),
                "{action}: in no folder"
            );
        }
    }
}
