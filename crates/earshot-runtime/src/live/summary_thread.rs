//! Summaries made during the meeting on their own thread (T8.3j, [`crate::live_summaries`]).
//!
//! The session sends the thread one [`Job`] per finished window: the live transcript so far
//! and its time chunks. The thread maps the jobs one at a time, in order, with the session's
//! [`MapChunk`] (the local model, loaded per window and freed after it), and after each
//! window publishes the partial and what happened ([`Mapped`]), which the session reports
//! and records at the end.
//!
//! Transcription never waits for it: the transcription workers share nothing with this
//! thread and the session only queues jobs (never blocks). The thread runs at below-normal
//! OS priority ([`crate::priority`]) when [`LiveSummaries::background`] says so, and stops
//! at once when it cannot lower itself. llama.cpp's own compute threads are not lowered;
//! their number is the meeting's share of the cores
//! ([`crate::live_summaries::live_summary_threads`], T8.3jb2 measures it).
//!
//! The end follows the speaker thread (`speaker_thread` docs): [`SummaryThread::finish`]
//! cancels the window in progress (a generation stops between tokens; a model load cannot
//! be interrupted), drops the jobs not started (the summary after the meeting maps those
//! windows), waits at most `wait` and then leaves the thread to end on its own; a thread
//! left like that publishes nothing more. A window that was waiting for llama.cpp's gate (the app
//! still choosing its DLLs, `earshot_llm_local::gate`) looks at the token again before it loads
//! anything, so a meeting that ended meanwhile starts no model load nobody wants.

use std::ops::Range;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, TryRecvError, channel};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use earshot_core::asr::CancelToken;
use earshot_core::prompt::SummaryInput;

use crate::RuntimeError;
use crate::live_summaries::{LivePartial, LiveSummaries, MapChunk};

/// One window to map.
pub(crate) struct Job {
    /// The window index.
    pub(crate) window: u64,
    /// The live transcript up to the end of the window.
    pub(crate) input: SummaryInput,
    /// Its time chunks; the last one is the window.
    pub(crate) ranges: Vec<Range<usize>>,
}

/// What one window's map did.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Mapped {
    /// The window index.
    pub(crate) window: u64,
    /// How long the map took (load included).
    pub(crate) took: Duration,
    /// Why there is no partial; `None` when there is one.
    pub(crate) error: Option<String>,
}

#[derive(Default)]
struct Published {
    partials: Vec<LivePartial>,
    mapped: Vec<Mapped>,
    stopped: Option<String>,
}

/// The session's summary thread (module docs).
pub(crate) struct SummaryThread {
    to: Option<Sender<Job>>,
    shared: Arc<Mutex<Published>>,
    cancel: CancelToken,
    detached: Arc<AtomicBool>,
    done: Receiver<()>,
    thread: Option<JoinHandle<()>>,
    model: String,
    seconds: u32,
    /// The session already said why it stopped this thread ([`Self::stop_work`]).
    told: bool,
}

/// What [`SummaryThread::finish`] found.
pub(crate) struct Finished {
    /// The record of what it mapped, `None` when it mapped nothing.
    pub(crate) record: Option<crate::live_summaries::LiveSummaryRecord>,
    /// The windows mapped and not yet reported.
    pub(crate) mapped: Vec<Mapped>,
    /// Why it stopped by itself (no lower priority, a crash), whether or not it mapped
    /// anything; `None` when it ran to the end or the session stopped it.
    pub(crate) stopped_itself: Option<String>,
}

/// Why the thread stopped when it could not lower its priority.
fn priority_reason(why: &str) -> String {
    format!("summarising during the meeting could not run below normal priority: {why}")
}

impl SummaryThread {
    /// Starts the thread.
    pub(crate) fn spawn(summaries: &LiveSummaries) -> Result<Self, RuntimeError> {
        let mapper = Arc::clone(&summaries.mapper);
        let background = summaries.background;
        let (to, inbox) = channel::<Job>();
        let (done_tx, done) = channel();
        let shared = Arc::new(Mutex::new(Published::default()));
        let cancel = CancelToken::new();
        let detached = Arc::new(AtomicBool::new(false));
        let (out, token, left) = (Arc::clone(&shared), cancel.clone(), Arc::clone(&detached));
        let model = mapper.model_id().to_owned();
        let seconds = mapper.seconds();
        let thread = std::thread::Builder::new()
            .name("earshot-live-summaries".into())
            .spawn(move || {
                run(
                    &*mapper,
                    &inbox,
                    &token,
                    &out,
                    &left,
                    background,
                    &earshot_llm_local::wait_for_llama,
                );
                let _ = done_tx.send(());
            })
            .map_err(RuntimeError::Spawn)?;
        Ok(Self {
            to: Some(to),
            shared,
            cancel,
            detached,
            done,
            thread: Some(thread),
            model,
            seconds,
            told: false,
        })
    }

    /// The window length in seconds.
    pub(crate) fn seconds(&self) -> u32 {
        self.seconds
    }

    /// Queues a window (never blocks).
    pub(crate) fn send(&self, job: Job) {
        if let Some(to) = &self.to {
            let _ = to.send(job);
        }
    }

    /// The windows mapped since the last call.
    pub(crate) fn drain(&self) -> Vec<Mapped> {
        std::mem::take(
            &mut self
                .shared
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .mapped,
        )
    }

    /// Stops mapping for the rest of the meeting because of `why`; the partials finished
    /// before stay. Returns whether the thread was still mapping.
    pub(crate) fn stop_work(&mut self, why: &str) -> bool {
        let running = matches!(self.done.try_recv(), Err(TryRecvError::Empty)) && self.to.is_some();
        if running {
            let mut p = self.shared.lock().unwrap_or_else(PoisonError::into_inner);
            p.stopped.get_or_insert_with(|| why.to_owned());
            self.told = true;
        }
        self.to = None;
        self.cancel.cancel();
        running
    }

    /// Tells the thread to stop (cancels the window in progress, takes no more) without
    /// waiting; [`Self::finish`] waits.
    pub(crate) fn request_stop(&mut self) {
        self.cancel.cancel();
        self.to = None;
    }

    /// Stops the thread and waits at most `wait` for it (module docs).
    pub(crate) fn finish(mut self, wait: Duration) -> Finished {
        self.cancel.cancel();
        self.to = None;
        let ended = !matches!(self.done.recv_timeout(wait), Err(RecvTimeoutError::Timeout));
        let mut crashed = false;
        if ended && let Some(t) = self.thread.take() {
            crashed = t.join().is_err();
        } else {
            self.detached.store(true, Ordering::SeqCst);
        }
        let mut p = self.shared.lock().unwrap_or_else(PoisonError::into_inner);
        if crashed {
            p.stopped
                .get_or_insert_with(|| "the live summary thread crashed".to_owned());
        }
        let mapped = std::mem::take(&mut p.mapped);
        let stopped_itself = if self.told { None } else { p.stopped.clone() };
        let record = (!p.partials.is_empty()).then(|| crate::live_summaries::LiveSummaryRecord {
            model: self.model.clone(),
            seconds: self.seconds,
            partials: std::mem::take(&mut p.partials),
            stopped: p.stopped.take(),
        });
        Finished {
            record,
            mapped,
            stopped_itself,
        }
    }
}

impl Drop for SummaryThread {
    fn drop(&mut self) {
        self.detached.store(true, Ordering::SeqCst);
        self.cancel.cancel();
        self.to = None;
    }
}

/// The thread's loop (module docs).
fn run(
    mapper: &dyn MapChunk,
    inbox: &Receiver<Job>,
    cancel: &CancelToken,
    shared: &Mutex<Published>,
    detached: &AtomicBool,
    background: bool,
    gate: &dyn Fn(),
) {
    if background && let Err(why) = crate::priority::lower_current_thread() {
        let mut p = shared.lock().unwrap_or_else(PoisonError::into_inner);
        if !detached.load(Ordering::SeqCst) {
            p.stopped = Some(priority_reason(&why));
        }
        return;
    }
    while let Ok(job) = inbox.recv() {
        if cancel.is_cancelled() {
            return;
        }
        gate();
        if cancel.is_cancelled() {
            return;
        }
        let started = Instant::now();
        let result = mapper.map(&job.input, &job.ranges, cancel);
        if cancel.is_cancelled() {
            // Stopped mid-window: the summary after the meeting maps it.
            return;
        }
        let mut p = shared.lock().unwrap_or_else(PoisonError::into_inner);
        if detached.load(Ordering::SeqCst) {
            return;
        }
        let error = match result {
            Ok(summary) => {
                p.partials.push(LivePartial {
                    window: job.window,
                    summary,
                });
                None
            }
            Err(e) => Some(e),
        };
        p.mapped.push(Mapped {
            window: job.window,
            took: started.elapsed(),
            error,
        });
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicU32;

    use earshot_core::summary::SummaryV1;

    use super::*;

    /// Counts the maps and refuses them.
    #[derive(Default)]
    struct Counting(AtomicU32);

    impl MapChunk for Counting {
        fn model_id(&self) -> &str {
            "fake-live-model"
        }

        fn seconds(&self) -> u32 {
            60
        }

        fn map(
            &self,
            _: &SummaryInput,
            _: &[Range<usize>],
            _: &CancelToken,
        ) -> Result<SummaryV1, String> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Err("refused".into())
        }
    }

    /// `run` over one window with `gate`; how many maps ran.
    fn maps_with_gate(cancel: &CancelToken, gate: &dyn Fn()) -> u32 {
        let mapper = Counting::default();
        let (to, inbox) = channel();
        to.send(Job {
            window: 0,
            input: SummaryInput::default(),
            ranges: Vec::new(),
        })
        .unwrap();
        drop(to);
        let shared = Mutex::new(Published::default());
        run(
            &mapper,
            &inbox,
            cancel,
            &shared,
            &AtomicBool::new(false),
            false,
            gate,
        );
        mapper.0.load(Ordering::SeqCst)
    }

    #[test]
    fn live_summary_cancelled_while_at_the_gate_loads_nothing() {
        let cancel = CancelToken::new();
        let during = cancel.clone();
        assert_eq!(maps_with_gate(&cancel, &|| during.cancel()), 0);
        // The control: through an open gate the window is mapped.
        assert_eq!(maps_with_gate(&CancelToken::new(), &|| {}), 1);
    }
}
