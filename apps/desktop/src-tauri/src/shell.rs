//! The tray, the global shortcut and the OS notifications (T7.5).
//!
//! Earshot lives in the tray: closing the window hides it (always, not only during a meeting,
//! so the shortcut and a meeting's last steps keep running), minimising it hides it while a
//! meeting records, and only "Quit Earshot" in the tray menu ends the process. Started by
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
}

impl Default for PhaseTracker {
    fn default() -> Self {
        Self {
            phase: Phase::Idle,
            meeting: None,
            before_start: Phase::Idle,
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
            MeetingEvent::Summary { meeting, .. } if self.follows(meeting) => {
                if self.phase == Phase::Starting {
                    self.before_start = Phase::Idle;
                    Phase::Starting
                } else {
                    Phase::Idle
                }
            }
            _ => self.phase,
        };
        let changed = next != self.phase;
        self.phase = next;
        changed
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
            self.phase = self.before_start;
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
}

/// [`ToggleAction`] from what the driver says.
#[must_use]
pub fn toggle_action(phase: DriverPhase) -> ToggleAction {
    match phase {
        DriverPhase::Idle | DriverPhase::Processing => ToggleAction::Start,
        DriverPhase::Recording => ToggleAction::Stop,
    }
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
        SummaryEvent::Skipped { .. } | SummaryEvent::Cancelled | SummaryEvent::PostCancelled => {
            None
        }
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
        JobEvent::Summary { meeting, result } => {
            finished_toast(result).map(|t| (t, meeting.clone()))
        }
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
/// shortcut is confirmed before the summary arrives; `None` after the call-end stop, whose own
/// notification already said it ([`call_end_stopped_toast`]). Fixed copy, never meeting content.
#[must_use]
pub fn ended_toast(reason: EndedReason, stopped_by_call_end: bool) -> Option<Toast> {
    if stopped_by_call_end {
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

/// The tray's parts that change.
struct TrayParts {
    icon: TrayIcon,
    toggle: MenuItem<tauri::Wry>,
    idle: tauri::image::Image<'static>,
    recording: tauri::image::Image<'static>,
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
    tray: Mutex<Option<TrayParts>>,
    latch: Mutex<KeyLatch>,
    has_tray: AtomicBool,
    /// The call-end question is open ([`call_end_open_after`]).
    call_end_open: AtomicBool,
    /// The call-end offer stopped this recording ([`ended_toast`] says nothing more).
    call_end_stopped: AtomicBool,
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
            tray: Mutex::new(None),
            latch: Mutex::new(KeyLatch::default()),
            has_tray: AtomicBool::new(false),
            call_end_open: AtomicBool::new(false),
            call_end_stopped: AtomicBool::new(false),
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
        *self.tray.lock().unwrap_or_else(PoisonError::into_inner) = Some(TrayParts {
            icon,
            toggle,
            idle,
            recording,
        });
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
        match toggle_action(self.driver().phase()) {
            ToggleAction::Stop => {
                // It may have just ended by itself; the UI shows that either way.
                let _ = self.driver().stop();
            }
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
        let started = self
            .app
            .state::<AppState>()
            .services()
            .and_then(|services| {
                let mode = crate::audio_settings::stored_capture_mode(&services)?;
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
            Ok(_) => {
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
        match event {
            MeetingEvent::Started { .. } => self.call_end_stopped.store(false, Ordering::SeqCst),
            MeetingEvent::CallEndOffer { open: true } if !self.window_watched() => {
                self.notify(&call_end_toast(), None);
            }
            MeetingEvent::CallEndStopped => {
                self.call_end_stopped.store(true, Ordering::SeqCst);
                if !self.window_watched() {
                    self.notify(&call_end_stopped_toast(), None);
                }
            }
            MeetingEvent::Ended { reason, .. } => {
                let by_call_end = self.call_end_stopped.swap(false, Ordering::SeqCst);
                if !self.window_watched()
                    && let Some(toast) = ended_toast(*reason, by_call_end)
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

    fn refresh_tray(&self) {
        let view = tray_view(self.tracker().phase());
        let tray = self.tray.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(parts) = tray.as_ref() {
            let image = if view.recording {
                &parts.recording
            } else {
                &parts.idle
            };
            let _ = parts.icon.set_icon(Some(image.clone()));
            let _ = parts.icon.set_tooltip(Some(view.tooltip));
            let _ = parts.toggle.set_text(view.toggle_label);
            let _ = parts.toggle.set_enabled(view.toggle_enabled);
        }
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
                if toast.stays && !me.call_end_open.load(Ordering::SeqCst) {
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
                if !me.call_end_open.load(Ordering::SeqCst) {
                    me.toasts.hide_staying();
                }
            });
    }

    #[cfg(not(windows))]
    fn take_back_call_end_toast(self: &Arc<Self>) {}

    #[cfg(not(windows))]
    fn notify(self: &Arc<Self>, _: &Toast, _: Option<String>) {}

    #[cfg(windows)]
    fn toast_clicked(&self, click: ToastClick, meeting: Option<&str>) {
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

    /// The window's own events: closing hides it, and minimising hides it while recording.
    pub fn on_window_event(&self, window: &Window, event: &WindowEvent) {
        if window.label() != MAIN_WINDOW {
            return;
        }
        match event {
            WindowEvent::CloseRequested { api, .. } if self.has_tray.load(Ordering::SeqCst) => {
                api.prevent_close();
                let _ = window.hide();
            }
            WindowEvent::Resized(_)
                if self.tracker().phase() == Phase::Recording
                    && self.has_tray.load(Ordering::SeqCst)
                    && window.is_minimized().unwrap_or(false) =>
            {
                let _ = window.hide();
            }
            _ => {}
        }
    }
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
