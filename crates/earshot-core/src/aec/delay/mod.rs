//! Bulk echo delay and clock drift estimation (PLAN.md T1.3).
//!
//! Delay: generalised cross-correlation with phase transform (GCC-PHAT; C. H. Knapp,
//! G. C. Carter, "The generalized correlation method for estimation of time delay", IEEE
//! TASSP 24(4), 1976). Every hop the last `frame` mic samples are correlated against the
//! last `frame + max_delay` reference samples; the per-frame PHAT-normalised cross-spectra
//! are exponentially averaged (only while both signals are active) and the peak of the
//! averaged correlation over lags `0..=max_delay` is the candidate delay, refined with a
//! parabolic fit. The peak-to-RMS ratio of the correlation is the confidence.
//!
//! Tracking: a candidate must repeat (within a tolerance) before it is trusted, both to
//! acquire lock and to jump to a new delay; confidence below the release threshold for
//! long enough drops the lock (echo gone, for example after a switch to headphones).
//!
//! Drift: least-squares slope of the tracked delay over a sliding window while the lock is
//! continuous. Positive ppm means the render clock runs fast, so the delay shrinks.

use std::sync::Arc;

use rustfft::num_complex::Complex;
use rustfft::{Fft, FftPlanner};

use super::{ConfigError, require, within};
use crate::audio::RingBuffer;

#[cfg(test)]
mod tests;

/// Welch segment of the candidate verification (samples).
const VERIFY_LEN: usize = 512;

/// A lock that moves by at most this much (s) keeps its drift history (see `lock_at`).
const DRIFT_KEEP_S: f64 = 0.01;

/// Estimator settings. [`Default`] suits 16 kHz audio with delays up to 600 ms.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DelayConfig {
    /// Sample rate (Hz).
    pub rate_hz: u32,
    /// Largest delay searched (samples).
    pub max_delay: usize,
    /// FFT size; the mic frame is `fft_len - max_delay` samples.
    pub fft_len: usize,
    /// Samples between estimates.
    pub hop: usize,
    /// Weight of the old average per active hop (exponential smoothing).
    pub smoothing: f32,
    /// Peak-to-RMS ratio needed to accept a candidate.
    pub lock_confidence: f64,
    /// Peak-to-RMS ratio below which a locked estimator counts towards losing lock.
    pub release_confidence: f64,
    /// Consecutive agreeing candidates needed to lock.
    pub confirmations: u32,
    /// Consecutive agreeing candidates needed to jump away from an existing lock.
    pub jump_confirmations: u32,
    /// Candidates within this many samples agree.
    pub agree_tolerance: f64,
    /// Coherence (see [`Candidate::coherence`]) a strong candidate needs to count at all
    /// (to lock or to jump).
    pub verify_coherence: f64,
    /// Active hops with low confidence before the lock is dropped.
    pub release_hops: u32,
    /// Reference frame power below this (linear, mean square) counts as silent.
    pub ref_floor: f64,
    /// Mic frame power below this counts as silent.
    pub mic_floor: f64,
    /// Hops whose mic power is more than this many dB below the recent mic peak (a gap
    /// holding only the decaying room tail) give no evidence.
    pub mic_gate_db: f64,
    /// Per-hop decay of the recent mic peak (linear power factor).
    pub mic_peak_decay: f64,
    /// Drift regression window (seconds).
    pub drift_window_s: f64,
    /// Shortest locked span (seconds) before drift is reported.
    pub drift_min_span_s: f64,
}

impl DelayConfig {
    /// Checks every setting (the constructor panics on what this rejects).
    ///
    /// # Errors
    /// [`ConfigError`] naming the first setting out of range.
    pub fn validate(&self) -> Result<(), ConfigError> {
        require(
            self.rate_hz > 0 && self.hop > 0,
            "rate_hz and hop must be > 0",
        )?;
        require(
            self.fft_len > self.max_delay,
            "fft_len must exceed max_delay",
        )?;
        require(
            self.fft_len - self.max_delay >= VERIFY_LEN,
            "the mic frame (fft_len - max_delay) must hold one verification segment",
        )?;
        require(
            within(f64::from(self.smoothing), 0.0, 0.999),
            "smoothing must be in [0, 0.999]",
        )?;
        require(
            within(self.release_confidence, 0.0, f64::MAX)
                && within(self.lock_confidence, self.release_confidence, f64::MAX),
            "need 0 <= release_confidence <= lock_confidence",
        )?;
        require(
            self.confirmations >= 1 && self.jump_confirmations >= 1 && self.release_hops >= 1,
            "confirmations, jump_confirmations and release_hops must be >= 1",
        )?;
        require(
            within(self.agree_tolerance, 0.0, self.max_delay as f64),
            "agree_tolerance must be in [0, max_delay]",
        )?;
        require(
            within(self.verify_coherence, 0.0, 1.0),
            "verify_coherence must be in [0, 1]",
        )?;
        require(
            within(self.ref_floor, 0.0, f64::MAX) && within(self.mic_floor, 0.0, f64::MAX),
            "ref_floor and mic_floor must be finite and >= 0",
        )?;
        require(
            within(self.mic_gate_db, 0.0, 200.0),
            "mic_gate_db must be in [0, 200]",
        )?;
        require(
            within(self.mic_peak_decay, f64::MIN_POSITIVE, 1.0),
            "mic_peak_decay must be in (0, 1]",
        )?;
        require(
            within(self.drift_window_s, f64::MIN_POSITIVE, 3_600.0)
                && within(self.drift_min_span_s, 0.0, self.drift_window_s),
            "need drift_window_s in (0, 3600] and drift_min_span_s in [0, drift_window_s]",
        )
    }
}

impl Default for DelayConfig {
    fn default() -> Self {
        Self {
            rate_hz: 16_000,
            max_delay: 9_600,
            fft_len: 16_384,
            hop: 2_000,
            smoothing: 0.7,
            lock_confidence: 9.0,
            release_confidence: 6.0,
            confirmations: 2,
            jump_confirmations: 3,
            agree_tolerance: 8.0,
            verify_coherence: 0.3,
            release_hops: 12,
            ref_floor: 1e-6,
            mic_floor: 1e-8,
            mic_gate_db: 15.0,
            mic_peak_decay: 0.98,
            drift_window_s: 20.0,
            drift_min_span_s: 2.0,
        }
    }
}

/// What the estimator currently believes.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum DelayState {
    /// No echo path found; the canceller must not adapt.
    NoLock,
    /// No live lock yet, but a warm-start hint (for example from the audio check) seeds the
    /// search. Not evidence of an echo path: cancellers treat it as no lock. Live
    /// tracking replaces it as soon as it locks.
    Hint {
        /// Hinted delay (samples).
        delay: f64,
    },
    /// Tracking an echo path.
    Locked {
        /// Delay of the echo's main peak behind the reference (samples).
        delay: f64,
        /// Peak-to-RMS ratio of the last accepted estimate.
        confidence: f64,
    },
}

impl DelayState {
    /// The delay to use, if any (hinted or locked).
    #[must_use]
    pub fn delay(&self) -> Option<f64> {
        match *self {
            Self::NoLock => None,
            Self::Hint { delay } | Self::Locked { delay, .. } => Some(delay),
        }
    }

    /// True when live tracking has locked.
    #[must_use]
    pub fn is_locked(&self) -> bool {
        matches!(self, Self::Locked { .. })
    }
}

/// One GCC-PHAT measurement.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Candidate {
    /// Mic sample count when it was taken.
    pub at: u64,
    /// Peak lag (samples, sub-sample refined).
    pub delay: f64,
    /// Peak-to-RMS ratio.
    pub confidence: f64,
    /// Magnitude-squared coherence (Welch, power weighted, 200 Hz - 6 kHz) of the mic frame
    /// with the reference read at this candidate's delay: about 1 when an echo path at this
    /// delay explains the mic, about the estimator bias (well under 0.2) for unrelated
    /// signals. See [`DelayEstimator`] lock rules.
    pub coherence: f64,
}

/// Streaming delay and drift estimator; see the module docs.
pub struct DelayEstimator {
    cfg: DelayConfig,
    fft: Arc<dyn Fft<f32>>,
    ifft: Arc<dyn Fft<f32>>,
    /// Welch FFT for the candidate verification.
    vfft: Arc<dyn Fft<f32>>,
    vbuf: Vec<Complex<f32>>,
    mic_hist: RingBuffer,
    ref_hist: RingBuffer,
    mic_buf: Vec<Complex<f32>>,
    ref_buf: Vec<Complex<f32>>,
    avg: Vec<Complex<f32>>,
    scratch_mic: Vec<f32>,
    scratch_ref: Vec<f32>,
    since_hop: usize,
    samples: u64,
    state: DelayState,
    hint: Option<f64>,
    pending: Option<(f64, u32)>,
    low_hops: u32,
    mic_peak: f64,
    epoch: u64,
    /// Drift history: (sample, delay, lock segment); see `lock_at`.
    track: Vec<(f64, f64, u32)>,
    segment: u32,
    last: Option<Candidate>,
}

impl std::fmt::Debug for DelayEstimator {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DelayEstimator")
            .field("cfg", &self.cfg)
            .field("state", &self.state)
            .field("epoch", &self.epoch)
            .finish_non_exhaustive()
    }
}

impl DelayEstimator {
    /// Creates an estimator.
    ///
    /// # Panics
    /// If [`DelayConfig::validate`] rejects `cfg`.
    #[must_use]
    pub fn new(cfg: DelayConfig) -> Self {
        if let Err(e) = cfg.validate() {
            panic!("{e}");
        }
        let mut planner = FftPlanner::<f32>::new();
        let fft = planner.plan_fft_forward(cfg.fft_len);
        let ifft = planner.plan_fft_inverse(cfg.fft_len);
        let vfft = planner.plan_fft_forward(VERIFY_LEN);
        let frame = cfg.fft_len - cfg.max_delay;
        let zero = Complex::new(0.0, 0.0);
        // Histories start full of silence so estimates begin after the first hop.
        let mut mic_hist = RingBuffer::new(frame);
        mic_hist.push(&vec![0.0; frame]);
        let mut ref_hist = RingBuffer::new(cfg.fft_len);
        ref_hist.push(&vec![0.0; cfg.fft_len]);
        Self {
            cfg,
            fft,
            ifft,
            vfft,
            vbuf: vec![zero; VERIFY_LEN],
            mic_hist,
            ref_hist,
            mic_buf: vec![zero; cfg.fft_len],
            ref_buf: vec![zero; cfg.fft_len],
            avg: vec![zero; cfg.fft_len],
            scratch_mic: vec![0.0; frame],
            scratch_ref: vec![0.0; cfg.fft_len],
            since_hop: 0,
            samples: 0,
            state: DelayState::NoLock,
            hint: None,
            pending: None,
            low_hops: 0,
            mic_peak: 0.0,
            epoch: 0,
            track: Vec::with_capacity(
                (cfg.drift_window_s * f64::from(cfg.rate_hz)) as usize / cfg.hop + 4,
            ),
            segment: 0,
            last: None,
        }
    }

    /// Sets a warm-start delay hint (samples), for example from the audio check. It only
    /// seeds the search (a live candidate agreeing with it locks after one confirmation)
    /// and is reported as [`DelayState::Hint`] until live tracking locks, which always
    /// overrides it. A hint never makes a canceller cancel; it is discarded when a lock is
    /// released. Ignored while locked.
    pub fn set_hint(&mut self, delay: f64) {
        self.hint = Some(delay);
        if !self.state.is_locked() {
            self.state = DelayState::Hint { delay };
        }
    }

    /// Current state.
    #[must_use]
    pub fn state(&self) -> DelayState {
        self.state
    }

    /// Increments whenever the tracked delay is (re)acquired or jumps, so a canceller can
    /// realign its filter.
    #[must_use]
    pub fn epoch(&self) -> u64 {
        self.epoch
    }

    /// The last raw GCC-PHAT measurement.
    #[must_use]
    pub fn last_candidate(&self) -> Option<Candidate> {
        self.last
    }

    /// Mic samples consumed so far.
    #[must_use]
    pub fn samples(&self) -> u64 {
        self.samples
    }

    /// Estimated render-vs-capture drift (ppm, positive = render fast) once the lock has
    /// lasted [`DelayConfig::drift_min_span_s`]; `None` before.
    #[must_use]
    pub fn drift_ppm(&self) -> Option<f64> {
        let (first, last) = (self.track.first()?, self.track.last()?);
        let span = (last.0 - first.0) / f64::from(self.cfg.rate_hz);
        if span < self.cfg.drift_min_span_s || self.track.len() < 3 {
            return None;
        }
        // Pooled within-segment regression: one common slope, a separate offset per lock
        // segment (the paths of one echo differ by a constant delay).
        let (mut sxy, mut sxx) = (0.0, 0.0);
        for seg in self.track.chunk_by(|a, b| a.2 == b.2) {
            let n = seg.len() as f64;
            let mt = seg.iter().map(|p| p.0).sum::<f64>() / n;
            let md = seg.iter().map(|p| p.1).sum::<f64>() / n;
            for &(t, d, _) in seg {
                sxy += (t - mt) * (d - md);
                sxx += (t - mt) * (t - mt);
            }
        }
        if sxx <= 0.0 {
            return None;
        }
        Some(-sxy / sxx * 1e6)
    }

    /// Feeds equal-length mic and reference chunks (same clock, same sample index).
    /// Returns true if the state changed during this call.
    ///
    /// # Panics
    /// If the lengths differ.
    pub fn push(&mut self, mic: &[f32], reference: &[f32]) -> bool {
        assert_eq!(
            mic.len(),
            reference.len(),
            "mic and reference lengths differ"
        );
        let before = self.state;
        let mut pos = 0;
        while pos < mic.len() {
            let take = (self.cfg.hop - self.since_hop).min(mic.len() - pos);
            self.mic_hist.push_overwrite(&mic[pos..pos + take]);
            self.ref_hist.push_overwrite(&reference[pos..pos + take]);
            self.since_hop += take;
            self.samples += take as u64;
            pos += take;
            if self.since_hop == self.cfg.hop {
                self.since_hop = 0;
                {
                    self.estimate();
                }
            }
        }
        before != self.state
    }

    fn estimate(&mut self) {
        let n = self.cfg.fft_len;
        let frame = n - self.cfg.max_delay;
        self.mic_hist.peek(&mut self.scratch_mic);
        self.ref_hist.peek(&mut self.scratch_ref);
        let p_mic = mean_square(&self.scratch_mic);
        // Reference power over the part that can reach this mic frame.
        let p_ref = mean_square(&self.scratch_ref);
        if !(p_mic.is_finite() && p_ref.is_finite()) {
            // A non-finite sample in either window: no evidence, and it must not reach
            // the recent mic peak (an infinite peak would gate every later hop).
            return;
        }
        let ref_active = p_ref > self.cfg.ref_floor;
        self.mic_peak = p_mic.max(self.mic_peak * self.cfg.mic_peak_decay);
        let mic_gated = p_mic < self.mic_peak * 10f64.powf(-self.cfg.mic_gate_db / 10.0);
        if ref_active && p_mic > self.cfg.mic_floor && mic_gated {
            // Tail-only frame: no evidence either way.
            return;
        }
        if !ref_active || p_mic <= self.cfg.mic_floor {
            // No new evidence, except that a silent mic while the reference plays says
            // there is no echo path.
            if ref_active && self.state.is_locked() {
                self.low_confidence_hop();
            }
            return;
        }
        {
            let zero = Complex::new(0.0, 0.0);
            for (i, c) in self.mic_buf.iter_mut().enumerate() {
                *c = if i < frame {
                    Complex::new(self.scratch_mic[i], 0.0)
                } else {
                    zero
                };
            }
            for (c, &v) in self.ref_buf.iter_mut().zip(&self.scratch_ref) {
                *c = Complex::new(v, 0.0);
            }
            self.fft.process(&mut self.mic_buf);
            self.fft.process(&mut self.ref_buf);
            // Band 100 Hz - 7 kHz; PHAT weighting per frame, then exponential averaging.
            let bin_hz = self.cfg.rate_hz as f32 / n as f32;
            let lo = (100.0 / bin_hz) as usize;
            let hi = ((7_000.0 / bin_hz) as usize).min(n / 2);
            let a = self.cfg.smoothing;
            for k in 0..n {
                let kk = if k <= n / 2 { k } else { n - k };
                let x = if (lo..=hi).contains(&kk) {
                    let c = self.ref_buf[k] * self.mic_buf[k].conj();
                    let m = c.norm();
                    if m > 1e-20 { c / m } else { zero }
                } else {
                    zero
                };
                self.avg[k] = self.avg[k] * a + x * (1.0 - a);
            }
        }

        // Correlation: c'[k] = sum ref_win[j + k] mic_win[j], peak at k = max_delay - D.
        self.ref_buf.copy_from_slice(&self.avg);
        self.ifft.process(&mut self.ref_buf);
        let maxd = self.cfg.max_delay;
        let corr = |d: usize| self.ref_buf[maxd - d].re;
        // Only interior lags can be peaks: a maximum on the search boundary is a window-edge
        // artefact (seen when both signals fade out together), never an echo path.
        let (mut best, mut best_v, mut sum_sq) = (1usize, f32::MIN, 0.0f64);
        for d in 0..=maxd {
            let v = corr(d);
            sum_sq += f64::from(v) * f64::from(v);
            if v > best_v && d > 0 && d < maxd {
                best_v = v;
                best = d;
            }
        }
        let rms = (sum_sq / (maxd + 1) as f64).sqrt();
        let confidence = if rms > 0.0 {
            f64::from(best_v) / rms
        } else {
            0.0
        };
        let mut delay = best as f64;
        if best > 0 && best < maxd {
            let (ym, y0, yp) = (
                f64::from(corr(best - 1)),
                f64::from(corr(best)),
                f64::from(corr(best + 1)),
            );
            let den = ym - 2.0 * y0 + yp;
            if den.abs() > 1e-20 {
                delay += (0.5 * (ym - yp) / den).clamp(-0.5, 0.5);
            }
        }
        let coherence = self.verify(delay);
        let cand = Candidate {
            at: self.samples,
            delay,
            confidence,
            coherence,
        };
        self.last = Some(cand);
        self.track_candidate(cand);
    }

    /// Welch magnitude-squared coherence of the mic frame and the reference at `delay`
    /// (power weighted over 200 Hz - 6 kHz); uses the frames of the current hop.
    fn verify(&mut self, delay: f64) -> f64 {
        let n = self.cfg.fft_len;
        let frame = n - self.cfg.max_delay;
        let d = delay.round().clamp(0.0, self.cfg.max_delay as f64) as usize;
        // Mic sample i of the frame (time now - frame + i) meets reference index
        // n - frame + i - d of the reference window.
        let off = n - frame - d;
        let bins = VERIFY_LEN / 2 + 1;
        let bin_hz = self.cfg.rate_hz as f64 / VERIFY_LEN as f64;
        let lo = (200.0 / bin_hz) as usize;
        let hi = ((6_000.0 / bin_hz) as usize).min(bins - 1);
        let mut sxx = vec![0.0f64; bins];
        let mut syy = vec![0.0f64; bins];
        let mut sxy = vec![Complex::new(0.0f64, 0.0); bins];
        let hann = |i: usize| {
            0.5 - 0.5 * (2.0 * std::f64::consts::PI * i as f64 / VERIFY_LEN as f64).cos()
        };
        let mut start = 0;
        while start + VERIFY_LEN <= frame {
            for (i, b) in self.vbuf.iter_mut().enumerate() {
                *b = Complex::new(
                    (f64::from(self.scratch_mic[start + i]) * hann(i)) as f32,
                    0.0,
                );
            }
            self.vfft.process(&mut self.vbuf);
            let y: Vec<Complex<f32>> = self.vbuf[..bins].to_vec();
            for (i, b) in self.vbuf.iter_mut().enumerate() {
                *b = Complex::new(
                    (f64::from(self.scratch_ref[off + start + i]) * hann(i)) as f32,
                    0.0,
                );
            }
            self.vfft.process(&mut self.vbuf);
            for k in lo..=hi {
                let x = self.vbuf[k];
                let (xr, xi, yr, yi) = (
                    f64::from(x.re),
                    f64::from(x.im),
                    f64::from(y[k].re),
                    f64::from(y[k].im),
                );
                sxx[k] += xr * xr + xi * xi;
                syy[k] += yr * yr + yi * yi;
                // conj(x) y
                sxy[k] += Complex::new(xr * yr + xi * yi, xr * yi - xi * yr);
            }
            start += VERIFY_LEN / 2;
        }
        let (mut num, mut den) = (0.0f64, 0.0f64);
        for k in lo..=hi {
            if sxx[k] > 0.0 {
                num += sxy[k].norm_sqr() / sxx[k];
            }
            den += syy[k];
        }
        if den > 0.0 { (num / den).min(1.0) } else { 0.0 }
    }

    fn track_candidate(&mut self, c: Candidate) {
        let cfg = self.cfg;
        // A strong peak is only an echo path if the mic is coherent with the reference
        // at that delay: GCC-PHAT whitens, so unrelated speech on the two tracks can give
        // repeated strong peaks (T1.5e); unrelated signals have a coherence near the
        // estimator bias, a real echo path well above it.
        // A peak at the edge of the searched lags (within `agree_tolerance` of `max_delay`)
        // means the true delay may lie outside the window: never a lock (T6.17, a live
        // lock at exactly 600 ms).
        let at_edge = c.delay >= cfg.max_delay as f64 - cfg.agree_tolerance;
        let strong =
            c.confidence >= cfg.lock_confidence && c.coherence >= cfg.verify_coherence && !at_edge;
        match self.state {
            DelayState::Locked { delay, .. } => {
                let agrees = (c.delay - delay).abs() <= cfg.agree_tolerance;
                if agrees && c.confidence >= cfg.release_confidence {
                    self.low_hops = 0;
                    self.pending = None;
                    self.state = DelayState::Locked {
                        delay: c.delay,
                        confidence: c.confidence,
                    };
                    self.record(c);
                } else if !agrees && strong && self.confirm(c.delay, cfg.jump_confirmations) {
                    // Confirmed jump to a new path.
                    self.lock_at(c);
                } else {
                    // Weak, or pointing elsewhere without confirmation: evidence against
                    // the current lock.
                    self.low_confidence_hop();
                }
            }
            DelayState::NoLock | DelayState::Hint { .. } => {
                // A hint only seeds the search: a strong candidate that agrees with it
                // needs one confirmation instead of `confirmations`.
                let seeded = self
                    .hint
                    .is_some_and(|h| (c.delay - h).abs() <= cfg.agree_tolerance);
                let needed = if seeded { 1 } else { cfg.confirmations };
                if strong && self.confirm(c.delay, needed) {
                    self.lock_at(c);
                }
            }
        }
    }

    fn low_confidence_hop(&mut self) {
        self.low_hops += 1;
        if self.low_hops >= self.cfg.release_hops {
            self.drop_lock();
        }
    }

    /// Counts agreeing strong candidates; true once `needed` agree.
    fn confirm(&mut self, delay: f64, needed: u32) -> bool {
        let tol = self.cfg.agree_tolerance;
        let count = match self.pending {
            Some((d, k)) if (d - delay).abs() <= tol => k + 1,
            _ => 1,
        };
        self.pending = Some((delay, count));
        count >= needed
    }

    fn lock_at(&mut self, c: Candidate) {
        let step = match self.state {
            DelayState::Locked { delay, .. } => Some(c.delay - delay),
            _ => None,
        };
        self.state = DelayState::Locked {
            delay: c.delay,
            confidence: c.confidence,
        };
        self.pending = None;
        self.low_hops = 0;
        self.epoch += 1;
        match step {
            // A small move of the lock (to another path of the same echo, for example the
            // second of two loudspeakers) does not change the clocks: keep the drift
            // history as a new segment with its own offset (see `drift_ppm`).
            Some(s) if s.abs() <= DRIFT_KEEP_S * f64::from(self.cfg.rate_hz) => {
                self.segment = self.segment.wrapping_add(1);
            }
            _ => self.track.clear(),
        }
        self.record(c);
    }

    fn drop_lock(&mut self) {
        // Released while the reference was active (only active hops count towards
        // release): the echo path is gone, and so is any hint (AGENTS.md invariant).
        self.state = DelayState::NoLock;
        self.hint = None;
        self.pending = None;
        self.low_hops = 0;
        self.track.clear();
        // Forget the old path so a later echo is acquired from fresh evidence.
        let zero = Complex::new(0.0, 0.0);
        self.avg.iter_mut().for_each(|v| *v = zero);
    }

    fn record(&mut self, c: Candidate) {
        let t = c.at as f64;
        self.track.push((t, c.delay, self.segment));
        let window = self.cfg.drift_window_s * f64::from(self.cfg.rate_hz);
        let cut = self.track.partition_point(|p| p.0 < t - window);
        if cut > 0 {
            self.track.drain(..cut);
        }
    }
}

fn mean_square(x: &[f32]) -> f64 {
    if x.is_empty() {
        return 0.0;
    }
    x.iter().map(|&v| f64::from(v) * f64::from(v)).sum::<f64>() / x.len() as f64
}
