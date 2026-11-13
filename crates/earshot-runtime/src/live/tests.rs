//! Live session tests: fakes only (replay and scripted capture sources, the fake
//! transcriber, the energy VAD, a temp data directory). Every name contains `live`.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::Receiver;
use std::time::Duration;

use earshot_core::asr::{
    AsrError, ChunkerConfig, EnergyVad, FakeTranscriber, RawSegment, TranscriptEvent, Word,
};
use earshot_core::audio::{MonoBuffer, SampleRate};
use earshot_core::capture::{
    AudioSource, CaptureEvent, CaptureSession, CaptureSink, DeviceTime, Discontinuity,
    DiscontinuityKind, EndOfStream, EndReason, EndpointWarning, ReplayBuilder, SourceStatus,
    TrackId, TrackInfo,
};
use earshot_core::identity::{EntityId, ManualClock};
use earshot_core::model::{
    AudioFileRepo, ExportRepo, MeetingRepo, MeetingTime, TranscriptPass, live_segments,
};
use earshot_core::pipeline::{LiveAsr, PipelineConfig, PipelineError};
use earshot_store::{DataDir, Store, read_flac};
use rand_chacha::ChaCha8Rng;
use rand_core::{Rng, SeedableRng};

use super::*;
use crate::{RuntimeError, Services};

const ORIGIN: DeviceTime = DeviceTime::from_ticks(9_000_000_000);
const R16: SampleRate = SampleRate::HZ_16000;

/// A temp directory removed on drop (std only; no tempfile crate, AGENTS.md).
struct TempDir(PathBuf);

impl TempDir {
    fn new() -> Self {
        static N: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "earshot-runtime-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::SeqCst)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn services(dir: &TempDir) -> Services {
    let data = DataDir::new(&dir.0);
    data.create().unwrap();
    let store = Store::open(&data.db_path()).unwrap();
    Services::new(store, data, Arc::new(ManualClock::new(1_760_000_000_000))).unwrap()
}

fn noise(seed: u64, n: usize, amp: f32) -> Vec<f32> {
    let mut rng = ChaCha8Rng::seed_from_u64(seed);
    (0..n)
        .map(|_| ((rng.next_u32() >> 8) as f32 / (1u32 << 24) as f32 * 2.0 - 1.0) * amp)
        .collect()
}

fn words(prefix: &str, start_ms: u64, n: usize) -> Vec<Word> {
    (0..n)
        .map(|i| {
            let s = start_ms + 400 * i as u64;
            Word::new(format!("{prefix}{i}"), s, s + 300, 0.9)
        })
        .collect()
}

/// Loud noise inside the word spans, quiet elsewhere (never repeats).
fn speech(words: &[Word], total_ms: u64, seed: u64) -> Vec<f32> {
    let base = noise(seed, (total_ms * 16) as usize, 1.0);
    base.iter()
        .enumerate()
        .map(|(i, x)| {
            let ms = i as u64 / 16;
            let loud = words
                .iter()
                .any(|w| (w.start.as_millis()..w.end.as_millis()).contains(&ms));
            x * if loud { 0.3 } else { 1e-4 }
        })
        .collect()
}

fn ids() -> earshot_core::asr::SegmentIds {
    let mut n = 0u8;
    Box::new(move || {
        n = n.wrapping_add(1);
        EntityId::from_bytes([0xB0, n, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1])
    })
}

/// Live ASR on `track` with a timeline fake over `audio`.
fn fake_asr(track: TrackId, audio: Vec<f32>, w: Vec<Word>) -> LiveAsrFactory {
    let mut slot = Some((audio, w));
    Box::new(move |t, entity| {
        if t != track {
            return Ok(None);
        }
        let Some((audio, w)) = slot.take() else {
            return Ok(None);
        };
        Ok(Some(
            LiveAsr {
                transcriber: Box::new(crate::testutil::timeline(audio, w)),
                vad: Box::new(EnergyVad::default()),
                config: ChunkerConfig::default(),
                track_entity: entity,
                segment_ids: ids(),
            }
            .into(),
        ))
    })
}

/// The live chunker (T6.10): most tests below pin the chunker's behaviour, which stays
/// available as a setting; the default, speech stretches (T6.13b), has its own tests
/// (`live_stretch_*`).
fn config(domain: earshot_core::model::ClockDomain) -> LiveConfig {
    LiveConfig {
        title: "Weekly sync".into(),
        clock_domain: domain,
        idle_wait: Duration::from_millis(1),
        transcription: crate::live::LiveTranscription::Chunker,
        ..LiveConfig::default()
    }
}

/// Speech stretches as configured by default (T6.13b), except the repetition guard: the
/// fake transcriber's synthetic words (`a0 a1 a2 ...`) compress like a loop (a ratio far
/// above real speech's), so the guard would drop every piece; it has its own tests in core
/// (`asr_pieces_*`, `asr_stretch_*`).
fn stretches() -> crate::live::LiveTranscription {
    crate::live::LiveTranscription::Stretches(earshot_core::asr::PieceConfig {
        compression_ratio_threshold: 1_000.0,
        ..earshot_core::asr::PieceConfig::default()
    })
}

/// [`config`] with [`stretches`].
fn stretch_config(domain: earshot_core::model::ClockDomain) -> LiveConfig {
    LiveConfig {
        transcription: stretches(),
        ..config(domain)
    }
}

fn collect(mut session: LiveSession) -> (Vec<RuntimeEvent>, LiveOutcome) {
    let events = session.take_events().expect("events not taken yet");
    let outcome = session.wait().expect("session ends cleanly");
    let all: Vec<RuntimeEvent> = events.try_iter().collect();
    (all, outcome)
}

fn warnings(events: &[RuntimeEvent]) -> Vec<RuntimeWarning> {
    events
        .iter()
        .filter_map(|e| match e {
            RuntimeEvent::Warning(w) => Some(w.clone()),
            _ => None,
        })
        .collect()
}

fn texts(words: &[Word]) -> Vec<String> {
    words.iter().map(|w| w.text.clone()).collect()
}

#[test]
fn live_start_warnings_follow_the_backends_and_come_before_the_end() {
    // T6.24: a capture-side finding known at the start (a Bluetooth hands-free mic) is a
    // warning right after Started and the backend events.
    let dir = TempDir::new();
    let svc = services(&dir);
    let w = words("a", 1000, 3);
    let audio = speech(&w, 4000, 1);
    let source = ReplayBuilder::new(ORIGIN, MonoBuffer::new(R16, audio.clone()))
        .build()
        .unwrap();
    let warning = RuntimeWarning::BluetoothHandsFreeMic {
        device_name: "Headset (Hands-Free)".into(),
        sample_rate: Some(16_000),
    };
    let session = start_live_with_warnings(
        source,
        svc.clone(),
        config(earshot_core::model::ClockDomain::Replay),
        Some(fake_asr(TrackId::Mic, audio, w)),
        Box::new(ChaCha8Rng::seed_from_u64(17)),
        vec![warning.clone()],
    )
    .unwrap();
    let (events, _) = collect(session);
    assert_eq!(warnings(&events), [warning]);
    let at = |pred: &dyn Fn(&RuntimeEvent) -> bool| events.iter().position(pred).unwrap();
    let started = at(&|e| matches!(e, RuntimeEvent::Started { .. }));
    let backend = at(&|e| matches!(e, RuntimeEvent::LiveBackend { .. }));
    let warned = at(&|e| matches!(e, RuntimeEvent::Warning(_)));
    let first_audio = at(&|e| {
        matches!(
            e,
            RuntimeEvent::Levels(_) | RuntimeEvent::Stats(_) | RuntimeEvent::Transcript { .. }
        )
    });
    assert_eq!(started, 0);
    assert!(backend < warned && warned < first_audio, "{events:?}");
}

#[test]
fn live_session_records_transcribes_and_persists_a_meeting() {
    let dir = TempDir::new();
    let svc = services(&dir);
    let mut w = words("a", 1000, 5);
    w.extend(words("b", 5000, 4));
    let audio = speech(&w, 8000, 1);
    let source = ReplayBuilder::new(ORIGIN, MonoBuffer::new(R16, audio.clone()))
        .build()
        .unwrap();
    let session = start_live(
        source,
        svc.clone(),
        config(earshot_core::model::ClockDomain::Replay),
        Some(fake_asr(TrackId::Mic, audio.clone(), w.clone())),
        Box::new(ChaCha8Rng::seed_from_u64(7)),
    )
    .unwrap();
    let meeting = session.meeting();
    let (events, outcome) = collect(session);

    // Event stream: Started first, Ended last, finals in between.
    assert!(
        matches!(events.first(), Some(RuntimeEvent::Started { meeting: m, .. }) if *m == meeting)
    );
    assert!(matches!(events.last(), Some(RuntimeEvent::Ended(o)) if *o == outcome));
    let finals: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            RuntimeEvent::Transcript {
                track: TrackId::Mic,
                event: TranscriptEvent::Final(s),
            } => Some(s.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(finals.len(), 2);
    assert!(events.iter().any(|e| matches!(e, RuntimeEvent::Levels(_))));
    assert!(warnings(&events).is_empty(), "{:?}", warnings(&events));

    assert_eq!(outcome.meeting, meeting);
    assert_eq!(outcome.end, MeetingTime::from_millis(8000));
    assert_eq!(outcome.reason, EndReason::Finished);
    assert_eq!(outcome.final_pass.tracks.len(), 1);
    assert!(outcome.unavailable_at_end.is_empty());

    let store = svc.store();
    let m = store.meeting(meeting).unwrap().unwrap();
    assert_eq!(m.title, "Weekly sync");
    assert_eq!(m.duration_ms, Some(8000));
    let tracks = store.tracks(meeting).unwrap();
    assert_eq!(tracks.len(), 1);
    assert_eq!(tracks[0].kind, TrackId::Mic);
    assert_eq!(
        outcome.final_pass.tracks[0].track_entity,
        Some(tracks[0].id)
    );
    // Live finals are stored as the Live pass, on the track created before capture.
    let rows = live_segments(&**store, meeting, TranscriptPass::Live).unwrap();
    let stored: Vec<String> = rows.iter().flat_map(|r| texts(&r.segment.words)).collect();
    assert_eq!(stored, texts(&w));
    assert!(rows.iter().all(|r| r.segment.track == tracks[0].id));
    let stored_segments: Vec<_> = rows.into_iter().map(|r| r.segment).collect();
    assert_eq!(stored_segments, finals);
    // The recording is finalised as 16 kHz FLAC with its AudioFile row.
    let files = store.audio_files(meeting).unwrap();
    assert_eq!(files.len(), 1);
    assert_eq!(outcome.audio_files, vec![(TrackId::Mic, files[0].id)]);
    let path = svc.data.resolve(&files[0].path).unwrap();
    let (rate, flac) = read_flac(&path).unwrap();
    assert_eq!(rate, 16_000);
    assert_eq!(flac.len(), audio.len());
    let worst = flac
        .iter()
        .zip(&audio)
        .map(|(a, b)| (a - b).abs())
        .fold(0.0f32, f32::max);
    assert!(worst <= 1.0 / 32768.0, "16-bit quantisation only: {worst}");
    // Obsidian export is on demand only: a live session never writes a vault note.
    let device = store.device();
    assert!(store.export_record(meeting, device).unwrap().is_none());
    assert!(outcome.problems.is_empty(), "{:?}", outcome.problems);
}

/// Starts an endless (paced) mic-only session and waits until audio flows.
fn running_session(svc: &Services, seed: u64) -> (LiveSession, Receiver<RuntimeEvent>) {
    let mut source = ScriptSource::new(
        CaptureSession::new(ORIGIN, TrackInfo::new(R16), None),
        Vec::new(),
    );
    source.endless = Some(0);
    let mut session = start_live(
        source,
        svc.clone(),
        config(earshot_core::model::ClockDomain::Qpc),
        None,
        Box::new(ChaCha8Rng::seed_from_u64(seed)),
    )
    .unwrap();
    let events = session.take_events().unwrap();
    assert!(matches!(
        events.recv_timeout(Duration::from_secs(10)).unwrap(),
        RuntimeEvent::Started { .. }
    ));
    while !matches!(
        events.recv_timeout(Duration::from_secs(10)).unwrap(),
        RuntimeEvent::Levels(_)
    ) {}
    (session, events)
}

#[test]
fn live_session_keeps_edits_made_to_the_meeting_while_it_runs() {
    let dir = TempDir::new();
    let svc = services(&dir);
    let (session, _events) = running_session(&svc, 20);
    let meeting = session.meeting();
    {
        // The user renames and tags the meeting during the call.
        let clock = ManualClock::new(1_760_000_100_000);
        let mut db = svc.store();
        let mut m = db.meeting(meeting).unwrap().unwrap();
        m.title = "Renamed".into();
        m.tags = vec!["project-x".into()];
        let (store, hlc) = db.parts();
        m.meta.touch(hlc, &clock);
        store.put_meeting(m).unwrap();
    }
    let outcome = session.stop().unwrap();
    let m = svc.store().meeting(meeting).unwrap().unwrap();
    assert_eq!(m.title, "Renamed");
    assert_eq!(m.tags, ["project-x"]);
    assert_eq!(m.duration_ms, Some(outcome.end.as_millis()));
}

#[test]
fn live_session_meeting_cannot_be_deleted_until_the_session_ends() {
    // T6.26: a deletion while the session records is refused (retryable); after the end
    // nothing of the meeting remains.
    let dir = TempDir::new();
    let svc = services(&dir);
    let (session, _events) = running_session(&svc, 22);
    let meeting = session.meeting();
    let err = svc.delete_meeting(meeting).unwrap_err();
    assert!(
        matches!(err, RuntimeError::MeetingBusy(m) if m == meeting),
        "{err:?}"
    );
    assert!(err.is_retryable());
    session.stop().unwrap();
    assert_eq!(svc.store().audio_files(meeting).unwrap().len(), 1);
    let report = svc.delete_meeting(meeting).unwrap();
    assert_eq!(report.audio_files_deleted, 1);
    let db = svc.store();
    assert!(db.meeting(meeting).unwrap().unwrap().meta.deleted);
    assert!(db.audio_files(meeting).unwrap().is_empty());
    assert!(
        db.local_keys()
            .unwrap()
            .iter()
            .all(|k| crate::local::meeting_of(k) != Some(meeting)),
        "{:?}",
        db.local_keys().unwrap()
    );
}

#[test]
fn live_session_dropped_handle_still_stops_and_finalises() {
    let dir = TempDir::new();
    let svc = services(&dir);
    let (session, events) = running_session(&svc, 21);
    let meeting = session.meeting();
    drop(session);
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    let ended = loop {
        assert!(
            std::time::Instant::now() < deadline,
            "a dropped session kept recording"
        );
        match events.recv_timeout(Duration::from_secs(10)).unwrap() {
            RuntimeEvent::Ended(outcome) => break outcome,
            _ => continue,
        }
    };
    assert_eq!(ended.reason, EndReason::Stopped);
    // The channel closes after Ended (the thread is gone).
    assert!(events.recv_timeout(Duration::from_secs(10)).is_err());
    assert_eq!(svc.store().audio_files(meeting).unwrap().len(), 1);
}

#[test]
fn live_session_that_cannot_start_leaves_no_meeting() {
    let dir = TempDir::new();
    let svc = services(&dir);
    let source = ReplayBuilder::new(ORIGIN, MonoBuffer::new(R16, vec![0.0; 1600]))
        .build()
        .unwrap();
    let factory: LiveAsrFactory = Box::new(|_, _| Err(RuntimeError::Internal("model missing")));
    let err = start_live(
        source,
        svc.clone(),
        config(earshot_core::model::ClockDomain::Replay),
        Some(factory),
        Box::new(ChaCha8Rng::seed_from_u64(22)),
    )
    .unwrap_err();
    assert!(matches!(err, RuntimeError::Internal("model missing")));
    assert!(svc.store().meetings().unwrap().is_empty());
    // No FLAC (or .part) left either.
    let audio = svc.data.root().join("audio");
    let leftovers = std::fs::read_dir(&audio)
        .map(|d| d.flatten().count())
        .unwrap_or(0);
    assert_eq!(leftovers, 0, "files left under {}", audio.display());
}

#[test]
fn live_session_reports_a_live_segment_it_cannot_store() {
    // Live ASR wired to a track id that is not a track of the meeting: storing its
    // finals fails (missing parent) and is reported; capture and recording go on.
    let dir = TempDir::new();
    let svc = services(&dir);
    let w = words("a", 500, 5);
    let audio = speech(&w, 4000, 23);
    let mut slot = Some((audio.clone(), w.clone()));
    let factory: LiveAsrFactory = Box::new(move |_, _| {
        let (audio, w) = slot.take().unwrap();
        Ok(Some(
            LiveAsr {
                transcriber: Box::new(crate::testutil::timeline(audio, w)),
                vad: Box::new(EnergyVad::default()),
                config: ChunkerConfig::default(),
                track_entity: EntityId::from_bytes([0xEE; 16]),
                segment_ids: ids(),
            }
            .into(),
        ))
    });
    let source = ReplayBuilder::new(ORIGIN, MonoBuffer::new(R16, audio))
        .build()
        .unwrap();
    let session = start_live(
        source,
        svc.clone(),
        config(earshot_core::model::ClockDomain::Replay),
        Some(factory),
        Box::new(ChaCha8Rng::seed_from_u64(24)),
    )
    .unwrap();
    let (events, outcome) = collect(session);
    assert!(
        warnings(&events)
            .iter()
            .any(|x| matches!(x, RuntimeWarning::Persistence(_)))
    );
    assert_eq!(outcome.audio_files.len(), 1);
    assert!(outcome.problems.is_empty());
}

#[test]
fn live_session_ends_when_the_source_ends_without_an_end_marker() {
    let dir = TempDir::new();
    let svc = services(&dir);
    let mut source = ScriptSource::new(
        CaptureSession::new(ORIGIN, TrackInfo::new(R16), None),
        (0..50)
            .map(|i| {
                frames(
                    TrackId::Mic,
                    noise(30 + i, 160, 0.01),
                    ORIGIN.after_frames(i * 160, R16),
                )
            })
            .collect(),
    );
    source.end_silently = true;
    let session = start_live(
        source,
        svc,
        config(earshot_core::model::ClockDomain::Replay),
        None,
        Box::new(ChaCha8Rng::seed_from_u64(25)),
    )
    .unwrap();
    let (_, outcome) = collect(session);
    // A source that ends without its end marker broke its contract: an error end.
    assert_eq!(outcome.reason, EndReason::Failed);
    assert_eq!(outcome.end, MeetingTime::from_millis(500));
}

#[test]
fn live_session_summary_kept_local_is_recorded_with_the_meeting() {
    // T12.13: the meeting row and its "kept on this computer" record come in one transaction.
    for keep in [true, false] {
        let dir = TempDir::new();
        let svc = services(&dir);
        let mut source = ScriptSource::new(
            CaptureSession::new(ORIGIN, TrackInfo::new(R16), None),
            (0..5)
                .map(|i| {
                    frames(
                        TrackId::Mic,
                        noise(40 + i, 160, 0.01),
                        ORIGIN.after_frames(i * 160, R16),
                    )
                })
                .collect(),
        );
        // The session ends when the script does (only the rows matter here).
        source.end_silently = true;
        let session = start_live(
            source,
            svc.clone(),
            LiveConfig {
                keep_summary_local: keep,
                ..config(earshot_core::model::ClockDomain::Replay)
            },
            None,
            Box::new(ChaCha8Rng::seed_from_u64(26)),
        )
        .unwrap();
        let (_, outcome) = collect(session);
        let want = if keep {
            crate::keep_local::SummaryPrivacy::KeptLocal
        } else {
            crate::keep_local::SummaryPrivacy::Writer
        };
        assert_eq!(
            crate::keep_local::summary_privacy(&svc.store(), outcome.meeting),
            want
        );
    }
}

#[test]
fn live_session_counts_repeated_capture_errors_instead_of_flooding() {
    let dir = TempDir::new();
    let svc = services(&dir);
    let mut script: Vec<Result<CaptureEvent, String>> =
        (0..200).map(|_| Err("device busy".to_owned())).collect();
    script.push(Err("device gone".to_owned()));
    script.push(Ok(CaptureEvent::End(EndOfStream {
        at: ORIGIN,
        reason: EndReason::Finished,
    })));
    let session = start_live(
        ScriptSource::new(
            CaptureSession::new(ORIGIN, TrackInfo::new(R16), None),
            script,
        ),
        svc,
        config(earshot_core::model::ClockDomain::Qpc),
        None,
        Box::new(ChaCha8Rng::seed_from_u64(26)),
    )
    .unwrap();
    let (events, _) = collect(session);
    assert_eq!(
        warnings(&events),
        [
            RuntimeWarning::Capture("\"device busy\"".into()),
            RuntimeWarning::CaptureRepeated {
                message: "\"device busy\"".into(),
                times: 199,
            },
            RuntimeWarning::Capture("\"device gone\"".into()),
        ]
    );
}

#[test]
fn live_session_records_both_tracks_with_a_reference() {
    let dir = TempDir::new();
    let svc = services(&dir);
    let mic = noise(2, 16_000 * 3, 0.05);
    let reference = noise(3, 16_000 * 3, 0.1);
    let source = ReplayBuilder::new(ORIGIN, MonoBuffer::new(R16, mic))
        .reference(MonoBuffer::new(R16, reference.clone()))
        .build()
        .unwrap();
    // Drift steering off: the 16 kHz reference bypasses the resampler, so the recorded
    // Them track equals the reference up to 16-bit quantisation.
    let cfg = LiveConfig {
        pipeline: PipelineConfig {
            drift_correction: false,
            ..PipelineConfig::default()
        },
        ..config(earshot_core::model::ClockDomain::Replay)
    };
    let session = start_live(
        source,
        svc.clone(),
        cfg,
        None,
        Box::new(ChaCha8Rng::seed_from_u64(8)),
    )
    .unwrap();
    let meeting = session.meeting();
    let (_, outcome) = collect(session);
    assert_eq!(outcome.final_pass.tracks.len(), 2);
    assert!(
        outcome
            .final_pass
            .tracks
            .iter()
            .all(|t| t.track_entity.is_none())
    );
    let store = svc.store();
    let kinds: Vec<TrackId> = store
        .tracks(meeting)
        .unwrap()
        .iter()
        .map(|t| t.kind)
        .collect();
    assert_eq!(kinds, [TrackId::Mic, TrackId::Reference]);
    let files = store.audio_files(meeting).unwrap();
    assert_eq!(files.len(), 2);
    let them = files
        .iter()
        .find(|f| f.path.ends_with("loopback.flac"))
        .expect("loopback.flac");
    let (_, flac) = read_flac(&svc.data.resolve(&them.path).unwrap()).unwrap();
    assert_eq!(flac.len(), reference.len());
    assert!(
        flac.iter()
            .zip(&reference)
            .all(|(a, b)| (a - b).abs() <= 1.0 / 32768.0)
    );
}

/// A capture source scripted as a list of capture events (one per pump), with optional
/// errors before given steps; delivers silence frames forever when `endless`.
struct ScriptSource {
    session: CaptureSession,
    script: VecDeque<Result<CaptureEvent, String>>,
    endless: Option<u64>,
    /// When the script runs out: report Ended without delivering an end (a broken source).
    end_silently: bool,
    stopped: bool,
    ended: bool,
}

impl ScriptSource {
    fn new(session: CaptureSession, script: Vec<Result<CaptureEvent, String>>) -> Self {
        Self {
            session,
            script: script.into(),
            endless: None,
            end_silently: false,
            stopped: false,
            ended: false,
        }
    }
}

impl AudioSource for ScriptSource {
    type Error = String;

    fn session(&self) -> &CaptureSession {
        &self.session
    }

    fn pump(&mut self, sink: &mut dyn CaptureSink) -> Result<SourceStatus, String> {
        if self.ended {
            return Ok(SourceStatus::Ended);
        }
        if self.stopped {
            self.ended = true;
            sink.end(EndOfStream {
                at: ORIGIN,
                reason: EndReason::Stopped,
            });
            return Ok(SourceStatus::Ended);
        }
        if let Some(n) = self.endless.as_mut() {
            sink.frames(
                TrackId::Mic,
                &[0.0; 160],
                ORIGIN.after_frames(*n * 160, R16),
            );
            *n += 1;
            // Pace like a device (10 ms packets), so the test sees a running session.
            std::thread::sleep(Duration::from_millis(1));
            return Ok(SourceStatus::Delivered);
        }
        match self.script.pop_front() {
            Some(Ok(event)) => {
                let end = matches!(event, CaptureEvent::End(_));
                event.deliver(sink);
                self.ended = end;
                Ok(if end {
                    SourceStatus::Ended
                } else {
                    SourceStatus::Delivered
                })
            }
            Some(Err(e)) => Err(e),
            None if self.end_silently => Ok(SourceStatus::Ended),
            None => Ok(SourceStatus::Idle),
        }
    }

    fn stop(&mut self) {
        self.stopped = true;
    }
}

fn frames(track: TrackId, samples: Vec<f32>, ts: DeviceTime) -> Result<CaptureEvent, String> {
    Ok(CaptureEvent::Frames {
        track,
        device_ts: ts,
        frames: samples,
    })
}

fn marker(track: TrackId, at: DeviceTime, kind: DiscontinuityKind) -> Result<CaptureEvent, String> {
    Ok(CaptureEvent::Discontinuity {
        track,
        discontinuity: Discontinuity { at, kind },
    })
}

#[test]
fn live_session_stops_on_request_and_finalises() {
    let dir = TempDir::new();
    let svc = services(&dir);
    let mut source = ScriptSource::new(
        CaptureSession::new(ORIGIN, TrackInfo::new(R16), None),
        Vec::new(),
    );
    source.endless = Some(0);
    let session = start_live(
        source,
        svc.clone(),
        config(earshot_core::model::ClockDomain::Qpc),
        None,
        Box::new(ChaCha8Rng::seed_from_u64(9)),
    )
    .unwrap();
    // Wait until audio flows, then stop.
    let mut session = session;
    let events = session.take_events().unwrap();
    let first = events.recv_timeout(Duration::from_secs(10)).unwrap();
    assert!(matches!(first, RuntimeEvent::Started { .. }));
    while !matches!(
        events.recv_timeout(Duration::from_secs(10)).unwrap(),
        RuntimeEvent::Levels(_)
    ) {}
    let meeting = session.meeting();
    let outcome = session.stop().unwrap();
    assert_eq!(outcome.reason, EndReason::Stopped);
    assert!(outcome.end > MeetingTime::ZERO);
    let store = svc.store();
    assert_eq!(
        store.meeting(meeting).unwrap().unwrap().duration_ms,
        Some(outcome.end.as_millis())
    );
    assert_eq!(store.audio_files(meeting).unwrap().len(), 1);
}

#[test]
fn live_session_request_stop_does_not_block_and_ends_with_the_finalised_outcome() {
    let dir = TempDir::new();
    let svc = services(&dir);
    let mut source = ScriptSource::new(
        CaptureSession::new(ORIGIN, TrackInfo::new(R16), None),
        Vec::new(),
    );
    source.endless = Some(0);
    let mut session = start_live(
        source,
        svc.clone(),
        config(earshot_core::model::ClockDomain::Qpc),
        None,
        Box::new(ChaCha8Rng::seed_from_u64(19)),
    )
    .unwrap();
    let events = session.take_events().unwrap();
    while !matches!(
        events.recv_timeout(Duration::from_secs(10)).unwrap(),
        RuntimeEvent::Levels(_)
    ) {}
    session.request_stop();
    session.request_stop();
    // The events keep flowing until Ended, which carries the finalised outcome.
    let ended = loop {
        if let RuntimeEvent::Ended(o) = events.recv_timeout(Duration::from_secs(10)).unwrap() {
            break o;
        }
    };
    assert_eq!(ended.reason, EndReason::Stopped);
    assert_eq!(ended.audio_files.len(), 1);
    assert_eq!(session.wait().unwrap(), ended);
}

#[test]
fn live_session_resyncs_both_tracks_when_the_mic_clock_drifts_too_far() {
    // The mic clock runs 1 % slow against QPC (stamps 10.1 ms apart for 10 ms of audio),
    // so its skew passes the 20 ms budget within a few seconds. The runtime must send
    // DataLost to both tracks back to back and report it.
    let dir = TempDir::new();
    let svc = services(&dir);
    let session_info = CaptureSession::new(ORIGIN, TrackInfo::new(R16), Some(TrackInfo::new(R16)));
    let mut script = Vec::new();
    let mic = noise(4, 160, 0.01);
    let reference = noise(5, 160, 0.01);
    for i in 0..1000u64 {
        let mic_ts = DeviceTime::from_ticks(ORIGIN.ticks() + i * 101_000);
        script.push(frames(TrackId::Mic, mic.clone(), mic_ts));
        script.push(frames(
            TrackId::Reference,
            reference.clone(),
            ORIGIN.after_frames(i * 160, R16),
        ));
    }
    script.push(Ok(CaptureEvent::End(EndOfStream {
        at: ORIGIN,
        reason: EndReason::Finished,
    })));
    let cfg = LiveConfig {
        mic_skew_budget: Duration::from_millis(20),
        resync_cooldown: Duration::from_secs(1),
        ..config(earshot_core::model::ClockDomain::Replay)
    };
    let session = start_live(
        ScriptSource::new(session_info, script),
        svc,
        cfg,
        None,
        Box::new(ChaCha8Rng::seed_from_u64(10)),
    )
    .unwrap();
    let (events, _) = collect(session);
    let resyncs = events
        .iter()
        .filter(|e| matches!(e, RuntimeEvent::Resynced { .. }))
        .count();
    assert!(
        warnings(&events).is_empty(),
        "a routine re-sync is not a warning"
    );
    assert!(resyncs >= 1, "no resync");
    // Each resync is a DataLost on the mic immediately followed by one on the reference.
    let lost: Vec<TrackId> = events
        .iter()
        .filter_map(|e| match e {
            RuntimeEvent::Discontinuity {
                track,
                kind: DiscontinuityKind::DataLost,
                ..
            } => Some(*track),
            _ => None,
        })
        .collect();
    assert_eq!(lost.len(), 2 * resyncs);
    assert!(
        lost.chunks(2)
            .all(|p| p == [TrackId::Mic, TrackId::Reference])
    );
    // After a resync the mic skew starts again from zero: never far beyond the budget.
    for e in &events {
        if let RuntimeEvent::Stats(s) = e {
            assert!(
                s.mic.skew_ms.abs() < 40.0,
                "mic skew {:.1} ms",
                s.mic.skew_ms
            );
        }
    }
}

#[test]
fn live_session_reports_device_loss_until_restored_and_other_capture_trouble() {
    let dir = TempDir::new();
    let svc = services(&dir);
    let session_info = CaptureSession::new(ORIGIN, TrackInfo::new(R16), Some(TrackInfo::new(R16)));
    let at = |packet: u64| ORIGIN.after_frames(packet * 160, R16);
    let lost = DiscontinuityKind::DeviceChanged {
        device_name: None,
        rate: R16,
        warning: None,
    };
    // A hands-free warning on a reference marker is ignored: the loopback is never a mic.
    let back = DiscontinuityKind::DeviceChanged {
        device_name: Some("Speakers".into()),
        rate: R16,
        warning: Some(EndpointWarning::BluetoothHandsFree {
            sample_rate: Some(16_000),
        }),
    };
    let mut script = Vec::new();
    for i in 0..100 {
        script.push(frames(TrackId::Mic, noise(i, 160, 0.01), at(i)));
        if i == 20 {
            script.push(marker(TrackId::Reference, at(i), lost.clone()));
        }
        if i == 60 {
            script.push(marker(TrackId::Reference, at(i), back.clone()));
            script.push(Err("device invalidated".to_owned()));
        }
        if !(20..60).contains(&i) {
            script.push(frames(
                TrackId::Reference,
                noise(1000 + i, 160, 0.01),
                at(i),
            ));
        }
    }
    // A mic stamp an hour ahead: not believed, reported.
    script.push(frames(
        TrackId::Mic,
        noise(7, 160, 0.01),
        ORIGIN + Duration::from_secs(3600),
    ));
    // The reference is lost again and never comes back.
    script.push(marker(TrackId::Reference, at(101), lost));
    script.push(Ok(CaptureEvent::End(EndOfStream {
        at: at(101),
        reason: EndReason::Finished,
    })));
    let session = start_live(
        ScriptSource::new(session_info, script),
        svc,
        config(earshot_core::model::ClockDomain::Qpc),
        None,
        Box::new(ChaCha8Rng::seed_from_u64(11)),
    )
    .unwrap();
    let (events, outcome) = collect(session);
    let w = warnings(&events);
    let unavailable = |w: &RuntimeWarning| {
        matches!(
            w,
            RuntimeWarning::DeviceUnavailable {
                track: TrackId::Reference
            }
        )
    };
    assert_eq!(w.iter().filter(|x| unavailable(x)).count(), 2, "{w:?}");
    assert!(w.iter().any(|x| matches!(
        x,
        RuntimeWarning::DeviceRestored { track: TrackId::Reference, device_name } if device_name == "Speakers"
    )));
    assert!(
        w.iter()
            .any(|x| matches!(x, RuntimeWarning::Capture(m) if m.contains("device invalidated")))
    );
    assert!(w.iter().any(|x| matches!(
        x,
        RuntimeWarning::Pipeline(PipelineError::TimestampJump {
            track: TrackId::Mic,
            ..
        })
    )));
    assert_eq!(outcome.unavailable_at_end, vec![TrackId::Reference]);
    assert!(
        !w.iter()
            .any(|x| matches!(x, RuntimeWarning::BluetoothHandsFreeMic { .. })),
        "{w:?}"
    );
}

#[test]
fn live_session_warns_once_per_switch_onto_a_hands_free_mic_before_its_marker() {
    // T6.24 follow-up: a mid-meeting re-open whose DeviceChanged marker carries the
    // source's hands-free warning is one BluetoothHandsFreeMic warning, right before that
    // marker's Discontinuity event (after DeviceRestored when the mic was lost).
    let dir = TempDir::new();
    let svc = services(&dir);
    let session_info = CaptureSession::new(ORIGIN, TrackInfo::new(R16), None);
    let at = |packet: u64| ORIGIN.after_frames(packet * 160, R16);
    let hf = |name: &str| DiscontinuityKind::DeviceChanged {
        device_name: Some(name.into()),
        rate: R16,
        warning: Some(EndpointWarning::BluetoothHandsFree {
            sample_rate: Some(16_000),
        }),
    };
    let plain = |name: Option<&str>| DiscontinuityKind::DeviceChanged {
        device_name: name.map(Into::into),
        rate: R16,
        warning: None,
    };
    let mut script = Vec::new();
    for i in 0..100 {
        match i {
            20 => script.push(marker(TrackId::Mic, at(i), hf("Headset (Hands-Free)"))),
            40 => script.push(marker(TrackId::Mic, at(i), plain(Some("Laptop Mic")))),
            60 => script.push(marker(TrackId::Mic, at(i), plain(None))),
            70 => script.push(marker(TrackId::Mic, at(i), hf("Buds (Hands-Free)"))),
            _ => {}
        }
        if !(60..70).contains(&i) {
            script.push(frames(TrackId::Mic, noise(i, 160, 0.01), at(i)));
        }
    }
    script.push(Ok(CaptureEvent::End(EndOfStream {
        at: at(100),
        reason: EndReason::Finished,
    })));
    let session = start_live(
        ScriptSource::new(session_info, script),
        svc,
        config(earshot_core::model::ClockDomain::Qpc),
        None,
        Box::new(ChaCha8Rng::seed_from_u64(23)),
    )
    .unwrap();
    let (events, _) = collect(session);
    let hands_free: Vec<usize> = events
        .iter()
        .enumerate()
        .filter(|(_, e)| {
            matches!(
                e,
                RuntimeEvent::Warning(RuntimeWarning::BluetoothHandsFreeMic { .. })
            )
        })
        .map(|(i, _)| i)
        .collect();
    assert_eq!(hands_free.len(), 2, "{events:?}");
    for (i, name) in hands_free
        .iter()
        .zip(["Headset (Hands-Free)", "Buds (Hands-Free)"])
    {
        assert_eq!(
            events[*i],
            RuntimeEvent::Warning(RuntimeWarning::BluetoothHandsFreeMic {
                device_name: name.into(),
                sample_rate: Some(16_000),
            })
        );
        assert!(
            matches!(
                &events[*i + 1],
                RuntimeEvent::Discontinuity {
                    track: TrackId::Mic,
                    kind: DiscontinuityKind::DeviceChanged { device_name: Some(n), .. },
                    ..
                } if n == name
            ),
            "{:?}",
            events[*i + 1]
        );
    }
    // The second switch followed a loss: DeviceRestored first, then the hands-free warning.
    assert!(matches!(
        &events[hands_free[1] - 1],
        RuntimeEvent::Warning(RuntimeWarning::DeviceRestored {
            track: TrackId::Mic,
            ..
        })
    ));
    // Audio came before the first switch.
    let first_audio = events
        .iter()
        .position(|e| matches!(e, RuntimeEvent::Levels(_) | RuntimeEvent::Stats(_)))
        .unwrap();
    assert!(first_audio < hands_free[0], "{events:?}");
}

#[test]
fn live_session_reports_a_chosen_device_given_up_not_as_back() {
    // T7.4i: a chosen mic lost mid-meeting is unavailable, then the source gives it up for
    // the default: ChosenDeviceGivenUp (never DeviceRestored), then the default's own
    // hands-free warning, before that marker's Discontinuity event.
    let dir = TempDir::new();
    let svc = services(&dir);
    let session_info = CaptureSession::new(ORIGIN, TrackInfo::new(R16), None);
    let at = |packet: u64| ORIGIN.after_frames(packet * 160, R16);
    let gone = DiscontinuityKind::DeviceChanged {
        device_name: None,
        rate: R16,
        warning: None,
    };
    let given_up = DiscontinuityKind::DeviceChanged {
        device_name: Some("Headset (Hands-Free)".into()),
        rate: R16,
        warning: Some(EndpointWarning::ChosenEndpointGone {
            chosen: "Acme Evolve".into(),
            new_endpoint: Some(Box::new(EndpointWarning::BluetoothHandsFree {
                sample_rate: Some(16_000),
            })),
        }),
    };
    let mut script = Vec::new();
    for i in 0..60 {
        match i {
            20 => script.push(marker(TrackId::Mic, at(i), gone.clone())),
            30 => script.push(marker(TrackId::Mic, at(i), given_up.clone())),
            _ => {}
        }
        if !(20..30).contains(&i) {
            script.push(frames(TrackId::Mic, noise(i, 160, 0.01), at(i)));
        }
    }
    script.push(Ok(CaptureEvent::End(EndOfStream {
        at: at(60),
        reason: EndReason::Finished,
    })));
    let session = start_live(
        ScriptSource::new(session_info, script),
        svc,
        config(earshot_core::model::ClockDomain::Qpc),
        None,
        Box::new(ChaCha8Rng::seed_from_u64(29)),
    )
    .unwrap();
    let (events, _) = collect(session);
    let warnings: Vec<&RuntimeWarning> = events
        .iter()
        .filter_map(|e| match e {
            RuntimeEvent::Warning(w) => Some(w),
            _ => None,
        })
        .collect();
    assert_eq!(
        warnings,
        [
            &RuntimeWarning::DeviceUnavailable {
                track: TrackId::Mic
            },
            &RuntimeWarning::ChosenDeviceGivenUp {
                track: TrackId::Mic,
                chosen: "Acme Evolve".into(),
                used: "Headset (Hands-Free)".into(),
            },
            &RuntimeWarning::BluetoothHandsFreeMic {
                device_name: "Headset (Hands-Free)".into(),
                sample_rate: Some(16_000),
            },
        ],
        "{events:?}"
    );
    let hf = events
        .iter()
        .position(|e| {
            matches!(
                e,
                RuntimeEvent::Warning(RuntimeWarning::BluetoothHandsFreeMic { .. })
            )
        })
        .unwrap();
    assert!(matches!(
        &events[hf + 1],
        RuntimeEvent::Discontinuity {
            track: TrackId::Mic,
            ..
        }
    ));
}

#[test]
fn live_session_keeps_recording_when_live_asr_fails() {
    let dir = TempDir::new();
    let svc = services(&dir);
    let w = words("a", 500, 10);
    let audio = speech(&w, 6000, 12);
    let factory: LiveAsrFactory = Box::new(|_, entity| {
        Ok(Some(
            LiveAsr {
                transcriber: Box::new(FakeTranscriber::scripted([Err::<Vec<RawSegment>, _>(
                    AsrError::Backend("gpu lost".into()),
                )])),
                vad: Box::new(EnergyVad::default()),
                config: ChunkerConfig::default(),
                track_entity: entity,
                segment_ids: ids(),
            }
            .into(),
        ))
    });
    let source = ReplayBuilder::new(ORIGIN, MonoBuffer::new(R16, audio.clone()))
        .build()
        .unwrap();
    let session = start_live(
        source,
        svc.clone(),
        config(earshot_core::model::ClockDomain::Replay),
        Some(factory),
        Box::new(ChaCha8Rng::seed_from_u64(12)),
    )
    .unwrap();
    let meeting = session.meeting();
    let (events, outcome) = collect(session);
    assert!(warnings(&events).iter().any(|x| matches!(
        x,
        RuntimeWarning::Pipeline(PipelineError::LiveAsr {
            track: TrackId::Mic,
            ..
        })
    )));
    assert_eq!(outcome.audio_files.len(), 1);
    assert!(outcome.final_pass.tracks[0].track_entity.is_some());
    let store = svc.store();
    assert!(
        live_segments(&**store, meeting, TranscriptPass::Live)
            .unwrap()
            .is_empty()
    );
}

#[test]
fn live_session_rejects_a_bad_pipeline_config_before_creating_rows() {
    let dir = TempDir::new();
    let svc = services(&dir);
    let source = ReplayBuilder::new(ORIGIN, MonoBuffer::new(R16, vec![0.0; 160]))
        .build()
        .unwrap();
    let cfg = LiveConfig {
        pipeline: PipelineConfig {
            stats_interval: Duration::ZERO,
            ..PipelineConfig::default()
        },
        ..config(earshot_core::model::ClockDomain::Replay)
    };
    let err = start_live(
        source,
        svc.clone(),
        cfg,
        None,
        Box::new(ChaCha8Rng::seed_from_u64(13)),
    )
    .unwrap_err();
    assert!(matches!(
        err,
        crate::RuntimeError::Pipeline(PipelineError::InvalidConfig(_))
    ));
    assert!(svc.store().meetings().unwrap().is_empty());
}

/// Last stats snapshot of an event list.
fn last_stats(events: &[RuntimeEvent]) -> earshot_core::pipeline::PipelineStats {
    events
        .iter()
        .rev()
        .find_map(|e| match e {
            RuntimeEvent::Stats(s) => Some(s.clone()),
            _ => None,
        })
        .expect("a stats event")
}

#[test]
fn live_uniform_two_rate_replay_never_restarts_or_drops() {
    // A uniform replay (exact timestamps, no gaps) of a 16 kHz mono mic and a 48 kHz
    // stereo reference of different lengths: nothing is late, nothing restarts.
    for (mic_ms, ref_ms) in [(5_000u64, 5_000u64), (4_900, 6_300), (6_300, 4_900)] {
        let dir = TempDir::new();
        let svc = services(&dir);
        let mic = noise(31, (mic_ms * 16) as usize, 0.1);
        let r48 = SampleRate::new(48_000).unwrap();
        let reference: Vec<f32> = noise(32, (ref_ms * 48) as usize, 0.1)
            .into_iter()
            .flat_map(|x| [x, x])
            .collect();
        let reference = earshot_core::capture::TrackBuffer::new(
            r48,
            std::num::NonZeroU16::new(2).unwrap(),
            reference,
        )
        .unwrap();
        let source = ReplayBuilder::new(ORIGIN, MonoBuffer::new(R16, mic))
            .reference(reference)
            .build()
            .unwrap();
        let session = start_live(
            source,
            svc.clone(),
            config(earshot_core::model::ClockDomain::Replay),
            None,
            Box::new(ChaCha8Rng::seed_from_u64(33)),
        )
        .unwrap();
        let (events, outcome) = collect(session);
        assert!(warnings(&events).is_empty(), "{:?}", warnings(&events));
        assert_eq!(outcome.end, MeetingTime::from_millis(mic_ms.max(ref_ms)));
        let s = last_stats(&events);
        let r = s.reference.expect("reference stats");
        assert_eq!(
            (
                s.mic.restarts,
                s.mic.late_dropped,
                r.restarts,
                r.late_dropped
            ),
            (0, 0, 0, 0),
            "{mic_ms}/{ref_ms} ms: {s:?}"
        );
    }
}

/// A call on loudspeakers rendered by the core simulator: noise-burst "utterances" for
/// the far end (reference) and the near end, the far end echoing into the mic through a
/// room at 100 ms bulk delay, near end 0 dB above the echo. `echo = false` is headphones.
pub(crate) fn leaky_call(echo: bool) -> earshot_core::sim::SimOutput {
    use earshot_core::sim::{Scenario, Setup, SpeechPool, simulate};
    let setup: Setup = toml_setup();
    let scenario: Scenario = serde_json::from_value(serde_json::json!({
        "name": "live-leak",
        "duration_s": 16.0,
        "seed": 61,
        "near_speech": true,
        "near_start_s": 1.0,
        "delay_ms": 100.0,
        "echo": echo,
        "ser_db": 0.0,
    }))
    .unwrap();
    let burst = |seed: u64, ms: u64| noise(seed, (ms * 16) as usize, 0.3);
    let far: Vec<Vec<f32>> = (0..6).map(|k| burst(100 + k, 1_400 + 150 * k)).collect();
    let near: Vec<Vec<f32>> = (0..6).map(|k| burst(200 + k, 1_200 + 100 * k)).collect();
    let pool = SpeechPool {
        far: &far,
        near: &near,
    };
    simulate(
        &scenario,
        &setup,
        &pool,
        &mut ChaCha8Rng::seed_from_u64(scenario.seed),
    )
    .unwrap()
}

fn toml_setup() -> earshot_core::sim::Setup {
    serde_json::from_value(serde_json::json!({
        "room": { "dims": [5.0, 4.0, 2.8], "absorption": 0.35 },
        "mic": [2.0, 1.5, 0.8],
        "speaker": [2.6, 1.5, 0.9],
        "speaker_moved": [2.6, 1.5, 0.9],
        "talker": [1.5, 1.4, 1.2],
        "rir_ms": 200.0,
    }))
    .unwrap()
}

/// Words every 300 ms through each region (16 kHz sample ranges), named `prefix<n>`.
pub(crate) fn region_words(
    prefix: &str,
    regions: &[std::ops::Range<usize>],
    shift_ms: u64,
) -> Vec<Word> {
    let mut out = Vec::new();
    for r in regions {
        let (s, e) = ((r.start / 16) as u64, (r.end / 16) as u64);
        let mut t = s + 100;
        while t + 250 <= e {
            let n = out.len();
            out.push(Word::new(
                format!("{prefix}{n}"),
                t + shift_ms,
                t + shift_ms + 250,
                0.9,
            ));
            t += 300;
        }
    }
    out
}

/// The canceller output the pipeline produces for `mic` and `reference` (drift steering
/// off, so the 16 kHz reference is not resampled): cleaned sample k at frame k, drained
/// with silence at the end.
pub(crate) fn cleaned(mic: &[f32], reference: &[f32]) -> Vec<f32> {
    use earshot_core::aec::{BLOCK, EchoCanceller, EchoCancellerConfig};
    let mut aec = EchoCanceller::new(EchoCancellerConfig::default());
    let latency = aec.output_latency();
    let total = (mic.len() + latency).div_ceil(BLOCK) * BLOCK;
    let pad = |x: &[f32]| {
        let mut v = x.to_vec();
        v.resize(total, 0.0);
        v
    };
    let mut out = vec![0.0; total];
    aec.process(&pad(mic), &pad(reference), &mut out);
    out[latency..latency + mic.len()].to_vec()
}

/// Live ASR on both tracks: timeline fakes over the cleaned mic (recognising the near
/// end and, as a leak, every echoed far-end word) and over the reference.
fn leak_asr(
    mic_stream: Vec<f32>,
    mic_words: Vec<Word>,
    reference: Vec<f32>,
    far: Vec<Word>,
    reference_delay: Option<Duration>,
) -> LiveAsrFactory {
    let mut slots = [Some((mic_stream, mic_words)), Some((reference, far))];
    Box::new(move |track, entity| {
        let Some((audio, words)) = slots[slot(track)].take() else {
            return Ok(None);
        };
        let mut n = 0u8;
        let tag = slot(track) as u8;
        let fake = crate::testutil::timeline(audio, words);
        let transcriber: Box<dyn earshot_core::asr::Transcriber + Send> =
            match reference_delay.filter(|_| track == TrackId::Reference) {
                Some(delay) => Box::new(SlowTranscriber {
                    inner: fake,
                    delay,
                    gate: None,
                    meet: None,
                }),
                None => Box::new(fake),
            };
        Ok(Some(
            LiveAsr {
                transcriber,
                vad: Box::new(EnergyVad::default()),
                config: ChunkerConfig::default(),
                track_entity: entity,
                segment_ids: Box::new(move || {
                    n = n.wrapping_add(1);
                    EntityId::from_bytes([0xC0 + tag, n, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1])
                }),
            }
            .into(),
        ))
    })
}

struct LeakRun {
    events: Vec<RuntimeEvent>,
    outcome: LiveOutcome,
    stored_mic: Vec<String>,
    near: Vec<Word>,
    echo: Vec<Word>,
    svc: Services,
    /// The scenario's `echo` flag.
    with_echo: bool,
    _dir: TempDir,
}

fn run_leaky_call(echo: bool) -> LeakRun {
    run_leaky_call_with(echo, None)
}

/// [`run_leaky_call`] with the far end's live decodes taking `reference_delay` each.
fn run_leaky_call_with(echo: bool, reference_delay: Option<Duration>) -> LeakRun {
    run_leaky_call_in(
        echo,
        reference_delay,
        crate::live::LiveTranscription::Chunker,
    )
}

/// [`run_leaky_call_with`] transcribing with `transcription`.
fn run_leaky_call_in(
    echo: bool,
    reference_delay: Option<Duration>,
    transcription: crate::live::LiveTranscription,
) -> LeakRun {
    let sim = leaky_call(echo);
    let n = sim.mic.len() / earshot_core::aec::BLOCK * earshot_core::aec::BLOCK;
    let (mic, reference) = (sim.mic[..n].to_vec(), sim.reference[..n].to_vec());
    let far = region_words("far", &sim.far_regions, 0);
    let near = region_words("near", &sim.near_regions, 0);
    // The echo of each far word, at the bulk delay plus the direct path (about 2 ms).
    let echo_words: Vec<Word> = far
        .iter()
        .map(|w| w.shifted(MeetingTime::from_millis(102)))
        .filter(|w| w.end.as_millis() < (n / 16) as u64)
        .collect();
    let mut mic_words: Vec<Word> = near.iter().chain(&echo_words).cloned().collect();
    mic_words.sort_by_key(|w| w.start);
    let source = ReplayBuilder::new(ORIGIN, MonoBuffer::new(R16, mic.clone()))
        .reference(MonoBuffer::new(R16, reference.clone()))
        .build()
        .unwrap();
    let cfg = LiveConfig {
        pipeline: PipelineConfig {
            drift_correction: false,
            ..PipelineConfig::default()
        },
        transcription,
        ..config(earshot_core::model::ClockDomain::Replay)
    };
    let dir = TempDir::new();
    let svc = services(&dir);
    let session = start_live(
        source,
        svc.clone(),
        cfg,
        Some(leak_asr(
            cleaned(&mic, &reference),
            mic_words,
            reference,
            far,
            reference_delay,
        )),
        Box::new(ChaCha8Rng::seed_from_u64(62)),
    )
    .unwrap();
    let meeting = session.meeting();
    let (events, outcome) = collect(session);
    let stored_mic = {
        let store = svc.store();
        let mic_track = store
            .tracks(meeting)
            .unwrap()
            .into_iter()
            .find(|t| t.kind == TrackId::Mic)
            .unwrap()
            .id;
        live_segments(&**store, meeting, TranscriptPass::Live)
            .unwrap()
            .into_iter()
            .filter(|r| r.segment.track == mic_track)
            .flat_map(|r| texts(&r.segment.words))
            .collect()
    };
    LeakRun {
        events,
        outcome,
        stored_mic,
        near,
        echo: echo_words,
        svc,
        with_echo: echo,
        _dir: dir,
    }
}

#[test]
fn live_echo_leak_is_not_stored_as_me_and_near_words_are_kept() {
    let run = run_leaky_call(true);
    let lock_at = run
        .events
        .iter()
        .find_map(|e| match e {
            RuntimeEvent::Stats(s) if s.aec.is_some_and(|a| a.locked) => Some(s.end),
            _ => None,
        })
        .expect("the canceller locks on the echo path");
    assert!(!run.outcome.echo_locks.is_empty(), "the lock was recorded");
    // Echo heard once the lock existed is not stored on the mic track as a run: the live
    // path has only the text rule, which needs `min_run` (3) repeated words, so an
    // isolated echo word between the user's words can survive it (the known limit of the
    // rule; the final pass's speaker check cleans those, see the post tests).
    let is_echo = |t: &String| {
        run.echo
            .iter()
            .any(|w| &w.text == t && w.start.as_millis() > lock_at.as_millis() + 2_000)
    };
    let runs: Vec<&[String]> = run
        .stored_mic
        .windows(3)
        .filter(|w| w.iter().all(is_echo))
        .collect();
    assert!(runs.is_empty(), "echo runs stored as Me: {runs:?}");
    let late_echo = run
        .echo
        .iter()
        .filter(|w| w.start.as_millis() > lock_at.as_millis() + 2_000)
        .count();
    let late_stored = run.stored_mic.iter().filter(|t| is_echo(t)).count();
    assert!(
        late_stored * 5 <= late_echo,
        "{late_stored} of {late_echo} echo words after the lock were stored"
    );
    // Every near-end word the recogniser finalised is kept. (Near-end words the canceller
    // and the VAD let through to no final at all are not the filter's to keep.)
    let heard = recognised_near(&run);
    assert!(
        heard.len() * 2 >= run.near.len(),
        "{} of {} near words heard",
        heard.len(),
        run.near.len()
    );
    let missing: Vec<&String> = heard
        .iter()
        .filter(|w| !run.stored_mic.contains(w))
        .collect();
    assert!(missing.is_empty(), "near-end words lost: {missing:?}");
    assert!(run.outcome.echo_words_removed > 0);
    // Shown finals match what was stored (no echo shown either).
    let shown: Vec<String> = run
        .events
        .iter()
        .filter_map(|e| match e {
            RuntimeEvent::Transcript {
                track: TrackId::Mic,
                event: TranscriptEvent::Final(s),
            } => Some(texts(&s.words)),
            _ => None,
        })
        .flatten()
        .collect();
    assert_eq!(shown, run.stored_mic);
    // The delay and the live report are stored for the final pass.
    let store = run.svc.store();
    let locks = crate::echo::echo_locks(&store, run.outcome.meeting).unwrap();
    assert_eq!(locks, run.outcome.echo_locks);
    let report = crate::echo::echo_report(&store, run.outcome.meeting, TranscriptPass::Live)
        .unwrap()
        .unwrap();
    assert_eq!(report.text_removed, run.outcome.echo_words_removed);
}

#[test]
fn live_echo_headphones_without_a_lock_remove_nothing() {
    // No echo path: the canceller never locks. The mic transcript repeats the far end at
    // an echo-like offset (the user's own words, as far as the runtime can tell): kept.
    let run = run_leaky_call(false);
    assert!(
        run.events
            .iter()
            .all(|e| !matches!(e, RuntimeEvent::Stats(s) if s.aec.is_some_and(|a| a.locked)))
    );
    assert!(run.outcome.echo_locks.is_empty());
    assert_eq!(run.outcome.echo_words_removed, 0);
    let near = texts(&run.near);
    let missing: Vec<&String> = near
        .iter()
        .filter(|w| !run.stored_mic.contains(w))
        .collect();
    assert!(missing.is_empty(), "near-end words lost: {missing:?}");
    // The far-end copies the recogniser heard on the mic (where its VAD opened) are kept:
    // shown finals equal stored ones and nothing was discarded as echo.
    assert!(run.stored_mic.iter().any(|t| t.starts_with("far")));
    assert!(!run.events.iter().any(|e| matches!(
        e,
        RuntimeEvent::Transcript {
            track: TrackId::Mic,
            event: TranscriptEvent::Discarded { .. }
        }
    )));
}

/// A transcriber that takes `delay` per call (a slow CPU), then answers like `inner`.
struct SlowTranscriber {
    inner: crate::testutil::Timeline,
    delay: Duration,
    /// When given, every call first waits until it is set (at most [`GATE_GUARD`]).
    gate: Option<Arc<std::sync::atomic::AtomicBool>>,
    /// When given, every call meets the capture side ([`CaptureMeet`]) before its `delay`.
    meet: Option<CaptureMeet>,
}

/// Deadlock guard of the test transcribers' `gate` waits (a gate opens when a whole replay
/// has been delivered, which takes minutes on a loaded host): only reached when the gate
/// never opens (the failing case).
const GATE_GUARD: Duration = Duration::from_secs(600);

/// Guard of a [`CaptureMeet`] wait: one packet delivery (or the first decode), which a
/// session thread that is not blocked gives within far less on any host.
const MEET_GUARD: Duration = Duration::from_secs(60);

/// Waits until `ready()` (at most `guard`); whether it became ready.
fn wait_until(guard: Duration, ready: impl Fn() -> bool) -> bool {
    let since = std::time::Instant::now();
    while !ready() {
        if since.elapsed() >= guard {
            return false;
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    true
}

/// A rendezvous between a [`TimedSource`] and a [`SlowTranscriber`] that proves capture is
/// delivered while decodes run, without any timing assumption: every decode waits until
/// the source has delivered a packet during it (or has delivered everything). If the
/// session thread waited on a decode, that packet could never come: the first such decode
/// is counted in `missed` and panics when [`MEET_GUARD`] expires.
#[derive(Clone, Default)]
struct CaptureMeet {
    /// Set while a decode runs.
    decoding: Arc<std::sync::atomic::AtomicBool>,
    /// Deliveries so far.
    delivered: Arc<AtomicU64>,
    /// Deliveries made while a decode ran.
    during_decode: Arc<AtomicU64>,
    /// Decodes started.
    started: Arc<AtomicU64>,
    /// Decodes that saw no delivery although capture had audio left.
    missed: Arc<AtomicU64>,
    /// The source has delivered everything.
    capture_ended: Arc<std::sync::atomic::AtomicBool>,
}

impl CaptureMeet {
    /// Called by the transcriber at the start of a decode: returns once capture delivered
    /// during it (or delivered everything).
    fn decode_started(&self) {
        let before = self.during_decode.load(Ordering::SeqCst);
        self.decoding.store(true, Ordering::SeqCst);
        self.started.fetch_add(1, Ordering::SeqCst);
        let met = wait_until(MEET_GUARD, || {
            self.during_decode.load(Ordering::SeqCst) > before
                || self.capture_ended.load(Ordering::SeqCst)
        });
        if !met {
            self.missed.fetch_add(1, Ordering::SeqCst);
            panic!("a decode saw no capture delivery in {MEET_GUARD:?} while audio was left");
        }
    }

    fn decode_ended(&self) {
        self.decoding.store(false, Ordering::SeqCst);
    }
}

impl earshot_core::asr::Transcriber for SlowTranscriber {
    fn transcribe(
        &mut self,
        audio: &MonoBuffer,
        options: &earshot_core::asr::TranscribeOptions,
        cancel: &earshot_core::asr::CancelToken,
    ) -> Result<earshot_core::asr::Transcription, AsrError> {
        if let Some(gate) = &self.gate {
            assert!(
                wait_until(GATE_GUARD, || gate.load(Ordering::SeqCst)),
                "gate never opened"
            );
        }
        if let Some(meet) = &self.meet {
            meet.decode_started();
        }
        std::thread::sleep(self.delay);
        if let Some(meet) = &self.meet {
            meet.decode_ended();
        }
        self.inner.transcribe(audio, options, cancel)
    }

    fn backend_info(&self) -> BackendInfo {
        self.inner.backend_info()
    }
}

/// Wraps a source, counts its deliveries and meets the slow transcriber ([`CaptureMeet`]).
struct TimedSource<S> {
    inner: S,
    meet: CaptureMeet,
    /// After this many deliveries the source is idle until a decode has started, so audio
    /// is left to deliver during a decode however fast the replay runs on a loaded host.
    hold_at: u64,
    /// When the hold began: a hold longer than [`MEET_GUARD`] panics (no decode started).
    held_since: Option<std::time::Instant>,
}

impl<S: AudioSource> AudioSource for TimedSource<S> {
    type Error = S::Error;

    fn session(&self) -> &CaptureSession {
        self.inner.session()
    }

    fn pump(&mut self, sink: &mut dyn CaptureSink) -> Result<SourceStatus, Self::Error> {
        if self.meet.delivered.load(Ordering::SeqCst) == self.hold_at
            && self.meet.started.load(Ordering::SeqCst) == 0
        {
            let held = self
                .held_since
                .get_or_insert_with(std::time::Instant::now)
                .elapsed();
            assert!(
                held < MEET_GUARD,
                "no decode started in {MEET_GUARD:?} while capture was held"
            );
            return Ok(SourceStatus::Idle);
        }
        let status = self.inner.pump(sink)?;
        match status {
            SourceStatus::Delivered => {
                self.meet.delivered.fetch_add(1, Ordering::SeqCst);
                if self.meet.decoding.load(Ordering::SeqCst) {
                    self.meet.during_decode.fetch_add(1, Ordering::SeqCst);
                }
            }
            SourceStatus::Ended => self.meet.capture_ended.store(true, Ordering::SeqCst),
            SourceStatus::Idle => {}
        }
        Ok(status)
    }

    fn stop(&mut self) {
        self.inner.stop();
    }
}

#[test]
fn live_slow_decodes_never_stall_capture_or_the_recording() {
    // T6.10a: live ASR runs on a worker thread. With every decode taking 800 ms, the
    // session thread still pumps capture while each decode runs, every captured frame
    // reaches the FLAC, and every word is still transcribed. Proved by a rendezvous
    // (`CaptureMeet`), not by wall-clock gaps: a gap bound fails on a starved host (a
    // 1.9 s gap under full load with no decode involved).
    let dir = TempDir::new();
    let svc = services(&dir);
    let mut w = words("a", 1000, 5);
    w.extend(words("b", 5000, 4));
    let audio = speech(&w, 8000, 21);
    let source = ReplayBuilder::new(ORIGIN, MonoBuffer::new(R16, audio.clone()))
        .build()
        .unwrap();
    let meet = CaptureMeet::default();
    // Held after 4 s (the "a" words and the silence after them) until the first decode.
    let timed = TimedSource {
        inner: source,
        meet: meet.clone(),
        hold_at: 400,
        held_since: None,
    };
    let decode = Duration::from_millis(800);
    let mut slot = Some((audio.clone(), w.clone(), meet.clone()));
    let factory: LiveAsrFactory = Box::new(move |t, entity| {
        if t != TrackId::Mic {
            return Ok(None);
        }
        let Some((audio, w, meet)) = slot.take() else {
            return Ok(None);
        };
        Ok(Some(
            LiveAsr {
                transcriber: Box::new(SlowTranscriber {
                    inner: crate::testutil::timeline(audio, w),
                    delay: decode,
                    gate: None,
                    meet: Some(meet),
                }),
                vad: Box::new(EnergyVad::default()),
                config: ChunkerConfig::default(),
                track_entity: entity,
                segment_ids: ids(),
            }
            .into(),
        ))
    });
    let session = start_live(
        timed,
        svc.clone(),
        config(earshot_core::model::ClockDomain::Replay),
        Some(factory),
        Box::new(ChaCha8Rng::seed_from_u64(23)),
    )
    .unwrap();
    let meeting = session.meeting();
    let (events, outcome) = collect(session);

    // Capture went on while decodes ran (timing-free: on the session thread it is 0).
    assert!(
        meet.during_decode.load(Ordering::SeqCst) > 0,
        "no capture packet was delivered during a decode"
    );
    // Capture never waited on a decode: every decode that started while audio was left
    // saw a delivery before it ended.
    assert!(
        meet.started.load(Ordering::SeqCst) > 0,
        "nothing was decoded"
    );
    assert_eq!(
        meet.missed.load(Ordering::SeqCst),
        0,
        "a decode ended without a capture delivery while audio was left (capture waited)"
    );
    assert_eq!(
        meet.delivered.load(Ordering::SeqCst),
        (audio.len() / 160) as u64,
        "every 10 ms packet was delivered"
    );
    // Every captured frame is in the recording.
    let store = svc.store();
    let files = store.audio_files(meeting).unwrap();
    let (_, flac) = read_flac(&svc.data.resolve(&files[0].path).unwrap()).unwrap();
    assert_eq!(flac.len(), audio.len());
    // And the transcript is complete, finished before the session ended.
    let rows = live_segments(&**store, meeting, TranscriptPass::Live).unwrap();
    let stored: Vec<String> = rows.iter().flat_map(|r| texts(&r.segment.words)).collect();
    assert_eq!(stored, texts(&w));
    assert!(matches!(events.last(), Some(RuntimeEvent::Ended(o)) if *o == outcome));
    assert_eq!(
        outcome.final_pass.tracks[0].track_entity,
        Some(store.tracks(meeting).unwrap()[0].id)
    );
    assert!(warnings(&events).is_empty(), "{:?}", warnings(&events));
}

#[test]
fn live_echo_hold_waits_for_a_slow_far_end_worker() {
    // T6.10a review: with live ASR on per-track workers, the far end's transcript can lag
    // the mic's. The live echo filter must hold mic finals until the far-end worker has
    // decoded the stretch that could hold the repeated words (as it did when both tracks
    // were decoded in lockstep), so echo runs are still not stored as Me.
    // The hold makes the live result independent of how far the far end's worker lags:
    // exactly the words a lockstep run keeps (without the hold: 12 echo words kept and 12
    // removed against 6 and 18).
    let baseline = run_leaky_call_with(true, None);
    let run = run_leaky_call_with(true, Some(Duration::from_millis(1_500)));
    assert_eq!(run.stored_mic, baseline.stored_mic);
    assert_eq!(
        run.outcome.echo_words_removed,
        baseline.outcome.echo_words_removed
    );
    let lock_at = run
        .events
        .iter()
        .find_map(|e| match e {
            RuntimeEvent::Stats(s) if s.aec.is_some_and(|a| a.locked) => Some(s.end),
            _ => None,
        })
        .expect("the canceller locks on the echo path");
    let is_echo = |t: &String| {
        run.echo
            .iter()
            .any(|w| &w.text == t && w.start.as_millis() > lock_at.as_millis() + 2_000)
    };
    let runs: Vec<&[String]> = run
        .stored_mic
        .windows(3)
        .filter(|w| w.iter().all(is_echo))
        .collect();
    assert!(runs.is_empty(), "echo runs stored as Me: {runs:?}");
    // Every near-end word the recogniser finalised is kept.
    let missing: Vec<String> = recognised_near(&run)
        .into_iter()
        .filter(|w| !run.stored_mic.contains(w))
        .collect();
    assert!(missing.is_empty(), "near-end words lost: {missing:?}");
}

/// The near-end words in the live chunker's finals on the canceller output of `run`'s
/// call (before the echo filter), computed offline with the production `LiveTrack` fed
/// 10 ms packets. Its partials depend on how a worker batches its input (a coalesced
/// partial is decoded at other points; since T6.12a's follow-up a partial never decodes
/// past the padded speech end the final decodes, but its tentative words still differ),
/// so partials are not what the echo filter must keep; its finals must not, and this checks
/// that the run's own finals were the same: the filter only removes words and counts each
/// one, so the stored mic words must be an in-order subsequence of the offline finals that
/// leaves out exactly `echo_words_removed` of them.
fn recognised_near(run: &LeakRun) -> Vec<String> {
    use earshot_core::pipeline::{LiveTrack, PartialPolicy, PipelineEvent};
    let sim = leaky_call(run.with_echo);
    let n = sim.mic.len() / earshot_core::aec::BLOCK * earshot_core::aec::BLOCK;
    let mut mic_words: Vec<Word> = run.near.iter().chain(&run.echo).cloned().collect();
    mic_words.sort_by_key(|w| w.start);
    let stream = cleaned(&sim.mic[..n], &sim.reference[..n]);
    let mut live = LiveTrack::new(
        TrackId::Mic,
        LiveAsr {
            transcriber: Box::new(crate::testutil::timeline(stream.clone(), mic_words)),
            vad: Box::new(EnergyVad::default()),
            config: ChunkerConfig::default(),
            track_entity: EntityId::from_bytes([1; 16]),
            segment_ids: ids(),
        },
    )
    .unwrap();
    let mut events = Vec::new();
    for packet in stream.chunks(160) {
        live.feed(packet, u64::MAX, &mut events);
        live.run(PartialPolicy::Coalesce, &mut events);
    }
    live.finish(PartialPolicy::Coalesce, &mut events);
    let finals: Vec<String> = events
        .iter()
        .filter_map(|e| match e {
            PipelineEvent::Transcript {
                event: TranscriptEvent::Final(s),
                ..
            } => Some(texts(&s.words)),
            PipelineEvent::Error(e) => panic!("{e:?}"),
            _ => None,
        })
        .flatten()
        .collect();
    let mut rest = finals.iter();
    assert!(
        run.stored_mic.iter().all(|w| rest.any(|f| f == w)),
        "stored mic words are not a subsequence of the offline finals:
{:?}
{finals:?}",
        run.stored_mic
    );
    assert_eq!(
        finals.len(),
        run.stored_mic.len() + run.outcome.echo_words_removed,
        "the run's finals differ from the offline finals {finals:?}"
    );
    finals
        .into_iter()
        .filter(|t| t.starts_with("near"))
        .collect()
}

/// A transcriber that panics on its first call.
struct PanickingTranscriber(crate::testutil::Timeline);

impl earshot_core::asr::Transcriber for PanickingTranscriber {
    fn transcribe(
        &mut self,
        _: &MonoBuffer,
        _: &earshot_core::asr::TranscribeOptions,
        _: &earshot_core::asr::CancelToken,
    ) -> Result<earshot_core::asr::Transcription, AsrError> {
        panic!("simulated transcriber crash");
    }

    fn backend_info(&self) -> BackendInfo {
        self.0.backend_info()
    }
}

#[test]
fn live_crashed_worker_is_reported_and_the_recording_completes() {
    // T6.10a review: a live ASR worker thread that panics must not end the transcript
    // silently: the session reports a LiveAsr warning and still finalises the recording.
    let dir = TempDir::new();
    let svc = services(&dir);
    let w = words("a", 1000, 5);
    let audio = speech(&w, 4000, 31);
    let source = ReplayBuilder::new(ORIGIN, MonoBuffer::new(R16, audio.clone()))
        .build()
        .unwrap();
    let mut slot = Some((audio.clone(), w));
    let factory: LiveAsrFactory = Box::new(move |t, entity| {
        if t != TrackId::Mic {
            return Ok(None);
        }
        let Some((audio, w)) = slot.take() else {
            return Ok(None);
        };
        Ok(Some(
            LiveAsr {
                transcriber: Box::new(PanickingTranscriber(crate::testutil::timeline(audio, w))),
                vad: Box::new(EnergyVad::default()),
                config: ChunkerConfig::default(),
                track_entity: entity,
                segment_ids: ids(),
            }
            .into(),
        ))
    });
    let session = start_live(
        source,
        svc.clone(),
        config(earshot_core::model::ClockDomain::Replay),
        Some(factory),
        Box::new(ChaCha8Rng::seed_from_u64(33)),
    )
    .unwrap();
    let meeting = session.meeting();
    let (events, _) = collect(session);
    assert!(
        warnings(&events).iter().any(|w| matches!(
            w,
            RuntimeWarning::Pipeline(PipelineError::LiveAsr {
                track: TrackId::Mic,
                ..
            })
        )),
        "{:?}",
        warnings(&events)
    );
    let store = svc.store();
    let files = store.audio_files(meeting).unwrap();
    let (_, flac) = read_flac(&svc.data.resolve(&files[0].path).unwrap()).unwrap();
    assert_eq!(flac.len(), audio.len());
}

/// Live ASR on the mic with every decode taking `delay`, each first waiting for `gate`.
fn slow_asr_gated(
    audio: Vec<f32>,
    w: Vec<Word>,
    delay: Duration,
    gate: Option<Arc<std::sync::atomic::AtomicBool>>,
) -> LiveAsrFactory {
    let mut slot = Some((audio, w));
    Box::new(move |t, entity| {
        if t != TrackId::Mic {
            return Ok(None);
        }
        let Some((audio, w)) = slot.take() else {
            return Ok(None);
        };
        Ok(Some(
            LiveAsr {
                transcriber: Box::new(SlowTranscriber {
                    inner: crate::testutil::timeline(audio, w),
                    delay,
                    gate: gate.clone(),
                    meet: None,
                }),
                vad: Box::new(EnergyVad::default()),
                config: ChunkerConfig::default(),
                track_entity: entity,
                segment_ids: ids(),
            }
            .into(),
        ))
    })
}

/// Wraps a source and sets `ended` once the source has delivered everything (its pump
/// returned `Ended`), and records when that first happened in `ended_at`.
struct EndFlagSource<S> {
    inner: S,
    ended: Arc<std::sync::atomic::AtomicBool>,
    ended_at: Arc<std::sync::OnceLock<std::time::Instant>>,
}

impl<S: AudioSource> AudioSource for EndFlagSource<S> {
    type Error = S::Error;

    fn session(&self) -> &CaptureSession {
        self.inner.session()
    }

    fn pump(&mut self, sink: &mut dyn CaptureSink) -> Result<SourceStatus, Self::Error> {
        let status = self.inner.pump(sink)?;
        if status == SourceStatus::Ended {
            self.ended_at.get_or_init(std::time::Instant::now);
            self.ended.store(true, Ordering::SeqCst);
        }
        Ok(status)
    }

    fn stop(&mut self) {
        self.inner.stop();
    }
}

#[test]
fn live_backpressure_turns_partials_off_and_keeps_every_word() {
    // T6.10b: a worker more than 6 s behind decodes finals only and says so; every word
    // still reaches a stored final. Decodes wait until the replay has delivered all 20 s,
    // so the worker is behind however fast the replay runs.
    let dir = TempDir::new();
    let svc = services(&dir);
    let w = words("a", 1000, 40);
    let audio = speech(&w, 20_000, 41);
    let ended = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let source = EndFlagSource {
        inner: ReplayBuilder::new(ORIGIN, MonoBuffer::new(R16, audio.clone()))
            .build()
            .unwrap(),
        ended: Arc::clone(&ended),
        ended_at: Arc::default(),
    };
    let session = start_live(
        source,
        svc.clone(),
        config(earshot_core::model::ClockDomain::Replay),
        Some(slow_asr_gated(
            audio,
            w.clone(),
            Duration::from_millis(300),
            Some(ended),
        )),
        Box::new(ChaCha8Rng::seed_from_u64(43)),
    )
    .unwrap();
    let meeting = session.meeting();
    let (events, _) = collect(session);
    assert!(
        events.iter().any(|e| matches!(
            e,
            RuntimeEvent::LivePartials {
                track: TrackId::Mic,
                enabled: false,
                behind,
            } if *behind > crate::live::worker::PARTIALS_OFF_BEHIND
        )),
        "partials were never switched off"
    );
    let store = svc.store();
    let rows = live_segments(&**store, meeting, TranscriptPass::Live).unwrap();
    let stored: Vec<String> = rows.iter().flat_map(|r| texts(&r.segment.words)).collect();
    assert_eq!(stored, texts(&w));
}

#[test]
fn live_stop_does_not_wait_for_a_far_behind_transcript() {
    // T6.10b: at the end a track more than `max_stop_backlog` behind stops decoding (with
    // a warning) instead of making the end wait for minutes; the recording is complete.
    // Decodes wait until the replay has delivered all 60 s, so the worker is far behind
    // at the end however fast the replay runs on a loaded host.
    let dir = TempDir::new();
    let svc = services(&dir);
    let w = words("a", 1000, 120);
    let audio = speech(&w, 60_000, 51);
    let ended = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let ended_at = Arc::new(std::sync::OnceLock::new());
    let source = EndFlagSource {
        inner: ReplayBuilder::new(ORIGIN, MonoBuffer::new(R16, audio.clone()))
            .build()
            .unwrap(),
        ended: Arc::clone(&ended),
        ended_at: Arc::clone(&ended_at),
    };
    let cfg = LiveConfig {
        max_stop_backlog: Duration::from_secs(10),
        ..config(earshot_core::model::ClockDomain::Replay)
    };
    let session = start_live(
        source,
        svc.clone(),
        cfg,
        Some(slow_asr_gated(
            audio.clone(),
            w,
            Duration::from_millis(500),
            Some(ended),
        )),
        Box::new(ChaCha8Rng::seed_from_u64(53)),
    )
    .unwrap();
    let meeting = session.meeting();
    let (events, outcome) = collect(session);
    let cut = warnings(&events).into_iter().find_map(|w| match w {
        RuntimeWarning::LiveCutAtStop { track, behind } => Some((track, behind)),
        _ => None,
    });
    let (track, behind) = cut.expect("the far-behind transcript was cut at the end");
    assert_eq!(track, TrackId::Mic);
    assert!(behind > Duration::from_secs(10), "{behind:?}");
    // The worker's queue reaches the stats through the pipeline (registered as an external
    // live track, T6.10a).
    let largest = events
        .iter()
        .filter_map(|e| match e {
            RuntimeEvent::Stats(s) => Some(s.mic.asr_backlog),
            _ => None,
        })
        .max();
    assert!(
        largest.is_some_and(|b| b > 10 * 16_000),
        "largest reported backlog {largest:?}"
    );
    // Decoding all of it would take about 60 s of 500 ms decodes; the end did not wait.
    // Measured from the end of capture: a loaded host can take longer than that to replay
    // the 60 s, which is not the end waiting.
    let waited = ended_at
        .get()
        .expect("the replay delivered everything")
        .elapsed();
    assert!(waited < Duration::from_secs(30), "{waited:?}");
    let store = svc.store();
    let files = store.audio_files(meeting).unwrap();
    let (_, flac) = read_flac(&svc.data.resolve(&files[0].path).unwrap()).unwrap();
    assert_eq!(flac.len(), audio.len());
    assert!(matches!(events.last(), Some(RuntimeEvent::Ended(o)) if *o == outcome));
}

/// The words the whole-track pass hears on `audio` with the fake timeline transcriber,
/// the default VAD and [`stretches`]' piece config: what live speech stretches must
/// produce on the same audio (D20).
fn whole_track_words(audio: Vec<f32>, w: Vec<Word>) -> Vec<String> {
    let crate::live::LiveTranscription::Stretches(pieces) = stretches() else {
        unreachable!()
    };
    let (t, _) = earshot_core::asr::transcribe_in_pieces(
        &mut crate::testutil::timeline(audio.clone(), w),
        &mut EnergyVad::default(),
        // The samples the recording stores, which the stretches decode (T6.13e).
        &MonoBuffer::new(R16, earshot_core::audio::quantise_pcm16(&audio)),
        &ChunkerConfig::default().options,
        &pieces,
        &earshot_core::asr::CancelToken::new(),
    )
    .unwrap();
    t.segments
        .iter()
        .flat_map(|s| s.words.iter().map(|w| w.text.clone()))
        .collect()
}

/// The mic track's finals and the number of its partials.
fn mic_events(events: &[RuntimeEvent]) -> (Vec<earshot_core::asr::Segment>, usize) {
    let mut finals = Vec::new();
    let mut partials = 0;
    for e in events {
        match e {
            RuntimeEvent::Transcript {
                track: TrackId::Mic,
                event: TranscriptEvent::Final(s),
            } => finals.push(s.clone()),
            RuntimeEvent::Transcript {
                track: TrackId::Mic,
                event: TranscriptEvent::Partial(_),
            } => partials += 1,
            _ => {}
        }
    }
    (finals, partials)
}

#[test]
fn live_stretch_session_stores_the_whole_track_result_without_partials() {
    // T6.13b: speech stretches show finals only, one per decoded piece, and store exactly
    // the words the whole-track pass hears on the same audio.
    let dir = TempDir::new();
    let svc = services(&dir);
    let mut w = words("a", 1000, 5);
    w.extend(words("b", 6000, 4));
    let audio = speech(&w, 9000, 71);
    let source = ReplayBuilder::new(ORIGIN, MonoBuffer::new(R16, audio.clone()))
        .build()
        .unwrap();
    let session = start_live(
        source,
        svc.clone(),
        stretch_config(earshot_core::model::ClockDomain::Replay),
        Some(fake_asr(TrackId::Mic, audio.clone(), w.clone())),
        Box::new(ChaCha8Rng::seed_from_u64(72)),
    )
    .unwrap();
    let meeting = session.meeting();
    let (events, outcome) = collect(session);
    let (finals, partials) = mic_events(&events);
    assert_eq!(partials, 0, "speech stretches show no partials");
    assert!(!finals.is_empty());
    assert!(warnings(&events).is_empty(), "{:?}", warnings(&events));
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, RuntimeEvent::LivePartials { .. }))
    );
    let expected = whole_track_words(audio, w.clone());
    assert_eq!(expected, texts(&w), "the fake hears every word");
    let store = svc.store();
    let rows = live_segments(&**store, meeting, TranscriptPass::Live).unwrap();
    let stored: Vec<String> = rows.iter().flat_map(|r| texts(&r.segment.words)).collect();
    assert_eq!(stored, expected);
    let stored_segments: Vec<_> = rows.into_iter().map(|r| r.segment).collect();
    assert_eq!(stored_segments, finals);
    assert!(matches!(events.last(), Some(RuntimeEvent::Ended(o)) if *o == outcome));
    assert!(outcome.problems.is_empty(), "{:?}", outcome.problems);
}

/// Forwards to a sink and counts the frames of every track.
struct CountingSink<'a> {
    inner: &'a mut dyn CaptureSink,
    frames: &'a mut usize,
}

impl CaptureSink for CountingSink<'_> {
    fn frames(&mut self, track: TrackId, frames: &[f32], device_ts: DeviceTime) {
        *self.frames += frames.len();
        self.inner.frames(track, frames, device_ts);
    }
    fn discontinuity(&mut self, track: TrackId, discontinuity: Discontinuity) {
        self.inner.discontinuity(track, discontinuity);
    }
    fn end(&mut self, end: EndOfStream) {
        self.inner.end(end);
    }
}

/// Wraps a mono source and delivers nothing more once `hold_at` frames are out until
/// `release` says so (or 60 s pass, recorded in `timed_out`).
struct HoldSource<S> {
    inner: S,
    hold_at: usize,
    delivered: usize,
    release: Box<dyn FnMut() -> bool + Send>,
    held_since: Option<std::time::Instant>,
    released: bool,
    timed_out: Arc<std::sync::atomic::AtomicBool>,
}

impl<S: AudioSource> AudioSource for HoldSource<S> {
    type Error = S::Error;

    fn session(&self) -> &CaptureSession {
        self.inner.session()
    }

    fn pump(&mut self, sink: &mut dyn CaptureSink) -> Result<SourceStatus, Self::Error> {
        if !self.released && self.delivered >= self.hold_at {
            let since = *self.held_since.get_or_insert_with(std::time::Instant::now);
            if (self.release)() {
                self.released = true;
            } else if since.elapsed() > Duration::from_secs(60) {
                self.timed_out.store(true, Ordering::SeqCst);
                self.released = true;
            } else {
                return Ok(SourceStatus::Idle);
            }
        }
        let mut counting = CountingSink {
            inner: sink,
            frames: &mut self.delivered,
        };
        self.inner.pump(&mut counting)
    }

    fn stop(&mut self) {
        self.inner.stop();
    }
}

#[test]
fn live_stretch_appears_during_the_session() {
    // T6.13b: a speech stretch is stored as soon as it is settled, while the meeting goes
    // on: the source holds back everything after 8 s until the first stretch (words at
    // 1-3 s, then silence) is stored as the live pass.
    let dir = TempDir::new();
    let svc = services(&dir);
    let mut w = words("a", 1000, 5);
    w.extend(words("b", 9000, 5));
    let audio = speech(&w, 12_000, 101);
    let meeting_slot: Arc<std::sync::Mutex<Option<EntityId>>> = Arc::default();
    let timed_out = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let release = {
        let (svc, slot) = (svc.clone(), Arc::clone(&meeting_slot));
        Box::new(move || {
            let Some(meeting) = *slot.lock().unwrap() else {
                return false;
            };
            let store = svc.store();
            !live_segments(&**store, meeting, TranscriptPass::Live)
                .unwrap()
                .is_empty()
        })
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
        stretch_config(earshot_core::model::ClockDomain::Replay),
        Some(fake_asr(TrackId::Mic, audio.clone(), w.clone())),
        Box::new(ChaCha8Rng::seed_from_u64(102)),
    )
    .unwrap();
    let meeting = session.meeting();
    *meeting_slot.lock().unwrap() = Some(meeting);
    let (events, outcome) = collect(session);
    assert!(
        !timed_out.load(Ordering::SeqCst),
        "no stretch was stored while the source held back the rest of the meeting"
    );
    assert_eq!(outcome.end, MeetingTime::from_millis(12_000));
    let (finals, partials) = mic_events(&events);
    assert_eq!(partials, 0);
    assert!(finals.len() >= 2, "{finals:?}");
    let store = svc.store();
    let rows = live_segments(&**store, meeting, TranscriptPass::Live).unwrap();
    let stored: Vec<String> = rows.iter().flat_map(|r| texts(&r.segment.words)).collect();
    assert_eq!(stored, texts(&w));
}

/// No echo run stored as Me and at most a fifth of the echo heard 2 s after the lock
/// stored (the live text rule's limits, as in the chunker's
/// `live_echo_leak_is_not_stored_as_me_and_near_words_are_kept`).
fn assert_echo_cleaned(run: &LeakRun) {
    let lock_at = run
        .events
        .iter()
        .find_map(|e| match e {
            RuntimeEvent::Stats(s) if s.aec.is_some_and(|a| a.locked) => Some(s.end),
            _ => None,
        })
        .expect("the canceller locks on the echo path");
    let is_echo = |t: &String| {
        run.echo
            .iter()
            .any(|w| &w.text == t && w.start.as_millis() > lock_at.as_millis() + 2_000)
    };
    let runs: Vec<&[String]> = run
        .stored_mic
        .windows(3)
        .filter(|w| w.iter().all(is_echo))
        .collect();
    assert!(runs.is_empty(), "echo runs stored as Me: {runs:?}");
    let late_echo = run
        .echo
        .iter()
        .filter(|w| w.start.as_millis() > lock_at.as_millis() + 2_000)
        .count();
    let late_stored = run.stored_mic.iter().filter(|t| is_echo(t)).count();
    assert!(
        late_stored * 5 <= late_echo,
        "{late_stored} of {late_echo} echo words after the lock were stored"
    );
    assert!(run.outcome.echo_words_removed > 0);
}

#[test]
fn live_stretch_echo_leak_is_not_stored_as_me_and_near_words_are_kept() {
    // The live echo rule cleans speech stretches as it cleans the chunker's finals: no echo
    // run stored as Me, most echo heard after the lock removed, and every near-end word
    // the whole-track pass hears on the cleaned mic kept.
    let run = run_leaky_call_in(true, None, stretches());
    assert_echo_cleaned(&run);
    let (finals, partials) = mic_events(&run.events);
    assert_eq!(partials, 0);
    let shown: Vec<String> = finals.iter().flat_map(|s| texts(&s.words)).collect();
    assert_eq!(shown, run.stored_mic);
    // Near-end words heard by the whole-track pass on the canceller output.
    let sim = leaky_call(true);
    let n = sim.mic.len() / earshot_core::aec::BLOCK * earshot_core::aec::BLOCK;
    let mut mic_words: Vec<Word> = run.near.iter().chain(&run.echo).cloned().collect();
    mic_words.sort_by_key(|w| w.start);
    let heard: Vec<String> =
        whole_track_words(cleaned(&sim.mic[..n], &sim.reference[..n]), mic_words)
            .into_iter()
            .filter(|t| t.starts_with("near"))
            .collect();
    assert!(
        heard.len() * 2 >= run.near.len(),
        "{} of {} near words heard",
        heard.len(),
        run.near.len()
    );
    let missing: Vec<&String> = heard
        .iter()
        .filter(|w| !run.stored_mic.contains(w))
        .collect();
    assert!(missing.is_empty(), "near-end words lost: {missing:?}");
}

#[test]
fn live_stretch_echo_hold_waits_for_a_slow_far_end_worker() {
    // As `live_echo_hold_waits_for_a_slow_far_end_worker`, with speech stretches: mic
    // pieces are held until the far end's decoded frontier passes them, so the result does
    // not depend on how far the far end's worker lags.
    let baseline = run_leaky_call_in(true, None, stretches());
    let run = run_leaky_call_in(true, Some(Duration::from_millis(1_500)), stretches());
    assert_eq!(run.stored_mic, baseline.stored_mic);
    assert_eq!(
        run.outcome.echo_words_removed,
        baseline.outcome.echo_words_removed
    );
    assert_echo_cleaned(&run);
}

#[test]
fn live_stretch_stop_finishes_the_queue() {
    // D20: speech stretches are the transcript, so at the end a worker far behind finishes
    // its queue instead of being cut (the chunker's `max_stop_backlog` does not apply) and
    // never switches partials. The chunker run on the same audio is the control: it is
    // far enough behind to be cut. Decodes wait until the replay has delivered all 25 s, so
    // the worker is far behind at the end however fast the replay runs on a loaded host.
    let mut w = Vec::new();
    for k in 0..4 {
        w.extend(words(&format!("g{k}w"), 1000 + 6000 * k, 8));
    }
    let audio = speech(&w, 25_000, 81);
    let run = |transcription| {
        let dir = TempDir::new();
        let svc = services(&dir);
        let ended = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let source = EndFlagSource {
            inner: ReplayBuilder::new(ORIGIN, MonoBuffer::new(R16, audio.clone()))
                .build()
                .unwrap(),
            ended: Arc::clone(&ended),
            ended_at: Arc::default(),
        };
        let cfg = LiveConfig {
            max_stop_backlog: Duration::from_secs(1),
            transcription,
            ..config(earshot_core::model::ClockDomain::Replay)
        };
        let delay = Duration::from_millis(700);
        let session = start_live(
            source,
            svc.clone(),
            cfg,
            Some(slow_asr_gated(audio.clone(), w.clone(), delay, Some(ended))),
            Box::new(ChaCha8Rng::seed_from_u64(82)),
        )
        .unwrap();
        let meeting = session.meeting();
        let (events, outcome) = collect(session);
        assert!(matches!(events.last(), Some(RuntimeEvent::Ended(o)) if *o == outcome));
        let store = svc.store();
        let rows = live_segments(&**store, meeting, TranscriptPass::Live).unwrap();
        let stored: Vec<String> = rows.iter().flat_map(|r| texts(&r.segment.words)).collect();
        let cut = warnings(&events)
            .iter()
            .any(|x| matches!(x, RuntimeWarning::LiveCutAtStop { .. }));
        (events, stored, cut)
    };
    let (_, _, chunker_cut) = run(crate::live::LiveTranscription::Chunker);
    assert!(chunker_cut, "control: the chunker is cut at the end");
    let (events, stored, cut) = run(stretches());
    assert!(!cut, "{:?}", warnings(&events));
    // Decodes start only when the replay has ended, and each takes 700 ms: the session's
    // own measure of the catch-up holds at least one of them.
    let after: Vec<Duration> = events
        .iter()
        .filter_map(|e| match e {
            RuntimeEvent::LiveTranscriptFinished { after_capture_end } => Some(*after_capture_end),
            _ => None,
        })
        .collect();
    assert_eq!(after.len(), 1);
    assert!(after[0] >= Duration::from_millis(700), "{after:?}");
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, RuntimeEvent::LivePartials { .. }))
    );
    assert_eq!(stored, whole_track_words(audio.clone(), w.clone()));
    assert_eq!(stored, texts(&w));
}

#[test]
fn live_stretch_failing_model_is_reported_and_the_recording_completes() {
    let dir = TempDir::new();
    let svc = services(&dir);
    let w = words("a", 500, 10);
    let audio = speech(&w, 6000, 91);
    let factory: LiveAsrFactory = Box::new(|_, entity| {
        Ok(Some(
            LiveAsr {
                transcriber: Box::new(FakeTranscriber::scripted([Err::<Vec<RawSegment>, _>(
                    AsrError::Backend("gpu lost".into()),
                )])),
                vad: Box::new(EnergyVad::default()),
                config: ChunkerConfig::default(),
                track_entity: entity,
                segment_ids: ids(),
            }
            .into(),
        ))
    });
    let source = ReplayBuilder::new(ORIGIN, MonoBuffer::new(R16, audio.clone()))
        .build()
        .unwrap();
    let session = start_live(
        source,
        svc.clone(),
        stretch_config(earshot_core::model::ClockDomain::Replay),
        Some(factory),
        Box::new(ChaCha8Rng::seed_from_u64(92)),
    )
    .unwrap();
    let meeting = session.meeting();
    let (events, outcome) = collect(session);
    assert!(warnings(&events).iter().any(|x| matches!(
        x,
        RuntimeWarning::Pipeline(PipelineError::LiveAsr {
            track: TrackId::Mic,
            ..
        })
    )));
    let store = svc.store();
    let files = store.audio_files(meeting).unwrap();
    let (_, flac) = read_flac(&svc.data.resolve(&files[0].path).unwrap()).unwrap();
    assert_eq!(flac.len(), audio.len());
    assert!(outcome.final_pass.tracks[0].track_entity.is_some());
    assert!(
        live_segments(&**store, meeting, TranscriptPass::Live)
            .unwrap()
            .is_empty()
    );
}

#[test]
fn live_stretch_crashed_worker_is_reported_and_the_recording_completes() {
    let dir = TempDir::new();
    let svc = services(&dir);
    let w = words("a", 1000, 5);
    let audio = speech(&w, 4000, 93);
    let source = ReplayBuilder::new(ORIGIN, MonoBuffer::new(R16, audio.clone()))
        .build()
        .unwrap();
    let mut slot = Some((audio.clone(), w));
    let factory: LiveAsrFactory = Box::new(move |t, entity| {
        if t != TrackId::Mic {
            return Ok(None);
        }
        let Some((audio, w)) = slot.take() else {
            return Ok(None);
        };
        Ok(Some(
            LiveAsr {
                transcriber: Box::new(PanickingTranscriber(crate::testutil::timeline(audio, w))),
                vad: Box::new(EnergyVad::default()),
                config: ChunkerConfig::default(),
                track_entity: entity,
                segment_ids: ids(),
            }
            .into(),
        ))
    });
    let session = start_live(
        source,
        svc.clone(),
        stretch_config(earshot_core::model::ClockDomain::Replay),
        Some(factory),
        Box::new(ChaCha8Rng::seed_from_u64(94)),
    )
    .unwrap();
    let meeting = session.meeting();
    let (events, _) = collect(session);
    assert!(
        warnings(&events).iter().any(|x| matches!(
            x,
            RuntimeWarning::Pipeline(PipelineError::LiveAsr {
                track: TrackId::Mic,
                ..
            })
        )),
        "{:?}",
        warnings(&events)
    );
    let store = svc.store();
    let files = store.audio_files(meeting).unwrap();
    let (_, flac) = read_flac(&svc.data.resolve(&files[0].path).unwrap()).unwrap();
    assert_eq!(flac.len(), audio.len());
}

/// The mic's stretch record of `meeting`.
fn mic_stretches(svc: &Services, meeting: EntityId) -> Option<crate::stretches::StretchRecord> {
    match crate::stretches::stretch_record(&svc.store(), meeting, TrackId::Mic).unwrap()? {
        crate::stretches::StoredStretches::Record(r) => Some(*r),
        crate::stretches::StoredStretches::Unreadable(why) => panic!("unreadable: {why}"),
    }
}

/// The words of a record's pieces, as decoded.
fn record_words(r: &crate::stretches::StretchRecord) -> Vec<String> {
    r.decoded
        .pieces
        .iter()
        .flat_map(|p| p.segments.iter().flat_map(|s| texts(&s.words)))
        .collect()
}

#[test]
fn live_stretch_session_records_its_pieces_for_the_final_pass() {
    // T6.13c: a stretch worker leaves a record of what it decoded (raw pieces, frontier,
    // model, options, piece settings, VAD frame), and the final pass is asked to decode
    // like it (greedy), so the cleanup can reuse the stretches.
    let dir = TempDir::new();
    let svc = services(&dir);
    let mut w = words("a", 1000, 5);
    w.extend(words("b", 6000, 4));
    let audio = speech(&w, 9000, 171);
    let source = ReplayBuilder::new(ORIGIN, MonoBuffer::new(R16, audio.clone()))
        .build()
        .unwrap();
    let session = start_live(
        source,
        svc.clone(),
        stretch_config(earshot_core::model::ClockDomain::Replay),
        Some(fake_asr(TrackId::Mic, audio.clone(), w.clone())),
        Box::new(ChaCha8Rng::seed_from_u64(172)),
    )
    .unwrap();
    let meeting = session.meeting();
    let (_, outcome) = collect(session);
    let record = mic_stretches(&svc, meeting).expect("a stretch record");
    let crate::live::LiveTranscription::Stretches(pieces) = stretches() else {
        unreachable!()
    };
    assert_eq!(record.backend.model, "fake");
    assert_eq!(
        record.vad,
        earshot_core::asr::Vad::id(&EnergyVad::default())
    );
    assert_eq!(record.options, ChunkerConfig::default().options);
    assert_eq!(record.pieces, pieces);
    assert_eq!(
        record.vad_frame,
        earshot_core::asr::Vad::frame_len(&EnergyVad::default())
    );
    assert_eq!(record.decoded.through, audio.len());
    assert_eq!(record.stopped, None);
    assert_eq!(record_words(&record), whole_track_words(audio, w));
    assert_eq!(outcome.final_pass.tracks[0].options, record.options);
}

/// A transcriber that answers like `inner` but fails (or panics) on call `on` (1-based).
struct FailOn {
    inner: crate::testutil::Timeline,
    on: usize,
    panic: bool,
    calls: usize,
}

impl earshot_core::asr::Transcriber for FailOn {
    fn transcribe(
        &mut self,
        audio: &MonoBuffer,
        options: &earshot_core::asr::TranscribeOptions,
        cancel: &earshot_core::asr::CancelToken,
    ) -> Result<earshot_core::asr::Transcription, AsrError> {
        self.calls += 1;
        if self.calls == self.on {
            assert!(!self.panic, "simulated transcriber crash");
            return Err(AsrError::Backend("gpu lost".into()));
        }
        self.inner.transcribe(audio, options, cancel)
    }

    fn backend_info(&self) -> BackendInfo {
        self.inner.backend_info()
    }
}

/// A stretch session over two turns (two pieces) whose transcriber fails on its second
/// call; the mic's record and the stored live words.
fn failing_second_piece(panic: bool) -> (crate::stretches::StretchRecord, Vec<String>) {
    let dir = TempDir::new();
    let svc = services(&dir);
    let mut w = words("a", 1000, 5);
    w.extend(words("b", 9000, 5));
    let audio = speech(&w, 12_000, 173);
    let mut slot = Some((audio.clone(), w));
    let factory: LiveAsrFactory = Box::new(move |t, entity| {
        if t != TrackId::Mic {
            return Ok(None);
        }
        let Some((audio, w)) = slot.take() else {
            return Ok(None);
        };
        Ok(Some(
            LiveAsr {
                transcriber: Box::new(FailOn {
                    inner: crate::testutil::timeline(audio, w),
                    on: 2,
                    panic,
                    calls: 0,
                }),
                vad: Box::new(EnergyVad::default()),
                config: ChunkerConfig::default(),
                track_entity: entity,
                segment_ids: ids(),
            }
            .into(),
        ))
    });
    let source = ReplayBuilder::new(ORIGIN, MonoBuffer::new(R16, audio.clone()))
        .build()
        .unwrap();
    let session = start_live(
        source,
        svc.clone(),
        stretch_config(earshot_core::model::ClockDomain::Replay),
        Some(factory),
        Box::new(ChaCha8Rng::seed_from_u64(174)),
    )
    .unwrap();
    let meeting = session.meeting();
    let _ = collect(session);
    let store = svc.store();
    let rows = live_segments(&**store, meeting, TranscriptPass::Live).unwrap();
    let stored = rows.iter().flat_map(|r| texts(&r.segment.words)).collect();
    drop(store);
    (
        mic_stretches(&svc, meeting).expect("a stretch record"),
        stored,
    )
}

/// Answers like `inner`, reporting CUDA until its first call and the CPU after it (a
/// per-call GPU fallback, as `VerifiedWhisper` reports one).
struct FallsBackToCpu {
    inner: crate::testutil::Timeline,
    called: bool,
}

impl earshot_core::asr::Transcriber for FallsBackToCpu {
    fn transcribe(
        &mut self,
        audio: &MonoBuffer,
        options: &earshot_core::asr::TranscribeOptions,
        cancel: &earshot_core::asr::CancelToken,
    ) -> Result<earshot_core::asr::Transcription, AsrError> {
        self.called = true;
        self.inner.transcribe(audio, options, cancel)
    }

    fn backend_info(&self) -> BackendInfo {
        let (kind, device) = if self.called {
            (earshot_core::asr::BackendKind::Cpu, "cpu")
        } else {
            (earshot_core::asr::BackendKind::Cuda, "gpu")
        };
        BackendInfo {
            kind,
            device: device.into(),
            model: self.inner.backend_info().model,
        }
    }
}

#[test]
fn live_stretch_record_keeps_the_backend_proven_at_the_end_of_the_run() {
    // The record's backend becomes the final pass's when every piece is reused, so it is
    // the backend reported after the last decode, not the one reported at the start.
    let dir = TempDir::new();
    let svc = services(&dir);
    let mut w = words("a", 1000, 5);
    w.extend(words("b", 9000, 5));
    let audio = speech(&w, 12_000, 177);
    let mut slot = Some((audio.clone(), w));
    let factory: LiveAsrFactory = Box::new(move |t, entity| {
        if t != TrackId::Mic {
            return Ok(None);
        }
        let Some((audio, w)) = slot.take() else {
            return Ok(None);
        };
        Ok(Some(
            LiveAsr {
                transcriber: Box::new(FallsBackToCpu {
                    inner: crate::testutil::timeline(audio, w),
                    called: false,
                }),
                vad: Box::new(EnergyVad::default()),
                config: ChunkerConfig::default(),
                track_entity: entity,
                segment_ids: ids(),
            }
            .into(),
        ))
    });
    let source = ReplayBuilder::new(ORIGIN, MonoBuffer::new(R16, audio.clone()))
        .build()
        .unwrap();
    let session = start_live(
        source,
        svc.clone(),
        stretch_config(earshot_core::model::ClockDomain::Replay),
        Some(factory),
        Box::new(ChaCha8Rng::seed_from_u64(178)),
    )
    .unwrap();
    let meeting = session.meeting();
    let _ = collect(session);
    let record = mic_stretches(&svc, meeting).expect("a stretch record");
    assert_eq!(record.decoded.pieces.len(), 2);
    assert_eq!(record.backend.kind, earshot_core::asr::BackendKind::Cpu);
    assert_eq!(record.backend.device, "cpu");
}

#[test]
fn live_stretch_record_says_where_a_failed_model_stopped() {
    let (record, stored) = failing_second_piece(false);
    assert_eq!(stored, ["a0", "a1", "a2", "a3", "a4"]);
    let stopped = record.stopped.clone().expect("stopped early");
    assert!(stopped.contains("gpu lost"), "{stopped}");
    // Whatever the worker had decoded before the failure is kept, and the frontier never
    // passes speech that is not in a kept piece (the second turn starts at 9 s).
    assert_eq!(record.decoded.pieces.len(), 1, "{record:?}");
    assert!(record.decoded.through <= 9 * 16_000 - 400 * 16);
    for p in &record.decoded.pieces {
        assert!(p.span.end <= record.decoded.through);
    }
}

#[test]
fn live_stretch_record_says_where_a_crashed_worker_stopped() {
    // Also when the first piece and the crash fall in one drain of the worker (a loaded
    // host): the first piece is logged and its words still shown and stored.
    let (record, stored) = failing_second_piece(true);
    assert_eq!(stored, ["a0", "a1", "a2", "a3", "a4"]);
    let stopped = record.stopped.clone().expect("stopped early");
    assert!(stopped.contains("crashed"), "{stopped}");
    assert_eq!(record.decoded.pieces.len(), 1, "{record:?}");
    assert!(record.decoded.through <= 9 * 16_000 - 400 * 16);
}

#[test]
fn live_chunker_session_keeps_no_stretch_record_and_a_batch_final_pass() {
    let dir = TempDir::new();
    let svc = services(&dir);
    let w = words("a", 1000, 5);
    let audio = speech(&w, 4000, 175);
    let source = ReplayBuilder::new(ORIGIN, MonoBuffer::new(R16, audio.clone()))
        .build()
        .unwrap();
    let session = start_live(
        source,
        svc.clone(),
        config(earshot_core::model::ClockDomain::Replay),
        Some(fake_asr(TrackId::Mic, audio, w)),
        Box::new(ChaCha8Rng::seed_from_u64(176)),
    )
    .unwrap();
    let meeting = session.meeting();
    let (_, outcome) = collect(session);
    assert!(mic_stretches(&svc, meeting).is_none());
    assert_eq!(
        outcome.final_pass.tracks[0].options,
        earshot_core::asr::TranscribeOptions::batch()
    );
}

/// The voice-activity changes of `track` in `events`, in order.
fn voice_changes(events: &[RuntimeEvent], track: TrackId) -> Vec<(bool, u64)> {
    events
        .iter()
        .filter_map(|e| match e {
            RuntimeEvent::VoiceActivity {
                track: t,
                speaking,
                at,
            } if *t == track => Some((*speaking, at.as_millis())),
            _ => None,
        })
        .collect()
}

/// `got` has the same switches as `want`, each within `tolerance_ms`.
fn assert_switches(got: &[(bool, u64)], want: &[(bool, u64)], tolerance_ms: u64) {
    assert_eq!(got.len(), want.len(), "{got:?} vs {want:?}");
    for (g, w) in got.iter().zip(want) {
        assert_eq!(g.0, w.0, "{got:?} vs {want:?}");
        assert!(g.1.abs_diff(w.1) <= tolerance_ms, "{got:?} vs {want:?}");
    }
}

#[test]
fn live_voice_activity_follows_the_speech_of_each_track() {
    // Mic speech at 1.0-2.9 s and 5.0-6.5 s (words 300 ms long, 100 ms apart: one stretch
    // each), the far end at 3.0-4.1 s; quiet noise (-80 dBFS) everywhere else.
    let dir = TempDir::new();
    let svc = services(&dir);
    let mut me = words("a", 1000, 5);
    me.extend(words("b", 5000, 4));
    let them = words("r", 3000, 3);
    let source = ReplayBuilder::new(ORIGIN, MonoBuffer::new(R16, speech(&me, 8000, 31)))
        .reference(MonoBuffer::new(R16, speech(&them, 8000, 32)))
        .build()
        .unwrap();
    let session = start_live(
        source,
        svc,
        config(earshot_core::model::ClockDomain::Replay),
        None,
        Box::new(ChaCha8Rng::seed_from_u64(33)),
    )
    .unwrap();
    let (events, _) = collect(session);
    // Within two 30 ms VAD frames of the speech boundaries.
    assert_switches(
        &voice_changes(&events, TrackId::Mic),
        &[(true, 1000), (false, 2900), (true, 5000), (false, 6500)],
        60,
    );
    assert_switches(
        &voice_changes(&events, TrackId::Reference),
        &[(true, 3000), (false, 4100)],
        60,
    );
    // An "off" is sent once the release has passed: after audio events beyond its time.
    let first_off = events
        .iter()
        .position(|e| {
            matches!(
                e,
                RuntimeEvent::VoiceActivity {
                    speaking: false,
                    ..
                }
            )
        })
        .unwrap();
    assert!(
        events[..first_off]
            .iter()
            .any(|e| matches!(e, RuntimeEvent::Levels(l) if l.end.as_millis() >= 3_300)),
        "{events:?}"
    );
    // No transcription: no transcript lag.
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, RuntimeEvent::TranscriptLag { .. }))
    );
}

/// The far-end warnings of a session with `mic` and `far` speech over `total_ms` (`far`
/// `None`: no reference track; empty: a digitally silent reference, as a process loopback of
/// the wrong process delivers), mic speech alone warning after 3 s.
fn far_end_warnings(
    mic: &[Word],
    far: Option<&[Word]>,
    total_ms: u64,
    seed: u64,
) -> Vec<RuntimeWarning> {
    let dir = TempDir::new();
    let svc = services(&dir);
    let builder = ReplayBuilder::new(ORIGIN, MonoBuffer::new(R16, speech(mic, total_ms, seed)));
    let builder = match far {
        None => builder,
        Some([]) => builder.reference(MonoBuffer::new(R16, vec![0.0; (total_ms * 16) as usize])),
        Some(far) => builder.reference(MonoBuffer::new(R16, speech(far, total_ms, seed + 1))),
    };
    let mut cfg = config(earshot_core::model::ClockDomain::Replay);
    cfg.voice_activity.far_end_silent_after = Duration::from_secs(3);
    cfg.voice_activity.far_end_heard_after = Duration::from_secs(1);
    let session = start_live(
        builder.build().unwrap(),
        svc,
        cfg,
        None,
        Box::new(ChaCha8Rng::seed_from_u64(seed + 2)),
    )
    .unwrap();
    let (events, _) = collect(session);
    events
        .into_iter()
        .filter_map(|e| match e {
            RuntimeEvent::Warning(
                w @ (RuntimeWarning::FarEndSilent | RuntimeWarning::FarEndHeard),
            ) => Some(w),
            _ => None,
        })
        .collect()
}

#[test]
fn live_far_end_warning_follows_what_each_track_hears() {
    let talk = words("a", 1000, 15);
    // The 2026-11-06 meetings: the mic speaks, the reference is all zeros.
    assert_eq!(
        far_end_warnings(&talk, Some(&[]), 8000, 40),
        [RuntimeWarning::FarEndSilent]
    );
    // The far end starts after the warning.
    let late = words("r", 6500, 5);
    assert_eq!(
        far_end_warnings(&talk, Some(&late), 10_000, 50),
        [RuntimeWarning::FarEndSilent, RuntimeWarning::FarEndHeard]
    );
    // Negative controls: the far end heard first; too little mic speech; no reference track
    // (a meeting in the room).
    let early = words("r", 200, 4);
    let after = words("a", 2500, 15);
    assert_eq!(far_end_warnings(&after, Some(&early), 9000, 60), []);
    assert_eq!(
        far_end_warnings(&words("a", 1000, 4), Some(&[]), 8000, 70),
        []
    );
    assert_eq!(far_end_warnings(&talk, None, 8000, 80), []);
}

/// The recording-gap warnings during a session with `gaps` (track, start ms, length ms) over
/// 12 s, and the total in its outcome. With `reference`, a second track runs alongside the mic.
fn recording_gaps(
    reference: bool,
    gaps: &[(TrackId, u64, u64)],
    seed: u64,
) -> (Vec<Duration>, Option<Duration>) {
    recording_gaps_with(reference, gaps, &[], seed)
}

/// [`recording_gaps`] with `markers` (track, frame at 16 kHz, kind) as well.
fn recording_gaps_with(
    reference: bool,
    gaps: &[(TrackId, u64, u64)],
    markers: &[(TrackId, u64, DiscontinuityKind)],
    seed: u64,
) -> (Vec<Duration>, Option<Duration>) {
    let dir = TempDir::new();
    let svc = services(&dir);
    let total_ms = 12_000;
    let talk = words("a", 500, 20);
    let mut builder =
        ReplayBuilder::new(ORIGIN, MonoBuffer::new(R16, speech(&talk, total_ms, seed)));
    if reference {
        builder = builder.reference(MonoBuffer::new(R16, speech(&talk, total_ms, seed + 1)));
    }
    for &(track, start, len) in gaps {
        builder = builder.gap(track, start * 16, len * 16).unwrap();
    }
    for (track, at, kind) in markers {
        builder = builder.discontinuity(*track, *at, kind.clone()).unwrap();
    }
    let session = start_live(
        builder.build().unwrap(),
        svc,
        config(earshot_core::model::ClockDomain::Replay),
        None,
        Box::new(ChaCha8Rng::seed_from_u64(seed + 2)),
    )
    .unwrap();
    let (events, outcome) = collect(session);
    let during = events
        .iter()
        .filter_map(|e| match e {
            RuntimeEvent::Warning(RuntimeWarning::RecordingGap { total }) => Some(*total),
            _ => None,
        })
        .collect();
    let at_end = outcome.problems.iter().find_map(|p| match p {
        RuntimeWarning::RecordingGap { total } => Some(*total),
        _ => None,
    });
    (during, at_end)
}

fn assert_about(got: Duration, want_ms: u64) {
    let got = got.as_millis() as u64;
    assert!(
        got.abs_diff(want_ms) <= 50,
        "{got} ms, wanted about {want_ms} ms"
    );
}

#[test]
fn live_recording_gap_on_the_mic_is_reported_during_and_after_the_meeting() {
    let (during, at_end) = recording_gaps(false, &[(TrackId::Mic, 4000, 3000)], 90);
    assert_about(*during.last().expect("a warning during the meeting"), 3000);
    assert_about(at_end.expect("in the outcome"), 3000);
}

#[test]
fn live_recording_gap_during_sleep_counts_once_across_both_tracks() {
    // A computer asleep stops both tracks together.
    let (during, at_end) = recording_gaps(
        true,
        &[(TrackId::Mic, 4000, 3000), (TrackId::Reference, 4000, 3000)],
        100,
    );
    assert_about(*during.last().expect("a warning"), 3000);
    assert_about(at_end.expect("in the outcome"), 3000);
}

/// The markers a capture re-open leaves on the mic (T9.21): no device from `lost_ms`, and
/// `back` from `back_ms` (`None`: the track ended and never came back).
fn mic_reopen_markers(
    lost_ms: u64,
    back_ms: Option<u64>,
) -> Vec<(TrackId, u64, DiscontinuityKind)> {
    let changed = |name: Option<&str>| DiscontinuityKind::DeviceChanged {
        device_name: name.map(str::to_owned),
        rate: R16,
        warning: None,
    };
    // Just before the gap's first frame, so the marker is not moved past the gap.
    let mut markers = vec![(TrackId::Mic, lost_ms * 16 - 1, changed(None))];
    if let Some(back) = back_ms {
        markers.push((TrackId::Mic, back * 16, changed(Some("Microphone"))));
    }
    markers
}

#[test]
fn live_recording_gap_after_a_stream_error_reopen_is_reported() {
    // The markers a capture re-open leaves (T9.21), three seconds apart: the silence filled
    // between is a gap like any other.
    let (during, at_end) = recording_gaps_with(
        true,
        &[(TrackId::Mic, 4000, 3000)],
        &mic_reopen_markers(4000, Some(7000)),
        160,
    );
    assert_about(*during.last().expect("a warning during the meeting"), 3000);
    assert_about(at_end.expect("in the outcome"), 3000);
}

#[test]
fn live_recording_gap_grows_when_the_mic_track_ends_and_the_reference_records_on() {
    // The markers of a mic track that ended (T9.21) while the far end went on: the rest of
    // the meeting is a gap on the mic.
    let (during, at_end) = recording_gaps_with(
        true,
        &[(TrackId::Mic, 6000, 6000)],
        &mic_reopen_markers(6000, None),
        170,
    );
    assert!(!during.is_empty(), "a warning during the meeting");
    assert_about(at_end.expect("in the outcome"), 6000);
}

#[test]
fn live_recording_gaps_add_up() {
    let (_, at_end) = recording_gaps(
        false,
        &[(TrackId::Mic, 2000, 2500), (TrackId::Mic, 7000, 3000)],
        110,
    );
    assert_about(at_end.expect("in the outcome"), 5500);
}

#[test]
fn live_recording_gap_is_not_reported_when_nothing_was_lost_from_the_mic() {
    // No gap at all; a gap too short to report; a reference that goes quiet, as any loopback
    // does while nothing plays.
    assert_eq!(recording_gaps(false, &[], 120), (vec![], None));
    assert_eq!(
        recording_gaps(false, &[(TrackId::Mic, 4000, 1000)], 130),
        (vec![], None)
    );
    assert_eq!(
        recording_gaps(true, &[(TrackId::Reference, 4000, 3000)], 140),
        (vec![], None)
    );
    assert_eq!(recording_gaps(true, &[], 150), (vec![], None));
}

/// A two-track meeting of `total_ms` with `faults` injected into its writers: its events, its
/// outcome and each track's FLAC as stored (`None`: no audio row).
struct Faulted {
    events: Vec<RuntimeEvent>,
    outcome: LiveOutcome,
    flacs: [Option<Vec<f32>>; 2],
    svc: Services,
    _dir: TempDir,
}

fn faulted_meeting(
    total_ms: u64,
    faults: &[(TrackId, earshot_store::WriteFault)],
    seed: u64,
) -> Faulted {
    let dir = TempDir::new();
    let svc = services(&dir);
    for &(track, fault) in faults {
        super::write_faults::set(&svc.data, track, fault);
    }
    let talk = words("a", 500, (total_ms / 400) as usize - 3);
    let far = words("r", 700, (total_ms / 400) as usize - 3);
    let source = ReplayBuilder::new(ORIGIN, MonoBuffer::new(R16, speech(&talk, total_ms, seed)))
        .reference(MonoBuffer::new(R16, speech(&far, total_ms, seed + 1)))
        .build()
        .unwrap();
    let session = start_live(
        source,
        svc.clone(),
        config(earshot_core::model::ClockDomain::Replay),
        None,
        Box::new(ChaCha8Rng::seed_from_u64(seed + 2)),
    )
    .unwrap();
    let (events, outcome) = collect(session);
    let mut flacs = [None, None];
    {
        let store = svc.store();
        for f in store.audio_files(outcome.meeting).unwrap() {
            let track = store
                .tracks(outcome.meeting)
                .unwrap()
                .into_iter()
                .find(|t| t.id == f.track_id)
                .unwrap();
            let (rate, samples) = read_flac(&svc.data.resolve(&f.path).unwrap()).unwrap();
            assert_eq!(rate, 16_000);
            assert_eq!(f.frames, samples.len() as u64, "the row matches the file");
            flacs[slot(track.kind)] = Some(samples);
        }
    }
    Faulted {
        events,
        outcome,
        flacs,
        svc,
        _dir: dir,
    }
}

fn stopped_warnings(events: &[RuntimeEvent]) -> Vec<RuntimeWarning> {
    warnings(events)
        .into_iter()
        .filter(|w| {
            matches!(
                w,
                RuntimeWarning::RecordingStopped { .. }
                    | RuntimeWarning::EveryRecordingStopped { .. }
                    | RuntimeWarning::Recording { .. }
            )
        })
        .collect()
}

#[test]
fn live_recording_write_failure_keeps_the_audio_before_it_and_the_other_track() {
    // T9.17: a full disk part way through the mic's recording. The same meeting without the
    // failure is the control: the replay is deterministic, so the cut file must be exactly the
    // start of the control's.
    let whole = faulted_meeting(12_000, &[], 200);
    assert!(stopped_warnings(&whole.events).is_empty());
    let full = earshot_store::WriteFault::DiskFullAfter(40_000);
    let cut = faulted_meeting(12_000, &[(TrackId::Mic, full)], 200);

    let control_mic = whole.flacs[0].clone().expect("control mic");
    let control_ref = whole.flacs[1].clone().expect("control reference");
    let mic = cut.flacs[0]
        .clone()
        .expect("the cut mic is a readable file with its row");
    assert!(
        !mic.is_empty() && mic.len() < control_mic.len(),
        "{} of {}",
        mic.len(),
        control_mic.len()
    );
    assert_eq!(
        mic,
        control_mic[..mic.len()],
        "the audio before the error, bit for bit"
    );
    assert_eq!(
        cut.flacs[1].as_deref(),
        Some(&control_ref[..]),
        "the reference goes on"
    );

    // Said once during the meeting, with where it stopped and why, and again in the outcome.
    let at = MeetingTime::from_pipeline_frames(mic.len() as u64);
    let said = stopped_warnings(&cut.events);
    assert!(
        matches!(
            said.as_slice(),
            [RuntimeWarning::RecordingStopped {
                track: TrackId::Mic,
                at: a,
                disk_full: true,
                ..
            }] if *a == at
        ),
        "{said:?}"
    );
    assert!(
        cut.outcome.problems.contains(&said[0]),
        "{:?}",
        cut.outcome.problems
    );
    assert!(
        !cut.outcome
            .problems
            .iter()
            .any(|p| matches!(p, RuntimeWarning::EveryRecordingStopped { .. })),
        "one track still records"
    );
    // The meeting ran to its end and keeps both files.
    assert_eq!(cut.outcome.reason, EndReason::Finished);
    assert_eq!(cut.outcome.end, whole.outcome.end);
    assert_eq!(cut.outcome.audio_files.len(), 2);
    // Nothing is left half written.
    let dir = cut
        .svc
        .data
        .root()
        .join("audio")
        .join(cut.outcome.meeting.to_string());
    let parts = std::fs::read_dir(dir)
        .unwrap()
        .filter(|e| {
            e.as_ref()
                .unwrap()
                .path()
                .extension()
                .is_some_and(|x| x == "part")
        })
        .count();
    assert_eq!(parts, 0);
}

#[test]
fn live_recording_every_track_failing_stops_the_meeting_itself() {
    let full = earshot_store::WriteFault::DiskFullAfter(20_000);
    let cut = faulted_meeting(
        60_000,
        &[(TrackId::Mic, full), (TrackId::Reference, full)],
        210,
    );
    let said = stopped_warnings(&cut.events);
    let stopped: Vec<TrackId> = said
        .iter()
        .filter_map(|w| match w {
            RuntimeWarning::RecordingStopped {
                track,
                disk_full: true,
                ..
            } => Some(*track),
            _ => None,
        })
        .collect();
    assert_eq!(stopped.len(), 2, "{said:?}");
    assert!(stopped.contains(&TrackId::Mic) && stopped.contains(&TrackId::Reference));
    // Once, after both tracks said so.
    assert_eq!(
        said.last(),
        Some(&RuntimeWarning::EveryRecordingStopped { disk_full: true })
    );
    assert_eq!(said.len(), 3, "{said:?}");
    assert!(
        cut.outcome
            .problems
            .contains(&RuntimeWarning::EveryRecordingStopped { disk_full: true })
    );
    // The session stopped it, long before the 60 s replay ran out.
    assert_eq!(cut.outcome.reason, EndReason::Stopped);
    assert!(
        cut.outcome.end < MeetingTime::from_millis(60_000),
        "{:?}",
        cut.outcome.end
    );
    // Both files keep their audio.
    for f in &cut.flacs {
        assert!(f.as_ref().is_some_and(|s| !s.is_empty()));
    }
    assert!(matches!(cut.events.last(), Some(RuntimeEvent::Ended(_))));
}

#[test]
fn live_recording_that_failed_before_any_frame_keeps_no_empty_file() {
    // Lane-merge review: a failure on the very first frame leaves nothing to keep, so no
    // AudioFile row and no `.part`, and nothing for the final pass; the meeting still ends.
    let full = earshot_store::WriteFault::DiskFullAfter(0);
    let cut = faulted_meeting(
        12_000,
        &[(TrackId::Mic, full), (TrackId::Reference, full)],
        270,
    );
    assert_eq!(cut.flacs, [None, None]);
    assert!(cut.outcome.audio_files.is_empty());
    assert!(
        cut.svc
            .store()
            .audio_files(cut.outcome.meeting)
            .unwrap()
            .is_empty()
    );
    let said = stopped_warnings(&cut.events);
    assert_eq!(said.len(), 3, "{said:?}");
    assert!(said[..2].iter().all(|w| matches!(
        w,
        RuntimeWarning::RecordingStopped { at, disk_full: true, .. } if *at == MeetingTime::from_millis(0)
    )));
    assert_eq!(
        said[2],
        RuntimeWarning::EveryRecordingStopped { disk_full: true }
    );
    assert_eq!(cut.outcome.reason, EndReason::Stopped);
    assert!(matches!(cut.events.last(), Some(RuntimeEvent::Ended(_))));
    for track in [TrackId::Mic, TrackId::Reference] {
        let path = cut
            .svc
            .data
            .resolve(&DataDir::audio_rel(cut.outcome.meeting, track))
            .unwrap();
        assert!(!path.exists() && !path.with_extension("flac.part").exists());
    }
    let (input, missing) =
        crate::post::load_post_input(&cut.svc, cut.outcome.meeting, &cut.outcome.final_pass)
            .unwrap();
    assert!(
        input.tracks.is_empty(),
        "no empty file reaches the final pass"
    );
    assert_eq!(missing.len(), 2);
}
#[test]
fn live_recording_that_cannot_be_finished_keeps_its_part_file() {
    // A drive that went away: the file cannot even be finished, so it stays as `.part` for
    // recovery (T9.18) and the track has no row; the meeting goes on with the other track.
    let broken = earshot_store::WriteFault::BrokenAfter(30_000);
    let cut = faulted_meeting(12_000, &[(TrackId::Mic, broken)], 220);
    assert!(cut.flacs[0].is_none());
    assert!(cut.flacs[1].is_some());
    let said = stopped_warnings(&cut.events);
    assert!(
        matches!(
            said.as_slice(),
            [
                RuntimeWarning::RecordingStopped {
                    track: TrackId::Mic,
                    disk_full: false,
                    ..
                },
                RuntimeWarning::Recording {
                    track: TrackId::Mic,
                    ..
                },
            ]
        ),
        "{said:?}"
    );
    let part = cut
        .svc
        .data
        .resolve(&DataDir::audio_rel(cut.outcome.meeting, TrackId::Mic))
        .unwrap()
        .with_extension("flac.part");
    assert!(part.exists(), "never deleted");
    assert_eq!(cut.outcome.reason, EndReason::Finished);
}

#[cfg(windows)]
#[test]
fn live_stop_saves_the_recording_when_the_capture_thread_never_ends() {
    // T9.20: a capture thread stuck in a call (its stop never returns). The source gives it up
    // after its stop wait; the session still ends, both files hold everything delivered before,
    // the user hears why once, and Ended is last.
    use earshot_capture_wasapi::{ChannelSource, LiveMsg};
    let dir = TempDir::new();
    let svc = services(&dir);
    let info = CaptureSession::new(ORIGIN, TrackInfo::new(R16), Some(TrackInfo::new(R16)));
    let (tx, rx) = std::sync::mpsc::channel::<LiveMsg<String>>();
    let packets = 300u64;
    for i in 0..packets {
        let at = ORIGIN.after_frames(i * 160, R16);
        for track in [TrackId::Mic, TrackId::Reference] {
            tx.send(LiveMsg::Event(CaptureEvent::Frames {
                track,
                device_ts: at,
                frames: noise(i + 50 * u64::from(track == TrackId::Mic), 160, 0.05),
            }))
            .unwrap();
        }
    }
    let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
    let release_rx = std::sync::Mutex::new(release_rx);
    let source = ChannelSource::new(info, rx, move || {
        let _ = release_rx.lock().unwrap().recv();
    })
    .with_stop_wait(Duration::from_millis(50));
    let session = start_live(
        source,
        svc.clone(),
        config(earshot_core::model::ClockDomain::Qpc),
        None,
        Box::new(ChaCha8Rng::seed_from_u64(230)),
    )
    .unwrap();
    session.request_stop();
    let (events, outcome) = collect(session);
    assert!(matches!(events.last(), Some(RuntimeEvent::Ended(_))));
    assert_eq!(outcome.reason, EndReason::Stopped);
    let said: Vec<RuntimeWarning> = warnings(&events)
        .into_iter()
        .filter(|w| matches!(w, RuntimeWarning::DeviceStoppedAnswering))
        .collect();
    assert_eq!(said, [RuntimeWarning::DeviceStoppedAnswering]);
    assert!(
        outcome
            .problems
            .contains(&RuntimeWarning::DeviceStoppedAnswering)
    );
    let saved = events
        .iter()
        .position(|e| matches!(e, RuntimeEvent::RecordingSaved { .. }))
        .expect("the recording is saved");
    assert!(saved < events.len() - 1);
    let store = svc.store();
    let files = store.audio_files(outcome.meeting).unwrap();
    assert_eq!(files.len(), 2);
    for f in files {
        let (_, samples) = read_flac(&svc.data.resolve(&f.path).unwrap()).unwrap();
        assert_eq!(
            samples.len() as u64,
            packets * 160,
            "everything delivered is kept"
        );
    }
    drop(store);
    // The stuck thread is left alone; let it go now the test is done.
    drop(tx);
    drop(release_tx);
}

/// How a catch-up test's transcriber behaves once its first decode starts.
#[derive(Clone, Copy)]
enum Decode {
    /// Stuck in a call that never returns (no poll) until the test releases it.
    Hangs,
    /// Slow (20 polls 100 ms apart per call) but polling its cancel token all along.
    SlowButPolling,
}

/// A transcriber over a timeline that behaves as `how` says.
struct CatchUpTranscriber {
    inner: crate::testutil::Timeline,
    how: Decode,
    release: Arc<std::sync::Mutex<std::sync::mpsc::Receiver<()>>>,
}

impl earshot_core::asr::Transcriber for CatchUpTranscriber {
    fn transcribe(
        &mut self,
        audio: &MonoBuffer,
        options: &earshot_core::asr::TranscribeOptions,
        cancel: &earshot_core::asr::CancelToken,
    ) -> Result<earshot_core::asr::Transcription, AsrError> {
        match self.how {
            Decode::Hangs => {
                let _ = self.release.lock().unwrap().recv();
            }
            Decode::SlowButPolling => {
                for _ in 0..20 {
                    if cancel.is_cancelled() {
                        return Err(AsrError::Cancelled);
                    }
                    std::thread::sleep(Duration::from_millis(100));
                }
            }
        }
        self.inner.transcribe(audio, options, cancel)
    }

    fn backend_info(&self) -> earshot_core::asr::BackendInfo {
        self.inner.backend_info()
    }
}

/// A stretch meeting of 6 s whose decodes behave as `how` says, stopped once it ends: its events
/// and outcome, and whether `cancel` was used once the recording was saved.
fn catch_up_meeting(
    how: Decode,
    stall: Duration,
    cancel: bool,
    seed: u64,
) -> (Vec<RuntimeEvent>, LiveOutcome, Services, TempDir) {
    let dir = TempDir::new();
    let svc = services(&dir);
    let w = words("a", 500, 12);
    let audio = speech(&w, 6000, seed);
    let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
    let release = Arc::new(std::sync::Mutex::new(release_rx));
    let mut slot = Some((audio.clone(), w));
    let factory: LiveAsrFactory = Box::new(move |t, entity| {
        let Some((audio, w)) = slot.take().filter(|_| t == TrackId::Mic) else {
            return Ok(None);
        };
        Ok(Some(
            LiveAsr {
                transcriber: Box::new(CatchUpTranscriber {
                    inner: crate::testutil::timeline(audio, w),
                    how,
                    release: Arc::clone(&release),
                }),
                vad: Box::new(EnergyVad::default()),
                config: ChunkerConfig::default(),
                track_entity: entity,
                segment_ids: ids(),
            }
            .into(),
        ))
    });
    let source = ReplayBuilder::new(ORIGIN, MonoBuffer::new(R16, audio))
        .build()
        .unwrap();
    let mut session = start_live(
        source,
        svc.clone(),
        LiveConfig {
            catch_up_stall: stall,
            ..stretch_config(earshot_core::model::ClockDomain::Replay)
        },
        Some(factory),
        Box::new(ChaCha8Rng::seed_from_u64(seed + 1)),
    )
    .unwrap();
    let rx = session.take_events().unwrap();
    let mut events = Vec::new();
    loop {
        let e = rx
            .recv_timeout(Duration::from_secs(120))
            .expect("the session ends");
        let saved = matches!(e, RuntimeEvent::RecordingSaved { .. });
        let end = matches!(e, RuntimeEvent::Ended(_));
        events.push(e);
        if saved && cancel {
            session.cancel_catch_up();
        }
        if end {
            break;
        }
    }
    let outcome = session.wait().unwrap();
    // A hung decode is left behind; let it go now.
    drop(release_tx);
    (events, outcome, svc, dir)
}

#[test]
fn live_catch_up_cancel_ends_the_session_without_waiting_for_a_hung_decode() {
    // T9.22: Cancel on "Finishing the meeting" reaches the live catch-up. No stall bound could
    // fire here (one hour); only the cancel ends it.
    let (events, outcome, svc, _dir) =
        catch_up_meeting(Decode::Hangs, Duration::from_secs(3600), true, 240);
    assert!(matches!(events.last(), Some(RuntimeEvent::Ended(_))));
    // Nothing to tell: the user asked for it.
    assert!(
        !warnings(&events)
            .iter()
            .any(|w| matches!(w, RuntimeWarning::LiveCatchUpStalled { .. }))
    );
    // The recording is kept whole.
    let files = svc.store().audio_files(outcome.meeting).unwrap();
    assert_eq!(files.len(), 1);
    assert_eq!(files[0].frames, 6 * 16_000);
    assert!(matches!(
        crate::stretches::stretch_record(&svc.store(), outcome.meeting, TrackId::Mic).unwrap(),
        Some(crate::stretches::StoredStretches::Record(r)) if r.stopped.is_some()
    ));
}

#[test]
fn live_catch_up_that_stops_moving_is_given_up_and_said() {
    let (events, outcome, _svc, _dir) =
        catch_up_meeting(Decode::Hangs, Duration::from_millis(300), false, 250);
    let said: Vec<RuntimeWarning> = warnings(&events)
        .into_iter()
        .filter(|w| matches!(w, RuntimeWarning::LiveCatchUpStalled { .. }))
        .collect();
    assert_eq!(
        said,
        [RuntimeWarning::LiveCatchUpStalled {
            track: TrackId::Mic
        }]
    );
    assert!(outcome.problems.contains(&said[0]));
    assert!(matches!(events.last(), Some(RuntimeEvent::Ended(_))));
}

#[test]
fn live_catch_up_that_is_slow_but_polling_is_never_cut() {
    // Negative control: each decode takes 2 s, longer than the 1 s bound, but polls every 100 ms,
    // so the catch-up runs to its end and every word lands.
    let (events, outcome, svc, _dir) =
        catch_up_meeting(Decode::SlowButPolling, Duration::from_secs(1), false, 260);
    assert!(
        !warnings(&events)
            .iter()
            .any(|w| matches!(w, RuntimeWarning::LiveCatchUpStalled { .. }))
    );
    let rows = live_segments(&**svc.store(), outcome.meeting, TranscriptPass::Live).unwrap();
    let stored: Vec<String> = rows.iter().flat_map(|r| texts(&r.segment.words)).collect();
    assert_eq!(stored, texts(&words("a", 500, 12)));
}

#[test]
fn live_voice_activity_stays_off_on_silence_and_ends_with_the_session() {
    // Negative control: quiet noise only, no "speaking" ever.
    let dir = TempDir::new();
    let svc = services(&dir);
    let quiet = speech(&[], 6000, 34);
    let source = ReplayBuilder::new(ORIGIN, MonoBuffer::new(R16, quiet))
        .build()
        .unwrap();
    let session = start_live(
        source,
        svc.clone(),
        config(earshot_core::model::ClockDomain::Replay),
        None,
        Box::new(ChaCha8Rng::seed_from_u64(35)),
    )
    .unwrap();
    let (events, _) = collect(session);
    assert!(
        voice_changes(&events, TrackId::Mic).is_empty(),
        "{events:?}"
    );

    // Speech up to the very end: switched off at the end, before Ended.
    let talk = words("t", 2000, 10);
    let source = ReplayBuilder::new(ORIGIN, MonoBuffer::new(R16, speech(&talk, 6000, 36)))
        .build()
        .unwrap();
    let session = start_live(
        source,
        svc,
        config(earshot_core::model::ClockDomain::Replay),
        None,
        Box::new(ChaCha8Rng::seed_from_u64(37)),
    )
    .unwrap();
    let (events, outcome) = collect(session);
    assert_switches(
        &voice_changes(&events, TrackId::Mic),
        &[(true, 2000), (false, outcome.end.as_millis())],
        60,
    );
    let n = events.len();
    assert!(
        matches!(
            &events[n - 2],
            RuntimeEvent::VoiceActivity {
                speaking: false,
                ..
            }
        ),
        "{:?}",
        &events[n - 2..]
    );
}

#[test]
fn live_transcript_lag_is_reported_for_each_transcribed_track_with_the_stats() {
    let dir = TempDir::new();
    let svc = services(&dir);
    let w = words("a", 1000, 5);
    let audio = speech(&w, 8000, 38);
    let source = ReplayBuilder::new(ORIGIN, MonoBuffer::new(R16, audio.clone()))
        .reference(MonoBuffer::new(R16, speech(&[], 8000, 39)))
        .build()
        .unwrap();
    let session = start_live(
        source,
        svc,
        stretch_config(earshot_core::model::ClockDomain::Replay),
        Some(fake_asr(TrackId::Mic, audio, w)),
        Box::new(ChaCha8Rng::seed_from_u64(40)),
    )
    .unwrap();
    let (events, outcome) = collect(session);
    let lags: Vec<(TrackId, Duration)> = events
        .iter()
        .filter_map(|e| match e {
            RuntimeEvent::TranscriptLag { track, behind } => Some((*track, *behind)),
            _ => None,
        })
        .collect();
    let stats = events
        .iter()
        .filter(|e| matches!(e, RuntimeEvent::Stats(_)))
        .count();
    assert!(!lags.is_empty(), "{events:?}");
    // Only the transcribed track, never more often than the stats, never beyond the audio.
    assert!(lags.iter().all(|(t, _)| *t == TrackId::Mic), "{lags:?}");
    assert!(lags.len() <= stats, "{} lags, {stats} stats", lags.len());
    let total = Duration::from_millis(outcome.end.as_millis());
    assert!(lags.iter().all(|(_, b)| *b <= total), "{lags:?}");
    // The first lag follows the first Stats event.
    let first_stats = events
        .iter()
        .position(|e| matches!(e, RuntimeEvent::Stats(_)))
        .unwrap();
    let first_lag = events
        .iter()
        .position(|e| matches!(e, RuntimeEvent::TranscriptLag { .. }))
        .unwrap();
    assert!(first_stats < first_lag);
}

#[test]
fn live_session_meeting_reads_as_recording_until_it_ends() {
    // T7.2a: the meeting activity of a running session, then of the finished recording
    // (no final pass yet).
    let dir = TempDir::new();
    let svc = services(&dir);
    let (session, _events) = running_session(&svc, 41);
    let meeting = session.meeting();
    assert_eq!(
        svc.meeting_activity(meeting).unwrap(),
        Some(crate::MeetingActivity::Recording)
    );
    session.stop().unwrap();
    assert_eq!(
        svc.meeting_activity(meeting).unwrap(),
        Some(crate::MeetingActivity::Unprocessed)
    );
}

#[path = "summary_tests.rs"]
mod summary_tests;

#[path = "queue_tests.rs"]
mod queue_tests;

#[path = "preview_tests.rs"]
mod preview_tests;

#[test]
fn live_call_output_refused_is_said_once_and_leaves_the_device_state_alone() {
    let dir = TempDir::new();
    let svc = services(&dir);
    let session_info = CaptureSession::new(ORIGIN, TrackInfo::new(R16), Some(TrackInfo::new(R16)));
    let at = |packet: u64| ORIGIN.after_frames(packet * 160, R16);
    let note = DiscontinuityKind::Note {
        warning: EndpointWarning::CallOutputRefused {
            wanted: "Headset".into(),
            used: Some("Speakers".into()),
        },
    };
    let mut script = Vec::new();
    for i in 0..60 {
        if i == 30 {
            script.push(marker(TrackId::Reference, at(i), note.clone()));
        }
        script.push(frames(TrackId::Mic, noise(i, 160, 0.01), at(i)));
        script.push(frames(
            TrackId::Reference,
            noise(i + 1000, 160, 0.01),
            at(i),
        ));
    }
    script.push(Ok(CaptureEvent::End(EndOfStream {
        at: at(60),
        reason: EndReason::Finished,
    })));
    let session = start_live(
        ScriptSource::new(session_info, script),
        svc,
        config(earshot_core::model::ClockDomain::Qpc),
        None,
        Box::new(ChaCha8Rng::seed_from_u64(31)),
    )
    .unwrap();
    let (events, _) = collect(session);
    let warnings: Vec<&RuntimeWarning> = events
        .iter()
        .filter_map(|e| match e {
            RuntimeEvent::Warning(w) => Some(w),
            _ => None,
        })
        .collect();
    // No DeviceRestored or DeviceUnavailable: nothing happened to the device.
    assert_eq!(
        warnings,
        [&RuntimeWarning::CallOutputRefused {
            wanted: "Headset".into(),
            used: Some("Speakers".into()),
        }],
        "{events:?}"
    );
}

/// A source that panics on its `panic_at`th pump (a bug on the session thread, T9.19).
struct PanicSource<S> {
    inner: S,
    pumps: u64,
    panic_at: u64,
}

impl<S: AudioSource> AudioSource for PanicSource<S> {
    type Error = S::Error;

    fn session(&self) -> &CaptureSession {
        self.inner.session()
    }

    fn pump(&mut self, sink: &mut dyn CaptureSink) -> Result<SourceStatus, Self::Error> {
        self.pumps += 1;
        assert!(
            self.pumps != self.panic_at,
            "an invented bug on the session thread"
        );
        self.inner.pump(sink)
    }

    fn stop(&mut self) {
        self.inner.stop();
    }
}

#[test]
fn live_recording_a_panic_on_the_session_thread_saves_the_recording_and_ends_once() {
    // T9.19: the same replay with and without the panic. With it, every track's file is
    // finished as far as it got (a readable FLAC with its row, the start of the control's audio),
    // the meeting has its duration, and RecordingSaved then Ended (reason Failed) come exactly
    // once, Ended last; `wait` reports the panic.
    let control = faulted_meeting(12_000, &[], 300);
    let dir = TempDir::new();
    let svc = services(&dir);
    let talk = words("a", 500, 27);
    let far = words("r", 700, 27);
    let replay = ReplayBuilder::new(ORIGIN, MonoBuffer::new(R16, speech(&talk, 12_000, 300)))
        .reference(MonoBuffer::new(R16, speech(&far, 12_000, 301)))
        .build()
        .unwrap();
    let mut session = start_live(
        PanicSource {
            inner: replay,
            pumps: 0,
            panic_at: 200,
        },
        svc.clone(),
        config(earshot_core::model::ClockDomain::Replay),
        None,
        Box::new(ChaCha8Rng::seed_from_u64(302)),
    )
    .unwrap();
    let meeting = session.meeting();
    let events_rx = session.take_events().unwrap();
    let result = session.wait();
    assert!(matches!(result, Err(RuntimeError::Poisoned)), "{result:?}");
    let events: Vec<RuntimeEvent> = events_rx.try_iter().collect();
    let saved = events
        .iter()
        .filter(|e| matches!(e, RuntimeEvent::RecordingSaved { .. }))
        .count();
    let ended: Vec<&LiveOutcome> = events
        .iter()
        .filter_map(|e| match e {
            RuntimeEvent::Ended(o) => Some(o),
            _ => None,
        })
        .collect();
    assert_eq!((saved, ended.len()), (1, 1), "{events:?}");
    assert!(matches!(events.last(), Some(RuntimeEvent::Ended(_))));
    assert!(matches!(
        events
            .iter()
            .find(|e| matches!(e, RuntimeEvent::RecordingSaved { .. })),
        Some(RuntimeEvent::RecordingSaved {
            reason: EndReason::Failed,
            ..
        })
    ));
    assert_eq!(ended[0].reason, EndReason::Failed);
    let store = svc.store();
    let rows = store.audio_files(meeting).unwrap();
    assert_eq!(rows.len(), 2, "both tracks finished");
    let tracks = store.tracks(meeting).unwrap();
    for f in &rows {
        let kind = tracks.iter().find(|t| t.id == f.track_id).unwrap().kind;
        let (_, samples) = read_flac(&svc.data.resolve(&f.path).unwrap()).unwrap();
        let whole = control.flacs[slot(kind)].clone().unwrap();
        assert!(
            !samples.is_empty() && samples.len() < whole.len(),
            "{kind:?}"
        );
        assert_eq!(
            samples,
            whole[..samples.len()],
            "{kind:?}: the audio before the panic"
        );
    }
    let m = store.meeting(meeting).unwrap().unwrap();
    assert_eq!(m.duration_ms, Some(ended[0].end.as_millis()));
    assert!(m.duration_ms.unwrap() > 0);
    drop(store);
    // Nothing left for start-up recovery.
    assert!(
        !std::fs::read_dir(svc.data.root().join("audio").join(meeting.to_string()))
            .unwrap()
            .any(|e| e.unwrap().file_name().to_string_lossy().ends_with(".part"))
    );
}

#[test]
fn live_recording_a_second_panic_while_saving_after_a_panic_still_ends_once() {
    // T9.18 review: the session thread panics mid-recording, then the loopback's finish panics
    // again inside the save that follows. The mic is still saved, the meeting gets its
    // duration, the loopback is said to be saved at the next start, and RecordingSaved then
    // Ended (Failed) still come exactly once each, Ended last.
    let dir = TempDir::new();
    let svc = services(&dir);
    super::finalize_panics::set(&svc.data, TrackId::Reference);
    let talk = words("a", 500, 27);
    let far = words("r", 700, 27);
    let replay = ReplayBuilder::new(ORIGIN, MonoBuffer::new(R16, speech(&talk, 12_000, 320)))
        .reference(MonoBuffer::new(R16, speech(&far, 12_000, 321)))
        .build()
        .unwrap();
    let mut session = start_live(
        PanicSource {
            inner: replay,
            pumps: 0,
            panic_at: 200,
        },
        svc.clone(),
        config(earshot_core::model::ClockDomain::Replay),
        None,
        Box::new(ChaCha8Rng::seed_from_u64(322)),
    )
    .unwrap();
    let meeting = session.meeting();
    let events_rx = session.take_events().unwrap();
    let result = session.wait();
    assert!(matches!(result, Err(RuntimeError::Poisoned)), "{result:?}");
    let events: Vec<RuntimeEvent> = events_rx.try_iter().collect();
    let saved: Vec<&Vec<RuntimeWarning>> = events
        .iter()
        .filter_map(|e| match e {
            RuntimeEvent::RecordingSaved { problems, .. } => Some(problems),
            _ => None,
        })
        .collect();
    let ended = events
        .iter()
        .filter(|e| matches!(e, RuntimeEvent::Ended(_)))
        .count();
    assert_eq!((saved.len(), ended), (1, 1), "{events:?}");
    assert!(matches!(events.last(), Some(RuntimeEvent::Ended(_))));
    let told = saved[0]
        .iter()
        .filter(|p| {
            matches!(
                p,
                RuntimeWarning::Recording {
                    track: TrackId::Reference,
                    ..
                }
            )
        })
        .count();
    assert_eq!(told, 1, "{:?}", saved[0]);
    let duration = svc.store().meeting(meeting).unwrap().unwrap().duration_ms;
    assert!(duration.is_some_and(|d| d > 0), "{duration:?}");
    assert_eq!(
        svc.store().audio_files(meeting).unwrap().len(),
        1,
        "the mic"
    );
    let report = crate::recover::recover_interrupted(&svc);
    assert_eq!(report.leftovers_finished, [meeting], "{report:?}");
    assert_eq!(svc.store().audio_files(meeting).unwrap().len(), 2);
}

#[test]
fn live_recording_a_panic_in_a_tracks_finish_is_recovered_at_the_next_start() {
    // T9.19 with T9.18: the session thread panics inside the loopback's finish, after the
    // recording ended and before RecordingSaved. The session still saves the mic, sets the
    // duration and says the loopback is saved at the next start; that start's recovery
    // finishes the loopback's file and writes its row.
    let dir = TempDir::new();
    let svc = services(&dir);
    super::finalize_panics::set(&svc.data, TrackId::Reference);
    let talk = words("a", 500, 27);
    let far = words("r", 700, 27);
    let replay = ReplayBuilder::new(ORIGIN, MonoBuffer::new(R16, speech(&talk, 12_000, 310)))
        .reference(MonoBuffer::new(R16, speech(&far, 12_000, 311)))
        .build()
        .unwrap();
    let mut session = start_live(
        replay,
        svc.clone(),
        config(earshot_core::model::ClockDomain::Replay),
        None,
        Box::new(ChaCha8Rng::seed_from_u64(312)),
    )
    .unwrap();
    let meeting = session.meeting();
    let events_rx = session.take_events().unwrap();
    let result = session.wait();
    assert!(matches!(result, Err(RuntimeError::Poisoned)), "{result:?}");
    let events: Vec<RuntimeEvent> = events_rx.try_iter().collect();
    let told = events.iter().any(|e| {
        matches!(e, RuntimeEvent::RecordingSaved { problems, .. }
            if problems.iter().any(|p| matches!(p,
                RuntimeWarning::Recording { track: TrackId::Reference, .. })))
    });
    assert!(told, "{events:?}");
    let duration = svc.store().meeting(meeting).unwrap().unwrap().duration_ms;
    assert!(duration.is_some_and(|d| d > 0), "{duration:?}");
    let rows = svc.store().audio_files(meeting).unwrap();
    assert_eq!(rows.len(), 1, "only the mic was finished");

    let report = crate::recover::recover_interrupted(&svc);
    assert_eq!(report.leftovers_finished, [meeting], "{report:?}");
    let store = svc.store();
    let rows = store.audio_files(meeting).unwrap();
    assert_eq!(rows.len(), 2, "the loopback has its row");
    let tracks = store.tracks(meeting).unwrap();
    for f in &rows {
        let (_, samples) = read_flac(&svc.data.resolve(&f.path).unwrap()).unwrap();
        assert!(!samples.is_empty());
        assert_eq!(f.frames, samples.len() as u64);
        assert!(tracks.iter().any(|t| t.id == f.track_id));
    }
    assert_eq!(
        store.meeting(meeting).unwrap().unwrap().duration_ms,
        duration
    );
    drop(store);
    assert!(
        !std::fs::read_dir(svc.data.root().join("audio").join(meeting.to_string()))
            .unwrap()
            .any(|e| e.unwrap().file_name().to_string_lossy().ends_with(".part"))
    );
}
