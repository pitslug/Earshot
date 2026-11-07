//! Test and acceptance support for `earshot-llm` (never shipped; PLAN.md 4.1 `llm` set).
//!
//! - [`mock`]: a scripted OpenAI-compatible or Anthropic Messages endpoint bound to 127.0.0.1:0.
//! - [`replay`]: loads the committed `fixtures/llm` cases and replays each through
//!   `earshot_llm::LlmSummarizer` (OpenAI-compatible or Anthropic) against a fresh mock.
//! - [`tcp`]: the lingering close every local test server uses after its response.

#![forbid(unsafe_code)]

pub mod mock;
pub mod replay;
pub mod tcp;
