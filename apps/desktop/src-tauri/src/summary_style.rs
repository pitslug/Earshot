//! The "Summary style" section of the Summaries page (T7.4b): the user's own words about
//! tone, length, emphasis and jargon, and the summary template beside them (T12.17, a
//! choice that applies at once). The settings and their rules are
//! `earshot_runtime::summary_style` (and `earshot_core::prompt::{SummaryStyle,
//! StylePreset}`); this module only maps them to the IPC types and holds the templates'
//! names and descriptions shown on the page.

use earshot_core::prompt::{MAX_STYLE_CHARS, StylePreset};
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
    /// One line when the stored template is one this version does not offer (Standard is used).
    pub unknown_template: Vec<String>,
    /// The longest style accepted, in characters.
    pub max_chars: u32,
    /// The chosen summary template (a `StylePreset` name; "standard" by default).
    pub template: String,
    /// Every template the page offers, in its order.
    pub templates: Vec<SummaryTemplateOption>,
}

/// One summary template on the page.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Type)]
pub struct SummaryTemplateOption {
    /// The stored name (`StylePreset::name`).
    pub value: String,
    /// The name shown.
    pub label: String,
    /// One line on what it gives.
    pub description: String,
}

/// What the page says about a template this version does not offer.
pub const UNKNOWN_TEMPLATE: &str = "This summary template is not one this version offers.";

/// The page's name and description of `preset`.
fn template_words(preset: StylePreset) -> (&'static str, &'static str) {
    match preset {
        StylePreset::Standard => (
            "Standard",
            "Short notes with every decision and action item.",
        ),
        StylePreset::Brief => (
            "Brief",
            "A one-sentence summary and at most three note bullets.",
        ),
        StylePreset::Decisions => (
            "Decisions and actions",
            "What was decided and who will do what, with only the background needed.",
        ),
        StylePreset::Detailed => (
            "Detailed notes",
            "Fuller notes on every topic, with the reasons for each decision.",
        ),
    }
}

/// Every template, in `StylePreset::ALL` order.
#[must_use]
pub fn template_options() -> Vec<SummaryTemplateOption> {
    StylePreset::ALL
        .into_iter()
        .map(|p| {
            let (label, description) = template_words(p);
            SummaryTemplateOption {
                value: p.name().to_owned(),
                label: label.to_owned(),
                description: description.to_owned(),
            }
        })
        .collect()
}

fn view(
    style: &str,
    template: StylePreset,
    unknown: Vec<String>,
    unknown_template: Vec<String>,
) -> SummaryStyleView {
    SummaryStyleView {
        style: style.to_owned(),
        unknown,
        unknown_template,
        max_chars: u32::try_from(MAX_STYLE_CHARS).unwrap_or(u32::MAX),
        template: template.name().to_owned(),
        templates: template_options(),
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
    let db = services.store();
    let (style, unknown) = summary_style::summary_style(&db)?;
    let (template, unknown_template) = summary_style::summary_template(&db)?;
    Ok(view(
        style.as_str(),
        template,
        unknown.iter().map(ToString::to_string).collect(),
        unknown_template.iter().map(ToString::to_string).collect(),
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
        Ok(_) => style_view(services),
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

/// Stores the summary template named `template` (a `StylePreset` name).
///
/// # Errors
/// `invalid_setting` ([`UNKNOWN_TEMPLATE`]; nothing stored); store errors.
pub fn set_template(services: &Services, template: &str) -> Result<SummaryStyleView, IpcError> {
    let preset: StylePreset = template.parse().map_err(|_| IpcError {
        kind: "invalid_setting".into(),
        message: UNKNOWN_TEMPLATE.into(),
        retryable: false,
    })?;
    summary_style::set_summary_template(&mut services.store(), preset)?;
    style_view(services)
}

/// Stores the summary template (one of `SummaryStyleView.templates`; `invalid_setting`:
/// nothing stored).
#[tauri::command]
#[specta::specta]
pub async fn set_summary_template(
    state: tauri::State<'_, crate::ipc::AppState>,
    template: String,
) -> Result<SummaryStyleView, IpcError> {
    let services = state.services()?;
    crate::ipc::blocking(move || set_template(&services, &template)).await
}
