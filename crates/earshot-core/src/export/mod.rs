//! Obsidian export (PLAN.md 3.8, T5.3): a pure renderer from the data model to a Markdown
//! note. A re-export writes the whole rendered note over the existing one (T14.63, the
//! owner: "export is export"): ticks, edits and anything the user added there are
//! replaced, and the app asks before re-exporting a meeting that was exported before.
//!
//! - [`render`]: file path `<folder>/YYYY-MM-DD HH-mm <Title>.md` (local start time from
//!   the meeting's recorded UTC offset; title sanitised with hyphens, never em dashes),
//!   YAML frontmatter (`earshot_id`, `date`, `start`, `duration`, `attendees`, `tags`,
//!   `source`), and the body between [`BEGIN_MARKER`] and [`END_MARKER`]: TL;DR,
//!   decisions, action items as `- [ ]` tasks (grouped into the user's and other people's,
//!   T14.61), enhanced notes, open questions, the user's raw notes, and the transcript in
//!   a collapsed `> [!note]- Transcript` callout. Rendering is deterministic, so two
//!   exports in a row write identical bytes.
//! - [`earshot_id_of`]: the `earshot_id` in a note's frontmatter (finding a renamed or
//!   moved note, which is replaced where it is, never duplicated).
//!
//! With [`ExportOptions::replace_em_dash`] (default on, D12) em dashes in generated text
//! (summary, transcript) become `" - "`; the user's own notes are written as typed.
//! Content can never forge a marker: `<!--` in any rendered text is written as `&lt;!--`.
//!
//! Escaping rule (T14.28): generated text (summary fields, speaker names and transcript
//! lines, all of which can come from a model, a recogniser or another computer) cannot
//! form an image or embed (`![...](...)`, `![[...]]`), raw HTML or an HTML comment, so
//! opening a note never fetches anything, and cannot hide the rest of the note behind an
//! Obsidian comment (`%%`), a code fence or display maths (`$$`): a `<` that could start
//! HTML is written `&lt;`, the `[` of every `![` `&#91;`, and so on (`text::neutralise_generated`
//! has the whole list). Entities, not backslash escapes, because a backslash already in
//! the text would cancel an inserted one; Obsidian's reading view shows every entity as
//! the original character. Plain links and wikilinks are kept (they fetch nothing until
//! clicked). The user's own notes are the user's text and are written as typed, with only
//! the `<!--` rule applied. An accepted limit: third-party plugins that run inline code
//! spans (for example Dataview inline queries) are not neutralised against.
//!
//! The frontmatter is written by hand, not by a YAML library: strings are double-quoted
//! with escapes.

mod note_id;
mod render;
mod text;

pub use note_id::earshot_id_of;
pub use render::{
    ExportInput, ExportOptions, ExportUser, OwnerGroup, RenderedExport, owner_group, owner_names,
    render,
};
pub use text::{MAX_TITLE_BYTES, MAX_TITLE_CHARS, replace_em_dash, sanitize_file_name};

use core::fmt;

use crate::model::VaultPathError;

/// First line of the body Earshot writes (kept in every note since T5.3, so notes keep
/// their shape; a re-export no longer reads it).
pub const BEGIN_MARKER: &str = "<!-- earshot:begin -->";
/// Last line of the body Earshot writes.
pub const END_MARKER: &str = "<!-- earshot:end -->";

/// Export errors.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExportError {
    /// The configured folder is not a vault-relative path.
    Folder(VaultPathError),
}

impl fmt::Display for ExportError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ExportError::Folder(e) => write!(f, "export folder: {e}"),
        }
    }
}

impl std::error::Error for ExportError {}

#[cfg(test)]
mod tests;
