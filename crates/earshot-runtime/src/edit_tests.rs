//! Meeting editing tests (T7.3a). Every name contains `edit`.

use std::sync::Arc;
use std::time::Duration;

use earshot_core::asr::{CancelToken, MeetingTime, Segment, Word};
use earshot_core::capture::TrackId;
use earshot_core::diar::SpeakerRole;
use earshot_core::export::ExportOptions;
use earshot_core::identity::EntityId;
use earshot_core::model::{MeetingRepo, SegmentRow, Speaker, Stamp, TranscriptPass};
use earshot_core::summary::{Decision, SummaryV1};
use earshot_core::voiceprint::{EnrolmentConfig, Person, VoiceprintRepo, merge_persons};
use rand_chacha::ChaCha8Rng;
use rand_core::SeedableRng;

use super::*;
use crate::host::tests::{fake_post, recorded};
use crate::host::{AfterMeeting, HostPost, SkipReason, after_meeting_with};
use crate::identities::{StoredIdentities, decode, record_key};
use crate::testutil::{TempDir, meeting, services};
use crate::view::MeetingView;

fn view(svc: &Services, meeting: EntityId) -> MeetingView {
    MeetingView::load(&**svc.store(), meeting).unwrap().unwrap()
}

fn speaker(svc: &Services, meeting: EntityId, name: &str) -> Speaker {
    view(svc, meeting)
        .speakers
        .into_iter()
        .find(|s| s.display_name == name)
        .unwrap_or_else(|| panic!("no speaker {name}"))
}

/// Text of each segment with its speaker's name, in meeting order.
fn lines(svc: &Services, meeting: EntityId) -> Vec<(String, String)> {
    let v = view(svc, meeting);
    v.segments
        .iter()
        .map(|s| (v.speaker_name(s), s.text.clone()))
        .collect()
}

fn refused(r: Result<impl std::fmt::Debug, RuntimeError>) -> EditError {
    match r {
        Err(RuntimeError::Edit(e)) => e,
        other => panic!("expected an edit refusal, got {other:?}"),
    }
}

/// A new speaker of `meeting` with one final segment on its track of `kind`.
fn add_speaker(
    svc: &Services,
    meeting: EntityId,
    kind: TrackId,
    name: &str,
    text: &str,
    start: u64,
    seed: u64,
) -> EntityId {
    let clock = Arc::clone(&svc.clock);
    let mut rng = ChaCha8Rng::seed_from_u64(seed);
    let mut db = svc.store();
    let track = db
        .tracks(meeting)
        .unwrap()
        .into_iter()
        .find(|t| t.kind == kind)
        .unwrap();
    let (store, hlc) = db.parts();
    let mut stamp = Stamp {
        clock: &*clock,
        hlc,
        rng: &mut rng,
    };
    let s = Speaker::new(meeting, SpeakerRole::Remote, name, None, &mut stamp);
    let words: Vec<Word> = text
        .split(' ')
        .enumerate()
        .map(|(i, w)| Word::new(w, start + 400 * i as u64, start + 400 * i as u64 + 300, 0.9))
        .collect();
    let mut seg = Segment::from_words(
        EntityId::generate(stamp.clock, &mut *stamp.rng),
        track.id,
        words,
        MeetingTime::from_millis(start),
    );
    seg.speaker_id = Some(s.id);
    store.put_speaker(s.clone()).unwrap();
    store
        .put_segment(SegmentRow::new(
            meeting,
            TranscriptPass::Final,
            seg,
            stamp.hlc,
            stamp.clock,
        ))
        .unwrap();
    s.id
}

fn new_person(svc: &Services, name: &str, is_me: bool, seed: u64) -> EntityId {
    let clock = Arc::clone(&svc.clock);
    let mut rng = ChaCha8Rng::seed_from_u64(seed);
    let mut db = svc.store();
    let (store, hlc) = db.parts();
    let mut stamp = Stamp {
        clock: &*clock,
        hlc,
        rng: &mut rng,
    };
    let p = Person::new(name, is_me, &mut stamp);
    store.put_person(p.clone()).unwrap();
    p.id
}

fn delete(svc: &Services, meeting: EntityId) {
    svc.delete_meeting(meeting).unwrap();
}

#[test]
fn edit_rename_speaker_rewrites_the_row_and_refuses_bad_input() {
    let dir = TempDir::new("edit");
    let (svc, _) = services(&dir);
    let m = meeting(&svc, "Release", 1);
    let alice = speaker(&svc, m, "Alice");
    assert!(rename_speaker(&svc, m, alice.id, "  Alice Smith ").unwrap());
    let row = speaker(&svc, m, "Alice Smith");
    assert!(row.meta.updated_at > alice.meta.updated_at);
    assert!(lines(&svc, m).contains(&(
        "Alice Smith".into(),
        "yes friday works for the release".into()
    )));
    // Renaming to the current name writes nothing.
    assert!(!rename_speaker(&svc, m, alice.id, "Alice Smith").unwrap());
    assert_eq!(speaker(&svc, m, "Alice Smith").meta, row.meta);

    for (bad, problem) in [
        ("   ", NameProblem::Empty),
        ("a\nb", NameProblem::ControlCharacter),
        (&"x".repeat(MAX_NAME_CHARS + 1)[..], NameProblem::TooLong),
    ] {
        assert_eq!(
            refused(rename_speaker(&svc, m, alice.id, bad)),
            EditError::InvalidName(problem)
        );
    }
    assert!(rename_speaker(&svc, m, alice.id, &"y".repeat(MAX_NAME_CHARS)).unwrap());
    // Another meeting's speaker, an unknown one, a deleted meeting.
    let other = meeting(&svc, "Other", 2);
    let theirs = speaker(&svc, other, "Alice");
    assert_eq!(
        refused(rename_speaker(&svc, m, theirs.id, "Bob")),
        EditError::SpeakerNotFound(theirs.id)
    );
    let nobody = EntityId::from_bytes([9; 16]);
    assert_eq!(
        refused(rename_speaker(&svc, m, nobody, "Bob")),
        EditError::SpeakerNotFound(nobody)
    );
    delete(&svc, other);
    assert_eq!(
        refused(rename_speaker(&svc, other, theirs.id, "Bob")),
        EditError::MeetingNotFound(other)
    );
}

#[test]
fn edit_merge_speakers_moves_segments_under_the_kept_name() {
    let dir = TempDir::new("edit");
    let (svc, _) = services(&dir);
    let m = meeting(&svc, "Release", 3);
    let alice = speaker(&svc, m, "Alice").id;
    let me = speaker(&svc, m, "Me").id;
    let extra = add_speaker(
        &svc,
        m,
        TrackId::Reference,
        "Speaker 2",
        "and the notes",
        9_000,
        30,
    );
    let before = speaker(&svc, m, "Alice");
    let merged = merge_speakers(&svc, m, alice, extra).unwrap();
    assert_eq!(merged.segments_moved, 1);
    assert!(
        !merged.identity_dropped,
        "no identity record in this meeting"
    );
    let v = view(&svc, m);
    assert!(
        v.speakers.iter().all(|s| s.id != extra),
        "merged row is gone"
    );
    assert!(lines(&svc, m).contains(&("Alice".into(), "and the notes".into())));
    // The kept row is re-stamped (a user edit for a re-run).
    assert!(speaker(&svc, m, "Alice").meta.updated_at > before.meta.updated_at);
    // The merged speaker row is a tombstone.
    let rows = svc.store().speakers(m).unwrap();
    assert!(rows.iter().find(|s| s.id == extra).unwrap().meta.deleted);

    // Merging again, into itself, or "Me" away is refused; into "Me" is allowed (here across
    // tracks: Alice's reference segments move to the mic speaker).
    assert_eq!(
        refused(merge_speakers(&svc, m, alice, extra)),
        EditError::SpeakerNotFound(extra)
    );
    assert_eq!(
        refused(merge_speakers(&svc, m, alice, alice)),
        EditError::SameSpeaker(alice)
    );
    assert_eq!(
        refused(merge_speakers(&svc, m, alice, me)),
        EditError::MergeAwayMe(me)
    );
    assert_eq!(
        merge_speakers(&svc, m, me, alice).unwrap().segments_moved,
        2
    );
    assert!(lines(&svc, m).iter().all(|(who, _)| who == "Me"));
}

#[test]
fn edit_link_speaker_to_a_new_or_existing_person() {
    let dir = TempDir::new("edit");
    let (svc, _) = services(&dir);
    let a = meeting(&svc, "One", 4);
    let b = meeting(&svc, "Two", 5);
    let alice_a = speaker(&svc, a, "Alice").id;
    let alice_b = speaker(&svc, b, "Alice").id;
    let person = link_speaker(
        &svc,
        a,
        alice_a,
        &PersonChoice::New {
            name: " Alice Jones ".into(),
        },
    )
    .unwrap();
    let row = speaker(&svc, a, "Alice Jones");
    assert_eq!(row.person_id, Some(person));
    assert_eq!(
        svc.store().person(person).unwrap().unwrap().display_name,
        "Alice Jones"
    );
    // Linking again writes nothing.
    assert_eq!(
        link_speaker(&svc, a, alice_a, &PersonChoice::Existing(person)).unwrap(),
        person
    );
    assert_eq!(speaker(&svc, a, "Alice Jones").meta, row.meta);
    // A person merged into another links to the one it was merged into.
    let dup = new_person(&svc, "A. Jones", false, 40);
    {
        let clock = Arc::clone(&svc.clock);
        let mut db = svc.store();
        let (store, hlc) = db.parts();
        merge_persons(store, dup, person, hlc, &*clock).unwrap();
    }
    assert_eq!(
        link_speaker(&svc, b, alice_b, &PersonChoice::Existing(dup)).unwrap(),
        person
    );
    assert_eq!(speaker(&svc, b, "Alice Jones").person_id, Some(person));
    // Unknown persons and bad names are refused, nothing written.
    let nobody = EntityId::from_bytes([8; 16]);
    assert_eq!(
        refused(link_speaker(
            &svc,
            b,
            alice_b,
            &PersonChoice::Existing(nobody)
        )),
        EditError::PersonNotFound(nobody)
    );
    assert_eq!(
        refused(link_speaker(
            &svc,
            b,
            alice_b,
            &PersonChoice::New { name: "".into() }
        )),
        EditError::InvalidName(NameProblem::Empty)
    );
    assert_eq!(list_persons(&svc).unwrap().len(), 1);
}

#[test]
fn edit_rename_person_rewrites_its_speakers_in_every_meeting() {
    let dir = TempDir::new("edit");
    let (svc, _) = services(&dir);
    let a = meeting(&svc, "One", 6);
    let b = meeting(&svc, "Two", 7);
    let c = meeting(&svc, "Three", 8);
    let person = new_person(&svc, "Alice", false, 41);
    let merged = new_person(&svc, "Alicia", false, 42);
    for (m, p) in [(a, person), (b, person), (c, merged)] {
        let id = speaker(&svc, m, "Alice").id;
        link_speaker(&svc, m, id, &PersonChoice::Existing(p)).unwrap();
    }
    {
        let clock = Arc::clone(&svc.clock);
        let mut db = svc.store();
        let (store, hlc) = db.parts();
        merge_persons(store, merged, person, hlc, &*clock).unwrap();
    }
    let deleted = meeting(&svc, "Gone", 9);
    let gone = speaker(&svc, deleted, "Alice").id;
    link_speaker(&svc, deleted, gone, &PersonChoice::Existing(person)).unwrap();
    delete(&svc, deleted);

    assert_eq!(rename_person(&svc, person, "Alice Wong").unwrap(), 3);
    for m in [a, b, c] {
        assert!(
            lines(&svc, m).contains(&(
                "Alice Wong".into(),
                "yes friday works for the release".into()
            )),
            "{m}"
        );
    }
    assert_eq!(rename_person(&svc, person, "Alice Wong").unwrap(), 0);
    assert_eq!(
        refused(rename_person(&svc, merged, "X")),
        EditError::PersonNotFound(merged),
        "a merged-away person is renamed through the one it was merged into"
    );
    assert_eq!(
        refused(rename_person(&svc, person, "\t")),
        EditError::InvalidName(NameProblem::Empty)
    );
}

/// A meeting recorded and processed by the host layer with the level fakes.
fn processed(svc: &Services, seed: u64) -> EntityId {
    let outcome = recorded(svc, seed);
    let models = crate::backends::ModelDir::new(svc.data.root().join("models"));
    let (post_cancel, summary_cancel) = (CancelToken::new(), CancelToken::new());
    let run = AfterMeeting {
        models: &models,
        model: "base.en",
        order: &[crate::backends::Accelerator::Cpu],
        post: true,
        whole_pass: false,
        summary: None,
        no_summary: SkipReason::NotRequested,
        post_cancel: &post_cancel,
        summary_cancel: &summary_cancel,
    };
    let result = after_meeting_with(svc, &outcome, &run, fake_post, &mut |_| {});
    assert!(
        matches!(result.post, Ok(HostPost::Done(_))),
        "{:?}",
        result.post
    );
    outcome.meeting
}

fn identity_speakers(svc: &Services, meeting: EntityId) -> Vec<EntityId> {
    match svc
        .store()
        .local_value(record_key(meeting).as_str())
        .unwrap()
        .map(|b| decode(&b))
    {
        None => Vec::new(),
        Some(StoredIdentities::Record(r)) => r.speakers.iter().map(|i| i.speaker).collect(),
        Some(other) => panic!("{other:?}"),
    }
}

fn remote_speakers(svc: &Services, meeting: EntityId) -> Vec<Speaker> {
    view(svc, meeting)
        .speakers
        .into_iter()
        .filter(|s| s.role != SpeakerRole::Me)
        .collect()
}

#[test]
fn edit_merge_drops_the_merged_speakers_identity_entry() {
    let dir = TempDir::new("edit");
    let (svc, _) = services(&dir);
    let m = processed(&svc, 300);
    let remote = remote_speakers(&svc, m);
    assert_eq!(remote.len(), 2, "{remote:?}");
    assert_eq!(identity_speakers(&svc, m), [remote[0].id, remote[1].id]);
    let merged = merge_speakers(&svc, m, remote[0].id, remote[1].id).unwrap();
    assert!(merged.identity_dropped);
    assert_eq!(identity_speakers(&svc, m), [remote[0].id]);
}

#[test]
fn edit_retranscribe_keeps_a_renamed_speaker_and_reports_missing_audio() {
    let dir = TempDir::new("edit");
    let (svc, _) = services(&dir);
    let m = processed(&svc, 310);
    let first = remote_speakers(&svc, m)[0].clone();
    assert!(rename_speaker(&svc, m, first.id, "Dana").unwrap());
    let old_ids: Vec<EntityId> = view(&svc, m).segments.iter().map(|s| s.id).collect();
    let dana_text: Vec<String> = lines(&svc, m)
        .into_iter()
        .filter(|(who, _)| who == "Dana")
        .map(|(_, t)| t)
        .collect();
    assert!(!dana_text.is_empty());

    let cancel = CancelToken::new();
    let mut steps = 0;
    let again = retranscribe_with(&svc, m, &cancel, fake_post, &mut |_| steps += 1).unwrap();
    assert!(again.missing_tracks.is_empty());
    assert!(matches!(again.post, HostPost::Done(_)), "{:?}", again.post);
    assert!(steps > 0);
    let v = view(&svc, m);
    assert_eq!(v.pass, Some(TranscriptPass::Final));
    assert!(
        v.segments.iter().all(|s| !old_ids.contains(&s.id)),
        "replaced"
    );
    // The renamed row is continued with the user's name.
    let dana = speaker(&svc, m, "Dana");
    assert_eq!(dana.id, first.id);
    let now: Vec<String> = lines(&svc, m)
        .into_iter()
        .filter(|(who, _)| who == "Dana")
        .map(|(_, t)| t)
        .collect();
    assert_eq!(now, dana_text);

    // Refused while another job processes it.
    {
        let _busy = svc
            .lease_meeting_for(m, crate::MeetingJob::Processing)
            .unwrap();
        assert!(matches!(
            retranscribe_with(&svc, m, &cancel, fake_post, &mut |_| {}),
            Err(RuntimeError::MeetingBusy(id)) if id == m
        ));
    }
    // Cancelled before it starts.
    let cancelled = CancelToken::new();
    cancelled.cancel();
    assert!(matches!(
        retranscribe_with(&svc, m, &cancelled, fake_post, &mut |_| {})
            .unwrap()
            .post,
        HostPost::Cancelled { .. }
    ));
    // The reference recording is gone: the mic is processed, the reference reported.
    let files = svc.store().audio_files(m).unwrap();
    let tracks = svc.store().tracks(m).unwrap();
    let file_of = |kind: TrackId| {
        let t = tracks.iter().find(|t| t.kind == kind).unwrap();
        let f = files.iter().find(|f| f.track_id == t.id).unwrap();
        svc.data.resolve(&f.path).unwrap()
    };
    std::fs::remove_file(file_of(TrackId::Reference)).unwrap();
    let partial = retranscribe_with(&svc, m, &cancel, fake_post, &mut |_| {}).unwrap();
    assert_eq!(partial.missing_tracks, [TrackId::Reference]);
    assert!(matches!(partial.post, HostPost::Done(_)));
    std::fs::remove_file(file_of(TrackId::Mic)).unwrap();
    let none = retranscribe_with(&svc, m, &cancel, fake_post, &mut |_| {}).unwrap();
    assert_eq!(none.missing_tracks, [TrackId::Mic, TrackId::Reference]);
    assert!(matches!(none.post, HostPost::NothingToProcess));
    // A deleted meeting.
    delete(&svc, m);
    assert_eq!(
        refused(retranscribe_with(&svc, m, &cancel, fake_post, &mut |_| {})),
        EditError::MeetingNotFound(m)
    );
}

#[test]
fn edit_speaker_sample_is_the_longest_segment_from_the_recording() {
    let dir = TempDir::new("edit");
    let (svc, _) = services(&dir);
    let m = processed(&svc, 320);
    let s = remote_speakers(&svc, m)[0].clone();
    let segment = view(&svc, m)
        .segments
        .into_iter()
        .find(|x| x.speaker_id == Some(s.id))
        .unwrap();
    let sample = speaker_sample(&svc, m, s.id, 100).unwrap().unwrap();
    assert_eq!(sample.segment_id, segment.id);
    assert_eq!(sample.start, segment.start);
    assert_eq!(sample.end.as_millis(), segment.start.as_millis() + 100);
    // Exactly what the recording stores there.
    let tracks = svc.store().tracks(m).unwrap();
    let file = svc
        .store()
        .audio_files(m)
        .unwrap()
        .into_iter()
        .find(|f| f.track_id == segment.track)
        .unwrap();
    let path = svc.data.resolve(&file.path).unwrap();
    let (rate, audio) = earshot_store::read_flac(&path).unwrap();
    assert_eq!(sample.sample_rate_hz, rate);
    let from = usize::try_from(segment.start.as_millis() * u64::from(rate) / 1000).unwrap();
    let to = from + usize::try_from(100 * u64::from(rate) / 1000).unwrap();
    let expected: Vec<i16> = audio[from..to]
        .iter()
        .map(|&x| earshot_core::audio::f32_to_i16(x))
        .collect();
    assert_eq!(sample.samples, expected);
    assert!(tracks.iter().any(|t| t.id == segment.track));
    // Uncapped: the whole segment.
    let whole = speaker_sample(&svc, m, s.id, 60_000).unwrap().unwrap();
    assert_eq!(whole.end, segment.end);
    // A speaker with no segment, and a recording that is gone: no sample, no error.
    let silent = add_speaker(&svc, m, TrackId::Reference, "Quiet", "x", 1_000, 50);
    {
        // Its only segment is removed again.
        let mut db = svc.store();
        let mut row = db
            .segments(m)
            .unwrap()
            .into_iter()
            .find(|r| r.segment.speaker_id == Some(silent))
            .unwrap();
        let clock = Arc::clone(&svc.clock);
        let (store, hlc) = db.parts();
        row.meta.mark_deleted(hlc, &*clock);
        store.put_segment(row).unwrap();
    }
    assert_eq!(speaker_sample(&svc, m, silent, 100).unwrap(), None);
    std::fs::remove_file(&path).unwrap();
    assert_eq!(speaker_sample(&svc, m, s.id, 100).unwrap(), None);
    let nobody = EntityId::from_bytes([7; 16]);
    assert_eq!(
        refused(speaker_sample(&svc, m, nobody, 100)),
        EditError::SpeakerNotFound(nobody)
    );
}

/// Post settings that make the level fakes' model enrollable: its threshold, and
/// `min_new_speech` (zero for `any`, else the default).
fn calibrated(any: bool) -> PostConfig {
    PostConfig {
        voiceprint_thresholds: crate::post::ModelThresholds::new()
            .with(earshot_core::diar::EmbeddingModelId::new("level@1"), 0.5),
        enrolment: if any {
            EnrolmentConfig {
                min_new_speech: Duration::ZERO,
            }
        } else {
            EnrolmentConfig::default()
        },
        ..PostConfig::default()
    }
}

/// The live voiceprint count of `person` (the number of centroids added).
fn print_count(svc: &Services, person: EntityId) -> Option<u32> {
    svc.store()
        .voiceprints_of(person)
        .unwrap()
        .into_iter()
        .find(|v| !v.meta.deleted)
        .map(|v| v.count)
}

/// Links `speaker` to `person` without the edit checks (data as an older build or a sync
/// may leave it).
fn force_link(svc: &Services, meeting: EntityId, speaker: EntityId, person: EntityId) {
    let clock = Arc::clone(&svc.clock);
    let mut db = svc.store();
    let mut s = db
        .speakers(meeting)
        .unwrap()
        .into_iter()
        .find(|s| s.id == speaker)
        .unwrap();
    s.person_id = Some(person);
    let (store, hlc) = db.parts();
    s.meta.touch(hlc, &*clock);
    store.put_speaker(s).unwrap();
}

#[test]
fn edit_remember_voices_enrols_linked_speakers_and_forget_removes_them() {
    let dir = TempDir::new("edit");
    let (svc, _) = services(&dir);
    let m = processed(&svc, 330);
    let remote = remote_speakers(&svc, m);
    let me = view(&svc, m)
        .speakers
        .into_iter()
        .find(|s| s.role == SpeakerRole::Me)
        .unwrap();
    let dana = link_speaker(
        &svc,
        m,
        remote[0].id,
        &PersonChoice::New {
            name: "Dana".into(),
        },
    )
    .unwrap();
    let any = calibrated(true);

    // Refusals write nothing: the second speaker has no person, "Me" is the user, and so is
    // a speaker linked to the user's person (which `link_speaker` itself refuses).
    assert_eq!(
        refused(remember_voices_with(
            &svc,
            m,
            &[remote[0].id, remote[1].id],
            &any
        )),
        EditError::NoPerson(remote[1].id)
    );
    assert_eq!(
        refused(remember_voices_with(&svc, m, &[me.id], &any)),
        EditError::MeVoice(me.id)
    );
    let user = new_person(&svc, "Casey", true, 43);
    force_link(&svc, m, remote[1].id, user);
    assert_eq!(
        refused(remember_voices_with(&svc, m, &[remote[1].id], &any)),
        EditError::MeVoice(remote[1].id)
    );
    assert_eq!(print_count(&svc, dana), None);
    // The production thresholds: the level fakes' model is not calibrated, so no print that
    // could never match is stored.
    let uncalibrated = remember_voices(&svc, m, &[remote[0].id]).unwrap();
    assert_eq!(
        uncalibrated[0].1,
        Remembered::Uncalibrated {
            model: earshot_core::diar::EmbeddingModelId::new("level@1")
        }
    );
    // The default minimum: one short segment is not enough for a new voiceprint.
    let short = remember_voices_with(&svc, m, &[remote[0].id], &calibrated(false)).unwrap();
    assert!(
        matches!(short[0].1, Remembered::NotEnoughSpeech { .. }),
        "{short:?}"
    );
    assert_eq!(print_count(&svc, dana), None);

    let done = remember_voices_with(&svc, m, &[remote[0].id, remote[0].id], &any).unwrap();
    assert_eq!(done.len(), 1, "duplicates are enrolled once");
    let Remembered::Enrolled { person, voiceprint } = done[0].1.clone() else {
        panic!("{done:?}");
    };
    assert_eq!(person, dana);
    let prints = svc.store().voiceprints_of(dana).unwrap();
    assert_eq!(prints.len(), 1);
    assert_eq!(prints[0].id, voiceprint);
    assert_eq!(prints[0].count, 1);
    assert_eq!(prints[0].model.as_str(), "level@1");
    // A repeat adds nothing.
    let again = remember_voices_with(&svc, m, &[remote[0].id], &any).unwrap();
    assert_eq!(again[0].1, Remembered::AlreadyEnrolled);
    assert_eq!(print_count(&svc, dana), Some(1));
    let listed = list_persons(&svc).unwrap();
    let info = |id| listed.iter().find(|p| p.id == id).unwrap().clone();
    assert!(info(dana).has_voiceprint && !info(dana).is_me);
    assert!(!info(user).has_voiceprint && info(user).is_me);

    // Forgetting: no live voiceprint, vector zeroed, the meeting's centroid entry gone.
    assert!(identity_speakers(&svc, m).contains(&remote[0].id));
    assert_eq!(forget_person_voices(&svc, dana).unwrap(), 1);
    let prints = svc.store().voiceprints_of(dana).unwrap();
    assert!(
        prints
            .iter()
            .all(|v| v.meta.deleted && v.sum.iter().all(|x| *x == 0.0))
    );
    assert!(!identity_speakers(&svc, m).contains(&remote[0].id));
    assert!(!list_persons(&svc).unwrap().iter().any(|p| p.has_voiceprint));
    assert_eq!(forget_person_voices(&svc, dana).unwrap(), 0);
    // With the centroid gone there is nothing to remember.
    let after = remember_voices_with(&svc, m, &[remote[0].id], &any).unwrap();
    assert_eq!(after[0].1, Remembered::NoStoredVoice);
    let nobody = EntityId::from_bytes([6; 16]);
    assert_eq!(
        refused(forget_person_voices(&svc, nobody)),
        EditError::PersonNotFound(nobody)
    );
}

#[test]
fn edit_remove_person_unnames_their_speakers_and_refuses_the_user() {
    let dir = TempDir::new("edit");
    let (svc, _) = services(&dir);
    let m = processed(&svc, 345);
    let s = remote_speakers(&svc, m)[0].id;
    let dana = link_speaker(
        &svc,
        m,
        s,
        &PersonChoice::New {
            name: "Dana".into(),
        },
    )
    .unwrap();
    remember_voices_with(&svc, m, &[s], &calibrated(true)).unwrap();
    assert_eq!(print_count(&svc, dana), Some(1));

    let report = remove_person(&svc, dana).unwrap();
    assert_eq!(
        (report.persons_removed, report.voiceprints_forgotten),
        (1, 1)
    );
    assert_eq!(report.speakers_unnamed, 1);
    assert!(!list_persons(&svc).unwrap().iter().any(|p| p.id == dana));
    let row = remote_speakers(&svc, m)
        .into_iter()
        .find(|r| r.id == s)
        .unwrap();
    assert_eq!(row.person_id, None);
    assert!(
        row.display_name.starts_with("Speaker "),
        "{}",
        row.display_name
    );
    // Their name is free to use again, for a new person.
    assert_eq!(
        refused(remove_person(&svc, dana)),
        EditError::PersonNotFound(dana)
    );
    let again = link_speaker(
        &svc,
        m,
        s,
        &PersonChoice::New {
            name: "Dana".into(),
        },
    )
    .unwrap();
    assert_ne!(again, dana);

    let me = new_person(&svc, "Me", true, 346);
    assert_eq!(
        refused(remove_person(&svc, me)),
        EditError::PersonIsTheUser(me)
    );
    assert!(list_persons(&svc).unwrap().iter().any(|p| p.id == me));
}

#[test]
fn edit_remove_person_refuses_someone_the_user_was_merged_into() {
    let dir = TempDir::new("edit");
    let (svc, _) = services(&dir);
    let me = new_person(&svc, "Me", true, 347);
    let other = new_person(&svc, "Other", false, 348);
    {
        let clock = Arc::clone(&svc.clock);
        let mut db = svc.store();
        let (store, hlc) = db.parts();
        merge_persons(store, me, other, hlc, &*clock).unwrap();
    }
    assert_eq!(
        refused(remove_person(&svc, other)),
        EditError::PersonIsTheUser(other)
    );
    assert!(!svc.store().person(me).unwrap().unwrap().meta.deleted);
    assert!(svc.store().person(other).unwrap().unwrap().is_live());
}

#[test]
fn edit_remember_mark_is_cleared_by_a_new_person_and_by_a_new_final_pass() {
    let dir = TempDir::new("edit");
    let (svc, _) = services(&dir);
    let m = processed(&svc, 340);
    let s = remote_speakers(&svc, m)[0].id;
    let any = calibrated(true);
    let dana = link_speaker(
        &svc,
        m,
        s,
        &PersonChoice::New {
            name: "Dana".into(),
        },
    )
    .unwrap();
    remember_voices_with(&svc, m, &[s], &any).unwrap();
    assert_eq!(print_count(&svc, dana), Some(1));
    // The voice was someone else: linked again, it can be remembered for them.
    let dee = link_speaker(&svc, m, s, &PersonChoice::New { name: "Dee".into() }).unwrap();
    let r = remember_voices_with(&svc, m, &[s], &any).unwrap();
    assert!(
        matches!(r[0].1, Remembered::Enrolled { person, .. } if person == dee),
        "{r:?}"
    );
    assert_eq!(
        print_count(&svc, dana),
        Some(1),
        "dana's print is not touched"
    );
    assert_eq!(
        remember_voices_with(&svc, m, &[s], &any).unwrap()[0].1,
        Remembered::AlreadyEnrolled
    );
    // A new final pass writes a new centroid, not yet remembered.
    let cancel = CancelToken::new();
    retranscribe_with(&svc, m, &cancel, fake_post, &mut |_| {}).unwrap();
    assert_eq!(speaker(&svc, m, "Dee").id, s, "the linked row is continued");
    let r = remember_voices_with(&svc, m, &[s], &any).unwrap();
    assert!(matches!(r[0].1, Remembered::Enrolled { .. }), "{r:?}");
    assert_eq!(print_count(&svc, dee), Some(2));
}

#[test]
fn edit_link_keeps_the_user_to_the_me_row() {
    let dir = TempDir::new("edit");
    let (svc, _) = services(&dir);
    let m = meeting(&svc, "Release", 12);
    let me = speaker(&svc, m, "Me").id;
    let alice = speaker(&svc, m, "Alice").id;
    let user = new_person(&svc, "Casey", true, 52);
    let bob = new_person(&svc, "Bob", false, 53);
    assert_eq!(
        refused(link_speaker(&svc, m, alice, &PersonChoice::Existing(user))),
        EditError::NotTheUser(alice)
    );
    assert_eq!(
        refused(link_speaker(&svc, m, me, &PersonChoice::Existing(bob))),
        EditError::MeIsTheUser(me)
    );
    assert_eq!(
        refused(link_speaker(
            &svc,
            m,
            me,
            &PersonChoice::New { name: "Sam".into() }
        )),
        EditError::MeIsTheUser(me)
    );
    // Nothing was written: no person "Sam", both rows as they were.
    assert!(!list_persons(&svc).unwrap().iter().any(|p| p.name == "Sam"));
    assert_eq!(speaker(&svc, m, "Alice").person_id, None);
    assert_eq!(
        link_speaker(&svc, m, me, &PersonChoice::Existing(user)).unwrap(),
        user
    );
    assert_eq!(speaker(&svc, m, "Casey").id, me);
    assert_eq!(
        link_speaker(&svc, m, alice, &PersonChoice::Existing(bob)).unwrap(),
        bob
    );
}

#[test]
fn edit_merge_deletes_an_unreadable_identity_record() {
    let dir = TempDir::new("edit");
    let (svc, _) = services(&dir);
    let m = processed(&svc, 350);
    let remote = remote_speakers(&svc, m);
    {
        let mut db = svc.store();
        crate::local::set(&mut db, &record_key(m), &[9, 0, 0, 0, b'{']).unwrap();
    }
    let merged = merge_speakers(&svc, m, remote[0].id, remote[1].id).unwrap();
    assert!(merged.identity_dropped);
    assert!(!merged.checkpoint_pending);
    assert_eq!(
        svc.store().local_value(record_key(m).as_str()).unwrap(),
        None
    );
}

#[test]
fn edit_merge_reports_a_checkpoint_another_connection_blocks() {
    let dir = TempDir::new("edit");
    let (svc, _) = services(&dir);
    let m = processed(&svc, 360);
    let remote = remote_speakers(&svc, m);
    let path = svc.data.db_path();
    let (ready_tx, ready_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
    // A reader holding a snapshot keeps the WAL from being truncated.
    let reader = std::thread::spawn(move || {
        let mut other = earshot_store::Store::open(&path).unwrap();
        other
            .transaction(|tx| -> Result<(), earshot_store::StoreError> {
                tx.meeting(m)?;
                ready_tx.send(()).unwrap();
                release_rx.recv().unwrap();
                Ok(())
            })
            .unwrap();
    });
    ready_rx.recv().unwrap();
    let merged = merge_speakers(&svc, m, remote[0].id, remote[1].id).unwrap();
    assert!(merged.identity_dropped);
    assert!(merged.checkpoint_pending, "{merged:?}");
    release_tx.send(()).unwrap();
    reader.join().unwrap();
    // The merge itself is committed.
    assert_eq!(identity_speakers(&svc, m), [remote[0].id]);
    svc.store().flush_erasures().unwrap();
}

#[test]
fn edit_retranscribe_reports_a_summary_of_the_replaced_transcript() {
    let dir = TempDir::new("edit");
    let (svc, _) = services(&dir);
    let m = processed(&svc, 370);
    let cancel = CancelToken::new();
    let plain = retranscribe_with(&svc, m, &cancel, fake_post, &mut |_| {}).unwrap();
    assert!(!plain.summary_stale, "no summary");
    store(&svc, m, "The release.", 66);
    let again = retranscribe_with(&svc, m, &cancel, fake_post, &mut |_| {}).unwrap();
    assert!(again.summary_stale);
    // A summary of the new transcript is current.
    store(&svc, m, "The release again.", 67);
    let h = summary_history(&svc, m).unwrap();
    assert!(h.current.is_some());
    let ids: std::collections::BTreeSet<EntityId> =
        view(&svc, m).segments.iter().map(|s| s.id).collect();
    assert!(cites_only(&h.current.unwrap().summary, &ids));
}

#[test]
fn edit_list_persons_shows_live_persons_by_name() {
    let dir = TempDir::new("edit");
    let (svc, _) = services(&dir);
    let zed = new_person(&svc, "zed", false, 44);
    let amy = new_person(&svc, "Amy", false, 45);
    let me = new_person(&svc, "Me", true, 46);
    let dup = new_person(&svc, "Amy B", false, 47);
    {
        let clock = Arc::clone(&svc.clock);
        let mut db = svc.store();
        let (store, hlc) = db.parts();
        merge_persons(store, dup, amy, hlc, &*clock).unwrap();
    }
    let gone = new_person(&svc, "Bea", false, 48);
    {
        let clock = Arc::clone(&svc.clock);
        let mut db = svc.store();
        let mut p = db.person(gone).unwrap().unwrap();
        let (store, hlc) = db.parts();
        p.meta.mark_deleted(hlc, &*clock);
        store.put_person(p).unwrap();
    }
    let listed = list_persons(&svc).unwrap();
    let names: Vec<&str> = listed.iter().map(|p| p.name.as_str()).collect();
    assert_eq!(names, ["Amy", "Me", "zed"]);
    assert_eq!(
        listed.iter().map(|p| p.id).collect::<Vec<_>>(),
        [amy, me, zed]
    );
    assert!(listed.iter().all(|p| !p.has_voiceprint));
    assert!(listed.iter().find(|p| p.id == me).unwrap().is_me);
}

#[test]
fn edit_list_persons_counts_a_merged_persons_voiceprint() {
    let dir = TempDir::new("edit");
    let (svc, _) = services(&dir);
    let amy = new_person(&svc, "Amy", false, 49);
    let dup = new_person(&svc, "Amy B", false, 50);
    {
        let clock = Arc::clone(&svc.clock);
        let mut db = svc.store();
        let (store, hlc) = db.parts();
        let mut rng = ChaCha8Rng::seed_from_u64(51);
        let mut stamp = Stamp {
            clock: &*clock,
            hlc: &mut *hlc,
            rng: &mut rng,
        };
        let e = earshot_core::diar::Embedding::new(
            earshot_core::diar::EmbeddingModelId::new("m@1"),
            vec![1.0, 0.0],
        );
        earshot_core::voiceprint::enroll(
            store,
            dup,
            &[e],
            Duration::from_secs(20),
            &EnrolmentConfig::default(),
            &mut stamp,
        )
        .unwrap();
        merge_persons(store, dup, amy, hlc, &*clock).unwrap();
    }
    let listed = list_persons(&svc).unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].id, amy);
    assert!(listed[0].has_voiceprint);
}

fn summary_citing(tldr: &str, segment: EntityId) -> SummaryV1 {
    SummaryV1 {
        title: "Release planning".into(),
        tldr: tldr.into(),
        enhanced_notes: vec![],
        decisions: vec![Decision {
            text: "Ship on Friday.".into(),
            source_segment_ids: vec![segment],
        }],
        action_items: vec![],
        open_questions: vec![],
        topics: vec![],
    }
}

fn store(svc: &Services, meeting: EntityId, tldr: &str, seed: u64) -> EntityId {
    let v = view(svc, meeting);
    let cited = v.segments[0].id;
    crate::summary::store_summary(
        svc,
        &v,
        "test-model".into(),
        &summary_citing(tldr, cited),
        &mut ChaCha8Rng::seed_from_u64(seed),
    )
    .unwrap()
    .0
}

fn exported(svc: &Services, dir: &TempDir, meeting: EntityId, seed: u64) -> String {
    let vault = dir.0.join("vault");
    std::fs::create_dir_all(vault.join(".obsidian")).unwrap();
    crate::export::export_meeting(
        svc,
        &vault,
        meeting,
        &ExportOptions::default(),
        &mut ChaCha8Rng::seed_from_u64(seed),
    )
    .unwrap();
    let notes: Vec<_> = std::fs::read_dir(vault.join("Meetings"))
        .unwrap()
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().ends_with(".md"))
        .collect();
    assert_eq!(notes.len(), 1);
    std::fs::read_to_string(notes[0].path()).unwrap()
}

#[test]
fn edit_summary_undo_swaps_current_and_previous_and_export_follows() {
    let dir = TempDir::new("edit");
    let (svc, _) = services(&dir);
    let m = meeting(&svc, "Release", 10);
    let h = summary_history(&svc, m).unwrap();
    assert!(h.current.is_none() && h.previous.is_none());
    let a = store(&svc, m, "First take on the release.", 60);
    assert_eq!(refused(undo_summary(&svc, m)), EditError::NoPreviousSummary);
    let b = store(&svc, m, "Second take on the release.", 61);
    let h = summary_history(&svc, m).unwrap();
    assert_eq!(h.current.as_ref().unwrap().id, b);
    let prev = h.previous.unwrap();
    assert_eq!(prev.summary.id, a);
    assert!(prev.restorable);
    assert!(exported(&svc, &dir, m, 70).contains("Second take"));

    assert_eq!(undo_summary(&svc, m).unwrap(), a);
    assert_eq!(view(&svc, m).summary.unwrap().id, a);
    let h = summary_history(&svc, m).unwrap();
    assert_eq!(h.previous.unwrap().summary.id, b);
    let note = exported(&svc, &dir, m, 71);
    assert!(
        note.contains("First take") && !note.contains("Second take"),
        "{note}"
    );

    // Twice: back to the original.
    assert_eq!(undo_summary(&svc, m).unwrap(), b);
    assert_eq!(view(&svc, m).summary.unwrap().id, b);
    assert!(exported(&svc, &dir, m, 72).contains("Second take"));

    // A third summary: only the one it replaced is the previous one.
    let c = store(&svc, m, "Third take on the release.", 62);
    let h = summary_history(&svc, m).unwrap();
    assert_eq!(h.current.unwrap().id, c);
    assert_eq!(h.previous.unwrap().summary.id, b);
    let live: Vec<EntityId> = svc
        .store()
        .summaries(m)
        .unwrap()
        .into_iter()
        .filter(|s| !s.meta.deleted)
        .map(|s| s.id)
        .collect();
    assert_eq!(live, [c]);
}

#[test]
fn edit_summary_undo_refuses_a_previous_summary_of_a_replaced_transcript() {
    let dir = TempDir::new("edit");
    let (svc, _) = services(&dir);
    let m = meeting(&svc, "Release", 11);
    store(&svc, m, "First take.", 63);
    // The transcript is replaced (a re-transcription gives segments new ids) before the
    // second summary, which cites the new transcript.
    {
        let clock = Arc::clone(&svc.clock);
        let mut db = svc.store();
        let mut segs: Vec<Segment> = view_of(&db, m).segments;
        let mut rng = ChaCha8Rng::seed_from_u64(64);
        for s in &mut segs {
            s.id = EntityId::generate(&*clock, &mut rng);
        }
        let (store, hlc) = db.parts();
        store
            .replace_segments(m, TranscriptPass::Final, segs, hlc, &*clock)
            .unwrap();
    }
    store(&svc, m, "Second take.", 65);
    let h = summary_history(&svc, m).unwrap();
    assert!(!h.previous.unwrap().restorable);
    assert_eq!(
        refused(undo_summary(&svc, m)),
        EditError::PreviousSummaryStale
    );
    assert!(
        view(&svc, m)
            .summary
            .unwrap()
            .summary
            .tldr
            .contains("Second")
    );
    delete(&svc, m);
    assert_eq!(
        refused(summary_history(&svc, m)),
        EditError::MeetingNotFound(m)
    );
}

fn view_of(db: &crate::Db, meeting: EntityId) -> MeetingView {
    MeetingView::load(&**db, meeting).unwrap().unwrap()
}

/// T9.12: "This is me" and "This isn't me".
#[path = "edit_mark_me_tests.rs"]
mod mark_me_tests;

/// T12.16: the title and the transcript text.
#[path = "edit_text_tests.rs"]
mod text_tests;

/// T12.19: saving and ignoring a voice.
#[path = "edit_speakers_tests.rs"]
mod speakers_tests;

/// T12.15ec: suggestions beside words.
#[path = "edit_words_tests.rs"]
mod words_tests;
