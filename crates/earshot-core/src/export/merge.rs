//! Re-export into an existing note: frontmatter merge and managed-region replacement.
//!
//! The merge works on the note's original text: every byte outside the owned frontmatter
//! keys and the managed region is copied as is, including its line endings (LF, CRLF or a
//! mix) and a leading UTF-8 byte-order mark. Generated text (owned keys, tags, the region,
//! separators) uses the note's dominant line ending.

use super::render::{
    RenderedAction, RenderedExport, name_key, owner_names, tags_block, union_tags,
};
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

/// Where a task line sits in a region: in "## Action items" under "### My actions", under
/// "### Other actions", in the one list (no subheadings, which includes every note written
/// before T14.61), or outside the action items.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Place {
    Mine,
    Other,
    Single,
    Elsewhere,
}

impl Place {
    /// Whether an old line at `self` and a new line at `new` can be the same action: the
    /// user's own lines only match the user's own, action lines never match lines from other
    /// sections, and a one-list line only when the old region has no "### My actions" or
    /// "### Other actions" at all (a line left above a subheading the user deleted could be
    /// anyone's).
    fn compatible(self, new: Place, old_has_subheadings: bool) -> bool {
        match (self, new) {
            (Place::Elsewhere, Place::Elsewhere) | (Place::Mine, Place::Mine) => true,
            (Place::Elsewhere, _) | (_, Place::Elsewhere) => false,
            (Place::Mine, _) | (_, Place::Mine) => false,
            (Place::Other, _) => true,
            (Place::Single, _) => !old_has_subheadings,
        }
    }
}

/// The place of every line of `region` (one entry per `split_inclusive('\n')` line).
fn places(region: &str) -> Vec<Place> {
    let mut place = Place::Elsewhere;
    region
        .split_inclusive('\n')
        .map(|l| {
            let l = l.trim_end_matches(['\n', '\r']);
            if l == "## Action items" {
                place = Place::Single;
            } else if place != Place::Elsewhere && l == "### My actions" {
                place = Place::Mine;
            } else if place != Place::Elsewhere && l == "### Other actions" {
                place = Place::Other;
            } else if l.starts_with("## ") || l == BEGIN_MARKER || l == END_MARKER {
                place = Place::Elsewhere;
            }
            place
        })
        .collect()
}

/// Whether `text` holds a balanced "(...)" group starting it or after a space, anywhere:
/// an owner list, which a task plugin may have followed with more text.
fn contains_group(text: &str) -> bool {
    text.char_indices().any(|(i, c)| {
        c == '(' && (i == 0 || text[..i].ends_with(' ')) && group_end(&text[i..]).is_some()
    })
}

/// The byte index just past the ")" closing the "(" that starts `text`.
fn group_end(text: &str) -> Option<usize> {
    let mut depth = 0usize;
    for (i, c) in text.char_indices() {
        match c {
            '(' => depth += 1,
            ')' => {
                depth = depth.checked_sub(1)?;
                if depth == 0 {
                    return Some(i + 1);
                }
            }
            _ => {}
        }
    }
    None
}

/// The names of a ", "-joined owner list as the merge compares them.
fn name_set(list: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for n in list.split(", ").map(name_key).filter(|n| !n.is_empty()) {
        if !out.contains(&n) {
            out.push(n);
        }
    }
    out.sort();
    out
}

/// One new action line as the render built it, in plain form and with names keyed.
struct NewAction {
    task: String,
    mine: bool,
    /// Every owner, the user's names included, keyed and sorted.
    owners: Vec<String>,
    /// The owners shown after "with" on a line of the user's own, keyed and sorted.
    others: Vec<String>,
}

impl NewAction {
    fn of(a: &RenderedAction) -> NewAction {
        let keyed = |names: &[String]| {
            name_set(
                &names
                    .iter()
                    .map(|n| plain_form(n))
                    .collect::<Vec<_>>()
                    .join(", "),
            )
        };
        NewAction {
            task: plain_form(&a.task),
            mine: a.mine,
            owners: keyed(&a.owners),
            others: keyed(&a.others),
        }
    }

    /// How old line `text` at `place` relates to this action (T14.61).
    ///
    /// The owner set is part of an action's identity: an old line matches only when its own
    /// owners, read from the line's form, are this action's. Ambiguity never carries a tick
    /// (losing one is acceptable, moving one is not), so the old line is read against this
    /// action's own task as rendered, never by stripping an arbitrary trailing bracket:
    /// - "**X**: <task>" (the form before T14.61, one line per owner text): matches when
    ///   <task> is this task exactly and every name in X is an owner here
    ///   ([`OldMatch::PerOwner`]; a shared action takes such lines only when together
    ///   they name exactly its owners);
    /// - under "### My actions": "<task>" (the user alone) or "<task> (with <names>)"
    ///   matches a line of the user's own with exactly those others;
    /// - elsewhere in the action items: "<task>" (no owner) or "<task> (<names>)" matches
    ///   when the names are exactly this action's owners, the user's names included, so a
    ///   one-list note still maps once a user name is set.
    ///
    /// A user name change: an old "My actions" line names the user by its place only, so it
    /// carries onto a line that is the user's now (a renamed user is still the user), and is
    /// lost when the user's name was cleared or no longer matches the owner text: the merge
    /// does not know the old name, and guessing could move a tick onto someone else's item.
    fn matches(&self, text: &str, place: Place) -> OldMatch {
        if let Some(after) = text.strip_prefix("**")
            && let Some(end) = after.find("**: ")
            && end > 0
        {
            let names = name_set(&owner_names(&after[..end]).join(", "));
            let fits = after[end + 4..] == self.task
                && !names.is_empty()
                && names.iter().all(|n| self.owners.contains(n));
            return if fits {
                OldMatch::PerOwner(names)
            } else {
                OldMatch::No
            };
        }
        let Some(rest) = text.strip_prefix(self.task.as_str()) else {
            return OldMatch::No;
        };
        let listed = if rest.is_empty() {
            Some("")
        } else {
            rest.strip_prefix(' ')
                .filter(|g| g.starts_with('(') && group_end(g) == Some(g.len()))
                .map(|g| &g[1..g.len() - 1])
        };
        let Some(listed) = listed else {
            return OldMatch::No;
        };
        let fits = match place {
            Place::Mine => {
                let others = match listed.strip_prefix("with ") {
                    Some(l) if !l.is_empty() => name_set(l),
                    Some(_) => return OldMatch::No,
                    None if listed.is_empty() => Vec::new(),
                    None => return OldMatch::No,
                };
                self.mine && others == self.others
            }
            Place::Other | Place::Single => name_set(listed) == self.owners,
            Place::Elsewhere => false,
        };
        if fits { OldMatch::Whole } else { OldMatch::No }
    }
}

/// How an old line matches a new action.
#[derive(Debug, PartialEq, Eq)]
enum OldMatch {
    No,
    /// The same action in the form since T14.61 (one line).
    Whole,
    /// One owner's line of it in the form before T14.61, with the names it holds (several
    /// may match; together they must name exactly the action's owners).
    PerOwner(Vec<String>),
}

/// The new managed region with the task marks the user set in the old one (T14.27).
///
/// Earshot renders action items as unticked `- [ ]` tasks inside the region, which a
/// re-export replaces whole. Matching runs in three passes over the task lines of the new
/// region, each old task line used at most once, first come first served. An action line
/// matches only an action line, and a line of the user's own only a line of the user's own
/// (`Place::compatible`): the owner set is part of an action's identity in every pass.
///
/// 1. Exact: every new task line, whatever its own mark, takes the first unused old line
///    with the same text after the checkbox. Repeated identical lines pair up in order,
///    and a line whose generated text already holds a mark (`- [x] foo` from a bullet
///    reading `[x] foo`) uses up its own old copy, so merging a render into its own
///    output stays the identity.
/// 2. Appended: a new unticked line still unmatched takes the first unused old line
///    whose text starts with the new text and a space (a task plugin added a done date
///    or a tag after the text). On an action line the rest must hold no balanced "(...)"
///    anywhere: that is an owner list (perhaps followed by a plugin's date), so another
///    owner set; a Dataview `(key:: value)` field loses its tick the same way, which is
///    accepted (lost, never moved). Only that direction: an old line that is a prefix of the
///    new text is another, longer item and does not match.
/// 3. Owners (T14.61): a new unticked action line still unmatched takes the old lines
///    that are the same action in another form ([`NewAction::matches`]): one line written
///    since T14.61, or the owners' lines written before when together they name exactly
///    its owners. A shared action is ticked if any line it took was ticked, with the mark of
///    the first ticked one in the old order.
///
/// Only an unticked new line (`[ ]`) is rewritten, to the matched line's mark exactly as
/// the user left it (`x`, `X` or another plugin's character); appended text is not
/// carried. Texts are compared in their plain form (`plain_form`), so a line written
/// before or after a change to the escaping rule matches its re-render. Only text between
/// the old markers is read; every other byte of the new region is unchanged, so with no
/// marks set the result is the new region itself.
fn carry_task_marks(old_region: &str, new_region: &str, actions: &[RenderedAction]) -> String {
    let old_places = places(old_region);
    let mut old: Vec<Option<(char, String, Place)>> = old_region
        .split_inclusive('\n')
        .zip(&old_places)
        .filter_map(|(l, p)| task_line(l).map(|(m, t)| Some((m, plain_form(t), *p))))
        .collect();
    let old_marks: Vec<char> = old.iter().flatten().map(|(m, _, _)| *m).collect();
    let subheadings = old_places
        .iter()
        .any(|p| matches!(p, Place::Mine | Place::Other));
    if old_marks.iter().all(|m| *m == ' ') {
        // Nothing marked: the new region as rendered.
        return new_region.to_owned();
    }
    let lines: Vec<&str> = new_region.split_inclusive('\n').collect();
    let new_places = places(new_region);
    let new: Vec<Option<(char, String, Place)>> = lines
        .iter()
        .zip(&new_places)
        .map(|(l, p)| task_line(l).map(|(m, t)| (m, plain_form(t), *p)))
        .collect();
    // The render's own action for each new action line, in order; none at all when the
    // region and the list disagree (nothing is then carried by owners).
    let mut action_of: Vec<Option<NewAction>> = Vec::with_capacity(lines.len());
    {
        let mut next = actions.iter();
        let mut agree = true;
        for n in &new {
            match n {
                Some((_, text, p)) if *p != Place::Elsewhere => {
                    let a = next.next();
                    agree &= a.is_some_and(|a| plain_form(&a.text) == *text);
                    action_of.push(a.map(NewAction::of));
                }
                _ => action_of.push(None),
            }
        }
        if !agree || next.next().is_some() {
            action_of.iter_mut().for_each(|a| *a = None);
        }
    }
    // The old mark each new line takes (pass 1, then pass 2, then pass 3).
    let mut taken: Vec<Option<char>> = vec![None; lines.len()];
    let mut take = |matches: &dyn Fn(&str, Place) -> bool| {
        let slot = old
            .iter_mut()
            .find(|o| o.as_ref().is_some_and(|(_, t, p)| matches(t, *p)))?;
        slot.take().map(|(m, _, _)| m)
    };
    for (i, n) in new.iter().enumerate() {
        if let Some((_, text, place)) = n {
            taken[i] = take(&|t: &str, p: Place| t == text && p.compatible(*place, subheadings));
        }
    }
    for (i, n) in new.iter().enumerate() {
        if let Some((' ', text, place)) = n
            && taken[i].is_none()
        {
            taken[i] = take(&|t: &str, p: Place| {
                p.compatible(*place, subheadings)
                    && t.strip_prefix(text.as_str())
                        .and_then(|rest| rest.strip_prefix(' '))
                        .is_some_and(|rest| *place == Place::Elsewhere || !contains_group(rest))
            });
        }
    }
    for (i, n) in new.iter().enumerate() {
        let (Some((' ', _, _)), None, Some(action)) = (n, taken[i], &action_of[i]) else {
            continue;
        };
        let mut collected: Vec<usize> = Vec::new();
        let mut per_owner: Vec<usize> = Vec::new();
        let mut named: Vec<String> = Vec::new();
        for (j, o) in old.iter().enumerate() {
            let Some((_, text, place)) = o else { continue };
            if *place == Place::Elsewhere || (*place == Place::Single && subheadings) {
                continue;
            }
            match action.matches(text, *place) {
                OldMatch::Whole if collected.is_empty() => collected.push(j),
                OldMatch::PerOwner(names) => {
                    per_owner.push(j);
                    named.extend(names);
                }
                _ => {}
            }
        }
        named.sort();
        named.dedup();
        if named == action.owners {
            collected.extend(per_owner);
        }
        collected.sort_unstable();
        if collected.is_empty() {
            continue;
        }
        for &j in &collected {
            old[j] = None;
        }
        taken[i] = Some(
            collected
                .iter()
                .map(|&j| old_marks[j])
                .find(|m| *m != ' ')
                .unwrap_or(' '),
        );
    }
    let mut out = String::with_capacity(new_region.len());
    for ((line, n), mark) in lines.iter().zip(&new).zip(taken) {
        match (n, mark) {
            (Some((' ', _, _)), Some(mark)) => {
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
            out.push_str(&carry_task_marks(&body[b..e], &region, &rendered.actions));
            out.push_str(after);
        }
        (0, _) | (_, 0) => return Err(ExportError::Markers("only one region marker present")),
        _ => return Err(ExportError::Markers("region markers repeated")),
    }
    Ok(out)
}
