//! Device timestamps.

use std::ops::Add;
use std::time::Duration;

use crate::audio::SampleRate;

/// A monotonic device timestamp in **100 ns ticks** ([`DeviceTime::TICKS_PER_SECOND`]).
///
/// This is the unit of Windows QPC positions as returned by WASAPI
/// (`IAudioCaptureClient::GetBuffer`), so the WASAPI adapter passes them through
/// unchanged. Other adapters convert: Android `AudioTimestamp.nanoTime` (and any other
/// nanosecond clock) goes through [`DeviceTime::from_nanos`]. The epoch is whatever the
/// platform clock uses; only differences matter. All tracks of one
/// [`CaptureSession`](super::CaptureSession) share the clock, and
/// [`CaptureSession::origin`](super::CaptureSession::origin) maps it to meeting time.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct DeviceTime(u64);

/// `frames` at `rate` in ticks, rounded to the nearest tick (u128 intermediate), or
/// `None` if that exceeds `u64`.
fn frames_to_ticks(frames: u64, rate: SampleRate) -> Option<u64> {
    let num = u128::from(frames) * u128::from(DeviceTime::TICKS_PER_SECOND);
    let hz = u128::from(rate.hz());
    u64::try_from((num + hz / 2) / hz).ok()
}

/// `d` in ticks, rounded to the nearest tick, or `None` if that exceeds `u64`.
fn duration_to_ticks(d: Duration) -> Option<u64> {
    u64::try_from((d.as_nanos() + 50) / 100).ok()
}

impl DeviceTime {
    /// Ticks per second: one tick is 100 ns.
    pub const TICKS_PER_SECOND: u64 = 10_000_000;

    /// A timestamp of `ticks` 100 ns units.
    #[must_use]
    pub const fn from_ticks(ticks: u64) -> Self {
        Self(ticks)
    }

    /// A timestamp from a nanosecond clock, rounded to the nearest tick.
    #[must_use]
    pub const fn from_nanos(nanos: u64) -> Self {
        Self(nanos / 100 + (nanos % 100 >= 50) as u64)
    }

    /// A timestamp from a clock counting `units_per_second`, rounded to the nearest
    /// tick. Returns `None` if `units_per_second` is 0 or the result overflows.
    #[must_use]
    pub fn from_units(value: u64, units_per_second: u64) -> Option<Self> {
        if units_per_second == 0 {
            return None;
        }
        let num = u128::from(value) * u128::from(Self::TICKS_PER_SECOND);
        let den = u128::from(units_per_second);
        u64::try_from((num + den / 2) / den).ok().map(Self)
    }

    /// The raw tick count.
    #[must_use]
    pub const fn ticks(self) -> u64 {
        self.0
    }

    /// The timestamp `frames` frames at `rate` later, rounded to the nearest tick.
    /// Saturates at `u64::MAX` ticks; use [`checked_after_frames`](Self::checked_after_frames)
    /// for untrusted input.
    #[must_use]
    pub fn after_frames(self, frames: u64, rate: SampleRate) -> Self {
        self.checked_after_frames(frames, rate)
            .unwrap_or(Self(u64::MAX))
    }

    /// Like [`after_frames`](Self::after_frames), `None` on overflow.
    #[must_use]
    pub fn checked_after_frames(self, frames: u64, rate: SampleRate) -> Option<Self> {
        self.0.checked_add(frames_to_ticks(frames, rate)?).map(Self)
    }

    /// `self + d` rounded to the nearest tick, `None` on overflow.
    #[must_use]
    pub fn checked_add(self, d: Duration) -> Option<Self> {
        self.0.checked_add(duration_to_ticks(d)?).map(Self)
    }

    /// Number of whole frames at `rate` in `ticks`, rounded to the nearest frame
    /// (saturating).
    #[must_use]
    pub fn frames_in_ticks(ticks: u64, rate: SampleRate) -> u64 {
        let num = u128::from(ticks) * u128::from(rate.hz());
        let den = u128::from(Self::TICKS_PER_SECOND);
        u64::try_from((num + den / 2) / den).unwrap_or(u64::MAX)
    }

    /// Time from `earlier` to `self`, or `None` if `earlier` is later.
    #[must_use]
    pub fn checked_duration_since(self, earlier: Self) -> Option<Duration> {
        let ticks = self.0.checked_sub(earlier.0)?;
        Some(Duration::new(
            ticks / Self::TICKS_PER_SECOND,
            ((ticks % Self::TICKS_PER_SECOND) * 100) as u32,
        ))
    }
}

impl Add<Duration> for DeviceTime {
    type Output = Self;

    /// Adds `rhs`, rounded to the nearest tick. Saturates at `u64::MAX` ticks; use
    /// [`DeviceTime::checked_add`] for untrusted input.
    fn add(self, rhs: Duration) -> Self {
        self.checked_add(rhs).unwrap_or(Self(u64::MAX))
    }
}
