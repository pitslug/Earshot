//! LocalAgreement-2 stabilisation of streaming hypotheses.
//!
//! Source: D. Machacek, R. Dabre, O. Bojar, "Turning Whisper into Real-Time Transcription
//! System", IJCNLP-AACL 2023 (system demonstrations), and its reference implementation
//! `HypothesisBuffer` in <https://github.com/ufal/whisper_streaming>
//! (`whisper_online.py`). The audio window is re-decoded as it grows; a word is committed
//! once two consecutive hypotheses agree on it, i.e. the committed text grows by the
//! longest common prefix of the last two hypotheses. Committed words never change.
//!
//! Removing already committed words from a new hypothesis (`HypothesisBuffer.insert`):
//! the reference drops words starting more than 100 ms before the end of the last
//! committed word, then removes a 1-5 word n-gram overlap with the committed tail when the
//! hypothesis starts within 1 s of it. whisper's word times move by 100 ms and more
//! between decoding contexts, so a pure time rule can drop a word that was never
//! committed. Here text anchors come first, and the time rule is only the fallback:
//! 1. **Committed anchor**: the longest suffix (up to 5 words) of the committed words that
//!    occurs in the hypothesis among the words starting within 1 s of the committed end;
//!    everything up to and including its earliest occurrence is dropped.
//! 2. **Untimed committed anchor**: otherwise the longest suffix of at least 3 words that
//!    occurs in the hypothesis starting at an untimed word (earliest occurrence).
//!    whisper.cpp sometimes gives the last words of a decode the buffer-end time
//!    (start == end, "untimed": the time says nothing about where the word was spoken);
//!    committed with those times, they push the committed end forward, and the next decode
//!    moves the same words to its own buffer end, more than 1 s later, out of reach of
//!    rule 1 (T2.4b). A timed word more than 1 s after the committed end is new speech
//!    even when it repeats committed words (a refrain), so it never anchors.
//! 3. **Tentative anchor**: otherwise, if the first uncommitted word of the previous
//!    hypothesis occurs among the words starting within 1 s of the committed end, and not
//!    more than 1 s before the start of the last committed word (so a common word at the
//!    window start never re-emits the committed words after it; the start, not the end, is
//!    the reference because a collapsed end can lie seconds after the speech), everything
//!    before it is dropped.
//! 4. Otherwise the reference time rule (start > last committed end - 100 ms).
//!
//! Words are compared after lower-casing and dropping punctuation (whisper flips "hello"
//! and "Hello," between runs). Words that normalise to nothing (punctuation only) never
//! match anything.

use std::time::Duration;

use super::types::{MeetingTime, Word};

/// Fallback time rule: words starting up to this long before the last committed end count
/// as committed.
const DUPLICATE_SLACK: Duration = Duration::from_millis(100);
/// Anchors are searched among words starting before the committed end plus this.
const ANCHOR_WINDOW: Duration = Duration::from_secs(1);
/// Longest committed suffix used as an anchor (and committed words kept for it).
const MAX_NGRAM: usize = 5;
/// Shortest committed suffix matched at untimed words (rule 2 of the module docs).
const MIN_UNBOUNDED_NGRAM: usize = 3;

/// A word whose time whisper.cpp collapsed to a point (start == end), typically the
/// buffer end: no evidence of where it was spoken.
fn untimed(w: &Word) -> bool {
    w.start == w.end
}

/// LocalAgreement-2 state over a sliding audio window.
#[derive(Debug, Clone, Default)]
pub struct LocalAgreement {
    previous: Vec<Word>,
    history: Vec<Word>,
    last_committed_end: Option<MeetingTime>,
}

impl LocalAgreement {
    /// Empty state.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Drops committed-word history and the previous hypothesis. Used at an utterance
    /// boundary, where windows do not overlap.
    pub fn clear_history(&mut self) {
        self.history.clear();
        self.previous.clear();
        self.last_committed_end = None;
    }

    /// Removes already committed words from a new hypothesis (meeting-time words), as
    /// described in the module docs. Does not change state.
    #[must_use]
    pub fn accept(&self, hypothesis: &[Word]) -> Vec<Word> {
        let Some(end) = self.last_committed_end else {
            return hypothesis.to_vec();
        };
        let near = hypothesis
            .iter()
            .take_while(|w| w.start < end + ANCHOR_WINDOW)
            .count();
        let max_n = self.history.len().min(MAX_NGRAM);
        for n in (1..=max_n).rev() {
            let suffix = &self.history[self.history.len() - n..];
            let found = (0..near)
                .filter(|&p| p + n <= hypothesis.len())
                .find(|&p| runs_match(suffix, &hypothesis[p..p + n]));
            if let Some(p) = found {
                return hypothesis[p + n..].to_vec();
            }
        }
        for n in (MIN_UNBOUNDED_NGRAM..=max_n).rev() {
            let suffix = &self.history[self.history.len() - n..];
            if let Some(p) = hypothesis
                .windows(n)
                .enumerate()
                .position(|(p, run)| untimed(&hypothesis[p]) && runs_match(suffix, run))
            {
                return hypothesis[p + n..].to_vec();
            }
        }
        if let Some(first) = self.previous.first()
            && let Some(p) = (0..near).find(|&p| {
                let floor = self.history.last().map_or(end, |w| w.start);
                hypothesis[p].start + ANCHOR_WINDOW > floor && same_word(first, &hypothesis[p])
            })
        {
            return hypothesis[p..].to_vec();
        }
        // whisper_streaming: keep words with start > last_commited_time - 0.1.
        hypothesis
            .iter()
            .filter(|w| w.start + DUPLICATE_SLACK > end)
            .cloned()
            .collect()
    }

    /// LocalAgreement-2 step: commits the longest common prefix of the previous
    /// hypothesis tail and `tail` (from [`LocalAgreement::accept`]), keeps the rest as the
    /// new previous hypothesis, and returns the newly committed words (texts and times from
    /// `tail`, the newer hypothesis).
    pub fn agree(&mut self, mut tail: Vec<Word>) -> Vec<Word> {
        let n = self
            .previous
            .iter()
            .zip(&tail)
            .take_while(|(a, b)| same_word(a, b))
            .count();
        let rest = tail.split_off(n);
        self.commit(&tail);
        self.previous = rest;
        tail
    }

    /// Commits `words` unconditionally (end of utterance or chunk), without touching the
    /// previous hypothesis.
    pub fn commit(&mut self, words: &[Word]) {
        let Some(last) = words.last() else {
            return;
        };
        self.last_committed_end = Some(
            self.last_committed_end
                .map_or(last.end, |e| e.max(last.end)),
        );
        self.history.extend_from_slice(words);
        let excess = self.history.len().saturating_sub(MAX_NGRAM);
        self.history.drain(..excess);
    }

    /// The uncommitted tail of the last hypothesis.
    #[must_use]
    pub fn tentative(&self) -> &[Word] {
        &self.previous
    }

    /// Replaces the uncommitted tail (carry-over across a chunk boundary).
    pub fn set_tentative(&mut self, words: Vec<Word>) {
        self.previous = words;
    }
}

/// Lower-cased alphanumerics (and apostrophes) of a word, for agreement comparisons.
#[must_use]
pub fn normalize_token(text: &str) -> String {
    text.chars()
        .filter(|c| c.is_alphanumeric() || *c == '\'')
        .flat_map(char::to_lowercase)
        .collect()
}

/// Two words agree when their normalised texts are equal and not empty.
fn same_word(a: &Word, b: &Word) -> bool {
    let na = normalize_token(&a.text);
    !na.is_empty() && na == normalize_token(&b.text)
}

fn runs_match(a: &[Word], b: &[Word]) -> bool {
    a.len() == b.len() && a.iter().zip(b).all(|(x, y)| same_word(x, y))
}
