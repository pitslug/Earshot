//! T14.24: the transcript a meeting shows, read without words, and a damaged row left out and
//! counted (`cargo test -p earshot-store shown_lines`).

mod common;

use common::TempDir;
use earshot_core::asr::{MeetingTime, Segment, Word};
use earshot_core::capture::TrackId;
use earshot_core::identity::{DeviceId, EntityId, Hlc, ManualClock};
use earshot_core::model::{
    ClockDomain, Meeting, MeetingRepo, SegmentRow, Stamp, Track, TranscriptPass, shown_lines_of,
};
use earshot_core::prompt::Template;
use earshot_store::{ReadOnlyStore, Store};
use rand_chacha::ChaCha8Rng;
use rand_core::SeedableRng;

struct World {
    clock: ManualClock,
    hlc: Hlc,
    rng: ChaCha8Rng,
}

impl World {
    fn new() -> Self {
        World {
            clock: ManualClock::new(1_800_000_000_000),
            hlc: Hlc::new(DeviceId::from_bytes([7; 16])),
            rng: ChaCha8Rng::seed_from_u64(1424),
        }
    }
    fn stamp(&mut self) -> Stamp<'_, ManualClock, ChaCha8Rng> {
        self.clock.advance_ms(1);
        Stamp {
            clock: &self.clock,
            hlc: &mut self.hlc,
            rng: &mut self.rng,
        }
    }
    fn line(
        &mut self,
        meeting: &Meeting,
        track: &Track,
        pass: TranscriptPass,
        at: u64,
    ) -> SegmentRow {
        self.clock.advance_ms(1);
        let seg = Segment::from_words(
            EntityId::generate(&self.clock, &mut self.rng),
            track.id,
            vec![
                Word::new("Invented", at, at + 300, 0.9),
                Word::new(format!("line{at}."), at + 310, at + 600, 0.8),
            ],
            MeetingTime::ZERO,
        );
        SegmentRow::new(meeting.id, pass, seg, &mut self.hlc, &self.clock)
    }
}

fn raw(db: &std::path::Path, sql: &str, id: EntityId) {
    let c = rusqlite::Connection::open(db).unwrap();
    let n = c.execute(sql, [id.as_uuid().as_bytes().to_vec()]).unwrap();
    assert_eq!(n, 1, "{sql}");
}

#[test]
fn store_shown_lines_leave_out_and_count_a_damaged_row_and_never_read_words() {
    let tmp = TempDir::new("shown-lines");
    let db = tmp.path().join("earshot.db");
    let mut store = Store::open(&db).unwrap();
    let mut w = World::new();
    let m = Meeting::new("Invented review", 1, 0, Template::General, &mut w.stamp());
    store.put_meeting(m.clone()).unwrap();
    let mic = Track::new(m.id, TrackId::Mic, None, ClockDomain::Qpc, &mut w.stamp());
    store.put_track(mic.clone()).unwrap();
    let rows: Vec<SegmentRow> = [1_000, 2_000, 3_000, 4_000]
        .map(|at| w.line(&m, &mic, TranscriptPass::Final, at))
        .into();
    for r in &rows {
        store.put_segment(r.clone()).unwrap();
    }
    // A draft row is never shown beside a final transcript.
    store
        .put_segment(w.line(&m, &mic, TranscriptPass::Live, 500))
        .unwrap();
    let whole = store.shown_lines(m.id).unwrap();
    assert_eq!(whole, shown_lines_of(store.segments(m.id).unwrap()));
    assert_eq!(whole.lines.len(), 4);
    drop(store);

    // Damaged words: the whole-row read fails, the shown lines do not read them.
    raw(
        &db,
        "UPDATE segment SET words = '[not json' WHERE id = ?1",
        rows[0].segment.id,
    );
    // Damaged text (not UTF-8) and a start that is no time: left out and counted.
    raw(
        &db,
        "UPDATE segment SET text = CAST(X'FF00FE' AS TEXT) WHERE id = ?1",
        rows[1].segment.id,
    );
    raw(
        &db,
        "UPDATE segment SET start_ms = -5 WHERE id = ?1",
        rows[2].segment.id,
    );

    let store = Store::open(&db).unwrap();
    assert!(store.segments(m.id).is_err(), "the whole rows do not read");
    let shown = store.shown_lines(m.id).unwrap();
    assert_eq!(shown.pass, Some(TranscriptPass::Final));
    assert_eq!(shown.unreadable, 2, "{shown:?}");
    let ids: Vec<EntityId> = shown.lines.iter().map(|l| l.id).collect();
    assert_eq!(ids, vec![rows[0].segment.id, rows[3].segment.id]);
    assert_eq!(shown.lines[0].text, rows[0].segment.text);
    assert_eq!(shown.lines[1].start, rows[3].segment.start);
    // The read-only store reads the same.
    drop(store);
    let ro = ReadOnlyStore::open(&db).unwrap();
    assert_eq!(ro.shown_lines(m.id).unwrap(), shown);
}
