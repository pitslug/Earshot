//! `sum_eval`: the summary-quality evaluation (T6.20, PLAN D19), run by `cargo xtask
//! sum-eval`, which builds this example in release and calls it once per candidate model.
//!
//! For every meeting of each job (`--job <split>:<source>`) it builds the production summary
//! input, summarises it with the candidate and scores the result against the AMI human
//! abstractive summary (scoring rules: `score.rs`; reference transcript rules: `ami.rs`):
//!
//! - source `reference`: the AMI manual words of every participant as speaker turns named
//!   by role;
//! - source `earshot`: the `SummaryInput` Earshot itself stored for the meeting
//!   (`earshot replay --summary-input`, written by xtask from the IHM headsets: the
//!   production live pass and cleanup, speakers named as Earshot names them).
//!
//! A local candidate (`--model`) runs in process on llama.cpp through
//! `earshot_llm_local::summarize` (the production request plan sized to the model,
//! grammar-constrained, validated and repaired as the remote summarizer); the backend comes
//! from llama.cpp's load log (`log::classify`) and must be the one asked for. `--remote`
//! instead sends the same inputs through `earshot_llm::LlmSummarizer` to
//! `EARSHOT_LLM_URL` (model `EARSHOT_LLM_MODEL`), the only host allowlisted; it is an
//! informational ceiling and every request is listed in the report. `--claude-code`
//! summarises through the user's installed Claude Code CLI as the production Claude Code
//! provider does (T7.4gb, `claude_cli.rs`; AMI reference transcripts only); `--codex` the
//! same through the user's installed Codex CLI (T7.4gc; `--codex-cli <path>` names the CLI
//! for an evaluation, else it is found as production finds it). The report (JSON) goes to
//! `--out`.
//!
//! Missing fixtures or models fail with the fetch command; nothing is skipped.

mod ami;
mod claude;
mod claude_cli;
mod facts;
mod judge;
mod score;
mod two_track;

use std::collections::BTreeMap;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::Instant;

use earshot_core::asr::{CancelToken, MeetingTime};
use earshot_core::identity::EntityId;
use earshot_core::prompt::{
    MeetingInfo, PromptSegment, PromptVariant, SummaryInput, Template, time_chunks,
};
use earshot_core::summary::SummaryV1;
use earshot_llm_local::grammar::SummaryLimits;
use earshot_llm_local::summarize::{
    StepTime, SummaryFailure, SummaryOptions, SummaryReport, TwoCallActions, summarize,
    summarize_chunks, summarize_chunks_two_call,
};
use earshot_llm_local::{LoadOptions, LocalLlm};
use serde_json::{Value, json};

const FETCH: &str = "cargo xtask fetch-fixtures --set ami-sum";

/// The request format: indented JSON replies (`earshot_llm_local::grammar::summary_grammar`;
/// compact JSON was tried on 2026-10-25 and dropped, see its docs), transcript segments cited
/// by number, content-sized reply budgets and the loop guard. Recorded in
/// the report and part of the checkpoint key, so rows made with an earlier format (segment
/// ids in the prompt, one fixed reply budget, the first reply ratios) are never reused. `-r2`
/// is the second set of reply ratios (`earshot_core::prompt::REPLY_PER_*`), `-r3` adds
/// dropping duplicate items (`earshot_core::summary::drop_repeats`), `-r4` the review fixes
/// (action items keyed on owner and task, bullets per section, merges citing only their
/// partials' segments, a number cited twice rejected), `-r5` the T6.20e `settled` rules as
/// the production prompt (T6.20f; the earlier rules are `--prompt-variant previous`).
const GRAMMAR: &str = "indented-numbered-r5";

/// The format key of a run: [`GRAMMAR`], plus `-chunk<seconds>` for forced chunking
/// (`earshot_core::prompt::time_chunks`), plus `-twocall` for the T6.20c two-call chunks,
/// plus `-twotrack` for the T8.3ja two-track chunks (`two_track.rs`), plus
/// `-prompt-<variant>` for a T6.20b prompt variant other than the production one, so rows
/// made with different chunking, calls or rules are never reused for each other, plus
/// `-claude-code` for the T7.4gb Claude Code candidate or `-codex` for the T7.4gc Codex
/// candidate (same place). xtask's `sum_eval::format_key` builds the same string.
/// An agent-CLI candidate (`claude_cli.rs`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AgentCli {
    /// `--claude-code` (T7.4gb).
    ClaudeCode,
    /// `--codex` (T7.4gc).
    Codex,
}

impl AgentCli {
    /// The engine flag.
    fn flag(self) -> &'static str {
        match self {
            Self::ClaudeCode => "--claude-code",
            Self::Codex => "--codex",
        }
    }
}

fn format_key(
    chunk_seconds: Option<u32>,
    variant: PromptVariant,
    two_call: bool,
    two_track: bool,
    agent: Option<AgentCli>,
) -> String {
    let mut key = match chunk_seconds {
        Some(s) => format!("{GRAMMAR}-chunk{s}"),
        None => GRAMMAR.to_owned(),
    };
    if two_call {
        key.push_str("-twocall");
    }
    if two_track {
        key.push_str("-twotrack");
    }
    match agent {
        Some(AgentCli::ClaudeCode) => key.push_str("-claude-code"),
        Some(AgentCli::Codex) => key.push_str("-codex"),
        None => {}
    }
    if !variant.is_current() {
        key.push_str("-prompt-");
        key.push_str(variant.name());
    }
    key
}

#[derive(Debug, Default)]
struct Args {
    annotations: PathBuf,
    earshot_inputs: Option<PathBuf>,
    jobs: Vec<(String, String)>,
    meetings: BTreeMap<String, Vec<String>>,
    model: Option<PathBuf>,
    model_id: String,
    /// Text after the template's assistant header (xtask's candidate table: what the
    /// model's own template renders there with thinking off).
    assistant_prefix: String,
    gpu: bool,
    threads: Option<i32>,
    context: u32,
    max_output: usize,
    remote: bool,
    out: PathBuf,
    /// Finished result files to re-score in place (no generation).
    rescore: Vec<PathBuf>,
    /// Write the Anthropic requests of every meeting here instead of summarising
    /// (`claude.rs`).
    export_prompts: Option<PathBuf>,
    /// Score the replies Claude Code agents wrote here for the exported requests.
    import_replies: Option<PathBuf>,
    /// Write judge packets here (`judge.rs`).
    judge_packets: Option<PathBuf>,
    /// Stored results to take summaries from: `<model>=<result.json>`.
    summaries: Vec<(String, PathBuf)>,
    /// Exported-prompt replies to turn into packets of model `--label`.
    judge_replies: Option<PathBuf>,
    /// Forced chunking: a map request per this many seconds of meeting time (`chunks.rs`).
    chunk_seconds: Option<u32>,
    /// The shared prompt rules (T6.20b); the production rules unless set.
    prompt_variant: PromptVariant,
    /// Two-call chunks (T6.20c): per chunk one request for the action items and one for
    /// everything else, then a merge that folds reworded actions. Needs `--chunk-seconds`.
    two_call: bool,
    /// Two-track chunks (T8.3ja, `two_track.rs`). Needs `--chunk-seconds`; reference source
    /// only.
    two_track: bool,
    /// Earlier blinded judge packets to carry into `--judge-packets` unchanged:
    /// `<model>=<packet dir>`, the model's label read from `--prior-key`.
    prior_packets: Vec<(String, PathBuf)>,
    /// The blinding key of the `--prior-packets` directories.
    prior_key: Option<PathBuf>,
    /// Judge verdicts to count (`judge.rs`), one directory per meeting.
    import_verdicts: Option<PathBuf>,
    /// Blinded judge packets: write (with `--judge-packets`) or read (with
    /// `--import-verdicts`) this key of `S<n>` labels to models.
    blind_key: Option<PathBuf>,
    /// Split the reference sentences of `--meetings` into atomic facts in this file
    /// (T6.20a, `facts.rs`), with the local `--model`.
    split_facts: Option<PathBuf>,
    /// The atomic facts file: `--judge-packets` and `--import-verdicts` use its facts in place
    /// of the sentences.
    facts: Option<PathBuf>,
    /// Judge the packets in this directory with the local `--model` (`facts.rs`), writing the
    /// packets and verdicts to `--out`.
    judge_facts: Option<PathBuf>,
    /// Compare the verdicts in this directory (the hand-labelled side) with `--against`.
    agreement: Option<PathBuf>,
    /// The other side of `--agreement`.
    against: Option<PathBuf>,
    /// Summarise through the installed Claude Code CLI (T7.4gb, `claude_cli.rs`).
    claude_code: bool,
    /// Claude Code's `--model` (`earshot_llm::claude_code::check_model`); its default unless
    /// set. Needs `--claude-code`.
    claude_model: Option<String>,
    /// Summarise through the Codex CLI (T7.4gc, `claude_cli.rs`).
    codex: bool,
    /// Codex's `-m` (`earshot_llm::codex::check_model`); its default unless set. Needs
    /// `--codex`.
    codex_model: Option<String>,
    /// The Codex CLI to run (evaluation only; without it `CodexCli::find`, as production).
    /// Needs `--codex`.
    codex_cli: Option<PathBuf>,
}

impl Args {
    /// The agent-CLI candidate of this run, if any.
    fn agent_cli(&self) -> Option<AgentCli> {
        if self.claude_code {
            Some(AgentCli::ClaudeCode)
        } else if self.codex {
            Some(AgentCli::Codex)
        } else {
            None
        }
    }
}

fn parse_args() -> Result<Args, String> {
    let mut a = Args {
        context: 16_384,
        max_output: 8192,
        ..Args::default()
    };
    let mut it = std::env::args().skip(1);
    while let Some(flag) = it.next() {
        let mut val = || it.next().ok_or_else(|| format!("{flag} needs a value"));
        match flag.as_str() {
            "--annotations" => a.annotations = val()?.into(),
            "--earshot-inputs" => a.earshot_inputs = Some(val()?.into()),
            "--job" => {
                let v = val()?;
                let (split, source) = v.split_once(':').ok_or("--job is <split>:<source>")?;
                if !matches!(source, "reference" | "earshot") {
                    return Err(format!("unknown source {source}"));
                }
                a.jobs.push((split.to_owned(), source.to_owned()));
            }
            "--meetings" => {
                let v = val()?;
                let (split, list) = v.split_once('=').ok_or("--meetings is <split>=a,b")?;
                a.meetings.insert(
                    split.to_owned(),
                    list.split(',').map(str::to_owned).collect(),
                );
            }
            "--model" => a.model = Some(val()?.into()),
            "--model-id" => a.model_id = val()?,
            "--assistant-prefix" => a.assistant_prefix = val()?,
            "--gpu" => a.gpu = true,
            "--threads" => a.threads = Some(val()?.parse().map_err(|e| format!("--threads: {e}"))?),
            "--context" => a.context = val()?.parse().map_err(|e| format!("--context: {e}"))?,
            "--max-output" => {
                a.max_output = val()?.parse().map_err(|e| format!("--max-output: {e}"))?;
            }
            "--remote" => a.remote = true,
            "--out" => a.out = val()?.into(),
            "--rescore" => a.rescore.push(val()?.into()),
            "--export-prompts" => a.export_prompts = Some(val()?.into()),
            "--import-replies" => a.import_replies = Some(val()?.into()),
            "--label" => a.model_id = val()?,
            "--judge-packets" => a.judge_packets = Some(val()?.into()),
            "--judge-replies" => a.judge_replies = Some(val()?.into()),
            "--summaries" => {
                let v = val()?;
                let (m, f) = v
                    .split_once('=')
                    .ok_or("--summaries is <model>=<result.json>")?;
                a.summaries.push((m.to_owned(), f.into()));
            }
            "--chunk-seconds" => {
                a.chunk_seconds = Some(
                    val()?
                        .parse()
                        .map_err(|e| format!("--chunk-seconds: {e}"))?,
                );
            }
            "--prompt-variant" => {
                a.prompt_variant = val()?
                    .parse()
                    .map_err(|e| format!("--prompt-variant: {e}"))?;
            }
            "--two-call" => a.two_call = true,
            "--two-track" => a.two_track = true,
            "--prior-packets" => {
                let v = val()?;
                let (m, d) = v
                    .split_once('=')
                    .ok_or("--prior-packets is <model>=<packet dir>")?;
                a.prior_packets.push((m.to_owned(), d.into()));
            }
            "--prior-key" => a.prior_key = Some(val()?.into()),
            "--import-verdicts" => a.import_verdicts = Some(val()?.into()),
            "--split-facts" => a.split_facts = Some(val()?.into()),
            "--facts" => a.facts = Some(val()?.into()),
            "--judge-facts" => a.judge_facts = Some(val()?.into()),
            "--agreement" => a.agreement = Some(val()?.into()),
            "--against" => a.against = Some(val()?.into()),
            "--blind-key" => a.blind_key = Some(val()?.into()),
            "--claude-code" => a.claude_code = true,
            "--claude-model" => a.claude_model = Some(val()?),
            "--codex" => a.codex = true,
            "--codex-model" => a.codex_model = Some(val()?),
            "--codex-cli" => a.codex_cli = Some(val()?.into()),
            other => return Err(format!("unknown argument {other}")),
        }
    }
    if a.split_facts.is_some() {
        if a.annotations.as_os_str().is_empty()
            || a.meetings.is_empty()
            || a.model.is_none()
            || a.model_id.is_empty()
        {
            return Err(
                "--split-facts needs --annotations, --meetings, --model and --model-id".into(),
            );
        }
        return Ok(a);
    }
    if a.judge_facts.is_some() {
        if a.meetings.is_empty() || a.model.is_none() || a.out.as_os_str().is_empty() {
            return Err("--judge-facts needs --meetings, --model and --out".into());
        }
        return Ok(a);
    }
    if a.agreement.is_some() {
        if a.against.is_none() || a.meetings.is_empty() || a.out.as_os_str().is_empty() {
            return Err("--agreement needs --against, --meetings and --out".into());
        }
        return Ok(a);
    }
    if a.facts.is_some() && a.judge_packets.is_none() && a.import_verdicts.is_none() {
        return Err("--facts needs --judge-packets or --import-verdicts".into());
    }
    if a.import_verdicts.is_some() {
        if a.annotations.as_os_str().is_empty()
            || a.meetings.is_empty()
            || a.out.as_os_str().is_empty()
        {
            return Err("--import-verdicts needs --annotations, --meetings and --out".into());
        }
        return Ok(a);
    }
    if a.claude_model.is_some() && !a.claude_code {
        return Err("--claude-model needs --claude-code".into());
    }
    if (a.codex_model.is_some() || a.codex_cli.is_some()) && !a.codex {
        return Err("--codex-model and --codex-cli need --codex".into());
    }
    if let Some(agent) = a.agent_cli() {
        check_agent_cli(&a, agent)?;
    }
    if a.chunk_seconds.is_some()
        && (a.remote
            || a.export_prompts.is_some()
            || a.import_replies.is_some()
            || a.judge_packets.is_some())
    {
        return Err("--chunk-seconds is for local candidates only".into());
    }
    if a.two_call && a.chunk_seconds.is_none() {
        return Err("--two-call needs --chunk-seconds".into());
    }
    if a.two_track
        && (a.chunk_seconds.is_none()
            || a.two_call
            || a.jobs.iter().any(|(_, source)| source != "reference"))
    {
        return Err("--two-track needs --chunk-seconds and reference jobs, not --two-call".into());
    }
    if !a.prior_packets.is_empty() && (a.judge_packets.is_none() || a.prior_key.is_none()) {
        return Err("--prior-packets needs --judge-packets and --prior-key".into());
    }
    if !a.prompt_variant.is_current()
        && (a.remote
            || a.export_prompts.is_some()
            || a.import_replies.is_some()
            || a.judge_packets.is_some()
            || !a.rescore.is_empty())
    {
        return Err("--prompt-variant is for local candidates only".into());
    }
    if !a.rescore.is_empty() {
        if a.annotations.as_os_str().is_empty() {
            return Err("--rescore needs --annotations".into());
        }
        return Ok(a);
    }
    if a.judge_packets.is_some() {
        if a.annotations.as_os_str().is_empty() || a.meetings.is_empty() {
            return Err("--judge-packets needs --annotations and --meetings".into());
        }
        if a.judge_replies.is_some() && a.model_id.is_empty() {
            return Err("--judge-replies needs --label".into());
        }
        return Ok(a);
    }
    if a.export_prompts.is_some() {
        if a.jobs.is_empty() || a.annotations.as_os_str().is_empty() {
            return Err("--export-prompts needs --job, --meetings and --annotations".into());
        }
        return Ok(a);
    }
    if a.import_replies.is_some() {
        if a.jobs.is_empty() || a.out.as_os_str().is_empty() || a.model_id.is_empty() {
            return Err("--import-replies needs --job, --meetings, --label and --out".into());
        }
        return Ok(a);
    }
    if a.jobs.is_empty() || a.out.as_os_str().is_empty() || a.model_id.is_empty() {
        return Err("needs --job, --model-id and --out".into());
    }
    if [a.remote, a.model.is_some(), a.claude_code, a.codex]
        .iter()
        .filter(|x| **x)
        .count()
        != 1
    {
        return Err("give exactly one of --model, --remote, --claude-code and --codex".into());
    }
    Ok(a)
}

/// What a `--claude-code` run refuses: anything but summary jobs over the public AMI
/// reference transcripts (`--job <split>:reference`; Earshot's own replays and private data
/// are never handed to Claude Code), another engine, the evaluation-only modes and prompt
/// variants. `--chunk-seconds` is the "chunks during the meeting" condition. The run itself
/// checks through [`check_agent_cli`]; this name is kept for the Claude Code tests.
#[cfg(test)]
fn check_claude_code(a: &Args) -> Result<(), String> {
    check_agent_cli(a, AgentCli::ClaudeCode)
}

/// What an agent-CLI run refuses ([`check_claude_code`]; the same for `--codex`, T7.4gc).
fn check_agent_cli(a: &Args, agent: AgentCli) -> Result<(), String> {
    let flag = agent.flag();
    if a.remote || a.model.is_some() || (a.claude_code && a.codex) {
        return Err("give exactly one of --model, --remote, --claude-code and --codex".into());
    }
    if a.jobs.iter().any(|(_, source)| source != "reference") || a.earshot_inputs.is_some() {
        return Err(format!(
            "{flag} summarises only the public AMI reference transcripts \
                 (--job <split>:reference, no --earshot-inputs)"
        ));
    }
    if a.two_call || a.two_track || !a.prompt_variant.is_current() || a.gpu {
        return Err(format!(
            "{flag} runs the production prompt and plan only (no --two-call, \
                 --two-track, --prompt-variant or --gpu)"
        ));
    }
    if !a.rescore.is_empty()
        || a.export_prompts.is_some()
        || a.import_replies.is_some()
        || a.judge_packets.is_some()
    {
        return Err(format!(
            "{flag} is a summary run (not --rescore, --export-prompts, \
                 --import-replies or --judge-packets)"
        ));
    }
    if let Some(m) = &a.claude_model {
        earshot_llm::claude_code::check_model(m).map_err(|why| format!("--claude-model: {why}"))?;
    }
    if let Some(m) = &a.codex_model {
        earshot_llm::codex::check_model(m).map_err(|why| format!("--codex-model: {why}"))?;
    }
    Ok(())
}

fn read_latin1(p: &Path) -> Result<String, String> {
    std::fs::read(p)
        .map(|b| ami::latin1(&b))
        .map_err(|e| format!("{}: {e}; run `{FETCH}`", p.display()))
}

/// A deterministic segment id (a valid UUIDv7 bit pattern) for reference turn `n`.
fn ref_id(n: usize) -> EntityId {
    format!("0193a5b0-1c00-7000-8000-{n:012x}")
        .parse()
        .expect("a valid id")
}

/// The reference-source input of `meeting` (rules in `ami.rs`).
fn reference_input(annotations: &Path, meeting: &str) -> Result<(SummaryInput, usize), String> {
    reference_input_timed(annotations, meeting).map(|(input, words, _)| (input, words))
}

/// [`reference_input`] with the end of every segment (ms), for the two-track mode.
fn reference_input_timed(
    annotations: &Path,
    meeting: &str,
) -> Result<(SummaryInput, usize, Vec<u64>), String> {
    let meta = ami::meeting_meta(
        &read_latin1(&annotations.join("corpusResources").join("meetings.xml"))?,
        meeting,
    )?;
    let mut turns = Vec::new();
    let mut attendees = Vec::new();
    let mut words = 0;
    for (letter, _channel, role) in &meta.speakers {
        let name = ami::role_name(role).ok_or_else(|| format!("{meeting}: unknown role {role}"))?;
        attendees.push(name.to_owned());
        let path = annotations
            .join("words")
            .join(format!("{meeting}.{letter}.words.xml"));
        let w = ami::parse_words(&read_latin1(&path)?)
            .map_err(|e| format!("{}: {e}", path.display()))?;
        words += w.iter().filter(|x| !x.punc).count();
        turns.extend(ami::turns(&w, name));
    }
    turns.sort_by(|a, b| a.start.total_cmp(&b.start));
    let ends = turns
        .iter()
        .map(|t| (t.end * 1000.0).round() as u64)
        .collect();
    let segments = turns
        .into_iter()
        .enumerate()
        .map(|(i, t)| PromptSegment {
            id: ref_id(i + 1),
            start: MeetingTime::from_millis((t.start * 1000.0).round() as u64),
            speaker: t.speaker,
            text: t.text,
        })
        .collect();
    Ok((
        SummaryInput {
            meeting: MeetingInfo {
                title: None,
                started: None,
                duration_ms: (meta.duration_s * 1000.0).round() as u64,
                attendees,
            },
            segments,
            notes: Vec::new(),
            template: Template::General,
            prompt_variant: PromptVariant::Current,
            style: Default::default(),
            user_name: None,
        },
        words,
        ends,
    ))
}

fn earshot_input(dir: &Path, meeting: &str) -> Result<(SummaryInput, usize), String> {
    let p = dir.join(format!("{meeting}.json"));
    let text = std::fs::read_to_string(&p).map_err(|e| {
        format!(
            "{}: {e} (written by `cargo xtask sum-eval` from the IHM headsets)",
            p.display()
        )
    })?;
    let input: SummaryInput =
        serde_json::from_str(&text).map_err(|e| format!("{}: {e}", p.display()))?;
    let words = input
        .segments
        .iter()
        .map(|s| s.text.split_whitespace().count())
        .sum();
    Ok((input, words))
}

fn report_json(r: &SummaryReport, chunked: bool) -> Value {
    let rate = |n: usize, s: f64| if s > 0.0 { n as f64 / s } else { 0.0 };
    json!({
        "requests": r.requests,
        "repairs": r.repairs,
        "truncated": r.truncated,
        "joined_parts": r.joined_parts,
        "looping": r.looping,
        "duplicates_dropped": r.duplicates_dropped,
        "max_reply_budget": r.max_reply_budget,
        "invalid_replies": r.invalid_replies,
        "map_parts": r.map_parts,
        "reduce_steps": r.reduce_steps,
        "planner_window": r.planner_window,
        "replans": r.replans,
        "max_prompt_tokens": r.max_prompt_tokens,
        "prompt_tokens": r.prompt_tokens,
        "generated_tokens": r.generated_tokens,
        "prompt_s": r.prompt_time.as_secs_f64(),
        "generate_s": r.generate_time.as_secs_f64(),
        "prompt_tokens_per_s": rate(r.prompt_tokens, r.prompt_time.as_secs_f64()),
        "generated_tokens_per_s": rate(r.generated_tokens, r.generate_time.as_secs_f64()),
        "steps": r.steps.iter().map(|s| json!({
            "step": s.step,
            "seconds": s.wall.as_secs_f64(),
            "requests": s.requests,
        })).collect::<Vec<_>>(),
        "timing": step_timing(&r.steps, chunked),
    })
}

/// Where a run's time went: `map_s` (every map step), `last_map_s`, `reduce_s` (every
/// reduce step: all of them run after the last map) and `hangup_s`, the wait after the
/// meeting ends. For a `chunked` run (forced chunking, summaries made during the meeting)
/// `hangup_s` = `last_map_s` + `reduce_s`: every earlier map step ran during the meeting.
/// An unchunked run starts when the meeting ends, so its `hangup_s` is the whole run
/// (`map_s` + `reduce_s` when the planner split it into map-reduce). A single request
/// counts as `hangup_s` in full.
///
/// A two-call run (T6.20c) has two map steps per chunk, `map-actions` and `map-rest`: both
/// count as map steps, and `last_map_s` is the last chunk's two steps together.
fn step_timing(steps: &[StepTime], chunked: bool) -> Value {
    let is_map = |s: &StepTime| MAP_STEPS.contains(&s.step);
    let secs = |kind: &str| -> f64 {
        steps
            .iter()
            .filter(|s| s.step == kind)
            .map(|s| s.wall.as_secs_f64())
            .fold(0.0, |a, b| a + b)
    };
    // The last chunk's map request(s): the last map step, plus, when it is a two-call
    // `map-rest`, the `map-actions` step right before it (steps are recorded in order, so
    // that is the same chunk's; a chunk whose action request failed has no rest step).
    let last_map: f64 = steps.iter().rposition(is_map).map_or(0.0, |i| {
        let pair = i > 0 && steps[i].step == "map-rest" && steps[i - 1].step == "map-actions";
        let first = if pair { i - 1 } else { i };
        steps[first..=i].iter().map(|s| s.wall.as_secs_f64()).sum()
    });
    let map_s: f64 = MAP_STEPS.iter().map(|k| secs(k)).sum();
    let single = secs("single");
    let reduce = secs("reduce");
    json!({
        "map_steps": steps.iter().filter(|s| is_map(s)).count(),
        "map_s": map_s,
        "last_map_s": last_map,
        "reduce_s": reduce,
        "single_s": single,
        "hangup_s": single + if chunked { last_map } else { map_s } + reduce,
    })
}

/// The step names of map requests: `map` (one request per chunk), `map-actions` and
/// `map-rest` (the two requests per chunk of a two-call run).
const MAP_STEPS: [&str; 3] = ["map", "map-actions", "map-rest"];

/// The action item counts of a two-call run (`summarize_chunks_two_call`).
fn actions_json(a: &TwoCallActions) -> Value {
    json!({
        "per_part": a.per_part,
        "extracted": a.extracted,
        "exact_repeats": a.exact_repeats,
        "to_merge": a.to_merge,
        "final": a.final_count,
        "cap_hit": a.cap_hit,
    })
}

/// One summariser: local llama.cpp or the remote endpoint.
enum Engine {
    Local {
        llm: Box<LocalLlm>,
        options: SummaryOptions,
        chunk_seconds: Option<u32>,
        two_call: bool,
    },
    Remote(Box<remote::Remote>),
    AgentCli(Box<claude_cli::AgentCliRun>),
}

impl Engine {
    /// The summary (or the failure text), the run report as JSON.
    fn summarise(&mut self, input: &SummaryInput) -> (Result<SummaryV1, String>, Value) {
        match self {
            Engine::Local {
                llm,
                options,
                chunk_seconds,
                two_call,
            } => {
                if *two_call {
                    let Some(s) = chunk_seconds else {
                        return (Err("--two-call needs --chunk-seconds".into()), json!({}));
                    };
                    let ranges = match time_chunks(input, *s) {
                        Ok(r) => r,
                        Err(e) => return (Err(e.to_string()), json!({})),
                    };
                    let run = summarize_chunks_two_call(llm.as_mut(), input, options, &ranges);
                    let mut report = report_json(&run.report, chunk_seconds.is_some());
                    report["actions"] = actions_json(&run.actions);
                    if let Err(SummaryFailure::Invalid {
                        last_reply_tail, ..
                    }) = &run.result
                    {
                        report["last_reply_tail"] = json!(last_reply_tail);
                    }
                    return (run.result.map_err(|e| e.to_string()), report);
                }
                let run = match chunk_seconds {
                    None => summarize(llm.as_mut(), input, options),
                    Some(s) => match time_chunks(input, *s) {
                        Ok(ranges) => summarize_chunks(llm.as_mut(), input, options, &ranges),
                        Err(e) => return (Err(e.to_string()), json!({})),
                    },
                };
                let mut report = report_json(&run.report, chunk_seconds.is_some());
                if let Err(SummaryFailure::Invalid {
                    last_reply_tail, ..
                }) = &run.result
                {
                    report["last_reply_tail"] = json!(last_reply_tail);
                }
                (run.result.map_err(|e| e.to_string()), report)
            }
            Engine::Remote(r) => r.summarise(input),
            Engine::AgentCli(c) => c.summarise(input),
        }
    }

    /// The two-track flow (`two_track.rs`) on `input`, whose segments end at `ends_ms`.
    fn summarise_two_track(
        &mut self,
        input: &SummaryInput,
        ends_ms: &[u64],
    ) -> (Result<SummaryV1, String>, Value) {
        let Engine::Local {
            llm,
            options,
            chunk_seconds: Some(seconds),
            ..
        } = self
        else {
            return (
                Err("--two-track needs a local model and --chunk-seconds".into()),
                json!({}),
            );
        };
        match two_track::summarise(llm.as_mut(), input, ends_ms, *seconds, options) {
            Ok((result, report, live_time, counts)) => {
                let mut json = report_json(&report, true);
                json["two_track"] = two_track::report_json(&report, live_time, &counts);
                if let Err(SummaryFailure::Invalid {
                    last_reply_tail, ..
                }) = &result
                {
                    json["last_reply_tail"] = json!(last_reply_tail);
                }
                (result.map_err(|e| e.to_string()), json)
            }
            Err(e) => (Err(e), json!({})),
        }
    }
}

/// Means and pooled counts over a job's meetings.
fn aggregate(meetings: &[Value]) -> Value {
    let n = meetings.len().max(1) as f64;
    let mean = |path: [&str; 2]| {
        meetings
            .iter()
            .map(|m| m["score"][path[0]][path[1]].as_f64().unwrap_or(0.0))
            .sum::<f64>()
            / n
    };
    let sum = |a: &str, b: &str| -> u64 {
        meetings
            .iter()
            .map(|m| m["score"][a][b].as_u64().unwrap_or(0))
            .sum()
    };
    let rsum = |k: &str| -> f64 {
        meetings
            .iter()
            .map(|m| m["report"][k].as_f64().unwrap_or(0.0))
            .sum()
    };
    let items = |k: &str| {
        let (c, r, s) = (sum(k, "caught"), sum(k, "reference"), sum(k, "summary"));
        json!({
            "caught": c, "reference": r, "summary": s,
            "recall": (r > 0).then(|| c as f64 / r as f64),
            "precision": (s > 0).then(|| c as f64 / s as f64),
        })
    };
    let ok = meetings.iter().filter(|m| m["ok"] == true).count();
    let seconds: f64 = meetings
        .iter()
        .map(|m| m["seconds"].as_f64().unwrap_or(0.0))
        .sum();
    let gen_s = rsum("generate_s");
    let prompt_s = rsum("prompt_s");
    let tsum = |k: &str| -> f64 {
        meetings
            .iter()
            .map(|m| m["report"]["timing"][k].as_f64().unwrap_or(0.0))
            .sum()
    };
    let audio_s: f64 = meetings
        .iter()
        .map(|m| m["duration_s"].as_f64().unwrap_or(0.0))
        .sum();
    // Two-call runs (T6.20c): action items in the chunk outputs against the final summary.
    let asum = |k: &str| -> u64 {
        meetings
            .iter()
            .map(|m| m["report"]["actions"][k].as_u64().unwrap_or(0))
            .sum()
    };
    let two_call = meetings
        .iter()
        .any(|m| m["report"]["actions"].is_object())
        .then(|| {
            json!({
                "extracted": asum("extracted"),
                "exact_repeats": asum("exact_repeats"),
                "to_merge": asum("to_merge"),
                "final": asum("final"),
                "cap_hit": asum("cap_hit"),
                "final_meetings": meetings
                    .iter()
                    .filter(|m| m["report"]["actions"]["final"].is_u64())
                    .count(),
            })
        });
    json!({
        "two_call_actions": two_call,
        "timing": {
            "audio_s": audio_s,
            "map_s": tsum("map_s"),
            "last_map_s": tsum("last_map_s"),
            "reduce_s": tsum("reduce_s"),
            "single_s": tsum("single_s"),
            "hangup_s": tsum("hangup_s"),
            "hangup_s_per_meeting": tsum("hangup_s") / n,
            "reduce_s_per_meeting": tsum("reduce_s") / n,
        },
        "meetings": meetings.len(),
        "ok": ok,
        "failed": meetings.len() - ok,
        "rougeL_f": mean(["rougeL", "f"]),
        "rougeLsum_f": mean(["rougeLsum", "f"]),
        "rouge1_f": mean(["rouge1", "f"]),
        "rouge2_f": mean(["rouge2", "f"]),
        "decisions": items("decisions"),
        "actions": items("actions"),
        "decisions_rr": items("decisions_rr"),
        "actions_rr": items("actions_rr"),
        "decisions_cov": items("decisions_cov"),
        "actions_cov": items("actions_cov"),
        "seconds_total": seconds,
        "seconds_per_meeting": seconds / n,
        "generated_tokens_per_s": if gen_s > 0.0 { rsum("generated_tokens") / gen_s } else { 0.0 },
        "prompt_tokens_per_s": if prompt_s > 0.0 { rsum("prompt_tokens") / prompt_s } else { 0.0 },
        "requests": rsum("requests"),
        "repairs": rsum("repairs"),
        "truncated": rsum("truncated"),
        "looping": rsum("looping"),
        "duplicates_dropped": rsum("duplicates_dropped"),
        "invalid_replies": rsum("invalid_replies"),
        "map_parts": rsum("map_parts"),
    })
}

/// Re-scores every stored summary of a finished result file with this build's scoring
/// rules and rewrites the file: scores and aggregates change, the summaries, run reports
/// and timings are kept as they were generated. A failed meeting keeps scoring 0.
fn rescore_file(annotations: &Path, file: &Path) -> Result<(), String> {
    let text = std::fs::read_to_string(file).map_err(|e| format!("{}: {e}", file.display()))?;
    let mut v: Value =
        serde_json::from_str(&text).map_err(|e| format!("{}: {e}", file.display()))?;
    let jobs = v["jobs"]
        .as_array_mut()
        .ok_or_else(|| format!("{}: no jobs", file.display()))?;
    let mut n = 0;
    for job in jobs.iter_mut() {
        let rows = job["meetings"]
            .as_array_mut()
            .ok_or_else(|| format!("{}: a job without meetings", file.display()))?;
        for row in rows.iter_mut() {
            let meeting = row["meeting"].as_str().unwrap_or_default().to_owned();
            let summ_path = annotations
                .join("abstractive")
                .join(format!("{meeting}.abssumm.xml"));
            let reference = ami::parse_abssumm(&read_latin1(&summ_path)?)
                .map_err(|e| format!("{}: {e}", summ_path.display()))?;
            let summary: Option<SummaryV1> = serde_json::from_value(row["summary"].clone())
                .map_err(|e| format!("{} {meeting}: {e}", file.display()))?;
            if summary.is_some() != (row["ok"] == true) {
                return Err(format!(
                    "{} {meeting}: summary and ok disagree",
                    file.display()
                ));
            }
            row["score"] = score::score(summary.as_ref(), &reference).to_json();
            n += 1;
        }
        let rows = job["meetings"].as_array().cloned().unwrap_or_default();
        job["aggregate"] = aggregate(&rows);
    }
    v["rescored"] = json!(true);
    std::fs::write(
        file,
        serde_json::to_vec_pretty(&v).map_err(|e| e.to_string())?,
    )
    .map_err(|e| format!("{}: {e}", file.display()))?;
    eprintln!("sum_eval: re-scored {n} meetings in {}", file.display());
    Ok(())
}

/// The summary input of `meeting` for `source`.
fn load_input(a: &Args, source: &str, meeting: &str) -> Result<(SummaryInput, usize), String> {
    match source {
        "reference" => reference_input(&a.annotations, meeting),
        _ => earshot_input(
            a.earshot_inputs
                .as_deref()
                .ok_or("--earshot-inputs missing")?,
            meeting,
        ),
    }
}

/// The meetings of `split` (`--meetings`).
fn split_meetings<'a>(a: &'a Args, split: &str) -> Result<&'a Vec<String>, String> {
    a.meetings
        .get(split)
        .ok_or_else(|| format!("no --meetings for split {split}"))
}

/// `path` with its directory resolved by the file system (the file itself need not exist),
/// so two spellings of one place compare equal.
fn canonical_file(path: &Path) -> Result<PathBuf, String> {
    let name = path
        .file_name()
        .ok_or_else(|| format!("{}: not a file path", path.display()))?;
    let parent = match path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p,
        _ => Path::new("."),
    };
    let parent = std::fs::canonicalize(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
    Ok(parent.join(name))
}

/// Refuses a `--judge-packets` run that would mix packets, unblind them or overwrite an
/// earlier round: a model name given twice (`--summaries`, `--prior-packets`, and `--label`
/// with `--judge-replies`; the import would sum both under one model), a blinding key
/// inside the packet directory, or a `--prior-packets` directory that is the packet
/// directory or inside it (the earlier round's packets would be overwritten; they may be
/// the only copy of an anchor). Paths are judged resolved, not by spelling; a prior
/// directory must exist. Creates the packet directory.
fn check_judge_sources(a: &Args, dir: &Path) -> Result<(), String> {
    let mut names: Vec<&str> = a.summaries.iter().map(|(m, _)| m.as_str()).collect();
    names.extend(a.prior_packets.iter().map(|(m, _)| m.as_str()));
    if a.judge_replies.is_some() {
        names.push(&a.model_id);
    }
    let mut seen = std::collections::BTreeSet::new();
    if let Some(twice) = names.iter().find(|n| !seen.insert(**n)) {
        return Err(format!(
            "model {twice} is given twice (--summaries, --prior-packets, --label): each packet \
             needs its own name"
        ));
    }
    std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let packets = std::fs::canonicalize(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    if let Some(k) = &a.blind_key
        && canonical_file(k)?.starts_with(&packets)
    {
        return Err("the blinding key must not be inside the packet directory".into());
    }
    for (model, prior) in &a.prior_packets {
        let p = std::fs::canonicalize(prior)
            .map_err(|e| format!("--prior-packets {model}: {}: {e}", prior.display()))?;
        if p.starts_with(&packets) {
            return Err(format!(
                "--prior-packets {model}: {} is the packet directory or inside it; the \
                 earlier round's packets must come from another directory",
                prior.display()
            ));
        }
    }
    Ok(())
}

/// What a judge checks in `meeting`: the annotated sentences, or with `--facts` their atomic
/// facts, checked against those sentences.
fn judged_reference(
    facts: Option<&facts::FactsFile>,
    meeting: &str,
    reference: &ami::AbsSumm,
) -> Result<Value, String> {
    let sentences = judge::reference_packet(meeting, reference);
    match facts {
        Some(f) => f.reference_packet(meeting, &sentences),
        None => Ok(sentences),
    }
}

/// The annotated sentences of `meeting`, as a judge reference packet.
fn meeting_sentences(a: &Args, meeting: &str) -> Result<Value, String> {
    let path = a
        .annotations
        .join("abstractive")
        .join(format!("{meeting}.abssumm.xml"));
    let reference =
        ami::parse_abssumm(&read_latin1(&path)?).map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(judge::reference_packet(meeting, &reference))
}

/// The local model of `--split-facts` or `--judge-facts`: on the GPU only with `--gpu`, and
/// `LocalLlm::load` refuses a GPU request that lands on the CPU.
fn load_fact_model(a: &Args) -> Result<LocalLlm, String> {
    let path = a.model.as_deref().ok_or("needs --model")?;
    if !path.is_file() {
        return Err(format!(
            "model not found at {}; run `cargo xtask fetch-models --set summary`",
            path.display()
        ));
    }
    let options = LoadOptions {
        gpu: a.gpu,
        context_tokens: a.context,
        threads: a.threads.unwrap_or_else(earshot_llm_local::default_threads),
    };
    let (llm, load) = LocalLlm::load(path, &options).map_err(|e| e.to_string())?;
    eprintln!("sum_eval: {} on {}", a.model_id, load.backend.name());
    Ok(llm)
}

/// `--split-facts` (`facts.rs`).
fn split_facts(a: &Args, path: &Path) -> Result<(), String> {
    let mut meetings = Vec::new();
    for meeting in a.meetings.values().flatten() {
        meetings.push((meeting.clone(), meeting_sentences(a, meeting)?));
    }
    let mut llm = load_fact_model(a)?;
    facts::split_meetings(&mut llm, &a.model_id, &a.assistant_prefix, path, &meetings)
}

/// `--judge-facts` (`facts.rs`).
fn judge_facts(a: &Args, packets: &Path) -> Result<(), String> {
    let all: Vec<String> = a.meetings.values().flatten().cloned().collect();
    let mut llm = load_fact_model(a)?;
    let started = Instant::now();
    let judged =
        facts::judge_packets_locally(&mut llm, &a.assistant_prefix, packets, &a.out, &all)?;
    eprintln!(
        "sum_eval: {judged} facts judged in {:.0} s, verdicts in {}",
        started.elapsed().as_secs_f64(),
        a.out.display()
    );
    Ok(())
}

/// `--agreement` (`facts.rs`): writes the comparison to `--out` and prints its headline.
fn agreement(a: &Args, gold: &Path) -> Result<(), String> {
    let all: Vec<String> = a.meetings.values().flatten().cloned().collect();
    let against = a.against.as_deref().ok_or("--agreement needs --against")?;
    let r = facts::compare_dirs(gold, against, &all)?;
    std::fs::write(
        &a.out,
        serde_json::to_vec_pretty(&r).map_err(|e| e.to_string())?,
    )
    .map_err(|e| format!("{}: {e}", a.out.display()))?;
    println!(
        "{} of {} agree ({:.1}%), kappa {}",
        r["agree"],
        r["items"],
        r["percent"].as_f64().unwrap_or(0.0),
        r["kappa"]
    );
    println!("gold by judge: {}", r["confusion_gold_by_judge"]);
    println!("per verdict: {}", r["per_verdict"]);
    Ok(())
}

/// `--judge-packets` (`judge.rs`): `reference.json` for every meeting of `--meetings`, a
/// packet per `--summaries` result file (the dev reference-source job), and with
/// `--judge-replies` a packet of model `--label` from exported-prompt replies (validated by
/// `claude::import_one`, as `--import-replies` does).
fn judge_packets(a: &Args, dir: &Path) -> Result<(), String> {
    check_judge_sources(a, dir)?;
    let facts = a.facts.as_deref().map(facts::FactsFile::read).transpose()?;
    let mut results = Vec::new();
    for (model, file) in &a.summaries {
        let text = std::fs::read_to_string(file).map_err(|e| format!("{}: {e}", file.display()))?;
        let v: Value =
            serde_json::from_str(&text).map_err(|e| format!("{}: {e}", file.display()))?;
        results.push((model.clone(), v));
    }
    let mut key = json!({});
    let mut written = 0;
    for (split, list) in &a.meetings {
        for meeting in list {
            let summ_path = a
                .annotations
                .join("abstractive")
                .join(format!("{meeting}.abssumm.xml"));
            let reference = ami::parse_abssumm(&read_latin1(&summ_path)?)
                .map_err(|e| format!("{}: {e}", summ_path.display()))?;
            judge::write(
                dir,
                meeting,
                "reference.json",
                &judged_reference(facts.as_ref(), meeting, &reference)?,
            )?;
            let mut packets: Vec<(String, Value)> = Vec::new();
            for (model, v) in &results {
                let row = v["jobs"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter(|j| j["split"] == split.as_str() && j["source"] == "reference")
                    .flat_map(|j| j["meetings"].as_array().into_iter().flatten())
                    .find(|m| m["meeting"] == meeting.as_str())
                    .ok_or_else(|| format!("{model}: no {split} reference row for {meeting}"))?;
                let summary: Option<SummaryV1> = serde_json::from_value(row["summary"].clone())
                    .map_err(|e| format!("{model} {meeting}: {e}"))?;
                let packet = judge::summary_packet(
                    meeting,
                    model,
                    summary.as_ref(),
                    row["failure"].as_str(),
                );
                packets.push((model.clone(), packet));
            }
            for (model, prior) in &a.prior_packets {
                let key = a
                    .prior_key
                    .as_deref()
                    .ok_or("--prior-packets needs --prior-key")?;
                packets.push((
                    model.clone(),
                    judge::prior_packet(prior, key, meeting, model, &reference)?,
                ));
            }
            if let Some(replies) = &a.judge_replies {
                let (input, _) = reference_input(&a.annotations, meeting)?;
                let result = claude::import_one(replies, meeting, "reference", &input);
                let packet = judge::summary_packet(
                    meeting,
                    &a.model_id,
                    result.as_ref().ok().map(|c| &c.summary),
                    result.as_ref().err().map(String::as_str),
                );
                packets.push((a.model_id.clone(), packet));
            }
            if a.blind_key.is_some() {
                for (label, model, packet) in judge::blinded_in_label_order(meeting, packets) {
                    judge::write(dir, meeting, &format!("summary.{label}.json"), &packet)?;
                    key[meeting.as_str()][label.as_str()] = json!(model);
                    written += 1;
                }
            } else {
                for (model, packet) in packets {
                    judge::write(dir, meeting, &format!("summary.{model}.json"), &packet)?;
                    written += 1;
                }
            }
        }
    }
    // Ties the key to these packets (checked by `--import-verdicts`).
    let all: Vec<String> = a.meetings.values().flatten().cloned().collect();
    let fingerprint = judge::packet_fingerprint(dir, &all)?;
    judge::write_packet_set(dir, &fingerprint)?;
    key["packet_set"] = json!(fingerprint);
    if let Some(path) = &a.blind_key {
        std::fs::write(
            path,
            serde_json::to_vec_pretty(&key).map_err(|e| e.to_string())?,
        )
        .map_err(|e| format!("{}: {e}", path.display()))?;
        eprintln!("sum_eval: blinding key in {}", path.display());
    }
    eprintln!(
        "sum_eval: wrote {written} summary packets to {}",
        dir.display()
    );
    Ok(())
}

/// `--import-verdicts` (`judge.rs`): for every meeting of `--meetings`, the packet
/// directory's `reference.json` (which must equal the packet rebuilt from the annotations)
/// and one `verdict.<model>.json` for every `summary.<model>.json` there (a verdict without
/// a summary packet fails); every meeting must hold the same models, and the packets must
/// be the set the blinding key and the packet directory's fingerprint were written for
/// (`judge::check_packet_set`). Writes per model and per meeting covered / contradicted / missing
/// counts to `--out` (JSON) and a Markdown table beside it (`.md`).
fn import_verdicts(a: &Args, dir: &Path) -> Result<(), String> {
    let facts = a.facts.as_deref().map(facts::FactsFile::read).transpose()?;
    let key: Option<Value> = match &a.blind_key {
        Some(p) => Some(
            serde_json::from_str(
                &std::fs::read_to_string(p).map_err(|e| format!("{}: {e}", p.display()))?,
            )
            .map_err(|e| format!("{}: {e}", p.display()))?,
        ),
        None => None,
    };
    let all: Vec<String> = a.meetings.values().flatten().cloned().collect();
    let legacy = judge::check_packet_set(key.as_ref(), dir, &all)?;
    let fingerprint = match &legacy {
        Some(note) => {
            eprintln!("sum_eval: {note}");
            None
        }
        None => Some(judge::packet_fingerprint(dir, &all)?),
    };
    let mut models: Option<Vec<String>> = None;
    let mut totals: BTreeMap<String, Value> = BTreeMap::new();
    let mut per_meeting = Vec::new();
    for list in a.meetings.values() {
        for meeting in list {
            let mdir = dir.join(meeting);
            let summ_path = a
                .annotations
                .join("abstractive")
                .join(format!("{meeting}.abssumm.xml"));
            let reference = judged_reference(
                facts.as_ref(),
                meeting,
                &ami::parse_abssumm(&read_latin1(&summ_path)?)
                    .map_err(|e| format!("{}: {e}", summ_path.display()))?,
            )?;
            let read = |p: &Path| -> Result<Value, String> {
                let text =
                    std::fs::read_to_string(p).map_err(|e| format!("{}: {e}", p.display()))?;
                serde_json::from_str(&text).map_err(|e| format!("{}: {e}", p.display()))
            };
            if read(&mdir.join("reference.json"))? != reference {
                return Err(format!(
                    "{meeting}: reference.json differs from the annotations; write the packets again"
                ));
            }
            let here = judge::packet_labels(&mdir)?;
            match &models {
                None => models = Some(here.clone()),
                Some(m) if *m != here => {
                    return Err(format!("{meeting}: models {here:?}, other meetings {m:?}"));
                }
                Some(_) => {}
            }
            for label in &here {
                let v = read(&mdir.join(format!("verdict.{label}.json")))?;
                let counts = judge::check_verdict(&reference, &v, meeting, label)?;
                // Blinded packets: the label is `S<n>`, the key names the model.
                let model = match &key {
                    Some(k) => k[meeting.as_str()][label.as_str()]
                        .as_str()
                        .ok_or_else(|| format!("{meeting}: {label} is not in the blinding key"))?
                        .to_owned(),
                    None => label.clone(),
                };
                judge::add_counts(
                    totals.entry(model.clone()).or_insert_with(|| json!({})),
                    &counts,
                );
                per_meeting.push(json!({
                    "meeting": meeting, "label": label, "model": model, "counts": counts,
                }));
            }
        }
    }
    let mut table = String::from(
        "| model | decisions covered | contradicted | missing | actions covered | contradicted | missing |\n|---|---|---|---|---|---|---|\n",
    );
    for (model, c) in &totals {
        let d = &c["decisions"];
        let ac = &c["action_items"];
        table.push_str(&format!(
            "| {model} | {} | {} | {} | {} | {} | {} |\n",
            d["covered"],
            d["contradicted"],
            d["missing"],
            ac["covered"],
            ac["contradicted"],
            ac["missing"]
        ));
    }
    if facts.is_some() {
        let rows: Vec<(String, Value)> =
            totals.iter().map(|(m, c)| (m.clone(), c.clone())).collect();
        table.push('\n');
        table.push_str(&facts::rates_table(&rows));
    }
    let mut out = json!({
        "verdicts": dir.display().to_string(),
        "meetings": a.meetings,
        "models": totals,
        "per_meeting": per_meeting,
    });
    record_packet_set(&mut out, &mut table, fingerprint, legacy);
    std::fs::write(
        &a.out,
        serde_json::to_vec_pretty(&out).map_err(|e| e.to_string())?,
    )
    .map_err(|e| format!("{}: {e}", a.out.display()))?;
    let md = a.out.with_extension("md");
    std::fs::write(&md, &table).map_err(|e| format!("{}: {e}", md.display()))?;
    print!("{table}");
    eprintln!("sum_eval: wrote {} and {}", a.out.display(), md.display());
    Ok(())
}

/// Records in a verdict report (`out` JSON, `table` Markdown) which packet set its verdicts
/// were checked against: the verified `fingerprint`, or, for a round written before
/// fingerprints, `"packet_set": null` with the `note` in both outputs.
fn record_packet_set(
    out: &mut Value,
    table: &mut String,
    fingerprint: Option<String>,
    note: Option<String>,
) {
    out["packet_set"] = json!(fingerprint);
    if let Some(note) = note {
        table.push_str(&format!("\nNote: {note}\n"));
        out["packet_set_note"] = json!(note);
    }
}

/// `--export-prompts` (`claude.rs`).
fn export_prompts(a: &Args, dir: &Path) -> Result<(), String> {
    std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let mut entries = Vec::new();
    for (split, source) in &a.jobs {
        for meeting in split_meetings(a, split)? {
            let (input, _) = load_input(a, source, meeting)?;
            let e = claude::export_one(dir, meeting, source, &input)?;
            if e["single"] != true {
                eprintln!("sum_eval: {meeting} {source}: map-reduce, only map steps exported");
            }
            entries.push(e);
        }
    }
    let n = entries.len();
    claude::export_finish(dir, entries)?;
    eprintln!("sum_eval: exported {n} requests to {}", dir.display());
    Ok(())
}

/// `--import-replies` (`claude.rs`): the agents' replies scored as candidate `--label`.
fn import_replies(a: &Args, dir: &Path) -> Result<(), String> {
    let mut jobs = Vec::new();
    for (split, source) in &a.jobs {
        let mut rows = Vec::new();
        for meeting in split_meetings(a, split)? {
            let summ_path = a
                .annotations
                .join("abstractive")
                .join(format!("{meeting}.abssumm.xml"));
            let reference = ami::parse_abssumm(&read_latin1(&summ_path)?)
                .map_err(|e| format!("{}: {e}", summ_path.display()))?;
            let (input, words) = load_input(a, source, meeting)?;
            let result = claude::import_one(dir, meeting, source, &input);
            let summary = result.as_ref().ok().map(|c| &c.summary);
            let s = score::score(summary, &reference);
            eprintln!(
                "sum_eval: {} {split}:{source} {meeting}: {}",
                a.model_id,
                match &result {
                    Ok(_) => "ok".to_owned(),
                    Err(e) => format!("FAILED ({e})"),
                }
            );
            rows.push(json!({
                "meeting": meeting,
                "ok": result.is_ok(),
                "failure": result.as_ref().err(),
                "seconds": 0.0,
                "input_segments": input.segments.len(),
                "input_words": words,
                "report": {
                    "requests": 1,
                    "duplicates_dropped": result.as_ref().map_or(0, |c| c.duplicates_dropped),
                },
                "score": s.to_json(),
                "summary": summary,
            }));
        }
        jobs.push(json!({
            "split": split,
            "source": source,
            "aggregate": aggregate(&rows),
            "meetings": rows,
        }));
    }
    let out = json!({
        "candidate": a.model_id,
        "kind": "claude-code-agent",
        "backend": "claude-code-agent",
        "note": "Replies written by Claude Code agents following the exported prompts \
                 (`sum_eval --export-prompts`), not a direct API call; no timing or token \
                 counts. Validated and scored exactly as the local candidates.",
        "replies": dir.display().to_string(),
        "context_tokens": earshot_llm::ANTHROPIC_DEFAULT_BUDGET.context_window,
        "max_output_tokens": earshot_llm::ANTHROPIC_DEFAULT_BUDGET.max_output_tokens,
        "grammar": GRAMMAR,
        "jobs": jobs,
    });
    std::fs::write(
        &a.out,
        serde_json::to_vec_pretty(&out).map_err(|e| e.to_string())?,
    )
    .map_err(|e| format!("{}: {e}", a.out.display()))?;
    eprintln!("sum_eval: wrote {}", a.out.display());
    Ok(())
}

fn run() -> Result<(), String> {
    let a = parse_args()?;
    if !a.rescore.is_empty() {
        for f in &a.rescore {
            rescore_file(&a.annotations, f)?;
        }
        return Ok(());
    }
    if let Some(path) = &a.split_facts {
        return split_facts(&a, path);
    }
    if let Some(dir) = &a.judge_facts {
        return judge_facts(&a, dir);
    }
    if let Some(gold) = &a.agreement {
        return agreement(&a, gold);
    }
    if let Some(dir) = &a.import_verdicts {
        return import_verdicts(&a, dir);
    }
    if let Some(dir) = &a.judge_packets {
        return judge_packets(&a, dir);
    }
    if let Some(dir) = &a.export_prompts {
        return export_prompts(&a, dir);
    }
    if let Some(dir) = &a.import_replies {
        return import_replies(&a, dir);
    }
    let (mut engine, header) = if let Some(path) = &a.model {
        if !path.is_file() {
            return Err(format!(
                "model not found at {}; run `cargo xtask fetch-models --set llm-eval`",
                path.display()
            ));
        }
        let options = LoadOptions {
            gpu: a.gpu,
            context_tokens: a.context,
            threads: a.threads.unwrap_or_else(earshot_llm_local::default_threads),
        };
        let (llm, load) = LocalLlm::load(path, &options).map_err(|e| e.to_string())?;
        eprintln!(
            "sum_eval: {} on {} (load {:.1} s): {}",
            a.model_id,
            load.backend.name(),
            load.load_time.as_secs_f64(),
            load.description
        );
        let header = json!({
            "candidate": a.model_id,
            "kind": "local",
            "model_file": path.file_name().map(|f| f.to_string_lossy().into_owned()),
            "backend": load.backend.name(),
            "load_evidence": load.evidence,
            "description": load.description,
            "load_s": load.load_time.as_secs_f64(),
            "context_tokens": load.context_tokens,
            "threads": options.threads,
            "max_output_tokens": a.max_output,
            "assistant_prefix": a.assistant_prefix,
            "grammar": format_key(a.chunk_seconds, a.prompt_variant, a.two_call, a.two_track, None),
            "chunk_seconds": a.chunk_seconds,
            "two_call": a.two_call,
            "two_track": a.two_track,
            "prompt_variant": a.prompt_variant.name(),
            "system_info": earshot_llm_local::system_info(),
        });
        let engine = Engine::Local {
            llm: Box::new(llm),
            options: SummaryOptions {
                max_output_tokens: a.max_output,
                assistant_prefix: a.assistant_prefix.clone(),
                limits: SummaryLimits::default(),
                cancel: CancelToken::new(),
            },
            chunk_seconds: a.chunk_seconds,
            two_call: a.two_call,
        };
        (engine, header)
    } else if let Some(agent) = a.agent_cli() {
        let (c, name) = match agent {
            AgentCli::ClaudeCode => (
                claude_cli::AgentCliRun::claude_code(a.claude_model.clone(), a.chunk_seconds)?,
                "Claude Code",
            ),
            AgentCli::Codex => (
                claude_cli::AgentCliRun::codex(
                    a.codex_model.clone(),
                    a.codex_cli.clone(),
                    a.chunk_seconds,
                )?,
                "Codex",
            ),
        };
        let mut header = c.header();
        header["candidate"] = json!(a.model_id);
        header["grammar"] = json!(format_key(
            a.chunk_seconds,
            a.prompt_variant,
            false,
            false,
            Some(agent)
        ));
        header["chunk_seconds"] = json!(a.chunk_seconds);
        header["prompt_variant"] = json!(a.prompt_variant.name());
        eprintln!(
            "sum_eval: {} through {name} {} (model {})",
            a.model_id,
            header["cli_version"]
                .as_str()
                .unwrap_or("of unknown version"),
            header["requested_model"].as_str().unwrap_or("?"),
        );
        (Engine::AgentCli(Box::new(c)), header)
    } else {
        let r = remote::Remote::from_env(a.context as usize, a.max_output)?;
        let header = json!({
            "candidate": a.model_id,
            "kind": "remote (informational)",
            "endpoint_host": r.host(),
            "model": r.model(),
            "context_tokens": a.context,
            "max_output_tokens": a.max_output,
        });
        (Engine::Remote(Box::new(r)), header)
    };

    // Checkpoint: every finished meeting is appended to `<out>.partial.jsonl` with the
    // settings it ran with; a run that was stopped resumes from it (greedy decoding gives
    // the same summary again). The file is removed once the report is written.
    let key = format!(
        "{}|{}|{}|{}|{}|{}",
        a.model_id,
        a.context,
        a.max_output,
        a.assistant_prefix,
        out_key(&header),
        format_key(
            a.chunk_seconds,
            a.prompt_variant,
            a.two_call,
            a.two_track,
            a.agent_cli()
        )
    );
    let partial = a.out.with_extension("partial.jsonl");
    let mut done: BTreeMap<(String, String, String), Value> = BTreeMap::new();
    if let Ok(text) = std::fs::read_to_string(&partial) {
        for line in text.lines() {
            if let Ok(v) = serde_json::from_str::<Value>(line)
                && v["key"] == key.as_str()
            {
                let k = (
                    v["split"].as_str().unwrap_or_default().to_owned(),
                    v["source"].as_str().unwrap_or_default().to_owned(),
                    v["row"]["meeting"].as_str().unwrap_or_default().to_owned(),
                );
                done.insert(k, v["row"].clone());
            }
        }
    }
    let mut checkpoint = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&partial)
        .map_err(|e| format!("{}: {e}", partial.display()))?;
    let mut jobs = Vec::new();
    for (split, source) in &a.jobs {
        let list = a
            .meetings
            .get(split)
            .ok_or_else(|| format!("no --meetings for split {split}"))?;
        let mut rows = Vec::new();
        for meeting in list {
            let summ_path = a
                .annotations
                .join("abstractive")
                .join(format!("{meeting}.abssumm.xml"));
            let reference = ami::parse_abssumm(&read_latin1(&summ_path)?)
                .map_err(|e| format!("{}: {e}", summ_path.display()))?;
            if let Some(row) = done.get(&(split.clone(), source.clone(), meeting.clone())) {
                // The summary is stored; the score is recomputed with this build's rules
                // (rows written before a scoring rule was added gain it).
                let mut row = row.clone();
                let summary: Option<SummaryV1> = serde_json::from_value(row["summary"].clone())
                    .map_err(|e| format!("checkpoint row for {meeting}: {e}"))?;
                row["score"] = score::score(summary.as_ref(), &reference).to_json();
                eprintln!(
                    "sum_eval: {} {split}:{source} {meeting}: from the checkpoint (re-scored)",
                    a.model_id
                );
                rows.push(row);
                continue;
            }
            let (mut input, words, ends) = match source.as_str() {
                "reference" => reference_input_timed(&a.annotations, meeting)?,
                _ => {
                    let (input, words) = earshot_input(
                        a.earshot_inputs
                            .as_deref()
                            .ok_or("--earshot-inputs missing")?,
                        meeting,
                    )?;
                    (input, words, Vec::new())
                }
            };
            input.prompt_variant = a.prompt_variant;
            let started = Instant::now();
            let (result, report) = if a.two_track {
                engine.summarise_two_track(&input, &ends)
            } else {
                engine.summarise(&input)
            };
            let seconds = started.elapsed().as_secs_f64();
            if a.agent_cli().is_some() {
                eprintln!(
                    "sum_eval: {} {split}:{source} {meeting}: plan {}, {} requests ({} repairs), \
                     {} characters handed over, at hang-up {:.1} s",
                    a.model_id,
                    report["plan"].as_str().unwrap_or("?"),
                    report["requests"],
                    report["repairs"],
                    report["characters_handed_over"],
                    report["timing"]["hangup_s"].as_f64().unwrap_or(0.0),
                );
            }
            let s = score::score(result.as_ref().ok(), &reference);
            eprintln!(
                "sum_eval: {} {split}:{source} {meeting}: {} in {seconds:.1} s, rougeL {:.3}, decisions {}/{}, actions {}/{}",
                a.model_id,
                if result.is_ok() { "ok" } else { "FAILED" },
                s.rouge.rouge_l.fmeasure,
                s.decisions.caught,
                s.decisions.reference,
                s.actions.caught,
                s.actions.reference,
            );
            let row = json!({
                "meeting": meeting,
                "ok": result.is_ok(),
                "failure": result.as_ref().err(),
                "seconds": seconds,
                "duration_s": input.meeting.duration_ms as f64 / 1000.0,
                "input_segments": input.segments.len(),
                "input_words": words,
                "report": report,
                "score": s.to_json(),
                "summary": result.as_ref().ok(),
            });
            let line = json!({"key": key, "split": split, "source": source, "row": row});
            writeln!(checkpoint, "{line}").map_err(|e| format!("{}: {e}", partial.display()))?;
            checkpoint.flush().map_err(|e| e.to_string())?;
            rows.push(row);
        }
        jobs.push(json!({
            "split": split,
            "source": source,
            "aggregate": aggregate(&rows),
            "meetings": rows,
        }));
    }
    let mut out = header;
    out["jobs"] = Value::Array(jobs);
    std::fs::write(
        &a.out,
        serde_json::to_vec_pretty(&out).map_err(|e| e.to_string())?,
    )
    .map_err(|e| format!("{}: {e}", a.out.display()))?;
    drop(checkpoint);
    let _ = std::fs::remove_file(&partial);
    eprintln!("sum_eval: wrote {}", a.out.display());
    Ok(())
}

/// The part of the header that identifies what ran (model file or endpoint and model).
fn out_key(header: &Value) -> String {
    format!(
        "{}|{}|{}",
        header["model_file"].as_str().unwrap_or_default(),
        header["endpoint_host"].as_str().unwrap_or_default(),
        header["model"].as_str().unwrap_or_default()
    )
}

fn main() {
    if let Err(e) = run() {
        eprintln!("sum_eval: {e}");
        std::process::exit(1);
    }
}

/// The remote ceiling (informational): `earshot_llm` over `earshot-net`, allowlisting only
/// `EARSHOT_LLM_URL`.
mod remote {
    use std::sync::Arc;

    use earshot_core::asr::CancelToken;
    use earshot_core::prompt::{ContextBudget, SummaryInput};
    use earshot_core::summary::SummaryV1;
    use earshot_llm::{ChatClient, LlmConfig, LlmSummarizer};
    use earshot_net::{Allowlist, AuditLog, NetClient, NetConfig, SystemClock};
    use serde_json::{Value, json};

    pub struct Remote {
        runtime: tokio::runtime::Runtime,
        summarizer: LlmSummarizer,
        audit: AuditLog,
        host: String,
        model: String,
    }

    impl Remote {
        /// From `EARSHOT_LLM_URL` and `EARSHOT_LLM_MODEL`.
        pub fn from_env(context: usize, max_output: usize) -> Result<Self, String> {
            let url = std::env::var("EARSHOT_LLM_URL")
                .map_err(|_| "--remote needs EARSHOT_LLM_URL".to_string())?;
            let model = std::env::var("EARSHOT_LLM_MODEL")
                .map_err(|_| "--remote needs EARSHOT_LLM_MODEL".to_string())?;
            let mut config = LlmConfig::new(&url, model.clone()).map_err(|e| e.to_string())?;
            config.budget = ContextBudget {
                context_window: context,
                max_output_tokens: max_output,
            };
            let mut allow = Allowlist::new();
            allow
                .allow_url(&config.endpoint)
                .map_err(|e| format!("{e:?}"))?;
            allow
                .check(&config.chat_url())
                .map_err(|e| format!("{e:?}"))?;
            let host = config.endpoint.host_str().unwrap_or_default().to_owned();
            let audit = AuditLog::new();
            let net =
                NetClient::new(NetConfig::new(allow, Arc::new(SystemClock)).audit(audit.clone()))
                    .map_err(|e| e.to_string())?;
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|e| e.to_string())?;
            Ok(Remote {
                runtime,
                summarizer: LlmSummarizer::new(ChatClient::new(net, config)),
                audit,
                host,
                model,
            })
        }

        pub fn host(&self) -> &str {
            &self.host
        }

        pub fn model(&self) -> &str {
            &self.model
        }

        pub fn summarise(&mut self, input: &SummaryInput) -> (Result<SummaryV1, String>, Value) {
            let before = self.audit.len();
            let run = self.runtime.block_on(
                self.summarizer
                    .summarize_with_report(input, &CancelToken::new()),
            );
            let entries = self.audit.entries();
            let hosts: Vec<String> = entries[before..].iter().map(|e| e.host.clone()).collect();
            let r = &run.report;
            (
                run.result.map_err(|e| e.to_string()),
                json!({
                    "requests": r.requests,
                    "repairs": r.repairs,
                    "map_parts": r.map_parts,
                    "reduce_steps": r.reduce_steps,
                    "duplicates_dropped": r.duplicates_dropped,
                    "audit_hosts": hosts,
                }),
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sum_eval_format_key_names_chunking_and_the_prompt_variant() {
        let current = PromptVariant::Current;
        assert_eq!(
            format_key(None, current, false, false, None),
            "indented-numbered-r5"
        );
        assert_eq!(
            format_key(Some(600), current, false, false, None),
            "indented-numbered-r5-chunk600"
        );
        assert_eq!(
            format_key(Some(600), PromptVariant::Lean, false, false, None),
            "indented-numbered-r5-chunk600-prompt-lean"
        );
        assert_eq!(
            format_key(None, PromptVariant::Owners, false, false, None),
            "indented-numbered-r5-prompt-owners"
        );
        assert_eq!(
            format_key(Some(600), current, true, false, None),
            "indented-numbered-r5-chunk600-twocall"
        );
        assert_eq!(
            format_key(Some(600), PromptVariant::Lean, true, false, None),
            "indented-numbered-r5-chunk600-twocall-prompt-lean"
        );
        assert_eq!(
            format_key(Some(600), current, false, true, None),
            "indented-numbered-r5-chunk600-twotrack"
        );
        // Every variant and call mode has its own key, and xtask builds the same strings.
        let keys: std::collections::BTreeSet<String> = PromptVariant::ALL
            .into_iter()
            .flat_map(|v| {
                [
                    format_key(Some(600), v, false, false, None),
                    format_key(Some(600), v, true, false, None),
                    format_key(Some(600), v, false, true, None),
                ]
            })
            .collect();
        assert_eq!(keys.len(), 3 * PromptVariant::ALL.len());
        // The Claude Code candidate (T7.4gb), after the meeting and chunked.
        assert_eq!(
            format_key(None, current, false, false, Some(AgentCli::ClaudeCode)),
            "indented-numbered-r5-claude-code"
        );
        assert_eq!(
            format_key(Some(600), current, false, false, Some(AgentCli::ClaudeCode)),
            "indented-numbered-r5-chunk600-claude-code"
        );
    }

    fn claude_args(extra: &[&str]) -> Args {
        let mut a = Args {
            jobs: vec![("dev".into(), "reference".into())],
            out: "o.json".into(),
            model_id: "claude-code".into(),
            claude_code: true,
            ..Args::default()
        };
        for e in extra {
            match *e {
                "earshot" => a.jobs.push(("test".into(), "earshot".into())),
                "remote" => a.remote = true,
                "two-call" => a.two_call = true,
                "variant" => a.prompt_variant = PromptVariant::Lean,
                "chunk" => a.chunk_seconds = Some(600),
                "bad-model" => a.claude_model = Some("--tools".into()),
                "model" => a.claude_model = Some("opus".into()),
                "rescore" => a.rescore.push("r.json".into()),
                _ => unreachable!(),
            }
        }
        a
    }

    #[test]
    fn sum_eval_claude_code_takes_only_reference_transcripts_and_the_production_prompt() {
        check_claude_code(&claude_args(&[])).unwrap();
        // Both conditions: after the meeting and chunks during it.
        check_claude_code(&claude_args(&["chunk"])).unwrap();
        check_claude_code(&claude_args(&["model"])).unwrap();
        let refused = |extra: &[&str], needle: &str| {
            let e = check_claude_code(&claude_args(extra)).unwrap_err();
            assert!(e.contains(needle), "{extra:?}: {e}");
        };
        refused(&["earshot"], "reference transcripts");
        refused(&["remote"], "exactly one");
        refused(&["two-call"], "production prompt");
        refused(&["variant"], "production prompt");
        refused(&["rescore"], "summary run");
        refused(&["bad-model"], "--claude-model");
        let mut with_inputs = claude_args(&[]);
        with_inputs.earshot_inputs = Some("e".into());
        assert!(check_claude_code(&with_inputs).is_err());
    }

    #[test]
    fn sum_eval_codex_has_its_key_and_the_claude_code_rules() {
        let current = PromptVariant::Current;
        assert_eq!(
            format_key(None, current, false, false, Some(AgentCli::Codex)),
            "indented-numbered-r5-codex"
        );
        assert_eq!(
            format_key(Some(600), current, false, false, Some(AgentCli::Codex)),
            "indented-numbered-r5-chunk600-codex"
        );
        let codex = |extra: &[&str]| {
            let mut a = claude_args(extra);
            a.claude_code = false;
            a.codex = true;
            a.model_id = "codex".into();
            if extra.contains(&"model") {
                a.claude_model = None;
                a.codex_model = Some("gpt-5.6-luna".into());
            }
            if extra.contains(&"bad-model") {
                a.claude_model = None;
                a.codex_model = Some("-s".into());
            }
            a
        };
        assert_eq!(codex(&[]).agent_cli(), Some(AgentCli::Codex));
        check_agent_cli(&codex(&[]), AgentCli::Codex).unwrap();
        check_agent_cli(&codex(&["chunk"]), AgentCli::Codex).unwrap();
        check_agent_cli(&codex(&["model"]), AgentCli::Codex).unwrap();
        let mut with_cli = codex(&[]);
        with_cli.codex_cli = Some("C:\\x\\codex.exe".into());
        check_agent_cli(&with_cli, AgentCli::Codex).unwrap();
        let refused = |extra: &[&str], needle: &str| {
            let e = check_agent_cli(&codex(extra), AgentCli::Codex).unwrap_err();
            assert!(e.contains(needle), "{extra:?}: {e}");
        };
        refused(
            &["earshot"],
            "--codex summarises only the public AMI reference transcripts",
        );
        refused(&["remote"], "exactly one");
        refused(&["two-call"], "production prompt");
        refused(&["variant"], "production prompt");
        refused(&["rescore"], "summary run");
        refused(&["bad-model"], "--codex-model");
        let mut both = codex(&[]);
        both.claude_code = true;
        assert!(check_agent_cli(&both, AgentCli::Codex).is_err());
    }

    #[test]
    fn sum_eval_two_call_timing_counts_both_map_requests_of_the_last_chunk() {
        let st = |step: &'static str, s: u64| StepTime {
            step,
            wall: std::time::Duration::from_secs(s),
            requests: 1,
        };
        let two = [
            st("map-actions", 1),
            st("map-rest", 2),
            st("map-actions", 3),
            st("map-rest", 4),
            st("reduce", 10),
        ];
        let t = step_timing(&two, true);
        assert_eq!(t["map_steps"], 4);
        assert_eq!(t["map_s"], 10.0);
        assert_eq!(t["last_map_s"], 7.0);
        assert_eq!(t["hangup_s"], 17.0);
        // One request per chunk: unchanged.
        let one = [st("map", 1), st("map", 2), st("reduce", 5)];
        let t = step_timing(&one, true);
        assert_eq!(
            (t["map_steps"].as_u64(), t["last_map_s"].as_f64()),
            (Some(2), Some(2.0))
        );
        assert_eq!(t["hangup_s"], 7.0);
    }

    #[test]
    fn sum_eval_two_call_last_map_is_one_chunk_after_a_failure() {
        let st = |step: &'static str, s: u64| StepTime {
            step,
            wall: std::time::Duration::from_secs(s),
            requests: 1,
        };
        // The last chunk's action request failed, so it has no rest step: the last map is
        // that request alone, never mixed with the chunk before's rest step.
        let failed = [
            st("map-actions", 1),
            st("map-rest", 2),
            st("map-actions", 4),
        ];
        let t = step_timing(&failed, true);
        assert_eq!(t["last_map_s"], 4.0);
        assert_eq!(t["hangup_s"], 4.0);
        // A rest step pairs only with the action step right before it.
        let rest_only = [st("map-rest", 2)];
        assert_eq!(step_timing(&rest_only, true)["last_map_s"], 2.0);
    }

    #[test]
    fn sum_eval_unchunked_map_reduce_timing_waits_for_every_map_step() {
        let st = |step: &'static str, s: u64| StepTime {
            step,
            wall: std::time::Duration::from_secs(s),
            requests: 1,
        };
        // An unchunked run the planner split into map-reduce: every request runs after the
        // meeting ends, so the wait is the whole run.
        let planned = [st("map", 1), st("map", 2), st("reduce", 5)];
        let t = step_timing(&planned, false);
        assert_eq!(t["hangup_s"], 8.0);
        assert_eq!(t["last_map_s"], 2.0);
        assert_eq!(t["map_s"], 3.0);
        // A single request is the whole run either way.
        let single = [st("single", 4)];
        assert_eq!(step_timing(&single, false)["hangup_s"], 4.0);
        assert_eq!(step_timing(&single, true)["hangup_s"], 4.0);
    }

    /// A fresh directory under the temp dir for one test.
    fn temp_root(name: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("sum_eval_{name}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        root
    }

    #[test]
    fn sum_eval_judge_packets_refuse_a_model_name_given_twice() {
        let root = temp_root("judge_names");
        let dir = root.join("packets");
        let summaries = |names: &[&str]| -> Vec<(String, PathBuf)> {
            names
                .iter()
                .map(|n| (n.to_string(), root.join(format!("{n}.json"))))
                .collect()
        };
        let ok = Args {
            summaries: summaries(&["a", "b"]),
            ..Args::default()
        };
        check_judge_sources(&ok, &dir).unwrap();
        let twice = Args {
            summaries: summaries(&["a", "b", "a"]),
            ..Args::default()
        };
        assert!(check_judge_sources(&twice, &dir).unwrap_err().contains("a"));
        // --label names the --judge-replies packet: it must differ from every summary.
        let label = Args {
            summaries: summaries(&["a", "b"]),
            judge_replies: Some(root.join("replies")),
            model_id: "b".into(),
            ..Args::default()
        };
        assert!(check_judge_sources(&label, &dir).is_err());
        let label_ok = Args {
            model_id: "c".into(),
            ..label
        };
        check_judge_sources(&label_ok, &dir).unwrap();
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn sum_eval_judge_packets_refuse_a_blind_key_inside_the_packets_however_spelled() {
        let root = temp_root("judge_key");
        let dir = root.join("packets");
        std::fs::create_dir_all(root.join("other")).unwrap();
        let with_key = |k: PathBuf| Args {
            blind_key: Some(k),
            ..Args::default()
        };
        check_judge_sources(&with_key(root.join("key.json")), &dir).unwrap();
        assert!(check_judge_sources(&with_key(dir.join("key.json")), &dir).is_err());
        // Lexically outside, really inside.
        let spelled = root
            .join("other")
            .join("..")
            .join("packets")
            .join("key.json");
        assert!(!spelled.starts_with(&dir));
        assert!(check_judge_sources(&with_key(spelled), &dir).is_err());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn sum_eval_judge_packets_refuse_prior_packets_that_repeat_a_model_or_the_directory() {
        let root = temp_root("judge_prior");
        let dir = root.join("packets");
        let prior = root.join("prior");
        std::fs::create_dir_all(&prior).unwrap();
        std::fs::create_dir_all(dir.join("ES2003c")).unwrap();
        std::fs::create_dir_all(root.join("other")).unwrap();
        let args = |priors: &[(&str, PathBuf)]| Args {
            summaries: vec![("local".into(), root.join("local.json"))],
            prior_packets: priors
                .iter()
                .map(|(m, p)| (m.to_string(), p.clone()))
                .collect(),
            prior_key: Some(root.join("prior-key.json")),
            ..Args::default()
        };
        check_judge_sources(&args(&[("claude", prior.clone())]), &dir).unwrap();
        // A prior model named like a --summaries model, or carried twice.
        assert!(check_judge_sources(&args(&[("local", prior.clone())]), &dir).is_err());
        assert!(
            check_judge_sources(
                &args(&[("claude", prior.clone()), ("claude", prior.clone())]),
                &dir
            )
            .is_err()
        );
        // The packet directory itself under another spelling, or a directory inside it:
        // writing the packets would overwrite the earlier round's.
        let spelled = root.join("other").join("..").join("packets");
        assert_ne!(spelled, dir);
        assert!(check_judge_sources(&args(&[("claude", spelled)]), &dir).is_err());
        assert!(check_judge_sources(&args(&[("claude", dir.join("ES2003c"))]), &dir).is_err());
        // A prior directory that does not exist fails closed.
        assert!(check_judge_sources(&args(&[("claude", root.join("missing"))]), &dir).is_err());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn sum_eval_verdict_report_records_the_packet_set_or_why_there_is_none() {
        let table = "| model |\n|---|\n";
        // A fingerprinted round: the verified fingerprint, no note.
        let mut out = json!({"models": {}});
        let mut md = table.to_string();
        record_packet_set(&mut out, &mut md, Some("00ff00ff00ff00ff".into()), None);
        assert_eq!(out["packet_set"], "00ff00ff00ff00ff");
        assert!(out["packet_set_note"].is_null());
        assert_eq!(md, table);
        // A round written before fingerprints: null plus the note, in the JSON and the table.
        let note = "packets: no packet-set fingerprint (packets written before fingerprints)";
        let mut out = json!({"models": {}});
        let mut md = table.to_string();
        record_packet_set(&mut out, &mut md, None, Some(note.into()));
        assert!(out.as_object().unwrap().contains_key("packet_set"));
        assert!(out["packet_set"].is_null());
        assert_eq!(out["packet_set_note"], note);
        assert!(md.starts_with(table) && md.contains(note), "{md}");
    }

    #[test]
    fn sum_eval_aggregate_sums_two_call_action_counts_only_for_two_call_rows() {
        let row = |extracted: u64, fin: Option<u64>| {
            json!({"ok": fin.is_some(), "report": {"actions": {
                "extracted": extracted, "exact_repeats": 1, "to_merge": extracted - 1, "final": fin,
                "cap_hit": 1,
            }}})
        };
        let g = aggregate(&[row(10, Some(6)), row(5, None)]);
        let a = &g["two_call_actions"];
        assert_eq!(
            (
                a["extracted"].as_u64(),
                a["exact_repeats"].as_u64(),
                a["to_merge"].as_u64()
            ),
            (Some(15), Some(2), Some(13))
        );
        assert_eq!(
            (a["final"].as_u64(), a["final_meetings"].as_u64()),
            (Some(6), Some(1))
        );
        assert_eq!(a["cap_hit"].as_u64(), Some(2));
        assert!(aggregate(&[json!({"ok": true, "report": {}})])["two_call_actions"].is_null());
    }
}
