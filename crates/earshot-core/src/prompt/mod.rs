//! Summary prompts (PLAN.md 3.7): chat messages built from the transcript, the user's
//! notes, the meeting details and a template.
//!
//! - [`SummaryInput`] ([`MeetingInfo`], [`PromptSegment`], [`UserNote`], [`Template`]):
//!   what a summary is built from. Segments carry their ids and speaker names; notes are
//!   anchored to meeting time.
//! - [`plan`]: one request when everything fits in the [`ContextBudget`], otherwise
//!   map-reduce over consecutive transcript chunks ([`MapChunk`]); [`reduce_groups`] and
//!   [`reduce_prompt`] merge the partial summaries, hierarchically when they do not fit
//!   in one request. [`plan_chunks`] builds the same map requests over chunks the caller
//!   chose. Each has a `*_with_rules` form taking the writer's [`SummaryRules`] in place of
//!   [`SummaryInput::rules`] (T12.17c).
//! - [`plan_chunks_two_call`], [`reduce_groups_two_call`], [`reduce_prompt_two_call`]: the
//!   two-call chunked evaluation (T6.20c, evaluation only): per chunk one request for the
//!   action items and one for everything else, then a merge that also folds reworded
//!   action items. Production never uses them.
//! - [`select_prompt`], [`selection_repair_message`]: the merge by selection (T12.25 part 2,
//!   evaluation only): the merge request shows the numbered items of every partial and the
//!   reply chooses which to keep and fold ([`crate::summary::Selection`]). Production never
//!   uses them.
//! - [`SummaryStyle`]: the user's style (T7.4b), one block at the end of the system message
//!   of the request that writes the summary the user sees (the single request and the last
//!   merge, [`MergeLevel::Last`]), after the fixed rules; never in a map request.
//! - [`Vocabulary`]: the user's own words (T12.15a), spelling help for names and acronyms,
//!   one block in the system message of every request (single, map, every merge), after
//!   any map or merge instruction and before the style.
//! - [`repair_message`]: the follow-up that feeds validator errors back to the model.
//! - [`estimate_tokens`]: a pessimistic token estimate without a tokenizer.
//!
//! Templates and the shared rules are data files under `src/prompt/templates/`, embedded
//! with `include_str!`. The shared rules are chosen by the writer ([`SummaryRules`],
//! T12.17c: `brief.md` for the agent CLIs, `system.md` for everyone else); [`PromptVariant`]
//! picks an alternative set of shared rules for evaluation only (T6.20b); production uses
//! [`PromptVariant::Current`], which reads [`SummaryInput::rules`]. The rules require one JSON object following the summary v1 schema
//! (included in the system message, so `json_object` and plain-text fallbacks see it too)
//! and citations by segment number: transcript lines are numbered 1, 2, 3, ... per meeting
//! and replies follow [`crate::summary::llm_json_schema`]; [`crate::summary::validate_reply`]
//! maps the numbers back to segment ids. Each request has its own reply budget, sized to
//! what it summarises ([`reply_tokens`]).

mod builder;
mod chunks;
mod select;
mod style;
mod template;
mod tokens;
mod vocabulary;

pub use builder::{
    ChatMessage, ContextBudget, MapChunk, MeetingInfo, MergeLevel, Prompt, PromptError, PromptPlan,
    PromptSegment, REPAIR_MAX_TOKENS, REPLY_MIN_TOKENS, REPLY_PER_PARTIAL_TOKEN,
    REPLY_PER_TRANSCRIPT_TOKEN, ReplyBasis, Role, SummaryInput, TwoCallChunk, UserNote,
    estimate_messages, grown_reply_tokens, plan, plan_chunks, plan_chunks_two_call,
    plan_chunks_with_rules, plan_with_rules, reduce_groups, reduce_groups_two_call,
    reduce_groups_with_rules, reduce_prompt, reduce_prompt_two_call, reduce_prompt_with_rules,
    repair_message, reply_tokens, segment_numbers,
};
pub use chunks::{TimeChunkError, time_chunks};
pub use select::{
    SELECT_REPLY_BASE_TOKENS, SELECT_REPLY_PER_ITEM_TOKENS, select_prompt, selection_repair_message,
};
pub use style::{
    MAX_STYLE_CHARS, STYLE_LINE_PREFIX, StyleError, StylePreset, SummaryStyle, UnknownStylePreset,
};
pub use template::{PromptVariant, SummaryRules, Template, UnknownPromptVariant, UnknownTemplate};
pub use tokens::{TOKENS_PER_MESSAGE, TOKENS_PER_REQUEST, estimate_tokens};
pub use vocabulary::{
    MAX_VOCABULARY_CHARS, MAX_VOCABULARY_WORD_CHARS, MAX_VOCABULARY_WORDS, VOCABULARY_LINE_PREFIX,
    Vocabulary, VocabularyError,
};

#[cfg(test)]
mod tests;
