//! Updates on the General page (T9.1b, D24). A check happens only when the user presses the button
//! or has turned automatic checks on; a download only when they press Download; an install only
//! after they confirm the restart.
//!
//! [`UpdateJobs`] holds what the page shows, and runs one check or download at a time on its own
//! thread under `catch_unwind`, so a panic still ends in a phase the page can leave. Failures reach
//! the page only as [`failure_copy`]. Installing goes through [`WorkGate::enter_restart`], so it is
//! refused during a meeting, a meeting job, the model check, the voice sample and the audio check,
//! and while a model download is writing.

use std::panic::AssertUnwindSafe;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};

use earshot_core::asr::CancelToken;
use earshot_runtime::SharedClock;
use earshot_runtime::update::{CheckResult, UpdateError, UpdateOffer, UpdateSource, Updater};
use serde::Serialize;
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
    /// Whether Earshot checks by itself when it starts.
    pub auto_check: bool,
}

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
    phase: Mutex<UpdatePhase>,
    cancel: Mutex<Option<CancelToken>>,
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

    /// Moves to `next` if no step is running, as one decision: two presses start one thread.
    fn claim(&self, next: UpdatePhase) -> Result<UpdatePhase, IpcError> {
        let mut phase = self.phase.lock().unwrap_or_else(PoisonError::into_inner);
        if matches!(
            *phase,
            // Ready too: the offer behind it is what an install will run, so no check replaces it.
            UpdatePhase::Checking | UpdatePhase::Downloading { .. } | UpdatePhase::Ready { .. }
        ) {
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
    /// Jobs over `work`, reporting to `sink`.
    #[must_use]
    pub fn new(work: Arc<dyn UpdateWork>, sink: UpdateSink) -> Self {
        Self(Arc::new(Inner {
            work,
            sink,
            phase: Mutex::new(UpdatePhase::Idle),
            cancel: Mutex::new(None),
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

    /// Starts a check. `quiet` is the automatic one: a failure says nothing, because nobody asked.
    ///
    /// # Errors
    /// `busy` while a step runs.
    pub fn start_check(&self, quiet: bool) -> Result<std::thread::JoinHandle<()>, IpcError> {
        self.0.claim(UpdatePhase::Checking)?;
        Ok(self.spawn(move |inner| {
            let next = match inner.work.check() {
                Ok(None) => UpdatePhase::UpToDate,
                Ok(Some((version, size))) => UpdatePhase::Available { version, size },
                Err(_) if quiet => UpdatePhase::Idle,
                Err(e) => UpdatePhase::Failed {
                    message: failure_copy(&e).into(),
                },
            };
            inner.set(next);
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
        if !matches!(self.0.phase(), UpdatePhase::Ready { .. }) {
            return Err(IpcError::new("update_not_ready", NOT_READY));
        }
        let busy = |message: &str| IpcError {
            kind: "busy".into(),
            message: message.into(),
            retryable: true,
        };
        if downloading {
            return Err(busy(DOWNLOADS_RUNNING));
        }
        let pass = gate.enter_restart().map_err(|_| busy(WORK_RUNNING))?;
        match self.0.work.launch() {
            Ok(()) => {
                std::mem::forget(pass);
                Ok(())
            }
            Err(e) => {
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
    let services = state.services()?;
    let auto_check = earshot_runtime::update::auto_check(&services.store())?;
    Ok(UpdateView {
        current: env!("CARGO_PKG_VERSION").into(),
        phase: jobs.phase(),
        auto_check,
    })
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

/// Turns the check at each start on or off.
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
