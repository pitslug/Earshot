//! Meeting summary, schema v1 (PLAN.md 3.7).
//!
//! - [`SummaryV1`] and its parts: the JSON the LLM must return, serialised with serde and
//!   described by [`json_schema`] (committed as `schemas/summary.v1.json`, regenerated
//!   with `cargo xtask schema --write` and drift-checked by `cargo xtask ci`).
//! - [`parse_summary`], [`validate_value`], [`validate_summary`]: validation with
//!   structured errors ([`SummaryError`]: JSON Pointer path + reason), including grounding:
//!   every decision and action item cites at least one segment id, and every cited id
//!   exists in the transcript.
//! - [`validate_reply`]: model replies cite segment numbers ([`llm_json_schema`],
//!   committed as `schemas/summary-llm.v1.json`); they are mapped back to segment ids and
//!   checked, including repetition.
//! - [`Selection`], [`parse_selection`], [`build_from_selection`]: the merge by selection
//!   (T12.25 part 2, evaluation only; production never calls them).
//! - [`Summarizer`]: the backend trait that produces a validated summary from a
//!   [`crate::prompt::SummaryInput`].

mod cite;
mod recite;
mod select;
mod summarizer;
mod validate;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::identity::EntityId;

pub use cite::{
    CheckedReply, Citable, JOINED_SUMMARY_NOTICE, MAX_CITATIONS, REPEAT_LIMIT,
    SUMMARY_LLM_SCHEMA_PATH, SegmentNumbers, drop_repeated_actions, drop_repeats, join_partials,
    llm_json_schema, repetition_errors, to_numbered, to_numbered_json, validate_reply,
};
pub use recite::{ReciteError, Recited, TimedSegment, recite};
pub use select::{
    SELECTION_FIELDS, SELECTION_MAX_KEPT, SelectCounts, SelectItems, Selection,
    build_from_selection, parse_selection,
};
pub use summarizer::Summarizer;
pub use validate::{
    SegmentIds, SummaryError, SummaryErrorKind, parse_summary, validate_summary, validate_value,
};

/// Version of the summary schema implemented here.
pub const SUMMARY_SCHEMA_VERSION: u32 = 1;

/// Repo-relative path of the committed JSON Schema for [`SummaryV1`].
pub const SUMMARY_SCHEMA_PATH: &str = "schemas/summary.v1.json";

/// A meeting summary produced by the LLM from the transcript and the user's notes.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(title = "Earshot meeting summary v1")]
pub struct SummaryV1 {
    /// Short meeting title, without date or time.
    pub title: String,
    /// One to three sentences: what the meeting was about and what came out of it.
    pub tldr: String,
    /// The user's notes, enhanced with what was said, grouped into sections.
    pub enhanced_notes: Vec<NoteSection>,
    /// Decisions that were made, each citing the segments where it was made.
    pub decisions: Vec<Decision>,
    /// Tasks someone agreed to do, each citing the segments where it was agreed.
    pub action_items: Vec<ActionItem>,
    /// Questions raised but not answered.
    pub open_questions: Vec<String>,
    /// Short topic labels.
    pub topics: Vec<String>,
}

/// A section of enhanced notes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct NoteSection {
    /// Section heading.
    pub heading: String,
    /// Bullet points of the section.
    pub bullets: Vec<NoteBullet>,
}

/// One bullet of enhanced notes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct NoteBullet {
    /// Bullet text.
    pub text: String,
    /// Ids of the transcript segments this bullet is based on (may be empty for content
    /// taken only from the user's notes).
    pub source_segment_ids: Vec<EntityId>,
}

/// A decision made in the meeting.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Decision {
    /// What was decided.
    pub text: String,
    /// Ids of the transcript segments where the decision was made (at least one).
    pub source_segment_ids: Vec<EntityId>,
}

/// A task someone agreed to do.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ActionItem {
    /// Who will do it (a speaker's name, or "Unassigned").
    pub owner: String,
    /// What will be done.
    pub task: String,
    /// When it is due, as said in the meeting (for example "Friday" or "2026-11-01"),
    /// or null when no date was given.
    // Required but nullable (no `serde(default)`), so the schema works with strict
    // structured-output modes that need every property listed as required.
    #[schemars(with = "RequiredNullableString")]
    pub due: Option<String>,
    /// Ids of the transcript segments where the task was agreed (at least one).
    pub source_segment_ids: Vec<EntityId>,
}

/// Schema stand-in for an `Option<String>` field that must be present but may be null.
/// schemars makes `Option` fields optional; a non-`Option` type keeps the field in
/// `required` while the schema still allows null.
struct RequiredNullableString;

impl JsonSchema for RequiredNullableString {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "RequiredNullableString".into()
    }

    fn inline_schema() -> bool {
        true
    }

    fn json_schema(_generator: &mut schemars::SchemaGenerator) -> schemars::Schema {
        schemars::json_schema!({ "type": ["string", "null"] })
    }
}

/// The JSON Schema (draft 2020-12) of [`SummaryV1`], as committed at
/// [`SUMMARY_SCHEMA_PATH`].
pub fn json_schema() -> serde_json::Value {
    schemars::schema_for!(SummaryV1).to_value()
}

#[cfg(test)]
mod tests;
