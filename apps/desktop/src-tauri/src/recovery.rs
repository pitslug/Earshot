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

/// Runs `recover` (the app: [`recover_interrupted`]; a panic in it is caught, so retention
/// still starts), tells `recovered` when it gave a meeting its audio back, then runs
/// `retention` (which never returns in the app). Recovery must come first: retention's orphan
/// sweep is the one thing that deletes unfinished audio.
pub fn recover_then(
    services: &Services,
    recover: impl FnOnce(&Services) -> RecoveryReport,
    recovered: impl FnOnce(&RecoveryReport),
    retention: impl FnOnce(&Services),
) {
    if let Ok(report) = catch_unwind(AssertUnwindSafe(|| recover(services)))
        && !report.recovered.is_empty()
    {
        recovered(&report);
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
            recover_then(&services, recover_interrupted, recovered, |services| {
                retention_loop(services, &status, earshot_runtime::retention::run, || {
                    std::thread::sleep(period);
                    true
                });
            });
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
