//! Tests of the final pass reusing the live speech stretches (T6.13c). Every name contains
//! `post`. Each meeting is recorded by a real live session (replay source, stretch
//! workers, FLAC), then post-processed twice from the same input: once with the stored
//! stretch records (the reuse) and once without them (the whole-meeting pass, D20's
//! fallback). Both must store the same final transcript.

use std::sync::atomic::Ordering::SeqCst;
use std::sync::atomic::{AtomicBool, AtomicUsize};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use earshot_core::asr::{
    AsrError, BackendInfo, BackendKind, CancelToken, ChunkerConfig, MeetingTime, PieceConfig,
    RawSegment, TranscribeOptions, Transcriber, Transcription, Vad, Word,
};
use earshot_core::audio::{MonoBuffer, PIPELINE_RATE, Sample};
use earshot_core::capture::{
    AudioSource, CaptureSession, CaptureSink, DeviceTime, ReplayBuilder, SourceStatus, TrackId,
};
use earshot_core::diar::{EmbedderInfo, EmbeddingModelId, SegmenterInfo, SpeakerRole};
use earshot_core::identity::EntityId;
use earshot_core::model::{ClockDomain, MeetingRepo, TranscriptPass, live_segments};
use earshot_core::pipeline::{LiveAsr, PipelineConfig};
use rand_chacha::ChaCha8Rng;
use rand_core::SeedableRng;

use super::tests::{CAMPPLUS_ID, LevelEmbedder, LevelSegmenter, TempDir, services};
use super::*;
use crate::Services;
use crate::live::{LiveAsrFactory, LiveConfig, LiveOutcome, LiveTranscription, start_live};
use crate::stretches::{StoredStretches, StretchRecord, stretch_record};

const ORIGIN: DeviceTime = DeviceTime::from_ticks(9_000_000_000);
const LEVEL_MODEL: &str = "level@1";

/// The piece settings of these tests: the defaults, with a repetition guard that the
/// synthetic word lists below never trip and a looping piece always does.
fn pieces() -> PieceConfig {
    PieceConfig {
        compression_ratio_threshold: 10.0,
        ..PieceConfig::default()
    }
}

/// 10 ms frames: speech when the frame's RMS is above 0.02 (word noise is about 0.23,
/// the background 0.002; 16-bit FLAC changes neither side).
struct RmsVad;

impl Vad for RmsVad {
    fn id(&self) -> String {
        "rms-vad".into()
    }
    fn frame_len(&self) -> usize {
        160
    }
    fn speech_probability(&mut self, frame: &[Sample]) -> Result<f32, AsrError> {
        let rms = (frame.iter().map(|x| x * x).sum::<f32>() / frame.len() as f32).sqrt();
        Ok(if rms > 0.02 { 1.0 } else { 0.0 })
    }
    fn reset(&mut self) {}
}

/// Finds a buffer in one of its timelines within 1e-4 (so it also finds a 16-bit FLAC
/// copy) and answers with the words whose midpoint lies inside, one segment per word, as
/// `FakeTranscriber::timeline` does. A word `LOOP` comes back as a looping segment (a
/// whisper repetition loop). Calls are recorded as (timeline, offset, length).
struct Oracle {
    timelines: Vec<(Vec<f32>, Vec<Word>)>,
    calls: Arc<Mutex<Vec<(usize, usize, usize)>>>,
    model: String,
    /// The reported backend kind and device (the model is `model`).
    backend: (BackendKind, String),
    /// Fail (or panic) on this call (1-based).
    fail_on: Option<(usize, bool)>,
}

impl Oracle {
    fn new(timelines: Vec<(Vec<f32>, Vec<Word>)>) -> Self {
        Self {
            timelines,
            calls: Arc::default(),
            model: "fake".into(),
            backend: (BackendKind::Fake, "fake".into()),
            fail_on: None,
        }
    }

    /// The same, reporting `kind` on `device` (the live worker's backend in a test).
    fn on(mut self, kind: BackendKind, device: &str) -> Self {
        self.backend = (kind, device.into());
        self
    }

    fn calls(&self) -> Vec<(usize, usize, usize)> {
        self.calls.lock().unwrap().clone()
    }
}

impl Transcriber for Oracle {
    fn transcribe(
        &mut self,
        audio: &MonoBuffer,
        _options: &TranscribeOptions,
        cancel: &CancelToken,
    ) -> Result<Transcription, AsrError> {
        if cancel.is_cancelled() {
            return Err(AsrError::Cancelled);
        }
        let n = self.calls.lock().unwrap().len() + 1;
        let (k, offset) = self
            .timelines
            .iter()
            .enumerate()
            .find_map(|(k, (a, _))| {
                crate::testutil::locate_close(a, &audio.samples).map(|o| (k, o))
            })
            .ok_or_else(|| AsrError::Backend("buffer not found in any timeline".into()))?;
        self.calls
            .lock()
            .unwrap()
            .push((k, offset, audio.samples.len()));
        match self.fail_on {
            Some((call, true)) if call == n => panic!("simulated transcriber crash"),
            Some((call, false)) if call == n => {
                return Err(AsrError::Backend("gpu lost".into()));
            }
            _ => {}
        }
        let start = MeetingTime::from_pipeline_frames(offset as u64);
        let end = MeetingTime::from_pipeline_frames((offset + audio.samples.len()) as u64);
        let segments = self.timelines[k]
            .1
            .iter()
            .filter(|w| w.midpoint() >= start && w.midpoint() < end)
            .map(|w| {
                // A word whose midpoint is inside but whose start is before the buffer is
                // clamped to the buffer start (the subtraction wrapped to u64::MAX before,
                // which the store refuses; first hit when the 30 Hz high-pass moved a piece
                // boundary, T6.23b).
                let rel = |t: MeetingTime| {
                    MeetingTime::from_millis(t.max(start).min(end).as_millis() - start.as_millis())
                };
                let word = Word {
                    text: w.text.clone(),
                    start: rel(w.start),
                    end: rel(w.end),
                    probability: w.probability,
                };
                let mut seg = RawSegment::from_words(vec![word]);
                if w.text == "LOOP" {
                    seg.text = "I checked the valve. ".repeat(100);
                }
                seg
            })
            .collect();
        Ok(Transcription {
            segments,
            language: Some("en".into()),
            backend: self.backend_info(),
        })
    }

    fn backend_info(&self) -> BackendInfo {
        BackendInfo {
            kind: self.backend.0,
            device: self.backend.1.clone(),
            model: self.model.clone(),
        }
    }
}

fn noise(seed: u64, n: usize, amp: f32) -> Vec<f32> {
    use rand_core::Rng;
    let mut rng = ChaCha8Rng::seed_from_u64(seed);
    (0..n)
        .map(|_| ((rng.next_u32() >> 8) as f32 / (1u32 << 24) as f32 * 2.0 - 1.0) * amp)
        .collect()
}

/// `n` words named `<prefix><i>` from `start_ms`, one every 400 ms, 300 ms long.
fn turn(prefix: &str, start_ms: u64, n: usize) -> Vec<Word> {
    (0..n)
        .map(|i| {
            let s = start_ms + 400 * i as u64;
            Word::new(format!("{prefix}{i}"), s, s + 300, 0.9)
        })
        .collect()
}

/// Loud noise (amplitude 0.4) inside the word spans, a quiet background (0.003) elsewhere.
fn speech(words: &[Word], total_ms: u64, seed: u64) -> Vec<f32> {
    let mut out = noise(seed, (total_ms * 16) as usize, 0.003);
    for (i, w) in words.iter().enumerate() {
        let (a, b) = (
            (w.start.as_millis() * 16) as usize,
            (w.end.as_millis() * 16) as usize,
        );
        out[a..b].copy_from_slice(&noise(seed + 1 + i as u64, b - a, 0.4));
    }
    out
}

/// Four turns separated by pauses (four pieces), 28 s.
fn four_turns() -> Vec<Word> {
    let mut w = turn("a", 1_000, 5);
    w.extend(turn("b", 7_000, 6));
    w.extend(turn("c", 14_000, 5));
    w.extend(turn("d", 21_000, 6));
    w
}

fn live_config(transcription: LiveTranscription) -> LiveConfig {
    LiveConfig {
        title: "Reuse".into(),
        clock_domain: ClockDomain::Replay,
        idle_wait: Duration::from_millis(1),
        pipeline: PipelineConfig {
            drift_correction: false,
            ..PipelineConfig::default()
        },
        transcription,
        ..LiveConfig::default()
    }
}

/// Live ASR with an [`Oracle`] per track over its timeline (`None`: no live ASR there).
fn oracle_asr<T: Transcriber + Send + 'static>(
    mic: Option<T>,
    reference: Option<T>,
) -> LiveAsrFactory {
    let mut slots = [mic, reference];
    Box::new(move |track, entity| {
        let slot = match track {
            TrackId::Mic => 0,
            TrackId::Reference => 1,
        };
        let Some(oracle) = slots[slot].take() else {
            return Ok(None);
        };
        let mut n = 0u8;
        Ok(Some(
            LiveAsr {
                transcriber: Box::new(oracle),
                vad: Box::new(RmsVad),
                config: ChunkerConfig::default(),
                track_entity: entity,
                segment_ids: Box::new(move || {
                    n = n.wrapping_add(1);
                    EntityId::from_bytes([
                        0xD0 + slot as u8,
                        n,
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
    })
}

struct Recorded {
    svc: Services,
    _dir: TempDir,
    outcome: LiveOutcome,
    input: PostInput,
}

fn record<S>(source: S, transcription: LiveTranscription, factory: LiveAsrFactory) -> Recorded
where
    S: AudioSource + Send + 'static,
    S::Error: std::fmt::Debug,
{
    let dir = TempDir::new();
    let svc = services(&dir);
    let session = start_live(
        source,
        svc.clone(),
        live_config(transcription),
        Some(factory),
        Box::new(ChaCha8Rng::seed_from_u64(81)),
    )
    .unwrap();
    let outcome = session.wait().unwrap();
    let (input, missing) = load_post_input(&svc, outcome.meeting, &outcome.final_pass).unwrap();
    assert!(missing.is_empty());
    Recorded {
        svc,
        _dir: dir,
        outcome,
        input,
    }
}

fn mic_only(audio: &[f32]) -> earshot_core::capture::ReplaySource {
    ReplayBuilder::new(ORIGIN, MonoBuffer::new(PIPELINE_RATE, audio.to_vec()))
        .build()
        .unwrap()
}

fn processor<T: Transcriber + Send>(
    oracle: T,
    model: &str,
) -> PostProcessor<T, LevelSegmenter, LevelEmbedder> {
    let id = EmbeddingModelId::new(model);
    let cfg = PostConfig {
        clustering_thresholds: ModelThresholds::new().with(id.clone(), 0.704_565_5),
        voiceprint_thresholds: ModelThresholds::new().with(id.clone(), 0.5),
        ..PostConfig::default()
    };
    PostProcessor::new(
        oracle,
        LevelSegmenter(SegmenterInfo::pyannote_segmentation_3_0("level")),
        LevelEmbedder(EmbedderInfo {
            model: id,
            dimension: 4,
            min_samples: 4000,
        }),
        cfg,
    )
    .unwrap()
    .with_pieces(Box::new(RmsVad), pieces())
    .unwrap()
}

/// `input` as the whole-meeting pass sees it: no stretch records.
fn whole(input: &PostInput) -> PostInput {
    let mut input = input.clone();
    for t in &mut input.tracks {
        t.stretches = None;
    }
    input
}

/// One stored final segment: its track, its words (text and times) and its speaker's
/// role and name.
type FinalView = (
    EntityId,
    Vec<(String, u64, u64)>,
    Option<(SpeakerRole, String)>,
);

fn final_view(svc: &Services) -> Vec<FinalView> {
    let store = svc.store();
    let meeting = store.meetings().unwrap()[0].id;
    let speakers = store.speakers(meeting).unwrap();
    live_segments(&**store, meeting, TranscriptPass::Final)
        .unwrap()
        .into_iter()
        .map(|r| {
            let s = r.segment;
            let words = s
                .words
                .iter()
                .map(|w| (w.text.clone(), w.start.as_millis(), w.end.as_millis()))
                .collect();
            let speaker = s.speaker_id.and_then(|id| {
                speakers
                    .iter()
                    .find(|x| x.id == id)
                    .map(|x| (x.role, x.display_name.clone()))
            });
            (s.track, words, speaker)
        })
        .collect()
}

fn words_of(view: &[FinalView]) -> Vec<String> {
    view.iter()
        .flat_map(|(_, w, _)| w.iter().map(|(t, _, _)| t.clone()))
        .collect()
}

/// Runs `post` on `input` and returns the outcome and the stored final transcript.
fn run<T: Transcriber + Send>(
    post: &mut PostProcessor<T, LevelSegmenter, LevelEmbedder>,
    svc: &Services,
    input: PostInput,
    seed: u64,
) -> (PostOutcome, Vec<FinalView>) {
    let outcome = post
        .run(
            svc,
            input,
            &mut ChaCha8Rng::seed_from_u64(seed),
            &CancelToken::new(),
        )
        .unwrap();
    (outcome, final_view(svc))
}

fn mic_record(rec: &Recorded) -> StretchRecord {
    let store = rec.svc.store();
    match stretch_record(&store, rec.outcome.meeting, TrackId::Mic)
        .unwrap()
        .expect("the stretch worker left a record")
    {
        StoredStretches::Record(r) => *r,
        StoredStretches::Unreadable(why) => panic!("the stretch record is unreadable: {why}"),
    }
}

/// The backend the fake live workers report (see [`Oracle::on`]).
fn live_backend() -> BackendInfo {
    BackendInfo {
        kind: BackendKind::Cuda,
        device: "live-gpu".into(),
        model: "fake".into(),
    }
}

fn use_of(outcome: &PostOutcome, track: TrackId) -> &StretchUse {
    &outcome
        .stretches
        .iter()
        .find(|(t, _)| *t == track)
        .expect("every processed track reports its stretch use")
        .1
}

#[test]
fn post_stretches_covering_the_meeting_are_reused_without_transcribing() {
    let w = four_turns();
    let audio = speech(&w, 28_000, 300);
    // The live worker runs on another backend than the (idle) final-pass transcriber.
    let live = Oracle::new(vec![(audio.clone(), w.clone())]).on(BackendKind::Cuda, "live-gpu");
    let factory = oracle_asr(Some(live), None);
    let rec = record(
        mic_only(&audio),
        LiveTranscription::Stretches(pieces()),
        factory,
    );
    // The final pass decodes like the stretches (greedy, D20), so they can be reused.
    assert_eq!(
        rec.outcome.final_pass.tracks[0].options,
        ChunkerConfig::default().options
    );
    let record = mic_record(&rec);
    assert_eq!(record.decoded.through, audio.len());
    assert_eq!(record.decoded.pieces.len(), 4);

    let mut reuse = processor(Oracle::new(vec![(audio.clone(), w.clone())]), LEVEL_MODEL);
    // The cleanup estimate sees the same decision: nothing left to transcribe.
    let all = Duration::from_secs(28);
    assert_eq!(
        reuse.cleanup_work(&rec.input),
        [CleanupWork {
            track: TrackId::Mic,
            to_transcribe: Duration::ZERO,
            to_diarise: all,
        }]
    );
    assert_eq!(reuse.cleanup_work(&whole(&rec.input))[0].to_transcribe, all);
    let (outcome, reused) = run(&mut reuse, &rec.svc, rec.input.clone(), 82);
    assert!(
        reuse.transcriber.calls().is_empty(),
        "nothing is transcribed again"
    );
    assert_eq!(
        *use_of(&outcome, TrackId::Mic),
        StretchUse::Reused {
            through: MeetingTime::from_pipeline_frames(audio.len() as u64),
            pieces: 4,
            backend: live_backend(),
        }
    );
    // Every word came from the live worker: the pass backend is its backend, stored too.
    assert_eq!(outcome.backend, live_backend());
    assert_eq!(
        crate::backends::pass_backend(
            &rec.svc.store(),
            rec.outcome.meeting,
            TranscriptPass::Final,
            None
        )
        .unwrap(),
        Some(live_backend())
    );

    let mut again = processor(Oracle::new(vec![(audio.clone(), w.clone())]), LEVEL_MODEL);
    let (outcome, whole_pass) = run(&mut again, &rec.svc, whole(&rec.input), 82);
    assert_eq!(again.transcriber.calls().len(), 4);
    assert_eq!(outcome.backend, again.transcriber.backend_info());
    assert!(matches!(
        use_of(&outcome, TrackId::Mic),
        StretchUse::Transcribed { .. }
    ));
    assert_eq!(reused, whole_pass);
    let texts: Vec<String> = w.iter().map(|x| x.text.clone()).collect();
    assert_eq!(
        words_of(&reused),
        texts,
        "every word reaches the final transcript"
    );
}

/// An [`Oracle`] whose answer also depends on every bit of the buffer: one more word, at
/// the buffer's end, named after a hash of the samples. Two decodes give the same words
/// only when they saw bit-identical audio. Every buffer is kept.
struct BitsOracle {
    inner: Oracle,
    seen: Arc<Mutex<Vec<Vec<f32>>>>,
}

impl Transcriber for BitsOracle {
    fn transcribe(
        &mut self,
        audio: &MonoBuffer,
        options: &TranscribeOptions,
        cancel: &CancelToken,
    ) -> Result<Transcription, AsrError> {
        let mut t = self.inner.transcribe(audio, options, cancel)?;
        // FNV-1a over the sample bits.
        let h = audio
            .samples
            .iter()
            .fold(0xcbf2_9ce4_8422_2325_u64, |h, x| {
                (h ^ u64::from(x.to_bits())).wrapping_mul(0x0100_0000_01b3)
            });
        let end = (audio.samples.len() / 16) as u64;
        t.segments.push(RawSegment::from_words(vec![Word::new(
            format!("h{h:016x}"),
            end.saturating_sub(5),
            end,
            0.9,
        )]));
        self.seen.lock().unwrap().push(audio.samples.clone());
        Ok(t)
    }

    fn backend_info(&self) -> BackendInfo {
        self.inner.backend_info()
    }
}

#[test]
fn post_stretches_decode_the_audio_the_recording_stores_bit_for_bit() {
    // T6.13e: the live stretches are decoded from the 16-bit samples the FLAC stores, so
    // reusing them equals the whole-meeting pass even for a model whose output depends on
    // every bit (on the f32 samples, VAD cuts and decodes could differ: T6.13 quality run).
    let w = four_turns();
    let audio = speech(&w, 28_000, 301);
    assert_ne!(
        earshot_core::audio::quantise_pcm16(&audio),
        audio,
        "the meeting audio is not already 16-bit"
    );
    let live_seen = Arc::new(Mutex::new(Vec::new()));
    let live = BitsOracle {
        inner: Oracle::new(vec![(audio.clone(), w.clone())]),
        seen: Arc::clone(&live_seen),
    };
    let rec = record(
        mic_only(&audio),
        LiveTranscription::Stretches(pieces()),
        oracle_asr(Some(live), None),
    );
    let seen = live_seen.lock().unwrap().clone();
    assert_eq!(seen.len(), 4, "one decode per piece");
    for b in &seen {
        assert_eq!(
            b,
            &earshot_core::audio::quantise_pcm16(b),
            "the live worker decoded samples a 16-bit recording cannot hold"
        );
    }
    let bits = |seen: &Arc<Mutex<Vec<Vec<f32>>>>| BitsOracle {
        inner: Oracle::new(vec![(audio.clone(), w.clone())]),
        seen: Arc::clone(seen),
    };
    let reuse_seen = Arc::new(Mutex::new(Vec::new()));
    let mut reuse = processor(bits(&reuse_seen), LEVEL_MODEL);
    let (outcome, reused) = run(&mut reuse, &rec.svc, rec.input.clone(), 83);
    assert!(matches!(
        use_of(&outcome, TrackId::Mic),
        StretchUse::Reused { pieces: 4, .. }
    ));
    assert!(
        reuse_seen.lock().unwrap().is_empty(),
        "nothing decoded again"
    );
    let whole_seen = Arc::new(Mutex::new(Vec::new()));
    let mut again = processor(bits(&whole_seen), LEVEL_MODEL);
    let (_, whole_pass) = run(&mut again, &rec.svc, whole(&rec.input), 83);
    // The whole pass decoded exactly the buffers the live worker did.
    assert_eq!(*whole_seen.lock().unwrap(), seen);
    assert_eq!(reused, whole_pass);
}

/// A mono source that delivers nothing after `hold_at` frames until `release` says so
/// (at most 60 s).
struct HoldSource<S> {
    inner: S,
    hold_at: usize,
    delivered: Arc<Mutex<usize>>,
    release: Box<dyn FnMut() -> bool + Send>,
    since: Option<std::time::Instant>,
    released: bool,
}

struct Counting<'a> {
    inner: &'a mut dyn CaptureSink,
    frames: &'a Mutex<usize>,
}

impl CaptureSink for Counting<'_> {
    fn frames(&mut self, track: TrackId, samples: &[f32], at: DeviceTime) {
        *self.frames.lock().unwrap() += samples.len();
        self.inner.frames(track, samples, at);
    }
    fn discontinuity(&mut self, track: TrackId, d: earshot_core::capture::Discontinuity) {
        self.inner.discontinuity(track, d);
    }
    fn end(&mut self, end: earshot_core::capture::EndOfStream) {
        self.inner.end(end);
    }
}

impl<S: AudioSource> AudioSource for HoldSource<S> {
    type Error = S::Error;

    fn session(&self) -> &CaptureSession {
        self.inner.session()
    }

    fn pump(&mut self, sink: &mut dyn CaptureSink) -> Result<SourceStatus, Self::Error> {
        if !self.released && *self.delivered.lock().unwrap() >= self.hold_at {
            let since = *self.since.get_or_insert_with(std::time::Instant::now);
            if (self.release)() || since.elapsed() > Duration::from_secs(60) {
                self.released = true;
            } else {
                return Ok(SourceStatus::Idle);
            }
        }
        let mut counting = Counting {
            inner: sink,
            frames: &self.delivered,
        };
        self.inner.pump(&mut counting)
    }

    fn stop(&mut self) {
        self.inner.stop();
    }
}

#[test]
fn post_stretches_stopped_mid_meeting_leave_only_the_rest_to_transcribe() {
    // Live transcription fails on its third piece: the record keeps the two pieces before
    // it and where they end; the final pass transcribes only what comes after.
    let w = four_turns();
    let audio = speech(&w, 28_000, 310);
    let mut live = Oracle::new(vec![(audio.clone(), w.clone())]).on(BackendKind::Cuda, "live-gpu");
    live.fail_on = Some((3, false));
    let factory = oracle_asr(Some(live), None);
    // Hold the audio after 13 s (the second turn is settled, the third not begun) until
    // two stretches are stored, so the failing third piece is decoded by a later push.
    let slot: Arc<Mutex<Option<Services>>> = Arc::default();
    let release = {
        let slot = Arc::clone(&slot);
        Box::new(move || {
            let Some(svc) = slot.lock().unwrap().clone() else {
                return false;
            };
            let store = svc.store();
            let Some(m) = store.meetings().unwrap().first().map(|m| m.id) else {
                return false;
            };
            live_segments(&**store, m, TranscriptPass::Live)
                .unwrap()
                .len()
                >= 2
        })
    };
    let source = HoldSource {
        inner: mic_only(&audio),
        hold_at: 13 * 16_000,
        delivered: Arc::default(),
        release,
        since: None,
        released: false,
    };
    let dir = TempDir::new();
    let svc = services(&dir);
    *slot.lock().unwrap() = Some(svc.clone());
    let session = start_live(
        source,
        svc.clone(),
        live_config(LiveTranscription::Stretches(pieces())),
        Some(factory),
        Box::new(ChaCha8Rng::seed_from_u64(83)),
    )
    .unwrap();
    let outcome = session.wait().unwrap();
    let (input, _) = load_post_input(&svc, outcome.meeting, &outcome.final_pass).unwrap();
    let rec = Recorded {
        svc,
        _dir: dir,
        outcome,
        input,
    };
    let record = mic_record(&rec);
    assert_eq!(record.decoded.pieces.len(), 2, "{record:?}");
    assert!(
        record
            .stopped
            .as_deref()
            .is_some_and(|s| s.contains("gpu lost"))
    );
    let through = record.decoded.through;
    assert!((10 * 16_000..14 * 16_000).contains(&through), "{through}");

    let mut reuse = processor(Oracle::new(vec![(audio.clone(), w.clone())]), LEVEL_MODEL);
    assert_eq!(
        reuse.cleanup_work(&rec.input)[0].to_transcribe,
        Duration::from_secs_f64((audio.len() - through) as f64 / 16_000.0)
    );
    let (outcome, reused) = run(&mut reuse, &rec.svc, rec.input.clone(), 84);
    let calls = reuse.transcriber.calls();
    assert_eq!(calls.len(), 2, "only the last two pieces: {calls:?}");
    assert!(
        calls.iter().all(|&(_, offset, _)| offset >= through),
        "{calls:?}"
    );
    assert_eq!(
        *use_of(&outcome, TrackId::Mic),
        StretchUse::Reused {
            through: MeetingTime::from_pipeline_frames(through as u64),
            pieces: 2,
            backend: live_backend(),
        }
    );
    // The final pass decoded the rest: the pass backend is its own.
    assert_eq!(outcome.backend, reuse.transcriber.backend_info());
    assert_eq!(
        crate::backends::pass_backend(
            &rec.svc.store(),
            rec.outcome.meeting,
            TranscriptPass::Final,
            None
        )
        .unwrap(),
        Some(reuse.transcriber.backend_info())
    );

    let mut again = processor(Oracle::new(vec![(audio.clone(), w.clone())]), LEVEL_MODEL);
    let (_, whole_pass) = run(&mut again, &rec.svc, whole(&rec.input), 84);
    assert_eq!(again.transcriber.calls().len(), 4);
    assert_eq!(reused, whole_pass);
    let texts: Vec<String> = w.iter().map(|x| x.text.clone()).collect();
    assert_eq!(words_of(&reused), texts);
}

#[test]
fn post_stretches_of_another_model_are_transcribed_again() {
    let w = four_turns();
    let audio = speech(&w, 28_000, 320);
    let factory = oracle_asr(Some(Oracle::new(vec![(audio.clone(), w.clone())])), None);
    let rec = record(
        mic_only(&audio),
        LiveTranscription::Stretches(pieces()),
        factory,
    );
    let mut other = Oracle::new(vec![(audio.clone(), w.clone())]);
    other.model = "other".into();
    let mut post = processor(other, LEVEL_MODEL);
    assert_eq!(
        post.cleanup_work(&rec.input)[0].to_transcribe,
        Duration::from_secs(28)
    );
    let (outcome, got) = run(&mut post, &rec.svc, rec.input.clone(), 85);
    assert_eq!(post.transcriber.calls().len(), 4, "the whole track again");
    match use_of(&outcome, TrackId::Mic) {
        StretchUse::Transcribed { reason } => {
            assert!(reason.contains("model"), "{reason}");
        }
        u => panic!("{u:?}"),
    }
    // Other piece settings are not the same transcription either.
    let mut post2 = processor(Oracle::new(vec![(audio.clone(), w.clone())]), LEVEL_MODEL);
    post2.pieces.as_mut().unwrap().config.context_gap = Duration::from_secs(3);
    let (outcome, _) = run(&mut post2, &rec.svc, rec.input.clone(), 85);
    assert_eq!(post2.transcriber.calls().len(), 4);
    assert!(matches!(
        use_of(&outcome, TrackId::Mic),
        StretchUse::Transcribed { reason } if reason.contains("piece")
    ));
    // Nor other decoding options (a beam-search final pass).
    let mut post3 = processor(Oracle::new(vec![(audio.clone(), w.clone())]), LEVEL_MODEL);
    let mut beam = rec.input.clone();
    beam.tracks[0].options = TranscribeOptions::batch();
    let (outcome, _) = run(&mut post3, &rec.svc, beam, 85);
    assert_eq!(post3.transcriber.calls().len(), 4);
    assert!(matches!(
        use_of(&outcome, TrackId::Mic),
        StretchUse::Transcribed { reason } if reason.contains("decoding")
    ));
    // Nor another VAD with the same frame length (its pieces can differ).
    let mut post4 = processor(Oracle::new(vec![(audio.clone(), w.clone())]), LEVEL_MODEL);
    post4.pieces.as_mut().unwrap().vad = Box::new(RenamedVad);
    let (outcome, _) = run(&mut post4, &rec.svc, rec.input.clone(), 85);
    assert_eq!(post4.transcriber.calls().len(), 4);
    assert!(matches!(
        use_of(&outcome, TrackId::Mic),
        StretchUse::Transcribed { reason } if reason.contains("VAD rms-vad")
            && reason.contains("other-vad")
    ));
    let texts: Vec<String> = w.iter().map(|x| x.text.clone()).collect();
    assert_eq!(words_of(&got), texts);
}

/// [`RmsVad`] under another identity.
struct RenamedVad;

impl Vad for RenamedVad {
    fn id(&self) -> String {
        "other-vad".into()
    }
    fn frame_len(&self) -> usize {
        RmsVad.frame_len()
    }
    fn speech_probability(&mut self, frame: &[Sample]) -> Result<f32, AsrError> {
        RmsVad.speech_probability(frame)
    }
    fn reset(&mut self) {}
}

#[test]
fn post_unreadable_stretch_record_is_a_warning_and_a_whole_pass() {
    // A record this build cannot read (a future version, a damaged value) never fails the
    // final pass: the track is transcribed whole, with a warning saying why.
    let w = four_turns();
    let audio = speech(&w, 28_000, 325);
    let factory = oracle_asr(Some(Oracle::new(vec![(audio.clone(), w.clone())])), None);
    let rec = record(
        mic_only(&audio),
        LiveTranscription::Stretches(pieces()),
        factory,
    );
    let key = crate::stretches::record_key(rec.outcome.meeting, TrackId::Mic);
    let mut future = (crate::stretches::STRETCH_RECORD_VERSION + 1)
        .to_le_bytes()
        .to_vec();
    future.extend_from_slice(b"{}");
    for (value, says) in [
        (future, "version 3"),
        (vec![2, 0, 0, 0, b'x'], "stretch record"),
    ] {
        rec.svc
            .store()
            .set_local_value(key.as_str(), &value)
            .unwrap();
        let (input, missing) =
            load_post_input(&rec.svc, rec.outcome.meeting, &rec.outcome.final_pass).unwrap();
        assert!(missing.is_empty());
        let mut post = processor(Oracle::new(vec![(audio.clone(), w.clone())]), LEVEL_MODEL);
        let (outcome, got) = run(&mut post, &rec.svc, input, 87);
        assert_eq!(post.transcriber.calls().len(), 4, "the whole track again");
        let StretchUse::Transcribed { reason } = use_of(&outcome, TrackId::Mic) else {
            panic!("{:?}", use_of(&outcome, TrackId::Mic));
        };
        assert!(reason.contains("cannot be read"), "{reason}");
        assert!(reason.contains(says), "{reason}");
        // One report: exactly one warning, with the stretch use's own reason.
        let warned: Vec<&String> = outcome
            .warnings
            .iter()
            .filter_map(|w| match w {
                PostWarning::StretchRecordUnreadable {
                    track: TrackId::Mic,
                    reason,
                } => Some(reason),
                _ => None,
            })
            .collect();
        assert_eq!(warned, [reason], "{:?}", outcome.warnings);
        let texts: Vec<String> = w.iter().map(|x| x.text.clone()).collect();
        assert_eq!(words_of(&got), texts);
        // The final pass deleted the record in its transaction, as for a readable one.
        assert!(
            stretch_record(&rec.svc.store(), rec.outcome.meeting, TrackId::Mic)
                .unwrap()
                .is_none()
        );
    }
}

#[test]
fn post_chunker_meeting_is_transcribed_whole() {
    let w = four_turns();
    let audio = speech(&w, 28_000, 330);
    let factory = oracle_asr(Some(Oracle::new(vec![(audio.clone(), w.clone())])), None);
    let rec = record(mic_only(&audio), LiveTranscription::Chunker, factory);
    assert!(
        stretch_record(&rec.svc.store(), rec.outcome.meeting, TrackId::Mic)
            .unwrap()
            .is_none()
    );
    // The chunker's final pass keeps the batch decoding (beam search).
    assert_eq!(
        rec.outcome.final_pass.tracks[0].options,
        TranscribeOptions::batch()
    );
    let mut post = processor(Oracle::new(vec![(audio.clone(), w.clone())]), LEVEL_MODEL);
    let (outcome, got) = run(&mut post, &rec.svc, rec.input.clone(), 86);
    assert_eq!(post.transcriber.calls().len(), 4);
    assert_eq!(
        *use_of(&outcome, TrackId::Mic),
        StretchUse::Transcribed {
            reason: "no stretch record".into()
        }
    );
    let texts: Vec<String> = w.iter().map(|x| x.text.clone()).collect();
    assert_eq!(words_of(&got), texts);
}

#[test]
fn post_stretch_dropped_piece_is_reported_like_a_final_pass_drop() {
    // The second turn loops on every decode: the stretch worker drops it, the record keeps
    // the drop, and the reused final pass reports it exactly as the whole pass does.
    let mut w = turn("a", 1_000, 5);
    w.push(Word::new("LOOP", 7_000, 9_000, 0.9));
    w.extend(turn("c", 14_000, 5));
    let audio = speech(&w, 20_000, 340);
    let factory = oracle_asr(Some(Oracle::new(vec![(audio.clone(), w.clone())])), None);
    let rec = record(
        mic_only(&audio),
        LiveTranscription::Stretches(pieces()),
        factory,
    );
    let record = mic_record(&rec);
    assert_eq!(
        record
            .decoded
            .pieces
            .iter()
            .map(|p| p.dropped)
            .collect::<Vec<_>>(),
        [false, true, false]
    );
    let dropped = |o: &PostOutcome| -> Vec<PostWarning> {
        o.warnings
            .iter()
            .filter(|x| matches!(x, PostWarning::RepetitionDropped { .. }))
            .cloned()
            .collect()
    };
    let mut reuse = processor(Oracle::new(vec![(audio.clone(), w.clone())]), LEVEL_MODEL);
    let (outcome, reused) = run(&mut reuse, &rec.svc, rec.input.clone(), 87);
    assert!(reuse.transcriber.calls().is_empty());
    let mut again = processor(Oracle::new(vec![(audio.clone(), w.clone())]), LEVEL_MODEL);
    let (whole_outcome, whole_pass) = run(&mut again, &rec.svc, whole(&rec.input), 87);
    assert!(!again.transcriber.calls().is_empty());
    assert_eq!(dropped(&outcome), dropped(&whole_outcome));
    assert_eq!(
        dropped(&outcome),
        [PostWarning::RepetitionDropped {
            track: TrackId::Mic,
            spans: vec![(
                MeetingTime::from_millis(6_600),
                MeetingTime::from_millis(9_400)
            )],
        }]
    );
    assert_eq!(reused, whole_pass);
    assert!(!words_of(&reused).contains(&"LOOP".to_owned()));
}

#[test]
fn post_stretch_echo_reuse_equals_the_whole_pass_with_the_live_echo_filter_on() {
    // A call on loudspeakers (the live tests' leaky call): the live echo filter removes
    // echo words from the live "Me" transcript, but the record keeps the stretches' raw
    // words, so the final pass cleans echo from exactly what the whole pass hears.
    use crate::live::tests::{cleaned, leaky_call, region_words};
    let sim = leaky_call(true);
    let n = sim.mic.len() / earshot_core::aec::BLOCK * earshot_core::aec::BLOCK;
    let (mic, reference) = (sim.mic[..n].to_vec(), sim.reference[..n].to_vec());
    let far = region_words("far", &sim.far_regions, 0);
    let near = region_words("near", &sim.near_regions, 0);
    let echo: Vec<Word> = far
        .iter()
        .map(|x| x.shifted(MeetingTime::from_millis(102)))
        .filter(|x| x.end.as_millis() < (n / 16) as u64)
        .collect();
    let mut mic_words: Vec<Word> = near.iter().chain(&echo).cloned().collect();
    mic_words.sort_by_key(|x| x.start);
    let mic_stream = cleaned(&mic, &reference);
    let timelines = || {
        vec![
            (mic_stream.clone(), mic_words.clone()),
            (reference.clone(), far.clone()),
        ]
    };
    // The two live workers ran on different backends.
    let factory = oracle_asr(
        Some(
            Oracle::new(vec![(mic_stream.clone(), mic_words.clone())])
                .on(BackendKind::Cuda, "live-gpu"),
        ),
        Some(Oracle::new(vec![(reference.clone(), far.clone())]).on(BackendKind::Cpu, "cpu")),
    );
    let source = ReplayBuilder::new(ORIGIN, MonoBuffer::new(PIPELINE_RATE, mic.clone()))
        .reference(MonoBuffer::new(PIPELINE_RATE, reference.clone()))
        .build()
        .unwrap();
    let rec = record(source, LiveTranscription::Stretches(pieces()), factory);
    assert!(
        rec.outcome.echo_words_removed > 0,
        "the live echo filter acted"
    );
    assert!(!rec.input.echo_locks.is_empty());
    // The record holds the raw words, echo included; the live transcript does not.
    let raw: Vec<String> = mic_record(&rec)
        .decoded
        .pieces
        .iter()
        .flat_map(|p| {
            p.segments
                .iter()
                .flat_map(|s| s.words.iter().map(|x| x.text.clone()))
        })
        .collect();
    let live: Vec<String> = {
        let store = rec.svc.store();
        let mic_id = rec.input.tracks[0].track.id;
        live_segments(&**store, rec.outcome.meeting, TranscriptPass::Live)
            .unwrap()
            .into_iter()
            .filter(|r| r.segment.track == mic_id)
            .flat_map(|r| r.segment.words.into_iter().map(|x| x.text))
            .collect()
    };
    assert!(
        raw.len() > live.len(),
        "{} raw, {} live",
        raw.len(),
        live.len()
    );
    assert!(raw.iter().any(|t| t.starts_with("far")));

    let mut reuse = processor(Oracle::new(timelines()), CAMPPLUS_ID);
    let (outcome, reused) = run(&mut reuse, &rec.svc, rec.input.clone(), 88);
    assert!(reuse.transcriber.calls().is_empty());
    assert!(matches!(
        use_of(&outcome, TrackId::Mic),
        StretchUse::Reused { .. }
    ));
    assert!(matches!(
        use_of(&outcome, TrackId::Reference),
        StretchUse::Reused { backend, .. } if backend.kind == BackendKind::Cpu
    ));
    // Each track names its own live backend; no single one ran the pass, so the pass
    // backend is the final-pass transcriber's, never the first track's.
    assert!(matches!(
        use_of(&outcome, TrackId::Mic),
        StretchUse::Reused { backend, .. } if backend.device == "live-gpu"
    ));
    assert_eq!(outcome.backend, reuse.transcriber.backend_info());
    let mut again = processor(Oracle::new(timelines()), CAMPPLUS_ID);
    let (whole_outcome, whole_pass) = run(&mut again, &rec.svc, whole(&rec.input), 88);
    assert!(!again.transcriber.calls().is_empty());
    assert_eq!(reused, whole_pass);
    assert_eq!(outcome.echo, whole_outcome.echo);
    assert!(outcome.echo.removed() > 0, "the final pass cleaned echo");
}

#[test]
fn post_live_recording_cut_short_track_goes_through_the_final_pass() {
    // T9.17: a full disk stopped the mic's recording part way, the reference went on. The final
    // pass takes the two files at their own lengths: the mic's words up to the cut, every word
    // of the reference.
    let mic_words = four_turns();
    let mut far = turn("far", 3_000, 8);
    far.extend(turn("far", 18_000, 8));
    let total_ms = 28_000;
    let mic = speech(&mic_words, total_ms, 300);
    let reference = speech(&far, total_ms, 400);
    let dir = TempDir::new();
    let svc = services(&dir);
    crate::live::write_faults::set(
        &svc.data,
        TrackId::Mic,
        earshot_store::WriteFault::DiskFullAfter(150_000),
    );
    let source = ReplayBuilder::new(ORIGIN, MonoBuffer::new(PIPELINE_RATE, mic))
        .reference(MonoBuffer::new(PIPELINE_RATE, reference))
        .build()
        .unwrap();
    let session = start_live(
        source,
        svc.clone(),
        live_config(LiveTranscription::Stretches(pieces())),
        Some(oracle_asr::<Oracle>(None, None)),
        Box::new(ChaCha8Rng::seed_from_u64(82)),
    )
    .unwrap();
    let live = session.wait().unwrap();
    assert!(
        live.problems
            .iter()
            .any(|p| matches!(p, crate::live::RuntimeWarning::RecordingStopped { .. })),
        "{:?}",
        live.problems
    );
    let (input, missing) = load_post_input(&svc, live.meeting, &live.final_pass).unwrap();
    assert!(missing.is_empty(), "both tracks have a file");
    let (mic_track, ref_track) = (&input.tracks[0], &input.tracks[1]);
    assert_eq!(
        (mic_track.track.kind, ref_track.track.kind),
        (TrackId::Mic, TrackId::Reference)
    );
    let cut = mic_track.audio.samples.len();
    assert!(
        cut > 16_000 * 5 && cut < ref_track.audio.samples.len(),
        "the mic stops part way: {cut} of {}",
        ref_track.audio.samples.len()
    );
    let cut_at = MeetingTime::from_pipeline_frames(cut as u64);
    let kept_words: Vec<Word> = mic_words
        .iter()
        .filter(|w| w.midpoint() < cut_at)
        .cloned()
        .collect();
    assert!(kept_words.len() < mic_words.len());
    let timelines = vec![
        (mic_track.audio.samples.clone(), kept_words.clone()),
        (ref_track.audio.samples.clone(), far.clone()),
    ];
    let (mic_id, ref_id) = (mic_track.track.id, ref_track.track.id);
    let mut post = processor(Oracle::new(timelines), CAMPPLUS_ID);
    let (_, view) = run(&mut post, &svc, input, 90);
    let on = |id: EntityId| -> Vec<String> {
        view.iter()
            .filter(|(t, _, _)| *t == id)
            .flat_map(|(_, w, _)| w.iter().map(|(t, _, _)| t.clone()))
            .collect()
    };
    let names = |w: &[Word]| w.iter().map(|x| x.text.clone()).collect::<Vec<_>>();
    assert_eq!(on(mic_id), names(&kept_words));
    assert_eq!(on(ref_id), names(&far));
}

// ---------------------------------------------------------------- live speaker windows
// T6.13d2: the live worker computes speaker windows during the meeting; the final pass
// finishes from them. Same standard as the stretches: the stored final transcript and
// speakers equal the whole-meeting pass on the same recording.

/// The [`LevelEmbedder`] plus a little noise drawn from a hash of the region's exact
/// sample bits, so any difference between the samples the live worker saw and the FLAC
/// the final pass reads changes the embeddings (and so the centroids in the outcome).
struct HashedEmbedder(LevelEmbedder);

impl earshot_core::diar::SpeakerEmbedder for HashedEmbedder {
    fn info(&self) -> &EmbedderInfo {
        self.0.info()
    }
    fn embed(
        &mut self,
        speech: &[&[f32]],
        cancel: &CancelToken,
    ) -> Result<Vec<earshot_core::diar::Embedding>, earshot_core::diar::DiarError> {
        let mut out = self.0.embed(speech, cancel)?;
        for (e, s) in out.iter_mut().zip(speech) {
            let mut h: u64 = 0xCBF2_9CE4_8422_2325;
            for x in s.iter() {
                h = (h ^ u64::from(x.to_bits())).wrapping_mul(0x0000_0100_0000_01B3);
            }
            let jitter = noise(h, e.vector.len(), 0.05);
            for (v, j) in e.vector.iter_mut().zip(jitter) {
                *v += j;
            }
        }
        Ok(out)
    }
}

/// A segmenter that counts the windows it segments.
struct CountedSegmenter {
    inner: LevelSegmenter,
    windows: Arc<AtomicUsize>,
}

impl earshot_core::diar::SpeakerSegmenter for CountedSegmenter {
    fn info(&self) -> &SegmenterInfo {
        self.inner.info()
    }
    fn segment(
        &mut self,
        windows: &[&[f32]],
        cancel: &CancelToken,
    ) -> Result<Vec<earshot_core::diar::LocalActivity>, earshot_core::diar::DiarError> {
        self.windows.fetch_add(windows.len(), SeqCst);
        self.inner.segment(windows, cancel)
    }
}

pub(crate) type BoxSeg = Box<dyn earshot_core::diar::SpeakerSegmenter + Send>;
pub(crate) type BoxEmb = Box<dyn earshot_core::diar::SpeakerEmbedder + Send>;

/// The models of these tests: segmentation model `seg`, embedding model `emb`.
pub(crate) fn speaker_models(seg: &str, emb: &str, windows: &Arc<AtomicUsize>) -> (BoxSeg, BoxEmb) {
    (
        Box::new(CountedSegmenter {
            inner: LevelSegmenter(SegmenterInfo::pyannote_segmentation_3_0(seg)),
            windows: Arc::clone(windows),
        }),
        Box::new(HashedEmbedder(LevelEmbedder(EmbedderInfo {
            model: EmbeddingModelId::new(emb),
            dimension: 4,
            min_samples: 4000,
        }))),
    )
}

const SEG_MODEL: &str = "level-seg@1";

/// `inner` plus live speaker work with [`speaker_models`] (default window settings) and
/// `threads`; the live segmenter counts the windows it is given into `windows`, finished
/// batches count into `done`; with `failing`, the live segmenter fails (and sets the flag)
/// once the first batch is finished, so the second batch fails.
fn with_speakers(
    mut inner: LiveAsrFactory,
    threads: Option<usize>,
    windows: Arc<AtomicUsize>,
    done: Arc<AtomicUsize>,
    failing: Option<Arc<AtomicBool>>,
) -> LiveAsrFactory {
    Box::new(move |track, entity| {
        let Some(mut asr) = inner(track, entity)? else {
            return Ok(None);
        };
        let (seg, emb) = speaker_models(SEG_MODEL, LEVEL_MODEL, &windows);
        let seg: BoxSeg = match &failing {
            Some(failed) => Box::new(FailingSegmenter {
                inner: seg,
                done: Arc::clone(&done),
                failed: Arc::clone(failed),
            }),
            None => seg,
        };
        asr.speakers = Some(crate::live::LiveSpeakers {
            models: Box::new(move || {
                Ok(earshot_core::diar::Diarizer::new(
                    seg,
                    emb,
                    earshot_core::diar::DiarizationConfig::default(),
                )
                .unwrap())
            }),
            threads,
            progress: Some(Arc::clone(&done)),
            background: false,
        });
        Ok(Some(asr))
    })
}

/// A segmenter that fails once 32 windows are finished (`done`), whatever the cancel
/// token says, and sets `failed`.
struct FailingSegmenter {
    inner: BoxSeg,
    done: Arc<AtomicUsize>,
    failed: Arc<AtomicBool>,
}

impl earshot_core::diar::SpeakerSegmenter for FailingSegmenter {
    fn info(&self) -> &SegmenterInfo {
        self.inner.info()
    }
    fn segment(
        &mut self,
        windows: &[&[f32]],
        cancel: &CancelToken,
    ) -> Result<Vec<earshot_core::diar::LocalActivity>, earshot_core::diar::DiarError> {
        if self.done.load(SeqCst) >= 32 {
            self.failed.store(true, SeqCst);
            return Err(earshot_core::diar::DiarError::Backend("cpu lost".into()));
        }
        self.inner.segment(windows, cancel)
    }
}

/// An embedder that fails once its segmenter has segmented more than 32 windows.
struct FailingAfter {
    inner: BoxEmb,
    windows: Arc<AtomicUsize>,
}

impl earshot_core::diar::SpeakerEmbedder for FailingAfter {
    fn info(&self) -> &EmbedderInfo {
        self.inner.info()
    }
    fn embed(
        &mut self,
        speech: &[&[f32]],
        cancel: &CancelToken,
    ) -> Result<Vec<earshot_core::diar::Embedding>, earshot_core::diar::DiarError> {
        if self.windows.load(SeqCst) > 32 {
            return Err(earshot_core::diar::DiarError::Backend("cpu lost".into()));
        }
        self.inner.embed(speech, cancel)
    }
}

type SpeakerPost = PostProcessor<Oracle, BoxSeg, BoxEmb>;

/// A final-pass processor with [`speaker_models`] `seg` / `emb`, `onset` and `threads`.
fn speaker_processor(
    oracle: Oracle,
    seg: &str,
    emb: &str,
    onset: f32,
    threads: Option<usize>,
    windows: &Arc<AtomicUsize>,
) -> SpeakerPost {
    let id = EmbeddingModelId::new(emb);
    let mut cfg = PostConfig {
        clustering_thresholds: ModelThresholds::new().with(id.clone(), 0.704_565_5),
        voiceprint_thresholds: ModelThresholds::new().with(id, 0.5),
        ..PostConfig::default()
    };
    cfg.diarization.onset = onset;
    let (s, e) = speaker_models(seg, emb, windows);
    PostProcessor::new(oracle, s, e, cfg)
        .unwrap()
        .with_pieces(Box::new(RmsVad), pieces())
        .unwrap()
        .with_speaker_threads(threads)
}

fn run_speakers(
    post: &mut SpeakerPost,
    svc: &Services,
    input: PostInput,
) -> (PostOutcome, Vec<FinalView>) {
    let outcome = post
        .run(
            svc,
            input,
            &mut ChaCha8Rng::seed_from_u64(91),
            &CancelToken::new(),
        )
        .unwrap();
    (outcome, final_view(svc))
}

/// Two voices on the mic (word noise of amplitude 0.4 and 0.7, two level classes),
/// alternating turns over 96 s: more than two batches of 32 speaker windows.
fn two_voices() -> (Vec<f32>, Vec<Word>) {
    let mut w = Vec::new();
    let mut loud = Vec::new();
    for k in 0..15u64 {
        let prefix = if k % 2 == 0 { "a" } else { "b" };
        let turn_words = turn(&format!("{prefix}{k}x"), 1_000 + k * 6_200, 6);
        if k % 2 == 1 {
            loud.extend(turn_words.iter().cloned());
        }
        w.extend(turn_words);
    }
    let mut audio = speech(&w, 96_000, 400);
    for x in &loud {
        let (a, b) = (
            (x.start.as_millis() * 16) as usize,
            (x.end.as_millis() * 16) as usize,
        );
        for s in &mut audio[a..b] {
            *s *= 1.75;
        }
    }
    (audio, w)
}

/// `input` without live speaker windows or stretches (the whole-meeting pass).
fn whole_speakers(input: &PostInput) -> PostInput {
    let mut input = whole(input);
    for t in &mut input.tracks {
        t.speakers = None;
    }
    input
}

fn speakers_of(outcome: &PostOutcome) -> Vec<(SpeakerRole, TrackId, String, Duration)> {
    outcome
        .speakers
        .iter()
        .map(|s| (s.role, s.track, s.display_name.clone(), s.speech))
        .collect()
}

fn window_use(outcome: &PostOutcome, track: TrackId) -> &SpeakerWindowUse {
    &outcome
        .speaker_windows
        .iter()
        .find(|(t, _)| *t == track)
        .expect("every processed track reports its speaker window use")
        .1
}

/// A recorded two-voice meeting with live speaker work. The source holds the audio back
/// after 50 s until the speaker thread has finished a batch (32 windows): speaker work
/// demonstrably ran during the meeting whatever the host's load (the thread runs below
/// normal priority). With `failing`, the source holds again after 80 s until the live
/// segmenter failed on the second batch.
fn record_with_speakers(threads: Option<usize>, failing: bool) -> (Recorded, Vec<f32>, Vec<Word>) {
    let (audio, w) = two_voices();
    let windows = Arc::new(AtomicUsize::new(0));
    let done = Arc::new(AtomicUsize::new(0));
    let failed = Arc::new(AtomicBool::new(false));
    let factory = with_speakers(
        oracle_asr(Some(Oracle::new(vec![(audio.clone(), w.clone())])), None),
        threads,
        Arc::clone(&windows),
        Arc::clone(&done),
        failing.then(|| Arc::clone(&failed)),
    );
    let first = HoldSource {
        inner: mic_only(&audio),
        hold_at: 50 * 16_000,
        delivered: Arc::default(),
        release: {
            let done = Arc::clone(&done);
            Box::new(move || done.load(SeqCst) >= 32)
        },
        since: None,
        released: false,
    };
    let rec = if failing {
        let second = HoldSource {
            inner: first,
            hold_at: 80 * 16_000,
            delivered: Arc::default(),
            release: {
                let failed = Arc::clone(&failed);
                Box::new(move || failed.load(SeqCst))
            },
            since: None,
            released: false,
        };
        record(second, LiveTranscription::Stretches(pieces()), factory)
    } else {
        record(first, LiveTranscription::Stretches(pieces()), factory)
    };
    (rec, audio, w)
}

fn speaker_rec(rec: &Recorded) -> crate::speakers::SpeakerRecord {
    let store = rec.svc.store();
    match crate::speakers::speaker_record(&store, rec.outcome.meeting, TrackId::Mic)
        .unwrap()
        .expect("the live worker left a speaker record")
    {
        crate::speakers::StoredSpeakers::Record(r) => *r,
        crate::speakers::StoredSpeakers::Unreadable(why) => panic!("unreadable: {why}"),
        crate::speakers::StoredSpeakers::Off(why) => panic!("speaker work was off: {why}"),
    }
}

#[test]
fn post_speaker_windows_from_the_meeting_give_the_whole_track_speakers() {
    let (rec, audio, w) = record_with_speakers(Some(3), false);
    let record = speaker_rec(&rec);
    assert_eq!(record.header.stopped, None);
    assert_eq!(record.header.threads, Some(3));
    let done = record.results.windows();
    assert!(done >= 32 && done.is_multiple_of(32), "{done} windows");
    // Bit-exact against the recording: the same windows computed from the FLAC's samples
    // (activity, local speakers and every embedding bit) equal the stored ones.
    let counter = Arc::new(AtomicUsize::new(0));
    let (seg, emb) = speaker_models(SEG_MODEL, LEVEL_MODEL, &counter);
    let mut fresh = earshot_core::diar::Diarizer::new(
        seg,
        emb,
        earshot_core::diar::DiarizationConfig::default(),
    )
    .unwrap();
    let mut stream = fresh.stream();
    stream.append(&rec.input.tracks[0].audio.samples);
    fresh
        .advance(&mut stream, done / 32, &CancelToken::new())
        .unwrap();
    assert_eq!(stream.results(), &record.results);

    let windows = Arc::new(AtomicUsize::new(0));
    let oracle = || Oracle::new(vec![(audio.clone(), w.clone())]);
    let mut reuse = speaker_processor(oracle(), SEG_MODEL, LEVEL_MODEL, 0.5, Some(3), &windows);
    let (outcome, reused) = run_speakers(&mut reuse, &rec.svc, rec.input.clone());
    assert_eq!(
        *window_use(&outcome, TrackId::Mic),
        SpeakerWindowUse::Reused {
            windows: done,
            stopped: None
        }
    );
    let final_windows = windows.load(SeqCst);

    let whole_windows = Arc::new(AtomicUsize::new(0));
    let mut again = speaker_processor(
        oracle(),
        SEG_MODEL,
        LEVEL_MODEL,
        0.5,
        Some(3),
        &whole_windows,
    );
    let (whole_outcome, whole_pass) =
        run_speakers(&mut again, &rec.svc, whole_speakers(&rec.input));
    assert_eq!(
        *window_use(&whole_outcome, TrackId::Mic),
        SpeakerWindowUse::Diarised {
            reason: "no speaker record".into()
        }
    );
    let all = whole_windows.load(SeqCst);
    // Only the windows the meeting did not compute are segmented again.
    assert_eq!(
        final_windows + done,
        all,
        "{final_windows} + {done} vs {all}"
    );
    // Same speakers (roles, names, speech, centroids bit for bit: the live windows saw the
    // FLAC's samples) and the same labelled final transcript.
    assert_eq!(outcome.speakers.len(), 2, "{:?}", speakers_of(&outcome));
    assert_eq!(speakers_of(&outcome), speakers_of(&whole_outcome));
    assert_eq!(outcome.speakers, whole_outcome.speakers);
    assert_eq!(reused, whole_pass);
    assert!(
        reused.iter().any(|(_, _, s)| s.is_some()),
        "segments are labelled"
    );
}

#[test]
fn post_speaker_windows_are_not_reused_when_anything_differs() {
    let (rec, audio, w) = record_with_speakers(Some(3), false);
    // Read before any final pass deletes it.
    let stored = crate::speakers::encode(&speaker_rec(&rec));
    let oracle = || Oracle::new(vec![(audio.clone(), w.clone())]);
    let counter = Arc::new(AtomicUsize::new(0));
    let reference = {
        let mut post = speaker_processor(oracle(), SEG_MODEL, LEVEL_MODEL, 0.5, Some(3), &counter);
        run_speakers(&mut post, &rec.svc, whole_speakers(&rec.input)).1
    };
    let reason = |post: &mut SpeakerPost, input: PostInput| {
        let (outcome, view) = run_speakers(post, &rec.svc, input);
        let SpeakerWindowUse::Diarised { reason } = window_use(&outcome, TrackId::Mic).clone()
        else {
            panic!("reused: {:?}", outcome.speaker_windows);
        };
        (reason, view, outcome.warnings)
    };

    // Another thread count (CAM++ output changes with it).
    let mut post = speaker_processor(oracle(), SEG_MODEL, LEVEL_MODEL, 0.5, Some(4), &counter);
    let (why, view, _) = reason(&mut post, rec.input.clone());
    assert!(why.contains("threads"), "{why}");
    assert_eq!(view, reference);
    // Another segmentation model file.
    let mut post = speaker_processor(oracle(), "level-seg@2", LEVEL_MODEL, 0.5, Some(3), &counter);
    let (why, _, _) = reason(&mut post, rec.input.clone());
    assert!(why.contains("segmentation model level-seg@1"), "{why}");
    // Another embedding model (thresholds registered for it).
    let mut post = speaker_processor(oracle(), SEG_MODEL, "level@2", 0.5, Some(3), &counter);
    let (why, _, _) = reason(&mut post, rec.input.clone());
    assert!(why.contains("embedding model level@1"), "{why}");
    // Another window setting.
    let mut post = speaker_processor(oracle(), SEG_MODEL, LEVEL_MODEL, 0.4, Some(3), &counter);
    let (why, _, _) = reason(&mut post, rec.input.clone());
    assert!(why.contains("other window settings"), "{why}");
    // A recording shorter than the windows (not the audio they were computed from).
    let mut short = rec.input.clone();
    short.tracks[0].audio.samples.truncate(20 * 16_000);
    let mut post = speaker_processor(oracle(), SEG_MODEL, LEVEL_MODEL, 0.5, Some(3), &counter);
    let (why, _, _) = reason(&mut post, short);
    assert!(why.contains("more audio than the recording"), "{why}");

    // A damaged record and records this build cannot read: a warning and the whole pass,
    // never an error.
    let mut other_version = stored.clone();
    other_version[0] = 2;
    for (bytes, expect) in [
        (stored[..stored.len() - 5].to_vec(), "windows"),
        (other_version, "version 2"),
        (b"junk".to_vec(), "version"),
        (Vec::new(), "truncated"),
    ] {
        {
            let mut store = rec.svc.store();
            crate::speakers::store_raw(&mut store, rec.outcome.meeting, TrackId::Mic, &bytes)
                .unwrap();
        }
        let (input, _) =
            load_post_input(&rec.svc, rec.outcome.meeting, &rec.outcome.final_pass).unwrap();
        let mut post = speaker_processor(oracle(), SEG_MODEL, LEVEL_MODEL, 0.5, Some(3), &counter);
        let (why, view, warnings) = reason(&mut post, input);
        assert!(
            why.contains("cannot be read") && why.contains(expect),
            "{why}"
        );
        assert!(
            warnings.iter().any(|w| matches!(
                w,
                PostWarning::SpeakerRecordUnreadable { track: TrackId::Mic, reason }
                    if reason.contains(expect)
            )),
            "{warnings:?}"
        );
        assert_eq!(view, reference);
    }
}

#[test]
fn post_speaker_windows_stopped_by_a_failing_model_keep_the_windows_before() {
    // The live segmenter fails in the second batch: speaker work stops, the record keeps
    // the first batch's windows and says why; the final pass finishes from them.
    let (rec, audio, w) = record_with_speakers(None, true);
    let record = speaker_rec(&rec);
    let why = record.header.stopped.clone().expect("the record says why");
    assert!(why.contains("cpu lost"), "{why}");
    let done = record.results.windows();
    assert_eq!(done, 32);

    let oracle = || Oracle::new(vec![(audio.clone(), w.clone())]);
    let counter = Arc::new(AtomicUsize::new(0));
    let mut reuse = speaker_processor(oracle(), SEG_MODEL, LEVEL_MODEL, 0.5, None, &counter);
    let (outcome, reused) = run_speakers(&mut reuse, &rec.svc, rec.input.clone());
    assert_eq!(
        *window_use(&outcome, TrackId::Mic),
        SpeakerWindowUse::Reused {
            windows: done,
            stopped: Some(why)
        }
    );
    let mut again = speaker_processor(oracle(), SEG_MODEL, LEVEL_MODEL, 0.5, None, &counter);
    let (whole_outcome, whole_pass) =
        run_speakers(&mut again, &rec.svc, whole_speakers(&rec.input));
    assert_eq!(reused, whole_pass);
    assert_eq!(outcome.speakers, whole_outcome.speakers);
}

/// Whether `needle` occurs in the database file or its WAL.
fn on_disk(db: &std::path::Path, needle: &[u8]) -> bool {
    ["", "-wal", "-journal"].iter().any(|suffix| {
        std::fs::read(format!("{}{suffix}", db.display()))
            .is_ok_and(|bytes| bytes.windows(needle.len()).any(|w| w == needle))
    })
}

/// What identifies a meeting's live records on disk: a known embedding's bytes and the
/// two keys.
fn record_needles(rec: &Recorded) -> Vec<Vec<u8>> {
    let record = speaker_rec(rec);
    let embedding: Vec<u8> = record
        .results
        .embeddings()
        .next()
        .expect("the live windows hold embeddings")
        .iter()
        .flat_map(|x| x.to_le_bytes())
        .collect();
    let m = rec.outcome.meeting;
    vec![
        embedding,
        crate::speakers::record_key(m, TrackId::Mic)
            .as_str()
            .as_bytes()
            .to_vec(),
        crate::stretches::record_key(m, TrackId::Mic)
            .as_str()
            .as_bytes()
            .to_vec(),
    ]
}

fn records_exist(rec: &Recorded) -> (bool, bool) {
    let store = rec.svc.store();
    (
        crate::speakers::speaker_record(&store, rec.outcome.meeting, TrackId::Mic)
            .unwrap()
            .is_some(),
        stretch_record(&store, rec.outcome.meeting, TrackId::Mic)
            .unwrap()
            .is_some(),
    )
}

#[test]
fn post_final_pass_erases_the_live_records_from_disk() {
    // The live speaker windows are biometric data and the stretches hold the raw
    // transcript: transient, deleted by the final pass whatever it reused.
    let (rec, audio, w) = record_with_speakers(Some(3), false);
    let db = rec.svc.data.db_path();
    let needles = record_needles(&rec);
    assert_eq!(records_exist(&rec), (true, true));
    rec.svc.store().checkpoint().unwrap();
    for n in &needles {
        assert!(on_disk(&db, n), "sanity: on disk before the final pass");
    }
    let oracle = Oracle::new(vec![(audio, w)]);
    let counter = Arc::new(AtomicUsize::new(0));
    let mut post = speaker_processor(oracle, SEG_MODEL, LEVEL_MODEL, 0.5, Some(3), &counter);
    let (outcome, _) = run_speakers(&mut post, &rec.svc, rec.input.clone());
    assert!(matches!(
        window_use(&outcome, TrackId::Mic),
        SpeakerWindowUse::Reused { .. }
    ));
    assert_eq!(records_exist(&rec), (false, false));
    for n in &needles {
        assert!(!on_disk(&db, n), "a live record survived the final pass");
    }
    // Still gone after the store closes with another connection open (so SQLite's own
    // last-connection checkpoint cannot hide anything).
    let other = earshot_store::Store::open(&db).unwrap();
    let Recorded { svc, _dir, .. } = rec;
    drop(svc);
    drop(post);
    for n in &needles {
        assert!(!on_disk(&db, n), "a live record outlived the session");
    }
    drop(other);
    drop(_dir);
}

#[test]
fn post_final_pass_erases_live_records_it_did_not_reuse() {
    // Fallback (another thread count) and an unreadable record: deleted too.
    let (rec, audio, w) = record_with_speakers(Some(3), false);
    let oracle = || Oracle::new(vec![(audio.clone(), w.clone())]);
    let counter = Arc::new(AtomicUsize::new(0));
    let mut post = speaker_processor(oracle(), SEG_MODEL, LEVEL_MODEL, 0.5, Some(4), &counter);
    let (outcome, _) = run_speakers(&mut post, &rec.svc, rec.input.clone());
    assert!(matches!(
        window_use(&outcome, TrackId::Mic),
        SpeakerWindowUse::Diarised { .. }
    ));
    assert_eq!(records_exist(&rec), (false, false));

    {
        let mut store = rec.svc.store();
        crate::speakers::store_raw(&mut store, rec.outcome.meeting, TrackId::Mic, b"junk").unwrap();
    }
    let (input, _) =
        load_post_input(&rec.svc, rec.outcome.meeting, &rec.outcome.final_pass).unwrap();
    let mut post = speaker_processor(oracle(), SEG_MODEL, LEVEL_MODEL, 0.5, Some(3), &counter);
    let (outcome, _) = run_speakers(&mut post, &rec.svc, input);
    assert!(
        outcome
            .warnings
            .iter()
            .any(|w| matches!(w, PostWarning::SpeakerRecordUnreadable { .. }))
    );
    assert_eq!(records_exist(&rec), (false, false));
}

#[test]
fn post_whole_pass_requested_ignores_the_live_work_and_still_erases_it() {
    // `PostConfig::reuse_live = false` (the CLI's `--whole-pass`, T6.13's quality check):
    // the same input with both live records is transcribed and diarised whole, says why,
    // gives the same labelled transcript and speakers as the reuse, and deletes the records.
    let (rec, audio, w) = record_with_speakers(Some(3), false);
    assert_eq!(records_exist(&rec), (true, true));
    let oracle = || Oracle::new(vec![(audio.clone(), w.clone())]);
    let whole_windows = Arc::new(AtomicUsize::new(0));
    let mut whole_post = speaker_processor(
        oracle(),
        SEG_MODEL,
        LEVEL_MODEL,
        0.5,
        Some(3),
        &whole_windows,
    );
    whole_post.config.reuse_live = false;
    let (whole_outcome, whole_pass) = run_speakers(&mut whole_post, &rec.svc, rec.input.clone());
    let why = crate::post::WHOLE_PASS_REQUESTED.to_owned();
    assert_eq!(
        *use_of(&whole_outcome, TrackId::Mic),
        StretchUse::Transcribed {
            reason: why.clone()
        }
    );
    assert_eq!(
        *window_use(&whole_outcome, TrackId::Mic),
        SpeakerWindowUse::Diarised { reason: why }
    );
    assert!(
        !whole_post.transcriber.calls().is_empty(),
        "the track is transcribed again"
    );
    assert_eq!(records_exist(&rec), (false, false), "the records go anyway");

    // The default reuses both records from the same input (held in memory) with the same
    // result.
    let reuse_windows = Arc::new(AtomicUsize::new(0));
    let mut reuse = speaker_processor(
        oracle(),
        SEG_MODEL,
        LEVEL_MODEL,
        0.5,
        Some(3),
        &reuse_windows,
    );
    let (outcome, reused) = run_speakers(&mut reuse, &rec.svc, rec.input.clone());
    assert!(matches!(
        use_of(&outcome, TrackId::Mic),
        StretchUse::Reused { .. }
    ));
    assert!(matches!(
        window_use(&outcome, TrackId::Mic),
        SpeakerWindowUse::Reused { .. }
    ));
    assert!(reuse.transcriber.calls().len() < whole_post.transcriber.calls().len());
    assert!(reuse_windows.load(SeqCst) < whole_windows.load(SeqCst));
    assert_eq!(outcome.speakers, whole_outcome.speakers);
    assert_eq!(reused, whole_pass);
}

#[test]
fn post_failed_or_cancelled_final_pass_keeps_the_live_records() {
    let (rec, audio, w) = record_with_speakers(Some(3), false);
    let oracle = || Oracle::new(vec![(audio.clone(), w.clone())]);
    // Cancelled.
    let counter = Arc::new(AtomicUsize::new(0));
    let mut post = speaker_processor(oracle(), SEG_MODEL, LEVEL_MODEL, 0.5, Some(3), &counter);
    let cancel = CancelToken::new();
    cancel.cancel();
    assert!(
        post.run(
            &rec.svc,
            rec.input.clone(),
            &mut ChaCha8Rng::seed_from_u64(3),
            &cancel
        )
        .is_err()
    );
    assert_eq!(records_exist(&rec), (true, true));
    // A speaker model that fails during the final pass.
    let failed_after_many = Arc::new(AtomicUsize::new(1_000));
    let (seg, emb) = speaker_models(SEG_MODEL, LEVEL_MODEL, &counter);
    let id = EmbeddingModelId::new(LEVEL_MODEL);
    let cfg = PostConfig {
        clustering_thresholds: ModelThresholds::new().with(id.clone(), 0.704_565_5),
        voiceprint_thresholds: ModelThresholds::new().with(id, 0.5),
        ..PostConfig::default()
    };
    let failing: BoxEmb = Box::new(FailingAfter {
        inner: emb,
        windows: failed_after_many,
    });
    let mut post = PostProcessor::new(oracle(), seg, failing, cfg)
        .unwrap()
        .with_pieces(Box::new(RmsVad), pieces())
        .unwrap()
        .with_speaker_threads(Some(3));
    assert!(
        post.run(
            &rec.svc,
            rec.input.clone(),
            &mut ChaCha8Rng::seed_from_u64(4),
            &CancelToken::new()
        )
        .is_err()
    );
    assert_eq!(records_exist(&rec), (true, true));
}

// ---------------------------------------------------------------- production cadence

/// Delivers one 10 ms packet per `interval` of wall time (back to back, never held), as a
/// capture device does at real time, here 10 times faster; stops (as a user hanging up
/// does) once the live speaker progress reaches `stop_at` windows, setting `stopped_early`
/// if packets were still coming; records the live speaker progress when the audio ends.
struct PacedSource<S> {
    inner: S,
    interval: Duration,
    last: Option<std::time::Instant>,
    progress: Arc<AtomicUsize>,
    progress_at_end: Arc<AtomicUsize>,
    stop_at: usize,
    stopped_early: Arc<AtomicBool>,
    ended: bool,
}

impl<S: AudioSource> AudioSource for PacedSource<S> {
    type Error = S::Error;

    fn session(&self) -> &CaptureSession {
        self.inner.session()
    }

    fn pump(&mut self, sink: &mut dyn CaptureSink) -> Result<SourceStatus, Self::Error> {
        if self.last.is_some_and(|t| t.elapsed() < self.interval) {
            return Ok(SourceStatus::Idle);
        }
        self.last = Some(std::time::Instant::now());
        if !self.ended
            && !self.stopped_early.load(SeqCst)
            && self.progress.load(SeqCst) >= self.stop_at
        {
            self.stopped_early.store(true, SeqCst);
            self.inner.stop();
        }
        let status = self.inner.pump(sink)?;
        if status == SourceStatus::Ended && !self.ended {
            self.ended = true;
            self.progress_at_end
                .store(self.progress.load(SeqCst), SeqCst);
        }
        Ok(status)
    }

    fn stop(&mut self) {
        self.inner.stop();
    }
}

/// Speaker models that take `per_window` per segmented window and `per_region` per
/// embedded region, checking the cancel token before each (as the ONNX models do per
/// sub-batch and per region).
struct SlowSegmenter {
    inner: BoxSeg,
    per_window: Duration,
}

impl earshot_core::diar::SpeakerSegmenter for SlowSegmenter {
    fn info(&self) -> &SegmenterInfo {
        self.inner.info()
    }
    fn segment(
        &mut self,
        windows: &[&[f32]],
        cancel: &CancelToken,
    ) -> Result<Vec<earshot_core::diar::LocalActivity>, earshot_core::diar::DiarError> {
        for _ in windows {
            if cancel.is_cancelled() {
                return Err(earshot_core::diar::DiarError::Cancelled);
            }
            std::thread::sleep(self.per_window);
        }
        self.inner.segment(windows, cancel)
    }
}

struct SlowEmbedder {
    inner: BoxEmb,
    per_region: Duration,
}

impl earshot_core::diar::SpeakerEmbedder for SlowEmbedder {
    fn info(&self) -> &EmbedderInfo {
        self.inner.info()
    }
    fn embed(
        &mut self,
        speech: &[&[f32]],
        cancel: &CancelToken,
    ) -> Result<Vec<earshot_core::diar::Embedding>, earshot_core::diar::DiarError> {
        for _ in speech {
            if cancel.is_cancelled() {
                return Err(earshot_core::diar::DiarError::Cancelled);
            }
            std::thread::sleep(self.per_region);
        }
        self.inner.embed(speech, cancel)
    }
}

/// A live factory with [`speaker_models`] wrapped by `seg` and `emb`, live speaker progress
/// into `done`.
fn with_speaker_segmenter(
    audio: &[f32],
    w: &[Word],
    seg: impl Fn(BoxSeg) -> BoxSeg + Send + 'static,
    emb: impl Fn(BoxEmb) -> BoxEmb + Send + 'static,
    windows: &Arc<AtomicUsize>,
    done: &Arc<AtomicUsize>,
) -> LiveAsrFactory {
    let mut inner = oracle_asr(Some(Oracle::new(vec![(audio.to_vec(), w.to_vec())])), None);
    let (windows, done) = (Arc::clone(windows), Arc::clone(done));
    Box::new(move |track, entity| {
        let Some(mut asr) = inner(track, entity)? else {
            return Ok(None);
        };
        let (s, e) = speaker_models(SEG_MODEL, LEVEL_MODEL, &windows);
        let (s, e) = (seg(s), emb(e));
        asr.speakers = Some(crate::live::LiveSpeakers {
            models: Box::new(move || {
                Ok(earshot_core::diar::Diarizer::new(
                    s,
                    e,
                    earshot_core::diar::DiarizationConfig::default(),
                )
                .unwrap())
            }),
            threads: None,
            progress: Some(Arc::clone(&done)),
            background: false,
        });
        Ok(Some(asr))
    })
}

#[test]
fn post_speaker_windows_are_computed_while_packets_keep_arriving() {
    // Production cadence: 10 ms packets back to back (here one per millisecond, ten times
    // real time; never held), speaker models slower per window than a packet interval (2 ms
    // per window, 1 ms per region). Batches finish while audio flows: the source stops
    // (hangs up) once the first batch is done, which must come before its audio runs out
    // (the meeting is padded with 120 s of silence so that a loaded host, where the
    // below-normal speaker thread gets less time, still has room).
    let (mut audio, w) = two_voices();
    audio.extend(std::iter::repeat_n(0.0, 120 * 16_000));
    let windows = Arc::new(AtomicUsize::new(0));
    let done = Arc::new(AtomicUsize::new(0));
    let factory = with_speaker_segmenter(
        &audio,
        &w,
        |inner| {
            Box::new(SlowSegmenter {
                inner,
                per_window: Duration::from_millis(2),
            })
        },
        |inner| {
            Box::new(SlowEmbedder {
                inner,
                per_region: Duration::from_millis(1),
            })
        },
        &windows,
        &done,
    );
    let at_end = Arc::new(AtomicUsize::new(0));
    let stopped_early = Arc::new(AtomicBool::new(false));
    let source = PacedSource {
        inner: ReplayBuilder::new(ORIGIN, MonoBuffer::new(PIPELINE_RATE, audio.clone()))
            .packet_duration(Duration::from_millis(10))
            .build()
            .unwrap(),
        interval: Duration::from_millis(1),
        last: None,
        progress: Arc::clone(&done),
        progress_at_end: Arc::clone(&at_end),
        stop_at: 32,
        stopped_early: Arc::clone(&stopped_early),
        ended: false,
    };
    let dir = TempDir::new();
    let svc = services(&dir);
    let session = start_live(
        source,
        svc.clone(),
        live_config(LiveTranscription::Stretches(pieces())),
        Some(factory),
        Box::new(ChaCha8Rng::seed_from_u64(85)),
    )
    .unwrap();
    let outcome = session.wait().unwrap();
    let ended_with = at_end.load(SeqCst);
    assert!(
        stopped_early.load(SeqCst),
        "speaker windows finished before the audio ended: {ended_with} (segmenter given {})",
        windows.load(SeqCst)
    );
    assert!(ended_with >= 32, "{ended_with}");
    // The record has them and the final pass finishes from them.
    let store = svc.store();
    let Some(crate::speakers::StoredSpeakers::Record(record)) =
        crate::speakers::speaker_record(&store, outcome.meeting, TrackId::Mic).unwrap()
    else {
        panic!("no speaker record");
    };
    assert_eq!(record.header.stopped, None);
    assert!(record.results.windows() >= ended_with);
}

/// A segmenter that signals `entered` and then holds, whatever its cancel token says,
/// until `release` is set (at most 600 s: the test waits for every live final first,
/// however slowly a loaded host decodes).
struct StuckSegmenter {
    inner: BoxSeg,
    entered: Arc<AtomicBool>,
    release: Arc<AtomicBool>,
}

impl earshot_core::diar::SpeakerSegmenter for StuckSegmenter {
    fn info(&self) -> &SegmenterInfo {
        self.inner.info()
    }
    fn segment(
        &mut self,
        windows: &[&[f32]],
        cancel: &CancelToken,
    ) -> Result<Vec<earshot_core::diar::LocalActivity>, earshot_core::diar::DiarError> {
        self.entered.store(true, SeqCst);
        let start = std::time::Instant::now();
        while !self.release.load(SeqCst) && start.elapsed() < Duration::from_secs(600) {
            std::thread::sleep(Duration::from_millis(1));
        }
        if cancel.is_cancelled() {
            return Err(earshot_core::diar::DiarError::Cancelled);
        }
        self.inner.segment(windows, cancel)
    }
}

#[test]
fn post_live_transcript_finished_comes_before_the_wait_for_speaker_work() {
    // T7.4c measures the live transcript's catch-up to this event: the first speaker batch
    // stays blocked until the test has seen it, so the bounded wait for the speaker thread
    // (600 s here) is not part of it.
    let (audio, w) = two_voices();
    let windows = Arc::new(AtomicUsize::new(0));
    let done = Arc::new(AtomicUsize::new(0));
    let entered = Arc::new(AtomicBool::new(false));
    let release = Arc::new(AtomicBool::new(false));
    let factory = {
        let (entered, release) = (Arc::clone(&entered), Arc::clone(&release));
        with_speaker_segmenter(
            &audio,
            &w,
            move |inner| {
                Box::new(StuckSegmenter {
                    inner,
                    entered: Arc::clone(&entered),
                    release: Arc::clone(&release),
                })
            },
            |e| e,
            &windows,
            &done,
        )
    };
    let dir = TempDir::new();
    let svc = services(&dir);
    let source = HoldSource {
        inner: mic_only(&audio),
        hold_at: 50 * 16_000,
        delivered: Arc::default(),
        release: {
            let entered = Arc::clone(&entered);
            Box::new(move || entered.load(SeqCst))
        },
        since: None,
        released: false,
    };
    let mut session = start_live(
        source,
        svc.clone(),
        LiveConfig {
            speaker_stop_wait: Duration::from_secs(600),
            ..live_config(LiveTranscription::Stretches(pieces()))
        },
        Some(factory),
        Box::new(ChaCha8Rng::seed_from_u64(87)),
    )
    .unwrap();
    let events = session.take_events().unwrap();
    let started = std::time::Instant::now();
    let mut finished = false;
    while !finished && started.elapsed() < Duration::from_secs(600) {
        finished = matches!(
            events.recv_timeout(Duration::from_millis(100)),
            Ok(crate::live::RuntimeEvent::LiveTranscriptFinished { .. })
        );
    }
    let batches_done = done.load(SeqCst);
    let was_entered = entered.load(SeqCst);
    release.store(true, SeqCst);
    session.wait().unwrap();
    assert!(
        finished,
        "no LiveTranscriptFinished before the speaker thread ended"
    );
    assert!(was_entered, "the speaker batch was blocked");
    assert_eq!(
        batches_done, 0,
        "no speaker batch finished before the event"
    );
}

#[test]
fn post_live_transcription_never_waits_for_speaker_work() {
    // The first speaker batch blocks until released, and the end waits for it. The source
    // holds the audio after 50 s until that batch has started, so the rest of the meeting
    // is delivered while it is blocked: every live final, also of the speech after 50 s and
    // of the last stretch decoded at the end, arrives while it is blocked.
    let (audio, w) = two_voices();
    let windows = Arc::new(AtomicUsize::new(0));
    let done = Arc::new(AtomicUsize::new(0));
    let entered = Arc::new(AtomicBool::new(false));
    let release = Arc::new(AtomicBool::new(false));
    let factory = {
        let (entered, release) = (Arc::clone(&entered), Arc::clone(&release));
        with_speaker_segmenter(
            &audio,
            &w,
            move |inner| {
                Box::new(StuckSegmenter {
                    inner,
                    entered: Arc::clone(&entered),
                    release: Arc::clone(&release),
                })
            },
            |e| e,
            &windows,
            &done,
        )
    };
    let dir = TempDir::new();
    let svc = services(&dir);
    let source = HoldSource {
        inner: mic_only(&audio),
        hold_at: 50 * 16_000,
        delivered: Arc::default(),
        release: {
            let entered = Arc::clone(&entered);
            Box::new(move || entered.load(SeqCst))
        },
        since: None,
        released: false,
    };
    let mut session = start_live(
        source,
        svc.clone(),
        LiveConfig {
            speaker_stop_wait: Duration::from_secs(600),
            ..live_config(LiveTranscription::Stretches(pieces()))
        },
        Some(factory),
        Box::new(ChaCha8Rng::seed_from_u64(86)),
    )
    .unwrap();
    let events = session.take_events().unwrap();
    let mut seen: Vec<(String, MeetingTime)> = Vec::new();
    let started = std::time::Instant::now();
    while !(entered.load(SeqCst) && seen.len() >= w.len())
        && started.elapsed() < Duration::from_secs(600)
    {
        if let Ok(crate::live::RuntimeEvent::Transcript {
            event: earshot_core::asr::TranscriptEvent::Final(seg),
            ..
        }) = events.recv_timeout(Duration::from_millis(100))
        {
            seen.extend(seg.words.iter().map(|x| (x.text.clone(), x.start)));
        }
    }
    let blocked = entered.load(SeqCst) && !release.load(SeqCst);
    let windows_while_blocked = done.load(SeqCst);
    release.store(true, SeqCst);
    let outcome = session.wait().unwrap();
    assert!(blocked, "the speaker batch was blocked");
    assert_eq!(windows_while_blocked, 0, "no speaker batch finished");
    let texts: Vec<String> = w.iter().map(|x| x.text.clone()).collect();
    assert_eq!(
        seen.iter().map(|(t, _)| t.clone()).collect::<Vec<_>>(),
        texts,
        "every live final arrived while speaker work was blocked"
    );
    assert!(
        seen.iter()
            .any(|(_, start)| *start > MeetingTime::from_millis(50_000)),
        "speech delivered after the batch blocked was transcribed"
    );
    // The record exists (whether the released batch finished depends on whether the end
    // had already stopped speaker work; the speaker thread tests pin both cases).
    let store = svc.store();
    let Some(crate::speakers::StoredSpeakers::Record(record)) =
        crate::speakers::speaker_record(&store, outcome.meeting, TrackId::Mic).unwrap()
    else {
        panic!("no speaker record");
    };
    assert_eq!(record.header.stopped, None);
    assert!(record.results.windows().is_multiple_of(32));
}

// ---------------------------------------------------------------- speaker detection timing

/// A power supply the test switches (T6.13d4).
#[derive(Clone)]
struct FakePower(Arc<Mutex<crate::power::PowerSupply>>);

impl FakePower {
    fn new(supply: crate::power::PowerSupply) -> Self {
        Self(Arc::new(Mutex::new(supply)))
    }

    fn set(&self, supply: crate::power::PowerSupply) {
        *self.0.lock().unwrap() = supply;
    }
}

impl crate::power::PowerSource for FakePower {
    fn supply(&self) -> crate::power::PowerSupply {
        *self.0.lock().unwrap()
    }
}

/// [`record`] with `cfg`, keeping the session's events.
fn record_events<S>(
    source: S,
    cfg: LiveConfig,
    factory: LiveAsrFactory,
) -> (Recorded, Vec<crate::live::RuntimeEvent>)
where
    S: AudioSource + Send + 'static,
    S::Error: std::fmt::Debug,
{
    let dir = TempDir::new();
    let svc = services(&dir);
    let mut session = start_live(
        source,
        svc.clone(),
        cfg,
        Some(factory),
        Box::new(ChaCha8Rng::seed_from_u64(81)),
    )
    .unwrap();
    let events = session.take_events().unwrap();
    let outcome = session.wait().unwrap();
    let events: Vec<_> = events.try_iter().collect();
    let (input, missing) = load_post_input(&svc, outcome.meeting, &outcome.final_pass).unwrap();
    assert!(missing.is_empty());
    (
        Recorded {
            svc,
            _dir: dir,
            outcome,
            input,
        },
        events,
    )
}

/// `inner`, counting the loads of its speaker models into `loads`.
fn counting_loads(mut inner: LiveAsrFactory, loads: Arc<AtomicUsize>) -> LiveAsrFactory {
    Box::new(move |track, entity| {
        let Some(mut asr) = inner(track, entity)? else {
            return Ok(None);
        };
        if let Some(s) = asr.speakers.as_mut() {
            let load = std::mem::replace(
                &mut s.models,
                Box::new(|| Err(crate::RuntimeError::Internal("replaced"))),
            );
            let loads = Arc::clone(&loads);
            s.models = Box::new(move || {
                loads.fetch_add(1, SeqCst);
                load()
            });
        }
        Ok(Some(asr))
    })
}

/// What a timed recording left: the recording and its events, the meeting's audio and
/// words, how many windows the live segmenter was given and how often speaker models were
/// loaded.
struct Timed {
    rec: Recorded,
    events: Vec<crate::live::RuntimeEvent>,
    audio: Vec<f32>,
    words: Vec<Word>,
    live_windows: usize,
    loads: usize,
}

/// The two-voice meeting with live speaker models under `timing` on `power`. With `hold`,
/// the source holds the audio after 50 s until the speaker thread finished a batch (as
/// [`record_with_speakers`]), then runs `at_release` (for example unplugging) and releases.
fn record_timed(
    timing: crate::speaker_timing::SpeakerTiming,
    power: &FakePower,
    hold: bool,
    at_release: impl Fn() + Send + 'static,
) -> Timed {
    let (audio, w) = two_voices();
    let windows = Arc::new(AtomicUsize::new(0));
    let done = Arc::new(AtomicUsize::new(0));
    let loads = Arc::new(AtomicUsize::new(0));
    let factory = counting_loads(
        with_speakers(
            oracle_asr(Some(Oracle::new(vec![(audio.clone(), w.clone())])), None),
            None,
            Arc::clone(&windows),
            Arc::clone(&done),
            None,
        ),
        Arc::clone(&loads),
    );
    let cfg = LiveConfig {
        speaker_timing: timing,
        power: crate::power::PowerProbe::new(power.clone()),
        // Every loop of the session thread reads the supply.
        power_poll: Duration::ZERO,
        ..live_config(LiveTranscription::Stretches(pieces()))
    };
    let (rec, events) = if hold {
        let source = HoldSource {
            inner: mic_only(&audio),
            hold_at: 50 * 16_000,
            delivered: Arc::default(),
            release: {
                let done = Arc::clone(&done);
                Box::new(move || {
                    let ready = done.load(SeqCst) >= 32;
                    if ready {
                        at_release();
                    }
                    ready
                })
            },
            since: None,
            released: false,
        };
        record_events(source, cfg, factory)
    } else {
        record_events(mic_only(&audio), cfg, factory)
    };
    Timed {
        rec,
        events,
        audio,
        words: w,
        live_windows: windows.load(SeqCst),
        loads: loads.load(SeqCst),
    }
}

/// The reason of the mic's reason-only speaker record.
fn off_record(rec: &Recorded) -> String {
    let store = rec.svc.store();
    match crate::speakers::speaker_record(&store, rec.outcome.meeting, TrackId::Mic).unwrap() {
        Some(crate::speakers::StoredSpeakers::Off(why)) => why,
        other => panic!("not a reason-only record: {other:?}"),
    }
}

fn speakers_off(events: &[crate::live::RuntimeEvent]) -> Vec<String> {
    events
        .iter()
        .filter_map(|e| match e {
            crate::live::RuntimeEvent::LiveSpeakersOff { reason } => Some(reason.clone()),
            _ => None,
        })
        .collect()
}

/// The final pass from the stored records and the whole-meeting pass give the same
/// speakers and transcript; returns how the stored windows were used.
fn final_pass_matches_whole(t: &Timed) -> SpeakerWindowUse {
    let oracle = || Oracle::new(vec![(t.audio.clone(), t.words.clone())]);
    let counter = Arc::new(AtomicUsize::new(0));
    let mut whole_post = speaker_processor(oracle(), SEG_MODEL, LEVEL_MODEL, 0.5, None, &counter);
    let (whole_outcome, whole_pass) =
        run_speakers(&mut whole_post, &t.rec.svc, whole_speakers(&t.rec.input));
    let mut post = speaker_processor(oracle(), SEG_MODEL, LEVEL_MODEL, 0.5, None, &counter);
    let (outcome, view) = run_speakers(&mut post, &t.rec.svc, t.rec.input.clone());
    assert_eq!(outcome.speakers.len(), 2, "{:?}", speakers_of(&outcome));
    assert_eq!(outcome.speakers, whole_outcome.speakers);
    assert_eq!(view, whole_pass);
    window_use(&outcome, TrackId::Mic).clone()
}

#[test]
fn post_speaker_timing_after_the_call_runs_no_live_speaker_work() {
    use crate::speaker_timing::{AFTER_CALL_REASON, SpeakerTiming};
    // Whatever the power supply: no speaker work during the meeting, the record says why,
    // and the final pass diarises the whole track with the same result.
    for supply in [
        crate::power::PowerSupply::Mains,
        crate::power::PowerSupply::NoBattery,
    ] {
        let t = record_timed(
            SpeakerTiming::AfterCall,
            &FakePower::new(supply),
            false,
            || {},
        );
        assert_eq!(t.live_windows, 0, "no live speaker work on {supply:?}");
        assert_eq!(t.loads, 0, "no speaker model loaded");
        assert_eq!(speakers_off(&t.events), vec![AFTER_CALL_REASON.to_owned()]);
        assert_eq!(off_record(&t.rec), AFTER_CALL_REASON);
        assert_eq!(
            final_pass_matches_whole(&t),
            SpeakerWindowUse::Diarised {
                reason: AFTER_CALL_REASON.to_owned()
            }
        );
        assert!(!records_exist(&t.rec).0, "the final pass deleted it");
    }
}

#[test]
fn post_speaker_timing_on_battery_runs_no_live_speaker_work_and_records_why() {
    use crate::speaker_timing::{ON_BATTERY_REASON, SpeakerTiming};
    let t = record_timed(
        SpeakerTiming::AfterCallOnBattery,
        &FakePower::new(crate::power::PowerSupply::Battery),
        false,
        || {},
    );
    assert_eq!(t.live_windows, 0);
    assert_eq!(t.loads, 0, "no speaker model loaded");
    assert_eq!(speakers_off(&t.events), vec![ON_BATTERY_REASON.to_owned()]);
    assert_eq!(off_record(&t.rec), ON_BATTERY_REASON);
    assert_eq!(
        final_pass_matches_whole(&t),
        SpeakerWindowUse::Diarised {
            reason: ON_BATTERY_REASON.to_owned()
        }
    );
}

#[test]
fn post_speaker_timing_during_the_call_and_battery_mode_on_mains_or_a_desktop_run_live() {
    use crate::power::PowerSupply::{Battery, Mains, NoBattery};
    use crate::speaker_timing::SpeakerTiming;
    // "During the call" ignores the battery; the battery mode on mains power, and on a
    // desktop (no battery), runs speaker work during the call as by default.
    for (timing, supply) in [
        (SpeakerTiming::DuringCall, Battery),
        (SpeakerTiming::AfterCallOnBattery, Mains),
        (SpeakerTiming::AfterCallOnBattery, NoBattery),
    ] {
        let t = record_timed(timing, &FakePower::new(supply), true, || {});
        assert!(t.live_windows >= 32, "{timing:?} on {supply:?}");
        assert_eq!(t.loads, 1);
        assert!(
            speakers_off(&t.events).is_empty(),
            "{timing:?} on {supply:?}"
        );
        let record = speaker_rec(&t.rec);
        let done = record.results.windows();
        assert!(done >= 32 && done.is_multiple_of(32), "{done} windows");
        assert_eq!(record.header.stopped, None);
        assert_eq!(
            final_pass_matches_whole(&t),
            SpeakerWindowUse::Reused {
                windows: done,
                stopped: None
            }
        );
    }
}

#[test]
fn post_speaker_timing_unplugged_mid_meeting_stops_with_the_prefix_kept() {
    use crate::power::PowerSupply::{Battery, Mains};
    use crate::speaker_timing::{SpeakerTiming, UNPLUGGED_REASON};
    let power = FakePower::new(Mains);
    let unplug = power.clone();
    // On mains for the first batch; unplugged at 50 s, before the second batch's audio
    // (up to 73 s) has arrived.
    let t = record_timed(SpeakerTiming::AfterCallOnBattery, &power, true, move || {
        unplug.set(Battery);
    });
    assert_eq!(speakers_off(&t.events), vec![UNPLUGGED_REASON.to_owned()]);
    let record = speaker_rec(&t.rec);
    assert_eq!(record.results.windows(), 32, "the finished batch is kept");
    assert_eq!(record.header.stopped.as_deref(), Some(UNPLUGGED_REASON));
    // No batch was started after the unplug.
    assert_eq!(t.live_windows, 32);
    assert_eq!(
        final_pass_matches_whole(&t),
        SpeakerWindowUse::Reused {
            windows: 32,
            stopped: Some(UNPLUGGED_REASON.to_owned())
        }
    );
}

#[test]
fn post_speaker_timing_desktop_is_never_watched_for_a_battery() {
    use crate::power::PowerSupply::{Battery, NoBattery};
    use crate::speaker_timing::SpeakerTiming;
    // A machine without a battery at the start is not watched: a (bogus) battery reading
    // later changes nothing.
    let power = FakePower::new(NoBattery);
    let flip = power.clone();
    let t = record_timed(SpeakerTiming::AfterCallOnBattery, &power, true, move || {
        flip.set(Battery);
    });
    assert!(speakers_off(&t.events).is_empty());
    let record = speaker_rec(&t.rec);
    assert!(record.results.windows() >= 32);
    assert_eq!(record.header.stopped, None);
}

#[test]
fn post_stretch_record_from_before_uncovered_speech_recovery_is_refused_by_reusable() {
    // T2.5d: a version 2 record stored before `PieceConfig::recover_speech` existed still
    // reads (`crate::stretches` tests), and the final pass's own check refuses to reuse it
    // under today's piece settings; the same record with today's settings is reused.
    let dir = TempDir::new();
    let svc = services(&dir);
    let (_, mic, _) = super::tests::meeting_rows(&svc);
    let mut t = super::tests::track_input(&mic, MonoBuffer::new(PIPELINE_RATE, vec![0.0; 48_000]));
    let pieces = Pieces {
        vad: Box::new(RmsVad),
        config: PieceConfig::default(),
    };
    let record = StretchRecord {
        backend: BackendInfo {
            kind: BackendKind::Fake,
            device: "fake".into(),
            model: "fake".into(),
        },
        options: t.options.clone(),
        pieces: PieceConfig::default(),
        vad: RmsVad.id(),
        vad_frame: RmsVad.frame_len(),
        decoded: earshot_core::asr::DecodedPieces {
            pieces: Vec::new(),
            through: 0,
        },
        stopped: None,
    };
    t.stretches = Some(StoredStretches::Record(Box::new(record.clone())));
    assert!(reusable(&t, "fake", Some(&pieces), true).is_ok());

    let mut json = serde_json::to_value(&record).unwrap();
    json["pieces"]
        .as_object_mut()
        .unwrap()
        .remove("recover_speech")
        .unwrap();
    let mut bytes = crate::stretches::STRETCH_RECORD_VERSION
        .to_le_bytes()
        .to_vec();
    bytes.extend_from_slice(&serde_json::to_vec(&json).unwrap());
    let old = crate::stretches::decode(&bytes);
    assert!(matches!(old, StoredStretches::Record(_)), "{old:?}");
    t.stretches = Some(old);
    let why = reusable(&t, "fake", Some(&pieces), true).unwrap_err();
    assert_eq!(why, "the stretches used other piece settings");
}
