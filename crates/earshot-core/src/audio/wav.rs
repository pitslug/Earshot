//! WAV read/write over `std::io` traits (never paths, PLAN.md 3.2).
//!
//! Reading uses hound and accepts PCM 8/16/24/32-bit integer and 32-bit float, including
//! `WAVE_FORMAT_EXTENSIBLE`. Writing is done here so it needs only `Write` (no `Seek`):
//! the whole clip is known up front, so the RIFF sizes are written first.

use std::fmt;
use std::io::{Read, Write};
use std::num::NonZeroU16;

use super::{Sample, SampleRate, f32_to_i16};

/// Sample encoding for [`write_wav`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WavFormat {
    /// 16-bit signed integer PCM (format tag 1). Samples are rounded and clamped.
    Pcm16,
    /// 32-bit IEEE float (format tag 3). Lossless for `f32`.
    Float32,
}

/// Decoded WAV audio. Fields are private so the invariants hold: at least one channel and
/// a whole number of frames ([`WavAudio::new`] checks them).
#[derive(Debug, Clone, PartialEq)]
pub struct WavAudio {
    rate: SampleRate,
    channels: NonZeroU16,
    samples: Vec<Sample>,
}

impl WavAudio {
    /// Audio of `channels` interleaved channels at `rate`.
    ///
    /// # Errors
    /// [`WavError::InvalidInput`] for zero channels or a sample count that is not a
    /// multiple of the channel count.
    pub fn new(rate: SampleRate, channels: u16, samples: Vec<Sample>) -> Result<Self, WavError> {
        let channels = NonZeroU16::new(channels)
            .ok_or_else(|| WavError::InvalidInput("channel count is 0".into()))?;
        if !samples.len().is_multiple_of(usize::from(channels.get())) {
            return Err(WavError::InvalidInput(format!(
                "{} samples is not a whole number of {}-channel frames",
                samples.len(),
                channels
            )));
        }
        Ok(WavAudio {
            rate,
            channels,
            samples,
        })
    }

    /// Sample rate.
    #[must_use]
    pub fn rate(&self) -> SampleRate {
        self.rate
    }

    /// Channel count (at least 1).
    #[must_use]
    pub fn channels(&self) -> u16 {
        self.channels.get()
    }

    /// Channel count as a [`NonZeroU16`].
    #[must_use]
    pub fn channel_count(&self) -> NonZeroU16 {
        self.channels
    }

    /// Interleaved samples scaled to `[-1.0, 1.0]` full scale.
    #[must_use]
    pub fn samples(&self) -> &[Sample] {
        &self.samples
    }

    /// The interleaved samples, consuming the audio.
    #[must_use]
    pub fn into_samples(self) -> Vec<Sample> {
        self.samples
    }

    /// Number of frames (samples per channel).
    #[must_use]
    pub fn frames(&self) -> usize {
        self.samples.len() / usize::from(self.channels.get())
    }

    /// One channel, de-interleaved. Returns `None` if `index >= channels`.
    #[must_use]
    pub fn channel(&self, index: u16) -> Option<Vec<Sample>> {
        if index >= self.channels.get() {
            return None;
        }
        let ch = usize::from(self.channels.get());
        Some(
            self.samples
                .iter()
                .skip(usize::from(index))
                .step_by(ch)
                .copied()
                .collect(),
        )
    }

    /// Mono mix-down: the mean of all channels per frame.
    #[must_use]
    pub fn to_mono(&self) -> Vec<Sample> {
        let ch = usize::from(self.channels.get());
        if ch == 1 {
            return self.samples.clone();
        }
        let scale = 1.0 / ch as f32;
        self.samples
            .chunks_exact(ch)
            .map(|frame| frame.iter().sum::<f32>() * scale)
            .collect()
    }
}

/// WAV read/write errors.
#[derive(Debug)]
pub enum WavError {
    /// The underlying reader or writer failed.
    Io(std::io::Error),
    /// The input is not a valid or supported WAV stream.
    Format(String),
    /// Invalid arguments to [`write_wav`] (zero channels, sample count not a multiple of
    /// the channel count, or data too large for a RIFF file).
    InvalidInput(String),
}

impl fmt::Display for WavError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(e) => write!(f, "wav i/o error: {e}"),
            Self::Format(msg) => write!(f, "invalid or unsupported wav: {msg}"),
            Self::InvalidInput(msg) => write!(f, "invalid wav write request: {msg}"),
        }
    }
}

impl std::error::Error for WavError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<std::io::Error> for WavError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

impl From<hound::Error> for WavError {
    fn from(e: hound::Error) -> Self {
        match e {
            hound::Error::IoError(io) => Self::Io(io),
            other => Self::Format(other.to_string()),
        }
    }
}

/// Reads a whole WAV stream into memory.
///
/// # Errors
/// [`WavError::Io`] on read failure, [`WavError::Format`] for malformed or unsupported
/// data (including a sample rate of 0 or a truncated data chunk).
pub fn read_wav<R: Read>(reader: R) -> Result<WavAudio, WavError> {
    let reader = hound::WavReader::new(reader)?;
    let spec = reader.spec();
    let rate = SampleRate::new(spec.sample_rate)
        .ok_or_else(|| WavError::Format("sample rate is 0".into()))?;
    if spec.channels == 0 {
        return Err(WavError::Format("channel count is 0".into()));
    }
    let samples = match (spec.sample_format, spec.bits_per_sample) {
        (hound::SampleFormat::Float, 32) => reader
            .into_samples::<f32>()
            .collect::<Result<Vec<_>, _>>()?,
        (hound::SampleFormat::Int, bits @ 1..=32) => {
            let scale = 1.0 / f64::from(1u32 << (bits - 1));
            reader
                .into_samples::<i32>()
                .map(|s| s.map(|v| (f64::from(v) * scale) as f32))
                .collect::<Result<Vec<_>, _>>()?
        }
        (fmt, bits) => {
            return Err(WavError::Format(format!(
                "unsupported sample format {fmt:?} with {bits} bits"
            )));
        }
    };
    WavAudio::new(rate, spec.channels, samples).map_err(|e| match e {
        WavError::InvalidInput(m) => WavError::Format(m),
        other => other,
    })
}

/// Writes interleaved `samples` as a complete WAV stream. Needs only `Write`, so it works
/// on pipes as well as in-memory buffers.
///
/// # Errors
/// [`WavError::InvalidInput`] if `channels` is 0, `samples.len()` is not a multiple of
/// `channels`, or the data exceeds the 4 GiB RIFF limit; [`WavError::Io`] on write failure.
pub fn write_wav<W: Write>(
    mut writer: W,
    rate: SampleRate,
    channels: u16,
    samples: &[Sample],
    format: WavFormat,
) -> Result<(), WavError> {
    if channels == 0 {
        return Err(WavError::InvalidInput("channel count is 0".into()));
    }
    if !samples.len().is_multiple_of(usize::from(channels)) {
        return Err(WavError::InvalidInput(format!(
            "{} samples is not a multiple of {channels} channels",
            samples.len()
        )));
    }
    let (tag, bytes_per_sample): (u16, u16) = match format {
        WavFormat::Pcm16 => (1, 2),
        WavFormat::Float32 => (3, 4),
    };
    let too_large = || WavError::InvalidInput("data exceeds the 4 GiB RIFF limit".into());
    let data_len = u32::try_from(samples.len())
        .ok()
        .and_then(|n| n.checked_mul(u32::from(bytes_per_sample)))
        .ok_or_else(too_large)?;
    let riff_len = data_len.checked_add(36).ok_or_else(too_large)?;
    let block_align = channels
        .checked_mul(bytes_per_sample)
        .ok_or_else(|| WavError::InvalidInput("too many channels".into()))?;
    let byte_rate = rate
        .hz()
        .checked_mul(u32::from(block_align))
        .ok_or_else(|| WavError::InvalidInput("byte rate overflows".into()))?;

    let mut header = Vec::with_capacity(44);
    header.extend_from_slice(b"RIFF");
    header.extend_from_slice(&riff_len.to_le_bytes());
    header.extend_from_slice(b"WAVEfmt ");
    header.extend_from_slice(&16u32.to_le_bytes());
    header.extend_from_slice(&tag.to_le_bytes());
    header.extend_from_slice(&channels.to_le_bytes());
    header.extend_from_slice(&rate.hz().to_le_bytes());
    header.extend_from_slice(&byte_rate.to_le_bytes());
    header.extend_from_slice(&block_align.to_le_bytes());
    header.extend_from_slice(&(bytes_per_sample * 8).to_le_bytes());
    header.extend_from_slice(b"data");
    header.extend_from_slice(&data_len.to_le_bytes());
    writer.write_all(&header)?;

    let mut block = [0u8; 4096];
    match format {
        WavFormat::Pcm16 => {
            for chunk in samples.chunks(block.len() / 2) {
                for (dst, &s) in block.as_chunks_mut::<2>().0.iter_mut().zip(chunk) {
                    dst.copy_from_slice(&f32_to_i16(s).to_le_bytes());
                }
                writer.write_all(&block[..chunk.len() * 2])?;
            }
        }
        WavFormat::Float32 => {
            for chunk in samples.chunks(block.len() / 4) {
                for (dst, &s) in block.as_chunks_mut::<4>().0.iter_mut().zip(chunk) {
                    dst.copy_from_slice(&s.to_le_bytes());
                }
                writer.write_all(&block[..chunk.len() * 4])?;
            }
        }
    }
    writer.flush()?;
    Ok(())
}
