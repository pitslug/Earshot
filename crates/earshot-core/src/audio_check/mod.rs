//! The audio check (T7.4j): a few short chirps are played through the chosen output while
//! the microphone and the output's loopback are captured on one device clock; this module
//! makes the chirps and judges what was captured. Pure: the host plays, captures, aligns
//! both tracks on the pipeline timeline and resamples them to [`PIPELINE_RATE`].
//!
//! What it measures, from the loopback (what was really played, `reference`) and the mic:
//! - **Delay**: the loopback-to-mic lag of the echo, in the delay estimator's convention
//!   (mic sample `n` hears reference sample `n - delay`), so it can seed
//!   [`crate::aec::EchoCanceller::set_delay_hint`] (a hint only seeds the search; the
//!   canceller never cancels on it). Found by the same generalised cross-correlation with
//!   phase transform the delay estimator uses (GCC-PHAT; Knapp and Carter, IEEE TASSP 24(4),
//!   1976), restricted to the chirps' band ([`CHIRP_START_HZ`] to [`CHIRP_END_HZ`]) so bins
//!   holding only noise carry no weight (the whitened correlation weights every frequency of
//!   the band alike, as the estimator's does). Each of the [`CHIRP_COUNT`] loudest stretches
//!   of the reference is correlated with the mic separately over lags `0..=MAX_LAG`; an echo
//!   is
//!   heard only when most stretches agree on one lag ([`AGREE`]) with a clear peak
//!   ([`MIN_PEAK_RATIO`]), so a noise peak never counts.
//! - **Echo level**: the echo's power at the mic (noise floor removed) relative to the
//!   played power over the same stretches, in dB, and its level at the mic in dBFS.
//! - **Headphones**: chirps were played (the loopback holds them) but no echo is heard.
//! - **Too quiet**: the mic delivered digital silence, or the echo was heard below
//!   [`TOO_QUIET_DBFS`] (too faint to trust the delay).
//! - **Clipping**: the mic reached full scale ([`ClipDetector`], run by the host on the
//!   mic's own samples before resampling, which would smooth the flat tops away).
//!
//! Every threshold here is a judgement written down before measuring, not a calibration.
//! Nothing here is used during a meeting.

use std::time::Duration;

use rustfft::FftPlanner;
use rustfft::num_complex::Complex;

use crate::audio::{PIPELINE_RATE, Sample, SampleRate};
use crate::dsp::{db, power};

#[cfg(test)]
mod tests;

/// Chirps played.
pub const CHIRP_COUNT: usize = 4;
/// Length of one chirp.
pub const CHIRP_LEN: Duration = Duration::from_millis(120);
/// Time from one chirp's start to the next one's. Chirps alternate rising and falling, so
/// the same sweep repeats every two periods (1.2 s), beyond [`MAX_LAG`]: a stretch's
/// correlation can never lock onto the echo of the next chirp (a rising and a falling sweep
/// barely correlate).
pub const CHIRP_PERIOD: Duration = Duration::from_millis(600);
/// Silence before the first chirp.
pub const LEAD_IN: Duration = Duration::from_millis(300);
/// Silence after the last chirp's period (room for the echo to arrive and decay).
pub const TAIL: Duration = Duration::from_millis(500);
/// Start frequency of the linear sweep (Hz).
pub const CHIRP_START_HZ: f64 = 300.0;
/// End frequency of the sweep (Hz); kept below the 8 kHz Nyquist limit of the pipeline
/// rate the analysis runs at.
pub const CHIRP_END_HZ: f64 = 6_000.0;
/// Peak amplitude of a chirp (-12 dBFS; a judgement: clearly audible at the system volume
/// without being loud, and far from clipping the output).
pub const CHIRP_PEAK: f32 = 0.25;
/// Raised-cosine fade at each end of a chirp (no click).
pub const CHIRP_FADE: Duration = Duration::from_millis(5);

/// Longest delay searched (1 s; wider than the canceller's 600 ms search so a long delay
/// is reported rather than missed).
pub const MAX_LAG: Duration = Duration::from_secs(1);
/// Stretches whose lags are within this of their median agree (5 ms; a judgement, widened from
/// 1 ms after a real run: a tablet's built-in speakers gave two arrivals about 3 ms apart with
/// each chirp's peak on either, so a clearly heard echo split two and two and read as
/// headphones). Which arrival the delay lands on can differ between runs; a hint only seeds
/// the canceller's search, so one on the other arrival gives no head start and does no harm.
pub const AGREE: Duration = Duration::from_millis(5);
/// A stretch's correlation peak must be at least this many times the median magnitude of
/// its correlation over all lags (a judgement: noise alone gives about 6.5 for 16 000
/// lags, the expected Gaussian maximum over the median).
pub const MIN_PEAK_RATIO: f64 = 10.0;
/// Agreeing stretches needed when [`CHIRP_COUNT`] were found (3 of 4); with fewer found,
/// every one of at least 2 must agree.
pub const MIN_AGREEING: usize = 3;
/// A reference whose loudest stretch is below this RMS (dBFS) was not played: the output
/// is muted for Earshot or the loopback heard nothing.
pub const NOT_PLAYED_DBFS: f64 = -60.0;
/// A mic whose whole recording is below this RMS (dBFS) delivers digital silence (muted,
/// or a privacy switch).
pub const MIC_SILENT_DBFS: f64 = -90.0;
/// An echo heard below this RMS at the mic (dBFS) is too quiet to trust (a judgement).
pub const TOO_QUIET_DBFS: f64 = -55.0;
/// A mic sample at or above this magnitude is at full scale.
pub const CLIP_LEVEL: f32 = 0.999;
/// This many consecutive full-scale mic samples are clipping (one is a loud peak).
pub const CLIP_RUN: usize = 3;
/// Percentile of the mic's 10 ms frame powers taken as its noise floor.
const NOISE_PERCENTILE: f64 = 0.2;
/// Stretches quieter than this, relative to the loudest, are not searched (30 dB).
const SEGMENT_FLOOR: f64 = 1e-3;

/// The whole test signal at `rate`: [`LEAD_IN`], then [`CHIRP_COUNT`] chirps
/// [`CHIRP_PERIOD`] apart, rising then falling in turn, then [`TAIL`]. Mono; the host
/// copies it to every output channel.
#[must_use]
pub fn test_signal(rate: SampleRate) -> Vec<Sample> {
    let lead = rate.frames_for(LEAD_IN);
    let period = rate.frames_for(CHIRP_PERIOD);
    let mut out = vec![0.0; lead + CHIRP_COUNT * period + rate.frames_for(TAIL)];
    let rising = chirp(rate, true);
    let falling = chirp(rate, false);
    for k in 0..CHIRP_COUNT {
        let at = lead + k * period;
        let one = if k % 2 == 0 { &rising } else { &falling };
        out[at..at + one.len()].copy_from_slice(one);
    }
    out
}

/// Duration of [`test_signal`].
#[must_use]
pub fn test_signal_duration() -> Duration {
    LEAD_IN + CHIRP_PERIOD * u32::try_from(CHIRP_COUNT).unwrap_or(u32::MAX) + TAIL
}

/// One linear chirp at `rate`: [`CHIRP_START_HZ`] to [`CHIRP_END_HZ`] (the end capped at
/// 45% of `rate`) over [`CHIRP_LEN`], or the other way round when not `rising`, peak
/// [`CHIRP_PEAK`], faded in and out over [`CHIRP_FADE`].
#[must_use]
pub fn chirp(rate: SampleRate, rising: bool) -> Vec<Sample> {
    let fs = f64::from(rate.hz());
    let n = rate.frames_for(CHIRP_LEN);
    let fade = rate.frames_for(CHIRP_FADE).max(1);
    let len_s = n as f64 / fs;
    let (low, high) = (CHIRP_START_HZ, CHIRP_END_HZ.min(0.45 * fs));
    let (f0, f1) = if rising { (low, high) } else { (high, low) };
    let k = (f1 - f0) / len_s;
    (0..n)
        .map(|i| {
            let t = i as f64 / fs;
            let phase = 2.0 * std::f64::consts::PI * (f0 * t + 0.5 * k * t * t);
            let edge = i.min(n - 1 - i);
            let w = if edge < fade {
                0.5 - 0.5 * (std::f64::consts::PI * edge as f64 / fade as f64).cos()
            } else {
                1.0
            };
            (f64::from(CHIRP_PEAK) * w * phase.sin()) as f32
        })
        .collect()
}

/// Finds runs of [`CLIP_RUN`] consecutive samples at [`CLIP_LEVEL`] or above, across
/// pushes (the host feeds it the mic's samples as they arrive, at the device rate).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ClipDetector {
    run: usize,
    clipped: bool,
}

impl ClipDetector {
    /// A detector that has seen nothing.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Looks at the next samples.
    pub fn push(&mut self, samples: &[Sample]) {
        for &s in samples {
            if s.abs() >= CLIP_LEVEL {
                self.run += 1;
                if self.run >= CLIP_RUN {
                    self.clipped = true;
                }
            } else {
                self.run = 0;
            }
        }
    }

    /// Whether any run so far was clipping.
    #[must_use]
    pub fn clipped(&self) -> bool {
        self.clipped
    }
}

/// What the check found.
#[derive(Debug, Clone, PartialEq)]
pub enum Outcome {
    /// The mic heard the chirps.
    Echo {
        /// Loopback-to-mic delay (samples at [`PIPELINE_RATE`]).
        delay_samples: f64,
        /// Echo power over played power (dB, negative: the echo is quieter).
        echo_db: f64,
        /// Echo RMS at the mic (dBFS).
        echo_dbfs: f64,
    },
    /// The chirps were played but the mic heard no echo: headphones (or the speaker's
    /// volume is off).
    NoEcho,
    /// The loopback holds no chirps: nothing was played on the output.
    NotPlayed,
    /// The mic delivered digital silence.
    MicSilent,
}

/// One searched stretch of the reference (diagnostics).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Stretch {
    /// First reference sample of the stretch.
    pub start: usize,
    /// Lag of its correlation peak (samples).
    pub lag: usize,
    /// Its peak over the median correlation magnitude.
    pub peak_ratio: f64,
}

/// The check's verdict.
#[derive(Debug, Clone, PartialEq)]
pub struct CheckResult {
    /// What was found.
    pub outcome: Outcome,
    /// The mic delivered silence, or the echo was below [`TOO_QUIET_DBFS`].
    pub too_quiet: bool,
    /// The mic reached full scale ([`ClipDetector`]).
    pub clipping: bool,
    /// The mic's noise floor (dBFS, [`NOISE_PERCENTILE`] of its 10 ms frame powers).
    pub noise_dbfs: f64,
    /// Every stretch searched, in time order.
    pub stretches: Vec<Stretch>,
}

impl CheckResult {
    /// The delay in milliseconds when an echo was heard.
    #[must_use]
    pub fn delay_ms(&self) -> Option<f64> {
        match self.outcome {
            Outcome::Echo { delay_samples, .. } => {
                Some(delay_samples * 1000.0 / f64::from(PIPELINE_RATE.hz()))
            }
            _ => None,
        }
    }

    /// The warm-start hint for the delay estimator (samples at [`PIPELINE_RATE`]): the
    /// delay when an echo was heard that was not too quiet and lies inside a search range
    /// of `max_delay` samples (the canceller's `DelayConfig::max_delay`); `None` otherwise.
    #[must_use]
    pub fn delay_hint(&self, max_delay: usize) -> Option<f64> {
        match self.outcome {
            Outcome::Echo { delay_samples, .. } => {
                usable_hint(delay_samples, self.too_quiet, max_delay)
            }
            _ => None,
        }
    }
}

/// The one hint rule ([`CheckResult::delay_hint`], and hosts reading a stored result): a
/// measured delay (samples at [`PIPELINE_RATE`]) seeds the canceller only when the echo was
/// not too quiet and the delay is finite, non-negative and at most `max_delay`.
#[must_use]
pub fn usable_hint(delay_samples: f64, too_quiet: bool, max_delay: usize) -> Option<f64> {
    (!too_quiet && delay_samples.is_finite() && (0.0..=max_delay as f64).contains(&delay_samples))
        .then_some(delay_samples)
}

/// Judges a capture: `reference` is the output's loopback and `mic` the microphone, both
/// mono at [`PIPELINE_RATE`] on the same timeline (sample `i` of each captured at the same
/// device time); `mic_clipped` is the host's [`ClipDetector`] verdict on the mic's own
/// samples. Works on any played signal with loud stretches, not only [`test_signal`]
/// (speech in a recorded call, for instance).
#[must_use]
pub fn analyse(reference: &[Sample], mic: &[Sample], mic_clipped: bool) -> CheckResult {
    let rate = PIPELINE_RATE;
    let w = rate.frames_for(CHIRP_LEN);
    let max_lag = rate.frames_for(MAX_LAG);
    let noise = noise_floor(mic, rate.frames_for(Duration::from_millis(10)));
    let mut result = CheckResult {
        outcome: Outcome::NotPlayed,
        too_quiet: false,
        clipping: mic_clipped,
        noise_dbfs: db(noise),
        stretches: Vec::new(),
    };
    if db(power(mic)) < MIC_SILENT_DBFS {
        result.outcome = Outcome::MicSilent;
        result.too_quiet = true;
        return result;
    }
    let starts = loudest_stretches(reference, w, CHIRP_COUNT);
    let loudest = starts.first().map_or(0.0, |&s| power(&reference[s..s + w]));
    if starts.is_empty() || db(loudest) < NOT_PLAYED_DBFS {
        return result;
    }
    let mut planner = FftPlanner::<f64>::new();
    let mut stretches: Vec<Stretch> = starts
        .iter()
        .map(|&start| {
            let (lag, peak_ratio) = correlate(
                &mut planner,
                &reference[start..start + w],
                mic.get(start..).unwrap_or(&[]),
                max_lag,
            );
            Stretch {
                start,
                lag,
                peak_ratio,
            }
        })
        .collect();
    stretches.sort_by_key(|s| s.start);
    result.stretches.clone_from(&stretches);

    let agree = rate.frames_for(AGREE);
    let mut lags: Vec<usize> = stretches.iter().map(|s| s.lag).collect();
    lags.sort_unstable();
    let median = lags[lags.len() / 2];
    let agreeing: Vec<&Stretch> = stretches
        .iter()
        .filter(|s| s.lag.abs_diff(median) <= agree && s.peak_ratio >= MIN_PEAK_RATIO)
        .collect();
    let needed = if stretches.len() >= CHIRP_COUNT {
        MIN_AGREEING
    } else {
        stretches.len().max(2)
    };
    if agreeing.len() < needed {
        result.outcome = Outcome::NoEcho;
        return result;
    }
    let mut agreed: Vec<usize> = agreeing.iter().map(|s| s.lag).collect();
    agreed.sort_unstable();
    let delay = agreed[agreed.len() / 2];
    // Echo and played power over the same stretches, the mic's window shifted by the delay.
    let (mut echo, mut played) = (0.0, 0.0);
    for s in &agreeing {
        let at = s.start + delay;
        let heard = mic.get(at..(at + w).min(mic.len())).unwrap_or(&[]);
        echo += (power(heard) - noise).max(0.0);
        played += power(&reference[s.start..s.start + w]);
    }
    let n = agreeing.len() as f64;
    let (echo, played) = (echo / n, played / n);
    let echo_dbfs = db(echo);
    result.too_quiet = echo_dbfs < TOO_QUIET_DBFS;
    result.outcome = Outcome::Echo {
        delay_samples: delay as f64,
        echo_db: db(echo) - db(played),
        echo_dbfs,
    };
    result
}

/// [`NOISE_PERCENTILE`] of the powers of consecutive `frame`-sample frames of `x`.
fn noise_floor(x: &[Sample], frame: usize) -> f64 {
    let mut powers: Vec<f64> = x.chunks(frame.max(1)).map(power).collect();
    if powers.is_empty() {
        return 0.0;
    }
    powers.sort_by(f64::total_cmp);
    let i = ((powers.len() - 1) as f64 * NOISE_PERCENTILE).round() as usize;
    powers[i]
}

/// Starts of up to `count` non-overlapping `w`-sample windows of `x` with the most energy,
/// loudest first; windows quieter than [`SEGMENT_FLOOR`] of the loudest are left out.
fn loudest_stretches(x: &[Sample], w: usize, count: usize) -> Vec<usize> {
    if w == 0 || x.len() < w {
        return Vec::new();
    }
    let mut prefix = Vec::with_capacity(x.len() + 1);
    prefix.push(0.0f64);
    for &v in x {
        let last = prefix[prefix.len() - 1];
        prefix.push(last + f64::from(v) * f64::from(v));
    }
    let energy = |s: usize| prefix[s + w] - prefix[s];
    let positions = x.len() - w + 1;
    let mut taken: Vec<usize> = Vec::new();
    let mut best_energy = None;
    for _ in 0..count {
        let free = |s: usize| taken.iter().all(|&t| s + w <= t || t + w <= s);
        let Some(best) = (0..positions)
            .filter(|&s| free(s))
            .max_by(|&a, &b| energy(a).total_cmp(&energy(b)).then(b.cmp(&a)))
        else {
            break;
        };
        let e = energy(best);
        let top = *best_energy.get_or_insert(e);
        if e <= 0.0 || e < top * SEGMENT_FLOOR {
            break;
        }
        taken.push(best);
    }
    taken
}

/// The lag in `0..=max_lag` where `x` correlates best with `y` (`y[n + lag]` against
/// `x[n]`, GCC-PHAT within the chirps' band, magnitude, so a reversed polarity is found
/// too) and its peak over the median magnitude. `y` shorter than `x.len() + max_lag` is
/// padded with silence.
fn correlate(
    planner: &mut FftPlanner<f64>,
    x: &[Sample],
    y: &[Sample],
    max_lag: usize,
) -> (usize, f64) {
    let ylen = x.len() + max_lag;
    let n = (ylen + x.len()).next_power_of_two();
    let fwd = planner.plan_fft_forward(n);
    let inv = planner.plan_fft_inverse(n);
    let mut a: Vec<Complex<f64>> = (0..n)
        .map(|i| Complex::new(x.get(i).map_or(0.0, |&v| f64::from(v)), 0.0))
        .collect();
    let mut b: Vec<Complex<f64>> = (0..n)
        .map(|i| {
            let v = if i < ylen { y.get(i).copied() } else { None };
            Complex::new(v.map_or(0.0, f64::from), 0.0)
        })
        .collect();
    fwd.process(&mut a);
    fwd.process(&mut b);
    let bin_hz = f64::from(PIPELINE_RATE.hz()) / n as f64;
    for (k, (p, q)) in a.iter_mut().zip(&b).enumerate() {
        let hz = k.min(n - k) as f64 * bin_hz;
        let g = p.conj() * q;
        let m = g.norm();
        *p = if (CHIRP_START_HZ..=CHIRP_END_HZ).contains(&hz) && m > 0.0 {
            g / m
        } else {
            Complex::new(0.0, 0.0)
        };
    }
    inv.process(&mut a);
    let mags: Vec<f64> = a[..=max_lag].iter().map(|c| c.re.abs()).collect();
    let (lag, peak) =
        mags.iter().copied().enumerate().fold(
            (0, 0.0),
            |best, (i, m)| if m > best.1 { (i, m) } else { best },
        );
    let mut sorted = mags;
    let mid = sorted.len() / 2;
    let median = *sorted.select_nth_unstable_by(mid, f64::total_cmp).1;
    let ratio = if median > 0.0 {
        peak / median
    } else if peak > 0.0 {
        f64::INFINITY
    } else {
        0.0
    };
    (lag, ratio)
}
