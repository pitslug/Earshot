//! Deterministic test doubles: [`FakeSegmenter`], [`FakeEmbedder`] and [`scene_audio`].
//!
//! The fakes read the ground truth out of the audio itself. [`scene_audio`] writes, in
//! every sample, the set of active speakers as a bit mask (`mask / 256`, exact in `f32`);
//! [`FakeSegmenter`] decodes the mask per output frame and [`FakeEmbedder`] embeds a
//! region as the mixture of the speakers heard in it. So the pipeline can be tested end to
//! end without models and without the fakes knowing window positions.

use std::time::Duration;

use super::types::{
    DiarError, EmbedderInfo, Embedding, EmbeddingModelId, LocalActivity, SegmenterInfo,
    SpeakerEmbedder, SpeakerSegmenter,
};
use crate::asr::CancelToken;
use crate::audio::{MonoBuffer, PIPELINE_RATE, Sample};

/// Most speakers [`scene_audio`] can encode.
pub const MAX_SCENE_SPEAKERS: usize = 8;

/// One ground-truth turn of a synthetic scene (milliseconds from the buffer start).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SceneTurn {
    /// Speaker number, `< MAX_SCENE_SPEAKERS`.
    pub speaker: usize,
    /// Start in ms.
    pub start_ms: u64,
    /// End in ms (exclusive).
    pub end_ms: u64,
}

impl SceneTurn {
    /// Speaker `speaker` from `start_ms` to `end_ms`.
    #[must_use]
    pub fn new(speaker: usize, start_ms: u64, end_ms: u64) -> Self {
        SceneTurn {
            speaker,
            start_ms,
            end_ms,
        }
    }
}

/// 16 kHz audio of `total_ms` whose every sample encodes the set of speakers active at
/// that time (bit `k` for speaker `k`) as `mask / 256`. Silence is 0.
///
/// # Panics
/// If a turn's speaker is `>= MAX_SCENE_SPEAKERS`.
#[must_use]
pub fn scene_audio(turns: &[SceneTurn], total_ms: u64) -> MonoBuffer {
    let ms = |t: u64| usize::try_from(t * 16).unwrap_or(usize::MAX);
    let n = ms(total_ms);
    let mut masks = vec![0u32; n];
    for t in turns {
        assert!(t.speaker < MAX_SCENE_SPEAKERS, "scene speaker out of range");
        let (s, e) = (ms(t.start_ms).min(n), ms(t.end_ms).min(n));
        for m in &mut masks[s..e] {
            *m |= 1 << t.speaker;
        }
    }
    MonoBuffer::new(
        PIPELINE_RATE,
        masks.into_iter().map(|m| m as f32 / 256.0).collect(),
    )
}

/// The speaker mask encoded in one [`scene_audio`] sample.
#[must_use]
pub fn sample_mask(x: Sample) -> u32 {
    ((x * 256.0).round().clamp(0.0, 255.0)) as u32
}

/// Most frequent mask in `samples` (lowest mask on ties), 0 for no samples.
fn mode_mask(samples: &[Sample]) -> u32 {
    let mut counts = [0usize; 256];
    for &x in samples {
        counts[sample_mask(x) as usize] += 1;
    }
    let mut best = 0;
    for (m, &c) in counts.iter().enumerate() {
        if c > counts[best] {
            best = m;
        }
    }
    best as u32
}

/// A [`SpeakerSegmenter`] that decodes [`scene_audio`].
///
/// For each output frame it takes the most frequent mask among the samples the frame
/// stands for. Speakers are given local slots in order of first appearance in the window,
/// rotated by `rotation * call_index` (to scramble local labels across windows); speakers
/// beyond [`SegmenterInfo::max_local_speakers`] in one window are not reported, like a
/// real segmentation model's local-speaker limit. With
/// [`FakeSegmenter::with_boundary_jitter`], each window decodes its frames from slightly
/// shifted samples, so boundaries differ from window to window.
#[derive(Debug, Clone)]
pub struct FakeSegmenter {
    info: SegmenterInfo,
    rotation: usize,
    windows_seen: usize,
    /// Largest per-window boundary shift, in samples.
    jitter: usize,
    fail: Option<String>,
    frames_override: Option<usize>,
    /// Jitter and rotation from the window's content instead of the call count.
    content_seeded: bool,
}

impl FakeSegmenter {
    /// A fake with the given geometry.
    #[must_use]
    pub fn new(info: SegmenterInfo) -> Self {
        FakeSegmenter {
            info,
            rotation: 0,
            windows_seen: 0,
            jitter: 0,
            fail: None,
            frames_override: None,
            content_seeded: false,
        }
    }

    /// A fake with pyannote segmentation-3.0 geometry.
    #[must_use]
    pub fn segmentation_3_0() -> Self {
        Self::new(SegmenterInfo::pyannote_segmentation_3_0(
            "fake-segmentation-3.0",
        ))
    }

    /// Rotates local slots by `rotation` per window.
    #[must_use]
    pub fn with_rotation(mut self, rotation: usize) -> Self {
        self.rotation = rotation;
        self
    }

    /// Shifts every frame of a window by the same deterministic per-window offset in
    /// `[-jitter, +jitter]` before decoding, so each window places speaker boundaries
    /// slightly wrong (as a real model does) and the pipeline has to reconcile windows.
    #[must_use]
    pub fn with_boundary_jitter(mut self, jitter: Duration) -> Self {
        self.jitter = PIPELINE_RATE.frames_for(jitter);
        self
    }

    /// Every call fails with [`DiarError::Backend`]`(message)`.
    #[must_use]
    pub fn failing(mut self, message: impl Into<String>) -> Self {
        self.fail = Some(message.into());
        self
    }

    /// Derives each window's jitter and rotation from a hash of the window's samples
    /// instead of the number of windows seen, so the output is a function of the input
    /// alone, as with a real model: a fresh fake gives the same answer for a window as one
    /// that has already processed others (tests that resume work on another instance).
    #[must_use]
    pub fn content_seeded(mut self) -> Self {
        self.content_seeded = true;
        self
    }

    /// Reports `frames` frames per window instead of the declared count (shape-error
    /// tests).
    #[must_use]
    pub fn with_wrong_frame_count(mut self, frames: usize) -> Self {
        self.frames_override = Some(frames);
        self
    }

    /// Number of windows processed so far.
    #[must_use]
    pub fn windows_seen(&self) -> usize {
        self.windows_seen
    }
}

impl SpeakerSegmenter for FakeSegmenter {
    fn info(&self) -> &SegmenterInfo {
        &self.info
    }

    fn segment(
        &mut self,
        windows: &[&[Sample]],
        cancel: &CancelToken,
    ) -> Result<Vec<LocalActivity>, DiarError> {
        if cancel.is_cancelled() {
            return Err(DiarError::Cancelled);
        }
        if let Some(m) = &self.fail {
            return Err(DiarError::Backend(m.clone()));
        }
        let info = &self.info;
        let slots = info.max_local_speakers;
        let mut out = Vec::with_capacity(windows.len());
        for w in windows {
            if w.len() != info.window {
                return Err(DiarError::Shape(format!(
                    "fake segmenter got {} samples, expected {}",
                    w.len(),
                    info.window
                )));
            }
            // The window's identity for its jitter and rotation: its position in the call
            // sequence, or its content.
            let key = if self.content_seeded {
                content_hash(w)
            } else {
                self.windows_seen as u64
            };
            // This window's boundary error, the same for every frame of the window.
            let offset = if self.jitter == 0 {
                0
            } else {
                let mut n = Noise(0x9E37_79B9_7F4A_7C15 ^ (key + 1));
                n.next();
                (n.next() * self.jitter as f32).round() as isize
            };
            let masks: Vec<u32> = (0..info.frames)
                .map(|i| {
                    let len = w.len() as isize;
                    let s = (info.frame_start(i) as isize + offset).clamp(0, len) as usize;
                    let e = (s + info.frame_step).min(w.len());
                    mode_mask(&w[s..e])
                })
                .collect();
            let mut order: Vec<usize> = Vec::new();
            for &m in &masks {
                for k in 0..MAX_SCENE_SPEAKERS {
                    if m & (1 << k) != 0 && !order.contains(&k) {
                        order.push(k);
                    }
                }
            }
            order.truncate(slots);
            let shift = self.rotation.wrapping_mul(key as usize) % slots;
            let frames = self.frames_override.unwrap_or(info.frames);
            let mut data = vec![0.0f32; frames * slots];
            for (f, &m) in masks.iter().enumerate().take(frames) {
                for (pos, &k) in order.iter().enumerate() {
                    if m & (1 << k) != 0 {
                        data[f * slots + (pos + shift) % slots] = 1.0;
                    }
                }
            }
            self.windows_seen += 1;
            out.push(LocalActivity::new(frames, slots, data)?);
        }
        Ok(out)
    }
}

/// FNV-1a over the length and the bits of every 61st sample of `samples` (cheap in debug
/// builds; any function of the input will do).
fn content_hash(samples: &[Sample]) -> u64 {
    let mut h: u64 = 0xCBF2_9CE4_8422_2325 ^ samples.len() as u64;
    for &x in samples.iter().step_by(61) {
        for b in x.to_bits().to_le_bytes() {
            h ^= u64::from(b);
            h = h.wrapping_mul(0x0000_0100_0000_01B3);
        }
    }
    h
}

/// xorshift64* in `[-1, 1)`.
#[derive(Debug, Clone)]
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

/// A [`SpeakerEmbedder`] that decodes [`scene_audio`].
///
/// A region embeds as the mixture of the speakers heard in it: the mean of
/// `centroids[k]` weighted by the number of samples in which speaker `k` is active (an
/// overlapped sample counts for each of its speakers, so overlapped speech pulls the
/// embedding towards the other voice, as with a real embedder), plus uniform noise of
/// amplitude `noise` per component from a seeded generator. A region with no speech
/// yields an all-zero (invalid) embedding.
#[derive(Debug, Clone)]
pub struct FakeEmbedder {
    info: EmbedderInfo,
    centroids: Vec<Vec<f32>>,
    noise: f32,
    rng: Noise,
    seed: u64,
    content_seeded: bool,
    regions: Vec<usize>,
    fail: Option<String>,
}

impl FakeEmbedder {
    /// Speaker `k` embeds near `centroids[k]`. `min_samples` is 0.25 s.
    ///
    /// # Panics
    /// If `centroids` is empty or their dimensions differ.
    #[must_use]
    pub fn new(model: &str, centroids: Vec<Vec<f32>>, noise: f32, seed: u64) -> Self {
        assert!(!centroids.is_empty(), "fake embedder needs centroids");
        let dimension = centroids[0].len();
        assert!(centroids.iter().all(|c| c.len() == dimension));
        FakeEmbedder {
            info: EmbedderInfo {
                model: EmbeddingModelId::new(model),
                dimension,
                min_samples: 4000,
            },
            centroids,
            noise,
            rng: Noise(seed | 1),
            seed,
            content_seeded: false,
            regions: Vec::new(),
            fail: None,
        }
    }

    /// Every call fails with [`DiarError::Backend`]`(message)`.
    #[must_use]
    pub fn failing(mut self, message: impl Into<String>) -> Self {
        self.fail = Some(message.into());
        self
    }

    /// Draws each region's noise from a generator seeded by the region's samples (and the
    /// seed) instead of one generator running across calls, so the output is a function of
    /// the input alone, as with a real model ([`FakeSegmenter::content_seeded`]).
    #[must_use]
    pub fn content_seeded(mut self) -> Self {
        self.content_seeded = true;
        self
    }

    /// Length in samples of every region embedded so far.
    #[must_use]
    pub fn regions(&self) -> &[usize] {
        &self.regions
    }

    /// Per speaker, the number of samples in which it is active (an overlapped sample
    /// counts for every speaker in it).
    fn presence(speech: &[Sample]) -> [usize; MAX_SCENE_SPEAKERS] {
        let mut masks = [0usize; 256];
        for &x in speech {
            masks[sample_mask(x) as usize] += 1;
        }
        let mut counts = [0usize; MAX_SCENE_SPEAKERS];
        for (m, &n) in masks.iter().enumerate().filter(|(_, n)| **n > 0) {
            for (k, c) in counts.iter_mut().enumerate() {
                if m & (1 << k) != 0 {
                    *c += n;
                }
            }
        }
        counts
    }
}

impl SpeakerEmbedder for FakeEmbedder {
    fn info(&self) -> &EmbedderInfo {
        &self.info
    }

    fn embed(
        &mut self,
        speech: &[&[Sample]],
        cancel: &CancelToken,
    ) -> Result<Vec<Embedding>, DiarError> {
        if cancel.is_cancelled() {
            return Err(DiarError::Cancelled);
        }
        if let Some(m) = &self.fail {
            return Err(DiarError::Backend(m.clone()));
        }
        let mut out = Vec::with_capacity(speech.len());
        for s in speech {
            self.regions.push(s.len());
            let counts = Self::presence(s);
            let total: usize = counts.iter().sum();
            let vector = if total == 0 {
                vec![0.0; self.info.dimension]
            } else {
                let mut v = vec![0.0f32; self.info.dimension];
                for (k, &n) in counts.iter().enumerate() {
                    if n == 0 {
                        continue;
                    }
                    let c = self.centroids.get(k).ok_or_else(|| {
                        DiarError::Backend(format!("fake embedder has no centroid {k}"))
                    })?;
                    let wgt = n as f32 / total as f32;
                    for (x, &y) in v.iter_mut().zip(c) {
                        *x += wgt * y;
                    }
                }
                let mut own = Noise((content_hash(s) ^ self.seed) | 1);
                let rng = if self.content_seeded {
                    &mut own
                } else {
                    &mut self.rng
                };
                v.iter().map(|&x| x + self.noise * rng.next()).collect()
            };
            out.push(Embedding::new(self.info.model.clone(), vector));
        }
        Ok(out)
    }
}
