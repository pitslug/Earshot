//! Decisions and action items caught: one-to-one matching of a summary's items against
//! annotated reference items (T6.20).
//!
//! The rule was fixed before any summary was scored and must not be changed after seeing
//! results (a different rule is a new metric with a new name):
//!
//! 1. Text to tokens: the ROUGE tokenizer ([`super::rouge::tokenize`]: lower case, runs of
//!    characters outside `[a-z0-9]` are separators), then every word of [`STOPWORDS`] is
//!    removed. What remains are the item's content tokens (a multiset).
//! 2. A reference item is one annotated sentence. Sentences with no content tokens, and the
//!    annotators' "nothing here" markers (content tokens exactly `na` or `none`), are not
//!    items.
//! 3. A summary item is the text of a decision, or the `task` of an action item (owner and
//!    due date are not compared: summaries name speakers as the transcript does, the
//!    annotations name roles).
//! 4. Pair score: token F1 of the two content-token multisets, `2 |A n B| / (|A| + |B|)`
//!    with multiset intersection.
//! 5. A pair can match only when its score is at least [`MATCH_F1`] (0.5). Matches are
//!    one-to-one and chosen to maximise their number, ties broken by the larger total score
//!    (maximum-weight assignment with weight `1 + F1` per eligible pair,
//!    [`super::hungarian::max_weight_assignment`]).
//! 6. Caught = number of matches. Recall = caught / reference items, precision = caught /
//!    summary items; across meetings the counts are summed before dividing.
//!
//! A second rule, [`ItemRule::ReferenceRecall`], was defined after seeing dev data (the owner,
//! 2026-10-25: the token-F1 rule misses matches that are right in meaning because summary
//! items are longer than the annotations) and is reported beside the first, never instead
//! of it; it must never be tuned on the test split. It differs only in rule 4 and 5: the
//! pair score is the share of the reference item's content tokens found in the summary
//! item, `|A n B| / |B|` (A summary, B reference, multiset intersection), and a pair can
//! match when it is at least [`MATCH_REFERENCE_RECALL`] (0.5). Tokens, stop words,
//! reference items, one-to-one maximum-count assignment (weight `1 + score`) and the
//! counts are those of rules 1-3 and 6.
//!
//! A third rule, [`covered_items`], was defined after seeing data (from a
//! hand check of ES2004c: one reference decision sentence bundles five decisions that a
//! summary lists as five items, so any one-to-one rule scores it 0 of 1). It is reported
//! beside the other two, never instead of them, and must never be tuned on the test split.
//! A reference item counts as caught when the multiset union of the content tokens of all
//! summary items of that list (decisions, or action-item tasks) holds at least
//! [`MATCH_COVERAGE`] (0.5) of its content tokens: `|U n B| / |B|`, U the union (per token
//! the largest count in any one summary item), B the reference item. Many-to-one: one
//! summary item may help cover several reference items, so only caught / reference is
//! meaningful (no precision). Tokens, stop words, reference items and summed counts are
//! those of rules 1-3 and 6.

use std::collections::BTreeMap;

use super::hungarian::max_weight_assignment;
use super::rouge::tokenize;

/// Smallest token F1 at which a summary item can match a reference item.
pub const MATCH_F1: f64 = 0.5;

/// Function words removed before matching (a fixed list; changing it changes the metric).
pub const STOPWORDS: &[&str] = &[
    "a", "about", "after", "again", "all", "also", "am", "an", "and", "any", "are", "as", "at",
    "be", "been", "before", "being", "both", "but", "by", "can", "could", "did", "do", "does",
    "doing", "for", "from", "further", "had", "has", "have", "having", "he", "her", "here", "hers",
    "him", "his", "how", "i", "if", "in", "into", "is", "it", "its", "itself", "just", "may", "me",
    "might", "more", "most", "must", "my", "no", "nor", "not", "now", "of", "off", "on", "once",
    "only", "or", "other", "our", "ours", "out", "over", "own", "s", "same", "shall", "she",
    "should", "so", "some", "such", "t", "than", "that", "the", "their", "theirs", "them", "then",
    "there", "these", "they", "this", "those", "through", "to", "too", "under", "until", "up",
    "very", "was", "we", "were", "what", "when", "where", "which", "while", "who", "whom", "why",
    "will", "with", "would", "you", "your", "yours",
];

/// The content tokens of `text` (rule 1), as a multiset.
#[must_use]
pub fn content_tokens(text: &str) -> BTreeMap<String, usize> {
    let mut out = BTreeMap::new();
    for t in tokenize(text) {
        if !STOPWORDS.contains(&t.as_str()) {
            *out.entry(t).or_insert(0) += 1;
        }
    }
    out
}

fn size(bag: &BTreeMap<String, usize>) -> usize {
    bag.values().sum()
}

/// Whether an annotated sentence is a reference item (rule 2).
#[must_use]
pub fn is_reference_item(sentence: &str) -> bool {
    let bag = content_tokens(sentence);
    let marker = bag.len() == 1 && bag.keys().all(|k| k == "na" || k == "none") && size(&bag) == 1;
    size(&bag) > 0 && !marker
}

/// Token F1 of two texts' content tokens (rule 4); 0 when either has none.
#[must_use]
pub fn token_f1(a: &str, b: &str) -> f64 {
    bag_f1(&content_tokens(a), &content_tokens(b))
}

fn bag_f1(a: &BTreeMap<String, usize>, b: &BTreeMap<String, usize>) -> f64 {
    let (na, nb) = (size(a), size(b));
    if na == 0 || nb == 0 {
        return 0.0;
    }
    let common: usize = a
        .iter()
        .map(|(t, &c)| c.min(b.get(t).copied().unwrap_or(0)))
        .sum();
    2.0 * common as f64 / (na + nb) as f64
}

/// Smallest share of a reference item's content tokens a summary item must contain to
/// match it under [`ItemRule::ReferenceRecall`].
pub const MATCH_REFERENCE_RECALL: f64 = 0.5;

/// Which pair score and threshold decide a match (module docs).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ItemRule {
    /// Content-token F1 >= [`MATCH_F1`] (rules 1-6, fixed before measuring).
    TokenF1,
    /// Share of the reference item's content tokens in the summary item >=
    /// [`MATCH_REFERENCE_RECALL`] (defined after seeing dev data).
    ReferenceRecall,
}

impl ItemRule {
    fn score(self, summary: &BTreeMap<String, usize>, reference: &BTreeMap<String, usize>) -> f64 {
        match self {
            ItemRule::TokenF1 => bag_f1(summary, reference),
            ItemRule::ReferenceRecall => {
                let nr = size(reference);
                if nr == 0 || size(summary) == 0 {
                    return 0.0;
                }
                common(summary, reference) as f64 / nr as f64
            }
        }
    }

    fn threshold(self) -> f64 {
        match self {
            ItemRule::TokenF1 => MATCH_F1,
            ItemRule::ReferenceRecall => MATCH_REFERENCE_RECALL,
        }
    }
}

fn common(a: &BTreeMap<String, usize>, b: &BTreeMap<String, usize>) -> usize {
    a.iter()
        .map(|(t, &c)| c.min(b.get(t).copied().unwrap_or(0)))
        .sum()
}

/// One match: indices into the summary items and the reference items, and the pair score.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ItemMatch {
    /// Index into the summary items.
    pub summary: usize,
    /// Index into the reference items.
    pub reference: usize,
    /// The pair score under the rule used (token F1 or reference recall).
    pub score: f64,
}

/// Counts of one comparison (or a sum over meetings).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ItemCounts {
    /// Matched pairs.
    pub caught: usize,
    /// Reference items (rule 2).
    pub reference: usize,
    /// Summary items.
    pub summary: usize,
}

impl ItemCounts {
    /// caught / reference, `None` without reference items.
    #[must_use]
    pub fn recall(&self) -> Option<f64> {
        (self.reference > 0).then(|| self.caught as f64 / self.reference as f64)
    }

    /// caught / summary, `None` without summary items.
    #[must_use]
    pub fn precision(&self) -> Option<f64> {
        (self.summary > 0).then(|| self.caught as f64 / self.summary as f64)
    }
}

impl core::ops::AddAssign for ItemCounts {
    fn add_assign(&mut self, o: Self) {
        self.caught += o.caught;
        self.reference += o.reference;
        self.summary += o.summary;
    }
}

/// The matching of rules 2-6: `summary` item texts against annotated `reference`
/// sentences (non-items are dropped first; indices in the result are into the filtered
/// reference list, see [`reference_items`]).
#[must_use]
pub fn match_items(summary: &[&str], reference: &[&str]) -> (ItemCounts, Vec<ItemMatch>) {
    match_items_by(ItemRule::TokenF1, summary, reference)
}

/// [`match_items`] under `rule`.
#[must_use]
pub fn match_items_by(
    rule: ItemRule,
    summary: &[&str],
    reference: &[&str],
) -> (ItemCounts, Vec<ItemMatch>) {
    let refs = reference_items(reference);
    let s_bags: Vec<_> = summary.iter().map(|s| content_tokens(s)).collect();
    let r_bags: Vec<_> = refs.iter().map(|r| content_tokens(r)).collect();
    let threshold = rule.threshold();
    let mut matches = Vec::new();
    if !s_bags.is_empty() && !r_bags.is_empty() {
        let scores: Vec<Vec<f64>> = s_bags
            .iter()
            .map(|s| r_bags.iter().map(|r| rule.score(s, r)).collect())
            .collect();
        let weights: Vec<Vec<f64>> = scores
            .iter()
            .map(|row| {
                row.iter()
                    .map(|&x| if x >= threshold { 1.0 + x } else { 0.0 })
                    .collect()
            })
            .collect();
        for (i, col) in max_weight_assignment(&weights).into_iter().enumerate() {
            if let Some(j) = col
                && scores[i][j] >= threshold
            {
                matches.push(ItemMatch {
                    summary: i,
                    reference: j,
                    score: scores[i][j],
                });
            }
        }
    }
    let counts = ItemCounts {
        caught: matches.len(),
        reference: refs.len(),
        summary: summary.len(),
    };
    (counts, matches)
}

/// Smallest share of a reference item's content tokens that the union of a list's summary
/// items must hold for [`covered_items`] (defined after seeing data).
pub const MATCH_COVERAGE: f64 = 0.5;

/// One reference item caught by [`covered_items`] and its covered share.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ItemCoverage {
    /// Index into the reference items.
    pub reference: usize,
    /// Share of its content tokens in the union of the summary items.
    pub score: f64,
}

/// The coverage rule (module docs; defined after seeing data): every reference item whose
/// content tokens the union of all `summary` items covers to at least [`MATCH_COVERAGE`].
/// `ItemCounts::caught` counts covered reference items; indices are into the filtered
/// reference list ([`reference_items`]).
#[must_use]
pub fn covered_items(summary: &[&str], reference: &[&str]) -> (ItemCounts, Vec<ItemCoverage>) {
    let refs = reference_items(reference);
    let mut union: BTreeMap<String, usize> = BTreeMap::new();
    for s in summary {
        for (t, c) in content_tokens(s) {
            let u = union.entry(t).or_insert(0);
            *u = (*u).max(c);
        }
    }
    let mut covered = Vec::new();
    for (j, r) in refs.iter().enumerate() {
        let bag = content_tokens(r);
        let n = size(&bag);
        if n == 0 || union.is_empty() {
            continue;
        }
        let score = common(&union, &bag) as f64 / n as f64;
        if score >= MATCH_COVERAGE {
            covered.push(ItemCoverage {
                reference: j,
                score,
            });
        }
    }
    let counts = ItemCounts {
        caught: covered.len(),
        reference: refs.len(),
        summary: summary.len(),
    };
    (counts, covered)
}

/// The annotated sentences that are reference items (rule 2), in order.
#[must_use]
pub fn reference_items<'a>(sentences: &[&'a str]) -> Vec<&'a str> {
    sentences
        .iter()
        .copied()
        .filter(|s| is_reference_item(s))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn items_coverage_catches_a_bundled_reference_split_into_several_items() {
        // ES2004c-like: one annotated sentence bundles several decisions.
        let reference = [
            "The remote will have an LCD screen, rubber buttons, coloured                           rubber covers and a kinetic battery.",
        ];
        let summary = [
            "Use an LCD screen.",
            "Buttons are made of rubber.",
            "Offer coloured covers.",
            "Power it with a kinetic battery.",
        ];
        let (strict, _) = match_items(&summary, &reference);
        let (rr, _) = match_items_by(ItemRule::ReferenceRecall, &summary, &reference);
        assert_eq!(
            (strict.caught, rr.caught),
            (0, 0),
            "one-to-one rules miss it"
        );
        let (cov, hits) = covered_items(&summary, &reference);
        assert_eq!((cov.caught, cov.reference, cov.summary), (1, 1, 4));
        // remote lcd screen rubber buttons coloured rubber covers kinetic battery: the
        // union holds lcd screen buttons rubber coloured covers kinetic battery (rubber
        // once: the union keeps the largest count in one item), 8 of 10.
        assert!((hits[0].score - 0.8).abs() < 1e-12, "{hits:?}");
        // The threshold binds: covering fewer than half is not caught.
        let (few, _) = covered_items(&["Use an LCD screen."], &reference);
        assert_eq!(few.caught, 0);
        // Nothing in the summary list catches nothing; non-items are not counted.
        let (none, _) = covered_items(&[], &reference);
        assert_eq!((none.caught, none.reference), (0, 1));
        let (marker, _) = covered_items(&summary, &["None."]);
        assert_eq!((marker.caught, marker.reference), (0, 0));
        // Many-to-one: one summary item may cover two reference items.
        let (two, _) = covered_items(
            &["Ship the beta on Friday with the release notes"],
            &["Ship the beta Friday.", "Release notes ship Friday."],
        );
        assert_eq!(two.caught, 2);
    }

    #[test]
    fn items_markers_and_empty_sentences_are_not_reference_items() {
        for s in ["NA.", "na", "None.", "N/A", "", "The.", " -- "] {
            // "N/A" tokenises to "n" "a"; "a" is a stopword, "n" is content: an item.
            assert_eq!(is_reference_item(s), s == "N/A", "{s:?}");
        }
        assert!(is_reference_item("The remote will have an LCD screen."));
        assert!(is_reference_item("None of the buttons will glow."));
    }

    #[test]
    fn items_token_f1_ignores_stopwords_and_case() {
        assert_eq!(token_f1("The LCD screen", "an lcd SCREEN"), 1.0);
        // {remote, made, rubber} vs {use, rubber, remote, casing}: 2 common of 3 + 4.
        let f = token_f1(
            "The remote will be made of rubber.",
            "Use rubber for the remote casing",
        );
        assert!((f - 4.0 / 7.0).abs() < 1e-12, "{f}");
        assert_eq!(token_f1("the of", "lcd"), 0.0);
        // Multiset: a repeated word counts once per occurrence on both sides.
        assert!((token_f1("blue blue red", "blue red red") - 2.0 * 2.0 / 6.0).abs() < 1e-12);
    }

    #[test]
    fn items_matching_is_one_to_one_and_maximises_the_number_caught() {
        // Summary 0 fits reference 0 best (F1 0.8) and reference 1 at the threshold (0.5);
        // summary 1 fits only reference 0 (0.5). Greedy by score would pair 0-0 and leave
        // 1 unmatched; the rule pairs 0-1 and 1-0.
        let summary = ["lcd screen rubber", "lcd yellow"];
        let reference = ["An LCD screen.", "Rubber."];
        let (c, m) = match_items(&summary, &reference);
        assert_eq!(
            c,
            ItemCounts {
                caught: 2,
                reference: 2,
                summary: 2
            }
        );
        let mut pairs: Vec<(usize, usize)> = m.iter().map(|x| (x.summary, x.reference)).collect();
        pairs.sort_unstable();
        assert_eq!(pairs, [(0, 1), (1, 0)]);
    }

    #[test]
    fn items_below_the_threshold_never_match() {
        // {prepare, functional, design} vs a long sentence: F1 below 0.5.
        let (c, m) = match_items(
            &["Prepare the functional design"],
            &[
                "The group will prepare for the functional design meeting to discuss the \
               components and functions of the product and the budget.",
            ],
        );
        assert_eq!(c.caught, 0);
        assert!(m.is_empty());
        assert_eq!((c.reference, c.summary), (1, 1));
        // Exactly at the threshold matches: {a1 a2} vs {a1 b1}: F1 = 0.5.
        let (c, _) = match_items(&["alpha beta"], &["alpha gamma"]);
        assert_eq!(c.caught, 1);
    }

    #[test]
    fn items_reference_recall_catches_long_paraphrases_the_f1_rule_misses() {
        // Real dev pair (Qwen3 8B, TS3003c): F1 0.47, but every reference content token
        // ({remote, changeable, case, covers}) is in the summary item.
        let sys = "The team agreed to proceed with the concept of changeable case covers for \
                   the remote control, allowing for different colors and styles.";
        let reference = "The remote will have changeable case covers.";
        assert_eq!(match_items(&[sys], &[reference]).0.caught, 0);
        let (c, m) = match_items_by(ItemRule::ReferenceRecall, &[sys], &[reference]);
        assert_eq!(c.caught, 1);
        assert_eq!(m[0].score, 1.0);
        // Half the reference tokens is enough, fewer is not: {remote, lcd, screen, speech,
        // recognition} against items holding three of them, then one.
        let r = "The remote will have an LCD screen and speech recognition.";
        let half = match_items_by(
            ItemRule::ReferenceRecall,
            &["LCD screen on the remote"],
            &[r],
        );
        assert_eq!(half.0.caught, 1);
        let quarter = match_items_by(ItemRule::ReferenceRecall, &["Add a screen"], &[r]);
        assert_eq!(quarter.0.caught, 0);
        // Exactly half matches.
        let exact = match_items_by(
            ItemRule::ReferenceRecall,
            &["alpha beta"],
            &["Alpha beta gamma delta."],
        );
        assert_eq!((exact.0.caught, exact.1[0].score), (1, 0.5));
        // Not symmetric: a short summary item inside a long reference is not caught.
        let long_ref = "The group will prepare the functional design, the budget, the \
                        components and the user interface for the next meeting.";
        assert_eq!(
            match_items_by(
                ItemRule::ReferenceRecall,
                &["Prepare the budget"],
                &[long_ref]
            )
            .0
            .caught,
            0
        );
        // Same one-to-one assignment and markers as the F1 rule.
        let (c, _) = match_items_by(
            ItemRule::ReferenceRecall,
            &["lcd screen remote"],
            &["An LCD screen.", "LCD screen on the remote.", "NA."],
        );
        assert_eq!((c.caught, c.reference, c.summary), (1, 2, 1));
    }

    #[test]
    fn items_counts_handle_empty_sides() {
        let (c, _) = match_items(&[], &["NA."]);
        assert_eq!(c, ItemCounts::default());
        assert_eq!(c.recall(), None);
        assert_eq!(c.precision(), None);
        let (c, _) = match_items(&["Ship it"], &["NA."]);
        assert_eq!((c.caught, c.reference, c.summary), (0, 0, 1));
        assert_eq!(c.precision(), Some(0.0));
        let mut total = c;
        total += ItemCounts {
            caught: 1,
            reference: 2,
            summary: 1,
        };
        assert_eq!(total.recall(), Some(0.5));
        assert_eq!(total.precision(), Some(0.5));
    }
}
