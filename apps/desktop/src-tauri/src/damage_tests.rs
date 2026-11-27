//! T14.65 (docs/sync-spec.md 17.1 to 17.9): the scan gate, the reports that request scans, the
//! banner, Details and Repair from sync in the app. Damage is made by SQL from a second
//! connection; every name is invented; each test states its control. Names start `damaged_`.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use earshot_core::identity::EntityId;
use earshot_runtime::Services;
use earshot_runtime::sync::{MemoryVault, SyncVault, TickDamage};

use super::*;
use crate::testutil::{TempDir, services};

type Jobs = Arc<Mutex<Vec<Box<dyn FnOnce() + Send>>>>;

/// A named route of the app that may report damage.
type Route<'a> = (&'static str, Box<dyn Fn() + 'a>);

/// A gate over `svc` whose jobs are held for the test to run, on a clock the test moves, its
/// events recorded; it is this thread's gate for [`report`].
struct Watch {
    scans: Arc<Scans>,
    jobs: Jobs,
    now: Arc<Mutex<Duration>>,
    events: Arc<Mutex<Vec<DamageEvent>>>,
}

impl Watch {
    fn new(svc: &Services) -> Self {
        let jobs: Jobs = Arc::default();
        let now = Arc::new(Mutex::new(Duration::from_secs(1_000)));
        let events: Arc<Mutex<Vec<DamageEvent>>> = Arc::default();
        let (j, n, e) = (Arc::clone(&jobs), Arc::clone(&now), Arc::clone(&events));
        let scans = Scans::new(
            svc.clone(),
            Arc::new(move || *n.lock().unwrap()),
            Arc::new(move |job| {
                j.lock().unwrap().push(job);
                true
            }),
            Arc::new(move |ev| e.lock().unwrap().push(ev)),
        );
        set_app_scans(&scans);
        Self {
            scans,
            jobs,
            now,
            events,
        }
    }

    fn started(&self) -> u64 {
        self.scans.started_for_test()
    }

    fn advance(&self, by: Duration) {
        *self.now.lock().unwrap() += by;
    }

    /// Runs every held job, and those they hold in turn, on this thread.
    fn run(&self) {
        loop {
            let next = self.jobs.lock().unwrap().pop();
            match next {
                Some(job) => job(),
                None => return,
            }
        }
    }

    fn held(&self) -> usize {
        self.jobs.lock().unwrap().len()
    }

    /// Details on another thread while this one runs the scan it waits for.
    fn details(&self, sync: &SyncHost) -> Result<DamagedView, IpcError> {
        std::thread::scope(|s| {
            let h = s.spawn(|| details(&self.scans, sync));
            while !h.is_finished() {
                self.run();
                std::thread::yield_now();
            }
            h.join().unwrap()
        })
    }
}

fn db(dir: &TempDir) -> std::path::PathBuf {
    dir.0.join("data").join("earshot.db")
}

fn damage(dir: &TempDir, sql: &str, id: EntityId) {
    let c = rusqlite::Connection::open(db(dir)).unwrap();
    let changed = c
        .execute(sql, rusqlite::params![id.as_uuid().as_bytes().to_vec()])
        .unwrap();
    assert_eq!(changed, 1, "{sql}");
}

/// A finished meeting titled `title` with two final lines.
fn a_meeting(svc: &Services, title: &str) -> EntityId {
    use earshot_core::asr::{MeetingTime, Segment, Word};
    use earshot_core::capture::TrackId;
    use earshot_core::model::{
        ClockDomain, Meeting, MeetingRepo, SegmentRow, Stamp, Track, TranscriptPass,
    };
    use rand_core::SeedableRng;
    let clock = Arc::clone(&svc.clock);
    let mut rng = rand_chacha::ChaCha8Rng::seed_from_u64(u64::try_from(title.len()).unwrap_or(0));
    let mut db = svc.store();
    let (store, hlc) = db.parts();
    let mut stamp = Stamp {
        clock: &*clock,
        hlc,
        rng: &mut rng,
    };
    let mut m = Meeting::new(
        title,
        crate::testutil::T0,
        600,
        earshot_core::prompt::Template::General,
        &mut stamp,
    );
    m.duration_ms = Some(60_000);
    let track = Track::new(m.id, TrackId::Mic, None, ClockDomain::Replay, &mut stamp);
    let rows: Vec<SegmentRow> = (0..2u64)
        .map(|i| {
            let start = 1_000 * i;
            let seg = Segment::from_words(
                EntityId::generate(stamp.clock, &mut *stamp.rng),
                track.id,
                vec![Word::new("invented", start, start + 300, 0.9)],
                MeetingTime::ZERO,
            );
            SegmentRow::new(m.id, TranscriptPass::Final, seg, stamp.hlc, stamp.clock)
        })
        .collect();
    let id = m.id;
    store.put_meeting(m).unwrap();
    store.put_track(track).unwrap();
    for r in rows {
        store.put_segment(r).unwrap();
    }
    id
}

/// Stores `problem` as the last sync's (what the sync thread writes when it stops on it).
fn last_problem(svc: &Services, problem: earshot_runtime::sync::SyncProblem) {
    let status = earshot_runtime::sync::SyncStatus {
        problem: Some(problem),
        ..Default::default()
    };
    svc.store()
        .set_local_value(
            "sync-status",
            &earshot_store::encode_versioned(1, &status).unwrap(),
        )
        .unwrap();
}

fn set_up_vault() -> Arc<dyn SyncVault> {
    let vault = Arc::new(MemoryVault::default());
    vault
        .save(
            &earshot_runtime::sync::VaultKey::generate().unwrap(),
            &earshot_runtime::sync::DeviceToken::new("an-invented-token".into()),
        )
        .unwrap();
    vault
}

fn copy() -> serde_json::Value {
    serde_json::from_str(include_str!(
        "../../../../fixtures/ui/damaged_copy_cases.json"
    ))
    .unwrap()
}

fn host() -> Arc<SyncHost> {
    Arc::new(SyncHost::new(
        Arc::new(MemoryVault::default()) as Arc<dyn SyncVault>
    ))
}

// ---- the gate (17.3) ----

/// Requests within the debounce run one scan; a request during a scan runs one more after it,
/// after the debounce; a forced scan waits only for one running, then starts at once. Control: a
/// request after the debounce starts at once, and with no request nothing starts.
#[test]
fn damaged_gate_debounces_and_never_overlaps() {
    let t = |s: u64| Duration::from_secs(s);
    let mut g = Gate::default();
    assert_eq!(g.request(t(0)), Start::Now);
    assert_eq!(g.request(t(1)), Start::None, "one runs: one more after it");
    assert_eq!(
        g.finished(t(2)),
        Start::At(t(30)),
        "an unforced one more waits for the debounce"
    );
    assert_eq!(g.request(t(3)), Start::None, "already due");
    assert_eq!(g.due(t(30)), Start::Now);
    assert_eq!(g.finished(t(31)), Start::None, "nothing more asked");
    // Control: a request after the debounce starts at once.
    assert_eq!(g.request(t(61)), Start::Now);
    assert_eq!(g.finished(t(62)), Start::None);
    // A forced scan ignores the debounce but not a running scan...
    assert_eq!(g.force(t(63)), Start::Now);
    assert!(g.running());
    // ...and one forced while a scan runs starts the moment it ends, debounce or not (17.7).
    assert_eq!(g.force(t(64)), Start::None, "a forced scan never overlaps");
    assert_eq!(g.finished(t(65)), Start::Now);
    assert_eq!(g.finished(t(66)), Start::None);
}

/// Test 1: each report scope requests a scan only above its figure; a meeting page with new
/// damage requests one while another meeting holds old damage. Controls: at or below the
/// figure nothing is requested, and with no damage no report reaches the gate.
#[test]
fn damaged_reports_request_a_scan_only_above_the_last_scans_figure() {
    let dir = TempDir::new("damaged-triggers");
    let svc = services(&dir);
    let old = a_meeting(&svc, "Invented Quillmere review");
    let new = a_meeting(&svc, "Invented Talloway planning");
    let lines = |m: EntityId| {
        use earshot_core::model::MeetingRepo;
        svc.store()
            .segments(m)
            .unwrap()
            .into_iter()
            .map(|r| r.segment.id)
            .collect::<Vec<_>>()
    };
    damage(
        &dir,
        "UPDATE segment SET start_ms = -1 WHERE id = ?1",
        lines(old)[0],
    );
    let w = Watch::new(&svc);
    // Control: with no figure yet a count of 0 asks nothing.
    report(Scope::Total, 0);
    assert_eq!(w.started(), 0);
    // The start-up scan finds the old damage.
    w.scans.scan_soon();
    w.run();
    assert_eq!(w.started(), 1);
    let f = w.scans.figures_for_test();
    assert_eq!(f.of_for_test(Scope::Meeting(old)), 1);
    assert_eq!(f.of_for_test(Scope::Total), 1);
    // At or below each figure: nothing.
    w.advance(Duration::from_secs(60));
    for scope in [Scope::Total, Scope::Search, Scope::Meeting(old)] {
        report(scope, 1);
    }
    for scope in [
        Scope::People,
        Scope::PeopleAndVoice,
        Scope::MeetingsAndFolders,
    ] {
        report(scope, 0);
    }
    assert_eq!(w.started(), 1, "leftovers already counted cause no scan");
    // New damage in another meeting: its page asks, though the old meeting's figure is 1.
    damage(
        &dir,
        "UPDATE segment SET start_ms = -1 WHERE id = ?1",
        lines(new)[0],
    );
    assert!(crate::detail::meeting_detail(&svc, new).is_err());
    assert_eq!(w.started(), 2, "the new meeting's page asked");
    w.run();
    assert_eq!(
        w.scans.figures_for_test().of_for_test(Scope::Meeting(new)),
        1
    );
    // Each other scope above its figure asks (the debounce holds the second for later).
    w.advance(Duration::from_secs(60));
    report(Scope::People, 1);
    assert_eq!(w.started(), 3);
    report(Scope::MeetingsAndFolders, 1);
    w.run();
    assert_eq!(
        w.started(),
        4,
        "a request during a scan ran one more after it"
    );
}

/// Test 1 through the app's own routes: the meeting list, the People list, a search, a refusal
/// of kind `damaged` and a sync tick each request a scan when they meet more than the figure.
/// Control: the same routes over an undamaged store request nothing.
#[test]
fn damaged_the_apps_routes_each_report_what_they_meet() {
    for damaged in [true, false] {
        let dir = TempDir::new("damaged-routes");
        let svc = services(&dir);
        let m = a_meeting(&svc, "Invented Brannock retro");
        if damaged {
            damage(
                &dir,
                "UPDATE meeting SET title = CAST(X'FF00FE' AS TEXT) WHERE id = ?1",
                m,
            );
        }
        let routes: Vec<Route<'_>> = vec![
            (
                "meeting list",
                Box::new(|| {
                    crate::ipc::meeting_list(&svc).unwrap();
                }),
            ),
            (
                "search",
                Box::new(|| {
                    crate::ipc::meeting_search(
                        &svc,
                        "invented",
                        &earshot_core::asr::CancelToken::new(),
                    )
                    .unwrap();
                }),
            ),
            (
                "sync tick",
                Box::new(|| {
                    let w = app_scans().unwrap();
                    w.heard_tick(&TickDamage {
                        damaged: usize::from(damaged),
                        repair_ended: false,
                    });
                }),
            ),
        ];
        for (name, route) in routes {
            let w = Watch::new(&svc);
            route();
            assert_eq!(w.started(), u64::from(damaged), "{name}");
        }
        // A refusal of kind `damaged` counts as 1 whatever the store holds.
        let w = Watch::new(&svc);
        let e =
            crate::detail::edit_error(&earshot_runtime::edit::EditError::FolderBlockedByDamage(m));
        assert_eq!(
            (e.kind.as_str(), e.message.as_str()),
            (DAMAGED_KIND, REFUSAL_COPY)
        );
        assert_eq!(w.started(), 1);
    }
}

/// Test 2: a scan over damaged rows requests no further scan (it reports nothing). Control: the
/// meeting list over the same rows requests one when the figure says 0.
#[test]
fn damaged_a_scan_never_starts_another() {
    let dir = TempDir::new("damaged-no-loop");
    let svc = services(&dir);
    let m = a_meeting(&svc, "Invented Ferrow review");
    damage(
        &dir,
        "UPDATE meeting SET title = CAST(X'FF00FE' AS TEXT) WHERE id = ?1",
        m,
    );
    let w = Watch::new(&svc);
    w.scans.scan_soon();
    w.run();
    assert_eq!((w.started(), w.held()), (1, 0));
    assert!(!w.scans.gate_for_test().running());
    // Control: a fresh gate (figure 0) and the meeting list over the same rows asks.
    let dir2 = TempDir::new("damaged-no-loop-control");
    let svc2 = services(&dir2);
    let m2 = a_meeting(&svc2, "Invented Ferrow review");
    damage(
        &dir2,
        "UPDATE meeting SET title = CAST(X'FF00FE' AS TEXT) WHERE id = ?1",
        m2,
    );
    let w2 = Watch::new(&svc2);
    crate::ipc::meeting_list(&svc2).unwrap();
    assert_eq!(w2.started(), 1);
}

/// Tests 3 and 4: the start-up scan shows the banner for a store damaged while the app was
/// closed; dismissed, it stays away; the window hears every scan; nothing serialised names a
/// row. Control: the invented name is in the store file.
#[test]
fn damaged_the_start_up_scan_shows_the_banner_and_dismiss_hides_it() {
    let dir = TempDir::new("damaged-banner");
    let svc = services(&dir);
    let m = a_meeting(&svc, "Invented Quillmere review");
    damage(
        &dir,
        "UPDATE meeting SET template = 'invented-word' WHERE id = ?1",
        m,
    );
    let w = Watch::new(&svc);
    assert!(!banner(&w.scans).unwrap(), "no scan yet");
    w.scans.scan_soon();
    w.run();
    assert!(banner(&w.scans).unwrap());
    let heard = w.events.lock().unwrap().clone();
    assert_eq!(
        heard.last().copied(),
        Some(DamageEvent {
            shown: true,
            repair: RepairView::Idle
        })
    );
    dismiss(&w.scans).unwrap();
    assert!(!banner(&w.scans).unwrap());
    // A scan with the same counts keeps it away.
    w.advance(Duration::from_secs(60));
    w.scans.scan_soon();
    w.run();
    assert!(!banner(&w.scans).unwrap());
    // Nothing the window gets names the meeting.
    let view = w.details(&host());
    let json = serde_json::to_string(&(view.ok(), heard)).unwrap();
    assert!(!json.contains("Quillmere"), "{json}");
    let file = std::fs::read(db(&dir)).unwrap();
    let mut wal = db(&dir).into_os_string();
    wal.push("-wal");
    let mut all = file;
    all.extend(std::fs::read(wal).unwrap_or_default());
    assert!(
        all.windows(9).any(|x| x == b"Quillmere"),
        "the search finds it"
    );
}

/// Test 10: with sync off Details offers no Repair and Repair is refused. Control: set up and
/// on, Details offers it and it starts, and a second press is `busy`.
#[test]
fn damaged_details_offer_repair_only_with_sync_set_up_and_on() {
    let dir = TempDir::new("damaged-details");
    let svc = services(&dir);
    let w = Watch::new(&svc);
    let sync = host();
    let off = w.details(&sync).unwrap();
    assert!(!off.sync_repairs);
    let refused = repair(&w.scans, &sync).unwrap_err();
    assert_eq!(refused.message, SYNC_OFF_COPY);
    // Control: set up (a key and token) and on.
    let on_host = Arc::new(SyncHost::new(set_up_vault()));
    earshot_runtime::sync::set_sync_on(&svc, true).unwrap();
    repair(&w.scans, &on_host).unwrap();
    assert_eq!(
        earshot_runtime::sync::repair_step(&svc).unwrap(),
        Some(earshot_runtime::sync::RepairStep::Armed)
    );
    let busy = repair(&w.scans, &on_host).unwrap_err();
    assert_eq!(
        (busy.kind.as_str(), busy.message.as_str()),
        ("busy", BUSY_COPY)
    );
    assert_eq!(
        w.scans.repair_view(&on_host),
        RepairView::Reading { percent: None },
        "no percentage while seen_seq is 0"
    );
}

/// Test 9: progress has no percentage while `seen_seq` is 0, at most 99 otherwise; a stopping
/// problem shows Stopped. Control: with sync running no stopped line.
#[test]
fn damaged_repair_progress_and_stops() {
    use earshot_runtime::sync::repair_percent;
    assert_eq!(repair_percent(0, 0), None);
    assert_eq!(repair_percent(5, 10), Some(50));
    assert_eq!(repair_percent(10, 10), Some(99));
    assert_eq!(repair_percent(30, 10), Some(99));
    let dir = TempDir::new("damaged-stopped");
    let svc = services(&dir);
    let w = Watch::new(&svc);
    {
        let sync = Arc::new(SyncHost::new(set_up_vault()));
        earshot_runtime::sync::set_sync_on(&svc, true).unwrap();
        repair(&w.scans, &sync).unwrap();
        assert!(matches!(
            w.scans.repair_view(&sync),
            RepairView::Reading { .. }
        ));
        for problem in [
            earshot_runtime::sync::SyncProblem::UpdateNeeded,
            earshot_runtime::sync::SyncProblem::UnknownDevice,
        ] {
            last_problem(&svc, problem);
            assert_eq!(w.scans.repair_view(&sync), RepairView::Stopped);
        }
        // Turning sync off ends it: no record, no stopped line.
        earshot_runtime::sync::set_sync_on(&svc, false).unwrap();
        assert_eq!(w.scans.repair_view(&sync), RepairView::Idle);
    }
}

/// Test 6 (the app's half) and 17.4 Done: the closing scan after a Repair keeps what it left;
/// Details names a damaged kind whose switch is off. Control: with nothing left, Done holds
/// zeros (`result_none`), and with the switch on no kind is off.
#[test]
fn damaged_details_name_kinds_whose_sync_is_off_and_done_keeps_what_is_left() {
    let dir = TempDir::new("damaged-kind-off");
    let svc = services(&dir);
    let w = Watch::new(&svc);
    let sync = host();
    // A Dictionary word row that does not read, Dictionary sync off.
    earshot_runtime::vocabulary::add_entry(&svc, "Quillmere").unwrap();
    let word = {
        use earshot_core::model::DictionaryRepo;
        svc.store().dictionary_words().unwrap()[0].id
    };
    damage(
        &dir,
        "UPDATE dictionary_word SET word = CAST(X'FF00FE' AS TEXT) WHERE id = ?1",
        word,
    );
    let run_details = |sync: &SyncHost| w.details(sync).unwrap();
    earshot_runtime::sync::set_sync_dictionary(&svc, false).unwrap();
    let off = run_details(&sync);
    assert_eq!(off.counts.words, 1);
    assert!(off.kinds_off.words && !off.kinds_off.voice);
    earshot_runtime::sync::set_sync_dictionary(&svc, true).unwrap();
    let on = run_details(&sync);
    assert!(!on.kinds_off.words, "control: the switch is on");
    // The end of a Repair starts the closing scan, whose counts Done keeps.
    w.advance(Duration::from_secs(60));
    w.scans.heard_tick(&TickDamage {
        damaged: 0,
        repair_ended: true,
    });
    w.run();
    assert_eq!(
        w.scans.repair_view(&sync),
        RepairView::Done {
            left: DamageCountsView {
                words: 1,
                ..DamageCountsView::default()
            }
        }
    );
    w.scans.repair_started();
    assert_eq!(w.scans.repair_view(&sync), RepairView::Idle);
}

/// The Rust copy is the table's (17.8).
#[test]
fn damaged_copy_is_the_tables() {
    let c = copy();
    assert_eq!(c["refusal"].as_str(), Some(REFUSAL_COPY));
    assert_eq!(c["busy"].as_str(), Some(BUSY_COPY));
    assert_eq!(c["no_repair_sync_off"].as_str(), Some(SYNC_OFF_COPY));
    for key in [
        "banner",
        "details_button",
        "dismiss",
        "details_title",
        "close",
        "nothing_damaged",
        "repair_intro",
        "nothing_healable",
        "kind_off_voice",
        "kind_off_words",
        "result_some",
        "repair_button",
        "repair_reading_start",
        "repair_reading",
        "repair_stopped",
        "result_none",
    ] {
        let text = c[key].as_str().unwrap_or_else(|| panic!("{key}"));
        assert!(!text.contains('\u{2014}'), "{key}: no em dash");
    }
    for kind in ["meetings", "parts", "people", "voice", "folders", "words"] {
        assert!(c["kinds"][kind]["one"].as_str().unwrap().starts_with("1 "));
        assert!(
            c["kinds"][kind]["many"]
                .as_str()
                .unwrap()
                .contains("{count}")
        );
    }
}

// ---- test 12: what goes and what stays (17.6) ----

/// The keys and names 17.6 removes.
const REMOVED: &[&str] = &[
    "list_damaged_note",
    "damaged_note",
    "folder_blocked_by_damage",
    "rename_blocked_by_damage",
    "remove_unreadable",
    "dictionary_damaged",
    "DAMAGED_NOTE",
    "FOLDER_BLOCKED_BY_DAMAGE_COPY",
    "RENAME_BLOCKED_BY_DAMAGE_COPY",
    "REMOVE_UNREADABLE_COPY",
    "meeting_list_copy_cases",
];

/// The ones the owner kept.
const KEPT: &[&str] = &[
    "forget_my_voice_incomplete",
    "remove_damaged_note",
    "forget_damaged_note",
    "UNREADABLE_NOTE",
];

/// Whether `text` holds `key` as a whole identifier (so `forget_damaged_note` never counts as
/// `damaged_note`).
fn holds_key(text: &str, key: &str) -> bool {
    let ident = |c: char| c.is_ascii_alphanumeric() || c == '_';
    text.match_indices(key).any(|(at, _)| {
        let before = text[..at].chars().next_back();
        let after = text[at + key.len()..].chars().next();
        !before.is_some_and(ident) && !after.is_some_and(ident)
    })
}

fn sources() -> Vec<(std::path::PathBuf, String)> {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../..");
    let mut out = Vec::new();
    let mut stack = vec![
        root.join("apps/desktop/src-tauri/src"),
        root.join("packages/ui/src"),
        root.join("fixtures/ui"),
    ];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            let name = path.file_name().unwrap().to_string_lossy().into_owned();
            // This file holds the lists themselves.
            if name == "damage_tests.rs" {
                continue;
            }
            if [".rs", ".ts", ".tsx", ".json"]
                .iter()
                .any(|e| name.ends_with(e))
            {
                out.push((path.clone(), std::fs::read_to_string(&path).unwrap()));
            }
        }
    }
    out
}

#[test]
fn damaged_old_notes_are_gone_and_kept_ones_stay() {
    let files = sources();
    assert!(files.len() > 50, "the scan read the sources");
    let mut found = Vec::new();
    for (path, text) in &files {
        for key in REMOVED {
            if holds_key(text, key) {
                found.push(format!("{} holds {key}", path.display()));
            }
        }
    }
    assert!(found.is_empty(), "{found:#?}");
    for key in KEPT {
        assert!(
            files.iter().any(|(_, t)| holds_key(t, key)),
            "{key} is kept"
        );
    }
    // Control: a planted removed key is found, and a kept one that contains it is not.
    assert!(holds_key(
        "\"list_damaged_note\": \"x\"",
        "list_damaged_note"
    ));
    assert!(holds_key("damaged_note: Option<String>", "damaged_note"));
    assert!(!holds_key("forget_damaged_note", "damaged_note"));
}

// ---- the fix round (T14.65c) ----

/// A Watch with sync set up and on, so `repair` arms.
fn sync_on(svc: &Services) -> Arc<SyncHost> {
    earshot_runtime::sync::set_sync_on(svc, true).unwrap();
    Arc::new(SyncHost::new(set_up_vault()))
}

/// Ends the armed Repair as a tick would, and runs the closing scan.
fn end_repair(w: &Watch, svc: &Services) {
    svc.store()
        .delete_local_value(earshot_runtime::sync::DAMAGE_REPAIR_KEY)
        .unwrap();
    w.advance(Duration::from_secs(60));
    w.scans.heard_tick(&TickDamage {
        damaged: 0,
        repair_ended: true,
    });
    w.run();
}

/// B1: Done never outlives the figures. A Repair ends with nothing left; a later scan finds a
/// damaged row; Details then shows its count with Repair offered and Done holding the fresh
/// count, never zeros (`result_none`). Control: with no later damage Done stays at zeros.
#[test]
fn damaged_done_follows_every_later_scan() {
    for later in [true, false] {
        let dir = TempDir::new("damaged-done-later");
        let svc = services(&dir);
        let m = a_meeting(&svc, "Invented Wrenfield review");
        let w = Watch::new(&svc);
        let sync = sync_on(&svc);
        repair(&w.scans, &sync).unwrap();
        end_repair(&w, &svc);
        assert_eq!(
            w.scans.repair_view(&sync),
            RepairView::Done {
                left: DamageCountsView::default()
            }
        );
        if later {
            damage(
                &dir,
                "UPDATE meeting SET template = 'invented-word' WHERE id = ?1",
                m,
            );
        }
        w.advance(Duration::from_secs(60));
        let view = w.details(&sync).unwrap();
        let want = DamageCountsView {
            meetings: u32::from(later),
            ..DamageCountsView::default()
        };
        assert_eq!(view.counts, want);
        assert!(view.sync_repairs, "Repair is offered");
        assert_eq!(view.repair, RepairView::Done { left: want });
    }
}

/// B2: a scan already running when the Repair ends is not the closing scan; the closing one is
/// the next to start. Control: with no scan in flight, the next scan closes it at once.
#[test]
fn damaged_a_scan_in_flight_when_a_repair_ends_does_not_close_it() {
    let dir = TempDir::new("damaged-closing-in-flight");
    let svc = services(&dir);
    let m = a_meeting(&svc, "Invented Talloway planning");
    let w = Watch::new(&svc);
    let sync = sync_on(&svc);
    repair(&w.scans, &sync).unwrap();
    // A scan starts (held) while the line is damaged.
    damage(
        &dir,
        "UPDATE meeting SET template = 'invented-word' WHERE id = ?1",
        m,
    );
    w.scans.scan_soon();
    assert_eq!(w.held(), 1, "the scan is in flight");
    // The Repair ends while that scan is still in flight.
    svc.store()
        .delete_local_value(earshot_runtime::sync::DAMAGE_REPAIR_KEY)
        .unwrap();
    w.scans.heard_tick(&TickDamage {
        damaged: 0,
        repair_ended: true,
    });
    // Run only the scan that started before the end: it does not close the Repair.
    let first = w.jobs.lock().unwrap().remove(0);
    first();
    assert_eq!(w.scans.repair_view(&sync), RepairView::Idle);
    // The closing scan, forced, starts at once after it and closes it with what is left.
    w.run();
    assert_eq!(
        w.scans.repair_view(&sync),
        RepairView::Done {
            left: DamageCountsView {
                meetings: 1,
                ..DamageCountsView::default()
            }
        }
    );
}

/// A2: a search's count covers meeting rows as its figure does, so a damaged meeting row a
/// scan already counted causes no scan. Control: a gate that has not scanned asks.
#[test]
fn damaged_a_search_over_a_counted_meeting_row_asks_no_scan() {
    let dir = TempDir::new("damaged-search-meeting");
    let svc = services(&dir);
    let m = a_meeting(&svc, "Invented Brannock retro");
    damage(
        &dir,
        "UPDATE meeting SET title = CAST(X'FF00FE' AS TEXT) WHERE id = ?1",
        m,
    );
    let w = Watch::new(&svc);
    w.scans.scan_soon();
    w.run();
    let before = w.started();
    w.advance(Duration::from_secs(60));
    let found =
        crate::ipc::meeting_search(&svc, "invented", &earshot_core::asr::CancelToken::new())
            .unwrap();
    assert_eq!(found.unreadable, 1);
    assert_eq!(w.started(), before, "the counted row asks for nothing");
    let w2 = Watch::new(&svc);
    crate::ipc::meeting_search(&svc, "invented", &earshot_core::asr::CancelToken::new()).unwrap();
    assert_eq!(w2.started(), 1, "control: an unscanned gate asks");
}

/// A3: a meeting page that fails on a damaged speaker reports the meeting's damaged parts.
/// Control: the same page over an undamaged meeting reads and reports nothing.
#[test]
fn damaged_a_meeting_page_failing_on_a_speaker_reports_its_parts() {
    for damaged in [true, false] {
        let dir = TempDir::new("damaged-page-speaker");
        let svc = services(&dir);
        let m = a_meeting(&svc, "Invented Quillmere review");
        {
            use earshot_core::model::{MeetingRepo, Speaker};
            let clock = Arc::clone(&svc.clock);
            let mut db = svc.store();
            let (store, hlc) = db.parts();
            store
                .put_speaker(Speaker {
                    id: EntityId::from_bytes([
                        0x01, 0x95, 0, 0, 0, 0, 0x70, 0, 0x80, 0, 0, 0, 0, 0, 7, 3,
                    ]),
                    meta: earshot_core::identity::SyncMeta::new(hlc, &*clock),
                    meeting_id: m,
                    role: earshot_core::diar::SpeakerRole::Remote,
                    display_name: "Invented Ferrow".into(),
                    person_id: None,
                    ignored: false,
                })
                .unwrap();
        }
        if damaged {
            let c = rusqlite::Connection::open(db(&dir)).unwrap();
            c.execute(
                "UPDATE speaker SET role = 'bogus' WHERE meeting_id = ?1",
                rusqlite::params![m.as_uuid().as_bytes().to_vec()],
            )
            .unwrap();
        }
        let (read, asked) = reports_for_test(|| crate::detail::meeting_detail(&svc, m).map(|_| ()));
        assert_eq!(read.is_err(), damaged);
        assert_eq!(asked, damaged);
    }
}

/// A4: a scan whose thread cannot start leaves the gate free and Details answers `internal`.
/// Control: with a thread, Details answers.
#[test]
fn damaged_a_scan_that_cannot_start_never_hangs_details() {
    let dir = TempDir::new("damaged-no-thread");
    let svc = services(&dir);
    let refuse = Arc::new(std::sync::atomic::AtomicBool::new(true));
    let r = Arc::clone(&refuse);
    let jobs: Jobs = Arc::default();
    let j = Arc::clone(&jobs);
    let scans = Scans::new(
        svc.clone(),
        Arc::new(|| Duration::from_secs(1_000)),
        Arc::new(move |job| {
            if r.load(std::sync::atomic::Ordering::SeqCst) {
                return false;
            }
            j.lock().unwrap().push(job);
            true
        }),
        Arc::new(|_| {}),
    );
    let sync = host();
    let e = details(&scans, &sync).unwrap_err();
    assert_eq!(e, IpcError::internal());
    assert!(!scans.gate_for_test().running(), "the gate is free again");
    refuse.store(false, std::sync::atomic::Ordering::SeqCst);
    let ok = std::thread::scope(|s| {
        let h = s.spawn(|| details(&scans, &sync));
        while !h.is_finished() {
            let next = jobs.lock().unwrap().pop();
            if let Some(job) = next {
                job();
            }
            std::thread::yield_now();
        }
        h.join().unwrap()
    });
    assert!(ok.is_ok(), "{ok:?}");
}

/// A5: Repair with sync off is refused in the arm's own transaction and arms nothing, even
/// past the page's check (a host that says set up). Control: on, it arms.
#[test]
fn damaged_repair_with_sync_off_arms_nothing() {
    let dir = TempDir::new("damaged-repair-off");
    let svc = services(&dir);
    assert_eq!(
        earshot_runtime::sync::start_repair(&svc).unwrap(),
        earshot_runtime::sync::RepairStart::SyncOff
    );
    assert_eq!(earshot_runtime::sync::repair_step(&svc).unwrap(), None);
    let w = Watch::new(&svc);
    let refused = repair(&w.scans, &Arc::new(SyncHost::new(set_up_vault()))).unwrap_err();
    assert_eq!(refused.message, SYNC_OFF_COPY);
    earshot_runtime::sync::set_sync_on(&svc, true).unwrap();
    assert_eq!(
        earshot_runtime::sync::start_repair(&svc).unwrap(),
        earshot_runtime::sync::RepairStart::Started
    );
}
