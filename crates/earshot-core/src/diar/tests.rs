//! Unit tests for diarisation. Every test path contains `diar`, so
//! `cargo test -p earshot-core diar` selects them all.

use std::time::Duration;

use super::pipeline::{assign_locals, window_starts};
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
