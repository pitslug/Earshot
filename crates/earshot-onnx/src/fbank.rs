//! Kaldi-compatible log mel filterbank features, the front-end of the CAM++ speaker embedder.
//!
//! The 3D-Speaker CAM++ VoxCeleb model was trained on features from
//! `speakerlab/process/processor.py` `FBank` (3D-Speaker): `torchaudio.compliance.kaldi.fbank(
//! wav, num_mel_bins=80, sample_frequency=16000, dither=0.0)` on waveforms in `[-1, 1]`,
//! followed by mean normalisation over time (`feat - feat.mean(0)`). sherpa-onnx reproduces it
//! with kaldi-native-fbank for this export (model metadata `normalize_samples=1`,
//! `feature_normalize_type=global-mean`). The torchaudio defaults used
//! (`torchaudio/compliance/kaldi.py`, `fbank` and `get_mel_banks`):
//!
//! | parameter | value |
//! |---|---|
//! | frame length / shift | 25 ms (400 samples) / 10 ms (160 samples), `snip_edges=True` |
//! | DC offset | removed per frame (`remove_dc_offset=True`) |
//! | pre-emphasis | 0.97, first sample against itself (replicate padding) |
//! | window | povey: `hann(periodic=False) ^ 0.85` |
//! | FFT | 512 points (`round_to_power_of_two=True`), power spectrum (`use_power=True`) |
//! | mel bank | 80 triangular bins on `1127 ln(1 + f / 700)`, 20 Hz to Nyquist, Nyquist bin weight 0 |
//! | log | `ln(max(energy, f32::EPSILON))` |
//! | dither | 0 (inference) |

use std::sync::Arc;

use rustfft::num_complex::Complex32;
use rustfft::{Fft, FftPlanner};

/// Samples per frame (25 ms at 16 kHz).
pub const FRAME_LEN: usize = 400;
/// Samples between frames (10 ms at 16 kHz).
pub const FRAME_SHIFT: usize = 160;
/// FFT size (frame length rounded up to a power of two).
pub const FFT_LEN: usize = 512;
/// Mel bins.
pub const NUM_MELS: usize = 80;
const SAMPLE_RATE: f32 = 16_000.0;
const LOW_FREQ: f32 = 20.0;
const PREEMPH: f32 = 0.97;

fn mel(f: f32) -> f32 {
    1127.0 * (1.0 + f / 700.0).ln()
}

/// Kaldi fbank extractor with precomputed window, mel weights and FFT plan.
#[derive(Clone)]
pub struct KaldiFbank {
    window: Vec<f32>,
    /// `NUM_MELS x (FFT_LEN / 2 + 1)` weights, row-major.
    mel_weights: Vec<f32>,
    fft: Arc<dyn Fft<f32>>,
}

impl std::fmt::Debug for KaldiFbank {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("KaldiFbank").finish_non_exhaustive()
    }
}

impl Default for KaldiFbank {
    fn default() -> Self {
        Self::new()
    }
}

impl KaldiFbank {
    /// The extractor with the parameters in the module docs.
    #[must_use]
    pub fn new() -> Self {
        let n = FRAME_LEN as f32;
        let window = (0..FRAME_LEN)
            .map(|i| {
                let h = 0.5 - 0.5 * (2.0 * std::f32::consts::PI * i as f32 / (n - 1.0)).cos();
                h.powf(0.85)
            })
            .collect();
        // torchaudio `get_mel_banks`: bins over FFT_LEN / 2 points, the Nyquist column is 0.
        let bins = FFT_LEN / 2 + 1;
        let bin_width = SAMPLE_RATE / FFT_LEN as f32;
        let (mel_low, mel_high) = (mel(LOW_FREQ), mel(SAMPLE_RATE / 2.0));
        let delta = (mel_high - mel_low) / (NUM_MELS + 1) as f32;
        let mut mel_weights = vec![0.0f32; NUM_MELS * bins];
        for m in 0..NUM_MELS {
            let left = mel_low + m as f32 * delta;
            let centre = left + delta;
            let right = centre + delta;
            for k in 0..FFT_LEN / 2 {
                let f = mel(bin_width * k as f32);
                let up = (f - left) / (centre - left);
                let down = (right - f) / (right - centre);
                mel_weights[m * bins + k] = up.min(down).max(0.0);
            }
        }
        let fft = FftPlanner::new().plan_fft_forward(FFT_LEN);
        KaldiFbank {
            window,
            mel_weights,
            fft,
        }
    }

    /// Number of frames for `samples` input samples (`snip_edges`): 0 if shorter than a frame.
    #[must_use]
    pub fn num_frames(samples: usize) -> usize {
        if samples < FRAME_LEN {
            0
        } else {
            1 + (samples - FRAME_LEN) / FRAME_SHIFT
        }
    }

    /// Log mel energies, `frames x NUM_MELS` row-major, without mean normalisation.
    #[must_use]
    pub fn compute(&self, samples: &[f32]) -> Vec<f32> {
        let frames = Self::num_frames(samples.len());
        let bins = FFT_LEN / 2 + 1;
        let mut out = Vec::with_capacity(frames * NUM_MELS);
        let mut buf = vec![Complex32::new(0.0, 0.0); FFT_LEN];
        let mut frame = [0.0f32; FRAME_LEN];
        let mut power = vec![0.0f32; bins];
        for t in 0..frames {
            let start = t * FRAME_SHIFT;
            frame.copy_from_slice(&samples[start..start + FRAME_LEN]);
            let mean = frame.iter().sum::<f32>() / FRAME_LEN as f32;
            for v in &mut frame {
                *v -= mean;
            }
            for i in (1..FRAME_LEN).rev() {
                frame[i] -= PREEMPH * frame[i - 1];
            }
            frame[0] -= PREEMPH * frame[0];
            for (b, (&x, &w)) in buf.iter_mut().zip(frame.iter().zip(&self.window)) {
                *b = Complex32::new(x * w, 0.0);
            }
            for b in &mut buf[FRAME_LEN..] {
                *b = Complex32::new(0.0, 0.0);
            }
            self.fft.process(&mut buf);
            for (p, c) in power.iter_mut().zip(&buf) {
                *p = c.norm_sqr();
            }
            for m in 0..NUM_MELS {
                let w = &self.mel_weights[m * bins..(m + 1) * bins];
                let e: f32 = w.iter().zip(&power).map(|(a, b)| a * b).sum();
                out.push(e.max(f32::EPSILON).ln());
            }
        }
        out
    }

    /// Log mel energies with the per-utterance mean of every bin subtracted (3D-Speaker
    /// `mean_nor`, sherpa-onnx `global-mean`).
    #[must_use]
    pub fn compute_mean_normalized(&self, samples: &[f32]) -> Vec<f32> {
        let mut feats = self.compute(samples);
        let frames = feats.len() / NUM_MELS;
        if frames == 0 {
            return feats;
        }
        let mut mean = [0.0f32; NUM_MELS];
        for row in feats.chunks(NUM_MELS) {
            for (m, &v) in mean.iter_mut().zip(row) {
                *m += v;
            }
        }
        for m in &mut mean {
            *m /= frames as f32;
        }
        for row in feats.chunks_mut(NUM_MELS) {
            for (v, m) in row.iter_mut().zip(&mean) {
                *v -= m;
            }
        }
        feats
    }

    #[cfg(test)]
    fn weight(&self, mel_bin: usize, fft_bin: usize) -> f32 {
        self.mel_weights[mel_bin * (FFT_LEN / 2 + 1) + fft_bin]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sine(freq: f32, n: usize) -> Vec<f32> {
        (0..n)
            .map(|i| 0.5 * (2.0 * std::f32::consts::PI * freq * i as f32 / SAMPLE_RATE).sin())
            .collect()
    }

    #[test]
    fn diar_fbank_frame_count_follows_snip_edges() {
        assert_eq!(KaldiFbank::num_frames(399), 0);
        assert_eq!(KaldiFbank::num_frames(400), 1);
        assert_eq!(KaldiFbank::num_frames(559), 1);
        assert_eq!(KaldiFbank::num_frames(560), 2);
        assert_eq!(KaldiFbank::num_frames(16_000), 98);
        assert_eq!(
            KaldiFbank::new().compute(&sine(440.0, 16_000)).len(),
            98 * 80
        );
    }

    #[test]
    fn diar_fbank_povey_window_and_mel_bank_shape() {
        let f = KaldiFbank::new();
        assert!(f.window[0].abs() < 1e-6 && f.window[FRAME_LEN - 1].abs() < 1e-6);
        let mid = (0.5f32 - 0.5 * (std::f32::consts::PI * 199.0 * 2.0 / 399.0).cos()).powf(0.85);
        assert!((f.window[199] - mid).abs() < 1e-6);
        // Nyquist column and the bins below 20 Hz carry no weight.
        for m in 0..NUM_MELS {
            assert_eq!(f.weight(m, FFT_LEN / 2), 0.0);
            assert_eq!(f.weight(m, 0), 0.0);
        }
        // Every mel bin has some weight, peaks at most 1, and the bank covers the band.
        for m in 0..NUM_MELS {
            let row: Vec<f32> = (0..=FFT_LEN / 2).map(|k| f.weight(m, k)).collect();
            let peak = row.iter().copied().fold(0.0, f32::max);
            assert!(peak > 0.0 && peak <= 1.0, "mel {m} peak {peak}");
        }
    }

    #[test]
    fn diar_fbank_tone_peaks_in_the_mel_bin_of_its_frequency() {
        let f = KaldiFbank::new();
        for freq in [300.0f32, 1000.0, 4000.0] {
            let feats = f.compute(&sine(freq, 4000));
            let row = &feats[5 * NUM_MELS..6 * NUM_MELS];
            let best = (0..NUM_MELS)
                .max_by(|&a, &b| row[a].total_cmp(&row[b]))
                .unwrap();
            let (lo, hi) = (mel(LOW_FREQ), mel(SAMPLE_RATE / 2.0));
            let delta = (hi - lo) / (NUM_MELS + 1) as f32;
            let expect = ((mel(freq) - lo) / delta - 1.0).round() as i64;
            assert!(
                (best as i64 - expect).abs() <= 1,
                "{freq} Hz: bin {best}, expected about {expect}"
            );
        }
    }

    #[test]
    fn diar_fbank_mean_normalisation_and_silence_floor() {
        let f = KaldiFbank::new();
        let feats = f.compute_mean_normalized(&sine(700.0, 8000));
        for m in 0..NUM_MELS {
            let mean: f32 =
                feats.chunks(NUM_MELS).map(|r| r[m]).sum::<f32>() / (feats.len() / NUM_MELS) as f32;
            assert!(mean.abs() < 1e-4, "bin {m} mean {mean}");
        }
        let silent = f.compute(&[0.0; 800]);
        assert!(silent.iter().all(|&v| v == f32::EPSILON.ln()));
    }
}
