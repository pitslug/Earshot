//! `accept m4`: summaries (T4.5, PLAN.md 3.7 and 4.2 `sum.*`).
//!
//! Every committed case in `fixtures/llm` is replayed through `earshot_llm`'s
//! `LlmSummarizer` against a loopback mock endpoint (one fresh server per case), with
//! an `earshot-net` client whose allowlist holds only that endpoint, speaking the case's
//! provider (OpenAI-compatible, or the Anthropic Messages API since T6.22). Every case is
//! run.
//!
//! Cases recorded with `expect.outcome: "invalid"` (`gives_up`, `anthropic_gives_up`)
//! are deliberately unrecoverable: they prove the client stops after its repair rounds.
//! They must end in `LlmError::Invalid { attempts: 3 }` with their recorded flow (checked
//! below). The case recorded with `expect.outcome: "http"` (`anthropic_auth_error`)
//! proves an error status that is not a format rejection ends the run without a
//! fallback: it must end in `LlmError::Http` with the status of its last exchange. Both
//! kinds are excluded from `sum.schema_replay`, whose denominator is the cases recorded
//! as recoverable (`outcome` valid, the default). They return no summary, so they add
//! nothing to `sum.grounding`. Exactly the cases in [`NOT_VALID_CASES`] may carry a
//! non-valid outcome, so relabelling a recoverable case cannot shrink the denominator.
//!
//! - `sum.schema_replay` (%): recoverable cases whose run ends with a valid summary, over
//!   all recoverable cases. A
//!   case is valid only when the summarizer returned a summary, that summary passes
//!   `earshot_core::summary::validate_summary` against the case's transcript (re-checked
//!   here, independently of the client's own validation), and the client sent no request
//!   beyond the recorded exchanges.
//! - `sum.grounding` (count): over the accepted summaries (every summary the summarizer
//!   returned), citations that do not name a transcript segment plus decisions and action
//!   items that cite nothing. Counted by walking the typed summary here, not with the
//!   validator.
//!
//! Checks: the replay set covers the PLAN.md T4.4 kinds, each case follows its recorded
//! request flow (response formats in order, repair rounds, every exchange used, and the
//! recorded outcome, so an `invalid` case that returns a summary fails), and the
//! audit log shows requests only to the mock endpoint (PLAN.md 3.9).

use std::collections::BTreeSet;
use std::fmt::Write as _;

use anyhow::{Result, anyhow};
use earshot_core::identity::EntityId;
use earshot_core::summary::{SummaryV1, validate_summary};
use earshot_llm::{LlmError, MAX_REPAIRS, Provider};
use earshot_llm_replay::replay::{CaseOutcome, Outcome, ReplayCase, load_cases, run_case_blocking};
use earshot_net::AuditOutcome;

use super::{Check, Ctx, Measurement, RunOutput};

/// The OpenAI-compatible case recorded as unrecoverable (`expect.outcome: "invalid"`).
pub const GIVES_UP_CASE: &str = "gives_up";

/// The Anthropic case recorded as unrecoverable.
pub const ANTHROPIC_GIVES_UP_CASE: &str = "anthropic_gives_up";

/// The Anthropic case recorded as ending in an HTTP error (`expect.outcome: "http"`).
pub const ANTHROPIC_HTTP_CASE: &str = "anthropic_auth_error";

/// The only cases that may be recorded with a non-valid outcome, with that outcome.
pub const NOT_VALID_CASES: &[(&str, Outcome)] = &[
    (ANTHROPIC_HTTP_CASE, Outcome::Http),
    (ANTHROPIC_GIVES_UP_CASE, Outcome::Invalid),
    (GIVES_UP_CASE, Outcome::Invalid),
];

/// Case names the replay set must contain (PLAN.md T4.4 plus the format fallbacks, and
/// the T6.22 Anthropic cases).
pub const REQUIRED_CASES: &[&str] = &[
    "valid",
    "fenced_json",
    "trailing_prose",
    "missing_fields",
    "bad_citations",
    "truncated",
    "json_schema_unsupported",
    "json_object_unsupported",
    GIVES_UP_CASE,
    "anthropic_valid",
    "anthropic_missing_fields",
    "anthropic_structured_output_unsupported",
    "anthropic_tools_unsupported",
    "anthropic_max_tokens",
    ANTHROPIC_HTTP_CASE,
    ANTHROPIC_GIVES_UP_CASE,
];

pub(super) fn run(ctx: &Ctx) -> Result<RunOutput> {
    let dir = ctx.root.join("fixtures").join("llm");
    let cases = load_cases(&dir).map_err(|e| anyhow!(e))?;
    let mut outcomes = Vec::with_capacity(cases.len());
    for case in &cases {
        let out = run_case_blocking(case)
            .map_err(|e| anyhow!("replay case {}: cannot start the mock: {e}", case.name))?;
        outcomes.push(out);
    }
    Ok(evaluate(&cases, &outcomes))
}

/// Citations in `summary` that are not in `ids`, plus decisions and action items with no
/// citation at all.
pub fn invalid_citations(summary: &SummaryV1, ids: &BTreeSet<EntityId>) -> usize {
    let unknown = |cited: &[EntityId]| cited.iter().filter(|id| !ids.contains(id)).count();
    let bullets: usize = summary
        .enhanced_notes
        .iter()
        .flat_map(|s| &s.bullets)
        .map(|b| unknown(&b.source_segment_ids))
        .sum();
    let decisions: usize = summary
        .decisions
        .iter()
        .map(|d| unknown(&d.source_segment_ids) + usize::from(d.source_segment_ids.is_empty()))
        .sum();
    let actions: usize = summary
        .action_items
        .iter()
        .map(|a| unknown(&a.source_segment_ids) + usize::from(a.source_segment_ids.is_empty()))
        .sum();
    bullets + decisions + actions
}

/// Builds the measurements and checks from replayed `outcomes` (one per case, same order).
pub fn evaluate(cases: &[ReplayCase], outcomes: &[CaseOutcome]) -> RunOutput {
    let mut valid = 0usize;
    let mut recoverable = 0usize;
    let mut grounding = 0usize;
    let mut detail = String::new();
    let mut flow_problems = Vec::new();
    let mut net_problems = Vec::new();

    for (case, out) in cases.iter().zip(outcomes) {
        let ids = case.segment_ids();
        let counted = case.expect.outcome == Outcome::Valid;
        recoverable += usize::from(counted);
        let status = match &out.result {
            Ok(summary) => {
                let bad = invalid_citations(summary, &ids);
                grounding += bad;
                let errors = validate_summary(summary, &ids);
                if errors.is_empty() && out.unscripted == 0 {
                    valid += usize::from(counted);
                    format!("valid ({bad} invalid citations)")
                } else {
                    format!(
                        "INVALID ({} validator errors, {} unscripted requests, {bad} invalid citations)",
                        errors.len(),
                        out.unscripted
                    )
                }
            }
            Err(e) => format!("FAILED: {e}"),
        };
        let status = if counted {
            status
        } else {
            format!("{status} [expected invalid, not counted]")
        };
        let _ = write!(
            detail,
            "{}: {status}; requests {}, repairs {}, formats {}; ",
            case.name,
            out.report.requests,
            out.report.repairs,
            out.modes_sent.join(">")
        );

        let outcome_ok = match case.expect.outcome {
            Outcome::Valid => out.result.is_ok(),
            Outcome::Invalid => matches!(
                out.result,
                Err(LlmError::Invalid { attempts, .. }) if attempts == MAX_REPAIRS + 1
            ),
            Outcome::Http => matches!(
                out.result,
                Err(LlmError::Http { status, .. })
                    if Some(status) == case.script.last().map(|r| r.status)
            ),
        };
        if !outcome_ok
            || out.modes_sent != case.expect.modes
            || out.report.repairs != case.expect.repairs
            || out.unused != 0
            || out.unscripted != 0
        {
            flow_problems.push(format!(
                "{}: outcome {} (expected {:?}), formats {:?} (expected {:?}), repairs {} (expected {}), {} unused, {} unscripted",
                case.name,
                match &out.result {
                    Ok(_) => "summary".to_string(),
                    Err(e) => e.to_string(),
                },
                case.expect.outcome,
                out.modes_sent,
                case.expect.modes,
                out.report.repairs,
                case.expect.repairs,
                out.unused,
                out.unscripted
            ));
        }

        let host = out.endpoint.host_str().unwrap_or_default();
        let port = out.endpoint.port_or_known_default().unwrap_or(0);
        for e in &out.audit {
            if e.host != host || e.port != port || e.outcome != AuditOutcome::Completed {
                net_problems.push(format!(
                    "{}: {} {}:{}{} {:?}",
                    case.name, e.method, e.host, e.port, e.path, e.outcome
                ));
            }
        }
        if out.audit.len() != out.report.requests {
            net_problems.push(format!(
                "{}: {} audit entries for {} requests",
                case.name,
                out.audit.len(),
                out.report.requests
            ));
        }
    }

    let total = outcomes.len();
    let percent = if recoverable == 0 {
        0.0
    } else {
        100.0 * valid as f64 / recoverable as f64
    };
    let mut schema = Measurement::value("sum.schema_replay", percent, "%");
    schema.detail = Some(format!(
        "{valid}/{recoverable} recoverable cases valid after repair ({} expected-invalid cases excluded). {detail}",
        total - recoverable
    ));
    let mut ground = Measurement::value("sum.grounding", grounding as f64, "count");
    ground.detail = Some(format!("over {total} replayed cases"));

    let names: BTreeSet<&str> = cases.iter().map(|c| c.name.as_str()).collect();
    let missing: Vec<&str> = REQUIRED_CASES
        .iter()
        .copied()
        .filter(|n| !names.contains(n))
        .collect();
    // Relabelling a recoverable case as `invalid` or `http` would drop it from
    // `sum.schema_replay`: exactly the cases in NOT_VALID_CASES carry those outcomes, and
    // every other case is valid.
    let mut label_problems = Vec::new();
    let mut not_valid: Vec<(&str, Outcome)> = cases
        .iter()
        .filter(|c| c.expect.outcome != Outcome::Valid)
        .map(|c| (c.name.as_str(), c.expect.outcome))
        .collect();
    not_valid.sort_by_key(|(n, _)| *n);
    if not_valid != NOT_VALID_CASES {
        label_problems.push(format!(
            "expected exactly {NOT_VALID_CASES:?} to be recorded as not valid, found {not_valid:?}"
        ));
    }
    for c in cases {
        if REQUIRED_CASES.contains(&c.name.as_str())
            && !NOT_VALID_CASES.iter().any(|(n, _)| *n == c.name)
            && c.expect.outcome != Outcome::Valid
        {
            label_problems.push(format!("required case {} is not recorded as valid", c.name));
        }
    }
    // Both providers are covered.
    for p in [Provider::OpenAi, Provider::Anthropic] {
        if !cases.iter().any(|c| c.provider == p) {
            label_problems.push(format!("no {} replay case", p.name()));
        }
    }
    let mut coverage_problems = Vec::new();
    if !missing.is_empty() {
        coverage_problems.push(format!("missing: {}", missing.join(", ")));
    }
    if cases.len() != outcomes.len() {
        coverage_problems.push(format!(
            "{} cases but {} outcomes",
            cases.len(),
            outcomes.len()
        ));
    }
    coverage_problems.extend(label_problems);
    let checks = vec![
        Check::new(
            "replay set covers the required case kinds",
            coverage_problems.is_empty(),
            if coverage_problems.is_empty() {
                format!(
                    "{} cases: {}; only {} are recorded as not valid",
                    cases.len(),
                    names.into_iter().collect::<Vec<_>>().join(", "),
                    NOT_VALID_CASES
                        .iter()
                        .map(|(n, o)| format!("{n} ({o:?})"))
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            } else {
                coverage_problems.join("; ")
            },
        ),
        Check::new(
            "each case follows its recorded request flow",
            flow_problems.is_empty(),
            if flow_problems.is_empty() {
                "outcomes, response formats, repair rounds and exchanges as recorded".to_string()
            } else {
                flow_problems.join("; ")
            },
        ),
        Check::new(
            "requests went only to the loopback mock endpoint",
            net_problems.is_empty(),
            if net_problems.is_empty() {
                "every audit entry is 127.0.0.1 on the case's mock port".to_string()
            } else {
                net_problems.join("; ")
            },
        ),
    ];

    RunOutput {
        measurements: vec![schema, ground],
        checks,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use earshot_core::summary::{ActionItem, Decision, NoteBullet, NoteSection};

    fn id(n: u8) -> EntityId {
        EntityId::from_bytes([n; 16])
    }

    fn summary(bullet: Vec<EntityId>, decision: Vec<EntityId>, action: Vec<EntityId>) -> SummaryV1 {
        SummaryV1 {
            title: "t".into(),
            tldr: "t".into(),
            enhanced_notes: vec![NoteSection {
                heading: "h".into(),
                bullets: vec![NoteBullet {
                    text: "b".into(),
                    source_segment_ids: bullet,
                }],
            }],
            decisions: vec![Decision {
                text: "d".into(),
                source_segment_ids: decision,
            }],
            action_items: vec![ActionItem {
                owner: "o".into(),
                task: "t".into(),
                due: None,
                source_segment_ids: action,
            }],
            open_questions: vec![],
            topics: vec![],
        }
    }

    #[test]
    fn m4_invalid_citations_counts_unknown_ids_and_uncited_items() {
        let ids: BTreeSet<EntityId> = [id(1), id(2)].into_iter().collect();
        assert_eq!(
            invalid_citations(&summary(vec![], vec![id(1)], vec![id(2), id(1)]), &ids),
            0,
            "empty bullet citations are allowed"
        );
        assert_eq!(
            invalid_citations(&summary(vec![id(9)], vec![id(1), id(8)], vec![id(7)]), &ids),
            3
        );
        assert_eq!(
            invalid_citations(&summary(vec![], vec![], vec![]), &ids),
            2,
            "uncited decision and action item"
        );
    }

    #[test]
    fn m4_required_cases_are_committed() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
        let cases = load_cases(&root.join("fixtures").join("llm")).unwrap();
        let names: BTreeSet<&str> = cases.iter().map(|c| c.name.as_str()).collect();
        for n in REQUIRED_CASES {
            assert!(names.contains(n), "{n}");
        }
    }

    #[test]
    fn m4_a_failed_or_ungrounded_case_is_counted() {
        use earshot_llm::{LlmError, ResponseMode, SummaryReport};
        use earshot_llm_replay::replay::run_case_blocking;

        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
        let cases = load_cases(&root.join("fixtures").join("llm")).unwrap();
        let mut outcomes: Vec<CaseOutcome> = cases
            .iter()
            .map(|c| run_case_blocking(c).unwrap())
            .collect();
        let all = evaluate(&cases, &outcomes);
        assert_eq!(all.measurements[0].value, Some(100.0));
        assert_eq!(all.measurements[1].value, Some(0.0));
        assert!(all.checks.iter().all(|c| c.pass), "{:?}", all.checks);

        // One recoverable case fails outright, another returns a summary citing an
        // unknown segment.
        let pos = |name: &str| cases.iter().position(|c| c.name == name).unwrap();
        let (failed, ungrounded) = (pos("bad_citations"), pos("fenced_json"));
        outcomes[failed].result = Err(LlmError::Cancelled);
        outcomes[failed].report = SummaryReport {
            requests: 0,
            repairs: 0,
            format_fallbacks: 0,
            mode: ResponseMode::JsonSchema,
            map_parts: 0,
            reduce_steps: 0,
            joined_parts: 0,
            duplicates_dropped: 0,
        };
        outcomes[ungrounded]
            .result
            .as_mut()
            .expect("fenced_json returns a summary")
            .decisions[0]
            .source_segment_ids = vec![id(0xEE)];
        let out = evaluate(&cases, &outcomes);
        let n = cases
            .iter()
            .filter(|c| c.expect.outcome == Outcome::Valid)
            .count() as f64;
        assert_eq!(
            n,
            (cases.len() - NOT_VALID_CASES.len()) as f64,
            "the not-valid cases are not counted"
        );
        assert_eq!(out.measurements[0].value, Some(100.0 * (n - 2.0) / n));
        assert_eq!(out.measurements[1].value, Some(1.0));
        assert!(!out.checks[1].pass, "flow check sees the empty run");
    }

    #[test]
    fn m4_an_expected_invalid_case_that_returns_a_summary_fails_the_flow_check() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
        let cases = load_cases(&root.join("fixtures").join("llm")).unwrap();
        let mut outcomes: Vec<CaseOutcome> = cases
            .iter()
            .map(|c| run_case_blocking(c).unwrap())
            .collect();
        let g = cases.iter().position(|c| c.name == "gives_up").unwrap();
        let v = cases.iter().position(|c| c.name == "valid").unwrap();
        let summary = outcomes[v].result.as_ref().unwrap().clone();
        outcomes[g].result = Ok(summary);
        let out = evaluate(&cases, &outcomes);
        assert_eq!(
            out.measurements[0].value,
            Some(100.0),
            "not counted either way"
        );
        assert!(!out.checks[1].pass);
        assert!(
            out.checks[1]
                .detail
                .as_deref()
                .unwrap()
                .contains("gives_up")
        );
    }

    #[test]
    fn m4_a_required_case_relabelled_invalid_fails_the_coverage_check() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
        let mut cases = load_cases(&root.join("fixtures").join("llm")).unwrap();
        let outcomes: Vec<CaseOutcome> = cases
            .iter()
            .map(|c| run_case_blocking(c).unwrap())
            .collect();
        assert!(evaluate(&cases, &outcomes).checks[0].pass);

        let i = cases
            .iter()
            .position(|c| c.name == "missing_fields")
            .unwrap();
        cases[i].expect.outcome = Outcome::Invalid;
        let out = evaluate(&cases, &outcomes);
        assert!(!out.checks[0].pass);
        let detail = out.checks[0].detail.clone().unwrap();
        assert!(detail.contains("missing_fields"), "{detail}");

        // A required Anthropic case relabelled `http` fails it too.
        cases[i].expect.outcome = Outcome::Valid;
        assert!(evaluate(&cases, &outcomes).checks[0].pass);
        let a = cases
            .iter()
            .position(|c| c.name == "anthropic_missing_fields")
            .unwrap();
        cases[a].expect.outcome = Outcome::Http;
        let out = evaluate(&cases, &outcomes);
        assert!(!out.checks[0].pass);
        assert!(
            out.checks[0]
                .detail
                .clone()
                .unwrap()
                .contains("anthropic_missing_fields")
        );
        cases[a].expect.outcome = Outcome::Valid;

        // gives_up relabelled valid: the not-valid set no longer matches.
        let g = cases.iter().position(|c| c.name == GIVES_UP_CASE).unwrap();
        cases[g].expect.outcome = Outcome::Valid;
        assert!(!evaluate(&cases, &outcomes).checks[0].pass);
    }

    #[test]
    fn m4_an_http_case_that_falls_back_or_succeeds_fails_the_flow_check() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
        let cases = load_cases(&root.join("fixtures").join("llm")).unwrap();
        let mut outcomes: Vec<CaseOutcome> = cases
            .iter()
            .map(|c| run_case_blocking(c).unwrap())
            .collect();
        assert!(evaluate(&cases, &outcomes).checks[1].pass);
        let h = cases
            .iter()
            .position(|c| c.name == ANTHROPIC_HTTP_CASE)
            .unwrap();
        // Another status than the recorded one (for example after a fallback).
        outcomes[h].result = Err(LlmError::Http {
            status: 400,
            message: "x".into(),
        });
        let out = evaluate(&cases, &outcomes);
        assert!(!out.checks[1].pass);
        assert!(
            out.checks[1]
                .detail
                .as_deref()
                .unwrap()
                .contains(ANTHROPIC_HTTP_CASE)
        );
        // A summary where the recorded flow ends in an error.
        let v = cases
            .iter()
            .position(|c| c.name == "anthropic_valid")
            .unwrap();
        outcomes[h].result = Ok(outcomes[v].result.as_ref().unwrap().clone());
        assert!(!evaluate(&cases, &outcomes).checks[1].pass);
    }
}
