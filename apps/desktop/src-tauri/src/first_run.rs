//! The first-run wizard's state (T9.1d). The record and the rule for showing the wizard are
//! `earshot_runtime::first_run`; this module only maps them to the IPC types. Everything the
//! wizard's steps do goes through the Settings pages' own commands.

use earshot_runtime::Services;
use earshot_runtime::first_run::{self, FirstRunState};
use serde::Serialize;
use specta::Type;

use crate::ipc::IpcError;

/// What the wizard needs to know.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Type)]
pub struct FirstRunView {
    /// The app opens on the wizard.
    pub show: bool,
}

impl From<FirstRunState> for FirstRunView {
    fn from(s: FirstRunState) -> Self {
        Self { show: s.show }
    }
}

/// The wizard's state in `services`' store.
///
/// # Errors
/// Store errors.
pub fn state_view(services: &Services) -> Result<FirstRunView, IpcError> {
    Ok(first_run::first_run_state(&services.store())?.into())
}

/// Marks the wizard done.
///
/// # Errors
/// Store errors.
pub fn finish(services: &Services) -> Result<FirstRunView, IpcError> {
    Ok(first_run::finish_first_run(&mut services.store())?.into())
}

/// Whether the app opens on the first-run wizard.
#[tauri::command]
#[specta::specta]
pub async fn first_run_state(
    state: tauri::State<'_, crate::ipc::AppState>,
) -> Result<FirstRunView, IpcError> {
    let services = state.services()?;
    crate::ipc::blocking(move || state_view(&services)).await
}

/// Marks the first-run wizard done, whichever steps were skipped.
#[tauri::command]
#[specta::specta]
pub async fn finish_first_run(
    state: tauri::State<'_, crate::ipc::AppState>,
) -> Result<FirstRunView, IpcError> {
    let services = state.services()?;
    crate::ipc::blocking(move || finish(&services)).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{TempDir, services};

    #[test]
    fn first_run_view_follows_the_record_through_the_ipc_types() {
        let dir = TempDir::new("first-run");
        let svc = services(&dir);
        assert_eq!(state_view(&svc).unwrap(), FirstRunView { show: true });
        let v = finish(&svc).unwrap();
        assert!(!v.show);
        assert_eq!(state_view(&svc).unwrap(), v);
    }
}
