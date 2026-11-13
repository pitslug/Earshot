//! Editing a meeting's title and transcript text (T12.16). Every name contains `edit`.

use super::*;
use crate::post::{PostWarning, drop_words_under_edits};

fn segment_row(svc: &Services, meeting: EntityId, id: EntityId) -> SegmentRow {
    svc.store()
        .segments(meeting)
        .unwrap()
        .into_iter()
        .find(|r| r.segment.id == id)
        .unwrap()
}

#[test]
fn edit_rename_meeting_sets_and_clears_the_title_and_refuses_bad_input() {
    let dir = TempDir::new("edit");
    let (svc, _) = services(&dir);
    let m = meeting(&svc, "Release", 400);
    let before = view(&svc, m).meeting;
    assert!(rename_meeting(&svc, m, "  Friday release plan ").unwrap());
    let after = view(&svc, m).meeting;
    assert_eq!(after.title, "Friday release plan");
    assert!(after.meta.updated_at > before.meta.updated_at);
    assert_eq!(
        (after.duration_ms, after.folder_id, after.tags.clone()),
        (before.duration_ms, before.folder_id, before.tags.clone()),
        "only the title changes"
    );
    // The current title writes nothing.
    assert!(!rename_meeting(&svc, m, "Friday release plan").unwrap());
    assert_eq!(view(&svc, m).meeting.meta, after.meta);
    // Empty is allowed: untitled again.
    assert!(rename_meeting(&svc, m, "   ").unwrap());
    assert_eq!(view(&svc, m).meeting.title, "");
    assert!(rename_meeting(&svc, m, &"t".repeat(MAX_TITLE_CHARS)).unwrap());
    for (bad, problem) in [
        ("a\nb", TextProblem::ControlCharacter),
        ("tab\there", TextProblem::ControlCharacter),
        (&"t".repeat(MAX_TITLE_CHARS + 1)[..], TextProblem::TooLong),
    ] {
        assert_eq!(
            refused(rename_meeting(&svc, m, bad)),
            EditError::InvalidTitle(problem)
        );
    }
    delete(&svc, m);
    assert_eq!(
        refused(rename_meeting(&svc, m, "Anything")),
        EditError::MeetingNotFound(m)
    );
}

#[test]
fn edit_segment_keeps_id_and_times_drops_words_and_search_and_export_follow() {
    let dir = TempDir::new("edit");
    let (svc, _) = services(&dir);
    let m = meeting(&svc, "Release", 401);
    let v = view(&svc, m);
    let target = v
        .segments
        .iter()
        .find(|s| s.text == "yes friday works for the release")
        .unwrap()
        .clone();
    assert!(!target.words.is_empty());
    let before = segment_row(&svc, m, target.id);
    assert_eq!(before.text_edited_at, None);

    assert!(edit_segment(&svc, m, target.id, " yes, Friday works for the launch ").unwrap());
    let row = segment_row(&svc, m, target.id);
    assert_eq!(row.segment.text, "yes, Friday works for the launch");
    assert_eq!(
        (row.segment.id, row.segment.track, row.segment.speaker_id),
        (target.id, target.track, target.speaker_id)
    );
    assert_eq!(
        (row.segment.start, row.segment.end),
        (target.start, target.end)
    );
    assert!(row.segment.words.is_empty(), "word times no longer apply");
    assert_eq!(row.pass, TranscriptPass::Final);
    assert_eq!(row.meta.created_at, before.meta.created_at);
    assert!(row.meta.updated_at > before.meta.updated_at);
    assert_eq!(row.text_edited_at, Some(row.meta.updated_at));
    let v = view(&svc, m);
    assert_eq!(v.edited.get(&target.id), row.text_edited_at.as_ref());
    assert_eq!(v.edited.len(), 1);
    assert!(lines(&svc, m).contains(&("Alice".into(), "yes, Friday works for the launch".into())));
    // The same text again writes nothing.
    assert!(!edit_segment(&svc, m, target.id, "yes, Friday works for the launch").unwrap());
    assert_eq!(segment_row(&svc, m, target.id).meta, row.meta);

    // Search and export read the edited text.
    let found = crate::search::search_meetings(&svc, "the launch").unwrap();
    assert_eq!(found.hits.len(), 1);
    assert_eq!(found.hits[0].meeting, m);
    assert!(
        crate::search::search_meetings(&svc, "for the release")
            .unwrap()
            .hits
            .is_empty()
    );
    let note = exported(&svc, &dir, m, 402);
    assert!(note.contains("Friday works for the launch"), "{note}");
    assert!(!note.contains("works for the release"), "{note}");
}

#[test]
fn edit_segment_refuses_a_draft_another_meeting_and_bad_text() {
    let dir = TempDir::new("edit");
    let (svc, _) = services(&dir);
    let m = meeting(&svc, "Release", 403);
    let id = view(&svc, m).segments[0].id;
    for (bad, problem) in [
        ("   ", TextProblem::Empty),
        ("two\nlines", TextProblem::ControlCharacter),
        (&"w".repeat(MAX_SEGMENT_CHARS + 1)[..], TextProblem::TooLong),
    ] {
        assert_eq!(
            refused(edit_segment(&svc, m, id, bad)),
            EditError::InvalidText(problem)
        );
    }
    assert!(edit_segment(&svc, m, id, &"w".repeat(MAX_SEGMENT_CHARS)).unwrap());
    let other = meeting(&svc, "Other", 404);
    let theirs = view(&svc, other).segments[0].id;
    assert_eq!(
        refused(edit_segment(&svc, m, theirs, "words")),
        EditError::SegmentNotFound(theirs)
    );

    // A meeting with only its live draft: the final pass would replace the edit.
    let draft = meeting(&svc, "Draft", 405);
    {
        let clock = Arc::clone(&svc.clock);
        let mut db = svc.store();
        let rows = db.segments(draft).unwrap();
        let (store, hlc) = db.parts();
        for mut r in rows {
            r.pass = TranscriptPass::Live;
            r.meta.touch(hlc, &*clock);
            store.put_segment(r).unwrap();
        }
    }
    let live = view(&svc, draft);
    assert_eq!(live.pass, Some(TranscriptPass::Live));
    assert_eq!(
        refused(edit_segment(&svc, draft, live.segments[0].id, "words")),
        EditError::DraftTranscript(draft)
    );
    // A tombstoned segment and a deleted meeting.
    {
        let clock = Arc::clone(&svc.clock);
        let mut db = svc.store();
        let mut row = db
            .segments(other)
            .unwrap()
            .into_iter()
            .find(|r| r.segment.id == theirs)
            .unwrap();
        let (store, hlc) = db.parts();
        row.meta.mark_deleted(hlc, &*clock);
        store.put_segment(row).unwrap();
    }
    assert_eq!(
        refused(edit_segment(&svc, other, theirs, "words")),
        EditError::SegmentNotFound(theirs)
    );
    delete(&svc, other);
    assert_eq!(
        refused(edit_segment(&svc, other, theirs, "words")),
        EditError::MeetingNotFound(other)
    );
}

#[test]
fn edit_segment_marks_an_older_summary_out_of_date_and_sends_nothing() {
    let dir = TempDir::new("edit");
    let (svc, _) = services(&dir);
    let m = meeting(&svc, "Release", 406);
    assert!(!view(&svc, m).summary_out_of_date(), "no summary");
    let first = store(&svc, m, "First take.", 407);
    assert!(!view(&svc, m).summary_out_of_date());
    // A title change is not a transcript change.
    assert!(rename_meeting(&svc, m, "Another title").unwrap());
    assert!(!view(&svc, m).summary_out_of_date());

    let id = view(&svc, m).segments[1].id;
    assert!(edit_segment(&svc, m, id, "yes Friday works").unwrap());
    let v = view(&svc, m);
    assert!(v.summary_out_of_date());
    assert_eq!(
        v.summary.as_ref().unwrap().id,
        first,
        "nothing was replaced"
    );
    assert_eq!(
        svc.store().summaries(m).unwrap().len(),
        1,
        "no summary was written by the edit"
    );
    // A summary written after the edit is current; restoring the older one is out of date
    // again (an undo keeps its creation time).
    let second = store(&svc, m, "Second take.", 408);
    assert!(!view(&svc, m).summary_out_of_date());
    assert_eq!(undo_summary(&svc, m).unwrap(), first);
    assert!(view(&svc, m).summary_out_of_date());
    assert_eq!(undo_summary(&svc, m).unwrap(), second);
    assert!(!view(&svc, m).summary_out_of_date());

    // A summary made from the text before an edit is not stored over it.
    let stale_view = view(&svc, m);
    assert!(edit_segment(&svc, m, id, "yes Friday works well").unwrap());
    let refused_store = crate::summary::store_summary(
        &svc,
        &stale_view,
        "test-model".into(),
        &summary_citing("Third take.", stale_view.segments[0].id),
        &mut ChaCha8Rng::seed_from_u64(409),
    );
    assert!(
        matches!(refused_store, Err(RuntimeError::Conflict(_))),
        "{refused_store:?}"
    );
}

#[test]
fn edit_segment_waits_while_a_summary_or_final_pass_works_on_the_meeting() {
    let dir = TempDir::new("edit");
    let (svc, _) = services(&dir);
    let m = meeting(&svc, "Release", 410);
    let id = view(&svc, m).segments[1].id;
    set_words_for_lines(&svc, &["Zorbek"]);
    for job in [
        MeetingJob::Processing,
        MeetingJob::Recording,
        MeetingJob::Waiting,
    ] {
        let lease = svc.lease_meeting_for(m, job).unwrap();
        let before = segment_row(&svc, m, id);
        // A summary run would be discarded at its store: the line edit waits instead.
        for r in [
            edit_segment(&svc, m, id, "yes Friday works").map(|_| ()),
            edit_segment_offering(&svc, m, id, "yes Friday works").map(|_| ()),
            use_word(&svc, m, id, 0, "Zorbek").map(|_| ()),
            put_back_word(&svc, m, id, 0).map(|_| ()),
        ] {
            assert!(
                matches!(r, Err(RuntimeError::MeetingBusy(x)) if x == m),
                "{job:?}: {r:?}"
            );
        }
        assert_eq!(segment_row(&svc, m, id), before, "{job:?}: nothing written");
        drop(lease);
    }
    // An export's lease does not change what is happening to the meeting.
    let export = svc.lease_meeting(m).unwrap();
    assert!(edit_segment(&svc, m, id, "yes Friday works").unwrap());
    drop(export);
    // The edit's own claim is gone afterwards: a summary run may start.
    drop(svc.lease_meeting_alone(m, MeetingJob::Processing).unwrap());
}

fn set_words_for_lines(svc: &Services, words: &[&str]) {
    crate::vocabulary::set_vocabulary(&mut svc.store(), words).unwrap();
}

#[test]
fn edit_retranscribe_keeps_edited_lines_and_warns() {
    let dir = TempDir::new("edit");
    let (svc, _) = services(&dir);
    let m = processed(&svc, 410);
    let v = view(&svc, m);
    // The fakes decode "first" on the far end at 9.0-9.3 s, every run.
    let first = v
        .segments
        .iter()
        .find(|s| s.text == "first")
        .unwrap()
        .clone();
    let speaker_before = first.speaker_id.unwrap();
    assert!(edit_segment(&svc, m, first.id, "first, corrected by hand").unwrap());
    let v = view(&svc, m);
    crate::summary::store_summary(
        &svc,
        &v,
        "test-model".into(),
        &summary_citing("A summary of the edited line.", first.id),
        &mut ChaCha8Rng::seed_from_u64(411),
    )
    .unwrap();

    let cancel = CancelToken::new();
    let again = retranscribe_with(&svc, m, &cancel, fake_post, &mut |_| {}).unwrap();
    let HostPost::Done(outcome) = &again.post else {
        panic!("{:?}", again.post);
    };
    assert!(
        outcome.warnings.contains(&PostWarning::EditedLinesKept {
            lines: 1,
            words_dropped: 1
        }),
        "{:?}",
        outcome.warnings
    );
    assert!(
        !again.summary_stale,
        "the edited line is still the one cited"
    );
    let v = view(&svc, m);
    let texts: Vec<&str> = v.segments.iter().map(|s| s.text.as_str()).collect();
    assert_eq!(texts, ["me", "first, corrected by hand", "second"]);
    let kept = v.segments.iter().find(|s| s.id == first.id).unwrap();
    assert_eq!((kept.start, kept.end), (first.start, first.end));
    assert!(v.edited.contains_key(&first.id));
    // Its speaker row is still live and named.
    let row = v.speakers.iter().find(|s| s.id == kept.speaker_id.unwrap());
    assert!(row.is_some(), "the edited line keeps a live speaker");
    assert_eq!(kept.speaker_id, Some(speaker_before));
    let names: Vec<&str> = v.speakers.iter().map(|s| s.display_name.as_str()).collect();
    let mut unique = names.clone();
    unique.sort_unstable();
    unique.dedup();
    assert_eq!(
        unique.len(),
        names.len(),
        "no two speakers share a name: {names:?}"
    );

    // A second run keeps it too.
    let third = retranscribe_with(&svc, m, &cancel, fake_post, &mut |_| {}).unwrap();
    assert!(matches!(third.post, HostPost::Done(_)));
    let texts: Vec<String> = view(&svc, m).segments.into_iter().map(|s| s.text).collect();
    assert_eq!(texts, ["me", "first, corrected by hand", "second"]);
}

#[test]
fn edit_retranscribe_counts_kept_lines_and_gives_their_speaker_this_runs_role() {
    let dir = TempDir::new("edit");
    let (svc, _) = services(&dir);
    let m = processed(&svc, 412);
    let v = view(&svc, m);
    // The fakes decode "first" on the far end at 9.0-9.3 s, every run.
    let first = v
        .segments
        .iter()
        .find(|s| s.text == "first")
        .unwrap()
        .clone();
    let speaker = first.speaker_id.unwrap();
    assert!(edit_segment(&svc, m, first.id, "first, corrected by hand").unwrap());
    // The speaker has no identity entry any more (as after an invalid centroid), so no new
    // voice continues its row, and the row carries a role an older run gave it.
    {
        let clock = Arc::clone(&svc.clock);
        let mut db = svc.store();
        db.delete_local_value(record_key(m).as_str()).unwrap();
        let mut row = db
            .speakers(m)
            .unwrap()
            .into_iter()
            .find(|s| s.id == speaker)
            .unwrap();
        assert_eq!(row.role, SpeakerRole::Remote);
        let (store, hlc) = db.parts();
        row.role = SpeakerRole::Local;
        row.meta.touch(hlc, &*clock);
        store.put_speaker(row).unwrap();
    }

    let cancel = CancelToken::new();
    let again = retranscribe_with(&svc, m, &cancel, fake_post, &mut |_| {}).unwrap();
    let HostPost::Done(outcome) = &again.post else {
        panic!("{:?}", again.post);
    };
    let v = view(&svc, m);
    let texts: Vec<&str> = v.segments.iter().map(|s| s.text.as_str()).collect();
    assert_eq!(texts, ["me", "first, corrected by hand", "second"]);
    assert_eq!(
        outcome.final_segments,
        v.segments.len(),
        "the kept edited line is part of the final transcript"
    );
    let kept = v.speakers.iter().find(|s| s.id == speaker).unwrap();
    assert_eq!(
        kept.role,
        SpeakerRole::Remote,
        "a far-end line's speaker is remote under this run's policy"
    );
}

#[test]
fn edit_retranscribe_role_change_of_a_kept_row_is_not_read_as_a_user_edit() {
    let dir = TempDir::new("edit");
    let (svc, _) = services(&dir);
    let m = processed(&svc, 414);
    let first = view(&svc, m)
        .segments
        .into_iter()
        .find(|s| s.text == "first")
        .unwrap();
    let speaker = first.speaker_id.unwrap();
    assert!(edit_segment(&svc, m, first.id, "first, corrected by hand").unwrap());
    // The row keeps an identity entry no new voice matches (another voice's centroid), and
    // a role an older run gave it; the entry reads as the pass's own naming.
    {
        let clock = Arc::clone(&svc.clock);
        let mut db = svc.store();
        let mut row = db
            .speakers(m)
            .unwrap()
            .into_iter()
            .find(|s| s.id == speaker)
            .unwrap();
        let (store, hlc) = db.parts();
        row.role = SpeakerRole::Local;
        row.meta.touch(hlc, &*clock);
        store.put_speaker(row.clone()).unwrap();
        let key = record_key(m);
        let Some(StoredIdentities::Record(mut record)) =
            store.local_value(key.as_str()).unwrap().map(|b| decode(&b))
        else {
            panic!("a record");
        };
        let entry = record
            .speakers
            .iter_mut()
            .find(|i| i.speaker == speaker)
            .unwrap();
        let n = entry.centroid.vector.len();
        let mut far = vec![0.0; n];
        far[n - 1] = 1.0;
        far[0] = -1.0;
        entry.centroid = earshot_core::diar::Embedding::new(entry.centroid.model.clone(), far);
        entry.given_stamp = Some(row.meta.updated_at);
        entry.user_edited = false;
        assert!(!entry.edited(&row));
        // Test set-up only: production writes this record through the runtime's registry.
        store
            .set_local_value(
                key.as_str(),
                &crate::identities::encode(&IdentityRecord {
                    speakers: record.speakers,
                }),
            )
            .unwrap();
    }

    let cancel = CancelToken::new();
    retranscribe_with(&svc, m, &cancel, fake_post, &mut |_| {}).unwrap();
    let row = view(&svc, m)
        .speakers
        .into_iter()
        .find(|s| s.id == speaker)
        .unwrap();
    assert_eq!(row.role, SpeakerRole::Remote, "the pass changed the role");
    let entry = identities_in(&svc.store(), m)
        .unwrap()
        .into_iter()
        .find(|i| i.speaker == speaker)
        .expect("the entry is carried");
    assert!(
        !entry.edited(&row),
        "the pass's own role change is not a user edit: {entry:?}"
    );
}

fn w(text: &str, start: u64, end: u64) -> Word {
    Word::new(text, start, end, 0.9)
}

fn seg(id: u8, track: u8, words: Vec<Word>) -> Segment {
    let mut s = Segment::from_words(
        EntityId::from_bytes([id; 16]),
        EntityId::from_bytes([track; 16]),
        words,
        MeetingTime::ZERO,
    );
    s.speaker_id = Some(EntityId::from_bytes([200; 16]));
    s
}

fn texts(segments: &[Segment]) -> Vec<&str> {
    segments.iter().map(|s| s.text.as_str()).collect()
}

#[test]
fn edit_retranscribe_rule_leaves_out_only_new_words_under_an_edited_span() {
    // The user's edited segment on track 1 covers 2.0-4.0 s.
    let mut edited = seg(1, 1, vec![w("old", 2_000, 4_000)]);
    edited.words.clear();
    edited.text = "the user's text".into();
    let edits = [edited];

    // No edits: everything stays.
    let new = vec![seg(10, 1, vec![w("a", 0, 500)])];
    assert_eq!(drop_words_under_edits(new.clone(), &[]), (new, 0));

    let new = vec![
        // Runs past the edit on both sides: only the words inside go, the id and speaker
        // stay, the span follows the words left.
        seg(
            11,
            1,
            vec![
                w("before", 1_000, 1_900),
                w("inside", 2_500, 3_000),
                w("after", 4_100, 4_500),
            ],
        ),
        // Wholly inside: dropped.
        seg(12, 1, vec![w("all", 2_100, 2_400), w("gone", 2_500, 3_900)]),
        // Straddles the edit's start or end: goes (the edit covers that audio).
        seg(13, 1, vec![w("edge", 1_800, 2_100)]),
        seg(14, 1, vec![w("edge", 3_900, 4_200)]),
        // Touching but not overlapping: stays.
        seg(
            15,
            1,
            vec![w("touch", 1_500, 2_000), w("touch", 4_000, 4_400)],
        ),
        // Untimed words: inside [start, end) go, at the end stay.
        seg(
            16,
            1,
            vec![w("untimed", 2_000, 2_000), w("at-end", 4_000, 4_000)],
        ),
        // Another track at the same time: untouched.
        seg(17, 2, vec![w("other", 2_000, 4_000)]),
    ];
    let (out, dropped) = drop_words_under_edits(new, &edits);
    assert_eq!(
        texts(&out),
        ["before after", "touch touch", "at-end", "other"]
    );
    assert_eq!(dropped, 1 + 2 + 1 + 1 + 1);
    let split = &out[0];
    assert_eq!(split.id, EntityId::from_bytes([11; 16]));
    assert_eq!(split.speaker_id, Some(EntityId::from_bytes([200; 16])));
    assert_eq!(
        (split.start.as_millis(), split.end.as_millis()),
        (1_000, 4_500)
    );

    // A new segment without words is judged by its span.
    let mut wordless = seg(18, 1, vec![]);
    wordless.start = MeetingTime::from_millis(3_000);
    wordless.end = MeetingTime::from_millis(3_500);
    let mut clear = wordless.clone();
    clear.id = EntityId::from_bytes([19; 16]);
    clear.start = MeetingTime::from_millis(5_000);
    clear.end = MeetingTime::from_millis(5_500);
    let (out, dropped) = drop_words_under_edits(vec![wordless, clear.clone()], &edits);
    assert_eq!(out, vec![clear]);
    assert_eq!(dropped, 1);

    // An edited segment with no length covers nothing.
    let mut point = seg(20, 1, vec![w("x", 3_000, 3_000)]);
    point.words.clear();
    let new = vec![seg(21, 1, vec![w("kept", 2_900, 3_100)])];
    assert_eq!(drop_words_under_edits(new.clone(), &[point]), (new, 0));
}

/// A summary with one of every editable field, citing `segment`.
fn full_summary(segment: EntityId) -> SummaryV1 {
    use earshot_core::summary::{ActionItem, NoteBullet, NoteSection};
    SummaryV1 {
        title: "Release planning".into(),
        tldr: "The release moves to Friday.".into(),
        enhanced_notes: vec![NoteSection {
            heading: "Timing".into(),
            bullets: vec![NoteBullet {
                text: "Testing needs two more days.".into(),
                source_segment_ids: vec![segment],
            }],
        }],
        decisions: vec![Decision {
            text: "Ship on Friday.".into(),
            source_segment_ids: vec![segment],
        }],
        action_items: vec![ActionItem {
            owner: "Alice".into(),
            task: "Tell the support team.".into(),
            due: Some("Thursday".into()),
            source_segment_ids: vec![segment],
        }],
        open_questions: vec!["Who signs off the notes?".into()],
        topics: vec![],
    }
}

fn store_full(svc: &Services, meeting: EntityId, seed: u64) -> EntityId {
    let v = view(svc, meeting);
    crate::summary::store_summary(
        svc,
        &v,
        "test-model".into(),
        &full_summary(v.segments[0].id),
        &mut ChaCha8Rng::seed_from_u64(seed),
    )
    .unwrap()
    .0
}

#[test]
fn edit_summary_fields_change_in_place_and_undo_brings_the_text_back() {
    let dir = TempDir::new("edit");
    let (svc, _) = services(&dir);
    let m = meeting(&svc, "Release", 420);
    let id = store_full(&svc, m, 421);
    let before = view(&svc, m).summary.unwrap();

    for (field, text) in [
        (SummaryField::Title, "Release plan"),
        (SummaryField::Tldr, "The release moves to Friday morning."),
        (SummaryField::Decision(0), "Ship on Friday morning."),
        (SummaryField::ActionOwner(0), "Bob"),
        (SummaryField::ActionTask(0), "Tell support and sales."),
        (SummaryField::ActionDue(0), "Wednesday"),
        (SummaryField::NoteHeading(0), "Dates"),
        (
            SummaryField::NoteBullet(0, 0),
            "Testing needs three more days.",
        ),
        (
            SummaryField::OpenQuestion(0),
            "Who signs off the release notes?",
        ),
    ] {
        assert!(edit_summary(&svc, m, id, field, &format!("  {text} ")).unwrap());
        // The same text again writes nothing.
        assert!(!edit_summary(&svc, m, id, field, text).unwrap());
    }
    let after = view(&svc, m).summary.unwrap();
    assert_eq!(after.id, id, "the summary keeps its id");
    assert_eq!(after.meta.created_at, before.meta.created_at);
    assert_eq!(
        (after.model.clone(), after.template),
        (before.model.clone(), before.template)
    );
    let s = &after.summary;
    assert_eq!(s.title, "Release plan");
    assert_eq!(s.tldr, "The release moves to Friday morning.");
    assert_eq!(s.decisions[0].text, "Ship on Friday morning.");
    let a = &s.action_items[0];
    assert_eq!(
        (a.owner.as_str(), a.task.as_str(), a.due.as_deref()),
        ("Bob", "Tell support and sales.", Some("Wednesday"))
    );
    assert_eq!(s.enhanced_notes[0].heading, "Dates");
    assert_eq!(
        s.enhanced_notes[0].bullets[0].text,
        "Testing needs three more days."
    );
    assert_eq!(s.open_questions[0], "Who signs off the release notes?");
    // Citations are untouched.
    assert_eq!(
        s.decisions[0].source_segment_ids,
        before.summary.decisions[0].source_segment_ids
    );
    // An empty due date removes it.
    assert!(edit_summary(&svc, m, id, SummaryField::ActionDue(0), " ").unwrap());
    assert_eq!(
        view(&svc, m).summary.unwrap().summary.action_items[0].due,
        None
    );

    // Export and search follow the edited summary.
    let note = exported(&svc, &dir, m, 422);
    assert!(note.contains("Ship on Friday morning."), "{note}");
    assert_eq!(
        crate::search::search_meetings(&svc, "release notes")
            .unwrap()
            .hits
            .len(),
        1
    );

    // Undo brings back the text before the last edit, and again the edit.
    let h = summary_history(&svc, m).unwrap();
    let prev = h.previous.unwrap();
    assert!(prev.restorable);
    assert_eq!(
        prev.summary.summary.action_items[0].due.as_deref(),
        Some("Wednesday")
    );
    assert_eq!(prev.summary.meta.created_at, before.meta.created_at);
    let restored = undo_summary(&svc, m).unwrap();
    assert_ne!(restored, id);
    let s = view(&svc, m).summary.unwrap();
    assert_eq!(s.summary.action_items[0].due.as_deref(), Some("Wednesday"));
    assert_eq!(s.summary.decisions[0].text, "Ship on Friday morning.");
    assert_eq!(undo_summary(&svc, m).unwrap(), id);
    assert_eq!(
        view(&svc, m).summary.unwrap().summary.action_items[0].due,
        None
    );
}

#[test]
fn edit_summary_refuses_a_replaced_summary_a_missing_item_and_bad_text() {
    let dir = TempDir::new("edit");
    let (svc, _) = services(&dir);
    let m = meeting(&svc, "Release", 423);
    let unknown = EntityId::from_bytes([5; 16]);
    assert_eq!(
        refused(edit_summary(&svc, m, unknown, SummaryField::Tldr, "Words.")),
        EditError::SummaryNotCurrent(unknown)
    );
    let old = store_full(&svc, m, 424);
    let new = store_full(&svc, m, 425);
    assert_eq!(
        refused(edit_summary(&svc, m, old, SummaryField::Tldr, "Words.")),
        EditError::SummaryNotCurrent(old)
    );
    for field in [
        SummaryField::Decision(1),
        SummaryField::ActionOwner(1),
        SummaryField::ActionTask(1),
        SummaryField::ActionDue(1),
        SummaryField::NoteHeading(1),
        SummaryField::NoteBullet(0, 1),
        SummaryField::NoteBullet(1, 0),
        SummaryField::OpenQuestion(1),
    ] {
        assert_eq!(
            refused(edit_summary(&svc, m, new, field, "Words.")),
            EditError::SummaryItemNotFound,
            "{field:?}"
        );
    }
    let too_long = "w".repeat(MAX_SUMMARY_TEXT_CHARS + 1);
    let long_title = "t".repeat(MAX_TITLE_CHARS + 1);
    for (field, bad, problem) in [
        (SummaryField::Tldr, "   ", TextProblem::Empty),
        (SummaryField::Title, " ", TextProblem::Empty),
        (SummaryField::ActionOwner(0), "", TextProblem::Empty),
        (
            SummaryField::Decision(0),
            "two\nlines",
            TextProblem::ControlCharacter,
        ),
        (SummaryField::Tldr, too_long.as_str(), TextProblem::TooLong),
        (
            SummaryField::Title,
            long_title.as_str(),
            TextProblem::TooLong,
        ),
    ] {
        assert_eq!(
            refused(edit_summary(&svc, m, new, field, bad)),
            EditError::InvalidSummaryText(problem)
        );
    }
    // A summary title may be as long as a meeting title.
    assert!(
        edit_summary(
            &svc,
            m,
            new,
            SummaryField::Title,
            &"t".repeat(MAX_TITLE_CHARS)
        )
        .unwrap()
    );
    delete(&svc, m);
    assert_eq!(
        refused(edit_summary(&svc, m, new, SummaryField::Tldr, "Words.")),
        EditError::MeetingNotFound(m)
    );
}

#[test]
fn edit_summary_keeps_the_summary_out_of_date_after_a_transcript_edit() {
    let dir = TempDir::new("edit");
    let (svc, _) = services(&dir);
    let m = meeting(&svc, "Release", 426);
    let id = store_full(&svc, m, 427);
    let line = view(&svc, m).segments[1].id;
    assert!(edit_segment(&svc, m, line, "a corrected line").unwrap());
    assert!(view(&svc, m).summary_out_of_date());
    // Correcting the summary's words does not make it a summary of the new transcript.
    assert!(edit_summary(&svc, m, id, SummaryField::Tldr, "Reworded.").unwrap());
    assert!(view(&svc, m).summary_out_of_date());
    // And the copy kept for undo is out of date too.
    undo_summary(&svc, m).unwrap();
    assert!(view(&svc, m).summary_out_of_date());
}
