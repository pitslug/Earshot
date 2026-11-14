//! One Parakeet TDT encoder on the graphics card for everything that transcribes on CUDA
//! (T12.58).
//!
//! Each CUDA Parakeet session used to hold its own copy of the encoder (2.27 GiB of weights for
//! 0.6B v2). Now `backends::load_parakeet` on CUDA, and so a meeting's two tracks, its first
//! preview session, the final pass, the model check and a meeting recording while the previous one
//! finishes, borrow one encoder per model and models folder ([`ENCODERS`]), each keeping its own
//! prediction network and joint with the threads it had. The encoder is run only by its own
//! thread (`earshot_onnx::shared_encoder`).
//!
//! The registry holds encoders weakly: an encoder is freed with its last borrower, and a broken
//! one (a run failed or panicked) leaves the registry so the next load makes a new one. A first
//! load always uses [`encoder_config`], whoever asks first (never a preview's background
//! threads). The registry's lock is held across the lookup and the load only, so two first asks
//! give one load; it is never taken holding the store, a `WorkGate` pass or any other lock, and
//! the load itself takes none of them.
//!
//! Saving memory never costs a meeting: a borrow or load that fails falls back to the model's own
//! encoder ([`on_shared_encoder`]) and says so once per process ([`take_note`]).

use std::path::PathBuf;
use std::sync::Mutex;

use earshot_core::asr::AsrError;
use earshot_onnx::parakeet::{SharedEncoder, WeakSharedEncoder};
use earshot_onnx::{Provider, SessionConfig};

/// What a registry keeps: a handle it can hold weakly and ask whether it is broken.
pub(crate) trait Borrowable: Clone {
    type Weak: Clone;
    fn downgrade(&self) -> Self::Weak;
    fn upgrade(weak: &Self::Weak) -> Option<Self>;
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

/// Which encoder: one per model and models folder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct EncoderKey {
    pub models: PathBuf,
    pub model: &'static str,
}

/// Encoders by key, held weakly ([`Registry::borrow`]).
pub(crate) struct Registry<H: Borrowable> {
    slots: Mutex<Vec<(EncoderKey, H::Weak)>>,
    config: fn() -> SessionConfig,
}

impl<H: Borrowable> Registry<H> {
    /// An empty registry whose first loads use `config()`.
    pub(crate) const fn new(config: fn() -> SessionConfig) -> Self {
        Self {
            slots: Mutex::new(Vec::new()),
            config,
        }
    }

    /// The live, unbroken encoder of `key`, or one `load`ed now with the registry's fixed
    /// configuration. Gone and broken entries are dropped first, so a broken encoder is replaced.
    ///
    /// # Errors
    /// `load`'s error; nothing is kept then.
    pub(crate) fn borrow<E>(
        &self,
        key: &EncoderKey,
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
        let h = load(&(self.config)())?;
        slots.push((key.clone(), h.downgrade()));
        Ok(h)
    }
}

/// The one configuration every shared encoder is loaded with: CUDA, normal (never background)
/// threads, the two-track live thread count for its CPU nodes (`backends::live_whisper_threads`).
#[must_use]
pub(crate) fn encoder_config() -> SessionConfig {
    let cores = std::thread::available_parallelism().map_or(1, std::num::NonZeroUsize::get);
    SessionConfig {
        provider: Provider::Cuda { device_id: 0 },
        intra_threads: Some(
            crate::backends::live_whisper_threads(earshot_core::asr::BackendKind::Cuda, cores, 2)
                .get(),
        ),
        background: None,
    }
}

/// The process's shared CUDA encoders.
pub(crate) static ENCODERS: Registry<SharedEncoder> = Registry::new(encoder_config);

/// What the user is told, once per process, when a load could not share the encoder.
pub const NOTE: &str = "Earshot could not share one copy of the speech model on the graphics \
                        card, so it loaded a copy of its own and uses more graphics memory for now.";

/// Whether a note waits to be shown, and whether one was.
static NOTE_STATE: Mutex<(bool, bool)> = Mutex::new((false, false));

/// Records a load that fell back from the shared encoder: [`take_note`] answers once.
fn record_fallback() {
    let mut state = NOTE_STATE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if !state.1 {
        state.0 = true;
    }
}

/// [`NOTE`] once per process, after the first load that could not share the encoder.
#[must_use]
pub fn take_note() -> Option<&'static str> {
    let mut state = NOTE_STATE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if state.0 {
        *state = (false, true);
        Some(NOTE)
    } else {
        None
    }
}

/// A model on the shared encoder (`shared`), or when that fails its own load (`own`). Only an own
/// load that works after a failed share is recorded for the note ([`take_note`]): when both fail,
/// the own load's error is the one reported (CUDA itself refused, say), with no word about sharing.
/// The shared attempt's error text is dropped: it can name paths.
///
/// # Errors
/// `own`'s error.
pub(crate) fn on_shared_encoder<T>(
    shared: impl FnOnce() -> Result<T, String>,
    own: impl FnOnce() -> Result<T, AsrError>,
) -> Result<T, AsrError> {
    on_shared_encoder_with(shared, own, record_fallback)
}

fn on_shared_encoder_with<T>(
    shared: impl FnOnce() -> Result<T, String>,
    own: impl FnOnce() -> Result<T, AsrError>,
    fell_back: impl FnOnce(),
) -> Result<T, AsrError> {
    match shared() {
        Ok(t) => Ok(t),
        Err(_) => {
            let t = own()?;
            fell_back();
            Ok(t)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{Arc, Weak};

    /// A fake encoder: an id, a broken flag and the configuration it was loaded with.
    #[derive(Clone)]
    struct Fake(Arc<(usize, AtomicBool, SessionConfig)>);

    impl Borrowable for Fake {
        type Weak = Weak<(usize, AtomicBool, SessionConfig)>;
        fn downgrade(&self) -> Self::Weak {
            Arc::downgrade(&self.0)
        }
        fn upgrade(weak: &Self::Weak) -> Option<Self> {
            weak.upgrade().map(Fake)
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

    fn key(model: &'static str) -> EncoderKey {
        EncoderKey {
            models: PathBuf::from("models"),
            model,
        }
    }

    /// A loader that counts its loads and numbers the encoders it makes.
    fn counting(loads: &AtomicUsize) -> impl FnOnce(&SessionConfig) -> Result<Fake, String> + '_ {
        move |config| {
            let n = loads.fetch_add(1, Ordering::SeqCst);
            Ok(Fake(Arc::new((n, AtomicBool::new(false), config.clone()))))
        }
    }

    #[test]
    fn shared_encoders_two_asks_give_one_load_with_the_fixed_configuration() {
        let registry: Registry<Fake> = Registry::new(fixed);
        let loads = AtomicUsize::new(0);
        let a = registry.borrow(&key("m"), counting(&loads)).unwrap();
        let b = registry.borrow(&key("m"), counting(&loads)).unwrap();
        assert_eq!(loads.load(Ordering::SeqCst), 1);
        assert!(Arc::ptr_eq(&a.0, &b.0));
        assert_eq!(a.0.2, fixed(), "loaded with the registry's configuration");
        // Another model, or the same model in another folder, is another encoder.
        let other = registry.borrow(&key("n"), counting(&loads)).unwrap();
        assert_eq!(other.0.0, 1);
        let elsewhere = registry
            .borrow(
                &EncoderKey {
                    models: PathBuf::from("other"),
                    model: "m",
                },
                counting(&loads),
            )
            .unwrap();
        assert_eq!(elsewhere.0.0, 2);
    }

    #[test]
    fn shared_encoders_two_first_asks_at_once_give_one_load() {
        let registry: Arc<Registry<Fake>> = Arc::new(Registry::new(fixed));
        let loads = Arc::new(AtomicUsize::new(0));
        let handles: Vec<_> = (0..8)
            .map(|_| {
                let (registry, loads) = (Arc::clone(&registry), Arc::clone(&loads));
                std::thread::spawn(move || {
                    registry
                        .borrow(&key("m"), |config| {
                            std::thread::sleep(std::time::Duration::from_millis(20));
                            counting(&loads)(config)
                        })
                        .unwrap()
                })
            })
            .collect();
        let got: Vec<Fake> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        assert_eq!(loads.load(Ordering::SeqCst), 1);
        assert!(got.iter().all(|f| Arc::ptr_eq(&f.0, &got[0].0)));
    }

    #[test]
    fn shared_encoders_an_encoder_is_freed_with_its_last_borrower() {
        let registry: Registry<Fake> = Registry::new(fixed);
        let loads = AtomicUsize::new(0);
        let a = registry.borrow(&key("m"), counting(&loads)).unwrap();
        let weak = Arc::downgrade(&a.0);
        drop(a);
        assert!(weak.upgrade().is_none(), "the registry holds it weakly");
        let b = registry.borrow(&key("m"), counting(&loads)).unwrap();
        assert_eq!(
            (loads.load(Ordering::SeqCst), b.0.0),
            (2, 1),
            "loaded again"
        );
    }

    /// A broken encoder (a run failed mid-meeting) leaves the registry: its borrowers keep their
    /// errors, and the next load (a final pass) gets a fresh encoder.
    #[test]
    fn shared_encoders_a_broken_encoder_is_replaced_for_the_next_load() {
        let registry: Registry<Fake> = Registry::new(fixed);
        let loads = AtomicUsize::new(0);
        let track = registry.borrow(&key("m"), counting(&loads)).unwrap();
        let preview = registry.borrow(&key("m"), counting(&loads)).unwrap();
        track.0.1.store(true, Ordering::SeqCst);
        assert!(preview.is_broken(), "every borrower of it sees it broken");
        let final_pass = registry.borrow(&key("m"), counting(&loads)).unwrap();
        assert_eq!(loads.load(Ordering::SeqCst), 2);
        assert!(!final_pass.is_broken());
        assert!(!Arc::ptr_eq(&final_pass.0, &track.0));
        let again = registry.borrow(&key("m"), counting(&loads)).unwrap();
        assert!(
            Arc::ptr_eq(&again.0, &final_pass.0),
            "the fresh one is shared"
        );
    }

    #[test]
    fn shared_encoders_a_failed_load_keeps_nothing() {
        let registry: Registry<Fake> = Registry::new(fixed);
        let loads = AtomicUsize::new(0);
        assert!(
            registry
                .borrow(&key("m"), |_| Err::<Fake, _>("no card".to_owned()))
                .is_err()
        );
        let a = registry.borrow(&key("m"), counting(&loads)).unwrap();
        assert_eq!((loads.load(Ordering::SeqCst), a.0.0), (1, 0));
    }

    #[test]
    fn shared_encoders_a_failed_borrow_falls_back_to_an_own_load_and_says_so() {
        let noted = AtomicUsize::new(0);
        let note = || {
            noted.fetch_add(1, Ordering::SeqCst);
        };
        let got = on_shared_encoder_with(|| Err::<&str, _>("refused".into()), || Ok("own"), note);
        assert_eq!(got.unwrap(), "own");
        assert_eq!(noted.load(Ordering::SeqCst), 1);
        // A borrow that works takes no own load and says nothing.
        let got = on_shared_encoder_with(
            || Ok("shared"),
            || -> Result<&str, AsrError> { panic!("no own load") },
            || panic!("no note"),
        );
        assert_eq!(got.unwrap(), "shared");
        // When the own load fails too, its error is the one reported, and sharing is not named.
        let got = on_shared_encoder_with(
            || Err::<&str, _>("refused".into()),
            || Err(AsrError::Backend("own failed".into())),
            || panic!("no note when nothing loaded"),
        );
        assert!(matches!(got, Err(AsrError::Backend(m)) if m == "own failed"));
    }

    #[test]
    fn shared_encoders_the_note_is_said_once_per_process() {
        // The process-wide state: other tests in this binary do not record fallbacks.
        record_fallback();
        record_fallback();
        assert_eq!(take_note(), Some(NOTE));
        assert_eq!(take_note(), None);
        record_fallback();
        assert_eq!(take_note(), None, "once per process");
    }

    #[test]
    fn shared_encoders_load_on_cuda_with_normal_threads_and_the_live_budget() {
        let config = encoder_config();
        assert_eq!(config.provider, Provider::Cuda { device_id: 0 });
        assert_eq!(
            config.background, None,
            "never a preview's background threads"
        );
        let cores = std::thread::available_parallelism().map_or(1, std::num::NonZeroUsize::get);
        assert_eq!(
            config.intra_threads,
            Some(
                crate::backends::live_whisper_threads(
                    earshot_core::asr::BackendKind::Cuda,
                    cores,
                    2
                )
                .get()
            )
        );
        assert!(std::ptr::fn_addr_eq(
            ENCODERS.config,
            encoder_config as fn() -> SessionConfig
        ));
    }
}
