//! Unit tests for diarisation. Every test path contains `diar`, so
//! `cargo test -p earshot-core diar` selects them all.

use std::time::Duration;

use super::pipeline::{assign_locals, frame_with_new, window_starts};
use super::*;
use crate::asr::{CancelToken, MeetingTime, Segment, Word};
use crate::audio::{MonoBuffer, SampleRate};
use crate::identity::EntityId;
use crate::metrics::der::{AMI_COLLAR_PER_SIDE_S, DerConfig, DerResult, Turn, der};

// ---------------------------------------------------------------- helpers

/// xorshift64* in [-1, 1).
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

/// `k` random unit vectors of dimension `dim`.
fn unit_centroids(k: usize, dim: usize, seed: u64) -> Vec<Vec<f32>> {
    let mut n = Noise(seed | 1);
    (0..k)
        .map(|_| {
            let v: Vec<f32> = (0..dim).map(|_| n.next()).collect();
            let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
            v.into_iter().map(|x| x / norm).collect()
        })
        .collect()
}

/// `per` noisy points around each centroid, interleaved (point i belongs to i % k).
fn points(
    centroids: &[Vec<f32>],
    per: usize,
    noise: f32,
    seed: u64,
) -> (Vec<Vec<f32>>, Vec<usize>) {
    let mut n = Noise(seed | 1);
    let mut out = Vec::new();
    let mut truth = Vec::new();
    for _ in 0..per {
        for (k, c) in centroids.iter().enumerate() {
            out.push(c.iter().map(|&x| x + noise * n.next()).collect());
            truth.push(k);
        }
    }
    (out, truth)
}

/// True if `labels` and `truth` define the same partition.
fn same_partition(labels: &[usize], truth: &[usize]) -> bool {
    labels.len() == truth.len()
        && (0..labels.len())
            .all(|i| (0..labels.len()).all(|j| (labels[i] == labels[j]) == (truth[i] == truth[j])))
}

fn cfg(threshold: f32, min_cluster_size: usize) -> ClusteringConfig {
    ClusteringConfig {
        threshold,
        min_cluster_size,
        ..ClusteringConfig::default()
    }
}

/// `clustering.threshold` of pyannote/speaker-diarization-3.1 (its `config.yaml`): the
/// Euclidean distance between means of unit-length embeddings.
const PYANNOTE_THRESHOLD: f32 = 0.704_565_5;

const DIM: usize = 32;

/// Per-window boundary error of the fake segmenter in the pipeline tests.
const JITTER: Duration = Duration::from_millis(120);

fn embedder(noise: f32) -> FakeEmbedder {
    FakeEmbedder::new(
        "fake-emb@1",
        unit_centroids(MAX_SCENE_SPEAKERS, DIM, 99),
        noise,
        7,
    )
}

fn segmenter() -> FakeSegmenter {
    FakeSegmenter::segmentation_3_0().with_boundary_jitter(JITTER)
}

fn diarizer(config: DiarizationConfig) -> Diarizer<FakeSegmenter, FakeEmbedder> {
    Diarizer::new(segmenter(), embedder(0.05), config).unwrap()
}

fn run(turns: &[SceneTurn], total_ms: u64, config: DiarizationConfig) -> Diarization {
    diarizer(config)
        .diarize(
            &scene_audio(turns, total_ms),
            MeetingTime::ZERO,
            &CancelToken::new(),
        )
        .unwrap()
}

fn reference(turns: &[SceneTurn]) -> Vec<Turn> {
    turns
        .iter()
        .map(|t| {
            Turn::new(
                t.start_ms as f64 / 1000.0,
                t.end_ms as f64 / 1000.0,
                format!("R{}", t.speaker),
            )
        })
        .collect()
}

/// DER with the AMI/NIST collar (0.25 s per side), overlap scored.
fn score(turns: &[SceneTurn], d: &Diarization) -> DerResult {
    der(
        &reference(turns),
        &d.der_turns(),
        &DerConfig::nist(AMI_COLLAR_PER_SIDE_S),
    )
}

/// Asserts DER <= 1% with the NIST collar (PLAN.md 4.0), and with no collar at all a DER
/// no larger than every reference boundary being off by the fake segmenter's jitter plus
/// frame quantisation (so boundaries are checked, not just hidden by the collar). Returns
/// the collared result.
fn assert_low_der(turns: &[SceneTurn], d: &Diarization, ctx: &str) -> DerResult {
    let r = score(turns, d);
    assert!(r.der() <= 0.01, "{ctx}: DER {:.4} ({r:?})", r.der());
    let strict = der(&reference(turns), &d.der_turns(), &DerConfig::nist(0.0));
    // Each reference boundary may be off by the fake's jitter plus frame quantisation
    // (25 ms: half a 17 ms frame plus window-to-frame rounding).
    let speech_s: f64 = turns
        .iter()
        .map(|t| (t.end_ms - t.start_ms) as f64 / 1000.0)
        .sum();
    let per_boundary_s = JITTER.as_secs_f64() + 0.025;
    let allowed = (2 * turns.len()) as f64 * per_boundary_s / speech_s;
    eprintln!(
        "{ctx}: DER {:.4} (no collar {:.4}, allowed {allowed:.4}), {} speakers",
        r.der(),
        strict.der(),
        d.speakers.len()
    );
    assert!(
        strict.der() <= allowed,
        "{ctx}: no-collar DER {:.4} > {allowed:.4} ({strict:?})",
        strict.der()
    );
    r
}

/// Exact condition: [`run`] with an unjittered segmenter, so only the pipeline itself can
/// misplace boundaries.
fn run_exact(turns: &[SceneTurn], total_ms: u64, config: DiarizationConfig) -> Diarization {
    Diarizer::new(FakeSegmenter::segmentation_3_0(), embedder(0.05), config)
        .unwrap()
        .diarize(
            &scene_audio(turns, total_ms),
            MeetingTime::ZERO,
            &CancelToken::new(),
        )
        .unwrap()
}

/// Asserts DER <= 1% both with the NIST collar (PLAN.md 4.0) and with no collar at all
/// (so turn boundaries are within a few frames, not just inside the collar). For the
/// exact (unjittered) segmenter; catches a systematic boundary shift that the jitter
/// bound of [`assert_low_der`] would allow. Returns the collared result.
fn assert_exact_der(turns: &[SceneTurn], d: &Diarization, ctx: &str) -> DerResult {
    let r = score(turns, d);
    assert!(r.der() <= 0.01, "{ctx}: DER {:.4} ({r:?})", r.der());
    let strict = der(&reference(turns), &d.der_turns(), &DerConfig::nist(0.0));
    eprintln!(
        "{ctx} (exact): DER {:.4} (no collar {:.4}), {} speakers",
        r.der(),
        strict.der(),
        d.speakers.len()
    );
    assert!(
        strict.der() <= 0.01,
        "{ctx}: no-collar DER {:.4} ({strict:?})",
        strict.der()
    );
    r
}

/// Round-robin scene: `n` speakers, `rounds` turns each of `turn_ms`, gaps of `gap_ms`.
fn round_robin(n: usize, rounds: usize, turn_ms: u64, gap_ms: u64) -> (Vec<SceneTurn>, u64) {
    let mut t = 500;
    let mut turns = Vec::new();
    for _ in 0..rounds {
        for k in 0..n {
            turns.push(SceneTurn::new(k, t, t + turn_ms));
            t += turn_ms + gap_ms;
        }
    }
    (turns, t + 500)
}

fn distinct_speakers(d: &Diarization) -> usize {
    let mut s: Vec<usize> = d.turns.iter().map(|t| t.speaker).collect();
    s.sort_unstable();
    s.dedup();
    s.len()
}

fn turn(speaker: usize, start_ms: u64, end_ms: u64) -> SpeakerTurn {
    SpeakerTurn {
        speaker,
        start: MeetingTime::from_millis(start_ms),
        end: MeetingTime::from_millis(end_ms),
    }
}

fn id(n: u8) -> EntityId {
    EntityId::from_bytes([n; 16])
}

fn segment(n: u8, words: Vec<Word>) -> Segment {
    Segment::from_words(id(n), id(200), words, MeetingTime::ZERO)
}

// ---------------------------------------------------------------- types

#[test]
fn diar_powerset_classes_match_pyannote_order() {
    let p = Powerset::new(3, 2);
    let expect: Vec<Vec<usize>> = vec![
        vec![],
        vec![0],
        vec![1],
        vec![2],
        vec![0, 1],
        vec![0, 2],
        vec![1, 2],
    ];
    assert_eq!(p.classes(), expect.as_slice());
    // Frame 0 -> class 5 {0,2}; frame 1 -> class 0 {}; frame 2 -> class 2 {1}.
    let scores = [
        0.0, 0.1, 0.1, 0.1, 0.2, 0.9, 0.3, //
        5.0, 0.1, 0.1, 0.1, 0.2, 0.9, 0.3, //
        -1.0, -2.0, 3.0, -1.0, 0.0, 0.0, 0.0,
    ];
    let a = p.to_multilabel(&scores).unwrap();
    assert_eq!((a.frames(), a.speakers()), (3, 3));
    let rows: Vec<Vec<f32>> = (0..3)
        .map(|f| (0..3).map(|s| a.get(f, s)).collect())
        .collect();
    assert_eq!(
        rows,
        vec![vec![1.0, 0.0, 1.0], vec![0.0; 3], vec![0.0, 1.0, 0.0]]
    );
    assert!(matches!(
        p.to_multilabel(&[0.0; 8]),
        Err(DiarError::Shape(_))
    ));
}

#[test]
fn diar_segmentation_3_0_geometry_is_consistent() {
    let info = SegmenterInfo::pyannote_segmentation_3_0("seg");
    info.validate().unwrap();
    // 589 frames of 991 samples every 270 samples fill 10 s exactly as pyannote computes.
    assert_eq!(
        (info.window - info.frame_duration) / info.frame_step + 1,
        info.frames
    );
    assert_eq!(info.frame_start(0), 360);
    assert_eq!(info.frame_start(1), 630);
    let bad = SegmenterInfo {
        frames: 600,
        ..info
    };
    assert!(matches!(bad.validate(), Err(DiarError::Config(_))));
}

#[test]
fn diar_embeddings_of_different_models_are_never_compared() {
    let a = Embedding::new(EmbeddingModelId::new("cam++@1"), vec![1.0, 0.0]);
    let b = Embedding::new(EmbeddingModelId::new("wespeaker@1"), vec![1.0, 0.0]);
    assert!(matches!(
        a.cosine_similarity(&b),
        Err(DiarError::ModelMismatch { .. })
    ));
    let c = Embedding::new(EmbeddingModelId::new("cam++@1"), vec![0.0, 2.0]);
    assert_eq!(a.cosine_similarity(&c).unwrap(), 0.0);
    assert!((a.cosine_similarity(&a).unwrap() - 1.0).abs() < 1e-6);
    let d = Embedding::new(EmbeddingModelId::new("cam++@1"), vec![1.0]);
    assert!(matches!(a.cosine_similarity(&d), Err(DiarError::Shape(_))));
    assert!(!Embedding::new(a.model.clone(), vec![f32::NAN, 1.0]).is_valid());
    assert!(!Embedding::new(a.model.clone(), vec![0.0, 0.0]).is_valid());
    assert!(a.is_valid());
}

// ---------------------------------------------------------------- clustering

#[test]
fn diar_cluster_recovers_known_clusters() {
    for (k, seed) in [(1usize, 1u64), (2, 2), (4, 3), (6, 4)] {
        let c = unit_centroids(k, 64, seed);
        let (pts, truth) = points(&c, 15, 0.08, seed + 100);
        let r = cluster_embeddings(&pts, &cfg(PYANNOTE_THRESHOLD, 5)).unwrap();
        assert_eq!(r.centroids.len(), k, "k={k}");
        assert!(same_partition(&r.labels, &truth), "k={k}");
        // Labels follow first appearance; centroids are unit length near the truth.
        assert_eq!(r.labels[..k].to_vec(), (0..k).collect::<Vec<_>>());
        for (j, cent) in r.centroids.iter().enumerate() {
            let n: f32 = cent.iter().map(|x| x * x).sum::<f32>().sqrt();
            assert!((n - 1.0).abs() < 1e-4);
            assert!(cosine(cent, &c[j]) > 0.99);
        }
    }
}

fn cosine(a: &[f32], b: &[f32]) -> f32 {
    let d: f32 = a.iter().zip(b).map(|(x, y)| x * y).sum();
    let na: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let nb: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();
    d / (na * nb)
}

/// Two unit vectors of dimension `dim` at Euclidean distance `d` (so cosine
/// `1 - d^2 / 2`).
fn two_at_distance(d: f32, dim: usize) -> Vec<Vec<f32>> {
    let cos = 1.0 - d * d / 2.0;
    let sin = (1.0 - cos * cos).sqrt();
    let mut a = vec![0.0; dim];
    let mut b = vec![0.0; dim];
    a[0] = 1.0;
    b[0] = cos;
    b[1] = sin;
    vec![a, b]
}

/// Mean of the unit-length versions of `pts[i]` for which `keep(i)`.
fn unit_mean(pts: &[Vec<f32>], keep: impl Fn(usize) -> bool) -> Vec<f32> {
    let mut m = vec![0.0f32; pts[0].len()];
    let mut n = 0.0f32;
    for (_, p) in pts.iter().enumerate().filter(|(i, _)| keep(*i)) {
        let norm = p.iter().map(|x| x * x).sum::<f32>().sqrt();
        for (a, &b) in m.iter_mut().zip(p) {
            *a += b / norm;
        }
        n += 1.0;
    }
    m.iter().map(|x| x / n).collect()
}

fn euclid(a: &[f32], b: &[f32]) -> f32 {
    a.iter()
        .zip(b)
        .map(|(x, y)| (x - y) * (x - y))
        .sum::<f32>()
        .sqrt()
}

#[test]
fn diar_cluster_threshold_decides_merging() {
    // Nearly noise-free: the cluster means are at the centroid distance 0.9.
    let c = two_at_distance(0.9, 16);
    let (pts, truth) = points(&c, 10, 0.01, 5);
    let split = cluster_embeddings(&pts, &cfg(0.85, 1)).unwrap();
    assert_eq!(split.centroids.len(), 2);
    assert!(same_partition(&split.labels, &truth));
    let merged = cluster_embeddings(&pts, &cfg(0.95, 1)).unwrap();
    assert_eq!(merged.centroids.len(), 1);
    assert!(merged.labels.iter().all(|&l| l == 0));
}

#[test]
fn diar_cluster_borderline_distances_follow_the_pyannote_threshold() {
    // Realistic within-speaker spread: each embedding has cosine about 0.9 with its
    // speaker's direction, so cluster means have norm about 0.9 and two speakers whose
    // directions are `d` apart have means about 0.9 d apart. pyannote's threshold applies
    // to that distance between means (scipy centroid linkage on unit vectors).
    for (d, expect) in [(0.72f32, 1usize), (0.84, 2)] {
        let c = two_at_distance(d, 64);
        let (pts, truth) = points(&c, 100, 0.105, 61);
        let ma = unit_mean(&pts, |i| truth[i] == 0);
        let mb = unit_mean(&pts, |i| truth[i] == 1);
        let between = euclid(&ma, &mb);
        // The scene really is borderline: within 0.1 of the threshold, on the intended side.
        assert!(
            (between - PYANNOTE_THRESHOLD).abs() < 0.1,
            "d={d}: {between}"
        );
        assert_eq!(
            between > PYANNOTE_THRESHOLD,
            expect == 2,
            "d={d}: {between}"
        );
        let r = cluster_embeddings(&pts, &cfg(PYANNOTE_THRESHOLD, 12)).unwrap();
        assert_eq!(r.centroids.len(), expect, "d={d}: means {between:.3} apart");
        if expect == 2 {
            assert!(same_partition(&r.labels, &truth));
        }
    }
}

#[test]
fn diar_cluster_known_count_overrides_threshold() {
    let c = unit_centroids(3, 64, 11);
    let (pts, truth) = points(&c, 10, 0.05, 12);
    // Threshold would keep 3; a known count of 2 forces one merge.
    let two = cluster_embeddings(&pts, &cfg(PYANNOTE_THRESHOLD, 1).with_num_speakers(2)).unwrap();
    assert_eq!(two.centroids.len(), 2);
    for i in 0..pts.len() {
        for j in 0..pts.len() {
            if truth[i] == truth[j] {
                assert_eq!(two.labels[i], two.labels[j]);
            }
        }
    }
    // Threshold would merge everything; a known count of 3 keeps 3.
    let three = cluster_embeddings(&pts, &cfg(2.0, 1).with_num_speakers(3)).unwrap();
    assert!(same_partition(&three.labels, &truth));
    // max_speakers alone caps; min_speakers alone floors.
    let capped = ClusteringConfig {
        max_speakers: Some(1),
        ..cfg(PYANNOTE_THRESHOLD, 1)
    };
    assert_eq!(
        cluster_embeddings(&pts, &capped).unwrap().centroids.len(),
        1
    );
    let floored = ClusteringConfig {
        min_speakers: 3,
        ..cfg(2.0, 1)
    };
    assert!(same_partition(
        &cluster_embeddings(&pts, &floored).unwrap().labels,
        &truth
    ));
    // More speakers asked for than embeddings: one cluster per embedding.
    let few = &pts[..2];
    let r = cluster_embeddings(few, &cfg(2.0, 1).with_num_speakers(5)).unwrap();
    assert_eq!(r.centroids.len(), 2);
}

/// Unit vector `e_axis` of dimension `dim`.
fn axis(axis: usize, dim: usize) -> Vec<f32> {
    let mut v = vec![0.0; dim];
    v[axis] = 1.0;
    v
}

/// `n` outlier singletons: `n` random unit vectors of dimension `dim`, unrelated to any
/// speaker.
fn outliers(n: usize, dim: usize, seed: u64) -> Vec<Vec<f32>> {
    unit_centroids(n, dim, seed)
}

/// True if no two points of different `truth` speakers (`truth[i] < k`) share a label and
/// all points of one speaker share one, ignoring the points with `truth[i] >= k`.
fn speakers_recovered(labels: &[usize], truth: &[usize], k: usize) -> bool {
    let idx: Vec<usize> = (0..truth.len()).filter(|&i| truth[i] < k).collect();
    idx.iter().all(|&i| {
        idx.iter()
            .all(|&j| (labels[i] == labels[j]) == (truth[i] == truth[j]))
    })
}

#[test]
fn diar_cluster_known_count_keeps_real_speakers_apart_from_outliers() {
    // Reviewer probe (T3.2-T3.4 review): two speakers of 30 embeddings whose directions
    // have cosine 0.6, plus three singletons orthogonal to them and to each other. A known
    // count of 2 must give the two speakers, not one merged speaker plus an outlier.
    let dim = 64;
    let c = vec![axis(0, dim), {
        let mut b = vec![0.0; dim];
        b[0] = 0.6;
        b[1] = 0.8;
        b
    }];
    let (mut pts, mut truth) = points(&c, 30, 0.02, 71);
    for a in [5, 6, 7] {
        pts.push(axis(a, dim));
        truth.push(2);
    }
    let r = cluster_embeddings(&pts, &ClusteringConfig::default().with_num_speakers(2)).unwrap();
    assert_eq!(r.centroids.len(), 2);
    assert!(speakers_recovered(&r.labels, &truth, 2));
    assert!(cosine(&r.centroids[0], &c[0]) > 0.99);
    assert!(cosine(&r.centroids[1], &c[1]) > 0.99);
}

#[test]
fn diar_cluster_known_count_recovers_2_to_6_speakers_with_outliers() {
    // Thresholds that over-split (0.2), are right (pyannote's) and merge everything (2.0):
    // a known count recovers the speakers in every case, and the outliers never become a
    // speaker of their own.
    for k in 2..=6usize {
        for threshold in [0.2f32, PYANNOTE_THRESHOLD, 2.0] {
            let seed = 200 + k as u64;
            let c = unit_centroids(k, 64, seed);
            let (mut pts, mut truth) = points(&c, 20, 0.05, seed + 1);
            for o in outliers(3, 64, seed + 2) {
                pts.push(o);
                truth.push(k);
            }
            let r = cluster_embeddings(&pts, &cfg(threshold, 12).with_num_speakers(k)).unwrap();
            assert_eq!(r.centroids.len(), k, "k={k} threshold={threshold}");
            assert!(
                speakers_recovered(&r.labels, &truth, k),
                "k={k} threshold={threshold}"
            );
        }
    }
}

#[test]
fn diar_cluster_speaker_bounds_with_outliers() {
    let c = unit_centroids(4, 64, 81);
    let (mut pts, mut truth) = points(&c, 20, 0.05, 82);
    for o in outliers(3, 64, 83) {
        pts.push(o);
        truth.push(4);
    }
    // Within the bounds the threshold decides: four speakers, outliers absorbed.
    let within = ClusteringConfig {
        min_speakers: 2,
        max_speakers: Some(6),
        ..cfg(PYANNOTE_THRESHOLD, 12)
    };
    let r = cluster_embeddings(&pts, &within).unwrap();
    assert_eq!(r.centroids.len(), 4);
    assert!(speakers_recovered(&r.labels, &truth, 4));
    // A floor above what a merge-everything threshold gives: the four speakers come back.
    let floored = ClusteringConfig {
        min_speakers: 4,
        ..cfg(2.0, 12)
    };
    let r = cluster_embeddings(&pts, &floored).unwrap();
    assert_eq!(r.centroids.len(), 4);
    assert!(speakers_recovered(&r.labels, &truth, 4));
    // A cap below the true count gives two clusters, and no outlier is a speaker of its
    // own: each holds at least a large cluster's worth (8 = round(0.1 * 83)) of
    // real-speaker embeddings. (pyannote's re-cut takes the iteration nearest the
    // threshold with two large clusters, which can leave another speaker in fragments
    // that are reassigned separately, so whole-speaker merges are not guaranteed.)
    let capped = ClusteringConfig {
        max_speakers: Some(2),
        ..cfg(PYANNOTE_THRESHOLD, 12)
    };
    let r = cluster_embeddings(&pts, &capped).unwrap();
    assert_eq!(r.centroids.len(), 2);
    for l in 0..2 {
        let real = (0..pts.len())
            .filter(|&i| truth[i] < 4 && r.labels[i] == l)
            .count();
        assert!(real >= 8, "cluster {l}: {real}");
    }
}

#[test]
fn diar_cluster_small_clusters_join_the_nearest_large_one() {
    let c = unit_centroids(3, 64, 21);
    let (mut pts, _) = points(&c[..2], 30, 0.05, 22);
    // Two points of a third voice, farther than the threshold from both clusters but
    // closer to cluster 0 than to cluster 1.
    for s in 0..2 {
        let p: Vec<f32> = c[2]
            .iter()
            .zip(&c[0])
            .map(|(x, y)| 0.6 * x + 0.4 * y + 0.001 * s as f32)
            .collect();
        pts.push(p);
    }
    let n = pts.len();
    let r = cluster_embeddings(&pts, &cfg(0.5, 5)).unwrap();
    assert_eq!(r.centroids.len(), 2);
    assert_eq!(r.labels[n - 1], r.labels[0]);
    assert_eq!(r.labels[n - 2], r.labels[0]);
    // With min_cluster_size 1 the small cluster survives.
    let keep = cluster_embeddings(&pts, &cfg(0.5, 1)).unwrap();
    assert_eq!(keep.centroids.len(), 3);
    // The minimum size is capped at 10% of the embeddings (here 6), so a huge setting
    // still keeps the two real clusters.
    let capped = cluster_embeddings(&pts, &cfg(0.5, 100)).unwrap();
    assert_eq!(capped.centroids.len(), 2);
    // A known count promotes the biggest small clusters to large.
    let forced = cluster_embeddings(&pts, &cfg(0.5, 100).with_num_speakers(3)).unwrap();
    assert_eq!(forced.centroids.len(), 3);
    // With no large cluster at all (20 unrelated singletons, minimum size 2), everything
    // joins the biggest one.
    let singletons = unit_centroids(20, 64, 23);
    let one = cluster_embeddings(&singletons, &cfg(0.0, 2)).unwrap();
    assert_eq!(one.centroids.len(), 1);
    // Three embeddings: 10% rounds to a minimum size of 1, so all three are large.
    let three = cluster_embeddings(&singletons[..3], &cfg(0.0, 2)).unwrap();
    assert_eq!(three.centroids.len(), 3);
}

/// Two large clusters on axes 0 and 1 (30 points each) plus `small` points of a voice in
/// direction `voice` (one small cluster at a low threshold).
fn two_large_and_a_voice(voice: &[f32], small: usize) -> (Vec<Vec<f32>>, usize) {
    let dim = voice.len();
    let (mut pts, _) = points(&[axis(0, dim), axis(1, dim)], 30, 0.02, 91);
    let first = pts.len();
    let mut n = Noise(93);
    for _ in 0..small {
        pts.push(voice.iter().map(|&x| x + 0.01 * n.next()).collect());
    }
    (pts, first)
}

fn floored(threshold: f32, floor: f32) -> ClusteringConfig {
    ClusteringConfig {
        unassigned_floor: floor,
        ..cfg(threshold, 12)
    }
}

#[test]
fn diar_cluster_unassigned_floor_leaves_a_voice_like_no_speaker_out() {
    // A short-spoken voice orthogonal to both speakers (cosine 0 to each): today it is
    // merged into a speaker; with a floor of 0.3 it is left out, as its own cluster marked
    // unassigned, and the two speakers are untouched.
    let (pts, first) = two_large_and_a_voice(&axis(5, 64), 4);
    let today = cluster_embeddings(&pts, &cfg(0.3, 12)).unwrap();
    assert_eq!(today.centroids.len(), 2);
    assert_eq!(today.unassigned, vec![false, false]);
    let r = cluster_embeddings(&pts, &floored(0.3, 0.3)).unwrap();
    assert_eq!(r.centroids.len(), 3);
    assert_eq!(r.unassigned, vec![false, false, true]);
    let voice = r.labels[first];
    assert!(r.unassigned[voice]);
    assert!(r.labels[first..].iter().all(|&l| l == voice));
    assert!(r.labels[..first].iter().all(|&l| !r.unassigned[l]));
    assert_eq!(&r.labels[..first], &today.labels[..first]);
    assert!(cosine(&r.centroids[voice], &axis(5, 64)) > 0.99);
}

#[test]
fn diar_cluster_unassigned_floor_still_merges_a_small_cluster_near_a_speaker() {
    // A small cluster at cosine 0.9 to speaker 0 (beyond a 0.3 threshold, so it stays
    // small in the hierarchy) still joins speaker 0 under a floor of 0.3; it is left out
    // only when the floor is above its cosine.
    let dim = 64;
    let near: Vec<f32> = (0..dim)
        .map(|i| match i {
            0 => 0.9,
            2 => (1.0f32 - 0.81).sqrt(),
            _ => 0.0,
        })
        .collect();
    let (pts, first) = two_large_and_a_voice(&near, 4);
    let r = cluster_embeddings(&pts, &floored(0.3, 0.3)).unwrap();
    assert_eq!(r.centroids.len(), 2);
    assert!(r.unassigned.iter().all(|u| !u));
    assert!(r.labels[first..].iter().all(|&l| l == r.labels[0]));
    let strict = cluster_embeddings(&pts, &floored(0.3, 0.95)).unwrap();
    assert_eq!(strict.unassigned, vec![false, false, true]);
}

/// FNV-1a over the bit patterns of every centroid value, in order.
fn centroid_hash(centroids: &[Vec<f32>]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for v in centroids {
        for x in v {
            for b in x.to_bits().to_le_bytes() {
                h ^= u64::from(b);
                h = h.wrapping_mul(0x0000_0100_0000_01b3);
            }
        }
    }
    h
}

#[test]
fn diar_cluster_unassigned_floor_of_minus_one_is_todays_clustering() {
    // -1 (the default) is pyannote's step 5 bit for bit. The labels and centroid bits below
    // were captured from the clustering before the floor existed (commit 0a75751, these
    // inputs and configs), on scenes with small clusters merged into large ones (the three
    // outlier singletons), a known count and a minimum size of 1; any change to step 5 at -1
    // fails here.
    assert_eq!(ClusteringConfig::default().unassigned_floor, -1.0);
    let c = unit_centroids(4, 64, 81);
    let (mut pts, _) = points(&c, 20, 0.05, 82);
    pts.extend(outliers(3, 64, 83));
    let round_robin =
        |tail: [usize; 3]| -> Vec<usize> { (0..80).map(|i| i % 4).chain(tail).collect() };
    let cases: [(ClusteringConfig, usize, Vec<usize>, u64); 4] = [
        (
            cfg(PYANNOTE_THRESHOLD, 12),
            4,
            round_robin([1, 3, 3]),
            0x309c_a7dc_18ec_7f5b,
        ),
        (cfg(0.2, 12), 1, vec![0; 83], 0x4bdf_9397_468a_9b3e),
        (
            cfg(2.0, 12).with_num_speakers(4),
            4,
            round_robin([1, 3, 3]),
            0x309c_a7dc_18ec_7f5b,
        ),
        (
            cfg(0.5, 1),
            7,
            round_robin([4, 5, 6]),
            0x964c_e42f_4c55_66ac,
        ),
    ];
    for (base, k, labels, hash) in cases {
        let r = cluster_embeddings(&pts, &base).unwrap();
        assert_eq!(r.centroids.len(), k, "{base:?}");
        assert_eq!(r.labels, labels, "{base:?}");
        assert_eq!(centroid_hash(&r.centroids), hash, "{base:?}");
        assert!(r.unassigned.iter().all(|u| !u));
    }
}
#[test]
fn diar_cluster_unassigned_outliers_never_count_as_speakers() {
    // A known count of 2 with three orthogonal outlier singletons and a high floor: the
    // two speakers are recovered and counted; each outlier is left unassigned rather than
    // made a speaker or merged into one.
    let dim = 64;
    let c = vec![axis(0, dim), axis(1, dim)];
    let (mut pts, mut truth) = points(&c, 30, 0.02, 71);
    for a in [5, 6, 7] {
        pts.push(axis(a, dim));
        truth.push(2);
    }
    let cfg2 = ClusteringConfig {
        unassigned_floor: 0.5,
        ..ClusteringConfig::default().with_num_speakers(2)
    };
    let r = cluster_embeddings(&pts, &cfg2).unwrap();
    let speakers = r.unassigned.iter().filter(|u| !**u).count();
    assert_eq!(speakers, 2);
    assert!(speakers_recovered(&r.labels, &truth, 2));
    for i in 60..63 {
        assert!(r.unassigned[r.labels[i]], "outlier {i} was given a speaker");
    }
}

#[test]
fn diar_cluster_unassigned_floor_is_validated_and_calibrated_per_model() {
    for bad in [-1.01f32, 1.01, f32::NAN] {
        let c = ClusteringConfig {
            unassigned_floor: bad,
            ..ClusteringConfig::default()
        };
        assert!(matches!(c.validate(), Err(DiarError::Config(_))), "{bad}");
    }
    use super::cluster::{CAMPPLUS_UNASSIGNED_FLOOR, calibrated_unassigned_floor};
    // T12.65: no floor passed its rule on dev data, so CAM++ ships today's clustering.
    assert_eq!(CAMPPLUS_UNASSIGNED_FLOOR, -1.0);
    assert_eq!(
        calibrated_unassigned_floor(&EmbeddingModelId::new("campplus-en-voxceleb@357a834f702b")),
        Some(CAMPPLUS_UNASSIGNED_FLOOR)
    );
    assert_eq!(
        calibrated_unassigned_floor(&EmbeddingModelId::new("unknown@1")),
        None
    );
}

#[test]
fn diar_pipeline_a_short_turn_voice_like_no_speaker_is_left_unassigned() {
    // Two speakers in long turns and a third voice in one short turn (fewer windows than a
    // large cluster's size). Today its speech goes to a speaker; with a floor above its
    // cosine to both, it has no speaker: no third speaker, its time in `unassigned`, and
    // scored as missed speech.
    let mut turns = Vec::new();
    let mut t = 500;
    for r in 0..24 {
        turns.push(SceneTurn::new(r % 2, t, t + 6_000));
        t += 6_300;
        if r == 11 {
            turns.push(SceneTurn::new(2, t, t + 600));
            t += 1_100;
        }
    }
    let total = t + 500;
    let voices = unit_centroids(MAX_SCENE_SPEAKERS, DIM, 99);
    let worst = cosine(&voices[2], &voices[0]).max(cosine(&voices[2], &voices[1]));
    let floor = 0.6;
    assert!(
        worst < floor,
        "the fake voice is too like a speaker: {worst}"
    );
    let short = *turns.iter().find(|t| t.speaker == 2).unwrap();

    let today = run_exact(&turns, total, DiarizationConfig::default());
    assert!(today.unassigned.is_empty());
    assert_eq!(today.speakers.len(), 2);
    let config = DiarizationConfig {
        clustering: ClusteringConfig {
            unassigned_floor: floor,
            ..ClusteringConfig::default()
        },
        ..DiarizationConfig::default()
    };
    let d = run_exact(&turns, total, config);
    assert_eq!(d.speakers.len(), 2);
    assert!(!d.unassigned.is_empty());
    let mid = MeetingTime::from_millis(u64::midpoint(short.start_ms, short.end_ms));
    assert!(
        d.unassigned.iter().any(|s| s.start <= mid && mid < s.end),
        "{:?}",
        d.unassigned
    );
    assert!(!d.turns.iter().any(|t| t.start <= mid && mid < t.end));
    // The speakers' own turns are unchanged by the floor away from the short turn.
    let long: Vec<SceneTurn> = turns.iter().filter(|t| t.speaker != 2).copied().collect();
    assert!(score(&long, &d).confusion <= score(&long, &today).confusion + 1e-9);
    // The left-out speech is missed, not confused, against the full reference.
    let full = score(&turns, &d);
    assert!(full.missed > score(&turns, &today).missed);
}

#[test]
fn diar_assign_unassigned_speech_leaves_words_without_a_speaker() {
    // Speaker 0 holds 0-4 s and 6-10 s; 4-6 s is unassigned. The word inside it has no
    // speaker, its segment is split from the speaker's words, and a tie goes to the speaker.
    let turns = vec![turn(0, 0, 4_000), turn(0, 6_000, 10_000)];
    let gap = [UnassignedSpan {
        start: MeetingTime::from_millis(4_000),
        end: MeetingTime::from_millis(6_000),
    }];
    let w = |text: &str, s: u64, e: u64| Word {
        text: text.to_owned(),
        start: MeetingTime::from_millis(s),
        end: MeetingTime::from_millis(e),
        probability: 0.9,
    };
    let seg = Segment::from_words(
        EntityId::from_bytes([1; 16]),
        EntityId::from_bytes([2; 16]),
        vec![
            w("one", 1_000, 1_500),
            w("edge", 3_500, 4_500),
            w("two", 4_600, 5_000),
            w("three", 7_000, 7_500),
        ],
        MeetingTime::ZERO,
    );
    let mut n = 10u8;
    let mut new_id = || {
        n += 1;
        EntityId::from_bytes([n; 16])
    };
    let out = label_segment_speakers_unassigned(vec![seg.clone()], &turns, &gap, &mut new_id);
    assert_eq!(out.words, 1);
    let parts = out.parts;
    assert_eq!(out.unassigned, [parts[1].0.id]);
    let labels: Vec<(String, Option<usize>)> =
        parts.iter().map(|(s, k)| (s.text.clone(), *k)).collect();
    assert_eq!(
        labels,
        [
            ("one edge".to_owned(), Some(0)),
            ("two".to_owned(), None),
            ("three".to_owned(), Some(0)),
        ]
    );
    // No unassigned speech: exactly label_segment_speakers.
    let mut m = 10u8;
    let mut id_a = || {
        m += 1;
        EntityId::from_bytes([m; 16])
    };
    let mut k = 10u8;
    let mut id_b = || {
        k += 1;
        EntityId::from_bytes([k; 16])
    };
    let plain = label_segment_speakers_unassigned(vec![seg.clone()], &turns, &[], &mut id_a);
    assert_eq!((plain.words, plain.unassigned.len()), (0, 0));
    assert_eq!(
        plain.parts,
        label_segment_speakers(vec![seg], &turns, &mut id_b)
    );
}

#[test]
fn diar_cluster_subsampled_training_still_labels_everything() {
    let c = unit_centroids(3, 64, 31);
    let (pts, truth) = points(&c, 40, 0.05, 32);
    let r = cluster_embeddings(
        &pts,
        &ClusteringConfig {
            max_train_embeddings: 10,
            ..cfg(PYANNOTE_THRESHOLD, 2)
        },
    )
    .unwrap();
    assert!(same_partition(&r.labels, &truth));
}

#[test]
fn diar_cluster_is_order_independent_and_validates_input() {
    let c = unit_centroids(4, 64, 41);
    let (pts, truth) = points(&c, 8, 0.08, 42);
    let mut rev = pts.clone();
    rev.reverse();
    let mut rev_truth = truth.clone();
    rev_truth.reverse();
    let r = cluster_embeddings(&rev, &cfg(PYANNOTE_THRESHOLD, 3)).unwrap();
    assert!(same_partition(&r.labels, &rev_truth));

    let empty = cluster_embeddings(&[], &ClusteringConfig::default()).unwrap();
    assert!(empty.labels.is_empty() && empty.centroids.is_empty());
    let bad = vec![vec![1.0, 0.0], vec![1.0]];
    assert!(matches!(
        cluster_embeddings(&bad, &ClusteringConfig::default()),
        Err(DiarError::Shape(_))
    ));
    let zero = vec![vec![0.0, 0.0]];
    assert!(matches!(
        cluster_embeddings(&zero, &ClusteringConfig::default()),
        Err(DiarError::Shape(_))
    ));
    for bad_cfg in [
        cfg(2.5, 1),
        cfg(-0.1, 1),
        ClusteringConfig {
            min_speakers: 0,
            ..cfg(0.2, 1)
        },
        ClusteringConfig {
            min_speakers: 3,
            max_speakers: Some(2),
            ..cfg(0.2, 1)
        },
        ClusteringConfig {
            max_train_embeddings: 0,
            ..cfg(0.2, 1)
        },
    ] {
        assert!(matches!(
            cluster_embeddings(&pts, &bad_cfg),
            Err(DiarError::Config(_))
        ));
    }
}

// ---------------------------------------------------------------- pipeline pieces

#[test]
fn diar_window_starts_cover_the_audio() {
    let (w, s) = (160_000, 16_000);
    assert_eq!(window_starts(1, w, s), vec![0]);
    assert_eq!(window_starts(64_000, w, s), vec![0]);
    assert_eq!(window_starts(160_000, w, s), vec![0]);
    assert_eq!(window_starts(176_000, w, s), vec![0, 16_000]);
    assert_eq!(
        window_starts(200_000, w, s),
        vec![0, 16_000, 32_000, 40_000]
    );
    assert!(window_starts(0, w, s).is_empty());
}

#[test]
fn diar_constrained_assignment_keeps_simultaneous_locals_apart() {
    // Both locals prefer cluster 0; constrained gives the better total.
    let sims = vec![vec![0.9, 0.8], vec![0.85, 0.1]];
    assert_eq!(assign_locals(&sims, true), vec![1, 0]);
    assert_eq!(assign_locals(&sims, false), vec![0, 0]);
    // More locals than clusters: the leftover takes its nearest centroid.
    let sims = vec![vec![0.9], vec![0.7], vec![0.2]];
    assert_eq!(assign_locals(&sims, true), vec![0, 0, 0]);
    let sims = vec![vec![0.9, 0.1], vec![0.2, 0.8], vec![0.6, 0.5]];
    assert_eq!(assign_locals(&sims, true), vec![0, 1, 0]);
    assert!(assign_locals(&[], true).is_empty());
}

// ---------------------------------------------------------------- pipeline + DER

#[test]
fn diar_two_alternating_speakers_score_near_zero_der() {
    let (turns, total) = round_robin(2, 4, 6000, 700);
    let d = run(&turns, total, DiarizationConfig::default());
    assert_eq!(d.speakers.len(), 2);
    assert_eq!(distinct_speakers(&d), 2);
    assert_low_der(&turns, &d, "scene");
    // Turns are sorted and lie inside the audio.
    assert!(d.turns.windows(2).all(|w| w[0].start <= w[1].start));
    assert!(
        d.turns
            .iter()
            .all(|t| t.start < t.end && t.end.as_millis() <= total)
    );
    assert_eq!(d.embedding_model.as_str(), "fake-emb@1");
    let speech: Duration = d.speakers.iter().map(|s| s.speech).sum();
    let truth_ms: u64 = turns.iter().map(|t| t.end_ms - t.start_ms).sum();
    assert!((speech.as_millis() as i64 - truth_ms as i64).abs() < 400);
}

#[test]
fn diar_exact_segmenter_places_boundaries_within_one_percent() {
    // The jittered runs above allow each boundary to be off by the jitter; here the
    // segmenter is exact, so the original flat 1% no-collar bound applies.
    let (turns, total) = round_robin(2, 4, 6000, 700);
    let d = run_exact(&turns, total, DiarizationConfig::default());
    assert_eq!(d.speakers.len(), 2);
    assert_exact_der(&turns, &d, "two alternating");
    for n in 1..=6 {
        let (turns, total) = round_robin(n, 2, 5000, 500);
        let d = run_exact(&turns, total, DiarizationConfig::default());
        assert_eq!(d.speakers.len(), n, "n={n}");
        assert_exact_der(&turns, &d, &format!("n={n}"));
    }
    let (turns, total) = round_robin(3, 3, 4000, 300);
    let d = Diarizer::new(
        FakeSegmenter::segmentation_3_0().with_rotation(1),
        embedder(0.05),
        DiarizationConfig::default(),
    )
    .unwrap()
    .diarize(
        &scene_audio(&turns, total),
        MeetingTime::ZERO,
        &CancelToken::new(),
    )
    .unwrap();
    assert_eq!(d.speakers.len(), 3);
    assert_exact_der(&turns, &d, "rotation 1");
}

#[test]
fn diar_defaults_are_pyannote_3_1() {
    // pyannote/speaker-diarization-3.1 config.yaml and pyannote.audio 3.1
    // pipelines/clustering.py; see the pipeline module docs for the deviations.
    let c = ClusteringConfig::default();
    assert_eq!(c.threshold, PYANNOTE_THRESHOLD);
    assert_eq!(c.min_cluster_size, 12);
    assert_eq!(c.max_train_embeddings, 1000);
    assert_eq!((c.min_speakers, c.max_speakers), (1, None));
    let p = DiarizationConfig::default();
    assert_eq!(p.window_step, Duration::from_secs(1));
    assert_eq!(p.min_active_ratio, 0.0);
    assert!(p.exclude_overlap);
    assert!(!p.constrained_assignment);
    assert_eq!(p.merge_gap, Duration::ZERO);
}

/// Two speakers interrupting each other: 6 s turns starting every 4 s, alternating, so
/// every turn overlaps its neighbours by 2 s and only a third of each turn is clean.
/// Embeddings of whole turns are 60/40 mixtures of the two voices; embeddings of the
/// clean parts are (nearly) pure.
fn interruption_scene() -> (Vec<SceneTurn>, u64) {
    let turns: Vec<SceneTurn> = (0..10u64)
        .map(|k| SceneTurn::new((k % 2) as usize, 500 + 4000 * k, 6500 + 4000 * k))
        .collect();
    (turns, 43_000)
}

#[test]
fn diar_excluding_overlap_from_embeddings_lowers_der() {
    let (turns, total) = interruption_scene();
    // Orthogonal voices so the scene depends only on what the embeddings are made of.
    let mut basis = vec![vec![0.0f32; DIM]; 2];
    basis[0][0] = 1.0;
    basis[1][1] = 1.0;
    let audio = scene_audio(&turns, total);
    let mut ders = Vec::new();
    for exclude_overlap in [true, false] {
        let mut dz = Diarizer::new(
            segmenter(),
            FakeEmbedder::new("fake-emb@1", basis.clone(), 0.05, 3),
            DiarizationConfig {
                exclude_overlap,
                ..DiarizationConfig::default()
            },
        )
        .unwrap();
        let d = dz
            .diarize(&audio, MeetingTime::ZERO, &CancelToken::new())
            .unwrap();
        ders.push((exclude_overlap, d.speakers.len(), score(&turns, &d).der()));
    }
    let (with, without) = (ders[0], ders[1]);
    // Clean embeddings of speaker 0 keep the two voices apart ...
    assert_eq!(with.1, 2, "{ders:?}");
    assert!(with.2 <= 0.05, "{ders:?}");
    // ... mixed ones pull them together, so speaker 1 is lost.
    assert!(without.2 >= with.2 + 0.2, "{ders:?}");
}

#[test]
fn diar_overlapped_speech_yields_two_simultaneous_turns() {
    let turns = vec![
        SceneTurn::new(0, 500, 9000),
        SceneTurn::new(1, 7000, 16_000), // 2 s overlap with speaker 0
        SceneTurn::new(0, 15_000, 22_000), // 1 s overlap
        SceneTurn::new(1, 23_000, 30_000),
        SceneTurn::new(0, 27_000, 33_000), // 3 s overlap
    ];
    let d = run(&turns, 34_000, DiarizationConfig::default());
    assert_eq!(d.speakers.len(), 2);
    assert_low_der(&turns, &d, "scene");
    // Both speakers are active in the middle of the first overlap.
    let at = MeetingTime::from_millis(8000);
    let active: Vec<usize> = d
        .turns
        .iter()
        .filter(|t| t.start <= at && at < t.end)
        .map(|t| t.speaker)
        .collect();
    assert_eq!(active.len(), 2, "{:?}", d.turns);
    assert_ne!(active[0], active[1]);
}

#[test]
fn diar_counts_one_to_six_speakers() {
    for n in 1..=6 {
        let (turns, total) = round_robin(n, 2, 5000, 500);
        let d = run(&turns, total, DiarizationConfig::default());
        assert_eq!(d.speakers.len(), n, "n={n}");
        assert_eq!(distinct_speakers(&d), n, "n={n}");
        assert_low_der(&turns, &d, &format!("n={n}"));
    }
}

#[test]
fn diar_stitches_scrambled_local_labels() {
    let (turns, total) = round_robin(3, 3, 4000, 300);
    for rotation in [1, 2] {
        let mut dz = Diarizer::new(
            segmenter().with_rotation(rotation),
            embedder(0.05),
            DiarizationConfig::default(),
        )
        .unwrap();
        let d = dz
            .diarize(
                &scene_audio(&turns, total),
                MeetingTime::ZERO,
                &CancelToken::new(),
            )
            .unwrap();
        assert_eq!(d.speakers.len(), 3);
        assert_low_der(&turns, &d, &format!("rotation {rotation}"));
    }
}

#[test]
fn diar_known_speaker_count_is_honoured() {
    let (turns, total) = round_robin(3, 2, 5000, 500);
    // A threshold that merges everything, rescued by the known count.
    let config = DiarizationConfig {
        clustering: ClusteringConfig {
            threshold: 2.0,
            ..ClusteringConfig::default()
        }
        .with_num_speakers(3),
        ..DiarizationConfig::default()
    };
    let d = run(&turns, total, config);
    assert_eq!(d.speakers.len(), 3);
    assert_low_der(&turns, &d, "known count");
}

#[test]
fn diar_der_check_detects_a_broken_clustering() {
    // Negative control: merging all speakers must show up as a large DER.
    let (turns, total) = round_robin(2, 4, 6000, 700);
    let config = DiarizationConfig {
        clustering: ClusteringConfig {
            threshold: 2.0,
            ..ClusteringConfig::default()
        },
        ..DiarizationConfig::default()
    };
    let d = run(&turns, total, config);
    assert_eq!(d.speakers.len(), 1);
    let r = score(&turns, &d);
    assert!(r.der() > 0.3, "DER {:.4}", r.der());
}

#[test]
fn diar_short_audio_uses_one_padded_window() {
    let turns = [SceneTurn::new(0, 500, 3500)];
    // One window, so no other window can average out a boundary error: no jitter here.
    let mut dz = Diarizer::new(
        FakeSegmenter::segmentation_3_0(),
        embedder(0.05),
        DiarizationConfig::default(),
    )
    .unwrap();
    let d = dz
        .diarize(
            &scene_audio(&turns, 4000),
            MeetingTime::ZERO,
            &CancelToken::new(),
        )
        .unwrap();
    assert_eq!(d.speakers.len(), 1);
    assert_eq!(d.turns.len(), 1);
    let t = &d.turns[0];
    assert!(t.start.as_millis().abs_diff(500) <= 30, "{t:?}");
    assert!(t.end.as_millis().abs_diff(3500) <= 30, "{t:?}");
    let (seg, _) = dz.into_parts();
    assert_eq!(seg.windows_seen(), 1);
}

#[test]
fn diar_silence_and_empty_audio_have_no_speakers() {
    for total in [0u64, 20_000] {
        let d = run(&[], total, DiarizationConfig::default());
        assert!(d.turns.is_empty() && d.speakers.is_empty(), "total={total}");
    }
}

#[test]
fn diar_turns_are_in_meeting_time() {
    let turns = [
        SceneTurn::new(0, 1000, 6000),
        SceneTurn::new(1, 7000, 12_000),
    ];
    let audio = scene_audio(&turns, 13_000);
    let origin = MeetingTime::from_millis(60_000);
    let a = diarizer(DiarizationConfig::default())
        .diarize(&audio, MeetingTime::ZERO, &CancelToken::new())
        .unwrap();
    let b = diarizer(DiarizationConfig::default())
        .diarize(&audio, origin, &CancelToken::new())
        .unwrap();
    assert_eq!(a.turns.len(), b.turns.len());
    for (x, y) in a.turns.iter().zip(&b.turns) {
        assert_eq!(x.start.as_millis() + 60_000, y.start.as_millis());
        assert_eq!(x.end.as_millis() + 60_000, y.end.as_millis());
    }
}

#[test]
fn diar_batch_size_does_not_change_the_result() {
    let (turns, total) = round_robin(3, 2, 4500, 400);
    let one = run(
        &turns,
        total,
        DiarizationConfig {
            batch_size: 1,
            ..DiarizationConfig::default()
        },
    );
    let many = run(&turns, total, DiarizationConfig::default());
    assert_eq!(one, many);
}

#[test]
fn diar_embeds_only_regions_the_model_accepts() {
    let turns = vec![
        SceneTurn::new(0, 500, 9000),
        SceneTurn::new(1, 8800, 9100), // 0.3 s, mostly overlapped
        SceneTurn::new(1, 9100, 16_000),
    ];
    let mut dz = diarizer(DiarizationConfig::default());
    dz.diarize(
        &scene_audio(&turns, 17_000),
        MeetingTime::ZERO,
        &CancelToken::new(),
    )
    .unwrap();
    let (_, emb) = dz.into_parts();
    assert!(!emb.regions().is_empty());
    assert!(
        emb.regions().iter().all(|&n| n >= 4000),
        "{:?}",
        emb.regions()
    );
}

#[test]
fn diar_errors_are_reported() {
    let (turns, total) = round_robin(2, 2, 5000, 500);
    let audio = scene_audio(&turns, total);

    let cancel = CancelToken::new();
    cancel.cancel();
    assert_eq!(
        diarizer(DiarizationConfig::default()).diarize(&audio, MeetingTime::ZERO, &cancel),
        Err(DiarError::Cancelled)
    );

    let slow = MonoBuffer::new(SampleRate::new(8000).unwrap(), audio.samples.clone());
    assert!(matches!(
        diarizer(DiarizationConfig::default()).diarize(
            &slow,
            MeetingTime::ZERO,
            &CancelToken::new()
        ),
        Err(DiarError::WrongSampleRate {
            expected: 16_000,
            actual: 8000
        })
    ));

    let mut bad_shape = Diarizer::new(
        FakeSegmenter::segmentation_3_0().with_wrong_frame_count(100),
        embedder(0.05),
        DiarizationConfig::default(),
    )
    .unwrap();
    assert!(matches!(
        bad_shape.diarize(&audio, MeetingTime::ZERO, &CancelToken::new()),
        Err(DiarError::Shape(_))
    ));

    let mut seg_fail = Diarizer::new(
        FakeSegmenter::segmentation_3_0().failing("boom"),
        embedder(0.05),
        DiarizationConfig::default(),
    )
    .unwrap();
    assert_eq!(
        seg_fail.diarize(&audio, MeetingTime::ZERO, &CancelToken::new()),
        Err(DiarError::Backend("boom".to_owned()))
    );

    let mut emb_fail = Diarizer::new(
        FakeSegmenter::segmentation_3_0(),
        embedder(0.05).failing("bang"),
        DiarizationConfig::default(),
    )
    .unwrap();
    assert_eq!(
        emb_fail.diarize(&audio, MeetingTime::ZERO, &CancelToken::new()),
        Err(DiarError::Backend("bang".to_owned()))
    );

    for config in [
        DiarizationConfig {
            window_step: Duration::ZERO,
            ..DiarizationConfig::default()
        },
        DiarizationConfig {
            batch_size: 0,
            ..DiarizationConfig::default()
        },
        DiarizationConfig {
            onset: 0.0,
            ..DiarizationConfig::default()
        },
        DiarizationConfig {
            min_active_ratio: 1.5,
            ..DiarizationConfig::default()
        },
    ] {
        assert!(matches!(
            Diarizer::new(FakeSegmenter::segmentation_3_0(), embedder(0.05), config),
            Err(DiarError::Config(_))
        ));
    }
}

// ---------------------------------------------------------------- words and segments

#[test]
fn diar_word_goes_to_the_speaker_with_most_overlap() {
    let turns = vec![turn(0, 0, 1000), turn(1, 900, 2000)];
    let words = vec![
        Word::new("a", 100, 400, 0.9),   // only 0
        Word::new("b", 850, 1150, 0.9),  // 0: 150, 1: 250 -> 1
        Word::new("c", 700, 950, 0.9),   // 0: 250, 1: 50 -> 0
        Word::new("d", 1500, 1800, 0.9), // only 1
    ];
    assert_eq!(
        assign_words(&words, &turns),
        vec![Some(0), Some(1), Some(0), Some(1)]
    );
}

#[test]
fn diar_word_ties_prefer_the_previous_speaker_then_the_lowest_index() {
    // Fully overlapped region 1000..2000: both speakers overlap equally.
    let turns = vec![turn(0, 0, 2000), turn(1, 1000, 3000)];
    let w = |s, e| Word::new("w", s, e, 0.9);
    // First word tied: no previous -> lowest index.
    assert_eq!(assign_words(&[w(1200, 1400)], &turns), vec![Some(0)]);
    // Previous word was speaker 1 -> tie goes to 1.
    assert_eq!(
        assign_words(&[w(2200, 2500), w(1200, 1400)], &turns),
        vec![Some(1), Some(1)]
    );
    // Previous word was speaker 0 -> tie goes to 0.
    assert_eq!(
        assign_words(&[w(100, 300), w(1200, 1400)], &turns),
        vec![Some(0), Some(0)]
    );
}

#[test]
fn diar_words_in_gaps_go_to_the_nearest_turn() {
    let turns = vec![turn(0, 0, 1000), turn(1, 2000, 3000)];
    let w = |s, e| Word::new("w", s, e, 0.9);
    assert_eq!(assign_words(&[w(1100, 1300)], &turns), vec![Some(0)]);
    assert_eq!(assign_words(&[w(1700, 1900)], &turns), vec![Some(1)]);
    // Equidistant (400 ms each side), no previous word: the earlier turn's speaker.
    assert_eq!(assign_words(&[w(1400, 1600)], &turns), vec![Some(0)]);
    // Equidistant, previous word was speaker 1.
    assert_eq!(
        assign_words(&[w(2500, 2600), w(1400, 1600)], &turns),
        vec![Some(1), Some(1)]
    );
    // After the last turn and before the first.
    assert_eq!(assign_words(&[w(9000, 9100)], &turns), vec![Some(1)]);
    // No turns at all.
    assert_eq!(assign_words(&[w(0, 10)], &[]), vec![None]);
    assert!(assign_words(&[], &turns).is_empty());
}

#[test]
fn diar_zero_length_word_uses_the_turn_containing_it() {
    let turns = vec![turn(0, 0, 1000), turn(1, 1000, 2000)];
    let w = |t| Word::new("w", t, t, 0.9);
    assert_eq!(
        assign_words(&[w(500), w(1000), w(1500)], &turns),
        vec![Some(0), Some(1), Some(1)]
    );
}

/// Words 100 ms long, each `(text, pause before in ms)`, from 0.
fn snap_line(spec: &[(&str, u64)]) -> Vec<Word> {
    let mut t = 0;
    spec.iter()
        .map(|(text, gap)| {
            t += gap;
            let w = Word::new(*text, t, t + 100, 0.9);
            t += 100;
            w
        })
        .collect()
}

fn snap_labels(v: &[usize]) -> Vec<Option<usize>> {
    v.iter().map(|&l| Some(l)).collect()
}

const SNAP_1: Snap = Snap {
    reach: 1,
    clause_ends: false,
};
const SNAP_2: Snap = Snap {
    reach: 2,
    clause_ends: false,
};

#[test]
fn diar_snap_gives_a_sentences_last_word_back_to_its_speaker() {
    // "... a concern for them." / "I think ..." with the change one word early, on "them.".
    let words = snap_line(&[
        ("a", 0),
        ("concern", 0),
        ("for", 0),
        ("them.", 0),
        ("I", 0),
        ("think", 0),
    ]);
    let labels = snap_labels(&[0, 0, 0, 1, 1, 1]);
    assert_eq!(
        snap_changes(&words, &labels, SNAP_1),
        snap_labels(&[0, 0, 0, 0, 1, 1])
    );
    // The other way: the new speaker's first word kept by the old one.
    let labels = snap_labels(&[0, 0, 0, 0, 0, 1]);
    assert_eq!(
        snap_changes(&words, &labels, SNAP_1),
        snap_labels(&[0, 0, 0, 0, 1, 1])
    );
}

#[test]
fn diar_snap_never_moves_a_change_on_a_sentence_end_or_a_pause() {
    // A real one-word answer after a question, and the first speaker going on after it.
    let words = snap_line(&[
        ("is", 0),
        ("it", 0),
        ("done?", 0),
        ("Yes.", 0),
        ("Good", 0),
        ("then", 0),
    ]);
    let labels = snap_labels(&[0, 0, 0, 1, 0, 0]);
    assert_eq!(snap_changes(&words, &labels, SNAP_2), labels);
    // A change after a pause of SNAP_PAUSE_MS stays even without punctuation.
    let words = snap_line(&[("so", 0), ("we", 0), ("go", SNAP_PAUSE_MS), ("there", 0)]);
    let labels = snap_labels(&[0, 0, 1, 1]);
    assert_eq!(snap_changes(&words, &labels, SNAP_2), labels);
}

#[test]
fn diar_snap_leaves_a_change_with_no_supported_boundary_in_reach() {
    let words = snap_line(&[("a", 0), ("b.", 0), ("c", 0), ("d", 0), ("e", 0), ("f", 0)]);
    // The sentence end is two words back: out of reach for 1, in reach for 2.
    let labels = snap_labels(&[0, 0, 0, 0, 1, 1]);
    assert_eq!(snap_changes(&words, &labels, SNAP_1), labels);
    assert_eq!(
        snap_changes(&words, &labels, SNAP_2),
        snap_labels(&[0, 0, 1, 1, 1, 1])
    );
    // Reach 0 changes nothing.
    let none = Snap {
        reach: 0,
        clause_ends: false,
    };
    assert_eq!(snap_changes(&words, &labels, none), labels);
}

#[test]
fn diar_snap_never_empties_a_run() {
    // The only supported boundary would take the whole one-word run of speaker 1.
    let words = snap_line(&[("a.", 0), ("b", 0), ("c", 0), ("d", 0)]);
    let labels = snap_labels(&[0, 1, 2, 2]);
    assert_eq!(snap_changes(&words, &labels, SNAP_2), labels);
    // Empty input and a single run pass through.
    assert!(snap_changes(&[], &[], SNAP_2).is_empty());
    let words = snap_line(&[("a", 0), ("b", 0)]);
    assert_eq!(
        snap_changes(&words, &snap_labels(&[1, 1]), SNAP_2),
        snap_labels(&[1, 1])
    );
}

#[test]
fn diar_snap_clause_ends_count_only_in_the_clause_variant() {
    let words = snap_line(&[
        ("we", 0),
        ("need", 0),
        ("them,", 0),
        ("Mike", 0),
        ("can", 0),
    ]);
    let labels = snap_labels(&[0, 0, 1, 1, 1]);
    assert_eq!(snap_changes(&words, &labels, SNAP_1), labels);
    let clause = Snap {
        reach: 1,
        clause_ends: true,
    };
    assert_eq!(
        snap_changes(&words, &labels, clause),
        snap_labels(&[0, 0, 0, 1, 1])
    );
}

#[test]
fn diar_label_segments_splits_at_speaker_changes() {
    let turns = vec![turn(0, 0, 2000), turn(1, 2000, 4000)];
    let seg = segment(
        1,
        vec![
            Word::new("hello", 100, 500, 0.8),
            Word::new("there.", 600, 1000, 0.6),
            Word::new("Hi", 2100, 2400, 1.0),
            Word::new("back", 2500, 2900, 0.5),
        ],
    );
    let mut next = 50u8;
    let mut new_id = || {
        next += 1;
        id(next)
    };
    let out = label_segments(vec![seg], &turns, |k| Some(id(100 + k as u8)), &mut new_id);
    assert_eq!(out.len(), 2);
    assert_eq!(out[0].id, id(1));
    assert_eq!(out[0].speaker_id, Some(id(100)));
    assert_eq!(out[0].text, "hello there.");
    assert_eq!(out[0].start, MeetingTime::from_millis(100));
    assert_eq!(out[0].end, MeetingTime::from_millis(1000));
    assert!((out[0].confidence - 0.7).abs() < 1e-6);
    assert_eq!(out[1].id, id(51));
    assert_eq!(out[1].speaker_id, Some(id(101)));
    assert_eq!(out[1].text, "Hi back");
    assert_eq!(out[1].track, id(200));
    assert_eq!(out[1].words.len(), 2);
}

#[test]
fn diar_label_segments_keeps_unsplit_segments_intact() {
    let turns = vec![turn(0, 0, 5000)];
    let mut seg = segment(
        2,
        vec![
            Word::new("one", 100, 300, 0.9),
            Word::new("two", 400, 600, 0.9),
        ],
    );
    seg.text = "One, two.".to_owned(); // recogniser text differs from the joined words
    let mut new_id = || -> EntityId { panic!("no split expected") };
    let out = label_segments(vec![seg.clone()], &turns, |_| Some(id(9)), &mut new_id);
    assert_eq!(out.len(), 1);
    let mut expect = seg;
    expect.speaker_id = Some(id(9));
    assert_eq!(out[0], expect);

    // A segment without words is assigned by its span; unmapped speakers stay None.
    let mut empty = segment(3, Vec::new());
    empty.start = MeetingTime::from_millis(1000);
    empty.end = MeetingTime::from_millis(2000);
    let out = label_segments(vec![empty.clone()], &turns, |_| None, &mut new_id);
    assert_eq!(out, vec![empty]);
    // No turns: nothing assigned.
    let s = segment(4, vec![Word::new("x", 0, 100, 0.9)]);
    let out = label_segments(vec![s.clone()], &[], |_| Some(id(9)), &mut new_id);
    assert_eq!(out, vec![s]);
}

#[test]
fn diar_segment_speakers_are_the_labels_label_segments_stores() {
    // Speaker 1 has a turn but no word lands on it (T7.3b): it labels nothing.
    let turns = vec![turn(0, 0, 2000), turn(1, 2000, 2300), turn(2, 2300, 6000)];
    let segments = vec![
        segment(
            1,
            vec![
                Word::new("a", 100, 500, 0.9),
                Word::new("b", 2400, 2800, 0.9),
            ],
        ),
        segment(2, vec![Word::new("c", 4000, 4400, 0.9)]),
    ];
    let ids = |start: u8| {
        let mut next = start;
        move || {
            next += 1;
            id(next)
        }
    };
    let mut new_id = ids(50);
    let labelled = label_segment_speakers(segments.clone(), &turns, &mut new_id);
    let speakers: Vec<Option<usize>> = labelled.iter().map(|(_, k)| *k).collect();
    assert_eq!(speakers, [Some(0), Some(2), Some(2)]);
    assert!(labelled.iter().all(|(s, _)| s.speaker_id.is_none()));

    // The same split and ids as label_segments, which maps each label to its id.
    let mut new_id = ids(50);
    let stored = label_segments(segments, &turns, |k| Some(id(100 + k as u8)), &mut new_id);
    assert_eq!(stored.len(), labelled.len());
    for (s, (l, k)) in stored.iter().zip(&labelled) {
        let mut want = l.clone();
        want.speaker_id = k.map(|k| id(100 + k as u8));
        assert_eq!(*s, want);
    }
}

#[test]
fn diar_end_to_end_words_get_their_true_speaker() {
    let (turns, total) = round_robin(3, 2, 5000, 500);
    let d = run(&turns, total, DiarizationConfig::default());
    let r = assert_low_der(&turns, &d, "end to end");
    // Truth speaker k -> hypothesis label "S<j>".
    let map = |k: usize| -> usize {
        let (_, h) = r
            .mapping
            .iter()
            .find(|(rl, _)| *rl == format!("R{k}"))
            .unwrap();
        h[1..].parse().unwrap()
    };
    // One segment per turn, plus one segment spanning a speaker change.
    let mut segments = Vec::new();
    let mut truth = Vec::new();
    for (i, t) in turns.iter().enumerate() {
        let words: Vec<Word> = (0..8)
            .map(|j| {
                let s = t.start_ms + 100 + 600 * j;
                Word::new(format!("w{i}_{j}"), s, s + 400, 0.9)
            })
            .collect();
        segments.push(segment(i as u8, words));
        truth.push(map(t.speaker));
    }
    let mut next = 150u8;
    let mut new_id = || {
        next += 1;
        id(next)
    };
    let out = label_segments(segments, &d.turns, |k| Some(id(100 + k as u8)), &mut new_id);
    assert_eq!(out.len(), turns.len());
    for (s, want) in out.iter().zip(&truth) {
        assert_eq!(s.speaker_id, Some(id(100 + *want as u8)));
    }
    let words = assign_words(
        &[
            Word::new("x", turns[0].end_ms - 600, turns[0].end_ms - 200, 0.9),
            Word::new("y", turns[1].start_ms + 200, turns[1].start_ms + 600, 0.9),
        ],
        &d.turns,
    );
    assert_eq!(words, vec![Some(map(0)), Some(map(1))]);
}

// ---------------------------------------------------------------- regrouping (T12.66)

fn regroup(floor: f32, distance: f32, min_speech_s: f32, weighted: bool) -> Option<Regroup> {
    Some(Regroup {
        floor,
        distance,
        min_speech_s,
        weighted,
    })
}

fn regrouped(base: &ClusteringConfig, r: Option<Regroup>) -> ClusteringConfig {
    ClusteringConfig {
        regroup: r,
        ..base.clone()
    }
}

/// `dim`-dimensional unit vector at `degrees` in the plane of axes `a` and `b`.
fn in_plane(a: usize, b: usize, degrees: f32, dim: usize) -> Vec<f32> {
    let mut v = vec![0.0; dim];
    let r = degrees.to_radians();
    v[a] = r.cos();
    v[b] = r.sin();
    v
}

/// Two large clusters on axes 0 and 1 (30 points each, 1 s of speech each), then three small
/// clusters of 4 points (1 s each) of one voice in directions near axis 5 that stay apart at a
/// 0.3 threshold, then one small cluster of 4 points (1 s each) on axis 10, unlike all.
/// Returns the points, their speech and the index of the first voice point.
fn speakers_and_a_split_voice() -> (Vec<Vec<f32>>, Vec<f32>, usize) {
    let dim = 64;
    let (mut pts, _) = points(&[axis(0, dim), axis(1, dim)], 30, 0.02, 91);
    let first = pts.len();
    let mut n = Noise(95);
    let mut dirs: Vec<Vec<f32>> = (6..9)
        .map(|k| {
            let mut v = axis(5, dim);
            v[k] = 0.4;
            v
        })
        .collect();
    dirs.push(axis(10, dim));
    for d in &dirs {
        for _ in 0..4 {
            pts.push(d.iter().map(|&x| x + 0.01 * n.next()).collect());
        }
    }
    let speech = vec![1.0; pts.len()];
    (pts, speech, first)
}

#[test]
fn diar_cluster_regroup_never_changes_the_clustering_it_starts_from() {
    // The labels, centroids and unassigned flags are those of today's clustering whatever the
    // regroup setting (the inputs and expected bits of
    // `diar_cluster_unassigned_floor_of_minus_one_is_todays_clustering`); a setting nothing
    // qualifies for (floor -1) makes no new speaker.
    assert_eq!(ClusteringConfig::default().regroup, None);
    let c = unit_centroids(4, 64, 81);
    let (mut pts, _) = points(&c, 20, 0.05, 82);
    pts.extend(outliers(3, 64, 83));
    let speech = vec![0.5; pts.len()];
    let cases: [(ClusteringConfig, u64); 4] = [
        (cfg(PYANNOTE_THRESHOLD, 12), 0x309c_a7dc_18ec_7f5b),
        (cfg(0.2, 12), 0x4bdf_9397_468a_9b3e),
        (cfg(2.0, 12).with_num_speakers(4), 0x309c_a7dc_18ec_7f5b),
        (cfg(0.5, 1), 0x964c_e42f_4c55_66ac),
    ];
    for (base, hash) in cases {
        let today = cluster_embeddings(&pts, &base).unwrap();
        assert_eq!(centroid_hash(&today.centroids), hash, "{base:?}");
        assert!(today.new_speakers.is_empty());
        assert_eq!(today.new_speaker_of, vec![None; pts.len()]);
        let none = cluster_embeddings_with(&pts, Some(&speech), &base).unwrap();
        assert_eq!(none, today, "{base:?}");
        let nothing = cluster_embeddings_with(
            &pts,
            Some(&speech),
            &regrouped(&base, regroup(-1.0, 2.0, 0.0, false)),
        )
        .unwrap();
        assert_eq!(nothing, today, "{base:?}");
        for weighted in [false, true] {
            let all = cluster_embeddings_with(
                &pts,
                Some(&speech),
                &regrouped(&base, regroup(1.0, 2.0, 0.0, weighted)),
            )
            .unwrap();
            assert_eq!(all.labels, today.labels, "{base:?}");
            assert_eq!(all.centroids, today.centroids, "{base:?}");
            assert_eq!(all.unassigned, today.unassigned, "{base:?}");
        }
    }
}

#[test]
fn diar_cluster_regroup_makes_one_new_speaker_of_a_voice_split_into_small_clusters() {
    let (pts, speech, first) = speakers_and_a_split_voice();
    let base = cfg(0.3, 12);
    let today = cluster_embeddings(&pts, &base).unwrap();
    // Today the voice's three small clusters (and the unlike one) go to the two speakers.
    assert_eq!(today.centroids.len(), 2);
    let r = cluster_embeddings_with(
        &pts,
        Some(&speech),
        &regrouped(&base, regroup(0.3, 0.6, 8.0, false)),
    )
    .unwrap();
    assert_eq!(r.labels, today.labels);
    assert_eq!(r.centroids, today.centroids);
    assert_eq!(r.new_speakers.len(), 1);
    assert!(cosine(&r.new_speakers[0], &axis(5, 64)) > 0.9);
    // The voice's 12 points are the new speaker; the speakers and the unlike cluster (4 s,
    // below the 8 s minimum, and too far from the voice to join it) are not.
    assert!(
        r.new_speaker_of[first..first + 12]
            .iter()
            .all(|s| *s == Some(0))
    );
    assert!(r.new_speaker_of[..first].iter().all(Option::is_none));
    assert!(r.new_speaker_of[first + 12..].iter().all(Option::is_none));
    // At a distance below the clusters' spacing nothing groups, and 4 s each is not enough.
    let apart = cluster_embeddings_with(
        &pts,
        Some(&speech),
        &regrouped(&base, regroup(0.3, 0.3, 8.0, false)),
    )
    .unwrap();
    assert!(apart.new_speakers.is_empty());
    // A floor below the voice's cosine to the speakers (about 0) takes no candidate.
    let strict = cluster_embeddings_with(
        &pts,
        Some(&speech),
        &regrouped(&base, regroup(-0.5, 0.6, 8.0, false)),
    )
    .unwrap();
    assert!(strict.new_speakers.is_empty());
}

#[test]
fn diar_cluster_regroup_with_too_little_speech_falls_back_to_the_baseline() {
    let (pts, speech, _) = speakers_and_a_split_voice();
    let base = cfg(0.3, 12);
    let today = cluster_embeddings(&pts, &base).unwrap();
    for weighted in [false, true] {
        let r = cluster_embeddings_with(
            &pts,
            Some(&speech),
            &regrouped(&base, regroup(0.3, 0.6, 12.5, weighted)),
        )
        .unwrap();
        assert_eq!(r, today, "weighted {weighted}");
    }
    // Exactly the minimum is enough.
    let r = cluster_embeddings_with(
        &pts,
        Some(&speech),
        &regrouped(&base, regroup(0.3, 0.6, 12.0, false)),
    )
    .unwrap();
    assert_eq!(r.new_speakers.len(), 1);
}

#[test]
fn diar_cluster_regroup_weighted_and_plain_linkage_differ() {
    // Three single-point small clusters at 0, +30 and -30 degrees in the plane of axes 5 and
    // 6 (0.52 apart from 0 to each side). 0 and +30 merge first (lowest index on the tie).
    // Plain, their mean is 0.75 from -30, beyond 0.6; weighted, the 0-degree point's 10 s
    // pulls the mean to 0.54 from -30, so all three group.
    let dim = 64;
    let (mut pts, _) = points(&[axis(0, dim), axis(1, dim)], 30, 0.02, 91);
    let first = pts.len();
    for deg in [0.0, 30.0, -30.0] {
        pts.push(in_plane(5, 6, deg, dim));
    }
    let mut speech = vec![1.0; first];
    speech.extend([10.0, 0.5, 3.0]);
    let base = cfg(0.3, 12);
    let plain = cluster_embeddings_with(
        &pts,
        Some(&speech),
        &regrouped(&base, regroup(0.3, 0.6, 4.0, false)),
    )
    .unwrap();
    assert_eq!(plain.new_speakers.len(), 1);
    assert_eq!(&plain.new_speaker_of[first..], &[Some(0), Some(0), None]);
    let weighted = cluster_embeddings_with(
        &pts,
        Some(&speech),
        &regrouped(&base, regroup(0.3, 0.6, 4.0, true)),
    )
    .unwrap();
    assert_eq!(weighted.new_speakers.len(), 1);
    assert_eq!(
        &weighted.new_speaker_of[first..],
        &[Some(0), Some(0), Some(0)]
    );
    assert_eq!(plain.labels, weighted.labels);
    // A weighted group with no speech takes no part.
    speech[first + 2] = 0.0;
    let silent = cluster_embeddings_with(
        &pts,
        Some(&speech),
        &regrouped(&base, regroup(0.3, 0.6, 4.0, true)),
    )
    .unwrap();
    assert_eq!(&silent.new_speaker_of[first..], &[Some(0), Some(0), None]);
}

#[test]
fn diar_cluster_regroup_gives_embeddings_outside_the_training_subset_by_similarity() {
    // Only 60 of the 76 embeddings train: an embedding left out goes to the new speaker only
    // when it is more like it than like its own cluster's centroid.
    let (pts, speech, first) = speakers_and_a_split_voice();
    let base = ClusteringConfig {
        max_train_embeddings: 60,
        ..cfg(0.3, 12)
    };
    let today = cluster_embeddings(&pts, &base).unwrap();
    let r = cluster_embeddings_with(
        &pts,
        Some(&speech),
        &regrouped(&base, regroup(0.3, 0.6, 3.0, false)),
    )
    .unwrap();
    assert_eq!(r.labels, today.labels);
    assert_eq!(r.new_speakers.len(), 1, "{:?}", r.new_speaker_of);
    let voice = &r.new_speakers[0];
    for (i, p) in pts.iter().enumerate() {
        let to_new = cosine(p, voice);
        let to_own = cosine(p, &today.centroids[today.labels[i]]);
        if i < first || i >= first + 12 {
            assert_eq!(r.new_speaker_of[i], None, "{i}");
        } else {
            assert!(to_new > to_own, "{i}");
            assert_eq!(r.new_speaker_of[i], Some(0), "{i}");
        }
    }
}

#[test]
fn diar_cluster_regroup_is_validated_and_needs_speech() {
    let bad = [
        (-1.01, 0.8, 4.0),
        (1.01, 0.8, 4.0),
        (f32::NAN, 0.8, 4.0),
        (0.3, -0.01, 4.0),
        (0.3, 2.01, 4.0),
        (0.3, f32::NAN, 4.0),
        (0.3, 0.8, -1.0),
        (0.3, 0.8, f32::INFINITY),
        (0.3, 0.8, f32::NAN),
    ];
    for (f, d, m) in bad {
        let c = regrouped(&ClusteringConfig::default(), regroup(f, d, m, false));
        assert!(
            matches!(c.validate(), Err(DiarError::Config(_))),
            "{f} {d} {m}"
        );
    }
    let good = regrouped(&ClusteringConfig::default(), regroup(1.0, 2.0, 0.0, true));
    assert!(good.validate().is_ok());
    let (pts, speech, _) = speakers_and_a_split_voice();
    assert!(matches!(
        cluster_embeddings(&pts, &good),
        Err(DiarError::Config(_))
    ));
    assert!(matches!(
        cluster_embeddings_with(&pts, Some(&speech[1..]), &good),
        Err(DiarError::Shape(_))
    ));
    let mut negative = speech.clone();
    negative[3] = -1.0;
    assert!(matches!(
        cluster_embeddings_with(&pts, Some(&negative), &good),
        Err(DiarError::Shape(_))
    ));
}

/// For each 10 ms frame of `total_ms`, the speakers of `d` active at its centre.
fn frame_speakers(d: &Diarization, total_ms: u64) -> Vec<Vec<usize>> {
    (0..total_ms / 10)
        .map(|f| {
            let t = MeetingTime::from_millis(f * 10 + 5);
            let mut v: Vec<usize> = d
                .turns
                .iter()
                .filter(|x| x.start <= t && t < x.end)
                .map(|x| x.speaker)
                .collect();
            v.sort_unstable();
            v.dedup();
            v
        })
        .collect()
}

#[test]
fn diar_pipeline_regroup_moves_speech_only_to_new_speakers() {
    // Two speakers in long turns and a third voice in one short turn (a small cluster unlike
    // both). Regrouped with a floor above its cosine, its turn becomes a new speaker; no frame
    // moves from one of today's speakers to another, and with too high a minimum speech the
    // result is today's exactly.
    let mut turns = Vec::new();
    let mut t = 500;
    for r in 0..24 {
        turns.push(SceneTurn::new(r % 2, t, t + 6_000));
        t += 6_300;
        if r == 11 {
            turns.push(SceneTurn::new(2, t, t + 600));
            t += 1_100;
        }
    }
    let total = t + 500;
    let short = *turns.iter().find(|t| t.speaker == 2).unwrap();
    let today = run_exact(&turns, total, DiarizationConfig::default());
    assert_eq!(today.speakers.len(), 2);
    assert!(today.new_speakers.is_empty());
    let with = |r: Option<Regroup>| DiarizationConfig {
        clustering: ClusteringConfig {
            regroup: r,
            ..ClusteringConfig::default()
        },
        ..DiarizationConfig::default()
    };
    let d = run_exact(&turns, total, with(regroup(0.6, 0.8, 0.3, false)));
    assert_eq!(d.speakers.len(), 3);
    assert_eq!(d.new_speakers.len(), 1);
    let new = d.new_speakers[0];
    let mid = MeetingTime::from_millis(u64::midpoint(short.start_ms, short.end_ms));
    assert!(
        d.turns
            .iter()
            .any(|t| t.speaker == new && t.start <= mid && mid < t.end),
        "{:?}",
        d.turns
    );
    // Map each of d's old speakers to today's by the cluster it came from; they keep their
    // centroids too.
    assert_eq!(d.speaker_clusters.len(), d.speakers.len());
    let to_today: Vec<Option<usize>> = d
        .speakers
        .iter()
        .enumerate()
        .map(|(i, s)| {
            if d.new_speakers.contains(&i) {
                return None;
            }
            let old = today
                .speaker_clusters
                .iter()
                .position(|&c| c == d.speaker_clusters[i])
                .expect("an old speaker was one of today's");
            assert_eq!(today.speakers[old].centroid, s.centroid);
            Some(old)
        })
        .collect();
    let (was, now) = (frame_speakers(&today, total), frame_speakers(&d, total));
    let mut moved = 0;
    for (w, n) in was.iter().zip(&now) {
        for &s in n {
            match to_today[s] {
                Some(old) => assert!(w.contains(&old), "speech moved between speakers"),
                None => moved += 1,
            }
        }
    }
    assert!(moved > 0);
    // The new speaker's speech is the short turn's (within the window edges), and scored
    // against the full reference the regrouping finds the voice.
    assert!(score(&turns, &d).der() < score(&turns, &today).der());
    let too_little = run_exact(&turns, total, with(regroup(0.6, 0.8, 5.0, true)));
    assert_eq!(too_little, today);
}

/// The frame's speakers by today's marking: the `count` most active clusters with activity.
fn top_count(act: &[u32], count: usize) -> Vec<usize> {
    let mut order: Vec<usize> = (0..act.len()).filter(|&c| act[c] > 0).collect();
    order.sort_by(|&a, &b| act[b].cmp(&act[a]).then(a.cmp(&b)));
    order.truncate(count);
    order
}

#[test]
fn diar_pipeline_regroup_frame_never_lets_another_baseline_speaker_win_at_count_one() {
    // Count 1, A (cluster 0) active in 4 covering windows, B (cluster 1) in 3: the baseline
    // marks A. Two of A's locals move to new speaker N: A keeps 2, loses 2.
    let (a, b, n) = (0, 1, 2);
    assert_eq!(top_count(&[4, 3], 1), vec![a]);
    // Marking afresh over A 2, B 3, N 2 would hand B a frame it never had.
    assert_eq!(top_count(&[2, 3, 2], 1), vec![b]);
    // The rule keeps the baseline marking: a loss equal to what A kept does not replace it.
    assert_eq!(frame_with_new(&[a], &[2, 3], &[2, 0], 1), vec![a]);
    // Three of A's four moved: N takes A's frame; B still never wins.
    assert_eq!(frame_with_new(&[a], &[1, 3], &[3, 0], 1), vec![n]);
    // N is the new speaker that took most of A's activity (lowest on ties).
    assert_eq!(frame_with_new(&[a], &[0, 3], &[1, 2, 0, 0], 2), vec![3]);
    assert_eq!(frame_with_new(&[a], &[0, 3], &[2, 2, 0, 0], 2), vec![2]);
}

#[test]
fn diar_pipeline_regroup_frame_at_count_two_replaces_only_the_speaker_that_lost() {
    // Count 2, A and B marked. A loses 3 of 4 to N: N and B. B never moves to a new speaker it
    // did not lose to, and an unmarked cluster C (2) is never added.
    let n = 3;
    assert_eq!(
        frame_with_new(&[0, 1], &[1, 3, 5], &[3, 0, 0], 1),
        vec![n, 1]
    );
    // Both lose to N: N replaces only the one that lost most (A, 3 against 2).
    assert_eq!(
        frame_with_new(&[0, 1], &[1, 1, 5], &[3, 2, 0], 1),
        vec![n, 1]
    );
    // An equal loss goes to the lower speaker.
    assert_eq!(
        frame_with_new(&[0, 1], &[0, 0, 5], &[2, 2, 0], 1),
        vec![n, 1]
    );
    // Each loses to its own new speaker: both are replaced.
    assert_eq!(
        frame_with_new(&[0, 1], &[0, 0, 5], &[2, 0, 0, 2, 0, 0], 2),
        vec![n, n + 1]
    );
    // Nothing lost: the baseline marking.
    assert_eq!(
        frame_with_new(&[0, 1], &[4, 3, 5], &[0, 0, 0], 1),
        vec![0, 1]
    );
}

#[test]
fn diar_cluster_regroup_is_chosen_per_model() {
    // T12.74: CAM++ gets T12.66's pick from the dev rule; another model gets none (it is
    // clustered as before, never refused).
    let campplus = EmbeddingModelId::new("campplus-en-voxceleb@357a834f702b");
    assert_eq!(calibrated_regroup(&campplus), Some(CAMPPLUS_REGROUP));
    assert_eq!(
        CAMPPLUS_REGROUP,
        Regroup {
            floor: 0.60,
            distance: 0.80,
            min_speech_s: 8.0,
            weighted: false,
        }
    );
    assert!(CAMPPLUS_REGROUP.validate().is_ok());
    assert_eq!(
        calibrated_regroup(&EmbeddingModelId::new("campplus-en-voxceleb@000000000000")),
        None
    );
    for c in CALIBRATED_REGROUPS {
        assert!(c.source.contains("AMI dev"), "{}", c.model);
        assert!(!c.source.contains("test-clean"), "{}", c.model);
    }
    // Off unless a caller turns it on.
    assert_eq!(ClusteringConfig::default().regroup, None);
}
