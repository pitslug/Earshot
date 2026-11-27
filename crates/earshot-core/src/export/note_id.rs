//! Reading a note's `earshot_id` (T14.63): the export writer reads it from the one file at
//! the path it is about to write, and replaces that file only when it names this meeting.
//! It is all an export reads from an existing note; nothing searches the vault.

use crate::identity::EntityId;

const BOM: char = '\u{feff}';

fn is_fence(line: &str) -> bool {
    line.trim_end_matches(['\n', '\r']) == "---"
}

/// The frontmatter between the opening and closing `---` lines; `None` when the note does
/// not start with a closed frontmatter block.
fn frontmatter(doc: &str) -> Option<&str> {
    let mut lines = doc.split_inclusive('\n');
    let open = lines.next()?;
    if !is_fence(open) || !open.ends_with('\n') {
        return None;
    }
    let mut pos = open.len();
    for line in lines {
        if is_fence(line) {
            return Some(&doc[open.len()..pos]);
        }
        pos += line.len();
    }
    None
}

fn unquote(s: &str) -> &str {
    let s = s.trim();
    s.strip_prefix('"')
        .and_then(|x| x.strip_suffix('"'))
        .or_else(|| s.strip_prefix('\'').and_then(|x| x.strip_suffix('\'')))
        .unwrap_or(s)
}

/// The value of a top-level `key: value` line whose key is `key`.
fn value_of<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    let first = line.chars().next()?;
    if first.is_whitespace() || matches!(first, '-' | '#') {
        return None;
    }
    let (k, v) = line.split_once(':')?;
    (unquote(k) == key).then_some(v)
}

/// The `earshot_id` in `doc`'s frontmatter (any line endings, optional BOM), if present
/// and valid. Only the first top-level `earshot_id` line counts.
#[must_use]
pub fn earshot_id_of(doc: &str) -> Option<EntityId> {
    let doc = doc.strip_prefix(BOM).unwrap_or(doc);
    let v = frontmatter(doc)?
        .lines()
        .find_map(|l| value_of(l, "earshot_id"))?;
    unquote(v).parse().ok()
}
