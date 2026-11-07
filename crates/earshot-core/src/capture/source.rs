//! Session description, sink and source traits.

use std::num::NonZeroU16;
use std::time::Duration;

use super::DeviceTime;
use crate::audio::{Sample, SampleRate};

/// One channel: the channel count of every mono track.
pub const MONO: NonZeroU16 = NonZeroU16::MIN;

/// One of the (at most two) tracks of a capture session.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum TrackId {
    /// The microphone ("Me"). Always present.
    Mic,
    /// The far-end reference ("Them"): loopback on Windows. Absent on Android and in
    /// in-room mode.
    Reference,
}

/// What a track delivers: `f32` frames of `channels` interleaved samples at `rate`
/// (layout in [`CaptureSink`]).
///
/// The mic is mono (adapters downmix it). The reference keeps the render endpoint's
/// channels (normally stereo), because a multichannel echo canceller needs each
/// loudspeaker's signal; consumers that need one channel (the "Them" ASR track) mix
/// it down with [`mix_to_mono`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrackInfo {
    /// Sample rate of the frames when the session starts. A
    /// [`DiscontinuityKind::DeviceChanged`] marker announces a new rate.
    pub rate: SampleRate,
    /// Samples per frame, fixed for the session (a re-opened device is asked for the
    /// same count). Channel order is the device's (for stereo: left, right).
    pub channels: NonZeroU16,
    /// Endpoint name, when known.
    pub device_name: Option<String>,
}

impl TrackInfo {
    /// A mono track at `rate` with no device name.
    #[must_use]
    pub fn new(rate: SampleRate) -> Self {
        Self {
            rate,
            channels: MONO,
            device_name: None,
        }
    }

    /// Sets the channel count.
    #[must_use]
    pub fn with_channels(mut self, channels: NonZeroU16) -> Self {
        self.channels = channels;
        self
    }

    /// Whole frames in `samples` interleaved samples of this track.
    #[must_use]
    pub fn frames_in(&self, samples: usize) -> usize {
        samples / usize::from(self.channels.get())
    }

    /// Sets the endpoint name.
    #[must_use]
    pub fn with_device_name(mut self, name: impl Into<String>) -> Self {
        self.device_name = Some(name.into());
        self
    }
}

/// Description of an opened capture session: mic plus optional reference, sharing one
/// device clock whose `origin` is meeting time zero.
///
/// The origin is taken by the adapter once, before any stream starts, and is frame 0
/// of every track (the raw-recording rule): a track never records its own start.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CaptureSession {
    /// Device time of meeting time zero.
    pub origin: DeviceTime,
    /// Microphone track.
    pub mic: TrackInfo,
    /// Reference (loopback) track, if captured.
    pub reference: Option<TrackInfo>,
}

impl CaptureSession {
    /// A session starting at `origin`.
    #[must_use]
    pub fn new(origin: DeviceTime, mic: TrackInfo, reference: Option<TrackInfo>) -> Self {
        Self {
            origin,
            mic,
            reference,
        }
    }

    /// The track's description, `None` for an absent reference.
    #[must_use]
    pub fn track(&self, id: TrackId) -> Option<&TrackInfo> {
        match id {
            TrackId::Mic => Some(&self.mic),
            TrackId::Reference => self.reference.as_ref(),
        }
    }

    /// The tracks present, mic first.
    pub fn tracks(&self) -> impl Iterator<Item = TrackId> + use<> {
        let reference = self.reference.is_some().then_some(TrackId::Reference);
        std::iter::once(TrackId::Mic).chain(reference)
    }

    /// Meeting-relative time of `ts`, or `None` before the origin.
    #[must_use]
    pub fn meeting_time(&self, ts: DeviceTime) -> Option<Duration> {
        ts.checked_duration_since(self.origin)
    }

    /// Device time of meeting time `t`, rounded to the nearest tick.
    #[must_use]
    pub fn device_time(&self, t: Duration) -> DeviceTime {
        self.origin + t
    }

    /// Meeting-relative frame index of `ts` at `rate` (nearest frame), or `None`
    /// before the origin.
    ///
    /// A frame index is only meaningful while a track keeps one rate. After a
    /// [`DiscontinuityKind::DeviceChanged`] with a new `rate`, key positions on
    /// [`meeting_time`](Self::meeting_time) (a `Duration`), not on this frame index.
    #[must_use]
    pub fn meeting_frame(&self, ts: DeviceTime, rate: SampleRate) -> Option<u64> {
        let ticks = ts.ticks().checked_sub(self.origin.ticks())?;
        Some(DeviceTime::frames_in_ticks(ticks, rate))
    }
}

/// A break in a track's sample stream. Gaps without a marker are legal too (an idle
/// loopback delivers nothing); the next frames' timestamp always says where they belong.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Discontinuity {
    /// Device time of the break: the timestamp of the next frames on the track, unless
    /// the source knows better (a device change carries the time the old stream closed).
    pub at: DeviceTime,
    /// What happened.
    pub kind: DiscontinuityKind,
}

/// Kind of [`Discontinuity`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiscontinuityKind {
    /// The device reported lost or glitched data (WASAPI `DATA_DISCONTINUITY`, an
    /// Android overrun). Downstream state that assumes continuity (AEC filter, VAD
    /// hangover) should be treated as suspect.
    DataLost,
    /// The stream was re-opened on another endpoint (default-device change, device
    /// loss and recovery). Frames after this marker are at `rate`; when it differs from
    /// the previous rate, key track positions on
    /// [`CaptureSession::meeting_time`] (a `Duration`), not on
    /// [`CaptureSession::meeting_frame`].
    DeviceChanged {
        /// New endpoint name, `None` while no device is available.
        device_name: Option<String>,
        /// Rate of the frames that follow.
        rate: SampleRate,
        /// Something the user should hear about the new endpoint, set by a source that
        /// judges endpoints (the WASAPI source: once per switch onto a Bluetooth hands-free
        /// mic, T6.24, and when a chosen endpoint is given up for the default, T7.4i);
        /// `None` otherwise and in every replay.
        warning: Option<EndpointWarning>,
    },
    /// Something to tell the user about the track's endpoints. Nothing about the audio changed:
    /// the frames go on as they were (T6.15).
    Note {
        /// What to tell.
        warning: EndpointWarning,
    },
}

/// A finding about a capture endpoint that the user should hear about (see
/// [`DiscontinuityKind::DeviceChanged`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EndpointWarning {
    /// The endpoint is a Bluetooth headset in hands-free mode: narrowband recording, and
    /// the headset's playback degrades while the mic is open.
    BluetoothHandsFree {
        /// The endpoint's native rate (Hz), when known.
        sample_rate: Option<u32>,
    },
    /// The endpoint the user chose for this track kept failing to open, so the source gave
    /// it up and the new endpoint is the default one for the rest of the capture.
    ChosenEndpointGone {
        /// The chosen endpoint's name as the user chose it (may be empty).
        chosen: String,
        /// The new endpoint's own warning, if any (a Bluetooth hands-free mic).
        new_endpoint: Option<Box<EndpointWarning>>,
    },
    /// A call plays on another output, which could not be opened, so the track stays on the one
    /// it records (T6.15).
    CallOutputRefused {
        /// The output the call plays on.
        wanted: String,
        /// The output still recorded; `None` when the track has no stream at the moment.
        used: Option<String>,
    },
}

/// Why a source ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EndReason {
    /// The source ran out of audio (end of a replay).
    Finished,
    /// [`AudioSource::stop`] was called.
    Stopped,
    /// Capture ended on an error (a live device that failed for good, or a capture
    /// thread that exited without ending the stream); the source reported the error
    /// before its end. What was captured until then is complete.
    Failed,
}

/// End of the session, delivered exactly once, after every other event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EndOfStream {
    /// Device time capture stopped. Tracks may be padded up to here.
    pub at: DeviceTime,
    /// Why it ended.
    pub reason: EndReason,
}

/// Receives capture data. Implemented by the pipeline (T6.2, `push_frames`) or by a
/// forwarding adapter that sends [`CaptureEvent`]s across a channel.
///
/// Contract kept by every [`AudioSource`]: per track, events arrive in capture order
/// and frame timestamps never go backwards except across a marker; `end` is called
/// exactly once and nothing follows it.
///
/// Frame layout: `frames` is interleaved, frame-major. With `c` =
/// [`TrackInfo::channels`] of the track, sample `i * c + ch` is channel `ch` of frame
/// `i`, `frames.len()` is a whole multiple of `c`, and `device_ts` is the time of
/// frame 0. A mono track is the case `c == 1`.
pub trait CaptureSink {
    /// Interleaved frames of `track` (layout above), the first captured at `device_ts`.
    fn frames(&mut self, track: TrackId, frames: &[Sample], device_ts: DeviceTime);
    /// A break in `track`'s stream, before the frames it affects.
    fn discontinuity(&mut self, track: TrackId, discontinuity: Discontinuity);
    /// The session ended.
    fn end(&mut self, end: EndOfStream);
}

/// Owned form of one sink call, for channels and tests.
#[derive(Debug, Clone, PartialEq)]
pub enum CaptureEvent {
    /// [`CaptureSink::frames`].
    Frames {
        /// Track.
        track: TrackId,
        /// Device time of the first frame.
        device_ts: DeviceTime,
        /// Interleaved frames, laid out as for [`CaptureSink::frames`].
        frames: Vec<Sample>,
    },
    /// [`CaptureSink::discontinuity`].
    Discontinuity {
        /// Track.
        track: TrackId,
        /// The break.
        discontinuity: Discontinuity,
    },
    /// [`CaptureSink::end`].
    End(EndOfStream),
}

impl CaptureEvent {
    /// Replays this event into `sink`.
    pub fn deliver(self, sink: &mut dyn CaptureSink) {
        match self {
            Self::Frames {
                track,
                device_ts,
                frames,
            } => sink.frames(track, &frames, device_ts),
            Self::Discontinuity {
                track,
                discontinuity,
            } => sink.discontinuity(track, discontinuity),
            Self::End(end) => sink.end(end),
        }
    }
}

impl CaptureSink for Vec<CaptureEvent> {
    fn frames(&mut self, track: TrackId, frames: &[Sample], device_ts: DeviceTime) {
        self.push(CaptureEvent::Frames {
            track,
            device_ts,
            frames: frames.to_vec(),
        });
    }

    fn discontinuity(&mut self, track: TrackId, discontinuity: Discontinuity) {
        self.push(CaptureEvent::Discontinuity {
            track,
            discontinuity,
        });
    }

    fn end(&mut self, end: EndOfStream) {
        self.push(CaptureEvent::End(end));
    }
}

/// Result of one [`AudioSource::pump`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceStatus {
    /// Something was delivered; more may be available now.
    Delivered,
    /// Nothing is available yet; pump again after the device signals (WASAPI event,
    /// JNI callback). Core never waits.
    Idle,
    /// [`CaptureSink::end`] has been called; later pumps deliver nothing.
    Ended,
}

/// A capture source: mic plus optional reference with device timestamps.
///
/// Sans-IO and thread-free: the caller owns the loop. A live adapter (WASAPI T6.3b,
/// Android JNI T10.3) waits on its device signal on its own capture thread and then
/// calls [`pump`](Self::pump), which delivers whatever is ready into the sink without
/// blocking. Replay sources deliver one packet (or marker) per pump.
pub trait AudioSource {
    /// Device or adapter failure. A failed pump may be retried or the source dropped.
    type Error: std::fmt::Debug;

    /// What this source captures. Fixed for the session's lifetime; rate changes are
    /// announced by [`DiscontinuityKind::DeviceChanged`].
    fn session(&self) -> &CaptureSession;

    /// Delivers available events into `sink`.
    ///
    /// # Errors
    /// A device or adapter failure.
    fn pump(&mut self, sink: &mut dyn CaptureSink) -> Result<SourceStatus, Self::Error>;

    /// Requests the end of capture. The next pump delivers [`EndOfStream`] with
    /// [`EndReason::Stopped`] (after flushing anything already captured, for live
    /// sources) and returns [`SourceStatus::Ended`].
    fn stop(&mut self);
}

/// Appends the mono mix of `interleaved` (`channels` samples per frame) to `out`: the
/// mean of each frame's channels, the downmix the WASAPI adapter applies to the mic.
/// Returns the frames appended; a trailing partial frame is ignored. Mono input is
/// copied unchanged.
///
/// For consumers that need one channel of a multichannel track (the "Them" ASR
/// track). The echo canceller takes the reference with its channels.
pub fn mix_to_mono(interleaved: &[Sample], channels: NonZeroU16, out: &mut Vec<Sample>) -> usize {
    let ch = usize::from(channels.get());
    if ch == 1 {
        out.extend_from_slice(interleaved);
        return interleaved.len();
    }
    let scale = 1.0 / f32::from(channels.get());
    let before = out.len();
    out.extend(
        interleaved
            .chunks_exact(ch)
            .map(|frame| frame.iter().sum::<f32>() * scale),
    );
    out.len() - before
}
