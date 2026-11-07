//! The summary style setting (T7.4b): the user's own words about tone, length, emphasis and
//! jargon, one slot in the request that writes the summary they read
//! ([`earshot_core::prompt::SummaryStyle`]; the fixed rules stay outside it and win).
//! Device-local (`local_state` key [`SUMMARY_STYLE_KEY`], never synced, no migration),
//! written only through [`set_summary_style`] after the core check; empty (the default) is
//! no style.
//!
//! Every summary [`crate::summary::SummaryService::summarize`] writes reads the style
//! stored at that moment, whoever writes it (this computer, your own endpoint, Claude Code,
//! Codex), so a re-summarise uses the current style. It is user text sent only where the
//! summary's prompt already goes. Live summary windows never carry it (they are map
//! requests).
//!
//! A stored value this build refuses (not UTF-8, too long, control characters) reads as no
//! style and is reported ([`UnknownSetting`], by its reason, never its text), as the other
//! settings pages do.

use std::fmt;

use earshot_core::prompt::{StyleError, SummaryStyle};
use earshot_store::Store;

use crate::RuntimeError;
use crate::local::DeviceRecord;
pub use crate::model_settings::UnknownSetting;

#[cfg(test)]
#[path = "summary_style_tests.rs"]
mod tests;

/// `local_state` key of the summary style.
pub const SUMMARY_STYLE_KEY: &str = "settings/summary-style";

/// Why a summary style was refused (nothing was stored).
#[derive(Debug)]
pub enum SummaryStyleError {
    /// The text is too long or holds control characters.
    Refused(StyleError),
    /// Writing the store failed.
    Store(RuntimeError),
}

impl fmt::Display for SummaryStyleError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Refused(e) => write!(f, "{e}"),
            Self::Store(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for SummaryStyleError {}

/// The stored style (none stored: empty) and, when the stored value is refused, why (the
/// empty style is used).
///
/// # Errors
/// Store errors.
pub fn summary_style(
    store: &Store,
) -> Result<(SummaryStyle, Option<UnknownSetting>), RuntimeError> {
    let Some(bytes) = store.local_value(SUMMARY_STYLE_KEY)? else {
        return Ok((SummaryStyle::default(), None));
    };
    let refused = |why: String| {
        Ok((
            SummaryStyle::default(),
            Some(UnknownSetting {
                key: SUMMARY_STYLE_KEY,
                stored: format!("a summary style this version refuses: {why}"),
            }),
        ))
    };
    match String::from_utf8(bytes) {
        Ok(text) => match SummaryStyle::new(&text) {
            Ok(style) => Ok((style, None)),
            Err(e) => refused(e.to_string()),
        },
        Err(_) => refused("it is not text".into()),
    }
}

/// Checks `text` ([`SummaryStyle::new`]: trimmed, at most
/// [`earshot_core::prompt::MAX_STYLE_CHARS`] characters, no control characters but line
/// breaks and tabs) and stores it, replacing the earlier style; empty text stores no style.
/// Returns what was stored.
///
/// # Errors
/// [`SummaryStyleError`]: nothing is stored on a refusal.
pub fn set_summary_style(store: &mut Store, text: &str) -> Result<SummaryStyle, SummaryStyleError> {
    let style = SummaryStyle::new(text).map_err(SummaryStyleError::Refused)?;
    crate::local::set(
        store,
        &DeviceRecord::SummaryStyle.key(),
        style.as_str().as_bytes(),
    )
    .map_err(|e| SummaryStyleError::Store(e.into()))?;
    Ok(style)
}
