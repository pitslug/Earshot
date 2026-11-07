//! Tests of the audio check on the Audio page (T7.4j): the jobs with replay sources of a
//! simulated room (never the speakers), the stored result shown for the devices a meeting
//! would use, and the page's words. Every name contains `audio_check`.

use std::num::NonZeroU16;
use std::sync::mpsc;

use earshot_core::audio::SampleRate;
use earshot_core::audio_check::{CheckResult, Outcome, test_signal};
use earshot_core::capture::{DeviceTime, ReplayBuilder, TrackBuffer};
use earshot_core::dsp::fft_filter;
use earshot_core::sim::{Room, image_source_rir};
use earshot_runtime::audio_check::{CheckedEndpoint, StoredChecks};

use super::*;
use crate::activity::{AUDIO_CHECK_RUNNING, MEETING_RUNNING_FOR_AUDIO_CHECK, VOICE_RUNNING};
use crate::testutil::{T0, TempDir, services};

const MIC: &str = "{0.0.1.00000000}.{mic-a}";
const OUT: &str = "{0.0.0.00000000}.{out-a}";

fn endpoint(id: &str, name: &str) -> CheckedEndpoint {
    CheckedEndpoint {
        id: id.into(),
        name: name.into(),
    }
}

/// The chirps at 48 kHz through a small room, `bulk_ms` late at `gain`, as a replay.
fn replay(bulk_ms: u64, gain: f32) -> earshot_core::capture::ReplaySource {
    let rate = SampleRate::HZ_48000;
    let played = test_signal(rate);
    let rir = image_source_rir(
        &Room {
            dims: [4.0, 3.5, 2.7],
            absorption: 0.35,
        },
        [1.0, 1.2, 0.9],
        [1.3, 1.25, 0.95],
        rate,
        rate.frames_for(Duration::from_millis(200)),
    );
    let shift = rate.frames_for(Duration::from_millis(bulk_ms));
    let total = shift + played.len() + rate.frames_for(Duration::from_millis(1500));
    let mut dry = vec![0.0; total];
    dry[shift..shift + played.len()].copy_from_slice(&played);
    // Room noise around -60 dBFS (a small LCG; broadband, so it survives resampling).
    let mut x = 12_345u32;
    let mic: Vec<f32> = fft_filter(&dry, &rir)
        .iter()
        .map(|e| {
            x = x.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            e * gain + (f64::from(x) / f64::from(u32::MAX) - 0.5) as f32 * 3.5e-3
        })
        .collect();
    let mut stereo: Vec<f32> = played.iter().flat_map(|&v| [v, v]).collect();
    stereo.resize(2 * total, 0.0);
    let one = NonZeroU16::new(1).unwrap();
    let two = NonZeroU16::new(2).unwrap();
    ReplayBuilder::new(
        DeviceTime::from_ticks(10_000_000),
        TrackBuffer::new(rate, one, mic).unwrap(),
    )
    .reference(TrackBuffer::new(rate, two, stereo).unwrap())
    .packet_duration(Duration::from_millis(10))
    .build()
    .unwrap()
}

/// Measures a replayed room through the runtime's capture path.
struct ReplayWork {
    bulk_ms: u64,
    gain: f32,
}

impl AudioCheckWork for ReplayWork {
    fn run(
        &self,
        _: &AudioSettings,
        cancel: &CancelToken,
        progress: &mut dyn FnMut(Duration),
    ) -> Result<Option<Measured>, RuntimeError> {
        let mut source = replay(self.bulk_ms, self.gain);
        let result = check::measure(&mut source, &mut || false, cancel, &mut || {}, progress)?;
        Ok(result
            .map(|r| Measured::new(endpoint(MIC, "Surface mic"), endpoint(OUT, "Speakers"), r)))
    }
}

/// Runs until cancelled (it says when it started), panics, or fails.
struct WaitingWork {
    started: Mutex<Option<mpsc::Sender<()>>>,
    panic: bool,
    fail: Option<String>,
}

impl AudioCheckWork for WaitingWork {
    fn run(
        &self,
        _: &AudioSettings,
        cancel: &CancelToken,
        progress: &mut dyn FnMut(Duration),
    ) -> Result<Option<Measured>, RuntimeError> {
        assert!(!self.panic, "the work panicked");
        if let Some(m) = &self.fail {
            return Err(RuntimeError::Capture(m.clone()));
        }
        progress(Duration::ZERO);
        if let Some(tx) = self.started.lock().unwrap().take() {
            tx.send(()).unwrap();
        }
        while !cancel.is_cancelled() {
            std::thread::yield_now();
        }
        Ok(None)
    }
}

fn waiting(started: Option<mpsc::Sender<()>>) -> WaitingWork {
    WaitingWork {
        started: Mutex::new(started),
        panic: false,
        fail: None,
    }
}

type Events = Arc<Mutex<Vec<AudioCheckEvent>>>;

fn jobs(work: impl AudioCheckWork + 'static, gate: &WorkGate) -> (AudioCheckJobs, Events) {
    let events: Events = Arc::new(Mutex::new(Vec::new()));
    let sink_events = Arc::clone(&events);
    let sink: AudioCheckSink = Arc::new(move |e| sink_events.lock().unwrap().push(e));
    (
        AudioCheckJobs::new(Arc::new(work), sink, gate.clone()),
        events,
    )
}

fn item(id: &str, default: bool) -> DeviceItem {
    DeviceItem {
        id: id.into(),
        name: format!("{id} name"),
        is_default: default,
        hands_free: false,
    }
}

fn devices() -> Devices {
    Devices {
        microphones: vec![item(MIC, true), item("{mic-b}", false)],
        outputs: vec![item(OUT, true), item("{out-b}", false)],
    }
}

#[test]
fn audio_check_runs_stores_and_shows_the_result_for_the_devices_in_use() {
    let dir = TempDir::new("audio-check-run");
    let svc = services(&dir);
    let gate = WorkGate::new();
    let (jobs, events) = jobs(
        ReplayWork {
            bulk_ms: 60,
            gain: 0.5,
        },
        &gate,
    );
    jobs.start(&svc).unwrap();
    jobs.join();
    let events = events.lock().unwrap().clone();
    let (last, progress) = events.split_last().unwrap();
    assert!(!progress.is_empty());
    assert!(
        progress.iter().all(
            |e| matches!(e, AudioCheckEvent::Listening { total_ms, .. } if *total_ms == 4_500)
        ),
        "{progress:?}"
    );
    let AudioCheckEvent::Done { view, note } = last else {
        panic!("{last:?}")
    };
    assert_eq!(*note, None);
    let r = view.result.as_ref().unwrap();
    assert_eq!(r.outcome, AudioCheckOutcome::Echo);
    assert_eq!(r.checked_at_ms, T0);
    assert_eq!(
        (r.microphone.as_str(), r.output.as_str()),
        ("Surface mic", "Speakers")
    );
    // 60 ms of device delay plus about 1 ms through the room.
    assert_eq!(r.delay_ms, Some(61));
    assert!(r.delay_used && !r.too_quiet && !r.clipping, "{r:?}");
    assert_eq!(r.headline, "Your microphone hears your speakers.");
    assert_eq!(r.details[0], "Delay from speaker to microphone: 61 ms.");
    // Stored for the pair, and it seeds the next meeting on it.
    let hint = check::delay_hint_for(&svc.store(), MIC, OUT, max_delay()).unwrap();
    assert!(
        hint.is_some_and(|h| (h / 16.0 - 61.0).abs() < 1.0),
        "{hint:?}"
    );
    // The page shows it for the defaults (which are these devices) and for the chosen ones.
    let defaults = AudioSettings::default();
    let v = check_view(&svc, &defaults, Ok(&devices())).unwrap();
    assert_eq!(v.result.as_ref(), Some(r));
    let chosen = AudioSettings {
        output: DeviceSetting::Chosen {
            id: "{out-b}".into(),
            name: "Headphones".into(),
        },
        ..AudioSettings::default()
    };
    assert_eq!(
        check_view(&svc, &chosen, Ok(&devices())).unwrap().result,
        None
    );
    // A chosen output that is not connected: the meeting would use the default, so its
    // result is shown.
    let gone = AudioSettings {
        output: DeviceSetting::Chosen {
            id: "{out-gone}".into(),
            name: "Dock".into(),
        },
        ..AudioSettings::default()
    };
    assert!(
        check_view(&svc, &gone, Ok(&devices()))
            .unwrap()
            .result
            .is_some()
    );
    // Devices that could not be listed: nothing to show.
    let err = "no list".to_owned();
    assert_eq!(check_view(&svc, &defaults, Err(&err)).unwrap().result, None);
}

#[test]
fn audio_check_headphones_are_shown_in_plain_words() {
    let dir = TempDir::new("audio-check-headphones");
    let svc = services(&dir);
    let gate = WorkGate::new();
    let (jobs, events) = jobs(
        ReplayWork {
            bulk_ms: 60,
            gain: 0.0,
        },
        &gate,
    );
    jobs.start(&svc).unwrap();
    jobs.join();
    let last = events.lock().unwrap().last().cloned().unwrap();
    let AudioCheckEvent::Done { view, .. } = last else {
        panic!("{last:?}")
    };
    let r = view.result.unwrap();
    assert_eq!(r.outcome, AudioCheckOutcome::Headphones);
    assert_eq!(
        r.headline,
        "Your microphone did not hear the sounds: you are probably using headphones, or the speaker volume is off."
    );
    assert_eq!(
        (r.delay_ms, r.echo_quieter_db, r.delay_used),
        (None, None, false)
    );
    assert!(r.details.is_empty(), "{:?}", r.details);
    assert_eq!(
        check::delay_hint_for(&svc.store(), MIC, OUT, max_delay()).unwrap(),
        None
    );
}

fn stored(result: &CheckResult) -> StoredCheck {
    StoredCheck::new(endpoint(MIC, "Mic"), endpoint(OUT, "Speakers"), T0, result)
}

fn echo(delay_samples: f64, too_quiet: bool, clipping: bool) -> CheckResult {
    CheckResult {
        outcome: Outcome::Echo {
            delay_samples,
            echo_db: -18.4,
            echo_dbfs: -40.0,
        },
        too_quiet,
        clipping,
        noise_dbfs: -70.0,
        stretches: Vec::new(),
    }
}

#[test]
fn audio_check_words_for_every_finding() {
    let max = max_delay();
    let v = result_view(&stored(&echo(832.0, false, false)), max);
    assert_eq!(
        v.details,
        [
            "Delay from speaker to microphone: 52 ms.",
            "Echo level: 18 dB quieter than the sound played.",
            "Meetings with this microphone and output use the delay to start cancelling echo sooner.",
        ]
    );
    assert_eq!((v.delay_ms, v.echo_quieter_db), (Some(52), Some(18)));
    let v = result_view(&stored(&echo(832.0, true, true)), max);
    assert!(v.too_quiet && v.clipping && !v.delay_used);
    assert!(v.details[2].starts_with("Too quiet: "), "{:?}", v.details);
    assert!(v.details[3].starts_with("Clipping: "), "{:?}", v.details);
    assert_eq!(v.details.len(), 4);
    let v = result_view(&stored(&echo(12_000.0, false, false)), max);
    assert!(!v.delay_used);
    assert_eq!(
        v.details[2],
        "The delay is longer than the echo canceller searches (600 ms), so it is not used."
    );
    for (outcome, want, start) in [
        (
            Outcome::NotPlayed,
            AudioCheckOutcome::NotPlayed,
            "No sound reached the output.",
        ),
        (
            Outcome::MicSilent,
            AudioCheckOutcome::MicSilent,
            "Your microphone sent only silence.",
        ),
    ] {
        let r = CheckResult {
            outcome,
            ..echo(0.0, true, false)
        };
        let v = result_view(&stored(&r), max);
        assert_eq!(v.outcome, want);
        assert!(v.headline.starts_with(start), "{}", v.headline);
        assert!(!v.delay_used);
    }
}

#[test]
fn audio_check_never_overlaps_a_meeting_the_voice_sample_or_another_check() {
    let dir = TempDir::new("audio-check-busy");
    let svc = services(&dir);
    let gate = WorkGate::new();
    let (tx, rx) = mpsc::channel();
    let (jobs, events) = jobs(waiting(Some(tx)), &gate);
    let meeting = gate.enter_meeting().unwrap();
    let e = jobs.start(&svc).unwrap_err();
    assert_eq!(
        (e.kind.as_str(), e.message.as_str(), e.retryable),
        ("busy", MEETING_RUNNING_FOR_AUDIO_CHECK, true)
    );
    drop(meeting);
    let voice = gate.enter_voice().unwrap();
    assert_eq!(jobs.start(&svc).unwrap_err().message, VOICE_RUNNING);
    drop(voice);
    jobs.start(&svc).unwrap();
    rx.recv().unwrap();
    assert_eq!(jobs.start(&svc).unwrap_err().kind, "busy");
    assert_eq!(
        gate.enter_meeting().unwrap_err().message,
        AUDIO_CHECK_RUNNING
    );
    assert_eq!(gate.enter_voice().unwrap_err().message, AUDIO_CHECK_RUNNING);
    jobs.cancel();
    jobs.join();
    assert_eq!(
        events.lock().unwrap().last(),
        Some(&AudioCheckEvent::Cancelled)
    );
    assert_eq!(
        check::stored_checks(&svc.store()).unwrap(),
        StoredChecks::default(),
        "nothing stored"
    );
    drop(gate.enter_meeting().unwrap());
}

#[test]
fn audio_check_panic_is_one_failed_event_and_releases_the_pass() {
    let dir = TempDir::new("audio-check-panic");
    let svc = services(&dir);
    let gate = WorkGate::new();
    let (jobs, events) = jobs(
        WaitingWork {
            panic: true,
            ..waiting(None)
        },
        &gate,
    );
    jobs.start(&svc).unwrap();
    jobs.join();
    assert_eq!(
        events.lock().unwrap().clone(),
        [AudioCheckEvent::Failed {
            message: AUDIO_CHECK_PANIC_MESSAGE.into()
        }]
    );
    drop(gate.enter_audio_check().unwrap());
}

#[test]
fn audio_check_failures_reach_the_page_as_plain_words() {
    let dir = TempDir::new("audio-check-fail");
    let svc = services(&dir);
    let gate = WorkGate::new();
    let (jobs, events) = jobs(
        WaitingWork {
            fail: Some("the sounds could not be played: device in use".into()),
            ..waiting(None)
        },
        &gate,
    );
    jobs.start(&svc).unwrap();
    jobs.join();
    assert_eq!(
        events.lock().unwrap().clone(),
        [AudioCheckEvent::Failed {
            message:
                "The audio check did not finish: the sounds could not be played: device in use."
                    .into()
        }]
    );
    assert_eq!(failure_copy(&RuntimeError::Poisoned), OTHER_FAILED);
}
