//! [`Summarizer`] over an OpenAI-compatible or Anthropic endpoint (PLAN.md 3.7, D19).

use std::ops::Range;
use std::time::{Duration, Instant};

use earshot_core::asr::CancelToken;
use earshot_core::prompt::{
    ChatMessage, MapChunk, MergeLevel, Prompt, PromptPlan, Role, SummaryInput, SummaryRules,
    estimate_messages, grown_reply_tokens, plan_chunks_with_rules, plan_with_rules,
    reduce_groups_with_rules, reduce_prompt_with_rules, repair_message, segment_numbers,
};
use earshot_core::summary::{
    SegmentNumbers, Summarizer, SummaryError, SummaryErrorKind, SummaryV1, join_partials,
    validate_reply,
};

use crate::client::Completion;
use crate::config::{MAX_REPAIRS, ResponseMode};
use crate::error::LlmError;
use crate::extract::extract_json;
use crate::llm_client::LlmClient;

/// Most validator errors listed in one repair message.
const MAX_REPAIR_ERRORS: usize = 30;

/// The reply echoed in a repair round is cut to this many bytes per reply token (the
/// request's `max_output_tokens`), so a runaway reply cannot crowd out the prompt.
const ECHO_BYTES_PER_TOKEN: usize = 4;

/// Appended to an echoed reply that was cut.
const ECHO_CUT_MARK: &str = "\n[... reply shortened ...]";

/// What a summary run did, for reports and acceptance metrics.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SummaryReport {
    /// HTTP requests sent (including rejected response formats).
    pub requests: usize,
    /// Repair rounds (requests that fed validator errors back).
    pub repairs: usize,
    /// Response formats the server rejected before one was accepted.
    pub format_fallbacks: usize,
    /// The response format in use at the end.
    pub mode: ResponseMode,
    /// Map parts (0 for a single-pass summary).
    pub map_parts: usize,
    /// Reduce requests that succeeded.
    pub reduce_steps: usize,
    /// Summaries joined by `join_partials` because a merge gave no valid reply (0 when every
    /// merge worked).
    pub joined_parts: usize,
    /// Repeated items dropped from valid replies (`CheckedReply::duplicates_dropped`).
    pub duplicates_dropped: usize,
}

/// The outcome of [`LlmSummarizer::summarize_with_report`].
#[derive(Debug)]
pub struct SummaryRun {
    /// The validated summary, or why there is none.
    pub result: Result<SummaryV1, LlmError>,
    /// What was done, also on failure.
    pub report: SummaryReport,
}

/// One request step of a run (a single, map or reduce request with its repair rounds), for
/// evaluation timings ([`LlmSummarizer::summarize_timed`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequestStep {
    /// `single`, `map` or `reduce`.
    pub step: &'static str,
    /// Wall time of the step, from its first request to its last reply.
    pub wall: Duration,
    /// Requests of the step (repair rounds and format fallbacks included).
    pub requests: usize,
}

/// A [`SummaryRun`] with the time of each request step.
#[derive(Debug)]
pub struct TimedRun {
    /// The run.
    pub run: SummaryRun,
    /// Its steps in order, also on failure (a failed step is listed with its requests).
    pub steps: Vec<RequestStep>,
}

/// Summaries from an OpenAI-compatible chat endpoint or the Anthropic Messages API
/// ([`LlmClient`]):
/// - the request plan from [`earshot_core::prompt::plan`] (single pass or map-reduce
///   sized from the configured context window);
/// - the summary schema as structured output ([`ResponseMode::JsonSchema`]), falling
///   back in the provider's order ([`crate::Provider::fallback`]: `json_object` then
///   plain text for OpenAI-compatible endpoints, a forced tool then plain text for
///   Anthropic) when the server rejects it (on any request before the first successful
///   reply, later only when the error names the format: `response_format`,
///   `json_schema` or `json_object`, or for Anthropic `output_config`, `json_schema`,
///   `tools`, `tool_choice`, `tool_use` or `input_schema`; otherwise the rejection is
///   [`LlmError::Http`], so a context overflow cannot degrade the format; a
///   context-length error of either provider is [`LlmError::Http`] on the first request
///   too); the working
///   mode is kept for the rest of the run;
/// - the reply schema `schemas/summary-llm.v1.json` for every provider: the model cites
///   segment numbers, and each request gets its own content-sized reply budget
///   (`Prompt::max_output_tokens`, T6.20);
/// - the JSON object extracted from the reply ([`extract_json`]) and validated with
///   [`validate_reply`] against the segment numbers the step may cite (mapped back to
///   segment ids);
/// - up to [`MAX_REPAIRS`] repair rounds per step that feed the validator errors back.
///   Each round sends the step's original messages, the last reply (cut to
///   `4 x max_output_tokens` bytes, and left out when the request would exceed the
///   prompt budget) and the errors. A reply cut off at the output limit
///   (`finish_reason: length`, Anthropic `stop_reason: max_tokens`) is parsed only as a
///   whole, never searched for an inner object, and reported as truncated; its round asks
///   again with a larger budget ([`grown_reply_tokens`]), and asks for less only when the
///   budget cannot grow. A refusal
///   ([`LlmError::Refused`], Anthropic `stop_reason: refusal`) ends the run at once,
///   without repair rounds.
///
/// Needs a tokio runtime with the time driver (see [`crate::ChatClient::complete`]).
#[derive(Debug, Clone)]
pub struct LlmSummarizer {
    client: LlmClient,
}

impl LlmSummarizer {
    /// A summarizer over `client` (a [`LlmClient`], [`crate::ChatClient`] or
    /// [`crate::MessagesClient`]).
    pub fn new(client: impl Into<LlmClient>) -> Self {
        LlmSummarizer {
            client: client.into(),
        }
    }

    /// The underlying client.
    pub fn client(&self) -> &LlmClient {
        &self.client
    }

    /// Summarises `input` and reports what was done.
    pub async fn summarize_with_report(
        &self,
        input: &SummaryInput,
        cancel: &CancelToken,
    ) -> SummaryRun {
        self.summarize_timed(input, cancel).await.run
    }

    /// [`LlmSummarizer::summarize_with_report`] with the wall time of every request step
    /// (evaluation: `sum_eval`).
    pub async fn summarize_timed(&self, input: &SummaryInput, cancel: &CancelToken) -> TimedRun {
        let mut run = self.run(cancel);
        let result = run.summarize(input).await;
        run.finish(result)
    }

    /// Evaluation only (T7.4gb; production plans with [`earshot_core::prompt::plan`]): one
    /// map request per range of `ranges` ([`earshot_core::prompt::plan_chunks`], the
    /// requests the local chunked summaries send, sized for this client's budget), then the
    /// production reduce levels. One range (or none) is the production plan, as
    /// `earshot_llm_local::summarize::summarize_chunks` does.
    pub async fn summarize_chunks_timed(
        &self,
        input: &SummaryInput,
        ranges: &[Range<usize>],
        cancel: &CancelToken,
    ) -> TimedRun {
        let mut run = self.run(cancel);
        let result = if ranges.len() <= 1 {
            run.summarize(input).await
        } else {
            run.summarize_chunks(input, ranges).await
        };
        run.finish(result)
    }

    fn run<'a>(&'a self, cancel: &'a CancelToken) -> Run<'a> {
        Run {
            client: &self.client,
            rules: self.client.summary_rules(),
            cancel,
            any_success: false,
            report: SummaryReport {
                requests: 0,
                repairs: 0,
                format_fallbacks: 0,
                mode: self.client.first_mode(),
                map_parts: 0,
                reduce_steps: 0,
                joined_parts: 0,
                duplicates_dropped: 0,
            },
            steps: Vec::new(),
        }
    }
}

impl Summarizer for LlmSummarizer {
    type Error = LlmError;

    async fn summarize(
        &self,
        input: &SummaryInput,
        cancel: &CancelToken,
    ) -> Result<SummaryV1, LlmError> {
        self.summarize_with_report(input, cancel).await.result
    }
}

struct Run<'a> {
    client: &'a LlmClient,
    /// This writer's shared rules ([`LlmClient::summary_rules`], T12.17c): every request of
    /// the run (single, map, every merge, repair) starts with them, whatever
    /// [`SummaryInput::rules`] says; an evaluation's prompt variant other than current still
    /// names its own rules file. The input is never copied for them.
    rules: SummaryRules,
    cancel: &'a CancelToken,
    /// Whether a request of this run has been answered with a completion.
    any_success: bool,
    report: SummaryReport,
    steps: Vec<RequestStep>,
}

impl Run<'_> {
    fn finish(self, result: Result<SummaryV1, LlmError>) -> TimedRun {
        TimedRun {
            run: SummaryRun {
                result,
                report: self.report,
            },
            steps: self.steps,
        }
    }

    async fn summarize(&mut self, input: &SummaryInput) -> Result<SummaryV1, LlmError> {
        let budget = self.client.budget();
        let numbers = segment_numbers(input);
        let chunks = match plan_with_rules(input, self.rules, budget)? {
            PromptPlan::Single(p) => return self.step("single", p, &numbers).await,
            PromptPlan::MapReduce(chunks) => chunks,
        };
        self.map_reduce(input, chunks, &numbers).await
    }

    /// [`LlmSummarizer::summarize_chunks_timed`] with more than one range.
    async fn summarize_chunks(
        &mut self,
        input: &SummaryInput,
        ranges: &[Range<usize>],
    ) -> Result<SummaryV1, LlmError> {
        let chunks = plan_chunks_with_rules(input, self.rules, self.client.budget(), ranges)?;
        self.map_reduce(input, chunks, &segment_numbers(input))
            .await
    }

    /// A map step per chunk, then reduce levels until one summary is left.
    async fn map_reduce(
        &mut self,
        input: &SummaryInput,
        chunks: Vec<MapChunk>,
        numbers: &SegmentNumbers,
    ) -> Result<SummaryV1, LlmError> {
        let budget = self.client.budget();
        self.report.map_parts = chunks.len();
        let mut partials = Vec::with_capacity(chunks.len());
        for chunk in chunks {
            partials.push(self.step("map", chunk.prompt, numbers).await?);
        }
        loop {
            let groups = reduce_groups_with_rules(input, self.rules, &partials, budget)?;
            // One group: this merge writes the summary the user sees (and carries the style).
            let level = if groups.len() == 1 {
                MergeLevel::Last
            } else {
                MergeLevel::Inner
            };
            let mut next = Vec::with_capacity(groups.len());
            for g in &groups {
                if g.len() == 1 && groups.len() > 1 {
                    // Carried over to the next level unchanged.
                    next.push(partials[g.start].clone());
                    continue;
                }
                let p = reduce_prompt_with_rules(
                    input,
                    self.rules,
                    &partials[g.clone()],
                    budget,
                    level,
                );
                let merged = match self.step("reduce", p, numbers).await {
                    // A rougher summary rather than none (T8.4d); it says so itself.
                    Err(LlmError::Invalid { .. }) => {
                        self.report.reduce_steps += 1;
                        self.report.joined_parts = partials.len();
                        return Ok(join_partials(&partials));
                    }
                    other => other?,
                };
                next.push(merged);
                self.report.reduce_steps += 1;
            }
            if groups.len() == 1 {
                return Ok(next.pop().expect("one group gives one summary"));
            }
            partials = next;
        }
    }

    /// One summary request with up to [`MAX_REPAIRS`] repair rounds; the reply may cite the
    /// segment numbers of `prompt.cites` and gets `prompt.max_output_tokens`.
    async fn step(
        &mut self,
        name: &'static str,
        prompt: Prompt,
        numbers: &SegmentNumbers,
    ) -> Result<SummaryV1, LlmError> {
        let started = Instant::now();
        let requests = self.report.requests;
        let result = self.step_untimed(prompt, numbers).await;
        self.steps.push(RequestStep {
            step: name,
            wall: started.elapsed(),
            requests: self.report.requests - requests,
        });
        result
    }

    async fn step_untimed(
        &mut self,
        prompt: Prompt,
        numbers: &SegmentNumbers,
    ) -> Result<SummaryV1, LlmError> {
        let budget = self.client.budget();
        let mut max_tokens = prompt.max_output_tokens;
        let original = prompt.messages;
        let mut messages = original.clone();
        let mut attempts = 0;
        loop {
            let reply = self.complete(&messages, max_tokens).await?;
            attempts += 1;
            let errors = match parse_reply(&reply) {
                Ok(value) => match validate_reply(&value, numbers, &prompt.cites) {
                    Ok(checked) => {
                        self.report.duplicates_dropped += checked.duplicates_dropped;
                        return Ok(checked.summary);
                    }
                    Err(errors) => errors,
                },
                Err(e) => vec![SummaryError {
                    path: String::new(),
                    kind: SummaryErrorKind::Json(e),
                }],
            };
            if attempts > MAX_REPAIRS {
                return Err(LlmError::Invalid { attempts, errors });
            }
            self.report.repairs += 1;
            if reply.truncated()
                && let Some(grown) =
                    grown_reply_tokens(max_tokens, estimate_messages(&original), budget)
            {
                // Ask again with more room rather than for less (T8.4d).
                max_tokens = grown;
                messages = original.clone();
                continue;
            }
            let repair = repair_message(&errors, reply.truncated(), MAX_REPAIR_ERRORS);
            let echo = ChatMessage::new(
                Role::Assistant,
                cap_echo(&reply.content, ECHO_BYTES_PER_TOKEN * max_tokens),
            );
            messages = original.clone();
            messages.push(echo);
            messages.push(repair.clone());
            if estimate_messages(&messages) + max_tokens > budget.context_window {
                // The path-qualified errors alone must do.
                messages = original.clone();
                messages.push(repair);
            }
        }
    }

    /// One completion, falling back through the response formats the server rejects.
    async fn complete(
        &mut self,
        messages: &[ChatMessage],
        max_tokens: usize,
    ) -> Result<Completion, LlmError> {
        loop {
            self.report.requests += 1;
            match self
                .client
                .complete(messages, self.report.mode, max_tokens, self.cancel)
                .await
            {
                Err(LlmError::FormatRejected { status, message }) => {
                    let about_format = self.client.names_format(&message);
                    match self.client.fallback(self.report.mode) {
                        Some(next) if !self.any_success || about_format => {
                            self.report.mode = next;
                            self.report.format_fallbacks += 1;
                        }
                        // A later error that is not about the format (for example a
                        // context overflow) must not degrade the format.
                        _ => return Err(LlmError::Http { status, message }),
                    }
                }
                Ok(c) => {
                    self.any_success = true;
                    return Ok(c);
                }
                other => return other,
            }
        }
    }
}

/// The JSON value of a reply. A reply cut off at the output limit is accepted only if the
/// whole text is JSON; searching it for an inner object would return a fragment (for
/// example one note section) and produce misleading repair errors.
fn parse_reply(reply: &Completion) -> Result<serde_json::Value, String> {
    if reply.truncated() {
        return serde_json::from_str(reply.content.trim()).map_err(|_| {
            "the reply was cut off at the output limit (finish_reason length or stop_reason \
             max_tokens) before the JSON object was complete"
                .to_string()
        });
    }
    extract_json(&reply.content)
}

/// `reply` cut to at most `max_bytes` (on a character boundary) plus a marker.
fn cap_echo(reply: &str, max_bytes: usize) -> String {
    if reply.len() <= max_bytes {
        return reply.to_string();
    }
    let mut end = max_bytes;
    while !reply.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}{ECHO_CUT_MARK}", &reply[..end])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cap_echo_keeps_short_replies_and_cuts_on_a_char_boundary() {
        assert_eq!(cap_echo("abc", 3), "abc");
        assert_eq!(cap_echo("abcd", 3), format!("abc{ECHO_CUT_MARK}"));
        // 'e' with acute accent is two bytes; byte 2 is inside it.
        assert_eq!(cap_echo("a\u{e9}b", 2), format!("a{ECHO_CUT_MARK}"));
    }

    #[test]
    fn truncated_replies_are_not_searched_for_inner_objects() {
        let cut = Completion {
            content: "{\"title\": \"T\", \"enhanced_notes\": [{\"heading\": \"H\", \"bullets\": []}, {\"hea"
                .into(),
            finish_reason: Some("length".into()),
        };
        assert!(parse_reply(&cut).unwrap_err().contains("cut off"));
        let complete = Completion {
            content: "{\"a\": 1}".into(),
            finish_reason: Some("length".into()),
        };
        assert_eq!(parse_reply(&complete).unwrap(), serde_json::json!({"a": 1}));
    }
}
