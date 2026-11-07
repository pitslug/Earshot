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
//! - [`retranscribe`]: the final pass again from the stored audio, with no live reuse.
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

use std::collections::BTreeSet;
use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use earshot_core::asr::{CancelToken, MeetingTime, TranscribeOptions, Transcriber};
use earshot_core::audio::f32_to_i16;
use earshot_core::capture::TrackId;
use earshot_core::diar::{EmbeddingModelId, SpeakerEmbedder, SpeakerRole, SpeakerSegmenter};
use earshot_core::identity::{Clock, EntityId};
use earshot_core::model::{
    AudioFileRepo, Meeting, MeetingRepo, Speaker, Stamp, Summary, Track, TranscriptPass,
    live_segments,
};
use earshot_core::pipeline::{FinalPassRequest, FinalPassTrack};
use earshot_core::summary::SummaryV1;
use earshot_core::voiceprint::{
    Person, VoiceprintError, VoiceprintRepo, enroll, identity_of, me_person, resolve_person,
};
use earshot_store::{StoreTx, first_free_speaker_name};
use rand_core::Rng;

use crate::backends::{Accelerator, EvidenceLoader, ModelDir, post_processor};
use crate::host::{HostPost, HostStep, ONNX_ACCELERATOR, PostLoad, final_pass};
use crate::identities::{IdentityRecord, StoredIdentities};
use crate::post::PostConfig;
use crate::view::MeetingView;
use crate::{MeetingJob, RuntimeError, Services, os_seeded_rng};

/// The longest name a speaker or person may get, in characters (after trimming).
pub const MAX_NAME_CHARS: usize = 100;

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
        }
    }
}

impl std::error::Error for EditError {}

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
fn live_meeting<R: MeetingRepo + ?Sized>(
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
/// "remembered" mark ([`remember_voices`]), so its voice can be remembered for the new one.
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
    let new_name = match choice {
        PersonChoice::New { name } => Some(clean_name(name)?),
        PersonChoice::Existing(_) => None,
    };
    write(services, |tx, stamp| {
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
            update_identity_entry(tx, meeting, speaker, |i| i.enrolled = false)?;
        }
        row.person_id = Some(person.id);
        row.display_name.clone_from(&person.display_name);
        row.meta.touch(stamp.hlc, stamp.clock);
        tx.put_speaker(row)?;
        Ok(person.id)
    })
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
/// `speakers`: what the who-is-who panel offers, by the refusals' own rule.
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

/// A person as the who-is-who panel lists it.
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

/// Opt-in enrolment ("Remember these voices for next time"): for each of `speakers` of
/// `meeting`, adds the speaker's centroid from the final pass's identity record to the
/// voiceprint of the person the speaker is linked to ([`enroll`], exact embedding model of
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
        live_meeting(&*tx, meeting)?;
        // Every speaker is checked before anything is enrolled.
        let mut targets = Vec::new();
        for &id in &speakers {
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

/// Whether every segment `summary` cites (notes, decisions, action items) is in `ids`. Only
/// the citations are checked: the summary passed validation when it was stored.
fn cites_only(summary: &SummaryV1, ids: &BTreeSet<EntityId>) -> bool {
    summary
        .enhanced_notes
        .iter()
        .flat_map(|n| &n.bullets)
        .flat_map(|b| &b.source_segment_ids)
        .chain(summary.decisions.iter().flat_map(|d| &d.source_segment_ids))
        .chain(
            summary
                .action_items
                .iter()
                .flat_map(|a| &a.source_segment_ids),
        )
        .all(|id| ids.contains(id))
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
    /// The meeting has a summary that cites a segment the current transcript no longer has
    /// (the same check as [`PreviousSummary::restorable`]): offer to summarise again.
    pub summary_stale: bool,
}

/// The final pass of `meeting` again from its stored audio: every live track of the
/// meeting, [`TranscribeOptions::batch`], no live reuse (`PostConfig::reuse_live` off),
/// production models ([`post_processor`]). Speaker names and Person links the user gave are
/// carried over by the final pass's identity matching. The existing summary is left as it
/// is; [`Retranscribed::summary_stale`] says when it cites the replaced transcript (the host
/// offers to summarise again).
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
        MeetingView::load(&**db, meeting)?.is_some_and(|v| {
            let ids: BTreeSet<EntityId> = v.segments.iter().map(|s| s.id).collect();
            v.summary.is_some_and(|s| !cites_only(&s.summary, &ids))
        })
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
