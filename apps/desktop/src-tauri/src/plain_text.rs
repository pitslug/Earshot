//! What the app says when something a meeting needs is not on this computer (T9.1f): fixed
//! copy that points at the Settings page holding it (T12.5b): the speech helpers on System,
//! transcription models on Transcription, summary models on Summaries. The runtime's own text
//! names developer commands
//! (`RuntimeError`'s Display, for the CLI and for tests); none of them may reach a window.
//!
//! The exits, each pinned by a test in this module: every runtime error becomes an `IpcError`
//! through `From<RuntimeError>` ([`runtime_text`]); the events a job or a meeting sends pass
//! through [`plain_meeting_event`], [`plain_job_event`] and [`plain_model_event`] on their
//! way into the sink, whatever built them (a banner, a summary's failure, a child's error);
//! the model check's child sends [`runtime_text`]; the ONNX Runtime fallback banners use
//! [`library_problem`]. Transcript text and summary titles are the user's and pass unchanged.
//!
//! The copy names what to download only when the error's own file says what it is
//! ([`Catalogue::model_of_file`]); otherwise it points at the System and Transcription pages,
//! which list what is missing. Telling someone to fetch the wrong thing is worse than telling them where to look.

use earshot_runtime::NetError;
use earshot_runtime::catalogue::{Catalogue, ModelKind, display_name};
use earshot_runtime::claude_code::LlmError;
use earshot_runtime::live::PreviewOffReason;
use earshot_runtime::post::PostWarning;
use earshot_runtime::{CaptureProblem, RuntimeError};
use earshot_store::StoreError;

use crate::cuda_settings::CudaEvent;
use crate::jobs::JobEvent;
use crate::meeting_events::{LiveTrack, MeetingEvent, SummaryEvent};
use crate::model_jobs::ModelJobEvent;

/// What the user is told when a download is missing and nothing says which.
pub const MODELS_NEEDED: &str = "Earshot needs a download that is not on this computer yet. Open \
     Settings: the System and Transcription pages show what is missing.";

/// The speech helpers are what is missing.
pub const HELPERS_NEEDED: &str = "Earshot needs the speech helpers, which are not on this \
     computer yet. Open Settings, System to download them.";

/// A transcription model is what is missing. Which one is the user's choice, so none is named:
/// on a clean install the one a run would try is only the fallback.
pub const TRANSCRIPTION_NEEDED: &str = "Earshot needs a transcription model that is not on this \
     computer yet. Open Settings, Transcription to download one.";

/// A transcription model and the speech helpers are both missing, as on a clean install: said in
/// one go, so a meeting is not refused for one and then for the other.
pub const TRANSCRIPTION_AND_HELPERS_NEEDED: &str = "Earshot needs a transcription model and the \
     speech helpers, which are not on this computer yet. Open Settings to download them: the \
     model on the Transcription page, the helpers on the System page.";

/// A summary model chosen in Settings is what is missing, when its file does not say which.
pub const SUMMARY_MODEL_NEEDED: &str = "The summary model you chose is not on this computer yet. \
     Open Settings, Summaries to download it or choose another.";

/// What the app says about what the final pass warned of, as `(banner key, text)`: fixed copy and
/// counts, one banner per kind and track, never a warning's own developer text. Left out: what
/// changes nothing for the user (a stored record that could not be read means the track was
/// transcribed or diarised whole; the backend each pass used is on the meeting's header).
#[must_use]
pub fn post_warning_copy(warnings: &[PostWarning]) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for track in [LiveTrack::You, LiveTrack::Them] {
        let of_track = |t: earshot_core::capture::TrackId| LiveTrack::of(t) == track;
        let dropped: usize = warnings
            .iter()
            .map(|w| match w {
                PostWarning::RepetitionDropped { track: t, spans } if of_track(*t) => spans.len(),
                _ => 0,
            })
            .sum();
        if dropped > 0 {
            let passages = if dropped == 1 {
                "1 passage".to_owned()
            } else {
                format!("{dropped} passages")
            };
            out.push((
                format!("post-repetition:{}", track.key()),
                format!(
                    "{passages} from {} could not be transcribed, because every attempt repeated \
                     itself. The final transcript has nothing for them.",
                    track.device()
                ),
            ));
        }
        let voices = warnings
            .iter()
            .filter(
                |w| matches!(w, PostWarning::VoiceWithoutWords { track: t, .. } if of_track(*t)),
            )
            .count();
        if voices > 0 {
            let heard = if voices == 1 {
                "1 voice".to_owned()
            } else {
                format!("{voices} voices")
            };
            out.push((
                format!("post-voices:{}", track.key()),
                format!(
                    "Earshot heard {heard} on {} with no transcribed words, so the speaker list \
                     leaves them out.",
                    track.device()
                ),
            ));
        }
    }
    // T12.16: the user's corrected lines were kept over the new transcript.
    let edited: usize = warnings
        .iter()
        .map(|w| match w {
            PostWarning::EditedLinesKept { lines, .. } => *lines,
            _ => 0,
        })
        .sum();
    if edited > 0 {
        let lines = if edited == 1 {
            "1 line you corrected was".to_owned()
        } else {
            format!("{edited} lines you corrected were")
        };
        out.push((
            "post-edited".to_owned(),
            format!(
                "{lines} kept as you wrote them. The new transcript leaves out its own words for those moments."
            ),
        ));
    }
    for w in warnings {
        let shown = match w {
            PostWarning::NotEnoughSpeechForMe { .. } => Some((
                "post-my-voice",
                "This meeting had too little of your voice to remember it.",
            )),
            PostWarning::SkippedVoiceprints(_) => Some((
                "post-voiceprints",
                "Some remembered voices could not be read, so those people were not recognised.",
            )),
            PostWarning::ProvenanceNotStored(_) => Some((
                "post-record",
                "The final transcript was saved, but Earshot could not save its note of how it \
                 was made.",
            )),
            PostWarning::NoSpeakerEchoCheck { .. } => Some((
                "post-echo",
                "The check for echo by voice could not run, so some of the other side may show \
                 as your words.",
            )),
            PostWarning::SpeakerNamesNotCarried { .. } => Some((
                "post-names",
                "The names given in this meeting could not be kept. Name its voices again.",
            )),
            PostWarning::BackendChanged { .. }
            | PostWarning::StretchRecordUnreadable { .. }
            | PostWarning::SpeakerRecordUnreadable { .. }
            | PostWarning::RepetitionDropped { .. }
            | PostWarning::VoiceWithoutWords { .. }
            | PostWarning::EditedLinesKept { .. } => None,
        };
        if let Some((key, text)) = shown
            && !out.iter().any(|(k, _)| k == key)
        {
            out.push((key.to_owned(), text.to_owned()));
        }
    }
    out
}

/// Whose provisional words a banner sentence is about (T12.33).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PreviewScope {
    /// Every voice the user asked provisional words for.
    All,
    /// The user's own voice, while other people's are shown or off for another reason.
    You,
    /// Other people's, while the user's own are shown or off for another reason.
    Them,
}

/// The user's own voice is refused beside the NPU: the other people's words are still shown.
pub const PREVIEWS_OWN_VOICE_NPU: &str = "Provisional words are shown for other people only. \
     Earshot cannot show them for your own voice on this computer yet.";

/// The user's own voice is refused for want of memory: the other people's words are still shown.
pub const PREVIEWS_OWN_VOICE_MEMORY: &str = "Provisional words are shown for other people only. \
     This computer does not have enough memory to show them for your own voice as well.";

/// What the meeting screen says when provisional words are off or waiting (T12.33), by kind:
/// fixed copy, never the runtime's text. `None` for what needs no word: the user turned them off
/// or never asked, the meeting has none by design, or another banner already says it (a stopped
/// live transcript).
#[must_use]
pub fn provisional_words_copy(reason: PreviewOffReason, scope: PreviewScope) -> Option<String> {
    let rest = match reason {
        PreviewOffReason::OnBattery => {
            "are off for this meeting because the computer is on battery. Plug it in before a \
             meeting starts to see them."
        }
        PreviewOffReason::CatchingUp => "start once the live transcript has caught up.",
        PreviewOffReason::FellBehind => {
            "are off for the rest of this meeting because the live transcript fell behind. The \
             transcript still appears as it is ready."
        }
        PreviewOffReason::NeedsCompiledModel => {
            "are off for this meeting because Earshot is still preparing the transcription model \
             for this computer. They start in a later meeting once it is ready."
        }
        PreviewOffReason::Model => {
            "are not available with the transcription model this meeting uses. You can choose \
             another model in Settings, Transcription."
        }
        PreviewOffReason::LoadFailed => {
            "are off for this meeting because Earshot could not load what they need. The \
             transcript is not affected."
        }
        PreviewOffReason::Failed => {
            "stopped working and are off for the rest of this meeting. The transcript is not \
             affected."
        }
        // About the user's own voice whatever the scope.
        PreviewOffReason::OwnVoiceOnNpu => return Some(PREVIEWS_OWN_VOICE_NPU.to_owned()),
        PreviewOffReason::NotEnoughMemory => return Some(PREVIEWS_OWN_VOICE_MEMORY.to_owned()),
        PreviewOffReason::TranscriptStopped
        | PreviewOffReason::OffByDefault
        | PreviewOffReason::TurnedOff
        | PreviewOffReason::NotAsked
        | PreviewOffReason::NotApplicable
        | PreviewOffReason::MeetingEnded => return None,
    };
    let who = match scope {
        PreviewScope::All => "Provisional words",
        PreviewScope::You => "Provisional words for your own voice",
        PreviewScope::Them => "Provisional words for other people",
    };
    Some(format!("{who} {rest}"))
}

/// Text only a developer can act on. Lower case and specific, so a device called "Cargo Pro"
/// or a folder name does not trip it; `xtask` also covers every `cargo xtask` command.
const DEVELOPER_WORDS: &[&str] = &[
    "cargo ",
    "xtask",
    "fetch-models",
    "qnn-convert",
    "EARSHOT_MODELS",
    "manifest.toml",
];

/// Whether `text` tells the reader to run a developer command or names a developer file.
#[must_use]
pub fn names_developer_tool(text: &str) -> bool {
    DEVELOPER_WORDS.iter().any(|w| text.contains(w))
}

/// `text`, or [`MODELS_NEEDED`] when it names a developer tool.
#[must_use]
pub fn plain(text: String) -> String {
    if names_developer_tool(&text) {
        MODELS_NEEDED.to_owned()
    } else {
        text
    }
}

/// The file `e` says is missing or damaged.
fn missing_file(e: &RuntimeError) -> Option<&std::path::Path> {
    match e {
        RuntimeError::ModelMissing { path, .. } | RuntimeError::ModelIncomplete { path, .. } => {
            Some(path)
        }
        _ => None,
    }
}

fn model_file_missing(e: &RuntimeError) -> bool {
    missing_file(e).is_some()
}

/// The copy for summary model `model` missing, named as the user knows it.
fn summary_copy_of(model: &str) -> String {
    format!(
        "The summary model you chose, {}, is not on this computer yet. Open Settings, Summaries to \
         download it or choose another.",
        display_name(model)
    )
}

/// The copy for a missing transcription model. `helpers_there` is whether the speech helpers are
/// installed in the same folder; unknown counts as there, and the next refusal then names them.
fn transcription_copy(helpers_there: bool) -> &'static str {
    if helpers_there {
        TRANSCRIPTION_NEEDED
    } else {
        TRANSCRIPTION_AND_HELPERS_NEEDED
    }
}

/// What the user is told when summary model `model`, chosen in Settings, is not downloaded.
#[must_use]
pub fn chosen_summary_model(model: &str) -> String {
    summary_copy_of(model)
}

/// Why a meeting kept on this computer (T12.13) gets no summary: only a summary model on this
/// computer may write it and `model` is not downloaded. `ahead` for the banner at the start
/// ("will not run"), else for the summary result and a refused re-summarise.
#[must_use]
pub fn kept_local_no_model(model: &str, ahead: bool) -> String {
    let lead = if ahead {
        "The summary will not run"
    } else {
        "There is no summary"
    };
    format!(
        "{lead}: this meeting is kept on this computer, so only a summary model here can write it, and {} is not downloaded yet. Download it in Settings, Summaries, then summarise this meeting.",
        display_name(model)
    )
}

/// What the user is told when a summary that would leave this computer is refused for a meeting
/// kept on it (T12.13).
pub const KEPT_LOCAL_REFUSED: &str = "This meeting is kept on this computer, so its summary is written only by the summary model here. Nothing was sent.";

/// No microphone is connected (T9.23).
pub const NO_MICROPHONE: &str = "Earshot found no microphone. Connect one, or check that it is \
     switched on in Windows' Sound settings, then try again.";
/// No speakers or headphones are connected, so the other side of a call cannot be recorded.
pub const NO_OUTPUT: &str = "Earshot found no speakers or headphones, so it cannot record the \
     other side of a call. Connect them and try again, or record a meeting in the room.";
/// Windows' microphone privacy switch is off for desktop apps.
pub const MICROPHONE_ACCESS_OFF: &str = "Windows is not letting Earshot use the microphone. Open \
     Windows Settings, Privacy and security, Microphone, turn on \"Let desktop apps access your \
     microphone\", then try again.";
/// Another app holds the device for itself.
pub const DEVICE_IN_USE: &str = "Another app is using the microphone or speakers on its own. \
     Close that app or change its sound settings, then try again.";
/// Capture failed for another reason.
pub const CAPTURE_FAILED: &str = "Earshot could not use the microphone or the computer's sound. \
     Check that they are connected, then try again. If it keeps failing, restart Earshot.";
/// The processor lacks the instructions whisper.cpp and llama.cpp need (T9.23).
pub const PROCESSOR_TOO_OLD: &str = "This computer's processor lacks instructions (AVX2) that \
     Whisper models and summaries made on this computer need. Choose another transcription \
     model in Settings, Transcription, and another way to summarise in Settings, Summaries.";
/// The database could not be read or written.
pub const DATA_FAILED: &str = "Earshot could not read or save its data on this computer. Check \
     that the disk has free space, then try again.";
/// The database was busy, or something changed meanwhile.
pub const TRY_AGAIN_SOON: &str = "Earshot was busy with this meeting. Try again in a moment.";
/// A meeting is being worked on.
pub const MEETING_BUSY: &str = "This meeting is being recorded, finished, exported or deleted. \
     Try again when that has finished.";
/// Something inside Earshot went wrong (a bug, not an input problem).
pub const INTERNAL: &str = "Something went wrong inside Earshot. Restart Earshot and try again.";
/// The audio could not be set up for a recording.
pub const PIPELINE_FAILED: &str = "Earshot could not process the sound from this device. Try \
     again, or choose another microphone in Settings, Audio.";
/// Transcription failed.
pub const TRANSCRIPTION_FAILED: &str = "The transcription did not finish. Try again; if it keeps \
     failing, choose another transcription model in Settings, Transcription.";
/// Diarisation or the voice models failed.
pub const VOICES_FAILED: &str = "Earshot could not tell the voices in this meeting apart. Try \
     again; if it keeps failing, download the speech helpers again in Settings, System.";
/// Remembered voices could not be read or saved.
pub const VOICEPRINTS_FAILED: &str = "Earshot could not read or save remembered voices. Try again.";
/// Nothing was recorded to process.
pub const NO_AUDIO: &str = "This meeting has no recorded sound to transcribe.";
/// This version cannot use a model it found, or a model named in its settings.
pub const NOT_THIS_VERSION: &str = "This version of Earshot cannot use a model it needs here. \
     Choose another in Settings, or update Earshot.";
/// No way of running the model worked.
pub const NO_BACKEND: &str = "Earshot could not run the transcription model on this computer. \
     Restart Earshot and try again; if it keeps failing, choose another model in Settings, \
     Transcription.";
/// The speech helpers (ONNX Runtime) failed.
pub const HELPERS_FAILED: &str = "The speech helpers could not run. Restart Earshot and try \
     again; if it keeps failing, download them again in Settings, System.";
/// The summary address cannot be used.
pub const ENDPOINT_UNUSABLE: &str = "The summary address in Settings, Summaries cannot be used. \
     Check it there, then try again.";
/// The summary address is plain http on the internet.
pub const ENDPOINT_NOT_HTTPS: &str = "Earshot sends summaries to an address on the internet only \
     over https. Change the address in Settings, Summaries.";
/// The network client could not be set up.
pub const NETWORK_SETUP_FAILED: &str =
    "Earshot could not set up its network connection. Restart Earshot and try again.";
/// The note could not be written to the vault.
pub const EXPORT_FAILED: &str = "Earshot could not write the note to your Obsidian vault. Check \
     the vault in Settings, Notes and storage, then try again.";
/// The summary service could not be reached.
pub const SUMMARY_UNREACHABLE: &str = "The summary could not be written: Earshot could not reach \
     the summary service. Check the network and the address in Settings, Summaries, then try \
     again.";
/// The summary service refused the key.
pub const SUMMARY_AUTH: &str = "The summary could not be written: the summary service refused \
     the API key. Check it in Settings, Summaries, then try again.";
/// The summary service answered with an error.
pub const SUMMARY_SERVICE_ERROR: &str = "The summary could not be written: the summary service \
     answered with an error. Try again later.";
/// The model's reply could not be used.
pub const SUMMARY_UNUSABLE: &str = "The summary could not be written: the model's reply could not \
     be used. Try again, or choose another way to summarise in Settings, Summaries.";
/// The meeting does not fit the model.
pub const SUMMARY_TOO_LONG: &str = "The summary could not be written: this meeting is too long \
     for the summary model. Choose another way to summarise in Settings, Summaries.";
/// The model declined.
pub const SUMMARY_REFUSED: &str =
    "The summary could not be written: the model declined to summarise this meeting.";
/// The summary was cancelled.
pub const SUMMARY_CANCELLED: &str = "The summary was cancelled.";
/// The local summary model failed.
pub const LOCAL_SUMMARY_FAILED: &str = "The summary model on this computer could not finish. \
     Try again; if it keeps failing, choose another summary model in Settings, Summaries.";

/// The copy for a capture that could not start, by its problem.
#[must_use]
pub fn capture_copy(problem: CaptureProblem) -> &'static str {
    match problem {
        CaptureProblem::NoMicrophone => NO_MICROPHONE,
        CaptureProblem::NoOutput => NO_OUTPUT,
        CaptureProblem::MicrophoneAccessOff => MICROPHONE_ACCESS_OFF,
        CaptureProblem::DeviceInUse => DEVICE_IN_USE,
        CaptureProblem::Other => CAPTURE_FAILED,
    }
}

fn store_copy(e: &StoreError) -> &'static str {
    match e {
        StoreError::SchemaTooNew { .. } => crate::ipc::NEWER_DATA_COPY,
        e if e.is_retryable() => TRY_AGAIN_SOON,
        StoreError::Vault(_) | StoreError::Export(_) | StoreError::UnusableNotePath(_) => {
            EXPORT_FAILED
        }
        _ => DATA_FAILED,
    }
}

fn summary_copy(e: &LlmError) -> String {
    match e {
        LlmError::ClaudeCode(cc) => crate::meeting_events::claude_code_copy(cc, false),
        LlmError::Codex(cx) => crate::meeting_events::codex_copy(cx, false),
        LlmError::Endpoint(_) | LlmError::Net(NetError::NotAllowed { .. }) => {
            ENDPOINT_UNUSABLE.to_owned()
        }
        LlmError::Net(_) => SUMMARY_UNREACHABLE.to_owned(),
        LlmError::Http {
            status: 401 | 403, ..
        } => SUMMARY_AUTH.to_owned(),
        LlmError::Http { .. } => SUMMARY_SERVICE_ERROR.to_owned(),
        LlmError::FormatRejected { .. } | LlmError::BadResponse(_) | LlmError::Invalid { .. } => {
            SUMMARY_UNUSABLE.to_owned()
        }
        LlmError::Prompt(_) => SUMMARY_TOO_LONG.to_owned(),
        LlmError::Refused { .. } => SUMMARY_REFUSED.to_owned(),
        LlmError::Cancelled => SUMMARY_CANCELLED.to_owned(),
    }
}

/// [`INTERNAL`], or [`MODELS_NEEDED`] when the runtime's fixed words name a developer command
/// (a missing NPU conversion is reported that way).
fn internal_copy(words: &'static str) -> &'static str {
    if names_developer_tool(words) {
        MODELS_NEEDED
    } else {
        INTERNAL
    }
}

/// What the user is told about runtime error `e`: fixed copy chosen by the error's kind,
/// never its text (T9.23). Exhaustive on purpose, so a new variant must be given its copy;
/// `plain_text_every_runtime_error_maps_to_fixed_copy` holds one of each.
#[must_use]
pub fn runtime_text(e: &RuntimeError) -> String {
    match e {
        RuntimeError::SummaryKeptLocal(_) => KEPT_LOCAL_REFUSED.to_owned(),
        RuntimeError::ModelMissing { .. } | RuntimeError::ModelIncomplete { .. } => {
            missing_model_copy(e)
        }
        RuntimeError::Store(s) => store_copy(s).to_owned(),
        RuntimeError::Repo(_) => DATA_FAILED.to_owned(),
        RuntimeError::Pipeline(_) => PIPELINE_FAILED.to_owned(),
        RuntimeError::Poisoned | RuntimeError::Spawn(_) => INTERNAL.to_owned(),
        RuntimeError::Internal(words) | RuntimeError::Unavailable(words) => {
            internal_copy(words).to_owned()
        }
        RuntimeError::Asr(_) => TRANSCRIPTION_FAILED.to_owned(),
        RuntimeError::Diar(_) => VOICES_FAILED.to_owned(),
        RuntimeError::Voiceprint(_) => VOICEPRINTS_FAILED.to_owned(),
        RuntimeError::NoTracks => NO_AUDIO.to_owned(),
        RuntimeError::Uncalibrated { .. } | RuntimeError::UnknownModel(_) => {
            NOT_THIS_VERSION.to_owned()
        }
        RuntimeError::Llm(l) => summary_copy(l),
        RuntimeError::LocalSummary(l) if l.is_cancelled() => SUMMARY_CANCELLED.to_owned(),
        RuntimeError::LocalSummary(_) => LOCAL_SUMMARY_FAILED.to_owned(),
        RuntimeError::Net(_) => NETWORK_SETUP_FAILED.to_owned(),
        RuntimeError::Endpoint(_) => ENDPOINT_UNUSABLE.to_owned(),
        RuntimeError::EndpointDenied(_) => ENDPOINT_NOT_HTTPS.to_owned(),
        RuntimeError::Export(_) => EXPORT_FAILED.to_owned(),
        RuntimeError::Conflict(_) => TRY_AGAIN_SOON.to_owned(),
        RuntimeError::NoBackend(_) => NO_BACKEND.to_owned(),
        RuntimeError::Onnx(_) => HELPERS_FAILED.to_owned(),
        RuntimeError::Capture(_) => CAPTURE_FAILED.to_owned(),
        RuntimeError::CaptureStart { problem, .. } => capture_copy(*problem).to_owned(),
        RuntimeError::ProcessorTooOld => PROCESSOR_TOO_OLD.to_owned(),
        RuntimeError::MeetingBusy(_) => MEETING_BUSY.to_owned(),
        RuntimeError::Edit(edit) => crate::detail::edit_error(edit).message,
        RuntimeError::ApiKey(k) => api_key_copy(k).to_owned(),
    }
}

/// The copy for an API key that could not be used, by kind; the credential store's own words
/// are Windows' text and never shown.
fn api_key_copy(e: &earshot_runtime::summary_settings::ApiKeyError) -> &'static str {
    use earshot_runtime::summary_settings::ApiKeyError;
    match e {
        ApiKeyError::Missing => {
            "Summaries go to Anthropic, but no API key is saved. Add it in Settings, Summaries, or choose another way to summarise."
        }
        ApiKeyError::Unsupported => {
            "Keeping an API key needs Windows Credential Manager, which this computer does not have. Choose another way to summarise in Settings, Summaries."
        }
        ApiKeyError::Invalid(_) => {
            "This API key cannot be saved. Copy it again without spaces or line breaks."
        }
        ApiKeyError::Store(_) => {
            "Windows Credential Manager could not be read. Check the key in Settings, Summaries, then try again."
        }
    }
}

/// The copy for a missing or incomplete model file, by what the file is.
fn missing_model_copy(e: &RuntimeError) -> String {
    let Some(path) = missing_file(e) else {
        return MODELS_NEEDED.to_owned();
    };
    let owner = Catalogue::embedded()
        .ok()
        .and_then(|c| c.model_of_file(path));
    match owner.map(|m| (m.kind, m.name.as_str())) {
        Some((ModelKind::Helpers, _)) => HELPERS_NEEDED.to_owned(),
        Some((ModelKind::Transcription, _)) => {
            let helpers_there = Catalogue::embedded()
                .ok()
                .and_then(|c| c.helpers_beside(path))
                .unwrap_or(true);
            transcription_copy(helpers_there).to_owned()
        }
        Some((ModelKind::Summary, name)) => summary_copy_of(name),
        // A model's NPU files are never loaded without every one of them (`npu_order`); the
        // Transcription page offers them.
        Some((ModelKind::NpuVersion, _)) | None => MODELS_NEEDED.to_owned(),
    }
}

/// What the user is told when the summary model chosen in Settings is not installed
/// (`host::SummaryPlanError::NotInstalled`): the caller knows it is a summary model whatever
/// the error's file says.
#[must_use]
pub fn summary_model_not_installed(e: &RuntimeError) -> String {
    let owner = missing_file(e)
        .and_then(|path| Catalogue::embedded().ok()?.model_of_file(path))
        .filter(|m| m.kind == ModelKind::Summary);
    owner.map_or_else(
        || SUMMARY_MODEL_NEEDED.to_owned(),
        |m| summary_copy_of(&m.name),
    )
}

/// Why an ONNX Runtime library could not load, inside a banner that says what runs instead.
#[must_use]
pub fn library_problem(e: &RuntimeError) -> String {
    let text = e.to_string();
    if model_file_missing(e) || names_developer_tool(&text) {
        "it is not downloaded".to_owned()
    } else {
        text
    }
}

fn plain_summary(result: SummaryEvent) -> SummaryEvent {
    match result {
        SummaryEvent::Skipped { reason } => SummaryEvent::Skipped {
            reason: plain(reason),
        },
        SummaryEvent::Failed { message, retryable } => SummaryEvent::Failed {
            message: plain(message),
            retryable,
        },
        SummaryEvent::RecordingFailed { message } => SummaryEvent::RecordingFailed {
            message: plain(message),
        },
        SummaryEvent::PostFailed { message, retryable } => SummaryEvent::PostFailed {
            message: plain(message),
            retryable,
        },
        other => other,
    }
}

/// `event` with its copy made plain; never touches transcript text or a summary's title.
#[must_use]
pub fn plain_meeting_event(event: MeetingEvent) -> MeetingEvent {
    match event {
        MeetingEvent::Warning { key, kind, message } => MeetingEvent::Warning {
            key,
            kind,
            message: plain(message),
        },
        MeetingEvent::Summary { meeting, result } => MeetingEvent::Summary {
            meeting,
            result: plain_summary(result),
        },
        other => other,
    }
}

/// `event` with its copy made plain.
#[must_use]
pub fn plain_job_event(event: JobEvent) -> JobEvent {
    match event {
        JobEvent::Summary { meeting, result } => JobEvent::Summary {
            meeting,
            result: plain_summary(result),
        },
        JobEvent::Warning { meeting, message } => JobEvent::Warning {
            meeting,
            message: plain(message),
        },
        JobEvent::Failed {
            meeting,
            job,
            message,
            retryable,
        } => JobEvent::Failed {
            meeting,
            job,
            message: plain(message),
            retryable,
        },
        JobEvent::Retranscribed {
            meeting,
            missing_tracks,
            no_audio,
            summary_stale,
            warnings,
        } => JobEvent::Retranscribed {
            meeting,
            missing_tracks,
            no_audio,
            summary_stale,
            warnings: warnings.into_iter().map(plain).collect(),
        },
        other => other,
    }
}

/// `event` with its copy made plain.
#[must_use]
pub fn plain_model_event(event: ModelJobEvent) -> ModelJobEvent {
    match event {
        ModelJobEvent::Warning { model, message } => ModelJobEvent::Warning {
            model,
            message: plain(message),
        },
        ModelJobEvent::Failed {
            model,
            message,
            retryable,
        } => ModelJobEvent::Failed {
            model,
            message: plain(message),
            retryable,
        },
        other => other,
    }
}

/// `event` with its copy made plain.
#[must_use]
pub fn plain_cuda_event(event: CudaEvent) -> CudaEvent {
    match event {
        CudaEvent::Warning { part, message } => CudaEvent::Warning {
            part,
            message: plain(message),
        },
        CudaEvent::Failed {
            part,
            message,
            retryable,
        } => CudaEvent::Failed {
            part,
            message: plain(message),
            retryable,
        },
        other => other,
    }
}

#[cfg(test)]
mod provisional_words_tests {
    use super::*;

    const SCOPES: [PreviewScope; 3] = [PreviewScope::All, PreviewScope::You, PreviewScope::Them];

    /// One of every kind. `index` is exhaustive, so a new kind fails to compile until it has an
    /// index here, a place in `ALL` and an entry in `plain_text_provisional_words_*` below.
    const ALL: [PreviewOffReason; 15] = [
        PreviewOffReason::OnBattery,
        PreviewOffReason::CatchingUp,
        PreviewOffReason::FellBehind,
        PreviewOffReason::TranscriptStopped,
        PreviewOffReason::NeedsCompiledModel,
        PreviewOffReason::OwnVoiceOnNpu,
        PreviewOffReason::NotEnoughMemory,
        PreviewOffReason::OffByDefault,
        PreviewOffReason::Model,
        PreviewOffReason::TurnedOff,
        PreviewOffReason::NotAsked,
        PreviewOffReason::NotApplicable,
        PreviewOffReason::LoadFailed,
        PreviewOffReason::Failed,
        PreviewOffReason::MeetingEnded,
    ];

    fn index(reason: PreviewOffReason) -> usize {
        match reason {
            PreviewOffReason::OnBattery => 0,
            PreviewOffReason::CatchingUp => 1,
            PreviewOffReason::FellBehind => 2,
            PreviewOffReason::TranscriptStopped => 3,
            PreviewOffReason::NeedsCompiledModel => 4,
            PreviewOffReason::OwnVoiceOnNpu => 5,
            PreviewOffReason::NotEnoughMemory => 6,
            PreviewOffReason::OffByDefault => 7,
            PreviewOffReason::Model => 8,
            PreviewOffReason::TurnedOff => 9,
            PreviewOffReason::NotAsked => 10,
            PreviewOffReason::NotApplicable => 11,
            PreviewOffReason::LoadFailed => 12,
            PreviewOffReason::Failed => 13,
            PreviewOffReason::MeetingEnded => 14,
        }
    }

    #[test]
    fn plain_text_provisional_words_list_holds_every_kind_once() {
        for (i, reason) in ALL.into_iter().enumerate() {
            assert_eq!(index(reason), i, "{reason:?}");
        }
    }

    /// Which kinds the user is told about, and which need no word.
    #[test]
    fn plain_text_provisional_words_say_only_what_the_user_can_act_on_or_wait_out() {
        let said = [
            PreviewOffReason::OnBattery,
            PreviewOffReason::CatchingUp,
            PreviewOffReason::FellBehind,
            PreviewOffReason::NeedsCompiledModel,
            PreviewOffReason::OwnVoiceOnNpu,
            PreviewOffReason::NotEnoughMemory,
            PreviewOffReason::Model,
            PreviewOffReason::LoadFailed,
            PreviewOffReason::Failed,
        ];
        for reason in ALL {
            for scope in SCOPES {
                assert_eq!(
                    provisional_words_copy(reason, scope).is_some(),
                    said.contains(&reason),
                    "{reason:?} {scope:?}"
                );
            }
        }
    }

    /// Plain sentences: no developer term, model id or part name, no em dash, no run of spaces
    /// (a lost line continuation), and every one starts "Provisional words" and ends in a stop.
    #[test]
    fn plain_text_provisional_words_copy_is_plain() {
        let mut seen = std::collections::BTreeSet::new();
        for reason in ALL {
            for scope in SCOPES {
                let Some(copy) = provisional_words_copy(reason, scope) else {
                    continue;
                };
                assert!(copy.starts_with("Provisional words"), "{copy}");
                assert!(copy.ends_with('.'), "{copy}");
                assert!(!copy.contains("  "), "a run of spaces in: {copy:?}");
                assert!(!copy.contains('\n'), "{copy:?}");
                assert!(!copy.contains('\u{2014}'), "{copy}");
                assert!(!copy.contains('!'), "{copy}");
                assert!(!names_developer_tool(&copy), "{copy}");
                let lower = copy.to_lowercase();
                for word in [
                    "preview", "session", "parakeet", "whisper", "onnx", "qnn", "npu", "cuda",
                    "backend", "thread", "decode", "compiled", "please",
                ] {
                    assert!(!lower.contains(word), "{word:?} in: {copy}");
                }
                seen.insert((index(reason), copy));
            }
        }
        // A kind's words differ by whose voices they are about, except the two about the user's
        // own voice, which name it themselves.
        let per_kind =
            |reason: PreviewOffReason| seen.iter().filter(|(i, _)| *i == index(reason)).count();
        assert_eq!(per_kind(PreviewOffReason::OnBattery), 3);
        assert_eq!(per_kind(PreviewOffReason::OwnVoiceOnNpu), 1);
        assert_eq!(per_kind(PreviewOffReason::NotEnoughMemory), 1);
        assert_eq!(
            provisional_words_copy(PreviewOffReason::OnBattery, PreviewScope::All).unwrap(),
            "Provisional words are off for this meeting because the computer is on battery. \
             Plug it in before a meeting starts to see them."
        );
        assert_eq!(
            provisional_words_copy(PreviewOffReason::FellBehind, PreviewScope::Them).unwrap(),
            "Provisional words for other people are off for the rest of this meeting because \
             the live transcript fell behind. The transcript still appears as it is ready."
        );
    }
}

#[cfg(test)]
mod post_warning_tests {
    use std::time::Duration;

    use earshot_core::asr::{BackendKind, MeetingTime};
    use earshot_core::capture::TrackId;
    use earshot_runtime::post::PostWarning;

    use super::{names_developer_tool, post_warning_copy};

    #[test]
    fn post_warnings_read_as_fixed_copy_with_counts_per_track_and_kind() {
        let span = (
            MeetingTime::from_millis(1_000),
            MeetingTime::from_millis(2_000),
        );
        let voice = |track| PostWarning::VoiceWithoutWords {
            track,
            speech: Duration::from_secs(1),
        };
        let copy = post_warning_copy(&[
            PostWarning::RepetitionDropped {
                track: TrackId::Mic,
                spans: vec![span, span],
            },
            PostWarning::RepetitionDropped {
                track: TrackId::Mic,
                spans: vec![span],
            },
            voice(TrackId::Reference),
            voice(TrackId::Reference),
            voice(TrackId::Mic),
            PostWarning::SkippedVoiceprints(2),
            PostWarning::SkippedVoiceprints(1),
        ]);
        let keys: Vec<&str> = copy.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(
            keys,
            [
                "post-repetition:you",
                "post-voices:you",
                "post-voices:them",
                "post-voiceprints"
            ]
        );
        assert!(copy[0].1.starts_with("3 passages from your microphone"));
        assert!(copy[1].1.contains("1 voice on your microphone"));
        assert!(copy[2].1.contains("2 voices on computer audio"));
    }

    #[test]
    fn post_warnings_say_how_many_corrected_lines_a_retranscription_kept() {
        let kept = |lines| PostWarning::EditedLinesKept {
            lines,
            words_dropped: 7,
        };
        let copy = post_warning_copy(&[kept(1)]);
        assert_eq!(copy.len(), 1);
        assert_eq!(copy[0].0, "post-edited");
        assert!(
            copy[0].1.starts_with("1 line you corrected was kept"),
            "{}",
            copy[0].1
        );
        let copy = post_warning_copy(&[kept(2), kept(1)]);
        assert_eq!(copy.len(), 1, "one banner");
        assert!(copy[0].1.starts_with("3 lines you corrected were kept"));
        assert!(!copy[0].1.contains('\u{2014}'));
        assert!(!names_developer_tool(&copy[0].1));
    }

    #[test]
    fn post_warnings_never_show_a_warnings_own_text_and_leave_out_what_changes_nothing() {
        let reason = "cargo xtask fetch-models secret-reason";
        let copy = post_warning_copy(&[
            PostWarning::ProvenanceNotStored(reason.into()),
            PostWarning::NoSpeakerEchoCheck {
                reason: reason.into(),
            },
            PostWarning::SpeakerNamesNotCarried {
                reason: reason.into(),
            },
        ]);
        assert_eq!(copy.len(), 3);
        for (_, text) in &copy {
            assert!(!text.contains("secret-reason"));
            assert!(!names_developer_tool(text));
            assert!(!text.contains('\u{2014}'));
        }
        assert!(
            post_warning_copy(&[
                PostWarning::BackendChanged {
                    before: BackendKind::Cpu,
                    after: BackendKind::Cpu,
                },
                PostWarning::StretchRecordUnreadable {
                    track: TrackId::Mic,
                    reason: reason.into(),
                },
                PostWarning::SpeakerRecordUnreadable {
                    track: TrackId::Mic,
                    reason: reason.into(),
                },
            ])
            .is_empty()
        );
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use earshot_runtime::backends::{Accelerator, CAMPPLUS, ModelDir, SILERO};
    use earshot_runtime::host;

    use super::*;
    use crate::ipc::IpcError;
    use crate::meeting_events::{WarningKind, skip_copy};
    use crate::testutil::TempDir;

    fn no_developer_text(text: &str) {
        for word in [
            "cargo",
            "xtask",
            "fetch-models",
            "EARSHOT_MODELS",
            "manifest",
        ] {
            assert!(!text.contains(word), "{word} in: {text}");
        }
    }

    /// The real refusals from a real empty models folder: each file a meeting needs in turn,
    /// with the copy each must give.
    fn real_refusals(dir: &TempDir) -> Vec<(RuntimeError, &'static str)> {
        let models = ModelDir::new(dir.0.join("models"));
        let mut out = vec![
            (
                // A clean install: both named at once, so a meeting is refused only once.
                host::require_models(&models, "base.en", true).unwrap_err(),
                TRANSCRIPTION_AND_HELPERS_NEEDED,
            ),
            (
                earshot_runtime::backends::init_onnx(&models, Accelerator::Cpu).unwrap_err(),
                HELPERS_NEEDED,
            ),
        ];
        let file = dir.0.join("models").join("whisper/ggml-base.en.bin");
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(&file, b"x").unwrap();
        // The transcription model is there: now a helper is what is missing.
        out.push((
            host::require_models(&models, "base.en", true).unwrap_err(),
            HELPERS_NEEDED,
        ));
        let silero = dir.0.join("models").join(SILERO);
        std::fs::create_dir_all(silero.parent().unwrap()).unwrap();
        std::fs::write(&silero, b"x").unwrap();
        out.push((
            host::require_models(&models, "base.en", true).unwrap_err(),
            HELPERS_NEEDED,
        ));
        assert!(!dir.0.join("models").join(CAMPPLUS).exists());
        out
    }

    #[test]
    fn plain_text_a_meeting_refused_for_a_missing_model_or_helper_shows_no_developer_command() {
        let dir = TempDir::new("plain-refusal");
        for (e, copy) in real_refusals(&dir) {
            // The runtime's own text keeps the command for the CLI and developers.
            assert!(e.to_string().contains("cargo xtask fetch-models"), "{e}");
            let ipc = IpcError::from(e);
            assert_eq!(ipc.kind, "model");
            assert_eq!(ipc.message, copy);
            no_developer_text(&ipc.message);
        }
        // Each points at the Settings page that holds what is missing (T12.5b).
        for (copy, pages) in [
            (MODELS_NEEDED, &["System and Transcription pages"][..]),
            (HELPERS_NEEDED, &["Settings, System"][..]),
            (TRANSCRIPTION_NEEDED, &["Settings, Transcription"][..]),
            (
                TRANSCRIPTION_AND_HELPERS_NEEDED,
                &["Transcription page", "System page"][..],
            ),
            (SUMMARY_MODEL_NEEDED, &["Settings, Summaries"][..]),
        ] {
            for page in pages {
                assert!(copy.contains(page), "{copy}");
            }
            no_developer_text(copy);
            assert_eq!(earshot_runtime::catalogue::manifest_id_in(copy), None);
        }
        // Copy that does not know what is missing names nothing to download.
        assert!(!MODELS_NEEDED.contains("speech helpers"));
        assert!(!MODELS_NEEDED.contains("transcription"));
        assert_eq!(
            earshot_runtime::catalogue::manifest_id_in(MODELS_NEEDED),
            None
        );
    }

    #[test]
    fn plain_text_a_missing_summary_model_is_never_sent_to_the_helpers_or_transcription() {
        use earshot_runtime::summary_model::{SUMMARY_MODELS, installed};
        let dir = TempDir::new("plain-summary");
        let models = ModelDir::new(dir.0.join("models"));
        let summary = &SUMMARY_MODELS[0];
        let missing = installed(&models, summary).unwrap_err();
        // A file of another size is the other variant, with the same answer.
        let file = dir.0.join("models").join(summary.file);
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(&file, b"x").unwrap();
        let incomplete = installed(&models, summary).unwrap_err();
        assert!(matches!(incomplete, RuntimeError::ModelIncomplete { .. }));
        for e in [missing, incomplete] {
            for text in [runtime_text(&e), summary_model_not_installed(&e)] {
                assert!(text.contains(display_name(summary.id)), "{text}");
                assert!(text.contains("Settings, Summaries"), "{text}");
                assert!(!text.contains("speech helpers"), "{text}");
                assert!(!text.contains("transcription"), "{text}");
                no_developer_text(&text);
                assert_eq!(earshot_runtime::catalogue::manifest_id_in(&text), None);
            }
        }
        // The caller knows it is a summary model even when the file says nothing.
        let other = RuntimeError::ModelMissing {
            path: PathBuf::from("somewhere/else.bin"),
            fetch_set: None,
        };
        assert_eq!(summary_model_not_installed(&other), SUMMARY_MODEL_NEEDED);
        assert_eq!(runtime_text(&other), MODELS_NEEDED);
    }

    #[test]
    fn plain_text_every_other_runtime_error_with_a_developer_command_is_replaced_too() {
        let incomplete = RuntimeError::ModelIncomplete {
            path: PathBuf::from("models/x.bin"),
            expected: 2,
            found: 1,
            fetch_set: "ci",
        };
        assert_eq!(IpcError::from(incomplete).message, MODELS_NEEDED);
        // Text the runtime builds for a QNN build and in `earshot_onnx::init`.
        let onnx = RuntimeError::Internal(
            "the NPU conversion is missing: run `cargo xtask qnn-convert --model m`",
        );
        assert_eq!(IpcError::from(onnx).message, MODELS_NEEDED);
        let unknown = RuntimeError::UnknownModel("old".into());
        no_developer_text(&IpcError::from(unknown).message);
        // Every other error is fixed copy too, never its own text (T9.23).
        let other = RuntimeError::Capture("the microphone is busy".into());
        assert_eq!(IpcError::from(other).message, CAPTURE_FAILED);
    }

    #[test]
    fn plain_text_events_lose_developer_commands_and_keep_the_users_words() {
        let dev = "run `cargo xtask fetch-models --set ci`".to_owned();
        let event = plain_meeting_event(MeetingEvent::Warning {
            key: "k".into(),
            kind: WarningKind::Backend,
            message: dev.clone(),
        });
        let MeetingEvent::Warning { message, .. } = event else {
            panic!("a warning stays a warning");
        };
        assert_eq!(message, MODELS_NEEDED);
        for result in [
            SummaryEvent::Skipped {
                reason: dev.clone(),
            },
            SummaryEvent::Failed {
                message: format!("The summary failed: {dev}"),
                retryable: false,
            },
            SummaryEvent::PostFailed {
                message: format!("The transcript could not be finished: {dev}"),
                retryable: false,
            },
        ] {
            let MeetingEvent::Summary { result, .. } = plain_meeting_event(MeetingEvent::Summary {
                meeting: "m".into(),
                result,
            }) else {
                panic!("a summary stays a summary");
            };
            no_developer_text(&serde_json::to_string(&result).unwrap());
        }
        for event in [
            JobEvent::Warning {
                meeting: "m".into(),
                message: dev.clone(),
            },
            JobEvent::Summary {
                meeting: "m".into(),
                result: SummaryEvent::Skipped {
                    reason: dev.clone(),
                },
            },
        ] {
            no_developer_text(&serde_json::to_string(&plain_job_event(event)).unwrap());
        }
        for event in [
            ModelJobEvent::Warning {
                model: None,
                message: dev.clone(),
            },
            ModelJobEvent::Failed {
                model: None,
                message: dev.clone(),
                retryable: false,
            },
        ] {
            no_developer_text(&serde_json::to_string(&plain_model_event(event)).unwrap());
        }
        // A person may say anything in a meeting: transcript and titles pass unchanged.
        let said = MeetingEvent::Landed {
            track: crate::meeting_events::LiveTrack::You,
            start_ms: 0,
            end_ms: 1_000,
            text: "run cargo xtask fetch-models".into(),
        };
        assert_eq!(plain_meeting_event(said.clone()), said);
        let titled = MeetingEvent::Summary {
            meeting: "m".into(),
            result: SummaryEvent::Stored {
                title: "cargo xtask review".into(),
                actions: 0,
                decisions: 0,
                voices_to_name: 0,
            },
        };
        assert_eq!(plain_meeting_event(titled.clone()), titled);
        // A device called Cargo is not a command.
        assert!(!names_developer_tool("Cargo Pro headset is back"));
    }

    #[test]
    fn plain_text_the_onnx_fallback_banners_say_what_runs_without_a_developer_command() {
        let dir = TempDir::new("plain-library");
        let missing = earshot_runtime::backends::init_onnx(
            &ModelDir::new(dir.0.join("models")),
            Accelerator::Cpu,
        )
        .unwrap_err();
        let banner = crate::meeting::onnx_fallback_message(Accelerator::Cuda, &missing);
        no_developer_text(&banner);
        assert!(banner.contains("it is not downloaded"), "{banner}");
        let check = crate::model_jobs::check_fallback_message(Accelerator::Cuda, &missing);
        no_developer_text(&check);
        assert!(check.contains("it is not downloaded"), "{check}");
    }

    /// With the helpers there, the copy names only the transcription model.
    #[test]
    fn plain_text_a_missing_transcription_model_names_the_helpers_only_when_they_are_missing_too() {
        assert_eq!(transcription_copy(true), TRANSCRIPTION_NEEDED);
        assert_eq!(transcription_copy(false), TRANSCRIPTION_AND_HELPERS_NEEDED);
    }

    #[test]
    fn plain_text_a_skipped_summary_points_at_the_summaries_page() {
        let dir = TempDir::new("plain-skip");
        let models = ModelDir::new(dir.0.join("models"));
        let why = host::require_models(&models, "base.en", false)
            .unwrap_err()
            .to_string();
        let copy = skip_copy(&earshot_runtime::host::SkipReason::NotInstalled {
            model: "qwen3.5-4b-q4_k_m",
            why,
        });
        no_developer_text(&copy);
        assert!(copy.contains("Settings, Summaries"), "{copy}");
        assert_eq!(earshot_runtime::catalogue::manifest_id_in(&copy), None);
    }

    /// The desktop's own sources never spell a developer command in shown text; the runtime's
    /// are caught at the exits above.
    #[test]
    fn plain_text_no_desktop_source_writes_a_developer_command_into_copy() {
        let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        for entry in std::fs::read_dir(&src).unwrap() {
            let path = entry.unwrap().path();
            let name = path.file_name().unwrap().to_string_lossy().into_owned();
            if !name.ends_with(".rs") || name.ends_with("_tests.rs") || name == "plain_text.rs" {
                continue;
            }
            let text = std::fs::read_to_string(&path).unwrap();
            // Comments may explain; string literals may not instruct.
            for (i, line) in text.lines().enumerate() {
                let code = line.split("//").next().unwrap_or("");
                assert!(
                    !code.contains("cargo xtask") && !code.contains("fetch-models"),
                    "{name}:{}: {line}",
                    i + 1
                );
            }
        }
    }
}

/// T9.23: raw error text never reaches a window. One error of every `RuntimeError` variant,
/// built twice with different words in every text field: the copy is the same both times (so it
/// comes from the kind, never the text), names no developer command or model id and holds none
/// of the words. `variant_index` is exhaustive, so a new variant fails to compile until it is
/// given an index here and a sample below.
#[cfg(test)]
mod every_variant_tests {
    use std::path::PathBuf;

    use earshot_core::asr::{AsrError, BackendKind};
    use earshot_core::diar::{DiarError, EmbeddingModelId};
    use earshot_core::export::ExportError;
    use earshot_core::identity::EntityId;
    use earshot_core::model::RepoError;
    use earshot_core::pipeline::PipelineError;
    use earshot_core::voiceprint::VoiceprintError;
    use earshot_runtime::claude_code::LlmError;
    use earshot_runtime::edit::EditError;
    use earshot_runtime::summary::LocalSummaryError;
    use earshot_runtime::summary_settings::ApiKeyError;
    use earshot_runtime::{
        Calibration, CaptureProblem, DenyReason, NetError, OnnxError, RuntimeError, UrlError,
    };
    use earshot_store::StoreError;

    use super::*;
    use crate::ipc::IpcError;

    const VARIANTS: usize = 31;

    fn variant_index(e: &RuntimeError) -> usize {
        match e {
            RuntimeError::Store(_) => 0,
            RuntimeError::Repo(_) => 1,
            RuntimeError::Pipeline(_) => 2,
            RuntimeError::Poisoned => 3,
            RuntimeError::Spawn(_) => 4,
            RuntimeError::Internal(_) => 5,
            RuntimeError::Asr(_) => 6,
            RuntimeError::Diar(_) => 7,
            RuntimeError::Voiceprint(_) => 8,
            RuntimeError::NoTracks => 9,
            RuntimeError::Uncalibrated { .. } => 10,
            RuntimeError::Llm(_) => 11,
            RuntimeError::LocalSummary(_) => 12,
            RuntimeError::Net(_) => 13,
            RuntimeError::Endpoint(_) => 14,
            RuntimeError::EndpointDenied(_) => 15,
            RuntimeError::Export(_) => 16,
            RuntimeError::Conflict(_) => 17,
            RuntimeError::ModelMissing { .. } => 18,
            RuntimeError::ModelIncomplete { .. } => 19,
            RuntimeError::UnknownModel(_) => 20,
            RuntimeError::NoBackend(_) => 21,
            RuntimeError::Unavailable(_) => 22,
            RuntimeError::Onnx(_) => 23,
            RuntimeError::Capture(_) => 24,
            RuntimeError::CaptureStart { .. } => 25,
            RuntimeError::ProcessorTooOld => 26,
            RuntimeError::MeetingBusy(_) => 27,
            RuntimeError::Edit(_) => 28,
            RuntimeError::ApiKey(_) => 29,
            RuntimeError::SummaryKeptLocal(_) => 30,
        }
    }

    /// One error of every variant, every text field holding `words`.
    fn samples(words: &'static str, id: EntityId) -> Vec<RuntimeError> {
        let path = || PathBuf::from(format!("models/{words}/{words}.bin"));
        vec![
            RuntimeError::Store(StoreError::Corrupt(words.into())),
            RuntimeError::Repo(RepoError::Storage(words.into())),
            RuntimeError::Pipeline(PipelineError::InvalidConfig(words.into())),
            RuntimeError::Poisoned,
            RuntimeError::Spawn(std::io::Error::other(words)),
            RuntimeError::Internal(words),
            RuntimeError::Asr(AsrError::Backend(words.into())),
            RuntimeError::Diar(DiarError::Backend(words.into())),
            RuntimeError::Voiceprint(VoiceprintError::Repo(words.into())),
            RuntimeError::NoTracks,
            RuntimeError::Uncalibrated {
                purpose: Calibration::Clustering,
                model: EmbeddingModelId::new(words),
            },
            RuntimeError::Llm(LlmError::Http {
                status: 500,
                message: words.into(),
            }),
            RuntimeError::LocalSummary(LocalSummaryError::ChildRun(words.into())),
            RuntimeError::Net(NetError::Build(words.into())),
            RuntimeError::Endpoint(UrlError::Parse(words.into())),
            RuntimeError::EndpointDenied(DenyReason::InsecureScheme),
            RuntimeError::Export(ExportError::Markers(words)),
            RuntimeError::Conflict(words),
            RuntimeError::ModelMissing {
                path: path(),
                fetch_set: Some("ci"),
            },
            RuntimeError::ModelIncomplete {
                path: path(),
                expected: 2,
                found: 1,
                fetch_set: "ci",
            },
            RuntimeError::UnknownModel(words.into()),
            RuntimeError::NoBackend(vec![(BackendKind::Cpu, AsrError::Backend(words.into()))]),
            RuntimeError::Unavailable(words),
            RuntimeError::Onnx(OnnxError::Runtime(words.into())),
            RuntimeError::Capture(words.into()),
            RuntimeError::CaptureStart {
                problem: CaptureProblem::MicrophoneAccessOff,
                detail: words.into(),
            },
            RuntimeError::ProcessorTooOld,
            RuntimeError::MeetingBusy(id),
            RuntimeError::Edit(EditError::MeetingNotFound(id)),
            RuntimeError::ApiKey(ApiKeyError::Store(words.into())),
            RuntimeError::SummaryKeptLocal(id),
        ]
    }

    #[test]
    fn plain_text_every_runtime_error_maps_to_fixed_copy() {
        let id = EntityId::from_bytes([9; 16]);
        let (a, b) = (samples("zq-first-words", id), samples("zq-other-words", id));
        let mut seen: Vec<usize> = a.iter().map(variant_index).collect();
        seen.sort_unstable();
        seen.dedup();
        assert_eq!(
            seen,
            (0..VARIANTS).collect::<Vec<_>>(),
            "one sample of each variant"
        );
        assert_eq!(a.len(), VARIANTS);
        for (x, y) in a.iter().zip(&b) {
            let copy = runtime_text(x);
            assert_eq!(copy, runtime_text(y), "the copy follows the text of {x:?}");
            assert!(!copy.contains("zq-"), "{x:?}: {copy}");
            assert!(!copy.contains(&id.to_string()), "{x:?}: {copy}");
            assert_ne!(copy, x.to_string(), "{x:?}");
            assert!(!copy.is_empty(), "{x:?}");
            assert!(!names_developer_tool(&copy), "{x:?}: {copy}");
            assert!(!copy.contains('\u{2014}'), "{x:?}: {copy}");
            assert_eq!(
                earshot_runtime::catalogue::manifest_id_in(&copy),
                None,
                "{x:?}"
            );
            // Lower-case Display words of the runtime never lead the copy.
            assert!(copy.starts_with(char::is_uppercase), "{x:?}: {copy}");
            // The window gets the same words through the IPC error.
            let ipc = IpcError::from(runtime_error_again(x));
            assert_eq!(ipc.message, copy, "{x:?}");
        }
    }

    /// `RuntimeError` is not `Clone`: the same sample again, by index.
    fn runtime_error_again(e: &RuntimeError) -> RuntimeError {
        let id = EntityId::from_bytes([9; 16]);
        samples("zq-first-words", id)
            .into_iter()
            .find(|s| variant_index(s) == variant_index(e))
            .unwrap()
    }

    /// Every fixed copy T9.23 adds or routes, as written in a window: no doubled space (a line
    /// continuation lost in a string literal leaves a run of spaces), no em dash, a full stop.
    #[test]
    fn plain_text_no_fixed_copy_holds_a_run_of_spaces() {
        use earshot_runtime::summary_settings::ApiKeyError as K;
        let id = EntityId::from_bytes([9; 16]);
        let mut copies: Vec<String> = [
            MODELS_NEEDED,
            HELPERS_NEEDED,
            TRANSCRIPTION_NEEDED,
            TRANSCRIPTION_AND_HELPERS_NEEDED,
            SUMMARY_MODEL_NEEDED,
            KEPT_LOCAL_REFUSED,
            NO_MICROPHONE,
            NO_OUTPUT,
            MICROPHONE_ACCESS_OFF,
            DEVICE_IN_USE,
            CAPTURE_FAILED,
            PROCESSOR_TOO_OLD,
            DATA_FAILED,
            TRY_AGAIN_SOON,
            MEETING_BUSY,
            INTERNAL,
            PIPELINE_FAILED,
            TRANSCRIPTION_FAILED,
            VOICES_FAILED,
            VOICEPRINTS_FAILED,
            NO_AUDIO,
            NOT_THIS_VERSION,
            NO_BACKEND,
            HELPERS_FAILED,
            ENDPOINT_UNUSABLE,
            ENDPOINT_NOT_HTTPS,
            NETWORK_SETUP_FAILED,
            EXPORT_FAILED,
            SUMMARY_UNREACHABLE,
            SUMMARY_AUTH,
            SUMMARY_SERVICE_ERROR,
            SUMMARY_UNUSABLE,
            SUMMARY_TOO_LONG,
            SUMMARY_REFUSED,
            SUMMARY_CANCELLED,
            LOCAL_SUMMARY_FAILED,
            crate::ipc::DATA_FOLDER_COPY,
            crate::ipc::NEWER_DATA_COPY,
            crate::model_jobs::NO_SERVER_ANSWERED,
            crate::meeting::SAVED_BUT_LIVE_FAILED,
            crate::meeting::MEETING_PANIC_MESSAGE,
        ]
        .into_iter()
        .map(str::to_owned)
        .collect();
        copies.extend(samples("zq-first-words", id).iter().map(runtime_text));
        for k in [
            K::Missing,
            K::Unsupported,
            K::Invalid("x"),
            K::Store("x".into()),
        ] {
            copies.push(runtime_text(&RuntimeError::ApiKey(k)));
        }
        copies.push(crate::meeting_events::post_failed_copy(
            &RuntimeError::NoTracks,
        ));
        for copy in &copies {
            assert!(!copy.contains("  "), "a run of spaces in: {copy:?}");
            assert!(!copy.contains('\n'), "a line break in: {copy:?}");
            assert!(!copy.contains('\u{2014}'), "{copy:?}");
            assert!(copy.ends_with('.'), "{copy:?}");
        }
    }

    #[test]
    fn plain_text_a_capture_that_cannot_start_says_why_and_what_to_do() {
        let copy = |problem| {
            runtime_text(&RuntimeError::CaptureStart {
                problem,
                detail: "IAudioClient::Initialize: Access is denied. (0x80070005)".into(),
            })
        };
        assert_eq!(
            copy(CaptureProblem::MicrophoneAccessOff),
            MICROPHONE_ACCESS_OFF
        );
        assert!(MICROPHONE_ACCESS_OFF.contains("Privacy and security, Microphone"));
        assert_eq!(copy(CaptureProblem::NoMicrophone), NO_MICROPHONE);
        assert_eq!(copy(CaptureProblem::NoOutput), NO_OUTPUT);
        assert_eq!(copy(CaptureProblem::DeviceInUse), DEVICE_IN_USE);
        assert_eq!(copy(CaptureProblem::Other), CAPTURE_FAILED);
        for text in [
            NO_MICROPHONE,
            NO_OUTPUT,
            MICROPHONE_ACCESS_OFF,
            DEVICE_IN_USE,
            CAPTURE_FAILED,
        ] {
            assert!(!text.contains("0x"), "{text}");
            assert!(text.contains("try again") || text.contains("Try"), "{text}");
        }
    }

    #[test]
    fn plain_text_summary_failures_say_what_to_do_by_kind() {
        let http = |status| {
            runtime_text(&RuntimeError::Llm(LlmError::Http {
                status,
                message: "server words".into(),
            }))
        };
        assert_eq!(http(401), SUMMARY_AUTH);
        assert_eq!(http(403), SUMMARY_AUTH);
        assert_eq!(http(503), SUMMARY_SERVICE_ERROR);
        assert_eq!(
            runtime_text(&RuntimeError::Llm(LlmError::Cancelled)),
            SUMMARY_CANCELLED
        );
        assert_eq!(
            runtime_text(&RuntimeError::Llm(LlmError::Net(
                NetError::TooManyRedirects
            ))),
            SUMMARY_UNREACHABLE
        );
        assert_eq!(
            runtime_text(&RuntimeError::Llm(LlmError::Net(NetError::NotAllowed {
                url: "http://x/".into(),
                reason: DenyReason::HostNotAllowed,
            }))),
            ENDPOINT_UNUSABLE
        );
        // A busy store says try again; one saved by a newer Earshot says update.
        assert_eq!(
            runtime_text(&RuntimeError::Store(StoreError::Busy("locked".into()))),
            TRY_AGAIN_SOON
        );
        assert_eq!(
            runtime_text(&RuntimeError::Store(StoreError::SchemaTooNew {
                found: 9,
                supported: 1
            })),
            crate::ipc::NEWER_DATA_COPY
        );
        // A processor without AVX2 names the pages with something that still works.
        assert!(PROCESSOR_TOO_OLD.contains("Settings, Transcription"));
        assert!(PROCESSOR_TOO_OLD.contains("Settings, Summaries"));
    }
}
