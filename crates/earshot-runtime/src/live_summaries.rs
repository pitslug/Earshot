//! Summaries made during the meeting (T8.3j, option A of `docs/live-summaries.md`).
//!
//! With the device-local setting on ([`LIVE_SUMMARIES_KEY`], off by default; the owner
//! 2026-11-03: an opt-in for a shorter wait at hang-up that may miss some points), a live
//! session maps each finished [`MapChunk::seconds`] window of its live transcript with the
//! local summary model on its own below-normal thread (`live::summary_thread`). A window is
//! finished once every transcribed track has settled past its end: the track's live worker
//! decoded past it and no mic final that starts before it is still held by the live echo
//! filter. The worker sees the live transcript so far, as the meeting would read it then: no
//! title, duration, attendees or notes, speakers "Me" and "Them" ([`crate::view`]).
//!
//! At the end of the session the partials are stored as one device-local record
//! (`live-summaries/<meeting>`, version outside the payload, [`encode`]). They hold summary
//! text, so they are content: [`crate::summary`]'s write transaction deletes the record with
//! every stored summary, and meeting deletion sweeps it with the other meeting records.
//!
//! At summary time the local path re-cites each recorded partial onto the transcript being
//! summarised ([`earshot_core::summary::recite`]: final segments on the same track that
//! overlap its live segments; action owners "Me"/"Them" renamed to the final speaker when
//! there is one) and reuses it for the window it was made for ([`reusable`]). A partial that
//! cannot be re-cited, or a record of another model, window length, Dictionary word list
//! (T12.20) or version, is not used:
//! those windows are mapped again from the final transcript, so the result is never worse
//! than the ordinary run, only slower. T8.3ja measured the reused partials against the
//! ordinary run on AMI (`docs/live-summaries.md`).
//!
//! A live session never makes a network connection, so only local summaries are made during
//! the meeting; a remote endpoint ignores the setting.

use std::fmt;
use std::ops::Range;
use std::sync::Arc;

use earshot_core::asr::CancelToken;
use earshot_core::identity::EntityId;
use earshot_core::prompt::SummaryInput;
use earshot_core::summary::{SummaryV1, TimedSegment, recite};
use earshot_store::Store;
use serde::{Deserialize, Serialize};

use crate::RuntimeError;
use crate::local::{DeviceRecord, LocalKey, MeetingRecord};

#[cfg(test)]
#[path = "live_summaries_tests.rs"]
mod tests;

/// The `local_state` key of the setting.
pub const LIVE_SUMMARIES_KEY: &str = "settings/live-summaries";

/// What the user is told about the setting (the CLI `--help` and the settings screen).
pub const LIVE_SUMMARIES_HELP: &str = "Summarise during the meeting: the summary is ready \
    sooner after hang-up, but it may miss some points.";

/// Why live summaries are off when the battery rule of speaker detection stops them.
pub const ON_BATTERY_REASON: &str =
    "summaries wait for the end of the call on battery power, as speaker detection does";
/// Why live summaries stopped when the machine switched to battery during the meeting.
pub const UNPLUGGED_REASON: &str =
    "summarising stopped for the rest of the call when the computer switched to battery power";

/// The setting as stored ([`live_summaries_setting`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StoredSwitch {
    /// Nothing stored.
    Absent,
    /// On or off.
    Set(bool),
    /// A value this build does not know (written by a newer one), as stored.
    Unknown(String),
}

impl StoredSwitch {
    /// Whether the setting is on: the stored value, else off (the default).
    #[must_use]
    pub fn on(&self) -> bool {
        matches!(self, Self::Set(true))
    }
}

/// The stored setting (one read).
///
/// # Errors
/// Store errors.
pub fn live_summaries_setting(store: &Store) -> Result<StoredSwitch, RuntimeError> {
    let Some(bytes) = store.local_value(LIVE_SUMMARIES_KEY)? else {
        return Ok(StoredSwitch::Absent);
    };
    Ok(match bytes.as_slice() {
        b"on" => StoredSwitch::Set(true),
        b"off" => StoredSwitch::Set(false),
        other => StoredSwitch::Unknown(String::from_utf8_lossy(other).into_owned()),
    })
}

/// Stores the setting (replacing an earlier one).
///
/// # Errors
/// Store errors.
pub fn set_live_summaries(store: &mut Store, on: bool) -> Result<(), RuntimeError> {
    let value: &[u8] = if on { b"on" } else { b"off" };
    crate::local::set(store, &DeviceRecord::LiveSummaries.key(), value)?;
    Ok(())
}

/// LLM threads of the map worker during a meeting: the logical cores live transcription
/// (`live_threads`, every track together) does not use, less two (the session and speaker
/// threads), at least one and at most `cap` (the post-meeting summary's count). The rule
/// T8.3jb2 checks on the Surface by the largest live transcript lag.
#[must_use]
pub fn live_summary_threads(cores: usize, live_threads: usize, cap: i32) -> i32 {
    let free = cores.saturating_sub(live_threads + 2).max(1);
    i32::try_from(free).unwrap_or(i32::MAX).clamp(1, cap.max(1))
}

/// Maps one window of the live transcript (the local model in production,
/// [`crate::summary::SummaryService::live_mapper`]; a fake in tests).
pub trait MapChunk: Send + Sync {
    /// The model's id, recorded with the partials ([`LiveSummaryRecord::model`]).
    fn model_id(&self) -> &str;
    /// The window length in seconds (the post-meeting summary's `chunk_seconds`).
    fn seconds(&self) -> u32;
    /// The partial summary of the last of `ranges` (`time_chunks` of `input`), citing the
    /// segments of `input`; stops early when `cancel` is cancelled.
    ///
    /// # Errors
    /// Why there is no partial (a load or run failure, cancelled), for the event and log.
    fn map(
        &self,
        input: &SummaryInput,
        ranges: &[Range<usize>],
        cancel: &CancelToken,
    ) -> Result<SummaryV1, String>;
}

/// Live summaries for a session ([`crate::live::LiveConfig::live_summaries`]).
#[derive(Clone)]
pub struct LiveSummaries {
    /// The model.
    pub mapper: Arc<dyn MapChunk>,
    /// Whether the worker runs at below-normal OS priority (true in production; tests of
    /// what it computes set false, as for speaker work).
    pub background: bool,
}

impl PartialEq for LiveSummaries {
    fn eq(&self, other: &Self) -> bool {
        std::ptr::addr_eq(Arc::as_ptr(&self.mapper), Arc::as_ptr(&other.mapper))
            && self.background == other.background
    }
}

impl fmt::Debug for LiveSummaries {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LiveSummaries")
            .field("model", &self.mapper.model_id())
            .field("seconds", &self.mapper.seconds())
            .field("background", &self.background)
            .finish()
    }
}

/// The format of the stored record; a record of another version is not reused. Version 2
/// (T12.20) added [`LiveSummaryRecord::vocabulary`].
pub const LIVE_SUMMARY_RECORD_VERSION: u32 = 2;

/// The versions [`decode`] reads: a version-1 record reads with no word list, which
/// [`reusable`] refuses.
const READABLE_VERSIONS: [u32; 2] = [1, LIVE_SUMMARY_RECORD_VERSION];

/// One window's partial summary, citing live segments.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LivePartial {
    /// The window: segments starting in `[window * seconds, (window + 1) * seconds)`.
    pub window: u64,
    /// The partial.
    pub summary: SummaryV1,
}

/// What a session's map worker left (module docs).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LiveSummaryRecord {
    /// The model that made the partials.
    pub model: String,
    /// The window length in seconds.
    pub seconds: u32,
    /// The partials, by window.
    pub partials: Vec<LivePartial>,
    /// Why the worker stopped before the end of the meeting; `None` when it ran to the end.
    pub stopped: Option<String>,
    /// The user's Dictionary words the partials were mapped with ([`crate::vocabulary`]);
    /// `None` when not known (a version-1 record), which is never reused.
    #[serde(default)]
    pub vocabulary: Option<Vec<String>>,
}

/// A stored record as read back: usable, or why not.
#[derive(Debug, Clone, PartialEq)]
pub enum StoredLiveSummaries {
    /// The record.
    Record(Box<LiveSummaryRecord>),
    /// A record of another version or that does not parse.
    Unreadable(String),
}

/// The local_state key of `meeting`'s record.
pub(crate) fn record_key(meeting: EntityId) -> LocalKey {
    MeetingRecord::LiveSummaries.key(&[], meeting, &[])
}

/// The stored form of `record`: version (u32 LE), then JSON.
///
/// # Errors
/// [`RuntimeError::Internal`] if the record does not serialise (never expected).
pub fn encode(record: &LiveSummaryRecord) -> Result<Vec<u8>, RuntimeError> {
    let payload = serde_json::to_vec(record)
        .map_err(|_| RuntimeError::Internal("a live summary record does not serialise"))?;
    let mut out = Vec::with_capacity(4 + payload.len());
    out.extend_from_slice(&LIVE_SUMMARY_RECORD_VERSION.to_le_bytes());
    out.extend_from_slice(&payload);
    Ok(out)
}

/// Reads [`encode`]: the record, or why it cannot be used (never an error).
#[must_use]
pub fn decode(bytes: &[u8]) -> StoredLiveSummaries {
    let Some(head) = bytes.get(..4) else {
        return StoredLiveSummaries::Unreadable("the live summary record is truncated".into());
    };
    let version = u32::from_le_bytes([head[0], head[1], head[2], head[3]]);
    if !READABLE_VERSIONS.contains(&version) {
        return StoredLiveSummaries::Unreadable(format!(
            "live summary record version {version}, this build reads \
             {LIVE_SUMMARY_RECORD_VERSION}"
        ));
    }
    match serde_json::from_slice::<LiveSummaryRecord>(&bytes[4..]) {
        Ok(mut record) => {
            // Version 1 never recorded the words, whatever its JSON holds.
            if version == 1 {
                record.vocabulary = None;
            }
            StoredLiveSummaries::Record(Box::new(record))
        }
        Err(e) => StoredLiveSummaries::Unreadable(format!("the live summary record: {e}")),
    }
}

/// Stores `record` for `meeting` (replacing an earlier one).
///
/// # Errors
/// Store errors.
pub fn record_live_summaries(
    store: &mut Store,
    meeting: EntityId,
    record: &LiveSummaryRecord,
) -> Result<(), RuntimeError> {
    crate::local::set(store, &record_key(meeting), &encode(record)?)?;
    Ok(())
}

/// The record of `meeting`, if any.
///
/// # Errors
/// Store errors.
pub fn live_summary_record(
    store: &Store,
    meeting: EntityId,
) -> Result<Option<StoredLiveSummaries>, RuntimeError> {
    Ok(store
        .local_value(record_key(meeting).as_str())?
        .map(|v| decode(&v)))
}

/// What a summary did with the recorded partials.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LiveReuse {
    /// Partials in the record.
    pub recorded: usize,
    /// Re-cited and used for their window.
    pub reused: usize,
    /// Could not be re-cited (their windows were mapped again).
    pub recite_failed: usize,
    /// Re-cited, but the transcript has no window of theirs (dropped).
    pub unplaced: usize,
    /// Why the record was not used at all (another model, window length, word list or version).
    pub not_used: Option<String>,
}

/// `rows` (segments of the meeting of `view`, any pass) as [`recite`] reads them: tracks
/// numbered by their position among the meeting's tracks, speakers as `view` names them
/// ("Me" / "Them" for a segment without a speaker).
#[must_use]
pub(crate) fn timed(
    view: &crate::view::MeetingView,
    rows: &[earshot_core::asr::Segment],
) -> Vec<TimedSegment> {
    rows.iter()
        .map(|s| TimedSegment {
            id: s.id,
            track: view
                .tracks
                .iter()
                .position(|t| t.id == s.track)
                .and_then(|i| u32::try_from(i).ok())
                .unwrap_or(u32::MAX),
            start_ms: s.start.as_millis(),
            end_ms: s.end.as_millis().max(s.start.as_millis()),
            speaker: view.speaker_name(s),
        })
        .collect()
}

/// The window of `range` of `input` with windows of `seconds`.
#[must_use]
pub fn window_of(input: &SummaryInput, range: &Range<usize>, seconds: u32) -> u64 {
    input.segments[range.start].start.as_millis() / (u64::from(seconds) * 1000)
}

/// The recorded partials re-cited onto `fin` (the segments of `input`, the transcript
/// being summarised, cut into `ranges` of `seconds`) from `live` (the live segments they
/// cite), one entry per range: `Some` where a partial is reused (module docs).
#[must_use]
pub fn reusable(
    record: &StoredLiveSummaries,
    model: &str,
    seconds: u32,
    live: &[TimedSegment],
    fin: &[TimedSegment],
    input: &SummaryInput,
    ranges: &[Range<usize>],
) -> (Vec<Option<SummaryV1>>, LiveReuse) {
    let mut done = vec![None; ranges.len()];
    let mut reuse = LiveReuse::default();
    let record = match record {
        StoredLiveSummaries::Record(r) => r,
        StoredLiveSummaries::Unreadable(why) => {
            reuse.not_used = Some(why.clone());
            return (done, reuse);
        }
    };
    reuse.recorded = record.partials.len();
    if record.model != model {
        reuse.not_used = Some(format!(
            "made with {}, this summary uses {model}",
            record.model
        ));
        return (done, reuse);
    }
    if record.seconds != seconds {
        reuse.not_used = Some(format!(
            "made with {} s windows, this summary uses {seconds} s",
            record.seconds
        ));
        return (done, reuse);
    }
    // The partials were mapped with the Dictionary of their meeting's start: a changed list
    // (or one not recorded) could spell names differently from the rest of the summary.
    match &record.vocabulary {
        Some(words) if words.as_slice() == input.vocabulary.words() => {}
        Some(_) => {
            reuse.not_used = Some("made with other Dictionary words than this summary uses".into());
            return (done, reuse);
        }
        None => {
            reuse.not_used = Some("made before Earshot recorded the Dictionary words".into());
            return (done, reuse);
        }
    }
    for p in &record.partials {
        let Ok(r) = recite(&p.summary, live, fin) else {
            reuse.recite_failed += 1;
            continue;
        };
        match ranges
            .iter()
            .position(|range| window_of(input, range, seconds) == p.window)
        {
            Some(j) if done[j].is_none() => {
                done[j] = Some(r.summary);
                reuse.reused += 1;
            }
            _ => reuse.unplaced += 1,
        }
    }
    (done, reuse)
}
