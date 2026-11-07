//! Replay of the committed `fixtures/llm` cases through a loopback mock (T4.4). Every test
//! name contains `replay` so `cargo test -p earshot-llm replay` selects them.

use std::path::Path;
use std::sync::Arc;

use earshot_core::asr::CancelToken;
use earshot_core::summary::{Summarizer, validate_summary};
use earshot_llm::{ANTHROPIC_VERSION, ChatClient, LlmConfig, LlmError, LlmSummarizer, Provider};
use earshot_llm_replay::replay::{Outcome, fixtures_dir, load_cases, run_case};
use earshot_net::{Allowlist, NetClient, NetConfig, SystemClock};

const REQUIRED: [&str; 16] = [
    "valid",
    "fenced_json",
    "trailing_prose",
    "missing_fields",
    "bad_citations",
    "truncated",
    "json_schema_unsupported",
    "json_object_unsupported",
    "gives_up",
    "anthropic_valid",
    "anthropic_missing_fields",
    "anthropic_structured_output_unsupported",
    "anthropic_tools_unsupported",
    "anthropic_max_tokens",
    "anthropic_auth_error",
    "anthropic_gives_up",
];

/// The cases recorded as not ending in a summary, with their outcome; every other case
/// is recorded as valid.
const NOT_VALID: [(&str, Outcome); 3] = [
    ("gives_up", Outcome::Invalid),
    ("anthropic_gives_up", Outcome::Invalid),
    ("anthropic_auth_error", Outcome::Http),
];

#[test]
fn replay_cases_cover_the_required_kinds() {
    let cases = load_cases(&fixtures_dir()).unwrap();
    let names: Vec<&str> = cases.iter().map(|c| c.name.as_str()).collect();
    for n in REQUIRED {
        assert!(names.contains(&n), "missing case {n} in {names:?}");
    }
    for c in &cases {
        assert!(!c.description.is_empty(), "{}", c.name);
        assert_eq!(
            c.script.len(),
            c.expect.modes.len(),
            "{}: one recorded exchange per expected request",
            c.name
        );
        assert!(!c.input.segments.is_empty(), "{}", c.name);
    }
    let not_valid: Vec<(&str, Outcome)> = cases
        .iter()
        .filter(|c| c.expect.outcome != Outcome::Valid)
        .map(|c| (c.name.as_str(), c.expect.outcome))
        .collect();
    let mut expected = NOT_VALID.to_vec();
    expected.sort_by_key(|(n, _)| *n);
    assert_eq!(not_valid, expected);
    for c in &cases {
        let anthropic = c.name.starts_with("anthropic_");
        assert_eq!(
            c.provider == Provider::Anthropic,
            anthropic,
            "{}: provider {:?}",
            c.name,
            c.provider
        );
    }
}

#[test]
fn replay_missing_fixtures_fail_loudly() {
    let err = load_cases(Path::new("no/such/fixtures/llm")).unwrap_err();
    assert!(err.contains("committed in fixtures/llm"), "{err}");
}

#[tokio::test]
async fn replay_every_case_ends_as_recorded() {
    for case in load_cases(&fixtures_dir()).unwrap() {
        let out = run_case(&case).await.unwrap();
        match (case.expect.outcome, &out.result) {
            (Outcome::Valid, Ok(summary)) => assert!(
                validate_summary(summary, &case.segment_ids()).is_empty(),
                "{}",
                case.name
            ),
            (Outcome::Invalid, Err(LlmError::Invalid { attempts: 3, .. })) => {}
            (Outcome::Http, Err(LlmError::Http { status, .. }))
                if *status == case.script.last().unwrap().status => {}
            (expected, got) => panic!("{}: expected {expected:?}, got {got:?}", case.name),
        }
        assert_eq!(out.modes_sent, case.expect.modes, "{}", case.name);
        for r in &out.requests {
            // Every provider asks for the numbered reply schema (T6.20): citations are
            // integers, never segment UUIDs, and each request carries its own budget.
            let body = r.json();
            let text = body.to_string();
            // The stored schema's citations are `format: uuid`; neither the reply schema nor
            // the numbered prompt mentions it.
            assert!(!text.contains("uuid"), "{}", case.name);
            for schema in [
                body.pointer("/output_config/format/schema"),
                body.pointer("/tools/0/input_schema"),
                body.pointer("/response_format/json_schema/schema"),
            ]
            .into_iter()
            .flatten()
            {
                let cites = &schema["$defs"]["Decision"]["properties"]["source_segment_ids"];
                assert_eq!(cites["items"]["type"], "integer", "{}", case.name);
            }
            assert!(
                body["max_tokens"].as_u64().is_some_and(|t| t > 0),
                "{}",
                case.name
            );
            match case.provider {
                Provider::OpenAi => assert_eq!(r.path, "/v1/chat/completions", "{}", case.name),
                Provider::Anthropic => {
                    assert_eq!(r.path, "/v1/messages", "{}", case.name);
                    assert_eq!(
                        r.header("anthropic-version"),
                        Some(ANTHROPIC_VERSION),
                        "{}",
                        case.name
                    );
                    assert!(r.json().get("temperature").is_none(), "{}", case.name);
                }
            }
        }
        assert_eq!(out.report.repairs, case.expect.repairs, "{}", case.name);
        assert_eq!((out.unused, out.unscripted), (0, 0), "{}", case.name);
        let port = out.endpoint.port().unwrap();
        assert_eq!(out.audit.len(), out.report.requests, "{}", case.name);
        for e in &out.audit {
            assert_eq!(
                (e.host.as_str(), e.port),
                ("127.0.0.1", port),
                "{}",
                case.name
            );
        }
    }
}

#[tokio::test]
async fn replay_harness_reports_an_unrepaired_case_as_invalid() {
    // Replace the final (valid) reply of the two-repair case with its first, broken one:
    // the run must fail after three attempts, so the harness cannot pass vacuously.
    let mut case = load_cases(&fixtures_dir())
        .unwrap()
        .into_iter()
        .find(|c| c.name == "bad_citations")
        .unwrap();
    let first = case.script[0].clone();
    *case.script.last_mut().unwrap() = first;
    let out = run_case(&case).await.unwrap();
    assert!(
        matches!(out.result, Err(LlmError::Invalid { attempts: 3, .. })),
        "{:?}",
        out.result
    );
}

/// Optional live check against a real endpoint. Never gates: ignored by default.
#[tokio::test]
#[ignore = "live LLM: set EARSHOT_LLM_URL (and optionally EARSHOT_LLM_MODEL), run with --ignored"]
async fn replay_live_endpoint_summarises_the_fixture_meeting() {
    let url = std::env::var("EARSHOT_LLM_URL")
        .expect("EARSHOT_LLM_URL is not set; this test only runs against a configured endpoint");
    let model = std::env::var("EARSHOT_LLM_MODEL").unwrap_or_else(|_| "local-model".into());
    let config = LlmConfig::new(&url, model).unwrap();
    let mut allow = Allowlist::new();
    allow.allow_url(&config.endpoint).unwrap();
    let net = NetClient::new(NetConfig::new(allow, Arc::new(SystemClock))).unwrap();
    let case = load_cases(&fixtures_dir())
        .unwrap()
        .into_iter()
        .find(|c| c.name == "valid")
        .unwrap();
    let summarizer = LlmSummarizer::new(ChatClient::new(net, config));
    let summary = summarizer
        .summarize(&case.input, &CancelToken::new())
        .await
        .unwrap();
    assert!(validate_summary(&summary, &case.segment_ids()).is_empty());
}
