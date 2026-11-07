//! The meeting detail screen (T7.3): what one stored meeting shows ([`meeting_detail`]) and
//! the speaker edits, sample, export and deletion behind its commands.
//!
//! Everything is read from `earshot_runtime::view::MeetingView` (final transcript, else the
//! live draft) and changed only through `earshot_runtime::edit` and the runtime's export and
//! deletion. The store lock is held for each read only; nothing here holds it across FLAC
//! decoding or vault work (the runtime functions take it themselves).

use std::collections::{BTreeMap, BTreeSet};

use earshot_core::asr::Segment;
use earshot_core::audio::{SampleRate, WavFormat, i16_to_f32, write_wav};
use earshot_core::capture::TrackId;
use earshot_core::diar::SpeakerRole;
use earshot_core::identity::EntityId;
use earshot_core::model::{AudioFileRepo, Speaker, TranscriptPass};
use earshot_core::summary::SummaryV1;
use earshot_runtime::backends::pass_backend;
use earshot_runtime::edit::{self, EditError, NameProblem, PersonChoice, PersonInfo, Remembered};
use earshot_runtime::identities::{SpeakerIdentity, StoredIdentities, decode};
use earshot_runtime::view::MeetingView;
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

/// One row of the who-is-who panel.
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
    /// Not the user and not linked to a person: the voice needs a name.
    pub needs_name: bool,
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
    /// The current summary cites segments the transcript no longer has.
    pub summary_stale: bool,
    /// At least one track's recording is still kept: re-transcription and speaker samples
    /// need it (T7.4k: audio retention deletes recordings, everything else stays).
    pub audio_kept: bool,
    /// Every time a summary of this meeting handed text to Claude Code (T7.4ga) or Codex
    /// (T7.4gc), oldest first (PLAN.md 3.9: when and how much, never text; empty for any
    /// other summary and for a record this version cannot read).
    pub handovers: Vec<HandoverItem>,
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

/// What "remember these voices" did for one speaker.
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

/// One speaker's result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Type)]
pub struct RememberResult {
    /// Speaker id.
    pub speaker: String,
    /// What happened.
    pub outcome: RememberOutcome,
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
        EditError::NoPreviousSummary => err("no_previous_summary", "There is no previous summary."),
        EditError::PreviousSummaryStale => err(
            "previous_summary_stale",
            "The previous summary refers to a transcript that has been replaced. Re-summarise \
             instead.",
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

fn summary_view(s: &SummaryV1, rows: &BTreeMap<EntityId, u32>) -> SummaryView {
    SummaryView {
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

fn is_stale(s: &SummaryView) -> bool {
    s.decisions
        .iter()
        .flat_map(|d| &d.citations)
        .chain(s.actions.iter().flat_map(|a| &a.citations))
        .chain(
            s.notes
                .iter()
                .flat_map(|n| &n.bullets)
                .flat_map(|b| &b.citations),
        )
        .any(|c| c.row.is_none())
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
                needs_name: needs_name(s.role, s.person_id.is_some()),
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
    let (persons, history, view, identities, backend, handovers, audio_kept) = {
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
        let identities = match db
            .local_value(&earshot_store::identities::record_key(meeting))
            .map_err(RuntimeError::from)?
            .map(|b| decode(&b))
        {
            Some(StoredIdentities::Record(r)) => r.speakers,
            None | Some(StoredIdentities::Unreadable(_)) => Vec::new(),
        };
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
        (
            persons, history, view, identities, backend, handovers, audio_kept,
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
        .map(|s| summary_view(&s.summary, &rows));
    let summary_stale = summary.as_ref().is_some_and(is_stale);
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
    })
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
    let choice = match choice {
        LinkChoice::Existing { person } => PersonChoice::Existing(parse_id(person)?),
        LinkChoice::New { name } => PersonChoice::New { name: name.clone() },
    };
    Ok(edit::link_speaker(services, meeting, speaker, &choice)?.to_string())
}

/// "Remember these voices for next time" for `speakers`.
///
/// # Errors
/// Edit refusals; store and voiceprint errors.
pub fn remember(
    services: &Services,
    meeting: EntityId,
    speakers: &[EntityId],
) -> Result<Vec<RememberResult>, IpcError> {
    Ok(edit::remember_voices(services, meeting, speakers)?
        .into_iter()
        .map(|(speaker, r)| RememberResult {
            speaker: speaker.to_string(),
            outcome: match r {
                Remembered::Enrolled { .. } => RememberOutcome::Remembered,
                Remembered::NotEnoughSpeech { .. } => RememberOutcome::NotEnoughSpeech,
                Remembered::NoStoredVoice => RememberOutcome::NoStoredVoice,
                Remembered::AlreadyEnrolled => RememberOutcome::AlreadyRemembered,
                Remembered::Uncalibrated { .. } => RememberOutcome::Unsupported,
            },
        })
        .collect())
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

/// Speaker ids from the UI, without duplicates, in order.
///
/// # Errors
/// `invalid_id`.
pub fn parse_ids(ids: &[String]) -> Result<Vec<EntityId>, IpcError> {
    let mut seen = BTreeSet::new();
    let mut out = Vec::new();
    for id in ids {
        let id = parse_id(id)?;
        if seen.insert(id) {
            out.push(id);
        }
    }
    Ok(out)
}

#[cfg(test)]
#[path = "detail_tests.rs"]
mod tests;
