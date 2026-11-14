//! One Parakeet TDT encoder session that several models borrow (T12.58).
//!
//! The session is run only by its own thread (named [`THREAD_NAME`]). A borrower sends owned
//! inputs and waits for owned outputs, answered in the order asked, so no borrower's thread (a
//! below-normal preview thread included) ever runs or holds the encoder, and ONNX Runtime's `Run`
//! is never called twice at once on it (ort 2.0.0-rc.14's `Session::run` takes `&mut self`
//! because concurrent runs crashed with CUDA, `src/session/mod.rs`). The thread is a plain std
//! thread: on Windows a new thread starts at normal priority whatever the priority of the thread
//! that created it, so a preview asking first cannot lower it. No `unsafe`.
//!
//! **Broken:** a run that fails or panics marks the encoder broken. That call gets the error, and
//! every later call (queued or new) gets an error at once, never a hang; the session is never run
//! again. A host that keeps encoders (`earshot_runtime::backends`) replaces a broken one.
//!
//! **Life:** the thread holds the session and ends when the last [`SharedEncoder`] handle is
//! dropped (the job channel closes), after the run in progress; the session, and the graphics
//! memory it holds, is freed then. A drop never waits for the thread: a borrower dropped mid-run
//! returns at once and the thread finishes that run alone.

use std::collections::{BTreeMap, VecDeque};
use std::panic::AssertUnwindSafe;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Weak, mpsc};
use std::time::{Duration, Instant};

use crate::runtime::{OnnxError, SessionBackend};

/// The encoder thread's name (fixed text: thread names reach crash logs).
pub const THREAD_NAME: &str = "earshot-shared-encoder";

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

/// One encoder call's output: `(values [T'][D], T', D)`.
pub(crate) type EncoderOutput = (Vec<f32>, usize, usize);

/// What runs encoder calls on the encoder thread: the session, or a fake in tests.
pub(crate) trait EncoderRun: Send + 'static {
    fn run(&mut self, input: EncoderInput) -> Result<EncoderOutput, OnnxError>;
}

/// The answer to one call, and how long it waited from asked to started.
type Reply = (Result<EncoderOutput, OnnxError>, Duration);

struct Job {
    input: EncoderInput,
    asked: Instant,
    reply: mpsc::SyncSender<Reply>,
}

/// What every handle shares; dropping the last one closes the job channel.
struct Inner {
    jobs: mpsc::Sender<Job>,
    broken: Arc<AtomicBool>,
    /// The latest [`KEPT_LOG`] calls: the asking thread's name and its wait ([`SharedEncoder::wait_log`]).
    log: Mutex<VecDeque<(String, Duration)>>,
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
        let (jobs, rx) = mpsc::channel::<Job>();
        let broken = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&broken);
        std::thread::Builder::new()
            .name(THREAD_NAME.to_owned())
            .spawn(move || serve(runner, &rx, &flag))
            .map_err(|_| OnnxError::Runtime("the shared encoder's thread did not start".into()))?;
        Ok(Self {
            inner: Arc::new(Inner {
                jobs,
                broken,
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

    /// Whether a run failed or panicked; a broken encoder answers every call with an error.
    #[must_use]
    pub fn is_broken(&self) -> bool {
        self.inner.broken.load(Ordering::SeqCst)
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
    /// waited before its run started (zero when it never reached the thread).
    pub(crate) fn run(&self, input: EncoderInput) -> Reply {
        let reply = match self.submit(input) {
            // The thread answers every job it takes; one that ended without answering dropped
            // the reply sender, so this never waits on a thread that is gone.
            Ok(answer) => answer
                .recv()
                .unwrap_or_else(|_| (Err(gone_error()), Duration::ZERO)),
            Err(reply) => reply,
        };
        let name = std::thread::current()
            .name()
            .unwrap_or("unnamed")
            .to_owned();
        let mut log = self
            .inner
            .log
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if log.len() == KEPT_LOG {
            log.pop_front();
        }
        log.push_back((name, reply.1));
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
            .cloned()
            .collect()
    }

    /// Queues one call: where its answer will come, or the answer now (broken, or no thread).
    fn submit(&self, input: EncoderInput) -> Result<mpsc::Receiver<Reply>, Reply> {
        if self.is_broken() {
            return Err((Err(broken_error()), Duration::ZERO));
        }
        let (reply, answer) = mpsc::sync_channel(1);
        let job = Job {
            input,
            asked: Instant::now(),
            reply,
        };
        if self.inner.jobs.send(job).is_err() {
            return Err((Err(gone_error()), Duration::ZERO));
        }
        Ok(answer)
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
fn serve(mut runner: impl EncoderRun, jobs: &mpsc::Receiver<Job>, broken: &AtomicBool) {
    while let Ok(job) = jobs.recv() {
        let waited = job.asked.elapsed();
        let result = if broken.load(Ordering::SeqCst) {
            Err(broken_error())
        } else {
            match std::panic::catch_unwind(AssertUnwindSafe(|| runner.run(job.input))) {
                Ok(Ok(out)) => Ok(out),
                Ok(Err(e)) => {
                    broken.store(true, Ordering::SeqCst);
                    Err(e)
                }
                Err(_) => {
                    broken.store(true, Ordering::SeqCst);
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::{Placement, Provider};
    use std::sync::Mutex;

    fn backend() -> SessionBackend {
        SessionBackend {
            requested: Provider::Cpu,
            placement: Placement::default(),
        }
    }

    fn input(tag: usize) -> EncoderInput {
        EncoderInput {
            features: vec![tag as f32],
            n_mels: 1,
            frames: 1,
            valid: 1,
        }
    }

    /// A fake session: records the order of the calls it ran, fails or panics on a chosen call,
    /// and says when it is dropped.
    struct Fake {
        ran: Arc<Mutex<Vec<usize>>>,
        fail_on: Option<usize>,
        panic_on: Option<usize>,
        hold: Option<mpsc::Receiver<()>>,
        dropped: Arc<AtomicBool>,
    }

    impl Fake {
        fn new() -> (Self, Arc<Mutex<Vec<usize>>>, Arc<AtomicBool>) {
            let ran = Arc::new(Mutex::new(Vec::new()));
            let dropped = Arc::new(AtomicBool::new(false));
            let fake = Fake {
                ran: Arc::clone(&ran),
                fail_on: None,
                panic_on: None,
                hold: None,
                dropped: Arc::clone(&dropped),
            };
            (fake, ran, dropped)
        }
    }

    impl Drop for Fake {
        fn drop(&mut self) {
            self.dropped.store(true, Ordering::SeqCst);
        }
    }

    impl EncoderRun for Fake {
        fn run(&mut self, input: EncoderInput) -> Result<EncoderOutput, OnnxError> {
            if let Some(hold) = &self.hold {
                let _ = hold.recv_timeout(Duration::from_secs(10));
            }
            let tag = input.features[0] as usize;
            self.ran.lock().unwrap().push(tag);
            if self.fail_on == Some(tag) {
                return Err(OnnxError::Runtime("fake run failed".into()));
            }
            assert!(self.panic_on != Some(tag), "fake run panicked");
            Ok((vec![tag as f32], 1, 1))
        }
    }

    fn shared(fake: Fake) -> SharedEncoder {
        SharedEncoder::spawn(fake, backend(), BTreeMap::new(), None).unwrap()
    }

    /// Runs `f` on its own thread and fails the test if it does not return within 10 s.
    fn within<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> T {
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(f());
        });
        rx.recv_timeout(Duration::from_secs(10))
            .expect("a shared encoder call hung")
    }

    #[test]
    fn shared_encoder_answers_each_call_with_its_own_output_in_the_order_asked() {
        let (mut fake, ran, _) = Fake::new();
        let (release, hold) = mpsc::channel();
        fake.hold = Some(hold);
        let enc = shared(fake);
        // Five borrowers queue calls 1..=5, in that order, while the thread is held.
        let answers: Vec<_> = (1..=5)
            .map(|tag| enc.clone().submit(input(tag)).ok().unwrap())
            .collect();
        for _ in 1..=5 {
            release.send(()).unwrap();
        }
        let answers = within(move || {
            answers
                .into_iter()
                .map(|a| a.recv().unwrap())
                .collect::<Vec<_>>()
        });
        for (tag, (out, waited)) in (1..=5).zip(answers) {
            assert_eq!(
                out.unwrap().0,
                vec![tag as f32],
                "each borrower gets its own answer"
            );
            assert!(waited <= Duration::from_secs(10));
        }
        assert_eq!(*ran.lock().unwrap(), vec![1, 2, 3, 4, 5]);
        assert!(!enc.is_broken());
    }

    #[test]
    fn shared_encoder_after_a_failed_run_answers_every_call_with_an_error() {
        let (mut fake, ran, _) = Fake::new();
        fake.fail_on = Some(2);
        let enc = shared(fake);
        let e = enc.clone();
        let results = within(move || {
            (1..=4)
                .map(|t| e.run(input(t)).0.is_ok())
                .collect::<Vec<_>>()
        });
        assert_eq!(results, vec![true, false, false, false]);
        assert!(enc.is_broken());
        assert_eq!(
            *ran.lock().unwrap(),
            vec![1, 2],
            "a broken encoder never runs again"
        );
    }

    #[test]
    fn shared_encoder_a_panicking_run_reaches_the_caller_and_every_waiter() {
        let (mut fake, _, _) = Fake::new();
        fake.panic_on = Some(1);
        let (release, hold) = mpsc::channel();
        fake.hold = Some(hold);
        let enc = shared(fake);
        // Three calls queued behind the one that will panic.
        let waiters: Vec<_> = (1..=3)
            .map(|t| enc.submit(input(t)).ok().unwrap())
            .collect();
        for _ in 0..3 {
            let _ = release.send(());
        }
        let errors = within(move || {
            waiters
                .into_iter()
                .map(|a| a.recv().unwrap().0.is_err())
                .collect::<Vec<_>>()
        });
        assert_eq!(errors, vec![true, true, true]);
        assert!(enc.is_broken());
        let e = enc.clone();
        assert!(within(move || e.run(input(9)).0.is_err()));
    }

    #[test]
    fn shared_encoder_thread_ends_and_frees_the_session_with_its_last_handle() {
        let (fake, _, dropped) = Fake::new();
        let enc = shared(fake);
        let weak = enc.downgrade();
        let second = enc.clone();
        assert_eq!(enc.users(), 2);
        drop(enc);
        assert!(weak.upgrade().is_some(), "a borrower still holds it");
        assert!(!dropped.load(Ordering::SeqCst));
        drop(second);
        assert!(weak.upgrade().is_none());
        let deadline = Instant::now() + Duration::from_secs(10);
        while !dropped.load(Ordering::SeqCst) {
            assert!(
                Instant::now() < deadline,
                "the session outlived its last borrower"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn shared_encoder_thread_is_named_with_fixed_text() {
        struct Name(Arc<Mutex<Option<String>>>);
        impl EncoderRun for Name {
            fn run(&mut self, _: EncoderInput) -> Result<EncoderOutput, OnnxError> {
                *self.0.lock().unwrap() = std::thread::current().name().map(str::to_owned);
                Ok((Vec::new(), 0, 0))
            }
        }
        let seen = Arc::new(Mutex::new(None));
        let enc = SharedEncoder::spawn(Name(Arc::clone(&seen)), backend(), BTreeMap::new(), None)
            .unwrap();
        let e = enc.clone();
        within(move || {
            std::thread::Builder::new()
                .name("a-borrower".into())
                .spawn(move || e.run(input(0)).0.unwrap())
                .unwrap()
                .join()
                .unwrap()
        });
        assert_eq!(seen.lock().unwrap().as_deref(), Some(THREAD_NAME));
        let log = enc.wait_log();
        assert_eq!(log.len(), 1);
        assert_eq!(
            log[0].0, "a-borrower",
            "the wait is logged under the asking thread"
        );
        assert_ne!(
            std::thread::current().name(),
            Some(THREAD_NAME),
            "the call ran on the encoder thread, not the caller's"
        );
    }
}
