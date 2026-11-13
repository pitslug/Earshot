//! T9.12: a voice the user marked as themselves, through a re-run of the cleanup. Every name
//! contains `post` and `edit`.

use super::*;
use crate::identities::{SpeakerIdentity, StoredIdentities, decode, record_key};

/// Two voices in the room: scene speaker 0, then scene speaker 3.
fn room_scene() -> MonoBuffer {
    scene_audio(
        &[
            SceneTurn::new(0, 0, 10_000),
            SceneTurn::new(3, 10_000, 20_000),
        ],
        20_000,
    )
}

/// One run's script of [`room_scene`]: "one" from the first voice, "two" from the second.
fn room_words() -> Vec<RawSegment> {
    vec![
        raw(&[("one", 2_000, 2_300)]),
        raw(&[("two", 12_000, 12_300)]),
    ]
}

type RoomPost = PostProcessor<FakeTranscriber, FakeSegmenter, FakeEmbedder>;

/// A processor whose enrolment would accept any speech, so a mark that reached enrolment
/// could not hide behind "not enough speech".
fn room_post(runs: usize, enroll_me: bool) -> RoomPost {
    PostProcessor::new(
        FakeTranscriber::scripted((0..runs).map(|_| Ok(room_words()))),
        FakeSegmenter::segmentation_3_0(),
        embedder(),
        PostConfig {
            enroll_me,
            enrolment: EnrolmentConfig {
                min_new_speech: Duration::ZERO,
            },
            ..config()
        },
    )
    .unwrap()
}

fn run_room(
    post: &mut RoomPost,
    svc: &Services,
    track: &Track,
    audio: MonoBuffer,
    seed: u64,
) -> PostOutcome {
    post.run(
        svc,
        PostInput {
            echo_locks: Vec::new(),
            meeting: track.meeting_id,
            tracks: vec![track_input(track, audio)],
        },
        &mut ChaCha8Rng::seed_from_u64(seed),
        &CancelToken::new(),
    )
    .unwrap()
}

fn live_rows(svc: &Services, meeting: EntityId) -> Vec<earshot_core::model::Speaker> {
    svc.store()
        .speakers(meeting)
        .unwrap()
        .into_iter()
        .filter(|s| !s.meta.deleted)
        .collect()
}

fn live_me_rows(svc: &Services, meeting: EntityId) -> Vec<EntityId> {
    live_rows(svc, meeting)
        .iter()
        .filter(|s| s.role == SpeakerRole::Me)
        .map(|s| s.id)
        .collect()
}

fn final_speakers(svc: &Services, meeting: EntityId) -> Vec<Option<EntityId>> {
    let store = svc.store();
    live_segments(&**store, meeting, TranscriptPass::Final)
        .unwrap()
        .iter()
        .map(|r| r.segment.speaker_id)
        .collect()
}

fn entries(svc: &Services, meeting: EntityId) -> Vec<SpeakerIdentity> {
    match svc
        .store()
        .local_value(record_key(meeting).as_str())
        .unwrap()
        .map(|b| decode(&b))
    {
        None => Vec::new(),
        Some(StoredIdentities::Record(r)) => r.speakers,
        Some(other) => panic!("{other:?}"),
    }
}

fn names(outcome: &PostOutcome) -> Vec<&str> {
    outcome
        .speakers
        .iter()
        .map(|s| s.display_name.as_str())
        .collect()
}

/// Enrols scene voice `k` for the existing person `person` (the explicit "Your voice").
fn enroll_existing(svc: &Services, person: EntityId, k: usize) {
    let clock = ManualClock::new(1_760_000_100_000);
    let mut rng = ChaCha8Rng::seed_from_u64(71);
    let mut db = svc.store();
    let (store, hlc) = db.parts();
    let mut stamp = Stamp {
        clock: &clock,
        hlc,
        rng: &mut rng,
    };
    let e = earshot_core::diar::Embedding::new(EmbeddingModelId::new(MODEL), centroid(k));
    enroll(
        &mut *store,
        person,
        &[e],
        Duration::from_secs(20),
        &EnrolmentConfig::default(),
        &mut stamp,
    )
    .unwrap();
}

#[test]
fn post_rerun_keeps_an_edit_marking_a_voice_as_me_and_adds_no_second_me() {
    let dir = TempDir::new();
    let svc = services(&dir);
    let mic = meeting_with(&svc, &[TrackId::Mic])[0].clone();
    let meeting = mic.meeting_id;
    let mut post = room_post(3, true);
    let first = run_room(&mut post, &svc, &mic, room_scene(), 500);
    assert_eq!(names(&first), ["Speaker 1", "Speaker 2"]);
    let (voice, other) = (first.speakers[0].id, first.speakers[1].id);

    let user = crate::edit::mark_me(&svc, meeting, voice).unwrap();

    // Twice, so the kept entry is itself good for the next run.
    for seed in [501, 502] {
        let again = run_room(&mut post, &svc, &mic, room_scene(), seed);
        let got: Vec<(EntityId, SpeakerRole, Option<EntityId>)> = again
            .speakers
            .iter()
            .map(|s| (s.id, s.role, s.person))
            .collect();
        assert_eq!(
            got,
            [
                (voice, SpeakerRole::Me, Some(user)),
                (other, SpeakerRole::Room, None),
            ]
        );
        assert_eq!(names(&again), ["Me", "Speaker 1"]);
        assert_eq!(live_me_rows(&svc, meeting), [voice]);
        assert_eq!(live_rows(&svc, meeting).len(), 2);
        // The stored row is what the outcome says: the role did not follow the policy.
        let row = live_rows(&svc, meeting)
            .into_iter()
            .find(|s| s.id == voice)
            .unwrap();
        assert_eq!((row.role, row.person_id), (SpeakerRole::Me, Some(user)));
        // Never enrolled from, even with the option on and any speech enough.
        assert!(again.enrolled_me.is_none());
        assert!(again.warnings.is_empty(), "{:?}", again.warnings);
        assert!(svc.store().voiceprints_of(user).unwrap().is_empty());
        assert_eq!(final_speakers(&svc, meeting), [Some(voice), Some(other)]);
        // The voice's entry is still there for the next run, as an edit.
        assert!(
            entries(&svc, meeting)
                .iter()
                .find(|i| i.speaker == voice)
                .unwrap()
                .user_edited
        );
    }
}

#[test]
fn post_rerun_loses_an_edit_mark_with_the_voice_when_it_does_not_match() {
    let dir = TempDir::new();
    let svc = services(&dir);
    let mic = meeting_with(&svc, &[TrackId::Mic])[0].clone();
    let meeting = mic.meeting_id;
    let mut post = room_post(2, true);
    let first = run_room(&mut post, &svc, &mic, room_scene(), 503);
    let voice = first.speakers[0].id;
    crate::edit::mark_me(&svc, meeting, voice).unwrap();

    // The marked voice is not in this run: another voice took its place.
    let changed = scene_audio(
        &[
            SceneTurn::new(5, 0, 10_000),
            SceneTurn::new(3, 10_000, 20_000),
        ],
        20_000,
    );
    let again = run_room(&mut post, &svc, &mic, changed, 504);
    assert!(again.speakers.iter().all(|s| s.role == SpeakerRole::Room));
    assert!(again.speakers.iter().all(|s| s.id != voice));
    assert!(live_me_rows(&svc, meeting).is_empty());
    assert!(live_rows(&svc, meeting).iter().all(|s| s.id != voice));
    assert!(
        entries(&svc, meeting).iter().all(|i| i.speaker != voice),
        "the centroid goes with the tombstoned row"
    );
    assert!(again.enrolled_me.is_none());
    // The user hears that no voice is "Me" any more.
    assert_eq!(
        again
            .warnings
            .iter()
            .filter(|w| matches!(w, PostWarning::MeNotFoundAgain))
            .count(),
        1,
        "{:?}",
        again.warnings
    );
}

/// A voice marked with no identity entry (an invalid centroid, an unreadable record) cannot be
/// matched by a re-run: the mark goes, and the run says so rather than dropping it silently.
#[test]
fn post_rerun_says_so_when_an_edit_mark_without_an_entry_goes() {
    let dir = TempDir::new();
    let svc = services(&dir);
    let mic = meeting_with(&svc, &[TrackId::Mic])[0].clone();
    let meeting = mic.meeting_id;
    let mut post = room_post(2, false);
    let first = run_room(&mut post, &svc, &mic, room_scene(), 513);
    let voice = first.speakers[0].id;
    // Test set-up only: the meeting has no identity record, as when no centroid was valid.
    svc.store()
        .delete_local_value(record_key(meeting).as_str())
        .unwrap();
    crate::edit::mark_me(&svc, meeting, voice).unwrap();
    assert_eq!(live_me_rows(&svc, meeting), [voice]);

    let again = run_room(&mut post, &svc, &mic, room_scene(), 514);
    assert!(live_me_rows(&svc, meeting).is_empty());
    assert!(again.speakers.iter().all(|s| s.role == SpeakerRole::Room));
    assert_eq!(again.warnings, [PostWarning::MeNotFoundAgain]);
}

/// A re-run that keeps the marked voice, or finds no earlier "Me" at all, says nothing of it.
#[test]
fn post_rerun_says_nothing_of_me_when_no_me_was_lost() {
    let dir = TempDir::new();
    let svc = services(&dir);
    let mic = meeting_with(&svc, &[TrackId::Mic])[0].clone();
    let meeting = mic.meeting_id;
    let mut post = room_post(3, false);
    let first = run_room(&mut post, &svc, &mic, room_scene(), 515);
    let again = run_room(&mut post, &svc, &mic, room_scene(), 516);
    assert!(again.warnings.is_empty(), "{:?}", again.warnings);
    crate::edit::mark_me(&svc, meeting, first.speakers[0].id).unwrap();
    let kept = run_room(&mut post, &svc, &mic, room_scene(), 517);
    assert!(kept.warnings.is_empty(), "{:?}", kept.warnings);
}

#[test]
fn post_rerun_a_voiceprint_me_takes_over_an_edit_mark_with_one_me_row() {
    // The user's explicit enrolment came after the mark. Whichever voice it matches is "Me"
    // (the naming rule), on the one Me row.
    for (matched_voice, seed) in [(0usize, 505u64), (3, 507)] {
        let dir = TempDir::new();
        let svc = services(&dir);
        let mic = meeting_with(&svc, &[TrackId::Mic])[0].clone();
        let meeting = mic.meeting_id;
        let mut post = room_post(2, false);
        let first = run_room(&mut post, &svc, &mic, room_scene(), seed);
        let voice = first.speakers[0].id;
        let user = crate::edit::mark_me(&svc, meeting, voice).unwrap();
        enroll_existing(&svc, user, matched_voice);

        let again = run_room(&mut post, &svc, &mic, room_scene(), seed + 1);
        assert_eq!(live_me_rows(&svc, meeting), [voice], "{matched_voice}");
        let mine: Vec<&PostSpeaker> = again
            .speakers
            .iter()
            .filter(|s| s.role == SpeakerRole::Me)
            .collect();
        assert_eq!(mine.len(), 1);
        assert_eq!(mine[0].id, voice);
        assert_eq!(mine[0].person, Some(user));
        assert_eq!(
            again
                .speakers
                .iter()
                .filter(|s| s.role == SpeakerRole::Room)
                .count(),
            1
        );
        // A voiceprint's Me row keeps no entry ("Me" is not recorded).
        assert!(entries(&svc, meeting).iter().all(|i| i.speaker != voice));
        // Every final segment names a live row.
        let live = live_rows(&svc, meeting);
        assert!(
            final_speakers(&svc, meeting)
                .iter()
                .all(|s| live.iter().any(|r| Some(r.id) == *s))
        );
    }
}

#[test]
fn post_rerun_after_undoing_an_edit_mark_names_the_voice_as_any_other() {
    let dir = TempDir::new();
    let svc = services(&dir);
    let mic = meeting_with(&svc, &[TrackId::Mic])[0].clone();
    let meeting = mic.meeting_id;
    let mut post = room_post(2, true);
    let first = run_room(&mut post, &svc, &mic, room_scene(), 509);
    let voice = first.speakers[0].id;
    crate::edit::mark_me(&svc, meeting, voice).unwrap();
    crate::edit::unmark_me(&svc, meeting, voice).unwrap();

    let again = run_room(&mut post, &svc, &mic, room_scene(), 510);
    assert_eq!(names(&again), ["Speaker 1", "Speaker 2"]);
    assert_eq!(again.speakers[0].id, voice);
    assert_eq!(again.speakers[0].role, SpeakerRole::Room);
    assert_eq!(again.speakers[0].person, None);
    assert!(live_me_rows(&svc, meeting).is_empty());
    let live = live_rows(&svc, meeting);
    for i in entries(&svc, meeting) {
        let row = live.iter().find(|s| s.id == i.speaker).unwrap();
        assert!(!i.edited(row), "an undone mark is not an edit");
    }
}

#[test]
fn post_rerun_of_a_call_never_carries_an_edit_mark_record_onto_the_me_row() {
    // Data no build writes (marking refuses a call): an entry for a call's Me row. The pass
    // treats it as it always did: not a candidate, and "Me" still comes from the mic.
    let dir = TempDir::new();
    let svc = services(&dir);
    let (meeting, mic, them) = meeting_rows(&svc);
    let (mic_audio, them_audio) = scene();
    let script = || {
        Ok(vec![
            raw(&[("hello", 1_000, 1_400)]),
            raw(&[("thanks", 31_000, 31_500)]),
        ])
    };
    let far = || Ok(vec![raw(&[("hi", 13_000, 13_300)])]);
    let mut post = PostProcessor::new(
        FakeTranscriber::scripted([script(), far(), script(), far()]),
        FakeSegmenter::segmentation_3_0(),
        embedder(),
        PostConfig {
            enroll_me: false,
            ..config()
        },
    )
    .unwrap();
    let run = |post: &mut RoomPost, seed| {
        post.run(
            &svc,
            PostInput {
                echo_locks: Vec::new(),
                meeting,
                tracks: vec![
                    track_input(&mic, mic_audio.clone()),
                    track_input(&them, them_audio.clone()),
                ],
            },
            &mut ChaCha8Rng::seed_from_u64(seed),
            &CancelToken::new(),
        )
        .unwrap()
    };
    let first = run(&mut post, 511);
    let me = first
        .speakers
        .iter()
        .find(|s| s.role == SpeakerRole::Me)
        .unwrap()
        .id;
    let mut record = entries(&svc, meeting);
    let mut stray = record[0].clone();
    stray.speaker = me;
    stray.track = TrackId::Mic;
    stray.user_edited = true;
    record.push(stray);
    // Test set-up only: production writes this record through the runtime's registry.
    svc.store()
        .set_local_value(
            record_key(meeting).as_str(),
            &crate::identities::encode(&crate::identities::IdentityRecord { speakers: record }),
        )
        .unwrap();
    let again = run(&mut post, 512);
    assert_eq!(live_me_rows(&svc, meeting), [me]);
    assert_eq!(
        again
            .speakers
            .iter()
            .filter(|s| s.role == SpeakerRole::Me)
            .count(),
        1
    );
}
