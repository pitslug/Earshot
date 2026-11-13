//! Summary templates, embedded data files under `src/prompt/templates/`.

use core::fmt;
use core::str::FromStr;

use serde::{Deserialize, Serialize};

/// Shared rules of every summary request written by the local model or an API writer
/// ([`SummaryRules::Standard`]); `{{schema}}` is replaced by the summary v1 JSON Schema.
pub(super) const SYSTEM: &str = include_str!("templates/system.md");
/// Shared rules of every summary request written by an agent CLI, Claude Code or Codex
/// ([`SummaryRules::Brief`], T12.17c): [`SYSTEM`] with shorter notes (at most three sections
/// of three one-sentence bullets), a one-sentence tldr and items of about fifteen words at
/// most, every decision and action item still listed. Measured as the T12.17 `brief`
/// variant (docs/summary-eval.md).
pub(super) const BRIEF: &str = include_str!("templates/brief.md");
/// Extra instruction for one part of a map-reduce run; `{{part}}` and `{{parts}}` are
/// replaced by the part number and count.
pub(super) const MAP: &str = include_str!("templates/map.md");
/// Extra instruction for merging partial summaries.
pub(super) const REDUCE: &str = include_str!("templates/reduce.md");
/// The user's style block (T7.4b), appended after everything else in the system message of
/// a final request; `{{style}}` is replaced by the neutralised style lines.
pub(super) const STYLE: &str = include_str!("templates/style.md");
/// The user's own words block (T12.15a), in the system message of every request after any
/// map or merge instruction and before the style; `{{words}}` is replaced by the prefixed
/// words, one per line.
pub(super) const VOCABULARY: &str = include_str!("templates/vocabulary.md");
/// The rule that says how the user appears in the transcript (T9.1h).
pub(super) const USER_NAME: &str = include_str!("templates/user.md");

/// Two-call chunked evaluation (T6.20c, evaluation only): appended to [`MAP`] in the request
/// that extracts only a part's action items.
pub(super) const TWO_CALL_MAP_ACTIONS: &str = include_str!("templates/twocall/map-actions.md");
/// Two-call chunked evaluation (T6.20c): appended to [`MAP`] in the request that summarises
/// everything but the action items.
pub(super) const TWO_CALL_MAP_REST: &str = include_str!("templates/twocall/map-rest.md");
/// Two-call chunked evaluation (T6.20c): appended to [`REDUCE`] so the merge folds action
/// items that are one commitment in different words.
pub(super) const TWO_CALL_REDUCE_MERGE: &str = include_str!("templates/twocall/reduce-merge.md");

const VARIANT_OWNERS: &str = include_str!("templates/variants/owners.md");
const VARIANT_DECISIONS: &str = include_str!("templates/variants/decisions.md");
const VARIANT_ITEMS: &str = include_str!("templates/variants/items.md");
const VARIANT_LEAN: &str = include_str!("templates/variants/lean.md");
const VARIANT_ASSIGNMENTS: &str = include_str!("templates/variants/assignments.md");
const VARIANT_HANDED: &str = include_str!("templates/variants/handed.md");
const VARIANT_CHECKED: &str = include_str!("templates/variants/checked.md");
const VARIANT_PREVIOUS: &str = include_str!("templates/variants/previous.md");
const VARIANT_SHORT: &str = include_str!("templates/variants/short.md");

const GENERAL: &str = include_str!("templates/general.md");
const ONE_ON_ONE: &str = include_str!("templates/one-on-one.md");
const STANDUP: &str = include_str!("templates/standup.md");

/// The kind of meeting a summary is written for. Each template adds its own focus to the
/// shared rules; the output schema is the same for all.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Template {
    /// Any meeting.
    #[default]
    General,
    /// A regular meeting of two people.
    OneOnOne,
    /// A team stand-up.
    Standup,
}

impl Template {
    /// Every template, in display order.
    pub const ALL: [Template; 3] = [Template::General, Template::OneOnOne, Template::Standup];

    /// The template's name as used in settings and data files (`general`, `one-on-one`,
    /// `standup`).
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Template::General => "general",
            Template::OneOnOne => "one-on-one",
            Template::Standup => "standup",
        }
    }

    /// The template's instructions (the embedded data file).
    #[must_use]
    pub const fn instructions(self) -> &'static str {
        match self {
            Template::General => GENERAL,
            Template::OneOnOne => ONE_ON_ONE,
            Template::Standup => STANDUP,
        }
    }
}

impl fmt::Display for Template {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// A template name that is not one of [`Template::ALL`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnknownTemplate(pub String);

impl fmt::Display for UnknownTemplate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "unknown summary template `{}`", self.0)
    }
}

impl std::error::Error for UnknownTemplate {}

impl FromStr for Template {
    type Err = UnknownTemplate;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Template::ALL
            .into_iter()
            .find(|t| t.name() == s)
            .ok_or_else(|| UnknownTemplate(s.to_string()))
    }
}

/// The shared rules a summary request uses in production (T12.17c), chosen by the writer
/// in one place (`earshot_llm::LlmClient::summary_rules`), never by a setting: the agent
/// CLIs (Claude Code, Codex) use [`SummaryRules::Brief`]; the local model and the
/// OpenAI-compatible and Anthropic API writers use [`SummaryRules::Standard`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SummaryRules {
    /// `templates/system.md`.
    #[default]
    Standard,
    /// `templates/brief.md`.
    Brief,
}

impl SummaryRules {
    /// The rules text; `{{schema}}` is replaced by the reply schema.
    #[must_use]
    pub(super) const fn rules(self) -> &'static str {
        match self {
            SummaryRules::Standard => SYSTEM,
            SummaryRules::Brief => BRIEF,
        }
    }

    /// Whether these are the standard rules (used to leave the field out of serialised
    /// inputs, so stored summary inputs are unchanged).
    #[must_use]
    pub fn is_standard(&self) -> bool {
        *self == SummaryRules::Standard
    }
}

/// Which set of shared rules a summary request uses (T6.20b prompt tuning).
///
/// [`PromptVariant::Current`] is the production prompt: the input's [`SummaryRules`], which
/// the writer chooses (T12.17c). Every other variant names one rules file whatever the
/// writer: [`PromptVariant::Standard`] is [`SYSTEM`], [`PromptVariant::Brief`] is [`BRIEF`]
/// and the rest are full alternative rules files under `src/prompt/templates/variants/`,
/// all for evaluation only (`cargo xtask sum-eval --prompt-variant`, docs/summary-eval.md, "T6.20b prompt
/// variants"). A variant replaces only the shared rules: templates, map and reduce
/// instructions, numbering, schema and validation are the same for all.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PromptVariant {
    /// The production rules: the input's [`SummaryRules`].
    #[default]
    Current,
    /// Action items: one per task, the owner is the person who will do it (often named or
    /// addressed by someone else), "Unassigned" when nobody is.
    Owners,
    /// Decisions: only what the group settled, the final choice, one choice per decision.
    Decisions,
    /// [`PromptVariant::Owners`] and [`PromptVariant::Decisions`] together.
    Items,
    /// [`PromptVariant::Items`] plus shorter notes that do not repeat decisions or actions.
    Lean,
    /// T6.20e round 1: action items are only tasks taken on or given for after the meeting
    /// (not what was done during it, not suggestions nobody took on); the owner is the
    /// person addressed or named; a shared task is one item naming everyone.
    Assignments,
    /// T6.20e round 3: the production rules (round 2 `settled`, adopted in T6.20f) plus
    /// tasks handed out by the person leading the meeting belong to the person they are
    /// handed to, and what the product or plan will not have is a decision.
    Handed,
    /// T6.20e round 4: the production rules (round 2 `settled`) restated tightly, with what
    /// to leave out of each list and a check that a choice was not changed later.
    Checked,
    /// The production rules before T6.20f (T6.20e round 0), kept as a baseline: action
    /// owners are the speaker, no rule for what a decision is.
    Previous,
    /// T12.17: the production rules with shorter notes (at most four sections of four
    /// one-sentence bullets, only what the decisions, actions and open questions need), a
    /// one- or two-sentence tldr, and every decision and action item still listed.
    Short,
    /// T12.17: [`PromptVariant::Short`], tighter (three sections of three bullets, a
    /// one-sentence tldr, items of about fifteen words at most). Since T12.17c the agent
    /// CLIs' production rules ([`BRIEF`]); as a variant it applies them to any writer.
    Brief,
    /// T12.17c: [`SYSTEM`] whatever the writer, so an evaluation can run the standard rules
    /// on an agent CLI, whose production rules are [`BRIEF`].
    Standard,
}

impl PromptVariant {
    /// Every variant, the production one first.
    pub const ALL: [PromptVariant; 12] = [
        PromptVariant::Current,
        PromptVariant::Owners,
        PromptVariant::Decisions,
        PromptVariant::Items,
        PromptVariant::Lean,
        PromptVariant::Assignments,
        PromptVariant::Handed,
        PromptVariant::Checked,
        PromptVariant::Previous,
        PromptVariant::Short,
        PromptVariant::Brief,
        PromptVariant::Standard,
    ];

    /// The variant's name as used on the command line and in format keys.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            PromptVariant::Current => "current",
            PromptVariant::Owners => "owners",
            PromptVariant::Decisions => "decisions",
            PromptVariant::Items => "items",
            PromptVariant::Lean => "lean",
            PromptVariant::Assignments => "assignments",
            PromptVariant::Handed => "handed",
            PromptVariant::Checked => "checked",
            PromptVariant::Previous => "previous",
            PromptVariant::Short => "short",
            PromptVariant::Brief => "brief",
            PromptVariant::Standard => "standard",
        }
    }

    /// Whether this is the production prompt (used to leave the field out of serialised
    /// inputs, so stored summary inputs are unchanged).
    #[must_use]
    pub fn is_current(&self) -> bool {
        *self == PromptVariant::Current
    }

    /// The shared rules of this variant for a writer of the standard rules (as
    /// [`PromptVariant::rules_for`] with [`SummaryRules::Standard`]).
    #[cfg(test)]
    #[must_use]
    pub(super) const fn rules(self) -> &'static str {
        self.rules_for(SummaryRules::Standard)
    }

    /// The shared rules of this variant for an input whose writer's rules are `writer`;
    /// `{{schema}}` is replaced by the reply schema. Only [`PromptVariant::Current`] reads
    /// `writer`.
    #[must_use]
    pub(super) const fn rules_for(self, writer: SummaryRules) -> &'static str {
        match self {
            PromptVariant::Current => writer.rules(),
            PromptVariant::Standard => SYSTEM,
            PromptVariant::Owners => VARIANT_OWNERS,
            PromptVariant::Decisions => VARIANT_DECISIONS,
            PromptVariant::Items => VARIANT_ITEMS,
            PromptVariant::Lean => VARIANT_LEAN,
            PromptVariant::Assignments => VARIANT_ASSIGNMENTS,
            PromptVariant::Handed => VARIANT_HANDED,
            PromptVariant::Checked => VARIANT_CHECKED,
            PromptVariant::Previous => VARIANT_PREVIOUS,
            PromptVariant::Short => VARIANT_SHORT,
            PromptVariant::Brief => BRIEF,
        }
    }
}

impl fmt::Display for PromptVariant {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// A prompt variant name that is not one of [`PromptVariant::ALL`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnknownPromptVariant(pub String);

impl fmt::Display for UnknownPromptVariant {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let names: Vec<&str> = PromptVariant::ALL.iter().map(|v| v.name()).collect();
        write!(
            f,
            "unknown prompt variant `{}` (one of {})",
            self.0,
            names.join(", ")
        )
    }
}

impl std::error::Error for UnknownPromptVariant {}

impl FromStr for PromptVariant {
    type Err = UnknownPromptVariant;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        PromptVariant::ALL
            .into_iter()
            .find(|v| v.name() == s)
            .ok_or_else(|| UnknownPromptVariant(s.to_string()))
    }
}
