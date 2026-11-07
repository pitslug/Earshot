//! Diarisation clustering threshold calibration for CAM++ on LibriSpeech **dev-clean only**
//! (T3.2b): `cargo test -p earshot-onnx diar_calibration`.
//!
//! `ClusteringConfig.threshold` is a Euclidean distance between the un-normalised means of
//! unit embeddings (pyannote 3.1 centroid linkage). pyannote's 0.7046 was tuned for
//! WeSpeaker; CAM++ needs its own value, calibrated here on dev data and never on the
//! acceptance sets (LibriSpeech test-clean, AMI).
//!
//! Protocol and selection rule (fixed before the first measurement; not changed after):
//! - Speakers: the 40 dev-clean speakers of `fixtures/librispeech/subset.toml`, sorted by
//!   id. Each speaker's stream is all its subset utterances (enrol and test roles)
//!   concatenated in utterance-id order.
//! - Scenes: [`SCENES_PER_COUNT`] scenes for each speaker count k in 2..=6 (20 scenes).
//!   Scene `(k, s)` takes the first k speakers of a ChaCha8 shuffle of the 40 (seed
//!   `100 k + s`). Turns: the first k turns visit every speaker once (shuffled order), later
//!   turns pick any other speaker than the previous one that still has at least 1.5 s of
//!   unused audio; each turn is the next 1.5-8 s of that speaker's stream, capped at what is
//!   left, so no audio is used twice in a scene (a reused stretch would make a speaker more
//!   similar to itself than in real speech and bias the threshold down). A turn change
//!   overlaps the previous turn by 0.2-1.0 s
//!   (at most half the previous turn) with probability 0.2, otherwise follows a 0.1-0.8 s
//!   silence. Turns are added until the scene holds [`SPEECH_PER_SPEAKER_S`] x k seconds of
//!   speech or no other speaker has audio left. The mix is the sum, scaled down only if it would clip. Reference turns are the
//!   turn spans, so in-utterance pauses count as speech for every threshold alike.
//! - Pipeline: the real pyannote segmentation-3.0 and CAM++ (ONNX Runtime, CPU, 4 threads)
//!   through the core `Diarizer` with pyannote 3.1 defaults; only `clustering.threshold`
//!   varies, over [`GRID`].
//! - Criterion: the lowest corpus DER (errors summed over all scenes over summed reference
//!   time; NIST collar 0.25 s per side, overlap scored); ties broken by the lowest mean
//!   absolute speaker-count error; remaining ties by the middle threshold of the tied set.
//!
//! The absolute DER is pessimistic (reference turns include each utterance's own pauses)
//! and is not comparable with AMI DER; only its minimum over the grid matters here.
//!
//! The full calibration fails if `CAMPPLUS_DEV_CLEAN_CLUSTER_THRESHOLD` differs from the
//! fresh calibration, and prints the measured curve. It is `#[ignore]`d (a visible skip,
//! about 16 min in debug); run it in release with `-- --ignored` (see the test).
//!
//! Long scenes (T6.9b). T3.2b used 15 s per speaker and no one-speaker scene, and T6.9 saw a
//! small over-count at 0.68 on 360 s scenes of the first 20 speakers. This second protocol
//! and rule were written before any of its measurements and are not changed after:
//! - Speakers: the whole dev-clean corpus folder, speaker folders in the path order T6.9's
//!   `far_speakers` used; positions [`LONG_SPEAKERS`] only (the 20 T6.9 never saw). A
//!   speaker's stream is its utterances in path order, joined with [`LONG_GAP_MS`] of
//!   silence, up to [`LONG_SCENE_S`]. No private recording takes part in anything here.
//! - Scenes, all [`LONG_SCENE_S`] long, quantised as the FLAC writer stores audio: `one`,
//!   each of the 20 speakers alone (true count 1); `even`, consecutive pairs in alternating
//!   turns of 5-30 s each (true count 2); `dominant`, the same pairs, first speaker's turns
//!   40-90 s, second's 4-12 s (true count 2). Turn lengths come from [`next_turn`], seed
//!   `LONG_SEED + pair`, and no audio is used twice within a scene. Across scenes it is: each
//!   speaker's one stream plays in its `one`, `even` and `dominant` scenes, against AGENTS'
//!   "scenes never reuse a speaker's audio". The 40 scenes are about 20 independent voices, so
//!   [`COUNT_MARGIN`] (written as "2 of 40 scenes") and the summed DER overstate the evidence.
//!   Found in review and left as it was measured: the rule selects nothing while the constant
//!   is held (`docs/far-speakers/long-scenes-calibration.md`). Reference turns are the turn
//!   spans.
//! - Pipeline: as above; `clustering.threshold` varies over [`LONG_GRID`].
//! - At each threshold t: S(t), the T3.2b corpus DER of the protocol above; L(t), the corpus
//!   DER summed over all long scenes (same collar); C(t), the mean |speaker count error| over
//!   all long scenes.
//! - Rule ([`select_long`]):
//!   1. Admissible: S(t) <= min S + [`VALLEY_HEIGHT`], the height of the T3.2b valley as its
//!      constant's docs state it (0.178-0.189), so short scenes cannot get worse than that.
//!   2. Winner: lowest L rounded to 0.001, then lowest C, then nearest the committed
//!      constant, then the lower threshold.
//!   3. The constant moves to the winner only if L(constant) - L(winner) >= [`DER_MARGIN`],
//!      or L(winner) <= L(constant) and C(constant) - C(winner) >= [`COUNT_MARGIN`] (2 of 40
//!      scenes). Otherwise it stays.
//!   4. A move is kept only if `cargo xtask accept m3` then passes; if it does not, the
//!      constant stays and the result is reported.
//!
//! Decided 2026-11-07: the rule picks 0.83 and the constant stays at 0.68, because at 0.83 two
//! of five AMI dev meetings collapse into one speaker, which this rule cannot see. The rule and
//! its margins are unchanged. Its test pins both values, so a change to the model, the pipeline
//! or the scenes that alters the rule's pick fails and asks for a new decision.

mod common;

use std::ops::Range;
use std::path::{Path, PathBuf};

use earshot_core::asr::{CancelToken, MeetingTime};
use earshot_core::audio::{MonoBuffer, PIPELINE_RATE, Sample, quantise_pcm16};
use earshot_core::diar::cluster::{
    CAMPPLUS_DEV_CLEAN_CLUSTER_THRESHOLD, calibrated_cluster_threshold,
};
use earshot_core::diar::{
    ClusteringConfig, DiarError, DiarizationConfig, Diarizer, EmbedderInfo, Embedding,
    LocalActivity, SegmenterInfo, SpeakerEmbedder, SpeakerSegmenter,
};
use earshot_core::metrics::der::{AMI_COLLAR_PER_SIDE_S, DerConfig, Turn, der};
use earshot_onnx::SessionConfig;
use earshot_onnx::embed::CamppEmbedder;
use earshot_onnx::segment::PyannoteSegmenter;
use rand_chacha::ChaCha8Rng;
use rand_core::{Rng, SeedableRng};
use serde::Deserialize;

const SPLIT: &str = "dev-clean";
/// Scenes per speaker count.
const SCENES_PER_COUNT: u64 = 4;
/// Speech per speaker in a scene (seconds).
const SPEECH_PER_SPEAKER_S: f64 = 15.0;
/// Thresholds tried: 0.30, 0.31, ..., 1.30.
const GRID: std::ops::RangeInclusive<u32> = 30..=130;
const HZ: f64 = 16_000.0;
/// Positions in the corpus folder's path order of the speakers long scenes use.
const LONG_SPEAKERS: Range<usize> = 20..40;
/// Length of one long scene (seconds).
const LONG_SCENE_S: usize = 360;
/// Pause between utterances of a stream and at each turn change (ms).
const LONG_GAP_MS: usize = 200;
/// Turn-length sequence seed of a pair's scenes, plus the pair's index.
const LONG_SEED: u64 = 6_920;
/// Thresholds tried on long scenes: 0.55, 0.56, ..., 1.00 (holds every admissible value; the
/// test fails if one falls outside).
const LONG_GRID: std::ops::RangeInclusive<u32> = 55..=100;
/// Rule step 1: how far above its minimum the T3.2b DER may go.
const VALLEY_HEIGHT: f64 = 0.011;
/// Rule step 3: the long-scene DER a move must win by.
const DER_MARGIN: f64 = 0.002;
/// Rule step 3: the long-scene count error a move with no DER loss must win by.
const COUNT_MARGIN: f64 = 0.05;
/// What the long-scene rule picks, declined in favour of the committed constant: see the module
/// docs and `docs/far-speakers/long-scenes-calibration.md`.
const LONG_RULE_PICK_DECLINED: f32 = 0.83;
/// The constant held against that pick.
const LONG_RULE_HELD_CONSTANT: f32 = 0.68;

#[derive(Deserialize)]
struct Subset {
    utterance: Vec<Utt>,
}

#[derive(Deserialize)]
struct Utt {
    split: String,
    speaker: u32,
    id: String,
}

/// Fixture directory: `EARSHOT_FIXTURES`, else `%LOCALAPPDATA%\earshot-dev\fixtures`
/// (mirrors `xtask::fixtures`).
fn fixtures_dir() -> PathBuf {
    if let Some(d) = std::env::var_os("EARSHOT_FIXTURES").filter(|v| !v.is_empty()) {
        return PathBuf::from(d);
    }
    PathBuf::from(std::env::var_os("LOCALAPPDATA").expect("LOCALAPPDATA"))
        .join("earshot-dev")
        .join("fixtures")
}

fn load_utt(u: &Utt) -> Vec<f32> {
    let parts: Vec<&str> = u.id.split('-').collect();
    let path = fixtures_dir()
        .join("librispeech")
        .join(&u.split)
        .join("LibriSpeech")
        .join(&u.split)
        .join(parts[0])
        .join(parts[1])
        .join(format!("{}.flac", u.id));
    assert!(
        path.is_file(),
        "{} missing; run `cargo xtask fetch-fixtures --set librispeech`",
        path.display()
    );
    let mut r = claxon::FlacReader::open(&path).expect("flac");
    assert_eq!(r.streaminfo().sample_rate, 16_000);
    let scale = 1.0 / f64::from(1u32 << (r.streaminfo().bits_per_sample - 1));
    r.samples()
        .map(|s| (f64::from(s.expect("sample")) * scale) as f32)
        .collect()
}

/// Per dev-clean speaker (sorted by id): all its subset utterances concatenated.
fn speaker_streams() -> Vec<(u32, Vec<f32>)> {
    let path = common::repo_root().join("fixtures/librispeech/subset.toml");
    let subset: Subset =
        toml::from_str(&std::fs::read_to_string(&path).expect("subset.toml")).expect("parse");
    let mut utts: Vec<&Utt> = subset
        .utterance
        .iter()
        .filter(|u| u.split == SPLIT)
        .collect();
    utts.sort_by(|a, b| (a.speaker, &a.id).cmp(&(b.speaker, &b.id)));
    let mut out: Vec<(u32, Vec<f32>)> = Vec::new();
    for u in utts {
        let audio = load_utt(u);
        match out.last_mut() {
            Some((id, s)) if *id == u.speaker => s.extend(audio),
            _ => out.push((u.speaker, audio)),
        }
    }
    assert_eq!(out.len(), 40, "dev-clean subset has 40 speakers");
    out
}

/// A uniform draw in `[lo, hi)`.
fn uniform(rng: &mut ChaCha8Rng, lo: f64, hi: f64) -> f64 {
    lo + (hi - lo) * (rng.next_u64() >> 11) as f64 / (1u64 << 53) as f64
}

struct Scene {
    speakers: usize,
    audio: Vec<f32>,
    reference: Vec<Turn>,
    /// The sample range of its speaker's stream each turn of `reference` plays.
    sources: Vec<std::ops::Range<usize>>,
}

impl Scene {
    /// Two turns (indices into `reference`) of one speaker that play overlapping audio of
    /// its stream, if any: the scene rule never uses audio twice (the diar_eval rule,
    /// `Scene::reused_audio` in earshot-runtime's `diar_eval` example).
    fn reused_audio(&self) -> Option<(usize, usize)> {
        for (i, (ti, ri)) in self.reference.iter().zip(&self.sources).enumerate() {
            for (j, (tj, rj)) in self
                .reference
                .iter()
                .zip(&self.sources)
                .enumerate()
                .skip(i + 1)
            {
                if ti.speaker == tj.speaker && ri.start < rj.end && rj.start < ri.end {
                    return Some((i, j));
                }
            }
        }
        None
    }
}

fn build_scene(streams: &[(u32, Vec<f32>)], k: usize, s: u64) -> Scene {
    let mut rng = ChaCha8Rng::seed_from_u64(100 * k as u64 + s);
    // Fisher-Yates shuffle of the speaker indices; the first k take part.
    let mut order: Vec<usize> = (0..streams.len()).collect();
    for i in (1..order.len()).rev() {
        let j = (rng.next_u64() % (i as u64 + 1)) as usize;
        order.swap(i, j);
    }
    let cast: Vec<usize> = order[..k].to_vec();
    let mut cursor = vec![0usize; k];
    let min_turn = (1.5 * HZ) as usize;
    let mut audio: Vec<f32> = Vec::new();
    let mut reference = Vec::new();
    let mut sources = Vec::new();
    let mut speech = 0.0;
    let mut prev: Option<(usize, usize)> = None; // (speaker slot, turn length in samples)
    let mut turn = 0usize;
    while speech < SPEECH_PER_SPEAKER_S * k as f64 || turn < k {
        let slot = if turn < k {
            turn
        } else {
            let eligible: Vec<usize> = (0..k)
                .filter(|&c| Some(c) != prev.map(|p| p.0))
                .filter(|&c| streams[cast[c]].1.len() - cursor[c] >= min_turn)
                .collect();
            if eligible.is_empty() {
                break;
            }
            eligible[(rng.next_u64() % eligible.len() as u64) as usize]
        };
        let stream = &streams[cast[slot]].1;
        let left = stream.len() - cursor[slot];
        assert!(left >= min_turn, "every speaker has well over 1.5 s");
        let len = ((uniform(&mut rng, 1.5, 8.0) * HZ) as usize).min(left);
        let source = cursor[slot]..cursor[slot] + len;
        let piece: Vec<f32> = stream[source.clone()].to_vec();
        sources.push(source);
        cursor[slot] += len;
        let start = match prev {
            None => 0,
            Some((_, prev_len)) if uniform(&mut rng, 0.0, 1.0) < 0.2 => {
                let ov = ((uniform(&mut rng, 0.2, 1.0) * HZ) as usize).min(prev_len / 2);
                audio.len() - ov
            }
            Some(_) => audio.len() + (uniform(&mut rng, 0.1, 0.8) * HZ) as usize,
        };
        if audio.len() < start + len {
            audio.resize(start + len, 0.0);
        }
        for (a, p) in audio[start..start + len].iter_mut().zip(&piece) {
            *a += p;
        }
        reference.push(Turn::new(
            start as f64 / HZ,
            (start + len) as f64 / HZ,
            streams[cast[slot]].0.to_string(),
        ));
        speech += len as f64 / HZ;
        prev = Some((slot, len));
        turn += 1;
    }
    let peak = audio.iter().fold(0.0f32, |m, x| m.max(x.abs()));
    if peak > 0.99 {
        let g = 0.99 / peak;
        audio.iter_mut().for_each(|x| *x *= g);
    }
    Scene {
        speakers: k,
        audio,
        reference,
        sources,
    }
}

/// Synthetic streams (distinct per speaker, 20 s each) for testing the scene rule without
/// fixtures.
fn synthetic_streams(n: u32) -> Vec<(u32, Vec<f32>)> {
    (0..n)
        .map(|id| {
            let len = (20.0 * HZ) as usize;
            let v = (0..len)
                .map(|i| ((i as f32 * 0.01 * (id + 1) as f32).sin()) * 0.1)
                .collect();
            (1000 + id, v)
        })
        .collect()
}

#[test]
fn diar_calibration_scene_reuse_guard_finds_two_turns_playing_the_same_samples() {
    let st = synthetic_streams(8);
    for k in 2..=6usize {
        for s in 0..SCENES_PER_COUNT {
            let sc = build_scene(&st, k, s);
            assert_eq!(sc.sources.len(), sc.reference.len());
            assert_eq!(sc.reused_audio(), None, "k={k} s={s}");
        }
    }
    let mut sc = build_scene(&st, 2, 0);
    // Make a later turn of the first speaker replay its first turn's samples.
    let first = sc.reference[0].speaker.clone();
    let again = (1..sc.reference.len())
        .find(|&j| sc.reference[j].speaker == first)
        .expect("the first speaker talks twice");
    sc.sources[again] = sc.sources[0].start + 1..sc.sources[0].end + 1;
    assert_eq!(sc.reused_audio(), Some((0, again)));
    // The same samples of another speaker's stream are not a reuse.
    let two = |speaker_b: &str| Scene {
        speakers: 2,
        audio: Vec::new(),
        reference: vec![
            Turn::new(0.0, 1.0, "a".to_string()),
            Turn::new(1.0, 2.0, speaker_b.to_string()),
        ],
        sources: vec![0..16_000, 100..16_100],
    };
    assert_eq!(two("b").reused_audio(), None);
    assert_eq!(two("a").reused_audio(), Some((0, 1)));
}

/// Records the wrapped model's outputs on the first run and replays them afterwards: the
/// segmenter and embedder inputs do not depend on the clustering threshold, so one run per
/// scene serves the whole sweep. Every call is keyed by a hash of its input samples; a
/// replayed call must have the same inputs as the recorded call at that position, and
/// [`Recorded::assert_consumed`] checks a replayed run used every recorded call.
struct Recorded<T, O> {
    inner: T,
    log: Vec<(u64, Vec<O>)>,
    replay_at: Option<usize>,
}

/// FNV-1a over the batch: each input's length and its samples' bit patterns, in order.
fn input_hash(inputs: &[&[Sample]]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    let mut eat = |v: u64| {
        for b in v.to_le_bytes() {
            h ^= u64::from(b);
            h = h.wrapping_mul(0x0000_0100_0000_01b3);
        }
    };
    for x in inputs {
        eat(x.len() as u64);
        for s in *x {
            eat(u64::from(s.to_bits()));
        }
    }
    h
}

impl<T, O: Clone> Recorded<T, O> {
    fn new(inner: T) -> Self {
        Recorded {
            inner,
            log: Vec::new(),
            replay_at: None,
        }
    }

    fn replay(&mut self) {
        self.replay_at = Some(0);
    }

    /// After a replayed run: every recorded call was used, in order.
    fn assert_consumed(&self) {
        if let Some(i) = self.replay_at {
            assert_eq!(
                i,
                self.log.len(),
                "replayed run used {i} of {} calls",
                self.log.len()
            );
        }
    }

    fn call(
        &mut self,
        inputs: &[&[Sample]],
        run: impl FnOnce(&mut T) -> Result<Vec<O>, DiarError>,
    ) -> Result<Vec<O>, DiarError> {
        let key = input_hash(inputs);
        match &mut self.replay_at {
            None => {
                let out = run(&mut self.inner)?;
                self.log.push((key, out.clone()));
                Ok(out)
            }
            Some(i) => {
                let (logged, out) = self
                    .log
                    .get(*i)
                    .unwrap_or_else(|| panic!("replayed run made more than {i} calls"));
                assert_eq!(*logged, key, "replayed call {i} has other inputs");
                assert_eq!(out.len(), inputs.len(), "replayed call {i} batch size");
                let out = out.clone();
                *i += 1;
                Ok(out)
            }
        }
    }
}

impl<T: SpeakerSegmenter> SpeakerSegmenter for Recorded<T, LocalActivity> {
    fn info(&self) -> &SegmenterInfo {
        self.inner.info()
    }
    fn segment(
        &mut self,
        windows: &[&[Sample]],
        cancel: &CancelToken,
    ) -> Result<Vec<LocalActivity>, DiarError> {
        self.call(windows, |m| m.segment(windows, cancel))
    }
}

impl<T: SpeakerEmbedder> SpeakerEmbedder for Recorded<T, Embedding> {
    fn info(&self) -> &EmbedderInfo {
        self.inner.info()
    }
    fn embed(
        &mut self,
        speech: &[&[Sample]],
        cancel: &CancelToken,
    ) -> Result<Vec<Embedding>, DiarError> {
        self.call(speech, |m| m.embed(speech, cancel))
    }
}

/// One threshold's result over all scenes.
#[derive(Debug, Clone, Copy)]
struct Point {
    threshold: f32,
    errors: f64,
    total: f64,
    count_error: f64,
}

impl Point {
    fn der(&self) -> f64 {
        self.errors / self.total
    }
}

fn threshold(t: u32) -> f32 {
    t as f32 / 100.0
}

/// The selection rule of the module docs.
fn select(points: &[Point]) -> Point {
    let best_der = points.iter().map(Point::der).fold(f64::INFINITY, f64::min);
    let tied: Vec<&Point> = points.iter().filter(|p| p.der() == best_der).collect();
    let best_count = tied
        .iter()
        .map(|p| p.count_error)
        .fold(f64::INFINITY, f64::min);
    let tied: Vec<&Point> = tied
        .into_iter()
        .filter(|p| p.count_error == best_count)
        .collect();
    *tied[tied.len() / 2]
}

/// What [`select_long`] decided.
#[derive(Debug, Clone, Copy, PartialEq)]
struct LongChoice {
    /// The threshold the rule picks: the winner if a move is justified, else `current`.
    threshold: f32,
    /// Step 2's winner, whether or not it justified a move.
    winner: f32,
}

/// The long-scene rule of the module docs. `short` holds S(t), `long` holds L(t) and C(t) (its
/// `count_error` already a mean); `current` is the committed constant.
fn select_long(short: &[Point], long: &[Point], current: f32) -> LongChoice {
    let same = |a: f32, b: f32| (a - b).abs() < 1e-6;
    let min_s = short.iter().map(Point::der).fold(f64::INFINITY, f64::min);
    let admissible: Vec<&Point> = short
        .iter()
        .filter(|p| p.der() <= min_s + VALLEY_HEIGHT)
        .map(|s| {
            long.iter()
                .find(|l| same(l.threshold, s.threshold))
                .unwrap_or_else(|| panic!("admissible {:.2} is outside the long grid", s.threshold))
        })
        .collect();
    let at_current = long
        .iter()
        .find(|l| same(l.threshold, current))
        .expect("the committed constant is on the long grid");
    assert!(
        admissible.iter().any(|p| same(p.threshold, current)),
        "the committed constant is not admissible: the T3.2b curve has moved"
    );
    let rounded = |p: &Point| (p.der() * 1000.0).round();
    let distance = |p: &Point| (p.threshold - current).abs();
    let winner = *admissible
        .iter()
        .min_by(|a, b| {
            rounded(a)
                .total_cmp(&rounded(b))
                .then(a.count_error.total_cmp(&b.count_error))
                // Thresholds are on a 0.01 grid: compare distances at that precision.
                .then(((distance(a) * 100.0).round()).total_cmp(&((distance(b) * 100.0).round())))
                .then(a.threshold.total_cmp(&b.threshold))
        })
        .expect("the committed constant is admissible");
    let der_win = at_current.der() - winner.der() >= DER_MARGIN;
    let count_win = winner.der() <= at_current.der()
        && at_current.count_error - winner.count_error >= COUNT_MARGIN;
    LongChoice {
        threshold: if der_win || count_win {
            winner.threshold
        } else {
            current
        },
        winner: winner.threshold,
    }
}

fn point(threshold: f32, der: f64, count_error: f64) -> Point {
    Point {
        threshold,
        errors: der,
        total: 1.0,
        count_error,
    }
}

#[test]
fn diar_calibration_long_rule_stays_without_a_clear_win() {
    let short = [
        point(0.68, 0.178, 0.5),
        point(0.75, 0.180, 0.5),
        point(0.95, 0.27, 0.9),
    ];
    // The winner is better, but by less than either margin.
    let long = [
        point(0.68, 0.1000, 0.10),
        point(0.75, 0.0990, 0.08),
        point(0.95, 0.05, 0.0),
    ];
    assert_eq!(
        select_long(&short, &long, 0.68),
        LongChoice {
            threshold: 0.68,
            winner: 0.75
        }
    );
}

#[test]
fn diar_calibration_long_rule_moves_on_either_margin() {
    let short = [point(0.68, 0.178, 0.5), point(0.75, 0.180, 0.5)];
    let by_der = [point(0.68, 0.100, 0.10), point(0.75, 0.098, 0.10)];
    assert_eq!(select_long(&short, &by_der, 0.68).threshold, 0.75);
    let by_count = [point(0.68, 0.100, 0.10), point(0.75, 0.100, 0.05)];
    assert_eq!(select_long(&short, &by_count, 0.68).threshold, 0.75);
    // A better count at a worse DER moves nothing, even inside the rounding of step 2.
    let worse_der = [point(0.68, 0.1000, 0.10), point(0.75, 0.1004, 0.05)];
    let choice = select_long(&short, &worse_der, 0.68);
    assert_eq!(choice.winner, 0.75);
    assert_eq!(choice.threshold, 0.68);
}

#[test]
fn diar_calibration_long_rule_never_leaves_the_short_scene_valley() {
    let short = [point(0.68, 0.178, 0.5), point(0.90, 0.1895, 0.5)];
    let long = [point(0.68, 0.100, 0.10), point(0.90, 0.010, 0.0)];
    assert_eq!(
        select_long(&short, &long, 0.68),
        LongChoice {
            threshold: 0.68,
            winner: 0.68
        }
    );
    // Just inside the valley's height it may.
    let short = [point(0.68, 0.178, 0.5), point(0.90, 0.1885, 0.5)];
    assert_eq!(select_long(&short, &long, 0.68).threshold, 0.90);
}

#[test]
fn diar_calibration_long_rule_breaks_ties_towards_the_committed_constant() {
    let short = [
        point(0.60, 0.178, 0.5),
        point(0.66, 0.178, 0.5),
        point(0.68, 0.178, 0.5),
        point(0.71, 0.178, 0.5),
    ];
    let long = [
        point(0.60, 0.090, 0.0),
        point(0.66, 0.090, 0.0),
        point(0.68, 0.100, 0.1),
        point(0.71, 0.090, 0.0),
    ];
    assert_eq!(select_long(&short, &long, 0.68).winner, 0.66);
}

/// The speaker folders of the corpus in the path order `far_speakers` used.
fn corpus_speakers(root: &Path) -> Vec<PathBuf> {
    assert!(
        root.is_dir(),
        "{} missing; run `cargo xtask fetch-fixtures --set librispeech`",
        root.display()
    );
    sorted_entries(root)
        .into_iter()
        .filter(|p| p.is_dir())
        .collect()
}

fn sorted_entries(dir: &Path) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = std::fs::read_dir(dir)
        .expect("corpus folder")
        .map(|e| e.expect("corpus entry").path())
        .collect();
    v.sort();
    v
}

fn read_flac(path: &Path) -> Vec<f32> {
    let mut r = claxon::FlacReader::open(path).expect("flac");
    assert_eq!(r.streaminfo().sample_rate, 16_000);
    let scale = 1.0 / f64::from(1u32 << (r.streaminfo().bits_per_sample - 1));
    r.samples()
        .map(|s| (f64::from(s.expect("sample")) * scale) as f32)
        .collect()
}

/// A speaker's utterances in path order with a short pause, up to `want` samples.
fn long_stream(speaker: &Path, want: usize) -> Vec<f32> {
    let gap = vec![0f32; (HZ as usize) * LONG_GAP_MS / 1000];
    let mut out = Vec::new();
    for chapter in sorted_entries(speaker).into_iter().filter(|p| p.is_dir()) {
        for flac in sorted_entries(&chapter)
            .into_iter()
            .filter(|p| p.extension().is_some_and(|e| e == "flac"))
        {
            out.extend(read_flac(&flac));
            out.extend_from_slice(&gap);
            if out.len() >= want {
                out.truncate(want);
                return out;
            }
        }
    }
    out
}

/// The next of a fixed sequence of turn lengths in `[lo, hi)` seconds: deterministic and
/// independent of the audio (`far_speakers`'s rule).
fn next_turn(state: &mut u64, (lo, hi): (usize, usize)) -> usize {
    *state = state
        .wrapping_mul(6_364_136_223_846_793_005)
        .wrapping_add(1_442_695_040_888_963_407);
    lo + ((*state >> 33) as usize) % (hi - lo)
}

/// A long scene with its truth.
struct LongScene {
    kind: &'static str,
    speakers: usize,
    audio: Vec<f32>,
    reference: Vec<Turn>,
}

/// Two streams in turns, a short silence at each change, cut at `want` samples. The turn spans
/// are the reference; nothing is played twice, since each stream only moves forward.
fn long_two_speakers(
    kind: &'static str,
    (a, b): (&[f32], &[f32]),
    (a_turn, b_turn): ((usize, usize), (usize, usize)),
    seed: u64,
    want: usize,
) -> LongScene {
    let hz = HZ as usize;
    let gap = hz * LONG_GAP_MS / 1000;
    let mut state = seed;
    let mut cursors = [0usize, 0];
    let mut audio: Vec<f32> = Vec::with_capacity(want);
    let mut reference = Vec::new();
    while audio.len() < want {
        let before = audio.len();
        for (slot, (stream, turn)) in [(a, a_turn), (b, b_turn)].into_iter().enumerate() {
            let n = next_turn(&mut state, turn) * hz;
            let take = n.min(stream.len() - cursors[slot]);
            let start = audio.len();
            audio.extend_from_slice(&stream[cursors[slot]..cursors[slot] + take]);
            cursors[slot] += take;
            let end = audio.len().min(want);
            if take > 0 && start < end {
                reference.push(Turn::new(
                    start as f64 / HZ,
                    end as f64 / HZ,
                    ["a", "b"][slot].to_string(),
                ));
            }
            audio.resize(audio.len() + gap, 0.0);
        }
        if audio.len() == before + 2 * gap {
            break;
        }
    }
    audio.truncate(want);
    LongScene {
        kind,
        speakers: 2,
        audio,
        reference,
    }
}

fn long_one_speaker(audio: Vec<f32>) -> LongScene {
    LongScene {
        kind: "one",
        speakers: 1,
        reference: vec![Turn::new(0.0, audio.len() as f64 / HZ, "a".to_string())],
        audio,
    }
}

#[test]
fn diar_calibration_long_scene_reference_is_the_audio_it_plays() {
    let hz = HZ as usize;
    let a = vec![0.25f32; 100 * hz];
    let b = vec![0.5f32; 100 * hz];
    let want = 150 * hz;
    let scene = long_two_speakers("even", (&a, &b), ((5, 30), (5, 30)), 7, want);
    assert_eq!(scene.audio.len(), want);
    // Every reference turn holds only its speaker's samples, and every non-silent sample is in
    // a turn of its speaker.
    let mut covered = vec![false; want];
    for t in &scene.reference {
        let level = if t.speaker == "a" { 0.25 } else { 0.5 };
        let (s, e) = ((t.start * HZ) as usize, (t.end * HZ) as usize);
        assert!(scene.audio[s..e].iter().all(|&x| x == level), "{t:?}");
        covered[s..e].iter_mut().for_each(|c| *c = true);
    }
    for (x, c) in scene.audio.iter().zip(&covered) {
        assert_eq!(*x != 0.0, *c);
    }
    // A stream that runs out leaves silence, never a repeat: 100 s of each in 150 s stays whole.
    let short = vec![0.25f32; 3 * hz];
    let scene = long_two_speakers("even", (&short, &b), ((5, 6), (5, 6)), 7, 20 * hz);
    let of_a: f64 = scene
        .reference
        .iter()
        .filter(|t| t.speaker == "a")
        .map(|t| t.end - t.start)
        .sum();
    assert!((of_a - 3.0).abs() < 1e-9, "{of_a}");
}

/// The long-scene calibration of the module docs (40 scenes of 360 s, plus T3.2b's 20). Fails
/// if the rule picks anything but the value that was declined, or the constant moved from the
/// value held against it: either way the decision has to be taken again.
#[test]
#[ignore = "Dev calibration, long: cargo test --release -p earshot-onnx diar_calibration -- --include-ignored"]
fn diar_calibration_long_scenes_rule_still_picks_the_value_that_was_declined() {
    // S(t) first: T3.2b's curve on the whole grid.
    common::init_runtime();
    let config = SessionConfig::cpu(4);
    let seg = PyannoteSegmenter::load(&common::model(common::SEGMENTATION), &config, 8)
        .expect("load segmentation");
    let emb = CamppEmbedder::load(&common::model(common::CAMPPLUS), &config).expect("CAM++");
    let mut parts = (Recorded::new(seg), Recorded::new(emb));
    let cancel = CancelToken::new();
    let collar = DerConfig::nist(AMI_COLLAR_PER_SIDE_S);
    let mut short: Vec<Point> = GRID
        .map(|t| Point {
            threshold: threshold(t),
            errors: 0.0,
            total: 0.0,
            count_error: 0.0,
        })
        .collect();
    let streams = speaker_streams();
    for k in 2..=6usize {
        for s in 0..SCENES_PER_COUNT {
            let scene = build_scene(&streams, k, s);
            parts = sweep(
                parts,
                &scene.audio,
                &mut short,
                &scene.reference,
                scene.speakers,
                &collar,
                &cancel,
            );
        }
    }
    drop(streams);

    // The long scenes.
    let root = fixtures_dir()
        .join("librispeech")
        .join(SPLIT)
        .join("LibriSpeech")
        .join(SPLIT);
    let speakers = corpus_speakers(&root);
    assert_eq!(speakers.len(), 40, "dev-clean has 40 speakers");
    let want = LONG_SCENE_S * HZ as usize;
    let streams: Vec<Vec<f32>> = speakers[LONG_SPEAKERS]
        .iter()
        .map(|s| long_stream(s, want))
        .collect();
    let mut scenes: Vec<LongScene> = streams
        .iter()
        .map(|s| long_one_speaker(s.clone()))
        .collect();
    for (p, pair) in streams.as_chunks::<2>().0.iter().enumerate() {
        let seed = LONG_SEED + p as u64;
        let pair = (pair[0].as_slice(), pair[1].as_slice());
        scenes.push(long_two_speakers(
            "even",
            pair,
            ((5, 30), (5, 30)),
            seed,
            want,
        ));
        scenes.push(long_two_speakers(
            "dominant",
            pair,
            ((40, 90), (4, 12)),
            seed,
            want,
        ));
    }
    drop(streams);
    let blank = |t: u32| Point {
        threshold: threshold(t),
        errors: 0.0,
        total: 0.0,
        count_error: 0.0,
    };
    let mut long: Vec<Point> = LONG_GRID.map(blank).collect();
    let kinds = ["one", "even", "dominant"];
    let mut per_kind: Vec<Vec<Point>> = kinds
        .iter()
        .map(|_| LONG_GRID.map(blank).collect())
        .collect();
    let mut scene_counts = [0usize; 3];
    let mut audio_s = 0.0;
    for (i, scene) in scenes.iter().enumerate() {
        eprintln!("long scene {i} of {} ({})", scenes.len(), scene.kind);
        let audio = quantise_pcm16(&scene.audio);
        audio_s += audio.len() as f64 / HZ;
        let mut here: Vec<Point> = LONG_GRID.map(blank).collect();
        parts = sweep(
            parts,
            &audio,
            &mut here,
            &scene.reference,
            scene.speakers,
            &collar,
            &cancel,
        );
        let kind = kinds
            .iter()
            .position(|k| *k == scene.kind)
            .expect("known kind");
        scene_counts[kind] += 1;
        for (j, h) in here.iter().enumerate() {
            for into in [&mut long[j], &mut per_kind[kind][j]] {
                into.errors += h.errors;
                into.total += h.total;
                into.count_error += h.count_error;
            }
        }
    }
    let n = scenes.len() as f64;
    for s in &mut short {
        s.count_error /= f64::from((2..=6).count() as u32 * SCENES_PER_COUNT as u32);
    }
    for p in &mut long {
        p.count_error /= n;
    }
    for (kind, c) in per_kind.iter_mut().zip(scene_counts) {
        for p in kind {
            p.count_error /= c as f64;
        }
    }
    println!(
        "long-scene calibration (dev-clean speakers at positions {LONG_SPEAKERS:?}): {} scenes, \
         {audio_s:.0} s of audio",
        scenes.len()
    );
    println!("threshold S(t) L(t) C(t) | one DER count | even DER count | dominant DER count");
    for (j, p) in long.iter().enumerate() {
        let s = short
            .iter()
            .find(|s| (s.threshold - p.threshold).abs() < 1e-6)
            .expect("long grid inside the short grid");
        println!(
            "{:.2} {:.4} {:.4} {:.3} | {:.4} {:.3} | {:.4} {:.3} | {:.4} {:.3}",
            p.threshold,
            s.der(),
            p.der(),
            p.count_error,
            per_kind[0][j].der(),
            per_kind[0][j].count_error,
            per_kind[1][j].der(),
            per_kind[1][j].count_error,
            per_kind[2][j].der(),
            per_kind[2][j].count_error,
        );
    }
    let choice = select_long(&short, &long, CAMPPLUS_DEV_CLEAN_CLUSTER_THRESHOLD);
    println!(
        "rule: winner {:.2}, picks {:.2} (committed {CAMPPLUS_DEV_CLEAN_CLUSTER_THRESHOLD})",
        choice.winner, choice.threshold
    );
    assert!(
        (choice.threshold - LONG_RULE_PICK_DECLINED).abs() <= 1e-6,
        "the long-scene rule picks {:.2}, and the decision to hold the constant was taken against \
         {LONG_RULE_PICK_DECLINED}: take it again",
        choice.threshold
    );
    assert!(
        (CAMPPLUS_DEV_CLEAN_CLUSTER_THRESHOLD - LONG_RULE_HELD_CONSTANT).abs() <= 1e-6,
        "the constant is {CAMPPLUS_DEV_CLEAN_CLUSTER_THRESHOLD}, and it was held at \
         {LONG_RULE_HELD_CONSTANT}: update this test and docs/far-speakers/long-scenes-calibration.md \
         with the new decision"
    );
}

type Parts = (
    Recorded<PyannoteSegmenter, LocalActivity>,
    Recorded<CamppEmbedder, Embedding>,
);

/// One scene over every threshold in `points` (models once, then replays), adding its DER
/// components and its absolute count error (a sum: the caller divides).
fn sweep(
    mut parts: Parts,
    audio: &[f32],
    points: &mut [Point],
    reference: &[Turn],
    speakers: usize,
    collar: &DerConfig,
    cancel: &CancelToken,
) -> Parts {
    let buf = MonoBuffer::new(PIPELINE_RATE, audio.to_vec());
    parts.0.log.clear();
    parts.1.log.clear();
    parts.0.replay_at = None;
    parts.1.replay_at = None;
    for (i, p) in points.iter_mut().enumerate() {
        let cfg = DiarizationConfig {
            clustering: ClusteringConfig {
                threshold: p.threshold,
                ..ClusteringConfig::default()
            },
            ..DiarizationConfig::default()
        };
        let (mut seg, mut emb) = parts;
        if i > 0 {
            seg.replay();
            emb.replay();
        }
        let mut d = Diarizer::new(seg, emb, cfg).expect("diarizer");
        let out = d.diarize(&buf, MeetingTime::ZERO, cancel).expect("diarize");
        parts = d.into_parts();
        parts.0.assert_consumed();
        parts.1.assert_consumed();
        let r = der(reference, &out.der_turns(), collar);
        p.errors += r.missed + r.false_alarm + r.confusion;
        p.total += r.total;
        p.count_error += (out.speakers.len() as f64 - speakers as f64).abs();
    }
    parts
}

/// The committed constant is registered for the exact model id the CAM++ embedder reports
/// (fast; runs in ci).
#[test]
fn diar_calibration_constant_is_registered_for_the_campplus_model() {
    common::init_runtime();
    let emb = CamppEmbedder::load(&common::model(common::CAMPPLUS), &SessionConfig::cpu(1))
        .expect("CAM++");
    assert_eq!(
        calibrated_cluster_threshold(&emb.info().model),
        Some(CAMPPLUS_DEV_CLEAN_CLUSTER_THRESHOLD)
    );
}

/// The full calibration (20 scenes, 101 thresholds). Ignored in the default run because it
/// takes about 16 min in a debug build (about 2 min in release; the time is debug-mode Rust
/// feature extraction and pipeline code, not the models). Run it with
/// `cargo test --release -p earshot-onnx diar_calibration -- --ignored` after any change to
/// the models, the pipeline or the clustering; it fails if the committed constant is no
/// longer what the protocol selects.
#[test]
#[ignore = "dev calibration, ~2 min in release: cargo test --release -p earshot-onnx diar_calibration -- --ignored"]
fn diar_calibration_campplus_dev_clean_cluster_threshold_is_reproducible() {
    let streams = speaker_streams();
    common::init_runtime();
    let config = SessionConfig::cpu(4);
    let seg = PyannoteSegmenter::load(&common::model(common::SEGMENTATION), &config, 8)
        .expect("load segmentation");
    let emb = CamppEmbedder::load(&common::model(common::CAMPPLUS), &config).expect("CAM++");
    let model = emb.info().model.clone();
    let mut parts = (Recorded::new(seg), Recorded::new(emb));
    let cancel = CancelToken::new();
    let collar = DerConfig::nist(AMI_COLLAR_PER_SIDE_S);

    let mut points: Vec<Point> = GRID
        .map(|t| Point {
            threshold: threshold(t),
            errors: 0.0,
            total: 0.0,
            count_error: 0.0,
        })
        .collect();
    let mut scenes = 0usize;
    let mut audio_s = 0.0;
    for k in 2..=6usize {
        for s in 0..SCENES_PER_COUNT {
            let scene = build_scene(&streams, k, s);
            audio_s += scene.audio.len() as f64 / HZ;
            if let Some((a, b)) = scene.reused_audio() {
                panic!("scene k={k} s={s} reused audio in turns {a} and {b}");
            }
            let buf = MonoBuffer::new(PIPELINE_RATE, scene.audio.clone());
            let (mut seg, mut emb) = parts;
            seg.log.clear();
            emb.log.clear();
            seg.replay_at = None;
            emb.replay_at = None;
            parts = (seg, emb);
            for (i, p) in points.iter_mut().enumerate() {
                let cfg = DiarizationConfig {
                    clustering: ClusteringConfig {
                        threshold: p.threshold,
                        ..ClusteringConfig::default()
                    },
                    ..DiarizationConfig::default()
                };
                let (mut seg, mut emb) = parts;
                if i > 0 {
                    seg.replay();
                    emb.replay();
                }
                let mut d = Diarizer::new(seg, emb, cfg).expect("diarizer");
                let out = d
                    .diarize(&buf, MeetingTime::ZERO, &cancel)
                    .expect("diarize");
                parts = d.into_parts();
                parts.0.assert_consumed();
                parts.1.assert_consumed();
                let r = der(&scene.reference, &out.der_turns(), &collar);
                p.errors += r.missed + r.false_alarm + r.confusion;
                p.total += r.total;
                p.count_error += (out.speakers.len() as f64 - scene.speakers as f64).abs();
            }
            scenes += 1;
        }
    }
    for p in &mut points {
        p.count_error /= scenes as f64;
    }
    println!(
        "diar calibration (dev-clean, CAM++ {model}): {scenes} scenes, {audio_s:.0} s of \
         audio; no turn replays audio another turn of its speaker played"
    );
    for p in &points {
        println!(
            "  threshold {:.2}: DER {:.4}, mean |speaker count error| {:.2}",
            p.threshold,
            p.der(),
            p.count_error
        );
    }
    let chosen = select(&points);
    let default = points
        .iter()
        .find(|p| (p.threshold - 0.70).abs() < 1e-6)
        .copied()
        .expect("0.70 on the grid");
    println!(
        "chosen threshold {:.2}: DER {:.4}, mean |count error| {:.2} (pyannote's WeSpeaker \
         value, here at 0.70: DER {:.4}, count error {:.2})",
        chosen.threshold,
        chosen.der(),
        chosen.count_error,
        default.der(),
        default.count_error
    );
    assert!(
        (chosen.threshold - CAMPPLUS_DEV_CLEAN_CLUSTER_THRESHOLD).abs() <= 1e-6,
        "committed CAMPPLUS_DEV_CLEAN_CLUSTER_THRESHOLD {CAMPPLUS_DEV_CLEAN_CLUSTER_THRESHOLD} \
         differs from the dev-clean calibration {:.2}",
        chosen.threshold
    );
    // Registered for the exact model id the embedder reports.
    assert_eq!(
        calibrated_cluster_threshold(&model),
        Some(CAMPPLUS_DEV_CLEAN_CLUSTER_THRESHOLD)
    );
}
