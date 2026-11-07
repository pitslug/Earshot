//! The agent-CLI candidates: Claude Code (T7.4gb, `--claude-code`), Claude on the user's
//! Claude.ai sign-in through the installed CLI, built as `earshot_runtime`'s
//! `SummaryService::claude_code` builds it (`ClaudeCli::find`, `ClaudeCodeConfig::new` with
//! the production budget `CLAUDE_CODE_BUDGET` and request timeout, `ClaudeCodeClient`,
//! `LlmSummarizer`); and Codex (T7.4gc, `--codex`), OpenAI on the user's ChatGPT sign-in,
//! built as `SummaryService::codex` builds it (`CodexCli::find`, or the evaluation-only
//! `--codex-cli <path>`; `CodexConfig::new` with `CODEX_BUDGET`, `CodexClient`). Both run the
//! production prompt, validation, repairs and citation rules. Not through `SummaryService`
//! itself, which reads its meeting from a store.
//!
//! Two plans: without `--chunk-seconds` the production plan (`earshot_core::prompt::plan`
//! over the provider's 200k-token budget: one request when the meeting fits, else
//! map-reduce), the "after the meeting" condition; with it one map request per time chunk
//! (`earshot_core::prompt::time_chunks`, as the local candidates), then the reduce, the
//! "chunks during the meeting" condition (`LlmSummarizer::summarize_chunks_timed`,
//! evaluation only).
//!
//! Only public AMI reference transcripts are handed over (`parse_args` refuses other
//! sources). Every run's handovers (characters handed over and answered, a fixed outcome
//! word; never text) go into the report. The CLI's output may name the model that answered,
//! but nothing of it leaves `earshot_llm` except the reply text (AGENTS.md), so the report
//! records the model asked for and the CLI's `--version`.

use std::path::PathBuf;
use std::sync::Arc;

use earshot_core::asr::CancelToken;
use earshot_core::prompt::{ContextBudget, SummaryInput, time_chunks};
use earshot_core::summary::SummaryV1;
use earshot_llm::claude_code::{CLAUDE_CODE_BUDGET, STATUS_TIMEOUT};
use earshot_llm::codex::{CODEX_BUDGET, STATUS_TIMEOUT as CODEX_STATUS_TIMEOUT};
use earshot_llm::handover::{Handover, HandoverLog, HandoverOutcome};
use earshot_llm::{
    ClaudeCli, ClaudeCodeClient, ClaudeCodeConfig, CodexCli, CodexClient, CodexConfig, CodexError,
    CodexSignIn, LlmSummarizer, SignIn, TimedRun,
};
use earshot_llm_local::summarize::StepTime;
use serde_json::{Value, json};

/// The handovers of one candidate's client, as the report lists them.
enum Handovers {
    ClaudeCode(HandoverLog),
    Codex(HandoverLog<CodexError>),
}

impl Handovers {
    /// `(characters handed over, characters answered, outcome word)` per handover.
    fn rows(&self) -> Vec<(usize, usize, &'static str)> {
        fn row<E>(
            h: &Handover<E>,
            kind: impl Fn(&E) -> &'static str,
        ) -> (usize, usize, &'static str) {
            match &h.outcome {
                HandoverOutcome::Answered { characters } => (h.characters, *characters, "answered"),
                HandoverOutcome::Failed(e) => (h.characters, 0, kind(e)),
                HandoverOutcome::Cancelled => (h.characters, 0, "cancelled"),
            }
        }
        match self {
            Self::ClaudeCode(log) => log.entries().iter().map(|h| row(h, |e| e.kind())).collect(),
            Self::Codex(log) => log.entries().iter().map(|h| row(h, |e| e.kind())).collect(),
        }
    }
}

/// The summariser of one agent-CLI run.
pub struct AgentCliRun {
    /// `claude-code` or `codex`: the report's `kind` and `backend`.
    kind: &'static str,
    budget: ContextBudget,
    runtime: tokio::runtime::Runtime,
    summarizer: LlmSummarizer,
    handovers: Handovers,
    chunk_seconds: Option<u32>,
    version: Option<String>,
    model: Option<String>,
}

fn runtime() -> Result<tokio::runtime::Runtime, String> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| e.to_string())
}

impl AgentCliRun {
    /// The installed Claude Code CLI with `model` (`None`: its default), after checking it
    /// is signed in.
    pub fn claude_code(model: Option<String>, chunk_seconds: Option<u32>) -> Result<Self, String> {
        let cli = ClaudeCli::find().ok_or("no Claude Code CLI found (install Claude Code)")?;
        if cli.sign_in(STATUS_TIMEOUT) == SignIn::NotSignedIn {
            return Err("Claude Code is not signed in (run `claude` and sign in)".into());
        }
        let version = cli.version(STATUS_TIMEOUT).ok();
        let config = ClaudeCodeConfig::new(cli, model.clone()).map_err(|e| e.to_string())?;
        let client = ClaudeCodeClient::new(config, Arc::new(earshot_net::SystemClock));
        Ok(Self {
            kind: "claude-code",
            budget: CLAUDE_CODE_BUDGET,
            handovers: Handovers::ClaudeCode(client.handovers()),
            runtime: runtime()?,
            summarizer: LlmSummarizer::new(client),
            chunk_seconds,
            version,
            model,
        })
    }

    /// Codex with `model` (`None`: its default): the CLI at `cli` (`--codex-cli`, evaluation
    /// only) or the one production finds (`CodexCli::find`), after checking it is signed in.
    pub fn codex(
        model: Option<String>,
        cli: Option<PathBuf>,
        chunk_seconds: Option<u32>,
    ) -> Result<Self, String> {
        let cli = match cli {
            Some(p) => CodexCli::at(p),
            None => CodexCli::find()
                .ok_or("no Codex CLI found (install Codex, or pass --codex-cli <path>)")?,
        };
        if cli.sign_in(CODEX_STATUS_TIMEOUT) == CodexSignIn::NotSignedIn {
            return Err("Codex is not signed in (run `codex login`)".into());
        }
        let version = cli.version(CODEX_STATUS_TIMEOUT).ok();
        let config = CodexConfig::new(cli, model.clone()).map_err(|e| e.to_string())?;
        let client = CodexClient::new(config, Arc::new(earshot_net::SystemClock));
        Ok(Self {
            kind: "codex",
            budget: CODEX_BUDGET,
            handovers: Handovers::Codex(client.handovers()),
            runtime: runtime()?,
            summarizer: LlmSummarizer::new(client),
            chunk_seconds,
            version,
            model,
        })
    }

    /// The report header fields of this candidate.
    pub fn header(&self) -> Value {
        json!({
            "kind": self.kind,
            "backend": self.kind,
            "cli_version": self.version,
            "requested_model": self.model.as_deref().unwrap_or("CLI default"),
            "model_reported": Value::Null,
            "model_note": "The model that answered is not recorded: the CLI's output never \
                           leaves earshot_llm except the reply text.",
            "context_tokens": self.budget.context_window,
            "max_output_tokens": self.budget.max_output_tokens,
        })
    }

    /// One meeting: the summary (or the failure text) and the run report.
    pub fn summarise(&mut self, input: &SummaryInput) -> (Result<SummaryV1, String>, Value) {
        let before = self.handovers.rows().len();
        let cancel = CancelToken::new();
        let timed: TimedRun = match self.chunk_seconds {
            None => self
                .runtime
                .block_on(self.summarizer.summarize_timed(input, &cancel)),
            Some(s) => match time_chunks(input, s) {
                Ok(ranges) => self.runtime.block_on(
                    self.summarizer
                        .summarize_chunks_timed(input, &ranges, &cancel),
                ),
                Err(e) => return (Err(e.to_string()), json!({})),
            },
        };
        let handovers: Vec<Value> = self.handovers.rows()[before..]
            .iter()
            .map(|(characters, answered, outcome)| {
                json!({"characters": characters, "answered_characters": answered, "outcome": outcome})
            })
            .collect();
        let sum = |k: &str| -> u64 { handovers.iter().filter_map(|h| h[k].as_u64()).sum() };
        let steps: Vec<StepTime> = timed
            .steps
            .iter()
            .map(|s| StepTime {
                step: s.step,
                wall: s.wall,
                requests: s.requests,
            })
            .collect();
        let r = &timed.run.report;
        let report = json!({
            "plan": plan_name(r.map_parts, self.chunk_seconds),
            "requests": r.requests,
            "repairs": r.repairs,
            "map_parts": r.map_parts,
            "reduce_steps": r.reduce_steps,
            "duplicates_dropped": r.duplicates_dropped,
            "characters_handed_over": sum("characters"),
            "characters_answered": sum("answered_characters"),
            "handovers": handovers,
            "steps": steps.iter().map(|s| json!({
                "step": s.step,
                "seconds": s.wall.as_secs_f64(),
                "requests": s.requests,
            })).collect::<Vec<_>>(),
            "timing": crate::step_timing(&steps, self.chunk_seconds.is_some()),
        });
        (timed.run.result.map_err(|e| e.to_string()), report)
    }
}

/// The plan a run used: `single` (one request), `map-reduce` (the production planner split
/// the meeting) or `chunks` (forced time chunks; one chunk is the production plan).
pub fn plan_name(map_parts: usize, chunk_seconds: Option<u32>) -> &'static str {
    match (map_parts, chunk_seconds) {
        (0, _) => "single",
        (_, None) => "map-reduce",
        (_, Some(_)) => "chunks",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sum_eval_claude_code_plan_names_the_plan_that_ran() {
        assert_eq!(plan_name(0, None), "single");
        assert_eq!(
            plan_name(0, Some(600)),
            "single",
            "one chunk is one request"
        );
        assert_eq!(plan_name(3, None), "map-reduce");
        assert_eq!(plan_name(3, Some(600)), "chunks");
    }

    #[test]
    fn sum_eval_codex_header_records_the_budget_and_never_the_answering_model() {
        let missing = std::env::temp_dir()
            .join("earshot-sum-eval-no-codex")
            .join("codex.exe");
        // A `--codex-cli` program that does not start reads as an unknown sign-in: the run
        // is built (a summary would then fail with the start error); nothing is handed over.
        let run = AgentCliRun::codex(None, Some(missing.clone()), None).unwrap();
        let h = run.header();
        assert_eq!(h["kind"], "codex");
        assert_eq!(h["backend"], "codex");
        assert_eq!(h["model_reported"], Value::Null);
        assert_eq!(h["context_tokens"], CODEX_BUDGET.context_window);
        assert_eq!(h["requested_model"], "CLI default");
        assert!(run.handovers.rows().is_empty());
        assert_eq!(h["cli_version"], Value::Null);
        // A model name Codex cannot take is refused before any run.
        assert!(AgentCliRun::codex(Some("-s".into()), Some(missing), None).is_err());
    }
}
