//! Tests of the audio check (T7.4j): the capture path on replays of simulated rooms (device
//! rates, a late loopback, breaks, cancel), the stored results and the meeting-start hint,
//! and (ignored) a recorded GATE-10 call and the real device run.

use std::num::NonZeroU16;
use std::time::Duration;

use earshot_core::asr::CancelToken;
use earshot_core::audio::{PIPELINE_RATE, SampleRate};
use earshot_core::audio_check::{CheckResult, Outcome, test_signal};
use earshot_core::capture::{
    AudioSource, DeviceTime, DiscontinuityKind, ReplayBuilder, ReplaySource, TrackBuffer, TrackId,
};
use earshot_core::dsp::fft_filter;
use earshot_core::sim::{Room, direct_delay_samples, image_source_rir};

use super::*;
use crate::testutil::TempDir;

#[path = "../../earshot-asr-whisper/examples/asr_eval/gate12_private.rs"]
mod gate12_private;

const ROOM: Room = Room {
    dims: [4.0, 3.5, 2.7],
    absorption: 0.35,
};
const SPEAKER: [f64; 3] = [1.0, 1.2, 0.9];
const MIC: [f64; 3] = [1.3, 1.25, 0.95];
const ORIGIN: DeviceTime = DeviceTime::from_ticks(50_000_000_000);

fn rate(hz: u32) -> SampleRate {
    SampleRate::new(hz).unwrap()
}

fn ch(n: u16) -> NonZeroU16 {
    NonZeroU16::new(n).unwrap()
}

/// Deterministic low-level noise (a small LCG; no RNG crate needed).
fn noise(n: usize, rms: f32, seed: u32) -> Vec<f32> {
    let mut x = seed.wrapping_mul(2_654_435_761).wrapping_add(1);
    (0..n)
        .map(|_| {
            x = x.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            (f64::from(x) / f64::from(u32::MAX) * 2.0 - 1.0) as f32 * rms * 1.732
        })
        .collect()
}

/// The chirps as a call would have them: the loopback holds the test signal (stereo, at
/// `out_hz`, starting `late_ms` after the origin with nothing before it, as a device
/// loopback that delivers no packets while nothing plays); the mic (at `mic_hz`) hears it
/// through the room `bulk_ms` later at `gain`, plus noise. Returns the replay and the true
/// delay (16 kHz samples).
fn call(out_hz: u32, mic_hz: u32, bulk_ms: u32, late_ms: u32, gain: f32) -> (ReplayBuilder, f64) {
    let played = test_signal(rate(out_hz));
    let late = rate(out_hz).frames_for(Duration::from_millis(u64::from(late_ms)));
    let mut loopback = vec![0.0; 2 * (late + played.len())];
    for (i, v) in played.iter().enumerate() {
        loopback[2 * (late + i)] = *v;
        loopback[2 * (late + i) + 1] = *v;
    }
    // The mic's copy of the played signal at its own rate, delayed by `late` + bulk.
    let heard = test_signal(rate(mic_hz));
    let mic_rate = rate(mic_hz);
    let rir = image_source_rir(
        &ROOM,
        SPEAKER,
        MIC,
        mic_rate,
        mic_rate.frames_for(Duration::from_millis(250)),
    );
    let shift = mic_rate.frames_for(Duration::from_millis(u64::from(late_ms + bulk_ms)));
    let total = shift + heard.len() + mic_rate.frames_for(Duration::from_millis(1500));
    let mut dry = vec![0.0; total];
    dry[shift..shift + heard.len()].copy_from_slice(&heard);
    let echo = fft_filter(&dry, &rir);
    let mic: Vec<f32> = echo
        .iter()
        .zip(noise(total, 1e-3, mic_hz))
        .map(|(e, n)| e * gain + n)
        .collect();
    let builder = ReplayBuilder::new(ORIGIN, TrackBuffer::new(mic_rate, ch(1), mic).unwrap())
        .reference(TrackBuffer::new(rate(out_hz), ch(2), loopback).unwrap())
        .packet_duration(Duration::from_millis(10));
    let truth = f64::from(bulk_ms) * 16.0 + direct_delay_samples(SPEAKER, MIC, PIPELINE_RATE);
    (builder, truth)
}

fn run(source: &mut ReplaySource) -> Result<Option<CheckResult>, RuntimeError> {
    measure(
        source,
        &mut || false,
        &CancelToken::new(),
        &mut || {},
        &mut |_| {},
    )
}

fn delay_of(r: &CheckResult) -> f64 {
    match r.outcome {
        Outcome::Echo { delay_samples, .. } => delay_samples,
        ref other => panic!("expected an echo, got {other:?}"),
    }
}

#[test]
fn audio_check_measures_the_delay_across_device_rates() {
    for (out_hz, mic_hz, bulk_ms) in [
        (48_000, 48_000, 40),
        (48_000, 44_100, 120),
        (44_100, 16_000, 300),
    ] {
        let (builder, truth) = call(out_hz, mic_hz, bulk_ms, 0, 0.5);
        let r = run(&mut builder.build().unwrap()).unwrap().unwrap();
        let delay = delay_of(&r);
        assert!(
            (delay - truth).abs() <= 16.0,
            "{out_hz}/{mic_hz} Hz, {bulk_ms} ms: {delay} vs {truth}"
        );
        assert!(!r.too_quiet && !r.clipping, "{r:?}");
    }
}

#[test]
fn audio_check_places_a_late_loopback_by_its_timestamps() {
    // The loopback's first packet arrives 400 ms after the origin (nothing played before):
    // its frames sit at their own time, so the delay is unchanged.
    let (builder, truth) = call(48_000, 48_000, 60, 400, 0.5);
    let builder = builder.gap(TrackId::Reference, 0, 19_200).unwrap();
    let r = run(&mut builder.build().unwrap()).unwrap().unwrap();
    assert!((delay_of(&r) - truth).abs() <= 16.0, "{r:?}");
}

#[test]
fn audio_check_hears_no_echo_with_headphones() {
    let (builder, _) = call(48_000, 48_000, 60, 0, 0.0);
    let r = run(&mut builder.build().unwrap()).unwrap().unwrap();
    assert_eq!(r.outcome, Outcome::NoEcho);
    assert_eq!(r.delay_hint(9_600), None);
}

#[test]
fn audio_check_flags_a_clipping_mic_on_its_own_samples() {
    let (builder, _) = call(48_000, 48_000, 60, 0, 40.0);
    let r = run(&mut builder.build().unwrap()).unwrap().unwrap();
    assert!(r.clipping, "{r:?}");
}

#[test]
fn audio_check_a_break_in_a_track_fails_the_run() {
    for track in [TrackId::Mic, TrackId::Reference] {
        let (builder, _) = call(48_000, 48_000, 60, 0, 0.5);
        let builder = builder
            .discontinuity(track, 24_000, DiscontinuityKind::DataLost)
            .unwrap();
        let e = run(&mut builder.build().unwrap()).unwrap_err();
        assert!(
            matches!(&e, RuntimeError::Capture(m) if m.contains("run it again")),
            "{e:?}"
        );
    }
}

#[test]
fn audio_check_needs_the_output_track() {
    let mic = TrackBuffer::new(rate(48_000), ch(1), vec![0.0; 48_000]).unwrap();
    let mut source = ReplayBuilder::new(ORIGIN, mic).build().unwrap();
    let e = run(&mut source).unwrap_err();
    assert!(matches!(e, RuntimeError::Capture(_)), "{e:?}");
}

#[test]
fn audio_check_cancel_stops_and_drops_the_audio() {
    let (builder, _) = call(48_000, 48_000, 60, 0, 0.5);
    let mut source = builder.build().unwrap();
    let cancel = CancelToken::new();
    let mut pumps = 0;
    let r = measure(&mut source, &mut || false, &cancel, &mut || {}, &mut |_| {
        pumps += 1;
        if pumps == 5 {
            cancel.cancel();
        }
    })
    .unwrap();
    assert!(r.is_none());
}

#[test]
fn audio_check_stops_after_the_playback_and_the_echo_tail() {
    // Playback reported done at once: capture keeps AFTER_PLAYBACK of mic audio, then stops.
    let (builder, _) = call(48_000, 48_000, 60, 0, 0.5);
    let mut source = builder.build().unwrap();
    let mut heard = Duration::ZERO;
    let r = measure(
        &mut source,
        &mut || true,
        &CancelToken::new(),
        &mut || {},
        &mut |d| heard = d,
    )
    .unwrap()
    .unwrap();
    assert!(heard >= AFTER_PLAYBACK, "{heard:?}");
    assert!(
        heard < AFTER_PLAYBACK + Duration::from_millis(100),
        "{heard:?}"
    );
    // What was heard by then: the first two chirps only, which still agree.
    assert!(
        matches!(r.outcome, Outcome::Echo { .. } | Outcome::NoEcho),
        "{r:?}"
    );
}

/// The pipeline's canceller locks for a replay (median of the locked delays, ms).
fn canceller_lock(mut replay: ReplaySource) -> Option<f64> {
    use earshot_core::pipeline::{Pipeline, PipelineConfig, PipelineEvent};
    let mut pipeline = Pipeline::new(replay.session().clone(), PipelineConfig::default()).unwrap();
    let mut locked = Vec::new();
    loop {
        let status = replay.pump(&mut pipeline).unwrap();
        for event in pipeline.poll() {
            if let PipelineEvent::Stats(s) = event
                && let Some(a) = s.aec
                && a.locked
                && let Some(ms) = a.delay_ms
            {
                locked.push(ms);
            }
        }
        if status == earshot_core::capture::SourceStatus::Ended && pipeline.is_ended() {
            break;
        }
    }
    locked.sort_by(f64::total_cmp);
    locked.get(locked.len() / 2).copied()
}

#[test]
fn audio_check_measures_the_delay_the_canceller_locks_on() {
    // The hint is in the canceller's convention: a broadband far end (noise bursts) played
    // through the room, replayed through the check and through the pipeline, gives the same
    // delay within 1 ms.
    let r48 = rate(48_000);
    let n = r48.frames_for(Duration::from_secs(12));
    let burst = r48.frames_for(Duration::from_millis(700));
    let mut far = noise(n, 0.1, 5);
    for (i, v) in far.iter_mut().enumerate() {
        if (i / burst) % 2 == 1 {
            *v = 0.0;
        }
    }
    let rir = image_source_rir(
        &ROOM,
        SPEAKER,
        MIC,
        r48,
        r48.frames_for(Duration::from_millis(250)),
    );
    let bulk = r48.frames_for(Duration::from_millis(150));
    let mut dry = vec![0.0; n];
    dry[bulk..].copy_from_slice(&far[..n - bulk]);
    let mic: Vec<f32> = fft_filter(&dry, &rir)
        .iter()
        .zip(noise(n, 1e-3, 6))
        .map(|(e, v)| e * 0.5 + v)
        .collect();
    let stereo: Vec<f32> = far.iter().flat_map(|&v| [v, v]).collect();
    let build = || {
        ReplayBuilder::new(ORIGIN, TrackBuffer::new(r48, ch(1), mic.clone()).unwrap())
            .reference(TrackBuffer::new(r48, ch(2), stereo.clone()).unwrap())
            .packet_duration(Duration::from_millis(10))
            .build()
            .unwrap()
    };
    let truth = 150.0 + direct_delay_samples(SPEAKER, MIC, PIPELINE_RATE) / 16.0;
    let checked = delay_of(&run(&mut build()).unwrap().unwrap()) / 16.0;
    let canceller = canceller_lock(build()).expect("the canceller never locked");
    eprintln!("truth {truth:.2} ms, check {checked:.2} ms, canceller {canceller:.2} ms");
    assert!(
        (checked - truth).abs() <= 1.0,
        "check {checked} vs truth {truth}"
    );
    assert!(
        (checked - canceller).abs() <= 1.0,
        "check {checked} vs canceller {canceller}"
    );
}

fn store(dir: &TempDir) -> Store {
    Store::open(&dir.0.join("earshot.db")).unwrap()
}

fn endpoint(id: &str) -> CheckedEndpoint {
    CheckedEndpoint {
        id: id.into(),
        name: format!("{id} name"),
    }
}

fn echo_result(delay_samples: f64, too_quiet: bool) -> CheckResult {
    CheckResult {
        outcome: Outcome::Echo {
            delay_samples,
            echo_db: -18.0,
            echo_dbfs: -35.0,
        },
        too_quiet,
        clipping: false,
        noise_dbfs: -70.0,
        stretches: Vec::new(),
    }
}

#[test]
fn audio_check_results_are_stored_per_pair() {
    let dir = TempDir::new("audio-check-store");
    let mut db = store(&dir);
    assert_eq!(stored_checks(&db).unwrap(), StoredChecks::default());
    let first = StoredCheck::new(
        endpoint("mic-a"),
        endpoint("out-a"),
        1_000,
        &echo_result(1_600.0, false),
    );
    record_check(&mut db, first.clone()).unwrap();
    let other = StoredCheck::new(
        endpoint("mic-a"),
        endpoint("out-b"),
        2_000,
        &echo_result(800.0, false),
    );
    record_check(&mut db, other.clone()).unwrap();
    let again = StoredCheck::new(
        endpoint("mic-a"),
        endpoint("out-a"),
        3_000,
        &echo_result(1_700.0, false),
    );
    record_check(&mut db, again.clone()).unwrap();
    let stored = stored_checks(&db).unwrap();
    assert!(!stored.unreadable);
    assert_eq!(
        stored.checks,
        vec![other.clone(), again.clone()],
        "the pair was replaced"
    );
    assert_eq!(stored.pair("mic-a", "out-a"), Some(&again));
    assert_eq!(stored.pair("mic-a", "out-c"), None);
    assert_eq!(again.delay_ms(), Some(1_700.0 / 16.0));
    // Survives a reopen (device-local).
    drop(db);
    let db = store(&dir);
    assert_eq!(stored_checks(&db).unwrap().checks.len(), 2);
}

#[test]
fn audio_check_keeps_the_check_just_written_whatever_the_clock_did() {
    // The wall clock stepped back: the newest write still survives eviction, and the
    // oldest write goes.
    let dir = TempDir::new("audio-check-clock");
    let mut db = store(&dir);
    for i in 0..MAX_PAIRS as u64 {
        let c = StoredCheck::new(
            endpoint(&format!("mic-{i}")),
            endpoint("out"),
            1_000_000 + i,
            &echo_result(100.0, false),
        );
        record_check(&mut db, c).unwrap();
    }
    let back = StoredCheck::new(
        endpoint("mic-late"),
        endpoint("out"),
        5,
        &echo_result(200.0, false),
    );
    record_check(&mut db, back.clone()).unwrap();
    let stored = stored_checks(&db).unwrap();
    assert_eq!(stored.checks.len(), MAX_PAIRS);
    assert_eq!(stored.checks.last(), Some(&back));
    assert_eq!(
        stored.checks[0].microphone.id, "mic-1",
        "the first written went"
    );
    // Re-checking a pair moves it to the end (the most recently written).
    let again = StoredCheck::new(
        endpoint("mic-1"),
        endpoint("out"),
        1,
        &echo_result(300.0, false),
    );
    record_check(&mut db, again.clone()).unwrap();
    let stored = stored_checks(&db).unwrap();
    assert_eq!(stored.checks.last(), Some(&again));
    assert_eq!(stored.pair("mic-1", "out"), Some(&again));
    assert_eq!(stored.checks.len(), MAX_PAIRS);
}

#[test]
fn audio_check_playback_stops_when_measuring_panics() {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, mpsc};
    // A playback that runs until told to stop, then reports it was stopped.
    let stopped = Arc::new(AtomicBool::new(false));
    let seen = Arc::clone(&stopped);
    let (started, ready) = mpsc::channel();
    let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let player = Playback::spawn(move |flag| {
            started.send(()).unwrap();
            while !flag.load(Ordering::Acquire) {
                std::thread::yield_now();
            }
            seen.store(true, Ordering::Release);
            Ok(())
        })
        .unwrap();
        ready.recv().unwrap();
        assert!(!player.is_finished());
        panic!("measuring failed");
    }));
    assert!(r.is_err());
    // The unwind dropped the guard, which set the flag and joined the thread.
    assert!(stopped.load(Ordering::Acquire));
    // The normal path: finish stops, joins and returns the playback's result.
    let player = Playback::spawn(|flag| {
        while !flag.load(Ordering::Acquire) {
            std::thread::yield_now();
        }
        Err("stopped".into())
    })
    .unwrap();
    assert_eq!(player.finish(), Err("stopped".into()));
    let done = Playback::spawn(|_| Ok(())).unwrap();
    while !done.is_finished() {
        std::thread::yield_now();
    }
    assert_eq!(done.finish(), Ok(()));
}

#[test]
fn audio_check_keeps_the_newest_pairs() {
    let dir = TempDir::new("audio-check-cap");
    let mut db = store(&dir);
    for i in 0..(MAX_PAIRS as u64 + 3) {
        let c = StoredCheck::new(
            endpoint(&format!("mic-{i}")),
            endpoint("out"),
            i,
            &echo_result(100.0, false),
        );
        record_check(&mut db, c).unwrap();
    }
    let stored = stored_checks(&db).unwrap();
    assert_eq!(stored.checks.len(), MAX_PAIRS);
    assert_eq!(stored.checks[0].microphone.id, "mic-3");
}

#[test]
fn audio_check_an_unreadable_record_is_reported_and_replaced() {
    let dir = TempDir::new("audio-check-bad");
    let mut db = store(&dir);
    for bad in [
        b"x".to_vec(),
        [2u32.to_le_bytes().to_vec(), b"[]".to_vec()].concat(),
        [1u32.to_le_bytes().to_vec(), b"{nope".to_vec()].concat(),
    ] {
        crate::local::set(&mut db, &DeviceRecord::AudioCheck.key(), &bad).unwrap();
        let stored = stored_checks(&db).unwrap();
        assert!(stored.unreadable && stored.checks.is_empty());
        assert_eq!(delay_hint_for(&db, "mic", "out", 9_600).unwrap(), None);
    }
    let c = StoredCheck::new(
        endpoint("mic"),
        endpoint("out"),
        5,
        &echo_result(320.0, false),
    );
    record_check(&mut db, c).unwrap();
    assert!(!stored_checks(&db).unwrap().unreadable);
}

#[test]
fn audio_check_hint_for_the_pair_at_meeting_start() {
    let dir = TempDir::new("audio-check-hint");
    let mut db = store(&dir);
    assert_eq!(delay_hint_for(&db, "mic", "out", 9_600).unwrap(), None);
    let cases = [
        ("out-echo", echo_result(1_600.0, false), Some(1_600.0)),
        ("out-quiet", echo_result(1_600.0, true), None),
        ("out-far", echo_result(12_000.0, false), None),
    ];
    for (out, result, _) in &cases {
        record_check(
            &mut db,
            StoredCheck::new(endpoint("mic"), endpoint(out), 1, result),
        )
        .unwrap();
    }
    let headphones = CheckResult {
        outcome: Outcome::NoEcho,
        ..echo_result(0.0, false)
    };
    record_check(
        &mut db,
        StoredCheck::new(endpoint("mic"), endpoint("out-headphones"), 1, &headphones),
    )
    .unwrap();
    for (out, _, want) in &cases {
        assert_eq!(
            delay_hint_for(&db, "mic", out, 9_600).unwrap(),
            *want,
            "{out}"
        );
    }
    assert_eq!(
        delay_hint_for(&db, "mic", "out-headphones", 9_600).unwrap(),
        None
    );
    // Another mic with the same output has no result.
    assert_eq!(
        delay_hint_for(&db, "other-mic", "out-echo", 9_600).unwrap(),
        None
    );
    // The stored rule is the core rule.
    let stored = StoredCheck::new(
        endpoint("m"),
        endpoint("o"),
        1,
        &echo_result(1_600.0, false),
    );
    assert_eq!(
        stored.delay_hint(9_600),
        echo_result(1_600.0, false).delay_hint(9_600)
    );
}

#[test]
fn audio_check_stored_outcomes_round_trip() {
    for outcome in [Outcome::NoEcho, Outcome::NotPlayed, Outcome::MicSilent] {
        let r = CheckResult {
            outcome: outcome.clone(),
            ..echo_result(0.0, true)
        };
        let s = StoredCheck::new(endpoint("m"), endpoint("o"), 9, &r);
        let back: StoredCheck = serde_json::from_str(&serde_json::to_string(&s).unwrap()).unwrap();
        assert_eq!(back, s);
        assert_eq!(s.delay_ms(), None);
        assert_eq!(s.delay_hint(9_600), None);
    }
}

/// The private GATE-10 desktop call recording (speech on the far end, no chirps): the check's
/// capture path over its real device timestamps measures the delay the canceller locks on
/// when the same recording is replayed through the pipeline. Agreement within 2 ms (a
/// tolerance written before measuring: the estimator and the matched filter peak on the
/// same direct path; the canceller's sub-sample refinement and drift move it a little).
///
/// Measured on a real recording, FAILING by a few ms, over a first minute in which the
/// canceller's lock moves and over the first seconds, the meeting start the hint is for. The
/// check's placement equals the WAV frames
/// as recorded (asserted below), and on simulated input check and canceller agree to
/// 0.02 ms (`audio_check_measures_the_delay_the_canceller_locks_on`), so the difference is a
/// property of that recording; left failing for the owner (see the T7.4j report).
#[test]
#[ignore = "needs the private GATE-10 recording: gate10.dir in .earshot-private/gate12.toml or EARSHOT_GATE10_DIR; cargo test -p earshot-runtime --lib audio_check_recorded -- --ignored --nocapture"]
fn audio_check_recorded_gate10_call_agrees_with_the_canceller() {
    use earshot_core::audio::read_wav;
    use earshot_core::capture::RawTimeline;
    use earshot_core::pipeline::{Pipeline, PipelineConfig, PipelineEvent};

    let dir = gate12_private::recording_dir(
        "EARSHOT_GATE10_DIR",
        &std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../.."),
        |s| &s.gate10.dir,
    );
    let read = |name: &str| {
        let f = std::fs::File::open(dir.join(name)).unwrap_or_else(|e| panic!("{name}: {e}"));
        TrackBuffer::try_from(read_wav(std::io::BufReader::new(f)).unwrap()).unwrap()
    };
    let timeline: RawTimeline =
        serde_json::from_slice(&std::fs::read(dir.join("timestamps.json")).unwrap()).unwrap();
    let source = || {
        ReplayBuilder::from_raw_timeline(&timeline, read("mic.wav"), read("loopback.wav"))
            .unwrap()
            .build()
            .unwrap()
    };
    let window = Duration::from_secs(10);
    let r = measure_up_to(
        &mut source(),
        window,
        &mut || false,
        &CancelToken::new(),
        &mut || {},
        &mut |_| {},
    )
    .unwrap()
    .unwrap();
    eprintln!("audio check: {r:?}");
    {
        // The same analysis on the WAV frames as written (frame 0 at the origin, no stamps):
        // placing packets by their device timestamps gives the recording's own alignment.
        let mono = |t: &TrackBuffer| {
            let mut out = Vec::new();
            earshot_core::capture::mix_to_mono(t.samples(), t.channels(), &mut out);
            let n = t.rate().frames_for(window).min(out.len());
            earshot_core::audio::resample_all(&out[..n], t.rate(), PIPELINE_RATE).unwrap()
        };
        let wav = earshot_core::audio_check::analyse(
            &mono(&read("loopback.wav")),
            &mono(&read("mic.wav")),
            false,
        );
        assert_eq!(
            wav.outcome, r.outcome,
            "placement by stamps moved the tracks"
        );
    }

    let mut replay = source();
    let mut pipeline = Pipeline::new(replay.session().clone(), PipelineConfig::default()).unwrap();
    let mut locked = Vec::new();
    loop {
        let status = replay.pump(&mut pipeline).unwrap();
        for event in pipeline.poll() {
            if let PipelineEvent::Stats(s) = event
                && s.end.as_millis() <= u64::try_from(window.as_millis()).unwrap()
                && let Some(a) = s.aec
                && a.locked
                && let Some(ms) = a.delay_ms
            {
                locked.push(ms);
            }
        }
        if status == earshot_core::capture::SourceStatus::Ended && pipeline.is_ended() {
            break;
        }
    }
    assert!(
        !locked.is_empty(),
        "the canceller never locked in the window"
    );
    let measured = delay_of(&r);
    locked.sort_by(f64::total_cmp);
    let canceller_ms = locked[locked.len() / 2];
    let measured_ms = measured / 16.0;
    eprintln!("audio check {measured_ms:.2} ms, canceller median lock {canceller_ms:.2} ms");
    assert!(
        (measured_ms - canceller_ms).abs() <= 2.0,
        "{measured_ms} vs {canceller_ms}"
    );
}

/// The real check on this computer's Audio page devices. It plays four short chirps through
/// the output at the system volume.
#[cfg(windows)]
#[test]
#[ignore = "plays sound: four short chirps through the output at the system volume; cargo test -p earshot-runtime --lib audio_check_device_run -- --ignored --nocapture"]
fn audio_check_device_run_plays_chirps_and_reports() {
    let devices = earshot_capture_wasapi::DeviceSelection::default();
    let measured = run_check(&devices, &CancelToken::new(), &mut |_| {})
        .unwrap()
        .unwrap();
    eprintln!(
        "mic {:?}, output {:?}: {:?}, delay {:?} ms, fallbacks {:?}",
        measured.microphone.name,
        measured.output.name,
        measured.result.outcome,
        measured.result.delay_ms(),
        measured.fallbacks
    );
    assert_ne!(
        measured.result.outcome,
        Outcome::NotPlayed,
        "the chirps were not played"
    );
}
