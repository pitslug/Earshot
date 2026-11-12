//! The audio check on the Audio page (T7.4j): plays a few short chirps through the output
//! and listens with the microphone (`earshot_runtime::audio_check`), then shows the delay,
//! the echo level, headphones, too quiet and clipping in plain words, with the date. The
//! result is stored per microphone + output pair and seeds the echo canceller's delay
//! search at the next meeting on that pair.
//!
//! The check runs on a thread of its own ([`AudioCheckJobs`]) holding the
//! [`crate::activity::AudioCheckPass`] for its whole life, so it never overlaps a meeting,
//! the model check or the voice sample. It sends [`AudioCheckEvent`]s: `Listening` progress,
//! then exactly one of `Done`, `Cancelled` or `Failed` (a panic in the work becomes
//! `Failed`). The audio is held in memory on that thread only and dropped there. Tests drive
//! it through [`AudioCheckWork`] fakes over replay sources, never the speakers.

use std::panic::AssertUnwindSafe;
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::JoinHandle;
use std::time::Duration;

use earshot_core::aec::delay::DelayConfig;
use earshot_core::asr::CancelToken;
use earshot_core::audio_check::test_signal_duration;
use earshot_runtime::audio_check::{
    self as check, AFTER_PLAYBACK, Measured, StoredCheck, StoredOutcome,
};
use earshot_runtime::audio_settings::{AudioSettings, DeviceSetting};
use earshot_runtime::{RuntimeError, Services};
use serde::Serialize;
use specta::Type;
use tauri_specta::Event;

use crate::activity::WorkGate;
use crate::audio_settings::{DeviceItem, Devices};
use crate::ipc::IpcError;

/// What a stored check found, for the page.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Type)]
pub enum AudioCheckOutcome {
    /// The microphone heard the speakers.
    Echo,
    /// The sounds were played but not heard: probably headphones, or the speaker volume is
    /// off.
    Headphones,
    /// No sound reached the output.
    NotPlayed,
    /// The microphone sent only silence.
    MicSilent,
}

/// One stored check as the page shows it.
#[derive(Debug, Clone, PartialEq, Serialize, Type)]
pub struct AudioCheckResultView {
    /// The microphone checked.
    pub microphone: String,
    /// The output checked.
    pub output: String,
    /// When it ran (Unix ms).
    pub checked_at_ms: u64,
    /// What it found.
    pub outcome: AudioCheckOutcome,
    /// The speaker-to-microphone delay (ms), when an echo was heard.
    pub delay_ms: Option<u32>,
    /// How much quieter the echo is than the sound played (dB, positive), when heard.
    pub echo_quieter_db: Option<u32>,
    /// The microphone or the echo is too quiet.
    pub too_quiet: bool,
    /// The microphone clips.
    pub clipping: bool,
    /// Meetings on this pair start the echo canceller from the measured delay.
    pub delay_used: bool,
    /// The result in one sentence.
    pub headline: String,
    /// Further lines (delay, echo level, warnings), in order.
    pub details: Vec<String>,
}

/// The audio check part of the Audio page.
#[derive(Debug, Clone, PartialEq, Serialize, Type)]
pub struct AudioCheckView {
    /// The last check of the microphone and output a meeting would use now (`null`: never
    /// checked, or the devices could not be listed).
    pub result: Option<AudioCheckResultView>,
    /// Stored results could not be read by this version (they are replaced by the next
    /// check).
    pub unreadable: bool,
}

/// Progress and the end of an audio check.
#[derive(Debug, Clone, PartialEq, Serialize, Type, Event)]
#[serde(tag = "type")]
pub enum AudioCheckEvent {
    /// Playing and listening: this much heard so far (ms), of about `total_ms`.
    Listening {
        /// Heard so far.
        heard_ms: u64,
        /// The check's usual length.
        total_ms: u64,
    },
    /// Finished (terminal): the result, stored.
    Done {
        /// The page's audio check part now.
        view: AudioCheckView,
        /// A note about the devices used (a chosen one was not connected), if any.
        note: Option<String>,
    },
    /// Cancelled; nothing was stored (terminal).
    Cancelled,
    /// Failed; nothing was stored (terminal).
    Failed {
        /// Why, in the page's words.
        message: String,
    },
}

/// The page's words for a stored check (`max_delay`: the canceller's search range).
#[must_use]
pub fn result_view(c: &StoredCheck, max_delay: usize) -> AudioCheckResultView {
    let delay_used = c.delay_hint(max_delay).is_some();
    let (outcome, headline) = match c.outcome {
        StoredOutcome::Echo { .. } => (
            AudioCheckOutcome::Echo,
            "Your microphone hears your speakers.".to_owned(),
        ),
        StoredOutcome::NoEcho => (
            AudioCheckOutcome::Headphones,
            "Your microphone did not hear the sounds: you are probably using headphones, or \
             the speaker volume is off."
                .to_owned(),
        ),
        StoredOutcome::NotPlayed => (
            AudioCheckOutcome::NotPlayed,
            "No sound reached the output. Check that it is not muted, then run the check again."
                .to_owned(),
        ),
        StoredOutcome::MicSilent => (
            AudioCheckOutcome::MicSilent,
            "Your microphone sent only silence. Check that it is not muted or switched off, then \
             run the check again."
                .to_owned(),
        ),
    };
    let (delay_ms, echo_quieter_db) = match c.outcome {
        StoredOutcome::Echo { echo_db, .. } => (
            c.delay_ms().map(|d| d.round().max(0.0) as u32),
            Some((-echo_db).round().max(0.0) as u32),
        ),
        _ => (None, None),
    };
    let mut details = Vec::new();
    if let (Some(d), Some(q)) = (delay_ms, echo_quieter_db) {
        details.push(format!("Delay from speaker to microphone: {d} ms."));
        details.push(format!("Echo level: {q} dB quieter than the sound played."));
    }
    if matches!(c.outcome, StoredOutcome::Echo { .. }) && c.too_quiet {
        details.push(
            "Too quiet: the microphone heard the sounds only faintly, so the delay is not used."
                .to_owned(),
        );
    } else if matches!(c.outcome, StoredOutcome::Echo { .. }) && !delay_used {
        let rate = u64::from(DelayConfig::default().rate_hz.max(1));
        details.push(format!(
            "The delay is longer than the echo canceller searches ({} ms), so it is not used.",
            max_delay as u64 * 1000 / rate
        ));
    }
    if c.clipping {
        details.push(
            "Clipping: the microphone is too loud and distorts. Lower its input level in the \
             Windows sound settings."
                .to_owned(),
        );
    }
    if delay_used {
        details.push(
            "Meetings with this microphone and output use the delay to start cancelling echo \
             sooner."
                .to_owned(),
        );
    }
    AudioCheckResultView {
        microphone: c.microphone.name.clone(),
        output: c.output.name.clone(),
        checked_at_ms: c.checked_at_ms,
        outcome,
        delay_ms,
        echo_quieter_db,
        too_quiet: c.too_quiet,
        clipping: c.clipping,
        delay_used,
        headline,
        details,
    }
}

/// The canceller's delay search range (samples at 16 kHz).
fn max_delay() -> usize {
    DelayConfig::default().max_delay
}

/// The endpoint a meeting would open for `setting` among `listed`: the chosen one when it
/// is listed, else the Windows default (a meeting replaces a missing chosen device by it).
fn opened<'a>(listed: &'a [DeviceItem], setting: &DeviceSetting) -> Option<&'a DeviceItem> {
    setting
        .id()
        .and_then(|id| listed.iter().find(|d| d.id == id))
        .or_else(|| listed.iter().find(|d| d.is_default))
}

/// The check part of the page: the stored result of the pair `settings` opens among
/// `devices` (none when they could not be listed).
///
/// # Errors
/// Store errors.
pub fn check_view(
    services: &Services,
    settings: &AudioSettings,
    devices: Result<&Devices, &String>,
) -> Result<AudioCheckView, IpcError> {
    let stored = check::stored_checks(&services.store())?;
    let result = devices.ok().and_then(|d| {
        let mic = opened(&d.microphones, &settings.microphone)?;
        let out = opened(&d.outputs, &settings.output)?;
        stored
            .pair(&mic.id, &out.id)
            .map(|c| result_view(c, max_delay()))
    });
    Ok(AudioCheckView {
        result,
        unreadable: stored.unreadable,
    })
}

/// The view right after a check: the stored result of the pair it measured.
fn done_view(services: &Services, m: &Measured) -> Result<AudioCheckView, RuntimeError> {
    let stored = check::stored_checks(&services.store())?;
    Ok(AudioCheckView {
        result: stored
            .pair(&m.microphone.id, &m.output.id)
            .map(|c| result_view(c, max_delay())),
        unreadable: false,
    })
}

/// The note for chosen devices that were not connected (the default was checked).
fn fallback_note(m: &Measured) -> Option<String> {
    #[cfg(windows)]
    {
        use earshot_core::capture::TrackId;
        let mic = m.fallbacks.iter().any(|f| f.track == TrackId::Mic);
        let out = m.fallbacks.iter().any(|f| f.track == TrackId::Reference);
        let what = match (mic, out) {
            (true, true) => "microphone and output were",
            (true, false) => "microphone was",
            (false, true) => "output was",
            (false, false) => return None,
        };
        Some(format!(
            "The chosen {what} not connected, so the Windows default was checked."
        ))
    }
    #[cfg(not(windows))]
    {
        let _ = m;
        None
    }
}

/// The work of an audio check (the speakers and microphone, or fakes in tests).
pub trait AudioCheckWork: Send + Sync {
    /// Runs the check on the devices of `settings`; `None` when cancelled. Runs on the check
    /// thread.
    ///
    /// # Errors
    /// Capture and playback errors.
    fn run(
        &self,
        settings: &AudioSettings,
        cancel: &CancelToken,
        progress: &mut dyn FnMut(Duration),
    ) -> Result<Option<Measured>, RuntimeError>;
}

/// Where audio check events go.
pub type AudioCheckSink = Arc<dyn Fn(AudioCheckEvent) + Send + Sync>;

struct Running {
    cancel: CancelToken,
    thread: JoinHandle<()>,
}

struct Inner {
    work: Arc<dyn AudioCheckWork>,
    sink: AudioCheckSink,
    gate: WorkGate,
    slot: Mutex<Option<Running>>,
}

/// Runs at most one audio check (cheap to clone; clones share it).
#[derive(Clone)]
pub struct AudioCheckJobs(Arc<Inner>);

/// How often progress is reported (of heard audio, not wall time).
pub const PROGRESS_STEP: Duration = Duration::from_millis(250);

/// What a check that panicked reports.
pub const AUDIO_CHECK_PANIC_MESSAGE: &str = "The audio check stopped unexpectedly.";

/// The check's usual length (the sounds plus the listening after them).
#[must_use]
pub fn check_length() -> Duration {
    test_signal_duration() + AFTER_PLAYBACK
}

impl AudioCheckJobs {
    /// Jobs of `work`, sending events to `sink`; a check holds an audio check pass of `gate`.
    pub fn new(work: Arc<dyn AudioCheckWork>, sink: AudioCheckSink, gate: WorkGate) -> Self {
        Self(Arc::new(Inner {
            work,
            sink,
            gate,
            slot: Mutex::new(None),
        }))
    }

    fn slot(&self) -> std::sync::MutexGuard<'_, Option<Running>> {
        self.0.slot.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Starts a check (events follow).
    ///
    /// # Errors
    /// `busy` (retryable) while a check, a meeting, the model check or the voice sample
    /// runs.
    pub fn start(&self, services: &Services) -> Result<(), IpcError> {
        let mut slot = self.slot();
        if let Some(old) = slot.take() {
            if !old.thread.is_finished() {
                *slot = Some(old);
                return Err(IpcError {
                    kind: "busy".into(),
                    message: "The audio check is already running.".into(),
                    retryable: true,
                });
            }
            let _ = old.thread.join();
        }
        let pass = self.0.gate.enter_audio_check()?;
        let cancel = CancelToken::new();
        let inner = Arc::clone(&self.0);
        let svc = services.clone();
        let token = cancel.clone();
        let thread = std::thread::Builder::new()
            .name("earshot-audio-check".into())
            .spawn(move || {
                let event =
                    std::panic::catch_unwind(AssertUnwindSafe(|| run(&inner, &svc, &token)))
                        .unwrap_or_else(|_| AudioCheckEvent::Failed {
                            message: AUDIO_CHECK_PANIC_MESSAGE.into(),
                        });
                drop(pass);
                (inner.sink)(event);
            })
            .map_err(|e| IpcError {
                kind: "internal".into(),
                message: format!("cannot start the audio check thread: {e}"),
                retryable: false,
            })?;
        *slot = Some(Running { cancel, thread });
        Ok(())
    }

    /// Cancels the running check (a `Cancelled` follows unless it already ended).
    pub fn cancel(&self) {
        if let Some(r) = self.slot().as_ref() {
            r.cancel.cancel();
        }
    }

    /// Waits for the running check.
    #[cfg(test)]
    pub fn join(&self) {
        if let Some(r) = self.slot().take() {
            let _ = r.thread.join();
        }
    }
}

/// The check, from the settings to the terminal event (returned, sent by the caller after
/// the pass is released).
fn run(inner: &Inner, svc: &Services, cancel: &CancelToken) -> AudioCheckEvent {
    let settings = match earshot_runtime::audio_settings::audio_settings(&svc.store()) {
        Ok((s, _)) => s,
        Err(e) => {
            return AudioCheckEvent::Failed {
                message: failure_copy(&e),
            };
        }
    };
    let total_ms = u64::try_from(check_length().as_millis()).unwrap_or(u64::MAX);
    let step = u64::try_from(PROGRESS_STEP.as_millis()).unwrap_or(1).max(1);
    let mut next = 0u64;
    let sink = Arc::clone(&inner.sink);
    let mut progress = |d: Duration| {
        let ms = u64::try_from(d.as_millis()).unwrap_or(u64::MAX);
        if ms >= next {
            sink(AudioCheckEvent::Listening {
                heard_ms: ms.min(total_ms),
                total_ms,
            });
            next = (ms / step + 1) * step;
        }
    };
    let measured = match inner.work.run(&settings, cancel, &mut progress) {
        Ok(Some(m)) => m,
        Ok(None) => return AudioCheckEvent::Cancelled,
        Err(e) => {
            return AudioCheckEvent::Failed {
                message: failure_copy(&e),
            };
        }
    };
    if cancel.is_cancelled() {
        return AudioCheckEvent::Cancelled;
    }
    let at = svc.clock.now_unix_ms();
    // The store lock is released before the view reads it again.
    let recorded = check::record_check(&mut svc.store(), measured.stored(at));
    let stored = recorded.and_then(|()| done_view(svc, &measured));
    match stored {
        Ok(view) => AudioCheckEvent::Done {
            view,
            note: fallback_note(&measured),
        },
        Err(e) => AudioCheckEvent::Failed {
            message: failure_copy(&e),
        },
    }
}

/// The store could not be read or written.
pub const STORE_FAILED: &str =
    "Earshot could not save the audio check to its database. Run the check again.";
/// A device stopped, changed or could not be used during the check.
pub const CAPTURE_FAILED: &str = "The audio check did not finish: the microphone or the output stopped, changed or could not be used. Check the devices on this page and run it again.";
/// Anything else.
pub const OTHER_FAILED: &str = "The audio check did not finish. Run it again.";

/// The page's words for a failed check.
#[must_use]
pub fn failure_copy(e: &RuntimeError) -> String {
    match e {
        // The runtime's words are for the CLI: they can carry a device error's debug text,
        // so the page shows fixed copy (T9.23).
        RuntimeError::Capture(_) => CAPTURE_FAILED.to_owned(),
        // Why a device could not start, and what to do (T9.23).
        RuntimeError::CaptureStart { problem, .. } => {
            crate::plain_text::capture_copy(*problem).to_owned()
        }
        RuntimeError::Store(_) | RuntimeError::Repo(_) => STORE_FAILED.to_owned(),
        _ => OTHER_FAILED.to_owned(),
    }
}

/// The production work: the Audio page's devices through WASAPI.
pub struct HostAudioCheckWork;

impl AudioCheckWork for HostAudioCheckWork {
    #[cfg(windows)]
    fn run(
        &self,
        settings: &AudioSettings,
        cancel: &CancelToken,
        progress: &mut dyn FnMut(Duration),
    ) -> Result<Option<Measured>, RuntimeError> {
        check::run_check(&settings.devices(), cancel, progress)
    }

    #[cfg(not(windows))]
    fn run(
        &self,
        _: &AudioSettings,
        _: &CancelToken,
        _: &mut dyn FnMut(Duration),
    ) -> Result<Option<Measured>, RuntimeError> {
        Err(RuntimeError::Capture(
            "the audio check needs Windows (WASAPI)".into(),
        ))
    }
}

/// The audio check part of the Audio page: the last check of the devices a meeting would use
/// now.
#[tauri::command]
#[specta::specta]
pub async fn audio_check(
    state: tauri::State<'_, crate::ipc::AppState>,
) -> Result<AudioCheckView, IpcError> {
    let services = state.services()?;
    blocking(move || {
        let (settings, _) = earshot_runtime::audio_settings::audio_settings(&services.store())?;
        let devices = crate::audio_settings::list_devices();
        check_view(&services, &settings, devices.as_ref())
    })
    .await
}

/// Starts the audio check on the Audio page's devices: it plays short sounds through the
/// output (`AudioCheckEvent`s follow). `busy` while a check, a meeting, the model check or
/// the voice sample runs.
#[tauri::command]
#[specta::specta]
pub fn start_audio_check(
    state: tauri::State<'_, crate::ipc::AppState>,
    jobs: tauri::State<'_, AudioCheckJobs>,
) -> Result<(), IpcError> {
    let services = state.services()?;
    jobs.start(&services)
}

/// Cancels the audio check; nothing is stored.
#[tauri::command]
#[specta::specta]
pub fn cancel_audio_check(jobs: tauri::State<'_, AudioCheckJobs>) {
    jobs.cancel();
}

/// Runs blocking work off the main thread.
async fn blocking<T: Send + 'static>(
    f: impl FnOnce() -> Result<T, IpcError> + Send + 'static,
) -> Result<T, IpcError> {
    tauri::async_runtime::spawn_blocking(f)
        .await
        .map_err(|_| IpcError {
            kind: "internal".into(),
            message: "the task stopped".into(),
            retryable: false,
        })?
}

#[cfg(test)]
#[path = "audio_check_tests.rs"]
mod tests;
