//! The native Anthropic Messages API client (T6.22) against the loopback mock endpoint.
//! Every test name contains `anthropic` so `cargo test -p earshot-llm anthropic` selects
//! them. No test calls a real endpoint.

use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use earshot_core::asr::{CancelToken, MeetingTime};
use earshot_core::identity::{EntityId, ManualClock};
use earshot_core::prompt::{MeetingInfo, PromptPlan, PromptSegment, SummaryInput, Template, plan};
use earshot_core::summary::{Summarizer, SummaryV1, validate_summary};
use earshot_llm::{
    ANTHROPIC_DEFAULT_BUDGET, ANTHROPIC_VERSION, ApiKey, LlmClient, LlmConfig, LlmError,
    LlmSummarizer, MessagesClient, Provider, ResponseMode, SCHEMA_NAME, SUMMARY_LLM_SCHEMA_JSON,
    SummaryRun,
};
use earshot_llm_replay::mock::{MockResponse, MockServer, RecordedRequest};
use earshot_net::{Allowlist, AuditLog, AuditOutcome, NetClient, NetConfig, NetError};
use serde_json::{Value, json};

const T0: u64 = 1_790_000_000_000;
const KEY: &str = "sk-ant-test-SECRET77";

fn seg_id(i: usize) -> EntityId {
    EntityId::from_bytes(((i as u128) | (0x0189_0a5d_ac96_7000u128 << 64)).to_be_bytes())
}

fn meeting(n: usize) -> SummaryInput {
    SummaryInput {
        meeting: MeetingInfo {
            title: Some("Beta planning".into()),
            started: Some("2026-10-17 14:00".into()),
            duration_ms: n as u64 * 10_000,
            attendees: vec!["Ana".into(), "Ben".into()],
        },
        segments: (0..n)
            .map(|i| PromptSegment {
                id: seg_id(i),
                start: MeetingTime::from_millis(i as u64 * 10_000),
                speaker: if i % 2 == 0 { "Ana" } else { "Ben" }.into(),
                text: format!("Point {i}: we ship the beta on Friday and Ana writes the notes."),
            })
            .collect(),
        notes: vec![],
        template: Template::General,
        prompt_variant: Default::default(),
        style: Default::default(),
        style_preset: Default::default(),
        vocabulary: Default::default(),
        user_name: None,
    }
}

/// A reply citing segment numbers (`[n]` in the prompt: segment `n - 1` of the meeting,
/// T6.20); a number past the meeting is an unknown citation.
fn summary_value(ids: &[u64]) -> Value {
    json!({
        "title": "Beta planning",
        "tldr": "The beta ships on Friday.",
        "enhanced_notes": [{"heading": "Beta", "bullets": [{"text": "Ship Friday", "source_segment_ids": ids}]}],
        "decisions": [{"text": "Ship the beta on Friday", "source_segment_ids": ids}],
        "action_items": [{"owner": "Ana", "task": "Write release notes", "due": "Thursday", "source_segment_ids": ids}],
        "open_questions": [],
        "topics": ["beta"]
    })
}

/// A valid reply for `input`, citing its first segment (number 1).
fn valid_text(input: &SummaryInput) -> MockResponse {
    assert!(!input.segments.is_empty());
    MockResponse::anthropic_text(&summary_value(&[1]).to_string(), "end_turn")
}

fn schema_without_draft_marker() -> Value {
    let mut schema: Value = serde_json::from_str(SUMMARY_LLM_SCHEMA_JSON).unwrap();
    schema.as_object_mut().unwrap().remove("$schema");
    schema
}

struct Setup {
    server: MockServer,
    summarizer: LlmSummarizer,
    audit: AuditLog,
}

fn setup_with(script: Vec<MockResponse>, edit: impl FnOnce(&mut LlmConfig)) -> Setup {
    let server = MockServer::start(script).unwrap();
    let mut config = LlmConfig::anthropic(server.base_url().as_str(), "claude-test").unwrap();
    config.api_key = ApiKey::new(KEY);
    edit(&mut config);
    let mut allow = Allowlist::new();
    allow.allow_url(&config.endpoint).unwrap();
    let audit = AuditLog::new();
    let net =
        NetClient::new(NetConfig::new(allow, Arc::new(ManualClock::new(T0))).audit(audit.clone()))
            .unwrap();
    Setup {
        summarizer: LlmSummarizer::new(LlmClient::new(net, config)),
        server,
        audit,
    }
}

fn setup(script: Vec<MockResponse>) -> Setup {
    setup_with(script, |_| {})
}

async fn run(s: &Setup, input: &SummaryInput) -> SummaryRun {
    s.summarizer
        .summarize_with_report(input, &CancelToken::new())
        .await
}

/// `(role, content)` of each turn in `messages`.
fn turns(r: &RecordedRequest) -> Vec<(String, String)> {
    r.json()["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| {
            (
                m["role"].as_str().unwrap().to_string(),
                m["content"].as_str().unwrap().to_string(),
            )
        })
        .collect()
}

/// The mode a recorded Messages request asked for.
fn mode_of(r: &RecordedRequest) -> &'static str {
    let b = r.json();
    match (b.get("output_config"), b.get("tool_choice")) {
        (Some(_), None) => "json_schema",
        (None, Some(_)) => "tool",
        (None, None) => "text",
        (Some(_), Some(_)) => "both",
    }
}

fn assert_no_key(texts: &[String]) {
    for t in texts {
        assert!(!t.contains("SECRET77"), "{t}");
    }
}

#[tokio::test]
async fn anthropic_valid_reply_is_one_structured_output_request_to_the_messages_api() {
    let input = meeting(3);
    let s = setup(vec![valid_text(&input)]);
    let out = run(&s, &input).await;
    let summary = out.result.unwrap();
    assert!(validate_summary(&summary, &input.segment_ids()).is_empty());
    assert_eq!((out.report.requests, out.report.repairs), (1, 0));
    assert_eq!(out.report.mode, ResponseMode::JsonSchema);
    assert_eq!(s.summarizer.client().provider(), Some(Provider::Anthropic));

    let reqs = s.server.requests();
    assert_eq!(reqs.len(), 1);
    let r = &reqs[0];
    assert_eq!(
        (r.method.as_str(), r.path.as_str()),
        ("POST", "/v1/messages")
    );
    assert_eq!(r.header("content-type"), Some("application/json"));
    assert_eq!(r.header("anthropic-version"), Some(ANTHROPIC_VERSION));
    assert_eq!(r.header("x-api-key"), Some(KEY));
    assert_eq!(r.header("authorization"), None, "no bearer header");
    let body = r.json();
    assert_eq!(body["model"], "claude-test");
    let PromptPlan::Single(p) = plan(&input, s.summarizer.client().budget()).unwrap() else {
        panic!("small meeting is one request");
    };
    // The request's own content-sized reply budget, at most the configured ceiling.
    assert_eq!(body["max_tokens"], p.max_output_tokens);
    assert!(p.max_output_tokens <= ANTHROPIC_DEFAULT_BUDGET.max_output_tokens);
    assert_eq!(
        body["output_config"],
        json!({"format": {"type": "json_schema", "schema": schema_without_draft_marker()}})
    );
    // Text only, no sampling parameters (current Claude models reject them), no tools.
    let keys: Vec<&str> = body
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    let mut keys = keys;
    keys.sort_unstable();
    assert_eq!(
        keys,
        ["max_tokens", "messages", "model", "output_config", "system"]
    );

    let system: Vec<&str> = p
        .messages
        .iter()
        .filter(|m| m.role == earshot_core::prompt::Role::System)
        .map(|m| m.content.as_str())
        .collect();
    assert_eq!(body["system"], system.join("\n\n"));
    let user: Vec<(String, String)> = p
        .messages
        .iter()
        .filter(|m| m.role != earshot_core::prompt::Role::System)
        .map(|m| ("user".to_string(), m.content.clone()))
        .collect();
    assert_eq!(turns(r), user);

    let log = s.audit.entries();
    assert_eq!(log.len(), 1);
    assert_eq!(
        (log[0].host.as_str(), log[0].port, log[0].path.as_str()),
        (
            "127.0.0.1",
            s.server.base_url().port().unwrap(),
            "/v1/messages"
        )
    );
    assert_eq!(log[0].outcome, AuditOutcome::Completed);
    assert_no_key(&[format!("{log:?}"), format!("{:?}", s.summarizer)]);
}

#[tokio::test]
async fn anthropic_key_is_never_in_errors_and_an_auth_error_does_not_fall_back() {
    let input = meeting(2);
    let s = setup(vec![MockResponse::anthropic_error(
        401,
        "authentication_error",
        &format!("invalid x-api-key: {KEY}"),
    )]);
    let out = run(&s, &input).await;
    let err = out.result.unwrap_err();
    assert!(matches!(err, LlmError::Http { status: 401, .. }), "{err:?}");
    assert_eq!(out.report.requests, 1);
    assert_no_key(&[
        format!("{err}"),
        format!("{err:?}"),
        format!("{:?}", s.summarizer),
    ]);
    assert!(format!("{err}").contains("<redacted>"), "{err}");
}

#[tokio::test]
async fn anthropic_without_a_key_sends_no_key_header() {
    let input = meeting(2);
    let s = setup_with(vec![valid_text(&input)], |c| c.api_key = None);
    run(&s, &input).await.result.unwrap();
    assert_eq!(s.server.requests()[0].header("x-api-key"), None);
}

#[tokio::test]
async fn anthropic_rejected_structured_output_falls_back_to_a_forced_tool_then_text() {
    let input = meeting(3);
    let fenced = format!(
        "Here is the summary:\n```json\n{}\n```",
        summary_value(&[10])
    );
    let s = setup(vec![
        MockResponse::anthropic_error(
            400,
            "invalid_request_error",
            "output_config.format: structured outputs are not supported for this model",
        ),
        MockResponse::anthropic_error(
            400,
            "invalid_request_error",
            "tool_choice: type \"tool\" and \"any\" are not supported for this model.",
        ),
        MockResponse::anthropic_text(&fenced, "end_turn"),
        // The repair round stays in text mode.
        MockResponse::anthropic_text(&summary_value(&[1]).to_string(), "end_turn"),
    ]);
    let out = run(&s, &input).await;
    out.result.unwrap();
    assert_eq!((out.report.requests, out.report.format_fallbacks), (4, 2));
    assert_eq!(out.report.repairs, 1, "unknown citation repaired");
    assert_eq!(out.report.mode, ResponseMode::Text);
    let reqs = s.server.requests();
    let modes: Vec<&str> = reqs.iter().map(mode_of).collect();
    assert_eq!(modes, ["json_schema", "tool", "text", "text"]);
    let tool = reqs[1].json();
    assert_eq!(
        tool["tools"],
        json!([{
            "name": SCHEMA_NAME,
            "description": tool["tools"][0]["description"],
            "input_schema": schema_without_draft_marker(),
        }])
    );
    assert_eq!(
        tool["tool_choice"],
        json!({"type": "tool", "name": SCHEMA_NAME})
    );
    assert!(reqs[2].json().get("tools").is_none());
}

#[tokio::test]
async fn anthropic_tool_use_input_is_read_as_the_summary() {
    let input = meeting(3);
    let s = setup(vec![
        MockResponse::anthropic_error(
            400,
            "invalid_request_error",
            "output_config: Extra inputs are not permitted",
        ),
        MockResponse::anthropic_tool(SCHEMA_NAME, summary_value(&[3]), "tool_use"),
    ]);
    let out = run(&s, &input).await;
    let summary: SummaryV1 = out.result.unwrap();
    assert_eq!(summary.decisions[0].source_segment_ids, [seg_id(2)]);
    assert_eq!(out.report.mode, ResponseMode::Tool);
}

#[tokio::test]
async fn anthropic_error_statuses_other_than_a_bad_request_never_fall_back_or_retry() {
    let input = meeting(2);
    for (status, kind) in [
        (401, "authentication_error"),
        (403, "permission_error"),
        (404, "not_found_error"),
        (413, "request_too_large"),
        (429, "rate_limit_error"),
        (500, "api_error"),
        (529, "overloaded_error"),
    ] {
        let s = setup(vec![MockResponse::anthropic_error(status, kind, "no")]);
        let out = run(&s, &input).await;
        assert!(
            matches!(&out.result, Err(LlmError::Http { status: st, .. }) if *st == status),
            "{status}: {:?}",
            out.result
        );
        assert_eq!(out.report.requests, 1, "{status}: no retry, no fallback");
        assert_eq!(out.report.mode, ResponseMode::JsonSchema);
    }
    // A 400 without the documented error type (for example from a proxy) is not a format
    // rejection either.
    let s = setup(vec![MockResponse::status(
        400,
        r#"{"error":"bad gateway config"}"#,
    )]);
    let out = run(&s, &input).await;
    assert!(
        matches!(out.result, Err(LlmError::Http { status: 400, .. })),
        "{:?}",
        out.result
    );
    assert_eq!(out.report.requests, 1);
}

#[tokio::test]
async fn anthropic_a_later_bad_request_not_naming_the_format_is_an_http_error() {
    let input = meeting(3);
    let mut bad = summary_value(&[1]);
    bad.as_object_mut().unwrap().remove("tldr");
    let s = setup(vec![
        MockResponse::anthropic_text(&bad.to_string(), "end_turn"),
        MockResponse::anthropic_error(
            400,
            "invalid_request_error",
            "prompt is too long: 210000 tokens > 200000 maximum",
        ),
    ]);
    let out = run(&s, &input).await;
    assert!(
        matches!(out.result, Err(LlmError::Http { status: 400, .. })),
        "{:?}",
        out.result
    );
    assert_eq!((out.report.requests, out.report.format_fallbacks), (2, 0));
    assert_eq!(out.report.mode, ResponseMode::JsonSchema);

    // One naming the format still falls back after a success.
    let s = setup(vec![
        MockResponse::anthropic_text(&bad.to_string(), "end_turn"),
        MockResponse::anthropic_error(
            400,
            "invalid_request_error",
            "output_config.format.schema: too many properties",
        ),
        MockResponse::anthropic_tool(SCHEMA_NAME, summary_value(&[1]), "tool_use"),
    ]);
    let out = run(&s, &input).await;
    out.result.unwrap();
    assert_eq!(out.report.mode, ResponseMode::Tool);
}

#[tokio::test]
async fn anthropic_a_first_bad_request_about_the_context_length_never_falls_back() {
    let input = meeting(3);
    for message in [
        "prompt is too long: 210000 tokens > 200000 maximum",
        "input length and `max_tokens` exceed context limit: 197000 + 8192 > 200000, \
         decrease input length or `max_tokens` and try again",
    ] {
        let s = setup(vec![MockResponse::anthropic_error(
            400,
            "invalid_request_error",
            message,
        )]);
        let out = run(&s, &input).await;
        assert!(
            matches!(out.result, Err(LlmError::Http { status: 400, .. })),
            "{message}: {:?}",
            out.result
        );
        assert_eq!(
            (out.report.requests, out.report.format_fallbacks),
            (1, 0),
            "{message}: the transcript is sent once"
        );
        assert_eq!(out.report.mode, ResponseMode::JsonSchema);
    }
    // Any other first bad request still falls back.
    let s = setup(vec![
        MockResponse::anthropic_error(400, "invalid_request_error", "this model is not supported"),
        MockResponse::anthropic_tool(SCHEMA_NAME, summary_value(&[1]), "tool_use"),
    ]);
    let out = run(&s, &input).await;
    out.result.unwrap();
    assert_eq!(out.report.mode, ResponseMode::Tool);
}

#[tokio::test]
async fn anthropic_a_later_bad_request_about_a_toolbox_does_not_reopen_the_fallback() {
    let input = meeting(3);
    let mut bad = summary_value(&[1]);
    bad.as_object_mut().unwrap().remove("tldr");
    let s = setup(vec![
        MockResponse::anthropic_text(&bad.to_string(), "end_turn"),
        MockResponse::anthropic_error(
            400,
            "invalid_request_error",
            "the toolbox integration is not enabled for this workspace",
        ),
    ]);
    let out = run(&s, &input).await;
    assert!(
        matches!(out.result, Err(LlmError::Http { status: 400, .. })),
        "{:?}",
        out.result
    );
    assert_eq!((out.report.requests, out.report.format_fallbacks), (2, 0));
    assert_eq!(out.report.mode, ResponseMode::JsonSchema);

    // The tool request fields still name the format after a success.
    for message in [
        "tools: not supported",
        "tool_choice: not supported",
        "tool_use blocks are not supported",
        "input_schema: unsupported keyword",
    ] {
        let s = setup(vec![
            MockResponse::anthropic_text(&bad.to_string(), "end_turn"),
            MockResponse::anthropic_error(400, "invalid_request_error", message),
            MockResponse::anthropic_tool(SCHEMA_NAME, summary_value(&[1]), "tool_use"),
        ]);
        let out = run(&s, &input).await;
        assert!(out.result.is_ok(), "{message}: {:?}", out.result);
        assert_eq!(out.report.mode, ResponseMode::Tool, "{message}");
    }
}

#[tokio::test]
async fn anthropic_refusal_stops_at_once_without_a_repair_round() {
    let input = meeting(3);
    let refusal = |details: Value| {
        MockResponse::ok(
            json!({
                "id": "msg_mock",
                "type": "message",
                "role": "assistant",
                "model": "mock",
                "content": [{"type": "text", "text": "I can't help with that."}],
                "stop_reason": "refusal",
                "stop_sequence": null,
                "stop_details": details,
                "usage": {"input_tokens": 1, "output_tokens": 1},
            })
            .to_string(),
        )
    };
    let s = setup(vec![
        refusal(json!({"type": "refusal", "category": "cyber", "explanation": "declined"})),
        valid_text(&input),
    ]);
    let out = run(&s, &input).await;
    assert!(
        matches!(&out.result, Err(LlmError::Refused { category: Some(c) }) if c == "cyber"),
        "{:?}",
        out.result
    );
    assert_eq!((out.report.requests, out.report.repairs), (1, 0));
    assert_eq!(
        (s.server.requests().len(), s.server.remaining()),
        (1, 1),
        "the transcript was sent once"
    );
    let err = out.result.unwrap_err();
    assert!(format!("{err}").contains("declined"), "{err}");

    // Also a refusal on a later step (after a repair was already needed) stops there.
    let mut bad = summary_value(&[1]);
    bad.as_object_mut().unwrap().remove("tldr");
    let s = setup(vec![
        MockResponse::anthropic_text(&bad.to_string(), "end_turn"),
        refusal(Value::Null),
        valid_text(&input),
    ]);
    let out = run(&s, &input).await;
    assert!(
        matches!(out.result, Err(LlmError::Refused { category: None })),
        "{:?}",
        out.result
    );
    assert_eq!(out.report.requests, 2);
    assert_eq!(s.server.remaining(), 1);
}

#[tokio::test]
async fn anthropic_repair_round_alternates_user_and_assistant_turns() {
    let input = meeting(3);
    let mut bad = summary_value(&[8]);
    bad.as_object_mut().unwrap().remove("tldr");
    let bad = bad.to_string();
    let s = setup(vec![
        MockResponse::anthropic_text(&bad, "end_turn"),
        valid_text(&input),
    ]);
    let out = run(&s, &input).await;
    out.result.unwrap();
    assert_eq!((out.report.requests, out.report.repairs), (2, 1));
    let reqs = s.server.requests();
    let first = turns(&reqs[0]);
    let second = turns(&reqs[1]);
    assert_eq!(second.len(), first.len() + 2);
    assert_eq!(second[..first.len()], first[..]);
    assert_eq!(second[first.len()], ("assistant".to_string(), bad));
    let repair = &second[first.len() + 1];
    assert_eq!(repair.0, "user");
    assert!(
        repair.1.contains("- /tldr: missing required field"),
        "{}",
        repair.1
    );
    assert_eq!(reqs[1].json()["system"], reqs[0].json()["system"]);
    assert_eq!(mode_of(&reqs[1]), "json_schema");
}

#[tokio::test]
async fn anthropic_max_tokens_reply_is_treated_as_truncated() {
    let input = meeting(3);
    let full = summary_value(&[1]).to_string();
    // More room first, as the OpenAI-compatible client does.
    let s = setup(vec![
        MockResponse::anthropic_text(&full[..full.len() / 2], "max_tokens"),
        valid_text(&input),
    ]);
    run(&s, &input).await.result.unwrap();
    let reqs = s.server.requests();
    assert_eq!(
        reqs[1].json()["max_tokens"].as_u64().unwrap(),
        2 * reqs[0].json()["max_tokens"].as_u64().unwrap(),
    );
    assert_eq!(turns(&reqs[1]), turns(&reqs[0]), "no echo, no repair");
    // At the ceiling the repair asks for less.
    let s = setup_with(
        vec![
            MockResponse::anthropic_text(&full[..full.len() / 2], "max_tokens"),
            valid_text(&input),
        ],
        |c| c.budget.max_output_tokens = earshot_core::prompt::REPLY_MIN_TOKENS,
    );
    run(&s, &input).await.result.unwrap();
    let second = turns(&s.server.requests()[1]);
    let repair = &second.last().unwrap().1;
    assert!(repair.contains("cut off"), "{repair}");
    assert!(!repair.contains("missing required field"), "{repair}");
}

#[tokio::test]
async fn anthropic_cancellation_abandons_the_request_promptly() {
    let input = meeting(2);
    let s = setup(vec![valid_text(&input).delayed(Duration::from_secs(5))]);
    let cancel = CancelToken::new();
    let c = cancel.clone();
    thread::spawn(move || {
        thread::sleep(Duration::from_millis(150));
        c.cancel();
    });
    let t = Instant::now();
    let out = s.summarizer.summarize_with_report(&input, &cancel).await;
    assert!(
        matches!(out.result, Err(LlmError::Cancelled)),
        "{:?}",
        out.result
    );
    assert!(t.elapsed() < Duration::from_secs(2), "{:?}", t.elapsed());
}

#[tokio::test]
async fn anthropic_endpoint_outside_the_allowlist_is_refused_before_sending() {
    let input = meeting(2);
    let server = MockServer::start(vec![valid_text(&input)]).unwrap();
    let audit = AuditLog::new();
    let net = NetClient::new(
        NetConfig::new(Allowlist::new(), Arc::new(ManualClock::new(T0))).audit(audit.clone()),
    )
    .unwrap();
    let config = LlmConfig::anthropic(server.base_url().as_str(), "m").unwrap();
    let summarizer = LlmSummarizer::new(MessagesClient::new(net, config));
    let err = summarizer
        .summarize(&input, &CancelToken::new())
        .await
        .unwrap_err();
    assert!(
        matches!(err, LlmError::Net(NetError::NotAllowed { .. })),
        "{err:?}"
    );
    assert!(server.requests().is_empty());
    assert!(matches!(
        audit.entries()[0].outcome,
        AuditOutcome::Blocked(_)
    ));
}

/// Optional live check against the real Anthropic API with the user's own key. Never
/// gates a task; never run by agents.
#[tokio::test]
#[ignore = "live Anthropic API: set ANTHROPIC_API_KEY (and optionally EARSHOT_ANTHROPIC_MODEL), run `cargo test -p earshot-llm --test anthropic anthropic_live -- --ignored`"]
async fn anthropic_live_summarises_a_meeting() {
    let key = std::env::var(earshot_llm::ANTHROPIC_API_KEY_ENV)
        .expect("ANTHROPIC_API_KEY is not set; this test only runs with the user's own key");
    let model = std::env::var("EARSHOT_ANTHROPIC_MODEL")
        .unwrap_or_else(|_| earshot_llm::ANTHROPIC_DEFAULT_MODEL.into());
    let mut config = LlmConfig::anthropic(earshot_llm::ANTHROPIC_DEFAULT_URL, model).unwrap();
    config.api_key = ApiKey::new(key);
    let mut allow = Allowlist::new();
    allow.allow_url(&config.endpoint).unwrap();
    let net = NetClient::new(NetConfig::new(allow, Arc::new(earshot_net::SystemClock))).unwrap();
    let input = meeting(4);
    let summary = LlmSummarizer::new(LlmClient::new(net, config))
        .summarize(&input, &CancelToken::new())
        .await
        .unwrap();
    assert!(validate_summary(&summary, &input.segment_ids()).is_empty());
}
