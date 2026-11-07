//! Re-export into an existing note: frontmatter merge and managed-region replacement.
//!
//! The merge works on the note's original text: every byte outside the owned frontmatter
//! keys and the managed region is copied as is, including its line endings (LF, CRLF or a
//! mix) and a leading UTF-8 byte-order mark. Generated text (owned keys, tags, the region,
//! separators) uses the note's dominant line ending.

use super::render::{RenderedExport, tags_block, union_tags};
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

/// Re-exports `rendered` into the existing note `existing` (PLAN.md 3.8): owned keys are
/// replaced in place (or appended), other keys kept verbatim, `tags` merged as a union
/// (existing order first); the managed region is replaced, or appended after the user's
/// text when both markers are gone; everything else is kept byte for byte, including its
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
            out.push_str(&region);
            out.push_str(after);
        }
        (0, _) | (_, 0) => return Err(ExportError::Markers("only one region marker present")),
        _ => return Err(ExportError::Markers("region markers repeated")),
    }
    Ok(out)
}
