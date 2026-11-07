//! Tests of "your voice" (T7.4h): recording from replay sources and enrolment with fake
//! speaker models. Every name contains `my_voice`.

use std::num::NonZeroU16;

use earshot_core::audio::SampleRate;
use earshot_core::capture::{ReplayBuilder, ReplaySource, TrackBuffer};
use earshot_core::diar::{
    DiarizationConfig, FakeEmbedder, FakeSegmenter, MAX_SCENE_SPEAKERS, SceneTurn, scene_audio,
};
use earshot_core::voiceprint::{EnrolmentConfig, VoiceprintRepo};

use super::*;
use crate::post::ModelThresholds;
use crate::testutil::{TempDir, services};

const MODEL: &str = "fake-emb@1";
const DIM: usize = 16;

fn embedder() -> FakeEmbedder {
    let centroids = (0..MAX_SCENE_SPEAKERS)
        .map(|k| (0..DIM).map(|i| if i == k { 1.0 } else { 0.0 }).collect())
        .collect();
    FakeEmbedder::new(MODEL, centroids, 0.05, 11)
}

fn config() -> PostConfig {
    PostConfig {
        diarization: DiarizationConfig::default(),
        clustering_thresholds: ModelThresholds::new()
            .with(EmbeddingModelId::new(MODEL), 0.704_565_5),
        voiceprint_thresholds: ModelThresholds::new().with(EmbeddingModelId::new(MODEL), 0.5),
        enrolment: EnrolmentConfig::default(),
        ..PostConfig::default()
    }
}

fn replay(audio: &MonoBuffer) -> ReplaySource {
    ReplayBuilder::new(DeviceTime::from_ticks(1_000_000), audio.clone())
        .build()
        .unwrap()
}

fn record(source: &mut ReplaySource, cancel: &CancelToken) -> Option<MonoBuffer> {
    record_sample(source, SAMPLE_LENGTH, cancel, &mut || {}, &mut |_| {}).unwrap()
}

/// One speaker (scene speaker `k`) talking from 1 s to `until_ms`, in a 40 s sample.
fn alone(k: usize, until_ms: u64) -> MonoBuffer {
    scene_audio(&[SceneTurn::new(k, 1_000, until_ms)], 40_000)
}

fn enrol(svc: &Services, audio: &MonoBuffer) -> MyVoiceOutcome {
    enrol_my_voice(
        svc,
        FakeSegmenter::segmentation_3_0(),
        embedder(),
        audio,
        &config(),
        &CancelToken::new(),
    )
    .unwrap()
}

#[test]
fn my_voice_record_takes_the_sample_length_of_the_microphone_bit_for_bit_at_16k() {
    let audio = alone(0, 39_000);
    let mut source = replay(&audio);
    let mut seen = Vec::new();
    let got = record_sample(
        &mut source,
        SAMPLE_LENGTH,
        &CancelToken::new(),
        &mut || {},
        &mut |d| seen.push(d),
    )
    .unwrap()
    .unwrap();
    assert_eq!(got.rate, PIPELINE_RATE);
    let wanted = 16 * SAMPLE_LENGTH.as_millis() as usize;
    assert_eq!(got.samples, quantise_pcm16(&audio.samples[..wanted]));
    assert!(seen.windows(2).all(|w| w[0] <= w[1]), "progress only grows");
    assert_eq!(seen.last(), Some(&SAMPLE_LENGTH));
}

#[test]
fn my_voice_record_resamples_a_48k_microphone_and_mixes_it_to_mono() {
    let rate = SampleRate::new(48_000).unwrap();
    // 35 s of a 440 Hz tone in two identical channels.
    let frames = 48_000 * 35;
    let mut samples = Vec::with_capacity(frames * 2);
    for i in 0..frames {
        let x = 0.25 * (2.0 * std::f32::consts::PI * 440.0 * i as f32 / 48_000.0).sin();
        samples.extend([x, x]);
    }
    let track = TrackBuffer::new(rate, NonZeroU16::new(2).unwrap(), samples).unwrap();
    let mut source = ReplayBuilder::new(DeviceTime::from_ticks(1), track)
        .build()
        .unwrap();
    let got = record(&mut source, &CancelToken::new()).unwrap();
    assert_eq!(got.rate, PIPELINE_RATE);
    let wanted = 16 * SAMPLE_LENGTH.as_millis() as usize;
    assert!(
        got.samples.len().abs_diff(wanted) <= 1,
        "{}",
        got.samples.len()
    );
    let peak = got.samples[16_000..]
        .iter()
        .fold(0f32, |m, x| m.max(x.abs()));
    assert!((peak - 0.25).abs() < 0.01, "peak {peak}");
}

#[test]
fn my_voice_record_gives_what_a_short_source_had_and_stops_on_cancel() {
    let short = scene_audio(&[SceneTurn::new(0, 0, 5_000)], 8_000);
    let got = record(&mut replay(&short), &CancelToken::new()).unwrap();
    assert_eq!(got.samples, quantise_pcm16(&short.samples));

    let cancel = CancelToken::new();
    cancel.cancel();
    let mut source = replay(&alone(0, 39_000));
    assert_eq!(record(&mut source, &cancel), None);
    // The source was stopped and drained to its end.
    let mut events: Vec<earshot_core::capture::CaptureEvent> = Vec::new();
    assert_eq!(source.pump(&mut events).unwrap(), SourceStatus::Ended);
}

#[test]
fn my_voice_record_refuses_a_microphone_that_changed_during_the_sample() {
    let audio = alone(0, 39_000);
    let mut source = ReplayBuilder::new(DeviceTime::from_ticks(1), audio.clone())
        .discontinuity(
            TrackId::Mic,
            16_000 * 5,
            DiscontinuityKind::DeviceChanged {
                device_name: Some("Headset".into()),
                rate: PIPELINE_RATE,
                warning: None,
            },
        )
        .unwrap()
        .build()
        .unwrap();
    let e = record_sample(
        &mut source,
        SAMPLE_LENGTH,
        &CancelToken::new(),
        &mut || {},
        &mut |_| {},
    )
    .unwrap_err();
    assert!(matches!(e, RuntimeError::Capture(_)), "{e:?}");
    // Lost data is no reason to record again.
    let mut source = ReplayBuilder::new(DeviceTime::from_ticks(1), audio)
        .discontinuity(TrackId::Mic, 16_000 * 5, DiscontinuityKind::DataLost)
        .unwrap()
        .build()
        .unwrap();
    assert!(record(&mut source, &CancelToken::new()).is_some());
}

#[test]
fn my_voice_enrols_the_user_from_a_recorded_sample_and_adds_later_ones() {
    let dir = TempDir::new("my-voice-enrol");
    let (svc, _) = services(&dir);
    assert_eq!(my_voice(&svc).unwrap(), MyVoice::default());
    assert!(!my_voice(&svc).unwrap().saved());
    let sample = record(&mut replay(&alone(3, 26_000)), &CancelToken::new()).unwrap();
    let MyVoiceOutcome::Enrolled {
        person,
        voiceprint,
        speech,
    } = enrol(&svc, &sample)
    else {
        panic!("not enrolled")
    };
    assert!(speech >= Duration::from_secs(20), "{speech:?}");
    {
        let db = svc.store();
        let p = db.person(person).unwrap().unwrap();
        assert!(p.is_me && p.is_live());
        let prints = db.voiceprints_of(person).unwrap();
        assert_eq!(prints.len(), 1);
        assert_eq!(prints[0].id, voiceprint);
        assert_eq!(prints[0].model, EmbeddingModelId::new(MODEL));
        // The voice of scene speaker 3.
        let best = prints[0]
            .sum
            .iter()
            .enumerate()
            .max_by(|a, b| a.1.total_cmp(b.1))
            .unwrap()
            .0;
        assert_eq!(best, 3);
    }
    let saved = my_voice(&svc).unwrap();
    assert!(saved.saved());
    // Stored in whole milliseconds.
    let ms = speech.as_millis();
    assert_eq!((saved.voiceprints, saved.speech.as_millis()), (1, ms));
    // A second sample adds to the same voiceprint of the same person.
    let again = enrol(&svc, &sample);
    let MyVoiceOutcome::Enrolled {
        person: p2,
        voiceprint: v2,
        ..
    } = again
    else {
        panic!("{again:?}")
    };
    assert_eq!((p2, v2), (person, voiceprint));
    let saved = my_voice(&svc).unwrap();
    assert_eq!((saved.voiceprints, saved.speech.as_millis()), (1, 2 * ms));
}

#[test]
fn my_voice_refuses_samples_it_cannot_trust_and_stores_nothing() {
    let dir = TempDir::new("my-voice-refuse");
    let (svc, _) = services(&dir);
    // Two people talking about equally.
    let two = scene_audio(
        &[
            SceneTurn::new(0, 0, 14_000),
            SceneTurn::new(1, 15_000, 29_000),
        ],
        30_000,
    );
    assert!(
        matches!(enrol(&svc, &two), MyVoiceOutcome::SeveralVoices { main, others }
            if main > Duration::ZERO && others > Duration::ZERO),
        "{:?}",
        enrol(&svc, &two)
    );
    // Silence.
    assert_eq!(
        enrol(&svc, &scene_audio(&[], 30_000)),
        MyVoiceOutcome::NoSpeech
    );
    // Too little speech for a first voiceprint.
    let short = alone(0, 6_000);
    assert!(
        matches!(enrol(&svc, &short), MyVoiceOutcome::NotEnoughSpeech { needed, got }
            if needed == Duration::from_secs(10) && got < needed),
        "{:?}",
        enrol(&svc, &short)
    );
    // A cancelled run.
    let cancel = CancelToken::new();
    cancel.cancel();
    let out = enrol_my_voice(
        &svc,
        FakeSegmenter::segmentation_3_0(),
        embedder(),
        &alone(0, 26_000),
        &config(),
        &cancel,
    )
    .unwrap();
    assert_eq!(out, MyVoiceOutcome::Cancelled);
    // A model with no calibrated threshold.
    let out = enrol_my_voice(
        &svc,
        FakeSegmenter::segmentation_3_0(),
        embedder(),
        &alone(0, 26_000),
        &PostConfig::default(),
        &CancelToken::new(),
    )
    .unwrap();
    assert_eq!(
        out,
        MyVoiceOutcome::Uncalibrated {
            model: EmbeddingModelId::new(MODEL)
        }
    );
    // Nothing was stored: no person, no voiceprint.
    let db = svc.store();
    assert!(db.persons().unwrap().is_empty());
    drop(db);
    assert_eq!(my_voice(&svc).unwrap(), MyVoice::default());
}

#[test]
fn my_voice_a_main_voice_with_a_little_of_another_is_enrolled() {
    let dir = TempDir::new("my-voice-main");
    let (svc, _) = services(&dir);
    // 24 s of the user and 4 s of someone else: under a third.
    let audio = scene_audio(
        &[
            SceneTurn::new(2, 0, 24_000),
            SceneTurn::new(5, 25_000, 29_000),
        ],
        30_000,
    );
    assert!(matches!(
        enrol(&svc, &audio),
        MyVoiceOutcome::Enrolled { speech, .. } if speech >= Duration::from_secs(20)
    ));
}

#[test]
fn my_voice_forget_zeroes_and_tombstones_every_voiceprint_of_the_user() {
    let dir = TempDir::new("my-voice-forget");
    let (svc, _) = services(&dir);
    assert_eq!(forget_my_voice(&svc).unwrap(), 0, "nothing to forget");
    let MyVoiceOutcome::Enrolled {
        person, voiceprint, ..
    } = enrol(&svc, &alone(1, 26_000))
    else {
        panic!("not enrolled")
    };
    assert_eq!(forget_my_voice(&svc).unwrap(), 1);
    assert_eq!(my_voice(&svc).unwrap(), MyVoice::default());
    let db = svc.store();
    let v = db
        .voiceprints_of(person)
        .unwrap()
        .into_iter()
        .find(|v| v.id == voiceprint)
        .unwrap();
    assert!(v.meta.deleted);
    assert!(v.sum.iter().all(|x| *x == 0.0));
    // The person stays (persons are kept), and can be enrolled again.
    assert!(db.person(person).unwrap().unwrap().is_me);
    drop(db);
    assert!(matches!(
        enrol(&svc, &alone(1, 26_000)),
        MyVoiceOutcome::Enrolled { person: p, .. } if p == person
    ));
}

/// The production path over real audio: one smoke speaker's clips (LibriSpeech 61) played
/// as a replay source, recorded and enrolled with the installed speaker models.
#[test]
#[ignore = "needs the installed speaker models and ONNX Runtime (cargo xtask fetch-models --set ci); run: cargo test -p earshot-runtime my_voice_installed -- --ignored"]
fn my_voice_installed_models_enrol_a_smoke_speaker() {
    let smoke = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/smoke");
    let mut clips: Vec<_> = std::fs::read_dir(&smoke)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| {
            p.file_name()
                .is_some_and(|n| n.to_string_lossy().starts_with("61-70968-"))
        })
        .collect();
    clips.sort();
    let mut samples = Vec::new();
    for clip in &clips {
        let (rate, s) = earshot_store::read_flac(clip).unwrap();
        assert_eq!(rate, 16_000, "{}", clip.display());
        samples.extend(s);
    }
    assert!(
        samples.len() >= 16 * SAMPLE_LENGTH.as_millis() as usize,
        "the smoke clips hold {} s",
        samples.len() / 16_000
    );
    let mut source = replay(&MonoBuffer::new(PIPELINE_RATE, samples));
    let sample = record(&mut source, &CancelToken::new()).unwrap();
    let dir = TempDir::new("my-voice-installed");
    let (svc, _) = services(&dir);
    let out = enrol_with_installed_models(&svc, &sample, &CancelToken::new(), &mut |a, e| {
        panic!("ONNX Runtime fell back from {a:?}: {e}")
    })
    .unwrap();
    assert!(
        matches!(out, MyVoiceOutcome::Enrolled { speech, .. } if speech >= Duration::from_secs(10)),
        "{out:?}"
    );
    assert!(my_voice(&svc).unwrap().saved());
}

#[test]
fn my_voice_forget_forgets_every_person_marked_as_the_user_at_once() {
    let dir = TempDir::new("my-voice-forget-all");
    let (svc, _) = services(&dir);
    let MyVoiceOutcome::Enrolled { person: first, .. } = enrol(&svc, &alone(1, 26_000)) else {
        panic!("not enrolled")
    };
    // A second "Me" (another device's, say) with its own voiceprint.
    let second = crate::edit::write(&svc, |tx, stamp| {
        let p = earshot_core::voiceprint::Person::new("Me", true, stamp);
        tx.put_person(p.clone()).map_err(RuntimeError::Voiceprint)?;
        let centroid = earshot_core::diar::Embedding::new(
            EmbeddingModelId::new(MODEL),
            (0..DIM).map(|i| if i == 2 { 1.0 } else { 0.0 }).collect(),
        );
        earshot_core::voiceprint::enroll(
            tx,
            p.id,
            &[centroid],
            Duration::from_secs(12),
            &EnrolmentConfig::default(),
            stamp,
        )
        .map_err(RuntimeError::Voiceprint)?;
        Ok(p.id)
    })
    .unwrap();
    assert_eq!(my_voice(&svc).unwrap().voiceprints, 2);
    assert_eq!(forget_my_voice(&svc).unwrap(), 2);
    assert_eq!(my_voice(&svc).unwrap(), MyVoice::default());
    let db = svc.store();
    for p in [first, second] {
        for v in db.voiceprints_of(p).unwrap() {
            assert!(v.meta.deleted && v.sum.iter().all(|x| *x == 0.0));
        }
    }
}

#[test]
fn my_name_is_stored_on_the_users_person_and_can_be_taken_back() {
    let dir = TempDir::new("my-name");
    let (svc, _) = services(&dir);
    assert_eq!(my_voice(&svc).unwrap().name, None);
    // No voice is needed, and giving a name does not count as having one.
    set_my_name(&svc, "  Casey Parker ").unwrap();
    let voice = my_voice(&svc).unwrap();
    assert_eq!(voice.name.as_deref(), Some("Casey Parker"));
    assert!(!voice.saved());
    // One person, however often it is set.
    set_my_name(&svc, "Casey").unwrap();
    assert_eq!(my_voice(&svc).unwrap().name.as_deref(), Some("Casey"));
    let people: Vec<_> = svc
        .store()
        .persons()
        .unwrap()
        .into_iter()
        .filter(|p| p.is_me && p.is_live())
        .collect();
    assert_eq!(people.len(), 1);
    // Forgetting the voice keeps the name.
    forget_my_voice(&svc).unwrap();
    assert_eq!(my_voice(&svc).unwrap().name.as_deref(), Some("Casey"));
    // Empty goes back to none, and the same refusals as any person's name apply.
    assert!(matches!(
        set_my_name(&svc, "Casey\nParker"),
        Err(RuntimeError::Edit(_))
    ));
    set_my_name(&svc, "   ").unwrap();
    assert_eq!(my_voice(&svc).unwrap().name, None);
}
