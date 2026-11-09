//! The Speakers page of Settings (T7.4h): when speaker detection runs (the T6.13d4 setting,
//! `earshot_runtime::speaker_timing`, read once per meeting start, so a change applies from
//! the next meeting) and "your voice" (`earshot_runtime::my_voice`): record a short sample
//! and save it as the user's voiceprint, or forget it.
//!
//! The voice sample runs on a thread of its own ([`VoiceJobs`]), holding the
//! [`crate::activity::VoicePass`] for its whole life, so it never overlaps a meeting or the
//! model check. It sends [`VoiceEvent`]s: `Recording` progress, `Saving`, then exactly one
//! of `Done`, `Cancelled` or `Failed` (a panic in the work becomes `Failed`). The audio is
//! held in memory on that thread only and dropped there; only the voiceprint is stored.
//! Tests drive it through [`VoiceWork`] fakes over replay sources, never the microphone.

use std::panic::AssertUnwindSafe;
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::JoinHandle;
use std::time::Duration;

use earshot_core::asr::CancelToken;
use earshot_core::audio::MonoBuffer;
use earshot_runtime::my_voice::{self, MyVoice, MyVoiceOutcome, SAMPLE_LENGTH};
use earshot_runtime::speaker_timing::{
    self as timing_setting, AFTER_CALL_WARNING, SpeakerTiming, StoredTiming, speaker_timing,
};
use earshot_runtime::{RuntimeError, Services};
use serde::{Deserialize, Serialize};
use specta::Type;
use tauri_specta::Event;

use crate::activity::WorkGate;
use crate::ipc::IpcError;

/// When speaker detection runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Type)]
pub enum SpeakerTimingChoice {
    /// During the call (the default).
    DuringCall,
    /// After the call.
    AfterCall,
    /// After the call on battery, during it on mains power (laptops).
    AfterCallOnBattery,
}

impl SpeakerTimingChoice {
    fn of(t: SpeakerTiming) -> Self {
        match t {
            SpeakerTiming::DuringCall => Self::DuringCall,
            SpeakerTiming::AfterCall => Self::AfterCall,
            SpeakerTiming::AfterCallOnBattery => Self::AfterCallOnBattery,
        }
    }

    fn timing(self) -> SpeakerTiming {
        match self {
            Self::DuringCall => SpeakerTiming::DuringCall,
            Self::AfterCall => SpeakerTiming::AfterCall,
            Self::AfterCallOnBattery => SpeakerTiming::AfterCallOnBattery,
        }
    }
}

/// One choice of the speaker detection setting.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Type)]
pub struct SpeakerTimingOption {
    /// The choice.
    pub value: SpeakerTimingChoice,
    /// Its label ("During the call", ...).
    pub label: String,
    /// One line on what it means.
    pub description: String,
}

/// What the page says about each choice.
fn description(t: SpeakerTiming) -> &'static str {
    match t {
        SpeakerTiming::DuringCall => {
            "Speakers are found while the meeting runs, so the final transcript and summary \
             are ready soon after you hang up."
        }
        SpeakerTiming::AfterCall => AFTER_CALL_WARNING,
        SpeakerTiming::AfterCallOnBattery => {
            "During the call when the computer is plugged in, after the call when it runs \
             on battery, to save power."
        }
    }
}

/// Whether the user's voice is saved.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Type)]
pub struct VoiceView {
    /// A voiceprint of the user is saved.
    pub saved: bool,
    /// The speech behind it (ms).
    pub speech_ms: u64,
    /// How long a sample records (ms).
    pub sample_ms: u64,
    /// The name the user gave; empty until they give one (T9.1h).
    pub name: String,
}

impl VoiceView {
    fn of(v: &MyVoice) -> Self {
        Self {
            saved: v.saved(),
            speech_ms: u64::try_from(v.speech.as_millis()).unwrap_or(u64::MAX),
            sample_ms: u64::try_from(SAMPLE_LENGTH.as_millis()).unwrap_or(u64::MAX),
            name: v.name.clone().unwrap_or_default(),
        }
    }
}

/// The Speakers page.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Type)]
pub struct SpeakersView {
    /// The setting in use (the default when nothing or an unknown value is stored).
    pub timing: SpeakerTimingChoice,
    /// The choices this computer offers, in order: the battery choice only on a computer
    /// with a battery (or when it is the stored choice).
    pub options: Vec<SpeakerTimingOption>,
    /// One line per stored value this version does not know.
    pub unknown: Vec<String>,
    /// Whether this computer has a battery.
    pub laptop: bool,
    /// The user's voice.
    pub voice: VoiceView,
}

fn invalid(message: &str) -> IpcError {
    IpcError {
        kind: "invalid_setting".into(),
        message: message.into(),
        retryable: false,
    }
}

/// The page for `services`' store; `laptop`: this computer has a battery.
///
/// # Errors
/// Store errors.
pub fn store_my_name(services: &Services, name: &str) -> Result<(), IpcError> {
    match earshot_runtime::my_voice::set_my_name(services, name) {
        Ok(_) => Ok(()),
        Err(RuntimeError::Edit(e)) => Err(crate::detail::edit_error(&e)),
        Err(e) => Err(e.into()),
    }
}

/// The Speakers page.
///
/// # Errors
/// Store errors.
pub fn speakers_view(services: &Services, laptop: bool) -> Result<SpeakersView, IpcError> {
    let stored = speaker_timing(&services.store())?;
    let timing = stored.timing();
    let unknown = match &stored {
        StoredTiming::Unknown(v) => vec![format!(
            "The stored speaker detection setting ({v:?}) is not known to this version; \
             during the call is used."
        )],
        StoredTiming::Absent | StoredTiming::Set(_) => Vec::new(),
    };
    let options = SpeakerTiming::ALL
        .into_iter()
        .filter(|t| laptop || *t != SpeakerTiming::AfterCallOnBattery || *t == timing)
        .map(|t| SpeakerTimingOption {
            value: SpeakerTimingChoice::of(t),
            label: t.label().into(),
            description: description(t).into(),
        })
        .collect();
    Ok(SpeakersView {
        timing: SpeakerTimingChoice::of(timing),
        options,
        unknown,
        laptop,
        voice: VoiceView::of(&my_voice::my_voice(services)?),
    })
}

/// Stores the speaker detection setting (used from the next meeting).
///
/// # Errors
/// `invalid_setting` for the battery choice on a computer without a battery; store errors.
pub fn set_timing(
    services: &Services,
    choice: SpeakerTimingChoice,
    laptop: bool,
) -> Result<SpeakersView, IpcError> {
    let timing = choice.timing();
    if timing == SpeakerTiming::AfterCallOnBattery && !laptop {
        return Err(invalid(
            "This computer has no battery, so this choice would work as during the call.",
        ));
    }
    timing_setting::set_speaker_timing(&mut services.store(), timing)?;
    speakers_view(services, laptop)
}

/// Forgets the user's voice; returns how many voiceprints were forgotten.
///
/// # Errors
/// Store errors (`store`, retryable when the deletion is committed but the checkpoint was
/// blocked).
pub fn forget_voice(services: &Services) -> Result<u32, IpcError> {
    let n = my_voice::forget_my_voice(services)?;
    Ok(u32::try_from(n).unwrap_or(u32::MAX))
}

/// How a saved or refused sample ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Type)]
pub enum VoiceResult {
    /// The voiceprint was saved.
    Saved,
    /// Nothing was saved: no speech heard.
    NoSpeech,
    /// Nothing was saved: another voice was heard too much.
    SeveralVoices,
    /// Nothing was saved: too little speech for a first voiceprint.
    NotEnoughSpeech,
    /// Nothing was saved: the speaker model cannot match voices.
    Uncalibrated,
}

/// Progress and the end of a voice sample.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Type, Event)]
#[serde(tag = "type")]
pub enum VoiceEvent {
    /// Recording: this much of the sample so far (ms), of `total_ms`.
    Recording {
        /// Recorded so far.
        recorded_ms: u64,
        /// The sample's length.
        total_ms: u64,
    },
    /// Recorded; the voice is being measured and saved.
    Saving,
    /// Finished (terminal): saved, or refused with the reason.
    Done {
        /// What happened.
        result: VoiceResult,
        /// What to tell the user.
        message: String,
        /// The voice now.
        voice: VoiceView,
    },
    /// Cancelled before anything was saved (terminal).
    Cancelled,
    /// Failed (terminal); nothing was saved.
    Failed {
        /// Why.
        message: String,
    },
}

/// The page's words for an outcome.
#[must_use]
pub fn outcome_event(outcome: &MyVoiceOutcome, voice: VoiceView) -> VoiceEvent {
    let secs = |d: &Duration| d.as_secs();
    let (result, message) = match outcome {
        MyVoiceOutcome::Enrolled { speech, .. } => (
            VoiceResult::Saved,
            format!(
                "Your voice is saved ({} seconds of speech). Earshot will name you \"Me\" when \
                 it hears you.",
                secs(speech)
            ),
        ),
        MyVoiceOutcome::NoSpeech => (
            VoiceResult::NoSpeech,
            "No speech was heard, so nothing was saved. Check the microphone and try again.".into(),
        ),
        MyVoiceOutcome::SeveralVoices { .. } => (
            VoiceResult::SeveralVoices,
            "More than one voice was heard, so nothing was saved. Record again somewhere \
             quiet, speaking alone."
                .into(),
        ),
        MyVoiceOutcome::NotEnoughSpeech { needed, got } => (
            VoiceResult::NotEnoughSpeech,
            format!(
                "Only {} seconds of speech were heard and {} are needed, so nothing was \
                 saved. Read the text aloud without long pauses and try again.",
                secs(got),
                secs(needed)
            ),
        ),
        MyVoiceOutcome::Uncalibrated { .. } => (
            VoiceResult::Uncalibrated,
            "The speaker model on this computer cannot recognise voices yet, so nothing was \
             saved."
                .into(),
        ),
        // Mapped to `VoiceEvent::Cancelled` by the caller.
        MyVoiceOutcome::Cancelled => return VoiceEvent::Cancelled,
    };
    VoiceEvent::Done {
        result,
        message,
        voice,
    }
}

/// The work of a voice sample (the microphone and the speaker models, or fakes in tests).
pub trait VoiceWork: Send + Sync {
    /// Records the sample; `None` when cancelled. Runs on the voice thread.
    ///
    /// # Errors
    /// Capture errors.
    fn record(
        &self,
        cancel: &CancelToken,
        progress: &mut dyn FnMut(Duration),
    ) -> Result<Option<MonoBuffer>, RuntimeError>;

    /// Saves the voice in `audio`.
    ///
    /// # Errors
    /// Model and store errors.
    fn enrol(
        &self,
        services: &Services,
        audio: &MonoBuffer,
        cancel: &CancelToken,
    ) -> Result<MyVoiceOutcome, RuntimeError>;
}

/// Where voice events go.
pub type VoiceSink = Arc<dyn Fn(VoiceEvent) + Send + Sync>;

struct Running {
    cancel: CancelToken,
    thread: JoinHandle<()>,
}

struct Inner {
    work: Arc<dyn VoiceWork>,
    sink: VoiceSink,
    gate: WorkGate,
    slot: Mutex<Option<Running>>,
}

/// Runs at most one voice sample (cheap to clone; clones share it).
#[derive(Clone)]
pub struct VoiceJobs(Arc<Inner>);

/// How often recording progress is reported (of recorded audio, not wall time).
pub const PROGRESS_STEP: Duration = Duration::from_millis(500);

/// What a voice sample that panicked reports.
pub const VOICE_PANIC_MESSAGE: &str = "The voice sample stopped unexpectedly.";

impl VoiceJobs {
    /// Jobs of `work`, sending events to `sink`; a sample holds a voice pass of `gate`.
    pub fn new(work: Arc<dyn VoiceWork>, sink: VoiceSink, gate: WorkGate) -> Self {
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

    /// Starts a sample (events follow).
    ///
    /// # Errors
    /// `busy` (retryable) while a sample, a meeting or the model check runs.
    pub fn start(&self, services: &Services) -> Result<(), IpcError> {
        let mut slot = self.slot();
        if let Some(old) = slot.take() {
            if !old.thread.is_finished() {
                *slot = Some(old);
                return Err(IpcError {
                    kind: "busy".into(),
                    message: "Your voice sample is already being recorded.".into(),
                    retryable: true,
                });
            }
            let _ = old.thread.join();
        }
        let pass = self.0.gate.enter_voice()?;
        let cancel = CancelToken::new();
        let inner = Arc::clone(&self.0);
        let svc = services.clone();
        let token = cancel.clone();
        let thread = std::thread::Builder::new()
            .name("earshot-voice".into())
            .spawn(move || {
                let event =
                    std::panic::catch_unwind(AssertUnwindSafe(|| run(&inner, &svc, &token)))
                        .unwrap_or_else(|_| VoiceEvent::Failed {
                            message: VOICE_PANIC_MESSAGE.into(),
                        });
                drop(pass);
                (inner.sink)(event);
            })
            .map_err(|e| IpcError {
                kind: "internal".into(),
                message: format!("cannot start the voice thread: {e}"),
                retryable: false,
            })?;
        *slot = Some(Running { cancel, thread });
        Ok(())
    }

    /// Cancels the running sample (a `Cancelled` follows unless it already ended).
    pub fn cancel(&self) {
        if let Some(r) = self.slot().as_ref() {
            r.cancel.cancel();
        }
    }

    /// Waits for the running sample.
    #[cfg(test)]
    pub fn join(&self) {
        if let Some(r) = self.slot().take() {
            let _ = r.thread.join();
        }
    }
}

/// The sample, from recording to the terminal event (returned, sent by the caller after the
/// pass is released).
fn run(inner: &Inner, svc: &Services, cancel: &CancelToken) -> VoiceEvent {
    let total_ms = u64::try_from(SAMPLE_LENGTH.as_millis()).unwrap_or(u64::MAX);
    let step = u64::try_from(PROGRESS_STEP.as_millis()).unwrap_or(1).max(1);
    let mut next = 0u64;
    let sink = Arc::clone(&inner.sink);
    let mut progress = |d: Duration| {
        let ms = u64::try_from(d.as_millis()).unwrap_or(u64::MAX);
        if ms >= next {
            sink(VoiceEvent::Recording {
                recorded_ms: ms.min(total_ms),
                total_ms,
            });
            next = (ms / step + 1) * step;
        }
    };
    let audio = match inner.work.record(cancel, &mut progress) {
        Ok(Some(a)) => a,
        Ok(None) => return VoiceEvent::Cancelled,
        Err(e) => {
            return VoiceEvent::Failed {
                message: failure_copy(&e),
            };
        }
    };
    if cancel.is_cancelled() {
        return VoiceEvent::Cancelled;
    }
    (inner.sink)(VoiceEvent::Saving);
    let outcome = inner.work.enrol(svc, &audio, cancel);
    drop(audio);
    match outcome {
        Ok(o) => match my_voice::my_voice(svc) {
            Ok(v) => outcome_event(&o, VoiceView::of(&v)),
            Err(e) => VoiceEvent::Failed {
                message: failure_copy(&e),
            },
        },
        Err(e) => VoiceEvent::Failed {
            message: failure_copy(&e),
        },
    }
}

/// What the page says when a voice sample fails (plain words, never the runtime's text).
pub const MIC_FAILED: &str = "The microphone could not be opened or stopped working, so nothing \
     was saved. Check that a microphone is connected and allowed in Windows privacy settings, \
     then try again.";
/// The speaker models (or ONNX Runtime) are missing or did not load.
pub const MODELS_FAILED: &str = "The speaker detection models are not installed or did not \
     load, so nothing was saved. Open Settings, System, download the speech helpers, then try \
     again.";
/// The store could not be read or written.
pub const STORE_FAILED: &str = "Earshot could not save your voice to its database, so nothing \
     was saved. Try again.";
/// Anything else.
pub const OTHER_FAILED: &str = "Your voice could not be saved. Try again.";

/// The page's words for a failed sample, by the kind of failure.
#[must_use]
pub fn failure_copy(e: &RuntimeError) -> String {
    match e {
        RuntimeError::Capture(_) => MIC_FAILED,
        RuntimeError::ModelMissing { .. }
        | RuntimeError::ModelIncomplete { .. }
        | RuntimeError::UnknownModel(_)
        | RuntimeError::NoBackend(_)
        | RuntimeError::Unavailable(_)
        | RuntimeError::Onnx(_)
        | RuntimeError::Diar(_)
        | RuntimeError::Uncalibrated { .. } => MODELS_FAILED,
        RuntimeError::Store(_) | RuntimeError::Repo(_) | RuntimeError::Voiceprint(_) => {
            STORE_FAILED
        }
        _ => OTHER_FAILED,
    }
    .to_owned()
}

/// The production work: the default microphone in the room (no loopback) through WASAPI,
/// and the installed speaker models.
pub struct HostVoiceWork;

#[cfg(windows)]
impl VoiceWork for HostVoiceWork {
    fn record(
        &self,
        cancel: &CancelToken,
        progress: &mut dyn FnMut(Duration),
    ) -> Result<Option<MonoBuffer>, RuntimeError> {
        use earshot_capture_wasapi::wasapi::WasapiSource;
        use earshot_capture_wasapi::{CaptureMode, MicRole};
        let mut source = WasapiSource::start(MicRole::default(), &CaptureMode::InRoom)
            .map_err(|e| RuntimeError::Capture(e.to_string()))?;
        my_voice::record_sample(
            &mut source,
            SAMPLE_LENGTH,
            cancel,
            &mut || std::thread::sleep(Duration::from_millis(10)),
            progress,
        )
    }

    fn enrol(
        &self,
        services: &Services,
        audio: &MonoBuffer,
        cancel: &CancelToken,
    ) -> Result<MyVoiceOutcome, RuntimeError> {
        my_voice::enrol_with_installed_models(services, audio, cancel, &mut |_, _| {})
    }
}

#[cfg(not(windows))]
impl VoiceWork for HostVoiceWork {
    fn record(
        &self,
        _: &CancelToken,
        _: &mut dyn FnMut(Duration),
    ) -> Result<Option<MonoBuffer>, RuntimeError> {
        Err(RuntimeError::Capture(
            "recording needs Windows (WASAPI)".into(),
        ))
    }

    fn enrol(
        &self,
        services: &Services,
        audio: &MonoBuffer,
        cancel: &CancelToken,
    ) -> Result<MyVoiceOutcome, RuntimeError> {
        my_voice::enrol_with_installed_models(services, audio, cancel, &mut |_, _| {})
    }
}

/// Whether this computer has a battery (the battery choice is offered only then). A power
/// status the OS cannot report counts as a battery, so the choice is not hidden wrongly.
#[must_use]
pub fn this_is_a_laptop() -> bool {
    earshot_runtime::power::system_power() != earshot_runtime::power::PowerSupply::NoBattery
}

/// The Speakers page: the speaker detection setting and whether your voice is saved.
#[tauri::command]
#[specta::specta]
pub async fn speakers(
    state: tauri::State<'_, crate::ipc::AppState>,
) -> Result<SpeakersView, IpcError> {
    let services = state.services()?;
    crate::ipc::blocking(move || speakers_view(&services, this_is_a_laptop())).await
}

/// Stores when speaker detection runs (from the next meeting); `invalid_setting` for the
/// battery choice on a computer without a battery.
#[tauri::command]
#[specta::specta]
pub async fn set_speaker_timing(
    state: tauri::State<'_, crate::ipc::AppState>,
    timing: SpeakerTimingChoice,
) -> Result<SpeakersView, IpcError> {
    let services = state.services()?;
    crate::ipc::blocking(move || set_timing(&services, timing, this_is_a_laptop())).await
}

/// Stores the user's name (T9.1h); empty goes back to none.
#[tauri::command]
#[specta::specta]
pub async fn set_my_name(
    state: tauri::State<'_, crate::ipc::AppState>,
    name: String,
) -> Result<SpeakersView, IpcError> {
    let services = state.services()?;
    crate::ipc::blocking(move || {
        store_my_name(&services, &name)?;
        speakers_view(&services, this_is_a_laptop())
    })
    .await
}

/// Starts recording a voice sample of the user, saved as their voiceprint when it is
/// usable (`VoiceEvent`s follow). The page asks the user before calling this. `busy` while
/// a sample, a meeting or the model check runs.
#[tauri::command]
#[specta::specta]
pub fn start_voice_sample(
    state: tauri::State<'_, crate::ipc::AppState>,
    voice: tauri::State<'_, VoiceJobs>,
) -> Result<(), IpcError> {
    let services = state.services()?;
    voice.start(&services)
}

/// Cancels the voice sample; nothing is saved.
#[tauri::command]
#[specta::specta]
pub fn cancel_voice_sample(voice: tauri::State<'_, VoiceJobs>) {
    voice.cancel();
}

/// Forgets the user's voice; returns how many voiceprints were forgotten.
#[tauri::command]
#[specta::specta]
pub async fn forget_my_voice(
    state: tauri::State<'_, crate::ipc::AppState>,
) -> Result<u32, IpcError> {
    let services = state.services()?;
    crate::ipc::blocking(move || forget_voice(&services)).await
}

#[cfg(test)]
#[path = "speaker_settings_tests.rs"]
mod tests;
