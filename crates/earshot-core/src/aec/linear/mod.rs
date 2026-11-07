//! Linear echo canceller: partitioned-block frequency-domain adaptive filter (PLAN.md T1.4).
//!
//! Multidelay block frequency-domain adaptive filter (MDF / PBFDAF): J.-S. Soo, K. K. Pang,
//! "Multidelay block frequency domain adaptive filter", IEEE TASSP 38(2), 1990; overlap-save
//! with a round-robin gradient constraint on one partition per block as in the Speex `mdf.c`
//! AUMDF variant (J.-M. Valin). Per-bin normalised step `mu / (sum_p |X_p(f)|^2 + delta)`:
//! each frequency bin is a `P`-tap complex NLMS filter normalised by its input power.
//!
//! Alignment: the [`DelayEstimator`] tracks the bulk delay `D`; the reference is read at
//! the fractional position `m - D(m) + pre` (windowed-sinc interpolation) so the direct
//! path lands `pre` taps into the filter. `D(m)` advances by the estimated drift, which is
//! resampling the **reference** at the drift ratio with zero buffering latency; the mic is
//! never delayed or resampled (PLAN.md 3.3). Output sample `n` is mic sample `n` minus the
//! echo estimate.
//!
//! Input samples that are not finite (NaN, infinity) are replaced by zero at the block
//! boundary, before they reach the delay estimator, the history or the filter: one bad
//! sample must not poison the filter for the rest of the lock.
//!
//! Without a live lock (no lock, or only a warm-start hint) the filter does not adapt and
//! the output is the mic. Divergence (output persistently louder than the mic) resets the filter.
//!
//! Stereo reference ([`LinearAec::process_block_stereo`], T1.5b): the two channels are
//! filtered as mid `(L + R) / 2` and side `(L - R) / 2` by two filters whose outputs add
//! (a two-channel MDF). Delay estimation and the mono path use the mid channel, so a
//! stereo reference with identical channels (side exactly zero) runs the mono canceller
//! unchanged. Stereo AEC has a non-uniqueness problem: with strongly correlated channels
//! many filter pairs cancel the echo and an NLMS update converges slowly and to a solution
//! that breaks when the far-end stereo image changes (M. M. Sondhi, D. R. Morgan,
//! J. L. Hall, "Stereophonic acoustic echo cancellation - an overview of the fundamental
//! problem", IEEE SPL 2(8), 1995). The usual mitigation of distorting the loudspeaker
//! signals (Benesty, Morgan, Sondhi 1998, half-wave rectifier) is not available here
//! because Earshot only captures what other applications play. The mitigation used is the
//! one that works on the receive side: the update is normalised per bin by the inverse of
//! the regularised 2x2 cross-power matrix of the two inputs (the generalised
//! multichannel frequency-domain adaptive filter of H. Buchner, J. Benesty, W. Kellermann,
//! "Generalized multichannel frequency-domain adaptive filtering: efficient realization
//! and application to hands-free speech communication", Signal Processing 85(3), 2005),
//! which decorrelates the channels in the update so the poorly excited difference
//! direction converges as fast as the common one. The matrix is regularised by
//! `stereo_regularisation` times the mean channel power (one channel silent makes mid
//! and side identical, and the inverse would amplify noise along their difference), and
//! the step is divided by the effective number of excited directions so the whole filter
//! keeps the single-channel NLMS step. With two loudspeakers the delay estimator's peak can
//! move between their paths; such a move within 10 ms keeps the
//! alignment ([`LinearAec::alignment_epoch`]).

use std::sync::Arc;

use rustfft::num_complex::Complex;
use rustfft::{Fft, FftPlanner};

use super::delay::{DelayConfig, DelayEstimator, DelayState};
use super::{ConfigError, require, within};

#[cfg(test)]
mod tests;

/// Samples per block; process lengths must be multiples of this.
pub const BLOCK: usize = 128;
const FFT_LEN: usize = 2 * BLOCK;
const BINS: usize = BLOCK + 1;

/// Fractional-delay interpolator: taps either side and table resolution.
const HALF: usize = 16;
const TAPS: usize = 2 * HALF;
const PHASES: usize = 512;
/// Smallest per-epoch pre-delay: covers the band-limited direct path's pre-ringing and
/// delay-estimate error.
const MIN_PRE: f64 = 20.0;
/// How far ahead the pre-delay is planned once the drift is known (seconds).
const REPLAN_HORIZON_S: f64 = 30.0;
/// Reference history (power of two) covering the largest delay plus interpolation.
const HIST: usize = 1 << 15;
/// With a stereo reference, a new lock within this many seconds of the applied delay is
/// the estimator moving between the two loudspeakers' paths (the mid reference correlates
/// with both, and which is stronger depends on the content), not a delay change.
const STEREO_PATH_WINDOW_S: f64 = 0.01;
/// The side filter starts when the side channel's power in the filter history reaches
/// this fraction of the mid channel's (-50 dB) and stops only below [`SIDE_OFF`]; between
/// the two the canceller keeps its mode, so near-identical channels cannot switch the
/// stereo path (and the double-talk detector that follows it) block by block.
const SIDE_ON: f32 = 1e-5;
/// Side-to-mid power ratio below which a running side filter stops (-60 dB).
const SIDE_OFF: f32 = 1e-6;

/// Linear stage settings.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LinearConfig {
    /// Filter partitions; filter length is `partitions * BLOCK` taps.
    pub partitions: usize,
    /// Scale of the adaptive per-bin step (Valin's residual-to-error ratio, at most 0.5
    /// per bin before scaling).
    pub step: f32,
    /// Fixed normalised step used until the filter has first adapted.
    pub initial_step: f32,
    /// Floor on the per-bin normalised step once adapted (keeps drift and path tracking).
    pub min_step: f32,
    /// Taps of pre-delay in front of the direct path (filter headroom for estimate error).
    pub pre: usize,
    /// Regularisation added to the per-bin power (relative to full-scale noise).
    pub regularisation: f32,
    /// Output/mic power ratio that counts as divergence.
    pub divergence_ratio: f32,
    /// Weight of each new estimate in the applied delay.
    pub delay_gain: f64,
    /// Partitions whose gradient is constrained each block (round robin; 1 = Speex AUMDF).
    pub constrain_per_block: usize,
    /// Regularisation of the 2x2 stereo normalisation, relative to the mean channel power
    /// per bin: bounds the step along a nearly unexcited direction.
    pub stereo_regularisation: f32,
    /// Delay estimator settings.
    pub delay: DelayConfig,
}

impl LinearConfig {
    /// Checks every setting, the delay estimator's included (the constructor panics on
    /// what this rejects).
    ///
    /// # Errors
    /// [`ConfigError`] naming the first setting out of range.
    pub fn validate(&self) -> Result<(), ConfigError> {
        self.delay.validate()?;
        require(
            (1..=256).contains(&self.partitions),
            "partitions must be in [1, 256]",
        )?;
        require(
            within(f64::from(self.step), 0.0, 4.0)
                && within(f64::from(self.initial_step), f64::MIN_POSITIVE, 1.0)
                && within(f64::from(self.min_step), 0.0, 1.0),
            "need step in [0, 4], initial_step in (0, 1], min_step in [0, 1]",
        )?;
        require(
            self.pre < self.partitions * BLOCK,
            "pre must be shorter than the filter",
        )?;
        require(
            self.delay.max_delay + self.pre + 2 * BLOCK + TAPS < HIST,
            "max delay exceeds the reference history",
        )?;
        require(
            within(f64::from(self.regularisation), f64::MIN_POSITIVE, 1.0),
            "regularisation must be in (0, 1]",
        )?;
        require(
            within(f64::from(self.divergence_ratio), 1.0, 1e6),
            "divergence_ratio must be in [1, 1e6]",
        )?;
        require(
            within(self.delay_gain, f64::MIN_POSITIVE, 1.0),
            "delay_gain must be in (0, 1]",
        )?;
        require(
            self.constrain_per_block >= 1,
            "constrain_per_block must be >= 1",
        )?;
        require(
            within(f64::from(self.stereo_regularisation), 0.0, 100.0),
            "stereo_regularisation must be in [0, 100]",
        )
    }
}

impl Default for LinearConfig {
    fn default() -> Self {
        Self {
            partitions: 32,
            step: 2.0,
            initial_step: 0.5,
            min_step: 0.0,
            pre: 48,
            regularisation: 1e-6,
            divergence_ratio: 4.0,
            delay_gain: 0.1,
            constrain_per_block: 1,
            stereo_regularisation: 0.3,
            delay: DelayConfig::default(),
        }
    }
}

/// Per-block view returned by [`LinearAec::process_block`].
#[derive(Debug, Clone, Copy)]
pub struct BlockOutput<'a> {
    /// Echo estimate for the block.
    pub echo: &'a [f32],
    /// Error (mic minus echo estimate) for the block.
    pub error: &'a [f32],
    /// Whether the filter is active (a delay is known).
    pub active: bool,
    /// Delay-aligned reference block (filter input; zeros when inactive). For a stereo
    /// reference the mid channel.
    pub aligned: &'a [f32],
    /// Delay-aligned side channel of a stereo reference (zeros otherwise).
    pub aligned_side: &'a [f32],
}

/// Streaming linear echo canceller; see the module docs.
pub struct LinearAec {
    cfg: LinearConfig,
    est: DelayEstimator,
    fft: Arc<dyn Fft<f32>>,
    ifft: Arc<dyn Fft<f32>>,
    interp: Vec<f32>,
    hist: Vec<f32>,
    /// Reference samples written so far (index of the next one).
    ref_count: u64,
    /// Mic samples processed so far.
    mic_count: u64,
    /// Applied bulk delay (samples) and its per-sample slope.
    applied: Option<f64>,
    slope: f64,
    /// Pre-delay (taps) in use for the current lock epoch.
    pre_eff: f64,
    /// The pre-delay has been re-planned for the drift in this epoch.
    replanned: bool,
    epoch: u64,
    /// Estimator epochs absorbed as stereo path changes, and the offset of the applied
    /// delay from the estimator's since the last one.
    absorbed: u64,
    path_offset: f64,
    last_cand_at: u64,
    prev_x: [f32; BLOCK],
    /// Reference spectra, newest at `head`.
    x_spec: Vec<Complex<f32>>,
    head: usize,
    w: Vec<Complex<f32>>,
    sx: Vec<f32>,
    /// Stereo (side channel): raw history, spectra, filter, the running sums
    /// `sum_p |S_p|^2` and `sum_p conj(M_p) S_p`, and whether a stereo reference was seen
    /// and the side filter is running.
    hist_s: Vec<f32>,
    prev_s: [f32; BLOCK],
    xs_spec: Vec<Complex<f32>>,
    ws: Vec<Complex<f32>>,
    s_ss: Vec<f32>,
    s_ms: Vec<Complex<f32>>,
    side_seen: bool,
    side_on: bool,
    /// Mid spectrum of the slot being overwritten (stereo running sums).
    old_mid: [Complex<f32>; BINS],
    blocks_since_resum: u32,
    constrain_next: usize,
    buf: Vec<Complex<f32>>,
    y_spec: Vec<Complex<f32>>,
    echo: [f32; BLOCK],
    error: [f32; BLOCK],
    p_err: f32,
    p_mic: f32,
    diverged_blocks: u32,
    resets: u64,
    /// Step control (Valin 2007): smoothed |E|^2 and |Y|^2 spectra, their covariance
    /// terms, and whether the initial fixed-step phase is over.
    eh: [f32; BINS],
    yh: [f32; BINS],
    pey: f32,
    pyy: f32,
    sum_adapt: f32,
    adapted: bool,
    leak: f32,
}

impl std::fmt::Debug for LinearAec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LinearAec")
            .field("cfg", &self.cfg)
            .field("applied", &self.applied)
            .field("resets", &self.resets)
            .finish_non_exhaustive()
    }
}

fn interp_table() -> Vec<f32> {
    // Phase p in [0, 1): weight of sample floor + j for j = -(HALF - 1)..=HALF at
    // fractional position p: Blackman-windowed sinc.
    let mut t = vec![0.0f32; (PHASES + 1) * TAPS];
    for p in 0..=PHASES {
        let frac = p as f64 / PHASES as f64;
        let mut sum = 0.0;
        for k in 0..TAPS {
            let j = k as f64 - (HALF as f64 - 1.0);
            let x = j - frac;
            let sinc = if x.abs() < 1e-12 {
                1.0
            } else {
                (std::f64::consts::PI * x).sin() / (std::f64::consts::PI * x)
            };
            let u = (x + HALF as f64) / (2.0 * HALF as f64);
            let w = if (0.0..=1.0).contains(&u) {
                0.42 - 0.5 * (2.0 * std::f64::consts::PI * u).cos()
                    + 0.08 * (4.0 * std::f64::consts::PI * u).cos()
            } else {
                0.0
            };
            t[p * TAPS + k] = (sinc * w) as f32;
            sum += sinc * w;
        }
        // Unity DC gain.
        for k in 0..TAPS {
            t[p * TAPS + k] /= sum as f32;
        }
    }
    t
}

impl LinearAec {
    /// Creates a canceller.
    ///
    /// # Panics
    /// If [`LinearConfig::validate`] rejects `cfg`.
    #[must_use]
    pub fn new(cfg: LinearConfig) -> Self {
        if let Err(e) = cfg.validate() {
            panic!("{e}");
        }
        let mut planner = FftPlanner::<f32>::new();
        let zero = Complex::new(0.0, 0.0);
        Self {
            est: DelayEstimator::new(cfg.delay),
            fft: planner.plan_fft_forward(FFT_LEN),
            ifft: planner.plan_fft_inverse(FFT_LEN),
            interp: interp_table(),
            hist: vec![0.0; HIST],
            ref_count: 0,
            mic_count: 0,
            applied: None,
            slope: 0.0,
            pre_eff: cfg.pre as f64,
            replanned: false,
            epoch: 0,
            absorbed: 0,
            path_offset: 0.0,
            last_cand_at: 0,
            prev_x: [0.0; BLOCK],
            x_spec: vec![zero; cfg.partitions * BINS],
            head: 0,
            w: vec![zero; cfg.partitions * BINS],
            sx: vec![0.0; BINS],
            hist_s: Vec::new(),
            prev_s: [0.0; BLOCK],
            xs_spec: Vec::new(),
            ws: Vec::new(),
            s_ss: Vec::new(),
            s_ms: Vec::new(),
            side_seen: false,
            side_on: false,
            old_mid: [zero; BINS],
            blocks_since_resum: 0,
            constrain_next: 0,
            buf: vec![zero; FFT_LEN],
            y_spec: vec![zero; BINS],
            echo: [0.0; BLOCK],
            error: [0.0; BLOCK],
            p_err: 0.0,
            p_mic: 0.0,
            diverged_blocks: 0,
            resets: 0,
            eh: [0.0; BINS],
            yh: [0.0; BINS],
            pey: 0.0,
            pyy: 0.0,
            sum_adapt: 0.0,
            adapted: false,
            leak: 0.0,
            cfg,
        }
    }

    /// The delay estimator (diagnostics).
    #[must_use]
    pub fn delay_estimator(&self) -> &DelayEstimator {
        &self.est
    }

    /// The delay estimator's state.
    #[must_use]
    pub fn delay_state(&self) -> DelayState {
        self.est.state()
    }

    /// Alignment epoch: changes whenever the reference is realigned to a newly locked delay
    /// (the delay estimator's epoch, except for stereo path changes, which keep the
    /// alignment and the filter).
    #[must_use]
    pub fn alignment_epoch(&self) -> u64 {
        self.est.epoch() - self.absorbed
    }

    /// Sets a warm-start delay hint (samples); see [`DelayEstimator::set_hint`].
    pub fn set_delay_hint(&mut self, delay: f64) {
        self.est.set_hint(delay);
    }

    /// Bulk delay currently applied to the reference (samples), if any.
    #[must_use]
    pub fn applied_delay(&self) -> Option<f64> {
        self.applied
    }

    /// Sum of squared filter coefficients (frequency domain); 0 until it adapts.
    #[must_use]
    pub fn filter_energy(&self) -> f64 {
        self.w
            .iter()
            .chain(&self.ws)
            .map(|c| f64::from(c.norm_sqr()))
            .sum()
    }

    /// True while the side (L - R) filter of a stereo reference is running.
    #[must_use]
    pub fn is_stereo(&self) -> bool {
        self.side_on
    }

    /// Leakage of the echo estimate into the error (Valin 2007): residual echo power in
    /// bin k is about `leak * |Y_k|^2`.
    #[must_use]
    pub fn leak(&self) -> f32 {
        self.leak
    }

    /// True once the fixed-step bootstrap after start or a reset is over.
    #[must_use]
    pub fn is_adapted(&self) -> bool {
        self.adapted
    }

    /// Number of divergence resets so far.
    #[must_use]
    pub fn resets(&self) -> u64 {
        self.resets
    }

    /// Clears the adaptive filter.
    pub fn reset_filter(&mut self) {
        let zero = Complex::new(0.0, 0.0);
        self.w.iter_mut().for_each(|c| *c = zero);
        self.ws.iter_mut().for_each(|c| *c = zero);
        self.resets += 1;
        self.adapted = false;
        self.sum_adapt = 0.0;
    }

    /// Processes equal-length mic and reference buffers whose length is a multiple of
    /// [`BLOCK`], writing the linear output (mic minus echo estimate) to `out`, with full
    /// adaptation. Output sample `n` corresponds to mic sample `n`.
    ///
    /// # Panics
    /// If the lengths differ or are not multiples of [`BLOCK`].
    pub fn process(&mut self, mic: &[f32], reference: &[f32], out: &mut [f32]) {
        assert!(
            mic.len() == reference.len() && mic.len() == out.len(),
            "mic, reference and out lengths differ"
        );
        assert!(
            mic.len().is_multiple_of(BLOCK),
            "length must be a multiple of BLOCK"
        );
        for ((m, r), o) in mic
            .as_chunks::<BLOCK>()
            .0
            .iter()
            .zip(reference.as_chunks::<BLOCK>().0)
            .zip(out.as_chunks_mut::<BLOCK>().0)
        {
            let b = self.process_block(m, r, 1.0);
            o.copy_from_slice(b.error);
        }
    }

    /// Processes one block of [`BLOCK`] samples with the adaptation step scaled by
    /// `adapt` (0 freezes the filter, for example during double talk).
    ///
    /// # Panics
    /// If a slice is not [`BLOCK`] long.
    pub fn process_block(&mut self, mic: &[f32], reference: &[f32], adapt: f32) -> BlockOutput<'_> {
        assert!(mic.len() == BLOCK && reference.len() == BLOCK, "block size");
        let (mic, reference) = (finite_block(mic), finite_block(reference));
        if self.side_seen {
            // Back to a mono reference: the side channel is silent from here on.
            self.write_side(&[0.0; BLOCK]);
        }
        self.process_mid(&mic, &reference, adapt)
    }

    /// Processes one block with a stereo reference (left and right channels); see the
    /// module docs. Output, delay tracking and the aligned reference (mid channel) are as
    /// for [`LinearAec::process_block`].
    ///
    /// # Panics
    /// If a slice is not [`BLOCK`] long.
    pub fn process_block_stereo(
        &mut self,
        mic: &[f32],
        left: &[f32],
        right: &[f32],
        adapt: f32,
    ) -> BlockOutput<'_> {
        assert!(
            mic.len() == BLOCK && left.len() == BLOCK && right.len() == BLOCK,
            "block size"
        );
        let (mic, left, right) = (finite_block(mic), finite_block(left), finite_block(right));
        let mut mid = [0.0f32; BLOCK];
        let mut side = [0.0f32; BLOCK];
        for (((m, s), &l), &r) in mid.iter_mut().zip(side.iter_mut()).zip(&left).zip(&right) {
            *m = 0.5 * (l + r);
            *s = 0.5 * (l - r);
        }
        if !self.side_seen && side.iter().any(|&v| v != 0.0) {
            let zero = Complex::new(0.0, 0.0);
            let n = self.cfg.partitions * BINS;
            self.side_seen = true;
            self.hist_s = vec![0.0; HIST];
            self.xs_spec = vec![zero; n];
            self.ws = vec![zero; n];
            self.s_ss = vec![0.0; BINS];
            self.s_ms = vec![zero; BINS];
        }
        if self.side_seen {
            self.write_side(&side);
        }
        self.process_mid(&mic, &mid, adapt)
    }

    /// Stores a side-channel block at the reference positions of the next mid block.
    fn write_side(&mut self, side: &[f32; BLOCK]) {
        for (i, &v) in side.iter().enumerate() {
            self.hist_s[((self.ref_count + i as u64) as usize) & (HIST - 1)] = v;
        }
    }

    fn process_mid(&mut self, mic: &[f32], reference: &[f32], adapt: f32) -> BlockOutput<'_> {
        self.est.push(mic, reference);
        for &v in reference {
            self.hist[(self.ref_count as usize) & (HIST - 1)] = v;
            self.ref_count += 1;
        }
        self.update_alignment();

        let Some(delay) = self.applied else {
            self.mic_count += BLOCK as u64;
            self.echo = [0.0; BLOCK];
            self.error.copy_from_slice(mic);
            return BlockOutput {
                echo: &self.echo,
                error: &self.error,
                active: false,
                aligned: &self.echo,
                aligned_side: &self.echo,
            };
        };

        // Aligned reference for this block (fractional read, drift applied per sample).
        // The read offset keeps the interpolator on samples already received; for very
        // short delays the pre-delay shrinks instead (direct path still at tap >= 0).
        let min_read = (HALF + 1) as f64;
        let pre = self.pre_eff;
        let mut x = [0.0f32; BLOCK];
        let mut xs = [0.0f32; BLOCK];
        for (i, (xv, sv)) in x.iter_mut().zip(xs.iter_mut()).enumerate() {
            let m = (self.mic_count + i as u64) as f64;
            let d = delay + self.slope * i as f64;
            let pos = m - (d - pre).max(min_read);
            *xv = self.read_ref(pos);
            if self.side_seen {
                *sv = read_hist(&self.interp, &self.hist_s, self.ref_count, pos);
            }
        }
        if self.slope != 0.0 {
            self.applied = Some(delay + self.slope * BLOCK as f64);
        }
        self.mic_count += BLOCK as u64;

        // X = FFT([prev block, this block]).
        let p = self.cfg.partitions;
        self.head = (self.head + p - 1) % p;
        for (b, &v) in self.buf.iter_mut().zip(self.prev_x.iter().chain(&x)) {
            *b = Complex::new(v, 0.0);
        }
        self.prev_x = x;
        self.fft.process(&mut self.buf);
        // Exact per-bin NLMS normaliser: running sum of |X|^2 over the P partitions
        // (the slot being overwritten holds the oldest spectrum).
        let base = self.head * BINS;
        if self.side_seen {
            self.old_mid
                .copy_from_slice(&self.x_spec[base..base + BINS]);
        }
        for k in 0..BINS {
            let xk = self.buf[k];
            let old = self.x_spec[base + k].norm_sqr();
            self.x_spec[base + k] = xk;
            self.sx[k] = (self.sx[k] - old + xk.norm_sqr()).max(0.0);
            if old > RESUM_RATIO * self.sx[k] {
                // The departing spectrum dwarfed the rest: the running sum lost its
                // precision to cancellation, so sum this bin exactly.
                self.sx[k] = (0..p).map(|q| self.x_spec[q * BINS + k].norm_sqr()).sum();
            }
        }
        if self.side_seen {
            for (b, &v) in self.buf.iter_mut().zip(self.prev_s.iter().chain(&xs)) {
                *b = Complex::new(v, 0.0);
            }
            self.prev_s = xs;
            self.fft.process(&mut self.buf);
            for (k, old_m) in self.old_mid.iter().enumerate() {
                let (sk, mk) = (self.buf[k], self.x_spec[base + k]);
                let old_s = self.xs_spec[base + k];
                self.xs_spec[base + k] = sk;
                self.s_ss[k] = (self.s_ss[k] - old_s.norm_sqr() + sk.norm_sqr()).max(0.0);
                self.s_ms[k] += mk.conj() * sk - old_m.conj() * old_s;
                if old_s.norm_sqr() > RESUM_RATIO * self.s_ss[k]
                    || old_m.norm_sqr() > RESUM_RATIO * self.sx[k]
                {
                    self.s_ss[k] = (0..p).map(|q| self.xs_spec[q * BINS + k].norm_sqr()).sum();
                    self.s_ms[k] = (0..p)
                        .map(|q| self.x_spec[q * BINS + k].conj() * self.xs_spec[q * BINS + k])
                        .sum();
                }
            }
            let (ss, mm): (f32, f32) = (self.s_ss.iter().sum(), self.sx.iter().sum());
            let on = if self.side_on { SIDE_OFF } else { SIDE_ON };
            self.side_on = ss > on * mm && ss > 0.0;
        }
        self.blocks_since_resum += 1;
        if self.blocks_since_resum >= 1_024 {
            // Re-sum occasionally so rounding cannot accumulate.
            self.blocks_since_resum = 0;
            for k in 0..BINS {
                self.sx[k] = (0..p).map(|q| self.x_spec[q * BINS + k].norm_sqr()).sum();
            }
            self.resum_side();
        }

        // Y = sum_p W_p X_{k-p}; echo estimate = last BLOCK samples of IFFT.
        self.y_spec
            .iter_mut()
            .for_each(|c| *c = Complex::new(0.0, 0.0));
        for part in 0..p {
            let xs = ((self.head + part) % p) * BINS;
            let ws = part * BINS;
            for k in 0..BINS {
                self.y_spec[k] += self.w[ws + k] * self.x_spec[xs + k];
            }
            if self.side_on {
                for k in 0..BINS {
                    self.y_spec[k] += self.ws[ws + k] * self.xs_spec[xs + k];
                }
            }
        }
        self.load_half_spectrum();
        self.ifft.process(&mut self.buf);
        let scale = 1.0 / FFT_LEN as f32;
        let mut pe = 0.0f32;
        let mut pm = 0.0f32;
        for (((y, e), &m), b) in self
            .echo
            .iter_mut()
            .zip(self.error.iter_mut())
            .zip(mic)
            .zip(&self.buf[BLOCK..])
        {
            *y = b.re * scale;
            *e = m - *y;
            pe += *e * *e;
            pm += m * m;
        }

        // Divergence: output persistently louder than the mic, or not finite at all (the
        // filter overflowed): reset at once, with the step-control statistics.
        self.p_err = 0.8 * self.p_err + 0.2 * pe;
        self.p_mic = 0.8 * self.p_mic + 0.2 * pm;
        if !(self.p_err.is_finite() && self.p_mic.is_finite()) {
            self.reset_filter();
            self.eh = [0.0; BINS];
            self.yh = [0.0; BINS];
            self.pey = 0.0;
            self.pyy = 0.0;
            self.diverged_blocks = 0;
            self.p_err = 0.0;
            self.p_mic = 0.0;
            for (e, (y, &m)) in self.error.iter_mut().zip(self.echo.iter_mut().zip(mic)) {
                *y = 0.0;
                *e = m;
            }
        } else if self.p_err > self.cfg.divergence_ratio * self.p_mic && self.p_err > 1e-9 {
            self.diverged_blocks += 1;
            if self.diverged_blocks >= 4 {
                self.reset_filter();
                self.diverged_blocks = 0;
                self.p_err = 0.0;
            }
        } else {
            self.diverged_blocks = 0;
        }

        // Always: the leakage estimate must keep running while adaptation is frozen.
        self.adapt(adapt);
        BlockOutput {
            echo: &self.echo,
            error: &self.error,
            active: true,
            aligned: &self.prev_x,
            aligned_side: &self.prev_s,
        }
    }

    /// Gradient step `W_p += mu_k conj(X_{k-p}) E / (sum_p |X_p|^2 + delta)` with
    /// `E = FFT([0, e])` and a per-bin step `mu_k` from the residual-echo-to-error ratio.
    ///
    /// Step control after J.-M. Valin, "On adjusting the learning rate in frequency domain
    /// echo cancellation with double-talk", IEEE TASLP 15(3), 2007 (as in the Speex MDF
    /// canceller): the leakage `eta` of the echo estimate into the error is the regression
    /// of the error power spectrum's fluctuations on the echo estimate's; the residual
    /// echo in bin k is then about `eta |Y_k|^2`, and
    /// `mu_k = min(eta |Y_k|^2, |E_k|^2 / 2) / |E_k|^2` (blended with the broadband ratio).
    /// Near-end speech, noise and nonlinear residue raise `|E_k|^2` but not
    /// `eta |Y_k|^2`, so they slow adaptation instead of disturbing it. Until the filter
    /// has adapted once, a fixed step bootstraps it.
    fn adapt(&mut self, scale: f32) {
        // Y = FFT([0, y]) and E = FFT([0, e]) power spectra.
        let mut yf = [0.0f32; BINS];
        for i in 0..BLOCK {
            self.buf[i] = Complex::new(0.0, 0.0);
            self.buf[BLOCK + i] = Complex::new(self.echo[i], 0.0);
        }
        self.fft.process(&mut self.buf);
        for (k, v) in yf.iter_mut().enumerate() {
            *v = self.buf[k].norm_sqr();
        }
        for i in 0..BLOCK {
            self.buf[i] = Complex::new(0.0, 0.0);
            self.buf[BLOCK + i] = Complex::new(self.error[i], 0.0);
        }
        self.fft.process(&mut self.buf);
        let p = self.cfg.partitions;
        let (mut see, mut syy, mut sey, mut sxx) = (0.0f32, 0.0f32, 0.0f32, 0.0f32);
        for i in 0..BLOCK {
            see += self.error[i] * self.error[i];
            syy += self.echo[i] * self.echo[i];
            sey += self.error[i] * self.echo[i];
            sxx += self.prev_x[i] * self.prev_x[i];
        }
        if self.side_on {
            sxx += self.prev_s.iter().map(|v| v * v).sum::<f32>();
        }
        // No far-end signal in this block: nothing to learn from.
        if sxx <= self.cfg.regularisation * BLOCK as f32 {
            return;
        }

        // Leakage estimate.
        let ss = 0.35 / p as f32;
        let (mut pey, mut pyy) = (0.0f32, 0.0f32);
        for (((b, eh), yh), &y) in self.buf[..BINS]
            .iter()
            .zip(self.eh.iter_mut())
            .zip(self.yh.iter_mut())
            .zip(&yf)
        {
            let ef = b.norm_sqr();
            *eh = (1.0 - ss) * *eh + ss * ef;
            *yh = (1.0 - ss) * *yh + ss * y;
            let (de, dy) = (ef - *eh, y - *yh);
            pey += de * dy;
            pyy += dy * dy;
        }
        self.pey = (1.0 - ss) * self.pey + ss * pey;
        self.pyy = (1.0 - ss) * self.pyy + ss * pyy;
        let pyy_floor = self.pyy.max(1e-20);
        let leak = (self.pey.max(0.005 * pyy_floor) / pyy_floor).min(1.0);
        self.leak = leak;
        if scale <= 0.0 {
            // Frozen (double talk): keep estimating the leakage, do not adapt.
            return;
        }

        // Broadband residual-to-error ratio, bounded by the error/echo correlation.
        let see_f = see.max(1e-12);
        let rer = ((1e-4 * sxx + 3.0 * leak * syy) / see_f)
            .min(sey * sey / (1e-12 + see_f * syy))
            .min(0.5);

        let delta = self.cfg.regularisation * (p * FFT_LEN) as f32;
        // Step per bin (before normalisation).
        let mut m = [0.0f32; BINS];
        if self.adapted {
            for (k, mk) in m.iter_mut().enumerate() {
                let e = self.buf[k].norm_sqr() + 1e-12;
                let r = (leak * yf[k]).min(0.5 * e);
                let r = 0.7 * r + 0.3 * rer * e;
                let mu = (self.cfg.step * r / e).clamp(self.cfg.min_step, 1.0);
                *mk = scale * mu;
            }
        } else {
            let mu = self.cfg.initial_step * scale;
            m = [mu; BINS];
            self.sum_adapt += mu;
            if self.sum_adapt > p as f32 {
                self.adapted = true;
            }
        }
        if self.side_on {
            self.adapt_stereo(&m, delta);
            self.constrain();
            return;
        }
        let mut g = [0.0f32; BINS];
        for (k, gk) in g.iter_mut().enumerate() {
            *gk = m[k] / (self.sx[k] + delta);
        }
        for part in 0..p {
            let xs = ((self.head + part) % p) * BINS;
            let ws = part * BINS;
            for (((w, x), b), &gk) in self.w[ws..ws + BINS]
                .iter_mut()
                .zip(&self.x_spec[xs..xs + BINS])
                .zip(&self.buf[..BINS])
                .zip(&g)
            {
                *w += x.conj() * b * gk;
            }
        }
        self.constrain();
    }

    /// Two-channel update with the per-bin inverse of the regularised cross-power matrix
    /// `Q = sum_p conj(x_p) x_p^T`, `x_p = (M_p, S_p)` (Buchner, Benesty, Kellermann 2005):
    /// `(W_m, W_s) += m_k Q^-1 (conj(M_p) E, conj(S_p) E)`. `self.buf` holds `E`.
    fn adapt_stereo(&mut self, m: &[f32; BINS], delta: f32) {
        let p = self.cfg.partitions;
        // Per bin: Q^-1 entries (mm, ss real; ms complex; sm = conj(ms)), times the step.
        let mut inv = [(0.0f32, 0.0f32, Complex::new(0.0f32, 0.0)); BINS];
        for (k, v) in inv.iter_mut().enumerate() {
            let reg = delta + self.cfg.stereo_regularisation * 0.5 * (self.sx[k] + self.s_ss[k]);
            let (a, c, b) = (self.sx[k] + reg, self.s_ss[k] + reg, self.s_ms[k]);
            let det = (a * c - b.norm_sqr()).max(1e-3 * a * c);
            // Every excited direction of the whitened update reduces the error by a full
            // step, so the step is shared among them: divide by the effective number of
            // directions tr(Q (Q + reg I)^-1) = 2 - reg tr((Q + reg I)^-1) (1 for a
            // single channel, 2 for two uncorrelated ones), which keeps the a-posteriori
            // error of the whole filter as in single-channel NLMS.
            let dirs = (2.0 - reg * (a + c) / det).clamp(1.0, 2.0);
            let g = m[k] / (det * dirs);
            *v = (g * c, g * a, -b * g);
        }
        for part in 0..p {
            let xs = ((self.head + part) % p) * BINS;
            let ws = part * BINS;
            for (k, &(imm, iss, ims)) in inv.iter().enumerate() {
                let e = self.buf[k];
                let um = self.x_spec[xs + k].conj() * e;
                let us = self.xs_spec[xs + k].conj() * e;
                self.w[ws + k] += um * imm + us * ims;
                self.ws[ws + k] += um * ims.conj() + us * iss;
            }
        }
    }

    /// Gradient constraint (keep the first BLOCK taps only) on `constrain_per_block`
    /// partitions per block, round robin; both filters when stereo.
    fn constrain(&mut self) {
        let p = self.cfg.partitions;
        for _ in 0..self.cfg.constrain_per_block.clamp(1, p) {
            let part = self.constrain_next;
            self.constrain_next = (self.constrain_next + 1) % p;
            let ws = part * BINS;
            let mut w = std::mem::take(&mut self.w);
            self.constrain_partition(&mut w[ws..ws + BINS]);
            self.w = w;
            if self.side_on {
                let mut w = std::mem::take(&mut self.ws);
                self.constrain_partition(&mut w[ws..ws + BINS]);
                self.ws = w;
            }
        }
    }

    fn constrain_partition(&mut self, w: &mut [Complex<f32>]) {
        self.y_spec.copy_from_slice(w);
        self.load_half_spectrum();
        self.ifft.process(&mut self.buf);
        let scale = 1.0 / FFT_LEN as f32;
        for i in 0..FFT_LEN {
            self.buf[i] = if i < BLOCK {
                Complex::new(self.buf[i].re * scale, 0.0)
            } else {
                Complex::new(0.0, 0.0)
            };
        }
        self.fft.process(&mut self.buf);
        w.copy_from_slice(&self.buf[..BINS]);
    }

    /// Recomputes the side running sums from the stored spectra.
    fn resum_side(&mut self) {
        if !self.side_seen {
            return;
        }
        let p = self.cfg.partitions;
        for k in 0..BINS {
            self.s_ss[k] = (0..p).map(|q| self.xs_spec[q * BINS + k].norm_sqr()).sum();
            self.s_ms[k] = (0..p)
                .map(|q| self.x_spec[q * BINS + k].conj() * self.xs_spec[q * BINS + k])
                .sum();
        }
    }

    /// Fills `buf` with the Hermitian spectrum whose first half is `y_spec`.
    fn load_half_spectrum(&mut self) {
        self.buf[..BINS].copy_from_slice(&self.y_spec);
        for k in 1..BLOCK {
            self.buf[FFT_LEN - k] = self.y_spec[k].conj();
        }
    }

    /// Reference value at fractional sample index `pos` (windowed-sinc interpolation).
    /// Positions outside the stored history read as zero.
    fn read_ref(&self, pos: f64) -> f32 {
        read_hist(&self.interp, &self.hist, self.ref_count, pos)
    }

    /// Follows the delay estimator: (re)acquire on a new epoch, track within an epoch.
    fn update_alignment(&mut self) {
        let state = self.est.state();
        match state {
            DelayState::NoLock => {
                if self.applied.is_some() {
                    // Echo path gone: stop cancelling and forget the filter.
                    self.applied = None;
                    self.reset_filter();
                    self.clear_history();
                }
            }
            DelayState::Hint { .. } => {
                // A hint is not evidence of an echo path: never cancel on it.
                if self.applied.is_some() {
                    self.applied = None;
                    self.reset_filter();
                    self.clear_history();
                }
            }
            DelayState::Locked { delay, .. } => {
                let epoch = self.est.epoch();
                let window = STEREO_PATH_WINDOW_S * f64::from(self.cfg.delay.rate_hz);
                if epoch != self.epoch
                    && self.side_on
                    && let Some(a) = self.applied
                    && (delay - a).abs() <= window
                {
                    // Stereo: the lock moved to the other loudspeaker's path. The filter
                    // covers both; keep the alignment, anchored on the earlier path (the
                    // later one always fits behind it, an earlier one might not fit in
                    // front of the pre-delay): moving to an earlier path re-reads the
                    // reference later and shifts the filter by the same taps (exact).
                    self.absorbed += epoch - self.epoch;
                    self.epoch = epoch;
                    if delay < a {
                        let min_read = (HALF + 1) as f64;
                        let pre = (delay - min_read - 8.0).clamp(MIN_PRE, self.pre_eff);
                        let old_off = (a - self.pre_eff).max(min_read);
                        let target_off = (delay - pre).max(min_read);
                        // Whole taps, so the shift is exact; the remainder stays in the
                        // applied delay (within a sample of the new path's).
                        let taps = (old_off - target_off).floor().max(0.0);
                        self.applied = Some(old_off - taps + pre);
                        self.pre_eff = pre;
                        self.shift_filter(-(taps as isize));
                        self.rebuild_history();
                        self.path_offset = 0.0;
                    } else {
                        self.path_offset = a - delay;
                    }
                } else if epoch != self.epoch || self.applied.is_none() {
                    self.path_offset = 0.0;
                    // New lock or a jump: realign; the room response is usually unchanged
                    // (a delay jump), so keep the filter but drop misaligned history.
                    self.epoch = epoch;
                    self.applied = Some(delay);
                    // Pre-delay for this epoch: the configured headroom, reduced for short
                    // delays so the read offset stays above the interpolator limit (a
                    // clamped offset would let the echo slide through the filter).
                    let pre_old = self.pre_eff;
                    self.pre_eff =
                        (delay - (HALF + 1) as f64 - 8.0).clamp(MIN_PRE, self.cfg.pre as f64);
                    // The kept filter must keep the room response at the same distance
                    // behind the pre-delay: a larger pre-delay moves it later (whole taps,
                    // exact), so a jump from a short to a long delay keeps cancelling.
                    let taps = (self.pre_eff - pre_old).round();
                    if taps != 0.0 {
                        self.shift_filter(-(taps as isize));
                    }
                    self.slope = 0.0;
                    self.replanned = false;
                    self.clear_history();
                } else if let Some(c) = self.est.last_candidate()
                    && c.at != self.last_cand_at
                {
                    self.last_cand_at = c.at;
                    if let Some(a) = self.applied {
                        let g = self.cfg.delay_gain;
                        self.applied = Some(a + g * (delay + self.path_offset - a));
                    }
                }
                if let Some(ppm) = self.est.drift_ppm() {
                    // Fast render clock (positive ppm) shrinks the delay.
                    self.slope = -ppm * 1e-6;
                    if !self.replanned {
                        self.replanned = true;
                        self.replan_pre();
                    }
                }
            }
        }
    }

    /// Once the drift is known: if the delay, projected [`REPLAN_HORIZON_S`] ahead, would
    /// push the read offset under the interpolator limit, lower the pre-delay now and
    /// shift the filter left by the same whole number of taps (exact, in the time domain),
    /// so the echo keeps its place in the filter instead of sliding later.
    fn replan_pre(&mut self) {
        let Some(delay) = self.applied else {
            return;
        };
        let min_read = (HALF + 1) as f64;
        let horizon = REPLAN_HORIZON_S * f64::from(self.cfg.delay.rate_hz);
        let lowest = delay + self.slope.min(0.0) * horizon;
        let wanted = (lowest - min_read).clamp(MIN_PRE, self.pre_eff);
        let shift = (self.pre_eff - wanted).floor();
        if shift < 1.0 {
            return;
        }
        self.pre_eff -= shift;
        self.shift_filter_left(shift as usize);
        self.rebuild_history();
    }

    /// Recomputes the reference spectra of the last `partitions + 1` blocks for the current
    /// delay, slope and pre-delay (re-reading the stored raw reference), so a change of
    /// alignment needs no re-filling of the filter history.
    fn rebuild_history(&mut self) {
        let Some(delay) = self.applied else {
            return;
        };
        let p = self.cfg.partitions;
        let min_read = (HALF + 1) as f64;
        let now = self.mic_count as f64;
        let side = self.side_seen;
        let mut blocks = vec![[0.0f32; BLOCK]; p + 1];
        let mut side_blocks = vec![[0.0f32; BLOCK]; if side { p + 1 } else { 0 }];
        for q in 0..=p {
            let start = now - ((q + 1) * BLOCK) as f64;
            for i in 0..BLOCK {
                let m = start + i as f64;
                if m < 0.0 {
                    continue;
                }
                let d = delay + self.slope * (m - now);
                let pos = m - (d - self.pre_eff).max(min_read);
                blocks[q][i] = self.read_ref(pos);
                if side {
                    side_blocks[q][i] = read_hist(&self.interp, &self.hist_s, self.ref_count, pos);
                }
            }
        }
        for q in 0..p {
            let base = ((self.head + q) % p) * BINS;
            self.spectrum_of(&blocks[q + 1], &blocks[q]);
            self.x_spec[base..base + BINS].copy_from_slice(&self.buf[..BINS]);
            if side {
                self.spectrum_of(&side_blocks[q + 1], &side_blocks[q]);
                self.xs_spec[base..base + BINS].copy_from_slice(&self.buf[..BINS]);
            }
        }
        self.prev_x = blocks[0];
        for k in 0..BINS {
            self.sx[k] = (0..p).map(|q| self.x_spec[q * BINS + k].norm_sqr()).sum();
        }
        if side {
            self.prev_s = side_blocks[0];
            self.resum_side();
        }
    }

    /// `self.buf = FFT([prev, cur])`.
    fn spectrum_of(&mut self, prev: &[f32; BLOCK], cur: &[f32; BLOCK]) {
        for (b, &v) in self.buf.iter_mut().zip(prev.iter().chain(cur)) {
            *b = Complex::new(v, 0.0);
        }
        self.fft.process(&mut self.buf);
    }

    /// Moves the filters' impulse responses `taps` earlier (dropping their first taps).
    fn shift_filter_left(&mut self, taps: usize) {
        self.shift_filter(taps as isize);
    }

    /// Moves the filters' impulse responses `taps` earlier (positive: dropping their first
    /// taps) or later (negative: dropping their last taps).
    fn shift_filter(&mut self, taps: isize) {
        let mut w = std::mem::take(&mut self.w);
        self.shift_by(&mut w, taps);
        self.w = w;
        if self.side_seen {
            let mut w = std::mem::take(&mut self.ws);
            self.shift_by(&mut w, taps);
            self.ws = w;
        }
    }

    fn shift_by(&mut self, w: &mut [Complex<f32>], taps: isize) {
        let p = self.cfg.partitions;
        let mut h = vec![0.0f32; p * BLOCK];
        let scale = 1.0 / FFT_LEN as f32;
        for part in 0..p {
            let ws = part * BINS;
            self.y_spec.copy_from_slice(&w[ws..ws + BINS]);
            self.load_half_spectrum();
            self.ifft.process(&mut self.buf);
            for (t, v) in h[part * BLOCK..(part + 1) * BLOCK].iter_mut().enumerate() {
                *v = self.buf[t].re * scale;
            }
        }
        let len = h.len();
        let n = taps.unsigned_abs().min(len);
        if taps >= 0 {
            h.copy_within(n.., 0);
            h[len - n..].iter_mut().for_each(|v| *v = 0.0);
        } else {
            h.copy_within(..len - n, n);
            h[..n].iter_mut().for_each(|v| *v = 0.0);
        }
        for part in 0..p {
            for (i, b) in self.buf.iter_mut().enumerate() {
                let v = if i < BLOCK { h[part * BLOCK + i] } else { 0.0 };
                *b = Complex::new(v, 0.0);
            }
            self.fft.process(&mut self.buf);
            let ws = part * BINS;
            w[ws..ws + BINS].copy_from_slice(&self.buf[..BINS]);
        }
    }

    fn clear_history(&mut self) {
        let zero = Complex::new(0.0, 0.0);
        self.x_spec.iter_mut().for_each(|c| *c = zero);
        self.prev_x = [0.0; BLOCK];
        if self.side_seen {
            self.xs_spec.iter_mut().for_each(|c| *c = zero);
            self.prev_s = [0.0; BLOCK];
            self.s_ss.iter_mut().for_each(|v| *v = 0.0);
            self.s_ms.iter_mut().for_each(|c| *c = zero);
            self.side_on = false;
        }
    }
}

/// Value of the history `hist` (ring of [`HIST`] samples, `ref_count` written so far) at
/// fractional sample index `pos` (windowed-sinc interpolation). Positions outside the
/// stored history read as zero.
fn read_hist(interp: &[f32], hist: &[f32], ref_count: u64, pos: f64) -> f32 {
    let fl = pos.floor();
    let frac = pos - fl;
    let base = fl as i64 - (HALF as i64 - 1);
    let newest = ref_count as i64 - 1;
    let oldest = ref_count as i64 - HIST as i64;
    // Linear interpolation between the two nearest table phases.
    let ph = frac * PHASES as f64;
    let p0 = (ph.floor() as usize).min(PHASES - 1);
    let t = (ph - p0 as f64) as f32;
    let row0 = &interp[p0 * TAPS..(p0 + 1) * TAPS];
    let row1 = &interp[(p0 + 1) * TAPS..(p0 + 2) * TAPS];
    let mut acc = 0.0f32;
    for k in 0..TAPS {
        let idx = base + k as i64;
        if idx >= 0 && idx >= oldest && idx <= newest {
            let c = row0[k] + t * (row1[k] - row0[k]);
            acc += c * hist[(idx as usize) & (HIST - 1)];
        }
    }
    acc
}

/// A running power sum is recomputed exactly when the spectrum leaving it held more than
/// this multiple of what remains (f32 cancellation would leave mostly rounding error).
const RESUM_RATIO: f32 = 1e4;

/// Largest input magnitude passed on (60 dB above full scale): absurd finite samples
/// would otherwise overflow the NLMS normaliser's power sums (to infinity, which stops
/// adaptation until the periodic re-sum, about 8 s).
const INPUT_LIMIT: f32 = 1e3;

/// Copy of a [`BLOCK`]-long slice with every non-finite sample replaced by zero and every
/// other sample limited to [`INPUT_LIMIT`] in magnitude.
pub(crate) fn finite_block(x: &[f32]) -> [f32; BLOCK] {
    let mut out = [0.0f32; BLOCK];
    for (o, &v) in out.iter_mut().zip(x) {
        *o = if v.is_finite() {
            v.clamp(-INPUT_LIMIT, INPUT_LIMIT)
        } else {
            0.0
        };
    }
    out
}
