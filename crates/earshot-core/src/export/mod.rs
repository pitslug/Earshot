//! Obsidian export (PLAN.md 3.8, T5.3): a pure renderer from the data model to a Markdown
//! note, and the merge that re-exports into an existing note without losing user edits.
//!
//! - [`render`]: file path `<folder>/YYYY-MM-DD HH-mm <Title>.md` (local start time from
//!   the meeting's recorded UTC offset; title sanitised with hyphens, never em dashes),
//!   YAML frontmatter (`earshot_id`, `date`, `start`, `duration`, `attendees`, `tags`,
//!   `source`), and the Earshot-managed body between [`BEGIN_MARKER`] and [`END_MARKER`]:
//!   TL;DR, enhanced notes, decisions, action items as `- [ ]` tasks, open questions, the
//!   user's raw notes, and the transcript in a collapsed `> [!note]- Transcript` callout.
//! - [`merge`]: replaces only the managed region and the Earshot-owned frontmatter keys
//!   ([`OWNED_KEYS`], updated in place), keeps every other key and all text outside the
//!   region byte for byte, and merges `tags` as a union. Merging a render into its own
//!   output is the identity, so re-export is byte-identical.
//! - [`earshot_id_of`]: the `earshot_id` in a note's frontmatter (finding a renamed or
//!   moved note).
//!
//! With [`ExportOptions::replace_em_dash`] (default on, D12) em dashes in generated text
//! (summary, transcript) become `" - "`; the user's own notes are written as typed.
//! Content can never forge a region marker: `<!--` in any rendered text is written as
//! `&lt;!--`.
//!
//! The frontmatter is written by hand, not by a YAML library: strings are double-quoted
//! with escapes, and merging works on top-level key blocks (a key line plus its indented
//! or list continuation lines), so user keys are preserved verbatim.

mod merge;
mod render;
mod text;

pub use merge::{earshot_id_of, merge};
pub use render::{ExportInput, ExportOptions, RenderedExport, render};
pub use text::{MAX_TITLE_BYTES, MAX_TITLE_CHARS, replace_em_dash, sanitize_file_name};

use core::fmt;

use crate::model::VaultPathError;

/// First line of the Earshot-managed body region.
pub const BEGIN_MARKER: &str = "<!-- earshot:begin -->";
/// Last line of the Earshot-managed body region.
pub const END_MARKER: &str = "<!-- earshot:end -->";

/// Frontmatter keys Earshot owns and rewrites in place on re-export (`tags` is merged
/// instead).
pub const OWNED_KEYS: [&str; 7] = [
    "earshot_id",
    "date",
    "start",
    "duration",
    "transcript",
    "attendees",
    "source",
];

/// Export errors.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExportError {
    /// The configured folder is not a vault-relative path.
    Folder(VaultPathError),
    /// The existing note's region markers are missing one half, repeated or reversed, so
    /// the managed region cannot be located safely; nothing is merged.
    Markers(&'static str),
}

impl fmt::Display for ExportError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ExportError::Folder(e) => write!(f, "export folder: {e}"),
            ExportError::Markers(m) => write!(f, "managed region markers: {m}"),
        }
    }
}

impl std::error::Error for ExportError {}

#[cfg(test)]
mod tests;
