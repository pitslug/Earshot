//! Capture abstractions (PLAN.md 3.3, 3.4): a session of a mic track plus an optional
//! reference (loopback) track, delivered as timestamped frames: the mic mono, the
//! reference with its device's channels interleaved (stereo on a desktop, T6.3c).
//!
//! - [`DeviceTime`]: monotonic device timestamp in 100 ns ticks (WASAPI QPC units).
//! - [`CaptureSession`]: what was opened (per-track rate, channels and device), and
//!   the origin that maps device time to meeting time.
//! - [`CaptureSink`]: receives `frames(track, frames, device_ts)` (interleaved),
//!   discontinuity markers and end-of-stream. The T6.2 pipeline implements it.
//! - [`mix_to_mono`]: the mono mix of a multichannel track, for consumers that need
//!   one channel (the "Them" ASR track).
//! - [`AudioSource`]: a pump-driven producer. Core owns no threads: adapters (WASAPI
//!   T6.3b, Android JNI T10.3) wait on their device on their own thread, then pump.
//! - [`ReplaySource`] / [`ReplayBuilder`]: in-memory replay of buffers
//!   ([`TrackBuffer`]), optionally following a raw recording's packet log
//!   ([`RawTimeline`]), with injected gaps and discontinuities. Opening files is left
//!   to the CLI, runtime or tests.

mod raw;
mod replay;
mod source;
mod time;

#[cfg(test)]
mod tests;

pub use raw::{RawDeviceChange, RawHandsFree, RawPacket, RawTimeline, RawTrackTimeline};
pub use replay::{
    DEFAULT_PACKET, ReplayBuilder, ReplayError, ReplaySource, SUPPORTED_SCHEMAS, TrackBuffer,
};
pub use source::{
    AudioSource, CaptureEvent, CaptureSession, CaptureSink, Discontinuity, DiscontinuityKind,
    EndOfStream, EndReason, EndpointWarning, MONO, SourceStatus, TrackId, TrackInfo, mix_to_mono,
};
pub use time::DeviceTime;
