//! `LlmSummarizer` and `ChatClient` against the loopback mock endpoint (T4.3).

use std::collections::BTreeSet;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use earshot_core::asr::{CancelToken, MeetingTime};
use earshot_core::identity::{EntityId, ManualClock};
use earshot_core::prompt::{
    ContextBudget, MeetingInfo, PromptPlan, PromptSegment, REPAIR_MAX_TOKENS, Role, SummaryInput,
    Template, plan, plan_chunks, reduce_groups, time_chunks,
};
use earshot_core::summary::{Summarizer, SummaryV1, validate_summary};
use earshot_llm::{
    ApiKey, ChatClient, LlmConfig, LlmError, LlmSummarizer, ResponseMode, SUMMARY_LLM_SCHEMA_JSON,
    SummaryRun, TimedRun,
};
use earshot_llm_replay::mock::{MockResponse, MockServer, RecordedRequest};
use earshot_net::{Allowlist, AuditLog, AuditOutcome, NetClient, NetConfig, NetError};
use serde_json::{Value, json};

const T0: u64 = 1_790_000_000_000;

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
        vocabulary: Default::default(),
        user_name: None,
    }
}

/// A reply citing segment `numbers` (1-based, as the prompt numbers the transcript).
fn summary_json(numbers: &[usize]) -> String {
    let ids = numbers.to_vec();
    json!({
        "title": "Beta planning",
        "tldr": "The beta ships on Friday.",
        "enhanced_notes": [{"heading": "Beta", "bullets": [{"text": "Ship Friday", "source_segment_ids": ids}]}],
        "decisions": [{"text": "Ship the beta on Friday", "source_segment_ids": ids}],
        "action_items": [{"owner": "Ana", "task": "Write release notes", "due": "Thursday", "source_segment_ids": ids}],
        "open_questions": [],
        "topics": ["beta"]
    })
    .to_string()
}

fn valid_reply(input: &SummaryInput) -> MockResponse {
    let _ = input;
    MockResponse::completion(&summary_json(&[1]), "stop")
}

struct Setup {
    server: MockServer,
    summarizer: LlmSummarizer,
    audit: AuditLog,
}

fn setup_with(script: Vec<MockResponse>, edit: impl FnOnce(&mut LlmConfig)) -> Setup {
    let server = MockServer::start(script).unwrap();
    let mut allow = Allowlist::new();
    allow.allow_url(&server.endpoint()).unwrap();
    let audit = AuditLog::new();
    let net =
        NetClient::new(NetConfig::new(allow, Arc::new(ManualClock::new(T0))).audit(audit.clone()))
            .unwrap();
    let mut config = LlmConfig::new(server.endpoint().as_str(), "test-model").unwrap();
    edit(&mut config);
    Setup {
        summarizer: LlmSummarizer::new(ChatClient::new(net, config)),
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

fn messages(r: &RecordedRequest) -> Vec<(String, String)> {
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

#[tokio::test]
async fn valid_reply_is_accepted_after_one_strict_json_schema_request() {
    let input = meeting(3);
    let s = setup(vec![valid_reply(&input)]);
    let out = run(&s, &input).await;
    let summary = out.result.unwrap();
    assert!(validate_summary(&summary, &input.segment_ids()).is_empty());
    assert_eq!(out.report.requests, 1);
    assert_eq!(out.report.repairs, 0);
    assert_eq!(out.report.mode, ResponseMode::JsonSchema);

    let reqs = s.server.requests();
    assert_eq!(reqs.len(), 1);
    let r = &reqs[0];
    assert_eq!(r.method, "POST");
    assert_eq!(r.path, "/v1/chat/completions");
    assert_eq!(r.header("content-type"), Some("application/json"));
    assert_eq!(r.header("authorization"), None, "no key configured");
    let body = r.json();
    assert_eq!(body["model"], "test-model");
    assert_eq!(body["stream"], false);
    let schema: Value = serde_json::from_str(SUMMARY_LLM_SCHEMA_JSON).unwrap();
    assert_eq!(
        body["response_format"],
        json!({"type": "json_schema", "json_schema": {"name": "earshot_summary_v1", "strict": true, "schema": schema}})
    );
    let PromptPlan::Single(p) = plan(&input, s.summarizer.client().budget()).unwrap() else {
        panic!("small meeting is one request");
    };
    // The request's own, content-sized reply budget (a short meeting gets the minimum).
    assert_eq!(body["max_tokens"], p.max_output_tokens);
    assert_eq!(p.max_output_tokens, earshot_core::prompt::REPLY_MIN_TOKENS);
    let expected: Vec<(String, String)> = p
        .messages
        .iter()
        .map(|m| {
            (
                serde_json::to_value(m.role)
                    .unwrap()
                    .as_str()
                    .unwrap()
                    .to_string(),
                m.content.clone(),
            )
        })
        .collect();
    assert_eq!(messages(r), expected);

    let log = s.audit.entries();
    assert_eq!(log.len(), 1);
    assert_eq!(
        (log[0].host.as_str(), log[0].path.as_str()),
        ("127.0.0.1", "/v1/chat/completions")
    );
    assert_eq!(log[0].outcome, AuditOutcome::Completed);
}

#[tokio::test]
async fn api_key_is_sent_as_bearer_and_never_appears_in_errors() {
    let input = meeting(2);
    let s = setup_with(
        vec![MockResponse::status(
            401,
            r#"{"error":{"message":"Incorrect API key provided: sk-test-SECRET42."}}"#,
        )],
        |c| c.api_key = ApiKey::new("sk-test-SECRET42"),
    );
    let out = run(&s, &input).await;
    let err = out.result.unwrap_err();
    assert!(matches!(err, LlmError::Http { status: 401, .. }), "{err:?}");
    for text in [
        format!("{err}"),
        format!("{err:?}"),
        format!("{:?}", s.summarizer),
    ] {
        assert!(!text.contains("SECRET42"), "{text}");
    }
    assert!(format!("{err}").contains("<redacted>"));
    assert_eq!(
        s.server.requests()[0].header("authorization"),
        Some("Bearer sk-test-SECRET42")
    );
}

#[tokio::test]
async fn rejected_formats_fall_back_to_json_object_then_text_and_stay_there() {
    let input = meeting(3);
    let fenced = format!(
        "Here is the summary:\n```json\n{}\n```",
        summary_json(&[10])
    );
    let s = setup(vec![
        MockResponse::status(
            400,
            r#"{"error":{"message":"response_format json_schema is not supported"}}"#,
        ),
        MockResponse::status(422, r#"{"detail":"json_object unsupported"}"#),
        MockResponse::completion(&fenced, "stop"),
        MockResponse::completion(&summary_json(&[2]), "stop"),
    ]);
    let out = run(&s, &input).await;
    out.result.unwrap();
    assert_eq!(out.report.requests, 4);
    assert_eq!(out.report.format_fallbacks, 2);
    assert_eq!(out.report.repairs, 1, "unknown citation repaired");
    assert_eq!(out.report.mode, ResponseMode::Text);
    let formats: Vec<Value> = s
        .server
        .requests()
        .iter()
        .map(|r| r.json()["response_format"]["type"].clone())
        .collect();
    assert_eq!(
        formats,
        [
            json!("json_schema"),
            json!("json_object"),
            Value::Null,
            Value::Null
        ]
    );
}

#[tokio::test]
async fn validator_errors_are_fed_back_in_a_repair_round() {
    let input = meeting(3);
    let mut bad: Value = serde_json::from_str(&summary_json(&[8])).unwrap();
    bad.as_object_mut().unwrap().remove("tldr");
    let bad = bad.to_string();
    let s = setup(vec![
        MockResponse::completion(&bad, "stop"),
        valid_reply(&input),
    ]);
    let out = run(&s, &input).await;
    out.result.unwrap();
    assert_eq!((out.report.requests, out.report.repairs), (2, 1));
    let reqs = s.server.requests();
    let first = messages(&reqs[0]);
    let second = messages(&reqs[1]);
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
    assert!(
        repair
            .1
            .contains("segment 8 is not one of the transcript lines you were given"),
        "{}",
        repair.1
    );
    assert_eq!(reqs[1].json()["response_format"]["type"], "json_schema");
}

#[tokio::test]
async fn gives_up_after_two_repair_rounds() {
    let input = meeting(3);
    let s = setup(vec![
        MockResponse::completion("I cannot do that.", "stop"),
        MockResponse::completion("{\"title\": \"x\"", "stop"),
        MockResponse::completion(&summary_json(&[]), "stop"),
    ]);
    let out = run(&s, &input).await;
    match out.result {
        Err(LlmError::Invalid { attempts, errors }) => {
            assert_eq!(attempts, 3);
            assert!(
                errors
                    .iter()
                    .any(|e| e.path == "/decisions/0/source_segment_ids")
            );
        }
        other => panic!("expected Invalid, got {other:?}"),
    }
    assert_eq!((out.report.requests, out.report.repairs), (3, 2));
    assert_eq!((s.server.remaining(), s.server.unscripted()), (0, 0));
}

#[tokio::test]
async fn truncated_reply_is_asked_again_with_more_room() {
    let input = meeting(3);
    let full = summary_json(&[1]);
    let s = setup(vec![
        MockResponse::completion(&full[..full.len() / 2], "length"),
        valid_reply(&input),
    ]);
    let done = run(&s, &input).await;
    done.result.unwrap();
    assert_eq!(done.report.repairs, 1);
    let reqs = s.server.requests();
    assert_eq!(
        reqs[1].json()["max_tokens"].as_u64().unwrap(),
        (2 * reqs[0].json()["max_tokens"].as_u64().unwrap()).min(2_048),
    );
    assert_eq!(messages(&reqs[1]), messages(&reqs[0]), "no echo, no repair");
}

#[tokio::test]
async fn truncated_reply_asks_for_a_shorter_answer_when_the_budget_cannot_grow() {
    let input = meeting(3);
    let full = summary_json(&[1]);
    let s = setup_with(
        vec![
            MockResponse::completion(&full[..full.len() / 2], "length"),
            valid_reply(&input),
        ],
        |c| c.budget.max_output_tokens = earshot_core::prompt::REPLY_MIN_TOKENS,
    );
    run(&s, &input).await.result.unwrap();
    let second = messages(&s.server.requests()[1]);
    let repair = &second.last().unwrap().1;
    assert!(repair.contains("cut off"), "{repair}");
    // No inner object (a note section) is extracted from a truncated reply.
    assert!(!repair.contains("unknown field"), "{repair}");
    assert!(!repair.contains("missing required field"), "{repair}");
}

#[tokio::test]
async fn truncated_fixture_reply_reports_truncation_not_an_inner_object() {
    // The committed truncated case cuts the reply after complete note sections, which a
    // brace scan would pick up as the answer.
    let case = earshot_llm_replay::replay::load_cases(&earshot_llm_replay::replay::fixtures_dir())
        .unwrap()
        .into_iter()
        .find(|c| c.name == "truncated")
        .unwrap();
    let s = setup_with(case.script.clone(), |c| {
        c.budget.max_output_tokens = earshot_core::prompt::REPLY_MIN_TOKENS;
    });
    run(&s, &case.input).await.result.unwrap();
    let repair = messages(&s.server.requests()[1]).last().unwrap().1.clone();
    assert!(repair.contains("cut off"), "{repair}");
    assert!(!repair.contains("/heading"), "{repair}");
    assert!(!repair.contains("unknown field"), "{repair}");
}

fn missing_tldr() -> String {
    let mut bad: Value = serde_json::from_str(&summary_json(&[1])).unwrap();
    bad.as_object_mut().unwrap().remove("tldr");
    bad.to_string()
}

#[tokio::test]
async fn a_later_400_that_does_not_name_the_format_is_an_http_error() {
    let input = meeting(3);
    let s = setup(vec![
        MockResponse::completion(&missing_tldr(), "stop"),
        MockResponse::status(
            400,
            r#"{"error":{"message":"This model's maximum context length is 8192 tokens"}}"#,
        ),
    ]);
    let out = run(&s, &input).await;
    assert!(
        matches!(&out.result, Err(LlmError::Http { status: 400, message }) if message.contains("context length")),
        "{:?}",
        out.result
    );
    assert_eq!(out.report.requests, 2);
    assert_eq!(out.report.format_fallbacks, 0);
    assert_eq!(
        out.report.mode,
        ResponseMode::JsonSchema,
        "mode not degraded"
    );
}

#[tokio::test]
async fn a_first_400_about_the_context_length_never_falls_back() {
    let input = meeting(3);
    for body in [
        // OpenAI: `code` names it.
        r#"{"error":{"message":"This model's maximum context length is 8192 tokens. However, your messages resulted in 9000 tokens. Please reduce the length of the messages.","type":"invalid_request_error","param":"messages","code":"context_length_exceeded"}}"#,
        // Older vLLM (top-level error object; not checked against current source): the
        // same message, no code.
        r#"{"object":"error","message":"This model's maximum context length is 4096 tokens. However, you requested 5000 tokens.","type":"BadRequestError","code":400}"#,
        // vLLM e0d00e4 (client.rs CONTEXT_LENGTH_PHRASES has the source lines): the prompt
        // alone is too long, in tokens and in characters.
        r#"{"error":{"message":"This model's maximum context length is 4096 tokens. However, your prompt contains 5000 input tokens, which exceeds this limit even without any output tokens. Please reduce the length of the input prompt.","type":"BadRequestError","param":"input_tokens","code":400}}"#,
        r#"{"error":{"message":"This model's maximum context length is 4096 tokens. However, your prompt contains 90000 characters (more than 81920 characters, which is the upper bound for 4096 input tokens). Please reduce the length of the input prompt.","type":"BadRequestError","param":"input_text","code":400}}"#,
        // vLLM: the input length check of get_max_tokens.
        r#"{"error":{"message":"Input length (5000) exceeds model's maximum context length (4096).","type":"BadRequestError","param":null,"code":400}}"#,
        // vLLM: a reply budget larger than the whole context.
        r#"{"error":{"message":"max_tokens=8192 cannot be greater than max_model_len=4096. Please request fewer output tokens.","type":"BadRequestError","param":"max_tokens","code":400}}"#,
        // llama.cpp server 060b2fe: its own error type, both messages.
        r#"{"error":{"code":400,"message":"request (9000 tokens) exceeds the available context size (8192 tokens), try increasing it","type":"exceed_context_size_error","n_prompt_tokens":9000,"n_ctx":8192}}"#,
        r#"{"error":{"code":400,"message":"input (9000 tokens) is larger than the max context size (8192 tokens). skipping","type":"exceed_context_size_error","n_prompt_tokens":9000,"n_ctx":8192}}"#,
    ] {
        let s = setup(vec![MockResponse::status(400, body)]);
        let out = run(&s, &input).await;
        assert!(
            matches!(out.result, Err(LlmError::Http { status: 400, .. })),
            "{body}: {:?}",
            out.result
        );
        assert_eq!(
            (out.report.requests, out.report.format_fallbacks),
            (1, 0),
            "{body}: the transcript is sent once"
        );
        assert_eq!(out.report.mode, ResponseMode::JsonSchema);
    }
    // Any other first 400 still falls back.
    let s = setup(vec![
        MockResponse::status(
            400,
            r#"{"error":{"message":"this model is not supported"}}"#,
        ),
        valid_reply(&input),
    ]);
    let out = run(&s, &input).await;
    out.result.unwrap();
    assert_eq!(out.report.mode, ResponseMode::JsonObject);
}

#[tokio::test]
async fn a_later_rejection_naming_the_format_still_falls_back() {
    let input = meeting(3);
    let s = setup(vec![
        MockResponse::completion(&missing_tldr(), "stop"),
        MockResponse::status(
            400,
            r#"{"error":{"message":"json_schema with nested arrays is not supported"}}"#,
        ),
        valid_reply(&input),
    ]);
    let out = run(&s, &input).await;
    out.result.unwrap();
    assert_eq!(out.report.mode, ResponseMode::JsonObject);
    let formats: Vec<Value> = s
        .server
        .requests()
        .iter()
        .map(|r| r.json()["response_format"]["type"].clone())
        .collect();
    assert_eq!(
        formats,
        [
            json!("json_schema"),
            json!("json_schema"),
            json!("json_object")
        ]
    );
}

#[tokio::test]
async fn repair_rounds_resend_only_the_last_reply() {
    let input = meeting(3);
    let s = setup(vec![
        MockResponse::completion("first bad reply", "stop"),
        MockResponse::completion("second bad reply", "stop"),
        valid_reply(&input),
    ]);
    run(&s, &input).await.result.unwrap();
    let reqs = s.server.requests();
    let first = messages(&reqs[0]);
    let third = messages(&reqs[2]);
    assert_eq!(third.len(), first.len() + 2);
    assert_eq!(third[first.len()].1, "second bad reply");
    assert!(!third.iter().any(|m| m.1 == "first bad reply"));
}

#[tokio::test]
async fn echoed_reply_is_capped() {
    let input = meeting(3);
    let long = "word ".repeat(2_000);
    let s = setup_with(
        vec![MockResponse::completion(&long, "stop"), valid_reply(&input)],
        |c| c.budget.max_output_tokens = 50,
    );
    run(&s, &input).await.result.unwrap();
    let second = messages(&s.server.requests()[1]);
    let echo = &second[second.len() - 2];
    assert_eq!(echo.0, "assistant");
    assert!(echo.1.len() <= 4 * 50 + 64, "{} bytes", echo.1.len());
    assert!(echo.1.starts_with("word word"));
}

#[tokio::test]
async fn echo_is_dropped_when_the_repair_prompt_would_exceed_the_budget() {
    let input = meeting(3);
    let max_output = 600;
    let PromptPlan::Single(p) = plan(
        &input,
        ContextBudget {
            context_window: 100_000,
            max_output_tokens: max_output,
        },
    )
    .unwrap() else {
        panic!("single");
    };
    // Room for the prompt, its reply budget and the (largest) repair message, not for a
    // 2400-byte echoed reply.
    let budget = ContextBudget {
        context_window: p.estimated_tokens + REPAIR_MAX_TOKENS + p.max_output_tokens,
        max_output_tokens: max_output,
    };
    let long = "{ ".repeat(2_000);
    let s = setup_with(
        vec![MockResponse::completion(&long, "stop"), valid_reply(&input)],
        |c| c.budget = budget,
    );
    run(&s, &input).await.result.unwrap();
    let first = messages(&s.server.requests()[0]);
    let second = messages(&s.server.requests()[1]);
    assert_eq!(second.len(), first.len() + 1, "echo dropped");
    assert_eq!(second.last().unwrap().0, "user");
    assert!(second.last().unwrap().1.contains("not valid JSON"));
}

#[tokio::test]
async fn non_format_errors_do_not_fall_back() {
    let input = meeting(2);
    let s = setup(vec![MockResponse::status(404, "Not Found")]);
    let out = run(&s, &input).await;
    assert!(
        matches!(&out.result, Err(LlmError::Http { status: 404, message }) if message == "Not Found"),
        "{:?}",
        out.result
    );
    assert_eq!(out.report.requests, 1);

    let s = setup(vec![MockResponse::ok("<html>proxy</html>")]);
    assert!(matches!(
        run(&s, &input).await.result,
        Err(LlmError::BadResponse(_))
    ));
}

#[tokio::test]
async fn cancellation_abandons_the_request_promptly() {
    let input = meeting(2);
    let s = setup(vec![valid_reply(&input).delayed(Duration::from_secs(5))]);
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

    let s = setup(vec![]);
    let cancel = CancelToken::new();
    cancel.cancel();
    let out = s.summarizer.summarize_with_report(&input, &cancel).await;
    assert!(matches!(out.result, Err(LlmError::Cancelled)));
    assert!(s.server.requests().is_empty());
}

#[tokio::test]
async fn request_timeout_is_applied() {
    let input = meeting(2);
    let s = setup_with(
        vec![valid_reply(&input).delayed(Duration::from_secs(3))],
        |c| c.request_timeout = Duration::from_millis(300),
    );
    let t = Instant::now();
    let out = run(&s, &input).await;
    assert!(
        matches!(out.result, Err(LlmError::Net(NetError::Transport(_)))),
        "{:?}",
        out.result
    );
    assert!(
        t.elapsed() < Duration::from_millis(2_500),
        "{:?}",
        t.elapsed()
    );
}

#[tokio::test]
async fn endpoint_outside_the_allowlist_is_refused_before_sending() {
    let input = meeting(2);
    let server = MockServer::start(vec![valid_reply(&input)]).unwrap();
    let audit = AuditLog::new();
    let net = NetClient::new(
        NetConfig::new(Allowlist::new(), Arc::new(ManualClock::new(T0))).audit(audit.clone()),
    )
    .unwrap();
    let config = LlmConfig::new(server.endpoint().as_str(), "m").unwrap();
    let summarizer = LlmSummarizer::new(ChatClient::new(net, config));
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

/// A reply with one decision per cited segment number (texts differ, so a merge never
/// repeats itself), each citing that number; bullet and action item cite the first.
fn merged_json(numbers: &[usize]) -> String {
    let decisions: Vec<Value> = numbers
        .iter()
        .map(|n| json!({"text": format!("Decision from segment {n}"), "source_segment_ids": [n]}))
        .collect();
    json!({
        "title": "Beta planning",
        "tldr": "The beta ships on Friday.",
        "enhanced_notes": [{"heading": "Beta", "bullets": [{"text": "Ship Friday", "source_segment_ids": [numbers[0]]}]}],
        "decisions": decisions,
        "action_items": [{"owner": "Ana", "task": "Write release notes", "due": "Thursday", "source_segment_ids": [numbers[0]]}],
        "open_questions": [],
        "topics": ["beta"]
    })
    .to_string()
}

/// The script a correct run consumes for a map-reduce `input`: one valid partial per
/// chunk citing the chunk's first segment number (optionally preceded, for part 1, by a
/// reply citing a segment outside the chunk), then one reply per multi-partial reduce
/// group at every level, keeping one decision per decision of its partials. Returns the
/// script, the number of map parts and reduce requests, and the ids the final summary's
/// decisions cite.
fn map_reduce_script(
    input: &SummaryInput,
    budget: ContextBudget,
    bad_first_part: bool,
) -> (Vec<MockResponse>, usize, usize, BTreeSet<EntityId>) {
    let PromptPlan::MapReduce(chunks) = plan(input, budget).unwrap() else {
        panic!("expected map-reduce");
    };
    let ranges: Vec<std::ops::Range<usize>> = chunks.iter().map(|c| c.segments.clone()).collect();
    map_reduce_script_for(input, budget, &ranges, bad_first_part)
}

/// [`map_reduce_script`] over map parts covering `chunks` (segment index ranges).
fn map_reduce_script_for(
    input: &SummaryInput,
    budget: ContextBudget,
    chunks: &[std::ops::Range<usize>],
    bad_first_part: bool,
) -> (Vec<MockResponse>, usize, usize, BTreeSet<EntityId>) {
    let mut script = Vec::new();
    if bad_first_part {
        let outside = chunks[1].start + 1;
        script.push(MockResponse::completion(&merged_json(&[outside]), "stop"));
    }
    // A reply as the summarizer stores it: numbers mapped back to segment ids.
    let stored = |reply: &str| {
        let numbers = earshot_core::prompt::segment_numbers(input);
        let v: Value = serde_json::from_str(reply).unwrap();
        earshot_core::summary::validate_reply(&v, &numbers, &(0..input.segments.len()).into())
            .unwrap()
            .summary
    };
    let mut level = Vec::new();
    for c in chunks {
        let reply = merged_json(&[c.start + 1]);
        level.push(stored(&reply));
        script.push(MockResponse::completion(&reply, "stop"));
    }
    let mut reduces = 0;
    loop {
        let groups = reduce_groups(input, &level, budget).unwrap();
        let mut next = Vec::new();
        for g in &groups {
            if g.len() == 1 && groups.len() > 1 {
                next.push(level[g.start].clone());
                continue;
            }
            let numbers: Vec<usize> = level[g.clone()]
                .iter()
                .flat_map(|p| {
                    p.decisions
                        .iter()
                        .flat_map(|d| d.source_segment_ids.clone())
                })
                .map(|id| input.segments.iter().position(|s| s.id == id).unwrap() + 1)
                .collect();
            let reply = merged_json(&numbers);
            next.push(stored(&reply));
            script.push(MockResponse::completion(&reply, "stop"));
            reduces += 1;
        }
        level = next;
        if groups.len() == 1 {
            break;
        }
    }
    let cited = level[0]
        .decisions
        .iter()
        .flat_map(|d| d.source_segment_ids.iter().copied())
        .collect();
    (script, chunks.len(), reduces, cited)
}

#[tokio::test]
async fn a_merge_with_no_valid_reply_joins_the_parts_and_says_so() {
    let input = meeting(40);
    let budget = ContextBudget {
        context_window: 3_564,
        max_output_tokens: 200,
    };
    let (mut script, parts, reduces, cited) = map_reduce_script(&input, budget, true);
    assert_eq!((parts, reduces), (2, 1));
    script.pop();
    for _ in 0..3 {
        script.push(MockResponse::completion("no summary here", "stop"));
    }
    let s = setup_with(script, |c| c.budget = budget);
    let out = run(&s, &input).await;
    let summary = out.result.unwrap();
    assert!(
        summary
            .tldr
            .starts_with(earshot_core::summary::JOINED_SUMMARY_NOTICE)
    );
    assert_eq!(out.report.joined_parts, 2);
    let got: BTreeSet<EntityId> = summary
        .decisions
        .iter()
        .flat_map(|d| d.source_segment_ids.iter().copied())
        .collect();
    assert_eq!(got, cited, "both parts are kept");
    assert_eq!((s.server.remaining(), s.server.unscripted()), (0, 0));
}

#[tokio::test]
async fn long_meetings_are_mapped_per_chunk_and_reduced() {
    let input = meeting(40);
    let budget = ContextBudget {
        // Two parts and one merge with this meeting (requests leave REPAIR_MAX_TOKENS free
        // beside their reply budget). 3,100 plus the system message's growth in T6.20f
        // (464 tokens).
        context_window: 3_564,
        max_output_tokens: 200,
    };
    let (script, parts, reduces, cited) = map_reduce_script(&input, budget, true);
    assert_eq!(
        (parts, reduces),
        (2, 1),
        "this budget gives one reduce level"
    );
    let s = setup_with(script, |c| c.budget = budget);
    let out = run(&s, &input).await;
    let summary = out.result.unwrap();
    let got: BTreeSet<EntityId> = summary
        .decisions
        .iter()
        .flat_map(|d| d.source_segment_ids.iter().copied())
        .collect();
    assert_eq!(got, cited);
    assert_eq!(cited.len(), 2, "both parts are cited");
    assert_eq!(out.report.map_parts, 2);
    assert_eq!(out.report.reduce_steps, 1);
    assert_eq!(
        out.report.repairs, 1,
        "part 1 cited a segment outside its chunk"
    );
    assert_eq!(out.report.requests, 2 + 1 + 1);
    assert_eq!((s.server.remaining(), s.server.unscripted()), (0, 0));

    let reqs = s.server.requests();
    assert_eq!(reqs[0].json()["max_tokens"], 200);
    let repair = messages(&reqs[1]);
    assert!(
        repair
            .last()
            .unwrap()
            .1
            .contains("is not one of the transcript lines you were given")
    );
    let reduce = messages(reqs.last().unwrap());
    assert!(reduce[1].1.contains("Partial summary 1 of 2:"));
    assert_eq!(reduce[0].0, serde_json::to_value(Role::System).unwrap());
}

#[tokio::test]
async fn reduce_is_hierarchical_when_partials_do_not_fit_together() {
    let input = meeting(40);
    let budget = ContextBudget {
        // Three parts whose partials merge two at a time: two merge levels. 2,800 plus the
        // system message's growth in T6.20f (464 tokens).
        context_window: 3_264,
        max_output_tokens: 200,
    };
    let (script, parts, reduces, cited) = map_reduce_script(&input, budget, false);
    assert!(
        reduces >= 2,
        "{parts} parts, {reduces} reduces: expected several levels"
    );
    let s = setup_with(script, |c| c.budget = budget);
    let out = run(&s, &input).await;
    let summary = out.result.unwrap();
    let got: BTreeSet<EntityId> = summary
        .decisions
        .iter()
        .flat_map(|d| d.source_segment_ids.iter().copied())
        .collect();
    assert_eq!(got, cited);
    assert_eq!(cited.len(), parts, "every part reaches the final summary");
    assert_eq!(out.report.map_parts, parts);
    assert_eq!(out.report.reduce_steps, reduces);
    assert_eq!(out.report.requests, parts + reduces);
    assert_eq!((s.server.remaining(), s.server.unscripted()), (0, 0));
}

fn assert_send<T: Send>(_: &T) {}

async fn via_trait<S: Summarizer>(s: &S, input: &SummaryInput) -> Result<SummaryV1, S::Error> {
    let cancel = CancelToken::new();
    let fut = s.summarize(input, &cancel);
    assert_send(&fut);
    fut.await
}

#[tokio::test]
async fn openai_summarizer_implements_the_core_trait() {
    let input = meeting(2);
    let s = setup(vec![valid_reply(&input)]);
    let summary = via_trait(&s.summarizer, &input).await.unwrap();
    assert_eq!(summary.title, "Beta planning");
}

/// The step names of a timed run.
fn step_names(t: &TimedRun) -> Vec<&'static str> {
    t.steps.iter().map(|s| s.step).collect()
}

#[tokio::test]
async fn timed_runs_name_the_production_plan_steps() {
    // A meeting that fits one request: one `single` step, as `summarize_with_report`.
    let input = meeting(3);
    let s = setup(vec![
        MockResponse::completion(&summary_json(&[9]), "stop"),
        valid_reply(&input),
    ]);
    let t = s
        .summarizer
        .summarize_timed(&input, &CancelToken::new())
        .await;
    assert!(t.run.result.is_ok(), "{:?}", t.run.result);
    assert_eq!(step_names(&t), ["single"]);
    assert_eq!(
        t.steps[0].requests, 2,
        "the repair round belongs to its step"
    );
    assert_eq!((t.run.report.requests, t.run.report.repairs), (2, 1));

    // A map-reduce plan: one `map` step per part, then `reduce`.
    let input = meeting(40);
    let budget = ContextBudget {
        context_window: 3_564,
        max_output_tokens: 200,
    };
    let (script, parts, reduces, _) = map_reduce_script(&input, budget, false);
    let s = setup_with(script, |c| c.budget = budget);
    let t = s
        .summarizer
        .summarize_timed(&input, &CancelToken::new())
        .await;
    assert!(t.run.result.is_ok(), "{:?}", t.run.result);
    let mut want = vec!["map"; parts];
    want.extend(vec!["reduce"; reduces]);
    assert_eq!(step_names(&t), want);
    assert_eq!(
        t.steps.iter().map(|s| s.requests).sum::<usize>(),
        t.run.report.requests
    );
}

#[tokio::test]
async fn chunked_runs_map_every_time_chunk_with_the_production_requests_then_reduce() {
    // 40 segments 10 s apart: four 100 s chunks, although the whole meeting fits one
    // request with the default budget.
    let input = meeting(40);
    let budget = earshot_llm::DEFAULT_BUDGET;
    assert!(matches!(
        plan(&input, budget).unwrap(),
        PromptPlan::Single(_)
    ));
    let ranges = time_chunks(&input, 100).unwrap();
    assert_eq!(ranges.len(), 4);
    let (script, parts, reduces, cited) = map_reduce_script_for(&input, budget, &ranges, true);
    assert_eq!((parts, reduces), (4, 1));
    let s = setup(script);
    let t = s
        .summarizer
        .summarize_chunks_timed(&input, &ranges, &CancelToken::new())
        .await;
    let summary = t.run.result.as_ref().unwrap();
    let got: BTreeSet<EntityId> = summary
        .decisions
        .iter()
        .flat_map(|d| d.source_segment_ids.iter().copied())
        .collect();
    assert_eq!(got, cited);
    assert_eq!(step_names(&t), ["map", "map", "map", "map", "reduce"]);
    assert_eq!(
        t.steps[0].requests, 2,
        "part 1 cited outside its chunk once"
    );
    assert_eq!(t.run.report.map_parts, 4);
    assert_eq!(t.run.report.reduce_steps, 1);
    assert_eq!(t.run.report.requests, 4 + 1 + 1);
    assert_eq!((s.server.remaining(), s.server.unscripted()), (0, 0));
    // Every map request is the one `plan_chunks` builds for its chunk.
    let chunks = plan_chunks(&input, budget, &ranges).unwrap();
    let reqs = s.server.requests();
    let firsts = [0, 2, 3, 4];
    for (chunk, at) in chunks.iter().zip(firsts) {
        let sent: Vec<String> = messages(&reqs[at]).into_iter().map(|(_, c)| c).collect();
        let want: Vec<String> = chunk
            .prompt
            .messages
            .iter()
            .map(|m| m.content.clone())
            .collect();
        assert_eq!(sent, want, "request {at}");
    }
    assert!(
        messages(reqs.last().unwrap())[1]
            .1
            .contains("Partial summary 1 of 4:")
    );

    // One chunk is the ordinary production plan (a single request).
    let one: Vec<std::ops::Range<usize>> = std::iter::once(0..input.segments.len()).collect();
    let s = setup(vec![valid_reply(&input)]);
    let t = s
        .summarizer
        .summarize_chunks_timed(&input, &one, &CancelToken::new())
        .await;
    assert!(t.run.result.is_ok());
    assert_eq!(step_names(&t), ["single"]);
}
