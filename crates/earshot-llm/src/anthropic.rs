//! Native Anthropic Messages API client (`POST <root>/v1/messages`) over `earshot-net`
//! (T6.22, D19: Claude as an opt-in summary provider).
//!
//! The request is text only: the system prompt goes in `system`, the other messages in
//! `messages` (consecutive messages of one role are joined, because the API expects
//! user and assistant turns to alternate). Headers: `x-api-key` (a secret header,
//! redacted by `earshot-net` and never formatted), `anthropic-version`
//! ([`ANTHROPIC_VERSION`]) and `content-type`. No sampling parameters are sent: current
//! Claude models reject non-default `temperature`/`top_p`/`top_k`.
//!
//! Response modes, in [`crate::Provider::Anthropic`] fallback order:
//! - [`ResponseMode::JsonSchema`]: structured output,
//!   `output_config: {"format": {"type": "json_schema", "schema": <summary schema>}}`;
//!   the reply is the text blocks.
//! - [`ResponseMode::Tool`]: one tool named [`SCHEMA_NAME`] whose `input_schema` is the
//!   summary schema, forced with `tool_choice: {"type": "tool", "name": ...}`; the reply
//!   is that `tool_use` block's `input`, serialised. Current Claude models (Opus 5.5,
//!   Sonnet 5.5) reject forced tool use, so this mode only serves servers without
//!   structured outputs.
//! - [`ResponseMode::Text`]: neither; the JSON object is extracted from the text.
//!
//! A mode is reported as rejected ([`LlmError::FormatRejected`]) only for the
//! documented shape of a bad request: HTTP 400 with `error.type` `invalid_request_error`,
//! unless the message is one of the API's context-length errors
//! ([`CONTEXT_LENGTH_PHRASES`]): another format would send the same transcript again and
//! fail the same way, so those are [`LlmError::Http`] on the first request too.
//! Every other error status (401 `authentication_error`, 403, 404, 413, 429
//! `rate_limit_error`, 500, 529 `overloaded_error`) is [`LlmError::Http`] and is not
//! retried, as with the OpenAI-compatible client: the caller decides whether to try
//! again. A reply with `stop_reason: "refusal"` (HTTP 200, the model's safety classifiers
//! declined) is [`LlmError::Refused`], so the summarizer stops at once instead of
//! re-sending the transcript in repair rounds. Token usage in the reply is not reported
//! (the OpenAI-compatible client does not report it either).

use earshot_core::asr::CancelToken;
use earshot_core::prompt::{ChatMessage, Role};
use earshot_net::{Method, NetClient, NetRequest};
use serde_json::{Map, Value, json};

use crate::client::{
    Completion, SCHEMA_NAME, SUMMARY_LLM_SCHEMA_JSON, cancelled, error_message, kind,
};
use crate::config::{ANTHROPIC_VERSION, LlmConfig, ResponseMode};
use crate::error::LlmError;

/// The only status and `error.type` that mean "this request shape is not supported".
const FORMAT_REJECTION: (u16, &str) = (400, "invalid_request_error");

/// Lower-case phrases of the Messages API's context-length errors ("prompt is too long:
/// N tokens > M maximum"; "input length and `max_tokens` exceed context limit: ..."). A
/// 400 `invalid_request_error` containing one is not a format rejection.
const CONTEXT_LENGTH_PHRASES: [&str; 2] = ["prompt is too long", "exceed context limit"];

/// Longest refusal category kept in [`LlmError::Refused`].
const MAX_CATEGORY: usize = 64;

/// Description of the forced summary tool.
const TOOL_DESCRIPTION: &str =
    "Record the structured meeting summary. Call it exactly once with the whole summary.";

/// An Anthropic Messages API client for one endpoint. Cheap to clone.
#[derive(Debug, Clone)]
pub struct MessagesClient {
    net: NetClient,
    config: LlmConfig,
    schema: Value,
}

impl MessagesClient {
    /// A client sending through `net`, whose allowlist must include the endpoint.
    /// `config.provider` is not consulted: this client always speaks the Messages API
    /// to [`LlmConfig::messages_url`].
    pub fn new(net: NetClient, config: LlmConfig) -> Self {
        // The reply schema (segment numbers as citations, T6.20), as for every provider.
        let mut schema: Value = serde_json::from_str(SUMMARY_LLM_SCHEMA_JSON)
            .expect("schemas/summary-llm.v1.json is valid JSON (checked by tests)");
        // The draft marker is not part of the schema subset structured outputs and tool
        // input schemas describe; it carries no constraint.
        if let Some(o) = schema.as_object_mut() {
            o.remove("$schema");
        }
        MessagesClient {
            net,
            config,
            schema,
        }
    }

    /// The endpoint settings.
    pub fn config(&self) -> &LlmConfig {
        &self.config
    }

    /// The JSON request body for `messages` in `mode` (module docs).
    /// [`ResponseMode::JsonObject`] is not an Anthropic mode and is sent as
    /// [`ResponseMode::JsonSchema`]. `max_tokens` is the request's own reply budget
    /// (`Prompt::max_output_tokens`).
    pub fn request_body(
        &self,
        messages: &[ChatMessage],
        mode: ResponseMode,
        max_tokens: usize,
    ) -> Value {
        let (system, turns) = split_messages(messages);
        let mut body = json!({
            "model": self.config.model,
            "max_tokens": max_tokens,
            "messages": turns,
        });
        if !system.is_empty() {
            body["system"] = Value::String(system);
        }
        match mode {
            ResponseMode::JsonSchema | ResponseMode::JsonObject => {
                body["output_config"] = json!({
                    "format": { "type": "json_schema", "schema": self.schema },
                });
            }
            ResponseMode::Tool => {
                body["tools"] = json!([{
                    "name": SCHEMA_NAME,
                    "description": TOOL_DESCRIPTION,
                    "input_schema": self.schema,
                }]);
                body["tool_choice"] = json!({ "type": "tool", "name": SCHEMA_NAME });
            }
            ResponseMode::Text => {}
        }
        body
    }

    /// Sends one Messages request in `mode`. Returns [`LlmError::FormatRejected`] for a
    /// 400 `invalid_request_error` in `json_schema` or `tool` mode that is not a
    /// context-length error (the caller may fall back), [`LlmError::Http`] for every other error status, and
    /// [`LlmError::Cancelled`] as soon as `cancel` is set, abandoning the request.
    ///
    /// Needs a tokio runtime with the time driver (see [`crate::ChatClient::complete`]).
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
        let mut req = NetRequest::new(Method::POST, self.config.messages_url())
            .header("content-type", "application/json")
            .header("accept", "application/json")
            .header("anthropic-version", ANTHROPIC_VERSION)
            .body(body)
            .timeout(self.config.request_timeout);
        if let Some(key) = &self.config.api_key {
            req = req.header("x-api-key", key.expose());
        }
        let resp = tokio::select! {
            biased;
            () = cancelled(cancel) => return Err(LlmError::Cancelled),
            r = self.net.send(req) => r.map_err(LlmError::Net)?,
        };
        if !(200..300).contains(&resp.status) {
            let message = error_message(&resp.body, self.config.api_key.as_ref());
            let rejected = mode != ResponseMode::Text
                && resp.status == FORMAT_REJECTION.0
                && error_type(&resp.body).as_deref() == Some(FORMAT_REJECTION.1)
                && !names_context_length(&message);
            return Err(if rejected {
                LlmError::FormatRejected {
                    status: resp.status,
                    message,
                }
            } else {
                LlmError::Http {
                    status: resp.status,
                    message,
                }
            });
        }
        parse_message(&resp.body)
    }
}

/// The system text (system messages joined) and the user/assistant turns, with
/// consecutive messages of one role joined into one turn.
fn split_messages(messages: &[ChatMessage]) -> (String, Vec<Value>) {
    let mut system: Vec<&str> = Vec::new();
    let mut turns: Vec<(&'static str, String)> = Vec::new();
    for m in messages {
        let role = match m.role {
            Role::System => {
                system.push(&m.content);
                continue;
            }
            Role::User => "user",
            Role::Assistant => "assistant",
        };
        match turns.last_mut() {
            Some((r, text)) if *r == role => {
                text.push_str("\n\n");
                text.push_str(&m.content);
            }
            _ => turns.push((role, m.content.clone())),
        }
    }
    let turns = turns
        .into_iter()
        .map(|(role, content)| json!({ "role": role, "content": content }))
        .collect();
    (system.join("\n\n"), turns)
}

/// Whether a bad-request `message` is a context-length error ([`CONTEXT_LENGTH_PHRASES`]).
fn names_context_length(message: &str) -> bool {
    let m = message.to_ascii_lowercase();
    CONTEXT_LENGTH_PHRASES.iter().any(|p| m.contains(p))
}

/// `error.type` of an Anthropic error body (`{"type": "error", "error": {"type": ...}}`).
fn error_type(body: &[u8]) -> Option<String> {
    serde_json::from_slice::<Value>(body)
        .ok()?
        .pointer("/error/type")?
        .as_str()
        .map(str::to_string)
}

/// A Messages API reply: the summary tool's `input` when there is such a `tool_use`
/// block, else the `text` blocks joined; other blocks (thinking) are ignored. A refusal
/// (`stop_reason: "refusal"`) is [`LlmError::Refused`] whatever the content.
fn parse_message(body: &[u8]) -> Result<Completion, LlmError> {
    let v: Value = serde_json::from_slice(body)
        .map_err(|e| LlmError::BadResponse(format!("body is not JSON: {e}")))?;
    if v.get("stop_reason").and_then(Value::as_str) == Some("refusal") {
        return Err(LlmError::Refused {
            category: v
                .pointer("/stop_details/category")
                .and_then(Value::as_str)
                .map(|c| c.chars().take(MAX_CATEGORY).collect()),
        });
    }
    let blocks = match v.get("content") {
        Some(Value::Array(b)) => b,
        Some(other) => {
            return Err(LlmError::BadResponse(format!(
                "message content is not an array: {}",
                kind(other)
            )));
        }
        None => return Err(LlmError::BadResponse("reply has no content".into())),
    };
    let tool_input = blocks.iter().find_map(|b| {
        (b.get("type").and_then(Value::as_str) == Some("tool_use")
            && b.get("name").and_then(Value::as_str) == Some(SCHEMA_NAME))
        .then(|| b.get("input").cloned().unwrap_or(Value::Object(Map::new())))
    });
    let content = match tool_input {
        Some(input) => input.to_string(),
        None => blocks
            .iter()
            .filter(|b| b.get("type").and_then(Value::as_str) == Some("text"))
            .filter_map(|b| b.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join(""),
    };
    Ok(Completion {
        content,
        finish_reason: v
            .get("stop_reason")
            .and_then(Value::as_str)
            .map(str::to_string),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn anthropic_messages_split_system_and_join_same_role_turns() {
        let (system, turns) = split_messages(&[
            ChatMessage::new(Role::System, "rules"),
            ChatMessage::new(Role::User, "transcript"),
            ChatMessage::new(Role::Assistant, "{bad}"),
            ChatMessage::new(Role::User, "fix it"),
            ChatMessage::new(Role::User, "and more"),
        ]);
        assert_eq!(system, "rules");
        assert_eq!(
            turns,
            vec![
                json!({"role": "user", "content": "transcript"}),
                json!({"role": "assistant", "content": "{bad}"}),
                json!({"role": "user", "content": "fix it\n\nand more"}),
            ]
        );
    }

    #[test]
    fn anthropic_replies_are_parsed_and_malformed_ones_rejected() {
        let c = parse_message(
            br#"{"type":"message","role":"assistant","content":[{"type":"thinking","thinking":""},{"type":"text","text":"{\"a\":"},{"type":"text","text":"1}"}],"stop_reason":"end_turn"}"#,
        )
        .unwrap();
        assert_eq!(c.content, "{\"a\":1}");
        assert!(!c.truncated());
        let c = parse_message(
            br#"{"content":[{"type":"text","text":"ignored"},{"type":"tool_use","id":"t","name":"earshot_summary_v1","input":{"b":[2]}}],"stop_reason":"max_tokens"}"#,
        )
        .unwrap();
        assert_eq!(c.content, "{\"b\":[2]}");
        assert!(c.truncated());
        // A tool call of another name is not the summary.
        let c = parse_message(
            br#"{"content":[{"type":"tool_use","name":"other","input":{}},{"type":"text","text":"t"}]}"#,
        )
        .unwrap();
        assert_eq!((c.content.as_str(), c.finish_reason), ("t", None));
        // A refusal stops whatever the content, with its category when given.
        assert!(matches!(
            parse_message(
                br#"{"content":[{"type":"text","text":"{}"}],"stop_reason":"refusal","stop_details":{"type":"refusal","category":"cyber","explanation":"x"}}"#
            ),
            Err(LlmError::Refused { category: Some(c) }) if c == "cyber"
        ));
        assert!(matches!(
            parse_message(br#"{"content":[],"stop_reason":"refusal","stop_details":null}"#),
            Err(LlmError::Refused { category: None })
        ));
        for bad in [
            &b"<html>"[..],
            br#"{"type":"message"}"#,
            br#"{"content":"text"}"#,
        ] {
            assert!(
                matches!(parse_message(bad), Err(LlmError::BadResponse(_))),
                "{}",
                String::from_utf8_lossy(bad)
            );
        }
    }

    #[test]
    fn anthropic_error_type_reads_the_documented_shape() {
        assert_eq!(
            error_type(
                br#"{"type":"error","error":{"type":"invalid_request_error","message":"m"}}"#
            )
            .as_deref(),
            Some("invalid_request_error")
        );
        assert_eq!(error_type(br#"{"error":"x"}"#), None);
        assert_eq!(error_type(b"oops"), None);
    }
}
