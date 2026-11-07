//! Endpoint configuration.

use std::fmt;
use std::time::Duration;

use earshot_core::prompt::ContextBudget;
use earshot_net::{Url, validate_url};
use serde::{Deserialize, Serialize};

use crate::error::LlmError;

/// Default per-request timeout. Local models on a CPU can take minutes for a long
/// meeting.
pub const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(300);

/// Default budget: an 8k context window with 2k tokens kept for the reply, a common
/// minimum for local instruction models.
pub const DEFAULT_BUDGET: ContextBudget = ContextBudget {
    context_window: 8_192,
    max_output_tokens: 2_048,
};

/// Default budget for the Anthropic Messages API: current Claude models have context
/// windows of 200k tokens or more, and adaptive thinking spends part of `max_tokens`
/// before the summary is written, so the reply budget is larger than for local models.
pub const ANTHROPIC_DEFAULT_BUDGET: ContextBudget = ContextBudget {
    context_window: 200_000,
    max_output_tokens: 16_000,
};

/// Default Anthropic API root; requests go to `<root>/v1/messages`.
pub const ANTHROPIC_DEFAULT_URL: &str = "https://api.anthropic.com";

/// Default Claude model for summaries (the user may configure another).
pub const ANTHROPIC_DEFAULT_MODEL: &str = "claude-sonnet-5-5";

/// The `anthropic-version` header sent with every Messages request. `2023-06-01` is
/// accepted by the Anthropic API and Anthropic-compatible servers alike; the request
/// sends none of the sampling parameters, documents or citations that later versions
/// changed.
pub const ANTHROPIC_VERSION: &str = "2023-06-01";

/// The environment variable the CLI reads the Anthropic API key from (never a
/// command-line argument, which would land in shell history).
pub const ANTHROPIC_API_KEY_ENV: &str = "ANTHROPIC_API_KEY";

/// Repair rounds after the first reply (PLAN.md 3.7): at most three requests per
/// summary step.
pub const MAX_REPAIRS: usize = 2;

/// An API key. Sent only as the `Authorization: Bearer` header (OpenAI-compatible) or the
/// `x-api-key` header (Anthropic); its `Debug` output is redacted and it has no
/// `Display`, so it cannot end up in logs or error messages by formatting.
#[derive(Clone, PartialEq, Eq)]
pub struct ApiKey(String);

impl ApiKey {
    /// Wraps a key; surrounding whitespace is trimmed. `None` for an empty key.
    pub fn new(key: impl Into<String>) -> Option<Self> {
        let k = key.into().trim().to_string();
        (!k.is_empty()).then_some(ApiKey(k))
    }

    /// The key itself, for building the request header.
    pub(crate) fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for ApiKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ApiKey(<redacted>)")
    }
}

/// How the reply format is requested (PLAN.md 3.7). Each [`Provider`] has its own
/// fallback order ([`Provider::fallback`]), starting at [`ResponseMode::JsonSchema`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ResponseMode {
    /// The summary schema as structured output: OpenAI-compatible
    /// `response_format: {"type": "json_schema", "json_schema": {..., "strict": true}}`;
    /// Anthropic `output_config: {"format": {"type": "json_schema", "schema": ...}}`.
    JsonSchema,
    /// OpenAI-compatible `response_format: {"type": "json_object"}` (not an Anthropic
    /// mode).
    JsonObject,
    /// Anthropic: one tool whose `input_schema` is the summary schema, forced with
    /// `tool_choice: {"type": "tool", ...}`; the `tool_use` block's `input` is the reply
    /// (not an OpenAI-compatible mode). Current Claude models reject forced tool use, so
    /// this is only the fallback for servers without structured outputs.
    Tool,
    /// No format request; the JSON object is extracted from the text.
    Text,
}

impl ResponseMode {
    /// Short name for reports (`json_schema`, `json_object`, `tool`, `text`).
    pub fn name(self) -> &'static str {
        match self {
            ResponseMode::JsonSchema => "json_schema",
            ResponseMode::JsonObject => "json_object",
            ResponseMode::Tool => "tool",
            ResponseMode::Text => "text",
        }
    }
}

/// Which API an endpoint speaks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Provider {
    /// OpenAI-compatible `POST <endpoint>/chat/completions` (the default: local servers,
    /// OpenAI, any compatible endpoint).
    #[default]
    OpenAi,
    /// Anthropic Messages API `POST <root>/v1/messages` (Claude, opt-in, D19).
    Anthropic,
}

impl Provider {
    /// The next response mode to try when the server rejects `mode`:
    /// `json_schema -> json_object -> text` for OpenAI-compatible endpoints and
    /// `json_schema -> tool -> text` for Anthropic.
    pub fn fallback(self, mode: ResponseMode) -> Option<ResponseMode> {
        match (self, mode) {
            (_, ResponseMode::Text) => None,
            (Provider::OpenAi, ResponseMode::JsonSchema) => Some(ResponseMode::JsonObject),
            (Provider::Anthropic, ResponseMode::JsonSchema) => Some(ResponseMode::Tool),
            (_, ResponseMode::JsonObject | ResponseMode::Tool) => Some(ResponseMode::Text),
        }
    }

    /// Short name for reports and settings (`openai`, `anthropic`).
    pub fn name(self) -> &'static str {
        match self {
            Provider::OpenAi => "openai",
            Provider::Anthropic => "anthropic",
        }
    }
}

/// Settings for one endpoint.
#[derive(Debug, Clone)]
pub struct LlmConfig {
    /// Which API the endpoint speaks.
    pub provider: Provider,
    /// Base URL. OpenAI-compatible: for example `http://127.0.0.1:1234/v1`, requests go
    /// to `<endpoint>/chat/completions`. Anthropic: the API root, for example
    /// `https://api.anthropic.com`, requests go to `<endpoint>/v1/messages`.
    pub endpoint: Url,
    /// Model name sent in every request.
    pub model: String,
    /// Optional API key.
    pub api_key: Option<ApiKey>,
    /// Timeout of each HTTP request (connect, send, whole reply).
    pub request_timeout: Duration,
    /// Context window and reply budget used for chunking and `max_tokens`.
    pub budget: ContextBudget,
    /// Sampling temperature. OpenAI-compatible only: current Claude models reject
    /// non-default sampling parameters, so the Anthropic client never sends it.
    pub temperature: f32,
}

impl LlmConfig {
    /// An OpenAI-compatible config for `endpoint` (validated: http/https, a host, no
    /// credentials in the URL) and `model`, with the defaults above and temperature 0.2.
    pub fn new(endpoint: &str, model: impl Into<String>) -> Result<Self, LlmError> {
        Self::for_provider(Provider::OpenAi, endpoint, model)
    }

    /// An Anthropic Messages API config for the API root `endpoint` (for example
    /// [`ANTHROPIC_DEFAULT_URL`]) and `model`, with [`ANTHROPIC_DEFAULT_BUDGET`].
    pub fn anthropic(endpoint: &str, model: impl Into<String>) -> Result<Self, LlmError> {
        Self::for_provider(Provider::Anthropic, endpoint, model)
    }

    /// A config for `provider` (endpoint validated as in [`LlmConfig::new`]) with that
    /// provider's default budget.
    pub fn for_provider(
        provider: Provider,
        endpoint: &str,
        model: impl Into<String>,
    ) -> Result<Self, LlmError> {
        let endpoint = validate_url(endpoint).map_err(LlmError::Endpoint)?;
        Ok(LlmConfig {
            provider,
            endpoint,
            model: model.into(),
            api_key: None,
            request_timeout: DEFAULT_REQUEST_TIMEOUT,
            budget: match provider {
                Provider::OpenAi => DEFAULT_BUDGET,
                Provider::Anthropic => ANTHROPIC_DEFAULT_BUDGET,
            },
            temperature: 0.2,
        })
    }

    /// The URL every request of this config goes to: [`LlmConfig::chat_url`] or
    /// [`LlmConfig::messages_url`] by provider.
    pub fn request_url(&self) -> Url {
        match self.provider {
            Provider::OpenAi => self.chat_url(),
            Provider::Anthropic => self.messages_url(),
        }
    }

    /// `<endpoint>/chat/completions`, keeping any path prefix of the endpoint.
    pub fn chat_url(&self) -> Url {
        self.join("chat/completions")
    }

    /// `<endpoint>/v1/messages` (Anthropic), keeping any path prefix of the endpoint.
    pub fn messages_url(&self) -> Url {
        self.join("v1/messages")
    }

    fn join(&self, path: &str) -> Url {
        let mut base = self.endpoint.clone();
        base.set_query(None);
        base.set_fragment(None);
        if !base.path().ends_with('/') {
            let p = format!("{}/", base.path());
            base.set_path(&p);
        }
        base.join(path)
            .expect("a relative path joins onto an http(s) URL")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn api_key_is_never_formatted() {
        let k = ApiKey::new("  sk-secret-123 ").unwrap();
        assert_eq!(k.expose(), "sk-secret-123");
        assert!(!format!("{k:?}").contains("secret"));
        let mut c = LlmConfig::new("http://127.0.0.1:1/v1", "m").unwrap();
        c.api_key = Some(k);
        assert!(!format!("{c:?}").contains("secret"));
        assert!(ApiKey::new("   ").is_none());
    }

    #[test]
    fn endpoint_is_validated() {
        for bad in [
            "ftp://h/v1",
            "file:///C:/x",
            "not a url",
            "http://",
            "https://u:p@h/v1",
        ] {
            assert!(
                matches!(LlmConfig::new(bad, "m"), Err(LlmError::Endpoint(_))),
                "{bad}"
            );
            assert!(
                matches!(LlmConfig::anthropic(bad, "m"), Err(LlmError::Endpoint(_))),
                "{bad}"
            );
        }
    }

    #[test]
    fn chat_url_appends_to_the_endpoint_path() {
        for (endpoint, chat) in [
            (
                "http://127.0.0.1:1234/v1",
                "http://127.0.0.1:1234/v1/chat/completions",
            ),
            (
                "http://127.0.0.1:1234/v1/",
                "http://127.0.0.1:1234/v1/chat/completions",
            ),
            ("https://h.example", "https://h.example/chat/completions"),
            (
                "https://h.example/api/v1?x=1#f",
                "https://h.example/api/v1/chat/completions",
            ),
        ] {
            let c = LlmConfig::new(endpoint, "m").unwrap();
            assert_eq!(c.chat_url().as_str(), chat, "{endpoint}");
            assert_eq!(c.request_url(), c.chat_url());
        }
    }

    #[test]
    fn anthropic_config_uses_the_messages_url_and_its_own_budget() {
        for (endpoint, url) in [
            (
                ANTHROPIC_DEFAULT_URL,
                "https://api.anthropic.com/v1/messages",
            ),
            ("http://127.0.0.1:9/", "http://127.0.0.1:9/v1/messages"),
            (
                "https://proxy.example/anthropic?k=1#f",
                "https://proxy.example/anthropic/v1/messages",
            ),
        ] {
            let c = LlmConfig::anthropic(endpoint, ANTHROPIC_DEFAULT_MODEL).unwrap();
            assert_eq!(c.provider, Provider::Anthropic);
            assert_eq!(c.request_url().as_str(), url, "{endpoint}");
            assert_eq!(c.budget, ANTHROPIC_DEFAULT_BUDGET);
        }
        let c = LlmConfig::new("http://127.0.0.1:9/v1", "m").unwrap();
        assert_eq!(c.provider, Provider::OpenAi);
        assert_eq!(c.budget, DEFAULT_BUDGET);
    }

    #[test]
    fn response_modes_fall_back_in_plan_order() {
        let chain = |p: Provider| {
            let mut m = Some(ResponseMode::JsonSchema);
            let mut seen = Vec::new();
            while let Some(x) = m {
                seen.push(x.name());
                m = p.fallback(x);
            }
            seen
        };
        assert_eq!(
            chain(Provider::OpenAi),
            ["json_schema", "json_object", "text"]
        );
        assert_eq!(chain(Provider::Anthropic), ["json_schema", "tool", "text"]);
    }

    #[test]
    fn provider_names_parse_as_settings() {
        for p in [Provider::OpenAi, Provider::Anthropic] {
            let v: Provider = serde_json::from_str(&format!("\"{}\"", p.name())).unwrap();
            assert_eq!(v, p);
        }
        assert_eq!(Provider::default(), Provider::OpenAi);
    }
}
