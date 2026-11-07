//! Replay of the committed LLM cases in `fixtures/llm` (PLAN.md 4.1).
//!
//! Layout: `fixtures/llm/<input>.json` holds a [`SummaryInput`]; each
//! `fixtures/llm/cases/<name>.json` holds a description, the [`Provider`] it speaks
//! (`openai` when absent, or `anthropic`), the input file name, an optional
//! [`ContextBudget`] (default: the provider's), the expected request flow ([`Expect`])
//! and the recorded HTTP exchanges, which the [`MockServer`] serves in order on
//! 127.0.0.1. The cases are part of the repository; a missing or unreadable case is an
//! error, never a skip.

use std::collections::BTreeSet;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use earshot_core::asr::CancelToken;
use earshot_core::identity::{EntityId, ManualClock};
use earshot_core::prompt::{ContextBudget, SummaryInput};
use earshot_core::summary::SummaryV1;
use earshot_net::{Allowlist, AuditEntry, AuditLog, NetClient, NetConfig, Url};
use serde::Deserialize;
use serde_json::Value;

use earshot_llm::{LlmClient, LlmConfig, LlmError, LlmSummarizer, Provider, SummaryReport};

use crate::mock::{MockResponse, MockServer, RecordedRequest};

/// Model name sent in replayed requests.
pub const REPLAY_MODEL: &str = "replay-model";

/// How a case must end.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Outcome {
    /// A valid summary (the default). Counted in `sum.schema_replay`.
    #[default]
    Valid,
    /// The client gives up: `LlmError::Invalid` after every repair round
    /// (`1 + MAX_REPAIRS` attempts). Checked for its flow, never counted in
    /// `sum.schema_replay`.
    Invalid,
    /// The server refuses with an error status that must not trigger a format fallback
    /// (for example a bad API key): `LlmError::Http` with the status of the last
    /// exchange. Checked for its flow, never counted in `sum.schema_replay`.
    Http,
}

/// The expected request flow of a case.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Expect {
    /// The response mode of each request in order ([`request_mode`]).
    pub modes: Vec<String>,
    /// Repair rounds.
    pub repairs: usize,
    /// How the run ends.
    #[serde(default)]
    pub outcome: Outcome,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CaseFile {
    description: String,
    #[serde(default)]
    provider: Provider,
    input: String,
    #[serde(default)]
    budget: Option<ContextBudget>,
    expect: Expect,
    exchanges: Vec<Exchange>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Exchange {
    status: u16,
    /// A JSON body, or a string sent verbatim.
    body: Value,
}

/// One loaded case.
#[derive(Debug, Clone)]
pub struct ReplayCase {
    /// File stem of the case.
    pub name: String,
    /// What the case exercises.
    pub description: String,
    /// The API the recorded exchanges speak.
    pub provider: Provider,
    /// The meeting to summarise.
    pub input: SummaryInput,
    /// Context budget for the run.
    pub budget: ContextBudget,
    /// Expected request flow.
    pub expect: Expect,
    /// Responses served in order.
    pub script: Vec<MockResponse>,
}

impl ReplayCase {
    /// The segment ids the summary may cite.
    pub fn segment_ids(&self) -> BTreeSet<EntityId> {
        self.input.segment_ids()
    }
}

/// `fixtures/llm` in this repository (for tests; xtask passes its own root).
pub fn fixtures_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("fixtures")
        .join("llm")
}

/// Loads every `cases/*.json` under `dir`, sorted by name. Fails when the directory or a
/// referenced input is missing or malformed, or when there are no cases.
pub fn load_cases(dir: &Path) -> Result<Vec<ReplayCase>, String> {
    let cases_dir = dir.join("cases");
    let entries = fs::read_dir(&cases_dir).map_err(|e| {
        format!(
            "cannot read {} ({e}); the LLM replay cases are committed in fixtures/llm",
            cases_dir.display()
        )
    })?;
    let mut paths: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "json"))
        .collect();
    paths.sort();
    if paths.is_empty() {
        return Err(format!("no replay cases in {}", cases_dir.display()));
    }
    paths.iter().map(|p| load_case(dir, p)).collect()
}

fn load_case(dir: &Path, path: &Path) -> Result<ReplayCase, String> {
    let read = |p: &Path| fs::read_to_string(p).map_err(|e| format!("{}: {e}", p.display()));
    let file: CaseFile =
        serde_json::from_str(&read(path)?).map_err(|e| format!("{}: {e}", path.display()))?;
    let input_name = Path::new(&file.input);
    if input_name.components().count() != 1 || input_name.file_name().is_none() {
        return Err(format!(
            "{}: input `{}` must be a file name in {}",
            path.display(),
            file.input,
            dir.display()
        ));
    }
    let input_path = dir.join(input_name);
    let input: SummaryInput = serde_json::from_str(&read(&input_path)?)
        .map_err(|e| format!("{}: {e}", input_path.display()))?;
    if file.exchanges.is_empty() {
        return Err(format!("{}: no exchanges", path.display()));
    }
    let script = file
        .exchanges
        .into_iter()
        .map(|x| {
            let body = match x.body {
                Value::String(s) => s,
                v => v.to_string(),
            };
            MockResponse::status(x.status, body)
        })
        .collect();
    let name = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or_default()
        .to_string();
    let budget = file.budget.unwrap_or(
        LlmConfig::for_provider(file.provider, "http://127.0.0.1/", REPLAY_MODEL)
            .map_err(|e| e.to_string())?
            .budget,
    );
    Ok(ReplayCase {
        name,
        description: file.description,
        provider: file.provider,
        input,
        budget,
        expect: file.expect,
        script,
    })
}

/// What happened when a case was replayed.
#[derive(Debug)]
pub struct CaseOutcome {
    /// Case name.
    pub name: String,
    /// The summarizer's result.
    pub result: Result<SummaryV1, LlmError>,
    /// The summarizer's report.
    pub report: SummaryReport,
    /// The response mode of each request the mock received ([`request_mode`]).
    pub modes_sent: Vec<String>,
    /// Every request the mock received, in order.
    pub requests: Vec<RecordedRequest>,
    /// Requests that arrived after the script was used up.
    pub unscripted: usize,
    /// Scripted responses that were never requested.
    pub unused: usize,
    /// The `earshot-net` audit log of the run.
    pub audit: Vec<AuditEntry>,
    /// The mock endpoint the run was configured with.
    pub endpoint: Url,
}

/// The response mode a recorded request asked for. OpenAI-compatible:
/// `response_format.type` (`text` when absent). Anthropic: `json_schema` for
/// `output_config.format`, `tool` for a `tool_choice`, `text` for neither (and
/// `invalid` for both, which the client never sends).
pub fn request_mode(provider: Provider, request: &RecordedRequest) -> String {
    let body = request.json();
    match provider {
        Provider::OpenAi => body
            .pointer("/response_format/type")
            .and_then(Value::as_str)
            .unwrap_or("text"),
        Provider::Anthropic => match (
            body.pointer("/output_config/format/type")
                .and_then(Value::as_str),
            body.get("tool_choice"),
        ) {
            (Some(t), None) => t,
            (None, Some(_)) => "tool",
            (None, None) => "text",
            (Some(_), Some(_)) => "invalid",
        },
    }
    .to_string()
}

/// Replays `case` against a fresh mock server, through an `earshot-net` client whose
/// allowlist holds only the mock endpoint, speaking the case's provider.
pub async fn run_case(case: &ReplayCase) -> io::Result<CaseOutcome> {
    let server = MockServer::start(case.script.clone())?;
    let endpoint = match case.provider {
        Provider::OpenAi => server.endpoint(),
        Provider::Anthropic => server.base_url(),
    };
    let mut allow = Allowlist::new();
    allow
        .allow_url(&endpoint)
        .map_err(|e| io::Error::other(e.to_string()))?;
    let audit = AuditLog::new();
    let net =
        NetClient::new(NetConfig::new(allow, Arc::new(ManualClock::new(0))).audit(audit.clone()))
            .map_err(|e| io::Error::other(e.to_string()))?;
    let mut config = LlmConfig::for_provider(case.provider, endpoint.as_str(), REPLAY_MODEL)
        .map_err(|e| io::Error::other(e.to_string()))?;
    config.budget = case.budget;
    let summarizer = LlmSummarizer::new(LlmClient::new(net, config));
    let run = summarizer
        .summarize_with_report(&case.input, &CancelToken::new())
        .await;
    let requests = server.requests();
    let modes_sent = requests
        .iter()
        .map(|r| request_mode(case.provider, r))
        .collect();
    Ok(CaseOutcome {
        name: case.name.clone(),
        result: run.result,
        report: run.report,
        modes_sent,
        requests,
        unscripted: server.unscripted(),
        unused: server.remaining(),
        audit: audit.entries(),
        endpoint,
    })
}

/// [`run_case`] on a new single-threaded tokio runtime, for callers without one
/// (`cargo xtask accept m4`).
pub fn run_case_blocking(case: &ReplayCase) -> io::Result<CaseOutcome> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?
        .block_on(run_case(case))
}
