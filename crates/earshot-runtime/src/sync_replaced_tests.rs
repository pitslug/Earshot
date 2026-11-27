//! T14.47: replaced transcript text is erased (docs/sync-spec.md section 15), through the real
//! relay (`LocalRelay`). A child of the sync tests, so it shares their relay and device
//! helpers. Every name holds `sync`; every word is invented.

use earshot_core::asr::{MeetingTime, Segment, Word};
use earshot_core::capture::TrackId;
use earshot_core::diar::SpeakerRole;
use earshot_core::identity::EntityId;
use earshot_core::model::{
    ClockDomain, Meeting, SegmentRow, Speaker, Stamp, Track, TranscriptPass,
};
use earshot_core::prompt::Template;
use rand_core::SeedableRng;

use super::*;

fn seg(
    track: EntityId,
    text: &str,
    start: u64,
    rng: &mut rand_chacha::ChaCha8Rng,
    clock: &ManualClock,
) -> Segment {
    let words: Vec<Word> = text
        .split(' ')
        .enumerate()
        .map(|(i, w)| Word::new(w, start + 400 * i as u64, start + 400 * i as u64 + 300, 0.9))
        .collect();
    Segment::from_words(
        EntityId::generate(clock, rng),
        track,
        words,
        MeetingTime::from_millis(start),
    )
}

/// A meeting recorded on `dev`: two tracks, a speaker, a draft on each track and no final line.
fn recorded(dev: &Device, seed: u64) -> (EntityId, Track, Track, EntityId) {
    let clock = Arc::clone(&dev.clock);
    let mut rng = rand_chacha::ChaCha8Rng::seed_from_u64(seed);
    let mut db = dev.svc.store();
    let (store, hlc) = db.parts();
    let mut stamp = Stamp {
        clock: &*clock,
        hlc,
        rng: &mut rng,
    };
    let m = Meeting::new("Invented review", 0, 600, Template::General, &mut stamp);
    let mic = Track::new(m.id, TrackId::Mic, None, ClockDomain::Replay, &mut stamp);
    let them = Track::new(
        m.id,
        TrackId::Reference,
        None,
        ClockDomain::Replay,
        &mut stamp,
    );
    let me = Speaker::new(m.id, SpeakerRole::Me, "Me", None, &mut stamp);
    store.put_meeting(m.clone()).unwrap();
    store.put_track(mic.clone()).unwrap();
    store.put_track(them.clone()).unwrap();
    store.put_speaker(me.clone()).unwrap();
    for (k, t) in [&mic, &them].iter().enumerate() {
        for j in 0..3u64 {
            let start = 1_000 + 5_000 * j + 500 * k as u64;
            let s = seg(t.id, "draft words here", start, stamp.rng, &clock);
            let row = SegmentRow::new(m.id, TranscriptPass::Live, s, stamp.hlc, stamp.clock);
            store.put_segment(row).unwrap();
        }
    }
    (m.id, mic, them, me.id)
}

/// The owner's final pass, as `post` writes it: final lines on both tracks, then what they
/// replaced cleared, last, in one transaction.
fn pass(dev: &Device, m: EntityId, mic: &Track, them: &Track, me: EntityId, seed: u64) {
    let clock = Arc::clone(&dev.clock);
    let mut rng = rand_chacha::ChaCha8Rng::seed_from_u64(seed);
    let mut db = dev.svc.store();
    let (store, hlc) = db.parts();
    store
        .transaction(|tx| -> Result<(), earshot_store::StoreError> {
            use earshot_core::model::MeetingRepo as _;
            for r in tx.segments(m).map_err(earshot_store::StoreError::Repo)? {
                if r.pass == TranscriptPass::Final && !r.meta.deleted {
                    let mut old = r;
                    old.meta.mark_deleted(hlc, &*clock);
                    tx.put_segment(old)
                        .map_err(earshot_store::StoreError::Repo)?;
                }
            }
            for (k, t) in [mic, them].iter().enumerate() {
                for j in 0..2u64 {
                    let mut s = seg(
                        t.id,
                        "final words",
                        1_000 + 6_000 * j + 500 * k as u64,
                        &mut rng,
                        &clock,
                    );
                    s.speaker_id = Some(me);
                    let row = SegmentRow::new(m, TranscriptPass::Final, s, hlc, &*clock);
                    tx.put_segment(row)
                        .map_err(earshot_store::StoreError::Repo)?;
                }
            }
            tx.clear_replaced_text(m, None, hlc, &*clock)?;
            tx.send_lines_first(hlc, &*clock)?;
            Ok(())
        })
        .unwrap();
}

/// How many lines `dev` shows for `m`, per track.
fn shown(dev: &Device, m: EntityId, mic: &Track, them: &Track) -> (usize, usize) {
    // The view's rule: live final lines, else the live draft, meeting-wide.
    let shown = dev.svc.store().shown_lines(m).unwrap();
    let ids: std::collections::BTreeSet<EntityId> = shown.lines.iter().map(|l| l.id).collect();
    let rows = dev.svc.store().segments(m).unwrap();
    let on = |t: &Track| {
        rows.iter()
            .filter(|r| ids.contains(&r.segment.id) && r.segment.track == t.id)
            .count()
    };
    (on(mic), on(them))
}

fn holds_replaced_text(dev: &Device, m: EntityId) -> Vec<String> {
    let rows = dev.svc.store().segments(m).unwrap();
    let finals: std::collections::BTreeSet<EntityId> = rows
        .iter()
        .filter(|r| r.pass == TranscriptPass::Final && !r.meta.deleted)
        .map(|r| r.segment.track)
        .collect();
    rows.iter()
        .filter(|r| {
            (r.meta.deleted && r.holds_text())
                || (r.pass == TranscriptPass::Live
                    && !r.meta.deleted
                    && finals.contains(&r.segment.track))
        })
        .map(|r| format!("{} {:?} deleted={}", r.segment.id, r.pass, r.meta.deleted))
        .collect()
}

/// What the receiver shows after each prefix of the owner's push, pulled a row at a time: the
/// draft or the lines, never nothing; at the end no row holds replaced text anywhere and the
/// receiver has queued nothing for the meeting (the cleared drafts were held until their lines
/// applied, then applied by the retry in the tick that brought them).
#[test]
fn sync_replaced_a_receiver_shows_the_draft_or_the_lines_never_nothing() {
    let relay = relay();
    let (a, b) = pair(&relay);
    let ca = a.client();
    let cb = b.client().with_pull_pages(1, 1);
    let (m, mic, them, me) = recorded(&a, 31);
    ca.tick().unwrap();
    for _ in 0..40 {
        cb.tick().unwrap();
    }
    assert_eq!(
        shown(&b, m, &mic, &them),
        (3, 3),
        "the receiver shows the draft"
    );
    a.clock.advance_ms(1_000);
    pass(&a, m, &mic, &them, me, 32);
    assert!(holds_replaced_text(&a, m).is_empty());
    ca.tick().unwrap();
    let mut ticks = 0;
    // The view is meeting-wide (live final lines, else the live draft, 15.2), so a track whose
    // final lines have not arrived yet shows nothing once the other track's have: that is the
    // order the rows arrive in, with or without this rule. What this rule must never cause is
    // the meeting showing nothing, or a track that showed final lines losing them.
    let mut had_lines = (false, false);
    loop {
        cb.tick().unwrap();
        ticks += 1;
        let (on_mic, on_them) = shown(&b, m, &mic, &them);
        assert!(
            on_mic + on_them > 0,
            "tick {ticks}: the meeting shows nothing"
        );
        let finals = |t: &Track| {
            b.svc.store().segments(m).unwrap().iter().any(|r| {
                r.pass == TranscriptPass::Final && !r.meta.deleted && r.segment.track == t.id
            })
        };
        assert!(
            !had_lines.0 || finals(&mic),
            "tick {ticks}: the mic lost its lines"
        );
        assert!(
            !had_lines.1 || finals(&them),
            "tick {ticks}: the far end lost its lines"
        );
        had_lines = (had_lines.0 || finals(&mic), had_lines.1 || finals(&them));
        if holds_replaced_text(&b, m).is_empty() && shown(&b, m, &mic, &them) == (2, 2) {
            break;
        }
        assert!(ticks < 60, "the receiver did not settle");
    }
    assert_eq!(
        b.svc
            .store()
            .held_count(&earshot_store::HoldReason::ALL)
            .unwrap(),
        0
    );
    let queued: Vec<_> = b
        .svc
        .store()
        .outbox(10_000)
        .unwrap()
        .into_iter()
        .filter(|r| r.meeting() == Some(m))
        .collect();
    assert!(queued.is_empty(), "{queued:?}");
}

/// A pulled tombstone that holds text, stamped ahead of the receiver's clock, still goes out
/// cleared and the relay accepts it; the owner then holds the cleared copy.
#[test]
fn sync_replaced_a_pulled_text_tombstone_ahead_goes_out_cleared() {
    let relay = relay();
    let (a, b) = pair(&relay);
    let (ca, cb) = (a.client(), b.client());
    let (m, mic, them, me) = recorded(&a, 41);
    pass(&a, m, &mic, &them, me, 42);
    ca.tick().unwrap();
    cb.tick().unwrap();
    // A peer that never clears (a 0.9.0 pass): one final line tombstoned with its text,
    // stamped an hour ahead of the receiver's clock.
    a.clock.advance_ms(3_600_000);
    let line = {
        let clock = Arc::clone(&a.clock);
        let mut db = a.svc.store();
        let (store, hlc) = db.parts();
        let mut line = store
            .segments(m)
            .unwrap()
            .into_iter()
            .find(|r| r.pass == TranscriptPass::Final && !r.meta.deleted)
            .unwrap();
        line.meta.mark_deleted(hlc, &*clock);
        // Written as an old build leaves it: straight into the table with its text.
        store.put_segment(line.clone()).unwrap();
        line
    };
    assert!(line.holds_text());
    ca.tick().unwrap();
    let got = cb.tick().unwrap();
    assert_eq!(got.text_emptied, 1, "{got:?}");
    let here = b
        .svc
        .store()
        .segments(m)
        .unwrap()
        .into_iter()
        .find(|r| r.segment.id == line.segment.id)
        .unwrap();
    assert!(
        here.is_cleared() && here.meta.updated_at > line.meta.updated_at,
        "{here:?}"
    );
    let sent = cb.tick().unwrap();
    assert!(sent.pushed >= 1 && !sent.clock_refused, "{sent:?}");
    ca.tick().unwrap();
    let there = a
        .svc
        .store()
        .segments(m)
        .unwrap()
        .into_iter()
        .find(|r| r.segment.id == line.segment.id)
        .unwrap();
    assert!(there.is_cleared(), "{there:?}");
}

/// The cleared form's 1.0 and an erasure's 0.0 come back exactly through a sealed envelope.
#[test]
fn sync_replaced_confidences_are_exact_through_a_sealed_envelope() {
    let keys = earshot_sync::VaultKey::from_bytes([7; 32]).keys();
    let dev = Device::new("sync-replaced-seal");
    let (m, _mic, _them, _me) = recorded(&dev, 51);
    let row = dev.svc.store().segments(m).unwrap().remove(0);
    let mut cleared = row.cleared();
    cleared.meta.updated_at = row.meta.updated_at;
    let mut erased = cleared.clone();
    erased.segment.confidence = 0.0;
    for (r, want) in [(cleared, 1.0_f32), (erased, 0.0)] {
        let sealed = earshot_sync::seal_row(&keys, &r).unwrap();
        let inc = super::engine::open(&keys, 1, &sealed.key.0, sealed.stamp, sealed.body).unwrap();
        let Some(earshot_store::SyncedRow::Segment(back)) = inc.row else {
            panic!("a segment row");
        };
        assert_eq!(back.segment.confidence.to_bits(), want.to_bits());
        assert_eq!(back.is_erasure(), want == 0.0);
        assert_eq!(back.is_cleared(), want == 1.0);
    }
}

/// The check of "lines go out before what they replace" and the push judge the same rows
/// refused: one guard, the push's own.
#[test]
fn sync_replaced_the_check_and_the_push_judge_the_same_rows_refused() {
    let keys = earshot_sync::VaultKey::from_bytes([9; 32]).keys();
    let dev = Device::new("sync-replaced-guard");
    let (m, _mic, _them, _me) = recorded(&dev, 61);
    let row = dev.svc.store().segments(m).unwrap().remove(0);
    let mut big = row.clone();
    big.segment.text = "a".repeat(earshot_sync::MAX_PLAINTEXT + 1);
    let mut audio = row.clone();
    audio.segment.text = concat!("invented f", "LaC words").into();
    let guard = crate::sync::push_guard();
    for (r, sendable) in [(row, true), (big, false), (audio, false)] {
        let r = earshot_store::SyncedRow::Segment(r);
        assert_eq!(guard.sendable(&r), sendable);
        assert_eq!(super::engine::seal_for_test(&keys, &r).is_ok(), sendable);
    }
}
