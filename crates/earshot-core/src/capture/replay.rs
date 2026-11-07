//! Buffer replay source: drives the pipeline headlessly from in-memory audio.

use std::collections::VecDeque;
use std::convert::Infallible;
use std::fmt;
use std::num::NonZeroU16;
use std::time::Duration;

use super::raw::{RawTimeline, RawTrackTimeline};
use super::{
    AudioSource, CaptureSession, CaptureSink, DeviceTime, Discontinuity, DiscontinuityKind,
    EndOfStream, EndReason, MONO, SourceStatus, TrackId, TrackInfo,
};
use crate::audio::{MonoBuffer, Sample, SampleRate, WavAudio};

/// `timestamps.json` schema versions [`ReplayBuilder::from_raw_timeline`] reads.
pub const SUPPORTED_SCHEMAS: [u32; 4] = [1, 2, 3, 4];

/// Audio of one replayed track: frames of `channels` interleaved samples at `rate`
/// (the [`CaptureSink`] layout). A [`MonoBuffer`] converts into a one-channel buffer.
#[derive(Debug, Clone, PartialEq)]
pub struct TrackBuffer {
    rate: SampleRate,
    channels: NonZeroU16,
    samples: Vec<Sample>,
}

impl TrackBuffer {
    /// Wraps interleaved `samples`.
    ///
    /// # Errors
    /// [`ReplayError::PartialFrame`] if `samples.len()` is not a multiple of `channels`.
    pub fn new(
        rate: SampleRate,
        channels: NonZeroU16,
        samples: Vec<Sample>,
    ) -> Result<Self, ReplayError> {
        if !samples.len().is_multiple_of(usize::from(channels.get())) {
            return Err(ReplayError::PartialFrame {
                samples: samples.len(),
                channels: channels.get(),
            });
        }
        Ok(Self {
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

    /// Samples per frame.
    #[must_use]
    pub fn channels(&self) -> NonZeroU16 {
        self.channels
    }

    /// Number of frames.
    #[must_use]
    pub fn frames(&self) -> u64 {
        (self.samples.len() / usize::from(self.channels.get())) as u64
    }

    /// The interleaved samples.
    #[must_use]
    pub fn samples(&self) -> &[Sample] {
        &self.samples
    }
}

impl From<MonoBuffer> for TrackBuffer {
    fn from(b: MonoBuffer) -> Self {
        Self {
            rate: b.rate,
            channels: MONO,
            samples: b.samples,
        }
    }
}

impl TryFrom<WavAudio> for TrackBuffer {
    type Error = ReplayError;

    /// Keeps the WAV's channels (a raw recording's `loopback.wav` may be stereo).
    fn try_from(w: WavAudio) -> Result<Self, ReplayError> {
        // WavAudio guarantees at least one channel and whole frames.
        let (rate, channels) = (w.rate(), w.channel_count());
        Self::new(rate, channels, w.into_samples())
    }
}

/// Default packet length of a uniform replay (WASAPI's usual 10 ms period).
pub const DEFAULT_PACKET: Duration = Duration::from_millis(10);

/// Why a replay could not be built.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReplayError {
    /// An edit names the reference track but there is none.
    NoReferenceTrack,
    /// An edit reaches `frame`, past the track's `len` frames.
    EditOutOfRange {
        /// Track edited.
        track: TrackId,
        /// First frame out of range.
        frame: u64,
        /// Track length in frames.
        len: u64,
    },
    /// The packet duration is under one frame at some track's rate.
    PacketTooShort,
    /// A raw timeline's rate differs from its buffer's.
    RateMismatch {
        /// Track.
        track: TrackId,
        /// `wav_sample_rate` in the timeline.
        expected: u32,
        /// Buffer rate.
        actual: u32,
    },
    /// A raw timeline's `frames_written` differs from its buffer's length.
    LengthMismatch {
        /// Track.
        track: TrackId,
        /// `frames_written` in the timeline.
        expected: u64,
        /// Buffer length.
        actual: u64,
    },
    /// A raw packet's kept frames do not fit in the buffer.
    PacketOutOfRange {
        /// Track.
        track: TrackId,
        /// Index in the packet log.
        packet: usize,
    },
    /// A raw timeline's `wav_channels` differs from its buffer's channel count.
    ChannelMismatch {
        /// Track.
        track: TrackId,
        /// `wav_channels` in the timeline.
        expected: u16,
        /// Buffer channels.
        actual: u16,
    },
    /// A buffer's sample count is not a whole number of frames (or it has no channels).
    PartialFrame {
        /// Samples in the buffer.
        samples: usize,
        /// Channels per frame.
        channels: u16,
    },
    /// The raw timeline is malformed (zero QPC rate, rate or channels, timestamps
    /// overflow).
    BadTimeline,
    /// The raw timeline's `schema_version` is not in [`SUPPORTED_SCHEMAS`].
    UnsupportedSchema(u32),
}

impl fmt::Display for ReplayError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoReferenceTrack => {
                write!(f, "edit names the reference track, but there is none")
            }
            Self::EditOutOfRange { track, frame, len } => {
                write!(f, "{track:?} edit reaches frame {frame}, track has {len}")
            }
            Self::PacketTooShort => write!(f, "packet duration is under one frame"),
            Self::RateMismatch {
                track,
                expected,
                actual,
            } => write!(
                f,
                "{track:?} timeline is {expected} Hz, buffer is {actual} Hz"
            ),
            Self::LengthMismatch {
                track,
                expected,
                actual,
            } => write!(
                f,
                "{track:?} timeline has {expected} frames, buffer has {actual}"
            ),
            Self::PacketOutOfRange { track, packet } => {
                write!(f, "{track:?} packet {packet} does not fit in the buffer")
            }
            Self::ChannelMismatch {
                track,
                expected,
                actual,
            } => write!(
                f,
                "{track:?} timeline has {expected} channels, buffer has {actual}"
            ),
            Self::PartialFrame { samples, channels } => write!(
                f,
                "{samples} samples are not whole frames of {channels} channels"
            ),
            Self::BadTimeline => write!(f, "malformed raw timeline"),
            Self::UnsupportedSchema(v) => {
                write!(f, "unsupported timestamps.json schema version {v}")
            }
        }
    }
}

impl std::error::Error for ReplayError {}

/// Frames `[start, start + len)` of a track, the first stamped `ts`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Step {
    start: u64,
    len: u64,
    ts: DeviceTime,
}

/// A marker placed before the first frames at or after `pos`.
#[derive(Debug, Clone, PartialEq, Eq)]
struct PendingMarker {
    pos: u64,
    /// Explicit device time; `None` takes the next frames' timestamp.
    at: Option<DeviceTime>,
    kind: DiscontinuityKind,
}

#[derive(Debug, Clone)]
struct TrackSpec {
    buffer: TrackBuffer,
    device_name: Option<String>,
    /// Packet log (raw timeline); `None` for uniform packets.
    steps: Option<Vec<Step>>,
    markers: Vec<PendingMarker>,
    gaps: Vec<(u64, u64)>,
}

impl TrackSpec {
    fn new(buffer: TrackBuffer) -> Self {
        Self {
            buffer,
            device_name: None,
            steps: None,
            markers: Vec::new(),
            gaps: Vec::new(),
        }
    }

    /// Length in frames.
    fn len(&self) -> u64 {
        self.buffer.frames()
    }
}

/// Builds a [`ReplaySource`].
///
/// Two modes:
/// - [`ReplayBuilder::new`]: uniform packets ([`DEFAULT_PACKET`] or
///   [`packet_duration`](Self::packet_duration)) at each track's own rate, frame `k`
///   stamped `origin + k / rate`.
/// - [`ReplayBuilder::from_raw_timeline`]: the packets of a raw recording, at their
///   recorded device times, with discontinuity flags and device changes as markers;
///   synthesized silence is not delivered (it was never captured).
///
/// In both modes [`gap`](Self::gap) and [`discontinuity`](Self::discontinuity) inject
/// faults, addressed by frame index in the track buffer. Tracks may have any channel
/// count ([`TrackBuffer`]); packets carry whole frames.
#[derive(Debug, Clone)]
pub struct ReplayBuilder {
    origin: DeviceTime,
    packet: Duration,
    end: Option<DeviceTime>,
    mic: TrackSpec,
    reference: Option<TrackSpec>,
}

fn qpc_ticks(qpc: u64, units: u64) -> Result<DeviceTime, ReplayError> {
    DeviceTime::from_units(qpc, units).ok_or(ReplayError::BadTimeline)
}

fn raw_track(
    track: TrackId,
    raw: &RawTrackTimeline,
    buffer: TrackBuffer,
    origin: DeviceTime,
    units: u64,
) -> Result<TrackSpec, ReplayError> {
    let rate = SampleRate::new(raw.wav_sample_rate).ok_or(ReplayError::BadTimeline)?;
    if raw.wav_channels == 0 {
        return Err(ReplayError::BadTimeline);
    }
    if buffer.rate != rate {
        return Err(ReplayError::RateMismatch {
            track,
            expected: rate.hz(),
            actual: buffer.rate.hz(),
        });
    }
    if buffer.channels.get() != raw.wav_channels {
        return Err(ReplayError::ChannelMismatch {
            track,
            expected: raw.wav_channels,
            actual: buffer.channels.get(),
        });
    }
    let len = buffer.frames();
    if len != raw.frames_written {
        return Err(ReplayError::LengthMismatch {
            track,
            expected: raw.frames_written,
            actual: len,
        });
    }
    // Timestamps derived later (splits, End, trailing markers) never pass this track
    // end or the step ends checked below, so plain `after_frames` is safe after here.
    origin
        .checked_after_frames(len, rate)
        .ok_or(ReplayError::BadTimeline)?;
    let mut spec = TrackSpec::new(buffer);
    spec.device_name = Some(raw.device_name.clone());
    // Device changes first: on the same frame they precede the new stream's flags.
    for dc in &raw.device_changes {
        spec.markers.push(PendingMarker {
            pos: dc.track_frame,
            at: Some(qpc_ticks(dc.qpc, units)?),
            kind: DiscontinuityKind::DeviceChanged {
                device_name: dc.device_name.clone(),
                rate,
                warning: None,
            },
        });
    }
    let mut steps: Vec<Step> = Vec::with_capacity(raw.packets.len());
    // As the live source: WASAPI flags the first packet of a stream (the track's first,
    // and the first after each re-open) as a discontinuity on start, which is not lost
    // data. A stream's first packet is its first one with kept frames.
    let mut reopens: Vec<u64> = raw.device_changes.iter().map(|dc| dc.track_frame).collect();
    reopens.sort_unstable();
    let mut reopens = reopens.into_iter().peekable();
    let mut stream_start = true;
    for (i, p) in raw.packets.iter().enumerate() {
        let out_of_range = ReplayError::PacketOutOfRange { track, packet: i };
        let kept = u64::from(p.frames)
            .checked_sub(p.trimmed_frames)
            .ok_or(out_of_range.clone())?;
        if p.track_frame.checked_add(kept).is_none_or(|e| e > len) {
            return Err(out_of_range);
        }
        // A timestamp-error packet has no usable qpc. The WASAPI timeline placed it
        // directly after the previous packet, so stamp it at that packet's end on this
        // track: deriving it from track_frame would drop the device drift the previous
        // qpc carries and could step backwards without a marker.
        let ts = if p.timestamp_error {
            match steps.last() {
                Some(prev) => prev.ts.checked_after_frames(prev.len, rate),
                None => origin.checked_after_frames(p.track_frame, rate),
            }
        } else {
            qpc_ticks(p.qpc, units)?.checked_after_frames(p.trimmed_frames, rate)
        }
        .ok_or(ReplayError::BadTimeline)?;
        // A stamp at or before the previous packet's start is unusable (the WASAPI
        // timeline appended the packet after the previous one): stamp it at that
        // packet's end, as the live source does, so timestamps never go back. A stamp
        // inside the previous packet is jitter and is kept.
        let ts = match steps.last() {
            Some(prev) if ts <= prev.ts => prev
                .ts
                .checked_after_frames(prev.len, rate)
                .ok_or(ReplayError::BadTimeline)?,
            _ => ts,
        };
        ts.checked_after_frames(kept, rate)
            .ok_or(ReplayError::BadTimeline)?;
        while reopens.next_if(|&f| p.track_frame >= f).is_some() {
            stream_start = true;
        }
        if p.discontinuity && !stream_start {
            spec.markers.push(PendingMarker {
                pos: p.track_frame,
                at: Some(ts),
                kind: DiscontinuityKind::DataLost,
            });
        }
        if kept > 0 {
            stream_start = false;
            steps.push(Step {
                start: p.track_frame,
                len: kept,
                ts,
            });
        }
    }
    spec.steps = Some(steps);
    Ok(spec)
}

impl ReplayBuilder {
    /// Uniform replay of `mic` (no reference) with frame 0 at `origin`.
    #[must_use]
    pub fn new(origin: DeviceTime, mic: impl Into<TrackBuffer>) -> Self {
        Self {
            origin,
            packet: DEFAULT_PACKET,
            end: None,
            mic: TrackSpec::new(mic.into()),
            reference: None,
        }
    }

    /// Adds a reference track (mono or interleaved), frame 0 also at the origin.
    #[must_use]
    pub fn reference(mut self, reference: impl Into<TrackBuffer>) -> Self {
        self.reference = Some(TrackSpec::new(reference.into()));
        self
    }

    /// Packet length of a uniform replay (ignored for raw timelines, which keep their
    /// recorded packets).
    #[must_use]
    pub fn packet_duration(mut self, packet: Duration) -> Self {
        self.packet = packet;
        self
    }

    /// Replays a raw recording: `mic` and `loopback` are its WAV tracks (at
    /// `wav_sample_rate` with `wav_channels` interleaved channels, `frames_written`
    /// frames long; a stereo v3 `loopback.wav` read with
    /// [`read_wav`](crate::audio::read_wav) converts with `TrackBuffer::try_from`) and
    /// `timeline` its parsed `timestamps.json`. Device times are converted to 100 ns
    /// ticks. Each packet is stamped at its own qpc, except a timestamp-error packet or
    /// one stamped at or before the previous packet's start, which is stamped at the
    /// previous packet's end (as the live WASAPI source does).
    ///
    /// # Errors
    /// The buffers do not match the timeline, or the timeline is malformed.
    pub fn from_raw_timeline(
        timeline: &RawTimeline,
        mic: impl Into<TrackBuffer>,
        loopback: impl Into<TrackBuffer>,
    ) -> Result<Self, ReplayError> {
        if !SUPPORTED_SCHEMAS.contains(&timeline.schema_version) {
            return Err(ReplayError::UnsupportedSchema(timeline.schema_version));
        }
        let units = timeline.qpc_units_per_second;
        let origin = qpc_ticks(timeline.start_qpc, units)?;
        let end = qpc_ticks(timeline.end_qpc, units)?;
        let mic = raw_track(TrackId::Mic, &timeline.mic, mic.into(), origin, units)?;
        let reference = raw_track(
            TrackId::Reference,
            &timeline.loopback,
            loopback.into(),
            origin,
            units,
        )?;
        Ok(Self {
            origin,
            packet: DEFAULT_PACKET,
            end: Some(end),
            mic,
            reference: Some(reference),
        })
    }

    fn spec_mut(&mut self, track: TrackId) -> Result<&mut TrackSpec, ReplayError> {
        match track {
            TrackId::Mic => Ok(&mut self.mic),
            TrackId::Reference => self.reference.as_mut().ok_or(ReplayError::NoReferenceTrack),
        }
    }

    /// Drops frames `[start, start + frames)` of `track` without a marker, like a
    /// loopback with nothing playing or a device that skipped packets. Frames after
    /// the gap keep their true timestamps.
    ///
    /// # Errors
    /// No such track, or the range runs past the track.
    pub fn gap(mut self, track: TrackId, start: u64, frames: u64) -> Result<Self, ReplayError> {
        let spec = self.spec_mut(track)?;
        let len = spec.len();
        let end = start.saturating_add(frames);
        if end > len {
            return Err(ReplayError::EditOutOfRange {
                track,
                frame: end,
                len,
            });
        }
        spec.gaps.push((start, end));
        Ok(self)
    }

    /// Emits a `kind` marker on `track` just before frame `at` (or the first frame
    /// delivered after it, if `at` falls in a gap), stamped with that frame's time.
    /// `at` may equal the track length (a marker after the last frame).
    ///
    /// # Errors
    /// No such track, or `at` is past the track.
    pub fn discontinuity(
        mut self,
        track: TrackId,
        at: u64,
        kind: DiscontinuityKind,
    ) -> Result<Self, ReplayError> {
        let spec = self.spec_mut(track)?;
        let len = spec.len();
        if at > len {
            return Err(ReplayError::EditOutOfRange {
                track,
                frame: at,
                len,
            });
        }
        spec.markers.push(PendingMarker {
            pos: at,
            at: None,
            kind,
        });
        Ok(self)
    }

    /// Builds the source.
    ///
    /// # Errors
    /// [`ReplayError::PacketTooShort`] if a uniform packet is under one frame.
    pub fn build(self) -> Result<ReplaySource, ReplayError> {
        let session = CaptureSession::new(
            self.origin,
            track_info(&self.mic),
            self.reference.as_ref().map(track_info),
        );
        let mic = plan_track(self.mic, self.origin, self.packet)?;
        let reference = self
            .reference
            .map(|r| plan_track(r, self.origin, self.packet))
            .transpose()?;
        let end = self.end.unwrap_or_else(|| {
            [Some(&mic), reference.as_ref()]
                .into_iter()
                .flatten()
                .map(|t| t.natural_end)
                .max()
                .unwrap_or(self.origin)
        });
        Ok(ReplaySource {
            session,
            tracks: [Some(mic), reference],
            end,
            stop_requested: false,
            ended: false,
        })
    }
}

fn track_info(spec: &TrackSpec) -> TrackInfo {
    TrackInfo {
        rate: spec.buffer.rate,
        channels: spec.buffer.channels,
        device_name: spec.device_name.clone(),
    }
}

/// Removes `gaps` from `steps`, splitting steps that straddle a gap edge.
fn cut_gaps(steps: Vec<Step>, gaps: &[(u64, u64)], rate: SampleRate) -> Vec<Step> {
    let mut out = steps;
    for &(g0, g1) in gaps {
        if g0 >= g1 {
            continue;
        }
        let mut next = Vec::with_capacity(out.len() + 1);
        for s in out {
            let s_end = s.start + s.len;
            if s_end <= g0 || s.start >= g1 {
                next.push(s);
                continue;
            }
            if s.start < g0 {
                next.push(Step {
                    start: s.start,
                    len: g0 - s.start,
                    ts: s.ts,
                });
            }
            if s_end > g1 {
                next.push(Step {
                    start: g1,
                    len: s_end - g1,
                    ts: s.ts.after_frames(g1 - s.start, rate),
                });
            }
        }
        out = next;
    }
    out
}

/// Splits steps so that `pos` starts a step when it falls strictly inside one.
fn split_at(steps: &mut Vec<Step>, pos: u64, rate: SampleRate) {
    if let Some(i) = steps
        .iter()
        .position(|s| s.start < pos && pos < s.start + s.len)
    {
        let s = steps[i];
        let head = pos - s.start;
        steps[i].len = head;
        steps.insert(
            i + 1,
            Step {
                start: pos,
                len: s.len - head,
                ts: s.ts.after_frames(head, rate),
            },
        );
    }
}

#[derive(Debug, Clone)]
enum Item {
    Frames(Step),
    Marker(Discontinuity),
}

impl Item {
    fn ts(&self) -> DeviceTime {
        match self {
            Self::Frames(s) => s.ts,
            Self::Marker(d) => d.at,
        }
    }
}

#[derive(Debug, Clone)]
struct TrackPlan {
    /// Interleaved samples.
    samples: Vec<Sample>,
    rate: SampleRate,
    /// Samples per frame.
    channels: usize,
    items: VecDeque<Item>,
    /// End of the whole buffer: `origin + len / rate`.
    natural_end: DeviceTime,
    /// End of the last delivered frames.
    delivered_end: Option<DeviceTime>,
}

fn plan_track(
    spec: TrackSpec,
    origin: DeviceTime,
    packet: Duration,
) -> Result<TrackPlan, ReplayError> {
    let rate = spec.buffer.rate;
    let len = spec.len();
    let steps = match spec.steps {
        Some(steps) => steps,
        None => {
            let n = rate.frames_for(packet) as u64;
            if n == 0 {
                return Err(ReplayError::PacketTooShort);
            }
            (0..len.div_ceil(n))
                .map(|k| Step {
                    start: k * n,
                    len: n.min(len - k * n),
                    ts: origin.after_frames(k * n, rate),
                })
                .collect()
        }
    };
    let mut steps = cut_gaps(steps, &spec.gaps, rate);
    let mut markers = spec.markers;
    // Stable: markers on the same frame keep insertion order.
    markers.sort_by_key(|m| m.pos);
    for m in &markers {
        split_at(&mut steps, m.pos, rate);
    }
    let mut items = VecDeque::with_capacity(steps.len() + markers.len());
    let mut markers = markers.into_iter().peekable();
    for s in steps {
        while let Some(m) = markers.next_if(|m| m.pos <= s.start) {
            items.push_back(Item::Marker(Discontinuity {
                at: m.at.unwrap_or(s.ts),
                kind: m.kind,
            }));
        }
        items.push_back(Item::Frames(s));
    }
    for m in markers {
        items.push_back(Item::Marker(Discontinuity {
            at: m.at.unwrap_or_else(|| origin.after_frames(m.pos, rate)),
            kind: m.kind,
        }));
    }
    Ok(TrackPlan {
        channels: usize::from(spec.buffer.channels.get()),
        samples: spec.buffer.samples,
        rate,
        items,
        natural_end: origin.after_frames(len, rate),
        delivered_end: None,
    })
}

/// In-memory [`AudioSource`]: replays mic and optional reference buffers (built by
/// [`ReplayBuilder`]) with device timestamps, one packet or marker per
/// [`pump`](AudioSource::pump), merged across tracks in timestamp order (mic first on
/// ties). Never [`SourceStatus::Idle`].
#[derive(Debug, Clone)]
pub struct ReplaySource {
    session: CaptureSession,
    tracks: [Option<TrackPlan>; 2],
    end: DeviceTime,
    stop_requested: bool,
    ended: bool,
}

impl ReplaySource {
    fn finish(&mut self, sink: &mut dyn CaptureSink, reason: EndReason) -> SourceStatus {
        let at = match reason {
            EndReason::Finished => self.end,
            // A replay never fails; `Failed` ends where delivery stopped, like a stop.
            EndReason::Stopped | EndReason::Failed => self
                .tracks
                .iter()
                .flatten()
                .filter_map(|t| t.delivered_end)
                .max()
                .unwrap_or(self.session.origin),
        };
        self.ended = true;
        sink.end(EndOfStream { at, reason });
        SourceStatus::Ended
    }
}

impl AudioSource for ReplaySource {
    type Error = Infallible;

    fn session(&self) -> &CaptureSession {
        &self.session
    }

    fn pump(&mut self, sink: &mut dyn CaptureSink) -> Result<SourceStatus, Infallible> {
        if self.ended {
            return Ok(SourceStatus::Ended);
        }
        if self.stop_requested {
            return Ok(self.finish(sink, EndReason::Stopped));
        }
        // Earliest head across tracks; `min_by_key` keeps the first (mic) on ties.
        let next = self
            .tracks
            .iter()
            .enumerate()
            .filter_map(|(i, t)| Some((i, t.as_ref()?.items.front()?.ts())))
            .min_by_key(|&(_, ts)| ts)
            .map(|(i, _)| i);
        let Some(i) = next else {
            return Ok(self.finish(sink, EndReason::Finished));
        };
        let track_id = if i == 0 {
            TrackId::Mic
        } else {
            TrackId::Reference
        };
        let Some(track) = self.tracks[i].as_mut() else {
            unreachable!("selected track exists");
        };
        match track.items.pop_front() {
            Some(Item::Frames(s)) => {
                let ch = track.channels;
                let range = s.start as usize * ch..(s.start + s.len) as usize * ch;
                sink.frames(track_id, &track.samples[range], s.ts);
                track.delivered_end = Some(s.ts.after_frames(s.len, track.rate));
            }
            Some(Item::Marker(d)) => sink.discontinuity(track_id, d),
            None => unreachable!("selected track has a head item"),
        }
        Ok(SourceStatus::Delivered)
    }

    fn stop(&mut self) {
        self.stop_requested = true;
    }
}
