//! What the model writes and how it becomes a [`SummaryV1`] (T6.20).
//!
//! Prompts number the transcript segments of a meeting 1, 2, 3, ... in meeting order and
//! the model cites those numbers: a segment UUID costs about 25 to 30 tokens in common
//! tokenizers (each hex digit is often its own token), a number one or two. The reply
//! follows [`llm_json_schema`]: the summary v1 shape with `source_segment_ids` holding at
//! most [`MAX_CITATIONS`] positive integers. [`validate_reply`] turns a reply into the
//! stored [`SummaryV1`] (numbers mapped back to segment ids, so the stored summary and
//! `schemas/summary.v1.json` are unchanged) and checks it: the numbers a request may cite,
//! the citation cap, every rule of [`validate_value`], and repetition: a text that comes
//! [`REPEAT_LIMIT`] times in one list (bullets per section, action items by owner and
//! task) is how looping replies show up and is an error ([`repetition_errors`]); one that
//! comes twice is a duplicate, the later copy is dropped ([`drop_repeats`]) and counted in
//! [`CheckedReply::duplicates_dropped`]. The duplicate rule was first an error too; on the AMI dev
//! split it failed whole meetings over one repeated topic ("User Interface" and "User
//! interface") that three repair rounds of greedy decoding never removed (T6.20,
//! docs/summary-eval.md).

use std::collections::{BTreeMap, BTreeSet};
use std::ops::Range;

use serde_json::{Map, Value, json};

use super::validate::{SummaryError, SummaryErrorKind, validate_value};
use super::{SummaryV1, json_schema};
use crate::identity::EntityId;

/// Repo-relative path of the committed JSON Schema of model replies.
pub const SUMMARY_LLM_SCHEMA_PATH: &str = "schemas/summary-llm.v1.json";

/// Most segments one bullet, decision or action item may cite.
pub const MAX_CITATIONS: usize = 3;

/// The JSON Schema of a model reply: [`json_schema`] with every `source_segment_ids`
/// holding at most [`MAX_CITATIONS`] segment numbers (integers from 1), and without
/// `description`, `title` and `$schema` annotations (the system prompt explains the fields;
/// the annotations made the prompt about 40% longer). Committed at
/// [`SUMMARY_LLM_SCHEMA_PATH`].
#[must_use]
pub fn llm_json_schema() -> Value {
    let mut s = json_schema();
    rewrite(&mut s);
    s
}

/// Rewrites one schema object (and the schemas below it). Keys of `properties` and `$defs`
/// maps are names, not annotations, and are kept.
fn rewrite(v: &mut Value) {
    let Value::Object(map) = v else {
        return;
    };
    map.remove("description");
    map.remove("title");
    map.remove("$schema");
    for (key, child) in map.iter_mut() {
        match key.as_str() {
            "properties" | "$defs" => {
                if let Value::Object(named) = child {
                    if let Some(c) = named.get_mut("source_segment_ids")
                        && key == "properties"
                    {
                        *c = json!({
                            "type": "array",
                            "items": {"type": "integer", "minimum": 1},
                            "maxItems": MAX_CITATIONS,
                        });
                    }
                    named.values_mut().for_each(rewrite);
                }
            }
            "items" => rewrite(child),
            "anyOf" | "oneOf" | "allOf" => {
                if let Value::Array(list) = child {
                    list.iter_mut().for_each(rewrite);
                }
            }
            _ => {}
        }
    }
}

/// The numbering of a meeting's transcript segments: segment `n` (from 1) is `ids[n - 1]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SegmentNumbers {
    ids: Vec<EntityId>,
    numbers: BTreeMap<EntityId, usize>,
}

impl SegmentNumbers {
    /// Numbers `ids` 1, 2, 3, ... in the given (meeting) order.
    #[must_use]
    pub fn new(ids: impl IntoIterator<Item = EntityId>) -> Self {
        let ids: Vec<EntityId> = ids.into_iter().collect();
        let numbers = ids.iter().enumerate().map(|(i, id)| (*id, i + 1)).collect();
        SegmentNumbers { ids, numbers }
    }

    /// The number of segment `id`.
    #[must_use]
    pub fn number_of(&self, id: &EntityId) -> Option<usize> {
        self.numbers.get(id).copied()
    }

    /// The segment numbered `n`.
    #[must_use]
    pub fn id_of(&self, n: usize) -> Option<EntityId> {
        n.checked_sub(1).and_then(|i| self.ids.get(i)).copied()
    }

    /// Number of segments.
    #[must_use]
    pub fn len(&self) -> usize {
        self.ids.len()
    }

    /// Whether there are none.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.ids.is_empty()
    }
}

/// The transcript segments a request's reply may cite, as indices into the meeting's
/// segments (segment `i` is number `i + 1`): a map part's own range, the whole meeting for
/// a single request, or for a merge the segments its partial summaries cite.
#[derive(Clone, PartialEq, Eq, Default)]
pub struct Citable(BTreeSet<usize>);

impl Citable {
    /// Every index of `indices`.
    #[must_use]
    pub fn new(indices: impl IntoIterator<Item = usize>) -> Self {
        Citable(indices.into_iter().collect())
    }

    /// Every segment cited by `summaries` (ids outside `numbers` are left out).
    #[must_use]
    pub fn cited_by<'a>(
        summaries: impl IntoIterator<Item = &'a SummaryV1>,
        numbers: &SegmentNumbers,
    ) -> Self {
        let mut set = BTreeSet::new();
        for s in summaries {
            let ids = s
                .enhanced_notes
                .iter()
                .flat_map(|sec| sec.bullets.iter())
                .flat_map(|b| b.source_segment_ids.iter())
                .chain(s.decisions.iter().flat_map(|d| d.source_segment_ids.iter()))
                .chain(
                    s.action_items
                        .iter()
                        .flat_map(|a| a.source_segment_ids.iter()),
                );
            set.extend(ids.filter_map(|id| numbers.number_of(id)).map(|n| n - 1));
        }
        Citable(set)
    }

    /// Whether segment index `i` may be cited.
    #[must_use]
    pub fn contains(&self, i: usize) -> bool {
        self.0.contains(&i)
    }

    /// The citable indices in order.
    pub fn iter(&self) -> impl Iterator<Item = usize> + '_ {
        self.0.iter().copied()
    }

    /// Number of citable segments.
    #[must_use]
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Whether nothing may be cited.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// The citable indices as runs of consecutive indices.
    #[must_use]
    pub fn runs(&self) -> Vec<Range<usize>> {
        let mut runs: Vec<Range<usize>> = Vec::new();
        for i in self.iter() {
            match runs.last_mut() {
                Some(r) if r.end == i => r.end = i + 1,
                _ => runs.push(i..i + 1),
            }
        }
        runs
    }
}

impl From<Range<usize>> for Citable {
    fn from(r: Range<usize>) -> Self {
        Citable::new(r)
    }
}

/// Runs, so a range reads as `0..4` and prompt snapshots stay short.
impl std::fmt::Debug for Citable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.runs().as_slice() {
            [] => write!(f, "{{}}"),
            [one] => write!(f, "{one:?}"),
            many => f.debug_set().entries(many).finish(),
        }
    }
}

/// `summary` as the model sees it in a merge (reduce) prompt: its JSON with each cited id
/// replaced by its segment number (ids outside `numbers` are left out).
#[must_use]
pub fn to_numbered(summary: &SummaryV1, numbers: &SegmentNumbers) -> Value {
    let mut v = serde_json::to_value(summary).unwrap_or(Value::Null);
    for_each_citation_list(&mut v, |_, list| {
        let mapped: Vec<Value> = list
            .iter()
            .filter_map(|x| x.as_str()?.parse::<EntityId>().ok())
            .filter_map(|id| numbers.number_of(&id))
            .map(|n| json!(n))
            .collect();
        *list = mapped;
    });
    v
}

/// [`to_numbered`] as JSON text in the field order of [`SummaryV1`] (title first, as the
/// model writes it), for merge prompts.
#[must_use]
pub fn to_numbered_json(summary: &SummaryV1, numbers: &SegmentNumbers) -> String {
    let mut s = summary.clone();
    // Ids outside the numbering are dropped, as in `to_numbered`.
    let keep = |ids: &mut Vec<EntityId>| ids.retain(|id| numbers.number_of(id).is_some());
    s.enhanced_notes
        .iter_mut()
        .flat_map(|sec| sec.bullets.iter_mut())
        .for_each(|b| keep(&mut b.source_segment_ids));
    s.decisions
        .iter_mut()
        .for_each(|d| keep(&mut d.source_segment_ids));
    s.action_items
        .iter_mut()
        .for_each(|a| keep(&mut a.source_segment_ids));
    let mut text = serde_json::to_string(&s).unwrap_or_default();
    // Every remaining id is quoted exactly once per citation; ids are fixed-length UUIDs, so
    // one cannot occur inside another.
    let mut ids: BTreeSet<EntityId> = BTreeSet::new();
    ids.extend(
        s.enhanced_notes
            .iter()
            .flat_map(|sec| sec.bullets.iter())
            .flat_map(|b| b.source_segment_ids.iter().copied()),
    );
    ids.extend(
        s.decisions
            .iter()
            .flat_map(|d| d.source_segment_ids.iter().copied()),
    );
    ids.extend(
        s.action_items
            .iter()
            .flat_map(|a| a.source_segment_ids.iter().copied()),
    );
    for id in ids {
        if let Some(n) = numbers.number_of(&id) {
            text = text.replace(&format!("\"{id}\""), &n.to_string());
        }
    }
    text
}

/// Calls `f(path, list)` for every `source_segment_ids` array of a summary-shaped value.
fn for_each_citation_list(v: &mut Value, mut f: impl FnMut(String, &mut Vec<Value>)) {
    let Some(root) = v.as_object_mut() else {
        return;
    };
    let mut visit = |obj: &mut Map<String, Value>, path: String| {
        if let Some(Value::Array(list)) = obj.get_mut("source_segment_ids") {
            f(format!("{path}/source_segment_ids"), list);
        }
    };
    if let Some(Value::Array(sections)) = root.get_mut("enhanced_notes") {
        for (i, s) in sections.iter_mut().enumerate() {
            if let Some(Value::Array(bullets)) = s.get_mut("bullets") {
                for (j, b) in bullets.iter_mut().enumerate() {
                    if let Some(obj) = b.as_object_mut() {
                        visit(obj, format!("/enhanced_notes/{i}/bullets/{j}"));
                    }
                }
            }
        }
    }
    for key in ["decisions", "action_items"] {
        if let Some(Value::Array(items)) = root.get_mut(key) {
            for (i, item) in items.iter_mut().enumerate() {
                if let Some(obj) = item.as_object_mut() {
                    visit(obj, format!("/{key}/{i}"));
                }
            }
        }
    }
}

/// A reply that passed [`validate_reply`]: the stored summary and how many repeated items
/// [`drop_repeats`] removed from it (callers report the count).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckedReply {
    /// The summary with segment ids.
    pub summary: SummaryV1,
    /// Items dropped as second copies ([`drop_repeats`]).
    pub duplicates_dropped: usize,
}

/// Validates a model reply (module docs): citations must be integers numbering segments
/// whose index (from 0) is in `citable` (a map request may cite only its own part, a merge
/// only what its partial summaries cite), at most [`MAX_CITATIONS`] per item and each
/// number once; then every rule of [`validate_value`], then [`repetition_errors`]; a valid
/// reply then has its second copies dropped ([`drop_repeats`]). Returns the summary with
/// segment ids and the number of items dropped, or every error found (paths into the
/// reply).
pub fn validate_reply(
    value: &Value,
    numbers: &SegmentNumbers,
    citable: &Citable,
) -> Result<CheckedReply, Vec<SummaryError>> {
    let mut v = value.clone();
    let mut errors = Vec::new();
    // Citation lists with a wrong citation (their exact paths).
    let mut wrong_lists: BTreeSet<String> = BTreeSet::new();
    for_each_citation_list(&mut v, |path, list| {
        if list.len() > MAX_CITATIONS {
            errors.push(SummaryError {
                path: path.clone(),
                kind: SummaryErrorKind::TooManyCitations(list.len()),
            });
        }
        let mut mapped = Vec::with_capacity(list.len());
        let mut seen = BTreeSet::new();
        for (i, item) in list.iter().enumerate() {
            let p = format!("{path}/{i}");
            let kind = match item.as_u64() {
                Some(n) if !seen.insert(n) => SummaryErrorKind::DuplicateSegmentNumber(n),
                Some(n) => {
                    let id = usize::try_from(n)
                        .ok()
                        .filter(|&k| k >= 1 && citable.contains(k - 1))
                        .and_then(|k| numbers.id_of(k));
                    match id {
                        Some(id) => {
                            mapped.push(Value::String(id.to_string()));
                            continue;
                        }
                        None => SummaryErrorKind::UnknownSegmentNumber(n),
                    }
                }
                None => SummaryErrorKind::InvalidSegmentNumber(item.to_string()),
            };
            wrong_lists.insert(path.clone());
            errors.push(SummaryError { path: p, kind });
        }
        // Wrong citations are reported above and left out here, so the checks below do
        // not report them a second time.
        *list = mapped;
    });
    let allowed: BTreeSet<EntityId> = citable
        .iter()
        .filter_map(|i| numbers.id_of(i + 1))
        .collect();
    match validate_value(&v, &allowed) {
        Ok(mut summary) if errors.is_empty() => {
            let rep = repetition_errors(&summary);
            if rep.is_empty() {
                let duplicates_dropped = drop_repeats(&mut summary);
                Ok(CheckedReply {
                    summary,
                    duplicates_dropped,
                })
            } else {
                Err(rep)
            }
        }
        Ok(summary) => {
            errors.extend(repetition_errors(&summary));
            Err(errors)
        }
        Err(more) => {
            // An item whose every citation was wrong would also be reported as uncited:
            // that list's own path, matched exactly.
            errors.extend(more.into_iter().filter(|e| {
                !(e.kind == SummaryErrorKind::Uncited && wrong_lists.contains(&e.path))
            }));
            // Repetition is reported with the other errors whenever the shape allows it.
            if let Ok(summary) = serde_json::from_value::<SummaryV1>(v) {
                errors.extend(repetition_errors(&summary));
            }
            Err(errors)
        }
    }
}

/// The text of `s` compared for repetition: lower case, whitespace collapsed, trailing
/// punctuation dropped.
fn normal(s: &str) -> String {
    let t = s
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase();
    t.trim_end_matches(|c: char| c.is_ascii_punctuation())
        .to_string()
}

/// The repetition key of an action item: its owner and task (two people may each own the
/// same task).
fn action_key(a: &super::ActionItem) -> String {
    format!("{}\u{1f}{}", normal(&a.owner), normal(&a.task))
}

/// How many times one item may come in one list before the reply counts as looping
/// ([`repetition_errors`]).
pub const REPEAT_LIMIT: usize = 3;

/// Looping inside one list of a summary: the same item [`REPEAT_LIMIT`] or more times in one
/// section's bullets, among the section headings, decisions, action items (owner and task),
/// open questions or topics, compared after [`normal`]-isation; each copy from the
/// [`REPEAT_LIMIT`]th on is reported. Bullets of sections with the same heading count as
/// one section ([`drop_repeats`] merges them). A fact may still appear once as a bullet and
/// once as a decision, or under two headings; an item that comes twice is dropped by
/// [`drop_repeats`].
#[must_use]
pub fn repetition_errors(summary: &SummaryV1) -> Vec<SummaryError> {
    let mut errors = Vec::new();
    let mut check = |items: Vec<(String, String)>| {
        let mut seen: BTreeMap<String, usize> = BTreeMap::new();
        for (path, key) in items {
            let n = seen.entry(key).or_default();
            *n += 1;
            if *n >= REPEAT_LIMIT {
                errors.push(SummaryError {
                    path,
                    kind: SummaryErrorKind::Repetition,
                });
            }
        }
    };
    // Bullets per section (sections grouped by heading).
    let mut by_heading: BTreeMap<String, Vec<(String, String)>> = BTreeMap::new();
    for (i, s) in summary.enhanced_notes.iter().enumerate() {
        let list = by_heading.entry(normal(&s.heading)).or_default();
        for (j, b) in s.bullets.iter().enumerate() {
            list.push((
                format!("/enhanced_notes/{i}/bullets/{j}/text"),
                normal(&b.text),
            ));
        }
    }
    for list in by_heading.into_values() {
        check(list);
    }
    check(
        summary
            .enhanced_notes
            .iter()
            .enumerate()
            .map(|(i, s)| (format!("/enhanced_notes/{i}/heading"), normal(&s.heading)))
            .collect(),
    );
    check(
        summary
            .decisions
            .iter()
            .enumerate()
            .map(|(i, d)| (format!("/decisions/{i}/text"), normal(&d.text)))
            .collect(),
    );
    check(
        summary
            .action_items
            .iter()
            .enumerate()
            .map(|(i, a)| (format!("/action_items/{i}/task"), action_key(a)))
            .collect(),
    );
    check(
        summary
            .open_questions
            .iter()
            .enumerate()
            .map(|(i, q)| (format!("/open_questions/{i}"), normal(q)))
            .collect(),
    );
    check(
        summary
            .topics
            .iter()
            .enumerate()
            .map(|(i, t)| (format!("/topics/{i}"), normal(t)))
            .collect(),
    );
    errors
}

/// Drops later copies of an item within one list (the lists and keys of
/// [`repetition_errors`]) and returns how many items were dropped. A section whose heading
/// repeats an earlier one has its bullets moved to the earlier section (the merged heading
/// counts as dropped) and bullets are compared within a section only, so every section
/// keeps its first bullet.
pub fn drop_repeats(summary: &mut SummaryV1) -> usize {
    fn keep_first<T>(list: &mut Vec<T>, key: impl Fn(&T) -> String) -> usize {
        let before = list.len();
        let mut seen = BTreeSet::new();
        list.retain(|x| seen.insert(key(x)));
        before - list.len()
    }
    let mut dropped = 0;
    let mut sections: Vec<super::NoteSection> = Vec::new();
    for s in std::mem::take(&mut summary.enhanced_notes) {
        let h = normal(&s.heading);
        if let Some(first) = sections.iter_mut().find(|f| normal(&f.heading) == h) {
            first.bullets.extend(s.bullets);
            dropped += 1;
        } else {
            sections.push(s);
        }
    }
    for s in &mut sections {
        dropped += keep_first(&mut s.bullets, |b| normal(&b.text));
    }
    summary.enhanced_notes = sections;
    dropped += keep_first(&mut summary.decisions, |d| normal(&d.text));
    dropped += keep_first(&mut summary.action_items, action_key);
    dropped += keep_first(&mut summary.open_questions, |q| normal(q));
    dropped += keep_first(&mut summary.topics, |t| normal(t));
    dropped
}

/// The first words of a summary's `tldr` when it was joined by [`join_partials`]: the summary
/// says so itself, in the app and in an export.
pub const JOINED_SUMMARY_NOTICE: &str = "Joined from parts of the meeting, because the final \
     merge did not finish, so this summary may repeat itself.";

/// One summary from `parts` in order, for a merge step that gave no valid reply (T8.4d): a
/// rougher summary rather than none. The first title, the [`JOINED_SUMMARY_NOTICE`] and every
/// part's `tldr`, and every list joined with exact repeats dropped ([`drop_repeats`]). No text
/// is invented and every citation is a part's own. Never used for a part that failed: each
/// part here is a validated summary.
#[must_use]
pub fn join_partials(parts: &[SummaryV1]) -> SummaryV1 {
    let mut tldr = JOINED_SUMMARY_NOTICE.to_owned();
    let mut joined = SummaryV1 {
        title: parts.first().map_or_else(String::new, |p| p.title.clone()),
        tldr: String::new(),
        enhanced_notes: Vec::new(),
        decisions: Vec::new(),
        action_items: Vec::new(),
        open_questions: Vec::new(),
        topics: Vec::new(),
    };
    for p in parts {
        tldr.push(' ');
        tldr.push_str(p.tldr.trim());
        joined
            .enhanced_notes
            .extend(p.enhanced_notes.iter().cloned());
        joined.decisions.extend(p.decisions.iter().cloned());
        joined.action_items.extend(p.action_items.iter().cloned());
        joined
            .open_questions
            .extend(p.open_questions.iter().cloned());
        joined.topics.extend(p.topics.iter().cloned());
    }
    joined.tldr = tldr;
    drop_repeats(&mut joined);
    joined
}

/// Drops every action item that repeats an earlier one across `partials` (in order: the
/// first copy is kept, later ones go, whatever they cite), with the key [`drop_repeats`]
/// uses for action items (owner and task after normalising), and returns how many were
/// dropped. For the two-call chunked evaluation (T6.20c, evaluation only): exact repeats are
/// removed in code before the merge, and reworded ones are left to the merge request.
pub fn drop_repeated_actions(partials: &mut [SummaryV1]) -> usize {
    let mut seen = BTreeSet::new();
    let mut dropped = 0;
    for p in partials {
        let before = p.action_items.len();
        p.action_items.retain(|a| seen.insert(action_key(a)));
        dropped += before - p.action_items.len();
    }
    dropped
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(n: u32) -> EntityId {
        format!("0193a5b0-1c00-7000-8000-{n:012x}").parse().unwrap()
    }

    fn numbers() -> SegmentNumbers {
        SegmentNumbers::new((1..=5).map(id))
    }

    /// Every segment of [`numbers`] may be cited.
    fn all() -> Citable {
        Citable::from(0..5)
    }

    #[test]
    fn summary_join_keeps_every_part_says_so_and_drops_only_exact_repeats() {
        let part = |title: &str, tldr: &str, decision: &str, seg: u32| SummaryV1 {
            title: title.into(),
            tldr: tldr.into(),
            enhanced_notes: vec![super::super::NoteSection {
                heading: "Design".into(),
                bullets: vec![super::super::NoteBullet {
                    text: format!("note {seg}"),
                    source_segment_ids: vec![id(seg)],
                }],
            }],
            decisions: vec![super::super::Decision {
                text: decision.into(),
                source_segment_ids: vec![id(seg)],
            }],
            action_items: vec![super::super::ActionItem {
                owner: "Ana".into(),
                task: "Send the notes".into(),
                due: None,
                source_segment_ids: vec![id(seg)],
            }],
            open_questions: vec!["When?".into()],
            topics: vec![format!("topic {seg}")],
        };
        let j = join_partials(&[
            part("First", "One.", "Use steel.", 1),
            part("Second", "Two.", "Ship in June.", 2),
        ]);
        assert_eq!(j.title, "First");
        assert_eq!(j.tldr, format!("{JOINED_SUMMARY_NOTICE} One. Two."));
        // Both decisions, in order, with their own citations.
        assert_eq!(j.decisions.len(), 2);
        assert_eq!(j.decisions[1].text, "Ship in June.");
        assert_eq!(j.decisions[1].source_segment_ids, vec![id(2)]);
        // The two sections with one heading become one, and neither bullet is lost.
        assert_eq!(j.enhanced_notes.len(), 1);
        assert_eq!(j.enhanced_notes[0].bullets.len(), 2);
        // Exact repeats go.
        assert_eq!(j.action_items.len(), 1);
        assert_eq!(j.open_questions.len(), 1);
        assert_eq!(j.topics.len(), 2);
        // It passes the same checks as a merged summary.
        assert!(repetition_errors(&j).is_empty());
        let ids: Vec<_> = (1..=5).map(id).collect();
        assert!(
            super::super::validate_summary(&j, &ids[..]).is_empty(),
            "{j:?}"
        );
    }

    fn reply(bullet_cites: Value, decision_cites: Value) -> Value {
        json!({
            "title": "T", "tldr": "x",
            "enhanced_notes": [{"heading": "H", "bullets": [
                {"text": "b", "source_segment_ids": bullet_cites}]}],
            "decisions": [{"text": "d", "source_segment_ids": decision_cites}],
            "action_items": [{"owner": "Me", "task": "t", "due": null,
                              "source_segment_ids": [5]}],
            "open_questions": [], "topics": ["a"],
        })
    }

    #[test]
    fn summary_reply_numbers_map_back_to_segment_ids() {
        let s = validate_reply(&reply(json!([1, 2]), json!([3])), &numbers(), &all())
            .unwrap()
            .summary;
        assert_eq!(
            s.enhanced_notes[0].bullets[0].source_segment_ids,
            [id(1), id(2)]
        );
        assert_eq!(s.decisions[0].source_segment_ids, [id(3)]);
        assert_eq!(s.action_items[0].source_segment_ids, [id(5)]);
        // to_numbered is the inverse (for merge prompts).
        let back = to_numbered(&s, &numbers());
        assert_eq!(back["decisions"][0]["source_segment_ids"], json!([3]));
        assert_eq!(
            back["enhanced_notes"][0]["bullets"][0]["source_segment_ids"],
            json!([1, 2])
        );
        // The text form keeps the field order and is the same value.
        let text = to_numbered_json(&s, &numbers());
        assert!(text.starts_with("{\"title\":\"T\",\"tldr\":"), "{text}");
        assert_eq!(serde_json::from_str::<Value>(&text).unwrap(), back);
    }

    #[test]
    fn summary_reply_citation_errors_are_reported_by_number() {
        let errs = validate_reply(
            &reply(
                json!(["0193a5b0-1c00-7000-8000-000000000001", 9]),
                json!([0]),
            ),
            &numbers(),
            &all(),
        )
        .unwrap_err();
        let kinds: Vec<_> = errs.iter().map(|e| (e.path.as_str(), &e.kind)).collect();
        assert!(kinds.contains(&(
            "/enhanced_notes/0/bullets/0/source_segment_ids/0",
            &SummaryErrorKind::InvalidSegmentNumber(
                "\"0193a5b0-1c00-7000-8000-000000000001\"".into()
            )
        )));
        assert!(kinds.contains(&(
            "/enhanced_notes/0/bullets/0/source_segment_ids/1",
            &SummaryErrorKind::UnknownSegmentNumber(9)
        )));
        assert!(kinds.contains(&(
            "/decisions/0/source_segment_ids/0",
            &SummaryErrorKind::UnknownSegmentNumber(0)
        )));
        // The decision whose only citation was wrong is not reported again as uncited.
        assert!(
            !errs.iter().any(|e| e.kind == SummaryErrorKind::Uncited),
            "{errs:?}"
        );
        // A map request may cite only its own part.
        let part =
            validate_reply(&reply(json!([1]), json!([3])), &numbers(), &(2..5).into()).unwrap_err();
        assert_eq!(
            part[0].kind,
            SummaryErrorKind::UnknownSegmentNumber(1),
            "{part:?}"
        );
        // An empty citation list on a decision is still uncited.
        let none = validate_reply(&reply(json!([]), json!([])), &numbers(), &all()).unwrap_err();
        assert_eq!(none[0].kind, SummaryErrorKind::Uncited);
    }

    #[test]
    fn summary_reply_caps_citations() {
        let errs = validate_reply(&reply(json!([1, 2, 3, 4]), json!([1])), &numbers(), &all())
            .unwrap_err();
        assert_eq!(errs.len(), 1, "{errs:?}");
        assert_eq!(errs[0].kind, SummaryErrorKind::TooManyCitations(4));
    }

    #[test]
    fn summary_reply_repetition_in_one_list_is_an_error() {
        // The same bullet text a third time in one section is a looping reply.
        let mut v = reply(json!([1]), json!([2]));
        for text in ["B.", "b "] {
            v["enhanced_notes"][0]["bullets"]
                .as_array_mut()
                .unwrap()
                .push(json!({"text": text, "source_segment_ids": [1]}));
        }
        let errs = validate_reply(&v, &numbers(), &all()).unwrap_err();
        assert_eq!(errs.len(), 1, "{errs:?}");
        assert_eq!(errs[0].path, "/enhanced_notes/0/bullets/2/text");
        assert_eq!(errs[0].kind, SummaryErrorKind::Repetition);
        // Reported beside other errors too.
        let mut bad = v.clone();
        bad["decisions"][0]["source_segment_ids"] = json!([9]);
        let errs = validate_reply(&bad, &numbers(), &all()).unwrap_err();
        assert!(
            errs.iter().any(|e| e.kind == SummaryErrorKind::Repetition),
            "{errs:?}"
        );
        // Sections with the same heading are one section: three copies across "H", "h"
        // and "H." loop too.
        let mut merged = reply(json!([1]), json!([2]));
        for heading in ["h", "H."] {
            merged["enhanced_notes"]
                .as_array_mut()
                .unwrap()
                .push(json!({"heading": heading, "bullets": [
                    {"text": "B", "source_segment_ids": [1]}]}));
        }
        let errs = validate_reply(&merged, &numbers(), &all()).unwrap_err();
        assert_eq!(errs[0].path, "/enhanced_notes/2/bullets/0/text", "{errs:?}");
        // The same bullet under three different headings is not a loop, and stays.
        let mut sections = reply(json!([1]), json!([2]));
        for heading in ["Other", "Third"] {
            sections["enhanced_notes"]
                .as_array_mut()
                .unwrap()
                .push(json!({"heading": heading, "bullets": [
                    {"text": "B.", "source_segment_ids": [1]}]}));
        }
        let ok = validate_reply(&sections, &numbers(), &all()).unwrap();
        assert_eq!(ok.duplicates_dropped, 0);
        assert_eq!(ok.summary.enhanced_notes.len(), 3);
        // Three action items with one task but three owners are not a loop either.
        let mut owners = reply(json!([1]), json!([2]));
        for owner in ["Ana", "Ben"] {
            owners["action_items"]
                .as_array_mut()
                .unwrap()
                .push(json!({"owner": owner, "task": "t", "due": null, "source_segment_ids": [5]}));
        }
        let ok = validate_reply(&owners, &numbers(), &all()).unwrap();
        assert_eq!(ok.summary.action_items.len(), 3);
        // The same text in two different lists is fine.
        let mut ok = reply(json!([1]), json!([2]));
        ok["decisions"][0]["text"] = json!("b");
        assert!(validate_reply(&ok, &numbers(), &all()).is_ok());
    }

    #[test]
    fn summary_reply_duplicates_are_dropped_keeping_the_first() {
        let mut v = reply(json!([1]), json!([2]));
        v["topics"] = json!([
            "User Interface",
            "Materials",
            "User interface",
            "materials."
        ]);
        v["decisions"]
            .as_array_mut()
            .unwrap()
            .push(json!({"text": "D", "source_segment_ids": [3]}));
        v["open_questions"] = json!(["Why?", "why"]);
        let actions = v["action_items"].as_array_mut().unwrap();
        // One task, two owners: both kept. The same owner and task in other case: dropped.
        actions.push(json!({"owner": "Bob", "task": "T.", "due": null, "source_segment_ids": [4]}));
        actions.push(json!({"owner": "me", "task": "T", "due": null, "source_segment_ids": [4]}));
        let notes = v["enhanced_notes"].as_array_mut().unwrap();
        // A second "H" section: its new bullet moves to the first, its copy of "b" is dropped.
        notes.push(json!({"heading": "h", "bullets": [
            {"text": "B", "source_segment_ids": [2]},
            {"text": "c", "source_segment_ids": [2]}]}));
        // The same bullet under another heading stays.
        notes.push(json!({"heading": "Other", "bullets": [
            {"text": "c.", "source_segment_ids": [3]}]}));
        // Dropping within a section keeps its first bullet; a section written empty stays.
        notes.push(json!({"heading": "Gone", "bullets": [
            {"text": "x", "source_segment_ids": [3]},
            {"text": "X.", "source_segment_ids": [3]}]}));
        notes.push(json!({"heading": "Empty", "bullets": []}));
        let checked = validate_reply(&v, &numbers(), &all()).unwrap();
        let s = &checked.summary;
        assert_eq!(s.topics, ["User Interface", "Materials"]);
        assert_eq!(s.decisions.len(), 1);
        assert_eq!(s.decisions[0].source_segment_ids, [id(2)]);
        let owners: Vec<&str> = s.action_items.iter().map(|a| a.owner.as_str()).collect();
        assert_eq!(owners, ["Me", "Bob"]);
        assert_eq!(s.open_questions, ["Why?"]);
        let headings: Vec<&str> = s
            .enhanced_notes
            .iter()
            .map(|n| n.heading.as_str())
            .collect();
        assert_eq!(headings, ["H", "Other", "Gone", "Empty"]);
        let bullets = |i: usize| -> Vec<&str> {
            s.enhanced_notes[i]
                .bullets
                .iter()
                .map(|b| b.text.as_str())
                .collect()
        };
        assert_eq!(bullets(0), ["b", "c"]);
        assert_eq!(bullets(1), ["c."]);
        assert_eq!(bullets(2), ["x"]);
        // The count the caller sees: 2 topics, 1 decision, 1 question, 1 action, 1 merged
        // heading, 2 bullets ("B" in H, "X." in Gone).
        assert_eq!(checked.duplicates_dropped, 8);
        let mut again = validate_value_for_test(&v);
        assert_eq!(drop_repeats(&mut again), 8);
        assert_eq!(drop_repeats(&mut again), 0);
    }

    #[test]
    fn summary_reply_rejects_a_segment_number_cited_twice_in_one_list() {
        let errs =
            validate_reply(&reply(json!([2, 2]), json!([1])), &numbers(), &all()).unwrap_err();
        assert_eq!(errs.len(), 1, "{errs:?}");
        assert_eq!(
            errs[0].path,
            "/enhanced_notes/0/bullets/0/source_segment_ids/1"
        );
        assert_eq!(errs[0].kind, SummaryErrorKind::DuplicateSegmentNumber(2));
        // The same number on two items is fine.
        assert!(validate_reply(&reply(json!([2]), json!([2])), &numbers(), &all()).is_ok());
    }

    #[test]
    fn summary_reply_uncited_suppression_matches_the_list_path_exactly() {
        // Decision 1 cites nothing (uncited); decision 10's only citation is wrong. Only
        // decision 10's uncited report is folded into its citation error.
        let mut v = reply(json!([1]), json!([2]));
        let decisions = v["decisions"].as_array_mut().unwrap();
        decisions.push(json!({"text": "uncited", "source_segment_ids": []}));
        for i in 2..10 {
            decisions.push(json!({"text": format!("d{i}"), "source_segment_ids": [3]}));
        }
        decisions.push(json!({"text": "wrong", "source_segment_ids": [9]}));
        let errs = validate_reply(&v, &numbers(), &all()).unwrap_err();
        let got: Vec<(&str, &SummaryErrorKind)> =
            errs.iter().map(|e| (e.path.as_str(), &e.kind)).collect();
        assert_eq!(
            got,
            [
                (
                    "/decisions/10/source_segment_ids/0",
                    &SummaryErrorKind::UnknownSegmentNumber(9)
                ),
                (
                    "/decisions/1/source_segment_ids",
                    &SummaryErrorKind::Uncited
                ),
            ],
            "{errs:?}"
        );
    }

    #[test]
    fn summary_citable_of_partials_is_what_they_cite() {
        let s = validate_reply(&reply(json!([1, 3]), json!([3])), &numbers(), &all())
            .unwrap()
            .summary;
        let t = validate_reply(&reply(json!([5]), json!([4])), &numbers(), &all())
            .unwrap()
            .summary;
        let c = Citable::cited_by([&s, &t], &numbers());
        assert_eq!(c.iter().collect::<Vec<_>>(), [0, 2, 3, 4]);
        assert_eq!(format!("{c:?}"), "{0..1, 2..5}");
        assert_eq!(format!("{:?}", Citable::from(0..4)), "0..4");
        // A merge reply may cite only those.
        let errs = validate_reply(&reply(json!([2]), json!([1])), &numbers(), &c).unwrap_err();
        assert_eq!(errs[0].kind, SummaryErrorKind::UnknownSegmentNumber(2));
        assert!(validate_reply(&reply(json!([1]), json!([5])), &numbers(), &c).is_ok());
    }

    /// The reply as a stored summary without dropping (citations mapped by hand).
    fn validate_value_for_test(v: &Value) -> SummaryV1 {
        let mut v = v.clone();
        for_each_citation_list(&mut v, |_, list| {
            for c in list.iter_mut() {
                let n = usize::try_from(c.as_u64().unwrap()).unwrap();
                *c = Value::String(numbers().id_of(n).unwrap().to_string());
            }
        });
        serde_json::from_value(v).unwrap()
    }

    #[test]
    fn summary_llm_schema_has_numbered_capped_citations_and_no_annotations() {
        let s = llm_json_schema().to_string();
        assert!(
            !s.contains("description")
                && !s.contains("$schema")
                && !s.contains("title\":\"Earshot")
        );
        assert!(!s.contains("uuid"));
        for def in ["NoteBullet", "Decision", "ActionItem"] {
            let c = &llm_json_schema()["$defs"][def]["properties"]["source_segment_ids"];
            assert_eq!(
                *c,
                json!({"type": "array", "items": {"type": "integer", "minimum": 1}, "maxItems": 3})
            );
        }
        // Everything else is the summary v1 shape: the same objects, properties and
        // required lists (a property called `title` is a name, not an annotation).
        let v1 = json_schema();
        let llm = llm_json_schema();
        let keys = |v: &Value| -> Vec<String> {
            v.as_object()
                .map(|m| m.keys().cloned().collect())
                .unwrap_or_default()
        };
        assert_eq!(keys(&llm["properties"]), keys(&v1["properties"]));
        assert!(keys(&llm["properties"]).contains(&"title".to_string()));
        assert_eq!(llm["required"], v1["required"]);
        assert_eq!(keys(&llm["$defs"]), keys(&v1["$defs"]));
        for def in ["ActionItem", "Decision", "NoteBullet", "NoteSection"] {
            assert_eq!(
                keys(&llm["$defs"][def]["properties"]),
                keys(&v1["$defs"][def]["properties"])
            );
            assert_eq!(llm["$defs"][def]["required"], v1["$defs"][def]["required"]);
        }
        assert_eq!(
            llm["$defs"]["ActionItem"]["properties"]["due"],
            json!({"type": ["string", "null"]})
        );
        assert!(
            s.len() * 2 < v1.to_string().len(),
            "{} vs {}",
            s.len(),
            v1.to_string().len()
        );
    }

    #[test]
    fn summary_repeated_actions_across_partials_keep_the_first_copy() {
        let action = |owner: &str, task: &str, cite: u32| crate::summary::ActionItem {
            owner: owner.into(),
            task: task.into(),
            due: None,
            source_segment_ids: vec![id(cite)],
        };
        let part = |actions: Vec<crate::summary::ActionItem>| SummaryV1 {
            action_items: actions,
            ..SummaryV1::default()
        };
        let mut partials = vec![
            part(vec![
                action("Ana", "Send the minutes.", 1),
                action("Ben", "Draw it", 1),
            ]),
            // Same owner and task after normalising: dropped, whatever it cites.
            part(vec![
                action("ana", "send the minutes", 2),
                action("Ben", "Draw it again", 2),
            ]),
            // The same task for another owner is another commitment: kept.
            part(vec![
                action("Ben", "Send the minutes", 3),
                action("Ben", "Draw it", 3),
            ]),
        ];
        assert_eq!(drop_repeated_actions(&mut partials), 2);
        let left: Vec<Vec<(&str, &str)>> = partials
            .iter()
            .map(|p| {
                p.action_items
                    .iter()
                    .map(|a| (a.owner.as_str(), a.task.as_str()))
                    .collect()
            })
            .collect();
        assert_eq!(
            left,
            [
                vec![("Ana", "Send the minutes."), ("Ben", "Draw it")],
                vec![("Ben", "Draw it again")],
                vec![("Ben", "Send the minutes")],
            ]
        );
        assert_eq!(partials[0].action_items[0].source_segment_ids, [id(1)]);
        assert_eq!(drop_repeated_actions(&mut partials), 0);
    }
}
