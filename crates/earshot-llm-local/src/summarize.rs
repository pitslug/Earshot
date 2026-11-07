//! A whole summary from a local model: the production request plan
//! ([`earshot_core::prompt::plan`], single pass or map-reduce, transcript lines numbered per
//! meeting), each request generated under the grounded summary grammar ([`summary_grammar`]
//! with the segment numbers that request may cite) with the request's own, content-sized
//! reply budget ([`Prompt::max_output_tokens`]), validated with [`validate_reply`] (numbers
//! mapped back to segment ids, repetition rejected), and repaired like the remote summarizer
//! (`earshot_llm::LlmSummarizer`): up to [`MAX_REPAIRS`] rounds per step, each sending the
//! step's original messages, the last reply (cut, and left out when it does not fit) and
//! [`repair_message`]. A reply the loop guard stops ([`crate::Stop::Looping`]) is rejected as
//! repetition, not as a truncated reply.
//!
//! The planner counts tokens with [`earshot_core::prompt::estimate_tokens`], which is tuned
//! for cl100k-style vocabularies; a local model's tokenizer can need more. So the plan is
//! sized to the model: [`plan_for_model`] plans for the model's context, counts every
//! planned request with the model's own tokenizer, and while one does not fit (with its
//! reply budget and [`REPAIR_MAX_TOKENS`] left free) re-plans with the planner's window
//! shrunk by the measured overshoot. Reduce requests use the same planner window and are
//! counted before they are sent.
//!
//! A run stops cooperatively on [`SummaryOptions::cancel`]: before every request and
//! between generated tokens ([`LocalLlm::generate_cancellable`]), ending in
//! [`SummaryFailure::Cancelled`].

use std::time::{Duration, Instant};

use earshot_core::asr::CancelToken;
use earshot_core::prompt::{
    ChatMessage, ContextBudget, MapChunk, MergeLevel, Prompt, PromptError, PromptPlan,
    REPAIR_MAX_TOKENS, Role, SummaryInput, grown_reply_tokens, plan, plan_chunks,
    plan_chunks_two_call, reduce_groups, reduce_groups_two_call, reduce_prompt,
    reduce_prompt_two_call, repair_message, segment_numbers,
};
use earshot_core::summary::{
    SegmentNumbers, SummaryError, SummaryErrorKind, SummaryV1, drop_repeated_actions,
    join_partials, validate_reply,
};

use crate::grammar::{GrammarError, SummaryLimits, SummaryPart, summary_part_grammar};
use crate::{Generation, LlmError, LocalLlm, Stop};

use SummaryPart::Whole;

/// Repair rounds after the first reply of a step (PLAN.md 3.7; the remote summarizer's
/// `earshot_llm::config::MAX_REPAIRS`, kept equal).
pub const MAX_REPAIRS: usize = 2;

/// Most validator errors listed in one repair message (as the remote summarizer).
const MAX_REPAIR_ERRORS: usize = 30;

/// The echoed reply is cut to this many bytes per reply token (as the remote summarizer).
const ECHO_BYTES_PER_TOKEN: usize = 4;

/// Most re-plans while sizing the plan to the model's tokenizer.
const MAX_REPLANS: usize = 8;

/// What the summarizer needs from a model: [`LocalLlm`] in production, a fake in tests.
pub trait Generate {
    /// Context size in tokens.
    fn context_tokens(&self) -> usize;
    /// Prompt tokens of `messages` plus the assistant prefix.
    ///
    /// # Errors
    /// The model's error.
    fn prompt_tokens(&self, messages: &[ChatMessage], prefix: &str) -> Result<usize, LlmError>;
    /// One constrained reply; stops with [`LlmError::Cancelled`] when `cancel` is
    /// cancelled while it runs.
    ///
    /// # Errors
    /// The model's error.
    fn generate(
        &mut self,
        messages: &[ChatMessage],
        prefix: &str,
        grammar: &str,
        max_tokens: usize,
        cancel: &CancelToken,
    ) -> Result<Generation, LlmError>;
}

impl Generate for LocalLlm {
    fn context_tokens(&self) -> usize {
        LocalLlm::context_tokens(self)
    }

    fn prompt_tokens(&self, messages: &[ChatMessage], prefix: &str) -> Result<usize, LlmError> {
        LocalLlm::prompt_tokens(self, messages, prefix)
    }

    fn generate(
        &mut self,
        messages: &[ChatMessage],
        prefix: &str,
        grammar: &str,
        max_tokens: usize,
        cancel: &CancelToken,
    ) -> Result<Generation, LlmError> {
        self.generate_cancellable(messages, prefix, grammar, max_tokens, cancel)
    }
}

/// Settings of one summary run.
#[derive(Debug, Clone)]
pub struct SummaryOptions {
    /// The largest reply budget of any request (`ContextBudget::max_output_tokens`); each
    /// request gets the budget the planner sizes to its content, at most this.
    pub max_output_tokens: usize,
    /// Text appended where the assistant's reply starts (see
    /// [`LocalLlm::generate_with_prefix`]); empty for most models.
    pub assistant_prefix: String,
    /// List and citation caps of the grammar.
    pub limits: SummaryLimits,
    /// Stops the run (module docs); `CancelToken::new()` for a run nobody cancels.
    pub cancel: CancelToken,
}

/// Why a run produced no summary.
#[derive(Debug, Clone, PartialEq)]
pub enum SummaryFailure {
    /// No plan fits the model's context.
    Plan(PromptError),
    /// A request still does not fit after sizing (for example a reduce request).
    TooLong {
        /// Prompt tokens counted by the model.
        prompt: usize,
        /// Tokens available for the prompt beside its reply budget.
        available: usize,
    },
    /// The grammar could not be built.
    Grammar(GrammarError),
    /// The model failed.
    Model(LlmError),
    /// [`SummaryOptions::cancel`] was cancelled.
    Cancelled,
    /// Still invalid after every repair round.
    Invalid {
        /// Which step failed: `single`, `map` or `reduce`.
        step: &'static str,
        /// Requests made for the step.
        attempts: usize,
        /// The last validation errors.
        errors: Vec<SummaryError>,
        /// The end of the last reply (at most [`REPLY_TAIL_CHARS`] characters), to tell a
        /// long summary from a reply that loops.
        last_reply_tail: String,
    },
}

/// Characters of the last reply kept in [`SummaryFailure::Invalid`].
pub const REPLY_TAIL_CHARS: usize = 400;

/// The last `n` characters of `s`.
fn tail(s: &str, n: usize) -> String {
    let count = s.chars().count();
    s.chars().skip(count.saturating_sub(n)).collect()
}

impl std::fmt::Display for SummaryFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SummaryFailure::Plan(e) => write!(f, "plan: {e}"),
            SummaryFailure::TooLong { prompt, available } => write!(
                f,
                "a request of {prompt} prompt tokens does not fit in {available}"
            ),
            SummaryFailure::Grammar(e) => write!(f, "{e}"),
            SummaryFailure::Model(e) => write!(f, "{e}"),
            SummaryFailure::Cancelled => f.write_str("the summary was cancelled"),
            SummaryFailure::Invalid {
                step,
                attempts,
                errors,
                ..
            } => {
                write!(f, "{step} step invalid after {attempts} requests: ")?;
                let list: Vec<String> = errors.iter().take(5).map(ToString::to_string).collect();
                write!(f, "{}", list.join("; "))
            }
        }
    }
}

/// What a run did, also on failure.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct SummaryReport {
    /// Model calls.
    pub requests: usize,
    /// Repair rounds.
    pub repairs: usize,
    /// Replies cut off at their reply budget.
    pub truncated: usize,
    /// Replies stopped by the loop guard.
    pub looping: usize,
    /// Replies that failed validation (each one triggers a repair or the failure).
    pub invalid_replies: usize,
    /// Repeated items dropped from valid replies (`CheckedReply::duplicates_dropped`).
    pub duplicates_dropped: usize,
    /// Summaries joined by `join_partials` because a merge gave no valid reply (0 when every
    /// merge worked).
    pub joined_parts: usize,
    /// Map parts (0 for a single request).
    pub map_parts: usize,
    /// Map parts given ready-made by the caller ([`summarize_chunks_with`]), not mapped in
    /// this run; included in `map_parts`.
    pub reused_parts: usize,
    /// Reduce requests.
    pub reduce_steps: usize,
    /// The planner's context window after sizing to the tokenizer.
    pub planner_window: usize,
    /// Re-plans needed to fit the model's tokenizer.
    pub replans: usize,
    /// Largest prompt of the run, in model tokens.
    pub max_prompt_tokens: usize,
    /// Largest reply budget of the run.
    pub max_reply_budget: usize,
    /// Prompt tokens over all requests.
    pub prompt_tokens: usize,
    /// Reply tokens over all requests.
    pub generated_tokens: usize,
    /// Prompt processing time over all requests.
    pub prompt_time: Duration,
    /// Generation time over all requests.
    pub generate_time: Duration,
    /// Every step in the order it ran (repairs included in their step).
    pub steps: Vec<StepTime>,
}

/// One step of a run: a single, map or reduce request with its repair rounds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StepTime {
    /// `single`, `map` or `reduce`.
    pub step: &'static str,
    /// Wall time of the step, from building its grammar to its last reply.
    pub wall: Duration,
    /// Model calls of the step.
    pub requests: usize,
}

/// The outcome of [`summarize`].
#[derive(Debug, Clone)]
pub struct SummaryRun {
    /// The validated summary, or why there is none.
    pub result: Result<SummaryV1, SummaryFailure>,
    /// What was done.
    pub report: SummaryReport,
}

/// Prompt tokens a request with a reply budget of `reply` may use on `model` (repair round
/// included): the context minus the reply budget.
fn room(model: &dyn Generate, reply: usize) -> usize {
    model.context_tokens().saturating_sub(reply)
}

/// The request plan for `input` sized to `model` (module docs): the plan and the planner
/// window it was made with, and the number of re-plans.
///
/// # Errors
/// [`SummaryFailure::Plan`] when no window fits, [`SummaryFailure::Model`] when counting
/// fails.
pub fn plan_for_model(
    model: &dyn Generate,
    input: &SummaryInput,
    options: &SummaryOptions,
) -> Result<(PromptPlan, usize, usize), SummaryFailure> {
    let mut window = model.context_tokens();
    for replans in 0..=MAX_REPLANS {
        let budget = ContextBudget {
            context_window: window,
            max_output_tokens: options.max_output_tokens,
        };
        let p = plan(input, budget).map_err(SummaryFailure::Plan)?;
        let prompts: Vec<&Prompt> = match &p {
            PromptPlan::Single(s) => vec![s],
            PromptPlan::MapReduce(chunks) => chunks.iter().map(|c| &c.prompt).collect(),
        };
        // The smallest share of its room any planned request leaves unused (below 1 when
        // one overshoots).
        let mut worst = f64::INFINITY;
        for pr in prompts {
            let tokens = model
                .prompt_tokens(&pr.messages, &options.assistant_prefix)
                .map_err(SummaryFailure::Model)?;
            let limit = room(model, pr.max_output_tokens).saturating_sub(REPAIR_MAX_TOKENS);
            worst = worst.min(limit as f64 / tokens.max(1) as f64);
        }
        if worst >= 1.0 {
            return Ok((p, window, replans));
        }
        // Shrink the planner's window by the overshoot, at least one token.
        #[allow(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            clippy::cast_precision_loss
        )]
        let scaled = (window as f64 * worst.max(0.0)) as usize;
        window = scaled.min(window.saturating_sub(1));
    }
    Err(SummaryFailure::Plan(PromptError::ContextTooSmall {
        needed: model.context_tokens() + 1,
        available: model.context_tokens(),
    }))
}

/// Summarises `input` on `model` (module docs).
pub fn summarize(
    model: &mut dyn Generate,
    input: &SummaryInput,
    options: &SummaryOptions,
) -> SummaryRun {
    let mut report = SummaryReport::default();
    let result = run(model, input, options, &mut report);
    SummaryRun { result, report }
}

fn run(
    model: &mut dyn Generate,
    input: &SummaryInput,
    options: &SummaryOptions,
    report: &mut SummaryReport,
) -> Result<SummaryV1, SummaryFailure> {
    let (p, window, replans) = plan_for_model(model, input, options)?;
    report.planner_window = window;
    report.replans = replans;
    let numbers = segment_numbers(input);
    let chunks = match p {
        PromptPlan::Single(s) => {
            return step(model, "single", Whole, s, &numbers, options, report);
        }
        PromptPlan::MapReduce(chunks) => chunks,
    };
    let budget = ContextBudget {
        context_window: window,
        max_output_tokens: options.max_output_tokens,
    };
    let none = vec![None; chunks.len()];
    map_reduce(
        model, input, chunks, none, budget, &numbers, options, report,
    )
}

/// Summarises `input` on `model` with map requests over the given transcript `ranges`
/// ([`earshot_core::prompt::plan_chunks`]: consecutive, non-empty, covering every segment)
/// and the usual reduce, instead of the planner's chunks: the forced-chunking evaluation
/// (T6.20, summaries made during a meeting). The reduce plans for the model's whole context
/// (no re-plan: a request the model's tokenizer finds too long fails as
/// [`SummaryFailure::TooLong`]). One range is the ordinary single request.
///
/// # Panics
/// When `ranges` do not cover the segments consecutively (see `plan_chunks`).
pub fn summarize_chunks(
    model: &mut dyn Generate,
    input: &SummaryInput,
    options: &SummaryOptions,
    ranges: &[std::ops::Range<usize>],
) -> SummaryRun {
    summarize_chunks_with(model, input, options, ranges, vec![None; ranges.len()])
}

/// [`summarize_chunks`] where some parts are already summarised (T8.3j: chunks mapped
/// during the meeting and re-cited onto this transcript, `earshot_core::summary::recite`):
/// `done[i]` is used as the partial of `ranges[i]` instead of a map step, and every other
/// range is mapped. The partials must cite only segments of `input`. One range is the
/// ordinary single request, whatever `done` holds.
///
/// # Panics
/// When `done` and `ranges` differ in length, or as [`summarize_chunks`].
pub fn summarize_chunks_with(
    model: &mut dyn Generate,
    input: &SummaryInput,
    options: &SummaryOptions,
    ranges: &[std::ops::Range<usize>],
    done: Vec<Option<SummaryV1>>,
) -> SummaryRun {
    assert_eq!(done.len(), ranges.len(), "one entry of `done` per range");
    let mut report = SummaryReport::default();
    let result = if ranges.len() <= 1 {
        run(model, input, options, &mut report)
    } else {
        let budget = chunk_budget(model, options);
        report.planner_window = budget.context_window;
        match plan_chunks(input, budget, ranges) {
            Ok(chunks) => map_reduce(
                model,
                input,
                chunks,
                done,
                budget,
                &segment_numbers(input),
                options,
                &mut report,
            ),
            Err(e) => Err(SummaryFailure::Plan(e)),
        }
    };
    SummaryRun { result, report }
}

/// The map step of the last of `ranges` alone (T8.3j: a chunk summarised during the
/// meeting, from the transcript so far): the same request [`summarize_chunks`] would send
/// for that part, numbered over all of `input`. Its result is that part's partial summary.
///
/// # Panics
/// When `ranges` is empty, or as [`summarize_chunks`].
pub fn map_last_chunk(
    model: &mut dyn Generate,
    input: &SummaryInput,
    options: &SummaryOptions,
    ranges: &[std::ops::Range<usize>],
) -> SummaryRun {
    assert!(!ranges.is_empty(), "no chunk to map");
    let mut report = SummaryReport::default();
    let budget = chunk_budget(model, options);
    report.planner_window = budget.context_window;
    let result = match plan_chunks(input, budget, ranges) {
        Ok(mut chunks) => {
            report.map_parts = 1;
            let last = chunks.pop().expect("one chunk per range");
            step(
                model,
                "map",
                Whole,
                last.prompt,
                &segment_numbers(input),
                options,
                &mut report,
            )
        }
        Err(e) => Err(SummaryFailure::Plan(e)),
    };
    SummaryRun { result, report }
}

/// The budget of a forced-chunking run: the model's whole context.
fn chunk_budget(model: &dyn Generate, options: &SummaryOptions) -> ContextBudget {
    ContextBudget {
        context_window: model.context_tokens(),
        max_output_tokens: options.max_output_tokens,
    }
}

/// The map steps of `chunks` (skipping those `done` already holds), then reduce levels
/// until one summary is left.
#[allow(clippy::too_many_arguments)]
fn map_reduce(
    model: &mut dyn Generate,
    input: &SummaryInput,
    chunks: Vec<MapChunk>,
    done: Vec<Option<SummaryV1>>,
    budget: ContextBudget,
    numbers: &SegmentNumbers,
    options: &SummaryOptions,
    report: &mut SummaryReport,
) -> Result<SummaryV1, SummaryFailure> {
    report.map_parts = chunks.len();
    let mut partials = Vec::with_capacity(chunks.len());
    for (chunk, ready) in chunks.into_iter().zip(done) {
        if let Some(p) = ready {
            report.reused_parts += 1;
            partials.push(p);
            continue;
        }
        partials.push(step(
            model,
            "map",
            Whole,
            chunk.prompt,
            numbers,
            options,
            report,
        )?);
    }
    reduce_levels(
        model,
        input,
        partials,
        budget,
        numbers,
        options,
        report,
        Merge::Plain,
        &mut |_| {},
    )
}

/// The reduce a merge uses: the production one, or the two-call merge (T6.20c, evaluation
/// only) that also folds reworded action items.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Merge {
    /// [`reduce_groups`] and [`reduce_prompt`].
    Plain,
    /// [`reduce_groups_two_call`] and [`reduce_prompt_two_call`].
    TwoCall,
}

/// Reduce levels over `partials` with `merge` until one summary is left: each level groups
/// the partials, carries a group of one over unchanged (unless it is the only group) and
/// sends one `reduce` step per other group, whose summary `on_reduce` sees. Always makes at
/// least one level.
#[allow(clippy::too_many_arguments)]
fn reduce_levels(
    model: &mut dyn Generate,
    input: &SummaryInput,
    mut partials: Vec<SummaryV1>,
    budget: ContextBudget,
    numbers: &SegmentNumbers,
    options: &SummaryOptions,
    report: &mut SummaryReport,
    merge: Merge,
    on_reduce: &mut dyn FnMut(&SummaryV1),
) -> Result<SummaryV1, SummaryFailure> {
    loop {
        let groups = match merge {
            Merge::Plain => reduce_groups(input, &partials, budget),
            Merge::TwoCall => reduce_groups_two_call(input, &partials, budget),
        }
        .map_err(SummaryFailure::Plan)?;
        // One group: this merge writes the summary the user sees (and carries the style).
        let level = if groups.len() == 1 {
            MergeLevel::Last
        } else {
            MergeLevel::Inner
        };
        let mut next = Vec::with_capacity(groups.len());
        for g in &groups {
            if g.len() == 1 && groups.len() > 1 {
                next.push(partials[g.start].clone());
                continue;
            }
            let p = match merge {
                Merge::Plain => reduce_prompt(input, &partials[g.clone()], budget, level),
                Merge::TwoCall => reduce_prompt_two_call(input, &partials[g.clone()], budget),
            };
            report.reduce_steps += 1;
            let merged = match step(model, "reduce", Whole, p, numbers, options, report) {
                // A rougher summary rather than none (T8.4d); it says so itself.
                Err(SummaryFailure::Invalid { .. }) if merge == Merge::Plain => {
                    report.joined_parts = partials.len();
                    return Ok(join_partials(&partials));
                }
                other => other?,
            };
            on_reduce(&merged);
            next.push(merged);
        }
        if groups.len() == 1 {
            return Ok(next.pop().expect("one group gives one summary"));
        }
        partials = next;
    }
}

/// Action item counts of a two-call run ([`summarize_chunks_two_call`]): whether the merge
/// loses actions.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TwoCallActions {
    /// Action items each part's extraction request returned (after that reply's own repeats
    /// were dropped by validation), in part order; parts not reached are missing.
    pub per_part: Vec<usize>,
    /// The sum of `per_part`.
    pub extracted: usize,
    /// Exact repeats across parts removed in code before the merge
    /// ([`drop_repeated_actions`]).
    pub exact_repeats: usize,
    /// Action items the merge was given (`extracted - exact_repeats`).
    pub to_merge: usize,
    /// Action items of the final summary; `None` when the run failed.
    pub final_count: Option<usize>,
    /// Merge (reduce) steps whose reply filled the action item list to the grammar's cap
    /// ([`SummaryLimits::max_items`]): such a merge may have lost actions to the cap rather
    /// than by folding them, so `to_merge -> final_count` alone cannot tell the two apart.
    pub cap_hit: usize,
}

/// The outcome of [`summarize_chunks_two_call`].
#[derive(Debug, Clone)]
pub struct TwoCallRun {
    /// The validated summary, or why there is none.
    pub result: Result<SummaryV1, SummaryFailure>,
    /// What was done (steps `map-actions`, `map-rest`, `reduce`).
    pub report: SummaryReport,
    /// Action item counts.
    pub actions: TwoCallActions,
}

/// The two-call chunked evaluation (T6.20c, evaluation only; production never calls it):
/// per range ([`plan_chunks_two_call`]) one request that may fill only the action items
/// ([`SummaryPart::ActionsOnly`]) and one that may fill everything else
/// ([`SummaryPart::NoActions`]), each validated and repaired as any step; a part's partial
/// summary is the second reply with the first reply's action items. Exact repeats of action
/// items across parts are then dropped in code ([`drop_repeated_actions`]) and the partials
/// merged with the two-call reduce ([`reduce_groups_two_call`], [`reduce_prompt_two_call`]),
/// which also folds reworded actions. One range gives one part and no merge. As
/// [`summarize_chunks`], the requests are planned for the model's whole context; no prompt
/// prefix (KV cache) is shared between requests.
///
/// # Panics
/// When `ranges` do not cover the segments consecutively (see `plan_chunks`).
pub fn summarize_chunks_two_call(
    model: &mut dyn Generate,
    input: &SummaryInput,
    options: &SummaryOptions,
    ranges: &[std::ops::Range<usize>],
) -> TwoCallRun {
    let mut report = SummaryReport::default();
    let mut actions = TwoCallActions::default();
    let result = two_call(model, input, options, ranges, &mut report, &mut actions);
    actions.final_count = result.as_ref().ok().map(|s| s.action_items.len());
    TwoCallRun {
        result,
        report,
        actions,
    }
}

fn two_call(
    model: &mut dyn Generate,
    input: &SummaryInput,
    options: &SummaryOptions,
    ranges: &[std::ops::Range<usize>],
    report: &mut SummaryReport,
    actions: &mut TwoCallActions,
) -> Result<SummaryV1, SummaryFailure> {
    let budget = ContextBudget {
        context_window: model.context_tokens(),
        max_output_tokens: options.max_output_tokens,
    };
    report.planner_window = budget.context_window;
    let chunks = plan_chunks_two_call(input, budget, ranges).map_err(SummaryFailure::Plan)?;
    let numbers = segment_numbers(input);
    report.map_parts = chunks.len();
    let mut partials = Vec::with_capacity(chunks.len());
    for chunk in chunks {
        let extracted = step(
            model,
            "map-actions",
            SummaryPart::ActionsOnly,
            chunk.actions,
            &numbers,
            options,
            report,
        )?;
        actions.per_part.push(extracted.action_items.len());
        actions.extracted += extracted.action_items.len();
        let mut partial = step(
            model,
            "map-rest",
            SummaryPart::NoActions,
            chunk.rest,
            &numbers,
            options,
            report,
        )?;
        partial.action_items = extracted.action_items;
        partials.push(partial);
    }
    actions.exact_repeats = drop_repeated_actions(&mut partials);
    actions.to_merge = actions.extracted - actions.exact_repeats;
    // One part needs no merge.
    if partials.len() == 1 {
        return Ok(partials.pop().expect("one partial"));
    }
    let cap = options.limits.max_items as usize;
    let cap_hit = &mut actions.cap_hit;
    reduce_levels(
        model,
        input,
        partials,
        budget,
        &numbers,
        options,
        report,
        Merge::TwoCall,
        &mut |merged| {
            if merged.action_items.len() >= cap {
                *cap_hit += 1;
            }
        },
    )
}

/// `reply` cut to at most `max_bytes` on a character boundary, with a marker.
fn cap_echo(reply: &str, max_bytes: usize) -> String {
    if reply.len() <= max_bytes {
        return reply.to_string();
    }
    let mut end = max_bytes;
    while !reply.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}\n[... reply shortened ...]", &reply[..end])
}

/// One request (`step`: single, map or reduce; `map-actions` or `map-rest` in a two-call run)
/// with up to [`MAX_REPAIRS`] repair rounds; the reply may cite the segment numbers of
/// `prompt.cites`, may fill the lists of `part` and gets `prompt.max_output_tokens`. Its wall
/// time goes into [`SummaryReport::steps`], also when it fails.
fn step(
    model: &mut dyn Generate,
    step: &'static str,
    part: SummaryPart,
    prompt: Prompt,
    numbers: &SegmentNumbers,
    options: &SummaryOptions,
    report: &mut SummaryReport,
) -> Result<SummaryV1, SummaryFailure> {
    let started = Instant::now();
    let before = report.requests;
    let result = step_requests(model, step, part, prompt, numbers, options, report);
    report.steps.push(StepTime {
        step,
        wall: started.elapsed(),
        requests: report.requests - before,
    });
    result
}

fn step_requests(
    model: &mut dyn Generate,
    step: &'static str,
    part: SummaryPart,
    prompt: Prompt,
    numbers: &SegmentNumbers,
    options: &SummaryOptions,
    report: &mut SummaryReport,
) -> Result<SummaryV1, SummaryFailure> {
    let grammar = summary_part_grammar(&prompt.cites, options.limits, part)
        .map_err(SummaryFailure::Grammar)?;
    let mut reply_budget = prompt.max_output_tokens;
    report.max_reply_budget = report.max_reply_budget.max(reply_budget);
    let original = prompt.messages;
    let mut messages = original.clone();
    let mut attempts = 0;
    loop {
        let limit = room(model, reply_budget);
        let tokens = model
            .prompt_tokens(&messages, &options.assistant_prefix)
            .map_err(SummaryFailure::Model)?;
        report.max_prompt_tokens = report.max_prompt_tokens.max(tokens);
        if tokens > limit {
            return Err(SummaryFailure::TooLong {
                prompt: tokens,
                available: limit,
            });
        }
        if options.cancel.is_cancelled() {
            return Err(SummaryFailure::Cancelled);
        }
        let g = model
            .generate(
                &messages,
                &options.assistant_prefix,
                &grammar,
                reply_budget,
                &options.cancel,
            )
            .map_err(|e| match e {
                LlmError::Cancelled => SummaryFailure::Cancelled,
                e => SummaryFailure::Model(e),
            })?;
        attempts += 1;
        report.requests += 1;
        report.prompt_tokens += g.prompt_tokens;
        report.generated_tokens += g.generated_tokens;
        report.prompt_time += g.prompt_time;
        report.generate_time += g.generate_time;
        let truncated = g.stop == Stop::MaxTokens;
        if truncated {
            report.truncated += 1;
        }
        let errors = if g.stop == Stop::Looping {
            report.looping += 1;
            vec![SummaryError {
                path: String::new(),
                kind: SummaryErrorKind::Repetition,
            }]
        } else {
            match serde_json::from_str::<serde_json::Value>(g.text.trim()) {
                Ok(value) => match validate_reply(&value, numbers, &prompt.cites) {
                    Ok(checked) => {
                        report.duplicates_dropped += checked.duplicates_dropped;
                        return Ok(checked.summary);
                    }
                    Err(errors) => errors,
                },
                Err(e) => vec![SummaryError {
                    path: String::new(),
                    kind: SummaryErrorKind::Json(if truncated {
                        "the reply was cut off at the output limit before the JSON object was \
                         complete"
                            .to_string()
                    } else {
                        e.to_string()
                    }),
                }],
            }
        };
        report.invalid_replies += 1;
        if attempts > MAX_REPAIRS {
            return Err(SummaryFailure::Invalid {
                step,
                attempts,
                errors,
                last_reply_tail: tail(&g.text, REPLY_TAIL_CHARS),
            });
        }
        report.repairs += 1;
        if truncated {
            // Ask again with more room rather than for less: greedy decoding repeats a
            // cut-off reply, and the budget must not decide a summary's length (T8.4d).
            let original_tokens = model
                .prompt_tokens(&original, &options.assistant_prefix)
                .map_err(SummaryFailure::Model)?;
            let budget = ContextBudget {
                context_window: model.context_tokens(),
                max_output_tokens: options.max_output_tokens,
            };
            if let Some(grown) = grown_reply_tokens(reply_budget, original_tokens, budget) {
                reply_budget = grown;
                report.max_reply_budget = report.max_reply_budget.max(reply_budget);
                messages = original.clone();
                continue;
            }
        }
        let repair = repair_message(&errors, truncated, MAX_REPAIR_ERRORS);
        let echo = ChatMessage::new(
            Role::Assistant,
            cap_echo(&g.text, ECHO_BYTES_PER_TOKEN * reply_budget),
        );
        messages = original.clone();
        messages.push(echo);
        messages.push(repair.clone());
        let with_echo = model
            .prompt_tokens(&messages, &options.assistant_prefix)
            .map_err(SummaryFailure::Model)?;
        if with_echo > limit {
            // The path-qualified errors alone must do.
            messages = original.clone();
            messages.push(repair);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;

    use earshot_core::asr::MeetingTime;
    use earshot_core::identity::EntityId;
    use earshot_core::prompt::{MeetingInfo, PromptSegment, Template, estimate_messages};

    use super::*;

    fn id(n: usize) -> EntityId {
        format!("0193a5b0-1c00-7000-8000-{n:012x}").parse().unwrap()
    }

    fn input(segments: usize) -> SummaryInput {
        SummaryInput {
            meeting: MeetingInfo {
                title: None,
                started: None,
                duration_ms: 60_000,
                attendees: vec!["A".into(), "B".into()],
            },
            segments: (0..segments)
                .map(|i| PromptSegment {
                    id: id(i + 1),
                    start: MeetingTime::from_millis(i as u64 * 1000),
                    speaker: if i % 2 == 0 { "A" } else { "B" }.into(),
                    text: format!("we talked about item number {i} and agreed on it"),
                })
                .collect(),
            notes: Vec::new(),
            template: Template::General,
            prompt_variant: Default::default(),
            style: Default::default(),
            user_name: None,
        }
    }

    /// The first segment number a request shows: a transcript line `[n] ...` or, in a
    /// merge request, the first citation of a partial summary.
    fn first_number(messages: &[ChatMessage]) -> Option<u64> {
        let user = &messages.iter().find(|m| m.role == Role::User)?.content;
        for line in user.lines() {
            if let Some(n) = line
                .strip_prefix('[')
                .and_then(|r| r.split(']').next())
                .and_then(|n| n.parse().ok())
            {
                return Some(n);
            }
        }
        let at = user.find("\"source_segment_ids\":[")? + "\"source_segment_ids\":[".len();
        user[at..].split([',', ']']).next()?.parse().ok()
    }

    fn valid(cite: u64) -> String {
        format!(
            r#"{{"title":"T","tldr":"x","enhanced_notes":[],"decisions":[{{"text":"d","source_segment_ids":[{cite}]}}],"action_items":[],"open_questions":[],"topics":[]}}"#
        )
    }

    /// Counts tokens as the estimate times `factor`; replies from `script` (None = a valid
    /// reply citing the first segment the request shows).
    /// With `cancel_during` set, the token is cancelled while that request (0-based)
    /// generates, and the request returns [`LlmError::Cancelled`] as `LocalLlm` does when
    /// the token is cancelled between tokens.
    struct Fake {
        context: usize,
        factor: f64,
        script: VecDeque<Option<(String, Stop)>>,
        calls: Vec<(Vec<ChatMessage>, String, String, usize)>,
        cancel_during: Option<(usize, CancelToken)>,
    }

    impl Fake {
        fn new(context: usize, factor: f64) -> Self {
            Fake {
                context,
                factor,
                script: VecDeque::new(),
                calls: Vec::new(),
                cancel_during: None,
            }
        }
    }

    impl Generate for Fake {
        fn context_tokens(&self) -> usize {
            self.context
        }

        fn prompt_tokens(&self, m: &[ChatMessage], prefix: &str) -> Result<usize, LlmError> {
            Ok(((estimate_messages(m) + prefix.len()) as f64 * self.factor).ceil() as usize)
        }

        fn generate(
            &mut self,
            m: &[ChatMessage],
            prefix: &str,
            grammar: &str,
            max_tokens: usize,
            cancel: &CancelToken,
        ) -> Result<Generation, LlmError> {
            assert!(
                !cancel.is_cancelled(),
                "a request started after cancellation"
            );
            let prompt = self.prompt_tokens(m, prefix)?;
            assert!(prompt + max_tokens <= self.context, "request overflows");
            if let Some((n, token)) = &self.cancel_during
                && *n == self.calls.len()
            {
                token.cancel();
                assert!(cancel.is_cancelled(), "the run's token");
                self.calls
                    .push((m.to_vec(), prefix.into(), grammar.into(), max_tokens));
                return Err(LlmError::Cancelled);
            }
            self.calls.push((
                m.to_vec(),
                prefix.to_string(),
                grammar.to_string(),
                max_tokens,
            ));
            let (text, stop) = match self.script.pop_front().flatten() {
                Some(x) => x,
                None => (
                    valid(first_number(m).expect("a number")),
                    Stop::EndOfGeneration,
                ),
            };
            Ok(Generation {
                text,
                stop,
                prompt_tokens: prompt,
                generated_tokens: 10,
                prompt_time: Duration::from_millis(1),
                generate_time: Duration::from_millis(2),
            })
        }
    }

    fn options() -> SummaryOptions {
        SummaryOptions {
            max_output_tokens: 2000,
            assistant_prefix: String::new(),
            limits: SummaryLimits::default(),
            cancel: CancelToken::new(),
        }
    }

    /// The citation alternatives of a grammar's decision rule (`( "1" | "2" )`).
    fn citable(grammar: &str) -> Vec<u64> {
        let line = grammar
            .lines()
            .find(|l| l.starts_with("Decision-source-segment-ids-item ::= "))
            .expect("citation rule");
        line.split('"').filter_map(|t| t.parse().ok()).collect()
    }

    #[test]
    fn summarize_small_meeting_is_one_grounded_request() {
        let mut m = Fake::new(16_384, 1.0);
        let run = summarize(&mut m, &input(5), &options());
        let s = run.result.unwrap();
        // The reply cited segment 1; the stored summary holds its id.
        assert_eq!(s.decisions[0].source_segment_ids, [id(1)]);
        assert_eq!(run.report.requests, 1);
        assert_eq!(run.report.map_parts, 0);
        assert_eq!(run.report.planner_window, 16_384);
        // The grammar lists every segment number of the meeting.
        assert_eq!(citable(&m.calls[0].2), [1, 2, 3, 4, 5]);
        // A short meeting gets the minimum reply budget, not the ceiling.
        assert_eq!(m.calls[0].3, earshot_core::prompt::REPLY_MIN_TOKENS);
    }

    #[test]
    fn summarize_long_meeting_maps_with_chunk_numbers_then_reduces() {
        let mut m = Fake::new(4096, 1.0);
        let inp = input(300);
        let run = summarize(&mut m, &inp, &options());
        run.result.unwrap();
        let r = &run.report;
        assert!(r.map_parts >= 2, "{r:?}");
        assert!(r.reduce_steps >= 1, "{r:?}");
        assert_eq!(r.requests, r.map_parts + r.reduce_steps);
        // A map request's grammar allows only its own chunk's numbers: the first chunk's
        // grammar never names the last segment.
        let first = citable(&m.calls[0].2);
        assert_eq!(first[0], 1);
        assert!(!first.contains(&300));
        // The final reduce may cite exactly what its partial summaries cite (as shown in
        // its prompt), never another segment of the meeting.
        let last = m.calls.last().unwrap();
        let mut shown: Vec<u64> = last.0[1]
            .content
            .split("\"source_segment_ids\":[")
            .skip(1)
            .flat_map(|rest| {
                rest.split(']')
                    .next()
                    .unwrap_or("")
                    .split(',')
                    .filter_map(|n| n.trim().parse().ok())
                    .collect::<Vec<u64>>()
            })
            .collect();
        shown.sort_unstable();
        shown.dedup();
        assert!(!shown.is_empty());
        assert_eq!(citable(&last.2), shown);
        assert!(!citable(&last.2).contains(&300));
        // Every request got the budget the planner sized for it.
        assert!(m.calls.iter().all(|c| c.3 <= options().max_output_tokens));
    }

    #[test]
    fn summarize_chunks_maps_the_given_chunks_then_reduces_and_times_each_step() {
        let mut m = Fake::new(16_384, 1.0);
        let inp = input(30);
        // Everything fits one request, but the chunks are forced.
        let ranges = [0..10, 10..25, 25..30];
        let run = summarize_chunks(&mut m, &inp, &options(), &ranges);
        run.result.unwrap();
        let r = &run.report;
        assert_eq!((r.map_parts, r.reduce_steps, r.requests), (3, 1, 4));
        for (call, range) in m.calls.iter().zip(&ranges) {
            let want: Vec<u64> = (range.start as u64 + 1..=range.end as u64).collect();
            assert_eq!(citable(&call.2), want);
        }
        let kinds: Vec<&str> = r.steps.iter().map(|s| s.step).collect();
        assert_eq!(kinds, ["map", "map", "map", "reduce"]);
        assert!(r.steps.iter().all(|s| s.requests == 1));
        // A repaired step counts its repair in the same step.
        let mut m = Fake::new(16_384, 1.0);
        m.script.push_back(None);
        m.script
            .push_back(Some(("{}".into(), Stop::EndOfGeneration)));
        let run = summarize_chunks(&mut m, &inp, &options(), &ranges);
        run.result.unwrap();
        let reqs: Vec<usize> = run.report.steps.iter().map(|s| s.requests).collect();
        assert_eq!(reqs, [1, 2, 1, 1]);
        // One chunk is the ordinary single request.
        let mut m = Fake::new(16_384, 1.0);
        let whole = 0..30;
        let run = summarize_chunks(&mut m, &inp, &options(), std::slice::from_ref(&whole));
        run.result.unwrap();
        assert_eq!(run.report.map_parts, 0);
        assert_eq!(run.report.steps.len(), 1);
        assert_eq!(run.report.steps[0].step, "single");
    }

    #[test]
    fn summarize_chunks_with_ready_parts_maps_only_the_others_and_reduces_them_all() {
        let inp = input(30);
        let ranges = [0..10, 10..25, 25..30];
        // A ready partial for the first part, citing segment 3 (in its range).
        let ready = SummaryV1 {
            title: "ready".into(),
            decisions: vec![earshot_core::summary::Decision {
                text: "made during the meeting".into(),
                source_segment_ids: vec![id(3)],
            }],
            ..SummaryV1::default()
        };
        let mut m = Fake::new(16_384, 1.0);
        let run = summarize_chunks_with(
            &mut m,
            &inp,
            &options(),
            &ranges,
            vec![Some(ready.clone()), None, None],
        );
        run.result.unwrap();
        let r = &run.report;
        assert_eq!((r.map_parts, r.reused_parts, r.reduce_steps), (3, 1, 1));
        let kinds: Vec<&str> = r.steps.iter().map(|s| s.step).collect();
        assert_eq!(kinds, ["map", "map", "reduce"]);
        // The two map requests are parts 2 and 3; the reduce shows the ready partial.
        assert_eq!(citable(&m.calls[0].2)[0], 11);
        assert_eq!(citable(&m.calls[1].2)[0], 26);
        assert!(m.calls[2].0[1].content.contains("made during the meeting"));
        assert!(citable(&m.calls[2].2).contains(&3));

        // All parts ready: only the reduce runs.
        let mut m = Fake::new(16_384, 1.0);
        let run = summarize_chunks_with(
            &mut m,
            &inp,
            &options(),
            &ranges,
            vec![
                Some(ready.clone()),
                Some(ready.clone()),
                Some(ready.clone()),
            ],
        );
        run.result.unwrap();
        assert_eq!((run.report.reused_parts, run.report.requests), (3, 1));

        // One range is the single request whatever is ready.
        let mut m = Fake::new(16_384, 1.0);
        let whole = 0..30;
        let run = summarize_chunks_with(
            &mut m,
            &inp,
            &options(),
            std::slice::from_ref(&whole),
            vec![Some(ready)],
        );
        run.result.unwrap();
        assert_eq!(run.report.steps[0].step, "single");
        assert_eq!(run.report.reused_parts, 0);
    }

    #[test]
    fn summarize_map_last_chunk_sends_the_same_request_as_that_part_of_a_chunked_run() {
        let inp = input(30);
        let ranges = [0..10, 10..25, 25..30];
        let mut whole = Fake::new(16_384, 1.0);
        summarize_chunks(&mut whole, &inp, &options(), &ranges)
            .result
            .unwrap();
        let mut one = Fake::new(16_384, 1.0);
        let run = map_last_chunk(&mut one, &inp, &options(), &ranges);
        let partial = run.result.unwrap();
        assert_eq!(one.calls.len(), 1);
        assert_eq!(one.calls[0], whole.calls[2], "the third map request");
        assert_eq!(partial.decisions[0].source_segment_ids, [id(26)]);
        assert_eq!((run.report.map_parts, run.report.reduce_steps), (1, 0));
        // The first window of a meeting so far is one range: part 1 of 1, still a map.
        let mut first = Fake::new(16_384, 1.0);
        let so_far = SummaryInput {
            segments: inp.segments[..10].to_vec(),
            ..inp.clone()
        };
        let window = 0..10;
        let run = map_last_chunk(
            &mut first,
            &so_far,
            &options(),
            std::slice::from_ref(&window),
        );
        run.result.unwrap();
        assert_eq!(run.report.steps[0].step, "map");
        assert_eq!(citable(&first.calls[0].2), (1..=10).collect::<Vec<u64>>());
    }

    #[test]
    fn summarize_chunks_sends_the_inputs_prompt_variant_in_every_request() {
        let owner_rule = "the owner is the person addressed or named";
        for (variant, expected) in [
            // The production rules carry the owner rule since T6.20f; the earlier ones do not.
            (earshot_core::prompt::PromptVariant::Previous, false),
            (earshot_core::prompt::PromptVariant::Current, true),
        ] {
            let mut m = Fake::new(16_384, 1.0);
            // The first map reply is not JSON, so a repair round is sent too.
            m.script
                .push_back(Some(("not json".into(), Stop::EndOfGeneration)));
            let inp = SummaryInput {
                prompt_variant: variant,
                ..input(30)
            };
            let run = summarize_chunks(&mut m, &inp, &options(), &[0..10, 10..25, 25..30]);
            run.result.unwrap();
            assert_eq!((run.report.repairs, run.report.requests), (1, 5));
            for call in &m.calls {
                assert_eq!(call.0[0].role, Role::System);
                assert_eq!(
                    call.0[0].content.contains(owner_rule),
                    expected,
                    "{variant}"
                );
            }
        }
    }

    #[test]
    fn summarize_sends_the_style_only_in_the_request_that_writes_the_summary() {
        let style = earshot_core::prompt::SummaryStyle::new("Name the Kestrel project.").unwrap();
        let styled = |inp: SummaryInput| SummaryInput {
            style: style.clone(),
            ..inp
        };
        let carries = |call: &(Vec<ChatMessage>, String, String, usize)| {
            call.0[0].content.contains("Kestrel")
        };
        // One request: it carries the style.
        let mut m = Fake::new(16_384, 1.0);
        summarize(&mut m, &styled(input(5)), &options())
            .result
            .unwrap();
        assert_eq!(m.calls.len(), 1);
        assert!(carries(&m.calls[0]));
        // Map-reduce, possibly over several merge levels: only the last merge carries it.
        let mut m = Fake::new(4096, 1.0);
        let run = summarize(&mut m, &styled(input(300)), &options());
        run.result.unwrap();
        assert!(run.report.map_parts >= 2, "{:?}", run.report);
        let (last, rest) = m.calls.split_last().unwrap();
        assert!(carries(last));
        assert!(!rest.iter().any(carries));
        // Forced chunks with the first part already summarised (live partials): the maps
        // still sent and every inner merge carry no style; the last merge does.
        let mut m = Fake::new(16_384, 1.0);
        let ranges = [0..10, 10..25, 25..30];
        let done = vec![
            Some(serde_json::from_str::<SummaryV1>(&valid_ids(1)).unwrap()),
            None,
            None,
        ];
        let run = summarize_chunks_with(&mut m, &styled(input(30)), &options(), &ranges, done);
        run.result.unwrap();
        assert_eq!(m.calls.len(), 3, "two maps and the merge");
        let (last, rest) = m.calls.split_last().unwrap();
        assert!(carries(last));
        assert!(!rest.iter().any(carries));
    }

    /// A stored summary (segment ids) citing segment `n`.
    fn valid_ids(n: usize) -> String {
        format!(
            r#"{{"title":"T","tldr":"x","enhanced_notes":[],"decisions":[{{"text":"d","source_segment_ids":["{}"]}}],"action_items":[],"open_questions":[],"topics":[]}}"#,
            id(n)
        )
    }

    /// A reply of one two-call request: `actions` as (owner, task, citation), plus one
    /// decision citing `decision` when set.
    fn part_reply(actions: &[(&str, &str, u64)], decision: Option<u64>) -> Option<(String, Stop)> {
        let actions: Vec<serde_json::Value> = actions
            .iter()
            .map(|(o, t, c)| serde_json::json!({"owner": o, "task": t, "due": null, "source_segment_ids": [c]}))
            .collect();
        let decisions: Vec<serde_json::Value> = decision
            .map(|c| serde_json::json!({"text": "d", "source_segment_ids": [c]}))
            .into_iter()
            .collect();
        let v = serde_json::json!({
            "title": "T", "tldr": "x", "enhanced_notes": [], "decisions": decisions,
            "action_items": actions, "open_questions": [], "topics": [],
        });
        Some((v.to_string(), Stop::EndOfGeneration))
    }

    #[test]
    fn summarize_two_call_extracts_actions_and_the_rest_per_chunk_then_merges() {
        use crate::grammar::{SummaryPart, summary_part_grammar};
        let mut m = Fake::new(16_384, 1.0);
        let inp = input(30);
        let ranges = [0..10, 10..25, 25..30];
        for r in [
            part_reply(
                &[("Ana", "Send the minutes", 1), ("Ben", "Draw it", 2)],
                None,
            ),
            part_reply(&[], Some(1)),
            // An exact repeat of part 1 (case and full stop aside) and a reworded task.
            part_reply(
                &[
                    ("ana", "send the minutes.", 11),
                    ("Ben", "Draw it again", 12),
                ],
                None,
            ),
            part_reply(&[], Some(11)),
            part_reply(&[], None),
            part_reply(&[], Some(26)),
            // The merge folds the reworded task into the first.
            part_reply(
                &[("Ana", "Send the minutes", 1), ("Ben", "Draw it", 2)],
                Some(11),
            ),
        ] {
            m.script.push_back(r);
        }
        let run = summarize_chunks_two_call(&mut m, &inp, &options(), &ranges);
        let summary = run.result.unwrap();
        let r = &run.report;
        assert_eq!((r.map_parts, r.reduce_steps, r.requests), (3, 1, 7));
        let kinds: Vec<&str> = r.steps.iter().map(|s| s.step).collect();
        assert_eq!(
            kinds,
            [
                "map-actions",
                "map-rest",
                "map-actions",
                "map-rest",
                "map-actions",
                "map-rest",
                "reduce"
            ]
        );
        // Each request is constrained to its part of the reply and to its own segments.
        let limits = SummaryLimits::default();
        for (i, range) in ranges.iter().enumerate() {
            let cites = range.clone().into();
            for (call, part) in [
                (2 * i, SummaryPart::ActionsOnly),
                (2 * i + 1, SummaryPart::NoActions),
            ] {
                assert_eq!(
                    m.calls[call].2,
                    summary_part_grammar(&cites, limits, part).unwrap()
                );
            }
            assert!(
                m.calls[2 * i].0[0]
                    .content
                    .contains("extracts only the action items")
            );
            assert!(
                m.calls[2 * i + 1].0[0]
                    .content
                    .contains("leave `action_items` as an empty list")
            );
            // Both requests show the same transcript part.
            assert_eq!(m.calls[2 * i].0[1], m.calls[2 * i + 1].0[1]);
        }
        let merge = &m.calls[6].0;
        assert!(
            merge[0]
                .content
                .contains("Never drop a distinct commitment")
        );
        // The merge sees part 1's actions and decision, part 2 without its exact repeat.
        let user = &merge[1].content;
        assert_eq!(user.matches("Send the minutes").count(), 1, "{user}");
        assert!(!user.contains("send the minutes."), "{user}");
        assert!(user.contains("Draw it again"), "{user}");
        assert_eq!(
            run.actions,
            TwoCallActions {
                per_part: vec![2, 2, 0],
                extracted: 4,
                exact_repeats: 1,
                to_merge: 3,
                final_count: Some(2),
                cap_hit: 0,
            }
        );
        assert_eq!(summary.action_items.len(), 2);
        assert_eq!(summary.decisions.len(), 1);
    }

    #[test]
    fn summarize_two_call_counts_merges_that_fill_the_action_list() {
        let cap = SummaryLimits::default().max_items as usize;
        let tasks: Vec<String> = (1..=cap).map(|k| format!("Task {k}")).collect();
        let full: Vec<(&str, &str, u64)> = tasks.iter().map(|t| ("Ana", t.as_str(), 1)).collect();
        let below = &full[..cap - 1];
        for (merge, hit) in [(&full[..], 1), (below, 0)] {
            let mut m = Fake::new(16_384, 1.0);
            for r in [
                part_reply(&[("Ana", "Send the minutes", 1)], None),
                part_reply(&[], Some(1)),
                part_reply(&[("Ben", "Draw it", 12)], None),
                part_reply(&[], Some(11)),
                // The merge's reply: at the list cap, or one below it.
                part_reply(merge, Some(11)),
            ] {
                m.script.push_back(r);
            }
            let run = summarize_chunks_two_call(&mut m, &input(30), &options(), &[0..10, 10..30]);
            let s = run.result.unwrap();
            assert_eq!(s.action_items.len(), merge.len());
            assert_eq!(run.report.reduce_steps, 1);
            // A merge at the cap may have lost actions to the cap, not to its judgement.
            assert_eq!(run.actions.cap_hit, hit, "{:?}", run.actions);
        }
    }

    #[test]
    fn summarize_two_call_one_chunk_is_its_two_requests_without_a_merge() {
        let mut m = Fake::new(16_384, 1.0);
        let inp = input(30);
        m.script
            .push_back(part_reply(&[("Ana", "Send the minutes", 3)], None));
        m.script.push_back(part_reply(&[], Some(2)));
        let whole = 0..30;
        let run = summarize_chunks_two_call(&mut m, &inp, &options(), std::slice::from_ref(&whole));
        let s = run.result.unwrap();
        assert_eq!(
            (
                run.report.map_parts,
                run.report.reduce_steps,
                run.report.requests
            ),
            (1, 0, 2)
        );
        // The extracted actions and everything else of the rest request.
        assert_eq!(s.action_items.len(), 1);
        assert_eq!(s.action_items[0].source_segment_ids, [id(3)]);
        assert_eq!(s.decisions[0].source_segment_ids, [id(2)]);
        assert_eq!(run.actions.final_count, Some(1));
        assert_eq!(run.actions.to_merge, 1);
    }

    #[test]
    fn summarize_two_call_a_failed_step_reports_no_final_count() {
        let mut m = Fake::new(16_384, 1.0);
        for _ in 0..=MAX_REPAIRS {
            m.script
                .push_back(Some(("not json".into(), Stop::EndOfGeneration)));
        }
        let run = summarize_chunks_two_call(&mut m, &input(30), &options(), &[0..10, 10..30]);
        assert!(
            matches!(
                run.result,
                Err(SummaryFailure::Invalid {
                    step: "map-actions",
                    ..
                })
            ),
            "{:?}",
            run.result
        );
        assert_eq!(run.actions.final_count, None);
        // Production chunked runs still ask every request for the whole summary.
        let mut m = Fake::new(16_384, 1.0);
        summarize_chunks(&mut m, &input(30), &options(), &[0..10, 10..30])
            .result
            .unwrap();
        assert_eq!(
            m.calls[0].2,
            crate::grammar::summary_grammar(&(0..10).into(), SummaryLimits::default()).unwrap()
        );
    }

    #[test]
    fn summarize_chunks_a_chunk_too_long_for_the_context_fails_without_requests() {
        let mut m = Fake::new(4096, 1.0);
        let run = summarize_chunks(&mut m, &input(300), &options(), &[0..1, 1..300]);
        assert!(
            matches!(run.result, Err(SummaryFailure::Plan(_))),
            "{:?}",
            run.result
        );
        assert_eq!(run.report.requests, 0);
    }

    #[test]
    fn summarize_plan_shrinks_until_the_model_tokenizer_fits() {
        // The model needs 1.6x the estimate: the first plan overflows, a re-plan fits.
        let mut m = Fake::new(8192, 1.6);
        let run = summarize(&mut m, &input(300), &options());
        run.result.unwrap();
        assert!(run.report.replans >= 1, "{:?}", run.report);
        assert!(run.report.planner_window < 8192);
        assert!(run.report.max_prompt_tokens + run.report.max_reply_budget <= 8192);
        // Without the overshoot nothing is re-planned.
        let mut exact = Fake::new(8192, 1.0);
        assert_eq!(
            summarize(&mut exact, &input(300), &options())
                .report
                .replans,
            0
        );
    }

    #[test]
    fn summarize_a_cut_off_reply_is_asked_again_with_more_room_not_for_less() {
        let mut m = Fake::new(16_384, 1.0);
        m.script
            .push_back(Some(("{\"title\":".into(), Stop::MaxTokens)));
        m.script
            .push_back(Some(("{\"title\":\"longer".into(), Stop::MaxTokens)));
        let roomy = SummaryOptions {
            max_output_tokens: 8192,
            ..options()
        };
        let run = summarize(&mut m, &input(5), &roomy);
        run.result.unwrap();
        assert_eq!(run.report.requests, 3);
        assert_eq!(run.report.truncated, 2);
        assert_eq!(run.report.repairs, 2);
        // The same request each time, with twice the budget up to the ceiling.
        let first = m.calls[0].3;
        assert_eq!(m.calls[1].3, first * 2);
        assert_eq!(m.calls[2].3, first * 4);
        assert_eq!(run.report.max_reply_budget, m.calls[2].3);
        for call in &m.calls[1..] {
            assert_eq!(call.0, m.calls[0].0, "no echo and no request to shorten");
        }
    }

    #[test]
    fn summarize_a_merge_with_no_valid_reply_joins_the_parts_and_says_so() {
        let inp = input(300);
        let mut probe = Fake::new(4096, 1.0);
        let parts = {
            let run = summarize(&mut probe, &inp, &options());
            run.result.unwrap();
            assert!(run.report.reduce_steps >= 1);
            assert_eq!(run.report.joined_parts, 0);
            run.report.map_parts
        };
        let mut m = Fake::new(4096, 1.0);
        for _ in 0..parts {
            m.script.push_back(None);
        }
        for _ in 0..=MAX_REPAIRS {
            m.script
                .push_back(Some(("{\"title\":\"x x x".into(), Stop::Looping)));
        }
        let run = summarize(&mut m, &inp, &options());
        let s = run.result.unwrap();
        assert!(
            s.tldr
                .starts_with(earshot_core::summary::JOINED_SUMMARY_NOTICE)
        );
        assert_eq!(run.report.joined_parts, parts);
        assert_eq!(run.report.requests, parts + MAX_REPAIRS + 1);
        // This fake gives every part the same decision text, so the exact repeats go. That
        // distinct parts all survive is pinned in `summary_join_...` and the remote client's
        // `a_merge_with_no_valid_reply_joins_the_parts_and_says_so`.
        assert_eq!(s.decisions.len(), 1);
    }

    #[test]
    fn summarize_a_map_step_with_no_valid_reply_still_fails() {
        // A part that failed is not known to be anything: nothing is joined over a gap.
        let inp = input(300);
        let mut m = Fake::new(4096, 1.0);
        for _ in 0..=MAX_REPAIRS {
            m.script
                .push_back(Some(("{\"title\":\"x x x".into(), Stop::Looping)));
        }
        let run = summarize(&mut m, &inp, &options());
        let Err(SummaryFailure::Invalid { step, .. }) = run.result else {
            panic!("{:?}", run.result);
        };
        assert_eq!(step, "map");
        assert_eq!(run.report.joined_parts, 0);
    }

    #[test]
    fn summarize_cancelled_during_a_merge_is_not_joined() {
        let inp = input(300);
        let mut probe = Fake::new(4096, 1.0);
        let parts = summarize(&mut probe, &inp, &options()).report.map_parts;
        let cancel = CancelToken::new();
        let mut m = Fake::new(4096, 1.0);
        m.cancel_during = Some((parts, cancel.clone()));
        let run = summarize(
            &mut m,
            &inp,
            &SummaryOptions {
                cancel,
                ..options()
            },
        );
        assert!(
            matches!(run.result, Err(SummaryFailure::Cancelled)),
            "{:?}",
            run.result
        );
        assert_eq!(run.report.joined_parts, 0);
    }

    #[test]
    fn summarize_a_cut_off_reply_is_never_accepted_however_often_it_grows() {
        let mut m = Fake::new(16_384, 1.0);
        for _ in 0..=MAX_REPAIRS {
            m.script
                .push_back(Some(("{\"title\":".into(), Stop::MaxTokens)));
        }
        let run = summarize(&mut m, &input(5), &options());
        let Err(SummaryFailure::Invalid { attempts, .. }) = run.result else {
            panic!("{:?}", run.result);
        };
        assert_eq!(attempts, MAX_REPAIRS + 1);
        assert_eq!(m.calls.len(), MAX_REPAIRS + 1);
    }

    #[test]
    fn summarize_repairs_invalid_and_truncated_replies_then_gives_up() {
        let mut m = Fake::new(16_384, 1.0);
        m.script
            .push_back(Some(("{\"title\":".into(), Stop::MaxTokens)));
        m.script
            .push_back(Some(("{}".into(), Stop::EndOfGeneration)));
        // At the ceiling the budget cannot grow, so the repair asks for less.
        let at_ceiling = SummaryOptions {
            max_output_tokens: earshot_core::prompt::REPLY_MIN_TOKENS,
            ..options()
        };
        let run = summarize(&mut m, &input(5), &at_ceiling);
        run.result.unwrap();
        assert_eq!(run.report.requests, 3);
        assert_eq!(run.report.repairs, 2);
        assert_eq!(run.report.truncated, 1);
        assert_eq!(run.report.invalid_replies, 2);
        // The repair round sends the original messages, the reply and the errors.
        let (msgs, _, _, _) = &m.calls[1];
        assert_eq!(msgs.len(), 4);
        assert_eq!(msgs[2].role, Role::Assistant);
        assert!(msgs[3].content.contains("cut off"));

        let mut bad = Fake::new(16_384, 1.0);
        for _ in 0..MAX_REPAIRS {
            bad.script
                .push_back(Some(("{}".into(), Stop::EndOfGeneration)));
        }
        let long = format!("{{\"title\":\"{}", "ab".repeat(400));
        bad.script.push_back(Some((long.clone(), Stop::MaxTokens)));
        let run = summarize(&mut bad, &input(5), &options());
        let Err(SummaryFailure::Invalid {
            step,
            attempts,
            last_reply_tail,
            ..
        }) = run.result
        else {
            panic!("{:?}", run.result);
        };
        assert_eq!((step, attempts), ("single", MAX_REPAIRS + 1));
        // The failure keeps the end of the last reply, cut to REPLY_TAIL_CHARS.
        assert_eq!(last_reply_tail.chars().count(), REPLY_TAIL_CHARS);
        assert!(long.ends_with(&last_reply_tail));
    }

    #[test]
    fn summarize_a_looping_reply_is_repetition_not_truncation() {
        let mut m = Fake::new(16_384, 1.0);
        m.script.push_back(Some((
            "{\"title\":\"T\",\"tldr\":\"x x x".into(),
            Stop::Looping,
        )));
        let run = summarize(&mut m, &input(5), &options());
        run.result.unwrap();
        assert_eq!((run.report.looping, run.report.truncated), (1, 0));
        let repair = &m.calls[1].0.last().unwrap().content;
        assert!(repair.contains("repeats"), "{repair}");
        assert!(!repair.contains("cut off"), "{repair}");
    }

    #[test]
    fn summarize_rejects_repeated_items_and_out_of_range_numbers() {
        let mut m = Fake::new(16_384, 1.0);
        // Three copies of one bullet (`REPEAT_LIMIT`) are a looping reply; two are dropped.
        let repeated = r#"{"title":"T","tldr":"x","enhanced_notes":[{"heading":"H","bullets":[{"text":"Same","source_segment_ids":[1]},{"text":"same.","source_segment_ids":[2]},{"text":"SAME","source_segment_ids":[3]}]}],"decisions":[{"text":"d","source_segment_ids":[9]}],"action_items":[],"open_questions":[],"topics":[]}"#;
        m.script
            .push_back(Some((repeated.into(), Stop::EndOfGeneration)));
        let run = summarize(&mut m, &input(5), &options());
        run.result.unwrap();
        let repair = &m.calls[1].0.last().unwrap().content;
        assert!(repair.contains("repeats an earlier item"), "{repair}");
        assert!(repair.contains("segment 9 is not one of"), "{repair}");
    }

    #[test]
    fn summarize_passes_the_assistant_prefix_and_counts_it() {
        let mut m = Fake::new(16_384, 1.0);
        let mut o = options();
        o.assistant_prefix = "<think>\n\n</think>\n\n".into();
        summarize(&mut m, &input(3), &o).result.unwrap();
        assert_eq!(m.calls[0].1, o.assistant_prefix);
    }

    #[test]
    fn summarize_context_too_small_is_a_plan_failure() {
        let mut m = Fake::new(1200, 1.0);
        let run = summarize(&mut m, &input(3), &options());
        assert!(
            matches!(run.result, Err(SummaryFailure::Plan(_))),
            "{:?}",
            run.result
        );
        assert_eq!(run.report.requests, 0);
    }

    #[test]
    fn summarize_cancelled_before_the_first_request_makes_none() {
        let mut m = Fake::new(16_384, 1.0);
        let o = options();
        o.cancel.cancel();
        let run = summarize(&mut m, &input(5), &o);
        assert_eq!(run.result, Err(SummaryFailure::Cancelled));
        assert_eq!(run.report.requests, 0);
        assert!(m.calls.is_empty());
        let run = summarize_chunks(&mut m, &input(30), &o, &[0..10, 10..30]);
        assert_eq!(run.result, Err(SummaryFailure::Cancelled));
        assert!(m.calls.is_empty());
    }

    #[test]
    fn summarize_cancelled_during_a_map_request_stops_the_run() {
        // Cancelled while the second map request generates: that request ends with the
        // model's Cancelled, no later map or reduce request starts (the fake asserts it).
        let o = options();
        let mut m = Fake::new(16_384, 1.0);
        m.cancel_during = Some((1, o.cancel.clone()));
        let run = summarize_chunks(&mut m, &input(30), &o, &[0..10, 10..25, 25..30]);
        assert_eq!(run.result, Err(SummaryFailure::Cancelled));
        assert_eq!(m.calls.len(), 2);
        assert_eq!(run.report.requests, 1);
        assert_eq!(run.report.steps.len(), 2);
        assert!(SummaryFailure::Cancelled.to_string().contains("cancelled"));
    }

    #[test]
    fn summarize_cancelled_between_repair_rounds_makes_no_repair_request() {
        // The first reply is invalid; the token is cancelled before the repair round.
        struct CancelAfterReply(Fake, CancelToken);
        impl Generate for CancelAfterReply {
            fn context_tokens(&self) -> usize {
                self.0.context_tokens()
            }
            fn prompt_tokens(&self, m: &[ChatMessage], p: &str) -> Result<usize, LlmError> {
                self.0.prompt_tokens(m, p)
            }
            fn generate(
                &mut self,
                m: &[ChatMessage],
                p: &str,
                g: &str,
                n: usize,
                cancel: &CancelToken,
            ) -> Result<Generation, LlmError> {
                let out = self.0.generate(m, p, g, n, cancel);
                self.1.cancel();
                out
            }
        }
        let o = options();
        let mut fake = Fake::new(16_384, 1.0);
        fake.script
            .push_back(Some(("{}".into(), Stop::EndOfGeneration)));
        let mut m = CancelAfterReply(fake, o.cancel.clone());
        let run = summarize(&mut m, &input(5), &o);
        assert_eq!(run.result, Err(SummaryFailure::Cancelled));
        assert_eq!(m.0.calls.len(), 1);
        assert_eq!(run.report.requests, 1);
    }
}
