//! Chat messages for a summary request, single pass or map-reduce.

use core::fmt;
use core::fmt::Write as _;
use core::ops::Range;
use std::collections::BTreeSet;
use std::sync::OnceLock;

use serde::{Deserialize, Serialize};

use super::style::{StylePreset, SummaryStyle, style_block};
use super::template::{
    MAP, PromptVariant, REDUCE, SummaryRules, TWO_CALL_MAP_ACTIONS, TWO_CALL_MAP_REST,
    TWO_CALL_REDUCE_MERGE, Template, USER_NAME,
};
use super::tokens::{TOKENS_PER_MESSAGE, TOKENS_PER_REQUEST, estimate_tokens};
use super::vocabulary::Vocabulary;
use crate::asr::{MeetingTime, Segment};
use crate::identity::EntityId;
use crate::summary::{
    Citable, SegmentNumbers, SummaryError, SummaryErrorKind, SummaryV1, llm_json_schema,
    to_numbered_json,
};

/// Who a chat message is from (OpenAI chat completions roles).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    /// Instructions.
    System,
    /// The request.
    User,
    /// A model reply (fed back in repair rounds).
    Assistant,
}

/// One chat message, serialised as `{"role": ..., "content": ...}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChatMessage {
    /// Who it is from.
    pub role: Role,
    /// The text.
    pub content: String,
}

impl ChatMessage {
    /// A message from `role`.
    pub fn new(role: Role, content: impl Into<String>) -> Self {
        ChatMessage {
            role,
            content: content.into(),
        }
    }
}

/// Estimated prompt tokens of a request with `messages` (see [`estimate_tokens`]).
#[must_use]
pub fn estimate_messages(messages: &[ChatMessage]) -> usize {
    TOKENS_PER_REQUEST
        + messages
            .iter()
            .map(|m| TOKENS_PER_MESSAGE + estimate_tokens(&m.content))
            .sum::<usize>()
}

/// A transcript segment as the model sees it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PromptSegment {
    /// Segment id, the value the model must cite.
    pub id: EntityId,
    /// Start of the segment.
    pub start: MeetingTime,
    /// Display name of the speaker (empty when unknown).
    pub speaker: String,
    /// What was said.
    pub text: String,
}

impl PromptSegment {
    /// The prompt form of `segment`, spoken by `speaker` (its display name after
    /// diarisation, or "Me" / "Them" for an undiarised two-track meeting).
    #[must_use]
    pub fn from_segment(segment: &Segment, speaker: &str) -> Self {
        PromptSegment {
            id: segment.id,
            start: segment.start,
            speaker: speaker.to_string(),
            text: segment.text.clone(),
        }
    }
}

/// A note the user typed, anchored to the meeting timeline (PLAN.md 3.5).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserNote {
    /// When in the meeting the note was written.
    pub at: MeetingTime,
    /// The note (may span several lines).
    pub text: String,
}

/// Meeting details given to the model.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct MeetingInfo {
    /// The title the user gave the meeting, if any.
    pub title: Option<String>,
    /// Local start date and time as the UI formats it (for example `2026-10-17 14:00`).
    /// Core has no clock or time zone; the caller formats it.
    pub started: Option<String>,
    /// Length of the meeting in milliseconds.
    pub duration_ms: u64,
    /// Display names of the attendees.
    pub attendees: Vec<String>,
}

/// Everything a summary is built from (PLAN.md 3.7).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct SummaryInput {
    /// Meeting details.
    pub meeting: MeetingInfo,
    /// The transcript in meeting order.
    pub segments: Vec<PromptSegment>,
    /// The user's notes in meeting order.
    pub notes: Vec<UserNote>,
    /// Which template to follow.
    pub template: Template,
    /// Which shared rules to use: [`PromptVariant::Current`] (the production prompt)
    /// unless an evaluation selects another (T6.20b). Left out of the JSON when current,
    /// so serialised inputs from before T6.20b read and write unchanged.
    #[serde(default, skip_serializing_if = "PromptVariant::is_current")]
    pub prompt_variant: PromptVariant,
    /// The writer's shared rules (T12.17c), used when [`SummaryInput::prompt_variant`] is
    /// current. Set only by the writer (`earshot_llm::LlmSummarizer` from
    /// `LlmClient::summary_rules`); everything else builds inputs with the standard rules.
    /// Left out of the JSON when standard, so serialised inputs from before T12.17c read and
    /// write unchanged.
    #[serde(default, skip_serializing_if = "SummaryRules::is_standard")]
    pub rules: SummaryRules,
    /// The user's summary style (T7.4b): appended after the fixed rules of the request that
    /// writes the summary the user sees (the single request, the last merge), never in a
    /// map request. Empty (the default) adds nothing. Left out of the JSON when empty, so
    /// serialised inputs from before T7.4b read and write unchanged.
    #[serde(default, skip_serializing_if = "SummaryStyle::is_empty")]
    pub style: SummaryStyle,
    /// The summary template the user picked (T12.17): its fixed text goes in the style block
    /// before [`SummaryInput::style`], in the same requests. Standard (the default) adds
    /// nothing and is left out of the JSON, so serialised inputs from before T12.17 read and
    /// write unchanged.
    #[serde(default, skip_serializing_if = "StylePreset::is_standard")]
    pub style_preset: StylePreset,
    /// The user's own words (T12.15a): spelling help for names and acronyms, in every
    /// request (single, map, every merge). Empty (the default) adds nothing. Left out of the
    /// JSON when empty, so serialised inputs from before T12.15a read and write unchanged.
    #[serde(default, skip_serializing_if = "Vocabulary::is_empty")]
    pub vocabulary: Vocabulary,
    /// How the user appears in the transcript, when they gave a name: every request says so
    /// (T9.1h). `None` adds nothing, and is left out of the JSON.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_name: Option<String>,
}

impl SummaryInput {
    /// The ids the summary may cite (for [`crate::summary::validate_value`]).
    #[must_use]
    pub fn segment_ids(&self) -> BTreeSet<EntityId> {
        self.segments.iter().map(|s| s.id).collect()
    }

    /// The shared rules every request of this input starts with when `writer` writes it:
    /// the prompt variant's rules file, which for [`PromptVariant::Current`] is `writer`'s
    /// ([`SummaryInput::rules`] unless the caller names a writer's, as the `*_with_rules`
    /// functions do).
    fn rules_text(&self, writer: SummaryRules) -> &'static str {
        self.prompt_variant.rules_for(writer)
    }
}

/// The model's context window and the ceiling of one reply.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextBudget {
    /// Context window of the configured model, in tokens.
    pub context_window: usize,
    /// The most tokens one reply may get. Each request's own budget
    /// ([`Prompt::max_output_tokens`], sent as `max_tokens`) is sized to what it summarises
    /// ([`reply_tokens`]) and never exceeds this.
    pub max_output_tokens: usize,
}

/// Upper bound on a [`repair_message`] as sent (its estimate plus
/// [`TOKENS_PER_MESSAGE`]). Planned prompts leave this much of the context free beside
/// their reply budget, so a repair round (the original messages plus the repair message)
/// always fits.
pub const REPAIR_MAX_TOKENS: usize = 512;

/// Longest error line (in characters, ellipsis included) a repair message quotes; longer
/// ones are cut.
pub(super) const REPAIR_LINE_MAX_CHARS: usize = 160;

// The ratios are generous on purpose: the reply budget must never decide how long a
// summary is (the loop guard and the repetition check stop runaway replies). The first
// values (1/3 of the transcript, 1/2 of the partials, at least 512) truncated legitimate
// merge replies on the AMI dev split (T6.20, docs/summary-eval.md); these were set on dev
// and never on test.

/// The smallest reply budget of any request.
pub const REPLY_MIN_TOKENS: usize = 1024;

/// Reply tokens per estimated token of transcript a request summarises (one in two).
pub const REPLY_PER_TRANSCRIPT_TOKEN: (usize, usize) = (1, 2);

/// Reply tokens per estimated token of partial summaries a merge request combines (one
/// for one: a merge drops duplicates but keeps every decision and action item, and its
/// reply is about as long as its input).
pub const REPLY_PER_PARTIAL_TOKEN: (usize, usize) = (1, 1);

impl ContextBudget {
    /// Tokens left for the prompt beside the largest reply.
    #[must_use]
    pub fn prompt_tokens(&self) -> usize {
        self.context_window.saturating_sub(self.max_output_tokens)
    }

    /// [`ContextBudget::prompt_tokens`] minus [`REPAIR_MAX_TOKENS`]: what a request may use
    /// when it gets the largest reply.
    #[must_use]
    pub fn request_tokens(&self) -> usize {
        self.prompt_tokens().saturating_sub(REPAIR_MAX_TOKENS)
    }

    /// Whether a request of `prompt` estimated tokens with a reply budget of `reply` fits,
    /// leaving [`REPAIR_MAX_TOKENS`] free.
    #[must_use]
    pub fn fits(&self, prompt: usize, reply: usize) -> bool {
        prompt + reply + REPAIR_MAX_TOKENS <= self.context_window
    }
}

/// What a request summarises, which sizes its reply budget.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReplyBasis {
    /// Estimated tokens of transcript lines.
    Transcript(usize),
    /// Estimated tokens of partial summaries.
    Partials(usize),
}

/// The reply budget of a request: [`REPLY_PER_TRANSCRIPT_TOKEN`] or
/// [`REPLY_PER_PARTIAL_TOKEN`] of what it summarises, at least [`REPLY_MIN_TOKENS`] and at
/// most `budget.max_output_tokens`. The length of a summary follows its content; the
/// ceiling is only what the context allows.
#[must_use]
pub fn reply_tokens(basis: ReplyBasis, budget: ContextBudget) -> usize {
    let (content, (num, den)) = match basis {
        ReplyBasis::Transcript(t) => (t, REPLY_PER_TRANSCRIPT_TOKEN),
        ReplyBasis::Partials(p) => (p, REPLY_PER_PARTIAL_TOKEN),
    };
    (content * num)
        .div_ceil(den)
        .max(REPLY_MIN_TOKENS)
        .min(budget.max_output_tokens)
}

/// The reply budget to ask again with after a reply was cut off at `current`: twice as much,
/// at most `budget.max_output_tokens` and what a prompt of `prompt_tokens` leaves free beside
/// [`REPAIR_MAX_TOKENS`]. `None` when it cannot grow, and only then is the model asked for
/// something shorter. The reply budget must never decide how long a summary is (T8.4d: greedy
/// decoding repeats a cut-off reply, so a repair at the same budget cannot help).
#[must_use]
pub fn grown_reply_tokens(
    current: usize,
    prompt_tokens: usize,
    budget: ContextBudget,
) -> Option<usize> {
    let free = budget
        .context_window
        .saturating_sub(prompt_tokens + REPAIR_MAX_TOKENS);
    let grown = current
        .saturating_mul(2)
        .min(budget.max_output_tokens)
        .min(free);
    (grown > current).then_some(grown)
}

/// Why no prompt could be built.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PromptError {
    /// The fixed part of a prompt (instructions, schema, meeting details, notes) or a
    /// single transcript segment or partial summary, with its reply budget and the repair
    /// headroom, does not fit in the context window.
    ContextTooSmall {
        /// Estimated tokens needed (prompt, reply budget and repair headroom).
        needed: usize,
        /// The context window.
        available: usize,
    },
}

impl fmt::Display for PromptError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PromptError::ContextTooSmall { needed, available } => write!(
                f,
                "the model's context window is too small: about {needed} tokens needed, {available} available"
            ),
        }
    }
}

impl std::error::Error for PromptError {}

/// A ready request: messages, their estimated size, its reply budget and the segments its
/// reply may cite.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Prompt {
    /// Messages to send.
    pub messages: Vec<ChatMessage>,
    /// [`estimate_messages`] of `messages`.
    pub estimated_tokens: usize,
    /// This request's reply budget ([`reply_tokens`]), sent as `max_tokens`.
    pub max_output_tokens: usize,
    /// The transcript segments (indices into [`SummaryInput::segments`]; segment number
    /// `i + 1`) the reply may cite, for [`crate::summary::validate_reply`].
    pub cites: Citable,
}

impl Prompt {
    pub(super) fn new(
        messages: Vec<ChatMessage>,
        max_output_tokens: usize,
        cites: Citable,
    ) -> Self {
        Prompt {
            estimated_tokens: estimate_messages(&messages),
            messages,
            max_output_tokens,
            cites,
        }
    }
}

/// One part of a map-reduce run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MapChunk {
    /// 1-based part number.
    pub part: usize,
    /// Number of parts.
    pub parts: usize,
    /// The transcript segments of this part (indices into [`SummaryInput::segments`]).
    pub segments: Range<usize>,
    /// The request for this part. Its summary may cite only these segments.
    pub prompt: Prompt,
}

/// How to summarise an input within a [`ContextBudget`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PromptPlan {
    /// Everything fits in one request.
    Single(Prompt),
    /// Summarise each chunk ("map"), then merge the partial summaries with
    /// [`reduce_groups`] and [`reduce_prompt`] ("reduce").
    MapReduce(Vec<MapChunk>),
}

/// The segment numbering of `input` (segment `i` is number `i + 1`).
#[must_use]
pub fn segment_numbers(input: &SummaryInput) -> SegmentNumbers {
    SegmentNumbers::new(input.segments.iter().map(|s| s.id))
}

/// Plans the requests for `input`: one request when the whole transcript fits in
/// `budget`, otherwise consecutive transcript chunks filled greedily. Every request gets
/// the reply budget [`reply_tokens`] of the transcript it summarises, and every prompt in
/// the plan satisfies [`ContextBudget::fits`] with that budget (leaving
/// [`REPAIR_MAX_TOKENS`] for a repair round).
pub fn plan(input: &SummaryInput, budget: ContextBudget) -> Result<PromptPlan, PromptError> {
    plan_with_rules(input, input.rules, budget)
}

/// [`plan`] with the shared rules of `rules` in place of [`SummaryInput::rules`]: a writer
/// with its own rules (T12.17c, `earshot_llm::LlmSummarizer`) plans with them without
/// copying the input.
///
/// # Errors
/// As [`plan`].
pub fn plan_with_rules(
    input: &SummaryInput,
    rules: SummaryRules,
    budget: ContextBudget,
) -> Result<PromptPlan, PromptError> {
    let numbered: Vec<String> = input
        .segments
        .iter()
        .enumerate()
        .map(|(i, s)| segment_line(i + 1, s))
        .collect();
    let line_costs: Vec<usize> = numbered.iter().map(|l| estimate_tokens(l)).collect();
    let transcript: usize = line_costs.iter().sum();
    let all = 0..input.segments.len();
    let single = Prompt::new(
        vec![
            ChatMessage::new(
                Role::System,
                system_text(
                    input.template,
                    input.rules_text(rules),
                    input.user_name.as_deref(),
                    None,
                    &input.vocabulary,
                    Some((input.style_preset, &input.style)),
                ),
            ),
            ChatMessage::new(Role::User, user_text(input, &numbered, &input.notes, None)),
        ],
        reply_tokens(ReplyBasis::Transcript(transcript), budget),
        all.into(),
    );
    if budget.fits(single.estimated_tokens, single.max_output_tokens) {
        return Ok(PromptPlan::Single(single));
    }

    // Upper bound of everything but the segment lines in any chunk: the map instruction
    // with six-digit part numbers, the meeting details, the part header and every note.
    // Estimates are subadditive, so a sum of parts bounds the joined message.
    let map_extra = map_text(999_999, 999_999);
    let fixed = TOKENS_PER_REQUEST
        + 2 * TOKENS_PER_MESSAGE
        + estimate_tokens(&system_text(
            input.template,
            input.rules_text(rules),
            input.user_name.as_deref(),
            Some(&map_extra),
            &input.vocabulary,
            None,
        ))
        + estimate_tokens(&user_text(
            input,
            &[],
            &input.notes,
            Some((999_999, 999_999)),
        ));
    let reply = |content: usize| reply_tokens(ReplyBasis::Transcript(content), budget);
    if !budget.fits(fixed, reply(0)) {
        return Err(PromptError::ContextTooSmall {
            needed: fixed + reply(0) + REPAIR_MAX_TOKENS,
            available: budget.context_window,
        });
    }
    let mut chunks: Vec<Range<usize>> = Vec::new();
    let mut start = 0;
    let mut content = 0;
    for (i, &cost) in line_costs.iter().enumerate() {
        if !budget.fits(fixed + cost, reply(cost)) {
            return Err(PromptError::ContextTooSmall {
                needed: fixed + cost + reply(cost) + REPAIR_MAX_TOKENS,
                available: budget.context_window,
            });
        }
        if !budget.fits(fixed + content + cost, reply(content + cost)) {
            chunks.push(start..i);
            start = i;
            content = 0;
        }
        content += cost;
    }
    chunks.push(start..input.segments.len());
    Ok(PromptPlan::MapReduce(map_chunks(
        input,
        rules,
        budget,
        &numbered,
        &line_costs,
        &chunks,
        MapAsk::Whole,
    )))
}

/// What a map request asks for: the whole partial summary (production), or one of the two
/// requests of a two-call part (T6.20c, evaluation only).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MapAsk {
    /// Everything ([`MAP`] alone).
    Whole,
    /// Only the action items ([`MAP`] plus [`TWO_CALL_MAP_ACTIONS`]).
    Actions,
    /// Everything but the action items ([`MAP`] plus [`TWO_CALL_MAP_REST`]).
    Rest,
}

impl MapAsk {
    /// The extra system text of part `part` of `parts`.
    fn text(self, part: usize, parts: usize) -> String {
        let map = map_text(part, parts);
        match self {
            MapAsk::Whole => map,
            MapAsk::Actions => format!("{map}{TWO_CALL_MAP_ACTIONS}"),
            MapAsk::Rest => format!("{map}{TWO_CALL_MAP_REST}"),
        }
    }
}

/// The map requests of `chunks` (the requests [`plan`] builds for its own chunks when `ask`
/// is [`MapAsk::Whole`]).
fn map_chunks(
    input: &SummaryInput,
    rules: SummaryRules,
    budget: ContextBudget,
    numbered: &[String],
    line_costs: &[usize],
    chunks: &[Range<usize>],
    ask: MapAsk,
) -> Vec<MapChunk> {
    let parts = chunks.len();
    chunks
        .iter()
        .enumerate()
        .map(|(i, range)| {
            let part = i + 1;
            let notes = notes_for_chunk(input, chunks, i);
            let content: usize = line_costs[range.clone()].iter().sum();
            let prompt = Prompt::new(
                vec![
                    ChatMessage::new(
                        Role::System,
                        system_text(
                            input.template,
                            input.rules_text(rules),
                            input.user_name.as_deref(),
                            Some(&ask.text(part, parts)),
                            &input.vocabulary,
                            None,
                        ),
                    ),
                    ChatMessage::new(
                        Role::User,
                        user_text(input, &numbered[range.clone()], &notes, Some((part, parts))),
                    ),
                ],
                reply_tokens(ReplyBasis::Transcript(content), budget),
                range.clone().into(),
            );
            MapChunk {
                part,
                parts,
                segments: range.clone(),
                prompt,
            }
        })
        .collect()
}

/// Map requests over chunks the caller chose (`ranges`: consecutive, non-empty, in order,
/// together every segment of `input`), built exactly as [`plan`] builds the map requests
/// of its own chunks; the partial summaries are then merged with [`reduce_groups`] and
/// [`reduce_prompt`] as usual. For the forced-chunking evaluation (T6.20: summaries made
/// during the meeting, a map request every few minutes of transcript).
///
/// # Errors
/// [`PromptError::ContextTooSmall`] when a chunk's request with its reply budget and
/// [`REPAIR_MAX_TOKENS`] does not fit `budget`.
///
/// # Panics
/// When `ranges` are not consecutive non-empty ranges covering every segment.
pub fn plan_chunks(
    input: &SummaryInput,
    budget: ContextBudget,
    ranges: &[Range<usize>],
) -> Result<Vec<MapChunk>, PromptError> {
    plan_chunks_with_rules(input, input.rules, budget, ranges)
}

/// [`plan_chunks`] with the shared rules of `rules` in place of [`SummaryInput::rules`]
/// (as [`plan_with_rules`]).
///
/// # Errors
/// As [`plan_chunks`].
///
/// # Panics
/// As [`plan_chunks`].
pub fn plan_chunks_with_rules(
    input: &SummaryInput,
    rules: SummaryRules,
    budget: ContextBudget,
    ranges: &[Range<usize>],
) -> Result<Vec<MapChunk>, PromptError> {
    plan_chunks_asking(input, rules, budget, ranges, MapAsk::Whole)
}

/// One part of a two-call chunked run (T6.20c, evaluation only): two requests over the same
/// transcript lines, notes, numbering, citable segments and reply budget as the
/// [`plan_chunks`] map request of the part.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TwoCallChunk {
    /// 1-based part number.
    pub part: usize,
    /// Number of parts.
    pub parts: usize,
    /// The transcript segments of this part (indices into [`SummaryInput::segments`]).
    pub segments: Range<usize>,
    /// Extracts only the part's action items; the rest of its reply is left empty.
    pub actions: Prompt,
    /// Summarises everything but the action items (`action_items` left empty).
    pub rest: Prompt,
}

/// The two-call requests over chunks the caller chose (T6.20c, evaluation only;
/// docs/summary-eval.md): per part, the [`plan_chunks`] map request with one more paragraph
/// in its system message, once asking only for the action items and once for everything
/// else. The partial summaries (the rest with the extracted action items) are merged with
/// [`reduce_groups_two_call`] and [`reduce_prompt_two_call`]. Production never uses it.
///
/// # Errors
/// As [`plan_chunks`], for either request of a part.
///
/// # Panics
/// As [`plan_chunks`].
pub fn plan_chunks_two_call(
    input: &SummaryInput,
    budget: ContextBudget,
    ranges: &[Range<usize>],
) -> Result<Vec<TwoCallChunk>, PromptError> {
    let actions = plan_chunks_asking(input, input.rules, budget, ranges, MapAsk::Actions)?;
    let rest = plan_chunks_asking(input, input.rules, budget, ranges, MapAsk::Rest)?;
    Ok(actions
        .into_iter()
        .zip(rest)
        .map(|(a, r)| TwoCallChunk {
            part: a.part,
            parts: a.parts,
            segments: a.segments,
            actions: a.prompt,
            rest: r.prompt,
        })
        .collect())
}

fn plan_chunks_asking(
    input: &SummaryInput,
    rules: SummaryRules,
    budget: ContextBudget,
    ranges: &[Range<usize>],
    ask: MapAsk,
) -> Result<Vec<MapChunk>, PromptError> {
    let mut at = 0;
    for r in ranges {
        assert!(
            r.start == at && r.end > r.start,
            "chunk {r:?} does not follow segment {at}"
        );
        at = r.end;
    }
    assert_eq!(at, input.segments.len(), "the chunks miss segments");
    let numbered: Vec<String> = input
        .segments
        .iter()
        .enumerate()
        .map(|(i, s)| segment_line(i + 1, s))
        .collect();
    let line_costs: Vec<usize> = numbered.iter().map(|l| estimate_tokens(l)).collect();
    let chunks = map_chunks(input, rules, budget, &numbered, &line_costs, ranges, ask);
    for c in &chunks {
        if !budget.fits(c.prompt.estimated_tokens, c.prompt.max_output_tokens) {
            return Err(PromptError::ContextTooSmall {
                needed: c.prompt.estimated_tokens + c.prompt.max_output_tokens + REPAIR_MAX_TOKENS,
                available: budget.context_window,
            });
        }
    }
    Ok(chunks)
}

/// Notes for chunk `i`: those written from the chunk's first segment up to the next
/// chunk's first segment; notes before the first segment go to the first chunk and notes
/// after the last chunk's start to the last.
fn notes_for_chunk(input: &SummaryInput, chunks: &[Range<usize>], i: usize) -> Vec<UserNote> {
    let from = if i == 0 {
        None
    } else {
        Some(input.segments[chunks[i].start].start)
    };
    let to = chunks
        .get(i + 1)
        .map(|next| input.segments[next.start].start);
    input
        .notes
        .iter()
        .filter(|n| from.is_none_or(|f| n.at >= f) && to.is_none_or(|t| n.at < t))
        .cloned()
        .collect()
}

/// Splits `partials` (in meeting order) into consecutive groups whose
/// [`reduce_prompt`] fits in `budget` with its reply budget. One group means the next
/// reduce is the final one; otherwise each group of two or more is reduced to one summary,
/// a single-partial group is carried over unchanged, and the results are grouped again. A
/// multi-group split always has fewer groups than partials, so every level shrinks the
/// list.
pub fn reduce_groups(
    input: &SummaryInput,
    partials: &[SummaryV1],
    budget: ContextBudget,
) -> Result<Vec<Range<usize>>, PromptError> {
    reduce_groups_with_rules(input, input.rules, partials, budget)
}

/// [`reduce_groups`] with the shared rules of `rules` in place of [`SummaryInput::rules`]
/// (as [`plan_with_rules`]).
///
/// # Errors
/// As [`reduce_groups`].
pub fn reduce_groups_with_rules(
    input: &SummaryInput,
    rules: SummaryRules,
    partials: &[SummaryV1],
    budget: ContextBudget,
) -> Result<Vec<Range<usize>>, PromptError> {
    reduce_groups_merging(input, rules, partials, budget, Merge::Plain)
}

/// [`reduce_groups`] for the merge requests of a two-call run ([`reduce_prompt_two_call`],
/// T6.20c, evaluation only).
///
/// # Errors
/// As [`reduce_groups`].
pub fn reduce_groups_two_call(
    input: &SummaryInput,
    partials: &[SummaryV1],
    budget: ContextBudget,
) -> Result<Vec<Range<usize>>, PromptError> {
    reduce_groups_merging(input, input.rules, partials, budget, Merge::TwoCall)
}

/// Which merge instruction a reduce request carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Merge {
    /// [`REDUCE`] (production).
    Plain,
    /// [`REDUCE`] plus [`TWO_CALL_REDUCE_MERGE`] (T6.20c, evaluation only).
    TwoCall,
}

impl Merge {
    fn text(self) -> String {
        match self {
            Merge::Plain => REDUCE.to_owned(),
            Merge::TwoCall => format!("{REDUCE}{TWO_CALL_REDUCE_MERGE}"),
        }
    }

    /// The style a merge at `level` carries: the input's on the last production merge, none
    /// on an inner merge (its reply is another partial) or a two-call merge (evaluation
    /// only, run with an empty style).
    fn style(
        self,
        input: &SummaryInput,
        level: MergeLevel,
    ) -> Option<(StylePreset, &SummaryStyle)> {
        (self == Merge::Plain && level == MergeLevel::Last)
            .then_some((input.style_preset, &input.style))
    }
}

/// Which merge of a reduce run a [`reduce_prompt`] is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MergeLevel {
    /// The merge whose reply is the summary the user sees: [`reduce_groups`] returned one
    /// group. It carries the user's style ([`SummaryInput::style`]).
    Last,
    /// A merge of one group of several ([`reduce_groups`] returned more than one): its
    /// reply is another partial summary, so it carries no style.
    Inner,
}

fn reduce_groups_merging(
    input: &SummaryInput,
    rules: SummaryRules,
    partials: &[SummaryV1],
    budget: ContextBudget,
    merge: Merge,
) -> Result<Vec<Range<usize>>, PromptError> {
    let whole = reduce_prompt_merging(input, rules, partials, budget, merge, MergeLevel::Last);
    if budget.fits(whole.estimated_tokens, whole.max_output_tokens) {
        return Ok(std::iter::once(0..partials.len()).collect());
    }
    let numbers = segment_numbers(input);
    let fixed = TOKENS_PER_REQUEST
        + 2 * TOKENS_PER_MESSAGE
        // The style is counted for every group: an inner merge does not carry it, but a
        // level that ends in one group must fit with it.
        + estimate_tokens(&system_text(
            input.template,
            input.rules_text(rules),
            input.user_name.as_deref(),
            Some(&merge.text()),
            &input.vocabulary,
            merge.style(input, MergeLevel::Last),
        ))
        + estimate_tokens(&reduce_user_text(input, &[], &numbers));
    // Six-digit part numbers bound the label of any position.
    let cost = |p: &SummaryV1| estimate_tokens(&partial_block(999_999, 999_999, p, &numbers));
    let reply = |content: usize| reply_tokens(ReplyBasis::Partials(content), budget);
    let fits = |content: usize| budget.fits(fixed + content, reply(content));
    let mut groups = Vec::new();
    let mut start = 0;
    let mut content = 0;
    for (i, p) in partials.iter().enumerate() {
        let c = cost(p);
        if !fits(content + c) && i > start {
            groups.push(start..i);
            start = i;
            content = 0;
        }
        content += c;
    }
    groups.push(start..partials.len());
    // Prefer merging a singleton tail now: borrow the previous group's last partial when
    // that group keeps at least two and the pair fits.
    if let [.., prev, last] = groups.as_mut_slice()
        && last.len() == 1
        && prev.len() >= 3
        && fits(cost(&partials[prev.end - 1]) + cost(&partials[last.start]))
    {
        prev.end -= 1;
        last.start -= 1;
    }
    let largest = partials.iter().map(&cost).max().unwrap_or(0);
    if !fits(largest) {
        return Err(PromptError::ContextTooSmall {
            needed: fixed + largest + reply(largest) + REPAIR_MAX_TOKENS,
            available: budget.context_window,
        });
    }
    if groups.len() >= partials.len() {
        // No two neighbours fit together: no progress is possible.
        let needed = partials
            .windows(2)
            .map(|w| {
                let c = cost(&w[0]) + cost(&w[1]);
                fixed + c + reply(c) + REPAIR_MAX_TOKENS
            })
            .min()
            .unwrap_or(fixed + largest + reply(largest) + REPAIR_MAX_TOKENS);
        return Err(PromptError::ContextTooSmall {
            needed,
            available: budget.context_window,
        });
    }
    Ok(groups)
}

/// The request that merges `partials` (consecutive partial summaries, in meeting order)
/// into one summary, with the reply budget [`reply_tokens`] of the partials. The partials
/// are shown citing segment numbers ([`crate::summary::to_numbered_json`]); the reply may cite
/// only the segments the partials cite ([`Citable::cited_by`]): a merge sees no transcript.
/// The [`MergeLevel::Last`] merge carries the user's style ([`SummaryInput::style`]).
#[must_use]
pub fn reduce_prompt(
    input: &SummaryInput,
    partials: &[SummaryV1],
    budget: ContextBudget,
    level: MergeLevel,
) -> Prompt {
    reduce_prompt_with_rules(input, input.rules, partials, budget, level)
}

/// [`reduce_prompt`] with the shared rules of `rules` in place of [`SummaryInput::rules`]
/// (as [`plan_with_rules`]).
#[must_use]
pub fn reduce_prompt_with_rules(
    input: &SummaryInput,
    rules: SummaryRules,
    partials: &[SummaryV1],
    budget: ContextBudget,
    level: MergeLevel,
) -> Prompt {
    reduce_prompt_merging(input, rules, partials, budget, Merge::Plain, level)
}

/// [`reduce_prompt`] for a two-call run (T6.20c, evaluation only): the same request with
/// one more paragraph in its system message, asking the merge to fold action items that
/// are one commitment in different words (keeping their citations) and to keep every
/// distinct commitment. Exact repeats are removed in code before the merge
/// ([`crate::summary::drop_repeated_actions`]). It never carries the user's style.
#[must_use]
pub fn reduce_prompt_two_call(
    input: &SummaryInput,
    partials: &[SummaryV1],
    budget: ContextBudget,
) -> Prompt {
    reduce_prompt_merging(
        input,
        input.rules,
        partials,
        budget,
        Merge::TwoCall,
        MergeLevel::Last,
    )
}

fn reduce_prompt_merging(
    input: &SummaryInput,
    rules: SummaryRules,
    partials: &[SummaryV1],
    budget: ContextBudget,
    merge: Merge,
    level: MergeLevel,
) -> Prompt {
    let numbers = segment_numbers(input);
    let content: usize = partials
        .iter()
        .enumerate()
        .map(|(i, p)| estimate_tokens(&partial_block(i + 1, partials.len(), p, &numbers)))
        .sum();
    Prompt::new(
        vec![
            ChatMessage::new(
                Role::System,
                system_text(
                    input.template,
                    input.rules_text(rules),
                    input.user_name.as_deref(),
                    Some(&merge.text()),
                    &input.vocabulary,
                    merge.style(input, level),
                ),
            ),
            ChatMessage::new(Role::User, reduce_user_text(input, partials, &numbers)),
        ],
        reply_tokens(ReplyBasis::Partials(content), budget),
        Citable::cited_by(partials, &numbers),
    )
}

/// The user message of a repair round (PLAN.md 3.7): the validator's errors, one per line
/// with their JSON Pointer path, at most `max_errors` of them, and whether the reply was
/// cut off at the output limit. The message never exceeds [`REPAIR_MAX_TOKENS`] (with its
/// per-message overhead): error lines are cut at a fixed length (the model's reply can put
/// anything into an error, for example a long invalid segment id), and errors that no
/// longer fit are counted in the "and N more" line.
#[must_use]
pub fn repair_message(errors: &[SummaryError], truncated: bool, max_errors: usize) -> ChatMessage {
    let mut head = String::new();
    if truncated {
        head.push_str(
            "Your reply was cut off because it reached the output limit. Write a shorter summary.\n",
        );
    }
    head.push_str("Your reply is not a valid summary. Problems found:\n");
    let mut tail = String::new();
    if errors.iter().any(|e| {
        matches!(
            e.kind,
            SummaryErrorKind::UnknownSegmentId(_)
                | SummaryErrorKind::UnknownSegmentNumber(_)
                | SummaryErrorKind::InvalidSegmentNumber(_)
        )
    }) {
        tail.push_str(
            "Cite only segment numbers that appear in square brackets in the transcript you were given.\n",
        );
    }
    tail.push_str(
        "Reply again with the complete corrected JSON object only, following the schema and the rules.",
    );
    // Estimates are subadditive, so the sum of the parts bounds the joined message.
    let more_line_max = estimate_tokens(&format!("- and {} more\n", usize::MAX));
    let fixed =
        TOKENS_PER_MESSAGE + estimate_tokens(&head) + estimate_tokens(&tail) + more_line_max;
    let mut room = REPAIR_MAX_TOKENS.saturating_sub(fixed);
    let mut s = head;
    let mut shown = 0;
    for e in errors.iter().take(max_errors) {
        let mut line = format!("- {e}");
        if line.chars().count() > REPAIR_LINE_MAX_CHARS {
            // Cut so the line, ellipsis included, is REPAIR_LINE_MAX_CHARS long.
            line = line.chars().take(REPAIR_LINE_MAX_CHARS - 3).collect();
            line.push_str("...");
        }
        line.push('\n');
        let cost = estimate_tokens(&line);
        if cost > room {
            break;
        }
        room -= cost;
        s.push_str(&line);
        shown += 1;
    }
    if errors.len() > shown {
        let _ = writeln!(s, "- and {} more", errors.len() - shown);
    }
    s.push_str(&tail);
    ChatMessage::new(Role::User, s)
}

/// The reply schema ([`llm_json_schema`]) as compact JSON (keys sorted, so the text is
/// stable).
fn schema_text() -> &'static str {
    static SCHEMA: OnceLock<String> = OnceLock::new();
    SCHEMA.get_or_init(|| llm_json_schema().to_string())
}

/// The system message: the shared rules (`rules`, [`SummaryInput::rules_text`]) with the
/// schema, the template, `extra` (a map or merge instruction), the user's own words
/// (T12.15a; every request, nothing when empty) and last the user's style block (T7.4b;
/// nothing when `style` is `None` or empty). A request without words or a style is
/// unchanged.
fn system_text(
    template: Template,
    rules: &str,
    user_name: Option<&str>,
    extra: Option<&str>,
    vocabulary: &Vocabulary,
    style: Option<(StylePreset, &SummaryStyle)>,
) -> String {
    let mut s = rules.replace("{{schema}}", schema_text());
    s.push('\n');
    s.push_str(template.instructions());
    if let Some(name) = user_name {
        s.push('\n');
        s.push_str(&USER_NAME.replace("{{name}}", &one_line(name)));
    }
    if let Some(e) = extra {
        s.push('\n');
        s.push_str(e);
    }
    if let Some(block) = vocabulary.block() {
        s.push('\n');
        s.push_str(&block);
    }
    if let Some(block) = style.and_then(|(preset, style)| style_block(preset, style)) {
        s.push('\n');
        s.push_str(&block);
    }
    s
}

fn map_text(part: usize, parts: usize) -> String {
    MAP.replace("{{part}}", &part.to_string())
        .replace("{{parts}}", &parts.to_string())
}

/// `HH:MM:SS` from the start of the meeting.
fn clock(t: MeetingTime) -> String {
    let s = t.as_millis() / 1000;
    format!("{:02}:{:02}:{:02}", s / 3600, (s / 60) % 60, s % 60)
}

pub(super) fn one_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// One transcript line: `[number] HH:MM:SS Speaker: text`, numbered from 1 per meeting.
fn segment_line(number: usize, seg: &PromptSegment) -> String {
    let speaker = one_line(&seg.speaker);
    let speaker = if speaker.is_empty() {
        "Unknown speaker".to_string()
    } else {
        speaker
    };
    format!(
        "[{number}] {} {}: {}\n",
        clock(seg.start),
        speaker,
        one_line(&seg.text)
    )
}

pub(super) fn meeting_block(m: &MeetingInfo) -> String {
    let mut s = String::from("Meeting\n");
    let title = m.title.as_deref().map(one_line).unwrap_or_default();
    let _ = writeln!(
        s,
        "Title: {}",
        if title.is_empty() { "(none)" } else { &title }
    );
    let _ = writeln!(
        s,
        "Started: {}",
        m.started.as_deref().map_or("(unknown)".into(), one_line)
    );
    let _ = writeln!(
        s,
        "Duration: {}",
        clock(MeetingTime::from_millis(m.duration_ms))
    );
    let attendees: Vec<String> = m
        .attendees
        .iter()
        .map(|a| one_line(a))
        .filter(|a| !a.is_empty())
        .collect();
    let _ = writeln!(
        s,
        "Attendees: {}",
        if attendees.is_empty() {
            "(not recorded)".to_string()
        } else {
            attendees.join(", ")
        }
    );
    s
}

fn notes_block(notes: &[UserNote]) -> String {
    let mut s = String::from("User notes (time from the start of the meeting)\n");
    let mut any = false;
    for n in notes {
        let lines: Vec<&str> = n
            .text
            .lines()
            .map(str::trim_end)
            .filter(|l| !l.trim().is_empty())
            .collect();
        let Some((first, rest)) = lines.split_first() else {
            continue;
        };
        any = true;
        let _ = writeln!(s, "[{}] {}", clock(n.at), first.trim_start());
        for l in rest {
            let _ = writeln!(s, "    {l}");
        }
    }
    if !any {
        s.push_str("(none)\n");
    }
    s
}

/// The user message; `lines` are rendered [`segment_line`]s.
fn user_text(
    input: &SummaryInput,
    lines: &[String],
    notes: &[UserNote],
    part: Option<(usize, usize)>,
) -> String {
    let mut s = meeting_block(&input.meeting);
    s.push('\n');
    match part {
        Some((p, n)) => {
            let _ = writeln!(s, "Transcript, part {p} of {n}");
        }
        None => s.push_str("Transcript\n"),
    }
    if lines.is_empty() && part.is_none() {
        s.push_str("(no speech was transcribed)\n");
    }
    for line in lines {
        s.push_str(line);
    }
    s.push('\n');
    s.push_str(&notes_block(notes));
    s
}

fn partial_block(i: usize, n: usize, p: &SummaryV1, numbers: &SegmentNumbers) -> String {
    let json = to_numbered_json(p, numbers);
    format!("Partial summary {i} of {n}:\n{json}\n")
}

fn reduce_user_text(
    input: &SummaryInput,
    partials: &[SummaryV1],
    numbers: &SegmentNumbers,
) -> String {
    let mut s = meeting_block(&input.meeting);
    s.push('\n');
    for (i, p) in partials.iter().enumerate() {
        s.push_str(&partial_block(i + 1, partials.len(), p, numbers));
    }
    s.push('\n');
    s.push_str(&notes_block(&input.notes));
    s
}
