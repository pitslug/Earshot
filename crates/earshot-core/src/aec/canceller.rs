//! Complete echo canceller: linear stage, double-talk detector, residual echo suppressor
//! and optional comfort noise (PLAN.md T1.5).
//!
//! Per block of [`BLOCK`] samples:
//!
//! 1. The linear stage ([`LinearAec`]) produces the echo estimate `y`, the error
//!    `e = mic - y` and the aligned reference `x`, adapting only when the double-talk
//!    detector of the previous block saw no near-end speech.
//! 2. Spectra of the last two blocks (Hann window) give per-bin powers `E`, `Y`, `X`, `D`.
//! 3. Residual echo estimate per bin: `max(eta, rho_k) Y_k`, where `eta` is the linear
//!    stage's leakage estimate (Valin 2007; the residual echo Speex's preprocessor
//!    suppresses) and `rho_k` the residual-to-estimate ratio learned while only the far
//!    end talks (it captures nonlinear loudspeaker residue that follows `Y`). In the cold
//!    phase (start, or after a linear-filter reset, until the linear stage first cancels
//!    6 dB) it is raised to `beta_k Xr_k`: `Xr` is the reference power with an exponential
//!    reverberation tail and `beta_k` the residual-to-reference gain learned while only
//!    the far end talks (the echo-power-from-reference idea of WebRTC AEC3's residual echo
//!    estimator). Below 125 Hz the postfilter also predicts the clipping products of
//!    voiced speech from the cubed reference (T1.6c; see [`super::postfilter`]).
//! 4. Double talk: total error power well above the residual echo estimate plus the noise
//!    floor (with a hangover) stops the learning of `rho` and `beta` and softens the
//!    suppression; much larger excess (`freeze_ratio`) also freezes linear adaptation.
//!    For a stereo reference the decision is taken by the normalised cross-correlation
//!    statistic of [`super::coherence`] (Benesty, Morgan, Cho 2000), which does not use
//!    the linear echo estimate (T1.5d). For a mono reference the same statistic only
//!    vetoes: while the reference explains the mic almost entirely (late reverberation at
//!    far-end offsets the linear stage has not cancelled), a call neither freezes the
//!    linear stage nor softens the suppression (T1.5f).
//! 5. Mismatch safeguard: once the linear stage has converged, an output louder than the
//!    mic means the echo path has gone (for example after switching to headphones), and
//!    the mic passes through unchanged, until the linear stage cancels again or the delay
//!    estimator locks onto a new delay (a new alignment epoch is evidence of a new path).
//! 6. Residual echo suppression: the STFT postfilter ([`super::postfilter`]), with a
//!    nonlinear echo estimate and a decision-directed Wiener gain. It adds a fixed output
//!    latency of [`EchoCanceller::output_latency`] samples (PLAN.md 3.3: the linear stage
//!    stays zero-latency, the postfilter may add one fixed latency of at most 32 ms).
//!    Without live evidence of an echo path, and in bypass, its gain is one: the output is
//!    the mic delayed by that latency.
//!
//! A stereo reference ([`EchoCanceller::process_stereo`], T1.5b) changes only step 1: the
//! linear stage runs its two-channel filter and reports the mid channel as `x`.
//!
//! [`EchoCancellerConfig::highpass`] (T6.23b, see [`super::highpass`]) high-passes the
//! inputs before step 1: with [`HighPass::Audio`] every step and the output see the
//! filtered mic; with [`HighPass::Internal`] steps 1-6 run on the filtered signals and only
//! the output is rebuilt on the full band from `A(mic)`, the matching all-pass of the mic
//! (linear output `A(mic) - y`, the postfilter's gains applied to its spectrum, and `A(mic)`
//! itself in bypass or without an echo path).

use std::sync::Arc;

use rustfft::num_complex::Complex;
use rustfft::{Fft, FftPlanner};

use super::coherence::{Coherence, CoherenceConfig};
use super::delay::DelayState;
use super::highpass::{AllPassFilter, HighPass, HighPassFilter};
use super::linear::{BLOCK, LinearAec, LinearConfig, finite_block};
use super::postfilter::{self, Frame, Postfilter, PostfilterConfig};
use super::{ConfigError, require, within};

const N: usize = 2 * BLOCK;
/// The canceller's sample rate (Hz).
const RATE_HZ: u32 = 16_000;
const BINS: usize = BLOCK + 1;

/// Canceller settings.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct EchoCancellerConfig {
    /// Linear stage.
    pub linear: LinearConfig,
    /// Residual echo suppressor.
    pub postfilter: PostfilterConfig,
    /// Double-talk statistic independent of the linear estimate (T1.5d).
    pub coherence: CoherenceConfig,
    /// `xi^2` below which near-end speech is assumed (see [`super::coherence`]).
    pub coherence_threshold: f32,
    /// Mono reference: `xi^2` at or above which a double-talk call of the linear witness
    /// neither freezes the linear stage nor softens the suppression; it still stops the
    /// models from learning (T1.5f). The statistic never declares double talk for a mono
    /// reference.
    pub mono_coherence_veto: f32,
    /// Error-to-(residual echo + noise) power ratio above which near-end speech is assumed.
    pub doubletalk_ratio: f32,
    /// Blocks the double-talk decision is held after the last detection.
    pub doubletalk_hangover: u32,
    /// Error-to-(residual echo + noise) power ratio above which linear adaptation is
    /// frozen (at or above `doubletalk_ratio`).
    pub freeze_ratio: f32,
    /// Mic-to-echo-estimate power ratio above which the mic counts as not explained by
    /// the echo (a near-end talker adds power the echo estimate lacks).
    pub unexplained_ratio: f32,
    /// Linear-output-to-mic power ratio that counts as an echo-path mismatch (bypass).
    pub mismatch_ratio: f32,
    /// Consecutive blocks the mismatch condition must hold before the bypass engages: an
    /// echo path that is gone stays gone, while the linear estimate overshooting a
    /// clipped echo burst lasts a few blocks and is left to the suppressor.
    pub mismatch_blocks: u32,
    /// Mic/echo-estimate normalised correlation below which the mic may hold near-end
    /// speech (gain-insensitive, so a misadjusted filter alone does not trigger it).
    pub doubletalk_correlation: f32,
    /// Linear-stage ERLE (power ratio) at which the linear stage counts as converged:
    /// the cold-phase model is dropped and double-talk detection starts.
    pub converged_ratio: f32,
    /// Double-talk detection runs once the linear stage has cancelled in this epoch and
    /// still cancels at least this power ratio (a weaker bar than `converged_ratio`).
    pub detector_min_erle: f32,
    /// Keep the reference-based residual model after convergence (learned only while
    /// the mic is explained by the echo estimate).
    pub ref_model_always: bool,
    /// Apply the residual echo suppressor (false leaves only the linear stage and the
    /// mismatch safeguard; for diagnostics).
    pub suppress: bool,
    /// High-pass filter of the canceller's inputs (T6.23b; [`super::highpass`]); default
    /// [`DEFAULT_HIGHPASS`].
    pub highpass: HighPass,
}

impl EchoCancellerConfig {
    /// Checks every setting, the stages' included. [`EchoCanceller::new`] panics on what
    /// this rejects; callers taking settings from users validate first.
    ///
    /// # Errors
    /// [`ConfigError`] naming the first setting out of range.
    pub fn validate(&self) -> Result<(), ConfigError> {
        self.linear.validate()?;
        self.postfilter.validate()?;
        self.coherence.validate()?;
        let pos = |x: f32| within(f64::from(x), f64::MIN_POSITIVE, 1e6);
        require(
            within(f64::from(self.coherence_threshold), 0.0, 1.0),
            "coherence_threshold must be in [0, 1]",
        )?;
        require(
            within(f64::from(self.mono_coherence_veto), 0.0, 1.0),
            "mono_coherence_veto must be in [0, 1]",
        )?;
        require(
            pos(self.doubletalk_ratio)
                && pos(self.freeze_ratio)
                && self.freeze_ratio >= self.doubletalk_ratio,
            "need 0 < doubletalk_ratio <= freeze_ratio",
        )?;
        require(
            pos(self.unexplained_ratio) && pos(self.mismatch_ratio),
            "unexplained_ratio and mismatch_ratio must be > 0",
        )?;
        require(
            within(f64::from(self.doubletalk_correlation), 0.0, 1.0),
            "doubletalk_correlation must be in [0, 1]",
        )?;
        require(
            within(f64::from(self.converged_ratio), 3.0, 1e6)
                && within(f64::from(self.detector_min_erle), 1.0, 1e6),
            "need converged_ratio >= 3 (its release level) and detector_min_erle >= 1",
        )?;
        require(self.mismatch_blocks >= 1, "mismatch_blocks must be >= 1")?;
        self.highpass.validate()
    }
}

impl EchoCancellerConfig {
    /// These settings with `highpass` instead.
    #[must_use]
    pub fn with_highpass(self, highpass: HighPass) -> Self {
        Self { highpass, ..self }
    }
}

impl Default for EchoCancellerConfig {
    fn default() -> Self {
        Self {
            linear: LinearConfig::default(),
            postfilter: PostfilterConfig::default(),
            coherence: CoherenceConfig::default(),
            coherence_threshold: 0.6,
            mono_coherence_veto: MONO_COHERENCE_VETO,
            doubletalk_ratio: 2.0,
            doubletalk_hangover: 8,
            freeze_ratio: 16.0,
            unexplained_ratio: 1.75,
            mismatch_ratio: 2.0,
            mismatch_blocks: 8,
            doubletalk_correlation: 0.8,
            converged_ratio: 6.0,
            detector_min_erle: 1.5,
            ref_model_always: true,
            suppress: true,
            highpass: DEFAULT_HIGHPASS,
        }
    }
}

/// Diagnostics of the last processed block.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BlockStatus {
    /// Delay estimator state.
    pub delay: DelayState,
    /// Near-end speech detected (suppression held back, models not learned).
    pub doubletalk: bool,
    /// Mono reference: `doubletalk` is held, but the coherence veto (T1.5f) kept it from
    /// freezing the linear stage and softening the suppression in this block (it still
    /// stops the models from learning).
    pub vetoed: bool,
    /// Strong near-end speech: linear adaptation frozen for the next block.
    pub frozen: bool,
    /// The mic was passed through because the linear output was louder than it.
    pub bypass: bool,
    /// Mean suppression gain over bins (1 = none).
    pub mean_gain: f32,
    /// The linear stage cancels at least 6 dB.
    pub converged: bool,
    /// Cold phase: the reference-based residual model is in use.
    pub cold: bool,
    /// Double-talk detector terms of the last block: [converged gate, mic power above the
    /// echo estimate, low mic/echo-estimate correlation, error above the residual].
    pub detector: [bool; 4],
    /// Normalised cross-correlation statistic `xi^2` of the last block (see
    /// [`super::coherence`]; 1 while not measured).
    pub coherence: f32,
}

/// An echo estimate more than 20 dB below its long-term level (while the reference is
/// active) is not evidence of an echo-path mismatch.
const MISMATCH_RELEVANCE: f32 = 1e-2;

/// Linear-output-to-mic block power ratio (20 dB) at which the mismatch bypass engages
/// after [`MISMATCH_GONE_BLOCKS`] instead of `mismatch_blocks`.
const MISMATCH_GONE_RATIO: f32 = 100.0;
/// Consecutive blocks of that before the bypass engages (a single block can be a
/// momentary misprediction).
const MISMATCH_GONE_BLOCKS: u32 = 3;

/// Stereo scale of the linear-stage ERLE bars (`converged_ratio` and its release level):
/// the two-channel filter has twice the coefficients, so under the same nonlinear residue
/// its steady-state misadjustment is about twice the mono filter's (B. Widrow,
/// S. D. Stearns, "Adaptive Signal Processing", 1985: LMS misadjustment grows with the
/// number of taps) and its ERLE about 3 dB lower. Safe only because stereo double talk is
/// detected by the coherence statistic, not by the linear estimate (T1.5d; with the linear
/// witness, a lower bar made far-end speech look like double talk).
const STEREO_ERLE_SCALE: f32 = 0.5;

/// Weight of the reference-based residual model in stereo double talk. Stereo double talk
/// is also flagged before the two-channel linear stage converges, where the leakage term
/// alone underestimates the residual echo (T1.5d; mono double talk is flagged only once
/// the linear stage cancels, and keeps weight 0).
const STEREO_DOUBLETALK_REF_WEIGHT: f32 = 0.1;

/// Default [`EchoCancellerConfig::highpass`] (T6.23b): a 30 Hz
/// second-order Butterworth high-pass of mic and reference before the canceller, so the
/// app's cleaned mic is high-passed at 30 Hz. A real recording's mic held non-echo power
/// almost all below 30 Hz that kept the linear
/// stage from being declared converged in talk-over; the filter changes the passband from
/// 80 Hz up by at most 0.09 dB and 1.2 ms of group delay. Chosen by the round-3 rule of
/// private notes. The owner accepted its trades (follow-up T6.23c): held-out `delay-jump`
/// recovery 2.5 -> 4.7 s (1.7 s since T6.23c ends the mismatch bypass on a new lock epoch)
/// and double-talk SER 0 SI-SDR 9.95 -> 9.25 dB, and two T1.5
/// invariants that hold only on the unfiltered canceller (near-end-pause leak -5.9 dB
/// against -6 dB; postfilter 12.65 dB against the linear stage's 13.17 dB in double talk).
/// Only sessions with a reference run the canceller: in-room capture (no loopback) has no
/// canceller and its mic stays unfiltered.
pub const DEFAULT_HIGHPASS: HighPass = HighPass::Audio { cutoff_hz: 30.0 };

/// Default [`EchoCancellerConfig::mono_coherence_veto`] (T1.5f). On `double-talk-ser-0`
/// the false mono double-talk calls in near-end pauses (late reverberation at far-end
/// offsets that the linear stage had not cancelled) came with `xi^2` of 0.72 to 0.80, while
/// the stereo detector already treats `xi^2` below 0.6 as near-end speech. Measured at 0.7
/// against no veto: worst 32 ms near-end-pause leak +0.2 -> -9.1 dB (seed 103) and
/// -1.4 -> -11.4 dB (seed 1103); near-end SI-SDR at SER 0 11.88 -> 11.97, 9.87 -> 10.01
/// and 10.78 -> 10.71 dB (seeds 103, 1103, 2103); mono robustness-grid minimum 10.12 ->
/// 10.85 dB (single points down by up to 0.85 dB). A veto at 0.8 left the 6.37 s leak in place.
const MONO_COHERENCE_VETO: f32 = 0.7;

/// Streaming acoustic echo canceller; see the module docs.
pub struct EchoCanceller {
    cfg: EchoCancellerConfig,
    lin: LinearAec,
    fft: Arc<dyn Fft<f32>>,
    window: [f32; N],
    prev: [[f32; BLOCK]; 4],
    post: Postfilter,
    base_was_mic: bool,
    /// Smoothed mic and linear-output powers over far-end-only blocks (their ratio is
    /// the linear stage's ERLE).
    acc_d: f32,
    acc_e: f32,
    /// The linear stage cancels at least 6 dB (hysteresis down to 3 dB).
    converged: bool,
    /// A bypass happened and the linear stage has not cancelled since.
    suspended: bool,
    /// Cold phase (start, or after a linear-filter reset) until the linear stage first
    /// cancels 6 dB: only then is the reference-based model used and learned.
    cold: bool,
    seen_resets: u64,
    /// The linear stage has cancelled (converged) since the current lock was acquired.
    cancelled_in_epoch: bool,
    epoch: u64,
    /// Sticky mismatch bypass and the count of cancelling blocks since it began.
    bypassing: bool,
    cancel_blocks: u32,
    mismatch_run: u32,
    gone_run: u32,
    /// Residual-to-echo-estimate power ratio per bin, learned while only the far end talks.
    rho: [f32; BINS],
    noise: [f32; BINS],
    p_e: f32,
    p_d: f32,
    p_y: f32,
    /// Long-term echo-estimate power while the reference is active (mismatch relevance).
    echo_level: f32,
    /// Smoothed mic x echo-estimate cross power (normalised correlation for double talk).
    p_dy: f32,
    detector: [bool; 4],
    coh: Coherence,
    coh_epoch: u64,
    dt_hold: u32,
    freeze_hold: u32,
    doubletalk: bool,
    buf: Vec<Complex<f32>>,
    status: BlockStatus,
    last_linear: [f32; BLOCK],
    /// High-pass filters of the mic, the reference and the second reference channel.
    hp: Option<[HighPassFilter; 3]>,
    /// Internal high-pass: the all-pass the output's mic goes through.
    allpass: Option<AllPassFilter>,
    /// Decision high-pass: filters of the mic, the echo estimate and the aligned mid and
    /// side references.
    dec: Option<[HighPassFilter; 4]>,
}

impl std::fmt::Debug for EchoCanceller {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EchoCanceller")
            .field("cfg", &self.cfg)
            .field("status", &self.status)
            .finish_non_exhaustive()
    }
}

impl EchoCanceller {
    /// Creates a canceller.
    ///
    /// # Panics
    /// If [`EchoCancellerConfig::validate`] rejects `cfg`.
    #[must_use]
    pub fn new(cfg: EchoCancellerConfig) -> Self {
        if let Err(e) = cfg.validate() {
            panic!("{e}");
        }
        let mut planner = FftPlanner::<f32>::new();
        let mut window = [0.0f32; N];
        for (i, w) in window.iter_mut().enumerate() {
            *w = 0.5 - 0.5 * (2.0 * std::f32::consts::PI * i as f32 / N as f32).cos();
        }
        let mut post = Postfilter::new(cfg.postfilter);
        if let HighPass::Decisions {
            cutoff_hz,
            postfilter: true,
        } = cfg.highpass
        {
            // The analysis is the filtered copy of the base: no split term.
            let h = HighPassFilter::new(cutoff_hz, RATE_HZ);
            post.set_analysis_response(|hz| (h.power_response(hz, RATE_HZ), 0.0));
        }
        if let HighPass::Internal { cutoff_hz } = cfg.highpass {
            let h = HighPassFilter::new(cutoff_hz, RATE_HZ);
            let a = AllPassFilter::matching(cutoff_hz, RATE_HZ);
            post.set_analysis_response(|hz| {
                let hf = h.response(hz, RATE_HZ);
                (hf.norm_sqr(), (hf - a.response(hz, RATE_HZ)).norm_sqr())
            });
        }
        Self {
            lin: LinearAec::new(cfg.linear),
            fft: planner.plan_fft_forward(N),
            window,
            prev: [[0.0; BLOCK]; 4],
            post,
            base_was_mic: true,
            acc_d: 0.0,
            acc_e: 0.0,
            converged: false,
            suspended: false,
            cold: true,
            seen_resets: 0,
            cancelled_in_epoch: false,
            epoch: 0,
            bypassing: false,
            cancel_blocks: 0,
            mismatch_run: 0,
            gone_run: 0,
            rho: [0.0; BINS],
            // Minimum tracking from above: the first block sets the floor.
            noise: [1e9; BINS],
            p_e: 0.0,
            p_d: 0.0,
            p_y: 0.0,
            echo_level: 0.0,
            p_dy: 0.0,
            detector: [false; 4],
            coh: Coherence::new(cfg.coherence),
            coh_epoch: 0,
            dt_hold: 0,
            freeze_hold: 0,
            doubletalk: false,
            buf: vec![Complex::new(0.0, 0.0); N],
            last_linear: [0.0; BLOCK],
            hp: match cfg.highpass {
                HighPass::Audio { cutoff_hz } | HighPass::Internal { cutoff_hz } => {
                    Some([HighPassFilter::new(cutoff_hz, RATE_HZ); 3])
                }
                _ => None,
            },
            dec: match cfg.highpass {
                HighPass::Decisions { cutoff_hz, .. } => {
                    Some([HighPassFilter::new(cutoff_hz, RATE_HZ); 4])
                }
                _ => None,
            },
            allpass: match cfg.highpass {
                HighPass::Internal { cutoff_hz } => {
                    Some(AllPassFilter::matching(cutoff_hz, RATE_HZ))
                }
                _ => None,
            },
            status: BlockStatus {
                delay: DelayState::NoLock,
                doubletalk: false,
                vetoed: false,
                frozen: false,
                bypass: true,
                mean_gain: 1.0,
                converged: false,
                cold: true,
                detector: [false; 4],
                coherence: 1.0,
            },
            cfg,
        }
    }

    /// Sets a warm-start delay hint (samples at 16 kHz), for example from the audio check.
    pub fn set_delay_hint(&mut self, delay: f64) {
        self.lin.set_delay_hint(delay);
    }

    /// Diagnostics of the last block.
    #[must_use]
    pub fn status(&self) -> BlockStatus {
        self.status
    }

    /// Fixed output latency in samples: output sample `n + output_latency()` belongs to
    /// mic sample `n` (the postfilter's STFT; the linear stage adds none). Downstream
    /// timestamps shift by it.
    #[must_use]
    pub fn output_latency(&self) -> usize {
        postfilter::LATENCY
    }

    /// The linear stage (for diagnostics).
    #[must_use]
    pub fn linear(&self) -> &LinearAec {
        &self.lin
    }

    /// Cancels echo: `mic` and `reference` are equal-length 16 kHz buffers whose length is
    /// a multiple of [`BLOCK`]; `out` receives the same number of samples, delayed by
    /// [`EchoCanceller::output_latency`] (sample `n + latency` belongs to mic sample `n`).
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
            self.process_block(m, r, o);
        }
    }

    /// Like [`EchoCanceller::process`], also writing the linear stage's output (mic minus
    /// echo estimate, before suppression, not delayed) to `linear` (for `aec.erle_linear`).
    ///
    /// # Panics
    /// As [`EchoCanceller::process`], or if `linear` has a different length.
    pub fn process_with_linear(
        &mut self,
        mic: &[f32],
        reference: &[f32],
        out: &mut [f32],
        linear: &mut [f32],
    ) {
        assert_eq!(linear.len(), mic.len(), "linear output length differs");
        assert!(
            mic.len() == reference.len() && mic.len() == out.len(),
            "mic, reference and out lengths differ"
        );
        assert!(
            mic.len().is_multiple_of(BLOCK),
            "length must be a multiple of BLOCK"
        );
        for (((m, r), o), l) in mic
            .as_chunks::<BLOCK>()
            .0
            .iter()
            .zip(reference.as_chunks::<BLOCK>().0)
            .zip(out.as_chunks_mut::<BLOCK>().0)
            .zip(linear.as_chunks_mut::<BLOCK>().0)
        {
            self.process_block(m, r, o);
            *l = self.last_linear;
        }
    }

    /// Like [`EchoCanceller::process`] with a stereo reference (left and right loopback
    /// channels): the linear stage runs the two-channel filter of
    /// [`LinearAec::process_block_stereo`]; with identical channels it is the mono
    /// canceller fed that channel.
    ///
    /// # Panics
    /// If the lengths differ or are not multiples of [`BLOCK`].
    pub fn process_stereo(&mut self, mic: &[f32], left: &[f32], right: &[f32], out: &mut [f32]) {
        assert!(
            mic.len() == left.len() && mic.len() == right.len() && mic.len() == out.len(),
            "mic, reference and out lengths differ"
        );
        assert!(
            mic.len().is_multiple_of(BLOCK),
            "length must be a multiple of BLOCK"
        );
        for (((m, l), r), o) in mic
            .as_chunks::<BLOCK>()
            .0
            .iter()
            .zip(left.as_chunks::<BLOCK>().0)
            .zip(right.as_chunks::<BLOCK>().0)
            .zip(out.as_chunks_mut::<BLOCK>().0)
        {
            self.process_block_ref(m, l, Some(r), o);
        }
    }

    /// Like [`EchoCanceller::process_stereo`], also writing the linear stage's output to
    /// `linear` (see [`EchoCanceller::process_with_linear`]).
    ///
    /// # Panics
    /// As [`EchoCanceller::process_stereo`], or if `linear` has a different length.
    pub fn process_stereo_with_linear(
        &mut self,
        mic: &[f32],
        left: &[f32],
        right: &[f32],
        out: &mut [f32],
        linear: &mut [f32],
    ) {
        assert_eq!(linear.len(), mic.len(), "linear output length differs");
        assert!(
            mic.len() == left.len() && mic.len() == right.len() && mic.len() == out.len(),
            "mic, reference and out lengths differ"
        );
        assert!(
            mic.len().is_multiple_of(BLOCK),
            "length must be a multiple of BLOCK"
        );
        for ((((m, l), r), o), lin) in mic
            .as_chunks::<BLOCK>()
            .0
            .iter()
            .zip(left.as_chunks::<BLOCK>().0)
            .zip(right.as_chunks::<BLOCK>().0)
            .zip(out.as_chunks_mut::<BLOCK>().0)
            .zip(linear.as_chunks_mut::<BLOCK>().0)
        {
            self.process_block_ref(m, l, Some(r), o);
            *lin = self.last_linear;
        }
    }

    /// Like [`EchoCanceller::process`], allocating the output.
    ///
    /// # Panics
    /// As [`EchoCanceller::process`].
    #[must_use]
    pub fn process_vec(&mut self, mic: &[f32], reference: &[f32]) -> Vec<f32> {
        let mut out = vec![0.0; mic.len()];
        self.process(mic, reference, &mut out);
        out
    }

    /// Power spectrum (Hann window) of `[prev, cur]` into `out`.
    fn spectrum(&mut self, prev: &[f32; BLOCK], cur: &[f32], out: &mut [f32; BINS]) {
        for (i, b) in self.buf.iter_mut().enumerate() {
            let v = if i < BLOCK { prev[i] } else { cur[i - BLOCK] };
            *b = Complex::new(v * self.window[i], 0.0);
        }
        self.fft.process(&mut self.buf);
        for (o, b) in out.iter_mut().zip(&self.buf) {
            *o = b.norm_sqr();
        }
    }

    fn process_block(
        &mut self,
        mic: &[f32; BLOCK],
        reference: &[f32; BLOCK],
        out: &mut [f32; BLOCK],
    ) {
        self.process_block_ref(mic, reference, None, out);
    }

    /// One block; `right` is the second reference channel of a stereo reference.
    fn process_block_ref(
        &mut self,
        mic: &[f32; BLOCK],
        reference: &[f32; BLOCK],
        right: Option<&[f32; BLOCK]>,
        out: &mut [f32; BLOCK],
    ) {
        // Non-finite input samples become zero here, before any detector or postfilter
        // state sees them (the linear stage does the same at its own boundary).
        let mut mic_a = finite_block(mic);
        let mut reference = finite_block(reference);
        let mut right = right.map(|r| finite_block(r));
        // `mic` is what every estimate and detector sees; `full` what the output is built on
        // (the same block unless the high-pass is internal, where it is the all-pass of the
        // mic).
        let mut full = mic_a;
        if let Some(a) = self.allpass.as_mut() {
            a.process(&mut full);
        }
        if let Some([fm, fr, fs]) = self.hp.as_mut() {
            fm.process(&mut mic_a);
            fr.process(&mut reference);
            if let Some(r) = right.as_mut() {
                fs.process(r);
            }
        }
        let internal = matches!(self.cfg.highpass, HighPass::Internal { .. });
        let full = if internal { full } else { mic_a };
        let mic = &mic_a;
        let reference = &reference;
        let adapt = if self.freeze_hold > 0 { 0.0 } else { 1.0 };
        let b = match &right {
            Some(r) => self.lin.process_block_stereo(mic, reference, r, adapt),
            None => self.lin.process_block(mic, reference, adapt),
        };
        let active = b.active;
        let mut e = [0.0f32; BLOCK];
        let mut y = [0.0f32; BLOCK];
        let mut x = [0.0f32; BLOCK];
        let mut xs = [0.0f32; BLOCK];
        e.copy_from_slice(b.error);
        y.copy_from_slice(b.echo);
        x.copy_from_slice(b.aligned);
        xs.copy_from_slice(b.aligned_side);
        // Linear output on the full band (internal high-pass): the estimate of the filtered
        // echo taken from the all-pass of the mic (see `super::highpass`).
        let e_full = if internal {
            std::array::from_fn(|i| full[i] - y[i])
        } else {
            e
        };
        self.last_linear = e_full;
        let delay = self.lin.delay_state();
        // Decision high-pass: from here on `mic`, `e`, `y`, `x` and `xs` are the filtered
        // signals every estimate and detector sees; the output is built from `full` and
        // `e_full` (unfiltered) and, without the postfilter option, the postfilter gets the
        // unfiltered `y_out` and `x_out`.
        let (y_out, x_out) = (y, x);
        let mut mic_dec = *mic;
        let dec_post = matches!(
            self.cfg.highpass,
            HighPass::Decisions {
                postfilter: true,
                ..
            }
        );
        if let Some([fm, fy, fx, fs]) = self.dec.as_mut() {
            fm.process(&mut mic_dec);
            fy.process(&mut y);
            fx.process(&mut x);
            fs.process(&mut xs);
            e = std::array::from_fn(|i| mic_dec[i] - y[i]);
        }
        let mic = &mic_dec;
        let separate = internal || dec_post;
        let (pf_echo, pf_aligned) = if self.dec.is_some() && !dec_post {
            (y_out, x_out)
        } else {
            (y, x)
        };

        if !active {
            // No echo path known: pass the mic through (unit gain, fixed latency) and forget
            // the models.
            self.post.reset_models();
            let to_mic = !self.base_was_mic;
            self.post.process(
                &Frame {
                    base: &full,
                    mic: &full,
                    to_mic,
                    echo: &[0.0; BLOCK],
                    aligned: &[0.0; BLOCK],
                    leak: 0.0,
                    suppress: false,
                    learn: false,
                    cold: false,
                    learn_cold: false,
                    doubletalk: false,
                    doubletalk_ref_weight: 0.0,
                    analysis: separate.then_some((mic, mic)),
                },
                out,
            );
            self.prev = [[0.0; BLOCK]; 4];
            self.prev[3].copy_from_slice(mic);
            self.acc_d = 0.0;
            self.acc_e = 0.0;
            self.converged = false;
            self.suspended = false;
            self.cold = true;
            self.cancelled_in_epoch = false;
            self.bypassing = false;
            self.cancel_blocks = 0;
            self.rho = [0.0; BINS];
            self.base_was_mic = true;
            self.doubletalk = false;
            self.dt_hold = 0;
            self.freeze_hold = 0;
            self.status = BlockStatus {
                delay,
                doubletalk: false,
                vetoed: false,
                frozen: false,
                bypass: true,
                mean_gain: 1.0,
                converged: false,
                cold: true,
                detector: [false; 4],
                coherence: 1.0,
            };
            return;
        }

        // Double-talk statistic on the aligned reference(s) and the mic, independent of
        // the linear stage's estimate; restarted on every realignment.
        let align = self.lin.alignment_epoch();
        if align != self.coh_epoch {
            self.coh_epoch = align;
            self.coh.reset();
        }
        self.coh.push(mic, &x, &xs);

        // Spectra of the last two blocks.
        let [pe_prev, py_prev, px_prev, _] = self.prev;
        let mut se = [0.0f32; BINS];
        let mut sy = [0.0f32; BINS];
        let mut sx = [0.0f32; BINS];
        self.spectrum(&pe_prev, &e, &mut se);
        self.spectrum(&py_prev, &y, &mut sy);
        self.spectrum(&px_prev, &x, &mut sx);
        self.prev = [e, y, x, *mic];

        // Broadband powers (smoothed over two blocks).
        let pow = |v: &[f32]| v.iter().map(|s| s * s).sum::<f32>();
        self.p_e = 0.5 * self.p_e + 0.5 * pow(&e);
        self.p_d = 0.5 * self.p_d + 0.5 * pow(mic);
        self.p_y = 0.5 * self.p_y + 0.5 * pow(&y);
        self.p_dy = 0.5 * self.p_dy + 0.5 * mic.iter().zip(&y).map(|(a, b)| a * b).sum::<f32>();
        // Once the linear stage has converged, an output louder than the mic means the
        // modelled echo is not there (echo path gone, for example after switching to
        // headphones): pass the mic through. Before convergence a misadjusted estimate is
        // normal and is not a reason to bypass.
        // Only an echo estimate at a relevant level counts: the decaying tail of the
        // filter output after the far end stops (tens of dB below the usual echo) can
        // exceed a near-silent mic without any echo-path change.
        let ref_active = sx.iter().sum::<f32>() > 1e-6 * N as f32;
        if ref_active {
            self.echo_level = 0.99 * self.echo_level + 0.01 * self.p_y;
        }
        // A new lock epoch needs fresh evidence of cancellation. It is also fresh evidence of
        // an echo path (the estimator confirmed and verified a new delay), so a mismatch
        // bypass, a verdict about the old alignment, ends with it (T6.23c): a jump to a
        // longer delay leaves the mic without echo for the difference while the filter
        // still predicts it, which looks exactly like an echo path that has gone, and a
        // bypass held past the re-lock kept the cold-phase model suspended until the
        // linear stage re-converged (dev-wide `delay-jump` recovery up to 3.5 s with the
        // 30 Hz high-pass, 1.0-1.4 s without the hold). Checked before the mismatch rules,
        // so the re-lock block itself is judged on the new epoch (its output and the
        // reference model's `suspended` agree).
        let epoch = self.lin.alignment_epoch();
        if epoch != self.epoch {
            self.epoch = epoch;
            self.cancelled_in_epoch = false;
            self.bypassing = false;
            self.suspended = false;
            self.mismatch_run = 0;
            self.gone_run = 0;
            self.cancel_blocks = 0;
        }
        let mismatch_now = self.converged
            && self.p_e > self.cfg.mismatch_ratio * self.p_d
            && self.p_y > 0.05 * self.p_d
            && self.p_y > MISMATCH_RELEVANCE * self.echo_level;
        self.mismatch_run = if mismatch_now {
            self.mismatch_run + 1
        } else {
            0
        };
        // An output 20 dB above the mic for a few blocks is no overshoot of a clipped burst
        // (those stay within about 12 dB): the echo path has gone, bypass without waiting
        // for `mismatch_blocks`. Block powers, not the smoothed ones (the mic drops at once
        // when the echo goes), with an echo estimate at a typical level; this needs only a
        // linear stage that has cancelled in this epoch, not the current `converged`
        // (double talk just before the switch can have cleared it).
        let gone_now = self.cancelled_in_epoch
            && pow(&e) > MISMATCH_GONE_RATIO * pow(mic)
            && pow(&y) > 0.1 * self.echo_level;
        self.gone_run = if gone_now { self.gone_run + 1 } else { 0 };
        let mismatch = self.mismatch_run >= self.cfg.mismatch_blocks.max(1)
            || self.gone_run >= MISMATCH_GONE_BLOCKS;
        if mismatch {
            self.bypassing = true;
            self.cancel_blocks = 0;
        } else if self.bypassing {
            // Leave the bypass only after sustained cancellation (the linear stage has
            // re-converged on a new path); with the echo gone the output never drops
            // below the mic and the bypass holds.
            if self.p_e < 0.7 * self.p_d {
                self.cancel_blocks += 1;
                if self.cancel_blocks >= 10 {
                    self.bypassing = false;
                }
            } else {
                self.cancel_blocks = 0;
            }
        }
        let bypass = self.bypassing;
        if mismatch {
            // Distrust the reference-based model until the linear stage cancels again.
            self.suspended = true;
            self.converged = false;
            self.acc_d = 0.0;
            self.acc_e = 0.0;
        }

        // Noise floor.
        for (n, &e) in self.noise.iter_mut().zip(&se) {
            *n = if e < *n { e.max(1e-12) } else { *n * 1.005 };
        }

        // A linear-filter reset (divergence or loss of the echo path) starts a new cold
        // phase: the empty filter has not cancelled anything, so the detector that uses
        // its estimate as witness waits for fresh cancellation (otherwise its stale gate
        // reads every block as near-end speech and freezes the filter for good).
        if self.lin.resets() != self.seen_resets {
            self.seen_resets = self.lin.resets();
            self.cold = true;
            self.converged = false;
            self.cancelled_in_epoch = false;
            self.acc_d = 0.0;
            self.acc_e = 0.0;
        }

        let leak = self.lin.leak();
        // Live evidence of an echo path (AGENTS.md): a lock, or a linear stage that has
        // cancelled in this epoch. Without it nothing is suppressed or learned.
        let evidence = delay.is_locked() || self.cancelled_in_epoch;

        // Double-talk detection: error power well above the leakage-based residual echo
        // (which keeps updating while adaptation is frozen, so the detector cannot lock
        // itself in) plus noise. Skipped while the linear stage bootstraps.
        let (sum_e, sum_rn): (f32, f32) = se
            .iter()
            .zip(sy.iter().zip(&self.rho))
            .zip(&self.noise)
            .fold((0.0, 0.0), |(a, b), ((e, (y, rho)), n)| {
                (a + e, b + leak.max(*rho) * y + 2.0 * n)
            });
        // Only meaningful once the linear stage cancels (its echo estimate explains the
        // echo); before that the cold-phase model covers suppression.
        let gate = ref_active
            && self.cancelled_in_epoch
            && self.acc_d > self.cfg.detector_min_erle * self.acc_e;
        // Near end: the error exceeds the residual estimate and the mic is not explained
        // by the echo estimate (mic power clearly above it).
        let corr = self.p_dy / (self.p_d * self.p_y).sqrt().max(1e-20);
        let unexplained = self.p_d > self.cfg.unexplained_ratio * self.p_y
            && corr < self.cfg.doubletalk_correlation;
        // Mono veto (T1.5f): at far-end offsets the late reverberation the linear stage has
        // not cancelled yet leaves an error as large as the echo, which the witness reads as
        // near-end speech; the call then softens the suppression and freezes the filter, and
        // the echo burst passes. While the aligned reference explains the mic almost
        // entirely (`xi^2`, independent of the linear estimate) the call neither freezes the
        // filter nor softens the suppression; it still stops the models from learning.
        // Withdrawing the call altogether also cost up to 1.9 dB of double-talk SI-SDR on
        // the robustness grid (9.59 dB at 500 ms +200 ppm, measured).
        let vetoed = !self.lin.is_stereo() && self.coh.xi2() >= self.cfg.mono_coherence_veto;
        let near_linear = gate && unexplained && sum_e > self.cfg.doubletalk_ratio * sum_rn;
        // For a stereo reference the coherence statistic (T1.5d), which needs no linear
        // estimate, decides alone: the two-channel linear stage converges slowly and
        // plateaus low under loudspeaker distortion, so its estimate is no reliable witness
        // (a poor estimate makes far-end speech look unexplained). Mono keeps the linear
        // witness: on a real recording (a mono reference of stereo playback) the
        // statistic dips in far-end-only speech and costs far-end-only ERLE as a
        // detector; for mono it only vetoes (above), which never adds a call.
        let coh_only = self.lin.is_stereo();
        let near_coh =
            coh_only && ref_active && evidence && self.coh.xi2() < self.cfg.coherence_threshold;
        let near = (near_linear && !coh_only) || near_coh;
        self.detector = [
            gate,
            self.p_d > self.cfg.unexplained_ratio * self.p_y,
            corr < self.cfg.doubletalk_correlation,
            sum_e > self.cfg.doubletalk_ratio * sum_rn,
        ];
        if near {
            self.dt_hold = self.cfg.doubletalk_hangover;
        } else {
            self.dt_hold = self.dt_hold.saturating_sub(1);
        }
        self.doubletalk = self.dt_hold > 0;
        // Strong near-end evidence freezes the linear stage for the next block (its own
        // step control already slows adaptation under weaker double talk). Only the
        // linear witness freezes: freezing on the coherence statistic stalls a stereo
        // linear stage that has not converged yet (measured, T1.5d).
        let strong = gate && unexplained && !vetoed && sum_e > self.cfg.freeze_ratio * sum_rn;
        if strong {
            self.freeze_hold = self.cfg.doubletalk_hangover;
        } else {
            self.freeze_hold = self.freeze_hold.saturating_sub(1);
        }

        // Learn the models while only the far end talks.
        let explained = !unexplained;
        if ref_active && !self.doubletalk && !bypass && (!self.converged || explained) {
            // Linear-stage ERLE (smoothed), with hysteresis for `converged`. Once
            // converged, only blocks the echo estimate explains count: near-end speech the
            // detector missed must not pull the ERLE down and switch detection off.
            self.acc_d = 0.95 * self.acc_d + pow(mic);
            self.acc_e = 0.95 * self.acc_e + pow(&e);
            let lin_erle = self.acc_d / self.acc_e.max(1e-12);
            let bar = if self.lin.is_stereo() {
                STEREO_ERLE_SCALE
            } else {
                1.0
            };
            if lin_erle > self.cfg.converged_ratio * bar {
                self.converged = true;
                self.cancelled_in_epoch = true;
                self.suspended = false;
                self.cold = false;
            } else if lin_erle < 3.0 * bar {
                self.converged = false;
            }
            for k in 0..BINS {
                // Only where the mic is explained by the echo estimate (no near end), a
                // test that does not depend on rho itself.
                if self.converged && explained && sy[k] > 1e-12 {
                    // Robust update: one block can raise rho at most fourfold, so a
                    // near-end burst the detector missed cannot inflate it.
                    let cap = (4.0 * self.rho[k]).max(1e-3);
                    let ratio = (se[k] / sy[k]).min(cap).clamp(1e-4, 1.0);
                    self.rho[k] = 0.9 * self.rho[k] + 0.1 * ratio;
                }
            }
        }

        // Base signal: linear output, or the mic itself on mismatch. Switching to the mic
        // swaps the postfilter's whole analysis history to the mic (no stale echo estimate
        // is released); switching back crossfades over one block.
        let to_mic = bypass && !self.base_was_mic;
        let base_of = |mic: &[f32; BLOCK], e: &[f32; BLOCK]| -> [f32; BLOCK] {
            std::array::from_fn(|i| {
                let w = (i as f32 + 0.5) / BLOCK as f32;
                match (self.base_was_mic, bypass) {
                    (_, true) => mic[i],
                    (false, false) => e[i],
                    (true, false) => (1.0 - w) * mic[i] + w * e[i],
                }
            })
        };
        let base = base_of(&full, &e_full);
        let base_a = base_of(mic, &e);
        self.base_was_mic = bypass;

        let learning = ref_active && !self.doubletalk && !bypass;
        let ref_model = (self.cold || self.cfg.ref_model_always) && !self.suspended && evidence;
        self.post.process(
            &Frame {
                base: &base,
                mic: &full,
                to_mic,
                echo: &pf_echo,
                aligned: &pf_aligned,
                leak,
                suppress: evidence && !bypass && self.cfg.suppress,
                learn: learning && self.converged && explained,
                cold: ref_model,
                learn_cold: learning
                    && evidence
                    && (self.cold || (self.cfg.ref_model_always && explained)),
                // The mono veto keeps the full suppression (T1.5f).
                doubletalk: self.doubletalk && !vetoed,
                doubletalk_ref_weight: if self.lin.is_stereo() {
                    STEREO_DOUBLETALK_REF_WEIGHT
                } else {
                    0.0
                },
                analysis: separate.then_some((&base_a, mic)),
            },
            out,
        );

        self.status = BlockStatus {
            delay,
            doubletalk: self.doubletalk,
            vetoed: self.doubletalk && vetoed,
            frozen: self.freeze_hold > 0,
            bypass,
            mean_gain: self.post.mean_gain(),
            converged: self.converged,
            cold: self.cold,
            detector: self.detector,
            coherence: self.coh.xi2(),
        };
    }
}
