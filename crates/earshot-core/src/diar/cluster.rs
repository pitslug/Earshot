//! Agglomerative clustering of speaker embeddings (centroid linkage on unit vectors).
//!
//! Follows the clustering stage of pyannote.audio 3.1 (`pyannote/audio/pipelines/
//! clustering.py`, `AgglomerativeClustering.cluster` and `BaseClustering`) with the
//! parameters of `pyannote/speaker-diarization-3.1` (`config.yaml`: `clustering.method:
//! centroid`, `min_cluster_size: 12`, `threshold: 0.7045654963945799`):
//!
//! 1. Embeddings are L2-normalised (pyannote divides by the norm when the metric is cosine
//!    and the method is centroid). At most `max_train_embeddings` of them (pyannote's
//!    `max_num_embeddings`, default 1000 in `BaseClustering`) take part in the hierarchy;
//!    the others are assigned to the nearest centroid at the end.
//! 2. Bottom-up merging with **centroid linkage**, as scipy
//!    `linkage(embeddings, method="centroid", metric="euclidean")`: the distance between
//!    two clusters is the Euclidean distance between the means of their unit-length
//!    members (means are not re-normalised), so pyannote's threshold applies unchanged.
//!    The whole dendrogram is built. Flat clusters are those of scipy
//!    `fcluster(criterion="distance")`: every merge before the first one farther apart
//!    than `threshold`. (Merges are made in order of distance; a later merge either
//!    involves a cluster that already contains a merge above the threshold or is itself
//!    farther apart, so stopping there is exact even though centroid linkage can produce
//!    inversions.)
//! 3. A cluster is *large* if it has at least `min(min_cluster_size, max(1, round(0.1
//!    n)))` members (n training embeddings, `round` ties to even as in Python).
//! 4. **Known or bounded speaker count** (pyannote.audio 3.1 `BaseClustering.
//!    set_num_clusters` and the `num_clusters` branch of `AgglomerativeClustering.
//!    cluster`): `min_speakers` and `max_speakers` are clamped to `[1, n]`. If the number
//!    of large clusters at the threshold is below `min_speakers`, above `max_speakers`, or
//!    differs from a fixed count (`min_speakers == max_speakers`), the dendrogram is
//!    re-cut: iterations are visited in order of `|merge distance - threshold|`, skipping
//!    merges that create a cluster smaller than the minimum size, and the first iteration
//!    with exactly the target number of large clusters is taken (else the closest one).
//!    Because the count is of *large* clusters, outlier singletons never count as
//!    speakers, so a forced count cannot merge two real speakers to keep an outlier.
//! 5. Small clusters are merged into the large cluster whose centroid is most
//!    cosine-similar (pyannote's small-cluster reassignment, `cdist(..., metric=
//!    "cosine")`), unless that best cosine is below [`ClusteringConfig::unassigned_floor`]:
//!    such a small cluster is kept apart and marked unassigned ([`Clustering::unassigned`]),
//!    so its speech gets no speaker rather than an unlike one (T12.65).
//! 6. **Regrouping small clusters** (T12.66, off in [`ClusteringConfig::default`], turned on
//!    by the final pass from [`CALIBRATED_REGROUPS`] since T12.74, [`ClusteringConfig::regroup`]):
//!    the small clusters of step 5 whose best cosine to every large centroid is below
//!    [`Regroup::floor`] are clustered among themselves (centroid linkage, each small cluster
//!    one node, merging while the closest pair is at most [`Regroup::distance`] apart), and a
//!    grouped cluster holding at least [`Regroup::min_speech_s`] of speech becomes a new
//!    speaker ([`Clustering::new_speakers`]). Steps 1-5 are unchanged by it: the labels and
//!    centroids are step 5's, and the new speakers are an extra output a caller may use
//!    ([`super::pipeline`] step 5). The rule and the measurement are
//!    `docs/far-speakers/short-turns.md`.
//!
//! The search keeps each cluster's nearest neighbour and recomputes it only for clusters
//! affected by a merge (the "generic" algorithm of D. Müllner, "Modern hierarchical,
//! agglomerative clustering algorithms", arXiv:1109.2378, 2011), so the hierarchy costs
//! about O(n^2 d) time and O(n d) memory. Ties are broken by the lowest index, so results
//! are deterministic.
//!
//! Deviations from pyannote 3.1 and their expected effect on DER:
//! - **Re-cut iteration order.** scipy sorts the linkage matrix by distance before
//!   pyannote numbers its iterations; here iterations are the merges in the order made.
//!   The two differ only after a centroid-linkage inversion. The state before the first
//!   merge is also a candidate (checked last), which scipy's dendrogram cannot express;
//!   it only matters when every embedding is a large cluster (fewer than 15 embeddings).
//!   Negligible DER effect.
//! - **Unreachable count.** When no iteration gives exactly the target, pyannote takes
//!   the closest iteration even if the threshold cut was as close; here the threshold cut
//!   is kept unless the re-cut is strictly closer, and if fewer than `min_speakers`
//!   clusters are then large, the biggest small clusters count as large (lowest member
//!   first on ties) until there are `min_speakers` (or no small cluster is left). This
//!   honours a known count that pyannote would undershoot (for example a short-spoken
//!   third voice); it can only lower DER relative to pyannote when the count is right,
//!   and raise it when the given count is too high. Only used when a count is given (not
//!   in the default pipeline).
//! - **No large cluster.** pyannote puts everything in one cluster; here (with
//!   `min_speakers` 1) the biggest cluster absorbs the rest. Same partition.
//! - **Subsampling.** pyannote shuffles with Python's unseeded `random`; here a fixed-seed
//!   xorshift draws the subset, so results are reproducible. No systematic DER effect.
//! - **Unassigned small clusters** (T12.65, not in pyannote). pyannote merges every small
//!   cluster into its most similar large cluster however unlike they are; here a small
//!   cluster whose best cosine is below `unassigned_floor` stays out and its speech is left
//!   without a speaker. Scored as missed speech, that turns confusion into missed speech
//!   (no DER change) where the small cluster was another talker, and raises DER where it
//!   belonged to the large cluster after all. The default floor, -1, is pyannote's
//!   behaviour bit for bit; a model's floor is chosen on dev data by the rule in
//!   `docs/far-speakers/unassigned-floor.md` ([`calibrated_unassigned_floor`]).
//! - **Regrouped small clusters** (T12.66, step 6, not in pyannote; on in the final pass since
//!   T12.74 through [`calibrated_regroup`]). pyannote gives a small cluster unlike every
//!   large one to the most similar large cluster; here such small clusters can become a new
//!   speaker of their own when, grouped among themselves, they hold enough speech. Expected
//!   effect on DER: lower where a short-turn talker was merged into another speaker (confusion
//!   turns into a correct speaker), slightly higher where the new speaker is a split of a
//!   speaker already found. On dev data it lowered the short-turn scenes' DER (0.1748 to
//!   0.1615) and changed no AMI dev meeting or long scene.
//! - The threshold was tuned by pyannote for its WeSpeaker ResNet34 embeddings. A
//!   different embedding model (CAM++) needs its own threshold, calibrated on dev data
//!   (T3.2/T3.5).

use super::types::{DiarError, EmbeddingModelId};

/// Clustering parameters.
#[derive(Debug, Clone, PartialEq)]
pub struct ClusteringConfig {
    /// Largest Euclidean distance (in `[0, 2]`) between the means of two clusters' unit
    /// embeddings that are still merged. Default 0.7045654963945799, pyannote/speaker-
    /// diarization-3.1 `config.yaml` `clustering.threshold` (tuned for its WeSpeaker
    /// embeddings); calibrate per embedding model on dev data
    /// ([`calibrated_cluster_threshold`], for example
    /// [`CAMPPLUS_DEV_CLEAN_CLUSTER_THRESHOLD`] for CAM++).
    pub threshold: f32,
    /// Lower bound on the number of speakers (1 = no lower bound): fewer large clusters at
    /// the threshold re-cut the dendrogram (module docs, step 4). Set together with
    /// `max_speakers` by [`ClusteringConfig::with_num_speakers`] for a known count.
    pub min_speakers: usize,
    /// Upper bound on the number of speakers: more large clusters at the threshold re-cut
    /// the dendrogram (module docs, step 4).
    pub max_speakers: Option<usize>,
    /// Clusters with fewer members (capped at 10% of the training embeddings, at least 1)
    /// are merged into the nearest large cluster. Default 12, pyannote/speaker-
    /// diarization-3.1 `config.yaml` `clustering.min_cluster_size`.
    pub min_cluster_size: usize,
    /// At most this many embeddings build the hierarchy; the rest are assigned to the
    /// nearest centroid. Default 1000, pyannote.audio 3.1 `BaseClustering`
    /// `max_num_embeddings`; it also bounds the hierarchy's cost.
    pub max_train_embeddings: usize,
    /// A small cluster whose highest cosine similarity to every large cluster's centroid is
    /// below this is left unassigned instead of merged (module docs, step 5). In `[-1, 1]`;
    /// default -1, which never leaves anything out (pyannote 3.1 bit for bit). A model's
    /// value comes from [`calibrated_unassigned_floor`].
    pub unassigned_floor: f32,
    /// Regrouping of small clusters like no large cluster into new speakers (module docs,
    /// step 6; T12.66, `docs/far-speakers/short-turns.md`). Default `None`: off, today's
    /// clustering. Evaluation only: nothing in the app sets it.
    pub regroup: Option<Regroup>,
}

/// Settings of the small-cluster regrouping (T12.66, module docs step 6).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Regroup {
    /// A small cluster whose highest cosine to every large centroid is below this is a
    /// candidate group (the test of [`ClusteringConfig::unassigned_floor`]); at 1 or above
    /// every small cluster is one. In `[-1, 1]`.
    pub floor: f32,
    /// Candidate groups merge while the closest pair's centroid distance (Euclidean between
    /// the means of their members' unit embeddings, as step 2) is at most this. In `[0, 2]`.
    pub distance: f32,
    /// A grouped cluster becomes a new speaker only with at least this much speech (seconds,
    /// the sum of its members' speech estimates). Finite and `>= 0`.
    pub min_speech_s: f32,
    /// Weighted linkage: each member embedding counts in a mean by its speech estimate
    /// instead of once. A candidate group with no speech at all then takes no part.
    pub weighted: bool,
}

impl Regroup {
    /// Checks parameter ranges.
    ///
    /// # Errors
    /// [`DiarError::Config`] for a floor outside `[-1, 1]`, a distance outside `[0, 2]` or a
    /// negative or non-finite minimum speech.
    pub fn validate(&self) -> Result<(), DiarError> {
        if !(-1.0..=1.0).contains(&self.floor) {
            return Err(DiarError::Config(
                "the regroup floor must be a cosine in [-1, 1]".to_owned(),
            ));
        }
        if !(0.0..=2.0).contains(&self.distance) {
            return Err(DiarError::Config(
                "the regroup distance must be in [0, 2]".to_owned(),
            ));
        }
        if !(self.min_speech_s.is_finite() && self.min_speech_s >= 0.0) {
            return Err(DiarError::Config(
                "the regroup minimum speech must be finite and >= 0".to_owned(),
            ));
        }
        Ok(())
    }
}

impl Default for ClusteringConfig {
    fn default() -> Self {
        ClusteringConfig {
            threshold: 0.704_565_5,
            min_speakers: 1,
            max_speakers: None,
            min_cluster_size: 12,
            max_train_embeddings: 1000,
            unassigned_floor: -1.0,
            regroup: None,
        }
    }
}

/// A clustering threshold ([`ClusteringConfig::threshold`]) fixed on dev data for one exact
/// embedding model.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CalibratedClusterThreshold {
    /// Exact [`EmbeddingModelId`] string.
    pub model: &'static str,
    /// Euclidean distance threshold between centroids.
    pub threshold: f32,
    /// How it was obtained.
    pub source: &'static str,
}

/// Clustering thresholds calibrated on LibriSpeech dev-clean only (never on test-clean or
/// AMI, the acceptance sets). Reproduced by `cargo test --release -p earshot-onnx diar_calibration --
/// --include-ignored` (the full calibration is `#[ignore]`d in the default run),
/// which fails if the value here differs from a fresh calibration.
pub const CALIBRATED_CLUSTER_THRESHOLDS: &[CalibratedClusterThreshold] =
    &[CalibratedClusterThreshold {
        model: "campplus-en-voxceleb@357a834f702b",
        threshold: CAMPPLUS_DEV_CLEAN_CLUSTER_THRESHOLD,
        source: "LibriSpeech dev-clean, 40 speakers (fixtures/librispeech/subset.toml): 20 \
                 synthetic scenes of 2-6 speakers (4 per count), 1.5-8 s turns, 20% of turn \
                 changes overlapped by 0.2-1.0 s, 15 s of speech per speaker; pyannote \
                 segmentation-3.0 + CAM++; grid 0.30-1.30 step 0.01; lowest corpus DER (NIST \
                 0.25 s per-side collar, overlap scored), ties by mean |speaker count error|",
    }];

/// The CAM++ VoxCeleb clustering threshold from the dev-clean calibration (see
/// [`CALIBRATED_CLUSTER_THRESHOLDS`]), measured 2026-10-18 on 1304 s of dev-clean scenes:
/// corpus DER 0.1782 and mean |speaker count error| 0.50 at 0.68. The curve has a broad
/// valley from 0.68 to 0.85 (DER 0.178-0.189, count error 0.40-0.55) between steep walls
/// (DER 0.27 at 0.60 and at 0.95, 0.48 at 0.50 and at 1.00). pyannote's WeSpeaker value
/// (0.70) sits inside the valley (DER 0.1792). These DER values are pessimistic (reference
/// turns include each utterance's own pauses) and not comparable with AMI DER; only the
/// minimum's position matters. No scene reuses a speaker's audio (at most 77% of one
/// speaker's stream is used; re-verified after the T3.2b review, same curve).
pub const CAMPPLUS_DEV_CLEAN_CLUSTER_THRESHOLD: f32 = 0.68;

/// The dev-calibrated clustering threshold for `model`, or `None` if it has none (a new or
/// changed embedding model must be calibrated on dev data first; pyannote's default
/// [`ClusteringConfig::default`] threshold is tuned for WeSpeaker only).
#[must_use]
pub fn calibrated_cluster_threshold(model: &EmbeddingModelId) -> Option<f32> {
    CALIBRATED_CLUSTER_THRESHOLDS
        .iter()
        .find(|c| c.model == model.as_str())
        .map(|c| c.threshold)
}

/// An unassigned floor ([`ClusteringConfig::unassigned_floor`]) chosen on dev data for one
/// exact embedding model.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CalibratedUnassignedFloor {
    /// Exact [`EmbeddingModelId`] string.
    pub model: &'static str,
    /// Cosine floor in `[-1, 1]`; -1 leaves nothing unassigned.
    pub floor: f32,
    /// How it was obtained.
    pub source: &'static str,
}

/// Unassigned floors chosen by the rule in `docs/far-speakers/unassigned-floor.md` (T12.65)
/// on dev data only (AMI dev mixes and dev-clean scenes, never the acceptance sets), measured
/// by `cargo run --release -p earshot-runtime --example cluster_sweep -- <repo> --models <dir>
/// --floors`.
pub const CALIBRATED_UNASSIGNED_FLOORS: &[CalibratedUnassignedFloor] =
    &[CalibratedUnassignedFloor {
        model: "campplus-en-voxceleb@357a834f702b",
        floor: CAMPPLUS_UNASSIGNED_FLOOR,
        source: "T12.65 rule (docs/far-speakers/unassigned-floor.md) on the five AMI dev \
                 mixes and 16 dev-clean long scenes at clustering threshold 0.68: no floor on \
                 the grid 0.00-0.60 passed (right speech left out was 4-9 times the wrong \
                 speech left out), so -1, today's clustering",
    }];

/// The CAM++ VoxCeleb unassigned floor (see [`CALIBRATED_UNASSIGNED_FLOORS`]): -1, nothing left
/// unassigned, because no floor passed the T12.65 rule on dev data (2026-11-16; the measurement
/// is `docs/far-speakers/unassigned-floor.json`). A new value needs a new rule run.
pub const CAMPPLUS_UNASSIGNED_FLOOR: f32 = -1.0;

/// The dev-chosen unassigned floor for `model`, or `None` if it has none (a new embedding
/// model needs its own, like its clustering threshold).
#[must_use]
pub fn calibrated_unassigned_floor(model: &EmbeddingModelId) -> Option<f32> {
    CALIBRATED_UNASSIGNED_FLOORS
        .iter()
        .find(|c| c.model == model.as_str())
        .map(|c| c.floor)
}

/// A small-cluster regrouping ([`ClusteringConfig::regroup`], module docs step 6) chosen on dev
/// data for one exact embedding model.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CalibratedRegroup {
    /// Exact [`EmbeddingModelId`] string.
    pub model: &'static str,
    /// The settings.
    pub regroup: Regroup,
    /// How they were obtained.
    pub source: &'static str,
}

/// Regroupings the final pass turns on (T12.74), each chosen by a rule on dev data only (AMI
/// dev mixes and dev-clean scenes, never the acceptance sets). Measured by `cargo run --release
/// -p earshot-runtime --example cluster_sweep -- <repo> --models <dir> --short-turns` (the grid)
/// and `--training-cap` (the check on all fifteen AMI dev meetings).
pub const CALIBRATED_REGROUPS: &[CalibratedRegroup] = &[CalibratedRegroup {
    model: "campplus-en-voxceleb@357a834f702b",
    regroup: CAMPPLUS_REGROUP,
    source: "T12.66 rule (docs/far-speakers/short-turns.md) on the five AMI dev mixes, 16              dev-clean long scenes and 10 short-turn scenes at clustering threshold 0.68: f 0.60,              d 0.80, m 8 s, plain finds 9 of 10 short-turn talkers (6 today) with no new speaker              on any long scene or AMI meeting; T12.85 (docs/far-speakers/training-cap.md)              picked it again over all fifteen AMI dev meetings, none changed",
}];

/// The CAM++ VoxCeleb regrouping (see [`CALIBRATED_REGROUPS`]): small clusters below cosine
/// 0.60 to every large cluster, grouped while at most 0.80 apart, a new speaker from 8 s of
/// speech, plain means.
pub const CAMPPLUS_REGROUP: Regroup = Regroup {
    floor: 0.60,
    distance: 0.80,
    min_speech_s: 8.0,
    weighted: false,
};

/// The dev-chosen regrouping for `model`, or `None` (a model without one is clustered without
/// regrouping, never refused: a missing setting must not cost a meeting).
#[must_use]
pub fn calibrated_regroup(model: &EmbeddingModelId) -> Option<Regroup> {
    CALIBRATED_REGROUPS
        .iter()
        .find(|c| c.model == model.as_str())
        .map(|c| c.regroup)
}

impl ClusteringConfig {
    /// The same configuration with a known count of `n` speakers: the result has `n`
    /// clusters when the dendrogram allows it (and there are at least `n` embeddings),
    /// otherwise as close to `n` as it allows (module docs, step 4).
    #[must_use]
    pub fn with_num_speakers(mut self, n: usize) -> Self {
        self.min_speakers = n.max(1);
        self.max_speakers = Some(n.max(1));
        self
    }

    /// Checks parameter ranges.
    ///
    /// # Errors
    /// [`DiarError::Config`] for a threshold outside `[0, 2]`, `min_speakers == 0`,
    /// `max_speakers < min_speakers`, `max_train_embeddings == 0`, an unassigned floor
    /// outside `[-1, 1]`, or invalid [`Regroup`] settings.
    pub fn validate(&self) -> Result<(), DiarError> {
        if let Some(r) = &self.regroup {
            r.validate()?;
        }
        if !(-1.0..=1.0).contains(&self.unassigned_floor) {
            return Err(DiarError::Config(
                "the unassigned floor must be a cosine in [-1, 1]".to_owned(),
            ));
        }
        if !(0.0..=2.0).contains(&self.threshold) {
            return Err(DiarError::Config(
                "clustering threshold must be a distance in [0, 2]".to_owned(),
            ));
        }
        if self.min_speakers == 0 {
            return Err(DiarError::Config("min_speakers must be >= 1".to_owned()));
        }
        if self.max_speakers.is_some_and(|m| m < self.min_speakers) {
            return Err(DiarError::Config(
                "max_speakers must be >= min_speakers".to_owned(),
            ));
        }
        if self.max_train_embeddings == 0 {
            return Err(DiarError::Config(
                "max_train_embeddings must be >= 1".to_owned(),
            ));
        }
        Ok(())
    }
}

/// Result of [`cluster_embeddings`].
#[derive(Debug, Clone, PartialEq)]
pub struct Clustering {
    /// Cluster of each input embedding, numbered `0..centroids.len()` in order of first
    /// appearance in the input.
    pub labels: Vec<usize>,
    /// Unit-length centroid of each cluster (normalised mean of its unit-length members).
    pub centroids: Vec<Vec<f32>>,
    /// Per cluster: a small cluster left unassigned because it is like no large cluster
    /// (module docs, step 5). Always all false with the default floor.
    pub unassigned: Vec<bool>,
    /// Unit-length centroid of each new speaker made by the regrouping (module docs, step 6;
    /// the normalised plain mean of its members' unit embeddings), in order of its lowest
    /// small cluster. Empty unless [`ClusteringConfig::regroup`] is set.
    pub new_speakers: Vec<Vec<f32>>,
    /// Per input embedding, the new speaker it belongs to (an index into
    /// [`Clustering::new_speakers`]). A training embedding belongs to the new speaker made
    /// from its small cluster; an embedding outside the training subset belongs to the most
    /// similar new speaker only when it is more similar to it than to its cluster's centroid.
    /// All `None` unless [`ClusteringConfig::regroup`] is set.
    pub new_speaker_of: Vec<Option<usize>>,
}

/// Clusters `embeddings` (all the same dimension, any scale). See the module docs. The same as
/// [`cluster_embeddings_with`] with no speech estimates, so a [`ClusteringConfig::regroup`]
/// setting is refused.
///
/// # Errors
/// [`DiarError::Config`] for an invalid `cfg` or one with a regroup setting,
/// [`DiarError::Shape`] for differing dimensions or a non-finite or all-zero embedding.
pub fn cluster_embeddings(
    embeddings: &[Vec<f32>],
    cfg: &ClusteringConfig,
) -> Result<Clustering, DiarError> {
    cluster_embeddings_with(embeddings, None, cfg)
}

/// [`cluster_embeddings`] with each embedding's speech estimate in seconds (`speech[i]` for
/// `embeddings[i]`), which the regrouping (module docs, step 6) needs for its minimum speech
/// and weighting. Without a regroup setting the speech estimates change nothing.
///
/// # Errors
/// [`DiarError::Config`] for an invalid `cfg` or a regroup setting without `speech`;
/// [`DiarError::Shape`] for differing dimensions, a non-finite or all-zero embedding, a
/// `speech` of another length or a negative or non-finite speech estimate.
pub fn cluster_embeddings_with(
    embeddings: &[Vec<f32>],
    speech: Option<&[f32]>,
    cfg: &ClusteringConfig,
) -> Result<Clustering, DiarError> {
    cfg.validate()?;
    let n = embeddings.len();
    if let Some(sp) = speech {
        if sp.len() != n {
            return Err(DiarError::Shape(format!(
                "{} speech estimates for {n} embeddings",
                sp.len()
            )));
        }
        if sp.iter().any(|x| !(x.is_finite() && *x >= 0.0)) {
            return Err(DiarError::Shape(
                "a speech estimate is negative or non-finite".to_owned(),
            ));
        }
    }
    let regroup = match (&cfg.regroup, speech) {
        (None, _) => None,
        (Some(r), Some(sp)) => Some((*r, sp)),
        (Some(_), None) => {
            return Err(DiarError::Config(
                "regrouping small clusters needs each embedding's speech".to_owned(),
            ));
        }
    };
    if n == 0 {
        return Ok(Clustering {
            labels: Vec::new(),
            centroids: Vec::new(),
            unassigned: Vec::new(),
            new_speakers: Vec::new(),
            new_speaker_of: Vec::new(),
        });
    }
    let dim = embeddings[0].len();
    let mut units = Vec::with_capacity(n);
    for (i, e) in embeddings.iter().enumerate() {
        if e.len() != dim || dim == 0 {
            return Err(DiarError::Shape(format!(
                "embedding {i} has dimension {}, expected {dim} (> 0)",
                e.len()
            )));
        }
        let u = normalized(e)
            .ok_or_else(|| DiarError::Shape(format!("embedding {i} is non-finite or all zeros")))?;
        units.push(u);
    }

    let train = training_subset(n, cfg.max_train_embeddings);
    let m = train.len();

    // Hierarchy over the training subset. Each cluster keeps the sum of its unit members.
    let mut sums: Vec<Vec<f64>> = train
        .iter()
        .map(|&i| units[i].iter().map(|&x| f64::from(x)).collect())
        .collect();
    let mut alive: Vec<bool> = vec![true; m];
    let mut sizes: Vec<f64> = vec![1.0; m];
    // Centroid linkage: Euclidean distance between the (un-normalised) means.
    let dist = |sums: &[Vec<f64>], sizes: &[f64], a: usize, b: usize| -> f64 {
        let (wa, wb) = (1.0 / sizes[a], 1.0 / sizes[b]);
        sums[a]
            .iter()
            .zip(&sums[b])
            .map(|(x, y)| {
                let d = x * wa - y * wb;
                d * d
            })
            .sum::<f64>()
            .sqrt()
    };
    // nn[a] = (distance, b): nearest live neighbour of a, lowest index on ties.
    let mut nn: Vec<(f64, usize)> = vec![(f64::INFINITY, usize::MAX); m];
    let recompute = |a: usize, sums: &[Vec<f64>], sizes: &[f64], alive: &[bool]| {
        let mut best = (f64::INFINITY, usize::MAX);
        for (b, &live) in alive.iter().enumerate() {
            if b != a && live {
                let d = dist(sums, sizes, a, b);
                if d < best.0 {
                    best = (d, b);
                }
            }
        }
        best
    };
    for (a, slot) in nn.iter_mut().enumerate() {
        *slot = recompute(a, &sums, &sizes, &alive);
    }
    // Full dendrogram: merges in the order made, each into the lower slot.
    let mut merges: Vec<Merge> = Vec::with_capacity(m.saturating_sub(1));
    for _ in 1..m {
        // Closest pair (lowest index on ties).
        let mut a = usize::MAX;
        for k in 0..m {
            if alive[k] && (a == usize::MAX || nn[k].0 < nn[a].0) {
                a = k;
            }
        }
        let (d, b) = nn[a];
        // Merge into the lower index so labels stay stable.
        let (keep, gone) = if a < b { (a, b) } else { (b, a) };
        let gone_sum = std::mem::take(&mut sums[gone]);
        for (x, y) in sums[keep].iter_mut().zip(&gone_sum) {
            *x += y;
        }
        sizes[keep] += sizes[gone];
        alive[gone] = false;
        merges.push(Merge {
            keep,
            gone,
            distance: d,
            size: sizes[keep] as usize,
        });
        // Update nearest neighbours.
        nn[keep] = recompute(keep, &sums, &sizes, &alive);
        for k in 0..m {
            if !alive[k] || k == keep {
                continue;
            }
            if nn[k].1 == keep || nn[k].1 == gone {
                nn[k] = recompute(k, &sums, &sizes, &alive);
            } else {
                let dk = dist(&sums, &sizes, k, keep);
                if dk < nn[k].0 || (dk == nn[k].0 && keep < nn[k].1) {
                    nn[k] = (dk, keep);
                }
            }
        }
    }

    let min_size = cfg.min_cluster_size.min(round_half_even_tenth(m).max(1));
    let threshold = f64::from(cfg.threshold);
    // fcluster(criterion="distance"): every merge before the first one above the threshold.
    let at_threshold = merges
        .iter()
        .position(|mg| mg.distance > threshold)
        .unwrap_or(merges.len());
    let mut groups = cut(&merges, at_threshold, m);
    let mut num_large = count_large(&groups, min_size);
    if let Some(target) = target_count(cfg, m, num_large) {
        let (k, n) = recut(&merges, threshold, m, min_size, target);
        // Keep the threshold cut unless the re-cut gets closer to the target.
        if n.abs_diff(target) < num_large.abs_diff(target) {
            groups = cut(&merges, k, m);
            num_large = n;
        }
    }

    // Large clusters; if fewer than `min_speakers` (clamped to `m`), the biggest small
    // clusters count as large too (lowest member first on ties).
    let mut large: Vec<bool> = groups.iter().map(|g| g.len() >= min_size).collect();
    let min_c = cfg.min_speakers.min(m).max(1);
    if num_large < min_c {
        let mut small: Vec<usize> = (0..groups.len()).filter(|&g| !large[g]).collect();
        small.sort_by(|&x, &y| groups[y].len().cmp(&groups[x].len()).then(x.cmp(&y)));
        for &g in small.iter().take(min_c - num_large) {
            large[g] = true;
        }
    }

    // Small clusters -> the large cluster with the most cosine-similar centroid.
    let sum_of = |mem: &[usize]| -> Vec<f64> {
        let mut s = vec![0.0f64; dim];
        for &t in mem {
            for (x, &y) in s.iter_mut().zip(&units[train[t]]) {
                *x += f64::from(y);
            }
        }
        s
    };
    let mut final_members: Vec<Vec<usize>> = Vec::new();
    let mut small_groups: Vec<Vec<usize>> = Vec::new();
    for (g, is_large) in groups.into_iter().zip(large) {
        if is_large {
            final_members.push(g);
        } else {
            small_groups.push(g);
        }
    }
    let large_centroids: Vec<Vec<f64>> = final_members.iter().map(|g| sum_of(g)).collect();
    // A floor of -1 never leaves anything out (exactly pyannote's step 5).
    let floor = f64::from(cfg.unassigned_floor);
    let mut left_out: Vec<Vec<usize>> = Vec::new();
    // Step 6's candidate groups (T12.66): small clusters merged here whose best cosine is
    // below the regroup floor. Collected only with a regroup setting.
    let mut candidates: Vec<Vec<usize>> = Vec::new();
    for g in small_groups {
        let (target, best) = nearest_with_cosine(&sum_of(&g), &large_centroids);
        if floor > -1.0 && best < floor {
            left_out.push(g);
        } else {
            if let Some((r, _)) = &regroup
                && (r.floor >= 1.0 || best < f64::from(r.floor))
            {
                candidates.push(g.clone());
            }
            final_members[target].extend(g);
        }
    }
    let assigned_count = final_members.len();
    final_members.extend(left_out);

    // Final centroids from the training members, then every embedding to its nearest one.
    let centroids: Vec<Vec<f64>> = final_members
        .iter()
        .map(|mem| {
            let mut s = vec![0.0f64; dim];
            for &t in mem {
                for (x, &y) in s.iter_mut().zip(&units[train[t]]) {
                    *x += f64::from(y);
                }
            }
            s
        })
        .collect();
    let mut raw_labels = vec![0usize; n];
    for (lab, mem) in final_members.iter().enumerate() {
        for &t in mem {
            raw_labels[train[t]] = lab;
        }
    }
    let in_train = {
        let mut v = vec![false; n];
        for &i in &train {
            v[i] = true;
        }
        v
    };
    for i in 0..n {
        if !in_train[i] {
            let u: Vec<f64> = units[i].iter().map(|&x| f64::from(x)).collect();
            raw_labels[i] = nearest(&u, &centroids);
        }
    }

    // Relabel by first appearance.
    let mut order: Vec<Option<usize>> = vec![None; centroids.len()];
    let mut next = 0;
    let mut labels = Vec::with_capacity(n);
    for &r in &raw_labels {
        let l = *order[r].get_or_insert_with(|| {
            next += 1;
            next - 1
        });
        labels.push(l);
    }
    let mut out_centroids = vec![Vec::new(); next];
    let mut unassigned = vec![false; next];
    for (r, l) in order.iter().enumerate() {
        if let Some(l) = l {
            let nrm = norm(&centroids[r]).max(f64::MIN_POSITIVE);
            out_centroids[*l] = centroids[r].iter().map(|&x| (x / nrm) as f32).collect();
            unassigned[*l] = r >= assigned_count;
        }
    }
    let (new_speakers, new_speaker_of) = match regroup {
        None => (Vec::new(), vec![None; n]),
        Some((r, sp)) => {
            let embedding_of = |t: usize| train[t];
            let speech_of = |t: usize| f64::from(sp[train[t]]);
            let made = regroup_candidates(&candidates, &units, &speech_of, &r, dim, &embedding_of);
            let mut of = vec![None; n];
            for (s, (_, members)) in made.iter().enumerate() {
                for &t in members {
                    of[train[t]] = Some(s);
                }
            }
            let new_sums: Vec<Vec<f64>> = made.into_iter().map(|(sum, _)| sum).collect();
            if !new_sums.is_empty() {
                for i in (0..n).filter(|&i| !in_train[i]) {
                    let u: Vec<f64> = units[i].iter().map(|&x| f64::from(x)).collect();
                    let (s, to_new) = nearest_with_cosine(&u, &new_sums);
                    let own = raw_labels[i];
                    let (_, to_own) = nearest_with_cosine(&u, &centroids[own..=own]);
                    if to_new > to_own {
                        of[i] = Some(s);
                    }
                }
            }
            let speakers = new_sums
                .iter()
                .map(|c| {
                    let nrm = norm(c).max(f64::MIN_POSITIVE);
                    c.iter().map(|&x| (x / nrm) as f32).collect()
                })
                .collect();
            (speakers, of)
        }
    };
    Ok(Clustering {
        labels,
        centroids: out_centroids,
        unassigned,
        new_speakers,
        new_speaker_of,
    })
}

/// Step 6 (T12.66): clusters the candidate groups (each a list of training indices `t`, whose
/// unit embedding is `units[embedding_of(t)]` and speech estimate `speech_of(t)`) among
/// themselves by centroid linkage while the closest pair is at most `r.distance` apart (lowest
/// index on ties), and keeps every grouped cluster with at least `r.min_speech_s` of speech.
/// Returns, per new speaker in order of its lowest group, the plain sum of its members' unit
/// embeddings and its members.
fn regroup_candidates(
    candidates: &[Vec<usize>],
    units: &[Vec<f32>],
    speech_of: &dyn Fn(usize) -> f64,
    r: &Regroup,
    dim: usize,
    embedding_of: &dyn Fn(usize) -> usize,
) -> Vec<(Vec<f64>, Vec<usize>)> {
    // Linkage sums and sizes: plain counts every member once, weighted by its speech.
    let weight = |t: usize| if r.weighted { speech_of(t) } else { 1.0 };
    let mut sums: Vec<Vec<f64>> = Vec::with_capacity(candidates.len());
    let mut sizes: Vec<f64> = Vec::with_capacity(candidates.len());
    let mut members: Vec<Vec<usize>> = candidates.to_vec();
    for g in candidates {
        let mut s = vec![0.0f64; dim];
        let mut total = 0.0;
        for &t in g {
            let w = weight(t);
            total += w;
            for (x, &y) in s.iter_mut().zip(&units[embedding_of(t)]) {
                *x += w * f64::from(y);
            }
        }
        sums.push(s);
        sizes.push(total);
    }
    // A weighted group with no speech has no mean: it takes no part (stays where step 5 put
    // it, since it could not reach a positive minimum speech on its own either).
    let mut alive: Vec<bool> = sizes.iter().map(|&w| w > 0.0).collect();
    let limit = f64::from(r.distance);
    loop {
        // Closest live pair, lowest (a, b) on ties.
        let mut best: Option<(f64, usize, usize)> = None;
        for a in (0..sums.len()).filter(|&a| alive[a]) {
            for b in (a + 1..sums.len()).filter(|&b| alive[b]) {
                let (wa, wb) = (1.0 / sizes[a], 1.0 / sizes[b]);
                let d = sums[a]
                    .iter()
                    .zip(&sums[b])
                    .map(|(x, y)| {
                        let d = x * wa - y * wb;
                        d * d
                    })
                    .sum::<f64>()
                    .sqrt();
                if best.is_none_or(|(bd, _, _)| d < bd) {
                    best = Some((d, a, b));
                }
            }
        }
        let Some((d, a, b)) = best else { break };
        if d > limit {
            break;
        }
        let gone = std::mem::take(&mut sums[b]);
        for (x, y) in sums[a].iter_mut().zip(&gone) {
            *x += y;
        }
        sizes[a] += sizes[b];
        let moved = std::mem::take(&mut members[b]);
        members[a].extend(moved);
        alive[b] = false;
    }
    let min_speech = f64::from(r.min_speech_s);
    let mut out = Vec::new();
    for (a, mem) in members.into_iter().enumerate() {
        if !alive[a] {
            continue;
        }
        let speech: f64 = mem.iter().map(|&t| speech_of(t)).sum();
        if speech < min_speech {
            continue;
        }
        let mut s = vec![0.0f64; dim];
        for &t in &mem {
            for (x, &y) in s.iter_mut().zip(&units[embedding_of(t)]) {
                *x += f64::from(y);
            }
        }
        out.push((s, mem));
    }
    out
}

/// One row of the dendrogram: cluster slot `gone` merged into slot `keep` (`keep < gone`;
/// a slot is the lowest training index of its cluster) at centroid distance `distance`,
/// giving a cluster of `size` members.
#[derive(Debug, Clone, Copy)]
struct Merge {
    keep: usize,
    gone: usize,
    distance: f64,
    size: usize,
}

/// Flat clusters of `m` training embeddings after the first `k` merges, each a list of
/// training indices, in order of their lowest member.
fn cut(merges: &[Merge], k: usize, m: usize) -> Vec<Vec<usize>> {
    fn find(parent: &mut [usize], mut x: usize) -> usize {
        while parent[x] != x {
            parent[x] = parent[parent[x]];
            x = parent[x];
        }
        x
    }
    let mut parent: Vec<usize> = (0..m).collect();
    for mg in &merges[..k] {
        let (a, b) = (find(&mut parent, mg.keep), find(&mut parent, mg.gone));
        let (lo, hi) = if a < b { (a, b) } else { (b, a) };
        parent[hi] = lo;
    }
    let mut slot: Vec<Option<usize>> = vec![None; m];
    let mut groups: Vec<Vec<usize>> = Vec::new();
    for t in 0..m {
        let r = find(&mut parent, t);
        let g = *slot[r].get_or_insert_with(|| {
            groups.push(Vec::new());
            groups.len() - 1
        });
        groups[g].push(t);
    }
    groups
}

/// Number of clusters with at least `min_size` members.
fn count_large(groups: &[Vec<usize>], min_size: usize) -> usize {
    groups.iter().filter(|g| g.len() >= min_size).count()
}

/// The number of large clusters to re-cut the dendrogram for, or `None` to keep the
/// threshold cut. pyannote.audio 3.1 `BaseClustering.set_num_clusters` (bounds clamped to
/// `[1, m]`; equal bounds fix the count) and `AgglomerativeClustering.cluster` (a count
/// of large clusters outside the bounds is forced to the nearest bound).
fn target_count(cfg: &ClusteringConfig, m: usize, num_large: usize) -> Option<usize> {
    let min_c = cfg.min_speakers.min(m).max(1);
    let max_c = cfg.max_speakers.unwrap_or(m).min(m).max(1);
    let target = if num_large < min_c {
        Some(min_c)
    } else if num_large > max_c {
        Some(max_c)
    } else {
        (min_c == max_c).then_some(min_c)
    };
    target.filter(|&t| t != num_large)
}

/// Number of merges to apply so that the number of large clusters is as close as possible
/// to `target`: pyannote.audio 3.1 `AgglomerativeClustering.cluster`
/// (`pyannote/audio/pipelines/clustering.py`, the `num_clusters` branch). Iterations are
/// visited in order of `|distance - threshold|`, skipping merges that create a cluster
/// smaller than `min_size` (they cannot change the count of large clusters); the first
/// iteration with exactly `target` large clusters wins, otherwise the closest one (all
/// merged, one large cluster, if none is closer). Unlike scipy's dendrogram, the state
/// before the first merge is also a candidate, checked last. Returns the number of merges
/// and the number of large clusters it gives.
fn recut(
    merges: &[Merge],
    threshold: f64,
    m: usize,
    min_size: usize,
    target: usize,
) -> (usize, usize) {
    let mut order: Vec<usize> = (0..merges.len()).collect();
    order.sort_by(|&x, &y| {
        (merges[x].distance - threshold)
            .abs()
            .total_cmp(&(merges[y].distance - threshold).abs())
    });
    let mut best = (merges.len(), 1usize);
    for it in order {
        if merges[it].size < min_size {
            continue;
        }
        let n = count_large(&cut(merges, it + 1, m), min_size);
        if n == target {
            return (it + 1, n);
        }
        if n.abs_diff(target) < best.1.abs_diff(target) {
            best = (it + 1, n);
        }
    }
    let unmerged = if min_size <= 1 { m } else { 0 };
    if unmerged.abs_diff(target) < best.1.abs_diff(target) {
        return (0, unmerged);
    }
    best
}

/// `round(0.1 m)` with ties to even, as Python's `round` in pyannote.
fn round_half_even_tenth(m: usize) -> usize {
    let (q, r) = (m / 10, m % 10);
    if r > 5 || (r == 5 && !q.is_multiple_of(2)) {
        q + 1
    } else {
        q
    }
}

/// Up to `max` of `0..n`, sorted: all of them if `n <= max`, otherwise a uniform random
/// subset from a fixed-seed xorshift64* (partial Fisher-Yates), so results are
/// reproducible.
fn training_subset(n: usize, max: usize) -> Vec<usize> {
    let mut idx: Vec<usize> = (0..n).collect();
    if n <= max {
        return idx;
    }
    let mut state: u64 = 0x9E37_79B9_7F4A_7C15;
    for i in 0..max {
        state ^= state >> 12;
        state ^= state << 25;
        state ^= state >> 27;
        let r = state.wrapping_mul(0x2545_F491_4F6C_DD1D);
        let j = i + (r % (n - i) as u64) as usize;
        idx.swap(i, j);
    }
    idx.truncate(max);
    idx.sort_unstable();
    idx
}

/// Index of the centroid (any scale) with the highest cosine similarity to `v`; lowest
/// index on ties.
fn nearest(v: &[f64], centroids: &[Vec<f64>]) -> usize {
    nearest_with_cosine(v, centroids).0
}

/// [`nearest`] with the winning cosine similarity (`-inf` with no centroid).
fn nearest_with_cosine(v: &[f64], centroids: &[Vec<f64>]) -> (usize, f64) {
    let nv = norm(v).max(f64::MIN_POSITIVE);
    let mut best = (f64::NEG_INFINITY, 0usize);
    for (k, c) in centroids.iter().enumerate() {
        let s = dot(v, c) / (nv * norm(c).max(f64::MIN_POSITIVE));
        if s > best.0 {
            best = (s, k);
        }
    }
    (best.1, best.0)
}

fn dot(a: &[f64], b: &[f64]) -> f64 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

fn norm(a: &[f64]) -> f64 {
    dot(a, a).sqrt()
}

/// `v / |v|`, or `None` if `v` is non-finite or all zeros.
pub(crate) fn normalized(v: &[f32]) -> Option<Vec<f32>> {
    if v.iter().any(|x| !x.is_finite()) {
        return None;
    }
    let n = v
        .iter()
        .map(|&x| f64::from(x) * f64::from(x))
        .sum::<f64>()
        .sqrt();
    if n == 0.0 {
        return None;
    }
    Some(v.iter().map(|&x| (f64::from(x) / n) as f32).collect())
}

/// Cosine similarity of two equal-length vectors; 0 if either is all zeros.
pub(crate) fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    let (mut ab, mut aa, mut bb) = (0.0f64, 0.0f64, 0.0f64);
    for (&x, &y) in a.iter().zip(b) {
        let (x, y) = (f64::from(x), f64::from(y));
        ab += x * y;
        aa += x * x;
        bb += y * y;
    }
    if aa == 0.0 || bb == 0.0 {
        return 0.0;
    }
    (ab / (aa.sqrt() * bb.sqrt())) as f32
}
