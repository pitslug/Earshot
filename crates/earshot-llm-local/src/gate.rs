//! The one gate in front of llama.cpp (T8.4af): while a [`LlamaHold`] lives, the first llama call
//! of any thread waits.
//!
//! The exe delay-loads `llama.dll`, and which set of DLLs it gets is decided by what is loaded when
//! its first symbol is called; a set cannot be replaced after that. Choosing a second set can take
//! minutes (hashing the files, a trial in a child), and an app must not hold its window or a
//! recording for that. So the app takes a hold before anything else, chooses on a thread, and every
//! path into llama.cpp waits here, wherever it comes from. [`crate::ffi::init`] is the only door:
//! every llama call runs after it (`ffi_calls_pass_the_gate`). A process that never takes a hold
//! pays one atomic read.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Condvar, Mutex, PoisonError};

static HOLDS: AtomicUsize = AtomicUsize::new(0);
static LOCK: Mutex<()> = Mutex::new(());
static RELEASED: Condvar = Condvar::new();
/// Callers that had to sleep at the gate, each counted once.
static SLEPT: AtomicUsize = AtomicUsize::new(0);

/// While this lives, no llama call runs. A panic on the holder's thread drops it like any other
/// value, so llama.cpp is never shut off for good.
#[derive(Debug)]
#[must_use = "dropping the hold opens the gate"]
pub struct LlamaHold(());

/// Closes the gate until the hold is dropped. Take it before anything can call llama.cpp: a call
/// already past the gate is not stopped.
pub fn hold_llama() -> LlamaHold {
    let _lock = LOCK.lock().unwrap_or_else(PoisonError::into_inner);
    HOLDS.fetch_add(1, Ordering::AcqRel);
    LlamaHold(())
}

/// Whether a hold is on now: a caller that only wants to know something about the build can then
/// decide without waiting.
#[must_use]
pub fn llama_held() -> bool {
    HOLDS.load(Ordering::Acquire) > 0
}

impl Drop for LlamaHold {
    fn drop(&mut self) {
        let _lock = LOCK.lock().unwrap_or_else(PoisonError::into_inner);
        HOLDS.fetch_sub(1, Ordering::AcqRel);
        RELEASED.notify_all();
    }
}

/// How many callers have slept at the gate in this process. For tests: it says a call reached the
/// gate and stopped there, which a clock cannot, because a first llama call can itself be slow.
#[doc(hidden)]
#[must_use]
pub fn callers_slept_at_the_gate() -> usize {
    SLEPT.load(Ordering::Acquire)
}

/// Waits until no hold is on. A worker that can be cancelled calls this itself before anything
/// that loads a model, so it can look at its token between the wait and the load.
pub fn wait_for_llama() {
    if !llama_held() {
        return;
    }
    let mut lock = LOCK.lock().unwrap_or_else(PoisonError::into_inner);
    // Counted under the lock: a release cannot slip between this and the sleep.
    if llama_held() {
        SLEPT.fetch_add(1, Ordering::AcqRel);
    }
    while llama_held() {
        lock = RELEASED.wait(lock).unwrap_or_else(PoisonError::into_inner);
    }
}
