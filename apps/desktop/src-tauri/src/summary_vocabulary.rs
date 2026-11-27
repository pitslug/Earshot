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
    /// The words in use (empty for none, the default): in the user's order while Dictionary
    /// sync is off, in creation order while it is on (T11.3e).
    pub words: Vec<String>,
    /// One line when the stored list is one this version refuses (none is used).
    pub unknown: Vec<String>,
    /// The most words accepted.
    pub max_words: u32,
    /// The longest word accepted, in characters.
    pub max_word_chars: u32,
    /// The longest list accepted, in characters over every word.
    pub max_chars: u32,
    /// Words stored but not in use: the list joined from the user's computers by Dictionary
    /// sync passes a limit (T11.3e). Always 0 while Dictionary sync is off.
    pub not_in_use: u32,
    /// What the page says about them ([`not_in_use_note`]); `None` when every word is in use.
    pub not_in_use_note: Option<String>,
    /// Saved names left out for what they hold (a control or invisible formatting
    /// character, such as a zero-width joiner, or more than 100 characters; T12.91a,
    /// `earshot_runtime::word_choices::people_left_out`): never given to a summary.
    pub people_left_out: u32,
    /// What the page says about them ([`people_left_out_note`]); `None` when none is left out.
    pub people_left_out_note: Option<String>,
}

/// The page's line for one word not in use (`fixtures/ui/summary_settings_cases.json`,
/// `messages.not_in_use.one`).
pub const NOT_IN_USE_ONE: &str = "1 word is not in use: the Dictionaries of your computers together pass the limits. Remove a word to make room for it.";

/// The page's line for `{n}` words not in use (`messages.not_in_use.many`).
pub const NOT_IN_USE_MANY: &str = "{n} words are not in use: the Dictionaries of your computers together pass the limits. Remove words to make room for them.";

/// The page's line for one saved name never given to a summary
/// (`fixtures/ui/summary_settings_cases.json`, `messages.people_left_out.one`).
pub const PEOPLE_LEFT_OUT_ONE: &str = "1 saved name on the People page is never given to a summary: it holds an invisible formatting character (such as a zero-width joiner) or a control character, or is longer than 100 characters.";

/// The page's line for `{n}` saved names left out (`messages.people_left_out.many`).
pub const PEOPLE_LEFT_OUT_MANY: &str = "{n} saved names on the People page are never given to a summary: each holds an invisible formatting character (such as a zero-width joiner) or a control character, or is longer than 100 characters.";

/// The page's line for `n` saved names never given to a summary; `None` for 0. The names
/// themselves are not listed on the Dictionary page: the People page holds them (T12.97).
#[must_use]
pub fn people_left_out_note(n: usize) -> Option<String> {
    match n {
        0 => None,
        1 => Some(PEOPLE_LEFT_OUT_ONE.to_owned()),
        n => Some(PEOPLE_LEFT_OUT_MANY.replace("{n}", &n.to_string())),
    }
}

/// The page's line for `n` stored words not in use; `None` for 0.
#[must_use]
pub fn not_in_use_note(n: usize) -> Option<String> {
    match n {
        0 => None,
        1 => Some(NOT_IN_USE_ONE.to_owned()),
        n => Some(NOT_IN_USE_MANY.replace("{n}", &n.to_string())),
    }
}

fn limit(n: usize) -> u32 {
    u32::try_from(n).unwrap_or(u32::MAX)
}

fn view(
    words: &[String],
    unknown: Vec<String>,
    not_in_use: usize,
    people_left_out: usize,
) -> SummaryVocabularyView {
    SummaryVocabularyView {
        words: words.to_vec(),
        unknown,
        max_words: limit(MAX_VOCABULARY_WORDS),
        max_word_chars: limit(MAX_VOCABULARY_WORD_CHARS),
        max_chars: limit(MAX_VOCABULARY_CHARS),
        not_in_use: limit(not_in_use),
        not_in_use_note: not_in_use_note(not_in_use),
        people_left_out: limit(people_left_out),
        people_left_out_note: people_left_out_note(people_left_out),
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
    let db = services.store();
    let state = vocabulary::dictionary_state(&db)?;
    let left_out = earshot_runtime::word_choices::summary_people_left_out(&db)?;
    Ok(view(
        state.words.words(),
        state.unknown.iter().map(ToString::to_string).collect(),
        state.not_in_use,
        left_out,
    ))
}

/// What an Add on the Dictionary page did (T11.3e: the page edits one word at a time).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Type)]
pub struct SummaryVocabularyAdded {
    /// False when the word was already there (case ignored): nothing was written.
    pub added: bool,
    /// The words after the add.
    pub view: SummaryVocabularyView,
}

/// Adds one typed entry (`vocabulary::add_entry`: the stored list read and written in one
/// transaction).
///
/// # Errors
/// `invalid_setting` (nothing stored, the reason in plain words: the limits, words not in use);
/// store errors.
pub fn add_word(services: &Services, text: &str) -> Result<SummaryVocabularyAdded, IpcError> {
    let added = vocabulary::add_entry(services, text).map_err(refusal)?;
    Ok(SummaryVocabularyAdded {
        added: added.added,
        view: vocabulary_view(services)?,
    })
}

/// Removes one entry (`vocabulary::remove_entry`: one transaction) and returns the words.
///
/// # Errors
/// Store errors.
pub fn remove_word(services: &Services, word: &str) -> Result<SummaryVocabularyView, IpcError> {
    vocabulary::remove_entry(services, word).map_err(refusal)?;
    vocabulary_view(services)
}

/// A Dictionary write's error for the UI: a refusal is `invalid_setting` with the rule in plain
/// words (the page's own message, also when a word is added from the transcript, T12.15f);
/// store errors as usual.
pub fn refusal(e: SetVocabularyError) -> IpcError {
    match e {
        SetVocabularyError::Store(e) => e.into(),
        SetVocabularyError::Refused(_)
        | SetVocabularyError::NotAWord
        | SetVocabularyError::NotInUse
        | SetVocabularyError::Stale => IpcError::invalid(sentence(&e.to_string())),
        // T14.65 (docs/sync-spec.md 17.5): the one refusal line, kind `damaged`; Details says
        // when Dictionary sync is off.
        SetVocabularyError::Damaged { .. } => crate::damage::refusal(),
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

/// Adds one word typed on the Dictionary page (`invalid_setting`: nothing stored).
#[tauri::command]
#[specta::specta]
pub async fn add_summary_vocabulary_word(
    state: tauri::State<'_, crate::ipc::AppState>,
    text: String,
) -> Result<SummaryVocabularyAdded, IpcError> {
    let services = state.services()?;
    crate::ipc::blocking(move || add_word(&services, &text)).await
}

/// Removes one word from the Dictionary page.
#[tauri::command]
#[specta::specta]
pub async fn remove_summary_vocabulary_word(
    state: tauri::State<'_, crate::ipc::AppState>,
    word: String,
) -> Result<SummaryVocabularyView, IpcError> {
    let services = state.services()?;
    crate::ipc::blocking(move || remove_word(&services, &word)).await
}
