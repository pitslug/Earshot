//! The format key of a `sum_eval` run (T12.49: its one source). The example records it as
//! `grammar` in every result file and checkpoint; `cargo xtask sum-eval` includes this file
//! by `#[path]` (std and earshot-core only) and reuses a stored report only when its key
//! equals the run's.
//!
//! The key is [`GRAMMAR`], then in this order: `-chunk<seconds>` for forced chunking
//! (`earshot_core::prompt::time_chunks`), `-twocall` for the T6.20c two-call chunks,
//! `-twotrack` for the T8.3ja two-track chunks, `-claude-code-brief` for the T7.4gb Claude
//! Code candidate or `-codex-brief` for the T7.4gc Codex candidate (`-brief` is their
//! T12.17c production rules, `earshot_llm::LlmClient::summary_rules`, so files from the
//! earlier standard rules, `-claude-code` and `-codex`, are never reused), `-prompt-<variant>`
//! for a T6.20b prompt variant other than the production one, `-reuse` for prompt prefix
//! reuse (T12.25 part 1), `-select` for the merge by selection (T12.25 part 2),
//! `-template-<name>` for a summary template other than Standard (T12.17) and `-items<n>`
//! for an item cap (T12.36). Rows made with different settings are never reused for each
//! other. Older keys (`-think<cap>` from the T12.10 thinking runs, removed in T12.49) name
//! result files that only history can reproduce.

use earshot_core::prompt::{PromptVariant, StylePreset};

/// The request format: indented JSON replies (`earshot_llm_local::grammar::summary_grammar`;
/// compact JSON was tried on 2026-10-25 and dropped, see its docs), transcript segments cited
/// by number, content-sized reply budgets and the loop guard. `-r2` is the second set of
/// reply ratios (`earshot_core::prompt::REPLY_PER_*`), `-r3` adds dropping duplicate items
/// (`earshot_core::summary::drop_repeats`), `-r4` the review fixes (action items keyed on
/// owner and task, bullets per section, merges citing only their partials' segments, a
/// number cited twice rejected), `-r5` the T6.20e `settled` rules as the production prompt
/// (T6.20f).
pub const GRAMMAR: &str = "indented-numbered-r5";

/// The agent-CLI candidate a key names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyAgent {
    /// Claude through Claude Code (T7.4gb).
    ClaudeCode,
    /// OpenAI through Codex (T7.4gc).
    Codex,
}

impl KeyAgent {
    /// The key part: `-claude-code-brief`, `-codex-brief`.
    #[must_use]
    pub fn suffix(self) -> &'static str {
        match self {
            Self::ClaudeCode => "-claude-code-brief",
            Self::Codex => "-codex-brief",
        }
    }
}

/// Every setting the format key names; the default is today's plain local run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct FormatKey {
    /// Forced chunking, seconds per map request.
    pub chunk_seconds: Option<u32>,
    /// Two-call chunks (T6.20c).
    pub two_call: bool,
    /// Two-track chunks (T8.3ja).
    pub two_track: bool,
    /// An agent-CLI candidate.
    pub agent: Option<KeyAgent>,
    /// The shared prompt rules (T6.20b).
    pub prompt_variant: PromptVariant,
    /// Prompt prefix reuse (T12.25 part 1).
    pub prefix_reuse: bool,
    /// Merge by selection (T12.25 part 2).
    pub merge_select: bool,
    /// The summary template (T12.17).
    pub style_preset: StylePreset,
    /// The local grammar's item cap (T12.36).
    pub max_items: Option<u32>,
}

impl FormatKey {
    /// The key text (module docs).
    #[must_use]
    pub fn key(&self) -> String {
        let mut key = GRAMMAR.to_owned();
        if let Some(s) = self.chunk_seconds {
            key.push_str(&format!("-chunk{s}"));
        }
        if self.two_call {
            key.push_str("-twocall");
        }
        if self.two_track {
            key.push_str("-twotrack");
        }
        if let Some(a) = self.agent {
            key.push_str(a.suffix());
        }
        if !self.prompt_variant.is_current() {
            key.push_str("-prompt-");
            key.push_str(self.prompt_variant.name());
        }
        if self.prefix_reuse {
            key.push_str("-reuse");
        }
        if self.merge_select {
            key.push_str("-select");
        }
        if !self.style_preset.is_standard() {
            key.push_str("-template-");
            key.push_str(self.style_preset.name());
        }
        if let Some(n) = self.max_items {
            key.push_str(&format!("-items{n}"));
        }
        key
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sum_eval_format_key_plain_run_is_the_grammar() {
        assert_eq!(FormatKey::default().key(), "indented-numbered-r5");
        assert_eq!(GRAMMAR, "indented-numbered-r5");
    }

    #[test]
    fn sum_eval_format_key_parts_come_in_their_fixed_order() {
        let all = FormatKey {
            chunk_seconds: Some(600),
            two_call: true,
            two_track: true,
            agent: Some(KeyAgent::Codex),
            prompt_variant: PromptVariant::Brief,
            prefix_reuse: true,
            merge_select: true,
            style_preset: StylePreset::Brief,
            max_items: Some(16),
        };
        assert_eq!(
            all.key(),
            "indented-numbered-r5-chunk600-twocall-twotrack-codex-brief-prompt-brief\
             -reuse-select-template-brief-items16"
        );
        assert_eq!(
            FormatKey {
                agent: Some(KeyAgent::ClaudeCode),
                ..FormatKey::default()
            }
            .key(),
            "indented-numbered-r5-claude-code-brief"
        );
    }

    #[test]
    fn sum_eval_format_key_names_every_variant_and_template_apart() {
        let variants: std::collections::BTreeSet<String> = PromptVariant::ALL
            .into_iter()
            .map(|prompt_variant| {
                FormatKey {
                    prompt_variant,
                    ..FormatKey::default()
                }
                .key()
            })
            .collect();
        assert_eq!(variants.len(), PromptVariant::ALL.len());
        let templates: std::collections::BTreeSet<String> = StylePreset::ALL
            .into_iter()
            .map(|style_preset| {
                FormatKey {
                    style_preset,
                    ..FormatKey::default()
                }
                .key()
            })
            .collect();
        assert_eq!(templates.len(), StylePreset::ALL.len());
    }
}
