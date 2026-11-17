//! What a summary step did (T6.4c, T12.63a): the service a step runs ([`PreparedSummary`]), its
//! result ([`SummaryResult`]) and the calls that run it ([`run_summary`],
//! [`run_summary_in_run`]). Outside [`crate::host`] so a build without the in-process engines
//! (feature `engines`, T11.6b: the summary worker) runs the order of summary writers; `host`
//! re-exports every item under its old path.

use earshot_core::asr::CancelToken;
use earshot_core::identity::EntityId;

use crate::summary::{SummaryOutcome, SummaryRunReport, SummaryService};
#[cfg(feature = "engines")]
use crate::summary_model::SummaryModelChoice;
use crate::{RuntimeError, Services, os_seeded_rng};

/// A summary service ready for [`crate::host::after_meeting`].
pub struct PreparedSummary {
    /// The service.
    pub service: SummaryService,
    /// For a local model: the model and whether the GPU is requested.
    #[cfg(feature = "engines")]
    pub local: Option<(SummaryModelChoice, bool)>,
}

impl std::fmt::Debug for PreparedSummary {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut d = f.debug_struct("PreparedSummary");
        d.field("model", &self.service.model());
        #[cfg(feature = "engines")]
        d.field("local", &self.local);
        d.finish_non_exhaustive()
    }
}

/// Why no summary was made.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkipReason {
    /// The plan runs none ([`crate::host::SummaryPlan::Skip`]).
    NotRequested,
    /// The default local model is not installed ([`crate::host::SummaryPlan::NotInstalled`]).
    NotInstalled {
        /// Its id.
        model: &'static str,
        /// Why (names the fetch command).
        why: String,
    },
    /// The local model chosen by name is not installed
    /// ([`crate::host::SummaryPlanError::NotInstalled`]): a meeting records and skips the
    /// summary; an explicit re-summary fails.
    ChosenNotInstalled {
        /// Its id.
        model: &'static str,
    },
    /// The post pass failed, so the run stopped there.
    PostFailed,
    /// The chosen endpoint needs an API key that could not be read
    /// ([`crate::host::SummaryPlan::NoApiKey`]).
    NoApiKey(crate::summary_settings::ApiKeyError),
    /// Claude Code was chosen but is not installed or not signed in
    /// ([`crate::host::SummaryPlan::ClaudeCodeUnavailable`]).
    ClaudeCodeUnavailable(earshot_llm::ClaudeCodeError),
    /// Codex was chosen but is not installed or not signed in
    /// ([`crate::host::SummaryPlan::CodexUnavailable`]).
    CodexUnavailable(earshot_llm::CodexError),
    /// The meeting is kept on this computer and no local summary model is installed
    /// ([`crate::host::SummaryPlan::KeptLocalNoModel`]); the remote writer is never used instead.
    KeptLocalNoModel {
        /// The local model it would have used.
        model: &'static str,
    },
    /// The order's local step would have run beside a newer meeting the meeting queue did not
    /// hold for it, and no later step wrote the summary (T12.63a, docs/summary-order.md 4.5).
    NotNow,
}

/// Where a stored summary was made.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SummaryBackend {
    /// The configured endpoint (its requests are in [`crate::host::HostOutcome::network`]).
    Remote,
    /// Claude through the user's Claude Code (its runs are in
    /// [`crate::host::HostOutcome::handovers`]).
    ClaudeCode,
    /// OpenAI through the user's Codex (its runs are in
    /// [`crate::host::HostOutcome::handovers`]).
    Codex,
    /// On this machine, where llama.cpp put the model (`LlamaBackend::name`: "cpu", "cuda", ...).
    Local(String),
}

/// What the summary step did.
#[derive(Debug)]
pub enum SummaryResult {
    /// A summary was stored.
    Stored {
        /// The stored `Summary` row.
        summary_id: EntityId,
        /// The model that made it.
        model: String,
        /// Where it ran.
        backend: SummaryBackend,
        /// The meeting had no title and now carries the summary's.
        title_set: bool,
        /// The whole outcome (the summary, the run's report).
        outcome: Box<SummaryOutcome>,
    },
    /// No summary was made.
    Skipped {
        /// Why.
        reason: SkipReason,
    },
    /// The summary failed; nothing was stored.
    Failed {
        /// Why.
        error: RuntimeError,
        /// Whether retrying can succeed without user action ([`RuntimeError::is_retryable`]).
        retryable: bool,
    },
    /// Cancelled through [`crate::host::AfterMeeting::summary_cancel`]; nothing was stored.
    Cancelled,
    /// Another of the user's devices was asked to write it (summary offload,
    /// [`crate::offload`]): nothing was written here; the summary arrives by sync, or this
    /// device writes it if the helper does not ([`crate::offload::Offload::step`]).
    Offloaded {
        /// The request.
        request: EntityId,
        /// The helper asked.
        device: earshot_core::identity::DeviceId,
        /// Its name, from its presence; empty when it is not the chosen device (the meeting
        /// already waited for another).
        name: String,
        /// The kind of writer it uses, when known (the chosen device's).
        writer: Option<crate::offload::WriterKind>,
    },
}

/// Whether to say that `service`'s Claude Code wrote the summary of `result` at its own default
/// effort (T12.20): only for a stored summary whose run found the help unreadable. The rule of
/// [`crate::host::HostOutcome::claude_effort_unread`] and of the app's re-summary job.
#[must_use]
pub fn claude_effort_unread(service: &SummaryService, result: &SummaryResult) -> bool {
    matches!(result, SummaryResult::Stored { .. }) && service.claude_effort_unread()
}

/// Runs `service` on `meeting` ([`SummaryService::summarize`]) and says what happened.
#[must_use]
pub fn run_summary(
    services: &Services,
    meeting: EntityId,
    service: &SummaryService,
    cancel: &CancelToken,
) -> SummaryResult {
    summary_result(
        service,
        service.summarize(services, meeting, &mut *os_seeded_rng(), cancel),
    )
}

/// [`run_summary`] as one step of a run of the order (T12.63a): the run's ended record is stored
/// with the summary ([`SummaryService::summarize_in_run`]).
#[must_use]
pub fn run_summary_in_run(
    services: &Services,
    meeting: EntityId,
    service: &SummaryService,
    end: &crate::summary_order::run::RunEnd,
    cancel: &CancelToken,
) -> SummaryResult {
    summary_result(
        service,
        service.summarize_in_run(services, meeting, end, &mut *os_seeded_rng(), cancel),
    )
}

fn summary_result(
    service: &SummaryService,
    result: Result<SummaryOutcome, RuntimeError>,
) -> SummaryResult {
    match result {
        Ok(outcome) => SummaryResult::Stored {
            summary_id: outcome.summary_id,
            model: service.model().to_owned(),
            backend: match &outcome.report {
                SummaryRunReport::Remote(_) => SummaryBackend::Remote,
                SummaryRunReport::ClaudeCode(_) => SummaryBackend::ClaudeCode,
                SummaryRunReport::Codex(_) => SummaryBackend::Codex,
                #[cfg(feature = "engines")]
                SummaryRunReport::Local(r) => SummaryBackend::Local(r.backend.name().to_owned()),
            },
            title_set: outcome.title_set,
            outcome: Box::new(outcome),
        },
        // Only the cancellation itself; any other failure stays one whatever the token says.
        Err(RuntimeError::Llm(earshot_llm::LlmError::Cancelled)) => SummaryResult::Cancelled,
        #[cfg(feature = "engines")]
        Err(RuntimeError::LocalSummary(crate::summary::LocalSummaryError::Run(
            earshot_llm_local::summarize::SummaryFailure::Cancelled,
        ))) => SummaryResult::Cancelled,
        Err(error) => SummaryResult::Failed {
            retryable: error.is_retryable(),
            error,
        },
    }
}
