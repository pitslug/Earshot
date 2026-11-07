//! Word error rate (WER) with substitution / deletion / insertion breakdown.
//!
//! Word-level Levenshtein alignment (unit costs). Ties between equally cheap alignments
//! are broken in the order match/substitution, deletion, insertion, as in common
//! reference tooling (for example `jiwer`/`sclite`); the total error count does not
//! depend on the tie-break.
//!
//! Corpus WER (PLAN.md 4.0) is total errors over total reference words, obtained by
//! summing per-utterance [`WerCounts`] with [`corpus_wer`], never by averaging rates.

use core::ops::{Add, AddAssign};

use super::normalize::normalize_english;

/// Error counts of one alignment (or a sum of alignments).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct WerCounts {
    /// Reference words replaced by a different hypothesis word.
    pub substitutions: usize,
    /// Reference words missing from the hypothesis.
    pub deletions: usize,
    /// Hypothesis words with no reference counterpart.
    pub insertions: usize,
    /// Number of reference words.
    pub ref_words: usize,
}

impl WerCounts {
    /// Total errors: S + D + I.
    pub fn errors(&self) -> usize {
        self.substitutions + self.deletions + self.insertions
    }

    /// (S + D + I) / N, or `None` when there are no reference words.
    /// Can exceed 1.0 when the hypothesis has many insertions.
    pub fn wer(&self) -> Option<f64> {
        (self.ref_words > 0).then(|| self.errors() as f64 / self.ref_words as f64)
    }
}

impl Add for WerCounts {
    type Output = WerCounts;
    fn add(self, o: WerCounts) -> WerCounts {
        WerCounts {
            substitutions: self.substitutions + o.substitutions,
            deletions: self.deletions + o.deletions,
            insertions: self.insertions + o.insertions,
            ref_words: self.ref_words + o.ref_words,
        }
    }
}

impl AddAssign for WerCounts {
    fn add_assign(&mut self, o: WerCounts) {
        *self = *self + o;
    }
}

/// Sums per-utterance counts; `.wer()` on the result is the corpus-level WER.
pub fn corpus_wer(counts: impl IntoIterator<Item = WerCounts>) -> WerCounts {
    counts.into_iter().fold(WerCounts::default(), Add::add)
}

/// Aligns two token sequences and counts S/D/I.
pub fn align_tokens<T: PartialEq>(reference: &[T], hypothesis: &[T]) -> WerCounts {
    let (n, m) = (reference.len(), hypothesis.len());
    // dp[i][j] = edit distance between reference[..i] and hypothesis[..j].
    let w = m + 1;
    let mut dp = vec![0usize; (n + 1) * w];
    for i in 0..=n {
        dp[i * w] = i;
    }
    for (j, cell) in dp.iter_mut().enumerate().take(m + 1) {
        *cell = j;
    }
    for i in 1..=n {
        for j in 1..=m {
            let sub = dp[(i - 1) * w + j - 1] + usize::from(reference[i - 1] != hypothesis[j - 1]);
            let del = dp[(i - 1) * w + j] + 1;
            let ins = dp[i * w + j - 1] + 1;
            dp[i * w + j] = sub.min(del).min(ins);
        }
    }
    // Backtrace.
    let mut c = WerCounts {
        ref_words: n,
        ..WerCounts::default()
    };
    let (mut i, mut j) = (n, m);
    while i > 0 || j > 0 {
        let here = dp[i * w + j];
        if i > 0 && j > 0 {
            let same = reference[i - 1] == hypothesis[j - 1];
            if dp[(i - 1) * w + j - 1] + usize::from(!same) == here {
                if !same {
                    c.substitutions += 1;
                }
                i -= 1;
                j -= 1;
                continue;
            }
        }
        if i > 0 && dp[(i - 1) * w + j] + 1 == here {
            c.deletions += 1;
            i -= 1;
        } else {
            c.insertions += 1;
            j -= 1;
        }
    }
    c
}

/// WER counts for whitespace-separated texts, compared verbatim (no normalisation).
pub fn word_errors(reference: &str, hypothesis: &str) -> WerCounts {
    let r: Vec<&str> = reference.split_whitespace().collect();
    let h: Vec<&str> = hypothesis.split_whitespace().collect();
    align_tokens(&r, &h)
}

/// WER counts after applying [`normalize_english`] to both texts (PLAN.md 4.0).
pub fn word_errors_normalized(reference: &str, hypothesis: &str) -> WerCounts {
    word_errors(
        &normalize_english(reference),
        &normalize_english(hypothesis),
    )
}
