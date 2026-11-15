use super::*;
use crate::runtime::{Placement, Provider};
use std::sync::atomic::{AtomicU64, AtomicUsize};

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

/// A fake session: records the order of the calls it ran, fails (in its input or its run) or
/// panics on a chosen call, and says when it is dropped.
struct Fake {
    ran: Arc<Mutex<Vec<usize>>>,
    fail_on: Option<usize>,
    input_error_on: Option<usize>,
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
            input_error_on: None,
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
    fn run(&mut self, input: EncoderInput) -> Result<EncoderOutput, RunError> {
        if let Some(hold) = &self.hold {
            let _ = hold.recv_timeout(Duration::from_secs(10));
        }
        let tag = input.features[0] as usize;
        if self.input_error_on == Some(tag) {
            return Err(RunError::Input(OnnxError::Runtime("fake tensor".into())));
        }
        self.ran.lock().unwrap().push(tag);
        if self.fail_on == Some(tag) {
            return Err(RunError::Session(OnnxError::Runtime(
                "fake run failed".into(),
            )));
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

/// Review finding 3: only a failed session run breaks the encoder. An input of the wrong shape
/// (refused on the caller's side) or a tensor that cannot be built is that caller's error alone.
#[test]
fn shared_encoder_a_bad_input_is_the_callers_error_and_leaves_it_usable() {
    let (mut fake, ran, _) = Fake::new();
    fake.input_error_on = Some(2);
    let enc = shared(fake);
    let (bad, other) = (enc.clone(), enc.clone());
    let wrong_shape = EncoderInput {
        features: vec![0.0; 3],
        n_mels: 2,
        frames: 2,
        valid: 2,
    };
    assert!(wrong_shape.check().is_err());
    let too_many_valid = EncoderInput {
        valid: 2,
        ..input(1)
    };
    assert!(too_many_valid.check().is_err());
    let results = within(move || {
        vec![
            bad.run(wrong_shape).0.is_ok(),
            bad.run(too_many_valid).0.is_ok(),
            // A tensor the runner cannot build: an input error from the thread.
            bad.run(input(2)).0.is_ok(),
            other.run(input(1)).0.is_ok(),
            other.run(input(3)).0.is_ok(),
        ]
    });
    assert_eq!(results, vec![false, false, false, true, true]);
    assert!(!enc.is_broken(), "nothing touched the session");
    assert_eq!(*ran.lock().unwrap(), vec![1, 3], "the bad inputs never ran");
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
fn shared_encoder_thread_ends_and_frees_the_session_after_its_last_handle() {
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
        fn run(&mut self, _: EncoderInput) -> Result<EncoderOutput, RunError> {
            *self.0.lock().unwrap() = std::thread::current().name().map(str::to_owned);
            Ok((Vec::new(), 0, 0))
        }
    }
    let seen = Arc::new(Mutex::new(None));
    let enc =
        SharedEncoder::spawn(Name(Arc::clone(&seen)), backend(), BTreeMap::new(), None).unwrap();
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

/// A clock the test moves by hand: a fixed start plus an offset.
#[derive(Clone)]
struct HandClock {
    start: Instant,
    offset_ms: Arc<AtomicU64>,
}

impl HandClock {
    fn new() -> Self {
        Self {
            start: Instant::now(),
            offset_ms: Arc::new(AtomicU64::new(0)),
        }
    }

    fn clock(&self) -> Clock {
        let me = self.clone();
        Arc::new(move || me.start + Duration::from_millis(me.offset_ms.load(Ordering::SeqCst)))
    }

    fn advance(&self, by: Duration) {
        self.offset_ms
            .fetch_add(u64::try_from(by.as_millis()).unwrap(), Ordering::SeqCst);
    }
}

/// A fake session whose run of a tag listed in `hang` never returns until released, saying when
/// it started; other runs answer at once.
struct Hang {
    started: mpsc::Sender<usize>,
    release: mpsc::Receiver<()>,
    hang: Vec<usize>,
}

impl EncoderRun for Hang {
    fn run(&mut self, input: EncoderInput) -> Result<EncoderOutput, RunError> {
        let tag = input.features[0] as usize;
        let _ = self.started.send(tag);
        if self.hang.contains(&tag) {
            // Released only at the end of the test, so the thread can end.
            let _ = self.release.recv();
        }
        Ok((vec![tag as f32], 1, 1))
    }
}

/// A shared encoder on `clock` whose call 1 hangs: (encoder, its "started" receiver, release).
fn hanging(clock: &HandClock) -> (SharedEncoder, mpsc::Receiver<usize>, mpsc::Sender<()>) {
    let (started, started_rx) = mpsc::channel();
    let (release, release_rx) = mpsc::channel();
    let enc = SharedEncoder::spawn_with_clock(
        Hang {
            started,
            release: release_rx,
            hang: vec![1],
        },
        backend(),
        BTreeMap::new(),
        None,
        clock.clock(),
    )
    .unwrap();
    (enc, started_rx, release)
}

/// Starts call 1 (the hanging one) on its own thread and waits until its run has begun.
fn start_hung_call(
    enc: &SharedEncoder,
    started: &mpsc::Receiver<usize>,
) -> mpsc::Receiver<Result<EncoderOutput, OnnxError>> {
    let (tx, rx) = mpsc::channel();
    let e = enc.clone();
    std::thread::spawn(move || {
        let _ = tx.send(e.run(input(1)).0);
    });
    assert_eq!(
        started.recv_timeout(Duration::from_secs(10)).unwrap(),
        1,
        "the hung run started"
    );
    rx
}

/// Review finding 2 (b): a caller queued behind a run that has gone past the limit gets an error,
/// not a hang; so does the hung call's own caller, and a later call.
#[test]
fn shared_encoder_a_caller_behind_a_hung_run_gets_an_error_after_the_limit() {
    let clock = HandClock::new();
    let (enc, started, release) = hanging(&clock);
    let hung = start_hung_call(&enc, &started);
    let e = enc.clone();
    let (tx, queued) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(e.run(input(2)).0);
    });
    std::thread::sleep(STALL_POLL * 3);
    assert!(queued.try_recv().is_err(), "still waiting below the limit");
    clock.advance(STALL_LIMIT + Duration::from_secs(1));
    let behind = queued
        .recv_timeout(Duration::from_secs(10))
        .expect("a caller behind a hung run waited for ever");
    assert!(matches!(behind, Err(OnnxError::Runtime(m)) if m.contains("stopped answering")));
    assert!(
        hung.recv_timeout(Duration::from_secs(10))
            .expect("the hung call's caller waited for ever")
            .is_err()
    );
    assert!(enc.is_broken());
    let e = enc.clone();
    assert!(within(move || e.run(input(3)).0.is_err()), "a later call");
    let _ = release.send(());
}

/// Review finding 2 (a) and (c): below the limit a long run replaces nothing and its waiters
/// keep waiting; past it the registry's next borrow gets a different encoder that works.
#[test]
fn shared_encoder_registry_replaces_a_hung_encoder_only_after_the_limit() {
    let clock = HandClock::new();
    let registry: Registry<&str, SharedEncoder> = Registry::new(SessionConfig::default);
    let loads = AtomicUsize::new(0);
    let releases = Mutex::new(Vec::new());
    let starts = Mutex::new(Vec::new());
    let load = |_: &SessionConfig| -> Result<SharedEncoder, String> {
        loads.fetch_add(1, Ordering::SeqCst);
        let (enc, started, release) = hanging(&clock);
        releases.lock().unwrap().push(release);
        starts.lock().unwrap().push(started);
        Ok(enc)
    };
    let first = registry.borrow(&"m", load).unwrap();
    let hung = {
        let starts = starts.lock().unwrap();
        start_hung_call(&first, &starts[0])
    };

    // (c) Below the limit: the same encoder, not broken, its caller still waiting.
    clock.advance(STALL_LIMIT - Duration::from_secs(1));
    let again = registry.borrow(&"m", load).unwrap();
    assert_eq!(loads.load(Ordering::SeqCst), 1);
    assert!(Arc::ptr_eq(&again.inner, &first.inner));
    assert!(!first.is_broken());
    std::thread::sleep(STALL_POLL * 3);
    assert!(hung.try_recv().is_err(), "a long run is waited for");

    // (a) Past the limit: the next borrow gets a fresh encoder, and it answers.
    clock.advance(Duration::from_secs(2));
    let fresh = registry.borrow(&"m", load).unwrap();
    assert_eq!(loads.load(Ordering::SeqCst), 2);
    assert!(!Arc::ptr_eq(&fresh.inner, &first.inner));
    assert!(first.is_broken() && !fresh.is_broken());
    let f = fresh.clone();
    assert_eq!(within(move || f.run(input(5)).0.unwrap()).0, vec![5.0]);
    assert!(hung.recv_timeout(Duration::from_secs(10)).unwrap().is_err());
    for release in releases.lock().unwrap().iter() {
        let _ = release.send(());
    }
}

/// A fake registry entry: an id, a broken flag and the configuration it was loaded with.
#[derive(Clone)]
struct Entry(Arc<(usize, AtomicBool, SessionConfig)>);

impl Borrowable for Entry {
    type Weak = Weak<(usize, AtomicBool, SessionConfig)>;
    fn downgrade(&self) -> Self::Weak {
        Arc::downgrade(&self.0)
    }
    fn upgrade(weak: &Self::Weak) -> Option<Self> {
        weak.upgrade().map(Entry)
    }
    fn is_broken(&self) -> bool {
        self.0.1.load(Ordering::SeqCst)
    }
}

fn fixed() -> SessionConfig {
    SessionConfig {
        provider: Provider::Cpu,
        intra_threads: Some(7),
        background: None,
    }
}

/// A loader that counts its loads and numbers the entries it makes.
fn counting(loads: &AtomicUsize) -> impl FnOnce(&SessionConfig) -> Result<Entry, String> + '_ {
    move |config| {
        let n = loads.fetch_add(1, Ordering::SeqCst);
        Ok(Entry(Arc::new((n, AtomicBool::new(false), config.clone()))))
    }
}

#[test]
fn shared_encoder_registry_two_asks_give_one_load_with_the_fixed_configuration() {
    let registry: Registry<(&str, &str), Entry> = Registry::new(fixed);
    let loads = AtomicUsize::new(0);
    let a = registry.borrow(&("models", "m"), counting(&loads)).unwrap();
    let b = registry.borrow(&("models", "m"), counting(&loads)).unwrap();
    assert_eq!(loads.load(Ordering::SeqCst), 1);
    assert!(Arc::ptr_eq(&a.0, &b.0));
    assert_eq!(a.0.2, fixed(), "loaded with the registry's configuration");
    assert_eq!(registry.configuration(), fixed());
    // Another model, or the same model in another folder, is another encoder.
    let other = registry.borrow(&("models", "n"), counting(&loads)).unwrap();
    assert_eq!(other.0.0, 1);
    let elsewhere = registry.borrow(&("other", "m"), counting(&loads)).unwrap();
    assert_eq!(elsewhere.0.0, 2);
}

#[test]
fn shared_encoder_registry_two_first_asks_at_once_give_one_load() {
    let registry: Arc<Registry<&str, Entry>> = Arc::new(Registry::new(fixed));
    let loads = Arc::new(AtomicUsize::new(0));
    let handles: Vec<_> = (0..8)
        .map(|_| {
            let (registry, loads) = (Arc::clone(&registry), Arc::clone(&loads));
            std::thread::spawn(move || {
                registry
                    .borrow(&"m", |config| {
                        std::thread::sleep(Duration::from_millis(20));
                        counting(&loads)(config)
                    })
                    .unwrap()
            })
        })
        .collect();
    let got: Vec<Entry> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    assert_eq!(loads.load(Ordering::SeqCst), 1);
    assert!(got.iter().all(|f| Arc::ptr_eq(&f.0, &got[0].0)));
}

#[test]
fn shared_encoder_registry_frees_an_encoder_with_its_last_borrower() {
    let registry: Registry<&str, Entry> = Registry::new(fixed);
    let loads = AtomicUsize::new(0);
    let a = registry.borrow(&"m", counting(&loads)).unwrap();
    let weak = Arc::downgrade(&a.0);
    drop(a);
    assert!(weak.upgrade().is_none(), "the registry holds it weakly");
    let b = registry.borrow(&"m", counting(&loads)).unwrap();
    assert_eq!(
        (loads.load(Ordering::SeqCst), b.0.0),
        (2, 1),
        "loaded again"
    );
}

/// A broken encoder (a run failed mid-meeting) leaves the registry: its borrowers keep their
/// errors, and the next load (a final pass) gets a fresh encoder.
#[test]
fn shared_encoder_registry_replaces_a_broken_encoder_for_the_next_load() {
    let registry: Registry<&str, Entry> = Registry::new(fixed);
    let loads = AtomicUsize::new(0);
    let track = registry.borrow(&"m", counting(&loads)).unwrap();
    let preview = registry.borrow(&"m", counting(&loads)).unwrap();
    track.0.1.store(true, Ordering::SeqCst);
    assert!(preview.is_broken(), "every borrower of it sees it broken");
    let final_pass = registry.borrow(&"m", counting(&loads)).unwrap();
    assert_eq!(loads.load(Ordering::SeqCst), 2);
    assert!(!final_pass.is_broken());
    assert!(!Arc::ptr_eq(&final_pass.0, &track.0));
    let again = registry.borrow(&"m", counting(&loads)).unwrap();
    assert!(
        Arc::ptr_eq(&again.0, &final_pass.0),
        "the fresh one is shared"
    );
}

#[test]
fn shared_encoder_registry_keeps_nothing_after_a_failed_load() {
    let registry: Registry<&str, Entry> = Registry::new(fixed);
    let loads = AtomicUsize::new(0);
    assert!(
        registry
            .borrow(&"m", |_| Err::<Entry, _>("no card".to_owned()))
            .is_err()
    );
    let a = registry.borrow(&"m", counting(&loads)).unwrap();
    assert_eq!((loads.load(Ordering::SeqCst), a.0.0), (1, 0));
}
