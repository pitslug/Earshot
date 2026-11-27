//! Re-export into an existing note: frontmatter merge and managed-region replacement.
//!
//! The merge works on the note's original text: every byte outside the owned frontmatter
//! keys and the managed region is copied as is, including its line endings (LF, CRLF or a
//! mix) and a leading UTF-8 byte-order mark. Generated text (owned keys, tags, the region,
//! separators) uses the note's dominant line ending.

use super::render::{RenderedExport, name_key, owner_names, tags_block, union_tags};
use super::{BEGIN_MARKER, END_MARKER, ExportError, OWNED_KEYS};
use crate::identity::EntityId;

const BOM: char = '\u{feff}';

/// One top-level frontmatter block: a key line and its continuation lines (indented, list
/// items, blank), or lines before the first key / comments (`key == None`).
struct Block<'a> {
    key: Option<String>,
    text: &'a str,
}

/// A note split around its frontmatter; every part keeps its own line endings.
struct Parts<'a> {
    /// The opening `---` line.
    open: &'a str,
    /// The frontmatter between the `---` lines.
    fm: &'a str,
    /// The closing `---` line (may lack a newline at end of file).
    close: &'a str,
    /// Everything after it.
    body: &'a str,
}

fn is_fence(line: &str) -> bool {
    line.trim_end_matches(['\n', '\r']) == "---"
}

/// `None` when the note does not start with a closed frontmatter block.
fn split_frontmatter(doc: &str) -> Option<Parts<'_>> {
    let mut lines = doc.split_inclusive('\n');
    let open = lines.next()?;
    if !is_fence(open) || !open.ends_with('\n') {
        return None;
    }
    let mut pos = open.len();
    for line in lines {
        if is_fence(line) {
            return Some(Parts {
                open,
                fm: &doc[open.len()..pos],
                close: line,
                body: &doc[pos + line.len()..],
            });
        }
        pos += line.len();
    }
    None
}

/// The key of a top-level `key: value` line.
fn key_of(line: &str) -> Option<String> {
    let first = line.chars().next()?;
    if first.is_whitespace() || matches!(first, '-' | '#') {
        return None;
    }
    let k = unquote(line.split_once(':')?.0);
    (!k.is_empty()).then(|| k.to_owned())
}

fn blocks(fm: &str) -> Vec<Block<'_>> {
    let mut out: Vec<Block<'_>> = Vec::new();
    let mut start = 0;
    let mut key: Option<String> = None;
    let mut pos = 0;
    for line in fm.split_inclusive('\n') {
        let k = key_of(line);
        let comment = line.starts_with('#');
        if (k.is_some() || comment) && pos > start {
            out.push(Block {
                key: key.take(),
                text: &fm[start..pos],
            });
            start = pos;
        }
        if k.is_some() || comment {
            key = k;
        }
        pos += line.len();
    }
    if pos > start {
        out.push(Block {
            key,
            text: &fm[start..pos],
        });
    }
    out
}

fn unquote(s: &str) -> &str {
    let s = s.trim();
    s.strip_prefix('"')
        .and_then(|x| x.strip_suffix('"'))
        .or_else(|| s.strip_prefix('\'').and_then(|x| x.strip_suffix('\'')))
        .unwrap_or(s)
}

/// Items of a flow sequence body (`a, "b c", 'd'`) or a scalar list (`a b, "c d"`): split on
/// commas (and, in a scalar, whitespace) outside quotes, quotes removed.
fn split_items(s: &str, split_on_space: bool) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quote: Option<char> = None;
    let mut flush = |cur: &mut String| {
        let t = cur.trim();
        if !t.is_empty() {
            out.push(t.to_owned());
        }
        cur.clear();
    };
    for c in s.chars() {
        match quote {
            Some(q) if c == q => quote = None,
            Some(_) => cur.push(c),
            None if c == '"' || c == '\'' => quote = Some(c),
            None if c == ',' || (split_on_space && c.is_whitespace()) => flush(&mut cur),
            None => cur.push(c),
        }
    }
    flush(&mut cur);
    out
}

/// Tags of an existing `tags` block: inline `[a, "b c"]`, a scalar (comma or space
/// separated, quotes respected), or a block list.
fn parse_tags(block: &str) -> Vec<String> {
    let mut lines = block.lines();
    let inline = lines
        .next()
        .and_then(|l| l.split_once(':'))
        .map_or("", |(_, v)| v.trim());
    let mut out = match inline.strip_prefix('[').and_then(|v| v.strip_suffix(']')) {
        Some(flow) => split_items(flow, false),
        None => split_items(inline, true),
    };
    for l in lines {
        if let Some(item) = l.trim_start().strip_prefix('-') {
            let t = unquote(item);
            if !t.is_empty() {
                out.push(t.to_owned());
            }
        }
    }
    out
}

/// The `earshot_id` in `doc`'s frontmatter (any line endings, optional BOM), if present
/// and valid.
#[must_use]
pub fn earshot_id_of(doc: &str) -> Option<EntityId> {
    let doc = doc.strip_prefix(BOM).unwrap_or(doc);
    let parts = split_frontmatter(doc)?;
    let b = blocks(parts.fm)
        .into_iter()
        .find(|b| b.key.as_deref() == Some("earshot_id"))?;
    let v = b.text.lines().next()?.split_once(':')?.1;
    unquote(v).parse().ok()
}

/// `"\r\n"` when more than half of the note's line breaks are CRLF, else `"\n"`.
fn dominant_eol(doc: &str) -> &'static str {
    let crlf = doc.matches("\r\n").count();
    let lf = doc.matches('\n').count();
    if crlf * 2 > lf { "\r\n" } else { "\n" }
}

/// Task text with every escape `neutralise_generated` has ever written undone (`&lt;`,
/// `&#91;`, `&#37;`, `&#36;`, `&#96;`, `&#126;`), so a line written under any version of
/// the escaping rule compares equal to its re-render. Pinned to `neutralise_generated` by
/// `export_plain_form_undoes_every_generated_escape`.
pub(super) fn plain_form(text: &str) -> String {
    text.replace("&lt;", "<")
        .replace("&#91;", "[")
        .replace("&#37;", "%")
        .replace("&#36;", "$")
        .replace("&#96;", "`")
        .replace("&#126;", "~")
}

/// A task line `- [<mark>] <text>` (line ending removed): its mark and its text.
fn task_line(line: &str) -> Option<(char, &str)> {
    let line = line.trim_end_matches(['\n', '\r']);
    let rest = line.strip_prefix("- [")?;
    let mut chars = rest.chars();
    let mark = chars.next()?;
    let text = chars.as_str().strip_prefix("] ")?;
    (!matches!(mark, '\n' | '\r')).then_some((mark, text))
}

/// An action-item line's task with its owners taken off (T14.61), for matching a line
/// across the change of format: a leading "**Owner**: " (the form before T14.61) and a
/// trailing "(<owners>)" or "(with <owners>)" (the form since) are removed, a trailing
/// "(due: ...)" is kept. Owner names are compared by `render::name_key` (old and new
/// lines hold the same summary's owner text, so no user rule is needed). A text that has neither is its own task with no owners.
#[derive(Debug, Default)]
struct TaskKey {
    task: String,
    owners: Vec<String>,
    /// The owners were listed as "(with ...)": a line of the user's own, shared.
    with: bool,
}

impl TaskKey {
    fn of(text: &str) -> TaskKey {
        let name = name_key;
        let mut owners = Vec::new();
        let mut rest = text;
        if let Some(after) = text.strip_prefix("**")
            && let Some(end) = after.find("**: ")
            && end > 0
        {
            owners = owner_names(&after[..end]).into_iter().map(name).collect();
            rest = &after[end + 4..];
        }
        let mut with = false;
        if let Some(open) = trailing_group(rest) {
            let inner = &rest[open + 1..rest.len() - 1];
            if !inner.starts_with("due: ") {
                let (listed, w) = match inner.strip_prefix("with ") {
                    Some(l) => (l, true),
                    None => (inner, false),
                };
                if owners.is_empty() {
                    owners = listed.split(", ").map(name).collect();
                    with = w;
                }
                rest = &rest[..open - 1];
            }
        }
        TaskKey {
            task: rest.to_owned(),
            owners,
            with,
        }
    }
}

/// [`TaskKey::of`] as (task, owners, with), for the tests.
#[cfg(test)]
pub(super) fn task_key_parts(text: &str) -> (String, Vec<String>, bool) {
    let k = TaskKey::of(text);
    (k.task, k.owners, k.with)
}

/// The byte index of the `(` that opens a balanced "(...)" ending `text`, when a space
/// comes before it.
fn trailing_group(text: &str) -> Option<usize> {
    if !text.ends_with(')') {
        return None;
    }
    let mut depth = 0usize;
    for (i, c) in text.char_indices().rev() {
        match c {
            ')' => depth += 1,
            '(' => {
                depth -= 1;
                if depth == 0 {
                    return (i > 0 && text[..i].ends_with(' ')).then_some(i);
                }
            }
            _ => {}
        }
    }
    None
}

/// The new managed region with the task marks the user set in the old one (T14.27).
///
/// Earshot renders action items as unticked `- [ ]` tasks inside the region, which a
/// re-export replaces whole. Matching runs in three passes over the task lines of the new
/// region, each old task line used at most once, first come first served (the third, for
/// notes written before T14.61's change of the action-item format, is described where it
/// runs):
///
/// 1. Exact: every new task line, whatever its own mark, takes the first unused old line
///    with the same text after the checkbox. Repeated identical lines pair up in order,
///    and a line whose generated text already holds a mark (`- [x] foo` from a bullet
///    reading `[x] foo`) uses up its own old copy, so merging a render into its own
///    output stays the identity.
/// 2. Appended: a new unticked line still unmatched takes the first unused old line
///    whose text starts with the new text and a space (a task plugin added a done date
///    or a tag after the text). Only that direction: an old line that is a prefix of the
///    new text is another, longer item and does not match.
///
/// Only an unticked new line (`[ ]`) is rewritten, to the matched line's mark exactly as
/// the user left it (`x`, `X` or another plugin's character); appended text is not
/// carried. Texts are compared in their plain form (`plain_form`), so a line written
/// before or after a change to the escaping rule matches its re-render. Only text between
/// the old markers is read; every other byte of the new region is unchanged, so with no
/// marks set the result is the new region itself.
fn carry_task_marks(old_region: &str, new_region: &str) -> String {
    let mut old: Vec<Option<(char, String)>> = old_region
        .split_inclusive('\n')
        .filter_map(task_line)
        .map(|(m, t)| Some((m, plain_form(t))))
        .collect();
    let old_marks: Vec<char> = old.iter().flatten().map(|(m, _)| *m).collect();
    if old_marks.iter().all(|m| *m == ' ') {
        // Nothing marked: the new region as rendered.
        return new_region.to_owned();
    }
    let lines: Vec<&str> = new_region.split_inclusive('\n').collect();
    let new: Vec<Option<(char, String)>> = lines
        .iter()
        .map(|l| task_line(l).map(|(m, t)| (m, plain_form(t))))
        .collect();
    // The old mark each new line takes (pass 1, then pass 2).
    let mut taken: Vec<Option<char>> = vec![None; lines.len()];
    let mut take = |matches: &dyn Fn(&str) -> bool| {
        let slot = old
            .iter_mut()
            .find(|o| o.as_ref().is_some_and(|(_, t)| matches(t)))?;
        slot.take().map(|(m, _)| m)
    };
    for (i, n) in new.iter().enumerate() {
        if let Some((_, text)) = n {
            taken[i] = take(&|t: &str| t == text);
        }
    }
    for (i, n) in new.iter().enumerate() {
        if let Some((' ', text)) = n
            && taken[i].is_none()
        {
            taken[i] = take(&|t: &str| {
                t.strip_prefix(text.as_str())
                    .is_some_and(|rest| rest.starts_with(' '))
            });
        }
    }
    // Pass 3 (T14.61): the action-item format changed from "**Owner**: task" once per
    // owner to grouped lines with the owners in a trailing "(...)". A new unticked line still
    // unmatched collects the unused old lines with the same task (`TaskKey`):
    // a. every one whose owner is among the new line's listed owners (a shared item takes
    //    each per-owner line it replaces);
    // b. then, for a line of the user's own (no owners listed, or "(with ...)"), the first
    //    remaining one whose owners are not listed: the user's own old line, whose name the
    //    merge does not know.
    // The merged line is ticked if any line it collected was ticked: it takes the mark of
    // the first ticked one in the old region's order. Each old line is still used once.
    let keys: Vec<Option<TaskKey>> = new
        .iter()
        .zip(&taken)
        .map(|(n, t)| match (n, t) {
            (Some((' ', text)), None) => Some(TaskKey::of(text)),
            _ => None,
        })
        .collect();
    let old_keys: Vec<TaskKey> = old
        .iter()
        .map(|o| {
            o.as_ref()
                .map_or_else(TaskKey::default, |(_, t)| TaskKey::of(t))
        })
        .collect();
    let mut collected: Vec<Vec<usize>> = vec![Vec::new(); lines.len()];
    for (i, k) in keys.iter().enumerate() {
        let Some(k) = k.as_ref().filter(|k| !k.owners.is_empty()) else {
            continue;
        };
        for (j, ok) in old_keys.iter().enumerate() {
            if old[j].is_some()
                && ok.task == k.task
                && ok.owners.iter().any(|o| k.owners.contains(o))
            {
                collected[i].push(j);
                old[j] = None;
            }
        }
    }
    for (i, k) in keys.iter().enumerate() {
        let Some(k) = k.as_ref().filter(|k| k.with || k.owners.is_empty()) else {
            continue;
        };
        if let Some(j) = (0..old.len()).find(|&j| {
            old[j].is_some()
                && old_keys[j].task == k.task
                && !old_keys[j].owners.iter().any(|o| k.owners.contains(o))
        }) {
            collected[i].push(j);
            old[j] = None;
        }
    }
    for (i, mut js) in collected.into_iter().enumerate() {
        if js.is_empty() {
            continue;
        }
        js.sort_unstable();
        taken[i] = Some(
            js.iter()
                .map(|&j| old_marks[j])
                .find(|m| *m != ' ')
                .unwrap_or(' '),
        );
    }
    let mut out = String::with_capacity(new_region.len());
    for ((line, n), mark) in lines.iter().zip(&new).zip(taken) {
        match (n, mark) {
            (Some((' ', _)), Some(mark)) => {
                out.push_str("- [");
                out.push(mark);
                // `- [ ` is four bytes: the rest (`] text` and the line ending) is kept.
                out.push_str(&line[4..]);
            }
            _ => out.push_str(line),
        }
    }
    out
}

/// Re-exports `rendered` into the existing note `existing` (PLAN.md 3.8): owned keys are
/// replaced in place (or appended), other keys kept verbatim, `tags` merged as a union
/// (existing order first); the managed region is replaced (keeping the marks the user set
/// on task lines whose text is unchanged, see `carry_task_marks`), or appended after the
/// user's text when both markers are gone; everything else is kept byte for byte, including its
/// line endings and a UTF-8 BOM.
///
/// # Errors
/// [`ExportError::Markers`] if exactly one marker is present, a marker repeats, or the end
/// marker comes first.
pub fn merge(existing: &str, rendered: &RenderedExport) -> Result<String, ExportError> {
    let (bom, doc) = match existing.strip_prefix(BOM) {
        Some(rest) => ("\u{feff}", rest),
        None => ("", existing),
    };
    let eol = dominant_eol(doc);
    let gen_text = |s: &str| {
        if eol == "\n" {
            s.to_owned()
        } else {
            s.replace('\n', eol)
        }
    };
    let parts = split_frontmatter(doc);

    let mut out = String::from(bom);
    out.push_str(parts.as_ref().map_or("", |p| p.open));
    if parts.is_none() {
        out.push_str("---");
        out.push_str(eol);
    }
    let mut written: Vec<String> = Vec::new();
    for b in parts.as_ref().map(|p| blocks(p.fm)).unwrap_or_default() {
        let Some(k) = b.key.as_deref() else {
            out.push_str(b.text);
            continue;
        };
        if k != "tags" && !OWNED_KEYS.contains(&k) {
            out.push_str(b.text);
            continue;
        }
        if written.iter().any(|w| w == k) {
            continue; // a repeated owned key is dropped
        }
        if k == "tags" {
            let mut tags = Vec::new();
            union_tags(&mut tags, parse_tags(b.text).iter().map(String::as_str));
            union_tags(&mut tags, rendered.tags.iter().map(String::as_str));
            out.push_str(&gen_text(&tags_block(&tags)));
        } else if let Some((_, t)) = rendered.frontmatter.iter().find(|(rk, _)| rk == k) {
            out.push_str(&gen_text(t));
        }
        written.push(k.to_owned());
    }
    if !out.ends_with('\n') {
        out.push_str(eol);
    }
    for (k, t) in &rendered.frontmatter {
        if !written.contains(k) {
            out.push_str(&gen_text(t));
        }
    }
    match &parts {
        Some(p) => {
            out.push_str(p.close);
            if !p.close.ends_with('\n') {
                out.push_str(eol);
            }
        }
        None => {
            out.push_str("---");
            out.push_str(eol);
        }
    }

    let body = parts.as_ref().map_or(doc, |p| p.body);
    let region = gen_text(&rendered.region);
    let begins = body.matches(BEGIN_MARKER).count();
    let ends = body.matches(END_MARKER).count();
    match (begins, ends) {
        (0, 0) => {
            if parts.is_none() && body.is_empty() {
                out.push_str(eol);
            }
            out.push_str(body);
            if !body.is_empty() {
                if !body.ends_with('\n') {
                    out.push_str(eol);
                }
                out.push_str(eol);
            }
            out.push_str(&region);
        }
        (1, 1) => {
            let b = body.find(BEGIN_MARKER).unwrap_or(0);
            let e = body.find(END_MARKER).unwrap_or(0);
            if e < b {
                return Err(ExportError::Markers("end marker before begin marker"));
            }
            let after = &body[e + END_MARKER.len()..];
            // The region ends with a line break; the old one's (if any) is replaced.
            let after = after
                .strip_prefix("\r\n")
                .or_else(|| after.strip_prefix('\n'))
                .unwrap_or(after);
            out.push_str(&body[..b]);
            out.push_str(&carry_task_marks(&body[b..e], &region));
            out.push_str(after);
        }
        (0, _) | (_, 0) => return Err(ExportError::Markers("only one region marker present")),
        _ => return Err(ExportError::Markers("region markers repeated")),
    }
    Ok(out)
}
