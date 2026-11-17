//! The user's own words (T12.15a): names, acronyms and other terms from their meetings, so
//! summaries spell them right ([`earshot_core::prompt::Vocabulary`], the rules and limits).
//! What summaries read is device-local (`local_state` key [`VOCABULARY_KEY`]), stored as a JSON
//! list of strings, written only by the Dictionary's edits after the core check: one word at a
//! time ([`add_entry`], [`remove_entry`], and [`add_word`] / [`remove_word`] from the
//! transcript), or a whole list from a caller that read it first ([`set_vocabulary`]); an empty
//! list (the default) is no words.
//!
//! Dictionary sync (T11.3e, docs/sync.md section 9): every write also keeps one synced
//! `DictionaryWord` row per word in the same transaction (a new word gets a row, a removed one
//! a tombstone with its word cleared), whether or not sync is on, so turning it on sends them.
//! While Dictionary sync is on ([`dictionary_syncs`]) the stored list is rebuilt from the live
//! rows ([`rebuild_tx`]) after every write and every applied page: the same word twice (case
//! ignored, as [`Vocabulary::new`] compares) counts once with the earliest-created spelling, and
//! words are taken in creation order until a limit of [`Vocabulary::new`] is reached. The rest
//! stay stored and the Dictionary page says how many are not in use ([`DictionaryState`]);
//! nothing is deleted to make room. Off, the list is this device's own, as written.
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
use std::sync::Arc;

use earshot_core::model::{DictionaryRepo, DictionaryWord, Stamp};
use earshot_core::prompt::{
    MAX_VOCABULARY_CHARS, MAX_VOCABULARY_WORDS, Vocabulary, VocabularyError,
};
use earshot_core::vocabulary::suggest::{is_common, split_punctuation};
use earshot_store::{Store, StoreError, StoreTx};

use crate::RuntimeError;
use crate::edit::DynStamp;
use crate::local::DeviceRecord;
pub use crate::local::UnknownSetting;
use crate::services::{Services, os_seeded_rng};

#[cfg(test)]
#[path = "vocabulary_tests.rs"]
mod tests;

/// Why an add is refused while some words are not in use (shown as a sentence by the app,
/// `fixtures/ui/summary_settings_cases.json` `messages.add_not_in_use`).
pub const NOT_IN_USE_REFUSAL: &str =
    "some words are not in use: remove words to make room before adding more";

/// Why a whole-list save is refused when the stored list changed since it was read
/// (`messages.stale_list`).
pub const STALE_LIST: &str =
    "the dictionary changed since this list was read: look at it again and try once more";

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
    /// punctuation (also an empty entry).
    NotAWord,
    /// Dictionary sync is on and some stored words are not in use: an added word would not be
    /// in use either (T11.3e).
    NotInUse,
    /// The stored list is not the one the caller read ([`set_vocabulary`]'s `base`): another
    /// computer or window changed it (T11.3e).
    Stale,
}

impl fmt::Display for SetVocabularyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Refused(e) => write!(f, "{e}"),
            Self::NotAWord => write!(f, "only a word can be added to the dictionary"),
            Self::NotInUse => write!(f, "{NOT_IN_USE_REFUSAL}"),
            Self::Stale => write!(f, "{STALE_LIST}"),
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

/// Stored entries read without leading and trailing single quotes (T12.45): a word added from
/// the transcript before quotes were stripped (`'Zorbek'`) reads as `Zorbek`, so it matches,
/// is found for removal and is not added twice; [`Vocabulary::new`] then drops the repeat a
/// stripped entry may make. Only quotes at the ends of a whole entry go: other punctuation the
/// user typed stays.
fn stored_without_quotes(words: &[String]) -> Vec<&str> {
    words.iter().map(|w| w.trim().trim_matches('\'')).collect()
}

/// The Dictionary as the page shows it: the words in use (what summaries read) and how many
/// stored words are not in use because the list joined from the user's computers passes a limit
/// (always 0 while Dictionary sync is off).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DictionaryState {
    /// The words in use.
    pub words: Vocabulary,
    /// Why a stored list was refused (no words are used then).
    pub unknown: Option<UnknownSetting>,
    /// Stored words not in use (distinct, case ignored).
    pub not_in_use: usize,
}

/// [`vocabulary`] plus how many stored words are not in use ([`DictionaryState`]).
///
/// # Errors
/// Store errors.
pub fn dictionary_state(store: &Store) -> Result<DictionaryState, RuntimeError> {
    let (words, unknown) = vocabulary(store)?;
    let not_in_use = if dictionary_syncs(store)? {
        joined(&store.dictionary_words()?).1
    } else {
        0
    };
    Ok(DictionaryState {
        words,
        unknown,
        not_in_use,
    })
}

/// Whether the stored list follows the synced rows: sync is on (`settings/sync`) and
/// Dictionary sync is not turned off (`settings/sync-dictionary`, on by default).
///
/// # Errors
/// Store errors.
pub fn dictionary_syncs(store: &Store) -> Result<bool, RuntimeError> {
    syncs_from(|k| store.local_value(k))
}

fn syncs_from(
    read: impl Fn(&str) -> Result<Option<Vec<u8>>, StoreError>,
) -> Result<bool, RuntimeError> {
    let switch = |key: &str, default: bool| -> Result<bool, RuntimeError> {
        Ok(read(key)?
            .and_then(|b| crate::local::on_off(&String::from_utf8_lossy(&b)))
            .unwrap_or(default))
    };
    Ok(switch(crate::sync::SYNC_ON_KEY, false)? && switch(crate::sync::SYNC_DICTIONARY_KEY, true)?)
}

/// The comparison key of a stored word, by the rules [`joined`] reads rows with: trimmed,
/// single quotes at the ends dropped, read by [`Vocabulary::new`] (inner white space joined),
/// lower-cased as it compares repeats. `None` for a word it refuses on its own (too long, a
/// control or formatting character) or that is empty; such a row is matched by its lower-cased
/// text instead ([`row_key`]), so a row from another build can still be removed.
fn word_key(word: &str) -> Option<String> {
    let one = Vocabulary::new([word.trim().trim_matches('\'')]).ok()?;
    one.words().first().map(|w| w.to_lowercase())
}

/// [`word_key`], or the lower-cased text of a word it refuses.
fn row_key(word: &str) -> String {
    word_key(word).unwrap_or_else(|| word.to_lowercase())
}

/// The list joined from the live word rows (docs/sync.md section 9): in creation order, the
/// same word twice ([`word_key`]) once with the earliest spelling, a word [`Vocabulary::new`]
/// refuses on its own not in use, taken until the next word would pass
/// [`MAX_VOCABULARY_WORDS`] or [`MAX_VOCABULARY_CHARS`] (a prefix: every word after it is not in
/// use). Returns the words in use and how many distinct words are not.
fn joined(rows: &[DictionaryWord]) -> (Vocabulary, usize) {
    let mut seen: HashSet<String> = HashSet::new();
    let mut kept: Vec<String> = Vec::new();
    let mut total = 0;
    let mut full = false;
    let mut not_in_use = 0;
    for row in rows.iter().filter(|r| r.is_live()) {
        let Ok(one) = Vocabulary::new([row.word.trim().trim_matches('\'')]) else {
            if seen.insert(row_key(&row.word)) {
                not_in_use += 1;
            }
            continue;
        };
        let Some(word) = one.words().first() else {
            continue;
        };
        if !seen.insert(word.to_lowercase()) {
            continue;
        }
        let chars = word.chars().count();
        if full || kept.len() + 1 > MAX_VOCABULARY_WORDS || total + chars > MAX_VOCABULARY_CHARS {
            full = true;
            not_in_use += 1;
            continue;
        }
        total += chars;
        kept.push(word.clone());
    }
    // Within every limit by construction; `new` checks it again.
    (Vocabulary::new(&kept).unwrap_or_default(), not_in_use)
}

/// Whether a rebuild may replace a stored list this version refuses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OverRefused {
    /// No: the refused list stays and is still reported by its reason (a pulled page or the
    /// switch turned on must not hide it).
    Keep,
    /// Yes: the user changed the Dictionary, as a write over a refused list does with sync off.
    Replace,
}

/// Rebuilds the stored list from the live word rows ([`joined`]) inside `tx` and returns it,
/// only while Dictionary sync is on ([`dictionary_syncs`], read inside `tx`, so a switch turned
/// off just before an apply leaves the list alone): `None` when it did nothing (sync off, or a
/// refused stored list kept under [`OverRefused::Keep`]). Run after every applied page and when
/// the switch is turned on (`crate::sync`), and after every write here.
///
/// # Errors
/// Store errors.
pub(crate) fn rebuild_tx(
    tx: &mut StoreTx<'_>,
    over: OverRefused,
) -> Result<Option<Vocabulary>, StoreError> {
    if !syncs_from(|k| tx.local_value(k)).map_err(runtime_to_store)? {
        return Ok(None);
    }
    if over == OverRefused::Keep {
        let (_, refused) = vocabulary_of(tx.local_value(VOCABULARY_KEY)?.as_deref());
        if refused.is_some() {
            return Ok(None);
        }
    }
    let (words, _) = joined(&tx.dictionary_words()?);
    write_list_tx(tx, &words)?;
    Ok(Some(words))
}

fn runtime_to_store(e: RuntimeError) -> StoreError {
    match e {
        RuntimeError::Store(e) => e,
        _ => StoreError::Corrupt("dictionary switch".into()),
    }
}

fn write_list_tx(tx: &mut StoreTx<'_>, words: &Vocabulary) -> Result<(), StoreError> {
    let bytes = serde_json::to_vec(words.words())
        .map_err(|_| StoreError::Corrupt("word list encoding".into()))?;
    crate::local::set_tx(tx, &DeviceRecord::Vocabulary.key(), &bytes)
}

/// Makes a row for each word of the stored list that has no live row ([`word_key`]): the words
/// an older Earshot stored before rows existed. Run when Dictionary sync is turned on, before
/// every sync tick pulls and inside every write here, so a rebuild never loses them. Returns how
/// many rows it made.
///
/// # Errors
/// Store errors.
pub(crate) fn reconcile_tx(
    tx: &mut StoreTx<'_>,
    stamp: &mut DynStamp<'_, '_>,
) -> Result<usize, StoreError> {
    let (current, _) = vocabulary_of(tx.local_value(VOCABULARY_KEY)?.as_deref());
    let rows = tx.dictionary_words()?;
    let mut made = 0;
    for word in current.words() {
        if !has_live_row(&rows, word) {
            tx.put_dictionary_word(DictionaryWord::new(word.clone(), stamp))?;
            made += 1;
        }
    }
    Ok(made)
}

/// What turning Dictionary syncing on does inside its transaction (the Dictionary switch, sync
/// itself, or setting up or joining a vault): rows for the words here ([`reconcile_tx`]), then
/// the list rebuilt from the rows ([`rebuild_tx`], a refused stored list kept), so the list never
/// lags the rows it will be checked against.
///
/// # Errors
/// Store errors.
pub(crate) fn syncing_turned_on_tx(
    tx: &mut StoreTx<'_>,
    stamp: &mut DynStamp<'_, '_>,
) -> Result<(), StoreError> {
    reconcile_tx(tx, stamp)?;
    rebuild_tx(tx, OverRefused::Keep)?;
    Ok(())
}

/// [`reconcile_tx`] in its own transaction.
///
/// # Errors
/// Store errors.
pub(crate) fn reconcile(services: &Services) -> Result<usize, RuntimeError> {
    with_stamp(services, |tx, stamp| Ok(reconcile_tx(tx, stamp)?))
}

fn has_live_row(rows: &[DictionaryWord], word: &str) -> bool {
    let wanted = row_key(word);
    rows.iter()
        .any(|r| r.is_live() && row_key(&r.word) == wanted)
}

/// Removes every live row of `word` ([`row_key`], so any spelling and a row from another build
/// go too): each a tombstone with its word cleared. Returns how many went.
fn remove_rows_tx(
    tx: &mut StoreTx<'_>,
    stamp: &mut DynStamp<'_, '_>,
    word: &str,
) -> Result<usize, StoreError> {
    let wanted = row_key(word);
    let mut gone = 0;
    for mut row in tx
        .dictionary_words()?
        .into_iter()
        .filter(|r| r.is_live() && row_key(&r.word) == wanted)
    {
        if row.remove(stamp.hlc, stamp.clock) {
            tx.put_dictionary_word(row)?;
            gone += 1;
        }
    }
    Ok(gone)
}

/// Runs `f` in one store transaction under the services lock, with a stamp from the services
/// clock, the device's HLC and a fresh OS-seeded RNG (for new row ids).
pub(crate) fn with_stamp<T, E: From<StoreError>>(
    services: &Services,
    f: impl FnOnce(&mut StoreTx<'_>, &mut DynStamp<'_, '_>) -> Result<T, E>,
) -> Result<T, E> {
    let clock = Arc::clone(&services.clock);
    let mut rng = os_seeded_rng();
    let mut db = services.store();
    let (store, hlc) = db.parts();
    store.transaction(|tx| {
        let mut stamp: DynStamp<'_, '_> = Stamp {
            clock: &*clock,
            hlc,
            rng: &mut *rng,
        };
        f(tx, &mut stamp)
    })
}

#[cfg(test)]
thread_local! {
    /// A test seam: run by [`add_entry`] and [`remove_entry`] on this thread after they read
    /// the stored list and before they write (T11.3e review).
    pub(crate) static BETWEEN_READ_AND_WRITE: std::cell::RefCell<Option<Box<dyn Fn()>>> =
        std::cell::RefCell::new(None);
}

/// Runs the test seam [`BETWEEN_READ_AND_WRITE`] when one is set; nothing in shipped builds.
fn between_read_and_write() {
    #[cfg(test)]
    BETWEEN_READ_AND_WRITE.with(|seam| {
        if let Some(f) = seam.borrow().as_ref() {
            f();
        }
    });
}

/// [`vocabulary`] from the stored bytes (`None`: nothing stored), so a store transaction can
/// read the words too (`crate::word_choices`).
pub(crate) fn vocabulary_of(bytes: Option<&[u8]>) -> (Vocabulary, Option<UnknownSetting>) {
    let refused = |why: String| format!("a word list this version refuses: {why}");
    // Parsed as plain strings first, so the reason names the rule broken (the core check),
    // never a word.
    crate::local::setting_of(
        VOCABULARY_KEY,
        bytes,
        |bytes| match serde_json::from_slice::<Vec<String>>(bytes) {
            Ok(words) => {
                Vocabulary::new(stored_without_quotes(&words)).map_err(|e| refused(e.to_string()))
            }
            Err(_) => Err(refused("it is not a list of words".into())),
        },
    )
    .or_default_with(Vocabulary::default)
}

/// Replaces the whole list, for a caller that shows the user a list and stores their edit of it
/// (the benchmark's scratch store, the shared case table): `base` is the list the caller read,
/// `words` the list to store (checked by [`Vocabulary::new`]: trimmed, empty ones and repeats
/// dropped, within the limits, one line each; empty stores no words). Returns what was stored:
/// while Dictionary sync is on, the list rebuilt from the rows ([`rebuild_tx`]).
///
/// One transaction: the stored list is read inside it and must still be `base` (as
/// [`Vocabulary::new`] reads both), or nothing is written ([`SetVocabularyError::Stale`]), so a
/// word another computer added after the caller read the list is never removed by this save.
/// The rows then follow the change from `base`: a word that left it has every live row of it
/// removed, a word new to it gets a row unless a live one exists. A stored word that is not in
/// use is not on `base`, so it is never removed here. The Dictionary page edits one word at a
/// time instead ([`add_entry`], [`remove_entry`]).
///
/// # Errors
/// [`SetVocabularyError`]: nothing is stored on a refusal.
pub fn set_vocabulary<B: AsRef<str>, S: AsRef<str>>(
    services: &Services,
    base: &[B],
    words: &[S],
) -> Result<Vocabulary, SetVocabularyError> {
    let vocabulary = Vocabulary::new(words).map_err(SetVocabularyError::Refused)?;
    let base = Vocabulary::new(base).map_err(|_| SetVocabularyError::Stale)?;
    with_stamp(
        services,
        |tx, stamp| -> Result<Vocabulary, SetVocabularyError> {
            let (stored, _refused) = vocabulary_of(tx.local_value(VOCABULARY_KEY)?.as_deref());
            if stored.words() != base.words() {
                return Err(SetVocabularyError::Stale);
            }
            // Words from before rows existed get theirs first, so a removal reaches them too.
            reconcile_tx(tx, stamp)?;
            for gone in base
                .words()
                .iter()
                .filter(|w| !listed_in(vocabulary.words(), w))
            {
                remove_rows_tx(tx, stamp, gone)?;
            }
            let rows = tx.dictionary_words()?;
            for new in vocabulary
                .words()
                .iter()
                .filter(|w| !listed_in(base.words(), w) && !has_live_row(&rows, w))
            {
                tx.put_dictionary_word(DictionaryWord::new(new.clone(), stamp))?;
            }
            if let Some(rebuilt) = rebuild_tx(tx, OverRefused::Replace)? {
                return Ok(rebuilt);
            }
            write_list_tx(tx, &vocabulary)?;
            Ok(vocabulary.clone())
        },
    )
}

fn store_err(e: StoreError) -> SetVocabularyError {
    SetVocabularyError::Store(e.into())
}

impl From<StoreError> for SetVocabularyError {
    fn from(e: StoreError) -> Self {
        store_err(e)
    }
}

impl From<earshot_core::model::RepoError> for SetVocabularyError {
    fn from(e: earshot_core::model::RepoError) -> Self {
        Self::Store(e.into())
    }
}

/// What [`add_entry`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AddedEntry {
    /// The entry as the Dictionary holds it now ("Undo" removes exactly this).
    pub word: String,
    /// False when the Dictionary already had it (case ignored): nothing was written.
    pub added: bool,
    /// The list in use after the add.
    pub words: Vocabulary,
}

/// Adds one entry typed on the Dictionary page (read by [`Vocabulary::new`]'s rules: trimmed,
/// inner white space joined) at the end of the list. Reads the stored list and writes the row
/// and the list in one transaction, so nothing another computer added in between is lost.
///
/// An entry already listed (case ignored) writes nothing (`added` false). While Dictionary sync
/// is on and some stored words are not in use, nothing is added
/// ([`SetVocabularyError::NotInUse`]): a new word would be stored and synced but not in use
/// itself. A stored list this version refuses reads as no words, so the new list holds this
/// entry alone.
///
/// # Errors
/// [`SetVocabularyError::Refused`] (the list's limits, as the page reports them; an empty entry
/// is [`SetVocabularyError::NotAWord`]), [`SetVocabularyError::NotInUse`]; store errors. Nothing
/// is stored on an error.
pub fn add_entry(services: &Services, text: &str) -> Result<AddedEntry, SetVocabularyError> {
    let entry = Vocabulary::new([text]).map_err(SetVocabularyError::Refused)?;
    let word = entry
        .words()
        .first()
        .cloned()
        .ok_or(SetVocabularyError::NotAWord)?;
    with_stamp(
        services,
        |tx, stamp| -> Result<AddedEntry, SetVocabularyError> {
            let (current, _refused) = vocabulary_of(tx.local_value(VOCABULARY_KEY)?.as_deref());
            between_read_and_write();
            if let Some(entry) = current
                .words()
                .iter()
                .find(|e| e.to_lowercase() == word.to_lowercase())
            {
                return Ok(AddedEntry {
                    word: entry.clone(),
                    added: false,
                    words: current.clone(),
                });
            }
            // A quick refusal while the joined list already leaves words out; the check that
            // decides is on the rebuilt list below.
            let syncs = syncs_from(|k| tx.local_value(k)).map_err(SetVocabularyError::Store)?;
            if syncs && joined(&tx.dictionary_words()?).1 > 0 {
                return Err(SetVocabularyError::NotInUse);
            }
            let mut next: Vec<String> = current.into();
            next.push(word.clone());
            let next = Vocabulary::new(&next).map_err(SetVocabularyError::Refused)?;
            reconcile_tx(tx, stamp)?;
            let rows = tx.dictionary_words()?;
            if !has_live_row(&rows, &word) {
                tx.put_dictionary_word(DictionaryWord::new(word.clone(), stamp))?;
            }
            let words = match rebuild_tx(tx, OverRefused::Replace)? {
                // Decided on the outcome: a word the rebuilt list leaves out would be stored
                // and synced but not in use, so the whole add rolls back (the stored list may
                // lag the rows, for example after a refused list was kept or sync was off).
                Some(rebuilt) if !listed_in(rebuilt.words(), &word) => {
                    return Err(SetVocabularyError::NotInUse);
                }
                Some(rebuilt) => rebuilt,
                None => {
                    write_list_tx(tx, &next)?;
                    next
                }
            };
            Ok(AddedEntry {
                word,
                added: true,
                words,
            })
        },
    )
}

/// Removes the Dictionary entry that is `word` (case ignored; surrounding white space ignored,
/// inner runs read as one space, as [`Vocabulary::new`] stores entries) and every live row of
/// it, reading the stored list and writing in one transaction. Returns whether an entry went; a
/// word not listed writes nothing. An entry that only contains `word` (a phrase) stays.
///
/// # Errors
/// Store errors.
pub fn remove_entry(services: &Services, word: &str) -> Result<bool, SetVocabularyError> {
    let word = word.split_whitespace().collect::<Vec<_>>().join(" ");
    if word.is_empty() {
        return Ok(false);
    }
    with_stamp(services, |tx, stamp| -> Result<bool, SetVocabularyError> {
        let (current, _refused) = vocabulary_of(tx.local_value(VOCABULARY_KEY)?.as_deref());
        between_read_and_write();
        if !listed_in(current.words(), &word) {
            return Ok(false);
        }
        reconcile_tx(tx, stamp)?;
        remove_rows_tx(tx, stamp, &word)?;
        if rebuild_tx(tx, OverRefused::Replace)?.is_none() {
            let wanted = word.to_lowercase();
            let kept: Vec<&String> = current
                .words()
                .iter()
                .filter(|e| e.to_lowercase() != wanted)
                .collect();
            let kept = Vocabulary::new(kept).unwrap_or_default();
            write_list_tx(tx, &kept)?;
        }
        Ok(true)
    })
}

// Adding words from the transcript (T12.15f): one word at a time, from a click on that word
// (a word of the line's text, or a button of the bar under a line just edited); never
// automatic. Both directions go through the page's own one-word edits ([`add_entry`],
// [`remove_entry`]), so the limits and refusals are the Dictionary page's own.

/// The word a transcript token adds to the Dictionary: its core (no surrounding punctuation,
/// `earshot_core::vocabulary::suggest::split_punctuation`, the matcher's own rule), when the
/// core holds a letter; `None` for a token that is not a word (punctuation, a number). The UI
/// decides which words get "Add to Dictionary" by the same rule
/// (`fixtures/ui/dictionary_word_cases.json`, which both suites run).
#[must_use]
pub fn dictionary_word(token: &str) -> Option<&str> {
    let core = without_single_quotes(split_punctuation(token.trim()).1);
    core.chars().any(char::is_alphabetic).then_some(core)
}

/// `core` without leading and trailing single quotes (T12.45: `'Zorbek'` adds `Zorbek`), split
/// again after each strip so punctuation inside the quotes goes too (`'Zorbek.'`). An
/// apostrophe inside the word stays (`don't`). Kept apart from the list's own checks.
fn without_single_quotes(mut core: &str) -> &str {
    loop {
        let stripped = core.trim_matches('\'');
        if stripped.len() == core.len() {
            return core;
        }
        core = split_punctuation(stripped).1;
    }
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
        .map(|t| without_single_quotes(split_punctuation(t).1))
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

/// Adds the word of `token` ([`dictionary_word`]) at the end of the stored Dictionary
/// ([`add_entry`]: one transaction). A word already there (case ignored) writes nothing. A
/// stored list this version refuses reads as no words ([`vocabulary`]), so the new list holds
/// this word alone, as an add on the Dictionary page does.
///
/// # Errors
/// [`SetVocabularyError::NotAWord`] for a token with no word; [`SetVocabularyError::Refused`]
/// (the list's limits, as the Dictionary page reports them); [`SetVocabularyError::NotInUse`];
/// store errors. Nothing is stored on an error.
pub fn add_word(services: &Services, token: &str) -> Result<AddedWord, SetVocabularyError> {
    let word = dictionary_word(token).ok_or(SetVocabularyError::NotAWord)?;
    let added = add_entry(services, word)?;
    Ok(AddedWord {
        word: added.word,
        added: added.added,
    })
}

/// Removes the Dictionary entry that is `word` ([`remove_entry`]: one transaction).
///
/// # Errors
/// Store errors.
pub fn remove_word(services: &Services, word: &str) -> Result<bool, SetVocabularyError> {
    remove_entry(services, word)
}
