//! A replay paced in real time (host glue): `earshot replay --realtime` and the Settings
//! benchmark (T7.4c) deliver a recorded meeting no faster than a capture device would.
//!
//! [`Paced`] wraps any [`AudioSource`]: with `realtime` it never delivers a packet before its
//! time has come on a [`PaceClock`] (the first pump starts the clock at the session origin);
//! without it, it delivers as fast as the session takes the audio. Either way it keeps
//! [`PaceState`]: how much was delivered, the largest lag of delivery behind the clock (how
//! long the session left captured audio waiting, as a device queue would hold it, T6.10b)
//! and when the source delivered its end.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use earshot_core::audio::Sample;
use earshot_core::capture::{
    AudioSource, CaptureSession, CaptureSink, DeviceTime, Discontinuity, EndOfStream, SourceStatus,
    TrackId,
};

/// A monotonic clock for pacing and timings (tests move their own).
pub trait PaceClock: Send + Sync {
    /// Time since an arbitrary fixed origin.
    fn now(&self) -> Duration;
}

/// The wall clock ([`Instant`]).
#[derive(Debug)]
pub struct WallClock(Instant);

impl WallClock {
    /// A clock starting now.
    #[must_use]
    pub fn new() -> Self {
        Self(Instant::now())
    }
}

impl Default for WallClock {
    fn default() -> Self {
        Self::new()
    }
}

impl PaceClock for WallClock {
    fn now(&self) -> Duration {
        self.0.elapsed()
    }
}

fn ms(d: Duration) -> u64 {
    u64::try_from(d.as_millis()).unwrap_or(u64::MAX)
}

/// What a [`Paced`] source did, shared with whoever watches it.
#[derive(Debug, Default)]
pub struct PaceState {
    heard_ms: AtomicU64,
    max_lag_ms: AtomicU64,
    ended: Mutex<Option<Duration>>,
}

impl PaceState {
    /// Audio delivered so far (the latest packet's start, from the session origin).
    #[must_use]
    pub fn heard(&self) -> Duration {
        Duration::from_millis(self.heard_ms.load(Ordering::SeqCst))
    }

    /// The largest lag of delivery behind the clock seen.
    #[must_use]
    pub fn max_lag(&self) -> Duration {
        Duration::from_millis(self.max_lag_ms.load(Ordering::SeqCst))
    }

    /// When the source delivered its end ([`PaceClock::now`]), once it has.
    #[must_use]
    pub fn ended(&self) -> Option<Duration> {
        *self.ended.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// A source paced on a clock (module docs).
pub struct Paced<S> {
    inner: S,
    clock: Arc<dyn PaceClock>,
    realtime: bool,
    start: Option<Duration>,
    /// Device time of the latest packet delivered.
    latest: Option<DeviceTime>,
    state: Arc<PaceState>,
    on_start: Option<Box<dyn FnOnce() + Send>>,
}

impl<S> Paced<S> {
    /// `inner` paced on `clock` when `realtime`, reporting to `state`.
    pub fn new(inner: S, clock: Arc<dyn PaceClock>, realtime: bool, state: Arc<PaceState>) -> Self {
        Self {
            inner,
            clock,
            realtime,
            start: None,
            latest: None,
            state,
            on_start: None,
        }
    }

    /// Calls `f` once, when the first pump starts the clock.
    #[must_use]
    pub fn on_start(mut self, f: impl FnOnce() + Send + 'static) -> Self {
        self.on_start = Some(Box::new(f));
        self
    }
}

/// Forwards to the session's sink and remembers the latest packet time.
struct Noting<'a> {
    sink: &'a mut dyn CaptureSink,
    latest: &'a mut Option<DeviceTime>,
}

impl CaptureSink for Noting<'_> {
    fn frames(&mut self, track: TrackId, frames: &[Sample], device_ts: DeviceTime) {
        *self.latest = Some(self.latest.map_or(device_ts, |t| t.max(device_ts)));
        self.sink.frames(track, frames, device_ts);
    }

    fn discontinuity(&mut self, track: TrackId, discontinuity: Discontinuity) {
        self.sink.discontinuity(track, discontinuity);
    }

    fn end(&mut self, end: EndOfStream) {
        self.sink.end(end);
    }
}

impl<S: AudioSource> AudioSource for Paced<S> {
    type Error = S::Error;

    fn session(&self) -> &CaptureSession {
        self.inner.session()
    }

    fn pump(&mut self, sink: &mut dyn CaptureSink) -> Result<SourceStatus, Self::Error> {
        let now = self.clock.now();
        let start = match self.start {
            Some(s) => s,
            None => {
                if let Some(f) = self.on_start.take() {
                    f();
                }
                *self.start.insert(now)
            }
        };
        let elapsed = now.saturating_sub(start);
        let origin = self.inner.session().origin;
        let played = self.latest.and_then(|t| t.checked_duration_since(origin));
        if self.realtime && played.is_some_and(|p| p > elapsed) {
            return Ok(SourceStatus::Idle);
        }
        if let Some(p) = played {
            self.state
                .max_lag_ms
                .fetch_max(ms(elapsed.saturating_sub(p)), Ordering::SeqCst);
        }
        let status = self.inner.pump(&mut Noting {
            sink,
            latest: &mut self.latest,
        })?;
        if let Some(p) = self.latest.and_then(|t| t.checked_duration_since(origin)) {
            self.state.heard_ms.fetch_max(ms(p), Ordering::SeqCst);
        }
        if status == SourceStatus::Ended {
            let mut ended = self
                .state
                .ended
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            ended.get_or_insert_with(|| self.clock.now());
        }
        Ok(status)
    }

    fn stop(&mut self) {
        self.inner.stop();
    }
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroU16;

    use earshot_core::audio::SampleRate;
    use earshot_core::capture::{ReplayBuilder, TrackBuffer};

    use super::*;

    /// A clock the test moves.
    #[derive(Debug, Default)]
    struct TestClock(AtomicU64);

    impl PaceClock for TestClock {
        fn now(&self) -> Duration {
            Duration::from_millis(self.0.load(Ordering::SeqCst))
        }
    }

    /// Counts the frames delivered.
    #[derive(Default)]
    struct Count(usize, bool);

    impl CaptureSink for Count {
        fn frames(&mut self, _: TrackId, frames: &[Sample], _: DeviceTime) {
            self.0 += frames.len();
        }
        fn discontinuity(&mut self, _: TrackId, _: Discontinuity) {}
        fn end(&mut self, _: EndOfStream) {
            self.1 = true;
        }
    }

    fn ten_seconds() -> earshot_core::capture::ReplaySource {
        let rate = SampleRate::new(16_000).unwrap();
        let mic = TrackBuffer::new(rate, NonZeroU16::MIN, vec![0.0; 160_000]).unwrap();
        ReplayBuilder::new(DeviceTime::from_ticks(10_000_000), mic)
            .build()
            .unwrap()
    }

    #[test]
    fn replay_paced_never_runs_ahead_of_its_clock() {
        let clock = Arc::new(TestClock::default());
        let state = Arc::new(PaceState::default());
        let started = Arc::new(AtomicU64::new(0));
        let s = Arc::clone(&started);
        let mut paced = Paced::new(
            ten_seconds(),
            Arc::clone(&clock) as Arc<dyn PaceClock>,
            true,
            Arc::clone(&state),
        )
        .on_start(move || {
            s.fetch_add(1, Ordering::SeqCst);
        });
        let mut sink = Count::default();
        let pump_all = |paced: &mut Paced<_>, sink: &mut Count| {
            for _ in 0..2_000 {
                paced.pump(sink).unwrap();
            }
        };
        // At time 0 the packet starting at 0 goes out, and the next: a packet is held back
        // only once the last one delivered starts after the clock (at most one 10 ms packet
        // ahead).
        pump_all(&mut paced, &mut sink);
        assert_eq!(sink.0, 320, "two 10 ms packets");
        assert_eq!(started.load(Ordering::SeqCst), 1);
        // A second later, the packets of that second and again at most one more.
        clock.0.store(1_000, Ordering::SeqCst);
        pump_all(&mut paced, &mut sink);
        assert_eq!(sink.0, 16_320);
        assert_eq!(state.heard(), Duration::from_millis(1_010));
        assert_eq!(state.ended(), None);
        // The session takes the rest 3 s late: the lag is recorded, then the end.
        clock.0.store(13_000, Ordering::SeqCst);
        pump_all(&mut paced, &mut sink);
        assert_eq!(sink.0, 160_000);
        assert!(sink.1);
        assert_eq!(state.ended(), Some(Duration::from_secs(13)));
        assert!(
            state.max_lag() >= Duration::from_millis(11_990),
            "{:?}",
            state.max_lag()
        );
        assert_eq!(started.load(Ordering::SeqCst), 1, "on_start runs once");
    }

    #[test]
    fn replay_unpaced_delivers_as_fast_as_it_is_pumped() {
        let clock = Arc::new(TestClock::default());
        let state = Arc::new(PaceState::default());
        let mut paced = Paced::new(ten_seconds(), clock, false, Arc::clone(&state));
        let mut sink = Count::default();
        for _ in 0..2_000 {
            paced.pump(&mut sink).unwrap();
        }
        assert_eq!(sink.0, 160_000);
        assert_eq!(state.ended(), Some(Duration::ZERO));
    }
}
