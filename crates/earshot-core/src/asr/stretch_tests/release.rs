//! Tests that a [`PieceStream`] lets go of audio no piece can still need (T14.20), and that
//! doing so changes nothing it decodes. Every name contains `asr_stretch`.
//!
//! What a stream gives out is pinned two ways: against the whole-track pass on the same
//! audio (same calls with the same samples, same segments), and against a digest of every
//! observable value of the stream (each piece as stored, each frontier, each call's
//! samples) captured from the commit before the stream released anything.

use super::*;

/// Kinds of audio in a [`noisy`] spec.
#[derive(Clone, Copy, PartialEq, Debug)]
enum N {
    /// Below the VAD's lower threshold.
    Quiet,
    /// Between the thresholds: keeps speech going, never starts it.
    Murmur,
    /// Speech.
    Loud,
}

/// Audio where every sample is distinct (a hash of its index and `seed`), silence
/// included, so a buffer cut at the wrong place never equals the right one.
fn noisy(spec: &[(usize, N)], seed: u64) -> Vec<f32> {
    let mut out = Vec::with_capacity(spec.iter().map(|s| s.0).sum());
    for &(n, kind) in spec {
        for _ in 0..n {
            out.push(noisy_sample(out.len(), kind, seed));
        }
    }
    out
}

fn noisy_sample(index: usize, kind: N, seed: u64) -> f32 {
    let i = index as u64;
    let h = (i ^ seed.rotate_left(17))
        .wrapping_add(1)
        .wrapping_mul(0x9E37_79B9_7F4A_7C15)
        >> 40;
    let u = h as f32 / (1u64 << 24) as f32;
    let sign = if i.is_multiple_of(2) { 1.0 } else { -1.0 };
    let peak = match kind {
        N::Quiet => 0.05,
        N::Murmur => 0.42,
        N::Loud => 0.7,
    };
    // At least 0.9 of the peak, so every 10 ms frame of a kind reads as that kind.
    sign * peak * (0.9 + 0.1 * u)
}

const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;

fn fnv(h: &mut u64, bytes: &[u8]) {
    for b in bytes {
        *h ^= u64::from(*b);
        *h = h.wrapping_mul(0x0100_0000_01b3);
    }
}

fn samples_hash(samples: &[f32]) -> u64 {
    let mut h = FNV_OFFSET;
    for s in samples {
        fnv(&mut h, &s.to_bits().to_le_bytes());
    }
    h
}

/// One call: a hash of the exact samples, their count, the prompt and the strategy.
type ExactCall = (u64, usize, Option<String>, DecodingStrategy);

/// Records every call's exact samples (as a hash) and answers from them.
#[derive(Default)]
struct Exact {
    calls: Vec<ExactCall>,
}

impl Transcriber for Exact {
    fn transcribe(
        &mut self,
        audio: &MonoBuffer,
        options: &TranscribeOptions,
        cancel: &CancelToken,
    ) -> Result<Transcription, AsrError> {
        if cancel.is_cancelled() {
            return Err(AsrError::Cancelled);
        }
        let h = samples_hash(&audio.samples);
        self.calls.push((
            h,
            audio.samples.len(),
            options.prompt.clone(),
            options.decoding.strategy,
        ));
        let call = self.calls.len() as u64;
        let words = vec![
            Word::new(spelt(h), 0, 30, 0.9),
            Word::new(spelt(call), 40, 70, 0.8),
        ];
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
            model: "exact".into(),
        }
    }
}

/// The configs of `asr_stretch_settled_pieces_are_always_a_prefix_of_the_final_plan`.
fn configs() -> [PieceConfig; 4] {
    [
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
    ]
}

/// The most audio a stream may hold after any push, in samples: two windows (the piece
/// that may still grow and the window being heard), the padding, the shortest region and
/// the silence that closes one, and the frames heard since pieces were last looked for.
/// Nothing in it grows with the length of the stream.
fn held_bound(config: &PieceConfig, frame: usize) -> usize {
    2 * sample_count(config.max_piece)
        + sample_count(config.speech_pad)
        + sample_count(config.min_speech)
        + sample_count(config.min_silence)
        + SETTLE_EVERY_FRAMES * frame
}

/// A random meeting for `config`: speech from blips to several windows, murmurs, and
/// silences from a breath to several windows, in sample counts that are no multiple of a
/// VAD frame.
fn random_spec(rng: &mut Lcg, config: &PieceConfig) -> Vec<(usize, N)> {
    let window = sample_count(config.max_piece);
    let mut pick = |lo: usize, hi: usize| lo + (rng.next() as usize) % (hi - lo);
    let mut spec = Vec::new();
    let parts = 8 + pick(0, 8);
    for _ in 0..parts {
        let kind = pick(0, 10);
        spec.push(match kind {
            0..=2 => (pick(HZ / 20, 3 * HZ), N::Quiet),
            3 | 4 => (pick(3 * HZ, window + 5 * HZ), N::Quiet),
            5 => (pick(window, 3 * window + HZ), N::Quiet),
            6 => (pick(HZ / 10, 2 * HZ), N::Murmur),
            7 => (pick(HZ / 10, HZ / 2), N::Loud),
            8 => (pick(HZ / 2, window), N::Loud),
            _ => (pick(window, 2 * window + window / 2), N::Loud),
        });
    }
    spec
}

/// Push sizes that put the pushes, and so the releases, at many different points: single
/// samples, parts of a frame, frames, seconds and whole windows.
fn random_pushes(rng: &mut Lcg, total: usize) -> Vec<usize> {
    let mut out = Vec::new();
    let mut left = total;
    while left > 0 {
        let n = match rng.next() % 12 {
            0 => 1 + (rng.next() as usize) % 5,
            1 | 2 => 1 + (rng.next() as usize) % 159,
            3 | 4 => 160,
            5 | 6 => 517,
            7 | 8 => 1 + (rng.next() as usize) % 4000,
            9 | 10 => 16_000,
            _ => 1 + (rng.next() as usize) % 100_000,
        }
        .min(left);
        out.push(n);
        left -= n;
    }
    out
}

/// Everything one stream run shows: fed to `digest`, and returned for the comparison with
/// the whole-track pass.
struct Run {
    pieces: Vec<StreamPiece>,
    summary: StreamSummary,
    calls: Vec<ExactCall>,
    /// The most audio held after any push.
    most_held: usize,
    /// How many pushes ended holding less than everything pushed and not yet decoded
    /// before them would have been without a release (the stream let go of silence).
    early: usize,
}

fn run_stream(
    samples: &[f32],
    pushes: &[usize],
    options: &TranscribeOptions,
    config: &PieceConfig,
    digest: &mut u64,
) -> Run {
    let mut t = Exact::default();
    let mut vad = LevelVad;
    let cancel = CancelToken::new();
    let mut s = PieceStream::new(&t, &mut vad, options, config).unwrap();
    let mut pieces: Vec<StreamPiece> = Vec::new();
    let (mut most_held, mut early) = (0, 0);
    let mut at = 0;
    for &n in pushes {
        let mut seen: Vec<(StreamPiece, usize)> = Vec::new();
        s.push_each(
            &mut t,
            &mut vad,
            &samples[at..at + n],
            &cancel,
            &mut |p, f| {
                seen.push((p, f));
            },
        )
        .unwrap();
        at += n;
        for (p, f) in seen {
            fnv(digest, serde_json::to_string(&p).unwrap().as_bytes());
            fnv(digest, &(f as u64).to_le_bytes());
            pieces.push(p);
        }
        fnv(digest, &(s.decoded_through() as u64).to_le_bytes());
        fnv(digest, &(s.pushed() as u64).to_le_bytes());
        fnv(digest, format!("{:?}", s.report()).as_bytes());
        assert_eq!(s.pushed(), at);
        // Never less than what a piece not yet decoded may need: everything from the
        // frontier on.
        assert!(
            s.held() >= at - s.decoded_through(),
            "held {} with {at} pushed and the frontier at {}",
            s.held(),
            s.decoded_through()
        );
        let bound = held_bound(config, vad.frame_len());
        assert!(
            s.held() <= bound,
            "held {} samples after {at}, bound {bound} ({config:?})",
            s.held()
        );
        most_held = most_held.max(s.held());
        let last_end = pieces.last().map_or(0, |p| p.span.end);
        if s.held() < at - last_end {
            early += 1;
        }
    }
    let mut seen: Vec<(StreamPiece, usize)> = Vec::new();
    let summary = s
        .finish_each(&mut t, &mut vad, &cancel, &mut |p, f| seen.push((p, f)))
        .unwrap();
    for (p, f) in seen {
        fnv(digest, serde_json::to_string(&p).unwrap().as_bytes());
        fnv(digest, &(f as u64).to_le_bytes());
        pieces.push(p);
    }
    fnv(digest, format!("{summary:?}").as_bytes());
    fnv(digest, format!("{:?}", t.calls).as_bytes());
    Run {
        pieces,
        summary,
        calls: t.calls,
        most_held,
        early,
    }
}

/// The digest of every observable value of the streams of
/// `asr_stretch_releasing_audio_changes_nothing_a_stream_gives_out`, captured at the commit
/// before a stream released any audio outside decoded pieces (43de71c). A change to it
/// means the stream decodes or reports something else than it did: never update it for a
/// change that is meant to alter only what is held.
const TRACE_DIGEST: u64 = 0xdeb0_59bf_6541_6315;

#[test]
fn asr_stretch_releasing_audio_changes_nothing_a_stream_gives_out() {
    let options = TranscribeOptions {
        prompt: Some("Earshot".into()),
        ..beam()
    };
    let mut rng = Lcg(0x7414_2000);
    let mut digest = FNV_OFFSET;
    let (mut pieces_seen, mut early, mut cut_pieces) = (0, 0, 0);
    for config in &configs() {
        for case in 0..12u64 {
            let spec = random_spec(&mut rng, config);
            let samples = noisy(&spec, case);
            let buffer = MonoBuffer::new(PIPELINE_RATE, samples.clone());
            let mut whole = Exact::default();
            let (want, want_report) = transcribe_in_pieces(
                &mut whole,
                &mut LevelVad,
                &buffer,
                &options,
                config,
                &CancelToken::new(),
            )
            .unwrap();
            let probs = frame_probabilities(&mut LevelVad, &samples, &CancelToken::new()).unwrap();
            let plan = plan_pieces(
                &speech_regions(&probs, 160, samples.len(), config),
                &probs,
                160,
                config,
            );
            for _ in 0..3 {
                let pushes = random_pushes(&mut rng, samples.len());
                let run = run_stream(&samples, &pushes, &options, config, &mut digest);
                let at = format!("case {case}, {} pushes, {config:?}", pushes.len());
                // The same calls with the same samples, in the same order.
                assert_eq!(run.calls, whole.calls, "calls, {at}");
                let spans: Vec<Span> = run.pieces.iter().map(|p| p.span).collect();
                assert_eq!(spans, plan, "pieces, {at}");
                let segments: Vec<RawSegment> =
                    run.pieces.iter().flat_map(|p| p.segments.clone()).collect();
                assert_eq!(segments, want.segments, "segments, {at}");
                assert_eq!(run.summary.report, want_report, "report, {at}");
                assert_eq!(run.summary.language, want.language, "language, {at}");
                assert_eq!(run.summary.backend, want.backend, "backend, {at}");
                // Each call's samples are the piece's own, by position in the recording.
                for (p, call) in run.pieces.iter().zip(&run.calls) {
                    assert_eq!(
                        call.0,
                        samples_hash(&samples[p.span.start..p.span.end]),
                        "samples of {:?}, {at}",
                        p.span
                    );
                }

                assert!(run.most_held <= held_bound(config, 160));
                pieces_seen += run.pieces.len();
                early += run.early;
                let max = sample_count(config.max_piece);
                cut_pieces += plan
                    .windows(2)
                    .filter(|w| w[0].end == w[1].start && w[0].len() <= max)
                    .count();
            }
        }
    }
    // Not vacuous: many pieces, regions cut into windows, and silence let go of early.
    assert!(pieces_seen > 300, "{pieces_seen} pieces");
    assert!(cut_pieces > 40, "{cut_pieces} cut pieces");
    assert!(
        early > 1000,
        "{early} pushes held less than the undecoded audio"
    );
    assert_eq!(
        digest, TRACE_DIGEST,
        "the streams gave out something else than before audio was released: {digest:#018x}"
    );
}

#[test]
fn asr_stretch_no_piece_still_to_come_starts_before_the_frontier() {
    // What the release rests on: the frontier `settle` gives is not only past every decoded
    // speech sample, no piece of the plan of any longer stream that is not settled yet
    // starts before it, padding and merging included. So audio before it is never needed.
    let frame = 160;
    let mut rng = Lcg(77);
    let (mut checked, mut moved) = (0, 0);
    for config in &configs() {
        for _ in 0..25 {
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
            let mut highest = 0;
            for n in (0..=probs.len()).step_by(7) {
                let (settled, frontier) = settle(&probs[..n], frame, config);
                assert!(frontier <= n * frame, "a frontier past the audio heard");
                if frontier > highest {
                    highest = frontier;
                    moved += 1;
                }
                // The stream as it ends now (with and without a last partial frame), a
                // little later, and at the end of the track.
                let later = [
                    n,
                    (n + 1 + (rng.next() % 40) as usize).min(probs.len()),
                    (n + 1 + (rng.next() % 2000) as usize).min(probs.len()),
                    probs.len(),
                ];
                for m in later {
                    for extra in [0, (rng.next() % frame as u64) as usize] {
                        // A partial last frame is one more probability over `extra` samples.
                        let (heard, total) = if extra > 0 && m < probs.len() {
                            (m + 1, m * frame + extra)
                        } else {
                            (m, m * frame)
                        };
                        let regions = speech_regions(&probs[..heard], frame, total, config);
                        let plan = plan_pieces(&regions, &probs[..heard], frame, config);
                        assert!(
                            settled.len() <= plan.len() && settled[..] == plan[..settled.len()]
                        );
                        for piece in &plan[settled.len()..] {
                            // The highest frontier so far holds too: what was true of
                            // every piece not settled then is true of those left now.
                            assert!(
                                piece.start >= highest,
                                "{piece:?} starts before the frontier {highest} \
                                 ({n} frames heard, {heard} at the end, {config:?})"
                            );
                            checked += 1;
                        }
                    }
                }
            }
        }
    }
    assert!(checked > 100_000, "{checked}");
    assert!(moved > 5_000, "{moved}");
}

/// 100 ms frames, so an hour is cheap to look through; the probability is the frame's
/// peak level.
struct CoarseVad;

impl Vad for CoarseVad {
    fn id(&self) -> String {
        "coarse-vad".into()
    }
    fn frame_len(&self) -> usize {
        1600
    }
    fn speech_probability(&mut self, frame: &[Sample]) -> Result<f32, AsrError> {
        Ok(frame.iter().fold(0.0f32, |m, s| m.max(s.abs())).min(1.0))
    }
    fn reset(&mut self) {}
}

#[test]
fn asr_stretch_an_hour_of_silence_holds_no_more_than_the_padding() {
    // A muted listener, or a track nobody speaks on: an hour of room noise in pushes of
    // 100 ms to 1 s. The stream keeps the padding a region starting now could reach back
    // over and the frames heard since it last looked, however long the silence.
    let config = PieceConfig::default();
    let frame = CoarseVad.frame_len();
    let bound = sample_count(config.speech_pad) + SETTLE_EVERY_FRAMES * frame;
    let second: Vec<f32> = (0..HZ).map(|i| noisy_sample(i, N::Quiet, 9)).collect();
    let mut t = Exact::default();
    let mut vad = CoarseVad;
    let cancel = CancelToken::new();
    let mut s = PieceStream::new(&t, &mut vad, &beam(), &config).unwrap();
    let mut rng = Lcg(3);
    let mut most = 0;
    let hour = 3600 * HZ;
    while s.pushed() < hour {
        let n = (1600 + (rng.next() as usize) % 14_400).min(hour - s.pushed());
        let pieces = s.push(&mut t, &mut vad, &second[..n], &cancel).unwrap();
        assert!(pieces.is_empty());
        assert!(
            s.held() <= bound,
            "{} samples held after {} s of silence (bound {bound})",
            s.held(),
            s.pushed() / HZ
        );
        most = most.max(s.held());
    }
    assert_eq!(s.pushed(), hour);
    // The frontier follows the silence, a padding behind.
    assert!(s.decoded_through() + bound >= hour);
    // Not vacuous the other way: the padding is held.
    assert!(most >= sample_count(config.speech_pad), "{most}");

    // Speech after the hour is decoded from its padded start, sample for sample.
    let speech: Vec<f32> = (0..3 * HZ)
        .map(|i| noisy_sample(hour + i, N::Loud, 9))
        .collect();
    let mut pieces = s.push(&mut t, &mut vad, &speech, &cancel).unwrap();
    for _ in 0..5 {
        pieces.extend(s.push(&mut t, &mut vad, &second, &cancel).unwrap());
        assert!(s.held() <= held_bound(&config, frame));
    }
    let pad = sample_count(config.speech_pad);
    let span = Span {
        start: hour - pad,
        end: hour + 3 * HZ + pad,
    };
    assert_eq!(pieces.len(), 1, "the speech settled during the stream");
    assert_eq!(pieces[0].span, span);
    // The piece as the recording holds it: the last padding of the silence, the speech,
    // the first padding of the silence after it.
    let mut want: Vec<f32> = Vec::new();
    // The silence before ended on a push boundary inside `second`; rebuild its last `pad`
    // samples from the pushes made.
    want.extend_from_slice(&tail_of_silence(&second, 3, hour, pad));
    want.extend_from_slice(&speech);
    want.extend_from_slice(&second[..pad]);
    assert_eq!(t.calls.len(), 1);
    assert_eq!(t.calls[0].1, span.len());
    assert_eq!(t.calls[0].0, samples_hash(&want));
    assert!(s.held() <= bound, "{} held after the piece", s.held());
    let (rest, summary) = s.finish(&mut t, &mut vad, &cancel).unwrap();
    assert!(rest.is_empty());
    assert_eq!(summary.report.pieces, 1);
}

/// The last `pad` samples of the silence `asr_stretch_an_hour_of_silence_holds_no_more_than
/// _the_padding` pushed before its speech: the same pushes (each the start of `second`)
/// made again from the same seed.
fn tail_of_silence(second: &[f32], seed: u64, hour: usize, pad: usize) -> Vec<f32> {
    let mut rng = Lcg(seed);
    let mut tail: Vec<f32> = Vec::new();
    let mut pushed = 0;
    while pushed < hour {
        let n = (1600 + (rng.next() as usize) % 14_400).min(hour - pushed);
        tail.extend_from_slice(&second[..n]);
        let keep = tail.len().saturating_sub(pad);
        tail.drain(..keep);
        pushed += n;
    }
    tail
}

#[test]
fn asr_stretch_resume_equals_the_whole_track_pass_across_long_silences() {
    // T6.13c with long silences between the turns, where the stream has let go of audio
    // long before it stops: what it decoded plus the rest from the recording is still the
    // whole-track pass.
    let cases = vec![
        (
            vec![
                (40.0, K::Silence),
                (3.0, K::Speech),
                (70.0, K::Silence),
                (2.0, K::Speech),
                (0.6, K::Silence),
                (4.0, K::Speech),
                (95.0, K::Silence),
                (45.0, K::Speech),
                (33.0, K::Silence),
            ],
            beam(),
        ),
        (
            vec![
                (2.0, K::Speech),
                (61.0, K::Silence),
                (3.0, K::LoopsWithPrompt),
                (2.5, K::Silence),
                (3.0, K::Speech),
                (120.0, K::Silence),
                (3.0, K::AlwaysLoops),
                (50.0, K::Silence),
                (2.0, K::Speech),
            ],
            beam(),
        ),
    ];
    let resumed_mid_stream = check_resume(false, cases);
    assert!(resumed_mid_stream > 40, "{resumed_mid_stream}");
}
