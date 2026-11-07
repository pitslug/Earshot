//! OpenAI-compatible `POST /chat/completions` over `earshot-net`.

use std::time::Duration;

use earshot_core::asr::CancelToken;
use earshot_core::prompt::ChatMessage;
use earshot_net::{Method, NetClient, NetRequest};
use serde_json::{Value, json};

use crate::config::{ApiKey, LlmConfig, ResponseMode};
use crate::error::LlmError;

/// The committed summary v1 schema (`schemas/summary.v1.json`) of stored summaries. The
/// `schema` ci step keeps it equal to `earshot_core::summary::json_schema()`.
pub const SUMMARY_SCHEMA_JSON: &str = include_str!("../../../schemas/summary.v1.json");

/// The committed reply schema (`schemas/summary-llm.v1.json`: summary v1 citing segment
/// numbers), sent as the strict `json_schema` response format. The `schema` ci step keeps
/// it equal to `earshot_core::summary::llm_json_schema()`.
pub const SUMMARY_LLM_SCHEMA_JSON: &str = include_str!("../../../schemas/summary-llm.v1.json");

/// Name of the schema in `response_format.json_schema.name`.
pub const SCHEMA_NAME: &str = "earshot_summary_v1";

/// How often an in-flight request checks the cancel token.
const CANCEL_POLL: Duration = Duration::from_millis(20);

/// Longest server error message kept in an error.
const MAX_ERROR_MESSAGE: usize = 300;

/// HTTP statuses that mean "this `response_format` is not supported" and trigger the
/// next mode: 400 (OpenAI and llama.cpp), 422 (vLLM and FastAPI-based servers), 501.
const FORMAT_REJECTION_STATUSES: [u16; 3] = [400, 422, 501];

/// `error.code` / `error.type` values of a context-length error: OpenAI's
/// `context_length_exceeded`, llama.cpp server's `exceed_context_size_error`.
// llama.cpp: ggml-org/llama.cpp 060b2fea4ed528810ddc91d48fb22b3e4d63eb85 (master,
// 2026-11-01), tools/server/server-common.cpp:125-127 `format_error_response`:
// ERROR_TYPE_EXCEED_CONTEXT_SIZE -> type "exceed_context_size_error", code 400; the body is
// `{"error": {"code", "message", "type", "n_prompt_tokens", "n_ctx"}}`
// (server-task.cpp:1546-1553, server-context.cpp:5286-5289) with the HTTP status from
// `code`. OpenAI's `context_length_exceeded` is from its API error responses; OpenAI
// publishes no server source, so it is not pinned to one.
const CONTEXT_LENGTH_CODES: [&str; 2] = ["context_length_exceeded", "exceed_context_size_error"];

/// Lower-case message phrases of a context-length error, a secondary match behind
/// [`CONTEXT_LENGTH_CODES`] (vLLM sends no code of its own): OpenAI and vLLM ("This
/// model's maximum context length is N tokens", vLLM also "exceeds model's maximum
/// context length"), vLLM's reply budget above the context ("max_tokens=N cannot be
/// greater than max_model_len=M"), llama.cpp server ("request (N tokens) exceeds the
/// available context size", "input (N tokens) is larger than the max context size").
// vLLM: vllm-project/vllm e0d00e4b8eb0c89ec30921d0695cfdd409e9c211 (main, 2026-11-01).
// vllm/renderers/params.py:404-413 and 543-550 ("This model's maximum context length is
// {max_total_tokens} tokens. However, your prompt contains ..."), 277-284
// ("{max_output_tokens_param}={n} cannot be greater than {max_total_tokens_param}={m}.
// Please request fewer output tokens."; chat completions name them `max_tokens` or
// `max_completion_tokens` and `max_model_len`, entrypoints/openai/chat_completion/
// protocol.py:689-702), entrypoints/serve/utils/api_utils.py:248-252 ("Input length (n)
// exceeds model's maximum context length (m)."); each is HTTP 400 with body `{"error":
// {"message", "type": "BadRequestError", "param", "code": 400}}`
// (entrypoints/serve/exception_handling/error_response.py, engine/protocol.py:79-87).
// llama.cpp (commit above): tools/server/server-context.cpp:4080-4086 ("request (%d tokens)
// exceeds the available context size (%d tokens), try increasing it") and 4070-4077
// ("input (%d tokens) is larger than the max context size (%d tokens). skipping"), both
// type exceed_context_size_error. OpenAI's wording is from its API error responses (no
// source to pin).
const CONTEXT_LENGTH_PHRASES: [&str; 4] = [
    "maximum context length",
    "cannot be greater than max_model_len",
    "exceeds the available context size",
    "is larger than the max context size",
];

/// One model reply.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Completion {
    /// The reply text: OpenAI-compatible `choices[0].message.content` (empty when the
    /// server sent null); Anthropic the summary tool's `input` as JSON when the reply
    /// has that `tool_use` block, else its `text` blocks joined.
    pub content: String,
    /// Why generation stopped: OpenAI-compatible `choices[0].finish_reason`, Anthropic
    /// `stop_reason`, if given.
    pub finish_reason: Option<String>,
}

impl Completion {
    /// Whether the reply stopped at the output limit (`finish_reason: "length"`, or the
    /// Anthropic `stop_reason: "max_tokens"`).
    pub fn truncated(&self) -> bool {
        matches!(self.finish_reason.as_deref(), Some("length" | "max_tokens"))
    }
}

/// A chat completions client for one endpoint. Cheap to clone.
#[derive(Debug, Clone)]
pub struct ChatClient {
    net: NetClient,
    config: LlmConfig,
    schema: Value,
}

impl ChatClient {
    /// A client sending through `net`, whose allowlist must include the endpoint.
    pub fn new(net: NetClient, config: LlmConfig) -> Self {
        let schema = serde_json::from_str(SUMMARY_LLM_SCHEMA_JSON)
            .expect("schemas/summary-llm.v1.json is valid JSON (checked by tests)");
        ChatClient {
            net,
            config,
            schema,
        }
    }

    /// The endpoint settings.
    pub fn config(&self) -> &LlmConfig {
        &self.config
    }

    /// The JSON request body for `messages` in `mode`, with a reply budget of
    /// `max_tokens` (the request's own budget, `Prompt::max_output_tokens`).
    pub fn request_body(
        &self,
        messages: &[ChatMessage],
        mode: ResponseMode,
        max_tokens: usize,
    ) -> Value {
        let mut body = json!({
            "model": self.config.model,
            "messages": messages,
            "temperature": self.config.temperature,
            "max_tokens": max_tokens,
            "stream": false,
        });
        let format = match mode {
            // `Tool` is an Anthropic mode that `Provider::OpenAi` never falls back to; if
            // asked for it, this client sends its own strictest format.
            ResponseMode::JsonSchema | ResponseMode::Tool => Some(json!({
                "type": "json_schema",
                "json_schema": {
                    "name": SCHEMA_NAME,
                    "strict": true,
                    "schema": self.schema,
                },
            })),
            ResponseMode::JsonObject => Some(json!({ "type": "json_object" })),
            ResponseMode::Text => None,
        };
        if let Some(f) = format {
            body["response_format"] = f;
        }
        body
    }

    /// Sends one chat completion request in `mode`. A rejection of the response format
    /// (see [`LlmError::FormatRejected`]) is returned for `json_schema` and `json_object`
    /// so the caller can fall back, unless it is a context-length error (`error.code`
    /// `context_length_exceeded`, [`CONTEXT_LENGTH_PHRASES`]), which is [`LlmError::Http`]
    /// on the first request too; in `text` mode an error status is [`LlmError::Http`]. Returns [`LlmError::Cancelled`] as soon as `cancel` is set,
    /// abandoning the request.
    ///
    /// Must run inside a tokio runtime with the time driver enabled (for example
    /// `Builder::enable_all` or `#[tokio::test]`): cancellation polling uses
    /// `tokio::time::sleep` and `earshot-net` timeouts use tokio timers. Without it the
    /// call panics.
    pub async fn complete(
        &self,
        messages: &[ChatMessage],
        mode: ResponseMode,
        max_tokens: usize,
        cancel: &CancelToken,
    ) -> Result<Completion, LlmError> {
        if cancel.is_cancelled() {
            return Err(LlmError::Cancelled);
        }
        let body = serde_json::to_vec(&self.request_body(messages, mode, max_tokens))
            .map_err(|e| LlmError::BadResponse(e.to_string()))?;
        let mut req = NetRequest::new(Method::POST, self.config.chat_url())
            .header("content-type", "application/json")
            .header("accept", "application/json")
            .body(body)
            .timeout(self.config.request_timeout);
        if let Some(key) = &self.config.api_key {
            req = req.header("authorization", format!("Bearer {}", key.expose()));
        }
        let resp = tokio::select! {
            biased;
            () = cancelled(cancel) => return Err(LlmError::Cancelled),
            r = self.net.send(req) => r.map_err(LlmError::Net)?,
        };
        if !(200..300).contains(&resp.status) {
            let message = error_message(&resp.body, self.config.api_key.as_ref());
            // A context-length error is not about the format: another format would send
            // the same transcript again and fail the same way.
            return Err(
                if mode != ResponseMode::Text
                    && FORMAT_REJECTION_STATUSES.contains(&resp.status)
                    && !names_context_length(&resp.body, &message)
                {
                    LlmError::FormatRejected {
                        status: resp.status,
                        message,
                    }
                } else {
                    LlmError::Http {
                        status: resp.status,
                        message,
                    }
                },
            );
        }
        parse_completion(&resp.body)
    }
}

/// Whether an error response is a context-length error: its `error.code` or `error.type`
/// is one of [`CONTEXT_LENGTH_CODES`], or its `message` holds one of
/// [`CONTEXT_LENGTH_PHRASES`].
fn names_context_length(body: &[u8], message: &str) -> bool {
    let coded = serde_json::from_slice::<Value>(body).is_ok_and(|v| {
        ["/error/code", "/error/type"]
            .iter()
            .filter_map(|p| v.pointer(p).and_then(Value::as_str))
            .any(|c| CONTEXT_LENGTH_CODES.contains(&c))
    });
    let m = message.to_ascii_lowercase();
    coded || CONTEXT_LENGTH_PHRASES.iter().any(|p| m.contains(p))
}

/// The server's error message: `error.message`, `error` or `message` of a JSON body
/// (OpenAI-compatible and Anthropic error shapes), else the body text; whitespace
/// collapsed, shortened, and with the API key removed in case the server echoed it.
pub(crate) fn error_message(body: &[u8], api_key: Option<&ApiKey>) -> String {
    let text = String::from_utf8_lossy(body);
    let from_json = serde_json::from_str::<Value>(&text).ok().and_then(|v| {
        [
            v.pointer("/error/message"),
            v.get("error"),
            v.get("message"),
        ]
        .into_iter()
        .flatten()
        .find_map(|m| m.as_str().map(str::to_string))
    });
    let mut msg = from_json
        .unwrap_or_else(|| text.into_owned())
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    if let Some(key) = api_key {
        msg = msg.replace(key.expose(), "<redacted>");
    }
    if msg.chars().count() > MAX_ERROR_MESSAGE {
        msg = msg.chars().take(MAX_ERROR_MESSAGE).collect::<String>() + "...";
    }
    msg
}

fn parse_completion(body: &[u8]) -> Result<Completion, LlmError> {
    let v: Value = serde_json::from_slice(body)
        .map_err(|e| LlmError::BadResponse(format!("body is not JSON: {e}")))?;
    let choice = v
        .pointer("/choices/0")
        .ok_or_else(|| LlmError::BadResponse("no choices".into()))?;
    let message = choice
        .get("message")
        .ok_or_else(|| LlmError::BadResponse("choices[0] has no message".into()))?;
    let content = match message.get("content") {
        Some(Value::String(s)) => s.clone(),
        None | Some(Value::Null) => String::new(),
        Some(other) => {
            return Err(LlmError::BadResponse(format!(
                "message content is not a string: {}",
                kind(other)
            )));
        }
    };
    Ok(Completion {
        content,
        finish_reason: choice
            .get("finish_reason")
            .and_then(Value::as_str)
            .map(str::to_string),
    })
}

pub(crate) fn kind(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

/// Resolves once `cancel` is set.
pub(crate) async fn cancelled(cancel: &CancelToken) {
    while !cancel.is_cancelled() {
        tokio::time::sleep(CANCEL_POLL).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_context_length_phrase_is_recognised_without_a_code() {
        // The upstream messages of CONTEXT_LENGTH_PHRASES (sources in its comment), each
        // in a body without a context-length code, so the phrase alone decides.
        for message in [
            "This model's maximum context length is 4096 tokens. However, your prompt contains 5000 input tokens, which exceeds this limit even without any output tokens. Please reduce the length of the input prompt.",
            "Input length (5000) exceeds model's maximum context length (4096).",
            "max_tokens=8192 cannot be greater than max_model_len=4096. Please request fewer output tokens.",
            "max_completion_tokens=8192 cannot be greater than max_model_len=4096. Please request fewer output tokens.",
            "request (9000 tokens) exceeds the available context size (8192 tokens), try increasing it",
            "input (9000 tokens) is larger than the max context size (8192 tokens). skipping",
        ] {
            let body =
                json!({"error": {"message": message, "type": "BadRequestError", "code": 400}});
            let body = serde_json::to_vec(&body).unwrap();
            let shown = error_message(&body, None);
            assert!(names_context_length(&body, &shown), "{message}");
        }
        // Other 400s, format complaints included, are not context-length errors.
        for message in [
            "response_format json_schema is not supported",
            "max_tokens must be at least 1, got 0.",
            "this model is not supported",
        ] {
            let body =
                json!({"error": {"message": message, "type": "BadRequestError", "code": 400}});
            let body = serde_json::to_vec(&body).unwrap();
            let shown = error_message(&body, None);
            assert!(!names_context_length(&body, &shown), "{message}");
        }
        // The codes alone decide too.
        for code in CONTEXT_LENGTH_CODES {
            let body =
                serde_json::to_vec(&json!({"error": {"message": "x", "type": code}})).unwrap();
            assert!(names_context_length(&body, "x"), "{code}");
        }
    }

    #[test]
    fn committed_schema_equals_the_core_schema() {
        let committed: Value = serde_json::from_str(SUMMARY_SCHEMA_JSON).unwrap();
        assert_eq!(committed, earshot_core::summary::json_schema());
        // Strict structured outputs need closed objects with every property required.
        assert_eq!(committed["additionalProperties"], json!(false));
    }

    #[test]
    fn completions_are_parsed_and_malformed_ones_rejected() {
        let c = parse_completion(
            br#"{"choices":[{"index":0,"message":{"role":"assistant","content":"{}"},"finish_reason":"stop"}]}"#,
        )
        .unwrap();
        assert_eq!(c.content, "{}");
        assert!(!c.truncated());
        let c = parse_completion(
            br#"{"choices":[{"message":{"content":null},"finish_reason":"length"}]}"#,
        )
        .unwrap();
        assert_eq!(c.content, "");
        assert!(c.truncated());
        for bad in [
            &b"<html>"[..],
            br#"{"choices":[]}"#,
            br#"{"choices":[{"text":"x"}]}"#,
            br#"{"choices":[{"message":{"content":[1]}}]}"#,
        ] {
            assert!(
                matches!(parse_completion(bad), Err(LlmError::BadResponse(_))),
                "{}",
                String::from_utf8_lossy(bad)
            );
        }
    }
}
