//! Keeping the computer awake while a meeting finishes (T14.56): from the end of its recording
//! until its final pass and summary end, and for the life of a re-transcribe or re-summarise job.
//! An idle computer that sleeps then only pauses the work, but the user waits for it on wake.
//!
//! The request is Windows' system-required power request (never display required: the screen may
//! still turn off), the same call the capture holds while it records (T9.16), taken through the
//! WASAPI adapter crate (all `unsafe` there) as [`crate::power`] and [`crate::priority`] wrap
//! theirs. A closed lid, the power button and Sleep still sleep. Other platforms have no power
//! API yet: there a hold succeeds and asks nothing.
//!
//! Each piece of work holds its own request ([`AwakeHold`], released when dropped, so on every
//! way off its thread, a panic's unwind included); Windows keeps the computer awake while any is
//! set. A refusal never costs the work: the caller reports it once and goes on ([`Refused`]).
//! Hosts reach it through a [`KeepAwake`] (production: [`SystemKeepAwake`]; tests:
//! [`FakeKeepAwake`]).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

/// Windows refused the power request. What it said is not kept: no work depends on it, and the
/// caller says only that the computer may sleep.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Refused;

/// A held power request: dropping it releases the request.
#[must_use = "dropping the hold releases the power request at once"]
pub struct AwakeHold {
    _held: Box<dyn Send>,
}

impl AwakeHold {
    /// A hold whose release is dropping `held` (an adapter's own request, or a test's guard).
    pub fn new(held: impl Send + 'static) -> Self {
        Self {
            _held: Box::new(held),
        }
    }
}

impl std::fmt::Debug for AwakeHold {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("AwakeHold")
    }
}

/// Something that keeps the computer awake while a hold lives (the OS, or a test fake).
pub trait KeepAwake: Send + Sync {
    /// Takes a request for work after a recording. Must not block.
    ///
    /// # Errors
    /// [`Refused`] when the OS refuses; the work goes on without it.
    fn hold(&self) -> Result<AwakeHold, Refused>;
}

/// The OS's power request (module docs).
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemKeepAwake;

impl KeepAwake for SystemKeepAwake {
    fn hold(&self) -> Result<AwakeHold, Refused> {
        #[cfg(windows)]
        {
            use earshot_capture_wasapi::wasapi::{AwakeReason, KeepAwake as Request};
            Request::hold_for(AwakeReason::Finishing)
                .map(AwakeHold::new)
                .map_err(|_| Refused)
        }
        #[cfg(not(windows))]
        {
            Ok(AwakeHold::new(()))
        }
    }
}

/// The OS's power request, shared ([`SystemKeepAwake`]).
#[must_use]
pub fn system() -> Arc<dyn KeepAwake> {
    Arc::new(SystemKeepAwake)
}

/// A [`KeepAwake`] for tests: counts the holds alive and taken, and refuses while told to. Clones
/// share the counts.
#[derive(Debug, Clone, Default)]
pub struct FakeKeepAwake {
    state: Arc<FakeState>,
}

#[derive(Debug, Default)]
struct FakeState {
    alive: AtomicUsize,
    taken: AtomicUsize,
    refused: AtomicUsize,
    refuse: AtomicBool,
}

/// What a [`FakeKeepAwake`] hold releases when dropped.
struct FakeGuard(Arc<FakeState>);

impl Drop for FakeGuard {
    fn drop(&mut self) {
        self.0.alive.fetch_sub(1, Ordering::SeqCst);
    }
}

impl FakeKeepAwake {
    /// A fake that grants every hold.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// A fake that refuses every hold.
    #[must_use]
    pub fn refusing() -> Self {
        let fake = Self::default();
        fake.state.refuse.store(true, Ordering::SeqCst);
        fake
    }

    /// The holds alive now.
    #[must_use]
    pub fn alive(&self) -> usize {
        self.state.alive.load(Ordering::SeqCst)
    }

    /// The holds granted so far.
    #[must_use]
    pub fn taken(&self) -> usize {
        self.state.taken.load(Ordering::SeqCst)
    }

    /// The holds refused so far.
    #[must_use]
    pub fn refused(&self) -> usize {
        self.state.refused.load(Ordering::SeqCst)
    }
}

impl KeepAwake for FakeKeepAwake {
    fn hold(&self) -> Result<AwakeHold, Refused> {
        if self.state.refuse.load(Ordering::SeqCst) {
            self.state.refused.fetch_add(1, Ordering::SeqCst);
            return Err(Refused);
        }
        self.state.alive.fetch_add(1, Ordering::SeqCst);
        self.state.taken.fetch_add(1, Ordering::SeqCst);
        Ok(AwakeHold::new(FakeGuard(Arc::clone(&self.state))))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keep_awake_fake_counts_holds_alive_until_dropped() {
        let fake = FakeKeepAwake::new();
        let first = fake.hold().unwrap();
        let second = fake.clone().hold().unwrap();
        assert_eq!((fake.alive(), fake.taken()), (2, 2));
        drop(first);
        assert_eq!(fake.alive(), 1);
        drop(second);
        assert_eq!((fake.alive(), fake.taken(), fake.refused()), (0, 2, 0));
    }

    #[test]
    fn keep_awake_refusing_fake_grants_nothing() {
        let fake = FakeKeepAwake::refusing();
        assert_eq!(fake.hold().unwrap_err(), Refused);
        assert_eq!((fake.alive(), fake.taken(), fake.refused()), (0, 0, 1));
    }

    #[test]
    fn keep_awake_a_hold_released_by_a_panic_unwind() {
        let fake = FakeKeepAwake::new();
        let inner = fake.clone();
        let result = std::panic::catch_unwind(move || {
            let _hold = inner.hold().unwrap();
            panic!("work failed");
        });
        assert!(result.is_err());
        assert_eq!(fake.alive(), 0);
    }

    /// The real request: accepted, side by side, and released (Windows has no unelevated call
    /// that reads a request back; the real proof is the owner's hand test past the idle timeout).
    #[test]
    fn keep_awake_system_request_is_accepted_side_by_side() {
        let awake = system();
        let first = awake.hold().expect("the system request");
        let second = awake.hold().expect("a second beside it");
        drop(first);
        drop(second);
    }
}
