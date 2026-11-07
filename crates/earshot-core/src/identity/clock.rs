//! Injected wall-clock time.

use std::sync::atomic::{AtomicU64, Ordering};

/// Source of wall-clock time. Core never reads the system clock (PLAN.md 3.2); adapters
/// implement this over the OS clock, tests and simulators use [`ManualClock`].
pub trait Clock {
    /// Milliseconds since the Unix epoch (1970-01-01T00:00:00Z).
    fn now_unix_ms(&self) -> u64;
}

impl<C: Clock + ?Sized> Clock for &C {
    fn now_unix_ms(&self) -> u64 {
        (**self).now_unix_ms()
    }
}

/// A clock that only moves when told to. Thread-safe.
#[derive(Debug, Default)]
pub struct ManualClock {
    ms: AtomicU64,
}

impl ManualClock {
    /// A clock reading `unix_ms`.
    pub fn new(unix_ms: u64) -> Self {
        ManualClock {
            ms: AtomicU64::new(unix_ms),
        }
    }

    /// Sets the reading (it may move backwards, like a stepped system clock).
    pub fn set_ms(&self, unix_ms: u64) {
        self.ms.store(unix_ms, Ordering::SeqCst);
    }

    /// Moves the reading forward by `delta_ms` (saturating).
    pub fn advance_ms(&self, delta_ms: u64) {
        let _ = self.ms.try_update(Ordering::SeqCst, Ordering::SeqCst, |m| {
            Some(m.saturating_add(delta_ms))
        });
    }
}

impl Clock for ManualClock {
    fn now_unix_ms(&self) -> u64 {
        self.ms.load(Ordering::SeqCst)
    }
}
