//! Audio retention (T7.4k): how long a meeting's recordings
//! are kept, when that time starts, and the runs that delete them.
//!
//! - The setting is device-local ([`AUDIO_RETENTION_KEY`], one of [`AudioRetention::ALL`],
//!   default 7 days); a stored value this build does not know is reported and the default
//!   used.
//! - The clock: each meeting's `audio-retention/<meeting>` record (format and the rule in
//!   `earshot_store::due_audio`) holds when its time started, from [`Services::clock`]. It is
//!   started once and never restarted, when the final transcript is first stored (in the
//!   final pass's write transaction: the cleanup after the meeting or a re-transcription),
//!   whatever the summary does: a summary is made from the transcript, so a failed, skipped
//!   or cancelled summary keeps no audio. A meeting being recorded, or
//!   whose final pass has not succeeded, has no clock and keeps its audio. A meeting with a
//!   final transcript but no clock (finished before this rule) gets one at the next [`run`].
//! - Only audio files (and their rows) are deleted; transcripts, summaries, speakers and
//!   export records stay.
//! - [`run`] deletes what is due now; a host calls it at start-up and periodically, so
//!   overdue audio goes when the app next runs. Runs are one at a time per process. A meeting
//!   that a job holds (recording, final pass, re-transcription, a speaker sample, an export)
//!   is skipped and goes in a later run; while its audio is deleted the meeting is claimed
//!   like a deletion, so such a job cannot start meanwhile (it is refused retryably). The
//!   claim, like [`Services::delete_meeting`]'s, is per process: a job of another process on
//!   the same data directory (a CLI run) is not seen. A meeting whose deletion fails (a file
//!   in use) is counted and tried again by a later run.
//! - After the wall clock steps forward while the app runs (T14.26), [`run`] deletes a meeting
//!   that only the step made due one run later, when that run still finds it due; the memory of
//!   the previous run is per process, so the first run after a start trusts the clock.
//!   [`run_choice`] (a saved setting) applies at once.
//! - [`preview`] counts the meetings whose recordings a setting would delete now, with the
//!   plan of the run (meetings a job holds counted apart).

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Mutex, PoisonError};

use earshot_core::identity::EntityId;
use earshot_core::model::{AudioFile, AudioFileRepo, TranscriptPass, live_segments};
use earshot_store::{ORPHAN_GRACE_MS, RetentionClock, RetentionPolicy, Store, StoreError, StoreTx};

use crate::RuntimeError;
use crate::local::UnknownSetting;
use crate::local::{DeviceRecord, LocalKey, MeetingRecord};
use crate::services::Services;

/// The `local_state` key of the setting.
pub const AUDIO_RETENTION_KEY: &str = "settings/audio-retention";

/// How long recordings are kept after a meeting's clock starts (module docs).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AudioRetention {
    /// 1 hour.
    OneHour,
    /// 4 hours.
    FourHours,
    /// 12 hours.
    TwelveHours,
    /// 1 day.
    OneDay,
    /// 3 days.
    ThreeDays,
    /// 7 days (default).
    #[default]
    SevenDays,
    /// 14 days.
    FourteenDays,
    /// 28 days.
    TwentyEightDays,
    /// 3 months (90 days).
    ThreeMonths,
    /// 6 months (180 days).
    SixMonths,
    /// 1 year (365 days).
    OneYear,
    /// Never deleted.
    Forever,
}

impl AudioRetention {
    /// Every choice, shortest first (the order the settings page lists them).
    pub const ALL: [Self; 12] = [
        Self::OneHour,
        Self::FourHours,
        Self::TwelveHours,
        Self::OneDay,
        Self::ThreeDays,
        Self::SevenDays,
        Self::FourteenDays,
        Self::TwentyEightDays,
        Self::ThreeMonths,
        Self::SixMonths,
        Self::OneYear,
        Self::Forever,
    ];

    /// The stored name (`1h`, `4h`, `12h`, `1d`, ..., `365d`, `forever`).
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::OneHour => "1h",
            Self::FourHours => "4h",
            Self::TwelveHours => "12h",
            Self::OneDay => "1d",
            Self::ThreeDays => "3d",
            Self::SevenDays => "7d",
            Self::FourteenDays => "14d",
            Self::TwentyEightDays => "28d",
            Self::ThreeMonths => "90d",
            Self::SixMonths => "180d",
            Self::OneYear => "365d",
            Self::Forever => "forever",
        }
    }

    /// Reads [`AudioRetention::as_str`].
    #[must_use]
    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|r| r.as_str() == name)
    }

    /// How the user sees it ("1 hour", "3 months", "Forever").
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::OneHour => "1 hour",
            Self::FourHours => "4 hours",
            Self::TwelveHours => "12 hours",
            Self::OneDay => "1 day",
            Self::ThreeDays => "3 days",
            Self::SevenDays => "7 days",
            Self::FourteenDays => "14 days",
            Self::TwentyEightDays => "28 days",
            Self::ThreeMonths => "3 months",
            Self::SixMonths => "6 months",
            Self::OneYear => "1 year",
            Self::Forever => "Forever",
        }
    }

    /// The store's policy for this choice.
    #[must_use]
    pub fn policy(self) -> RetentionPolicy {
        match self {
            Self::OneHour => RetentionPolicy::hours(1),
            Self::FourHours => RetentionPolicy::hours(4),
            Self::TwelveHours => RetentionPolicy::hours(12),
            Self::OneDay => RetentionPolicy::days(1),
            Self::ThreeDays => RetentionPolicy::days(3),
            Self::SevenDays => RetentionPolicy::days(7),
            Self::FourteenDays => RetentionPolicy::days(14),
            Self::TwentyEightDays => RetentionPolicy::days(28),
            Self::ThreeMonths => RetentionPolicy::days(90),
            Self::SixMonths => RetentionPolicy::days(180),
            Self::OneYear => RetentionPolicy::days(365),
            Self::Forever => RetentionPolicy::KeepForever,
        }
    }
}

/// The stored setting (absent: the default) and, when the stored value is not known to this
/// build, what was stored (the default is used).
///
/// # Errors
/// Store errors.
pub fn audio_retention(
    store: &Store,
) -> Result<(AudioRetention, Option<UnknownSetting>), RuntimeError> {
    Ok(
        crate::local::read_setting(store, AUDIO_RETENTION_KEY, |text| {
            AudioRetention::parse(&text).ok_or(text)
        })?
        .or_default_with(AudioRetention::default),
    )
}

/// Stores the setting (replacing an earlier one). Deletes nothing: [`run`] applies it.
///
/// # Errors
/// Store errors.
pub fn set_audio_retention(store: &mut Store, choice: AudioRetention) -> Result<(), RuntimeError> {
    crate::local::set(
        store,
        &DeviceRecord::AudioRetention.key(),
        choice.as_str().as_bytes(),
    )?;
    Ok(())
}

/// The registry key of `meeting`'s retention clock (`earshot_store::retention_clock_key`).
pub(crate) fn clock_key(meeting: EntityId) -> LocalKey {
    MeetingRecord::AudioRetention.key(&[], meeting, &[])
}

/// Starts `meeting`'s retention clock at `now_unix_ms` inside `tx`, unless it has a clock
/// already (started earlier, or a record a newer version wrote, which is left as it is).
///
/// # Errors
/// Store errors.
pub(crate) fn start_clock_tx(
    tx: &mut StoreTx<'_>,
    meeting: EntityId,
    now_unix_ms: u64,
) -> Result<(), StoreError> {
    let key = clock_key(meeting);
    if tx.local_value(key.as_str())?.is_none() {
        crate::local::set_tx(
            tx,
            &key,
            &earshot_store::encode_retention_clock(now_unix_ms),
        )?;
    }
    Ok(())
}

/// Starts the clock at `now` for every meeting with audio and a final transcript but no
/// clock: meetings whose final pass ended before the clock started with the transcript
/// (2026-11-05). Returns how many were started.
///
/// # Errors
/// Store errors.
fn start_missing_clocks(services: &Services, now: u64) -> Result<usize, RuntimeError> {
    let mut db = services.store();
    let meetings: BTreeSet<EntityId> = db
        .all_audio_files()?
        .into_iter()
        .map(|f| f.meeting_id)
        .collect();
    let mut started = 0;
    for meeting in meetings {
        if earshot_store::retention_clock(&db, meeting)? != RetentionClock::NotStarted
            || live_segments(&**db, meeting, TranscriptPass::Final)?.is_empty()
        {
            continue;
        }
        db.transaction(|tx| start_clock_tx(tx, meeting, now))?;
        started += 1;
    }
    Ok(started)
}

/// The meetings `choice` deletes recordings of now, split into those a run can take and
/// those a job holds (skipped until a later run): the one plan of [`preview`] and
/// [`run_choice`].
struct Plan {
    /// Due and not held: their audio rows.
    ready: BTreeMap<EntityId, Vec<AudioFile>>,
    /// Due but held by a job of this process.
    busy: usize,
    /// Meetings with audio whose clock could not be read (kept).
    unreadable_clocks: usize,
}

fn plan(services: &Services, choice: AudioRetention, now: u64) -> Result<Plan, RuntimeError> {
    // The activity lock, then the store lock, never nested.
    let held = services.leased_meetings();
    let due = earshot_store::due_audio(&services.store(), choice.policy(), now)?;
    let (ready, busy): (BTreeMap<_, _>, BTreeMap<_, _>) = due
        .meetings
        .into_iter()
        .partition(|(m, _)| !held.contains(m));
    Ok(Plan {
        ready,
        busy: busy.len(),
        unreadable_clocks: due.unreadable_clocks,
    })
}

/// What [`preview`] counts.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RetentionPreview {
    /// Meetings whose recordings a run would delete now.
    pub meetings: usize,
    /// Meetings that are due but held by a job now (deleted by a later run).
    pub busy: usize,
}

/// How many meetings' recordings `choice` would delete now, with the plan of [`run_choice`]
/// (meetings a job holds counted apart). Deletes nothing.
///
/// # Errors
/// Store errors.
pub fn preview(
    services: &Services,
    choice: AudioRetention,
) -> Result<RetentionPreview, RuntimeError> {
    let p = plan(services, choice, services.clock.now_unix_ms())?;
    Ok(RetentionPreview {
        meetings: p.ready.len(),
        busy: p.busy,
    })
}

/// What a retention run did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RetentionRun {
    /// The setting the run applied.
    pub choice: AudioRetention,
    /// A stored setting this build does not know (the default was applied).
    pub unknown_setting: Option<UnknownSetting>,
    /// Meetings whose recordings were deleted.
    pub meetings: usize,
    /// Audio files deleted.
    pub deleted_files: usize,
    /// Rows whose file was already gone (removed).
    pub missing_files: usize,
    /// Due meetings skipped because a job holds them (they go in a later run).
    pub busy: usize,
    /// Meetings due only because the wall clock stepped forward since the previous
    /// unattended run (T14.26): kept until the next run agrees they are due.
    pub waiting: usize,
    /// Due meetings whose recordings could not all be deleted (a file in use, a store
    /// error); what could be deleted is gone, the rest is tried again by a later run.
    pub failed: usize,
    /// Meetings with audio whose clock could not be read (kept).
    pub unreadable_clocks: usize,
    /// Audio files without a row older than `ORPHAN_GRACE_MS`, removed.
    pub orphans_removed: usize,
    /// The sweep of such files failed (after the deletions counted above); a later run
    /// tries again.
    pub sweep_failed: bool,
    /// Meetings with a final transcript but no clock (finished before the clock started
    /// with the transcript) whose clock this run started.
    pub clocks_started: usize,
}

impl RetentionRun {
    /// Some due recordings are still there (held by a job, waiting for a second run after a
    /// clock step, or their deletion failed).
    #[must_use]
    pub fn some_left(&self) -> bool {
        self.busy > 0 || self.waiting > 0 || self.failed > 0 || self.sweep_failed
    }
}

/// One run at a time in this process.
static RUN_LOCK: Mutex<()> = Mutex::new(());

/// How far the wall clock may run ahead of the time that passed between two runs before
/// [`run`] counts it as a step forward (T14.26): above the slewing and small corrections a
/// time service makes, far below a run's period. A judgement.
pub const CLOCK_STEP_TOLERANCE_MS: u64 = 60_000;

/// What an unattended run ([`run`]) remembers of the run before it, in memory only: a
/// restart starts again with no previous run.
#[derive(Debug, Clone, PartialEq, Eq)]
struct LastRun {
    /// The wall clock then (Unix ms).
    wall_ms: u64,
    /// The time since this process's anchor then, which no clock step moves.
    since_start: std::time::Duration,
    /// The setting it applied.
    choice: AudioRetention,
    /// The meetings that were due then.
    due: BTreeSet<EntityId>,
}

/// The unattended runs' memory, per data folder (one in the app; one per test).
static LAST_RUNS: Mutex<BTreeMap<std::path::PathBuf, LastRun>> = Mutex::new(BTreeMap::new());

/// The anchor of [`LastRun::since_start`]. `Instant` counts time asleep on Windows, as the wall
/// clock does, so a sleep is not a step there; where it does not (Linux), a run after a
/// suspend waits one period.
fn since_start() -> std::time::Duration {
    static ANCHOR: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
    ANCHOR.get_or_init(std::time::Instant::now).elapsed()
}

/// The wall-clock time a run at `wall_ms` should trust (T14.26): `wall_ms` itself, unless the
/// wall clock ran more than [`CLOCK_STEP_TOLERANCE_MS`] ahead of the time that passed since
/// `last` (a step forward), then the last run's wall clock plus that time. `None` when there is
/// no step to distrust: the first run, or a clock that kept time.
fn stepped_from(
    last: Option<&LastRun>,
    wall_ms: u64,
    since_start: std::time::Duration,
) -> Option<u64> {
    let last = last?;
    let passed =
        u64::try_from(since_start.saturating_sub(last.since_start).as_millis()).unwrap_or(u64::MAX);
    let trusted = last.wall_ms.saturating_add(passed);
    (wall_ms.saturating_sub(trusted) > CLOCK_STEP_TOLERANCE_MS).then_some(trusted)
}

/// Deletes the recordings the stored setting makes due now (module docs), as the app does at
/// start-up and then periodically. After the wall clock stepped forward since the previous run
/// of this process (T14.26), a meeting that only the step made due is not deleted by this run:
/// it waits ([`RetentionRun::waiting`]) for the next run to agree that it is due, so a clock set
/// wrong for a moment deletes nothing. Meetings the previous run also found due, or that are
/// due by the time that truly passed, go as before. The first run of a process trusts the clock.
///
/// # Errors
/// Store errors reading the setting or the plan. A meeting whose deletion fails is counted
/// in [`RetentionRun::failed`] and the run goes on; a failed orphan sweep is
/// [`RetentionRun::sweep_failed`], with the deletions before it still counted.
pub fn run(services: &Services) -> Result<RetentionRun, RuntimeError> {
    let (choice, unknown) = audio_retention(&services.store())?;
    let mut out = run_with(services, choice, Some(since_start()))?;
    out.unknown_setting = unknown;
    Ok(out)
}

/// [`run`] with `choice` instead of the stored setting, applied at once: the run after the user
/// saves a setting, whose [`preview`] they were shown. It neither waits after a clock step nor
/// counts as an unattended run.
///
/// # Errors
/// As [`run`].
pub fn run_choice(
    services: &Services,
    choice: AudioRetention,
) -> Result<RetentionRun, RuntimeError> {
    run_with(services, choice, None)
}

/// A run of `choice`; with `unattended` (the time since the process anchor), one of [`run`]'s,
/// judged against the previous one (T14.26).
fn run_with(
    services: &Services,
    choice: AudioRetention,
    unattended: Option<std::time::Duration>,
) -> Result<RetentionRun, RuntimeError> {
    let _one = RUN_LOCK.lock().unwrap_or_else(PoisonError::into_inner);
    let now = services.clock.now_unix_ms();
    let clocks_started = start_missing_clocks(services, now)?;
    let mut plan = plan(services, choice, now)?;
    let mut waiting = 0;
    if let Some(at) = unattended {
        let root = services.data.root().to_path_buf();
        let mut last_runs = LAST_RUNS.lock().unwrap_or_else(PoisonError::into_inner);
        let last = last_runs.get(&root);
        let due_now: BTreeSet<EntityId> =
            earshot_store::due_audio(&services.store(), choice.policy(), now)?
                .meetings
                .into_keys()
                .collect();
        if let Some(trusted) = stepped_from(last, now, at) {
            let agreed = last
                .filter(|l| l.choice == choice)
                .map(|l| l.due.clone())
                .unwrap_or_default();
            let truly_due: BTreeSet<EntityId> =
                earshot_store::due_audio(&services.store(), choice.policy(), trusted)?
                    .meetings
                    .into_keys()
                    .collect();
            let before = plan.ready.len();
            plan.ready
                .retain(|m, _| agreed.contains(m) || truly_due.contains(m));
            waiting = before - plan.ready.len();
        }
        last_runs.insert(
            root,
            LastRun {
                wall_ms: now,
                since_start: at,
                choice,
                due: due_now,
            },
        );
    }
    let mut out = RetentionRun {
        choice,
        clocks_started,
        busy: plan.busy,
        waiting,
        unreadable_clocks: plan.unreadable_clocks,
        ..RetentionRun::default()
    };
    for (meeting, files) in plan.ready {
        // Claimed like a deletion: refused while a job holds the meeting (one may have
        // started since the plan), and no job can start on it until the claim is dropped
        // (activity lock first, then the store's).
        let Ok(_claim) = services.begin_deletion(meeting) else {
            out.busy += 1;
            continue;
        };
        match remove_meeting_audio(services, meeting, files) {
            Ok(removed) => {
                out.deleted_files += removed.deleted_files;
                out.missing_files += removed.missing_files;
                if removed.rows_removed > 0 {
                    out.meetings += 1;
                }
            }
            Err(_) => out.failed += 1,
        }
    }
    match earshot_store::sweep_orphan_audio(&services.store(), &services.data, now, ORPHAN_GRACE_MS)
    {
        Ok(n) => out.orphans_removed = n,
        Err(_) => out.sweep_failed = true,
    }
    Ok(out)
}

/// Removes `files` of `meeting` that still have rows (a deletion may have run since the
/// plan).
fn remove_meeting_audio(
    services: &Services,
    meeting: EntityId,
    files: Vec<AudioFile>,
) -> Result<earshot_store::RemovedAudio, RuntimeError> {
    let mut db = services.store();
    let rows = db.audio_files(meeting)?;
    let files: Vec<_> = files.into_iter().filter(|f| rows.contains(f)).collect();
    Ok(earshot_store::remove_audio(
        &mut db,
        &services.data,
        &files,
    )?)
}

#[cfg(test)]
#[path = "retention_tests.rs"]
mod tests;
