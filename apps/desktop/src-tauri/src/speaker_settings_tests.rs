//! Tests of the Speakers page (T7.4h): the speaker detection setting and the voice sample
//! with replay sources and fake speaker models. Every name contains `speakers`.

use std::sync::mpsc;

use earshot_core::capture::{DeviceTime, ReplayBuilder};
use earshot_core::diar::{
    DiarizationConfig, EmbeddingModelId, FakeEmbedder, FakeSegmenter, MAX_SCENE_SPEAKERS,
    SceneTurn, scene_audio,
};
use earshot_runtime::post::{ModelThresholds, PostConfig};

use super::*;
use crate::activity::{MEETING_RUNNING, VOICE_RUNNING};
use crate::testutil::{TempDir, services};

const MODEL: &str = "fake-emb@1";

fn config() -> PostConfig {
    PostConfig {
        diarization: DiarizationConfig::default(),
        clustering_thresholds: ModelThresholds::new()
            .with(EmbeddingModelId::new(MODEL), 0.704_565_5),
        voiceprint_thresholds: ModelThresholds::new().with(EmbeddingModelId::new(MODEL), 0.5),
        ..PostConfig::default()
    }
}

fn embedder() -> FakeEmbedder {
    let centroids = (0..MAX_SCENE_SPEAKERS)
        .map(|k| (0..16).map(|i| if i == k { 1.0 } else { 0.0 }).collect())
        .collect();
    FakeEmbedder::new(MODEL, centroids, 0.05, 7)
}

/// Records `audio` from a replay and enrols it with fake models.
struct ReplayWork {
    audio: MonoBuffer,
}

impl VoiceWork for ReplayWork {
    fn record(
        &self,
        cancel: &CancelToken,
        progress: &mut dyn FnMut(Duration),
    ) -> Result<Option<MonoBuffer>, RuntimeError> {
        let mut source = ReplayBuilder::new(DeviceTime::from_ticks(1), self.audio.clone())
            .build()
            .unwrap();
        my_voice::record_sample(&mut source, SAMPLE_LENGTH, cancel, &mut || {}, progress)
    }

    fn enrol(
        &self,
        services: &Services,
        audio: &MonoBuffer,
        cancel: &CancelToken,
    ) -> Result<MyVoiceOutcome, RuntimeError> {
        my_voice::enrol_my_voice(
            services,
            FakeSegmenter::segmentation_3_0(),
            embedder(),
            audio,
            &config(),
            cancel,
        )
    }
}

/// Records until cancelled (it says when it started), or panics.
struct WaitingWork {
    started: Mutex<Option<mpsc::Sender<()>>>,
    panic: bool,
}

impl VoiceWork for WaitingWork {
    fn record(
        &self,
        cancel: &CancelToken,
        progress: &mut dyn FnMut(Duration),
    ) -> Result<Option<MonoBuffer>, RuntimeError> {
        assert!(!self.panic, "the work panicked");
        progress(Duration::ZERO);
        if let Some(tx) = self.started.lock().unwrap().take() {
            tx.send(()).unwrap();
        }
        while !cancel.is_cancelled() {
            std::thread::yield_now();
        }
        Ok(None)
    }

    fn enrol(
        &self,
        _: &Services,
        _: &MonoBuffer,
        _: &CancelToken,
    ) -> Result<MyVoiceOutcome, RuntimeError> {
        unreachable!("never recorded")
    }
}

type Events = Arc<Mutex<Vec<VoiceEvent>>>;

fn jobs(work: impl VoiceWork + 'static, gate: &WorkGate) -> (VoiceJobs, Events) {
    let events: Events = Arc::new(Mutex::new(Vec::new()));
    let sink_events = Arc::clone(&events);
    let sink: VoiceSink = Arc::new(move |e| sink_events.lock().unwrap().push(e));
    (VoiceJobs::new(Arc::new(work), sink, gate.clone()), events)
}

fn one_voice() -> MonoBuffer {
    scene_audio(&[SceneTurn::new(4, 500, 27_000)], 35_000)
}

#[test]
fn speakers_timing_offers_the_battery_choice_on_laptops_only() {
    let dir = TempDir::new("speakers-options");
    let svc = services(&dir);
    let laptop = speakers_view(&svc, true).unwrap();
    assert_eq!(laptop.timing, SpeakerTimingChoice::DuringCall);
    let values: Vec<_> = laptop.options.iter().map(|o| o.value).collect();
    assert_eq!(
        values,
        [
            SpeakerTimingChoice::DuringCall,
            SpeakerTimingChoice::AfterCall,
            SpeakerTimingChoice::AfterCallOnBattery
        ]
    );
    assert_eq!(laptop.options[1].description, AFTER_CALL_WARNING);
    assert_eq!(laptop.options[2].label, "After the call on battery");
    for o in &laptop.options {
        assert!(!o.description.contains('\u{2014}') && !o.label.contains('\u{2014}'));
    }
    assert!(laptop.unknown.is_empty());
    assert!(!laptop.voice.saved);
    assert_eq!(laptop.voice.sample_ms, 30_000);
    let desktop = speakers_view(&svc, false).unwrap();
    assert_eq!(desktop.options.len(), 2);
    assert!(!desktop.laptop);
    // Refused on a desktop, nothing stored.
    let e = set_timing(&svc, SpeakerTimingChoice::AfterCallOnBattery, false).unwrap_err();
    assert_eq!(e.kind, "invalid_setting");
    assert_eq!(
        speakers_view(&svc, true).unwrap().timing,
        SpeakerTimingChoice::DuringCall
    );
    // Stored on a laptop, and still shown on a desktop (a disk moved, say) so it can be
    // changed.
    set_timing(&svc, SpeakerTimingChoice::AfterCallOnBattery, true).unwrap();
    let desktop = speakers_view(&svc, false).unwrap();
    assert_eq!(desktop.timing, SpeakerTimingChoice::AfterCallOnBattery);
    assert_eq!(desktop.options.len(), 3);
}

#[test]
fn speakers_timing_is_what_the_next_meeting_start_reads() {
    let dir = TempDir::new("speakers-timing");
    let svc = services(&dir);
    for (choice, timing) in [
        (SpeakerTimingChoice::AfterCall, SpeakerTiming::AfterCall),
        (
            SpeakerTimingChoice::AfterCallOnBattery,
            SpeakerTiming::AfterCallOnBattery,
        ),
        (SpeakerTimingChoice::DuringCall, SpeakerTiming::DuringCall),
    ] {
        let view = set_timing(&svc, choice, true).unwrap();
        assert_eq!(view.timing, choice);
        // The meeting driver's pre-flight read (`meeting::HostPrepare`).
        let (read, _) =
            earshot_runtime::host::choose_speaker_timing(&mut svc.store(), None, false).unwrap();
        assert_eq!(read, timing);
    }
}

#[test]
fn speakers_timing_reports_a_stored_value_it_does_not_know() {
    let dir = TempDir::new("speakers-unknown");
    let svc = services(&dir);
    svc.store()
        .set_local_value(
            earshot_runtime::speaker_timing::SPEAKER_TIMING_KEY,
            b"after-on-solar",
        )
        .unwrap();
    let view = speakers_view(&svc, true).unwrap();
    assert_eq!(view.timing, SpeakerTimingChoice::DuringCall);
    assert_eq!(view.unknown.len(), 1);
    assert!(
        view.unknown[0].contains("after-on-solar"),
        "{:?}",
        view.unknown
    );
}

#[test]
fn speakers_voice_sample_records_saves_and_is_forgotten() {
    let dir = TempDir::new("speakers-voice");
    let svc = services(&dir);
    let gate = WorkGate::new();
    let (jobs, events) = jobs(ReplayWork { audio: one_voice() }, &gate);
    jobs.start(&svc).unwrap();
    jobs.join();
    let events = events.lock().unwrap().clone();
    let (last, rest) = events.split_last().unwrap();
    let VoiceEvent::Done {
        result,
        message,
        voice,
    } = last
    else {
        panic!("{events:?}")
    };
    assert_eq!(*result, VoiceResult::Saved);
    assert!(message.starts_with("Your voice is saved ("), "{message}");
    assert!(voice.saved && voice.speech_ms >= 20_000, "{voice:?}");
    assert_eq!(rest.last(), Some(&VoiceEvent::Saving));
    let recorded: Vec<u64> = rest
        .iter()
        .filter_map(|e| match e {
            VoiceEvent::Recording {
                recorded_ms,
                total_ms,
            } => {
                assert_eq!(*total_ms, 30_000);
                Some(*recorded_ms)
            }
            _ => None,
        })
        .collect();
    assert!(recorded.windows(2).all(|w| w[0] < w[1]), "{recorded:?}");
    // Throttled to one event per half second of audio, ending at the full sample.
    assert!(recorded.len() <= 62, "{}", recorded.len());
    assert_eq!(recorded.last(), Some(&30_000));
    assert!(speakers_view(&svc, true).unwrap().voice.saved);
    // The pass is released: a meeting may start now.
    drop(gate.enter_meeting().unwrap());
    assert_eq!(forget_voice(&svc).unwrap(), 1);
    assert!(!speakers_view(&svc, true).unwrap().voice.saved);
}

#[test]
fn speakers_voice_sample_refusals_say_why_and_save_nothing() {
    let dir = TempDir::new("speakers-voice-refused");
    let svc = services(&dir);
    let gate = WorkGate::new();
    let two = scene_audio(
        &[
            SceneTurn::new(0, 0, 14_000),
            SceneTurn::new(1, 15_000, 29_000),
        ],
        30_000,
    );
    let (jobs, events) = jobs(ReplayWork { audio: two }, &gate);
    jobs.start(&svc).unwrap();
    jobs.join();
    let last = events.lock().unwrap().last().cloned().unwrap();
    let VoiceEvent::Done {
        result,
        message,
        voice,
    } = last
    else {
        panic!("{last:?}")
    };
    assert_eq!(result, VoiceResult::SeveralVoices);
    assert!(message.contains("nothing was saved"), "{message}");
    assert!(!voice.saved);
    for outcome in [
        MyVoiceOutcome::NoSpeech,
        MyVoiceOutcome::NotEnoughSpeech {
            needed: Duration::from_secs(10),
            got: Duration::from_secs(4),
        },
        MyVoiceOutcome::Uncalibrated {
            model: EmbeddingModelId::new(MODEL),
        },
    ] {
        let VoiceEvent::Done { message, .. } = outcome_event(&outcome, voice.clone()) else {
            panic!("{outcome:?}")
        };
        assert!(message.contains("nothing was saved"), "{message}");
        assert!(!message.contains('\u{2014}'));
    }
    assert_eq!(
        outcome_event(&MyVoiceOutcome::Cancelled, voice),
        VoiceEvent::Cancelled
    );
}

#[test]
fn speakers_voice_sample_never_overlaps_a_meeting_or_another_sample() {
    let dir = TempDir::new("speakers-voice-busy");
    let svc = services(&dir);
    let gate = WorkGate::new();
    let (tx, rx) = mpsc::channel();
    let (jobs, events) = jobs(
        WaitingWork {
            started: Mutex::new(Some(tx)),
            panic: false,
        },
        &gate,
    );
    let meeting = gate.enter_meeting().unwrap();
    let e = jobs.start(&svc).unwrap_err();
    assert_eq!(
        (e.kind.as_str(), e.message.as_str()),
        ("busy", MEETING_RUNNING)
    );
    drop(meeting);
    jobs.start(&svc).unwrap();
    rx.recv().unwrap();
    // While it records: no second sample, no meeting.
    assert_eq!(jobs.start(&svc).unwrap_err().kind, "busy");
    assert_eq!(gate.enter_meeting().unwrap_err().message, VOICE_RUNNING);
    jobs.cancel();
    jobs.join();
    assert_eq!(events.lock().unwrap().last(), Some(&VoiceEvent::Cancelled));
    assert!(!speakers_view(&svc, true).unwrap().voice.saved);
    drop(gate.enter_meeting().unwrap());
}

#[test]
fn speakers_voice_sample_panic_is_one_failed_event_and_releases_the_pass() {
    let dir = TempDir::new("speakers-voice-panic");
    let svc = services(&dir);
    let gate = WorkGate::new();
    let (jobs, events) = jobs(
        WaitingWork {
            started: Mutex::new(None),
            panic: true,
        },
        &gate,
    );
    jobs.start(&svc).unwrap();
    jobs.join();
    let events = events.lock().unwrap().clone();
    assert_eq!(
        events,
        [VoiceEvent::Failed {
            message: VOICE_PANIC_MESSAGE.into()
        }]
    );
    drop(gate.enter_voice().unwrap());
}

/// Records `audio` (or fails to), then fails to load the models.
struct FailingWork {
    record_fails: bool,
}

impl VoiceWork for FailingWork {
    fn record(
        &self,
        _: &CancelToken,
        _: &mut dyn FnMut(Duration),
    ) -> Result<Option<MonoBuffer>, RuntimeError> {
        if self.record_fails {
            return Err(RuntimeError::Capture(
                "IAudioClient::Initialize: 0x88890004".into(),
            ));
        }
        Ok(Some(one_voice()))
    }

    fn enrol(
        &self,
        _: &Services,
        _: &MonoBuffer,
        _: &CancelToken,
    ) -> Result<MyVoiceOutcome, RuntimeError> {
        Err(RuntimeError::ModelMissing {
            path: "C:\\models\\embedding\\campplus.onnx".into(),
            fetch_set: Some("ci"),
        })
    }
}

#[test]
fn speakers_voice_sample_failures_reach_the_page_as_plain_words() {
    let dir = TempDir::new("speakers-voice-fail");
    let svc = services(&dir);
    for (record_fails, want) in [(true, MIC_FAILED), (false, MODELS_FAILED)] {
        let gate = WorkGate::new();
        let (jobs, events) = jobs(FailingWork { record_fails }, &gate);
        jobs.start(&svc).unwrap();
        jobs.join();
        let last = events.lock().unwrap().last().cloned();
        assert_eq!(
            last,
            Some(VoiceEvent::Failed {
                message: want.into()
            })
        );
        assert!(!want.contains("0x") && !want.contains("campplus") && !want.contains('\u{2014}'));
    }
    assert_eq!(failure_copy(&RuntimeError::Poisoned), OTHER_FAILED);
    assert!(!speakers_view(&svc, true).unwrap().voice.saved);
}

#[test]
fn speakers_view_carries_the_name_and_refuses_an_unusable_one_in_plain_words() {
    let dir = TempDir::new("speakers-name");
    let svc = services(&dir);
    assert_eq!(speakers_view(&svc, false).unwrap().voice.name, "");
    store_my_name(&svc, "Casey Parker").unwrap();
    assert_eq!(
        speakers_view(&svc, false).unwrap().voice.name,
        "Casey Parker"
    );
    let refused = store_my_name(&svc, &"x".repeat(5_000)).unwrap_err();
    assert!(!refused.message.contains("xxxx"), "{}", refused.message);
    assert_eq!(
        speakers_view(&svc, false).unwrap().voice.name,
        "Casey Parker"
    );
    store_my_name(&svc, "").unwrap();
    assert_eq!(speakers_view(&svc, false).unwrap().voice.name, "");
}
