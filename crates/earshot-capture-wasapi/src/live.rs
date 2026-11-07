//! Live capture as the core `AudioSource` (T6.3b). Platform independent; the WASAPI
//! capture thread (`wasapi::WasapiSource`) plugs into it.
//!
//! - [`TrackEmitter`]: turns one track's WASAPI packets and re-opens into core
//!   [`CaptureSink`] calls: frames at their QPC device time (100 ns, passed through as
//!   [`DeviceTime`]), [`DiscontinuityKind::DataLost`] for `DATA_DISCONTINUITY`,
//!   [`DiscontinuityKind::DeviceChanged`] for a reported re-open.
//! - [`ChannelSink`] / [`EmitterTrack`]: the capture-thread side, sending
//!   [`LiveMsg`]s over a channel.
//! - [`ChannelSource`]: the consumer side, an [`AudioSource`] whose `pump` only
//!   `try_recv`s (never blocks) and which guarantees [`CaptureSink::end`] exactly once,
//!   last.
//!
//! Frames are delivered at the device's native rate (normally 48 kHz), not resampled:
//! resampling to 16 kHz is a pipeline stage (PLAN.md 3.3, "resample 16k -> AEC"), and
//! the AEC's drift compensation resamples the reference in core. The mic is mono, the
//! reference keeps its device channels interleaved (T6.3c; the session's
//! `TrackInfo::channels`). A re-open keeps the track's rate and channel count (the
//! audio engine converts), and the rate is announced in every `DeviceChanged` marker.
//!
//! Placement follows the raw-recording rules of [`TrackTimeline`](crate::TrackTimeline):
//! frame 0 of every track is the session origin (frames stamped before it are trimmed,
//! a later start is a legal gap), the first packet after a re-open is trimmed to the end
//! of the previous one, and a packet without a usable timestamp (flagged, later than the
//! capture horizon, or starting at or before the previous packet's start) is stamped at
//! the previous packet's end. Per track, packet start timestamps therefore always
//! increase (the core contract: `device_ts` of successive `frames` calls never goes
//! backwards). A packet may still start inside the previous one (start < ts < end) by up
//! to the device's timestamp jitter (measured up to 1 ms on USB): that is not repeated
//! audio, so it keeps its stamp and all its frames, and replay of a raw recording places
//! it the same way. Silence is never synthesized:
//! gaps are left to the consumer, as the core contract allows.

use std::fmt;
use std::num::NonZeroU16;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, Sender, TryRecvError};
use std::time::Duration;

use earshot_core::audio::{Sample, SampleRate};
use earshot_core::capture::{
    AudioSource, CaptureEvent, CaptureSession, CaptureSink, DeviceTime, Discontinuity,
    DiscontinuityKind, EndOfStream, EndReason, EndpointWarning, MONO, SourceStatus, TrackId,
    TrackInfo,
};

use crate::endpoint::{EndpointProperties, HandsFreeFollower};
use crate::follow::ReopenReason;
use crate::observer::{ReopenOutcome, TrackObserver, reopen_is_reported};
use crate::timeline::PacketInfo;

/// Converts one track's packets and re-opens into sink calls.
#[derive(Clone, Debug)]
pub struct TrackEmitter {
    track: TrackId,
    rate: SampleRate,
    /// Samples per frame of the packets and the emitted frames.
    channels: NonZeroU16,
    origin: DeviceTime,
    /// While set, leading frames stamped before it are trimmed: the origin until the
    /// first kept frame, the previous packet's end after a re-open.
    anchor: Option<DeviceTime>,
    /// Start and end device time of the last delivered frames.
    last: Option<(DeviceTime, DeviceTime)>,
    /// Packets stamped later than this are treated as timestamp errors.
    horizon: Option<DeviceTime>,
    /// For a mic track: when a re-open lands on a Bluetooth hands-free mic (T6.24).
    hands_free: Option<HandsFreeFollower>,
    zeros: Vec<Sample>,
}

impl TrackEmitter {
    /// Mono emitter for `track` at `rate` whose frame 0 is `origin`.
    pub fn new(track: TrackId, rate: SampleRate, origin: DeviceTime) -> Self {
        Self {
            track,
            rate,
            channels: MONO,
            origin,
            anchor: Some(origin),
            last: None,
            horizon: None,
            hands_free: None,
            zeros: Vec::new(),
        }
    }

    /// A mic track that started on `start` (its properties; `None` when unknown): a
    /// re-open onto a Bluetooth hands-free mic carries
    /// [`EndpointWarning::BluetoothHandsFree`] in its `DeviceChanged` marker, once per
    /// switch onto such a mic ([`HandsFreeFollower`]).
    #[must_use]
    pub fn with_mic_warnings(mut self, start: Option<&EndpointProperties>) -> Self {
        self.hands_free = Some(HandsFreeFollower::new(start));
        self
    }

    /// Packets (and emitted frames) carry `channels` interleaved samples per frame.
    pub fn with_channels(mut self, channels: NonZeroU16) -> Self {
        self.channels = channels;
        self
    }

    /// Packets stamped after `qpc` are bogus (see [`TrackObserver::set_horizon`]).
    pub fn set_horizon(&mut self, qpc: u64) {
        self.horizon = Some(DeviceTime::from_ticks(qpc));
    }

    /// Current rate of the track's frames.
    pub fn rate(&self) -> SampleRate {
        self.rate
    }

    /// Device time just after the last delivered frame.
    pub fn end(&self) -> Option<DeviceTime> {
        self.last.map(|(_, end)| end)
    }

    /// Delivers one packet into `sink`: a `DataLost` marker first when the packet is
    /// flagged as a discontinuity (except the first packet of a stream, after the origin
    /// or a re-open, which WASAPI flags on start), then its frames (zeros when flagged silent), unless
    /// every frame predates the anchor.
    ///
    /// `samples` is interleaved with the emitter's channel count; trimming drops whole
    /// frames.
    ///
    /// # Panics
    /// If the packet is not silent and `samples.len() != info.frames * channels`.
    pub fn packet(&mut self, info: PacketInfo, samples: &[f32], sink: &mut dyn CaptureSink) {
        let ch = usize::from(self.channels.get());
        assert!(
            info.silent || samples.len() == info.frames as usize * ch,
            "packet has {} frames of {ch} channels but {} samples were given",
            info.frames,
            samples.len()
        );
        let frames = u64::from(info.frames);
        if frames == 0 {
            return;
        }
        let bogus = info.timestamp_error || self.horizon.is_some_and(|h| info.qpc > h.ticks());
        let mut ts = if bogus {
            self.end().unwrap_or(self.origin)
        } else {
            DeviceTime::from_ticks(info.qpc)
        };
        let mut trim = 0;
        if let Some(anchor) = self.anchor
            && ts < anchor
        {
            // Frames before the anchor predate the origin (or overlap the previous
            // stream); the first kept frame sits exactly at the anchor.
            trim = DeviceTime::frames_in_ticks(anchor.ticks() - ts.ticks(), self.rate);
            if trim >= frames {
                return;
            }
            ts = ts.after_frames(trim, self.rate).max(anchor);
        }
        // A packet starting at or before the previous one's start has no usable
        // timestamp. One starting inside the previous packet keeps its stamp and every
        // frame: within one stream that is timestamp jitter (up to 1 ms on USB), never
        // repeated audio, and replay of a raw recording places it the same way.
        if let Some((start, end)) = self.last
            && ts <= start
        {
            ts = end;
        }
        // WASAPI flags the first packet after a stream starts as a discontinuity; nothing
        // was lost then (the anchor already places the stream), so that flag is not a
        // DataLost. The first packet of a stream is the one that clears the anchor.
        let first_of_stream = self.anchor.is_some();
        if info.discontinuity && !first_of_stream {
            sink.discontinuity(
                self.track,
                Discontinuity {
                    at: ts,
                    kind: DiscontinuityKind::DataLost,
                },
            );
        }
        let kept = frames - trim;
        if info.silent {
            self.zeros.resize(kept as usize * ch, 0.0);
            sink.frames(self.track, &self.zeros, ts);
        } else {
            sink.frames(self.track, &samples[trim as usize * ch..], ts);
        }
        self.last = Some((ts, ts.after_frames(kept, self.rate)));
        self.anchor = None;
    }

    /// The stream was closed at `qpc` and re-opened (or not): the next packet is trimmed
    /// to the end of the previous one, and a reported re-open (see
    /// [`reopen_is_reported`]) is announced as `DeviceChanged` at `qpc`, with a warning
    /// when a mic track (see [`with_mic_warnings`](Self::with_mic_warnings)) lands on a
    /// Bluetooth hands-free mic.
    pub fn reopened(
        &mut self,
        qpc: u64,
        reason: ReopenReason,
        outcome: ReopenOutcome<'_>,
        sink: &mut dyn CaptureSink,
    ) {
        self.anchor = Some(self.end().unwrap_or(self.origin));
        let (device_name, warning) = match outcome {
            ReopenOutcome::Opened {
                device_name,
                rate,
                endpoint,
                gave_up,
                ..
            } => {
                if let Some(rate) = SampleRate::new(rate) {
                    self.rate = rate;
                }
                let hands_free = self
                    .hands_free
                    .as_mut()
                    .and_then(|f| f.opened(endpoint))
                    .map(|mic| EndpointWarning::BluetoothHandsFree {
                        sample_rate: mic.sample_rate,
                    });
                // A chosen endpoint given up for the default (T7.4i) says so, carrying the
                // default's own warning.
                let warning = match gave_up {
                    Some(chosen) => Some(EndpointWarning::ChosenEndpointGone {
                        chosen: chosen.to_owned(),
                        new_endpoint: hands_free.map(Box::new),
                    }),
                    None => hands_free,
                };
                (Some(device_name.to_owned()), warning)
            }
            ReopenOutcome::Failed { .. } => (None, None),
        };
        if reopen_is_reported(reason, outcome.is_opened()) {
            sink.discontinuity(
                self.track,
                Discontinuity {
                    at: DeviceTime::from_ticks(qpc),
                    kind: DiscontinuityKind::DeviceChanged {
                        device_name,
                        rate: self.rate,
                        warning,
                    },
                },
            );
        }
    }
}

impl TrackEmitter {
    /// The call plays on `wanted`, which could not be opened (T6.15): one note at `qpc`, and the
    /// track's frames go on untouched.
    pub fn call_output_refused(
        &mut self,
        qpc: u64,
        wanted: &str,
        used: Option<&str>,
        sink: &mut dyn CaptureSink,
    ) {
        sink.discontinuity(
            self.track,
            Discontinuity {
                at: DeviceTime::from_ticks(qpc),
                kind: DiscontinuityKind::Note {
                    warning: EndpointWarning::CallOutputRefused {
                        wanted: wanted.to_owned(),
                        used: used.map(str::to_owned),
                    },
                },
            },
        );
    }
}

/// What the capture thread sends to the consumer.
#[derive(Debug)]
pub enum LiveMsg<E> {
    /// One sink call.
    Event(CaptureEvent),
    /// Capture failed; an [`EndOfStream`] follows.
    Failed(E),
}

/// How much audio the live channel may hold before frames are dropped: the runtime pumps
/// about every 10 ms while idle and is late only while a live decode runs (seconds), so
/// 30 s of queue means the consumer has stalled.
pub const LIVE_QUEUE_CAP: Duration = Duration::from_secs(30);

/// The sample limit of a live channel carrying `mic` and `reference` (none in the in-room
/// mode): [`LIVE_QUEUE_CAP`] of every track at its rate and channel count (as opened; a
/// re-open keeps them).
#[must_use]
pub fn queue_limit(mic: &TrackInfo, reference: Option<&TrackInfo>) -> u64 {
    let per_second = |t: &TrackInfo| u64::from(t.rate.hz()) * u64::from(t.channels.get());
    LIVE_QUEUE_CAP.as_secs() * (per_second(mic) + reference.map_or(0, per_second))
}

/// Samples queued in a live channel, shared by its sinks (which add what they send) and
/// its [`ChannelSource`] (which releases what it receives).
#[derive(Debug, Clone, Default)]
pub struct QueueGauge(Arc<AtomicU64>);

impl QueueGauge {
    /// An empty gauge.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Samples currently queued.
    #[must_use]
    pub fn queued(&self) -> u64 {
        self.0.load(Ordering::Acquire)
    }

    fn add(&self, n: u64) {
        self.0.fetch_add(n, Ordering::AcqRel);
    }

    fn release(&self, n: u64) {
        let _ = self.0.try_update(Ordering::AcqRel, Ordering::Acquire, |q| {
            Some(q.saturating_sub(n))
        });
    }
}

/// A queue limit shared by the sinks of one channel.
#[derive(Debug, Clone)]
struct QueueCap {
    gauge: QueueGauge,
    /// Most samples queued at once.
    limit: u64,
}

/// Capture-thread side of the channel: a [`CaptureSink`] that sends every call as a
/// [`LiveMsg::Event`]. Sending never blocks.
///
/// Without a cap ([`ChannelSink::new`]) the channel is unbounded: a slow consumer costs
/// memory (about 0.4 MB/s for two 48 kHz tracks), never captured audio. With a cap
/// ([`ChannelSink::with_cap`], what the live source uses): once the queue would exceed
/// the limit, frames are dropped until the consumer has drained it to half the limit;
/// then that track's next frames are preceded by one [`DiscontinuityKind::DataLost`]
/// marker at their own time (the pipeline restarts the track there and fills the gap
/// with silence). Markers and the end of stream are never dropped.
#[derive(Debug)]
pub struct ChannelSink<E> {
    tx: Sender<LiveMsg<E>>,
    closed: bool,
    cap: Option<QueueCap>,
    /// Frames are being dropped (until the queue halves).
    dropping: bool,
}

impl<E> ChannelSink<E> {
    /// Sink sending into `tx` (unbounded).
    pub fn new(tx: Sender<LiveMsg<E>>) -> Self {
        Self {
            tx,
            closed: false,
            cap: None,
            dropping: false,
        }
    }

    /// Sink sending into `tx`, dropping frames while `gauge` holds more than `limit`
    /// samples (see the type docs). Every sink of one channel and its [`ChannelSource`]
    /// share the gauge.
    pub fn with_cap(tx: Sender<LiveMsg<E>>, gauge: QueueGauge, limit: u64) -> Self {
        Self {
            cap: Some(QueueCap { gauge, limit }),
            ..Self::new(tx)
        }
    }

    /// Sends `msg`; a gone consumer marks the sink closed.
    pub fn send(&mut self, msg: LiveMsg<E>) {
        if !self.closed && self.tx.send(msg).is_err() {
            self.closed = true;
        }
    }

    /// The consumer has dropped its receiver.
    pub fn is_closed(&self) -> bool {
        self.closed
    }
}

impl<E> CaptureSink for ChannelSink<E> {
    fn frames(&mut self, track: TrackId, frames: &[Sample], device_ts: DeviceTime) {
        if let Some(cap) = self.cap.clone() {
            let n = frames.len() as u64;
            let queued = cap.gauge.queued();
            if self.dropping {
                if queued + n > cap.limit / 2 {
                    return;
                }
                self.dropping = false;
                self.send(LiveMsg::Event(CaptureEvent::Discontinuity {
                    track,
                    discontinuity: Discontinuity {
                        at: device_ts,
                        kind: DiscontinuityKind::DataLost,
                    },
                }));
            } else if queued + n > cap.limit {
                self.dropping = true;
                return;
            }
            cap.gauge.add(n);
        }
        self.send(LiveMsg::Event(CaptureEvent::Frames {
            track,
            device_ts,
            frames: frames.to_vec(),
        }));
    }

    fn discontinuity(&mut self, track: TrackId, discontinuity: Discontinuity) {
        self.send(LiveMsg::Event(CaptureEvent::Discontinuity {
            track,
            discontinuity,
        }));
    }

    fn end(&mut self, end: EndOfStream) {
        self.send(LiveMsg::Event(CaptureEvent::End(end)));
    }
}

/// A [`TrackObserver`] that emits into a channel: the live source's per-track observer.
#[derive(Debug)]
pub struct EmitterTrack<E> {
    /// Packet -> event conversion.
    pub emitter: TrackEmitter,
    /// Where events go.
    pub sink: ChannelSink<E>,
}

impl<E> TrackObserver for EmitterTrack<E> {
    fn packet(&mut self, info: PacketInfo, samples: &[f32]) {
        self.emitter.packet(info, samples, &mut self.sink);
    }

    fn reopened(&mut self, qpc: u64, reason: ReopenReason, outcome: ReopenOutcome<'_>) {
        self.emitter.reopened(qpc, reason, outcome, &mut self.sink);
    }

    fn call_output_refused(&mut self, qpc: u64, wanted: &str, used: Option<&str>, _error: &str) {
        self.emitter
            .call_output_refused(qpc, wanted, used, &mut self.sink);
    }

    fn set_horizon(&mut self, qpc: u64) {
        self.emitter.set_horizon(qpc);
    }

    fn wants_more(&self) -> bool {
        !self.sink.is_closed()
    }
}

/// Error of a [`ChannelSource`] pump.
#[derive(Debug)]
pub enum LiveError<E> {
    /// The capture thread reported a failure; capture has ended and the next pump
    /// delivers [`EndOfStream`].
    Capture(E),
    /// The capture thread exited without ending the stream (it panicked); the next
    /// pump delivers a substitute [`EndOfStream`].
    CaptureThreadExited,
}

impl<E: fmt::Display> fmt::Display for LiveError<E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Capture(e) => write!(f, "capture failed: {e}"),
            Self::CaptureThreadExited => f.write_str("capture thread exited unexpectedly"),
        }
    }
}

impl<E: std::error::Error + 'static> std::error::Error for LiveError<E> {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Capture(e) => Some(e),
            Self::CaptureThreadExited => None,
        }
    }
}

type Stopper = Box<dyn FnOnce() + Send>;

/// Consumer side of a live capture: an [`AudioSource`] fed by a capture thread.
///
/// [`pump`](AudioSource::pump) delivers every event already received and never waits.
/// [`stop`](AudioSource::stop) asks the thread to stop and waits until it has flushed
/// its last packets and the end of stream into the channel (normally one wait period,
/// about 20 ms), so the next pump delivers them and returns [`SourceStatus::Ended`].
/// Dropping the source stops the thread the same way.
pub struct ChannelSource<E> {
    session: CaptureSession,
    rx: Receiver<LiveMsg<E>>,
    stopper: Option<Stopper>,
    thread_lost: bool,
    ended: bool,
    /// Current rate per track (index: mic, reference), for the substitute end time.
    rates: [SampleRate; 2],
    /// Samples per frame per track (fixed for the session).
    channels: [NonZeroU16; 2],
    last_end: DeviceTime,
    /// Released as frames are received (capped channels).
    gauge: Option<QueueGauge>,
}

impl<E> fmt::Debug for ChannelSource<E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ChannelSource")
            .field("session", &self.session)
            .field("ended", &self.ended)
            .finish_non_exhaustive()
    }
}

impl<E> ChannelSource<E> {
    /// Source for `session` receiving from `rx`. `stopper` is called once, from
    /// [`stop`](AudioSource::stop) or on drop; it must make the producer flush, send the
    /// end of stream and exit before it returns.
    pub fn new(
        session: CaptureSession,
        rx: Receiver<LiveMsg<E>>,
        stopper: impl FnOnce() + Send + 'static,
    ) -> Self {
        let mic = session.mic.rate;
        let reference = session.reference.as_ref().map_or(mic, |r| r.rate);
        let channels = [
            session.mic.channels,
            session.reference.as_ref().map_or(MONO, |r| r.channels),
        ];
        Self {
            channels,
            last_end: session.origin,
            session,
            rx,
            stopper: Some(Box::new(stopper)),
            thread_lost: false,
            ended: false,
            rates: [mic, reference],
            gauge: None,
        }
    }

    /// Releases received frames from `gauge` (the one the channel's capped sinks use).
    #[must_use]
    pub fn with_gauge(mut self, gauge: QueueGauge) -> Self {
        self.gauge = Some(gauge);
        self
    }

    /// Keeps the latest frame end and the per-track rate, for a substitute end time.
    fn track_progress(&mut self, event: &CaptureEvent) {
        let slot = |track| match track {
            TrackId::Mic => 0,
            TrackId::Reference => 1,
        };
        match event {
            CaptureEvent::Frames {
                track,
                device_ts,
                frames,
            } => {
                let i = slot(*track);
                let n = frames.len() / usize::from(self.channels[i].get());
                let end = device_ts.after_frames(n as u64, self.rates[i]);
                self.last_end = self.last_end.max(end);
            }
            CaptureEvent::Discontinuity {
                track,
                discontinuity:
                    Discontinuity {
                        kind: DiscontinuityKind::DeviceChanged { rate, .. },
                        ..
                    },
            } => self.rates[slot(*track)] = *rate,
            CaptureEvent::Discontinuity { .. } | CaptureEvent::End(_) => {}
        }
    }
}

impl<E: fmt::Debug> AudioSource for ChannelSource<E> {
    type Error = LiveError<E>;

    fn session(&self) -> &CaptureSession {
        &self.session
    }

    fn pump(&mut self, sink: &mut dyn CaptureSink) -> Result<SourceStatus, Self::Error> {
        if self.ended {
            return Ok(SourceStatus::Ended);
        }
        let mut delivered = false;
        loop {
            match self.rx.try_recv() {
                Ok(LiveMsg::Event(event)) => {
                    if let (Some(g), CaptureEvent::Frames { frames, .. }) = (&self.gauge, &event) {
                        g.release(frames.len() as u64);
                    }
                    let is_end = matches!(event, CaptureEvent::End(_));
                    self.track_progress(&event);
                    event.deliver(sink);
                    if is_end {
                        self.ended = true;
                        return Ok(SourceStatus::Ended);
                    }
                    delivered = true;
                }
                Ok(LiveMsg::Failed(e)) => return Err(LiveError::Capture(e)),
                Err(TryRecvError::Empty) => {
                    return Ok(if delivered {
                        SourceStatus::Delivered
                    } else {
                        SourceStatus::Idle
                    });
                }
                Err(TryRecvError::Disconnected) if delivered => {
                    return Ok(SourceStatus::Delivered);
                }
                Err(TryRecvError::Disconnected) if !self.thread_lost => {
                    self.thread_lost = true;
                    return Err(LiveError::CaptureThreadExited);
                }
                Err(TryRecvError::Disconnected) => {
                    // The thread exited without ending the stream (reported as
                    // `CaptureThreadExited`): capture ended on an error, stop or not.
                    self.ended = true;
                    sink.end(EndOfStream {
                        at: self.last_end,
                        reason: EndReason::Failed,
                    });
                    return Ok(SourceStatus::Ended);
                }
            }
        }
    }

    fn stop(&mut self) {
        if let Some(stop) = self.stopper.take() {
            stop();
        }
    }
}

impl<E> Drop for ChannelSource<E> {
    fn drop(&mut self) {
        if let Some(stop) = self.stopper.take() {
            stop();
        }
    }
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroU16;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::mpsc;

    use earshot_core::capture::TrackInfo;

    use super::*;
    use crate::format::{MixFormat, SampleFormat};

    const SR: u32 = 48_000;
    /// 10 ms packet at 48 kHz.
    const PF: u32 = 480;
    const PQ: u64 = 100_000;
    const T0: u64 = 1_000_000_000;

    fn rate() -> SampleRate {
        SampleRate::new(SR).unwrap()
    }

    fn emitter() -> TrackEmitter {
        TrackEmitter::new(TrackId::Mic, rate(), DeviceTime::from_ticks(T0))
    }

    fn pkt(qpc: u64, frames: u32) -> PacketInfo {
        PacketInfo {
            qpc,
            device_position: 0,
            frames,
            ..Default::default()
        }
    }

    fn ramp(n: u32) -> Vec<f32> {
        (0..n).map(|k| k as f32).collect()
    }

    fn t(ticks: u64) -> DeviceTime {
        DeviceTime::from_ticks(ticks)
    }

    /// (track, device_ts, frames) of every Frames event.
    fn frames(events: &[CaptureEvent]) -> Vec<(TrackId, DeviceTime, Vec<f32>)> {
        events
            .iter()
            .filter_map(|e| match e {
                CaptureEvent::Frames {
                    track,
                    device_ts,
                    frames,
                } => Some((*track, *device_ts, frames.clone())),
                _ => None,
            })
            .collect()
    }

    fn markers(events: &[CaptureEvent]) -> Vec<Discontinuity> {
        events
            .iter()
            .filter_map(|e| match e {
                CaptureEvent::Discontinuity { discontinuity, .. } => Some(discontinuity.clone()),
                _ => None,
            })
            .collect()
    }

    fn fmt48() -> MixFormat {
        MixFormat {
            sample_rate: SR,
            channels: 2,
            block_align: 8,
            bits_per_sample: 32,
            valid_bits_per_sample: 32,
            sample_format: SampleFormat::F32,
        }
    }

    fn opened(name: &str) -> ReopenOutcome<'_> {
        ReopenOutcome::Opened {
            device_name: name,
            device_format: fmt48(),
            rate: SR,
            endpoint: None,
            gave_up: None,
        }
    }

    #[test]
    fn live_contiguous_packets_pass_through_at_their_qpc() {
        let mut e = emitter();
        let mut out = Vec::new();
        for i in 0..3 {
            e.packet(pkt(T0 + i * PQ, PF), &ramp(PF), &mut out);
        }
        let f = frames(&out);
        assert_eq!(f.len(), 3);
        for (i, (track, ts, x)) in f.iter().enumerate() {
            assert_eq!(*track, TrackId::Mic);
            assert_eq!(*ts, t(T0 + i as u64 * PQ));
            assert_eq!(x, &ramp(PF));
        }
        assert!(markers(&out).is_empty());
        assert_eq!(e.end(), Some(t(T0 + 3 * PQ)));
    }

    #[test]
    fn live_first_packet_is_trimmed_to_the_origin() {
        let mut e = emitter();
        let mut out = Vec::new();
        // Entirely before the origin: dropped.
        e.packet(pkt(T0 - 200_000, PF), &ramp(PF), &mut out);
        assert!(out.is_empty());
        // Straddles the origin by 5 ms (240 frames).
        e.packet(pkt(T0 - 50_000, PF), &ramp(PF), &mut out);
        let f = frames(&out);
        assert_eq!(f.len(), 1);
        assert_eq!(f[0].1, t(T0), "frame 0 sits exactly at the origin");
        assert_eq!(f[0].2.len(), 240);
        assert_eq!(f[0].2[0], 240.0);
        // The next packet is not trimmed.
        e.packet(pkt(T0 + 50_000, PF), &ramp(PF), &mut out);
        let f = frames(&out);
        assert_eq!((f[1].1, f[1].2.len()), (t(T0 + 50_000), PF as usize));
    }

    #[test]
    fn live_late_first_packet_keeps_its_timestamp() {
        let mut e = emitter();
        let mut out = Vec::new();
        e.packet(pkt(T0 + 1_000_000, PF), &ramp(PF), &mut out);
        let f = frames(&out);
        assert_eq!(f[0].1, t(T0 + 1_000_000), "a gap is legal, never padded");
        assert_eq!(f[0].2.len(), PF as usize);
    }

    #[test]
    fn live_discontinuity_flag_is_data_lost_before_the_frames() {
        let mut e = emitter();
        let mut out = Vec::new();
        e.packet(pkt(T0, PF), &ramp(PF), &mut out);
        let mut p = pkt(T0 + 3 * PQ, PF);
        p.discontinuity = true;
        e.packet(p, &ramp(PF), &mut out);
        assert_eq!(out.len(), 3);
        assert_eq!(
            out[1],
            CaptureEvent::Discontinuity {
                track: TrackId::Mic,
                discontinuity: Discontinuity {
                    at: t(T0 + 3 * PQ),
                    kind: DiscontinuityKind::DataLost,
                },
            }
        );
        assert!(
            matches!(out[2], CaptureEvent::Frames { device_ts, .. } if device_ts == t(T0 + 3 * PQ))
        );
    }

    fn data_lost(out: &[CaptureEvent]) -> usize {
        out.iter()
            .filter(|e| {
                matches!(
                    e,
                    CaptureEvent::Discontinuity {
                        discontinuity: Discontinuity {
                            kind: DiscontinuityKind::DataLost,
                            ..
                        },
                        ..
                    }
                )
            })
            .count()
    }

    #[test]
    fn live_first_packet_of_a_stream_flagged_discontinuous_is_not_data_lost() {
        // WASAPI flags the first packet after Start; nothing was lost.
        let mut e = emitter();
        let mut out = Vec::new();
        let mut first = pkt(T0 - 50_000, PF);
        first.discontinuity = true;
        e.packet(first, &ramp(PF), &mut out);
        assert_eq!(data_lost(&out), 0, "{out:?}");
        assert_eq!(frames(&out).len(), 1);
        // The same after a re-open: the re-open is announced, the flag on the new
        // stream's first packet is not a DataLost.
        e.reopened(
            T0 + 2 * PQ,
            ReopenReason::DefaultDeviceChanged,
            opened("Headset"),
            &mut out,
        );
        let mut p = pkt(T0 + 3 * PQ, PF);
        p.discontinuity = true;
        e.packet(p, &ramp(PF), &mut out);
        assert_eq!(data_lost(&out), 0, "{out:?}");
        // A flag on any later packet is still a DataLost.
        let mut p = pkt(T0 + 5 * PQ, PF);
        p.discontinuity = true;
        e.packet(p, &ramp(PF), &mut out);
        assert_eq!(data_lost(&out), 1, "{out:?}");
    }

    #[test]
    fn live_flagged_first_packet_trimmed_away_leaves_the_next_one_first() {
        // A first packet entirely before the origin is dropped; the stream's first
        // delivered packet is the next one, and its flag is not a DataLost either.
        let mut e = emitter();
        let mut out = Vec::new();
        let mut early = pkt(T0 - 10 * PQ, PF);
        early.discontinuity = true;
        e.packet(early, &ramp(PF), &mut out);
        assert!(out.is_empty());
        let mut p = pkt(T0, PF);
        p.discontinuity = true;
        e.packet(p, &ramp(PF), &mut out);
        assert_eq!(data_lost(&out), 0, "{out:?}");
        assert_eq!(frames(&out).len(), 1);
    }
    #[test]
    fn live_silent_packets_deliver_zeros() {
        let mut e = emitter();
        let mut out = Vec::new();
        let mut p = pkt(T0 - 50_000, PF);
        p.silent = true;
        e.packet(p, &[], &mut out);
        let f = frames(&out);
        assert_eq!(f[0].2, vec![0.0; 240]);
        let mut p = pkt(T0 + 50_000, PF);
        p.silent = true;
        e.packet(p, &[], &mut out);
        assert_eq!(frames(&out)[1].2, vec![0.0; PF as usize]);
    }

    #[test]
    fn live_timestamp_errors_are_stamped_at_the_previous_end() {
        let mut e = emitter();
        let mut out = Vec::new();
        let mut bad = pkt(T0 + 50 * PQ, PF);
        bad.timestamp_error = true;
        // First packet without a usable timestamp: at the origin.
        e.packet(bad, &ramp(PF), &mut out);
        e.packet(bad, &ramp(PF), &mut out);
        let f = frames(&out);
        assert_eq!(f[0].1, t(T0));
        assert_eq!(f[1].1, t(T0 + PQ));
        assert_eq!(f[1].2.len(), PF as usize, "never trimmed");
    }

    #[test]
    fn live_timestamps_beyond_the_horizon_are_bogus() {
        let mut e = emitter();
        let mut out = Vec::new();
        e.set_horizon(T0 + 10 * PQ);
        e.packet(pkt(T0, PF), &ramp(PF), &mut out);
        e.packet(pkt(u64::MAX / 4, PF), &ramp(PF), &mut out);
        // Normal packets afterwards keep their own timestamps.
        e.packet(pkt(T0 + 2 * PQ, PF), &ramp(PF), &mut out);
        let ts: Vec<_> = frames(&out).iter().map(|f| f.1).collect();
        assert_eq!(ts, vec![t(T0), t(T0 + PQ), t(T0 + 2 * PQ)]);
    }

    #[test]
    fn live_backwards_timestamps_never_leave_the_sink() {
        let mut e = emitter();
        let mut out = Vec::new();
        e.packet(pkt(T0 + 5 * PQ, PF), &ramp(PF), &mut out);
        e.packet(pkt(T0 + PQ, PF), &ramp(PF), &mut out); // earlier than the previous start
        e.packet(pkt(T0 + 5 * PQ + 99_000, PF), &ramp(PF), &mut out); // before the restamped start
        e.packet(pkt(T0 + 7 * PQ, PF), &ramp(PF), &mut out); // exactly at the previous start
        let ts: Vec<_> = frames(&out).iter().map(|f| f.1).collect();
        assert_eq!(
            ts,
            vec![
                t(T0 + 5 * PQ),
                t(T0 + 6 * PQ),
                t(T0 + 7 * PQ),
                t(T0 + 8 * PQ)
            ],
            "a packet starting at or before the previous one's start goes to the previous \
             end"
        );
        assert!(ts.windows(2).all(|w| w[0] < w[1]));
        assert!(
            frames(&out).iter().all(|f| f.2 == ramp(PF)),
            "never trimmed"
        );
    }

    #[test]
    fn live_jitter_inside_the_previous_packet_keeps_every_frame_and_its_stamp() {
        // USB timestamp jitter (measured on a USB interface mic: 43% of packets stamped up to
        // 1.0 ms inside the previous packet) is not duplicated audio: WASAPI never
        // delivers a frame twice within one stream. Such a packet keeps all its frames
        // and its own stamp (replay of a raw recording does the same), and frame
        // timestamps still never go backwards.
        let mut e = emitter();
        let mut out = Vec::new();
        e.packet(pkt(T0, PF), &ramp(PF), &mut out);
        e.packet(pkt(T0 + PQ - 10_000, PF), &ramp(PF), &mut out); // 1 ms (48 frames) inside
        e.packet(pkt(T0 + 2 * PQ - 1, PF), &ramp(PF), &mut out); // 0.1 us inside
        let f = frames(&out);
        let ts: Vec<_> = f.iter().map(|f| f.1).collect();
        assert_eq!(ts, vec![t(T0), t(T0 + PQ - 10_000), t(T0 + 2 * PQ - 1)]);
        assert!(f.iter().all(|f| f.2 == ramp(PF)), "never trimmed");
        assert!(markers(&out).is_empty());
    }

    #[test]
    fn live_reopen_announces_the_device_and_trims_the_overlap() {
        let mut e = emitter();
        let mut out = Vec::new();
        e.packet(pkt(T0, PF), &ramp(PF), &mut out);
        e.reopened(
            T0 + 2 * PQ,
            ReopenReason::DefaultDeviceChanged,
            opened("Headset"),
            &mut out,
        );
        assert_eq!(
            out[1],
            CaptureEvent::Discontinuity {
                track: TrackId::Mic,
                discontinuity: Discontinuity {
                    at: t(T0 + 2 * PQ),
                    kind: DiscontinuityKind::DeviceChanged {
                        device_name: Some("Headset".into()),
                        rate: rate(),
                        warning: None,
                    },
                },
            }
        );
        // The new device's first packet starts 5 ms before the old stream's last frame
        // ended: 240 frames are trimmed.
        e.packet(pkt(T0 + PQ - 50_000, PF), &ramp(PF), &mut out);
        let f = frames(&out);
        assert_eq!(f[1].1, t(T0 + PQ));
        assert_eq!(f[1].2.len(), 240);
        assert_eq!(f[1].2[0], 240.0);
    }

    #[test]
    fn live_a_chosen_endpoint_given_up_is_named_in_the_marker() {
        let hfp = EndpointProperties {
            name: "Headset".into(),
            enumerator: Some("BTHHFENUM".into()),
            sample_rate: Some(16_000),
            ..Default::default()
        };
        let usb = EndpointProperties {
            name: "USB".into(),
            enumerator: Some("USB".into()),
            sample_rate: Some(48_000),
            ..Default::default()
        };
        let given_up = |p: &EndpointProperties| {
            let mut e = emitter().with_mic_warnings(Some(&usb));
            let mut out = Vec::new();
            e.reopened(
                T0,
                ReopenReason::Retry,
                ReopenOutcome::Opened {
                    device_name: "Default mic",
                    device_format: fmt48(),
                    rate: SR,
                    endpoint: Some(p),
                    gave_up: Some("Acme Evolve"),
                },
                &mut out,
            );
            match markers(&out).remove(0).kind {
                DiscontinuityKind::DeviceChanged {
                    device_name,
                    warning,
                    ..
                } => (device_name, warning),
                other => panic!("unexpected {other:?}"),
            }
        };
        // Reported although it is a retry (it opened), naming the chosen endpoint.
        assert_eq!(
            given_up(&usb),
            (
                Some("Default mic".into()),
                Some(EndpointWarning::ChosenEndpointGone {
                    chosen: "Acme Evolve".into(),
                    new_endpoint: None,
                })
            )
        );
        // The default's own hands-free warning is kept inside it.
        assert_eq!(
            given_up(&hfp).1,
            Some(EndpointWarning::ChosenEndpointGone {
                chosen: "Acme Evolve".into(),
                new_endpoint: Some(Box::new(EndpointWarning::BluetoothHandsFree {
                    sample_rate: Some(16_000),
                })),
            })
        );
    }

    #[test]
    fn live_call_output_refused_is_a_note_and_the_frames_go_on_untouched() {
        let mut e = emitter();
        let before = e.rate();
        let mut out = Vec::new();
        e.packet(pkt(T0, 480), &ramp(480), &mut out);
        let end = e.end();
        e.call_output_refused(T0 + PQ, "Headset", Some("Speakers"), &mut out);
        assert_eq!(e.end(), end, "no anchor: the stream did not change");
        assert_eq!(e.rate(), before);
        assert_eq!(
            markers(&out)[0].kind,
            DiscontinuityKind::Note {
                warning: EndpointWarning::CallOutputRefused {
                    wanted: "Headset".into(),
                    used: Some("Speakers".into()),
                },
            }
        );
        assert_eq!(markers(&out).len(), 1);
        e.packet(pkt(T0 + PQ, 480), &ramp(480), &mut out);
        assert_eq!(e.end(), Some(t(T0 + 2 * PQ)));
    }

    #[test]
    fn live_a_moved_output_is_reported_as_a_device_change() {
        let mut e = emitter();
        let mut out = Vec::new();
        e.packet(pkt(T0, 480), &ramp(480), &mut out);
        let opened = ReopenOutcome::Opened {
            device_name: "Headset",
            device_format: fmt48(),
            rate: 48_000,
            endpoint: None,
            gave_up: None,
        };
        e.reopened(T0 + PQ, ReopenReason::CallOutputMoved, opened, &mut out);
        assert!(matches!(
            &markers(&out)[0].kind,
            DiscontinuityKind::DeviceChanged { device_name: Some(n), warning: None, .. } if n == "Headset"
        ));
    }

    #[test]
    fn live_reopen_announces_a_new_rate() {
        let mut e = emitter();
        let mut out = Vec::new();
        e.reopened(
            T0,
            ReopenReason::StreamError,
            ReopenOutcome::Opened {
                device_name: "USB",
                device_format: fmt48(),
                rate: 44_100,
                endpoint: None,
                gave_up: None,
            },
            &mut out,
        );
        let m = markers(&out);
        let r44 = SampleRate::new(44_100).unwrap();
        assert_eq!(
            m[0].kind,
            DiscontinuityKind::DeviceChanged {
                device_name: Some("USB".into()),
                rate: r44,
                warning: None,
            }
        );
        assert_eq!(e.rate(), r44);
        e.packet(pkt(T0, 441), &ramp(441), &mut out);
        assert_eq!(e.end(), Some(t(T0 + PQ)));
    }

    #[test]
    fn live_failed_reopens_are_announced_except_retries() {
        let mut e = emitter();
        let mut out = Vec::new();
        e.packet(pkt(T0, PF), &ramp(PF), &mut out);
        let lost = ReopenOutcome::Failed { error: "gone" };
        e.reopened(T0 + PQ, ReopenReason::StreamError, lost, &mut out);
        e.reopened(T0 + 2 * PQ, ReopenReason::Retry, lost, &mut out);
        e.reopened(T0 + 3 * PQ, ReopenReason::Retry, opened("Back"), &mut out);
        let names: Vec<_> = markers(&out)
            .into_iter()
            .map(|m| match m.kind {
                DiscontinuityKind::DeviceChanged { device_name, .. } => (m.at, device_name),
                other => panic!("unexpected {other:?}"),
            })
            .collect();
        assert_eq!(
            names,
            vec![(t(T0 + PQ), None), (t(T0 + 3 * PQ), Some("Back".into()))]
        );
    }

    #[test]
    fn live_mic_reopen_onto_a_hands_free_mic_warns_once_per_switch() {
        let hfp = EndpointProperties {
            name: "Headset (Hands-Free)".into(),
            enumerator: Some("BTHHFENUM".into()),
            sample_rate: Some(16_000),
            ..Default::default()
        };
        let usb = EndpointProperties {
            name: "USB".into(),
            enumerator: Some("USB".into()),
            sample_rate: Some(48_000),
            ..Default::default()
        };
        fn on(p: &EndpointProperties) -> ReopenOutcome<'_> {
            ReopenOutcome::Opened {
                device_name: "dev",
                device_format: fmt48(),
                rate: SR,
                endpoint: Some(p),
                gave_up: None,
            }
        }
        let mut e = emitter().with_mic_warnings(Some(&usb));
        let mut out = Vec::new();
        let switch = ReopenReason::DefaultDeviceChanged;
        e.reopened(T0, switch, on(&hfp), &mut out);
        e.reopened(T0 + PQ, ReopenReason::StreamError, on(&hfp), &mut out);
        // Lost and back on the same hands-free mic: not a new switch.
        let lost = ReopenOutcome::Failed { error: "gone" };
        e.reopened(T0 + 2 * PQ, ReopenReason::StreamError, lost, &mut out);
        e.reopened(T0 + 3 * PQ, ReopenReason::Retry, on(&hfp), &mut out);
        e.reopened(T0 + 4 * PQ, switch, on(&usb), &mut out);
        e.reopened(T0 + 5 * PQ, switch, on(&hfp), &mut out);
        let warned: Vec<_> = markers(&out)
            .into_iter()
            .map(|m| match m.kind {
                DiscontinuityKind::DeviceChanged { warning, .. } => (m.at, warning),
                other => panic!("unexpected {other:?}"),
            })
            .collect();
        let hf = Some(EndpointWarning::BluetoothHandsFree {
            sample_rate: Some(16_000),
        });
        assert_eq!(
            warned,
            vec![
                (t(T0), hf.clone()),
                (t(T0 + PQ), None),
                (t(T0 + 2 * PQ), None),
                (t(T0 + 3 * PQ), None),
                (t(T0 + 4 * PQ), None),
                (t(T0 + 5 * PQ), hf),
            ]
        );
        // Without mic warnings (the loopback) a hands-free endpoint is never warned.
        let mut lb = emitter();
        let mut out = Vec::new();
        lb.reopened(T0, switch, on(&hfp), &mut out);
        assert!(matches!(
            &markers(&out)[0].kind,
            DiscontinuityKind::DeviceChanged { warning: None, .. }
        ));
    }

    #[test]
    fn live_reopen_before_any_packet_keeps_the_origin_anchor() {
        let mut e = emitter();
        let mut out = Vec::new();
        e.reopened(
            T0,
            ReopenReason::DefaultDeviceChanged,
            opened("B"),
            &mut out,
        );
        e.packet(pkt(T0 - 50_000, PF), &ramp(PF), &mut out);
        let f = frames(&out);
        assert_eq!((f[0].1, f[0].2.len()), (t(T0), 240));
    }

    #[test]
    fn live_empty_packets_emit_nothing() {
        let mut e = emitter();
        let mut out = Vec::new();
        let mut p = pkt(T0, 0);
        p.discontinuity = true;
        e.packet(p, &[], &mut out);
        assert!(out.is_empty());
        assert_eq!(e.end(), None);
    }

    const STEREO: NonZeroU16 = NonZeroU16::new(2).unwrap();

    /// `frames` stereo frames: left `k`, right `-k`.
    fn stereo(frames: u32) -> Vec<f32> {
        (0..frames).flat_map(|k| [k as f32, -(k as f32)]).collect()
    }

    #[test]
    fn live_stereo_packets_are_trimmed_and_timed_in_whole_frames() {
        let mut e = TrackEmitter::new(TrackId::Reference, rate(), t(T0)).with_channels(STEREO);
        let mut out = Vec::new();
        // Straddles the origin by 5 ms (240 frames).
        e.packet(pkt(T0 - 50_000, PF), &stereo(PF), &mut out);
        e.packet(pkt(T0 + 50_000, PF), &stereo(PF), &mut out);
        let mut p = pkt(T0 + 150_000, PF);
        p.silent = true;
        e.packet(p, &[], &mut out);
        let f = frames(&out);
        assert_eq!(f[0].0, TrackId::Reference);
        assert_eq!(f[0].1, t(T0));
        assert_eq!(f[0].2.len(), 2 * 240, "240 frames of two samples");
        assert_eq!(f[0].2[..2], [240.0, -240.0], "frame 240, both channels");
        assert_eq!(f[1].2, stereo(PF));
        assert_eq!(
            f[2].2,
            vec![0.0; 2 * PF as usize],
            "silence is interleaved too"
        );
        assert_eq!(e.end(), Some(t(T0 + 250_000)), "end advances by frames");
    }

    #[test]
    #[should_panic(expected = "samples")]
    fn live_stereo_packet_with_mono_samples_panics() {
        let mut e = TrackEmitter::new(TrackId::Reference, rate(), t(T0)).with_channels(STEREO);
        e.packet(pkt(T0, PF), &ramp(PF), &mut Vec::new());
    }

    #[test]
    fn live_substitute_end_counts_stereo_frames() {
        let (tx, rx) = mpsc::channel();
        let session = CaptureSession::new(
            t(T0),
            TrackInfo::new(rate()),
            Some(TrackInfo::new(rate()).with_channels(STEREO)),
        );
        let mut src = ChannelSource::new(session, rx, || {});
        // 480 stereo frames (960 samples) at 48 kHz last 10 ms, not 20.
        tx.send(frames_event(TrackId::Reference, T0, 960)).unwrap();
        drop(tx);
        let mut out = Vec::new();
        assert_eq!(src.pump(&mut out).unwrap(), SourceStatus::Delivered);
        assert!(src.pump(&mut out).is_err());
        assert_eq!(src.pump(&mut out).unwrap(), SourceStatus::Ended);
        assert_eq!(
            out.last(),
            Some(&CaptureEvent::End(EndOfStream {
                at: t(T0 + PQ),
                reason: EndReason::Failed,
            }))
        );
    }

    #[test]
    fn live_channel_sink_closes_when_the_consumer_is_gone() {
        let (tx, rx) = mpsc::channel::<LiveMsg<String>>();
        let mut sink = ChannelSink::new(tx);
        sink.frames(TrackId::Mic, &[1.0], t(T0));
        assert!(!sink.is_closed());
        assert!(matches!(
            rx.try_recv(),
            Ok(LiveMsg::Event(CaptureEvent::Frames { .. }))
        ));
        drop(rx);
        sink.frames(TrackId::Mic, &[1.0], t(T0));
        assert!(sink.is_closed());
        let mut track = EmitterTrack {
            emitter: emitter(),
            sink,
        };
        assert!(!TrackObserver::wants_more(&track));
        // Still safe to feed.
        TrackObserver::packet(&mut track, pkt(T0, PF), &ramp(PF));
    }

    fn session() -> CaptureSession {
        CaptureSession::new(
            t(T0),
            TrackInfo::new(rate()).with_device_name("Mic"),
            Some(TrackInfo::new(SampleRate::new(44_100).unwrap())),
        )
    }

    fn frames_event(track: TrackId, ts: u64, n: usize) -> LiveMsg<String> {
        LiveMsg::Event(CaptureEvent::Frames {
            track,
            device_ts: t(ts),
            frames: vec![0.5; n],
        })
    }

    fn end_event(at: u64, reason: EndReason) -> LiveMsg<String> {
        LiveMsg::Event(CaptureEvent::End(EndOfStream { at: t(at), reason }))
    }

    /// A source with a counting stopper.
    fn source() -> (
        ChannelSource<String>,
        mpsc::Sender<LiveMsg<String>>,
        Arc<AtomicUsize>,
    ) {
        let (tx, rx) = mpsc::channel();
        let stops = Arc::new(AtomicUsize::new(0));
        let s = stops.clone();
        let src = ChannelSource::new(session(), rx, move || {
            s.fetch_add(1, Ordering::SeqCst);
        });
        (src, tx, stops)
    }

    fn ends(events: &[CaptureEvent]) -> usize {
        events
            .iter()
            .filter(|e| matches!(e, CaptureEvent::End(_)))
            .count()
    }

    #[test]
    fn live_pump_is_idle_without_events_and_delivers_in_order() {
        let (mut src, tx, _) = source();
        assert_eq!(src.session(), &session());
        let mut out = Vec::new();
        assert_eq!(src.pump(&mut out).unwrap(), SourceStatus::Idle);
        tx.send(frames_event(TrackId::Mic, T0, 480)).unwrap();
        tx.send(frames_event(TrackId::Reference, T0, 441)).unwrap();
        tx.send(frames_event(TrackId::Mic, T0 + PQ, 480)).unwrap();
        assert_eq!(src.pump(&mut out).unwrap(), SourceStatus::Delivered);
        assert_eq!(out.len(), 3);
        assert_eq!(src.pump(&mut out).unwrap(), SourceStatus::Idle);
        assert_eq!(out.len(), 3);
    }

    #[test]
    fn live_end_is_delivered_once_and_last() {
        let (mut src, tx, _) = source();
        let mut out = Vec::new();
        tx.send(frames_event(TrackId::Mic, T0, 480)).unwrap();
        tx.send(end_event(T0 + PQ, EndReason::Stopped)).unwrap();
        // Anything after the end is never delivered.
        tx.send(frames_event(TrackId::Mic, T0 + PQ, 480)).unwrap();
        tx.send(end_event(T0 + 2 * PQ, EndReason::Stopped)).unwrap();
        assert_eq!(src.pump(&mut out).unwrap(), SourceStatus::Ended);
        assert_eq!(src.pump(&mut out).unwrap(), SourceStatus::Ended);
        drop(tx);
        assert_eq!(src.pump(&mut out).unwrap(), SourceStatus::Ended);
        assert_eq!(out.len(), 2);
        assert_eq!(ends(&out), 1);
        assert!(matches!(out.last(), Some(CaptureEvent::End(_))));
    }

    #[test]
    fn live_capture_failure_is_an_error_then_the_end() {
        let (mut src, tx, _) = source();
        let mut out = Vec::new();
        tx.send(frames_event(TrackId::Mic, T0, 480)).unwrap();
        tx.send(LiveMsg::Failed("device exploded".into())).unwrap();
        tx.send(end_event(T0 + PQ, EndReason::Failed)).unwrap();
        drop(tx);
        match src.pump(&mut out) {
            Err(LiveError::Capture(e)) => assert_eq!(e, "device exploded"),
            r => panic!("{r:?}"),
        }
        assert_eq!(out.len(), 1, "frames before the failure were delivered");
        assert_eq!(src.pump(&mut out).unwrap(), SourceStatus::Ended);
        assert_eq!(ends(&out), 1);
        assert!(
            matches!(
                out.last(),
                Some(CaptureEvent::End(EndOfStream {
                    reason: EndReason::Failed,
                    ..
                }))
            ),
            "the producer's own end is passed through: {out:?}"
        );
    }

    #[test]
    fn live_vanished_thread_is_an_error_then_one_substitute_end() {
        let (mut src, tx, _) = source();
        let mut out = Vec::new();
        tx.send(frames_event(TrackId::Mic, T0, 480)).unwrap();
        tx.send(frames_event(TrackId::Reference, T0 + PQ, 441))
            .unwrap();
        drop(tx);
        assert_eq!(src.pump(&mut out).unwrap(), SourceStatus::Delivered);
        assert!(matches!(
            src.pump(&mut out),
            Err(LiveError::CaptureThreadExited)
        ));
        assert_eq!(src.pump(&mut out).unwrap(), SourceStatus::Ended);
        assert_eq!(src.pump(&mut out).unwrap(), SourceStatus::Ended);
        assert_eq!(ends(&out), 1);
        assert_eq!(
            out.last(),
            Some(&CaptureEvent::End(EndOfStream {
                // The reference's 441 frames at 44.1 kHz end 10 ms after they start.
                at: t(T0 + 2 * PQ),
                reason: EndReason::Failed,
            }))
        );
    }

    #[test]
    fn live_substitute_end_follows_announced_rates_and_fails_even_after_a_stop() {
        let (mut src, tx, _) = source();
        let mut out = Vec::new();
        tx.send(LiveMsg::Event(CaptureEvent::Discontinuity {
            track: TrackId::Mic,
            discontinuity: Discontinuity {
                at: t(T0),
                kind: DiscontinuityKind::DeviceChanged {
                    device_name: None,
                    rate: SampleRate::new(16_000).unwrap(),
                    warning: None,
                },
            },
        }))
        .unwrap();
        tx.send(frames_event(TrackId::Mic, T0, 160)).unwrap();
        drop(tx);
        src.stop();
        assert_eq!(src.pump(&mut out).unwrap(), SourceStatus::Delivered);
        assert!(src.pump(&mut out).is_err());
        assert_eq!(src.pump(&mut out).unwrap(), SourceStatus::Ended);
        assert_eq!(
            out.last(),
            Some(&CaptureEvent::End(EndOfStream {
                at: t(T0 + PQ),
                // The thread vanished without ending the stream: an error, not a stop.
                reason: EndReason::Failed,
            }))
        );
    }

    #[test]
    fn live_stopper_runs_exactly_once() {
        let (mut src, _tx, stops) = source();
        src.stop();
        src.stop();
        drop(src);
        assert_eq!(stops.load(Ordering::SeqCst), 1);
        let (src, _tx, stops) = source();
        drop(src);
        assert_eq!(
            stops.load(Ordering::SeqCst),
            1,
            "drop stops a running source"
        );
    }

    #[test]
    fn live_threaded_producer_ends_exactly_once_with_ordered_tracks() {
        // A producer thread interleaving both tracks, stopped by the consumer: every
        // event arrives, per-track timestamps are monotonic and the end comes once, last.
        let (tx, rx) = mpsc::channel();
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = stop.clone();
        let producer = std::thread::spawn(move || {
            let mut mic = TrackEmitter::new(TrackId::Mic, rate(), t(T0));
            let mut lb =
                TrackEmitter::new(TrackId::Reference, SampleRate::new(44_100).unwrap(), t(T0));
            let mut sink = ChannelSink::<String>::new(tx);
            let mut i = 0u64;
            while !flag.load(Ordering::Acquire) && i < 100_000 {
                mic.packet(pkt(T0 + i * PQ, PF), &ramp(PF), &mut sink);
                if i.is_multiple_of(3) {
                    lb.packet(pkt(T0 + i * PQ, 441), &ramp(441), &mut sink);
                }
                i += 1;
            }
            let at = mic.end().max(lb.end()).unwrap_or(t(T0));
            sink.end(EndOfStream {
                at,
                reason: EndReason::Stopped,
            });
        });
        let stopper = std::sync::Mutex::new(Some(producer));
        let mut src = ChannelSource::new(session(), rx, move || {
            stop.store(true, Ordering::Release);
            if let Some(h) = stopper.lock().unwrap().take() {
                h.join().unwrap();
            }
        });
        let mut out = Vec::new();
        for _ in 0..50 {
            src.pump(&mut out).unwrap();
        }
        src.stop();
        assert_eq!(src.pump(&mut out).unwrap(), SourceStatus::Ended);
        assert_eq!(ends(&out), 1);
        assert!(matches!(out.last(), Some(CaptureEvent::End(_))));
        for track in [TrackId::Mic, TrackId::Reference] {
            let ts: Vec<_> = frames(&out)
                .into_iter()
                .filter(|f| f.0 == track)
                .map(|f| f.1)
                .collect();
            assert!(ts.windows(2).all(|w| w[0] < w[1]), "{track:?}");
        }
    }

    #[test]
    fn live_queue_cap_drops_while_the_consumer_stalls_and_marks_the_gap() {
        // Cap: 2 400 samples (5 packets of 480). The consumer does not pump while 8
        // packets arrive: packets 6-8 are dropped. After it drains the queue, the next
        // packet is preceded by one DataLost marker at its own time.
        let (tx, rx) = mpsc::channel();
        let gauge = QueueGauge::new();
        let mut sink: ChannelSink<()> = ChannelSink::with_cap(tx, gauge.clone(), 2_400);
        let session = CaptureSession::new(t(T0), TrackInfo::new(rate()), None);
        let mut source = ChannelSource::new(session, rx, || {}).with_gauge(gauge.clone());
        for k in 0..8u64 {
            sink.frames(TrackId::Mic, &ramp(PF), t(T0 + k * PQ));
        }
        assert_eq!(gauge.queued(), 2_400, "only what fits was queued");
        let mut got: Vec<CaptureEvent> = Vec::new();
        assert_eq!(source.pump(&mut got).unwrap(), SourceStatus::Delivered);
        assert_eq!(got.len(), 5);
        assert_eq!(gauge.queued(), 0, "the consumer releases what it receives");
        got.clear();
        sink.frames(TrackId::Mic, &ramp(PF), t(T0 + 8 * PQ));
        sink.frames(TrackId::Mic, &ramp(PF), t(T0 + 9 * PQ));
        source.pump(&mut got).unwrap();
        assert!(matches!(
            &got[0],
            CaptureEvent::Discontinuity {
                track: TrackId::Mic,
                discontinuity: Discontinuity { at, kind: DiscontinuityKind::DataLost },
            } if *at == t(T0 + 8 * PQ)
        ));
        assert!(
            matches!(&got[1], CaptureEvent::Frames { device_ts, .. } if *device_ts == t(T0 + 8 * PQ))
        );
        assert!(matches!(&got[2], CaptureEvent::Frames { .. }));
        assert_eq!(got.len(), 3, "one marker per gap");
    }

    #[test]
    fn live_queue_cap_resumes_only_after_the_queue_halves() {
        let (tx, rx) = mpsc::channel();
        let gauge = QueueGauge::new();
        let mut sink: ChannelSink<()> = ChannelSink::with_cap(tx, gauge.clone(), 1_000);
        let session = CaptureSession::new(t(T0), TrackInfo::new(rate()), None);
        let mut source = ChannelSource::new(session, rx, || {}).with_gauge(gauge.clone());
        sink.frames(TrackId::Mic, &[0.0; 600], t(T0));
        sink.frames(TrackId::Mic, &[0.0; 600], t(T0 + PQ)); // over the cap: dropping
        assert_eq!(gauge.queued(), 600);
        // Still above half the cap with the next packet: keeps dropping.
        sink.frames(TrackId::Mic, &[0.0; 100], t(T0 + 2 * PQ));
        assert_eq!(gauge.queued(), 600);
        let mut got: Vec<CaptureEvent> = Vec::new();
        source.pump(&mut got).unwrap();
        got.clear();
        sink.frames(TrackId::Mic, &[0.0; 100], t(T0 + 3 * PQ));
        source.pump(&mut got).unwrap();
        assert_eq!(got.len(), 2, "marker then frames: {got:?}");
        // Without a cap nothing is ever dropped.
        let (tx, rx) = mpsc::channel();
        let mut open: ChannelSink<()> = ChannelSink::new(tx);
        for k in 0..100 {
            open.frames(TrackId::Mic, &[0.0; 480], t(T0 + k * PQ));
        }
        assert_eq!(rx.try_iter().count(), 100);
    }

    #[test]
    fn live_queue_cap_marks_each_track_once_when_both_dropped() {
        let (tx, rx) = mpsc::channel();
        let gauge = QueueGauge::new();
        let mut mic: ChannelSink<()> = ChannelSink::with_cap(tx.clone(), gauge.clone(), 1_000);
        let mut lb: ChannelSink<()> = ChannelSink::with_cap(tx, gauge.clone(), 1_000);
        let session =
            CaptureSession::new(t(T0), TrackInfo::new(rate()), Some(TrackInfo::new(rate())));
        let mut source = ChannelSource::new(session, rx, || {}).with_gauge(gauge.clone());
        mic.frames(TrackId::Mic, &[0.0; 480], t(T0));
        lb.frames(TrackId::Reference, &[0.0; 480], t(T0));
        // Both over the cap now: both drop.
        mic.frames(TrackId::Mic, &[0.0; 480], t(T0 + PQ));
        lb.frames(TrackId::Reference, &[0.0; 480], t(T0 + PQ));
        let mut got: Vec<CaptureEvent> = Vec::new();
        source.pump(&mut got).unwrap();
        assert_eq!(got.len(), 2);
        got.clear();
        mic.frames(TrackId::Mic, &[0.0; 100], t(T0 + 2 * PQ));
        lb.frames(TrackId::Reference, &[0.0; 100], t(T0 + 2 * PQ));
        mic.frames(TrackId::Mic, &[0.0; 100], t(T0 + 3 * PQ));
        source.pump(&mut got).unwrap();
        let markers: Vec<TrackId> = got
            .iter()
            .filter_map(|e| match e {
                CaptureEvent::Discontinuity {
                    track,
                    discontinuity:
                        Discontinuity {
                            kind: DiscontinuityKind::DataLost,
                            ..
                        },
                } => Some(*track),
                _ => None,
            })
            .collect();
        assert_eq!(markers, [TrackId::Mic, TrackId::Reference], "{got:?}");
        assert_eq!(got.len(), 5, "two markers, three packets");
    }

    #[test]
    fn live_queue_limit_is_the_cap_of_both_tracks_at_their_rates() {
        let mic = TrackInfo::new(rate());
        let stereo = TrackInfo::new(SampleRate::new(44_100).unwrap())
            .with_channels(NonZeroU16::new(2).unwrap());
        assert_eq!(queue_limit(&mic, Some(&stereo)), 30 * (48_000 + 2 * 44_100));
        assert_eq!(LIVE_QUEUE_CAP.as_secs(), 30);
    }

    #[test]
    fn live_queue_limit_of_a_mic_only_session_is_the_cap_of_the_mic() {
        let mic = TrackInfo::new(SampleRate::new(16_000).unwrap());
        assert_eq!(queue_limit(&mic, None), 30 * 16_000);
    }
}
