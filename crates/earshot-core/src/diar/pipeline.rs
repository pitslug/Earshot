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
//!    `constrained_argmax`).
//! 6. **Reconstruction and overlap.** Per global frame, each cluster's activity is the
//!    fraction of covering windows in which one of its local speakers is active, and the
//!    instantaneous speaker count is the rounded (ties to even, as `np.rint`) mean number
//!    of active local speakers over the covering windows. The `count` most active clusters
//!    are marked active, so overlapped speech yields two simultaneous turns.
//! 7. **Turns.** Runs of active frames per cluster become [`SpeakerTurn`]s; same-speaker
//!    gaps up to `merge_gap` are closed. Speakers are numbered by first turn.
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

use super::cluster::{ClusteringConfig, cluster_embeddings, cosine_similarity, normalized};
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
        let centroids = cluster_embeddings(&vectors, &self.config.clustering)?.centroids;
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

        // 6: reconstruction on the global frame grid with the per-frame speaker count.
        let k = centroids.len();
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
        let mut used: Vec<usize> = (0..k).filter(|&c| !runs[c].is_empty()).collect();
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
        Ok(Diarization {
            speakers,
            turns,
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
