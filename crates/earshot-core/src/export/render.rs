//! Rendering a meeting to an Obsidian note.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use super::text::{
    clock, local_civil, neutralise, neutralise_generated, replace_em_dash, sanitize_file_name,
    sanitize_tag, stamp, yaml_str, yaml_tag,
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
    /// Segments of `segments` the final pass left without a speaker (T12.65): named
    /// [`crate::diar::UNKNOWN_SPEAKER`]. Any other segment without a speaker name is
    /// "Unknown", as before.
    pub unknown_speaker_lines: &'a [EntityId],
    /// The user, once they have given a name (T14.61): action items they own are listed
    /// under "My actions". `None` without a name.
    pub user: Option<ExportUser<'a>>,
}

/// The user as the note's action groups know them (T14.61).
#[derive(Debug, Clone, Copy)]
pub struct ExportUser<'a> {
    /// The user's name: in the app the `is_me` person's name, never the default "Me".
    pub name: &'a str,
    /// How names are compared: an owner is the user when `key(owner) == key(name)`. The app
    /// passes the runtime's `edit::person_name_key`, the rule the meeting page uses, so page
    /// and note group alike; core has no copy of it.
    pub key: fn(&str) -> String,
}

impl ExportUser<'_> {
    /// Whether an action item's `owner` names the user.
    #[must_use]
    pub fn owns(&self, owner: &str) -> bool {
        let user = (self.key)(self.name);
        !user.trim().is_empty() && (self.key)(owner) == user
    }
}

/// A name as one line's owners are told apart, and as the merge compares old and new
/// owners: white space trimmed and collapsed, lower case. Not the user rule
/// ([`ExportUser::key`]).
pub(super) fn name_key(name: &str) -> String {
    name.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

/// Owners that name no one person, lower case (T14.60: `GROUP_OWNERS` in
/// `packages/ui/src/detail/actionGroups.ts`, changed together).
pub const GROUP_OWNERS: [&str; 3] = ["everyone", "all", "team"];

/// The names in an action item's owner text (the meeting page's rule, T14.60, pinned by
/// `fixtures/ui/action_owner_cases.json`): split on ", ", " and " and " & ", each trimmed,
/// empty ones dropped. An owner that is one of [`GROUP_OWNERS`] (any case) names no one
/// person and stays whole, as written (the UI's `GROUP_OWNERS`).
#[must_use]
pub fn owner_names(owner: &str) -> Vec<&str> {
    let whole = owner.trim();
    if GROUP_OWNERS.contains(&whole.to_lowercase().as_str()) {
        return vec![whole];
    }
    let mut names = vec![whole];
    for sep in [", ", " and ", " & "] {
        names = names.into_iter().flat_map(|n| n.split(sep)).collect();
    }
    names
        .into_iter()
        .map(str::trim)
        .filter(|n| !n.is_empty())
        .collect()
}

/// Which group one action item's `owner` text puts it in, and the names shown on its line
/// (T14.61, the meeting page's rule): with a user who is among the names, "My actions" and
/// the other names (shown as "(with ...)"); with a user who is not, "Other actions" and every
/// name; without a user, no groups and every name. Names the key makes equal are one name,
/// first spelling shown (without a user, told apart by white space and case only).
#[must_use]
pub fn owner_group<'o>(owner: &'o str, user: Option<ExportUser<'_>>) -> (OwnerGroup, Vec<&'o str>) {
    let key = |s: &str| user.map_or_else(|| name_key(s), |u| (u.key)(s));
    let mut names: Vec<&str> = Vec::new();
    for n in owner_names(owner) {
        if !names.iter().any(|o| key(o) == key(n)) {
            names.push(n);
        }
    }
    let Some(user) = user.filter(|u| !(u.key)(u.name).trim().is_empty()) else {
        return (OwnerGroup::NoGroups, names);
    };
    if names.iter().any(|n| user.owns(n)) {
        names.retain(|n| !user.owns(n));
        (OwnerGroup::Mine, names)
    } else {
        (OwnerGroup::Other, names)
    }
}

/// An action item's group in the note ([`owner_group`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OwnerGroup {
    /// "### My actions".
    Mine,
    /// "### Other actions".
    Other,
    /// No user name: one list.
    NoGroups,
}

/// One action-item line: items with the same task text, due date and citations, owners in
/// summary order.
struct ActionLine<'a> {
    task: &'a str,
    due: Option<&'a str>,
    owners: Vec<&'a str>,
    mine: bool,
}

/// The action items as lines (T14.61), with the meeting page's rules (T14.60): items with
/// the same task text, due date and citations are one line, with each owner once (told
/// apart by `name_key`, written trimmed as first given); a line is the user's when any of
/// its owners is the user. Lines are in meeting order: by each line's earliest cited
/// segment (its place in `segments`, which are in meeting order), a stable sort, and lines
/// citing no segment of the transcript last, in summary order.
fn action_lines<'a>(
    sum: &'a SummaryV1,
    segments: &[Segment],
    user: Option<ExportUser<'_>>,
) -> Vec<ActionLine<'a>> {
    let mut lines: Vec<(ActionLine<'a>, &'a [EntityId])> = Vec::new();
    let key = |s: &str| user.map_or_else(|| name_key(s), |u| (u.key)(s));
    for a in &sum.action_items {
        let names = owner_names(&a.owner);
        let due = a.due.as_deref().filter(|d| !d.trim().is_empty());
        let mine = user.is_some_and(|u| names.iter().any(|n| u.owns(n)));
        let at = lines.iter().position(|(l, ids)| {
            l.task == a.task && l.due == due && *ids == a.source_segment_ids.as_slice()
        });
        let i = at.unwrap_or_else(|| {
            lines.push((
                ActionLine {
                    task: &a.task,
                    due,
                    owners: Vec::new(),
                    mine: false,
                },
                &a.source_segment_ids,
            ));
            lines.len() - 1
        });
        let l = &mut lines[i].0;
        for n in names {
            if !l.owners.iter().any(|o| key(o) == key(n)) {
                l.owners.push(n);
            }
        }
        l.mine |= mine;
    }
    let place: BTreeMap<EntityId, usize> = segments
        .iter()
        .enumerate()
        .map(|(i, s)| (s.id, i))
        .rev()
        .collect();
    let first_cited = |ids: &[EntityId]| ids.iter().filter_map(|id| place.get(id)).min().copied();
    // `sort_by_key` is stable; `None` (nothing cited) sorts after every `Some`.
    lines.sort_by_key(|(_, ids)| first_cited(ids).map_or((1, 0), |i| (0, i)));
    lines.into_iter().map(|(l, _)| l).collect()
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
    /// The action-item lines of `region`, in order, as the render built them (T14.61): what
    /// the merge needs to carry a tick to the same action in another form.
    pub actions: Vec<RenderedAction>,
}

/// One action-item line of a rendered region (T14.61). All text is as written (generated
/// text escaped).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderedAction {
    /// The line's text after `- [ ] `.
    pub text: String,
    /// The task and its " (due: ...)", without owners.
    pub task: String,
    /// Under "### My actions".
    pub mine: bool,
    /// Every owner name, the user's included.
    pub owners: Vec<String>,
    /// The names shown after "with" on a line of the user's own; empty otherwise.
    pub others: Vec<String>,
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

    let (region, actions) = region(input, opts);
    Ok(RenderedExport {
        path,
        frontmatter,
        tags,
        region,
        actions,
    })
}

/// The line an empty action group holds (fixed text, not a task line, so the merge never
/// reads or matches it).
const EMPTY_GROUP: &str = "_None_\n";

/// The "## Action items" section (T14.61), as the meeting page lists them: with a user
/// name, "### My actions" (the user's own, no owner; one the user shares ends
/// "(with <others>)") then "### Other actions" (each line ends "(<owners>)"), each in
/// meeting order, both always written and an empty one holding [`EMPTY_GROUP`]; without
/// one, a single list with the owners at the end of each line. A due date stays
/// "(due: ...)" before the owners. Every piece of generated text goes through `gen_text` on
/// its own.
fn action_items(
    sum: &SummaryV1,
    segments: &[Segment],
    user: Option<ExportUser<'_>>,
    gen_text: &dyn Fn(&str) -> String,
) -> (String, Vec<RenderedAction>) {
    let user = user.filter(|u| !(u.key)(u.name).trim().is_empty());
    let lines = action_lines(sum, segments, user);
    let mut actions = Vec::new();
    let mut line = |l: &ActionLine<'_>, shown: &[&str], mine: bool| {
        let mut task = gen_text(l.task);
        if let Some(due) = l.due {
            let _ = write!(task, " (due: {})", gen_text(due));
        }
        let names: Vec<String> = shown.iter().map(|o| gen_text(o)).collect();
        let mut text = task.clone();
        if !names.is_empty() {
            let _ = write!(
                text,
                " ({}{})",
                if mine { "with " } else { "" },
                names.join(", ")
            );
        }
        let s = format!("- [ ] {text}\n");
        actions.push(RenderedAction {
            text,
            task,
            mine,
            owners: l.owners.iter().map(|o| gen_text(o)).collect(),
            others: if mine { names } else { Vec::new() },
        });
        s
    };
    let mut s = String::from("## Action items\n\n");
    let Some(user) = user else {
        for l in &lines {
            s.push_str(&line(l, &l.owners, false));
        }
        return (s, actions);
    };
    let mine: Vec<&ActionLine<'_>> = lines.iter().filter(|l| l.mine).collect();
    let other: Vec<&ActionLine<'_>> = lines.iter().filter(|l| !l.mine).collect();
    // Both groups always (the owner, 2026-11-27): an empty one says so, so its absence never
    // looks like a fault, and the merge can tell a deleted heading from an empty group.
    s.push_str("### My actions\n\n");
    if mine.is_empty() {
        s.push_str(EMPTY_GROUP);
    }
    for l in &mine {
        let others: Vec<&str> = l.owners.iter().copied().filter(|o| !user.owns(o)).collect();
        s.push_str(&line(l, &others, true));
    }
    s.push_str("\n### Other actions\n\n");
    if other.is_empty() {
        s.push_str(EMPTY_GROUP);
    }
    for l in &other {
        s.push_str(&line(l, &l.owners, false));
    }
    (s, actions)
}

/// The managed region and its action-item lines.
fn region(input: &ExportInput<'_>, opts: &ExportOptions) -> (String, Vec<RenderedAction>) {
    // Generated text: one line, em dashes per the option, no markers, images, embeds or
    // HTML.
    let gen_text = |s: &str| {
        let s = neutralise_generated(&s.replace(['\r', '\n'], " "));
        if opts.replace_em_dash {
            replace_em_dash(&s)
        } else {
            s
        }
    };
    let mut sections: Vec<String> = Vec::new();
    let mut actions = Vec::new();

    if let Some(sum) = input.summary {
        if !sum.tldr.trim().is_empty() {
            sections.push(format!("## TL;DR\n\n{}\n", gen_text(&sum.tldr)));
        }
        // The meeting page's order (T14.61): decisions and actions before the note sections.
        if !sum.decisions.is_empty() {
            let mut s = String::from("## Decisions\n\n");
            for dcs in &sum.decisions {
                let _ = writeln!(s, "- {}", gen_text(&dcs.text));
            }
            sections.push(s);
        }
        if !sum.action_items.is_empty() {
            let (s, a) = action_items(sum, input.segments, input.user, &gen_text);
            sections.push(s);
            actions = a;
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
                .unwrap_or(if input.unknown_speaker_lines.contains(&seg.id) {
                    crate::diar::UNKNOWN_SPEAKER
                } else {
                    "Unknown"
                });
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
    (out, actions)
}
