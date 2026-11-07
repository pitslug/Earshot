//! Streaming resampler with a runtime-adjustable ratio.
//!
//! Wraps rubato's asynchronous windowed-sinc resampler (`rubato::Async::new_sinc`,
//! <https://github.com/HEnquist/rubato>): a 256-tap Blackman-Harris-windowed sinc evaluated
//! at 128x oversampling with cubic interpolation between table points. The ratio can be
//! nudged while streaming; rubato ramps it linearly over the next internal chunk, so a
//! drift correction does not produce a click.

use std::fmt;

use rubato::audioadapter_buffers::direct::InterleavedSlice;
use rubato::{
    Adjustable, Async, FixedAsync, Indexing, Resampler as _, SincInterpolationParameters,
    WindowFunction,
};

use super::{Sample, SampleRate};

/// Default bound on [`Resampler::set_relative_ratio`]: the relative ratio may move up to
/// 1 % (10 000 ppm) either side of 1.0, far beyond the +/-200 ppm drift budget (PLAN.md 4.0).
pub const DEFAULT_MAX_DEVIATION: f64 = 0.01;

/// Lowest sample rate [`Resampler`] accepts, in Hz.
pub const MIN_RATE_HZ: u32 = 1_000;
/// Highest sample rate [`Resampler`] accepts, in Hz.
pub const MAX_RATE_HZ: u32 = 768_000;
/// Largest accepted ratio between the two rates (either direction).
pub const MAX_RATE_RATIO: f64 = 48.0;

const SINC_LEN: usize = 256;
const OVERSAMPLING: usize = 128;

/// Errors from [`Resampler`] construction and ratio changes.
#[derive(Debug, Clone, PartialEq)]
pub enum ResamplerError {
    /// The requested relative ratio is not finite or lies outside
    /// `[1 - max_deviation, 1 + max_deviation]`.
    RatioOutOfBounds {
        /// Requested relative ratio.
        requested: f64,
        /// Allowed deviation from 1.0.
        max_deviation: f64,
    },
    /// `max_deviation` must be finite and in `[0, 0.5]`.
    InvalidMaxDeviation(f64),
    /// A rate is outside [`MIN_RATE_HZ`]..=[`MAX_RATE_HZ`], their ratio exceeds
    /// [`MAX_RATE_RATIO`], or rubato rejected the configuration.
    Construction(String),
}

impl fmt::Display for ResamplerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::RatioOutOfBounds {
                requested,
                max_deviation,
            } => write!(
                f,
                "relative resample ratio {requested} outside 1 +/- {max_deviation}"
            ),
            Self::InvalidMaxDeviation(d) => {
                write!(f, "max ratio deviation {d} must be finite and in [0, 0.5]")
            }
            Self::Construction(msg) => write!(f, "resampler construction failed: {msg}"),
        }
    }
}

impl std::error::Error for ResamplerError {}

/// A streaming mono resampler from one [`SampleRate`] to another.
///
/// Feed input of any length with [`Resampler::process`]; output is appended to a caller
/// `Vec`. Input is consumed in fixed internal chunks of 10 ms, so output is produced in
/// bursts and lags input by up to one chunk plus half the filter length. The filter's
/// start-up delay is trimmed, so output frame `n` corresponds to input time
/// `n / to_rate` within half an output frame (the filter delay is rounded to the nearest
/// whole output frame), downsampling and upsampling alike. The output does not depend on
/// how the input is split across calls.
///
/// [`Resampler::set_relative_ratio`] scales the nominal `to / from` ratio for clock-drift
/// compensation. PLAN.md 3.3: apply it to the **reference** track only, never the mic.
///
/// No allocation happens after construction as long as the output `Vec` has spare
/// capacity of at least [`Resampler::max_output_len`] for the input passed.
pub struct Resampler {
    inner: Async<f32>,
    from: SampleRate,
    to: SampleRate,
    relative: f64,
    max_deviation: f64,
    /// Input waiting for a full chunk. Capacity fixed at `input_frames_max`.
    staged: Vec<f32>,
    /// Output scratch sized `output_frames_max`.
    scratch: Vec<f32>,
    /// Start-up delay trimmed from every stream, fixed at construction from the nominal
    /// ratio (rubato's own delay figure follows the current ratio).
    trim: usize,
    /// Start-up delay output frames still to drop in this stream.
    to_trim: usize,
    /// No chunk has been processed since construction or reset, so a ratio change can
    /// apply without a ramp.
    fresh: bool,
    /// Output frames owed for the real (non-padding) input consumed so far.
    expected_out: f64,
    /// Output frames emitted (after trimming) since the stream started.
    emitted: u64,
}

impl fmt::Debug for Resampler {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Resampler")
            .field("from", &self.from)
            .field("to", &self.to)
            .field("relative", &self.relative)
            .field("max_deviation", &self.max_deviation)
            .field("staged", &self.staged.len())
            .finish_non_exhaustive()
    }
}

impl Resampler {
    /// Creates a resampler from `from` to `to` whose relative ratio may move within
    /// [`DEFAULT_MAX_DEVIATION`].
    ///
    /// # Errors
    /// [`ResamplerError::Construction`] for rates outside
    /// [`MIN_RATE_HZ`]..=[`MAX_RATE_HZ`] or a rate ratio above [`MAX_RATE_RATIO`].
    pub fn new(from: SampleRate, to: SampleRate) -> Result<Self, ResamplerError> {
        Self::with_max_deviation(from, to, DEFAULT_MAX_DEVIATION)
    }

    /// The pipeline's device-to-processing converter: 48 kHz -> 16 kHz (PLAN.md 3.3).
    ///
    /// # Panics
    /// Never: both rates are fixed and within the accepted range.
    #[must_use]
    pub fn pipeline_48k_to_16k() -> Self {
        Self::new(SampleRate::HZ_48000, SampleRate::HZ_16000)
            .expect("48 kHz -> 16 kHz is within the accepted range")
    }

    /// Like [`Resampler::new`] with a custom bound on the relative ratio.
    ///
    /// # Errors
    /// [`ResamplerError::InvalidMaxDeviation`] if `max_deviation` is not finite or not in
    /// `[0, 0.5]`; [`ResamplerError::Construction`] for out-of-range rates (see
    /// [`Resampler::new`]) or if rubato rejects the configuration.
    pub fn with_max_deviation(
        from: SampleRate,
        to: SampleRate,
        max_deviation: f64,
    ) -> Result<Self, ResamplerError> {
        if !max_deviation.is_finite() || !(0.0..=0.5).contains(&max_deviation) {
            return Err(ResamplerError::InvalidMaxDeviation(max_deviation));
        }
        for rate in [from, to] {
            if !(MIN_RATE_HZ..=MAX_RATE_HZ).contains(&rate.hz()) {
                return Err(ResamplerError::Construction(format!(
                    "sample rate {} Hz outside {MIN_RATE_HZ}..={MAX_RATE_HZ} Hz",
                    rate.hz()
                )));
            }
        }
        let ratio = f64::from(to.hz()) / f64::from(from.hz());
        if !(1.0 / MAX_RATE_RATIO..=MAX_RATE_RATIO).contains(&ratio) {
            return Err(ResamplerError::Construction(format!(
                "rate ratio {ratio} outside 1/{MAX_RATE_RATIO}..={MAX_RATE_RATIO}"
            )));
        }
        let chunk = (from.hz() as usize / 100).max(1);
        let params = SincInterpolationParameters::new(SINC_LEN, WindowFunction::BlackmanHarris2)
            .oversampling_factor(OVERSAMPLING);
        // rubato bounds the ratio multiplicatively: [r / m, r * m]. 1 / (1 - d) >= 1 + d, so
        // this covers 1 +/- d; the tighter symmetric bound is enforced in set_relative_ratio.
        let max_rel = 1.0 / (1.0 - max_deviation);
        let inner = Async::<f32>::new_sinc(ratio, max_rel, &params, chunk, 1, FixedAsync::Input)
            .map_err(|e| ResamplerError::Construction(e.to_string()))?;
        let staged = Vec::with_capacity(inner.input_frames_max());
        let scratch = vec![0.0; inner.output_frames_max()];
        // rubato reports the sinc delay as floor(SINC_LEN / 2 * ratio) output frames
        // (`Async::output_delay`), but the output it produces sits one output frame
        // earlier than that exact value (measured 2026-10-18 at 48k<->16k, 32k->16k,
        // 44.1k<->16k and 16k->22.05k: lag = exact - 1 - trim). Trimming the nearest
        // whole frame of the true delay keeps output frame n within half a frame of input
        // time n / to_rate in both directions.
        let exact = SINC_LEN as f64 / 2.0 * ratio - 1.0;
        // Positive for every accepted ratio: at the smallest, 1 / MAX_RATE_RATIO, it is
        // 128 / 48 - 1 = 1.67.
        let trim = exact.round() as usize;
        Ok(Self {
            inner,
            from,
            to,
            relative: 1.0,
            max_deviation,
            staged,
            scratch,
            trim,
            to_trim: trim,
            fresh: true,
            expected_out: 0.0,
            emitted: 0,
        })
    }

    /// Input rate.
    #[must_use]
    pub fn from_rate(&self) -> SampleRate {
        self.from
    }

    /// Nominal output rate.
    #[must_use]
    pub fn to_rate(&self) -> SampleRate {
        self.to
    }

    /// Current relative ratio (1.0 = nominal).
    #[must_use]
    pub fn relative_ratio(&self) -> f64 {
        self.relative
    }

    /// Filter delay in output frames that is trimmed from the start of the stream. Output
    /// lags input by this much plus up to one internal chunk.
    #[must_use]
    pub fn output_delay(&self) -> usize {
        self.trim
    }

    /// Sets the output/input ratio to `relative * to / from`. A value above 1.0 produces
    /// more output frames per input frame (use it when the input clock runs slow relative
    /// to the target). Mid-stream the change is ramped linearly over the next internal
    /// chunk (10 ms of input) to avoid a discontinuity. Before the first chunk of a stream
    /// (after construction, [`Resampler::reset`] or [`Resampler::flush`]) it applies at
    /// once, so a stream at a constant ratio yields exactly `round(input_len * ratio)`
    /// frames.
    ///
    /// # Errors
    /// [`ResamplerError::RatioOutOfBounds`] if `relative` is not finite or deviates from 1.0
    /// by more than the configured maximum. The ratio is unchanged on error.
    pub fn set_relative_ratio(&mut self, relative: f64) -> Result<(), ResamplerError> {
        // Tolerate rounding so that exactly `1 +/- max_deviation` is accepted.
        let limit = self.max_deviation * (1.0 + 1e-9);
        if !relative.is_finite() || (relative - 1.0).abs() > limit {
            return Err(ResamplerError::RatioOutOfBounds {
                requested: relative,
                max_deviation: self.max_deviation,
            });
        }
        self.inner
            .set_resample_ratio_relative(relative, !self.fresh)
            .map_err(|_| ResamplerError::RatioOutOfBounds {
                requested: relative,
                max_deviation: self.max_deviation,
            })?;
        self.relative = relative;
        Ok(())
    }

    /// Upper bound on the frames [`Resampler::process`] appends for `input_len` input
    /// frames. Reserve this much spare capacity to keep `process` allocation-free.
    #[must_use]
    pub fn max_output_len(&self, input_len: usize) -> usize {
        let chunks = (self.staged.len() + input_len) / self.inner.input_frames_next();
        chunks * self.inner.output_frames_max()
    }

    /// Consumes all of `input` and appends the output that is ready to `out`. Returns the
    /// number of frames appended.
    pub fn process(&mut self, mut input: &[Sample], out: &mut Vec<Sample>) -> usize {
        let start = out.len();
        while !input.is_empty() {
            let need = self.inner.input_frames_next();
            let take = (need - self.staged.len()).min(input.len());
            self.staged.extend_from_slice(&input[..take]);
            input = &input[take..];
            if self.staged.len() == need {
                self.run_chunk(need, out);
            }
        }
        out.len() - start
    }

    /// Ends the stream: pushes out the staged input and the filter tail, appending to
    /// `out` exactly the frames owed for the input consumed since the stream started
    /// (so a whole stream at a constant ratio yields `round(input_len * ratio)` frames).
    /// The resampler is then reset for a new stream, keeping its relative ratio.
    /// Returns the number of frames appended.
    pub fn flush(&mut self, out: &mut Vec<Sample>) -> usize {
        let start = out.len();
        let real = self.staged.len();
        if real > 0 {
            let need = self.inner.input_frames_next();
            self.staged.resize(need, 0.0);
            self.run_chunk(real, out);
        }
        let owed = self.expected_out.round() as u64;
        while self.emitted < owed {
            let need = self.inner.input_frames_next();
            self.staged.resize(need, 0.0);
            self.run_chunk(0, out);
        }
        let excess = usize::try_from(self.emitted - owed).unwrap_or(usize::MAX);
        out.truncate(out.len().saturating_sub(excess).max(start));
        self.reset();
        out.len() - start
    }

    /// Discards all state and starts a new stream, keeping the relative ratio.
    pub fn reset(&mut self) {
        self.inner.reset();
        if self.relative != 1.0 {
            // In bounds: it was accepted before.
            let _ = self.inner.set_resample_ratio_relative(self.relative, false);
        }
        self.staged.clear();
        self.to_trim = self.trim;
        self.fresh = true;
        self.expected_out = 0.0;
        self.emitted = 0;
    }

    /// Runs one chunk from `staged` (which holds exactly `input_frames_next` frames, the
    /// first `real` of which are signal) and appends the trimmed output.
    fn run_chunk(&mut self, real: usize, out: &mut Vec<Sample>) {
        let n_in = self.staged.len();
        let n_out = self.scratch.len();
        let input = InterleavedSlice::new(&self.staged[..], 1, n_in)
            .expect("staged length matches one channel");
        let mut output = InterleavedSlice::new_mut(&mut self.scratch[..], 1, n_out)
            .expect("scratch length matches one channel");
        let indexing = Indexing::new().partial_len(real);
        let ratio_before = self.inner.resample_ratio();
        let (_, produced) = self
            .inner
            .process_into_buffer(&input, &mut output, Some(&indexing))
            .expect("buffers are sized from the resampler's own limits");
        // Over a ramped chunk the ratio moves linearly, so the mean ratio applies.
        let ratio_mean = 0.5 * (ratio_before + self.inner.resample_ratio());
        self.expected_out += real as f64 * ratio_mean;
        self.staged.clear();
        self.fresh = false;
        let skip = self.to_trim.min(produced);
        self.to_trim -= skip;
        out.extend_from_slice(&self.scratch[skip..produced]);
        self.emitted += (produced - skip) as u64;
    }
}

/// Resamples a whole clip from `from` to `to` (for example 48 kHz device audio to the
/// 16 kHz pipeline rate). Returns `round(input.len() * to / from)` frames, time-aligned
/// with the input (start-up delay trimmed).
///
/// # Errors
/// As [`Resampler::new`].
pub fn resample_all(
    input: &[Sample],
    from: SampleRate,
    to: SampleRate,
) -> Result<Vec<Sample>, ResamplerError> {
    let mut r = Resampler::new(from, to)?;
    let mut out = Vec::with_capacity(r.max_output_len(input.len()) + r.output_delay() * 4 + 64);
    r.process(input, &mut out);
    r.flush(&mut out);
    Ok(out)
}
