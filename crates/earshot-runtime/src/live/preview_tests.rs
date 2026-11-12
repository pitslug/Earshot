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
use crate::live::preview::{COVERED_SLACK, FELL_BEHIND_REASON, ON_BATTERY_REASON, PreviewWords};
use crate::live::{LivePreviews, PreviewLoader, PreviewReport};
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

/// A track's real transcriber reporting CUDA, as a meeting transcribed on the graphics card does.
struct OnCuda(Box<dyn earshot_core::asr::Transcriber + Send>);

impl earshot_core::asr::Transcriber for OnCuda {
    fn transcribe(
        &mut self,
        audio: &MonoBuffer,
        options: &TranscribeOptions,
        cancel: &CancelToken,
    ) -> Result<Transcription, AsrError> {
        let mut t = self.0.transcribe(audio, options, cancel)?;
        t.backend = self.backend_info();
        Ok(t)
    }

    fn backend_info(&self) -> BackendInfo {
        BackendInfo {
            kind: BackendKind::Cuda,
            ..self.0.backend_info()
        }
    }
}

/// [`factory_of`], with both tracks' transcribers reporting CUDA.
fn cuda_factory_of(scene: &Scene) -> LiveAsrFactory {
    let mut inner = factory_of(scene, Arc::default());
    Box::new(move |track, entity| {
        let mut asr = inner(track, entity)?;
        if let Some(asr) = asr.as_mut() {
            let real = std::mem::replace(
                &mut asr.asr.transcriber,
                Box::new(super::queue_tests::ContentWords),
            );
            asr.asr.transcriber = Box::new(OnCuda(real));
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
    let refused: PreviewLoader = Arc::new(|_| Err("not on this backend".to_owned()));
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
        own_voice: Some(Arc::new(|_| Err("needs more memory".to_owned()))),
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
    let load: PreviewLoader = Arc::new(|_| Err("not on this backend".to_owned()));
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
    let gates = held_gates();
    let dir = TempDir::new();
    let svc = services(&dir);
    let go = Arc::new(AtomicBool::new(true));
    let source = WaitingSource {
        inner: fast_source(&scene),
        go: Arc::clone(&go),
    };
    let config = LiveConfig {
        previews: Some(previews(loader(&seen, marker_words))),
        ..held_config(gates.clone())
    };
    let mut session = start_live(
        source,
        svc.clone(),
        config,
        Some(factory_of(&scene, Arc::default())),
        Box::new(ChaCha8Rng::seed_from_u64(321)),
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
    for kind in [BackendKind::Qnn, BackendKind::Vulkan, BackendKind::Fake] {
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

/// What the fake streaming model was given.
#[derive(Default)]
struct Heard {
    /// Samples pushed since the start, over every restart.
    samples: AtomicUsize,
    restarts: AtomicUsize,
}

/// A streaming model that finishes a `MARKER` word every half second of audio it is given.
struct FakeStream {
    heard: Arc<Heard>,
    /// Samples since the last restart.
    position: usize,
    words: Vec<Word>,
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
        for k in before..after {
            let ms = k as u64 * 500;
            self.words
                .push(Word::new(format!("{MARKER}{k}"), ms, ms + 100, 0.9));
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

fn stream_loader(heard: &Arc<Heard>) -> crate::live::StreamLoader {
    let heard = Arc::clone(heard);
    Arc::new(move |_| {
        Ok(Box::new(FakeStream {
            heard: Arc::clone(&heard),
            position: 0,
            words: Vec::new(),
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
