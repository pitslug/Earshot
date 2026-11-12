//! Model downloads and the model check in the app (T7.4e): one download per model at a time
//! and one check at a time, each on a thread of its own, their progress and result sent to
//! the UI as [`ModelJobEvent`]s keyed by model.
//!
//! A download fetches every artefact of the model one after the other
//! (`earshot_runtime::host::download_model`, through `earshot_runtime::download`); a cancel
//! keeps the `.part` files, so starting it again resumes. The check runs the bundled clip
//! through the installed models as `earshot-cli benchmark` does, in a child process
//! (`check_child`: ONNX Runtime and whisper.cpp's log are process-wide), and the parent
//! records the answer with every trial (`host::record_model_check`); a cancel kills the
//! child. The check is excluded from meetings and meeting jobs both ways
//! (`activity::WorkGate`). A transcription model's finished download starts the check
//! (the owner, T7.4: the check runs automatically after a download) through
//! [`ModelJobs::start_check`]; when it cannot start, a `Warning` says why, after the
//! `Downloaded`. The speech helpers download through the same path (T9.1f); the check needs
//! them, so it waits for them and starts when they arrive if a transcription model is there
//! ([`check_follows`]). The CUDA parts have their own runner (`cuda_settings::CudaJobs`), which
//! takes this module's check slot first ([`ModelJobs::without_check`]). Each thread runs under
//! `catch_unwind` and sends exactly one terminal event. The work is behind [`ModelWork`], so tests
//! run fakes (never the network, a child or real models).

use std::collections::BTreeMap;
use std::panic::AssertUnwindSafe;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::JoinHandle;

use earshot_core::asr::CancelToken;
use earshot_runtime::Services;
use earshot_runtime::catalogue::{Catalogue, ModelDownloadError, ModelKind};
use serde::Serialize;
use specta::Type;

use crate::activity::WorkGate;
use crate::ipc::IpcError;
use crate::models::{RunsOn, Verdict};

/// One event of a model job. A download sends `Progress` steps, then exactly one of
/// `Downloaded`, `Cancelled` or `Failed` (with its model); a transcription model's
/// `Downloaded` is followed by the check's events, or by one `Warning` with the model when
/// the check cannot start; the check sends `Warning`s (a
/// library that could not load, before the trials) and `CheckProgress` steps, then exactly
/// one of `Checked`, `CheckGaveWay` or `Failed` (without a model). The terminal event is sent just before
/// the job's thread ends, so starting the same job again right after it may briefly be
/// refused as `busy` (retryable).
#[derive(Debug, Clone, PartialEq, Serialize, Type, tauri_specta::Event)]
#[serde(tag = "type")]
pub enum ModelJobEvent {
    /// Bytes held of a model being downloaded (over all its files).
    Progress {
        /// The model.
        model: String,
        /// Bytes held.
        done: u64,
        /// Bytes in all.
        total: u64,
    },
    /// Every file of the model is downloaded and verified.
    Downloaded {
        /// The model.
        model: String,
    },
    /// The download was cancelled; what arrived is kept for a resume.
    Cancelled {
        /// The model.
        model: String,
    },
    /// Something the user is told (not an end): the check's ONNX Runtime library for an
    /// accelerator could not load, so it tests without it; or the check did not start after
    /// a transcription model's download, and why.
    Warning {
        /// The model it concerns; `None` for the check as a whole.
        model: Option<String>,
        /// Plain copy.
        message: String,
    },
    /// The check starts or finished a trial.
    CheckProgress {
        /// The model tried.
        model: String,
        /// Where.
        runs_on: RunsOn,
        /// `None` when the trial starts, else what it found.
        verdict: Option<Verdict>,
    },
    /// The check finished and recorded its answer.
    Checked {
        /// The model it chose (`None`: no installed model works here; nothing recorded).
        model: Option<String>,
        /// Where it passed.
        runs_on: Option<RunsOn>,
        /// Trials run.
        trials: u32,
    },
    /// A check nobody asked for stopped because a meeting started: nothing to tell the user.
    CheckGaveWay,
    /// The job failed (also one that stopped unexpectedly).
    Failed {
        /// The model of a download; `None` for the check.
        model: Option<String>,
        /// What failed (plain copy).
        message: String,
        /// Whether starting it again can succeed (a download resumes).
        retryable: bool,
    },
}

/// One step of the check, as the work reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckStep {
    /// The model.
    pub model: String,
    /// Where.
    pub runs_on: RunsOn,
    /// `None` when the trial starts.
    pub verdict: Option<Verdict>,
}

/// The check's answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckDone {
    /// The model chosen and where (`None`: nothing works here).
    pub answer: Option<(String, RunsOn)>,
    /// Trials run.
    pub trials: u32,
}

/// How a download ended other than in success.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DownloadEnd {
    /// Cancelled through the token.
    Cancelled,
    /// Failed.
    Failed(IpcError),
}

/// The work of the model jobs (production: [`HostModelWork`]).
pub trait ModelWork: Send + Sync {
    /// Downloads every file of `model`, `progress(done, total)` over the whole model, until
    /// done or `cancel` is set.
    ///
    /// # Errors
    /// [`DownloadEnd`].
    fn download(
        &self,
        services: &Services,
        model: &str,
        cancel: &CancelToken,
        progress: &mut dyn FnMut(u64, u64),
    ) -> Result<(), DownloadEnd>;

    /// Runs the model check for the meeting languages and records its answer, until done
    /// or `cancel` is set; `warn` hears notices for the user (an accelerator's library that
    /// could not load), before the trials.
    ///
    /// # Errors
    /// Setup failures (ONNX Runtime, the store); a child that failed or was cancelled.
    fn check(
        &self,
        services: &Services,
        cancel: &CancelToken,
        progress: &mut dyn FnMut(CheckStep),
        warn: &mut dyn FnMut(String),
    ) -> Result<CheckDone, IpcError>;

    /// Whether `model`'s finished download starts the check ([`check_follows`] on the models
    /// directory a run would use).
    fn check_follows(&self, services: &Services, model: &str) -> bool {
        check_follows(&earshot_runtime::host::model_dir(&services.data), model)
    }
}

/// Where model job events go.
pub type ModelSink = Arc<dyn Fn(ModelJobEvent) + Send + Sync>;

struct Download {
    cancel: CancelToken,
    thread: JoinHandle<()>,
    /// True until the download itself has ended (set before the thread starts its automatic
    /// check), so the check is refused while any download still writes, yet a download
    /// finishing never counts itself or another download that has already ended.
    writing: Arc<AtomicBool>,
    /// Whether the check waits for it: a transcription model enters the check's fingerprint
    /// (`model_bench::check_fingerprint` lists `model_check::MODELS`) and the check runs on
    /// the speech helpers, so a summary download alone never blocks the check.
    in_check: bool,
}

struct Inner {
    work: Arc<dyn ModelWork>,
    sink: ModelSink,
    downloads: Mutex<BTreeMap<String, Download>>,
    check: Mutex<Option<(CancelToken, JoinHandle<()>)>>,
    gate: WorkGate,
}

/// Runs the model jobs (cheap to clone; clones share the jobs).
#[derive(Clone)]
pub struct ModelJobs(Arc<Inner>);

/// Why a download or a removal is refused while the model check runs.
pub const CHECK_RUNNING_MESSAGE: &str =
    "The model check is running. Wait for it to finish or cancel it, then try again.";

/// Why the model check is refused while a transcription model download is still writing.
pub const DOWNLOADING_MESSAGE: &str =
    "A model is still downloading. Run the model check when the downloads have finished.";

/// What a model job that panicked reports.
pub const MODEL_JOB_PANIC_MESSAGE: &str = "The model job stopped unexpectedly.";

fn busy(message: &str) -> IpcError {
    IpcError {
        kind: "busy".into(),
        message: message.into(),
        retryable: true,
    }
}

fn spawn_error(_: &std::io::Error) -> IpcError {
    IpcError {
        kind: "internal".into(),
        message: crate::plain_text::INTERNAL.into(),
        retryable: false,
    }
}

/// `unknown_model` unless `model` is a catalogue model (transcription or summary).
///
/// # Errors
/// As described.
pub fn require_catalogue_model(model: &str) -> Result<(), IpcError> {
    if Catalogue::embedded().is_ok_and(|c| c.model(model).is_some()) {
        Ok(())
    } else {
        Err(IpcError {
            kind: "unknown_model".into(),
            message: crate::models::UNKNOWN_MODEL.into(),
            retryable: false,
        })
    }
}

impl ModelJobs {
    /// A runner of `work`, sending events to `sink`; the check is excluded from meetings
    /// and meeting jobs through `gate`.
    pub fn new(work: Arc<dyn ModelWork>, sink: ModelSink, gate: WorkGate) -> Self {
        let sink: ModelSink = Arc::new(move |e| sink(crate::plain_text::plain_model_event(e)));
        Self(Arc::new(Inner {
            work,
            sink,
            downloads: Mutex::new(BTreeMap::new()),
            check: Mutex::new(None),
            gate,
        }))
    }

    fn downloads(&self) -> std::sync::MutexGuard<'_, BTreeMap<String, Download>> {
        self.0
            .downloads
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    fn check_slot(&self) -> std::sync::MutexGuard<'_, Option<(CancelToken, JoinHandle<()>)>> {
        self.0.check.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// `busy` while the check holds its slot (`slot` is the locked slot).
    fn refuse_while_checking(slot: &Option<(CancelToken, JoinHandle<()>)>) -> Result<(), IpcError> {
        if slot.as_ref().is_some_and(|(_, t)| !t.is_finished()) {
            Err(busy(CHECK_RUNNING_MESSAGE))
        } else {
            Ok(())
        }
    }

    /// Starts downloading `model` and returns once it runs. Refused while the model check
    /// runs: a model installed mid-check would enter its fingerprint untested. The check
    /// slot is held (lock order: check, then downloads) until the download is registered,
    /// so a check cannot start between the test and the start.
    ///
    /// # Errors
    /// `unknown_model`; `busy` (retryable) while `model` downloads or the check runs.
    pub fn start_download(&self, services: &Services, model: &str) -> Result<(), IpcError> {
        require_catalogue_model(model)?;
        let check = self.check_slot();
        Self::refuse_while_checking(&check)?;
        let mut downloads = self.downloads();
        downloads.retain(|_, d| !d.thread.is_finished());
        if downloads.contains_key(model) {
            return Err(busy("This model is already downloading."));
        }
        let cancel = CancelToken::new();
        let writing = Arc::new(AtomicBool::new(true));
        let (jobs, svc, name, token, flag) = (
            self.clone(),
            services.clone(),
            model.to_owned(),
            cancel.clone(),
            Arc::clone(&writing),
        );
        let thread = std::thread::Builder::new()
            .name("earshot-download".into())
            .spawn(move || {
                let downloaded = download_guarded(&jobs.0, &svc, &name, &token);
                // The download has ended: from here it no longer blocks a check.
                flag.store(false, Ordering::SeqCst);
                if downloaded && jobs.0.work.check_follows(&svc, &name) {
                    jobs.check_after_download(&svc, &name);
                }
            })
            .map_err(|e| spawn_error(&e))?;
        downloads.insert(
            model.to_owned(),
            Download {
                cancel,
                thread,
                writing,
                in_check: is_kind(model, ModelKind::Transcription)
                    || is_kind(model, ModelKind::Helpers),
            },
        );
        drop(downloads);
        drop(check);
        Ok(())
    }

    /// Cancels the download of `model`; its `Cancelled` event follows.
    ///
    /// # Errors
    /// `not_downloading` when `model` is not downloading.
    pub fn cancel_download(&self, model: &str) -> Result<(), IpcError> {
        let downloads = self.downloads();
        match downloads.get(model) {
            Some(d) if !d.thread.is_finished() => {
                d.cancel.cancel();
                Ok(())
            }
            _ => Err(IpcError {
                kind: "not_downloading".into(),
                message: "This model is not downloading.".into(),
                retryable: false,
            }),
        }
    }

    /// Runs `f` (a removal of `model`'s files) unless `model` downloads or the model check
    /// runs (a model removed mid-check would record a false failing trial), holding the
    /// check slot and the downloads lock throughout, so neither
    /// [`ModelJobs::start_download`] nor [`ModelJobs::start_check`] can start meanwhile.
    ///
    /// # Errors
    /// `busy` (retryable) while `model` downloads or the check runs; `f`'s error.
    pub fn without_download<T>(
        &self,
        model: &str,
        f: impl FnOnce() -> Result<T, IpcError>,
    ) -> Result<T, IpcError> {
        let check = self.check_slot();
        Self::refuse_while_checking(&check)?;
        let downloads = self.downloads();
        if downloads
            .get(model)
            .is_some_and(|d| !d.thread.is_finished())
        {
            return Err(busy(
                "This model is downloading; cancel the download first.",
            ));
        }
        let out = f();
        drop(downloads);
        drop(check);
        out
    }

    /// Runs `f` unless the model check runs, holding the check slot throughout, so no check can
    /// start meanwhile (the CUDA parts' downloads and removals: `cuda_settings`; lock order: the
    /// check slot, then the caller's own).
    ///
    /// # Errors
    /// `busy` (retryable) while the check runs; `f`'s error.
    pub fn without_check<T>(&self, f: impl FnOnce() -> Result<T, IpcError>) -> Result<T, IpcError> {
        let check = self.check_slot();
        Self::refuse_while_checking(&check)?;
        let out = f();
        drop(check);
        out
    }

    /// Runs `f` (a CUDA part's removal) unless the model check, a meeting or a meeting job runs: a
    /// meeting or a job may have the files mapped. The check slot and the gate's pass stop any of
    /// them starting until `f` has returned (lock order: the check slot, then the caller's own).
    ///
    /// # Errors
    /// `busy` (retryable) for those; `f`'s error.
    pub fn without_check_or_work<T>(
        &self,
        f: impl FnOnce() -> Result<T, IpcError>,
    ) -> Result<T, IpcError> {
        let check = self.check_slot();
        Self::refuse_while_checking(&check)?;
        let pass = self.0.gate.enter_cuda_removal()?;
        let out = f();
        drop(pass);
        drop(check);
        out
    }

    /// Whether the downloads lock is held now (tests of the critical section).
    #[cfg(test)]
    pub fn downloads_locked(&self) -> bool {
        self.0.downloads.try_lock().is_err()
    }

    /// Whether the check slot is locked now (tests of the critical section).
    #[cfg(test)]
    pub fn check_slot_locked(&self) -> bool {
        self.0.check.try_lock().is_err()
    }

    /// Whether `model` downloads.
    #[must_use]
    pub fn downloading(&self, model: &str) -> bool {
        self.downloads()
            .get(model)
            .is_some_and(|d| !d.thread.is_finished())
    }

    /// Whether any model downloads.
    #[must_use]
    pub fn any_downloading(&self) -> bool {
        self.downloads().values().any(|d| !d.thread.is_finished())
    }

    /// Starts the model check and returns once it runs. Refused while any download is still
    /// writing (a model landing mid-check would enter its fingerprint untested); the check
    /// slot is taken, then the downloads lock (the order of `start_download`), and the
    /// downloads lock is held until the check is registered.
    ///
    /// # Errors
    /// `busy` (retryable) while a check, a download, a meeting or a meeting job runs.
    pub fn start_check(&self, services: &Services) -> Result<(), IpcError> {
        self.start_check_with(services, false)
    }

    /// [`Self::start_check`] for a check nobody asked for at this moment: a meeting that starts
    /// while it runs cancels it (`CheckPass::gives_way_to_meetings`).
    ///
    /// # Errors
    /// As [`Self::start_check`].
    pub fn start_check_giving_way(&self, services: &Services) -> Result<(), IpcError> {
        self.start_check_with(services, true)
    }

    fn start_check_with(&self, services: &Services, gives_way: bool) -> Result<(), IpcError> {
        let mut check = self.check_slot();
        if check.as_ref().is_some_and(|(_, t)| !t.is_finished()) {
            return Err(busy("The model check is already running."));
        }
        let downloads = self.downloads();
        if downloads
            .values()
            .any(|d| d.in_check && d.writing.load(Ordering::SeqCst))
        {
            return Err(busy(DOWNLOADING_MESSAGE));
        }
        let pass = self.0.gate.enter_check()?;
        let cancel = CancelToken::new();
        if gives_way {
            pass.gives_way_to_meetings(cancel.clone());
        }
        let (inner, svc, token) = (Arc::clone(&self.0), services.clone(), cancel.clone());
        let thread = std::thread::Builder::new()
            .name("earshot-model-check".into())
            .spawn(move || {
                check_guarded(&inner, &svc, &token, gives_way);
                drop(pass);
            })
            .map_err(|e| spawn_error(&e))?;
        *check = Some((cancel, thread));
        drop(downloads);
        Ok(())
    }

    /// Cancels the running model check (its child is killed); its `Failed` follows.
    ///
    /// # Errors
    /// `not_checking` when no check runs.
    pub fn cancel_check(&self) -> Result<(), IpcError> {
        let check = self.0.check.lock().unwrap_or_else(PoisonError::into_inner);
        match check.as_ref() {
            Some((cancel, t)) if !t.is_finished() => {
                cancel.cancel();
                Ok(())
            }
            _ => Err(IpcError {
                kind: "not_checking".into(),
                message: "the model check is not running".into(),
                retryable: false,
            }),
        }
    }

    /// Waits for every job.
    #[cfg(test)]
    pub fn join(&self) {
        let all: Vec<Download> = std::mem::take(&mut *self.downloads())
            .into_values()
            .collect();
        for d in all {
            let _ = d.thread.join();
        }
        let check = self
            .0
            .check
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        if let Some((_, t)) = check {
            let _ = t.join();
        }
    }
}

/// Whether `model` is a model of that kind in the catalogue.
fn is_kind(model: &str, kind: ModelKind) -> bool {
    Catalogue::embedded().is_ok_and(|c| c.model(model).is_some_and(|m| m.kind == kind))
}

/// Whether a finished download of `model` starts the check: any transcription model; the
/// speech helpers only when there is a transcription model in `models` to check (the check
/// did not run when that model arrived first, or failed for want of them).
#[must_use]
pub fn check_follows(models: &earshot_runtime::backends::ModelDir, model: &str) -> bool {
    is_kind(model, ModelKind::Transcription)
        || (is_kind(model, ModelKind::Helpers)
            && Catalogue::embedded().is_ok_and(|c| {
                c.models_of(ModelKind::Transcription)
                    .any(|m| c.is_installed(models, &m.name))
            }))
}

/// What the user is told about a download that failed: fixed copy, never a URL or a file name
/// (they hold manifest ids), and a retry resumes what can be resumed.
#[must_use]
pub fn download_failure_copy(e: &ModelDownloadError) -> String {
    use earshot_runtime::archive::ExtractError;
    use earshot_runtime::download::DownloadError as D;
    const DISK: &str = "Earshot could not write the download to this computer. Check that the \
                        disk has free space, then try again.";
    const NOT_THIS_VERSION: &str = "This version of Earshot cannot use this download. An update \
                                    may fix it.";
    match e {
        ModelDownloadError::Catalogue(_) => NOT_THIS_VERSION.to_owned(),
        ModelDownloadError::Extract { error, .. } => match error {
            ExtractError::Io { .. } => DISK.to_owned(),
            ExtractError::InUse { .. } => {
                "A file this download replaces is in use or not writable. Restart Earshot and try \
                 again."
                    .to_owned()
            }
            // `of_extraction` makes this a cancelled download; never shown.
            ExtractError::Cancelled => "The download was paused.".to_owned(),
            ExtractError::Refused(_) => NOT_THIS_VERSION.to_owned(),
        },
        ModelDownloadError::Download { error, .. } => match error {
            D::Io { .. } => DISK.to_owned(),
            D::Verification { .. } | D::TooLong { .. } => {
                "The download did not match what Earshot expected, so it was thrown away. Try \
                 again."
                    .to_owned()
            }
            D::Status { status, .. } => {
                format!(
                    "The download server answered with an error (HTTP {status}). Try again later."
                )
            }
            D::Manifest(_) | D::Host { .. } | D::Setup(_) => NOT_THIS_VERSION.to_owned(),
            // No server answered at all: on a work network that needs a proxy every download
            // ends here, and Earshot never uses one (PLAN 3.9), so say so (T9.23).
            e @ D::Request { .. } if e.reached_no_server() => NO_SERVER_ANSWERED.to_owned(),
            D::Request { .. }
            | D::ContentRange { .. }
            | D::Interrupted { .. }
            | D::Cancelled { .. } => {
                "The download stopped because of a network problem. Try again: it carries on \
                 from where it stopped."
                    .to_owned()
            }
        },
    }
}

/// A download that reached no server (T9.23): the network may need a proxy, which Earshot does
/// not use. The README says the same.
pub const NO_SERVER_ANSWERED: &str = "The download could not reach its server. Check that this \
     computer is online, then try again. If this is a work network that reaches the internet \
     only through a proxy, that may be why: Earshot does not use a proxy. Try the download on \
     another network.";

/// What the user is told when the check did not start after a download (`why`: the
/// refusal's own text).
#[must_use]
pub fn check_not_started_message(why: &str) -> String {
    format!("The model check did not start after the download. {why}")
}

impl ModelJobs {
    /// Starts the check after `model`'s download; a refusal (a meeting, a meeting job, a
    /// check running or another download still writing) is one `Warning` with the model,
    /// never a silent skip. Rule: the transcription download that ends last starts the check
    /// (each marks itself ended before calling this); an earlier one finding another
    /// transcription download still writing only warns. Summary downloads are ignored (they
    /// are not in the check's fingerprint), so two transcription downloads give one check,
    /// after both.
    fn check_after_download(&self, services: &Services, model: &str) {
        if let Err(e) = self.start_check(services) {
            (self.0.sink)(ModelJobEvent::Warning {
                model: Some(model.to_owned()),
                message: check_not_started_message(&e.message),
            });
        }
    }
}

/// Runs the download and sends its one terminal event; `true` when it was `Downloaded`.
fn download_guarded(inner: &Inner, svc: &Services, model: &str, cancel: &CancelToken) -> bool {
    let sink = &*inner.sink;
    let result = std::panic::catch_unwind(AssertUnwindSafe(|| {
        inner.work.download(svc, model, cancel, &mut |done, total| {
            sink(ModelJobEvent::Progress {
                model: model.to_owned(),
                done,
                total,
            });
        })
    }));
    let model = model.to_owned();
    let downloaded = matches!(result, Ok(Ok(())));
    sink(match result {
        Ok(Ok(())) => ModelJobEvent::Downloaded { model },
        Ok(Err(DownloadEnd::Cancelled)) => ModelJobEvent::Cancelled { model },
        Ok(Err(DownloadEnd::Failed(e))) => ModelJobEvent::Failed {
            model: Some(model),
            message: e.message,
            retryable: e.retryable,
        },
        Err(_) => ModelJobEvent::Failed {
            model: Some(model),
            message: MODEL_JOB_PANIC_MESSAGE.into(),
            retryable: false,
        },
    });
    downloaded
}

fn check_guarded(inner: &Inner, svc: &Services, cancel: &CancelToken, gives_way: bool) {
    let sink = &*inner.sink;
    let result = std::panic::catch_unwind(AssertUnwindSafe(|| {
        inner.work.check(
            svc,
            cancel,
            &mut |step| {
                sink(ModelJobEvent::CheckProgress {
                    model: step.model,
                    runs_on: step.runs_on,
                    verdict: step.verdict,
                });
            },
            &mut |message| {
                sink(ModelJobEvent::Warning {
                    model: None,
                    message,
                })
            },
        )
    }));
    sink(match result {
        Ok(Ok(done)) => {
            let (model, runs_on) = done.answer.unzip();
            ModelJobEvent::Checked {
                model,
                runs_on,
                trials: done.trials,
            }
        }
        // A meeting, or the user's own cancel: either way nobody asked for this check to finish.
        Ok(Err(_)) if gives_way && cancel.is_cancelled() => ModelJobEvent::CheckGaveWay,
        Ok(Err(e)) => ModelJobEvent::Failed {
            model: None,
            message: e.message,
            retryable: e.retryable,
        },
        Err(_) => ModelJobEvent::Failed {
            model: None,
            message: MODEL_JOB_PANIC_MESSAGE.into(),
            retryable: false,
        },
    });
}

/// What the check tells the user when ONNX Runtime's library for `library` cannot load
/// (`e`, the runtime's own text: paths only inside the models directory, never a URL).
#[must_use]
pub fn check_fallback_message(
    library: earshot_runtime::backends::Accelerator,
    e: &earshot_runtime::RuntimeError,
) -> String {
    let why = crate::plain_text::library_problem(e);
    format!(
        "The {library:?} library of ONNX Runtime is unavailable, so the model check tests \
         Parakeet models on the processor only ({why})."
    )
}

/// The production work: downloads through the manifest's allowlist, the check through
/// `earshot_runtime::host`.
pub struct HostModelWork;

impl ModelWork for HostModelWork {
    fn download(
        &self,
        svc: &Services,
        model: &str,
        cancel: &CancelToken,
        progress: &mut dyn FnMut(u64, u64),
    ) -> Result<(), DownloadEnd> {
        use earshot_runtime::download::Downloader;
        use earshot_runtime::host;

        let models = host::model_dir(&svc.data);
        // Its own current-thread runtime; this is a plain thread, never inside another one.
        // The error's words name the manifest: the page gets fixed copy (T9.23).
        let downloader = Downloader::for_manifest(Arc::clone(&svc.clock)).map_err(|_| {
            DownloadEnd::Failed(IpcError {
                kind: "download".into(),
                message: crate::models::MODEL_FILES_FAILED.into(),
                retryable: false,
            })
        })?;
        let result = host::download_model(&downloader, &models, model, cancel, progress);
        result.map_err(|e| {
            if e.cancelled() {
                DownloadEnd::Cancelled
            } else {
                DownloadEnd::Failed(IpcError {
                    kind: "download".into(),
                    message: download_failure_copy(&e),
                    retryable: e.retryable(),
                })
            }
        })
    }

    fn check(
        &self,
        svc: &Services,
        cancel: &CancelToken,
        progress: &mut dyn FnMut(CheckStep),
        warn: &mut dyn FnMut(String),
    ) -> Result<CheckDone, IpcError> {
        use earshot_runtime::host;

        let models = host::model_dir(&svc.data);
        let languages = {
            let db = svc.store();
            earshot_runtime::model_settings::model_settings(&db)?
                .0
                .languages
        };
        let mut events = JobLines { progress, warn };
        let outcome =
            crate::check_child::run_child(&languages, models.root(), cancel, &mut events)?;
        record_outcome(svc, &models, &languages, &outcome)
    }
}

/// Records a finished check in the parent's store (`host::record_model_check`) and says
/// what it found.
///
/// # Errors
/// Store errors.
pub fn record_outcome(
    svc: &Services,
    models: &earshot_runtime::backends::ModelDir,
    languages: &[String],
    outcome: &earshot_runtime::model_bench::CheckOutcome,
) -> Result<CheckDone, IpcError> {
    let langs: Vec<&str> = languages.iter().map(String::as_str).collect();
    {
        let mut db = svc.store();
        earshot_runtime::host::record_model_check(&mut db, models, &langs, outcome)?;
    }
    Ok(CheckDone {
        answer: outcome
            .recommendation
            .as_ref()
            .map(|r| (r.model.to_owned(), RunsOn::of(r.backend))),
        trials: u32::try_from(outcome.trials.len()).unwrap_or(u32::MAX),
    })
}

/// The child's lines as the job's progress and warnings.
pub struct JobLines<'a> {
    /// Hears each trial step.
    pub progress: &'a mut dyn FnMut(CheckStep),
    /// Hears each notice.
    pub warn: &'a mut dyn FnMut(String),
}

impl crate::check_child::LineEvents for JobLines<'_> {
    fn warning(&mut self, message: String) {
        (self.warn)(message);
    }

    fn trial(
        &mut self,
        model: &str,
        backend: earshot_core::asr::BackendKind,
        verdict: Option<earshot_runtime::model_check::Verdict>,
    ) {
        (self.progress)(CheckStep {
            model: model.to_owned(),
            runs_on: RunsOn::of(backend),
            verdict: verdict.map(Verdict::of_trial),
        });
    }
}

#[cfg(test)]
#[path = "model_jobs_tests.rs"]
mod tests;
