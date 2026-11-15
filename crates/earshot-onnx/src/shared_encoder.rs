//! One Parakeet TDT encoder session that several models borrow (T12.58), and the registry a host
//! keeps them in ([`Registry`]).
//!
//! The session is run only by its own thread (named [`THREAD_NAME`]). A borrower sends owned
//! inputs and waits for owned outputs, answered in the order asked, so no borrower's thread (a
//! below-normal preview thread included) ever runs or holds the encoder, and ONNX Runtime's `Run`
//! is never called twice at once on it (ort 2.0.0-rc.14's `Session::run` takes `&mut self`
//! because concurrent runs crashed with CUDA, `src/session/mod.rs`). The thread is a plain std
//! thread: on Windows a new thread starts at normal priority whatever the priority of the thread
//! that created it, so a preview asking first cannot lower it. No `unsafe`.
//!
//! **Inputs** are checked on the caller's side ([`EncoderInput::check`]) and a tensor that cannot
//! be built is that caller's error alone: neither touches the session or breaks the encoder.
//!
//! **Broken:** a session run that fails or panics marks the encoder broken. That call gets the
//! error, and every later call (queued or new) gets an error at once, never a hang; the session is
//! never run again. A [`Registry`] replaces a broken encoder at the next borrow.
//!
//! **Stalled:** a run still going [`STALL_LIMIT`] of time awake after it started marks the encoder
//! stalled (checked by waiting callers and by [`SharedEncoder::is_broken`], so by every registry
//! borrow). Time awake, not `Instant`: on Windows `Instant` is `QueryPerformanceCounter`, which
//! keeps counting while the computer sleeps (T9.16 saw capture timestamps jump across Modern
//! Standby), so a final pass in flight at a sleep would come back stalled ([`stalled`], T12.62):
//! callers waiting behind it get an error instead of waiting for ever (a stuck meeting never
//! blocks the next, T9.22), and the registry loads a fresh encoder for the next load. The hung run
//! cannot be stopped without `unsafe`: its thread and session stay until the run returns or the
//! process ends, so two encoders can sit on the graphics card in that state.
//!
//! **Life:** the thread holds the session and ends after the last [`SharedEncoder`] handle is
//! dropped (the job channel closes): the session, and the graphics memory it holds, is freed on
//! the encoder thread after any run in progress, not at the drop itself. A file removal or an
//! immediate reload right after the last borrower lets go can briefly still see the old session. A
//! drop never waits for the thread.

use std::collections::{BTreeMap, VecDeque};
use std::panic::AssertUnwindSafe;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Weak, mpsc};
use std::time::{Duration, Instant};

use crate::runtime::{OnnxError, SessionBackend, SessionConfig};

/// The encoder thread's name (fixed text: thread names reach crash logs).
pub const THREAD_NAME: &str = "earshot-shared-encoder";

/// How long one encoder run may take before the encoder counts as stalled. A judgement, not
/// measured: a run of one 30 s chunk is well under a second on CUDA (T12.3d), and T9.22's per-step
/// limit for the work after a meeting is 120 s, so a stalled encoder is replaced well inside it.
pub const STALL_LIMIT: Duration = Duration::from_secs(60);

/// How often a waiting caller looks whether the run it waits behind has stalled. An answer that
/// comes is taken at once; this only bounds how late a stall is noticed.
const STALL_POLL: Duration = Duration::from_millis(100);

/// How many calls the wait log keeps; older ones are dropped.
const KEPT_LOG: usize = 8192;

/// One encoder call's owned input: NeMo log-mel features `[n_mels][frames]` and the valid
/// frame count.
#[derive(Debug, Clone)]
pub(crate) struct EncoderInput {
    pub features: Vec<f32>,
    pub n_mels: usize,
    pub frames: usize,
    pub valid: usize,
}

impl EncoderInput {
    /// Whether the input has the shape the encoder takes: `n_mels * frames` features, at most
    /// `frames` valid frames, and a valid count that fits the encoder's `i64` length.
    ///
    /// # Errors
    /// [`OnnxError::Signature`] naming what is wrong.
    pub(crate) fn check(&self) -> Result<(), OnnxError> {
        let wrong = |m: String| Err(OnnxError::Signature(format!("encoder input: {m}")));
        if self.n_mels == 0 || self.features.len() != self.n_mels * self.frames {
            return wrong(format!(
                "{} features for {} mels by {} frames",
                self.features.len(),
                self.n_mels,
                self.frames
            ));
        }
        if self.valid > self.frames || i64::try_from(self.valid).is_err() {
            return wrong(format!("{} valid of {} frames", self.valid, self.frames));
        }
        Ok(())
    }
}

/// One encoder call's output: `(values [T'][D], T', D)`.
pub(crate) type EncoderOutput = (Vec<f32>, usize, usize);

/// Why an encoder call failed: its input (that caller's error alone), or the session.
#[derive(Debug)]
pub(crate) enum RunError {
    /// The input tensors could not be built: the session was not run.
    Input(OnnxError),
    /// The session's run, or reading its outputs, failed: the encoder is broken.
    Session(OnnxError),
}

/// What runs encoder calls on the encoder thread: the session, or a fake in tests.
pub(crate) trait EncoderRun: Send + 'static {
    fn run(&mut self, input: EncoderInput) -> Result<EncoderOutput, RunError>;
}

/// Where the encoder reads the time for waits (asked to started): the real clock, or a fake in
/// tests.
pub(crate) type Clock = Arc<dyn Fn() -> Instant + Send + Sync>;

/// Where the encoder reads time awake for the stall rule: a monotonic count from any origin that
/// leaves out time the computer slept ([`awake_clock`]), or a fake in tests.
pub(crate) type AwakeClock = Arc<dyn Fn() -> Duration + Send + Sync>;

/// Whether a run that started at `started` (time awake) has stalled by `now`: more than
/// [`STALL_LIMIT`] of time awake has passed. A clock that went backwards never stalls a run.
pub(crate) fn stalled(started: Duration, now: Duration) -> bool {
    now.saturating_sub(started) > STALL_LIMIT
}

/// The production awake clock: `earshot_proc::awake_time` (Windows' unbiased interrupt time) when
/// it can be read, else time since this clock was made by `Instant` (off Windows, where the stall
/// rule then counts a sleep as it did before T12.62). Chosen once, so one clock never mixes the
/// two.
pub(crate) fn awake_clock() -> AwakeClock {
    if earshot_proc::awake_time().is_some() {
        Arc::new(|| earshot_proc::awake_time().unwrap_or_default())
    } else {
        let origin = Instant::now();
        Arc::new(move || origin.elapsed())
    }
}

/// The answer to one call, and how long it waited from asked to started.
type Reply = (Result<EncoderOutput, OnnxError>, Duration);

struct Job {
    input: EncoderInput,
    asked: Instant,
    reply: mpsc::SyncSender<Reply>,
}

/// What the encoder thread and every handle share.
struct State {
    broken: AtomicBool,
    stalled: AtomicBool,
    /// When the run in progress started, in time awake (`None` between runs).
    running_since: Mutex<Option<Duration>>,
    clock: Clock,
    awake: AwakeClock,
}

impl State {
    fn now(&self) -> Instant {
        (self.clock)()
    }

    fn awake_now(&self) -> Duration {
        (self.awake)()
    }

    /// Whether the encoder may no longer be used: broken, or stalled (checked now, so a run past
    /// [`STALL_LIMIT`] of time awake marks it stalled here, [`stalled`]).
    fn unusable(&self) -> bool {
        if self.broken.load(Ordering::SeqCst) || self.stalled.load(Ordering::SeqCst) {
            return true;
        }
        let since = *self
            .running_since
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if since.is_some_and(|t| stalled(t, self.awake_now())) {
            self.stalled.store(true, Ordering::SeqCst);
            return true;
        }
        false
    }

    fn set_running(&self, since: Option<Duration>) {
        *self
            .running_since
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = since;
    }
}

/// What every handle shares; dropping the last one closes the job channel.
struct Inner {
    jobs: mpsc::Sender<Job>,
    state: Arc<State>,
    /// The latest [`KEPT_LOG`] calls: the asking thread and its wait ([`SharedEncoder::wait_log`]).
    /// The thread's handle, not its name: taking it clones a reference count, so a call allocates
    /// nothing for the log once it is full (T12.62); names are read only by `wait_log`.
    log: Mutex<VecDeque<(std::thread::Thread, Duration)>>,
    backend: SessionBackend,
    metadata: BTreeMap<String, String>,
    audio_shape: Option<Vec<i64>>,
}

/// A handle on a shared encoder ([`ParakeetTdt::load_sharing`] borrows one). Cloning makes
/// another borrower; the encoder lives while any handle does.
///
/// [`ParakeetTdt::load_sharing`]: crate::parakeet::ParakeetTdt::load_sharing
#[derive(Clone)]
pub struct SharedEncoder {
    inner: Arc<Inner>,
}

/// A weak handle: it keeps nothing alive ([`SharedEncoder::downgrade`]).
#[derive(Clone, Debug)]
pub struct WeakSharedEncoder(Weak<Inner>);

impl std::fmt::Debug for SharedEncoder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SharedEncoder")
            .field("backend", &self.inner.backend)
            .field("broken", &self.is_broken())
            .finish_non_exhaustive()
    }
}

impl std::fmt::Debug for Inner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SharedEncoder")
    }
}

fn broken_error() -> OnnxError {
    OnnxError::Runtime("the shared encoder is broken: an earlier run failed".to_owned())
}

fn stalled_error() -> OnnxError {
    OnnxError::Runtime("the shared encoder stopped answering: a run did not end".to_owned())
}

fn gone_error() -> OnnxError {
    OnnxError::Runtime("the shared encoder's thread has ended".to_owned())
}

impl SharedEncoder {
    /// Starts the encoder thread with `runner`; `backend`, `metadata` and `audio_shape` are what
    /// the session reported when it was loaded (read before it moved to the thread).
    ///
    /// # Errors
    /// [`OnnxError::Runtime`] when the thread cannot start.
    pub(crate) fn spawn(
        runner: impl EncoderRun,
        backend: SessionBackend,
        metadata: BTreeMap<String, String>,
        audio_shape: Option<Vec<i64>>,
    ) -> Result<Self, OnnxError> {
        Self::spawn_with_clock(
            runner,
            backend,
            metadata,
            audio_shape,
            Arc::new(Instant::now),
            awake_clock(),
        )
    }

    /// [`Self::spawn`] reading waits from `clock` and time awake from `awake`.
    fn spawn_with_clock(
        runner: impl EncoderRun,
        backend: SessionBackend,
        metadata: BTreeMap<String, String>,
        audio_shape: Option<Vec<i64>>,
        clock: Clock,
        awake: AwakeClock,
    ) -> Result<Self, OnnxError> {
        let (jobs, rx) = mpsc::channel::<Job>();
        let state = Arc::new(State {
            broken: AtomicBool::new(false),
            stalled: AtomicBool::new(false),
            running_since: Mutex::new(None),
            clock,
            awake,
        });
        let shared = Arc::clone(&state);
        std::thread::Builder::new()
            .name(THREAD_NAME.to_owned())
            .spawn(move || serve(runner, &rx, &shared))
            .map_err(|_| OnnxError::Runtime("the shared encoder's thread did not start".into()))?;
        Ok(Self {
            inner: Arc::new(Inner {
                jobs,
                state,
                log: Mutex::new(VecDeque::new()),
                backend,
                metadata,
                audio_shape,
            }),
        })
    }

    /// The provider requested and where ONNX Runtime placed the encoder's nodes.
    #[must_use]
    pub fn backend(&self) -> &SessionBackend {
        &self.inner.backend
    }

    /// Whether the encoder may no longer be used: a run failed or panicked, or the run in progress
    /// has gone past [`STALL_LIMIT`] of time awake (checked now). Such an encoder answers every
    /// call with an error.
    #[must_use]
    pub fn is_broken(&self) -> bool {
        self.inner.state.unusable()
    }

    /// How many handles exist now (each borrowing model holds one).
    #[must_use]
    pub fn users(&self) -> usize {
        Arc::strong_count(&self.inner)
    }

    /// A handle that keeps nothing alive.
    #[must_use]
    pub fn downgrade(&self) -> WeakSharedEncoder {
        WeakSharedEncoder(Arc::downgrade(&self.inner))
    }

    pub(crate) fn metadata(&self) -> &BTreeMap<String, String> {
        &self.inner.metadata
    }

    pub(crate) fn audio_shape(&self) -> Option<Vec<i64>> {
        self.inner.audio_shape.clone()
    }

    /// Runs one call on the encoder thread and waits for it: the output and how long the call
    /// waited before its run started (zero when it never reached the thread). An input of the
    /// wrong shape is refused here, before the thread sees it.
    pub(crate) fn run(&self, input: EncoderInput) -> Reply {
        let reply = match input.check() {
            Err(e) => (Err(e), Duration::ZERO),
            Ok(()) => match self.submit(input) {
                Ok(answer) => self.wait(&answer),
                Err(reply) => reply,
            },
        };
        let asker = std::thread::current();
        let mut log = self
            .inner
            .log
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if log.len() == KEPT_LOG {
            log.pop_front();
        }
        log.push_back((asker, reply.1));
        drop(log);
        reply
    }

    /// Every call's wait (asked to started) with the name of the thread that asked, oldest first,
    /// at most the latest [`KEPT_LOG`]. For evaluation reports.
    #[must_use]
    pub fn wait_log(&self) -> Vec<(String, Duration)> {
        self.inner
            .log
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .map(|(thread, waited)| (thread.name().unwrap_or("unnamed").to_owned(), *waited))
            .collect()
    }

    /// Queues one call: where its answer will come, or the answer now (unusable, or no thread).
    fn submit(&self, input: EncoderInput) -> Result<mpsc::Receiver<Reply>, Reply> {
        if self.inner.state.unusable() {
            return Err((Err(self.unusable_error()), Duration::ZERO));
        }
        let (reply, answer) = mpsc::sync_channel(1);
        let job = Job {
            input,
            asked: self.inner.state.now(),
            reply,
        };
        if self.inner.jobs.send(job).is_err() {
            return Err((Err(gone_error()), Duration::ZERO));
        }
        Ok(answer)
    }

    /// Waits for a queued call's answer. The thread answers every job it takes, and one that
    /// ended without answering dropped the reply sender; a run that stalls ends the wait with an
    /// error. So this never waits for ever on a thread that is gone or stuck.
    fn wait(&self, answer: &mpsc::Receiver<Reply>) -> Reply {
        loop {
            match answer.recv_timeout(STALL_POLL) {
                Ok(reply) => return reply,
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    return (Err(gone_error()), Duration::ZERO);
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    // Marks a run past the limit stalled. A broken encoder still answers this
                    // job (with an error), so only a stall ends the wait here.
                    let _ = self.inner.state.unusable();
                    if self.inner.state.stalled.load(Ordering::SeqCst) {
                        return (Err(stalled_error()), Duration::ZERO);
                    }
                }
            }
        }
    }

    fn unusable_error(&self) -> OnnxError {
        if self.inner.state.stalled.load(Ordering::SeqCst) {
            stalled_error()
        } else {
            broken_error()
        }
    }
}

impl WeakSharedEncoder {
    /// The encoder, while any handle on it lives.
    #[must_use]
    pub fn upgrade(&self) -> Option<SharedEncoder> {
        self.0.upgrade().map(|inner| SharedEncoder { inner })
    }
}

/// The encoder thread: answers jobs in the order they came until every handle is gone.
fn serve(mut runner: impl EncoderRun, jobs: &mpsc::Receiver<Job>, state: &State) {
    while let Ok(job) = jobs.recv() {
        let started = state.now();
        let waited = started.saturating_duration_since(job.asked);
        let result = if state.broken.load(Ordering::SeqCst) {
            Err(broken_error())
        } else if state.stalled.load(Ordering::SeqCst) {
            Err(stalled_error())
        } else {
            state.set_running(Some(state.awake_now()));
            let ran = std::panic::catch_unwind(AssertUnwindSafe(|| runner.run(job.input)));
            state.set_running(None);
            match ran {
                Ok(Ok(out)) => Ok(out),
                // The session was not run: that caller's error alone.
                Ok(Err(RunError::Input(e))) => Err(e),
                Ok(Err(RunError::Session(e))) => {
                    state.broken.store(true, Ordering::SeqCst);
                    Err(e)
                }
                Err(_) => {
                    state.broken.store(true, Ordering::SeqCst);
                    Err(OnnxError::Runtime(
                        "the shared encoder panicked in a run".to_owned(),
                    ))
                }
            }
        };
        // A borrower that stopped waiting dropped its receiver: nothing to answer.
        let _ = job.reply.send((result, waited));
    }
}

/// What a [`Registry`] keeps: a handle it can hold weakly and ask whether it may still be used.
pub trait Borrowable: Clone {
    /// The weak form of the handle.
    type Weak: Clone;
    /// A weak handle that keeps nothing alive.
    fn downgrade(&self) -> Self::Weak;
    /// The handle, while any strong one lives.
    fn upgrade(weak: &Self::Weak) -> Option<Self>;
    /// Whether it may no longer be used (broken, or stalled).
    fn is_broken(&self) -> bool;
}

impl Borrowable for SharedEncoder {
    type Weak = WeakSharedEncoder;

    fn downgrade(&self) -> WeakSharedEncoder {
        SharedEncoder::downgrade(self)
    }

    fn upgrade(weak: &WeakSharedEncoder) -> Option<Self> {
        weak.upgrade()
    }

    fn is_broken(&self) -> bool {
        SharedEncoder::is_broken(self)
    }
}

/// Shared encoders by key, held weakly ([`Registry::borrow`]). A host keeps one per process.
pub struct Registry<K, H: Borrowable> {
    slots: Mutex<Vec<(K, H::Weak)>>,
    config: fn() -> SessionConfig,
}

impl<K: PartialEq + Clone, H: Borrowable> Registry<K, H> {
    /// An empty registry whose first loads use `config()`.
    pub const fn new(config: fn() -> SessionConfig) -> Self {
        Self {
            slots: Mutex::new(Vec::new()),
            config,
        }
    }

    /// The configuration every first load of this registry uses.
    #[must_use]
    pub fn configuration(&self) -> SessionConfig {
        (self.config)()
    }

    /// The live, usable encoder of `key`, or one `load`ed now with the registry's fixed
    /// configuration. Gone, broken and stalled entries are dropped first, so such an encoder is
    /// replaced. The lock is held across the lookup and the load only, so two first asks give
    /// one load.
    ///
    /// # Errors
    /// `load`'s error; nothing is kept then.
    pub fn borrow<E>(
        &self,
        key: &K,
        load: impl FnOnce(&SessionConfig) -> Result<H, E>,
    ) -> Result<H, E> {
        let mut slots = self
            .slots
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        slots.retain(|(_, weak)| H::upgrade(weak).is_some_and(|h| !h.is_broken()));
        if let Some(h) = slots
            .iter()
            .find(|(k, _)| k == key)
            .and_then(|(_, weak)| H::upgrade(weak))
        {
            return Ok(h);
        }
        let h = load(&self.configuration())?;
        slots.push((key.clone(), h.downgrade()));
        Ok(h)
    }
}

#[cfg(test)]
#[path = "shared_encoder_tests.rs"]
mod tests;
