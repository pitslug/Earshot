//! IPC contract tests: the meeting list, errors, the compact window and the bindings.

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use earshot_core::asr::{BackendInfo, BackendKind};
use earshot_core::identity::{Clock, EntityId};
use earshot_core::model::{Meeting, Stamp, TranscriptPass};
use earshot_core::prompt::Template;
use earshot_runtime::{MeetingJob, RuntimeError, Services};
use earshot_store::{DataDir, StoreError};
use rand_chacha::ChaCha8Rng;
use rand_core::SeedableRng;

use super::*;
use crate::testutil::{T0, TempDir, services};

/// A meeting row written the way the runtime writes one (`live::start_live`): stamped
/// with the services' clock and HLC under the store lock. Distinct seeds per call.
fn meeting(
    svc: &Services,
    title: &str,
    started: u64,
    duration: Option<u64>,
    seed: u64,
) -> EntityId {
    let clock: Arc<dyn Clock + Send + Sync> = Arc::clone(&svc.clock);
    let mut rng = ChaCha8Rng::seed_from_u64(seed);
    let mut db = svc.store();
    let (store, hlc) = db.parts();
    let mut stamp = Stamp {
        clock: &*clock,
        hlc,
        rng: &mut rng,
    };
    let mut m = Meeting::new(title, started, 600, Template::General, &mut stamp);
    m.duration_ms = duration;
    let id = m.id;
    store.put_meeting(m).unwrap();
    id
}

/// Records that the final pass ran on `meeting` (what makes a meeting without speech
/// `Ready`).
fn final_pass_ran(svc: &Services, meeting: EntityId) {
    let mut db = svc.store();
    earshot_runtime::backends::record_pass_backend(
        &mut db,
        meeting,
        TranscriptPass::Final,
        None,
        &BackendInfo {
            kind: BackendKind::Cpu,
            device: "cpu".into(),
            model: "ggml-base.en".into(),
        },
    )
    .unwrap();
}

#[test]
fn list_meetings_returns_live_meetings_newest_first_without_deleted_ones() {
    let dir = TempDir::new("list");
    let svc = services(&dir);
    let old = meeting(&svc, "Budget review", T0, Some(1_800_000), 1);
    let gone = meeting(&svc, "Deleted", T0 + 60_000, Some(60_000), 2);
    let new = meeting(&svc, "Design sync", T0 + 3_600_000, None, 3);
    let mid = meeting(&svc, "", T0 + 1_800_000, Some(900_000), 4);
    svc.delete_meeting(gone).unwrap();

    let list = meeting_list(&svc).unwrap();
    let ids: Vec<String> = list.iter().map(|m| m.id.clone()).collect();
    assert_eq!(ids, [new.to_string(), mid.to_string(), old.to_string()]);
    assert_eq!(list[0].title, "Design sync");
    assert_eq!(
        list[0].started_at_ms,
        i64::try_from(T0 + 3_600_000).unwrap()
    );
    assert_eq!(list[0].duration_ms, None);
    assert_eq!(list[1].title, "");
    assert_eq!(list[1].duration_ms, Some(900_000));
}

#[test]
fn list_meetings_states_follow_the_runtime_activity() {
    let dir = TempDir::new("states");
    let svc = services(&dir);
    let recording = meeting(&svc, "Recording", T0 + 5_000, None, 11);
    let processing = meeting(&svc, "Processing", T0 + 4_000, Some(60_000), 12);
    let _interrupted = meeting(&svc, "Interrupted", T0 + 3_000, None, 13);
    let _unprocessed = meeting(&svc, "Unprocessed", T0 + 2_000, Some(60_000), 14);
    let ready = meeting(&svc, "Ready", T0 + 1_000, Some(60_000), 15);
    final_pass_ran(&svc, ready);
    let _rec = svc
        .lease_meeting_for(recording, MeetingJob::Recording)
        .unwrap();
    let _proc = svc
        .lease_meeting_for(processing, MeetingJob::Processing)
        .unwrap();

    let states: Vec<(String, MeetingState)> = meeting_list(&svc)
        .unwrap()
        .into_iter()
        .map(|m| (m.title, m.state))
        .collect();
    let want = [
        ("Recording", MeetingState::Recording),
        ("Processing", MeetingState::Processing),
        ("Interrupted", MeetingState::Interrupted),
        ("Unprocessed", MeetingState::Unprocessed),
        ("Ready", MeetingState::Ready),
    ];
    assert_eq!(states, want.map(|(t, s)| (t.to_owned(), s)).to_vec());
    // The leases are this process's: once dropped the meetings read from their rows.
    drop(_rec);
    drop(_proc);
    let after: Vec<MeetingState> = meeting_list(&svc)
        .unwrap()
        .into_iter()
        .map(|m| m.state)
        .collect();
    assert_eq!(
        after[..2],
        [MeetingState::Interrupted, MeetingState::Unprocessed]
    );
}

/// Adds a mic track to `meeting` and one segment of `pass` on it, tombstoned when `gone`.
fn segment(svc: &Services, meeting: EntityId, pass: TranscriptPass, gone: bool, seed: u64) {
    use earshot_core::asr::{MeetingTime, Segment, Word};
    use earshot_core::capture::TrackId;
    use earshot_core::model::{ClockDomain, MeetingRepo, SegmentRow, Track};
    let clock: Arc<dyn Clock + Send + Sync> = Arc::clone(&svc.clock);
    let mut rng = ChaCha8Rng::seed_from_u64(seed);
    let mut db = svc.store();
    let (store, hlc) = db.parts();
    let mut stamp = Stamp {
        clock: &*clock,
        hlc,
        rng: &mut rng,
    };
    let track = Track::new(meeting, TrackId::Mic, None, ClockDomain::Replay, &mut stamp);
    store.put_track(track.clone()).unwrap();
    let s = Segment::from_words(
        EntityId::generate(stamp.clock, &mut *stamp.rng),
        track.id,
        vec![Word::new("hello", 0, 400, 0.9)],
        MeetingTime::ZERO,
    );
    let mut row = SegmentRow::new(meeting, pass, s, &mut *stamp.hlc, stamp.clock);
    if gone {
        row.meta.mark_deleted(&mut *stamp.hlc, stamp.clock);
    }
    store.put_segment(row).unwrap();
}

#[test]
fn list_meetings_final_transcript_answer_equals_loading_the_segments() {
    // T9.24a: the list asks the store whether a live Final segment exists instead of loading
    // every segment. Its states must equal the rule it replaced (`live_segments` of the Final
    // pass non-empty, or the final pass recorded as run).
    let dir = TempDir::new("final-exists");
    let svc = services(&dir);
    let fin = meeting(&svc, "Final", T0 + 6_000, Some(60_000), 31);
    segment(&svc, fin, TranscriptPass::Final, false, 41);
    let draft = meeting(&svc, "Draft", T0 + 5_000, Some(60_000), 32);
    segment(&svc, draft, TranscriptPass::Live, false, 42);
    let empty = meeting(&svc, "Empty", T0 + 4_000, Some(60_000), 33);
    let tomb = meeting(&svc, "Tombstoned final", T0 + 3_000, Some(60_000), 34);
    segment(&svc, tomb, TranscriptPass::Final, true, 43);
    let both = meeting(&svc, "Draft and final", T0 + 2_000, Some(60_000), 35);
    segment(&svc, both, TranscriptPass::Live, false, 44);
    segment(&svc, both, TranscriptPass::Final, false, 45);
    let silent = meeting(&svc, "Final pass, no speech", T0 + 1_000, Some(60_000), 36);
    final_pass_ran(&svc, silent);

    let list = meeting_list(&svc).unwrap();
    let states: Vec<(&str, MeetingState)> =
        list.iter().map(|m| (m.title.as_str(), m.state)).collect();
    assert_eq!(
        states,
        [
            ("Final", MeetingState::Ready),
            ("Draft", MeetingState::Unprocessed),
            ("Empty", MeetingState::Unprocessed),
            ("Tombstoned final", MeetingState::Unprocessed),
            ("Draft and final", MeetingState::Ready),
            ("Final pass, no speech", MeetingState::Ready),
        ]
    );
    // The replaced rule, computed from the loaded segments, gives the same answer.
    let db = svc.store();
    for (item, id) in list.iter().zip([fin, draft, empty, tomb, both, silent]) {
        let old_ready = !earshot_core::model::live_segments(&**db, id, TranscriptPass::Final)
            .unwrap()
            .is_empty()
            || earshot_runtime::backends::pass_backend(&db, id, TranscriptPass::Final, None)
                .unwrap()
                .is_some();
        let old = if old_ready {
            MeetingState::Ready
        } else {
            MeetingState::Unprocessed
        };
        assert_eq!(item.state, old, "{}", item.title);
    }
}

#[test]
fn list_meetings_of_an_empty_store_is_empty() {
    let dir = TempDir::new("empty");
    assert_eq!(meeting_list(&services(&dir)).unwrap(), []);
}

#[test]
fn search_answers_with_the_meetings_found_newest_first() {
    let dir = TempDir::new("search");
    let svc = services(&dir);
    let old = meeting(&svc, "Orchard review", T0, Some(60_000), 21);
    let new = meeting(&svc, "Orchard walk", T0 + 60_000, Some(60_000), 22);
    let gone = meeting(&svc, "Orchard gone", T0 + 120_000, Some(60_000), 23);
    meeting(&svc, "Something else", T0 + 180_000, Some(60_000), 24);
    svc.delete_meeting(gone).unwrap();

    let found = meeting_search(&svc, " ORCHARD ", &CancelToken::new()).unwrap();
    assert!(!found.capped);
    let ids: Vec<&str> = found.items.iter().map(|i| i.id.as_str()).collect();
    assert_eq!(ids, [new.to_string(), old.to_string()]);
    assert_eq!(
        found.items[0],
        SearchResultItem {
            id: new.to_string(),
            title: "Orchard walk".into(),
            started_at_ms: i64::try_from(T0 + 60_000).unwrap(),
            matched: SearchMatch::Title,
            snippet: "Orchard walk".into(),
            line_ms: None,
        }
    );
}

#[test]
fn search_of_nothing_or_no_match_is_an_empty_answer_that_does_not_repeat_the_query() {
    let dir = TempDir::new("search-none");
    let svc = services(&dir);
    meeting(&svc, "Orchard review", T0, Some(60_000), 31);
    for query in ["", "   ", "quinceharvest"] {
        let answer = meeting_search(&svc, query, &CancelToken::new()).unwrap();
        assert_eq!(answer.items, []);
        assert!(!answer.capped);
        let json = serde_json::to_string(&answer).unwrap();
        assert!(!json.contains("quinceharvest"), "{json}");
    }
}

#[test]
fn search_replaced_by_a_newer_one_fails_as_superseded_without_the_query() {
    let dir = TempDir::new("search-superseded");
    let svc = services(&dir);
    meeting(&svc, "Orchard review", T0, Some(60_000), 41);
    let searches = earshot_runtime::search::Searches::default();
    let older = searches.begin();
    let newer = searches.begin();
    let err = meeting_search(&svc, "orchard", &older).unwrap_err();
    assert_eq!(err.kind, SEARCH_SUPERSEDED_KIND);
    assert!(!err.message.contains("orchard"), "{}", err.message);
    assert_eq!(
        meeting_search(&svc, "orchard", &newer).unwrap().items.len(),
        1
    );
}

#[test]
fn search_sent_later_keeps_its_answer_when_an_older_one_begins_after_it() {
    let dir = TempDir::new("search-sent-order");
    let svc = services(&dir);
    meeting(&svc, "Orchard review", T0, Some(60_000), 43);
    let searches = earshot_runtime::search::Searches::default();
    let newer = searches.begin_sent(8);
    let older = searches.begin_sent(7);
    assert_eq!(
        meeting_search(&svc, "orchard", &older).unwrap_err().kind,
        SEARCH_SUPERSEDED_KIND
    );
    assert_eq!(
        meeting_search(&svc, "orchard", &newer).unwrap().items.len(),
        1
    );
}

#[test]
fn app_state_opens_and_reports_the_store() {
    let dir = TempDir::new("state");
    let state = AppState::open(DataDir::new(dir.0.join("fresh")));
    assert!(state.services.is_ok());
    assert!(dir.0.join("fresh").join("earshot.db").is_file());
    assert_eq!(meeting_list(&state.services().unwrap()).unwrap(), []);
}

#[test]
fn ipc_error_keeps_the_runtime_errors_retryability() {
    let id = EntityId::from_bytes([7; 16]);
    let busy = IpcError::from(RuntimeError::MeetingBusy(id));
    assert_eq!(busy.kind, "meeting_busy");
    assert!(busy.retryable);
    // Fixed copy, never the runtime's own text (which names the meeting's id).
    assert_eq!(busy.message, crate::plain_text::MEETING_BUSY);
    assert!(!busy.message.contains(&id.to_string()));
    let conflict = IpcError::from(RuntimeError::Conflict("changed"));
    assert!(conflict.retryable);
    let fixed = IpcError::from(RuntimeError::NoTracks);
    assert_eq!(fixed.kind, "no_tracks");
    assert!(!fixed.retryable);
    let busy_store = || RuntimeError::Store(StoreError::Busy("locked".into()));
    assert!(busy_store().is_retryable());
    let e = IpcError::from(busy_store());
    assert_eq!(e.kind, "store");
    assert!(e.retryable);
}

#[test]
fn app_state_a_data_folder_that_cannot_be_opened_is_its_own_kind_without_its_text() {
    let dir = TempDir::new("state-blocked");
    // A file where the folder should be: neither the folder nor the database can be made.
    let blocked = dir.0.join("blocked");
    std::fs::write(&blocked, b"not a folder").unwrap();
    let state = AppState::open(DataDir::new(blocked.clone()));
    let e = state.services().unwrap_err();
    assert_eq!(e.kind, DATA_FOLDER_KIND);
    assert_eq!(e.message, DATA_FOLDER_COPY);
    assert!(!e.message.contains("blocked"), "{}", e.message);
    // The folder is still known, for the pages that show it.
    assert_eq!(state.data_dir().map(DataDir::root), Some(blocked.as_path()));
    // A database saved by a newer Earshot keeps its own screen.
    let newer = data_folder_error(Some(&RuntimeError::Store(StoreError::SchemaTooNew {
        found: 99,
        supported: 1,
    })));
    assert_eq!(newer.kind, NEWER_DATA_KIND);
    assert_eq!(data_folder_error(None).kind, DATA_FOLDER_KIND);
}

/// A window that records what was done to it.
#[derive(Default)]
struct FakeWindow {
    size: RefCell<(u32, u32)>,
    maximized: RefCell<bool>,
    log: RefCell<Vec<String>>,
}

impl WindowOps for FakeWindow {
    fn size(&self) -> Result<(u32, u32), IpcError> {
        Ok(*self.size.borrow())
    }
    fn maximized(&self) -> Result<bool, IpcError> {
        Ok(*self.maximized.borrow())
    }
    fn unmaximize(&self) -> Result<(), IpcError> {
        *self.maximized.borrow_mut() = false;
        self.log.borrow_mut().push("unmaximize".into());
        Ok(())
    }
    fn maximize(&self) -> Result<(), IpcError> {
        *self.maximized.borrow_mut() = true;
        self.log.borrow_mut().push("maximize".into());
        Ok(())
    }
    fn set_physical_size(&self, size: (u32, u32)) -> Result<(), IpcError> {
        *self.size.borrow_mut() = size;
        self.log
            .borrow_mut()
            .push(format!("size {}x{}", size.0, size.1));
        Ok(())
    }
    fn set_logical_size(&self, (w, h): (f64, f64)) -> Result<(), IpcError> {
        self.log.borrow_mut().push(format!("logical {w}x{h}"));
        Ok(())
    }
    fn set_min_size(&self, size: Option<(f64, f64)>) -> Result<(), IpcError> {
        self.log.borrow_mut().push(format!("min {size:?}"));
        Ok(())
    }
    fn set_always_on_top(&self, on: bool) -> Result<(), IpcError> {
        self.log.borrow_mut().push(format!("on top {on}"));
        Ok(())
    }
}

#[test]
fn compact_window_shrinks_to_an_always_on_top_card_and_restores_the_size_before() {
    let window = FakeWindow {
        size: RefCell::new((1650, 1080)),
        ..FakeWindow::default()
    };
    let compact = CompactWindow::new(Some((800.0, 560.0)));
    compact.set(&window, true).unwrap();
    // Compacting twice changes nothing and keeps the size to go back to.
    compact.set(&window, true).unwrap();
    compact.set(&window, false).unwrap();
    // Expanding when not compact changes nothing.
    compact.set(&window, false).unwrap();
    assert_eq!(
        *window.log.borrow(),
        [
            "min None",
            "logical 300x170",
            "on top true",
            "on top false",
            "min Some((800.0, 560.0))",
            "size 1650x1080",
        ]
    );
}

#[test]
fn compact_window_restores_a_maximised_window_as_maximised() {
    let window = FakeWindow {
        size: RefCell::new((2560, 1380)),
        maximized: RefCell::new(true),
        ..FakeWindow::default()
    };
    let compact = CompactWindow::new(None);
    compact.set(&window, true).unwrap();
    assert!(!*window.maximized.borrow());
    compact.set(&window, false).unwrap();
    assert!(*window.maximized.borrow());
    assert_eq!(
        window.log.borrow().first().map(String::as_str),
        Some("unmaximize")
    );
    assert_eq!(
        window.log.borrow().last().map(String::as_str),
        Some("maximize")
    );
}

/// Where the generated bindings live, relative to this crate.
const BINDINGS_PATH: &str = "../../../packages/ui/src/ipc/bindings.ts";

/// First line of the generated bindings.
const BINDINGS_HEADER: &str = "// Source: apps/desktop/src-tauri/src/ipc.rs. Regenerate with \
     `EARSHOT_UPDATE_BINDINGS=1 cargo test -p earshot-desktop bindings`.";

/// The TypeScript exporter used for the committed bindings.
fn typescript() -> specta_typescript::Typescript {
    specta_typescript::Typescript::default().header(BINDINGS_HEADER)
}

fn bindings_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join(BINDINGS_PATH)
}

/// The generated TypeScript, LF line endings.
fn generated_bindings(dir: &TempDir) -> String {
    let out = dir.0.join("bindings.ts");
    specta_builder().export(typescript(), &out).unwrap();
    std::fs::read_to_string(&out).unwrap().replace("\r\n", "\n")
}

#[test]
fn bindings_are_generated_deterministically() {
    let dir = TempDir::new("bindings-twice");
    assert_eq!(generated_bindings(&dir), generated_bindings(&dir));
}

#[test]
fn bindings_are_up_to_date() {
    let dir = TempDir::new("bindings");
    let fresh = generated_bindings(&dir);
    let path = bindings_path();
    if std::env::var_os("EARSHOT_UPDATE_BINDINGS").is_some_and(|v| v == "1") {
        std::fs::write(&path, &fresh).unwrap();
        return;
    }
    let committed = std::fs::read_to_string(&path).unwrap_or_default();
    assert!(
        committed == fresh,
        "{} is out of date: run `EARSHOT_UPDATE_BINDINGS=1 cargo test -p earshot-desktop \
         bindings` and commit the result",
        path.display()
    );
}
