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

/// The relay order of two re-transcriptions (15.4): the owner re-transcribes twice while the
/// receiver, holding the first pass's lines F0 live, is away. The relay then serves cleared F0,
/// F2 and cleared F1, in that order. Pulled a row at a time the receiver shows F0 or F2 at each
/// step, never nothing: H2 holds the last F0 line of each track until F2 applies.
#[test]
fn sync_replaced_the_relay_order_of_two_retranscriptions_never_blanks_a_receiver() {
    let relay = relay();
    let (a, b) = pair(&relay);
    let ca = a.client();
    let cb = b.client().with_pull_pages(1, 1);
    let (m, mic, them, me) = recorded(&a, 101);
    a.clock.advance_ms(1_000);
    pass(&a, m, &mic, &them, me, 102);
    ca.tick().unwrap();
    for _ in 0..60 {
        cb.tick().unwrap();
    }
    assert_eq!(shown(&b, m, &mic, &them), (2, 2), "the receiver holds F0");
    a.clock.advance_ms(1_000);
    pass(&a, m, &mic, &them, me, 103);
    ca.tick().unwrap();
    a.clock.advance_ms(1_000);
    pass(&a, m, &mic, &them, me, 104);
    ca.tick().unwrap();
    let mut held_once = false;
    for tick in 0..80 {
        let r = cb.tick().unwrap();
        held_once |= b
            .svc
            .store()
            .held_count(&[earshot_store::HoldReason::Parent])
            .unwrap()
            > 0;
        let (on_mic, on_them) = shown(&b, m, &mic, &them);
        assert!(
            on_mic + on_them > 0,
            "tick {tick}: the receiver shows nothing"
        );
        if r.pull_complete && holds_replaced_text(&b, m).is_empty() && (on_mic, on_them) == (2, 2) {
            break;
        }
    }
    assert!(held_once, "H2 held a row on the way");
    assert_eq!(shown(&b, m, &mic, &them), (2, 2));
    assert!(holds_replaced_text(&b, m).is_empty());
}

/// Word choices are dropped in a pulled page's transaction (15.3) without failing it on one
/// segment row that does not read: the page applies, the choice on the damaged line is dropped
/// and the one on a line that still reads as recorded stays.
#[test]
fn sync_replaced_word_choices_beside_a_damaged_line_never_fail_a_page() {
    use crate::word_choices::{Choice, WordChoice};
    let relay = relay();
    let (a, b) = pair(&relay);
    let (ca, cb) = (a.client(), b.client());
    let m = crate::testutil::meeting(&a.svc, "Invented planning", 111);
    ca.tick().unwrap();
    cb.tick().unwrap();
    let lines: Vec<SegmentRow> = b
        .svc
        .store()
        .segments(m)
        .unwrap()
        .into_iter()
        .filter(|r| r.pass == TranscriptPass::Final && !r.meta.deleted)
        .collect();
    let mic = lines
        .iter()
        .find(|r| r.segment.text.starts_with("shall"))
        .unwrap();
    let them = lines
        .iter()
        .find(|r| r.segment.text.starts_with("yes"))
        .unwrap();
    let choice = |segment: EntityId, decoded: &str| WordChoice {
        segment,
        token: 0,
        decoded: decoded.into(),
        choice: Choice::Kept,
    };
    let record = crate::word_choices::encode(&[
        choice(mic.segment.id, "shall"),
        choice(them.segment.id, "yes"),
    ])
    .unwrap();
    b.svc
        .store()
        .set_local_value(crate::word_choices::record_key(m).as_str(), &record)
        .unwrap();
    // The mic line does not read on B.
    let hex: String = mic
        .segment
        .id
        .as_uuid()
        .as_bytes()
        .iter()
        .map(|x| format!("{x:02x}"))
        .collect();
    rusqlite::Connection::open(b.svc.data.db_path())
        .unwrap()
        .execute_batch(&format!(
            "UPDATE segment SET created_logical = -1 WHERE id = x'{hex}'"
        ))
        .unwrap();
    // A edits the far end's line; B pulls it.
    a.clock.advance_ms(1_000);
    crate::edit::edit_segment(&a.svc, m, them.segment.id, "yes indeed").unwrap();
    ca.tick().unwrap();
    let got = cb.tick().unwrap();
    assert!(got.written >= 1, "{got:?}");
    let now = b
        .svc
        .store()
        .local_value(crate::word_choices::record_key(m).as_str())
        .unwrap()
        .map(|v| crate::word_choices::decode(&v))
        .unwrap_or_default();
    assert_eq!(now, vec![choice(them.segment.id, "yes")]);
}

// ---- A format-4 peer with no clearing rule (15.9, 15.10) ----
//
// The peer is a device whose own writes are made as 0.9.0 makes them: its passes tombstone every
// unedited final line of a processed track with its text and leave its draft live, and it never
// runs the clearing, the sweep or a re-stamp. It needs no test switch in shipped code: in what it
// pulls here (only cleared copies of its own tombstones, which hold no text) this build's apply
// does what 0.9.0's does, whole-row last writer wins, no hold (H1 and H2 need a live copy here)
// and no clearing (there is no text to clear); the tests assert that it holds nothing and sends
// nothing back. It never runs `clear_replaced_text`, and it holds no queued cleared row, so the
// commit's "lines go out first" check finds nothing to stamp again.

/// A meeting recorded on `dev` with tracks of `kinds`, a speaker and three draft rows a track.
fn recorded_with(dev: &Device, kinds: &[TrackId], seed: u64) -> (EntityId, Vec<Track>, EntityId) {
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
    store.put_meeting(m.clone()).unwrap();
    let tracks: Vec<Track> = kinds
        .iter()
        .map(|k| {
            let t = Track::new(m.id, *k, None, ClockDomain::Replay, &mut stamp);
            store.put_track(t.clone()).unwrap();
            t
        })
        .collect();
    let me = Speaker::new(m.id, SpeakerRole::Me, "Me", None, &mut stamp);
    store.put_speaker(me.clone()).unwrap();
    for (k, t) in tracks.iter().enumerate() {
        for j in 0..3u64 {
            let start = 1_000 + 5_000 * j + 500 * k as u64;
            let s = seg(t.id, "draft words here", start, stamp.rng, &clock);
            let row = SegmentRow::new(m.id, TranscriptPass::Live, s, stamp.hlc, stamp.clock);
            store.put_segment(row).unwrap();
        }
    }
    (m.id, tracks, me.id)
}

/// A pass as 0.9.0 writes it on the peer: each track in `found` (by index into `tracks`) gets
/// two new final lines; every track processed (all of them) has its unedited live final lines
/// tombstoned with their text, before the new lines; drafts stay live; nothing is cleared.
fn pass_as_0_9_0(
    dev: &Device,
    m: EntityId,
    tracks: &[Track],
    found: &[usize],
    me: EntityId,
    seed: u64,
) {
    use earshot_core::model::MeetingRepo as _;
    let clock = Arc::clone(&dev.clock);
    let mut rng = rand_chacha::ChaCha8Rng::seed_from_u64(seed);
    let mut db = dev.svc.store();
    let (store, hlc) = db.parts();
    store
        .transaction(|tx| -> Result<(), earshot_store::StoreError> {
            for r in tx.segments(m).map_err(earshot_store::StoreError::Repo)? {
                if r.pass == TranscriptPass::Final && !r.meta.deleted && r.text_edited_at.is_none()
                {
                    let mut old = r;
                    old.meta.mark_deleted(hlc, &*clock);
                    tx.put_segment(old)
                        .map_err(earshot_store::StoreError::Repo)?;
                }
            }
            for &k in found {
                for j in 0..2u64 {
                    let start = 1_000 + 6_000 * j + 500 * k as u64;
                    let mut s = seg(tracks[k].id, "final words", start, &mut rng, &clock);
                    s.speaker_id = Some(me);
                    let row = SegmentRow::new(m, TranscriptPass::Final, s, hlc, &*clock);
                    tx.put_segment(row)
                        .map_err(earshot_store::StoreError::Repo)?;
                }
            }
            Ok(())
        })
        .unwrap();
}

/// What `dev` shows of `m`: the pass shown and the number of lines per track of `tracks`.
fn showing(dev: &Device, m: EntityId, tracks: &[Track]) -> (Option<TranscriptPass>, Vec<usize>) {
    let shown = dev.svc.store().shown_lines(m).unwrap();
    let ids: std::collections::BTreeSet<EntityId> = shown.lines.iter().map(|l| l.id).collect();
    let rows = dev.svc.store().segments(m).unwrap();
    let per = tracks
        .iter()
        .map(|t| {
            rows.iter()
                .filter(|r| ids.contains(&r.segment.id) && r.segment.track == t.id)
                .count()
        })
        .collect();
    (shown.pass, per)
}

/// Drafts of `m` on `dev` that still hold their text.
fn live_drafts(dev: &Device, m: EntityId) -> usize {
    dev.svc
        .store()
        .segments(m)
        .unwrap()
        .iter()
        .filter(|r| r.pass == TranscriptPass::Live && !r.meta.deleted && r.holds_text())
        .count()
}

/// Tombstones of `m` on `dev` that still hold text.
fn text_tombstones(dev: &Device, m: EntityId) -> usize {
    dev.svc
        .store()
        .segments(m)
        .unwrap()
        .iter()
        .filter(|r| r.meta.deleted && r.holds_text())
        .count()
}

/// The peer and two updated devices joined to its vault.
fn peer_and_two(relay: &TestRelay) -> (Device, Device, Device) {
    let (p, a) = pair(relay);
    let b = Device::new("sync-replaced-b");
    let offer = offer_pairing_code(&p.svc, p.vault()).unwrap();
    join_with_code(&b.svc, &*b.vault, &relay.relay.url(), offer.code()).unwrap();
    (p, a, b)
}

/// Ticks the peer, then each updated device a row at a time until a whole round brings nothing,
/// checking after every tick that no device shows nothing for `m`.
fn settle_watching(
    peer: &SyncClient,
    others: &[(&Device, &SyncClient)],
    m: EntityId,
    tracks: &[Track],
) {
    for round in 0..200 {
        let mut busy = false;
        let r = peer.tick().unwrap();
        assert_eq!(r.held, 0, "the peer holds nothing: {r:?}");
        busy |= r.pushed > 0 || r.written > 0;
        for (dev, c) in others {
            let r = c.tick().unwrap();
            busy |= r.pushed > 0 || r.written > 0 || r.held > 0 || !r.pull_complete;
            let (pass, per) = showing(dev, m, tracks);
            assert!(
                pass.is_some() && per.iter().sum::<usize>() > 0,
                "round {round}: a device shows nothing ({pass:?}, {per:?})"
            );
        }
        if !busy {
            return;
        }
    }
    panic!("the devices did not settle");
}

/// Runs each updated device's sweep (after a quiet tick, so the sweep does not wait on it).
fn sweep_updated(devices: &[(&Device, &SyncClient)]) {
    for (dev, c) in devices {
        c.tick().unwrap();
        let sync = crate::replaced_text::StoreSync::of(&dev.svc);
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let r = crate::replaced_text::sweep(
            &dev.svc,
            &sync,
            &crate::replaced_text::RealWait::new(stop),
        )
        .unwrap();
        assert!(r.done, "{r:?}");
    }
}

/// The two-track case: the updated devices clear the text of the peer's tombstones and never its
/// draft, also in their sweeps; a re-transcription that finds nothing for one track leaves every
/// device, the peer included, showing the other track's final lines and nothing for that track
/// once the release has run, and so after further ticks; the peer applies the cleared rows and
/// sends nothing back for them.
#[test]
fn sync_replaced_a_peer_with_no_clearing_rule_two_tracks() {
    let relay = relay();
    let (p, a, b) = peer_and_two(&relay);
    let cp = p.client();
    let (ca, cb) = (
        a.client().with_pull_pages(1, 1),
        b.client().with_pull_pages(1, 1),
    );
    let (m, tracks, me) = recorded_with(&p, &[TrackId::Mic, TrackId::Reference], 71);
    cp.tick().unwrap();
    for _ in 0..40 {
        ca.tick().unwrap();
        cb.tick().unwrap();
    }
    let others = [(&a, &ca), (&b, &cb)];
    // The first pass, as 0.9.0: lines on both tracks, the drafts left live.
    p.clock.advance_ms(1_000);
    pass_as_0_9_0(&p, m, &tracks, &[0, 1], me, 72);
    settle_watching(&cp, &others, m, &tracks);
    for d in [&a, &b] {
        assert_eq!(
            live_drafts(d, m),
            6,
            "the peer's draft is never cleared elsewhere"
        );
        assert_eq!(showing(d, m, &tracks).1, vec![2, 2]);
    }
    sweep_updated(&others);
    for d in [&a, &b] {
        assert_eq!(live_drafts(d, m), 6, "nor by a sweep");
    }
    // A re-transcription that finds nothing for the reference: its lines tombstoned with text.
    p.clock.advance_ms(1_000);
    pass_as_0_9_0(&p, m, &tracks, &[0], me, 73);
    assert!(text_tombstones(&p, m) >= 4);
    settle_watching(&cp, &others, m, &tracks);
    for d in [&p, &a, &b] {
        assert_eq!(
            showing(d, m, &tracks),
            (Some(TranscriptPass::Final), vec![2, 0]),
            "the other track's lines, nothing for the emptied one"
        );
    }
    for d in [&a, &b] {
        assert_eq!(
            text_tombstones(d, m),
            0,
            "the updated devices hold no replaced text"
        );
        assert_eq!(live_drafts(d, m), 6);
    }
    // The peer took the cleared copies and sends nothing back for them.
    assert_eq!(text_tombstones(&p, m), 0);
    let queued: Vec<_> = p
        .svc
        .store()
        .outbox(10_000)
        .unwrap()
        .into_iter()
        .filter(|r| r.meeting() == Some(m))
        .collect();
    assert!(queued.is_empty(), "{queued:?}");
    // And so it stays.
    settle_watching(&cp, &others, m, &tracks);
    for d in [&p, &a, &b] {
        assert_eq!(showing(d, m, &tracks).1, vec![2, 0]);
    }
}

/// The one-track case: a re-transcription that finds nothing tombstones every line; after the
/// release every device shows the meeting's draft, never nothing.
#[test]
fn sync_replaced_a_peer_with_no_clearing_rule_one_track() {
    let relay = relay();
    let (p, a, b) = peer_and_two(&relay);
    let cp = p.client();
    let (ca, cb) = (
        a.client().with_pull_pages(1, 1),
        b.client().with_pull_pages(1, 1),
    );
    let (m, tracks, me) = recorded_with(&p, &[TrackId::Mic], 81);
    cp.tick().unwrap();
    for _ in 0..40 {
        ca.tick().unwrap();
        cb.tick().unwrap();
    }
    let others = [(&a, &ca), (&b, &cb)];
    p.clock.advance_ms(1_000);
    pass_as_0_9_0(&p, m, &tracks, &[0], me, 82);
    settle_watching(&cp, &others, m, &tracks);
    p.clock.advance_ms(1_000);
    pass_as_0_9_0(&p, m, &tracks, &[], me, 83);
    settle_watching(&cp, &others, m, &tracks);
    for d in [&p, &a, &b] {
        assert_eq!(
            showing(d, m, &tracks),
            (Some(TranscriptPass::Live), vec![3]),
            "the draft, which nobody cleared"
        );
    }
    for d in [&a, &b] {
        assert_eq!(text_tombstones(d, m), 0);
    }
}

/// The peer's push cut between its tombstones and its lines: it does not stamp its tombstones
/// again when a line is written, so after an edit of every new line its tombstones go out first;
/// the updated devices, pulling a row at a time, show the draft or the lines, never nothing, and
/// the lines once everything is in.
#[test]
fn sync_replaced_a_peer_push_cut_between_its_tombstones_and_its_lines() {
    let relay = relay();
    let (p, a, b) = peer_and_two(&relay);
    let cp = p.client();
    let (ca, cb) = (
        a.client().with_pull_pages(1, 1),
        b.client().with_pull_pages(1, 1),
    );
    let (m, tracks, me) = recorded_with(&p, &[TrackId::Mic, TrackId::Reference], 91);
    cp.tick().unwrap();
    for _ in 0..40 {
        ca.tick().unwrap();
        cb.tick().unwrap();
    }
    let others = [(&a, &ca), (&b, &cb)];
    p.clock.advance_ms(1_000);
    pass_as_0_9_0(&p, m, &tracks, &[0, 1], me, 92);
    settle_watching(&cp, &others, m, &tracks);
    p.clock.advance_ms(1_000);
    pass_as_0_9_0(&p, m, &tracks, &[0, 1], me, 93);
    // Every new line edited before the push: lines now stamped after the tombstones.
    let new_lines: Vec<EntityId> = p
        .svc
        .store()
        .segments(m)
        .unwrap()
        .into_iter()
        .filter(|r| r.pass == TranscriptPass::Final && !r.meta.deleted)
        .map(|r| r.segment.id)
        .collect();
    for id in &new_lines {
        p.clock.advance_ms(10);
        crate::edit::edit_segment(&p.svc, m, *id, "edited final words").unwrap();
    }
    settle_watching(&cp, &others, m, &tracks);
    for d in [&a, &b] {
        assert_eq!(
            showing(d, m, &tracks),
            (Some(TranscriptPass::Final), vec![2, 2])
        );
        assert_eq!(text_tombstones(d, m), 0);
    }
}
