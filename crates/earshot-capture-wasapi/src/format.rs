//! Mix-format description and conversion of raw WASAPI packets to mono f32.
//!
//! Platform independent: the WASAPI layer hands over the raw `WAVEFORMATEX`
//! bytes and the raw packet bytes, everything else happens here.

use std::fmt;

use serde::Serialize;

/// `WAVE_FORMAT_PCM`.
const TAG_PCM: u16 = 0x0001;
/// `WAVE_FORMAT_IEEE_FLOAT`.
const TAG_IEEE_FLOAT: u16 = 0x0003;
/// `WAVE_FORMAT_EXTENSIBLE`.
const TAG_EXTENSIBLE: u16 = 0xFFFE;
/// Bytes 4..16 of every `KSDATAFORMAT_SUBTYPE_*` GUID derived from a wave format tag
/// (`xxxxxxxx-0000-0010-8000-00aa00389b71`), in memory order.
const KS_SUBTYPE_TAIL: [u8; 12] = [
    0x00, 0x00, 0x10, 0x00, 0x80, 0x00, 0x00, 0xaa, 0x00, 0x38, 0x9b, 0x71,
];
/// Size of `WAVEFORMATEX` without trailing extension bytes.
const WAVEFORMATEX_LEN: usize = 18;
/// Size of `WAVEFORMATEXTENSIBLE`.
const WAVEFORMATEXTENSIBLE_LEN: usize = 40;

/// Sample encoding of one channel sample inside a frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SampleFormat {
    /// 32-bit IEEE float.
    F32,
    /// Signed 16-bit integer.
    I16,
    /// Signed 24-bit integer packed in 3 bytes.
    I24,
    /// Signed integer in a 32-bit container (valid bits are the most significant ones).
    I32,
}

/// Device mix format, as reported by `IAudioClient::GetMixFormat`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct MixFormat {
    /// Native sample rate in Hz.
    pub sample_rate: u32,
    /// Interleaved channel count.
    pub channels: u16,
    /// Bytes per frame (all channels).
    pub block_align: u16,
    /// Container size of one sample in bits.
    pub bits_per_sample: u16,
    /// Meaningful bits per sample (equals `bits_per_sample` unless the format is extensible).
    pub valid_bits_per_sample: u16,
    /// Sample encoding.
    pub sample_format: SampleFormat,
}

/// Errors from format parsing and packet conversion.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FormatError {
    /// The `WAVEFORMATEX` bytes are shorter than the structure they claim to hold.
    Truncated {
        /// Bytes needed.
        needed: usize,
        /// Bytes available.
        got: usize,
    },
    /// Format tag, sub-format or bit depth this crate does not convert.
    Unsupported(String),
    /// A packet's byte length is not a whole number of frames.
    PartialFrame {
        /// Packet length in bytes.
        len: usize,
        /// Bytes per frame.
        block_align: u16,
    },
    /// A stream was initialised with another rate or channel count than requested.
    NotDelivered {
        /// Requested rate (`None`: any).
        requested_rate: Option<u32>,
        /// Requested channel count (`None`: any).
        requested_channels: Option<u16>,
        /// Delivered rate.
        rate: u32,
        /// Delivered channel count.
        channels: u16,
    },
}

impl fmt::Display for FormatError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Truncated { needed, got } => {
                write!(f, "wave format truncated: need {needed} bytes, got {got}")
            }
            Self::Unsupported(what) => write!(f, "unsupported mix format: {what}"),
            Self::PartialFrame { len, block_align } => write!(
                f,
                "packet of {len} bytes is not a whole number of {block_align}-byte frames"
            ),
            Self::NotDelivered {
                requested_rate,
                requested_channels,
                rate,
                channels,
            } => {
                let any = |v: Option<String>| v.unwrap_or_else(|| "any".to_owned());
                write!(
                    f,
                    "stream delivers {rate} Hz, {channels} ch instead of the requested {} Hz, {} ch",
                    any(requested_rate.map(|r| r.to_string())),
                    any(requested_channels.map(|c| c.to_string()))
                )
            }
        }
    }
}

impl std::error::Error for FormatError {}

fn u16_at(b: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([b[at], b[at + 1]])
}

fn u32_at(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}

impl MixFormat {
    /// Parses a little-endian `WAVEFORMATEX` or `WAVEFORMATEXTENSIBLE` byte image.
    ///
    /// Accepts PCM (16, 24, 32 bit containers) and IEEE float (32 bit), either as a plain
    /// tag or as an extensible sub-format.
    pub fn from_waveformat_bytes(bytes: &[u8]) -> Result<Self, FormatError> {
        if bytes.len() < WAVEFORMATEX_LEN {
            return Err(FormatError::Truncated {
                needed: WAVEFORMATEX_LEN,
                got: bytes.len(),
            });
        }
        let tag = u16_at(bytes, 0);
        let channels = u16_at(bytes, 2);
        let sample_rate = u32_at(bytes, 4);
        let block_align = u16_at(bytes, 12);
        let bits_per_sample = u16_at(bytes, 14);

        let (effective_tag, valid_bits) = if tag == TAG_EXTENSIBLE {
            if bytes.len() < WAVEFORMATEXTENSIBLE_LEN {
                return Err(FormatError::Truncated {
                    needed: WAVEFORMATEXTENSIBLE_LEN,
                    got: bytes.len(),
                });
            }
            let valid = u16_at(bytes, 18);
            let guid = &bytes[24..40];
            if guid[4..16] != KS_SUBTYPE_TAIL || guid[2..4] != [0, 0] {
                return Err(FormatError::Unsupported(format!(
                    "extensible sub-format {guid:02x?}"
                )));
            }
            let sub = u16_at(guid, 0);
            (sub, if valid == 0 { bits_per_sample } else { valid })
        } else {
            (tag, bits_per_sample)
        };

        let sample_format = match (effective_tag, bits_per_sample) {
            (TAG_IEEE_FLOAT, 32) => SampleFormat::F32,
            (TAG_PCM, 16) => SampleFormat::I16,
            (TAG_PCM, 24) => SampleFormat::I24,
            (TAG_PCM, 32) => SampleFormat::I32,
            (t, b) => {
                return Err(FormatError::Unsupported(format!(
                    "format tag {t:#06x} with {b} bits per sample"
                )));
            }
        };
        if channels == 0 {
            return Err(FormatError::Unsupported("zero channels".into()));
        }
        if usize::from(block_align) < usize::from(channels) * usize::from(bits_per_sample / 8) {
            return Err(FormatError::Unsupported(format!(
                "block align {block_align} too small for {channels} x {bits_per_sample} bit"
            )));
        }
        Ok(Self {
            sample_rate,
            channels,
            block_align,
            bits_per_sample,
            valid_bits_per_sample: valid_bits,
            sample_format,
        })
    }

    /// Number of whole frames in `byte_len` bytes, or an error for a partial frame.
    pub fn frames_in(&self, byte_len: usize) -> Result<usize, FormatError> {
        let ba = usize::from(self.block_align);
        if !byte_len.is_multiple_of(ba) {
            return Err(FormatError::PartialFrame {
                len: byte_len,
                block_align: self.block_align,
            });
        }
        Ok(byte_len / ba)
    }
}

fn sample_to_f32(fmt: SampleFormat, s: &[u8]) -> f32 {
    match fmt {
        SampleFormat::F32 => f32::from_le_bytes([s[0], s[1], s[2], s[3]]),
        SampleFormat::I16 => f32::from(i16::from_le_bytes([s[0], s[1]])) / 32_768.0,
        // Sign-extend by placing the 3 bytes in the top of an i32.
        SampleFormat::I24 => (i32::from_le_bytes([0, s[0], s[1], s[2]]) >> 8) as f32 / 8_388_608.0,
        // Valid bits are left-justified in the container, so full-scale is always 2^31.
        SampleFormat::I32 => {
            (f64::from(i32::from_le_bytes([s[0], s[1], s[2], s[3]])) / 2_147_483_648.0) as f32
        }
    }
}

/// Converts one interleaved packet to mono f32 (mean of all channels) and appends it to `out`.
///
/// Returns the number of frames appended. The native sample rate is kept.
pub fn append_mono_f32(
    data: &[u8],
    fmt: &MixFormat,
    out: &mut Vec<f32>,
) -> Result<usize, FormatError> {
    let frames = fmt.frames_in(data.len())?;
    let width = usize::from(fmt.bits_per_sample / 8);
    let channels = usize::from(fmt.channels);
    let scale = 1.0 / channels as f32;
    out.reserve(frames);
    for frame in data.chunks_exact(usize::from(fmt.block_align)) {
        let sum: f32 = (0..channels)
            .map(|c| sample_to_f32(fmt.sample_format, &frame[c * width..(c + 1) * width]))
            .sum();
        out.push(sum * scale);
    }
    Ok(frames)
}

/// Converts one interleaved packet to interleaved f32, keeping every channel in device
/// order, and appends it to `out`.
///
/// Returns the number of frames appended (samples / channels). The native sample rate
/// is kept.
pub fn append_interleaved_f32(
    data: &[u8],
    fmt: &MixFormat,
    out: &mut Vec<f32>,
) -> Result<usize, FormatError> {
    let frames = fmt.frames_in(data.len())?;
    let width = usize::from(fmt.bits_per_sample / 8);
    let channels = usize::from(fmt.channels);
    out.reserve(frames * channels);
    for frame in data.chunks_exact(usize::from(fmt.block_align)) {
        out.extend(
            (0..channels)
                .map(|c| sample_to_f32(fmt.sample_format, &frame[c * width..(c + 1) * width])),
        );
    }
    Ok(frames)
}

/// How a track's packets are converted to f32.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChannelMode {
    /// One channel: the mean of the device channels ([`append_mono_f32`]). The mic.
    Mono,
    /// Every device channel, interleaved ([`append_interleaved_f32`]). The loopback
    /// reference, so a multichannel echo canceller sees each loudspeaker.
    Interleaved,
}

impl ChannelMode {
    /// Channels per frame of the track for packets in `fmt`.
    pub fn track_channels(self, fmt: &MixFormat) -> u16 {
        match self {
            Self::Mono => 1,
            Self::Interleaved => fmt.channels,
        }
    }

    /// Converts one packet in `fmt` and appends it to `out`; returns frames appended.
    pub fn append_f32(
        self,
        data: &[u8],
        fmt: &MixFormat,
        out: &mut Vec<f32>,
    ) -> Result<usize, FormatError> {
        match self {
            Self::Mono => append_mono_f32(data, fmt, out),
            Self::Interleaved => append_interleaved_f32(data, fmt, out),
        }
    }
}

/// The float format (rate, channels) to request from the audio engine so that a stream
/// on an endpoint whose mix format is `mix` delivers `rate` and `channels` (each
/// `None` = the mix format's own), or `None` when the mix format already matches and
/// no conversion is needed.
pub fn conversion_target(
    mix: &MixFormat,
    rate: Option<u32>,
    channels: Option<u16>,
) -> Option<(u32, u16)> {
    let rate = rate.unwrap_or(mix.sample_rate);
    let channels = channels.unwrap_or(mix.channels);
    (rate != mix.sample_rate || channels != mix.channels).then_some((rate, channels))
}

/// Checks that `delivered` (the format a stream was initialised with) has the requested
/// `rate` and `channels` (each `None` = any). A re-opened stream keeps its track's rate
/// and channel count only through this check.
///
/// # Errors
/// [`FormatError::NotDelivered`] on any difference.
pub fn check_delivered(
    delivered: &MixFormat,
    rate: Option<u32>,
    channels: Option<u16>,
) -> Result<(), FormatError> {
    let rate_ok = rate.is_none_or(|r| r == delivered.sample_rate);
    let channels_ok = channels.is_none_or(|c| c == delivered.channels);
    if rate_ok && channels_ok {
        Ok(())
    } else {
        Err(FormatError::NotDelivered {
            requested_rate: rate,
            requested_channels: channels,
            rate: delivered.sample_rate,
            channels: delivered.channels,
        })
    }
}

/// Little-endian `WAVEFORMATEX` image for interleaved 32-bit IEEE float at
/// `sample_rate` with `channels` channels (`cbSize` 0). Used where a stream format is
/// requested rather than read from the device: process loopback has no mix format,
/// and a re-opened device is asked for the track's original rate.
pub fn float_waveformat_bytes(sample_rate: u32, channels: u16) -> [u8; WAVEFORMATEX_LEN] {
    let block_align = channels.saturating_mul(4);
    let avg_bytes = sample_rate.saturating_mul(u32::from(block_align));
    let mut b = [0u8; WAVEFORMATEX_LEN];
    b[0..2].copy_from_slice(&TAG_IEEE_FLOAT.to_le_bytes());
    b[2..4].copy_from_slice(&channels.to_le_bytes());
    b[4..8].copy_from_slice(&sample_rate.to_le_bytes());
    b[8..12].copy_from_slice(&avg_bytes.to_le_bytes());
    b[12..14].copy_from_slice(&block_align.to_le_bytes());
    b[14..16].copy_from_slice(&32u16.to_le_bytes());
    // cbSize (16..18) stays 0.
    b
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroU16;

    use earshot_core::capture::mix_to_mono;

    use super::*;

    #[test]
    fn float_waveformat_round_trips() {
        let b = float_waveformat_bytes(48_000, 2);
        assert_eq!(b.len(), 18);
        assert_eq!(u32_at(&b, 8), 384_000, "nAvgBytesPerSec");
        assert_eq!(u16_at(&b, 16), 0, "cbSize");
        let f = MixFormat::from_waveformat_bytes(&b).unwrap();
        assert_eq!(
            f,
            MixFormat {
                sample_rate: 48_000,
                channels: 2,
                block_align: 8,
                bits_per_sample: 32,
                valid_bits_per_sample: 32,
                sample_format: SampleFormat::F32,
            }
        );
        let f = MixFormat::from_waveformat_bytes(&float_waveformat_bytes(44_100, 1)).unwrap();
        assert_eq!((f.sample_rate, f.channels, f.block_align), (44_100, 1, 4));
    }

    /// Builds a `WAVEFORMATEX` image.
    fn wfx(tag: u16, ch: u16, rate: u32, bits: u16, cb_size: u16) -> Vec<u8> {
        let ba = ch * bits / 8;
        let mut v = Vec::new();
        v.extend_from_slice(&tag.to_le_bytes());
        v.extend_from_slice(&ch.to_le_bytes());
        v.extend_from_slice(&rate.to_le_bytes());
        v.extend_from_slice(&(rate * u32::from(ba)).to_le_bytes());
        v.extend_from_slice(&ba.to_le_bytes());
        v.extend_from_slice(&bits.to_le_bytes());
        v.extend_from_slice(&cb_size.to_le_bytes());
        v
    }

    fn extensible(ch: u16, rate: u32, bits: u16, valid: u16, sub: u16) -> Vec<u8> {
        let mut v = wfx(TAG_EXTENSIBLE, ch, rate, bits, 22);
        v.extend_from_slice(&valid.to_le_bytes());
        v.extend_from_slice(&3u32.to_le_bytes()); // front left | front right
        v.extend_from_slice(&u32::from(sub).to_le_bytes());
        v.extend_from_slice(&KS_SUBTYPE_TAIL);
        v
    }

    #[test]
    fn parses_extensible_float_stereo_48k() {
        let f = MixFormat::from_waveformat_bytes(&extensible(2, 48_000, 32, 32, 3)).unwrap();
        assert_eq!(f.sample_format, SampleFormat::F32);
        assert_eq!((f.sample_rate, f.channels, f.block_align), (48_000, 2, 8));
    }

    #[test]
    fn parses_extensible_24_in_32_pcm() {
        let f = MixFormat::from_waveformat_bytes(&extensible(2, 44_100, 32, 24, 1)).unwrap();
        assert_eq!(f.sample_format, SampleFormat::I32);
        assert_eq!(f.valid_bits_per_sample, 24);
    }

    #[test]
    fn parses_plain_pcm16_and_packed_24() {
        let f = MixFormat::from_waveformat_bytes(&wfx(TAG_PCM, 1, 16_000, 16, 0)).unwrap();
        assert_eq!(f.sample_format, SampleFormat::I16);
        let f = MixFormat::from_waveformat_bytes(&wfx(TAG_PCM, 2, 96_000, 24, 0)).unwrap();
        assert_eq!(f.sample_format, SampleFormat::I24);
        assert_eq!(f.block_align, 6);
    }

    #[test]
    fn rejects_unsupported_and_truncated() {
        assert!(matches!(
            MixFormat::from_waveformat_bytes(&wfx(TAG_IEEE_FLOAT, 2, 48_000, 64, 0)),
            Err(FormatError::Unsupported(_))
        ));
        assert!(matches!(
            MixFormat::from_waveformat_bytes(&wfx(TAG_PCM, 1, 8_000, 8, 0)),
            Err(FormatError::Unsupported(_))
        ));
        assert!(matches!(
            MixFormat::from_waveformat_bytes(&[0u8; 10]),
            Err(FormatError::Truncated { .. })
        ));
        let short_ext = &extensible(2, 48_000, 32, 32, 3)[..30];
        assert!(matches!(
            MixFormat::from_waveformat_bytes(short_ext),
            Err(FormatError::Truncated { .. })
        ));
        let mut bad_guid = extensible(2, 48_000, 32, 32, 3);
        bad_guid[39] ^= 0xff;
        assert!(matches!(
            MixFormat::from_waveformat_bytes(&bad_guid),
            Err(FormatError::Unsupported(_))
        ));
    }

    fn fmt(sample_format: SampleFormat, channels: u16, bits: u16) -> MixFormat {
        MixFormat {
            sample_rate: 48_000,
            channels,
            block_align: channels * bits / 8,
            bits_per_sample: bits,
            valid_bits_per_sample: bits,
            sample_format,
        }
    }

    #[test]
    fn float_stereo_downmixes_to_mean() {
        let samples = [0.5f32, -0.5, 1.0, 0.0, -0.25, -0.75];
        let bytes: Vec<u8> = samples.iter().flat_map(|s| s.to_le_bytes()).collect();
        let mut out = Vec::new();
        let n = append_mono_f32(&bytes, &fmt(SampleFormat::F32, 2, 32), &mut out).unwrap();
        assert_eq!(n, 3);
        assert_eq!(out, [0.0, 0.5, -0.5]);
    }

    #[test]
    fn int16_scales_to_unit_range() {
        let samples = [i16::MIN, 0, 16_384, i16::MAX];
        let bytes: Vec<u8> = samples.iter().flat_map(|s| s.to_le_bytes()).collect();
        let mut out = vec![9.0];
        append_mono_f32(&bytes, &fmt(SampleFormat::I16, 1, 16), &mut out).unwrap();
        assert_eq!(out[0], 9.0, "appends, does not clear");
        assert_eq!(out[1], -1.0);
        assert_eq!(out[2], 0.0);
        assert_eq!(out[3], 0.5);
        assert!((out[4] - 32_767.0 / 32_768.0).abs() < 1e-7);
    }

    #[test]
    fn packed_int24_sign_extends() {
        // -8388608 (min), -1, 4194304 (0.5)
        let bytes = [0x00, 0x00, 0x80, 0xff, 0xff, 0xff, 0x00, 0x00, 0x40];
        let mut out = Vec::new();
        append_mono_f32(&bytes, &fmt(SampleFormat::I24, 1, 24), &mut out).unwrap();
        assert_eq!(out[0], -1.0);
        assert!((out[1] + 1.0 / 8_388_608.0).abs() < 1e-9);
        assert_eq!(out[2], 0.5);
    }

    #[test]
    fn int32_container_is_left_justified() {
        let samples = [i32::MIN, 0x4000_0000, 0x0000_0100];
        let bytes: Vec<u8> = samples.iter().flat_map(|s| s.to_le_bytes()).collect();
        let mut f = fmt(SampleFormat::I32, 1, 32);
        f.valid_bits_per_sample = 24;
        let mut out = Vec::new();
        append_mono_f32(&bytes, &f, &mut out).unwrap();
        assert_eq!(out[0], -1.0);
        assert_eq!(out[1], 0.5);
        // smallest 24-bit step in a 32-bit container
        assert!((out[2] - 1.0 / 8_388_608.0).abs() < 1e-12);
    }

    #[test]
    fn block_align_padding_is_skipped() {
        // 2 channels of i16 in a 6-byte frame (2 padding bytes per frame).
        let mut f = fmt(SampleFormat::I16, 2, 16);
        f.block_align = 6;
        let bytes = [
            0x00, 0x40, 0x00, 0x40, 0xAA, 0xAA, 0x00, 0xC0, 0x00, 0xC0, 0xAA, 0xAA,
        ];
        let mut out = Vec::new();
        assert_eq!(append_mono_f32(&bytes, &f, &mut out).unwrap(), 2);
        assert_eq!(out, [0.5, -0.5]);
    }

    #[test]
    fn interleaved_keeps_every_channel_in_order() {
        let samples = [0.5f32, -0.5, 1.0, 0.0, -0.25, -0.75];
        let bytes: Vec<u8> = samples.iter().flat_map(|s| s.to_le_bytes()).collect();
        let f = fmt(SampleFormat::F32, 2, 32);
        let mut out = vec![9.0];
        let n = append_interleaved_f32(&bytes, &f, &mut out).unwrap();
        assert_eq!(n, 3, "frames, not samples");
        assert_eq!(out[0], 9.0, "appends, does not clear");
        assert_eq!(out[1..], samples);
        // The mono mix of the interleaved packet is exactly the old downmix.
        let mut mono = Vec::new();
        append_mono_f32(&bytes, &f, &mut mono).unwrap();
        let mut mixed = Vec::new();
        mix_to_mono(&out[1..], NonZeroU16::new(2).unwrap(), &mut mixed);
        assert_eq!(mixed, mono);
    }

    #[test]
    fn interleaved_skips_padding_and_converts_integers() {
        // 2 channels of i16 in a 6-byte frame (2 padding bytes per frame).
        let mut f = fmt(SampleFormat::I16, 2, 16);
        f.block_align = 6;
        let bytes = [
            0x00, 0x40, 0x00, 0xC0, 0xAA, 0xAA, 0x00, 0xC0, 0x00, 0x40, 0xAA, 0xAA,
        ];
        let mut out = Vec::new();
        assert_eq!(append_interleaved_f32(&bytes, &f, &mut out).unwrap(), 2);
        assert_eq!(out, [0.5, -0.5, -0.5, 0.5]);
        assert!(matches!(
            append_interleaved_f32(&bytes[..5], &f, &mut out),
            Err(FormatError::PartialFrame { .. })
        ));
        assert_eq!(out.len(), 4, "nothing appended on error");
    }

    #[test]
    fn channel_modes_choose_the_track_layout() {
        let stereo = fmt(SampleFormat::F32, 2, 32);
        assert_eq!(ChannelMode::Mono.track_channels(&stereo), 1);
        assert_eq!(ChannelMode::Interleaved.track_channels(&stereo), 2);
        let bytes: Vec<u8> = [0.5f32, 0.25]
            .iter()
            .flat_map(|s| s.to_le_bytes())
            .collect();
        let mut out = Vec::new();
        assert_eq!(
            ChannelMode::Mono
                .append_f32(&bytes, &stereo, &mut out)
                .unwrap(),
            1
        );
        assert_eq!(out, [0.375]);
        out.clear();
        assert_eq!(
            ChannelMode::Interleaved
                .append_f32(&bytes, &stereo, &mut out)
                .unwrap(),
            1
        );
        assert_eq!(out, [0.5, 0.25]);
    }

    #[test]
    fn conversion_is_requested_only_when_rate_or_channels_differ() {
        let mix = fmt(SampleFormat::I16, 2, 16);
        assert_eq!(conversion_target(&mix, None, None), None);
        assert_eq!(conversion_target(&mix, Some(48_000), Some(2)), None);
        assert_eq!(
            conversion_target(&mix, Some(44_100), None),
            Some((44_100, 2)),
            "a new rate keeps the device channels"
        );
        assert_eq!(
            conversion_target(&mix, None, Some(1)),
            Some((48_000, 1)),
            "a new channel count keeps the device rate"
        );
        assert_eq!(
            conversion_target(&fmt(SampleFormat::F32, 6, 32), Some(44_100), Some(2)),
            Some((44_100, 2))
        );
    }

    #[test]
    fn delivered_format_must_match_the_request() {
        let f = fmt(SampleFormat::I16, 2, 16);
        assert_eq!(check_delivered(&f, None, None), Ok(()));
        assert_eq!(check_delivered(&f, Some(48_000), Some(2)), Ok(()));
        let err = check_delivered(&f, Some(44_100), Some(1)).unwrap_err();
        assert_eq!(
            err,
            FormatError::NotDelivered {
                requested_rate: Some(44_100),
                requested_channels: Some(1),
                rate: 48_000,
                channels: 2,
            }
        );
        assert_eq!(
            err.to_string(),
            "stream delivers 48000 Hz, 2 ch instead of the requested 44100 Hz, 1 ch"
        );
        assert!(
            check_delivered(&f, None, Some(6)).is_err(),
            "channels alone"
        );
        assert_eq!(
            check_delivered(&f, Some(16_000), None)
                .unwrap_err()
                .to_string(),
            "stream delivers 48000 Hz, 2 ch instead of the requested 16000 Hz, any ch"
        );
        // What `CaptureStream::open` initialises (the mix format, or the float
        // conversion target) always passes.
        for (mix, rate, channels) in [
            (f, None, None),
            (f, Some(44_100), None),
            (f, None, Some(1)),
            (fmt(SampleFormat::F32, 6, 32), Some(44_100), Some(2)),
        ] {
            let delivered = match conversion_target(&mix, rate, channels) {
                Some((r, ch)) => {
                    MixFormat::from_waveformat_bytes(&float_waveformat_bytes(r, ch)).unwrap()
                }
                None => mix,
            };
            assert_eq!(check_delivered(&delivered, rate, channels), Ok(()));
        }
    }

    #[test]
    fn partial_frame_is_an_error() {
        let mut out = Vec::new();
        let err = append_mono_f32(&[0u8; 7], &fmt(SampleFormat::F32, 2, 32), &mut out);
        assert_eq!(
            err,
            Err(FormatError::PartialFrame {
                len: 7,
                block_align: 8
            })
        );
        assert!(out.is_empty());
    }
}
