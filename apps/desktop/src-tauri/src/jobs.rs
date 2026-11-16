//! Re-transcribe and re-summarise a stored meeting (T7.3): one job per meeting at a time,
//! each on a thread of its own, its progress and result sent to the UI as [`JobEvent`]s.
//!
//! A re-transcription leases the meeting itself (`edit::retranscribe`, a
//! `lease_meeting_alone(Processing)`); a re-summary takes the same lease here before its
//! thread starts, so it is refused while the meeting records, is processed or is deleted, and
//! a deletion is refused while it runs. The work is behind [`JobWork`], so tests run fakes.
//!
//! Each job has its own `CancelToken` (T12.41), set by [`JobRunner::cancel`] and polled as the
//! final pass and the summary already poll theirs: a cancelled job writes nothing beyond what a
//! cancelled final pass or summary writes (the meeting's live speaker windows deleted) and ends in
//! one [`JobEvent::Cancelled`]. A cancel after the work has finished changes nothing.

use std::collections::BTreeMap;
use std::panic::AssertUnwindSafe;
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::JoinHandle;

use earshot_core::asr::CancelToken;
use earshot_core::identity::EntityId;
use earshot_runtime::edit::Retranscribed;
use earshot_runtime::host::{HostPost, HostStep, SummaryResult};
use earshot_runtime::view::MeetingView;
use earshot_runtime::{MeetingJob, MeetingLease, Services};
use serde::Serialize;
use specta::Type;

use crate::ipc::IpcError;
use crate::meeting_events::{
    PostProgressMapper, PostStep, SummaryEvent, needs_name, summary_result_event,
};

/// Which job.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Type)]
pub enum JobKind {
    /// The final pass again from the stored audio.
    Retranscribe,
    /// The summary again.
    Resummarise,
}

/// One event of a job on a stored meeting: `Progress` steps and `Warning`s (re-transcription
/// only), then exactly one of `Retranscribed`, `Summary`, `Cancelled` or `Failed`.
#[derive(Debug, Clone, PartialEq, Serialize, Type, tauri_specta::Event)]
#[serde(tag = "type")]
pub enum JobEvent {
    /// A re-transcription step.
    Progress {
        /// Meeting id.
        meeting: String,
        /// The step.
        step: PostStep,
        /// Steps done.
        done: u32,
        /// Steps in all, once known.
        total: Option<u32>,
    },
    /// The re-transcription stored a new final transcript (or found no audio).
    Retranscribed {
        /// Meeting id.
        meeting: String,
        /// Tracks whose recording is gone.
        missing_tracks: u32,
        /// Every recording is gone: nothing was transcribed.
        no_audio: bool,
        /// The summary cites the replaced transcript: offer to summarise again.
        summary_stale: bool,
        /// What the final pass warned of (`plain_text::post_warning_copy`). Here and not as
        /// `Warning` events because the screen keeps a job's last event only.
        warnings: Vec<String>,
    },
    /// The re-summary's result.
    Summary {
        /// Meeting id.
        meeting: String,
        /// What happened.
        result: SummaryEvent,
        /// A summary written by a later step of the order, or none because every step failed
        /// (T12.63b: banner `summary-steps`, `summary_order::steps_text`). Here and not as a
        /// `Warning` because the screen keeps a job's last event only.
        note: Option<String>,
    },
    /// A re-summary that went on with a run after a computer handed it back found nothing to go
    /// on with (T12.63b: the run no longer waits on that request): nothing was run, written or
    /// sent, and the screen drops the job without a notice.
    Settled {
        /// Meeting id.
        meeting: String,
    },
    /// The re-summary asked one of the user's other computers (T12.63b): it arrives by sync, or
    /// the run goes on when that computer hands it back.
    Offloaded {
        /// Meeting id.
        meeting: String,
        /// The meeting's waiting line (fixed copy, `offload::waiting_copy`).
        message: String,
    },
    /// Something the user may need to know while the job runs (a backend fallback).
    Warning {
        /// Meeting id.
        meeting: String,
        /// Plain copy.
        message: String,
    },
    /// The job was cancelled ([`JobRunner::cancel`], T12.41): the transcript and the summary are
    /// as they were.
    Cancelled {
        /// Meeting id.
        meeting: String,
        /// Which job.
        job: JobKind,
    },
    /// The job failed; nothing was changed by it (also a job that stopped unexpectedly).
    Failed {
        /// Meeting id.
        meeting: String,
        /// Which job.
        job: JobKind,
        /// What failed (plain copy).
        message: String,
        /// Whether trying again can succeed without changing anything.
        retryable: bool,
    },
}

/// The work of the jobs (production: [`HostJobs`]).
pub trait JobWork: Send + Sync {
    /// Re-transcribes `meeting` (`edit::retranscribe`, which leases the meeting); `warn`
    /// hears notices for the user (an ONNX Runtime library fallback). A pass that answers
    /// `cancel` is `HostPost::Cancelled`.
    ///
    /// # Errors
    /// Setup and final-pass errors.
    fn retranscribe(
        &self,
        services: &Services,
        meeting: EntityId,
        cancel: &CancelToken,
        progress: &mut dyn FnMut(HostStep<'_>),
        warn: &mut dyn FnMut(String),
    ) -> Result<Retranscribed, IpcError>;

    /// Summarises `meeting` again with the meeting's summary plan; `warn` hears notices for
    /// the user (the chosen graphics card not used). A summary that answers `cancel` is
    /// [`SummaryResult::Cancelled`].
    ///
    /// # Errors
    /// Setup errors (a summary model that cannot load); a summary that ran and failed is
    /// [`SummaryResult::Failed`].
    fn summarise(
        &self,
        services: &Services,
        meeting: EntityId,
        cancel: &CancelToken,
        warn: &mut dyn FnMut(String),
    ) -> Result<SummaryResult, IpcError>;

    /// Summarises `meeting` again as `start` says (T12.63b: "Use my order", a writer named by the
    /// user, or a run going on after a computer handed it back). The default is
    /// [`JobWork::summarise`], for work that does not tell them apart (test fakes).
    ///
    /// # Errors
    /// As [`JobWork::summarise`].
    fn summarise_run(
        &self,
        services: &Services,
        meeting: EntityId,
        start: &SummaryStart,
        cancel: &CancelToken,
        warn: &mut dyn FnMut(String),
    ) -> Result<JobSummary, IpcError> {
        let _ = start;
        self.summarise(services, meeting, cancel, warn)
            .map(JobSummary::from)
    }
}

/// How a re-summary runs the order of summary writers (T12.63b, docs/summary-order.md 4.3, 5).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SummaryStart {
    /// "Use my order": the order read when the job starts, under the meeting's privacy.
    Order,
    /// One writer the user named: one step, no next step, not filtered (a remote writer on a
    /// meeting kept here is refused).
    Named(earshot_runtime::summary_order::OrderStep),
    /// The run that waited on `request` goes on from its copied steps after the computer handed
    /// it back with `word`.
    Continue {
        /// The request handed back.
        request: EntityId,
        /// The computer's outcome.
        word: earshot_runtime::summary_order::run::OutcomeWord,
    },
    /// A request asked before T12.63a was handed back: written here as before.
    Legacy,
}

/// What a re-summary did.
#[derive(Debug)]
pub struct JobSummary {
    /// The summary.
    pub result: SummaryResult,
    /// The `summary-steps` banner's text, when a later step wrote it or every step failed.
    pub note: Option<String>,
    /// A continuation found nothing to continue: nothing was run, written or sent.
    pub settled: bool,
}

impl From<SummaryResult> for JobSummary {
    fn from(result: SummaryResult) -> Self {
        Self {
            result,
            note: None,
            settled: false,
        }
    }
}

/// Where job events go.
pub type JobSink = Arc<dyn Fn(JobEvent) + Send + Sync>;

struct Inner {
    work: Arc<dyn JobWork>,
    sink: JobSink,
    running: Mutex<BTreeMap<EntityId, Running>>,
    gate: crate::activity::WorkGate,
}

/// A job that runs: which, its cancel and its thread.
struct Running {
    job: JobKind,
    cancel: CancelToken,
    thread: JoinHandle<()>,
}

impl Running {
    fn runs(&self) -> bool {
        !self.thread.is_finished()
    }
}

/// Runs the jobs (cheap to clone; clones share the jobs).
#[derive(Clone)]
pub struct JobRunner(Arc<Inner>);

fn busy() -> IpcError {
    IpcError::busy("This meeting is already being re-transcribed or re-summarised.")
}

impl JobRunner {
    /// A runner of `work`, sending events to `sink`; each job holds a pass of `gate` for
    /// its thread's life (no model check meanwhile).
    ///
    /// `_cuda` is the app's CUDA start ([`crate::cuda_settings::begin`]), asked for only so a
    /// runner cannot exist before the start has closed its gates: a job may reach llama.cpp or
    /// ONNX Runtime.
    pub fn new(
        work: Arc<dyn JobWork>,
        sink: JobSink,
        gate: crate::activity::WorkGate,
        _cuda: &crate::cuda_settings::CudaLatch,
    ) -> Self {
        let sink: JobSink = Arc::new(move |e| sink(crate::plain_text::plain_job_event(e)));
        Self(Arc::new(Inner {
            work,
            sink,
            running: Mutex::new(BTreeMap::new()),
            gate,
        }))
    }

    /// Starts `job` on `meeting` and returns once it runs.
    ///
    /// # Errors
    /// `busy` (retryable) while a job of this meeting or the model check runs;
    /// `meeting_busy` (retryable) while the meeting records, is processed or is deleted.
    pub fn start(
        &self,
        services: &Services,
        meeting: EntityId,
        job: JobKind,
    ) -> Result<(), IpcError> {
        self.start_noted(services, meeting, job, None)
    }

    /// [`JobRunner::start`], with `note` sent as the job's first [`JobEvent::Warning`] (why it
    /// runs: a summary another computer did not write, T11.3db).
    ///
    /// # Errors
    /// As [`JobRunner::start`].
    pub fn start_noted(
        &self,
        services: &Services,
        meeting: EntityId,
        job: JobKind,
        note: Option<String>,
    ) -> Result<(), IpcError> {
        self.start_inner(services, meeting, job, SummaryStart::Order, note)
    }

    /// Starts a re-summary of `meeting` that runs as `start` says (T12.63b), with `note` as its
    /// first [`JobEvent::Warning`].
    ///
    /// # Errors
    /// As [`JobRunner::start`].
    pub fn start_summary(
        &self,
        services: &Services,
        meeting: EntityId,
        start: SummaryStart,
        note: Option<String>,
    ) -> Result<(), IpcError> {
        self.start_inner(services, meeting, JobKind::Resummarise, start, note)
    }

    fn start_inner(
        &self,
        services: &Services,
        meeting: EntityId,
        job: JobKind,
        start: SummaryStart,
        note: Option<String>,
    ) -> Result<(), IpcError> {
        let mut running = self
            .0
            .running
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        running.retain(|_, r| r.runs());
        if running.contains_key(&meeting) {
            return Err(busy());
        }
        let pass = self.0.gate.enter_work()?;
        // Refused now rather than from the thread. A re-transcription takes its own lease on
        // the thread, so this one is only a check there.
        let lease = services.lease_meeting_alone(meeting, MeetingJob::Processing)?;
        let lease = match job {
            JobKind::Resummarise => Some(lease),
            JobKind::Retranscribe => {
                drop(lease);
                None
            }
        };
        let inner = Arc::clone(&self.0);
        let svc = services.clone();
        let cancel = CancelToken::new();
        let token = cancel.clone();
        let thread = std::thread::Builder::new()
            .name("earshot-job".into())
            .spawn(move || {
                if let Some(message) = note {
                    (inner.sink)(JobEvent::Warning {
                        meeting: meeting.to_string(),
                        message,
                    });
                }
                run_guarded(&inner, &svc, meeting, job, &start, &token, lease);
                drop(pass);
            })
            .map_err(|_| IpcError::internal())?;
        running.insert(
            meeting,
            Running {
                job,
                cancel,
                thread,
            },
        );
        Ok(())
    }

    /// Cancels the job of `meeting` (T12.41) and returns which job it was. Its one terminal event
    /// follows as usual: `Cancelled` when the work answered the cancel, else what it finished
    /// with. No job, a job already finished and a second press change nothing.
    pub fn cancel(&self, meeting: EntityId) -> Option<JobKind> {
        let running = self
            .0
            .running
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let r = running.get(&meeting).filter(|r| r.runs())?;
        r.cancel.cancel();
        Some(r.job)
    }

    /// Whether a job of `meeting` runs.
    #[cfg(test)]
    #[must_use]
    pub fn running(&self, meeting: EntityId) -> bool {
        self.running_job(meeting).is_some()
    }

    /// The job of `meeting` that runs, if any (T12.38: a meeting deletion names it, and stops it
    /// first when the user agreed, T12.41). True until the job's thread has ended, which is after
    /// the job let go of its lease.
    #[must_use]
    pub fn running_job(&self, meeting: EntityId) -> Option<JobKind> {
        self.0
            .running
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&meeting)
            .filter(|r| r.runs())
            .map(|r| r.job)
    }

    /// Waits for every job.
    #[cfg(test)]
    pub fn join(&self) {
        let all: Vec<JoinHandle<()>> = std::mem::take(
            &mut *self
                .0
                .running
                .lock()
                .unwrap_or_else(PoisonError::into_inner),
        )
        .into_values()
        .map(|r| r.thread)
        .collect();
        for t in all {
            let _ = t.join();
        }
    }
}

/// Voices that still need a name after a summary (as the summary-ready notice counts them).
fn voices_to_name(services: &Services, meeting: EntityId) -> u32 {
    let db = services.store();
    // An unreadable record ignores nobody (the count can only be higher, never hide a voice).
    let identities = earshot_runtime::edit::identities_in(&db, meeting).unwrap_or_default();
    MeetingView::load(&**db, meeting)
        .ok()
        .flatten()
        .map_or(0, |v| {
            let n = v
                .speakers
                .iter()
                .filter(|s| {
                    needs_name(
                        s.role,
                        s.person_id.is_some(),
                        earshot_runtime::edit::is_ignored(s, &identities),
                    )
                })
                .count();
            u32::try_from(n).unwrap_or(u32::MAX)
        })
}

/// What a job that panicked reports.
pub const JOB_PANIC_MESSAGE: &str = "The job stopped unexpectedly.";

/// The job thread: [`run`], with a panic in the work turned into its one `Failed` event.
fn run_guarded(
    inner: &Inner,
    svc: &Services,
    meeting: EntityId,
    job: JobKind,
    start: &SummaryStart,
    cancel: &CancelToken,
    lease: Option<MeetingLease>,
) {
    let result = std::panic::catch_unwind(AssertUnwindSafe(|| {
        run(inner, svc, meeting, job, start, cancel, lease);
    }));
    if result.is_err() {
        (inner.sink)(JobEvent::Failed {
            meeting: meeting.to_string(),
            job,
            message: JOB_PANIC_MESSAGE.into(),
            retryable: false,
        });
    }
}

fn run(
    inner: &Inner,
    svc: &Services,
    meeting: EntityId,
    job: JobKind,
    start: &SummaryStart,
    cancel: &CancelToken,
    lease: Option<MeetingLease>,
) {
    let sink = &*inner.sink;
    let id = meeting.to_string();
    let cancelled = || JobEvent::Cancelled {
        meeting: id.clone(),
        job,
    };
    let failed = |e: IpcError| JobEvent::Failed {
        meeting: id.clone(),
        job,
        message: e.message,
        retryable: e.retryable,
    };
    match job {
        JobKind::Retranscribe => {
            let mut mapper = PostProgressMapper::default();
            let result = inner.work.retranscribe(
                svc,
                meeting,
                cancel,
                &mut |step| {
                    if let Some(crate::meeting_events::MeetingEvent::PostProgress {
                        step,
                        done,
                        total,
                        ..
                    }) = mapper.step(&step)
                    {
                        sink(JobEvent::Progress {
                            meeting: id.clone(),
                            step,
                            done,
                            total,
                        });
                    }
                },
                &mut |message| {
                    sink(JobEvent::Warning {
                        meeting: id.clone(),
                        message,
                    });
                },
            );
            sink(match result {
                // The pass answered the cancel: nothing stored, the speaker windows deleted.
                Ok(r) if matches!(r.post, HostPost::Cancelled { .. }) => cancelled(),
                Ok(r) => JobEvent::Retranscribed {
                    meeting: id.clone(),
                    missing_tracks: u32::try_from(r.missing_tracks.len()).unwrap_or(u32::MAX),
                    no_audio: matches!(r.post, HostPost::NothingToProcess),
                    summary_stale: r.summary_stale,
                    warnings: match &r.post {
                        HostPost::Done(post) => {
                            crate::plain_text::post_warning_copy(&post.warnings)
                                .into_iter()
                                .map(|(_, text)| text)
                                .collect()
                        }
                        _ => Vec::new(),
                    },
                },
                Err(e) => failed(e),
            });
        }
        JobKind::Resummarise => {
            let result = inner
                .work
                .summarise_run(svc, meeting, start, cancel, &mut |message| {
                    sink(JobEvent::Warning {
                        meeting: id.clone(),
                        message,
                    });
                });
            drop(lease);
            if let Some(message) = result
                .as_ref()
                .ok()
                .and_then(|s| crate::meeting_events::summary_fallback_note(&s.result))
            {
                sink(JobEvent::Warning {
                    meeting: id.clone(),
                    message,
                });
            }
            sink(match result {
                // Nothing to go on with: the job ends without a summary result.
                Ok(JobSummary { settled: true, .. }) => JobEvent::Settled {
                    meeting: id.clone(),
                },
                Ok(JobSummary {
                    result: SummaryResult::Cancelled,
                    ..
                }) => cancelled(),
                // A computer was asked: the job ends here, the summary arrives by sync (T12.63b).
                Ok(JobSummary {
                    result: SummaryResult::Offloaded { name, .. },
                    ..
                }) => JobEvent::Offloaded {
                    meeting: id.clone(),
                    message: crate::offload::waiting_copy(Some(&name)),
                },
                Ok(JobSummary { result, note, .. }) => JobEvent::Summary {
                    meeting: id.clone(),
                    result: summary_result_event(&result, voices_to_name(svc, meeting)),
                    note,
                },
                Err(e) => failed(e),
            });
        }
    }
}

/// The production work: the meeting's own model choice through `earshot_runtime::host` (the
/// meeting driver's pre-flight rules), and the order of summary writers (T12.63b) with the
/// installed key store and agent CLIs, each computer step asked through `computers` once the app
/// set it.
pub struct HostJobs {
    /// Asks a computer step's computer (set once sync is ready).
    pub computers: crate::summary_order::ComputerSlot,
}

impl JobWork for HostJobs {
    fn retranscribe(
        &self,
        svc: &Services,
        meeting: EntityId,
        cancel: &CancelToken,
        progress: &mut dyn FnMut(HostStep<'_>),
        warn: &mut dyn FnMut(String),
    ) -> Result<Retranscribed, IpcError> {
        use earshot_runtime::edit::{Retranscribe, retranscribe};
        use earshot_runtime::host;

        let models = host::model_dir(&svc.data);
        // Waits for this start's choice of ONNX Runtime's library, if it is still being made.
        let build = host::settled_order();
        let mut setup = {
            let db = svc.store();
            host::transcription_setup(&db, &models, &build, &earshot_runtime::hardware())?
        };
        host::require_models(&models, &setup.choice.model, true)?;
        // "Runs on the processor" for want of an NPU version: once until it changes (T12.45),
        // recorded once the warnings went out.
        let npu_note = host::npu_note_once(&svc.store(), &mut setup);
        let choice = setup.choice;
        let order = setup.order;
        for note in setup.notes {
            warn(note);
        }
        npu_note.record(&mut svc.store());
        // A library that cannot load falls back to the CPU one, told as the meeting driver does.
        host::init_onnx_for(&models, &choice.model, &order, &mut |library, e| {
            warn(crate::meeting::onnx_fallback_message(library, e));
        })?;
        // The final pass looks at the token before it loads a model and between its steps.
        let run = Retranscribe {
            models: &models,
            model: &choice.model,
            order: &order,
            cancel,
        };
        Ok(retranscribe(svc, meeting, &run, progress)?)
    }

    fn summarise(
        &self,
        svc: &Services,
        meeting: EntityId,
        cancel: &CancelToken,
        warn: &mut dyn FnMut(String),
    ) -> Result<SummaryResult, IpcError> {
        self.summarise_run(svc, meeting, &SummaryStart::Order, cancel, warn)
            .map(|s| s.result)
    }

    fn summarise_run(
        &self,
        svc: &Services,
        meeting: EntityId,
        start: &SummaryStart,
        cancel: &CancelToken,
        warn: &mut dyn FnMut(String),
    ) -> Result<JobSummary, IpcError> {
        let models = earshot_runtime::host::model_dir(&svc.data);
        let probes = crate::summary_order::SummaryProbes::installed();
        let chooser = crate::summary_order::chooser_of(&self.computers);
        let ask = |d| match &chooser {
            Some(c) => crate::summary_order::remembering(c, d),
            None => earshot_runtime::offload::decide(Some(d), None, false),
        };
        summarise_run_with(
            svc,
            meeting,
            start,
            cancel,
            &models,
            (&*probes.keys, &*probes.claude, &*probes.codex),
            Some(&ask),
            warn,
        )
    }
}

/// A re-summary of `meeting` by "Use my order" with `models`, `keys` and the two CLIs, each
/// computer step read as sync off ([`summarise_run_with`] with [`SummaryStart::Order`]).
///
/// # Errors
/// As [`summarise_run_with`].
#[cfg(test)]
#[allow(clippy::too_many_arguments)] // The seams tests fill in: models, the key store, two CLIs.
pub(crate) fn summarise_with(
    svc: &Services,
    meeting: EntityId,
    cancel: &CancelToken,
    models: &earshot_runtime::backends::ModelDir,
    keys: &dyn earshot_runtime::summary_settings::ApiKeyStore,
    claude: &dyn earshot_runtime::claude_code::ClaudeCodeProbe,
    codex: &dyn earshot_runtime::codex::CodexProbe,
    warn: &mut dyn FnMut(String),
) -> Result<SummaryResult, IpcError> {
    summarise_run_with(
        svc,
        meeting,
        &SummaryStart::Order,
        cancel,
        models,
        (keys, claude, codex),
        None,
        warn,
    )
    .map(|s| s.result)
}

/// The key store and the two agent CLIs a run's writer steps read when they are tried.
pub(crate) type Probes<'a> = (
    &'a dyn earshot_runtime::summary_settings::ApiKeyStore,
    &'a dyn earshot_runtime::claude_code::ClaudeCodeProbe,
    &'a dyn earshot_runtime::codex::CodexProbe,
);

/// A re-summary of `meeting` as `start` says (T12.63b, docs/summary-order.md 4.3 and 5): a run
/// of the order of summary writers (`summary_order::run::run_summary_order`) over the production
/// writers of `models` and `probes`, each read when its step is tried, each computer step asked
/// through `computers` (none: sync off). A re-summary is outside the meeting queue, so a local
/// step is never "not now".
///
/// The user asked for this summary, so a run of one step (a named writer, or a one-step order)
/// fails the job with today's fixed copy where a meeting's own summary is skipped: `api_key`
/// without the endpoint's key, `claude_code` / `codex` for a CLI not installed or not signed in,
/// `summary_kept_local` for a meeting kept on this computer without its local model (or a named
/// remote writer before "Allow"), `model` for a summary model chosen by name and not downloaded.
/// A run of more steps ends with the `summary-steps` text instead ([`JobSummary::note`]). A set
/// `cancel` (T12.41) is [`SummaryResult::Cancelled`] before anything is read or run.
///
/// # Errors
/// The fixed copy above; store errors.
#[allow(clippy::too_many_arguments)] // The seams tests fill in.
pub(crate) fn summarise_run_with(
    svc: &Services,
    meeting: EntityId,
    start: &SummaryStart,
    cancel: &CancelToken,
    models: &earshot_runtime::backends::ModelDir,
    probes: Probes<'_>,
    computers: Option<
        &dyn Fn(earshot_core::identity::DeviceId) -> earshot_runtime::offload::OffloadChoice,
    >,
    warn: &mut dyn FnMut(String),
) -> Result<JobSummary, IpcError> {
    use earshot_runtime::host::SkipReason;
    use earshot_runtime::summary_order::run::{RunPlan, RunStart, run_summary_order};

    // Cancelled during the setup: nothing is read, loaded or sent.
    if cancel.is_cancelled() {
        return Ok(SummaryResult::Cancelled.into());
    }
    let privacy = earshot_runtime::keep_local::summary_privacy(&svc.store(), meeting);
    let run_start = match start {
        // The order as it is now, when the job starts.
        SummaryStart::Order => {
            let (order, notes) = earshot_runtime::summary_order::summary_order(&svc.store())?;
            for note in notes {
                warn(note);
            }
            for note in first_writer_notes(svc, &order.filtered(privacy), privacy) {
                warn(note);
            }
            RunStart::Order(order)
        }
        SummaryStart::Named(step) => RunStart::Named(*step),
        SummaryStart::Continue { request, word } => RunStart::Continue {
            request: *request,
            word: *word,
        },
        SummaryStart::Legacy => RunStart::Legacy,
    };
    let (keys, claude, codex) = probes;
    let out = crate::summary_order::with_writers(models, keys, claude, codex, computers, |w| {
        run_summary_order(
            svc,
            meeting,
            run_start,
            &RunPlan {
                writers: w,
                local_not_now: &|| false,
                order: None,
            },
            cancel,
        )
    });
    // A continuation whose run no longer waits on the request: nothing was run or sent.
    if out.settled {
        return Ok(JobSummary {
            settled: true,
            ..JobSummary::from(out.summary)
        });
    }
    for why in &out.handover_record_failures {
        warn(crate::meeting_events::handover_record_copy(why));
    }
    if out.claude_effort_unread {
        warn(crate::meeting_events::CLAUDE_EFFORT_UNREAD.into());
    }
    if crate::summary_order::tried_steps(&out.steps) <= 1 {
        match &out.summary {
            SummaryResult::Skipped {
                reason: SkipReason::KeptLocalNoModel { model },
            } => return Err(crate::meeting_events::kept_local_error(model)),
            SummaryResult::Skipped {
                reason: SkipReason::NoApiKey(e),
            } => {
                return Err(IpcError::from(earshot_runtime::RuntimeError::ApiKey(
                    e.clone(),
                )));
            }
            SummaryResult::Skipped {
                reason: SkipReason::ClaudeCodeUnavailable(e),
            } => {
                return Err(IpcError::from(earshot_runtime::claude_code::runtime_error(
                    e.clone(),
                )));
            }
            SummaryResult::Skipped {
                reason: SkipReason::CodexUnavailable(e),
            } => {
                return Err(IpcError::from(earshot_runtime::codex::runtime_error(
                    e.clone(),
                )));
            }
            SummaryResult::Skipped {
                reason: SkipReason::ChosenNotInstalled { model },
            } => {
                return Err(IpcError::new(
                    "model",
                    crate::plain_text::chosen_summary_model(model),
                ));
            }
            _ => {}
        }
    }
    if let SummaryResult::Failed {
        error: earshot_runtime::RuntimeError::SummaryKeptLocal(m),
        ..
    } = &out.summary
    {
        return Err(IpcError::from(
            earshot_runtime::RuntimeError::SummaryKeptLocal(*m),
        ));
    }
    let cx = crate::summary_order::StepContext::read(svc);
    Ok(JobSummary {
        note: crate::summary_order::steps_text(&out.steps, &cx),
        result: out.summary,
        settled: false,
    })
}

/// What the user is told about the first writer step of `steps` (a "where it runs" choice not
/// used, a stored setting this version does not know), read with no API key.
fn first_writer_notes(
    svc: &Services,
    steps: &[earshot_runtime::summary_order::OrderStep],
    privacy: earshot_runtime::keep_local::SummaryPrivacy,
) -> Vec<String> {
    let Some(first) = steps.iter().find_map(|s| match s {
        earshot_runtime::summary_order::OrderStep::Writer(w) => Some(*w),
        earshot_runtime::summary_order::OrderStep::Computer(_) => None,
    }) else {
        return Vec::new();
    };
    let db = svc.store();
    earshot_runtime::host::summary_for_writer(
        &db,
        first.writer(),
        &earshot_runtime::backends::accelerator_order(),
        &earshot_runtime::hardware(),
        &earshot_runtime::summary_settings::NoApiKeys,
        privacy,
    )
    .map(|s| s.notes)
    .unwrap_or_default()
}

#[cfg(test)]
#[path = "jobs_tests.rs"]
mod tests;
