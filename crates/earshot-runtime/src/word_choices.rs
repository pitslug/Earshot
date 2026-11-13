//! Suggestions beside words (T12.15e): the word list the transcript is checked against, the
//! user's choices on suggested words, and what the meeting detail marks.
//!
//! The list ([`word_list`]) is built here and nowhere else: the Dictionary's entries
//! ([`crate::vocabulary`]) split into white-space words, then the white-space words of every
//! saved person's name (live persons only: removed and merged-away persons are left out), each
//! kept in the first form seen and dropped when it repeats an earlier word in another case
//! (the matcher reads a word listed twice as two candidates, so it would never be suggested).
//! The user's own person is listed only once they gave it a name: the default "Me" is not a
//! word anyone says, and a set name is said in meetings like anyone else's. People are listed
//! by name (ASCII case ignored), as the Speakers panel lists them. The list stays on this
//! computer: no summary input reads it (people's names reach a summary only as the speakers
//! of that meeting), and nothing here logs a word.
//!
//! The matcher is `earshot_core::vocabulary::suggest` ([`Suggester`]), unchanged.
//!
//! The user's choices live in the device-local record `word-choices/<meeting>`
//! ([`crate::local::MeetingRecord::WordChoices`]): `version u32 LE | JSON list` of
//! [`WordChoice`]. It holds transcript words, so it is content: meeting deletion erases it
//! with the meeting's other records. A record this version cannot read is no choices, never
//! an error, and the next choice replaces it. A choice whose segment is no longer a live line
//! of the final transcript, or whose token no longer reads as recorded (the written token for
//! [`Choice::Changed`], the decoded token for [`Choice::Kept`]), is ignored, and dropped at
//! the next write: a re-transcription that replaced a line, or a hand edit of the line, ends
//! it harmlessly. The final pass drops such choices in its own write transaction
//! (`drop_stale_tx`), so a replaced line's words do not outlive it. The edits themselves are `crate::edit::use_word`, `put_back_word` and
//! `keep_word`.

use std::collections::{BTreeMap, HashSet};

use earshot_core::identity::EntityId;
use earshot_core::model::{MeetingRepo, TranscriptPass};
use earshot_core::vocabulary::suggest::{Suggester, split_punctuation};
use earshot_core::voiceprint::{DEFAULT_ME_NAME, Person, VoiceprintRepo};
use earshot_store::Store;
use serde::{Deserialize, Serialize};

use crate::RuntimeError;
use crate::local::{LocalKey, MeetingRecord};
use crate::view::MeetingView;

/// The record's version, written before its JSON payload.
pub const WORD_CHOICES_RECORD_VERSION: u32 = 1;

/// What the user chose for one suggested word.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Choice {
    /// "Keep": the decoded word stays, and it is not suggested again in this meeting.
    Kept,
    /// "Use it" or another listed word: the token now reads `written` (punctuation kept).
    Changed {
        /// The token as written now.
        written: String,
    },
}

/// One choice: which token of which line, the token as the recogniser wrote it (punctuation
/// kept), and the choice.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WordChoice {
    /// The transcript segment (line).
    pub segment: EntityId,
    /// Index among the line's white-space tokens.
    pub token: usize,
    /// The token as decoded.
    pub decoded: String,
    /// What the user chose.
    pub choice: Choice,
}

impl WordChoice {
    /// What the token must read for the choice to still hold.
    fn expected(&self) -> &str {
        match &self.choice {
            Choice::Kept => &self.decoded,
            Choice::Changed { written } => written,
        }
    }

    /// Whether the choice still holds for `lines` (each live final line's id and text).
    pub(crate) fn holds(&self, lines: &BTreeMap<EntityId, &str>) -> bool {
        lines
            .get(&self.segment)
            .and_then(|text| text.split_whitespace().nth(self.token))
            .is_some_and(|t| t == self.expected())
    }
}

/// The record's key for `meeting`.
pub(crate) fn record_key(meeting: EntityId) -> LocalKey {
    MeetingRecord::WordChoices.key(&[], meeting, &[])
}

/// The record's value: `version u32 LE | JSON list`.
///
/// # Errors
/// `Internal` when the list cannot be encoded.
pub(crate) fn encode(choices: &[WordChoice]) -> Result<Vec<u8>, RuntimeError> {
    let mut out = WORD_CHOICES_RECORD_VERSION.to_le_bytes().to_vec();
    serde_json::to_writer(&mut out, choices)
        .map_err(|_| RuntimeError::Internal("word choices encoding"))?;
    Ok(out)
}

/// The choices of a stored record; a record this version cannot read (another version, a
/// damaged payload) is no choices.
#[must_use]
pub(crate) fn decode(bytes: &[u8]) -> Vec<WordChoice> {
    let Some((head, body)) = bytes.split_first_chunk::<4>() else {
        return Vec::new();
    };
    if u32::from_le_bytes(*head) != WORD_CHOICES_RECORD_VERSION {
        return Vec::new();
    }
    serde_json::from_slice(body).unwrap_or_default()
}

/// Drops from `meeting`'s record every choice that no longer holds for its live final
/// transcript (module doc): the final pass calls it in its write transaction, after writing
/// the new lines, so the words of lines it replaced do not stay on this computer. A record
/// left with no choice is erased, and one this version cannot read is left alone (the next
/// choice replaces it).
///
/// # Errors
/// Store errors.
pub(crate) fn drop_stale_tx(
    tx: &mut earshot_store::StoreTx<'_>,
    meeting: EntityId,
) -> Result<(), RuntimeError> {
    let key = record_key(meeting);
    let Some(bytes) = tx.local_value(key.as_str())? else {
        return Ok(());
    };
    let choices = decode(&bytes);
    if choices.is_empty() {
        return Ok(());
    }
    let rows = tx.segments(meeting)?;
    let lines: BTreeMap<EntityId, &str> = rows
        .iter()
        .filter(|r| r.pass == TranscriptPass::Final && !r.meta.deleted)
        .map(|r| (r.segment.id, r.segment.text.as_str()))
        .collect();
    let holding: Vec<WordChoice> = choices
        .iter()
        .filter(|c| c.holds(&lines))
        .cloned()
        .collect();
    if holding.len() == choices.len() {
        return Ok(());
    }
    if holding.is_empty() {
        tx.delete_local_value(key.as_str())?;
    } else {
        crate::local::set_tx(tx, &key, &encode(&holding)?)?;
    }
    Ok(())
}

/// The list from the Dictionary's entries and the saved persons (module doc).
#[must_use]
pub fn list_of<S: AsRef<str>>(dictionary: &[S], persons: &[Person]) -> Vec<String> {
    let mut people: Vec<&Person> = persons
        .iter()
        .filter(|p| p.is_live() && !(p.is_me && p.display_name == DEFAULT_ME_NAME))
        .collect();
    people.sort_by(|a, b| {
        a.display_name
            .to_ascii_lowercase()
            .cmp(&b.display_name.to_ascii_lowercase())
            .then(a.id.cmp(&b.id))
    });
    let mut seen = HashSet::new();
    dictionary
        .iter()
        .flat_map(|e| e.as_ref().split_whitespace())
        .chain(
            people
                .iter()
                .flat_map(|p| p.display_name.split_whitespace()),
        )
        .filter(|w| seen.insert(w.to_lowercase()))
        .map(str::to_owned)
        .collect()
}

/// The list as stored now (module doc); a Dictionary value this version refuses counts as no
/// words.
fn list_from<R: VoiceprintRepo + ?Sized>(
    repo: &R,
    vocabulary: Option<&[u8]>,
) -> Result<Vec<String>, RuntimeError> {
    let (vocabulary, _refused) = crate::vocabulary::vocabulary_of(vocabulary);
    let persons = repo.persons().map_err(RuntimeError::Voiceprint)?;
    Ok(list_of(vocabulary.words(), &persons))
}

/// The word list (module doc), from a store the caller has locked.
///
/// # Errors
/// Store errors.
pub fn word_list(store: &Store) -> Result<Vec<String>, RuntimeError> {
    let bytes = store.local_value(crate::local::DeviceRecord::Vocabulary.key().as_str())?;
    list_from(store, bytes.as_deref())
}

/// [`word_list`] inside a store transaction.
pub(crate) fn word_list_tx(tx: &earshot_store::StoreTx<'_>) -> Result<Vec<String>, RuntimeError> {
    let bytes = tx.local_value(crate::local::DeviceRecord::Vocabulary.key().as_str())?;
    list_from(tx, bytes.as_deref())
}

/// The stored choices of `meeting` (none when there is no record or it cannot be read).
///
/// # Errors
/// Store errors.
pub fn word_choices(store: &Store, meeting: EntityId) -> Result<Vec<WordChoice>, RuntimeError> {
    Ok(store
        .local_value(record_key(meeting).as_str())?
        .map(|b| decode(&b))
        .unwrap_or_default())
}

/// A word the matcher offers a listed word for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WordSuggestion {
    /// Index among the line's white-space tokens.
    pub token: usize,
    /// The word as decoded, without the punctuation around it.
    pub decoded: String,
    /// The one listed word it sounds like.
    pub listed: String,
}

/// A word the user changed ([`Choice::Changed`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChangedWord {
    /// Index among the line's white-space tokens.
    pub token: usize,
    /// The word as decoded, without the punctuation around it.
    pub decoded: String,
    /// The word as written now, without the punctuation around it.
    pub written: String,
}

/// The marks of one line.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LineWords {
    /// Suggested words, in token order.
    pub suggestions: Vec<WordSuggestion>,
    /// Changed words, in token order.
    pub changed: Vec<ChangedWord>,
}

/// What the meeting detail marks: the list (for "another word") and each final line's words.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WordMarks {
    /// The word list ([`word_list`]); empty for a draft transcript.
    pub list: Vec<String>,
    /// The lines with a mark, by segment id; empty for a draft transcript.
    pub lines: BTreeMap<EntityId, LineWords>,
}

/// The core of a token (no surrounding punctuation).
fn core(token: &str) -> String {
    split_punctuation(token).1.to_owned()
}

/// The marks of `view` (module doc): nothing unless it shows the final transcript. A token
/// covered by a choice that still holds is never suggested.
///
/// # Errors
/// Store errors.
pub fn word_marks(store: &Store, view: &MeetingView) -> Result<WordMarks, RuntimeError> {
    if view.pass != Some(TranscriptPass::Final) {
        return Ok(WordMarks::default());
    }
    let list = word_list(store)?;
    let choices = word_choices(store, view.meeting.id)?;
    let texts: BTreeMap<EntityId, &str> = view
        .segments
        .iter()
        .map(|s| (s.id, s.text.as_str()))
        .collect();
    let holding: Vec<&WordChoice> = choices.iter().filter(|c| c.holds(&texts)).collect();
    let suggester = Suggester::new(&list);
    let mut lines = BTreeMap::new();
    for s in &view.segments {
        let mine: Vec<&&WordChoice> = holding.iter().filter(|c| c.segment == s.id).collect();
        let suggestions: Vec<WordSuggestion> = suggester
            .suggest(&s.text)
            .into_iter()
            .filter(|g| !mine.iter().any(|c| c.token == g.token))
            .map(|g| WordSuggestion {
                token: g.token,
                decoded: core(&g.decoded),
                listed: g.listed,
            })
            .collect();
        let mut changed: Vec<ChangedWord> = mine
            .iter()
            .filter_map(|c| match &c.choice {
                Choice::Changed { written } => Some(ChangedWord {
                    token: c.token,
                    decoded: core(&c.decoded),
                    written: core(written),
                }),
                Choice::Kept => None,
            })
            .collect();
        changed.sort_by_key(|c| c.token);
        if !suggestions.is_empty() || !changed.is_empty() {
            lines.insert(
                s.id,
                LineWords {
                    suggestions,
                    changed,
                },
            );
        }
    }
    Ok(WordMarks { list, lines })
}
