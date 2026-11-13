//! Updates on the General page (T9.1b, D24). A check happens only when the user presses the button
//! or has turned automatic checks on; a download only when they press Download; an install only
//! after they confirm the restart.
//!
//! [`UpdateJobs`] holds what the page shows, and runs one check or download at a time on its own
//! thread under `catch_unwind`, so a panic still ends in a phase the page can leave. Failures reach
//! the page only as [`failure_copy`]. Installing goes through [`WorkGate::enter_restart`], so it is
//! refused during a meeting, a meeting job, the model check, the voice sample and the audio check,
//! and while a model download is writing.
//!
//! Automatic checks (T12.22) run at the start and on the user's schedule, on one thread
//! ([`SCHEDULE_THREAD`], [`spawn_schedule`]) that wakes at most every [`SCHEDULE_STEP`], reads the
//! switch, the schedule and the last check again, and starts the same quiet check as the start
//! when [`earshot_runtime::update::next_due`] says so. Off means no request. Every finished check,
//! the button's too, is stored as the last one ([`after_check`]); an automatic check that finds a
//! version not yet notified shows one notification when the window is not watched. No check
//! starts while an update installs: [`UpdateJobs::install`] closes the jobs to checks before it
//! takes [`WorkGate::enter_restart`], for good once the installer started.

use std::panic::AssertUnwindSafe;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use earshot_core::asr::CancelToken;
use earshot_core::identity::Clock;
use earshot_runtime::update::{
    CheckResult, UpdateError, UpdateOffer, UpdateSchedule, UpdateSource, Updater,
};
use earshot_runtime::{RuntimeError, Services, SharedClock};
use serde::{Deserialize, Serialize};
use specta::Type;
use tauri::{AppHandle, State};

use crate::activity::WorkGate;
use crate::cuda_settings::CudaJobs;
use crate::ipc::{AppState, IpcError, blocking};
use crate::meeting::MeetingDriver;

/// Where the update stands.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Type)]
#[serde(tag = "state")]
pub enum UpdatePhase {
    /// Nothing asked yet.
    Idle,
    /// Asking the update host.
    Checking,
    /// The latest release is this version or older.
    UpToDate,
    /// A newer release can be downloaded.
    Available {
        /// The new version.
        version: String,
        /// The installer's size in bytes.
        size: u64,
    },
    /// The installer is downloading.
    Downloading {
        /// The new version.
        version: String,
        /// Bytes held.
        done: u64,
        /// Bytes in all.
        total: u64,
    },
    /// The installer is here and verified: the next step restarts the app.
    Ready {
        /// The new version.
        version: String,
    },
    /// The last step failed. Fixed copy, never a URL or a path.
    Failed {
        /// What to tell the user.
        message: String,
    },
}

/// Where the update stands now, as an event.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Type, tauri_specta::Event)]
pub struct UpdateEvent {
    /// The new phase.
    pub phase: UpdatePhase,
}

/// What the page reads when it opens.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Type)]
pub struct UpdateView {
    /// The running version.
    pub current: String,
    /// Where the update stands.
    pub phase: UpdatePhase,
    /// Whether Earshot checks by itself: when it starts and on its schedule.
    pub auto_check: bool,
    /// How often it checks while it runs (with `auto_check` on).
    pub schedule: ScheduleChoice,
    /// A stored schedule this version does not know, read as daily.
    pub schedule_unknown: Option<String>,
}

/// How often automatic checks run, as the page names it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "snake_case")]
pub enum ScheduleChoice {
    /// Every hour.
    Hourly,
    /// Every 4 hours.
    FourHours,
    /// Every 12 hours.
    TwelveHours,
    /// Every day.
    Daily,
    /// Every week.
    Weekly,
}

impl From<UpdateSchedule> for ScheduleChoice {
    fn from(s: UpdateSchedule) -> Self {
        match s {
            UpdateSchedule::Hourly => Self::Hourly,
            UpdateSchedule::FourHours => Self::FourHours,
            UpdateSchedule::TwelveHours => Self::TwelveHours,
            UpdateSchedule::Daily => Self::Daily,
            UpdateSchedule::Weekly => Self::Weekly,
        }
    }
}

impl From<ScheduleChoice> for UpdateSchedule {
    fn from(s: ScheduleChoice) -> Self {
        match s {
            ScheduleChoice::Hourly => Self::Hourly,
            ScheduleChoice::FourHours => Self::FourHours,
            ScheduleChoice::TwelveHours => Self::TwelveHours,
            ScheduleChoice::Daily => Self::Daily,
            ScheduleChoice::Weekly => Self::Weekly,
        }
    }
}

/// How a check ended, for [`after_check`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckEnd {
    /// Nobody pressed the button: the check at the start or on the schedule.
    pub automatic: bool,
    /// The newer version it found.
    pub found: Option<String>,
}

/// Told about every check that ran, whatever it found or whether it failed.
pub type CheckRecorder = Arc<dyn Fn(&CheckEnd) + Send + Sync>;

/// The schedule thread's name.
pub const SCHEDULE_THREAD: &str = "earshot-update-schedule";
/// The longest the schedule sleeps before it reads the settings and the clock again, so a
/// changed setting applies without a restart and a wake from sleep is noticed within it.
pub const SCHEDULE_STEP: Duration = Duration::from_secs(5 * 60);

/// No release key in this build.
pub const NOT_SET_UP: &str = "Updates aren't set up in this build.";
/// The host could not be reached, or answered something unexpected.
pub const NO_CONNECTION: &str =
    "Earshot couldn't reach the update server. Check your connection and try again.";
/// The latest release has no update file.
pub const NOT_PUBLISHED: &str = "No update information is available yet. Try again later.";
/// The signature or the signed contents were refused.
pub const NOT_VERIFIED: &str =
    "This update could not be verified, so it was not used. Nothing was changed.";
/// A transfer was cut or its bytes were wrong.
pub const DOWNLOAD_FAILED: &str = "The download did not finish. Try again to carry on.";
/// The file on disk failed its last check.
pub const INSTALLER_CHANGED: &str =
    "The downloaded update failed its check, so it was not run. Nothing was changed.";
/// The installer could not be started.
pub const COULD_NOT_START: &str =
    "The update could not start. Earshot was not changed. Try again, or install it by hand.";
/// The worker thread panicked.
pub const SOMETHING_WENT_WRONG: &str = "Something went wrong with the update. Try again.";
/// A check or download is already running.
const ALREADY_RUNNING: &str = "An update step is already running.";
/// Install was asked for with nothing ready.
const NOT_READY: &str = "There is no downloaded update to install.";
/// A model download is writing.
const DOWNLOADS_RUNNING: &str = "Wait for the model download to finish, then try again.";
/// A meeting or other work is running.
const WORK_RUNNING: &str =
    "Earshot is busy. Try again when the meeting or other work has finished.";

/// The fixed text of a failure.
#[must_use]
pub fn failure_copy(error: &UpdateError) -> &'static str {
    match error {
        UpdateError::NoKey => NOT_SET_UP,
        UpdateError::Setup(_) | UpdateError::Network(_) | UpdateError::Status(_) => NO_CONNECTION,
        UpdateError::NotPublished => NOT_PUBLISHED,
        UpdateError::Refused(_) => NOT_VERIFIED,
        UpdateError::Download(_) => DOWNLOAD_FAILED,
        UpdateError::InstallerChanged | UpdateError::Unreadable(_) => INSTALLER_CHANGED,
    }
}

/// The work behind the page, so tests need no network and start no installer.
pub trait UpdateWork: Send + Sync {
    /// Asks the host: the new version and its size, or `None` when up to date.
    ///
    /// # Errors
    /// [`UpdateError`].
    fn check(&self) -> Result<Option<(String, u64)>, UpdateError>;

    /// Downloads and verifies what the last check found.
    ///
    /// # Errors
    /// [`UpdateError`].
    fn download(
        &self,
        cancel: &CancelToken,
        progress: &mut dyn FnMut(u64, u64),
    ) -> Result<(), UpdateError>;

    /// Reads the installer once more, then starts it.
    ///
    /// # Errors
    /// [`UpdateError`], or a failed start as the text of [`UpdateError::Unreadable`].
    fn launch(&self) -> Result<(), UpdateError>;
}

/// The real work: this repo's releases, the key built into the app, `<data>\updates`.
pub struct HostUpdates {
    /// `None` without a data folder: then nothing can be kept, and every step says so.
    dir: Option<PathBuf>,
    clock: SharedClock,
    /// What the last check found; the only way to an installer.
    offer: Mutex<Option<UpdateOffer>>,
}

impl HostUpdates {
    /// Work that keeps its installer in `dir`.
    #[must_use]
    pub fn new(dir: Option<PathBuf>, clock: SharedClock) -> Self {
        Self {
            dir,
            clock,
            offer: Mutex::new(None),
        }
    }

    fn dir(&self) -> Result<&PathBuf, UpdateError> {
        self.dir
            .as_ref()
            .ok_or_else(|| UpdateError::Setup("no data folder".into()))
    }

    fn updater(&self) -> Result<Updater, UpdateError> {
        Updater::new(UpdateSource::releases()?, Arc::clone(&self.clock))
    }

    fn offer(&self) -> Result<UpdateOffer, UpdateError> {
        self.offer
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
            .ok_or(UpdateError::InstallerChanged)
    }
}

impl UpdateWork for HostUpdates {
    fn check(&self) -> Result<Option<(String, u64)>, UpdateError> {
        let result = self.updater()?.check(env!("CARGO_PKG_VERSION"))?;
        let found = match &result {
            CheckResult::UpToDate => None,
            CheckResult::Available(offer) => Some((offer.version().to_owned(), offer.size())),
        };
        *self.offer.lock().unwrap_or_else(PoisonError::into_inner) = match result {
            CheckResult::UpToDate => None,
            CheckResult::Available(offer) => Some(offer),
        };
        Ok(found)
    }

    fn download(
        &self,
        cancel: &CancelToken,
        progress: &mut dyn FnMut(u64, u64),
    ) -> Result<(), UpdateError> {
        let offer = self.offer()?;
        self.updater()?
            .download(&offer, self.dir()?, cancel, progress)
            .map(|_| ())
    }

    fn launch(&self) -> Result<(), UpdateError> {
        let offer = self.offer()?;
        let path = offer.installer_path(self.dir()?);
        earshot_runtime::update::verify_installer(&offer, &path)?;
        earshot_proc::spawn_detached(&path, &earshot_runtime::update::installer_args())
            .map_err(|e| UpdateError::Unreadable(e.to_string()))
    }
}

/// What the page's events go to.
pub type UpdateSink = Arc<dyn Fn(UpdateEvent) + Send + Sync>;

struct Inner {
    work: Arc<dyn UpdateWork>,
    sink: UpdateSink,
    recorder: CheckRecorder,
    phase: Mutex<UpdatePhase>,
    cancel: Mutex<Option<CancelToken>>,
    /// An install is starting, or started: no check from then on.
    installing: AtomicBool,
}

impl Inner {
    fn phase(&self) -> UpdatePhase {
        self.phase
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    fn set(&self, phase: UpdatePhase) {
        *self.phase.lock().unwrap_or_else(PoisonError::into_inner) = phase.clone();
        (self.sink)(UpdateEvent { phase });
    }

    /// Tells the recorder; its panic costs only the record, never the check's phase.
    fn recorded(&self, end: &CheckEnd) {
        let _ = std::panic::catch_unwind(AssertUnwindSafe(|| (self.recorder)(end)));
    }

    /// Moves to `next` if no step is running, as one decision: two presses start one thread.
    fn claim(&self, next: UpdatePhase) -> Result<UpdatePhase, IpcError> {
        let mut phase = self.phase.lock().unwrap_or_else(PoisonError::into_inner);
        if self.installing.load(Ordering::SeqCst)
            || matches!(
                *phase,
                // Ready too: the offer behind it is what an install will run, so no check replaces it.
                UpdatePhase::Checking | UpdatePhase::Downloading { .. } | UpdatePhase::Ready { .. }
            )
        {
            return Err(IpcError {
                kind: "busy".into(),
                message: ALREADY_RUNNING.into(),
                retryable: true,
            });
        }
        let before = std::mem::replace(&mut *phase, next.clone());
        drop(phase);
        (self.sink)(UpdateEvent { phase: next });
        Ok(before)
    }
}

/// Runs the page's steps (cheap to clone; clones share them).
#[derive(Clone)]
pub struct UpdateJobs(Arc<Inner>);

impl UpdateJobs {
    /// Jobs over `work`, reporting to `sink`, with no recorder (tests; the app records its
    /// checks through [`Self::with_recorder`]).
    #[cfg(test)]
    #[must_use]
    pub fn new(work: Arc<dyn UpdateWork>, sink: UpdateSink) -> Self {
        Self::with_recorder(work, sink, Arc::new(|_: &CheckEnd| {}))
    }

    /// Jobs over `work`, reporting to `sink`, and telling `recorder` how every check ended.
    #[must_use]
    pub fn with_recorder(
        work: Arc<dyn UpdateWork>,
        sink: UpdateSink,
        recorder: CheckRecorder,
    ) -> Self {
        Self(Arc::new(Inner {
            work,
            sink,
            recorder,
            phase: Mutex::new(UpdatePhase::Idle),
            cancel: Mutex::new(None),
            installing: AtomicBool::new(false),
        }))
    }

    /// Where the update stands.
    #[must_use]
    pub fn phase(&self) -> UpdatePhase {
        self.0.phase()
    }

    /// Runs `step` on its own thread; a panic becomes a phase the page can leave.
    fn spawn(&self, step: impl FnOnce(&Inner) + Send + 'static) -> std::thread::JoinHandle<()> {
        let inner = Arc::clone(&self.0);
        std::thread::Builder::new()
            .name("earshot-update".into())
            .spawn(move || {
                let run = std::panic::catch_unwind(AssertUnwindSafe(|| step(&inner)));
                if run.is_err() {
                    inner.set(UpdatePhase::Failed {
                        message: SOMETHING_WENT_WRONG.into(),
                    });
                }
            })
            .unwrap_or_else(|_| {
                // No thread to run on: the same end as a panic, on a thread that always exists.
                let inner = Arc::clone(&self.0);
                inner.set(UpdatePhase::Failed {
                    message: SOMETHING_WENT_WRONG.into(),
                });
                std::thread::spawn(|| {})
            })
    }

    /// Starts a check. `quiet` is the automatic one: a failure says nothing, because nobody asked,
    /// and an offer the page already shows stays. The recorder hears how every check that ran
    /// ended, a panicking one too.
    ///
    /// # Errors
    /// `busy` while a step runs or an install starts.
    pub fn start_check(&self, quiet: bool) -> Result<std::thread::JoinHandle<()>, IpcError> {
        let before = self.0.claim(UpdatePhase::Checking)?;
        Ok(self.spawn(move |inner| {
            let result = std::panic::catch_unwind(AssertUnwindSafe(|| inner.work.check()));
            let end = CheckEnd {
                automatic: quiet,
                found: match &result {
                    Ok(Ok(Some((version, _)))) => Some(version.clone()),
                    _ => None,
                },
            };
            // What a quiet check that failed or panicked leaves: the offer the work keeps behind
            // it when a check fails, else nothing to say.
            let quietly = |before: UpdatePhase| match before {
                UpdatePhase::Available { .. } => before,
                _ => UpdatePhase::Idle,
            };
            let result = match result {
                Ok(result) => result,
                Err(_) if quiet => {
                    inner.set(quietly(before));
                    inner.recorded(&end);
                    return;
                }
                Err(panic) => {
                    inner.recorded(&end);
                    std::panic::resume_unwind(panic);
                }
            };
            let next = match result {
                Ok(None) => UpdatePhase::UpToDate,
                Ok(Some((version, size))) => UpdatePhase::Available { version, size },
                Err(_) if quiet => quietly(before),
                Err(e) => UpdatePhase::Failed {
                    message: failure_copy(&e).into(),
                },
            };
            inner.set(next);
            inner.recorded(&end);
        }))
    }

    /// Starts the download of what the last check found.
    ///
    /// # Errors
    /// `busy` while a step runs; `update_not_ready` with nothing to download.
    pub fn start_download(&self) -> Result<std::thread::JoinHandle<()>, IpcError> {
        let UpdatePhase::Available { version, size } = self.0.phase() else {
            return Err(IpcError::new("update_not_ready", NOT_READY));
        };
        let downloading = |done| UpdatePhase::Downloading {
            version: version.clone(),
            done,
            total: size,
        };
        self.0.claim(downloading(0))?;
        let cancel = CancelToken::new();
        *self.0.cancel.lock().unwrap_or_else(PoisonError::into_inner) = Some(cancel.clone());
        Ok(self.spawn(move |inner| {
            let mut report = |done, total| {
                inner.set(UpdatePhase::Downloading {
                    version: version.clone(),
                    done,
                    total,
                });
            };
            let result = inner.work.download(&cancel, &mut report);
            let next = match result {
                Ok(()) => UpdatePhase::Ready {
                    version: version.clone(),
                },
                Err(_) if cancel.is_cancelled() => UpdatePhase::Available {
                    version: version.clone(),
                    size,
                },
                Err(e) => UpdatePhase::Failed {
                    message: failure_copy(&e).into(),
                },
            };
            inner.set(next);
        }))
    }

    /// Stops a download; what arrived is kept for the next try.
    pub fn cancel(&self) {
        if let Some(token) = self
            .0
            .cancel
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .as_ref()
        {
            token.cancel();
        }
    }

    /// Starts the verified installer, if nothing is running. On success the app must exit next,
    /// and it stays closed to new work until it does.
    ///
    /// # Errors
    /// `update_not_ready`, `busy`; a failed start reaches the page as a phase too.
    pub fn install(&self, gate: &WorkGate, downloading: bool) -> Result<(), IpcError> {
        {
            // One decision with the check's claim: from here no check starts (T12.22).
            let phase = self.0.phase.lock().unwrap_or_else(PoisonError::into_inner);
            if !matches!(*phase, UpdatePhase::Ready { .. }) {
                return Err(IpcError::new("update_not_ready", NOT_READY));
            }
            self.0.installing.store(true, Ordering::SeqCst);
        }
        let reopen = || self.0.installing.store(false, Ordering::SeqCst);
        let busy = |message: &str| IpcError {
            kind: "busy".into(),
            message: message.into(),
            retryable: true,
        };
        if downloading {
            reopen();
            return Err(busy(DOWNLOADS_RUNNING));
        }
        let pass = gate.enter_restart().map_err(|_| {
            reopen();
            busy(WORK_RUNNING)
        })?;
        match self.0.work.launch() {
            Ok(()) => {
                // Both stay closed until the process ends.
                std::mem::forget(pass);
                Ok(())
            }
            Err(e) => {
                reopen();
                let message = match e {
                    UpdateError::InstallerChanged => INSTALLER_CHANGED,
                    _ => COULD_NOT_START,
                };
                self.0.set(UpdatePhase::Failed {
                    message: message.into(),
                });
                Err(IpcError::new("update_failed", message))
            }
        }
    }
}

fn view(state: &AppState, jobs: &UpdateJobs) -> Result<UpdateView, IpcError> {
    // A database saved by a newer Earshot cannot be opened, and installing the newer version is
    // the way out (T9.5c): the check by hand stays on offer, with the automatic one off.
    let (auto_check, (schedule, schedule_unknown)) = match state.services() {
        Ok(services) => {
            let store = services.store();
            (
                earshot_runtime::update::auto_check(&store)?,
                earshot_runtime::update::update_schedule(&store)?,
            )
        }
        Err(e) if e.kind == crate::ipc::NEWER_DATA_KIND => {
            (false, (UpdateSchedule::default(), None))
        }
        Err(e) => return Err(e),
    };
    Ok(UpdateView {
        current: env!("CARGO_PKG_VERSION").into(),
        phase: jobs.phase(),
        auto_check,
        schedule: schedule.into(),
        schedule_unknown,
    })
}

/// What a finished check leaves behind (the recorder of the app's [`UpdateJobs`]): the time it
/// ended, as the last check, whatever it found; and for an automatic check that found a version
/// no notification was shown for, `notify(version)`, which says whether it showed one (only when
/// the window is not watched). A shown version is stored, so each version is notified once.
///
/// # Errors
/// Store errors.
pub fn after_check(
    services: &Services,
    end: &CheckEnd,
    now_ms: u64,
    notify: impl FnOnce(&str) -> bool,
) -> Result<(), RuntimeError> {
    let notified = {
        let mut store = services.store();
        earshot_runtime::update::record_check(&mut store, now_ms)?;
        earshot_runtime::update::notified_version(&store)?
    };
    let Some(version) = end.found.as_deref().filter(|_| end.automatic) else {
        return Ok(());
    };
    // The store lock is free while the notification is asked for.
    if notified.as_deref() != Some(version) && notify(version) {
        earshot_runtime::update::set_notified_version(&mut services.store(), version)?;
    }
    Ok(())
}

/// One wake of the schedule at `now_ms`: starts a quiet check when automatic checks are on and
/// one is due, waits for it, and returns how long to sleep before the next wake (at most
/// [`SCHEDULE_STEP`]). `ran` is when this thread last started a check, so a check whose end could
/// not be stored is not repeated before its interval.
///
/// # Errors
/// Store errors: the caller sleeps a whole step.
pub fn schedule_tick(
    services: &Services,
    jobs: &UpdateJobs,
    now_ms: u64,
    ran: &mut Option<u64>,
) -> Result<Duration, RuntimeError> {
    let ((schedule, _), last) = {
        let mut store = services.store();
        // Off is no request at all, whatever the schedule says, and no write either.
        if !earshot_runtime::update::auto_check(&store)? {
            return Ok(SCHEDULE_STEP);
        }
        let mut last = earshot_runtime::update::last_check(&store)?;
        // The wall clock moved back past the last check: count from now, once, or each wake
        // would put the check one interval further away.
        if last.is_some_and(|l| l > now_ms) {
            earshot_runtime::update::record_check(&mut store, now_ms)?;
            last = Some(now_ms);
        }
        (earshot_runtime::update::update_schedule(&store)?, last)
    };
    // The same for this thread's own memory of its last check.
    if ran.is_some_and(|r| r > now_ms) {
        *ran = Some(now_ms);
    }
    let last = match (last, *ran) {
        (Some(a), Some(b)) => Some(a.max(b)),
        (a, b) => a.or(b),
    };
    let due = earshot_runtime::update::next_due(last, schedule.interval(), now_ms);
    if due > now_ms {
        return Ok(Duration::from_millis(due - now_ms).min(SCHEDULE_STEP));
    }
    // Busy (a check or download running, an update ready or installing): nothing now; the next
    // wake reads again.
    if let Ok(check) = jobs.start_check(true) {
        *ran = Some(now_ms);
        let _ = check.join();
    }
    Ok(SCHEDULE_STEP)
}

/// Runs [`schedule_tick`] until `wait` says stop: `wait(d)` sleeps up to `d` and returns whether
/// to go on. It sleeps first, because the check at the start covers the start. A panic or a
/// store error costs one step, never the thread.
pub fn schedule_loop(
    services: &Services,
    jobs: &UpdateJobs,
    clock: &dyn Clock,
    mut wait: impl FnMut(Duration) -> bool,
) {
    let mut ran = None;
    let mut sleep = SCHEDULE_STEP;
    while wait(sleep) {
        sleep = std::panic::catch_unwind(AssertUnwindSafe(|| {
            schedule_tick(services, jobs, clock.now_unix_ms(), &mut ran)
        }))
        .ok()
        .and_then(Result::ok)
        .unwrap_or(SCHEDULE_STEP);
    }
}

/// Starts the schedule on its own thread ([`SCHEDULE_THREAD`]) for the life of the app.
pub fn spawn_schedule(services: Services, jobs: UpdateJobs, clock: SharedClock) {
    let _ = std::thread::Builder::new()
        .name(SCHEDULE_THREAD.into())
        .spawn(move || {
            schedule_loop(&services, &jobs, clock.as_ref(), |d| {
                std::thread::sleep(d);
                true
            });
        });
}

/// What the Updates section shows.
#[tauri::command]
#[specta::specta]
pub async fn update_status(
    state: State<'_, AppState>,
    jobs: State<'_, UpdateJobs>,
) -> Result<UpdateView, IpcError> {
    view(state.inner(), jobs.inner())
}

/// Asks the update host for the latest version; `UpdateEvent`s follow.
#[tauri::command]
#[specta::specta]
pub async fn check_for_update(jobs: State<'_, UpdateJobs>) -> Result<(), IpcError> {
    jobs.start_check(false).map(|_| ())
}

/// Downloads what the last check found; `UpdateEvent`s follow.
#[tauri::command]
#[specta::specta]
pub async fn download_update(jobs: State<'_, UpdateJobs>) -> Result<(), IpcError> {
    jobs.start_download().map(|_| ())
}

/// Stops a download.
#[tauri::command]
#[specta::specta]
pub async fn cancel_update(jobs: State<'_, UpdateJobs>) -> Result<(), IpcError> {
    jobs.cancel();
    Ok(())
}

/// Turns automatic checks (at each start and on the schedule) on or off.
#[tauri::command]
#[specta::specta]
pub async fn set_update_auto_check(
    state: State<'_, AppState>,
    jobs: State<'_, UpdateJobs>,
    on: bool,
) -> Result<UpdateView, IpcError> {
    let services = state.services()?;
    earshot_runtime::update::set_auto_check(&mut services.store(), on)?;
    view(state.inner(), jobs.inner())
}

/// Stores how often automatic checks run; the schedule reads it at its next wake.
#[tauri::command]
#[specta::specta]
pub async fn set_update_schedule(
    state: State<'_, AppState>,
    jobs: State<'_, UpdateJobs>,
    schedule: ScheduleChoice,
) -> Result<UpdateView, IpcError> {
    let services = state.services()?;
    earshot_runtime::update::set_update_schedule(&mut services.store(), schedule.into())?;
    view(state.inner(), jobs.inner())
}

/// Starts the verified installer and exits; the installer starts the new version when it ends.
#[tauri::command]
#[specta::specta]
pub async fn install_update(
    app: AppHandle,
    jobs: State<'_, UpdateJobs>,
    gate: State<'_, WorkGate>,
    cuda: State<'_, CudaJobs>,
    driver: State<'_, MeetingDriver>,
) -> Result<(), IpcError> {
    let (jobs, gate, downloading, driver) = (
        jobs.inner().clone(),
        gate.inner().clone(),
        cuda.any_downloading(),
        driver.inner().clone(),
    );
    blocking(move || {
        jobs.install(&gate, downloading)?;
        // No meeting can be running: the gate refused otherwise. The usual way out all the same.
        driver.stop_and_wait();
        app.exit(0);
        Ok(())
    })
    .await
}

#[cfg(test)]
#[path = "update_settings_tests.rs"]
mod tests;
