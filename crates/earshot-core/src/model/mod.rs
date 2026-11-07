//! Data model (PLAN.md 3.5, T5.1): the persisted entities on the T0.7 identity base,
//! repository traits and an in-memory repository.
//!
//! | Entity | Sync class | Notes |
//! |---|---|---|
//! | [`Meeting`] | Syncable | start time (wall clock, injected), UTC offset, tags, template |
//! | [`Track`] | Syncable | metadata only: kind, device name, clock domain |
//! | [`SegmentRow`] | Syncable | an [`crate::asr::Segment`] plus sync metadata and its pass |
//! | [`Speaker`] | Syncable | per meeting; optional [`Person`] |
//! | [`Person`], [`Voiceprint`] | Syncable, SyncableOptIn | defined in [`crate::voiceprint`] |
//! | [`Note`] | Syncable | anchored in [`MeetingTime`] (ms from meeting start), never wall clock |
//! | [`Summary`] | Syncable | a validated [`crate::summary::SummaryV1`] |
//! | [`AudioFile`] | NeverSync | data-dir-relative path, format, pipeline and AEC version |
//! | [`ExportRecord`] | NeverSync | vault-relative path ([`VaultPath`]) and device id |
//!
//! `NeverSync` rows carry no [`SyncMeta`](crate::identity::SyncMeta): they never leave the device, so they are
//! hard-deleted rather than tombstoned. `tests/model_never_sync.rs` proves at compile time
//! that neither can reach [`crate::identity::sync_envelope`].
//!
//! [`MeetingTime`] stays defined in [`crate::asr`] (ASR produces it; it depends on the
//! pipeline rate) and is re-exported here, so there is one type for transcript times and
//! note anchors.
//!
//! Repositories: [`MeetingRepo`] (meetings and their syncable children), [`AudioFileRepo`],
//! [`ExportRepo`]; persons and voiceprints use [`crate::voiceprint::VoiceprintRepo`].
//! [`InMemoryRepo`] implements the three here; `earshot-store` implements them over SQLite.
//! Both run the same checks from [`repo_contract`]. Reads return tombstoned rows too (the
//! deletion must sync); [`live_segments`] and the `is_live` helpers filter them.

mod entities;
mod memory;
mod repo;
pub mod repo_contract;

pub use crate::asr::MeetingTime;
pub use crate::voiceprint::{Person, Stamp, Voiceprint};
pub use entities::{
    AudioFile, AudioFormat, ClockDomain, ExportRecord, Meeting, Note, SegmentRow, Speaker, Summary,
    Track, TranscriptPass, VaultPath, VaultPathError,
};
pub use memory::InMemoryRepo;
pub use repo::{
    AudioFileRepo, ExportRepo, MeetingErasure, MeetingRepo, RepoError, delete_meeting,
    live_segments, replace_segments,
};

#[cfg(test)]
mod tests;
