//! The user's summary style (T7.4b): free text about tone, length, emphasis and jargon,
//! one slot in the request that writes the summary the user sees.
//!
//! The style never replaces the fixed rules (reply format, schema, citations, merging
//! duplicates, keeping every decision and action item): it is appended after them in its
//! own block ([`STYLE`]), introduced by fixed text that says the rules above win, and every
//! line of the user's text is prefixed with [`STYLE_LINE_PREFIX`], so no line of it can
//! close or fake the block's markers. Map requests never carry it (a partial summary is
//! not what the user reads); layout stays in the export renderer.

use core::fmt;

use serde::{Deserialize, Serialize};

use super::template::STYLE;

/// Longest style accepted, in characters (after trimming).
pub const MAX_STYLE_CHARS: usize = 1000;

/// The prefix of every line of the user's text inside the style block. The block's markers
/// are lines without it, so the user's text cannot end the block early.
pub const STYLE_LINE_PREFIX: &str = "| ";

/// The user's summary style: empty (the default, no style block at all) or checked text
/// of at most [`MAX_STYLE_CHARS`] characters without control characters other than line
/// breaks and tabs. Built only through [`SummaryStyle::new`] (deserialising checks the
/// same rules).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct SummaryStyle(String);

/// Why a style text was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StyleError {
    /// Longer than [`MAX_STYLE_CHARS`] characters (the length found).
    TooLong(usize),
    /// Holds a control character other than a line break or a tab.
    ControlCharacter,
}

impl fmt::Display for StyleError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StyleError::TooLong(n) => write!(
                f,
                "the summary style can be at most {MAX_STYLE_CHARS} characters; this one has {n}"
            ),
            StyleError::ControlCharacter => write!(
                f,
                "the summary style can hold only text, line breaks and tabs"
            ),
        }
    }
}

impl std::error::Error for StyleError {}

/// Characters a style may hold although they are control characters.
fn allowed_control(c: char) -> bool {
    matches!(c, '\n' | '\r' | '\t')
}

impl SummaryStyle {
    /// `text` trimmed, checked: at most [`MAX_STYLE_CHARS`] characters, no control
    /// characters but `\n`, `\r` and `\t`. Line breaks are stored as `\n`.
    ///
    /// # Errors
    /// [`StyleError`].
    pub fn new(text: &str) -> Result<Self, StyleError> {
        // Checked before trimming: some control characters (NEL) are also whitespace.
        if text.chars().any(|c| c.is_control() && !allowed_control(c)) {
            return Err(StyleError::ControlCharacter);
        }
        let text = text.replace("\r\n", "\n").replace('\r', "\n");
        let text = text.trim();
        let n = text.chars().count();
        if n > MAX_STYLE_CHARS {
            return Err(StyleError::TooLong(n));
        }
        Ok(SummaryStyle(text.to_owned()))
    }

    /// Whether there is no style (no block in any prompt).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// The text as stored.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The style block appended to a final request's system message: `None` when empty,
    /// else [`STYLE`] with the neutralised text ([`neutralise`]) in its slot.
    #[must_use]
    pub(super) fn block(&self) -> Option<String> {
        (!self.is_empty()).then(|| STYLE.replace("{{style}}", &neutralise(&self.0)))
    }
}

impl TryFrom<String> for SummaryStyle {
    type Error = StyleError;

    fn try_from(s: String) -> Result<Self, StyleError> {
        SummaryStyle::new(&s)
    }
}

impl From<SummaryStyle> for String {
    fn from(s: SummaryStyle) -> String {
        s.0
    }
}

/// `text` as the lines of the style block: every line break a model may read as one
/// (`\r`, vertical tab, form feed, NEL, line and paragraph separators) becomes `\n`, other
/// control characters are dropped, tabs become spaces, line ends are trimmed, and every
/// line starts with [`STYLE_LINE_PREFIX`] (an empty line is the prefix without its
/// space). No line of the result equals a marker of the block.
fn neutralise(text: &str) -> String {
    let unified: String = text
        .replace("\r\n", "\n")
        .chars()
        .filter_map(|c| match c {
            '\r' | '\u{0B}' | '\u{0C}' | '\u{85}' | '\u{2028}' | '\u{2029}' => Some('\n'),
            '\t' => Some(' '),
            '\n' => Some('\n'),
            c if c.is_control() => None,
            c => Some(c),
        })
        .collect();
    unified
        .trim_matches('\n')
        .split('\n')
        .map(|line| {
            let line = line.trim_end();
            if line.is_empty() {
                STYLE_LINE_PREFIX.trim_end().to_owned()
            } else {
                format!("{STYLE_LINE_PREFIX}{line}")
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
pub(super) fn neutralise_for_tests(text: &str) -> String {
    neutralise(text)
}
