//! ROUGE-N, ROUGE-L and ROUGE-Lsum (Lin 2004, "ROUGE: A Package for Automatic Evaluation of
//! Summaries", Text Summarization Branches Out workshop, ACL 2004).
//!
//! A port of Google's `rouge-score` package (google-research/google-research, `rouge/`, at
//! commit 0b7b841b4d58aac0854fd6daa3f80ea2724d14b3: `rouge_scorer.py`, `tokenize.py`,
//! `scoring.py`) with `use_stemmer=False` and `split_summaries=False`, the defaults of
//! `rouge_scorer.RougeScorer` and of Hugging Face `evaluate`'s `rouge` metric. Expected
//! values in `metrics/rouge_reference.rs` come from running that upstream code, never from
//! this crate.
//!
//! - Tokens: the text lower-cased (Unicode), every run of characters outside `[a-z0-9]`
//!   replaced by a space, split on whitespace, empty tokens dropped (`tokenize.tokenize`).
//! - ROUGE-N: clipped n-gram overlap; precision over the prediction's n-grams, recall over
//!   the target's, each denominator at least 1 (`_score_ngrams`).
//! - ROUGE-L: the longest common subsequence of the two whole token lists; all zero when
//!   either is empty (`_score_lcs`).
//! - ROUGE-Lsum: summary-level LCS (section 3.2 of the paper): texts split into sentences at
//!   `\n` (empty lines dropped), for each target sentence the union of one LCS against
//!   every prediction sentence, hits counted at most as often as the token occurs in both
//!   texts, as ROUGE-1.5.5 does (`_summary_level_lcs`, including its backtracking
//!   tie-break, which decides which LCS joins the union).
//! - F-measure: `2PR / (P + R)`, 0 when `P + R = 0` (`scoring.fmeasure`).

use std::collections::{BTreeMap, BTreeSet};

/// Precision, recall and F-measure of one ROUGE comparison.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct RougeScore {
    /// Share of the prediction found in the target.
    pub precision: f64,
    /// Share of the target found in the prediction.
    pub recall: f64,
    /// Harmonic mean of the two.
    pub fmeasure: f64,
}

impl RougeScore {
    fn new(precision: f64, recall: f64) -> Self {
        RougeScore {
            precision,
            recall,
            fmeasure: fmeasure(precision, recall),
        }
    }
}

/// The ROUGE types this module computes, all for one target/prediction pair.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct RougeScores {
    /// Unigram overlap.
    pub rouge1: RougeScore,
    /// Bigram overlap.
    pub rouge2: RougeScore,
    /// Whole-text longest common subsequence.
    pub rouge_l: RougeScore,
    /// Summary-level LCS over newline-separated sentences.
    pub rouge_lsum: RougeScore,
}

/// `scoring.fmeasure`.
fn fmeasure(precision: f64, recall: f64) -> f64 {
    if precision + recall > 0.0 {
        2.0 * precision * recall / (precision + recall)
    } else {
        0.0
    }
}

/// `tokenize.tokenize` without a stemmer.
#[must_use]
pub fn tokenize(text: &str) -> Vec<String> {
    let lower = text.to_lowercase();
    let mapped: String = lower
        .chars()
        .map(|c| {
            if c.is_ascii_lowercase() || c.is_ascii_digit() {
                c
            } else {
                ' '
            }
        })
        .collect();
    mapped.split_whitespace().map(str::to_owned).collect()
}

/// All four scores of `prediction` against `target` (the upstream argument order is
/// `score(target, prediction)`).
#[must_use]
pub fn rouge(target: &str, prediction: &str) -> RougeScores {
    let t = tokenize(target);
    let p = tokenize(prediction);
    RougeScores {
        rouge1: rouge_n(&t, &p, 1),
        rouge2: rouge_n(&t, &p, 2),
        rouge_l: rouge_l(&t, &p),
        rouge_lsum: rouge_lsum(target, prediction),
    }
}

fn ngrams(tokens: &[String], n: usize) -> BTreeMap<&[String], usize> {
    let mut counts = BTreeMap::new();
    if tokens.len() >= n {
        for w in tokens.windows(n) {
            *counts.entry(w).or_insert(0) += 1;
        }
    }
    counts
}

/// ROUGE-N over token lists (`_create_ngrams` + `_score_ngrams`).
///
/// # Panics
/// When `n` is 0.
#[must_use]
pub fn rouge_n(target: &[String], prediction: &[String], n: usize) -> RougeScore {
    assert!(n > 0, "rouge-n needs n >= 1");
    let t = ngrams(target, n);
    let p = ngrams(prediction, n);
    let overlap: usize = t
        .iter()
        .map(|(g, &c)| c.min(p.get(g).copied().unwrap_or(0)))
        .sum();
    let t_total: usize = t.values().sum();
    let p_total: usize = p.values().sum();
    RougeScore::new(
        overlap as f64 / p_total.max(1) as f64,
        overlap as f64 / t_total.max(1) as f64,
    )
}

/// `_lcs_table`: `(rows + 1) x (cols + 1)` LCS lengths.
fn lcs_table(r: &[String], c: &[String]) -> Vec<Vec<usize>> {
    let mut t = vec![vec![0usize; c.len() + 1]; r.len() + 1];
    for i in 1..=r.len() {
        for j in 1..=c.len() {
            t[i][j] = if r[i - 1] == c[j - 1] {
                t[i - 1][j - 1] + 1
            } else {
                t[i - 1][j].max(t[i][j - 1])
            };
        }
    }
    t
}

/// ROUGE-L over token lists (`_score_lcs`).
#[must_use]
pub fn rouge_l(target: &[String], prediction: &[String]) -> RougeScore {
    if target.is_empty() || prediction.is_empty() {
        return RougeScore::default();
    }
    let lcs = lcs_table(target, prediction)[target.len()][prediction.len()];
    RougeScore::new(
        lcs as f64 / prediction.len() as f64,
        lcs as f64 / target.len() as f64,
    )
}

/// `_backtrack_norec`: the indices into `r` of one LCS, with upstream's tie-break.
fn lcs_indices(r: &[String], c: &[String]) -> Vec<usize> {
    let t = lcs_table(r, c);
    let (mut i, mut j) = (r.len(), c.len());
    let mut out = Vec::new();
    while i > 0 && j > 0 {
        if r[i - 1] == c[j - 1] {
            out.push(i - 1);
            i -= 1;
            j -= 1;
        } else if t[i][j - 1] > t[i - 1][j] {
            j -= 1;
        } else {
            i -= 1;
        }
    }
    out.reverse();
    out
}

/// Non-empty lines of `text`, tokenised (`get_sents` with `split_summaries=False`). A line
/// with no token stays in the list as an empty sentence, as upstream keeps it.
fn sentences(text: &str) -> Vec<Vec<String>> {
    text.split('\n')
        .filter(|s| !s.is_empty())
        .map(tokenize)
        .collect()
}

/// ROUGE-Lsum of two newline-separated texts (`_summary_level_lcs`).
#[must_use]
pub fn rouge_lsum(target: &str, prediction: &str) -> RougeScore {
    let refs = sentences(target);
    let cands = sentences(prediction);
    if refs.is_empty() || cands.is_empty() {
        return RougeScore::default();
    }
    let m: usize = refs.iter().map(Vec::len).sum();
    let n: usize = cands.iter().map(Vec::len).sum();
    if m == 0 || n == 0 {
        return RougeScore::default();
    }
    let mut count_r: BTreeMap<&str, usize> = BTreeMap::new();
    let mut count_c: BTreeMap<&str, usize> = BTreeMap::new();
    for t in refs.iter().flatten() {
        *count_r.entry(t).or_insert(0) += 1;
    }
    for t in cands.iter().flatten() {
        *count_c.entry(t).or_insert(0) += 1;
    }
    let mut hits = 0usize;
    for r in &refs {
        let union: BTreeSet<usize> = cands.iter().flat_map(|c| lcs_indices(r, c)).collect();
        for i in union {
            let tok = r[i].as_str();
            let (cr, cc) = (count_r.get(tok).copied(), count_c.get(tok).copied());
            if let (Some(cr), Some(cc)) = (cr, cc)
                && cr > 0
                && cc > 0
            {
                hits += 1;
                count_r.insert(tok, cr - 1);
                count_c.insert(tok, cc - 1);
            }
        }
    }
    RougeScore::new(hits as f64 / n as f64, hits as f64 / m as f64)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn toks(s: &str) -> Vec<String> {
        tokenize(s)
    }

    #[test]
    fn rouge_tokenizer_keeps_ascii_alphanumerics_only() {
        assert_eq!(
            toks("The LCD-screen's 3 buttons, caf\u{e9}!"),
            ["the", "lcd", "screen", "s", "3", "buttons", "caf"]
        );
        assert!(toks(" \n-- ").is_empty());
    }

    #[test]
    fn rouge_identical_texts_score_one_and_disjoint_texts_zero() {
        let s = rouge("a b c\nd e", "a b c\nd e");
        for x in [s.rouge1, s.rouge2, s.rouge_l, s.rouge_lsum] {
            assert_eq!(x, RougeScore::new(1.0, 1.0));
        }
        let z = rouge("a b", "c d");
        assert_eq!(z.rouge_l.fmeasure, 0.0);
        assert_eq!(z.rouge1.fmeasure, 0.0);
    }

    #[test]
    fn rouge_empty_inputs_score_zero() {
        assert_eq!(rouge("", "a").rouge_l, RougeScore::default());
        assert_eq!(rouge("a", "").rouge_lsum, RougeScore::default());
        assert_eq!(rouge("--", "a").rouge1, RougeScore::default());
    }

    #[test]
    fn rouge_l_is_the_lcs_ratio() {
        // LCS of "a b c d" and "a c x d" is "a c d" (3).
        let s = rouge_l(&toks("a b c d"), &toks("a c x d"));
        assert_eq!(s, RougeScore::new(0.75, 0.75));
    }
}
