//! Post-meeting tests (fakes only). Every name contains `post`.

use std::sync::Arc;
use std::time::Duration;

use earshot_core::asr::{
    AsrError, CancelToken, FakeTranscriber, RawSegment, TranscribeOptions, Word,
};
use earshot_core::audio::{MonoBuffer, PIPELINE_RATE};
use earshot_core::capture::{DeviceTime, ReplayBuilder, TrackId};
use earshot_core::diar::{
    DiarizationConfig, EmbeddingModelId, FakeEmbedder, FakeSegmenter, MAX_SCENE_SPEAKERS,
    SceneTurn, SpeakerRole, scene_audio,
};
use earshot_core::identity::{EntityId, ManualClock};
use earshot_core::model::{
    ClockDomain, Meeting, MeetingRepo, Stamp, Track, TranscriptPass, live_segments,
};
use earshot_core::prompt::Template;
use earshot_core::voiceprint::{EnrolmentConfig, Person, VoiceprintRepo, enroll, match_embedding};
use earshot_store::{DataDir, Store};
use rand_chacha::ChaCha8Rng;
use rand_core::SeedableRng;

use super::*;
use crate::live::{LiveConfig, start_live};
use crate::{RuntimeError, Services};

const MODEL: &str = "fake-emb@1";
const DIM: usize = 16;

pub(super) struct TempDir(std::path::PathBuf);

impl TempDir {
    pub(super) fn new() -> Self {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "earshot-runtime-post-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::SeqCst)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

pub(super) fn services(dir: &TempDir) -> Services {
    let data = DataDir::new(&dir.0);
    data.create().unwrap();
    let store = Store::open(&data.db_path()).unwrap();
    Services::new(store, data, Arc::new(ManualClock::new(1_760_000_000_000))).unwrap()
}

/// One-hot centroid of scene speaker `k` (what the fake embedder produces, up to noise).
fn centroid(k: usize) -> Vec<f32> {
    (0..DIM).map(|i| if i == k { 1.0 } else { 0.0 }).collect()
}

fn embedder() -> FakeEmbedder {
    FakeEmbedder::new(
        MODEL,
        (0..MAX_SCENE_SPEAKERS).map(centroid).collect(),
        0.05,
        11,
    )
}

fn config() -> PostConfig {
    PostConfig {
        diarization: DiarizationConfig::default(),
        clustering_thresholds: ModelThresholds::new()
            .with(EmbeddingModelId::new(MODEL), 0.704_565_5),
        voiceprint_thresholds: ModelThresholds::new().with(EmbeddingModelId::new(MODEL), 0.5),
        enroll_me: true,
        enrolment: EnrolmentConfig::default(),
        reuse_live: true,
    }
}

/// A meeting row with a mic and a reference track.
pub(super) fn meeting_rows(svc: &Services) -> (EntityId, Track, Track) {
    let tracks = meeting_with(svc, &[TrackId::Mic, TrackId::Reference]);
    (tracks[0].meeting_id, tracks[0].clone(), tracks[1].clone())
}

/// A meeting with tracks of `kinds` (distinct ids on every call).
fn meeting_with(svc: &Services, kinds: &[TrackId]) -> Vec<Track> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEED: AtomicU64 = AtomicU64::new(1000);
    let clock = ManualClock::new(1_760_000_000_000);
    let mut rng = ChaCha8Rng::seed_from_u64(SEED.fetch_add(1, Ordering::SeqCst));
    let mut db = svc.store();
    let (store, hlc) = db.parts();
    let mut stamp = Stamp {
        clock: &clock,
        hlc,
        rng: &mut rng,
    };
    let m = Meeting::new(
        "Planning",
        1_760_000_000_000,
        0,
        Template::General,
        &mut stamp,
    );
    store.put_meeting(m.clone()).unwrap();
    kinds
        .iter()
        .map(|k| {
            let t = Track::new(m.id, *k, None, ClockDomain::Replay, &mut stamp);
            store.put_track(t.clone()).unwrap();
            t
        })
        .collect()
}

/// Enrols "Alice" with scene speaker `k`'s voice.
fn enroll_alice(svc: &Services, k: usize) -> EntityId {
    let clock = ManualClock::new(1_760_000_000_000);
    let mut rng = ChaCha8Rng::seed_from_u64(2);
    let mut db = svc.store();
    let (store, hlc) = db.parts();
    let mut stamp = Stamp {
        clock: &clock,
        hlc,
        rng: &mut rng,
    };
    let alice = Person::new("Alice", false, &mut stamp);
    store.put_person(alice.clone()).unwrap();
    let e = earshot_core::diar::Embedding::new(EmbeddingModelId::new(MODEL), centroid(k));
    enroll(
        &mut *store,
        alice.id,
        &[e],
        Duration::from_secs(20),
        &EnrolmentConfig::default(),
        &mut stamp,
    )
    .unwrap();
    alice.id
}

pub(crate) fn raw(words: &[(&str, u64, u64)]) -> RawSegment {
    RawSegment::from_words(
        words
            .iter()
            .map(|(t, s, e)| Word::new(*t, *s, *e, 0.9))
            .collect(),
    )
}

pub(super) fn track_input(track: &Track, audio: MonoBuffer) -> PostTrack {
    PostTrack {
        track: track.clone(),
        audio,
        options: TranscribeOptions::batch(),
        stretches: None,
        speakers: None,
    }
}

/// Mic: "Me" (scene speaker 0) at 0-12 s and 30-40 s. Loopback: Alice (speaker 1) at
/// 12-20 s and 40-44 s, an unknown voice (speaker 2) at 20-30 s.
fn scene() -> (MonoBuffer, MonoBuffer) {
    let mic = scene_audio(
        &[
            SceneTurn::new(0, 0, 12_000),
            SceneTurn::new(0, 30_000, 40_000),
        ],
        45_000,
    );
    let them = scene_audio(
        &[
            SceneTurn::new(1, 12_000, 20_000),
            SceneTurn::new(2, 20_000, 30_000),
            SceneTurn::new(1, 40_000, 44_000),
        ],
        45_000,
    );
    (mic, them)
}

fn transcriber() -> FakeTranscriber {
    // Called once per track, mic first (the request's order).
    FakeTranscriber::scripted([
        Ok(vec![
            raw(&[("hello", 1_000, 1_400), ("all", 1_500, 1_900)]),
            raw(&[("thanks", 31_000, 31_500)]),
        ]),
        Ok(vec![
            raw(&[("hi", 13_000, 13_300)]),
            // One segment across the 20 s speaker change: split by speaker.
            raw(&[("see", 19_000, 19_300), ("you", 21_000, 21_300)]),
            raw(&[("bye", 41_000, 41_300)]),
        ]),
    ])
}

#[test]
fn post_final_pass_diarises_names_and_stores_speakers() {
    let dir = TempDir::new();
    let svc = services(&dir);
    let (meeting, mic, them) = meeting_rows(&svc);
    let alice = enroll_alice(&svc, 1);
    let (mic_audio, them_audio) = scene();
    let mut post = PostProcessor::new(
        transcriber(),
        FakeSegmenter::segmentation_3_0(),
        embedder(),
        config(),
    )
    .unwrap();
    let input = PostInput {
        echo_locks: Vec::new(),
        meeting,
        tracks: vec![track_input(&mic, mic_audio), track_input(&them, them_audio)],
    };
    let mut rng = ChaCha8Rng::seed_from_u64(3);
    let outcome = post
        .run(&svc, input, &mut rng, &CancelToken::new())
        .unwrap();

    let roles: Vec<(SpeakerRole, TrackId, &str)> = outcome
        .speakers
        .iter()
        .map(|s| (s.role, s.track, s.display_name.as_str()))
        .collect();
    assert_eq!(
        roles,
        [
            (SpeakerRole::Me, TrackId::Mic, "Me"),
            (SpeakerRole::Remote, TrackId::Reference, "Alice"),
            (SpeakerRole::Remote, TrackId::Reference, "Speaker 1"),
        ]
    );
    assert_eq!(outcome.speakers[1].person, Some(alice));
    assert_eq!(outcome.speakers[2].person, None);
    assert!(outcome.warnings.is_empty(), "{:?}", outcome.warnings);

    let store = svc.store();
    let speakers = store.speakers(meeting).unwrap();
    assert_eq!(speakers.len(), 3);
    let me = &outcome.speakers[0];
    assert!(me.person.is_some());
    assert!(store.person(me.person.unwrap()).unwrap().unwrap().is_me);

    // Final pass stored, labelled; the cross-speaker segment was split.
    let finals = live_segments(&**store, meeting, TranscriptPass::Final).unwrap();
    let labelled: Vec<(String, EntityId, EntityId)> = finals
        .iter()
        .map(|r| {
            (
                r.segment.text.clone(),
                r.segment.track,
                r.segment
                    .speaker_id
                    .expect("every final segment has a speaker"),
            )
        })
        .collect();
    let id = |k: usize| outcome.speakers[k].id;
    assert_eq!(
        labelled,
        [
            ("hello all".into(), mic.id, id(0)),
            ("hi".into(), them.id, id(1)),
            ("see".into(), them.id, id(1)),
            ("you".into(), them.id, id(2)),
            ("thanks".into(), mic.id, id(0)),
            ("bye".into(), them.id, id(1)),
        ]
    );
    assert_eq!(outcome.final_segments, 6);

    // "Me" was enrolled (22 s of mic speech) and is now recognised.
    let vp = outcome.enrolled_me.expect("Me enrolled");
    let prints = store.voiceprints_of(me.person.unwrap()).unwrap();
    assert!(prints.iter().any(|p| p.id == vp));
    let probe = earshot_core::diar::Embedding::new(EmbeddingModelId::new(MODEL), centroid(0));
    assert!(matches!(
        match_embedding(&**store, &probe, 0.5).unwrap(),
        earshot_core::voiceprint::MatchOutcome::Known { person, .. } if Some(person) == me.person
    ));
}

#[test]
fn post_without_calibration_is_an_error_at_construction() {
    let missing_clustering = PostConfig {
        clustering_thresholds: ModelThresholds::new(),
        ..config()
    };
    let err = PostProcessor::new(
        transcriber(),
        FakeSegmenter::segmentation_3_0(),
        embedder(),
        missing_clustering,
    )
    .unwrap_err();
    assert!(matches!(
        &err,
        RuntimeError::Uncalibrated { purpose: Calibration::Clustering, model } if model.as_str() == MODEL
    ));
    let missing_voiceprint = PostConfig {
        voiceprint_thresholds: ModelThresholds::new(),
        ..config()
    };
    assert!(matches!(
        PostProcessor::new(
            transcriber(),
            FakeSegmenter::segmentation_3_0(),
            embedder(),
            missing_voiceprint,
        ),
        Err(RuntimeError::Uncalibrated {
            purpose: Calibration::Voiceprint,
            ..
        })
    ));
    // The shipped defaults come from the core dev-data calibrations (CAM++ only); other
    // models stay uncalibrated (0.7046 is WeSpeaker's and is never a fallback).
    let defaults = PostConfig::default();
    let campp = EmbeddingModelId::new("campplus-en-voxceleb@357a834f702b");
    assert!(
        defaults.voiceprint_threshold(&campp).is_ok(),
        "from the core calibration"
    );
    let clustering = defaults.clustering_threshold(&campp).unwrap();
    assert_eq!(
        Some(clustering),
        earshot_core::diar::cluster::calibrated_cluster_threshold(&campp)
    );
    assert_ne!(clustering, 0.704_565_5);
    assert!(matches!(
        defaults.clustering_threshold(&EmbeddingModelId::new("unknown@1")),
        Err(RuntimeError::Uncalibrated {
            purpose: Calibration::Clustering,
            ..
        })
    ));
    assert!(!defaults.enroll_me, "biometric enrolment is opt-in");
}

#[test]
fn post_single_track_uses_the_in_person_policy_and_respects_enrolment_opt_out() {
    let dir = TempDir::new();
    let svc = services(&dir);
    let tracks = meeting_with(&svc, &[TrackId::Mic]);
    let (meeting, mic) = (tracks[0].meeting_id, tracks[0].clone());
    let room = scene_audio(
        &[
            SceneTurn::new(0, 0, 10_000),
            SceneTurn::new(3, 10_000, 20_000),
        ],
        20_000,
    );
    let mut post = PostProcessor::new(
        FakeTranscriber::scripted([Ok(vec![
            raw(&[("one", 2_000, 2_300)]),
            raw(&[("two", 12_000, 12_300)]),
        ])]),
        FakeSegmenter::segmentation_3_0(),
        embedder(),
        PostConfig {
            enroll_me: false,
            ..config()
        },
    )
    .unwrap();
    let outcome = post
        .run(
            &svc,
            PostInput {
                echo_locks: Vec::new(),
                meeting,
                tracks: vec![track_input(&mic, room)],
            },
            &mut ChaCha8Rng::seed_from_u64(4),
            &CancelToken::new(),
        )
        .unwrap();
    let roles: Vec<(SpeakerRole, &str)> = outcome
        .speakers
        .iter()
        .map(|s| (s.role, s.display_name.as_str()))
        .collect();
    assert_eq!(
        roles,
        [
            (SpeakerRole::Room, "Speaker 1"),
            (SpeakerRole::Room, "Speaker 2")
        ]
    );
    assert!(outcome.enrolled_me.is_none());
    let store = svc.store();
    assert!(
        store.persons().unwrap().is_empty(),
        "no Me person without enrolment or a match"
    );
}

#[test]
fn post_in_person_mic_voice_matching_the_users_voiceprint_is_me() {
    // In the room (one mic track) no voice is assumed to be the user; the voice that
    // matches the user's voiceprint is "Me", another enrolled person keeps their name.
    let dir = TempDir::new();
    let svc = services(&dir);
    let user = enroll_person(&svc, "Casey", true, 3);
    let colleague = enroll_person(&svc, "Alex", false, 0);
    let tracks = meeting_with(&svc, &[TrackId::Mic]);
    let (meeting, mic) = (tracks[0].meeting_id, tracks[0].clone());
    let room = scene_audio(
        &[
            SceneTurn::new(0, 0, 10_000),
            SceneTurn::new(3, 10_000, 20_000),
            SceneTurn::new(5, 20_000, 30_000),
        ],
        30_000,
    );
    let mut post = PostProcessor::new(
        FakeTranscriber::scripted([Ok(vec![
            raw(&[("one", 2_000, 2_300)]),
            raw(&[("two", 12_000, 12_300)]),
            raw(&[("three", 22_000, 22_300)]),
        ])]),
        FakeSegmenter::segmentation_3_0(),
        embedder(),
        PostConfig {
            enroll_me: false,
            ..config()
        },
    )
    .unwrap();
    let outcome = post
        .run(
            &svc,
            PostInput {
                echo_locks: Vec::new(),
                meeting,
                tracks: vec![track_input(&mic, room)],
            },
            &mut ChaCha8Rng::seed_from_u64(4),
            &CancelToken::new(),
        )
        .unwrap();
    let roles: Vec<(SpeakerRole, &str, Option<EntityId>)> = outcome
        .speakers
        .iter()
        .map(|s| (s.role, s.display_name.as_str(), s.person))
        .collect();
    assert_eq!(
        roles,
        [
            (SpeakerRole::Room, "Alex", Some(colleague)),
            // The user's own person, shown by its name.
            (SpeakerRole::Me, "Casey", Some(user)),
            (SpeakerRole::Room, "Speaker 1", None),
        ]
    );
    assert!(outcome.enrolled_me.is_none(), "enrolment stays opt-in");
}

#[test]
fn post_failures_leave_the_live_transcript_and_report_the_stage() {
    let dir = TempDir::new();
    let svc = services(&dir);
    let (meeting, mic, them) = meeting_rows(&svc);
    let (mic_audio, them_audio) = scene();
    let input = || PostInput {
        echo_locks: Vec::new(),
        meeting,
        tracks: vec![
            track_input(&mic, mic_audio.clone()),
            track_input(&them, them_audio.clone()),
        ],
    };
    // The decoder fails: nothing is written.
    let mut post = PostProcessor::new(
        FakeTranscriber::scripted([Err(AsrError::Backend("gpu lost".into()))]),
        FakeSegmenter::segmentation_3_0(),
        embedder(),
        config(),
    )
    .unwrap();
    let err = post
        .run(
            &svc,
            input(),
            &mut ChaCha8Rng::seed_from_u64(5),
            &CancelToken::new(),
        )
        .unwrap_err();
    assert!(matches!(err, RuntimeError::Asr(AsrError::Backend(_))));
    // Diarisation fails: same.
    let mut post = PostProcessor::new(
        transcriber(),
        FakeSegmenter::segmentation_3_0().failing("segmenter crashed"),
        embedder(),
        config(),
    )
    .unwrap();
    let err = post
        .run(
            &svc,
            input(),
            &mut ChaCha8Rng::seed_from_u64(6),
            &CancelToken::new(),
        )
        .unwrap_err();
    assert!(matches!(err, RuntimeError::Diar(_)), "{err:?}");
    // Cancelled before it starts.
    let cancel = CancelToken::new();
    cancel.cancel();
    let mut post = PostProcessor::new(
        transcriber(),
        FakeSegmenter::segmentation_3_0(),
        embedder(),
        config(),
    )
    .unwrap();
    assert!(matches!(
        post.run(&svc, input(), &mut ChaCha8Rng::seed_from_u64(7), &cancel),
        Err(RuntimeError::Asr(AsrError::Cancelled))
    ));
    let store = svc.store();
    assert!(store.speakers(meeting).unwrap().is_empty());
    assert!(
        live_segments(&**store, meeting, TranscriptPass::Final)
            .unwrap()
            .is_empty()
    );
}

#[test]
fn post_input_loads_the_recorded_tracks_of_a_live_session() {
    let dir = TempDir::new();
    let svc = services(&dir);
    let mic = vec![0.25f32; 16_000];
    let them = vec![-0.5f32; 16_000];
    let origin = DeviceTime::from_ticks(1_000_000);
    let source = ReplayBuilder::new(origin, MonoBuffer::new(PIPELINE_RATE, mic))
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
        Box::new(ChaCha8Rng::seed_from_u64(8)),
    )
    .unwrap();
    let outcome = live.wait().unwrap();
    let (input, missing) = load_post_input(&svc, outcome.meeting, &outcome.final_pass).unwrap();
    assert!(missing.is_empty());
    assert_eq!(input.meeting, outcome.meeting);
    // The echo delay the live session ended with travels to the final pass.
    assert_eq!(input.echo_locks, outcome.echo_locks);
    let lock = crate::echo::EchoLock {
        from_ms: 0,
        to_ms: 900,
        delay_ms: 230,
    };
    crate::echo::record_echo_locks(&mut svc.store(), outcome.meeting, &[lock]).unwrap();
    let (relock, _) = load_post_input(&svc, outcome.meeting, &outcome.final_pass).unwrap();
    assert_eq!(relock.echo_locks, [lock]);
    let kinds: Vec<TrackId> = input.tracks.iter().map(|t| t.track.kind).collect();
    assert_eq!(kinds, [TrackId::Mic, TrackId::Reference]);
    for t in &input.tracks {
        assert_eq!(t.audio.rate, PIPELINE_RATE);
        assert_eq!(t.audio.samples.len(), 16_000);
        assert_eq!(t.options, TranscribeOptions::batch());
        assert_eq!(
            Some(t.track.id),
            outcome
                .tracks
                .iter()
                .find(|(k, _)| *k == t.track.kind)
                .map(|x| x.1)
        );
    }
    // The Them recording is the reference (constant -0.5), quantised to 16 bits (away
    // from the resampler's start and end transients).
    let them = &input.tracks[1].audio.samples;
    assert!(them[4000..12_000].iter().all(|&x| (x + 0.5).abs() < 1e-3));
    // The live transcript is untouched by loading.
    let store = svc.store();
    assert!(
        live_segments(&**store, outcome.meeting, TranscriptPass::Live)
            .unwrap()
            .is_empty()
    );
    drop(store);
    // A request track whose recording is gone is reported, not fatal.
    let gone = svc
        .data
        .resolve(&DataDir::audio_rel(outcome.meeting, TrackId::Reference))
        .unwrap();
    std::fs::remove_file(gone).unwrap();
    let (input, missing) = load_post_input(&svc, outcome.meeting, &outcome.final_pass).unwrap();
    assert_eq!(input.tracks.len(), 1);
    assert_eq!(missing, [TrackId::Reference]);
}

#[test]
fn post_job_runs_on_its_own_thread_and_returns_the_models() {
    let dir = TempDir::new();
    let svc = services(&dir);
    let (meeting, mic, _) = meeting_rows(&svc);
    let room = scene_audio(&[SceneTurn::new(0, 0, 8_000)], 8_000);
    let post = PostProcessor::new(
        FakeTranscriber::scripted([Ok(vec![raw(&[("solo", 1_000, 1_300)])])]),
        FakeSegmenter::segmentation_3_0(),
        embedder(),
        config(),
    )
    .unwrap();
    let job = post
        .spawn(
            svc.clone(),
            PostInput {
                echo_locks: Vec::new(),
                meeting,
                tracks: vec![track_input(&mic, room)],
            },
            Box::new(ChaCha8Rng::seed_from_u64(9)),
        )
        .unwrap();
    let (post, result) = job.wait().unwrap();
    let outcome = result.unwrap();
    assert_eq!(outcome.final_segments, 1);
    assert_eq!(outcome.speakers.len(), 1);
    // The processor comes back for the next meeting; its scripted transcriber is spent.
    let mut post = post;
    let again = post.run(
        &svc,
        PostInput {
            echo_locks: Vec::new(),
            meeting,
            tracks: vec![track_input(
                &mic,
                scene_audio(&[SceneTurn::new(0, 0, 8_000)], 8_000),
            )],
        },
        &mut ChaCha8Rng::seed_from_u64(10),
        &CancelToken::new(),
    );
    assert!(matches!(
        again,
        Err(RuntimeError::Asr(AsrError::Backend(_)))
    ));
}

#[test]
fn post_rerun_replaces_the_earlier_speakers_and_final_transcript() {
    let dir = TempDir::new();
    let svc = services(&dir);
    let (meeting, mic, _) = meeting_rows(&svc);
    let room = || scene_audio(&[SceneTurn::new(0, 0, 8_000)], 8_000);
    let mut post = PostProcessor::new(
        FakeTranscriber::scripted([
            Ok(vec![raw(&[("first", 1_000, 1_300)])]),
            Ok(vec![raw(&[("second", 1_000, 1_300)])]),
        ]),
        FakeSegmenter::segmentation_3_0(),
        embedder(),
        config(),
    )
    .unwrap();
    for seed in [11, 12] {
        post.run(
            &svc,
            PostInput {
                echo_locks: Vec::new(),
                meeting,
                tracks: vec![track_input(&mic, room())],
            },
            &mut ChaCha8Rng::seed_from_u64(seed),
            &CancelToken::new(),
        )
        .unwrap();
    }
    let store = svc.store();
    let live: Vec<_> = store
        .speakers(meeting)
        .unwrap()
        .into_iter()
        .filter(|s| !s.meta.deleted)
        .collect();
    assert_eq!(live.len(), 1, "one live speaker after a re-run");
    let finals = live_segments(&**store, meeting, TranscriptPass::Final).unwrap();
    assert_eq!(finals.len(), 1);
    assert_eq!(finals[0].segment.text, "second");
    assert_eq!(finals[0].segment.speaker_id, Some(live[0].id));
}

/// Enrols a person (the user when `is_me`) with scene speaker `k`'s voice.
fn enroll_person(svc: &Services, name: &str, is_me: bool, k: usize) -> EntityId {
    enroll_person_with(svc, name, is_me, k, MODEL)
}

/// [`enroll_person`] with voiceprints of embedding model `model`.
fn enroll_person_with(svc: &Services, name: &str, is_me: bool, k: usize, model: &str) -> EntityId {
    let clock = ManualClock::new(1_760_000_000_000);
    let mut rng = ChaCha8Rng::seed_from_u64(40 + k as u64);
    let mut db = svc.store();
    let (store, hlc) = db.parts();
    let mut stamp = Stamp {
        clock: &clock,
        hlc,
        rng: &mut rng,
    };
    let p = Person::new(name, is_me, &mut stamp);
    store.put_person(p.clone()).unwrap();
    let e = earshot_core::diar::Embedding::new(EmbeddingModelId::new(model), centroid(k));
    enroll(
        &mut *store,
        p.id,
        &[e],
        Duration::from_secs(20),
        &EnrolmentConfig::default(),
        &mut stamp,
    )
    .unwrap();
    p.id
}

fn finals_of(svc: &Services, meeting: EntityId) -> Vec<earshot_core::asr::Segment> {
    live_segments(&**svc.store(), meeting, TranscriptPass::Final)
        .unwrap()
        .into_iter()
        .map(|r| r.segment)
        .collect()
}

fn live_speakers(svc: &Services, meeting: EntityId) -> Vec<earshot_core::model::Speaker> {
    svc.store()
        .speakers(meeting)
        .unwrap()
        .into_iter()
        .filter(|s| !s.meta.deleted)
        .collect()
}

#[test]
fn post_reference_only_input_is_diarised_on_the_reference_track() {
    let dir = TempDir::new();
    let svc = services(&dir);
    let (meeting, _, them) = meeting_rows(&svc);
    let (_, them_audio) = scene();
    let mut post = PostProcessor::new(
        FakeTranscriber::scripted([Ok(vec![
            raw(&[("hi", 13_000, 13_300)]),
            raw(&[("there", 22_000, 22_300)]),
        ])]),
        FakeSegmenter::segmentation_3_0(),
        embedder(),
        config(),
    )
    .unwrap();
    let outcome = post
        .run(
            &svc,
            PostInput {
                echo_locks: Vec::new(),
                meeting,
                tracks: vec![track_input(&them, them_audio)],
            },
            &mut ChaCha8Rng::seed_from_u64(30),
            &CancelToken::new(),
        )
        .unwrap();
    // A two-track meeting keeps its roles: these are remote speakers.
    assert_eq!(outcome.speakers.len(), 2);
    assert!(
        outcome
            .speakers
            .iter()
            .all(|s| s.track == TrackId::Reference && s.role == SpeakerRole::Remote)
    );
    let finals = finals_of(&svc, meeting);
    assert_eq!(finals.len(), 2);
    assert!(
        finals
            .iter()
            .all(|s| s.track == them.id && s.speaker_id.is_some())
    );
    assert_ne!(finals[0].speaker_id, finals[1].speaker_id);
}

#[test]
fn post_without_tracks_is_refused_and_writes_nothing() {
    let dir = TempDir::new();
    let svc = services(&dir);
    let (meeting, mic, them) = meeting_rows(&svc);
    let (mic_audio, them_audio) = scene();
    let mut post = PostProcessor::new(
        transcriber(),
        FakeSegmenter::segmentation_3_0(),
        embedder(),
        config(),
    )
    .unwrap();
    post.run(
        &svc,
        PostInput {
            echo_locks: Vec::new(),
            meeting,
            tracks: vec![track_input(&mic, mic_audio), track_input(&them, them_audio)],
        },
        &mut ChaCha8Rng::seed_from_u64(31),
        &CancelToken::new(),
    )
    .unwrap();
    let before = (finals_of(&svc, meeting), live_speakers(&svc, meeting));
    let err = post
        .run(
            &svc,
            PostInput {
                echo_locks: Vec::new(),
                meeting,
                tracks: vec![],
            },
            &mut ChaCha8Rng::seed_from_u64(32),
            &CancelToken::new(),
        )
        .unwrap_err();
    assert!(matches!(err, RuntimeError::NoTracks));
    assert_eq!(
        (finals_of(&svc, meeting), live_speakers(&svc, meeting)),
        before
    );
}

#[test]
fn post_rerun_over_fewer_tracks_keeps_the_other_tracks_transcript() {
    let dir = TempDir::new();
    let svc = services(&dir);
    let (meeting, mic, them) = meeting_rows(&svc);
    let _alice = enroll_alice(&svc, 1);
    let (mic_audio, them_audio) = scene();
    let mut post = PostProcessor::new(
        FakeTranscriber::scripted([
            // First run: mic, then reference.
            Ok(vec![raw(&[("hello", 1_000, 1_400)])]),
            Ok(vec![
                raw(&[("hi", 13_000, 13_300)]),
                raw(&[("you", 21_000, 21_300)]),
            ]),
            // Re-run of the mic only.
            Ok(vec![raw(&[
                ("hello", 1_000, 1_400),
                ("again", 1_500, 1_900),
            ])]),
        ]),
        FakeSegmenter::segmentation_3_0(),
        embedder(),
        config(),
    )
    .unwrap();
    post.run(
        &svc,
        PostInput {
            echo_locks: Vec::new(),
            meeting,
            tracks: vec![
                track_input(&mic, mic_audio.clone()),
                track_input(&them, them_audio),
            ],
        },
        &mut ChaCha8Rng::seed_from_u64(33),
        &CancelToken::new(),
    )
    .unwrap();
    let them_before: Vec<_> = finals_of(&svc, meeting)
        .into_iter()
        .filter(|s| s.track == them.id)
        .collect();
    assert_eq!(them_before.len(), 2);
    let me_before = live_speakers(&svc, meeting)
        .into_iter()
        .find(|s| s.role == SpeakerRole::Me)
        .unwrap();

    let again = post
        .run(
            &svc,
            PostInput {
                echo_locks: Vec::new(),
                meeting,
                tracks: vec![track_input(&mic, mic_audio)],
            },
            &mut ChaCha8Rng::seed_from_u64(34),
            &CancelToken::new(),
        )
        .unwrap();
    let finals = finals_of(&svc, meeting);
    let them_after: Vec<_> = finals
        .iter()
        .filter(|s| s.track == them.id)
        .cloned()
        .collect();
    assert_eq!(
        them_after, them_before,
        "the reference transcript is untouched"
    );
    let mic_after: Vec<_> = finals.iter().filter(|s| s.track == mic.id).collect();
    assert_eq!(mic_after.len(), 1);
    assert_eq!(mic_after[0].text, "hello again");
    // The kept reference speakers stay; "Me" is not duplicated.
    let speakers = live_speakers(&svc, meeting);
    assert_eq!(
        speakers
            .iter()
            .filter(|s| s.role == SpeakerRole::Me)
            .count(),
        1
    );
    assert!(speakers.iter().any(|s| s.display_name == "Alice"));
    assert!(speakers.iter().any(|s| s.display_name == "Speaker 1"));
    assert_eq!(again.speakers[0].id, me_before.id);
    for s in &finals {
        assert!(speakers.iter().any(|sp| Some(sp.id) == s.speaker_id));
    }
}

#[test]
fn post_failure_inside_the_transaction_writes_nothing() {
    // The speakers are written first; a segment on a track of another meeting then fails
    // the transaction (missing parent): nothing may remain.
    let dir = TempDir::new();
    let svc = services(&dir);
    let (meeting, _, _) = meeting_rows(&svc);
    let (_, other_mic, _) = meeting_rows(&svc);
    let mut post = PostProcessor::new(
        FakeTranscriber::scripted([Ok(vec![raw(&[("stray", 1_000, 1_300)])])]),
        FakeSegmenter::segmentation_3_0(),
        embedder(),
        config(),
    )
    .unwrap();
    let err = post
        .run(
            &svc,
            PostInput {
                echo_locks: Vec::new(),
                meeting,
                tracks: vec![track_input(
                    &other_mic,
                    scene_audio(&[SceneTurn::new(0, 0, 8_000)], 8_000),
                )],
            },
            &mut ChaCha8Rng::seed_from_u64(35),
            &CancelToken::new(),
        )
        .unwrap_err();
    assert!(matches!(err, RuntimeError::Repo(_)), "{err:?}");
    assert!(live_speakers(&svc, meeting).is_empty());
    assert!(finals_of(&svc, meeting).is_empty());
    assert!(svc.store().persons().unwrap().is_empty());
}

#[test]
fn post_two_mic_voices_the_users_match_becomes_me() {
    // A shared room: two voices on the mic (scene speakers 0 and 3). The user is enrolled
    // as speaker 3, so that voice becomes "Me"; the other mic voice is an unknown local
    // speaker. A far-end voice that also matches the user (echo residue) is not a second
    // "Me". Unknowns are numbered densely.
    let dir = TempDir::new();
    let svc = services(&dir);
    let (meeting, mic, them) = meeting_rows(&svc);
    let user = enroll_person(&svc, "Casey", true, 3);
    let mic_audio = scene_audio(
        &[
            SceneTurn::new(0, 0, 10_000),
            SceneTurn::new(3, 10_000, 25_000),
        ],
        40_000,
    );
    let them_audio = scene_audio(
        &[
            SceneTurn::new(3, 26_000, 32_000),
            SceneTurn::new(2, 32_000, 40_000),
        ],
        40_000,
    );
    let mut post = PostProcessor::new(
        FakeTranscriber::scripted([
            Ok(vec![
                raw(&[("morning", 2_000, 2_400)]),
                raw(&[("hi", 12_000, 12_300)]),
            ]),
            Ok(vec![
                raw(&[("echo", 27_000, 27_300)]),
                raw(&[("bye", 33_000, 33_300)]),
            ]),
        ]),
        FakeSegmenter::segmentation_3_0(),
        embedder(),
        config(),
    )
    .unwrap();
    let outcome = post
        .run(
            &svc,
            PostInput {
                echo_locks: Vec::new(),
                meeting,
                tracks: vec![track_input(&mic, mic_audio), track_input(&them, them_audio)],
            },
            &mut ChaCha8Rng::seed_from_u64(36),
            &CancelToken::new(),
        )
        .unwrap();
    let named: Vec<(SpeakerRole, TrackId, &str, Option<EntityId>)> = outcome
        .speakers
        .iter()
        .map(|s| (s.role, s.track, s.display_name.as_str(), s.person))
        .collect();
    assert_eq!(
        named,
        [
            (SpeakerRole::Local, TrackId::Mic, "Speaker 1", None),
            (SpeakerRole::Me, TrackId::Mic, "Casey", Some(user)),
            (SpeakerRole::Remote, TrackId::Reference, "Speaker 2", None),
            (SpeakerRole::Remote, TrackId::Reference, "Speaker 3", None),
        ]
    );
    // The promotion is real: enrolment updated the user's voiceprint.
    assert!(outcome.enrolled_me.is_some());
}

/// Speaker classes by RMS level, so the fakes survive 16-bit FLAC: uniform noise of
/// amplitude 0.1, 0.4 or 0.7 (RMS 0.058, 0.23, 0.40).
fn level_class(x: &[f32]) -> Option<usize> {
    let rms = (x.iter().map(|v| v * v).sum::<f32>() / x.len().max(1) as f32).sqrt();
    [(0.03, 0.09), (0.15, 0.30), (0.33, 0.50)]
        .iter()
        .position(|(lo, hi)| (*lo..*hi).contains(&rms))
}

/// A segmenter keyed by level (see [`level_class`]), pyannote 3.0 geometry.
pub(crate) struct LevelSegmenter(pub(crate) earshot_core::diar::SegmenterInfo);

impl earshot_core::diar::SpeakerSegmenter for LevelSegmenter {
    fn info(&self) -> &earshot_core::diar::SegmenterInfo {
        &self.0
    }
    fn segment(
        &mut self,
        windows: &[&[f32]],
        _cancel: &CancelToken,
    ) -> Result<Vec<earshot_core::diar::LocalActivity>, earshot_core::diar::DiarError> {
        let info = &self.0;
        let slots = info.max_local_speakers;
        windows
            .iter()
            .map(|w| {
                let classes: Vec<Option<usize>> = (0..info.frames)
                    .map(|i| {
                        let s = info.frame_start(i).min(w.len());
                        let e = (s + info.frame_step).min(w.len());
                        level_class(&w[s..e])
                    })
                    .collect();
                let mut order: Vec<usize> = Vec::new();
                for c in classes.iter().flatten() {
                    if !order.contains(c) {
                        order.push(*c);
                    }
                }
                let mut data = vec![0.0; info.frames * slots];
                for (f, c) in classes.iter().enumerate() {
                    if let Some(pos) = c.and_then(|c| order.iter().position(|o| *o == c))
                        && pos < slots
                    {
                        data[f * slots + pos] = 1.0;
                    }
                }
                earshot_core::diar::LocalActivity::new(info.frames, slots, data)
            })
            .collect()
    }
}

/// An embedder keyed by level: one-hot of the region's class.
pub(crate) struct LevelEmbedder(pub(crate) earshot_core::diar::EmbedderInfo);

impl earshot_core::diar::SpeakerEmbedder for LevelEmbedder {
    fn info(&self) -> &earshot_core::diar::EmbedderInfo {
        &self.0
    }
    fn embed(
        &mut self,
        speech: &[&[f32]],
        _cancel: &CancelToken,
    ) -> Result<Vec<earshot_core::diar::Embedding>, earshot_core::diar::DiarError> {
        Ok(speech
            .iter()
            .map(|r| {
                let mut v = vec![0.0f32; 4];
                if let Some(c) = level_class(r) {
                    v[c] = 1.0;
                }
                earshot_core::diar::Embedding::new(self.0.model.clone(), v)
            })
            .collect())
    }
}

fn level_noise(seed: u64, n: usize, amp: f32) -> Vec<f32> {
    let mut rng = ChaCha8Rng::seed_from_u64(seed);
    (0..n)
        .map(|_| {
            use rand_core::Rng;
            ((rng.next_u32() >> 8) as f32 / (1u32 << 24) as f32 * 2.0 - 1.0) * amp
        })
        .collect()
}

/// `total_ms` of near silence with `(amp, start_ms, end_ms)` stretches of noise.
pub(crate) fn levels(total_ms: u64, parts: &[(f32, u64, u64)], seed: u64) -> Vec<f32> {
    let mut out = level_noise(seed, (total_ms * 16) as usize, 1e-4);
    for (i, (amp, s, e)) in parts.iter().enumerate() {
        let (a, b) = ((s * 16) as usize, (e * 16) as usize);
        out[a..b].copy_from_slice(&level_noise(seed + 1 + i as u64, b - a, *amp));
    }
    out
}

#[test]
fn post_round_trip_from_a_live_session_through_flac() {
    // live session (two tracks, fakes) -> FLAC -> load_post_input -> run: the real path,
    // with fakes keyed by level so they survive 16-bit quantisation.
    let dir = TempDir::new();
    let svc = services(&dir);
    let mic = levels(20_000, &[(0.1, 1_000, 6_000)], 50);
    let them = levels(20_000, &[(0.4, 7_000, 12_000), (0.7, 13_000, 18_000)], 60);
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
        Box::new(ChaCha8Rng::seed_from_u64(51)),
    )
    .unwrap();
    let outcome = live.wait().unwrap();
    let (input, missing) = load_post_input(&svc, outcome.meeting, &outcome.final_pass).unwrap();
    assert!(missing.is_empty());
    assert!(input.tracks.iter().all(|t| t.audio.rate == PIPELINE_RATE));

    let model = EmbeddingModelId::new("level@1");
    let cfg = PostConfig {
        clustering_thresholds: ModelThresholds::new().with(model.clone(), 0.704_565_5),
        voiceprint_thresholds: ModelThresholds::new().with(model.clone(), 0.5),
        ..PostConfig::default()
    };
    let mut post = PostProcessor::new(
        FakeTranscriber::scripted([
            Ok(vec![raw(&[("me", 3_000, 3_300)])]),
            Ok(vec![
                raw(&[("first", 9_000, 9_300)]),
                raw(&[("second", 15_000, 15_300)]),
            ]),
        ]),
        LevelSegmenter(earshot_core::diar::SegmenterInfo::pyannote_segmentation_3_0("level")),
        LevelEmbedder(earshot_core::diar::EmbedderInfo {
            model,
            dimension: 4,
            min_samples: 4000,
        }),
        cfg,
    )
    .unwrap();
    let result = post
        .run(
            &svc,
            input,
            &mut ChaCha8Rng::seed_from_u64(52),
            &CancelToken::new(),
        )
        .unwrap();
    let named: Vec<(SpeakerRole, TrackId, &str)> = result
        .speakers
        .iter()
        .map(|s| (s.role, s.track, s.display_name.as_str()))
        .collect();
    assert_eq!(
        named,
        [
            (SpeakerRole::Me, TrackId::Mic, "Me"),
            (SpeakerRole::Remote, TrackId::Reference, "Speaker 1"),
            (SpeakerRole::Remote, TrackId::Reference, "Speaker 2"),
        ]
    );
    let tracks = &outcome.tracks;
    let id_of = |k: TrackId| tracks.iter().find(|(t, _)| *t == k).unwrap().1;
    let finals = finals_of(&svc, outcome.meeting);
    let labelled: Vec<(String, EntityId, Option<EntityId>)> = finals
        .iter()
        .map(|s| (s.text.clone(), s.track, s.speaker_id))
        .collect();
    let sid = |k: usize| Some(result.speakers[k].id);
    assert_eq!(
        labelled,
        [
            ("me".into(), id_of(TrackId::Mic), sid(0)),
            ("first".into(), id_of(TrackId::Reference), sid(1)),
            ("second".into(), id_of(TrackId::Reference), sid(2)),
        ]
    );
}
#[test]
fn post_in_room_session_stores_one_track_and_names_me_by_voiceprint_only() {
    // T6.27: an in-room meeting (mic only, as `WasapiSource` delivers with
    // `CaptureMode::InRoom`) -> live session -> FLAC -> final pass. One Track row and one
    // audio file, no echo canceller and so no echo lock, speakers clustered on the mic with
    // the in-person policy; "Me" only from the user's voiceprint, never by amount of
    // speech (the user's voice is the longest here).
    let dir = TempDir::new();
    let svc = services(&dir);
    let mic = levels(
        30_000,
        &[
            (0.1, 1_000, 6_000),
            (0.4, 7_000, 19_000),
            (0.7, 20_000, 26_000),
        ],
        70,
    );
    let source = ReplayBuilder::new(
        DeviceTime::from_ticks(5_000_000),
        MonoBuffer::new(PIPELINE_RATE, mic),
    )
    .build()
    .unwrap();
    assert!(
        earshot_core::capture::AudioSource::session(&source)
            .reference
            .is_none()
    );
    let live = start_live(
        source,
        svc.clone(),
        LiveConfig {
            clock_domain: ClockDomain::Replay,
            idle_wait: Duration::from_millis(1),
            ..LiveConfig::default()
        },
        None,
        Box::new(ChaCha8Rng::seed_from_u64(71)),
    )
    .unwrap();
    let outcome = live.wait().unwrap();
    assert_eq!(
        outcome.tracks.iter().map(|(k, _)| *k).collect::<Vec<_>>(),
        [TrackId::Mic]
    );
    assert_eq!(
        outcome
            .audio_files
            .iter()
            .map(|(k, _)| *k)
            .collect::<Vec<_>>(),
        [TrackId::Mic]
    );
    assert!(
        outcome.echo_locks.is_empty(),
        "no reference, no canceller lock"
    );
    assert_eq!(outcome.echo_words_removed, 0);
    {
        let store = svc.store();
        let rows = store.tracks(outcome.meeting).unwrap();
        assert_eq!(
            rows.iter().map(|t| t.kind).collect::<Vec<_>>(),
            [TrackId::Mic]
        );
        assert_eq!(rows[0].id, outcome.tracks[0].1);
        let files = store.audio_files(outcome.meeting).unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!(
            files[0].aec_version, None,
            "the mic of an in-room meeting is not echo cancelled"
        );
    }
    let (input, missing) = load_post_input(&svc, outcome.meeting, &outcome.final_pass).unwrap();
    assert!(missing.is_empty());
    assert_eq!(input.tracks.len(), 1);
    assert!(input.echo_locks.is_empty());

    let model = EmbeddingModelId::new("level@1");
    let processor = || {
        PostProcessor::new(
            FakeTranscriber::scripted([Ok(vec![
                raw(&[("hello", 3_000, 3_300)]),
                raw(&[("mine", 10_000, 10_300)]),
                raw(&[("bye", 22_000, 22_300)]),
            ])]),
            LevelSegmenter(earshot_core::diar::SegmenterInfo::pyannote_segmentation_3_0("level")),
            LevelEmbedder(earshot_core::diar::EmbedderInfo {
                model: model.clone(),
                dimension: 4,
                min_samples: 4000,
            }),
            PostConfig {
                clustering_thresholds: ModelThresholds::new().with(model.clone(), 0.704_565_5),
                voiceprint_thresholds: ModelThresholds::new().with(model.clone(), 0.5),
                enroll_me: false,
                ..PostConfig::default()
            },
        )
        .unwrap()
    };
    let roles = |o: &PostOutcome| -> Vec<(SpeakerRole, TrackId, String)> {
        o.speakers
            .iter()
            .map(|s| (s.role, s.track, s.display_name.clone()))
            .collect()
    };

    // Without a voiceprint nobody is "Me".
    let first = processor()
        .run(
            &svc,
            input.clone(),
            &mut ChaCha8Rng::seed_from_u64(72),
            &CancelToken::new(),
        )
        .unwrap();
    assert_eq!(
        roles(&first),
        [
            (SpeakerRole::Room, TrackId::Mic, "Speaker 1".to_owned()),
            (SpeakerRole::Room, TrackId::Mic, "Speaker 2".to_owned()),
            (SpeakerRole::Room, TrackId::Mic, "Speaker 3".to_owned()),
        ]
    );
    assert_eq!(first.echo.removed(), 0);
    assert!(first.echo.locks.is_empty());

    // With the user's voiceprint (the 0.4 voice, level class 1), that voice is "Me".
    let user = {
        let clock = ManualClock::new(1_760_000_000_000);
        let mut rng = ChaCha8Rng::seed_from_u64(73);
        let mut db = svc.store();
        let (store, hlc) = db.parts();
        let mut stamp = Stamp {
            clock: &clock,
            hlc,
            rng: &mut rng,
        };
        let p = Person::new("Casey", true, &mut stamp);
        store.put_person(p.clone()).unwrap();
        let print = earshot_core::diar::Embedding::new(model.clone(), vec![0.0, 1.0, 0.0, 0.0]);
        enroll(
            &mut *store,
            p.id,
            &[print],
            Duration::from_secs(20),
            &EnrolmentConfig::default(),
            &mut stamp,
        )
        .unwrap();
        p.id
    };
    let second = processor()
        .run(
            &svc,
            input,
            &mut ChaCha8Rng::seed_from_u64(74),
            &CancelToken::new(),
        )
        .unwrap();
    assert_eq!(
        roles(&second),
        [
            (SpeakerRole::Room, TrackId::Mic, "Speaker 1".to_owned()),
            (SpeakerRole::Me, TrackId::Mic, "Casey".to_owned()),
            (SpeakerRole::Room, TrackId::Mic, "Speaker 2".to_owned()),
        ]
    );
    assert_eq!(second.speakers[1].person, Some(user));
    let finals = finals_of(&svc, outcome.meeting);
    let labelled: Vec<(String, Option<EntityId>)> = finals
        .iter()
        .map(|s| (s.text.clone(), s.speaker_id))
        .collect();
    assert_eq!(
        labelled,
        [
            ("hello".to_owned(), Some(second.speakers[0].id)),
            ("mine".to_owned(), Some(second.speakers[1].id)),
            ("bye".to_owned(), Some(second.speakers[2].id)),
        ]
    );
    assert!(finals.iter().all(|s| s.track == outcome.tracks[0].1));
}

#[test]
fn post_single_loopback_track_meeting_is_in_person_on_that_track() {
    let dir = TempDir::new();
    let svc = services(&dir);
    let tracks = meeting_with(&svc, &[TrackId::Reference]);
    let (meeting, lb) = (tracks[0].meeting_id, tracks[0].clone());
    let (_, audio) = scene();
    let mut post = PostProcessor::new(
        FakeTranscriber::scripted([Ok(vec![raw(&[("hi", 13_000, 13_300)])])]),
        FakeSegmenter::segmentation_3_0(),
        embedder(),
        config(),
    )
    .unwrap();
    let outcome = post
        .run(
            &svc,
            PostInput {
                echo_locks: Vec::new(),
                meeting,
                tracks: vec![track_input(&lb, audio)],
            },
            &mut ChaCha8Rng::seed_from_u64(37),
            &CancelToken::new(),
        )
        .unwrap();
    assert!(!outcome.speakers.is_empty());
    assert!(
        outcome
            .speakers
            .iter()
            .all(|s| s.track == TrackId::Reference && s.role == SpeakerRole::Room)
    );
    let finals = finals_of(&svc, meeting);
    assert_eq!(finals.len(), 1);
    assert_eq!(finals[0].track, lb.id);
    assert!(finals[0].speaker_id.is_some());
}

/// A speaker row without segments, as a final pass before T7.3b left for a voice no word
/// went to.
fn wordless_speaker_row(svc: &Services, meeting: EntityId) -> EntityId {
    let clock = ManualClock::new(1_760_000_100_000);
    let mut rng = ChaCha8Rng::seed_from_u64(62);
    let mut db = svc.store();
    let (store, hlc) = db.parts();
    let mut stamp = Stamp {
        clock: &clock,
        hlc,
        rng: &mut rng,
    };
    let s = Speaker::new(meeting, SpeakerRole::Remote, "Speaker 9", None, &mut stamp);
    store.put_speaker(s.clone()).unwrap();
    s.id
}

#[test]
fn post_partial_rerun_keeps_speakers_of_the_other_track_without_segments() {
    // The loopback's second voice (scene speaker 2) said nothing the transcriber kept.
    // Since T7.3b such a voice gets no row; a row without segments from an earlier build
    // stands in for it. A mic-only re-run must not tombstone it.
    let dir = TempDir::new();
    let svc = services(&dir);
    let (meeting, mic, them) = meeting_rows(&svc);
    let (mic_audio, them_audio) = scene();
    let mut post = PostProcessor::new(
        FakeTranscriber::scripted([
            Ok(vec![raw(&[("hello", 1_000, 1_400)])]),
            Ok(vec![raw(&[("hi", 13_000, 13_300)])]),
            Ok(vec![raw(&[("hello", 1_000, 1_400)])]),
        ]),
        FakeSegmenter::segmentation_3_0(),
        embedder(),
        config(),
    )
    .unwrap();
    let first = post
        .run(
            &svc,
            PostInput {
                echo_locks: Vec::new(),
                meeting,
                tracks: vec![
                    track_input(&mic, mic_audio.clone()),
                    track_input(&them, them_audio),
                ],
            },
            &mut ChaCha8Rng::seed_from_u64(60),
            &CancelToken::new(),
        )
        .unwrap();
    assert_eq!(first.speakers.len(), 2, "the silent voice gets no row");
    let silent = wordless_speaker_row(&svc, meeting);
    post.run(
        &svc,
        PostInput {
            echo_locks: Vec::new(),
            meeting,
            tracks: vec![track_input(&mic, mic_audio)],
        },
        &mut ChaCha8Rng::seed_from_u64(61),
        &CancelToken::new(),
    )
    .unwrap();
    assert!(live_speakers(&svc, meeting).iter().any(|s| s.id == silent));
}

#[test]
fn post_voices_without_words_get_no_speaker_row() {
    // Scene speaker 2 talks on the loopback from 20 to 30 s, but no transcribed word lies
    // in its turns: it is reported, and every stored speaker owns a final segment.
    let dir = TempDir::new();
    let svc = services(&dir);
    let (meeting, mic, them) = meeting_rows(&svc);
    let (mic_audio, them_audio) = scene();
    let mut post = PostProcessor::new(
        FakeTranscriber::scripted([
            Ok(vec![raw(&[("hello", 1_000, 1_400)])]),
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
    let outcome = post
        .run(
            &svc,
            PostInput {
                echo_locks: Vec::new(),
                meeting,
                tracks: vec![track_input(&mic, mic_audio), track_input(&them, them_audio)],
            },
            &mut ChaCha8Rng::seed_from_u64(63),
            &CancelToken::new(),
        )
        .unwrap();
    let silent: Vec<&PostWarning> = outcome
        .warnings
        .iter()
        .filter(|w| matches!(w, PostWarning::VoiceWithoutWords { .. }))
        .collect();
    assert_eq!(silent.len(), 1, "{:?}", outcome.warnings);
    let PostWarning::VoiceWithoutWords { track, speech } = silent[0] else {
        unreachable!()
    };
    assert_eq!(*track, TrackId::Reference);
    assert!(*speech >= Duration::from_secs(5), "{speech:?}");

    let finals = finals_of(&svc, meeting);
    let speakers = live_speakers(&svc, meeting);
    assert_eq!(speakers.len(), 2, "Me and the far-end voice that spoke");
    assert_eq!(outcome.speakers.len(), 2);
    for s in &speakers {
        assert!(
            finals.iter().any(|f| f.speaker_id == Some(s.id)),
            "{} owns no segment",
            s.display_name
        );
    }
    // Unknown voices stay numbered densely: the silent one used no number.
    let names: Vec<&str> = speakers
        .iter()
        .filter(|s| s.role != SpeakerRole::Me)
        .map(|s| s.display_name.as_str())
        .collect();
    assert_eq!(names, ["Speaker 1"]);
}

#[test]
fn post_enrols_the_person_the_reused_me_row_links_to() {
    let dir = TempDir::new();
    let svc = services(&dir);
    let tracks = meeting_with(&svc, &[TrackId::Mic, TrackId::Reference]);
    let (meeting, mic) = (tracks[0].meeting_id, tracks[0].clone());
    let mic_audio = scene_audio(&[SceneTurn::new(0, 0, 15_000)], 15_000);
    let mut post = PostProcessor::new(
        FakeTranscriber::scripted([
            Ok(vec![raw(&[("first", 1_000, 1_300)])]),
            Ok(vec![raw(&[("second", 1_000, 1_300)])]),
        ]),
        FakeSegmenter::segmentation_3_0(),
        embedder(),
        PostConfig {
            enroll_me: true,
            ..config()
        },
    )
    .unwrap();
    let run = |post: &mut PostProcessor<_, _, _>, seed| {
        post.run(
            &svc,
            PostInput {
                echo_locks: Vec::new(),
                meeting,
                tracks: vec![track_input(&mic, mic_audio.clone())],
            },
            &mut ChaCha8Rng::seed_from_u64(seed),
            &CancelToken::new(),
        )
        .unwrap()
    };
    let first = run(&mut post, 62);
    // The user re-links the Me speaker to a person of their own naming.
    let casey = enroll_person(&svc, "Casey", false, 5);
    {
        let clock = ManualClock::new(1_760_000_000_000);
        let mut db = svc.store();
        let mut me = db
            .speakers(meeting)
            .unwrap()
            .into_iter()
            .find(|s| s.id == first.speakers[0].id)
            .unwrap();
        me.person_id = Some(casey);
        let (store, hlc) = db.parts();
        me.meta.touch(hlc, &clock);
        store.put_speaker(me).unwrap();
    }
    let second = run(&mut post, 63);
    assert_eq!(second.speakers[0].id, first.speakers[0].id);
    assert_eq!(second.speakers[0].person, Some(casey));
    let vp = second.enrolled_me.expect("enrolled");
    assert!(
        svc.store()
            .voiceprints_of(casey)
            .unwrap()
            .iter()
            .any(|v| v.id == vp)
    );
}

/// A transcriber that reports CUDA until its first call, then the CPU (a per-call GPU
/// fallback, as the production loader reports it).
struct Degrading {
    inner: FakeTranscriber,
    called: bool,
}

impl earshot_core::asr::Transcriber for Degrading {
    fn transcribe(
        &mut self,
        audio: &MonoBuffer,
        options: &TranscribeOptions,
        cancel: &CancelToken,
    ) -> Result<earshot_core::asr::Transcription, AsrError> {
        self.called = true;
        self.inner.transcribe(audio, options, cancel)
    }
    fn backend_info(&self) -> earshot_core::asr::BackendInfo {
        let mut info = self.inner.backend_info();
        info.kind = if self.called {
            earshot_core::asr::BackendKind::Cpu
        } else {
            earshot_core::asr::BackendKind::Cuda
        };
        info
    }
}

#[test]
fn post_a_backend_change_during_the_final_pass_is_reported_and_recorded() {
    let dir = TempDir::new();
    let svc = services(&dir);
    let tracks = meeting_with(&svc, &[TrackId::Mic]);
    let (meeting, mic) = (tracks[0].meeting_id, tracks[0].clone());
    let mut post = PostProcessor::new(
        Degrading {
            inner: FakeTranscriber::scripted([Ok(vec![raw(&[("hi", 1_000, 1_300)])])]),
            called: false,
        },
        FakeSegmenter::segmentation_3_0(),
        embedder(),
        config(),
    )
    .unwrap();
    let outcome = post
        .run(
            &svc,
            PostInput {
                echo_locks: Vec::new(),
                meeting,
                tracks: vec![track_input(
                    &mic,
                    scene_audio(&[SceneTurn::new(0, 0, 4_000)], 4_000),
                )],
            },
            &mut ChaCha8Rng::seed_from_u64(90),
            &CancelToken::new(),
        )
        .unwrap();
    assert_eq!(outcome.backend.kind, earshot_core::asr::BackendKind::Cpu);
    assert_eq!(
        outcome.warnings,
        [PostWarning::BackendChanged {
            before: earshot_core::asr::BackendKind::Cuda,
            after: earshot_core::asr::BackendKind::Cpu,
        }]
    );
    let stored =
        crate::backends::pass_backend(&svc.store(), meeting, TranscriptPass::Final, None).unwrap();
    assert_eq!(
        stored.map(|b| b.kind),
        Some(earshot_core::asr::BackendKind::Cpu)
    );
}

/// One lock stretch over the whole (test) call with `delay_ms`.
fn whole_call(delay_ms: u64) -> Vec<crate::echo::EchoLock> {
    vec![crate::echo::EchoLock {
        from_ms: 0,
        to_ms: 20_000,
        delay_ms,
    }]
}

/// The CAM++ model id, so the echo speaker check has a calibrated margin (the fake
/// embedder produces one-hot voices; only the id matters for the calibration lookup).
pub(super) const CAMPPLUS_ID: &str = "campplus-en-voxceleb@357a834f702b";

fn echo_config(model: &str) -> PostConfig {
    PostConfig {
        clustering_thresholds: ModelThresholds::new()
            .with(EmbeddingModelId::new(model), 0.704_565_5),
        voiceprint_thresholds: ModelThresholds::new().with(EmbeddingModelId::new(model), 0.5),
        enroll_me: false,
        ..config()
    }
}

fn echo_embedder(model: &str) -> FakeEmbedder {
    FakeEmbedder::new(
        model,
        (0..MAX_SCENE_SPEAKERS).map(centroid).collect(),
        0.05,
        11,
    )
}

/// A call with a leaky echo path, 150 ms delay. Mic: the user (scene speaker 0) at
/// 0-6 s, then only the far end's echo (speaker 1's voice) at 8.15-14.15 s. Loopback:
/// the far end (speaker 1) at 8-14 s. The final pass recognises the echo on the mic: a
/// run repeating the far end ("we should ship it") and, later, garbled fragments ("wee",
/// "shop") that no text rule can match.
fn echo_scene() -> (MonoBuffer, MonoBuffer, FakeTranscriber) {
    let mic = scene_audio(
        &[
            SceneTurn::new(0, 0, 6_000),
            SceneTurn::new(1, 8_150, 14_150),
        ],
        16_000,
    );
    let them = scene_audio(&[SceneTurn::new(1, 8_000, 14_000)], 16_000);
    let asr = FakeTranscriber::scripted([
        Ok(vec![
            raw(&[("hello", 1_000, 1_400), ("all", 1_500, 1_900)]),
            raw(&[
                ("we", 8_650, 8_900),
                ("should", 8_950, 9_200),
                ("ship", 9_250, 9_500),
                ("it", 9_550, 9_800),
            ]),
            raw(&[("wee", 12_150, 12_400), ("shop", 12_650, 12_900)]),
        ]),
        Ok(vec![
            raw(&[
                ("we", 8_500, 8_750),
                ("should", 8_800, 9_050),
                ("ship", 9_100, 9_350),
                ("it", 9_400, 9_650),
            ]),
            raw(&[("on", 12_000, 12_250), ("friday", 12_500, 12_750)]),
        ]),
    ]);
    (mic, them, asr)
}

fn mic_finals(svc: &Services, meeting: EntityId, mic: &Track) -> Vec<String> {
    finals_of(svc, meeting)
        .into_iter()
        .filter(|s| s.track == mic.id)
        .flat_map(|s| s.words.into_iter().map(|w| w.text))
        .collect()
}

fn run_echo(
    model: &str,
    echo_locks: Vec<crate::echo::EchoLock>,
) -> (Services, TempDir, EntityId, Track, PostOutcome) {
    let dir = TempDir::new();
    let svc = services(&dir);
    let (meeting, mic, them) = meeting_rows(&svc);
    let (mic_audio, them_audio, asr) = echo_scene();
    let mut post = PostProcessor::new(
        asr,
        FakeSegmenter::segmentation_3_0(),
        echo_embedder(model),
        echo_config(model),
    )
    .unwrap();
    let input = PostInput {
        meeting,
        tracks: vec![track_input(&mic, mic_audio), track_input(&them, them_audio)],
        echo_locks,
    };
    let outcome = post
        .run(
            &svc,
            input,
            &mut ChaCha8Rng::seed_from_u64(70),
            &CancelToken::new(),
        )
        .unwrap();
    (svc, dir, meeting, mic, outcome)
}

#[test]
fn post_echo_text_rule_and_speaker_check_clean_the_me_transcript() {
    let (svc, _dir, meeting, mic, outcome) = run_echo(CAMPPLUS_ID, whole_call(150));
    // The user's words stay; the repeated run and the garbled echo are gone.
    assert_eq!(mic_finals(&svc, meeting, &mic), ["hello", "all"]);
    assert_eq!(
        outcome.echo,
        EchoReport {
            locks: whole_call(150),
            text_removed: 4,
            speaker_removed: 2,
            speaker_check_skipped: None,
            bridged_gaps: Vec::new(),
            cluster_removed: 0,
        }
    );
    assert!(outcome.warnings.is_empty(), "{:?}", outcome.warnings);
    // The mic held two clusters (the user and the echo). The echo cluster lost every word,
    // so it is no speaker: one mic speaker remains, and it is "Me"; no row for the echo.
    let mic_speakers: Vec<(SpeakerRole, &str)> = outcome
        .speakers
        .iter()
        .filter(|s| s.track == TrackId::Mic)
        .map(|s| (s.role, s.display_name.as_str()))
        .collect();
    assert_eq!(mic_speakers, [(SpeakerRole::Me, "Me")]);
    let rows = live_speakers(&svc, meeting);
    assert_eq!(
        rows.iter()
            .map(|s| (s.role, s.display_name.as_str()))
            .collect::<Vec<_>>(),
        [(SpeakerRole::Me, "Me"), (SpeakerRole::Remote, "Speaker 1")]
    );
    // The far end keeps every word, and the report is stored with the pass.
    let them: Vec<String> = finals_of(&svc, meeting)
        .into_iter()
        .filter(|s| s.track != mic.id)
        .flat_map(|s| s.words.into_iter().map(|w| w.text))
        .collect();
    assert_eq!(them, ["we", "should", "ship", "it", "on", "friday"]);
    let stored = crate::echo::echo_report(&svc.store(), meeting, TranscriptPass::Final)
        .unwrap()
        .unwrap();
    assert_eq!(stored, outcome.echo);
}

#[test]
fn post_echo_without_a_lock_removes_nothing() {
    // Headphones: no echo path, no lock. The mic transcript is kept whole.
    let (svc, _dir, meeting, mic, outcome) = run_echo(CAMPPLUS_ID, Vec::new());
    assert_eq!(
        mic_finals(&svc, meeting, &mic),
        ["hello", "all", "we", "should", "ship", "it", "wee", "shop"]
    );
    assert_eq!(outcome.echo.removed(), 0);
    assert!(outcome.echo.locks.is_empty());
    assert!(outcome.warnings.is_empty(), "{:?}", outcome.warnings);
    // Without a lock the echo cluster is not identified: both mic voices stay (as before
    // T6.4g, two local voices without a voiceprint match).
    let mic_roles: Vec<SpeakerRole> = outcome
        .speakers
        .iter()
        .filter(|s| s.track == TrackId::Mic)
        .map(|s| s.role)
        .collect();
    assert_eq!(mic_roles, [SpeakerRole::Local, SpeakerRole::Local]);
}

/// `MicEcho` holds one mic and one reference track; a second mic would be ignored by echo
/// cleaning, so a debug build refuses it loudly.
#[test]
#[cfg(debug_assertions)]
#[should_panic(expected = "echo cleaning handles at most one mic track")]
fn post_echo_two_mic_tracks_panic_in_debug() {
    let dir = TempDir::new();
    let svc = services(&dir);
    let tracks = meeting_with(&svc, &[TrackId::Mic, TrackId::Mic]);
    let meeting = tracks[0].meeting_id;
    let audio = || scene_audio(&[SceneTurn::new(0, 0, 4_000)], 5_000);
    let mut post = PostProcessor::new(
        FakeTranscriber::scripted([
            Ok(vec![raw(&[("one", 1_000, 1_300)])]),
            Ok(vec![raw(&[("two", 2_000, 2_300)])]),
        ]),
        FakeSegmenter::segmentation_3_0(),
        embedder(),
        PostConfig {
            enroll_me: false,
            ..config()
        },
    )
    .unwrap();
    let _ = post.run(
        &svc,
        PostInput {
            echo_locks: Vec::new(),
            meeting,
            tracks: vec![
                track_input(&tracks[0], audio()),
                track_input(&tracks[1], audio()),
            ],
        },
        &mut ChaCha8Rng::seed_from_u64(5),
        &CancelToken::new(),
    );
}

#[test]
fn post_echo_clean_mic_echo_runs_the_text_rule_without_diarisation() {
    // The public entry point evaluation tools call: with a lock and both transcripts but
    // no diarisation, the text rule runs in the bridged stretches and the speaker check
    // reports why it did not.
    let reference: Vec<Word> = ["we", "should", "ship", "it"]
        .iter()
        .enumerate()
        .map(|(k, t)| Word::new(*t, 8_000 + 300 * k as u64, 8_250 + 300 * k as u64, 0.9))
        .collect();
    let mut words = vec![Word::new("hello", 1_000, 1_300, 0.9)];
    words.extend(
        reference
            .iter()
            .map(|w| w.shifted(MeetingTime::from_millis(150))),
    );
    let segment = |words: Vec<Word>| {
        Segment::from_words(
            EntityId::from_bytes([1; 16]),
            EntityId::from_bytes([2; 16]),
            words,
            MeetingTime::ZERO,
        )
    };
    // The echo run lies in a 4 s gap between two stretches at the same delay.
    let locks = [
        EchoLock {
            from_ms: 0,
            to_ms: 6_000,
            delay_ms: 150,
        },
        EchoLock {
            from_ms: 10_000,
            to_ms: 20_000,
            delay_ms: 151,
        },
    ];
    let mut mic = vec![segment(words.clone())];
    let out = clean_mic_echo(
        MicEcho {
            locks: &locks,
            mic: Some(&mut mic),
            reference: Some(&reference),
            mic_diar: None,
            ref_diar: None,
            mic_audio: None,
            voiceprint_threshold: 0.5,
        },
        &mut embedder(),
        &CancelToken::new(),
    )
    .unwrap();
    assert_eq!(out.report.text_removed, 4);
    assert_eq!(out.report.bridged_gaps, [(6_000, 10_000)]);
    assert_eq!(
        out.report.speaker_check_skipped.as_deref(),
        Some("no diarisation of both tracks")
    );
    assert_eq!(out.me, None);
    let kept: Vec<&str> = mic
        .iter()
        .flat_map(|s| s.words.iter().map(|w| w.text.as_str()))
        .collect();
    assert_eq!(kept, ["hello"]);

    // No lock: nothing is removed and the mic transcript is untouched.
    let mut mic = vec![segment(words)];
    let before = mic.clone();
    let out = clean_mic_echo(
        MicEcho {
            locks: &[],
            mic: Some(&mut mic),
            reference: Some(&reference),
            mic_diar: None,
            ref_diar: None,
            mic_audio: None,
            voiceprint_threshold: 0.5,
        },
        &mut embedder(),
        &CancelToken::new(),
    )
    .unwrap();
    assert_eq!(out.report.removed(), 0);
    assert_eq!(mic, before);
}

#[test]
fn post_echo_uncalibrated_model_reports_the_missing_speaker_check() {
    // The fake model has no calibrated speaker-check margin: the text rule still runs,
    // the speaker check does not, and that is reported (warning and stored report).
    let (svc, _dir, meeting, mic, outcome) = run_echo(MODEL, whole_call(150));
    assert_eq!(
        mic_finals(&svc, meeting, &mic),
        ["hello", "all", "wee", "shop"]
    );
    assert_eq!(outcome.echo.text_removed, 4);
    assert_eq!(outcome.echo.speaker_removed, 0);
    let reason = outcome.echo.speaker_check_skipped.clone().expect("skipped");
    assert!(reason.contains("fake-emb@1"), "{reason}");
    assert!(
        outcome
            .warnings
            .iter()
            .any(|w| matches!(w, PostWarning::NoSpeakerEchoCheck { reason: r } if *r == reason)),
        "{:?}",
        outcome.warnings
    );
    let stored = crate::echo::echo_report(&svc.store(), meeting, TranscriptPass::Final)
        .unwrap()
        .unwrap();
    assert_eq!(stored.speaker_check_skipped, Some(reason));
}

/// Runs the final pass over `mic_turns` / `them_turns` (scene speakers, ms) with the
/// scripted transcripts, `locks` and `model`; `enroll_me` as given.
fn run_scene(
    svc: &Services,
    model: &str,
    mic_turns: &[(usize, u64, u64)],
    them_turns: &[(usize, u64, u64)],
    asr: FakeTranscriber,
    locks: Vec<crate::echo::EchoLock>,
    enroll_me: bool,
) -> (EntityId, Track, PostOutcome) {
    let (meeting, mic, them) = meeting_rows(svc);
    let total = 20_500;
    let turns = |t: &[(usize, u64, u64)]| -> Vec<SceneTurn> {
        t.iter().map(|&(k, s, e)| SceneTurn::new(k, s, e)).collect()
    };
    let mut post = PostProcessor::new(
        asr,
        FakeSegmenter::segmentation_3_0(),
        echo_embedder(model),
        PostConfig {
            enroll_me,
            ..echo_config(model)
        },
    )
    .unwrap();
    let input = PostInput {
        meeting,
        tracks: vec![
            track_input(&mic, scene_audio(&turns(mic_turns), total)),
            track_input(&them, scene_audio(&turns(them_turns), total)),
        ],
        echo_locks: locks,
    };
    let outcome = post
        .run(
            svc,
            input,
            &mut ChaCha8Rng::seed_from_u64(72),
            &CancelToken::new(),
        )
        .unwrap();
    (meeting, mic, outcome)
}

fn lock(from_ms: u64, to_ms: u64, delay_ms: u64) -> crate::echo::EchoLock {
    crate::echo::EchoLock {
        from_ms,
        to_ms,
        delay_ms,
    }
}

fn mic_roles(outcome: &PostOutcome) -> Vec<(SpeakerRole, String)> {
    outcome
        .speakers
        .iter()
        .filter(|s| s.track == TrackId::Mic)
        .map(|s| (s.role, s.display_name.clone()))
        .collect()
}

#[test]
fn post_echo_lock_dropped_before_the_end_still_cleans_the_loudspeaker_half() {
    // Loudspeakers for the first half (echo of the far end at 6.15-10.15 s, lock 0-11 s),
    // then headphones: the lock is gone at the end, the user keeps talking over the far
    // end without echo. The stored Final "Me" has no far-end words and all of the user's.
    let dir = TempDir::new();
    let svc = services(&dir);
    let asr = FakeTranscriber::scripted([
        Ok(vec![
            raw(&[("morning", 1_000, 1_400), ("everyone", 1_500, 1_900)]),
            raw(&[
                ("we", 6_650, 6_900),
                ("should", 6_950, 7_200),
                ("ship", 7_250, 7_500),
                ("it", 7_550, 7_800),
            ]),
            raw(&[("sounds", 13_000, 13_400), ("good", 13_500, 13_900)]),
        ]),
        Ok(vec![
            raw(&[
                ("we", 6_500, 6_750),
                ("should", 6_800, 7_050),
                ("ship", 7_100, 7_350),
                ("it", 7_400, 7_650),
            ]),
            raw(&[("on", 14_000, 14_250), ("friday", 14_500, 14_750)]),
        ]),
    ]);
    let (meeting, mic, outcome) = run_scene(
        &svc,
        CAMPPLUS_ID,
        &[(0, 0, 4_000), (1, 6_150, 10_150), (0, 12_000, 16_000)],
        &[(1, 6_000, 10_000), (1, 12_500, 18_000)],
        asr,
        vec![lock(0, 11_000, 150)],
        false,
    );
    assert_eq!(
        mic_finals(&svc, meeting, &mic),
        ["morning", "everyone", "sounds", "good"]
    );
    assert_eq!(outcome.echo.text_removed, 4);
    assert_eq!(
        mic_roles(&outcome),
        [(SpeakerRole::Me, "Me".to_owned())],
        "the echo cluster is dropped, the user is the one mic voice"
    );
}

#[test]
fn post_echo_cleans_each_lock_stretch_and_nothing_between_them() {
    // Two lock stretches (0-6 s at 150 ms, then 12-20 s at 300 ms after a device change);
    // a repeated run between them (8 s, no lock) is kept: a gap at another delay is not
    // bridged, and nothing is cleaned without a lock.
    let dir = TempDir::new();
    let svc = services(&dir);
    let run3 = |a: &'static str, b: &'static str, c: &'static str, t: u64| {
        raw(&[
            (a, t, t + 250),
            (b, t + 300, t + 550),
            (c, t + 600, t + 850),
        ])
    };
    let asr = FakeTranscriber::scripted([
        Ok(vec![
            run3("alpha", "beta", "gamma", 2_150),
            run3("delta", "echo", "foxtrot", 8_150),
            run3("golf", "hotel", "india", 14_300),
        ]),
        Ok(vec![
            run3("alpha", "beta", "gamma", 2_000),
            run3("delta", "echo", "foxtrot", 8_000),
            run3("golf", "hotel", "india", 14_000),
        ]),
    ]);
    let (meeting, mic, outcome) = run_scene(
        &svc,
        MODEL,
        &[(0, 0, 20_000)],
        &[(1, 0, 20_000)],
        asr,
        vec![lock(0, 6_000, 150), lock(12_000, 20_000, 300)],
        false,
    );
    assert_eq!(
        mic_finals(&svc, meeting, &mic),
        ["delta", "echo", "foxtrot"]
    );
    assert_eq!(outcome.echo.text_removed, 6);
    assert_eq!(outcome.echo.locks.len(), 2);
    assert!(outcome.echo.bridged_gaps.is_empty());
}

#[test]
fn post_echo_bridges_a_short_lock_gap_at_the_same_delay() {
    // T6.23 (a private recording): in talk-over the canceller dropped its lock for a few
    // seconds and re-locked at the same delay; a repeated run in that gap is echo. The
    // stretches as recorded stay in the report.
    let dir = TempDir::new();
    let svc = services(&dir);
    let run3 = |a: &'static str, b: &'static str, c: &'static str, t: u64| {
        raw(&[
            (a, t, t + 250),
            (b, t + 300, t + 550),
            (c, t + 600, t + 850),
        ])
    };
    let asr = FakeTranscriber::scripted([
        Ok(vec![
            run3("alpha", "beta", "gamma", 2_150),
            run3("delta", "echo", "foxtrot", 8_150),
            run3("golf", "hotel", "india", 14_150),
        ]),
        Ok(vec![
            run3("alpha", "beta", "gamma", 2_000),
            run3("delta", "echo", "foxtrot", 8_000),
            run3("golf", "hotel", "india", 14_000),
        ]),
    ]);
    let locks = vec![lock(0, 6_000, 150), lock(12_000, 20_000, 155)];
    let (meeting, mic, outcome) = run_scene(
        &svc,
        MODEL,
        &[(0, 0, 20_000)],
        &[(1, 0, 20_000)],
        asr,
        locks.clone(),
        false,
    );
    assert!(mic_finals(&svc, meeting, &mic).is_empty());
    assert_eq!(outcome.echo.text_removed, 9);
    assert_eq!(outcome.echo.locks, locks);
    assert_eq!(outcome.echo.bridged_gaps, [(6_000, 12_000)]);
}

#[test]
fn post_echo_speaker_check_does_not_guess_me_among_real_mic_voices() {
    // The mic holds the user (scene speaker 0, 0-6 s), the far end's echo (speaker 1,
    // 8.15-14.15 s) and a colleague in the room (speaker 2, 14.5-16 s). The echo cluster
    // is dropped; two real voices remain, both talking alone, so the speaker check cannot
    // tell which is the user: without a voiceprint neither is "Me".
    let dir = TempDir::new();
    let svc = services(&dir);
    let asr = FakeTranscriber::scripted([
        Ok(vec![
            raw(&[("hello", 1_000, 1_400), ("all", 1_500, 1_900)]),
            raw(&[("wee", 9_150, 9_400), ("shop", 12_650, 12_900)]),
            raw(&[("agreed", 15_000, 15_400)]),
        ]),
        Ok(vec![raw(&[("we", 9_000, 9_250), ("ship", 12_500, 12_750)])]),
    ]);
    let (meeting, mic, outcome) = run_scene(
        &svc,
        CAMPPLUS_ID,
        &[(0, 0, 6_000), (1, 8_150, 14_150), (2, 14_500, 16_000)],
        &[(1, 8_000, 14_000)],
        asr,
        whole_call(150),
        true,
    );
    assert_eq!(outcome.echo.speaker_removed, 2);
    assert_eq!(
        mic_roles(&outcome),
        [
            (SpeakerRole::Local, "Speaker 1".to_owned()),
            (SpeakerRole::Local, "Speaker 2".to_owned())
        ]
    );
    assert_eq!(outcome.enrolled_me, None);
    assert_eq!(mic_finals(&svc, meeting, &mic), ["hello", "all", "agreed"]);
}

#[test]
fn post_echo_users_voiceprint_beats_a_louder_colleague() {
    // The user (scene speaker 0, enrolled as the user) talks less than a colleague
    // (speaker 2) next to them; the far end echoes in between. The user is "Me" by their
    // voiceprint, the colleague stays "Speaker 1", and enrolment goes to the user only.
    let dir = TempDir::new();
    let svc = services(&dir);
    let user = enroll_person_with(&svc, "Casey", true, 0, CAMPPLUS_ID);
    let asr = FakeTranscriber::scripted([
        Ok(vec![
            raw(&[("hi", 1_000, 1_300)]),
            raw(&[
                ("lots", 4_000, 4_300),
                ("of", 4_400, 4_600),
                ("talk", 4_700, 5_000),
            ]),
            raw(&[
                ("we", 10_650, 10_900),
                ("should", 10_950, 11_200),
                ("ship", 11_250, 11_500),
                ("it", 11_550, 11_800),
            ]),
        ]),
        Ok(vec![raw(&[
            ("we", 10_500, 10_750),
            ("should", 10_800, 11_050),
            ("ship", 11_100, 11_350),
            ("it", 11_400, 11_650),
        ])]),
    ]);
    let (_meeting, _mic, outcome) = run_scene(
        &svc,
        CAMPPLUS_ID,
        &[(0, 0, 2_500), (2, 3_000, 9_500), (1, 10_150, 14_150)],
        &[(1, 10_000, 14_000)],
        asr,
        whole_call(150),
        true,
    );
    let mic = mic_roles(&outcome);
    assert_eq!(
        mic,
        [
            (SpeakerRole::Me, "Casey".to_owned()),
            (SpeakerRole::Local, "Speaker 1".to_owned())
        ]
    );
    let me = outcome
        .speakers
        .iter()
        .find(|s| s.role == SpeakerRole::Me)
        .unwrap();
    assert_eq!(me.person, Some(user));
    // Enrolment went to the user, from the user's own voice.
    let vp = outcome.enrolled_me.expect("Me enrolled");
    let store = svc.store();
    let prints = store.voiceprints_of(user).unwrap();
    let stored = prints
        .iter()
        .find(|p| p.id == vp)
        .expect("the user's print");
    let user_voice =
        earshot_core::diar::Embedding::new(EmbeddingModelId::new(CAMPPLUS_ID), centroid(0));
    // The print adds the Me centroid to the earlier enrolment: still the user's voice (a
    // colleague's centroid would pull it to 0.7).
    let centroid_now = stored.centroid().expect("a centroid");
    assert!(centroid_now.cosine_similarity(&user_voice).unwrap() > 0.9);
}

#[test]
fn post_echo_speaker_check_names_me_only_when_it_discriminates() {
    // The user (speaker 0) and an echo cluster (speaker 1) whose last word falls after the
    // lock (kept: nothing is removed without a lock), so the cluster stays a mic voice.
    // The speaker check found only the user to be the user (the cluster never talks alone
    // and sounds like the far end): the user is "Me", the cluster a local voice. A "Me"
    // chosen by the check is never enrolled.
    let dir = TempDir::new();
    let svc = services(&dir);
    let asr = FakeTranscriber::scripted([
        Ok(vec![
            raw(&[("hello", 1_000, 1_400), ("all", 1_500, 1_900)]),
            raw(&[("wee", 9_150, 9_400), ("shop", 10_650, 10_900)]),
            raw(&[("tail", 13_000, 13_300)]),
        ]),
        Ok(vec![raw(&[("we", 9_000, 9_250), ("ship", 10_500, 10_750)])]),
    ]);
    let (meeting, mic, outcome) = run_scene(
        &svc,
        CAMPPLUS_ID,
        &[(0, 0, 6_000), (1, 8_150, 14_150)],
        &[(1, 8_000, 14_000)],
        asr,
        vec![lock(0, 12_000, 150)],
        true,
    );
    assert_eq!(
        mic_roles(&outcome),
        [
            (SpeakerRole::Me, "Me".to_owned()),
            (SpeakerRole::Local, "Speaker 1".to_owned())
        ]
    );
    assert_eq!(
        outcome.enrolled_me, None,
        "never enrolled from the check's choice"
    );
    assert_eq!(mic_finals(&svc, meeting, &mic), ["hello", "all", "tail"]);
}

#[test]
fn post_echo_mostly_echo_cluster_that_sounds_like_the_far_end_is_not_a_mic_voice() {
    // T6.23a (a private recording): besides the user (scene speaker 0, 0-6 s) the mic holds a
    // cluster with the far end's voice (speaker 1): its echo at 8.15-14.15 s and a stretch
    // at 17-18.5 s where the far end is silent, so it "talks alone" and passes the speaker
    // check's user test (the check then judges its spans against its own centroid and
    // flags none). The text rule removes its echo run; one word is left. Most of its words
    // were echo and its voice matches the far end: an echo cluster, so the user is the
    // single mic voice and "Me". Its leftover word is a far-end word: removed and counted,
    // never moved to "Me".
    let dir = TempDir::new();
    let svc = services(&dir);
    let asr = FakeTranscriber::scripted([
        Ok(vec![
            raw(&[("hello", 1_000, 1_400), ("all", 1_500, 1_900)]),
            raw(&[
                ("we", 8_650, 8_900),
                ("should", 8_950, 9_200),
                ("ship", 9_250, 9_500),
            ]),
            raw(&[("uh", 17_500, 17_800)]),
        ]),
        Ok(vec![raw(&[
            ("we", 8_500, 8_750),
            ("should", 8_800, 9_050),
            ("ship", 9_100, 9_350),
        ])]),
    ]);
    let (meeting, mic, outcome) = run_scene(
        &svc,
        CAMPPLUS_ID,
        &[(0, 0, 6_000), (1, 8_150, 14_150), (1, 17_000, 18_500)],
        &[(1, 8_000, 14_000)],
        asr,
        whole_call(150),
        false,
    );
    assert_eq!(outcome.echo.text_removed, 3);
    assert_eq!(
        outcome.echo.speaker_removed, 0,
        "the check passed the cluster as a user voice (solo talk)"
    );
    assert_eq!(outcome.echo.cluster_removed, 1);
    assert_eq!(outcome.echo.removed(), 4);
    assert_eq!(mic_roles(&outcome), [(SpeakerRole::Me, "Me".to_owned())]);
    assert_eq!(mic_finals(&svc, meeting, &mic), ["hello", "all"]);
    let stored = crate::echo::echo_report(&svc.store(), meeting, TranscriptPass::Final)
        .unwrap()
        .unwrap();
    assert_eq!(stored, outcome.echo);
}

#[test]
fn post_echo_real_mic_voice_that_lost_most_words_to_echo_stays_a_voice() {
    // Negative control for the rule above: a colleague in the room (scene speaker 2)
    // whose words at 8.15-11 s happen to repeat the far end at the echo delay (removed by
    // the text rule), then "agreed". Most of the colleague's words were removed, but the
    // voice matches no far-end speaker: still a real mic voice, so two local voices and
    // no "Me" without a voiceprint.
    let dir = TempDir::new();
    let svc = services(&dir);
    let asr = FakeTranscriber::scripted([
        Ok(vec![
            raw(&[("hello", 1_000, 1_400), ("all", 1_500, 1_900)]),
            raw(&[
                ("we", 8_650, 8_900),
                ("should", 8_950, 9_200),
                ("ship", 9_250, 9_500),
                ("agreed", 10_200, 10_500),
            ]),
        ]),
        Ok(vec![raw(&[
            ("we", 8_500, 8_750),
            ("should", 8_800, 9_050),
            ("ship", 9_100, 9_350),
        ])]),
    ]);
    let (meeting, mic, outcome) = run_scene(
        &svc,
        CAMPPLUS_ID,
        &[(0, 0, 6_000), (2, 8_150, 11_000)],
        &[(1, 8_000, 14_000)],
        asr,
        whole_call(150),
        false,
    );
    assert_eq!(outcome.echo.text_removed, 3);
    assert_eq!(outcome.echo.cluster_removed, 0);
    assert_eq!(
        mic_roles(&outcome),
        [
            (SpeakerRole::Local, "Speaker 1".to_owned()),
            (SpeakerRole::Local, "Speaker 2".to_owned())
        ]
    );
    assert_eq!(mic_finals(&svc, meeting, &mic), ["hello", "all", "agreed"]);
}

/// A diarisation of `model` with one speaker per centroid; speaker `k` talks in `turns[k]`.
fn diar_of(centroids: &[Vec<f32>], turns: &[(u64, u64)]) -> Diarization {
    use earshot_core::diar::{DiarSpeaker, Embedding, SpeakerTurn};
    Diarization {
        speakers: centroids
            .iter()
            .map(|c| DiarSpeaker {
                centroid: Embedding::new(EmbeddingModelId::new(MODEL), c.clone()),
                speech: Duration::from_secs(1),
            })
            .collect(),
        turns: turns
            .iter()
            .enumerate()
            .map(|(k, &(s, e))| SpeakerTurn {
                speaker: k,
                start: MeetingTime::from_millis(s),
                end: MeetingTime::from_millis(e),
            })
            .collect(),
        embedding_model: EmbeddingModelId::new(MODEL),
    }
}

fn seg_of(words: &[(&str, u64, u64)]) -> Segment {
    let ws: Vec<Word> = words
        .iter()
        .map(|(t, s, e)| Word::new(*t, *s, *e, 0.9))
        .collect();
    Segment::from_words(
        EntityId::from_bytes([1; 16]),
        EntityId::from_bytes([2; 16]),
        ws,
        MeetingTime::ZERO,
    )
}

#[test]
fn post_echo_mostly_echo_rule_never_removes_every_mic_voice() {
    // Two mic voices that both lost most of their words and both match the far end's
    // speaker (cosine 0.71 each): dropping both would leave their remaining words without
    // a mic speaker, so neither is dropped by that rule. With one of them clean, only the
    // other is dropped, and only while its kept words lie inside a cleaning stretch.
    let a = vec![1.0, 0.0, 0.0];
    let b = vec![0.0, 1.0, 0.0];
    let far = vec![0.6, 0.6, 0.0];
    let reference = diar_of(&[far], &[(0, 10_000)]);
    let stretches = vec![lock(0, 10_000, 150)];
    let before = vec![
        seg_of(&[("a1", 100, 200), ("a2", 300, 400), ("a3", 500, 600)]),
        seg_of(&[
            ("b1", 2_100, 2_200),
            ("b2", 2_300, 2_400),
            ("b3", 2_500, 2_600),
        ]),
    ];
    let after = vec![seg_of(&[("a3", 500, 600)]), seg_of(&[("b3", 2_500, 2_600)])];
    let mut d = diar_of(&[a.clone(), b.clone()], &[(0, 1_000), (2_000, 3_000)]);
    let got = drop_echo_clusters(
        &mut d,
        Some(&reference),
        0.5,
        &stretches,
        &before,
        &after,
        Some(&[0, 1]),
        None,
    )
    .unwrap();
    assert_eq!(d.speakers.len(), 2, "the last mic voices are kept");
    assert_eq!(got.me, Some(vec![0, 1]));
    assert_eq!(got.cluster_words, [false, false]);

    // Voice a kept all its words: b alone is the echo cluster.
    let after = vec![before[0].clone(), seg_of(&[("b3", 2_500, 2_600)])];
    let mut d = diar_of(&[a.clone(), b], &[(0, 1_000), (2_000, 3_000)]);
    let got = drop_echo_clusters(
        &mut d,
        Some(&reference),
        0.5,
        &stretches,
        &before,
        &after,
        Some(&[0, 1]),
        None,
    )
    .unwrap();
    assert_eq!(d.speakers.len(), 1);
    assert_eq!(d.speakers[0].centroid.vector, a);
    assert!(d.turns.iter().all(|t| t.speaker == 0));
    assert_eq!(got.me, Some(vec![0]));
    // b's kept word is a far-end word, marked for removal; a's words stay.
    assert_eq!(got.cluster_words, [false, false, false, true]);

    // A kept word outside every cleaning stretch was never judged by echo cleaning: b
    // stays a voice.
    let mut d = diar_of(
        &[a.clone(), vec![0.0, 1.0, 0.0]],
        &[(0, 1_000), (2_000, 3_000)],
    );
    let short = vec![lock(0, 2_400, 150)];
    let got = drop_echo_clusters(
        &mut d,
        Some(&reference),
        0.5,
        &short,
        &before,
        &after,
        Some(&[0, 1]),
        None,
    )
    .unwrap();
    assert_eq!(d.speakers.len(), 2);
    assert_eq!(got.me, Some(vec![0, 1]));
    assert!(!got.cluster_words.contains(&true));

    // Without a loopback diarisation nothing matches the far end: only the old rule.
    let mut d = diar_of(
        &[vec![1.0, 0.0, 0.0], vec![0.0, 1.0, 0.0]],
        &[(0, 1_000), (2_000, 3_000)],
    );
    let got = drop_echo_clusters(
        &mut d,
        None,
        0.5,
        &stretches,
        &before,
        &after,
        Some(&[0, 1]),
        None,
    )
    .unwrap();
    assert_eq!(d.speakers.len(), 2);
    assert_eq!(got.me, Some(vec![0, 1]));
    assert!(!got.cluster_words.contains(&true));

    // Centroids of different embedding models are an error, never "does not match".
    let mut other = diar_of(&[vec![0.6, 0.6, 0.0]], &[(0, 10_000)]);
    other.embedding_model = EmbeddingModelId::new("other@2");
    other.speakers[0].centroid.model = other.embedding_model.clone();
    let mut d = diar_of(&[a, vec![0.0, 1.0, 0.0]], &[(0, 1_000), (2_000, 3_000)]);
    assert!(matches!(
        drop_echo_clusters(
            &mut d,
            Some(&other),
            0.5,
            &stretches,
            &before,
            &after,
            Some(&[0, 1]),
            None,
        ),
        Err(DiarError::ModelMismatch { .. })
    ));
}

/// 10 ms frames; speech wherever any sample is non-zero (`scene_audio` silence is 0).
struct LevelVad;

impl earshot_core::asr::Vad for LevelVad {
    fn id(&self) -> String {
        "level-vad".into()
    }
    fn frame_len(&self) -> usize {
        160
    }
    fn speech_probability(
        &mut self,
        frame: &[earshot_core::audio::Sample],
    ) -> Result<f32, AsrError> {
        Ok(if frame.iter().any(|s| *s != 0.0) {
            1.0
        } else {
            0.0
        })
    }
    fn reset(&mut self) {}
}

#[test]
fn post_pieces_decode_speech_only_and_never_store_a_loop() {
    // The mic speaks at 0-8 s and 60-68 s with silence between (T6.16). Decoded in speech
    // pieces: the silence is never transcribed, piece times are shifted to meeting time,
    // and a piece whose decodes keep repeating is dropped and reported, not stored.
    let dir = TempDir::new();
    let svc = services(&dir);
    let (meeting, mic, _) = meeting_rows(&svc);
    let audio = scene_audio(
        &[
            SceneTurn::new(0, 0, 8_000),
            SceneTurn::new(0, 60_000, 68_000),
        ],
        90_000,
    );
    let looping = "I checked the valve. ".repeat(20);
    let loop_words: Vec<(&str, u64, u64)> = looping
        .split_whitespace()
        .enumerate()
        .map(|(i, w)| (w, 100 + 50 * i as u64, 140 + 50 * i as u64))
        .collect();
    let mut post = PostProcessor::new(
        FakeTranscriber::scripted([
            Ok(vec![raw(&[
                ("our", 1_000, 1_300),
                ("main", 1_400, 1_700),
                ("risk", 1_800, 2_100),
            ])]),
            // The second piece loops with beam search and again greedily: dropped.
            Ok(vec![raw(&loop_words)]),
            Ok(vec![raw(&loop_words)]),
        ]),
        FakeSegmenter::segmentation_3_0(),
        embedder(),
        config(),
    )
    .unwrap()
    .with_pieces(
        Box::new(LevelVad),
        earshot_core::asr::PieceConfig::default(),
    )
    .unwrap();
    assert!(post.decodes_in_pieces());
    let outcome = post
        .run(
            &svc,
            PostInput {
                echo_locks: Vec::new(),
                meeting,
                tracks: vec![track_input(&mic, audio)],
            },
            &mut ChaCha8Rng::seed_from_u64(41),
            &CancelToken::new(),
        )
        .unwrap();
    let words: Vec<String> = finals_of(&svc, meeting)
        .iter()
        .flat_map(|s| s.words.iter().map(|w| w.text.clone()))
        .collect();
    assert_eq!(words, ["our", "main", "risk"]);
    let first = &finals_of(&svc, meeting)[0];
    // The first piece starts 0.4 s before the speech, clamped at the start of the track.
    assert_eq!(first.words[0].start, MeetingTime::from_millis(1_000));
    let dropped: Vec<_> = outcome
        .warnings
        .iter()
        .filter_map(|w| match w {
            PostWarning::RepetitionDropped { track, spans } => Some((*track, spans.clone())),
            _ => None,
        })
        .collect();
    assert_eq!(
        dropped,
        vec![(
            TrackId::Mic,
            vec![(
                MeetingTime::from_millis(59_600),
                MeetingTime::from_millis(68_400)
            )]
        )]
    );
}

#[test]
fn post_whisper_non_speech_annotations_are_never_stored() {
    // T6.7: whisper's `[BLANK_AUDIO]` / `[MUSIC PLAYING]` annotations are not speech: a
    // segment of annotations only is not stored, and a mixed one keeps only its words.
    let dir = TempDir::new();
    let svc = services(&dir);
    let (meeting, mic, _) = meeting_rows(&svc);
    let mut post = PostProcessor::new(
        FakeTranscriber::scripted([Ok(vec![
            raw(&[("[BLANK_AUDIO]", 100, 900)]),
            raw(&[
                ("Thanks", 1_000, 1_300),
                ("everyone.", 1_400, 1_700),
                ("[MUSIC", 1_800, 1_900),
                ("PLAYING]", 1_900, 2_000),
            ]),
        ])]),
        FakeSegmenter::segmentation_3_0(),
        embedder(),
        config(),
    )
    .unwrap();
    post.run(
        &svc,
        PostInput {
            echo_locks: Vec::new(),
            meeting,
            tracks: vec![track_input(
                &mic,
                scene_audio(&[SceneTurn::new(0, 0, 8_000)], 8_000),
            )],
        },
        &mut ChaCha8Rng::seed_from_u64(43),
        &CancelToken::new(),
    )
    .unwrap();
    let finals = finals_of(&svc, meeting);
    let texts: Vec<&str> = finals.iter().map(|s| s.text.as_str()).collect();
    assert_eq!(texts, ["Thanks everyone."]);
}

/// The progress names of a two-track run (`second`: with a second diarizer).
fn progress_names(second: bool) -> Vec<String> {
    let dir = TempDir::new();
    let svc = services(&dir);
    let (meeting, mic, them) = meeting_rows(&svc);
    let (mic_audio, them_audio) = scene();
    let mut post = PostProcessor::new(
        transcriber(),
        FakeSegmenter::segmentation_3_0(),
        embedder(),
        config(),
    )
    .unwrap();
    if second {
        post = post
            .with_second_diarizer(FakeSegmenter::segmentation_3_0(), embedder())
            .unwrap();
        assert!(post.diarises_tracks_in_parallel());
    }
    let mut stages = Vec::new();
    post.run_reporting(
        &svc,
        PostInput {
            echo_locks: Vec::new(),
            meeting,
            tracks: vec![track_input(&mic, mic_audio), track_input(&them, them_audio)],
        },
        &mut ChaCha8Rng::seed_from_u64(47),
        &CancelToken::new(),
        &mut |s| stages.push(s),
    )
    .unwrap();
    stages
        .iter()
        .map(|s| match s {
            PostStage::Transcribed { track, .. } => format!("transcribed {track:?}"),
            PostStage::Diarised {
                track, speakers, ..
            } => format!("diarised {track:?} {speakers}"),
            PostStage::EchoChecked { .. } => "echo".into(),
            PostStage::Stored { .. } => "stored".into(),
        })
        .collect()
}

/// The names of `kind` in `names`, in order.
fn of_kind<'a>(names: &'a [String], kind: &str) -> Vec<&'a str> {
    names
        .iter()
        .map(String::as_str)
        .filter(|n| n.starts_with(kind))
        .collect()
}

#[test]
fn post_progress_reports_each_step_in_order() {
    // T6.18: the host is told when each cleanup step finishes. Transcription and
    // diarisation run at the same time (T6.12), so their steps interleave in any order; the
    // order `PostStage` promises is per step kind: each kind in track order, every step
    // exactly once, and the echo check then storing last.
    let names = progress_names(false);
    assert_eq!(
        of_kind(&names, "transcribed "),
        ["transcribed Mic", "transcribed Reference"],
        "{names:?}"
    );
    assert_eq!(
        of_kind(&names, "diarised "),
        ["diarised Mic 1", "diarised Reference 2"],
        "{names:?}"
    );
    assert_eq!(names.len(), 6, "{names:?}");
    assert_eq!(names[4..], ["echo", "stored"], "{names:?}");
}

#[test]
fn post_progress_with_a_second_diarizer_reports_each_step_once() {
    // With a second diarizer the two tracks are diarised at the same time: their steps
    // come in either order (`PostStage`), each exactly once; the rest is unchanged.
    let names = progress_names(true);
    assert_eq!(
        of_kind(&names, "transcribed "),
        ["transcribed Mic", "transcribed Reference"],
        "{names:?}"
    );
    let mut diarised = of_kind(&names, "diarised ");
    diarised.sort_unstable();
    assert_eq!(
        diarised,
        ["diarised Mic 1", "diarised Reference 2"],
        "{names:?}"
    );
    assert_eq!(names.len(), 6, "{names:?}");
    assert_eq!(names[4..], ["echo", "stored"], "{names:?}");
}

/// Two parties that each record their own start and wait (bounded) for the other's.
#[derive(Default)]
struct Meet {
    state: std::sync::Mutex<[bool; 2]>,
    cv: std::sync::Condvar,
}

impl Meet {
    /// Marks `me` as started and waits up to `MEET_WAIT` for `other`; whether it came.
    fn arrive(&self, me: usize, other: usize) -> bool {
        let mut s = self.state.lock().unwrap();
        s[me] = true;
        self.cv.notify_all();
        let (s, _) = self
            .cv
            .wait_timeout_while(s, MEET_WAIT, |s| !s[other])
            .unwrap();
        s[other]
    }
}

/// Only reached when the two stages do not overlap (the failing case).
const MEET_WAIT: Duration = Duration::from_secs(5);

struct MeetingTranscriber {
    inner: FakeTranscriber,
    meet: Arc<Meet>,
    met: Arc<std::sync::atomic::AtomicBool>,
}

impl earshot_core::asr::Transcriber for MeetingTranscriber {
    fn transcribe(
        &mut self,
        audio: &MonoBuffer,
        options: &TranscribeOptions,
        cancel: &CancelToken,
    ) -> Result<earshot_core::asr::Transcription, AsrError> {
        if self.meet.arrive(0, 1) {
            self.met.store(true, std::sync::atomic::Ordering::SeqCst);
        }
        self.inner.transcribe(audio, options, cancel)
    }
    fn backend_info(&self) -> earshot_core::asr::BackendInfo {
        self.inner.backend_info()
    }
}

struct MeetingSegmenter {
    inner: FakeSegmenter,
    meet: Arc<Meet>,
    met: Arc<std::sync::atomic::AtomicBool>,
}

impl earshot_core::diar::SpeakerSegmenter for MeetingSegmenter {
    fn info(&self) -> &earshot_core::diar::SegmenterInfo {
        self.inner.info()
    }
    fn segment(
        &mut self,
        windows: &[&[f32]],
        cancel: &CancelToken,
    ) -> Result<Vec<earshot_core::diar::LocalActivity>, earshot_core::diar::DiarError> {
        if self.meet.arrive(1, 0) {
            self.met.store(true, std::sync::atomic::Ordering::SeqCst);
        }
        self.inner.segment(windows, cancel)
    }
}

#[test]
fn post_transcription_and_diarisation_run_at_the_same_time_with_the_same_result() {
    let run = |parallel_probe: bool| {
        let dir = TempDir::new();
        let svc = services(&dir);
        let (meeting, mic, them) = meeting_rows(&svc);
        enroll_alice(&svc, 1);
        let (mic_audio, them_audio) = scene();
        // With the probe both stages share one meeting point; without it each gets one that
        // is always open, so nothing waits.
        let open = || {
            let m = Meet::default();
            *m.state.lock().unwrap() = [true, true];
            Arc::new(m)
        };
        let meet = Arc::new(Meet::default());
        let asr_met = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let diar_met = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let mut post = PostProcessor::new(
            MeetingTranscriber {
                inner: transcriber(),
                meet: if parallel_probe {
                    Arc::clone(&meet)
                } else {
                    open()
                },
                met: Arc::clone(&asr_met),
            },
            MeetingSegmenter {
                inner: FakeSegmenter::segmentation_3_0(),
                meet: if parallel_probe {
                    Arc::clone(&meet)
                } else {
                    open()
                },
                met: Arc::clone(&diar_met),
            },
            embedder(),
            config(),
        )
        .unwrap();
        let input = PostInput {
            echo_locks: Vec::new(),
            meeting,
            tracks: vec![track_input(&mic, mic_audio), track_input(&them, them_audio)],
        };
        let mut stages = Vec::new();
        let outcome = post
            .run_reporting(
                &svc,
                input,
                &mut ChaCha8Rng::seed_from_u64(3),
                &CancelToken::new(),
                &mut |s| stages.push(s),
            )
            .unwrap();
        let finals: Vec<(String, Option<EntityId>)> =
            live_segments(&**svc.store(), meeting, TranscriptPass::Final)
                .unwrap()
                .iter()
                .map(|r| (r.segment.text.clone(), r.segment.speaker_id))
                .collect();
        let names: Vec<String> = outcome
            .speakers
            .iter()
            .map(|s| s.display_name.clone())
            .collect();
        (
            asr_met.load(std::sync::atomic::Ordering::SeqCst),
            diar_met.load(std::sync::atomic::Ordering::SeqCst),
            names,
            finals.len(),
            stages.len(),
        )
    };
    // The transcriber's first call and the segmenter's first call meet: each waits for the
    // other to have started, which only happens when the two stages overlap.
    let (asr_met, diar_met, names, finals, stages) = run(true);
    assert!(
        asr_met && diar_met,
        "transcription and diarisation did not overlap"
    );
    // Same outcome as the reference test of the sequential design
    // (post_final_pass_diarises_names_and_stores_speakers).
    assert_eq!(names, ["Me", "Alice", "Speaker 1"]);
    assert_eq!(finals, 6);
    // Every stage reported: 2 transcribed, 2 diarised, echo checked, stored.
    assert_eq!(stages, 6);
    // And the result does not depend on how the two threads interleave.
    let (_, _, names2, finals2, _) = run(false);
    assert_eq!((names2, finals2), (names, finals));
}

#[test]
fn post_a_failed_transcription_stops_the_diarisation_and_writes_nothing() {
    let dir = TempDir::new();
    let svc = services(&dir);
    let (meeting, mic, them) = meeting_rows(&svc);
    let (mic_audio, them_audio) = scene();
    let mut post = PostProcessor::new(
        FakeTranscriber::scripted([Err(AsrError::Backend("broken".into()))]),
        FakeSegmenter::segmentation_3_0(),
        embedder(),
        config(),
    )
    .unwrap();
    let input = PostInput {
        echo_locks: Vec::new(),
        meeting,
        tracks: vec![track_input(&mic, mic_audio), track_input(&them, them_audio)],
    };
    let err = post
        .run(
            &svc,
            input,
            &mut ChaCha8Rng::seed_from_u64(3),
            &CancelToken::new(),
        )
        .unwrap_err();
    // The transcription's own error, not the diarisation's cancellation.
    assert!(
        matches!(err, RuntimeError::Asr(AsrError::Backend(_))),
        "{err:?}"
    );
    assert!(
        live_segments(&**svc.store(), meeting, TranscriptPass::Final)
            .unwrap()
            .is_empty()
    );
    assert!(svc.store().speakers(meeting).unwrap().is_empty());
}

#[test]
fn post_two_tracks_are_diarised_at_the_same_time_on_two_model_copies() {
    let run = |second: bool| {
        let dir = TempDir::new();
        let svc = services(&dir);
        let (meeting, mic, them) = meeting_rows(&svc);
        enroll_alice(&svc, 1);
        let (mic_audio, them_audio) = scene();
        let meet = Arc::new(Meet::default());
        let met = [
            Arc::new(std::sync::atomic::AtomicBool::new(false)),
            Arc::new(std::sync::atomic::AtomicBool::new(false)),
        ];
        // The first copy's segmenter is party 0 and the second's party 1: each waits for
        // the other, which only meets when the two tracks are diarised at the same time.
        let segmenter = |party: usize| MeetingSegmenter2 {
            inner: FakeSegmenter::segmentation_3_0(),
            meet: Arc::clone(&meet),
            party,
            met: Arc::clone(&met[party]),
        };
        let mut post =
            PostProcessor::new(transcriber(), segmenter(0), embedder(), config()).unwrap();
        if second {
            post = post.with_second_diarizer(segmenter(1), embedder()).unwrap();
        } else {
            // One copy: party 1 never arrives, so mark it present to keep the run fast.
            meet.state.lock().unwrap()[1] = true;
        }
        assert_eq!(post.diarises_tracks_in_parallel(), second);
        let input = PostInput {
            echo_locks: Vec::new(),
            meeting,
            tracks: vec![track_input(&mic, mic_audio), track_input(&them, them_audio)],
        };
        let outcome = post
            .run(
                &svc,
                input,
                &mut ChaCha8Rng::seed_from_u64(3),
                &CancelToken::new(),
            )
            .unwrap();
        let finals: Vec<(String, Option<EntityId>)> =
            live_segments(&**svc.store(), meeting, TranscriptPass::Final)
                .unwrap()
                .iter()
                .map(|r| (r.segment.text.clone(), r.segment.speaker_id))
                .collect();
        let speakers: Vec<(SpeakerRole, TrackId, String)> = outcome
            .speakers
            .iter()
            .map(|s| (s.role, s.track, s.display_name.clone()))
            .collect();
        let both_met = met
            .iter()
            .all(|m| m.load(std::sync::atomic::Ordering::SeqCst));
        (both_met, speakers, finals.len())
    };
    let (met, speakers, finals) = run(true);
    assert!(met, "the two tracks were not diarised at the same time");
    let (_, speakers1, finals1) = run(false);
    assert_eq!((speakers, finals), (speakers1, finals1));
    // A second copy of a different embedding model is refused.
    let other = FakeEmbedder::new("other@1", vec![centroid(0)], 0.05, 11);
    let post = PostProcessor::new(
        transcriber(),
        FakeSegmenter::segmentation_3_0(),
        embedder(),
        config(),
    )
    .unwrap();
    assert!(
        post.with_second_diarizer(FakeSegmenter::segmentation_3_0(), other)
            .is_err()
    );
}

struct MeetingSegmenter2 {
    inner: FakeSegmenter,
    meet: Arc<Meet>,
    party: usize,
    met: Arc<std::sync::atomic::AtomicBool>,
}

impl earshot_core::diar::SpeakerSegmenter for MeetingSegmenter2 {
    fn info(&self) -> &earshot_core::diar::SegmenterInfo {
        self.inner.info()
    }
    fn segment(
        &mut self,
        windows: &[&[f32]],
        cancel: &CancelToken,
    ) -> Result<Vec<earshot_core::diar::LocalActivity>, earshot_core::diar::DiarError> {
        if self.meet.arrive(self.party, 1 - self.party) {
            self.met.store(true, std::sync::atomic::Ordering::SeqCst);
        }
        self.inner.segment(windows, cancel)
    }
}

/// Cancels the run's outer token on its first call, then waits (bounded) for the token it
/// was given to be cancelled too, and records whether it was.
struct CancellingTranscriber {
    outer: CancelToken,
    saw_inner_cancel: Arc<std::sync::atomic::AtomicBool>,
}

impl earshot_core::asr::Transcriber for CancellingTranscriber {
    fn transcribe(
        &mut self,
        _audio: &MonoBuffer,
        _options: &TranscribeOptions,
        cancel: &CancelToken,
    ) -> Result<earshot_core::asr::Transcription, AsrError> {
        self.outer.cancel();
        let started = std::time::Instant::now();
        while !cancel.is_cancelled() && started.elapsed() < MEET_WAIT {
            std::thread::sleep(Duration::from_millis(5));
        }
        self.saw_inner_cancel
            .store(cancel.is_cancelled(), std::sync::atomic::Ordering::SeqCst);
        Err(AsrError::Cancelled)
    }
    fn backend_info(&self) -> earshot_core::asr::BackendInfo {
        FakeTranscriber::scripted([]).backend_info()
    }
}

#[test]
fn post_an_outer_cancel_reaches_both_workers_and_writes_nothing() {
    let dir = TempDir::new();
    let svc = services(&dir);
    let (meeting, mic, them) = meeting_rows(&svc);
    let (mic_audio, them_audio) = scene();
    let outer = CancelToken::new();
    let saw = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let mut post = PostProcessor::new(
        CancellingTranscriber {
            outer: outer.clone(),
            saw_inner_cancel: Arc::clone(&saw),
        },
        FakeSegmenter::segmentation_3_0(),
        embedder(),
        config(),
    )
    .unwrap();
    let input = PostInput {
        echo_locks: Vec::new(),
        meeting,
        tracks: vec![track_input(&mic, mic_audio), track_input(&them, them_audio)],
    };
    let err = post
        .run(&svc, input, &mut ChaCha8Rng::seed_from_u64(3), &outer)
        .unwrap_err();
    assert!(
        matches!(err, RuntimeError::Asr(AsrError::Cancelled)),
        "{err:?}"
    );
    // The worker's own token was cancelled from the caller's token.
    assert!(saw.load(std::sync::atomic::Ordering::SeqCst));
    assert!(
        live_segments(&**svc.store(), meeting, TranscriptPass::Final)
            .unwrap()
            .is_empty()
    );
}

/// Bounds only a failing gated test (a saturated host can take seconds to start a pass).
const GATE_WAIT: Duration = Duration::from_secs(120);

/// Reports its first call, then waits (bounded) for a go before answering like `inner`.
struct GatedTranscriber {
    inner: FakeTranscriber,
    started: Option<std::sync::mpsc::Sender<()>>,
    go: std::sync::mpsc::Receiver<()>,
}

impl earshot_core::asr::Transcriber for GatedTranscriber {
    fn transcribe(
        &mut self,
        audio: &MonoBuffer,
        options: &TranscribeOptions,
        cancel: &CancelToken,
    ) -> Result<earshot_core::asr::Transcription, AsrError> {
        if let Some(started) = self.started.take() {
            started.send(()).unwrap();
            self.go
                .recv_timeout(GATE_WAIT)
                .expect("the test lets the pass go on");
        }
        self.inner.transcribe(audio, options, cancel)
    }
    fn backend_info(&self) -> earshot_core::asr::BackendInfo {
        self.inner.backend_info()
    }
}

#[test]
fn post_meeting_cannot_be_deleted_while_the_final_pass_runs() {
    // T6.26: the final pass holds the meeting; a deletion meanwhile is refused (retryable)
    // and succeeds once the pass has stored its result.
    let dir = TempDir::new();
    let svc = services(&dir);
    let (meeting, mic, them) = meeting_rows(&svc);
    let (mic_audio, them_audio) = scene();
    let (started_tx, started) = std::sync::mpsc::channel();
    let (go, go_rx) = std::sync::mpsc::channel();
    let mut post = PostProcessor::new(
        GatedTranscriber {
            inner: transcriber(),
            started: Some(started_tx),
            go: go_rx,
        },
        FakeSegmenter::segmentation_3_0(),
        embedder(),
        config(),
    )
    .unwrap();
    let input = PostInput {
        echo_locks: Vec::new(),
        meeting,
        tracks: vec![track_input(&mic, mic_audio), track_input(&them, them_audio)],
    };
    let svc2 = svc.clone();
    let pass = std::thread::spawn(move || {
        post.run(
            &svc2,
            input,
            &mut ChaCha8Rng::seed_from_u64(3),
            &CancelToken::new(),
        )
        .map(|o| o.final_segments)
    });
    started.recv_timeout(GATE_WAIT).expect("the pass started");
    let err = svc.delete_meeting(meeting).unwrap_err();
    assert!(
        matches!(err, RuntimeError::MeetingBusy(m) if m == meeting),
        "{err:?}"
    );
    assert!(err.is_retryable());
    go.send(()).unwrap();
    assert_eq!(pass.join().unwrap().unwrap(), 6);
    svc.delete_meeting(meeting).unwrap();
    let db = svc.store();
    assert!(db.segments(meeting).unwrap().iter().all(|s| s.meta.deleted));
    assert!(db.speakers(meeting).unwrap().iter().all(|s| s.meta.deleted));
    assert!(
        db.local_keys()
            .unwrap()
            .iter()
            .all(|k| crate::local::meeting_of(k) != Some(meeting)),
        "{:?}",
        db.local_keys().unwrap()
    );
}

#[test]
fn post_final_pass_of_a_meeting_deleted_elsewhere_writes_nothing() {
    // Another process (no shared lease table) deleted the meeting: the write transaction
    // refuses it.
    let dir = TempDir::new();
    let svc = services(&dir);
    let (meeting, mic, them) = meeting_rows(&svc);
    svc.delete_meeting(meeting).unwrap();
    let (mic_audio, them_audio) = scene();
    let mut post = PostProcessor::new(
        transcriber(),
        FakeSegmenter::segmentation_3_0(),
        embedder(),
        config(),
    )
    .unwrap();
    let input = PostInput {
        echo_locks: Vec::new(),
        meeting,
        tracks: vec![track_input(&mic, mic_audio), track_input(&them, them_audio)],
    };
    let err = post
        .run(
            &svc,
            input,
            &mut ChaCha8Rng::seed_from_u64(3),
            &CancelToken::new(),
        )
        .unwrap_err();
    assert!(matches!(err, RuntimeError::Conflict(_)), "{err:?}");
    let db = svc.store();
    assert!(db.segments(meeting).unwrap().is_empty());
    assert!(db.speakers(meeting).unwrap().is_empty());
    assert!(
        db.local_keys()
            .unwrap()
            .iter()
            .all(|k| crate::local::meeting_of(k) != Some(meeting))
    );
}

struct FailingSegmenter(FakeSegmenter);

impl earshot_core::diar::SpeakerSegmenter for FailingSegmenter {
    fn info(&self) -> &earshot_core::diar::SegmenterInfo {
        self.0.info()
    }
    fn segment(
        &mut self,
        _windows: &[&[f32]],
        _cancel: &CancelToken,
    ) -> Result<Vec<earshot_core::diar::LocalActivity>, earshot_core::diar::DiarError> {
        Err(earshot_core::diar::DiarError::Config("broken".into()))
    }
}

#[test]
fn post_a_failed_diarisation_is_reported_over_the_induced_cancellation() {
    let dir = TempDir::new();
    let svc = services(&dir);
    let (meeting, mic, them) = meeting_rows(&svc);
    let (mic_audio, them_audio) = scene();
    let mut post = PostProcessor::new(
        transcriber(),
        FailingSegmenter(FakeSegmenter::segmentation_3_0()),
        embedder(),
        config(),
    )
    .unwrap();
    let input = PostInput {
        echo_locks: Vec::new(),
        meeting,
        tracks: vec![track_input(&mic, mic_audio), track_input(&them, them_audio)],
    };
    let err = post
        .run(
            &svc,
            input,
            &mut ChaCha8Rng::seed_from_u64(3),
            &CancelToken::new(),
        )
        .unwrap_err();
    assert!(
        matches!(
            err,
            RuntimeError::Diar(earshot_core::diar::DiarError::Config(_))
        ),
        "{err:?}"
    );
    assert!(
        live_segments(&**svc.store(), meeting, TranscriptPass::Final)
            .unwrap()
            .is_empty()
    );
}

/// T6.25: speaker identities survive a re-run of the cleanup.
mod identity_tests;

/// Audio with `level` (peak) over each of `spans`, silence elsewhere.
fn level_audio(total_ms: u64, spans: &[(u64, u64, f32)]) -> MonoBuffer {
    let mut samples = vec![0.0_f32; total_ms as usize * 16];
    for &(from, to, level) in spans {
        for (i, s) in samples[from as usize * 16..to as usize * 16]
            .iter_mut()
            .enumerate()
        {
            *s = if i % 2 == 0 { level } else { -level };
        }
    }
    MonoBuffer::new(earshot_core::audio::PIPELINE_RATE, samples)
}

/// The scene of T9.1i: the user speaks at -20 dBFS at 0-10 s and a second mic voice has three
/// one-word fragments at `level` at `fragments`; the far end speaks at 20-30 s. Returns what the
/// rule did: the voices left and the words marked for removal.
fn quiet_scene(level: f32, fragments: [u64; 3], extra_user_words: usize) -> (usize, Vec<bool>) {
    let me: Vec<(String, u64, u64)> = (0..40 + extra_user_words as u64)
        .map(|i| (format!("me{i}"), i * 200, i * 200 + 150))
        .collect();
    let me_words: Vec<(&str, u64, u64)> = me.iter().map(|(t, s, e)| (t.as_str(), *s, *e)).collect();
    let after = vec![
        seg_of(&me_words),
        seg_of(&[
            ("f1", fragments[0], fragments[0] + 300),
            ("f2", fragments[1], fragments[1] + 300),
            ("f3", fragments[2], fragments[2] + 300),
        ]),
    ];
    let end_of_user = 200 * (40 + extra_user_words as u64);
    let mut spans = vec![(0, end_of_user, 0.1)];
    for f in fragments {
        spans.push((f, f + 300, level));
    }
    let audio = level_audio(60_000, &spans);
    let mut d = diar_of(
        &[vec![1.0, 0.0, 0.0], vec![0.0, 1.0, 0.0]],
        &[
            (0, end_of_user),
            (fragments[0], fragments[0] + 300),
            (fragments[1], fragments[1] + 300),
            (fragments[2], fragments[2] + 300),
        ],
    );
    // diar_of gives each turn its own speaker index: all the fragments are the second voice.
    for t in d.turns.iter_mut().skip(1) {
        t.speaker = 1;
    }
    let reference = diar_of(&[vec![0.0, 0.0, 1.0]], &[(20_000, 30_000)]);
    let got = drop_echo_clusters(
        &mut d,
        Some(&reference),
        0.5,
        &[lock(0, 60_000, 150)],
        &after,
        &after,
        Some(&[0, 1]),
        Some(&audio),
    )
    .unwrap();
    (d.speakers.len(), got.cluster_words)
}

#[test]
fn post_echo_quiet_voice_over_the_far_end_is_dropped_with_its_words() {
    let (voices, marked) = quiet_scene(0.01, [21_000, 24_000, 28_000], 0);
    assert_eq!(voices, 1);
    assert_eq!(marked.iter().filter(|m| **m).count(), 3);
    assert!(marked[..40].iter().all(|m| !m), "the user's words stay");
}

#[test]
fn post_echo_quiet_voice_needs_all_three_tests() {
    // A person across the room: just as quiet, but talking while the far end is silent.
    let (voices, marked) = quiet_scene(0.01, [40_000, 45_000, 50_000], 0);
    assert_eq!((voices, marked.iter().any(|m| *m)), (2, false));
    // Over the far end, but not quiet (9.5 dB below the user is not 10).
    let (voices, marked) = quiet_scene(0.0335, [21_000, 24_000, 28_000], 0);
    assert_eq!((voices, marked.iter().any(|m| *m)), (2, false));
    // Just past that line it is.
    let (voices, _) = quiet_scene(0.0316, [21_000, 24_000, 28_000], 0);
    assert_eq!(voices, 1);
    // One fragment of three is not "mostly over the far end".
    let (voices, _) = quiet_scene(0.01, [21_000, 40_000, 50_000], 0);
    assert_eq!(voices, 2);
}

#[test]
fn post_echo_quiet_voice_with_a_real_share_of_the_words_is_kept() {
    // Three words of the mic's 13 is too much to be residue.
    let got = {
        let after = vec![
            seg_of(&[
                ("a", 100, 400),
                ("b", 500, 800),
                ("c", 900, 1_200),
                ("d", 1_300, 1_600),
                ("e", 1_700, 2_000),
                ("g", 2_100, 2_400),
                ("h", 2_500, 2_800),
                ("i", 2_900, 3_200),
                ("j", 3_300, 3_600),
                ("k", 3_700, 4_000),
            ]),
            seg_of(&[
                ("f1", 21_000, 21_300),
                ("f2", 24_000, 24_300),
                ("f3", 28_000, 28_300),
            ]),
        ];
        let audio = level_audio(
            60_000,
            &[
                (0, 4_000, 0.1),
                (21_000, 21_300, 0.01),
                (24_000, 24_300, 0.01),
                (28_000, 28_300, 0.01),
            ],
        );
        let mut d = diar_of(
            &[vec![1.0, 0.0, 0.0], vec![0.0, 1.0, 0.0]],
            &[(0, 4_000), (21_000, 28_400)],
        );
        let reference = diar_of(&[vec![0.0, 0.0, 1.0]], &[(20_000, 30_000)]);
        drop_echo_clusters(
            &mut d,
            Some(&reference),
            0.5,
            &[lock(0, 60_000, 150)],
            &after,
            &after,
            Some(&[0, 1]),
            Some(&audio),
        )
        .unwrap();
        d.speakers.len()
    };
    assert_eq!(got, 2);
}

#[test]
fn post_echo_quiet_voice_is_never_judged_without_an_echo_path_or_audio() {
    let after = vec![seg_of(&[("a", 100, 400)]), seg_of(&[("f", 21_000, 21_300)])];
    let audio = level_audio(60_000, &[(0, 400, 0.1), (21_000, 21_300, 0.01)]);
    let reference = diar_of(&[vec![0.0, 0.0, 1.0]], &[(20_000, 30_000)]);
    for (stretches, audio) in [
        (vec![], Some(&audio)),
        (vec![lock(0, 60_000, 150)], None),
        // Outside every stretch nothing is attributed to echo.
        (vec![lock(0, 10_000, 150)], Some(&audio)),
    ] {
        let mut d = diar_of(
            &[vec![1.0, 0.0, 0.0], vec![0.0, 1.0, 0.0]],
            &[(0, 400), (21_000, 21_300)],
        );
        drop_echo_clusters(
            &mut d,
            Some(&reference),
            0.5,
            &stretches,
            &after,
            &after,
            Some(&[0, 1]),
            audio,
        )
        .unwrap();
        assert_eq!(d.speakers.len(), 2);
    }
}
