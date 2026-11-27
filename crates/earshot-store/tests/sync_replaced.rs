//! T14.47: replaced transcript text is erased (docs/sync-spec.md section 15), in the store: the
//! cleared form and its predicate (15.1, 15.2), "lines go out before what they replace" (15.2)
//! and the apply's holds, clearing and counts (15.4), through straight `apply_remote` pages
//! (`cargo test -p earshot-store sync`). Every word is invented.

mod common;

use common::TempDir;
use earshot_core::asr::{MeetingTime, Segment, Word};
use earshot_core::capture::TrackId;
use earshot_core::diar::SpeakerRole;
use earshot_core::identity::{Clock, DeviceId, EntityId, Hlc, HlcTimestamp, ManualClock, SyncMeta};
use earshot_core::model::{
    AudioFile, AudioFileRepo, AudioFormat, ClockDomain, Meeting, MeetingRepo, SegmentRow, Speaker,
    Track, TranscriptPass,
};
use earshot_core::prompt::Template;
use earshot_store::{
    Applied, ApplyRules, HoldReason, Incoming, PushGuard, Store, StoreError, SyncKinds, SyncedRow,
};
use rand_chacha::ChaCha8Rng;
use rand_core::SeedableRng;

const T0: u64 = 1_800_000_000_000;
/// Another computer.
const PEER: DeviceId = DeviceId::from_bytes([0x0B; 16]);

fn id(n: u8) -> EntityId {
    EntityId::from_bytes([0x01, 0x90, 0, 0, 0, 0, 0x70, 0, 0x80, 0, 0, 0, 0, 0, n, 7])
}

const MEETING: u8 = 1;
const MIC: u8 = 2;
const REFERENCE: u8 = 3;
const SPEAKER: u8 = 4;
const OTHER_MEETING: u8 = 5;
const OTHER_TRACK: u8 = 6;

fn ts(dt: u64, node: DeviceId) -> HlcTimestamp {
    HlcTimestamp {
        wall_ms: T0 + dt,
        logical: 0,
        node,
    }
}

fn meta_at(at: HlcTimestamp, origin: DeviceId) -> SyncMeta {
    SyncMeta {
        created_at: at,
        updated_at: at,
        deleted: false,
        origin_device_id: origin,
    }
}

struct Env {
    store: Store,
    clock: ManualClock,
    me: DeviceId,
    hlc: Hlc,
    seq: u64,
    _dir: TempDir,
}

fn env(name: &str) -> Env {
    let dir = TempDir::new(name);
    let mut store = Store::open(&dir.path().join("earshot.db")).unwrap();
    let clock = ManualClock::new(T0 + 10_000);
    let me = store
        .device_id(&clock, &mut ChaCha8Rng::seed_from_u64(9))
        .unwrap();
    let hlc = store.hlc(me).unwrap();
    Env {
        store,
        clock,
        me,
        hlc,
        seq: 0,
        _dir: dir,
    }
}

fn words(list: &[&str], from: u64) -> Vec<Word> {
    list.iter()
        .enumerate()
        .map(|(i, w)| {
            let s = from + 400 * i as u64;
            Word::new(*w, s, s + 300, 0.9)
        })
        .collect()
}

impl Env {
    fn now(&mut self) -> HlcTimestamp {
        self.clock.advance_ms(1);
        self.hlc.now(&self.clock)
    }

    /// The meeting with a mic and a reference track and one speaker, made by `origin`.
    fn meeting(&mut self, origin: DeviceId) {
        let at = self.now();
        let m = Meeting {
            id: id(MEETING),
            meta: meta_at(at, origin),
            title: "Invented planning".into(),
            started_unix_ms: T0,
            utc_offset_minutes: 600,
            duration_ms: Some(60_000),
            tags: Vec::new(),
            template: Template::General,
            folder_id: None,
            summary_kept_local: false,
        };
        self.store.put_meeting(m).unwrap();
        for (n, kind) in [(MIC, TrackId::Mic), (REFERENCE, TrackId::Reference)] {
            let at = self.now();
            self.store
                .put_track(Track {
                    id: id(n),
                    meta: meta_at(at, origin),
                    meeting_id: id(MEETING),
                    kind,
                    device_name: None,
                    clock_domain: ClockDomain::Replay,
                })
                .unwrap();
        }
        let at = self.now();
        self.store
            .put_speaker(Speaker {
                id: id(SPEAKER),
                meta: meta_at(at, origin),
                meeting_id: id(MEETING),
                role: SpeakerRole::Me,
                display_name: "Me".into(),
                person_id: None,
                ignored: false,
            })
            .unwrap();
    }

    fn row(&mut self, n: u8, track: u8, pass: TranscriptPass, list: &[&str]) -> SegmentRow {
        let at = self.now();
        let mut s = Segment::from_words(
            id(n),
            id(track),
            words(list, u64::from(n) * 2_000),
            MeetingTime::ZERO,
        );
        if pass == TranscriptPass::Final {
            s.speaker_id = Some(id(SPEAKER));
        }
        SegmentRow {
            meta: meta_at(at, self.me),
            meeting_id: id(MEETING),
            pass,
            segment: s,
            text_edited_at: None,
            pass_run: None,
        }
    }

    fn put(&mut self, row: SegmentRow) {
        self.store.put_segment(row).unwrap();
    }

    fn tomb(&mut self, mut row: SegmentRow) -> SegmentRow {
        row.meta.deleted = true;
        row.meta.updated_at = self.now();
        row
    }

    fn get(&self, n: u8) -> SegmentRow {
        self.store
            .segments(id(MEETING))
            .unwrap()
            .into_iter()
            .find(|r| r.segment.id == id(n))
            .unwrap()
    }

    fn clear(&mut self, limit: usize) -> earshot_store::Cleared {
        self.store
            .clear_replaced_text(id(MEETING), limit, &mut self.hlc, &self.clock)
            .unwrap()
    }

    fn queued(&self, n: u8) -> bool {
        self.store
            .outbox(100_000)
            .unwrap()
            .iter()
            .any(|r| r.id() == id(n))
    }

    /// Marks everything queued as sent (a push that reached the relay).
    fn push_all(&mut self) {
        let rows = self.store.outbox(100_000).unwrap();
        let marks: Vec<(&str, EntityId, HlcTimestamp)> = rows
            .iter()
            .map(|r| (r.table(), r.id(), r.meta().updated_at))
            .collect();
        self.store.outbox_sent(&marks).unwrap();
    }

    /// Row `n`'s `updated_at` (wall, logical), read in SQL, so a damaged row elsewhere in the
    /// meeting does not stop the read.
    fn stamp(&self, n: u8) -> (i64, i64) {
        let c = rusqlite::Connection::open(self._dir.path().join("earshot.db")).unwrap();
        c.query_row(
            "SELECT updated_wall, updated_logical FROM segment WHERE id = ?1",
            [&id(n).as_uuid().as_bytes()[..]],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap()
    }

    fn exec(&self, sql: &str) {
        let c = rusqlite::Connection::open(self._dir.path().join("earshot.db")).unwrap();
        c.execute_batch(sql).unwrap();
    }

    fn apply_page(&mut self, rows: &[SyncedRow]) -> Applied {
        let page = self.page(rows);
        let rules = ApplyRules {
            kinds: SyncKinds::ALL,
            leased: &|_| false,
            now_ms: self.clock.now_unix_ms(),
            after: None,
        };
        self.store
            .apply_remote(&page, &rules, &mut self.hlc, &self.clock)
            .unwrap()
    }

    fn page(&mut self, rows: &[SyncedRow]) -> Vec<Incoming> {
        rows.iter()
            .map(|row| {
                self.seq += 1;
                incoming(self.seq, row)
            })
            .collect()
    }

    fn release(
        &mut self,
        rows: &[SyncedRow],
        blocked: &dyn Fn(EntityId, EntityId) -> bool,
    ) -> Applied {
        let page = self.page(rows);
        let rules = ApplyRules {
            kinds: SyncKinds::ALL,
            leased: &|_| false,
            now_ms: self.clock.now_unix_ms(),
            after: None,
        };
        self.store
            .apply_remote_releasing(&page, &rules, blocked, &mut self.hlc, &self.clock)
            .unwrap()
    }

    fn held(&self) -> u64 {
        self.store.held_count(&[HoldReason::Parent]).unwrap()
    }
}

fn incoming(seq: u64, row: &SyncedRow) -> Incoming {
    let mut key = [0u8; 16];
    key.copy_from_slice(&row.id().as_uuid().as_bytes()[..]);
    key[0] ^= 0xFF;
    Incoming {
        key,
        seq,
        stamp: row.meta().updated_at,
        body: vec![0xA5; 40],
        entity: Some(row.table().into()),
        row: Some(row.clone()),
    }
}

/// The meeting row as another computer would send it (the base rows a receiver holds).
fn base_rows(origin: DeviceId) -> Vec<SyncedRow> {
    let at = ts(0, origin);
    let m = Meeting {
        id: id(MEETING),
        meta: meta_at(at, origin),
        title: "Invented planning".into(),
        started_unix_ms: T0,
        utc_offset_minutes: 600,
        duration_ms: Some(60_000),
        tags: Vec::new(),
        template: Template::General,
        folder_id: None,
        summary_kept_local: false,
    };
    let track = |n: u8, kind| Track {
        id: id(n),
        meta: meta_at(at, origin),
        meeting_id: id(MEETING),
        kind,
        device_name: None,
        clock_domain: ClockDomain::Replay,
    };
    vec![
        SyncedRow::Meeting(m),
        SyncedRow::Track(track(MIC, TrackId::Mic)),
        SyncedRow::Track(track(REFERENCE, TrackId::Reference)),
        SyncedRow::Speaker(Speaker {
            id: id(SPEAKER),
            meta: meta_at(at, origin),
            meeting_id: id(MEETING),
            role: SpeakerRole::Me,
            display_name: "Me".into(),
            person_id: None,
            ignored: false,
        }),
    ]
}

/// A segment row of `origin`'s at `dt`, as a peer would send it.
fn peer_row(n: u8, track: u8, pass: TranscriptPass, list: &[&str], dt: u64) -> SegmentRow {
    let mut s = Segment::from_words(
        id(n),
        id(track),
        words(list, u64::from(n) * 2_000),
        MeetingTime::ZERO,
    );
    if pass == TranscriptPass::Final {
        s.speaker_id = Some(id(SPEAKER));
    }
    SegmentRow {
        meta: meta_at(ts(dt, PEER), PEER),
        meeting_id: id(MEETING),
        pass,
        segment: s,
        text_edited_at: None,
        pass_run: None,
    }
}

fn tomb_at(row: &SegmentRow, dt: u64) -> SegmentRow {
    let mut t = row.clone();
    t.meta.deleted = true;
    t.meta.updated_at = ts(dt, PEER);
    t
}

fn cleared_at(row: &SegmentRow, dt: u64) -> SegmentRow {
    let mut t = row.cleared();
    t.meta.updated_at = ts(dt, PEER);
    t
}

fn seg(row: SegmentRow) -> SyncedRow {
    SyncedRow::Segment(row)
}

// ---- 15.1 and 15.2: the cleared form and the predicate ----

/// P1, P2 per track, a track without final rows, a track whose final rows are all tombstones,
/// and the rows naming another meeting's track or no track, on the meeting's owner.
#[test]
fn sync_replaced_predicate_on_the_owner_case_by_case() {
    let mut e = env("predicate-owner");
    let me = e.me;
    e.meeting(me);
    // Mic: a draft and a live final line: the draft is replaced (P2).
    let draft_mic = e.row(10, MIC, TranscriptPass::Live, &["shall", "we"]);
    e.put(draft_mic);
    let final_mic = e.row(11, MIC, TranscriptPass::Final, &["shall", "we", "sail"]);
    e.put(final_mic.clone());
    // A final tombstone that kept its text (P1), with a speaker.
    let old = e.row(12, MIC, TranscriptPass::Final, &["older", "words"]);
    let old = e.tomb(old);
    e.put(old);
    // Reference: a draft and only a tombstoned final line: the draft stays, the tombstone is
    // cleared (P1).
    let draft_ref = e.row(20, REFERENCE, TranscriptPass::Live, &["far", "end"]);
    e.put(draft_ref);
    let gone = e.row(21, REFERENCE, TranscriptPass::Final, &["far", "gone"]);
    let gone = e.tomb(gone);
    e.put(gone);
    // Another meeting with a track and a live final line on it.
    let at = e.now();
    let mut other = e.store.meeting(id(MEETING)).unwrap().unwrap();
    other.id = id(OTHER_MEETING);
    other.meta = meta_at(at, me);
    e.store.put_meeting(other).unwrap();
    let at = e.now();
    e.store
        .put_track(Track {
            id: id(OTHER_TRACK),
            meta: meta_at(at, me),
            meeting_id: id(OTHER_MEETING),
            kind: TrackId::Mic,
            device_name: None,
            clock_domain: ClockDomain::Replay,
        })
        .unwrap();
    // A draft of this meeting naming the other meeting's track (which has a live final line in
    // its own meeting), and one naming no track row at all: neither is ever replaced.
    let cols = "id, created_wall, created_logical, created_node, updated_wall, updated_logical, \
                updated_node, deleted, origin_device, meeting_id, track_id, pass, start_ms, \
                end_ms, text, words, confidence";
    let copy = |new: u8, meeting: u8, track: u8, pass: &str, from: u8| {
        format!(
            "INSERT INTO segment ({cols}) SELECT x'{new}', created_wall, created_logical, \
             created_node, updated_wall, updated_logical, updated_node, 0, origin_device, \
             x'{meeting}', x'{track}', '{pass}', start_ms, end_ms, text, words, confidence \
             FROM segment WHERE id = x'{from}';",
            new = hex(id(new)),
            meeting = hex(id(meeting)),
            track = hex(id(track)),
            from = hex(id(from)),
        )
    };
    e.exec(&format!(
        "PRAGMA foreign_keys = OFF; {} {} {}",
        copy(30, MEETING, OTHER_TRACK, "live", 10),
        copy(31, MEETING, 99, "live", 10),
        copy(32, OTHER_MEETING, OTHER_TRACK, "final", 11),
    ));
    assert!(e.store.owns_meeting(id(MEETING)).unwrap());
    let done = e.clear(1_000);
    assert_eq!(done.rows, 3, "{done:?}");
    assert!(!done.more);
    // Replaced: the mic draft and both text-bearing tombstones.
    for n in [10, 12, 21] {
        let r = e.get(n);
        assert!(r.is_cleared(), "{n}: {r:?}");
        assert!(r.segment.speaker_id.is_none(), "{n}");
        assert!(e.queued(n), "{n} goes out");
    }
    // Kept: the live final line, the reference draft (no live final line on its track), and the
    // drafts naming another meeting's track or none.
    assert_eq!(e.get(11), final_mic);
    assert!(e.get(20).holds_text() && !e.get(20).meta.deleted);
    for n in [30, 31] {
        let r = e.get(n);
        assert!(r.holds_text() && !r.meta.deleted, "{n}: {r:?}");
    }
    // Idempotent.
    assert_eq!(e.clear(1_000).rows, 0);
}

/// P2 never matches on a device that is not the meeting's owner, also with audio rows copied
/// in by hand; P1 still does.
#[test]
fn sync_replaced_a_non_owner_never_clears_a_draft() {
    let mut e = env("predicate-non-owner");
    e.meeting(PEER);
    let draft = e.row(10, MIC, TranscriptPass::Live, &["shall", "we"]);
    e.put(draft.clone());
    let line = e.row(11, MIC, TranscriptPass::Final, &["shall", "we", "sail"]);
    e.put(line);
    let old = e.row(12, MIC, TranscriptPass::Final, &["older"]);
    let old = e.tomb(old);
    e.put(old);
    e.store
        .put_audio_file(AudioFile {
            id: id(40),
            meeting_id: id(MEETING),
            track_id: id(MIC),
            path: format!("audio/{}/mic.flac", id(MEETING)),
            format: AudioFormat::Flac,
            sample_rate_hz: 16_000,
            frames: 16_000,
            size_bytes: 1_000,
            pipeline_version: "test".into(),
            aec_version: None,
            created_unix_ms: T0,
        })
        .unwrap();
    assert!(!e.store.owns_meeting(id(MEETING)).unwrap());
    assert_eq!(e.clear(1_000).rows, 1);
    assert!(e.get(12).is_cleared());
    assert_eq!(e.get(10), draft, "the draft is the owner's to clear");
}

/// In a meeting that reads deleted here every row that holds text is written in the erasure
/// form; the limit stops a call and the next goes on.
#[test]
fn sync_replaced_deleted_meeting_gets_the_erasure_form_and_the_limit_holds() {
    let mut e = env("deleted-limit");
    let me = e.me;
    e.meeting(me);
    for n in 10..15 {
        let r = e.row(n, MIC, TranscriptPass::Final, &["one", "two"]);
        let r = e.tomb(r);
        e.put(r);
    }
    let first = e.clear(2);
    assert_eq!((first.rows, first.more), (2, true));
    let second = e.clear(2);
    assert_eq!((second.rows, second.more), (2, true));
    let third = e.clear(2);
    assert_eq!((third.rows, third.more), (1, false));
    assert_eq!(e.clear(2).rows, 0);
    for n in 10..15 {
        assert!(e.get(n).is_cleared());
    }
    // A meeting deleted here: the erasure form, to rows that hold text. (The cleared rows are
    // pushed first: a line written alone, without the check, must not follow queued ones.)
    e.push_all();
    let live = e.row(16, MIC, TranscriptPass::Final, &["still", "here"]);
    e.put(live);
    e.exec(&format!(
        "UPDATE meeting SET deleted = 1 WHERE id = x'{}'",
        hex(id(MEETING))
    ));
    assert_eq!(e.clear(100).rows, 1);
    let erased = e.get(16);
    assert!(erased.is_erasure() && !erased.is_cleared(), "{erased:?}");
    // Rows already cleared hold no text and are left alone.
    assert!(e.get(10).is_cleared());
}

/// The SQL test of "holds text" and `SegmentRow::holds_text` agree, and 1.0 and 0.0 come back
/// exactly through SQLite.
#[test]
fn sync_replaced_sql_and_holds_text_agree_and_confidences_are_exact() {
    let mut e = env("sql-agrees");
    let me = e.me;
    e.meeting(me);
    let cases: [(u8, &str, bool); 4] = [
        (10, "text and words", true),
        (11, "text only", true),
        (12, "words only", true),
        (13, "neither", false),
    ];
    for (n, what, _) in cases {
        let mut r = e.row(n, MIC, TranscriptPass::Final, &["a", "b"]);
        match what {
            "text only" => r.segment.words.clear(),
            "words only" => r.segment.text.clear(),
            "neither" => {
                r.segment.text.clear();
                r.segment.words.clear();
            }
            _ => {}
        }
        r.segment.confidence = if n == 13 { 0.0 } else { 1.0 };
        e.put(r);
    }
    for (n, what, holds) in cases {
        let row = e.get(n);
        assert_eq!(row.holds_text(), holds, "{what}");
        assert_eq!(
            e.store.segment_holds_text_in_sql(id(n)).unwrap(),
            Some(holds),
            "{what}"
        );
        let want = if n == 13 { 0.0_f32 } else { 1.0 };
        assert_eq!(row.segment.confidence.to_bits(), want.to_bits(), "{what}");
    }
}

/// A row whose stored values do not convert is cleared, then reads and goes out.
#[test]
fn sync_replaced_a_damaged_row_is_cleared_and_reads_afterwards() {
    let mut e = env("damaged");
    let me = e.me;
    e.meeting(me);
    let r = e.row(10, MIC, TranscriptPass::Final, &["broken", "row"]);
    let r = e.tomb(r);
    e.put(r);
    e.exec(&format!(
        "UPDATE segment SET created_logical = -1 WHERE id = x'{}'",
        hex(id(10))
    ));
    assert!(
        e.store
            .outbox_page(100, &[], SyncKinds::ALL)
            .unwrap()
            .unreadable
            > 0
    );
    assert_eq!(e.clear(100).rows, 1);
    let row = e.get(10);
    assert!(row.is_cleared(), "{row:?}");
    let page = e.store.outbox_page(100, &[], SyncKinds::ALL).unwrap();
    assert_eq!(page.unreadable, 0);
    assert!(page.rows.iter().any(|r| r.id() == id(10)));
}

// ---- 15.2: lines go out before what they replace ----

/// A pass's transaction as the owner writes it: four final lines, then the drafts it replaced
/// cleared (stamped after the lines).
fn after_a_pass(e: &mut Env) -> Vec<SegmentRow> {
    let me = e.me;
    e.meeting(me);
    for n in 10..14 {
        let d = e.row(n, MIC, TranscriptPass::Live, &["draft", "words"]);
        e.put(d);
    }
    let lines: Vec<SegmentRow> = (20..24)
        .map(|n| e.row(n, MIC, TranscriptPass::Final, &["final", "words"]))
        .collect();
    let (store, hlc, clock) = (&mut e.store, &mut e.hlc, &e.clock);
    store
        .transaction(|tx| -> Result<(), StoreError> {
            for l in &lines {
                tx.put_segment(l.clone()).map_err(StoreError::Repo)?;
            }
            tx.clear_replaced_text(id(MEETING), None, hlc, clock)?;
            tx.send_lines_first(hlc, clock)?;
            Ok(())
        })
        .unwrap();
    for n in 10..14 {
        assert!(e.get(n).is_cleared());
    }
    assert!(e.store.lines_go_first(id(MEETING), id(MIC)).unwrap());
    lines
}

fn cleared_stamps(e: &Env) -> Vec<(i64, i64)> {
    (10..14).map(|n| e.stamp(n)).collect()
}

fn rewrite(e: &mut Env, n: u8, text: &str) {
    let r = e.get(n);
    rewrite_row(e, r, text);
}

fn rewrite_row(e: &mut Env, mut r: SegmentRow, text: &str) {
    r.segment.text = text.into();
    r.segment.words.clear();
    let at = e.now();
    r.meta.updated_at = at;
    r.text_edited_at = Some(at);
    let (store, hlc, clock) = (&mut e.store, &mut e.hlc, &e.clock);
    store
        .transaction(|tx| -> Result<(), StoreError> {
            tx.put_segment(r).map_err(StoreError::Repo)?;
            tx.send_lines_first(hlc, clock)?;
            Ok(())
        })
        .unwrap();
}

/// Rewriting every line of the track after its pass leaves the invariant holding at commit: the
/// cleared rows are stamped again above the lines. Rewriting all but one stamps nothing again.
#[test]
fn sync_replaced_rewriting_every_line_stamps_the_cleared_rows_again() {
    let mut e = env("lines-first-all");
    let _ = after_a_pass(&mut e);
    let before = cleared_stamps(&e);
    for n in 20..23 {
        rewrite(&mut e, n, "edited line");
    }
    assert_eq!(cleared_stamps(&e), before, "one line still goes first");
    rewrite(&mut e, 23, "edited line");
    let after = cleared_stamps(&e);
    let top_line = (20..24).map(|n| e.stamp(n)).max().unwrap();
    assert!(
        after.iter().all(|s| *s > top_line),
        "{after:?} above {top_line:?}"
    );
    assert!(e.store.lines_go_first(id(MEETING), id(MIC)).unwrap());
    // Content unchanged.
    for n in 10..14 {
        assert!(e.get(n).is_cleared());
    }
    // A hundred more writes on one line stamp nothing again: the other lines go first.
    let again = cleared_stamps(&e);
    for _ in 0..100 {
        rewrite(&mut e, 20, "edited once more");
    }
    assert_eq!(cleared_stamps(&e), again);
}

/// A hundred writes on one line of a track whose other lines are untouched stamp the cleared
/// rows again at most once (here, never).
#[test]
fn sync_replaced_a_hundred_writes_on_one_line_stamp_at_most_once() {
    let mut e = env("lines-first-hundred");
    let _ = after_a_pass(&mut e);
    let before = cleared_stamps(&e);
    for _ in 0..100 {
        rewrite(&mut e, 21, "a chosen word");
    }
    assert_eq!(cleared_stamps(&e), before);
}

/// The store has no stamp source of its own: a writer that commits a live final line without
/// `send_lines_first`, leaving the track's cleared rows to go out first, is a programming error
/// (a debug assertion). Its control: the same writes through `send_lines_first` commit.
#[test]
#[cfg(debug_assertions)]
#[should_panic(expected = "without `send_lines_first`")]
fn sync_replaced_a_commit_without_the_check_is_a_debug_assertion() {
    let mut e = env("lines-first-commit");
    let _ = after_a_pass(&mut e);
    for n in 20..23 {
        rewrite(&mut e, n, "edited line");
    }
    let mut r = e.get(23);
    r.segment.text = "edited line".into();
    r.segment.words.clear();
    r.meta.updated_at = e.now();
    e.store
        .transaction(|tx| tx.put_segment(r).map_err(StoreError::Repo))
        .unwrap();
}

/// Re-queueing (an epoch reset or a join) after such edits leaves it holding.
#[test]
fn sync_replaced_requeueing_keeps_the_invariant() {
    for join in [false, true] {
        let mut e = env("lines-first-requeue");
        let _ = after_a_pass(&mut e);
        e.push_all();
        // Edits after the push: the lines are queued again, stamped after the pushed cleared
        // rows (which are not queued).
        for n in 20..24 {
            rewrite(&mut e, n, "edited line");
        }
        assert!(!e.queued(10));
        let (store, hlc, clock) = (&mut e.store, &mut e.hlc, &e.clock);
        if join {
            store.queue_everything(hlc, clock).unwrap();
        } else {
            store
                .transaction(|tx| -> Result<(), StoreError> {
                    tx.queue_every_row()?;
                    tx.send_lines_first(hlc, clock)?;
                    Ok(())
                })
                .unwrap();
        }
        assert!(e.queued(10) && e.queued(20));
        assert!(
            e.store.lines_go_first(id(MEETING), id(MIC)).unwrap(),
            "join={join}"
        );
        let lines = (20..24).map(|n| e.stamp(n)).max().unwrap();
        assert!(cleared_stamps(&e).iter().all(|s| *s > lines), "join={join}");
    }
}

/// A queued line that does not convert never counts as going first: beside one sendable line
/// stamped above the cleared rows, those are stamped again above it. Its control: the same line
/// readable stamps nothing.
#[test]
fn sync_replaced_an_unreadable_queued_line_never_counts_as_going_first() {
    for damaged in [true, false] {
        let mut e = env("lines-first-unreadable");
        let _ = after_a_pass(&mut e);
        // Every line but 20 is rewritten above the cleared rows: 20 is the one queued line
        // stamped below them.
        for n in 21..24 {
            rewrite(&mut e, n, "edited line");
        }
        let row = e.get(21);
        let before = cleared_stamps(&e);
        if damaged {
            e.exec(&format!(
                "UPDATE segment SET created_logical = -1 WHERE id = x'{}'",
                hex(id(20))
            ));
        }
        // Any write of a line of the track runs the check.
        rewrite_row(&mut e, row, "edited again");
        let after = cleared_stamps(&e);
        if damaged {
            assert_ne!(after, before, "the damaged line does not count");
            let top = (21..24).map(|n| e.stamp(n)).max().unwrap();
            assert!(after.iter().all(|s| *s > top));
        } else {
            assert_eq!(after, before, "the readable line goes first");
        }
    }
}

/// The same for a queued line the push's guard refuses.
#[test]
fn sync_replaced_a_refused_queued_line_never_counts_as_going_first() {
    for refused in [true, false] {
        let mut e = env("lines-first-refused");
        let _ = after_a_pass(&mut e);
        for n in 21..24 {
            rewrite(&mut e, n, "edited line");
        }
        // Line 20 is now the one queued line below the cleared rows; the guard refuses it.
        let refuse = id(20);
        e.store
            .set_push_guard(PushGuard::new(move |row| !(refused && row.id() == refuse)));
        let before = cleared_stamps(&e);
        rewrite(&mut e, 21, "edited again");
        let after = cleared_stamps(&e);
        assert_eq!(after != before, refused, "refused={refused}");
    }
}

/// A re-stamp while a push page holding the cleared rows is on the way: the page's answer does
/// not take the re-stamped rows out of the outbox (their stamp changed), so they go again; rows
/// already pushed are never touched; only the written track is checked.
#[test]
fn sync_replaced_a_restamp_during_a_push_keeps_the_rows_queued() {
    let mut e = env("lines-first-in-flight");
    let _ = after_a_pass(&mut e);
    // A reference draft and line, cleared and pushed: never touched again.
    let d = e.row(30, REFERENCE, TranscriptPass::Live, &["far"]);
    e.put(d);
    let l = e.row(31, REFERENCE, TranscriptPass::Final, &["far", "end"]);
    e.put(l);
    e.clear(100);
    let reference_cleared = e.get(30).meta.updated_at;
    // The page is read (sealed) now.
    let page = e.store.outbox(100_000).unwrap();
    let marks: Vec<(&str, EntityId, HlcTimestamp)> = page
        .iter()
        .map(|r| (r.table(), r.id(), r.meta().updated_at))
        .collect();
    // Meanwhile every mic line is rewritten: the cleared mic rows are stamped again.
    for n in 20..24 {
        rewrite(&mut e, n, "edited line");
    }
    // The page's answer comes back.
    e.store.outbox_sent(&marks).unwrap();
    for n in 10..14 {
        assert!(e.queued(n), "{n} goes again above the lines");
    }
    for n in 20..24 {
        assert!(e.queued(n));
    }
    assert!(!e.queued(30));
    assert_eq!(e.get(30).meta.updated_at, reference_cleared);
}

/// A pulled line written without queuing it stamps nothing again.
#[test]
fn sync_replaced_a_pulled_line_stamps_nothing_again() {
    let mut e = env("lines-first-pulled");
    let lines = after_a_pass(&mut e);
    let before = cleared_stamps(&e);
    let mut pulled = lines[0].clone();
    pulled.segment.text = "pulled text".into();
    pulled.meta.updated_at = ts(20_000, PEER);
    let a = e.apply_page(&[seg(pulled)]);
    assert_eq!(a.written, 1);
    assert_eq!(cleared_stamps(&e), before);
}

// ---- 15.4: the apply ----

/// A pulled tombstone that holds text is written cleared here, stamped after it, so it goes out;
/// also when it is stamped ahead of this device's clock. `text_emptied` counts it.
#[test]
fn sync_replaced_a_pulled_text_tombstone_is_cleared_and_goes_out() {
    let mut e = env("apply-clear");
    e.apply_page(&base_rows(PEER));
    let a_line = peer_row(10, MIC, TranscriptPass::Final, &["kept", "line"], 10);
    let b_line = peer_row(11, MIC, TranscriptPass::Final, &["gone", "line"], 11);
    e.apply_page(&[seg(a_line), seg(b_line.clone())]);
    // Stamped an hour ahead of this device's clock.
    let ahead = tomb_at(&b_line, 3_600_000 + 20_000);
    let a = e.apply_page(&[seg(ahead.clone())]);
    assert_eq!(a.text_emptied, 1, "{a:?}");
    let here = e.get(11);
    assert!(here.is_cleared(), "{here:?}");
    assert!(here.meta.updated_at > ahead.meta.updated_at);
    assert!(e.queued(11));
    // A second apply of the same page writes nothing.
    let again = e.apply_page(&[seg(ahead)]);
    assert_eq!((again.written, again.text_emptied), (0, 0), "{again:?}");
}

/// `text_emptied` counts a pulled cleared copy over a copy that held text, and does not count
/// a pulled edit.
#[test]
fn sync_replaced_text_emptied_counts_rows_never_text_changes() {
    let mut e = env("apply-count");
    e.apply_page(&base_rows(PEER));
    let a = peer_row(10, MIC, TranscriptPass::Final, &["one", "line"], 10);
    let b = peer_row(11, MIC, TranscriptPass::Final, &["two", "line"], 11);
    e.apply_page(&[seg(a.clone()), seg(b.clone())]);
    let mut edit = a;
    edit.segment.text = "one changed".into();
    edit.segment.words.clear();
    edit.meta.updated_at = ts(30, PEER);
    edit.text_edited_at = Some(ts(30, PEER));
    let edited = e.apply_page(&[seg(edit)]);
    assert_eq!((edited.written, edited.text_emptied), (1, 0));
    let cleared = e.apply_page(&[seg(cleared_at(&b, 40))]);
    assert_eq!(
        (cleared.written, cleared.text_emptied),
        (1, 1),
        "{cleared:?}"
    );
    assert!(!e.queued(11), "a cleared copy is applied as it arrives");
}

/// H1: a cleared draft over a live draft of a track with no live final line here is held until
/// the final lines apply, which count as a parent for the end-of-tick retry.
#[test]
fn sync_replaced_h1_holds_a_cleared_draft_until_its_lines_apply() {
    let mut e = env("apply-h1");
    e.apply_page(&base_rows(PEER));
    let draft = peer_row(10, MIC, TranscriptPass::Live, &["draft", "words"], 10);
    e.apply_page(&[seg(draft.clone())]);
    let a = e.apply_page(&[seg(cleared_at(&draft, 50))]);
    assert_eq!(
        a.held[HoldReason::ALL
            .iter()
            .position(|r| *r == HoldReason::Parent)
            .unwrap()],
        1
    );
    assert!(
        e.get(10).holds_text(),
        "the draft is shown until the lines arrive"
    );
    let line = peer_row(11, MIC, TranscriptPass::Final, &["final", "words"], 40);
    let lines = e.apply_page(&[seg(line)]);
    assert_eq!(lines.parents_written, 1, "{lines:?}");
    // The retry applies the held row.
    let retried = e.apply_page(&[seg(cleared_at(&draft, 50))]);
    assert_eq!(retried.written, 1);
    assert!(e.get(10).is_cleared());
    assert_eq!(e.held(), 0);
    assert!(!e.queued(10));
}

/// H2: a final line taken out over the last live final line of its track here is held; a
/// pulled tombstone that holds text is held first and cleared when applied. The release applies
/// it unless its track is blocked.
#[test]
fn sync_replaced_h2_holds_the_last_line_and_the_release_lets_it_go() {
    let mut e = env("apply-h2");
    e.apply_page(&base_rows(PEER));
    let line = peer_row(10, MIC, TranscriptPass::Final, &["only", "line"], 10);
    e.apply_page(&[seg(line.clone())]);
    let tomb = tomb_at(&line, 50);
    let a = e.apply_page(&[seg(tomb.clone())]);
    assert_eq!(a.written, 0, "{a:?}");
    assert_eq!(e.held(), 1);
    assert!(!e.get(10).meta.deleted);
    // Blocked: a held row could still give the track a line.
    let blocked = e.release(&[seg(tomb.clone())], &|_, _| true);
    assert_eq!(blocked.written, 0);
    assert_eq!(e.held(), 1);
    // Released.
    let released = e.release(&[seg(tomb)], &|_, _| false);
    assert_eq!(
        (released.written, released.text_emptied),
        (1, 1),
        "{released:?}"
    );
    assert!(e.get(10).is_cleared());
    assert_eq!(e.held(), 0);
}

/// The store keeps which `parent` rows are H1's and H2's, so the release retries only those: a
/// segment tombstone held because its speaker is missing is not among them; an applied row
/// leaves the list.
#[test]
fn sync_replaced_only_the_two_holds_rows_are_the_releases() {
    let mut e = env("apply-theirs");
    e.apply_page(&base_rows(PEER));
    let line = peer_row(10, MIC, TranscriptPass::Final, &["only", "line"], 10);
    e.apply_page(&[seg(line.clone())]);
    let tomb = tomb_at(&line, 50);
    e.apply_page(&[seg(tomb.clone())]);
    // A tombstone naming a speaker that is not here: held for its parent, not by H1 or H2.
    let mut orphan = peer_row(11, MIC, TranscriptPass::Final, &["orphan"], 20);
    orphan.segment.speaker_id = Some(id(77));
    let orphan = tomb_at(&orphan, 51);
    e.apply_page(&[seg(orphan.clone())]);
    assert_eq!(e.held(), 2);
    let theirs = e.store.held_by_replaced_text().unwrap();
    assert_eq!(theirs.len(), 1);
    assert!(theirs.contains(&incoming(0, &seg(tomb.clone())).key));
    assert!(!theirs.contains(&incoming(0, &seg(orphan)).key));
    // Released: it leaves the list.
    e.release(&[seg(tomb)], &|_, _| false);
    assert!(e.store.held_by_replaced_text().unwrap().is_empty());
    assert_eq!(e.held(), 1, "the orphan still waits for its speaker");
}

/// A tombstone over a local row that does not read is applied without a hold.
#[test]
fn sync_replaced_a_tombstone_over_an_unreadable_row_is_not_held() {
    let mut e = env("apply-unreadable");
    e.apply_page(&base_rows(PEER));
    let line = peer_row(10, MIC, TranscriptPass::Final, &["only", "line"], 10);
    e.apply_page(&[seg(line.clone())]);
    e.exec(&format!(
        "UPDATE segment SET created_logical = -1 WHERE id = x'{}'",
        hex(id(10))
    ));
    let a = e.apply_page(&[seg(tomb_at(&line, 50))]);
    assert_eq!(a.written, 1, "{a:?}");
    assert_eq!(e.held(), 0);
    assert!(e.get(10).is_cleared());
}

/// In a meeting deleted here a pulled text-bearing tombstone is written as an erasure and goes
/// out once; a pulled cleared row and a pulled erasure are applied with their own stamps and
/// nothing is queued for them; two devices that both hold the deletion and exchange these send
/// nothing back after the first exchange.
#[test]
fn sync_replaced_a_deleted_meeting_erases_and_sends_nothing_back() {
    let mut a = env("deleted-a");
    let mut b = env("deleted-b");
    for e in [&mut a, &mut b] {
        e.apply_page(&base_rows(PEER));
        let rows: Vec<SyncedRow> = (10..13)
            .map(|n| {
                seg(peer_row(
                    n,
                    MIC,
                    TranscriptPass::Final,
                    &["deleted", "soon"],
                    10,
                ))
            })
            .collect();
        e.apply_page(&rows);
        e.exec(&format!(
            "UPDATE meeting SET deleted = 1 WHERE id = x'{}'",
            hex(id(MEETING))
        ));
        e.push_all();
    }
    let line = peer_row(10, MIC, TranscriptPass::Final, &["deleted", "soon"], 10);
    // A text-bearing tombstone: erased here, queued once.
    let got = a.apply_page(&[seg(tomb_at(&line, 60))]);
    assert_eq!(got.text_emptied, 1);
    let erased = a.get(10);
    assert!(erased.is_erasure(), "{erased:?}");
    assert!(a.queued(10));
    // A cleared copy and an erasure: applied with their own stamps, nothing queued.
    let other = peer_row(11, MIC, TranscriptPass::Final, &["deleted", "soon"], 10);
    let cleared = cleared_at(&other, 70);
    a.apply_page(&[seg(cleared.clone())]);
    assert_eq!(a.get(11).meta.updated_at, cleared.meta.updated_at);
    assert!(!a.queued(11));
    // A's erasure reaches B; B's answer sends nothing back.
    let a_out = a.store.outbox(1000).unwrap();
    a.push_all();
    let got_b = b.apply_page(&a_out);
    assert!(got_b.written > 0);
    let b_out: Vec<SyncedRow> = b
        .store
        .outbox(1000)
        .unwrap()
        .into_iter()
        .filter(|r| matches!(r, SyncedRow::Segment(_)))
        .collect();
    assert!(b_out.is_empty(), "{b_out:?}");
    // And A, applying B's copy of the cleared row, sends nothing either.
    b.apply_page(&[seg(cleared)]);
    let back = b.store.outbox(1000).unwrap();
    a.apply_page(&back);
    assert!(
        a.store
            .outbox(1000)
            .unwrap()
            .iter()
            .all(|r| !matches!(r, SyncedRow::Segment(_))),
    );
}

/// The mutation control of 15.9: a "cleared" form written with confidence 0 would be an
/// erasure, ordered by plain stamps, so it would beat an edit made elsewhere; the cleared form
/// (confidence 1.0) loses to it.
#[test]
fn sync_replaced_an_edit_beats_a_cleared_line_but_would_lose_to_an_erasure() {
    for erasure in [false, true] {
        let mut e = env("mutation-control");
        e.apply_page(&base_rows(PEER));
        let line = peer_row(10, MIC, TranscriptPass::Final, &["shall", "we"], 10);
        let other = peer_row(11, MIC, TranscriptPass::Final, &["keep", "me"], 10);
        e.apply_page(&[seg(line.clone()), seg(other)]);
        // An edit here.
        let mut mine = e.get(10);
        mine.segment.text = "shall we sail".into();
        mine.segment.words.clear();
        let at = e.now();
        mine.meta.updated_at = at;
        mine.text_edited_at = Some(at);
        e.store.put_segment(mine).unwrap();
        // The owner's later clearing of the line it replaced.
        let mut pulled = cleared_at(&line, 30_000);
        if erasure {
            pulled.segment.confidence = 0.0;
        }
        e.apply_page(&[seg(pulled)]);
        let kept = e.get(10);
        assert_eq!(
            kept.segment.text == "shall we sail",
            !erasure,
            "erasure={erasure}: {kept:?}"
        );
    }
}

/// A peer that never clears: each of its text-bearing tombstones is written here once, and its
/// re-sent copies change nothing (no loop).
#[test]
fn sync_replaced_a_peer_that_never_clears_is_written_once() {
    let mut e = env("never-clears");
    e.apply_page(&base_rows(PEER));
    let l = peer_row(10, MIC, TranscriptPass::Final, &["one"], 10);
    let k = peer_row(11, MIC, TranscriptPass::Final, &["two"], 10);
    e.apply_page(&[seg(l.clone()), seg(k)]);
    let t = tomb_at(&l, 40);
    let first = e.apply_page(&[seg(t.clone())]);
    assert_eq!(first.text_emptied, 1);
    let cleared_stamp = e.get(10).meta.updated_at;
    for _ in 0..3 {
        let again = e.apply_page(&[seg(t.clone())]);
        assert_eq!((again.written, again.text_emptied), (0, 0));
    }
    assert_eq!(e.get(10).meta.updated_at, cleared_stamp);
}

/// A damaged pulled tombstone that holds text: cleared, then made canonical, and the page goes
/// on.
#[test]
fn sync_replaced_a_damaged_pulled_tombstone_is_cleared_and_the_page_goes_on() {
    let mut e = env("damaged-pulled");
    e.apply_page(&base_rows(PEER));
    let l = peer_row(10, MIC, TranscriptPass::Final, &["one"], 10);
    let k = peer_row(11, MIC, TranscriptPass::Final, &["two"], 10);
    e.apply_page(&[seg(l.clone()), seg(k.clone())]);
    // Whatever writes the pulled row leaves a value that does not convert.
    e.exec(&format!(
        "CREATE TRIGGER damage AFTER UPDATE OF text ON segment WHEN NEW.id = x'{}' \
         AND NEW.text <> '' BEGIN UPDATE segment SET created_logical = -1 WHERE rowid = NEW.rowid; END;",
        hex(id(10))
    ));
    let mut later = peer_row(12, MIC, TranscriptPass::Final, &["three"], 45);
    later.meta.updated_at = ts(45, PEER);
    let a = e.apply_page(&[seg(tomb_at(&l, 40)), seg(later)]);
    assert_eq!(a.written, 2, "{a:?}");
    let row = e.get(10);
    assert!(row.is_cleared(), "{row:?}");
    assert!(!e.get(12).meta.deleted);
}

/// Three devices that apply two pages in every order end with the same content (stamps aside:
/// each device stamps its own clearing).
#[test]
fn sync_replaced_receivers_in_every_order_of_two_pages_agree() {
    let line = peer_row(10, MIC, TranscriptPass::Final, &["one"], 10);
    let kept = peer_row(11, MIC, TranscriptPass::Final, &["two"], 10);
    let p1 = vec![seg(tomb_at(&line, 40))];
    let p2 = vec![seg(peer_row(
        12,
        MIC,
        TranscriptPass::Final,
        &["three"],
        41,
    ))];
    let mut contents = Vec::new();
    for (k, order) in [[&p1, &p2], [&p2, &p1]].iter().enumerate() {
        let mut e = env(&format!("three-{k}"));
        e.apply_page(&base_rows(PEER));
        e.apply_page(&[seg(line.clone()), seg(kept.clone())]);
        for p in order {
            e.apply_page(p);
        }
        let c: Vec<String> = e
            .store
            .segments(id(MEETING))
            .unwrap()
            .iter()
            .map(|r| {
                format!(
                    "{} {} {:?} {} {}",
                    r.segment.id,
                    r.meta.deleted,
                    r.segment.text,
                    r.segment.words.len(),
                    r.segment.confidence
                )
            })
            .collect();
        contents.push(c);
    }
    assert_eq!(contents[0], contents[1]);
}

fn hex(id: EntityId) -> String {
    id.as_uuid()
        .as_bytes()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}
