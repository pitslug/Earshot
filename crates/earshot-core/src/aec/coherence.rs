//! Double-talk statistic independent of the linear echo estimate (T1.5d).
//!
//! Normalised cross-correlation detector of J. Benesty, D. R. Morgan, J. H. Cho, "A new
//! class of doubletalk detectors based on cross-correlation", IEEE Trans. Speech Audio
//! Process. 8(2), 2000, in its frequency-domain form (the fraction of the mic power that a
//! linear filter of the far-end reference can explain, i.e. the power-weighted multiple
//! coherence; see also T. Gänsler, M. Hansson, C.-J. Ivarsson, G. Salomonsson, "A
//! double-talk detector based on coherence", IEEE Trans. Commun. 44(11), 1996):
//!
//! ```text
//! xi^2 = sum_k s_xd(k)^H S_xx(k)^-1 s_xd(k) / sum_k S_dd(k)
//! ```
//!
//! with recursively averaged auto- and cross-spectra of the delay-aligned reference
//! channels `x` (one, or mid and side for a stereo reference) and the mic `d`. With only
//! the far end talking the mic is (up to loudspeaker distortion, noise and reverberation
//! beyond the analysis frame) a linear function of the reference and `xi^2` is near 1;
//! near-end speech adds power no filter of the reference explains and lowers it. Unlike the
//! detector on the linear stage's output, it needs no converged echo canceller: the spectra
//! estimate the echo path implicitly.

use std::sync::Arc;

use rustfft::num_complex::Complex;
use rustfft::{Fft, FftPlanner};

use super::linear::BLOCK;
use super::{ConfigError, require, within};

/// Analysis frame (samples, Hann window for mic and references alike).
pub const FRAME: usize = 8 * BLOCK;
const BINS: usize = FRAME / 2 + 1;

/// Settings of the statistic.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CoherenceConfig {
    /// Weight of the old spectra per block (exponential averaging).
    pub smoothing: f32,
    /// Analysed band (Hz).
    pub band_hz: (f32, f32),
    /// Sample rate (Hz).
    pub rate_hz: f32,
}

impl CoherenceConfig {
    /// Checks every setting.
    ///
    /// # Errors
    /// [`ConfigError`] naming the first setting out of range.
    pub fn validate(&self) -> Result<(), ConfigError> {
        require(
            within(f64::from(self.smoothing), 0.0, 0.999),
            "coherence smoothing must be in [0, 0.999]",
        )?;
        let (lo, hi) = (f64::from(self.band_hz.0), f64::from(self.band_hz.1));
        require(
            within(f64::from(self.rate_hz), 1.0, 1e6)
                && within(lo, 0.0, hi)
                && within(hi, lo, f64::from(self.rate_hz) / 2.0)
                && hi > lo,
            "coherence band must satisfy 0 <= low < high <= rate_hz / 2",
        )
    }
}

impl Default for CoherenceConfig {
    fn default() -> Self {
        Self {
            smoothing: 0.9,
            band_hz: (200.0, 6_000.0),
            rate_hz: 16_000.0,
        }
    }
}

/// Streaming `xi^2`; see the module docs.
pub struct Coherence {
    cfg: CoherenceConfig,
    fft: Arc<dyn Fft<f32>>,
    window: Vec<f32>,
    hist: [Vec<f32>; 3],
    buf: Vec<Complex<f32>>,
    /// Smoothed spectra: S_dd, S_mm, S_ss, S_ms, s_md, s_sd.
    dd: Vec<f32>,
    mm: Vec<f32>,
    ss: Vec<f32>,
    ms: Vec<Complex<f32>>,
    md: Vec<Complex<f32>>,
    sd: Vec<Complex<f32>>,
    /// This block's spectra of mic, mid and side (allocated once).
    spec: [Vec<Complex<f32>>; 3],
    xi2: f32,
}

impl std::fmt::Debug for Coherence {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Coherence")
            .field("cfg", &self.cfg)
            .field("xi2", &self.xi2)
            .finish_non_exhaustive()
    }
}

impl Coherence {
    /// A statistic with empty history.
    #[must_use]
    pub fn new(cfg: CoherenceConfig) -> Self {
        let mut planner = FftPlanner::<f32>::new();
        let window = (0..FRAME)
            .map(|i| 0.5 - 0.5 * (2.0 * std::f32::consts::PI * i as f32 / FRAME as f32).cos())
            .collect();
        let zero = Complex::new(0.0, 0.0);
        Self {
            cfg,
            fft: planner.plan_fft_forward(FRAME),
            window,
            hist: [vec![0.0; FRAME], vec![0.0; FRAME], vec![0.0; FRAME]],
            buf: vec![zero; FRAME],
            dd: vec![0.0; BINS],
            mm: vec![0.0; BINS],
            ss: vec![0.0; BINS],
            ms: vec![zero; BINS],
            md: vec![zero; BINS],
            sd: vec![zero; BINS],
            spec: [vec![zero; BINS], vec![zero; BINS], vec![zero; BINS]],
            xi2: 1.0,
        }
    }

    /// Forgets all history (for example after a realignment).
    pub fn reset(&mut self) {
        *self = Self::new(self.cfg);
    }

    /// The last value of `xi^2` (1 before any far-end signal).
    #[must_use]
    pub fn xi2(&self) -> f32 {
        self.xi2
    }

    /// Spectrum of history `which` (mic, mid, side) into `self.spec[which]`.
    fn spectrum(&mut self, which: usize) {
        for ((b, &v), &w) in self.buf.iter_mut().zip(&self.hist[which]).zip(&self.window) {
            *b = Complex::new(v * w, 0.0);
        }
        self.fft.process(&mut self.buf);
        self.spec[which].copy_from_slice(&self.buf[..BINS]);
    }

    /// Adds one block of mic, aligned mid reference and aligned side reference (zeros for
    /// a mono reference) and returns the updated `xi^2`.
    pub fn push(&mut self, mic: &[f32; BLOCK], mid: &[f32; BLOCK], side: &[f32; BLOCK]) -> f32 {
        for (h, x) in self.hist.iter_mut().zip([mic, mid, side]) {
            h.copy_within(BLOCK.., 0);
            h[FRAME - BLOCK..].copy_from_slice(x);
        }
        let stereo = self.hist[2].iter().any(|&v| v != 0.0) || self.ss.iter().any(|&v| v > 0.0);
        self.spectrum(0);
        self.spectrum(1);
        if stereo {
            self.spectrum(2);
        }
        let [d, m, s] = &self.spec;
        let a = self.cfg.smoothing;
        let b = 1.0 - a;
        let bin_hz = self.cfg.rate_hz / FRAME as f32;
        let lo = (self.cfg.band_hz.0 / bin_hz) as usize;
        let hi = ((self.cfg.band_hz.1 / bin_hz) as usize).min(BINS - 1);
        let (mut num, mut den) = (0.0f32, 0.0f32);
        for k in 0..BINS {
            self.dd[k] = a * self.dd[k] + b * d[k].norm_sqr();
            self.mm[k] = a * self.mm[k] + b * m[k].norm_sqr();
            self.md[k] = a * self.md[k] + b * m[k].conj() * d[k];
            if stereo {
                self.ss[k] = a * self.ss[k] + b * s[k].norm_sqr();
                self.ms[k] = a * self.ms[k] + b * m[k].conj() * s[k];
                self.sd[k] = a * self.sd[k] + b * s[k].conj() * d[k];
            }
            if k < lo || k > hi {
                continue;
            }
            // Explained power s^H S^-1 s, regularised so a silent reference explains
            // nothing.
            // Diagonal loading (Tikhonov) keeps the 2x2 inverse well defined when only one
            // loudspeaker channel plays (mid and side then coincide); the quadratic form
            // must use the loaded matrix exactly, a clamped determinant would cancel it.
            let reg = 1e-3 * (self.mm[k] + self.ss[k]) + 1e-12;
            let explained = if stereo {
                let (p, q, c) = (self.mm[k] + reg, self.ss[k] + reg, self.ms[k]);
                let det = p * q - c.norm_sqr();
                let (u, v) = (self.md[k], self.sd[k]);
                // [u v]^H [[p c][c* q]]^-1 [u v] with the inverse (1/det)[[q -c][-c* p]].
                ((q * u.norm_sqr() + p * v.norm_sqr()) - 2.0 * (u.conj() * c * v).re) / det
            } else {
                self.md[k].norm_sqr() / (self.mm[k] + reg)
            };
            num += explained.min(self.dd[k]);
            den += self.dd[k];
        }
        self.xi2 = if den > 1e-12 { num / den } else { 1.0 };
        self.xi2
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn noise(seed: &mut u64) -> f32 {
        *seed ^= *seed << 13;
        *seed ^= *seed >> 7;
        *seed ^= *seed << 17;
        ((*seed >> 11) as f64 / (1u64 << 53) as f64 - 0.5) as f32
    }

    #[test]
    fn aec_coherence_separates_echo_from_near_end_speech() {
        // Mic = filtered reference (an echo): xi^2 near 1; plus an independent talker of
        // equal power: about 0.5.
        let mut seed = 3u64;
        let x: Vec<f32> = (0..64 * BLOCK).map(|_| noise(&mut seed)).collect();
        let near: Vec<f32> = (0..64 * BLOCK).map(|_| noise(&mut seed)).collect();
        let echo: Vec<f32> = (0..x.len())
            .map(|i| 0.6 * x[i] + if i >= 7 { 0.3 * x[i - 7] } else { 0.0 })
            .collect();
        let run = |mic: &[f32]| {
            let mut c = Coherence::new(CoherenceConfig::default());
            let mut last = 0.0;
            for (m, r) in mic
                .as_chunks::<BLOCK>()
                .0
                .iter()
                .zip(x.as_chunks::<BLOCK>().0)
            {
                last = c.push(m, r, &[0.0; BLOCK]);
            }
            last
        };
        let only_echo = run(&echo);
        let p = |v: &[f32]| v.iter().map(|a| a * a).sum::<f32>();
        let g = (p(&echo) / p(&near)).sqrt();
        let both: Vec<f32> = echo.iter().zip(&near).map(|(e, n)| e + g * n).collect();
        let double = run(&both);
        assert!(only_echo > 0.9, "echo only: {only_echo}");
        assert!((double - 0.5).abs() < 0.15, "double talk: {double}");
    }

    #[test]
    fn aec_coherence_explains_a_two_channel_echo() {
        // Two independent channels through different paths: the stereo statistic explains
        // the echo, the mid channel alone does not.
        let mut seed = 5u64;
        let l: Vec<f32> = (0..64 * BLOCK).map(|_| noise(&mut seed)).collect();
        let r: Vec<f32> = (0..64 * BLOCK).map(|_| noise(&mut seed)).collect();
        let mic: Vec<f32> = (0..l.len())
            .map(|i| 0.5 * l[i] + if i >= 20 { 0.4 * r[i - 20] } else { 0.0 })
            .collect();
        let mid: Vec<f32> = l.iter().zip(&r).map(|(a, b)| 0.5 * (a + b)).collect();
        let side: Vec<f32> = l.iter().zip(&r).map(|(a, b)| 0.5 * (a - b)).collect();
        let mut st = Coherence::new(CoherenceConfig::default());
        let mut mono = Coherence::new(CoherenceConfig::default());
        let (mut xs, mut xm) = (0.0, 0.0);
        for ((d, m), s) in mic
            .as_chunks::<BLOCK>()
            .0
            .iter()
            .zip(mid.as_chunks::<BLOCK>().0)
            .zip(side.as_chunks::<BLOCK>().0)
        {
            xs = st.push(d, m, s);
            xm = mono.push(d, m, &[0.0; BLOCK]);
        }
        assert!(xs > 0.9, "stereo: {xs}");
        assert!(xm < 0.8, "mid only: {xm}");
    }
}
