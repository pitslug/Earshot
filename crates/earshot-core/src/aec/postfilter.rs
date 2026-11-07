//! Residual echo suppressor: STFT postfilter with a nonlinear echo estimate (PLAN.md T1.5c).
//!
//! Runs after the zero-latency linear stage on frames of [`FRAME`] samples (square-root
//! Hann analysis and synthesis, hop [`BLOCK`], overlap-add), so it adds a fixed output
//! latency of [`LATENCY`] samples (16 ms at 16 kHz; PLAN.md 3.3 allows one fixed postfilter
//! latency of at most 32 ms). With unit gain the output is the input delayed by exactly
//! [`LATENCY`] samples (up to float rounding).
//!
//! Residual echo estimate per bin `k`, from the echo estimate `Y`, the aligned reference
//! `X` and the error `E` (linear output):
//!
//! - **linear misadjustment**: `eta |Y_k|^2`, the linear stage's leakage (Valin 2007, the
//!   residual Speex's preprocessor suppresses);
//! - **nonlinear echo**: `rho_k[q] |Y_k|^2`, a residual-to-echo-estimate ratio learned per
//!   reference-level class `q` (6 dB classes) while only the far end talks. Loudspeaker
//!   distortion grows with the drive level, so the ratio is a function of level; this is
//!   the level-dependent nonlinear residual echo model of A. Bendersky, J. Stokes,
//!   H. Malvar, "Nonlinear residual acoustic echo suppression for high levels of harmonic
//!   distortion", ICASSP 2008 (memoryless per-bin form), close to the spectral nonlinear
//!   echo estimate of O. Hoshuyama, A. Sugiyama, "Nonlinear echo suppression using a
//!   spectral-magnitude-based approach" (2006);
//! - **cold start**: `beta_k Xr_k`, the reference power with a reverberation tail times a
//!   learned echo-path gain, until the linear stage has cancelled once;
//! - **low-frequency products** (bins below 125 Hz, `NL_LF_HZ`, T1.6c): `gamma_k[q] |X3_k|^2`,
//!   with `X3` the spectrum of the cubed aligned reference `x^3 / ms` (`ms` its long-term
//!   mean square, so `x^3 / ms` keeps the reference's units) and `gamma` the ratio of the
//!   residual the other terms leave to it, learned per level class like `rho` while only
//!   the far end talks. A memoryless odd nonlinearity (loudspeaker clipping) of an
//!   asymmetric waveform (voiced speech) puts a product below the voice's fundamental,
//!   where the echo estimate and the reference carry almost nothing, so the in-bin terms
//!   cannot predict it; the cube is the third-order basis of a power filter (F. Kuech,
//!   W. Kellermann, "Orthogonalized power filters for nonlinear acoustic echo
//!   cancellation", Signal Processing 86(6), 2006), used here as a spectral predictor of
//!   the residual rather than as an adaptive filter. Measured on the dev-wide voices
//!   (T1.6c, softclip 3): this product dominated the far-end-only output on 6 of 8 voice
//!   pairs (median ERLE 10.9-18.6 dB, 34.8-43.5 dB once both signals were high-passed at
//!   120 Hz). Above that band the in-bin terms already cover the products and the cube
//!   only over-suppressed near-end speech in double talk (dev-wide, measured);
//!
//! combined as `max(...)` and given an exponential reverberation tail across frames (the
//! room smears distortion products in time).
//!
//! Gain: decision-directed Wiener (Y. Ephraim, D. Malah, "Speech enhancement using a
//! minimum mean-square error short-time spectral amplitude estimator", IEEE TASSP 32(6),
//! 1984), with the residual echo in place of the noise: `G = xi / (1 + xi)`, `xi` from the
//! previous frame's cleaned power and this frame's a-posteriori ratio, floored at
//! `min_gain`. Without echo evidence (the canceller decides) the gain is exactly one.

use super::{ConfigError, require, within};
use std::sync::Arc;

use rustfft::num_complex::Complex;
use rustfft::{Fft, FftPlanner};

use super::linear::BLOCK;

/// STFT frame length (samples).
pub const FRAME: usize = 3 * BLOCK;
/// Output latency added by the postfilter (samples).
pub const LATENCY: usize = FRAME - BLOCK;
const PBINS: usize = FRAME / 2 + 1;
/// Reference-level classes (6 dB wide from -60 dBFS up).
const LEVELS: usize = 9;
/// Frames of learning before a level class's own ratios are used.
const MIN_CLASS_FRAMES: u32 = 20;
/// Upper edge of the band the low-frequency product model covers (Hz): below the lowest
/// voice fundamentals, where clipping products of voiced speech have no in-bin predictor.
/// Chosen on the dev-wide set (T1.6c) by a rule fixed before measuring: of 125 Hz, 250 Hz
/// and every bin, the band with the largest summed double-talk SI-SDR over the 16 dev-wide
/// double-talk runs whose worst far-end-only ERLE (mono and stereo) stays within 3 dB of
/// the every-bin model's (sums 132.8, 128.6 and 107.7 dB; worst mono / stereo far-end-only
/// ERLE 35.1 / 19.2, 35.1 / 19.2 and 36.2 / 21.9 dB).
const NL_LF_HZ: usize = 125;
/// Bins whose centre frequency is below [`NL_LF_HZ`] at 16 kHz (0, 41.7 and 83.3 Hz).
const NL_LF_BINS: usize = (NL_LF_HZ * FRAME).div_ceil(16_000);
/// Largest learned low-frequency product ratio `gamma` (as the cold-start gain `beta`).
const NL_LF_MAX_RATIO: f32 = 4.0;
/// Smallest analysis-filter power response a residual estimate is divided by (-20 dB):
/// bins far below a high-pass cutoff carry almost nothing in the analysis, and their
/// mapped estimate must stay finite.
pub const MIN_ANALYSIS_RESPONSE: f32 = 0.01;
/// Floor of the reference mean square the cube is normalised by, so the cube spectrum stays
/// finite at the canceller's input limit (1e3) after a quiet stretch.
const NL_LF_MIN_MS: f32 = 1e-6;

/// Postfilter settings.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PostfilterConfig {
    /// Scale of the residual estimate outside double talk.
    pub overdrive: f32,
    /// Scale of the residual estimate during double talk.
    pub doubletalk_overdrive: f32,
    /// Smallest gain (amplitude).
    pub min_gain: f32,
    /// Weight of the previous frame in the decision-directed a-priori ratio.
    pub dd_alpha: f32,
    /// Per-frame decay of the residual's reverberation tail (power).
    pub reverb_decay: f32,
    /// Initial echo-path gain of the cold-start model.
    pub beta_init: f32,
    /// Comfort noise seed; `None` disables comfort noise.
    pub comfort_noise: Option<u64>,
    /// Include the linear stage's leakage in the residual estimate.
    pub use_leak: bool,
    /// Gain rule: decision-directed Wiener (false) or `max(1 - R / E, floor)` (true).
    pub subtractive: bool,
}

impl PostfilterConfig {
    /// Checks every setting.
    ///
    /// # Errors
    /// [`ConfigError`] naming the first setting out of range.
    pub fn validate(&self) -> Result<(), ConfigError> {
        require(
            within(f64::from(self.overdrive), 0.0, 100.0)
                && within(f64::from(self.doubletalk_overdrive), 0.0, 100.0),
            "overdrive and doubletalk_overdrive must be in [0, 100]",
        )?;
        require(
            within(f64::from(self.min_gain), 0.0, 1.0),
            "min_gain must be in [0, 1]",
        )?;
        require(
            within(f64::from(self.dd_alpha), 0.0, 0.999)
                && within(f64::from(self.reverb_decay), 0.0, 0.999),
            "dd_alpha and reverb_decay must be in [0, 0.999]",
        )?;
        require(
            within(f64::from(self.beta_init), 0.0, 4.0),
            "beta_init must be in [0, 4]",
        )
    }
}

impl Default for PostfilterConfig {
    fn default() -> Self {
        Self {
            overdrive: 1.5,
            doubletalk_overdrive: 1.0,
            min_gain: 0.03,
            dd_alpha: 0.98,
            reverb_decay: 0.8,
            beta_init: 1.0,
            comfort_noise: None,
            use_leak: true,
            subtractive: false,
        }
    }
}

/// What the canceller tells the postfilter about one block.
#[derive(Debug, Clone, Copy)]
pub struct Frame<'a> {
    /// Signal to clean: the linear output, or the mic itself (bypass, no echo path).
    pub base: &'a [f32; BLOCK],
    /// The raw mic block.
    pub mic: &'a [f32; BLOCK],
    /// The base switched to the mic in this block: analyse the mic's history from now on,
    /// so earlier linear output (which may hold a stale echo estimate) is never released
    /// at unit gain.
    pub to_mic: bool,
    /// Linear echo estimate.
    pub echo: &'a [f32; BLOCK],
    /// Delay-aligned reference.
    pub aligned: &'a [f32; BLOCK],
    /// Linear stage's leakage estimate.
    pub leak: f32,
    /// Suppression allowed (live echo evidence, no bypass).
    pub suppress: bool,
    /// Only the far end talks and the linear stage has converged: learn the ratios.
    pub learn: bool,
    /// Cold phase: use (and, with `learn_cold`, learn) the reference-based model.
    pub cold: bool,
    /// Learn the cold-start model in this frame.
    pub learn_cold: bool,
    /// Double talk detected.
    pub doubletalk: bool,
    /// Weight of the reference-based model in double talk this frame (0: leakage and
    /// nonlinear terms only).
    pub doubletalk_ref_weight: f32,
    /// Separate analysis signals `(base, mic)` (internal high-pass, T6.23b): the echo
    /// models are learned and the residual echo estimated from these (and the filtered
    /// `echo` and `aligned`); the estimate is mapped to `base`'s band by the filter's power
    /// response ([`Postfilter::set_analysis_response`]) and the gains are computed from and
    /// applied to `base`. `None` analyses `base` and `mic` themselves.
    pub analysis: Option<(&'a [f32; BLOCK], &'a [f32; BLOCK])>,
}

/// Streaming STFT residual echo suppressor; see the module docs.
pub struct Postfilter {
    cfg: PostfilterConfig,
    fft: Arc<dyn Fft<f32>>,
    ifft: Arc<dyn Fft<f32>>,
    window: [f32; FRAME],
    hist_base: [f32; FRAME],
    hist_mic: [f32; FRAME],
    /// Analysis histories (equal to `hist_base` / `hist_mic` without a separate analysis).
    hist_abase: [f32; FRAME],
    hist_amic: [f32; FRAME],
    hist_echo: [f32; FRAME],
    hist_ref: [f32; FRAME],
    ola: [f32; FRAME],
    buf: Vec<Complex<f32>>,
    spec: Vec<Complex<f32>>,
    rho: [[f32; PBINS]; LEVELS],
    rho_frames: [u32; LEVELS],
    rho_all: [f32; PBINS],
    /// Low-frequency product ratios per level class and their class-independent ratio.
    gamma: [[f32; NL_LF_BINS]; LEVELS],
    gamma_all: [f32; NL_LF_BINS],
    /// History of the cubed, level-normalised aligned reference.
    hist_cube: [f32; FRAME],
    /// Long-term mean square of the aligned reference while it is active.
    ref_ms: f32,
    beta: [f32; PBINS],
    xr: [f32; PBINS],
    tail: [f32; PBINS],
    prev_clean: [f32; PBINS],
    gain: [f32; PBINS],
    noise: [f32; PBINS],
    /// Inverse power response of the analysis filter per bin (all one without a separate
    /// analysis).
    inv_response: [f32; PBINS],
    /// Power of the echo the analysis filter removes relative to the echo estimate per bin
    /// (`|L|^2 / |H|^2`; all zero without a separate analysis).
    split_ratio: [f32; PBINS],
    rng: u64,
}

impl std::fmt::Debug for Postfilter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Postfilter")
            .field("cfg", &self.cfg)
            .finish_non_exhaustive()
    }
}

fn power(c: &Complex<f32>) -> f32 {
    c.norm_sqr()
}

impl Postfilter {
    /// Creates a postfilter.
    #[must_use]
    pub fn new(cfg: PostfilterConfig) -> Self {
        let mut planner = FftPlanner::<f32>::new();
        let mut window = [0.0f32; FRAME];
        for (i, w) in window.iter_mut().enumerate() {
            // Periodic square-root Hann: analysis x synthesis = Hann, which sums to 1.5 at
            // a hop of a third of the frame.
            let h = 0.5 - 0.5 * (std::f32::consts::TAU * i as f32 / FRAME as f32).cos();
            *w = h.sqrt();
        }
        Self {
            fft: planner.plan_fft_forward(FRAME),
            ifft: planner.plan_fft_inverse(FRAME),
            window,
            hist_base: [0.0; FRAME],
            hist_mic: [0.0; FRAME],
            hist_abase: [0.0; FRAME],
            hist_amic: [0.0; FRAME],
            hist_echo: [0.0; FRAME],
            hist_ref: [0.0; FRAME],
            ola: [0.0; FRAME],
            buf: vec![Complex::new(0.0, 0.0); FRAME],
            spec: vec![Complex::new(0.0, 0.0); FRAME],
            rho: [[0.0; PBINS]; LEVELS],
            rho_frames: [0; LEVELS],
            rho_all: [0.0; PBINS],
            gamma: [[0.0; NL_LF_BINS]; LEVELS],
            gamma_all: [0.0; NL_LF_BINS],
            hist_cube: [0.0; FRAME],
            ref_ms: 0.0,
            beta: [cfg.beta_init; PBINS],
            xr: [0.0; PBINS],
            tail: [0.0; PBINS],
            prev_clean: [0.0; PBINS],
            gain: [1.0; PBINS],
            noise: [1e9; PBINS],
            inv_response: [1.0; PBINS],
            split_ratio: [0.0; PBINS],
            rng: cfg.comfort_noise.unwrap_or(0) | 1,
            cfg,
        }
    }

    /// Forgets the learned echo models (a new echo path).
    pub fn reset_models(&mut self) {
        self.rho = [[0.0; PBINS]; LEVELS];
        self.rho_frames = [0; LEVELS];
        self.rho_all = [0.0; PBINS];
        self.gamma = [[0.0; NL_LF_BINS]; LEVELS];
        self.gamma_all = [0.0; NL_LF_BINS];
        self.beta = [self.cfg.beta_init; PBINS];
        self.xr = [0.0; PBINS];
        self.tail = [0.0; PBINS];
    }

    /// Sets the responses of the filter split behind [`Frame::analysis`]: the analysis
    /// signals went through `H`, the base through `A` with `H = A + L`, and the echo
    /// estimate estimates `H(echo)`. `responses(f_hz)` returns `(|H(f)|^2, |L(f)|^2)`. The
    /// residual echo in the base is then the analysis residual seen through `A` instead of
    /// `H` (divided by `|H|^2`, floored at [`MIN_ANALYSIS_RESPONSE`]) plus `L(echo)`, whose
    /// power is `|L|^2 / |H|^2` times the echo estimate's.
    pub fn set_analysis_response(&mut self, responses: impl Fn(f64) -> (f64, f64)) {
        for k in 0..PBINS {
            let f = k as f64 * 16_000.0 / FRAME as f64;
            let (h2, l2) = responses(f);
            let h2 = h2.max(f64::from(MIN_ANALYSIS_RESPONSE));
            self.inv_response[k] = (1.0 / h2) as f32;
            self.split_ratio[k] = (l2 / h2) as f32;
        }
    }

    /// Mean gain of the last frame (1 = no suppression).
    #[must_use]
    pub fn mean_gain(&self) -> f32 {
        self.gain.iter().sum::<f32>() / PBINS as f32
    }

    fn push(hist: &mut [f32; FRAME], block: &[f32; BLOCK]) {
        hist.copy_within(BLOCK.., 0);
        hist[FRAME - BLOCK..].copy_from_slice(block);
    }

    /// Windowed spectrum of `hist` into `self.buf`.
    fn analyse(&mut self, which: u8) {
        let hist = match which {
            0 => &self.hist_abase,
            1 => &self.hist_echo,
            2 => &self.hist_ref,
            _ => &self.hist_base,
        };
        for ((b, &v), &w) in self.buf.iter_mut().zip(hist.iter()).zip(&self.window) {
            *b = Complex::new(v * w, 0.0);
        }
        self.fft.process(&mut self.buf);
    }

    /// Processes one block; `out` receives the cleaned signal delayed by [`LATENCY`].
    pub fn process(&mut self, f: &Frame<'_>, out: &mut [f32; BLOCK]) {
        Self::push(&mut self.hist_base, f.base);
        Self::push(&mut self.hist_mic, f.mic);
        let (abase, amic) = f.analysis.unwrap_or((f.base, f.mic));
        Self::push(&mut self.hist_abase, abase);
        Self::push(&mut self.hist_amic, amic);
        if f.to_mic {
            self.hist_base = self.hist_mic;
            self.hist_abase = self.hist_amic;
        }
        Self::push(&mut self.hist_echo, f.echo);
        Self::push(&mut self.hist_ref, f.aligned);

        self.analyse(1);
        let mut sy = [0.0f32; PBINS];
        for (s, c) in sy.iter_mut().zip(&self.buf) {
            *s = power(c);
        }
        self.analyse(2);
        let mut sx = [0.0f32; PBINS];
        for (s, c) in sx.iter_mut().zip(&self.buf) {
            *s = power(c);
        }
        self.analyse(0);
        let mut se = [0.0f32; PBINS];
        for (s, c) in se.iter_mut().zip(&self.buf) {
            *s = power(c);
        }
        // Power of the signal the gains are applied to (the base; `se` without a separate
        // analysis).
        let mut sb = se;
        if f.analysis.is_some() {
            self.analyse(3);
            for (s, c) in sb.iter_mut().zip(&self.buf) {
                *s = power(c);
            }
        }
        self.spec.copy_from_slice(&self.buf);

        // Reference level class of this frame (mean square of the aligned reference).
        let ms = self.hist_ref.iter().map(|v| v * v).sum::<f32>() / FRAME as f32;
        let level_db = 10.0 * ms.max(1e-12).log10();
        let q = (((level_db + 60.0) / 6.0).floor().max(0.0) as usize).min(LEVELS - 1);

        for k in 0..PBINS {
            // Floors keep decaying state out of f32 denormals (slow arithmetic).
            self.xr[k] = sx[k].max(self.cfg.reverb_decay * self.xr[k]).max(1e-20);
            self.noise[k] = if se[k] < self.noise[k] {
                se[k].max(1e-12)
            } else {
                self.noise[k] * 1.005
            };
        }

        // Cubed reference (low-frequency product predictor), normalised by the reference's
        // long-term mean square so it keeps the reference's units at every level.
        let block_ms = f.aligned.iter().map(|v| v * v).sum::<f32>() / BLOCK as f32;
        if block_ms > 1e-8 {
            self.ref_ms = if self.ref_ms > 0.0 {
                0.99 * self.ref_ms + 0.01 * block_ms
            } else {
                block_ms
            };
        }
        let inv_ms = if self.ref_ms > 0.0 {
            1.0 / self.ref_ms.max(NL_LF_MIN_MS)
        } else {
            0.0
        };
        let mut cube = [0.0f32; BLOCK];
        for (c, &v) in cube.iter_mut().zip(f.aligned) {
            *c = v * v * v * inv_ms;
        }
        Self::push(&mut self.hist_cube, &cube);
        for ((b, &v), &w) in self.buf.iter_mut().zip(&self.hist_cube).zip(&self.window) {
            *b = Complex::new(v * w, 0.0);
        }
        self.fft.process(&mut self.buf);
        let mut sc = [0.0f32; NL_LF_BINS];
        for (s, c) in sc.iter_mut().zip(&self.buf) {
            *s = power(c);
        }
        let leak_in = if self.cfg.use_leak { f.leak } else { 0.0 };
        if f.learn {
            // The residual the in-bin terms leave, per unit of cube power.
            let ready = self.rho_frames[q] >= MIN_CLASS_FRAMES;
            let fresh = self.rho_frames[q] == 0;
            for k in 0..NL_LF_BINS {
                if sc[k] <= 1e-30 {
                    continue;
                }
                let rho = if ready {
                    self.rho[q][k]
                } else {
                    self.rho_all[k]
                };
                let in_bin = leak_in.max(rho) * sy[k];
                let raw = ((se[k] - in_bin).max(0.0) / sc[k]).min(NL_LF_MAX_RATIO);
                // Robust update as for rho: at most fourfold per frame.
                let update = |base: f32| {
                    let cap = (4.0 * base).max(1e-3);
                    0.9 * base + 0.1 * raw.min(cap)
                };
                let all = self.gamma_all[k];
                let class = if fresh { all } else { self.gamma[q][k] };
                self.gamma[q][k] = update(class);
                self.gamma_all[k] = update(all);
            }
        }
        // Learn while only the far end talks.
        if f.learn {
            for k in 0..PBINS {
                if sy[k] > 1e-12 {
                    let raw = se[k] / sy[k];
                    // Robust update: a burst can at most quadruple the ratio, so a near-end
                    // onset the detector missed cannot inflate it.
                    // The ratio is at most 1: the model cannot predict residue in bins where
                    // the linear estimate is weak. That is the remaining far-end leak at
                    // 9.34-9.47 s on double-talk-ser-0 (output/echo -9 dB): softclip 3
                    // distortion of a loud passage lands in low bins with E_k up to 17 dB
                    // above Y_k (softclip 4: -31 dB, linear: -51 dB). Raising the clamp to 4
                    // or 16 cut that leak to -16.7 / -20.5 dB but cost 0.2-0.5 dB of headline
                    // double-talk SI-SDR and took robustness grid points below 10 dB
                    // (measured 2026-10-24); a cross-bin (harmonic) model is the open fix.
                    let update = |base: f32| {
                        let cap = (4.0 * base).max(1e-3);
                        0.9 * base + 0.1 * raw.min(cap).clamp(1e-4, 1.0)
                    };
                    let all = self.rho_all[k];
                    // A fresh class starts from the class-independent ratio.
                    let class = if self.rho_frames[q] == 0 {
                        all
                    } else {
                        self.rho[q][k]
                    };
                    self.rho[q][k] = update(class);
                    self.rho_all[k] = update(all);
                }
            }
            self.rho_frames[q] = self.rho_frames[q].saturating_add(1);
        }
        if f.learn_cold {
            for ((b, &xr), &e) in self.beta.iter_mut().zip(&self.xr).zip(&se) {
                if xr > 1e-12 {
                    *b = 0.9 * *b + 0.1 * (e / xr).clamp(1e-6, 4.0);
                }
            }
        }

        // Residual echo estimate with a reverberation tail.
        let class_ready = self.rho_frames[q] >= MIN_CLASS_FRAMES;
        let rho_of = |k: usize| {
            if class_ready {
                self.rho[q][k]
            } else {
                self.rho_all[k]
            }
        };
        let gamma_of = |k: usize| {
            if class_ready {
                self.gamma[q][k]
            } else {
                self.gamma_all[k]
            }
        };
        let leak = leak_in;
        let mut tail = self.tail;
        for (k, (t, &y)) in tail.iter_mut().zip(&sy).enumerate() {
            let mut r = leak.max(rho_of(k)) * y;
            if k < NL_LF_BINS {
                r = r.max(gamma_of(k) * sc[k]);
            }
            // The reference-based model overestimates the residual; with the near end
            // talking only the (accurate) leakage and nonlinear terms are used.
            if f.cold {
                let w = if f.doubletalk {
                    f.doubletalk_ref_weight
                } else {
                    1.0
                };
                r = r.max(w * self.beta[k] * self.xr[k]);
            }
            // In the base's band (exactly `r` without a separate analysis).
            let r = r * self.inv_response[k] + self.split_ratio[k] * y;
            *t = r.max(self.cfg.reverb_decay * *t).max(1e-20);
        }
        self.tail = tail;

        // Decision-directed Wiener gain.
        let od = if f.doubletalk {
            self.cfg.doubletalk_overdrive
        } else {
            self.cfg.overdrive
        };
        let cfg = self.cfg;
        for (((g, clean), &t), &e) in self
            .gain
            .iter_mut()
            .zip(self.prev_clean.iter_mut())
            .zip(&self.tail)
            .zip(&sb)
        {
            *g = if f.suppress {
                let rr = (od * t).max(1e-12);
                if cfg.subtractive {
                    (1.0 - rr / e.max(1e-12)).max(cfg.min_gain)
                } else {
                    let post = e / rr;
                    let prio =
                        cfg.dd_alpha * *clean / rr + (1.0 - cfg.dd_alpha) * (post - 1.0).max(0.0);
                    (prio / (1.0 + prio)).max(cfg.min_gain)
                }
            } else {
                1.0
            };
            *clean = (*g * *g * e).max(1e-20);
        }

        // Apply (Hermitian), optional comfort noise, inverse transform, overlap-add.
        for k in 0..PBINS {
            self.buf[k] = self.spec[k] * self.gain[k];
        }
        if self.cfg.comfort_noise.is_some() && f.suppress {
            for k in 1..PBINS - 1 {
                let level = (self.noise[k] * (1.0 - self.gain[k] * self.gain[k]).max(0.0)).sqrt();
                self.rng ^= self.rng << 13;
                self.rng ^= self.rng >> 7;
                self.rng ^= self.rng << 17;
                let ph = (self.rng >> 11) as f32 / (1u64 << 53) as f32 * std::f32::consts::TAU;
                self.buf[k] += Complex::from_polar(level, ph);
            }
        }
        for k in 1..FRAME - PBINS + 1 {
            self.buf[FRAME - k] = self.buf[k].conj();
        }
        self.ifft.process(&mut self.buf);
        // Hann (analysis x synthesis) sums to FRAME / BLOCK / 2 at a hop of BLOCK.
        let scale = 1.0 / (FRAME as f32 * (FRAME / BLOCK) as f32 / 2.0);
        for ((o, b), &w) in self.ola.iter_mut().zip(&self.buf).zip(&self.window) {
            *o += b.re * w * scale;
        }
        out.copy_from_slice(&self.ola[..BLOCK]);
        self.ola.copy_within(BLOCK.., 0);
        self.ola[FRAME - BLOCK..].iter_mut().for_each(|v| *v = 0.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unity<'a>(base: &'a [f32; BLOCK], zero: &'a [f32; BLOCK]) -> Frame<'a> {
        Frame {
            base,
            mic: base,
            to_mic: false,
            echo: zero,
            aligned: zero,
            leak: 0.0,
            suppress: false,
            learn: false,
            cold: false,
            learn_cold: false,
            doubletalk: false,
            doubletalk_ref_weight: 0.0,
            analysis: None,
        }
    }

    /// An asymmetric (zero-mean) far-end waveform, 300 Hz with a second harmonic in cosine
    /// phase, amplitude-modulated at 3 Hz: its cube has a baseband component (below 125 Hz)
    /// that the waveform itself lacks, as clipping an asymmetric glottal waveform does.
    fn asymmetric_far_end(n: usize) -> Vec<f32> {
        (0..n)
            .map(|i| {
                let t = i as f32 / 16_000.0;
                let th = std::f32::consts::TAU * 300.0 * t;
                let env = 0.6 + 0.4 * (std::f32::consts::TAU * 3.0 * t).sin();
                0.1 * env * (th.sin() + 0.6 * (2.0 * th).cos())
            })
            .collect()
    }

    fn lf_power(x: &[f32]) -> f32 {
        // Power below 125 Hz of the last FRAME samples (Hann window, DFT bins 0..3).
        let n = x.len();
        let frame = &x[n - FRAME..];
        (0..NL_LF_BINS)
            .map(|k| {
                let (mut re, mut im) = (0.0f32, 0.0f32);
                for (i, &v) in frame.iter().enumerate() {
                    let w = 0.5 - 0.5 * (std::f32::consts::TAU * i as f32 / FRAME as f32).cos();
                    let ph = std::f32::consts::TAU * (k * i) as f32 / FRAME as f32;
                    re += w * v * ph.cos();
                    im -= w * v * ph.sin();
                }
                re * re + im * im
            })
            .sum()
    }

    /// Runs `blocks` far-end-only frames: the base holds the low-frequency product
    /// `0.05 x^3 / ms` (`ms` the reference's mean square) plus a weak linear residual
    /// `0.05 y`, the echo estimate is `y = 0.3 x`; returns the mean gain of the last frame
    /// over the bins below 125 Hz, with `extra` added to the base from block `extra_from`.
    fn lf_gain(blocks: usize, learn: bool, extra: &dyn Fn(usize) -> f32, extra_from: usize) -> f32 {
        let x = asymmetric_far_end(blocks * BLOCK);
        let ms = x.iter().map(|v| v * v).sum::<f32>() / x.len() as f32;
        let mut pf = Postfilter::new(PostfilterConfig::default());
        let mut out = [0.0f32; BLOCK];
        for b in 0..blocks {
            let r = b * BLOCK..(b + 1) * BLOCK;
            let mut aligned = [0.0f32; BLOCK];
            aligned.copy_from_slice(&x[r.clone()]);
            let mut echo = [0.0f32; BLOCK];
            let mut base = [0.0f32; BLOCK];
            for (i, j) in r.clone().enumerate() {
                echo[i] = 0.3 * x[j];
                base[i] = 0.05 * x[j] * x[j] * x[j] / ms + 0.05 * echo[i];
                if b >= extra_from {
                    base[i] += extra(j);
                }
            }
            let f = Frame {
                base: &base,
                mic: &base,
                to_mic: false,
                echo: &echo,
                aligned: &aligned,
                leak: 0.0,
                suppress: true,
                learn: learn && b < extra_from,
                cold: false,
                learn_cold: false,
                doubletalk: false,
                doubletalk_ref_weight: 0.0,
                analysis: None,
            };
            pf.process(&f, &mut out);
        }
        pf.gain[..NL_LF_BINS].iter().sum::<f32>() / NL_LF_BINS as f32
    }

    #[test]
    fn aec_postfilter_suppresses_low_frequency_products_the_echo_estimate_lacks() {
        // Precondition: the product is in the base below 125 Hz, the echo estimate is not.
        let x = asymmetric_far_end(4 * FRAME);
        let x3: Vec<f32> = x.iter().map(|v| v * v * v).collect();
        let (px, px3) = (lf_power(&x), lf_power(&x3));
        let ms = x.iter().map(|v| v * v).sum::<f32>() / x.len() as f32;
        assert!(
            px3 / (ms * ms) > 1e3 * px,
            "the cube must carry baseband power the waveform lacks: {px3} vs {px}"
        );
        // Learned while only the far end talks, the low-frequency product is suppressed;
        // without learning (no model) it passes.
        let none = |_: usize| 0.0;
        let learned = lf_gain(400, true, &none, usize::MAX);
        let unlearned = lf_gain(400, false, &none, usize::MAX);
        eprintln!("low-frequency gain: learned {learned:.3}, not learned {unlearned:.3}");
        assert!(
            learned < 0.2,
            "low-frequency product passed: gain {learned}"
        );
        assert!(unlearned > 0.5, "control: gain {unlearned} without a model");
    }

    #[test]
    fn aec_postfilter_keeps_near_end_low_frequency_content_above_the_product() {
        // A near-end 40 Hz component far above the learned product (added after learning)
        // keeps its gain: the model predicts the echo's product, not everything below 125 Hz.
        let hum = |j: usize| 0.2 * (std::f32::consts::TAU * 40.0 * j as f32 / 16_000.0).sin();
        let g = lf_gain(500, true, &hum, 400);
        eprintln!("low-frequency gain with near-end content: {g:.3}");
        assert!(
            g > 0.8,
            "near-end low-frequency content suppressed: gain {g}"
        );
    }

    #[test]
    fn aec_postfilter_cube_stays_finite_at_the_input_limit() {
        // A faint reference (mean square 4e-8) for 50 blocks, then the canceller's input
        // limit (1e3, square waves) on reference, echo estimate and base: the cube spectrum,
        // the learned ratio, the gains and the output stay finite.
        let mut pf = Postfilter::new(PostfilterConfig::default());
        let mut out = [0.0f32; BLOCK];
        for b in 0..80 {
            let amp = if b < 50 { 2e-4 } else { 1e3 };
            let mut aligned = [0.0f32; BLOCK];
            for (i, a) in aligned.iter_mut().enumerate() {
                *a = if (i / 8) % 2 == 0 { amp } else { -amp };
            }
            let f = Frame {
                base: &aligned,
                mic: &aligned,
                to_mic: false,
                echo: &aligned,
                aligned: &aligned,
                leak: 0.0,
                suppress: true,
                learn: true,
                cold: false,
                learn_cold: false,
                doubletalk: false,
                doubletalk_ref_weight: 0.0,
                analysis: None,
            };
            pf.process(&f, &mut out);
            assert!(
                out.iter().all(|v| v.is_finite()),
                "block {b}: output not finite"
            );
            assert!(
                pf.gain.iter().all(|g| g.is_finite()),
                "block {b}: gain not finite"
            );
            assert!(
                pf.tail.iter().all(|t| t.is_finite()),
                "block {b}: residual not finite"
            );
            assert!(
                pf.gamma
                    .iter()
                    .flatten()
                    .chain(&pf.gamma_all)
                    .all(|g| g.is_finite()),
                "block {b}: ratio not finite"
            );
        }
    }

    #[test]
    fn aec_postfilter_low_frequency_band_is_below_125_hz() {
        let hz = |k: usize| k as f32 * 16_000.0 / FRAME as f32;
        let edge = NL_LF_HZ as f32;
        assert_eq!(NL_LF_BINS, 3);
        assert!(hz(NL_LF_BINS - 1) < edge && hz(NL_LF_BINS) >= edge);
    }

    #[test]
    fn aec_postfilter_unity_gain_is_a_pure_delay() {
        let mut pf = Postfilter::new(PostfilterConfig::default());
        let x: Vec<f32> = (0..BLOCK * 40)
            .map(|i| ((i as f32 * 0.37).sin() + (i as f32 * 0.051).cos()) * 0.3)
            .collect();
        let zero = [0.0f32; BLOCK];
        let mut y = vec![0.0f32; x.len()];
        for (xb, yb) in x
            .as_chunks::<BLOCK>()
            .0
            .iter()
            .zip(y.as_chunks_mut::<BLOCK>().0)
        {
            pf.process(&unity(xb, &zero), yb);
        }
        for i in 0..x.len() - LATENCY {
            assert!((y[i + LATENCY] - x[i]).abs() < 1e-5, "sample {i}");
        }
        assert!(y[..LATENCY].iter().all(|v| v.abs() < 1e-6));
    }
}
