//! Re-transcribe and re-summarise a stored meeting (T7.3): one job per meeting at a time,
//! each on a thread of its own, its progress and result sent to the UI as [`JobEvent`]s.
//!
//! A re-transcription leases the meeting itself (`edit::retranscribe`, a
//! `lease_meeting_alone(Processing)`); a re-summary takes the same lease here before its
//! thread starts, so it is refused while the meeting records, is processed or is deleted, and
//! a deletion is refused while it runs. The work is behind [`JobWork`], so tests run fakes.

use std::collections::BTreeMap;
use std::panic::AssertUnwindSafe;
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::JoinHandle;

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
/// only), then exactly one of `Retranscribed`, `Summary` or `Failed`.
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
    },
    /// Something the user may need to know while the job runs (a backend fallback).
    Warning {
        /// Meeting id.
        meeting: String,
        /// Plain copy.
        message: String,
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
    /// hears notices for the user (an ONNX Runtime library fallback).
    ///
    /// # Errors
    /// Setup and final-pass errors.
    fn retranscribe(
        &self,
        services: &Services,
        meeting: EntityId,
        progress: &mut dyn FnMut(HostStep<'_>),
        warn: &mut dyn FnMut(String),
    ) -> Result<Retranscribed, IpcError>;

    /// Summarises `meeting` again with the meeting's summary plan; `warn` hears notices for
    /// the user (the chosen graphics card not used).
    ///
    /// # Errors
    /// Setup errors (a summary model that cannot load); a summary that ran and failed is
    /// [`SummaryResult::Failed`].
    fn summarise(
        &self,
        services: &Services,
        meeting: EntityId,
        warn: &mut dyn FnMut(String),
    ) -> Result<SummaryResult, IpcError>;
}

/// Where job events go.
pub type JobSink = Arc<dyn Fn(JobEvent) + Send + Sync>;

struct Inner {
    work: Arc<dyn JobWork>,
    sink: JobSink,
    running: Mutex<BTreeMap<EntityId, (JobKind, JoinHandle<()>)>>,
    gate: crate::activity::WorkGate,
}

/// Runs the jobs (cheap to clone; clones share the jobs).
#[derive(Clone)]
pub struct JobRunner(Arc<Inner>);

fn busy() -> IpcError {
    IpcError {
        kind: "busy".into(),
        message: "This meeting is already being re-transcribed or re-summarised.".into(),
        retryable: true,
    }
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
        let mut running = self
            .0
            .running
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        running.retain(|_, (_, t)| !t.is_finished());
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
        let thread = std::thread::Builder::new()
            .name("earshot-job".into())
            .spawn(move || {
                run_guarded(&inner, &svc, meeting, job, lease);
                drop(pass);
            })
            .map_err(|_| IpcError {
                kind: "internal".into(),
                message: crate::plain_text::INTERNAL.into(),
                retryable: false,
            })?;
        running.insert(meeting, (job, thread));
        Ok(())
    }

    /// Whether a job of `meeting` runs.
    #[cfg(test)]
    #[must_use]
    pub fn running(&self, meeting: EntityId) -> bool {
        self.running_job(meeting).is_some()
    }

    /// The job of `meeting` that runs, if any (T12.38: a meeting deletion names it). Jobs have no
    /// cancel, so a deletion is refused while one runs.
    #[must_use]
    pub fn running_job(&self, meeting: EntityId) -> Option<JobKind> {
        self.0
            .running
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&meeting)
            .filter(|(_, t)| !t.is_finished())
            .map(|(job, _)| *job)
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
        .map(|(_, t)| t)
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
    lease: Option<MeetingLease>,
) {
    let result = std::panic::catch_unwind(AssertUnwindSafe(|| {
        run(inner, svc, meeting, job, lease);
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
    lease: Option<MeetingLease>,
) {
    let sink = &*inner.sink;
    let id = meeting.to_string();
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
            let result = inner.work.summarise(svc, meeting, &mut |message| {
                sink(JobEvent::Warning {
                    meeting: id.clone(),
                    message,
                });
            });
            drop(lease);
            if let Some(message) = result
                .as_ref()
                .ok()
                .and_then(crate::meeting_events::summary_fallback_note)
            {
                sink(JobEvent::Warning {
                    meeting: id.clone(),
                    message,
                });
            }
            sink(match result {
                Ok(r) => JobEvent::Summary {
                    meeting: id.clone(),
                    result: summary_result_event(&r, voices_to_name(svc, meeting)),
                },
                Err(e) => failed(e),
            });
        }
    }
}

/// The production work: the meeting's own model choice and summary plan through
/// `earshot_runtime::host` (the meeting driver's pre-flight rules).
pub struct HostJobs;

impl JobWork for HostJobs {
    fn retranscribe(
        &self,
        svc: &Services,
        meeting: EntityId,
        progress: &mut dyn FnMut(HostStep<'_>),
        warn: &mut dyn FnMut(String),
    ) -> Result<Retranscribed, IpcError> {
        use earshot_core::asr::CancelToken;
        use earshot_runtime::edit::{Retranscribe, retranscribe};
        use earshot_runtime::host;

        let models = host::model_dir(&svc.data);
        // Waits for this start's choice of ONNX Runtime's library, if it is still being made.
        let build = host::settled_order();
        let setup = {
            let db = svc.store();
            host::transcription_setup(&db, &models, &build, &earshot_runtime::hardware())?
        };
        let choice = setup.choice;
        host::require_models(&models, &choice.model, true)?;
        let order = setup.order;
        for note in setup.notes {
            warn(note);
        }
        // A library that cannot load falls back to the CPU one, told as the meeting driver does.
        host::init_onnx_for(&models, &choice.model, &order, &mut |library, e| {
            warn(crate::meeting::onnx_fallback_message(library, e));
        })?;
        // The app has no cancel control yet: the token is never set.
        let cancel = CancelToken::new();
        let run = Retranscribe {
            models: &models,
            model: &choice.model,
            order: &order,
            cancel: &cancel,
        };
        Ok(retranscribe(svc, meeting, &run, progress)?)
    }

    fn summarise(
        &self,
        svc: &Services,
        meeting: EntityId,
        warn: &mut dyn FnMut(String),
    ) -> Result<SummaryResult, IpcError> {
        let models = earshot_runtime::host::model_dir(&svc.data);
        summarise_with(
            svc,
            meeting,
            &models,
            &crate::credentials::CredentialManager::anthropic(),
            &earshot_runtime::claude_code::InstalledClaudeCode::meeting_start(),
            &earshot_runtime::codex::InstalledCodex::meeting_start(),
            warn,
        )
    }
}

/// A re-summary of `meeting` with the summary setup of `models` and `keys`. The user asked
/// for this summary, so an endpoint whose API key cannot be read fails the job with the
/// `api_key` message (a meeting's own summary is skipped instead), and Claude Code that is
/// not installed or not signed in fails it with that reason (`claude_code`), and Codex
/// likewise (`codex`, T7.4gc). A meeting kept on this computer (T12.13) is summarised only by
/// the local summary model, and fails `summary_kept_local` without it.
///
/// # Errors
/// As `meeting::summary_setup`; `api_key` without the endpoint's key; `claude_code`;
/// `codex`; `summary_kept_local`.
pub(crate) fn summarise_with(
    svc: &Services,
    meeting: EntityId,
    models: &earshot_runtime::backends::ModelDir,
    keys: &dyn earshot_runtime::summary_settings::ApiKeyStore,
    claude: &dyn earshot_runtime::claude_code::ClaudeCodeProbe,
    codex: &dyn earshot_runtime::codex::CodexProbe,
    warn: &mut dyn FnMut(String),
) -> Result<SummaryResult, IpcError> {
    use earshot_core::asr::CancelToken;
    use earshot_runtime::host::{self, SkipReason};

    // A meeting kept on this computer (T12.13) is read here, before anything is set up: a
    // record that cannot be read keeps it here (`keep_local::summary_privacy`).
    let privacy = earshot_runtime::keep_local::summary_privacy(&svc.store(), meeting);
    let setup = crate::meeting::summary_setup(svc, models, keys, claude, codex, privacy)?;
    if let Some(e) = setup.chosen_missing {
        return Err(e);
    }
    match setup.no_summary {
        SkipReason::KeptLocalNoModel { model } => {
            return Err(crate::meeting_events::kept_local_error(model));
        }
        SkipReason::NoApiKey(e) => {
            return Err(IpcError::from(earshot_runtime::RuntimeError::ApiKey(e)));
        }
        SkipReason::ClaudeCodeUnavailable(e) => {
            return Err(IpcError::from(earshot_runtime::claude_code::runtime_error(
                e,
            )));
        }
        SkipReason::CodexUnavailable(e) => {
            return Err(IpcError::from(earshot_runtime::codex::runtime_error(e)));
        }
        _ => {}
    }
    for note in setup.notes {
        warn(note);
    }
    Ok(match setup.summary {
        Some(s) => {
            let result = host::run_summary(svc, meeting, &s.service, &CancelToken::new());
            for why in s.service.handover_record_failures() {
                warn(crate::meeting_events::handover_record_copy(&why));
            }
            if host::claude_effort_unread(&s.service, &result) {
                warn(crate::meeting_events::CLAUDE_EFFORT_UNREAD.into());
            }
            result
        }
        None => SummaryResult::Skipped {
            reason: setup.no_summary,
        },
    })
}

#[cfg(test)]
#[path = "jobs_tests.rs"]
mod tests;
