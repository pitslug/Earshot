//! The Summaries page of Settings (T7.4g): who writes the summary, the user's own endpoint,
//! "summarise during the meeting", and the Anthropic API key, which the page can store, ask
//! about and remove but never read ([`crate::credentials::KeyVault`]), and whether Claude Code
//! is installed and signed in for the "Claude (through Claude Code)" choice (T7.4ga,
//! [`claude_code_status`]), and likewise Codex for "OpenAI (through Codex)" (T7.4gc,
//! [`codex_status`]). The settings and
//! their rules are `earshot_runtime::summary_settings`; this module only maps them to the
//! IPC types.

use earshot_runtime::Services;
use earshot_runtime::claude_code::{ClaudeCodeProbe, ClaudeCodeStatus, SignIn};
use earshot_runtime::codex::{CodexProbe, CodexSignIn, CodexStatus};
use earshot_runtime::host::LIVE_SUMMARIES_NEED_LOCAL;
use earshot_runtime::summary_settings::{
    ANTHROPIC_DEFAULT_URL, ApiKeyError, Effort, EndpointProvider, SummarySettings,
    SummarySettingsError, SummaryWriter, set_summary_settings, summary_settings,
};
use serde::{Deserialize, Serialize};
use specta::Type;

use crate::credentials::KeyVault;
use crate::ipc::IpcError;

/// Who writes the summary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Type)]
pub enum WriterChoice {
    /// The local model of the Models page.
    OnThisComputer,
    /// The user's own endpoint.
    OwnEndpoint,
    /// Claude through the user's Claude Code (T7.4ga).
    ClaudeCode,
    /// OpenAI through the user's Codex (T7.4gc).
    Codex,
    /// No summary.
    NoSummary,
}

impl WriterChoice {
    fn of(w: SummaryWriter) -> Self {
        match w {
            SummaryWriter::OnThisComputer => Self::OnThisComputer,
            SummaryWriter::OwnEndpoint => Self::OwnEndpoint,
            SummaryWriter::ClaudeCode => Self::ClaudeCode,
            SummaryWriter::Codex => Self::Codex,
            SummaryWriter::NoSummary => Self::NoSummary,
        }
    }

    fn writer(self) -> SummaryWriter {
        match self {
            Self::OnThisComputer => SummaryWriter::OnThisComputer,
            Self::OwnEndpoint => SummaryWriter::OwnEndpoint,
            Self::ClaudeCode => SummaryWriter::ClaudeCode,
            Self::Codex => SummaryWriter::Codex,
            Self::NoSummary => SummaryWriter::NoSummary,
        }
    }
}

/// Which API the endpoint speaks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Type)]
pub enum ProviderChoice {
    /// An OpenAI-compatible server.
    OpenAiCompatible,
    /// Anthropic (Claude) with the user's API key.
    Anthropic,
}

impl ProviderChoice {
    fn of(p: EndpointProvider) -> Self {
        match p {
            EndpointProvider::OpenAiCompatible => Self::OpenAiCompatible,
            EndpointProvider::Anthropic => Self::Anthropic,
        }
    }

    fn provider(self) -> EndpointProvider {
        match self {
            Self::OpenAiCompatible => EndpointProvider::OpenAiCompatible,
            Self::Anthropic => EndpointProvider::Anthropic,
        }
    }
}

/// How hard Claude Code works on a summary (T12.18): `--effort low|medium|high|xhigh|max`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Type)]
pub enum EffortChoice {
    /// `low`.
    Low,
    /// `medium`.
    Medium,
    /// `high`.
    High,
    /// `xhigh`.
    ExtraHigh,
    /// `max`.
    Max,
}

impl EffortChoice {
    fn of(e: Effort) -> Self {
        match e {
            Effort::Low => Self::Low,
            Effort::Medium => Self::Medium,
            Effort::High => Self::High,
            Effort::ExtraHigh => Self::ExtraHigh,
            Effort::Max => Self::Max,
        }
    }

    fn effort(self) -> Effort {
        match self {
            Self::Low => Effort::Low,
            Self::Medium => Effort::Medium,
            Self::High => Effort::High,
            Self::ExtraHigh => Effort::ExtraHigh,
            Self::Max => Effort::Max,
        }
    }
}

/// The Summaries page settings as the UI sends and reads them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct SummarySettingsValue {
    /// Who writes the summary.
    pub writer: WriterChoice,
    /// The endpoint's provider.
    pub provider: ProviderChoice,
    /// The endpoint address ("" when none).
    pub url: String,
    /// The model name sent to the endpoint ("" when none).
    pub model: String,
    /// Summarise during the meeting.
    pub live: bool,
    /// The model Claude Code is asked for ("" for Claude Code's own default).
    pub claude_model: String,
    /// The effort Claude Code is asked for (`null` for Claude Code's own default; T12.18).
    pub claude_effort: Option<EffortChoice>,
    /// The model Codex is asked for ("" for Codex's own default).
    pub codex_model: String,
}

impl SummarySettingsValue {
    fn of(s: &SummarySettings) -> Self {
        Self {
            writer: WriterChoice::of(s.writer),
            provider: ProviderChoice::of(s.provider),
            url: s.url.clone(),
            model: s.model.clone(),
            live: s.live,
            claude_model: s.claude_model.clone(),
            claude_effort: s.claude_effort.map(EffortChoice::of),
            codex_model: s.codex_model.clone(),
        }
    }

    fn settings(&self) -> SummarySettings {
        SummarySettings {
            writer: self.writer.writer(),
            provider: self.provider.provider(),
            url: self.url.clone(),
            model: self.model.clone(),
            live: self.live,
            claude_model: self.claude_model.clone(),
            claude_effort: self.claude_effort.map(EffortChoice::effort),
            codex_model: self.codex_model.clone(),
        }
    }
}

/// The stored settings with what the page says about them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Type)]
pub struct SummarySettingsView {
    /// The settings in use (defaults for anything absent or unknown).
    pub value: SummarySettingsValue,
    /// One line per stored value this version does not know.
    pub unknown: Vec<String>,
    /// The stored endpoint's host, for the warning that names where the meeting is sent.
    pub endpoint_host: Option<String>,
    /// Whether the stored endpoint is this computer or the local network (no warning).
    pub endpoint_local: bool,
    /// Why "summarise during the meeting" is not used with the stored choice (`null` when
    /// it is: on this computer).
    pub live_unavailable: Option<String>,
    /// The Anthropic API address, offered when the endpoint is empty.
    pub anthropic_url: String,
}

/// `text` with its first letter upper-case and a full stop (runtime reasons are clauses).
fn sentence(text: &str) -> String {
    let mut c = text.chars();
    let mut out: String = c
        .next()
        .map(|f| f.to_uppercase().collect())
        .unwrap_or_default();
    out.push_str(c.as_str());
    if !out.ends_with('.') {
        out.push('.');
    }
    out
}

fn view(s: &SummarySettings, unknown: Vec<String>) -> SummarySettingsView {
    SummarySettingsView {
        value: SummarySettingsValue::of(s),
        unknown,
        endpoint_host: s.endpoint_host(),
        endpoint_local: s.endpoint_is_local(),
        live_unavailable: (s.writer != SummaryWriter::OnThisComputer)
            .then(|| sentence(LIVE_SUMMARIES_NEED_LOCAL)),
        anthropic_url: ANTHROPIC_DEFAULT_URL.to_owned(),
    }
}

/// The settings of `services`' store.
///
/// # Errors
/// Store errors.
pub fn settings_view(services: &Services) -> Result<SummarySettingsView, IpcError> {
    let (s, unknown) = summary_settings(&services.store())?;
    Ok(view(&s, unknown.iter().map(ToString::to_string).collect()))
}

/// The UI error of a refused setting.
#[must_use]
pub fn settings_error(e: SummarySettingsError) -> IpcError {
    match e {
        SummarySettingsError::Store(e) => IpcError::from(e),
        other => IpcError {
            kind: "invalid_setting".into(),
            message: sentence(&other.to_string()),
            retryable: false,
        },
    }
}

/// Checks and stores `value`; returns the settings as stored.
///
/// # Errors
/// `invalid_setting` (nothing stored); store errors.
pub fn set_settings(
    services: &Services,
    value: &SummarySettingsValue,
) -> Result<SummarySettingsView, IpcError> {
    let stored = {
        let mut db = services.store();
        set_summary_settings(&mut db, &value.settings()).map_err(settings_error)?
    };
    Ok(view(&stored, Vec::new()))
}

/// The UI error of a key that could not be stored, read or removed (never the key).
#[must_use]
pub fn key_error(e: &ApiKeyError) -> IpcError {
    IpcError {
        kind: match e {
            ApiKeyError::Invalid(_) => "invalid_setting",
            ApiKeyError::Missing | ApiKeyError::Unsupported | ApiKeyError::Store(_) => "api_key",
        }
        .into(),
        message: sentence(&e.to_string()),
        retryable: false,
    }
}

/// Stores the Anthropic API key in `vault` (replacing an earlier one).
///
/// # Errors
/// `invalid_setting` for something that is not a key (nothing stored); `api_key` when the
/// store fails.
pub fn set_api_key(vault: &dyn KeyVault, key: &str) -> Result<(), IpcError> {
    vault.save(key).map_err(|e| key_error(&e))
}

/// Whether `vault` holds an API key.
///
/// # Errors
/// `api_key` when the store fails.
pub fn has_api_key(vault: &dyn KeyVault) -> Result<bool, IpcError> {
    vault.has().map_err(|e| key_error(&e))
}

/// Removes the API key from `vault` (no key is not an error).
///
/// # Errors
/// `api_key` when the store fails.
pub fn remove_api_key(vault: &dyn KeyVault) -> Result<(), IpcError> {
    vault.remove().map_err(|e| key_error(&e))
}

/// Claude Code's state as the Summaries page shows it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Type)]
pub enum ClaudeCodeState {
    /// No Claude Code CLI was found.
    NotInstalled,
    /// Installed and signed in.
    SignedIn,
    /// Installed, not signed in.
    NotSignedIn,
    /// Installed; whether it is signed in could not be read.
    Unknown,
}

/// Whether Claude Code can write the summaries, with how to fix it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Type)]
pub struct ClaudeCodeStatusView {
    /// The state.
    pub state: ClaudeCodeState,
    /// `claude --version` when installed and it answered.
    pub version: Option<String>,
    /// One sentence on the state.
    pub message: String,
    /// What to do, when something is needed.
    pub fix: Option<String>,
    /// Why the effort choice does nothing with this Claude Code (its `--help` lists no
    /// `--effort`), `null` when it works or could not be told (T12.18).
    pub effort_note: Option<String>,
}

/// What the page says when the installed Claude Code has no `--effort` (T12.18).
pub const EFFORT_NEEDS_NEWER: &str = "Choosing the effort needs a newer Claude Code. This one runs at its own default effort; update Claude Code to choose it.";

/// The status `probe` reports, as the page shows it. Runs child processes (`claude
/// --version`, `claude auth status`, `claude --help`; never a sign-in): call it off the UI
/// thread and without the store lock.
#[must_use]
pub fn claude_code_status(probe: &dyn ClaudeCodeProbe) -> ClaudeCodeStatusView {
    let (state, version, accepts_effort) = match probe.status() {
        ClaudeCodeStatus::NotInstalled => (ClaudeCodeState::NotInstalled, None, None),
        ClaudeCodeStatus::Installed {
            version,
            sign_in,
            accepts_effort,
            ..
        } => (
            match sign_in {
                SignIn::SignedIn { .. } => ClaudeCodeState::SignedIn,
                SignIn::NotSignedIn => ClaudeCodeState::NotSignedIn,
                SignIn::Unknown(_) => ClaudeCodeState::Unknown,
            },
            version,
            accepts_effort,
        ),
    };
    let name = match version.as_deref().and_then(|v| v.split_whitespace().next()) {
        Some(v) => format!("Claude Code {v}"),
        None => "Claude Code".to_owned(),
    };
    let (message, fix) = match state {
        ClaudeCodeState::NotInstalled => (
            "Claude Code is not installed on this computer.".to_owned(),
            Some("Install Claude Code and sign in to it, then check again."),
        ),
        ClaudeCodeState::SignedIn => (format!("{name} is installed and signed in."), None),
        ClaudeCodeState::NotSignedIn => (
            format!("{name} is installed but not signed in."),
            Some(
                "Open Claude Code and sign in (in a terminal, run claude, then /login), then check again.",
            ),
        ),
        ClaudeCodeState::Unknown => (
            format!("{name} is installed, but Earshot could not tell whether it is signed in."),
            Some("Open Claude Code to check that it is signed in."),
        ),
    };
    ClaudeCodeStatusView {
        state,
        version,
        message,
        fix: fix.map(str::to_owned),
        effort_note: (accepts_effort == Some(false)).then(|| EFFORT_NEEDS_NEWER.to_owned()),
    }
}

#[cfg(test)]
#[path = "summaries_tests.rs"]
mod tests;

/// Codex's state as the Summaries page shows it (T7.4gc).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Type)]
pub enum CodexState {
    /// No Codex CLI was found.
    NotInstalled,
    /// Installed and signed in.
    SignedIn,
    /// Installed, not signed in.
    NotSignedIn,
    /// Installed; whether it is signed in could not be read.
    Unknown,
}

/// Whether Codex can write the summaries, with how to fix it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Type)]
pub struct CodexStatusView {
    /// The state.
    pub state: CodexState,
    /// `codex --version` when installed and it answered.
    pub version: Option<String>,
    /// One sentence on the state.
    pub message: String,
    /// What to do, when something is needed.
    pub fix: Option<String>,
}

/// The status `probe` reports, as the page shows it. Runs child processes (`codex
/// --version`, `codex login status`; never a sign-in): call it off the UI thread and without
/// the store lock.
#[must_use]
pub fn codex_status(probe: &dyn CodexProbe) -> CodexStatusView {
    let (state, version, method) = match probe.status() {
        CodexStatus::NotInstalled => (CodexState::NotInstalled, None, None),
        CodexStatus::Installed {
            version, sign_in, ..
        } => match sign_in {
            CodexSignIn::SignedIn { method } => (CodexState::SignedIn, version, method),
            CodexSignIn::NotSignedIn => (CodexState::NotSignedIn, version, None),
            CodexSignIn::Unknown(_) => (CodexState::Unknown, version, None),
        },
    };
    // `codex --version` prints "codex-cli 0.155.1": the number is the last word.
    let name = match version.as_deref().and_then(|v| v.split_whitespace().last()) {
        Some(v) => format!("Codex {v}"),
        None => "Codex".to_owned(),
    };
    let (message, fix) = match state {
        CodexState::NotInstalled => (
            "Codex is not installed on this computer.".to_owned(),
            Some(
                "Install the Codex CLI (in a terminal, run npm install -g @openai/codex) and sign in to it, then check again.",
            ),
        ),
        CodexState::SignedIn => (
            match method {
                Some("chatgpt") => format!("{name} is installed and signed in with ChatGPT."),
                Some("api_key") => format!("{name} is installed and signed in with an API key."),
                _ => format!("{name} is installed and signed in."),
            },
            None,
        ),
        CodexState::NotSignedIn => (
            format!("{name} is installed but not signed in."),
            Some("Sign in to Codex (in a terminal, run codex login), then check again."),
        ),
        CodexState::Unknown => (
            format!("{name} is installed, but Earshot could not tell whether it is signed in."),
            Some("In a terminal, run codex login status to check that it is signed in."),
        ),
    };
    CodexStatusView {
        state,
        version,
        message,
        fix: fix.map(str::to_owned),
    }
}
