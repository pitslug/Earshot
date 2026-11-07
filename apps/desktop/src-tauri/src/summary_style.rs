//! The "Summary style" section of the Summaries page (T7.4b): the user's own words about
//! tone, length, emphasis and jargon. The setting and its rules are
//! `earshot_runtime::summary_style` (and `earshot_core::prompt::SummaryStyle`); this module
//! only maps them to the IPC types.

use earshot_core::prompt::MAX_STYLE_CHARS;
use earshot_runtime::Services;
use earshot_runtime::summary_style::{self, SummaryStyleError};
use serde::Serialize;
use specta::Type;

use crate::ipc::IpcError;

#[cfg(test)]
#[path = "summary_style_tests.rs"]
mod tests;

/// The stored style with what the page says about it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Type)]
pub struct SummaryStyleView {
    /// The style in use ("" for none, the default).
    pub style: String,
    /// One line when the stored style is one this version refuses (none is used).
    pub unknown: Vec<String>,
    /// The longest style accepted, in characters.
    pub max_chars: u32,
}

fn view(style: &str, unknown: Vec<String>) -> SummaryStyleView {
    SummaryStyleView {
        style: style.to_owned(),
        unknown,
        max_chars: u32::try_from(MAX_STYLE_CHARS).unwrap_or(u32::MAX),
    }
}

/// `text` with its first letter upper-case and a full stop.
fn sentence(text: &str) -> String {
    let mut c = text.chars();
    let mut out: String = c
        .next()
        .map(|f| f.to_uppercase().collect())
        .unwrap_or_default();
    out.push_str(c.as_str());
    if !out.ends_with('.') {
        out.push('.');
    }
    out
}

/// The style of `services`' store.
///
/// # Errors
/// Store errors.
pub fn style_view(services: &Services) -> Result<SummaryStyleView, IpcError> {
    let (style, unknown) = summary_style::summary_style(&services.store())?;
    Ok(view(
        style.as_str(),
        unknown.iter().map(ToString::to_string).collect(),
    ))
}

/// Checks and stores `style` ("" for none); returns it as stored.
///
/// # Errors
/// `invalid_setting` (nothing stored, the reason in plain words); store errors.
pub fn set_style(services: &Services, style: &str) -> Result<SummaryStyleView, IpcError> {
    let stored = {
        let mut db = services.store();
        summary_style::set_summary_style(&mut db, style)
    };
    match stored {
        Ok(s) => Ok(view(s.as_str(), Vec::new())),
        Err(SummaryStyleError::Store(e)) => Err(e.into()),
        Err(SummaryStyleError::Refused(e)) => Err(IpcError {
            kind: "invalid_setting".into(),
            message: sentence(&e.to_string()),
            retryable: false,
        }),
    }
}

/// The summary style (and a stored one this version refuses).
#[tauri::command]
#[specta::specta]
pub async fn summary_style(
    state: tauri::State<'_, crate::ipc::AppState>,
) -> Result<SummaryStyleView, IpcError> {
    let services = state.services()?;
    crate::ipc::blocking(move || style_view(&services)).await
}

/// Checks and stores the summary style ("" for none; `invalid_setting`: nothing stored).
#[tauri::command]
#[specta::specta]
pub async fn set_summary_style(
    state: tauri::State<'_, crate::ipc::AppState>,
    style: String,
) -> Result<SummaryStyleView, IpcError> {
    let services = state.services()?;
    crate::ipc::blocking(move || set_style(&services, &style)).await
}
