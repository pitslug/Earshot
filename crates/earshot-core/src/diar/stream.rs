//! The incremental half of the diarisation pipeline (T6.13d): [`WindowResults`],
//! [`WindowParams`] and [`DiarStream`].
//!
//! The pipeline ([`super::pipeline`]) has two parts. Steps 1-3 (segmentation of each
//! sliding window and the embeddings of its local speakers) depend only on the audio of
//! that window; steps 4-7 (clustering, stitching, reconstruction, turns) need every
//! window. Every window of the whole-track pass except the last one starts on the grid
//! `k * step` and is computed once its audio is known, so a meeting's windows can be
//! segmented and embedded while it runs and only the tail and the clustering remain at
//! the end:
//!
//! - [`DiarStream`] takes the track's audio as it arrives (from meeting time zero, at the
//!   pipeline rate); [`super::Diarizer::advance`] segments and embeds every batch of
//!   windows whose audio is all known, in the same batches and the same order as
//!   [`super::Diarizer::diarize`], so stateless models give bit-identical results.
//! - [`WindowResults`] is what the whole-track pass keeps after steps 1-3 for a prefix of
//!   its windows: activity per window, local speakers and their embeddings, local
//!   speakers still queued for embedding. [`super::Diarizer::diarize_from`] finishes the
//!   remaining windows (including the end-aligned last one) and runs steps 4-7; fed the
//!   same audio, its output equals [`super::Diarizer::diarize`] wherever the stream
//!   stopped.
//!
//! The results only fit a diarizer with the same [`WindowParams`] (the segmenter and its
//! window geometry, the embedding model, the window step, onset, overlap rule and batch
//! size); [`super::Diarizer::diarize_from`] refuses others with [`DiarError::State`].
//! Settings of steps 4-7 (clustering, assignment, merge gap, `min_active_ratio`) do not
//! affect them. [`WindowResults::to_bytes`] is an exact, versioned binary form.

use serde::{Deserialize, Serialize};

use super::types::{DiarError, EmbedderInfo, SegmenterInfo};
use crate::audio::Sample;

/// Everything that decides the per-window results (module docs).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WindowParams {
    /// The segmentation model and its window geometry.
    pub segmenter: SegmenterInfo,
    /// The embedding model, its dimension and minimum input.
    pub embedder: EmbedderInfo,
    /// Distance between window starts (samples at the pipeline rate): the global window
    /// grid is `k * window_step` from meeting time zero.
    pub window_step: usize,
    /// [`super::DiarizationConfig::onset`].
    pub onset: f32,
    /// [`super::DiarizationConfig::exclude_overlap`].
    pub exclude_overlap: bool,
    /// [`super::DiarizationConfig::batch_size`].
    pub batch_size: usize,
}

/// One active local speaker of one window (pipeline step 3).
#[derive(Debug, Clone, PartialEq)]
pub(super) struct Local {
    pub(super) window: usize,
    pub(super) local: usize,
    pub(super) clean_frames: usize,
    /// Unit-length embedding, if the region was long enough and the model's output valid.
    pub(super) embedding: Option<Vec<f32>>,
}

/// The per-window results of a prefix of the windows (module docs).
#[derive(Debug, Clone, PartialEq)]
pub struct WindowResults {
    pub(super) params: WindowParams,
    /// Binarised `frames x local speakers` activity per window.
    pub(super) bins: Vec<Vec<bool>>,
    pub(super) locals: Vec<Local>,
    /// Indices into `locals` queued for the next embedding call.
    pub(super) pending: Vec<usize>,
}

/// Where [`WindowResults::rollback`] returns to.
pub(super) struct Mark {
    bins: usize,
    locals: usize,
    pending: Vec<usize>,
}

/// Leading bytes of [`WindowResults::to_bytes`].
const MAGIC: &[u8; 4] = b"EDWR";

/// The format of [`WindowResults::to_bytes`]; other versions are refused.
pub const WINDOW_RESULTS_FORMAT: u32 = 1;

impl WindowResults {
    /// No windows yet.
    #[must_use]
    pub fn new(params: WindowParams) -> Self {
        WindowResults {
            params,
            bins: Vec::new(),
            locals: Vec::new(),
            pending: Vec::new(),
        }
    }

    /// The settings these results were computed with.
    #[must_use]
    pub fn params(&self) -> &WindowParams {
        &self.params
    }

    /// Windows segmented: the first `windows()` windows of the grid.
    #[must_use]
    pub fn windows(&self) -> usize {
        self.bins.len()
    }

    /// The unit-length embeddings of the local speakers computed so far, in window order
    /// (for inspection, for example checks that a stored copy was erased).
    pub fn embeddings(&self) -> impl Iterator<Item = &[f32]> {
        self.locals.iter().filter_map(|l| l.embedding.as_deref())
    }

    /// Samples from meeting time zero that the computed windows cover.
    #[must_use]
    pub fn covered(&self) -> usize {
        match self.bins.len() {
            0 => 0,
            n => (n - 1) * self.params.window_step + self.params.segmenter.window,
        }
    }

    pub(super) fn mark(&self) -> Mark {
        Mark {
            bins: self.bins.len(),
            locals: self.locals.len(),
            pending: self.pending.clone(),
        }
    }

    /// Undoes everything since `mark` (a failed batch).
    pub(super) fn rollback(&mut self, mark: Mark) {
        self.bins.truncate(mark.bins);
        self.locals.truncate(mark.locals);
        for &li in &mark.pending {
            if let Some(l) = self.locals.get_mut(li) {
                l.embedding = None;
            }
        }
        self.pending = mark.pending;
    }

    /// Checks the internal consistency of results that may come from storage.
    ///
    /// # Errors
    /// [`DiarError::State`] naming the first inconsistency.
    pub(super) fn check(&self) -> Result<(), DiarError> {
        let bad = |m: &str| Err(DiarError::State(format!("window results: {m}")));
        let p = &self.params;
        if let Err(e) = p.segmenter.validate() {
            return bad(&e.to_string());
        }
        if p.window_step == 0 || p.batch_size == 0 {
            return bad("zero window step or batch size");
        }
        let cells = p.segmenter.frames * p.segmenter.max_local_speakers;
        if self.bins.iter().any(|b| b.len() != cells) {
            return bad("activity of the wrong shape");
        }
        let mut last = (0, 0);
        for (i, l) in self.locals.iter().enumerate() {
            if l.window >= self.bins.len() || l.local >= p.segmenter.max_local_speakers {
                return bad("a local speaker outside the windows");
            }
            if i > 0 && (l.window, l.local) <= last {
                return bad("local speakers out of order");
            }
            last = (l.window, l.local);
            if l.clean_frames > p.segmenter.frames {
                return bad("more clean frames than frames");
            }
            if l.embedding
                .as_ref()
                .is_some_and(|e| e.len() != p.embedder.dimension)
            {
                return bad("an embedding of the wrong dimension");
            }
        }
        for (i, &li) in self.pending.iter().enumerate() {
            if li >= self.locals.len() || (i > 0 && li <= self.pending[i - 1]) {
                return bad("queued local speakers out of range or order");
            }
            if self.locals[li].embedding.is_some() {
                return bad("a queued local speaker is already embedded");
            }
        }
        Ok(())
    }

    /// An exact binary form (little endian): magic, [`WINDOW_RESULTS_FORMAT`], the
    /// parameters as JSON, the activity bit-packed per window, the local speakers with
    /// their embeddings, the queue.
    #[must_use]
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(MAGIC);
        out.extend_from_slice(&WINDOW_RESULTS_FORMAT.to_le_bytes());
        // WindowParams is plain data; serialising it cannot fail.
        let params = serde_json::to_vec(&self.params).unwrap_or_default();
        put_len(&mut out, params.len());
        out.extend_from_slice(&params);
        put_len(&mut out, self.bins.len());
        for bin in &self.bins {
            let mut bytes = vec![0u8; bin.len().div_ceil(8)];
            for (i, _) in bin.iter().enumerate().filter(|(_, b)| **b) {
                bytes[i / 8] |= 1 << (i % 8);
            }
            out.extend_from_slice(&bytes);
        }
        put_len(&mut out, self.locals.len());
        for l in &self.locals {
            put_len(&mut out, l.window);
            put_len(&mut out, l.local);
            put_len(&mut out, l.clean_frames);
            match &l.embedding {
                None => out.push(0),
                Some(e) => {
                    out.push(1);
                    for x in e {
                        out.extend_from_slice(&x.to_le_bytes());
                    }
                }
            }
        }
        put_len(&mut out, self.pending.len());
        for &li in &self.pending {
            put_len(&mut out, li);
        }
        out
    }

    /// Reads [`WindowResults::to_bytes`].
    ///
    /// # Errors
    /// [`DiarError::State`] for another format, damaged or inconsistent bytes.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, DiarError> {
        let mut r = Reader { bytes, at: 0 };
        if r.take(4)? != MAGIC {
            return Err(DiarError::State(
                "not diarisation window results".to_owned(),
            ));
        }
        let format = r.u32()?;
        if format != WINDOW_RESULTS_FORMAT {
            return Err(DiarError::State(format!(
                "window results format {format}, expected {WINDOW_RESULTS_FORMAT}"
            )));
        }
        let n = r.len()?;
        let params: WindowParams = serde_json::from_slice(r.take(n)?)
            .map_err(|e| DiarError::State(format!("window parameters: {e}")))?;
        let cells = params
            .segmenter
            .frames
            .checked_mul(params.segmenter.max_local_speakers)
            .ok_or_else(|| DiarError::State("window results: bad window shape".to_owned()))?;
        let windows = r.len()?;
        let mut bins = Vec::new();
        for _ in 0..windows {
            let packed = r.take(cells.div_ceil(8))?;
            bins.push(
                (0..cells)
                    .map(|i| packed[i / 8] & (1 << (i % 8)) != 0)
                    .collect(),
            );
        }
        let count = r.len()?;
        let mut locals = Vec::new();
        for _ in 0..count {
            let (window, local, clean_frames) = (r.len()?, r.len()?, r.len()?);
            let embedding = match r.take(1)?[0] {
                0 => None,
                1 => Some(
                    r.take(params.embedder.dimension.saturating_mul(4))?
                        .as_chunks::<4>()
                        .0
                        .iter()
                        .map(|b| f32::from_le_bytes(*b))
                        .collect(),
                ),
                _ => return Err(DiarError::State("window results: bad flag".to_owned())),
            };
            locals.push(Local {
                window,
                local,
                clean_frames,
                embedding,
            });
        }
        let count = r.len()?;
        let mut pending = Vec::new();
        for _ in 0..count {
            pending.push(r.len()?);
        }
        if r.at != bytes.len() {
            return Err(DiarError::State(
                "window results: trailing bytes".to_owned(),
            ));
        }
        let results = WindowResults {
            params,
            bins,
            locals,
            pending,
        };
        results.check()?;
        Ok(results)
    }
}

fn put_len(out: &mut Vec<u8>, n: usize) {
    out.extend_from_slice(&(n as u64).to_le_bytes());
}

struct Reader<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], DiarError> {
        let end = self
            .at
            .checked_add(n)
            .filter(|&e| e <= self.bytes.len())
            .ok_or_else(|| DiarError::State("window results are truncated".to_owned()))?;
        let s = &self.bytes[self.at..end];
        self.at = end;
        Ok(s)
    }

    fn u32(&mut self) -> Result<u32, DiarError> {
        let b = self.take(4)?;
        Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }

    /// A length or index, bounded by the bytes left so a damaged count cannot allocate.
    fn len(&mut self) -> Result<usize, DiarError> {
        let b = self.take(8)?;
        let v = u64::from_le_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]]);
        usize::try_from(v)
            .ok()
            .filter(|&v| v <= self.bytes.len().saturating_mul(8))
            .ok_or_else(|| DiarError::State("window results: a count out of range".to_owned()))
    }
}

/// A track's audio arriving during the meeting and the windows computed from it so far
/// (module docs). Made by [`super::Diarizer::stream`]; work is done by
/// [`super::Diarizer::advance`].
#[derive(Debug, Clone)]
pub struct DiarStream {
    pub(super) results: WindowResults,
    /// Audio from sample `buf_start` (meeting time zero = 0) to everything received.
    pub(super) buf: Vec<Sample>,
    pub(super) buf_start: usize,
}

impl DiarStream {
    pub(super) fn new(params: WindowParams) -> Self {
        DiarStream {
            results: WindowResults::new(params),
            buf: Vec::new(),
            buf_start: 0,
        }
    }

    /// Appends the next samples of the track (16 kHz). Cheap: no model runs.
    pub fn append(&mut self, samples: &[Sample]) {
        self.buf.extend_from_slice(samples);
    }

    /// Samples received since meeting time zero.
    #[must_use]
    pub fn received(&self) -> usize {
        self.buf_start + self.buf.len()
    }

    /// Samples held for windows not yet computed or regions not yet embedded.
    #[must_use]
    pub fn buffered(&self) -> usize {
        self.buf.len()
    }

    /// Windows computed so far.
    #[must_use]
    pub fn windows_done(&self) -> usize {
        self.results.windows()
    }

    /// Batches of windows whose audio is all known and that [`super::Diarizer::advance`]
    /// would compute now.
    #[must_use]
    pub fn ready_batches(&self) -> usize {
        let p = &self.results.params;
        let received = self.received();
        if received < p.segmenter.window {
            return 0;
        }
        let known = (received - p.segmenter.window) / p.window_step + 1;
        known.saturating_sub(self.results.windows()) / p.batch_size
    }

    /// The results so far.
    #[must_use]
    pub fn results(&self) -> &WindowResults {
        &self.results
    }

    /// The results so far, for [`super::Diarizer::diarize_from`] or storage.
    #[must_use]
    pub fn into_results(self) -> WindowResults {
        self.results
    }

    /// Drops audio that no remaining window or queued region needs.
    pub(super) fn trim(&mut self) {
        let step = self.results.params.window_step;
        let r = &self.results;
        let next = r.bins.len() * step;
        let keep = r
            .pending
            .iter()
            .map(|&li| r.locals[li].window * step)
            .fold(next, usize::min);
        let drop = keep.saturating_sub(self.buf_start).min(self.buf.len());
        self.buf.drain(..drop);
        self.buf_start += drop;
    }
}
