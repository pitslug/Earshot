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
//! The registry (`earshot_onnx::shared_encoder::Registry`) holds encoders weakly: an encoder is
//! freed after its last borrower lets go, and a broken one (a session run failed or panicked) or a
//! stalled one (a run still going after `earshot_onnx::shared_encoder::STALL_LIMIT`) leaves the
//! registry so the next load makes a new one; a stalled encoder's hung thread and session stay on
//! the card beside the new one until its run returns. A first load always uses
//! [`encoder_config`], whoever asks first (never a preview's background threads). The registry's
//! lock is held across the lookup and the load only, so two first asks give one load. Its holder
//! may already hold a `WorkGate` pass (a meeting's loads, the benchmark's), but while holding the
//! lock it never waits on a pass, the store, `ModelJobs`, `log::LOADS` or the CUDA start's gates:
//! the load takes none of them.
//!
//! Saving memory never costs a meeting: a borrow or load that fails falls back to the model's own
//! encoder ([`on_shared_encoder`]) and says so once per process ([`take_note`]).

use std::path::PathBuf;
use std::sync::Mutex;

use earshot_core::asr::AsrError;
use earshot_onnx::parakeet::SharedEncoder;
use earshot_onnx::shared_encoder::Registry;
use earshot_onnx::{Provider, SessionConfig};

/// Which encoder: one per model and models folder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct EncoderKey {
    pub models: PathBuf,
    pub model: &'static str,
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
pub(crate) static ENCODERS: Registry<EncoderKey, SharedEncoder> = Registry::new(encoder_config);

/// What the user is told, once per process, when a load could not share the encoder. Past tense
/// with no claim about the present: it can be shown at a later meeting than the fallback (a preview
/// or final pass that fell back shows at the next track load).
pub const NOTE: &str = "Earshot could not share one copy of the speech model on the graphics \
                        card this time, so it loaded a copy of its own, which used more graphics \
                        memory.";

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
    use std::sync::atomic::{AtomicUsize, Ordering};

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

    /// Review finding 1: the note can be taken at a later meeting than the fallback, so it says
    /// what happened and claims nothing about now.
    #[test]
    fn shared_encoders_the_note_speaks_of_the_past_only() {
        assert!(NOTE.contains("this time") && NOTE.contains("used more"));
        for present in ["for now", "uses more", " now", "is using", "currently"] {
            assert!(!NOTE.contains(present), "{present:?} in {NOTE:?}");
        }
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
        assert_eq!(
            ENCODERS.configuration(),
            config,
            "the registry loads with it"
        );
    }
}
