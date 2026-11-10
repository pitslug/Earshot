//! A meeting kept on this computer (T12.13) as the meeting detail shows it, and the one way
//! back to the summary writer (local to remote, never the other way). The rule and the record
//! live in `earshot_runtime::keep_local`; this module only maps them to the IPC types.

use earshot_runtime::Services;
use earshot_runtime::keep_local::{SummaryPrivacy, allow_writer, summary_privacy};
use earshot_runtime::summary_settings::{SummaryWriter, summary_settings};
use earshot_store::Store;
use serde::Serialize;
use specta::Type;
use tauri::State;

use crate::detail::parse_id;
use crate::ipc::{AppState, IpcError, blocking};

#[cfg(test)]
#[path = "keep_local_tests.rs"]
mod tests;

/// A meeting whose summary is kept on this computer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Type)]
pub struct KeptLocalView {
    /// The writer the Summaries page names when it would send the meeting elsewhere ("Claude
    /// Code", "Codex", "your own server"): what "Allow" lets summarise it. `None` while the
    /// writer is this computer or no summary, when allowing would change nothing today.
    pub writer: Option<String>,
}

/// The name the detail gives a writer that sends text off this computer, `None` for the others.
#[must_use]
pub fn remote_writer_name(writer: SummaryWriter) -> Option<&'static str> {
    match writer {
        SummaryWriter::ClaudeCode => Some("Claude Code"),
        SummaryWriter::Codex => Some("Codex"),
        SummaryWriter::OwnEndpoint => Some("your own server"),
        SummaryWriter::OnThisComputer | SummaryWriter::NoSummary => None,
    }
}

/// Whether `meeting` is kept on this computer, read under the caller's store lock. A record
/// that cannot be read counts as kept (`summary_privacy`); unreadable settings name no writer.
#[must_use]
pub fn kept_local_view(
    db: &Store,
    meeting: earshot_core::identity::EntityId,
) -> Option<KeptLocalView> {
    if summary_privacy(db, meeting) == SummaryPrivacy::Writer {
        return None;
    }
    let writer = summary_settings(db)
        .ok()
        .and_then(|(s, _)| remote_writer_name(s.writer))
        .map(str::to_owned);
    Some(KeptLocalView { writer })
}

/// Lets the summary writer summarise `meeting` from now on. Sends nothing: the next summary
/// does, when the user asks for it.
///
/// # Errors
/// An unknown meeting (`detail::edit_error`); store errors.
pub fn allow(services: &Services, meeting: &str) -> Result<(), IpcError> {
    let id = parse_id(meeting)?;
    allow_writer(services, id).map_err(|e| match e {
        earshot_runtime::RuntimeError::Edit(e) => crate::detail::edit_error(&e),
        e => IpcError::from(e),
    })
}

/// "Allow": the meeting kept on this computer may be summarised by the summary writer from now
/// on (the UI asks first). Nothing is sent until the user re-summarises.
#[tauri::command]
#[specta::specta]
pub async fn allow_summary_writer(
    state: State<'_, AppState>,
    meeting: String,
) -> Result<(), IpcError> {
    let services = state.services()?;
    blocking(move || allow(&services, &meeting)).await
}
