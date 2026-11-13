//! The parts of a raw recording's `timestamps.json` (written by `earshot-cli record-raw`,
//! `earshot-capture-wasapi::recording`) that replay needs. Schema v1, v2 (adds
//! `device_changes`), v3 (per-track `wav_channels` may exceed 1: the loopback WAV
//! keeps its device channels, interleaved), v4 (a mic device change may carry
//! `hands_free`) and v5 (the mic section may carry `processing`); unknown fields are
//! ignored. Parsing (for example with `serde_json::from_reader`) is done by the caller.

use serde::Deserialize;

fn mono() -> u16 {
    1
}

/// Root of `timestamps.json`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct RawTimeline {
    /// Layout version (1 to 5).
    pub schema_version: u32,
    /// Units of every `*qpc` field per second (10 000 000 for WASAPI QPC).
    pub qpc_units_per_second: u64,
    /// Origin: frame 0 of both WAVs.
    pub start_qpc: u64,
    /// Capture stop.
    pub end_qpc: u64,
    /// `mic.wav` description.
    pub mic: RawTrackTimeline,
    /// `loopback.wav` description.
    pub loopback: RawTrackTimeline,
}

/// Per-track section of `timestamps.json`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct RawTrackTimeline {
    /// Endpoint friendly name.
    pub device_name: String,
    /// WAV sample rate.
    pub wav_sample_rate: u32,
    /// WAV channel count; samples are interleaved. Always 1 in v1 and v2 (a missing
    /// field reads as 1); in v3 the loopback keeps its device channels.
    #[serde(default = "mono")]
    pub wav_channels: u16,
    /// Frames in the WAV file (samples per channel).
    pub frames_written: u64,
    /// Frames of synthesized silence (gaps without packets, tail padding).
    pub synthesized_silence_frames: u64,
    /// Every packet received, in order.
    pub packets: Vec<RawPacket>,
    /// Stream re-opens (schema v2; empty for v1).
    #[serde(default)]
    pub device_changes: Vec<RawDeviceChange>,
    /// How the track's stream was opened (schema v5, T12.32): `"raw"` on a mic opened without
    /// Windows' default signal processing; absent for the default stream, on the loopback and
    /// in v1 to v4. Informational: replay is the same either way.
    #[serde(default)]
    pub processing: RawProcessing,
}

/// How a raw recording's mic stream was opened (schema v5). Any other value is refused: a
/// take whose mic was opened some other way cannot be read as either.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RawProcessing {
    /// With the endpoint's default signal processing (what every meeting records).
    #[default]
    Default,
    /// Without it (`earshot-cli record-raw --mic-raw`).
    Raw,
}

/// One captured packet.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct RawPacket {
    /// Device time of the packet's first frame (before trimming).
    pub qpc: u64,
    /// Frames in the packet (before trimming).
    pub frames: u32,
    /// WAV frame of the first kept frame.
    pub track_frame: u64,
    /// Leading frames dropped because they predate the origin.
    pub trimmed_frames: u64,
    /// Flagged silent (the WAV holds zeros).
    pub silent: bool,
    /// Flagged as a data discontinuity.
    pub discontinuity: bool,
    /// `qpc` is not reliable.
    pub timestamp_error: bool,
}

/// A stream re-open (schema v2).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct RawDeviceChange {
    /// Device time the old stream was closed.
    pub qpc: u64,
    /// WAV frame where the discontinuity starts.
    pub track_frame: u64,
    /// New endpoint name, `None` when no device could be opened.
    pub device_name: Option<String>,
    /// Set when the re-open switched the mic onto a Bluetooth hands-free mic (schema v4;
    /// `None` when absent and in v1 to v3). Replay never turns it into a marker warning.
    #[serde(default)]
    pub hands_free: Option<RawHandsFree>,
}

/// The Bluetooth hands-free warning of a mic re-open (schema v4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
pub struct RawHandsFree {
    /// The new mic's mix-format rate (Hz), when known.
    pub sample_rate: Option<u32>,
}
