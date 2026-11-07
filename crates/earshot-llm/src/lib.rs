//! Remote LLM clients used for meeting summaries (PLAN.md 3.7, D19), built on
//! `earshot-net` (every request passes its allowlist and audit log).
//!
//! - [`LlmConfig`]: [`Provider`], validated endpoint URL, model, optional [`ApiKey`]
//!   (sent as a bearer or `x-api-key` header, never formatted), request timeout and
//!   [`earshot_core::prompt::ContextBudget`].
//! - [`ChatClient`]: OpenAI-compatible `POST <endpoint>/chat/completions` with a
//!   [`ResponseMode`] (`json_schema` with `schemas/summary.v1.json`, `json_object`, or
//!   plain text).
//! - [`MessagesClient`]: the native Anthropic Messages API `POST <root>/v1/messages`
//!   (structured output with the same schema, a forced tool, or plain text; T6.22).
//! - [`ClaudeCodeClient`]: the user's installed Claude Code CLI on their Claude.ai sign-in
//!   (T7.4ga): no HTTP, the prompt on standard input, every run in the [`HandoverLog`].
//! - [`CodexClient`]: the user's installed OpenAI Codex CLI on their ChatGPT sign-in
//!   (T7.4gc): the same route as Claude Code ([`codex`]).
//! - [`LlmClient`]: any of these clients, the HTTP ones chosen by [`LlmConfig::provider`];
//!   requests are cancellable through a [`earshot_core::asr::CancelToken`].
//! - [`LlmSummarizer`]: the [`earshot_core::summary::Summarizer`] implementation: format
//!   fallback chain per provider, JSON extraction, validation with grounding, up to
//!   [`MAX_REPAIRS`] repair rounds, map-reduce for long meetings.
//!
//! The loopback mock endpoint and the `fixtures/llm` replay cases live in the
//! non-shipped `earshot-llm-replay` crate.

#![forbid(unsafe_code)]

mod anthropic;
pub mod claude_code;
mod client;
pub mod codex;
mod config;
mod error;
mod extract;
pub mod handover;
mod llm_client;
mod summarizer;

pub use anthropic::MessagesClient;
pub use claude_code::{
    ClaudeCli, ClaudeCodeClient, ClaudeCodeConfig, ClaudeCodeError, Handover, HandoverLog,
    HandoverOutcome, HandoverSink, SignIn,
};
pub use client::{
    ChatClient, Completion, SCHEMA_NAME, SUMMARY_LLM_SCHEMA_JSON, SUMMARY_SCHEMA_JSON,
};
pub use codex::{CodexCli, CodexClient, CodexConfig, CodexError, CodexSignIn};
pub use config::{
    ANTHROPIC_API_KEY_ENV, ANTHROPIC_DEFAULT_BUDGET, ANTHROPIC_DEFAULT_MODEL,
    ANTHROPIC_DEFAULT_URL, ANTHROPIC_VERSION, ApiKey, DEFAULT_BUDGET, DEFAULT_REQUEST_TIMEOUT,
    LlmConfig, MAX_REPAIRS, Provider, ResponseMode,
};
pub use error::LlmError;
pub use extract::extract_json;
pub use llm_client::LlmClient;
pub use summarizer::{LlmSummarizer, RequestStep, SummaryReport, SummaryRun, TimedRun};
