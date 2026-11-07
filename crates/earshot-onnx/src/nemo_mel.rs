//! NeMo log-mel features, the front end of NVIDIA's Parakeet models (T6.10e1).
//!
//! Reference: NVIDIA NeMo `nemo/collections/asr/parts/preprocessing/features.py`,
//! `FilterbankFeatures.forward`, with the `AudioToMelSpectrogramPreprocessor` settings in the
//! Parakeet checkpoints' `model_config.yaml` (parakeet-tdt_ctc-110m: 80 bins; parakeet-tdt
//! 0.6b v2/v3: 128 bins); cross-checked against onnx-asr's NumPy port
//! (`src/onnx_asr/preprocessors/numpy_preprocessor.py`) and checked in tests against onnx-asr's
//! `nemo128.onnx`. Inference settings (dither is training only):
//!
//! | step | value |
//! |---|---|
//! | valid frames | `len / 160` (`get_seq_len`: `(len + 2 * 256 - 512) / 160`) |
//! | pre-emphasis | 0.97 over the whole signal, first sample kept |
//! | STFT | `center=True` with zero padding of 256 each side, hop 160, `n_fft` 512 |
//! | window | `torch.hann_window(400, periodic=False)` centred in 512 (56 zeros each side) |
//! | spectrum | power (`mag_power` 2) |
//! | mel | `librosa.filters.mel(sr=16000, n_fft=512, fmin=0, fmax=8000, htk=False, norm="slaney")` |
//! | log | `ln(x + 2^-24)` (`log_zero_guard_type` add) |
//! | normalise | per feature over the valid frames: mean, std with N-1, `std += 1e-5` |
//! | mask | frames at or past the valid count set to 0 |
//!
//! Output is `[n_mels][frames]` row-major (the models' `[B, n_mels, T]` input with B = 1) with
//! `frames = 1 + len / 160` (the STFT's frame count) of which the first `len / 160` are valid.

use std::sync::Arc;

use rustfft::num_complex::Complex32;
use rustfft::{Fft, FftPlanner};

/// Samples per hop (10 ms at 16 kHz).
pub const HOP: usize = 160;
/// Window length (25 ms at 16 kHz).
pub const WIN: usize = 400;
/// FFT size.
pub const N_FFT: usize = 512;
const BINS: usize = N_FFT / 2 + 1;
const SAMPLE_RATE: f64 = 16_000.0;
const PREEMPH: f32 = 0.97;
const LOG_GUARD: f32 = 1.0 / 16_777_216.0; // 2^-24
const STD_GUARD: f32 = 1e-5;

/// librosa `hz_to_mel` (Slaney: linear below 1 kHz, logarithmic above).
fn hz_to_mel(f: f64) -> f64 {
    let f_sp = 200.0 / 3.0;
    let min_log_hz = 1000.0;
    let min_log_mel = min_log_hz / f_sp;
    let logstep = 6.4f64.ln() / 27.0;
    if f >= min_log_hz {
        min_log_mel + (f / min_log_hz).ln() / logstep
    } else {
        f / f_sp
    }
}

/// librosa `mel_to_hz` (Slaney).
fn mel_to_hz(m: f64) -> f64 {
    let f_sp = 200.0 / 3.0;
    let min_log_hz = 1000.0;
    let min_log_mel = min_log_hz / f_sp;
    let logstep = 6.4f64.ln() / 27.0;
    if m >= min_log_mel {
        min_log_hz * (logstep * (m - min_log_mel)).exp()
    } else {
        f_sp * m
    }
}

/// librosa `filters.mel(sr=16000, n_fft=512, n_mels, fmin=0, fmax=8000, htk=False,
/// norm="slaney")`, computed in f64 as librosa does, `[n_mels][BINS]`.
#[must_use]
pub fn slaney_mel_filters(n_mels: usize) -> Vec<f32> {
    #[allow(clippy::cast_precision_loss)]
    let fft_freqs: Vec<f64> = (0..BINS)
        .map(|k| k as f64 * (SAMPLE_RATE / 2.0) / (BINS - 1) as f64)
        .collect();
    let (lo, hi) = (hz_to_mel(0.0), hz_to_mel(SAMPLE_RATE / 2.0));
    #[allow(clippy::cast_precision_loss)]
    let mel_f: Vec<f64> = (0..n_mels + 2)
        .map(|i| mel_to_hz(lo + (hi - lo) * i as f64 / (n_mels + 1) as f64))
        .collect();
    let mut out = vec![0.0f32; n_mels * BINS];
    for i in 0..n_mels {
        let (d0, d1) = (mel_f[i + 1] - mel_f[i], mel_f[i + 2] - mel_f[i + 1]);
        let enorm = 2.0 / (mel_f[i + 2] - mel_f[i]);
        for (k, &f) in fft_freqs.iter().enumerate() {
            let lower = (f - mel_f[i]) / d0;
            let upper = (mel_f[i + 2] - f) / d1;
            #[allow(clippy::cast_possible_truncation)]
            let w = (lower.min(upper).max(0.0) * enorm) as f32;
            out[i * BINS + k] = w;
        }
    }
    out
}

/// The NeMo front end for `n_mels` bins (module docs).
#[derive(Clone)]
pub struct NemoMel {
    n_mels: usize,
    filters: Vec<f32>,
    window: Vec<f32>,
    fft: Arc<dyn Fft<f32>>,
}

impl std::fmt::Debug for NemoMel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NemoMel")
            .field("n_mels", &self.n_mels)
            .finish_non_exhaustive()
    }
}

impl NemoMel {
    /// A front end with `n_mels` bins (80 or 128 for the Parakeet models).
    #[must_use]
    pub fn new(n_mels: usize) -> Self {
        // torch.hann_window(400, periodic=False), zero-padded to 512 and centred.
        let mut window = vec![0.0f32; N_FFT];
        let left = (N_FFT - WIN) / 2;
        #[allow(clippy::cast_precision_loss)]
        for n in 0..WIN {
            let x = 2.0 * std::f64::consts::PI * n as f64 / (WIN - 1) as f64;
            #[allow(clippy::cast_possible_truncation)]
            let w = (0.5 - 0.5 * x.cos()) as f32;
            window[left + n] = w;
        }
        Self {
            n_mels,
            filters: slaney_mel_filters(n_mels),
            window,
            fft: FftPlanner::new().plan_fft_forward(N_FFT),
        }
    }

    /// Mel bins.
    #[must_use]
    pub fn n_mels(&self) -> usize {
        self.n_mels
    }

    /// `(frames, valid frames)` for `samples` input samples.
    #[must_use]
    pub fn frames(samples: usize) -> (usize, usize) {
        (1 + samples / HOP, samples / HOP)
    }

    /// Features of 16 kHz `samples`: `[n_mels][frames]` row-major (module docs).
    #[must_use]
    pub fn compute(&self, samples: &[f32]) -> Vec<f32> {
        let (_, valid) = Self::frames(samples.len());
        self.compute_valid(samples, valid)
    }

    /// [`NemoMel::compute`] with `valid` frames normalised and kept instead of NeMo's
    /// `len / 160` (at most all frames). onnx-asr's `nemo80/128.onnx` keep every frame
    /// (`1 + len / 160`); the tests use this to compare everything else with them exactly.
    #[must_use]
    pub fn compute_valid(&self, samples: &[f32], valid: usize) -> Vec<f32> {
        let (frames, _) = Self::frames(samples.len());
        let valid = valid.min(frames);
        // Pre-emphasis, then the centre padding.
        let pad = N_FFT / 2;
        let mut x = vec![0.0f32; samples.len() + 2 * pad];
        for (i, s) in samples.iter().enumerate() {
            let prev = if i == 0 { 0.0 } else { samples[i - 1] };
            x[pad + i] = if i == 0 { *s } else { s - PREEMPH * prev };
        }
        let mut power = vec![0.0f32; BINS];
        let mut buf = vec![Complex32::new(0.0, 0.0); N_FFT];
        let mut out = vec![0.0f32; self.n_mels * frames];
        for t in 0..frames {
            let start = t * HOP;
            for (n, b) in buf.iter_mut().enumerate() {
                *b = Complex32::new(x[start + n] * self.window[n], 0.0);
            }
            self.fft.process(&mut buf);
            for (k, p) in power.iter_mut().enumerate() {
                *p = buf[k].norm_sqr();
            }
            for m in 0..self.n_mels {
                let row = &self.filters[m * BINS..(m + 1) * BINS];
                let e: f32 = row.iter().zip(&power).map(|(w, p)| w * p).sum();
                out[m * frames + t] = (e + LOG_GUARD).ln();
            }
        }
        // Per-feature normalisation over the valid frames, then the mask.
        for m in 0..self.n_mels {
            let row = &mut out[m * frames..(m + 1) * frames];
            #[allow(clippy::cast_precision_loss)]
            let (n, dof) = (valid as f32, valid.saturating_sub(1) as f32);
            let mean = row[..valid].iter().sum::<f32>() / n.max(1.0);
            let var = row[..valid]
                .iter()
                .map(|v| (v - mean) * (v - mean))
                .sum::<f32>();
            // NeMo: std over N-1 (NaN for one frame, set to 0), then + 1e-5.
            let std = if valid > 1 { (var / dof).sqrt() } else { 0.0 } + STD_GUARD;
            for (t, v) in row.iter_mut().enumerate() {
                *v = if t < valid { (*v - mean) / std } else { 0.0 };
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parakeet_mel_scale_matches_librosa_slaney_points() {
        // librosa.hz_to_mel(1000) = 15, (6400) = 42, linear below 1 kHz.
        assert!((hz_to_mel(1000.0) - 15.0).abs() < 1e-12);
        assert!((hz_to_mel(6400.0) - 42.0).abs() < 1e-9);
        assert!((hz_to_mel(500.0) - 7.5).abs() < 1e-12);
        for f in [0.0, 300.0, 999.0, 1000.0, 4321.0, 8000.0] {
            assert!((mel_to_hz(hz_to_mel(f)) - f).abs() < 1e-6, "{f}");
        }
    }

    #[test]
    fn parakeet_mel_filters_are_slaney_triangles() {
        let fb = slaney_mel_filters(80);
        assert_eq!(fb.len(), 80 * BINS);
        for m in 0..80 {
            let row = &fb[m * BINS..(m + 1) * BINS];
            assert!(row.iter().all(|w| *w >= 0.0));
            assert!(row.iter().any(|w| *w > 0.0), "filter {m} is empty");
            // One peak, rising then falling.
            let peak = row
                .iter()
                .enumerate()
                .max_by(|a, b| a.1.total_cmp(b.1))
                .unwrap()
                .0;
            assert!(row[..peak].windows(2).all(|w| w[0] <= w[1]));
            assert!(row[peak..].windows(2).all(|w| w[0] >= w[1]));
        }
        // Slaney area normalisation: each triangle integrates (in Hz) to about 1.
        let hz_per_bin = 8000.0 / 256.0;
        for m in [10, 40, 79] {
            let area: f32 = fb[m * BINS..(m + 1) * BINS].iter().sum::<f32>() * hz_per_bin;
            assert!((area - 1.0).abs() < 0.1, "filter {m} area {area}");
        }
    }

    #[test]
    fn parakeet_mel_shape_normalisation_and_mask() {
        let mel = NemoMel::new(80);
        let x: Vec<f32> = (0..16_037)
            .map(|i| ((i as f32) * 0.05).sin() * 0.3)
            .collect();
        let out = mel.compute(&x);
        let (frames, valid) = NemoMel::frames(x.len());
        // NeMo `get_seq_len`: (len + 2 * 256 - 512) / 160, no + 1 (features.py on main).
        assert_eq!((frames, valid), (101, 100));
        assert_eq!(NemoMel::frames(70_559), (441, 440));
        assert_eq!(out.len(), 80 * frames);
        for m in [0, 20, 79] {
            let row = &out[m * frames..(m + 1) * frames];
            let mean = row[..valid].iter().sum::<f32>() / valid as f32;
            assert!(mean.abs() < 1e-3, "bin {m} mean {mean}");
            assert_eq!(row[valid], 0.0, "frames past the length are zero");
        }
        assert!(out.iter().all(|v| v.is_finite()));
    }
}
