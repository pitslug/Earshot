//! Errors of the LLM client.

use std::fmt;

use earshot_core::prompt::PromptError;
use earshot_core::summary::SummaryError;
use earshot_net::{NetError, UrlError};

/// What can go wrong when summarising through a remote endpoint. No variant carries the
/// API key: server messages are scrubbed of it before they are stored.
#[derive(Debug)]
pub enum LlmError {
    /// The endpoint URL is not usable (PLAN.md 3.7: http/https, host present).
    Endpoint(UrlError),
    /// The request failed in `earshot-net` (refused by the allowlist, transport error,
    /// timeout).
    Net(NetError),
    /// The server answered with an error status.
    Http {
        /// HTTP status.
        status: u16,
        /// The server's error message, shortened.
        message: String,
    },
    /// The server rejected the requested reply format: OpenAI-compatible
    /// `response_format` (`json_schema` or `json_object`) with 400, 422 or 501; Anthropic
    /// structured output or forced tool with 400 `invalid_request_error`.
    /// [`crate::ChatClient::complete`] and [`crate::MessagesClient::complete`] return it;
    /// [`crate::LlmSummarizer`] falls back to the next format when no request of the run
    /// has succeeded yet or the message names the format, and reports [`LlmError::Http`]
    /// otherwise.
    FormatRejected {
        /// HTTP status of the last rejection.
        status: u16,
        /// The server's error message, shortened.
        message: String,
    },
    /// The reply is not a chat completion (no `choices[0].message`) or Messages reply (no
    /// `content` array).
    BadResponse(String),
    /// The meeting does not fit the configured context window.
    Prompt(PromptError),
    /// The model's reply was still invalid after the repair rounds.
    Invalid {
        /// Requests made for this summary step (first reply plus repairs).
        attempts: usize,
        /// Validator errors of the last reply.
        errors: Vec<SummaryError>,
    },
    /// The model declined the request (Anthropic `stop_reason: "refusal"` with HTTP 200).
    /// The run stops at once: no repair round re-sends the transcript.
    Refused {
        /// `stop_details.category` when the server gave one (for example `cyber`).
        category: Option<String>,
    },
    /// The Claude Code CLI gave no reply (T7.4ga).
    ClaudeCode(crate::claude_code::ClaudeCodeError),
    /// The Codex CLI gave no reply (T7.4gc).
    Codex(crate::codex::CodexError),
    /// The cancel token was set.
    Cancelled,
}

impl fmt::Display for LlmError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LlmError::Endpoint(e) => write!(f, "invalid LLM endpoint: {e}"),
            LlmError::Net(e) => write!(f, "LLM request failed: {e}"),
            LlmError::Http { status, message } => {
                write!(f, "LLM endpoint returned HTTP {status}: {message}")
            }
            LlmError::FormatRejected { status, message } => write!(
                f,
                "LLM endpoint rejected the response format (HTTP {status}): {message}"
            ),
            LlmError::BadResponse(e) => write!(f, "LLM reply is not usable: {e}"),
            LlmError::Prompt(e) => write!(f, "{e}"),
            LlmError::Invalid { attempts, errors } => {
                write!(
                    f,
                    "no valid summary after {attempts} attempts ({} problems",
                    errors.len()
                )?;
                if let Some(e) = errors.first() {
                    write!(f, ", first: {e}")?;
                }
                f.write_str(")")
            }
            LlmError::Refused { category } => {
                f.write_str("the model declined to summarise this meeting")?;
                match category {
                    Some(c) => write!(f, " (refusal, category {c})"),
                    None => f.write_str(" (refusal)"),
                }
            }
            LlmError::ClaudeCode(e) => write!(f, "{e}"),
            LlmError::Codex(e) => write!(f, "{e}"),
            LlmError::Cancelled => f.write_str("summary cancelled"),
        }
    }
}

impl std::error::Error for LlmError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            LlmError::Endpoint(e) => Some(e),
            LlmError::Net(e) => Some(e),
            LlmError::Prompt(e) => Some(e),
            LlmError::ClaudeCode(e) => Some(e),
            LlmError::Codex(e) => Some(e),
            _ => None,
        }
    }
}

impl From<PromptError> for LlmError {
    fn from(e: PromptError) -> Self {
        LlmError::Prompt(e)
    }
}
