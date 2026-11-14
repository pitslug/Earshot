//! One job at a time on its own named thread (T12.53): the runner behind the voice sample,
//! the audio check and the benchmark.
//!
//! A start takes the slot (refused `busy` while the last job's thread is unfinished, else
//! that thread is joined), takes the caller's pass, makes a new [`CancelToken`] and spawns a
//! named thread. The thread runs the work under `catch_unwind` (a panic becomes the caller's
//! panic event), drops the pass, then sends exactly one terminal event. A thread that cannot
//! start is reported as [`crate::plain_text::INTERNAL`], never the operating system's text.

use std::panic::AssertUnwindSafe;
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::thread::JoinHandle;
use std::time::Duration;

use earshot_core::asr::CancelToken;

use crate::ipc::IpcError;

/// How a job's thread is started (a seam: tests make it fail).
pub type Spawn = fn(&'static str, Box<dyn FnOnce() + Send>) -> std::io::Result<JoinHandle<()>>;

/// The production [`Spawn`]: a thread named `name`.
pub(crate) fn spawn_named(
    name: &'static str,
    body: Box<dyn FnOnce() + Send>,
) -> std::io::Result<JoinHandle<()>> {
    std::thread::Builder::new().name(name.into()).spawn(body)
}

struct Running {
    cancel: CancelToken,
    thread: JoinHandle<()>,
}

/// At most one job at a time (see the module docs).
pub struct SlotJob {
    /// The thread's name: fixed text, because it goes into crash logs.
    thread_name: &'static str,
    /// What a start says while a job runs.
    busy: &'static str,
    spawn: Spawn,
    slot: Mutex<Option<Running>>,
}

impl SlotJob {
    /// A runner whose jobs run on threads named `thread_name`, refusing a second start with
    /// `busy`.
    pub fn new(thread_name: &'static str, busy: &'static str) -> Self {
        Self {
            thread_name,
            busy,
            spawn: spawn_named,
            slot: Mutex::new(None),
        }
    }

    /// The runner with its threads started by `spawn`.
    #[cfg(test)]
    pub fn with_spawn(mut self, spawn: Spawn) -> Self {
        self.set_spawn(spawn);
        self
    }

    /// Starts this runner's threads with `spawn` from now on.
    #[cfg(test)]
    pub fn set_spawn(&mut self, spawn: Spawn) {
        self.spawn = spawn;
    }

    fn slot(&self) -> MutexGuard<'_, Option<Running>> {
        self.slot.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Starts a job: `enter` takes its pass (with the slot held), `work` runs on the thread,
    /// `panicked` is the event of a panic in `work`, and `send` gets the one terminal event
    /// after the pass is dropped.
    ///
    /// # Errors
    /// `busy` (retryable) while the last job runs; `enter`'s refusal; `internal` with fixed
    /// copy when the thread cannot start (the pass is dropped).
    pub fn start<P, E>(
        &self,
        enter: impl FnOnce() -> Result<P, IpcError>,
        work: impl FnOnce(&CancelToken) -> E + Send + 'static,
        panicked: impl FnOnce() -> E + Send + 'static,
        send: impl FnOnce(E) + Send + 'static,
    ) -> Result<(), IpcError>
    where
        P: Send + 'static,
    {
        let mut slot = self.slot();
        if let Some(old) = slot.take() {
            if !old.thread.is_finished() {
                *slot = Some(old);
                return Err(IpcError::busy(self.busy));
            }
            let _ = old.thread.join();
        }
        let pass = enter()?;
        let cancel = CancelToken::new();
        let token = cancel.clone();
        let body = Box::new(move || {
            let event = std::panic::catch_unwind(AssertUnwindSafe(|| work(&token)))
                .unwrap_or_else(|_| panicked());
            drop(pass);
            send(event);
        });
        let thread = (self.spawn)(self.thread_name, body).map_err(|_| IpcError::internal())?;
        *slot = Some(Running { cancel, thread });
        Ok(())
    }

    /// Cancels the running job (its own terminal event follows unless it already ended).
    pub fn cancel(&self) {
        if let Some(r) = self.slot().as_ref() {
            r.cancel.cancel();
        }
    }

    /// Waits for the running job.
    #[cfg(test)]
    pub fn join(&self) {
        if let Some(r) = self.slot().take() {
            let _ = r.thread.join();
        }
    }
}

/// Progress of `total` reported at most once per `step` of progress (not wall time):
/// `report(done_ms, total_ms)` for the first report and then each time a step is passed,
/// `done_ms` capped at `total_ms`.
pub fn progress_every(
    step: Duration,
    total: Duration,
    mut report: impl FnMut(u64, u64),
) -> impl FnMut(Duration) {
    let total_ms = u64::try_from(total.as_millis()).unwrap_or(u64::MAX);
    let step = u64::try_from(step.as_millis()).unwrap_or(1).max(1);
    let mut next = 0u64;
    move |d: Duration| {
        let ms = u64::try_from(d.as_millis()).unwrap_or(u64::MAX);
        if ms >= next {
            report(ms.min(total_ms), total_ms);
            next = (ms / step + 1) * step;
        }
    }
}

#[cfg(test)]
#[path = "slot_job_tests.rs"]
mod tests;
