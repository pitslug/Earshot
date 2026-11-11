//! The meeting detail screen (T7.3): what one stored meeting shows ([`meeting_detail`]) and
//! the speaker edits, sample, export and deletion behind its commands.
//!
//! Everything is read from `earshot_runtime::view::MeetingView` (final transcript, else the
//! live draft) and changed only through `earshot_runtime::edit` and the runtime's export and
//! deletion. The store lock is held for each read only; nothing here holds it across FLAC
//! decoding or vault work (the runtime functions take it themselves).

use std::collections::BTreeMap;

use earshot_core::asr::Segment;
use earshot_core::audio::{SampleRate, WavFormat, i16_to_f32, write_wav};
use earshot_core::capture::TrackId;
use earshot_core::diar::SpeakerRole;
use earshot_core::identity::EntityId;
use earshot_core::model::{AudioFileRepo, Speaker, TranscriptPass};
use earshot_core::summary::SummaryV1;
use earshot_runtime::backends::pass_backend;
use earshot_runtime::edit::{
    self, EditError, NameProblem, PersonChoice, PersonInfo, Remembered, SummaryField, TextProblem,
};
use earshot_runtime::identities::SpeakerIdentity;
use earshot_runtime::view::MeetingView;
use earshot_runtime::vocabulary;
use earshot_runtime::word_choices;
use earshot_runtime::{RuntimeError, Services, os_seeded_rng};
use serde::{Deserialize, Serialize};
use specta::Type;

use crate::ipc::{IpcError, MeetingState};
use crate::meeting_events::{CaptureMode, backend_label, needs_name};

/// How much of a speaker's longest segment the play button plays (ms).
pub const SAMPLE_MAX_MS: u64 = 8_000;

/// The longest sample line of a speaker card, in characters (before the "...").
pub const SAMPLE_LINE_CHARS: usize = 80;

/// Which transcript the meeting shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Type)]
pub enum TranscriptKind {
    /// The final pass.
    Final,
    /// The live draft (no final pass yet).
    Draft,
}

/// Where a transcript row was heard.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Type)]
pub enum RowTrack {
    /// The mic of a call.
    You,
    /// The computer's audio of a call.
    Them,
    /// The mic of an in-room meeting.
    Room,
}

/// A link from a summary item to the transcript.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Type)]
pub struct Citation {
    /// The cited segment (hyphenated UUID).
    pub segment_id: String,
    /// Its position in [`MeetingDetail::transcript`]; `None` when the transcript no longer
    /// has it (the meeting was transcribed again since the summary).
    pub row: Option<u32>,
}

/// A decision or a note bullet with its citations.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Type)]
pub struct CitedText {
    /// The text.
    pub text: String,
    /// What it cites.
    pub citations: Vec<Citation>,
}

/// An action item (read-only: ticks live in the Obsidian note).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Type)]
pub struct ActionView {
    /// Who.
    pub owner: String,
    /// What.
    pub task: String,
    /// When, as the summary said it.
    pub due: Option<String>,
    /// What it cites.
    pub citations: Vec<Citation>,
}

/// A section of enhanced notes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Type)]
pub struct NotesView {
    /// The heading.
    pub heading: String,
    /// The bullets.
    pub bullets: Vec<CitedText>,
}

/// The current summary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Type)]
pub struct SummaryView {
    /// Its id (hyphenated UUID): edits name the summary they change (T12.16).
    pub id: String,
    /// Its title.
    pub title: String,
    /// TL;DR.
    pub tldr: String,
    /// Decisions.
    pub decisions: Vec<CitedText>,
    /// Action items.
    pub actions: Vec<ActionView>,
    /// Enhanced notes (may be empty).
    pub notes: Vec<NotesView>,
    /// Open questions (may be empty).
    pub open_questions: Vec<String>,
}

/// One transcript row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Type)]
pub struct TranscriptRow {
    /// Segment id (hyphenated UUID).
    pub segment_id: String,
    /// Start in meeting time (ms).
    pub start_ms: u64,
    /// The speaker row (`None` in a live draft before speakers were found).
    pub speaker_id: Option<String>,
    /// The name shown.
    pub speaker: String,
    /// Where it was heard.
    pub track: RowTrack,
    /// The text.
    pub text: String,
    /// The user edited the text (T12.16): it is kept by a re-transcription, and it has no
    /// per-word times.
    pub edited: bool,
    /// Words the word list suggests a listed word for (T12.15e), in token order; empty in a
    /// draft.
    pub suggestions: Vec<WordSuggestionView>,
    /// Words changed from a suggestion (T12.15e), in token order; empty in a draft.
    pub changed_words: Vec<ChangedWordView>,
}

/// A word of a transcript line with a listed word that sounds like it
/// (`earshot_runtime::word_choices`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Type)]
pub struct WordSuggestionView {
    /// Index among the line's white-space tokens (the text split on runs of white space).
    pub token: u32,
    /// The word as decoded, without the punctuation around it.
    pub decoded: String,
    /// The listed word offered in its place.
    pub listed: String,
}

/// A word of a transcript line the user changed from a suggestion.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Type)]
pub struct ChangedWordView {
    /// Index among the line's white-space tokens.
    pub token: u32,
    /// The word as decoded ("Put back" restores it), without the punctuation around it.
    pub decoded: String,
    /// The word as written now, without the punctuation around it.
    pub written: String,
}

/// Whether a speaker is the user.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Type)]
pub enum SpeakerKind {
    /// The meeting's "Me" row.
    Me,
    /// Anyone else.
    Other,
}

/// Where a speaker came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Type)]
pub enum SpeakerSource {
    /// Heard on the mic.
    Mic,
    /// Heard in the computer's audio (the far end of a call).
    FarEnd,
    /// Named by a stored voiceprint after the meeting.
    Voiceprint,
}

/// The person a speaker is linked to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Type)]
pub struct PersonRef {
    /// Person id (hyphenated UUID).
    pub id: String,
    /// Their name.
    pub name: String,
    /// A voiceprint is stored for them.
    pub has_voiceprint: bool,
}

/// One row of the Speakers panel.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Type)]
pub struct SpeakerCard {
    /// Speaker id (hyphenated UUID).
    pub id: String,
    /// The name shown in the transcript.
    pub name: String,
    /// Me or someone else.
    pub kind: SpeakerKind,
    /// The linked person.
    pub person: Option<PersonRef>,
    /// Where the speaker came from.
    pub source: SpeakerSource,
    /// Share of the transcript's talk time, in whole percent.
    pub share_percent: u32,
    /// The start of their longest segment (at most [`SAMPLE_LINE_CHARS`], then "...").
    pub sample_line: String,
    /// Not the user, not linked to a person and not ignored: the voice needs a name
    /// ([`needs_name`]).
    pub needs_name: bool,
    /// The user chose to ignore this voice in this meeting (T12.19, `edit::is_ignored`).
    pub ignored: bool,
    /// The user may ignore this voice (`edit::can_ignore`).
    pub can_ignore: bool,
    /// The user may say this voice is theirs ("This is me": `edit::can_mark_me`).
    pub can_mark_me: bool,
    /// The user may say this "Me" row is not theirs ("This isn't me": `edit::can_unmark_me`).
    pub can_unmark_me: bool,
}

/// The meeting's header line.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Type)]
pub struct DetailHeader {
    /// Title; empty until the user or a summary gives one.
    pub title: String,
    /// Start (Unix ms).
    pub started_at_ms: i64,
    /// Length in ms (`None` for an interrupted recording).
    pub duration_ms: Option<u64>,
    /// What it recorded.
    pub mode: CaptureMode,
    /// Which transcript is shown (`None` without any).
    pub transcript: Option<TranscriptKind>,
    /// The model that wrote the current summary, when known.
    pub summary_model: Option<String>,
    /// The backend that ran the shown transcript ("GPU (CUDA)"), when recorded.
    pub transcript_backend: Option<String>,
}

/// Everything the meeting detail screen shows.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Type)]
pub struct MeetingDetail {
    /// Meeting id (hyphenated UUID).
    pub id: String,
    /// Where it is in its life.
    pub state: MeetingState,
    /// The header.
    pub header: DetailHeader,
    /// The current summary.
    pub summary: Option<SummaryView>,
    /// The transcript in meeting order.
    pub transcript: Vec<TranscriptRow>,
    /// The speakers with a line in the transcript, in creation order.
    pub speakers: Vec<SpeakerCard>,
    /// The previous summary can be restored ("Switch to previous summary").
    pub can_undo_summary: bool,
    /// The current summary no longer matches the transcript
    /// (`MeetingView::summary_out_of_date`): it cites segments the transcript no longer has,
    /// or the user edited the transcript's text after it was written (T12.16).
    pub summary_stale: bool,
    /// At least one track's recording is still kept: re-transcription and speaker samples
    /// need it (T7.4k: audio retention deletes recordings, everything else stays).
    pub audio_kept: bool,
    /// Every time a summary of this meeting handed text to Claude Code (T7.4ga) or Codex
    /// (T7.4gc), oldest first (PLAN.md 3.9: when and how much, never text; empty for any
    /// other summary and for a record this version cannot read).
    pub handovers: Vec<HandoverItem>,
    /// The meeting's summary is kept on this computer (T12.13), `None` when it may go to the
    /// summary writer.
    pub kept_local: Option<crate::keep_local::KeptLocalView>,
    /// The word list (the Dictionary's words, then saved people's names; T12.15e) for "another
    /// word" on a changed word; empty for a draft transcript. It stays on this computer.
    pub word_list: Vec<String>,
    /// The Dictionary's entries as stored (T12.15f), so a word of the transcript offers "Remove
    /// from Dictionary" when it is one (case ignored) and "Add to Dictionary" otherwise; empty
    /// for a draft transcript, which offers neither.
    pub dictionary: Vec<String>,
}

/// One handover to Claude Code or Codex.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Type)]
pub struct HandoverItem {
    /// Which CLI (a fixed word: `claude_code`, `codex`).
    pub cli: String,
    /// When (Unix ms).
    pub at_ms: u64,
    /// Characters handed over.
    pub characters: u64,
    /// How the run ended (a fixed word: `answered`, `cancelled`, `not_signed_in`, ...).
    pub outcome: String,
}

/// A person in the "Link to person" list.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Type)]
pub struct PersonItem {
    /// Person id (hyphenated UUID).
    pub id: String,
    /// Their name.
    pub name: String,
    /// A voiceprint is stored for them.
    pub has_voiceprint: bool,
    /// The user.
    pub is_me: bool,
}

impl PersonItem {
    fn of(p: &PersonInfo) -> Self {
        Self {
            id: p.id.to_string(),
            name: p.name.clone(),
            has_voiceprint: p.has_voiceprint,
            is_me: p.is_me,
        }
    }
}

/// Who a speaker is linked to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(tag = "kind")]
pub enum LinkChoice {
    /// An existing person.
    Existing {
        /// Person id.
        person: String,
    },
    /// A new person with this name.
    New {
        /// The name.
        name: String,
    },
}

/// What remembering a saved voice did (T12.19).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Type)]
pub enum RememberOutcome {
    /// Added to the person's voiceprint.
    Remembered,
    /// Not enough speech for a new voiceprint.
    NotEnoughSpeech,
    /// No stored voice for this speaker.
    NoStoredVoice,
    /// Already remembered.
    AlreadyRemembered,
    /// The speaker model cannot match voiceprints.
    Unsupported,
}

impl RememberOutcome {
    fn of(r: &Remembered) -> Self {
        match r {
            Remembered::Enrolled { .. } => Self::Remembered,
            Remembered::NotEnoughSpeech { .. } => Self::NotEnoughSpeech,
            Remembered::NoStoredVoice => Self::NoStoredVoice,
            Remembered::AlreadyEnrolled => Self::AlreadyRemembered,
            Remembered::Uncalibrated { .. } => Self::Unsupported,
        }
    }
}

/// What "Save" on a voice did (T12.19).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Type)]
pub struct SavedVoiceResult {
    /// The person the voice is linked to (hyphenated UUID).
    pub person: String,
    /// What remembering its voice did.
    pub outcome: RememberOutcome,
}

/// Which text of a summary an edit changes (T12.16; `edit::SummaryField`). Indices count
/// from 0 in the order the summary shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(tag = "kind")]
pub enum SummaryFieldRef {
    /// The title.
    Title,
    /// The TL;DR.
    Tldr,
    /// A decision.
    Decision {
        /// Which.
        index: u32,
    },
    /// Who an action is for.
    ActionOwner {
        /// Which action.
        index: u32,
    },
    /// What an action is.
    ActionTask {
        /// Which action.
        index: u32,
    },
    /// When an action is due ("" removes it).
    ActionDue {
        /// Which action.
        index: u32,
    },
    /// A notes heading.
    NoteHeading {
        /// Which section.
        section: u32,
    },
    /// A notes bullet.
    NoteBullet {
        /// Which section.
        section: u32,
        /// Which bullet of it.
        bullet: u32,
    },
    /// An open question.
    OpenQuestion {
        /// Which.
        index: u32,
    },
}

impl SummaryFieldRef {
    /// The runtime's field.
    #[must_use]
    pub fn field(self) -> SummaryField {
        let at = |i: u32| usize::try_from(i).unwrap_or(usize::MAX);
        match self {
            Self::Title => SummaryField::Title,
            Self::Tldr => SummaryField::Tldr,
            Self::Decision { index } => SummaryField::Decision(at(index)),
            Self::ActionOwner { index } => SummaryField::ActionOwner(at(index)),
            Self::ActionTask { index } => SummaryField::ActionTask(at(index)),
            Self::ActionDue { index } => SummaryField::ActionDue(at(index)),
            Self::NoteHeading { section } => SummaryField::NoteHeading(at(section)),
            Self::NoteBullet { section, bullet } => {
                SummaryField::NoteBullet(at(section), at(bullet))
            }
            Self::OpenQuestion { index } => SummaryField::OpenQuestion(at(index)),
        }
    }
}

/// Where an export wrote the note.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Type)]
pub struct ExportDone {
    /// Vault-relative path of the note.
    pub note_path: String,
}

fn err(kind: &str, message: &str) -> IpcError {
    IpcError {
        kind: kind.into(),
        message: message.into(),
        retryable: false,
    }
}

/// Plain user copy for an edit refusal. Exhaustive: a new refusal needs its copy here.
#[must_use]
pub fn edit_error(e: &EditError) -> IpcError {
    match e {
        EditError::MeetingNotFound(_) => err("meeting_not_found", "This meeting no longer exists."),
        EditError::SpeakerNotFound(_) => err(
            "speaker_not_found",
            "This speaker is no longer in the meeting.",
        ),
        EditError::PersonNotFound(_) => err("person_not_found", "This person no longer exists."),
        EditError::InvalidName(NameProblem::Empty) => err("invalid_name", "Enter a name."),
        EditError::InvalidName(NameProblem::TooLong) => {
            err("invalid_name", "Names can be up to 100 characters long.")
        }
        EditError::InvalidName(NameProblem::ControlCharacter) => {
            err("invalid_name", "Names cannot contain line breaks or tabs.")
        }
        EditError::SameSpeaker(_) => err("same_speaker", "A speaker cannot be merged into itself."),
        EditError::MergeAwayMe(_) => err(
            "merge_away_me",
            "Your own row cannot be merged away. Merge the other speaker into it instead.",
        ),
        EditError::NoPerson(_) => err("no_person", "Name this voice or link it to a person first."),
        EditError::MeVoice(_) => err(
            "me_voice",
            "Your own voice is set up in Settings, not from a meeting.",
        ),
        EditError::NotTheUser(_) => err("not_the_user", "Only your own row can be linked to you."),
        EditError::MeIsTheUser(_) => {
            err("me_is_the_user", "Your own row can only be linked to you.")
        }
        EditError::PersonIsTheUser(_) => err(
            "person_is_the_user",
            "You cannot be removed. Your name and voice are set above.",
        ),
        EditError::NotInRoom(_) => err(
            "not_in_room",
            "Only a voice in an in-room meeting can be marked as you. In a call, your voice comes \
             from your microphone.",
        ),
        EditError::MeetingHasMe(_) => err(
            "meeting_has_me",
            "One voice in this meeting is already you. Choose \"This isn't me\" on it first.",
        ),
        EditError::NotMe(_) => err("not_me", "This voice is not marked as you."),
        EditError::NoPreviousSummary => err("no_previous_summary", "There is no previous summary."),
        EditError::PreviousSummaryStale => err(
            "previous_summary_stale",
            "The previous summary refers to a transcript that has been replaced. Re-summarise \
             instead.",
        ),
        // Folder refusals (T9.5b), shown by the folder screens (T9.5c).
        EditError::FolderNotFound(_) => err("folder_not_found", "This folder no longer exists."),
        EditError::FolderNameTaken(_) => {
            err("folder_name_taken", "Another folder already has this name.")
        }
        EditError::MeetingRecording(_) => err(
            "meeting_recording",
            "A meeting can be moved to a folder once it has finished recording.",
        ),
        // Title and transcript text (T12.16).
        EditError::InvalidTitle(TextProblem::TooLong) => {
            err("invalid_title", "Titles can be up to 200 characters long.")
        }
        EditError::InvalidTitle(TextProblem::ControlCharacter) => err(
            "invalid_title",
            "Titles cannot contain line breaks or tabs.",
        ),
        // An empty title is allowed; kept for exhaustiveness.
        EditError::InvalidTitle(TextProblem::Empty) => err("invalid_title", "Enter a title."),
        EditError::InvalidText(TextProblem::Empty) => err(
            "invalid_text",
            "Enter the words of this line. A line cannot be left empty.",
        ),
        EditError::InvalidText(TextProblem::TooLong) => {
            err("invalid_text", "A line can be up to 5,000 characters long.")
        }
        EditError::InvalidText(TextProblem::ControlCharacter) => {
            err("invalid_text", "A line cannot contain line breaks or tabs.")
        }
        EditError::SegmentNotFound(_) => err(
            "segment_not_found",
            "This line is no longer in the transcript. It may have been transcribed again.",
        ),
        EditError::DraftTranscript(_) => err(
            "draft_transcript",
            "The words can be corrected once the final transcript is ready.",
        ),
        EditError::SummaryNotCurrent(_) | EditError::SummaryItemNotFound => err(
            "summary_changed",
            "The summary changed while you were editing it. Check the new text and edit again.",
        ),
        EditError::InvalidSummaryText(TextProblem::Empty) => err(
            "invalid_summary_text",
            "Enter some words. Only a due date can be left empty.",
        ),
        EditError::InvalidSummaryText(TextProblem::TooLong) => err(
            "invalid_summary_text",
            "This text is too long for the summary.",
        ),
        EditError::InvalidSummaryText(TextProblem::ControlCharacter) => err(
            "invalid_summary_text",
            "Summary text cannot contain line breaks or tabs.",
        ),
        // The Speakers panel's Ignore (T12.19).
        EditError::NotUnnamed(_) => err(
            "not_unnamed",
            "Only a voice that needs a name can be ignored.",
        ),
        EditError::NoStoredVoice(_) => err(
            "no_stored_voice",
            "Earshot kept nothing of this voice to remember the choice by. Re-transcribe the \
             meeting, then ignore it.",
        ),
        // Suggested words (T12.15e).
        EditError::WordNotFound(_) => err(
            "word_not_found",
            "This word is no longer in the line. The line may have been edited.",
        ),
        EditError::WordNotListed => err(
            "word_not_listed",
            "That word is not in your dictionary or among the people you saved.",
        ),
        EditError::NoWordChoice(_) => err(
            "no_word_choice",
            "There is nothing to put back for this word. The line may have been edited.",
        ),
    }
}

/// An id from the UI.
///
/// # Errors
/// `invalid_id` for text that is not a hyphenated UUID.
pub fn parse_id(id: &str) -> Result<EntityId, IpcError> {
    id.parse()
        .map_err(|_| err("invalid_id", "The app sent an id it cannot read."))
}

fn not_found() -> IpcError {
    err("meeting_not_found", "This meeting no longer exists.")
}

fn citations(ids: &[EntityId], rows: &BTreeMap<EntityId, u32>) -> Vec<Citation> {
    ids.iter()
        .map(|id| Citation {
            segment_id: id.to_string(),
            row: rows.get(id).copied(),
        })
        .collect()
}

fn summary_view(id: EntityId, s: &SummaryV1, rows: &BTreeMap<EntityId, u32>) -> SummaryView {
    SummaryView {
        id: id.to_string(),
        title: s.title.clone(),
        tldr: s.tldr.clone(),
        decisions: s
            .decisions
            .iter()
            .map(|d| CitedText {
                text: d.text.clone(),
                citations: citations(&d.source_segment_ids, rows),
            })
            .collect(),
        actions: s
            .action_items
            .iter()
            .map(|a| ActionView {
                owner: a.owner.clone(),
                task: a.task.clone(),
                due: a.due.clone(),
                citations: citations(&a.source_segment_ids, rows),
            })
            .collect(),
        notes: s
            .enhanced_notes
            .iter()
            .map(|n| NotesView {
                heading: n.heading.clone(),
                bullets: n
                    .bullets
                    .iter()
                    .map(|b| CitedText {
                        text: b.text.clone(),
                        citations: citations(&b.source_segment_ids, rows),
                    })
                    .collect(),
            })
            .collect(),
        open_questions: s.open_questions.clone(),
    }
}

/// The first [`SAMPLE_LINE_CHARS`] of `text`, cut at a word, with "..." when cut.
#[must_use]
pub fn sample_line(text: &str) -> String {
    let text = text.trim();
    if text.chars().count() <= SAMPLE_LINE_CHARS {
        return text.to_owned();
    }
    let cut: String = text.chars().take(SAMPLE_LINE_CHARS).collect();
    let at_word = match cut.rfind(' ') {
        Some(i) if i > 0 => &cut[..i],
        _ => &cut[..],
    };
    format!("{}...", at_word.trim_end())
}

fn duration_ms(s: &Segment) -> u64 {
    s.end.as_millis().saturating_sub(s.start.as_millis())
}

/// Whether the final pass named `row` from a voiceprint (it linked the row to a person and
/// the user has not edited the row since).
fn named_by_voiceprint(row: &Speaker, identities: &[SpeakerIdentity]) -> bool {
    identities.iter().any(|i| {
        i.speaker == row.id
            && i.given_person.is_some()
            && i.given_person == row.person_id
            && !i.edited(row)
    })
}

fn speaker_cards(
    view: &MeetingView,
    persons: &[PersonInfo],
    identities: &[SpeakerIdentity],
) -> Vec<SpeakerCard> {
    let total: u64 = view.segments.iter().map(duration_ms).sum();
    view.speakers
        .iter()
        .filter_map(|s| {
            let own: Vec<&Segment> = view
                .segments
                .iter()
                .filter(|seg| seg.speaker_id == Some(s.id))
                .collect();
            // A speaker with no line in this transcript (a voice no word went to, from a
            // final pass before T7.3b) has nothing to show, play or name.
            if own.is_empty() {
                return None;
            }
            let talk: u64 = own.iter().map(|seg| duration_ms(seg)).sum();
            let share_percent = (talk.saturating_mul(100) + total / 2)
                .checked_div(total)
                .map_or(0, |p| u32::try_from(p).unwrap_or(100));
            let longest = own
                .iter()
                .max_by_key(|seg| (duration_ms(seg), std::cmp::Reverse(seg.start)))
                .map(|seg| sample_line(&seg.text))
                .unwrap_or_default();
            let person = s.person_id.and_then(|id| {
                persons.iter().find(|p| p.id == id).map(|p| PersonRef {
                    id: p.id.to_string(),
                    name: p.name.clone(),
                    has_voiceprint: p.has_voiceprint,
                })
            });
            let ignored = edit::is_ignored(s, identities);
            let kind = if s.role == SpeakerRole::Me {
                SpeakerKind::Me
            } else {
                SpeakerKind::Other
            };
            let source = if kind == SpeakerKind::Other && named_by_voiceprint(s, identities) {
                SpeakerSource::Voiceprint
            } else if s.role == SpeakerRole::Remote {
                SpeakerSource::FarEnd
            } else {
                SpeakerSource::Mic
            };
            Some(SpeakerCard {
                id: s.id.to_string(),
                name: s.display_name.clone(),
                kind,
                needs_name: needs_name(s.role, s.person_id.is_some(), ignored),
                ignored,
                can_ignore: edit::can_ignore(s, identities),
                can_mark_me: edit::can_mark_me(&view.tracks, &view.speakers, s),
                can_unmark_me: edit::can_unmark_me(&view.tracks, s),
                person,
                source,
                share_percent,
                sample_line: longest,
            })
        })
        .collect()
}

/// The meeting detail of `meeting`.
///
/// # Errors
/// `meeting_not_found` for an unknown or deleted meeting; store errors.
pub fn meeting_detail(services: &Services, meeting: EntityId) -> Result<MeetingDetail, IpcError> {
    // The activity comes from this process's job leases, read before the store lock
    // (`meeting_activity` takes the activity lock, then the store lock, never nested); a job
    // that starts or ends between the two reads shows on the next read.
    let state = services
        .meeting_activity(meeting)?
        .map(MeetingState::of)
        .ok_or_else(not_found)?;
    // Everything else is one snapshot under one store lock, so the shown summary,
    // `can_undo_summary` and the transcript the citations resolve against agree.
    let (
        persons,
        history,
        view,
        identities,
        backend,
        handovers,
        audio_kept,
        kept_local,
        words,
        dictionary,
    ) = {
        let db = services.store();
        let persons = edit::list_persons_in(&db)?;
        let history = match edit::summary_history_in(&**db, meeting) {
            Ok(h) => h,
            Err(RuntimeError::Edit(EditError::MeetingNotFound(_))) => return Err(not_found()),
            Err(e) => return Err(e.into()),
        };
        let view = MeetingView::load(&**db, meeting)
            .map_err(RuntimeError::from)?
            .ok_or_else(not_found)?;
        let identities = edit::identities_in(&db, meeting)?;
        let backend = match view.pass {
            Some(TranscriptPass::Final) => pass_backend(&db, meeting, TranscriptPass::Final, None)?,
            Some(TranscriptPass::Live) => {
                pass_backend(&db, meeting, TranscriptPass::Live, Some(TrackId::Mic))?
            }
            None => None,
        };
        let handovers = earshot_runtime::claude_code::summary_handovers(&db, meeting)?;
        let audio_kept = db
            .audio_files(meeting)
            .map_err(RuntimeError::from)?
            .iter()
            .any(|f| services.data.resolve(&f.path).is_ok_and(|p| p.exists()));
        let kept_local = crate::keep_local::kept_local_view(&db, meeting);
        let words = word_choices::word_marks(&db, &view)?;
        let dictionary = if view.pass == Some(TranscriptPass::Final) {
            // A stored list this version refuses reads as no words, as everywhere else.
            Vec::from(vocabulary::vocabulary(&db)?.0)
        } else {
            Vec::new()
        };
        (
            persons, history, view, identities, backend, handovers, audio_kept, kept_local, words,
            dictionary,
        )
    };
    let in_room = !view.tracks.iter().any(|t| t.kind == TrackId::Reference);
    let track_of = |segment: &Segment| match view
        .tracks
        .iter()
        .find(|t| t.id == segment.track)
        .map(|t| t.kind)
    {
        _ if in_room => RowTrack::Room,
        Some(TrackId::Reference) => RowTrack::Them,
        _ => RowTrack::You,
    };
    let transcript: Vec<TranscriptRow> = view
        .segments
        .iter()
        .map(|s| TranscriptRow {
            segment_id: s.id.to_string(),
            start_ms: s.start.as_millis(),
            speaker_id: s.speaker_id.map(|id| id.to_string()),
            speaker: view.speaker_name(s),
            track: track_of(s),
            text: s.text.clone(),
            edited: view.edited.contains_key(&s.id),
            suggestions: words.lines.get(&s.id).map_or_else(Vec::new, |l| {
                l.suggestions
                    .iter()
                    .map(|w| WordSuggestionView {
                        token: token_index(w.token),
                        decoded: w.decoded.clone(),
                        listed: w.listed.clone(),
                    })
                    .collect()
            }),
            changed_words: words.lines.get(&s.id).map_or_else(Vec::new, |l| {
                l.changed
                    .iter()
                    .map(|w| ChangedWordView {
                        token: token_index(w.token),
                        decoded: w.decoded.clone(),
                        written: w.written.clone(),
                    })
                    .collect()
            }),
        })
        .collect();
    let rows: BTreeMap<EntityId, u32> = view
        .segments
        .iter()
        .enumerate()
        .map(|(i, s)| (s.id, u32::try_from(i).unwrap_or(u32::MAX)))
        .collect();
    let summary = view
        .summary
        .as_ref()
        .map(|s| summary_view(s.id, &s.summary, &rows));
    let summary_stale = view.summary_out_of_date();
    let can_undo_summary = history.previous.is_some_and(|p| p.restorable);
    Ok(MeetingDetail {
        id: meeting.to_string(),
        state,
        header: DetailHeader {
            title: view.meeting.title.clone(),
            started_at_ms: i64::try_from(view.meeting.started_unix_ms).unwrap_or(i64::MAX),
            duration_ms: view.meeting.duration_ms,
            mode: if in_room {
                CaptureMode::InRoom
            } else {
                CaptureMode::Call
            },
            transcript: view.pass.map(|p| match p {
                TranscriptPass::Final => TranscriptKind::Final,
                TranscriptPass::Live => TranscriptKind::Draft,
            }),
            summary_model: view.summary.as_ref().and_then(|s| s.model.clone()),
            transcript_backend: backend.map(|b| backend_label(b.kind).to_owned()),
        },
        speakers: speaker_cards(&view, &persons, &identities),
        summary,
        transcript,
        can_undo_summary,
        summary_stale,
        audio_kept,
        handovers: handovers
            .entries
            .iter()
            .map(|e| HandoverItem {
                cli: serde_json::to_value(e.cli)
                    .ok()
                    .and_then(|v| v.as_str().map(str::to_owned))
                    .unwrap_or_default(),
                at_ms: e.at_unix_ms,
                characters: e.characters,
                outcome: serde_json::to_value(e.outcome)
                    .ok()
                    .and_then(|v| v.as_str().map(str::to_owned))
                    .unwrap_or_default(),
            })
            .collect(),
        kept_local,
        word_list: words.list,
        dictionary,
    })
}

/// What adding a word from the transcript did (T12.15f).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Type)]
pub struct DictionaryWordAdded {
    /// The word as the Dictionary holds it ("Undo" removes exactly this).
    pub word: String,
    /// False when the Dictionary already had it (case ignored): nothing changed, nothing to
    /// undo.
    pub added: bool,
}

/// Adds the word of a transcript token to the Dictionary (`vocabulary::add_word`: the token's
/// core, at the end of the list).
///
/// # Errors
/// `invalid_setting` with the Dictionary page's own message (a full list, a word too long, a
/// token that is not a word): nothing stored; store errors.
pub fn add_dictionary_word(
    services: &Services,
    token: &str,
) -> Result<DictionaryWordAdded, IpcError> {
    let added = vocabulary::add_word(&mut services.store(), token)
        .map_err(crate::summary_vocabulary::refusal)?;
    Ok(DictionaryWordAdded {
        word: added.word,
        added: added.added,
    })
}

/// Removes the Dictionary entry that is `word` (case ignored; "Remove from Dictionary" and
/// "Undo" after an add). Returns whether an entry went.
///
/// # Errors
/// Store errors.
pub fn remove_dictionary_word(services: &Services, word: &str) -> Result<bool, IpcError> {
    vocabulary::remove_word(&mut services.store(), word).map_err(crate::summary_vocabulary::refusal)
}

/// Saves a line edit (`edit::edit_segment_offering`) and returns the words the bar under the
/// line offers to add to the Dictionary (T12.15f); nothing is added.
///
/// # Errors
/// As `edit::edit_segment`, through [`edit_error`].
pub fn edit_line(
    services: &Services,
    meeting: EntityId,
    segment: EntityId,
    text: &str,
) -> Result<Vec<String>, IpcError> {
    Ok(edit::edit_segment_offering(
        services, meeting, segment, text,
    )?)
}

/// A token index for the UI (a line holds at most a few thousand tokens).
fn token_index(token: usize) -> u32 {
    u32::try_from(token).unwrap_or(u32::MAX)
}

/// Every live person, for "Link to person".
///
/// # Errors
/// Store errors.
pub fn persons(services: &Services) -> Result<Vec<PersonItem>, IpcError> {
    Ok(edit::list_persons(services)?
        .iter()
        .map(PersonItem::of)
        .collect())
}

/// Links a speaker to a person; returns the person's id.
///
/// # Errors
/// Edit refusals ([`edit_error`]); store errors.
pub fn link(
    services: &Services,
    meeting: EntityId,
    speaker: EntityId,
    choice: &LinkChoice,
) -> Result<String, IpcError> {
    Ok(edit::link_speaker(services, meeting, speaker, &person_choice(choice)?)?.to_string())
}

fn person_choice(choice: &LinkChoice) -> Result<PersonChoice, IpcError> {
    Ok(match choice {
        LinkChoice::Existing { person } => PersonChoice::Existing(parse_id(person)?),
        LinkChoice::New { name } => PersonChoice::New { name: name.clone() },
    })
}

/// "Save" on a voice (T12.19, `edit::save_voice`): names it, links it and remembers it.
///
/// # Errors
/// Edit refusals ([`edit_error`]); store and voiceprint errors.
pub fn save_voice(
    services: &Services,
    meeting: EntityId,
    speaker: EntityId,
    choice: &LinkChoice,
) -> Result<SavedVoiceResult, IpcError> {
    let saved = edit::save_voice(services, meeting, speaker, &person_choice(choice)?)?;
    Ok(SavedVoiceResult {
        person: saved.person.to_string(),
        outcome: RememberOutcome::of(&saved.remembered),
    })
}

/// The bytes of a 16-bit mono WAV file of `samples` at `rate_hz` (the core WAV writer).
///
/// # Errors
/// `internal` for a zero rate or a WAV writer failure.
pub fn sample_wav(rate_hz: u32, samples: &[i16]) -> Result<Vec<u8>, IpcError> {
    let rate = SampleRate::new(rate_hz).ok_or_else(|| err("internal", "a sample rate of 0 Hz"))?;
    let audio: Vec<f32> = samples.iter().map(|&s| i16_to_f32(s)).collect();
    let mut out = Vec::with_capacity(44 + samples.len() * 2);
    write_wav(&mut out, rate, 1, &audio, WavFormat::Pcm16)
        .map_err(|e| err("internal", &format!("the sample could not be encoded: {e}")))?;
    Ok(out)
}

const BASE64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Standard base64 with padding (RFC 4648 section 4).
#[must_use]
pub fn base64(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            chunk.get(1).copied().unwrap_or(0),
            chunk.get(2).copied().unwrap_or(0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        for (i, shift) in [18, 12, 6, 0].into_iter().enumerate() {
            if i <= chunk.len() {
                out.push(char::from(BASE64[((n >> shift) & 63) as usize]));
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// The play-sample audio of a speaker as a base64 WAV file; `None` when the audio is no
/// longer kept (or the speaker has no segment with audio).
///
/// # Errors
/// Edit refusals; a recording that cannot be decoded; store errors.
pub fn speaker_sample(
    services: &Services,
    meeting: EntityId,
    speaker: EntityId,
) -> Result<Option<String>, IpcError> {
    match edit::speaker_sample(services, meeting, speaker, SAMPLE_MAX_MS)? {
        None => Ok(None),
        Some(s) => Ok(Some(base64(&sample_wav(s.sample_rate_hz, &s.samples)?))),
    }
}

/// Exports `meeting` into the vault and subfolder of the Obsidian settings
/// (`earshot_runtime::export_settings`), or `no_vault` when no vault is set.
///
/// # Errors
/// `no_vault`; `meeting_busy` (retryable) while the meeting is deleted; export and store
/// errors.
pub fn export(services: &Services, meeting: EntityId) -> Result<ExportDone, IpcError> {
    let (settings, _) = earshot_runtime::export_settings::obsidian_settings(&services.store())?;
    let Some(vault) = settings.vault.as_deref() else {
        return Err(err(
            "no_vault",
            "Set your Obsidian vault in Settings to export.",
        ));
    };
    let report = earshot_runtime::export::export_meeting(
        services,
        vault,
        meeting,
        &settings.export_options(),
        &mut *os_seeded_rng(),
    )?;
    Ok(ExportDone {
        note_path: report.path.to_string(),
    })
}

#[cfg(test)]
#[path = "detail_tests.rs"]
mod tests;
