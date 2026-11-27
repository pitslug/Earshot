//! T14.65 (docs/sync-spec.md 17.2 to 17.4, 17.9): the damage scan's record and Repair from
//! sync, against the real relay in process. Damage is made by SQL from a second connection;
//! every name is invented; each test states its control. Names hold `damaged` or
//! `sync_repair`, so `cargo test -p earshot-runtime damaged` and `... sync` select them.

use std::sync::{Arc, Mutex};

use earshot_core::identity::{EntityId, SyncMeta};
use earshot_core::model::MeetingRepo;
use earshot_core::voiceprint::{Person, VoiceprintRepo};
use earshot_store::DamageCounts;

use super::*;
use crate::damaged;
use crate::services::MeetingJob;
use crate::sync::repair::{self, RepairStart, RepairStep};
use crate::testutil::meeting;

fn db_path(dev: &Device) -> std::path::PathBuf {
    earshot_store::DataDir::new(dev._dir.0.join("data")).db_path()
}

/// Damages the row `id` of a table by `sql` (`?1` is the id) from a second connection.
fn damage(dev: &Device, sql: &str, id: EntityId) {
    let c = rusqlite::Connection::open(db_path(dev)).unwrap();
    let changed = c.execute(sql, [id.as_uuid().as_bytes().to_vec()]).unwrap();
    assert_eq!(changed, 1, "{sql}");
}

fn person(dev: &Device, name: &str) -> EntityId {
    let clock = Arc::clone(&dev.svc.clock);
    let mut db = dev.svc.store();
    let (store, hlc) = db.parts();
    let id = EntityId::generate(&*clock, &mut *crate::services::os_seeded_rng());
    store
        .put_person(Person {
            id,
            meta: SyncMeta::new(hlc, &*clock),
            display_name: name.into(),
            is_me: false,
            merged_into: None,
        })
        .unwrap();
    id
}

/// The meeting's final lines in time order.
fn lines(dev: &Device, m: EntityId) -> Vec<EntityId> {
    let mut rows = dev.svc.store().segments(m).unwrap();
    rows.sort_by_key(|r| r.segment.start);
    rows.iter().map(|r| r.segment.id).collect()
}

fn found(dev: &Device) -> DamageCounts {
    damaged::scan(&dev.svc).unwrap().counts()
}

fn step(dev: &Device) -> Option<RepairStep> {
    repair::repair_step(&dev.svc).unwrap()
}

fn cursor(dev: &Device) -> Cursor {
    cursor_record(&dev.svc.store()).unwrap()
}

/// Sets the stored cursor's `pull_seq`, keeping the rest (a fault injected between two steps).
fn set_pull_seq(dev: &Device, to: u64) {
    let mut c = cursor(dev);
    c.pull_seq = to;
    let mut db = dev.svc.store();
    local::set(
        &mut db,
        &DeviceRecord::SyncCursor.key(),
        &encode_cursor_record(&c),
    )
    .unwrap();
}

/// A and B paired, a meeting and a person made on A and held by both.
fn shared(relay: &TestRelay) -> (Device, Device, EntityId, EntityId) {
    let (a, b) = pair(relay);
    let m = meeting(&a.svc, "Invented Quillmere review", 41);
    let p = person(&a, "Invented Talloway");
    a.client().tick().unwrap();
    b.client().tick().unwrap();
    assert!(b.title(m).is_some());
    (a, b, m, p)
}

// ---- the record (17.2) ----

fn counts(parts: u64, people: u64) -> DamageCounts {
    DamageCounts {
        parts,
        people,
        ..DamageCounts::default()
    }
}

#[test]
fn damaged_items_bytes_are_pinned() {
    let items = damaged::DamagedItems {
        found: DamageCounts {
            meetings: 1,
            parts: 2,
            people: 3,
            voice: 4,
            folders: 5,
            words: 6,
        },
        dismissed: Some(counts(2, 0)),
    };
    let mut want = 1u32.to_le_bytes().to_vec();
    want.extend_from_slice(
        br#"{"found":{"meetings":1,"parts":2,"people":3,"voice":4,"folders":5,"words":6},"dismissed":{"meetings":0,"parts":2,"people":0,"voice":0,"folders":0,"words":0}}"#,
    );
    assert_eq!(damaged::encode(&items), want);
    assert_eq!(damaged::decode_for_test(Some(want)).items, items);
    // An unreadable record counts as nothing found, and says so.
    let bad = damaged::decode_for_test(Some(b"\x01\x00\x00\x00{".to_vec()));
    assert!(bad.unreadable && bad.items == damaged::DamagedItems::default());
    let none = damaged::decode_for_test(None);
    assert!(!none.unreadable);
}

#[test]
fn sync_repair_record_bytes_are_pinned() {
    let mut armed = 1u32.to_le_bytes().to_vec();
    armed.extend_from_slice(br#"{"step":"armed"}"#);
    let mut reading = 1u32.to_le_bytes().to_vec();
    reading.extend_from_slice(br#"{"step":"reading"}"#);
    assert_eq!(repair::encode_step(RepairStep::Armed), armed);
    assert_eq!(repair::encode_step(RepairStep::Reading), reading);
    assert_eq!(
        repair::decode_step_for_test(Some(reading)),
        Some(RepairStep::Reading)
    );
    // A record that does not decode is a Repair in progress, read again from row 0.
    assert_eq!(
        repair::decode_step_for_test(Some(b"junk".to_vec())),
        Some(RepairStep::Armed)
    );
    assert_eq!(repair::decode_step_for_test(None), None);
}

/// Test 4: the banner's rules on the record. Controls: fewer found does not bring a dismissed
/// banner back; after a clean scan, smaller new damage does.
#[test]
fn damaged_banner_dismissal_follows_the_record() {
    let dev = Device::new("damaged-banner");
    let s = &dev.svc;
    assert!(!damaged::stored(s).unwrap().items.shown(), "nothing found");
    assert!(damaged::record_scan(s, counts(2, 1)).unwrap().shown());
    // A healed row keeps the banner until a scan clears it: the record alone decides.
    assert!(damaged::stored(s).unwrap().items.shown());
    assert!(!damaged::dismiss(s).unwrap().shown(), "dismissed");
    assert!(
        !damaged::record_scan(s, counts(2, 1)).unwrap().shown(),
        "same counts"
    );
    assert!(
        !damaged::record_scan(s, counts(1, 1)).unwrap().shown(),
        "fewer"
    );
    assert!(
        damaged::record_scan(s, counts(1, 2)).unwrap().shown(),
        "more people"
    );
    damaged::dismiss(s).unwrap();
    // A scan that finds nothing clears the dismissed counts...
    let clean = damaged::record_scan(s, DamageCounts::default()).unwrap();
    assert!(!clean.shown() && clean.dismissed.is_none());
    // ...so new damage, even less than when it was dismissed, shows it again.
    assert!(damaged::record_scan(s, counts(1, 0)).unwrap().shown());
    // It survives a restart: it is the stored record.
    assert!(damaged::stored(s).unwrap().items.shown());
}

/// Test 4: a dismiss between a scan's read and its write keeps both. Control: the scan's
/// counts and the dismiss's are both in the record.
#[test]
fn damaged_a_dismiss_during_a_scan_keeps_both_writes() {
    let dev = Device::new("damaged-dismiss-race");
    let s = &dev.svc;
    damaged::record_scan(s, counts(3, 0)).unwrap();
    // A scan reads the store (the record is not part of it)...
    let scanned = damaged::scan(s).unwrap();
    // ...the person dismisses meanwhile...
    damaged::dismiss(s).unwrap();
    // ...and the scan stores what it found.
    let after = damaged::record_scan(s, scanned.counts()).unwrap();
    assert_eq!(after.found, scanned.counts());
    // Nothing is damaged in this store, so the clean scan cleared the dismissal it kept.
    assert_eq!(after.dismissed, None);
    damaged::record_scan(s, counts(3, 0)).unwrap();
    damaged::dismiss(s).unwrap();
    let after = damaged::record_scan(s, counts(4, 0)).unwrap();
    assert_eq!(
        (after.found, after.dismissed),
        (counts(4, 0), Some(counts(3, 0)))
    );
}

/// Test 3: a store damaged while the app was closed is found by a scan, by kind; nothing in the
/// record names a row. Control: the same search finds the invented names in the store file.
#[test]
fn damaged_a_scan_counts_by_kind_and_stores_no_names() {
    let dev = Device::new("damaged-start-up");
    let m = meeting(&dev.svc, "Invented Quillmere review", 43);
    let p = person(&dev, "Invented Talloway");
    let line = lines(&dev, m)[0];
    damage(&dev, "UPDATE segment SET start_ms = -1 WHERE id = ?1", line);
    damage(&dev, "UPDATE person SET created_wall = -1 WHERE id = ?1", p);
    let scan = damaged::scan(&dev.svc).unwrap();
    assert_eq!(scan.counts(), counts(1, 1));
    assert_eq!(scan.in_meeting(m), 1);
    assert_eq!(scan.in_table("segment"), 1);
    let items = damaged::record_scan(&dev.svc, scan.counts()).unwrap();
    assert!(items.shown());
    let bytes = dev
        .svc
        .store()
        .local_value(damaged::DAMAGED_ITEMS_KEY)
        .unwrap()
        .unwrap();
    // The store's bytes, its write-ahead log included.
    let mut file = std::fs::read(db_path(&dev)).unwrap();
    let mut wal = db_path(&dev).into_os_string();
    wal.push("-wal");
    file.extend(std::fs::read(wal).unwrap_or_default());
    for name in ["Quillmere", "Talloway"] {
        let has = |hay: &[u8]| hay.windows(name.len()).any(|w| w == name.as_bytes());
        assert!(!has(&bytes), "{name} in the record");
        assert!(!has(format!("{:?}", scan.counts()).as_bytes()));
        assert!(has(&file), "the search finds {name} in the store");
    }
}

// ---- Repair from sync (17.4) ----

/// Ticks `client` until the Repair ends (at most `ticks`), returning the ending tick's report.
fn tick_to_end(client: &SyncClient, ticks: usize) -> Option<TickReport> {
    for _ in 0..ticks {
        let r = client.tick().unwrap();
        if r.repair_ended {
            return Some(r);
        }
    }
    None
}

/// Test 5: A's damaged line and person row, which B holds, read as B's after Repair and the
/// closing scan finds nothing; a word choice on the damaged line is kept through an ordinary
/// page that wrote another line of the meeting, and holds once the line reads. Control: the
/// same ticks without Repair leave both damaged.
#[test]
fn sync_repair_heals_rows_another_computer_holds_and_keeps_word_choices() {
    let relay = relay();
    let (a, b, m, p) = shared(&relay);
    let ls = lines(&a, m);
    let (said, other) = (ls[0], ls[1]);
    let choices = vec![crate::word_choices::WordChoice {
        segment: said,
        token: 0,
        decoded: "shall".into(),
        choice: crate::word_choices::Choice::Kept,
    }];
    {
        let mut db = a.svc.store();
        local::set(
            &mut db,
            &crate::word_choices::record_key(m),
            &crate::word_choices::encode(&choices).unwrap(),
        )
        .unwrap();
    }
    damage(&a, "UPDATE segment SET start_ms = -1 WHERE id = ?1", said);
    damage(&a, "UPDATE person SET created_wall = -1 WHERE id = ?1", p);
    assert_eq!(found(&a), counts(1, 1));
    // B edits the other line; A's ordinary tick writes it while the line still does not read.
    crate::edit::edit_segment(&b.svc, m, other, "yes friday works for the launch").unwrap();
    b.client().tick().unwrap();
    let (ca, cb) = (a.client(), b.client());
    for _ in 0..3 {
        ca.tick().unwrap();
        cb.tick().unwrap();
    }
    // Control: without Repair nothing heals.
    assert_eq!(found(&a), counts(1, 1));
    assert_eq!(
        crate::word_choices::word_choices(&a.svc.store(), m).unwrap(),
        choices,
        "the choice on the damaged line is kept"
    );
    assert_eq!(repair::start_repair(&a.svc).unwrap(), RepairStart::Started);
    assert_eq!(step(&a), Some(RepairStep::Armed));
    let ended = tick_to_end(&ca, 3).expect("the Repair ends");
    assert_eq!(ended.held, 0);
    assert_eq!(step(&a), None, "the record is gone");
    assert_eq!(
        found(&a),
        DamageCounts::default(),
        "nothing left: result_none"
    );
    let mine = a.svc.store().segments(m).unwrap();
    let theirs = b.svc.store().segments(m).unwrap();
    assert_eq!(
        mine.iter().find(|r| r.segment.id == said),
        theirs.iter().find(|r| r.segment.id == said)
    );
    assert_eq!(
        a.svc.store().person(p).unwrap(),
        b.svc.store().person(p).unwrap()
    );
    // The choice holds again on the line that now reads.
    let rows = a.svc.store().segments(m).unwrap();
    let texts: std::collections::BTreeMap<EntityId, &str> = rows
        .iter()
        .map(|r| (r.segment.id, r.segment.text.as_str()))
        .collect();
    assert!(choices.iter().all(|c| c.holds(&texts)));
    assert_eq!(
        crate::word_choices::word_choices(&a.svc.store(), m).unwrap(),
        choices
    );
}

/// Test 6: beside a healed row, a damaged meeting tombstone against a live copy, a damaged
/// voiceprint whose kind is switched off and a row never pushed stay damaged and are counted.
/// Control: with the tombstone's copy a tombstone, voiceprint sync on and the row pushed before
/// its damage, each is healed.
#[test]
fn sync_repair_leaves_the_unhealable_counted() {
    for control in [false, true] {
        let relay = relay();
        let (a, b, m, p) = shared(&relay);
        let (ca, cb) = (a.client(), b.client());
        // Voice data synced from A while both have voiceprint sync on.
        set_sync_voiceprints(&a.svc, true).unwrap();
        set_sync_voiceprints(&b.svc, true).unwrap();
        let print = {
            let clock = Arc::clone(&a.svc.clock);
            let mut db = a.svc.store();
            let (store, hlc) = db.parts();
            let id = EntityId::generate(&*clock, &mut *crate::services::os_seeded_rng());
            store
                .put_voiceprint(earshot_core::voiceprint::Voiceprint::from_stored(
                    id,
                    SyncMeta::new(hlc, &*clock),
                    p,
                    earshot_core::diar::EmbeddingModelId::new("invented-embedder@3"),
                    vec![0.6, 0.8],
                    1,
                    5_000,
                ))
                .unwrap();
            id
        };
        let gone = meeting(&a.svc, "Invented Brannock sync", 44);
        ca.tick().unwrap();
        cb.tick().unwrap();
        // The deletion: pushed in the control (B's copy a tombstone), else only here.
        a.clock.advance_ms(1_000);
        a.svc.delete_meeting(gone).unwrap();
        if control {
            ca.tick().unwrap();
            cb.tick().unwrap();
        } else {
            set_sync_voiceprints(&a.svc, false).unwrap();
        }
        // A row never pushed (in the control, pushed before its damage).
        let late = person(&a, "Invented Ferrow");
        if control {
            ca.tick().unwrap();
        }
        let healed = lines(&a, m)[0];
        damage(&a, "UPDATE segment SET start_ms = -1 WHERE id = ?1", healed);
        damage(
            &a,
            "UPDATE meeting SET started_unix_ms = -1 WHERE id = ?1",
            gone,
        );
        damage(&a, "UPDATE voiceprint SET count = -1 WHERE id = ?1", print);
        damage(
            &a,
            "UPDATE person SET created_wall = -1 WHERE id = ?1",
            late,
        );
        assert_eq!(
            found(&a),
            DamageCounts {
                meetings: 1,
                parts: 1,
                people: 1,
                voice: 1,
                ..DamageCounts::default()
            }
        );
        repair::start_repair(&a.svc).unwrap();
        tick_to_end(&ca, 4).expect("the Repair ends");
        let left = found(&a);
        if control {
            assert_eq!(left, DamageCounts::default(), "every row healed");
        } else {
            assert_eq!(
                left,
                DamageCounts {
                    meetings: 1,
                    people: 1,
                    voice: 1,
                    ..DamageCounts::default()
                },
                "the healed line goes; the rest is left and counted"
            );
        }
    }
}

/// Test 7: Repair alone pushes nothing and is no vault reset: no row sent, the epoch,
/// `seen_seq` and the resets kept, the outbox unchanged. Controls: a local edit during Repair
/// is pushed; another epoch in the same harness gives a reset.
#[test]
fn sync_repair_sends_nothing_and_is_no_vault_reset() {
    let relay = relay();
    let (a, _b, m, _p) = shared(&relay);
    let ca = a.client();
    ca.tick().unwrap();
    damage(
        &a,
        "UPDATE segment SET start_ms = -1 WHERE id = ?1",
        lines(&a, m)[0],
    );
    let before = cursor(&a);
    let outbox = a.svc.store().outbox_len().unwrap();
    repair::start_repair(&a.svc).unwrap();
    let mut pushed = 0;
    let mut reset = false;
    for _ in 0..3 {
        let r = ca.tick().unwrap();
        pushed += r.pushed;
        reset |= r.vault_reset;
    }
    assert_eq!((pushed, reset), (0, false));
    let after = cursor(&a);
    assert_eq!(
        (&after.epoch, after.seen_seq, &after.resets),
        (&before.epoch, before.seen_seq, &before.resets)
    );
    // The one entry queued was the damaged line's own (the update that damaged it queued
    // it); the pull's write healed the line and took it out. Nothing else changed.
    assert_eq!(outbox, 1);
    assert_eq!(a.svc.store().outbox_len().unwrap(), 0);
    // Control: a local edit during Repair is pushed.
    repair::start_repair(&a.svc).unwrap();
    a.clock.advance_ms(1_000);
    a.rename(m, "Invented Quillmere review, renamed");
    assert!(ca.tick().unwrap().pushed > 0);
    // Control: a relay holding fewer rows than this device has seen gives a reset in the same
    // harness.
    let mut c = cursor(&a);
    c.seen_seq = c.seen_seq.saturating_add(1_000);
    let mut db = a.svc.store();
    local::set(
        &mut db,
        &DeviceRecord::SyncCursor.key(),
        &encode_cursor_record(&c),
    )
    .unwrap();
    drop(db);
    let mut reset = false;
    for _ in 0..2 {
        reset |= ca.tick().unwrap().vault_reset;
    }
    assert!(reset);
}

/// Test 8: the record's writers. The mark comes before the first request from 0 and a later
/// complete pull with nothing held ends it; a tick already past 0 when Repair is pressed does
/// not end it; an empty relay ends after one request; a compare-and-set on a wrong value writes
/// nothing; the restart and the arm commit together.
#[test]
fn sync_repair_marks_before_reading_from_zero_and_ends_on_a_complete_pull() {
    let relay = relay();
    let (a, _b, m, _p) = shared(&relay);
    damage(
        &a,
        "UPDATE segment SET start_ms = -1 WHERE id = ?1",
        lines(&a, m)[0],
    );
    // A tick already past 0 when Repair is pressed: armed after its pages, it does not end.
    let svc = a.svc.clone();
    let pressed = Arc::new(Mutex::new(false));
    let once = Arc::clone(&pressed);
    let late = a.client().with_before_extra_push(Arc::new(move || {
        let mut p = once.lock().unwrap();
        if !*p {
            *p = true;
            repair::start_repair(&svc).unwrap();
        }
    }));
    let r = late.tick().unwrap();
    assert!(r.pull_complete && !r.repair_ended, "{r:?}");
    assert_eq!(step(&a), Some(RepairStep::Armed));
    assert_eq!(cursor(&a).pull_seq, 0, "the restart and the arm together");
    // Pressing again changes nothing (absent -> armed only).
    set_pull_seq(&a, 3);
    assert_eq!(
        repair::start_repair(&a.svc).unwrap(),
        RepairStart::AlreadyRunning
    );
    assert_eq!(cursor(&a).pull_seq, 3, "nothing written");
    set_pull_seq(&a, 0);
    // One page per tick: the first tick marks before its request and does not finish.
    let slow = a.client().with_pull_pages(1, 1);
    let r = slow.tick().unwrap();
    assert!(!r.pull_complete && !r.repair_ended);
    assert_eq!(step(&a), Some(RepairStep::Reading));
    let ended = tick_to_end(&slow, 200).expect("ends on the complete pull");
    assert!(ended.pull_complete && ended.held == 0);
    assert_eq!(step(&a), None);
    assert_eq!(found(&a).parts, 0, "the read from row 0 healed the line");
    // A compare-and-set on a wrong value writes nothing: no mark or end without a record.
    a.svc.guarded_write(|w| {
        assert!(!repair::mark_before_request(w).unwrap());
        assert!(!repair::end_after_tick(w).unwrap());
    });
    assert_eq!(step(&a), None);
    // The end needs `reading`: on `armed` it writes nothing.
    {
        let mut db = a.svc.store();
        local::set(
            &mut db,
            &DeviceRecord::DamageRepair.key(),
            &repair::encode_step(RepairStep::Armed),
        )
        .unwrap();
    }
    a.svc
        .guarded_write(|w| assert!(!repair::end_after_tick(w).unwrap()));
    assert_eq!(step(&a), Some(RepairStep::Armed));
}

/// Test 8: with an empty relay Repair ends after one request.
#[test]
fn sync_repair_on_an_empty_relay_ends_after_one_request() {
    let relay = relay();
    let a = Device::new("sync-repair-empty");
    set_up_first_device(&a.svc, &*a.vault, &relay.relay.url(), SECRET).unwrap();
    let ca = a.client();
    ca.tick().unwrap();
    repair::start_repair(&a.svc).unwrap();
    let pulls = |c: &SyncClient| {
        c.audit_log()
            .entries()
            .iter()
            .filter(|e| e.path.starts_with("/v1/rows"))
            .count()
    };
    let before = pulls(&ca);
    let r = ca.tick().unwrap();
    assert!(r.repair_ended, "{r:?}");
    assert_eq!(pulls(&ca) - before, 1);
}

/// Test 8: a crash between a cursor advance and the mark resumes from 0 after a restart.
/// Control: the line is healed, which only a read from 0 can do.
#[test]
fn sync_repair_resumes_from_zero_after_a_crash_before_the_mark() {
    let relay = relay();
    let (a, _b, m, _p) = shared(&relay);
    damage(
        &a,
        "UPDATE segment SET start_ms = -1 WHERE id = ?1",
        lines(&a, m)[0],
    );
    let far = cursor(&a).pull_seq;
    assert!(far > 0);
    repair::start_repair(&a.svc).unwrap();
    // The crash: the cursor advanced past 0 and the process ended before the mark.
    set_pull_seq(&a, far);
    // A restart: a new client resumes from the record.
    let ca = a.client();
    tick_to_end(&ca, 3).expect("ends");
    assert_eq!(found(&a), DamageCounts::default(), "read from 0");
}

/// Test 8: a held row keeps the Repair running until it applies. Control: freed, the next
/// tick applies it and ends the Repair.
#[test]
fn sync_repair_a_held_row_keeps_it_running_until_it_applies() {
    let relay = relay();
    let (a, _b, m, _p) = shared(&relay);
    damage(
        &a,
        "UPDATE segment SET start_ms = -1 WHERE id = ?1",
        lines(&a, m)[0],
    );
    let job = a.svc.lease_meeting_for(m, MeetingJob::Processing).unwrap();
    repair::start_repair(&a.svc).unwrap();
    let ca = a.client();
    for _ in 0..2 {
        let r = ca.tick().unwrap();
        assert!(r.held > 0 && !r.repair_ended, "{r:?}");
    }
    assert_eq!(step(&a), Some(RepairStep::Reading));
    drop(job);
    tick_to_end(&ca, 3).expect("ends once the row applies");
    assert_eq!(found(&a), DamageCounts::default());
}

/// Test 9: turning sync off, removing this device and joining delete the record. Control: with
/// sync left on the record stays.
#[test]
fn sync_repair_ends_when_sync_is_turned_off_or_this_device_leaves() {
    let relay = relay();
    let (a, b, _m, _p) = shared(&relay);
    repair::start_repair(&a.svc).unwrap();
    set_sync_on(&a.svc, true).unwrap();
    assert_eq!(step(&a), Some(RepairStep::Armed), "on keeps it");
    set_sync_on(&a.svc, false).unwrap();
    assert_eq!(step(&a), None, "off ends it");
    set_sync_on(&a.svc, true).unwrap();
    repair::start_repair(&a.svc).unwrap();
    remove_this_device(&a.svc, a.vault()).unwrap();
    assert_eq!(step(&a), None, "leaving ends it");
    // Joining: a record written after the removal and right before the join (as a Repair of an
    // earlier vault left behind) goes with the join's own transaction.
    let reading = |dev: &Device| {
        let mut db = dev.svc.store();
        local::set(
            &mut db,
            &DeviceRecord::DamageRepair.key(),
            &repair::encode_step(RepairStep::Reading),
        )
        .unwrap();
    };
    let offer = offer_pairing_code(&b.svc, b.vault()).unwrap();
    reading(&a);
    assert_eq!(step(&a), Some(RepairStep::Reading));
    join_with_code(&a.svc, &*a.vault, &relay.relay.url(), offer.code()).unwrap();
    assert_eq!(step(&a), None, "joining ends it");
}

/// Test 9: setting up as a vault's first device ends a Repair a record left behind. Control:
/// the record is there right before the setup.
#[test]
fn sync_repair_ends_when_this_device_sets_up_a_vault() {
    let relay = relay();
    let a = Device::new("sync-repair-setup");
    {
        let mut db = a.svc.store();
        local::set(
            &mut db,
            &DeviceRecord::DamageRepair.key(),
            &repair::encode_step(RepairStep::Reading),
        )
        .unwrap();
    }
    assert_eq!(step(&a), Some(RepairStep::Reading));
    set_up_first_device(&a.svc, &*a.vault, &relay.relay.url(), SECRET).unwrap();
    assert_eq!(step(&a), None, "setting up ends it");
}

/// Test 13 (informational): what the scan and a Repair cost at a stated size, on whatever host
/// runs it. Prints rows and milliseconds; asserts nothing about time.
#[test]
#[ignore = "informational cost figures: cargo test -p earshot-runtime --release damaged_cost -- --ignored --nocapture"]
fn damaged_cost_of_a_scan_and_a_repair() {
    let relay = relay();
    let (a, b) = pair(&relay);
    for i in 0..200 {
        meeting(&a.svc, "Invented cost meeting", 1_000 + i);
    }
    let ca = a.client();
    for _ in 0..20 {
        if ca.tick().unwrap().pull_complete {
            break;
        }
    }
    let rows = a.svc.store().outbox_len().unwrap();
    let t = std::time::Instant::now();
    let scan = damaged::scan(&a.svc).unwrap();
    println!(
        "scan: {:?} for 200 meetings ({} queued), found {}",
        t.elapsed(),
        rows,
        scan.counts().total()
    );
    let cb = b.client();
    let t = std::time::Instant::now();
    repair::start_repair(&b.svc).unwrap();
    let ticks = (0..100)
        .take_while(|_| !cb.tick().unwrap().repair_ended)
        .count()
        + 1;
    println!("repair: {:?} in {ticks} ticks", t.elapsed());
}
