//! Meeting driver tests: replay starters and fake post work. Every name contains `meeting`.

use std::sync::atomic::Ordering;
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use earshot_core::asr::{BackendKind, CancelToken};
use earshot_core::audio::{MonoBuffer, PIPELINE_RATE};
use earshot_core::capture::{
    AudioSource, CaptureSession, CaptureSink, DeviceTime, ReplayBuilder, ReplaySource,
    SourceStatus, TrackId,
};
use earshot_core::model::{AudioFileRepo, ClockDomain, MeetingRepo};
use earshot_runtime::call_end::{CallSessions, CallSignal};
use earshot_runtime::host::{HostOutcome, HostPost, HostStep, SkipReason, SummaryResult};
use earshot_runtime::live::{LiveConfig, start_live};
use earshot_runtime::meeting_queue::QueueTicket;
use earshot_runtime::post::{CleanupWork, PostStage};
use earshot_runtime::{RuntimeError, Services};
use rand_chacha::ChaCha8Rng;
use rand_core::SeedableRng;

use super::*;
use crate::meeting_events::{EndedReason, LiveTrack, PostStep, WarningKind};
use crate::testutil::{TempDir, services};

/// A tone the level meters and voice activity can see.
fn tone(seconds: f32, amplitude: f32) -> Vec<f32> {
    let n = (seconds * 16_000.0) as usize;
    (0..n)
        .map(|i| amplitude * (i as f32 * 0.07).sin())
        .collect()
}

fn replay(seconds: f32, reference: bool) -> ReplaySource {
    let mut b = ReplayBuilder::new(
        DeviceTime::from_ticks(5_000_000),
        MonoBuffer::new(PIPELINE_RATE, tone(seconds, 0.2)),
    );
    if reference {
        b = b.reference(MonoBuffer::new(PIPELINE_RATE, tone(seconds, 0.3)));
    }
    b.build().unwrap()
}

/// A replay that delivers nothing until it is stopped: a meeting that runs until the user
/// stops it.
struct UntilStopped {
    inner: ReplaySource,
    stopped: bool,
}

impl AudioSource for UntilStopped {
    type Error = <ReplaySource as AudioSource>::Error;
    fn session(&self) -> &CaptureSession {
        self.inner.session()
    }
    fn pump(&mut self, sink: &mut dyn CaptureSink) -> Result<SourceStatus, Self::Error> {
        if self.stopped {
            self.inner.pump(sink)
        } else {
            Ok(SourceStatus::Idle)
        }
    }
    fn stop(&mut self) {
        self.stopped = true;
        self.inner.stop();
    }
}

fn config() -> LiveConfig {
    LiveConfig {
        clock_domain: ClockDomain::Replay,
        idle_wait: Duration::from_millis(1),
        ..LiveConfig::default()
    }
}

/// How the fake pre-flight behaves.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Script {
    /// A replay that ends by itself.
    Ends,
    /// A replay that records until stopped.
    UntilStopped,
    /// The pre-flight fails.
    PreflightFails,
    /// The session cannot start.
    StartFails,
    /// The pre-flight panics.
    PreflightPanics,
    /// The work after the meeting panics.
    AfterPanics,
    /// Records until stopped, and its stop never returns until the test releases it.
    StopHangs,
    /// Records until stopped through a capture source whose thread never ends (T9.20).
    CaptureThreadHangs,
}

/// Holds a stuck stop until the test lets it go (`true`).
type StopGate = Arc<(Mutex<bool>, std::sync::Condvar)>;

fn release(gate: &StopGate) {
    *gate.0.lock().unwrap() = true;
    gate.1.notify_all();
}

/// [`UntilStopped`] whose stop blocks until its gate opens: a source stuck in a call.
struct HangingStop {
    inner: UntilStopped,
    gate: StopGate,
}

impl AudioSource for HangingStop {
    type Error = <ReplaySource as AudioSource>::Error;
    fn session(&self) -> &CaptureSession {
        self.inner.session()
    }
    fn pump(&mut self, sink: &mut dyn CaptureSink) -> Result<SourceStatus, Self::Error> {
        self.inner.pump(sink)
    }
    fn stop(&mut self) {
        let (open, cv) = &*self.gate;
        let mut released = open.lock().unwrap();
        while !*released {
            released = cv.wait(released).unwrap();
        }
        drop(released);
        self.inner.stop();
    }
}

/// A capture source whose thread delivered 1 s on both tracks and never ends: its stop gives
/// the thread up after a short wait (T9.20). The gate releases the stuck thread at the end.
#[cfg(windows)]
fn stuck_capture(gate: StopGate) -> earshot_capture_wasapi::ChannelSource<String> {
    use earshot_capture_wasapi::LiveMsg;
    use earshot_core::capture::{CaptureEvent, TrackInfo};
    let origin = DeviceTime::from_ticks(5_000_000);
    let info = CaptureSession::new(
        origin,
        TrackInfo::new(PIPELINE_RATE),
        Some(TrackInfo::new(PIPELINE_RATE)),
    );
    let (tx, rx) = mpsc::channel::<LiveMsg<String>>();
    for i in 0..100u64 {
        for track in [TrackId::Mic, TrackId::Reference] {
            tx.send(LiveMsg::Event(CaptureEvent::Frames {
                track,
                device_ts: origin.after_frames(i * 160, PIPELINE_RATE),
                frames: tone(0.01, 0.2),
            }))
            .unwrap();
        }
    }
    earshot_capture_wasapi::ChannelSource::new(info, rx, move || {
        // The sender lives as long as the stuck thread: the channel never closes.
        let _keep = tx;
        let (open, cv) = &*gate;
        let mut released = open.lock().unwrap();
        while !*released {
            released = cv.wait(released).unwrap();
        }
    })
    .with_stop_wait(Duration::from_millis(50))
}

/// The setup note the fake pre-flight reports (as `host::transcription_setup` would).
const PLACEMENT_NOTE: &str =
    "Whisper large-v3-turbo has no NPU version, so it runs on the processor.";

struct FakePrepare {
    script: Mutex<Script>,
    /// The meeting `after` was given.
    after_saw: Arc<Mutex<Option<String>>>,
    seed: std::sync::atomic::AtomicU64,
    /// Handed to the next meeting.
    call_signal: Mutex<Option<CallSignal>>,
    /// The summary step reports Claude Code at its default effort (an unreadable help).
    effort_unread: std::sync::atomic::AtomicBool,
    /// Holds a [`Script::StopHangs`] or [`Script::CaptureThreadHangs`] stop.
    stop_gate: StopGate,
}

impl FakePrepare {
    fn new(script: Script) -> Arc<Self> {
        Arc::new(Self {
            script: Mutex::new(script),
            after_saw: Arc::default(),
            seed: 100.into(),
            call_signal: Mutex::new(None),
            effort_unread: false.into(),
            stop_gate: Arc::default(),
        })
    }
}

fn two_tracks() -> Vec<CleanupWork> {
    [TrackId::Mic, TrackId::Reference]
        .map(|track| CleanupWork {
            track,
            to_transcribe: Duration::from_secs(1),
            to_diarise: Duration::from_secs(1),
        })
        .to_vec()
}

impl Prepare for FakePrepare {
    fn prepare(
        &self,
        _svc: &Services,
        request: &StartRequest,
        _ticket: &Arc<QueueTicket>,
    ) -> Result<Prepared, IpcError> {
        let script = *self.script.lock().unwrap();
        assert!(script != Script::PreflightPanics, "a pre-flight bug");
        if script == Script::PreflightFails {
            return Err(IpcError {
                kind: "model".into(),
                message: crate::plain_text::MODELS_NEEDED.into(),
                retryable: false,
            });
        }
        let reference = request.mode == CaptureMode::Call;
        let seed = self.seed.fetch_add(1, Ordering::Relaxed);
        let mut cfg = config();
        cfg.title = request.title.clone();
        let gate = Arc::clone(&self.stop_gate);
        let start: StartFn = Box::new(move |svc: &Services| match script {
            Script::StartFails => Err(RuntimeError::CaptureStart {
                problem: earshot_runtime::CaptureProblem::MicrophoneAccessOff,
                detail: "IAudioClient::Initialize: Access is denied. (0x80070005)".into(),
            }),
            Script::StopHangs => start_live(
                HangingStop {
                    inner: UntilStopped {
                        inner: replay(0.5, reference),
                        stopped: false,
                    },
                    gate: Arc::clone(&gate),
                },
                svc.clone(),
                cfg,
                None,
                Box::new(ChaCha8Rng::seed_from_u64(seed)),
            ),
            #[cfg(windows)]
            Script::CaptureThreadHangs => start_live(
                stuck_capture(Arc::clone(&gate)),
                svc.clone(),
                LiveConfig {
                    clock_domain: ClockDomain::Qpc,
                    ..cfg
                },
                None,
                Box::new(ChaCha8Rng::seed_from_u64(seed)),
            ),
            Script::UntilStopped => start_live(
                UntilStopped {
                    inner: replay(0.5, reference),
                    stopped: false,
                },
                svc.clone(),
                cfg,
                None,
                Box::new(ChaCha8Rng::seed_from_u64(seed)),
            ),
            _ => start_live(
                replay(3.0, reference),
                svc.clone(),
                cfg,
                None,
                Box::new(ChaCha8Rng::seed_from_u64(seed)),
            ),
        });
        let saw = Arc::clone(&self.after_saw);
        let effort_unread = self.effort_unread.load(Ordering::Relaxed);
        let after: AfterFn = Box::new(move |_svc, outcome, _cancel, progress| {
            *saw.lock().unwrap() = Some(outcome.meeting.to_string());
            assert!(script != Script::AfterPanics, "a post-meeting bug");
            let work = two_tracks();
            let el = Duration::from_millis(1);
            progress(HostStep::PostLoaded { missing: &[] });
            progress(HostStep::PostStarting {
                fallbacks: &[],
                work: &work,
                asr_backend: BackendKind::Cpu,
            });
            for track in [TrackId::Mic, TrackId::Reference] {
                progress(HostStep::Post(PostStage::Transcribed {
                    track,
                    elapsed: el,
                }));
                progress(HostStep::Post(PostStage::Diarised {
                    track,
                    speakers: 1,
                    elapsed: el,
                }));
            }
            progress(HostStep::Post(PostStage::EchoChecked { elapsed: el }));
            progress(HostStep::Post(PostStage::Stored { elapsed: el }));
            let post = Ok(HostPost::Skipped);
            progress(HostStep::PostFinished(&post));
            HostOutcome {
                missing_tracks: vec![],
                post,
                summary: SummaryResult::Skipped {
                    reason: SkipReason::NotRequested,
                },
                network: vec![],
                handovers: vec![],
                handover_record_failures: vec![],
                claude_effort_unread: effort_unread,
            }
        });
        Ok(Prepared {
            start,
            after,
            notices: std::iter::once(MeetingEvent::Warning {
                key: "onnx".into(),
                kind: WarningKind::Backend,
                message: "A pre-flight notice.".into(),
            })
            .chain(super::setup_notices(vec![PLACEMENT_NOTE.into()]))
            .collect(),
            call_signal: self.call_signal.lock().unwrap().take(),
        })
    }
}

fn driver(prepare: Arc<FakePrepare>) -> (MeetingDriver, Receiver<MeetingEvent>) {
    let (tx, rx) = mpsc::channel();
    let tx = Mutex::new(tx);
    let sink: Sink = Arc::new(move |e| {
        let _ = tx.lock().unwrap().send(e);
    });
    let t0 = Instant::now();
    let now: Now = Arc::new(move || t0);
    (
        MeetingDriver::new(prepare, sink, now, crate::activity::WorkGate::new()),
        rx,
    )
}

fn request(mode: CaptureMode) -> StartRequest {
    StartRequest {
        mode,
        title: "Design sync".into(),
        keep_local: false,
    }
}

/// Events until the summary result (the last event), bounded so a failure cannot hang.
fn until_summary(rx: &Receiver<MeetingEvent>) -> Vec<MeetingEvent> {
    let mut out = Vec::new();
    loop {
        let e = rx
            .recv_timeout(Duration::from_secs(60))
            .expect("no summary event within 60 s");
        let last = matches!(e, MeetingEvent::Summary { .. });
        out.push(e);
        if last {
            return out;
        }
    }
}

fn position(events: &[MeetingEvent], f: impl Fn(&MeetingEvent) -> bool) -> usize {
    events
        .iter()
        .position(f)
        .unwrap_or_else(|| panic!("missing in {events:?}"))
}

/// A meeting's call signal and clock under a test's control.
struct CallScript {
    readings: Arc<Mutex<std::collections::VecDeque<CallSessions>>>,
    seconds: Arc<std::sync::atomic::AtomicU64>,
}

impl CallScript {
    fn read(&self, readings: &[CallSessions]) {
        self.readings.lock().unwrap().extend(readings);
        // The meeting thread looks about every EVENT_WAIT and takes one reading per look.
        let deadline = Instant::now() + MEET_GUARD;
        while !self.readings.lock().unwrap().is_empty() {
            assert!(Instant::now() < deadline, "readings not taken");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn at(&self, seconds: u64) {
        self.seconds.store(seconds, Ordering::SeqCst);
    }
}

/// Bounds a wait that only a failure runs into.
const MEET_GUARD: Duration = Duration::from_secs(60);

/// A driver whose next meeting records until stopped, with its call signal and clock scripted.
fn call_end_driver() -> (MeetingDriver, Receiver<MeetingEvent>, CallScript) {
    let prepare = FakePrepare::new(Script::UntilStopped);
    let readings: Arc<Mutex<std::collections::VecDeque<CallSessions>>> = Arc::default();
    let queue = Arc::clone(&readings);
    *prepare.call_signal.lock().unwrap() =
        Some(Box::new(move || queue.lock().unwrap().pop_front()));
    let seconds = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let clock = Arc::clone(&seconds);
    let t0 = Instant::now();
    let now: Now = Arc::new(move || t0 + Duration::from_secs(clock.load(Ordering::SeqCst)));
    let (tx, rx) = mpsc::channel();
    let tx = Mutex::new(tx);
    let sink: Sink = Arc::new(move |e| {
        let _ = tx.lock().unwrap().send(e);
    });
    (
        MeetingDriver::new(prepare, sink, now, crate::activity::WorkGate::new()),
        rx,
        CallScript { readings, seconds },
    )
}

/// The next `CallEndOffer` event, skipping everything else.
fn next_offer(rx: &Receiver<MeetingEvent>) -> bool {
    let deadline = Instant::now() + MEET_GUARD;
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        match rx.recv_timeout(left).expect("no offer event") {
            MeetingEvent::CallEndOffer { open, .. } => return open,
            _ => continue,
        }
    }
}

/// Passes several looks of the meeting thread; only then is "nothing happened" evidence.
fn several_looks() {
    std::thread::sleep(EVENT_WAIT * 4);
}

fn offers_in(events: &[MeetingEvent]) -> usize {
    events
        .iter()
        .filter(|e| matches!(e, MeetingEvent::CallEndOffer { .. }))
        .count()
}

#[test]
fn meeting_call_end_opens_and_closes_the_question_with_the_call() {
    let readings: Arc<Mutex<std::collections::VecDeque<CallSessions>>> = Arc::default();
    let queue = Arc::clone(&readings);
    let t0 = Instant::now();
    let at = |seconds: u64| t0 + Duration::from_secs(seconds);
    let keep = std::sync::atomic::AtomicBool::new(false);
    let mut offer = CallEndOffer::new(
        Box::new(move || queue.lock().unwrap().pop_front()),
        CallEndConfig::default(),
        t0,
    );
    let far_end = |speaking| RuntimeEvent::VoiceActivity {
        track: TrackId::Reference,
        speaking,
        at: earshot_core::asr::MeetingTime::ZERO,
    };
    let event = |open| Some(MeetingEvent::CallEndOffer { open });
    readings.lock().unwrap().extend([
        CallSessions::Active,
        CallSessions::NoneActive,
        CallSessions::NoneActive,
    ]);
    for s in 0..3 {
        assert_eq!(offer.tick(&keep, at(s)), None);
    }
    assert_eq!(offer.tick(&keep, at(32)), event(true));

    // Withdrawn and back: asked again.
    offer.event(&far_end(true), at(40));
    assert_eq!(offer.tick(&keep, at(40)), event(false));
    offer.event(&far_end(false), at(41));
    assert_eq!(offer.tick(&keep, at(71)), event(true));

    // The session returned: a new cycle asks again.
    readings.lock().unwrap().push_back(CallSessions::Active);
    assert_eq!(offer.tick(&keep, at(72)), event(false));
    readings
        .lock()
        .unwrap()
        .extend([CallSessions::NoneActive, CallSessions::NoneActive]);
    assert_eq!(offer.tick(&keep, at(73)), None);
    assert_eq!(offer.tick(&keep, at(74)), None);
    assert_eq!(offer.tick(&keep, at(104)), event(true));

    // "Keep recording" closes it; only a session active and ended again asks.
    keep.store(true, Ordering::SeqCst);
    assert_eq!(offer.tick(&keep, at(105)), event(false));
    readings.lock().unwrap().extend([
        CallSessions::Active,
        CallSessions::NoneActive,
        CallSessions::NoneActive,
    ]);
    for s in 106..109 {
        assert_eq!(offer.tick(&keep, at(s)), None);
    }
    assert_eq!(offer.tick(&keep, at(138)), event(true));
}

#[test]
fn meeting_call_end_stops_the_recording_when_nobody_answers_for_the_wait() {
    let dir = TempDir::new("call-end");
    let svc = services(&dir);
    let (d, rx, script) = call_end_driver();
    d.start(&svc, request(CaptureMode::Call)).unwrap();
    script.read(&[
        CallSessions::Active,
        CallSessions::NoneActive,
        CallSessions::NoneActive,
    ]);
    script.at(31);
    assert!(next_offer(&rx), "the question is asked");

    // One second short of the wait: still recording.
    script.at(31 + 179);
    several_looks();
    assert_eq!(d.phase(), DriverPhase::Recording);

    // Past the wait with no recent reading: a poller gone quiet stops nothing.
    script.at(31 + 180);
    several_looks();
    assert_eq!(d.phase(), DriverPhase::Recording);

    script.read(&[CallSessions::NoneActive]);
    let events = until_summary(&rx);
    d.join();
    let at = |wanted: fn(&MeetingEvent) -> bool| events.iter().position(wanted);
    let stopped = at(|e| matches!(e, MeetingEvent::CallEndStopped)).expect("the stop is told");
    let ended = at(|e| {
        matches!(
            e,
            MeetingEvent::Ended {
                reason: EndedReason::Stopped,
                ..
            }
        )
    })
    .expect("the recording ended as a stop");
    let banner =
        at(|e| matches!(e, MeetingEvent::Warning { key, .. } if key == "call-end-stopped"))
            .expect("the user is told why");
    assert!(
        stopped < ended && ended < banner,
        "{stopped} {ended} {banner}"
    );
}

#[test]
fn meeting_call_end_a_stop_the_user_asked_for_is_not_reported_as_automatic() {
    let dir = TempDir::new("call-end-answered");
    let svc = services(&dir);
    let (d, rx, script) = call_end_driver();
    d.start(&svc, request(CaptureMode::Call)).unwrap();
    script.read(&[
        CallSessions::Active,
        CallSessions::NoneActive,
        CallSessions::NoneActive,
    ]);
    script.at(31);
    assert!(next_offer(&rx));

    d.stop().unwrap();
    let events = until_summary(&rx);
    d.join();
    assert!(events.iter().any(|e| matches!(
        e,
        MeetingEvent::Ended {
            reason: EndedReason::Stopped,
            ..
        }
    )));
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, MeetingEvent::CallEndStopped)
                || matches!(e, MeetingEvent::Warning { key, .. } if key == "call-end-stopped"))
    );
}

#[test]
fn meeting_call_end_keep_recording_takes_the_question_away_and_it_stays_away() {
    let dir = TempDir::new("call-end-keep");
    let svc = services(&dir);
    let (d, rx, script) = call_end_driver();
    d.start(&svc, request(CaptureMode::Call)).unwrap();
    script.read(&[
        CallSessions::Active,
        CallSessions::NoneActive,
        CallSessions::NoneActive,
    ]);
    script.at(31);
    assert!(next_offer(&rx));

    d.keep_recording();
    assert!(!next_offer(&rx), "taken away");
    script.read(&[CallSessions::NoneActive, CallSessions::NoneActive]);
    script.at(10_000);
    several_looks();
    assert_eq!(d.phase(), DriverPhase::Recording);

    // A new session that ends asks again.
    script.read(&[
        CallSessions::Active,
        CallSessions::NoneActive,
        CallSessions::NoneActive,
    ]);
    script.at(10_031);
    assert!(next_offer(&rx));

    d.stop().unwrap();
    until_summary(&rx);
    d.join();
}

#[test]
fn meeting_call_end_silence_with_the_session_still_active_never_asks() {
    let dir = TempDir::new("call-end-active");
    let svc = services(&dir);
    let (d, rx, script) = call_end_driver();
    d.start(&svc, request(CaptureMode::Call)).unwrap();
    script.read(&[CallSessions::Active, CallSessions::Active]);
    script.at(86_400);
    script.read(&[CallSessions::Active]);
    several_looks();
    assert_eq!(d.phase(), DriverPhase::Recording);
    d.stop().unwrap();
    let events = until_summary(&rx);
    d.join();
    assert_eq!(offers_in(&events), 0);
}

#[test]
fn meeting_call_end_in_the_room_never_asks_and_never_polls() {
    let dir = TempDir::new("call-end-room");
    let svc = services(&dir);
    let (d, rx, script) = call_end_driver();
    d.start(&svc, request(CaptureMode::InRoom)).unwrap();
    script.readings.lock().unwrap().extend([
        CallSessions::Active,
        CallSessions::NoneActive,
        CallSessions::NoneActive,
    ]);
    script.at(86_400);
    several_looks();
    // The signal was dropped unread.
    assert_eq!(script.readings.lock().unwrap().len(), 3);
    assert_eq!(d.phase(), DriverPhase::Recording);
    d.stop().unwrap();
    let events = until_summary(&rx);
    d.join();
    assert_eq!(offers_in(&events), 0);
}

#[test]
fn meeting_call_end_an_offer_open_at_the_stop_says_nothing_more() {
    let dir = TempDir::new("call-end-stop");
    let svc = services(&dir);
    let (d, rx, script) = call_end_driver();
    d.start(&svc, request(CaptureMode::Call)).unwrap();
    script.read(&[
        CallSessions::Active,
        CallSessions::NoneActive,
        CallSessions::NoneActive,
    ]);
    script.at(31);
    assert!(next_offer(&rx));
    d.stop().unwrap();
    let events = until_summary(&rx);
    d.join();
    assert_eq!(offers_in(&events), 0);
}

#[test]
fn meeting_driver_runs_start_events_end_post_progress_then_the_summary() {
    let dir = TempDir::new("driver");
    let svc = services(&dir);
    let prepare = FakePrepare::new(Script::Ends);
    let (d, rx) = driver(Arc::clone(&prepare));
    let meeting = d.start(&svc, request(CaptureMode::Call)).unwrap();
    let events = until_summary(&rx);
    d.join();

    assert_eq!(
        events[0],
        MeetingEvent::Started {
            meeting: meeting.clone(),
            mode: CaptureMode::Call,
            tracks: vec![LiveTrack::You, LiveTrack::Them],
        }
    );
    // The pre-flight's notices come right after Started.
    assert!(matches!(&events[1], MeetingEvent::Warning { key, .. } if key == "onnx"));
    let levels = position(&events, |e| matches!(e, MeetingEvent::Levels { .. }));
    assert!(matches!(
        events[levels],
        MeetingEvent::Levels {
            them_dbfs: Some(_),
            ..
        }
    ));
    // The clock never moves, so the throttle lets exactly one level through.
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(e, MeetingEvent::Levels { .. }))
            .count(),
        1
    );
    let ended = position(&events, |e| {
        *e == MeetingEvent::Ended {
            meeting: meeting.clone(),
            reason: EndedReason::Finished,
        }
    });
    let progress: Vec<(PostStep, u32, Option<u32>)> = events[ended + 1..]
        .iter()
        .filter_map(|e| match e {
            MeetingEvent::PostProgress {
                step, done, total, ..
            } => Some((*step, *done, *total)),
            _ => None,
        })
        .collect();
    assert_eq!(progress.first(), Some(&(PostStep::Loading, 0, None)));
    assert_eq!(progress.last(), Some(&(PostStep::Stored, 6, Some(6))));
    assert_eq!(progress.len(), 8);
    assert_eq!(
        events.last(),
        Some(&MeetingEvent::Summary {
            meeting: meeting.clone(),
            result: SummaryEvent::Skipped {
                reason: "Summaries are turned off.".into()
            }
        })
    );
    assert_eq!(
        prepare.after_saw.lock().unwrap().as_deref(),
        Some(&*meeting)
    );
    // The meeting row carries the title; the thread is done, so another meeting may start.
    let stored = svc.store().meetings().unwrap();
    assert_eq!(stored.len(), 1);
    assert_eq!(stored[0].title, "Design sync");
    assert!(!d.busy());
}

#[test]
fn meeting_driver_refuses_a_second_start_and_stops_on_request() {
    let dir = TempDir::new("driver-stop");
    let svc = services(&dir);
    let (d, rx) = driver(FakePrepare::new(Script::UntilStopped));
    d.start(&svc, request(CaptureMode::InRoom)).unwrap();
    assert!(d.busy());
    let second = d.start(&svc, request(CaptureMode::Call)).unwrap_err();
    assert_eq!(second.kind, "busy");
    assert!(second.retryable);
    d.stop().unwrap();
    let events = until_summary(&rx);
    d.join();
    assert_eq!(
        events[0],
        MeetingEvent::Started {
            meeting: events_meeting(&events),
            mode: CaptureMode::InRoom,
            tracks: vec![LiveTrack::You],
        }
    );
    assert!(events.contains(&MeetingEvent::Ended {
        meeting: events_meeting(&events),
        reason: EndedReason::Stopped
    }));
    // Room mode has no "Them" level.
    assert!(events.iter().all(|e| !matches!(
        e,
        MeetingEvent::Levels {
            them_dbfs: Some(_),
            ..
        }
    )));
    assert_eq!(d.stop().unwrap_err().kind, "not_recording");
    assert_eq!(
        svc.store().meetings().unwrap().len(),
        1,
        "only the first started"
    );
}

fn events_meeting(events: &[MeetingEvent]) -> String {
    match &events[0] {
        MeetingEvent::Started { meeting, .. } => meeting.clone(),
        other => panic!("{other:?}"),
    }
}

#[test]
fn meeting_driver_a_failed_preflight_records_nothing_and_leaves_the_driver_free() {
    let dir = TempDir::new("driver-preflight");
    let svc = services(&dir);
    let prepare = FakePrepare::new(Script::PreflightFails);
    let (d, rx) = driver(Arc::clone(&prepare));
    let e = d.start(&svc, request(CaptureMode::Call)).unwrap_err();
    assert_eq!(e.kind, "model");
    assert_eq!(e.message, crate::plain_text::MODELS_NEEDED);
    assert!(!d.busy());
    assert!(
        rx.try_recv().is_err(),
        "no event for a meeting that never started"
    );
    assert!(svc.store().meetings().unwrap().is_empty());
    assert_eq!(d.stop().unwrap_err().kind, "not_recording");

    // The next start runs.
    *prepare.script.lock().unwrap() = Script::Ends;
    d.start(&svc, request(CaptureMode::Call)).unwrap();
    until_summary(&rx);
    d.join();
}

#[test]
fn meeting_driver_a_session_that_cannot_start_is_an_ipc_error() {
    let dir = TempDir::new("driver-start");
    let svc = services(&dir);
    let (d, rx) = driver(FakePrepare::new(Script::StartFails));
    let e = d.start(&svc, request(CaptureMode::Call)).unwrap_err();
    assert_eq!(e.kind, "capture");
    // What to do, by the error's kind; never Windows' own words (T9.23).
    assert_eq!(e.message, crate::plain_text::MICROPHONE_ACCESS_OFF);
    assert!(!e.message.contains("0x80070005"));
    assert!(rx.try_recv().is_err());
    assert!(svc.store().meetings().unwrap().is_empty());
    assert!(!d.busy());
}

#[test]
fn meeting_driver_stop_before_the_first_event_is_honoured() {
    let dir = TempDir::new("driver-early-stop");
    let svc = services(&dir);
    let (d, rx) = driver(FakePrepare::new(Script::UntilStopped));
    d.start(&svc, request(CaptureMode::Call)).unwrap();
    d.stop().unwrap();
    let events = until_summary(&rx);
    d.join();
    assert!(events.contains(&MeetingEvent::Ended {
        meeting: events_meeting(&events),
        reason: EndedReason::Stopped
    }));
}

#[test]
fn meeting_driver_a_panic_after_the_start_ends_with_a_failed_summary_and_frees_the_driver() {
    let dir = TempDir::new("driver-panic");
    let svc = services(&dir);
    let prepare = FakePrepare::new(Script::AfterPanics);
    let (d, rx) = driver(Arc::clone(&prepare));
    d.start(&svc, request(CaptureMode::Call)).unwrap();
    let events = until_summary(&rx);
    assert_eq!(
        events.last(),
        Some(&MeetingEvent::Summary {
            meeting: events_meeting(&events),
            result: SummaryEvent::RecordingFailed {
                message: MEETING_PANIC_MESSAGE.into(),
            },
        })
    );
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(e, MeetingEvent::Summary { .. }))
            .count(),
        1
    );
    d.join();
    assert!(!d.busy());
    assert!(rx.try_recv().is_err(), "nothing after the last event");
    // The next meeting runs.
    *prepare.script.lock().unwrap() = Script::Ends;
    d.start(&svc, request(CaptureMode::Call)).unwrap();
    until_summary(&rx);
    d.join();
}

#[test]
fn meeting_driver_a_panicking_preflight_is_a_failed_start() {
    let dir = TempDir::new("driver-preflight-panic");
    let svc = services(&dir);
    let (d, rx) = driver(FakePrepare::new(Script::PreflightPanics));
    let e = d.start(&svc, request(CaptureMode::Call)).unwrap_err();
    assert_eq!(e.kind, "internal");
    assert_eq!(
        e.message,
        "The meeting stopped unexpectedly before it started."
    );
    assert!(!d.busy());
    assert!(rx.try_recv().is_err());
}

#[test]
fn meeting_summary_setup_errors_name_the_missing_dlls_in_one_sentence() {
    use earshot_runtime::host::SummarySetupError;
    let e = summary_setup_error(SummarySetupError::LlamaDllsMissing(vec![
        "llama.dll".into(),
        "ggml.dll".into(),
    ]));
    assert_eq!(e.kind, "summary_dlls");
    assert_eq!(
        e.message,
        "A summary on this computer needs llama.cpp's DLLs next to Earshot; not found: llama.dll, ggml.dll"
    );
    let e = summary_setup_error(SummarySetupError::CudaDllsMissing(vec![
        "cublas64_13*.dll".into(),
    ]));
    assert_eq!(
        e.message,
        "A summary on this computer needs the CUDA runtime's DLLs on PATH; not found: cublas64_13*.dll"
    );
}

#[test]
fn meeting_driver_says_when_claude_code_ran_at_its_default_effort() {
    let dir = TempDir::new("driver-effort");
    let svc = services(&dir);
    let prepare = FakePrepare::new(Script::Ends);
    prepare.effort_unread.store(true, Ordering::Relaxed);
    let (d, rx) = driver(prepare);
    d.start(&svc, request(CaptureMode::Call)).unwrap();
    let events = until_summary(&rx);
    d.join();
    let ended = position(&events, |e| matches!(e, MeetingEvent::Ended { .. }));
    let note = position(
        &events,
        |e| matches!(e, MeetingEvent::Warning { key, .. } if key == "summary-effort"),
    );
    assert!(
        note > ended,
        "after the recording, so the screen shows it: {events:?}"
    );
    assert_eq!(
        events[note],
        MeetingEvent::Warning {
            key: "summary-effort".into(),
            kind: WarningKind::Backend,
            message: crate::meeting_events::CLAUDE_EFFORT_UNREAD.into(),
        }
    );
    // Without it, no such banner (a store of its own: the fake's ids repeat per driver).
    let dir = TempDir::new("driver-effort-none");
    let svc = services(&dir);
    let (d, rx) = driver(FakePrepare::new(Script::Ends));
    d.start(&svc, request(CaptureMode::Call)).unwrap();
    let events = until_summary(&rx);
    d.join();
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, MeetingEvent::Warning { key, .. } if key == "summary-effort")),
        "{events:?}"
    );
}

#[test]
fn meeting_driver_shows_a_setup_note_after_started() {
    let dir = TempDir::new("driver-note");
    let svc = services(&dir);
    let (d, rx) = driver(FakePrepare::new(Script::Ends));
    d.start(&svc, request(CaptureMode::Call)).unwrap();
    let events = until_summary(&rx);
    d.join();
    let started = position(&events, |e| matches!(e, MeetingEvent::Started { .. }));
    let note = position(
        &events,
        |e| matches!(e, MeetingEvent::Warning { key, .. } if key == "settings-0"),
    );
    assert!(note > started, "{events:?}");
    assert_eq!(
        events[note],
        MeetingEvent::Warning {
            key: "settings-0".into(),
            kind: WarningKind::Backend,
            message: PLACEMENT_NOTE.into(),
        }
    );
    let keys: Vec<String> = super::setup_notices(vec!["a".into(), "b".into()])
        .into_iter()
        .filter_map(|e| match e {
            MeetingEvent::Warning { key, .. } => Some(key),
            _ => None,
        })
        .collect();
    assert_eq!(keys, ["settings-0", "settings-1"], "one banner per note");
}

#[test]
fn meeting_driver_and_the_model_check_exclude_each_other() {
    use crate::activity::{CHECK_RUNNING, WORK_RUNNING, WorkGate};
    let dir = TempDir::new("driver-gate");
    let svc = services(&dir);
    let gate = WorkGate::new();
    let (tx, rx) = mpsc::channel();
    let tx = Mutex::new(tx);
    let sink: Sink = Arc::new(move |e| {
        let _ = tx.lock().unwrap().send(e);
    });
    let t0 = Instant::now();
    let d = MeetingDriver::new(
        FakePrepare::new(Script::UntilStopped),
        sink,
        Arc::new(move || t0),
        gate.clone(),
    );
    // A check runs: no meeting starts, and nothing is recorded.
    let check = gate.enter_check().unwrap();
    let e = d.start(&svc, request(CaptureMode::Call)).unwrap_err();
    assert_eq!(
        (e.kind.as_str(), e.message.as_str(), e.retryable),
        ("busy", CHECK_RUNNING, true)
    );
    assert!(svc.store().meetings().unwrap().is_empty());
    drop(check);
    // A meeting runs: no check starts until its thread has ended.
    d.start(&svc, request(CaptureMode::Call)).unwrap();
    assert_eq!(gate.enter_check().unwrap_err().message, WORK_RUNNING);
    d.stop().unwrap();
    until_summary(&rx);
    d.join();
    drop(gate.enter_check().unwrap());
}

#[test]
fn meeting_driver_stop_and_wait_gives_up_at_its_ceiling() {
    // T9.20: a source whose stop never returns holds the session thread; Quit must still end.
    let dir = TempDir::new("driver-stop-ceiling");
    let svc = services(&dir);
    let prepare = FakePrepare::new(Script::StopHangs);
    let gate = Arc::clone(&prepare.stop_gate);
    let (d, rx) = driver(prepare);
    let d = d.with_stop_ceiling(Duration::from_millis(200));
    d.start(&svc, request(CaptureMode::Call)).unwrap();
    assert!(
        !d.stop_and_wait(),
        "it returns, and says the recording is not saved"
    );
    assert_eq!(d.phase(), DriverPhase::Recording);
    // Once the source lets go, the meeting finishes as usual.
    release(&gate);
    let events = until_summary(&rx);
    assert!(
        events
            .iter()
            .any(|e| matches!(e, MeetingEvent::Ended { .. }))
    );
    d.join();
}

#[test]
fn meeting_driver_stop_keep_waiting_rule() {
    // The ceiling's rule, clock-free.
    let c = Duration::from_secs(15);
    assert!(keep_waiting(true, Duration::ZERO, c));
    assert!(keep_waiting(true, c - Duration::from_millis(1), c));
    assert!(!keep_waiting(true, c, c));
    assert!(!keep_waiting(false, Duration::ZERO, c));
    assert_eq!(STOP_CEILING, c);
    // It outlasts the capture source's own stop wait, with room to save the files after it.
    #[cfg(windows)]
    assert!(STOP_CEILING >= earshot_capture_wasapi::STOP_WAIT + Duration::from_secs(5));
}

#[cfg(windows)]
#[test]
fn meeting_driver_stop_saves_the_recording_when_the_capture_thread_never_ends() {
    // T9.20: the capture source gives its stuck thread up, the recording is saved and the
    // user hears why; the stop and wait returns with nothing recording.
    let dir = TempDir::new("driver-stuck-capture");
    let svc = services(&dir);
    let prepare = FakePrepare::new(Script::CaptureThreadHangs);
    let gate = Arc::clone(&prepare.stop_gate);
    let (d, rx) = driver(prepare);
    let meeting = d.start(&svc, request(CaptureMode::Call)).unwrap();
    assert!(d.stop_and_wait());
    let events = until_summary(&rx);
    let warned = position(
        &events,
        |e| matches!(e, MeetingEvent::Warning { key, .. } if key == "device-stopped-answering"),
    );
    let ended = position(&events, |e| matches!(e, MeetingEvent::Ended { .. }));
    assert!(warned < ended, "{events:?}");
    let id: earshot_core::identity::EntityId = meeting.parse().unwrap();
    let files = svc.store().audio_files(id).unwrap();
    assert_eq!(files.len(), 2, "both tracks are saved");
    d.join();
    release(&gate);
}

#[test]
fn meeting_driver_stop_and_wait_returns_with_the_recording_finalised() {
    let dir = TempDir::new("driver-stop-wait");
    let svc = services(&dir);
    let (d, rx) = driver(FakePrepare::new(Script::UntilStopped));
    assert_eq!(d.phase(), DriverPhase::Idle);
    d.stop_and_wait();
    d.start(&svc, request(CaptureMode::Call)).unwrap();
    assert_eq!(d.phase(), DriverPhase::Recording);
    d.stop_and_wait();
    assert_ne!(d.phase(), DriverPhase::Recording);
    let events = until_summary(&rx);
    assert!(
        events
            .iter()
            .any(|e| matches!(e, MeetingEvent::Ended { .. })),
        "the recording ended"
    );
    d.join();
    assert_eq!(d.phase(), DriverPhase::Idle);
}

/// How the queue's fake pre-flight runs each meeting: a replay until stopped and an `after` step
/// that waits for its own gate, each recording what ran in what order.
struct QueuePrepare {
    gates: Mutex<Vec<mpsc::Receiver<()>>>,
    order: Arc<Mutex<Vec<String>>>,
    seed: std::sync::atomic::AtomicU64,
    hold_failure: bool,
}

impl QueuePrepare {
    fn new(gates: Vec<mpsc::Receiver<()>>) -> Arc<Self> {
        Arc::new(Self {
            gates: Mutex::new(gates),
            order: Arc::default(),
            seed: 500.into(),
            hold_failure: false,
        })
    }
}

impl Prepare for QueuePrepare {
    fn prepare(
        &self,
        _svc: &Services,
        request: &StartRequest,
        ticket: &Arc<QueueTicket>,
    ) -> Result<Prepared, IpcError> {
        use earshot_runtime::live::{LiveAsrFactory, LiveHold};
        use earshot_runtime::meeting_queue::MeetingHold;
        let gate = {
            let mut gates = self.gates.lock().unwrap();
            (!gates.is_empty()).then(|| gates.remove(0))
        };
        let order = Arc::clone(&self.order);
        let seed = self.seed.fetch_add(1, Ordering::Relaxed);
        let title = request.title.clone();
        let hold = LiveHold::new(Arc::new(MeetingHold::new(Arc::clone(ticket))));
        let failing = self.hold_failure;
        let start: StartFn = Box::new(move |svc: &Services| {
            let factory: LiveAsrFactory = Box::new(move |_, _| {
                if failing {
                    Err(RuntimeError::Internal("model missing"))
                } else {
                    Ok(None)
                }
            });
            start_live(
                UntilStopped {
                    inner: replay(0.5, false),
                    stopped: false,
                },
                svc.clone(),
                LiveConfig {
                    title,
                    hold: Some(hold),
                    ..config()
                },
                Some(factory),
                Box::new(ChaCha8Rng::seed_from_u64(seed)),
            )
        });
        let after: AfterFn = Box::new(move |svc, outcome, _cancel, progress| {
            let id = outcome.meeting;
            order.lock().unwrap().push(format!("{id} starts"));
            let post = Ok(HostPost::Skipped);
            progress(HostStep::PostFinished(&post));
            if let Some(gate) = gate {
                gate.recv_timeout(Duration::from_secs(120)).unwrap();
            }
            order.lock().unwrap().push(format!("{id} ends"));
            let _ = svc;
            HostOutcome {
                missing_tracks: vec![],
                post,
                summary: SummaryResult::Skipped {
                    reason: SkipReason::NotRequested,
                },
                network: vec![],
                handovers: vec![],
                handover_record_failures: vec!["a store error".into()],
                claude_effort_unread: false,
            }
        });
        Ok(Prepared {
            start,
            after,
            notices: Vec::new(),
            call_signal: None,
        })
    }
}

fn queue_driver(
    prepare: Arc<QueuePrepare>,
    svc: &Services,
) -> (MeetingDriver, Receiver<MeetingEvent>) {
    let _ = svc;
    let (tx, rx) = mpsc::channel();
    let tx = Mutex::new(tx);
    let sink: Sink = Arc::new(move |e| {
        let _ = tx.lock().unwrap().send(e);
    });
    let t0 = Instant::now();
    (
        MeetingDriver::new(
            prepare,
            sink,
            Arc::new(move || t0),
            crate::activity::WorkGate::new(),
        ),
        rx,
    )
}

/// Events until `pred` holds, bounded.
fn events_until(
    rx: &Receiver<MeetingEvent>,
    pred: impl Fn(&MeetingEvent) -> bool,
) -> Vec<MeetingEvent> {
    let mut out = Vec::new();
    loop {
        let e = rx
            .recv_timeout(Duration::from_secs(120))
            .expect("the event never came");
        let hit = pred(&e);
        out.push(e);
        if hit {
            return out;
        }
    }
}

fn saved_is(meeting: &str, e: &MeetingEvent) -> bool {
    matches!(e, MeetingEvent::Ended { meeting: m, .. } if m == meeting)
}

/// The whole queue through the driver: a second meeting starts while the first finishes, records,
/// waits its turn, and the older meeting's end reaches the UI with its own id.
#[test]
fn meeting_queue_driver_a_second_meeting_starts_while_the_first_finishes() {
    let dir = TempDir::new("queue");
    let svc = services(&dir);
    let (first_done, first_gate) = mpsc::channel();
    let prepare = QueuePrepare::new(vec![first_gate]);
    let (d, rx) = queue_driver(Arc::clone(&prepare), &svc);

    let first = d.start(&svc, request(CaptureMode::InRoom)).unwrap();
    assert_eq!(
        d.start(&svc, request(CaptureMode::InRoom))
            .unwrap_err()
            .kind,
        "busy",
        "one recording at a time"
    );
    d.stop().unwrap();
    events_until(&rx, |e| saved_is(&first, e));
    // Its `after` is running and blocked: the phase says processing, and a start goes in.
    let limit = Instant::now() + Duration::from_secs(120);
    while d.phase() != DriverPhase::Processing {
        assert!(Instant::now() < limit);
        std::thread::sleep(Duration::from_millis(1));
    }
    let second = d.start(&svc, request(CaptureMode::InRoom)).unwrap();
    assert_ne!(first, second);
    assert_eq!(d.phase(), DriverPhase::Recording);
    assert_eq!(
        d.start(&svc, request(CaptureMode::InRoom))
            .unwrap_err()
            .kind,
        "busy"
    );
    // Held: it says so, with no model in sight.
    let held = events_until(&rx, |e| {
        *e == MeetingEvent::Transcript {
            state: crate::meeting_events::TranscriptState::WaitingForPreviousMeeting,
        }
    });
    assert!(matches!(&held[0], MeetingEvent::Started { meeting, .. } if *meeting == second));

    // The second stops while the first still runs: its recording is saved, its `after` waits.
    d.stop_and_wait();
    assert_ne!(d.phase(), DriverPhase::Recording);
    let id = crate::detail::parse_id(&second).unwrap();
    let saved = svc.store().audio_files(id).unwrap();
    assert_eq!(saved.len(), 1, "saved before its turn");
    let limit = Instant::now() + Duration::from_secs(120);
    while svc.meeting_activity(id).unwrap() != Some(earshot_runtime::MeetingActivity::Waiting) {
        assert!(Instant::now() < limit);
        std::thread::sleep(Duration::from_millis(1));
    }
    assert_eq!(
        prepare.order.lock().unwrap().as_slice(),
        [format!("{first} starts")],
        "the second's work waits for the first's"
    );

    first_done.send(()).unwrap();
    let rest = events_until(
        &rx,
        |e| matches!(e, MeetingEvent::Summary { meeting, .. } if *meeting == second),
    );
    d.join();
    assert_eq!(
        prepare.order.lock().unwrap().as_slice(),
        [
            format!("{first} starts"),
            format!("{first} ends"),
            format!("{second} starts"),
            format!("{second} ends"),
        ]
    );
    // The first's summary came first, with its own id; the meeting on screen heard no warning of
    // the older one's: it had moved on.
    let summaries: Vec<&String> = rest
        .iter()
        .filter_map(|e| match e {
            MeetingEvent::Summary { meeting, .. } => Some(meeting),
            _ => None,
        })
        .collect();
    assert_eq!(summaries, [&first, &second]);
    let warnings = rest
        .iter()
        .filter(|e| matches!(e, MeetingEvent::Warning { .. }))
        .count();
    assert_eq!(warnings, 1, "only the newest meeting's: {rest:?}");
    assert_eq!(d.phase(), DriverPhase::Idle);
    assert!(!d.busy());
}

/// Quitting with one meeting finishing and another held loses nothing of the held one.
#[test]
fn meeting_queue_driver_quitting_during_a_hold_saves_the_recording() {
    let dir = TempDir::new("queue-quit");
    let svc = services(&dir);
    let (first_done, first_gate) = mpsc::channel();
    let prepare = QueuePrepare::new(vec![first_gate]);
    let (d, rx) = queue_driver(prepare, &svc);
    let first = d.start(&svc, request(CaptureMode::InRoom)).unwrap();
    d.stop().unwrap();
    events_until(&rx, |e| saved_is(&first, e));
    let second = d.start(&svc, request(CaptureMode::InRoom)).unwrap();
    // What "Quit Earshot" does: the older meeting still runs and this one is held.
    d.stop_and_wait();
    let id = crate::detail::parse_id(&second).unwrap();
    assert_eq!(svc.store().audio_files(id).unwrap().len(), 1);
    assert!(
        svc.store()
            .meeting(id)
            .unwrap()
            .unwrap()
            .duration_ms
            .is_some(),
        "nothing left for a later start to call interrupted"
    );
    // The rest is cut by the exit in the app; here the test lets it end.
    first_done.send(()).unwrap();
    d.join();
}

/// A failed model load during a hold costs no audio, and the meeting ends normally.
#[test]
fn meeting_queue_driver_a_failed_load_keeps_the_meeting_recording() {
    let dir = TempDir::new("queue-load");
    let svc = services(&dir);
    let prepare = Arc::new(QueuePrepare {
        gates: Mutex::new(Vec::new()),
        order: Arc::default(),
        seed: 600.into(),
        hold_failure: true,
    });
    let (d, rx) = queue_driver(prepare, &svc);
    let meeting = d.start(&svc, request(CaptureMode::InRoom)).unwrap();
    let events = events_until(&rx, |e| {
        *e == MeetingEvent::Transcript {
            state: crate::meeting_events::TranscriptState::Stopped,
        }
    });
    assert!(
        events
            .iter()
            .all(|e| !matches!(e, MeetingEvent::Warning { .. })),
        "its own banner says it: {events:?}"
    );
    assert_eq!(d.phase(), DriverPhase::Recording, "still recording");
    d.stop().unwrap();
    until_summary(&rx);
    d.join();
    let id = crate::detail::parse_id(&meeting).unwrap();
    assert_eq!(svc.store().audio_files(id).unwrap().len(), 1);
}

/// After the save a session's failure is about the live work: nothing says the recording failed.
#[test]
fn meeting_queue_driver_a_session_failure_after_the_save_does_not_blame_the_recording() {
    let e = RuntimeError::Poisoned;
    // Before the save it claims nothing about what was kept and offers no re-summary.
    let SummaryEvent::RecordingFailed { message: before } = session_failure(false, &e) else {
        panic!("before the save the recording failed");
    };
    assert!(!before.contains("saved"), "{before}");
    assert!(
        before.starts_with("The recording could not be finished"),
        "{before}"
    );
    // Fixed copy after it, never the error's own text (T9.23).
    assert!(before.ends_with(crate::plain_text::INTERNAL), "{before}");
    assert!(!before.contains(&e.to_string()), "{before}");
    // After the save it is the live work: the screen offers Re-transcribe.
    let SummaryEvent::PostFailed { message: after, .. } = session_failure(true, &e) else {
        panic!("after the save the transcript is what did not finish");
    };
    assert!(after.starts_with("The recording is saved"), "{after}");
    assert!(after.contains("live transcript"), "{after}");
    assert!(!after.contains("could not be finished"), "{after}");
}

/// What a meeting's work after the recording does in the cancel tests.
enum Finish {
    /// A final pass that runs until it is cancelled; says when it is running.
    PassUntilCancelled(mpsc::Sender<()>),
    /// A summary that runs until it is cancelled, after a final pass; says when it is running.
    SummaryUntilCancelled(mpsc::Sender<()>),
    /// Runs until its gate opens and logs whether it was cancelled.
    Gate(mpsc::Receiver<()>),
    /// The live transcript's catch-up hangs in a call that never returns (until the test
    /// releases it), given up after `stall` (T9.22).
    LiveHangs {
        stall: Duration,
        release: Arc<Mutex<mpsc::Receiver<()>>>,
    },
    /// The work after the meeting hangs in a call that polls nothing, until the test releases it:
    /// in a final-pass step when `in_step`, else while it loads (before any step; T9.22b). Says
    /// when it is in the call.
    AfterHangs {
        in_step: bool,
        inside: mpsc::Sender<()>,
        release: mpsc::Receiver<()>,
    },
    /// The live model load on the hold thread hangs until the test releases it, the hold's load
    /// bound `stall` (T9.22b). Says when it is loading.
    LoadHangs {
        stall: Duration,
        loading: mpsc::Sender<()>,
        release: mpsc::Receiver<()>,
    },
    /// Only logs that it started (what `LoadHangs` runs after the recording, if anything).
    Logs,
}

/// A transcriber stuck in a call that never returns, polling nothing, until released.
struct HungTranscriber(Arc<Mutex<mpsc::Receiver<()>>>);

impl earshot_core::asr::Transcriber for HungTranscriber {
    fn transcribe(
        &mut self,
        _audio: &MonoBuffer,
        _options: &earshot_core::asr::TranscribeOptions,
        _cancel: &CancelToken,
    ) -> Result<earshot_core::asr::Transcription, earshot_core::asr::AsrError> {
        let _ = self.0.lock().unwrap().recv();
        Err(earshot_core::asr::AsrError::Cancelled)
    }

    fn backend_info(&self) -> earshot_core::asr::BackendInfo {
        earshot_core::asr::BackendInfo {
            kind: BackendKind::Fake,
            device: "hung".into(),
            model: "hung".into(),
        }
    }
}

/// A replay that delivers all its audio at once, then holds its end until it is stopped: a
/// meeting whose live transcript has all of it to catch up on after the stop.
struct PlaysThenWaits {
    inner: ReplaySource,
    end: Option<earshot_core::capture::EndOfStream>,
    played: bool,
    stopped: bool,
    ended: bool,
}

struct KeepEnd<'a>(
    &'a mut dyn CaptureSink,
    &'a mut Option<earshot_core::capture::EndOfStream>,
);

impl CaptureSink for KeepEnd<'_> {
    fn frames(&mut self, track: TrackId, frames: &[f32], at: DeviceTime) {
        self.0.frames(track, frames, at);
    }
    fn discontinuity(&mut self, track: TrackId, d: earshot_core::capture::Discontinuity) {
        self.0.discontinuity(track, d);
    }
    fn end(&mut self, end: earshot_core::capture::EndOfStream) {
        *self.1 = Some(end);
    }
}

impl AudioSource for PlaysThenWaits {
    type Error = <ReplaySource as AudioSource>::Error;
    fn session(&self) -> &CaptureSession {
        self.inner.session()
    }
    fn pump(&mut self, sink: &mut dyn CaptureSink) -> Result<SourceStatus, Self::Error> {
        if self.ended {
            return Ok(SourceStatus::Ended);
        }
        if !self.played {
            let status = self.inner.pump(&mut KeepEnd(sink, &mut self.end))?;
            if status != SourceStatus::Ended {
                return Ok(status);
            }
            self.played = true;
        }
        if !self.stopped {
            return Ok(SourceStatus::Idle);
        }
        self.ended = true;
        if let Some(mut end) = self.end.take() {
            end.reason = earshot_core::capture::EndReason::Stopped;
            sink.end(end);
        }
        Ok(SourceStatus::Ended)
    }
    fn stop(&mut self) {
        self.stopped = true;
    }
}

/// Quiet noise, then two seconds of loud noise, then quiet: one stretch of speech to decode.
fn one_stretch() -> ReplaySource {
    let mut x: u32 = 0x2545_f491;
    let mut noise = |n: usize, amp: f32| -> Vec<f32> {
        (0..n)
            .map(|_| {
                x = x.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                ((x >> 8) as f32 / (1u32 << 24) as f32 * 2.0 - 1.0) * amp
            })
            .collect()
    };
    let mut audio = noise(32_000, 1e-4);
    audio.extend(noise(32_000, 0.3));
    audio.extend(noise(32_000, 1e-4));
    ReplayBuilder::new(
        DeviceTime::from_ticks(5_000_000),
        MonoBuffer::new(PIPELINE_RATE, audio),
    )
    .build()
    .unwrap()
}

/// Waits for a cancel the way a model call does: nothing else ends it, and only a failure runs into
/// the guard.
fn until_cancelled(cancel: &CancelToken) {
    let deadline = Instant::now() + MEET_GUARD;
    while !cancel.is_cancelled() {
        assert!(Instant::now() < deadline, "no cancel came");
        std::thread::sleep(Duration::from_millis(1));
    }
}

fn outcome_of(post: Result<HostPost, RuntimeError>, summary: SummaryResult) -> HostOutcome {
    HostOutcome {
        missing_tracks: vec![],
        post,
        summary,
        network: vec![],
        handovers: vec![],
        handover_record_failures: vec![],
        claude_effort_unread: false,
    }
}

/// Meetings in the real queue (a replay until stopped, held by their ticket), each with its scripted
/// work after the recording.
struct CancelPrepare {
    finishes: Mutex<std::collections::VecDeque<Finish>>,
    log: Arc<Mutex<Vec<String>>>,
    seed: std::sync::atomic::AtomicU64,
}

impl CancelPrepare {
    fn new(finishes: Vec<Finish>) -> Arc<Self> {
        Arc::new(Self {
            finishes: Mutex::new(finishes.into()),
            log: Arc::default(),
            seed: 700.into(),
        })
    }

    fn logged(&self) -> Vec<String> {
        self.log.lock().unwrap().clone()
    }
}

impl Prepare for CancelPrepare {
    fn prepare(
        &self,
        _svc: &Services,
        request: &StartRequest,
        ticket: &Arc<QueueTicket>,
    ) -> Result<Prepared, IpcError> {
        use earshot_runtime::live::{LiveAsrFactory, LiveHold};
        use earshot_runtime::meeting_queue::MeetingHold;
        let finish = self.finishes.lock().unwrap().pop_front().unwrap();
        let log = Arc::clone(&self.log);
        let seed = self.seed.fetch_add(1, Ordering::Relaxed);
        let title = request.title.clone();
        let hold = LiveHold::new(Arc::new(MeetingHold::new(Arc::clone(ticket))));
        if let Finish::LiveHangs { stall, release } = &finish {
            let (stall, release) = (*stall, Arc::clone(release));
            let start: StartFn = Box::new(move |svc: &Services| {
                let mut slot = Some(release);
                let factory: LiveAsrFactory = Box::new(move |track, entity| {
                    let Some(release) = slot.take().filter(|_| track == TrackId::Mic) else {
                        return Ok(None);
                    };
                    let n = std::cell::Cell::new(0u8);
                    Ok(Some(
                        earshot_core::pipeline::LiveAsr {
                            transcriber: Box::new(HungTranscriber(release)),
                            vad: Box::new(earshot_core::asr::EnergyVad::default()),
                            config: earshot_core::asr::ChunkerConfig::default(),
                            track_entity: entity,
                            segment_ids: Box::new(move || {
                                n.set(n.get().wrapping_add(1));
                                earshot_core::identity::EntityId::from_bytes([
                                    0xC0,
                                    n.get(),
                                    0,
                                    0,
                                    0,
                                    0,
                                    0,
                                    0,
                                    0,
                                    0,
                                    0,
                                    0,
                                    0,
                                    0,
                                    0,
                                    1,
                                ])
                            }),
                        }
                        .into(),
                    ))
                });
                start_live(
                    PlaysThenWaits {
                        inner: one_stretch(),
                        end: None,
                        played: false,
                        stopped: false,
                        ended: false,
                    },
                    svc.clone(),
                    LiveConfig {
                        title,
                        catch_up_stall: stall,
                        ..config()
                    },
                    Some(factory),
                    Box::new(ChaCha8Rng::seed_from_u64(seed)),
                )
            });
            let after: AfterFn = Box::new(move |_svc, outcome, cancel, _progress| {
                log.lock().unwrap().push(format!(
                    "{} after, cancelled {}",
                    outcome.meeting,
                    cancel.is_cancelled()
                ));
                outcome_of(Ok(HostPost::Skipped), SummaryResult::Cancelled)
            });
            return Ok(Prepared {
                start,
                after,
                notices: Vec::new(),
                call_signal: None,
            });
        }
        let mut hold = hold;
        let (finish, hung_load) = match finish {
            Finish::LoadHangs {
                stall,
                loading,
                release,
            } => {
                hold.load_stall = stall;
                (Finish::Logs, Some(Mutex::new((loading, release))))
            }
            other => (other, None),
        };
        let start: StartFn = Box::new(move |svc: &Services| {
            let factory: LiveAsrFactory = match hung_load {
                Some(hung) => Box::new(move |_, _| {
                    let hung = hung.lock().unwrap();
                    let _ = hung.0.send(());
                    let _ = hung.1.recv();
                    Err(RuntimeError::Internal("released"))
                }),
                None => Box::new(|_, _| Ok(None)),
            };
            start_live(
                UntilStopped {
                    inner: replay(0.5, false),
                    stopped: false,
                },
                svc.clone(),
                LiveConfig {
                    title,
                    hold: Some(hold),
                    ..config()
                },
                Some(factory),
                Box::new(ChaCha8Rng::seed_from_u64(seed)),
            )
        });
        let after: AfterFn = Box::new(move |_svc, outcome, cancel, progress| {
            let id = outcome.meeting;
            log.lock().unwrap().push(format!("{id} starts"));
            match finish {
                Finish::PassUntilCancelled(running) => {
                    running.send(()).unwrap();
                    until_cancelled(cancel);
                    let post = Ok(HostPost::Cancelled {
                        speaker_windows_deleted: Ok(0),
                    });
                    progress(HostStep::PostFinished(&post));
                    outcome_of(post, SummaryResult::Cancelled)
                }
                Finish::SummaryUntilCancelled(running) => {
                    let post = Ok(HostPost::Skipped);
                    progress(HostStep::PostFinished(&post));
                    running.send(()).unwrap();
                    until_cancelled(cancel);
                    outcome_of(post, SummaryResult::Cancelled)
                }
                Finish::Gate(gate) => {
                    let post = Ok(HostPost::Skipped);
                    progress(HostStep::PostFinished(&post));
                    gate.recv_timeout(MEET_GUARD).unwrap();
                    let cancelled = cancel.is_cancelled();
                    log.lock()
                        .unwrap()
                        .push(format!("{id} ends, cancelled {cancelled}"));
                    let reason = SkipReason::NotRequested;
                    outcome_of(post, SummaryResult::Skipped { reason })
                }
                Finish::AfterHangs {
                    in_step,
                    inside,
                    release,
                } => {
                    if in_step {
                        progress(HostStep::PostStarting {
                            fallbacks: &[],
                            work: &[],
                            asr_backend: BackendKind::Fake,
                        });
                    }
                    inside.send(()).unwrap();
                    let _ = release.recv();
                    let cancelled = cancel.is_cancelled();
                    log.lock()
                        .unwrap()
                        .push(format!("{id} returned, cancelled {cancelled}"));
                    outcome_of(Ok(HostPost::Skipped), SummaryResult::Cancelled)
                }
                Finish::Logs => outcome_of(
                    Ok(HostPost::Skipped),
                    SummaryResult::Skipped {
                        reason: SkipReason::NotRequested,
                    },
                ),
                Finish::LiveHangs { .. } | Finish::LoadHangs { .. } => {
                    unreachable!("taken above")
                }
            }
        });
        Ok(Prepared {
            start,
            after,
            notices: Vec::new(),
            call_signal: None,
        })
    }
}

fn cancel_driver(prepare: Arc<CancelPrepare>) -> (MeetingDriver, Receiver<MeetingEvent>) {
    cancel_driver_on(prepare, crate::activity::WorkGate::new())
}

/// [`cancel_driver`] whose meetings take their passes from `gate`.
fn cancel_driver_on(
    prepare: Arc<CancelPrepare>,
    gate: crate::activity::WorkGate,
) -> (MeetingDriver, Receiver<MeetingEvent>) {
    let (tx, rx) = mpsc::channel();
    let tx = Mutex::new(tx);
    let sink: Sink = Arc::new(move |e| {
        let _ = tx.lock().unwrap().send(e);
    });
    let t0 = Instant::now();
    (
        MeetingDriver::new(prepare, sink, Arc::new(move || t0), gate),
        rx,
    )
}

fn is_summary_of(meeting: &str, e: &MeetingEvent) -> bool {
    matches!(e, MeetingEvent::Summary { meeting: m, .. } if m == meeting)
}

/// The result in the summary event of `meeting` at the end of `events`.
fn result_of(events: &[MeetingEvent], meeting: &str) -> SummaryEvent {
    match events.last() {
        Some(MeetingEvent::Summary { meeting: m, result }) if m == meeting => result.clone(),
        other => panic!("{other:?}"),
    }
}

/// Waits for what the meeting's rows and leases say about it.
fn activity_becomes(svc: &Services, meeting: &str, wanted: earshot_runtime::MeetingActivity) {
    let id = crate::detail::parse_id(meeting).unwrap();
    let deadline = Instant::now() + MEET_GUARD;
    while svc.meeting_activity(id).unwrap() != Some(wanted) {
        assert!(Instant::now() < deadline, "never became {wanted:?}");
        std::thread::sleep(Duration::from_millis(1));
    }
}

fn recordings_of(svc: &Services, meeting: &str) -> usize {
    let id = crate::detail::parse_id(meeting).unwrap();
    svc.store().audio_files(id).unwrap().len()
}

#[test]
fn meeting_cancel_stops_the_final_pass_and_leaves_the_meeting_unprocessed() {
    let dir = TempDir::new("cancel-pass");
    let svc = services(&dir);
    let (running_tx, running) = mpsc::channel();
    let prepare = CancelPrepare::new(vec![Finish::PassUntilCancelled(running_tx)]);
    let (d, rx) = cancel_driver(prepare);
    let meeting = d.start(&svc, request(CaptureMode::InRoom)).unwrap();
    d.stop().unwrap();
    running.recv_timeout(MEET_GUARD).unwrap();

    d.cancel_finishing(&meeting);
    let events = events_until(&rx, |e| is_summary_of(&meeting, e));
    d.join();
    assert_eq!(result_of(&events, &meeting), SummaryEvent::PostCancelled);
    assert!(rx.try_recv().is_err(), "nothing after the last event");
    assert_eq!(recordings_of(&svc, &meeting), 1, "the audio stays");
    activity_becomes(
        &svc,
        &meeting,
        earshot_runtime::MeetingActivity::Unprocessed,
    );
    assert_eq!(d.phase(), DriverPhase::Idle);
}

#[test]
fn meeting_cancel_reaches_a_hung_live_catch_up() {
    // T9.22: Cancel on "Finishing the meeting" works while the live transcript catches up, even
    // when its decode never returns. The stall bound (an hour) cannot end it within the guard.
    let dir = TempDir::new("cancel-catch-up");
    let svc = services(&dir);
    let (release_tx, release_rx) = mpsc::channel::<()>();
    let prepare = CancelPrepare::new(vec![Finish::LiveHangs {
        stall: Duration::from_secs(3600),
        release: Arc::new(Mutex::new(release_rx)),
    }]);
    let (d, rx) = cancel_driver(Arc::clone(&prepare));
    let meeting = d.start(&svc, request(CaptureMode::InRoom)).unwrap();
    assert!(d.stop_and_wait(), "the recording is saved");
    d.cancel_finishing(&meeting);
    let events = events_until(&rx, |e| is_summary_of(&meeting, e));
    d.join();
    assert_eq!(result_of(&events, &meeting), SummaryEvent::PostCancelled);
    assert!(prepare.logged().is_empty(), "nothing ran after the cancel");
    assert_eq!(recordings_of(&svc, &meeting), 1, "the audio stays");
    activity_becomes(
        &svc,
        &meeting,
        earshot_runtime::MeetingActivity::Unprocessed,
    );
    drop(release_tx);
}

#[test]
fn meeting_with_a_stalled_live_catch_up_is_left_and_the_next_one_goes() {
    // T9.22: a catch-up that stops moving is given up; the meeting is left unprocessed with a
    // banner and the next meeting's turn comes without a restart.
    let dir = TempDir::new("stalled-catch-up");
    let svc = services(&dir);
    let (release_tx, release_rx) = mpsc::channel::<()>();
    let (open, gate) = mpsc::channel();
    let prepare = CancelPrepare::new(vec![
        Finish::LiveHangs {
            stall: Duration::from_millis(200),
            release: Arc::new(Mutex::new(release_rx)),
        },
        Finish::Gate(gate),
    ]);
    let (d, rx) = cancel_driver(Arc::clone(&prepare));
    let first = d.start(&svc, request(CaptureMode::InRoom)).unwrap();
    assert!(d.stop_and_wait());
    let events = events_until(&rx, |e| is_summary_of(&first, e));
    assert!(
        events
            .iter()
            .any(|e| matches!(e, MeetingEvent::Warning { key, .. } if key == "behind:you")),
        "{events:?}"
    );
    assert_eq!(
        result_of(&events, &first),
        SummaryEvent::Failed {
            message: CATCH_UP_STALLED_MESSAGE.into(),
            retryable: false,
        }
    );
    activity_becomes(&svc, &first, earshot_runtime::MeetingActivity::Unprocessed);

    let second = d.start(&svc, request(CaptureMode::InRoom)).unwrap();
    d.stop().unwrap();
    open.send(()).unwrap();
    let events = events_until(&rx, |e| is_summary_of(&second, e));
    d.join();
    assert!(matches!(
        result_of(&events, &second),
        SummaryEvent::Skipped { .. }
    ));
    assert_eq!(
        prepare.logged(),
        [
            format!("{second} starts"),
            format!("{second} ends, cancelled false")
        ],
        "the first never ran anything after its recording"
    );
    drop(release_tx);
}

/// Short bounds for the work after the meeting: only work that never answers runs into them.
const SHORT_AFTER: AfterBounds = AfterBounds {
    load: Duration::from_millis(200),
    step: Duration::from_millis(200),
    cancelled: Duration::from_millis(200),
};

/// Waits until the log holds `line`: a thread left behind ends on its own.
fn logged_eventually(prepare: &CancelPrepare, line: &str) {
    let deadline = Instant::now() + MEET_GUARD;
    while !prepare.logged().iter().any(|l| l == line) {
        assert!(Instant::now() < deadline, "never logged {line}");
        std::thread::sleep(Duration::from_millis(1));
    }
}

/// T9.22b: work after the meeting that stops answering (in a final-pass step, or while it loads)
/// is left: its banner and a failed summary event, the meeting unprocessed, the driver free while
/// the thread is still stuck, and its token cancelled so it stops when its call returns.
fn after_that_stops_answering_is_left(in_step: bool) {
    let dir = TempDir::new("after-stalls");
    let svc = services(&dir);
    let (inside_tx, inside) = mpsc::channel();
    let (release, release_rx) = mpsc::channel();
    let prepare = CancelPrepare::new(vec![Finish::AfterHangs {
        in_step,
        inside: inside_tx,
        release: release_rx,
    }]);
    let (d, rx) = cancel_driver(Arc::clone(&prepare));
    let d = d.with_after_bounds(SHORT_AFTER);
    let meeting = d.start(&svc, request(CaptureMode::InRoom)).unwrap();
    d.stop().unwrap();
    inside.recv_timeout(MEET_GUARD).unwrap();
    let events = events_until(&rx, |e| is_summary_of(&meeting, e));
    assert_eq!(
        result_of(&events, &meeting),
        SummaryEvent::Failed {
            message: POST_STALLED_MESSAGE.into(),
            retryable: false,
        }
    );
    assert!(
        events.iter().any(|e| matches!(
            e,
            MeetingEvent::Warning { key, message, .. }
                if key == POST_STALLED_KEY && message == POST_STALLED_BANNER
        )),
        "{events:?}"
    );
    // Still stuck, and nothing waits for it.
    d.join();
    assert_eq!(d.phase(), DriverPhase::Idle);
    assert_eq!(prepare.logged(), [format!("{meeting} starts")]);
    assert_eq!(recordings_of(&svc, &meeting), 1, "the audio stays");
    activity_becomes(
        &svc,
        &meeting,
        earshot_runtime::MeetingActivity::Unprocessed,
    );
    release.send(()).unwrap();
    logged_eventually(&prepare, &format!("{meeting} returned, cancelled true"));
}

#[test]
fn meeting_after_work_left_behind_keeps_its_pass_until_it_ends() {
    // A left thread may still wake and load models: until it ends, no model check or benchmark
    // starts beside it, as for a meeting job; the voice sample may run beside it, as beside a job.
    use crate::activity::{WORK_RUNNING, WORK_RUNNING_FOR_BENCHMARK, WorkGate};
    let dir = TempDir::new("after-left-pass");
    let svc = services(&dir);
    let (inside_tx, inside) = mpsc::channel();
    let (release, release_rx) = mpsc::channel();
    let prepare = CancelPrepare::new(vec![Finish::AfterHangs {
        in_step: true,
        inside: inside_tx,
        release: release_rx,
    }]);
    let gate = WorkGate::new();
    let (d, rx) = cancel_driver_on(Arc::clone(&prepare), gate.clone());
    let d = d.with_after_bounds(SHORT_AFTER);
    let meeting = d.start(&svc, request(CaptureMode::InRoom)).unwrap();
    d.stop().unwrap();
    inside.recv_timeout(MEET_GUARD).unwrap();
    let events = events_until(&rx, |e| is_summary_of(&meeting, e));
    assert!(matches!(
        result_of(&events, &meeting),
        SummaryEvent::Failed { .. }
    ));
    d.join();
    assert_eq!(d.phase(), DriverPhase::Idle);
    assert_eq!(gate.enter_check().unwrap_err().message, WORK_RUNNING);
    assert_eq!(
        gate.enter_benchmark().unwrap_err().message,
        WORK_RUNNING_FOR_BENCHMARK
    );
    drop(gate.enter_voice().unwrap());
    release.send(()).unwrap();
    logged_eventually(&prepare, &format!("{meeting} returned, cancelled true"));
    let deadline = Instant::now() + MEET_GUARD;
    let check = loop {
        match gate.enter_check() {
            Ok(check) => break check,
            Err(e) => assert_eq!(e.message, WORK_RUNNING),
        }
        assert!(Instant::now() < deadline, "the left thread kept its pass");
        std::thread::sleep(Duration::from_millis(1));
    };
    drop(check);
}

#[test]
fn meeting_final_pass_step_that_stops_answering_is_left() {
    after_that_stops_answering_is_left(true);
}

#[test]
fn meeting_final_pass_load_that_stops_answering_is_left() {
    after_that_stops_answering_is_left(false);
}

#[test]
fn meeting_live_model_load_that_never_returns_leaves_the_meeting_unprocessed() {
    // T9.22b: a live model load on the hold thread still running at the end is left after its
    // bound; the meeting is left unprocessed before its turn, as for a stalled catch-up.
    let dir = TempDir::new("load-stalls");
    let svc = services(&dir);
    let (loading_tx, loading) = mpsc::channel();
    let (release, release_rx) = mpsc::channel();
    let prepare = CancelPrepare::new(vec![Finish::LoadHangs {
        stall: Duration::from_millis(200),
        loading: loading_tx,
        release: release_rx,
    }]);
    let (d, rx) = cancel_driver(Arc::clone(&prepare));
    let meeting = d.start(&svc, request(CaptureMode::InRoom)).unwrap();
    loading.recv_timeout(MEET_GUARD).unwrap();
    d.stop().unwrap();
    let events = events_until(&rx, |e| is_summary_of(&meeting, e));
    d.join();
    assert_eq!(
        result_of(&events, &meeting),
        SummaryEvent::Failed {
            message: LIVE_LOAD_STALLED_MESSAGE.into(),
            retryable: false,
        }
    );
    assert!(
        events
            .iter()
            .any(|e| matches!(e, MeetingEvent::Warning { key, .. } if key == "live-load-stalled")),
        "{events:?}"
    );
    assert!(
        prepare.logged().is_empty(),
        "nothing ran after the recording"
    );
    assert_eq!(recordings_of(&svc, &meeting), 1, "the audio stays");
    activity_becomes(
        &svc,
        &meeting,
        earshot_runtime::MeetingActivity::Unprocessed,
    );
    drop(release);
}

/// T9.22: while the first meeting's thread is still stuck (its `release` held back), a second
/// meeting records, takes its turn and finishes: nothing the stuck thread holds keeps the queue.
/// `stuck` waits until the first meeting is inside its stuck call.
fn a_stuck_meeting_never_blocks_the_next(first: Finish, stuck: impl FnOnce(), expected: &str) {
    let dir = TempDir::new("stuck-first");
    let svc = services(&dir);
    let (open, gate) = mpsc::channel();
    let prepare = CancelPrepare::new(vec![first, Finish::Gate(gate)]);
    let (d, rx) = cancel_driver(Arc::clone(&prepare));
    let d = d.with_after_bounds(SHORT_AFTER);
    let first = d.start(&svc, request(CaptureMode::InRoom)).unwrap();
    stuck();
    d.stop().unwrap();
    let events = events_until(&rx, |e| is_summary_of(&first, e));
    assert_eq!(
        result_of(&events, &first),
        SummaryEvent::Failed {
            message: expected.into(),
            retryable: false,
        }
    );

    let second = d.start(&svc, request(CaptureMode::InRoom)).unwrap();
    d.stop().unwrap();
    open.send(()).unwrap();
    let events = events_until(&rx, |e| is_summary_of(&second, e));
    d.join();
    assert!(matches!(
        result_of(&events, &second),
        SummaryEvent::Skipped { .. }
    ));
    assert!(
        prepare.logged().ends_with(&[
            format!("{second} starts"),
            format!("{second} ends, cancelled false")
        ]),
        "{:?}",
        prepare.logged()
    );
    assert!(
        !prepare
            .logged()
            .iter()
            .any(|l| l.starts_with(&format!("{first} returned"))),
        "the first is still stuck"
    );
}

#[test]
fn meeting_with_a_final_pass_step_that_never_returns_never_blocks_the_next() {
    let (inside_tx, inside) = mpsc::channel();
    let (release, release_rx) = mpsc::channel::<()>();
    a_stuck_meeting_never_blocks_the_next(
        Finish::AfterHangs {
            in_step: true,
            inside: inside_tx,
            release: release_rx,
        },
        || {},
        POST_STALLED_MESSAGE,
    );
    // Its call came, and it is released only now.
    inside.recv_timeout(MEET_GUARD).unwrap();
    drop(release);
}

#[test]
fn meeting_with_a_final_pass_load_that_never_returns_never_blocks_the_next() {
    let (inside_tx, inside) = mpsc::channel();
    let (release, release_rx) = mpsc::channel::<()>();
    a_stuck_meeting_never_blocks_the_next(
        Finish::AfterHangs {
            in_step: false,
            inside: inside_tx,
            release: release_rx,
        },
        || {},
        POST_STALLED_MESSAGE,
    );
    inside.recv_timeout(MEET_GUARD).unwrap();
    drop(release);
}

#[test]
fn meeting_with_a_live_model_load_that_never_returns_never_blocks_the_next() {
    // The stuck hold thread holds the first meeting's ticket: only ending it explicitly lets the
    // second take its turn.
    let (loading_tx, loading) = mpsc::channel();
    let (release, release_rx) = mpsc::channel::<()>();
    a_stuck_meeting_never_blocks_the_next(
        Finish::LoadHangs {
            stall: Duration::from_millis(200),
            loading: loading_tx,
            release: release_rx,
        },
        || loading.recv_timeout(MEET_GUARD).unwrap(),
        LIVE_LOAD_STALLED_MESSAGE,
    );
    drop(release);
}

#[test]
fn meeting_after_watch_rule_is_no_sign_of_life_for_the_phase_bound() {
    // T9.22b, clock-free: the bound of the phase the work is in, counted from its last step or
    // token poll; the summary is watched elsewhere.
    let s = Duration::from_secs;
    let bounds = AfterBounds::default();
    assert_eq!((bounds.load, bounds.step), (s(600), s(120)));
    let mut w = AfterWatch::new(bounds);
    assert!(!w.stalled(AfterPhase::Loading, 0, 0, s(0), false));
    assert!(!w.stalled(AfterPhase::Loading, 0, 0, s(599), false));
    assert!(
        w.stalled(AfterPhase::Loading, 0, 0, s(600), false),
        "a load that never returns"
    );
    // A step starts the step bound afresh, whatever time it came at.
    assert!(!w.stalled(AfterPhase::Steps, 1, 0, s(700), false));
    assert!(!w.stalled(AfterPhase::Steps, 1, 0, s(819), false));
    assert!(
        w.stalled(AfterPhase::Steps, 1, 0, s(820), false),
        "a step that never returns"
    );
    // Any sign of life starts it again: a poll of the token, a step.
    let mut w = AfterWatch::new(bounds);
    for (k, (steps, polls)) in [(1, 0), (1, 1), (2, 1)].into_iter().enumerate() {
        let at = s(1000 * k as u64);
        assert!(!w.stalled(AfterPhase::Steps, steps, polls, at, false));
        assert!(!w.stalled(AfterPhase::Steps, steps, polls, at + s(119), false));
    }
    // A decode that polls slowly but steadily is never cut, however long it takes.
    let mut slow = AfterWatch::new(bounds);
    for minute in 0..600u64 {
        assert!(!slow.stalled(AfterPhase::Steps, 1, minute, s(60 * minute), false));
    }
    // Nor is a summary, whatever it does: its own watch decides.
    let mut summary = AfterWatch::new(bounds);
    assert!(!summary.stalled(AfterPhase::Summary, 3, 0, s(0), false));
    assert!(!summary.stalled(AfterPhase::Summary, 3, 0, s(86_400), false));
}

#[test]
fn meeting_after_watch_gives_cancelled_work_a_short_grace() {
    // Clock-free: once cancelled, work silent for the grace is given up, counted from the later
    // of its last sign of life and the moment the cancel was seen; a sign of life starts it again.
    let s = Duration::from_secs;
    let bounds = AfterBounds::default();
    assert_eq!(bounds.cancelled, s(30));
    for phase in [AfterPhase::Loading, AfterPhase::Steps] {
        let mut w = AfterWatch::new(bounds);
        assert!(!w.stalled(phase, 1, 0, s(0), false));
        // Silent for 50 s, then cancelled: the grace starts at the cancel, not at the silence.
        assert!(!w.stalled(phase, 1, 0, s(50), true));
        assert!(!w.stalled(phase, 1, 0, s(79), true));
        assert!(w.stalled(phase, 1, 0, s(80), true), "{phase:?}");
        // A poll after the cancel starts the grace afresh.
        let mut w = AfterWatch::new(bounds);
        assert!(!w.stalled(phase, 1, 0, s(0), true));
        assert!(!w.stalled(phase, 1, 1, s(20), true));
        assert!(!w.stalled(phase, 1, 1, s(49), true));
        assert!(w.stalled(phase, 1, 1, s(50), true), "{phase:?}");
    }
    // The summary keeps its own watch, cancelled or not.
    let mut summary = AfterWatch::new(bounds);
    assert!(!summary.stalled(AfterPhase::Summary, 3, 0, s(0), true));
    assert!(!summary.stalled(AfterPhase::Summary, 3, 0, s(86_400), true));
}

#[test]
fn meeting_cancel_on_after_work_that_stopped_answering_leaves_it_after_the_grace() {
    // A Cancel on a hung final-pass step does not wait for the step bound (an hour here).
    let dir = TempDir::new("after-cancel-grace");
    let svc = services(&dir);
    let (inside_tx, inside) = mpsc::channel();
    let (release, release_rx) = mpsc::channel();
    let prepare = CancelPrepare::new(vec![Finish::AfterHangs {
        in_step: true,
        inside: inside_tx,
        release: release_rx,
    }]);
    let (d, rx) = cancel_driver(Arc::clone(&prepare));
    let hour = Duration::from_secs(3600);
    let d = d.with_after_bounds(AfterBounds {
        load: hour,
        step: hour,
        cancelled: Duration::from_millis(200),
    });
    let meeting = d.start(&svc, request(CaptureMode::InRoom)).unwrap();
    d.stop().unwrap();
    inside.recv_timeout(MEET_GUARD).unwrap();
    d.cancel_finishing(&meeting);
    let events = events_until(&rx, |e| is_summary_of(&meeting, e));
    assert_eq!(
        result_of(&events, &meeting),
        SummaryEvent::Failed {
            message: POST_STALLED_MESSAGE.into(),
            retryable: false,
        }
    );
    d.join();
    assert_eq!(d.phase(), DriverPhase::Idle);
    release.send(()).unwrap();
    logged_eventually(&prepare, &format!("{meeting} returned, cancelled true"));
}

#[test]
fn meeting_after_phase_follows_the_host_steps() {
    let post = Ok(HostPost::Skipped);
    assert_eq!(
        AfterPhase::of(&HostStep::PostLoaded { missing: &[] }),
        AfterPhase::Loading
    );
    let starting = HostStep::PostStarting {
        fallbacks: &[],
        work: &[],
        asr_backend: BackendKind::Fake,
    };
    assert_eq!(AfterPhase::of(&starting), AfterPhase::Steps);
    assert_eq!(
        AfterPhase::of(&HostStep::Post(PostStage::EchoChecked {
            elapsed: Duration::ZERO
        })),
        AfterPhase::Steps
    );
    assert_eq!(
        AfterPhase::of(&HostStep::PostFinished(&post)),
        AfterPhase::Summary
    );
    assert_eq!(
        AfterPhase::of(&HostStep::SummaryStarting),
        AfterPhase::Summary
    );
}

#[test]
fn meeting_cancel_during_the_summary_is_a_cancelled_summary() {
    let dir = TempDir::new("cancel-summary");
    let svc = services(&dir);
    let (running_tx, running) = mpsc::channel();
    let prepare = CancelPrepare::new(vec![Finish::SummaryUntilCancelled(running_tx)]);
    let (d, rx) = cancel_driver(prepare);
    let meeting = d.start(&svc, request(CaptureMode::InRoom)).unwrap();
    d.stop().unwrap();
    running.recv_timeout(MEET_GUARD).unwrap();

    d.cancel_finishing(&meeting);
    let events = events_until(&rx, |e| is_summary_of(&meeting, e));
    d.join();
    assert_eq!(result_of(&events, &meeting), SummaryEvent::Cancelled);
}

#[test]
fn meeting_cancel_while_it_records_changes_nothing() {
    let dir = TempDir::new("cancel-recording");
    let svc = services(&dir);
    let (open, gate) = mpsc::channel();
    let prepare = CancelPrepare::new(vec![Finish::Gate(gate)]);
    let (d, rx) = cancel_driver(Arc::clone(&prepare));
    let meeting = d.start(&svc, request(CaptureMode::InRoom)).unwrap();

    d.cancel_finishing(&meeting);
    several_looks();
    assert_eq!(d.phase(), DriverPhase::Recording, "still recording");

    d.stop().unwrap();
    open.send(()).unwrap();
    let events = events_until(&rx, |e| is_summary_of(&meeting, e));
    d.join();
    assert!(
        matches!(result_of(&events, &meeting), SummaryEvent::Skipped { .. }),
        "its normal end"
    );
    assert_eq!(
        prepare.logged(),
        [
            format!("{meeting} starts"),
            format!("{meeting} ends, cancelled false")
        ],
        "the press was not kept for later"
    );
}

/// The older meeting is cancelled while a newer one records: the recording carries on, and the
/// newer meeting's turn comes because the ticket ended.
#[test]
fn meeting_cancel_leaves_a_newer_recording_alone_and_gives_it_its_turn() {
    let dir = TempDir::new("cancel-newer");
    let svc = services(&dir);
    let (running_tx, running) = mpsc::channel();
    let (open, gate) = mpsc::channel();
    let prepare = CancelPrepare::new(vec![
        Finish::PassUntilCancelled(running_tx),
        Finish::Gate(gate),
    ]);
    let (d, rx) = cancel_driver(Arc::clone(&prepare));
    let first = d.start(&svc, request(CaptureMode::InRoom)).unwrap();
    d.stop().unwrap();
    running.recv_timeout(MEET_GUARD).unwrap();
    events_until(&rx, |e| saved_is(&first, e));

    let second = d.start(&svc, request(CaptureMode::InRoom)).unwrap();
    events_until(&rx, |e| {
        *e == MeetingEvent::Transcript {
            state: crate::meeting_events::TranscriptState::WaitingForPreviousMeeting,
        }
    });

    d.cancel_finishing(&first);
    let events = events_until(&rx, |e| is_summary_of(&first, e));
    assert_eq!(result_of(&events, &first), SummaryEvent::PostCancelled);
    // The hold opened: the newer meeting no longer waits, and it still records.
    events_until(&rx, |e| {
        matches!(e, MeetingEvent::Transcript { state } if *state
            != crate::meeting_events::TranscriptState::WaitingForPreviousMeeting)
    });
    assert_eq!(d.phase(), DriverPhase::Recording);

    d.stop().unwrap();
    open.send(()).unwrap();
    let events = events_until(&rx, |e| is_summary_of(&second, e));
    d.join();
    assert!(
        matches!(result_of(&events, &second), SummaryEvent::Skipped { .. }),
        "its own end"
    );
    assert_eq!(recordings_of(&svc, &first), 1);
    assert_eq!(recordings_of(&svc, &second), 1);
    assert_eq!(
        prepare.logged(),
        [
            format!("{first} starts"),
            format!("{second} starts"),
            format!("{second} ends, cancelled false"),
        ]
    );
}

/// A meeting waiting behind an older one is cancelled: it runs nothing and reads as unprocessed at
/// once, and the older one, and a queued meeting behind it, go on.
#[test]
fn meeting_cancel_while_it_waits_for_its_turn_runs_nothing_and_touches_no_other() {
    let dir = TempDir::new("cancel-waiting");
    let svc = services(&dir);
    let (open_first, first_gate) = mpsc::channel();
    let (never_tx, _never) = mpsc::channel();
    let (open_third, third_gate) = mpsc::channel();
    let prepare = CancelPrepare::new(vec![
        Finish::Gate(first_gate),
        Finish::PassUntilCancelled(never_tx),
        Finish::Gate(third_gate),
    ]);
    let (d, rx) = cancel_driver(Arc::clone(&prepare));
    let first = d.start(&svc, request(CaptureMode::InRoom)).unwrap();
    d.stop().unwrap();
    events_until(&rx, |e| saved_is(&first, e));
    let deadline = Instant::now() + MEET_GUARD;
    while prepare.logged().is_empty() {
        assert!(Instant::now() < deadline, "the first never started");
        std::thread::sleep(Duration::from_millis(1));
    }
    let second = d.start(&svc, request(CaptureMode::InRoom)).unwrap();
    d.stop().unwrap();
    events_until(&rx, |e| saved_is(&second, e));
    activity_becomes(&svc, &second, earshot_runtime::MeetingActivity::Waiting);
    let third = d.start(&svc, request(CaptureMode::InRoom)).unwrap();
    d.stop().unwrap();
    events_until(&rx, |e| saved_is(&third, e));
    activity_becomes(&svc, &third, earshot_runtime::MeetingActivity::Waiting);

    d.cancel_finishing(&second);
    let events = events_until(&rx, |e| is_summary_of(&second, e));
    assert_eq!(result_of(&events, &second), SummaryEvent::PostCancelled);
    activity_becomes(&svc, &second, earshot_runtime::MeetingActivity::Unprocessed);
    assert_eq!(recordings_of(&svc, &second), 1);
    assert_eq!(
        prepare.logged(),
        [format!("{first} starts")],
        "the older one still runs, and the one behind still waits for it"
    );

    open_first.send(()).unwrap();
    events_until(&rx, |e| is_summary_of(&first, e));
    open_third.send(()).unwrap();
    events_until(&rx, |e| is_summary_of(&third, e));
    d.join();
    assert_eq!(
        prepare.logged(),
        [
            format!("{first} starts"),
            format!("{first} ends, cancelled false"),
            format!("{third} starts"),
            format!("{third} ends, cancelled false"),
        ]
    );
}

#[test]
fn meeting_cancel_twice_and_after_the_end_is_harmless_with_one_summary_event() {
    let dir = TempDir::new("cancel-twice");
    let svc = services(&dir);
    let (running_tx, running) = mpsc::channel();
    let prepare = CancelPrepare::new(vec![Finish::PassUntilCancelled(running_tx)]);
    let (d, rx) = cancel_driver(prepare);
    let meeting = d.start(&svc, request(CaptureMode::InRoom)).unwrap();
    d.stop().unwrap();
    running.recv_timeout(MEET_GUARD).unwrap();

    d.cancel_finishing(&meeting);
    d.cancel_finishing(&meeting);
    events_until(&rx, |e| is_summary_of(&meeting, e));
    d.cancel_finishing(&meeting);
    d.join();
    d.cancel_finishing(&meeting);
    d.cancel_finishing("not a meeting");
    assert!(rx.try_recv().is_err(), "exactly one summary event");
    assert_eq!(d.phase(), DriverPhase::Idle);
}

/// A meeting's token goes with its thread, also on a panic's way out.
#[test]
fn meeting_cancel_has_nothing_left_to_reach_after_a_panic() {
    let dir = TempDir::new("cancel-panic");
    let svc = services(&dir);
    let prepare = FakePrepare::new(Script::AfterPanics);
    let (d, rx) = driver(prepare);
    let meeting = d.start(&svc, request(CaptureMode::Call)).unwrap();
    until_summary(&rx);
    d.join();
    assert!(d.0.finishing.lock().unwrap().is_empty());
    d.cancel_finishing(&meeting);
}

#[test]
fn meeting_wait_until_gives_up_at_its_limit_on_a_clock_it_is_given() {
    // T9.19: clock-free. A recording that never saves: the wait ends at the limit, having
    // slept no longer than it.
    let start = Instant::now();
    let clock = std::cell::Cell::new(start);
    let slept = std::cell::Cell::new(Duration::ZERO);
    let limit = Duration::from_millis(4_000);
    let saved = wait_until(
        limit,
        || clock.get(),
        |d| {
            slept.set(slept.get() + d);
            clock.set(clock.get() + d);
        },
        || false,
    );
    assert!(!saved);
    assert_eq!(slept.get(), limit);
    // One that saves on the third look: true, after two polls.
    let looks = std::cell::Cell::new(0);
    clock.set(start);
    slept.set(Duration::ZERO);
    let saved = wait_until(
        limit,
        || clock.get(),
        |d| {
            slept.set(slept.get() + d);
            clock.set(clock.get() + d);
        },
        || {
            looks.set(looks.get() + 1);
            looks.get() == 3
        },
    );
    assert!(saved);
    assert_eq!(slept.get(), STOP_POLL * 2);
    // A check that comes true during the last sleep still counts.
    clock.set(start);
    let saved = wait_until(
        Duration::from_millis(120),
        || clock.get(),
        |d| clock.set(clock.get() + d),
        || clock.get() >= start + Duration::from_millis(120),
    );
    assert!(saved);
}

#[test]
fn meeting_driver_stop_and_wait_for_saves_the_recording_within_its_limit() {
    let dir = TempDir::new("driver-stop-limit");
    let svc = services(&dir);
    let (d, rx) = driver(FakePrepare::new(Script::UntilStopped));
    assert!(
        d.stop_and_wait_for(Duration::from_secs(4)),
        "nothing records"
    );
    d.start(&svc, request(CaptureMode::Call)).unwrap();
    assert_eq!(d.phase(), DriverPhase::Recording);
    // A wide bound: the replay stops at once; under load this only has to be under a minute.
    assert!(d.stop_and_wait_for(Duration::from_secs(60)));
    assert_ne!(d.phase(), DriverPhase::Recording);
    let events = until_summary(&rx);
    assert!(
        events
            .iter()
            .any(|e| matches!(e, MeetingEvent::Ended { .. })),
        "the recording ended"
    );
    d.join();
}
