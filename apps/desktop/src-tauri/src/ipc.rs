//! The IPC contract between the desktop UI and Rust (T7.1, T7.2).
//!
//! Every command is registered in [`specta_builder`], and every event (today only
//! [`MeetingEvent`]); tauri-specta generates the TypeScript side into
//! `packages/ui/src/ipc/bindings.ts`, which `bindings_are_up_to_date` keeps in step
//! (`EARSHOT_UPDATE_BINDINGS=1 cargo test -p earshot-desktop bindings` rewrites it).

use std::sync::{Arc, Mutex, PoisonError};

use earshot_core::asr::CancelToken;
use earshot_core::identity::EntityId;
use earshot_core::model::{Meeting, MeetingRepo};
use earshot_runtime::search::{MatchedIn, Searches};
use earshot_runtime::{MeetingActivity, RuntimeError, Services, SystemClock};
use earshot_store::{DataDir, StoreError};
use serde::{Deserialize, Serialize};
use specta::Type;
use tauri::State;
use tauri_specta::{Builder, collect_commands, collect_events};

use crate::credentials::{CredentialManager, KeyVault};
use crate::detail::{
    self, ExportDone, LinkChoice, MeetingDetail, PersonItem, SavedVoiceResult, SummaryFieldRef,
    parse_id,
};
use crate::jobs::{JobEvent, JobKind, JobRunner};
use crate::meeting::{MeetingDriver, StartRequest};
use crate::meeting_events::{CaptureMode, MeetingEvent};
use crate::model_jobs::{ModelJobEvent, ModelJobs};
use crate::models::{HardwareView, ModelSettingsValue, ModelSettingsView, ModelsView, RemovedView};
use crate::summaries::{SummarySettingsValue, SummarySettingsView};

/// The tauri-specta builder with every command and event of the contract.
///
/// 64-bit integers are exported as TypeScript `number`: the contract carries only Unix
/// milliseconds, meeting times, durations and byte counts (model sizes, memory) as
/// integers, which stay far below 2^53 (ids are strings).
pub fn specta_builder() -> Builder<tauri::Wry> {
    Builder::<tauri::Wry>::new()
        .commands(collect_commands![
            app_info,
            list_meetings,
            search_meetings,
            start_meeting,
            stop_meeting,
            cancel_finishing,
            keep_recording,
            set_compact,
            set_window_theme,
            get_meeting,
            rename_meeting,
            edit_segment,
            use_word,
            put_back_word,
            keep_word,
            add_dictionary_word,
            remove_dictionary_word,
            edit_summary,
            rename_speaker,
            merge_speakers,
            link_speaker,
            mark_me,
            unmark_me,
            rename_person,
            list_persons,
            save_voice,
            ignore_speaker,
            unignore_speaker,
            forget_person_voices,
            remove_person,
            speaker_sample,
            retranscribe,
            resummarise,
            undo_summary,
            export_meeting,
            crate::deletion::meeting_deletion,
            crate::deletion::delete_meeting,
            crate::keep_local::allow_summary_writer,
            crate::recovery::recovered_recordings,
            crate::folders::list_folders,
            crate::folders::create_folder,
            crate::folders::rename_folder,
            crate::folders::move_to_folder,
            crate::folders::delete_folder_keeping_meetings,
            crate::folders::delete_folder_with_meetings,
            hardware,
            models,
            model_settings,
            set_model_settings,
            start_download,
            cancel_download,
            remove_model,
            run_check,
            cancel_check,
            summary_settings,
            set_summary_settings,
            set_api_key,
            has_api_key,
            remove_api_key,
            claude_code_status,
            codex_status,
            crate::storage_settings::storage_settings,
            crate::storage_settings::preview_retention,
            crate::storage_settings::set_storage_settings,
            crate::speaker_settings::speakers,
            crate::speaker_settings::set_speaker_timing,
            crate::speaker_settings::set_my_name,
            crate::speaker_settings::start_voice_sample,
            crate::speaker_settings::cancel_voice_sample,
            crate::speaker_settings::forget_my_voice,
            crate::obsidian::obsidian_settings,
            crate::obsidian::set_obsidian_settings,
            crate::obsidian::pick_obsidian_vault,
            crate::general::general_settings,
            crate::update_settings::update_status,
            crate::update_settings::check_for_update,
            crate::update_settings::download_update,
            crate::update_settings::cancel_update,
            crate::update_settings::set_update_auto_check,
            crate::update_settings::set_update_schedule,
            crate::update_settings::install_update,
            crate::general::set_start_with_windows,
            crate::general::previews_settings,
            crate::general::set_previews,
            crate::crash::crash_logs_folder,
            crate::audio_settings::audio_settings,
            crate::audio_settings::set_audio_settings,
            crate::audio_settings::default_capture_mode,
            crate::audio_check::audio_check,
            crate::audio_check::start_audio_check,
            crate::audio_check::cancel_audio_check,
            crate::benchmark::benchmark,
            crate::benchmark::start_benchmark,
            crate::benchmark::cancel_benchmark,
            crate::summary_style::summary_style,
            crate::summary_style::set_summary_style,
            crate::summary_style::set_summary_template,
            crate::summary_vocabulary::summary_vocabulary,
            crate::summary_vocabulary::set_summary_vocabulary,
            crate::first_run::first_run_state,
            crate::first_run::finish_first_run,
            crate::cuda_settings::cuda_settings,
            crate::cuda_settings::set_cuda_settings,
            crate::cuda_settings::start_cuda_download,
            crate::cuda_settings::cancel_cuda_download,
            crate::cuda_settings::remove_cuda_part,
            crate::cuda_settings::open_cuda_terms,
            crate::cuda_settings::restart_app,
            crate::shell::quit_app
        ])
        .events(collect_events![
            MeetingEvent,
            JobEvent,
            ModelJobEvent,
            crate::update_settings::UpdateEvent,
            crate::speaker_settings::VoiceEvent,
            crate::audio_check::AudioCheckEvent,
            crate::benchmark::BenchmarkEvent,
            crate::cuda_settings::CudaEvent,
            crate::shell::ShellEvent
        ])
        .dangerously_cast_bigints_to_number()
}

/// An IPC failure as the UI sees it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Type)]
pub struct IpcError {
    /// A stable machine-readable kind (`snake_case`), for the UI to choose a message.
    pub kind: String,
    /// What the user is told: fixed copy chosen by the error's kind
    /// ([`crate::plain_text::runtime_text`]), never a runtime error's own text.
    pub message: String,
    /// Whether retrying the same operation can succeed without user action.
    pub retryable: bool,
}

impl IpcError {
    pub(crate) fn new(kind: &str, message: impl Into<String>) -> Self {
        Self {
            kind: kind.into(),
            message: message.into(),
            retryable: false,
        }
    }
}

/// What the app says when its database was saved by a newer Earshot (T9.5c): the store is
/// not touched, and every command fails with kind [`NEWER_DATA_KIND`].
pub const NEWER_DATA_COPY: &str =
    "This data was saved by a newer version of Earshot. Install the latest version to open it.";

/// The error kind of a database saved by a newer Earshot; the UI shows its own screen for it.
pub const NEWER_DATA_KIND: &str = "newer_data";

/// The error kind of a data folder that could not be opened at start (T9.23); the UI shows its
/// own screen for it, whose Try again restarts the app (the store is opened once, at start).
pub const DATA_FOLDER_KIND: &str = "data_folder";

/// What the app says when its data folder could not be opened.
pub const DATA_FOLDER_COPY: &str = "Earshot could not open its data folder on this computer. \
     Check that the disk has free space and that another program (a backup or antivirus tool) \
     is not holding it, then try again.";

/// The error of a data folder that could not be created or opened: [`NEWER_DATA_KIND`] for a
/// database saved by a newer Earshot, else [`DATA_FOLDER_KIND`]; never the error's text.
pub(crate) fn data_folder_error(e: Option<&RuntimeError>) -> IpcError {
    if let Some(RuntimeError::Store(StoreError::SchemaTooNew { .. })) = e {
        return IpcError::new(NEWER_DATA_KIND, NEWER_DATA_COPY);
    }
    IpcError::new(DATA_FOLDER_KIND, DATA_FOLDER_COPY)
}

impl From<RuntimeError> for IpcError {
    fn from(e: RuntimeError) -> Self {
        if let RuntimeError::Store(StoreError::SchemaTooNew { .. }) = &e {
            return Self::new(NEWER_DATA_KIND, NEWER_DATA_COPY);
        }
        // Edit refusals keep their own kinds; everything else is fixed copy chosen by the
        // error's kind, never its text (`plain_text::runtime_text`, T9.23).
        if let RuntimeError::Edit(edit) = &e {
            return crate::detail::edit_error(edit);
        }
        // Claude Code (T7.4ga): the reason and what to do, in the page's words.
        if let RuntimeError::Llm(earshot_runtime::claude_code::LlmError::ClaudeCode(cc)) = &e {
            return Self {
                kind: "claude_code".into(),
                message: crate::meeting_events::claude_code_copy(cc, false),
                retryable: e.is_retryable(),
            };
        }
        // Codex (T7.4gc): the same.
        if let RuntimeError::Llm(earshot_runtime::codex::LlmError::Codex(cx)) = &e {
            return Self {
                kind: "codex".into(),
                message: crate::meeting_events::codex_copy(cx, false),
                retryable: e.is_retryable(),
            };
        }
        Self {
            kind: runtime_kind(&e).into(),
            message: crate::plain_text::runtime_text(&e),
            retryable: e.is_retryable(),
        }
    }
}

/// The kind of a runtime error. Exhaustive on purpose: a new runtime error variant must be
/// given a kind here.
fn runtime_kind(e: &RuntimeError) -> &'static str {
    match e {
        RuntimeError::Store(_) | RuntimeError::Repo(_) => "store",
        RuntimeError::Pipeline(_) => "pipeline",
        RuntimeError::Poisoned | RuntimeError::Spawn(_) | RuntimeError::Internal(_) => "internal",
        RuntimeError::Asr(_) => "asr",
        RuntimeError::Diar(_) => "diarisation",
        RuntimeError::Voiceprint(_) => "voiceprint",
        RuntimeError::NoTracks => "no_tracks",
        RuntimeError::Uncalibrated { .. } => "uncalibrated",
        RuntimeError::Llm(_) | RuntimeError::LocalSummary(_) => "summary",
        RuntimeError::Net(_) | RuntimeError::Endpoint(_) | RuntimeError::EndpointDenied(_) => {
            "endpoint"
        }
        RuntimeError::Export(_) => "export",
        RuntimeError::Conflict(_) => "conflict",
        RuntimeError::ModelMissing { .. }
        | RuntimeError::ModelIncomplete { .. }
        | RuntimeError::UnknownModel(_) => "model",
        RuntimeError::NoBackend(_) | RuntimeError::Unavailable(_) | RuntimeError::Onnx(_) => {
            "backend"
        }
        RuntimeError::Capture(_) | RuntimeError::CaptureStart { .. } => "capture",
        RuntimeError::ProcessorTooOld => "processor",
        RuntimeError::MeetingBusy(_) => "meeting_busy",
        RuntimeError::Edit(_) => "edit",
        RuntimeError::ApiKey(_) => "api_key",
        RuntimeError::SummaryKeptLocal(_) => "summary_kept_local",
    }
}

/// What the app is and where its data lives.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Type)]
pub struct AppInfo {
    /// The app version (the workspace version).
    pub version: String,
    /// The data directory (`%LOCALAPPDATA%\Earshot`), empty when there is none.
    pub data_dir: String,
}

/// Where a meeting is in its life ([`Services::meeting_activity`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Type)]
pub enum MeetingState {
    /// Being recorded.
    Recording,
    /// Recorded; the work after the meeting (final pass, summary) is running.
    Processing,
    /// Recorded; the work after the meeting waits for an older meeting's to end.
    Waiting,
    /// The recording never finished (the app or the computer stopped during the meeting);
    /// the live transcript up to then is kept.
    Interrupted,
    /// Recorded to the end, but the cleanup after the meeting did not run or did not finish:
    /// only the live draft exists.
    Unprocessed,
    /// Recorded and processed.
    Ready,
}

impl MeetingState {
    /// The state of a meeting with this activity.
    #[must_use]
    pub fn of(activity: MeetingActivity) -> Self {
        match activity {
            MeetingActivity::Recording => Self::Recording,
            MeetingActivity::Processing => Self::Processing,
            MeetingActivity::Waiting => Self::Waiting,
            MeetingActivity::Interrupted => Self::Interrupted,
            MeetingActivity::Unprocessed => Self::Unprocessed,
            MeetingActivity::Ready => Self::Ready,
        }
    }
}

/// One row of the meeting list.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Type)]
pub struct MeetingListItem {
    /// Meeting id (hyphenated UUID).
    pub id: String,
    /// Title; empty until the user or a summary gives one.
    pub title: String,
    /// Start (Unix ms).
    pub started_at_ms: i64,
    /// Length in ms; `None` while recording (and for an interrupted recording).
    pub duration_ms: Option<u64>,
    /// Where it is in its life.
    pub state: MeetingState,
    /// The folder it is in (folder id), resolved by `earshot_runtime::folders`: `None` for no
    /// folder and for one that is deleted or unknown.
    pub folder_id: Option<String>,
}

impl MeetingListItem {
    fn of(m: &Meeting, state: MeetingState, folder: Option<EntityId>) -> Self {
        Self {
            id: m.id.to_string(),
            title: m.title.clone(),
            started_at_ms: i64::try_from(m.started_unix_ms).unwrap_or(i64::MAX),
            duration_ms: m.duration_ms,
            state,
            folder_id: folder.map(|f| f.to_string()),
        }
    }
}

/// The live meetings, newest first, each with its activity and resolved folder.
///
/// # Errors
/// Store errors.
pub fn meeting_list(services: &Services) -> Result<Vec<MeetingListItem>, IpcError> {
    let meetings = {
        let db = services.store();
        db.meetings().map_err(RuntimeError::from)?
    };
    // Never the raw `Meeting::folder_id`: a deleted or unknown folder reads as none.
    let filed = earshot_runtime::folders::meeting_folders(services)?;
    let mut items: Vec<(u64, MeetingListItem)> = Vec::new();
    for m in meetings.iter().filter(|m| !m.meta.deleted) {
        // `None`: deleted since the read above.
        if let Some(activity) = services.meeting_activity(m.id)? {
            items.push((
                m.started_unix_ms,
                MeetingListItem::of(m, MeetingState::of(activity), filed.get(&m.id).copied()),
            ));
        }
    }
    items.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| b.1.id.cmp(&a.1.id)));
    Ok(items.into_iter().map(|(_, i)| i).collect())
}

/// Where a search found a meeting.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Type)]
pub enum SearchMatch {
    /// Its title.
    Title,
    /// A line of the transcript it shows.
    Transcript,
    /// Its current summary.
    Summary,
}

/// One meeting a search found.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Type)]
pub struct SearchResultItem {
    /// Meeting id (hyphenated UUID).
    pub id: String,
    /// Title; empty until the user or a summary gives one.
    pub title: String,
    /// Start (Unix ms).
    pub started_at_ms: i64,
    /// Where it matched.
    pub matched: SearchMatch,
    /// The text around the first match, on one line.
    pub snippet: String,
    /// When the matching line starts (ms from the meeting's start); only for a transcript match.
    pub line_ms: Option<u64>,
}

/// What a search found.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Type)]
pub struct SearchResultsView {
    /// The meetings found, newest first.
    pub items: Vec<SearchResultItem>,
    /// Whether more meetings matched than `items` holds.
    pub capped: bool,
}

/// The error kind of a search that a newer search replaced before it answered (T9.24); the UI
/// waits for the newer one's answer.
pub const SEARCH_SUPERSEDED_KIND: &str = "superseded";

/// [`earshot_runtime::search::search_meetings_unless`] as the UI sees it. The query goes no
/// further: not into a log, a record or an error.
///
/// # Errors
/// Store errors, and kind [`SEARCH_SUPERSEDED_KIND`] once `cancel` is cancelled.
pub fn meeting_search(
    services: &Services,
    query: &str,
    cancel: &CancelToken,
) -> Result<SearchResultsView, IpcError> {
    let Some(results) = earshot_runtime::search::search_meetings_unless(services, query, cancel)?
    else {
        return Err(IpcError::new(
            SEARCH_SUPERSEDED_KIND,
            "A newer search replaced this one.",
        ));
    };
    Ok(SearchResultsView {
        items: results
            .hits
            .into_iter()
            .map(|hit| SearchResultItem {
                id: hit.meeting.to_string(),
                title: hit.title,
                started_at_ms: i64::try_from(hit.started_unix_ms).unwrap_or(i64::MAX),
                matched: match hit.matched {
                    MatchedIn::Title => SearchMatch::Title,
                    MatchedIn::Transcript => SearchMatch::Transcript,
                    MatchedIn::Summary => SearchMatch::Summary,
                },
                snippet: hit.snippet,
                line_ms: hit.line_ms,
            })
            .collect(),
        capped: results.capped,
    })
}

/// The app's shared state: the services over the store, or why they could not be opened.
pub struct AppState {
    data: Option<DataDir>,
    services: Result<Services, IpcError>,
    /// Where the Anthropic API key is kept (Windows Credential Manager).
    keys: Arc<dyn KeyVault>,
}

impl AppState {
    /// Opens the store in the default data directory (`%LOCALAPPDATA%\Earshot`, PLAN.md 3.6,
    /// the CLI's models default too), creating it on first use.
    pub fn open_default() -> Self {
        match DataDir::from_local_app_data() {
            Some(data) => Self::open(data),
            None => Self {
                data: None,
                keys: Arc::new(CredentialManager::anthropic()),
                services: Err(data_folder_error(None)),
            },
        }
    }

    /// Opens (creating) the store in `data`.
    pub fn open(data: DataDir) -> Self {
        let services = Self::services_in(&data);
        Self {
            data: Some(data),
            services,
            keys: Arc::new(CredentialManager::anthropic()),
        }
    }

    pub(crate) fn services_in(data: &DataDir) -> Result<Services, IpcError> {
        data.create().map_err(|_| data_folder_error(None))?;
        let store = data
            .open_store()
            .map_err(|e| data_folder_error(Some(&RuntimeError::from(e))))?;
        Services::new(store, data.clone(), Arc::new(SystemClock))
            .map_err(|e| data_folder_error(Some(&e)))
    }

    /// The data folder, known even when the store failed to open.
    pub(crate) fn data_dir(&self) -> Option<&DataDir> {
        self.data.as_ref()
    }

    pub(crate) fn services(&self) -> Result<Services, IpcError> {
        self.services.clone()
    }
}

/// The app version and data directory.
#[tauri::command]
#[specta::specta]
fn app_info(state: State<'_, AppState>) -> AppInfo {
    AppInfo {
        version: env!("CARGO_PKG_VERSION").into(),
        data_dir: state
            .data
            .as_ref()
            .map(|d| d.root().display().to_string())
            .unwrap_or_default(),
    }
}

/// The live meetings, newest first, with what is happening to each. Runs off the main
/// thread; the store lock is held only for each read.
#[tauri::command]
#[specta::specta]
async fn list_meetings(state: State<'_, AppState>) -> Result<Vec<MeetingListItem>, IpcError> {
    let services = state.services()?;
    tauri::async_runtime::spawn_blocking(move || meeting_list(&services))
        .await
        .map_err(|_| IpcError::new("internal", "the meeting list task stopped"))?
}

/// The live meetings whose title, transcript or summary contains `query`, newest first (T9.7).
/// Runs off the main thread; the store lock is held only for each meeting's read. A newer
/// search stops this one, which then fails with kind [`SEARCH_SUPERSEDED_KIND`] (T9.24). `sent`
/// numbers the window's searches in the order it sent them, so a newer one is never replaced by
/// an older one that happened to begin later, and `page` is a number the window picks at random
/// each time it loads, so a reloaded window's searches replace one still running from before
/// ([`Searches::begin_sent`]).
#[tauri::command]
#[specta::specta]
async fn search_meetings(
    state: State<'_, AppState>,
    searches: State<'_, Searches>,
    query: String,
    page: u32,
    sent: u32,
) -> Result<SearchResultsView, IpcError> {
    let sent = u64::from(sent);
    let cancel = searches.begin_sent(page, sent);
    let answer = match state.services() {
        Ok(services) => blocking(move || meeting_search(&services, &query, &cancel)).await,
        Err(e) => Err(e),
    };
    searches.end_sent(page, sent);
    answer
}

/// Starts a meeting in `mode` with an optional title and returns its id once it records.
/// `keep_local` keeps its summary on this computer (T12.13; asked only here).
/// Every check (models, summary setup, ONNX Runtime) runs first; a failure records nothing.
/// The meeting's progress arrives as `MeetingEvent`s. Fails with `busy` while another
/// meeting records or is processed.
#[tauri::command]
#[specta::specta]
async fn start_meeting(
    state: State<'_, AppState>,
    driver: State<'_, MeetingDriver>,
    mode: CaptureMode,
    title: Option<String>,
    keep_local: bool,
) -> Result<String, IpcError> {
    let services = state.services()?;
    let driver = driver.inner().clone();
    let request = StartRequest {
        mode,
        title: title.map(|t| t.trim().to_owned()).unwrap_or_default(),
        keep_local,
    };
    tauri::async_runtime::spawn_blocking(move || driver.start(&services, request))
        .await
        .map_err(|_| IpcError::new("internal", "the meeting start task stopped"))?
}

/// Stops the recording meeting; the recording is finalised and the work after the meeting
/// runs (`MeetingEvent::PostProgress`, then `Summary`). Fails with `not_recording` when no
/// meeting records.
#[tauri::command]
#[specta::specta]
fn stop_meeting(driver: State<'_, MeetingDriver>) -> Result<(), IpcError> {
    driver.stop()
}

/// Stops the final pass and the summary of a meeting whose recording is saved; its audio stays and
/// it can be re-transcribed. The meeting's `MeetingEvent::Summary` follows as usual. A meeting still
/// recording, or one that has just finished, is left as it is: never an error.
#[tauri::command]
#[specta::specta]
fn cancel_finishing(driver: State<'_, MeetingDriver>, meeting: String) {
    driver.cancel_finishing(&meeting);
}

/// The user answered "Keep recording" to the offer to stop when a call ends (T9.4).
#[tauri::command]
#[specta::specta]
fn keep_recording(driver: State<'_, MeetingDriver>) {
    driver.keep_recording();
}

/// Runs blocking work off the main thread.
pub(crate) async fn blocking<T: Send + 'static>(
    f: impl FnOnce() -> Result<T, IpcError> + Send + 'static,
) -> Result<T, IpcError> {
    tauri::async_runtime::spawn_blocking(f)
        .await
        .map_err(|_| IpcError::new("internal", "the task stopped"))?
}

/// Everything the meeting detail screen shows of `meeting`.
#[tauri::command]
#[specta::specta]
async fn get_meeting(
    state: State<'_, AppState>,
    meeting: String,
) -> Result<MeetingDetail, IpcError> {
    let services = state.services()?;
    let meeting = parse_id(&meeting)?;
    blocking(move || detail::meeting_detail(&services, meeting)).await
}

/// Renames a speaker of `meeting`.
#[tauri::command]
#[specta::specta]
async fn rename_speaker(
    state: State<'_, AppState>,
    meeting: String,
    speaker: String,
    name: String,
) -> Result<(), IpcError> {
    let services = state.services()?;
    let (meeting, speaker) = (parse_id(&meeting)?, parse_id(&speaker)?);
    blocking(move || {
        earshot_runtime::edit::rename_speaker(&services, meeting, speaker, &name)?;
        Ok(())
    })
    .await
}

/// Gives `meeting` a new title (empty: untitled).
#[tauri::command]
#[specta::specta]
async fn rename_meeting(
    state: State<'_, AppState>,
    meeting: String,
    title: String,
) -> Result<(), IpcError> {
    let services = state.services()?;
    let meeting = parse_id(&meeting)?;
    blocking(move || {
        earshot_runtime::edit::rename_meeting(&services, meeting, &title)?;
        Ok(())
    })
    .await
}

/// Replaces the text of one line of `meeting`'s final transcript (T12.16); returns the words
/// the bar under the line offers to add to the Dictionary (T12.15f, nothing added).
#[tauri::command]
#[specta::specta]
async fn edit_segment(
    state: State<'_, AppState>,
    meeting: String,
    segment: String,
    text: String,
) -> Result<Vec<String>, IpcError> {
    let services = state.services()?;
    let (meeting, segment) = (parse_id(&meeting)?, parse_id(&segment)?);
    blocking(move || crate::detail::edit_line(&services, meeting, segment, &text)).await
}

/// "Add to Dictionary" on a word of the transcript (T12.15f): the word of `token` joins the
/// Dictionary (`invalid_setting` with the Dictionary page's message: nothing stored).
#[tauri::command]
#[specta::specta]
async fn add_dictionary_word(
    state: State<'_, AppState>,
    token: String,
) -> Result<crate::detail::DictionaryWordAdded, IpcError> {
    let services = state.services()?;
    blocking(move || crate::detail::add_dictionary_word(&services, &token)).await
}

/// "Remove from Dictionary", and "Undo" after an add (T12.15f): the Dictionary entry that is
/// `word` (case ignored) goes; false when there is none.
#[tauri::command]
#[specta::specta]
async fn remove_dictionary_word(
    state: State<'_, AppState>,
    word: String,
) -> Result<bool, IpcError> {
    let services = state.services()?;
    blocking(move || crate::detail::remove_dictionary_word(&services, &word)).await
}

/// "Use it", or another word from the list (T12.15e): word `token` of a final transcript line
/// becomes `word`, which must be on the word list.
#[tauri::command]
#[specta::specta]
async fn use_word(
    state: State<'_, AppState>,
    meeting: String,
    segment: String,
    token: u32,
    word: String,
) -> Result<(), IpcError> {
    let services = state.services()?;
    let (meeting, segment) = (parse_id(&meeting)?, parse_id(&segment)?);
    blocking(move || {
        earshot_runtime::edit::use_word(
            &services,
            meeting,
            segment,
            usize::try_from(token).unwrap_or(usize::MAX),
            &word,
        )?;
        Ok(())
    })
    .await
}

/// "Put back" (T12.15e): word `token` of a line reads the decoded word again.
#[tauri::command]
#[specta::specta]
async fn put_back_word(
    state: State<'_, AppState>,
    meeting: String,
    segment: String,
    token: u32,
) -> Result<(), IpcError> {
    let services = state.services()?;
    let (meeting, segment) = (parse_id(&meeting)?, parse_id(&segment)?);
    blocking(move || {
        earshot_runtime::edit::put_back_word(
            &services,
            meeting,
            segment,
            usize::try_from(token).unwrap_or(usize::MAX),
        )?;
        Ok(())
    })
    .await
}

/// "Keep" (T12.15e): word `token` of a line stays and is not suggested again.
#[tauri::command]
#[specta::specta]
async fn keep_word(
    state: State<'_, AppState>,
    meeting: String,
    segment: String,
    token: u32,
) -> Result<(), IpcError> {
    let services = state.services()?;
    let (meeting, segment) = (parse_id(&meeting)?, parse_id(&segment)?);
    blocking(move || {
        earshot_runtime::edit::keep_word(
            &services,
            meeting,
            segment,
            usize::try_from(token).unwrap_or(usize::MAX),
        )?;
        Ok(())
    })
    .await
}

/// Corrects one text of `meeting`'s current summary `summary` (T12.16); the text before the
/// edit becomes the previous summary.
#[tauri::command]
#[specta::specta]
async fn edit_summary(
    state: State<'_, AppState>,
    meeting: String,
    summary: String,
    field: SummaryFieldRef,
    text: String,
) -> Result<(), IpcError> {
    let services = state.services()?;
    let (meeting, summary) = (parse_id(&meeting)?, parse_id(&summary)?);
    blocking(move || {
        earshot_runtime::edit::edit_summary(&services, meeting, summary, field.field(), &text)?;
        Ok(())
    })
    .await
}

/// Merges speaker `merge` into `keep`.
#[tauri::command]
#[specta::specta]
async fn merge_speakers(
    state: State<'_, AppState>,
    meeting: String,
    keep: String,
    merge: String,
) -> Result<(), IpcError> {
    let services = state.services()?;
    let (meeting, keep, merge) = (parse_id(&meeting)?, parse_id(&keep)?, parse_id(&merge)?);
    blocking(move || {
        earshot_runtime::edit::merge_speakers(&services, meeting, keep, merge)?;
        Ok(())
    })
    .await
}

/// Links a speaker to an existing or a new person; returns the person's id.
#[tauri::command]
#[specta::specta]
async fn link_speaker(
    state: State<'_, AppState>,
    meeting: String,
    speaker: String,
    choice: LinkChoice,
) -> Result<String, IpcError> {
    let services = state.services()?;
    let (meeting, speaker) = (parse_id(&meeting)?, parse_id(&speaker)?);
    blocking(move || detail::link(&services, meeting, speaker, &choice)).await
}

/// Says a voice of an in-room meeting is the user's ("This is me").
#[tauri::command]
#[specta::specta]
async fn mark_me(
    state: State<'_, AppState>,
    meeting: String,
    speaker: String,
) -> Result<(), IpcError> {
    let services = state.services()?;
    let (meeting, speaker) = (parse_id(&meeting)?, parse_id(&speaker)?);
    blocking(move || {
        earshot_runtime::edit::mark_me(&services, meeting, speaker)?;
        Ok(())
    })
    .await
}

/// Puts a "Me" row of an in-room meeting back as an unnamed voice ("This isn't me").
#[tauri::command]
#[specta::specta]
async fn unmark_me(
    state: State<'_, AppState>,
    meeting: String,
    speaker: String,
) -> Result<(), IpcError> {
    let services = state.services()?;
    let (meeting, speaker) = (parse_id(&meeting)?, parse_id(&speaker)?);
    blocking(move || {
        earshot_runtime::edit::unmark_me(&services, meeting, speaker)?;
        Ok(())
    })
    .await
}

/// Renames a person (and their speaker rows in every meeting); returns how many rows changed.
#[tauri::command]
#[specta::specta]
async fn rename_person(
    state: State<'_, AppState>,
    person: String,
    name: String,
) -> Result<u32, IpcError> {
    let services = state.services()?;
    let person = parse_id(&person)?;
    blocking(move || {
        let n = earshot_runtime::edit::rename_person(&services, person, &name)?;
        Ok(u32::try_from(n).unwrap_or(u32::MAX))
    })
    .await
}

/// Every live person, by name.
#[tauri::command]
#[specta::specta]
async fn list_persons(state: State<'_, AppState>) -> Result<Vec<PersonItem>, IpcError> {
    let services = state.services()?;
    blocking(move || detail::persons(&services)).await
}

/// "Save" on a voice that needs a name (T12.19): names it, links it to that person and
/// remembers the voice, in one step (the user's consent for this one voice).
#[tauri::command]
#[specta::specta]
async fn save_voice(
    state: State<'_, AppState>,
    meeting: String,
    speaker: String,
    choice: LinkChoice,
) -> Result<SavedVoiceResult, IpcError> {
    let services = state.services()?;
    let (meeting, speaker) = (parse_id(&meeting)?, parse_id(&speaker)?);
    blocking(move || detail::save_voice(&services, meeting, speaker, &choice)).await
}

/// "Ignore" on a voice that needs a name (T12.19): it no longer needs one and keeps its
/// "Speaker N" label.
#[tauri::command]
#[specta::specta]
async fn ignore_speaker(
    state: State<'_, AppState>,
    meeting: String,
    speaker: String,
) -> Result<(), IpcError> {
    let services = state.services()?;
    let (meeting, speaker) = (parse_id(&meeting)?, parse_id(&speaker)?);
    blocking(move || {
        earshot_runtime::edit::ignore_speaker(&services, meeting, speaker)?;
        Ok(())
    })
    .await
}

/// Undoes "Ignore": the voice needs a name again.
#[tauri::command]
#[specta::specta]
async fn unignore_speaker(
    state: State<'_, AppState>,
    meeting: String,
    speaker: String,
) -> Result<(), IpcError> {
    let services = state.services()?;
    let (meeting, speaker) = (parse_id(&meeting)?, parse_id(&speaker)?);
    blocking(move || {
        earshot_runtime::edit::unignore_speaker(&services, meeting, speaker)?;
        Ok(())
    })
    .await
}

/// Removes a person: their name and voice, from this computer (the Speakers page).
#[tauri::command]
#[specta::specta]
async fn remove_person(state: State<'_, AppState>, person: String) -> Result<(), IpcError> {
    let services = state.services()?;
    let person = parse_id(&person)?;
    blocking(move || {
        earshot_runtime::edit::remove_person(&services, person)?;
        Ok(())
    })
    .await
}

/// Forgets every voiceprint of a person; returns how many were forgotten.
#[tauri::command]
#[specta::specta]
async fn forget_person_voices(state: State<'_, AppState>, person: String) -> Result<u32, IpcError> {
    let services = state.services()?;
    let person = parse_id(&person)?;
    blocking(move || {
        let n = earshot_runtime::edit::forget_person_voices(&services, person)?;
        Ok(u32::try_from(n).unwrap_or(u32::MAX))
    })
    .await
}

/// A speaker's play-sample audio (a base64 WAV file); `None` when the audio is gone.
#[tauri::command]
#[specta::specta]
async fn speaker_sample(
    state: State<'_, AppState>,
    meeting: String,
    speaker: String,
) -> Result<Option<String>, IpcError> {
    let services = state.services()?;
    let (meeting, speaker) = (parse_id(&meeting)?, parse_id(&speaker)?);
    blocking(move || detail::speaker_sample(&services, meeting, speaker)).await
}

/// Starts a re-transcription of `meeting` (`JobEvent`s follow).
#[tauri::command]
#[specta::specta]
fn retranscribe(
    state: State<'_, AppState>,
    jobs: State<'_, JobRunner>,
    meeting: String,
) -> Result<(), IpcError> {
    let services = state.services()?;
    jobs.start(&services, parse_id(&meeting)?, JobKind::Retranscribe)
}

/// Starts a re-summary of `meeting`: the new summary replaces the current one, which is
/// kept for one undo (`JobEvent`s follow).
#[tauri::command]
#[specta::specta]
fn resummarise(
    state: State<'_, AppState>,
    jobs: State<'_, JobRunner>,
    meeting: String,
) -> Result<(), IpcError> {
    let services = state.services()?;
    jobs.start(&services, parse_id(&meeting)?, JobKind::Resummarise)
}

/// Swaps the current summary with the previous one.
#[tauri::command]
#[specta::specta]
async fn undo_summary(state: State<'_, AppState>, meeting: String) -> Result<(), IpcError> {
    let services = state.services()?;
    let meeting = parse_id(&meeting)?;
    blocking(move || {
        earshot_runtime::edit::undo_summary(&services, meeting)?;
        Ok(())
    })
    .await
}

/// Exports `meeting` to the Obsidian vault and subfolder of the Obsidian settings
/// (`no_vault` while none is set).
#[tauri::command]
#[specta::specta]
async fn export_meeting(
    state: State<'_, AppState>,
    meeting: String,
) -> Result<ExportDone, IpcError> {
    let services = state.services()?;
    let meeting = parse_id(&meeting)?;
    blocking(move || detail::export(&services, meeting)).await
}

/// This computer's hardware as read (processor, graphics, NPU, memory) and the parts this
/// build runs models on, best first.
#[tauri::command]
#[specta::specta]
async fn hardware() -> Result<HardwareView, IpcError> {
    blocking(|| Ok(crate::models::this_hardware())).await
}

/// The Models page: every model with its size, languages, install state and where it would
/// run, the four transcription choices, the recommendations, the stored check and what the
/// app uses now.
#[tauri::command]
#[specta::specta]
async fn models(
    state: State<'_, AppState>,
    jobs: State<'_, ModelJobs>,
    cuda: State<'_, Arc<crate::cuda_settings::CudaLatch>>,
) -> Result<ModelsView, IpcError> {
    let services = state.services()?;
    let jobs = jobs.inner().clone();
    let cuda = Arc::clone(cuda.inner());
    blocking(move || {
        let cuda = cuda.current();
        let models = earshot_runtime::host::model_dir(&services.data);
        let order = earshot_runtime::backends::accelerator_order();
        let mut view = crate::models::models_view(
            &services,
            &models,
            &earshot_runtime::hardware(),
            &order,
            &|m| jobs.downloading(m),
        )?;
        crate::cuda_settings::apply_start(&mut view, &cuda);
        Ok(view)
    })
    .await
}

/// The Models page settings (and stored values this version does not know).
#[tauri::command]
#[specta::specta]
async fn model_settings(state: State<'_, AppState>) -> Result<ModelSettingsView, IpcError> {
    let services = state.services()?;
    blocking(move || crate::models::settings_view(&services)).await
}

/// Checks and stores the Models page settings (`invalid_setting`: nothing stored).
#[tauri::command]
#[specta::specta]
async fn set_model_settings(
    state: State<'_, AppState>,
    settings: ModelSettingsValue,
) -> Result<ModelSettingsView, IpcError> {
    let services = state.services()?;
    blocking(move || crate::models::set_settings(&services, &settings)).await
}

/// Starts downloading `model` (`ModelJobEvent`s follow). Fails with `busy` while it
/// downloads, `unknown_model` for a name the catalogue does not have.
#[tauri::command]
#[specta::specta]
fn start_download(
    state: State<'_, AppState>,
    jobs: State<'_, ModelJobs>,
    model: String,
) -> Result<(), IpcError> {
    let services = state.services()?;
    jobs.start_download(&services, &model)
}

/// Cancels the download of `model`; what arrived is kept, so a new start resumes.
#[tauri::command]
#[specta::specta]
fn cancel_download(jobs: State<'_, ModelJobs>, model: String) -> Result<(), IpcError> {
    jobs.cancel_download(&model)
}

/// Removes `model`'s files; returns the bytes freed. Fails with `model_in_use` for a chosen
/// or kept model and `busy` while it downloads.
#[tauri::command]
#[specta::specta]
async fn remove_model(
    state: State<'_, AppState>,
    jobs: State<'_, ModelJobs>,
    model: String,
) -> Result<RemovedView, IpcError> {
    let services = state.services()?;
    let jobs = jobs.inner().clone();
    blocking(move || {
        let models = earshot_runtime::host::model_dir(&services.data);
        crate::models::remove(
            &services,
            &models,
            &earshot_runtime::hardware(),
            &earshot_runtime::backends::accelerator_order(),
            &jobs,
            &model,
        )
    })
    .await
}

/// Starts the model check in its own process (`ModelJobEvent`s follow); `busy` while one
/// runs, or a meeting or a meeting job runs.
#[tauri::command]
#[specta::specta]
fn run_check(state: State<'_, AppState>, jobs: State<'_, ModelJobs>) -> Result<(), IpcError> {
    let services = state.services()?;
    jobs.start_check(&services)
}

/// Cancels the running model check (its process is stopped; a `Failed` follows).
#[tauri::command]
#[specta::specta]
fn cancel_check(jobs: State<'_, ModelJobs>) -> Result<(), IpcError> {
    jobs.cancel_check()
}

/// The Summaries page settings (and stored values this version does not know).
#[tauri::command]
#[specta::specta]
async fn summary_settings(state: State<'_, AppState>) -> Result<SummarySettingsView, IpcError> {
    let services = state.services()?;
    blocking(move || crate::summaries::settings_view(&services)).await
}

/// Checks and stores the Summaries page settings (`invalid_setting`: nothing stored).
#[tauri::command]
#[specta::specta]
async fn set_summary_settings(
    state: State<'_, AppState>,
    settings: SummarySettingsValue,
) -> Result<SummarySettingsView, IpcError> {
    let services = state.services()?;
    blocking(move || crate::summaries::set_settings(&services, &settings)).await
}

/// Stores the Anthropic API key in Windows Credential Manager (`invalid_setting` for
/// something that is not a key; `api_key` when Credential Manager fails). The key is never
/// sent back to the UI.
#[tauri::command]
#[specta::specta]
async fn set_api_key(state: State<'_, AppState>, key: String) -> Result<(), IpcError> {
    let keys = Arc::clone(&state.keys);
    blocking(move || {
        let stored = crate::summaries::set_api_key(keys.as_ref(), &key);
        crate::credentials::wipe_string(key);
        stored
    })
    .await
}

/// Whether an Anthropic API key is stored (the key itself is never returned).
#[tauri::command]
#[specta::specta]
async fn has_api_key(state: State<'_, AppState>) -> Result<bool, IpcError> {
    let keys = Arc::clone(&state.keys);
    blocking(move || crate::summaries::has_api_key(keys.as_ref())).await
}

/// Removes the stored Anthropic API key.
#[tauri::command]
#[specta::specta]
async fn remove_api_key(state: State<'_, AppState>) -> Result<(), IpcError> {
    let keys = Arc::clone(&state.keys);
    blocking(move || crate::summaries::remove_api_key(keys.as_ref())).await
}

/// Whether Codex is installed and signed in (T7.4gc), with how to fix it. Runs the CLI's
/// own status checks (never a sign-in) on a blocking thread.
#[tauri::command]
#[specta::specta]
async fn codex_status() -> Result<crate::summaries::CodexStatusView, IpcError> {
    blocking(|| {
        Ok(crate::summaries::codex_status(
            &earshot_runtime::codex::InstalledCodex::status_page(),
        ))
    })
    .await
}

/// Whether Claude Code is installed and signed in (T7.4ga), with how to fix it. Runs the
/// CLI's own status checks (never a sign-in) on a blocking thread.
#[tauri::command]
#[specta::specta]
async fn claude_code_status() -> Result<crate::summaries::ClaudeCodeStatusView, IpcError> {
    blocking(|| {
        Ok(crate::summaries::claude_code_status(
            &earshot_runtime::claude_code::InstalledClaudeCode::status_page(),
        ))
    })
    .await
}

/// The compact listening card's window size (logical pixels).
pub const COMPACT_SIZE: (f64, f64) = (300.0, 170.0);

/// The window operations [`CompactWindow`] needs (the Tauri window, or a fake in tests).
pub trait WindowOps {
    /// The inner size (physical pixels).
    fn size(&self) -> Result<(u32, u32), IpcError>;
    /// Whether the window is maximised.
    fn maximized(&self) -> Result<bool, IpcError>;
    /// Restores a maximised window.
    fn unmaximize(&self) -> Result<(), IpcError>;
    /// Maximises the window.
    fn maximize(&self) -> Result<(), IpcError>;
    /// Sets the inner size in physical pixels.
    fn set_physical_size(&self, size: (u32, u32)) -> Result<(), IpcError>;
    /// Sets the inner size in logical pixels.
    fn set_logical_size(&self, size: (f64, f64)) -> Result<(), IpcError>;
    /// Sets (or clears) the minimum inner size in logical pixels.
    fn set_min_size(&self, size: Option<(f64, f64)>) -> Result<(), IpcError>;
    /// Keeps the window above others, or not.
    fn set_always_on_top(&self, on: bool) -> Result<(), IpcError>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Restore {
    size: (u32, u32),
    maximized: bool,
}

/// Switches the main window between the full app and the compact listening card (D21),
/// remembering the full window's size to go back to.
#[derive(Debug, Default)]
pub struct CompactWindow {
    saved: Mutex<Option<Restore>>,
    /// The window's configured minimum size (logical), restored on expand.
    min_size: Option<(f64, f64)>,
}

impl CompactWindow {
    /// With the window's configured minimum size.
    #[must_use]
    pub fn new(min_size: Option<(f64, f64)>) -> Self {
        Self {
            saved: Mutex::new(None),
            min_size,
        }
    }

    /// Compact (a small always-on-top card of [`COMPACT_SIZE`]) or back to the size before.
    /// Repeating the current state changes nothing.
    ///
    /// # Errors
    /// Window errors.
    pub fn set(&self, window: &dyn WindowOps, compact: bool) -> Result<(), IpcError> {
        let mut saved = self.saved.lock().unwrap_or_else(PoisonError::into_inner);
        if compact {
            if saved.is_some() {
                return Ok(());
            }
            let restore = Restore {
                size: window.size()?,
                maximized: window.maximized()?,
            };
            if restore.maximized {
                window.unmaximize()?;
            }
            window.set_min_size(None)?;
            window.set_logical_size(COMPACT_SIZE)?;
            window.set_always_on_top(true)?;
            *saved = Some(restore);
        } else if let Some(restore) = saved.take() {
            window.set_always_on_top(false)?;
            window.set_min_size(self.min_size)?;
            window.set_physical_size(restore.size)?;
            if restore.maximized {
                window.maximize()?;
            }
        }
        Ok(())
    }
}

fn window_error(e: tauri::Error) -> IpcError {
    IpcError::new("window", e.to_string())
}

impl WindowOps for tauri::WebviewWindow {
    fn size(&self) -> Result<(u32, u32), IpcError> {
        let s = self.inner_size().map_err(window_error)?;
        Ok((s.width, s.height))
    }
    fn maximized(&self) -> Result<bool, IpcError> {
        self.is_maximized().map_err(window_error)
    }
    fn unmaximize(&self) -> Result<(), IpcError> {
        tauri::WebviewWindow::unmaximize(self).map_err(window_error)
    }
    fn maximize(&self) -> Result<(), IpcError> {
        tauri::WebviewWindow::maximize(self).map_err(window_error)
    }
    fn set_physical_size(&self, (w, h): (u32, u32)) -> Result<(), IpcError> {
        self.set_size(tauri::PhysicalSize::new(w, h))
            .map_err(window_error)
    }
    fn set_logical_size(&self, (w, h): (f64, f64)) -> Result<(), IpcError> {
        self.set_size(tauri::LogicalSize::new(w, h))
            .map_err(window_error)
    }
    fn set_min_size(&self, size: Option<(f64, f64)>) -> Result<(), IpcError> {
        tauri::WebviewWindow::set_min_size(self, size.map(|(w, h)| tauri::LogicalSize::new(w, h)))
            .map_err(window_error)
    }
    fn set_always_on_top(&self, on: bool) -> Result<(), IpcError> {
        tauri::WebviewWindow::set_always_on_top(self, on).map_err(window_error)
    }
}

/// Shows the main window as the compact listening card (`true`) or the full app (`false`).
#[tauri::command]
#[specta::specta]
fn set_compact(
    window: tauri::WebviewWindow,
    compact_window: State<'_, CompactWindow>,
    compact: bool,
) -> Result<(), IpcError> {
    compact_window.set(&window, compact)
}

/// A theme the window's title bar can be put on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Type)]
#[serde(rename_all = "lowercase")]
pub enum WindowTheme {
    /// Light.
    Light,
    /// Dark.
    Dark,
}

/// Puts the native title bar on the theme chosen in Settings, General; `None` follows the system.
/// The choice itself lives in the page (T9.1n).
#[tauri::command]
#[specta::specta]
fn set_window_theme(
    window: tauri::WebviewWindow,
    theme: Option<WindowTheme>,
) -> Result<(), IpcError> {
    window
        .set_theme(theme.map(|t| match t {
            WindowTheme::Light => tauri::Theme::Light,
            WindowTheme::Dark => tauri::Theme::Dark,
        }))
        .map_err(|e| IpcError {
            kind: "window".into(),
            message: e.to_string(),
            retryable: false,
        })
}

#[cfg(test)]
#[path = "ipc_tests.rs"]
mod tests;
