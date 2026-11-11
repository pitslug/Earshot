//! Suggestions on meeting speech (T12.15ea, docs/vocabulary.md "Suggestions on meeting
//! speech"): every replacement the careful matchers would make on a baseline hypothesis, each
//! judged alone against the segment's reference.

use std::collections::HashSet;
use std::ops::AddAssign;

use serde::Serialize;

use crate::correct::Change;
use crate::lists;
use crate::score;

/// The bar of "What the numbers decide": at most this many wrong suggestions per meeting hour.
pub const WRONG_PER_HOUR_LIMIT: f64 = 5.0;

/// What one suggestion, applied alone, does to the segment's word errors.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    /// Errors fall.
    Right,
    /// Errors rise.
    Wrong,
    /// Errors stay the same.
    NoEffect,
}

/// One suggestion judged: its position in the hypothesis, its effect, and whether it offers a
/// distractor (a listed word not among the meeting's words).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Judged {
    pub position: usize,
    pub effect: Verdict,
    pub distractor: bool,
}

/// Counts of judged suggestions.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct SuggestCounts {
    pub suggestions: usize,
    pub right: usize,
    pub wrong: usize,
    pub no_effect: usize,
    pub distractor: usize,
}

impl AddAssign for SuggestCounts {
    fn add_assign(&mut self, o: SuggestCounts) {
        self.suggestions += o.suggestions;
        self.right += o.right;
        self.wrong += o.wrong;
        self.no_effect += o.no_effect;
        self.distractor += o.distractor;
    }
}

impl SuggestCounts {
    /// The counts of `judged`.
    pub fn of(judged: &[Judged]) -> SuggestCounts {
        let mut c = SuggestCounts {
            suggestions: judged.len(),
            ..SuggestCounts::default()
        };
        for j in judged {
            match j.effect {
                Verdict::Right => c.right += 1,
                Verdict::Wrong => c.wrong += 1,
                Verdict::NoEffect => c.no_effect += 1,
            }
            c.distractor += usize::from(j.distractor);
        }
        c
    }

    /// Each count per meeting hour.
    pub fn per_hour(&self, hours: f64) -> serde_json::Value {
        #[allow(clippy::cast_precision_loss)]
        let h = |n: usize| (hours > 0.0).then(|| n as f64 / hours);
        serde_json::json!({
            "suggestions": h(self.suggestions),
            "right": h(self.right),
            "wrong": h(self.wrong),
            "no_effect": h(self.no_effect),
            "distractor": h(self.distractor),
        })
    }

    /// The bar: at most [`WRONG_PER_HOUR_LIMIT`] wrong suggestions per meeting hour and more
    /// right than wrong.
    pub fn passes_bar(&self, hours: f64) -> bool {
        #[allow(clippy::cast_precision_loss)]
        let wrong_per_hour = self.wrong as f64 / hours;
        hours > 0.0 && wrong_per_hour <= WRONG_PER_HOUR_LIMIT && self.right > self.wrong
    }
}

/// Word errors (substitutions, deletions, insertions) of `hypothesis` words against
/// `reference`, both normalised as WER is (the scoring of score.rs).
pub fn word_errors(reference: &str, hypothesis: &[String]) -> usize {
    let r = lists::tokens(reference);
    let h = lists::tokens(&hypothesis.join(" "));
    let r: Vec<&str> = r.iter().map(String::as_str).collect();
    let h: Vec<&str> = h.iter().map(String::as_str).collect();
    score::score(&r, &h, &HashSet::new(), None).0.errors
}

/// Judges every change alone: the baseline `words` with only that change's position replaced
/// by `corrected`'s word there (the matcher's output, punctuation kept), scored against
/// `reference`. `said` holds every token of the meeting's words.
pub fn judge(
    reference: &str,
    words: &[String],
    corrected: &[String],
    changes: &[Change],
    said: &HashSet<String>,
) -> Vec<Judged> {
    let before = word_errors(reference, words);
    changes
        .iter()
        .map(|c| {
            let mut one = words.to_vec();
            one[c.position].clone_from(&corrected[c.position]);
            let after = word_errors(reference, &one);
            let effect = match after.cmp(&before) {
                std::cmp::Ordering::Less => Verdict::Right,
                std::cmp::Ordering::Greater => Verdict::Wrong,
                std::cmp::Ordering::Equal => Verdict::NoEffect,
            };
            let listed = lists::tokens(&c.listed);
            Judged {
                position: c.position,
                effect,
                distractor: listed.is_empty() || listed.iter().any(|t| !said.contains(t)),
            }
        })
        .collect()
}

/// Meeting hours: the UEM spans `(start, end)` in seconds, summed.
pub fn meeting_hours(uems: &[(f64, f64)]) -> f64 {
    uems.iter().map(|(a, b)| b - a).sum::<f64>() / 3600.0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::correct::{Variant, correct};

    fn s(words: &[&str]) -> Vec<String> {
        words.iter().map(|w| (*w).to_owned()).collect()
    }

    fn change(position: usize, original: &str, listed: &str) -> Change {
        Change {
            position,
            original: original.to_owned(),
            listed: listed.to_owned(),
        }
    }

    #[test]
    fn vocab_suggest_word_errors_follow_the_wer_normalisation() {
        assert_eq!(
            word_errors("Zorbla met Quent.", &s(&["zorbla,", "met", "quent"])),
            0
        );
        assert_eq!(word_errors("zorbla met quent", &s(&["zorble", "met"])), 2);
        assert_eq!(word_errors("", &s(&["zorbla"])), 1);
    }

    #[test]
    fn vocab_suggest_each_suggestion_is_judged_alone() {
        let said: HashSet<String> = ["zorbla", "met", "quent", "vantrix", "the"]
            .map(str::to_owned)
            .into();
        let reference = "zorbla met quent the vantrix";
        // Two wrong words and one right word that a matcher would replace.
        let words = s(&["Zorble,", "met", "quent", "the", "vantrax", "drimble"]);
        let corrected = s(&["Zorbla,", "met", "Quint", "the", "Vantrix", "Blortz"]);
        let changes = [
            change(0, "Zorble,", "Zorbla"),
            change(2, "quent", "Quint"),
            change(4, "vantrax", "Vantrix"),
            change(5, "drimble", "Blortz"),
        ];
        let judged = judge(reference, &words, &corrected, &changes, &said);
        let effects: Vec<Verdict> = judged.iter().map(|j| j.effect).collect();
        // Each fix alone lowers the errors; a right word made wrong raises them; an inserted
        // word replaced by another wrong word changes nothing.
        assert_eq!(
            effects,
            [
                Verdict::Right,
                Verdict::Wrong,
                Verdict::Right,
                Verdict::NoEffect
            ]
        );
        let distractors: Vec<bool> = judged.iter().map(|j| j.distractor).collect();
        assert_eq!(distractors, [false, true, false, true]);
        let c = SuggestCounts::of(&judged);
        assert_eq!(
            c,
            SuggestCounts {
                suggestions: 4,
                right: 2,
                wrong: 1,
                no_effect: 1,
                distractor: 2,
            }
        );
        // Judging alone: the baseline words are never changed by an earlier suggestion.
        assert_eq!(words[0], "Zorble,");
    }

    #[test]
    fn vocab_suggest_judges_what_the_matcher_writes() {
        let common: HashSet<String> = ["met", "the"].map(str::to_owned).into();
        let said: HashSet<String> = ["bozzle", "met", "the"].map(str::to_owned).into();
        let words = s(&["Bozle", "met", "the"]);
        let (corrected, changes) = correct(&words, &s(&["Bozzle"]), Variant::SpellingRare, &common);
        let judged = judge("bozzle met the", &words, &corrected, &changes, &said);
        assert_eq!(judged.len(), 1);
        assert_eq!(judged[0].effect, Verdict::Right);
        assert!(!judged[0].distractor);
    }

    #[test]
    fn vocab_suggest_meeting_hours_sum_the_uem_spans() {
        let h = meeting_hours(&[(10.0, 1810.0), (0.0, 1800.0), (5.5, 1805.5)]);
        assert!((h - 1.5).abs() < 1e-12, "{h}");
        assert_eq!(meeting_hours(&[]), 0.0);
    }

    #[test]
    fn vocab_suggest_bar_needs_few_wrong_and_more_right_than_wrong() {
        let c = |right, wrong| SuggestCounts {
            suggestions: right + wrong,
            right,
            wrong,
            ..SuggestCounts::default()
        };
        // 2 hours: up to 10 wrong allowed.
        assert!(c(11, 10).passes_bar(2.0));
        assert!(!c(12, 11).passes_bar(2.0));
        assert!(!c(10, 10).passes_bar(2.0));
        assert!(c(1, 0).passes_bar(2.0));
        assert!(!c(0, 0).passes_bar(2.0));
        assert!(!c(1, 0).passes_bar(0.0));
    }
}
