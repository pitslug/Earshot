//! Moving a transcript line to another speaker and splitting a line (T12.90). Every name
//! contains `edit`; every word is invented.

use earshot_core::diar::{Embedding, EmbeddingModelId};
use earshot_core::voiceprint::{SampleOrigin, enroll};

use super::*;
use crate::post::PostWarning;

fn row_of(svc: &Services, meeting: EntityId, id: EntityId) -> SegmentRow {
    svc.store()
        .segments(meeting)
        .unwrap()
        .into_iter()
        .find(|r| r.segment.id == id)
        .unwrap()
}

fn line_with(svc: &Services, meeting: EntityId, text: &str) -> Segment {
    view(svc, meeting)
        .segments
        .into_iter()
        .find(|s| s.text == text)
        .unwrap_or_else(|| panic!("no line {text}"))
}

/// Rewrites line `id` as the recogniser would have decoded `text` (one word per token, the
/// line's times kept, probabilities 0.5, 0.6, ...), with no edit mark.
fn decoded_words(svc: &Services, meeting: EntityId, id: EntityId, text: &str) {
    let clock = Arc::clone(&svc.clock);
    let mut db = svc.store();
    let mut row = db
        .segments(meeting)
        .unwrap()
        .into_iter()
        .find(|r| r.segment.id == id)
        .unwrap();
    let start = row.segment.start.as_millis();
    let words: Vec<Word> = text
        .split_whitespace()
        .enumerate()
        .map(|(i, w)| {
            let at = start + 400 * i as u64;
            let p = 0.5 + 0.1 * f32::from(u8::try_from(i).unwrap());
            Word::new(w, at, at + 300, p)
        })
        .collect();
    let (end, speaker_id) = (row.segment.end, row.segment.speaker_id);
    row.segment = Segment {
        end,
        speaker_id,
        ..Segment::from_words(id, row.segment.track, words, row.segment.start)
    };
    let (store, hlc) = db.parts();
    row.meta.touch(hlc, &*clock);
    store.put_segment(row).unwrap();
}

fn make_draft(svc: &Services, meeting: EntityId) {
    let clock = Arc::clone(&svc.clock);
    let mut db = svc.store();
    let rows = db.segments(meeting).unwrap();
    let (store, hlc) = db.parts();
    for mut r in rows {
        r.pass = TranscriptPass::Live;
        r.meta.touch(hlc, &*clock);
        store.put_segment(r).unwrap();
    }
}

#[test]
fn edit_move_line_to_a_speaker_of_the_same_track_keeps_the_line_and_marks_it_edited() {
    let dir = TempDir::new("edit");
    let (svc, _) = services(&dir);
    let m = meeting(&svc, "Release", 1200);
    let bob = add_speaker(
        &svc,
        m,
        TrackId::Reference,
        "Bob",
        "and the notes",
        9_000,
        1201,
    );
    let line = line_with(&svc, m, "yes friday works for the release");
    let before = row_of(&svc, m, line.id);
    assert_eq!(movable(&svc, m, &line), [bob]);

    assert!(move_line(&svc, m, line.id, LineSpeaker::Existing(bob)).unwrap());
    let row = row_of(&svc, m, line.id);
    assert_eq!(row.segment.speaker_id, Some(bob));
    assert_eq!(
        (
            &row.segment.text,
            &row.segment.words,
            row.segment.start,
            row.segment.end
        ),
        (
            &before.segment.text,
            &before.segment.words,
            before.segment.start,
            before.segment.end
        ),
        "text, words and times stay"
    );
    assert!(row.meta.updated_at > before.meta.updated_at);
    assert_eq!(row.text_edited_at, Some(row.meta.updated_at));
    assert!(lines(&svc, m).contains(&("Bob".into(), "yes friday works for the release".into())));
    // Its own speaker writes nothing.
    assert!(!move_line(&svc, m, line.id, LineSpeaker::Existing(bob)).unwrap());
    assert_eq!(row_of(&svc, m, line.id).meta, row.meta);
    // Alice keeps her row, though she has no line left (the detail lists only speakers
    // with lines).
    let alice = svc
        .store()
        .speakers(m)
        .unwrap()
        .into_iter()
        .find(|s| s.display_name == "Alice")
        .unwrap();
    assert!(!alice.meta.deleted);
    // She is still offered on the far end by her role, so the move can be undone...
    assert_eq!(
        movable(&svc, m, &line_with(&svc, m, "and the notes")),
        [alice.id]
    );
    assert_eq!(
        movable(&svc, m, &row_of(&svc, m, line.id).segment),
        [alice.id]
    );
    // ...but not on the mic.
    let me = speaker(&svc, m, "Me").id;
    let mic = line_with(&svc, m, "shall we ship on friday");
    assert!(!movable(&svc, m, &mic).contains(&alice.id));
    assert_eq!(
        refused(move_line(&svc, m, mic.id, LineSpeaker::Existing(alice.id))),
        EditError::SpeakerNotOnTrack(alice.id)
    );
    assert!(move_line(&svc, m, line.id, LineSpeaker::Existing(alice.id)).unwrap());
    assert!(lines(&svc, m).contains(&("Alice".into(), "yes friday works for the release".into())));
    // Me with no line left is offered on the mic only.
    let mic_speaker = add_speaker(&svc, m, TrackId::Mic, "Sam", "over here", 12_000, 1202);
    assert!(move_line(&svc, m, mic.id, LineSpeaker::Existing(mic_speaker)).unwrap());
    let sam_line = line_with(&svc, m, "over here");
    assert_eq!(movable(&svc, m, &sam_line), [me]);
    assert!(!movable(&svc, m, &line_with(&svc, m, "and the notes")).contains(&me));
}

#[test]
fn edit_movable_by_track_is_every_lines_offer_without_its_own_speaker() {
    // T12.90a: the detail sends one list per track; a line's menu leaves its own speaker out.
    // That gives every line exactly `movable_speakers`, before and after lines move away.
    let dir = TempDir::new("edit");
    let (svc, _) = services(&dir);
    let m = meeting(&svc, "Release", 1290);
    let bob = add_speaker(
        &svc,
        m,
        TrackId::Reference,
        "Bob",
        "and the notes",
        9_000,
        1291,
    );
    add_speaker(&svc, m, TrackId::Mic, "Sam", "over here", 12_000, 1292);
    let check = |at: &str| {
        let v = view(&svc, m);
        let by_track = movable_by_track(&v.segments, &v.speakers, &v.tracks);
        let live_tracks: Vec<EntityId> = v
            .tracks
            .iter()
            .filter(|t| !t.meta.deleted)
            .map(|t| t.id)
            .collect();
        assert!(
            live_tracks.iter().all(|t| by_track.contains_key(t)),
            "{at}: a list for every live track"
        );
        for line in &v.segments {
            let shown: Vec<EntityId> = by_track[&line.track]
                .iter()
                .copied()
                .filter(|id| Some(*id) != line.speaker_id)
                .collect();
            assert_eq!(
                shown,
                movable_speakers(&v.segments, &v.speakers, &v.tracks, line),
                "{at}: {}",
                line.text
            );
        }
    };
    check("at the start");
    let line = line_with(&svc, m, "yes friday works for the release");
    assert!(move_line(&svc, m, line.id, LineSpeaker::Existing(bob)).unwrap());
    check("after a move leaves Alice with no line");
    assert!(move_line(&svc, m, line.id, LineSpeaker::New).unwrap());
    check("after a new speaker");
}

/// What the menu of `line` offers, by the detail's call of the rule.
fn movable(svc: &Services, meeting: EntityId, line: &Segment) -> Vec<EntityId> {
    let v = view(svc, meeting);
    movable_speakers(&v.segments, &v.speakers, &v.tracks, line)
}

#[test]
fn edit_move_line_in_the_room_offers_a_speaker_left_with_no_line_by_its_room_role() {
    let dir = TempDir::new("edit");
    let (svc, _) = services(&dir);
    let m = meeting(&svc, "Room", 1205);
    let two = add_speaker(
        &svc,
        m,
        TrackId::Mic,
        "Speaker 2",
        "over here",
        12_000,
        1206,
    );
    // An in-room meeting: the far-end track goes, its line and speaker with it, and the mic's
    // voices are voices of the room.
    {
        let clock = Arc::clone(&svc.clock);
        let mut db = svc.store();
        let rows = db.segments(m).unwrap();
        let tracks = db.tracks(m).unwrap();
        let speakers = db.speakers(m).unwrap();
        let (store, hlc) = db.parts();
        let far = tracks
            .iter()
            .find(|t| t.kind == TrackId::Reference)
            .unwrap();
        for mut r in rows.into_iter().filter(|r| r.segment.track == far.id) {
            r.meta.mark_deleted(hlc, &*clock);
            store.put_segment(r).unwrap();
        }
        let mut t = far.clone();
        t.meta.mark_deleted(hlc, &*clock);
        store.put_track(t).unwrap();
        for mut sp in speakers {
            if sp.display_name == "Alice" {
                sp.meta.mark_deleted(hlc, &*clock);
            } else {
                sp.role = SpeakerRole::Room;
                if sp.display_name == "Me" {
                    sp.display_name = "Speaker 1".into();
                }
                sp.meta.touch(hlc, &*clock);
            }
            store.put_speaker(sp).unwrap();
        }
    }
    assert!(records_in_room(&view(&svc, m).tracks));
    let line = line_with(&svc, m, "shall we ship on friday");
    let one = row_of(&svc, m, line.id).segment.speaker_id.unwrap();
    assert!(move_line(&svc, m, line.id, LineSpeaker::Existing(two)).unwrap());
    // Speaker 1 has no line left: offered back by its room role, and the move undone.
    assert_eq!(movable(&svc, m, &row_of(&svc, m, line.id).segment), [one]);
    assert!(move_line(&svc, m, line.id, LineSpeaker::Existing(one)).unwrap());
    // A new speaker of the room is a room voice.
    let other = line_with(&svc, m, "over here");
    assert!(move_line(&svc, m, line.id, LineSpeaker::Existing(two)).unwrap());
    assert!(move_line(&svc, m, other.id, LineSpeaker::New).unwrap());
    assert_eq!(speaker(&svc, m, "Speaker 3").role, SpeakerRole::Room);
}

#[test]
fn edit_move_line_refuses_a_speaker_of_the_other_track_and_unknown_speakers() {
    let dir = TempDir::new("edit");
    let (svc, _) = services(&dir);
    let m = meeting(&svc, "Release", 1210);
    let me = speaker(&svc, m, "Me").id;
    let alice = speaker(&svc, m, "Alice").id;
    let far = line_with(&svc, m, "yes friday works for the release");
    let mic = line_with(&svc, m, "shall we ship on friday");
    let before = row_of(&svc, m, far.id);
    assert_eq!(
        refused(move_line(&svc, m, far.id, LineSpeaker::Existing(me))),
        EditError::SpeakerNotOnTrack(me)
    );
    assert_eq!(
        refused(move_line(&svc, m, mic.id, LineSpeaker::Existing(alice))),
        EditError::SpeakerNotOnTrack(alice)
    );
    // Each speaker's only line is on the other track.
    assert!(movable(&svc, m, &far).is_empty());
    assert!(movable(&svc, m, &mic).is_empty());
    let nobody = EntityId::from_bytes([9; 16]);
    assert_eq!(
        refused(move_line(&svc, m, far.id, LineSpeaker::Existing(nobody))),
        EditError::SpeakerNotFound(nobody)
    );
    let other = meeting(&svc, "Other", 1211);
    let theirs = speaker(&svc, other, "Alice").id;
    assert_eq!(
        refused(move_line(&svc, m, far.id, LineSpeaker::Existing(theirs))),
        EditError::SpeakerNotFound(theirs)
    );
    assert_eq!(row_of(&svc, m, far.id), before, "nothing written");
}

#[test]
fn edit_move_line_to_a_new_speaker_takes_the_next_free_name_and_the_tracks_role() {
    let dir = TempDir::new("edit");
    let (svc, _) = services(&dir);
    let m = meeting(&svc, "Release", 1220);
    let one = add_speaker(
        &svc,
        m,
        TrackId::Reference,
        "Speaker 1",
        "and the notes",
        9_000,
        1221,
    );
    // Speaker 1 has two lines once the far line joins it.
    let far = line_with(&svc, m, "yes friday works for the release");
    assert!(move_line(&svc, m, far.id, LineSpeaker::Existing(one)).unwrap());
    assert!(move_line(&svc, m, far.id, LineSpeaker::New).unwrap());
    let new = speaker(&svc, m, "Speaker 2");
    assert_eq!(new.role, SpeakerRole::Remote, "its speaker's role");
    assert_eq!(new.person_id, None);
    assert_eq!(row_of(&svc, m, far.id).segment.speaker_id, Some(new.id));

    // A line moved off "Me" never makes a second "Me": a mic voice of a call is local. "Me"
    // is the user, never a voice of its own, so its only line moves too.
    let mic = line_with(&svc, m, "shall we ship on friday");
    assert!(move_line(&svc, m, mic.id, LineSpeaker::New).unwrap());
    let local = speaker(&svc, m, "Speaker 3");
    assert_eq!(local.role, SpeakerRole::Local);
    let mes = svc
        .store()
        .speakers(m)
        .unwrap()
        .into_iter()
        .filter(|s| s.role == SpeakerRole::Me && !s.meta.deleted)
        .count();
    assert_eq!(mes, 1);
    // The role rule for every case.
    assert_eq!(
        new_speaker_role(None, true, TrackId::Mic),
        SpeakerRole::Room
    );
    assert_eq!(
        new_speaker_role(Some(SpeakerRole::Me), true, TrackId::Mic),
        SpeakerRole::Room
    );
    assert_eq!(
        new_speaker_role(None, false, TrackId::Reference),
        SpeakerRole::Remote
    );
    assert_eq!(
        new_speaker_role(Some(SpeakerRole::Me), false, TrackId::Mic),
        SpeakerRole::Local
    );
    assert_eq!(
        new_speaker_role(Some(SpeakerRole::Room), true, TrackId::Mic),
        SpeakerRole::Room
    );
}

#[test]
fn edit_move_line_to_a_new_speaker_twice_makes_one_speaker() {
    let dir = TempDir::new("edit");
    let (svc, _) = services(&dir);
    let m = meeting(&svc, "Release", 1225);
    let bob = add_speaker(
        &svc,
        m,
        TrackId::Reference,
        "Bob",
        "and the notes",
        9_000,
        1226,
    );
    let far = line_with(&svc, m, "yes friday works for the release");
    assert!(move_line(&svc, m, far.id, LineSpeaker::Existing(bob)).unwrap());
    let count = |svc: &Services| svc.store().speakers(m).unwrap().len();
    let before = count(&svc);
    assert!(move_line(&svc, m, far.id, LineSpeaker::New).unwrap());
    assert_eq!(count(&svc), before + 1);
    let row = row_of(&svc, m, far.id);
    // The line is already the only line of an unnamed speaker: a second New writes nothing.
    assert!(!move_line(&svc, m, far.id, LineSpeaker::New).unwrap());
    assert_eq!(count(&svc), before + 1);
    assert_eq!(row_of(&svc, m, far.id), row);
    // A speaker with a person is not "a speaker of its own": New makes one.
    let person = new_person(&svc, "Quillan", false, 1227);
    force_link(&svc, m, row.segment.speaker_id.unwrap(), person);
    assert!(move_line(&svc, m, far.id, LineSpeaker::New).unwrap());
    assert_eq!(count(&svc), before + 2);
}

#[test]
fn edit_move_line_gives_a_speakerless_line_a_speaker() {
    let dir = TempDir::new("edit");
    let (svc, _) = services(&dir);
    let m = meeting(&svc, "Release", 1230);
    let bob = add_speaker(
        &svc,
        m,
        TrackId::Reference,
        "Bob",
        "and the notes",
        9_000,
        1231,
    );
    let far = line_with(&svc, m, "yes friday works for the release");
    {
        let mut db = svc.store();
        let mut row = db
            .segments(m)
            .unwrap()
            .into_iter()
            .find(|r| r.segment.id == far.id)
            .unwrap();
        row.segment.speaker_id = None;
        db.put_segment(row).unwrap();
    }
    let v = view(&svc, m);
    let line = v.segments.iter().find(|s| s.id == far.id).unwrap();
    // Bob by his line there, Alice (no line left) by her far-end role.
    let alice = speaker(&svc, m, "Alice").id;
    assert_eq!(
        movable_speakers(&v.segments, &v.speakers, &v.tracks, line),
        [alice, bob]
    );
    assert!(move_line(&svc, m, far.id, LineSpeaker::Existing(bob)).unwrap());
    let row = row_of(&svc, m, far.id);
    assert_eq!(row.segment.speaker_id, Some(bob));
    assert!(row.text_edited_at.is_some());
}

#[test]
fn edit_move_and_split_refuse_a_draft_and_a_busy_meeting() {
    let dir = TempDir::new("edit");
    let (svc, _) = services(&dir);
    let m = meeting(&svc, "Release", 1240);
    let bob = add_speaker(
        &svc,
        m,
        TrackId::Reference,
        "Bob",
        "and the notes",
        9_000,
        1241,
    );
    let far = line_with(&svc, m, "yes friday works for the release");
    for job in [
        MeetingJob::Processing,
        MeetingJob::Recording,
        MeetingJob::Waiting,
    ] {
        let lease = svc.lease_meeting_for(m, job).unwrap();
        let before = svc.store().segments(m).unwrap();
        for r in [
            move_line(&svc, m, far.id, LineSpeaker::Existing(bob)).map(|_| ()),
            move_line(&svc, m, far.id, LineSpeaker::New).map(|_| ()),
            split_line(&svc, m, far.id, 2).map(|_| ()),
        ] {
            assert!(
                matches!(r, Err(RuntimeError::MeetingBusy(x)) if x == m),
                "{job:?}: {r:?}"
            );
        }
        assert_eq!(svc.store().segments(m).unwrap(), before, "{job:?}");
        drop(lease);
    }

    let draft = meeting(&svc, "Draft", 1242);
    make_draft(&svc, draft);
    let live = view(&svc, draft).segments[1].id;
    assert_eq!(
        refused(move_line(&svc, draft, live, LineSpeaker::New)),
        EditError::DraftTranscript(draft)
    );
    assert_eq!(
        refused(split_line(&svc, draft, live, 1)),
        EditError::DraftTranscript(draft)
    );
    let speakers = svc.store().speakers(draft).unwrap().len();
    assert_eq!(speakers, 2, "a refused New makes no speaker");
}

#[test]
fn edit_moved_line_survives_a_retranscription_with_its_speaker() {
    let dir = TempDir::new("edit");
    let (svc, _) = services(&dir);
    let m = processed(&svc, 1250);
    // The fakes decode "first" and "second" on the far end, each its own voice.
    let first = line_with(&svc, m, "first");
    let second = line_with(&svc, m, "second");
    let to = first.speaker_id.unwrap();
    assert_ne!(second.speaker_id, Some(to));
    assert!(move_line(&svc, m, second.id, LineSpeaker::Existing(to)).unwrap());

    let cancel = CancelToken::new();
    let again = retranscribe_with(&svc, m, &cancel, fake_post, &mut |_| {}).unwrap();
    let HostPost::Done(outcome) = &again.post else {
        panic!("{:?}", again.post);
    };
    assert!(
        outcome
            .warnings
            .iter()
            .any(|w| matches!(w, PostWarning::EditedLinesKept { lines: 1, .. })),
        "{:?}",
        outcome.warnings
    );
    let v = view(&svc, m);
    let texts: Vec<&str> = v.segments.iter().map(|s| s.text.as_str()).collect();
    assert_eq!(texts, ["me", "first", "second"]);
    let kept = v.segments.iter().find(|s| s.id == second.id).unwrap();
    assert_eq!(
        kept.speaker_id,
        Some(to),
        "kept with the speaker it was moved to"
    );
    assert_eq!(kept.words, second.words, "its words stay");
    assert!(v.speakers.iter().any(|s| s.id == to));
}

#[test]
fn edit_move_line_changes_no_voice_sample_voiceprint_or_identity_entry() {
    let dir = TempDir::new("edit");
    let (svc, _) = services(&dir);
    let m = processed(&svc, 1260);
    let first = line_with(&svc, m, "first");
    let second = line_with(&svc, m, "second");
    let from = second.speaker_id.unwrap();
    let to = first.speaker_id.unwrap();
    let rowan = link_speaker(
        &svc,
        m,
        from,
        &PersonChoice::New {
            name: "Rowan".into(),
        },
    )
    .unwrap()
    .person;
    let model = EmbeddingModelId::new("level@1");
    write(&svc, |tx, stamp| {
        enroll(
            tx,
            rowan,
            &[Embedding::new(
                model.clone(),
                (0..8).map(|i| if i == 1 { 0.9 } else { 0.05 }).collect(),
            )],
            Duration::from_secs(20),
            SampleOrigin::Meeting {
                meeting: m,
                speaker: from,
            },
            &EnrolmentConfig::default(),
            stamp,
        )
        .map_err(voiceprint_err)?;
        Ok(())
    })
    .unwrap();
    let snapshot = |svc: &Services| {
        let db = svc.store();
        (
            db.samples(&model).unwrap(),
            db.voiceprints(&model).unwrap(),
            db.local_value(record_key(m).as_str()).unwrap(),
        )
    };
    let before = snapshot(&svc);
    assert_eq!(before.0.len(), 1);

    assert!(move_line(&svc, m, second.id, LineSpeaker::Existing(to)).unwrap());
    assert!(move_line(&svc, m, first.id, LineSpeaker::New).unwrap());
    assert_eq!(snapshot(&svc), before, "no biometric row or entry changed");
    // The speaker left with no line keeps its row and its person.
    let left = svc
        .store()
        .speakers(m)
        .unwrap()
        .into_iter()
        .find(|s| s.id == from)
        .unwrap();
    assert!(!left.meta.deleted);
    assert_eq!(left.person_id, Some(rowan));
}

/// T12.92: a split's second part is of the first part's final-pass run.
#[test]
fn edit_split_line_second_part_is_of_the_first_parts_run() {
    let dir = TempDir::new("edit");
    let (svc, _) = services(&dir);
    let m = meeting(&svc, "Release", 1276);
    let line = line_with(&svc, m, "yes friday works for the release");
    let run = EntityId::from_bytes([0x5A; 16]);
    {
        let clock = Arc::clone(&svc.clock);
        let mut db = svc.store();
        let mut row = db
            .segments(m)
            .unwrap()
            .into_iter()
            .find(|r| r.segment.id == line.id)
            .unwrap();
        row.pass_run = Some(run);
        let (store, hlc) = db.parts();
        row.meta.touch(hlc, &*clock);
        store.put_segment(row).unwrap();
    }
    let second = split_line(&svc, m, line.id, 2).unwrap();
    assert_eq!(row_of(&svc, m, line.id).pass_run, Some(run));
    assert_eq!(row_of(&svc, m, second).pass_run, Some(run));
}

#[test]
fn edit_split_line_divides_words_times_and_confidence_and_keeps_the_first_id() {
    let dir = TempDir::new("edit");
    let (svc, _) = services(&dir);
    let m = meeting(&svc, "Release", 1270);
    let line = line_with(&svc, m, "yes friday works for the release");
    decoded_words(&svc, m, line.id, "yes friday,  works for the release.");
    let line = row_of(&svc, m, line.id).segment;
    assert_eq!(line.text, "yes friday, works for the release.");
    assert!(can_split_line(&line));
    let summary = store(&svc, m, "One take.", 1271);
    let original_end = line.end;

    let second = split_line(&svc, m, line.id, 2).unwrap();
    assert_ne!(second, line.id);
    let a = row_of(&svc, m, line.id);
    let b = row_of(&svc, m, second);
    assert_eq!(a.segment.text, "yes friday,");
    assert_eq!(b.segment.text, "works for the release.");
    assert_eq!(a.segment.words, line.words[..2]);
    assert_eq!(b.segment.words, line.words[2..]);
    assert_eq!(a.segment.start, line.start);
    // The parts meet where the second part's first word starts, so they cover the line.
    assert!(line.words[1].end < line.words[2].start);
    assert_eq!(a.segment.end, line.words[2].start);
    assert_eq!(b.segment.start, line.words[2].start);
    assert_eq!(b.segment.end, original_end);
    assert!(
        (a.segment.confidence - 0.55).abs() < 1e-6,
        "{}",
        a.segment.confidence
    );
    assert!(
        (b.segment.confidence - 0.85).abs() < 1e-6,
        "{}",
        b.segment.confidence
    );
    for r in [&a, &b] {
        assert_eq!(
            (r.segment.track, r.segment.speaker_id, r.pass),
            (line.track, line.speaker_id, TranscriptPass::Final)
        );
        assert_eq!(r.text_edited_at, Some(r.meta.updated_at));
    }
    let v = view(&svc, m);
    let texts: Vec<&str> = v.segments.iter().map(|s| s.text.as_str()).collect();
    assert_eq!(
        texts,
        [
            "shall we ship on friday",
            "yes friday,",
            "works for the release."
        ],
        "in order of start"
    );
    // The summary still finds what it cited, and reads as out of date.
    assert_eq!(v.summary.as_ref().unwrap().id, summary);
    assert!(v.summary_out_of_date());
}

#[test]
fn edit_split_line_parts_of_untimed_words_still_cover_the_line_for_a_rerun() {
    let dir = TempDir::new("edit");
    let (svc, _) = services(&dir);
    let m = meeting(&svc, "Release", 1275);
    let line = line_with(&svc, m, "yes friday works for the release");
    // Every word untimed at the line's start (start == end), as a decoder can leave them.
    {
        let clock = Arc::clone(&svc.clock);
        let mut db = svc.store();
        let mut row = db
            .segments(m)
            .unwrap()
            .into_iter()
            .find(|r| r.segment.id == line.id)
            .unwrap();
        let at = row.segment.start.as_millis();
        for w in &mut row.segment.words {
            *w = Word::new(w.text.clone(), at, at, 0.9);
        }
        let (store, hlc) = db.parts();
        row.meta.touch(hlc, &*clock);
        store.put_segment(row).unwrap();
    }
    let (start, end) = (line.start, line.end);
    let second = split_line(&svc, m, line.id, 2).unwrap();
    let a = row_of(&svc, m, line.id).segment;
    let b = row_of(&svc, m, second).segment;
    let ms = Duration::from_millis(1);
    assert_eq!((a.start, a.end), (start, start + ms));
    assert_eq!((b.start, b.end), (start, end));
    // A new decode's words under either part are left out; one after the line stays.
    let new = |words: Vec<Word>| {
        let mut s = Segment::from_words(
            EntityId::from_bytes([7; 16]),
            line.track,
            words,
            MeetingTime::ZERO,
        );
        s.speaker_id = line.speaker_id;
        s
    };
    let at = start.as_millis();
    let decoded = vec![new(vec![
        Word::new("yes", at, at, 0.9),
        Word::new("works", at + 1_000, at + 1_300, 0.9),
        Word::new("later", end.as_millis() + 500, end.as_millis() + 800, 0.9),
    ])];
    let (kept, dropped) =
        crate::post::drop_words_under_edits(decoded.clone(), &[a.clone(), b.clone()]);
    assert_eq!(dropped, 2);
    assert_eq!(kept.len(), 1);
    assert_eq!(kept[0].text, "later");
    // The first part alone guards the untimed word at the line's start.
    let (_, by_first) = crate::post::drop_words_under_edits(decoded, &[a]);
    assert_eq!(by_first, 1);
}

#[test]
fn edit_split_line_refuses_a_position_outside_the_line_and_an_edited_line() {
    let dir = TempDir::new("edit");
    let (svc, _) = services(&dir);
    let m = meeting(&svc, "Release", 1280);
    let line = line_with(&svc, m, "yes friday works for the release");
    for token in [0, 6, 99] {
        assert_eq!(
            refused(split_line(&svc, m, line.id, token)),
            EditError::SplitOutOfRange(line.id),
            "{token}"
        );
    }
    let before = svc.store().segments(m).unwrap();
    assert!(edit_segment(&svc, m, line.id, "yes Friday works for the launch").unwrap());
    let edited = row_of(&svc, m, line.id).segment;
    assert!(!can_split_line(&edited));
    assert_eq!(
        refused(split_line(&svc, m, line.id, 2)),
        EditError::SplitNeedsWordTimes(line.id)
    );
    assert_eq!(svc.store().segments(m).unwrap().len(), before.len());
    // A one-word line cannot be split anywhere.
    let one = line_with(&svc, m, "shall we ship on friday");
    decoded_words(&svc, m, one.id, "hello");
    assert!(!can_split_line(&row_of(&svc, m, one.id).segment));
    assert_eq!(
        refused(split_line(&svc, m, one.id, 1)),
        EditError::SplitOutOfRange(one.id)
    );
}

#[test]
fn edit_split_line_moves_word_choices_of_later_words_to_the_new_line() {
    let dir = TempDir::new("edit");
    let (svc, _) = services(&dir);
    let m = meeting(&svc, "Planning", 1290);
    let id = line_with(&svc, m, "yes friday works for the release").id;
    decoded_words(&svc, m, id, "sorbeck asked sorbeck again");
    crate::testutil::replace_words(&svc, &["Zorbek"]).unwrap();
    assert!(keep_word(&svc, m, id, 0).unwrap());
    assert!(keep_word(&svc, m, id, 2).unwrap());
    let stored = |svc: &Services| crate::word_choices::word_choices(&svc.store(), m).unwrap();
    assert_eq!(stored(&svc).len(), 2);

    let second = split_line(&svc, m, id, 1).unwrap();
    let mut got: Vec<(EntityId, usize, String)> = stored(&svc)
        .into_iter()
        .map(|c| (c.segment, c.token, c.decoded))
        .collect();
    got.sort_by_key(|c| (c.0 != id, c.1));
    assert_eq!(
        got,
        [
            (id, 0, "sorbeck".to_owned()),
            (second, 1, "sorbeck".to_owned())
        ]
    );
    // The choices still hold where they now are: neither word is suggested again.
    let db = svc.store();
    let v = MeetingView::load(&**db, m).unwrap().unwrap();
    let marks = crate::word_choices::word_marks(&db, &v).unwrap();
    for line in [id, second] {
        assert!(
            marks
                .lines
                .get(&line)
                .is_none_or(|l| l.suggestions.is_empty()),
            "{line}"
        );
    }
}

#[test]
fn edit_split_then_move_the_second_part_fixes_an_edge_word() {
    let dir = TempDir::new("edit");
    let (svc, _) = services(&dir);
    let m = meeting(&svc, "Release", 1300);
    let bob = add_speaker(
        &svc,
        m,
        TrackId::Reference,
        "Bob",
        "and the notes",
        9_000,
        1301,
    );
    let line = line_with(&svc, m, "yes friday works for the release");
    let second = split_line(&svc, m, line.id, 5).unwrap();
    assert!(move_line(&svc, m, second, LineSpeaker::Existing(bob)).unwrap());
    let got = lines(&svc, m);
    assert!(got.contains(&("Alice".into(), "yes friday works for the".into())));
    assert!(got.contains(&("Bob".into(), "release".into())));
    // A split line's second part splits again, by its own words.
    assert_eq!(
        refused(split_line(&svc, m, second, 1)),
        EditError::SplitOutOfRange(second)
    );
    assert!(split_line(&svc, m, line.id, 1).is_ok());
}

#[test]
fn edit_split_of_an_unknown_line_keeps_both_parts_unknown() {
    let dir = TempDir::new("edit");
    let (svc, _) = services(&dir);
    let m = meeting(&svc, "Release", 1310);
    let line = line_with(&svc, m, "yes friday works for the release");
    {
        let mut db = svc.store();
        let mut row = db
            .segments(m)
            .unwrap()
            .into_iter()
            .find(|r| r.segment.id == line.id)
            .unwrap();
        row.segment.speaker_id = None;
        db.put_segment(row).unwrap();
        let ids = [line.id].into_iter().collect();
        db.set_local_value(
            crate::unknown_lines::record_key(m).as_str(),
            &crate::unknown_lines::encode(&ids).unwrap(),
        )
        .unwrap();
    }
    let second = split_line(&svc, m, line.id, 3).unwrap();
    let unknown = crate::unknown_lines::read(&svc.store(), m).unwrap();
    assert!(unknown.contains(&line.id) && unknown.contains(&second));
}
