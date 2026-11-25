//! T12.92: a line edit and a re-transcription made on two computers before they sync leave one
//! version of the words (docs/sync-spec.md section 7 rule 4), through the real relay
//! (`LocalRelay`), because the relay arbitrates by `updated_at` alone. A child of the sync
//! tests, so it shares their relay and device helpers. Every name holds `sync`; every word is
//! invented.

use std::collections::BTreeSet;

use earshot_core::asr::{Segment, Word};
use earshot_core::capture::TrackId;
use earshot_core::identity::EntityId;
use earshot_core::model::{SegmentRow, Speaker, Stamp, TranscriptPass};
use rand_core::SeedableRng;

use super::*;
use crate::edit::{LineSpeaker, edit_segment, move_line, split_line};

/// A third computer joined to `a`'s vault.
fn third(relay: &TestRelay, a: &Device) -> Device {
    let c = Device::new("sync-c");
    let offer = offer_pairing_code(&a.svc, a.vault()).unwrap();
    join_with_code(&c.svc, &*c.vault, &relay.relay.url(), offer.code()).unwrap();
    c
}

/// Ticks every client in turn until a whole round sends, writes and repairs nothing.
fn settle(clients: &[&SyncClient]) {
    for _ in 0..12 {
        let mut busy = false;
        for c in clients {
            let r = c.tick().unwrap();
            busy |= r.pushed > 0 || r.written > 0 || r.repaired > 0 || r.held > 0;
        }
        if !busy {
            return;
        }
    }
    panic!("the computers did not settle");
}

fn track_of(dev: &Device, m: EntityId, kind: TrackId) -> EntityId {
    dev.svc
        .store()
        .tracks(m)
        .unwrap()
        .into_iter()
        .find(|t| t.kind == kind)
        .unwrap()
        .id
}

/// The live final lines of `kind`'s track, in time order.
fn lines(dev: &Device, m: EntityId, kind: TrackId) -> Vec<SegmentRow> {
    let track = track_of(dev, m, kind);
    dev.svc
        .store()
        .segments(m)
        .unwrap()
        .into_iter()
        .filter(|r| r.pass == TranscriptPass::Final && !r.meta.deleted)
        .filter(|r| r.segment.track == track)
        .collect()
}

fn line_on(dev: &Device, m: EntityId, kind: TrackId) -> EntityId {
    let found = lines(dev, m, kind);
    assert_eq!(found.len(), 1, "{found:?}");
    found[0].segment.id
}

fn speaker_live(dev: &Device, m: EntityId, id: EntityId) -> bool {
    dev.svc
        .store()
        .speakers(m)
        .unwrap()
        .iter()
        .any(|s| s.id == id && !s.meta.deleted)
}

/// What a re-transcription on `dev` writes, without models: every live final line without an
/// edit is tombstoned and decoded again under a new id, a new "Speaker N" and one new run
/// (`extra` words appended to the mic's new line); speakers no kept edited line names are
/// tombstoned, as the final pass does. Returns the run.
fn retranscribe(dev: &Device, m: EntityId, seed: u64, extra: &[Word]) -> EntityId {
    let clock = Arc::clone(&dev.svc.clock);
    let mut rng = rand_chacha::ChaCha8Rng::seed_from_u64(seed);
    let mic = track_of(dev, m, TrackId::Mic);
    let mut db = dev.svc.store();
    let (store, hlc) = db.parts();
    let mut stamp = Stamp {
        clock: &*clock,
        hlc,
        rng: &mut rng,
    };
    let run = EntityId::generate(stamp.clock, &mut *stamp.rng);
    let finals: Vec<SegmentRow> = store
        .segments(m)
        .unwrap()
        .into_iter()
        .filter(|r| r.pass == TranscriptPass::Final && !r.meta.deleted)
        .collect();
    let kept: BTreeSet<EntityId> = finals
        .iter()
        .filter(|r| r.text_edited_at.is_some())
        .filter_map(|r| r.segment.speaker_id)
        .collect();
    let speakers: Vec<Speaker> = store
        .speakers(m)
        .unwrap()
        .into_iter()
        .filter(|s| !s.meta.deleted)
        .collect();
    for s in &speakers {
        if !kept.contains(&s.id) {
            let mut gone = s.clone();
            gone.meta.mark_deleted(stamp.hlc, stamp.clock);
            store.put_speaker(gone).unwrap();
        }
    }
    for (n, old) in finals
        .into_iter()
        .filter(|r| r.text_edited_at.is_none())
        .enumerate()
    {
        let role = speakers
            .iter()
            .find(|s| Some(s.id) == old.segment.speaker_id)
            .map_or(earshot_core::diar::SpeakerRole::Remote, |s| s.role);
        let voice = Speaker::new(m, role, format!("Speaker {}", n + 1), None, &mut stamp);
        store.put_speaker(voice.clone()).unwrap();
        let mut words = old.segment.words.clone();
        if old.segment.track == mic {
            words.extend(extra.iter().cloned());
        }
        let mut fresh = Segment::from_words(
            EntityId::generate(stamp.clock, &mut *stamp.rng),
            old.segment.track,
            words,
            old.segment.start,
        );
        fresh.speaker_id = Some(voice.id);
        let mut gone = old;
        gone.meta.mark_deleted(stamp.hlc, stamp.clock);
        store.put_segment(gone).unwrap();
        let mut row = SegmentRow::new(m, TranscriptPass::Final, fresh, stamp.hlc, stamp.clock);
        row.pass_run = Some(run);
        store.put_segment(row).unwrap();
    }
    run
}

/// Both computers show the edit as the mic's one line, its speaker live, and the
/// re-transcription's line on the other track.
fn assert_edit_kept(dev: &Device, m: EntityId, text: &str, run: EntityId) {
    let mic = lines(dev, m, TrackId::Mic);
    assert_eq!(mic.len(), 1, "one version of the words: {mic:?}");
    assert_eq!(mic[0].segment.text, text);
    assert!(mic[0].text_edited_at.is_some());
    let speaker = mic[0].segment.speaker_id.unwrap();
    assert!(
        speaker_live(dev, m, speaker),
        "the edited line's speaker is live"
    );
    let other = lines(dev, m, TrackId::Reference);
    assert_eq!(other.len(), 1, "{other:?}");
    assert_eq!(other[0].pass_run, Some(run), "the far end is the new run's");
}

#[test]
fn sync_edits_an_edit_and_a_retranscription_leave_one_version_in_both_push_orders() {
    for edit_first in [true, false] {
        let relay = relay();
        let (a, b) = pair(&relay);
        let (ca, cb) = (a.client(), b.client());
        let m = meeting(&a.svc, "Invented planning", 31);
        settle(&[&ca, &cb]);
        let line = line_on(&a, m, TrackId::Mic);
        a.clock.advance_ms(1_000);
        assert!(edit_segment(&a.svc, m, line, "shall we sail on friday").unwrap());
        // The re-transcription is stamped later than the edit.
        b.clock.advance_ms(2_000);
        let run = retranscribe(&b, m, 7, &[]);
        let order: [&SyncClient; 2] = if edit_first { [&ca, &cb] } else { [&cb, &ca] };
        settle(&order);
        for dev in [&a, &b] {
            assert_edit_kept(dev, m, "shall we sail on friday", run);
        }
        assert_eq!(a.rows(), b.rows(), "edit first: {edit_first}");
    }
}

#[test]
fn sync_edits_three_computers_settle_on_the_edit() {
    let relay = relay();
    let (a, b) = pair(&relay);
    let c = third(&relay, &a);
    let (ca, cb, cc) = (a.client(), b.client(), c.client());
    let m = meeting(&a.svc, "Invented planning", 32);
    settle(&[&ca, &cb, &cc]);
    let line = line_on(&a, m, TrackId::Mic);
    a.clock.advance_ms(1_000);
    assert!(edit_segment(&a.svc, m, line, "shall we sail on friday").unwrap());
    b.clock.advance_ms(2_000);
    let run = retranscribe(&b, m, 8, &[]);
    settle(&[&cc, &cb, &ca]);
    for dev in [&a, &b, &c] {
        assert_edit_kept(dev, m, "shall we sail on friday", run);
    }
    assert_eq!(a.rows(), b.rows());
    assert_eq!(b.rows(), c.rows());
}

#[test]
fn sync_edits_a_held_edit_reaches_a_third_computer_from_the_retranscribing_one_alone() {
    let relay = relay();
    let (a, b) = pair(&relay);
    let c = third(&relay, &a);
    let (ca, cb, cc) = (a.client(), b.client(), c.client());
    let m = meeting(&a.svc, "Invented planning", 33);
    settle(&[&ca, &cb, &cc]);
    let line = line_on(&a, m, TrackId::Mic);
    a.clock.advance_ms(1_000);
    assert!(edit_segment(&a.svc, m, line, "shall we sail on friday").unwrap());
    ca.tick().unwrap();
    // The other computer is re-transcribing: the edit waits there.
    let job = b.svc.lease_meeting_for(m, MeetingJob::Processing).unwrap();
    let r = cb.tick().unwrap();
    assert_eq!(r.held, 1, "{r:?}");
    b.clock.advance_ms(2_000);
    let run = retranscribe(&b, m, 9, &[]);
    drop(job);
    // The editing computer goes offline; the third hears only from the re-transcribing one.
    settle(&[&cb, &cc]);
    for dev in [&b, &c] {
        assert_edit_kept(dev, m, "shall we sail on friday", run);
    }
    assert_eq!(b.rows(), c.rows());
}

#[test]
fn sync_edits_a_split_and_a_retranscription_keep_both_parts() {
    let relay = relay();
    let (a, b) = pair(&relay);
    let (ca, cb) = (a.client(), b.client());
    let m = meeting(&a.svc, "Invented planning", 34);
    settle(&[&ca, &cb]);
    let line = line_on(&a, m, TrackId::Mic);
    a.clock.advance_ms(1_000);
    split_line(&a.svc, m, line, 2).unwrap();
    b.clock.advance_ms(2_000);
    retranscribe(&b, m, 10, &[]);
    settle(&[&ca, &cb]);
    for dev in [&a, &b] {
        let mic = lines(dev, m, TrackId::Mic);
        let texts: Vec<&str> = mic.iter().map(|r| r.segment.text.as_str()).collect();
        assert_eq!(texts, ["shall we", "ship on friday"], "{mic:?}");
        assert!(mic.iter().all(|r| r.text_edited_at.is_some()));
        assert_eq!(mic[0].pass_run, mic[1].pass_run, "the parts share a run");
    }
    assert_eq!(a.rows(), b.rows());
}

#[test]
fn sync_edits_a_move_and_a_retranscription_keep_the_moved_line() {
    let relay = relay();
    let (a, b) = pair(&relay);
    let (ca, cb) = (a.client(), b.client());
    let m = meeting(&a.svc, "Invented planning", 35);
    settle(&[&ca, &cb]);
    // The user moves their own line to a new voice (a far-end line alone with its unnamed
    // voice has nothing to move to).
    let mine = line_on(&a, m, TrackId::Mic);
    a.clock.advance_ms(1_000);
    assert!(move_line(&a.svc, m, mine, LineSpeaker::New).unwrap());
    let moved_to = lines(&a, m, TrackId::Mic)[0].segment.speaker_id.unwrap();
    b.clock.advance_ms(2_000);
    retranscribe(&b, m, 11, &[]);
    settle(&[&ca, &cb]);
    for dev in [&a, &b] {
        let mic = lines(dev, m, TrackId::Mic);
        assert_eq!(mic.len(), 1, "{mic:?}");
        assert_eq!(mic[0].segment.id, mine, "the moved line stays");
        assert_eq!(mic[0].segment.speaker_id, Some(moved_to));
        assert_eq!(
            mic[0].segment.text, "shall we ship on friday",
            "with its words"
        );
        assert!(speaker_live(dev, m, moved_to));
    }
    assert_eq!(a.rows(), b.rows());
}

#[test]
fn sync_edits_an_edit_after_the_pass_changes_nothing_else() {
    let relay = relay();
    let (a, b) = pair(&relay);
    let (ca, cb) = (a.client(), b.client());
    let m = meeting(&a.svc, "Invented planning", 36);
    settle(&[&ca, &cb]);
    b.clock.advance_ms(1_000);
    retranscribe(&b, m, 12, &[]);
    settle(&[&ca, &cb]);
    let far_before = lines(&a, m, TrackId::Reference);
    // The pass reached both; now the user edits one of its lines.
    let line = line_on(&a, m, TrackId::Mic);
    a.clock.advance_ms(5_000);
    assert!(edit_segment(&a.svc, m, line, "shall we sail on friday").unwrap());
    let mut repaired = 0;
    for _ in 0..3 {
        repaired += ca.tick().unwrap().repaired;
        repaired += cb.tick().unwrap().repaired;
    }
    assert_eq!(repaired, 0, "no clash: nothing is repaired");
    for dev in [&a, &b] {
        assert_eq!(lines(dev, m, TrackId::Reference), far_before, "unchanged");
        let mic = lines(dev, m, TrackId::Mic);
        assert_eq!(mic.len(), 1);
        assert_eq!(mic[0].segment.text, "shall we sail on friday");
    }
    assert_eq!(a.rows(), b.rows());
}

#[test]
fn sync_edits_two_edits_and_a_tombstone_on_three_computers_keep_the_later_edit() {
    let relay = relay();
    let (a, b) = pair(&relay);
    let c = third(&relay, &a);
    let (ca, cb, cc) = (a.client(), b.client(), c.client());
    let m = meeting(&a.svc, "Invented planning", 37);
    settle(&[&ca, &cb, &cc]);
    let line = line_on(&a, m, TrackId::Mic);
    a.clock.advance_ms(1_000);
    assert!(edit_segment(&a.svc, m, line, "shall we sail on friday").unwrap());
    c.clock.advance_ms(1_500);
    assert!(edit_segment(&c.svc, m, line, "shall we sell on friday").unwrap());
    b.clock.advance_ms(2_000);
    let run = retranscribe(&b, m, 13, &[]);
    settle(&[&ca, &cb, &cc]);
    for dev in [&a, &b, &c] {
        assert_edit_kept(dev, m, "shall we sell on friday", run);
    }
    assert_eq!(a.rows(), b.rows());
    assert_eq!(b.rows(), c.rows());
    // Settled: no further writes anywhere.
    for client in [&ca, &cb, &cc] {
        let quiet = client.tick().unwrap();
        assert_eq!(
            (quiet.pushed, quiet.written, quiet.repaired),
            (0, 0, 0),
            "{quiet:?}"
        );
    }
}

#[test]
fn sync_edits_a_rewrite_and_an_edit_of_the_same_line_keep_the_edit() {
    let relay = relay();
    let (a, b) = pair(&relay);
    let (ca, cb) = (a.client(), b.client());
    let m = meeting(&a.svc, "Invented planning", 38);
    settle(&[&ca, &cb]);
    let line = line_on(&a, m, TrackId::Mic);
    a.clock.advance_ms(1_000);
    assert!(edit_segment(&a.svc, m, line, "shall we sail on friday").unwrap());
    // The re-transcription's mic line runs past the edited one.
    b.clock.advance_ms(2_000);
    retranscribe(&b, m, 14, &[Word::new("please", 3_500, 3_800, 0.9)]);
    ca.tick().unwrap();
    cb.tick().unwrap();
    ca.tick().unwrap();
    // The editing computer now holds the new line rewritten to the word outside its edit.
    let rewritten: Vec<String> = lines(&a, m, TrackId::Mic)
        .iter()
        .map(|r| r.segment.text.clone())
        .collect();
    assert_eq!(rewritten, ["shall we sail on friday", "please"]);
    // Before hearing of the edit (its own push replaced it at the relay, so it comes back only
    // once the editing computer stamps it again), the re-transcribing computer edits its new
    // line.
    let new_line = lines(&b, m, TrackId::Mic)
        .into_iter()
        .find(|r| r.text_edited_at.is_none())
        .unwrap()
        .segment
        .id;
    assert!(edit_segment(&b.svc, m, new_line, "shall we ship on friday, please").unwrap());
    settle(&[&ca, &cb]);
    for dev in [&a, &b] {
        let texts: Vec<String> = lines(dev, m, TrackId::Mic)
            .iter()
            .map(|r| r.segment.text.clone())
            .collect();
        assert_eq!(
            texts,
            ["shall we sail on friday", "shall we ship on friday, please"],
            "both edits stay"
        );
    }
    assert_eq!(a.rows(), b.rows());
}

/// Review advisory 1: an edit on one computer and a deletion of the meeting on another. The
/// deletion's rows reach the editing computer one per tick (its lines' erasures before the
/// meeting row), and at no point does it hold transcript text of the meeting to send; both
/// computers end with every line erased.
#[test]
fn sync_edits_a_deletion_split_over_pull_pages_never_sends_an_edit_back() {
    let relay = relay();
    let (a, b) = pair(&relay);
    let (ca, cb) = (a.client(), b.client());
    let m = meeting(&a.svc, "Invented planning", 39);
    settle(&[&ca, &cb]);
    let line = line_on(&a, m, TrackId::Mic);
    a.clock.advance_ms(1_000);
    assert!(edit_segment(&a.svc, m, line, "shall we sail on friday").unwrap());
    ca.tick().unwrap();
    b.clock.advance_ms(2_000);
    b.svc.delete_meeting(m).unwrap();
    cb.tick().unwrap();
    // The editing computer pulls one row per tick.
    let slow = a.client().with_pull_pages(1, 1);
    let text_queued = |dev: &Device| {
        dev.svc
            .store()
            .outbox(10_000)
            .unwrap()
            .iter()
            .any(|r| matches!(r, earshot_store::SyncedRow::Segment(s) if s.meeting_id == m && !s.segment.text.is_empty()))
    };
    for round in 0..40 {
        assert!(
            !text_queued(&a),
            "round {round}: transcript text queued after the deletion"
        );
        slow.tick().unwrap();
        if a.title(m).is_none() {
            break;
        }
    }
    assert!(!text_queued(&a));
    assert_eq!(
        a.title(m),
        None,
        "the deletion reached the editing computer"
    );
    settle(&[&ca, &cb]);
    for dev in [&a, &b] {
        for r in dev.svc.store().segments(m).unwrap() {
            assert!(r.is_erasure(), "{r:?}");
        }
    }
    assert_eq!(a.rows(), b.rows());
}
