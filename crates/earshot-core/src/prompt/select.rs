//! The selection request of a merge by selection (T12.25 part 2, evaluation only; production
//! never calls it): instead of writing the merged summary again, the model is shown the
//! numbered decisions, action items and open questions of every partial summary and replies
//! with the numbers to keep and fold and a short `tldr` ([`crate::summary::Selection`]).
//! Code builds the summary ([`crate::summary::build_from_selection`]).
//!
//! The system message is one fixed text ([`SELECT`]) for every prompt arm: it carries the
//! action-item and decision definitions (rules 5 and 6), which `system.md` and `brief.md`
//! share word for word (pinned by `prompt_select_carries_the_shared_item_rules`).

use core::fmt::Write as _;

use super::builder::{
    ChatMessage, Prompt, REPAIR_LINE_MAX_CHARS, REPAIR_MAX_TOKENS, Role, SummaryInput,
    meeting_block, one_line, segment_numbers,
};
use super::tokens::{TOKENS_PER_MESSAGE, estimate_tokens};
use crate::summary::{
    Citable, SelectCounts, SelectItems, SummaryError, SummaryErrorKind, SummaryV1,
};

/// The system message of a selection request; `{{max_items}}` is replaced by the list cap.
pub(super) const SELECT: &str = include_str!("templates/select/system.md");

/// Reply tokens of a selection request before any item: the JSON frame and the `tldr`.
pub const SELECT_REPLY_BASE_TOKENS: usize = 256;

/// Reply tokens per numbered item: one number in its own group, with the indentation a model
/// writes (a judgement with room to spare; a cut-off reply is asked again with more).
pub const SELECT_REPLY_PER_ITEM_TOKENS: usize = 12;

/// The selection request over `partials` (consecutive partial summaries of `input`, in
/// meeting order): the system message [`SELECT`] with the list cap `max_items`; the meeting
/// details, each part's `tldr`, and the decisions (`D<n>`), action items (`A<n>`) and open
/// questions (`Q<n>`) numbered as [`SelectItems::of`], each labelled with its part. Its reply
/// budget is [`SELECT_REPLY_BASE_TOKENS`] plus [`SELECT_REPLY_PER_ITEM_TOKENS`] per item, at
/// most `max_output_tokens`. `cites` is what the partials cite (the built summary cites only
/// those); the reply itself cites nothing.
#[must_use]
pub fn select_prompt(
    input: &SummaryInput,
    partials: &[SummaryV1],
    max_items: u32,
    max_output_tokens: usize,
) -> Prompt {
    let numbers = segment_numbers(input);
    let items = SelectItems::of(partials);
    let n = partials.len();
    let mut u = meeting_block(&input.meeting);
    u.push('\n');
    u.push_str("Parts\n");
    for (i, p) in partials.iter().enumerate() {
        let _ = writeln!(u, "Part {} of {n}: {}", i + 1, one_line(&p.tldr));
    }
    u.push_str("\nDecisions\n");
    if items.decisions.is_empty() {
        u.push_str("(none)\n");
    }
    for (k, (part, d)) in items.decisions.iter().enumerate() {
        let _ = writeln!(u, "D{} (part {}): {}", k + 1, part + 1, one_line(&d.text));
    }
    u.push_str("\nAction items\n");
    if items.action_items.is_empty() {
        u.push_str("(none)\n");
    }
    for (k, (part, a)) in items.action_items.iter().enumerate() {
        let due = a
            .due
            .as_deref()
            .map(one_line)
            .filter(|d| !d.is_empty())
            .map_or_else(String::new, |d| format!(" (due: {d})"));
        let _ = writeln!(
            u,
            "A{} (part {}): {}: {}{due}",
            k + 1,
            part + 1,
            one_line(&a.owner),
            one_line(&a.task)
        );
    }
    u.push_str("\nOpen questions\n");
    if items.open_questions.is_empty() {
        u.push_str("(none)\n");
    }
    for (k, (part, q)) in items.open_questions.iter().enumerate() {
        let _ = writeln!(u, "Q{} (part {}): {}", k + 1, part + 1, one_line(q));
    }
    let count = SelectCounts::of_parts(partials).total();
    let reply =
        (SELECT_REPLY_BASE_TOKENS + SELECT_REPLY_PER_ITEM_TOKENS * count).min(max_output_tokens);
    Prompt::new(
        vec![
            ChatMessage::new(
                Role::System,
                SELECT.replace("{{max_items}}", &max_items.to_string()),
            ),
            ChatMessage::new(Role::User, u),
        ],
        reply,
        Citable::cited_by(partials, &numbers),
    )
}

/// The user message of a selection repair round: the problems found, one per line with its
/// JSON Pointer path, in the selection's own words (an out-of-range number is an item number
/// here), at most `max_errors` of them and never more than [`REPAIR_MAX_TOKENS`], and
/// whether the reply was cut off.
#[must_use]
pub fn selection_repair_message(
    errors: &[SummaryError],
    truncated: bool,
    max_errors: usize,
) -> ChatMessage {
    let mut s = String::new();
    if truncated {
        s.push_str(
            "Your reply was cut off because it reached the output limit. Keep the tldr short.\n",
        );
    }
    s.push_str("Your reply is not a valid selection. Problems found:\n");
    let tail =
        "Reply again with the complete corrected JSON object only, in the form you were given.";
    let mut room = REPAIR_MAX_TOKENS
        .saturating_sub(TOKENS_PER_MESSAGE + estimate_tokens(&s) + estimate_tokens(tail) + 16);
    let mut shown = 0;
    for e in errors.iter().take(max_errors) {
        let problem = match &e.kind {
            SummaryErrorKind::Repetition => {
                "this number is used twice; use each number at most once".to_owned()
            }
            SummaryErrorKind::UnknownSegmentNumber(n) => {
                format!("{n} is not one of the item numbers you were given")
            }
            SummaryErrorKind::Blank => "must not be empty".to_owned(),
            other => other.to_string(),
        };
        let path = if e.path.is_empty() {
            "(document)"
        } else {
            &e.path
        };
        let mut line = format!("- {path}: {problem}");
        if line.chars().count() > REPAIR_LINE_MAX_CHARS {
            line = line.chars().take(REPAIR_LINE_MAX_CHARS - 3).collect();
            line.push_str("...");
        }
        line.push('\n');
        let cost = estimate_tokens(&line);
        if cost > room {
            break;
        }
        room -= cost;
        s.push_str(&line);
        shown += 1;
    }
    if errors.len() > shown {
        let _ = writeln!(s, "- and {} more", errors.len() - shown);
    }
    s.push_str(tail);
    ChatMessage::new(Role::User, s)
}

#[cfg(test)]
mod tests {
    use super::super::template::{BRIEF, SYSTEM};
    use super::*;
    use crate::asr::MeetingTime;
    use crate::identity::EntityId;
    use crate::prompt::{MeetingInfo, PromptSegment};
    use crate::summary::{ActionItem, Decision};

    fn id(n: u32) -> EntityId {
        format!("0193a5b0-1c00-7000-8000-{n:012x}").parse().unwrap()
    }

    fn input() -> SummaryInput {
        let segments = (1..=4)
            .map(|i| PromptSegment {
                id: id(i),
                start: MeetingTime::from_millis(u64::from(i) * 1000),
                speaker: "Ana".into(),
                text: format!("line {i}"),
            })
            .collect();
        SummaryInput {
            meeting: MeetingInfo {
                title: None,
                started: None,
                duration_ms: 5000,
                attendees: vec!["Ana".into(), "Ben".into()],
            },
            segments,
            notes: Vec::new(),
            template: Default::default(),
            prompt_variant: Default::default(),
            rules: Default::default(),
            style: Default::default(),
            style_preset: Default::default(),
            vocabulary: Default::default(),
            user_name: None,
        }
    }

    fn part(
        tldr: &str,
        decisions: &[(&str, u32)],
        actions: &[(&str, &str, u32)],
        qs: &[&str],
    ) -> SummaryV1 {
        SummaryV1 {
            title: "Remote".into(),
            tldr: tldr.into(),
            enhanced_notes: Vec::new(),
            decisions: decisions
                .iter()
                .map(|(t, s)| Decision {
                    text: (*t).into(),
                    source_segment_ids: vec![id(*s)],
                })
                .collect(),
            action_items: actions
                .iter()
                .map(|(o, t, s)| ActionItem {
                    owner: (*o).into(),
                    task: (*t).into(),
                    due: (*s == 4).then(|| "Friday".to_owned()),
                    source_segment_ids: vec![id(*s)],
                })
                .collect(),
            open_questions: qs.iter().map(|q| (*q).to_owned()).collect(),
            topics: Vec::new(),
        }
    }

    #[test]
    fn prompt_select_numbers_every_item_by_part_in_meeting_order() {
        let partials = [
            part(
                "Case chosen.",
                &[("Plastic case.", 1), ("Grey remote.", 2)],
                &[("Ana", "Send costs", 2)],
                &[],
            ),
            part(
                "Colour changed.",
                &[("Yellow remote.", 3)],
                &[("Ben", "Draw it", 4)],
                &["Which battery?"],
            ),
        ];
        let p = select_prompt(&input(), &partials, 12, 8192);
        assert_eq!(p.messages.len(), 2);
        assert_eq!(p.messages[0].role, Role::System);
        assert!(
            p.messages[0]
                .content
                .contains("Keep at most 12 items in each list.")
        );
        assert!(!p.messages[0].content.contains("{{"));
        let u = &p.messages[1].content;
        let expected = "Parts\nPart 1 of 2: Case chosen.\nPart 2 of 2: Colour changed.\n\n\
            Decisions\nD1 (part 1): Plastic case.\nD2 (part 1): Grey remote.\nD3 (part 2): Yellow remote.\n\n\
            Action items\nA1 (part 1): Ana: Send costs\nA2 (part 2): Ben: Draw it (due: Friday)\n\n\
            Open questions\nQ1 (part 2): Which battery?\n";
        assert!(u.ends_with(expected), "{u}");
        assert!(u.starts_with("Meeting\n"));
        // 6 items: the base plus 12 each, under the ceiling; the ceiling wins when lower.
        assert_eq!(p.max_output_tokens, 256 + 12 * 6);
        assert_eq!(
            select_prompt(&input(), &partials, 12, 300).max_output_tokens,
            300
        );
        // The partials' citations only.
        assert_eq!(p.cites.iter().collect::<Vec<_>>(), [0, 1, 2, 3]);
    }

    #[test]
    fn prompt_select_shows_none_for_an_empty_list() {
        let partials = [
            part("Nothing.", &[], &[], &[]),
            part("Still nothing.", &[], &[], &[]),
        ];
        let u = &select_prompt(&input(), &partials, 12, 8192).messages[1].content;
        assert!(
            u.contains("Decisions\n(none)\n\nAction items\n(none)\n\nOpen questions\n(none)\n"),
            "{u}"
        );
    }

    /// The selection carries the shared action-item and decision rules (5 and 6) of both
    /// prompt arms word for word.
    #[test]
    fn prompt_select_carries_the_shared_item_rules() {
        let rules = |text: &str| -> Vec<String> {
            text.lines()
                .filter(|l| l.starts_with("5. ") || l.starts_with("6. "))
                .map(str::to_owned)
                .collect()
        };
        let standard = rules(SYSTEM);
        assert_eq!(standard.len(), 2);
        assert_eq!(rules(BRIEF), standard);
        assert_eq!(rules(SELECT), standard);
        assert!(!SELECT.contains('\u{2014}'));
    }

    #[test]
    fn prompt_select_repair_names_the_problems_in_selection_words() {
        let errors = [
            SummaryError {
                path: "/decisions/1/0".into(),
                kind: SummaryErrorKind::Repetition,
            },
            SummaryError {
                path: "/action_items/0/0".into(),
                kind: SummaryErrorKind::UnknownSegmentNumber(9),
            },
        ];
        let m = selection_repair_message(&errors, true, 30);
        assert_eq!(m.role, Role::User);
        assert!(m.content.starts_with("Your reply was cut off"));
        assert!(
            m.content
                .contains("- /decisions/1/0: this number is used twice")
        );
        assert!(
            m.content
                .contains("- /action_items/0/0: 9 is not one of the item numbers")
        );
        assert!(!m.content.contains("segment"));
        // Bounded like the summary repair.
        let many: Vec<SummaryError> = (0..500)
            .map(|i| SummaryError {
                path: format!("/decisions/{i}/0"),
                kind: SummaryErrorKind::Repetition,
            })
            .collect();
        let m = selection_repair_message(&many, false, 500);
        assert!(estimate_tokens(&m.content) + TOKENS_PER_MESSAGE <= REPAIR_MAX_TOKENS);
        assert!(m.content.contains("more\n"));
    }
}
