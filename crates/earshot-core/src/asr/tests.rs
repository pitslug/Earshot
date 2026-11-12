//! Unit tests for ASR types, VAD, LocalAgreement, the chunker and the fake. Every test
//! path contains `asr`, so `cargo test -p earshot-core asr` selects them all.

use std::collections::HashMap;
use std::time::Duration;

use super::*;
use crate::audio::{MonoBuffer, PIPELINE_RATE, SampleRate};
use crate::identity::{EntityId, Syncable};

// ---------------------------------------------------------------- helpers

/// xorshift64* noise in [-1, 1): every sample distinct, so the timeline fake can locate
/// any buffer by exact match.
struct Noise(u64);

impl Noise {
    fn next(&mut self) -> f32 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        let x = self.0.wrapping_mul(0x2545_F491_4F6C_DD1D);
        ((x >> 40) as f32 / (1u64 << 24) as f32) * 2.0 - 1.0
    }
}

const SPEECH_AMP: f32 = 0.3; // about -15 dBFS RMS
const QUIET_AMP: f32 = 1e-4; // about -85 dBFS RMS

fn ms_to_frames(ms: u64) -> usize {
    (ms * 16) as usize
}

/// `n` words starting at `start_ms`, each 300 ms long, one every 400 ms.
fn words_at(prefix: &str, start_ms: u64, n: usize) -> Vec<Word> {
    (0..n)
        .map(|i| {
            let s = start_ms + 400 * i as u64;
            Word::new(format!("{prefix}{i}"), s, s + 300, 0.9)
        })
        .collect()
}

/// Audio of `total_ms`: loud noise inside every word span, quiet noise elsewhere.
fn audio_for(words: &[Word], total_ms: u64, seed: u64) -> Vec<f32> {
    let mut noise = Noise(seed);
    let n = ms_to_frames(total_ms);
    let mut loud = vec![false; n];
    for w in words {
        let a = ms_to_frames(w.start.as_millis()).min(n);
        let b = ms_to_frames(w.end.as_millis()).min(n);
        loud[a..b].iter_mut().for_each(|l| *l = true);
    }
    loud.iter()
        .map(|&l| noise.next() * if l { SPEECH_AMP } else { QUIET_AMP })
        .collect()
}

fn seq_ids() -> SegmentIds {
    let mut n: u128 = 0;
    Box::new(move || {
        n += 1;
        EntityId::from_bytes((0xA5A5_0000_0000_0000_0000_0000_0000_0000 | n).to_be_bytes())
    })
}

fn track() -> EntityId {
    EntityId::from_bytes([7; 16])
}

fn chunker(
    fake: FakeTranscriber,
    config: ChunkerConfig,
    origin: MeetingTime,
) -> StreamingChunker<FakeTranscriber, EnergyVad> {
    StreamingChunker::new(
        fake,
        EnergyVad::default(),
        config,
        track(),
        origin,
        seq_ids(),
    )
    .unwrap()
}

fn run<T: Transcriber, V: Vad>(
    c: &mut StreamingChunker<T, V>,
    audio: &[f32],
    chunk: usize,
) -> Vec<TranscriptEvent> {
    let mut events = Vec::new();
    for part in audio.chunks(chunk) {
        events.extend(c.push(part).unwrap());
    }
    events.extend(c.finish().unwrap());
    events
}

fn texts(words: &[Word]) -> Vec<String> {
    words.iter().map(|w| w.text.clone()).collect()
}

fn finals(events: &[TranscriptEvent]) -> Vec<Segment> {
    events
        .iter()
        .filter_map(|e| match e {
            TranscriptEvent::Final(s) => Some(s.clone()),
            _ => None,
        })
        .collect()
}

/// Stream contract: per id, partials then exactly one Final/Discarded and nothing after;
/// each partial's committed words are a prefix of every later partial and of the final.
fn check_stream_contract(events: &[TranscriptEvent]) {
    let mut closed: HashMap<EntityId, bool> = HashMap::new();
    let mut committed: HashMap<EntityId, Vec<String>> = HashMap::new();
    for e in events {
        match e {
            TranscriptEvent::Partial(p) => {
                let id = p.segment.id;
                assert!(
                    !closed.get(&id).copied().unwrap_or(false),
                    "partial after close"
                );
                closed.insert(id, false);
                let all = texts(&p.segment.words);
                if let Some(prev) = committed.get(&id) {
                    assert!(
                        all.starts_with(prev),
                        "committed words changed: {prev:?} -> {all:?}"
                    );
                    assert!(p.committed_words >= prev.len(), "committed prefix shrank");
                }
                committed.insert(id, all[..p.committed_words].to_vec());
                assert_eq!(p.segment.track, track());
            }
            TranscriptEvent::Final(s) => {
                assert!(!closed.get(&s.id).copied().unwrap_or(false), "double close");
                closed.insert(s.id, true);
                if let Some(prev) = committed.get(&s.id) {
                    assert!(
                        texts(&s.words).starts_with(prev),
                        "final dropped committed words"
                    );
                }
                assert_eq!(s.track, track());
                assert_eq!(s.text, join_words(&s.words));
                assert!(s.speaker_id.is_none());
            }
            TranscriptEvent::Discarded { id } => {
                assert_eq!(
                    closed.get(id),
                    Some(&false),
                    "discard of unknown or closed id"
                );
                closed.insert(*id, true);
            }
        }
    }
    assert!(
        closed.values().all(|&c| c),
        "segment left open after finish"
    );
}

// ---------------------------------------------------------------- types

#[test]
fn asr_meeting_time_converts_pipeline_frames() {
    assert_eq!(MeetingTime::from_pipeline_frames(16_000).as_millis(), 1000);
    assert_eq!(MeetingTime::from_pipeline_frames(8).as_millis(), 1); // 0.5 ms rounds up
    assert_eq!(MeetingTime::from_millis(1500).to_pipeline_frames(), 24_000);
    // Untrusted extremes neither overflow nor wrap.
    assert_eq!(
        MeetingTime::from_pipeline_frames(u64::MAX).as_millis(),
        u64::MAX / 16 + 1 // 2^64 / 16 - 1/16 rounds up
    );
    assert_eq!(
        MeetingTime::from_millis(u64::MAX).to_pipeline_frames(),
        u64::MAX
    );
    assert_eq!(format!("{:?}", MeetingTime::from_millis(61_005)), "61.005s");
    let t = MeetingTime::from_millis(1000);
    assert_eq!(
        t + Duration::from_millis(250),
        MeetingTime::from_millis(1250)
    );
    assert_eq!(
        t.saturating_sub_duration(Duration::from_secs(5)),
        MeetingTime::ZERO
    );
    assert_eq!(
        MeetingTime::from_millis(1200).saturating_sub(t),
        Duration::from_millis(200)
    );
}

#[test]
fn asr_segment_from_words_derives_text_span_confidence() {
    let words = vec![
        Word::new("Hello,", 100, 400, 0.8),
        Word::new("world.", 500, 900, 0.6),
    ];
    let s = Segment::from_words(
        EntityId::from_bytes([1; 16]),
        track(),
        words,
        MeetingTime::ZERO,
    );
    assert_eq!(s.text, "Hello, world.");
    assert_eq!(s.start.as_millis(), 100);
    assert_eq!(s.end.as_millis(), 900);
    assert!((s.confidence - 0.7).abs() < 1e-6);
    assert_eq!(s.speaker_id, None);

    let empty = Segment::from_words(s.id, track(), vec![], MeetingTime::from_millis(42));
    assert_eq!((empty.start.as_millis(), empty.end.as_millis()), (42, 42));
    assert_eq!(empty.text, "");
    assert_eq!(empty.confidence, 0.0);
}

#[test]
fn asr_segment_is_syncable_and_serde_round_trips() {
    fn syncable<T: Syncable>() {}
    syncable::<Segment>();

    let s = Segment::from_words(
        EntityId::from_bytes([1; 16]),
        track(),
        vec![Word::new("hi", 0, 300, 0.5)],
        MeetingTime::ZERO,
    );
    let json = serde_json::to_string(&s).unwrap();
    assert!(
        json.contains("\"start\":0"),
        "times serialise as plain ms: {json}"
    );
    assert_eq!(serde_json::from_str::<Segment>(&json).unwrap(), s);

    let ev = TranscriptEvent::Final(s.clone());
    let json = serde_json::to_string(&ev).unwrap();
    assert!(json.contains("\"kind\":\"final\""), "{json}");
    assert_eq!(serde_json::from_str::<TranscriptEvent>(&json).unwrap(), ev);

    let p = PartialSegment {
        segment: Segment::from_words(
            s.id,
            track(),
            vec![
                Word::new("a", 0, 1, 1.0),
                Word::new("b", 2, 3, 1.0),
                Word::new("c", 4, 5, 1.0),
            ],
            MeetingTime::ZERO,
        ),
        committed_words: 2,
    };
    assert_eq!(p.committed_text(), "a b");
    assert_eq!(p.tentative_text(), "c");
}

// ---------------------------------------------------------------- transcriber API

#[test]
fn asr_decoding_defaults_follow_plan_4_0() {
    assert_eq!(DecodingOptions::LIVE.strategy, DecodingStrategy::Greedy);
    assert_eq!(DecodingOptions::LIVE.temperature, 0.0);
    assert_eq!(
        DecodingOptions::BATCH.strategy,
        DecodingStrategy::BeamSearch { beam_size: 5 }
    );
    assert_eq!(DecodingOptions::BATCH.temperature, 0.0);
    // T2.4d: Whisper's reference fallback on both paths (openai/whisper transcribe.py).
    for d in [DecodingOptions::LIVE, DecodingOptions::BATCH] {
        let f = d.fallback.expect("temperature fallback");
        assert_eq!(f, TemperatureFallback::WHISPER);
        assert_eq!(
            (f.increment, f.repetition_threshold, f.logprob_threshold),
            (0.2, 2.4, -1.0)
        );
    }
    assert_eq!(TranscribeOptions::live().decoding, DecodingOptions::LIVE);
    assert_eq!(TranscribeOptions::batch().decoding, DecodingOptions::BATCH);
    assert_eq!(
        TranscribeOptions::live().language,
        Language::Code("en".into())
    );
    assert_eq!(ChunkerConfig::default().options, TranscribeOptions::live());
    let json = serde_json::to_string(&DecodingOptions::BATCH).unwrap();
    assert_eq!(
        json,
        r#"{"strategy":{"strategy":"beam_search","beam_size":5},"temperature":0.0,"fallback":{"increment":0.2,"repetition_threshold":2.4,"logprob_threshold":-1.0}}"#
    );
}

#[test]
fn asr_check_pipeline_rate_rejects_other_rates() {
    assert!(check_pipeline_rate(&MonoBuffer::new(PIPELINE_RATE, vec![0.0; 4])).is_ok());
    let err = check_pipeline_rate(&MonoBuffer::new(SampleRate::HZ_48000, vec![0.0; 4]));
    assert_eq!(
        err,
        Err(AsrError::WrongSampleRate {
            expected: 16_000,
            actual: 48_000
        })
    );
    assert_eq!(
        err.unwrap_err().to_string(),
        "audio must be 16000 Hz, got 48000 Hz"
    );
}

#[test]
fn asr_cancel_token_is_shared_by_clones() {
    let a = CancelToken::new();
    let b = a.clone();
    assert!(!b.is_cancelled());
    a.cancel();
    assert!(b.is_cancelled());
}

#[test]
fn asr_cancel_token_counts_every_poll_on_any_clone() {
    // T9.22: the polls are a decode's sign of life for a watchdog.
    let a = CancelToken::new();
    let b = a.clone();
    assert_eq!(a.polls(), 0);
    let _ = b.is_cancelled();
    let _ = a.is_cancelled();
    assert_eq!(b.polls(), 2);
    // Cancelling and reading the count are not polls.
    a.cancel();
    assert_eq!(a.polls(), 2);
    // A token of its own counts its own.
    assert_eq!(CancelToken::new().polls(), 0);
}

#[test]
fn asr_raw_segment_interpolates_missing_word_times() {
    let raw = RawSegment {
        start: MeetingTime::from_millis(1000),
        end: MeetingTime::from_millis(1600),
        text: " ab  abcd ".into(),
        words: vec![],
    };
    let w = raw.timed_words();
    assert_eq!(texts(&w), ["ab", "abcd"]);
    assert_eq!((w[0].start.as_millis(), w[0].end.as_millis()), (1000, 1200));
    assert_eq!((w[1].start.as_millis(), w[1].end.as_millis()), (1200, 1600));
}

#[test]
fn asr_transcription_into_segments_shifts_and_drops_empty() {
    let backend = FakeTranscriber::scripted([]).backend_info();
    let t = Transcription {
        segments: vec![
            RawSegment::from_words(vec![
                Word::new("one", 0, 300, 1.0),
                Word::new("two", 400, 700, 0.5),
            ]),
            RawSegment {
                start: MeetingTime::from_millis(800),
                end: MeetingTime::from_millis(900),
                text: "  ".into(),
                words: vec![],
            },
            RawSegment::from_words(vec![Word::new("three", 1000, 1300, 1.0)]),
        ],
        language: Some("en".into()),
        backend,
    };
    assert_eq!(texts(&t.words()), ["one", "two", "three"]);
    let mut ids = seq_ids();
    let segs = t.into_segments(track(), MeetingTime::from_millis(60_000), &mut ids);
    assert_eq!(segs.len(), 2);
    assert_eq!(segs[0].text, "one two");
    assert_eq!(segs[0].start.as_millis(), 60_000);
    assert_eq!(segs[0].end.as_millis(), 60_700);
    assert!((segs[0].confidence - 0.75).abs() < 1e-6);
    assert_eq!(segs[1].words[0].start.as_millis(), 61_000);
    assert_ne!(segs[0].id, segs[1].id);
    assert_eq!(segs[1].track, track());
}

// ---------------------------------------------------------------- fake

#[test]
fn asr_fake_scripted_returns_in_order_and_records_calls() {
    let mut fake = FakeTranscriber::scripted([
        Ok(vec![RawSegment::from_words(vec![Word::new(
            "a", 0, 100, 1.0,
        )])]),
        Err(AsrError::Backend("boom".into())),
    ]);
    assert_eq!(fake.backend_info().kind, BackendKind::Fake);
    let audio = MonoBuffer::new(PIPELINE_RATE, vec![0.0; 1600]);
    let cancel = CancelToken::new();
    let first = fake
        .transcribe(&audio, &TranscribeOptions::batch(), &cancel)
        .unwrap();
    assert_eq!(texts(&first.words()), ["a"]);
    assert_eq!(first.backend.kind, BackendKind::Fake);
    assert_eq!(
        fake.transcribe(&audio, &TranscribeOptions::live(), &cancel),
        Err(AsrError::Backend("boom".into()))
    );
    assert!(matches!(
        fake.transcribe(&audio, &TranscribeOptions::live(), &cancel),
        Err(AsrError::Backend(_))
    ));
    assert_eq!(fake.calls().len(), 3);
    assert_eq!(fake.calls()[0].samples, 1600);
    assert_eq!(fake.calls()[0].options, TranscribeOptions::batch());
    assert_eq!(fake.calls()[1].options, TranscribeOptions::live());
}

#[test]
fn asr_fake_timeline_jitter_is_deterministic_and_bounded() {
    let words = words_at("w", 1000, 5);
    let audio = audio_for(&words, 4000, 24);
    let buf = MonoBuffer::new(
        PIPELINE_RATE,
        audio[ms_to_frames(500)..ms_to_frames(3500)].to_vec(),
    );
    let cancel = CancelToken::new();
    let run_calls = || {
        let mut fake = FakeTranscriber::timeline(audio.clone(), words.clone())
            .with_timestamp_jitter(Duration::from_millis(300));
        (0..8)
            .map(|_| {
                fake.transcribe(&buf, &TranscribeOptions::live(), &cancel)
                    .unwrap()
                    .words()
            })
            .collect::<Vec<_>>()
    };
    let a = run_calls();
    assert_eq!(a, run_calls(), "jitter must be deterministic");
    let starts: Vec<u64> = a.iter().map(|ws| ws[1].start.as_millis()).collect();
    assert!(
        starts.iter().any(|&s| s != starts[0]),
        "no jitter applied: {starts:?}"
    );
    for ws in &a {
        assert_eq!(texts(ws), texts(&words));
        // w1 is at 900..1200 ms in the buffer; shifted by at most 300 ms.
        assert!((600..=1200).contains(&ws[1].start.as_millis()), "{ws:?}");
        assert_eq!(ws[1].end.as_millis() - ws[1].start.as_millis(), 300);
    }
}

#[test]
fn asr_fake_checks_rate_and_cancellation_first() {
    let mut fake = FakeTranscriber::scripted([Ok(vec![])]);
    let cancel = CancelToken::new();
    let wrong = MonoBuffer::new(SampleRate::HZ_48000, vec![0.0; 10]);
    assert!(matches!(
        fake.transcribe(&wrong, &TranscribeOptions::live(), &cancel),
        Err(AsrError::WrongSampleRate { .. })
    ));
    cancel.cancel();
    let ok = MonoBuffer::new(PIPELINE_RATE, vec![0.0; 10]);
    assert_eq!(
        fake.transcribe(&ok, &TranscribeOptions::live(), &cancel),
        Err(AsrError::Cancelled)
    );
    assert!(fake.calls().is_empty());
}

#[test]
fn asr_fake_timeline_returns_words_inside_buffer() {
    let words = words_at("w", 1000, 5); // w0 1000-1300 ... w4 2600-2900
    let audio = audio_for(&words, 4000, 1);
    let mut fake = FakeTranscriber::timeline(audio.clone(), words);
    let cancel = CancelToken::new();
    // Buffer 1100..2500 ms: midpoints 1150 (w0), 1550, 1950, 2350 inside; w4 (2750) not.
    let buf = MonoBuffer::new(
        PIPELINE_RATE,
        audio[ms_to_frames(1100)..ms_to_frames(2500)].to_vec(),
    );
    let t = fake
        .transcribe(&buf, &TranscribeOptions::live(), &cancel)
        .unwrap();
    let w = t.words();
    assert_eq!(texts(&w), ["w0", "w1", "w2", "w3"]);
    assert_eq!((w[1].start.as_millis(), w[1].end.as_millis()), (300, 600));
    assert_eq!(w[0].start.as_millis(), 0, "clamped to the buffer start");
    assert_eq!(fake.calls()[0].timeline_offset, Some(ms_to_frames(1100)));

    // Unstable tail of 400 ms: w3 ends at the buffer end (unstable), w2 at 2100 (stable).
    let mut fake = fake.with_unstable_tail(Duration::from_millis(400));
    let a = fake
        .transcribe(&buf, &TranscribeOptions::live(), &cancel)
        .unwrap()
        .words();
    let b = fake
        .transcribe(&buf, &TranscribeOptions::live(), &cancel)
        .unwrap()
        .words();
    assert_eq!(texts(&a)[..3], ["w0", "w1", "w2"]);
    assert_ne!(a[3].text, "w3");
    assert_ne!(normalize_token(&a[3].text), normalize_token(&b[3].text));

    let foreign = MonoBuffer::new(PIPELINE_RATE, vec![0.123; 100]);
    assert!(matches!(
        fake.transcribe(&foreign, &TranscribeOptions::live(), &cancel),
        Err(AsrError::Backend(_))
    ));
}

// ---------------------------------------------------------------- energy VAD

fn level_frames(amp: f32, frames: usize, seed: u64) -> Vec<Vec<f32>> {
    let mut n = Noise(seed);
    (0..frames)
        .map(|_| (0..480).map(|_| n.next() * amp).collect())
        .collect()
}

fn decisions(vad: &mut EnergyVad, frames: &[Vec<f32>]) -> Vec<bool> {
    frames
        .iter()
        .map(|f| vad.speech_probability(f).unwrap() >= 0.5)
        .collect()
}

#[test]
fn asr_energy_vad_silence_and_speech() {
    let mut vad = EnergyVad::default();
    assert_eq!(vad.frame_len(), 480);
    assert!(
        decisions(&mut vad, &vec![vec![0.0; 480]; 10])
            .iter()
            .all(|&s| !s)
    );
    assert!(
        decisions(&mut vad, &level_frames(QUIET_AMP, 20, 2))
            .iter()
            .all(|&s| !s)
    );
    assert!(
        decisions(&mut vad, &level_frames(SPEECH_AMP, 20, 3))
            .iter()
            .all(|&s| s)
    );
    assert!(
        decisions(&mut vad, &level_frames(QUIET_AMP, 5, 4))
            .iter()
            .all(|&s| !s)
    );
}

#[test]
fn asr_energy_vad_ignores_quiet_signal_below_absolute_gate() {
    let mut vad = EnergyVad::default();
    decisions(&mut vad, &vec![vec![0.0; 480]; 5]);
    // About -65 dBFS: far above the digital-silence floor but under -50 dBFS.
    assert!(
        decisions(&mut vad, &level_frames(1e-3, 20, 5))
            .iter()
            .all(|&s| !s)
    );
}

#[test]
fn asr_energy_vad_adapts_to_steady_noise() {
    let mut vad = EnergyVad::default();
    // Steady -35 dBFS noise from the start: the floor follows it, so it is not speech.
    let steady = level_frames(0.03, 100, 6);
    assert!(decisions(&mut vad, &steady).iter().all(|&s| !s));
    let floor = vad.noise_floor_dbfs().unwrap();
    assert!((-42.0..-30.0).contains(&floor), "floor {floor}");
    // 20 dB louder bursts are speech.
    assert!(
        decisions(&mut vad, &level_frames(0.3, 10, 7))
            .iter()
            .all(|&s| s)
    );
    vad.reset();
    assert_eq!(vad.noise_floor_dbfs(), None);
}

#[test]
fn asr_energy_vad_id_changes_with_every_setting() {
    let base = EnergyVad::default().id();
    assert_eq!(base, EnergyVad::default().id());
    let other = |c: EnergyVadConfig| EnergyVad::new(c).unwrap().id();
    let d = EnergyVadConfig::default();
    for changed in [
        EnergyVadConfig {
            frame_len: 512,
            ..d
        },
        EnergyVadConfig {
            min_level_dbfs: -55.0,
            ..d
        },
        EnergyVadConfig {
            margin_db: 9.0,
            ..d
        },
        EnergyVadConfig {
            floor_rise_db_per_s: d.floor_rise_db_per_s + 1.0,
            ..d
        },
        EnergyVadConfig {
            min_window: Duration::from_secs(2),
            ..d
        },
    ] {
        assert_ne!(other(changed), base, "{changed:?}");
    }
}

#[test]
fn asr_energy_vad_rejects_bad_frames_and_config() {
    let mut vad = EnergyVad::default();
    assert!(matches!(
        vad.speech_probability(&[0.0; 479]),
        Err(AsrError::Config(_))
    ));
    assert!(
        EnergyVad::new(EnergyVadConfig {
            frame_len: 0,
            ..Default::default()
        })
        .is_err()
    );
    assert!(
        EnergyVad::new(EnergyVadConfig {
            margin_db: f32::NAN,
            ..Default::default()
        })
        .is_err()
    );
    let v = EnergyVad::new(EnergyVadConfig {
        frame_len: 512,
        ..Default::default()
    })
    .unwrap();
    assert_eq!(v.frame_len(), 512);
    assert!(
        EnergyVad::new(EnergyVadConfig {
            min_window: Duration::ZERO,
            ..Default::default()
        })
        .is_err()
    );
}

/// Frames alternating `on` frames at `loud` and `off` frames at `quiet`, `cycles` times.
fn pulsed(loud: f32, quiet: f32, on: usize, off: usize, cycles: usize, seed: u64) -> Vec<Vec<f32>> {
    let mut out = Vec::new();
    for c in 0..cycles {
        out.extend(level_frames(loud, on, seed + 2 * c as u64));
        out.extend(level_frames(quiet, off, seed + 2 * c as u64 + 1));
    }
    out
}

#[test]
fn asr_energy_vad_floor_rises_at_the_configured_rate() {
    // Silence, then a steady -35 dBFS hum that never stops. It starts as "speech", but the
    // floor rises at 3 dB/s (from -85 dBFS it needs about 13 s to come within 10 dB of
    // the hum), after which the hum is background. A VAD that ignored the rise rate would
    // either never adapt or adapt at once.
    let mut vad = EnergyVad::default();
    decisions(&mut vad, &level_frames(QUIET_AMP, 20, 30));
    let hum = decisions(&mut vad, &level_frames(0.03, 700, 31)); // 21 s
    assert!(hum[..300].iter().all(|&s| s), "adapted faster than 3 dB/s");
    assert!(hum[500..].iter().all(|&s| !s), "never adapted to the hum");
}

#[test]
fn asr_energy_vad_floor_is_bounded_by_recent_minimum() {
    // Speech with a short pause every second. Even with a fast-rising floor (80 dB/s), the
    // floor may not exceed the minimum level of the last 1.5 s (the pauses), so speech is
    // never absorbed into the floor (minimum statistics, Martin 2001).
    let config = EnergyVadConfig {
        floor_rise_db_per_s: 80.0,
        ..Default::default()
    };
    let mut vad = EnergyVad::new(config).unwrap();
    decisions(&mut vad, &level_frames(QUIET_AMP, 5, 39)); // background before the talker
    let frames = pulsed(SPEECH_AMP, QUIET_AMP, 31, 2, 10, 40);
    let d = decisions(&mut vad, &frames);
    for c in 0..10 {
        let speech = &d[c * 33..c * 33 + 31];
        assert!(
            speech.iter().all(|&s| s),
            "cycle {c}: speech lost {speech:?}"
        );
    }
}

// ---------------------------------------------------------------- LocalAgreement

fn w(text: &str, s: u64, e: u64) -> Word {
    Word::new(text, s, e, 0.9)
}

#[test]
fn asr_normalize_token_ignores_case_and_punctuation() {
    assert_eq!(normalize_token("Hello,"), "hello");
    assert_eq!(normalize_token("Don't!"), "don't");
    assert_eq!(normalize_token("--"), "");
}

#[test]
fn asr_local_agreement_commits_common_prefix_of_last_two() {
    let mut la = LocalAgreement::new();
    let h1 = vec![w("a", 0, 100), w("b", 200, 300), w("c", 400, 500)];
    assert!(
        la.agree(la.accept(&h1)).is_empty(),
        "one hypothesis commits nothing"
    );
    assert_eq!(texts(la.tentative()), ["a", "b", "c"]);

    let h2 = vec![
        w("A,", 0, 110),
        w("b", 200, 310),
        w("d", 400, 520),
        w("e", 600, 700),
    ];
    let committed = la.agree(la.accept(&h2));
    assert_eq!(
        texts(&committed),
        ["A,", "b"],
        "texts and times come from the newer one"
    );
    assert_eq!(committed[0].end.as_millis(), 110);
    assert_eq!(texts(la.tentative()), ["d", "e"]);

    // Committed words are filtered by time from later hypotheses.
    let h3 = vec![
        w("a", 0, 100),
        w("b", 200, 300),
        w("d", 400, 520),
        w("e", 600, 700),
        w("f", 800, 900),
    ];
    let tail = la.accept(&h3);
    assert_eq!(texts(&tail), ["d", "e", "f"]);
    assert_eq!(texts(&la.agree(tail)), ["d", "e"]);
    assert_eq!(texts(la.tentative()), ["f"]);
}

#[test]
fn asr_local_agreement_removes_ngram_overlap_with_committed_tail() {
    let mut la = LocalAgreement::new();
    la.commit(&[w("x", 0, 300), w("y", 400, 700)]);
    // The recogniser re-timed "y" so that it starts after the committed end: the time rule
    // keeps it, the 1-gram overlap rule removes it.
    let tail = la.accept(&[w("Y", 700, 900), w("z", 1000, 1200)]);
    assert_eq!(texts(&tail), ["z"]);
    // Two-word overlap.
    let tail = la.accept(&[w("x", 650, 700), w("y", 700, 900), w("z", 1000, 1200)]);
    assert_eq!(texts(&tail), ["z"]);
    // Far from the committed end (> 1 s): no n-gram removal.
    let tail = la.accept(&[w("y", 2000, 2200), w("z", 2300, 2500)]);
    assert_eq!(texts(&tail), ["y", "z"]);
    la.clear_history();
    let tail = la.accept(&[w("y", 0, 300), w("z", 1000, 1200)]);
    assert_eq!(texts(&tail), ["y", "z"]);
}

#[test]
fn asr_local_agreement_keeps_tentative_words_retimed_earlier() {
    // Committed "x y" (y ends at 700 ms); "z" is tentative. The next decoding context moves
    // every word 300 ms earlier, so "z" now starts before the committed end: a pure time
    // rule (or a time floor) would drop it.
    let mut la = LocalAgreement::new();
    la.commit(&[w("x", 0, 300), w("y", 400, 700)]);
    la.set_tentative(vec![w("z", 800, 1100)]);
    // Anchored on the committed tail "y".
    let tail = la.accept(&[w("y", 100, 400), w("z", 500, 800), w("q", 900, 1200)]);
    assert_eq!(texts(&tail), ["z", "q"]);
    // Committed words not in the window: anchored on the tentative "z".
    let tail = la.accept(&[w("z", 500, 800), w("q", 900, 1200)]);
    assert_eq!(texts(&tail), ["z", "q"]);
    // Neither anchor: the whisper_streaming time rule applies.
    let tail = la.accept(&[w("r", 500, 800), w("q", 900, 1200)]);
    assert_eq!(texts(&tail), ["q"]);
}

#[test]
fn asr_local_agreement_anchors_committed_run_when_whisper_collapses_word_times() {
    // T2.4b, from a dev-clean trace (base.en, streaming): whisper.cpp gave the last words
    // of a decode the buffer-end time, so the committed run "name of the book here," (and
    // the committed end) sits at 39.638 s. The next decode re-times the same words to its
    // own buffer end (40.668 s), more than 1 s after the committed end, so a time-bounded
    // anchor search missed them and the time rule re-emitted "I mention the name ...".
    let mut la = LocalAgreement::new();
    let collapsed = |t: &str| w(t, 39_638, 39_638);
    la.commit(&[
        collapsed("name"),
        collapsed("of"),
        collapsed("the"),
        collapsed("book"),
        collapsed("here,"),
    ]);
    la.set_tentative(
        ["though", "I", "should", "fancy", "the", "book."]
            .iter()
            .map(|t| collapsed(t))
            .collect(),
    );
    let mut hyp = vec![
        w("I", 28_408, 28_568),
        w("am", 28_568, 28_628),
        w("surprised,", 28_628, 29_188),
        w("reviewer.", 36_098, 36_618),
        w("Neither", 36_858, 38_448),
        w("shall", 38_448, 39_718),
        w("I", 39_718, 39_968),
        w("mention", 39_968, 40_668),
    ];
    for t in [
        "the", "name", "of", "the", "book", "here,", "though", "I", "should", "fancy", "that",
    ] {
        hyp.push(w(t, 40_668, 40_668));
    }
    let tail = la.accept(&hyp);
    assert_eq!(texts(&tail), ["though", "I", "should", "fancy", "that"]);
}

#[test]
fn asr_local_agreement_tentative_anchor_never_reaches_back_before_the_committed_end() {
    // T2.4b: the previous tentative tail starts with "I"; the new hypothesis re-decodes the
    // window from its start, which also begins with "I" 10 s before the committed end.
    // Anchoring there re-emitted the whole committed utterance.
    let mut la = LocalAgreement::new();
    la.commit(&[w("x", 10_000, 10_500)]);
    la.set_tentative(vec![w("I", 10_600, 10_700), w("b", 10_700, 10_900)]);
    let tail = la.accept(&[
        w("I", 0, 100),
        w("am", 100, 300),
        w("b", 10_600, 10_900),
        w("c", 10_900, 11_200),
    ]);
    assert_eq!(texts(&tail), ["b", "c"]);
}

#[test]
fn asr_local_agreement_tentative_anchor_survives_a_collapsed_committed_end() {
    // T2.4b, from a dev-clean trace: "violets" was committed with whisper's buffer-end time
    // (31.16-33.11 s), so the committed end lies 1.4 s after the tentative "screaming"
    // (31.68 s) of the next decode. The tentative anchor is bounded by the committed
    // word's start, not its collapsed end, so it still applies.
    let mut la = LocalAgreement::new();
    la.commit(&[
        w("white-headed", 30_480, 31_140),
        w("violets", 31_160, 33_110),
    ]);
    la.set_tentative(vec![
        w("screaming", 33_110, 33_110),
        w("all", 33_110, 33_110),
    ]);
    let tail = la.accept(&[
        w("white-headed", 30_480, 31_140),
        w("Violet", 31_140, 31_680),
        w("screaming", 31_680, 32_190),
        w("all", 32_190, 32_420),
        w("the", 32_420, 32_610),
        w("while", 32_610, 32_980),
    ]);
    assert_eq!(texts(&tail), ["screaming", "all", "the", "while"]);
}

#[test]
fn asr_local_agreement_keeps_speech_that_repeats_committed_words() {
    // "I think" was committed; the speaker says "I think" again right away. The earliest
    // candidate occurrence is the committed one, so the repeat is kept.
    let mut la = LocalAgreement::new();
    la.commit(&[w("I", 4_000, 4_300), w("think", 4_300, 4_800)]);
    let tail = la.accept(&[
        w("I", 4_000, 4_300),
        w("think", 4_300, 4_800),
        w("I", 5_200, 5_400),
        w("think", 5_400, 5_900),
        w("so", 6_000, 6_300),
    ]);
    assert_eq!(texts(&tail), ["I", "think", "so"]);

    // After a cut committed "... thank you very much", the next window's overlap decodes
    // the cut word differently ("hank"), so the committed run is not in the hypothesis;
    // a refrain of the same words 6 s later is new speech and must not anchor.
    let mut la = LocalAgreement::new();
    la.commit(&[
        w("thank", 9_000, 9_300),
        w("you", 9_300, 9_500),
        w("very", 9_500, 9_800),
        w("much", 9_800, 10_000),
    ]);
    let tail = la.accept(&[
        w("hank", 9_050, 9_300),
        w("and", 10_200, 10_400),
        w("then", 10_400, 10_700),
        w("thank", 16_000, 16_300),
        w("you", 16_300, 16_500),
        w("very", 16_500, 16_800),
        w("much", 16_800, 17_000),
        w("bye", 17_200, 17_500),
    ]);
    assert_eq!(
        texts(&tail),
        ["and", "then", "thank", "you", "very", "much", "bye"]
    );
}

#[test]
fn asr_local_agreement_never_matches_empty_tokens() {
    let mut la = LocalAgreement::new();
    assert!(la.agree(vec![w("--", 0, 100), w("a", 200, 300)]).is_empty());
    let committed = la.agree(vec![w("...", 0, 100), w("a", 200, 300)]);
    assert!(
        committed.is_empty(),
        "punctuation-only tokens agreed: {committed:?}"
    );
    // Nor as a dedupe anchor.
    let mut la = LocalAgreement::new();
    la.commit(&[w("a", 0, 100), w("?", 200, 300)]);
    let tail = la.accept(&[w("!", 300, 400), w("b", 500, 600)]);
    assert_eq!(texts(&tail), ["!", "b"]);
}

// ---------------------------------------------------------------- chunker

#[test]
fn asr_chunker_config_validation() {
    assert!(ChunkerConfig::default().validate().is_ok());
    let bad = [
        ChunkerConfig {
            max_window: Duration::ZERO,
            ..Default::default()
        },
        ChunkerConfig {
            max_window: Duration::from_secs(31),
            ..Default::default()
        },
        ChunkerConfig {
            overlap: Duration::from_millis(7500),
            ..Default::default()
        },
        ChunkerConfig {
            partial_interval: Duration::ZERO,
            ..Default::default()
        },
        ChunkerConfig {
            vad_threshold: 0.0,
            ..Default::default()
        },
        ChunkerConfig {
            vad_threshold: f32::NAN,
            ..Default::default()
        },
        ChunkerConfig {
            min_silence: Duration::ZERO,
            ..Default::default()
        },
        ChunkerConfig {
            speech_pad: Duration::from_millis(251),
            ..Default::default()
        },
    ];
    for c in bad {
        assert!(matches!(c.validate(), Err(AsrError::Config(_))), "{c:?}");
        let r = StreamingChunker::new(
            FakeTranscriber::scripted([]),
            EnergyVad::default(),
            c,
            track(),
            MeetingTime::ZERO,
            seq_ids(),
        );
        assert!(r.is_err());
    }
}

#[test]
fn asr_chunker_silence_produces_nothing() {
    let audio = audio_for(&[], 5000, 11);
    let mut c = chunker(
        FakeTranscriber::timeline(audio.clone(), vec![]),
        ChunkerConfig::default(),
        MeetingTime::ZERO,
    );
    assert!(run(&mut c, &audio, 4000).is_empty());
    assert!(c.transcriber().calls().is_empty());
}

#[test]
fn asr_chunker_single_utterance_partials_then_final_with_stable_id() {
    let words = words_at("w", 1000, 10); // 1.0 s .. 4.9 s
    let audio = audio_for(&words, 7000, 12);
    let fake = FakeTranscriber::timeline(audio.clone(), words.clone())
        .with_unstable_tail(Duration::from_millis(150));
    let mut c = chunker(
        fake,
        ChunkerConfig::default(),
        MeetingTime::from_millis(60_000),
    );
    let events = run(&mut c, &audio, 1600);
    check_stream_contract(&events);

    let partials: Vec<&PartialSegment> = events
        .iter()
        .filter_map(|e| match e {
            TranscriptEvent::Partial(p) => Some(p),
            _ => None,
        })
        .collect();
    let f = finals(&events);
    assert_eq!(f.len(), 1);
    assert!(
        partials.len() >= 2,
        "expected several partials, got {}",
        partials.len()
    );
    assert!(
        partials.iter().all(|p| p.segment.id == f[0].id),
        "segment id must be stable"
    );
    assert!(
        partials.iter().any(|p| p.committed_words > 0),
        "LocalAgreement never committed"
    );
    for p in &partials {
        assert!(
            !p.committed_text().contains('~'),
            "unstable word committed: {p:?}"
        );
    }
    assert_eq!(texts(&f[0].words), texts(&words));
    // Meeting-relative times: origin 60 s + 1.0 s.
    assert_eq!(f[0].start.as_millis(), 61_000);
    assert_eq!(f[0].end.as_millis(), 64_900);
    assert!((f[0].confidence - 0.9).abs() < 1e-6);

    // Every decode is live-greedy and the stream is done after the final.
    for call in c.transcriber().calls() {
        assert_eq!(call.options.decoding, DecodingOptions::LIVE);
        assert_eq!(
            call.options.prompt, None,
            "nothing final before the first utterance"
        );
    }
}

#[test]
fn asr_chunker_long_speech_is_cut_at_most_15s_without_losing_words() {
    let words = words_at("w", 500, 100); // 40 s of continuous speech
    let audio = audio_for(&words, 42_000, 13);
    let fake = FakeTranscriber::timeline(audio.clone(), words.clone())
        .with_unstable_tail(Duration::from_millis(150));
    let mut c = chunker(fake, ChunkerConfig::default(), MeetingTime::ZERO);
    let events = run(&mut c, &audio, 4800);
    check_stream_contract(&events);

    let max = ms_to_frames(15_000);
    for call in c.transcriber().calls() {
        assert!(
            call.samples <= max,
            "window of {} samples exceeds 15 s",
            call.samples
        );
    }
    let f = finals(&events);
    assert!(
        f.len() >= 3,
        "40 s must be cut into at least 3 segments, got {}",
        f.len()
    );
    let mut ids: Vec<EntityId> = f.iter().map(|s| s.id).collect();
    ids.dedup();
    assert_eq!(ids.len(), f.len(), "every segment gets its own id");
    let all: Vec<String> = f.iter().flat_map(|s| texts(&s.words)).collect();
    assert_eq!(
        all,
        texts(&words),
        "words lost or duplicated across chunk boundaries"
    );
    for pair in f.windows(2) {
        assert!(pair[0].end <= pair[1].start, "segments overlap: {pair:?}");
    }
}

#[test]
fn asr_chunker_cuts_survive_timestamp_jitter_below_half_overlap() {
    // whisper's word times move between decoding contexts. With up to 400 ms of per-call
    // jitter (default overlap 1 s, so below overlap / 2), no word may be lost or duplicated
    // at a cut.
    let words = words_at("w", 500, 100);
    let audio = audio_for(&words, 42_000, 21);
    for jitter_ms in [100, 250, 400] {
        let fake = FakeTranscriber::timeline(audio.clone(), words.clone())
            .with_timestamp_jitter(Duration::from_millis(jitter_ms));
        let mut c = chunker(fake, ChunkerConfig::default(), MeetingTime::ZERO);
        let events = run(&mut c, &audio, 4800);
        check_stream_contract(&events);
        let f = finals(&events);
        assert!(f.len() >= 3, "jitter {jitter_ms}: expected cuts");
        let all: Vec<String> = f.iter().flat_map(|s| texts(&s.words)).collect();
        assert_eq!(all, texts(&words), "jitter {jitter_ms} ms");
    }
}

/// Speech (one loud block) from 0.5 s to `loud_end_ms`, then quiet, with words inside it.
fn block_stream(loud_end_ms: u64, total_ms: u64, seed: u64) -> (Vec<f32>, Vec<Word>) {
    let audio = audio_for(&[Word::new("x", 500, loud_end_ms, 1.0)], total_ms, seed);
    let n = ((loud_end_ms - 500 - 300) / 400 + 1) as usize;
    (audio, words_at("w", 500, n))
}

#[test]
fn asr_chunker_utterance_ending_just_after_a_cut_is_safe() {
    // The cut moves the window start to `cut - overlap`; if the utterance then ends with
    // `last_speech_end + speech_pad` before that, the close range used to underflow.
    // Reachable whenever overlap + speech_pad < min_silence.
    let cases = [
        // (overlap, min_silence, loud_end): overlap 0, cut at about 15.27 s.
        (0, 500, 15_000),
        // Default overlap, long min_silence: silence at the cut exceeds overlap + pad.
        (1000, 1500, 14_000),
    ];
    for (overlap_ms, min_silence_ms, loud_end) in cases {
        let (audio, words) = block_stream(loud_end, 20_000, 22);
        let config = ChunkerConfig {
            overlap: Duration::from_millis(overlap_ms),
            min_silence: Duration::from_millis(min_silence_ms),
            ..Default::default()
        };
        assert!(config.validate().is_ok());
        let fake = FakeTranscriber::timeline(audio.clone(), words.clone());
        let mut c = chunker(fake, config, MeetingTime::ZERO);
        let events = run(&mut c, &audio, 4800);
        check_stream_contract(&events);
        for call in c.transcriber().calls() {
            assert!(call.samples > 0 && call.samples <= ms_to_frames(15_000));
        }
        let all: Vec<String> = finals(&events)
            .iter()
            .flat_map(|s| texts(&s.words))
            .collect();
        assert_eq!(
            all,
            texts(&words),
            "overlap {overlap_ms}, min_silence {min_silence_ms}"
        );
    }
}

/// A VAD that ignores the audio and follows a seeded speech/silence pattern.
struct PatternVad {
    noise: Noise,
    speech: bool,
    left: u32,
}

impl Vad for PatternVad {
    fn id(&self) -> String {
        "pattern-vad".into()
    }
    fn frame_len(&self) -> usize {
        480
    }
    fn speech_probability(&mut self, _frame: &[f32]) -> Result<f32, AsrError> {
        if self.left == 0 {
            self.speech = !self.speech;
            let r = (self.noise.next().abs() * 1000.0) as u32;
            // Speech runs of 1-700 frames (up to 21 s), silence runs of 1-100 frames.
            self.left = 1 + if self.speech { r % 700 } else { r % 100 };
        }
        self.left -= 1;
        Ok(if self.speech { 0.9 } else { 0.1 })
    }
    fn reset(&mut self) {}
}

#[test]
fn asr_chunker_every_valid_config_is_safe_for_any_vad_pattern() {
    let words = words_at("w", 0, 150); // 60 s of words
    let audio = audio_for(&words, 60_000, 23);
    let ms = Duration::from_millis;
    let configs = [
        ChunkerConfig::default(),
        ChunkerConfig {
            overlap: ms(0),
            ..Default::default()
        },
        ChunkerConfig {
            overlap: ms(7_490),
            ..Default::default()
        },
        ChunkerConfig {
            min_silence: ms(3_000),
            speech_pad: ms(1_500),
            ..Default::default()
        },
        ChunkerConfig {
            min_silence: ms(30),
            speech_pad: ms(0),
            min_speech: ms(0),
            ..Default::default()
        },
        ChunkerConfig {
            max_window: ms(1_000),
            overlap: ms(400),
            partial_interval: ms(300),
            ..Default::default()
        },
        ChunkerConfig {
            max_window: ms(30_000),
            overlap: ms(250),
            partial_interval: ms(20_000),
            ..Default::default()
        },
        ChunkerConfig {
            min_speech: ms(1_000),
            min_silence: ms(1_300),
            overlap: ms(100),
            ..Default::default()
        },
    ];
    // Partial decodes: on, off, or toggled on every push (the pipeline's backpressure).
    let modes = ["partials on", "partials off", "partials toggled per push"];
    for (i, config) in configs.into_iter().enumerate() {
        assert!(config.validate().is_ok(), "config {i}");
        let max = PIPELINE_RATE.frames_for(config.max_window);
        for (seed, mode) in [1, 2, 3].into_iter().flat_map(|s| modes.map(|m| (s, m))) {
            let vad = PatternVad {
                noise: Noise(seed * 7919 + i as u64),
                speech: false,
                left: 0,
            };
            let fake = FakeTranscriber::timeline(audio.clone(), words.clone());
            let mut c = StreamingChunker::new(
                fake,
                vad,
                config.clone(),
                track(),
                MeetingTime::ZERO,
                seq_ids(),
            )
            .unwrap();
            c.set_partial_decodes(mode != modes[1]);
            let mut events = Vec::new();
            for (k, part) in audio.chunks(1601).enumerate() {
                if mode == modes[2] {
                    c.set_partial_decodes(k.is_multiple_of(2));
                }
                events.extend(c.push(part).unwrap());
            }
            events.extend(c.finish().unwrap());
            check_stream_contract(&events);
            if mode == modes[1] {
                assert!(
                    !events
                        .iter()
                        .any(|e| matches!(e, TranscriptEvent::Partial(_))),
                    "config {i} seed {seed}: partial with partials off"
                );
            }
            for call in c.transcriber().calls() {
                assert!(
                    call.samples > 0 && call.samples <= max,
                    "config {i} seed {seed} {mode}"
                );
            }
            let mut all: Vec<String> = finals(&events)
                .iter()
                .flat_map(|s| texts(&s.words))
                .collect();
            let n = all.len();
            all.sort();
            all.dedup();
            assert_eq!(
                all.len(),
                n,
                "config {i} seed {seed} {mode}: duplicated words"
            );
        }
    }
}

/// Speech exactly inside the given sample spans (30 ms frames, judged by the frame's
/// first sample): a VAD that misses a quiet word.
struct SpanVad {
    spans: Vec<std::ops::Range<usize>>,
    at: usize,
}

impl Vad for SpanVad {
    fn id(&self) -> String {
        "span-vad".into()
    }
    fn frame_len(&self) -> usize {
        480
    }
    fn speech_probability(&mut self, _frame: &[crate::audio::Sample]) -> Result<f32, AsrError> {
        let speech = self.spans.iter().any(|s| s.contains(&self.at));
        self.at += 480;
        Ok(if speech { 1.0 } else { 0.0 })
    }
    fn reset(&mut self) {
        self.at = 0;
    }
}

/// Words `a0`..`a4` (0.3-1.9 s, VAD speech until 2.0 s), a quiet word `late` at
/// 2.3-2.5 s the VAD misses (past the 200 ms speech pad; the utterance closes at 2.52 s),
/// then `b0`..`b2` from `b_ms`; the chunker over them with `config`, partial decodes off
/// until `partials_from` (samples) and on after it.
fn missed_word_stream(
    config: ChunkerConfig,
    partials_from: usize,
    b_ms: u64,
) -> Vec<TranscriptEvent> {
    let mut words = words_at("a", 300, 5);
    words.push(Word::new("late", 2_300, 2_500, 0.9));
    words.extend(words_at("b", b_ms, 3));
    let audio = audio_for(&words, 6_000, 31);
    let vad = SpanVad {
        spans: vec![
            ms_to_frames(300)..ms_to_frames(2_000),
            ms_to_frames(b_ms)..ms_to_frames(b_ms + 900),
        ],
        at: 0,
    };
    let fake = FakeTranscriber::timeline(audio.clone(), words);
    let mut c =
        StreamingChunker::new(fake, vad, config, track(), MeetingTime::ZERO, seq_ids()).unwrap();
    let mut events = Vec::new();
    c.set_partial_decodes(false);
    events.extend(c.push(&audio[..partials_from]).unwrap());
    c.set_partial_decodes(true);
    for part in audio[partials_from..].chunks(480) {
        events.extend(c.push(part).unwrap());
    }
    events.extend(c.finish().unwrap());
    check_stream_contract(&events);
    MISSED_CALLS.with(|calls| {
        *calls.borrow_mut() = c
            .transcriber()
            .calls()
            .iter()
            .map(|k| (k.timeline_offset.unwrap_or(usize::MAX), k.samples))
            .collect();
    });
    events
}

thread_local! {
    /// The decode ranges (timeline offset, samples) of this thread's last
    /// [`missed_word_stream`].
    static MISSED_CALLS: std::cell::RefCell<Vec<(usize, usize)>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

#[test]
fn asr_chunker_partials_never_show_a_word_outside_the_final_window() {
    // A coalesced backlog switches partials back on during the silence that closes an
    // utterance (T6.12a): that partial must decode only what the final will, so it never
    // shows a word (`late`, missed by the VAD past the speech pad) that no final holds.
    // `b` starts late enough that its pre-roll does not reach `late`.
    let events = missed_word_stream(ChunkerConfig::default(), ms_to_frames(2_430), 3_000);
    let shown: Vec<String> = events
        .iter()
        .filter_map(|e| match e {
            TranscriptEvent::Partial(p) => Some(texts(&p.segment.words)),
            _ => None,
        })
        .flatten()
        .collect();
    assert!(!shown.is_empty(), "a partial was decoded in the silence");
    let kept: Vec<String> = finals(&events)
        .iter()
        .flat_map(|s| texts(&s.words))
        .collect();
    for w in &shown {
        assert!(
            kept.contains(w),
            "partial word {w} is in no final: {kept:?}"
        );
    }
    // The finals themselves do not depend on partials (exact decoder).
    let off = missed_word_stream(ChunkerConfig::default(), ms_to_frames(6_000), 3_000);
    let kept_off: Vec<String> = finals(&off).iter().flat_map(|s| texts(&s.words)).collect();
    assert_eq!(kept, kept_off);
}

#[test]
fn asr_chunker_partials_never_commit_audio_the_next_window_decodes_again() {
    // Frequent partials during the closing silence used to agree on `late` past the
    // speech pad and commit it; the next utterance's pre-roll then decoded it again.
    let config = ChunkerConfig {
        partial_interval: Duration::from_millis(30),
        ..ChunkerConfig::default()
    };
    // `b`'s pre-roll (from 2.35 s) holds `late`: it is `b`'s word.
    let events = missed_word_stream(config, 0, 2_550);
    let mut all: Vec<String> = finals(&events)
        .iter()
        .flat_map(|s| texts(&s.words))
        .collect();
    let n = all.len();
    all.sort();
    all.dedup();
    assert_eq!(all.len(), n, "duplicated words: {:?}", finals(&events));
    // The closing silence's partials all have the same range once it passes the speech
    // pad: that range is decoded once by a partial and once by the close, never more.
    let calls = MISSED_CALLS.with(|c| c.borrow().clone());
    for range in &calls {
        let n = calls.iter().filter(|c| *c == range).count();
        assert!(n <= 2, "{range:?} decoded {n} times: {calls:?}");
    }
}

#[test]
fn asr_chunker_cut_in_the_closing_silence_leaves_no_partial_range() {
    // Without overlap a window cut in the closing silence starts past the padded speech
    // end: the next partial has nothing it may decode and is skipped (never a reversed
    // range); the words still reach exactly one final.
    let words = words_at("a", 300, 4);
    let audio = audio_for(&words, 4_000, 37);
    let config = ChunkerConfig {
        max_window: Duration::from_secs(2),
        overlap: Duration::ZERO,
        partial_interval: Duration::from_millis(30),
        ..ChunkerConfig::default()
    };
    let vad = SpanVad {
        spans: std::iter::once(ms_to_frames(300)..ms_to_frames(1_800)).collect(),
        at: 0,
    };
    let fake = FakeTranscriber::timeline(audio.clone(), words.clone());
    let mut c =
        StreamingChunker::new(fake, vad, config, track(), MeetingTime::ZERO, seq_ids()).unwrap();
    let events = run(&mut c, &audio, 480);
    check_stream_contract(&events);
    for call in c.transcriber().calls() {
        assert!(call.samples > 0);
    }
    let all: Vec<String> = finals(&events)
        .iter()
        .flat_map(|s| texts(&s.words))
        .collect();
    assert_eq!(all, texts(&words));
}

#[test]
fn asr_chunker_overlap_is_configurable() {
    let words = words_at("w", 500, 60);
    let audio = audio_for(&words, 26_000, 14);
    for overlap_ms in [0, 400, 2000] {
        let fake = FakeTranscriber::timeline(audio.clone(), words.clone());
        let config = ChunkerConfig {
            overlap: Duration::from_millis(overlap_ms),
            max_window: Duration::from_secs(10),
            ..Default::default()
        };
        let mut c = chunker(fake, config, MeetingTime::ZERO);
        let events = run(&mut c, &audio, 4800);
        check_stream_contract(&events);
        let f = finals(&events);
        let all: Vec<String> = f.iter().flat_map(|s| texts(&s.words)).collect();
        assert_eq!(all, texts(&words), "overlap {overlap_ms} ms");
        // The window after the first cut starts `overlap` before the cut.
        let calls = c.transcriber().calls();
        let cut = (0..calls.len() - 1)
            .find(|&i| calls[i + 1].timeline_offset != calls[i].timeline_offset)
            .expect("a cut");
        assert!(
            calls[cut].samples > ms_to_frames(9_000),
            "first window ends in a cut"
        );
        let cut_end = calls[cut].timeline_offset.unwrap() + calls[cut].samples;
        assert_eq!(
            calls[cut + 1].timeline_offset.unwrap(),
            cut_end - ms_to_frames(overlap_ms),
            "overlap {overlap_ms} ms"
        );
    }
}

#[test]
fn asr_chunker_two_utterances_two_segments_and_prompt_context() {
    let mut words = words_at("a", 1000, 5); // 1.0 .. 2.9 s
    words.extend(words_at("b", 5000, 5)); // 5.0 .. 6.9 s
    let audio = audio_for(&words, 9000, 15);
    let fake = FakeTranscriber::timeline(audio.clone(), words.clone());
    let mut c = chunker(fake, ChunkerConfig::default(), MeetingTime::ZERO);
    let events = run(&mut c, &audio, 333);
    check_stream_contract(&events);
    let f = finals(&events);
    assert_eq!(f.len(), 2);
    assert_ne!(f[0].id, f[1].id);
    assert_eq!(f[0].text, "a0 a1 a2 a3 a4");
    assert_eq!(f[1].text, "b0 b1 b2 b3 b4");
    let last = c.transcriber().calls().last().unwrap();
    assert_eq!(last.options.prompt.as_deref(), Some("a0 a1 a2 a3 a4"));

    // prompt_chars limits the prompt to the most recent characters; 0 disables it.
    for (chars, expected) in [(5, Some("a3 a4")), (4, Some("a4")), (0, None)] {
        let fake = FakeTranscriber::timeline(audio.clone(), words.clone());
        let config = ChunkerConfig {
            prompt_chars: chars,
            ..Default::default()
        };
        let mut c = chunker(fake, config, MeetingTime::ZERO);
        run(&mut c, &audio, 1600);
        assert_eq!(
            c.transcriber()
                .calls()
                .last()
                .unwrap()
                .options
                .prompt
                .as_deref(),
            expected
        );
    }
}

#[test]
fn asr_chunker_events_do_not_depend_on_push_sizes() {
    let mut words = words_at("a", 700, 30);
    words.extend(words_at("b", 14_000, 40));
    let audio = audio_for(&words, 31_000, 16);
    let reference = {
        let fake = FakeTranscriber::timeline(audio.clone(), words.clone())
            .with_unstable_tail(Duration::from_millis(150));
        run(
            &mut chunker(fake, ChunkerConfig::default(), MeetingTime::ZERO),
            &audio,
            audio.len(),
        )
    };
    assert!(!reference.is_empty());
    for chunk in [7, 480, 1601] {
        let fake = FakeTranscriber::timeline(audio.clone(), words.clone())
            .with_unstable_tail(Duration::from_millis(150));
        let events = run(
            &mut chunker(fake, ChunkerConfig::default(), MeetingTime::ZERO),
            &audio,
            chunk,
        );
        assert_eq!(events, reference, "push size {chunk}");
    }
}

#[test]
fn asr_chunker_finish_flushes_open_utterance() {
    let words = words_at("w", 1000, 6); // speech runs to the end of the stream
    let audio = audio_for(&words, 3300, 17);
    let fake = FakeTranscriber::timeline(audio.clone(), words.clone());
    let mut c = chunker(fake, ChunkerConfig::default(), MeetingTime::ZERO);
    let mut events = c.push(&audio).unwrap();
    assert!(finals(&events).is_empty(), "still open before finish");
    events.extend(c.finish().unwrap());
    check_stream_contract(&events);
    assert_eq!(texts(&finals(&events)[0].words), texts(&words));
    // A finished stream accepts more audio and keeps meeting time running.
    let later = c.push(&audio_for(&[], 1000, 18)).unwrap();
    assert!(later.is_empty());
}

#[test]
fn asr_chunker_discards_segment_whose_final_is_empty() {
    // Speech 1.0-2.0 s: one partial decode (at 1.8 s), then the final at utterance end.
    let loud = vec![Word::new("x", 1000, 2000, 1.0)];
    let audio = audio_for(&loud, 3500, 19);
    let fake = FakeTranscriber::scripted([
        Ok(vec![RawSegment::from_words(vec![Word::new(
            "um", 200, 500, 0.4,
        )])]),
        Ok(vec![]),
    ]);
    let mut c = chunker(fake, ChunkerConfig::default(), MeetingTime::ZERO);
    let events = run(&mut c, &audio, 1600);
    check_stream_contract(&events);
    assert_eq!(c.transcriber().calls().len(), 2);
    match events.as_slice() {
        [
            TranscriptEvent::Partial(p),
            TranscriptEvent::Discarded { id },
        ] => {
            assert_eq!(p.segment.id, *id);
            assert_eq!(p.committed_words, 0);
            assert_eq!(p.tentative_text(), "um");
        }
        other => panic!("unexpected events {other:?}"),
    }
}

#[test]
fn asr_chunker_cancellation() {
    let words = words_at("w", 1000, 10);
    let audio = audio_for(&words, 7000, 20);

    // Cancelled from "another thread" before the push: nothing is decoded.
    let mut c = chunker(
        FakeTranscriber::timeline(audio.clone(), words.clone()),
        ChunkerConfig::default(),
        MeetingTime::ZERO,
    );
    let token = c.cancel_token();
    token.cancel();
    assert_eq!(c.push(&audio), Err(AsrError::Cancelled));
    assert_eq!(c.finish(), Err(AsrError::Cancelled));
    assert!(c.transcriber().calls().is_empty());

    // The backend reports cancellation mid-decode: the stream stays cancelled.
    let fake = FakeTranscriber::scripted([Err(AsrError::Cancelled)]);
    let mut c = chunker(fake, ChunkerConfig::default(), MeetingTime::ZERO);
    assert_eq!(c.push(&audio), Err(AsrError::Cancelled));
    assert!(c.cancel_token().is_cancelled());
    assert_eq!(c.push(&audio[..480]), Err(AsrError::Cancelled));
    assert_eq!(c.transcriber().calls().len(), 1);
    assert_eq!(c.backend_info().kind, BackendKind::Fake);
}

#[test]
fn asr_chunker_close_right_after_a_cut_keeps_carried_words() {
    // TODO.md (T2.1 re-review): loud audio ends at 14 s and min_silence is 1.5 s, so the
    // window (opened at 0.3 s) is cut at about 15.3 s, before the utterance closes. The cut
    // moves the window start to 14.3 s, after `last_speech_end + speech_pad` (14.2 s), so
    // the close has an empty range. "tail" (midpoint 14.85 s, after the 14.8 s split) is
    // first seen in the cut decode and carried as tentative: it must reach the final.
    let (audio, mut words) = block_stream(14_000, 20_000, 23);
    words.push(Word::new("tail", 14_700, 15_000, 0.9));
    let config = ChunkerConfig {
        min_silence: Duration::from_millis(1500),
        ..Default::default()
    };
    let fake = FakeTranscriber::timeline(audio.clone(), words.clone());
    let mut c = chunker(fake, config, MeetingTime::ZERO);
    let events = run(&mut c, &audio, 4800);
    check_stream_contract(&events);
    let all: Vec<String> = finals(&events)
        .iter()
        .flat_map(|s| texts(&s.words))
        .collect();
    assert_eq!(all, texts(&words));
}

#[test]
fn asr_chunker_audio_under_carried_words_never_reenters_the_next_window() {
    // Reachable when min_silence > overlap + speech_pad. Speech is loud from 0.8 s to
    // 14 s, so the window opens at 0.3 s (pad 500 ms) and is cut at about 15.3 s; with
    // overlap 200 ms it moves to about 15.1 s, after `last speech + pad` (14.5 s), so the
    // close at about 15.5 s has an empty range. "tail" (midpoint 15.25 s) is decoded by
    // the cut (its end clamped to the cut, about 15.3 s), carried and committed by the
    // close. The next utterance starts at 15.56 s; its 500 ms pre-roll (15.06 s) must not
    // reach back under "tail".
    let next = words_at("n", 15_560, 4);
    let mut loud = vec![Word::new("x", 800, 14_000, 1.0)];
    loud.extend(next.iter().cloned());
    let audio = audio_for(&loud, 20_000, 25);
    let mut words = words_at("w", 800, 33); // w32 ends at 13.9 s
    words.push(Word::new("tail", 15_100, 15_400, 0.9));
    words.extend(next.iter().cloned());
    let config = ChunkerConfig {
        overlap: Duration::from_millis(200),
        min_silence: Duration::from_millis(1500),
        speech_pad: Duration::from_millis(500),
        ..Default::default()
    };
    assert!(config.validate().is_ok());
    let fake = FakeTranscriber::timeline(audio.clone(), words.clone());
    let mut c = chunker(fake, config, MeetingTime::ZERO);
    let events = run(&mut c, &audio, 4800);
    check_stream_contract(&events);
    let segments = finals(&events);
    let all: Vec<String> = segments.iter().flat_map(|s| texts(&s.words)).collect();
    assert_eq!(all, texts(&words));
    let tail = segments
        .iter()
        .flat_map(|s| &s.words)
        .find(|w| w.text == "tail")
        .unwrap();
    let tail_end = ms_to_frames(tail.end.as_millis());
    let second_window = c
        .transcriber()
        .calls()
        .iter()
        .filter_map(|call| call.timeline_offset)
        .filter(|&start| start > ms_to_frames(1_000))
        .collect::<Vec<_>>();
    assert!(!second_window.is_empty());
    for start in second_window {
        assert!(
            start >= tail_end,
            "a window starts at {} ms, under the carried word (ends {} ms)",
            start / 16,
            tail_end / 16
        );
    }
}

#[test]
fn asr_chunker_partial_decodes_can_be_skipped_without_changing_finals() {
    let mut words = words_at("a", 500, 50); // 20 s: one cut
    words.extend(words_at("b", 23_000, 5));
    let audio = audio_for(&words, 27_000, 24);
    let run_with = |partials: bool| {
        let fake = FakeTranscriber::timeline(audio.clone(), words.clone());
        let mut c = chunker(fake, ChunkerConfig::default(), MeetingTime::ZERO);
        c.set_partial_decodes(partials);
        let events = run(&mut c, &audio, 1600);
        check_stream_contract(&events);
        (events, c.transcriber().calls().len())
    };
    let (with, calls_with) = run_with(true);
    let (without, calls_without) = run_with(false);
    assert!(
        without
            .iter()
            .all(|e| !matches!(e, TranscriptEvent::Partial(_)))
    );
    assert_eq!(calls_without, 3, "one cut decode and two closes");
    assert!(calls_with > 20);
    let spans = |ev: &[TranscriptEvent]| -> Vec<(Vec<String>, MeetingTime, MeetingTime)> {
        finals(ev)
            .iter()
            .map(|s| (texts(&s.words), s.start, s.end))
            .collect()
    };
    assert_eq!(spans(&with), spans(&without));

    // Re-enabled after a stretch without partials: the next frame decodes at once (one
    // coalesced partial for everything skipped).
    let fake = FakeTranscriber::timeline(audio.clone(), words.clone());
    let mut c = chunker(fake, ChunkerConfig::default(), MeetingTime::ZERO);
    c.set_partial_decodes(false);
    assert!(c.push(&audio[..ms_to_frames(6000)]).unwrap().is_empty());
    assert!(c.transcriber().calls().is_empty());
    c.set_partial_decodes(true);
    let events = c
        .push(&audio[ms_to_frames(6000)..ms_to_frames(6030)])
        .unwrap();
    assert!(matches!(events.as_slice(), [TranscriptEvent::Partial(_)]));
    assert_eq!(c.transcriber().calls().len(), 1);
}

// ---------------------------------------------------------------- non-speech (T6.7)

#[test]
fn asr_nonspeech_never_reaches_live_finals_or_final_segments() {
    // Live: the chunker's transcriber emits whisper annotations between real words.
    let mut ws = words_at("w", 500, 5);
    ws.push(Word::new("[MUSIC", 2_600, 2_800, 0.9));
    ws.push(Word::new("PLAYING]", 2_800, 2_900, 0.9));
    ws.push(Word::new("[BLANK_AUDIO]", 3_000, 3_300, 0.9));
    ws.sort_by_key(|w| w.start);
    let audio = audio_for(&ws, 6_000, 11);
    let fake = FakeTranscriber::timeline(audio.clone(), ws);
    let mut c = chunker(fake, ChunkerConfig::default(), MeetingTime::ZERO);
    let events = run(&mut c, &audio, 1_600);
    check_stream_contract(&events);
    let live: Vec<String> = finals(&events)
        .iter()
        .flat_map(|s| texts(&s.words))
        .collect();
    assert_eq!(live, ["w0", "w1", "w2", "w3", "w4"]);

    // Final pass: a segment of annotations only is not stored; a mixed one keeps its words.
    let t = Transcription {
        segments: vec![
            RawSegment::from_words(vec![Word::new("[BLANK_AUDIO]", 0, 900, 0.9)]),
            RawSegment::from_words(vec![
                Word::new("Thanks", 1_000, 1_300, 0.9),
                Word::new("(gentle", 1_400, 1_500, 0.9),
                Word::new("music)", 1_500, 1_600, 0.9),
                Word::new("everyone.", 1_700, 2_000, 0.9),
            ]),
        ],
        language: Some("en".into()),
        backend: BackendInfo {
            kind: BackendKind::Fake,
            device: "fake".to_owned(),
            model: "fake".to_owned(),
        },
    };
    let mut ids = seq_ids();
    let segs = t.into_segments(track(), MeetingTime::ZERO, &mut *ids);
    assert_eq!(segs.len(), 1);
    assert_eq!(texts(&segs[0].words), ["Thanks", "everyone."]);
    assert_eq!(segs[0].text, "Thanks everyone.");
}
