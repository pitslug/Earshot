//! Running the order of summary writers for one meeting (T12.63a, docs/summary-order.md section
//! 4): the meeting's run record, the closed move-on table, computer steps as suspensions and how a
//! hand-back continues the run.
//!
//! # The run record
//!
//! `summary-run/<meeting>` ([`record_key`], `version u32 LE | JSON`, fixed words, step and request
//! ids only, swept by meeting deletion): [`SummaryRun`]. Every write re-reads the record in one
//! store transaction and writes only a record of its own run ([`SummaryRun::run`]):
//!
//! - starting a run replaces it and withdraws any open request this device asked for the meeting
//!   in the same transaction ([`run_summary_order`]);
//! - asking a computer sets `waiting_on` and `state: waiting` in the transaction that writes the
//!   request ([`crate::offload::ask_for_run`]);
//! - `state: ended` is written in the summary's own write transaction
//!   ([`crate::summary::SummaryService::summarize_in_run`]), when the run stops, when every step
//!   failed, or when the computer's `Done` arrives ([`end_after_done`]).
//!
//! # Moving on
//!
//! The next step runs only for the results of the closed table ([`skip_word`],
//! [`failure_word`], [`unavailable_word`]); anything else stops the run with that error.
//!
//! # Privacy
//!
//! A summary goes only to the steps of the order, in order; the one exception is the local step a
//! meeting kept on this computer always ends with ([`super::SummaryOrder::for_meeting`]). A named
//! run has one step and no next step. A continuation runs the steps copied when the run started,
//! never the current order.

use earshot_core::asr::CancelToken;
use earshot_core::identity::{DeviceId, EntityId};
use earshot_core::model::{MeetingRepo, RequestState, SummaryRequestRepo};
use earshot_net::AuditEntry;
use earshot_store::{Store, StoreTx};
use rand_core::Rng;
use serde::{Deserialize, Serialize};

use super::{OrderStep, SummaryOrder, WriterStep};
use crate::host::{PreparedSummary, SkipReason, SummaryResult};
use crate::keep_local::SummaryPrivacy;
use crate::local::{LocalKey, MeetingRecord};
use crate::offload::{HelperWriter, OffloadChoice, Unavailable, WriterKind, reason};
use crate::{RuntimeError, Services};

/// The run record's format version.
pub const RUN_VERSION: u32 = 1;

/// The key of `meeting`'s run record.
#[must_use]
pub(crate) fn record_key(meeting: EntityId) -> LocalKey {
    MeetingRecord::SummaryRun.key(&[], meeting, &[])
}

/// Where a run came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunOrigin {
    /// The order: an automatic summary, or a re-summarise "Use my order".
    Order,
    /// A re-summarise naming one writer: one step, no next step.
    Named,
}

/// Where a run stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunState {
    /// Trying its steps.
    Running,
    /// A computer step waits for that computer ([`SummaryRun::waiting_on`]).
    Waiting,
    /// A re-summarise job continues it after the computer handed it back.
    Continuing,
    /// Over: a summary was stored, it stopped, or every step failed.
    Ended,
}

/// Why a step was left for the next one (fixed words, docs/summary-order.md 4.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OutcomeWord {
    /// The computer is offline, or sync could not reach it.
    Offline,
    /// The computer is recording a meeting.
    Recording,
    /// The computer does not write summaries for the others.
    NotOffering,
    /// The computer has no summary writer.
    NoWriter,
    /// The computer did not claim the request, or went quiet after claiming it.
    NoReply,
    /// The computer declined.
    Declined,
    /// The local model, or the agent CLI, is not installed.
    NotInstalled,
    /// The endpoint needs an API key that could not be read.
    NoApiKey,
    /// The agent CLI is not signed in.
    NotSignedIn,
    /// The writer failed.
    Failed,
    /// The local model would have run beside a newer meeting the queue did not hold for it.
    NotNow,
    /// Dropped for a meeting kept on this computer.
    SkippedKeptLocal,
}

impl OutcomeWord {
    /// Every word.
    pub const ALL: [Self; 12] = [
        Self::Offline,
        Self::Recording,
        Self::NotOffering,
        Self::NoWriter,
        Self::NoReply,
        Self::Declined,
        Self::NotInstalled,
        Self::NoApiKey,
        Self::NotSignedIn,
        Self::Failed,
        Self::NotNow,
        Self::SkippedKeptLocal,
    ];

    /// The words the user reads.
    #[must_use]
    pub const fn text(self) -> &'static str {
        match self {
            Self::Offline => "offline",
            Self::Recording => "recording",
            Self::NotOffering => "not offering",
            Self::NoWriter => "no writer",
            Self::NoReply => "no reply",
            Self::Declined => "declined",
            Self::NotInstalled => "not installed",
            Self::NoApiKey => "no API key",
            Self::NotSignedIn => "not signed in",
            Self::Failed => "failed",
            Self::NotNow => "not now",
            Self::SkippedKeptLocal => "skipped (kept on this computer)",
        }
    }
}

/// One step tried.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StepOutcome {
    /// Its index in [`SummaryRun::steps`].
    pub step: usize,
    /// Why the run went on.
    pub word: OutcomeWord,
}

/// A meeting's run of the order (module docs).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SummaryRun {
    /// Which run this is (random): a write changes only a record of its own run.
    pub run: u64,
    /// Where it came from.
    pub origin: RunOrigin,
    /// Its steps, copied when it started (a meeting kept on this computer's implicit local step
    /// included).
    pub steps: Vec<OrderStep>,
    /// The next step to try.
    pub cursor: usize,
    /// One per step tried and left (dropped steps included).
    #[serde(default)]
    pub outcomes: Vec<StepOutcome>,
    /// The request a computer step waits for.
    #[serde(default)]
    pub waiting_on: Option<EntityId>,
    /// Where it stands.
    pub state: RunState,
    /// The step that wrote the summary, once one did.
    #[serde(default)]
    pub writer: Option<usize>,
}

impl SummaryRun {
    fn new(origin: RunOrigin) -> Self {
        Self {
            run: crate::os_seeded_rng().next_u64(),
            origin,
            steps: Vec::new(),
            cursor: 0,
            outcomes: Vec::new(),
            waiting_on: None,
            state: RunState::Running,
            writer: None,
        }
    }

    /// The outcome recorded for step `i`.
    #[must_use]
    pub fn outcome_of(&self, i: usize) -> Option<OutcomeWord> {
        self.outcomes.iter().find(|o| o.step == i).map(|o| o.word)
    }

    /// The first step at or after `from` with no outcome.
    fn next_step(&self, from: usize) -> Option<usize> {
        (from..self.steps.len()).find(|i| self.outcome_of(*i).is_none())
    }

    fn push(&mut self, step: usize, word: OutcomeWord) {
        if self.outcome_of(step).is_none() {
            self.outcomes.push(StepOutcome { step, word });
        }
    }

    /// Whether its shape holds (indexes inside its steps).
    fn holds(&self) -> bool {
        let n = self.steps.len();
        self.cursor <= n
            && self.outcomes.iter().all(|o| o.step < n)
            && self.writer.is_none_or(|w| w < n)
            && n <= super::MAX_STEPS + 1
    }
}

/// A meeting's run record as read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunRecord {
    /// None stored (no run since this version, or a request asked before it).
    Absent,
    /// The record.
    Read(SummaryRun),
    /// Damaged, or a version this build does not know: fails closed.
    Unreadable,
}

/// The stored bytes of `run`.
#[must_use]
pub fn encode(run: &SummaryRun) -> Vec<u8> {
    earshot_store::encode_versioned(RUN_VERSION, run).expect("a run serialises")
}

/// The record in `bytes` (`None`: nothing stored).
#[must_use]
pub fn decode(bytes: Option<&[u8]>) -> RunRecord {
    let Some(bytes) = bytes else {
        return RunRecord::Absent;
    };
    earshot_store::split_version(bytes)
        .filter(|(v, _)| *v == RUN_VERSION)
        .and_then(|(_, payload)| serde_json::from_slice::<SummaryRun>(payload).ok())
        .filter(SummaryRun::holds)
        .map_or(RunRecord::Unreadable, RunRecord::Read)
}

/// `meeting`'s run record.
///
/// # Errors
/// Store errors.
pub fn read(store: &Store, meeting: EntityId) -> Result<RunRecord, RuntimeError> {
    Ok(decode(
        store.local_value(record_key(meeting).as_str())?.as_deref(),
    ))
}

fn read_tx(tx: &StoreTx<'_>, meeting: EntityId) -> Result<RunRecord, RuntimeError> {
    Ok(decode(
        tx.local_value(record_key(meeting).as_str())?.as_deref(),
    ))
}

fn put_tx(tx: &mut StoreTx<'_>, meeting: EntityId, run: &SummaryRun) -> Result<(), RuntimeError> {
    crate::local::set_tx(tx, &record_key(meeting), &encode(run))?;
    Ok(())
}

/// Writes `run` over `meeting`'s record inside `tx` only when the stored record is still of the
/// same run; returns whether it wrote.
pub(crate) fn put_own_tx(
    tx: &mut StoreTx<'_>,
    meeting: EntityId,
    run: &SummaryRun,
) -> Result<bool, RuntimeError> {
    match read_tx(tx, meeting)? {
        RunRecord::Read(stored) if stored.run == run.run => {
            put_tx(tx, meeting, run)?;
            Ok(true)
        }
        _ => Ok(false),
    }
}

/// [`put_own_tx`] in a transaction of its own.
fn put_own(services: &Services, meeting: EntityId, run: &SummaryRun) -> Result<bool, RuntimeError> {
    services
        .store()
        .transaction(|tx| -> Result<bool, RuntimeError> { put_own_tx(tx, meeting, run) })
}

/// The ended record a summary's write transaction stores ([`SummaryService::summarize_in_run`]).
///
/// [`SummaryService::summarize_in_run`]: crate::summary::SummaryService::summarize_in_run
#[derive(Debug, Clone)]
pub struct RunEnd {
    pub(crate) record: SummaryRun,
}

/// Stores `end` in the summary's write transaction, when the record is still of its run.
pub(crate) fn end_in_tx(
    tx: &mut StoreTx<'_>,
    meeting: EntityId,
    end: &RunEnd,
) -> Result<(), RuntimeError> {
    put_own_tx(tx, meeting, &end.record).map(|_| ())
}

/// Faults the tests inject inside the transactions this module names (each then rolls back).
#[cfg(test)]
pub(crate) mod fault {
    use std::cell::Cell;

    use crate::RuntimeError;

    thread_local! {
        /// 0: none; 1: a fault that is not a store error; 2: a store error.
        static ASK: Cell<u8> = const { Cell::new(0) };
    }

    /// Fails the next computer ask on this thread after its request is written and before the
    /// record names it, with an error that is not a store error ("request not written").
    pub(crate) fn fail_ask() {
        ASK.with(|f| f.set(1));
    }

    /// As [`fail_ask`], with a store error.
    pub(crate) fn fail_ask_store() {
        ASK.with(|f| f.set(2));
    }

    pub(crate) fn take_ask() -> Option<RuntimeError> {
        match ASK.with(|f| f.replace(0)) {
            1 => Some(RuntimeError::Internal("an injected fault")),
            2 => Some(RuntimeError::Store(earshot_store::StoreError::Busy(
                "an injected fault".into(),
            ))),
            _ => None,
        }
    }
}

// ---- the move-on table (docs/summary-order.md 4.3) ----

/// The word of a writer step that ran no summary for `reason`, when the run moves on for it;
/// `None` stops the run.
#[must_use]
pub fn skip_word(reason: &SkipReason) -> Option<OutcomeWord> {
    match reason {
        SkipReason::NotInstalled { .. }
        | SkipReason::ChosenNotInstalled { .. }
        | SkipReason::KeptLocalNoModel { .. } => Some(OutcomeWord::NotInstalled),
        SkipReason::NoApiKey(_) => Some(OutcomeWord::NoApiKey),
        SkipReason::ClaudeCodeUnavailable(e) => claude_code_word(e),
        SkipReason::CodexUnavailable(e) => codex_word(e),
        SkipReason::NotNow => Some(OutcomeWord::NotNow),
        SkipReason::NotRequested | SkipReason::PostFailed => None,
    }
}

/// The word of writer step `step` whose summary failed with `error`, when the run moves on for
/// it; `None` stops the run (cancellation, a changed transcript or deleted meeting, a busy
/// meeting, store errors, a handover that could not be recorded, and anything not listed). The
/// writers' own error enums are matched without a wildcard, so a new variant does not compile
/// until it is classified here.
#[must_use]
pub fn failure_word(step: WriterStep, error: &RuntimeError) -> Option<OutcomeWord> {
    use earshot_llm::LlmError;
    match (step, error) {
        (WriterStep::Local, RuntimeError::LocalSummary(e)) => local_word(e),
        (WriterStep::Local, RuntimeError::ModelMissing { .. }) => Some(OutcomeWord::NotInstalled),
        (WriterStep::Endpoint, RuntimeError::ApiKey(_)) => Some(OutcomeWord::NoApiKey),
        (_, RuntimeError::Llm(e)) => match e {
            // Listed for every writer: still invalid after the repair rounds, or too long.
            LlmError::Invalid { .. } | LlmError::Prompt(_) => Some(OutcomeWord::Failed),
            LlmError::Http { .. }
            | LlmError::Net(_)
            | LlmError::Refused { .. }
            | LlmError::FormatRejected { .. } => {
                (step == WriterStep::Endpoint).then_some(OutcomeWord::Failed)
            }
            LlmError::ClaudeCode(e) => {
                if step == WriterStep::ClaudeCode {
                    claude_code_word(e)
                } else {
                    None
                }
            }
            LlmError::Codex(e) => {
                if step == WriterStep::Codex {
                    codex_word(e)
                } else {
                    None
                }
            }
            LlmError::Endpoint(_) | LlmError::BadResponse(_) | LlmError::Cancelled => None,
        },
        _ => None,
    }
}

/// The local model's failures: every one moves on (each after its own CPU retry) except a
/// cancellation.
fn local_word(e: &crate::summary::LocalSummaryError) -> Option<OutcomeWord> {
    use crate::summary::LocalSummaryError as L;
    use earshot_llm_local::summarize::SummaryFailure as F;
    match e {
        L::Run(F::Cancelled) => None,
        L::Run(
            F::Plan(_) | F::TooLong { .. } | F::Grammar(_) | F::Model(_) | F::Invalid { .. },
        )
        | L::Load(_)
        | L::LoadRetried { .. }
        | L::GpuStalled
        | L::LoadStalled
        | L::GpuChildStalled
        | L::GpuChildDied { .. }
        | L::CudaFilesChanged { .. }
        | L::ChildRun(_)
        | L::Chunks(_) => Some(OutcomeWord::Failed),
    }
}

/// Claude Code's errors: all move on except a handover that could not be recorded.
fn claude_code_word(e: &earshot_llm::ClaudeCodeError) -> Option<OutcomeWord> {
    use earshot_llm::ClaudeCodeError as E;
    match e {
        E::NotRecorded => None,
        E::NotInstalled => Some(OutcomeWord::NotInstalled),
        E::NotSignedIn | E::SignInExpired => Some(OutcomeWord::NotSignedIn),
        E::SignInBusy
        | E::PromptTooLong
        | E::UnsupportedVersion
        | E::Start(_)
        | E::TimedOut(_)
        | E::OutputIncomplete
        | E::BadOutput(_)
        | E::Failed { .. } => Some(OutcomeWord::Failed),
    }
}

/// Codex's errors: all move on except a handover that could not be recorded.
fn codex_word(e: &earshot_llm::CodexError) -> Option<OutcomeWord> {
    use earshot_llm::CodexError as E;
    match e {
        E::NotRecorded => None,
        E::NotInstalled => Some(OutcomeWord::NotInstalled),
        E::NotSignedIn | E::SignInExpired => Some(OutcomeWord::NotSignedIn),
        E::AccessDenied
        | E::PromptTooLong
        | E::UnsupportedVersion
        | E::ToolRan
        | E::UnknownEvent
        | E::Start(_)
        | E::TimedOut(_)
        | E::OutputIncomplete
        | E::BadOutput(_)
        | E::Failed { .. } => Some(OutcomeWord::Failed),
    }
}

/// Whether a computer step's failed ask stops the run: a store error, a deleted meeting, or the
/// run replaced meanwhile ([`crate::offload::ask_for_run`]'s conflict). Any other error is the
/// table's "request not written" and moves on.
#[must_use]
pub fn stops_ask(error: &RuntimeError) -> bool {
    matches!(
        error,
        RuntimeError::Store(_)
            | RuntimeError::Repo(_)
            | RuntimeError::Conflict(_)
            | RuntimeError::Edit(crate::edit::EditError::MeetingNotFound(_))
    )
}

/// The word of a computer step not asked for `why` (the run always moves on).
#[must_use]
pub const fn unavailable_word(why: Unavailable) -> OutcomeWord {
    match why {
        Unavailable::SyncOff | Unavailable::Unreachable | Unavailable::Offline => {
            OutcomeWord::Offline
        }
        Unavailable::NotOffering => OutcomeWord::NotOffering,
        Unavailable::NoWriter => OutcomeWord::NoWriter,
        Unavailable::Recording => OutcomeWord::Recording,
        Unavailable::NoFinalTranscript | Unavailable::NotAsked => OutcomeWord::Failed,
    }
}

// ---- running ----

/// A writer step built for one try.
pub enum Built {
    /// The service to run.
    Service(Box<PreparedSummary>),
    /// No summary from this writer, with why (moves on when [`skip_word`] says so).
    Skip(SkipReason),
    /// The writer cannot be set up: the run stops with this error.
    Stop(RuntimeError),
}

/// How a run gets each step (production: [`crate::host::SettingsWriters`]). Asked only when the
/// step is tried, never for a dropped step.
pub trait StepWriters {
    /// The service of `step` from this computer's settings now (an API key read, a CLI probed).
    fn writer(&self, services: &Services, step: WriterStep) -> Built;
    /// Whether to ask `device` now ([`crate::offload::decide`] over its presence).
    fn computer(&self, services: &Services, device: DeviceId) -> OffloadChoice;
}

/// What a run needs from its host.
pub struct RunPlan<'a> {
    /// The steps.
    pub writers: &'a dyn StepWriters,
    /// Whether a local step would now run beside a newer meeting the meeting queue did not hold
    /// for it (docs/summary-order.md 4.5: a meeting whose hold was recorded without the local
    /// model while a newer meeting's ticket is open). A re-summarise job: never.
    pub local_not_now: &'a dyn Fn() -> bool,
}

/// How a run starts.
#[derive(Debug, Clone)]
pub enum RunStart {
    /// The order (an automatic summary, or "Use my order"), filtered by the meeting's privacy.
    Order(SummaryOrder),
    /// One writer or computer named by a re-summarise: not filtered, no next step.
    Named(OrderStep),
    /// A re-summarise job continuing the run that waited on `request`, which the computer handed
    /// back with `word`.
    Continue {
        /// The request handed back.
        request: EntityId,
        /// The computer's outcome.
        word: OutcomeWord,
    },
    /// A request asked before this version was handed back: written here with the order's first
    /// writer step under the meeting's privacy, as an ordinary re-summarise reads it.
    Legacy,
}

/// What became of one step of a run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StepResult {
    /// It wrote the summary.
    Wrote,
    /// A computer was asked and the run waits for it.
    Asked,
    /// It was left for the next step.
    Moved(OutcomeWord),
    /// Not tried.
    NotTried,
}

/// One step as the host shows it (the CLI prints one line each).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RunStepView {
    /// The step.
    pub step: OrderStep,
    /// What became of it.
    pub result: StepResult,
}

/// What a run did.
#[derive(Debug)]
pub struct RunOutcome {
    /// The summary, as one writer's result says it (the last step tried when every step failed).
    pub summary: SummaryResult,
    /// Every step of the run.
    pub steps: Vec<RunStepView>,
    /// Every network request any step made (the audit is their union).
    pub network: Vec<AuditEntry>,
    /// Every handover to an agent CLI any step made.
    pub handovers: Vec<crate::claude_code::HandoverEntry>,
    /// Handover records that could not be stored (store error text).
    pub handover_record_failures: Vec<String>,
    /// A stored summary was written by Claude Code at its default effort because its help could
    /// not be read ([`crate::host::claude_effort_unread`]).
    pub claude_effort_unread: bool,
    /// The first computer step's decision (one-step orders keep today's banner from it).
    pub offload: Option<OffloadChoice>,
    /// A continuation found nothing to continue (the record no longer waits on its request):
    /// nothing was run, written or sent.
    pub settled: bool,
}

impl RunOutcome {
    fn new(summary: SummaryResult) -> Self {
        Self {
            summary,
            steps: Vec::new(),
            network: Vec::new(),
            handovers: Vec::new(),
            handover_record_failures: Vec::new(),
            claude_effort_unread: false,
            offload: None,
            settled: false,
        }
    }

    fn failed(error: RuntimeError) -> Self {
        Self::new(SummaryResult::Failed {
            retryable: error.is_retryable(),
            error,
        })
    }

    /// No run: the summary failed with `error` before it started.
    #[must_use]
    pub fn from_error(error: RuntimeError) -> Self {
        Self::failed(error)
    }

    /// No run: skipped for `reason` (the final pass failed).
    #[must_use]
    pub fn skipped(reason: SkipReason) -> Self {
        Self::new(SummaryResult::Skipped { reason })
    }

    /// No run: cancelled before it started.
    #[must_use]
    pub fn cancelled() -> Self {
        Self::new(SummaryResult::Cancelled)
    }

    fn with_steps(mut self, run: &SummaryRun) -> Self {
        self.steps = views(run);
        self
    }
}

impl RunStepView {
    /// One fixed line for the CLI: the step and what became of it (a computer by its device id,
    /// never a meeting word).
    #[must_use]
    pub fn line(&self) -> String {
        let step = match self.step {
            OrderStep::Writer(WriterStep::Local) => "the local model".to_owned(),
            OrderStep::Writer(WriterStep::Endpoint) => "your endpoint".to_owned(),
            OrderStep::Writer(WriterStep::ClaudeCode) => "Claude Code".to_owned(),
            OrderStep::Writer(WriterStep::Codex) => "Codex".to_owned(),
            OrderStep::Computer(d) => format!("computer {d}"),
        };
        let result = match self.result {
            StepResult::Wrote => "wrote the summary",
            StepResult::Asked => "asked, waiting",
            StepResult::Moved(word) => word.text(),
            StepResult::NotTried => "not tried",
        };
        format!("{step}: {result}")
    }
}

/// The steps of `run` as the host shows them.
#[must_use]
pub fn views(run: &SummaryRun) -> Vec<RunStepView> {
    run.steps
        .iter()
        .enumerate()
        .map(|(i, step)| RunStepView {
            step: *step,
            result: if run.writer == Some(i) {
                StepResult::Wrote
            } else if let Some(word) = run.outcome_of(i) {
                StepResult::Moved(word)
            } else if run.state == RunState::Waiting && run.cursor == i + 1 {
                StepResult::Asked
            } else {
                StepResult::NotTried
            },
        })
        .collect()
}

/// Starts `run` for `meeting`: replaces the record and withdraws every open request this device
/// asked for the meeting, in one transaction.
fn start(services: &Services, meeting: EntityId, run: &SummaryRun) -> Result<(), RuntimeError> {
    let me = services.store().device();
    crate::edit::write(services, |tx, stamp| {
        crate::edit::live_meeting(&*tx, meeting)?;
        for mut r in tx.summary_requests()? {
            if r.meeting_id == meeting
                && r.asking_device == me
                && !r.meta.deleted
                && r.state.is_open()
                && crate::offload::authored(&r)
            {
                r.state = RequestState::Withdrawn;
                r.reason = String::new();
                r.meta.touch(stamp.hlc, stamp.clock);
                tx.put_summary_request(r)?;
            }
        }
        put_tx(tx, meeting, run)
    })
}

/// The first transaction of a continuation: the computer's outcome recorded, `waiting_on`
/// cleared, `state: continuing`, only when the record still waits on `request`. Returns the
/// record to continue.
fn begin_continue(
    services: &Services,
    meeting: EntityId,
    request: EntityId,
    word: OutcomeWord,
) -> Result<Option<SummaryRun>, RuntimeError> {
    services
        .store()
        .transaction(|tx| -> Result<Option<SummaryRun>, RuntimeError> {
            let RunRecord::Read(mut run) = read_tx(tx, meeting)? else {
                return Ok(None);
            };
            if run.waiting_on != Some(request)
                || run.state != RunState::Waiting
                || run.origin != RunOrigin::Order
            {
                return Ok(None);
            }
            run.push(run.cursor.saturating_sub(1), word);
            run.waiting_on = None;
            run.state = RunState::Continuing;
            put_tx(tx, meeting, &run)?;
            Ok(Some(run))
        })
}

/// Runs a summary of `meeting` as `start` says (module docs), inside the caller's queue turn and
/// lease. Each step builds its own service when it is tried.
#[must_use]
pub fn run_summary_order(
    services: &Services,
    meeting: EntityId,
    start_as: RunStart,
    plan: &RunPlan<'_>,
    cancel: &CancelToken,
) -> RunOutcome {
    let privacy = crate::keep_local::summary_privacy(&services.store(), meeting);
    let mut run = match start_as {
        RunStart::Order(order) => {
            let mut run = SummaryRun::new(RunOrigin::Order);
            for (i, (step, dropped)) in order.for_meeting(privacy).into_iter().enumerate() {
                run.steps.push(step);
                if dropped {
                    run.push(i, OutcomeWord::SkippedKeptLocal);
                }
            }
            run
        }
        RunStart::Named(step) => {
            let mut run = SummaryRun::new(RunOrigin::Named);
            run.steps.push(step);
            run
        }
        RunStart::Legacy => {
            let order = match crate::summary_order::summary_order(&services.store()) {
                Ok((order, _)) => order,
                Err(e) => return RunOutcome::failed(e),
            };
            let mut run = SummaryRun::new(RunOrigin::Named);
            if let Some(w) = order.first_writer() {
                run.steps
                    .push(if privacy == SummaryPrivacy::KeptLocal && w.is_remote() {
                        OrderStep::LOCAL
                    } else {
                        OrderStep::Writer(w)
                    });
            }
            run
        }
        RunStart::Continue { request, word } => {
            match begin_continue(services, meeting, request, word) {
                Ok(Some(run)) => run,
                Ok(None) => {
                    let mut out = RunOutcome::new(SummaryResult::Skipped {
                        reason: SkipReason::NotRequested,
                    });
                    out.settled = true;
                    return out;
                }
                Err(e) => return RunOutcome::failed(e),
            }
        }
    };
    if run.state == RunState::Running {
        if run.steps.is_empty() {
            run.state = RunState::Ended;
        }
        if let Err(e) = start(services, meeting, &run) {
            return RunOutcome::failed(e);
        }
        if run.state == RunState::Ended {
            return RunOutcome::new(SummaryResult::Skipped {
                reason: SkipReason::NotRequested,
            })
            .with_steps(&run);
        }
    }
    run_steps(services, meeting, run, privacy, plan, cancel)
}

/// Ends `run` (no summary written by it) and returns `out` with its steps.
fn ended(
    services: &Services,
    meeting: EntityId,
    mut run: SummaryRun,
    out: RunOutcome,
) -> RunOutcome {
    run.state = RunState::Ended;
    run.waiting_on = None;
    run.cursor = run.cursor.min(run.steps.len());
    let mut out = out;
    if let Err(e) = put_own(services, meeting, &run)
        && !matches!(out.summary, SummaryResult::Failed { .. })
    {
        out.summary = SummaryResult::Failed {
            retryable: e.is_retryable(),
            error: e,
        };
    }
    out.with_steps(&run)
}

fn stop(
    services: &Services,
    meeting: EntityId,
    run: SummaryRun,
    mut out: RunOutcome,
    summary: SummaryResult,
) -> RunOutcome {
    out.summary = summary;
    ended(services, meeting, run, out)
}

#[allow(clippy::too_many_lines)] // One loop over the closed table, read top to bottom.
fn run_steps(
    services: &Services,
    meeting: EntityId,
    mut run: SummaryRun,
    privacy: SummaryPrivacy,
    plan: &RunPlan<'_>,
    cancel: &CancelToken,
) -> RunOutcome {
    let mut out = RunOutcome::new(SummaryResult::Skipped {
        reason: SkipReason::NotRequested,
    });
    let mut last: Option<SummaryResult> = None;
    while let Some(i) = run.next_step(run.cursor) {
        if cancel.is_cancelled() {
            return stop(services, meeting, run, out, SummaryResult::Cancelled);
        }
        run.cursor = i;
        match run.steps[i] {
            OrderStep::Writer(w) => {
                // A named remote writer on a meeting kept on this computer: today's refusal,
                // before anything is read or probed (section 5).
                if w.is_remote() && privacy == SummaryPrivacy::KeptLocal {
                    let error = RuntimeError::SummaryKeptLocal(meeting);
                    let failed = SummaryResult::Failed {
                        retryable: error.is_retryable(),
                        error,
                    };
                    return stop(services, meeting, run, out, failed);
                }
                if w == WriterStep::Local && (plan.local_not_now)() {
                    run.push(i, OutcomeWord::NotNow);
                    last = Some(SummaryResult::Skipped {
                        reason: SkipReason::NotNow,
                    });
                    run.cursor = i + 1;
                    continue;
                }
                match plan.writers.writer(services, w) {
                    Built::Skip(reason) => match skip_word(&reason) {
                        Some(word) => {
                            run.push(i, word);
                            last = Some(SummaryResult::Skipped { reason });
                        }
                        None => {
                            return stop(
                                services,
                                meeting,
                                run,
                                out,
                                SummaryResult::Skipped { reason },
                            );
                        }
                    },
                    Built::Stop(error) => {
                        let failed = SummaryResult::Failed {
                            retryable: error.is_retryable(),
                            error,
                        };
                        return stop(services, meeting, run, out, failed);
                    }
                    Built::Service(prepared) => {
                        if cancel.is_cancelled() {
                            return stop(services, meeting, run, out, SummaryResult::Cancelled);
                        }
                        let mut end = run.clone();
                        end.state = RunState::Ended;
                        end.writer = Some(i);
                        end.cursor = i + 1;
                        end.waiting_on = None;
                        let service = &prepared.service;
                        let result = crate::host::run_summary_in_run(
                            services,
                            meeting,
                            service,
                            &RunEnd {
                                record: end.clone(),
                            },
                            cancel,
                        );
                        out.network.extend(service.audit_log().entries());
                        out.handovers.extend(service.handovers());
                        out.handover_record_failures
                            .extend(service.handover_record_failures());
                        match result {
                            SummaryResult::Stored { .. } => {
                                out.claude_effort_unread =
                                    crate::host::claude_effort_unread(service, &result);
                                out.summary = result;
                                return out.with_steps(&end);
                            }
                            SummaryResult::Failed { error, retryable } => {
                                match failure_word(w, &error) {
                                    Some(word) => {
                                        run.push(i, word);
                                        last = Some(SummaryResult::Failed { error, retryable });
                                    }
                                    None => {
                                        let failed = SummaryResult::Failed { error, retryable };
                                        return stop(services, meeting, run, out, failed);
                                    }
                                }
                            }
                            other => return stop(services, meeting, run, out, other),
                        }
                    }
                }
            }
            OrderStep::Computer(device) => {
                let choice = plan.writers.computer(services, device);
                if out.offload.is_none() {
                    out.offload = Some(choice.clone());
                }
                match choice {
                    // Trust boundary: the host's answer names a device, and the transcript goes
                    // only to the computer the user put in this step. An answer for any other
                    // device is not asked.
                    OffloadChoice::Ask {
                        device: answered, ..
                    } if answered != device => {
                        run.push(i, unavailable_word(Unavailable::NotAsked));
                    }
                    OffloadChoice::Ask { name, writer, .. } => {
                        let mut waiting = run.clone();
                        waiting.cursor = i + 1;
                        waiting.state = RunState::Waiting;
                        match crate::offload::ask_for_run(services, meeting, device, &waiting) {
                            Ok(Some(asked)) => {
                                waiting.waiting_on = Some(asked.request);
                                let same = asked.target == device;
                                out.summary = SummaryResult::Offloaded {
                                    request: asked.request,
                                    device: asked.target,
                                    name: if same { name } else { String::new() },
                                    writer: same.then_some(writer),
                                };
                                return out.with_steps(&waiting);
                            }
                            Ok(None) => {
                                run.push(i, unavailable_word(Unavailable::NoFinalTranscript))
                            }
                            // A store error, a deleted meeting or a run replaced meanwhile
                            // stops the run; only a request that was otherwise not written
                            // moves on.
                            Err(error) if stops_ask(&error) => {
                                let failed = SummaryResult::Failed {
                                    retryable: error.is_retryable(),
                                    error,
                                };
                                return stop(services, meeting, run, out, failed);
                            }
                            Err(_) => run.push(i, unavailable_word(Unavailable::NotAsked)),
                        }
                    }
                    OffloadChoice::HereInstead { why, .. } => run.push(i, unavailable_word(why)),
                    OffloadChoice::Here => run.push(i, OutcomeWord::Offline),
                }
                if last.is_none() {
                    last = Some(SummaryResult::Skipped {
                        reason: SkipReason::NotRequested,
                    });
                }
            }
        }
        run.cursor = i + 1;
    }
    // Every step failed, or none was left.
    let dropped = run
        .outcomes
        .iter()
        .any(|o| o.word == OutcomeWord::SkippedKeptLocal);
    out.summary = match last {
        // A meeting kept on this computer whose only writer left was the local model, and that
        // is not installed: today's kept-local result (T12.13).
        Some(SummaryResult::Skipped {
            reason:
                SkipReason::NotInstalled { model, .. } | SkipReason::ChosenNotInstalled { model },
        }) if dropped && privacy == SummaryPrivacy::KeptLocal => SummaryResult::Skipped {
            reason: SkipReason::KeptLocalNoModel { model },
        },
        Some(r) => r,
        None => SummaryResult::Skipped {
            reason: SkipReason::NotRequested,
        },
    };
    run.cursor = run.steps.len();
    ended(services, meeting, run, out)
}

// ---- the hand-back (docs/summary-order.md 4.3) ----

/// What the asker's offload step does with a request a computer handed back.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HandBack {
    /// Nothing more: the record names another request or none, cannot be read (fails closed), or
    /// a named run was ended here.
    Settle,
    /// No record at all: a request asked before this version, written here as before.
    Legacy,
    /// The record waits on it in an order run: a re-summarise job continues the run.
    ContinueOrder,
}

/// Decides about `request` of `meeting`, handed back with `word`; a named run waiting on it is
/// ended here, in one transaction re-reading the record.
///
/// # Errors
/// Store errors.
pub(crate) fn hand_back(
    services: &Services,
    meeting: EntityId,
    request: EntityId,
    word: OutcomeWord,
) -> Result<HandBack, RuntimeError> {
    services
        .store()
        .transaction(|tx| -> Result<HandBack, RuntimeError> {
            match read_tx(tx, meeting)? {
                RunRecord::Absent => Ok(HandBack::Legacy),
                RunRecord::Unreadable => Ok(HandBack::Settle),
                RunRecord::Read(run)
                    if run.waiting_on == Some(request) && run.state == RunState::Waiting =>
                {
                    match run.origin {
                        RunOrigin::Order => Ok(HandBack::ContinueOrder),
                        RunOrigin::Named => {
                            let mut run = run;
                            run.push(run.cursor.saturating_sub(1), word);
                            run.waiting_on = None;
                            run.state = RunState::Ended;
                            put_tx(tx, meeting, &run)?;
                            Ok(HandBack::Settle)
                        }
                    }
                }
                RunRecord::Read(_) => Ok(HandBack::Settle),
            }
        })
}

/// Whether `meeting`'s record waits on `request` (the asker's offload step then reads its `Done`).
///
/// # Errors
/// Store errors.
pub(crate) fn waits_on(
    store: &Store,
    meeting: EntityId,
    request: EntityId,
) -> Result<bool, RuntimeError> {
    Ok(matches!(
        read(store, meeting)?,
        RunRecord::Read(run) if run.waiting_on == Some(request) && run.state == RunState::Waiting
    ))
}

/// The computer stored the summary (its request is `Done`, arrived through sync): a record
/// waiting on `request` is ended with that computer as the writer, in one transaction re-reading
/// it. Returns whether it ended one.
///
/// # Errors
/// Store errors.
pub fn end_after_done(
    services: &Services,
    meeting: EntityId,
    request: EntityId,
) -> Result<bool, RuntimeError> {
    services
        .store()
        .transaction(|tx| -> Result<bool, RuntimeError> {
            let RunRecord::Read(mut run) = read_tx(tx, meeting)? else {
                return Ok(false);
            };
            if run.waiting_on != Some(request) || run.state != RunState::Waiting {
                return Ok(false);
            }
            run.writer = Some(run.cursor.saturating_sub(1));
            run.waiting_on = None;
            run.state = RunState::Ended;
            put_tx(tx, meeting, &run)?;
            Ok(true)
        })
}

// ---- readers ----

/// A meeting's last run as the host shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunView {
    /// No run recorded.
    None,
    /// Over: its account (banner, CLI) until the next run.
    Ended(SummaryRun),
    /// A computer step waits.
    Waiting(SummaryRun),
    /// Running or continuing: interrupted, unless the host knows the meeting's work still runs.
    Unfinished(SummaryRun),
    /// The record cannot be read: "The summary was interrupted".
    Unreadable,
}

/// `meeting`'s last run. A waiting run whose meeting holds a summary created after the request
/// reads as ended with that computer as the writer, so a restart before the asker's step never
/// shows "interrupted" over a good summary.
///
/// # Errors
/// Store errors.
pub fn run_view(store: &Store, meeting: EntityId) -> Result<RunView, RuntimeError> {
    Ok(match read(store, meeting)? {
        RunRecord::Absent => RunView::None,
        RunRecord::Unreadable => RunView::Unreadable,
        RunRecord::Read(run) => match run.state {
            RunState::Ended => RunView::Ended(run),
            RunState::Running | RunState::Continuing => RunView::Unfinished(run),
            RunState::Waiting => {
                let asked_at = run
                    .waiting_on
                    .map(|id| store.summary_request(id))
                    .transpose()?
                    .flatten()
                    .map(|r| r.meta.created_at);
                let newer = match asked_at {
                    Some(at) => store
                        .summaries(meeting)?
                        .iter()
                        .any(|s| s.meta.created_at > at),
                    None => false,
                };
                if newer {
                    let mut ended = run;
                    ended.writer = Some(ended.cursor.saturating_sub(1));
                    ended.waiting_on = None;
                    ended.state = RunState::Ended;
                    RunView::Ended(ended)
                } else {
                    RunView::Waiting(run)
                }
            }
        },
    })
}

/// The steps of `steps` (a meeting's filtered order) known to fail now, with their words: the
/// start banner's checks (docs/summary-order.md 4.5). They never block recording and every step
/// is still tried at summary time.
#[must_use]
pub fn start_checks(
    services: &Services,
    steps: &[OrderStep],
    writers: &dyn StepWriters,
) -> Vec<(usize, OutcomeWord)> {
    let mut out = Vec::new();
    for (i, step) in steps.iter().enumerate() {
        match step {
            OrderStep::Writer(w) => match writers.writer(services, *w) {
                Built::Service(_) => {}
                Built::Skip(reason) => {
                    out.push((i, skip_word(&reason).unwrap_or(OutcomeWord::Failed)))
                }
                Built::Stop(_) => out.push((i, OutcomeWord::Failed)),
            },
            OrderStep::Computer(d) => match writers.computer(services, *d) {
                OffloadChoice::Ask { .. } => {}
                OffloadChoice::HereInstead { why, .. } => out.push((i, unavailable_word(why))),
                OffloadChoice::Here => out.push((i, OutcomeWord::Offline)),
            },
        }
    }
    out
}

/// The writer this computer uses for another computer's request (docs/summary-order.md 4.6):
/// exactly the kind its presence advertised, the first writer step of its order
/// ([`WriterKind::of_order`]); a meeting kept on its computer turns a remote kind into the local
/// model. It runs no computer step and no next step: a failure declines the request.
#[must_use]
pub fn helper_writer(
    services: &Services,
    writers: &dyn StepWriters,
    privacy: SummaryPrivacy,
) -> HelperWriter {
    let first = crate::summary_order::summary_order(&services.store())
        .ok()
        .and_then(|(order, _)| order.first_writer());
    let Some(first) = first else {
        return HelperWriter::Decline(reason::NO_WRITER);
    };
    let kept = privacy == SummaryPrivacy::KeptLocal && first.is_remote();
    let step = if kept { WriterStep::Local } else { first };
    match writers.writer(services, step) {
        Built::Service(prepared) => {
            let hold = crate::meeting_queue::SummaryHold::of(Some(&prepared));
            HelperWriter::Write(Box::new(prepared.service), hold)
        }
        Built::Skip(_) if kept => HelperWriter::Decline(reason::KEPT_LOCAL_NO_MODEL),
        Built::Skip(_) | Built::Stop(_) => HelperWriter::Decline(reason::NO_WRITER),
    }
}

/// Whether a local step of the meeting holding `ticket` would now run beside a newer meeting the
/// queue did not hold for it (docs/summary-order.md 4.5): its hold was recorded without a local
/// model and a newer meeting's ticket is open. What a meeting's [`RunPlan::local_not_now`] asks.
#[must_use]
pub fn local_not_now(ticket: &crate::meeting_queue::QueueTicket) -> bool {
    ticket.summary() == crate::meeting_queue::SummaryHold::OffThisComputer && ticket.newer_open()
}

/// The writer kind of `step` with `settings` (presence carries it).
#[must_use]
pub fn kind_of(
    step: WriterStep,
    settings: &crate::summary_settings::SummarySettings,
) -> WriterKind {
    use crate::summary_settings::EndpointProvider;
    match step {
        WriterStep::Local => WriterKind::Local,
        WriterStep::Endpoint => match settings.provider {
            EndpointProvider::Anthropic => WriterKind::Anthropic,
            EndpointProvider::OpenAiCompatible => WriterKind::OpenaiCompatible,
        },
        WriterStep::ClaudeCode => WriterKind::ClaudeCode,
        WriterStep::Codex => WriterKind::Codex,
    }
}

#[cfg(test)]
#[path = "run_tests.rs"]
mod tests;
