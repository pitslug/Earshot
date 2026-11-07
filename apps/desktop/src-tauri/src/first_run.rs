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
    /// The optional models the user said "never" to.
    pub declined_models: Vec<String>,
}

impl From<FirstRunState> for FirstRunView {
    fn from(s: FirstRunState) -> Self {
        Self {
            show: s.show,
            declined_models: s.declined_models,
        }
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

/// Records or clears "never" for `model`.
///
/// # Errors
/// `unknown_model` (nothing stored); store errors.
pub fn set_declined(
    services: &Services,
    model: &str,
    declined: bool,
) -> Result<FirstRunView, IpcError> {
    crate::model_jobs::require_catalogue_model(model)?;
    Ok(first_run::set_model_declined(&mut services.store(), model, declined)?.into())
}

/// Whether the app opens on the first-run wizard, and its "never" answers.
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

/// Records or clears the wizard's "never" for an optional model (`unknown_model`: nothing
/// stored). It stops no download and removes no file.
#[tauri::command]
#[specta::specta]
pub async fn set_model_declined(
    state: tauri::State<'_, crate::ipc::AppState>,
    model: String,
    declined: bool,
) -> Result<FirstRunView, IpcError> {
    let services = state.services()?;
    crate::ipc::blocking(move || set_declined(&services, &model, declined)).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{TempDir, services};

    #[test]
    fn first_run_view_follows_the_record_through_the_ipc_types() {
        let dir = TempDir::new("first-run");
        let svc = services(&dir);
        assert_eq!(
            state_view(&svc).unwrap(),
            FirstRunView {
                show: true,
                declined_models: Vec::new(),
            }
        );
        let v = set_declined(&svc, "tiny.en", true).unwrap();
        assert!(v.show);
        assert_eq!(v.declined_models, ["tiny.en"]);
        let v = finish(&svc).unwrap();
        assert!(!v.show);
        assert_eq!(v.declined_models, ["tiny.en"]);
        assert_eq!(state_view(&svc).unwrap(), v);
        assert!(
            set_declined(&svc, "tiny.en", false)
                .unwrap()
                .declined_models
                .is_empty()
        );
    }

    #[test]
    fn first_run_refuses_a_model_the_catalogue_does_not_have() {
        let dir = TempDir::new("first-run");
        let svc = services(&dir);
        let e = set_declined(&svc, "no-such-model", true).unwrap_err();
        assert_eq!(e.kind, "unknown_model");
        assert!(state_view(&svc).unwrap().declined_models.is_empty());
    }
}
