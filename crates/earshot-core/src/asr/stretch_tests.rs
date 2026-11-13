//! Tests of `asr::stretch` (T6.13a). Every name contains `asr_stretch`.

use std::time::Duration;

use super::pieces::{cut_point, samples as sample_count};
use super::stretch::*;
use super::*;
use crate::asr::transcriber::{BackendKind, Language, Transcription};
use crate::asr::types::Word;
use crate::asr::{DecodingOptions, DecodingStrategy, transcribe_in_pieces};
use crate::audio::{MonoBuffer, PIPELINE_RATE, Sample};

/// 10 ms frames; the probability is the frame's peak level (so loud means speech).
struct LevelVad;

impl Vad for LevelVad {
    fn id(&self) -> String {
        "level-vad".into()
    }
    fn frame_len(&self) -> usize {
        160
    }
    fn speech_probability(&mut self, frame: &[Sample]) -> Result<f32, AsrError> {
        Ok(frame.iter().fold(0.0f32, |m, s| m.max(s.abs())).min(1.0))
    }
    fn reset(&mut self) {}
}

const HZ: usize = 16_000;

/// Kinds of audio in a spec.
#[derive(Clone, Copy)]
enum K {
    Silence,
    Speech,
    /// Speech whose decode loops when given a prompt (a retry without one fixes it).
    LoopsWithPrompt,
    /// Speech whose every decode loops (the piece is dropped).
    AlwaysLoops,
    /// Speech a skipping [`Recorder`] returns nothing for when a call longer than
    /// [`SKIPPED_OVER`] starts in it (T2.5d).
    Skipped,
}

/// A skipping [`Recorder`] call starting in [`K::Skipped`] audio returns nothing when it is
/// longer than this.
const SKIPPED_OVER: usize = 4 * HZ;

fn audio(spec: &[(f64, K)]) -> Vec<f32> {
    let mut s = Vec::new();
    for &(secs, kind) in spec {
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let n = (secs * HZ as f64) as usize;
        let level = match kind {
            K::Silence => 0.0,
            K::Speech => 0.6,
            K::LoopsWithPrompt => 0.8,
            K::AlwaysLoops => 0.95,
            K::Skipped => 0.5,
        };
        s.extend((0..n).map(|i| if i % 2 == 0 { level } else { -level }));
    }
    s
}

/// Records every call and answers from the audio: words carry the call's length and peak,
/// loops as the spec says. A skipping recorder ([`Recorder::skipping`]) reports
/// [`Transcriber::may_skip_speech`] and answers with a word every 500 ms of the buffer
/// instead (so its words cover the speech), except for [`K::Skipped`] calls.
#[derive(Default)]
struct Recorder {
    calls: Vec<(usize, Option<String>, DecodingStrategy)>,
    skips: bool,
}

impl Recorder {
    fn skipping() -> Self {
        Self {
            calls: Vec::new(),
            skips: true,
        }
    }
}

/// A word that does not compress, from `n`.
fn spelt(n: u64) -> String {
    let mut z = n.wrapping_mul(0x9E37_79B9_7F4A_7C15);
    (0..6)
        .map(|_| {
            z ^= z >> 29;
            z = z.wrapping_mul(0xBF58_476D_1CE4_E5B9);
            char::from(b'a' + u8::try_from(z % 26).unwrap())
        })
        .collect()
}

impl Transcriber for Recorder {
    fn transcribe(
        &mut self,
        audio: &MonoBuffer,
        options: &TranscribeOptions,
        cancel: &CancelToken,
    ) -> Result<Transcription, AsrError> {
        if cancel.is_cancelled() {
            return Err(AsrError::Cancelled);
        }
        self.calls.push((
            audio.samples.len(),
            options.prompt.clone(),
            options.decoding.strategy,
        ));
        let peak = audio.samples.iter().fold(0.0f32, |m, s| m.max(s.abs()));
        let loops = peak > 0.9 || (peak > 0.7 && options.prompt.is_some());
        if self.skips && !loops {
            let len_ms = (audio.samples.len() / 16) as u64;
            let skipped = audio.samples.len() > SKIPPED_OVER
                && audio
                    .samples
                    .iter()
                    .find(|s| **s != 0.0)
                    .is_some_and(|s| (s.abs() - 0.5).abs() < 1e-6);
            let call = self.calls.len() as u64;
            let words: Vec<Word> = if skipped {
                Vec::new()
            } else {
                (0..len_ms.saturating_sub(300) / 500 + u64::from(len_ms >= 300))
                    .map(|i| Word::new(spelt(call * 1000 + i), i * 500, i * 500 + 300, 0.9))
                    .collect()
            };
            return Ok(Transcription {
                segments: vec![RawSegment::from_words(words)],
                language: Some("en".into()),
                backend: self.backend_info(),
            });
        }
        let text = if loops {
            "I checked the valve. ".repeat(20)
        } else {
            format!(
                "piece {} level {:.2} call {}",
                audio.samples.len(),
                peak,
                self.calls.len()
            )
        };
        let words: Vec<Word> = text
            .split_whitespace()
            .enumerate()
            .map(|(i, w)| {
                let i = i as u64;
                Word::new(w, i * 20, i * 20 + 10, 0.9)
            })
            .collect();
        Ok(Transcription {
            segments: vec![RawSegment::from_words(words)],
            language: Some("en".into()),
            backend: self.backend_info(),
        })
    }

    fn backend_info(&self) -> BackendInfo {
        BackendInfo {
            kind: BackendKind::Cpu,
            device: "test".into(),
            model: "recorder".into(),
        }
    }

    fn may_skip_speech(&self) -> bool {
        self.skips
    }
}

fn beam() -> TranscribeOptions {
    TranscribeOptions::batch()
}

/// The stream over `samples` in pushes of `push` on the recorder `t`: every piece, the
/// summary, the calls, and how many pieces came out before `finish`.
fn stream_on(
    mut t: Recorder,
    samples: &[f32],
    push: usize,
    options: &TranscribeOptions,
    config: &PieceConfig,
) -> (Vec<StreamPiece>, StreamSummary, Recorder, usize) {
    let mut vad = LevelVad;
    let cancel = CancelToken::new();
    let mut s = PieceStream::new(&t, &mut vad, options, config).unwrap();
    let mut pieces = Vec::new();
    for chunk in samples.chunks(push.max(1)) {
        pieces.extend(s.push(&mut t, &mut vad, chunk, &cancel).unwrap());
    }
    let early = pieces.len();
    let (rest, summary) = s.finish(&mut t, &mut vad, &cancel).unwrap();
    pieces.extend(rest);
    (pieces, summary, t, early)
}

/// The whole-track pass over `samples` on the recorder `t`.
fn batch_on(
    mut t: Recorder,
    samples: &[f32],
    options: &TranscribeOptions,
    config: &PieceConfig,
) -> (Transcription, PieceReport, Recorder) {
    let (tr, report) = transcribe_in_pieces(
        &mut t,
        &mut LevelVad,
        &MonoBuffer::new(PIPELINE_RATE, samples.to_vec()),
        options,
        config,
        &CancelToken::new(),
    )
    .unwrap();
    (tr, report, t)
}

fn assert_equivalent(spec: &[(f64, K)], options: &TranscribeOptions, config: &PieceConfig) {
    assert_equivalent_on(false, spec, options, config);
}

/// [`assert_equivalent`] on a skipping recorder when `skips`.
fn assert_equivalent_on(
    skips: bool,
    spec: &[(f64, K)],
    options: &TranscribeOptions,
    config: &PieceConfig,
) {
    let recorder = || Recorder {
        calls: Vec::new(),
        skips,
    };
    let samples = audio(spec);
    let (want, want_report, want_calls) = batch_on(recorder(), &samples, options, config);
    for push in [1usize, 160, 517, 16_000, samples.len().max(1)] {
        let (pieces, summary, calls, _) = stream_on(recorder(), &samples, push, options, config);
        let segments: Vec<RawSegment> = pieces.iter().flat_map(|p| p.segments.clone()).collect();
        assert_eq!(calls.calls, want_calls.calls, "calls, push {push}");
        assert_eq!(segments, want.segments, "segments, push {push}");
        assert_eq!(summary.report, want_report, "report, push {push}");
        assert_eq!(summary.language, want.language, "language, push {push}");
        assert_eq!(summary.backend, want.backend, "backend, push {push}");
        let dropped: Vec<(MeetingTime, MeetingTime)> = pieces
            .iter()
            .filter(|p| p.dropped)
            .map(|p| (p.start(), p.end()))
            .collect();
        assert_eq!(dropped, want_report.dropped, "dropped, push {push}");
        let uncovered = pieces.iter().filter(|p| p.uncovered).count();
        assert_eq!(uncovered, want_report.uncovered, "uncovered, push {push}");
    }
}

/// Specs where a skipping recorder leaves speech uncovered (T2.5d): pieces starting in
/// [`K::Skipped`] audio, one inside a long region cut into windows, next to the other
/// kinds.
fn skipping_cases() -> Vec<Vec<(f64, K)>> {
    vec![
        vec![
            (1.0, K::Silence),
            (3.0, K::Speech),
            (15.0, K::Silence),
            (12.0, K::Skipped),
            (3.0, K::Speech),
            (15.0, K::Silence),
            (3.0, K::Skipped),
            (3.0, K::Silence),
        ],
        vec![(2.0, K::Silence), (40.0, K::Skipped), (35.0, K::Speech)],
        vec![
            (3.0, K::Speech),
            (2.5, K::Silence),
            (3.0, K::LoopsWithPrompt),
            (15.0, K::Silence),
            (25.0, K::Skipped),
            (3.0, K::Silence),
            (3.0, K::AlwaysLoops),
        ],
    ]
}

#[test]
fn asr_stretch_equals_the_whole_track_pass_when_uncovered_speech_is_decoded_again() {
    let config = PieceConfig::default();
    let mut uncovered = 0;
    for spec in skipping_cases() {
        assert_equivalent_on(true, &spec, &beam(), &config);
        let (t, report, _) = batch_on(Recorder::skipping(), &audio(&spec), &beam(), &config);
        uncovered += report.uncovered;
        // Every skipped stretch got words back.
        assert!(!t.words().is_empty());
    }
    assert!(uncovered >= 3, "not vacuous: {uncovered}");
}

#[test]
fn asr_stretch_equals_the_whole_track_pass_on_the_pieces_cases() {
    let config = PieceConfig::default();
    // Speech, short pauses (merged), a long pause (new piece, no prompt carried), silence
    // only at the ends.
    assert_equivalent(
        &[
            (1.0, K::Silence),
            (3.0, K::Speech),
            (0.5, K::Silence),
            (2.0, K::Speech),
            (15.0, K::Silence),
            (4.0, K::Speech),
            (3.0, K::Silence),
        ],
        &beam(),
        &config,
    );
    // Long speech cut into windows; speech running to the very end.
    assert_equivalent(&[(2.0, K::Silence), (75.0, K::Speech)], &beam(), &config);
    // A piece that loops with a prompt (retried) and one that always loops (dropped).
    assert_equivalent(
        &[
            (3.0, K::Speech),
            (2.5, K::Silence),
            (3.0, K::LoopsWithPrompt),
            (3.0, K::Silence),
            (3.0, K::AlwaysLoops),
            (3.0, K::Silence),
            (2.0, K::Speech),
        ],
        &beam(),
        &config,
    );
    // Greedy decoding has no greedy retry; a caller's prompt goes to the first piece only.
    let greedy = TranscribeOptions {
        decoding: DecodingOptions {
            strategy: DecodingStrategy::Greedy,
            ..DecodingOptions::BATCH
        },
        prompt: Some("Earshot".into()),
        language: Language::Code("en".into()),
    };
    assert_equivalent(
        &[
            (3.0, K::LoopsWithPrompt),
            (2.5, K::Silence),
            (3.0, K::Speech),
        ],
        &greedy,
        &config,
    );
    // A length that is not a whole number of VAD frames: the partial last frame is decoded
    // at `finish`, zero padded, as the whole-track pass does.
    assert_equivalent(&[(1.0, K::Silence), (3.0023, K::Speech)], &beam(), &config);
    assert_eq!(
        audio(&[(1.0, K::Silence), (3.0023, K::Speech)]).len() % 160,
        36
    );
    // Silence only, and nothing at all.
    assert_equivalent(&[(10.0, K::Silence)], &beam(), &config);
    assert_equivalent(&[], &beam(), &config);
}

/// A tiny deterministic generator (no rand dependency in core tests).
struct Lcg(u64);

impl Lcg {
    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        self.0 >> 33
    }
}

#[test]
fn asr_stretch_settled_pieces_are_always_a_prefix_of_the_final_plan() {
    let frame = 160;
    let configs = [
        PieceConfig::default(),
        PieceConfig {
            max_piece: Duration::from_secs(2),
            merge_gap: Duration::from_millis(3000),
            min_silence: Duration::from_millis(200),
            speech_pad: Duration::from_millis(50),
            ..PieceConfig::default()
        },
        PieceConfig {
            max_piece: Duration::from_secs(5),
            merge_gap: Duration::ZERO,
            min_speech: Duration::ZERO,
            speech_pad: Duration::ZERO,
            ..PieceConfig::default()
        },
        PieceConfig {
            max_piece: Duration::from_secs(3),
            min_silence: Duration::from_millis(1000),
            speech_pad: Duration::from_millis(600),
            merge_gap: Duration::from_millis(400),
            ..PieceConfig::default()
        },
    ];
    let mut rng = Lcg(42);
    let (mut checked, mut early) = (0, 0);
    for config in &configs {
        for _ in 0..200 {
            // Runs of speech, near-threshold and silence of random lengths.
            let mut probs: Vec<f32> = Vec::new();
            while probs.len() < 3000 {
                let len = 1 + (rng.next() % 400) as usize;
                let p = match rng.next() % 5 {
                    0 | 1 => 0.0,
                    2 => 0.4,
                    3 => 0.9,
                    _ => (rng.next() % 100) as f32 / 100.0,
                };
                probs.extend(std::iter::repeat_n(p, len));
            }
            let extra = (rng.next() % frame as u64) as usize;
            let total = probs.len() * frame + extra;
            let regions = speech_regions(&probs, frame, total, config);
            let plan = plan_pieces(&regions, &probs, frame, config);
            let mut previous: Vec<Span> = Vec::new();
            for n in (0..=probs.len()).step_by(7) {
                let settled = settled_pieces(&probs[..n], frame, config);
                assert!(
                    settled.len() <= plan.len() && settled[..] == plan[..settled.len()],
                    "not a prefix at frame {n}: {settled:?} vs {plan:?} ({config:?})"
                );
                // Settled pieces never go away.
                assert!(
                    settled.len() >= previous.len() && settled[..previous.len()] == previous[..]
                );
                if n < probs.len() && !settled.is_empty() {
                    early += 1;
                }
                previous = settled;
                checked += 1;
            }
        }
    }
    assert!(checked > 10_000);
    // Not vacuous: most prefixes already have settled pieces.
    assert!(
        early * 2 > checked,
        "{early} of {checked} prefixes had settled pieces"
    );
}

/// A cheap config for sweeps over every stream length (T12.31): windows of `max_piece`, no
/// padding, every region kept.
fn small_windows(max_piece: Duration) -> PieceConfig {
    PieceConfig {
        max_piece,
        min_speech: Duration::ZERO,
        min_silence: Duration::from_millis(300),
        speech_pad: Duration::ZERO,
        merge_gap: Duration::from_millis(200),
        ..PieceConfig::default()
    }
}

/// Checks, for every stream length of `probs`, that the settled pieces are a prefix of the
/// whole-track plan and never go away. Returns the plan and, per plan piece, the number of
/// frames heard when it first settled (`None`: only at the end).
fn sweep_every_length(
    probs: &[f32],
    frame: usize,
    config: &PieceConfig,
) -> (Vec<Span>, Vec<Option<usize>>) {
    let total = probs.len() * frame;
    let regions = speech_regions(probs, frame, total, config);
    let plan = plan_pieces(&regions, probs, frame, config);
    let mut first: Vec<Option<usize>> = vec![None; plan.len()];
    let mut previous = 0;
    for n in 0..=probs.len() {
        let settled = settled_pieces(&probs[..n], frame, config);
        assert!(
            settled.len() <= plan.len() && settled[..] == plan[..settled.len()],
            "not a prefix at frame {n} (frame {frame}): {settled:?} vs {plan:?} ({config:?})"
        );
        assert!(
            settled.len() >= previous,
            "a settled piece went away at {n}"
        );
        for slot in &mut first[previous..settled.len()] {
            *slot = Some(n);
        }
        previous = settled.len();
    }
    (plan, first)
}

/// Pieces shorter than a window that settled before the window after them was heard, while
/// the speech after them had not paused, although the next piece starts where they end: cut
/// pieces released without waiting for the next cut. (A piece of exactly one window never
/// waits, nothing can join it; nor does a cut piece of a region that has ended.)
fn released_before_the_next_window(
    plan: &[Span],
    first: &[Option<usize>],
    probs: &[f32],
    frame: usize,
    config: &PieceConfig,
) -> usize {
    let max = sample_count(config.max_piece);
    plan.windows(2)
        .zip(first)
        .filter(|(pair, at)| {
            pair[1].start == pair[0].end
                && pair[0].len() < max
                && at.is_some_and(|n| {
                    n * frame < pair[0].end + max
                        && probs[pair[0].end / frame..n]
                            .iter()
                            .all(|p| *p >= config.threshold - 0.15)
                })
        })
        .count()
}

#[test]
fn asr_stretch_a_cut_piece_settles_when_its_own_window_is_heard() {
    // Unbroken speech with the least speech-like frame of each 2 s window in its middle:
    // pieces of 1.5 s (150 frames), cut at frames 150, 300, 450, ...
    let frame = 160;
    let config = small_windows(Duration::from_secs(2));
    let mut probs = vec![0.6f32; 1000];
    for dip in (150..1000).step_by(150) {
        probs[dip] = 0.45;
    }
    let (plan, first) = sweep_every_length(&probs, frame, &config);
    assert!(plan.len() >= 6, "{plan:?}");
    for (i, piece) in plan.iter().take(5).enumerate() {
        assert_eq!(
            (piece.start, piece.end),
            (i * 150 * frame, (i + 1) * 150 * frame)
        );
        // Its window is frames `150 i ..< 150 i + 200`; the region is known to run past it
        // one frame later. Waiting for the next window would be frame `150 (i + 1) + 201`.
        assert_eq!(first[i], Some(150 * i + 201), "piece {i}");
    }

    // The same through a stream: each piece is decoded within one settle interval of its
    // window, not a whole piece later.
    let samples: Vec<f32> = probs
        .iter()
        .flat_map(|&p| (0..frame).map(move |i| if i % 2 == 0 { p } else { -p }))
        .collect();
    let mut t = Recorder::default();
    let mut vad = LevelVad;
    let cancel = CancelToken::new();
    let mut s = PieceStream::new(&t, &mut vad, &beam(), &config).unwrap();
    let mut seen: Vec<(Span, usize)> = Vec::new();
    for chunk in samples.chunks(frame) {
        for p in s.push(&mut t, &mut vad, chunk, &cancel).unwrap() {
            seen.push((p.span, s.pushed() / frame));
        }
    }
    assert!(seen.len() >= 5, "{seen:?}");
    for (i, (span, heard)) in seen.iter().enumerate() {
        assert_eq!(*span, plan[i]);
        assert!(
            *heard <= 150 * i + 201 + SETTLE_EVERY_FRAMES,
            "piece {i} came out after {heard} frames"
        );
        assert_eq!(s.decoded_through().min(span.end), span.end);
    }
}

#[test]
fn asr_stretch_a_cut_piece_is_held_while_the_next_cut_could_join_it() {
    // The tie: a region starting on a frame, a window whose half is a whole number of
    // frames and every frame equally speech-like, so each cut falls on its first candidate
    // frame and two cut parts are exactly one window: the plan joins them.
    let frame = 160;
    let config = small_windows(Duration::from_secs(1));
    let max = 16_000;
    let probs = vec![0.9f32; 700];
    let (plan, first) = sweep_every_length(&probs, frame, &config);
    assert!(plan.len() >= 6, "{plan:?}");
    for piece in &plan[..plan.len() - 1] {
        assert_eq!(piece.len(), max, "{plan:?}");
    }
    // Such a piece settles when its second cut is known (the window from frame
    // `100 i + 50`), and then at once: the cut after it is at least half a window on.
    assert_eq!(
        released_before_the_next_window(&plan, &first, &probs, frame, &config),
        0
    );
    for (i, at) in first.iter().take(5).enumerate() {
        assert_eq!(*at, Some(100 * i + 151), "piece {i}");
    }

    // One later frame less speech-like than the first candidate of the second window: the
    // second cut cannot be the first candidate, but the stream does not know that until the
    // window is heard, so the first part is still held, and the plan no longer joins them.
    let mut probs = vec![0.9f32; 700];
    probs[120] = 0.5;
    let (plan, first) = sweep_every_length(&probs, frame, &config);
    assert_eq!((plan[0].start, plan[0].end), (0, 50 * frame));
    assert_eq!(plan[1].end, 120 * frame);
    assert_eq!(first[0], Some(151));
    assert_eq!(first[1], Some(151));
}

#[test]
fn asr_stretch_no_cut_is_earlier_than_the_earliest_cut() {
    let mut rng = Lcg(31);
    let (mut reached, mut later) = (0, 0);
    for (frame, max) in [
        (160usize, 16_000usize),
        (160, 16_001),
        (512, 16_000),
        (512, 480_000),
        (160, 480_000),
        (7, 16_000),
        (40_000, 16_000),
    ] {
        for _ in 0..300 {
            let s = (rng.next() % 100_000) as usize;
            let s = if rng.next().is_multiple_of(2) {
                s / frame * frame
            } else {
                s
            };
            let frames = (s + max) / frame + 2;
            let earliest = earliest_cut(s, frame, max);
            assert!(earliest > s && earliest <= s + max);
            // Constant probabilities cut at the first candidate; any others no earlier.
            let flat = vec![0.9f32; frames];
            assert_eq!(cut_point(s, &flat, frame, max), earliest);
            let levels: Vec<f32> = (0..frames).map(|_| (rng.next() % 5) as f32 / 5.0).collect();
            let known = (rng.next() % (frames as u64 + 1)) as usize;
            for probs in [&levels[..], &levels[..known], &[]] {
                let cut = cut_point(s, probs, frame, max);
                assert!(cut >= earliest, "{cut} < {earliest} ({s}, {frame}, {max})");
                if cut == earliest {
                    reached += 1;
                } else {
                    later += 1;
                }
            }
        }
    }
    assert!(reached > 100 && later > 100, "{reached} {later}");
}

#[test]
fn asr_stretch_settled_cut_pieces_are_a_prefix_of_the_plan_at_every_length() {
    let even = Duration::from_secs(1);
    // 16,001 samples: an odd window, whose two halves are one sample short of it.
    let odd = Duration::from_nanos(1_000_062_500);
    assert_eq!(sample_count(odd), 16_001);
    let configs = [
        small_windows(even),
        small_windows(odd),
        // Padding, and a merge gap longer than the window (pieces join across regions).
        PieceConfig {
            speech_pad: Duration::from_millis(50),
            merge_gap: Duration::from_millis(3000),
            ..small_windows(even)
        },
        // Padding that is not a whole number of frames: windows start off the frame grid.
        PieceConfig {
            speech_pad: Duration::from_micros(33_125),
            min_speech: Duration::from_millis(100),
            ..small_windows(even)
        },
    ];
    let (mut checked, mut released, mut inputs) = (0usize, 0usize, 0usize);
    let mut run = |probs: &[f32], frame: usize, config: &PieceConfig| {
        let (plan, first) = sweep_every_length(probs, frame, config);
        released += released_before_the_next_window(&plan, &first, probs, frame, config);
        checked += probs.len() + 1;
        inputs += 1;
    };
    for config in &configs {
        for frame in [160usize, 512] {
            // Speech that ends just past a cut (and further on), then a pause too short and
            // one long enough to end the region, then more speech. `dip` moves the first
            // cut off its first candidate frame; with none, the cuts tie.
            let window = sample_count(config.max_piece) / frame;
            for speech in (window + 1..=2 * window + 30).step_by(3) {
                for dip in [None, Some(1), Some(window / 4), Some(window / 2 - 2)] {
                    for pause in [8usize, 80] {
                        let mut probs = vec![0.9f32; speech];
                        if let Some(d) = dip {
                            probs[window / 2 + d] = 0.5;
                        }
                        probs.extend(std::iter::repeat_n(0.0, pause));
                        probs.extend(std::iter::repeat_n(0.9, window + window / 2));
                        probs.extend(std::iter::repeat_n(0.0, 40));
                        run(&probs, frame, config);
                    }
                }
            }
            // Long runs of speech at a few levels (so least speech-like frames tie), with
            // rare pauses.
            let mut rng = Lcg(7 + frame as u64);
            for _ in 0..12 {
                let mut probs: Vec<f32> = Vec::new();
                while probs.len() < 6 * window {
                    let len = 1 + (rng.next() % (window as u64 / 2)) as usize;
                    let p = match rng.next() % 12 {
                        0 => 0.0,
                        1..=4 => 0.9,
                        5..=7 => 0.6,
                        8 | 9 => 0.4,
                        _ => 0.5 + (rng.next() % 4) as f32 / 10.0,
                    };
                    probs.extend(std::iter::repeat_n(p, len));
                }
                run(&probs, frame, config);
            }
        }
    }
    assert!(inputs > 1000 && checked > 200_000, "{inputs} {checked}");
    // Not vacuous: cut pieces were released before the window after them was heard.
    assert!(released > 1000, "{released} cut pieces released early");
}

#[test]
fn asr_stretch_pieces_arrive_during_the_stream_not_only_at_the_end() {
    let config = PieceConfig::default();
    // A speaker who never pauses for 95 s: pieces of at most 30 s come out while they talk.
    let samples = audio(&[(95.0, K::Speech)]);
    let mut t = Recorder::default();
    let mut vad = LevelVad;
    let cancel = CancelToken::new();
    let mut s = PieceStream::new(&t, &mut vad, &beam(), &config).unwrap();
    let mut seen: Vec<(f64, f64)> = Vec::new();
    for chunk in samples.chunks(1600) {
        for p in s.push(&mut t, &mut vad, chunk, &cancel).unwrap() {
            let now = s.pushed() as f64 / HZ as f64;
            let end = p.span.end as f64 / HZ as f64;
            assert!(p.span.len() <= 30 * HZ, "{p:?}");
            seen.push((end, now));
        }
    }
    assert!(seen.len() >= 2, "{seen:?}");
    // Each cut is settled once the cut window has been heard: within 30 s of the piece end.
    for (end, now) in &seen {
        assert!(
            now - end <= 30.0,
            "piece ending at {end} s came out at {now} s"
        );
    }
    // Turns separated by pauses: each turn comes out about min_silence + speech_pad after it
    // ends, plus at most one settle interval and the 1 s merge gap.
    let samples = audio(&[
        (5.0, K::Speech),
        (6.0, K::Silence),
        (5.0, K::Speech),
        (6.0, K::Silence),
    ]);
    let mut t = Recorder::default();
    let mut s = PieceStream::new(&t, &mut vad, &beam(), &config).unwrap();
    let mut seen = Vec::new();
    for chunk in samples.chunks(160) {
        for p in s.push(&mut t, &mut vad, chunk, &cancel).unwrap() {
            seen.push((p.span.end as f64 / HZ as f64, s.pushed() as f64 / HZ as f64));
        }
    }
    assert_eq!(seen.len(), 2, "{seen:?}");
    for (end, now) in &seen {
        assert!(now - end <= 2.0 + 0.4 + 1.0 + 0.1, "{seen:?}");
    }
}

#[test]
fn asr_stretch_cancel_and_bad_config_are_errors() {
    let mut t = Recorder::default();
    let mut vad = LevelVad;
    let bad = PieceConfig {
        max_piece: Duration::from_secs(40),
        ..PieceConfig::default()
    };
    assert!(matches!(
        PieceStream::new(&t, &mut vad, &beam(), &bad),
        Err(AsrError::Config(_))
    ));
    let cancel = CancelToken::new();
    let mut s = PieceStream::new(&t, &mut vad, &beam(), &PieceConfig::default()).unwrap();
    cancel.cancel();
    assert!(matches!(
        s.push(&mut t, &mut vad, &audio(&[(1.0, K::Speech)]), &cancel),
        Err(AsrError::Cancelled)
    ));
}

#[test]
fn asr_stretch_frontier_only_passes_speech_that_is_decoded() {
    let config = PieceConfig::default();
    let samples = audio(&[
        (2.0, K::Silence),
        (5.0, K::Speech),
        (6.0, K::Silence),
        (40.0, K::Speech),
        (3.0, K::Silence),
    ]);
    let mut t = Recorder::default();
    let mut vad = LevelVad;
    let cancel = CancelToken::new();
    let mut s = PieceStream::new(&t, &mut vad, &beam(), &config).unwrap();
    let mut decoded: Vec<Span> = Vec::new();
    let mut last = 0;
    for chunk in samples.chunks(800) {
        decoded.extend(
            s.push(&mut t, &mut vad, chunk, &cancel)
                .unwrap()
                .iter()
                .map(|p| p.span),
        );
        let f = s.decoded_through();
        assert!(f >= last, "the frontier never moves back");
        assert!(f <= s.pushed());
        last = f;
        // Every speech sample before the frontier lies in a decoded piece.
        for (i, v) in samples[..f].iter().enumerate() {
            if v.abs() > 0.5 {
                assert!(
                    decoded.iter().any(|p| p.start <= i && i < p.end),
                    "speech at {i} before the frontier {f} is not decoded"
                );
            }
        }
    }
    // It moves during the stream, not only at the end.
    assert!(last > 7 * HZ, "frontier {last}");
    let (_, _) = s.finish(&mut t, &mut vad, &cancel).unwrap();
}

/// The specs of `asr_stretch_equals_the_whole_track_pass_on_the_pieces_cases` with the
/// options each runs with.
fn resume_cases() -> Vec<(Vec<(f64, K)>, TranscribeOptions)> {
    let greedy = TranscribeOptions {
        decoding: DecodingOptions {
            strategy: DecodingStrategy::Greedy,
            ..DecodingOptions::BATCH
        },
        prompt: Some("Earshot".into()),
        language: Language::Code("en".into()),
    };
    vec![
        (
            vec![
                (1.0, K::Silence),
                (3.0, K::Speech),
                (0.5, K::Silence),
                (2.0, K::Speech),
                (15.0, K::Silence),
                (4.0, K::Speech),
                (3.0, K::Silence),
            ],
            beam(),
        ),
        (vec![(2.0, K::Silence), (75.0, K::Speech)], beam()),
        (
            vec![
                (3.0, K::Speech),
                (2.5, K::Silence),
                (3.0, K::LoopsWithPrompt),
                (3.0, K::Silence),
                (3.0, K::AlwaysLoops),
                (3.0, K::Silence),
                (2.0, K::Speech),
            ],
            beam(),
        ),
        (
            vec![
                (3.0, K::LoopsWithPrompt),
                (2.5, K::Silence),
                (3.0, K::Speech),
            ],
            greedy,
        ),
        (vec![(1.0, K::Silence), (3.0023, K::Speech)], beam()),
        (vec![(10.0, K::Silence)], beam()),
        (vec![], beam()),
    ]
}

#[test]
fn asr_stretch_resume_equals_the_whole_track_pass_wherever_the_stream_stopped() {
    // T6.13c: what a stream decoded before it stopped (or finished), plus the rest decoded
    // by `transcribe_rest_in_pieces`, is the whole-track pass: same segments, report and
    // language, and the rest makes exactly the whole pass's remaining calls.
    let resumed_mid_stream = check_resume(false, resume_cases());
    // Not vacuous: many stops had pieces on both sides.
    assert!(resumed_mid_stream > 20, "{resumed_mid_stream}");
}

#[test]
fn asr_stretch_resume_equals_the_whole_track_pass_when_uncovered_speech_is_decoded_again() {
    // T2.5d: the same with pieces whose uncovered speech is decoded again (the report's
    // `uncovered` count and the prompt text after such a piece are restored from the
    // stored pieces).
    let cases = skipping_cases()
        .into_iter()
        .map(|spec| (spec, beam()))
        .collect();
    let resumed_mid_stream = check_resume(true, cases);
    assert!(resumed_mid_stream > 20, "{resumed_mid_stream}");
}

/// The resume check of `asr_stretch_resume_equals_the_whole_track_pass_wherever_the_stream
/// _stopped` over `cases`, on a skipping recorder when `skips`: how many stops resumed with
/// pieces on both sides.
fn check_resume(skips: bool, cases: Vec<(Vec<(f64, K)>, TranscribeOptions)>) -> usize {
    let config = PieceConfig::default();
    let recorder = || Recorder {
        calls: Vec::new(),
        skips,
    };
    let mut resumed_mid_stream = 0;
    for (spec, options) in cases {
        let samples = audio(&spec);
        let (want, want_report, want_calls) = batch_on(recorder(), &samples, &options, &config);
        let buffer = MonoBuffer::new(PIPELINE_RATE, samples.clone());
        for push in [517usize, 16_000] {
            let chunks: Vec<&[f32]> = samples.chunks(push).collect();
            // Stopped after `k` pushes (about 50 points spread over the stream, and the
            // last), or `None`: finished normally.
            let step = chunks.len().div_ceil(50).max(1);
            let stops = (0..=chunks.len())
                .step_by(step)
                .chain([chunks.len()])
                .map(Some)
                .chain([None]);
            for stop in stops {
                let mut live = recorder();
                let mut vad = LevelVad;
                let cancel = CancelToken::new();
                let mut s = PieceStream::new(&live, &mut vad, &options, &config).unwrap();
                let mut done = DecodedPieces::default();
                for chunk in &chunks[..stop.unwrap_or(chunks.len())] {
                    done.pieces
                        .extend(s.push(&mut live, &mut vad, chunk, &cancel).unwrap());
                    done.through = s.decoded_through();
                }
                if stop.is_none() {
                    let (rest, _) = s.finish(&mut live, &mut vad, &cancel).unwrap();
                    done.pieces.extend(rest);
                    done.through = samples.len();
                }
                // The record survives storage unchanged.
                let json = serde_json::to_string(&done).unwrap();
                let done: DecodedPieces = serde_json::from_str(&json).unwrap();
                // The fake numbers its answers by call: the rest continues the count.
                let live_calls = live.calls.len();
                let mut rest = Recorder {
                    calls: live.calls,
                    skips,
                };
                let (got, report) = transcribe_rest_in_pieces(
                    &mut rest,
                    &mut LevelVad,
                    &buffer,
                    &options,
                    &config,
                    &done,
                    &cancel,
                )
                .unwrap();
                let at = format!("push {push}, stop {stop:?}, spec {}", spec.len());
                assert_eq!(got.segments, want.segments, "segments, {at}");
                assert_eq!(report, want_report, "report, {at}");
                assert_eq!(got.language, want.language, "language, {at}");
                assert_eq!(rest.calls, want_calls.calls, "calls, {at}");
                if stop.is_none() {
                    assert_eq!(
                        rest.calls.len(),
                        live_calls,
                        "nothing left after finish, {at}"
                    );
                } else if !done.pieces.is_empty() && rest.calls.len() > live_calls {
                    resumed_mid_stream += 1;
                }
            }
        }
    }
    resumed_mid_stream
}

#[test]
fn asr_stretch_resume_keeps_the_stored_pieces_when_the_plans_differ() {
    // The stored pieces came from slightly different audio (in production: the live stream
    // before 16-bit FLAC quantisation): they are kept as they are, and only plan pieces
    // ending after the frontier are decoded, clipped to start there, so no audio is decoded
    // twice and no speech after the frontier is missed.
    let config = PieceConfig::default();
    let options = beam();
    let live_audio = audio(&[
        (1.0, K::Silence),
        (3.0, K::Speech),
        (6.0, K::Silence),
        (4.0, K::Speech),
        (6.0, K::Silence),
    ]);
    // The recording: the first turn ends 200 ms later, the second starts 300 ms earlier.
    let recorded = audio(&[
        (1.0, K::Silence),
        (3.2, K::Speech),
        (5.5, K::Silence),
        (4.3, K::Speech),
        (6.0, K::Silence),
    ]);
    let mut live = Recorder::default();
    let mut vad = LevelVad;
    let cancel = CancelToken::new();
    let mut s = PieceStream::new(&live, &mut vad, &options, &config).unwrap();
    let mut done = DecodedPieces::default();
    // Stop at 9.5 s: the first turn is decoded, the second is not.
    for chunk in live_audio[..19 * HZ / 2].chunks(1_600) {
        done.pieces
            .extend(s.push(&mut live, &mut vad, chunk, &cancel).unwrap());
        done.through = s.decoded_through();
    }
    assert_eq!(done.pieces.len(), 1);
    let stored: Vec<RawSegment> = done.pieces[0].segments.clone();
    let probs = frame_probabilities(&mut LevelVad, &recorded, &cancel).unwrap();
    let plan = plan_pieces(
        &speech_regions(&probs, 160, recorded.len(), &config),
        &probs,
        160,
        &config,
    );
    assert_ne!(plan[0], done.pieces[0].span, "the plans differ");
    let expected: Vec<usize> = plan
        .iter()
        .filter(|p| p.end > done.through)
        .map(|p| p.end - p.start.max(done.through))
        .collect();
    assert!(!expected.is_empty());
    let mut rest = Recorder::default();
    let (got, report) = transcribe_rest_in_pieces(
        &mut rest,
        &mut LevelVad,
        &MonoBuffer::new(PIPELINE_RATE, recorded),
        &options,
        &config,
        &done,
        &cancel,
    )
    .unwrap();
    assert_eq!(got.segments[..stored.len()], stored[..]);
    let lengths: Vec<usize> = rest.calls.iter().map(|c| c.0).collect();
    assert_eq!(lengths, expected);
    assert_eq!(report.pieces, 1 + expected.len());
    for s in &got.segments[stored.len()..] {
        assert!(s.start.as_millis() as usize * 16 >= done.through, "{s:?}");
    }
}

/// Speech (`true`, peak about 0.6) and silence spans given in samples, every speech sample
/// distinct (a hash of its index), so a buffer handed to a transcriber can be found in the
/// recording.
fn distinct(spec: &[(usize, bool)]) -> Vec<f32> {
    let mut out = Vec::new();
    for &(n, speech) in spec {
        for _ in 0..n {
            let i = out.len() as u64;
            if !speech {
                out.push(0.0);
                continue;
            }
            let h = i.wrapping_add(1).wrapping_mul(0x9E37_79B9_7F4A_7C15) >> 40;
            let u = h as f32 / (1u64 << 24) as f32;
            let sign = if i.is_multiple_of(2) { 1.0 } else { -1.0 };
            out.push(sign * 0.6 * (0.5 + 0.5 * u));
        }
    }
    out
}

/// Answers like [`Recorder`] and records where each call's audio lies in `recording`
/// (offset, length).
struct Locator {
    recording: Vec<f32>,
    inner: Recorder,
    calls: Vec<(usize, usize)>,
}

impl Transcriber for Locator {
    fn transcribe(
        &mut self,
        audio: &MonoBuffer,
        options: &TranscribeOptions,
        cancel: &CancelToken,
    ) -> Result<Transcription, AsrError> {
        let needle = &audio.samples;
        // Checked first at the loudest sample, which is distinct.
        let m = (0..needle.len())
            .max_by(|&a, &b| needle[a].abs().total_cmp(&needle[b].abs()))
            .unwrap_or(0);
        let at = (0..=self.recording.len() - needle.len())
            .find(|&i| {
                self.recording[i + m] == needle[m]
                    && self.recording[i..i + needle.len()] == **needle
            })
            .expect("the call's audio lies in the recording");
        self.calls.push((at, needle.len()));
        self.inner.transcribe(audio, options, cancel)
    }

    fn backend_info(&self) -> BackendInfo {
        self.inner.backend_info()
    }
}

#[test]
fn asr_stretch_resume_starts_a_piece_that_straddles_the_frontier_at_the_frontier() {
    // The live stream stopped with its frontier inside the pause after the first turn; in
    // the recording a second turn starts before that frontier and ends after it, so the
    // whole-track plan has a piece straddling the frontier. The rest is decoded from the
    // frontier exactly: nothing before it again, nothing of that piece after it missed.
    let config = PieceConfig::default();
    let options = beam();
    let live_audio = distinct(&[(HZ, false), (3 * HZ, true), (16 * HZ, false)]);
    let mut live = Recorder::default();
    let mut vad = LevelVad;
    let cancel = CancelToken::new();
    let mut s = PieceStream::new(&live, &mut vad, &options, &config).unwrap();
    let mut done = DecodedPieces::default();
    for chunk in live_audio[..19 * HZ / 2].chunks(1_600) {
        done.pieces
            .extend(s.push(&mut live, &mut vad, chunk, &cancel).unwrap());
        done.through = s.decoded_through();
    }
    assert_eq!(done.pieces.len(), 1);
    let through = done.through;
    let first_end = done.pieces[0].span.end;
    assert!(through >= first_end + 2 * HZ, "{first_end} {through}");
    // The recording: the same first turn, then a 3 s turn starting 0.5 s before the
    // frontier.
    let second = through - HZ / 2;
    let recording = distinct(&[
        (HZ, false),
        (3 * HZ, true),
        (second - 4 * HZ, false),
        (3 * HZ, true),
        (6 * HZ, false),
    ]);
    let buffer = MonoBuffer::new(PIPELINE_RATE, recording.clone());
    let locator = || Locator {
        recording: recording.clone(),
        inner: Recorder::default(),
        calls: Vec::new(),
    };

    // The whole-track pass on the recording has a piece across the frontier.
    let mut whole = locator();
    transcribe_in_pieces(
        &mut whole,
        &mut LevelVad,
        &buffer,
        &options,
        &config,
        &cancel,
    )
    .unwrap();
    let straddling = *whole
        .calls
        .iter()
        .find(|&&(at, len)| at < through && through < at + len)
        .unwrap_or_else(|| panic!("no piece straddles {through}: {:?}", whole.calls));

    let mut rest = locator();
    let (got, report) = transcribe_rest_in_pieces(
        &mut rest,
        &mut LevelVad,
        &buffer,
        &options,
        &config,
        &done,
        &cancel,
    )
    .unwrap();
    let (at, len) = rest.calls[0];
    assert_eq!(
        at, through,
        "the first call starts at the frontier: {:?}",
        rest.calls
    );
    assert_eq!(
        at + len,
        straddling.0 + straddling.1,
        "and ends where the straddling piece ends"
    );
    // No call reaches back before the frontier, calls do not overlap, and every part of the
    // whole pass's pieces after the frontier is decoded.
    let mut end = through;
    for &(at, len) in &rest.calls {
        assert!(at >= end, "{:?}", rest.calls);
        end = at + len;
    }
    for &(w_at, w_len) in &whole.calls {
        for k in w_at.max(through)..w_at + w_len {
            assert!(
                rest.calls
                    .iter()
                    .any(|&(at, len)| (at..at + len).contains(&k)),
                "sample {k} of a whole-pass piece is not decoded: {:?}",
                rest.calls
            );
        }
    }
    // The stored piece's words come first, unchanged.
    let stored = &done.pieces[0].segments;
    assert_eq!(got.segments[..stored.len()], stored[..]);
    assert_eq!(report.pieces, 1 + rest.calls.len());
}

#[test]
fn asr_stretch_resume_refuses_an_inconsistent_record() {
    let samples = audio(&[(3.0, K::Speech), (3.0, K::Silence)]);
    let buffer = MonoBuffer::new(PIPELINE_RATE, samples.clone());
    let piece = |start: usize, end: usize| StreamPiece {
        span: Span { start, end },
        segments: Vec::new(),
        dropped: false,
        retried: false,
        uncovered: false,
        language: None,
    };
    let bad = [
        // A frontier past the end of the recording.
        DecodedPieces {
            pieces: Vec::new(),
            through: samples.len() + 1,
        },
        // A piece past the frontier.
        DecodedPieces {
            pieces: vec![piece(0, 2_000)],
            through: 1_000,
        },
        // Overlapping pieces.
        DecodedPieces {
            pieces: vec![piece(0, 2_000), piece(1_000, 3_000)],
            through: 4_000,
        },
        // An empty piece.
        DecodedPieces {
            pieces: vec![piece(500, 500)],
            through: 4_000,
        },
    ];
    for done in bad {
        let r = transcribe_rest_in_pieces(
            &mut Recorder::default(),
            &mut LevelVad,
            &buffer,
            &beam(),
            &PieceConfig::default(),
            &done,
            &CancelToken::new(),
        );
        assert!(matches!(r, Err(AsrError::Config(_))), "{done:?}: {r:?}");
    }
}
