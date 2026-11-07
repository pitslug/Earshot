//! Rendering a meeting to an Obsidian note.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use super::text::{
    clock, local_civil, neutralise, replace_em_dash, sanitize_file_name, sanitize_tag, stamp,
    yaml_str, yaml_tag,
};
use super::{BEGIN_MARKER, END_MARKER, ExportError};
use crate::asr::Segment;
use crate::identity::EntityId;
use crate::model::{Meeting, Note, Speaker, TranscriptPass, VaultPath};
use crate::summary::SummaryV1;

/// Export settings (PLAN.md 3.8).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExportOptions {
    /// Vault-relative folder for meeting notes (`""` for the vault root).
    pub folder: String,
    /// Write attendees as `[[Person]]` links.
    pub link_attendees: bool,
    /// Replace em dashes in generated text with `" - "` (default on, D12).
    pub replace_em_dash: bool,
    /// Tags added to every note (before the meeting's own tags).
    pub tags: Vec<String>,
}

impl Default for ExportOptions {
    fn default() -> Self {
        ExportOptions {
            folder: "Meetings".into(),
            link_attendees: true,
            replace_em_dash: true,
            tags: vec!["meeting".into()],
        }
    }
}

/// What a note is rendered from. The caller picks the transcript (normally the live final
/// pass, in meeting order) and the attendee names.
#[derive(Debug, Clone, Copy)]
pub struct ExportInput<'a> {
    /// The meeting.
    pub meeting: &'a Meeting,
    /// Attendee display names, in the order to show them.
    pub attendees: &'a [String],
    /// The meeting's speakers (names for the transcript).
    pub speakers: &'a [Speaker],
    /// Transcript segments in meeting order.
    pub segments: &'a [Segment],
    /// The user's notes in meeting order.
    pub notes: &'a [Note],
    /// The current summary, if any.
    pub summary: Option<&'a SummaryV1>,
    /// Which pass `segments` are: the final transcript, or the live draft (a meeting not
    /// yet processed after recording, written `transcript: draft` so it is visibly not the
    /// final text); `None` without a transcript.
    pub transcript: Option<TranscriptPass>,
}

/// A rendered note.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderedExport {
    /// Vault-relative path for a new note.
    pub path: VaultPath,
    /// Frontmatter blocks in order: `(key, text)`, the text being the key line and its
    /// continuation lines, each ending in `\n`.
    pub frontmatter: Vec<(String, String)>,
    /// Tags as written (for the union merge).
    pub tags: Vec<String>,
    /// The managed region, from [`BEGIN_MARKER`] to [`END_MARKER`] and a final `\n`.
    pub region: String,
}

impl RenderedExport {
    /// The whole note for a new file.
    #[must_use]
    pub fn to_file(&self) -> String {
        let mut s = String::from("---\n");
        for (_, block) in &self.frontmatter {
            s.push_str(block);
        }
        s.push_str("---\n\n");
        s.push_str(&self.region);
        s
    }
}

/// A YAML block list (`key: []` when empty).
pub(super) fn yaml_list(key: &str, items: &[String]) -> String {
    if items.is_empty() {
        return format!("{key}: []\n");
    }
    let mut s = format!("{key}:\n");
    for i in items {
        let _ = writeln!(s, "  - {i}");
    }
    s
}

/// The `tags` block for already-sanitised tags.
pub(super) fn tags_block(tags: &[String]) -> String {
    let items: Vec<String> = tags.iter().map(|t| yaml_tag(t)).collect();
    yaml_list("tags", &items)
}

/// Appends sanitised `tags` not yet in `out`.
pub(super) fn union_tags<'a>(out: &mut Vec<String>, tags: impl IntoIterator<Item = &'a str>) {
    for t in tags {
        if let Some(t) = sanitize_tag(t)
            && !out.contains(&t)
        {
            out.push(t);
        }
    }
}

/// An attendee name usable inside `[[...]]`.
fn link_name(name: &str) -> String {
    let s: String = name
        .chars()
        .filter(|c| !matches!(c, '[' | ']'))
        .map(|c| if matches!(c, '|' | '#' | '^') { '-' } else { c })
        .collect();
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Renders the note.
///
/// # Errors
/// [`ExportError::Folder`] if `opts.folder` is not a vault-relative path.
pub fn render(
    input: &ExportInput<'_>,
    opts: &ExportOptions,
) -> Result<RenderedExport, ExportError> {
    let m = input.meeting;
    let (y, mo, d, h, mi) = local_civil(m.started_unix_ms, m.utc_offset_minutes);
    let date = format!("{y:04}-{mo:02}-{d:02}");

    let title = [
        Some(m.title.as_str()),
        input.summary.map(|s| s.title.as_str()),
    ]
    .into_iter()
    .flatten()
    .map(sanitize_file_name)
    .find(|t| !t.is_empty())
    .unwrap_or_else(|| "Meeting".to_owned());
    let file = format!("{date} {h:02}-{mi:02} {title}.md");
    let path = if opts.folder.is_empty() {
        VaultPath::new(&file)
    } else {
        VaultPath::new(&opts.folder).and_then(|f| VaultPath::new(&format!("{}/{file}", f.as_str())))
    }
    .map_err(ExportError::Folder)?;

    let mut attendees: Vec<String> = Vec::new();
    for a in input.attendees {
        let name = link_name(a);
        if name.is_empty() {
            continue;
        }
        let v = if opts.link_attendees {
            yaml_str(&format!("[[{name}]]"))
        } else {
            yaml_str(&name)
        };
        if !attendees.contains(&v) {
            attendees.push(v);
        }
    }
    let mut tags = Vec::new();
    union_tags(&mut tags, opts.tags.iter().map(String::as_str));
    union_tags(&mut tags, m.tags.iter().map(String::as_str));

    let mut frontmatter = vec![
        (
            "earshot_id".to_owned(),
            format!("earshot_id: {}\n", yaml_str(&m.id.to_string())),
        ),
        ("date".to_owned(), format!("date: {date}\n")),
        (
            "start".to_owned(),
            format!("start: {}\n", yaml_str(&format!("{h:02}:{mi:02}"))),
        ),
        (
            "duration".to_owned(),
            match m.duration_ms {
                Some(ms) => format!("duration: {}\n", yaml_str(&clock(ms, false))),
                None => "duration: null\n".to_owned(),
            },
        ),
        ("attendees".to_owned(), yaml_list("attendees", &attendees)),
        ("tags".to_owned(), tags_block(&tags)),
        ("source".to_owned(), "source: earshot\n".to_owned()),
    ];
    // Which transcript the note holds, right after the duration (none without one).
    if let Some(pass) = input.transcript {
        let value = match pass {
            TranscriptPass::Final => "final",
            TranscriptPass::Live => "draft",
        };
        frontmatter.insert(
            4,
            ("transcript".to_owned(), format!("transcript: {value}\n")),
        );
    }

    Ok(RenderedExport {
        path,
        frontmatter,
        tags,
        region: region(input, opts),
    })
}

/// The managed region.
fn region(input: &ExportInput<'_>, opts: &ExportOptions) -> String {
    // Generated text: one line, em dashes per the option, no markers.
    let gen_text = |s: &str| {
        let s = neutralise(&s.replace(['\r', '\n'], " "));
        if opts.replace_em_dash {
            replace_em_dash(&s)
        } else {
            s
        }
    };
    let mut sections: Vec<String> = Vec::new();

    if let Some(sum) = input.summary {
        if !sum.tldr.trim().is_empty() {
            sections.push(format!("## TL;DR\n\n{}\n", gen_text(&sum.tldr)));
        }
        if !sum.enhanced_notes.is_empty() {
            let mut s = String::from("## Notes\n");
            for sec in &sum.enhanced_notes {
                let _ = write!(s, "\n### {}\n\n", gen_text(&sec.heading));
                for b in &sec.bullets {
                    let _ = writeln!(s, "- {}", gen_text(&b.text));
                }
            }
            sections.push(s);
        }
        if !sum.decisions.is_empty() {
            let mut s = String::from("## Decisions\n\n");
            for dcs in &sum.decisions {
                let _ = writeln!(s, "- {}", gen_text(&dcs.text));
            }
            sections.push(s);
        }
        if !sum.action_items.is_empty() {
            let mut s = String::from("## Action items\n\n");
            for a in &sum.action_items {
                let _ = write!(s, "- [ ] **{}**: {}", gen_text(&a.owner), gen_text(&a.task));
                if let Some(due) = a.due.as_deref().filter(|d| !d.trim().is_empty()) {
                    let _ = write!(s, " (due: {})", gen_text(due));
                }
                s.push('\n');
            }
            sections.push(s);
        }
        if !sum.open_questions.is_empty() {
            let mut s = String::from("## Open questions\n\n");
            for q in &sum.open_questions {
                let _ = writeln!(s, "- {}", gen_text(q));
            }
            sections.push(s);
        }
    }

    if !input.notes.is_empty() {
        // The user's words: kept as typed (no em-dash replacement), markers neutralised.
        let mut s = String::from("## My notes\n\n");
        for n in input.notes {
            let text = neutralise(&n.text.replace("\r\n", "\n"));
            let mut lines = text.split('\n');
            let _ = writeln!(s, "- `{}` {}", stamp(n.at), lines.next().unwrap_or(""));
            for l in lines {
                if l.trim().is_empty() {
                    s.push('\n');
                } else {
                    let _ = writeln!(s, "  {l}");
                }
            }
        }
        sections.push(s);
    }

    if !input.segments.is_empty() {
        let names: BTreeMap<EntityId, &str> = input
            .speakers
            .iter()
            .map(|sp| (sp.id, sp.display_name.as_str()))
            .collect();
        let mut s = String::from("> [!note]- Transcript\n");
        for (i, seg) in input.segments.iter().enumerate() {
            if i > 0 {
                s.push_str(">\n");
            }
            let who = seg
                .speaker_id
                .and_then(|id| names.get(&id).copied())
                .unwrap_or("Unknown");
            let _ = writeln!(
                s,
                "> **`{}` {}:** {}",
                stamp(seg.start),
                gen_text(who),
                gen_text(&seg.text)
            );
        }
        sections.push(s);
    }

    let mut out = format!("{BEGIN_MARKER}\n");
    out.push_str(&sections.join("\n"));
    out.push_str(END_MARKER);
    out.push('\n');
    out
}
