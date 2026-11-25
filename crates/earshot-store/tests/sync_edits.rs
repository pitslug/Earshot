//! T12.92: a user's line edit wins over a re-transcription made on another computer that did
//! not see it (docs/sync-spec.md section 7 rule 4), through straight `apply_remote` pages
//! (`cargo test -p earshot-store sync`). Every name and word is invented.

mod common;

use common::TempDir;
use earshot_core::asr::{MeetingTime, Segment, Word};
use earshot_core::capture::TrackId;
use earshot_core::diar::SpeakerRole;
use earshot_core::identity::{DeviceId, EntityId, HlcTimestamp, ManualClock, SyncMeta};
use earshot_core::model::{
    ClockDomain, Meeting, MeetingRepo, SegmentRow, Speaker, Track, TranscriptPass,
};
use earshot_core::prompt::Template;
use earshot_store::{Applied, ApplyRules, HoldReason, Incoming, Store, SyncKinds, SyncedRow};

const HERE: DeviceId = DeviceId::from_bytes([0x11; 16]);
/// The computer that edits.
const EDITOR: DeviceId = DeviceId::from_bytes([0x0A; 16]);
/// The computer that re-transcribes.
const PASSER: DeviceId = DeviceId::from_bytes([0x0B; 16]);
/// A third computer that edits too.
const OTHER: DeviceId = DeviceId::from_bytes([0x0C; 16]);

const T0: u64 = 1_800_000_000_000;

fn id(n: u8) -> EntityId {
    EntityId::from_bytes([0x01, 0x90, 0, 0, 0, 0, 0x70, 0, 0x80, 0, 0, 0, 0, 0, n, 1])
}

fn at(dt: u64, node: DeviceId) -> HlcTimestamp {
    HlcTimestamp {
        wall_ms: T0 + dt,
        logical: 0,
        node,
    }
}

fn meta(created: HlcTimestamp, updated: HlcTimestamp) -> SyncMeta {
    SyncMeta {
        created_at: created,
        updated_at: updated,
        deleted: false,
        origin_device_id: created.node,
    }
}

const MEETING: u8 = 1;
const MIC: u8 = 2;
const REFERENCE: u8 = 3;
/// The mic's speaker before the re-transcription.
const ME: u8 = 4;
/// The re-transcription's new mic speaker.
const NEW_SPEAKER: u8 = 6;
/// The edited line.
const LINE: u8 = 10;
/// The re-transcription's line over the same words.
const NEW_LINE: u8 = 11;
/// The re-transcription's run.
const RUN: u8 = 20;

fn base_meta() -> SyncMeta {
    meta(at(0, EDITOR), at(0, EDITOR))
}

fn speaker(n: u8, name: &str, m: SyncMeta) -> Speaker {
    Speaker {
        id: id(n),
        meta: m,
        meeting_id: id(MEETING),
        role: SpeakerRole::Me,
        display_name: name.into(),
        person_id: None,
        ignored: false,
    }
}

fn words(list: &[(&str, u64, u64)]) -> Vec<Word> {
    list.iter()
        .map(|(t, s, e)| Word::new(*t, *s, *e, 0.9))
        .collect()
}

const SHIP: &[(&str, u64, u64)] = &[
    ("shall", 1_000, 1_300),
    ("we", 1_400, 1_700),
    ("ship", 1_800, 2_100),
];

fn line(
    n: u8,
    track: u8,
    speaker: u8,
    list: &[(&str, u64, u64)],
    m: SyncMeta,
    run: Option<u8>,
) -> SegmentRow {
    let mut s = Segment::from_words(id(n), id(track), words(list), MeetingTime::ZERO);
    s.speaker_id = Some(id(speaker));
    SegmentRow {
        meta: m,
        meeting_id: id(MEETING),
        pass: TranscriptPass::Final,
        segment: s,
        text_edited_at: None,
        pass_run: run.map(id),
    }
}

/// The meeting as both computers first held it: one mic line by "Me", no run.
fn base() -> Vec<SyncedRow> {
    let m = Meeting {
        id: id(MEETING),
        meta: base_meta(),
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
        meta: base_meta(),
        meeting_id: id(MEETING),
        kind,
        device_name: None,
        clock_domain: ClockDomain::Replay,
    };
    vec![
        SyncedRow::Meeting(m),
        SyncedRow::Track(track(MIC, TrackId::Mic)),
        SyncedRow::Track(track(REFERENCE, TrackId::Reference)),
        SyncedRow::Speaker(speaker(ME, "Me", base_meta())),
        SyncedRow::Segment(line(LINE, MIC, ME, SHIP, base_meta(), None)),
    ]
}

/// The line as `node` edited it at `dt`: new text, no words, same span and speaker.
fn edited(text: &str, dt: u64, node: DeviceId) -> SegmentRow {
    let mut row = line(LINE, MIC, ME, SHIP, meta(at(0, EDITOR), at(dt, node)), None);
    row.segment.text = text.into();
    row.segment.words.clear();
    row.text_edited_at = Some(at(dt, node));
    row
}

/// The re-transcription's rows: the old line and its speaker tombstoned, a new speaker and a
/// new line of run [`RUN`] over the same words (plus `extra` words after them).
fn pass(extra: &[(&str, u64, u64)]) -> Vec<SyncedRow> {
    let tomb = |m: SyncMeta, dt: u64| {
        let mut m = m;
        m.deleted = true;
        m.updated_at = at(dt, PASSER);
        m
    };
    let mut old = line(LINE, MIC, ME, SHIP, base_meta(), None);
    old.meta = tomb(old.meta, 200);
    let mut me = speaker(ME, "Me", base_meta());
    me.meta = tomb(me.meta, 201);
    let fresh = meta(at(202, PASSER), at(202, PASSER));
    let all: Vec<(&str, u64, u64)> = SHIP.iter().chain(extra).copied().collect();
    vec![
        SyncedRow::Speaker(me),
        SyncedRow::Speaker(speaker(NEW_SPEAKER, "Speaker 1", fresh)),
        SyncedRow::Segment(old),
        SyncedRow::Segment(line(NEW_LINE, MIC, NEW_SPEAKER, &all, fresh, Some(RUN))),
    ]
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

struct Env {
    store: Store,
    clock: ManualClock,
    seq: u64,
    _dir: TempDir,
}

fn env(name: &str) -> Env {
    let dir = TempDir::new(name);
    Env {
        store: Store::open(&dir.path().join("earshot.db")).unwrap(),
        clock: ManualClock::new(T0 + 10_000),
        seq: 0,
        _dir: dir,
    }
}

fn free(_: EntityId) -> bool {
    false
}

fn busy(_: EntityId) -> bool {
    true
}

impl Env {
    fn apply_with(&mut self, rows: &[SyncedRow], leased: &dyn Fn(EntityId) -> bool) -> Applied {
        let page: Vec<Incoming> = rows
            .iter()
            .map(|r| {
                self.seq += 1;
                incoming(self.seq, r)
            })
            .collect();
        let rules = ApplyRules {
            kinds: SyncKinds::ALL,
            leased,
            now_ms: T0 + 10_000,
            after: None,
        };
        let mut hlc = self.store.hlc(HERE).unwrap();
        self.store
            .apply_remote(&page, &rules, &mut hlc, &self.clock)
            .unwrap()
    }

    fn apply(&mut self, rows: &[SyncedRow]) -> Applied {
        self.apply_with(rows, &free)
    }

    fn segment(&self, n: u8) -> SegmentRow {
        self.store
            .segments(id(MEETING))
            .unwrap()
            .into_iter()
            .find(|r| r.segment.id == id(n))
            .unwrap()
    }

    fn speaker(&self, n: u8) -> Speaker {
        self.store
            .speakers(id(MEETING))
            .unwrap()
            .into_iter()
            .find(|s| s.id == id(n))
            .unwrap()
    }

    fn queued(&self, n: u8) -> bool {
        self.store
            .outbox(10_000)
            .unwrap()
            .iter()
            .any(|r| r.id() == id(n))
    }

    /// What must agree across computers: each line's and speaker's content and deletion
    /// (stamps differ, as each computer repairs with its own).
    fn content(&self) -> Vec<String> {
        let mut out: Vec<String> = self
            .store
            .segments(id(MEETING))
            .unwrap()
            .iter()
            .map(|r| {
                format!(
                    "line {} deleted={} text={:?} words={} speaker={:?} edited={:?} run={:?}",
                    r.segment.id,
                    r.meta.deleted,
                    r.segment.text,
                    r.segment.words.len(),
                    r.segment.speaker_id,
                    r.text_edited_at,
                    r.pass_run
                )
            })
            .collect();
        out.extend(self.store.speakers(id(MEETING)).unwrap().iter().map(|s| {
            format!(
                "speaker {} deleted={} name={}",
                s.id, s.meta.deleted, s.display_name
            )
        }));
        out.sort();
        out
    }
}

/// Counts of a page that changed nothing beyond ignoring what it held.
fn quiet(a: &Applied) {
    assert_eq!(
        (
            a.written,
            a.words_dropped_under_edits,
            a.edited_lines_kept,
            a.speakers_revived,
            a.deletions_kept
        ),
        (0, 0, 0, 0, 0),
        "{a:?}"
    );
}

#[test]
fn sync_edits_an_edit_here_is_kept_over_a_later_retranscription_elsewhere() {
    let mut e = env("edits-kept");
    e.apply(&base());
    // The user edits the line here.
    let mut mine = edited("shall we sail", 100, HERE);
    mine.meta.created_at = at(0, EDITOR);
    e.store.put_segment(mine).unwrap();
    // The other computer re-transcribed without seeing it, stamped later.
    let a = e.apply(&pass(&[]));
    let kept = e.segment(LINE);
    assert!(!kept.meta.deleted, "the edit stays live: {kept:?}");
    assert_eq!(kept.segment.text, "shall we sail");
    assert!(
        kept.meta.updated_at > at(200, PASSER),
        "stamped again above the tombstone, so it wins at the relay: {kept:?}"
    );
    assert_eq!(
        kept.text_edited_at,
        Some(at(100, HERE)),
        "edit stamp unchanged"
    );
    assert!(e.queued(LINE), "the edit goes out again");
    // The new line's words lie under the edit: it is tombstoned, keeping its run.
    let new = e.segment(NEW_LINE);
    assert!(new.meta.deleted, "{new:?}");
    assert_eq!(new.pass_run, Some(id(RUN)));
    assert!(e.queued(NEW_LINE));
    // The edited line's speaker is live again, with its name.
    let me = e.speaker(ME);
    assert!(!me.meta.deleted && me.display_name == "Me", "{me:?}");
    assert_eq!(
        (
            a.edited_lines_kept,
            a.words_dropped_under_edits,
            a.speakers_revived
        ),
        (1, 3, 1),
        "{a:?}"
    );
    assert!(a.meetings.contains(&id(MEETING)));
    // The same page again changes nothing.
    let before = e.content();
    quiet(&e.apply(&pass(&[])));
    assert_eq!(e.content(), before);
}

#[test]
fn sync_edits_a_late_edit_replaces_a_retranscription_tombstone_whatever_the_stamps() {
    let mut e = env("edits-late");
    e.apply(&base());
    e.apply(&pass(&[]));
    assert!(e.segment(LINE).meta.deleted);
    assert!(!e.queued(LINE), "applied rows are not sent back");
    // The edit, stamped before the tombstone, arrives after it.
    let a = e.apply(&[SyncedRow::Segment(edited("shall we sail", 100, EDITOR))]);
    let won = e.segment(LINE);
    assert!(!won.meta.deleted, "{won:?}");
    assert_eq!(won.segment.text, "shall we sail");
    assert_eq!(won.text_edited_at, Some(at(100, EDITOR)));
    assert!(
        won.meta.updated_at > at(200, PASSER),
        "stamped again: {won:?}"
    );
    assert!(e.queued(LINE), "the re-stamped edit goes out");
    assert!(e.segment(NEW_LINE).meta.deleted);
    assert!(!e.speaker(ME).meta.deleted);
    assert_eq!(
        (a.written, a.words_dropped_under_edits, a.speakers_revived),
        (1, 3, 1),
        "{a:?}"
    );
    // Neither copy brings anything more.
    quiet(&e.apply(&pass(&[])));
    quiet(&e.apply(&[SyncedRow::Segment(edited("shall we sail", 100, EDITOR))]));
}

#[test]
fn sync_edits_third_computers_reach_the_same_lines_in_any_order() {
    let edit = vec![SyncedRow::Segment(edited("shall we sail", 100, EDITOR))];
    let orders: Vec<Vec<Vec<SyncedRow>>> = vec![
        vec![edit.clone(), pass(&[])],
        vec![pass(&[]), edit.clone()],
        vec![[edit.clone(), pass(&[])].concat()],
        vec![[pass(&[]), edit.clone()].concat()],
    ];
    let mut results = Vec::new();
    for (i, order) in orders.iter().enumerate() {
        let mut e = env(&format!("edits-order-{i}"));
        e.apply(&base());
        for page in order {
            e.apply(page);
        }
        let line = e.segment(LINE);
        assert!(!line.meta.deleted, "order {i}: {line:?}");
        assert_eq!(line.segment.text, "shall we sail", "order {i}");
        assert!(e.segment(NEW_LINE).meta.deleted, "order {i}");
        assert!(!e.speaker(ME).meta.deleted, "order {i}");
        // Settled: every page again writes nothing.
        let before = e.content();
        for page in order {
            quiet(&e.apply(page));
        }
        assert_eq!(e.content(), before, "order {i}");
        results.push(before);
    }
    for (i, r) in results.iter().enumerate().skip(1) {
        assert_eq!(r, &results[0], "order {i} reached other rows than order 0");
    }
}

#[test]
fn sync_edits_a_new_line_keeps_its_words_outside_the_edit() {
    let mut e = env("edits-partial");
    e.apply(&base());
    e.apply(&[SyncedRow::Segment(edited("shall we sail", 100, EDITOR))]);
    let a = e.apply(&pass(&[("friday", 2_500, 2_800)]));
    let new = e.segment(NEW_LINE);
    assert!(!new.meta.deleted, "{new:?}");
    assert_eq!(new.segment.text, "friday");
    assert_eq!(new.segment.start, MeetingTime::from_millis(2_500));
    assert_eq!(
        new.segment.speaker_id,
        Some(id(NEW_SPEAKER)),
        "same speaker"
    );
    assert_eq!(new.pass_run, Some(id(RUN)), "same run");
    assert_eq!(new.text_edited_at, None, "a rewrite is not an edit");
    assert_eq!(a.words_dropped_under_edits, 3, "{a:?}");
    quiet(&e.apply(&pass(&[("friday", 2_500, 2_800)])));
}

#[test]
fn sync_edits_lines_of_the_edited_lines_own_run_and_lines_without_a_run_are_never_compared() {
    // An edit made after the pass on the computer that ran it: every line is of one run.
    let mut e = env("edits-same-run");
    let mut rows = base();
    let mut own = edited("shall we sail", 100, EDITOR);
    own.pass_run = Some(id(RUN));
    rows.push(SyncedRow::Segment(own));
    // Another line of the same run overlapping the edited span, and one with no run.
    let same = line(
        NEW_LINE,
        MIC,
        ME,
        &[("we", 1_400, 1_700)],
        base_meta(),
        Some(RUN),
    );
    let none = line(12, MIC, ME, &[("ship", 1_800, 2_100)], base_meta(), None);
    rows.push(SyncedRow::Segment(same.clone()));
    rows.push(SyncedRow::Segment(none.clone()));
    let a = e.apply(&rows);
    assert_eq!(
        (
            a.words_dropped_under_edits,
            a.speakers_revived,
            a.edited_lines_kept
        ),
        (0, 0, 0),
        "{a:?}"
    );
    assert_eq!(e.segment(NEW_LINE), same, "nothing written");
    assert_eq!(e.segment(12), none, "nothing written");
    assert!(
        e.store.outbox(10_000).unwrap().is_empty(),
        "nothing to send"
    );
}

#[test]
fn sync_edits_a_held_edit_wins_when_its_meeting_is_free() {
    let mut e = env("edits-held");
    e.apply(&base());
    e.apply(&pass(&[]));
    let edit = [SyncedRow::Segment(edited("shall we sail", 100, EDITOR))];
    let a = e.apply_with(&edit, &busy);
    assert_eq!(a.held, {
        let mut h = [0; 4];
        h[HoldReason::ALL
            .iter()
            .position(|r| *r == HoldReason::Busy)
            .unwrap()] = 1;
        h
    });
    assert!(e.segment(LINE).meta.deleted, "held, not applied");
    // Tried again once the meeting is free (the client retries held rows before a page).
    let a = e.apply(&edit);
    assert!(!e.segment(LINE).meta.deleted, "{a:?}");
    assert!(e.segment(NEW_LINE).meta.deleted);
    assert!(e.queued(LINE));
}

#[test]
fn sync_edits_of_two_edits_the_later_wins_and_a_tombstone_never_does() {
    let first = SyncedRow::Segment(edited("shall we sail", 100, EDITOR));
    let second = SyncedRow::Segment(edited("shall we sell", 150, OTHER));
    let tomb = pass(&[]);
    let pages = [vec![first], vec![second], tomb];
    let orders: [[usize; 3]; 6] = [
        [0, 1, 2],
        [0, 2, 1],
        [1, 0, 2],
        [1, 2, 0],
        [2, 0, 1],
        [2, 1, 0],
    ];
    let mut results = Vec::new();
    for (i, order) in orders.iter().enumerate() {
        let mut e = env(&format!("edits-two-{i}"));
        e.apply(&base());
        for k in order {
            e.apply(&pages[*k]);
        }
        let line = e.segment(LINE);
        assert!(!line.meta.deleted, "order {order:?}: {line:?}");
        assert_eq!(line.segment.text, "shall we sell", "order {order:?}");
        assert_eq!(line.text_edited_at, Some(at(150, OTHER)));
        let before = e.content();
        for page in &pages {
            quiet(&e.apply(page));
        }
        assert_eq!(e.content(), before, "order {i}");
        results.push(before);
    }
    for (i, r) in results.iter().enumerate().skip(1) {
        assert_eq!(r, &results[0], "order {i}");
    }
}

#[test]
fn sync_edits_a_rewrite_loses_to_an_edit_of_the_same_line() {
    let mut e = env("edits-rewrite");
    e.apply(&base());
    e.apply(&[SyncedRow::Segment(edited("shall we sail", 100, EDITOR))]);
    e.apply(&pass(&[("friday", 2_500, 2_800)]));
    let rewritten = e.segment(NEW_LINE);
    assert_eq!(rewritten.segment.text, "friday");
    // The computer that re-transcribed edits its new line before hearing of anything, stamped
    // below the rewrite here.
    let mut theirs = line(
        NEW_LINE,
        MIC,
        NEW_SPEAKER,
        &[],
        meta(at(202, PASSER), at(250, PASSER)),
        Some(RUN),
    );
    theirs.segment.start = MeetingTime::from_millis(1_000);
    theirs.segment.end = MeetingTime::from_millis(2_800);
    theirs.segment.text = "shall we ship on friday".into();
    theirs.text_edited_at = Some(at(250, PASSER));
    assert!(theirs.meta.updated_at < rewritten.meta.updated_at);
    let a = e.apply(&[SyncedRow::Segment(theirs)]);
    let won = e.segment(NEW_LINE);
    assert!(!won.meta.deleted, "{won:?}");
    assert_eq!(won.segment.text, "shall we ship on friday");
    assert_eq!(won.text_edited_at, Some(at(250, PASSER)));
    assert!(
        won.meta.updated_at > rewritten.meta.updated_at,
        "stamped again"
    );
    assert_eq!(
        a.words_dropped_under_edits, 0,
        "edited lines are never changed"
    );
    // Both edits stay.
    assert_eq!(e.segment(LINE).segment.text, "shall we sail");
}

#[test]
fn sync_edits_live_draft_copies_keep_plain_order() {
    let mut e = env("edits-live");
    e.apply(&base());
    let mut draft = line(30, MIC, ME, SHIP, base_meta(), None);
    draft.pass = TranscriptPass::Live;
    draft.text_edited_at = Some(at(100, HERE));
    draft.meta.updated_at = at(100, HERE);
    e.store.put_segment(draft.clone()).unwrap();
    let mut later = line(30, MIC, ME, SHIP, base_meta(), None);
    later.pass = TranscriptPass::Live;
    later.meta.updated_at = at(300, PASSER);
    later.meta.deleted = true;
    let a = e.apply(&[SyncedRow::Segment(later.clone())]);
    assert_eq!(a.written, 1, "{a:?}");
    assert_eq!(e.segment(30), later, "the later copy wins as written");
}

/// The line as a meeting deletion on `node` leaves it at `dt` (core `delete_meeting`'s erasure).
fn erased(dt: u64, node: DeviceId) -> SegmentRow {
    let mut row = line(LINE, MIC, ME, SHIP, base_meta(), None);
    row.meta.deleted = true;
    row.meta.updated_at = at(dt, node);
    row.segment.text.clear();
    row.segment.words.clear();
    row.segment.confidence = 0.0;
    row.segment.speaker_id = None;
    assert!(row.is_erasure());
    row
}

/// Review advisory 1: a meeting deletion's erasure is compared by plain stamp order, so a
/// later erasure replaces an edit here (nothing re-stamped, no text sent) and an earlier one
/// changes nothing.
#[test]
fn sync_edits_a_deletion_erasure_is_compared_by_its_stamp_alone() {
    let mut e = env("edits-erasure");
    e.apply(&base());
    e.store
        .put_segment(edited("shall we sail", 100, HERE))
        .unwrap();
    let a = e.apply(&[SyncedRow::Segment(erased(200, PASSER))]);
    let line = e.segment(LINE);
    assert!(line.is_erasure(), "{line:?}");
    assert_eq!(line.meta.updated_at, at(200, PASSER), "written as it came");
    assert!(!e.queued(LINE), "no text goes out");
    assert_eq!((a.written, a.edited_lines_kept), (1, 0), "{a:?}");

    let mut e = env("edits-erasure-older");
    e.apply(&base());
    let mine = edited("shall we sail", 100, HERE);
    e.store.put_segment(mine.clone()).unwrap();
    let a = e.apply(&[SyncedRow::Segment(erased(50, PASSER))]);
    assert_eq!(e.segment(LINE), mine, "an older erasure changes nothing");
    assert_eq!((a.ignored, a.edited_lines_kept), (1, 0), "{a:?}");
}

/// Review advisory 1: rule 4a applies only while the meeting is live here; with the meeting
/// deleted here a later pass tombstone wins by its stamp and nothing is re-stamped.
#[test]
fn sync_edits_the_edit_order_needs_the_meeting_live_here() {
    let mut e = env("edits-deleted-meeting");
    e.apply(&base());
    e.store
        .put_segment(edited("shall we sail", 100, HERE))
        .unwrap();
    let mut gone = e.store.meeting(id(MEETING)).unwrap().unwrap();
    gone.meta.deleted = true;
    gone.meta.updated_at = at(150, HERE);
    e.store.put_meeting(gone).unwrap();
    let mut tomb = line(LINE, MIC, ME, SHIP, base_meta(), None);
    tomb.meta.deleted = true;
    tomb.meta.updated_at = at(200, PASSER);
    let a = e.apply(&[SyncedRow::Segment(tomb.clone())]);
    assert_eq!(e.segment(LINE), tomb, "plain stamp order");
    assert_eq!(a.edited_lines_kept, 0, "{a:?}");
}

/// Review advisory 1: rules 4b and 4c skip a meeting deleted here, even when the page wrote a
/// row of it.
#[test]
fn sync_edits_the_span_repair_needs_the_meeting_live_here() {
    let mut e = env("edits-deleted-repair");
    e.apply(&base());
    e.store
        .put_segment(edited("shall we sail", 100, HERE))
        .unwrap();
    // A different run's line under the edit, and a tombstoned speaker the edit names: written
    // here directly, so no rule has run over them yet.
    let under = line(NEW_LINE, MIC, ME, SHIP, base_meta(), Some(RUN));
    e.store.put_segment(under.clone()).unwrap();
    let mut me = e.speaker(ME);
    me.meta.deleted = true;
    me.meta.updated_at = at(120, HERE);
    e.store.put_speaker(me.clone()).unwrap();
    let mut gone = e.store.meeting(id(MEETING)).unwrap().unwrap();
    gone.meta.deleted = true;
    gone.meta.updated_at = at(150, HERE);
    e.store.put_meeting(gone).unwrap();
    // The page writes a tombstone of another line of the meeting.
    let mut other = line(
        12,
        REFERENCE,
        ME,
        &[("far", 9_000, 9_300)],
        base_meta(),
        None,
    );
    other.meta.deleted = true;
    other.meta.updated_at = at(200, PASSER);
    let a = e.apply(&[SyncedRow::Segment(other)]);
    assert!(a.meetings.contains(&id(MEETING)), "{a:?}");
    assert_eq!(e.segment(NEW_LINE), under, "untouched");
    assert_eq!(e.speaker(ME), me, "not revived");
    assert_eq!(
        (a.words_dropped_under_edits, a.speakers_revived),
        (0, 0),
        "{a:?}"
    );
}

/// Review advisory 6: rules 4b and 4c skip a meeting this process holds, even when the page
/// wrote one of its rows (a summary request is never held as busy).
#[test]
fn sync_edits_the_span_repair_waits_while_the_meeting_is_leased() {
    use earshot_core::model::{RequestState, SummaryRequest};
    let mut e = env("edits-leased");
    e.apply(&base());
    e.store
        .put_segment(edited("shall we sail", 100, HERE))
        .unwrap();
    let under = line(NEW_LINE, MIC, ME, SHIP, base_meta(), Some(RUN));
    e.store.put_segment(under.clone()).unwrap();
    let request = |dt: u64| SummaryRequest {
        id: id(40),
        meta: meta(at(200, PASSER), at(dt, PASSER)),
        meeting_id: id(MEETING),
        target_device: HERE,
        asking_device: PASSER,
        segment_count: 2,
        state: RequestState::Requested,
        reason: String::new(),
    };
    let a = e.apply_with(&[SyncedRow::SummaryRequest(request(200))], &busy);
    assert_eq!(a.written, 1, "the request is written while leased: {a:?}");
    assert!(a.meetings.contains(&id(MEETING)));
    assert_eq!(e.segment(NEW_LINE), under, "untouched while leased");
    assert_eq!(a.words_dropped_under_edits, 0, "{a:?}");
    // Once free, the next page that names the meeting repairs it.
    let a = e.apply(&[SyncedRow::SummaryRequest(request(210))]);
    assert!(e.segment(NEW_LINE).meta.deleted, "{a:?}");
    assert_eq!(
        (a.words_dropped_under_edits, a.lines_rewritten_under_edits),
        (3, 1),
        "{a:?}"
    );
}

/// Review advisory 2: a "Me" row an edited line names comes back as a room voice when the
/// meeting already has a live "Me" row, never as a second "Me".
#[test]
fn sync_edits_a_revived_me_row_is_a_room_voice_beside_a_live_me() {
    let mut e = env("edits-two-me");
    e.apply(&base());
    e.apply(&[SyncedRow::Segment(edited("shall we sail", 100, EDITOR))]);
    // After the re-transcription the user marked the new voice as themselves.
    let mut rows = pass(&[]);
    for r in &mut rows {
        if let SyncedRow::Speaker(s) = r
            && s.id == id(NEW_SPEAKER)
        {
            s.role = SpeakerRole::Me;
        }
    }
    let a = e.apply(&rows);
    assert_eq!(a.speakers_revived, 1, "{a:?}");
    let revived = e.speaker(ME);
    assert!(!revived.meta.deleted);
    assert_eq!(revived.role, SpeakerRole::Room, "{revived:?}");
    assert_eq!(revived.display_name, "Me", "name kept");
    let live_me = e
        .store
        .speakers(id(MEETING))
        .unwrap()
        .into_iter()
        .filter(|s| !s.meta.deleted && s.role == SpeakerRole::Me)
        .count();
    assert_eq!(live_me, 1);
    // With no other live "Me", the row comes back as "Me" (the first test).
}

/// Review advisory 6: a final copy and a live copy of one row keep plain stamp order: a later
/// live copy replaces an edited final one.
#[test]
fn sync_edits_mixed_final_and_live_copies_keep_plain_order() {
    let mut e = env("edits-mixed");
    e.apply(&base());
    e.store
        .put_segment(edited("shall we sail", 100, HERE))
        .unwrap();
    let mut draft = line(LINE, MIC, ME, SHIP, base_meta(), None);
    draft.pass = TranscriptPass::Live;
    draft.meta.updated_at = at(300, PASSER);
    let a = e.apply(&[SyncedRow::Segment(draft.clone())]);
    assert_eq!((a.written, a.edited_lines_kept), (1, 0), "{a:?}");
    assert_eq!(e.segment(LINE), draft);
}

/// Review advisory 6, the spec's known edge: a split part may end 1 ms past the line, and a
/// different run's word starting in that millisecond is dropped; one starting after it stays.
#[test]
fn sync_edits_a_word_in_a_split_parts_last_millisecond_is_dropped() {
    let mut e = env("edits-split-edge");
    e.apply(&base());
    // The second part of a split whose untimed words sat at the line's end (2 100 ms).
    let mut part = edited("ship", 100, EDITOR);
    part.segment.start = MeetingTime::from_millis(2_099);
    part.segment.end = MeetingTime::from_millis(2_101);
    e.apply(&[SyncedRow::Segment(part)]);
    let fresh = meta(at(202, PASSER), at(202, PASSER));
    let inside = line(
        NEW_LINE,
        MIC,
        ME,
        &[("edge", 2_100, 2_400)],
        fresh,
        Some(RUN),
    );
    let after = line(12, MIC, ME, &[("after", 2_101, 2_400)], fresh, Some(RUN));
    let a = e.apply(&[
        SyncedRow::Segment(inside),
        SyncedRow::Segment(after.clone()),
    ]);
    assert!(e.segment(NEW_LINE).meta.deleted, "{a:?}");
    assert_eq!(e.segment(12), after, "a word from the line's end on stays");
    assert_eq!(a.words_dropped_under_edits, 1, "{a:?}");
}
