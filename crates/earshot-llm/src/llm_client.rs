//! One client type over the providers: the two HTTP APIs, chosen by [`LlmConfig::provider`],
//! and the Claude Code CLI (T7.4ga).

use earshot_core::asr::CancelToken;
use earshot_core::prompt::{ChatMessage, ContextBudget, SummaryRules};
use earshot_net::NetClient;

use crate::anthropic::MessagesClient;
use crate::claude_code::ClaudeCodeClient;
use crate::client::{ChatClient, Completion};
use crate::codex::CodexClient;
use crate::config::{LlmConfig, Provider, ResponseMode};
use crate::error::LlmError;

/// Words in an error message that show a 400 is about the requested reply format, per
/// provider. OpenAI-compatible: `response_format` and its types. Anthropic: structured
/// outputs (`output_config`, `json_schema`) and the tool request fields (`tools`,
/// `tool_choice`, `tool_use`, `input_schema`); never a bare `tool`, which would match
/// unrelated words such as "toolbox".
const OPENAI_FORMAT_WORDS: [&str; 3] = ["response_format", "json_schema", "json_object"];
const ANTHROPIC_FORMAT_WORDS: [&str; 6] = [
    "output_config",
    "json_schema",
    "tool_choice",
    "tools",
    "tool_use",
    "input_schema",
];

/// A summary client: an OpenAI-compatible or Anthropic endpoint, or the user's Claude Code
/// CLI. Cheap to clone.
#[derive(Debug, Clone)]
pub enum LlmClient {
    /// `POST <endpoint>/chat/completions`.
    OpenAi(ChatClient),
    /// `POST <root>/v1/messages`.
    Anthropic(MessagesClient),
    /// The Claude Code CLI on the user's Claude.ai sign-in (no HTTP; plain text only).
    ClaudeCode(ClaudeCodeClient),
    /// The Codex CLI on the user's ChatGPT sign-in (no HTTP; plain text only; T7.4gc).
    Codex(CodexClient),
}

impl LlmClient {
    /// The client for `config.provider`, sending through `net` (whose allowlist must
    /// include the endpoint).
    pub fn new(net: NetClient, config: LlmConfig) -> Self {
        match config.provider {
            Provider::OpenAi => LlmClient::OpenAi(ChatClient::new(net, config)),
            Provider::Anthropic => LlmClient::Anthropic(MessagesClient::new(net, config)),
        }
    }

    /// The endpoint settings (`None` for Claude Code, which has no endpoint).
    pub fn config(&self) -> Option<&LlmConfig> {
        match self {
            LlmClient::OpenAi(c) => Some(c.config()),
            LlmClient::Anthropic(c) => Some(c.config()),
            LlmClient::ClaudeCode(_) | LlmClient::Codex(_) => None,
        }
    }

    /// The context budget the request plan is sized for.
    pub fn budget(&self) -> ContextBudget {
        match self {
            LlmClient::OpenAi(c) => c.config().budget,
            LlmClient::Anthropic(c) => c.config().budget,
            LlmClient::ClaudeCode(c) => c.config().budget,
            LlmClient::Codex(c) => c.config().budget,
        }
    }

    /// The HTTP API this client speaks (decides the response-mode fallback order); `None`
    /// for Claude Code.
    pub fn provider(&self) -> Option<Provider> {
        match self {
            LlmClient::OpenAi(_) => Some(Provider::OpenAi),
            LlmClient::Anthropic(_) => Some(Provider::Anthropic),
            LlmClient::ClaudeCode(_) | LlmClient::Codex(_) => None,
        }
    }

    /// The shared rules this writer's summaries follow (T12.17c, docs/summary-eval.md): the
    /// agent CLIs (Claude Code, Codex) write with [`SummaryRules::Brief`], which on Claude
    /// kept meaning coverage at about 60% fewer words; the OpenAI-compatible and Anthropic
    /// API writers keep [`SummaryRules::Standard`] (system.md). The one place a writer's
    /// rules are chosen; [`crate::LlmSummarizer`] applies it to every request of a run.
    pub fn summary_rules(&self) -> SummaryRules {
        match self {
            LlmClient::ClaudeCode(_) | LlmClient::Codex(_) => SummaryRules::Brief,
            LlmClient::OpenAi(_) | LlmClient::Anthropic(_) => SummaryRules::Standard,
        }
    }

    /// The response mode of a run's first request: the strictest the client offers
    /// ([`ResponseMode::JsonSchema`]; Claude Code: [`ResponseMode::Text`], the schema is in
    /// the prompt).
    pub fn first_mode(&self) -> ResponseMode {
        match self {
            LlmClient::ClaudeCode(_) | LlmClient::Codex(_) => ResponseMode::Text,
            _ => ResponseMode::JsonSchema,
        }
    }

    /// The next response mode after the server rejected `mode` ([`Provider::fallback`];
    /// none for Claude Code).
    pub fn fallback(&self, mode: ResponseMode) -> Option<ResponseMode> {
        self.provider().and_then(|p| p.fallback(mode))
    }

    /// One request in `mode` with a reply budget of `max_tokens` ([`ChatClient::complete`],
    /// [`MessagesClient::complete`] or [`ClaudeCodeClient::complete`], which blocks the
    /// calling thread until the CLI ends and takes neither mode nor budget).
    pub async fn complete(
        &self,
        messages: &[ChatMessage],
        mode: ResponseMode,
        max_tokens: usize,
        cancel: &CancelToken,
    ) -> Result<Completion, LlmError> {
        match self {
            LlmClient::OpenAi(c) => c.complete(messages, mode, max_tokens, cancel).await,
            LlmClient::Anthropic(c) => c.complete(messages, mode, max_tokens, cancel).await,
            LlmClient::ClaudeCode(c) => c.complete(messages, cancel),
            LlmClient::Codex(c) => c.complete(messages, cancel),
        }
    }

    /// Whether a rejection `message` names the reply format (lower-cased word match), so
    /// a later rejection may still fall back.
    pub(crate) fn names_format(&self, message: &str) -> bool {
        let m = message.to_ascii_lowercase();
        let words: &[&str] = match self.provider() {
            Some(Provider::OpenAi) => &OPENAI_FORMAT_WORDS,
            Some(Provider::Anthropic) => &ANTHROPIC_FORMAT_WORDS,
            None => &[],
        };
        words.iter().any(|w| m.contains(w))
    }
}
impl From<ChatClient> for LlmClient {
    fn from(c: ChatClient) -> Self {
        LlmClient::OpenAi(c)
    }
}

impl From<MessagesClient> for LlmClient {
    fn from(c: MessagesClient) -> Self {
        LlmClient::Anthropic(c)
    }
}

impl From<ClaudeCodeClient> for LlmClient {
    fn from(c: ClaudeCodeClient) -> Self {
        LlmClient::ClaudeCode(c)
    }
}

impl From<CodexClient> for LlmClient {
    fn from(c: CodexClient) -> Self {
        LlmClient::Codex(c)
    }
}
