//! The metrics of docs/vocabulary.md: WER, B-WER and U-WER (Le et al. 2021), listed recall,
//! distractor hits and changed words against a baseline.

use std::collections::HashSet;
use std::ops::AddAssign;

use serde::Serialize;

/// What became of one reference position.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fate {
    Correct,
    Substituted,
    Deleted,
}

/// A word alignment: the fate of every reference position and the inserted hypothesis
/// positions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Alignment {
    pub fates: Vec<Fate>,
    pub inserted: Vec<usize>,
}

/// Word Levenshtein alignment with `metrics::wer::align_tokens`'s costs and tie-break
/// (match/substitution, deletion, insertion), keeping the path.
pub fn align(reference: &[&str], hypothesis: &[&str]) -> Alignment {
    let (n, m) = (reference.len(), hypothesis.len());
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
    let mut fates = vec![Fate::Deleted; n];
    let mut inserted = Vec::new();
    let (mut i, mut j) = (n, m);
    while i > 0 || j > 0 {
        let here = dp[i * w + j];
        if i > 0 && j > 0 {
            let same = reference[i - 1] == hypothesis[j - 1];
            if dp[(i - 1) * w + j - 1] + usize::from(!same) == here {
                fates[i - 1] = if same {
                    Fate::Correct
                } else {
                    Fate::Substituted
                };
                i -= 1;
                j -= 1;
                continue;
            }
        }
        if i > 0 && dp[(i - 1) * w + j] + 1 == here {
            fates[i - 1] = Fate::Deleted;
            i -= 1;
        } else {
            inserted.push(j - 1);
            j -= 1;
        }
    }
    inserted.reverse();
    Alignment { fates, inserted }
}

/// Counts of one decode (summed over utterances).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct Counts {
    /// Reference words.
    pub ref_words: usize,
    /// All word errors (S + D + I).
    pub errors: usize,
    /// Reference positions of listed (spoken) words.
    pub listed_words: usize,
    /// B-WER numerator: listed positions not correct, plus inserted listed tokens.
    pub listed_errors: usize,
    /// Listed positions aligned to an equal token.
    pub listed_correct: usize,
    /// Other reference positions.
    pub unlisted_words: usize,
    /// U-WER numerator.
    pub unlisted_errors: usize,
    /// Hypothesis tokens that are distractors.
    pub distractor_hits: usize,
    /// Positions correct in the baseline and wrong here, listed and unlisted, and the reverse.
    pub listed_lost: usize,
    pub listed_gained: usize,
    pub unlisted_lost: usize,
    pub unlisted_gained: usize,
}

impl AddAssign for Counts {
    fn add_assign(&mut self, o: Counts) {
        self.ref_words += o.ref_words;
        self.errors += o.errors;
        self.listed_words += o.listed_words;
        self.listed_errors += o.listed_errors;
        self.listed_correct += o.listed_correct;
        self.unlisted_words += o.unlisted_words;
        self.unlisted_errors += o.unlisted_errors;
        self.distractor_hits += o.distractor_hits;
        self.listed_lost += o.listed_lost;
        self.listed_gained += o.listed_gained;
        self.unlisted_lost += o.unlisted_lost;
        self.unlisted_gained += o.unlisted_gained;
    }
}

fn percent(num: usize, den: usize) -> Option<f64> {
    #[allow(clippy::cast_precision_loss)]
    (den > 0).then(|| 100.0 * num as f64 / den as f64)
}

impl Counts {
    pub fn wer(&self) -> Option<f64> {
        percent(self.errors, self.ref_words)
    }
    pub fn b_wer(&self) -> Option<f64> {
        percent(self.listed_errors, self.listed_words)
    }
    pub fn u_wer(&self) -> Option<f64> {
        percent(self.unlisted_errors, self.unlisted_words)
    }
    pub fn listed_recall(&self) -> Option<f64> {
        percent(self.listed_correct, self.listed_words)
    }
    /// Distractor hits per 1,000 reference words.
    pub fn distractors_per_1000(&self) -> Option<f64> {
        #[allow(clippy::cast_precision_loss)]
        (self.ref_words > 0).then(|| 1000.0 * self.distractor_hits as f64 / self.ref_words as f64)
    }
}

/// Scores one utterance: `reference` and `hypothesis` are normalised tokens, `listed` the
/// utterance's listed tokens, `baseline` the baseline's alignment of the same reference (for
/// lost and gained words; `None` for the baseline itself).
pub fn score(
    reference: &[&str],
    hypothesis: &[&str],
    listed: &HashSet<&str>,
    baseline: Option<&Alignment>,
) -> (Counts, Alignment) {
    let said: HashSet<&str> = reference.iter().copied().collect();
    score_in(reference, hypothesis, listed, &said, baseline)
}

/// [`score`] with the distractors taken against `said` (a listed token not in it is a
/// distractor) instead of the reference: a meeting's list is judged against every word of
/// the meeting (T12.15c).
pub fn score_in(
    reference: &[&str],
    hypothesis: &[&str],
    listed: &HashSet<&str>,
    said: &HashSet<&str>,
    baseline: Option<&Alignment>,
) -> (Counts, Alignment) {
    let a = align(reference, hypothesis);
    let mut c = Counts {
        ref_words: reference.len(),
        ..Counts::default()
    };
    for (i, (&r, &fate)) in reference.iter().zip(&a.fates).enumerate() {
        let is_listed = listed.contains(r);
        let wrong = fate != Fate::Correct;
        if is_listed {
            c.listed_words += 1;
            c.listed_errors += usize::from(wrong);
            c.listed_correct += usize::from(!wrong);
        } else {
            c.unlisted_words += 1;
            c.unlisted_errors += usize::from(wrong);
        }
        c.errors += usize::from(wrong);
        if let Some(b) = baseline {
            let before = b.fates[i] == Fate::Correct;
            let (lost, gained) = (before && wrong, !before && !wrong);
            if is_listed {
                c.listed_lost += usize::from(lost);
                c.listed_gained += usize::from(gained);
            } else {
                c.unlisted_lost += usize::from(lost);
                c.unlisted_gained += usize::from(gained);
            }
        }
    }
    for &j in &a.inserted {
        c.errors += 1;
        if listed.contains(hypothesis[j]) {
            c.listed_errors += 1;
        } else {
            c.unlisted_errors += 1;
        }
    }
    c.distractor_hits = hypothesis
        .iter()
        .filter(|h| listed.contains(**h) && !said.contains(**h))
        .count();
    (c, a)
}

#[cfg(test)]
mod tests {
    use super::*;
    use earshot_core::metrics::wer::align_tokens;

    fn w(s: &str) -> Vec<&str> {
        s.split_whitespace().collect()
    }

    #[test]
    fn vocab_alignment_counts_equal_the_wer_module() {
        let cases = [
            ("a b c d", "a x c d e"),
            ("a b c", ""),
            ("", "a b"),
            ("the cat sat on the mat", "the bat sat the mat mat"),
            ("a a b b", "b b a a"),
        ];
        for (r, h) in cases {
            let (r, h) = (w(r), w(h));
            let a = align(&r, &h);
            let want = align_tokens(&r, &h);
            let subs = a.fates.iter().filter(|f| **f == Fate::Substituted).count();
            let dels = a.fates.iter().filter(|f| **f == Fate::Deleted).count();
            assert_eq!(
                (subs, dels, a.inserted.len()),
                (want.substitutions, want.deletions, want.insertions),
                "{r:?} / {h:?}"
            );
        }
    }

    #[test]
    fn vocab_scores_split_listed_and_unlisted_words() {
        let listed: HashSet<&str> = ["bozzle", "quixote", "ursus"].into();
        let r = w("old bozzle met quixote");
        let base = w("old bottle met quixote");
        let (b, ba) = score(&r, &base, &listed, None);
        assert_eq!(
            (b.listed_words, b.listed_errors, b.listed_correct),
            (2, 1, 1)
        );
        assert_eq!((b.unlisted_words, b.unlisted_errors), (2, 0));
        assert_eq!(b.distractor_hits, 0);
        // The method fixes bozzle, writes the distractor ursus, and loses "met".
        let hyp = w("old bozzle ursus quixote");
        let (c, _) = score(&r, &hyp, &listed, Some(&ba));
        assert_eq!((c.listed_errors, c.listed_correct), (0, 2));
        assert_eq!(c.unlisted_errors, 1);
        assert_eq!(c.distractor_hits, 1);
        assert_eq!((c.listed_gained, c.listed_lost), (1, 0));
        assert_eq!((c.unlisted_gained, c.unlisted_lost), (0, 1));
        assert_eq!(c.errors, 1);
        // An inserted listed token is a B-WER error.
        let (d, _) = score(&r, &w("old bozzle met quixote ursus"), &listed, Some(&ba));
        assert_eq!((d.listed_errors, d.unlisted_errors, d.errors), (1, 0, 1));
        let mut sum = Counts::default();
        sum += b;
        sum += c;
        assert_eq!(sum.ref_words, 8);
        assert_eq!(sum.b_wer(), Some(25.0));
        assert_eq!(Counts::default().wer(), None);
    }

    #[test]
    fn vocab_meeting_scores_take_distractors_against_the_whole_meeting() {
        let listed: HashSet<&str> = ["bozzle", "ursus"].into();
        let said: HashSet<&str> = ["old", "met", "bozzle"].into();
        // bozzle is said elsewhere in the meeting: an inserted listed word, not a distractor.
        let (c, _) = score_in(
            &w("old met"),
            &w("old bozzle met ursus"),
            &listed,
            &said,
            None,
        );
        assert_eq!(c.distractor_hits, 1);
        assert_eq!((c.listed_errors, c.errors), (2, 2));
        // With the reference alone both would be distractors.
        let (d, _) = score(&w("old met"), &w("old bozzle met ursus"), &listed, None);
        assert_eq!(d.distractor_hits, 2);
    }
}
