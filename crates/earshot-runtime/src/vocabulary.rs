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

use std::fmt;

use earshot_core::prompt::{Vocabulary, VocabularyError};
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
}

impl fmt::Display for SetVocabularyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Refused(e) => write!(f, "{e}"),
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
