//! The user's own words (T12.15a): names, acronyms and other terms from their meetings, so
//! a summary spells them as the user does even where speech recognition did not.
//!
//! Unlike the style ([`super::SummaryStyle`]), the words are spelling help for the
//! transcript, so every request that shows transcript text or text written from it carries
//! them: the single request, every map request, every merge (inner and last) and the
//! two-call evaluation requests. They are one block in the system message ([`VOCABULARY`]),
//! after the fixed rules, the template and any map or merge instruction and before the
//! style, introduced by fixed text that says they are words to spell and never
//! instructions; every word is one line prefixed with [`VOCABULARY_LINE_PREFIX`], so no
//! word can close or fake the block's markers. An empty list adds nothing, so every prompt
//! without words is byte-identical to one from before T12.15a.

use core::fmt;

use serde::{Deserialize, Serialize};

use super::template::VOCABULARY;

/// Most words in the list. A list is read by eye on the Dictionary page and sent with every
/// request of a summary, map requests included; a hundred covers the names and acronyms a
/// team uses without making the block a prompt of its own.
pub const MAX_VOCABULARY_WORDS: usize = 100;

/// Longest word, in characters (after trimming and joining inner spaces). A word is a name,
/// an acronym or a short phrase ("Quillmarsh Ridge site"); forty characters holds any of
/// those and refuses pasted sentences.
pub const MAX_VOCABULARY_WORD_CHARS: usize = 40;

/// Longest list, in characters over every word. The block goes into every request, so it
/// is held to the size of the summary style ([`super::MAX_STYLE_CHARS`]), which goes into
/// one: the whole block stays under 1,500 estimated tokens however the words are written,
/// under a tenth of the local summary model's 16k context.
pub const MAX_VOCABULARY_CHARS: usize = 1000;

/// The prefix of every word's line inside the block. The block's markers are lines without
/// it, so no word can end the block early.
pub const VOCABULARY_LINE_PREFIX: &str = "| ";

/// The user's own words: empty (the default, no block in any prompt) or a checked list of
/// at most [`MAX_VOCABULARY_WORDS`] words of at most [`MAX_VOCABULARY_WORD_CHARS`]
/// characters each and [`MAX_VOCABULARY_CHARS`] in all, each one line without control
/// characters, in the user's order, with no two that differ only in case. Built only
/// through [`Vocabulary::new`] (deserialising checks the same rules).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(try_from = "Vec<String>", into = "Vec<String>")]
pub struct Vocabulary(Vec<String>);

/// Why a list of words was refused. Positions count the words as given, from 1, empty ones
/// included. The messages name the rule broken and the numbers, never the word or its
/// position: the Dictionary page adds one word at a time, so the refused word is the one just
/// typed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VocabularyError {
    /// More than [`MAX_VOCABULARY_WORDS`] words (the count found, after dropping empty
    /// entries and repeats).
    TooManyWords(usize),
    /// A word longer than [`MAX_VOCABULARY_WORD_CHARS`] characters.
    WordTooLong {
        /// Its position, from 1.
        line: usize,
        /// Its length.
        chars: usize,
    },
    /// More than [`MAX_VOCABULARY_CHARS`] characters over every word (the total found).
    TooLong(usize),
    /// A word holds a control character (a line break or a tab included).
    ControlCharacter {
        /// Its position, from 1.
        line: usize,
    },
}

impl fmt::Display for VocabularyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            VocabularyError::TooManyWords(n) => write!(
                f,
                "the list can hold at most {MAX_VOCABULARY_WORDS} words; this one has {n}"
            ),
            VocabularyError::WordTooLong { chars, .. } => write!(
                f,
                "a word can have at most {MAX_VOCABULARY_WORD_CHARS} characters; this one has {chars}"
            ),
            VocabularyError::TooLong(n) => write!(
                f,
                "the list can have at most {MAX_VOCABULARY_CHARS} characters in all; this one has {n}"
            ),
            VocabularyError::ControlCharacter { .. } => {
                write!(f, "a word can hold only text, with no line breaks or tabs")
            }
        }
    }
}

impl std::error::Error for VocabularyError {}

impl Vocabulary {
    /// `words` checked: each trimmed with runs of white space inside it joined to one space,
    /// empty ones dropped, a word that repeats an earlier one in another case dropped (the
    /// first spelling wins), then at most [`MAX_VOCABULARY_WORDS`] words of at most
    /// [`MAX_VOCABULARY_WORD_CHARS`] characters and [`MAX_VOCABULARY_CHARS`] in all. A
    /// control character anywhere in a word (a line break or a tab included) refuses the
    /// list: a word is one line.
    ///
    /// # Errors
    /// [`VocabularyError`].
    pub fn new<I, S>(words: I) -> Result<Self, VocabularyError>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let mut kept: Vec<String> = Vec::new();
        let mut folded: Vec<String> = Vec::new();
        let mut total = 0;
        for (i, word) in words.into_iter().enumerate() {
            let line = i + 1;
            let word = word.as_ref();
            // Checked before joining white space: some control characters are also white
            // space (tab, NEL).
            if word.chars().any(char::is_control) {
                return Err(VocabularyError::ControlCharacter { line });
            }
            // `split_whitespace` also splits at the Unicode line and paragraph separators,
            // so a kept word is always one line.
            let word = word.split_whitespace().collect::<Vec<_>>().join(" ");
            if word.is_empty() {
                continue;
            }
            let chars = word.chars().count();
            if chars > MAX_VOCABULARY_WORD_CHARS {
                return Err(VocabularyError::WordTooLong { line, chars });
            }
            let key = word.to_lowercase();
            if folded.contains(&key) {
                continue;
            }
            folded.push(key);
            total += chars;
            kept.push(word);
        }
        if kept.len() > MAX_VOCABULARY_WORDS {
            return Err(VocabularyError::TooManyWords(kept.len()));
        }
        if total > MAX_VOCABULARY_CHARS {
            return Err(VocabularyError::TooLong(total));
        }
        Ok(Vocabulary(kept))
    }

    /// Whether there are no words (no block in any prompt).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// The words as stored, in the user's order.
    #[must_use]
    pub fn words(&self) -> &[String] {
        &self.0
    }

    /// The block for a request's system message: `None` when empty, else [`VOCABULARY`]
    /// with one [`VOCABULARY_LINE_PREFIX`]ed line per word in its slot.
    #[must_use]
    pub(super) fn block(&self) -> Option<String> {
        (!self.is_empty()).then(|| {
            let lines: Vec<String> = self
                .0
                .iter()
                .map(|w| format!("{VOCABULARY_LINE_PREFIX}{w}"))
                .collect();
            VOCABULARY.replace("{{words}}", &lines.join("\n"))
        })
    }
}

impl TryFrom<Vec<String>> for Vocabulary {
    type Error = VocabularyError;

    fn try_from(words: Vec<String>) -> Result<Self, VocabularyError> {
        Vocabulary::new(words)
    }
}

impl From<Vocabulary> for Vec<String> {
    fn from(v: Vocabulary) -> Vec<String> {
        v.0
    }
}
