//! Loudspeaker-path nonlinearity estimator (T1.2a): `cargo xtask echo-nl`.
//!
//! Measures how much of the echo in a two-track recording (mic + loopback) no linear
//! filter can explain, so the call simulator's loudspeaker soft clip
//! (`acceptance/aec_scenarios.toml`, `softclip`) is set from real data instead of being
//! guessed. The same estimator runs on simulator output with known soft-clip levels, which
//! turns the real measurement into a soft-clip setting.
//!
//! Method (all at the 16 kHz pipeline rate):
//!
//! 1. Both tracks are high-passed at `highpass_hz` (identical filter, so their linear
//!    relation is unchanged): room rumble below it cannot come from the loudspeaker.
//! 2. Bulk delay: GCC-PHAT (Knapp and Carter, "The generalized correlation method for
//!    estimation of time delay", IEEE Trans. ASSP 24(4), 1976) over 2 s windows every
//!    0.5 s; the loopback is shifted by the median lag of the confident windows minus a
//!    causal margin `pre`. On a real recording the strongest peak flips between nearby
//!    components (several early paths, for example two loudspeakers), so `pre` and `taps`
//!    cover the whole spread instead of chasing the peak.
//! 3. Blocks (`block`, 256 ms) are *far-active* when the aligned loopback is within
//!    `far_active_db` of its active level, *far-silent* when it is below `far_silent_db`
//!    over the block and the echo tail before it. Blocks touching a capture gap or
//!    discontinuity (from `timestamps.json`) are excluded from the fit; a loopback gap
//!    (nothing rendered) still counts as far-silent for the noise floor.
//! 4. Linear echo path, per chunk of `chunk_blocks` blocks (so slow path changes during a
//!    call, which an adaptive canceller tracks, are not counted as nonlinearity): a
//!    `taps`-long FIR fitted over the chunk's far-end-only frames. The start is the Welch
//!    cross-spectral Wiener estimate `H = S_xy / S_xx` (Hann frames; Bendat and Piersol,
//!    "Random Data", 4th ed., ch. 6 and 9), refined by `h += IFFT(S_xe / S_xx)` truncated
//!    to `taps` (a Richardson iteration on the normal equations preconditioned by the
//!    Welch spectrum). Its fixed point zeroes the windowed cross-correlation between
//!    residual and loopback at every lag in `0..taps`, i.e. the least-squares FIR, without
//!    the lag-window bias of a plain Welch estimate. Residual power is scaled by
//!    `n / (n - taps)` (`n` = fitted samples), the least-squares degrees-of-freedom
//!    correction, so short chunks are not flattered.
//! 5. Residual model: over far-end-only blocks, `P_res = N + r P_echo`, fitted by
//!    iteratively reweighted least squares on the relative error. `N` is background that
//!    does not scale with the echo (mic and room noise, loudspeaker hiss); `r P_echo` is
//!    the part that does.
//! 6. Double-talk rejection: far-active blocks whose residual exceeds the model by more
//!    than `near_margin_db` hold near-end speech and are dropped; the paths and the model
//!    are refitted on the rest, `selection_rounds` times. The first round starts from the
//!    lower envelope (20th percentiles), which tolerates a majority of double-talk blocks.
//! 7. Results, over the far-end-only blocks:
//!    - `loud_enr`, the decision value: `sum P_echo / sum P_res` over the loudest
//!      quarter of the blocks, nothing subtracted. Clipping distortion is strongest there,
//!      and attributing all residual (noise, sub-sample path changes, stereo rendering
//!      the mono loopback cannot represent) to the loudspeaker can only overstate the
//!      distortion, which is the safe direction for choosing a simulator setting.
//!    - `enr`, the best estimate: `sum P_echo / (sum P_res - k N)` over all `k` blocks.
//!    - The idle noise floor (far-silent blocks within 3 dB of their 10th percentile) and
//!      `enr_idle_db`, the ENR with only that floor subtracted.
//!
//!    Intervals are the block jackknife (Quenouille/Tukey; Efron and Tibshirani, "An
//!    Introduction to the Bootstrap", ch. 11) +/- 2 sigma over 10 contiguous groups,
//!    refitting the model for each.
//! 8. Calibration: the same measurement on the far-only scenario at each level of
//!    [`SOFTCLIP_GRID`]; the simulator setting is the mildest level whose `loud_enr` is at
//!    or below the real recording's (rounded towards more distortion).

use std::fs::File;
use std::io::BufReader;
use std::ops::Range;
use std::path::Path;

use anyhow::{Context, Result, bail, ensure};
use earshot_core::audio::{PIPELINE_RATE, read_wav, resample_all};
use earshot_core::capture::{RawTimeline, RawTrackTimeline};
use earshot_core::dsp::fft_filter;
use earshot_core::sim::{Scenario, ScenarioFile, SimOutput, SpeechPool, simulate};
use rand_chacha::ChaCha8Rng;
use rand_core::SeedableRng;
use rustfft::FftPlanner;
use rustfft::num_complex::Complex;

/// Analysis rate (the pipeline rate).
pub const RATE_HZ: u32 = 16_000;

/// Estimator settings. [`Config::default`] is what the tool and the tests use.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Config {
    /// High-pass corner applied to both tracks (Hz).
    pub highpass_hz: f64,
    /// Block length for classification and statistics (samples).
    pub block: usize,
    /// Welch frame length (samples); a whole number of blocks.
    pub frame: usize,
    /// FIR length of the fitted linear echo path (samples), `pre` included.
    pub taps: usize,
    /// Causal margin kept before the bulk delay (samples).
    pub pre: usize,
    /// Blocks per chunk with its own echo path fit.
    pub chunk_blocks: usize,
    /// Largest bulk delay searched (samples).
    pub max_delay: usize,
    /// GCC-PHAT window (samples).
    pub lag_window: usize,
    /// GCC-PHAT window step (samples).
    pub lag_step: usize,
    /// A window is confident when its peak is this many times the correlation RMS.
    pub lag_quality: f64,
    /// Far-active: aligned loopback block power at most this far below the active level (dB).
    pub far_active_db: f64,
    /// Far-silent: loopback power over block and echo tail below the active level by more
    /// than this (dB, negative).
    pub far_silent_db: f64,
    /// Blocks whose residual exceeds the model by more than this are double talk (dB).
    pub near_margin_db: f64,
    /// Rounds of fit -> reject double talk -> refit.
    pub selection_rounds: usize,
    /// Refinement iterations of the FIR fit.
    pub refine_iterations: usize,
    /// Regularisation of the Welch preconditioner, relative to the mean loopback spectrum.
    pub regularisation: f64,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            highpass_hz: 120.0,
            block: 4_096,
            frame: 16_384,
            taps: 8_192,
            pre: 256,
            chunk_blocks: 32,
            max_delay: 9_600,
            lag_window: 32_000,
            lag_step: 8_000,
            lag_quality: 8.0,
            far_active_db: -30.0,
            far_silent_db: -60.0,
            near_margin_db: 8.0,
            selection_rounds: 4,
            refine_iterations: 8,
            regularisation: 1e-3,
        }
    }
}

/// Sample ranges (16 kHz) whose data cannot be trusted: capture gaps, discontinuities.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Excluded {
    /// Ranges in the mic track.
    pub mic: Vec<Range<usize>>,
    /// Ranges in the loopback track.
    pub loopback: Vec<Range<usize>>,
}

/// How a block was classified.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockClass {
    /// Touches a capture gap or discontinuity.
    Excluded,
    /// Far end silent, mic at the idle floor: used for the noise floor.
    Noise,
    /// Far end silent, mic above the idle floor (near-end speech, noise bursts).
    FarSilent,
    /// Far end active, no near end detected: used for the results.
    FarOnly,
    /// Far end active, rejected as double talk.
    DoubleTalk,
    /// Far end active, but its chunk had too little far-end-only audio to fit a path.
    Unfitted,
    /// Neither active nor silent (quiet far end, or the echo tail of a far-end burst).
    Other,
}

/// Per-block levels (dBFS power, after the high-pass) and class.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BlockStat {
    /// Block start (s).
    pub start_s: f64,
    /// Aligned loopback.
    pub loopback_db: f64,
    /// Mic.
    pub mic_db: f64,
    /// Linear echo estimate.
    pub echo_db: f64,
    /// Mic minus linear echo estimate (degrees-of-freedom corrected).
    pub residual_db: f64,
    /// Classification.
    pub class: BlockClass,
}

/// One GCC-PHAT window.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LagWindow {
    /// Window centre (s).
    pub centre_s: f64,
    /// Lag of the mic behind the loopback (samples).
    pub lag: f64,
    /// Peak over correlation RMS.
    pub quality: f64,
}

/// A chunk with its own echo path fit.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Chunk {
    /// Blocks covered.
    pub blocks: (usize, usize),
    /// Whether enough far-end-only frames existed to fit the path.
    pub fitted: bool,
    /// Energy of the fitted FIR's last quarter relative to all of it (dB): a check that
    /// `taps` covers the echo tail.
    pub tail_db: f64,
}

/// A ratio with its jackknife interval (dB).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Estimate {
    /// Point estimate.
    pub db: f64,
    /// Lower end (more distortion).
    pub low_db: f64,
    /// Upper end (less distortion; may be infinite).
    pub high_db: f64,
}

/// Result of [`measure`].
#[derive(Debug, Clone, PartialEq)]
pub struct Measurement {
    /// Every GCC-PHAT window.
    pub lags: Vec<LagWindow>,
    /// Median lag of the confident windows (samples).
    pub delay: f64,
    /// Smallest and largest confident lag (samples).
    pub lag_range: (f64, f64),
    /// Chunks.
    pub chunks: Vec<Chunk>,
    /// Far-active audio (s).
    pub far_active_s: f64,
    /// Far-end-only audio used for the results (s).
    pub far_only_s: f64,
    /// Blocks used for the idle noise floor.
    pub noise_blocks: usize,
    /// Linear echo estimate power over the far-only blocks (dBFS).
    pub echo_dbfs: f64,
    /// Residual power over the far-only blocks (dBFS).
    pub residual_dbfs: f64,
    /// Background `N` of the residual model (dBFS).
    pub background_dbfs: f64,
    /// Idle noise floor from far-silent blocks (dBFS; -300 when there is none).
    pub idle_noise_dbfs: f64,
    /// Echo over residual, nothing subtracted, all far-only blocks (dB).
    pub enr_raw_db: f64,
    /// Echo over residual minus the idle floor (dB).
    pub enr_idle_db: f64,
    /// Echo over residual minus the model background, all far-only blocks (dB).
    pub enr: Estimate,
    /// Echo over residual, nothing subtracted, loudest quarter of the far-only blocks (dB):
    /// the decision value.
    pub loud_enr: Estimate,
    /// Per-block detail.
    pub blocks: Vec<BlockStat>,
}

fn db(p: f64) -> f64 {
    earshot_core::dsp::db(p)
}

fn power(x: &[f32]) -> f64 {
    earshot_core::dsp::power(x)
}

/// Value at quantile `q` (0..=1, nearest rank) of `v`; `None` when empty.
fn quantile(v: &[f64], q: f64) -> Option<f64> {
    if v.is_empty() {
        return None;
    }
    let mut s = v.to_vec();
    s.sort_by(f64::total_cmp);
    let i = ((s.len() - 1) as f64 * q.clamp(0.0, 1.0)).round() as usize;
    Some(s[i])
}

/// GCC-PHAT lag (samples, parabolic sub-sample) of `y` behind `x` over `y[start..start + len]`,
/// searching lags `0..max_lag`, and the peak's height over the RMS of the correlation.
/// Needs `start >= max_lag`.
fn gcc_phat(
    planner: &mut FftPlanner<f64>,
    x: &[f32],
    y: &[f32],
    start: usize,
    len: usize,
    max_lag: usize,
) -> (f64, f64) {
    let n = (len + max_lag).next_power_of_two() * 2;
    let fwd = planner.plan_fft_forward(n);
    let inv = planner.plan_fft_inverse(n);
    let mut a = vec![Complex::new(0.0, 0.0); n];
    let mut b = vec![Complex::new(0.0, 0.0); n];
    for i in 0..len {
        a[i] = Complex::new(f64::from(y[start + i]), 0.0);
    }
    let r0 = start - max_lag;
    for i in r0..start + len {
        b[i - r0] = Complex::new(f64::from(x[i]), 0.0);
    }
    fwd.process(&mut a);
    fwd.process(&mut b);
    for (p, q) in a.iter_mut().zip(&b) {
        let c = *p * q.conj();
        let m = c.norm();
        *p = if m > 1e-20 {
            c / m
        } else {
            Complex::new(0.0, 0.0)
        };
    }
    inv.process(&mut a);
    // IFFT(A conj(B))[k] = sum_i y_win[i + k] x_win[i]; y[m] ~ x[m - lag] gives
    // k = lag - max_lag (mod n).
    let corr = |lag: usize| -> f64 {
        let k = lag as isize - max_lag as isize;
        a[k.rem_euclid(n as isize) as usize].re
    };
    let best = (0..max_lag)
        .max_by(|&p, &q| corr(p).total_cmp(&corr(q)))
        .unwrap_or(0);
    let rms = ((0..max_lag).map(|l| corr(l).powi(2)).sum::<f64>() / max_lag as f64).sqrt();
    let quality = if rms > 0.0 { corr(best) / rms } else { 0.0 };
    if best == 0 || best + 1 >= max_lag {
        return (best as f64, quality);
    }
    let (ym, y0, yp) = (corr(best - 1), corr(best), corr(best + 1));
    let denom = ym - 2.0 * y0 + yp;
    let frac = if denom.abs() > 1e-12 {
        0.5 * (ym - yp) / denom
    } else {
        0.0
    };
    (best as f64 + frac, quality)
}

/// GCC-PHAT windows over the recording (step 2 of the module doc), skipping windows
/// whose loopback is near silent.
#[must_use]
pub fn lag_track(mic: &[f32], loopback: &[f32], cfg: &Config) -> Vec<LagWindow> {
    let n = mic.len().min(loopback.len());
    let mut planner = FftPlanner::new();
    let active = quantile(
        &loopback[..n]
            .chunks(cfg.block)
            .map(power)
            .filter(|&p| p > 0.0)
            .collect::<Vec<_>>(),
        0.9,
    )
    .unwrap_or(0.0);
    let mut out = Vec::new();
    let mut start = cfg.max_delay;
    while start + cfg.lag_window <= n {
        let lb = power(&loopback[start - cfg.max_delay..start + cfg.lag_window]);
        if lb > active * 1e-3 {
            let (lag, quality) = gcc_phat(
                &mut planner,
                loopback,
                mic,
                start,
                cfg.lag_window,
                cfg.max_delay,
            );
            out.push(LagWindow {
                centre_s: (start + cfg.lag_window / 2) as f64 / f64::from(RATE_HZ),
                lag,
                quality,
            });
        }
        start += cfg.lag_step;
    }
    out
}

/// Fourth-order high-pass at `fc` Hz: two cascaded second-order Butterworth sections
/// (Q = 1/sqrt 2; coefficients from R. Bristow-Johnson, "Cookbook formulae for audio EQ
/// biquad filter coefficients"). Applied identically to mic and loopback it leaves their
/// linear relation unchanged and removes rumble the loudspeaker cannot have produced.
#[must_use]
pub fn highpass(x: &[f32], fc: f64) -> Vec<f32> {
    let w0 = 2.0 * std::f64::consts::PI * fc / f64::from(RATE_HZ);
    let alpha = w0.sin() / (2.0 * std::f64::consts::FRAC_1_SQRT_2);
    let cw = w0.cos();
    let a0 = 1.0 + alpha;
    let b = [
        (1.0 + cw) / 2.0 / a0,
        -(1.0 + cw) / a0,
        (1.0 + cw) / 2.0 / a0,
    ];
    let a = [-2.0 * cw / a0, (1.0 - alpha) / a0];
    let mut y: Vec<f64> = x.iter().map(|&v| f64::from(v)).collect();
    for _ in 0..2 {
        let (mut x1, mut x2, mut y1, mut y2) = (0.0, 0.0, 0.0, 0.0);
        for v in &mut y {
            let x0 = *v;
            let y0 = b[0] * x0 + b[1] * x1 + b[2] * x2 - a[0] * y1 - a[1] * y2;
            (x2, x1, y2, y1) = (x1, x0, y1, y0);
            *v = y0;
        }
    }
    y.into_iter().map(|v| v as f32).collect()
}

fn hann(n: usize) -> Vec<f64> {
    (0..n)
        .map(|t| 0.5 - 0.5 * (2.0 * std::f64::consts::PI * t as f64 / n as f64).cos())
        .collect()
}

/// Linear echo path fit (step 4 of the module doc) over the frames starting at `frames`:
/// returns the FIR, `h * x` and `y - h * x`.
fn fit_linear_path(
    x: &[f32],
    y: &[f32],
    frames: &[usize],
    cfg: &Config,
) -> (Vec<f32>, Vec<f32>, Vec<f32>) {
    let n = cfg.frame;
    let m = 2 * n;
    let mut planner = FftPlanner::<f64>::new();
    let fwd = planner.plan_fft_forward(m);
    let inv = planner.plan_fft_inverse(m);
    let w = hann(n);
    let spectrum = |sig: &[f32], s: usize| -> Vec<Complex<f64>> {
        let mut buf = vec![Complex::new(0.0, 0.0); m];
        for t in 0..n {
            buf[t] = Complex::new(f64::from(sig[s + t]) * w[t], 0.0);
        }
        fwd.process(&mut buf);
        buf
    };
    let mut sxx = vec![0.0f64; m];
    for &s in frames {
        for (acc, v) in sxx.iter_mut().zip(spectrum(x, s)) {
            *acc += v.norm_sqr();
        }
    }
    let reg = cfg.regularisation * sxx.iter().sum::<f64>() / m as f64;
    let mut h = vec![0.0f64; cfg.taps];
    let mut hf: Vec<f32> = vec![0.0; cfg.taps];
    let mut echo = vec![0.0f32; y.len()];
    let mut resid: Vec<f32> = y.to_vec();
    let mut last = f64::INFINITY;
    for _ in 0..=cfg.refine_iterations {
        let mut sxe = vec![Complex::new(0.0, 0.0); m];
        for &s in frames {
            let xs = spectrum(x, s);
            let es = spectrum(&resid, s);
            for ((acc, a), b) in sxe.iter_mut().zip(&xs).zip(&es) {
                *acc += a.conj() * b;
            }
        }
        for (v, &p) in sxe.iter_mut().zip(&sxx) {
            *v /= p + reg;
        }
        inv.process(&mut sxe);
        let mut next = h.clone();
        for (hk, d) in next.iter_mut().zip(&sxe) {
            *hk += d.re / m as f64;
        }
        let nf: Vec<f32> = next.iter().map(|&v| v as f32).collect();
        let e = fft_filter(x, &nf);
        let r: Vec<f32> = y.iter().zip(&e).map(|(a, b)| a - b).collect();
        let p: f64 = frames.iter().map(|&s| power(&r[s..s + n])).sum::<f64>();
        // Keep the step only if it helps; stop once it gains less than 0.01 dB.
        if p >= last {
            break;
        }
        (h, hf, echo, resid) = (next, nf, e, r);
        if p > last * 10f64.powf(-0.001) {
            break;
        }
        last = p;
    }
    (hf, echo, resid)
}

/// Residual model `P_res = n + r P_echo` (step 5): IRLS on the relative error from the
/// starting point `(n, r)`, with `n, r >= 0`.
fn fit_residual_model(pe: &[f64], pr: &[f64], start: (f64, f64)) -> (f64, f64) {
    let (mut n, mut r) = start;
    for _ in 0..20 {
        let (mut s1, mut se, mut see, mut sr, mut ser) = (0.0, 0.0, 0.0, 0.0, 0.0);
        for (&e, &p) in pe.iter().zip(pr) {
            let m = (n + r * e).max(1e-30);
            let w = 1.0 / (m * m);
            s1 += w;
            se += w * e;
            see += w * e * e;
            sr += w * p;
            ser += w * e * p;
        }
        let det = s1 * see - se * se;
        let (mut nn, mut rr) = if det.abs() > 0.0 {
            ((see * sr - se * ser) / det, (s1 * ser - se * sr) / det)
        } else {
            (sr / s1, 0.0)
        };
        if nn < 0.0 {
            nn = 0.0;
            rr = ser / see;
        }
        if rr < 0.0 {
            rr = 0.0;
            nn = sr / s1;
        }
        let done = (nn - n).abs() <= 1e-9 * nn.max(1e-30) && (rr - r).abs() <= 1e-9 * rr;
        (n, r) = (nn, rr);
        if done {
            break;
        }
    }
    (n, r)
}

/// `sum pe / (sum pr - k n)` in dB; infinite when nothing is left.
fn enr_of(pe: &[f64], pr: &[f64], n: f64) -> f64 {
    let excess = pr.iter().sum::<f64>() - pr.len() as f64 * n;
    if excess <= 0.0 {
        f64::INFINITY
    } else {
        db(pe.iter().sum::<f64>()) - db(excess)
    }
}

/// `stat(pe, pr)` with its block-jackknife interval over up to 10 contiguous groups.
fn jackknife(pe: &[f64], pr: &[f64], stat: &dyn Fn(&[f64], &[f64]) -> f64) -> Estimate {
    let all = pe.len();
    let point = stat(pe, pr);
    let groups = all.min(10);
    if groups < 2 {
        return Estimate {
            db: point,
            low_db: f64::NEG_INFINITY,
            high_db: f64::INFINITY,
        };
    }
    let reps: Vec<f64> = (0..groups)
        .map(|g| {
            let (lo, hi) = (g * all / groups, (g + 1) * all / groups);
            let keep = |v: &[f64]| {
                v.iter()
                    .enumerate()
                    .filter(|(j, _)| *j < lo || *j >= hi)
                    .map(|(_, &p)| p)
                    .collect::<Vec<_>>()
            };
            stat(&keep(pe), &keep(pr))
        })
        .collect();
    let sigma = if reps.iter().all(|r| r.is_finite()) {
        let mean = reps.iter().sum::<f64>() / groups as f64;
        ((groups - 1) as f64 / groups as f64 * reps.iter().map(|r| (r - mean).powi(2)).sum::<f64>())
            .sqrt()
    } else {
        f64::INFINITY
    };
    Estimate {
        db: point,
        low_db: point - 2.0 * sigma,
        high_db: point + 2.0 * sigma,
    }
}

/// Raw `sum pe / sum pr` over the loudest quarter (by `pe`) of the blocks.
fn loud_ratio(pe: &[f64], pr: &[f64]) -> f64 {
    let mut idx: Vec<usize> = (0..pe.len()).collect();
    idx.sort_by(|&a, &b| pe[b].total_cmp(&pe[a]));
    let k = pe.len().div_ceil(4);
    let (se, sr) = idx[..k]
        .iter()
        .fold((0.0, 0.0), |(se, sr), &j| (se + pe[j], sr + pr[j]));
    db(se) - db(sr)
}

/// Measures the loudspeaker-path nonlinearity of a mic/loopback pair at 16 kHz.
///
/// # Errors
/// When there is no confident echo delay or too little far-end-only audio to fit a path.
pub fn measure(
    mic: &[f32],
    loopback: &[f32],
    excluded: &Excluded,
    cfg: &Config,
) -> Result<Measurement> {
    ensure!(
        cfg.frame.is_multiple_of(cfg.block)
            && cfg.taps <= cfg.frame
            && cfg.chunk_blocks * cfg.block >= cfg.frame,
        "frame must be a multiple of block, at least taps long and fit in a chunk"
    );
    let n = mic.len().min(loopback.len());
    let y = highpass(&mic[..n], cfg.highpass_hz);
    let lb = highpass(&loopback[..n], cfg.highpass_hz);
    let b = cfg.block;
    let nb = n / b;
    let blk = |i: usize| i * b..(i + 1) * b;

    // Bulk delay (step 2).
    let lags = lag_track(&y, &lb, cfg);
    let confident: Vec<f64> = lags
        .iter()
        .filter(|w| w.quality >= cfg.lag_quality)
        .map(|w| w.lag)
        .collect();
    let delay =
        quantile(&confident, 0.5).context("no confident GCC-PHAT window: is there echo?")?;
    let lag_range = (
        confident.iter().copied().fold(f64::MAX, f64::min),
        confident.iter().copied().fold(f64::MIN, f64::max),
    );
    let shift = (delay.round() as usize).saturating_sub(cfg.pre);
    let mut x = vec![0.0f32; n];
    x[shift.min(n)..].copy_from_slice(&lb[..n - shift.min(n)]);

    // Untrusted blocks (step 3). A loopback gap affects the echo from its aligned start to
    // the end of the echo tail; for the noise floor only mic gaps matter.
    let mut mic_bad = vec![false; nb];
    let mut lb_bad = vec![false; nb];
    let mark = |bad: &mut [bool], r: Range<usize>| {
        let lo = r.start.saturating_sub(b) / b;
        let hi = (r.end + b).div_ceil(b).min(nb);
        for v in bad.iter_mut().take(hi).skip(lo) {
            *v = true;
        }
    };
    for r in &excluded.mic {
        mark(&mut mic_bad, r.clone());
    }
    for r in &excluded.loopback {
        mark(&mut lb_bad, r.start + shift..r.end + shift + cfg.taps);
    }
    let bad: Vec<bool> = (0..nb).map(|i| mic_bad[i] || lb_bad[i]).collect();

    let lb_pow: Vec<f64> = (0..nb).map(|i| power(&x[blk(i)])).collect();
    let tail_pow: Vec<f64> = (0..nb)
        .map(|i| power(&x[(i * b).saturating_sub(cfg.taps)..(i + 1) * b]))
        .collect();
    let mic_pow: Vec<f64> = (0..nb).map(|i| power(&y[blk(i)])).collect();
    let active = quantile(
        &lb_pow
            .iter()
            .copied()
            .filter(|&p| p > 0.0)
            .collect::<Vec<_>>(),
        0.9,
    )
    .unwrap_or(0.0);
    ensure!(active > 0.0, "the loopback is silent");
    let far_active: Vec<bool> = (0..nb)
        .map(|i| !bad[i] && lb_pow[i] >= active * 10f64.powf(cfg.far_active_db / 10.0))
        .collect();
    let far_silent: Vec<bool> = (0..nb)
        .map(|i| !mic_bad[i] && tail_pow[i] <= active * 10f64.powf(cfg.far_silent_db / 10.0))
        .collect();

    // Idle noise floor (reported for reference).
    let silent_mic: Vec<f64> = (0..nb)
        .filter(|&i| far_silent[i])
        .map(|i| mic_pow[i])
        .collect();
    let mut is_noise = vec![false; nb];
    let mut idle = 0.0;
    if let Some(p10) = quantile(&silent_mic, 0.1) {
        let ps: Vec<f64> = (0..nb)
            .filter(|&i| far_silent[i] && mic_pow[i] <= 2.0 * p10)
            .inspect(|&i| is_noise[i] = true)
            .map(|i| mic_pow[i])
            .collect();
        idle = ps.iter().sum::<f64>() / ps.len() as f64;
    }

    // Fit per chunk, model, reject double talk, refit (steps 4 to 6).
    let per_frame = cfg.frame / b;
    let bounds: Vec<(usize, usize)> = (0..nb)
        .step_by(cfg.chunk_blocks)
        .map(|s| (s, (s + cfg.chunk_blocks).min(nb)))
        .collect();
    let mut selected = far_active.clone();
    let mut echo = vec![0.0f32; n];
    let mut resid = vec![0.0f32; n];
    let mut dof = vec![1.0f64; nb];
    let mut chunks: Vec<Chunk> = bounds
        .iter()
        .map(|&blocks| Chunk {
            blocks,
            fitted: false,
            tail_db: f64::NEG_INFINITY,
        })
        .collect();
    let mut model = (0.0, 0.0);
    for round in 0..cfg.selection_rounds {
        for c in &mut chunks {
            let (s, e) = c.blocks;
            c.fitted = false;
            let starts: Vec<usize> = (s..e.saturating_sub(per_frame - 1))
                .filter(|&f| selected[f..f + per_frame].iter().all(|&v| v))
                .collect();
            let mut covered = vec![false; e - s];
            for &f in &starts {
                for v in &mut covered[f - s..f - s + per_frame] {
                    *v = true;
                }
            }
            let n_fit = covered.iter().filter(|&&v| v).count() * b;
            if n_fit < 4 * cfg.taps {
                continue;
            }
            // Slice with `taps` samples of history.
            let s0 = (s * b).saturating_sub(cfg.taps);
            let frames: Vec<usize> = starts.iter().map(|&f| f * b - s0).collect();
            let (h, es, rs) = fit_linear_path(&x[s0..e * b], &y[s0..e * b], &frames, cfg);
            echo[s * b..e * b].copy_from_slice(&es[s * b - s0..]);
            resid[s * b..e * b].copy_from_slice(&rs[s * b - s0..]);
            let corr = n_fit as f64 / (n_fit - cfg.taps) as f64;
            for v in &mut dof[s..e] {
                *v = corr;
            }
            let energy = |r: &[f32]| r.iter().map(|&v| f64::from(v).powi(2)).sum::<f64>();
            c.tail_db = db(energy(&h[cfg.taps * 3 / 4..])) - db(energy(&h));
            c.fitted = true;
        }
        let fitted_block = |i: usize| chunks[(i / cfg.chunk_blocks).min(chunks.len() - 1)].fitted;
        let usable: Vec<usize> = (0..nb)
            .filter(|&i| far_active[i] && fitted_block(i))
            .collect();
        ensure!(
            !usable.is_empty(),
            "too little far-end-only audio to fit an echo path"
        );
        let pe: Vec<f64> = usable.iter().map(|&i| power(&echo[blk(i)])).collect();
        let pr: Vec<f64> = usable
            .iter()
            .map(|&i| power(&resid[blk(i)]) * dof[i])
            .collect();
        let kept: Vec<usize> = (0..usable.len()).filter(|&j| selected[usable[j]]).collect();
        let pick = |v: &[f64]| kept.iter().map(|&j| v[j]).collect::<Vec<_>>();
        let (kpe, kpr) = (pick(&pe), pick(&pr));
        model = if round == 0 {
            // Lower envelope: robust to a majority of double-talk blocks.
            let ratios: Vec<f64> = kpe.iter().zip(&kpr).map(|(e, r)| r / e).collect();
            (
                quantile(&kpr, 0.2).unwrap_or(0.0) * 0.5,
                quantile(&ratios, 0.2).unwrap_or(0.0) * 0.5,
            )
        } else {
            fit_residual_model(&kpe, &kpr, model)
        };
        let margin = 10f64.powf(cfg.near_margin_db / 10.0);
        let mut next = vec![false; nb];
        for (j, &i) in usable.iter().enumerate() {
            next[i] = pr[j] <= margin * (model.0 + model.1 * pe[j]);
        }
        selected = next;
    }

    let used: Vec<usize> = (0..nb).filter(|&i| selected[i]).collect();
    ensure!(used.len() >= 8, "only {} far-end-only blocks", used.len());
    let pe: Vec<f64> = used.iter().map(|&i| power(&echo[blk(i)])).collect();
    let pr: Vec<f64> = used
        .iter()
        .map(|&i| power(&resid[blk(i)]) * dof[i])
        .collect();
    let model = fit_residual_model(&pe, &pr, model);
    let enr = jackknife(&pe, &pr, &|e, r| {
        enr_of(e, r, fit_residual_model(e, r, model).0)
    });
    let loud_enr = jackknife(&pe, &pr, &loud_ratio);

    let rate = f64::from(RATE_HZ);
    let fitted_block = |i: usize| chunks[(i / cfg.chunk_blocks).min(chunks.len() - 1)].fitted;
    let blocks = (0..nb)
        .map(|i| BlockStat {
            start_s: (i * b) as f64 / rate,
            loopback_db: db(lb_pow[i]),
            mic_db: db(mic_pow[i]),
            echo_db: db(power(&echo[blk(i)])),
            residual_db: db(power(&resid[blk(i)]) * dof[i]),
            class: if bad[i] {
                BlockClass::Excluded
            } else if selected[i] {
                BlockClass::FarOnly
            } else if far_active[i] && fitted_block(i) {
                BlockClass::DoubleTalk
            } else if far_active[i] {
                BlockClass::Unfitted
            } else if is_noise[i] {
                BlockClass::Noise
            } else if far_silent[i] {
                BlockClass::FarSilent
            } else {
                BlockClass::Other
            },
        })
        .collect();
    let mean = |v: &[f64]| v.iter().sum::<f64>() / v.len() as f64;
    Ok(Measurement {
        lags,
        delay,
        lag_range,
        chunks,
        far_active_s: far_active.iter().filter(|&&v| v).count() as f64 * b as f64 / rate,
        far_only_s: used.len() as f64 * b as f64 / rate,
        noise_blocks: is_noise.iter().filter(|&&v| v).count(),
        echo_dbfs: db(mean(&pe)),
        residual_dbfs: db(mean(&pr)),
        background_dbfs: db(model.0),
        idle_noise_dbfs: db(idle),
        enr_raw_db: db(mean(&pe)) - db(mean(&pr)),
        enr_idle_db: enr_of(&pe, &pr, idle),
        enr,
        loud_enr,
        blocks,
    })
}

/// A raw recording (`earshot-cli record-raw` output) at 16 kHz.
#[derive(Debug, Clone, PartialEq)]
pub struct Recording {
    /// Mic track.
    pub mic: Vec<f32>,
    /// Loopback track.
    pub loopback: Vec<f32>,
    /// Untrusted ranges from `timestamps.json`.
    pub excluded: Excluded,
}

/// Gaps (synthesized silence), discontinuities and device changes of one track, in WAV frames.
fn track_excluded(t: &RawTrackTimeline) -> Vec<Range<u64>> {
    let mut out = Vec::new();
    let mut expected = 0u64;
    for p in &t.packets {
        let start = p.track_frame;
        if start > expected {
            out.push(expected..start);
        }
        let end = start + u64::from(p.frames).saturating_sub(p.trimmed_frames);
        if p.discontinuity || p.timestamp_error {
            out.push(start..end.max(start + 1));
        }
        expected = expected.max(end);
    }
    if t.frames_written > expected {
        out.push(expected..t.frames_written);
    }
    for c in &t.device_changes {
        out.push(c.track_frame..c.track_frame + 1);
    }
    out
}

fn to_16k(r: &Range<u64>, rate: u32) -> Range<usize> {
    let s = |f: u64, up: bool| {
        let num = u128::from(f) * u128::from(RATE_HZ);
        let den = u128::from(rate);
        (if up { num.div_ceil(den) } else { num / den }) as usize
    };
    s(r.start, false)..s(r.end, true)
}

fn read_track(path: &Path) -> Result<(Vec<f32>, u32)> {
    let f = File::open(path).with_context(|| format!("cannot open {}", path.display()))?;
    let wav = read_wav(BufReader::new(f)).with_context(|| format!("{}", path.display()))?;
    let mono = wav.to_mono();
    let rate = wav.rate().hz();
    let out = if rate == RATE_HZ {
        mono
    } else {
        resample_all(&mono, wav.rate(), PIPELINE_RATE)
            .with_context(|| format!("resampling {}", path.display()))?
    };
    Ok((out, rate))
}

/// Loads `mic.wav`, `loopback.wav` and `timestamps.json` (any schema in
/// `earshot_core::capture::SUPPORTED_SCHEMAS`, v1 to v5) from `dir`.
///
/// # Errors
/// Missing or malformed files, or an unsupported schema.
pub fn load_recording(dir: &Path) -> Result<Recording> {
    let ts_path = dir.join("timestamps.json");
    let ts: RawTimeline = serde_json::from_reader(BufReader::new(
        File::open(&ts_path).with_context(|| format!("cannot open {}", ts_path.display()))?,
    ))
    .with_context(|| format!("{}", ts_path.display()))?;
    // Same schema set as the core reader; v3 stereo loopback is mixed to mono by read_track.
    ensure!(
        earshot_core::capture::SUPPORTED_SCHEMAS.contains(&ts.schema_version),
        "unsupported timestamps.json schema {}",
        ts.schema_version
    );
    let (mic, mic_rate) = read_track(&dir.join("mic.wav"))?;
    let (loopback, lb_rate) = read_track(&dir.join("loopback.wav"))?;
    if mic_rate != ts.mic.wav_sample_rate || lb_rate != ts.loopback.wav_sample_rate {
        bail!("WAV rates disagree with timestamps.json");
    }
    let excluded = Excluded {
        mic: track_excluded(&ts.mic)
            .iter()
            .map(|r| to_16k(r, mic_rate))
            .collect(),
        loopback: track_excluded(&ts.loopback)
            .iter()
            .map(|r| to_16k(r, lb_rate))
            .collect(),
    };
    Ok(Recording {
        mic,
        loopback,
        excluded,
    })
}

/// Far-end and near-end smoke utterances (speakers 61 and 121), as the core tests load them.
pub type Speech = (Vec<Vec<f32>>, Vec<Vec<f32>>);

/// Decodes a 16 kHz mono FLAC file.
///
/// # Errors
/// An unreadable file or another format.
pub fn decode_flac(path: &Path) -> Result<Vec<f32>> {
    let mut reader = claxon::FlacReader::open(path)
        .map_err(|e| anyhow::anyhow!("cannot open {}: {e}", path.display()))?;
    let info = reader.streaminfo();
    ensure!(
        info.sample_rate == RATE_HZ && info.channels == 1,
        "{}: expected 16 kHz mono",
        path.display()
    );
    let scale = 1.0 / f64::from(1u32 << (info.bits_per_sample - 1));
    reader
        .samples()
        .map(|s| {
            s.map(|v| (f64::from(v) * scale) as f32)
                .map_err(|e| anyhow::anyhow!("{}: {e}", path.display()))
        })
        .collect()
}

/// Loads the committed smoke speech from `root/fixtures/smoke`.
///
/// # Errors
/// Missing or unreadable FLAC files.
pub fn smoke_speech(root: &Path) -> Result<Speech> {
    let dir = root.join("fixtures/smoke");
    let mut names: Vec<String> = std::fs::read_dir(&dir)
        .with_context(|| format!("cannot list {}", dir.display()))?
        .filter_map(|e| e.ok()?.file_name().into_string().ok())
        .filter(|n| n.ends_with(".flac"))
        .collect();
    names.sort();
    let (mut far, mut near) = (Vec::new(), Vec::new());
    for name in names {
        if name.starts_with("61-") {
            far.push(decode_flac(&dir.join(&name))?);
        } else if name.starts_with("121-") {
            near.push(decode_flac(&dir.join(&name))?);
        }
    }
    ensure!(
        !far.is_empty() && !near.is_empty(),
        "smoke speakers 61 and 121 missing"
    );
    Ok((far, near))
}

/// Parses `root/acceptance/aec_scenarios.toml`.
///
/// # Errors
/// Missing or malformed file.
pub fn scenario_file(root: &Path) -> Result<ScenarioFile> {
    let path = root.join("acceptance/aec_scenarios.toml");
    let text = std::fs::read_to_string(&path)
        .with_context(|| format!("cannot read {}", path.display()))?;
    toml::from_str(&text).with_context(|| format!("{}", path.display()))
}

/// Runs `scn` exactly as the core tests do (`ChaCha8Rng` seeded with the scenario seed).
///
/// # Errors
/// As [`simulate`].
pub fn run_scenario(file: &ScenarioFile, scn: &Scenario, speech: &Speech) -> Result<SimOutput> {
    let pool = SpeechPool {
        far: &speech.0,
        near: &speech.1,
    };
    let mut rng = ChaCha8Rng::seed_from_u64(scn.seed);
    simulate(scn, &file.setup, &pool, &mut rng).map_err(|e| anyhow::anyhow!("{e}"))
}

/// The calibration scenario: the file's `far-only` scenario (noise-free, no near end)
/// with the given loudspeaker soft clip.
///
/// # Errors
/// When the file has no `far-only` scenario.
pub fn calibration_scenario(file: &ScenarioFile, softclip: Option<f64>) -> Result<Scenario> {
    let base = file
        .get("far-only")
        .context("aec_scenarios.toml has no far-only scenario")?;
    Ok(Scenario {
        name: format!("calibrate-softclip-{softclip:?}"),
        softclip,
        ..base.clone()
    })
}

/// Soft-clip grid of the calibration table, from linear to heavy clipping.
pub const SOFTCLIP_GRID: &[Option<f64>] = &[
    None,
    Some(16.0),
    Some(12.0),
    Some(8.0),
    Some(6.0),
    Some(5.0),
    Some(4.0),
    Some(3.0),
    Some(2.5),
    Some(2.0),
    Some(1.5),
    Some(1.0),
];

/// One row of the calibration table.
#[derive(Debug, Clone, PartialEq)]
pub struct CalibrationPoint {
    /// Soft-clip setting (`None` = linear).
    pub softclip: Option<f64>,
    /// [`Measurement::loud_enr`] (dB): compared with the real recording.
    pub loud_enr_db: f64,
    /// [`Measurement::enr`] (dB).
    pub enr_db: f64,
}

/// Measures [`calibration_scenario`] at each soft-clip level.
///
/// # Errors
/// Simulation or measurement failure.
pub fn calibrate(
    file: &ScenarioFile,
    speech: &Speech,
    levels: &[Option<f64>],
    cfg: &Config,
) -> Result<Vec<CalibrationPoint>> {
    levels
        .iter()
        .map(|&softclip| {
            let scn = calibration_scenario(file, softclip)?;
            let out = run_scenario(file, &scn, speech)?;
            let m = measure(&out.mic, &out.reference, &Excluded::default(), cfg)?;
            Ok(CalibrationPoint {
                softclip,
                loud_enr_db: m.loud_enr.db,
                enr_db: m.enr.db,
            })
        })
        .collect()
}

/// The least-distorting soft clip in `table` (ordered from linear to heavy clipping) whose
/// simulated loud-block ENR is at most `measured_db`, i.e. at least as severe as the
/// measurement. `None` when even the heaviest level is milder than the measurement.
#[must_use]
pub fn choose_softclip(table: &[CalibrationPoint], measured_db: f64) -> Option<Option<f64>> {
    table
        .iter()
        .find(|p| p.loud_enr_db <= measured_db)
        .map(|p| p.softclip)
}

fn show(c: Option<f64>) -> String {
    c.map_or("none".to_string(), |c| format!("{c}"))
}

fn show_est(e: &Estimate) -> String {
    format!("{:.2} dB [{:.2}, {:.2}]", e.db, e.low_db, e.high_db)
}

/// `cargo xtask echo-nl`: print the calibration table and, with `dir`, measure that
/// recording and the soft clip to configure.
///
/// # Errors
/// Any load, simulation or measurement failure.
pub fn run(root: &Path, dir: Option<&Path>, verbose: bool) -> Result<()> {
    let cfg = Config::default();
    let file = scenario_file(root)?;
    let speech = smoke_speech(root)?;
    println!("calibration: far-only scenario (noise-free), same estimator");
    println!("  softclip  loud-quarter ENR dB  all-blocks ENR dB");
    let table = calibrate(&file, &speech, SOFTCLIP_GRID, &cfg)?;
    for p in &table {
        println!(
            "  {:>8}  {:19.2}  {:17.2}",
            show(p.softclip),
            p.loud_enr_db,
            p.enr_db
        );
    }
    let Some(dir) = dir else {
        return Ok(());
    };
    let rec = load_recording(dir)?;
    let m = measure(&rec.mic, &rec.loopback, &rec.excluded, &cfg)?;
    let rate = f64::from(RATE_HZ);
    if verbose {
        println!("GCC-PHAT windows (centre s, lag samples, peak/rms):");
        for w in &m.lags {
            println!("  {:7.2} {:9.2} {:7.1}", w.centre_s, w.lag, w.quality);
        }
        println!("blocks (start s, loopback, mic, echo est, residual dBFS, class):");
        for blk in &m.blocks {
            println!(
                "  {:7.3} {:7.1} {:7.1} {:7.1} {:7.1} {:?}",
                blk.start_s, blk.loopback_db, blk.mic_db, blk.echo_db, blk.residual_db, blk.class
            );
        }
        println!("chunks (start s, end s, fitted, FIR last-quarter energy dB):");
        for c in &m.chunks {
            println!(
                "  {:7.2} {:7.2} {:5} {:7.1}",
                (c.blocks.0 * cfg.block) as f64 / rate,
                (c.blocks.1 * cfg.block) as f64 / rate,
                c.fitted,
                c.tail_db
            );
        }
    }
    println!("recording {}:", dir.display());
    println!(
        "  bulk delay {:.2} ms (median lag {:.1} samples; confident lags {:.1} to {:.1})",
        m.delay / rate * 1_000.0,
        m.delay,
        m.lag_range.0,
        m.lag_range.1
    );
    println!(
        "  far-active {:.1} s, far-end only {:.1} s",
        m.far_active_s, m.far_only_s
    );
    println!("  far-end-only residual by echo level (bin dBFS, blocks, echo dBFS, residual dBFS):");
    let far: Vec<&BlockStat> = m
        .blocks
        .iter()
        .filter(|b| b.class == BlockClass::FarOnly)
        .collect();
    let mut lo = -90.0;
    while lo < 0.0 {
        let bin: Vec<&&BlockStat> = far
            .iter()
            .filter(|b| b.echo_db >= lo && b.echo_db < lo + 5.0)
            .collect();
        if !bin.is_empty() {
            let mean_db = |f: fn(&BlockStat) -> f64| {
                db(bin.iter().map(|b| 10f64.powf(f(b) / 10.0)).sum::<f64>() / bin.len() as f64)
            };
            println!(
                "    {lo:5.0} {:4} {:7.1} {:7.1}",
                bin.len(),
                mean_db(|b| b.echo_db),
                mean_db(|b| b.residual_db)
            );
        }
        lo += 5.0;
    }
    println!(
        "  echo {:.1} dBFS, residual {:.1} dBFS, model background {:.1} dBFS, idle floor {:.1} dBFS ({} blocks)",
        m.echo_dbfs, m.residual_dbfs, m.background_dbfs, m.idle_noise_dbfs, m.noise_blocks
    );
    println!(
        "  ENR all blocks: raw {:.2} dB, idle floor subtracted {:.2} dB, background subtracted {}",
        m.enr_raw_db,
        m.enr_idle_db,
        show_est(&m.enr)
    );
    println!(
        "  ENR loudest quarter, raw (decision value): {}",
        show_est(&m.loud_enr)
    );
    for (what, value) in [
        ("the decision value", m.loud_enr.db),
        ("its interval's low end", m.loud_enr.low_db),
    ] {
        match choose_softclip(&table, value) {
            Some(c) => println!(
                "mildest softclip at least as severe as {what} ({value:.2} dB): {}",
                show(c)
            ),
            None => println!("no softclip in the grid is as severe as {what} ({value:.2} dB)"),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    //! Simulator-only checks of the estimator (deterministic: seeded scenarios, committed
    //! smoke speech); every test name contains `echo_nl`. The GATE-10 measurement over
    //! private data is `#[ignore]`d (AGENTS.md section 11).
    use super::*;
    use std::sync::OnceLock;

    fn file() -> &'static ScenarioFile {
        static F: OnceLock<ScenarioFile> = OnceLock::new();
        F.get_or_init(|| scenario_file(&crate::repo_root()).expect("scenario file"))
    }

    fn speech() -> &'static Speech {
        static S: OnceLock<Speech> = OnceLock::new();
        S.get_or_init(|| smoke_speech(&crate::repo_root()).expect("smoke speech"))
    }

    fn sim(scn: &Scenario) -> SimOutput {
        run_scenario(file(), scn, speech()).expect("simulates")
    }

    fn far_only(c: Option<f64>) -> &'static Measurement {
        static M: OnceLock<Vec<(Option<f64>, Measurement)>> = OnceLock::new();
        let all = M.get_or_init(|| {
            [None, Some(8.0), Some(4.0), Some(2.0)]
                .into_iter()
                .map(|c| {
                    let out = sim(&calibration_scenario(file(), c).unwrap());
                    let m = measure(
                        &out.mic,
                        &out.reference,
                        &Excluded::default(),
                        &Config::default(),
                    )
                    .expect("measures");
                    (c, m)
                })
                .collect()
        });
        &all.iter()
            .find(|(k, _)| *k == c)
            .expect("calibrated level")
            .1
    }

    /// Ground truth for a soft-clipped run: the same scenario without the clip has the
    /// identical reference and echo path, so the best scalar fit of the clipped echo onto
    /// the linear echo leaves exactly the distortion (Bussgang's linear part). Both are
    /// high-passed like the estimator's input.
    fn true_enr_db(softclip: f64) -> f64 {
        let fc = Config::default().highpass_hz;
        let lin = highpass(&sim(&calibration_scenario(file(), None).unwrap()).echo, fc);
        let clip = highpass(
            &sim(&calibration_scenario(file(), Some(softclip)).unwrap()).echo,
            fc,
        );
        let (a, c): (Vec<f64>, Vec<f64>) = (
            lin.iter().map(|&v| f64::from(v)).collect(),
            clip.iter().map(|&v| f64::from(v)).collect(),
        );
        let g = a.iter().zip(&c).map(|(p, q)| p * q).sum::<f64>()
            / a.iter().map(|p| p * p).sum::<f64>();
        let sig: f64 = a.iter().map(|p| (g * p).powi(2)).sum();
        let dist: f64 = a.iter().zip(&c).map(|(p, q)| (q - g * p).powi(2)).sum();
        db(sig) - db(dist)
    }

    #[test]
    fn echo_nl_finds_the_simulated_delay() {
        let out = sim(&calibration_scenario(file(), None).unwrap());
        let m = far_only(None);
        let truth = out.delay.delay_at(out.mic.len() / 2);
        assert!((m.delay - truth).abs() < 1.0, "{} vs {truth}", m.delay);
        let fitted: Vec<&Chunk> = m.chunks.iter().filter(|c| c.fitted).collect();
        assert!(fitted.len() >= 2, "{:?}", m.chunks);
        // The 250 ms simulated room response fits in the FIR: its last quarter (from
        // 368 ms after the bulk delay) is empty.
        assert!(fitted.iter().all(|c| c.tail_db < -40.0), "{:?}", m.chunks);
    }

    #[test]
    fn echo_nl_linear_echo_path_is_explained_almost_completely() {
        let m = far_only(None);
        // The estimator's own floor (FIR truncation, fit error) must sit far below the
        // distortion levels it has to tell apart.
        assert!(m.enr.db > 40.0, "linear path ENR {:?}", m.enr);
        assert!(
            m.loud_enr.db > 40.0,
            "linear path loud ENR {:?}",
            m.loud_enr
        );
        // (How many blocks the residual model keeps is not meaningful here: the residual
        // is fit error, not distortion.)
    }

    #[test]
    fn echo_nl_mapping_is_monotonic_in_the_soft_clip() {
        let levels = [None, Some(8.0), Some(4.0), Some(2.0)];
        for w in levels.windows(2) {
            let (a, b) = (far_only(w[0]), far_only(w[1]));
            for (x, y) in [(a.loud_enr.db, b.loud_enr.db), (a.enr.db, b.enr.db)] {
                assert!(
                    y < x - 3.0,
                    "ENR must fall as clipping gets harder: {:?} {x:.2} -> {:?} {y:.2}",
                    w[0],
                    w[1]
                );
            }
        }
    }

    #[test]
    fn echo_nl_matches_the_true_distortion_of_the_simulator() {
        for c in [8.0, 4.0, 2.0] {
            let m = far_only(Some(c));
            let truth = true_enr_db(c);
            // A time-invariant filter can only explain more of the clipped echo than the
            // scalar Bussgang gain, so the measurement may sit slightly above the truth.
            assert!(
                m.enr.db > truth - 1.0 && m.enr.db < truth + 2.0,
                "softclip {c}: measured {:.2} dB, true {truth:.2} dB",
                m.enr.db
            );
            // Noise-free: the model finds (almost) no background, and everything kept.
            assert!(
                m.far_only_s > 25.0,
                "softclip {c}: kept {:.1} s",
                m.far_only_s
            );
        }
    }

    #[test]
    fn echo_nl_rejects_double_talk() {
        let cfg = Config::default();
        for c in [4.0, 2.0] {
            let far = far_only(Some(c));
            let base = file().get("double-talk-ser-0").unwrap();
            let out = sim(&Scenario {
                softclip: Some(c),
                ..base.clone()
            });
            let m = measure(&out.mic, &out.reference, &Excluded::default(), &cfg).unwrap();
            // Near-end speech left in the kept blocks adds at most 1 dB to their residual.
            let near = highpass(&out.near_clean, cfg.highpass_hz);
            let b = cfg.block;
            let (mut pn, mut pr) = (0.0, 0.0);
            for (i, blk) in m.blocks.iter().enumerate() {
                if blk.class == BlockClass::FarOnly {
                    pn += power(&near[i * b..(i + 1) * b]);
                    pr += 10f64.powf(blk.residual_db / 10.0);
                }
            }
            assert!(
                pn < 0.2 * pr,
                "softclip {c}: near end {:.1} dB of the kept residual",
                db(pn / pr)
            );
            assert!(
                m.far_only_s >= 3.0,
                "softclip {c}: {:.1} s kept",
                m.far_only_s
            );
            for (x, y) in [(m.enr.db, far.enr.db), (m.loud_enr.db, far.loud_enr.db)] {
                assert!(
                    (x - y).abs() < 1.5,
                    "softclip {c}: double talk {x:.2} dB vs far-only {y:.2} dB"
                );
            }
        }
    }

    #[test]
    fn echo_nl_separates_the_noise_floor() {
        // Far end silent for 6 s, then far-only speech; noise throughout 15 dB below the
        // echo (about the level of the soft-clip distortion). The background-corrected ENR
        // must match the noise-free measurement, the model's background the idle floor
        // measured in the silent stretch, and the raw loud-block value may only err
        // towards more distortion.
        let cfg = Config::default();
        let c = 4.0;
        let far = far_only(Some(c));
        let base = calibration_scenario(file(), Some(c)).unwrap();
        let talk = sim(&Scenario {
            noise_snr_db: Some(15.0),
            ..base.clone()
        });
        let quiet = sim(&Scenario {
            far_speech: false,
            noise_snr_db: Some(15.0),
            duration_s: 6.0,
            seed: base.seed + 1,
            ..base.clone()
        });
        // Without speech the simulator scales noise to the echo target, as with speech.
        let mic: Vec<f32> = quiet.mic.iter().chain(&talk.mic).copied().collect();
        let lb: Vec<f32> = quiet
            .reference
            .iter()
            .chain(&talk.reference)
            .copied()
            .collect();
        let m = measure(&mic, &lb, &Excluded::default(), &cfg).unwrap();
        assert!(m.noise_blocks >= 10, "{} noise blocks", m.noise_blocks);
        assert!(
            m.enr_raw_db < far.enr.db - 1.0,
            "noise must show in the raw ratio: {:.2} dB",
            m.enr_raw_db
        );
        assert!(
            (m.enr.db - far.enr.db).abs() < 1.5,
            "background-corrected {:?} vs noise-free {:?}",
            m.enr,
            far.enr
        );
        assert!(
            (m.background_dbfs - m.idle_noise_dbfs).abs() < 1.0,
            "background {:.2} dBFS vs idle floor {:.2} dBFS",
            m.background_dbfs,
            m.idle_noise_dbfs
        );
        assert!(
            m.loud_enr.db <= far.loud_enr.db + 0.5,
            "noise must not make the loud blocks look cleaner: {:?} vs {:?}",
            m.loud_enr,
            far.loud_enr
        );
        assert!(m.enr.low_db <= m.enr.db && m.enr.db <= m.enr.high_db);
    }

    #[test]
    fn echo_nl_choose_softclip_rounds_towards_more_distortion() {
        let p = |softclip, loud_enr_db| CalibrationPoint {
            softclip,
            loud_enr_db,
            enr_db: f64::NAN,
        };
        let t = vec![p(None, 50.0), p(Some(8.0), 30.0), p(Some(4.0), 20.0)];
        assert_eq!(choose_softclip(&t, 60.0), Some(None));
        assert_eq!(choose_softclip(&t, 30.0), Some(Some(8.0)));
        assert_eq!(choose_softclip(&t, 29.9), Some(Some(4.0)));
        assert_eq!(choose_softclip(&t, 19.0), None);
    }

    #[test]
    fn echo_nl_highpass_removes_rumble_and_keeps_speech_band() {
        let tone = |f: f64| -> Vec<f32> {
            (0..32_000)
                .map(|i| (2.0 * std::f64::consts::PI * f * f64::from(i) / 16_000.0).sin() as f32)
                .collect()
        };
        let gain = |f: f64| {
            let x = tone(f);
            let y = highpass(&x, 120.0);
            db(power(&y[16_000..]) / power(&x[16_000..]))
        };
        assert!(gain(30.0) < -40.0, "30 Hz: {:.1} dB", gain(30.0));
        assert!(gain(1_000.0).abs() < 0.1, "1 kHz: {:.2} dB", gain(1_000.0));
    }

    #[test]
    fn echo_nl_track_gaps_become_exclusions() {
        let p = |track_frame, frames, discontinuity| earshot_core::capture::RawPacket {
            qpc: 0,
            frames,
            track_frame,
            trimmed_frames: 0,
            silent: false,
            discontinuity,
            timestamp_error: false,
        };
        let t = RawTrackTimeline {
            device_name: String::new(),
            wav_sample_rate: 48_000,
            wav_channels: 1,
            frames_written: 2_000,
            synthesized_silence_frames: 0,
            packets: vec![p(72, 480, true), p(552, 480, false), p(1_200, 480, false)],
            device_changes: Vec::new(),
            processing: earshot_core::capture::RawProcessing::Default,
        };
        assert_eq!(
            track_excluded(&t),
            vec![0..72, 72..552, 1_032..1_200, 1_680..2_000]
        );
        assert_eq!(to_16k(&(1_032..1_200), 48_000), 344..400);
        assert_eq!(to_16k(&(1..2), 48_000), 0..1);
    }

    /// Writes a 0.1 s recording (16 kHz mono mic, stereo loopback) whose
    /// `timestamps.json` has `version` and one mic device change with `extra` members.
    fn write_recording(dir: &Path, version: u32, extra: &str) {
        use earshot_core::audio::{SampleRate, WavFormat, write_wav};
        std::fs::create_dir_all(dir).unwrap();
        let rate = SampleRate::new(RATE_HZ).unwrap();
        let wav = |name: &str, channels: u16| {
            let f = std::fs::File::create(dir.join(name)).unwrap();
            let samples = vec![0.0; 1_600 * usize::from(channels)];
            write_wav(f, rate, channels, &samples, WavFormat::Float32).unwrap();
        };
        wav("mic.wav", 1);
        wav("loopback.wav", 2);
        let track = |channels: u16, changes: &str| {
            format!(
                r#"{{"device_name": "d", "wav_sample_rate": 16000, "wav_channels": {channels},
                   "frames_written": 1600, "synthesized_silence_frames": 0,
                   "packets": [{{"qpc": 0, "frames": 1600, "track_frame": 0,
                     "trimmed_frames": 0, "silent": false, "discontinuity": false,
                     "timestamp_error": false}}],
                   "device_changes": [{changes}]}}"#
            )
        };
        let change = format!(
            r#"{{"qpc": 5, "track_frame": 800, "reason": "default_device_changed",
                "device_name": "Headset (Hands-Free)", "device_format": null,
                "error": null{extra}}}"#
        );
        let json = format!(
            r#"{{"schema_version": {version}, "qpc_units_per_second": 10000000,
               "start_qpc": 0, "end_qpc": 1000000, "mic": {}, "loopback": {}}}"#,
            track(1, &change),
            track(2, "")
        );
        std::fs::write(dir.join("timestamps.json"), json).unwrap();
    }

    #[test]
    fn echo_nl_load_recording_reads_schema_v3_to_v5() {
        let root = std::env::temp_dir().join(format!("earshot-echo-nl-{}", std::process::id()));
        let v3 = root.join("v3");
        write_recording(&v3, 3, "");
        // A v4 hands-free warning and fields this reader does not know are ignored.
        let v4 = root.join("v4");
        write_recording(
            &v4,
            4,
            r#", "hands_free": {"sample_rate": 16000}, "unknown": 1"#,
        );
        // v5: a take whose mic was opened raw loads as any other.
        let v5 = root.join("v5");
        write_recording(&v5, 5, "");
        let ts = v5.join("timestamps.json");
        let json = std::fs::read_to_string(&ts).unwrap();
        let marked = json.replacen(
            r#""device_name": "d","#,
            r#""device_name": "d", "processing": "raw","#,
            1,
        );
        assert_ne!(marked, json, "the mic section was marked");
        std::fs::write(&ts, marked).unwrap();
        let v6 = root.join("v6");
        write_recording(&v6, 6, "");
        let (a, b, c, d) = (
            load_recording(&v3),
            load_recording(&v4),
            load_recording(&v5),
            load_recording(&v6),
        );
        let _ = std::fs::remove_dir_all(&root);
        let (a, b, c) = (a.unwrap(), b.unwrap(), c.unwrap());
        assert_eq!(a.excluded.mic, vec![800..801], "the device change");
        assert_eq!(a, b, "v3 and v4 load the same");
        assert_eq!(b, c, "v4 and a raw v5 take load the same");
        assert_eq!(b.loopback.len(), 1_600, "the stereo loopback mixed to mono");
        let err = format!("{:#}", d.unwrap_err());
        assert!(
            err.contains("unsupported timestamps.json schema 6"),
            "{err}"
        );
    }

    fn gate10_dir() -> std::path::PathBuf {
        crate::t613::gate12_private::recording_dir("EARSHOT_GATE10_DIR", &crate::repo_root(), |s| {
            &s.gate10.dir
        })
    }

    #[test]
    #[ignore = "needs the private GATE-10 recording: gate10.dir in .earshot-private/gate12.toml or EARSHOT_GATE10_DIR"]
    fn echo_nl_gate10_sets_the_scenario_soft_clip() {
        let dir = gate10_dir();
        let rec = load_recording(&dir)
            .unwrap_or_else(|e| panic!("GATE-10 recording at {}: {e:#}", dir.display()));
        let cfg = Config::default();
        let m = measure(&rec.mic, &rec.loopback, &rec.excluded, &cfg).unwrap();
        let ms = m.delay / f64::from(RATE_HZ) * 1_000.0;
        // The range the recording's strongest path stays in lives outside the repo.
        let path = crate::t613::gate12_private::default_path();
        let settings = crate::t613::gate12_private::load(path.as_deref())
            .unwrap_or_else(|e| panic!("private settings: {e}"))
            .expect("this test needs .earshot-private/gate12.toml in the home folder");
        let [low, high] = settings.gate10.delay_ms;
        assert!((low..high).contains(&ms), "bulk delay {ms:.2} ms");
        assert!(m.far_only_s > 20.0, "far-end only {:.1} s", m.far_only_s);
        assert!(m.loud_enr.low_db <= m.loud_enr.db && m.loud_enr.db <= m.loud_enr.high_db);
        // The gating scenarios use the mildest grid level at least as severe as the
        // measurement (acceptance/aec_scenarios.toml header).
        let configured = file().get("far-only").unwrap().softclip;
        let k = SOFTCLIP_GRID
            .iter()
            .position(|&c| c == configured)
            .expect("far-only soft clip is a calibration grid level");
        assert!(k > 0, "the measurement is not linear");
        let t = calibrate(file(), speech(), &SOFTCLIP_GRID[k - 1..=k], &cfg).unwrap();
        println!(
            "GATE-10 loud ENR {:?}; softclip {:?}: {:.2} dB, {:?}: {:.2} dB",
            m.loud_enr, t[0].softclip, t[0].loud_enr_db, t[1].softclip, t[1].loud_enr_db
        );
        assert!(
            t[1].loud_enr_db <= m.loud_enr.db,
            "softclip {configured:?} is milder than GATE-10"
        );
        assert!(
            t[0].loud_enr_db > m.loud_enr.db,
            "softclip {:?} would already be as severe as GATE-10",
            t[0].softclip
        );
    }

    #[test]
    #[ignore = "needs the private GATE-10 recording: gate10.dir in .earshot-private/gate12.toml or EARSHOT_GATE10_DIR"]
    fn echo_nl_gate10_canceller_erle_on_far_end_only_blocks() {
        // Informational (T1.5d review): the echo canceller's ERLE on the GATE-10 recording
        // over the blocks this estimator classifies as far-end only (double talk
        // rejected), next to the ERLE over the whole recording that `accept m1` reports.
        use earshot_core::aec::{BLOCK, EchoCanceller, EchoCancellerConfig};
        let dir = gate10_dir();
        let rec = load_recording(&dir)
            .unwrap_or_else(|e| panic!("GATE-10 recording at {}: {e:#}", dir.display()));
        let cfg = Config::default();
        let m = measure(&rec.mic, &rec.loopback, &rec.excluded, &cfg).unwrap();
        let n = rec.mic.len().min(rec.loopback.len()) / BLOCK * BLOCK;
        let mut aec = EchoCanceller::new(EchoCancellerConfig::default());
        let out = aec.process_vec(&rec.mic[..n], &rec.loopback[..n]);
        let out = &out[aec.output_latency()..];
        let (mut pm, mut po, mut far_s) = (0.0f64, 0.0f64, 0.0f64);
        for b in m.blocks.iter().filter(|b| b.class == BlockClass::FarOnly) {
            let start = (b.start_s * f64::from(RATE_HZ)).round() as usize;
            let r = start..(start + cfg.block).min(out.len());
            if r.is_empty() {
                continue;
            }
            pm += power(&rec.mic[r.clone()]) * r.len() as f64;
            po += power(&out[r.clone()]) * r.len() as f64;
            far_s += r.len() as f64 / f64::from(RATE_HZ);
        }
        println!(
            "GATE-10 canceller ERLE over far-end-only blocks: {:.2} dB ({far_s:.1} s)",
            db(pm) - db(po)
        );
        assert!(far_s > 20.0, "far-end only {far_s:.1} s");
    }
}
