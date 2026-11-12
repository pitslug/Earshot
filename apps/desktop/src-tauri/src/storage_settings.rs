//! The Storage page of Settings (T7.4k): how long recordings are kept after a meeting's
//! summary is stored. The choices, the rule and the runs are `earshot_runtime::retention`;
//! this module maps them to the IPC types and holds the loop that runs retention at start-up
//! and periodically ([`retention_loop`], started after start-up recovery by
//! `crate::recovery::spawn`), keeping the last automatic run's problem for the page
//! ([`RetentionStatus`]).

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use earshot_runtime::retention::{
    AudioRetention, RetentionRun, audio_retention, preview, run_choice, set_audio_retention,
};
use earshot_runtime::{RuntimeError, Services};
use serde::Serialize;
use specta::Type;

use crate::ipc::IpcError;

/// How often retention runs while the app is open (the shortest choice is 1 hour).
pub const RETENTION_PERIOD: Duration = Duration::from_secs(15 * 60);

/// What the page says when some due recordings could not be deleted (no paths, no error
/// text: a file in use, or a recording a job is using).
pub const SOME_LEFT_MESSAGE: &str =
    "Some recordings could not be deleted now. Earshot tries again later.";

/// What the page says when an automatic run stopped unexpectedly.
pub const RUN_STOPPED_MESSAGE: &str =
    "Deleting old recordings stopped unexpectedly. Earshot tries again later.";

/// One choice of the list.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Type)]
pub struct RetentionOption {
    /// The stored name (`1h`, `7d`, `forever`, ...), sent back to choose it.
    pub value: String,
    /// What the user sees ("1 hour", "3 months", "Forever").
    pub label: String,
    /// How long recordings are kept, in ms (`null`: forever).
    pub keep_ms: Option<u64>,
}

/// Every choice, shortest first, as the app sends them.
#[must_use]
pub fn retention_options() -> Vec<RetentionOption> {
    AudioRetention::ALL
        .iter()
        .map(|r| RetentionOption {
            value: r.as_str().to_owned(),
            label: r.label().to_owned(),
            keep_ms: match r.policy() {
                earshot_store::RetentionPolicy::KeepFor { ms } => Some(ms),
                earshot_store::RetentionPolicy::KeepForever => None,
            },
        })
        .collect()
}

/// The Storage page.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Type)]
pub struct StorageView {
    /// The choice in use (the default when nothing or an unknown value is stored).
    pub retention: String,
    /// Every choice, shortest first.
    pub options: Vec<RetentionOption>,
    /// One line per stored value this version does not know.
    pub unknown: Vec<String>,
    /// What went wrong in the last automatic run (`null` when it went well).
    pub problem: Option<String>,
}

/// The last automatic run's problem, shared by the retention thread and the page.
#[derive(Debug, Clone, Default)]
pub struct RetentionStatus(Arc<Mutex<Option<String>>>);

impl RetentionStatus {
    /// The last problem.
    #[must_use]
    pub fn problem(&self) -> Option<String> {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    fn set(&self, problem: Option<&str>) {
        *self.0.lock().unwrap_or_else(PoisonError::into_inner) = problem.map(str::to_owned);
    }
}

fn view(choice: AudioRetention, unknown: Vec<String>, status: &RetentionStatus) -> StorageView {
    StorageView {
        retention: choice.as_str().to_owned(),
        options: retention_options(),
        unknown,
        problem: status.problem(),
    }
}

fn choice_of(value: &str) -> Result<AudioRetention, IpcError> {
    AudioRetention::parse(value).ok_or_else(|| IpcError {
        kind: "invalid_setting".into(),
        message: "This is not one of the choices.".into(),
        retryable: false,
    })
}

/// The stored setting.
///
/// # Errors
/// Store errors.
pub fn storage_view(
    services: &Services,
    status: &RetentionStatus,
) -> Result<StorageView, IpcError> {
    let (choice, unknown) = audio_retention(&services.store())?;
    Ok(view(
        choice,
        unknown.iter().map(ToString::to_string).collect(),
        status,
    ))
}

/// What a choice would delete ([`retention_preview`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Type)]
pub struct RetentionCount {
    /// Meetings whose recordings would be deleted now.
    pub meetings: u32,
    /// Meetings whose recordings are due but in use now (deleted once they are not).
    pub in_use: u32,
}

fn count(n: usize) -> u32 {
    u32::try_from(n).unwrap_or(u32::MAX)
}

/// How many meetings' recordings `value` would delete (nothing is deleted).
///
/// # Errors
/// `invalid_setting` for a value that is not a choice; store errors.
pub fn retention_preview(services: &Services, value: &str) -> Result<RetentionCount, IpcError> {
    let p = preview(services, choice_of(value)?)?;
    Ok(RetentionCount {
        meetings: count(p.meetings),
        in_use: count(p.busy),
    })
}

/// What storing a choice did.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Type)]
pub struct StorageSaved {
    /// The settings as stored.
    pub view: StorageView,
    /// Meetings whose recordings were deleted now.
    pub deleted: u32,
    /// Set when some due recordings could not be deleted now ([`SOME_LEFT_MESSAGE`]); a
    /// later run tries again.
    pub some_left: Option<String>,
}

/// Stores `value`, then applies it to existing meetings at once (the page asks first when
/// [`retention_preview`] counts any). Once stored, the result always says so: a run that
/// fails or leaves recordings is reported in [`StorageSaved::some_left`], never as an error.
///
/// # Errors
/// `invalid_setting` or a store error storing the choice (nothing stored).
pub fn set_storage(
    services: &Services,
    status: &RetentionStatus,
    value: &str,
) -> Result<StorageSaved, IpcError> {
    let choice = choice_of(value)?;
    set_audio_retention(&mut services.store(), choice)?;
    let (deleted, left) = match run_choice(services, choice) {
        Ok(run) => (count(run.meetings), run.some_left()),
        Err(_) => (0, true),
    };
    Ok(StorageSaved {
        view: view(choice, Vec::new(), status),
        deleted,
        some_left: left.then(|| SOME_LEFT_MESSAGE.to_owned()),
    })
}

/// Runs `run` once, then again each time `wait` returns true; stops when it returns false.
/// Each run's problem goes to `status` ([`SOME_LEFT_MESSAGE`] for a failed run or a
/// meeting it could not delete, [`RUN_STOPPED_MESSAGE`] for a panic, which does not end the
/// loop); a run that went well clears it.
pub fn retention_loop(
    services: &Services,
    status: &RetentionStatus,
    run: impl Fn(&Services) -> Result<RetentionRun, RuntimeError>,
    mut wait: impl FnMut() -> bool,
) {
    loop {
        match catch_unwind(AssertUnwindSafe(|| run(services))) {
            Ok(Ok(r)) if r.failed == 0 && !r.sweep_failed => status.set(None),
            Ok(_) => status.set(Some(SOME_LEFT_MESSAGE)),
            Err(_) => status.set(Some(RUN_STOPPED_MESSAGE)),
        }
        if !wait() {
            return;
        }
    }
}

/// The Storage page settings.
#[tauri::command]
#[specta::specta]
pub async fn storage_settings(
    state: tauri::State<'_, crate::ipc::AppState>,
    status: tauri::State<'_, RetentionStatus>,
) -> Result<StorageView, IpcError> {
    let services = state.services()?;
    let status = status.inner().clone();
    crate::ipc::blocking(move || storage_view(&services, &status)).await
}

/// How many meetings' recordings `retention` would delete (`invalid_setting` for a value
/// that is not a choice). Deletes nothing.
#[tauri::command]
#[specta::specta]
pub async fn preview_retention(
    state: tauri::State<'_, crate::ipc::AppState>,
    retention: String,
) -> Result<RetentionCount, IpcError> {
    let services = state.services()?;
    crate::ipc::blocking(move || retention_preview(&services, &retention)).await
}

/// Stores `retention` and deletes the recordings it makes due now (`invalid_setting`: nothing
/// stored; once stored, problems deleting are in the result).
#[tauri::command]
#[specta::specta]
pub async fn set_storage_settings(
    state: tauri::State<'_, crate::ipc::AppState>,
    status: tauri::State<'_, RetentionStatus>,
    retention: String,
) -> Result<StorageSaved, IpcError> {
    let services = state.services()?;
    let status = status.inner().clone();
    crate::ipc::blocking(move || set_storage(&services, &status, &retention)).await
}

#[cfg(test)]
#[path = "storage_settings_tests.rs"]
pub(crate) mod tests;
