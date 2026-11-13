//! Live summary tests in a session (T8.3j): fakes only. Every name contains `live_summar`.

use std::sync::Mutex;

use earshot_core::asr::CancelToken;
use earshot_core::prompt::SummaryInput;
use earshot_core::summary::{Decision, SummaryV1};

use super::*;
use crate::live_summaries::{LiveSummaries, MapChunk};

/// What the fake model does with a window.
#[derive(Clone, Copy, PartialEq)]
enum Behaviour {
    /// Answers at once with a partial citing the window's first segment.
    Answer,
    /// Waits until the run is cancelled (a model slower than the meeting).
    UntilCancelled,
    /// Panics: the summary thread crashes.
    Panics,
}

/// One window the fake saw.
#[derive(Debug, Clone)]
struct Seen {
    window: u64,
    /// (speaker, text) of the window's segments.
    lines: Vec<(String, String)>,
    /// Segments before the window in the input.
    before: usize,
    title: Option<String>,
    /// The user's own words the window carried (T12.15a).
    vocabulary: Vec<String>,
    /// The thread's OS priority (Windows).
    priority: Option<i32>,
}

struct FakeMapper {
    seconds: u32,
    behaviour: Behaviour,
    seen: Mutex<Vec<Seen>>,
}

impl FakeMapper {
    fn new(seconds: u32, behaviour: Behaviour) -> Arc<Self> {
        Arc::new(Self {
            seconds,
            behaviour,
            seen: Mutex::default(),
        })
    }

    fn seen(&self) -> Vec<Seen> {
        self.seen.lock().unwrap().clone()
    }
}

impl MapChunk for FakeMapper {
    fn model_id(&self) -> &str {
        "fake-live-model"
    }

    fn seconds(&self) -> u32 {
        self.seconds
    }

    fn map(
        &self,
        input: &SummaryInput,
        ranges: &[std::ops::Range<usize>],
        cancel: &CancelToken,
    ) -> Result<SummaryV1, String> {
        let last = ranges.last().unwrap().clone();
        #[cfg(windows)]
        let priority = Some(crate::priority::current());
        #[cfg(not(windows))]
        let priority = None;
        self.seen.lock().unwrap().push(Seen {
            window: crate::live_summaries::window_of(input, &last, self.seconds),
            lines: input.segments[last.clone()]
                .iter()
                .map(|s| (s.speaker.clone(), s.text.clone()))
                .collect(),
            before: last.start,
            title: input.meeting.title.clone(),
            vocabulary: input.vocabulary.words().to_vec(),
            priority,
        });
        assert!(self.behaviour != Behaviour::Panics, "the model crashed");
        if self.behaviour == Behaviour::UntilCancelled {
            let guard = std::time::Instant::now();
            while !cancel.is_cancelled() {
                assert!(guard.elapsed() < Duration::from_secs(60), "never cancelled");
                std::thread::sleep(Duration::from_millis(5));
            }
            return Err("cancelled".into());
        }
        Ok(SummaryV1 {
            title: String::new(),
            tldr: "t".into(),
            enhanced_notes: Vec::new(),
            decisions: vec![Decision {
                text: "made live".into(),
                source_segment_ids: vec![input.segments[last.start].id],
            }],
            action_items: Vec::new(),
            open_questions: Vec::new(),
            topics: Vec::new(),
        })
    }
}

fn summaries(mapper: &Arc<FakeMapper>, background: bool) -> Option<LiveSummaries> {
    Some(LiveSummaries {
        mapper: Arc::clone(mapper) as Arc<dyn MapChunk>,
        background,
    })
}

fn record_of(
    svc: &Services,
    meeting: EntityId,
) -> Option<crate::live_summaries::LiveSummaryRecord> {
    match crate::live_summaries::live_summary_record(&svc.store(), meeting).unwrap() {
        Some(crate::live_summaries::StoredLiveSummaries::Record(r)) => Some(*r),
        Some(other) => panic!("{other:?}"),
        None => None,
    }
}

fn mapped(events: &[RuntimeEvent]) -> Vec<(u64, Option<String>)> {
    events
        .iter()
        .filter_map(|e| match e {
            RuntimeEvent::LiveSummaryMapped { window, error, .. } => Some((*window, error.clone())),
            _ => None,
        })
        .collect()
}

#[test]
fn live_summaries_map_each_settled_window_during_the_session_and_record_it() {
    // Words at 1-3 s (window 0 of 4 s windows), silence, words from 9 s (window 2). The
    // source holds back everything after 8 s until window 0 was mapped: it is mapped while
    // the meeting goes on.
    let dir = TempDir::new();
    let svc = services(&dir);
    let mut w = words("a", 1000, 5);
    w.extend(words("b", 9000, 5));
    let audio = speech(&w, 12_000, 201);
    let mapper = FakeMapper::new(4, Behaviour::Answer);
    // The user's own words reach every window (T12.15a; an invented word).
    crate::vocabulary::set_vocabulary(&mut svc.store(), &["Quillmarsh"]).unwrap();
    let timed_out = Arc::new(std::sync::atomic::AtomicBool::new(false));
    // Released by the publish, not the call: a window whose `map` has returned is dropped if
    // the end cancels the thread before it publishes.
    let published = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let release = {
        let p = Arc::clone(&published);
        Box::new(move || p.load(Ordering::SeqCst))
    };
    let source = HoldSource {
        inner: ReplayBuilder::new(ORIGIN, MonoBuffer::new(R16, audio.clone()))
            .build()
            .unwrap(),
        hold_at: 8 * 16_000,
        delivered: 0,
        release,
        held_since: None,
        released: false,
        timed_out: Arc::clone(&timed_out),
    };
    let session = start_live(
        source,
        svc.clone(),
        LiveConfig {
            live_summaries: summaries(&mapper, false),
            ..stretch_config(earshot_core::model::ClockDomain::Replay)
        },
        Some(fake_asr(TrackId::Mic, audio.clone(), w.clone())),
        Box::new(ChaCha8Rng::seed_from_u64(202)),
    )
    .unwrap();
    let meeting = session.meeting();
    let mut session = session;
    let stream = session.take_events().unwrap();
    let mut events = Vec::new();
    for e in stream.iter() {
        if matches!(e, RuntimeEvent::LiveSummaryMapped { window: 0, .. }) {
            published.store(true, Ordering::SeqCst);
        }
        let end = matches!(e, RuntimeEvent::Ended(_));
        events.push(e);
        if end {
            break;
        }
    }
    let outcome = session.wait().unwrap();
    assert!(
        !timed_out.load(Ordering::SeqCst),
        "window 0 was not mapped while the source held back the rest of the meeting"
    );
    let seen = mapper.seen();
    assert_eq!(seen[0].window, 0);
    // The live transcript as the meeting reads during it: "Me", no title.
    let a: Vec<String> = texts(&w[..5]);
    assert_eq!(seen[0].title, None);
    assert!(
        seen.iter().all(|s| s.vocabulary == ["Quillmarsh"]),
        "{seen:?}"
    );
    assert_eq!(seen[0].before, 0);
    assert!(seen[0].lines.iter().all(|(who, _)| who == "Me"));
    let text: Vec<String> = seen[0]
        .lines
        .iter()
        .flat_map(|(_, t)| t.split(' ').map(str::to_owned))
        .collect();
    assert_eq!(text, a);
    // Window 1 had no speech: nothing mapped for it.
    assert!(seen.iter().all(|s| s.window != 1));
    assert_eq!(mapped(&events)[0], (0, None));
    // The record: window 0's partial, citing a stored live segment.
    let record = record_of(&svc, meeting).expect("a record");
    assert_eq!(record.model, "fake-live-model");
    assert_eq!(record.seconds, 4);
    // The words the windows were mapped with, so a later Dictionary change refuses them.
    assert_eq!(
        record.vocabulary.as_deref(),
        Some(&["Quillmarsh".to_owned()][..])
    );
    assert_eq!(record.partials[0].window, 0);
    let rows = live_segments(&**svc.store(), meeting, TranscriptPass::Live).unwrap();
    let cited = record.partials[0].summary.decisions[0].source_segment_ids[0];
    assert!(rows.iter().any(|r| r.segment.id == cited));
    // Every word still reached the live transcript.
    let stored: Vec<String> = rows.iter().flat_map(|r| texts(&r.segment.words)).collect();
    assert_eq!(stored, texts(&w));
    assert!(matches!(events.last(), Some(RuntimeEvent::Ended(o)) if *o == outcome));
}

/// A source that holds its end marker until `ready` says so. The session stops the summary thread
/// when capture ends, and a stop that arrives before a window reaches the model maps nothing, as
/// it should: a test about a window in progress needs that window in the model first, however
/// fast the replay ran. Never holds for longer than a failing test needs.
struct HoldEndUntil<S> {
    inner: S,
    ready: Box<dyn Fn() -> bool + Send>,
    held: Option<EndOfStream>,
    since: Option<std::time::Instant>,
}

impl<S> HoldEndUntil<S> {
    fn new(inner: S, ready: impl Fn() -> bool + Send + 'static) -> Self {
        Self {
            inner,
            ready: Box::new(ready),
            held: None,
            since: None,
        }
    }
}

/// Passes everything on but the end marker, which it keeps.
struct KeepEnd<'a> {
    sink: &'a mut dyn CaptureSink,
    held: &'a mut Option<EndOfStream>,
}

impl CaptureSink for KeepEnd<'_> {
    fn frames(&mut self, track: TrackId, frames: &[f32], device_ts: DeviceTime) {
        self.sink.frames(track, frames, device_ts);
    }

    fn discontinuity(&mut self, track: TrackId, discontinuity: Discontinuity) {
        self.sink.discontinuity(track, discontinuity);
    }

    fn end(&mut self, end: EndOfStream) {
        *self.held = Some(end);
    }
}

impl<S: AudioSource> AudioSource for HoldEndUntil<S> {
    type Error = S::Error;

    fn session(&self) -> &CaptureSession {
        self.inner.session()
    }

    fn pump(&mut self, sink: &mut dyn CaptureSink) -> Result<SourceStatus, Self::Error> {
        if self.held.is_none() {
            let status = self.inner.pump(&mut KeepEnd {
                sink,
                held: &mut self.held,
            })?;
            if self.held.is_none() {
                return Ok(status);
            }
        }
        let since = *self.since.get_or_insert_with(std::time::Instant::now);
        assert!(
            since.elapsed() < Duration::from_secs(60),
            "no window reached the model during the meeting"
        );
        if !(self.ready)() {
            return Ok(SourceStatus::Idle);
        }
        if let Some(end) = self.held.take() {
            sink.end(end);
        }
        Ok(SourceStatus::Ended)
    }

    fn stop(&mut self) {
        self.inner.stop();
    }
}

#[test]
fn live_summaries_never_hold_up_live_transcription() {
    // The model never finishes a window during the meeting: transcription, the recording
    // and the end go on without it; the window in progress is cancelled at the end and
    // nothing is recorded (the summary after the meeting maps it).
    let dir = TempDir::new();
    let svc = services(&dir);
    let mut w = words("a", 1000, 5);
    w.extend(words("b", 5000, 5));
    w.extend(words("c", 9000, 5));
    let audio = speech(&w, 12_000, 211);
    let mapper = FakeMapper::new(2, Behaviour::UntilCancelled);
    let in_model = Arc::clone(&mapper);
    let source = HoldEndUntil::new(
        ReplayBuilder::new(ORIGIN, MonoBuffer::new(R16, audio.clone()))
            .build()
            .unwrap(),
        move || !in_model.seen().is_empty(),
    );
    let session = start_live(
        source,
        svc.clone(),
        LiveConfig {
            live_summaries: summaries(&mapper, false),
            ..stretch_config(earshot_core::model::ClockDomain::Replay)
        },
        Some(fake_asr(TrackId::Mic, audio.clone(), w.clone())),
        Box::new(ChaCha8Rng::seed_from_u64(212)),
    )
    .unwrap();
    let meeting = session.meeting();
    let (events, _) = collect(session);
    assert!(!mapper.seen().is_empty(), "a window was being mapped");
    let rows = live_segments(&**svc.store(), meeting, TranscriptPass::Live).unwrap();
    let stored: Vec<String> = rows.iter().flat_map(|r| texts(&r.segment.words)).collect();
    assert_eq!(stored, texts(&w));
    assert!(mapped(&events).is_empty());
    assert!(record_of(&svc, meeting).is_none());
    let files = svc.store().audio_files(meeting).unwrap();
    assert_eq!(files.len(), 1);
}

/// A model that ignores cancellation and answers only when the test releases it.
struct Blocked {
    released: std::sync::atomic::AtomicBool,
    /// Whether `map` returned because it was released (not by its own bound).
    answered_released: std::sync::atomic::AtomicBool,
    calls: std::sync::atomic::AtomicU64,
}

impl MapChunk for Blocked {
    fn model_id(&self) -> &str {
        "fake-blocked-model"
    }

    fn seconds(&self) -> u32 {
        2
    }

    fn map(
        &self,
        _: &SummaryInput,
        _: &[std::ops::Range<usize>],
        _: &CancelToken,
    ) -> Result<SummaryV1, String> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        // Bounded only for a failing test.
        let guard = std::time::Instant::now();
        while !self.released.load(Ordering::SeqCst) {
            if guard.elapsed() > Duration::from_secs(60) {
                return Err("never released".into());
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        self.answered_released.store(true, Ordering::SeqCst);
        Err("released".into())
    }
}

#[test]
fn live_summaries_stop_wait_comes_after_the_live_transcript_is_finished() {
    // T7.4c: the live transcript is complete (`LiveTranscriptFinished`) before the session
    // waits for the live summary thread, so that wait (up to `speaker_stop_wait`) never
    // counts as transcription. The model here answers only when the test sees the event;
    // were the thread waited for first, the event would come only after the 60 s wait.
    let dir = TempDir::new();
    let svc = services(&dir);
    let mut w = words("a", 1000, 5);
    w.extend(words("b", 5000, 5));
    let audio = speech(&w, 8_000, 221);
    let mapper = Arc::new(Blocked {
        released: std::sync::atomic::AtomicBool::new(false),
        answered_released: std::sync::atomic::AtomicBool::new(false),
        calls: std::sync::atomic::AtomicU64::new(0),
    });
    let in_model = Arc::clone(&mapper);
    let source = HoldEndUntil::new(
        ReplayBuilder::new(ORIGIN, MonoBuffer::new(R16, audio.clone()))
            .build()
            .unwrap(),
        move || in_model.calls.load(Ordering::SeqCst) >= 1,
    );
    let mut session = start_live(
        source,
        svc.clone(),
        LiveConfig {
            live_summaries: Some(LiveSummaries {
                mapper: Arc::clone(&mapper) as Arc<dyn MapChunk>,
                background: false,
            }),
            speaker_stop_wait: Duration::from_secs(60),
            ..stretch_config(earshot_core::model::ClockDomain::Replay)
        },
        Some(fake_asr(TrackId::Mic, audio.clone(), w.clone())),
        Box::new(ChaCha8Rng::seed_from_u64(222)),
    )
    .unwrap();
    let events = session.take_events().unwrap();
    let mut order = Vec::new();
    for e in events.iter() {
        match &e {
            RuntimeEvent::LiveTranscriptFinished { .. } => {
                order.push("finished");
                mapper.released.store(true, Ordering::SeqCst);
            }
            RuntimeEvent::Ended(_) => {
                order.push("ended");
                break;
            }
            _ => {}
        }
    }
    session.wait().unwrap();
    assert_eq!(order, ["finished", "ended"]);
    assert!(
        mapper.calls.load(Ordering::SeqCst) >= 1,
        "a window was being mapped"
    );
    assert!(
        mapper.answered_released.load(Ordering::SeqCst),
        "the session waited for the summary thread before finishing the live transcript"
    );
}

#[test]
fn live_summaries_thread_that_stops_itself_is_reported_with_or_without_a_record() {
    // A crash at the first window: nothing was mapped, so no record says it stopped. The
    // session's event does (a refused lower priority takes the same path).
    let dir = TempDir::new();
    let svc = services(&dir);
    let mut w = words("a", 1000, 5);
    w.extend(words("b", 5000, 5));
    let audio = speech(&w, 8_000, 231);
    let mapper = FakeMapper::new(2, Behaviour::Panics);
    let source = ReplayBuilder::new(ORIGIN, MonoBuffer::new(R16, audio.clone()))
        .build()
        .unwrap();
    let session = start_live(
        source,
        svc.clone(),
        LiveConfig {
            live_summaries: summaries(&mapper, false),
            ..stretch_config(earshot_core::model::ClockDomain::Replay)
        },
        Some(fake_asr(TrackId::Mic, audio.clone(), w.clone())),
        Box::new(ChaCha8Rng::seed_from_u64(232)),
    )
    .unwrap();
    let meeting = session.meeting();
    let (events, _) = collect(session);
    assert!(!mapper.seen().is_empty(), "the model was called");
    assert!(record_of(&svc, meeting).is_none());
    let reasons: Vec<&str> = events
        .iter()
        .filter_map(|e| match e {
            RuntimeEvent::LiveSummariesOff { reason } => Some(reason.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(reasons, ["the live summary thread crashed"]);
}

/// A power supply fixed for the test.
struct OnBattery;

impl crate::power::PowerSource for OnBattery {
    fn supply(&self) -> PowerSupply {
        PowerSupply::Battery
    }
}

#[test]
fn live_summaries_are_off_on_battery_with_the_battery_speaker_setting() {
    let dir = TempDir::new();
    let svc = services(&dir);
    let w = words("a", 1000, 5);
    let audio = speech(&w, 6_000, 221);
    let mapper = FakeMapper::new(2, Behaviour::Answer);
    let source = ReplayBuilder::new(ORIGIN, MonoBuffer::new(R16, audio.clone()))
        .build()
        .unwrap();
    let session = start_live(
        source,
        svc.clone(),
        LiveConfig {
            live_summaries: summaries(&mapper, false),
            speaker_timing: SpeakerTiming::AfterCallOnBattery,
            power: PowerProbe::new(OnBattery),
            ..stretch_config(earshot_core::model::ClockDomain::Replay)
        },
        Some(fake_asr(TrackId::Mic, audio.clone(), w.clone())),
        Box::new(ChaCha8Rng::seed_from_u64(222)),
    )
    .unwrap();
    let meeting = session.meeting();
    let (events, _) = collect(session);
    assert!(events.iter().any(|e| matches!(
        e,
        RuntimeEvent::LiveSummariesOff { reason } if reason == crate::live_summaries::ON_BATTERY_REASON
    )));
    assert!(mapper.seen().is_empty());
    assert!(record_of(&svc, meeting).is_none());

    // The same on mains power maps (the probe is read only for the battery setting).
    let dir = TempDir::new();
    let svc = services(&dir);
    let mapper = FakeMapper::new(2, Behaviour::Answer);
    let session = start_live(
        ReplayBuilder::new(ORIGIN, MonoBuffer::new(R16, audio.clone()))
            .build()
            .unwrap(),
        svc.clone(),
        LiveConfig {
            live_summaries: summaries(&mapper, false),
            power: PowerProbe::new(OnBattery),
            ..stretch_config(earshot_core::model::ClockDomain::Replay)
        },
        Some(fake_asr(TrackId::Mic, audio, w)),
        Box::new(ChaCha8Rng::seed_from_u64(223)),
    )
    .unwrap();
    let (events, _) = collect(session);
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, RuntimeEvent::LiveSummariesOff { .. }))
    );
}

#[cfg(windows)]
#[test]
fn live_summaries_thread_runs_below_normal_priority() {
    use crate::live::summary_thread::{Job, SummaryThread};
    let mapper = FakeMapper::new(2, Behaviour::Answer);
    let thread = SummaryThread::spawn(&LiveSummaries {
        mapper: Arc::clone(&mapper) as Arc<dyn MapChunk>,
        background: true,
    })
    .unwrap();
    let input = SummaryInput {
        meeting: earshot_core::prompt::MeetingInfo {
            title: None,
            started: None,
            duration_ms: 0,
            attendees: Vec::new(),
        },
        segments: vec![earshot_core::prompt::PromptSegment {
            id: ids()(),
            start: MeetingTime::from_millis(500),
            speaker: "Me".into(),
            text: "hello".into(),
        }],
        notes: Vec::new(),
        template: earshot_core::prompt::Template::General,
        prompt_variant: earshot_core::prompt::PromptVariant::Current,
        rules: Default::default(),
        style: Default::default(),
        style_preset: Default::default(),
        vocabulary: Default::default(),
        user_name: None,
    };
    thread.send(Job {
        window: 0,
        input,
        ranges: std::iter::once(0..1).collect(),
    });
    // The publish, not the call: `map` marks `seen` before it returns, and `finish` cancels
    // first, so a starved below-normal thread has its window dropped by design.
    let drained = std::sync::Mutex::new(Vec::new());
    assert!(wait_until(Duration::from_secs(60), || {
        let mut d = drained.lock().unwrap();
        d.extend(thread.drain());
        !d.is_empty()
    }));
    let drained = drained.into_inner().unwrap();
    assert_eq!(drained[0].window, 0);
    assert_eq!(drained[0].error, None);
    let record = thread.finish(Duration::from_secs(60)).record;
    assert_eq!(
        mapper.seen()[0].priority,
        Some(crate::priority::BELOW_NORMAL)
    );
    assert_eq!(record.unwrap().partials.len(), 1);
    assert_eq!(
        crate::priority::current(),
        0,
        "only the summary thread changed"
    );
}
