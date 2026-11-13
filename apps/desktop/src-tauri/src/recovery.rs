//! Start-up recovery of interrupted recordings (T9.18) as the app runs it: once, on its own
//! thread, before audio retention's first run, and the notices the window shows once. The
//! rules live in `earshot_runtime::recover`; this module only orders the start and maps the
//! notices to the IPC types.

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::time::Duration;

use earshot_runtime::Services;
use earshot_runtime::recover::{RecoveryReport, recover_interrupted, take_notices};
use serde::Serialize;
use specta::Type;
use tauri::State;

use crate::ipc::{AppState, IpcError, blocking};
use crate::storage_settings::{RetentionStatus, retention_loop};

#[cfg(test)]
#[path = "recovery_tests.rs"]
mod tests;

/// A recording Earshot was closed during, whose audio start-up recovery saved.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Type)]
pub struct RecoveredRecording {
    /// The meeting's id.
    pub meeting: String,
    /// How much was saved, in ms (well below 2^53).
    pub saved_ms: u64,
}

/// What the storage settings say when recovery could not finish a recording it found (no paths,
/// no error text: the audio stays on disk for the next start).
pub const RECOVERY_FAILED: &str = "Earshot could not finish saving a recording it was closed \
     during. Its audio stays on disk, and Earshot tries again at its next start.";

/// What the storage settings say when recovery could not look for recordings at all.
pub const RECOVERY_DID_NOT_RUN: &str = "Earshot could not check for recordings it was closed \
     during. It tries again at its next start.";

/// The problem a recovery leaves (`None`: a panic), if any.
fn problem_of(report: Option<&RecoveryReport>) -> Option<&'static str> {
    match report {
        None => Some(RECOVERY_DID_NOT_RUN),
        Some(r) if r.list_failed.is_some() => Some(RECOVERY_DID_NOT_RUN),
        Some(r) if !r.failed.is_empty() => Some(RECOVERY_FAILED),
        Some(_) => None,
    }
}

/// Runs `recover` (the app: [`recover_interrupted`]; a panic in it is caught, so retention
/// still starts), tells `recovered` when it gave a meeting its audio back and `problem` (fixed
/// copy) when it failed or could not run, then runs `retention` (which never returns in the
/// app). Recovery must come first: retention's orphan sweep is the one thing that deletes
/// unfinished audio.
pub fn recover_then(
    services: &Services,
    recover: impl FnOnce(&Services) -> RecoveryReport,
    recovered: impl FnOnce(&RecoveryReport),
    problem: impl FnOnce(&'static str),
    retention: impl FnOnce(&Services),
) {
    let report = catch_unwind(AssertUnwindSafe(|| recover(services))).ok();
    if let Some(report) = &report
        && !report.recovered.is_empty()
    {
        recovered(report);
    }
    if let Some(p) = problem_of(report.as_ref()) {
        problem(p);
    }
    retention(services);
}

/// [`recover_then`] with the app's retention loop, on a thread of its own: at once, then
/// retention every `period` while the app runs.
pub fn spawn(
    services: Services,
    status: RetentionStatus,
    period: Duration,
    recovered: impl FnOnce(&RecoveryReport) + Send + 'static,
) {
    let _ = std::thread::Builder::new()
        .name("earshot-recovery".into())
        .spawn(move || {
            recover_then(
                &services,
                recover_interrupted,
                recovered,
                |p| status.set_recovery(p),
                |services| {
                    retention_loop(services, &status, earshot_runtime::retention::run, || {
                        std::thread::sleep(period);
                        true
                    });
                },
            );
        });
}

/// The recordings recovered since the window last asked, each returned once.
///
/// # Errors
/// Store errors.
pub fn recovered(services: &Services) -> Result<Vec<RecoveredRecording>, IpcError> {
    Ok(take_notices(services)?
        .into_iter()
        .map(|n| RecoveredRecording {
            meeting: n.meeting.to_string(),
            saved_ms: n.saved_ms,
        })
        .collect())
}

/// The recordings recovered at this start (or an earlier one whose window never asked), each
/// returned once.
#[tauri::command]
#[specta::specta]
pub async fn recovered_recordings(
    state: State<'_, AppState>,
) -> Result<Vec<RecoveredRecording>, IpcError> {
    let services = state.services()?;
    blocking(move || recovered(&services)).await
}
