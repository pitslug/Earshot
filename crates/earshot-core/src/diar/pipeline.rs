//! The diarisation pipeline glue (pyannote.audio 3.x `SpeakerDiarization` structure).
//!
//! Reference: pyannote.audio 3.1, `pyannote/audio/pipelines/speaker_diarization.py`
//! (`get_segmentations`, `get_embeddings`, `reconstruct`, `to_diarization`) and its
//! C++ port in sherpa-onnx (`sherpa-onnx/csrc/offline-speaker-diarization-pyannote-impl.h`).
//!
//! Defaults ([`DiarizationConfig::default`], [`ClusteringConfig::default`]) are those of
//! `pyannote/speaker-diarization-3.1`; each field's doc names where its value comes from.
//!
//! 1. **Sliding windows.** Windows of [`SegmenterInfo::window`] samples every
//!    [`DiarizationConfig::window_step`]; the last window is aligned to the end of the
//!    audio, and audio shorter than one window is zero-padded.
//! 2. **Local activity.** The segmenter returns `frames x local speakers` activity per
//!    window, binarised at [`DiarizationConfig::onset`].
//! 3. **Local-speaker embeddings.** For each active local speaker the samples of its
//!    active frames are concatenated and embedded. With `exclude_overlap`, only frames
//!    where it is the sole active speaker are used when they amount to at least the
//!    embedder's `min_samples` (pyannote's `embedding_exclude_overlap`); regions shorter
//!    than `min_samples` get no embedding and that local speaker is dropped (pyannote
//!    drops NaN embeddings in `filter_embeddings`).
//! 4. **Clustering.** Every valid embedding trains the clustering ([`super::cluster`]),
//!    unless the extra `min_active_ratio` filter is set; if it leaves none, all do.
//! 5. **Stitching.** Every embedded local speaker is assigned to the cluster centroid
//!    with the highest cosine similarity (pyannote's `assign_embeddings`). With
//!    `constrained_assignment`, the local speakers of one window go to distinct clusters
//!    (Hungarian assignment maximising total similarity, as pyannote's
//!    `constrained_argmax`). With [`ClusteringConfig::regroup`] set (T12.66, off by
//!    default, `docs/far-speakers/short-turns.md`) this stitching is computed first,
//!    unchanged; then each local speaker whose embedding belongs to a new speaker of the
//!    clustering ([`super::Clustering::new_speaker_of`]) is given to that new speaker instead,
//!    except that of two locals of one window given to the same new speaker only the more
//!    cosine-similar one (the lower local on ties) goes there and the other keeps its
//!    speaker. An embedded local outside the clustering's input (the `min_active_ratio`
//!    filter) goes to its most similar new speaker only when it is more similar to it than
//!    to the centroid it was stitched to. Each training embedding's speech estimate is its
//!    clean frames times the frame step, divided by the windows covering one moment (window
//!    over window step). Step 6 then keeps each frame's baseline speakers by construction
//!    (see there), and step 7 runs over the clusters plus the new speakers
//!    ([`Diarization::new_speakers`]).
//! 6. **Reconstruction and overlap.** Per global frame, each cluster's activity is the
//!    fraction of covering windows in which one of its local speakers is active, and the
//!    instantaneous speaker count is the rounded (ties to even, as `np.rint`) mean number
//!    of active local speakers over the covering windows. The `count` most active clusters
//!    are marked active, so overlapped speech yields two simultaneous turns. With new speakers
//!    (T12.66) the marking is first made exactly as without them, from the stitching before
//!    any local moved; then in each frame a marked speaker X is replaced by the new speaker N
//!    that took most of X's activity there (lowest N on ties) when the activity X lost to N
//!    exceeds the activity X kept, and one N replaces at most one X per frame (the X that lost
//!    most to it, the lower X on ties); see [`frame_with_new`]. So a baseline speaker never
//!    gains a frame and loses one only to a new speaker.
//! 7. **Turns.** Runs of active frames per cluster become [`SpeakerTurn`]s; same-speaker
//!    gaps up to `merge_gap` are closed. Speakers are numbered by first turn. The runs of
//!    clusters the clustering left unassigned (T12.65) become [`Diarization::unassigned`]
//!    spans instead: no speaker, no centroid.
//!
//! # Deviations from pyannote 3.1 and their expected effect on DER
//!
//! - **Embedding input.** pyannote passes the whole 10 s window plus a frame mask to its
//!   WeSpeaker model (masked statistics pooling). Here the selected samples are
//!   concatenated and passed alone, as sherpa-onnx does, because the CAM++/WeSpeaker ONNX
//!   exports take no mask. Splice points add a little noise to embeddings; expected
//!   effect small, and absorbed by calibrating the threshold for the exported model.
//! - **Leftover local speakers under `constrained_assignment`** (off by default). pyannote
//!   drops a local speaker that the Hungarian assignment leaves unassigned (more local
//!   speakers in the window than clusters); here it falls back to its most similar
//!   centroid. Keeps that speech (fewer misses) at the risk of attributing it to a speaker
//!   already active in the window (more confusion); no effect when the option is off.
//! - **Inactive clusters are never selected.** pyannote marks the `count` top clusters of
//!   a frame even when some have zero activity there (an arbitrary speaker); here only
//!   clusters with activity > 0 are marked. Removes false alarms that pyannote would count
//!   as confusion; slightly lower DER in frames where the count exceeds the active
//!   clusters.
//! - **Last window.** pyannote zero-pads a final partial chunk starting at the next step;
//!   here the last window is aligned to the end of the audio, so it has full context.
//!   Affects only the last few seconds; negligible.
//! - **Frame edges.** The first and last global frames are extended to the start and end
//!   of the audio (about 22 ms and 38 ms with segmentation-3.0). Negligible.
//! - **`min_active_ratio`** is an extra filter that pyannote 3.1 does not have; its default
//!   0 disables it.
//!
//! # Two halves (T6.13d)
//!
//! Steps 1-3 run per window and steps 4-7 over all of them, as in pyannote, where
//! `get_segmentations` and `get_embeddings` produce per-chunk arrays that `clustering`
//! and `reconstruct` consume. [`Diarizer::diarize`] is [`Diarizer::diarize_from`] with no
//! windows done; a [`DiarStream`] computes steps 1-3 for the windows of the global grid
//! during the meeting ([`Diarizer::advance`], [`super::stream`] has the rules).

use std::time::Duration;

use super::cluster::{ClusteringConfig, cluster_embeddings_with, cosine_similarity, normalized};
use super::stream::{DiarStream, Local, WindowParams, WindowResults};
use super::types::{
    DiarError, Embedding, EmbeddingModelId, LocalActivity, SegmenterInfo, SpeakerEmbedder,
    SpeakerSegmenter,
};
use crate::asr::{CancelToken, MeetingTime};
use crate::audio::{MonoBuffer, PIPELINE_RATE, Sample};
use crate::metrics::der::Turn;
use crate::metrics::hungarian::max_weight_assignment;

/// Pipeline parameters.
#[derive(Debug, Clone, PartialEq)]
pub struct DiarizationConfig {
    /// Distance between window starts. Default 1 s: pyannote.audio 3.1
    /// `SpeakerDiarization(segmentation_step=0.1)`, a tenth of the 10 s window.
    pub window_step: Duration,
    /// Local activity at or above this is "active". Powerset output is already 0 or 1,
    /// so any value in `(0, 1]` gives pyannote 3.1's hard decisions.
    pub onset: f32,
    /// Extra filter (not in pyannote 3.1): a local speaker trains the clustering only if
    /// it is the sole active speaker in at least this fraction of the window's frames.
    /// Default 0, which keeps pyannote 3.1 `filter_embeddings` behaviour (every active
    /// local speaker with a valid embedding trains).
    pub min_active_ratio: f32,
    /// Embed from non-overlapped frames when there are enough of them. Default true,
    /// pyannote.audio 3.1 `SpeakerDiarization(embedding_exclude_overlap=True)` as set by
    /// pyannote/speaker-diarization-3.1.
    pub exclude_overlap: bool,
    /// Local speakers of one window go to distinct clusters. Default false, pyannote.audio
    /// 3.1 `BaseClustering(constrained_assignment=False)` (not overridden by
    /// speaker-diarization-3.1). See the module docs for the leftover-local fallback.
    pub constrained_assignment: bool,
    /// Same-speaker gaps up to this long are closed when building turns. Default 0,
    /// pyannote/speaker-diarization-3.1 `segmentation.min_duration_off: 0.0`.
    pub merge_gap: Duration,
    /// Windows (and speech regions) per model call.
    pub batch_size: usize,
    /// Clustering parameters, including an optional known speaker count.
    pub clustering: ClusteringConfig,
}

impl Default for DiarizationConfig {
    fn default() -> Self {
        DiarizationConfig {
            window_step: Duration::from_secs(1),
            onset: 0.5,
            min_active_ratio: 0.0,
            exclude_overlap: true,
            constrained_assignment: false,
            merge_gap: Duration::ZERO,
            batch_size: 32,
            clustering: ClusteringConfig::default(),
        }
    }
}

impl DiarizationConfig {
    /// Checks parameter ranges.
    ///
    /// # Errors
    /// [`DiarError::Config`] for a zero step or batch, an onset outside `(0, 1]`, a ratio
    /// outside `[0, 1]`, or an invalid clustering configuration.
    pub fn validate(&self) -> Result<(), DiarError> {
        if self.window_step.is_zero() {
            return Err(DiarError::Config("window_step must be > 0".to_owned()));
        }
        if self.batch_size == 0 {
            return Err(DiarError::Config("batch_size must be > 0".to_owned()));
        }
        if !(self.onset > 0.0 && self.onset <= 1.0) {
            return Err(DiarError::Config("onset must be in (0, 1]".to_owned()));
        }
        if !(0.0..=1.0).contains(&self.min_active_ratio) {
            return Err(DiarError::Config(
                "min_active_ratio must be in [0, 1]".to_owned(),
            ));
        }
        self.clustering.validate()
    }
}

/// One stretch of speech by one speaker. Turns of different speakers may overlap.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpeakerTurn {
    /// Index into [`Diarization::speakers`].
    pub speaker: usize,
    /// Start (meeting time).
    pub start: MeetingTime,
    /// End (meeting time, exclusive).
    pub end: MeetingTime,
}

/// A stretch of speech that diarisation left without a speaker (T12.65).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UnassignedSpan {
    /// Start (meeting time).
    pub start: MeetingTime,
    /// End (meeting time, exclusive).
    pub end: MeetingTime,
}

/// One speaker found by diarisation.
#[derive(Debug, Clone, PartialEq)]
pub struct DiarSpeaker {
    /// Unit-length centroid of the speaker's embeddings (for voiceprint matching).
    pub centroid: Embedding,
    /// Total speech of this speaker.
    pub speech: Duration,
}

/// Diarisation of one track.
#[derive(Debug, Clone, PartialEq)]
pub struct Diarization {
    /// Speakers, numbered in order of their first turn.
    pub speakers: Vec<DiarSpeaker>,
    /// Turns sorted by start, then speaker.
    pub turns: Vec<SpeakerTurn>,
    /// Speech left without a speaker (T12.65: small clusters like no speaker, see
    /// [`super::cluster`] step 5), sorted and non-overlapping. Empty with the default
    /// floor. Not in [`Diarization::der_turns`], so it scores as missed speech.
    pub unassigned: Vec<UnassignedSpan>,
    /// Indices into [`Diarization::speakers`] of the speakers made by the small-cluster
    /// regrouping (T12.66, [`ClusteringConfig::regroup`]), ascending. Empty unless it is set.
    pub new_speakers: Vec<usize>,
    /// Per speaker, the cluster of the clustering it came from (a new speaker `s` is
    /// `clusters + s`), so speakers of two runs over the same clustering can be matched.
    /// Empty for a diarisation the pipeline did not make.
    pub speaker_clusters: Vec<usize>,
    /// The embedding model that produced the centroids.
    pub embedding_model: EmbeddingModelId,
}

impl Diarization {
    /// The turns as [`crate::metrics::der::Turn`]s (seconds, labels `"S<index>"`), for
    /// scoring with [`crate::metrics::der::der`].
    #[must_use]
    pub fn der_turns(&self) -> Vec<Turn> {
        self.turns
            .iter()
            .map(|t| {
                Turn::new(
                    t.start.as_millis() as f64 / 1000.0,
                    t.end.as_millis() as f64 / 1000.0,
                    format!("S{}", t.speaker),
                )
            })
            .collect()
    }
}

/// The diarisation pipeline over a [`SpeakerSegmenter`] and a [`SpeakerEmbedder`].
#[derive(Debug)]
pub struct Diarizer<S, E> {
    segmenter: S,
    embedder: E,
    config: DiarizationConfig,
}

impl<S: SpeakerSegmenter, E: SpeakerEmbedder> Diarizer<S, E> {
    /// A pipeline over `segmenter` and `embedder`.
    ///
    /// # Errors
    /// [`DiarError::Config`] if `config` or the segmenter description is invalid.
    pub fn new(segmenter: S, embedder: E, config: DiarizationConfig) -> Result<Self, DiarError> {
        config.validate()?;
        segmenter.info().validate()?;
        Ok(Diarizer {
            segmenter,
            embedder,
            config,
        })
    }

    /// The configuration.
    #[must_use]
    pub fn config(&self) -> &DiarizationConfig {
        &self.config
    }

    /// Gives back the models.
    pub fn into_parts(self) -> (S, E) {
        (self.segmenter, self.embedder)
    }

    /// The embedder, for embedding other stretches with the same model (for example the
    /// echo speaker check, [`super::speaker_echo_spans`]).
    pub fn embedder_mut(&mut self) -> &mut E {
        &mut self.embedder
    }

    /// The settings that decide the per-window results ([`super::stream`]).
    #[must_use]
    pub fn window_params(&self) -> WindowParams {
        WindowParams {
            segmenter: self.segmenter.info().clone(),
            embedder: self.embedder.info().clone(),
            window_step: PIPELINE_RATE.frames_for(self.config.window_step).max(1),
            onset: self.config.onset,
            exclude_overlap: self.config.exclude_overlap,
            batch_size: self.config.batch_size,
        }
    }

    /// A stream for computing a track's windows while the meeting runs (T6.13d): feed it
    /// with [`DiarStream::append`], compute with [`Diarizer::advance`], finish with
    /// [`Diarizer::diarize_from`].
    #[must_use]
    pub fn stream(&self) -> DiarStream {
        DiarStream::new(self.window_params())
    }

    /// Computes steps 1-3 for up to `max_batches` batches of windows whose audio `stream`
    /// has received ([`DiarStream::ready_batches`]), in the batches and order of
    /// [`Diarizer::diarize`]. Returns the number of windows computed. A batch that fails
    /// leaves the results as they were before it.
    ///
    /// # Errors
    /// [`DiarError::State`] if the stream was made with other [`WindowParams`],
    /// [`DiarError::Cancelled`], and the errors of [`Diarizer::diarize`].
    pub fn advance(
        &mut self,
        stream: &mut DiarStream,
        max_batches: usize,
        cancel: &CancelToken,
    ) -> Result<usize, DiarError> {
        if stream.results.params != self.window_params() {
            return Err(DiarError::State(
                "the stream was made with other diarisation settings".to_owned(),
            ));
        }
        let step = stream.results.params.window_step;
        let batch = self.config.batch_size;
        let mut done = 0;
        for _ in 0..max_batches {
            if stream.ready_batches() == 0 {
                break;
            }
            if cancel.is_cancelled() {
                return Err(DiarError::Cancelled);
            }
            let first = stream.results.windows();
            let starts: Vec<usize> = (first..first + batch).map(|w| w * step).collect();
            let received = stream.received();
            let DiarStream {
                results,
                buf,
                buf_start,
            } = &mut *stream;
            let source = Source {
                samples: buf,
                offset: *buf_start,
                total: received,
                window: results.params.segmenter.window,
            };
            let mark = results.mark();
            if let Err(e) = self.segment_batch(results, &starts, &|w| w * step, &source, cancel) {
                results.rollback(mark);
                return Err(e);
            }
            done += batch;
            stream.trim();
        }
        Ok(done)
    }

    /// Diarises `audio` (one track at 16 kHz) whose first sample is at meeting time
    /// `origin`.
    ///
    /// # Errors
    /// [`DiarError::WrongSampleRate`] for audio not at 16 kHz, [`DiarError::Cancelled`],
    /// [`DiarError::Shape`] if a model returns output of the wrong shape or count, and
    /// model errors as returned.
    pub fn diarize(
        &mut self,
        audio: &MonoBuffer,
        origin: MeetingTime,
        cancel: &CancelToken,
    ) -> Result<Diarization, DiarError> {
        let results = WindowResults::new(self.window_params());
        self.diarize_from(results, audio, origin, cancel)
    }

    /// The final half (T6.13d): diarises `audio` as [`Diarizer::diarize`] does, taking the
    /// first windows from `results` (computed from the same audio by a [`DiarStream`],
    /// possibly by another diarizer with the same [`WindowParams`]). Fed the same audio,
    /// the output equals [`Diarizer::diarize`].
    ///
    /// # Errors
    /// [`DiarError::State`] if `results` were made with other [`WindowParams`], cover more
    /// audio than `audio`, or are inconsistent; the errors of [`Diarizer::diarize`].
    pub fn diarize_from(
        &mut self,
        mut results: WindowResults,
        audio: &MonoBuffer,
        origin: MeetingTime,
        cancel: &CancelToken,
    ) -> Result<Diarization, DiarError> {
        if audio.rate != PIPELINE_RATE {
            return Err(DiarError::WrongSampleRate {
                expected: PIPELINE_RATE.hz(),
                actual: audio.rate.hz(),
            });
        }
        if cancel.is_cancelled() {
            return Err(DiarError::Cancelled);
        }
        if results.params != self.window_params() {
            return Err(DiarError::State(
                "the window results were made with other diarisation settings".to_owned(),
            ));
        }
        results.check()?;
        let info = self.segmenter.info().clone();
        let model = self.embedder.info().model.clone();
        let samples = &audio.samples;
        let total = samples.len();
        let step = results.params.window_step;
        let starts = window_starts(total, info.window, step);
        let nloc = info.max_local_speakers;
        let done = results.windows();
        if !done.is_multiple_of(self.config.batch_size) {
            return Err(DiarError::State(
                "the window results end inside a batch".to_owned(),
            ));
        }
        if results.covered() > total {
            return Err(DiarError::State(format!(
                "the window results cover {} samples, the track has {total}",
                results.covered()
            )));
        }

        // 1-3: local activity per window and local-speaker embeddings, for the windows
        // not done yet. Every window done is a full window of the grid, so `starts`
        // begins with them.
        let source = Source {
            samples,
            offset: 0,
            total,
            window: info.window,
        };
        let start_of = |w: usize| starts[w];
        for batch in starts[done..].chunks(self.config.batch_size) {
            self.segment_batch(&mut results, batch, &start_of, &source, cancel)?;
        }
        self.flush(&mut results, &start_of, &source, cancel)?;
        let WindowResults { bins, locals, .. } = results;

        // 4: clustering.
        let embedded: Vec<usize> = (0..locals.len())
            .filter(|&i| locals[i].embedding.is_some())
            .collect();
        if embedded.is_empty() {
            return Ok(Diarization {
                speakers: Vec::new(),
                turns: Vec::new(),
                unassigned: Vec::new(),
                new_speakers: Vec::new(),
                speaker_clusters: Vec::new(),
                embedding_model: model,
            });
        }
        let min_clean = (self.config.min_active_ratio * info.frames as f32).ceil() as usize;
        let mut train: Vec<usize> = embedded
            .iter()
            .copied()
            .filter(|&i| locals[i].clean_frames >= min_clean)
            .collect();
        if train.is_empty() {
            train.clone_from(&embedded);
        }
        let vectors: Vec<Vec<f32>> = train
            .iter()
            .filter_map(|&i| locals[i].embedding.clone())
            .collect();
        // Speech estimate per training embedding (T12.66): clean frames over the windows
        // that cover one moment.
        let cover_per_moment = info.window as f64 / step as f64;
        let speech: Vec<f32> = train
            .iter()
            .map(|&i| {
                (locals[i].clean_frames as f64 * info.frame_step as f64
                    / f64::from(PIPELINE_RATE.hz())
                    / cover_per_moment) as f32
            })
            .collect();
        let clustering = cluster_embeddings_with(&vectors, Some(&speech), &self.config.clustering)?;
        let (mut centroids, mut left_out) = (clustering.centroids, clustering.unassigned);
        let new_centroids = clustering.new_speakers;
        // 5: stitching, one window at a time.
        let mut by_window: Vec<Vec<usize>> = vec![Vec::new(); bins.len()];
        for &i in &embedded {
            by_window[locals[i].window].push(i);
        }
        let mut win_cluster: Vec<Vec<Option<usize>>> = vec![vec![None; nloc]; bins.len()];
        for idx in &by_window {
            let sims: Vec<Vec<f32>> = idx
                .iter()
                .map(|&i| {
                    let e = locals[i].embedding.as_deref().unwrap_or(&[]);
                    centroids.iter().map(|c| cosine_similarity(e, c)).collect()
                })
                .collect();
            let assigned = assign_locals(&sims, self.config.constrained_assignment);
            for (&i, c) in idx.iter().zip(assigned) {
                win_cluster[locals[i].window][locals[i].local] = Some(c);
            }
        }
        // 5b (T12.66): locals of new speakers leave their baseline cluster, in a copy of the
        // stitching; step 6 marks frames from the baseline stitching first.
        let base_k = centroids.len();
        let mut moved: Option<Vec<Vec<Option<usize>>>> = None;
        if !new_centroids.is_empty() {
            let mut mv = win_cluster.clone();
            let mut new_of: Vec<Option<usize>> = vec![None; locals.len()];
            for (&i, s) in train.iter().zip(&clustering.new_speaker_of) {
                new_of[i] = *s;
            }
            for &i in &embedded {
                if train.binary_search(&i).is_ok() {
                    continue;
                }
                let e = locals[i].embedding.as_deref().unwrap_or(&[]);
                let Some(c) = win_cluster[locals[i].window][locals[i].local] else {
                    continue;
                };
                let own = cosine_similarity(e, &centroids[c]);
                let mut best: Option<(f32, usize)> = None;
                for (s, nc) in new_centroids.iter().enumerate() {
                    let v = cosine_similarity(e, nc);
                    if best.is_none_or(|(b, _)| v > b) {
                        best = Some((v, s));
                    }
                }
                if let Some((v, s)) = best
                    && v > own
                {
                    new_of[i] = Some(s);
                }
            }
            for idx in &by_window {
                // Per new speaker, the most similar local of this window (lowest on ties).
                let mut keep: Vec<Option<(f32, usize)>> = vec![None; new_centroids.len()];
                for &i in idx {
                    if let Some(s) = new_of[i] {
                        let e = locals[i].embedding.as_deref().unwrap_or(&[]);
                        let v = cosine_similarity(e, &new_centroids[s]);
                        if keep[s].is_none_or(|(b, _)| v > b) {
                            keep[s] = Some((v, i));
                        }
                    }
                }
                for (s, kept) in keep.iter().enumerate() {
                    if let Some((_, i)) = kept {
                        mv[locals[*i].window][locals[*i].local] = Some(base_k + s);
                    }
                }
            }
            moved = Some(mv);
            centroids.extend(new_centroids);
            left_out.resize(centroids.len(), false);
        }

        // 6: reconstruction on the global frame grid with the per-frame speaker count (from
        // the baseline stitching).
        let k = base_k;
        let fstep = info.frame_step;
        let valid_frames = |start: usize| {
            let valid = (total - start).min(info.window);
            (0..info.frames)
                .take_while(|&f| info.frame_start(f) < valid)
                .count()
        };
        // Global frame of a window's frame 0 (pyannote's `closest_frame`).
        let base = |start: usize| (start + fstep / 2) / fstep;
        let g_total = starts
            .iter()
            .map(|&s| base(s) + valid_frames(s))
            .max()
            .unwrap_or(0);
        let mut cover = vec![0u32; g_total];
        let mut count_sum = vec![0u32; g_total];
        let mut act = vec![0u32; g_total * k];
        let mut seen: Vec<usize> = Vec::with_capacity(nloc);
        for (w, &s) in starts.iter().enumerate() {
            let b = base(s);
            for f in 0..valid_frames(s) {
                let g = b + f;
                cover[g] += 1;
                seen.clear();
                for j in 0..nloc {
                    if !bins[w][f * nloc + j] {
                        continue;
                    }
                    count_sum[g] += 1;
                    if let Some(c) = win_cluster[w][j]
                        && !seen.contains(&c)
                    {
                        seen.push(c);
                        act[g * k + c] += 1;
                    }
                }
            }
        }
        let mut on = vec![false; g_total * k];
        let mut order: Vec<usize> = Vec::with_capacity(k);
        for g in 0..g_total {
            if cover[g] == 0 {
                continue;
            }
            let count = (count_sum[g] as f32 / cover[g] as f32).round_ties_even() as usize;
            let row = &act[g * k..(g + 1) * k];
            order.clear();
            order.extend((0..k).filter(|&c| row[c] > 0));
            order.sort_by(|&a, &b| row[b].cmp(&row[a]).then(a.cmp(&b)));
            for &c in order.iter().take(count) {
                on[g * k + c] = true;
            }
        }
        // 6b (T12.66): new speakers take frames only from the speakers marked above.
        let (k, on) = match &moved {
            None => (k, on),
            Some(mv) => {
                let nn = centroids.len() - base_k;
                // Per frame and baseline cluster: windows where it kept an active local, and
                // windows where it lost one to each new speaker.
                let mut kept = vec![0u32; g_total * base_k];
                let mut lost = vec![0u32; g_total * base_k * nn];
                let mut pairs: Vec<(usize, usize)> = Vec::with_capacity(nloc);
                for (w, &s) in starts.iter().enumerate() {
                    let b = base(s);
                    for f in 0..valid_frames(s) {
                        let g = b + f;
                        pairs.clear();
                        for j in 0..nloc {
                            if !bins[w][f * nloc + j] {
                                continue;
                            }
                            if let (Some(x), Some(c)) = (win_cluster[w][j], mv[w][j])
                                && !pairs.contains(&(x, c))
                            {
                                pairs.push((x, c));
                            }
                        }
                        let mut kept_here: Vec<usize> = Vec::with_capacity(pairs.len());
                        for &(x, c) in &pairs {
                            if c < base_k {
                                if !kept_here.contains(&c) {
                                    kept_here.push(c);
                                    kept[g * base_k + c] += 1;
                                }
                            } else {
                                lost[(g * base_k + x) * nn + (c - base_k)] += 1;
                            }
                        }
                    }
                }
                let k2 = centroids.len();
                let mut on2 = vec![false; g_total * k2];
                let mut marked: Vec<usize> = Vec::with_capacity(base_k);
                for g in 0..g_total {
                    marked.clear();
                    marked.extend((0..base_k).filter(|&c| on[g * base_k + c]));
                    if marked.is_empty() {
                        continue;
                    }
                    let rows = (
                        &kept[g * base_k..(g + 1) * base_k],
                        &lost[g * base_k * nn..(g + 1) * base_k * nn],
                    );
                    for c in frame_with_new(&marked, rows.0, rows.1, nn) {
                        on2[g * k2 + c] = true;
                    }
                }
                (k2, on2)
            }
        };

        // 7: turns. Global frame g stands for samples [frame_lo(g), frame_hi(g)); the
        // first and last frames extend to the ends of the audio.
        let offset = info.frame_start(0);
        let frame_lo = |g: usize| {
            if g == 0 {
                0
            } else {
                (g * fstep + offset).min(total)
            }
        };
        let frame_hi = |g: usize| {
            if g + 1 == g_total {
                total
            } else {
                ((g + 1) * fstep + offset).min(total)
            }
        };
        let gap = PIPELINE_RATE.frames_for(self.config.merge_gap);
        let mut runs: Vec<Vec<(usize, usize)>> = vec![Vec::new(); k];
        for (c, cluster_runs) in runs.iter_mut().enumerate() {
            let mut g = 0;
            while g < g_total {
                if !on[g * k + c] {
                    g += 1;
                    continue;
                }
                let g0 = g;
                while g < g_total && on[g * k + c] {
                    g += 1;
                }
                let (lo, hi) = (frame_lo(g0), frame_hi(g - 1));
                if hi <= lo {
                    continue;
                }
                match cluster_runs.last_mut() {
                    Some(last) if lo <= last.1 + gap => last.1 = hi,
                    _ => cluster_runs.push((lo, hi)),
                }
            }
        }
        // Unassigned clusters (T12.65) become speech with no speaker, never a speaker.
        let mut spans: Vec<(usize, usize)> = (0..k)
            .filter(|&c| left_out[c])
            .flat_map(|c| runs[c].iter().copied())
            .collect();
        spans.sort_unstable();
        let mut merged: Vec<(usize, usize)> = Vec::new();
        for (lo, hi) in spans {
            match merged.last_mut() {
                Some(last) if lo <= last.1 => last.1 = last.1.max(hi),
                _ => merged.push((lo, hi)),
            }
        }
        let mut used: Vec<usize> = (0..k)
            .filter(|&c| !left_out[c] && !runs[c].is_empty())
            .collect();
        used.sort_by_key(|&c| (runs[c][0].0, c));
        let to_time = |x: usize| origin + MeetingTime::from_pipeline_frames(x as u64);
        let mut speakers = Vec::with_capacity(used.len());
        let mut turns = Vec::new();
        for (label, &c) in used.iter().enumerate() {
            let mut speech = 0usize;
            for &(lo, hi) in &runs[c] {
                speech += hi - lo;
                turns.push(SpeakerTurn {
                    speaker: label,
                    start: to_time(lo),
                    end: to_time(hi),
                });
            }
            speakers.push(DiarSpeaker {
                centroid: Embedding::new(model.clone(), centroids[c].clone()),
                speech: PIPELINE_RATE.duration_of(speech),
            });
        }
        turns.sort_by_key(|t| (t.start, t.speaker));
        let new_speakers: Vec<usize> = (0..used.len()).filter(|&l| used[l] >= base_k).collect();
        let speaker_clusters = used.clone();
        let unassigned = merged
            .into_iter()
            .map(|(lo, hi)| UnassignedSpan {
                start: to_time(lo),
                end: to_time(hi),
            })
            .collect();
        Ok(Diarization {
            speakers,
            turns,
            unassigned,
            new_speakers,
            speaker_clusters,
            embedding_model: model,
        })
    }

    /// Binarised `frames x local speakers` activity; frames that start beyond the `valid`
    /// (non-padding) samples are inactive.
    fn binarize(
        &self,
        info: &SegmenterInfo,
        act: &LocalActivity,
        valid: usize,
    ) -> Result<Vec<bool>, DiarError> {
        if act.frames() != info.frames || act.speakers() != info.max_local_speakers {
            return Err(DiarError::Shape(format!(
                "segmenter returned {} x {} activity, expected {} x {}",
                act.frames(),
                act.speakers(),
                info.frames,
                info.max_local_speakers
            )));
        }
        let nloc = info.max_local_speakers;
        let mut bin = vec![false; info.frames * nloc];
        for f in 0..info.frames {
            if info.frame_start(f) >= valid {
                break;
            }
            for j in 0..nloc {
                bin[f * nloc + j] = act.get(f, j) >= self.config.onset;
            }
        }
        Ok(bin)
    }

    /// Steps 1-3 for the windows starting at `batch` (one segmenter call): binarised
    /// activity, local speakers, and their embeddings whenever `batch_size` regions are
    /// queued (as [`Diarizer::diarize`] has always batched them).
    fn segment_batch(
        &mut self,
        results: &mut WindowResults,
        batch: &[usize],
        start_of: &dyn Fn(usize) -> usize,
        source: &Source<'_>,
        cancel: &CancelToken,
    ) -> Result<(), DiarError> {
        if cancel.is_cancelled() {
            return Err(DiarError::Cancelled);
        }
        let info = self.segmenter.info().clone();
        let padded: Vec<Vec<Sample>> = batch
            .iter()
            .map(|&s| {
                let mut v = source.window_audio(s).to_vec();
                v.resize(info.window, 0.0);
                v
            })
            .collect();
        let refs: Vec<&[Sample]> = padded.iter().map(Vec::as_slice).collect();
        let acts = self.segmenter.segment(&refs, cancel)?;
        if acts.len() != batch.len() {
            return Err(DiarError::Shape(format!(
                "segmenter returned {} outputs for {} windows",
                acts.len(),
                batch.len()
            )));
        }
        for (&start, act) in batch.iter().zip(&acts) {
            let valid = source.valid(start);
            let bin = self.binarize(&info, act, valid)?;
            self.collect_locals(&info, &bin, valid, results);
            results.bins.push(bin);
            if results.pending.len() >= self.config.batch_size {
                self.flush(results, start_of, source, cancel)?;
            }
        }
        Ok(())
    }

    /// The frames whose samples are embedded for local speaker `j` (all its active frames,
    /// or with `exclude_overlap` only those where it is alone when they amount to at least
    /// `min_samples`), and its number of clean frames.
    fn speech_frames(
        &self,
        info: &SegmenterInfo,
        bin: &[bool],
        j: usize,
        valid: usize,
    ) -> (Vec<usize>, usize) {
        let nloc = info.max_local_speakers;
        let min_samples = self.embedder.info().min_samples.max(1);
        let active: Vec<usize> = (0..info.frames).filter(|&f| bin[f * nloc + j]).collect();
        let clean: Vec<usize> = active
            .iter()
            .copied()
            .filter(|&f| (0..nloc).all(|o| o == j || !bin[f * nloc + o]))
            .collect();
        let n_clean = clean.len();
        let speech =
            if self.config.exclude_overlap && frames_len(info, &clean, valid) >= min_samples {
                clean
            } else {
                active
            };
        (speech, n_clean)
    }

    /// Records the active local speakers of the next window (activity `bin`, `valid`
    /// real samples) and queues those with enough speech for embedding.
    fn collect_locals(
        &self,
        info: &SegmenterInfo,
        bin: &[bool],
        valid: usize,
        results: &mut WindowResults,
    ) {
        let nloc = info.max_local_speakers;
        let min_samples = self.embedder.info().min_samples.max(1);
        let window = results.bins.len();
        for j in 0..nloc {
            if !(0..info.frames).any(|f| bin[f * nloc + j]) {
                continue;
            }
            let (speech, clean_frames) = self.speech_frames(info, bin, j, valid);
            let li = results.locals.len();
            results.locals.push(Local {
                window,
                local: j,
                clean_frames,
                embedding: None,
            });
            if frames_len(info, &speech, valid) >= min_samples {
                results.pending.push(li);
            }
        }
    }

    /// Embeds the queued speech regions and stores valid, unit-length embeddings.
    fn flush(
        &mut self,
        results: &mut WindowResults,
        start_of: &dyn Fn(usize) -> usize,
        source: &Source<'_>,
        cancel: &CancelToken,
    ) -> Result<(), DiarError> {
        if results.pending.is_empty() {
            return Ok(());
        }
        if cancel.is_cancelled() {
            return Err(DiarError::Cancelled);
        }
        let info = self.segmenter.info().clone();
        let speech: Vec<Vec<Sample>> = results
            .pending
            .iter()
            .map(|&li| {
                let l = &results.locals[li];
                let start = start_of(l.window);
                let valid = source.valid(start);
                let (frames, _) =
                    self.speech_frames(&info, &results.bins[l.window], l.local, valid);
                gather(&info, &frames, source.window_audio(start), valid)
            })
            .collect();
        let refs: Vec<&[Sample]> = speech.iter().map(Vec::as_slice).collect();
        let embs = self.embedder.embed(&refs, cancel)?;
        if embs.len() != refs.len() {
            return Err(DiarError::Shape(format!(
                "embedder returned {} embeddings for {} regions",
                embs.len(),
                refs.len()
            )));
        }
        let info = self.embedder.info();
        for (&li, e) in results.pending.iter().zip(embs) {
            if e.model != info.model {
                return Err(DiarError::Shape(format!(
                    "embedder declared model {} but returned {}",
                    info.model, e.model
                )));
            }
            if e.vector.len() != info.dimension {
                return Err(DiarError::Shape(format!(
                    "embedding has dimension {}, expected {}",
                    e.vector.len(),
                    info.dimension
                )));
            }
            results.locals[li].embedding = normalized(&e.vector);
        }
        results.pending.clear();
        Ok(())
    }
}

/// Track audio for steps 1-3: `samples` start at track sample `offset`; the track has
/// `total` samples so far.
struct Source<'a> {
    samples: &'a [Sample],
    offset: usize,
    total: usize,
    window: usize,
}

impl Source<'_> {
    /// Real (non-padding) samples of the window starting at `start`.
    fn valid(&self, start: usize) -> usize {
        (self.total - start).min(self.window)
    }

    /// The real samples of the window starting at `start`.
    fn window_audio(&self, start: usize) -> &[Sample] {
        let lo = start - self.offset;
        &self.samples[lo..lo + self.valid(start)]
    }
}

/// Samples that `frames` of a window with `valid` real samples stand for.
fn frames_len(info: &SegmenterInfo, frames: &[usize], valid: usize) -> usize {
    frames
        .iter()
        .map(|&f| {
            let lo = info.frame_start(f).min(valid);
            (lo + info.frame_step).min(valid) - lo
        })
        .sum()
}

/// The samples of `frames`, concatenated, from a window's real samples `audio`.
fn gather(info: &SegmenterInfo, frames: &[usize], audio: &[Sample], valid: usize) -> Vec<Sample> {
    let mut out = Vec::new();
    for &f in frames {
        let lo = info.frame_start(f).min(valid);
        let hi = (lo + info.frame_step).min(valid);
        out.extend_from_slice(&audio[lo..hi]);
    }
    out
}

/// Window starts (samples) for `total` samples: every `step` while the window fits, plus
/// one aligned to the end if audio remains; a single window at 0 for short audio.
pub(crate) fn window_starts(total: usize, window: usize, step: usize) -> Vec<usize> {
    if total == 0 {
        return Vec::new();
    }
    if total <= window {
        return vec![0];
    }
    let mut v = Vec::new();
    let mut s = 0;
    while s + window <= total {
        v.push(s);
        s += step;
    }
    if v.last().is_some_and(|&l| l + window < total) {
        v.push(total - window);
    }
    v
}

/// One frame's marked speakers with new speakers (T12.66, module docs step 6): `marked` are
/// the baseline clusters marked in the frame, `kept[x]` the windows in which cluster `x` kept an
/// active local, `lost[x * new + n]` those in which it lost one to new speaker `n`. A marked
/// `x` is replaced by the new speaker `n` it lost most to (lowest `n` on ties) when that loss
/// exceeds what it kept; a new speaker replaces at most one `x` (the one that lost most to it,
/// the lowest `x` on ties) and the others stay. Returns the frame's speakers: baseline clusters
/// as they are, new speaker `n` as `kept.len() + n`. Nothing unmarked is ever returned.
pub(crate) fn frame_with_new(
    marked: &[usize],
    kept: &[u32],
    lost: &[u32],
    new: usize,
) -> Vec<usize> {
    let base = kept.len();
    // Per marked speaker, the new speaker that would replace it and the loss.
    let mut wants: Vec<Option<(usize, u32)>> = Vec::with_capacity(marked.len());
    for &x in marked {
        let row = &lost[x * new..(x + 1) * new];
        let mut best: Option<(usize, u32)> = None;
        for (n, &v) in row.iter().enumerate() {
            if best.is_none_or(|(_, b)| v > b) {
                best = Some((n, v));
            }
        }
        wants.push(best.filter(|&(_, v)| v > kept[x]));
    }
    let mut out = Vec::with_capacity(marked.len());
    for (i, &x) in marked.iter().enumerate() {
        let replaced = wants[i].is_some_and(|(n, v)| {
            // `x` wins `n` unless another marked speaker lost more to it, or as much with a
            // lower index.
            marked.iter().enumerate().all(|(j, &y)| {
                j == i || !wants[j].is_some_and(|(m, w)| m == n && (w > v || (w == v && y < x)))
            })
        });
        match wants[i] {
            Some((n, _)) if replaced => out.push(base + n),
            _ => out.push(x),
        }
    }
    out
}

/// Cluster of each local speaker of one window from its similarity to every centroid
/// (`sims[local][cluster]`). Constrained: distinct clusters maximising total similarity,
/// locals left over (more locals than clusters) take their most similar centroid.
/// Unconstrained: each local takes its most similar centroid. Lowest index on ties.
pub(crate) fn assign_locals(sims: &[Vec<f32>], constrained: bool) -> Vec<usize> {
    let argmax = |row: &[f32]| {
        let mut best = 0;
        for (c, &v) in row.iter().enumerate() {
            if v > row[best] {
                best = c;
            }
        }
        best
    };
    if !constrained {
        return sims.iter().map(|r| argmax(r)).collect();
    }
    let weights: Vec<Vec<f64>> = sims
        .iter()
        .map(|r| r.iter().map(|&x| f64::from(x)).collect())
        .collect();
    max_weight_assignment(&weights)
        .into_iter()
        .zip(sims)
        .map(|(a, r)| a.unwrap_or_else(|| argmax(r)))
        .collect()
}
