//! Merge by selection (T12.25 part 2, evaluation only; production never calls it): the
//! partial summaries of a chunked run are kept as written, a selection request
//! ([`crate::prompt::select_prompt`]) chooses by number which of their decisions, action items
//! and open questions the summary keeps and which to fold together, and code builds the
//! summary from the kept items ([`build_from_selection`]). Nothing is reworded: every kept
//! item's text is a partial's own; only the `tldr` is the selection reply's.

use std::collections::BTreeSet;

use serde_json::Value;

use super::cite::{MAX_CITATIONS, drop_repeats, join_partials};
use super::validate::{SummaryError, SummaryErrorKind};
use super::{ActionItem, Decision, SummaryV1};

/// How many items of each list a set of partials holds (the numbers a selection may use) or
/// a summary kept.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SelectCounts {
    /// Decisions.
    pub decisions: usize,
    /// Action items.
    pub action_items: usize,
    /// Open questions.
    pub open_questions: usize,
}

impl SelectCounts {
    /// The counts of `parts` together, in the numbering of [`SelectItems`].
    #[must_use]
    pub fn of_parts(parts: &[SummaryV1]) -> Self {
        SelectCounts {
            decisions: parts.iter().map(|p| p.decisions.len()).sum(),
            action_items: parts.iter().map(|p| p.action_items.len()).sum(),
            open_questions: parts.iter().map(|p| p.open_questions.len()).sum(),
        }
    }

    /// The counts of one summary.
    #[must_use]
    pub fn of_summary(s: &SummaryV1) -> Self {
        Self::of_parts(std::slice::from_ref(s))
    }

    /// All items.
    #[must_use]
    pub fn total(&self) -> usize {
        self.decisions + self.action_items + self.open_questions
    }
}

/// The items of `parts` as a selection numbers them: each list in meeting order (part by
/// part, each part's own order), item `k` of a list is number `k + 1`; with the part index
/// (from 0) of each.
#[derive(Debug, Clone, PartialEq)]
pub struct SelectItems<'a> {
    /// Decisions.
    pub decisions: Vec<(usize, &'a Decision)>,
    /// Action items.
    pub action_items: Vec<(usize, &'a ActionItem)>,
    /// Open questions.
    pub open_questions: Vec<(usize, &'a String)>,
}

impl<'a> SelectItems<'a> {
    /// The numbered items of `parts`.
    #[must_use]
    pub fn of(parts: &'a [SummaryV1]) -> Self {
        let mut items = SelectItems {
            decisions: Vec::new(),
            action_items: Vec::new(),
            open_questions: Vec::new(),
        };
        for (i, p) in parts.iter().enumerate() {
            items.decisions.extend(p.decisions.iter().map(|d| (i, d)));
            items
                .action_items
                .extend(p.action_items.iter().map(|a| (i, a)));
            items
                .open_questions
                .extend(p.open_questions.iter().map(|q| (i, q)));
        }
        items
    }
}

/// A checked selection reply ([`parse_selection`]): per list, the groups of item numbers
/// (from 1) the summary keeps, one summary item per group (a group of several folds them,
/// the first one's words kept); and the summary's `tldr`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Selection {
    /// Groups of decision numbers.
    pub decisions: Vec<Vec<usize>>,
    /// Groups of action item numbers.
    pub action_items: Vec<Vec<usize>>,
    /// Groups of open question numbers.
    pub open_questions: Vec<Vec<usize>>,
    /// The summary's `tldr`.
    pub tldr: String,
}

impl Selection {
    /// Numbers given in groups of two or more beyond the first of each group: items folded
    /// away into another.
    #[must_use]
    pub fn folded(&self) -> usize {
        [&self.decisions, &self.action_items, &self.open_questions]
            .into_iter()
            .flatten()
            .map(|g| g.len().saturating_sub(1))
            .sum()
    }
}

/// The keys of a selection reply, in the order the grammar writes them: the numbers kept of
/// each list, then the groups of numbers that are one item in other words (decisions and
/// action items), then the `tldr`.
pub const SELECTION_FIELDS: [&str; 6] = [
    "decisions",
    "action_items",
    "open_questions",
    "same_decisions",
    "same_action_items",
    "tldr",
];

/// One number of a selection list: an integer from 1 to `count`, else an error at `path`.
fn number(n: &Value, count: usize, path: String, errors: &mut Vec<SummaryError>) -> Option<usize> {
    match n.as_u64() {
        Some(k) if k >= 1 && usize::try_from(k).is_ok_and(|k| k <= count) => {
            usize::try_from(k).ok()
        }
        Some(k) => {
            errors.push(SummaryError {
                path,
                kind: SummaryErrorKind::UnknownSegmentNumber(k),
            });
            None
        }
        None => {
            errors.push(SummaryError {
                path,
                kind: SummaryErrorKind::WrongType {
                    expected: "an integer from 1",
                },
            });
            None
        }
    }
}

/// The array at `/{name}` of `obj`, or an error.
fn array<'a>(
    obj: &'a serde_json::Map<String, Value>,
    name: &str,
    errors: &mut Vec<SummaryError>,
) -> &'a [Value] {
    match obj.get(name) {
        None => {
            errors.push(SummaryError {
                path: format!("/{name}"),
                kind: SummaryErrorKind::MissingField,
            });
            &[]
        }
        Some(Value::Array(a)) => a,
        Some(_) => {
            errors.push(SummaryError {
                path: format!("/{name}"),
                kind: SummaryErrorKind::WrongType { expected: "array" },
            });
            &[]
        }
    }
}

/// The numbers kept of list `name`: each in range, each at most once.
fn kept(
    obj: &serde_json::Map<String, Value>,
    name: &str,
    count: usize,
    errors: &mut Vec<SummaryError>,
) -> Vec<usize> {
    let mut out = Vec::new();
    for (k, n) in array(obj, name, errors).iter().enumerate() {
        let path = format!("/{name}/{k}");
        if let Some(n) = number(n, count, path.clone(), errors) {
            if out.contains(&n) {
                errors.push(SummaryError {
                    path,
                    kind: SummaryErrorKind::Repetition,
                });
            } else {
                out.push(n);
            }
        }
    }
    out
}

/// The groups of list `name`: each of two or more numbers in range, a number in at most one
/// group.
fn same(
    obj: &serde_json::Map<String, Value>,
    name: &str,
    count: usize,
    errors: &mut Vec<SummaryError>,
) -> Vec<Vec<usize>> {
    let mut seen = BTreeSet::new();
    let mut out = Vec::new();
    for (g, group) in array(obj, name, errors).iter().enumerate() {
        let Some(numbers) = group.as_array() else {
            errors.push(SummaryError {
                path: format!("/{name}/{g}"),
                kind: SummaryErrorKind::WrongType { expected: "array" },
            });
            continue;
        };
        if numbers.len() < 2 {
            errors.push(SummaryError {
                path: format!("/{name}/{g}"),
                kind: SummaryErrorKind::WrongType {
                    expected: "a group of two or more numbers",
                },
            });
            continue;
        }
        let mut members = Vec::new();
        for (k, n) in numbers.iter().enumerate() {
            let path = format!("/{name}/{g}/{k}");
            if let Some(n) = number(n, count, path.clone(), errors) {
                if seen.insert(n) {
                    members.push(n);
                } else {
                    errors.push(SummaryError {
                        path,
                        kind: SummaryErrorKind::Repetition,
                    });
                }
            }
        }
        out.push(members);
    }
    out
}

/// The summary's items from the numbers `kept` and the `same` groups: in the order kept, one
/// item per kept number not already folded into an earlier one, folding in the other members
/// of its group (kept or not).
fn fold(kept: &[usize], same: &[Vec<usize>]) -> Vec<Vec<usize>> {
    let mut used = BTreeSet::new();
    let mut out = Vec::new();
    for &k in kept {
        if !used.insert(k) {
            continue;
        }
        let mut item = vec![k];
        if let Some(group) = same.iter().find(|g| g.contains(&k)) {
            item.extend(group.iter().copied().filter(|&n| used.insert(n)));
        }
        out.push(item);
    }
    out
}

/// Checks a selection reply against `counts` (the items it may number) and turns it into
/// groups: an object with exactly [`SELECTION_FIELDS`]; `decisions`, `action_items` and
/// `open_questions` the numbers kept (integers from 1 to the list's count, each at most
/// once); `same_decisions` and `same_action_items` groups of two or more numbers that are one
/// item in other words (a number in at most one group); a non-blank `tldr`. Each kept number
/// gives one item, in the order kept, with the other members of its group folded into it
/// (kept or not); a kept number already folded into an earlier one gives none, and a group
/// none of whose numbers is kept changes nothing. Errors carry a JSON Pointer path:
/// [`SummaryErrorKind::UnknownSegmentNumber`] for a number outside the list (an item number
/// here, not a segment) and [`SummaryErrorKind::Repetition`] for a number given twice.
///
/// # Errors
/// Every problem found.
pub fn parse_selection(
    value: &Value,
    counts: SelectCounts,
) -> Result<Selection, Vec<SummaryError>> {
    let Some(obj) = value.as_object() else {
        return Err(vec![SummaryError {
            path: String::new(),
            kind: SummaryErrorKind::WrongType { expected: "object" },
        }]);
    };
    let mut errors = Vec::new();
    for k in obj.keys() {
        if !SELECTION_FIELDS.contains(&k.as_str()) {
            errors.push(SummaryError {
                path: format!("/{k}"),
                kind: SummaryErrorKind::UnknownField,
            });
        }
    }
    let decisions = kept(obj, "decisions", counts.decisions, &mut errors);
    let action_items = kept(obj, "action_items", counts.action_items, &mut errors);
    let open_questions = kept(obj, "open_questions", counts.open_questions, &mut errors);
    let same_decisions = same(obj, "same_decisions", counts.decisions, &mut errors);
    let same_actions = same(obj, "same_action_items", counts.action_items, &mut errors);
    let tldr = match obj.get("tldr") {
        Some(Value::String(s)) if !s.trim().is_empty() => s.trim().to_owned(),
        Some(Value::String(_)) => {
            errors.push(SummaryError {
                path: "/tldr".into(),
                kind: SummaryErrorKind::Blank,
            });
            String::new()
        }
        None => {
            errors.push(SummaryError {
                path: "/tldr".into(),
                kind: SummaryErrorKind::MissingField,
            });
            String::new()
        }
        Some(_) => {
            errors.push(SummaryError {
                path: "/tldr".into(),
                kind: SummaryErrorKind::WrongType { expected: "string" },
            });
            String::new()
        }
    };
    if !errors.is_empty() {
        return Err(errors);
    }
    Ok(Selection {
        decisions: fold(&decisions, &same_decisions),
        action_items: fold(&action_items, &same_actions),
        open_questions: open_questions.into_iter().map(|n| vec![n]).collect(),
        tldr,
    })
}

/// The summary `selection` keeps of `parts` (numbered as [`SelectItems::of`]): what
/// [`join_partials`] gives for the title (the first part's), the notes and the topics (every
/// part's, exact repeats dropped), with the selection's `tldr` and, per list, one item per
/// group: the group's first item as written, citing the segments of every item of the group
/// in order without repeats, at most [`MAX_CITATIONS`]. Exact repeats are then dropped
/// ([`drop_repeats`]). No text is invented; every citation is a part's own.
///
/// # Panics
/// When a number is outside `parts`' items ([`parse_selection`] refuses such a selection).
#[must_use]
pub fn build_from_selection(parts: &[SummaryV1], selection: &Selection) -> SummaryV1 {
    let items = SelectItems::of(parts);
    let mut s = join_partials(parts);
    s.tldr = selection.tldr.trim().to_owned();
    s.decisions = selection
        .decisions
        .iter()
        .map(|g| {
            let mut d = items.decisions[g[0] - 1].1.clone();
            d.source_segment_ids = cited(
                g.iter()
                    .map(|&n| &items.decisions[n - 1].1.source_segment_ids),
            );
            d
        })
        .collect();
    s.action_items = selection
        .action_items
        .iter()
        .map(|g| {
            let mut a = items.action_items[g[0] - 1].1.clone();
            a.source_segment_ids = cited(
                g.iter()
                    .map(|&n| &items.action_items[n - 1].1.source_segment_ids),
            );
            a
        })
        .collect();
    s.open_questions = selection
        .open_questions
        .iter()
        .map(|g| items.open_questions[g[0] - 1].1.clone())
        .collect();
    drop_repeats(&mut s);
    s
}

/// The citations of a folded group: every list's ids in order, without repeats, at most
/// [`MAX_CITATIONS`].
fn cited<'a, I>(lists: impl Iterator<Item = &'a Vec<I>>) -> Vec<I>
where
    I: PartialEq + Clone + 'a,
{
    let mut out: Vec<I> = Vec::new();
    for id in lists.flatten() {
        if out.len() == MAX_CITATIONS {
            break;
        }
        if !out.contains(id) {
            out.push(id.clone());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::super::{NoteBullet, NoteSection};
    use super::*;
    use crate::identity::EntityId;

    fn id(n: u32) -> EntityId {
        format!("0193a5b0-1c00-7000-8000-{n:012x}").parse().unwrap()
    }

    fn decision(text: &str, segs: &[u32]) -> Decision {
        Decision {
            text: text.into(),
            source_segment_ids: segs.iter().map(|&s| id(s)).collect(),
        }
    }

    fn action(owner: &str, task: &str, segs: &[u32]) -> ActionItem {
        ActionItem {
            owner: owner.into(),
            task: task.into(),
            due: None,
            source_segment_ids: segs.iter().map(|&s| id(s)).collect(),
        }
    }

    /// Two invented parts of a remote-control design meeting.
    fn parts() -> Vec<SummaryV1> {
        vec![
            SummaryV1 {
                title: "Remote kick-off".into(),
                tldr: "The team picked a case.".into(),
                enhanced_notes: vec![NoteSection {
                    heading: "Case".into(),
                    bullets: vec![NoteBullet {
                        text: "Plastic was cheaper".into(),
                        source_segment_ids: vec![id(1)],
                    }],
                }],
                decisions: vec![
                    decision("The case is plastic.", &[1, 2]),
                    decision("The remote is grey.", &[3]),
                ],
                action_items: vec![action("Ana", "Send the costs", &[4])],
                open_questions: vec!["Which battery?".into()],
                topics: vec!["case".into()],
            },
            SummaryV1 {
                title: "Remote, part two".into(),
                tldr: "The colour changed.".into(),
                enhanced_notes: vec![NoteSection {
                    heading: "Colour".into(),
                    bullets: vec![NoteBullet {
                        text: "Yellow tested better".into(),
                        source_segment_ids: vec![id(6)],
                    }],
                }],
                decisions: vec![
                    decision("The remote is yellow.", &[6]),
                    decision("A plastic case.", &[2, 7, 8]),
                ],
                action_items: vec![action("Ana", "Mail the cost sheet", &[9])],
                open_questions: vec!["When is the next meeting?".into()],
                topics: vec!["colour".into(), "case".into()],
            },
        ]
    }

    #[test]
    fn summary_select_items_are_numbered_part_by_part_in_meeting_order() {
        let p = parts();
        let items = SelectItems::of(&p);
        let texts: Vec<(usize, &str)> = items
            .decisions
            .iter()
            .map(|(i, d)| (*i, d.text.as_str()))
            .collect();
        assert_eq!(
            texts,
            [
                (0, "The case is plastic."),
                (0, "The remote is grey."),
                (1, "The remote is yellow."),
                (1, "A plastic case.")
            ]
        );
        assert_eq!(
            SelectCounts::of_parts(&p),
            SelectCounts {
                decisions: 4,
                action_items: 2,
                open_questions: 2
            }
        );
        assert_eq!(SelectCounts::of_parts(&p).total(), 8);
    }

    #[test]
    fn summary_select_keeps_folds_and_drops_by_number_and_rewords_nothing() {
        let p = parts();
        // Keep the plastic case folded with its later wording, drop the superseded grey,
        // keep yellow; fold the two cost tasks; drop the answered battery question.
        let sel = Selection {
            decisions: vec![vec![1, 4], vec![3]],
            action_items: vec![vec![1, 2]],
            open_questions: vec![vec![2]],
            tldr: " The team chose a yellow plastic remote. ".into(),
        };
        let s = build_from_selection(&p, &sel);
        assert_eq!(s.title, "Remote kick-off");
        assert_eq!(s.tldr, "The team chose a yellow plastic remote.");
        assert_eq!(
            s.decisions,
            vec![
                // The first item's words; citations of both, in order, no repeat, at most 3.
                decision("The case is plastic.", &[1, 2, 7]),
                decision("The remote is yellow.", &[6]),
            ]
        );
        assert_eq!(
            s.action_items,
            vec![action("Ana", "Send the costs", &[4, 9])]
        );
        assert_eq!(
            s.open_questions,
            vec!["When is the next meeting?".to_owned()]
        );
        // Notes and topics are every part's, joined as `join_partials` joins them.
        let joined = join_partials(&p);
        assert_eq!(s.enhanced_notes, joined.enhanced_notes);
        assert_eq!(s.topics, vec!["case".to_owned(), "colour".to_owned()]);
        // Every kept text is a partial's own.
        for d in &s.decisions {
            assert!(
                p.iter()
                    .flat_map(|x| &x.decisions)
                    .any(|x| x.text == d.text)
            );
        }
        assert_eq!(sel.folded(), 2);
        assert_eq!(
            SelectCounts::of_summary(&s),
            SelectCounts {
                decisions: 2,
                action_items: 1,
                open_questions: 1
            }
        );
    }

    #[test]
    fn summary_select_an_empty_selection_keeps_no_items() {
        let p = parts();
        let sel = Selection {
            decisions: vec![],
            action_items: vec![],
            open_questions: vec![],
            tldr: "Nothing settled.".into(),
        };
        let s = build_from_selection(&p, &sel);
        assert!(s.decisions.is_empty() && s.action_items.is_empty() && s.open_questions.is_empty());
        assert_eq!(s.tldr, "Nothing settled.");
    }

    /// A reply of the given lists with no folds, as the grammar writes it.
    fn reply(d: Value, a: Value, q: Value) -> Value {
        json!({"decisions": d, "action_items": a, "open_questions": q, "same_decisions": [], "same_action_items": [], "tldr": "Done."})
    }

    #[test]
    fn summary_select_parse_takes_kept_numbers_and_folds_them_by_group() {
        let counts = SelectCounts::of_parts(&parts());
        // Kept in the order given; nothing folded.
        assert_eq!(
            parse_selection(&reply(json!([3, 1]), json!([2]), json!([])), counts).unwrap(),
            Selection {
                decisions: vec![vec![3], vec![1]],
                action_items: vec![vec![2]],
                open_questions: vec![],
                tldr: "Done.".into(),
            }
        );
        // A group folds its other members into the first kept member, kept or not; a kept
        // number already folded gives no item; a group with no kept member changes nothing.
        let mut v = reply(json!([1, 3, 4]), json!([2, 1]), json!([2]));
        v["same_decisions"] = json!([[4, 1], [2, 3]]);
        v["same_action_items"] = json!([[1, 2]]);
        let s = parse_selection(&v, counts).unwrap();
        assert_eq!(s.decisions, vec![vec![1, 4], vec![3, 2]]);
        assert_eq!(s.action_items, vec![vec![2, 1]]);
        assert_eq!(s.open_questions, vec![vec![2]]);
        assert_eq!(s.folded(), 3);
        let mut v = reply(json!([3]), json!([]), json!([]));
        v["same_decisions"] = json!([[1, 2]]);
        assert_eq!(
            parse_selection(&v, counts).unwrap().decisions,
            vec![vec![3]]
        );
    }

    #[test]
    fn summary_select_parse_refuses_repeats_ranges_shapes_and_blanks() {
        let counts = SelectCounts::of_parts(&parts());
        let kinds = |v: Value| -> Vec<(String, SummaryErrorKind)> {
            parse_selection(&v, counts)
                .unwrap_err()
                .into_iter()
                .map(|e| (e.path, e.kind))
                .collect()
        };
        // A number kept twice; a number in two groups.
        assert_eq!(
            kinds(reply(json!([1, 2, 1]), json!([]), json!([]))),
            [("/decisions/2".to_owned(), SummaryErrorKind::Repetition)]
        );
        let mut v = reply(json!([1]), json!([]), json!([]));
        v["same_decisions"] = json!([[1, 2], [3, 2]]);
        assert_eq!(
            kinds(v),
            [(
                "/same_decisions/1/1".to_owned(),
                SummaryErrorKind::Repetition
            )]
        );
        // Out of range: 0, past the list's count (2 actions), and in a group.
        let mut v = reply(json!([0]), json!([3]), json!([]));
        v["same_action_items"] = json!([[1, 9]]);
        assert_eq!(
            kinds(v),
            [
                (
                    "/decisions/0".to_owned(),
                    SummaryErrorKind::UnknownSegmentNumber(0)
                ),
                (
                    "/action_items/0".to_owned(),
                    SummaryErrorKind::UnknownSegmentNumber(3)
                ),
                (
                    "/same_action_items/0/1".to_owned(),
                    SummaryErrorKind::UnknownSegmentNumber(9)
                ),
            ]
        );
        // A group of one, a blank tldr, a missing list, an unknown field, a wrong type.
        let e = kinds(
            json!({"decisions": [1], "action_items": "no", "same_decisions": [[2]], "same_action_items": [], "tldr": " ", "title": "t"}),
        );
        assert!(
            e.contains(&("/title".into(), SummaryErrorKind::UnknownField)),
            "{e:?}"
        );
        assert!(
            e.contains(&(
                "/same_decisions/0".into(),
                SummaryErrorKind::WrongType {
                    expected: "a group of two or more numbers"
                }
            )),
            "{e:?}"
        );
        assert!(e.contains(&(
            "/action_items".into(),
            SummaryErrorKind::WrongType { expected: "array" }
        )));
        assert!(e.contains(&("/open_questions".into(), SummaryErrorKind::MissingField)));
        assert!(e.contains(&("/tldr".into(), SummaryErrorKind::Blank)));
        assert_eq!(
            kinds(json!([1])),
            [(
                String::new(),
                SummaryErrorKind::WrongType { expected: "object" }
            )]
        );
        assert!(
            kinds(reply(json!(["1"]), json!([]), json!([])))
                .iter()
                .any(|(p, _)| p == "/decisions/0")
        );
    }

    /// A built summary always passes the reply validation of its parts' segments.
    #[test]
    fn summary_select_built_summaries_validate_against_the_parts_segments() {
        use crate::summary::{Citable, SegmentNumbers, to_numbered_json, validate_reply};
        let p = parts();
        let numbers = SegmentNumbers::new((1..=9).map(id));
        let sel = Selection {
            decisions: vec![vec![4, 1, 2], vec![3]],
            action_items: vec![vec![2, 1]],
            open_questions: vec![vec![1], vec![2]],
            tldr: "A yellow plastic remote.".into(),
        };
        let s = build_from_selection(&p, &sel);
        let v: Value = serde_json::from_str(&to_numbered_json(&s, &numbers)).unwrap();
        let checked = validate_reply(&v, &numbers, &Citable::cited_by(&p, &numbers)).unwrap();
        assert_eq!(checked.summary, s);
        // The first item's words, citations of all three in order, capped at three.
        assert_eq!(s.decisions[0], decision("A plastic case.", &[2, 7, 8]));
    }
}
