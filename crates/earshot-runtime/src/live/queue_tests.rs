//! T9.1l: a recording starts at once and its live work takes its turn. Replays and fakes only.
//! Every name contains `meeting_queue`.

use std::sync::atomic::{AtomicBool, AtomicUsize};
use std::sync::{Mutex, mpsc};
use std::time::Instant;

use earshot_core::asr::{CancelToken, Word};
use earshot_core::capture::SourceStatus;
use earshot_core::model::ClockDomain;
use earshot_core::pipeline::{LiveTrack, PipelineEvent};
use earshot_llm_replay::mock::{MockResponse, MockServer};

use super::*;
use crate::MeetingActivity;
use crate::backends::{Accelerator, ModelDir};
use crate::host::{AfterMeeting, HostStep, SkipReason, after_meeting_with};
use crate::live::worker::LiveWorker;
use crate::meeting_queue::{MeetingHold, MeetingQueue};
use crate::speakers::StoredSpeakers;
use crate::stretches::{StoredStretches, stretch_record};
use crate::summary::{SummaryConfig, SummaryService};

const GUARD: Duration = Duration::from_secs(120);

/// Gates a test opens by hand.
#[derive(Default)]
pub(super) struct HandGates {
    speakers: AtomicBool,
    pub(super) transcription: AtomicBool,
    reason: Mutex<Option<HoldReason>>,
}

fn wait(open: &AtomicBool, cancel: &CancelToken) -> bool {
    let limit = Instant::now() + GUARD;
    while !open.load(Ordering::SeqCst) {
        if cancel.is_cancelled() {
            return false;
        }
        assert!(Instant::now() < limit, "a gate was never opened");
        std::thread::sleep(Duration::from_millis(1));
    }
    !cancel.is_cancelled()
}

impl HoldGates for HandGates {
    fn wait_speakers(&self, cancel: &CancelToken) -> bool {
        wait(&self.speakers, cancel)
    }
    fn wait_transcription(&self, cancel: &CancelToken) -> bool {
        wait(&self.transcription, cancel)
    }
    fn reason(&self) -> Option<HoldReason> {
        *self.reason.lock().unwrap()
    }
}

pub(super) fn held_gates() -> Arc<HandGates> {
    let gates = HandGates::default();
    *gates.reason.lock().unwrap() = Some(HoldReason::PreviousMeeting);
    Arc::new(gates)
}

/// A replay that keeps its end back until it is released or stopped: a meeting that has
/// delivered everything it will and goes on recording.
pub(super) struct EndWhenReleased<S> {
    inner: S,
    released: Arc<AtomicBool>,
    delivered: Arc<AtomicBool>,
    end: Option<EndOfStream>,
}

struct KeepEnd<'a> {
    sink: &'a mut dyn CaptureSink,
    end: &'a mut Option<EndOfStream>,
}

impl CaptureSink for KeepEnd<'_> {
    fn frames(&mut self, track: TrackId, frames: &[f32], device_ts: DeviceTime) {
        self.sink.frames(track, frames, device_ts);
    }
    fn discontinuity(&mut self, track: TrackId, discontinuity: Discontinuity) {
        self.sink.discontinuity(track, discontinuity);
    }
    fn end(&mut self, end: EndOfStream) {
        *self.end = Some(end);
    }
}

impl<S: AudioSource> AudioSource for EndWhenReleased<S> {
    type Error = S::Error;

    fn session(&self) -> &CaptureSession {
        self.inner.session()
    }

    fn pump(&mut self, sink: &mut dyn CaptureSink) -> Result<SourceStatus, Self::Error> {
        if self.end.is_none() {
            let mut keep = KeepEnd {
                sink,
                end: &mut self.end,
            };
            let status = self.inner.pump(&mut keep)?;
            if self.end.is_none() {
                return Ok(status);
            }
            self.delivered.store(true, Ordering::SeqCst);
        }
        if self.released.load(Ordering::SeqCst)
            && let Some(end) = self.end.take()
        {
            sink.end(end);
            return Ok(SourceStatus::Ended);
        }
        Ok(SourceStatus::Idle)
    }

    fn stop(&mut self) {
        self.released.store(true, Ordering::SeqCst);
        // Stopped by someone, not run out: what a real capture says.
        if let Some(end) = &mut self.end {
            end.reason = EndReason::Stopped;
        }
        self.inner.stop();
    }
}

/// A two-track scene: speech on both tracks.
pub(super) struct Scene {
    pub(super) mic: Vec<f32>,
    pub(super) them: Vec<f32>,
}

pub(super) fn scene_of(seconds: u64) -> Scene {
    let bursts = |prefix: &str, offset: u64| -> Vec<Word> {
        (0..seconds / 13)
            .flat_map(|k| words(prefix, offset + k * 13_000, 10))
            .collect()
    };
    Scene {
        mic: speech(&bursts("m", 1_000), seconds * 1000, 201),
        them: speech(&bursts("t", 7_000), seconds * 1000, 202),
    }
}

/// Short: the canceller is slow in a debug build.
fn scene() -> Scene {
    scene_of(14)
}

/// A mic-only source: the speaker tests need more audio than the canceller gets through quickly.
fn mic_source_of(
    scene: &Scene,
) -> (
    EndWhenReleased<earshot_core::capture::ReplaySource>,
    Handles,
) {
    let source = ReplayBuilder::new(ORIGIN, MonoBuffer::new(R16, scene.mic.clone()))
        .build()
        .unwrap();
    let (released, delivered) = (Arc::default(), Arc::default());
    (
        EndWhenReleased {
            inner: source,
            released: Arc::clone(&released),
            delivered: Arc::clone(&delivered),
            end: None,
        },
        Handles {
            released,
            delivered,
        },
    )
}

/// A transcriber whose words are a function of exactly the samples it was given: two runs store the
/// same words only if the same buffers were decoded. (The canceller and the drift resampler change
/// the samples, so a fake that looks a buffer up in the source audio would not find it.)
pub(super) struct ContentWords;

impl earshot_core::asr::Transcriber for ContentWords {
    fn transcribe(
        &mut self,
        audio: &MonoBuffer,
        _: &earshot_core::asr::TranscribeOptions,
        _: &CancelToken,
    ) -> Result<earshot_core::asr::Transcription, AsrError> {
        let mut hash = 0xcbf2_9ce4_8422_2325u64;
        for sample in &audio.samples {
            for byte in sample.to_bits().to_le_bytes() {
                hash = (hash ^ u64::from(byte)).wrapping_mul(0x0100_0000_01b3);
            }
        }
        let ms = audio.samples.len() as u64 / 16;
        Ok(earshot_core::asr::Transcription {
            segments: vec![RawSegment::from_words(vec![Word::new(
                format!("w{hash:016x}"),
                0,
                ms.max(1),
                0.9,
            )])],
            language: None,
            backend: self.backend_info(),
        })
    }

    fn backend_info(&self) -> earshot_core::asr::BackendInfo {
        earshot_core::asr::BackendInfo {
            kind: earshot_core::asr::BackendKind::Fake,
            device: "fake".to_owned(),
            model: "fake".to_owned(),
        }
    }
}

pub(super) struct Handles {
    pub(super) released: Arc<AtomicBool>,
    pub(super) delivered: Arc<AtomicBool>,
}

pub(super) fn source_of(
    scene: &Scene,
) -> (
    EndWhenReleased<earshot_core::capture::ReplaySource>,
    Handles,
) {
    let source = ReplayBuilder::new(ORIGIN, MonoBuffer::new(R16, scene.mic.clone()))
        .reference(MonoBuffer::new(R16, scene.them.clone()))
        .build()
        .unwrap();
    let (released, delivered) = (Arc::default(), Arc::default());
    (
        EndWhenReleased {
            inner: source,
            released: Arc::clone(&released),
            delivered: Arc::clone(&delivered),
            end: None,
        },
        Handles {
            released,
            delivered,
        },
    )
}

/// [`ContentWords`] on both tracks; `calls` counts the factory's calls.
pub(super) fn factory_of(_scene: &Scene, calls: Arc<AtomicUsize>) -> LiveAsrFactory {
    Box::new(move |track, entity| {
        calls.fetch_add(1, Ordering::SeqCst);
        // Segment ids apart per track: a row of the same id replaces the other's.
        let first = match track {
            TrackId::Mic => 0xB1,
            TrackId::Reference => 0xB2,
        };
        let mut n = 0u8;
        let segment_ids: earshot_core::asr::SegmentIds = Box::new(move || {
            n = n.wrapping_add(1);
            EntityId::from_bytes([first, n, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1])
        });
        Ok(Some(
            LiveAsr {
                transcriber: Box::new(ContentWords),
                vad: Box::new(EnergyVad::default()),
                config: ChunkerConfig::default(),
                track_entity: entity,
                segment_ids,
            }
            .into(),
        ))
    })
}

pub(super) fn held_config(gates: Arc<dyn HoldGates>) -> LiveConfig {
    LiveConfig {
        hold: Some(LiveHold::new(gates)),
        ..stretch_config(ClockDomain::Replay)
    }
}

/// `(track, text, start, end)` of each stored live word.
fn live_words(svc: &Services, meeting: EntityId) -> Vec<(EntityId, String, u64, u64)> {
    let store = svc.store();
    live_segments(&**store, meeting, TranscriptPass::Live)
        .unwrap()
        .into_iter()
        .flat_map(|r| {
            let track = r.segment.track;
            r.segment
                .words
                .into_iter()
                .map(move |w| (track, w.text, w.start.as_millis(), w.end.as_millis()))
        })
        .collect()
}

/// The words without their track row (two runs have different rows), by track kind.
pub(super) fn by_kind(svc: &Services, meeting: EntityId) -> Vec<(TrackId, String, u64, u64)> {
    let tracks = svc.store().tracks(meeting).unwrap();
    let mut out: Vec<_> = live_words(svc, meeting)
        .into_iter()
        .map(|(t, text, s, e)| {
            let kind = tracks.iter().find(|r| r.id == t).unwrap().kind;
            (kind, text, s, e)
        })
        .collect();
    out.sort_by(|a, b| (a.2, &a.1).cmp(&(b.2, &b.1)));
    out
}

/// Each recorded track's samples, mic first.
fn recorded_samples(svc: &Services, meeting: EntityId) -> Vec<(TrackId, Vec<f32>)> {
    let store = svc.store();
    let tracks = store.tracks(meeting).unwrap();
    let mut out: Vec<_> = store
        .audio_files(meeting)
        .unwrap()
        .into_iter()
        .map(|file| {
            let kind = tracks.iter().find(|t| t.id == file.track_id).unwrap().kind;
            let (rate, flac) = read_flac(&svc.data.resolve(&file.path).unwrap()).unwrap();
            assert_eq!(rate, 16_000);
            (kind, flac)
        })
        .collect();
    out.sort_by_key(|(kind, _)| *kind == TrackId::Reference);
    out
}

/// Both tracks are saved, each with all its audio.
fn assert_recorded_whole(svc: &Services, meeting: EntityId, scene: &Scene) {
    let recorded = recorded_samples(svc, meeting);
    assert_eq!(recorded.len(), 2, "both tracks");
    assert_eq!(recorded[0].0, TrackId::Mic);
    assert_eq!(recorded[0].1.len(), scene.mic.len());
    assert_eq!(recorded[1].1.len(), scene.them.len());
}

fn until(events: &Receiver<RuntimeEvent>, what: &str, pred: impl Fn(&RuntimeEvent) -> bool) {
    let limit = Instant::now() + GUARD;
    loop {
        assert!(Instant::now() < limit, "no {what}");
        if let Ok(e) = events.recv_timeout(Duration::from_secs(10))
            && pred(&e)
        {
            return;
        }
    }
}

fn wait_for(flag: &AtomicBool) {
    let limit = Instant::now() + GUARD;
    while !flag.load(Ordering::SeqCst) {
        assert!(Instant::now() < limit, "never set");
        std::thread::sleep(Duration::from_millis(1));
    }
}

/// What a run with no hold stores: its words and its recording.
struct Baseline {
    words: Vec<(TrackId, String, u64, u64)>,
    samples: Vec<(TrackId, Vec<f32>)>,
    outcome: LiveOutcome,
}

fn no_hold_run(scene: &Scene) -> Baseline {
    let dir = TempDir::new();
    let svc = services(&dir);
    let (source, handles) = source_of(scene);
    handles.released.store(true, Ordering::SeqCst);
    let session = start_live(
        source,
        svc.clone(),
        stretch_config(ClockDomain::Replay),
        Some(factory_of(scene, Arc::default())),
        Box::new(ChaCha8Rng::seed_from_u64(210)),
    )
    .unwrap();
    let meeting = session.meeting();
    let outcome = session.wait().unwrap();
    Baseline {
        words: by_kind(&svc, meeting),
        samples: recorded_samples(&svc, meeting),
        outcome,
    }
}

fn transcript_states(events: &[RuntimeEvent]) -> Vec<LiveTranscriptState> {
    events
        .iter()
        .filter_map(|e| match e {
            RuntimeEvent::LiveTranscript(s) => Some(*s),
            _ => None,
        })
        .collect()
}

fn reply(title: &str) -> MockResponse {
    let summary = earshot_core::summary::SummaryV1 {
        title: title.into(),
        tldr: "The team agreed to ship on Friday.".into(),
        enhanced_notes: vec![],
        decisions: vec![],
        action_items: vec![],
        open_questions: vec![],
        topics: vec!["release".into()],
    };
    MockResponse::completion(&serde_json::to_string(&summary).unwrap(), "stop")
}

/// The meeting of the acceptance: a replay starts a second meeting while a slow fake summary runs
/// and the older summary finishes; the second meeting's FLAC holds all its audio from the start and
/// its live words match a run with no hold.
#[test]
fn meeting_queue_a_second_meeting_records_while_the_older_summary_runs() {
    let dir = TempDir::new();
    let svc = services(&dir);
    let queue = MeetingQueue::new();
    let first_ticket = queue.enter();
    let first = crate::host::tests::recorded(&svc, 220);
    let scene = Arc::new(scene_of(27));
    let baseline = no_hold_run(&scene);
    for kind in [TrackId::Mic, TrackId::Reference] {
        assert!(
            baseline.words.iter().any(|w| w.0 == kind),
            "{kind:?}: {:?}",
            baseline.words
        );
    }

    // The older meeting's summary is slow: the second meeting starts inside its request, records
    // everything it will, and the request answers only then.
    let calls = Arc::new(AtomicUsize::new(0));
    let second: Arc<Mutex<Option<(LiveSession, Handles)>>> = Arc::default();
    let summary_finished = Arc::new(AtomicBool::new(false));
    let server = {
        let (svc, queue, scene, calls, second, finished) = (
            svc.clone(),
            queue.clone(),
            Arc::clone(&scene),
            Arc::clone(&calls),
            Arc::clone(&second),
            Arc::clone(&summary_finished),
        );
        MockServer::start_with(move |_| {
            let mut slot = second.lock().unwrap();
            if slot.is_none() {
                let (source, handles) = source_of(&scene);
                let hold = MeetingHold::new(Arc::new(queue.enter()));
                let reason = hold.reason();
                assert_eq!(reason, Some(HoldReason::PreviousMeeting));
                let session = start_live(
                    source,
                    svc.clone(),
                    held_config(Arc::new(hold)),
                    Some(factory_of(&scene, Arc::clone(&calls))),
                    Box::new(ChaCha8Rng::seed_from_u64(221)),
                )
                .unwrap();
                wait_for(&handles.delivered);
                assert_eq!(calls.load(Ordering::SeqCst), 0, "no model during the hold");
                assert_eq!(
                    svc.meeting_activity(session.meeting()).unwrap(),
                    Some(MeetingActivity::Recording)
                );
                *slot = Some((session, handles));
            }
            assert!(!finished.load(Ordering::SeqCst));
            reply("The older meeting")
        })
        .unwrap()
    };
    let service = SummaryService::new(
        &SummaryConfig::new(server.endpoint().as_str(), "test-model"),
        svc.clock.clone(),
    )
    .unwrap();
    let models = ModelDir::new(dir.0.join("models"));
    let (a, b) = (CancelToken::new(), CancelToken::new());
    let result = after_meeting_with(
        &svc,
        &first,
        &AfterMeeting {
            models: &models,
            model: "base.en",
            order: &[Accelerator::Cpu],
            post: true,
            whole_pass: false,
            summary: Some(&service),
            no_summary: SkipReason::NotRequested,
            post_cancel: &a,
            summary_cancel: &b,
        },
        crate::host::tests::fake_post,
        &mut |step: HostStep<'_>| {
            if matches!(step, HostStep::PostFinished(_)) {
                first_ticket.final_pass_done();
            }
        },
    );
    assert!(
        matches!(result.summary, crate::host::SummaryResult::Stored { .. }),
        "{:?}",
        result.summary
    );
    summary_finished.store(true, Ordering::SeqCst);
    // Still held: the older meeting's job has not ended.
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    drop(first_ticket);

    let (mut session, handles) = second.lock().unwrap().take().unwrap();
    let meeting = session.meeting();
    let events = session.take_events().unwrap();
    let limit = Instant::now() + GUARD;
    while calls.load(Ordering::SeqCst) == 0 {
        assert!(Instant::now() < limit, "its turn never came");
        std::thread::sleep(Duration::from_millis(1));
    }
    handles.released.store(true, Ordering::SeqCst);
    let outcome = session.wait().unwrap();
    let events: Vec<RuntimeEvent> = events.try_iter().collect();

    assert_eq!(
        by_kind(&svc, meeting),
        baseline.words,
        "the same live words"
    );
    assert_recorded_whole(&svc, meeting, &scene);
    assert!(
        recorded_samples(&svc, meeting) == baseline.samples,
        "the same recording, sample for sample"
    );
    for (held, plain) in outcome
        .final_pass
        .tracks
        .iter()
        .zip(&baseline.outcome.final_pass.tracks)
    {
        assert_eq!(held.options, plain.options);
        assert!(held.track_entity.is_some());
    }
    let states = transcript_states(&events);
    assert_eq!(
        states.first(),
        Some(&LiveTranscriptState::Held(HoldReason::PreviousMeeting))
    );
    assert!(
        states.contains(&LiveTranscriptState::CatchingUp),
        "{states:?}"
    );
    // The pieces are on record for the final pass, as in a run with no hold.
    for track in [TrackId::Mic, TrackId::Reference] {
        let record = stretch_record(&svc.store(), meeting, track).unwrap();
        let Some(StoredStretches::Record(record)) = record else {
            panic!("{track:?} has no stretch record");
        };
        assert_eq!(record.decoded.through, scene.mic.len(), "{track:?}");
    }
}

#[test]
fn meeting_queue_events_say_held_catching_up_and_up_to_date_in_order() {
    let dir = TempDir::new();
    let svc = services(&dir);
    let scene = scene();
    let gates = held_gates();
    let (source, handles) = source_of(&scene);
    let mut session = start_live(
        source,
        svc.clone(),
        held_config(gates.clone()),
        Some(factory_of(&scene, Arc::default())),
        Box::new(ChaCha8Rng::seed_from_u64(230)),
    )
    .unwrap();
    let events = session.take_events().unwrap();
    until(&events, "Started", |e| {
        matches!(e, RuntimeEvent::Started { .. })
    });
    until(&events, "the held state", |e| {
        *e == RuntimeEvent::LiveTranscript(LiveTranscriptState::Held(HoldReason::PreviousMeeting))
    });
    wait_for(&handles.delivered);
    gates.transcription.store(true, Ordering::SeqCst);
    until(&events, "the catch-up", |e| {
        *e == RuntimeEvent::LiveTranscript(LiveTranscriptState::CatchingUp)
    });
    until(&events, "the end of the catch-up", |e| {
        *e == RuntimeEvent::LiveTranscript(LiveTranscriptState::UpToDate)
    });
    handles.released.store(true, Ordering::SeqCst);
    session.wait().unwrap();
}

/// No older meeting: the load itself may take seconds and no banner is shown for it.
#[test]
fn meeting_queue_no_older_meeting_says_nothing_about_a_hold() {
    let dir = TempDir::new();
    let svc = services(&dir);
    let scene = scene();
    let gates = Arc::new(HandGates::default());
    gates.speakers.store(true, Ordering::SeqCst);
    gates.transcription.store(true, Ordering::SeqCst);
    let (source, handles) = source_of(&scene);
    handles.released.store(true, Ordering::SeqCst);
    let (events, _) = collect(
        start_live(
            source,
            svc.clone(),
            held_config(gates),
            Some(factory_of(&scene, Arc::default())),
            Box::new(ChaCha8Rng::seed_from_u64(231)),
        )
        .unwrap(),
    );
    assert!(
        !transcript_states(&events)
            .iter()
            .any(|s| matches!(s, LiveTranscriptState::Held(_))),
        "{:?}",
        transcript_states(&events)
    );
}

#[test]
fn meeting_queue_passing_the_backlog_cap_stops_the_live_transcript_and_keeps_the_audio() {
    let dir = TempDir::new();
    let svc = services(&dir);
    let scene = scene();
    let gates = held_gates();
    let calls = Arc::new(AtomicUsize::new(0));
    let mut cfg = held_config(gates.clone());
    if let Some(hold) = &mut cfg.hold {
        hold.max_held = Duration::from_secs(5);
    }
    let (source, handles) = source_of(&scene);
    let mut session = start_live(
        source,
        svc.clone(),
        cfg,
        Some(factory_of(&scene, Arc::clone(&calls))),
        Box::new(ChaCha8Rng::seed_from_u64(232)),
    )
    .unwrap();
    let meeting = session.meeting();
    let events = session.take_events().unwrap();
    until(&events, "the stop", |e| {
        *e == RuntimeEvent::LiveTranscript(LiveTranscriptState::Stopped)
    });
    wait_for(&handles.delivered);
    // Its turn comes too late: nothing loads.
    gates.transcription.store(true, Ordering::SeqCst);
    handles.released.store(true, Ordering::SeqCst);
    let outcome = session.wait().unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert_recorded_whole(&svc, meeting, &scene);
    assert!(live_words(&svc, meeting).is_empty());
    // No record, so the final pass transcribes the whole meeting, as the stretches would have.
    for t in &outcome.final_pass.tracks {
        assert!(
            stretch_record(&svc.store(), meeting, t.track)
                .unwrap()
                .is_none()
        );
        assert_eq!(
            t.options,
            ChunkerConfig::default().options,
            "the stretches' own decoding"
        );
    }
    assert_eq!(outcome.audio_files.len(), 2);
    assert!(outcome.problems.is_empty(), "{:?}", outcome.problems);
    // The state stays stopped, whatever else happens.
    assert!(events.try_iter().all(|e| !matches!(
        e,
        RuntimeEvent::LiveTranscript(
            LiveTranscriptState::CatchingUp | LiveTranscriptState::UpToDate
        )
    )));
}

/// The worker's own cap, after the hold: a queue past it stops the track and says so.
#[test]
fn meeting_queue_a_worker_past_its_cap_stops_the_live_transcript() {
    let dir = TempDir::new();
    let svc = services(&dir);
    let scene = scene();
    let gates = held_gates();
    let calls = Arc::new(AtomicUsize::new(0));
    let mut cfg = LiveConfig {
        transcription: LiveTranscription::Chunker,
        ..held_config(gates.clone())
    };
    cfg.pipeline.max_asr_backlog = Duration::from_secs(1);
    let (source, handles) = source_of(&scene);
    let session = start_live(
        source,
        svc.clone(),
        cfg,
        Some(factory_of(&scene, Arc::clone(&calls))),
        Box::new(ChaCha8Rng::seed_from_u64(233)),
    )
    .unwrap();
    let meeting = session.meeting();
    // Everything is queued, far past the worker's 1 s, when its turn comes.
    wait_for(&handles.delivered);
    gates.transcription.store(true, Ordering::SeqCst);
    let limit = Instant::now() + GUARD;
    while calls.load(Ordering::SeqCst) == 0 {
        assert!(Instant::now() < limit, "its turn never came");
        std::thread::sleep(Duration::from_millis(1));
    }
    handles.released.store(true, Ordering::SeqCst);
    let (events, outcome) = collect(session);
    assert_eq!(outcome.audio_files.len(), 2);
    assert_recorded_whole(&svc, meeting, &scene);
    assert_eq!(
        transcript_states(&events).last(),
        Some(&LiveTranscriptState::Stopped),
        "{:?}",
        transcript_states(&events)
    );
}

#[test]
fn meeting_queue_a_failed_model_load_during_a_hold_costs_no_audio() {
    let dir = TempDir::new();
    let svc = services(&dir);
    let scene = scene();
    let gates = held_gates();
    let (source, handles) = source_of(&scene);
    let factory: LiveAsrFactory = Box::new(|_, _| Err(RuntimeError::Internal("model missing")));
    let mut session = start_live(
        source,
        svc.clone(),
        held_config(gates.clone()),
        Some(factory),
        Box::new(ChaCha8Rng::seed_from_u64(234)),
    )
    .unwrap();
    let meeting = session.meeting();
    let events = session.take_events().unwrap();
    wait_for(&handles.delivered);
    gates.transcription.store(true, Ordering::SeqCst);
    until(&events, "the stop", |e| {
        *e == RuntimeEvent::LiveTranscript(LiveTranscriptState::Stopped)
    });
    handles.released.store(true, Ordering::SeqCst);
    let outcome = session.wait().unwrap();
    assert_recorded_whole(&svc, meeting, &scene);
    assert_eq!(outcome.reason, EndReason::Finished);
    assert!(outcome.problems.is_empty(), "{:?}", outcome.problems);
    let db = svc.store();
    let m = db.meeting(meeting).unwrap().unwrap();
    assert!(!m.meta.deleted, "a failed load never withdraws the meeting");
    assert_eq!(m.duration_ms, Some(scene.mic.len() as u64 / 16));
}

#[test]
fn meeting_queue_the_load_failure_is_a_warning_with_the_reason() {
    let dir = TempDir::new();
    let svc = services(&dir);
    let scene = scene();
    let gates = held_gates();
    gates.transcription.store(true, Ordering::SeqCst);
    let (source, handles) = source_of(&scene);
    handles.released.store(true, Ordering::SeqCst);
    let factory: LiveAsrFactory = Box::new(|_, _| Err(RuntimeError::Internal("model missing")));
    // It may come before or after the replay's end; the replay must not outrun it here.
    handles.released.store(false, Ordering::SeqCst);
    let mut session = start_live(
        source,
        svc.clone(),
        held_config(gates),
        Some(factory),
        Box::new(ChaCha8Rng::seed_from_u64(235)),
    )
    .unwrap();
    let events = session.take_events().unwrap();
    until(&events, "the warning", |e| {
        matches!(
            e,
            RuntimeEvent::Warning(RuntimeWarning::LiveLoadFailed { error }) if error.contains("model missing")
        )
    });
    handles.released.store(true, Ordering::SeqCst);
    session.wait().unwrap();
}

/// Quitting during a hold: the recording is saved at once, with the gates still closed, and with
/// no session left the meeting reads as recorded with only the live draft.
#[test]
fn meeting_queue_quitting_during_a_hold_keeps_the_audio_and_leaves_the_meeting_unprocessed() {
    let dir = TempDir::new();
    let svc = services(&dir);
    let scene = scene();
    let gates = held_gates();
    let calls = Arc::new(AtomicUsize::new(0));
    let (source, handles) = source_of(&scene);
    let mut session = start_live(
        source,
        svc.clone(),
        held_config(gates),
        Some(factory_of(&scene, Arc::clone(&calls))),
        Box::new(ChaCha8Rng::seed_from_u64(236)),
    )
    .unwrap();
    let meeting = session.meeting();
    let events = session.take_events().unwrap();
    wait_for(&handles.delivered);
    session.request_stop();
    until(&events, "the saved recording", |e| {
        matches!(
            e,
            RuntimeEvent::RecordingSaved {
                reason: EndReason::Stopped,
                ..
            }
        )
    });
    // Saved by now: this is what a process that exits here leaves.
    assert_recorded_whole(&svc, meeting, &scene);
    assert_eq!(
        svc.store().meeting(meeting).unwrap().unwrap().duration_ms,
        Some(scene.mic.len() as u64 / 16)
    );
    let outcome = session.wait().unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 0, "the hold was abandoned");
    assert_eq!(outcome.audio_files.len(), 2);
    assert_eq!(
        svc.meeting_activity(meeting).unwrap(),
        Some(MeetingActivity::Unprocessed)
    );
}

/// A factory whose model load never returns until the test releases it, polling nothing; says
/// when it is loading (a rendezvous, so the stop comes during the load).
fn load_that_hangs(loading: Arc<AtomicBool>, release: mpsc::Receiver<()>) -> LiveAsrFactory {
    let release = Mutex::new(release);
    Box::new(move |_, _| {
        loading.store(true, Ordering::SeqCst);
        let _ = release.lock().unwrap().recv();
        Err(RuntimeError::Internal("released"))
    })
}

/// A held session stopped while its live model load hangs, its hold's load bound `stall`.
fn stop_during_a_hung_load(
    svc: &Services,
    stall: Duration,
    seed: u64,
) -> (LiveSession, Receiver<RuntimeEvent>, mpsc::Sender<()>) {
    let gates = held_gates();
    gates.transcription.store(true, Ordering::SeqCst);
    let (source, handles) = source_of(&scene());
    let loading = Arc::new(AtomicBool::new(false));
    let (release, released) = mpsc::channel();
    let mut config = held_config(gates);
    if let Some(hold) = &mut config.hold {
        hold.load_stall = stall;
    }
    let mut session = start_live(
        source,
        svc.clone(),
        config,
        Some(load_that_hangs(Arc::clone(&loading), released)),
        Box::new(ChaCha8Rng::seed_from_u64(seed)),
    )
    .unwrap();
    let events = session.take_events().unwrap();
    wait_for(&handles.delivered);
    wait_for(&loading);
    session.request_stop();
    (session, events, release)
}

/// T9.22b: a live model load that never returns cannot keep the session from ending: after the
/// hold's load bound with nothing from the thread, the thread is left and the session ends with
/// its recording saved and the stall in its warnings and outcome.
#[test]
fn meeting_queue_a_live_model_load_that_never_returns_is_left_at_the_end() {
    let dir = TempDir::new();
    let svc = services(&dir);
    let (session, events, release) = stop_during_a_hung_load(&svc, Duration::from_millis(200), 237);
    let meeting = session.meeting();
    let outcome = session.wait().unwrap();
    assert_recorded_whole(&svc, meeting, &scene());
    assert_eq!(outcome.problems, [RuntimeWarning::LiveLoadStalled]);
    let seen: Vec<RuntimeEvent> = events.try_iter().collect();
    let saved = seen
        .iter()
        .position(|e| matches!(e, RuntimeEvent::RecordingSaved { .. }))
        .expect("saved");
    let warned = seen
        .iter()
        .position(|e| *e == RuntimeEvent::Warning(RuntimeWarning::LiveLoadStalled))
        .expect("warned");
    assert!(saved < warned, "saved before the wait: {seen:?}");
    assert!(matches!(seen.last(), Some(RuntimeEvent::Ended(_))));
    // The load finally returning, long after, changes nothing.
    drop(release);
}

/// T9.22b: the catch-up's cancel ends the wait for a load in progress at once, whatever the bound,
/// and a cancel is the user's, never a stall.
#[test]
fn meeting_queue_a_cancelled_catch_up_leaves_a_load_in_progress_unreported() {
    let dir = TempDir::new();
    let svc = services(&dir);
    let (session, events, release) = stop_during_a_hung_load(&svc, Duration::from_secs(3600), 238);
    until(&events, "the saved recording", |e| {
        matches!(e, RuntimeEvent::RecordingSaved { .. })
    });
    session.cancel_catch_up();
    let outcome = session.wait().unwrap();
    assert!(outcome.problems.is_empty(), "{:?}", outcome.problems);
    drop(release);
}

/// A slow load that returns within the bound is waited for and joined, never reported.
#[test]
fn meeting_queue_a_slow_live_model_load_is_waited_for() {
    let dir = TempDir::new();
    let svc = services(&dir);
    let (session, events, release) = stop_during_a_hung_load(&svc, Duration::from_secs(3600), 239);
    until(&events, "the saved recording", |e| {
        matches!(e, RuntimeEvent::RecordingSaved { .. })
    });
    release.send(()).unwrap();
    let outcome = session.wait().unwrap();
    assert!(outcome.problems.is_empty(), "{:?}", outcome.problems);
}

/// A backlog never delays saving: the decode is held back past the recording's save.
#[test]
fn meeting_queue_the_recording_is_saved_before_a_backlog_is_decoded() {
    let dir = TempDir::new();
    let svc = services(&dir);
    let w = words("a", 1000, 10);
    let audio = speech(&w, 8000, 240);
    let gates = held_gates();
    gates.transcription.store(true, Ordering::SeqCst);
    let decode = Arc::new(AtomicBool::new(false));
    let source = ReplayBuilder::new(ORIGIN, MonoBuffer::new(R16, audio.clone()))
        .build()
        .unwrap();
    let mut session = start_live(
        source,
        svc.clone(),
        held_config(gates),
        Some(slow_asr_gated(
            audio.clone(),
            w.clone(),
            Duration::ZERO,
            Some(Arc::clone(&decode)),
        )),
        Box::new(ChaCha8Rng::seed_from_u64(241)),
    )
    .unwrap();
    let meeting = session.meeting();
    let events = session.take_events().unwrap();
    until(&events, "the saved recording", |e| {
        matches!(e, RuntimeEvent::RecordingSaved { .. })
    });
    let files = svc.store().audio_files(meeting).unwrap();
    assert_eq!(files.len(), 1, "saved while the decode waits");
    decode.store(true, Ordering::SeqCst);
    session.wait().unwrap();
}

/// A second speaker factory call per track: the fake models of the post tests.
fn speaker_factory(loaded: Arc<AtomicUsize>, windows: Arc<AtomicUsize>) -> HeldSpeakerFactory {
    Arc::new(move |_track| {
        loaded.fetch_add(1, Ordering::SeqCst);
        let (seg, emb) = crate::post::reuse_tests::speaker_models("seg@1", "emb@1", &windows);
        Ok(Some(LiveSpeakers {
            models: Box::new(move || {
                Ok(earshot_core::diar::Diarizer::new(
                    seg,
                    emb,
                    earshot_core::diar::DiarizationConfig::default(),
                )
                .unwrap())
            }),
            threads: Some(2),
            progress: None,
            background: false,
        }))
    })
}

/// Speaker work starts when older final passes are over, before its transcription's turn, and
/// computes its windows from the start of the track.
#[test]
fn meeting_queue_speaker_work_starts_before_the_turn_and_from_the_start_of_the_track() {
    let dir = TempDir::new();
    let svc = services(&dir);
    // A batch of speaker windows needs about 73 s of audio.
    let scene = scene_of(90);
    let (source, handles) = mic_source_of(&scene);
    let gates = held_gates();
    let (loaded, windows) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
    let calls = Arc::new(AtomicUsize::new(0));
    let mut cfg = held_config(gates.clone());
    if let Some(hold) = &mut cfg.hold {
        hold.speakers = Some(speaker_factory(Arc::clone(&loaded), Arc::clone(&windows)));
    }
    let session = start_live(
        source,
        svc.clone(),
        cfg,
        Some(factory_of(&scene, Arc::clone(&calls))),
        Box::new(ChaCha8Rng::seed_from_u64(250)),
    )
    .unwrap();
    let meeting = session.meeting();
    wait_for(&handles.delivered);
    assert_eq!(loaded.load(Ordering::SeqCst), 0, "an older final pass runs");
    gates.speakers.store(true, Ordering::SeqCst);
    let limit = Instant::now() + GUARD;
    while windows.load(Ordering::SeqCst) == 0 {
        assert!(Instant::now() < limit, "speaker work never ran");
        std::thread::sleep(Duration::from_millis(1));
    }
    assert_eq!(calls.load(Ordering::SeqCst), 0, "transcription still waits");
    gates.transcription.store(true, Ordering::SeqCst);
    while calls.load(Ordering::SeqCst) == 0 {
        assert!(Instant::now() < limit, "its turn never came");
        std::thread::sleep(Duration::from_millis(1));
    }
    handles.released.store(true, Ordering::SeqCst);
    session.wait().unwrap();
    assert_eq!(loaded.load(Ordering::SeqCst), 1, "the mic");
    // Whatever finished is recorded with no reason to stop, from frame 0.
    match crate::speakers::speaker_record(&svc.store(), meeting, TrackId::Mic).unwrap() {
        Some(StoredSpeakers::Record(r)) => {
            assert_eq!(r.header.stopped, None);
            assert_eq!(r.header.threads, Some(2));
        }
        other => panic!("{other:?}"),
    }
}

/// Past the buffer the speaker work stops, says why and leaves a reason-only record; the audio and
/// the transcription do not care.
#[test]
fn meeting_queue_passing_the_speaker_buffer_stops_only_speaker_work() {
    let dir = TempDir::new();
    let svc = services(&dir);
    let scene = scene();
    let gates = held_gates();
    let loaded = Arc::new(AtomicUsize::new(0));
    let mut cfg = held_config(gates.clone());
    if let Some(hold) = &mut cfg.hold {
        hold.max_speaker_buffer = Duration::from_secs(5);
        hold.speakers = Some(speaker_factory(Arc::clone(&loaded), Arc::default()));
    }
    let (source, handles) = source_of(&scene);
    let mut session = start_live(
        source,
        svc.clone(),
        cfg,
        Some(factory_of(&scene, Arc::default())),
        Box::new(ChaCha8Rng::seed_from_u64(251)),
    )
    .unwrap();
    let meeting = session.meeting();
    let events = session.take_events().unwrap();
    until(
        &events,
        "the reason",
        |e| matches!(e, RuntimeEvent::LiveSpeakersOff { reason } if reason.contains("held")),
    );
    wait_for(&handles.delivered);
    gates.transcription.store(true, Ordering::SeqCst);
    handles.released.store(true, Ordering::SeqCst);
    session.wait().unwrap();
    assert_eq!(loaded.load(Ordering::SeqCst), 0);
    assert_recorded_whole(&svc, meeting, &scene);
    assert!(!live_words(&svc, meeting).is_empty(), "transcription ran");
    for track in [TrackId::Mic, TrackId::Reference] {
        assert!(matches!(
            crate::speakers::speaker_record(&svc.store(), meeting, track).unwrap(),
            Some(StoredSpeakers::Off(_))
        ));
    }
}

#[test]
fn meeting_queue_live_summaries_wait_for_the_transcript() {
    use crate::live_summaries::{LiveSummaries, MapChunk};
    /// Counts the windows it is asked to map.
    struct Counting(Arc<AtomicUsize>);
    impl MapChunk for Counting {
        fn model_id(&self) -> &str {
            "m"
        }
        fn seconds(&self) -> u32 {
            10
        }
        fn map(
            &self,
            _: &SummaryInput,
            _: &[std::ops::Range<usize>],
            _: &CancelToken,
        ) -> Result<earshot_core::summary::SummaryV1, String> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Err("refused".into())
        }
    }
    let dir = TempDir::new();
    let svc = services(&dir);
    // Windows of 10 s to settle.
    let scene = scene_of(27);
    let gates = held_gates();
    let maps = Arc::new(AtomicUsize::new(0));
    let cfg = LiveConfig {
        live_summaries: Some(LiveSummaries {
            mapper: Arc::new(Counting(Arc::clone(&maps))),
            background: false,
        }),
        ..held_config(gates.clone())
    };
    let (source, handles) = source_of(&scene);
    let session = start_live(
        source,
        svc.clone(),
        cfg,
        Some(factory_of(&scene, Arc::default())),
        Box::new(ChaCha8Rng::seed_from_u64(252)),
    )
    .unwrap();
    wait_for(&handles.delivered);
    assert_eq!(maps.load(Ordering::SeqCst), 0, "no window during the hold");
    gates.transcription.store(true, Ordering::SeqCst);
    let limit = Instant::now() + GUARD;
    while maps.load(Ordering::SeqCst) == 0 {
        assert!(Instant::now() < limit, "no window after the hold");
        std::thread::sleep(Duration::from_millis(1));
    }
    handles.released.store(true, Ordering::SeqCst);
    session.wait().unwrap();
}

/// A [`ContentWords`] that counts its calls and blocks the second until released.
struct SecondCallWaits {
    calls: Arc<AtomicUsize>,
    release: Arc<AtomicBool>,
}

impl earshot_core::asr::Transcriber for SecondCallWaits {
    fn transcribe(
        &mut self,
        audio: &MonoBuffer,
        options: &earshot_core::asr::TranscribeOptions,
        cancel: &CancelToken,
    ) -> Result<earshot_core::asr::Transcription, AsrError> {
        if self.calls.fetch_add(1, Ordering::SeqCst) == 1 {
            wait_for(&self.release);
        }
        ContentWords.transcribe(audio, options, cancel)
    }

    fn backend_info(&self) -> earshot_core::asr::BackendInfo {
        ContentWords.backend_info()
    }
}

fn stretch_track(transcriber: Box<dyn earshot_core::asr::Transcriber + Send>) -> LiveTrack {
    let LiveTranscription::Stretches(pieces) = stretches() else {
        panic!("stretches");
    };
    LiveTrack::with_stretches(
        TrackId::Mic,
        LiveAsr {
            transcriber,
            vad: Box::new(EnergyVad::default()),
            config: ChunkerConfig::default(),
            track_entity: EntityId::from_bytes([4; 16]),
            segment_ids: ids(),
        },
        &pieces,
    )
    .unwrap()
}

/// A backlog reaches the transcript as it is decoded: with the second piece's decode blocked, the
/// first piece's words are already out. Taken whole, they would wait for the second.
#[test]
fn meeting_queue_a_stretch_worker_shows_a_backlog_as_it_decodes_it() {
    let at = |s: u64| words("w", s * 1000, 5);
    let spoken: Vec<Word> = [1, 41, 81].iter().flat_map(|s| at(*s)).collect();
    let audio = speech(&spoken, 100_000, 300);
    let (calls, release) = (
        Arc::new(AtomicUsize::new(0)),
        Arc::new(AtomicBool::new(false)),
    );
    let worker = LiveWorker::spawn(
        TrackId::Mic,
        stretch_track(Box::new(SecondCallWaits {
            calls: Arc::clone(&calls),
            release: Arc::clone(&release),
        })),
        earshot_core::pipeline::PartialPolicy::Coalesce,
        u64::MAX,
    )
    .unwrap();
    // As a held session hands it over: the packets it queued.
    for packet in audio.chunks(1_600) {
        worker.send_owned(packet.to_vec());
    }
    let limit = Instant::now() + GUARD;
    let mut shown = Vec::new();
    while shown.is_empty() {
        assert!(Instant::now() < limit, "nothing shown during the backlog");
        shown.extend(worker.drain().0);
        std::thread::sleep(Duration::from_millis(1));
    }
    // The second decode comes, and waits: what is shown is the first's alone.
    while calls.load(Ordering::SeqCst) < 2 {
        assert!(Instant::now() < limit, "no second decode");
        std::thread::sleep(Duration::from_millis(1));
    }
    release.store(true, Ordering::SeqCst);
    let (rest, _) = worker.finish();
    let finals = |events: &[PipelineEvent]| {
        events
            .iter()
            .filter(|e| {
                matches!(
                    e,
                    PipelineEvent::Transcript {
                        event: TranscriptEvent::Final(_),
                        ..
                    }
                )
            })
            .count()
    };
    assert_eq!(finals(&shown), 1);
    assert!(finals(&rest) >= 2, "{rest:?}");
}

/// The queue past the cap is taken whole, and the stretches stop as "fell behind".
#[test]
fn meeting_queue_a_stretch_worker_past_its_cap_stops_and_says_so() {
    let audio = noise(301, 16_000, 0.3);
    let worker = LiveWorker::spawn(
        TrackId::Mic,
        stretch_track(Box::new(ContentWords)),
        earshot_core::pipeline::PartialPolicy::Coalesce,
        8_000,
    )
    .unwrap();
    worker.send_owned(audio);
    let (events, _) = worker.finish();
    assert!(
        events.iter().any(|e| matches!(
            e,
            PipelineEvent::Error(PipelineError::LiveAsr {
                track: TrackId::Mic,
                ..
            })
        )),
        "{events:?}"
    );
}

/// A hold the speaker thread can take over without passing its own bound at the next packet.
#[test]
fn meeting_queue_a_full_speaker_hold_fits_the_thread_it_hands_over_to() {
    let hold = LiveHold::new(held_gates());
    assert!(hold.max_speaker_buffer < SPEAKER_MAX_BUFFER);
}

/// A failed save is in the event and in the outcome, the whole record: a host that reads only
/// `wait()` still hears it.
#[test]
fn meeting_queue_a_failed_save_is_in_the_event_and_in_the_outcome() {
    let dir = TempDir::new();
    let svc = services(&dir);
    let scene = scene();
    let (source, handles) = mic_source_of(&scene);
    let session = start_live(
        source,
        svc.clone(),
        stretch_config(ClockDomain::Replay),
        None,
        Box::new(ChaCha8Rng::seed_from_u64(311)),
    )
    .unwrap();
    // A non-empty folder where the file should go: the rename into place fails.
    let path = svc
        .data
        .root()
        .join("audio")
        .join(session.meeting().to_string())
        .join("mic.flac");
    std::fs::create_dir_all(path.join("in-the-way")).unwrap();
    handles.released.store(true, Ordering::SeqCst);
    let (events, outcome) = collect(session);
    let told: Vec<RuntimeWarning> = events
        .iter()
        .filter_map(|e| match e {
            RuntimeEvent::RecordingSaved { problems, .. } => Some(problems.clone()),
            _ => None,
        })
        .flatten()
        .collect();
    assert!(
        matches!(
            told.as_slice(),
            [RuntimeWarning::Recording {
                track: TrackId::Mic,
                ..
            }]
        ),
        "{told:?}"
    );
    assert_eq!(outcome.problems, told);
}
