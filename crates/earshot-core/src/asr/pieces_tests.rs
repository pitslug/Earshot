//! Tests of `asr::pieces` (T6.16).

use std::time::Duration;

use super::*;
use crate::audio::{MonoBuffer, PIPELINE_RATE, Sample};

/// 10 ms frames; speech wherever any sample is louder than 0.05.
struct LevelVad;

impl Vad for LevelVad {
    fn id(&self) -> String {
        "level-vad".into()
    }
    fn frame_len(&self) -> usize {
        160
    }
    fn speech_probability(&mut self, frame: &[Sample]) -> Result<f32, AsrError> {
        Ok(if frame.iter().any(|s| s.abs() > 0.05) {
            1.0
        } else {
            0.0
        })
    }
    fn reset(&mut self) {}
}

const HZ: usize = 16_000;

/// `spec` is (seconds, speech?) pieces in order.
fn audio(spec: &[(f64, bool)]) -> MonoBuffer {
    let mut s = Vec::new();
    for &(secs, speech) in spec {
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let n = (secs * HZ as f64) as usize;
        s.extend((0..n).map(|i| {
            if speech {
                if i % 2 == 0 { 0.5 } else { -0.5 }
            } else {
                0.0
            }
        }));
    }
    MonoBuffer::new(PIPELINE_RATE, s)
}

fn seg(text: &str, start_ms: u64, end_ms: u64) -> RawSegment {
    RawSegment::from_words(
        text.split_whitespace()
            .enumerate()
            .map(|(i, w)| {
                let i = i as u64;
                Word::new(w, start_ms + i * 10, start_ms + i * 10 + 10, 0.9)
            })
            .map(|mut w| {
                w.end = w.end.min(MeetingTime::from_millis(end_ms));
                w
            })
            .collect(),
    )
}

fn loop_text() -> String {
    "I checked the valve. ".repeat(20)
}

#[test]
fn asr_pieces_compression_ratio_flags_loops_not_speech() {
    assert_eq!(compression_ratio(""), 0.0);
    let normal = "The quarterly numbers came in slightly above the forecast we set in March.";
    assert!(
        compression_ratio(normal) < 2.4,
        "{}",
        compression_ratio(normal)
    );
    assert!(
        compression_ratio(&loop_text()) > 2.4,
        "{}",
        compression_ratio(&loop_text())
    );
    // A sentence said twice is ordinary speech, not a loop.
    let twice = format!("{normal} {normal}");
    assert!(
        compression_ratio(&twice) < 2.4,
        "{}",
        compression_ratio(&twice)
    );
}

#[test]
fn asr_pieces_speech_regions_pad_merge_and_drop_blips() {
    let cfg = PieceConfig::default();
    let a = audio(&[
        (5.0, false),
        (3.0, true),
        (1.0, false),
        (2.0, true),
        (10.0, false),
        (0.1, true),
        (5.0, false),
    ]);
    let probs = frame_probabilities(&mut LevelVad, &a.samples, &CancelToken::new()).unwrap();
    let r = speech_regions(&probs, 160, a.samples.len(), &cfg);
    // The 1 s pause is shorter than min_silence: one region, padded by 0.4 s each side.
    // The 0.1 s blip is shorter than min_speech and is dropped.
    // Speech 5-8 s and 9-11 s -> one region 5-11 s, padded to 4.6-11.4 s.
    assert_eq!(
        r,
        vec![Span {
            start: 73_600,
            end: 182_400
        }]
    );
}

#[test]
fn asr_pieces_silence_only_has_no_regions() {
    let a = audio(&[(60.0, false)]);
    let probs = frame_probabilities(&mut LevelVad, &a.samples, &CancelToken::new()).unwrap();
    assert!(speech_regions(&probs, 160, a.samples.len(), &PieceConfig::default()).is_empty());
}

#[test]
fn asr_pieces_long_speech_is_cut_into_windows_at_the_quietest_frame() {
    let cfg = PieceConfig::default();
    let a = audio(&[(70.0, true)]);
    let mut probs = frame_probabilities(&mut LevelVad, &a.samples, &CancelToken::new()).unwrap();
    // A dip at 25 s (frame 2500) is the cut for the first window.
    probs[2500] = 0.6;
    let regions = speech_regions(&probs, 160, a.samples.len(), &cfg);
    let pieces = plan_pieces(&regions, &probs, 160, &cfg);
    assert_eq!(pieces.first().unwrap().end, 2500 * 160);
    for p in &pieces {
        assert!(p.len() <= 30 * HZ, "piece {p:?} longer than one window");
    }
    for w in pieces.windows(2) {
        assert_eq!(w[0].end, w[1].start, "contiguous cover of the region");
    }
    assert_eq!(pieces.last().unwrap().end, a.samples.len());
}

#[test]
fn asr_pieces_close_regions_share_a_piece_far_ones_do_not() {
    let cfg = PieceConfig {
        min_silence: Duration::from_millis(500),
        speech_pad: Duration::ZERO,
        ..PieceConfig::default()
    };
    let a = audio(&[
        (2.0, true),
        (0.8, false),
        (2.0, true),
        (5.0, false),
        (2.0, true),
    ]);
    let probs = frame_probabilities(&mut LevelVad, &a.samples, &CancelToken::new()).unwrap();
    let regions = speech_regions(&probs, 160, a.samples.len(), &cfg);
    assert_eq!(regions.len(), 3);
    let pieces = plan_pieces(&regions, &probs, 160, &cfg);
    assert_eq!(
        pieces,
        vec![
            Span {
                start: 0,
                end: 4 * HZ + 12_800
            },
            Span {
                start: 9 * HZ + 12_800,
                end: 11 * HZ + 12_800
            }
        ]
    );
}

#[test]
fn asr_pieces_silence_is_never_decoded_and_times_are_shifted() {
    let a = audio(&[
        (20.0, false),
        (3.0, true),
        (20.0, false),
        (3.0, true),
        (5.0, false),
    ]);
    let mut fake = FakeTranscriber::scripted([
        Ok(vec![seg("first piece", 100, 900)]),
        Ok(vec![seg("second piece", 100, 900)]),
    ]);
    let (t, report) = transcribe_in_pieces(
        &mut fake,
        &mut LevelVad,
        &a,
        &TranscribeOptions::batch(),
        &PieceConfig::default(),
        &CancelToken::new(),
    )
    .unwrap();
    assert_eq!(report.pieces, 2);
    assert_eq!(
        fake.calls().len(),
        2,
        "one call per piece, none for silence"
    );
    // Each piece is its speech plus 0.4 s padding: 3.8 s.
    assert!(fake.calls().iter().all(|c| c.samples == 3 * HZ + 12_800));
    let words = t.words();
    assert_eq!(words[0].text, "first");
    assert_eq!(words[0].start, MeetingTime::from_millis(19_600 + 100));
    assert_eq!(words[2].text, "second");
    assert_eq!(words[2].start, MeetingTime::from_millis(42_600 + 100));
}

#[test]
fn asr_pieces_prompt_is_carried_only_across_short_pauses_from_accepted_pieces() {
    // Pieces at 0 s, after a 3 s pause (prompted), after a 30 s pause (not prompted).
    let a = audio(&[
        (3.0, true),
        (3.0, false),
        (3.0, true),
        (30.0, false),
        (3.0, true),
    ]);
    let mut fake = FakeTranscriber::scripted([
        Ok(vec![seg("alpha beta", 0, 500)]),
        Ok(vec![seg("gamma", 0, 500)]),
        Ok(vec![seg("delta", 0, 500)]),
    ]);
    let opts = TranscribeOptions {
        prompt: Some("caller prompt".into()),
        ..TranscribeOptions::batch()
    };
    transcribe_in_pieces(
        &mut fake,
        &mut LevelVad,
        &a,
        &opts,
        &PieceConfig::default(),
        &CancelToken::new(),
    )
    .unwrap();
    let prompts: Vec<_> = fake
        .calls()
        .iter()
        .map(|c| c.options.prompt.clone())
        .collect();
    assert_eq!(
        prompts,
        vec![
            Some("caller prompt".into()),
            Some("alpha beta".into()),
            None
        ]
    );
}

#[test]
fn asr_pieces_a_looping_piece_is_retried_without_prompt_then_greedy() {
    let a = audio(&[(3.0, true), (3.0, false), (3.0, true)]);
    let mut fake = FakeTranscriber::scripted([
        Ok(vec![seg("hello there", 0, 500)]),
        Ok(vec![seg(&loop_text(), 0, 2000)]),
        Ok(vec![seg(&loop_text(), 0, 2000)]),
        Ok(vec![seg("our main risk", 0, 900)]),
    ]);
    let (t, report) = transcribe_in_pieces(
        &mut fake,
        &mut LevelVad,
        &a,
        &TranscribeOptions::batch(),
        &PieceConfig::default(),
        &CancelToken::new(),
    )
    .unwrap();
    assert_eq!(report.retried, 1);
    assert!(report.dropped.is_empty());
    let calls = fake.calls();
    assert_eq!(calls.len(), 4);
    assert_eq!(calls[1].options.prompt.as_deref(), Some("hello there"));
    assert_eq!(calls[2].options.prompt, None);
    assert_eq!(
        calls[2].options.decoding.strategy,
        DecodingStrategy::BeamSearch { beam_size: 5 }
    );
    assert_eq!(calls[3].options.prompt, None);
    assert_eq!(calls[3].options.decoding.strategy, DecodingStrategy::Greedy);
    assert_eq!(join_words(&t.words()), "hello there our main risk");
}

#[test]
fn asr_pieces_a_piece_that_keeps_looping_is_dropped_and_reported() {
    let a = audio(&[
        (3.0, true),
        (30.0, false),
        (3.0, true),
        (30.0, false),
        (3.0, true),
    ]);
    let mut fake = FakeTranscriber::scripted([
        Ok(vec![seg("before", 0, 500)]),
        Ok(vec![seg(&loop_text(), 0, 2000)]),
        Ok(vec![seg(&loop_text(), 0, 2000)]),
        Ok(vec![seg("after", 0, 500)]),
    ]);
    let (t, report) = transcribe_in_pieces(
        &mut fake,
        &mut LevelVad,
        &a,
        &TranscribeOptions::batch(),
        &PieceConfig::default(),
        &CancelToken::new(),
    )
    .unwrap();
    // No prompt on the looping piece (long pause), so one retry: greedy.
    assert_eq!(fake.calls().len(), 4);
    assert_eq!(
        report.dropped,
        vec![(
            MeetingTime::from_millis(32_600),
            MeetingTime::from_millis(36_400)
        )]
    );
    assert_eq!(join_words(&t.words()), "before after");
}

#[test]
fn asr_pieces_greedy_options_are_not_retried_with_the_same_decoder() {
    let a = audio(&[(3.0, true)]);
    let mut fake = FakeTranscriber::scripted([Ok(vec![seg(&loop_text(), 0, 2000)])]);
    let (_, report) = transcribe_in_pieces(
        &mut fake,
        &mut LevelVad,
        &a,
        &TranscribeOptions::live(),
        &PieceConfig::default(),
        &CancelToken::new(),
    )
    .unwrap();
    assert_eq!(fake.calls().len(), 1);
    assert_eq!(report.dropped.len(), 1);
}

#[test]
fn asr_pieces_cancel_and_bad_config_are_errors() {
    let a = audio(&[(3.0, true)]);
    let cancel = CancelToken::new();
    cancel.cancel();
    let mut fake = FakeTranscriber::scripted([]);
    let err = transcribe_in_pieces(
        &mut fake,
        &mut LevelVad,
        &a,
        &TranscribeOptions::batch(),
        &PieceConfig::default(),
        &cancel,
    );
    assert!(matches!(err, Err(AsrError::Cancelled)));
    let bad = PieceConfig {
        max_piece: Duration::from_secs(31),
        ..PieceConfig::default()
    };
    let err = transcribe_in_pieces(
        &mut fake,
        &mut LevelVad,
        &a,
        &TranscribeOptions::batch(),
        &bad,
        &CancelToken::new(),
    );
    assert!(matches!(err, Err(AsrError::Config(_))));
}

/// Seeded noise for `true` stretches (never repeating, so the timeline fake can locate any
/// buffer), zeros for `false`.
fn noise(spec: &[(f64, bool)]) -> MonoBuffer {
    let mut state = 7u64;
    let mut s = Vec::new();
    for &(secs, speech) in spec {
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let n = (secs * HZ as f64) as usize;
        s.extend((0..n).map(|_| {
            if speech {
                state = state
                    .wrapping_mul(6_364_136_223_846_793_005)
                    .wrapping_add(1_442_695_040_888_963_407);
                #[allow(clippy::cast_precision_loss)]
                let u = (state >> 40) as f32 / (1u64 << 24) as f32;
                // |s| in [0.1, 0.5]: every frame is speech for `LevelVad`.
                let mag = 0.1 + 0.4 * u;
                if state & (1 << 20) == 0 { mag } else { -mag }
            } else {
                0.0
            }
        }));
    }
    MonoBuffer::new(PIPELINE_RATE, s)
}

/// A 300 ms word every 500 ms from `from_ms` while it ends by `to_ms`, spelt from its
/// start time (letters that do not compress, so no text looks like a loop).
fn dense_words(from_ms: u64, to_ms: u64) -> Vec<Word> {
    let spell = |s: u64| -> String {
        let mut z = s.wrapping_mul(0x9E37_79B9_7F4A_7C15);
        (0..6)
            .map(|_| {
                z ^= z >> 29;
                z = z.wrapping_mul(0xBF58_476D_1CE4_E5B9);
                char::from(b'a' + u8::try_from(z % 26).unwrap())
            })
            .collect()
    };
    (0..)
        .map(|i| from_ms + i * 500)
        .take_while(|s| s + 300 <= to_ms)
        .map(|s| Word::new(spell(s), s, s + 300, 0.9))
        .collect()
}

/// 1 s silence, 20 s speech, 1 s silence: one piece, 0.6-21.4 s.
const SPEECH_20S: [(f64, bool); 3] = [(1.0, false), (20.0, true), (1.0, false)];
const PIECE_20S: Span = Span {
    start: 9_600,
    end: 342_400,
};

/// The 20 s case on the timeline fake with dense words, `empty` calls answered with
/// nothing, and the fake skipping speech when `skips`.
fn skipping_case(
    empty: &[(usize, usize)],
    skips: bool,
) -> (Transcription, PieceReport, Vec<FakeCall>) {
    let a = noise(&SPEECH_20S);
    let words = dense_words(1000, 21_000);
    let mut fake =
        FakeTranscriber::timeline(a.samples.clone(), words).with_empty_calls(empty.to_vec());
    if skips {
        fake = fake.skipping_speech();
    }
    let (t, report) = transcribe_in_pieces(
        &mut fake,
        &mut LevelVad,
        &a,
        &TranscribeOptions::live(),
        &PieceConfig::default(),
        &CancelToken::new(),
    )
    .unwrap();
    (t, report, fake.calls().to_vec())
}

#[test]
fn asr_pieces_speech_no_word_covers_is_decoded_again_in_halves() {
    // T2.5d: Parakeet TDT 0.6B returns nothing for some whole pieces. Decoding the same
    // input again would repeat that, so the piece is decoded again in two halves cut at
    // the least speech-like frame of its middle half, the one nearest the middle on a tie
    // (all frames are speech here: the middle, 11 s).
    let (t, report, calls) = skipping_case(&[(PIECE_20S.start, PIECE_20S.len())], true);
    let offsets: Vec<(Option<usize>, usize)> = calls
        .iter()
        .map(|c| (c.timeline_offset, c.samples))
        .collect();
    let cut = (PIECE_20S.start + PIECE_20S.end) / 2;
    assert_eq!(cut, 176_000);
    assert_eq!(
        offsets,
        vec![
            (Some(PIECE_20S.start), PIECE_20S.len()),
            (Some(PIECE_20S.start), cut - PIECE_20S.start),
            (Some(cut), PIECE_20S.end - cut),
        ]
    );
    assert!(calls[1..].iter().all(|c| c.options.prompt.is_none()));
    assert_eq!(report.uncovered, 1);
    assert_eq!(report.pieces, 1);
    assert_eq!(
        join_words(&t.words()),
        join_words(&dense_words(1000, 21_000)),
        "every word comes back once, in order"
    );
}

#[test]
fn asr_pieces_uncovered_speech_is_cut_at_its_quietest_frame() {
    // A 0.3 s pause at 8.0-8.3 s (inside the middle half, 5.8-16.2 s): the cut is its
    // frame nearest the middle, 8.29 s.
    let spec = [
        (1.0, false),
        (7.0, true),
        (0.3, false),
        (12.7, true),
        (1.0, false),
    ];
    let a = noise(&spec);
    let mut words = dense_words(1000, 8000);
    words.extend(dense_words(8300, 21_000));
    let mut fake = FakeTranscriber::timeline(a.samples.clone(), words.clone())
        .with_empty_calls([(PIECE_20S.start, PIECE_20S.len())])
        .skipping_speech();
    let (t, _) = transcribe_in_pieces(
        &mut fake,
        &mut LevelVad,
        &a,
        &TranscribeOptions::live(),
        &PieceConfig::default(),
        &CancelToken::new(),
    )
    .unwrap();
    let cut = 829 * 160;
    let offsets: Vec<(Option<usize>, usize)> = fake
        .calls()
        .iter()
        .map(|c| (c.timeline_offset, c.samples))
        .collect();
    assert_eq!(
        offsets,
        vec![
            (Some(PIECE_20S.start), PIECE_20S.len()),
            (Some(PIECE_20S.start), cut - PIECE_20S.start),
            (Some(cut), PIECE_20S.end - cut),
        ]
    );
    assert_eq!(join_words(&t.words()), join_words(&words));
}

#[test]
fn asr_pieces_uncovered_speech_is_left_alone_for_a_transcriber_that_does_not_skip() {
    let (t, report, calls) = skipping_case(&[(PIECE_20S.start, PIECE_20S.len())], false);
    assert_eq!(calls.len(), 1);
    assert_eq!(report.uncovered, 0);
    assert!(t.words().is_empty());
    // Nor when the setting is off.
    let a = noise(&SPEECH_20S);
    let mut fake = FakeTranscriber::timeline(a.samples.clone(), dense_words(1000, 21_000))
        .with_empty_calls([(PIECE_20S.start, PIECE_20S.len())])
        .skipping_speech();
    let off = PieceConfig {
        recover_speech: None,
        ..PieceConfig::default()
    };
    let (_, report) = transcribe_in_pieces(
        &mut fake,
        &mut LevelVad,
        &a,
        &TranscribeOptions::live(),
        &off,
        &CancelToken::new(),
    )
    .unwrap();
    assert_eq!((fake.calls().len(), report.uncovered), (1, 0));
}

/// The 20 s case with the reference words starting in `missing_ms` taken out: the calls
/// made and the report.
fn gap_case(missing_ms: std::ops::Range<u64>) -> (Vec<FakeCall>, PieceReport) {
    let a = noise(&SPEECH_20S);
    let words: Vec<Word> = dense_words(1000, 21_000)
        .into_iter()
        .filter(|w| !missing_ms.contains(&w.start.as_millis()))
        .collect();
    let mut fake = FakeTranscriber::timeline(a.samples.clone(), words).skipping_speech();
    let (_, report) = transcribe_in_pieces(
        &mut fake,
        &mut LevelVad,
        &a,
        &TranscribeOptions::live(),
        &PieceConfig::default(),
        &CancelToken::new(),
    )
    .unwrap();
    (fake.calls().to_vec(), report)
}

#[test]
fn asr_pieces_only_more_uncovered_speech_than_the_setting_is_decoded_again() {
    // Words at 4.5 s (ending 4.8 s) and 6.5 s: 1.7 s of speech between them.
    let (calls, report) = gap_case(5000..6500);
    assert_eq!((calls.len(), report.uncovered), (1, 0));
    // Words at 4.5 s and 7.0 s: the 2.2 s of speech between them is decoded again, on its
    // own first (an input other than the piece).
    let (calls, report) = gap_case(5000..7000);
    assert_eq!(report.uncovered, 1);
    assert_eq!(calls[1].timeline_offset, Some(4_800 * 16));
    assert_eq!(calls[1].samples, 2_200 * 16);
}

#[test]
fn asr_pieces_decoding_uncovered_speech_again_is_bounded() {
    // Nothing is ever recognised: the piece, then halves of halves down to three levels
    // (1 + 2 + 4 + 8 calls), none shorter than a second.
    let a = noise(&SPEECH_20S);
    let mut fake = FakeTranscriber::timeline(a.samples.clone(), Vec::new()).skipping_speech();
    let (t, report) = transcribe_in_pieces(
        &mut fake,
        &mut LevelVad,
        &a,
        &TranscribeOptions::live(),
        &PieceConfig::default(),
        &CancelToken::new(),
    )
    .unwrap();
    assert!(t.words().is_empty());
    assert_eq!(report.uncovered, 1);
    assert_eq!(fake.calls().len(), 1 + 2 + 4 + 8);
    assert!(fake.calls().iter().all(|c| c.samples >= HZ));
}

#[test]
fn asr_pieces_recovered_words_split_the_segment_around_them() {
    // The piece's one segment has words up to 2.7 s and from 15 s (piece time); the 12.3 s
    // of speech between them is decoded on its own and comes back as its own segment
    // between the two halves of the first, in time order and without overlap.
    let a = noise(&SPEECH_20S);
    let mut first = dense_words(400, 3000);
    first.extend(dense_words(15_000, 20_400));
    let mut fake = FakeTranscriber::scripted([
        Ok(vec![RawSegment::from_words(first)]),
        Ok(vec![RawSegment::from_words(dense_words(0, 12_300))]),
    ])
    .skipping_speech();
    let (t, report) = transcribe_in_pieces(
        &mut fake,
        &mut LevelVad,
        &a,
        &TranscribeOptions::live(),
        &PieceConfig::default(),
        &CancelToken::new(),
    )
    .unwrap();
    assert_eq!(report.uncovered, 1);
    let calls = fake.calls();
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[1].samples, 12_300 * 16);
    let starts: Vec<u64> = t.segments.iter().map(|s| s.start.as_millis()).collect();
    assert_eq!(starts, vec![600 + 400, 600 + 2_700, 600 + 15_000]);
    for w in t.segments.windows(2) {
        assert!(w[0].end <= w[1].start, "{:?} overlaps {:?}", w[0], w[1]);
    }
    let words = t.words();
    for w in words.windows(2) {
        assert!(w[0].start < w[1].start, "words in time order");
    }
    assert_eq!(words.len(), 5 + 25 + 11);
    for s in &t.segments {
        assert_eq!(
            s.text,
            join_words(&s.words),
            "segment text follows its words"
        );
    }
}

#[test]
fn asr_pieces_an_empty_answer_for_uncovered_speech_changes_nothing() {
    // The same gap, but every decode of it answers with an empty segment: the piece's one
    // segment stays whole and nothing is added.
    let a = noise(&SPEECH_20S);
    let mut first = dense_words(400, 3000);
    first.extend(dense_words(15_000, 20_400));
    let piece_segment = RawSegment::from_words(first);
    let empty = || Ok(vec![RawSegment::from_words(Vec::new())]);
    let mut fake = FakeTranscriber::scripted(
        std::iter::once(Ok(vec![piece_segment.clone()]))
            .chain(std::iter::repeat_with(empty).take(15)),
    )
    .skipping_speech();
    let (t, report) = transcribe_in_pieces(
        &mut fake,
        &mut LevelVad,
        &a,
        &TranscribeOptions::live(),
        &PieceConfig::default(),
        &CancelToken::new(),
    )
    .unwrap();
    assert_eq!(report.uncovered, 1);
    assert_eq!(t.segments.len(), 1);
    assert_eq!(t.segments[0].text, piece_segment.text);
    assert_eq!(t.words().len(), 5 + 11);
}

#[test]
fn asr_pieces_recover_speech_must_be_positive() {
    let bad = PieceConfig {
        recover_speech: Some(Duration::ZERO),
        ..PieceConfig::default()
    };
    assert!(matches!(bad.validate(), Err(AsrError::Config(_))));
}

#[test]
fn asr_pieces_speech_between_words_measures_every_uncovered_stretch() {
    // Words at 0.5-1.0 s and 3.0-3.5 s over 0-4 s; 10 ms frames, speech from 1.5 s to
    // 2.5 s and from 3.6 s on.
    let segments = vec![
        RawSegment::from_words(vec![Word::new("a", 500, 1_000, 0.9)]),
        RawSegment::from_words(vec![Word::new("b", 3_000, 3_500, 0.9)]),
    ];
    let probs: Vec<f32> = (0..400)
        .map(|k| {
            if (150..250).contains(&k) || k >= 360 {
                0.9
            } else {
                0.1
            }
        })
        .collect();
    let span = Span {
        start: 0,
        end: 64_000,
    };
    let gaps = speech_between_words(&segments, span, &probs, 160, 0.5);
    assert_eq!(
        gaps,
        [
            (
                Span {
                    start: 0,
                    end: 8_000
                },
                0
            ),
            (
                Span {
                    start: 16_000,
                    end: 48_000
                },
                16_000
            ),
            (
                Span {
                    start: 56_000,
                    end: 64_000
                },
                6_400
            ),
        ]
    );
    assert!(speech_between_words(&segments, span, &probs, 0, 0.5).is_empty());
}
