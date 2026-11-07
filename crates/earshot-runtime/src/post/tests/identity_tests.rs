//! T6.25: speaker names and Person links survive a re-run of the cleanup. Every name
//! contains `post`.

use super::*;
use crate::identities::{SpeakerIdentity, StoredIdentities, decode, record_key};

/// The user renames speaker row `id` of `meeting` and links it to `person`, as the app does.
fn edit_speaker(
    svc: &Services,
    meeting: EntityId,
    id: EntityId,
    name: &str,
    person: Option<EntityId>,
) {
    let clock = ManualClock::new(1_760_000_100_000);
    let mut db = svc.store();
    let mut s = db
        .speakers(meeting)
        .unwrap()
        .into_iter()
        .find(|s| s.id == id)
        .unwrap();
    s.display_name = name.into();
    s.person_id = person;
    let (store, hlc) = db.parts();
    s.meta.touch(hlc, &clock);
    store.put_speaker(s).unwrap();
}

/// A person without a voiceprint (the user created them to link a speaker).
fn new_person(svc: &Services, name: &str) -> EntityId {
    let clock = ManualClock::new(1_760_000_100_000);
    let mut rng = ChaCha8Rng::seed_from_u64(70);
    let mut db = svc.store();
    let (store, hlc) = db.parts();
    let mut stamp = Stamp {
        clock: &clock,
        hlc,
        rng: &mut rng,
    };
    let p = Person::new(name, false, &mut stamp);
    store.put_person(p.clone()).unwrap();
    p.id
}

/// `runs` copies of one script (mic, then reference, per run).
fn transcriber_runs(runs: usize) -> FakeTranscriber {
    let one = || {
        [
            Ok(vec![
                raw(&[("hello", 1_000, 1_400), ("all", 1_500, 1_900)]),
                raw(&[("thanks", 31_000, 31_500)]),
            ]),
            Ok(vec![
                raw(&[("hi", 13_000, 13_300)]),
                raw(&[("there", 22_000, 22_300)]),
                raw(&[("bye", 41_000, 41_300)]),
            ]),
        ]
    };
    FakeTranscriber::scripted((0..runs).flat_map(|_| one()))
}

fn run_both(
    post: &mut PostProcessor<FakeTranscriber, FakeSegmenter, FakeEmbedder>,
    svc: &Services,
    meeting: EntityId,
    tracks: [(&Track, MonoBuffer); 2],
    seed: u64,
) -> PostOutcome {
    post.run(
        svc,
        PostInput {
            echo_locks: Vec::new(),
            meeting,
            tracks: tracks.into_iter().map(|(t, a)| track_input(t, a)).collect(),
        },
        &mut ChaCha8Rng::seed_from_u64(seed),
        &CancelToken::new(),
    )
    .unwrap()
}

fn names(outcome: &PostOutcome) -> Vec<&str> {
    outcome
        .speakers
        .iter()
        .map(|s| s.display_name.as_str())
        .collect()
}

fn identities_of(svc: &Services, meeting: EntityId) -> Vec<SpeakerIdentity> {
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

/// Every final segment names a live speaker row.
fn assert_finals_name_live_speakers(svc: &Services, meeting: EntityId) {
    let speakers = live_speakers(svc, meeting);
    for s in finals_of(svc, meeting) {
        assert!(
            speakers.iter().any(|sp| Some(sp.id) == s.speaker_id),
            "{s:?}"
        );
    }
}

#[test]
fn post_rerun_keeps_a_renamed_speaker_and_a_person_link() {
    let dir = TempDir::new();
    let svc = services(&dir);
    let (meeting, mic, them) = meeting_rows(&svc);
    let (mic_audio, them_audio) = scene();
    let mut post = PostProcessor::new(
        transcriber_runs(2),
        FakeSegmenter::segmentation_3_0(),
        embedder(),
        config(),
    )
    .unwrap();
    let both = || [(&mic, mic_audio.clone()), (&them, them_audio.clone())];
    let first = run_both(&mut post, &svc, meeting, both(), 80);
    assert_eq!(names(&first), ["Me", "Speaker 1", "Speaker 2"]);
    // The identities of the non-"Me" speakers are kept for a re-run.
    let ids: Vec<EntityId> = identities_of(&svc, meeting)
        .iter()
        .map(|i| i.speaker)
        .collect();
    assert_eq!(ids, [first.speakers[1].id, first.speakers[2].id]);

    // The user renames voice 1 and links voice 2 to a person.
    edit_speaker(&svc, meeting, first.speakers[1].id, "Dana", None);
    let erin = new_person(&svc, "Erin");
    edit_speaker(&svc, meeting, first.speakers[2].id, "Erin", Some(erin));

    let again = run_both(&mut post, &svc, meeting, both(), 81);
    let named: Vec<(SpeakerRole, &str, Option<EntityId>)> = again
        .speakers
        .iter()
        .map(|s| (s.role, s.display_name.as_str(), s.person))
        .collect();
    assert_eq!(
        named,
        [
            (SpeakerRole::Me, "Me", first.speakers[0].person),
            (SpeakerRole::Remote, "Dana", None),
            (SpeakerRole::Remote, "Erin", Some(erin)),
        ]
    );
    // The same rows: the user's edit is not rewritten, "Me" stays the Me row.
    let same: Vec<EntityId> = first.speakers.iter().map(|s| s.id).collect();
    assert_eq!(
        again.speakers.iter().map(|s| s.id).collect::<Vec<_>>(),
        same
    );
    let live = live_speakers(&svc, meeting);
    assert_eq!(live.len(), 3);
    let row = |id| live.iter().find(|s| s.id == id).unwrap().clone();
    assert_eq!(row(same[1]).display_name, "Dana");
    assert_eq!(
        (row(same[2]).display_name, row(same[2]).person_id),
        ("Erin".to_string(), Some(erin))
    );
    assert_finals_name_live_speakers(&svc, meeting);
    assert!(again.warnings.is_empty(), "{:?}", again.warnings);
    // Still seen as edited by the next run: the record keeps what the pass gave.
    let record = identities_of(&svc, meeting);
    assert_eq!(record.len(), 2);
    assert!(record.iter().all(|i| i.edited(&row(i.speaker))));
}

#[test]
fn post_rerun_merge_keeps_the_renamed_speaker() {
    // Two voices in the first run; the re-run (a much looser clustering) finds one cluster
    // close to both. The one the user renamed keeps its row and name; the other row is
    // tombstoned (without an edit, the closer centroid would win:
    // `identities::match_previous`).
    let dir = TempDir::new();
    let svc = services(&dir);
    let (meeting, mic, them) = meeting_rows(&svc);
    let (mic_audio, them_audio) = scene();
    let mut post = PostProcessor::new(
        transcriber_runs(1),
        FakeSegmenter::segmentation_3_0(),
        embedder(),
        config(),
    )
    .unwrap();
    let first = run_both(
        &mut post,
        &svc,
        meeting,
        [(&mic, mic_audio.clone()), (&them, them_audio.clone())],
        82,
    );
    let (voice1, voice2) = (first.speakers[1].id, first.speakers[2].id);
    edit_speaker(&svc, meeting, voice2, "Bob", None);

    let mut merging = PostProcessor::new(
        transcriber_runs(1),
        FakeSegmenter::segmentation_3_0(),
        embedder(),
        PostConfig {
            clustering_thresholds: ModelThresholds::new().with(EmbeddingModelId::new(MODEL), 1.9),
            ..config()
        },
    )
    .unwrap();
    let again = run_both(
        &mut merging,
        &svc,
        meeting,
        [(&mic, mic_audio), (&them, them_audio)],
        83,
    );
    let roles: Vec<(SpeakerRole, &str)> = again
        .speakers
        .iter()
        .map(|s| (s.role, s.display_name.as_str()))
        .collect();
    assert_eq!(
        roles,
        [(SpeakerRole::Me, "Me"), (SpeakerRole::Remote, "Bob")]
    );
    assert_eq!(again.speakers[1].id, voice2);
    let live = live_speakers(&svc, meeting);
    assert!(
        live.iter().all(|s| s.id != voice1),
        "the merged-away row is gone"
    );
    assert_eq!(live.len(), 2);
    assert_finals_name_live_speakers(&svc, meeting);
    let record: Vec<EntityId> = identities_of(&svc, meeting)
        .iter()
        .map(|i| i.speaker)
        .collect();
    assert_eq!(
        record,
        [voice2],
        "the tombstoned speaker's centroid is dropped"
    );
}

#[test]
fn post_rerun_new_speaker_is_numbered_densely_around_carried_names() {
    // First run: voices 1 and 2. The user names voice 1 "Dana" and voice 2 "Speaker 1".
    // The re-run hears a third voice: it is "Speaker 2", never a second "Speaker 1".
    let dir = TempDir::new();
    let svc = services(&dir);
    let (meeting, mic, them) = meeting_rows(&svc);
    let (mic_audio, them_audio) = scene();
    let mic_script = || {
        Ok(vec![
            raw(&[("hello", 1_000, 1_400), ("all", 1_500, 1_900)]),
            raw(&[("thanks", 31_000, 31_500)]),
        ])
    };
    // Since T7.3b a voice needs a word to get a row: the third voice says "more".
    let mut post = PostProcessor::new(
        FakeTranscriber::scripted([
            mic_script(),
            Ok(vec![
                raw(&[("hi", 13_000, 13_300)]),
                raw(&[("there", 22_000, 22_300)]),
                raw(&[("bye", 41_000, 41_300)]),
            ]),
            mic_script(),
            Ok(vec![
                raw(&[("hi", 13_000, 13_300)]),
                raw(&[("there", 22_000, 22_300)]),
                raw(&[("more", 33_000, 33_300)]),
                raw(&[("bye", 41_000, 41_300)]),
            ]),
        ]),
        FakeSegmenter::segmentation_3_0(),
        embedder(),
        config(),
    )
    .unwrap();
    let first = run_both(
        &mut post,
        &svc,
        meeting,
        [(&mic, mic_audio.clone()), (&them, them_audio)],
        84,
    );
    let (voice1, voice2) = (first.speakers[1].id, first.speakers[2].id);
    edit_speaker(&svc, meeting, voice1, "Dana", None);
    edit_speaker(&svc, meeting, voice2, "Speaker 1", None);

    let three = scene_audio(
        &[
            SceneTurn::new(1, 12_000, 20_000),
            SceneTurn::new(2, 20_000, 30_000),
            SceneTurn::new(3, 30_000, 38_000),
            SceneTurn::new(1, 40_000, 44_000),
        ],
        45_000,
    );
    let again = run_both(
        &mut post,
        &svc,
        meeting,
        [(&mic, mic_audio), (&them, three)],
        85,
    );
    assert_eq!(names(&again), ["Me", "Dana", "Speaker 1", "Speaker 2"]);
    assert_eq!(again.speakers[1].id, voice1);
    assert_eq!(again.speakers[2].id, voice2);
    assert!(![voice1, voice2].contains(&again.speakers[3].id));
    assert_eq!(live_speakers(&svc, meeting).len(), 4);
    assert_eq!(identities_of(&svc, meeting).len(), 3);
    assert_finals_name_live_speakers(&svc, meeting);
}

#[test]
fn post_rerun_without_edits_renumbers_densely_and_keeps_the_rows() {
    // Nothing edited: the re-run names the speakers itself (dense numbering in meeting
    // order), on the same rows. Voice 1 is gone from the re-run, so voice 2 is now
    // "Speaker 1".
    let dir = TempDir::new();
    let svc = services(&dir);
    let (meeting, mic, them) = meeting_rows(&svc);
    let (mic_audio, them_audio) = scene();
    let mut post = PostProcessor::new(
        transcriber_runs(2),
        FakeSegmenter::segmentation_3_0(),
        embedder(),
        config(),
    )
    .unwrap();
    let first = run_both(
        &mut post,
        &svc,
        meeting,
        [(&mic, mic_audio.clone()), (&them, them_audio)],
        95,
    );
    let voice2 = first.speakers[2].id;
    let only_two = scene_audio(&[SceneTurn::new(2, 20_000, 30_000)], 45_000);
    let again = run_both(
        &mut post,
        &svc,
        meeting,
        [(&mic, mic_audio), (&them, only_two)],
        96,
    );
    assert_eq!(names(&again), ["Me", "Speaker 1"]);
    assert_eq!(again.speakers[1].id, voice2);
    let live = live_speakers(&svc, meeting);
    assert_eq!(live.len(), 2);
    assert_eq!(
        live.iter().find(|s| s.id == voice2).unwrap().display_name,
        "Speaker 1"
    );
    // Renamed by the pass, not by the user: not an edit for the next run.
    let record = identities_of(&svc, meeting);
    assert_eq!(record.len(), 1);
    assert!(!record[0].edited(live.iter().find(|s| s.id == voice2).unwrap()));
}

#[test]
fn post_rerun_keeps_a_speaker_renamed_back_to_its_automatic_name() {
    // The user renames voice 2 ("Speaker 2") to "Bob" and then back to "Speaker 2": the row
    // matches what the pass gave it, but it was edited, so the re-run keeps the name the
    // user chose instead of renumbering it (without the edit it would become "Speaker 1",
    // as in `post_rerun_without_edits_renumbers_densely_and_keeps_the_rows`).
    let dir = TempDir::new();
    let svc = services(&dir);
    let (meeting, mic, them) = meeting_rows(&svc);
    let (mic_audio, them_audio) = scene();
    let mut post = PostProcessor::new(
        transcriber_runs(2),
        FakeSegmenter::segmentation_3_0(),
        embedder(),
        config(),
    )
    .unwrap();
    let first = run_both(
        &mut post,
        &svc,
        meeting,
        [(&mic, mic_audio.clone()), (&them, them_audio)],
        97,
    );
    assert_eq!(names(&first), ["Me", "Speaker 1", "Speaker 2"]);
    let voice2 = first.speakers[2].id;
    edit_speaker(&svc, meeting, voice2, "Bob", None);
    edit_speaker(&svc, meeting, voice2, "Speaker 2", None);
    let row = live_speakers(&svc, meeting)
        .into_iter()
        .find(|s| s.id == voice2)
        .unwrap();
    let record = identities_of(&svc, meeting);
    let entry = record.iter().find(|i| i.speaker == voice2).unwrap();
    assert_eq!(
        (row.display_name.as_str(), row.person_id),
        (entry.given_name.as_str(), entry.given_person),
        "the row reads as the pass left it"
    );
    assert!(entry.edited(&row), "but it was edited");

    let only_two = scene_audio(&[SceneTurn::new(2, 20_000, 30_000)], 45_000);
    let again = run_both(
        &mut post,
        &svc,
        meeting,
        [(&mic, mic_audio), (&them, only_two)],
        98,
    );
    assert_eq!(names(&again), ["Me", "Speaker 2"]);
    assert_eq!(again.speakers[1].id, voice2);
    // Still an edit for the run after.
    let row = live_speakers(&svc, meeting)
        .into_iter()
        .find(|s| s.id == voice2)
        .unwrap();
    let record = identities_of(&svc, meeting);
    assert_eq!(record.len(), 1);
    assert!(record[0].edited(&row));
}

#[test]
fn post_rerun_with_another_embedding_model_carries_nothing_and_says_so() {
    let dir = TempDir::new();
    let svc = services(&dir);
    let (meeting, mic, them) = meeting_rows(&svc);
    let (mic_audio, them_audio) = scene();
    let mut post = PostProcessor::new(
        transcriber_runs(1),
        FakeSegmenter::segmentation_3_0(),
        embedder(),
        config(),
    )
    .unwrap();
    let first = run_both(
        &mut post,
        &svc,
        meeting,
        [(&mic, mic_audio.clone()), (&them, them_audio.clone())],
        86,
    );
    edit_speaker(&svc, meeting, first.speakers[1].id, "Dana", None);

    const OTHER: &str = "fake-emb@2";
    let mut other = PostProcessor::new(
        transcriber_runs(1),
        FakeSegmenter::segmentation_3_0(),
        FakeEmbedder::new(
            OTHER,
            (0..MAX_SCENE_SPEAKERS).map(centroid).collect(),
            0.05,
            11,
        ),
        PostConfig {
            clustering_thresholds: ModelThresholds::new()
                .with(EmbeddingModelId::new(OTHER), 0.704_565_5),
            voiceprint_thresholds: ModelThresholds::new().with(EmbeddingModelId::new(OTHER), 0.5),
            ..config()
        },
    )
    .unwrap();
    let again = run_both(
        &mut other,
        &svc,
        meeting,
        [(&mic, mic_audio), (&them, them_audio)],
        87,
    );
    assert_eq!(names(&again), ["Me", "Speaker 1", "Speaker 2"]);
    // "Me" is still the Me row; the others are new rows.
    assert_eq!(again.speakers[0].id, first.speakers[0].id);
    assert!(
        again.speakers[1..]
            .iter()
            .all(|s| first.speakers.iter().all(|f| f.id != s.id))
    );
    let reasons: Vec<&str> = again
        .warnings
        .iter()
        .filter_map(|w| match w {
            PostWarning::SpeakerNamesNotCarried { reason } => Some(reason.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(reasons.len(), 1, "{:?}", again.warnings);
    assert!(
        reasons[0].contains(MODEL) && reasons[0].contains(OTHER),
        "{}",
        reasons[0]
    );
    let record = identities_of(&svc, meeting);
    assert_eq!(record.len(), 2);
    assert!(
        record
            .iter()
            .all(|i| i.centroid.model == EmbeddingModelId::new(OTHER))
    );
    assert_eq!(live_speakers(&svc, meeting).len(), 3);
}

#[test]
fn post_rerun_with_an_unreadable_identity_record_carries_nothing_and_says_so() {
    let dir = TempDir::new();
    let svc = services(&dir);
    let (meeting, mic, them) = meeting_rows(&svc);
    let (mic_audio, them_audio) = scene();
    let mut post = PostProcessor::new(
        transcriber_runs(2),
        FakeSegmenter::segmentation_3_0(),
        embedder(),
        config(),
    )
    .unwrap();
    let both = || [(&mic, mic_audio.clone()), (&them, them_audio.clone())];
    let first = run_both(&mut post, &svc, meeting, both(), 88);
    edit_speaker(&svc, meeting, first.speakers[1].id, "Dana", None);
    svc.store()
        .set_local_value(record_key(meeting).as_str(), &[9, 0, 0, 0, b'{'])
        .unwrap();
    let again = run_both(&mut post, &svc, meeting, both(), 89);
    assert_eq!(again.speakers[1].display_name, "Speaker 1");
    assert!(again.warnings.iter().any(|w| matches!(
        w,
        PostWarning::SpeakerNamesNotCarried { reason } if reason.contains("version 9")
    )));
    // A fresh record replaces the unreadable one.
    assert_eq!(identities_of(&svc, meeting).len(), 2);
}

#[test]
fn post_rerun_as_a_whole_pass_through_flac_keeps_the_renamed_speaker() {
    // The real path: live session -> FLAC -> load_post_input -> run, then the user renames
    // a speaker and the cleanup runs again as the whole-meeting pass (`reuse_live` off),
    // with fakes keyed by level so they survive 16-bit quantisation.
    let dir = TempDir::new();
    let svc = services(&dir);
    let mic = levels(20_000, &[(0.1, 1_000, 6_000)], 90);
    let them = levels(20_000, &[(0.4, 7_000, 12_000), (0.7, 13_000, 18_000)], 91);
    let source = ReplayBuilder::new(
        DeviceTime::from_ticks(5_000_000),
        MonoBuffer::new(PIPELINE_RATE, mic),
    )
    .reference(MonoBuffer::new(PIPELINE_RATE, them))
    .build()
    .unwrap();
    let live = start_live(
        source,
        svc.clone(),
        LiveConfig {
            clock_domain: ClockDomain::Replay,
            idle_wait: Duration::from_millis(1),
            ..LiveConfig::default()
        },
        None,
        Box::new(ChaCha8Rng::seed_from_u64(92)),
    )
    .unwrap();
    let outcome = live.wait().unwrap();
    let meeting = outcome.meeting;

    let model = EmbeddingModelId::new("level@1");
    let run = |reuse_live: bool, seed: u64| {
        let (input, missing) = load_post_input(&svc, meeting, &outcome.final_pass).unwrap();
        assert!(missing.is_empty());
        PostProcessor::new(
            FakeTranscriber::scripted([
                Ok(vec![raw(&[("me", 3_000, 3_300)])]),
                Ok(vec![
                    raw(&[("first", 9_000, 9_300)]),
                    raw(&[("second", 15_000, 15_300)]),
                ]),
            ]),
            LevelSegmenter(earshot_core::diar::SegmenterInfo::pyannote_segmentation_3_0("level")),
            LevelEmbedder(earshot_core::diar::EmbedderInfo {
                model: model.clone(),
                dimension: 4,
                min_samples: 4000,
            }),
            PostConfig {
                clustering_thresholds: ModelThresholds::new().with(model.clone(), 0.704_565_5),
                voiceprint_thresholds: ModelThresholds::new().with(model.clone(), 0.5),
                reuse_live,
                ..PostConfig::default()
            },
        )
        .unwrap()
        .run(
            &svc,
            input,
            &mut ChaCha8Rng::seed_from_u64(seed),
            &CancelToken::new(),
        )
        .unwrap()
    };
    let first = run(true, 93);
    assert_eq!(names(&first), ["Me", "Speaker 1", "Speaker 2"]);
    edit_speaker(&svc, meeting, first.speakers[2].id, "Fran", None);

    let again = run(false, 94);
    assert!(again.speaker_windows.iter().all(|(_, u)| matches!(
        u,
        SpeakerWindowUse::Diarised { reason } if reason == WHOLE_PASS_REQUESTED
    )));
    assert_eq!(names(&again), ["Me", "Speaker 1", "Fran"]);
    assert_eq!(
        again.speakers.iter().map(|s| s.id).collect::<Vec<_>>(),
        first.speakers.iter().map(|s| s.id).collect::<Vec<_>>()
    );
    assert_finals_name_live_speakers(&svc, meeting);
}

#[test]
fn post_rerun_drops_a_named_speaker_left_without_words() {
    // Voice 2 spoke in the first run and the user named it; in the re-run no word of it is
    // transcribed (T7.3b): its row goes with its words, no row without segments is left,
    // and its centroid leaves the identity record.
    let dir = TempDir::new();
    let svc = services(&dir);
    let (meeting, mic, them) = meeting_rows(&svc);
    let (mic_audio, them_audio) = scene();
    let mic_script = || {
        Ok(vec![
            raw(&[("hello", 1_000, 1_400), ("all", 1_500, 1_900)]),
            raw(&[("thanks", 31_000, 31_500)]),
        ])
    };
    let mut post = PostProcessor::new(
        FakeTranscriber::scripted([
            mic_script(),
            Ok(vec![
                raw(&[("hi", 13_000, 13_300)]),
                raw(&[("there", 22_000, 22_300)]),
                raw(&[("bye", 41_000, 41_300)]),
            ]),
            mic_script(),
            Ok(vec![
                raw(&[("hi", 13_000, 13_300)]),
                raw(&[("bye", 41_000, 41_300)]),
            ]),
        ]),
        FakeSegmenter::segmentation_3_0(),
        embedder(),
        config(),
    )
    .unwrap();
    let both = || [(&mic, mic_audio.clone()), (&them, them_audio.clone())];
    let first = run_both(&mut post, &svc, meeting, both(), 90);
    assert_eq!(names(&first), ["Me", "Speaker 1", "Speaker 2"]);
    let named = first.speakers[2].id;
    edit_speaker(&svc, meeting, named, "Robin", None);

    let second = run_both(&mut post, &svc, meeting, both(), 91);
    assert_eq!(names(&second), ["Me", "Speaker 1"]);
    assert!(
        second.warnings.iter().any(|w| matches!(
            w,
            PostWarning::VoiceWithoutWords {
                track: TrackId::Reference,
                ..
            }
        )),
        "{:?}",
        second.warnings
    );
    let speakers = live_speakers(&svc, meeting);
    assert!(speakers.iter().all(|s| s.id != named));
    let finals = finals_of(&svc, meeting);
    for s in &speakers {
        assert!(
            finals.iter().any(|f| f.speaker_id == Some(s.id)),
            "{}",
            s.display_name
        );
    }
    assert!(
        identities_of(&svc, meeting)
            .iter()
            .all(|i| i.speaker != named)
    );
    assert_finals_name_live_speakers(&svc, meeting);
}
