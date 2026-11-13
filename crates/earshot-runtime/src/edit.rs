//! Editing a stored meeting (T7.3a): what the meeting detail screen changes after the
//! final pass. Every write is one store transaction under the services lock that re-reads
//! the rows it changes and changes only its own fields; nothing here holds the lock across
//! FLAC decoding or model work.
//!
//! - Speakers: [`rename_speaker`], [`merge_speakers`], [`link_speaker`]. Each write
//!   re-stamps the speaker row, so a later re-run of the final pass reads it as a user edit
//!   and keeps it (`earshot_store::identities`: any write after the pass is an edit).
//! - The user's voice in the room: [`mark_me`] and its undo [`unmark_me`] (T9.12), in-room
//!   meetings only: the row becomes the meeting's one "Me" row, with no biometric data made.
//! - Persons: [`rename_person`] (rewrites that person's speaker rows in every meeting,
//!   since [`MeetingView`] names come from `Speaker.display_name`), [`list_persons`].
//! - Voiceprints (biometric, opt-in): [`remember_voices`] enrols named speakers from the
//!   final pass's stored identity centroids; [`forget_person_voices`] is
//!   `Store::forget_voiceprints` (zeroed, tombstoned, checkpointed). "Me" is never enrolled
//!   here: the user's own voiceprint is only ever made by the explicit "Me" enrolment
//!   (`PostConfig::enroll_me`).
//! - Summaries: one undo. `store_summary` tombstones the summary it replaces and keeps its
//!   content (tombstones sync), so the most recently tombstoned summary of a meeting is "the
//!   previous one" ([`summary_history`]); [`undo_summary`] tombstones the current one and
//!   revives the previous one with a fresh stamp, so the two swap and a second undo returns
//!   to the original. Older summaries stay tombstones. [`MeetingView`] (and so export) keeps
//!   reading the newest live summary. No schema change.
//! - Title and transcript (T12.16): [`rename_meeting`], [`edit_segment`]. An edited segment
//!   keeps its id and times (summary citations still find it) and loses its per-word times,
//!   which described the recogniser's words, not the user's (`SegmentRow::text_edited_at`).
//!   Search and export read the edited text through [`MeetingView`]. A text edit later than
//!   the summary marks the summary out of date ([`MeetingView::summary_out_of_date`]), and
//!   nothing is summarised by itself.
//! - Suggested words (T12.15e): [`use_word`], [`put_back_word`], [`keep_word`] change one
//!   token of a final line through [`edit_segment`]'s rules and record the choice in
//!   `crate::word_choices`, in one transaction.
//! - Summary text (T12.16): [`edit_summary`] corrects one field of the current summary in
//!   place and keeps the text before the edit as the previous summary, so
//!   [`undo_summary`] undoes the edit. No schema change.
//! - [`retranscribe`]: the final pass again from the stored audio, with no live reuse. It
//!   keeps every edited segment and leaves out the new words under it
//!   (`crate::post::drop_words_under_edits`), saying so in a warning
//!   (`PostWarning::EditedLinesKept`).
//! - [`speaker_sample`]: a short stretch of a speaker's audio for the "play" button.
//!
//! Merging across tracks is allowed: a `Speaker` row has no track (its segments do), the
//! final pass already treats a speaker with segments on two tracks as belonging to neither
//! track's re-run, and the user's typical reason (an echo cluster on the mic that is really
//! the far-end person) needs it. Merging the "Me" row away is refused (one "Me" row per
//! meeting); merging another speaker into it is allowed.
//!
//! A merge holds until the next re-transcription ([`retranscribe`]), which diarises again:
//! the merged speaker's identity entry is gone, so if its voice forms its own cluster again
//! it comes back as a new speaker. A cross-track merge is always undone that way (the
//! final pass matches identities on the same track only), so the echo cluster of the example
//! returns as a new mic speaker; merge it again after re-transcribing.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use earshot_core::asr::{CancelToken, MeetingTime, TranscribeOptions, Transcriber};
use earshot_core::audio::f32_to_i16;
use earshot_core::capture::TrackId;
use earshot_core::diar::{EmbeddingModelId, SpeakerEmbedder, SpeakerRole, SpeakerSegmenter};
use earshot_core::identity::{Clock, EntityId};
use earshot_core::model::{
    AudioFileRepo, Meeting, MeetingRepo, SegmentRow, Speaker, Stamp, Summary, Track,
    TranscriptPass, live_segments,
};
use earshot_core::pipeline::{FinalPassRequest, FinalPassTrack};
use earshot_core::summary::SummaryV1;
use earshot_core::vocabulary::suggest::split_punctuation;
use earshot_core::voiceprint::{
    Person, VoiceprintError, VoiceprintRepo, enroll, identity_of, me_person, resolve_person,
};
use earshot_store::{StoreTx, first_free_speaker_name};
use rand_core::Rng;

use crate::backends::{Accelerator, EvidenceLoader, ModelDir, post_processor};
use crate::host::{HostPost, HostStep, ONNX_ACCELERATOR, PostLoad, final_pass};
use crate::identities::{IdentityRecord, StoredIdentities};
use crate::post::PostConfig;
use crate::view::{MeetingView, cites_only};
use crate::word_choices::{self, Choice, WordChoice};
use crate::{MeetingJob, RuntimeError, Services, os_seeded_rng};

/// The longest name a speaker or person may get, in characters (after trimming).
pub const MAX_NAME_CHARS: usize = 100;

/// The longest meeting title, in characters (after trimming).
pub const MAX_TITLE_CHARS: usize = 200;

/// The longest text of one transcript segment the user may write, in characters (after
/// trimming). A segment is at most about 30 s of speech, a few hundred characters.
pub const MAX_SEGMENT_CHARS: usize = 5_000;

/// The longest text of one summary field the user may write, in characters (after
/// trimming); a title follows [`MAX_TITLE_CHARS`].
pub const MAX_SUMMARY_TEXT_CHARS: usize = 2_000;

/// Why an edit was refused (nothing was written).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EditError {
    /// No such meeting, or it is deleted.
    MeetingNotFound(EntityId),
    /// No such speaker in this meeting, or it is deleted (merged away).
    SpeakerNotFound(EntityId),
    /// No such person, or it is deleted or merged into another.
    PersonNotFound(EntityId),
    /// The name is unusable.
    InvalidName(NameProblem),
    /// A speaker cannot be merged into itself.
    SameSpeaker(EntityId),
    /// The meeting's "Me" row cannot be merged away (merge the other speaker into it).
    MergeAwayMe(EntityId),
    /// The speaker is not linked to a person, so there is nobody to remember the voice for.
    NoPerson(EntityId),
    /// The speaker is the user ("Me", or linked to the user's person): the user's voiceprint
    /// is only made by the explicit "Me" enrolment.
    MeVoice(EntityId),
    /// Only the meeting's "Me" row may be linked to the user's person (the final pass's
    /// naming rule: nobody else is the user).
    NotTheUser(EntityId),
    /// The meeting's "Me" row may be linked only to the user's person.
    MeIsTheUser(EntityId),
    /// The user's own person cannot be removed (their name and voice have their own controls).
    PersonIsTheUser(EntityId),
    /// This meeting (the id) is not in-room, or the voice is not a voice of the room: a
    /// voice can be marked as the user only in an in-room meeting ([`records_in_room`]). In a
    /// call "Me" comes from the mic track.
    NotInRoom(EntityId),
    /// The meeting already has its "Me" row (the id), and there is one per meeting: undo it
    /// first ([`unmark_me`]).
    MeetingHasMe(EntityId),
    /// The speaker is not the meeting's "Me" row, so there is nothing to undo.
    NotMe(EntityId),
    /// The meeting has no earlier summary to go back to.
    NoPreviousSummary,
    /// The earlier summary cites transcript segments that are no longer there (the meeting
    /// was transcribed again since): it cannot be restored.
    PreviousSummaryStale,
    /// No such folder, or it is deleted ([`crate::folders`]).
    FolderNotFound(EntityId),
    /// Another live folder (the id) already has this name, ignoring case
    /// ([`crate::folders`]). Never carries the name: it is content.
    FolderNameTaken(EntityId),
    /// The meeting (the id) is recording: it can be moved to a folder once it has ended
    /// ([`crate::folders::move_meeting`]).
    MeetingRecording(EntityId),
    /// The title is unusable ([`rename_meeting`]). Never carries the text: it is content.
    InvalidTitle(TextProblem),
    /// The transcript text is unusable ([`edit_segment`]). Never carries the text.
    InvalidText(TextProblem),
    /// No such segment in the transcript the meeting shows, or it is deleted (replaced by
    /// a re-transcription meanwhile).
    SegmentNotFound(EntityId),
    /// The meeting (the id) shows only its live draft: the final pass replaces it, so an
    /// edit would be lost. Its text can be edited once the final transcript exists.
    DraftTranscript(EntityId),
    /// The summary (the id) is no longer the meeting's current one ([`edit_summary`]): a
    /// new summary or an undo replaced it meanwhile.
    SummaryNotCurrent(EntityId),
    /// The summary has no such item ([`edit_summary`]; it changed meanwhile).
    SummaryItemNotFound,
    /// The summary text is unusable ([`edit_summary`]). Never carries the text.
    InvalidSummaryText(TextProblem),
    /// The speaker is the user or already has a name (a linked person): only a voice that
    /// needs a name can be ignored ([`ignore_speaker`], T12.19).
    NotUnnamed(EntityId),
    /// The final pass kept no voice of this speaker (no identity entry, or the meeting's
    /// identity record cannot be read), so the choice to ignore it has nowhere to be kept
    /// ([`ignore_speaker`]). A re-transcription stores one.
    NoStoredVoice(EntityId),
    /// The line (the id) has no word at that position: it changed meanwhile ([`use_word`],
    /// [`put_back_word`], [`keep_word`]).
    WordNotFound(EntityId),
    /// The word is not on the word list ([`use_word`]). Never carries the word.
    WordNotListed,
    /// The word at that position of the line (the id) was not changed from a suggestion, or
    /// the line changed since: there is nothing to put back ([`put_back_word`]).
    NoWordChoice(EntityId),
}

/// What is wrong with a title or transcript text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TextProblem {
    /// Nothing but whitespace (transcript text only: an empty title is allowed).
    Empty,
    /// Longer than the limit ([`MAX_TITLE_CHARS`], [`MAX_SEGMENT_CHARS`]).
    TooLong,
    /// A control character (a line break, a tab).
    ControlCharacter,
}

/// What is wrong with a name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NameProblem {
    /// Nothing but whitespace.
    Empty,
    /// More than [`MAX_NAME_CHARS`] characters.
    TooLong,
    /// A control character (a line break, a tab).
    ControlCharacter,
}

impl fmt::Display for EditError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MeetingNotFound(id) => write!(f, "meeting {id} does not exist or is deleted"),
            Self::SpeakerNotFound(id) => {
                write!(f, "speaker {id} is not a current speaker of this meeting")
            }
            Self::PersonNotFound(id) => write!(f, "person {id} does not exist or is deleted"),
            Self::InvalidName(NameProblem::Empty) => f.write_str("the name is empty"),
            Self::InvalidName(NameProblem::TooLong) => {
                write!(f, "the name is longer than {MAX_NAME_CHARS} characters")
            }
            Self::InvalidName(NameProblem::ControlCharacter) => {
                f.write_str("the name contains a line break or another control character")
            }
            Self::SameSpeaker(id) => write!(f, "speaker {id} cannot be merged into itself"),
            Self::MergeAwayMe(id) => write!(
                f,
                "speaker {id} is \"Me\" and cannot be merged away; merge the other speaker into it"
            ),
            Self::NoPerson(id) => write!(
                f,
                "speaker {id} is not linked to a person; name or link it first"
            ),
            Self::MeVoice(id) => write!(
                f,
                "speaker {id} is you; your voice is remembered only through \"Me\" enrolment"
            ),
            Self::NotTheUser(id) => write!(
                f,
                "speaker {id} is not \"Me\" and cannot be linked to you; only the \"Me\" speaker is you"
            ),
            Self::MeIsTheUser(id) => {
                write!(f, "speaker {id} is \"Me\" and can be linked only to you")
            }
            Self::PersonIsTheUser(id) => write!(f, "person {id} is you and cannot be removed"),
            Self::NotInRoom(id) => write!(
                f,
                "meeting {id} is not an in-room meeting, so no voice in it can be marked as you"
            ),
            Self::MeetingHasMe(id) => {
                write!(f, "speaker {id} is already \"Me\" in this meeting")
            }
            Self::NotMe(id) => write!(f, "speaker {id} is not \"Me\" in this meeting"),
            Self::NoPreviousSummary => f.write_str("there is no previous summary"),
            Self::PreviousSummaryStale => f.write_str(
                "the previous summary cites a transcript that has been replaced; summarise again",
            ),
            Self::FolderNotFound(id) => write!(f, "folder {id} does not exist or is deleted"),
            Self::FolderNameTaken(id) => {
                write!(f, "folder {id} already has this name (case is ignored)")
            }
            Self::MeetingRecording(id) => write!(
                f,
                "meeting {id} is recording; it can be moved to a folder once it has ended"
            ),
            Self::InvalidTitle(p) => write!(f, "the title {}", text_problem(*p, MAX_TITLE_CHARS)),
            Self::InvalidText(p) => write!(f, "the text {}", text_problem(*p, MAX_SEGMENT_CHARS)),
            Self::SegmentNotFound(id) => {
                write!(f, "segment {id} is not in this meeting's transcript")
            }
            Self::DraftTranscript(id) => write!(
                f,
                "meeting {id} has only its live draft; edit the text once the final transcript exists"
            ),
            Self::SummaryNotCurrent(id) => {
                write!(f, "summary {id} is no longer the meeting's current summary")
            }
            Self::SummaryItemNotFound => f.write_str("the summary has no such item"),
            Self::InvalidSummaryText(p) => {
                write!(f, "the text {}", text_problem(*p, MAX_SUMMARY_TEXT_CHARS))
            }
            Self::NotUnnamed(id) => write!(
                f,
                "speaker {id} is you or has a name; only a voice that needs a name can be ignored"
            ),
            Self::NoStoredVoice(id) => write!(
                f,
                "no voice of speaker {id} is stored, so ignoring it cannot be kept; transcribe the meeting again first"
            ),
            Self::WordNotFound(id) => {
                write!(f, "segment {id} has no word at that position")
            }
            Self::WordNotListed => f.write_str("the word is not on the word list"),
            Self::NoWordChoice(id) => {
                write!(
                    f,
                    "segment {id} has no changed word at that position to put back"
                )
            }
        }
    }
}

impl std::error::Error for EditError {}

fn text_problem(p: TextProblem, max: usize) -> String {
    match p {
        TextProblem::Empty => "is empty".into(),
        TextProblem::TooLong => format!("is longer than {max} characters"),
        TextProblem::ControlCharacter => {
            "contains a line break or another control character".into()
        }
    }
}

/// `text` trimmed, if it fits in `max` characters with no control character; empty only
/// when `allow_empty`.
fn clean_text(text: &str, max: usize, allow_empty: bool) -> Result<String, TextProblem> {
    let text = text.trim();
    if text.is_empty() && !allow_empty {
        return Err(TextProblem::Empty);
    }
    if text.chars().count() > max {
        return Err(TextProblem::TooLong);
    }
    if text.chars().any(char::is_control) {
        return Err(TextProblem::ControlCharacter);
    }
    Ok(text.to_owned())
}

/// `title` trimmed, if it is usable for a meeting: at most [`MAX_TITLE_CHARS`], no control
/// character. Empty is allowed: the meeting is untitled again, and its next summary may
/// give it one.
///
/// # Errors
/// [`EditError::InvalidTitle`].
pub fn clean_title(title: &str) -> Result<String, EditError> {
    clean_text(title, MAX_TITLE_CHARS, true).map_err(EditError::InvalidTitle)
}

/// `text` trimmed, if it is usable for a transcript segment: not empty, at most
/// [`MAX_SEGMENT_CHARS`], no control character.
///
/// # Errors
/// [`EditError::InvalidText`].
pub fn clean_segment_text(text: &str) -> Result<String, EditError> {
    clean_text(text, MAX_SEGMENT_CHARS, false).map_err(EditError::InvalidText)
}

/// Gives meeting `meeting` the title `title` (trimmed, [`clean_title`]), changing only the
/// title of the row re-read in the transaction. Returns whether the row changed (the
/// current title writes nothing).
///
/// # Errors
/// [`EditError`] (unknown or deleted meeting, unusable title); store errors.
pub fn rename_meeting(
    services: &Services,
    meeting: EntityId,
    title: &str,
) -> Result<bool, RuntimeError> {
    let title = clean_title(title)?;
    write(services, |tx, stamp| {
        let mut row = live_meeting(&*tx, meeting)?;
        if row.title == title {
            return Ok(false);
        }
        row.title = title;
        row.meta.touch(stamp.hlc, stamp.clock);
        tx.put_meeting(row)?;
        Ok(true)
    })
}

/// Replaces the text of segment `segment` of `meeting`'s final transcript with `text`
/// (trimmed, [`clean_segment_text`]). The segment keeps its id, track, speaker, start and
/// end, so summary citations and the speaker's audio still find it; its words are dropped
/// (their times described the recogniser's words) and `SegmentRow::text_edited_at` records
/// the edit, which a re-transcription keeps and which marks an older summary out of date.
/// Returns whether the row changed (the current text writes nothing).
///
/// Only the final transcript can be edited: a meeting showing its live draft refuses
/// ([`EditError::DraftTranscript`]), because its final pass would replace the draft. A
/// final pass running meanwhile cannot lose the edit: it re-reads the rows in its own
/// transaction, and an edit after it finds the replaced segment gone
/// ([`EditError::SegmentNotFound`]).
///
/// Refused while a live session, a final pass or a summary works on the meeting
/// (`line_edit_claim`): a summary run would otherwise be discarded at its store.
///
/// # Errors
/// [`EditError`] (unknown or deleted meeting or segment, a draft transcript, unusable
/// text); [`RuntimeError::MeetingBusy`] (retryable) while the meeting is busy; store errors.
pub fn edit_segment(
    services: &Services,
    meeting: EntityId,
    segment: EntityId,
    text: &str,
) -> Result<bool, RuntimeError> {
    let text = clean_segment_text(text)?;
    let _claim = line_edit_claim(services, meeting)?;
    write(services, |tx, stamp| {
        let (row, _) = final_line(tx, meeting, segment)?;
        put_line_text(tx, stamp, row, text)
    })
}

/// [`edit_segment`], returning the words the bar under the line offers to add to the
/// Dictionary (T12.15f, `crate::vocabulary::words_to_offer`: the words the edit wrote that are
/// neither common nor on the word list), compared with the line's text in the same
/// transaction as the write. An edit that changes nothing offers nothing. Nothing is added
/// here: a word joins the Dictionary only from a click on it (`crate::vocabulary::add_word`).
///
/// # Errors
/// As [`edit_segment`].
pub fn edit_segment_offering(
    services: &Services,
    meeting: EntityId,
    segment: EntityId,
    text: &str,
) -> Result<Vec<String>, RuntimeError> {
    let text = clean_segment_text(text)?;
    let _claim = line_edit_claim(services, meeting)?;
    write(services, |tx, stamp| {
        let (row, _) = final_line(tx, meeting, segment)?;
        let before = row.segment.text.clone();
        let after = text.clone();
        if !put_line_text(tx, stamp, row, text)? {
            return Ok(Vec::new());
        }
        let listed = word_choices::word_list_tx(tx)?;
        Ok(crate::vocabulary::words_to_offer(&before, &after, &listed))
    })
}

/// Claims `meeting` for one line edit, refused with retryable [`RuntimeError::MeetingBusy`]
/// while a live session records it or a final pass or summary works on it or waits to
/// ([`Services::lease_meeting_alone`]). A summary is stored only over the transcript it
/// read (`crate::summary::store_summary` conflicts on a text edit), so an edit during the
/// run would discard the whole run: the edit waits instead, as summary edits do. The claim
/// is an [`MeetingJob::Other`] lease, held across the write only.
fn line_edit_claim(
    services: &Services,
    meeting: EntityId,
) -> Result<crate::MeetingLease, RuntimeError> {
    services.lease_meeting_alone(meeting, MeetingJob::Other)
}

/// The live final-transcript row `segment` of the live meeting `meeting`, with every live
/// final line's id and text (for word choices), by [`edit_segment`]'s refusals.
fn final_line(
    tx: &StoreTx<'_>,
    meeting: EntityId,
    segment: EntityId,
) -> Result<(SegmentRow, BTreeMap<EntityId, String>), RuntimeError> {
    live_meeting(tx, meeting)?;
    let rows = tx.segments(meeting)?;
    let lines: BTreeMap<EntityId, String> = rows
        .iter()
        .filter(|r| r.pass == TranscriptPass::Final && !r.meta.deleted)
        .map(|r| (r.segment.id, r.segment.text.clone()))
        .collect();
    let Some(row) = rows
        .into_iter()
        .find(|r| r.segment.id == segment && !r.meta.deleted)
    else {
        return Err(EditError::SegmentNotFound(segment).into());
    };
    if row.pass != TranscriptPass::Final {
        return Err(if lines.is_empty() {
            EditError::DraftTranscript(meeting)
        } else {
            // A draft row of a meeting that has a final transcript is not shown.
            EditError::SegmentNotFound(segment)
        }
        .into());
    }
    Ok((row, lines))
}

/// Writes `text` (already cleaned) into `row` as a user edit ([`edit_segment`]); returns
/// whether the row changed.
fn put_line_text(
    tx: &mut StoreTx<'_>,
    stamp: &mut DynStamp<'_, '_>,
    mut row: SegmentRow,
    text: String,
) -> Result<bool, RuntimeError> {
    if row.segment.text == text {
        return Ok(false);
    }
    row.segment.text = text;
    row.segment.words.clear();
    row.meta.touch(stamp.hlc, stamp.clock);
    row.text_edited_at = Some(row.meta.updated_at);
    tx.put_segment(row)?;
    Ok(true)
}

/// The byte range of every white-space token of `text`, in order (the tokens of
/// `str::split_whitespace`, which the word matcher counts).
fn token_ranges(text: &str) -> Vec<std::ops::Range<usize>> {
    let mut out = Vec::new();
    let mut start = None;
    for (i, c) in text.char_indices() {
        match (c.is_whitespace(), start) {
            (true, Some(s)) => {
                out.push(s..i);
                start = None;
            }
            (false, None) => start = Some(i),
            _ => {}
        }
    }
    if let Some(s) = start {
        out.push(s..text.len());
    }
    out
}

/// Token `token` of `row` (its byte range and text), if it holds a word.
fn word_at(
    row: &SegmentRow,
    token: usize,
) -> Result<(std::ops::Range<usize>, String), RuntimeError> {
    let text = &row.segment.text;
    token_ranges(text)
        .into_iter()
        .nth(token)
        .map(|r| (r.clone(), text[r].to_owned()))
        .filter(|(_, t)| !split_punctuation(t).1.is_empty())
        .ok_or_else(|| EditError::WordNotFound(row.segment.id).into())
}

/// The meeting's word choices that still hold for `lines` (`crate::word_choices`).
fn holding_choices(
    tx: &StoreTx<'_>,
    meeting: EntityId,
    lines: &BTreeMap<EntityId, String>,
) -> Result<Vec<WordChoice>, RuntimeError> {
    let lines: BTreeMap<EntityId, &str> = lines.iter().map(|(k, v)| (*k, v.as_str())).collect();
    Ok(tx
        .local_value(word_choices::record_key(meeting).as_str())?
        .map(|b| word_choices::decode(&b))
        .unwrap_or_default()
        .into_iter()
        .filter(|c| c.holds(&lines))
        .collect())
}

/// Stores `choices` as the meeting's word choices; none erases the record.
fn put_choices(
    tx: &mut StoreTx<'_>,
    meeting: EntityId,
    choices: &[WordChoice],
) -> Result<(), RuntimeError> {
    let key = word_choices::record_key(meeting);
    if choices.is_empty() {
        tx.delete_local_value(key.as_str())?;
    } else {
        crate::local::set_tx(tx, &key, &word_choices::encode(choices)?)?;
    }
    Ok(())
}

/// "Use it", or another listed word (T12.15e): token `token` (counted among the white-space
/// tokens) of line `segment` of `meeting`'s final transcript becomes `word`, its punctuation
/// kept, and the choice is recorded (`crate::word_choices`), in one transaction. `word` must
/// be on the word list ([`word_choices::word_list`], any case; the listed form is written).
/// The line is edited by [`edit_segment`]'s rules: final transcript only, marked edited
/// (kept by a re-transcription), and a summary older than the edit is out of date. A token
/// changed before keeps the word the recogniser wrote, so "Put back" still restores it, and
/// choosing that word again drops the choice as [`put_back_word`] does. Returns whether the
/// line's text changed.
///
/// # Errors
/// [`EditError`] ([`edit_segment`]'s refusals, [`EditError::WordNotFound`] for a token that
/// is not there or holds no word, [`EditError::WordNotListed`]); store errors.
pub fn use_word(
    services: &Services,
    meeting: EntityId,
    segment: EntityId,
    token: usize,
    word: &str,
) -> Result<bool, RuntimeError> {
    let _claim = line_edit_claim(services, meeting)?;
    write(services, |tx, stamp| {
        let (row, lines) = final_line(tx, meeting, segment)?;
        let (range, current) = word_at(&row, token)?;
        let wanted = word.trim().to_lowercase();
        let listed = word_choices::word_list_tx(tx)?
            .into_iter()
            .find(|w| w.to_lowercase() == wanted)
            .ok_or(EditError::WordNotListed)?;
        let mut choices = holding_choices(tx, meeting, &lines)?;
        let earlier = choices
            .iter()
            .position(|c| c.segment == segment && c.token == token)
            .map(|i| choices.remove(i));
        let decoded = match earlier {
            Some(WordChoice {
                decoded,
                choice: Choice::Changed { .. },
                ..
            }) => decoded,
            _ => current.clone(),
        };
        let (pre, _, post) = split_punctuation(&current);
        let written = format!("{pre}{listed}{post}");
        if written != decoded {
            choices.push(WordChoice {
                segment,
                token,
                decoded,
                choice: Choice::Changed {
                    written: written.clone(),
                },
            });
        }
        let mut text = row.segment.text.clone();
        text.replace_range(range, &written);
        let text = clean_segment_text(&text)?;
        put_choices(tx, meeting, &choices)?;
        put_line_text(tx, stamp, row, text)
    })
}

/// "Put back" (T12.15e): token `token` of line `segment` reads the word the recogniser wrote
/// again and its choice is dropped, in one transaction. The line stays marked edited (its
/// word times went with the change). Returns whether the line's text changed.
///
/// # Errors
/// [`EditError`] ([`edit_segment`]'s refusals, [`EditError::NoWordChoice`] when the token
/// has no changed word that still reads as recorded); store errors.
pub fn put_back_word(
    services: &Services,
    meeting: EntityId,
    segment: EntityId,
    token: usize,
) -> Result<bool, RuntimeError> {
    let _claim = line_edit_claim(services, meeting)?;
    write(services, |tx, stamp| {
        let (row, lines) = final_line(tx, meeting, segment)?;
        let mut choices = holding_choices(tx, meeting, &lines)?;
        let Some(i) = choices.iter().position(|c| {
            c.segment == segment && c.token == token && matches!(c.choice, Choice::Changed { .. })
        }) else {
            return Err(EditError::NoWordChoice(segment).into());
        };
        let choice = choices.remove(i);
        let (range, _) = word_at(&row, token)?;
        let mut text = row.segment.text.clone();
        text.replace_range(range, &choice.decoded);
        let text = clean_segment_text(&text)?;
        put_choices(tx, meeting, &choices)?;
        put_line_text(tx, stamp, row, text)
    })
}

/// "Keep" (T12.15e): token `token` of line `segment` stays as decoded and is not suggested
/// again in this meeting (`crate::word_choices`); the line is not written. Returns whether a
/// choice was recorded (a token whose choice still holds records nothing).
///
/// # Errors
/// [`EditError`] ([`edit_segment`]'s refusals, [`EditError::WordNotFound`]); store errors.
pub fn keep_word(
    services: &Services,
    meeting: EntityId,
    segment: EntityId,
    token: usize,
) -> Result<bool, RuntimeError> {
    write(services, |tx, _| {
        let (row, lines) = final_line(tx, meeting, segment)?;
        let (_, current) = word_at(&row, token)?;
        let mut choices = holding_choices(tx, meeting, &lines)?;
        if choices
            .iter()
            .any(|c| c.segment == segment && c.token == token)
        {
            return Ok(false);
        }
        choices.push(WordChoice {
            segment,
            token,
            decoded: current,
            choice: Choice::Kept,
        });
        put_choices(tx, meeting, &choices)?;
        Ok(true)
    })
}

/// A text field of a summary the user may correct (T12.16). Indices count from 0 in the
/// stored order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SummaryField {
    /// The summary's title.
    Title,
    /// The TL;DR.
    Tldr,
    /// A decision's text.
    Decision(usize),
    /// Who an action item is for.
    ActionOwner(usize),
    /// What an action item is.
    ActionTask(usize),
    /// When an action item is due ("" removes it).
    ActionDue(usize),
    /// A notes section's heading.
    NoteHeading(usize),
    /// A notes bullet: the section, then the bullet.
    NoteBullet(usize, usize),
    /// An open question.
    OpenQuestion(usize),
}

/// Writes `text` into `field` of `summary`; `Ok(false)` when it already holds it.
fn set_summary_field(
    summary: &mut SummaryV1,
    field: SummaryField,
    text: String,
) -> Result<bool, EditError> {
    let missing = || EditError::SummaryItemNotFound;
    let slot: &mut String = match field {
        SummaryField::Title => &mut summary.title,
        SummaryField::Tldr => &mut summary.tldr,
        SummaryField::Decision(i) => &mut summary.decisions.get_mut(i).ok_or_else(missing)?.text,
        SummaryField::ActionOwner(i) => {
            &mut summary.action_items.get_mut(i).ok_or_else(missing)?.owner
        }
        SummaryField::ActionTask(i) => {
            &mut summary.action_items.get_mut(i).ok_or_else(missing)?.task
        }
        SummaryField::ActionDue(i) => {
            let due = &mut summary.action_items.get_mut(i).ok_or_else(missing)?.due;
            let new = (!text.is_empty()).then_some(text);
            if *due == new {
                return Ok(false);
            }
            *due = new;
            return Ok(true);
        }
        SummaryField::NoteHeading(i) => {
            &mut summary
                .enhanced_notes
                .get_mut(i)
                .ok_or_else(missing)?
                .heading
        }
        SummaryField::NoteBullet(i, j) => {
            &mut summary
                .enhanced_notes
                .get_mut(i)
                .and_then(|n| n.bullets.get_mut(j))
                .ok_or_else(missing)?
                .text
        }
        SummaryField::OpenQuestion(i) => summary.open_questions.get_mut(i).ok_or_else(missing)?,
    };
    if *slot == text {
        return Ok(false);
    }
    *slot = text;
    Ok(true)
}

/// Corrects one text field of `meeting`'s current summary `summary` (T12.16). Citations,
/// the model and the template stay. The text is trimmed and at most
/// [`MAX_SUMMARY_TEXT_CHARS`] ([`MAX_TITLE_CHARS`] for the title) with no control
/// character; it may be empty only for an action's due date (which removes it).
///
/// Today's undo covers it: the summary as it was before the edit is kept as a tombstone
/// copy with a new id (the row's own `created_at`, so its staleness reads as the original's)
/// and becomes the previous summary, while the current row keeps its id and `created_at`
/// and is changed in place. "Switch to previous summary" ([`undo_summary`]) therefore
/// brings back the text before the last edit, and switching again returns to the edit;
/// each edit keeps one such copy. The previous summary is always the newest tombstone, so
/// after an edit the summary a re-summarise replaced is no longer reachable by undo (it
/// stays a tombstone, with its body, until the meeting is deleted). An edit does not make the summary current with the
/// transcript: [`MeetingView::summary_out_of_date`] still compares the transcript's edits
/// with the summary's creation. Returns whether anything changed (the same text writes
/// nothing).
///
/// # Errors
/// [`EditError`] (unknown or deleted meeting, a summary that is not the current one, an
/// item it does not have, unusable text); store errors.
pub fn edit_summary(
    services: &Services,
    meeting: EntityId,
    summary: EntityId,
    field: SummaryField,
    text: &str,
) -> Result<bool, RuntimeError> {
    let text = match field {
        SummaryField::Title => clean_text(text, MAX_TITLE_CHARS, false),
        SummaryField::ActionDue(_) => clean_text(text, MAX_SUMMARY_TEXT_CHARS, true),
        _ => clean_text(text, MAX_SUMMARY_TEXT_CHARS, false),
    }
    .map_err(EditError::InvalidSummaryText)?;
    write(services, |tx, stamp| {
        live_meeting(&*tx, meeting)?;
        let current = history(&*tx, meeting)?
            .current
            .filter(|s| s.id == summary)
            .ok_or(EditError::SummaryNotCurrent(summary))?;
        let mut edited = current.clone();
        if !set_summary_field(&mut edited.summary, field, text)? {
            return Ok(false);
        }
        // The text before the edit, as the previous summary (doc comment).
        let mut before = current;
        before.id = EntityId::generate(stamp.clock, &mut *stamp.rng);
        before.meta.origin_device_id = stamp.hlc.node();
        before.meta.mark_deleted(stamp.hlc, stamp.clock);
        tx.put_summary(before)?;
        edited.meta.touch(stamp.hlc, stamp.clock);
        tx.put_summary(edited)?;
        Ok(true)
    })
}

impl From<EditError> for RuntimeError {
    fn from(e: EditError) -> Self {
        Self::Edit(e)
    }
}

/// `name` trimmed, if it is usable for a speaker or a person.
///
/// # Errors
/// [`EditError::InvalidName`].
pub fn clean_name(name: &str) -> Result<String, EditError> {
    let name = name.trim();
    if name.is_empty() {
        return Err(EditError::InvalidName(NameProblem::Empty));
    }
    if name.chars().count() > MAX_NAME_CHARS {
        return Err(EditError::InvalidName(NameProblem::TooLong));
    }
    if name.chars().any(char::is_control) {
        return Err(EditError::InvalidName(NameProblem::ControlCharacter));
    }
    Ok(name.to_owned())
}

pub(crate) type DynStamp<'a, 'b> = Stamp<'a, dyn Clock + Send + Sync, dyn Rng + Send + 'b>;

/// Runs `f` in one store transaction under the services lock, with a stamp from the
/// services clock, the device's HLC and a fresh OS-seeded RNG (for new row ids).
pub(crate) fn write<T>(
    services: &Services,
    f: impl FnOnce(&mut StoreTx<'_>, &mut DynStamp<'_, '_>) -> Result<T, RuntimeError>,
) -> Result<T, RuntimeError> {
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

fn voiceprint_err(e: VoiceprintError) -> RuntimeError {
    RuntimeError::Voiceprint(e)
}

/// The live meeting `meeting`.
pub(crate) fn live_meeting<R: MeetingRepo + ?Sized>(
    repo: &R,
    meeting: EntityId,
) -> Result<Meeting, RuntimeError> {
    repo.meeting(meeting)?
        .filter(|m| !m.meta.deleted)
        .ok_or_else(|| EditError::MeetingNotFound(meeting).into())
}

/// The live speaker `speaker` of the live meeting `meeting`.
fn live_speaker<R: MeetingRepo + ?Sized>(
    repo: &R,
    meeting: EntityId,
    speaker: EntityId,
) -> Result<Speaker, RuntimeError> {
    live_meeting(repo, meeting)?;
    repo.speakers(meeting)?
        .into_iter()
        .find(|s| s.id == speaker && !s.meta.deleted)
        .ok_or_else(|| EditError::SpeakerNotFound(speaker).into())
}

/// The live person `person` (a merged one resolves to the person it was merged into).
fn live_person<R: VoiceprintRepo + ?Sized>(
    repo: &R,
    person: EntityId,
) -> Result<Person, RuntimeError> {
    resolve_person(repo, person)
        .map_err(voiceprint_err)?
        .ok_or_else(|| EditError::PersonNotFound(person).into())
}

/// Renames speaker `speaker` of `meeting` to `name` (trimmed). The Person link is kept: a
/// later [`rename_person`] of the linked person rewrites this row too. Returns whether the
/// row changed (renaming to the current name writes nothing).
///
/// # Errors
/// [`EditError`] (unknown or deleted meeting or speaker, unusable name); store errors.
pub fn rename_speaker(
    services: &Services,
    meeting: EntityId,
    speaker: EntityId,
    name: &str,
) -> Result<bool, RuntimeError> {
    let name = clean_name(name)?;
    write(services, |tx, stamp| {
        let mut row = live_speaker(&*tx, meeting, speaker)?;
        if row.display_name == name {
            return Ok(false);
        }
        row.display_name = name;
        row.meta.touch(stamp.hlc, stamp.clock);
        tx.put_speaker(row)?;
        Ok(true)
    })
}

/// What [`merge_speakers`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Merged {
    /// Segments (any pass) moved from the merged speaker to the kept one.
    pub segments_moved: usize,
    /// The merged speaker's stored identity entry (its centroid, biometric) was dropped, or
    /// the meeting's unreadable identity record was deleted whole.
    pub identity_dropped: bool,
    /// The dropped centroid is deleted (committed) but the checkpoint that removes it from
    /// the WAL could not finish (another connection was reading, or it failed): it leaves at
    /// the next checkpoint (any later erasure, or the store's closing checkpoint).
    pub checkpoint_pending: bool,
}

/// Merges speaker `merge` into `keep` (both live speakers of `meeting`): every live segment
/// of `merge`, in either pass, now names `keep`; `merge` becomes a tombstone; its entry in the
/// meeting's speaker identity record is dropped (a tombstoned speaker's centroid is not kept,
/// as the final pass does); `keep` is re-stamped, so a re-run treats it as the user's edit
/// (it wins when the two voices form one cluster again). Across tracks is allowed (module
/// docs); merging "Me" away is refused.
///
/// # Errors
/// [`EditError`] (unknown or deleted meeting or speakers, the same speaker twice, "Me" as
/// `merge`); store errors.
pub fn merge_speakers(
    services: &Services,
    meeting: EntityId,
    keep: EntityId,
    merge: EntityId,
) -> Result<Merged, RuntimeError> {
    if keep == merge {
        return Err(EditError::SameSpeaker(keep).into());
    }
    let mut merged = write(services, |tx, stamp| {
        let mut kept = live_speaker(&*tx, meeting, keep)?;
        let mut gone = live_speaker(&*tx, meeting, merge)?;
        if gone.role == SpeakerRole::Me {
            return Err(EditError::MergeAwayMe(merge).into());
        }
        let mut segments_moved = 0;
        for mut row in tx.segments(meeting)? {
            if row.meta.deleted || row.segment.speaker_id != Some(merge) {
                continue;
            }
            row.segment.speaker_id = Some(keep);
            row.meta.touch(stamp.hlc, stamp.clock);
            tx.put_segment(row)?;
            segments_moved += 1;
        }
        gone.meta.mark_deleted(stamp.hlc, stamp.clock);
        tx.put_speaker(gone)?;
        kept.meta.touch(stamp.hlc, stamp.clock);
        tx.put_speaker(kept)?;
        let identity_dropped = drop_identity_entry(tx, meeting, merge)?;
        Ok(Merged {
            segments_moved,
            identity_dropped,
            checkpoint_pending: false,
        })
    })?;
    if merged.identity_dropped {
        // The dropped centroid leaves the WAL too (as the final pass does); the merge is
        // committed whatever the checkpoint does, so a failure is reported, not an error.
        let flushed = services.store().flush_erasures();
        merged.checkpoint_pending = flushed.is_err();
    }
    Ok(merged)
}

/// Drops `speaker`'s entry from `meeting`'s identity record inside `tx`; returns whether
/// anything was deleted. An unreadable record is deleted whole (it may hold that entry and
/// carries nothing; as `earshot_store::identities` does when forgetting a person).
fn drop_identity_entry(
    tx: &mut StoreTx<'_>,
    meeting: EntityId,
    speaker: EntityId,
) -> Result<bool, RuntimeError> {
    let key = crate::identities::record_key(meeting);
    let mut record = match tx
        .local_value(key.as_str())?
        .map(|b| crate::identities::decode(&b))
    {
        None => return Ok(false),
        Some(StoredIdentities::Unreadable(_)) => {
            tx.delete_local_value(key.as_str())?;
            return Ok(true);
        }
        Some(StoredIdentities::Record(r)) => r,
    };
    let before = record.speakers.len();
    record.speakers.retain(|i| i.speaker != speaker);
    if record.speakers.len() == before {
        return Ok(false);
    }
    if record.speakers.is_empty() {
        tx.delete_local_value(key.as_str())?;
    } else {
        crate::local::set_tx(
            tx,
            &key,
            &crate::identities::encode(&IdentityRecord {
                speakers: record.speakers,
            }),
        )?;
    }
    Ok(true)
}

/// Applies `f` to `speaker`'s entry in `meeting`'s identity record inside `tx` and writes the
/// record back if the entry changed; returns whether it did. A missing record, entry or an
/// unreadable record changes nothing.
fn update_identity_entry(
    tx: &mut StoreTx<'_>,
    meeting: EntityId,
    speaker: EntityId,
    f: impl FnOnce(&mut crate::identities::SpeakerIdentity),
) -> Result<bool, RuntimeError> {
    let key = crate::identities::record_key(meeting);
    let Some(StoredIdentities::Record(mut record)) = tx
        .local_value(key.as_str())?
        .map(|b| crate::identities::decode(&b))
    else {
        return Ok(false);
    };
    let Some(entry) = record.speakers.iter_mut().find(|i| i.speaker == speaker) else {
        return Ok(false);
    };
    let before = entry.clone();
    f(entry);
    if *entry == before {
        return Ok(false);
    }
    crate::local::set_tx(tx, &key, &crate::identities::encode(&record))?;
    Ok(true)
}

/// Who [`link_speaker`] links a speaker to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PersonChoice {
    /// An existing person (a merged one resolves to the person it was merged into).
    Existing(EntityId),
    /// A new person with this name (trimmed).
    New {
        /// The name.
        name: String,
    },
}

/// Links speaker `speaker` of `meeting` to a person: `person_id` and the display name
/// become the person's. Returns the person's id. Linking a speaker to the person it is
/// already linked to under that name writes nothing. The user's person (`is_me`) is the
/// "Me" row's alone, and the "Me" row links only to it, as the final pass's naming rule
/// ("nobody else is the user"). Linking to another person clears the speaker's
/// "remembered" mark ([`remember_voices`]), so its voice can be remembered for the new one,
/// and ends an ignore ([`ignore_speaker`]).
///
/// # Errors
/// [`EditError`] (unknown or deleted meeting, speaker or person, unusable name,
/// [`EditError::NotTheUser`], [`EditError::MeIsTheUser`]); store errors.
pub fn link_speaker(
    services: &Services,
    meeting: EntityId,
    speaker: EntityId,
    choice: &PersonChoice,
) -> Result<EntityId, RuntimeError> {
    let new_name = checked_choice(choice)?;
    write(services, |tx, stamp| {
        link_in(tx, stamp, meeting, speaker, choice, new_name)
    })
}

/// The checked name of a new person in `choice` (`None` for an existing person).
fn checked_choice(choice: &PersonChoice) -> Result<Option<String>, RuntimeError> {
    Ok(match choice {
        PersonChoice::New { name } => Some(clean_name(name)?),
        PersonChoice::Existing(_) => None,
    })
}

/// [`link_speaker`] inside `tx` (`new_name`: [`checked_choice`] of `choice`). Linking a voice
/// to a person also ends an ignore ([`ignore_speaker`]): a named voice is not ignored.
fn link_in(
    tx: &mut StoreTx<'_>,
    stamp: &mut DynStamp<'_, '_>,
    meeting: EntityId,
    speaker: EntityId,
    choice: &PersonChoice,
    new_name: Option<String>,
) -> Result<EntityId, RuntimeError> {
    let mut row = live_speaker(&*tx, meeting, speaker)?;
    let person = match (choice, new_name) {
        (PersonChoice::Existing(id), _) => live_person(&*tx, *id)?,
        (PersonChoice::New { .. }, Some(name)) => {
            let p = Person::new(name, false, stamp);
            tx.put_person(p.clone()).map_err(voiceprint_err)?;
            p
        }
        (PersonChoice::New { .. }, None) => {
            return Err(RuntimeError::Internal(
                "a new person's name was not checked",
            ));
        }
    };
    match (row.role == SpeakerRole::Me, person.is_me) {
        (false, true) => return Err(EditError::NotTheUser(speaker).into()),
        (true, false) => return Err(EditError::MeIsTheUser(speaker).into()),
        _ => {}
    }
    if row.person_id == Some(person.id) && row.display_name == person.display_name {
        return Ok(person.id);
    }
    if row.person_id != Some(person.id) {
        update_identity_entry(tx, meeting, speaker, |i| {
            i.enrolled = false;
            i.ignored = false;
        })?;
    }
    row.person_id = Some(person.id);
    row.display_name.clone_from(&person.display_name);
    row.meta.touch(stamp.hlc, stamp.clock);
    tx.put_speaker(row)?;
    Ok(person.id)
}

/// Whether a meeting with these Track rows (its live ones count) was recorded in the room,
/// by the final pass's own rule ([`crate::post::in_room_kinds`]).
#[must_use]
pub fn records_in_room(tracks: &[Track]) -> bool {
    let kinds: BTreeSet<TrackId> = tracks
        .iter()
        .filter(|t| !t.meta.deleted)
        .map(|t| t.kind)
        .collect();
    crate::post::in_room_kinds(&kinds)
}

/// Whether [`mark_me`] would accept `row` of a meeting with these `tracks` and live
/// `speakers`: what the Speakers panel offers, by the refusals' own rule.
#[must_use]
pub fn can_mark_me(tracks: &[Track], speakers: &[Speaker], row: &Speaker) -> bool {
    records_in_room(tracks)
        && row.role == SpeakerRole::Room
        && !speakers
            .iter()
            .any(|s| s.role == SpeakerRole::Me && !s.meta.deleted)
}

/// Whether [`unmark_me`] would accept `row` of a meeting with these `tracks`.
#[must_use]
pub fn can_unmark_me(tracks: &[Track], row: &Speaker) -> bool {
    records_in_room(tracks) && row.role == SpeakerRole::Me
}

/// "This is me" (T9.12): the user says that voice `speaker` of the in-room meeting `meeting`
/// is theirs. The row becomes the meeting's "Me" row: role Me, linked to the user's person
/// ([`me_person`], created as the final pass does) under the name that person displays.
/// Returns the person's id.
///
/// There is one "Me" row per meeting, so a meeting that has one refuses
/// ([`EditError::MeetingHasMe`]); the way to move it is [`unmark_me`] first.
///
/// This creates no biometric data: no voiceprint is enrolled or added to, and
/// [`remember_voices`] keeps refusing the row. The voice's existing entry in the identity
/// record (the centroid the final pass already stored) stays, marked as the user's edit, so a
/// re-run can match the voice again (`crate::post`, step 4): the one kind of "Me" row with an
/// entry. Like any edit the mark goes with the voice when a re-run does not match it. Written
/// summaries are not touched.
///
/// # Errors
/// [`EditError`] (unknown or deleted meeting or speaker, [`EditError::NotInRoom`],
/// [`EditError::MeetingHasMe`]); store errors.
pub fn mark_me(
    services: &Services,
    meeting: EntityId,
    speaker: EntityId,
) -> Result<EntityId, RuntimeError> {
    write(services, |tx, stamp| {
        let mut row = live_speaker(&*tx, meeting, speaker)?;
        if !records_in_room(&tx.tracks(meeting)?) {
            return Err(EditError::NotInRoom(meeting).into());
        }
        if let Some(me) = tx
            .speakers(meeting)?
            .into_iter()
            .find(|s| s.role == SpeakerRole::Me && !s.meta.deleted)
        {
            return Err(EditError::MeetingHasMe(me.id).into());
        }
        if row.role != SpeakerRole::Room {
            return Err(EditError::NotInRoom(meeting).into());
        }
        let me = me_person(tx, stamp).map_err(voiceprint_err)?;
        row.role = SpeakerRole::Me;
        row.person_id = Some(me.id);
        row.display_name.clone_from(&me.display_name);
        row.meta.touch(stamp.hlc, stamp.clock);
        tx.put_speaker(row)?;
        update_identity_entry(tx, meeting, speaker, |i| {
            i.user_edited = true;
            i.enrolled = false;
            i.ignored = false;
        })?;
        Ok(me.id)
    })
}

/// "This isn't me" (T9.12), the way out of a mistake: the "Me" row `speaker` of the in-room
/// meeting `meeting` goes back to an unnamed voice of the room, the smallest free "Speaker N"
/// that needs a name, linked to nobody. Its identity entry reads as the pass's own naming
/// again, so a re-run names the voice as any other. The user's person stays. Also takes a "Me"
/// row a voiceprint named in this meeting; a re-run then names it again by that voiceprint.
///
/// # Errors
/// [`EditError`] (unknown or deleted meeting or speaker, [`EditError::NotInRoom`],
/// [`EditError::NotMe`]); store errors.
pub fn unmark_me(
    services: &Services,
    meeting: EntityId,
    speaker: EntityId,
) -> Result<(), RuntimeError> {
    write(services, |tx, stamp| {
        let mut row = live_speaker(&*tx, meeting, speaker)?;
        if !records_in_room(&tx.tracks(meeting)?) {
            return Err(EditError::NotInRoom(meeting).into());
        }
        if row.role != SpeakerRole::Me {
            return Err(EditError::NotMe(speaker).into());
        }
        let taken: Vec<String> = tx
            .speakers(meeting)?
            .into_iter()
            .filter(|s| !s.meta.deleted && s.id != speaker)
            .map(|s| s.display_name)
            .collect();
        row.role = SpeakerRole::Room;
        row.person_id = None;
        row.display_name = first_free_speaker_name(&taken);
        row.meta.touch(stamp.hlc, stamp.clock);
        tx.put_speaker(row.clone())?;
        update_identity_entry(tx, meeting, speaker, |i| {
            i.given_name.clone_from(&row.display_name);
            i.given_person = None;
            i.given_stamp = Some(row.meta.updated_at);
            i.user_edited = false;
            i.enrolled = false;
            i.ignored = false;
        })?;
        Ok(())
    })
}

/// Renames person `person` to `name` (trimmed) and rewrites the display name of every live
/// speaker row, in every live meeting, linked to that identity (the person and everyone
/// merged into it), all in one transaction. Returns how many speaker rows were rewritten.
///
/// # Errors
/// [`EditError`] (unknown, deleted or merged-away person, unusable name); store errors.
pub fn rename_person(
    services: &Services,
    person: EntityId,
    name: &str,
) -> Result<usize, RuntimeError> {
    let name = clean_name(name)?;
    write(services, |tx, stamp| {
        let mut p = tx
            .person(person)
            .map_err(voiceprint_err)?
            .filter(Person::is_live)
            .ok_or(EditError::PersonNotFound(person))?;
        if p.display_name != name {
            p.display_name.clone_from(&name);
            p.meta.touch(stamp.hlc, stamp.clock);
            tx.put_person(p).map_err(voiceprint_err)?;
        }
        let identity = identity_of(&*tx, person).map_err(voiceprint_err)?;
        let mut rewritten = 0;
        for m in tx.meetings()? {
            if m.meta.deleted {
                continue;
            }
            for mut s in tx.speakers(m.id)? {
                if s.meta.deleted
                    || !s.person_id.is_some_and(|id| identity.contains(&id))
                    || s.display_name == name
                {
                    continue;
                }
                s.display_name.clone_from(&name);
                s.meta.touch(stamp.hlc, stamp.clock);
                tx.put_speaker(s)?;
                rewritten += 1;
            }
        }
        Ok(rewritten)
    })
}

/// A person as the Speakers panel lists it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PersonInfo {
    /// The person.
    pub id: EntityId,
    /// Their name.
    pub name: String,
    /// A live voiceprint of any model exists for them (or for someone merged into them,
    /// which matches as them).
    pub has_voiceprint: bool,
    /// The user of this installation.
    pub is_me: bool,
}

/// Every live person (deleted and merged-away persons are left out; their voiceprints count
/// for the person they were merged into), by name (ASCII case ignored), then id.
///
/// # Errors
/// Store errors.
pub fn list_persons(services: &Services) -> Result<Vec<PersonInfo>, RuntimeError> {
    list_persons_in(&services.store())
}

/// [`list_persons`] from a store the caller has locked (one snapshot with other reads).
///
/// # Errors
/// Store errors.
pub fn list_persons_in(repo: &earshot_store::Store) -> Result<Vec<PersonInfo>, RuntimeError> {
    let mut out = Vec::new();
    for p in repo.persons().map_err(voiceprint_err)? {
        if !p.is_live() {
            continue;
        }
        let mut has_voiceprint = false;
        for id in identity_of(repo, p.id).map_err(voiceprint_err)? {
            if repo
                .voiceprints_of(id)
                .map_err(voiceprint_err)?
                .iter()
                .any(|v| !v.meta.deleted)
            {
                has_voiceprint = true;
                break;
            }
        }
        out.push(PersonInfo {
            id: p.id,
            name: p.display_name,
            has_voiceprint,
            is_me: p.is_me,
        });
    }
    out.sort_by(|a, b| {
        a.name
            .to_ascii_lowercase()
            .cmp(&b.name.to_ascii_lowercase())
            .then(a.id.cmp(&b.id))
    });
    Ok(out)
}

/// What [`remember_voices`] did for one speaker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Remembered {
    /// The speaker's centroid was added to the person's voiceprint (created if needed).
    Enrolled {
        /// The person.
        person: EntityId,
        /// The voiceprint.
        voiceprint: EntityId,
    },
    /// A new voiceprint needs more speech than the speaker's final segments hold.
    NotEnoughSpeech {
        /// Required.
        needed: Duration,
        /// The speaker's final segments.
        got: Duration,
    },
    /// No stored voice for this speaker: the final pass has not run, did not record a
    /// centroid for it, or its identity record cannot be read.
    NoStoredVoice,
    /// This centroid was already added (an earlier call); it is not added twice.
    AlreadyEnrolled,
    /// The centroid's embedding model has no calibrated voiceprint threshold, so a
    /// voiceprint of it could never match: nothing is stored.
    Uncalibrated {
        /// The model.
        model: EmbeddingModelId,
    },
}

/// Opt-in enrolment (in the app since T12.19 only through [`save_voice`], one voice the user
/// saved): for each of `speakers` of `meeting`, adds the speaker's centroid from the final
/// pass's identity record to the voiceprint of the person the speaker is linked to ([`enroll`], exact embedding model of
/// the centroid; the enrolment settings and voiceprint thresholds of
/// [`PostConfig::default`]). The speech behind a centroid is the total length of the
/// speaker's live final segments. A centroid is added once: the entry is marked enrolled
/// (`SpeakerIdentity::enrolled`) in the same transaction, and a repeat is
/// [`Remembered::AlreadyEnrolled`]; a new final pass (a new centroid) or a link to another
/// person clears the mark. Returns one result per distinct speaker, in order.
///
/// # Errors
/// [`EditError`] for a speaker that is unknown or deleted, not linked to a person (or linked
/// to a deleted one), or the user ([`EditError::MeVoice`]); nothing is enrolled then. Store
/// and voiceprint errors.
pub fn remember_voices(
    services: &Services,
    meeting: EntityId,
    speakers: &[EntityId],
) -> Result<Vec<(EntityId, Remembered)>, RuntimeError> {
    remember_voices_with(services, meeting, speakers, &PostConfig::default())
}

/// [`remember_voices`] with other enrolment settings and thresholds (`config.enrolment`,
/// `config.voiceprint_thresholds`; tests).
///
/// # Errors
/// As [`remember_voices`].
pub fn remember_voices_with(
    services: &Services,
    meeting: EntityId,
    speakers: &[EntityId],
    config: &PostConfig,
) -> Result<Vec<(EntityId, Remembered)>, RuntimeError> {
    let mut seen = BTreeSet::new();
    let speakers: Vec<EntityId> = speakers
        .iter()
        .copied()
        .filter(|s| seen.insert(*s))
        .collect();
    write(services, |tx, stamp| {
        remember_in(tx, stamp, meeting, &speakers, config)
    })
}

/// [`remember_voices_with`] inside `tx`, for distinct `speakers`.
fn remember_in(
    tx: &mut StoreTx<'_>,
    stamp: &mut DynStamp<'_, '_>,
    meeting: EntityId,
    speakers: &[EntityId],
    config: &PostConfig,
) -> Result<Vec<(EntityId, Remembered)>, RuntimeError> {
    live_meeting(&*tx, meeting)?;
    // Every speaker is checked before anything is enrolled.
    let mut targets = Vec::new();
    for &id in speakers {
        let row = live_speaker(&*tx, meeting, id)?;
        if row.role == SpeakerRole::Me {
            return Err(EditError::MeVoice(id).into());
        }
        let linked = row.person_id.ok_or(EditError::NoPerson(id))?;
        let person = live_person(&*tx, linked)?;
        if person.is_me {
            return Err(EditError::MeVoice(id).into());
        }
        targets.push((id, person.id));
    }
    let key = crate::identities::record_key(meeting);
    let record = match tx
        .local_value(key.as_str())?
        .map(|b| crate::identities::decode(&b))
    {
        Some(StoredIdentities::Record(r)) => r.speakers,
        None | Some(StoredIdentities::Unreadable(_)) => Vec::new(),
    };
    let finals = live_segments(&*tx, meeting, TranscriptPass::Final)?;
    let mut out = Vec::new();
    for (id, person) in targets {
        let Some(entry) = record
            .iter()
            .find(|i| i.speaker == id && i.centroid.is_valid())
        else {
            out.push((id, Remembered::NoStoredVoice));
            continue;
        };
        if entry.enrolled {
            out.push((id, Remembered::AlreadyEnrolled));
            continue;
        }
        match config.voiceprint_threshold(&entry.centroid.model) {
            Ok(_) => {}
            Err(RuntimeError::Uncalibrated { model, .. }) => {
                out.push((id, Remembered::Uncalibrated { model }));
                continue;
            }
            Err(e) => return Err(e),
        }
        let speech: Duration = finals
            .iter()
            .filter(|r| r.segment.speaker_id == Some(id))
            .map(|r| r.segment.end.saturating_sub(r.segment.start))
            .sum();
        let result = enroll(
            tx,
            person,
            std::slice::from_ref(&entry.centroid),
            speech,
            &config.enrolment,
            stamp,
        );
        out.push((
            id,
            match result {
                Ok(voiceprint) => {
                    update_identity_entry(tx, meeting, id, |i| i.enrolled = true)?;
                    Remembered::Enrolled { person, voiceprint }
                }
                Err(VoiceprintError::NotEnoughSpeech { needed, got }) => {
                    Remembered::NotEnoughSpeech { needed, got }
                }
                Err(e) => return Err(voiceprint_err(e)),
            },
        ));
    }
    Ok(out)
}

/// What [`save_voice`] did: the person the voice is now linked to and what remembering
/// its voice did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SavedVoice {
    /// The person.
    pub person: EntityId,
    /// Remembering the voice ([`Remembered::Enrolled`] or why nothing was stored).
    pub remembered: Remembered,
}

/// "Save" on a voice that needs a name (T12.19): links speaker `speaker` of `meeting` to a
/// person ([`link_speaker`]) and remembers its voice for that person ([`remember_voices`]),
/// in one transaction. The user's choice to save this one voice is the consent the
/// enrolment needs (it replaced the panel-wide "Remember these voices" tickbox). The link
/// is kept whatever remembering does: too little speech, no stored voice or an
/// uncalibrated speaker model store no voiceprint and say so in
/// [`SavedVoice::remembered`]; a voice already remembered for that person is
/// [`Remembered::AlreadyEnrolled`].
///
/// # Errors
/// As [`link_speaker`] and [`remember_voices`] ("Me" and the user's person are refused, so
/// the user's voice is never enrolled here); nothing is written then.
pub fn save_voice(
    services: &Services,
    meeting: EntityId,
    speaker: EntityId,
    choice: &PersonChoice,
) -> Result<SavedVoice, RuntimeError> {
    save_voice_with(services, meeting, speaker, choice, &PostConfig::default())
}

/// [`save_voice`] with other enrolment settings and thresholds (tests).
///
/// # Errors
/// As [`save_voice`].
pub fn save_voice_with(
    services: &Services,
    meeting: EntityId,
    speaker: EntityId,
    choice: &PersonChoice,
    config: &PostConfig,
) -> Result<SavedVoice, RuntimeError> {
    let new_name = checked_choice(choice)?;
    write(services, |tx, stamp| {
        let person = link_in(tx, stamp, meeting, speaker, choice, new_name)?;
        let remembered = remember_in(tx, stamp, meeting, &[speaker], config)?
            .into_iter()
            .next()
            .map(|(_, r)| r)
            .ok_or(RuntimeError::Internal(
                "remembering one voice gave no result",
            ))?;
        Ok(SavedVoice { person, remembered })
    })
}

/// The entries of `meeting`'s speaker identity record (empty when there is none or it
/// cannot be read), from a store the caller has locked.
///
/// # Errors
/// Store errors.
pub fn identities_in(
    store: &earshot_store::Store,
    meeting: EntityId,
) -> Result<Vec<crate::identities::SpeakerIdentity>, RuntimeError> {
    Ok(
        match store
            .local_value(crate::identities::record_key(meeting).as_str())?
            .map(|b| crate::identities::decode(&b))
        {
            Some(StoredIdentities::Record(r)) => r.speakers,
            None | Some(StoredIdentities::Unreadable(_)) => Vec::new(),
        },
    )
}

/// Whether the user ignored `row` in its meeting ([`ignore_speaker`]), by its entry among
/// `identities` ([`identities_in`]). Only a voice that still needs a name counts: "Me" and a
/// linked voice never do.
#[must_use]
pub fn is_ignored(row: &Speaker, identities: &[crate::identities::SpeakerIdentity]) -> bool {
    row.role != SpeakerRole::Me
        && row.person_id.is_none()
        && identities.iter().any(|i| i.speaker == row.id && i.ignored)
}

/// Whether [`ignore_speaker`] would accept `row` with these `identities`: what the Speakers
/// panel offers, by the refusals' own rule.
#[must_use]
pub fn can_ignore(row: &Speaker, identities: &[crate::identities::SpeakerIdentity]) -> bool {
    row.role != SpeakerRole::Me
        && row.person_id.is_none()
        && identities.iter().any(|i| i.speaker == row.id)
}

/// "Ignore" (T12.19): the user does not want to name voice `speaker` of `meeting`. It no
/// longer needs a name, keeps its "Speaker N" label everywhere (the row is not written), and
/// is never enrolled or named as a person: a later final pass continues it as a user edit
/// (`crate::post` step 4), keeping it unlinked whatever voiceprint it matches. Kept in the
/// voice's entry of the meeting's speaker identity record
/// (`earshot_store::identities::SpeakerIdentity::ignored`), device-locally. Returns whether
/// anything changed (ignoring an ignored voice writes nothing). Undone by
/// [`unignore_speaker`]; naming the voice ([`link_speaker`]) also ends it.
///
/// # Errors
/// [`EditError`] (unknown or deleted meeting or speaker, [`EditError::NotUnnamed`] for "Me"
/// or a linked voice, [`EditError::NoStoredVoice`] for a voice without an entry); store
/// errors.
pub fn ignore_speaker(
    services: &Services,
    meeting: EntityId,
    speaker: EntityId,
) -> Result<bool, RuntimeError> {
    write(services, |tx, _| {
        let row = live_speaker(&*tx, meeting, speaker)?;
        if row.role == SpeakerRole::Me || row.person_id.is_some() {
            return Err(EditError::NotUnnamed(speaker).into());
        }
        let key = crate::identities::record_key(meeting);
        let has_entry = matches!(
            tx.local_value(key.as_str())?.map(|b| crate::identities::decode(&b)),
            Some(StoredIdentities::Record(r)) if r.speakers.iter().any(|i| i.speaker == speaker)
        );
        if !has_entry {
            return Err(EditError::NoStoredVoice(speaker).into());
        }
        update_identity_entry(tx, meeting, speaker, |i| i.ignored = true)
    })
}

/// Undoes [`ignore_speaker`]: voice `speaker` of `meeting` needs a name again, and its entry
/// reads as the pass's own naming again (unless the user also edited the row). Returns
/// whether anything changed (a voice that is not ignored writes nothing).
///
/// # Errors
/// [`EditError`] (unknown or deleted meeting or speaker); store errors.
pub fn unignore_speaker(
    services: &Services,
    meeting: EntityId,
    speaker: EntityId,
) -> Result<bool, RuntimeError> {
    write(services, |tx, _| {
        let row = live_speaker(&*tx, meeting, speaker)?;
        update_identity_entry(tx, meeting, speaker, |i| {
            if !i.ignored {
                return;
            }
            i.ignored = false;
            // A re-run carried the ignore as an edit (no stamp, `user_edited`). When the row
            // is still what the pass gave it, nothing else of it was the user's: it reads as
            // the pass's own naming again (as `unmark_me` leaves it), so a later run may
            // renumber or name it. A row the user renamed or linked stays an edit.
            if i.user_edited
                && i.given_stamp.is_none()
                && row.display_name == i.given_name
                && row.person_id == i.given_person
            {
                i.user_edited = false;
                i.given_stamp = Some(row.meta.updated_at);
            }
        })
    })
}

/// Forgets every voiceprint of person `person` and of everyone merged into them, and their
/// speaker centroids in every meeting's identity record: `Store::forget_voiceprints` (zeroed,
/// tombstoned, then a truncating checkpoint). Deleted and merged-away persons are accepted
/// (forgetting biometric data always works). Returns how many voiceprints were forgotten.
///
/// # Errors
/// [`EditError::PersonNotFound`] for an id that was never a person; `StoreError::Busy`
/// (retryable, [`RuntimeError::is_retryable`]) when another connection blocked the
/// checkpoint (the deletion is committed); store errors.
pub fn forget_person_voices(services: &Services, person: EntityId) -> Result<usize, RuntimeError> {
    let clock = Arc::clone(&services.clock);
    let mut db = services.store();
    if db.person(person).map_err(voiceprint_err)?.is_none() {
        return Err(EditError::PersonNotFound(person).into());
    }
    let (store, hlc) = db.parts();
    Ok(store.forget_voiceprints(person, hlc, &*clock)?)
}

/// Removes a person from this computer (the Speakers page, T9.1u): their name, voiceprints and
/// speaker centroids, and anyone merged into them; their speakers go back to "Speaker N"
/// ([`earshot_store::Store::remove_person`]). Text already written is not rewritten.
///
/// # Errors
/// [`EditError::PersonNotFound`] for an unknown, deleted or merged-away person;
/// [`EditError::PersonIsTheUser`] for the user, or someone merged into them; store errors.
pub fn remove_person(
    services: &Services,
    person: EntityId,
) -> Result<earshot_store::PersonRemoval, RuntimeError> {
    let clock = Arc::clone(&services.clock);
    let mut db = services.store();
    let row = db
        .person(person)
        .map_err(voiceprint_err)?
        .filter(Person::is_live)
        .ok_or(EditError::PersonNotFound(person))?;
    if row.is_me {
        return Err(EditError::PersonIsTheUser(person).into());
    }
    let store: &earshot_store::Store = &db;
    for id in identity_of(store, person).map_err(voiceprint_err)? {
        if store
            .person(id)
            .map_err(voiceprint_err)?
            .is_some_and(|p| p.is_me)
        {
            return Err(EditError::PersonIsTheUser(person).into());
        }
    }
    let (store, hlc) = db.parts();
    Ok(store.remove_person(person, hlc, &*clock)?)
}

/// The previous summary of a meeting ([`summary_history`]).
#[derive(Debug, Clone, PartialEq)]
pub struct PreviousSummary {
    /// The summary (a tombstone until restored).
    pub summary: Summary,
    /// Whether every segment it cites is in the meeting's current transcript; only then can
    /// [`undo_summary`] restore it.
    pub restorable: bool,
}

/// A meeting's current summary and the one before it.
#[derive(Debug, Clone, PartialEq)]
pub struct SummaryHistory {
    /// What [`MeetingView`] shows: the most recently updated live summary.
    pub current: Option<Summary>,
    /// The summary the current one replaced (module docs).
    pub previous: Option<PreviousSummary>,
}

/// [`summary_history`] from a repository the caller has locked (one snapshot with other
/// reads).
///
/// # Errors
/// [`EditError::MeetingNotFound`]; store errors.
pub fn summary_history_in<R: MeetingRepo + ?Sized>(
    repo: &R,
    meeting: EntityId,
) -> Result<SummaryHistory, RuntimeError> {
    history(repo, meeting)
}

fn history<R: MeetingRepo + ?Sized>(
    repo: &R,
    meeting: EntityId,
) -> Result<SummaryHistory, RuntimeError> {
    let view = MeetingView::load(repo, meeting)?.ok_or(EditError::MeetingNotFound(meeting))?;
    let previous = repo
        .summaries(meeting)?
        .into_iter()
        .filter(|s| s.meta.deleted)
        .max_by_key(|s| s.meta.updated_at)
        .map(|summary| {
            let ids: BTreeSet<EntityId> = view.segments.iter().map(|s| s.id).collect();
            PreviousSummary {
                restorable: cites_only(&summary.summary, &ids),
                summary,
            }
        });
    Ok(SummaryHistory {
        current: view.summary,
        previous,
    })
}

/// The current summary of `meeting` and the previous one, if any.
///
/// # Errors
/// [`EditError::MeetingNotFound`]; store errors.
pub fn summary_history(
    services: &Services,
    meeting: EntityId,
) -> Result<SummaryHistory, RuntimeError> {
    history(&**services.store(), meeting)
}

/// Swaps `meeting`'s current summary with the previous one (module docs): the current one
/// becomes a tombstone and the previous one is live again with a fresh stamp, so it is what
/// [`MeetingView`] and export read. A second call swaps them back. Returns the restored
/// summary's id.
///
/// # Errors
/// [`EditError`] (unknown or deleted meeting, no previous summary, a previous summary citing
/// a replaced transcript); store errors.
pub fn undo_summary(services: &Services, meeting: EntityId) -> Result<EntityId, RuntimeError> {
    write(services, |tx, stamp| {
        let h = history(&*tx, meeting)?;
        let previous = h.previous.ok_or(EditError::NoPreviousSummary)?;
        if !previous.restorable {
            return Err(EditError::PreviousSummaryStale.into());
        }
        // Tombstone first, so the summary restored now is newer than the one it replaces and
        // the next undo finds that one as the previous summary.
        for mut s in tx.summaries(meeting)? {
            if !s.meta.deleted {
                s.meta.mark_deleted(stamp.hlc, stamp.clock);
                tx.put_summary(s)?;
            }
        }
        let mut restored = previous.summary;
        restored.meta.deleted = false;
        restored.meta.touch(stamp.hlc, stamp.clock);
        let id = restored.id;
        tx.put_summary(restored)?;
        Ok(id)
    })
}

/// How [`retranscribe`] runs.
pub struct Retranscribe<'a> {
    /// Where the models are.
    pub models: &'a ModelDir,
    /// The transcription model.
    pub model: &'a str,
    /// Accelerators to try, best first.
    pub order: &'a [Accelerator],
    /// Cancels the pass.
    pub cancel: &'a CancelToken,
}

/// What [`retranscribe`] did.
#[derive(Debug)]
pub struct Retranscribed {
    /// Tracks whose recording is gone (audio retention or by hand); not processed.
    pub missing_tracks: Vec<TrackId>,
    /// The final pass ([`HostPost::NothingToProcess`] when every recording is gone,
    /// [`HostPost::Cancelled`] when cancelled).
    pub post: HostPost,
    /// The meeting's summary no longer matches the transcript
    /// ([`MeetingView::summary_out_of_date`]: it cites a segment the transcript no longer
    /// has, the check of [`PreviousSummary::restorable`], or a line was edited after it was
    /// written): offer to summarise again.
    pub summary_stale: bool,
}

/// The final pass of `meeting` again from its stored audio: every live track of the
/// meeting, [`TranscribeOptions::batch`], no live reuse (`PostConfig::reuse_live` off),
/// production models ([`post_processor`]). Speaker names and Person links the user gave are
/// carried over by the final pass's identity matching. The existing summary is left as it
/// is; [`Retranscribed::summary_stale`] says when it no longer matches (the host offers to
/// summarise again). Lines the user edited are kept (module docs).
///
/// # Errors
/// [`EditError::MeetingNotFound`]; [`RuntimeError::MeetingBusy`] (retryable) while the
/// meeting is recorded, processed or deleted; model and final-pass errors.
pub fn retranscribe(
    services: &Services,
    meeting: EntityId,
    run: &Retranscribe<'_>,
    progress: &mut dyn FnMut(HostStep<'_>),
) -> Result<Retranscribed, RuntimeError> {
    let load = || {
        let setup = post_processor(
            &EvidenceLoader::default(),
            run.models,
            run.model,
            run.order,
            ONNX_ACCELERATOR,
            PostConfig {
                reuse_live: false,
                ..PostConfig::default()
            },
        )?;
        Ok(PostLoad {
            processor: setup.processor,
            asr_backend: setup.asr_backend,
            fallbacks: setup.fallbacks,
        })
    };
    retranscribe_with(services, meeting, run.cancel, load, progress)
}

/// [`retranscribe`] with the final-pass processor from `load` (called only when some track
/// still has its recording). The processor should not reuse live work: the meeting's live
/// records are gone after its first final pass anyway.
///
/// # Errors
/// As [`retranscribe`].
pub fn retranscribe_with<T, S, E>(
    services: &Services,
    meeting: EntityId,
    cancel: &CancelToken,
    load: impl FnOnce() -> Result<PostLoad<T, S, E>, RuntimeError>,
    progress: &mut dyn FnMut(HostStep<'_>),
) -> Result<Retranscribed, RuntimeError>
where
    T: Transcriber + Send,
    S: SpeakerSegmenter + Send,
    E: SpeakerEmbedder + Send,
{
    let _lease = services.lease_meeting_alone(meeting, MeetingJob::Processing)?;
    let request = {
        let db = services.store();
        let m = live_meeting(&**db, meeting)?;
        let mut tracks: Vec<FinalPassTrack> = db
            .tracks(meeting)?
            .into_iter()
            .filter(|t| !t.meta.deleted)
            .map(|t| FinalPassTrack {
                track: t.kind,
                track_entity: Some(t.id),
                options: TranscribeOptions::batch(),
            })
            .collect();
        tracks.sort_by_key(|t| t.track);
        FinalPassRequest {
            end: MeetingTime::from_millis(m.duration_ms.unwrap_or(0)),
            tracks,
        }
    };
    let mut missing_tracks = Vec::new();
    let post = final_pass(
        services,
        meeting,
        &request,
        cancel,
        load,
        &mut missing_tracks,
        progress,
    )?;
    let summary_stale = {
        let db = services.store();
        MeetingView::load(&**db, meeting)?.is_some_and(|v| v.summary_out_of_date())
    };
    Ok(Retranscribed {
        missing_tracks,
        post,
        summary_stale,
    })
}

/// A stretch of one speaker's audio ([`speaker_sample`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpeakerSample {
    /// Sample rate of `samples`.
    pub sample_rate_hz: u32,
    /// Mono 16-bit samples, as the recording stores them.
    pub samples: Vec<i16>,
    /// The segment the sample comes from.
    pub segment_id: EntityId,
    /// Where the sample starts in the meeting.
    pub start: MeetingTime,
    /// Where it ends (at most `max_ms` after `start`).
    pub end: MeetingTime,
}

/// The first `max_ms` of speaker `speaker`'s longest segment in the transcript
/// [`MeetingView`] shows (final, else live), read from its track's recording. `None` (not an
/// error) when the speaker has no segment with audio or the recording is gone. The meeting
/// is leased while the file is read (a deletion waits); the store lock is released before
/// decoding.
///
/// # Errors
/// [`EditError`] (unknown or deleted meeting or speaker); [`RuntimeError::MeetingBusy`]
/// while the meeting is being deleted; a recording that cannot be decoded; store errors.
pub fn speaker_sample(
    services: &Services,
    meeting: EntityId,
    speaker: EntityId,
    max_ms: u64,
) -> Result<Option<SpeakerSample>, RuntimeError> {
    let _lease = services.lease_meeting(meeting)?;
    let found = {
        let db = services.store();
        live_speaker(&**db, meeting, speaker)?;
        let view = MeetingView::load(&**db, meeting)?.ok_or(EditError::MeetingNotFound(meeting))?;
        let Some(segment) = view
            .segments
            .iter()
            .filter(|s| s.speaker_id == Some(speaker) && s.end > s.start)
            .max_by_key(|s| (s.end.saturating_sub(s.start), std::cmp::Reverse(s.start)))
            .cloned()
        else {
            return Ok(None);
        };
        db.audio_files(meeting)?
            .into_iter()
            .find(|f| f.track_id == segment.track)
            .map(|f| (segment, f))
    };
    let Some((segment, file)) = found else {
        return Ok(None);
    };
    let path = services.data.resolve(&file.path)?;
    if !path.exists() {
        return Ok(None);
    }
    let (rate, audio) = earshot_store::read_flac(&path)?;
    if rate != file.sample_rate_hz || rate == 0 {
        return Err(RuntimeError::Internal(
            "a recording's sample rate differs from its AudioFile row",
        ));
    }
    let start_ms = segment.start.as_millis();
    let end_ms = segment.end.as_millis().min(start_ms.saturating_add(max_ms));
    let frame = |ms: u64| {
        usize::try_from(ms.saturating_mul(u64::from(rate)) / 1000)
            .unwrap_or(usize::MAX)
            .min(audio.len())
    };
    let (a, b) = (frame(start_ms), frame(end_ms));
    if a >= b {
        return Ok(None);
    }
    Ok(Some(SpeakerSample {
        sample_rate_hz: rate,
        samples: audio[a..b].iter().map(|&x| f32_to_i16(x)).collect(),
        segment_id: segment.id,
        start: MeetingTime::from_millis(start_ms),
        end: MeetingTime::from_millis(end_ms),
    }))
}

#[cfg(test)]
#[path = "edit_tests.rs"]
mod tests;
