//! The user's own words (T12.15a): names, acronyms and other terms from their meetings, so
//! summaries spell them right ([`earshot_core::prompt::Vocabulary`], the rules and limits).
//! Device-local (`local_state` key [`VOCABULARY_KEY`], never synced, no migration), stored as
//! a JSON list of strings, written only through [`set_vocabulary`] after the core check; an
//! empty list (the default) is no words.
//!
//! Every summary [`crate::summary::SummaryService::summarize`] writes reads the list stored
//! at that moment, whoever writes it (this computer, your own endpoint, Claude Code, Codex),
//! so a re-summarise uses the current list; a live session reads it once at its start for
//! its summary windows (`live` module). The words are user text sent only where the
//! summary's prompt already goes.
//!
//! A stored value this build refuses (not a list of strings, too long, control characters)
//! reads as no words and is reported ([`UnknownSetting`], by its reason, never its text), as
//! the other settings pages do.

use std::collections::HashSet;
use std::fmt;

use earshot_core::prompt::{Vocabulary, VocabularyError};
use earshot_core::vocabulary::suggest::{is_common, split_punctuation};
use earshot_store::Store;

use crate::RuntimeError;
use crate::local::DeviceRecord;
pub use crate::model_settings::UnknownSetting;

#[cfg(test)]
#[path = "vocabulary_tests.rs"]
mod tests;

/// `local_state` key of the user's words.
pub const VOCABULARY_KEY: &str = "settings/vocabulary";

/// Why a list of words was refused (nothing was stored).
#[derive(Debug)]
pub enum SetVocabularyError {
    /// Too many words, a word too long, or a control character.
    Refused(VocabularyError),
    /// Writing the store failed.
    Store(RuntimeError),
    /// A word from the transcript ([`add_word`]) that is not a word: no letter between its
    /// punctuation.
    NotAWord,
}

impl fmt::Display for SetVocabularyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Refused(e) => write!(f, "{e}"),
            Self::NotAWord => write!(f, "only a word can be added to the dictionary"),
            Self::Store(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for SetVocabularyError {}

/// The stored words (none stored: empty) and, when the stored value is refused, why (no
/// words are used).
///
/// # Errors
/// Store errors.
pub fn vocabulary(store: &Store) -> Result<(Vocabulary, Option<UnknownSetting>), RuntimeError> {
    Ok(vocabulary_of(store.local_value(VOCABULARY_KEY)?.as_deref()))
}

/// [`vocabulary`] from the stored bytes (`None`: nothing stored), so a store transaction can
/// read the words too (`crate::word_choices`).
pub(crate) fn vocabulary_of(bytes: Option<&[u8]>) -> (Vocabulary, Option<UnknownSetting>) {
    let Some(bytes) = bytes else {
        return (Vocabulary::default(), None);
    };
    let refused = |why: String| {
        (
            Vocabulary::default(),
            Some(UnknownSetting {
                key: VOCABULARY_KEY,
                stored: format!("a word list this version refuses: {why}"),
            }),
        )
    };
    // Parsed as plain strings first, so the reason names the rule broken (the core check),
    // never a word.
    match serde_json::from_slice::<Vec<String>>(bytes) {
        Ok(words) => match Vocabulary::new(&words) {
            Ok(v) => (v, None),
            Err(e) => refused(e.to_string()),
        },
        Err(_) => refused("it is not a list of words".into()),
    }
}

/// Checks `words` ([`Vocabulary::new`]: trimmed, empty ones and repeats dropped, within the
/// limits, one line each) and stores them, replacing the earlier list; an empty list stores
/// no words. Returns what was stored.
///
/// # Errors
/// [`SetVocabularyError`]: nothing is stored on a refusal.
pub fn set_vocabulary<S: AsRef<str>>(
    store: &mut Store,
    words: &[S],
) -> Result<Vocabulary, SetVocabularyError> {
    let vocabulary = Vocabulary::new(words).map_err(SetVocabularyError::Refused)?;
    let bytes = serde_json::to_vec(vocabulary.words())
        .map_err(|_| SetVocabularyError::Store(RuntimeError::Internal("word list encoding")))?;
    crate::local::set(store, &DeviceRecord::Vocabulary.key(), &bytes)
        .map_err(|e| SetVocabularyError::Store(e.into()))?;
    Ok(vocabulary)
}

// Adding words from the transcript (T12.15f): one word at a time, from a click on that word
// (a word of the line's text, or a button of the bar under a line just edited); never
// automatic. Both directions go through [`set_vocabulary`], so the limits and refusals are the
// Dictionary page's own.

/// The word a transcript token adds to the Dictionary: its core (no surrounding punctuation,
/// `earshot_core::vocabulary::suggest::split_punctuation`, the matcher's own rule), when the
/// core holds a letter; `None` for a token that is not a word (punctuation, a number). The UI
/// decides which words get "Add to Dictionary" by the same rule
/// (`fixtures/ui/dictionary_word_cases.json`, which both suites run).
#[must_use]
pub fn dictionary_word(token: &str) -> Option<&str> {
    let (_, core, _) = split_punctuation(token.trim());
    core.chars().any(char::is_alphabetic).then_some(core)
}

/// Whether `word` is an entry of `entries` (case ignored, as [`Vocabulary::new`] drops a
/// repeat in another case).
fn listed_in<S: AsRef<str>>(entries: &[S], word: &str) -> bool {
    let wanted = word.to_lowercase();
    entries.iter().any(|e| e.as_ref().to_lowercase() == wanted)
}

/// The words of a line the bar under it offers after a saved edit: each white-space token of
/// `after` whose [`dictionary_word`] is not the core of any token of `before` (a word the edit
/// wrote), leaving out common words (`earshot_core::vocabulary::suggest::is_common`, the
/// matcher's rule: a common word is never suggested, so listing it would do nothing), words on
/// `listed` (the word list, `crate::word_choices::word_list`: the Dictionary's words and saved
/// people's names, case ignored) and repeats in another case; in line order.
#[must_use]
pub fn words_to_offer<S: AsRef<str>>(before: &str, after: &str, listed: &[S]) -> Vec<String> {
    let before: HashSet<&str> = before
        .split_whitespace()
        .map(|t| split_punctuation(t).1)
        .collect();
    let mut seen = HashSet::new();
    after
        .split_whitespace()
        .filter_map(dictionary_word)
        .filter(|w| !before.contains(w) && !is_common(w) && !listed_in(listed, w))
        .filter(|w| seen.insert(w.to_lowercase()))
        .map(str::to_owned)
        .collect()
}

/// What [`add_word`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AddedWord {
    /// The word as the Dictionary holds it now ("Undo" removes exactly this).
    pub word: String,
    /// False when the Dictionary already had it (case ignored): nothing was written.
    pub added: bool,
}

/// Adds the word of `token` ([`dictionary_word`]) at the end of the stored Dictionary in one
/// [`set_vocabulary`] write. A word already there (case ignored) writes nothing. A stored list
/// this version refuses reads as no words ([`vocabulary`]), so the new list holds this word
/// alone, as an add on the Dictionary page does.
///
/// # Errors
/// [`SetVocabularyError::NotAWord`] for a token with no word; [`SetVocabularyError::Refused`]
/// (the list's limits, as the Dictionary page reports them); store errors. Nothing is stored on
/// an error.
pub fn add_word(store: &mut Store, token: &str) -> Result<AddedWord, SetVocabularyError> {
    let word = dictionary_word(token)
        .ok_or(SetVocabularyError::NotAWord)?
        .to_owned();
    let (current, _refused) = vocabulary(store).map_err(SetVocabularyError::Store)?;
    if let Some(entry) = current
        .words()
        .iter()
        .find(|e| e.to_lowercase() == word.to_lowercase())
    {
        return Ok(AddedWord {
            word: entry.clone(),
            added: false,
        });
    }
    let mut words: Vec<String> = current.into();
    words.push(word.clone());
    set_vocabulary(store, &words)?;
    Ok(AddedWord { word, added: true })
}

/// Removes the Dictionary entry that is `word` (case ignored; surrounding white space ignored,
/// inner runs read as one space, as [`Vocabulary::new`] stores entries) in one
/// [`set_vocabulary`] write. Returns whether an entry went; a word not listed writes nothing.
/// An entry that only contains `word` (a phrase) stays.
///
/// # Errors
/// Store errors.
pub fn remove_word(store: &mut Store, word: &str) -> Result<bool, SetVocabularyError> {
    let word = word.split_whitespace().collect::<Vec<_>>().join(" ");
    let (current, _refused) = vocabulary(store).map_err(SetVocabularyError::Store)?;
    if word.is_empty() || !listed_in(current.words(), &word) {
        return Ok(false);
    }
    let wanted = word.to_lowercase();
    let kept: Vec<&String> = current
        .words()
        .iter()
        .filter(|e| e.to_lowercase() != wanted)
        .collect();
    set_vocabulary(store, &kept)?;
    Ok(true)
}
