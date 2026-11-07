//! Sample, sample-rate and mono buffer types.

use std::num::NonZeroU32;
use std::time::Duration;

/// One mono audio sample. Full scale is `[-1.0, 1.0]`; values outside are allowed in
/// intermediate processing and clamped only on conversion to integer PCM.
pub type Sample = f32;

/// A sample rate in Hz. Never zero.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SampleRate(NonZeroU32);

impl SampleRate {
    /// 16 kHz, the pipeline rate (AEC, VAD, ASR).
    pub const HZ_16000: Self = Self::from_const(16_000);
    /// 48 kHz, the usual Windows shared-mode device rate.
    pub const HZ_48000: Self = Self::from_const(48_000);

    const fn from_const(hz: u32) -> Self {
        match NonZeroU32::new(hz) {
            Some(v) => Self(v),
            None => panic!("sample rate must be non-zero"),
        }
    }

    /// Returns `None` for 0 Hz.
    #[must_use]
    pub const fn new(hz: u32) -> Option<Self> {
        match NonZeroU32::new(hz) {
            Some(v) => Some(Self(v)),
            None => None,
        }
    }

    /// The rate in Hz.
    #[must_use]
    pub const fn hz(self) -> u32 {
        self.0.get()
    }

    /// Number of frames in `duration` at this rate, rounded to the nearest frame.
    #[must_use]
    pub fn frames_for(self, duration: Duration) -> usize {
        (duration.as_secs_f64() * f64::from(self.hz())).round() as usize
    }

    /// Duration of `frames` frames at this rate.
    #[must_use]
    pub fn duration_of(self, frames: usize) -> Duration {
        Duration::from_secs_f64(frames as f64 / f64::from(self.hz()))
    }
}

/// The rate every track is resampled to before AEC, VAD and ASR (PLAN.md 3.3).
pub const PIPELINE_RATE: SampleRate = SampleRate::HZ_16000;

/// An owned block of mono audio at a known sample rate.
#[derive(Debug, Clone, PartialEq)]
pub struct MonoBuffer {
    /// Sample rate of `samples`.
    pub rate: SampleRate,
    /// The samples, one per frame.
    pub samples: Vec<Sample>,
}

impl MonoBuffer {
    /// Wraps `samples` recorded at `rate`.
    #[must_use]
    pub fn new(rate: SampleRate, samples: Vec<Sample>) -> Self {
        Self { rate, samples }
    }

    /// Number of frames.
    #[must_use]
    pub fn len(&self) -> usize {
        self.samples.len()
    }

    /// True if there are no frames.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.samples.is_empty()
    }

    /// Playback duration.
    #[must_use]
    pub fn duration(&self) -> Duration {
        self.rate.duration_of(self.samples.len())
    }
}
