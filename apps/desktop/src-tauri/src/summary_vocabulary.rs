//! The Dictionary page of Settings (T12.15a): names, acronyms and other terms the user wants
//! summaries to spell right. The setting and its rules are
//! `earshot_runtime::vocabulary` (and `earshot_core::prompt::Vocabulary`); this module only
//! maps them to the IPC types.

use earshot_core::prompt::{MAX_VOCABULARY_CHARS, MAX_VOCABULARY_WORD_CHARS, MAX_VOCABULARY_WORDS};
use earshot_runtime::Services;
use earshot_runtime::vocabulary::{self, SetVocabularyError};
use serde::Serialize;
use specta::Type;

use crate::ipc::IpcError;

#[cfg(test)]
#[path = "summary_vocabulary_tests.rs"]
mod tests;

/// The stored words with what the page says about them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Type)]
pub struct SummaryVocabularyView {
    /// The words in use, in the user's order (empty for none, the default).
    pub words: Vec<String>,
    /// One line when the stored list is one this version refuses (none is used).
    pub unknown: Vec<String>,
    /// The most words accepted.
    pub max_words: u32,
    /// The longest word accepted, in characters.
    pub max_word_chars: u32,
    /// The longest list accepted, in characters over every word.
    pub max_chars: u32,
}

fn limit(n: usize) -> u32 {
    u32::try_from(n).unwrap_or(u32::MAX)
}

fn view(words: &[String], unknown: Vec<String>) -> SummaryVocabularyView {
    SummaryVocabularyView {
        words: words.to_vec(),
        unknown,
        max_words: limit(MAX_VOCABULARY_WORDS),
        max_word_chars: limit(MAX_VOCABULARY_WORD_CHARS),
        max_chars: limit(MAX_VOCABULARY_CHARS),
    }
}

/// `text` with its first letter upper-case and a full stop.
fn sentence(text: &str) -> String {
    let mut c = text.chars();
    let mut out: String = c
        .next()
        .map(|f| f.to_uppercase().collect())
        .unwrap_or_default();
    out.push_str(c.as_str());
    if !out.ends_with('.') {
        out.push('.');
    }
    out
}

/// The words of `services`' store.
///
/// # Errors
/// Store errors.
pub fn vocabulary_view(services: &Services) -> Result<SummaryVocabularyView, IpcError> {
    let (words, unknown) = vocabulary::vocabulary(&services.store())?;
    Ok(view(
        words.words(),
        unknown.iter().map(ToString::to_string).collect(),
    ))
}

/// Checks and stores `words` (empty for none); returns them as stored.
///
/// # Errors
/// `invalid_setting` (nothing stored, the reason in plain words); store errors.
pub fn set_words(services: &Services, words: &[String]) -> Result<SummaryVocabularyView, IpcError> {
    let stored = {
        let mut db = services.store();
        vocabulary::set_vocabulary(&mut db, words)
    };
    stored.map(|v| view(v.words(), Vec::new())).map_err(refusal)
}

/// A Dictionary write's error for the UI: a refusal is `invalid_setting` with the rule in plain
/// words (the page's own message, also when a word is added from the transcript, T12.15f);
/// store errors as usual.
pub fn refusal(e: SetVocabularyError) -> IpcError {
    match e {
        SetVocabularyError::Store(e) => e.into(),
        SetVocabularyError::Refused(_) | SetVocabularyError::NotAWord => IpcError {
            kind: "invalid_setting".into(),
            message: sentence(&e.to_string()),
            retryable: false,
        },
    }
}

/// The user's own words for summaries (and a stored list this version refuses).
#[tauri::command]
#[specta::specta]
pub async fn summary_vocabulary(
    state: tauri::State<'_, crate::ipc::AppState>,
) -> Result<SummaryVocabularyView, IpcError> {
    let services = state.services()?;
    crate::ipc::blocking(move || vocabulary_view(&services)).await
}

/// Checks and stores the user's own words (empty for none; `invalid_setting`: nothing
/// stored).
#[tauri::command]
#[specta::specta]
pub async fn set_summary_vocabulary(
    state: tauri::State<'_, crate::ipc::AppState>,
    words: Vec<String>,
) -> Result<SummaryVocabularyView, IpcError> {
    let services = state.services()?;
    crate::ipc::blocking(move || set_words(&services, &words)).await
}
