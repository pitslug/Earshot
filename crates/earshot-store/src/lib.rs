//! Storage adapter: SQLite database, FLAC audio files, retention and export file writer.
//!
//! - [`Store`]: `earshot.db` (WAL, migrations) implementing the core repository traits.
//! - [`DataDir`]: the per-device layout of PLAN.md 3.6.
//! - [`FlacTrackWriter`]: streaming 16-bit mono FLAC per track, finalised by rename.
//! - [`Store::finalize_track_audio`]: the only way an `AudioFile` row is written for new
//!   audio, and only after its FLAC file is finalised (PLAN.md 3.6 crash safety).
//! - [`run_retention`] (summary-gated, [`due_audio`]), [`load_meeting_audio`]: retention and
//!   re-transcription inputs; both tolerate rows whose file is missing.
//! - [`Store::delete_meeting`]: deletes a meeting (files before rows, tombstones, export
//!   records, device-local records the caller names, a truncating checkpoint).
//! - [`export_note`], [`validate_vault`]: atomic Obsidian export with re-export merge and
//!   find-by-`earshot_id`.

mod audio;
mod db;
mod delete;
mod export;
mod flac;
mod fsutil;
pub mod identities;
mod layout;
mod repo;
mod schema;

pub use audio::{
    AudioProvenance, DAY_MS, DueAudio, HOUR_MS, MeetingAudio, ORPHAN_GRACE_MS,
    RETENTION_CLOCK_PREFIX, RETENTION_CLOCK_VERSION, RemovedAudio, RetentionClock, RetentionPolicy,
    RetentionReport, TrackFileRecovery, decode_retention_clock, due_audio, encode_retention_clock,
    load_meeting_audio, recover_track_file, remove_audio, retention_clock, retention_clock_key,
    run_retention, sweep_orphan_audio,
};
pub use db::{SCHEMA_VERSION, Store, StoreError, StoreTx};
pub use delete::{MeetingDeletion, PersonRemoval, first_free_speaker_name};
pub use export::{
    ExportAction, ExportDb, ExportItem, ExportReport, VaultError, export_note, export_notes,
    validate_vault,
};
pub use flac::{
    FinishedFlac, FlacTrackWriter, PartRecovery, WriteFault, describe_flac, read_flac, recover_part,
};
pub use fsutil::total_memory;
pub use layout::DataDir;
