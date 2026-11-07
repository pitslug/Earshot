//! Audio primitives shared by the AEC, ASR and capture code (PLAN.md 3.3).
//!
//! - [`Sample`], [`SampleRate`], [`MonoBuffer`]: mono `f32` audio at a known rate.
//! - [`i16_to_f32`] / [`f32_to_i16`] and their slice forms: PCM16 conversion.
//! - [`RingBuffer`]: fixed-capacity streaming FIFO, no allocation after construction.
//! - [`Resampler`]: streaming windowed-sinc resampler (rubato) with a runtime-adjustable
//!   ratio for clock-drift compensation. Per PLAN.md 3.3 drift is corrected by resampling
//!   the **reference**, never the mic.
//! - [`read_wav`] / [`write_wav`]: WAV over `std::io::Read` / `std::io::Write`, never paths.

mod convert;
mod resample;
mod ring;
mod types;
mod wav;

#[cfg(test)]
mod tests;

pub use convert::{f32_slice_to_i16, f32_to_i16, i16_slice_to_f32, i16_to_f32, quantise_pcm16};
pub use resample::{
    DEFAULT_MAX_DEVIATION, MAX_RATE_HZ, MAX_RATE_RATIO, MIN_RATE_HZ, Resampler, ResamplerError,
    resample_all,
};
pub use ring::RingBuffer;
pub use types::{MonoBuffer, PIPELINE_RATE, Sample, SampleRate};
pub use wav::{WavAudio, WavError, WavFormat, read_wav, write_wav};
