//! Claude through the user's installed Claude Code CLI as the summary writer (T7.4ga).
//!
//! The provider itself is `earshot_llm::claude_code` (finding the CLI, the restricted run,
//! the handover record); this module holds what the app decides from: whether Claude Code
//! is installed and signed in ([`ClaudeCodeProbe`], [`ClaudeCodeStatus`]), checked at the
//! start of a meeting and shown on the Summaries page, and the summary configuration
//! ([`ClaudeCodeSummaryConfig`]).
//!
//! # Privacy (PLAN.md 3.9 exception)
//!
//! Off unless chosen. Earshot hands the summary prompt text (title, attendee and speaker
//! names, transcript, notes) to Claude Code on standard input; Claude Code sends it to
//! Anthropic on the user's Claude.ai sign-in, a request Earshot cannot see. Every run is
//! recorded as an `earshot_llm::Handover` (when, how many characters, how it ended; never
//! the text), reported in `host::HostOutcome::handovers` beside the HTTP audit log, and
//! stored device-locally per meeting ([`HandoverEntry`] in `summary-handovers/<meeting>`,
//! version outside the payload: Unix ms, characters, which CLI and a fixed outcome word only;
//! OpenAI Codex (T7.4gc, [`crate::codex`]) stores its handovers in the same record). Each run
//! first stores a `started` entry in a short write of its own, before any text is written to
//! the CLI (a run whose `started` cannot be stored is not made), then gives that entry its
//! outcome in another short write when the run ends; a crash leaves `started`. The record is
//! never synced and is erased by meeting deletion (the `local` registry). A failure to store an
//! outcome never replaces the summary's own error: it is reported beside it
//! (`SummaryService::handover_record_failures`, `host::HostOutcome::handover_record_failures`).

use std::time::Duration;

use earshot_llm::CodexError;
use earshot_llm::claude_code::STATUS_TIMEOUT;
pub use earshot_llm::claude_code::check_model;
pub use earshot_llm::{ClaudeCli, ClaudeCodeError, Handover, HandoverOutcome, LlmError, SignIn};

use earshot_core::identity::EntityId;
use earshot_core::model::MeetingRepo;
use std::sync::{Arc, Mutex, PoisonError};

use earshot_llm::HandoverSink;
use earshot_store::Store;

use crate::local::{LocalKey, MeetingRecord};
use crate::{RuntimeError, Services};

/// The runtime error of a Claude Code failure (`RuntimeError::Llm`).
#[must_use]
pub fn runtime_error(e: ClaudeCodeError) -> RuntimeError {
    RuntimeError::Llm(LlmError::ClaudeCode(e))
}

/// The model name stored with a summary Claude Code wrote with `model` (`None`: its
/// default): `claude-code` or `claude-code/<model>`.
#[must_use]
pub fn stored_model_name(model: Option<&str>) -> String {
    match model {
        Some(m) => format!("claude-code/{m}"),
        None => "claude-code".to_owned(),
    }
}

/// A summary through Claude Code.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ClaudeCodeSummaryConfig {
    /// `--model`; `None`: Claude Code's own default.
    pub model: Option<String>,
    /// The CLI; `None`: found when the service is built (`ClaudeCli::find`).
    pub cli: Option<ClaudeCli>,
    /// Deadline of each run (default: `earshot_llm::DEFAULT_REQUEST_TIMEOUT`).
    pub request_timeout: Option<Duration>,
}

/// Whether Claude Code can write a summary here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClaudeCodeStatus {
    /// No CLI was found.
    NotInstalled,
    /// The CLI was found.
    Installed {
        /// The CLI.
        cli: ClaudeCli,
        /// `claude --version`, when it answered.
        version: Option<String>,
        /// The sign-in state (`claude auth status`; never a sign-in).
        sign_in: SignIn,
    },
}

impl ClaudeCodeStatus {
    /// Why a summary cannot run now: not installed, or not signed in. An unknown sign-in
    /// state is no reason (the run reports its own error).
    #[must_use]
    pub fn problem(&self) -> Option<ClaudeCodeError> {
        match self {
            Self::NotInstalled => Some(ClaudeCodeError::NotInstalled),
            Self::Installed {
                sign_in: SignIn::NotSignedIn,
                ..
            } => Some(ClaudeCodeError::NotSignedIn),
            Self::Installed { .. } => None,
        }
    }

    /// The CLI, when installed.
    #[must_use]
    pub fn cli(&self) -> Option<&ClaudeCli> {
        match self {
            Self::NotInstalled => None,
            Self::Installed { cli, .. } => Some(cli),
        }
    }
}

/// Asks whether Claude Code is installed and signed in (the CLI in production, a fake in
/// tests). Runs child processes: never call it with the store lock held.
pub trait ClaudeCodeProbe {
    /// The status now.
    fn status(&self) -> ClaudeCodeStatus;
}

/// Deadline of the sign-in check at the start of a meeting: a CLI that does not answer in
/// time reads as unknown and the recording starts anyway (the run reports its own error).
pub const MEETING_START_TIMEOUT: Duration = Duration::from_secs(5);

/// The installed CLI: `ClaudeCli::find`, then `claude auth status --json`, and for the
/// Summaries page also `claude --version`, each run with `timeout`.
#[derive(Debug, Clone, Copy)]
pub struct InstalledClaudeCode {
    /// Also ask `claude --version`.
    pub version: bool,
    /// Deadline of each check.
    pub timeout: Duration,
}

impl InstalledClaudeCode {
    /// The check at the start of a meeting: the sign-in only, within
    /// [`MEETING_START_TIMEOUT`].
    #[must_use]
    pub fn meeting_start() -> Self {
        Self {
            version: false,
            timeout: MEETING_START_TIMEOUT,
        }
    }

    /// The Summaries page's status: version and sign-in, within [`STATUS_TIMEOUT`].
    #[must_use]
    pub fn status_page() -> Self {
        Self {
            version: true,
            timeout: STATUS_TIMEOUT,
        }
    }
}

impl ClaudeCodeProbe for InstalledClaudeCode {
    fn status(&self) -> ClaudeCodeStatus {
        let Some(cli) = ClaudeCli::find() else {
            return ClaudeCodeStatus::NotInstalled;
        };
        let version = self
            .version
            .then(|| cli.version(self.timeout).ok())
            .flatten();
        let sign_in = cli.sign_in(self.timeout);
        ClaudeCodeStatus::Installed {
            cli,
            version,
            sign_in,
        }
    }
}
/// A fixed answer (tests, and hosts that never offer Claude Code).
#[derive(Debug, Clone)]
pub struct FixedClaudeCode(pub ClaudeCodeStatus);

impl ClaudeCodeProbe for FixedClaudeCode {
    fn status(&self) -> ClaudeCodeStatus {
        self.0.clone()
    }
}

// ---------------------------------------------------------------------------------------
// The local record of what was handed over (PLAN.md 3.9 exception).

/// Format version of the `summary-handovers/<meeting>` record (outside the payload), written
/// by this build. Version 2 (T7.4gc) adds each entry's `cli` (Claude Code or Codex) and the
/// `access_denied`, `tool_ran` and `unknown_event` outcomes (the last two came after the first
/// version 2 code on the T7.4gc lane, never released: a reader without them reports such a
/// record unreadable, never misreads it): a version 1 reader would have shown a Codex handover as Claude
/// Code's (it ignores unknown fields), so the version was bumped and such a reader reports the
/// record unreadable. The `started` word is part of version 1: no build wrote the record
/// without it (T7.4ga was never released before it). A later new outcome word that older
/// readers must not misread needs another bump (an unknown word fails their parse, which reads
/// as unreadable, never as an error).
pub const HANDOVER_RECORD_VERSION: u32 = 2;

/// Every record version this build reads: version 1 entries were all Claude Code's.
pub const READABLE_HANDOVER_VERSIONS: [u32; 2] = [1, 2];

/// Which agent CLI a handover went to, as stored.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HandoverCli {
    /// Claude Code (T7.4ga; every version 1 entry).
    #[default]
    ClaudeCode,
    /// OpenAI Codex (T7.4gc).
    Codex,
}

impl HandoverCli {
    /// The CLI's product name (`Claude Code`, `Codex`).
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::ClaudeCode => "Claude Code",
            Self::Codex => "Codex",
        }
    }
}

/// A provider error that a stored handover can name: which CLI, and the fixed outcome word.
pub trait HandoverError {
    /// The CLI whose error this is.
    const CLI: HandoverCli;
    /// The stored outcome of a run that failed with this error.
    fn kind(&self) -> HandoverKind;
}

impl HandoverError for ClaudeCodeError {
    const CLI: HandoverCli = HandoverCli::ClaudeCode;
    fn kind(&self) -> HandoverKind {
        match self {
            ClaudeCodeError::NotSignedIn => HandoverKind::NotSignedIn,
            ClaudeCodeError::SignInExpired => HandoverKind::SignInExpired,
            ClaudeCodeError::SignInBusy => HandoverKind::SignInBusy,
            ClaudeCodeError::PromptTooLong => HandoverKind::PromptTooLong,
            ClaudeCodeError::UnsupportedVersion => HandoverKind::UnsupportedVersion,
            ClaudeCodeError::TimedOut(_) => HandoverKind::TimedOut,
            ClaudeCodeError::OutputIncomplete => HandoverKind::OutputIncomplete,
            ClaudeCodeError::BadOutput(_) => HandoverKind::BadOutput,
            ClaudeCodeError::NotInstalled
            | ClaudeCodeError::Start(_)
            | ClaudeCodeError::NotRecorded
            | ClaudeCodeError::Failed { .. } => HandoverKind::Failed,
        }
    }
}

impl HandoverError for CodexError {
    const CLI: HandoverCli = HandoverCli::Codex;
    fn kind(&self) -> HandoverKind {
        match self {
            CodexError::NotSignedIn => HandoverKind::NotSignedIn,
            CodexError::SignInExpired => HandoverKind::SignInExpired,
            CodexError::AccessDenied => HandoverKind::AccessDenied,
            CodexError::PromptTooLong => HandoverKind::PromptTooLong,
            CodexError::UnsupportedVersion => HandoverKind::UnsupportedVersion,
            CodexError::ToolRan => HandoverKind::ToolRan,
            CodexError::UnknownEvent => HandoverKind::UnknownEvent,
            CodexError::TimedOut(_) => HandoverKind::TimedOut,
            CodexError::OutputIncomplete => HandoverKind::OutputIncomplete,
            CodexError::BadOutput(_) => HandoverKind::BadOutput,
            CodexError::NotInstalled
            | CodexError::Start(_)
            | CodexError::NotRecorded
            | CodexError::Failed { .. } => HandoverKind::Failed,
        }
    }
}

/// How a handover ended, as stored: a fixed word, never CLI output.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HandoverKind {
    /// The text is being handed over; no outcome was stored (the run is going on, or Earshot
    /// stopped before it ended).
    Started,
    /// The CLI answered.
    Answered,
    /// The run was cancelled and the CLI stopped.
    Cancelled,
    /// The CLI was not signed in.
    NotSignedIn,
    /// The CLI could not use its sign-in.
    SignInExpired,
    /// Another Claude Code process was renewing the sign-in.
    SignInBusy,
    /// The user's ChatGPT workspace does not allow Codex (version 2).
    AccessDenied,
    /// Codex used a tool, so its reply was not used (version 2).
    ToolRan,
    /// Codex sent an event Earshot does not know, so its reply was not used (version 2).
    UnknownEvent,
    /// The meeting was longer than the CLI accepts.
    PromptTooLong,
    /// This CLI version refused an option.
    UnsupportedVersion,
    /// The run did not finish within its deadline.
    TimedOut,
    /// The output was cut off.
    OutputIncomplete,
    /// The output was not the CLI's result.
    BadOutput,
    /// Any other failure.
    Failed,
}

impl HandoverKind {
    /// The kind of `outcome`.
    #[must_use]
    pub fn of<E: HandoverError>(outcome: &HandoverOutcome<E>) -> Self {
        match outcome {
            HandoverOutcome::Answered { .. } => Self::Answered,
            HandoverOutcome::Cancelled => Self::Cancelled,
            HandoverOutcome::Failed(e) => e.kind(),
        }
    }
}

/// One stored handover: when, how many characters, to which CLI, how it ended. Never text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct HandoverEntry {
    /// When the CLI was started (Unix milliseconds, the services clock).
    pub at_unix_ms: u64,
    /// Characters handed over on standard input.
    pub characters: u64,
    /// Which CLI (absent in version 1: Claude Code).
    #[serde(default)]
    pub cli: HandoverCli,
    /// How the run ended.
    pub outcome: HandoverKind,
}

impl HandoverEntry {
    /// The stored form of `h`.
    #[must_use]
    pub fn of<E: HandoverError>(h: &Handover<E>) -> Self {
        Self {
            at_unix_ms: h.timestamp_ms,
            characters: u64::try_from(h.characters).unwrap_or(u64::MAX),
            cli: E::CLI,
            outcome: HandoverKind::of(&h.outcome),
        }
    }
}

#[derive(serde::Serialize, serde::Deserialize)]
struct HandoverPayload {
    entries: Vec<HandoverEntry>,
}

/// A meeting's stored handovers.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HandoverRecord {
    /// Oldest first; empty without a record.
    pub entries: Vec<HandoverEntry>,
    /// Why a stored record could not be read (its entries are then not shown).
    pub unreadable: Option<String>,
}

/// The local_state key of `meeting`'s handover record.
pub(crate) fn record_key(meeting: EntityId) -> LocalKey {
    MeetingRecord::SummaryHandovers.key(&[], meeting, &[])
}

/// The stored form of `entries`: version (u32 LE), then JSON.
#[must_use]
pub fn encode(entries: &[HandoverEntry]) -> Vec<u8> {
    let payload = serde_json::to_vec(&HandoverPayload {
        entries: entries.to_vec(),
    })
    .expect("numbers and fixed words serialise");
    let mut out = Vec::with_capacity(4 + payload.len());
    out.extend_from_slice(&HANDOVER_RECORD_VERSION.to_le_bytes());
    out.extend_from_slice(&payload);
    out
}

/// Reads [`encode`]; a record of another version or that does not parse is reported, never an
/// error.
#[must_use]
pub fn decode(bytes: &[u8]) -> HandoverRecord {
    let Some(head) = bytes.get(..4) else {
        return HandoverRecord {
            entries: Vec::new(),
            unreadable: Some("the handover record is truncated".into()),
        };
    };
    let version = u32::from_le_bytes([head[0], head[1], head[2], head[3]]);
    if !READABLE_HANDOVER_VERSIONS.contains(&version) {
        return HandoverRecord {
            entries: Vec::new(),
            unreadable: Some(format!(
                "handover record version {version}, this build reads versions 1 to \
                 {HANDOVER_RECORD_VERSION}"
            )),
        };
    }
    match serde_json::from_slice::<HandoverPayload>(&bytes[4..]) {
        // Version 1 knew neither Codex nor its outcome word.
        Ok(p)
            if version == 1
                && p.entries.iter().any(|e| {
                    e.cli != HandoverCli::ClaudeCode
                        || matches!(
                            e.outcome,
                            HandoverKind::AccessDenied
                                | HandoverKind::ToolRan
                                | HandoverKind::UnknownEvent
                        )
                }) =>
        {
            HandoverRecord {
                entries: Vec::new(),
                unreadable: Some("the handover record does not parse".into()),
            }
        }
        Ok(p) => HandoverRecord {
            entries: p.entries,
            unreadable: None,
        },
        Err(_) => HandoverRecord {
            entries: Vec::new(),
            unreadable: Some("the handover record does not parse".into()),
        },
    }
}

/// The handovers stored for `meeting` (empty without a record: meetings from before T7.4ga
/// or never summarised through Claude Code or Codex).
///
/// # Errors
/// Store errors.
pub fn summary_handovers(store: &Store, meeting: EntityId) -> Result<HandoverRecord, RuntimeError> {
    Ok(store
        .local_value(record_key(meeting).as_str())?
        .map(|v| decode(&v))
        .unwrap_or_default())
}

/// Changes `meeting`'s record with `change` in one short transaction. Nothing is written for a
/// meeting that no longer exists or is deleted (`Ok(false)`); a record that cannot be read is
/// replaced (its entries could not be shown anyway).
fn change_record(
    services: &Services,
    meeting: EntityId,
    change: impl FnOnce(&mut Vec<HandoverEntry>),
) -> Result<bool, RuntimeError> {
    let mut db = services.store();
    db.transaction(|tx| -> Result<bool, RuntimeError> {
        if !tx.meeting(meeting)?.is_some_and(|m| !m.meta.deleted) {
            return Ok(false);
        }
        let key = record_key(meeting);
        let mut entries = tx
            .local_value(key.as_str())?
            .map(|v| decode(&v).entries)
            .unwrap_or_default();
        change(&mut entries);
        crate::local::set_tx(tx, &key, &encode(&entries))?;
        Ok(true)
    })
}

/// Stores a `started` entry before any text is handed over (a crash then leaves it).
///
/// # Errors
/// Store errors; [`RuntimeError::Conflict`] for a meeting that no longer exists (nothing is
/// handed over for it).
pub(crate) fn record_started(
    services: &Services,
    meeting: EntityId,
    cli: HandoverCli,
    at_unix_ms: u64,
    characters: usize,
) -> Result<(), RuntimeError> {
    let entry = HandoverEntry {
        at_unix_ms,
        characters: u64::try_from(characters).unwrap_or(u64::MAX),
        cli,
        outcome: HandoverKind::Started,
    };
    if change_record(services, meeting, |e| e.push(entry))? {
        Ok(())
    } else {
        Err(RuntimeError::Conflict("the meeting was deleted"))
    }
}

/// Stores how `handover` ended: its `started` entry (same CLI, time and size, the last one)
/// takes the outcome, else the entry is appended.
///
/// # Errors
/// Store errors.
pub(crate) fn record_ended<E: HandoverError>(
    services: &Services,
    meeting: EntityId,
    handover: &Handover<E>,
) -> Result<(), RuntimeError> {
    let ended = HandoverEntry::of(handover);
    change_record(services, meeting, |entries| {
        let started = entries.iter_mut().rev().find(|e| {
            e.outcome == HandoverKind::Started
                && e.cli == ended.cli
                && e.at_unix_ms == ended.at_unix_ms
                && e.characters == ended.characters
        });
        match started {
            Some(e) => *e = ended,
            None => entries.push(ended),
        }
    })
    .map(|_| ())
}

/// Stores the handovers of one meeting's summary run as they happen ([`HandoverSink`]):
/// `started` before the text is written, the outcome after. A failure to store is kept in
/// `failures` (the store's error text, never meeting text); a `started` that cannot be stored
/// stops the handover.
pub(crate) struct MeetingHandovers {
    pub(crate) services: Services,
    pub(crate) meeting: EntityId,
    pub(crate) failures: Arc<Mutex<Vec<String>>>,
}

impl MeetingHandovers {
    fn failed(&self, e: &RuntimeError) {
        self.failures
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(e.to_string());
    }
}

impl MeetingHandovers {
    fn start(&self, cli: HandoverCli, timestamp_ms: u64, characters: usize) -> Result<(), String> {
        record_started(&self.services, self.meeting, cli, timestamp_ms, characters).map_err(|e| {
            self.failed(&e);
            e.to_string()
        })
    }

    fn end<E: HandoverError>(&self, handover: &Handover<E>) {
        if let Err(e) = record_ended(&self.services, self.meeting, handover) {
            self.failed(&e);
        }
    }
}

impl HandoverSink for MeetingHandovers {
    fn started(&self, timestamp_ms: u64, characters: usize) -> Result<(), String> {
        self.start(HandoverCli::ClaudeCode, timestamp_ms, characters)
    }

    fn ended(&self, handover: &Handover) {
        self.end(handover);
    }
}

impl HandoverSink<CodexError> for MeetingHandovers {
    fn started(&self, timestamp_ms: u64, characters: usize) -> Result<(), String> {
        self.start(HandoverCli::Codex, timestamp_ms, characters)
    }

    fn ended(&self, handover: &Handover<CodexError>) {
        self.end(handover);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn installed(sign_in: SignIn) -> ClaudeCodeStatus {
        ClaudeCodeStatus::Installed {
            cli: ClaudeCli {
                program: "claude.exe".into(),
                leading_args: Vec::new(),
            },
            version: Some("2.1.290 (Claude Code)".into()),
            sign_in,
        }
    }

    #[test]
    fn claude_code_status_problems_are_not_installed_and_not_signed_in_only() {
        assert_eq!(
            ClaudeCodeStatus::NotInstalled.problem(),
            Some(ClaudeCodeError::NotInstalled)
        );
        assert_eq!(
            installed(SignIn::NotSignedIn).problem(),
            Some(ClaudeCodeError::NotSignedIn)
        );
        assert_eq!(installed(SignIn::SignedIn { method: None }).problem(), None);
        assert_eq!(installed(SignIn::Unknown("x".into())).problem(), None);
        assert!(ClaudeCodeStatus::NotInstalled.cli().is_none());
        assert_eq!(stored_model_name(None), "claude-code");
        assert_eq!(stored_model_name(Some("opus")), "claude-code/opus");
        // The meeting start asks only the sign-in, briefly; the Summaries page also the version.
        let start = InstalledClaudeCode::meeting_start();
        assert!(!start.version);
        assert_eq!(start.timeout, Duration::from_secs(5));
        let page = InstalledClaudeCode::status_page();
        assert!(page.version);
        assert_eq!(page.timeout, STATUS_TIMEOUT);
    }
}
