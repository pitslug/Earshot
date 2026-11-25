//! The tray, the global shortcut and the OS notifications (T7.5).
//!
//! Earshot lives in the tray: closing the window hides it (always, not only during a meeting,
//! so the shortcut and a meeting's last steps keep running), minimising it leaves it on the
//! taskbar, and only "Quit Earshot" in the tray menu ends the process. Started by
//! Windows (`--autostart`, the `Run` value of [`crate::general`]), no window opens at all.
//!
//! Quitting while a meeting records asks first, in the app's window ([`ShellEvent::QuitAsked`]),
//! then [`quit_app`] stops the meeting and waits for the recording to be finalised before the
//! process exits. A final pass or summary still running is cut: the meeting stays
//! `Unprocessed` and can be re-transcribed from its page.
//!
//! The shortcut and the tray start and stop meetings in Rust, not through the UI, so they work
//! with the window hidden; the UI follows through the same `MeetingEvent`s. A start that fails
//! opens the window and shows why ([`ShellEvent::StartFailed`]).
//!
//! A notification never names a meeting: Windows keeps notifications in its own database and
//! shows them on the lock screen, so a title would outlive the meeting's deletion
//! ([`summary_toast`] is not given one). They are silent, so none is heard by a microphone.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use serde::Serialize;
use specta::Type;
use tauri::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Manager, State, Window, WindowEvent};
use tauri_specta::Event;

use crate::ipc::{AppState, IpcError};
use crate::meeting::{DriverPhase, MeetingDriver, StartRequest};
use crate::meeting_events::{EndedReason, MeetingEvent, SummaryEvent};
use crate::sync_settings::{QUIT_GONE_WAIT, SyncHost};

/// The argument of the `Run` value: started by Windows, stay in the tray.
pub const AUTOSTART_ARG: &str = "--autostart";
/// The global shortcut, as the page and the notifications write it.
pub const SHORTCUT_TEXT: &str = "Ctrl+Alt+Shift+R";
/// The label of the main window (`tauri.conf.json`).
const MAIN_WINDOW: &str = "main";

/// Whether this process was started by Windows at sign-in.
#[must_use]
pub fn started_by_windows(args: &[std::ffi::OsString]) -> bool {
    args.iter().any(|a| a == AUTOSTART_ARG)
}

/// How long the app holds up the end of the process to save a recording (T9.19): Windows shows
/// its "apps are preventing restart" screen after about five seconds and may end the process
/// any time after. A judgement; what is not saved by then start-up recovery (T9.18) saves.
/// It bounds only this wait: stopping capture joins the capture thread with no bound of its
/// own (T9.20 adds one), so a stalled device makes the limit expire and recovery takes over.
pub const EXIT_SAVE_LIMIT: std::time::Duration = std::time::Duration::from_secs(4);

/// Saves a recording in progress before the process ends (T9.19): on Tauri's last event, which
/// a Windows restart, shut down or sign-out also sends. The recording stops and its files are
/// finished, waiting at most [`EXIT_SAVE_LIMIT`]; the final pass and summary are cut and the
/// meeting stays Unprocessed, as tray Quit does. While it saves, Windows is told why Earshot
/// holds up the restart. Nothing happens when nothing records (tray Quit has already stopped).
///
/// With sync on, the other computers are told this one is gone (T11.3ca): the send starts
/// first on a thread of its own and is waited for last, at most [`QUIT_GONE_WAIT`] and never
/// past [`EXIT_SAVE_LIMIT`] from the start; an unfinished send is cut by the process ending.
pub fn save_before_exit(app: &AppHandle) {
    let started = std::time::Instant::now();
    let gone = app
        .try_state::<Arc<SyncHost>>()
        .and_then(|host| host.say_gone());
    save_recording_before_exit(app);
    if let Some(rx) = gone {
        let _ = rx.recv_timeout(gone_wait(started.elapsed()));
    }
}

/// How long quitting still waits for the gone presence after `spent` on saving a recording.
#[must_use]
pub fn gone_wait(spent: std::time::Duration) -> std::time::Duration {
    EXIT_SAVE_LIMIT.saturating_sub(spent).min(QUIT_GONE_WAIT)
}

fn save_recording_before_exit(app: &AppHandle) {
    let Some(driver) = app.try_state::<MeetingDriver>() else {
        return;
    };
    if driver.phase() != DriverPhase::Recording {
        return;
    }
    #[cfg(windows)]
    let _reason = app
        .get_webview_window(MAIN_WINDOW)
        .and_then(|w| w.hwnd().ok())
        .map(crate::shutdown_win::BlockReason::set);
    let _ = driver.stop_and_wait_for(EXIT_SAVE_LIMIT);
}

/// Asks the UI to do something the tray, the shortcut or a notification cannot do alone.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Type, tauri_specta::Event)]
#[serde(tag = "type")]
pub enum ShellEvent {
    /// Show this meeting (a notification was clicked).
    OpenMeeting {
        /// Meeting id.
        meeting: String,
        /// Open its Speakers panel too.
        name_voices: bool,
    },
    /// "Quit Earshot" during a meeting: ask first, then call `quit_app`.
    QuitAsked {
        /// True while the meeting records; false while its last steps run.
        recording: bool,
    },
    /// A meeting started from the tray or the shortcut could not start (nothing was recorded).
    StartFailed {
        /// Why.
        message: String,
    },
    /// Start-up recovery gave an interrupted recording its audio back (T9.18): read the
    /// notices (`recovered_recordings`) and show them.
    RecordingsRecovered,
    /// The question "a call started: start recording?" opened or closed (T12.86). Answered with
    /// `answer_call_start`.
    CallStartOffer {
        /// The question is open.
        open: bool,
    },
}

/// Where the current meeting is, as the tray shows it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    /// No meeting.
    Idle,
    /// The tray or the shortcut asked for a meeting; its checks and models are loading.
    Starting,
    /// Recording.
    Recording,
    /// The recording is saved; the final pass and the summary run. A new meeting may start.
    Finishing,
}

/// Follows a meeting's events.
#[derive(Debug, Clone)]
pub struct PhaseTracker {
    phase: Phase,
    meeting: Option<String>,
    /// The phase before the tray or the shortcut asked for a meeting.
    before_start: Phase,
    /// The meeting the tray or the shortcut started, from the driver's answer
    /// ([`PhaseTracker::start_reported`]), until its `Started` arrives (T14.5): one that ends with
    /// no `Started` (its thread stopped first) still ends the start.
    reported: Option<String>,
    /// The meetings whose last event arrived while a start was on its way and that were not the
    /// one followed: the driver's answer can come after the meeting's own last event.
    ended_while_starting: Vec<String>,
}

impl Default for PhaseTracker {
    fn default() -> Self {
        Self {
            phase: Phase::Idle,
            meeting: None,
            before_start: Phase::Idle,
            reported: None,
            ended_while_starting: Vec::new(),
        }
    }
}

impl PhaseTracker {
    /// Applies `event`; true when the phase changed. An older meeting finishing behind a newer one
    /// changes nothing.
    pub fn apply(&mut self, event: &MeetingEvent) -> bool {
        let next = match event {
            MeetingEvent::Started { meeting, .. } => {
                self.meeting = Some(meeting.clone());
                Phase::Recording
            }
            MeetingEvent::Ended { meeting, .. } if self.follows(meeting) => Phase::Finishing,
            // The meeting this start made, ended with no `Started`: followed from here on.
            MeetingEvent::Ended { meeting, .. } if self.reported(meeting) => {
                self.meeting = self.reported.take();
                Phase::Finishing
            }
            // The same with no end either (its thread stopped): the start is over.
            MeetingEvent::Summary { meeting, .. } if self.reported(meeting) => self.before_start,
            MeetingEvent::Summary { meeting, .. } if self.follows(meeting) => {
                if self.phase == Phase::Starting {
                    self.before_start = Phase::Idle;
                    Phase::Starting
                } else {
                    Phase::Idle
                }
            }
            MeetingEvent::Summary { meeting, .. } if self.phase == Phase::Starting => {
                self.ended_while_starting.push(meeting.clone());
                Phase::Starting
            }
            _ => self.phase,
        };
        self.moved_to(next)
    }

    /// Sets the phase; true when it changed. What belongs to a start goes with it.
    fn moved_to(&mut self, next: Phase) -> bool {
        if next != Phase::Starting {
            self.reported = None;
            self.ended_while_starting.clear();
        }
        let changed = next != self.phase;
        self.phase = next;
        changed
    }

    fn reported(&self, meeting: &str) -> bool {
        self.reported.as_deref() == Some(meeting)
    }

    /// The driver answered the tray's or the shortcut's start with `meeting` (T14.5); true when
    /// the phase changed. Its `Started` usually came first and this changes nothing. Without one,
    /// the meeting's end or its last event now ends the start, also when that event came before
    /// this answer.
    pub fn start_reported(&mut self, meeting: &str) -> bool {
        if self.phase != Phase::Starting {
            return false;
        }
        if self.ended_while_starting.iter().any(|m| m == meeting) {
            return self.moved_to(self.before_start);
        }
        self.reported = Some(meeting.to_owned());
        false
    }

    /// The phase.
    #[must_use]
    pub fn phase(&self) -> Phase {
        self.phase
    }

    fn follows(&self, meeting: &str) -> bool {
        self.meeting.as_deref() == Some(meeting)
    }

    /// The last meeting that started.
    #[cfg(test)]
    #[must_use]
    pub fn meeting(&self) -> Option<&str> {
        self.meeting.as_deref()
    }

    /// The tray or shortcut asked for a meeting. False when one is on its way or recording.
    pub fn begin_start(&mut self) -> bool {
        if matches!(self.phase, Phase::Idle | Phase::Finishing) {
            self.before_start = self.phase;
            self.phase = Phase::Starting;
            true
        } else {
            false
        }
    }

    /// The start failed (ignored once the meeting's events moved the phase on): back to what the
    /// last meeting was doing.
    pub fn start_failed(&mut self) {
        if self.phase == Phase::Starting {
            self.moved_to(self.before_start);
        }
    }
}

/// How the tray looks in a phase.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TrayView {
    /// The icon carries the red recording dot.
    pub recording: bool,
    /// The tooltip (never a meeting's title).
    pub tooltip: &'static str,
    /// The start/stop menu item.
    pub toggle_label: &'static str,
    /// Whether that item can be used.
    pub toggle_enabled: bool,
}

/// The tray in `phase`.
#[must_use]
pub fn tray_view(phase: Phase) -> TrayView {
    match phase {
        Phase::Idle => TrayView {
            recording: false,
            tooltip: "Earshot",
            toggle_label: "Start meeting",
            toggle_enabled: true,
        },
        Phase::Starting => TrayView {
            recording: false,
            tooltip: "Earshot: starting a meeting",
            toggle_label: "Starting a meeting",
            toggle_enabled: false,
        },
        Phase::Recording => TrayView {
            recording: true,
            tooltip: "Earshot: listening",
            toggle_label: "Stop meeting",
            toggle_enabled: true,
        },
        Phase::Finishing => TrayView {
            recording: false,
            tooltip: "Earshot: finishing the meeting",
            toggle_label: "Start meeting",
            toggle_enabled: true,
        },
    }
}

/// What the tray's item or the shortcut does now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToggleAction {
    /// Start a meeting.
    Start,
    /// Stop the recording.
    Stop,
    /// Nothing: a meeting is starting (T14.6).
    Nothing,
}

/// [`ToggleAction`] from what the driver says. `starting`: the meeting it calls recording has
/// not sent its `Started` yet. Its pre-flight can take seconds with no window showing it, and a
/// second press then must not stop the meeting the moment it starts (T14.6), so it does nothing.
/// The window's own Stop is the driver's and is not decided here.
#[must_use]
pub fn toggle_action(phase: DriverPhase, starting: bool) -> ToggleAction {
    match phase {
        DriverPhase::Idle | DriverPhase::Processing => ToggleAction::Start,
        DriverPhase::Recording if starting => ToggleAction::Nothing,
        DriverPhase::Recording => ToggleAction::Stop,
    }
}

/// A press of the tray's item or the shortcut, as far as `driver` goes: the recording is asked
/// to stop here; a start is the shell's to make, on a thread of its own.
pub fn press(driver: &MeetingDriver) -> ToggleAction {
    let (phase, starting) = driver.phase_with_starting();
    let action = toggle_action(phase, starting);
    if action == ToggleAction::Stop {
        // It may have just ended by itself; the UI shows that either way.
        let _ = driver.stop();
    }
    action
}

/// What "Quit Earshot" does now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuitAction {
    /// Nothing to lose: quit.
    Quit,
    /// Ask first ([`ShellEvent::QuitAsked`]).
    Ask {
        /// The meeting records.
        recording: bool,
    },
}

/// [`QuitAction`] from what the driver says.
#[must_use]
pub fn quit_action(phase: DriverPhase) -> QuitAction {
    match phase {
        DriverPhase::Idle => QuitAction::Quit,
        DriverPhase::Recording => QuitAction::Ask { recording: true },
        DriverPhase::Processing => QuitAction::Ask { recording: false },
    }
}

/// Lets one key press through: Windows repeats a hotkey while it is held, and each repeat
/// would otherwise start or stop a meeting.
#[derive(Debug, Default)]
pub struct KeyLatch {
    held: bool,
}

impl KeyLatch {
    /// A press arrived; true when it counts.
    pub fn pressed(&mut self) -> bool {
        !std::mem::replace(&mut self.held, true)
    }

    /// The key was released.
    pub fn released(&mut self) {
        self.held = false;
    }
}

/// What a notification button does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToastClick {
    /// Open the window.
    Window,
    /// Open the window on the meeting.
    Meeting,
    /// Open the window on the meeting's Speakers panel.
    NameVoices,
    /// Stop the recording: the answer to the call-end question, with the window left alone.
    StopRecording,
    /// Keep recording: the other answer to it.
    KeepRecording,
    /// Start recording: the answer to the call-start question (T12.86).
    StartRecording,
    /// Not now: the other answer to it.
    NotNow,
}

impl ToastClick {
    /// The activation argument in the notification's XML.
    #[must_use]
    pub fn argument(self) -> &'static str {
        match self {
            Self::Window => "window",
            Self::Meeting => "meeting",
            Self::NameVoices => "names",
            Self::StopRecording => "stop",
            Self::KeepRecording => "keep",
            Self::StartRecording => "start",
            Self::NotNow => "not-now",
        }
    }

    /// The click an activation argument stands for (anything else opens the window).
    #[must_use]
    pub fn from_argument(argument: &str) -> Self {
        match argument {
            "meeting" => Self::Meeting,
            "names" => Self::NameVoices,
            "stop" => Self::StopRecording,
            "keep" => Self::KeepRecording,
            "start" => Self::StartRecording,
            "not-now" => Self::NotNow,
            _ => Self::Window,
        }
    }
}

/// One notification. Its text is fixed copy and counts, never meeting content (module docs).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Toast {
    /// First line.
    pub title: String,
    /// Second line.
    pub body: String,
    /// What a click on its body does.
    pub click: ToastClick,
    /// Its buttons.
    pub buttons: Vec<(&'static str, ToastClick)>,
    /// It stays on screen until answered or taken back, rather than going by itself.
    pub stays: bool,
}

fn counted(n: u32, one: &str, many: &str) -> String {
    if n == 1 {
        format!("1 {one}")
    } else {
        format!("{n} {many}")
    }
}

/// The "summary ready" notification.
#[must_use]
pub fn summary_toast(actions: u32, decisions: u32, voices_to_name: u32) -> Toast {
    let mut body = format!(
        "{}, {}.",
        counted(actions, "action", "actions"),
        counted(decisions, "decision", "decisions")
    );
    let mut buttons = vec![("Open", ToastClick::Meeting)];
    if voices_to_name > 0 {
        let voices = if voices_to_name == 1 {
            "1 voice needs a name.".to_owned()
        } else {
            format!("{voices_to_name} voices need names.")
        };
        body.push(' ');
        body.push_str(&voices);
        buttons.push(("Name voices", ToastClick::NameVoices));
    }
    Toast {
        title: "Summary ready".into(),
        body,
        click: ToastClick::Meeting,
        buttons,
        stays: false,
    }
}

/// The notification when a meeting ends with `result`; `None` when there is nothing to say (no
/// summary was asked for, or it was cancelled). A failure's own text stays in the app.
#[must_use]
pub fn finished_toast(result: &SummaryEvent) -> Option<Toast> {
    match result {
        SummaryEvent::Stored {
            actions,
            decisions,
            voices_to_name,
            ..
        } => Some(summary_toast(*actions, *decisions, *voices_to_name)),
        SummaryEvent::Failed { .. } => Some(Toast {
            title: "The summary was not made".into(),
            body: "Open the meeting to see why.".into(),
            click: ToastClick::Meeting,
            buttons: vec![("Open", ToastClick::Meeting)],
            stays: false,
        }),
        SummaryEvent::RecordingFailed { .. } => Some(Toast {
            title: "The recording stopped".into(),
            body: "Open the meeting to see why.".into(),
            click: ToastClick::Meeting,
            buttons: vec![("Open", ToastClick::Meeting)],
            stays: false,
        }),
        SummaryEvent::PostFailed { .. } => Some(Toast {
            title: "The transcript was not finished".into(),
            body: "Open the meeting to see why and to re-transcribe it.".into(),
            click: ToastClick::Meeting,
            buttons: vec![("Open", ToastClick::Meeting)],
            stays: false,
        }),
        // Another computer writes it (T11.3db): nothing to open yet.
        SummaryEvent::Skipped { .. }
        | SummaryEvent::Cancelled
        | SummaryEvent::PostCancelled
        | SummaryEvent::Offloaded { .. } => None,
    }
}

/// The notification when a job on a stored meeting ends, and the meeting its clicks open: a
/// re-summary says what a meeting's own summary says ([`finished_toast`]), and a re-summarise job
/// that failed before it ran says the summary was not made. Nothing for a re-transcription or a
/// job's steps.
#[must_use]
pub fn job_toast(event: &crate::jobs::JobEvent) -> Option<(Toast, String)> {
    use crate::jobs::{JobEvent, JobKind};
    match event {
        JobEvent::Summary {
            meeting, result, ..
        } => finished_toast(result).map(|t| (t, meeting.clone())),
        JobEvent::Failed {
            meeting,
            job: JobKind::Resummarise,
            ..
        } => finished_toast(&SummaryEvent::Failed {
            message: String::new(),
            retryable: false,
        })
        .map(|t| (t, meeting.clone())),
        _ => None,
    }
}

/// The notification that an automatic check found `version` (T12.22): fixed copy and the version
/// number, which is all the check knows.
#[must_use]
pub fn update_toast(version: &str) -> Toast {
    Toast {
        title: "An update is available".into(),
        body: format!("Earshot {version} can be downloaded from Settings, General."),
        click: ToastClick::Window,
        buttons: Vec::new(),
        stays: false,
    }
}

/// The notification when a meeting started with no window on screen: someone is being
/// recorded, and a tray icon alone can be hidden in the overflow.
#[must_use]
pub fn listening_toast() -> Toast {
    Toast {
        title: "Earshot is listening".into(),
        body: format!("Press {SHORTCUT_TEXT} to stop."),
        click: ToastClick::Window,
        buttons: Vec::new(),
        stays: false,
    }
}

/// The notification when a recording ends for `reason` with no window on screen, so a stop by the
/// shortcut is confirmed before the summary arrives; `None` after the call-end stop or a meeting that
/// stopped because no track could record, whose own notification already said it
/// ([`call_end_stopped_toast`], [`recording_stopped_toast`]). Fixed copy, never meeting content.
#[must_use]
pub fn ended_toast(reason: EndedReason, already_told: bool) -> Option<Toast> {
    if already_told {
        return None;
    }
    let (title, body) = match reason {
        EndedReason::Stopped | EndedReason::Finished => (
            "Recording saved",
            "Earshot is finishing the meeting in the background.",
        ),
        EndedReason::Failed => (
            "Recording stopped",
            "Something went wrong. What was recorded is kept: open Earshot to see why.",
        ),
    };
    Some(Toast {
        title: title.into(),
        body: body.into(),
        click: ToastClick::Window,
        buttons: Vec::new(),
        stays: false,
    })
}

/// The notification that the call seems to have ended (T9.4): fixed copy, and it stays on screen
/// until it is answered or taken back.
#[must_use]
pub fn call_end_toast() -> Toast {
    Toast {
        title: "The call seems to have ended".into(),
        body: "Earshot will stop recording within 3 minutes unless you keep it going.".into(),
        click: ToastClick::Window,
        buttons: vec![
            ("Stop recording", ToastClick::StopRecording),
            ("Keep recording", ToastClick::KeepRecording),
        ],
        stays: true,
    }
}

/// The notification that a call seems to have started (T12.86): fixed copy, never the call app's
/// name or anything about the call, and it stays on screen until answered or taken back.
#[must_use]
pub fn call_start_toast() -> Toast {
    Toast {
        title: "A call seems to have started".into(),
        body: "Do you want Earshot to record it?".into(),
        click: ToastClick::Window,
        buttons: vec![
            ("Start recording", ToastClick::StartRecording),
            ("Not now", ToastClick::NotNow),
        ],
        stays: true,
    }
}

/// What a click answers on the call-start question: `Some(true)` start, `Some(false)` not now.
/// `None` for any other click, and for one on a question that has closed: a late click must never
/// start a recording.
#[must_use]
pub fn call_start_answer(click: ToastClick, open: bool) -> Option<bool> {
    match click {
        ToastClick::StartRecording if open => Some(true),
        ToastClick::NotNow if open => Some(false),
        _ => None,
    }
}

/// The call-start reading of one microphone poll (T12.86): `None` when the poller has nothing
/// new.
#[must_use]
pub fn mic_reading(
    latest: Option<Result<Vec<earshot_capture_wasapi::SessionPeak>, String>>,
) -> Option<earshot_runtime::call_start::MicReading> {
    use earshot_runtime::call_start::MicReading;
    latest.map(|r| match r {
        Ok(sessions) if earshot_capture_wasapi::dedicated_call_app_has_mic(&sessions) => {
            MicReading::CallApp
        }
        Ok(_) => MicReading::NoCallApp,
        Err(_) => MicReading::Unknown,
    })
}

/// How often the call-start watch looks at its poller and the setting (the poller itself reads
/// every [`earshot_capture_wasapi::session::CALL_POLL_INTERVAL`]).
const CALL_START_STEP: std::time::Duration = std::time::Duration::from_millis(500);

/// The General page's "offer to record when a call starts" as the call-start watch reads it
/// (T14.9): kept from its first read and from every change, so the watch does not take the
/// store's lock twice a second for a value that changes when the user changes it. The setting
/// is device-local and has one writer (`call_start::set_offer_on`, from the General page's
/// command, which tells this cache).
#[derive(Debug, Default)]
pub struct OfferSetting(Mutex<Option<bool>>);

impl OfferSetting {
    fn kept(&self) -> std::sync::MutexGuard<'_, Option<bool>> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The setting: the value kept, else what `read` gives (the store), kept from then on. A
    /// read that fails (`None`: no store) is off for now and is read again next time. The lock
    /// is not held across `read`; a change made meanwhile wins.
    pub fn get(&self, read: impl FnOnce() -> Option<bool>) -> bool {
        if let Some(on) = *self.kept() {
            return on;
        }
        let read = read();
        let mut kept = self.kept();
        if kept.is_none() {
            *kept = read;
        }
        kept.unwrap_or(false)
    }

    /// The setting was stored as `on`.
    pub fn set(&self, on: bool) {
        *self.kept() = Some(on);
    }
}

/// The notification that Earshot started in the tray and could not open its data (T14.9): with
/// no window on screen nothing else says so, and every meeting would fail to start. Fixed copy;
/// the window says why.
#[must_use]
pub fn data_unopened_toast() -> Toast {
    Toast {
        title: "Earshot could not open its data".into(),
        body: "Meetings cannot be recorded until this is fixed. Open Earshot to see why.".into(),
        click: ToastClick::Window,
        buttons: Vec::new(),
        stays: false,
    }
}

/// What a start says in a notification: [`data_unopened_toast`] when the store failed to open
/// and no window shows it (started by Windows, in the tray). With a window, the window says it.
#[must_use]
pub fn start_toast(shows_window: bool, store_failed: bool) -> Option<Toast> {
    (store_failed && !shows_window).then(data_unopened_toast)
}

/// The notification that nobody answered and the recording stopped by itself.
#[must_use]
pub fn call_end_stopped_toast() -> Toast {
    Toast {
        title: "Earshot stopped recording".into(),
        body: "The call seemed to have ended, so Earshot is finishing the meeting.".into(),
        click: ToastClick::Window,
        buttons: Vec::new(),
        stays: false,
    }
}

/// The notification that every track stopped recording and the meeting stopped itself (T9.17):
/// shown whether or not the window is watched, because nothing more is being recorded. Fixed
/// copy, never meeting content.
#[must_use]
pub fn recording_stopped_toast() -> Toast {
    Toast {
        title: "Earshot stopped recording".into(),
        body: "It could not save any more audio. What was recorded is kept: open Earshot to see \
               why."
            .into(),
        click: ToastClick::Window,
        buttons: Vec::new(),
        stays: false,
    }
}

/// The notification `event` asks for whatever the window shows: only the banner of a meeting
/// that stopped itself because no track could record ([`recording_stopped_toast`]).
#[must_use]
pub fn always_shown_toast(event: &MeetingEvent) -> Option<Toast> {
    match event {
        MeetingEvent::Warning { key, .. }
            if key == crate::meeting_events::RECORDING_STOPPED_KEY =>
        {
            Some(recording_stopped_toast())
        }
        _ => None,
    }
}

/// Whether the call-end question is open after `event`, with the shell's phase already updated by
/// it and `was` the state before. A question can be open only while a meeting records, so every way
/// out of recording closes it, a meeting thread's panic (a failed summary and no end) included. A
/// summary of an older meeting while a newer one records changes nothing.
#[must_use]
pub fn call_end_open_after(event: &MeetingEvent, phase: Phase, was: bool) -> bool {
    if phase != Phase::Recording {
        return false;
    }
    match event {
        MeetingEvent::CallEndOffer { open } => *open,
        MeetingEvent::CallEndStopped | MeetingEvent::Started { .. } => false,
        _ => was,
    }
}

/// An answer to the call-end question.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CallEndAnswer {
    /// Stop the recording.
    Stop,
    /// Keep recording.
    Keep,
}

/// What a click on the notification answers. `None` for a click that is not an answer, and for one
/// on a notification whose question has closed: it must not stop a later call.
#[must_use]
pub fn call_end_answer(click: ToastClick, open: bool) -> Option<CallEndAnswer> {
    match click {
        ToastClick::StopRecording if open => Some(CallEndAnswer::Stop),
        ToastClick::KeepRecording if open => Some(CallEndAnswer::Keep),
        _ => None,
    }
}

/// `rgba` with the red recording dot in its lower right corner.
#[must_use]
pub fn with_record_dot(rgba: &[u8], width: u32, height: u32) -> Vec<u8> {
    let mut out = rgba.to_vec();
    let size = f64::from(width.min(height));
    let radius = size * 0.26;
    let (cx, cy) = (
        f64::from(width) - radius - size * 0.02,
        f64::from(height) - radius - size * 0.02,
    );
    for (i, pixel) in out.as_chunks_mut::<4>().0.iter_mut().enumerate() {
        let i = u32::try_from(i).unwrap_or(u32::MAX);
        let (x, y) = (f64::from(i % width.max(1)), f64::from(i / width.max(1)));
        if (x + 0.5 - cx).hypot(y + 0.5 - cy) <= radius {
            *pixel = [0xD1, 0x34, 0x38, 0xFF];
        }
    }
    out
}

/// What draws the tray: Tauri's icon and menu item in the app, a fake in tests.
pub trait TraySurface: Send + Sync {
    /// Shows `view`. Called off the main thread, each of Tauri's tray and menu calls waits until
    /// the main thread has run it (tauri 2.12 `run_item_main_thread!`).
    fn show(&self, view: TrayView);
}

/// The tray's surface and the one way to draw on it (T14.5). No lock is held across a call to
/// the surface: a call made off the main thread waits for the main thread, and the main thread
/// draws too (the shortcut, the tray's own menu), so a lock held across the call would have each
/// waiting for the other, with the window and the tray dead.
#[derive(Default)]
pub struct TrayBoard {
    surface: Mutex<Option<Arc<dyn TraySurface>>>,
}

impl TrayBoard {
    /// The tray exists: draw on `surface` from now on.
    pub fn set(&self, surface: Arc<dyn TraySurface>) {
        *self.surface.lock().unwrap_or_else(PoisonError::into_inner) = Some(surface);
    }

    fn surface(&self) -> Option<Arc<dyn TraySurface>> {
        self.surface
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Draws the tray of the phase `phase` reads. With no lock around the drawing, two threads
    /// may draw at once and the older phase may land last, so each draws again until the phase it
    /// drew is still the phase: whichever draws last then drew the phase as it is.
    pub fn refresh(&self, phase: impl Fn() -> Phase) {
        loop {
            let drawn = phase();
            let Some(surface) = self.surface() else {
                return;
            };
            surface.show(tray_view(drawn));
            if phase() == drawn {
                return;
            }
        }
    }
}

/// The tray's parts that change.
struct TrayParts {
    icon: TrayIcon,
    toggle: MenuItem<tauri::Wry>,
    idle: tauri::image::Image<'static>,
    recording: tauri::image::Image<'static>,
}

impl TraySurface for TrayParts {
    fn show(&self, view: TrayView) {
        let image = if view.recording {
            &self.recording
        } else {
            &self.idle
        };
        let _ = self.icon.set_icon(Some(image.clone()));
        let _ = self.icon.set_tooltip(Some(view.tooltip));
        let _ = self.toggle.set_text(view.toggle_label);
        let _ = self.toggle.set_enabled(view.toggle_enabled);
    }
}

/// The shortcut's state, read by the General page.
#[derive(Debug, Default)]
pub struct ShortcutStatus {
    error: Mutex<Option<String>>,
}

impl ShortcutStatus {
    /// Why the shortcut could not be registered.
    #[must_use]
    pub fn error(&self) -> Option<String> {
        self.error
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    fn set_error(&self, message: String) {
        *self.error.lock().unwrap_or_else(PoisonError::into_inner) = Some(message);
    }
}

/// The tray, the shortcut and the notifications of this process.
pub struct Shell {
    app: AppHandle,
    tracker: Mutex<PhaseTracker>,
    tray: TrayBoard,
    latch: Mutex<KeyLatch>,
    has_tray: AtomicBool,
    /// The call-end question is open ([`call_end_open_after`]).
    call_end_open: AtomicBool,
    /// The call-start question is open (T12.86).
    call_start_open: AtomicBool,
    /// The call-start rule, fed by [`Shell::spawn_call_start_watch`] and answered by the user.
    call_start: Mutex<earshot_runtime::call_start::CallStartWatch>,
    /// Whether the call-start offer is on, as last read or stored.
    call_start_setting: OfferSetting,
    /// The call-end offer stopped this recording, or every track stopped recording (T9.17): its own
    /// notification said so, and [`ended_toast`] says nothing more.
    end_already_told: AtomicBool,
    #[cfg(windows)]
    toasts: crate::toast_win::Toaster,
}

impl Shell {
    /// The shell of `app` (no tray until [`Shell::build_tray`]).
    #[must_use]
    pub fn new(app: AppHandle) -> Self {
        Self {
            app,
            tracker: Mutex::new(PhaseTracker::default()),
            tray: TrayBoard::default(),
            latch: Mutex::new(KeyLatch::default()),
            has_tray: AtomicBool::new(false),
            call_end_open: AtomicBool::new(false),
            call_start_open: AtomicBool::new(false),
            call_start: Mutex::new(earshot_runtime::call_start::CallStartWatch::new(
                earshot_runtime::call_start::CallStartConfig::default(),
            )),
            call_start_setting: OfferSetting::default(),
            end_already_told: AtomicBool::new(false),
            #[cfg(windows)]
            toasts: crate::toast_win::Toaster::default(),
        }
    }

    fn tracker(&self) -> std::sync::MutexGuard<'_, PhaseTracker> {
        self.tracker.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn driver(&self) -> MeetingDriver {
        self.app.state::<MeetingDriver>().inner().clone()
    }

    /// Builds the tray icon and its menu.
    ///
    /// # Errors
    /// Tauri's, when the tray cannot be made (the caller opens the window instead).
    pub fn build_tray(self: &Arc<Self>) -> tauri::Result<()> {
        let app = &self.app;
        let view = tray_view(Phase::Idle);
        let open = MenuItem::with_id(app, "open", "Open Earshot", true, None::<&str>)?;
        let toggle = MenuItem::with_id(
            app,
            "toggle",
            view.toggle_label,
            view.toggle_enabled,
            None::<&str>,
        )?;
        let separator = PredefinedMenuItem::separator(app)?;
        let quit = MenuItem::with_id(app, "quit", "Quit Earshot", true, None::<&str>)?;
        let menu = Menu::with_items(app, &[&open, &toggle, &separator, &quit])?;
        let base =
            app.default_window_icon()
                .ok_or(tauri::Error::InvalidIcon(std::io::Error::other(
                    "the app has no icon",
                )))?;
        let idle = base.clone().to_owned();
        let dotted = with_record_dot(base.rgba(), base.width(), base.height());
        let recording = tauri::image::Image::new_owned(dotted, base.width(), base.height());
        let on_menu = Arc::clone(self);
        let on_click = Arc::clone(self);
        let icon = TrayIconBuilder::with_id("earshot")
            .icon(idle.clone())
            .tooltip(view.tooltip)
            .menu(&menu)
            .show_menu_on_left_click(false)
            .on_menu_event(move |_, event: MenuEvent| match event.id().as_ref() {
                "open" => on_menu.show_window(),
                "toggle" => on_menu.toggle(),
                "quit" => on_menu.quit(),
                _ => {}
            })
            .on_tray_icon_event(move |_, event| {
                if let TrayIconEvent::Click {
                    button: MouseButton::Left,
                    button_state: MouseButtonState::Up,
                    ..
                } = event
                {
                    on_click.show_window();
                }
            })
            .build(app)?;
        self.tray.set(Arc::new(TrayParts {
            icon,
            toggle,
            idle,
            recording,
        }));
        self.has_tray.store(true, Ordering::SeqCst);
        Ok(())
    }

    /// Registers the global shortcut; a failure (another app holds it) is shown on the General
    /// page and costs nothing else.
    pub fn register_shortcut(self: &Arc<Self>) {
        use tauri_plugin_global_shortcut::{
            Code, GlobalShortcutExt, Modifiers, Shortcut, ShortcutState,
        };
        let shortcut = Shortcut::new(
            Some(Modifiers::CONTROL | Modifiers::ALT | Modifiers::SHIFT),
            Code::KeyR,
        );
        let me = Arc::clone(self);
        let result =
            self.app
                .global_shortcut()
                .on_shortcut(shortcut, move |_, _, event| match event.state() {
                    ShortcutState::Pressed => {
                        let counts = me
                            .latch
                            .lock()
                            .unwrap_or_else(PoisonError::into_inner)
                            .pressed();
                        if counts {
                            me.toggle();
                        }
                    }
                    ShortcutState::Released => me
                        .latch
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .released(),
                });
        if result.is_err() {
            self.app.state::<ShortcutStatus>().set_error(format!(
                "{SHORTCUT_TEXT} is in use by another app, so it does nothing here."
            ));
        }
    }

    /// Opens the window (and brings it forward).
    pub fn show_window(&self) {
        if let Some(window) = self.app.get_webview_window(MAIN_WINDOW) {
            let _ = window.unminimize();
            let _ = window.show();
            let _ = window.set_focus();
        }
    }

    /// Whether the user is looking at the window.
    fn window_watched(&self) -> bool {
        self.app.get_webview_window(MAIN_WINDOW).is_some_and(|w| {
            w.is_visible().unwrap_or(false)
                && !w.is_minimized().unwrap_or(false)
                && w.is_focused().unwrap_or(false)
        })
    }

    /// Starts a meeting, or stops the recording one.
    pub fn toggle(self: &Arc<Self>) {
        match press(&self.driver()) {
            ToggleAction::Stop | ToggleAction::Nothing => {}
            ToggleAction::Start => {
                if !self.tracker().begin_start() {
                    return;
                }
                self.refresh_tray();
                let me = Arc::clone(self);
                // Pre-flight and model loads take seconds: never on the UI or shortcut thread.
                let spawned = std::thread::Builder::new()
                    .name("earshot-shell-start".into())
                    .spawn(move || me.start_meeting());
                if spawned.is_err() {
                    self.start_failed("Earshot could not start the meeting.".into());
                }
            }
        }
    }

    fn start_meeting(self: &Arc<Self>) {
        self.start_meeting_in(None);
    }

    /// [`Self::start_meeting`] in `mode`, or the Audio page's stored mode when `None`.
    fn start_meeting_in(self: &Arc<Self>, mode: Option<crate::meeting_events::CaptureMode>) {
        let started = self
            .app
            .state::<AppState>()
            .services()
            .and_then(|services| {
                let mode = match mode {
                    Some(mode) => mode,
                    None => crate::audio_settings::stored_capture_mode(&services)?,
                };
                self.driver().start(
                    &services,
                    StartRequest {
                        mode,
                        title: String::new(),
                        // Asked only on the new-meeting screen (T12.13).
                        keep_local: false,
                    },
                )
            });
        match started {
            Ok(meeting) => {
                // Its `Started` usually came first; one that ended without it still ends the
                // start (T14.5).
                let changed = self.tracker().start_reported(&meeting);
                if changed {
                    self.refresh_tray();
                }
                if !self.window_watched() {
                    self.notify(&listening_toast(), None);
                }
            }
            Err(e) => self.start_failed(e.message),
        }
    }

    fn start_failed(&self, message: String) {
        self.tracker().start_failed();
        self.refresh_tray();
        self.show_window();
        let _ = ShellEvent::StartFailed { message }.emit(&self.app);
    }

    /// Starts the call-start watch's thread (T12.86): it reads the microphone sessions while the
    /// General page's offer is on, and asks when a call app opens the microphone with no meeting
    /// recording. It runs for the process's life; a panic costs one step, never the thread.
    pub fn spawn_call_start_watch(self: &Arc<Self>) {
        let me = Arc::clone(self);
        let _ = std::thread::Builder::new()
            .name("earshot-call-start".into())
            .spawn(move || {
                let mut poller = None;
                loop {
                    let step = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        me.call_start_step(&mut poller);
                    }));
                    if step.is_err() {
                        poller = None;
                    }
                    std::thread::sleep(CALL_START_STEP);
                }
            });
    }

    #[cfg(windows)]
    fn call_start_step(
        self: &Arc<Self>,
        poller: &mut Option<earshot_capture_wasapi::RenderSessionPoller>,
    ) {
        let on = self.call_start_setting.get(|| {
            let services = self.app.state::<AppState>().services().ok()?;
            earshot_runtime::call_start::offer_on(&services.store()).ok()
        });
        if !on {
            // Turned off (or no store): no readings, and an open question goes.
            *poller = None;
            *self.call_start_watch() = earshot_runtime::call_start::CallStartWatch::new(
                earshot_runtime::call_start::CallStartConfig::default(),
            );
            if self.call_start_open.swap(false, Ordering::SeqCst) {
                self.call_start_closed();
            }
            return;
        }
        if poller.is_none() {
            *poller = earshot_capture_wasapi::RenderSessionPoller::start_microphones(
                earshot_capture_wasapi::session::CALL_POLL_INTERVAL,
            )
            .ok();
        }
        let Some(reading) = poller.as_mut().and_then(|p| mic_reading(p.latest())) else {
            return;
        };
        // One lock at a time.
        let phase = self.tracker().phase();
        let recording = matches!(phase, Phase::Starting | Phase::Recording)
            || self.driver().phase() == DriverPhase::Recording;
        let change = self.call_start_watch().reading(reading, recording);
        match change {
            Some(earshot_runtime::call_start::CallStartChange::Offer) => {
                self.call_start_open.store(true, Ordering::SeqCst);
                let _ = ShellEvent::CallStartOffer { open: true }.emit(&self.app);
                if !self.window_watched() {
                    self.notify(&call_start_toast(), None);
                }
            }
            Some(earshot_runtime::call_start::CallStartChange::Withdraw) => {
                self.call_start_open.store(false, Ordering::SeqCst);
                self.call_start_closed();
            }
            None => {}
        }
    }

    #[cfg(not(windows))]
    fn call_start_step(self: &Arc<Self>, _: &mut Option<()>) {}

    fn call_start_watch(
        &self,
    ) -> std::sync::MutexGuard<'_, earshot_runtime::call_start::CallStartWatch> {
        self.call_start
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// The General page stored the call-start offer as `on`: the watch reads it from here.
    pub fn call_start_offer_stored(&self, on: bool) {
        self.call_start_setting.set(on);
    }

    /// Says once, at the start, what no window is there to say (T14.9, [`start_toast`]):
    /// `shows_window` is whether this start opened the window.
    pub fn tell_start_problems(self: &Arc<Self>, shows_window: bool) {
        let store_failed = self.app.state::<AppState>().services().is_err();
        if let Some(toast) = start_toast(shows_window, store_failed) {
            self.notify(&toast, None);
        }
    }

    /// The question closed: the window's banner and the notification go.
    fn call_start_closed(self: &Arc<Self>) {
        let _ = ShellEvent::CallStartOffer { open: false }.emit(&self.app);
        self.take_back_call_end_toast();
    }

    /// The user's answer to the call-start question, from the banner or the notification. A
    /// question already closed takes no answer: a late press must never start a recording.
    /// Returns whether it was taken.
    pub fn answer_call_start(self: &Arc<Self>, start: bool) -> bool {
        if !self.call_start_open.swap(false, Ordering::SeqCst) {
            return false;
        }
        self.call_start_watch().answered();
        self.call_start_closed();
        if start && self.tracker().begin_start() {
            self.refresh_tray();
            let me = Arc::clone(self);
            let spawned = std::thread::Builder::new()
                .name("earshot-shell-start".into())
                .spawn(move || me.start_meeting_in(Some(crate::meeting_events::CaptureMode::Call)));
            if spawned.is_err() {
                self.start_failed("Earshot could not start the meeting.".into());
            }
        }
        true
    }

    /// "Quit Earshot": at once with no meeting, else the window asks first.
    pub fn quit(&self) {
        match quit_action(self.driver().phase()) {
            QuitAction::Quit => self.app.exit(0),
            QuitAction::Ask { recording } => {
                self.show_window();
                let _ = ShellEvent::QuitAsked { recording }.emit(&self.app);
            }
        }
    }

    /// A meeting's event, after the UI got it: the tray follows, and a finished meeting
    /// notifies when the window is not being looked at.
    pub fn on_meeting_event(self: &Arc<Self>, event: &MeetingEvent) {
        let changed = self.tracker().apply(event);
        if changed {
            self.refresh_tray();
        }
        let was = self.call_end_open.load(Ordering::SeqCst);
        let open = call_end_open_after(event, self.tracker().phase(), was);
        self.call_end_open.store(open, Ordering::SeqCst);
        if was && !open {
            self.take_back_call_end_toast();
        }
        // Once per meeting (the banner comes again with the saved recording's problems); its own
        // notification says the recording ended, so the end's is not shown after it.
        if let Some(toast) = always_shown_toast(event)
            && !self.end_already_told.swap(true, Ordering::SeqCst)
        {
            self.notify(&toast, None);
        }
        match event {
            MeetingEvent::Started { .. } => self.end_already_told.store(false, Ordering::SeqCst),
            MeetingEvent::CallEndOffer { open: true } if !self.window_watched() => {
                self.notify(&call_end_toast(), None);
            }
            MeetingEvent::CallEndStopped => {
                self.end_already_told.store(true, Ordering::SeqCst);
                if !self.window_watched() {
                    self.notify(&call_end_stopped_toast(), None);
                }
            }
            MeetingEvent::Ended { reason, .. } => {
                let already_told = self.end_already_told.swap(false, Ordering::SeqCst);
                if !self.window_watched()
                    && let Some(toast) = ended_toast(*reason, already_told)
                {
                    self.notify(&toast, None);
                }
            }
            _ => {}
        }
        if let MeetingEvent::Summary { meeting, result } = event
            && !self.window_watched()
            && let Some(toast) = finished_toast(result)
        {
            self.notify(&toast, Some(meeting.clone()));
        }
    }

    /// A job's event, after the UI got it: a finished re-summary notifies when the window is not
    /// being looked at, as a meeting's own summary does.
    pub fn on_job_event(self: &Arc<Self>, event: &crate::jobs::JobEvent) {
        if !self.window_watched()
            && let Some((toast, meeting)) = job_toast(event)
        {
            self.notify(&toast, Some(meeting));
        }
    }

    /// An automatic update check found `version`: notifies when the window is not being looked
    /// at, and says whether it did (the page shows the offer either way).
    pub fn on_update_found(self: &Arc<Self>, version: &str) -> bool {
        if self.window_watched() {
            return false;
        }
        self.notify(&update_toast(version), None);
        true
    }

    fn refresh_tray(&self) {
        self.tray.refresh(|| self.tracker().phase());
    }

    /// Shows `toast`; `meeting` is what its clicks open. Never blocks the caller (the meeting
    /// thread), and a notification Windows refuses costs nothing but the notification.
    #[cfg(windows)]
    fn notify(self: &Arc<Self>, toast: &Toast, meeting: Option<String>) {
        let me = Arc::clone(self);
        let toast = toast.clone();
        let _ = std::thread::Builder::new()
            .name("earshot-toast".into())
            .spawn(move || {
                let clicked = Arc::clone(&me);
                let _ = me.toasts.show(
                    &toast,
                    Arc::new(move |click| clicked.toast_clicked(click, meeting.as_deref())),
                );
                // The question may have closed while this thread was starting.
                if toast.stays && !me.question_open() {
                    me.toasts.hide_staying();
                }
            });
    }

    /// Takes the staying notification off the screen. Never blocks the caller.
    #[cfg(windows)]
    fn take_back_call_end_toast(self: &Arc<Self>) {
        let me = Arc::clone(self);
        let _ = std::thread::Builder::new()
            .name("earshot-toast".into())
            .spawn(move || {
                // A new question may have opened since: its notification stays.
                if !me.question_open() {
                    me.toasts.hide_staying();
                }
            });
    }

    #[cfg(not(windows))]
    fn take_back_call_end_toast(self: &Arc<Self>) {}

    /// A staying notification's question is open: the call-end or the call-start one (never both:
    /// one asks only while recording, the other only while not).
    #[cfg(windows)]
    fn question_open(&self) -> bool {
        self.call_end_open.load(Ordering::SeqCst) || self.call_start_open.load(Ordering::SeqCst)
    }

    #[cfg(not(windows))]
    fn notify(self: &Arc<Self>, _: &Toast, _: Option<String>) {}

    #[cfg(windows)]
    fn toast_clicked(self: &Arc<Self>, click: ToastClick, meeting: Option<&str>) {
        if matches!(click, ToastClick::StartRecording | ToastClick::NotNow) {
            if let Some(start) =
                call_start_answer(click, self.call_start_open.load(Ordering::SeqCst))
            {
                self.answer_call_start(start);
            }
            return;
        }
        if matches!(click, ToastClick::StopRecording | ToastClick::KeepRecording) {
            match call_end_answer(click, self.call_end_open.load(Ordering::SeqCst)) {
                // It may have just ended by itself; the UI shows that either way.
                Some(CallEndAnswer::Stop) => {
                    let _ = self.driver().stop();
                }
                Some(CallEndAnswer::Keep) => self.driver().keep_recording(),
                None => {}
            }
            return;
        }
        self.show_window();
        if let Some(meeting) = meeting
            && click != ToastClick::Window
        {
            let _ = ShellEvent::OpenMeeting {
                meeting: meeting.to_owned(),
                name_voices: click == ToastClick::NameVoices,
            }
            .emit(&self.app);
        }
    }

    /// The window's own events: closing hides it to the tray. Minimising leaves it on the
    /// taskbar, as any window, recording or not (the owner, 2026-11-19).
    pub fn on_window_event(&self, window: &Window, event: &WindowEvent) {
        if window.label() != MAIN_WINDOW {
            return;
        }
        if let WindowEvent::CloseRequested { api, .. } = event
            && self.has_tray.load(Ordering::SeqCst)
        {
            api.prevent_close();
            let _ = window.hide();
        }
    }
}

/// The answer to the call-start question (T12.86) from the window's banner: `start` starts a call
/// meeting. A question already closed takes no answer (`false`).
#[tauri::command]
#[specta::specta]
pub fn answer_call_start(shell: State<'_, Arc<Shell>>, start: bool) -> bool {
    shell.inner().answer_call_start(start)
}

/// Quits Earshot: a recording meeting is stopped and finalised first. Its later steps are cut;
/// the meeting can be re-transcribed from its page (module docs).
#[tauri::command]
#[specta::specta]
pub async fn quit_app(app: AppHandle, driver: State<'_, MeetingDriver>) -> Result<(), IpcError> {
    let driver = driver.inner().clone();
    crate::ipc::blocking(move || {
        driver.stop_and_wait();
        app.exit(0);
        Ok(())
    })
    .await
}

#[cfg(test)]
#[path = "shell_tests.rs"]
mod tests;
