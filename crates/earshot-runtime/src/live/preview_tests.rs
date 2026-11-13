//! T9.1m: provisional words are display-only. Replays and fakes only. Every name contains
//! `live_preview`.

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicUsize};
use std::time::Instant;

use earshot_core::asr::{BackendInfo, BackendKind, CancelToken, TranscribeOptions, Transcription};
use earshot_core::capture::SourceStatus;
use earshot_core::model::ClockDomain;
use earshot_llm_replay::mock::{MockResponse, MockServer};

use super::queue_tests::{Scene, by_kind, factory_of, held_config, held_gates, scene_of};
use super::*;
use crate::live::preview::{
    COVERED_SLACK, FELL_BEHIND_REASON, ON_BATTERY_REASON, PREVIEW_THREAD_LEFT, PreviewWords,
    StreamFeed,
};
use crate::live::{LivePreviews, PreviewLoader, PreviewOffReason, PreviewRefusal, PreviewReport};
use crate::power::{PowerProbe, PowerSource, PowerSupply};
use crate::replay::{PaceClock, PaceState, Paced};
use crate::stretches::{StoredStretches, stretch_record};
use crate::summary::{SummaryConfig, SummaryService};

/// A word no real decode says: wherever it is found, a preview's text got there.
const MARKER: &str = "zzprovisional";
const GUARD: Duration = Duration::from_secs(120);

/// A clock running `self.1` times as fast as the wall's: a replay in "real time" of a test's own.
struct FastClock(Instant, u32);

impl PaceClock for FastClock {
    fn now(&self) -> Duration {
        self.0.elapsed() * self.1
    }
}

/// `scene` paced at ten times real time: the previews get turns between its packets.
fn fast_source(scene: &Scene) -> Paced<earshot_core::capture::ReplaySource> {
    let source = ReplayBuilder::new(ORIGIN, MonoBuffer::new(R16, scene.mic.clone()))
        .reference(MonoBuffer::new(R16, scene.them.clone()))
        .build()
        .unwrap();
    Paced::new(
        source,
        Arc::new(FastClock(Instant::now(), 10)),
        true,
        Arc::new(PaceState::default()),
    )
}

/// What the fake preview model saw.
#[derive(Default)]
struct Seen {
    calls: AtomicUsize,
    /// Decodes running now.
    running: AtomicUsize,
    /// The audio length of each call (samples).
    lengths: Mutex<Vec<usize>>,
    /// When each call began.
    began: Mutex<Vec<Instant>>,
}

/// The fake's answer to one call.
type Script = Box<dyn FnMut(usize, &CancelToken) -> Result<Vec<Word>, AsrError> + Send>;

/// No preview starts sooner than the interval after the one before: a queue being worked off
/// would run them back to back (the test configs' interval is 10 ms).
fn assert_not_queued(seen: &Seen) {
    let began = seen.began.lock().unwrap();
    for pair in began.windows(2) {
        let gap = pair[1] - pair[0];
        assert!(gap >= Duration::from_millis(10), "{gap:?} between previews");
    }
}

/// A preview model that answers `MARKER`-words; `script` may wait or say something else.
struct FakePreview {
    seen: Arc<Seen>,
    script: Script,
    /// The backend it reports.
    kind: BackendKind,
}

impl earshot_core::asr::Transcriber for FakePreview {
    fn transcribe(
        &mut self,
        audio: &MonoBuffer,
        _: &TranscribeOptions,
        cancel: &CancelToken,
    ) -> Result<Transcription, AsrError> {
        let call = self.seen.calls.fetch_add(1, Ordering::SeqCst);
        self.seen.running.fetch_add(1, Ordering::SeqCst);
        self.seen.lengths.lock().unwrap().push(audio.samples.len());
        self.seen.began.lock().unwrap().push(Instant::now());
        let words = (self.script)(call, cancel);
        self.seen.running.fetch_sub(1, Ordering::SeqCst);
        Ok(Transcription {
            segments: vec![RawSegment::from_words(words?)],
            language: None,
            backend: self.backend_info(),
        })
    }

    fn backend_info(&self) -> BackendInfo {
        BackendInfo {
            kind: self.kind,
            device: "fake".to_owned(),
            model: "fake".to_owned(),
        }
    }
}

/// A track's real transcriber reporting `.0`, as a meeting transcribed on the graphics card (CUDA)
/// or the NPU (QNN) does.
struct OnBackend(BackendKind, Box<dyn earshot_core::asr::Transcriber + Send>);

impl earshot_core::asr::Transcriber for OnBackend {
    fn transcribe(
        &mut self,
        audio: &MonoBuffer,
        options: &TranscribeOptions,
        cancel: &CancelToken,
    ) -> Result<Transcription, AsrError> {
        let mut t = self.1.transcribe(audio, options, cancel)?;
        t.backend = self.backend_info();
        Ok(t)
    }

    fn backend_info(&self) -> BackendInfo {
        BackendInfo {
            kind: self.0,
            ..self.1.backend_info()
        }
    }
}

/// [`factory_of`], with both tracks' transcribers reporting CUDA.
fn cuda_factory_of(scene: &Scene) -> LiveAsrFactory {
    factory_on(scene, BackendKind::Cuda)
}

/// [`factory_of`], with both tracks' transcribers reporting `kind`.
fn factory_on(scene: &Scene, kind: BackendKind) -> LiveAsrFactory {
    let mut inner = factory_of(scene, Arc::default());
    Box::new(move |track, entity| {
        let mut asr = inner(track, entity)?;
        if let Some(asr) = asr.as_mut() {
            let real = std::mem::replace(
                &mut asr.asr.transcriber,
                Box::new(super::queue_tests::ContentWords),
            );
            asr.asr.transcriber = Box::new(OnBackend(kind, real));
        }
        Ok(asr)
    })
}

/// Words every decode agrees on, whatever the window.
fn marker_words(_: usize, _: &CancelToken) -> Result<Vec<Word>, AsrError> {
    Ok((0..4)
        .map(|i| Word::new(format!("{MARKER}{i}"), 100 + i * 100, 180 + i * 100, 0.9))
        .collect())
}

fn loader(
    seen: &Arc<Seen>,
    script: impl FnMut(usize, &CancelToken) -> Result<Vec<Word>, AsrError> + Send + 'static,
) -> PreviewLoader {
    let seen = Arc::clone(seen);
    let script = Mutex::new(Some(script));
    Arc::new(move |_| {
        let script = script.lock().unwrap().take().ok_or("loaded twice")?;
        Ok(Box::new(FakePreview {
            seen: Arc::clone(&seen),
            script: Box::new(script),
            kind: BackendKind::Fake,
        }) as Box<dyn earshot_core::asr::Transcriber + Send>)
    })
}

/// A [`FakePreview`] answering `MARKER`-words that reports the backend it was loaded for, as a real
/// session reports where ONNX Runtime placed it.
fn fake_session_on(
    seen: &Arc<Seen>,
    on: &BackendInfo,
) -> Box<dyn earshot_core::asr::Transcriber + Send> {
    Box::new(FakePreview {
        seen: Arc::clone(seen),
        script: Box::new(marker_words),
        kind: on.kind,
    })
}

fn previews(load: PreviewLoader) -> LivePreviews {
    LivePreviews {
        load,
        interval: Duration::from_millis(10),
        window: Duration::from_secs(12),
        slow: Duration::from_secs(60),
        slow_pause: Duration::from_secs(60),
        // A 10x replay turns a brief stall of a busy host into many seconds of queue. The rule has
        // its own test, at its production value.
        fell_behind: Duration::from_secs(3600),
        background: false,
        own_voice: None,
        rule: None,
        own_voice_rule: None,
        user_asked: true,
        stream: None,
    }
}

/// Every report a session sent, one per track that had previews.
fn reports_of(events: &[RuntimeEvent]) -> Vec<PreviewReport> {
    events
        .iter()
        .filter_map(|e| match e {
            RuntimeEvent::LivePreviewReport(r) => Some(r.clone()),
            _ => None,
        })
        .collect()
}

fn report_for(events: &[RuntimeEvent], track: TrackId) -> PreviewReport {
    let reports = reports_of(events);
    let mine: Vec<&PreviewReport> = reports.iter().filter(|r| r.track == Some(track)).collect();
    assert_eq!(mine.len(), 1, "one report for {track:?}: {reports:?}");
    mine[0].clone()
}

fn shown_on(events: &[RuntimeEvent], track: TrackId) -> bool {
    previews_of(events)
        .iter()
        .any(|(t, w)| *t == track && !w.is_empty())
}

fn config_with(previews: Option<LivePreviews>) -> LiveConfig {
    LiveConfig {
        previews,
        ..stretch_config(ClockDomain::Replay)
    }
}

fn report_of(events: &[RuntimeEvent]) -> Option<PreviewReport> {
    let reports: Vec<&PreviewReport> = events
        .iter()
        .filter_map(|e| match e {
            RuntimeEvent::LivePreviewReport(r) => Some(r),
            _ => None,
        })
        .collect();
    assert!(reports.len() <= 1, "one report per session: {reports:?}");
    reports.first().map(|r| (*r).clone())
}

fn previews_of(events: &[RuntimeEvent]) -> Vec<(TrackId, Vec<Word>)> {
    events
        .iter()
        .filter_map(|e| match e {
            RuntimeEvent::LivePreview { track, words } => Some((*track, words.clone())),
            _ => None,
        })
        .collect()
}

/// Everything a run stored of its live pass: the words and the stretch records.
#[derive(Debug, PartialEq)]
struct Stored {
    words: Vec<(TrackId, String, u64, u64)>,
    records: Vec<(TrackId, earshot_core::asr::DecodedPieces)>,
}

fn stored(svc: &Services, meeting: EntityId) -> Stored {
    let store = svc.store();
    let records = [TrackId::Mic, TrackId::Reference]
        .into_iter()
        .map(
            |track| match stretch_record(&store, meeting, track).unwrap() {
                Some(StoredStretches::Record(r)) => (track, r.decoded),
                other => panic!("{track:?}: {other:?}"),
            },
        )
        .collect();
    drop(store);
    Stored {
        words: by_kind(svc, meeting),
        records,
    }
}

/// A replay of `scene` with `previews`, on its own store; what it stored and sent.
fn run_with(
    scene: &Scene,
    previews: Option<LivePreviews>,
    seed: u64,
) -> (Stored, Vec<RuntimeEvent>, Services, EntityId, TempDir) {
    let dir = TempDir::new();
    let svc = services(&dir);
    let session = start_live(
        fast_source(scene),
        svc.clone(),
        config_with(previews),
        Some(factory_of(scene, Arc::default())),
        Box::new(ChaCha8Rng::seed_from_u64(seed)),
    )
    .unwrap();
    let meeting = session.meeting();
    let (events, _) = collect(session);
    (stored(&svc, meeting), events, svc, meeting, dir)
}

/// The rule above all: with previews on and off a replay stores identical words and identical
/// stretch records. The fake real model's words are a hash of exactly the samples it is given, so
/// the words match only if the same buffers were decoded.
#[test]
fn live_preview_on_and_off_store_identical_words_and_stretch_records() {
    let scene = scene_of(27);
    let (without, events, ..) = run_with(&scene, None, 301);
    assert_eq!(report_of(&events), None, "no previews, no report");
    assert!(previews_of(&events).is_empty());
    assert!(!without.words.is_empty());
    let seen = Arc::new(Seen::default());
    let (with, events, ..) = run_with(&scene, Some(previews(loader(&seen, marker_words))), 301);
    assert_eq!(with, without);
    // It proves something only if previews ran and were shown.
    let report = report_of(&events).expect("a report");
    assert!(report.decodes > 1, "{report:?}");
    assert_eq!(report.off, None);
    assert_eq!(report.track, Some(TrackId::Reference));
    assert!(
        previews_of(&events).iter().any(|(_, w)| !w.is_empty()),
        "nothing was shown: {report:?}"
    );
}

/// A slow real model.
struct SlowReal {
    inner: super::queue_tests::ContentWords,
    decode: Duration,
    /// Each decode also waits for this.
    open: Arc<AtomicBool>,
}

impl earshot_core::asr::Transcriber for SlowReal {
    fn transcribe(
        &mut self,
        audio: &MonoBuffer,
        options: &TranscribeOptions,
        cancel: &CancelToken,
    ) -> Result<Transcription, AsrError> {
        std::thread::sleep(self.decode);
        let limit = Instant::now() + GUARD;
        while !self.open.load(Ordering::SeqCst) {
            assert!(Instant::now() < limit, "a real decode was never let go");
            std::thread::sleep(Duration::from_millis(1));
        }
        self.inner.transcribe(audio, options, cancel)
    }

    fn backend_info(&self) -> BackendInfo {
        self.inner.backend_info()
    }
}

fn real_factory(open: Arc<AtomicBool>, decode: Duration) -> LiveAsrFactory {
    Box::new(move |track, entity| {
        let first = u8::from(track == TrackId::Reference) + 0xC1;
        let mut n = 0u8;
        let segment_ids: earshot_core::asr::SegmentIds = Box::new(move || {
            n = n.wrapping_add(1);
            EntityId::from_bytes([first, n, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1])
        });
        Ok(Some(
            LiveAsr {
                transcriber: Box::new(SlowReal {
                    inner: super::queue_tests::ContentWords,
                    decode,
                    open: Arc::clone(&open),
                }),
                vad: Box::new(EnergyVad::default()),
                config: ChunkerConfig::default(),
                track_entity: entity,
                segment_ids,
            }
            .into(),
        ))
    })
}

fn gated_factory(open: Arc<AtomicBool>) -> LiveAsrFactory {
    real_factory(open, Duration::ZERO)
}

/// Real decodes go ahead of a preview that never returns, and previews are never queued.
#[test]
fn live_preview_real_decodes_go_ahead_of_a_stuck_preview() {
    let scene = scene_of(40);
    let seen = Arc::new(Seen::default());
    // The first preview stays in its decode until the real transcript has come through, cancelled
    // or not: an encoder call cannot be interrupted.
    let release = Arc::new(AtomicBool::new(false));
    let waited = Arc::clone(&release);
    let load = loader(&seen, move |call, cancel| {
        if call == 0 {
            let limit = Instant::now() + GUARD;
            while !waited.load(Ordering::SeqCst) {
                assert!(Instant::now() < limit, "the real decodes never came");
                std::thread::sleep(Duration::from_millis(1));
            }
        }
        marker_words(call, cancel)
    });
    let dir = TempDir::new();
    let svc = services(&dir);
    let mut session = start_live(
        fast_source(&scene),
        svc.clone(),
        config_with(Some(previews(load))),
        Some(factory_of(&scene, Arc::default())),
        Box::new(ChaCha8Rng::seed_from_u64(303)),
    )
    .unwrap();
    let meeting = session.meeting();
    let events = session.take_events().unwrap();
    let mut all = Vec::new();
    let mut finals_while_stuck = 0;
    for event in &events {
        let is_final = matches!(
            event,
            RuntimeEvent::Transcript {
                event: TranscriptEvent::Final(_),
                ..
            }
        );
        if is_final && seen.running.load(Ordering::SeqCst) == 1 && !release.load(Ordering::SeqCst) {
            finals_while_stuck += 1;
            // Stretches have landed behind the stuck preview: let it go.
            if finals_while_stuck >= 2 {
                release.store(true, Ordering::SeqCst);
            }
        }
        let ended = matches!(event, RuntimeEvent::Ended(_));
        all.push(event);
        if ended {
            break;
        }
    }
    session.wait().unwrap();
    assert!(finals_while_stuck >= 2, "{finals_while_stuck}");
    assert_not_queued(&seen);
    let (without, ..) = run_with(&scene, None, 303);
    assert_eq!(stored(&svc, meeting), without);
    // Not one queued behind the stuck one: the next starts from the audio of its own moment.
    let lengths = seen.lengths.lock().unwrap();
    let window = R16.frames_for(Duration::from_secs(12));
    assert!(lengths.iter().all(|l| *l <= window), "{lengths:?}");
}

/// While a real decode runs, a due preview is skipped, not queued.
#[test]
fn live_preview_skips_while_a_real_decode_runs_and_never_queues() {
    let scene = scene_of(27);
    let seen = Arc::new(Seen::default());
    let dir = TempDir::new();
    let svc = services(&dir);
    // Real decodes wait until the recording is saved, so their queue passes `REAL_DECODE_QUEUE`
    // whatever the host's load (a timed fake decode only outran a replay that kept up).
    let open = Arc::new(AtomicBool::new(false));
    let mut session = start_live(
        fast_source(&scene),
        svc.clone(),
        config_with(Some(previews(loader(&seen, marker_words)))),
        Some(gated_factory(Arc::clone(&open))),
        Box::new(ChaCha8Rng::seed_from_u64(305)),
    )
    .unwrap();
    let mut all = Vec::new();
    for event in &session.take_events().unwrap() {
        if matches!(event, RuntimeEvent::RecordingSaved { .. }) {
            open.store(true, Ordering::SeqCst);
        }
        let ended = matches!(event, RuntimeEvent::Ended(_));
        all.push(event);
        if ended {
            break;
        }
    }
    session.wait().unwrap();
    let report = report_of(&all).expect("a report");
    assert!(report.skipped_busy > 0, "{report:?}");
    assert_not_queued(&seen);
}

/// Provisional text never reaches the store, an export or a summary request.
#[test]
fn live_preview_text_never_reaches_the_store_an_export_or_a_summary_request() {
    let scene = scene_of(27);
    let seen = Arc::new(Seen::default());
    let (stored_words, events, svc, meeting, dir) =
        run_with(&scene, Some(previews(loader(&seen, marker_words))), 307);
    assert!(
        previews_of(&events)
            .iter()
            .flat_map(|(_, w)| w)
            .any(|w| w.text.starts_with(MARKER)),
        "the marker must have been shown for this test to mean anything"
    );
    let carries = |text: &str| text.contains(MARKER);
    assert!(!carries(&format!("{stored_words:?}")));

    // A summary request, as a provider would see it.
    let server = MockServer::start(vec![MockResponse::completion(
        r#"{"title":"T","tldr":"S.","enhanced_notes":[],"decisions":[],"action_items":[],"open_questions":[],"topics":[]}"#,
        "stop",
    )])
    .unwrap();
    let summaries = SummaryService::new(
        &SummaryConfig::new(server.endpoint().as_str(), "m"),
        Arc::clone(&svc.clock),
    )
    .unwrap();
    let _ = summaries.summarize(
        &svc,
        meeting,
        &mut ChaCha8Rng::seed_from_u64(306),
        &CancelToken::new(),
    );
    let requests = server.requests();
    assert!(!requests.is_empty(), "the request is the test");
    for request in requests {
        assert!(!carries(&request.json().to_string()));
    }

    // An export.
    let vault = dir.0.join("vault");
    std::fs::create_dir_all(&vault).unwrap();
    crate::export::export_meeting(
        &svc,
        &vault,
        meeting,
        &earshot_core::export::ExportOptions::default(),
        &mut ChaCha8Rng::seed_from_u64(308),
    )
    .unwrap();
    let mut folders = vec![vault];
    let mut notes = 0;
    while let Some(folder) = folders.pop() {
        for entry in std::fs::read_dir(folder).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                folders.push(path);
            } else {
                notes += 1;
                assert!(!carries(&std::fs::read_to_string(path).unwrap()));
            }
        }
    }
    assert!(notes > 0, "the export is the test");

    // And no byte of any file of the data folder (the database, its log, the audio's folder).
    drop(svc);
    let mut folders = vec![dir.0.clone()];
    let needle = MARKER.as_bytes();
    while let Some(folder) = folders.pop() {
        for entry in std::fs::read_dir(folder).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                folders.push(path);
            } else {
                let bytes = std::fs::read(&path).unwrap();
                assert!(
                    !bytes.windows(needle.len()).any(|w| w == needle),
                    "{}",
                    path.display()
                );
            }
        }
    }
}

fn word(text: &str, start: u64) -> Word {
    Word::new(text, start, start + 300, 0.9)
}

/// `(texts shown)` after each of `decodes`, each with the frontier at 0.
fn shown_after(decodes: &[Vec<Word>]) -> Vec<Vec<String>> {
    let mut words = PreviewWords::default();
    decodes
        .iter()
        .map(|d| {
            words.hypothesis(d.clone(), MeetingTime::ZERO);
            texts(words.shown())
        })
        .collect()
}

/// A shown word stays through decodes that stay on blank, partly or wholly (`may_skip_speech`).
#[test]
fn live_preview_a_blank_staying_decode_does_not_make_shown_words_vanish_and_return() {
    let abc = vec![word("a", 0), word("b", 400), word("c", 800)];
    let abcd = [abc.clone(), vec![word("d", 1200)]].concat();
    let abcde = [abcd.clone(), vec![word("e", 1600)]].concat();
    let shown = shown_after(&[
        abc.clone(),
        abcd.clone(),
        Vec::new(),
        vec![word("a", 0)],
        Vec::new(),
        abcde.clone(),
        abcde.clone(),
    ]);
    let as_text = |s: &[&str]| s.iter().map(ToString::to_string).collect::<Vec<_>>();
    assert_eq!(
        shown[0],
        Vec::<String>::new(),
        "one decode is not agreement"
    );
    assert_eq!(shown[1], as_text(&["a", "b", "c"]));
    for later in &shown[2..] {
        assert!(
            later.starts_with(&shown[1]),
            "a shown word vanished: {shown:?}"
        );
    }
    for pair in shown.windows(2) {
        assert!(pair[1].starts_with(&pair[0]), "{shown:?}");
    }
    assert_eq!(shown.last().unwrap(), &as_text(&["a", "b", "c", "d", "e"]));
}

/// The same through a session: what is sent never loses a word except to settled text.
#[test]
fn live_preview_blank_decodes_in_a_session_keep_what_is_shown() {
    let scene = scene_of(27);
    let seen = Arc::new(Seen::default());
    // Agreeing decodes, then only blanks.
    let script = move |call: usize, cancel: &CancelToken| {
        if call < 2 {
            marker_words(call, cancel)
        } else {
            Ok(Vec::new())
        }
    };
    let (_, events, ..) = run_with(&scene, Some(previews(loader(&seen, script))), 309);
    assert!(seen.calls.load(Ordering::SeqCst) > 2, "blanks ran");
    let sends = previews_of(&events).len();
    assert!(sends > 0);
    assert_eq!(words_left_early(&events), Vec::<Word>::new());
    assert!(
        previews_of(&events).last().unwrap().1.is_empty(),
        "nothing provisional is left at the end"
    );
}

/// Words a [`RuntimeEvent::LivePreview`] took away before settled text had reached them, up to the
/// end of the live transcript (which clears what is left).
fn words_left_early(events: &[RuntimeEvent]) -> Vec<Word> {
    let mut settled = MeetingTime::ZERO;
    let mut last: Vec<Word> = Vec::new();
    let mut early = Vec::new();
    for event in events {
        match event {
            RuntimeEvent::LiveTranscriptFinished { .. } => break,
            RuntimeEvent::Transcript {
                event: TranscriptEvent::Final(s),
                ..
            } => settled = settled.max(s.end),
            RuntimeEvent::LivePreview { words, .. } => {
                early.extend(
                    last.iter()
                        .filter(|w| !words.contains(w) && w.start + COVERED_SLACK >= settled)
                        .cloned(),
                );
                last.clone_from(words);
            }
            _ => {}
        }
    }
    early
}

/// The check above can fail.
#[test]
fn live_preview_words_left_early_finds_a_word_that_vanished() {
    let shown = |words: Vec<Word>| RuntimeEvent::LivePreview {
        track: TrackId::Reference,
        words,
    };
    let a = word("a", 1000);
    let b = word("b", 1400);
    let events = [
        shown(vec![a.clone(), b.clone()]),
        // A blank decode that took the words away, with no settled text at them.
        shown(vec![a.clone()]),
    ];
    assert_eq!(words_left_early(&events), vec![b]);
    let final_at = |end: u64| RuntimeEvent::Transcript {
        track: TrackId::Reference,
        event: TranscriptEvent::Final(earshot_core::asr::Segment::from_words(
            EntityId::from_bytes([1; 16]),
            EntityId::from_bytes([2; 16]),
            vec![word("x", end - 300)],
            MeetingTime::ZERO,
        )),
    };
    let events = [
        shown(vec![a.clone(), word("b", 1400)]),
        final_at(2000),
        shown(Vec::new()),
    ];
    assert!(words_left_early(&events).is_empty());
}

/// A window that slides over speech cuts its first word; the join keeps every word once.
#[test]
fn live_preview_windows_join_without_repeating_or_changing_shown_words() {
    let speech: Vec<Word> = (0..40).map(|i| word(&format!("w{i}"), i * 400)).collect();
    let mut words = PreviewWords::default();
    let mut shown_before: Vec<String> = Vec::new();
    let mut changes = 0;
    // A 6 s window, one step a second: its first word is cut each time (a different word), and
    // its last, not yet heard whole, comes out wrong.
    for second in 1u64..=16 {
        let end = second * 1000;
        let from = end.saturating_sub(6000);
        let mut hypothesis: Vec<Word> = speech
            .iter()
            .filter(|w| w.start.as_millis() > from && w.end.as_millis() <= end)
            .cloned()
            .collect();
        if let Some(last) = hypothesis.last_mut() {
            last.text = format!("{}x{second}", last.text);
        }
        words.hypothesis(hypothesis, MeetingTime::ZERO);
        let now = texts(words.shown());
        if !now.starts_with(&shown_before) {
            changes += 1;
        }
        shown_before = now;
    }
    assert_eq!(changes, 0, "a shown word changed");
    let unique: std::collections::BTreeSet<&String> = shown_before.iter().collect();
    assert_eq!(unique.len(), shown_before.len(), "{shown_before:?}");
    assert!(shown_before.len() >= 30, "{shown_before:?}");
    assert!(
        shown_before.iter().all(|w| !w.contains('x')),
        "an unstable word was shown: {shown_before:?}"
    );
}

/// Words the real decode covers leave, the rest stay; once none is left the next stretch starts
/// afresh rather than anchoring on the last one.
#[test]
fn live_preview_settled_words_leave_and_the_rest_stay() {
    let mut words = PreviewWords::default();
    let abc = vec![word("a", 0), word("b", 400), word("c", 800)];
    words.hypothesis(abc.clone(), MeetingTime::ZERO);
    words.hypothesis(abc, MeetingTime::ZERO);
    assert_eq!(words.shown().len(), 3);
    assert!(!words.settled(MeetingTime::ZERO), "nothing covered yet");
    assert!(words.settled(MeetingTime::from_millis(700)));
    assert_eq!(texts(words.shown()), vec!["c"]);
    assert!(words.settled(MeetingTime::from_millis(1_500)));
    assert!(words.shown().is_empty());
    // A later stretch that repeats those words is new speech.
    let again = vec![word("c", 2000), word("d", 2400)];
    words.hypothesis(again.clone(), MeetingTime::from_millis(2000));
    words.hypothesis(again, MeetingTime::from_millis(2000));
    assert_eq!(
        texts(words.shown()),
        vec!["c", "d"],
        "not anchored on the old c"
    );
}

/// Words the worker's frontier has passed during a decode are not shown.
#[test]
fn live_preview_words_behind_the_frontier_are_not_shown() {
    let mut words = PreviewWords::default();
    let abc = vec![word("a", 0), word("b", 400), word("c", 800)];
    words.hypothesis(abc.clone(), MeetingTime::from_millis(500));
    words.hypothesis(abc, MeetingTime::from_millis(500));
    assert_eq!(texts(words.shown()), vec!["c"]);
}

/// The echo rule works on settled text, so a call's mic shows no previews: the far end's words
/// that leak into the mic are never decoded for display, with the echo in or out.
#[test]
fn live_preview_mic_in_a_call_shows_no_far_end_word() {
    let sim = leaky_call(true);
    let n = sim.mic.len() / earshot_core::aec::BLOCK * earshot_core::aec::BLOCK;
    let scene = Scene {
        mic: sim.mic[..n].to_vec(),
        them: sim.reference[..n].to_vec(),
    };
    let seen = Arc::new(Seen::default());
    let (_, events, ..) = run_with(&scene, Some(previews(loader(&seen, marker_words))), 311);
    let report = report_of(&events).expect("a report");
    assert_eq!(report.track, Some(TrackId::Reference));
    assert!(
        previews_of(&events)
            .iter()
            .all(|(track, _)| *track == TrackId::Reference)
    );
}

/// In the room there is no far end: the mic is the track with previews.
#[test]
fn live_preview_in_the_room_runs_on_the_mic() {
    let scene = scene_of(27);
    let seen = Arc::new(Seen::default());
    let dir = TempDir::new();
    let svc = services(&dir);
    let source = ReplayBuilder::new(ORIGIN, MonoBuffer::new(R16, scene.mic.clone()))
        .build()
        .unwrap();
    let paced = Paced::new(
        source,
        Arc::new(FastClock(Instant::now(), 10)),
        true,
        Arc::new(PaceState::default()),
    );
    let session = start_live(
        paced,
        svc.clone(),
        config_with(Some(previews(loader(&seen, marker_words)))),
        Some(factory_of(&scene, Arc::default())),
        Box::new(ChaCha8Rng::seed_from_u64(313)),
    )
    .unwrap();
    let (events, _) = collect(session);
    let report = report_of(&events).expect("a report");
    assert_eq!(report.track, Some(TrackId::Mic));
    assert!(report.decodes > 0, "{report:?}");
}

/// T9.14: with the user's own track chosen, a call shows previews on both tracks, each from its own
/// model session, and sends one report per track; what is stored does not change.
#[test]
fn live_preview_own_voice_in_a_call_shows_both_tracks_and_reports_each() {
    let scene = scene_of(27);
    let far = Arc::new(Seen::default());
    let own = Arc::new(Seen::default());
    let config = LivePreviews {
        own_voice: Some(loader(&own, marker_words)),
        ..previews(loader(&far, marker_words))
    };
    let (with, events, ..) = run_with(&scene, Some(config), 331);
    let reports = reports_of(&events);
    assert_eq!(reports.len(), 2, "{reports:?}");
    for track in [TrackId::Reference, TrackId::Mic] {
        let report = report_for(&events, track);
        assert_eq!(report.off, None, "{report:?}");
        assert!(report.decodes > 0, "{report:?}");
        assert!(shown_on(&events, track), "nothing shown on {track:?}");
    }
    // Two sessions of their own: each loader was called once (`loader` refuses a second load).
    assert!(far.calls.load(Ordering::SeqCst) > 0);
    assert!(own.calls.load(Ordering::SeqCst) > 0);
    assert!(words_left_early(&events).is_empty());
    for track in [TrackId::Reference, TrackId::Mic] {
        let last = previews_of(&events)
            .into_iter()
            .rev()
            .find(|(t, _)| *t == track)
            .expect("a preview");
        assert!(
            last.1.is_empty(),
            "nothing provisional is left on {track:?}"
        );
    }
    let (without, ..) = run_with(&scene, None, 331);
    assert_eq!(with, without);
}

/// T12.3a: in a meeting transcribed on CUDA, the far end's previews load on the track's backend and
/// the user's own track's session loads on the CPU (`backends::load_preview_second_with`, the rule's
/// own decision); each track's report says where its session ran.
#[test]
fn live_preview_own_voice_beside_cuda_loads_on_the_cpu_and_reports_it() {
    use crate::preview_settings::PreviewChoice;
    use earshot_hardware::{Gpu, Hardware};
    let scene = scene_of(27);
    let far = Arc::new(Seen::default());
    let own = Arc::new(Seen::default());
    let big = Hardware {
        total_memory: Some(64 * 1024 * 1024 * 1024),
        gpus: vec![Gpu {
            name: "A card".to_owned(),
            vendor_id: 0x10de,
            dedicated_memory: Some(crate::backends::PREVIEW_MIN_CARD_MEMORY),
            driver_version: None,
            integrated: Some(false),
            compute_capability: None,
        }],
        ..Hardware::default()
    };
    let asked = Arc::new(Mutex::new(Vec::new()));
    let far_load: PreviewLoader = {
        let (far, asked) = (Arc::clone(&far), Arc::clone(&asked));
        Arc::new(move |backend: &BackendInfo| {
            asked.lock().unwrap().push(("far", backend.kind));
            Ok(fake_session_on(&far, backend))
        })
    };
    let own_load: PreviewLoader = {
        let (own, asked) = (Arc::clone(&own), Arc::clone(&asked));
        Arc::new(move |backend: &BackendInfo| {
            asked.lock().unwrap().push(("own", backend.kind));
            crate::backends::load_preview_second_with(
                backend,
                &big,
                PreviewChoice::Everyone,
                |on| Ok(fake_session_on(&own, on)),
            )
        })
    };
    let config = LivePreviews {
        own_voice: Some(own_load),
        ..previews(far_load)
    };
    let dir = TempDir::new();
    let svc = services(&dir);
    let session = start_live(
        fast_source(&scene),
        svc.clone(),
        config_with(Some(config)),
        Some(cuda_factory_of(&scene)),
        Box::new(ChaCha8Rng::seed_from_u64(341)),
    )
    .unwrap();
    let (events, _) = collect(session);
    // Both loaders were handed the track's backend: CUDA.
    let mut asked = asked.lock().unwrap().clone();
    asked.sort_unstable_by_key(|(slot, _)| *slot);
    assert_eq!(
        asked,
        vec![("far", BackendKind::Cuda), ("own", BackendKind::Cuda)]
    );
    let mic = report_for(&events, TrackId::Mic);
    assert_eq!(mic.backend, Some(BackendKind::Cpu), "{mic:?}");
    assert_eq!(mic.off, None, "{mic:?}");
    assert!(mic.decodes > 0, "{mic:?}");
    let far_report = report_for(&events, TrackId::Reference);
    assert_eq!(
        far_report.backend,
        Some(BackendKind::Cuda),
        "{far_report:?}"
    );
    assert_eq!(far_report.off, None, "{far_report:?}");
    assert!(shown_on(&events, TrackId::Mic));
    assert!(shown_on(&events, TrackId::Reference));
}

/// T12.3c: in a meeting transcribed on CUDA with a card too small for a preview session, both
/// preview sessions load on the CPU (`backends::load_preview_with` and `load_preview_second_with`,
/// the rule's own decisions), and each track's report says so.
#[test]
fn live_preview_beside_a_small_card_loads_both_sessions_on_the_cpu_and_reports_it() {
    use crate::preview_settings::PreviewChoice;
    use earshot_hardware::{Gpu, Hardware};
    let scene = scene_of(27);
    let far = Arc::new(Seen::default());
    let own = Arc::new(Seen::default());
    let small_card = Hardware {
        total_memory: Some(64 * 1024 * 1024 * 1024),
        gpus: vec![Gpu {
            name: "A card".to_owned(),
            vendor_id: 0x10de,
            dedicated_memory: Some(crate::backends::PREVIEW_MIN_CARD_MEMORY - 1),
            driver_version: None,
            integrated: Some(false),
            compute_capability: None,
        }],
        ..Hardware::default()
    };
    let asked = Arc::new(Mutex::new(Vec::new()));
    let far_load: PreviewLoader = {
        let (far, asked, hw) = (Arc::clone(&far), Arc::clone(&asked), small_card.clone());
        Arc::new(move |backend: &BackendInfo| {
            asked.lock().unwrap().push(("far", backend.kind));
            crate::backends::load_preview_with(backend, &hw, PreviewChoice::Everyone, |on| {
                Ok(fake_session_on(&far, on))
            })
        })
    };
    let own_load: PreviewLoader = {
        let (own, asked, hw) = (Arc::clone(&own), Arc::clone(&asked), small_card);
        Arc::new(move |backend: &BackendInfo| {
            asked.lock().unwrap().push(("own", backend.kind));
            crate::backends::load_preview_second_with(backend, &hw, PreviewChoice::Everyone, |on| {
                Ok(fake_session_on(&own, on))
            })
        })
    };
    let config = LivePreviews {
        own_voice: Some(own_load),
        ..previews(far_load)
    };
    let dir = TempDir::new();
    let svc = services(&dir);
    let session = start_live(
        fast_source(&scene),
        svc.clone(),
        config_with(Some(config)),
        Some(cuda_factory_of(&scene)),
        Box::new(ChaCha8Rng::seed_from_u64(347)),
    )
    .unwrap();
    let (events, _) = collect(session);
    // Both loaders were handed the track's backend, CUDA; the rule moved both to the CPU.
    let mut asked = asked.lock().unwrap().clone();
    asked.sort_unstable_by_key(|(slot, _)| *slot);
    assert_eq!(
        asked,
        vec![("far", BackendKind::Cuda), ("own", BackendKind::Cuda)]
    );
    for track in [TrackId::Mic, TrackId::Reference] {
        let report = report_for(&events, track);
        assert_eq!(report.backend, Some(BackendKind::Cpu), "{report:?}");
        assert_eq!(report.off, None, "{report:?}");
        assert!(report.decodes > 0, "{report:?}");
        assert!(shown_on(&events, track), "{track:?}");
    }
}

/// A report says where its session ran, and nothing when no session loaded.
#[test]
fn live_preview_report_names_the_loaded_sessions_backend() {
    let scene = scene_of(27);
    let seen = Arc::new(Seen::default());
    let (_, events, ..) = run_with(&scene, Some(previews(loader(&seen, marker_words))), 343);
    let report = report_of(&events).expect("a report");
    assert_eq!(report.backend, Some(BackendKind::Fake), "{report:?}");
    let refused: PreviewLoader = Arc::new(|_| Err("not on this backend".into()));
    let (_, events, ..) = run_with(&scene, Some(previews(refused)), 345);
    let report = report_of(&events).expect("a report");
    assert_eq!(report.backend, None, "{report:?}");
}

/// Without the user's own track chosen a call is as before: the far end only, one report.
#[test]
fn live_preview_without_own_voice_a_call_shows_only_the_far_end() {
    let scene = scene_of(27);
    let seen = Arc::new(Seen::default());
    let (_, events, ..) = run_with(&scene, Some(previews(loader(&seen, marker_words))), 333);
    let reports = reports_of(&events);
    assert_eq!(reports.len(), 1, "{reports:?}");
    assert_eq!(reports[0].track, Some(TrackId::Reference));
    assert!(shown_on(&events, TrackId::Reference));
    assert!(!shown_on(&events, TrackId::Mic));
}

/// The user's own track failing to load is that track's report: the far end still shows words.
#[test]
fn live_preview_own_voice_failing_to_load_leaves_the_far_end_running() {
    let scene = scene_of(27);
    let seen = Arc::new(Seen::default());
    let config = LivePreviews {
        own_voice: Some(Arc::new(|_| Err("needs more memory".into()))),
        ..previews(loader(&seen, marker_words))
    };
    let (with, events, ..) = run_with(&scene, Some(config), 335);
    let mic = report_for(&events, TrackId::Mic);
    assert_eq!(mic.off.as_deref(), Some("needs more memory"));
    let far = report_for(&events, TrackId::Reference);
    assert_eq!(far.off, None, "{far:?}");
    assert!(far.decodes > 0, "{far:?}");
    assert!(shown_on(&events, TrackId::Reference));
    assert!(!shown_on(&events, TrackId::Mic));
    assert!(warnings(&events).is_empty(), "{:?}", warnings(&events));
    let (without, ..) = run_with(&scene, None, 335);
    assert_eq!(with, without);
}

/// The user's own track crashing stops only its previews.
#[test]
fn live_preview_own_voice_crashing_leaves_the_far_end_running() {
    let scene = scene_of(27);
    let far = Arc::new(Seen::default());
    let own = Arc::new(Seen::default());
    let crash = |_: usize, _: &CancelToken| -> Result<Vec<Word>, AsrError> {
        panic!("a test crash");
    };
    let config = LivePreviews {
        own_voice: Some(loader(&own, crash)),
        ..previews(loader(&far, marker_words))
    };
    let (_, events, ..) = run_with(&scene, Some(config), 337);
    let mic = report_for(&events, TrackId::Mic);
    assert_eq!(mic.off.as_deref(), Some("the preview thread crashed"));
    let far_report = report_for(&events, TrackId::Reference);
    assert_eq!(far_report.off, None, "{far_report:?}");
    assert!(shown_on(&events, TrackId::Reference));
}

/// In the room the mic has the one session; the user's own track setting adds nothing.
#[test]
fn live_preview_in_the_room_ignores_own_voice() {
    let scene = scene_of(27);
    let first = Arc::new(Seen::default());
    let own = Arc::new(Seen::default());
    let dir = TempDir::new();
    let svc = services(&dir);
    let source = ReplayBuilder::new(ORIGIN, MonoBuffer::new(R16, scene.mic.clone()))
        .build()
        .unwrap();
    let paced = Paced::new(
        source,
        Arc::new(FastClock(Instant::now(), 10)),
        true,
        Arc::new(PaceState::default()),
    );
    let config = LivePreviews {
        own_voice: Some(loader(&own, marker_words)),
        ..previews(loader(&first, marker_words))
    };
    let session = start_live(
        paced,
        svc.clone(),
        config_with(Some(config)),
        Some(factory_of(&scene, Arc::default())),
        Box::new(ChaCha8Rng::seed_from_u64(339)),
    )
    .unwrap();
    let (events, _) = collect(session);
    let report = report_of(&events).expect("a report");
    assert_eq!(report.track, Some(TrackId::Mic));
    assert!(report.decodes > 0, "{report:?}");
    assert!(first.calls.load(Ordering::SeqCst) > 0);
    assert_eq!(own.calls.load(Ordering::SeqCst), 0, "own_voice loaded");
}

struct Fixed(PowerSupply);

impl PowerSource for Fixed {
    fn supply(&self) -> PowerSupply {
        self.0
    }
}

#[test]
fn live_preview_is_off_on_battery_and_costs_nothing() {
    let scene = scene_of(27);
    let seen = Arc::new(Seen::default());
    let dir = TempDir::new();
    let svc = services(&dir);
    let config = LiveConfig {
        power: PowerProbe::new(Fixed(PowerSupply::Battery)),
        ..config_with(Some(previews(loader(&seen, marker_words))))
    };
    let session = start_live(
        fast_source(&scene),
        svc.clone(),
        config,
        Some(factory_of(&scene, Arc::default())),
        Box::new(ChaCha8Rng::seed_from_u64(315)),
    )
    .unwrap();
    let meeting = session.meeting();
    let (events, _) = collect(session);
    let report = report_of(&events).expect("a report");
    assert_eq!(report.off.as_deref(), Some(ON_BATTERY_REASON));
    assert_eq!(seen.calls.load(Ordering::SeqCst), 0);
    assert!(previews_of(&events).is_empty());
    let (without, ..) = run_with(&scene, None, 315);
    assert_eq!(stored(&svc, meeting), without);
}

/// A preview model that cannot load is a line in the report, nothing else.
#[test]
fn live_preview_a_failed_load_costs_nothing() {
    let scene = scene_of(27);
    let load: PreviewLoader = Arc::new(|_| Err("not on this backend".into()));
    let (with, events, ..) = run_with(&scene, Some(previews(load)), 317);
    let report = report_of(&events).expect("a report");
    assert_eq!(report.off.as_deref(), Some("not on this backend"));
    assert!(warnings(&events).is_empty(), "{:?}", warnings(&events));
    let (without, ..) = run_with(&scene, None, 317);
    assert_eq!(with, without);
}

/// A decode that fails or a thread that crashes stops the previews and nothing else.
#[test]
fn live_preview_a_failing_decode_stops_only_the_previews() {
    let scene = scene_of(27);
    let seen = Arc::new(Seen::default());
    let script = |_: usize, _: &CancelToken| Err(AsrError::Backend("bad".to_owned()));
    let (with, events, ..) = run_with(&scene, Some(previews(loader(&seen, script))), 319);
    let report = report_of(&events).expect("a report");
    assert!(
        report.off.as_deref().unwrap_or("").contains("bad"),
        "{report:?}"
    );
    let (without, ..) = run_with(&scene, None, 319);
    assert_eq!(with, without);
}

/// A crash on the preview thread is a line in the report: the recording and the words go on.
#[test]
fn live_preview_a_crashed_thread_stops_only_the_previews() {
    let scene = scene_of(27);
    let seen = Arc::new(Seen::default());
    let script = |_: usize, _: &CancelToken| -> Result<Vec<Word>, AsrError> {
        panic!("a test crash");
    };
    let (with, events, ..) = run_with(&scene, Some(previews(loader(&seen, script))), 325);
    let report = report_of(&events).expect("a report");
    assert_eq!(report.off.as_deref(), Some("the preview thread crashed"));
    let (without, ..) = run_with(&scene, None, 325);
    assert_eq!(with, without);
}

/// A preview thread still in a decode when the end wait runs out is left, and its report says so:
/// it never reads as previews that ran to the end.
#[test]
fn live_preview_a_thread_left_at_the_end_wait_says_so_in_the_report() {
    let scene = scene_of(27);
    let seen = Arc::new(Seen::default());
    // Every decode stays in progress, cancelled or not, until the session has ended.
    let release = Arc::new(AtomicBool::new(false));
    let waited = Arc::clone(&release);
    let load = loader(&seen, move |call, cancel| {
        let limit = Instant::now() + GUARD;
        while !waited.load(Ordering::SeqCst) {
            assert!(Instant::now() < limit, "the session never ended");
            std::thread::sleep(Duration::from_millis(1));
        }
        marker_words(call, cancel)
    });
    let dir = TempDir::new();
    let svc = services(&dir);
    let session = start_live(
        fast_source(&scene),
        svc.clone(),
        LiveConfig {
            speaker_stop_wait: Duration::from_millis(50),
            ..config_with(Some(previews(load)))
        },
        Some(factory_of(&scene, Arc::default())),
        Box::new(ChaCha8Rng::seed_from_u64(327)),
    )
    .unwrap();
    let (events, _) = collect(session);
    release.store(true, Ordering::SeqCst);
    assert!(
        seen.calls.load(Ordering::SeqCst) > 0,
        "a decode was in progress"
    );
    let report = report_of(&events).expect("a report");
    assert_eq!(
        report.off.as_deref(),
        Some(PREVIEW_THREAD_LEFT),
        "{report:?}"
    );
    assert_eq!(report.reason, Some(PreviewOffReason::Failed), "{report:?}");
}

/// Previews that take too long pause and come back: other work having the machine for a while does
/// not cost the rest of the meeting its previews.
#[test]
fn live_preview_slow_previews_pause_and_come_back() {
    let scene = scene_of(27);
    let seen = Arc::new(Seen::default());
    let script = |call: usize, cancel: &CancelToken| {
        if call < 3 {
            std::thread::sleep(Duration::from_millis(5));
        }
        marker_words(call, cancel)
    };
    let config = LivePreviews {
        slow: Duration::from_millis(4),
        slow_pause: Duration::from_millis(50),
        ..previews(loader(&seen, script))
    };
    let (_, events, ..) = run_with(&scene, Some(config), 327);
    let report = report_of(&events).expect("a report");
    assert_eq!(report.slow_pauses, 1, "{report:?}");
    assert_eq!(report.off, None);
    assert!(report.decodes > 3, "they came back: {report:?}");
    let began = seen.began.lock().unwrap();
    assert!(
        began[3] - began[2] >= Duration::from_millis(50),
        "the pause: {:?}",
        began[3] - began[2]
    );
}

/// Holds its packets until the test has seen what it wants.
struct WaitingSource<S> {
    inner: S,
    go: Arc<AtomicBool>,
}

impl<S: AudioSource> AudioSource for WaitingSource<S> {
    type Error = S::Error;

    fn session(&self) -> &CaptureSession {
        self.inner.session()
    }

    fn pump(&mut self, sink: &mut dyn CaptureSink) -> Result<SourceStatus, Self::Error> {
        if self.go.load(Ordering::SeqCst) {
            self.inner.pump(sink)
        } else {
            Ok(SourceStatus::Idle)
        }
    }

    fn stop(&mut self) {
        self.inner.stop();
    }
}

/// During a hold and the catch-up after it, no preview runs; they start once it is up to date.
#[test]
fn live_preview_stays_off_during_a_hold_and_its_catch_up() {
    let scene = scene_of(27);
    let seen = Arc::new(Seen::default());
    let load = loader(&seen, marker_words);
    assert_previews_wait_for_the_hold(&scene, factory_of(&scene, Arc::default()), load, &seen, 321);
}

/// T12.12c: the same on the NPU, through the app's rule on an NPU machine: nothing is loaded or
/// decoded for held audio, and the far end's previews start, on the NPU, once it is up to date.
#[test]
fn live_preview_on_the_npu_stays_off_during_a_hold_and_its_catch_up() {
    use crate::preview_settings::PreviewChoice;
    let scene = scene_of(27);
    let seen = Arc::new(Seen::default());
    let asked = Arc::new(Mutex::new(Vec::new()));
    let load = npu_rule_loader(
        &seen,
        &asked,
        npu_machine(Some(15 * GIB)),
        PreviewChoice::On,
    );
    let events = assert_previews_wait_for_the_hold(
        &scene,
        factory_on(&scene, BackendKind::Qnn),
        load,
        &seen,
        353,
    );
    assert_eq!(*asked.lock().unwrap(), vec![BackendKind::Qnn]);
    let report = report_of(&events).expect("a report");
    assert_eq!(report.backend, Some(BackendKind::Qnn), "{report:?}");
}

/// Runs `scene` held until its first `Held`, checks no preview model was called for held audio and
/// that words show only after the transcript is up to date; returns every event.
fn assert_previews_wait_for_the_hold(
    scene: &Scene,
    factory: LiveAsrFactory,
    load: PreviewLoader,
    seen: &Arc<Seen>,
    seed: u64,
) -> Vec<RuntimeEvent> {
    let gates = held_gates();
    let dir = TempDir::new();
    let svc = services(&dir);
    let go = Arc::new(AtomicBool::new(true));
    let source = WaitingSource {
        inner: fast_source(scene),
        go: Arc::clone(&go),
    };
    let config = LiveConfig {
        previews: Some(previews(load)),
        ..held_config(gates.clone())
    };
    let mut session = start_live(
        source,
        svc.clone(),
        config,
        Some(factory),
        Box::new(ChaCha8Rng::seed_from_u64(seed)),
    )
    .unwrap();
    let events = session.take_events().unwrap();
    let mut all = Vec::new();
    for event in &events {
        if matches!(
            event,
            RuntimeEvent::LiveTranscript(LiveTranscriptState::Held(_))
        ) {
            // Some audio is held; no preview model may be touched for it.
            std::thread::sleep(Duration::from_millis(100));
            assert_eq!(seen.calls.load(Ordering::SeqCst), 0, "held");
            gates.transcription.store(true, Ordering::SeqCst);
        }
        let ended = matches!(event, RuntimeEvent::Ended(_));
        all.push(event);
        if ended {
            break;
        }
    }
    session.wait().unwrap();
    // The session sends these in order, so this reads when it happened, not when the test looked.
    let at = |pred: &dyn Fn(&RuntimeEvent) -> bool| all.iter().position(pred);
    let held = at(&|e| {
        matches!(
            e,
            RuntimeEvent::LiveTranscript(LiveTranscriptState::Held(_))
        )
    });
    let up_to_date = at(&|e| {
        matches!(
            e,
            RuntimeEvent::LiveTranscript(LiveTranscriptState::UpToDate)
        )
    });
    let shown = at(&|e| matches!(e, RuntimeEvent::LivePreview { words, .. } if !words.is_empty()));
    assert!(held.is_some());
    let (up_to_date, shown) = (up_to_date.expect("caught up"), shown.expect("shown"));
    assert!(
        up_to_date < shown,
        "shown at {shown}, caught up at {up_to_date}"
    );
    assert!(report_of(&all).expect("a report").decodes > 0);
    all
}

/// A track that falls behind stops its previews.
#[test]
fn live_preview_a_track_falling_behind_turns_them_off() {
    let scene = scene_of(27);
    let seen = Arc::new(Seen::default());
    let dir = TempDir::new();
    let svc = services(&dir);
    // The first real decode stays in progress until the whole replay has queued up behind it.
    let (source, handles) = super::queue_tests::source_of(&scene);
    let open = Arc::new(AtomicBool::new(false));
    let session = start_live(
        source,
        svc.clone(),
        config_with(Some(LivePreviews {
            fell_behind: LivePreviews::new(loader(&seen, marker_words)).fell_behind,
            ..previews(loader(&seen, marker_words))
        })),
        Some(gated_factory(Arc::clone(&open))),
        Box::new(ChaCha8Rng::seed_from_u64(323)),
    )
    .unwrap();
    let limit = Instant::now() + GUARD;
    while !handles.delivered.load(Ordering::SeqCst) {
        assert!(Instant::now() < limit, "the replay never ended");
        std::thread::sleep(Duration::from_millis(1));
    }
    // The session looks every pump: a wide margin for it to see the queue.
    std::thread::sleep(Duration::from_millis(500));
    open.store(true, Ordering::SeqCst);
    handles.released.store(true, Ordering::SeqCst);
    let (events, _) = collect(session);
    let report = report_of(&events).expect("a report");
    assert_eq!(report.off.as_deref(), Some(FELL_BEHIND_REASON));
}

/// Which computers run previews (`docs/live-previews.md`).
#[test]
fn live_preview_runs_only_where_it_was_measured_and_fits() {
    use crate::backends::{PREVIEW_MIN_CARD_MEMORY, PREVIEW_MIN_MEMORY, PREVIEW_MIN_MEMORY_CUDA};
    use crate::preview_settings::PreviewChoice;
    use earshot_hardware::{Gpu, Hardware};
    let previews_refused = |kind, machine: &Hardware| {
        crate::backends::previews_refused(kind, machine, PreviewChoice::Automatic)
    };
    let machine = |memory: Option<u64>, card: Option<u64>| Hardware {
        total_memory: memory,
        gpus: vec![Gpu {
            name: "A card".to_owned(),
            vendor_id: 0x10de,
            dedicated_memory: card,
            driver_version: None,
            integrated: Some(false),
            compute_capability: None,
        }],
        ..Hardware::default()
    };
    let big = machine(Some(PREVIEW_MIN_MEMORY_CUDA), Some(PREVIEW_MIN_CARD_MEMORY));
    assert_eq!(previews_refused(BackendKind::Cpu, &big), None);
    assert_eq!(previews_refused(BackendKind::Cuda, &big), None);
    // T12.12c: the NPU runs them (its rule's tests are in `backends_tests`); whisper-only backends
    // do not.
    assert_eq!(previews_refused(BackendKind::Qnn, &big), None);
    for kind in [BackendKind::Vulkan, BackendKind::Fake] {
        assert!(previews_refused(kind, &big).is_some(), "{kind:?}");
    }
    let small = machine(Some(PREVIEW_MIN_MEMORY - 1), Some(PREVIEW_MIN_CARD_MEMORY));
    assert!(previews_refused(BackendKind::Cpu, &small).is_some());
    assert!(previews_refused(BackendKind::Cuda, &small).is_some());
    let below_cuda = machine(
        Some(PREVIEW_MIN_MEMORY_CUDA - 1),
        Some(PREVIEW_MIN_CARD_MEMORY),
    );
    assert_eq!(previews_refused(BackendKind::Cpu, &below_cuda), None);
    assert!(previews_refused(BackendKind::Cuda, &below_cuda).is_some());
    assert!(previews_refused(BackendKind::Cpu, &machine(None, None)).is_some());
    let card = machine(
        Some(PREVIEW_MIN_MEMORY_CUDA),
        Some(PREVIEW_MIN_CARD_MEMORY - 1),
    );
    assert_eq!(previews_refused(BackendKind::Cpu, &card), None);
    // T12.3c: a card too small (or unknown) moves a CUDA track's previews to the CPU, under the
    // CPU's memory limit, instead of refusing them.
    let preview_backend = |kind, machine: &Hardware| {
        crate::backends::preview_backend(kind, machine, PreviewChoice::Automatic)
    };
    assert_eq!(previews_refused(BackendKind::Cuda, &card), None);
    assert_eq!(
        preview_backend(BackendKind::Cuda, &card),
        Ok(BackendKind::Cpu)
    );
    let unknown_card = machine(Some(PREVIEW_MIN_MEMORY_CUDA), None);
    assert_eq!(
        preview_backend(BackendKind::Cuda, &unknown_card),
        Ok(BackendKind::Cpu)
    );
    let cpu_limit = machine(Some(PREVIEW_MIN_MEMORY), Some(PREVIEW_MIN_CARD_MEMORY - 1));
    assert_eq!(
        preview_backend(BackendKind::Cuda, &cpu_limit),
        Ok(BackendKind::Cpu)
    );
    let below_cpu = machine(
        Some(PREVIEW_MIN_MEMORY - 1),
        Some(PREVIEW_MIN_CARD_MEMORY - 1),
    );
    assert_eq!(
        previews_refused(BackendKind::Cuda, &below_cpu),
        previews_refused(BackendKind::Cpu, &below_cpu)
    );
    assert!(previews_refused(BackendKind::Cuda, &below_cpu).is_some());
    assert_eq!(
        preview_backend(BackendKind::Cuda, &big),
        Ok(BackendKind::Cuda)
    );
    assert_eq!(
        preview_backend(BackendKind::Cpu, &big),
        Ok(BackendKind::Cpu)
    );
}

const GIB: u64 = 1024 * 1024 * 1024;

/// An NPU machine (the Surface's kind): `memory` of system memory, an NPU, no graphics card.
fn npu_machine(memory: Option<u64>) -> earshot_hardware::Hardware {
    earshot_hardware::Hardware {
        total_memory: memory,
        npus: vec![earshot_hardware::Npu {
            name: "A test NPU".to_owned(),
            driver_version: Some("1.0.0.0".to_owned()),
        }],
        ..earshot_hardware::Hardware::default()
    }
}

/// A first-session loader deciding through the app's rule (`backends::load_preview_with`) on
/// `machine`, recording the backend each load is asked for, loading a [`fake_session_on`].
fn npu_rule_loader(
    seen: &Arc<Seen>,
    asked: &Arc<Mutex<Vec<BackendKind>>>,
    machine: earshot_hardware::Hardware,
    choice: crate::preview_settings::PreviewChoice,
) -> PreviewLoader {
    let (seen, asked) = (Arc::clone(seen), Arc::clone(asked));
    Arc::new(move |backend: &BackendInfo| {
        crate::backends::load_preview_with(backend, &machine, choice, |on| {
            asked.lock().unwrap().push(on.kind);
            Ok(fake_session_on(&seen, on))
        })
    })
}

/// [`npu_rule_loader`] for the user's own voice (`backends::load_preview_second_with`).
fn npu_rule_own_voice(
    seen: &Arc<Seen>,
    asked: &Arc<Mutex<Vec<BackendKind>>>,
    machine: earshot_hardware::Hardware,
) -> PreviewLoader {
    use crate::preview_settings::PreviewChoice;
    let (seen, asked) = (Arc::clone(seen), Arc::clone(asked));
    Arc::new(move |backend: &BackendInfo| {
        crate::backends::load_preview_second_with(
            backend,
            &machine,
            PreviewChoice::Everyone,
            |on| {
                asked.lock().unwrap().push(on.kind);
                Ok(fake_session_on(&seen, on))
            },
        )
    })
}

/// One meeting with both tracks on the NPU and `previews`; its events, what it stored, and the same
/// meeting's store without previews.
fn npu_run(
    scene: &Scene,
    previews: LivePreviews,
    power: Option<PowerSupply>,
    seed: u64,
) -> (Vec<RuntimeEvent>, Stored, Stored) {
    let run = |previews: Option<LivePreviews>| {
        let dir = TempDir::new();
        let svc = services(&dir);
        let mut config = config_with(previews);
        if let Some(supply) = power {
            config.power = PowerProbe::new(Fixed(supply));
        }
        let session = start_live(
            fast_source(scene),
            svc.clone(),
            config,
            Some(factory_on(scene, BackendKind::Qnn)),
            Box::new(ChaCha8Rng::seed_from_u64(seed)),
        )
        .unwrap();
        let meeting = session.meeting();
        let (events, _) = collect(session);
        (events, stored(&svc, meeting))
    };
    let (events, with) = run(Some(previews));
    let (_, without) = run(None);
    (events, with, without)
}

/// T12.12c: a meeting on the NPU of a machine above the limit shows the far end's provisional words
/// from a session on the NPU, refuses the user's own voice with its reason under Everyone, and
/// stores exactly what it stores without previews.
#[test]
fn live_preview_on_the_npu_runs_for_the_far_end_and_refuses_your_own_voice() {
    let scene = scene_of(27);
    let (far, own) = (Arc::new(Seen::default()), Arc::new(Seen::default()));
    let (far_asked, own_asked) = (
        Arc::new(Mutex::new(Vec::new())),
        Arc::new(Mutex::new(Vec::new())),
    );
    let surface = npu_machine(Some(15 * GIB));
    let config = LivePreviews {
        own_voice: Some(npu_rule_own_voice(&own, &own_asked, surface.clone())),
        ..previews(npu_rule_loader(
            &far,
            &far_asked,
            surface,
            crate::preview_settings::PreviewChoice::Everyone,
        ))
    };
    let (events, with, without) = npu_run(&scene, config, None, 351);
    assert_eq!(*far_asked.lock().unwrap(), vec![BackendKind::Qnn]);
    assert!(own_asked.lock().unwrap().is_empty(), "own voice loaded");
    assert_eq!(own.calls.load(Ordering::SeqCst), 0);
    let far_report = report_for(&events, TrackId::Reference);
    assert_eq!(far_report.backend, Some(BackendKind::Qnn), "{far_report:?}");
    assert_eq!(far_report.off, None, "{far_report:?}");
    assert!(far_report.decodes > 0, "{far_report:?}");
    assert!(shown_on(&events, TrackId::Reference));
    let mic = report_for(&events, TrackId::Mic);
    assert_eq!(
        mic.off.as_deref(),
        Some(crate::backends::OWN_VOICE_NPU_REFUSED),
        "{mic:?}"
    );
    assert_eq!(mic.backend, None, "{mic:?}");
    assert!(!shown_on(&events, TrackId::Mic));
    assert!(warnings(&events).is_empty(), "{:?}", warnings(&events));
    assert_eq!(with, without);
}

/// T12.12c: below the NPU's limit a meeting has no previews by default, and loads nothing.
#[test]
fn live_preview_on_the_npu_below_the_limit_is_off_by_default_and_loads_nothing() {
    let scene = scene_of(27);
    let seen = Arc::new(Seen::default());
    let asked = Arc::new(Mutex::new(Vec::new()));
    let small = npu_machine(Some(crate::backends::PREVIEW_MIN_MEMORY_NPU - 1));
    let load = npu_rule_loader(
        &seen,
        &asked,
        small,
        crate::preview_settings::PreviewChoice::Automatic,
    );
    let (events, with, without) = npu_run(&scene, previews(load), None, 355);
    assert!(asked.lock().unwrap().is_empty());
    let report = report_of(&events).expect("a report");
    assert_eq!(
        report.off.as_deref(),
        Some("not enough memory for a second session of the model"),
        "{report:?}"
    );
    assert!(previews_of(&events).is_empty());
    assert_eq!(with, without);
}

/// T12.12c: on battery a meeting on the NPU has no previews, and nothing is loaded for them.
#[test]
fn live_preview_on_the_npu_is_off_on_battery_and_loads_nothing() {
    let scene = scene_of(27);
    let seen = Arc::new(Seen::default());
    let asked = Arc::new(Mutex::new(Vec::new()));
    let load = npu_rule_loader(
        &seen,
        &asked,
        npu_machine(Some(15 * GIB)),
        crate::preview_settings::PreviewChoice::On,
    );
    let (events, with, without) = npu_run(&scene, previews(load), Some(PowerSupply::Battery), 357);
    assert!(asked.lock().unwrap().is_empty());
    assert_eq!(seen.calls.load(Ordering::SeqCst), 0);
    let report = report_of(&events).expect("a report");
    assert_eq!(report.off.as_deref(), Some(ON_BATTERY_REASON));
    assert!(previews_of(&events).is_empty());
    assert_eq!(with, without);
}

/// T12.12c: a Parakeet model whose NPU version is not installed: the app's real loader, asked for
/// the NPU, fails to load (no NPU files here) and the meeting goes on without previews, storing what
/// it stores without them. A failed preview load is never a failed meeting.
#[test]
fn live_preview_on_the_npu_without_the_npu_version_costs_the_meeting_nothing() {
    let scene = scene_of(27);
    let empty = TempDir::new();
    let models = crate::backends::ModelDir::new(&empty.0);
    let load: PreviewLoader = Arc::new(move |backend: &BackendInfo| {
        crate::backends::load_preview(&models, backend, crate::preview_settings::PreviewChoice::On)
    });
    let (events, with, without) = npu_run(&scene, previews(load), None, 359);
    let report = report_of(&events).expect("a report");
    let why = report.off.clone().expect("no previews");
    assert!(
        !why.contains("memory") && !why.contains("Settings") && !why.contains("battery"),
        "refused instead of failing to load: {why}"
    );
    assert_eq!(report.backend, None, "{report:?}");
    assert!(previews_of(&events).is_empty());
    assert!(warnings(&events).is_empty(), "{:?}", warnings(&events));
    assert_eq!(with, without);
}

/// What the fake streaming model was given.
#[derive(Default)]
struct Heard {
    /// Samples pushed since the start, over every restart.
    samples: AtomicUsize,
    restarts: AtomicUsize,
}

/// A streaming model that finishes a `MARKER` word at the end of every half second of audio it
/// is given (word `k` from `500k + 400` to `500k + 500` ms).
struct FakeStream {
    heard: Arc<Heard>,
    /// Samples since the last restart.
    position: usize,
    words: Vec<Word>,
    /// Each push that finishes a word takes at least this long.
    slow: Duration,
}

impl crate::live::StreamingPreview for FakeStream {
    fn backend_info(&self) -> BackendInfo {
        BackendInfo {
            kind: BackendKind::Fake,
            device: "fake".to_owned(),
            model: "fake-stream".to_owned(),
        }
    }

    fn restart(&mut self) {
        self.heard.restarts.fetch_add(1, Ordering::SeqCst);
        self.position = 0;
        self.words.clear();
    }

    fn push(&mut self, samples: &[earshot_core::audio::Sample]) -> Result<u64, String> {
        self.heard
            .samples
            .fetch_add(samples.len(), Ordering::SeqCst);
        let before = self.position / 8000;
        self.position += samples.len();
        let after = self.position / 8000;
        if after > before {
            std::thread::sleep(self.slow);
        }
        // Each word ends where the half second that finished it ends: at the newest audio.
        for k in before..after {
            let end = (k as u64 + 1) * 500;
            self.words
                .push(Word::new(format!("{MARKER}{k}"), end - 100, end, 0.9));
        }
        Ok((after - before) as u64)
    }

    fn finished_words(&self) -> Vec<Word> {
        self.words.clone()
    }

    fn forget_before(&mut self, time: MeetingTime) {
        self.words.retain(|w| w.start >= time);
    }
}

fn fake_stream(heard: &Arc<Heard>) -> FakeStream {
    FakeStream {
        heard: Arc::clone(heard),
        position: 0,
        words: Vec::new(),
        slow: Duration::ZERO,
    }
}

fn stream_loader(heard: &Arc<Heard>) -> crate::live::StreamLoader {
    slow_stream_loader(heard, Duration::ZERO)
}

fn slow_stream_loader(heard: &Arc<Heard>, slow: Duration) -> crate::live::StreamLoader {
    let heard = Arc::clone(heard);
    Arc::new(move |_| {
        Ok(Box::new(FakeStream {
            heard: Arc::clone(&heard),
            position: 0,
            words: Vec::new(),
            slow,
        }) as Box<dyn crate::live::StreamingPreview + Send>)
    })
}

/// T12.23 (evaluation only): a streaming preview is display-only like a window one. The words
/// stored are the same with and without it, its words reach the screen, every sample goes to it
/// once with no restart, and the window session is never loaded.
#[test]
fn live_preview_stream_shows_words_hears_each_sample_once_and_stores_the_same() {
    let scene = scene_of(27);
    let (without, ..) = run_with(&scene, None, 341);
    let seen = Arc::new(Seen::default());
    let heard = Arc::new(Heard::default());
    let config = LivePreviews {
        stream: Some(stream_loader(&heard)),
        ..previews(loader(&seen, marker_words))
    };
    let (with, events, ..) = run_with(&scene, Some(config), 341);
    assert_eq!(with, without);
    let report = report_of(&events).expect("a report");
    assert_eq!(report.off, None);
    assert_eq!(report.track, Some(TrackId::Reference));
    assert_eq!(report.backend, Some(BackendKind::Fake));
    assert!(report.decodes > 1 && report.words_shown > 1, "{report:?}");
    assert!(shown_on(&events, TrackId::Reference), "{report:?}");
    assert!(!shown_on(&events, TrackId::Mic));
    assert_eq!(seen.calls.load(Ordering::SeqCst), 0, "no window decode");
    let pushed = heard.samples.load(Ordering::SeqCst);
    assert_eq!(heard.restarts.load(Ordering::SeqCst), 0);
    assert!(pushed > 0 && pushed <= scene.them.len(), "{pushed}");
    #[allow(clippy::cast_precision_loss)]
    let pushed_s = pushed as f64 / 16_000.0;
    assert!((report.audio_s - pushed_s).abs() < 1e-6, "{report:?}");
}

/// T12.23: no show of a streaming preview holds a word settled text already covered, the last
/// show is empty, and no preview word reaches the store. (Words the model finished in silence
/// after a stretch's last word leave once the worker has decoded past them, which can be before
/// settled text reaches them: `live_lag` counts those as `vanished` for the real model.)
#[test]
fn live_preview_stream_words_leave_when_settled_and_never_reach_the_store() {
    let scene = scene_of(40);
    let seen = Arc::new(Seen::default());
    let heard = Arc::new(Heard::default());
    let config = LivePreviews {
        stream: Some(stream_loader(&heard)),
        ..previews(loader(&seen, marker_words))
    };
    let (with, events, ..) = run_with(&scene, Some(config), 343);
    assert!(
        with.words
            .iter()
            .all(|(_, text, ..)| !text.contains(MARKER)),
        "a preview word was stored"
    );
    assert!(shown_on(&events, TrackId::Reference));
    let mut settled = MeetingTime::ZERO;
    for event in &events {
        match event {
            RuntimeEvent::Transcript {
                track: TrackId::Reference,
                event: TranscriptEvent::Final(s),
            } => settled = settled.max(s.end),
            RuntimeEvent::LivePreview { words, .. } => {
                assert!(
                    words.iter().all(|w| w.start + COVERED_SLACK >= settled),
                    "a shown word behind settled text at {settled:?}: {words:?}"
                );
            }
            _ => {}
        }
    }
    let last = previews_of(&events)
        .into_iter()
        .rev()
        .find(|(t, _)| *t == TrackId::Reference)
        .expect("a show");
    assert!(
        last.1.is_empty(),
        "previews end with nothing shown: {last:?}"
    );
}

fn texts_at(words: &[Word]) -> Vec<(String, u64)> {
    words
        .iter()
        .map(|w| (w.text.clone(), w.start.as_millis()))
        .collect()
}

/// T12.23 (review finding): a paused streaming preview still lets settled text take covered
/// words away, and starts its model over once; the words it had finished stay until covered.
#[test]
fn live_preview_stream_covered_words_leave_during_a_pause() {
    let heard = Arc::new(Heard::default());
    let mut feed = StreamFeed::new(Box::new(fake_stream(&heard)));
    feed.audio(0, &vec![0.0; 16_000]);
    let audio = feed.take_pending().expect("audio");
    assert_eq!(feed.push(&audio).unwrap(), 2);
    let (shown, added) = feed.show(MeetingTime::ZERO).expect("words");
    assert_eq!(added, 2);
    assert_eq!(
        texts_at(&shown),
        [(format!("{MARKER}0"), 400), (format!("{MARKER}1"), 900)]
    );
    feed.pause();
    feed.pause();
    assert_eq!(heard.restarts.load(Ordering::SeqCst), 1, "starts over once");
    assert_eq!(feed.take_pending(), None);
    assert_eq!(feed.show(MeetingTime::ZERO), None, "nothing changed yet");
    let (shown, added) = feed.show(MeetingTime::from_millis(800)).expect("a change");
    assert_eq!(added, 0);
    assert_eq!(texts_at(&shown), [(format!("{MARKER}1"), 900)]);
    let (shown, _) = feed
        .show(MeetingTime::from_millis(1_100))
        .expect("a change");
    assert!(shown.is_empty());
}

/// T12.23 (review finding): audio after a gap starts the model over at the packet after it; what
/// was pending before the gap is dropped, the finished words stay, and no word spans the gap.
#[test]
fn live_preview_stream_a_gap_starts_the_model_over() {
    let heard = Arc::new(Heard::default());
    let mut feed = StreamFeed::new(Box::new(fake_stream(&heard)));
    feed.audio(0, &vec![0.0; 12_000]);
    let audio = feed.take_pending().unwrap();
    feed.push(&audio).unwrap();
    // Pending, never pushed: lost with the gap.
    feed.audio(12_000, &vec![0.0; 1_000]);
    // Frames 13,000 to 14,000 never arrive.
    feed.audio(14_000, &vec![0.0; 2_000]);
    assert_eq!(heard.restarts.load(Ordering::SeqCst), 1);
    assert_eq!(feed.take_pending().map(|a| a.len()), Some(2_000));
    feed.push(&vec![0.0; 2_000]).unwrap();
    feed.audio(16_000, &vec![0.0; 14_000]);
    let audio = feed.take_pending().unwrap();
    feed.push(&audio).unwrap();
    assert_eq!(heard.restarts.load(Ordering::SeqCst), 1);
    assert_eq!(
        heard.samples.load(Ordering::SeqCst),
        12_000 + 2_000 + 14_000
    );
    let (shown, _) = feed.show(MeetingTime::ZERO).expect("words");
    // Frame 14,000 is 875 ms: the new run's words are timed from there.
    assert_eq!(
        texts_at(&shown),
        [
            (format!("{MARKER}0"), 400),
            (format!("{MARKER}0"), 1_275),
            (format!("{MARKER}1"), 1_775)
        ]
    );
    let (gap_start, gap_end) = (MeetingTime::from_millis(750), MeetingTime::from_millis(875));
    assert!(
        shown
            .iter()
            .all(|w| w.end <= gap_start || w.start >= gap_end),
        "a word spans the gap: {shown:?}"
    );
}

/// T12.23 (review finding): in a session, previews paused for slowness (here for the rest of the
/// meeting, after their first words) still take covered words off the screen. At the end of the
/// live transcript the screen holds no word that settled text covered before its last final (the
/// last one may not have reached the thread yet). Without the fix the last words shown before
/// the pause stayed (checked by making the paused loop skip the show: this test then fails).
#[test]
fn live_preview_stream_paused_for_slowness_still_clears_covered_words() {
    let scene = scene_of(60);
    let seen = Arc::new(Seen::default());
    let heard = Arc::new(Heard::default());
    let config = LivePreviews {
        stream: Some(slow_stream_loader(&heard, Duration::from_millis(20))),
        slow: Duration::from_millis(4),
        slow_pause: Duration::from_secs(600),
        ..previews(loader(&seen, marker_words))
    };
    let (_, events, ..) = run_with(&scene, Some(config), 345);
    let report = report_of(&events).expect("a report");
    assert_eq!(report.slow_pauses, 1, "{report:?}");
    let mut ends = Vec::new();
    let mut last: Vec<Word> = Vec::new();
    let mut was_shown = false;
    for event in &events {
        match event {
            RuntimeEvent::LiveTranscriptFinished { .. } => break,
            RuntimeEvent::Transcript {
                track: TrackId::Reference,
                event: TranscriptEvent::Final(s),
            } => ends.push(s.end),
            RuntimeEvent::LivePreview {
                track: TrackId::Reference,
                words,
            } => {
                was_shown |= !words.is_empty();
                last.clone_from(words);
            }
            _ => {}
        }
    }
    assert!(was_shown, "{report:?}");
    ends.pop();
    let settled = ends.into_iter().max().expect("finals before the last");
    assert!(
        settled > MeetingTime::from_millis(5_000),
        "the meeting settled too little to judge: {settled:?}"
    );
    assert!(
        last.iter().all(|w| w.start + COVERED_SLACK >= settled),
        "covered words still shown at the end, settled to {settled:?}: {last:?}"
    );
}

/// T12.33: what each track said of its previews, in order.
fn states_of(events: &[RuntimeEvent]) -> Vec<(TrackId, Option<PreviewOffReason>)> {
    events
        .iter()
        .filter_map(|e| match e {
            RuntimeEvent::LivePreviewState { track, off } => Some((*track, *off)),
            _ => None,
        })
        .collect()
}

fn position(events: &[RuntimeEvent], pred: impl Fn(&RuntimeEvent) -> bool) -> usize {
    events.iter().position(pred).expect("the event was sent")
}

/// T12.33: previews that run say so once, when their model is loaded and before any word; a
/// session without previews, and the mic in a call nobody asked previews for, say nothing.
#[test]
fn live_preview_state_says_running_once_and_only_for_a_track_with_previews() {
    let scene = scene_of(27);
    let (_, events, ..) = run_with(&scene, None, 361);
    assert!(states_of(&events).is_empty(), "no previews, nothing said");
    let seen = Arc::new(Seen::default());
    let (_, events, ..) = run_with(&scene, Some(previews(loader(&seen, marker_words))), 361);
    assert_eq!(states_of(&events), vec![(TrackId::Reference, None)]);
    let said = position(&events, |e| {
        matches!(e, RuntimeEvent::LivePreviewState { .. })
    });
    let shown = position(
        &events,
        |e| matches!(e, RuntimeEvent::LivePreview { words, .. } if !words.is_empty()),
    );
    assert!(said < shown, "said at {said}, words at {shown}");
    assert_eq!(report_of(&events).expect("a report").reason, None);
}

/// T12.33: on battery the reason is said once, as a kind, while the meeting records (before its
/// recording is saved), never only in the report at the end.
#[test]
fn live_preview_on_battery_says_why_once_during_the_meeting() {
    let scene = scene_of(27);
    let seen = Arc::new(Seen::default());
    let dir = TempDir::new();
    let svc = services(&dir);
    let config = LiveConfig {
        power: PowerProbe::new(Fixed(PowerSupply::Battery)),
        ..config_with(Some(previews(loader(&seen, marker_words))))
    };
    let session = start_live(
        fast_source(&scene),
        svc,
        config,
        Some(factory_of(&scene, Arc::default())),
        Box::new(ChaCha8Rng::seed_from_u64(363)),
    )
    .unwrap();
    let (events, _) = collect(session);
    assert_eq!(
        states_of(&events),
        vec![(TrackId::Reference, Some(PreviewOffReason::OnBattery))]
    );
    let said = position(&events, |e| {
        matches!(e, RuntimeEvent::LivePreviewState { .. })
    });
    let saved = position(&events, |e| {
        matches!(e, RuntimeEvent::RecordingSaved { .. })
    });
    let first_words = position(&events, |e| {
        matches!(
            e,
            RuntimeEvent::Transcript {
                event: TranscriptEvent::Final(_),
                ..
            }
        )
    });
    assert!(said < saved, "said at {said}, saved at {saved}");
    assert!(said < first_words, "said at {said}, words at {first_words}");
    let report = report_of(&events).expect("a report");
    assert_eq!(report.reason, Some(PreviewOffReason::OnBattery));
    assert_eq!(report.off.as_deref(), Some(ON_BATTERY_REASON));
}

/// T12.33: during a hold and its catch-up the track says it is waiting, then that previews run
/// once the transcript is up to date: the wait is taken back.
#[test]
fn live_preview_state_waits_for_the_catch_up_then_takes_it_back() {
    let scene = scene_of(27);
    let seen = Arc::new(Seen::default());
    let events = held_run(
        &scene,
        previews(loader(&seen, marker_words)),
        None,
        365,
        true,
    );
    assert_eq!(
        states_of(&events),
        vec![
            (TrackId::Reference, Some(PreviewOffReason::CatchingUp)),
            (TrackId::Reference, None),
        ]
    );
    let state = |want: Option<PreviewOffReason>| {
        position(
            &events,
            |e| matches!(e, RuntimeEvent::LivePreviewState { off, .. } if *off == want),
        )
    };
    let up_to_date = position(&events, |e| {
        matches!(
            e,
            RuntimeEvent::LiveTranscript(LiveTranscriptState::UpToDate)
        )
    });
    assert!(state(Some(PreviewOffReason::CatchingUp)) < up_to_date);
    assert!(up_to_date < state(None));
}

/// T12.33: a held meeting on battery never says it waits for the catch-up, because previews will
/// not start after it: it says the battery, once.
#[test]
fn live_preview_a_held_meeting_on_battery_says_only_the_battery() {
    let scene = scene_of(27);
    let seen = Arc::new(Seen::default());
    let gates = held_gates();
    let dir = TempDir::new();
    let svc = services(&dir);
    let config = LiveConfig {
        previews: Some(previews(loader(&seen, marker_words))),
        power: PowerProbe::new(Fixed(PowerSupply::Battery)),
        ..held_config(gates.clone())
    };
    let mut session = start_live(
        fast_source(&scene),
        svc,
        config,
        Some(factory_of(&scene, Arc::default())),
        Box::new(ChaCha8Rng::seed_from_u64(367)),
    )
    .unwrap();
    let events = session.take_events().unwrap();
    let mut all = Vec::new();
    for event in &events {
        if matches!(
            event,
            RuntimeEvent::LiveTranscript(LiveTranscriptState::Held(_))
        ) {
            gates.transcription.store(true, Ordering::SeqCst);
        }
        let ended = matches!(event, RuntimeEvent::Ended(_));
        all.push(event);
        if ended {
            break;
        }
    }
    session.wait().unwrap();
    assert_eq!(
        states_of(&all),
        vec![(TrackId::Reference, Some(PreviewOffReason::OnBattery))]
    );
    assert_eq!(seen.calls.load(Ordering::SeqCst), 0);
}

/// T12.33: a track that falls behind says previews ran, then why they stopped.
#[test]
fn live_preview_a_track_falling_behind_says_why() {
    let scene = scene_of(27);
    let seen = Arc::new(Seen::default());
    let dir = TempDir::new();
    let svc = services(&dir);
    let (source, handles) = super::queue_tests::source_of(&scene);
    let open = Arc::new(AtomicBool::new(false));
    let session = start_live(
        source,
        svc,
        config_with(Some(LivePreviews {
            fell_behind: LivePreviews::new(loader(&seen, marker_words)).fell_behind,
            ..previews(loader(&seen, marker_words))
        })),
        Some(gated_factory(Arc::clone(&open))),
        Box::new(ChaCha8Rng::seed_from_u64(369)),
    )
    .unwrap();
    let limit = Instant::now() + GUARD;
    while !handles.delivered.load(Ordering::SeqCst) {
        assert!(Instant::now() < limit, "the replay never ended");
        std::thread::sleep(Duration::from_millis(1));
    }
    std::thread::sleep(Duration::from_millis(500));
    open.store(true, Ordering::SeqCst);
    handles.released.store(true, Ordering::SeqCst);
    let (events, _) = collect(session);
    let states = states_of(&events);
    assert_eq!(
        states.last(),
        Some(&(TrackId::Reference, Some(PreviewOffReason::FellBehind))),
        "{states:?}"
    );
    // Once per change: never the same thing twice in a row, and nothing after the turn-off.
    assert!(states.windows(2).all(|w| w[0] != w[1]), "{states:?}");
    assert_eq!(
        states
            .iter()
            .filter(|(_, off)| *off == Some(PreviewOffReason::FellBehind))
            .count(),
        1,
        "{states:?}"
    );
    let report = report_of(&events).expect("a report");
    assert_eq!(report.reason, Some(PreviewOffReason::FellBehind));
}

/// T12.33: under Everyone beside the NPU the far end's previews run and the user's own voice
/// says its refusal once, as its own kind.
#[test]
fn live_preview_on_the_npu_own_voice_says_its_refusal_once() {
    let scene = scene_of(27);
    let (far, own) = (Arc::new(Seen::default()), Arc::new(Seen::default()));
    let (far_asked, own_asked) = (
        Arc::new(Mutex::new(Vec::new())),
        Arc::new(Mutex::new(Vec::new())),
    );
    let surface = npu_machine(Some(15 * GIB));
    let config = LivePreviews {
        own_voice: Some(npu_rule_own_voice(&own, &own_asked, surface.clone())),
        ..previews(npu_rule_loader(
            &far,
            &far_asked,
            surface,
            crate::preview_settings::PreviewChoice::Everyone,
        ))
    };
    let (events, ..) = npu_run(&scene, config, None, 371);
    let of = |track: TrackId| -> Vec<Option<PreviewOffReason>> {
        states_of(&events)
            .into_iter()
            .filter(|(t, _)| *t == track)
            .map(|(_, off)| off)
            .collect()
    };
    assert_eq!(of(TrackId::Reference), vec![None]);
    assert_eq!(
        of(TrackId::Mic),
        vec![Some(PreviewOffReason::OwnVoiceOnNpu)]
    );
    let mic = report_for(&events, TrackId::Mic);
    assert_eq!(mic.reason, Some(PreviewOffReason::OwnVoiceOnNpu));
}

/// T12.33: a load that fails and a chunker session each say their kind.
#[test]
fn live_preview_a_failed_load_and_a_chunker_session_say_their_kind() {
    let scene = scene_of(27);
    let load: PreviewLoader = Arc::new(|_| Err("not on this backend".into()));
    let (_, events, ..) = run_with(&scene, Some(previews(load)), 373);
    assert_eq!(
        states_of(&events),
        vec![(TrackId::Reference, Some(PreviewOffReason::LoadFailed))]
    );
    let seen = Arc::new(Seen::default());
    let dir = TempDir::new();
    let svc = services(&dir);
    let config = LiveConfig {
        transcription: LiveTranscription::Chunker,
        ..config_with(Some(previews(loader(&seen, marker_words))))
    };
    let session = start_live(
        fast_source(&scene),
        svc,
        config,
        Some(factory_of(&scene, Arc::default())),
        Box::new(ChaCha8Rng::seed_from_u64(375)),
    )
    .unwrap();
    let (events, _) = collect(session);
    assert_eq!(
        states_of(&events),
        vec![(TrackId::Reference, Some(PreviewOffReason::NotApplicable))]
    );
    assert_eq!(seen.calls.load(Ordering::SeqCst), 0);
}

/// T12.33: every refusal of the app's rule and loader carries its kind beside the text the
/// string forms give, so a host never reads the text to know what happened.
#[test]
fn live_preview_refusals_carry_their_kind_beside_the_same_text() {
    use crate::backends::{
        OWN_VOICE_NPU_REFUSED, PREVIEW_BOTH_MIN_MEMORY, PREVIEW_MIN_MEMORY, PREVIEW_NEEDS_COMPILED,
        preview_backend, preview_backend_or_refusal, preview_load_refusal, second_preview_backend,
        second_preview_backend_or_refusal,
    };
    use crate::preview_settings::PreviewChoice;
    use earshot_hardware::Hardware;
    let machine = |memory: u64| Hardware {
        total_memory: Some(memory),
        ..Hardware::default()
    };
    let (big, small) = (machine(64 * GIB), machine(PREVIEW_MIN_MEMORY - 1));
    let first = |kind, hw: &Hardware, choice| {
        let typed = preview_backend_or_refusal(kind, hw, choice);
        assert_eq!(
            typed.clone().map_err(|r| r.text),
            preview_backend(kind, hw, choice)
        );
        typed.err().map(|r| r.reason)
    };
    let second = |kind, hw: &Hardware, choice| {
        let typed = second_preview_backend_or_refusal(kind, hw, choice);
        assert_eq!(
            typed.clone().map_err(|r| r.text),
            second_preview_backend(kind, hw, choice)
        );
        typed.err().map(|r| r.reason)
    };
    for kind in [BackendKind::Cpu, BackendKind::Cuda, BackendKind::Qnn] {
        assert_eq!(
            first(kind, &big, PreviewChoice::Off),
            Some(PreviewOffReason::TurnedOff)
        );
        assert_eq!(first(kind, &big, PreviewChoice::On), None);
        // The memory limit is the automatic choice's alone: nobody asked for previews there.
        assert_eq!(
            first(kind, &small, PreviewChoice::Automatic),
            Some(PreviewOffReason::OffByDefault)
        );
        assert_eq!(first(kind, &small, PreviewChoice::On), None);
        for choice in [PreviewChoice::Automatic, PreviewChoice::On] {
            assert_eq!(second(kind, &big, choice), Some(PreviewOffReason::NotAsked));
        }
    }
    assert_eq!(
        first(BackendKind::Vulkan, &big, PreviewChoice::On),
        Some(PreviewOffReason::Model)
    );
    assert_eq!(
        second(BackendKind::Vulkan, &big, PreviewChoice::Everyone),
        Some(PreviewOffReason::Model)
    );
    assert_eq!(
        second(BackendKind::Cpu, &big, PreviewChoice::Everyone),
        None
    );
    assert_eq!(
        second(BackendKind::Qnn, &big, PreviewChoice::Everyone),
        Some(PreviewOffReason::OwnVoiceOnNpu)
    );
    // The user asked for their own voice too, and the machine has no room for it.
    assert_eq!(
        second(
            BackendKind::Cpu,
            &machine(PREVIEW_BOTH_MIN_MEMORY - 1),
            PreviewChoice::Everyone
        ),
        Some(PreviewOffReason::NotEnoughMemory)
    );
    // The loader: the NPU's own refusal, as `qnn_load_steps` makes it, and any other failure.
    let needs = preview_load_refusal(&AsrError::Backend(PREVIEW_NEEDS_COMPILED.to_owned()));
    assert_eq!(needs.reason, PreviewOffReason::NeedsCompiledModel);
    assert!(needs.contains(PREVIEW_NEEDS_COMPILED), "{needs}");
    let other = preview_load_refusal(&AsrError::Backend("a file is missing".to_owned()));
    assert_eq!(other.reason, PreviewOffReason::LoadFailed);
    assert!(other.contains("a file is missing"), "{other}");
    assert_ne!(OWN_VOICE_NPU_REFUSED, PREVIEW_NEEDS_COMPILED);
    // Plain words from a loader are a load that failed.
    assert_eq!(
        PreviewRefusal::from("no session").reason,
        PreviewOffReason::LoadFailed
    );
}

/// A held meeting of `scene` with `previews`, released at its first `Held`; every event. With
/// `catch_up_gate` the first real decode after the hold stays in progress, so the catch-up
/// lasts, until the session has said its previews wait for it (a rendezvous, never a sleep;
/// `GUARD` opens it in a failing case, so the test fails instead of hanging).
fn held_run(
    scene: &Scene,
    previews: LivePreviews,
    power: Option<PowerSupply>,
    seed: u64,
    catch_up_gate: bool,
) -> Vec<RuntimeEvent> {
    held_run_on(
        scene,
        previews,
        |config| {
            if let Some(supply) = power {
                config.power = PowerProbe::new(Fixed(supply));
            }
        },
        |factory| factory,
        seed,
        catch_up_gate,
        |event| {
            matches!(
                event,
                RuntimeEvent::LivePreviewState {
                    off: Some(PreviewOffReason::CatchingUp),
                    ..
                }
            )
        },
    )
}

/// [`held_run`] with the session's config changed by `tweak`, the tracks' transcribers made by
/// `wrap` around the run's own factory, and the gated catch-up let go at the first event `opens`
/// accepts (in [`held_run`], the wait being said).
fn held_run_on(
    scene: &Scene,
    previews: LivePreviews,
    tweak: impl FnOnce(&mut LiveConfig),
    wrap: impl FnOnce(LiveAsrFactory) -> LiveAsrFactory,
    seed: u64,
    catch_up_gate: bool,
    mut opens: impl FnMut(&RuntimeEvent) -> bool,
) -> Vec<RuntimeEvent> {
    let open = Arc::new(AtomicBool::new(false));
    let factory = wrap(if catch_up_gate {
        gated_factory(Arc::clone(&open))
    } else {
        factory_of(scene, Arc::default())
    });
    let finished = Arc::new(AtomicBool::new(false));
    let watchdog = {
        let (open, finished) = (Arc::clone(&open), Arc::clone(&finished));
        std::thread::spawn(move || {
            let limit = Instant::now() + GUARD;
            while !finished.load(Ordering::SeqCst) && Instant::now() < limit {
                std::thread::sleep(Duration::from_millis(5));
            }
            open.store(true, Ordering::SeqCst);
        })
    };
    let gates = held_gates();
    let dir = TempDir::new();
    let svc = services(&dir);
    let mut config = LiveConfig {
        previews: Some(previews),
        ..held_config(gates.clone())
    };
    tweak(&mut config);
    let mut session = start_live(
        fast_source(scene),
        svc,
        config,
        Some(factory),
        Box::new(ChaCha8Rng::seed_from_u64(seed)),
    )
    .unwrap();
    let events = session.take_events().unwrap();
    let mut all = Vec::new();
    for event in &events {
        // Released at once, or once the hold has kept more audio than counts as caught up
        // (judged on the audio's own time, so there is a catch-up to wait for).
        let release = match event {
            RuntimeEvent::LiveTranscript(LiveTranscriptState::Held(_)) => !catch_up_gate,
            RuntimeEvent::Levels(l) => {
                catch_up_gate && l.end >= MeetingTime::from_millis(2 * CAUGHT_UP_BACKLOG_MS)
            }
            _ => false,
        };
        if release {
            gates.transcription.store(true, Ordering::SeqCst);
        }
        if opens(&event) {
            open.store(true, Ordering::SeqCst);
        }
        let ended = matches!(event, RuntimeEvent::Ended(_));
        all.push(event);
        if ended {
            break;
        }
    }
    session.wait().unwrap();
    finished.store(true, Ordering::SeqCst);
    watchdog.join().unwrap();
    all
}

const CAUGHT_UP_BACKLOG_MS: u64 = crate::live::hold::CAUGHT_UP_BACKLOG.as_millis() as u64;

fn refusing_rule(reason: PreviewOffReason, text: &'static str) -> crate::live::PreviewRule {
    Arc::new(move |_: &BackendInfo| Some(PreviewRefusal::new(reason, text)))
}

/// T12.33: what the computer's rule refuses is known before anything is said, so a meeting
/// whose previews are turned off (or off by default here) never says it waits for the catch-up
/// or blames the battery, and loads nothing.
#[test]
fn live_preview_the_rule_is_asked_before_a_wait_or_the_battery_is_said() {
    let scene = scene_of(27);
    for (reason, text, power, seed) in [
        (
            PreviewOffReason::TurnedOff,
            "turned off in Settings",
            None,
            377,
        ),
        (
            PreviewOffReason::OffByDefault,
            "not enough memory for a second session of the model",
            Some(PowerSupply::Battery),
            379,
        ),
    ] {
        let seen = Arc::new(Seen::default());
        let config = LivePreviews {
            rule: Some(refusing_rule(reason, text)),
            ..previews(loader(&seen, marker_words))
        };
        let events = held_run(&scene, config, power, seed, false);
        assert_eq!(
            states_of(&events),
            vec![(TrackId::Reference, Some(reason))],
            "{reason:?}"
        );
        assert_eq!(seen.calls.load(Ordering::SeqCst), 0, "{reason:?}");
        let report = report_of(&events).expect("a report");
        assert_eq!(report.off.as_deref(), Some(text));
        assert_eq!(report.reason, Some(reason));
        assert!(previews_of(&events).is_empty());
    }
    // A rule that allows them changes nothing: the wait is said, then that they run.
    let seen = Arc::new(Seen::default());
    let allows: crate::live::PreviewRule = Arc::new(|_: &BackendInfo| None);
    let config = LivePreviews {
        rule: Some(allows),
        ..previews(loader(&seen, marker_words))
    };
    assert_eq!(
        states_of(&held_run(&scene, config, None, 381, true)),
        vec![
            (TrackId::Reference, Some(PreviewOffReason::CatchingUp)),
            (TrackId::Reference, None),
        ]
    );
    // Without a hold the rule still comes before the battery.
    let seen = Arc::new(Seen::default());
    let dir = TempDir::new();
    let config = LiveConfig {
        power: PowerProbe::new(Fixed(PowerSupply::Battery)),
        ..config_with(Some(LivePreviews {
            rule: Some(refusing_rule(
                PreviewOffReason::Model,
                "previews run on Parakeet only",
            )),
            ..previews(loader(&seen, marker_words))
        }))
    };
    let session = start_live(
        fast_source(&scene),
        services(&dir),
        config,
        Some(factory_of(&scene, Arc::default())),
        Box::new(ChaCha8Rng::seed_from_u64(383)),
    )
    .unwrap();
    let (events, _) = collect(session);
    assert_eq!(
        states_of(&events),
        vec![(TrackId::Reference, Some(PreviewOffReason::Model))]
    );
}

/// T12.33: a preview thread that crashes says so while the meeting records, in the words the
/// report at the end keeps.
#[test]
fn live_preview_a_crashed_thread_says_so_during_the_meeting() {
    let scene = scene_of(27);
    let seen = Arc::new(Seen::default());
    let script = |_: usize, _: &CancelToken| -> Result<Vec<Word>, AsrError> {
        panic!("a test crash");
    };
    let (with, events, ..) = run_with(&scene, Some(previews(loader(&seen, script))), 387);
    assert_eq!(
        states_of(&events),
        vec![
            (TrackId::Reference, None),
            (TrackId::Reference, Some(PreviewOffReason::Failed)),
        ]
    );
    let said = position(&events, |e| {
        matches!(e, RuntimeEvent::LivePreviewState { off: Some(_), .. })
    });
    let saved = position(&events, |e| {
        matches!(e, RuntimeEvent::RecordingSaved { .. })
    });
    assert!(said < saved, "said at {said}, saved at {saved}");
    let report = report_of(&events).expect("a report");
    assert_eq!(report.off.as_deref(), Some("the preview thread crashed"));
    assert_eq!(report.reason, Some(PreviewOffReason::Failed));
    let (without, ..) = run_with(&scene, None, 387);
    assert_eq!(with, without);
}

/// T12.33: the user's own track in a call is judged by its own rule, the far end by the first;
/// in the room the mic is judged by the first.
#[test]
fn live_preview_each_track_is_judged_by_its_own_rule() {
    let scene = scene_of(27);
    let (far, own) = (Arc::new(Seen::default()), Arc::new(Seen::default()));
    let config = LivePreviews {
        own_voice: Some(loader(&own, marker_words)),
        own_voice_rule: Some(refusing_rule(
            PreviewOffReason::OwnVoiceOnNpu,
            crate::backends::OWN_VOICE_NPU_REFUSED,
        )),
        ..previews(loader(&far, marker_words))
    };
    let (_, events, ..) = run_with(&scene, Some(config), 385);
    let mut states = states_of(&events);
    states.sort_by_key(|(track, _)| *track == TrackId::Reference);
    assert_eq!(
        states,
        vec![
            (TrackId::Mic, Some(PreviewOffReason::OwnVoiceOnNpu)),
            (TrackId::Reference, None),
        ]
    );
    assert_eq!(own.calls.load(Ordering::SeqCst), 0, "own voice loaded");
    assert!(far.calls.load(Ordering::SeqCst) > 0);
    let mic = report_for(&events, TrackId::Mic);
    assert_eq!(
        mic.off.as_deref(),
        Some(crate::backends::OWN_VOICE_NPU_REFUSED)
    );
}

/// T12.33: the app's previews carry the loaders' own rule: the first for every choice, the
/// user's own voice only under Everyone; with the setting off the rule says so on any backend.
#[test]
fn live_preview_the_apps_previews_carry_the_loaders_rule() {
    use crate::preview_settings::PreviewChoice;
    let empty = TempDir::new();
    let models = crate::backends::ModelDir::new(&empty.0);
    let on = |kind| BackendInfo {
        kind,
        device: "a device".to_owned(),
        model: "a model".to_owned(),
    };
    for choice in [
        PreviewChoice::Off,
        PreviewChoice::Automatic,
        PreviewChoice::On,
        PreviewChoice::Everyone,
    ] {
        let previews = crate::host::live_previews(&models, choice);
        let rule = previews.rule.clone().expect("the first rule");
        assert_eq!(
            previews.own_voice_rule.is_some(),
            choice == PreviewChoice::Everyone,
            "{choice:?}"
        );
        assert_eq!(
            previews.own_voice.is_some(),
            previews.own_voice_rule.is_some(),
            "{choice:?}"
        );
        for kind in [BackendKind::Cpu, BackendKind::Cuda, BackendKind::Qnn] {
            let said = rule(&on(kind)).map(|r| r.reason);
            if choice == PreviewChoice::Off {
                assert_eq!(said, Some(PreviewOffReason::TurnedOff), "{kind:?}");
            } else {
                assert_ne!(
                    said,
                    Some(PreviewOffReason::TurnedOff),
                    "{choice:?} {kind:?}"
                );
            }
            // The rule is the loader's: what one refuses, the other refuses in the same words.
            assert_eq!(
                rule(&on(kind)).map(|r| r.text),
                crate::backends::previews_refused(kind, &crate::hardware(), choice),
                "{choice:?} {kind:?}"
            );
        }
        // No preview runs beside a part that is not Parakeet's, whatever was chosen.
        if choice != PreviewChoice::Off {
            assert_eq!(
                rule(&on(BackendKind::Vulkan)).map(|r| r.reason),
                Some(PreviewOffReason::Model)
            );
        }
        if let Some(own) = &previews.own_voice_rule {
            assert_eq!(
                own(&on(BackendKind::Qnn)).map(|r| r.reason),
                Some(PreviewOffReason::OwnVoiceOnNpu)
            );
        }
    }
}

/// A power supply that counts its reads, on battery while `.1` is set.
struct CountingPower(Arc<AtomicUsize>, Arc<AtomicBool>);

impl PowerSource for CountingPower {
    fn supply(&self) -> PowerSupply {
        self.0.fetch_add(1, Ordering::SeqCst);
        if self.1.load(Ordering::SeqCst) {
            PowerSupply::Battery
        } else {
            PowerSupply::Mains
        }
    }
}

fn is_state(event: &RuntimeEvent, want: Option<PreviewOffReason>) -> bool {
    matches!(event, RuntimeEvent::LivePreviewState { off, .. } if *off == want)
}

/// T12.33 review: through a long catch-up (many session loops with the track's backend known)
/// the rule is asked once for the track and the power supply is read at its own pace, never on
/// every loop; what is said does not change.
#[test]
fn live_preview_a_long_catch_up_asks_the_rule_once_and_reads_the_supply_at_its_pace() {
    let scene = scene_of(27);
    let seen = Arc::new(Seen::default());
    let asked = Arc::new(AtomicUsize::new(0));
    let rule: crate::live::PreviewRule = {
        let asked = Arc::clone(&asked);
        Arc::new(move |_: &BackendInfo| {
            asked.fetch_add(1, Ordering::SeqCst);
            None
        })
    };
    let reads = Arc::new(AtomicUsize::new(0));
    let power = CountingPower(Arc::clone(&reads), Arc::default());
    let config = LivePreviews {
        rule: Some(rule),
        ..previews(loader(&seen, marker_words))
    };
    // The catch-up lasts while 40 meters go by after the wait is said: many session loops.
    let mut meters_after_the_wait: Option<u32> = None;
    let events = held_run_on(
        &scene,
        config,
        |c| {
            c.power = PowerProbe::new(power);
            c.power_poll = Duration::from_secs(3600);
        },
        |factory| factory,
        389,
        true,
        |event| {
            if is_state(event, Some(PreviewOffReason::CatchingUp)) {
                meters_after_the_wait = Some(0);
            }
            if let (RuntimeEvent::Levels(_), Some(n)) = (event, meters_after_the_wait.as_mut()) {
                *n += 1;
            }
            meters_after_the_wait.is_some_and(|n| n >= 40)
        },
    );
    assert_eq!(
        states_of(&events),
        vec![
            (TrackId::Reference, Some(PreviewOffReason::CatchingUp)),
            (TrackId::Reference, None),
        ]
    );
    assert_eq!(asked.load(Ordering::SeqCst), 1, "the rule is asked once");
    // At most one read each with an hour between reads: the session's start, its speaker watch,
    // the waiting previews, the start after the catch-up and the running previews.
    let reads = reads.load(Ordering::SeqCst);
    assert!(reads <= 5, "{reads} reads of the power supply");
}

/// T12.33 review: a computer that goes on battery while previews wait for the catch-up says the
/// battery while the transcript is still catching up, not only when the wait is over.
#[test]
fn live_preview_going_on_battery_during_the_catch_up_says_it_at_once() {
    let scene = scene_of(27);
    let seen = Arc::new(Seen::default());
    let battery = Arc::new(AtomicBool::new(false));
    let power = CountingPower(Arc::default(), Arc::clone(&battery));
    let events = held_run_on(
        &scene,
        previews(loader(&seen, marker_words)),
        |c| {
            c.power = PowerProbe::new(power);
            c.power_poll = Duration::from_millis(1);
        },
        |factory| factory,
        391,
        true,
        |event| {
            if is_state(event, Some(PreviewOffReason::CatchingUp)) {
                battery.store(true, Ordering::SeqCst);
            }
            is_state(event, Some(PreviewOffReason::OnBattery))
        },
    );
    assert_eq!(
        states_of(&events),
        vec![
            (TrackId::Reference, Some(PreviewOffReason::CatchingUp)),
            (TrackId::Reference, Some(PreviewOffReason::OnBattery)),
        ]
    );
    let battery_said = position(&events, |e| is_state(e, Some(PreviewOffReason::OnBattery)));
    let up_to_date = position(&events, |e| {
        matches!(
            e,
            RuntimeEvent::LiveTranscript(LiveTranscriptState::UpToDate)
        )
    });
    assert!(battery_said < up_to_date, "{battery_said} {up_to_date}");
    // Still decided at the start after the catch-up: on battery nothing loads.
    assert_eq!(seen.calls.load(Ordering::SeqCst), 0);
    assert_eq!(
        report_of(&events).expect("a report").reason,
        Some(PreviewOffReason::OnBattery)
    );
}

/// A track's real transcriber reporting the model `.1` on `.0`, as a meeting transcribed with
/// that model does.
struct Reporting(
    BackendKind,
    &'static str,
    Box<dyn earshot_core::asr::Transcriber + Send>,
);

impl earshot_core::asr::Transcriber for Reporting {
    fn transcribe(
        &mut self,
        audio: &MonoBuffer,
        options: &TranscribeOptions,
        cancel: &CancelToken,
    ) -> Result<Transcription, AsrError> {
        let mut t = self.2.transcribe(audio, options, cancel)?;
        t.backend = self.backend_info();
        Ok(t)
    }

    fn backend_info(&self) -> BackendInfo {
        BackendInfo {
            kind: self.0,
            model: self.1.to_owned(),
            ..self.2.backend_info()
        }
    }
}

/// `inner` with both tracks' transcribers reporting `model` on `kind`.
fn reporting(mut inner: LiveAsrFactory, kind: BackendKind, model: &'static str) -> LiveAsrFactory {
    Box::new(move |track, entity| {
        let mut asr = inner(track, entity)?;
        if let Some(asr) = asr.as_mut() {
            let real = std::mem::replace(
                &mut asr.asr.transcriber,
                Box::new(super::queue_tests::ContentWords),
            );
            asr.asr.transcriber = Box::new(Reporting(kind, model, real));
        }
        Ok(asr)
    })
}

/// A whisper model of the manifest.
const WHISPER: &str = "base.en";

/// T12.33 review: the app's rule holds the loader's model rule, so a whisper track is refused by
/// the rule in the loader's own words and kind, on every part, before anything is loaded.
#[test]
fn live_preview_the_apps_rule_refuses_a_whisper_track_as_the_loader_does() {
    use crate::preview_settings::PreviewChoice;
    assert!(matches!(
        crate::backends::asr_model(WHISPER),
        Ok(crate::backends::AsrModel::Whisper(_))
    ));
    let empty = TempDir::new();
    let models = crate::backends::ModelDir::new(&empty.0);
    let whisper = |kind| BackendInfo {
        kind,
        device: "a device".to_owned(),
        model: WHISPER.to_owned(),
    };
    for choice in [PreviewChoice::On, PreviewChoice::Everyone] {
        let previews = crate::host::live_previews(&models, choice);
        let rule = previews.rule.clone().expect("the first rule");
        for kind in [BackendKind::Cpu, BackendKind::Cuda, BackendKind::Qnn] {
            let refused = rule(&whisper(kind)).expect("a whisper track is refused");
            assert_eq!(
                refused.reason,
                PreviewOffReason::Model,
                "{choice:?} {kind:?}"
            );
            let Err(loaded) = (previews.load)(&whisper(kind)) else {
                panic!("a whisper preview loaded");
            };
            assert_eq!(loaded, refused, "{choice:?} {kind:?}");
        }
        if let (Some(own), Some(own_rule)) = (&previews.own_voice, &previews.own_voice_rule) {
            let Err(loaded) = own(&whisper(BackendKind::Cpu)) else {
                panic!("a whisper preview loaded");
            };
            assert_eq!(Some(loaded), own_rule(&whisper(BackendKind::Cpu)));
        }
        assert!(previews.user_asked, "{choice:?}");
    }
    assert!(!crate::host::live_previews(&models, PreviewChoice::Automatic).user_asked);
}

/// T12.33 review: a held meeting on battery transcribed by a whisper model never says it waits
/// for the catch-up or blames the battery: previews never run on it. Turned on, it says the model;
/// under the automatic choice it says nothing a host shows.
#[test]
fn live_preview_a_whisper_track_says_the_model_or_nothing_never_a_wait_or_the_battery() {
    use crate::preview_settings::PreviewChoice;
    let scene = scene_of(27);
    let empty = TempDir::new();
    let models = crate::backends::ModelDir::new(&empty.0);
    for (choice, said, seed) in [
        (PreviewChoice::On, PreviewOffReason::Model, 393),
        (
            PreviewChoice::Automatic,
            PreviewOffReason::OffByDefault,
            395,
        ),
    ] {
        let previews = LivePreviews {
            background: false,
            ..crate::host::live_previews(&models, choice)
        };
        let events = held_run_on(
            &scene,
            previews,
            |c| c.power = PowerProbe::new(Fixed(PowerSupply::Battery)),
            |factory| reporting(factory, BackendKind::Cpu, WHISPER),
            seed,
            false,
            |_| false,
        );
        assert_eq!(
            states_of(&events),
            vec![(TrackId::Reference, Some(said))],
            "{choice:?}"
        );
        let report = report_of(&events).expect("a report");
        assert_eq!(report.reason, Some(said), "{choice:?}");
        // The report keeps the rule's own words.
        assert_eq!(
            report.off.as_deref(),
            crate::backends::preview_rule(
                &BackendInfo {
                    kind: BackendKind::Cpu,
                    device: String::new(),
                    model: WHISPER.to_owned(),
                },
                &crate::hardware(),
                choice
            )
            .err()
            .map(|r| r.text)
            .as_deref(),
            "{choice:?}"
        );
        assert!(previews_of(&events).is_empty());
    }
}
