//! OpenAI through the user's installed Codex CLI as the summary writer (T7.4gc), the same
//! route as Claude Code ([`crate::claude_code`]).
//!
//! The provider itself is `earshot_llm::codex` (finding the CLI, the restricted run); this
//! module holds what the app decides from: whether Codex is installed and signed in
//! ([`CodexProbe`], [`CodexStatus`]), checked at the start of a meeting and shown on the
//! Summaries page, and the summary configuration ([`CodexSummaryConfig`]).
//!
//! # Privacy (PLAN.md 3.9 exception)
//!
//! Off unless chosen. Earshot hands the summary prompt text (title, attendee and speaker
//! names, transcript, notes) to Codex on standard input; Codex sends it to OpenAI on the
//! user's ChatGPT sign-in, a request Earshot cannot see. Every run is recorded exactly like a
//! Claude Code run: an `earshot_llm::handover::Handover` (when, how many characters, how it
//! ended; never the text), reported in `host::HostOutcome::handovers` and stored in the
//! meeting's `summary-handovers/<meeting>` record with `cli: codex`
//! ([`crate::claude_code::HandoverEntry`]), `started` before any text is written, then the
//! outcome. A missing CLI or sign-in skips the summary (never another provider).

use std::time::Duration;

use earshot_llm::codex::STATUS_TIMEOUT;
pub use earshot_llm::codex::check_model;
pub use earshot_llm::{CodexCli, CodexError, CodexSignIn, LlmError};

use crate::RuntimeError;

/// The runtime error of a Codex failure (`RuntimeError::Llm`).
#[must_use]
pub fn runtime_error(e: CodexError) -> RuntimeError {
    RuntimeError::Llm(LlmError::Codex(e))
}

/// The model name stored with a summary Codex wrote with `model` (`None`: its default):
/// `codex` or `codex/<model>`.
#[must_use]
pub fn stored_model_name(model: Option<&str>) -> String {
    match model {
        Some(m) => format!("codex/{m}"),
        None => "codex".to_owned(),
    }
}

/// A summary through Codex.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CodexSummaryConfig {
    /// `-m`; `None`: Codex's own default.
    pub model: Option<String>,
    /// The CLI; `None`: found when the service is built (`CodexCli::find`).
    pub cli: Option<CodexCli>,
    /// Deadline of each run (default: `earshot_llm::DEFAULT_REQUEST_TIMEOUT`).
    pub request_timeout: Option<Duration>,
}

/// Whether Codex can write a summary here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CodexStatus {
    /// No CLI was found.
    NotInstalled,
    /// The CLI was found.
    Installed {
        /// The CLI.
        cli: CodexCli,
        /// `codex --version`, when it answered.
        version: Option<String>,
        /// The sign-in state (`codex login status`; never a sign-in).
        sign_in: CodexSignIn,
    },
}

impl CodexStatus {
    /// Why a summary cannot run now: not installed, or not signed in. An unknown sign-in
    /// state is no reason (the run reports its own error).
    #[must_use]
    pub fn problem(&self) -> Option<CodexError> {
        match self {
            Self::NotInstalled => Some(CodexError::NotInstalled),
            Self::Installed {
                sign_in: CodexSignIn::NotSignedIn,
                ..
            } => Some(CodexError::NotSignedIn),
            Self::Installed { .. } => None,
        }
    }

    /// The CLI, when installed.
    #[must_use]
    pub fn cli(&self) -> Option<&CodexCli> {
        match self {
            Self::NotInstalled => None,
            Self::Installed { cli, .. } => Some(cli),
        }
    }
}

/// Asks whether Codex is installed and signed in (the CLI in production, a fake in tests).
/// Runs child processes: never call it with the store lock held.
pub trait CodexProbe {
    /// The status now.
    fn status(&self) -> CodexStatus;
}

/// The installed CLI: `CodexCli::find`, then `codex login status`, and for the Summaries
/// page also `codex --version`, each run with `timeout`.
#[derive(Debug, Clone, Copy)]
pub struct InstalledCodex {
    /// Also ask `codex --version`.
    pub version: bool,
    /// Deadline of each check.
    pub timeout: Duration,
}

impl InstalledCodex {
    /// The check at the start of a meeting: the sign-in only, within
    /// [`crate::claude_code::MEETING_START_TIMEOUT`] (a CLI that does not answer in time
    /// reads as unknown and the recording starts anyway).
    #[must_use]
    pub fn meeting_start() -> Self {
        Self {
            version: false,
            timeout: crate::claude_code::MEETING_START_TIMEOUT,
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

impl CodexProbe for InstalledCodex {
    fn status(&self) -> CodexStatus {
        let Some(cli) = CodexCli::find() else {
            return CodexStatus::NotInstalled;
        };
        let version = self
            .version
            .then(|| cli.version(self.timeout).ok())
            .flatten();
        let sign_in = cli.sign_in(self.timeout);
        CodexStatus::Installed {
            cli,
            version,
            sign_in,
        }
    }
}

/// A fixed answer (tests, and hosts that never offer Codex).
#[derive(Debug, Clone)]
pub struct FixedCodex(pub CodexStatus);

impl CodexProbe for FixedCodex {
    fn status(&self) -> CodexStatus {
        self.0.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn installed(sign_in: CodexSignIn) -> CodexStatus {
        CodexStatus::Installed {
            cli: CodexCli::at("codex.exe".into()),
            version: Some("codex-cli 0.155.1".into()),
            sign_in,
        }
    }

    #[test]
    fn codex_status_problems_are_not_installed_and_not_signed_in_only() {
        assert_eq!(
            CodexStatus::NotInstalled.problem(),
            Some(CodexError::NotInstalled)
        );
        assert_eq!(
            installed(CodexSignIn::NotSignedIn).problem(),
            Some(CodexError::NotSignedIn)
        );
        assert_eq!(
            installed(CodexSignIn::SignedIn { method: None }).problem(),
            None
        );
        assert_eq!(installed(CodexSignIn::Unknown("x".into())).problem(), None);
        assert!(CodexStatus::NotInstalled.cli().is_none());
        assert_eq!(stored_model_name(None), "codex");
        assert_eq!(
            stored_model_name(Some("gpt-5.6-luna")),
            "codex/gpt-5.6-luna"
        );
        let start = InstalledCodex::meeting_start();
        assert!(!start.version);
        assert_eq!(start.timeout, Duration::from_secs(5));
        let page = InstalledCodex::status_page();
        assert!(page.version);
        assert_eq!(page.timeout, STATUS_TIMEOUT);
    }
}
