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
        unassigned_floors: ModelThresholds::new().with(EmbeddingModelId::new(MODEL), -1.0),
        voiceprint_thresholds: ModelThresholds::new().with(EmbeddingModelId::new(MODEL), 0.5),
        naming_thresholds: ModelThresholds::new().with(EmbeddingModelId::new(MODEL), 0.5),
        voiceprint_margins: ModelThresholds::new(),
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
        assert_eq!(prints[0].id(), voiceprint);
        assert_eq!(prints[0].model(), &EmbeddingModelId::new(MODEL));
        // The voice of scene speaker 3 (the print folded with its samples).
        let store: &earshot_store::Store = &db;
        let (_, folded) = earshot_core::voiceprint::live_prints_of(store, person)
            .unwrap()
            .remove(0);
        let best = folded
            .sum()
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
    assert_eq!(
        forget_my_voice(&svc).unwrap(),
        2,
        "the print and its one sample"
    );
    assert_eq!(my_voice(&svc).unwrap(), MyVoice::default());
    let db = svc.store();
    let v = db
        .voiceprints_of(person)
        .unwrap()
        .into_iter()
        .find(|v| v.id() == voiceprint)
        .unwrap();
    assert!(v.meta().deleted);
    assert!(
        earshot_core::voiceprint::effective(&v, &[])
            .sum()
            .iter()
            .all(|x| *x == 0.0)
    );
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
            earshot_core::voiceprint::SampleOrigin::Own,
            &EnrolmentConfig::default(),
            stamp,
        )
        .map_err(RuntimeError::Voiceprint)?;
        Ok(p.id)
    })
    .unwrap();
    assert_eq!(my_voice(&svc).unwrap().voiceprints, 2);
    assert_eq!(
        forget_my_voice(&svc).unwrap(),
        4,
        "two prints and their samples"
    );
    assert_eq!(my_voice(&svc).unwrap(), MyVoice::default());
    let db = svc.store();
    for p in [first, second] {
        for v in db.voiceprints_of(p).unwrap() {
            let zero = earshot_core::voiceprint::effective(&v, &[])
                .sum()
                .iter()
                .all(|x| *x == 0.0);
            assert!(v.meta().deleted && zero);
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

#[test]
fn my_voice_renaming_yourself_marks_a_summary_that_used_your_name() {
    use rand_core::SeedableRng;
    let dir = TempDir::new("my-name-summary");
    let (svc, _) = services(&dir);
    set_my_name(&svc, "Casey").unwrap();
    let me = svc
        .store()
        .persons()
        .unwrap()
        .into_iter()
        .find(|p| p.is_me && p.is_live())
        .unwrap()
        .id;
    let named = |m| {
        crate::view::MeetingView::load_named(&svc.store(), m)
            .unwrap()
            .unwrap()
    };
    let mut meetings = Vec::new();
    for (seed, tldr) in [
        (1, "Casey will send the plan."),
        (2, "The plan was agreed."),
    ] {
        let m = crate::testutil::meeting(&svc, "Planning", seed);
        let row = named(m)
            .speakers
            .into_iter()
            .find(|s| s.role == earshot_core::diar::SpeakerRole::Me)
            .unwrap();
        crate::edit::link_speaker(&svc, m, row.id, &crate::edit::PersonChoice::Existing(me))
            .unwrap();
        let v = named(m);
        let s = earshot_core::summary::SummaryV1 {
            title: "Planning".into(),
            tldr: tldr.into(),
            enhanced_notes: vec![],
            decisions: vec![],
            action_items: vec![],
            open_questions: vec![],
            topics: vec![],
        };
        crate::summary::store_summary(
            &svc,
            &v,
            "test-model".into(),
            &s,
            &mut rand_chacha::ChaCha8Rng::seed_from_u64(seed + 10),
        )
        .unwrap();
        meetings.push(m);
    }
    assert!(!named(meetings[0]).summary_out_of_date());

    set_my_name(&svc, "Casey Lee").unwrap();
    assert!(
        named(meetings[0]).summary_names_changed(),
        "renamed yourself"
    );
    assert!(
        !named(meetings[1]).summary_out_of_date(),
        "a summary that never used the name"
    );

    set_my_name(&svc, "casey").unwrap();
    assert!(!named(meetings[0]).summary_out_of_date(), "put back");
    assert!(!named(meetings[1]).summary_out_of_date());
}

/// T12.77b2: a print created with a zero base holds its voice only in stored samples; every
/// reader (your voice, has-voiceprint, the People list) folds them, so it reads as a voice.
#[test]
fn my_voice_a_zero_base_print_with_stored_samples_reads_as_a_voice() {
    use earshot_core::voiceprint::{SampleOrigin, Voiceprint, VoiceprintSample};
    let dir = TempDir::new("my-voice-samples");
    let (svc, _) = services(&dir);
    let print = crate::edit::write(&svc, |tx, stamp| {
        let p = earshot_core::voiceprint::Person::new("Me", true, stamp);
        tx.put_person(p.clone()).map_err(RuntimeError::Voiceprint)?;
        let id = earshot_core::identity::EntityId::generate(stamp.clock, &mut *stamp.rng);
        let meta = earshot_core::identity::SyncMeta::new(stamp.hlc, stamp.clock);
        let v = Voiceprint::from_stored(
            id,
            meta,
            p.id,
            EmbeddingModelId::new(MODEL),
            vec![0.0; DIM],
            0,
            0,
        );
        tx.put_voiceprint(v.clone())
            .map_err(RuntimeError::Voiceprint)?;
        Ok(v)
    })
    .unwrap();
    let has_voice = |svc: &Services| {
        let db = svc.store();
        let store: &earshot_store::Store = &db;
        crate::edit::user_has_voiceprint(store)
    };
    // Empty: no voice anywhere.
    assert_eq!(my_voice(&svc).unwrap().voiceprints, 0);
    assert!(!has_voice(&svc).unwrap());
    let listed = |svc: &Services| {
        crate::edit::list_persons(svc)
            .unwrap()
            .into_iter()
            .find(|p| p.id == print.person_id())
            .unwrap()
            .has_voiceprint
    };
    assert!(!listed(&svc));

    crate::edit::write(&svc, |tx, stamp| {
        let mut v = vec![0.0; DIM];
        v[3] = 1.0;
        let s = VoiceprintSample::new(&print, &v, 11_000, SampleOrigin::Own, stamp)
            .map_err(RuntimeError::Voiceprint)?;
        tx.put_sample(s).map_err(RuntimeError::Voiceprint)
    })
    .unwrap();
    let mine = my_voice(&svc).unwrap();
    assert_eq!(
        (mine.voiceprints, mine.speech),
        (1, Duration::from_millis(11_000))
    );
    assert!(has_voice(&svc).unwrap());
    assert!(listed(&svc));
    // T12.77b3: forgetting counts the sample with the print, and leaves no voice.
    assert_eq!(forget_my_voice(&svc).unwrap(), 2);
    assert_eq!(my_voice(&svc).unwrap().voiceprints, 0);
    assert!(svc.store().samples_of(print.id()).unwrap().is_empty());
}

/// People, prints and samples for the T14.57 baseline: merges into the user, a tombstoned
/// "Me" with someone merged into it, prints whose voice is only in samples, and samples the
/// fold leaves out (another model, another length).
fn damaged_baseline_store(svc: &Services) {
    use earshot_core::identity::{EntityId, SyncMeta};
    use earshot_core::voiceprint::{Person, SampleRecord, Voiceprint, VoiceprintSample};
    let pid =
        |n: u8| EntityId::from_bytes([0x01, 0x91, 0, 0, 0, 0, 0x70, 0, 0x80, 0, 0, 0, 0, 0, n, 3]);
    crate::edit::write(svc, |tx, stamp| {
        // (n, name, is_me, deleted, merged into)
        let people: [(u8, &str, bool, bool, Option<u8>); 9] = [
            (1, "Me", true, false, None),
            (2, "Invented Rowan", false, false, Some(1)),
            (3, "Invented Sasha", false, false, None),
            (4, "Invented Tamsin", false, false, None),
            (5, "Invented Ulla", false, false, None),
            (6, "Me", true, true, None),
            (7, "Invented Vito", false, false, Some(6)),
            (8, "Invented Wren", false, false, None),
            (9, "Invented Xavi", false, false, Some(3)),
        ];
        for (n, name, is_me, deleted, into) in people {
            let mut meta = SyncMeta::new(stamp.hlc, stamp.clock);
            meta.deleted = deleted;
            tx.put_person(Person {
                id: pid(n),
                meta,
                display_name: if deleted { String::new() } else { name.into() },
                is_me,
                merged_into: into.map(pid),
            })
            .map_err(RuntimeError::Voiceprint)?;
        }
        let unit =
            |k: usize| -> Vec<f32> { (0..DIM).map(|i| if i == k { 1.0 } else { 0.0 }).collect() };
        // (print n, person, base count, base sum axis)
        let prints: [(u8, u8, u32); 7] = [
            (31, 1, 2),
            (32, 2, 0),
            (33, 3, 0),
            (34, 4, 0),
            (35, 5, 0),
            (36, 6, 1),
            (37, 7, 0),
        ];
        let mut stored = Vec::new();
        for (n, person, count) in prints {
            let sum = if count == 0 {
                vec![0.0; DIM]
            } else {
                unit(usize::from(n) % DIM)
            };
            let v = Voiceprint::from_stored(
                pid(n),
                SyncMeta::new(stamp.hlc, stamp.clock),
                pid(person),
                EmbeddingModelId::new(MODEL),
                sum,
                count,
                u64::from(count) * 5_000,
            );
            tx.put_voiceprint(v.clone())
                .map_err(RuntimeError::Voiceprint)?;
            stored.push(v);
        }
        // (sample n, print, person, model, vector, origin own)
        let records: [(u8, u8, u8, &str, Vec<f32>, bool); 8] = [
            (41, 31, 1, MODEL, unit(4), true),
            (42, 32, 2, MODEL, unit(5), false),
            // The fold leaves these out: another model, another length.
            (43, 33, 3, "other-emb@1", unit(6), false),
            (44, 34, 4, MODEL, unit(1)[..DIM / 2].to_vec(), false),
            // The control of the last two: a matching sample makes a voice.
            (45, 35, 5, MODEL, unit(7), false),
            (46, 36, 6, MODEL, unit(8), true),
            (47, 37, 7, MODEL, unit(9), false),
            // A sample of the user's on another person's print.
            (48, 35, 1, MODEL, unit(10), true),
        ];
        for (n, print, person, model, vector, own) in records {
            let r = SampleRecord {
                id: pid(n),
                meta: SyncMeta::new(stamp.hlc, stamp.clock),
                voiceprint_id: Some(pid(print)),
                person_id: Some(pid(person)),
                model: Some(EmbeddingModelId::new(model)),
                vector,
                speech_ms: Some(6_000),
                meeting_id: (!own).then(|| pid(200)),
                speaker_id: (!own).then(|| pid(201)),
            };
            let s = VoiceprintSample::from_stored(r).map_err(RuntimeError::Voiceprint)?;
            tx.put_sample(s).map_err(RuntimeError::Voiceprint)?;
        }
        Ok(())
    })
    .unwrap();
}

/// T14.57 test 12 (docs/sync-spec.md 16.9): on a store with no damaged row, the People list
/// (who is listed, who has a voice, by today's fold) and Forget my voice (its count, and the
/// live count it answers with when the checkpoint is blocked) answer as before T14.57. The
/// expected values were captured from the commit before the change (57f0369).
#[test]
fn my_voice_damaged_same_answers_without_damage_as_before() {
    let dir = TempDir::new("my-voice-damaged-baseline");
    let (svc, _) = services(&dir);
    damaged_baseline_store(&svc);
    let listed: Vec<String> = crate::edit::list_persons(&svc)
        .unwrap()
        .into_iter()
        .map(|p| {
            format!(
                "{} voice={} me={} meetings={}",
                p.name, p.has_voiceprint, p.is_me, p.meetings
            )
        })
        .collect();
    assert_eq!(
        listed,
        [
            "Invented Sasha voice=false me=false meetings=0",
            "Invented Tamsin voice=false me=false meetings=0",
            "Invented Ulla voice=true me=false meetings=0",
            "Invented Wren voice=false me=false meetings=0",
            "Me voice=true me=true meetings=0",
        ],
        "the People list"
    );
    assert_eq!(forget_my_voice(&svc).unwrap(), 9, "Forget my voice");

    // The same store, with a reader holding the file so the checkpoint is refused: the answer
    // is the live count read before the erasure.
    let dir = TempDir::new("my-voice-damaged-baseline-busy");
    let (svc, _) = services(&dir);
    damaged_baseline_store(&svc);
    let db = earshot_store::DataDir::new(dir.0.join("data")).db_path();
    let reader = rusqlite::Connection::open(&db).unwrap();
    reader
        .execute_batch("BEGIN; SELECT count(*) FROM person;")
        .unwrap();
    assert_eq!(
        forget_my_voice(&svc).unwrap(),
        9,
        "the live count when Busy"
    );
    reader.execute_batch("COMMIT;").unwrap();
}
