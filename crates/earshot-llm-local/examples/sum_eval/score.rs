//! Scoring one summary against a human abstractive summary (T6.20). Rules fixed before
//! measuring:
//!
//! - ROUGE (`earshot_core::metrics::rouge`, a port of Google's `rouge-score`, no
//!   stemming): the summary's narrative text is its `tldr` followed by every
//!   `enhanced_notes` bullet, one per line; the reference is the human `abstract`, one
//!   sentence per line. The headline number is ROUGE-L F1; ROUGE-1, ROUGE-2 and ROUGE-Lsum
//!   are reported beside it. A meeting without a valid summary scores 0 on every ROUGE
//!   type (the failure is counted, never dropped).
//! - Decisions and action items caught: `earshot_core::metrics::items` (its module docs
//!   hold the matching rule) of the summary's `decisions[].text` against the human
//!   `decisions` sentences and the summary's `action_items[].task` against the human
//!   `actions` sentences. A failed meeting has no summary items: its reference items count
//!   as missed. Both rules of that module are reported: `decisions`/`actions` use the
//!   token-F1 rule fixed before measuring; `decisions_rr`/`actions_rr` use the
//!   reference-recall rule defined after seeing dev data;
//!   `decisions_cov`/`actions_cov` the many-to-one coverage rule defined after seeing data
//!   (`items::covered_items`: only caught / reference is meaningful).

use earshot_core::metrics::items::{
    ItemCounts, ItemCoverage, ItemMatch, ItemRule, covered_items, match_items_by,
};
use earshot_core::metrics::rouge::{RougeScores, rouge};
use earshot_core::summary::SummaryV1;
use serde_json::{Value, json};

use crate::ami::AbsSumm;

/// The narrative text of `s` that ROUGE compares (tldr, then every bullet; one per line).
#[must_use]
pub fn summary_text(s: &SummaryV1) -> String {
    let mut lines = vec![s.tldr.clone()];
    for section in &s.enhanced_notes {
        for b in &section.bullets {
            lines.push(b.text.clone());
        }
    }
    lines.join("\n")
}

/// The reference text: the abstract, one sentence per line.
#[must_use]
pub fn reference_text(r: &AbsSumm) -> String {
    r.abstract_.join("\n")
}

/// Scores of one meeting.
#[derive(Debug, Clone)]
pub struct MeetingScore {
    /// ROUGE of the narrative text (all zero without a summary).
    pub rouge: RougeScores,
    /// Decisions caught.
    pub decisions: ItemCounts,
    /// The decision matches.
    pub decision_matches: Vec<ItemMatch>,
    /// Action items caught.
    pub actions: ItemCounts,
    /// The action matches.
    pub action_matches: Vec<ItemMatch>,
    /// Decisions caught under the reference-recall rule (defined after seeing dev data).
    pub decisions_rr: ItemCounts,
    /// Those matches.
    pub decision_matches_rr: Vec<ItemMatch>,
    /// Action items caught under the reference-recall rule.
    pub actions_rr: ItemCounts,
    /// Those matches.
    pub action_matches_rr: Vec<ItemMatch>,
    /// Decisions caught under the coverage rule (many-to-one, defined after seeing data).
    pub decisions_cov: ItemCounts,
    /// The covered reference decisions.
    pub decision_cover: Vec<ItemCoverage>,
    /// Action items caught under the coverage rule.
    pub actions_cov: ItemCounts,
    /// The covered reference actions.
    pub action_cover: Vec<ItemCoverage>,
}

/// Scores `summary` (None when the run failed) against `reference`.
#[must_use]
pub fn score(summary: Option<&SummaryV1>, reference: &AbsSumm) -> MeetingScore {
    let refd: Vec<&str> = reference.decisions.iter().map(String::as_str).collect();
    let refa: Vec<&str> = reference.actions.iter().map(String::as_str).collect();
    let (sysd, sysa): (Vec<&str>, Vec<&str>) = match summary {
        Some(s) => (
            s.decisions.iter().map(|d| d.text.as_str()).collect(),
            s.action_items.iter().map(|a| a.task.as_str()).collect(),
        ),
        None => (Vec::new(), Vec::new()),
    };
    let (decisions, decision_matches) = match_items_by(ItemRule::TokenF1, &sysd, &refd);
    let (actions, action_matches) = match_items_by(ItemRule::TokenF1, &sysa, &refa);
    let (decisions_rr, decision_matches_rr) =
        match_items_by(ItemRule::ReferenceRecall, &sysd, &refd);
    let (actions_rr, action_matches_rr) = match_items_by(ItemRule::ReferenceRecall, &sysa, &refa);
    let (decisions_cov, decision_cover) = covered_items(&sysd, &refd);
    let (actions_cov, action_cover) = covered_items(&sysa, &refa);
    MeetingScore {
        rouge: summary.map_or_else(RougeScores::default, |s| {
            rouge(&reference_text(reference), &summary_text(s))
        }),
        decisions,
        decision_matches,
        actions,
        action_matches,
        decisions_rr,
        decision_matches_rr,
        actions_rr,
        action_matches_rr,
        decisions_cov,
        decision_cover,
        actions_cov,
        action_cover,
    }
}

fn cover_json(c: &ItemCounts, cover: &[ItemCoverage]) -> Value {
    json!({
        "caught": c.caught,
        "reference": c.reference,
        "summary": c.summary,
        "covered": cover
            .iter()
            .map(|m| json!({"reference": m.reference, "score": m.score}))
            .collect::<Vec<_>>(),
    })
}

fn counts_json(c: &ItemCounts, matches: &[ItemMatch]) -> Value {
    json!({
        "caught": c.caught,
        "reference": c.reference,
        "summary": c.summary,
        "matches": matches
            .iter()
            .map(|m| json!({"summary": m.summary, "reference": m.reference, "score": m.score}))
            .collect::<Vec<_>>(),
    })
}

impl MeetingScore {
    /// JSON for the report.
    #[must_use]
    pub fn to_json(&self) -> Value {
        let r = |s: earshot_core::metrics::rouge::RougeScore| json!({"p": s.precision, "r": s.recall, "f": s.fmeasure});
        json!({
            "rouge1": r(self.rouge.rouge1),
            "rouge2": r(self.rouge.rouge2),
            "rougeL": r(self.rouge.rouge_l),
            "rougeLsum": r(self.rouge.rouge_lsum),
            "decisions": counts_json(&self.decisions, &self.decision_matches),
            "actions": counts_json(&self.actions, &self.action_matches),
            "decisions_rr": counts_json(&self.decisions_rr, &self.decision_matches_rr),
            "actions_rr": counts_json(&self.actions_rr, &self.action_matches_rr),
            "decisions_cov": cover_json(&self.decisions_cov, &self.decision_cover),
            "actions_cov": cover_json(&self.actions_cov, &self.action_cover),
        })
    }
}

#[cfg(test)]
mod tests {
    use earshot_core::summary::{ActionItem, Decision, NoteBullet, NoteSection};

    use super::*;

    fn summary() -> SummaryV1 {
        SummaryV1 {
            title: "Kick-off".into(),
            tldr: "The team kicked off the remote control project.".into(),
            enhanced_notes: vec![NoteSection {
                heading: "Budget".into(),
                bullets: vec![NoteBullet {
                    text: "The remote sells for 25 euros.".into(),
                    source_segment_ids: vec![],
                }],
            }],
            decisions: vec![Decision {
                text: "The remote will have an LCD screen".into(),
                source_segment_ids: vec![],
            }],
            action_items: vec![ActionItem {
                owner: "Me".into(),
                task: "Prepare the functional design".into(),
                due: None,
                source_segment_ids: vec![],
            }],
            open_questions: vec![],
            topics: vec![],
        }
    }

    fn reference() -> AbsSumm {
        AbsSumm {
            abstract_: vec![
                "The project manager kicked off the remote control project.".into(),
                "The remote will sell for 25 euros.".into(),
            ],
            actions: vec!["The group will prepare the functional design.".into()],
            decisions: vec!["NA.".into()],
            problems: vec![],
        }
    }

    #[test]
    fn score_text_is_tldr_then_bullets_and_the_abstract() {
        assert_eq!(
            summary_text(&summary()),
            "The team kicked off the remote control project.\nThe remote sells for 25 euros."
        );
        assert_eq!(reference_text(&reference()).lines().count(), 2);
    }

    #[test]
    fn score_counts_items_and_na_has_no_reference_decision() {
        let s = score(Some(&summary()), &reference());
        assert!(s.rouge.rouge_l.fmeasure > 0.5);
        // "NA." is not a decision: the summary's decision is an extra item.
        assert_eq!(
            (
                s.decisions.caught,
                s.decisions.reference,
                s.decisions.summary
            ),
            (0, 0, 1)
        );
        // {prepare, functional, design} vs {group, prepare, functional, design}: F1 6/7.
        assert_eq!(
            (s.actions.caught, s.actions.reference, s.actions.summary),
            (1, 1, 1)
        );
        // The reference-recall rule is scored beside it, on the same items.
        assert_eq!((s.actions_rr.caught, s.decisions_rr.summary), (1, 1));
        let j = s.to_json();
        assert_eq!(j["actions_rr"]["caught"], 1);
        assert_eq!(j["decisions"]["summary"], 1);
    }

    #[test]
    fn score_reference_recall_counts_a_long_paraphrase() {
        let mut sum = summary();
        sum.decisions[0].text = "The team agreed to proceed with the concept of changeable case \
                                 covers for the remote control, allowing for different colors and styles."
            .into();
        let mut r = reference();
        r.decisions = vec!["The remote will have changeable case covers.".into()];
        let s = score(Some(&sum), &r);
        assert_eq!(s.decisions.caught, 0);
        assert_eq!(s.decisions_rr.caught, 1);
        assert_eq!(s.decisions_cov.caught, 1);
    }

    #[test]
    fn score_coverage_counts_a_bundled_decision_listed_as_separate_items() {
        let mut sum = summary();
        sum.decisions = [
            "Use an LCD screen.",
            "Rubber buttons.",
            "A kinetic battery.",
        ]
        .into_iter()
        .map(|t| Decision {
            text: t.into(),
            source_segment_ids: vec![],
        })
        .collect();
        let mut r = reference();
        r.decisions = vec![
            "The remote will have an LCD screen, rubber buttons and a kinetic battery.".into(),
        ];
        let s = score(Some(&sum), &r);
        assert_eq!((s.decisions.caught, s.decisions_rr.caught), (0, 0));
        assert_eq!((s.decisions_cov.caught, s.decisions_cov.reference), (1, 1));
        let j = s.to_json();
        assert_eq!(j["decisions_cov"]["caught"], 1);
        assert_eq!(j["actions_cov"]["caught"], 1);
    }

    #[test]
    fn score_without_a_summary_is_zero_and_misses_every_item() {
        let s = score(None, &reference());
        assert_eq!(s.rouge.rouge_l.fmeasure, 0.0);
        assert_eq!(s.rouge.rouge_lsum.fmeasure, 0.0);
        assert_eq!(
            (s.actions.caught, s.actions.reference, s.actions.summary),
            (0, 1, 0)
        );
    }
}
