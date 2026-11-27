//! T14.47: the one-off sweep of replaced transcript text (docs/sync-spec.md 15.6). Clock and
//! pause are injected. Every name holds `sync` so `cargo test -p earshot-runtime sync` selects
//! them. Every word is invented.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use earshot_core::asr::{MeetingTime, Segment, Word};
use earshot_core::capture::TrackId;
use earshot_core::diar::SpeakerRole;
use earshot_core::identity::{DeviceId, EntityId};
use earshot_core::model::{
    ClockDomain, Meeting, MeetingRepo, SegmentRow, Speaker, Track, TranscriptPass,
};
use earshot_core::prompt::Template;
use earshot_core::voiceprint::Stamp;
use rand_chacha::ChaCha8Rng;
use rand_core::SeedableRng;

use super::*;
use crate::services::MeetingJob;
use crate::sync::TickSeen;
use crate::testutil::{TempDir, services};

/// A meeting in the old form: drafts on both tracks, final lines on the mic only, a final
/// tombstone that kept its text, made on this device or on `origin`.
struct OldForm {
    meeting: EntityId,
    mic_drafts: Vec<EntityId>,
    reference_draft: EntityId,
    tombstone: EntityId,
}

fn old_form(svc: &Services, seed: u64, origin: Option<DeviceId>) -> OldForm {
    let clock = Arc::clone(&svc.clock);
    let mut rng = ChaCha8Rng::seed_from_u64(seed);
    let mut db = svc.store();
    let (store, hlc) = db.parts();
    let mut stamp = Stamp {
        clock: &*clock,
        hlc,
        rng: &mut rng,
    };
    let mut m = Meeting::new("Invented review", 0, 600, Template::General, &mut stamp);
    if let Some(origin) = origin {
        m.meta.origin_device_id = origin;
    }
    let mic = Track::new(m.id, TrackId::Mic, None, ClockDomain::Replay, &mut stamp);
    let reference = Track::new(
        m.id,
        TrackId::Reference,
        None,
        ClockDomain::Replay,
        &mut stamp,
    );
    let me = Speaker::new(m.id, SpeakerRole::Me, "Me", None, &mut stamp);
    store.put_meeting(m.clone()).unwrap();
    store.put_track(mic.clone()).unwrap();
    store.put_track(reference.clone()).unwrap();
    store.put_speaker(me.clone()).unwrap();
    let seg = |track: EntityId, text: &str, start: u64, stamp: &mut Stamp<'_, _, _>| {
        let words: Vec<Word> = text
            .split(' ')
            .enumerate()
            .map(|(i, w)| Word::new(w, start + 400 * i as u64, start + 400 * i as u64 + 300, 0.9))
            .collect();
        Segment::from_words(
            EntityId::generate(stamp.clock, &mut *stamp.rng),
            track,
            words,
            MeetingTime::from_millis(start),
        )
    };
    let mut mic_drafts = Vec::new();
    for (k, text) in ["shall we ship", "on friday then"].iter().enumerate() {
        let s = seg(mic.id, text, 1_000 + 3_000 * k as u64, &mut stamp);
        mic_drafts.push(s.id);
        let row = SegmentRow::new(m.id, TranscriptPass::Live, s, stamp.hlc, stamp.clock);
        store.put_segment(row).unwrap();
    }
    let s = seg(reference.id, "far end words", 2_000, &mut stamp);
    let reference_draft = s.id;
    store
        .put_segment(SegmentRow::new(
            m.id,
            TranscriptPass::Live,
            s,
            stamp.hlc,
            stamp.clock,
        ))
        .unwrap();
    let mut s = seg(mic.id, "shall we ship on friday", 1_000, &mut stamp);
    s.speaker_id = Some(me.id);
    store
        .put_segment(SegmentRow::new(
            m.id,
            TranscriptPass::Final,
            s,
            stamp.hlc,
            stamp.clock,
        ))
        .unwrap();
    let mut s = seg(mic.id, "an older final line", 1_000, &mut stamp);
    s.speaker_id = Some(me.id);
    let tombstone = s.id;
    let mut row = SegmentRow::new(m.id, TranscriptPass::Final, s, stamp.hlc, stamp.clock);
    row.meta.mark_deleted(stamp.hlc, stamp.clock);
    store.put_segment(row).unwrap();
    OldForm {
        meeting: m.id,
        mic_drafts,
        reference_draft,
        tombstone,
    }
}

fn row(svc: &Services, meeting: EntityId, id: EntityId) -> SegmentRow {
    svc.store()
        .segments(meeting)
        .unwrap()
        .into_iter()
        .find(|r| r.segment.id == id)
        .unwrap()
}

#[derive(Default)]
struct FakeSync {
    seen: Mutex<TickSeen>,
    waiting: AtomicU64,
}

impl SweepSync for FakeSync {
    fn seen(&self) -> TickSeen {
        *self.seen.lock().unwrap()
    }
    fn waiting_to_send(&self) -> Result<u64, RuntimeError> {
        Ok(self.waiting.load(Ordering::Relaxed))
    }
}

type Hook = Box<dyn Fn(usize, Duration) + Send + Sync>;

/// A clock that moves only by the pauses, a count of them, a hook called at each, and an
/// optional stop after `stop_after` pauses.
struct FakeWait {
    elapsed: Mutex<Duration>,
    pauses: Mutex<usize>,
    hook: Hook,
    stop_after: Option<usize>,
}

impl FakeWait {
    fn new() -> Self {
        Self::with(Box::new(|_, _| {}))
    }
    fn with(hook: Hook) -> Self {
        Self {
            elapsed: Mutex::new(Duration::ZERO),
            pauses: Mutex::new(0),
            hook,
            stop_after: None,
        }
    }
    fn pauses(&self) -> usize {
        *self.pauses.lock().unwrap()
    }
}

impl SweepWait for FakeWait {
    fn elapsed(&self) -> Duration {
        *self.elapsed.lock().unwrap()
    }
    fn pause(&self, d: Duration) -> bool {
        let n = {
            let mut p = self.pauses.lock().unwrap();
            *p += 1;
            *p
        };
        *self.elapsed.lock().unwrap() += d;
        (self.hook)(n, d);
        self.stop_after.is_none_or(|s| n < s)
    }
}

fn sync_off() -> FakeSync {
    FakeSync::default()
}

/// A store in the old form is cleared: drafts only in the meetings this device owns, and only
/// on tracks with final lines; text-bearing tombstones everywhere. The record is written with
/// its pinned bytes; a second run writes nothing.
#[test]
fn sync_sweep_clears_the_old_form_and_records_it() {
    let dir = TempDir::new("sweep-old-form");
    let (svc, _clock) = services(&dir);
    let mine = old_form(&svc, 1, None);
    let theirs = old_form(&svc, 2, Some(DeviceId::from_bytes([0x0B; 16])));
    let report = sweep(&svc, &sync_off(), &FakeWait::new()).unwrap();
    assert!(report.done && !report.stopped, "{report:?}");
    assert_eq!(report.failed, 0);
    // Mine: the mic drafts and the tombstone cleared; the reference draft (no final line on its
    // track, so the meeting still shows it there if nothing else) kept. No audio rows exist:
    // an owner whose audio retention already deleted still clears its draft.
    for id in &mine.mic_drafts {
        assert!(row(&svc, mine.meeting, *id).is_cleared());
    }
    assert!(row(&svc, mine.meeting, mine.tombstone).is_cleared());
    assert!(row(&svc, mine.meeting, mine.reference_draft).holds_text());
    // Theirs: drafts kept, the tombstone cleared.
    for id in &theirs.mic_drafts {
        let r = row(&svc, theirs.meeting, *id);
        assert!(r.holds_text() && !r.meta.deleted);
    }
    assert!(row(&svc, theirs.meeting, theirs.tombstone).is_cleared());
    assert_eq!(report.rows, 4);
    // The record, pinned.
    let bytes = svc.store().local_value(SWEPT_KEY).unwrap().unwrap();
    assert_eq!(
        bytes,
        [1, 0, 0, 0]
            .iter()
            .copied()
            .chain(*br#"{"done":true}"#)
            .collect::<Vec<u8>>()
    );
    assert_eq!(swept(&svc).unwrap(), Swept::Done);
    // A second run writes nothing, and with the record gone a run finds nothing left.
    let again = sweep(&svc, &sync_off(), &FakeWait::new()).unwrap();
    assert_eq!((again.rows, again.done), (0, true));
    svc.store().delete_local_value(SWEPT_KEY).unwrap();
    let probe = sweep(&svc, &sync_off(), &FakeWait::new()).unwrap();
    assert_eq!((probe.rows, probe.done), (0, true), "{probe:?}");
}

/// The record's bytes are pinned.
#[test]
fn sync_sweep_record_bytes_are_pinned() {
    assert_eq!(record_bytes(), b"\x01\x00\x00\x00{\"done\":true}".to_vec());
}

/// An old-form meeting whose final lines are all tombstones keeps its draft and still shows it.
#[test]
fn sync_sweep_keeps_the_draft_of_a_meeting_with_no_live_final_line() {
    let dir = TempDir::new("sweep-no-final");
    let (svc, _clock) = services(&dir);
    let m = old_form(&svc, 3, None);
    // The live final line is taken out too.
    let clock = Arc::clone(&svc.clock);
    {
        let mut db = svc.store();
        let (store, hlc) = db.parts();
        for mut r in store.segments(m.meeting).unwrap() {
            if r.pass == TranscriptPass::Final && !r.meta.deleted {
                r.meta.mark_deleted(hlc, &*clock);
                store.put_segment(r).unwrap();
            }
        }
    }
    sweep(&svc, &sync_off(), &FakeWait::new()).unwrap();
    for id in &m.mic_drafts {
        assert!(row(&svc, m.meeting, *id).holds_text());
    }
    let shown = svc.store().shown_lines(m.meeting).unwrap();
    assert_eq!(shown.pass, Some(TranscriptPass::Live));
    assert_eq!(shown.lines.len(), 3);
}

/// Stopped part way, it writes no record; run again, it finishes.
#[test]
fn sync_sweep_stopped_part_way_finishes_when_run_again() {
    let dir = TempDir::new("sweep-stopped");
    let (svc, _clock) = services(&dir);
    let a = old_form(&svc, 4, None);
    let b = old_form(&svc, 5, None);
    let mut wait = FakeWait::new();
    wait.stop_after = Some(1);
    let first = sweep(&svc, &sync_off(), &wait).unwrap();
    assert!(first.stopped && !first.done, "{first:?}");
    assert_eq!(swept(&svc).unwrap(), Swept::Absent);
    let second = sweep(&svc, &sync_off(), &FakeWait::new()).unwrap();
    assert!(second.done);
    for m in [&a, &b] {
        assert!(row(&svc, m.meeting, m.tombstone).is_cleared());
    }
}

/// With sync running the sweep starts after the first completed pull, or at the cap; with sync
/// off at once.
#[test]
fn sync_sweep_start_waits_for_a_completed_pull_or_the_cap() {
    let dir = TempDir::new("sweep-start");
    let (svc, _clock) = services(&dir);
    old_form(&svc, 6, None);
    // Running, no pull yet: the cap.
    let sync = FakeSync::default();
    *sync.seen.lock().unwrap() = TickSeen {
        running: true,
        last_ok: Some(false),
        text_emptied: 0,
        pull_completed: false,
    };
    let wait = FakeWait::new();
    let r = sweep(&svc, &sync, &wait).unwrap();
    assert!(r.done);
    assert!(wait.elapsed() >= START_CAP, "{:?}", wait.elapsed());
    // A completed pull lets it start at once (the next start, record removed).
    svc.store().delete_local_value(SWEPT_KEY).unwrap();
    let sync = Arc::new(FakeSync::default());
    *sync.seen.lock().unwrap() = TickSeen {
        running: true,
        last_ok: Some(true),
        text_emptied: 0,
        pull_completed: false,
    };
    let flip = Arc::clone(&sync);
    let wait = FakeWait::with(Box::new(move |n, _| {
        if n == 3 {
            flip.seen.lock().unwrap().pull_completed = true;
        }
    }));
    sweep(&svc, &*sync, &wait).unwrap();
    assert!(wait.elapsed() < START_CAP);
    assert_eq!(wait.pauses(), 3);
    // Sync off: no wait.
    svc.store().delete_local_value(SWEPT_KEY).unwrap();
    let wait = FakeWait::new();
    sweep(&svc, &sync_off(), &wait).unwrap();
    assert_eq!(wait.pauses(), 0);
}

/// An unreadable record means the sweep runs again, is reported as such, and is replaced.
#[test]
fn sync_sweep_an_unreadable_record_sweeps_again_and_is_replaced() {
    let dir = TempDir::new("sweep-unreadable");
    let (svc, _clock) = services(&dir);
    let m = old_form(&svc, 7, None);
    svc.store()
        .set_local_value(SWEPT_KEY, b"\x02\x00\x00\x00{\"done\":true}")
        .unwrap();
    assert_eq!(swept(&svc).unwrap(), Swept::Unreadable);
    let r = sweep(&svc, &sync_off(), &FakeWait::new()).unwrap();
    assert!(r.unreadable_record && r.done);
    assert!(row(&svc, m.meeting, m.tombstone).is_cleared());
    assert_eq!(swept(&svc).unwrap(), Swept::Done);
}

/// Nothing is written while a meeting records; a meeting leased here is done later.
#[test]
fn sync_sweep_waits_for_recordings_and_does_leased_meetings_later() {
    let dir = TempDir::new("sweep-leases");
    let (svc, _clock) = services(&dir);
    let a = old_form(&svc, 8, None);
    let b = old_form(&svc, 9, None);
    let recording = Arc::new(Mutex::new(Some(
        svc.lease_meeting_for(a.meeting, MeetingJob::Recording)
            .unwrap(),
    )));
    let busy = Arc::new(Mutex::new(Some(
        svc.lease_meeting_for(b.meeting, MeetingJob::Processing)
            .unwrap(),
    )));
    let written_while_recording = Arc::new(AtomicBool::new(false));
    let (rec, bz, seen, svc2, am) = (
        Arc::clone(&recording),
        Arc::clone(&busy),
        Arc::clone(&written_while_recording),
        svc.clone(),
        a.meeting,
    );
    let wait = FakeWait::with(Box::new(move |n, d| {
        if rec.lock().unwrap().is_some() {
            let cleared = svc2
                .store()
                .segments(am)
                .unwrap()
                .iter()
                .any(SegmentRow::is_cleared);
            seen.fetch_or(cleared, Ordering::Relaxed);
        }
        if n == 5 {
            rec.lock().unwrap().take();
        }
        if d == LEASED_RETRY {
            bz.lock().unwrap().take();
        }
    }));
    // The leased meeting (b) is not the one recording (a): a's lease keeps everything waiting.
    let r = sweep(&svc, &sync_off(), &wait).unwrap();
    assert!(r.done, "{r:?}");
    assert!(!written_while_recording.load(Ordering::Relaxed));
    assert!(row(&svc, a.meeting, a.tombstone).is_cleared());
    assert!(row(&svc, b.meeting, b.tombstone).is_cleared());
}

/// A meeting leased after the sweep first looked at it, while it waited, is looked at again
/// under the store lock before its batch: nothing is written while the lease is held, and it is
/// done later.
#[test]
fn sync_sweep_looks_at_the_lease_again_before_each_batch() {
    let dir = TempDir::new("sweep-lease-again");
    let (svc, _clock) = services(&dir);
    let a = old_form(&svc, 15, None);
    let b = old_form(&svc, 16, None);
    let recording = Arc::new(Mutex::new(Some(
        svc.lease_meeting_for(a.meeting, MeetingJob::Recording)
            .unwrap(),
    )));
    let lease_b: Arc<Mutex<Option<crate::services::MeetingLease>>> = Arc::default();
    let written_while_leased = Arc::new(AtomicBool::new(false));
    let (rec, lb, seen, svc2, bm) = (
        Arc::clone(&recording),
        Arc::clone(&lease_b),
        Arc::clone(&written_while_leased),
        svc.clone(),
        b.meeting,
    );
    let wait = FakeWait::with(Box::new(move |n, d| {
        if lb.lock().unwrap().is_some() {
            let cleared = svc2
                .store()
                .segments(bm)
                .unwrap()
                .iter()
                .any(SegmentRow::is_cleared);
            seen.fetch_or(cleared, Ordering::Relaxed);
        }
        // While the sweep waits for the recording, a job takes `b`; then the recording ends.
        if n == 1 {
            *lb.lock().unwrap() = Some(svc2.lease_meeting_for(bm, MeetingJob::Processing).unwrap());
        }
        if n == 2 {
            rec.lock().unwrap().take();
        }
        if d == LEASED_RETRY && n > 2 {
            lb.lock().unwrap().take();
        }
    }));
    let r = sweep(&svc, &sync_off(), &wait).unwrap();
    assert!(r.done, "{r:?}");
    assert!(!written_while_leased.load(Ordering::Relaxed));
    assert!(row(&svc, a.meeting, a.tombstone).is_cleared());
    assert!(row(&svc, b.meeting, b.tombstone).is_cleared());
}

/// Both waits: while sync runs and its last tick succeeded, the sweep waits while its last tick
/// emptied text or more than [`OUTBOX_WAIT`] rows wait to send; it goes on when the tick fails
/// or sync stops.
#[test]
fn sync_sweep_gives_way_to_sync_and_goes_on_when_sync_stops() {
    for case in ["text_emptied", "outbox", "tick_failed"] {
        let dir = TempDir::new("sweep-waits");
        let (svc, _clock) = services(&dir);
        let m = old_form(&svc, 10, None);
        let sync = Arc::new(FakeSync::default());
        *sync.seen.lock().unwrap() = TickSeen {
            running: true,
            last_ok: Some(true),
            text_emptied: usize::from(case == "text_emptied"),
            pull_completed: true,
        };
        if case == "outbox" {
            sync.waiting.store(OUTBOX_WAIT + 1, Ordering::Relaxed);
        }
        let (s, svc2, mm) = (Arc::clone(&sync), svc.clone(), m.meeting);
        let wrote_early = Arc::new(AtomicBool::new(false));
        let early = Arc::clone(&wrote_early);
        let wait = FakeWait::with(Box::new(move |n, _| {
            if n < 4 {
                let cleared = svc2
                    .store()
                    .segments(mm)
                    .unwrap()
                    .iter()
                    .any(SegmentRow::is_cleared);
                early.fetch_or(cleared, Ordering::Relaxed);
            }
            if n == 4 {
                if case == "tick_failed" {
                    s.seen.lock().unwrap().last_ok = Some(false);
                } else {
                    s.seen.lock().unwrap().running = false;
                }
            }
        }));
        if case == "tick_failed" {
            sync.seen.lock().unwrap().text_emptied = 1;
        }
        let r = sweep(&svc, &*sync, &wait).unwrap();
        assert!(r.done, "{case}: {r:?}");
        assert!(!wrote_early.load(Ordering::Relaxed), "{case}");
        assert!(wait.pauses() >= 4, "{case}");
        assert!(row(&svc, m.meeting, m.tombstone).is_cleared(), "{case}");
    }
}

/// A meeting that fails with a store error is counted, left as it was, and the record is not
/// written; the rest are done.
#[test]
fn sync_sweep_a_failing_meeting_is_counted_and_the_record_not_written() {
    let dir = TempDir::new("sweep-failing");
    let (svc, _clock) = services(&dir);
    let bad = old_form(&svc, 11, None);
    let good = old_form(&svc, 12, None);
    let hex: String = bad
        .meeting
        .as_uuid()
        .as_bytes()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    {
        let c = rusqlite::Connection::open(svc.data.db_path()).unwrap();
        c.execute_batch(&format!(
            "CREATE TRIGGER refuse BEFORE UPDATE ON segment WHEN OLD.meeting_id = x'{hex}' \
             BEGIN SELECT RAISE(ABORT, 'refused'); END;"
        ))
        .unwrap();
    }
    let r = sweep(&svc, &sync_off(), &FakeWait::new()).unwrap();
    assert_eq!(r.failed, 1, "{r:?}");
    assert!(!r.done);
    assert_eq!(swept(&svc).unwrap(), Swept::Absent);
    assert!(row(&svc, bad.meeting, bad.tombstone).holds_text());
    assert!(row(&svc, good.meeting, good.tombstone).is_cleared());
}

/// The order is this device's own: two devices sort the same meetings differently.
#[test]
fn sync_sweep_order_depends_on_the_device() {
    let meetings: Vec<EntityId> = (0..8u8).map(|n| EntityId::from_bytes([n; 16])).collect();
    let order = |d: u8| {
        let mut m = meetings.clone();
        m.sort_by_key(|m| super::order_key(DeviceId::from_bytes([d; 16]), *m));
        m
    };
    assert_ne!(order(1), order(2));
    assert_eq!(order(1), order(1));
}

/// T14.47 (15.2, "Writers of draft rows"): `merge_speakers` of a speaker with final lines writes
/// no draft row: it moves only rows that name the speaker, which no draft row does.
#[test]
fn edit_merge_speakers_writes_no_draft_row() {
    let dir = TempDir::new("merge-no-draft");
    let (svc, _clock) = services(&dir);
    let m = old_form(&svc, 13, None);
    let alice = {
        let clock = Arc::clone(&svc.clock);
        let mut rng = ChaCha8Rng::seed_from_u64(14);
        let mut db = svc.store();
        let (store, hlc) = db.parts();
        let mut stamp = Stamp {
            clock: &*clock,
            hlc,
            rng: &mut rng,
        };
        let alice = Speaker::new(m.meeting, SpeakerRole::Local, "Alice", None, &mut stamp);
        store.put_speaker(alice.clone()).unwrap();
        // A final line of Alice's.
        let mut line = store
            .segments(m.meeting)
            .unwrap()
            .into_iter()
            .find(|r| r.pass == TranscriptPass::Final && !r.meta.deleted)
            .unwrap();
        line.segment.speaker_id = Some(alice.id);
        line.meta.touch(stamp.hlc, stamp.clock);
        store.put_segment(line).unwrap();
        alice.id
    };
    let me = svc
        .store()
        .speakers(m.meeting)
        .unwrap()
        .into_iter()
        .find(|s| s.role == SpeakerRole::Me)
        .unwrap()
        .id;
    let drafts = |svc: &Services| -> Vec<SegmentRow> {
        svc.store()
            .segments(m.meeting)
            .unwrap()
            .into_iter()
            .filter(|r| r.pass == TranscriptPass::Live)
            .collect()
    };
    let before = drafts(&svc);
    crate::edit::merge_speakers(&svc, m.meeting, me, alice).unwrap();
    assert_eq!(drafts(&svc), before, "no draft row written");
}

// ---- 15.2 "Writers of draft rows": the scan ----

/// The shipped sources the scan reads: every `.rs` file of the crates that write segment rows
/// (core, store, runtime, CLI, worker, desktop), test files and test modules left out.
fn shipped_sources() -> Vec<(String, String)> {
    fn walk(dir: &std::path::Path, root: &std::path::Path, out: &mut Vec<(String, String)>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for e in entries.flatten() {
            let path = e.path();
            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if path.is_dir() {
                if name != "tests" {
                    walk(&path, root, out);
                }
                continue;
            }
            if path.extension().is_none_or(|x| x != "rs")
                || name.ends_with("tests.rs")
                || matches!(name, "testutil.rs" | "repo_contract.rs")
            {
                continue;
            }
            let text = std::fs::read_to_string(&path).unwrap();
            // A test module at the end of a file is not shipped.
            let text = match text.find("#[cfg(test)]\nmod ") {
                Some(at) => text[..at].to_owned(),
                None => text,
            };
            let rel = path
                .strip_prefix(root)
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/");
            out.push((rel, text));
        }
    }
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let root = root.canonicalize().unwrap();
    let mut out = Vec::new();
    for dir in [
        "crates/earshot-core/src",
        "crates/earshot-store/src",
        "crates/earshot-runtime/src",
        "crates/earshot-cli/src",
        "crates/earshot-worker/src",
        "apps/desktop/src-tauri/src",
    ] {
        let d = root.join(dir);
        assert!(d.is_dir(), "{dir}");
        walk(&d, &root, &mut out);
    }
    assert!(out.len() > 100, "{} files read", out.len());
    out
}

/// The name of the function that encloses byte `at` of `src` (the last `fn name(` before it).
fn enclosing_fn(src: &str, at: usize) -> &str {
    let before = &src[..at];
    let Some(start) = before.rfind("fn ") else {
        return "";
    };
    let rest = &before[start + 3..];
    let end = rest
        .find(|c: char| !(c.is_alphanumeric() || c == '_'))
        .unwrap_or(rest.len());
    &rest[..end]
}

/// The body of the function `name` in `src` (from its `fn name(` to the next top-level `\n}`).
fn fn_body<'a>(src: &'a str, name: &str) -> &'a str {
    let header = format!("fn {name}(");
    let Some(start) = src.find(&header) else {
        return "";
    };
    let end = src[start..].find("\n}\n").map_or(src.len(), |i| start + i);
    &src[start..end]
}

/// The arguments of the call whose `(` is at `open` in `src`.
fn call_args(src: &str, open: usize) -> &str {
    let mut depth = 0usize;
    for (i, c) in src[open..].char_indices() {
        match c {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    return &src[open + 1..open + i];
                }
            }
            _ => {}
        }
    }
    &src[open..]
}

/// The named sites that give a segment a speaker (15.2): the final pass's labelling of its own
/// Final rows, `move_line` (after `final_line`) and `merge_speakers` (behind its guard).
const GIVE_SITES: &[(&str, &str)] = &[
    ("crates/earshot-runtime/src/post/mod.rs", "run_reporting"),
    ("crates/earshot-core/src/diar/assign.rs", "label_segments"),
    ("crates/earshot-runtime/src/edit.rs", "move_line"),
    ("crates/earshot-runtime/src/edit.rs", "merge_speakers"),
];

/// The functions that rebuild a segment from another and keep its speaker
/// (`x.speaker_id = y.speaker_id`): echo cleaning, the split of a segment by speaker turns and
/// the drop of words under edits.
const COPY_SITES: &[(&str, &str)] = &[
    (
        "crates/earshot-core/src/asr/echo.rs",
        "dedupe_echo_segments",
    ),
    ("crates/earshot-core/src/asr/edit_span.rs", "drop_masked"),
    (
        "crates/earshot-core/src/diar/assign.rs",
        "label_segment_speakers_keeping",
    ),
    ("crates/earshot-runtime/src/live/echo.rs", "mic_partial"),
    (
        "crates/earshot-runtime/src/post/mod.rs",
        "text_and_speaker_rules",
    ),
];

/// What the scan of 15.2 finds wrong in `sources` (path, text): constructions of a
/// `pass = Live` segment row outside `store_live`, assignments to a segment's `speaker_id`
/// with a value other than `None` (or a copy of a segment's own speaker) outside the named
/// sites, `merge_speakers` without its guard, and a call of `replace_segments` in shipped code.
fn draft_writer_findings(sources: &[(String, String)]) -> Vec<String> {
    let live = concat!("TranscriptPass::", "Live");
    let mut found = Vec::new();
    for (path, src) in sources {
        // Comments do not count.
        let code: String = src
            .lines()
            .map(|l| {
                if l.trim_start().starts_with("//") {
                    ""
                } else {
                    l
                }
            })
            .collect::<Vec<_>>()
            .join("\n");
        // (a) constructions.
        let new = concat!("SegmentRow::", "new(");
        for (at, _) in code.match_indices(new) {
            let args = call_args(&code, at + new.len() - 1);
            if args.contains(live)
                && !(path == "crates/earshot-runtime/src/live/mod.rs"
                    && enclosing_fn(&code, at) == "store_live")
            {
                found.push(format!(
                    "{path}: a draft row made in {}",
                    enclosing_fn(&code, at)
                ));
            }
        }
        for pattern in [format!("pass: {live}"), format!(".pass = {live}")] {
            for (at, _) in code.match_indices(&pattern) {
                found.push(format!(
                    "{path}: a draft row made in {}",
                    enclosing_fn(&code, at)
                ));
            }
        }
        // (b) a speaker given to a segment: an assignment, or a struct literal's field.
        let mut sites: Vec<(usize, &str)> = code.match_indices(".speaker_id = ").collect();
        sites.extend(code.match_indices("speaker_id: Some("));
        for (at, pattern) in sites {
            let value_start = at + pattern.len();
            let value = if pattern.ends_with('(') {
                "Some(..)"
            } else {
                code[value_start..].split(';').next().unwrap_or("").trim()
            };
            if value == "None" {
                continue;
            }
            // `x.speaker_id = y.speaker_id`: a rebuilt segment keeping its source's speaker.
            let copy = value.ends_with(".speaker_id")
                && value
                    .chars()
                    .all(|c| c.is_alphanumeric() || c == '_' || c == '.');
            let f = enclosing_fn(&code, at);
            let named = if copy {
                COPY_SITES.contains(&(path.as_str(), f))
            } else {
                GIVE_SITES.contains(&(path.as_str(), f))
            };
            if !named {
                let what = if copy { "copied onto" } else { "given to" };
                found.push(format!("{path}: a speaker {what} a segment in {f}"));
            }
        }
        if path == "crates/earshot-runtime/src/edit.rs" {
            let body = fn_body(&code, "merge_speakers");
            if !body.contains("speaker_id != Some(merge)") {
                found.push(format!("{path}: merge_speakers without its guard"));
            }
        }
        // `replace_segments` (core) would write tombstones back as live rows: no shipped caller.
        let call = concat!("replace_", "segments(");
        for (at, _) in code.match_indices(call) {
            let before = &code[..at];
            let defines = before.ends_with("pub fn ") || before.ends_with("fn ");
            let named = path == "crates/earshot-store/src/db.rs"
                && enclosing_fn(&code, at) == "replace_segments";
            if !defines && !named {
                found.push(format!(
                    "{path}: replace_segments called in {}",
                    enclosing_fn(&code, at)
                ));
            }
        }
    }
    found
}

/// T14.47 (docs/sync-spec.md 15.2, "Writers of draft rows"): a live `pass = Live` row is written
/// only by the live session while it records, and never carries a speaker; `merge_speakers`
/// keeps its guard; `replace_segments` stays out of shipped code. Two mutation controls.
#[test]
fn edit_draft_rows_are_written_only_by_the_live_session() {
    let sources = shipped_sources();
    let found = draft_writer_findings(&sources);
    assert!(found.is_empty(), "{found:#?}");
    // Mutation control 1: an added construction outside `store_live`.
    let mut added = sources.clone();
    added.push((
        "crates/earshot-runtime/src/somewhere.rs".into(),
        format!(
            "fn sneaky() {{\n    let row = SegmentRow::new(m, {}, s, hlc, clock);\n}}\n",
            concat!("TranscriptPass::", "Live")
        ),
    ));
    assert_eq!(draft_writer_findings(&added).len(), 1);
    // Mutation control 3: a struct literal giving a segment a speaker, and a copy outside the
    // named rebuild functions.
    let mut literal = sources.clone();
    literal.push((
        "crates/earshot-runtime/src/somewhere.rs".into(),
        "fn sneaky() {\n    let s = Segment { speaker_id: Some(x), ..s };\n    \
         t.speaker_id = s.speaker_id;\n}\n"
            .into(),
    ));
    let found = draft_writer_findings(&literal);
    assert_eq!(found.len(), 2, "{found:#?}");
    // Mutation control 2: `merge_speakers` without its guard.
    let unguarded: Vec<(String, String)> = sources
        .iter()
        .map(|(p, t)| {
            if p == "crates/earshot-runtime/src/edit.rs" {
                (
                    p.clone(),
                    t.replace("row.segment.speaker_id != Some(merge)", "false"),
                )
            } else {
                (p.clone(), t.clone())
            }
        })
        .collect();
    let found = draft_writer_findings(&unguarded);
    assert!(
        found
            .iter()
            .any(|f| f.contains("merge_speakers without its guard")),
        "{found:#?}"
    );
}
