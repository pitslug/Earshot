//! Reads the FLAC STREAMINFO block (sample rate, channels, total samples).
//!
//! Layout from the FLAC format specification (RFC 9639, section 8.2): the stream starts with
//! `fLaC`, then metadata blocks with a 4-byte header (last-block flag, 7-bit type, 24-bit
//! length). STREAMINFO is type 0 and must come first; bytes 10..18 of its body pack
//! sample rate (20 bits), channels - 1 (3 bits), bits per sample - 1 (5 bits) and total
//! samples (36 bits).

use std::fs::File;
use std::io::Read;
use std::path::Path;

use anyhow::{Context, Result, bail};

/// Stream parameters from STREAMINFO.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StreamInfo {
    /// Samples per second.
    pub sample_rate: u32,
    /// Channel count.
    pub channels: u8,
    /// Bits per sample.
    pub bits_per_sample: u8,
    /// Samples per channel; 0 means unknown.
    pub total_samples: u64,
}

impl StreamInfo {
    /// Duration in seconds.
    pub fn seconds(&self) -> f64 {
        self.total_samples as f64 / f64::from(self.sample_rate)
    }
}

/// Parses STREAMINFO from the first 42 bytes of a FLAC stream.
pub fn parse(head: &[u8]) -> Result<StreamInfo> {
    if head.len() < 42 || &head[..4] != b"fLaC" {
        bail!("not a FLAC stream");
    }
    if head[4] & 0x7f != 0 {
        bail!("first metadata block is not STREAMINFO");
    }
    let b = &head[8..42];
    let sample_rate = (u32::from(b[10]) << 12) | (u32::from(b[11]) << 4) | (u32::from(b[12]) >> 4);
    let channels = ((b[12] >> 1) & 0x07) + 1;
    let bits_per_sample = (((b[12] & 0x01) << 4) | (b[13] >> 4)) + 1;
    let total_samples = (u64::from(b[13] & 0x0f) << 32)
        | (u64::from(b[14]) << 24)
        | (u64::from(b[15]) << 16)
        | (u64::from(b[16]) << 8)
        | u64::from(b[17]);
    if sample_rate == 0 {
        bail!("STREAMINFO sample rate is 0");
    }
    Ok(StreamInfo {
        sample_rate,
        channels,
        bits_per_sample,
        total_samples,
    })
}

/// Reads STREAMINFO from a file.
pub fn read(path: &Path) -> Result<StreamInfo> {
    let mut head = [0u8; 42];
    File::open(path)
        .and_then(|mut f| f.read_exact(&mut head))
        .with_context(|| format!("read {}", path.display()))?;
    parse(&head).with_context(|| path.display().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds a STREAMINFO header by hand for the given parameters.
    fn head(rate: u32, ch: u8, bps: u8, total: u64) -> Vec<u8> {
        let mut v = b"fLaC".to_vec();
        v.extend_from_slice(&[0x80, 0, 0, 34]);
        let mut si = [0u8; 34];
        si[10] = (rate >> 12) as u8;
        si[11] = (rate >> 4) as u8;
        si[12] = (((rate & 0xf) << 4) as u8) | ((ch - 1) << 1) | ((bps - 1) >> 4);
        si[13] = (((bps - 1) & 0xf) << 4) | ((total >> 32) as u8 & 0xf);
        si[14..18].copy_from_slice(&(total as u32).to_be_bytes());
        v.extend_from_slice(&si);
        v
    }

    #[test]
    fn parses_librispeech_style_header() {
        let si = parse(&head(16_000, 1, 16, 166_960)).unwrap();
        assert_eq!(si.sample_rate, 16_000);
        assert_eq!(si.channels, 1);
        assert_eq!(si.bits_per_sample, 16);
        assert_eq!(si.total_samples, 166_960);
        assert!((si.seconds() - 10.435).abs() < 1e-9);
    }

    #[test]
    fn parses_wide_values() {
        let si = parse(&head(96_000, 8, 24, (1u64 << 35) + 7)).unwrap();
        assert_eq!(
            (
                si.sample_rate,
                si.channels,
                si.bits_per_sample,
                si.total_samples
            ),
            (96_000, 8, 24, (1u64 << 35) + 7)
        );
    }

    #[test]
    fn rejects_non_flac() {
        assert!(parse(b"RIFF....").is_err());
        let mut h = head(16_000, 1, 16, 1);
        h[0] = b'x';
        assert!(parse(&h).is_err());
    }
}
