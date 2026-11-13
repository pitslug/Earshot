//! `cargo xtask sum-eval` (T6.20): the summary-quality evaluation over AMI meetings.
//!
//! 1. Earshot's own transcripts (source `earshot`, test split only): each test meeting is
//!    replayed as a call through the release `earshot` CLI (`earshot-cli replay`, the
//!    production live pass and cleanup, transcription model [`ASR_MODEL`]): the Project
//!    Manager's headset is the mic ("Me"), the other three headsets summed are the far end
//!    (the loopback reference). `--summary-input` stores the summary input Earshot would
//!    send. Replays are cached under the output directory and reused unless
//!    `--rerun-earshot`.
//! 2. The `sum_eval` example of `earshot-llm-local` (release, `--features cuda` when doctor
//!    reports CUDA) summarises every meeting of each job with each candidate of
//!    [`CANDIDATES`] and scores it (rules in the example's `score.rs` and
//!    `earshot_core::metrics::{rouge, items}`), one process per candidate.
//! 3. When `EARSHOT_LLM_URL` is set, the same jobs go to that endpoint (model
//!    `EARSHOT_LLM_MODEL`) as an informational ceiling; otherwise the report says "not run".
//!    No other endpoint is ever called.
//!
//! `--model claude-code` (or `claude-code/<model>`, T7.4gb) is a run of its own: Claude
//! through the user's installed Claude Code CLI, as the production Claude Code provider
//! summarises (the example's `claude_cli.rs`), over the public AMI reference transcripts
//! only. Without `--chunk-seconds` it is the production plan ("after the meeting"); with it
//! one map request per time chunk, then the reduce ("chunks during the meeting"). See
//! [`run_claude_code`]. `--model codex` (or `codex/<model>`, T7.4gc) is the same run through
//! the user's installed Codex CLI on their ChatGPT sign-in ([`AgentCli::Codex`]); the CLI is
//! found the production way (`earshot_llm::CodexCli::find`) unless `--codex-cli <path>`
//! names one (evaluation only).
//!
//! The report is `<out>/report.json` (default `target/sum-eval`) plus a Markdown table in
//! `<out>/table.md`. Thresholds are not checked here: they are agreed with the owner first.
//!
//! Settings rule (written before the runs it decides): with numbered
//! citations, content-sized reply budgets, the loop guard and the indented grammar, setting
//! S16 is `--context 16384 --max-output 8192` (the defaults) and setting S32 is `--context
//! 32768 --max-output 16384` (most AMI meetings then fit one request). Both run on the dev
//! split, reference transcripts, for Phi-4-mini, Qwen3 4B and Qwen3 8B (60 meeting runs
//! each). The score of a setting is the mean ROUGE-Lsum F1 over its 60 runs, a run without a
//! valid summary (or a candidate that cannot load) counting 0. S32 is chosen when its score
//! is at least 0.01 above S16's; otherwise S16 (half the memory). The test split then runs
//! once with the chosen setting for those three candidates, both sources. Earlier results
//! (segment ids in the prompt, a fixed 3072-token reply budget; and the compact-grammar
//! runs) are kept only as labelled asides.

use std::ffi::OsString;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use anyhow::{Context, Result, bail, ensure};
use earshot_core::prompt::{PromptVariant, StylePreset};
use serde_json::{Value, json};

use crate::ci::cargo_command;
use crate::fixtures::{AMI_ANNOTATIONS_DIR, AMI_MEETINGS_XML, ami_sum_meetings};
use crate::manifest::Manifest;
use crate::proc::run_with_timeout;

/// Transcription model of the Earshot source: the top of the English ladder for CUDA and
/// the CPU in `models/manifest.toml`.
pub const ASR_MODEL: &str = "parakeet-tdt-0.6b-v2";

/// Deadline for one release build (a cold CUDA build of whisper.cpp and llama.cpp).
const BUILD_TIMEOUT: Duration = Duration::from_secs(90 * 60);
/// Deadline for one meeting's replay (about 8 min for 18 min of audio on the desktop).
const REPLAY_TIMEOUT: Duration = Duration::from_secs(60 * 60);

/// One summary candidate: a model of the `llm-eval` set and what its chat template renders
/// after the assistant header with thinking off (llama.cpp's built-in templates stop at
/// the header).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Candidate {
    /// Artefact id in `models/manifest.toml`.
    pub id: &'static str,
    /// Display name.
    pub name: &'static str,
    /// Assistant prefix (from the model's own GGUF chat template).
    pub prefix: &'static str,
}

/// Qwen3 (hybrid) and Qwen3.5 templates with `enable_thinking` false (Qwen3.5's default).
const QWEN_NO_THINK: &str = "<think>\n\n</think>\n\n";
/// Granite 4.2's template with thinking off (its default).
const GRANITE_NO_THINK: &str = "<think></think>";
/// Gemma 4 has no prefix: llama.cpp cannot format its template, so
/// `earshot_llm_local`'s `chat_format::gemma4` writes the whole generation prompt,
/// including the empty thought channel that `enable_thinking` false opens the reply with.
/// A prefix here would repeat that channel (every Gemma result made before this was fixed
/// on 2026-10-26 was measured that way).
const GEMMA4_NO_THINK: &str = "";

/// The Claude Code candidate id (T7.4gb); `claude-code/<model>` asks for a model.
pub const CLAUDE_CODE: &str = "claude-code";

/// The Codex candidate id (T7.4gc); `codex/<model>` asks for a model.
pub const CODEX: &str = "codex";

/// An agent-CLI candidate: Claude Code (T7.4gb) or Codex (T7.4gc). Both run alone, over the
/// public AMI reference transcripts only, through the production provider of
/// `earshot_llm`, with its production budget; neither records the model that answered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentCli {
    /// Claude through Claude Code.
    ClaudeCode,
    /// OpenAI through Codex.
    Codex,
}

impl AgentCli {
    /// Every agent CLI.
    pub const ALL: [Self; 2] = [Self::ClaudeCode, Self::Codex];

    /// The candidate id without a model, also the backend label: `claude-code`, `codex`.
    #[must_use]
    pub fn id(self) -> &'static str {
        match self {
            Self::ClaudeCode => CLAUDE_CODE,
            Self::Codex => CODEX,
        }
    }

    /// The agent CLI candidate `id` names (`<id>` or `<id>/<model>`), if any.
    #[must_use]
    pub fn of_id(id: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|a| {
            id == a.id()
                || id
                    .strip_prefix(a.id())
                    .is_some_and(|rest| rest.starts_with('/'))
        })
    }

    /// The format key part of this candidate: `-claude-code`, `-codex`.
    #[must_use]
    pub fn key_suffix(self) -> &'static str {
        match self {
            Self::ClaudeCode => "-claude-code",
            Self::Codex => "-codex",
        }
    }

    /// The production budget of the provider.
    #[must_use]
    pub fn budget(self) -> earshot_core::prompt::ContextBudget {
        match self {
            Self::ClaudeCode => earshot_llm::claude_code::CLAUDE_CODE_BUDGET,
            Self::Codex => earshot_llm::codex::CODEX_BUDGET,
        }
    }

    /// The model asked for by candidate `id` (`<id>`: the CLI's default, `None`;
    /// `<id>/<model>`: `<model>`, checked with the provider's own `check_model`).
    ///
    /// # Errors
    /// Not this candidate's id, or a model name the provider refuses.
    pub fn model(self, id: &str) -> Result<Option<String>> {
        if id == self.id() {
            return Ok(None);
        }
        let model = id
            .strip_prefix(self.id())
            .and_then(|r| r.strip_prefix('/'))
            .with_context(|| format!("{id} is not the {} candidate", self.id()))?;
        ensure!(!model.is_empty(), "{}/ needs a model name", self.id());
        match self {
            Self::ClaudeCode => earshot_llm::claude_code::check_model(model),
            Self::Codex => earshot_llm::codex::check_model(model),
        }
        .map_err(|why| anyhow::anyhow!("{} model {model:?}: {why}", self.id()))?;
        Ok(Some(model.to_owned()))
    }

    /// The example's engine flag (`--claude-code`, `--codex`).
    fn engine_flag(self) -> &'static str {
        match self {
            Self::ClaudeCode => "--claude-code",
            Self::Codex => "--codex",
        }
    }

    /// The example's model flag (`--claude-model`, `--codex-model`).
    fn model_flag(self) -> &'static str {
        match self {
            Self::ClaudeCode => "--claude-model",
            Self::Codex => "--codex-model",
        }
    }

    /// The candidate's name in the report.
    fn display_name(self, model: Option<&str>) -> String {
        let route = match self {
            Self::ClaudeCode => "Claude through Claude Code",
            Self::Codex => "OpenAI through Codex",
        };
        match model {
            Some(m) => format!("{route} ({m})"),
            None => format!("{route} (CLI default model)"),
        }
    }

    /// The task that added the candidate.
    fn task(self) -> &'static str {
        match self {
            Self::ClaudeCode => "T7.4gb",
            Self::Codex => "T7.4gc",
        }
    }
}

/// The candidates, smallest first, then later additions.
pub const CANDIDATES: [Candidate; 15] = [
    Candidate {
        id: "qwen2.5-1.5b-instruct-q4_k_m",
        name: "Qwen2.5 1.5B Instruct",
        prefix: "",
    },
    Candidate {
        id: "phi-4-mini-instruct-q4_k_m",
        name: "Phi-4-mini-instruct (3.8B)",
        prefix: "",
    },
    Candidate {
        id: "qwen3-4b-q4_k_m",
        name: "Qwen3 4B",
        prefix: QWEN_NO_THINK,
    },
    Candidate {
        id: "qwen3-4b-instruct-2507-q4_k_m",
        name: "Qwen3 4B Instruct 2507",
        prefix: "",
    },
    Candidate {
        id: "qwen3.5-4b-q4_k_m",
        name: "Qwen3.5 4B",
        prefix: QWEN_NO_THINK,
    },
    Candidate {
        id: "qwen3-8b-q4_k_m",
        name: "Qwen3 8B",
        prefix: QWEN_NO_THINK,
    },
    Candidate {
        id: "granite-4.2-8b-q4_k_m",
        name: "Granite 4.2 8B",
        prefix: GRANITE_NO_THINK,
    },
    Candidate {
        id: "qwen3.5-9b-q4_k_m",
        name: "Qwen3.5 9B",
        prefix: QWEN_NO_THINK,
    },
    Candidate {
        id: "gemma-4-12b-it-qat-q4_0",
        name: "Gemma 4 12B IT (QAT Q4_0)",
        prefix: GEMMA4_NO_THINK,
    },
    Candidate {
        id: "qwen3-14b-q4_k_m",
        name: "Qwen3 14B",
        prefix: QWEN_NO_THINK,
    },
    Candidate {
        id: "qwen3.5-27b-q4_k_m",
        name: "Qwen3.5 27B",
        prefix: QWEN_NO_THINK,
    },
    // T6.20d screen (evaluation only): too large for the 16 GB GPU, runs on the CPU. The
    // 2507 Instruct release is non-thinking (ChatML, nothing after the assistant header).
    Candidate {
        id: "qwen3-30b-a3b-instruct-2507-q4_k_m",
        name: "Qwen3 30B-A3B Instruct 2507",
        prefix: "",
    },
    // T8.3l (evaluation only): a faster CPU model of the shipped family; kept last so the
    // earlier entries keep their positions.
    Candidate {
        id: "qwen3.5-2b-q4_k_m",
        name: "Qwen3.5 2B",
        prefix: QWEN_NO_THINK,
    },
    // T12.27 screen (evaluation only): the 3B of the Granite 4.2 line, against the Qwen
    // 3.5 4B; thinking off, as for the 8B.
    Candidate {
        id: "granite-4.2-3b-q4_k_m",
        name: "Granite 4.2 3B",
        prefix: GRANITE_NO_THINK,
    },
    // T12.27 (evaluation only, desktop GPU only): a Qwen 3.5 9B derivative whose template
    // differs from Qwen 3.5's in thinking on by default; thinking off renders the same block.
    Candidate {
        id: "ornith-1.5-9b-q4_k_m",
        name: "Ornith 1.5 9B",
        prefix: QWEN_NO_THINK,
    },
];

/// What to run.
#[derive(Debug, Clone)]
pub struct Options {
    /// `cpu` or `cuda`; `None`: CUDA when doctor reports it.
    pub backend: Option<String>,
    /// Candidate ids (empty: all).
    pub models: Vec<String>,
    /// `dev`, `test` (empty: both).
    pub splits: Vec<String>,
    /// `reference`, `earshot` (empty: both; `earshot` only on the test split).
    pub sources: Vec<String>,
    /// Only the first N meetings of each split (for a quick check).
    pub limit: Option<usize>,
    /// Output directory (default `target/sum-eval`).
    pub out: Option<PathBuf>,
    /// Replay the meetings again even when a cached summary input exists.
    pub rerun_earshot: bool,
    /// Only prepare the Earshot source (replay the test meetings); no summaries.
    pub replay_only: bool,
    /// Re-score the finished result files in `out` with the current scoring rules (no
    /// generation, no models needed) and rewrite the report.
    pub rescore: bool,
    /// Write the Anthropic requests of every meeting into this directory (for Claude Code
    /// agents) instead of summarising.
    pub export_prompts: Option<PathBuf>,
    /// Score the agents' replies in this directory as candidate [`Options::label`].
    pub import_replies: Option<PathBuf>,
    /// Candidate name of imported replies.
    pub label: Option<String>,
    /// Context window of every local model (tokens).
    pub context: u32,
    /// Ceiling of any one reply budget (tokens); each request gets a budget sized to what it
    /// summarises (`earshot_core::prompt::reply_tokens`).
    pub max_output: usize,
    /// CPU threads (default: the crate's `default_threads`).
    pub threads: Option<i32>,
    /// Cargo target directory (default: `<repo>/target` when short, else
    /// `%SystemDrive%\eb\host`, as `cargo xtask build`).
    pub target_dir: Option<PathBuf>,
    /// Forced chunking (T6.20 chunking experiment): a map request per this many seconds of
    /// meeting time (the rule is `earshot_core::prompt::time_chunks`, docs/summary-eval.md).
    pub chunk_seconds: Option<u32>,
    /// The shared prompt rules (T6.20b prompt tuning, docs/summary-eval.md): the production
    /// rules unless set; local summary runs only.
    pub prompt_variant: PromptVariant,
    /// A summary template in every input (T12.17, evaluation): Standard (none) unless set;
    /// summary runs only. Its format key part `-template-<name>` comes last.
    pub style_preset: StylePreset,
    /// Two-call chunks (T6.20c, evaluation only): per chunk one request for the action items
    /// and one for everything else, then a merge that folds reworded actions. Needs
    /// [`Options::chunk_seconds`]; local summary runs only.
    pub two_call: bool,
    /// Two-track chunks (T8.3ja, evaluation of `docs/live-summaries.md` option A): every
    /// chunk but the last is mapped from a two-track ("Me" / "Them") copy of the transcript
    /// so far, re-cited onto the real transcript and reused; then the last chunk and the
    /// reduce. Needs [`Options::chunk_seconds`]; local summary runs on the reference source
    /// only.
    pub two_track: bool,
    /// Only these meetings (each must be in its split's whole list; empty: all);
    /// [`Options::limit`] applies after this selection.
    pub meetings: Vec<String>,
    /// The Codex CLI of a `--model codex` run (evaluation only, T7.4gc): without it the CLI
    /// is found as production finds it (`earshot_llm::CodexCli::find`).
    pub codex_cli: Option<PathBuf>,
    /// Qwen thinking on (T12.10, evaluation only): every reply may think first, up to this
    /// many tokens counted apart from the reply budget (`earshot_llm_local::thinking`). Local
    /// summary runs of the candidates whose prefix is the empty thinking block only.
    pub thinking: Option<u32>,
}

/// The request format the `sum_eval` example records as `grammar` (its `format_key`): a
/// stored report is reused only when it equals this. `-chunk<seconds>` marks forced
/// chunking, `-twocall` the T6.20c two-call chunks, `-twotrack` the T8.3ja two-track
/// chunks, `-claude-code` the T7.4gb Claude Code candidate or `-codex` the T7.4gc Codex
/// candidate ([`AgentCli::key_suffix`]), `-think<cap>` T12.10 thinking on with that token
/// cap, `-prompt-<variant>` a T6.20b prompt variant other than the production one.
#[must_use]
pub fn format_key(
    chunk_seconds: Option<u32>,
    variant: PromptVariant,
    two_call: bool,
    two_track: bool,
    agent: Option<AgentCli>,
    thinking: Option<u32>,
) -> String {
    let mut key = match chunk_seconds {
        Some(s) => format!("indented-numbered-r5-chunk{s}"),
        None => "indented-numbered-r5".to_owned(),
    };
    if two_call {
        key.push_str("-twocall");
    }
    if two_track {
        key.push_str("-twotrack");
    }
    if let Some(a) = agent {
        key.push_str(a.key_suffix());
    }
    if let Some(cap) = thinking {
        key.push_str(&format!("-think{cap}"));
    }
    if !variant.is_current() {
        key.push_str("-prompt-");
        key.push_str(variant.name());
    }
    key
}

/// Whether a finished result file `v` was made with another format key than this run's,
/// so this run must not overwrite it (result files are named by candidate and backend
/// only: runs with different format keys need different `--out` directories). A finished
/// file (one with `jobs`) without a recorded key comes from older scoring and is protected
/// from every run.
#[must_use]
pub fn other_format(v: &Value, opts: &Options) -> Option<String> {
    match v["grammar"].as_str() {
        Some(g) => (g != opts.format_key()).then(|| g.to_owned()),
        None => v["jobs"]
            .is_array()
            .then(|| "none recorded (older scoring)".to_owned()),
    }
}

/// Whether a finished result file `v` (one with `jobs`) was made with another context size
/// or reply ceiling than this run's (or records none), so this run must not overwrite it:
/// with the same format key and meetings [`covers`] still misses, and the run would replace
/// the stored aggregate. Runs with other settings need another `--out`.
#[must_use]
pub fn other_settings(v: &Value, opts: &Options) -> Option<String> {
    if !v["jobs"].is_array() {
        return None;
    }
    let context = v["context_tokens"].as_u64();
    let max_output = v["max_output_tokens"].as_u64();
    let (want_context, want_output) = opts.budget();
    let mut diffs = Vec::new();
    if context != Some(want_context) {
        diffs.push(format!("context_tokens {context:?}, not {want_context}"));
    }
    if max_output != Some(want_output) {
        diffs.push(format!(
            "max_output_tokens {max_output:?}, not {want_output}"
        ));
    }
    (!diffs.is_empty()).then(|| diffs.join("; "))
}

/// The meetings of a finished result file `v` (`<split>:<source>:<meeting>`) that a run of
/// `jobs` over `meetings` would not hold: its report replaces the file, so a `--meeting`
/// subset, a smaller `--limit` or fewer jobs would silently drop them and their aggregate.
/// Empty when the run holds every stored meeting (or `v` is not a finished file).
#[must_use]
pub fn dropped_meetings(
    v: &Value,
    jobs: &[(String, String)],
    meetings: &[(String, Vec<String>)],
) -> Vec<String> {
    let mut dropped = Vec::new();
    for j in v["jobs"].as_array().into_iter().flatten() {
        let split = j["split"].as_str().unwrap_or_default();
        let source = j["source"].as_str().unwrap_or_default();
        let run = jobs.iter().any(|(s, src)| s == split && src == source);
        let list = meetings
            .iter()
            .find(|(s, _)| s == split)
            .map(|(_, m)| m.as_slice())
            .unwrap_or_default();
        for m in j["meetings"].as_array().into_iter().flatten() {
            let meeting = m["meeting"].as_str().unwrap_or_default();
            if !run || !list.iter().any(|x| x == meeting) {
                dropped.push(format!("{split}:{source}:{meeting}"));
            }
        }
    }
    dropped
}

impl Options {
    /// [`format_key`] of this run.
    #[must_use]
    pub fn format_key(&self) -> String {
        let mut key = format_key(
            self.chunk_seconds,
            self.prompt_variant,
            self.two_call,
            self.two_track,
            self.agent_cli(),
            self.thinking,
        );
        // T12.17: the summary template last, as the example's `template_suffix`.
        if !self.style_preset.is_standard() {
            key.push_str("-template-");
            key.push_str(self.style_preset.name());
        }
        key
    }

    /// The agent-CLI candidate of this run (`--model claude-code[/<model>]` or
    /// `--model codex[/<model>]`), if any.
    #[must_use]
    pub fn agent_cli(&self) -> Option<AgentCli> {
        self.models.iter().find_map(|m| AgentCli::of_id(m))
    }

    /// Whether this run is the Claude Code candidate (`--model claude-code[/<model>]`).
    #[must_use]
    pub fn is_claude_code(&self) -> bool {
        self.agent_cli() == Some(AgentCli::ClaudeCode)
    }

    /// Whether this run is the Codex candidate (`--model codex[/<model>]`).
    #[must_use]
    pub fn is_codex(&self) -> bool {
        self.agent_cli() == Some(AgentCli::Codex)
    }

    /// The context window and reply ceiling a result file of this run records: the
    /// provider's production budget for an agent-CLI candidate
    /// (`earshot_llm::claude_code::CLAUDE_CODE_BUDGET`, `earshot_llm::codex::CODEX_BUDGET`),
    /// else `--context` and `--max-output`.
    #[must_use]
    pub fn budget(&self) -> (u64, u64) {
        match self.agent_cli() {
            Some(a) => {
                let b = a.budget();
                (b.context_window as u64, b.max_output_tokens as u64)
            }
            None => (u64::from(self.context), self.max_output as u64),
        }
    }

    /// Whether this run only exports prompts or imports stored replies (no models).
    #[must_use]
    pub fn stored_only(&self) -> bool {
        self.export_prompts.is_some() || self.import_replies.is_some()
    }
}

impl Default for Options {
    fn default() -> Self {
        Options {
            backend: None,
            models: Vec::new(),
            splits: Vec::new(),
            sources: Vec::new(),
            limit: None,
            out: None,
            rerun_earshot: false,
            replay_only: false,
            rescore: false,
            export_prompts: None,
            import_replies: None,
            label: None,
            context: 16_384,
            max_output: 8192,
            threads: None,
            target_dir: None,
            chunk_seconds: None,
            prompt_variant: PromptVariant::Current,
            style_preset: StylePreset::Standard,
            two_call: false,
            two_track: false,
            meetings: Vec::new(),
            codex_cli: None,
            thinking: None,
        }
    }
}

/// The jobs (`split`, `source`) for the chosen splits and sources. The Earshot source has
/// audio only for the test split.
///
/// # Errors
/// An unknown split or source, or nothing left to run.
pub fn jobs(splits: &[String], sources: &[String]) -> Result<Vec<(String, String)>> {
    let splits: Vec<&str> = if splits.is_empty() {
        vec!["dev", "test"]
    } else {
        splits.iter().map(String::as_str).collect()
    };
    let sources: Vec<&str> = if sources.is_empty() {
        vec!["reference", "earshot"]
    } else {
        sources.iter().map(String::as_str).collect()
    };
    let mut out = Vec::new();
    for split in &splits {
        ensure!(matches!(*split, "dev" | "test"), "unknown split {split}");
        for source in &sources {
            ensure!(
                matches!(*source, "reference" | "earshot"),
                "unknown source {source}"
            );
            if *source == "earshot" && *split != "test" {
                continue;
            }
            out.push(((*split).to_owned(), (*source).to_owned()));
        }
    }
    ensure!(
        !out.is_empty(),
        "no job: the earshot source exists only for the test split"
    );
    Ok(out)
}

/// The candidates selected by `ids` (empty: all), in [`CANDIDATES`] order.
///
/// # Errors
/// An unknown id.
pub fn select_candidates(ids: &[String]) -> Result<Vec<Candidate>> {
    for id in ids {
        ensure!(
            CANDIDATES.iter().any(|c| c.id == id),
            "unknown candidate {id}; known: {}",
            CANDIDATES.map(|c| c.id).join(", ")
        );
    }
    Ok(CANDIDATES
        .into_iter()
        .filter(|c| ids.is_empty() || ids.iter().any(|i| i == c.id))
        .collect())
}

/// `(headset channel, role)` of every participant of `meeting` in
/// `corpusResources/meetings.xml` (ISO-8859-1 text), sorted by channel.
///
/// # Errors
/// The meeting is missing or a speaker lacks a channel or role.
pub fn ami_headset_roles(xml: &str, meeting: &str) -> Result<Vec<(u8, String)>> {
    let attr = |tag: &str, name: &str| -> Option<String> {
        let key = format!(" {name}=\"");
        let at = tag.find(&key)? + key.len();
        let len = tag[at..].find('"')?;
        Some(tag[at..at + len].to_owned())
    };
    let open = format!(" observation=\"{meeting}\"");
    let at = xml
        .find(&open)
        .with_context(|| format!("meeting {meeting} not in {AMI_MEETINGS_XML}"))?;
    let body = &xml[at..];
    let body = &body[..body.find("</meeting>").context("unterminated <meeting>")?];
    let mut out = Vec::new();
    let mut rest = body;
    while let Some(i) = rest.find("<speaker ") {
        rest = &rest[i..];
        let end = rest.find('>').context("unterminated <speaker>")?;
        let tag = &rest[..end];
        rest = &rest[end..];
        let channel: u8 = attr(tag, "channel")
            .with_context(|| format!("speaker without channel: {tag}"))?
            .parse()?;
        let role = attr(tag, "role").with_context(|| format!("speaker without role: {tag}"))?;
        out.push((channel, role));
    }
    out.sort();
    Ok(out)
}

/// The mic headset (the Project Manager's) and the far-end headsets of a meeting.
///
/// # Errors
/// Not exactly one Project Manager, or not four headsets 0-3.
pub fn call_channels(roles: &[(u8, String)]) -> Result<(u8, Vec<u8>)> {
    let channels: Vec<u8> = roles.iter().map(|(c, _)| *c).collect();
    ensure!(
        channels == [0, 1, 2, 3],
        "headsets {channels:?}, expected 0-3"
    );
    let pm: Vec<u8> = roles
        .iter()
        .filter(|(_, r)| r == "PM")
        .map(|(c, _)| *c)
        .collect();
    ensure!(pm.len() == 1, "{} Project Managers", pm.len());
    Ok((
        pm[0],
        channels.into_iter().filter(|c| *c != pm[0]).collect(),
    ))
}

/// The samples of a 16-bit PCM mono WAV file.
///
/// # Errors
/// Not RIFF/WAVE, not 16-bit mono PCM at 16 kHz, or no data chunk.
pub fn read_wav_16k_mono(bytes: &[u8]) -> Result<Vec<i16>> {
    ensure!(
        bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WAVE",
        "not a RIFF/WAVE file"
    );
    let mut pos = 12;
    let mut fmt_ok = false;
    while pos + 8 <= bytes.len() {
        let id = &bytes[pos..pos + 4];
        let len = u32::from_le_bytes(bytes[pos + 4..pos + 8].try_into()?) as usize;
        let body = bytes
            .get(pos + 8..pos + 8 + len)
            .context("truncated WAV chunk")?;
        if id == b"fmt " {
            ensure!(body.len() >= 16, "short fmt chunk");
            let format = u16::from_le_bytes([body[0], body[1]]);
            let channels = u16::from_le_bytes([body[2], body[3]]);
            let rate = u32::from_le_bytes(body[4..8].try_into()?);
            let bits = u16::from_le_bytes([body[14], body[15]]);
            ensure!(
                format == 1 && channels == 1 && rate == 16_000 && bits == 16,
                "expected 16 kHz 16-bit mono PCM, got format {format}, {channels} ch, {rate} Hz, {bits} bit"
            );
            fmt_ok = true;
        } else if id == b"data" {
            ensure!(fmt_ok, "data before fmt");
            return Ok(body
                .as_chunks::<2>()
                .0
                .iter()
                .map(|b| i16::from_le_bytes(*b))
                .collect());
        }
        pos += 8 + len + (len & 1);
    }
    bail!("no data chunk")
}

/// The sum of `tracks` (as long as the longest), clipped to 16 bits.
#[must_use]
pub fn mix(tracks: &[Vec<i16>]) -> Vec<i16> {
    let n = tracks.iter().map(Vec::len).max().unwrap_or(0);
    (0..n)
        .map(|i| {
            let s: i32 = tracks
                .iter()
                .map(|t| i32::from(t.get(i).copied().unwrap_or(0)))
                .sum();
            i16::try_from(s.clamp(i32::from(i16::MIN), i32::from(i16::MAX))).unwrap_or(0)
        })
        .collect()
}

/// A 16 kHz 16-bit mono WAV file of `samples`.
#[must_use]
pub fn wav_bytes(samples: &[i16]) -> Vec<u8> {
    let data = u32::try_from(samples.len() * 2).unwrap_or(u32::MAX);
    let mut v = Vec::with_capacity(44 + samples.len() * 2);
    v.extend_from_slice(b"RIFF");
    v.extend_from_slice(&(36 + data).to_le_bytes());
    v.extend_from_slice(b"WAVEfmt ");
    v.extend_from_slice(&16u32.to_le_bytes());
    v.extend_from_slice(&1u16.to_le_bytes());
    v.extend_from_slice(&1u16.to_le_bytes());
    v.extend_from_slice(&16_000u32.to_le_bytes());
    v.extend_from_slice(&32_000u32.to_le_bytes());
    v.extend_from_slice(&2u16.to_le_bytes());
    v.extend_from_slice(&16u16.to_le_bytes());
    v.extend_from_slice(b"data");
    v.extend_from_slice(&data.to_le_bytes());
    for s in samples {
        v.extend_from_slice(&s.to_le_bytes());
    }
    v
}

/// The `executable` cargo reports for target `name` in its JSON build messages.
#[must_use]
pub fn executable(messages: &str, name: &str) -> Option<PathBuf> {
    messages
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter(|m| m["reason"] == "compiler-artifact" && m["target"]["name"] == name)
        .find_map(|m| m["executable"].as_str().map(PathBuf::from))
}

/// llama.cpp's DLL directory: `<out_dir>/bin` of llama-cpp-sys-2's build script.
#[must_use]
pub fn llama_bin_dir(messages: &str) -> Option<PathBuf> {
    crate::llama_dlls::bin_dirs(messages).into_iter().next()
}

/// cuDNN 9's DLL directory for ONNX Runtime's CUDA library (Parakeet on CUDA), if
/// installed in NVIDIA's default location: the newest `CUDNN\v9.*\bin\<cuda>\x64` holding
/// `cudnn64_9.dll`.
fn cudnn_bin() -> Option<PathBuf> {
    let root = PathBuf::from(std::env::var_os("ProgramFiles")?)
        .join("NVIDIA")
        .join("CUDNN");
    let mut found = Vec::new();
    for v in std::fs::read_dir(&root).ok()?.flatten() {
        let Ok(cudas) = std::fs::read_dir(v.path().join("bin")) else {
            continue;
        };
        for c in cudas.flatten() {
            let dir = c.path().join("x64");
            if dir.join("cudnn64_9.dll").is_file() {
                found.push(dir);
            }
        }
    }
    found.sort();
    found.pop()
}

/// `PATH` with `dirs` in front.
fn path_with(dirs: &[PathBuf]) -> Result<OsString> {
    let mut all: Vec<PathBuf> = dirs.to_vec();
    if let Some(p) = std::env::var_os("PATH") {
        all.extend(std::env::split_paths(&p));
    }
    Ok(std::env::join_paths(all)?)
}

/// Release build of `args` (cargo JSON messages on stdout).
fn build(root: &Path, args: &[String], what: &str) -> Result<String> {
    let mut cmd = cargo_command();
    cmd.args(args).current_dir(root);
    // llama.cpp's Vulkan backend needs Ninja and the Visual Studio x64 environment on x64.
    if cfg!(target_arch = "x86_64") && args.windows(2).any(|w| w == ["--features", "vulkan"]) {
        crate::build::apply_msvc_x64_env(&mut cmd)?;
    }
    eprintln!("sum-eval: building {what} (release)");
    let out = run_with_timeout(&mut cmd, BUILD_TIMEOUT)
        .with_context(|| format!("building {what} did not finish within the deadline"))?;
    if !out.success {
        bail!("building {what} failed: {}", out.describe());
    }
    out.require_complete(what)?;
    Ok(out.stdout)
}

/// The cargo build arguments of a package/target with `feature` (`cuda` or `vulkan`) or none.
#[must_use]
pub fn build_args(
    package: &str,
    target: &[&str],
    feature: Option<&str>,
    target_dir: &Path,
) -> Vec<String> {
    let mut v: Vec<String> = ["build", "--release", "-p", package]
        .map(Into::into)
        .to_vec();
    v.extend(target.iter().map(|s| (*s).to_owned()));
    if let Some(f) = feature {
        v.extend(["--features".into(), f.into()]);
    }
    v.extend([
        "--target-dir".into(),
        target_dir.display().to_string(),
        "--message-format=json-render-diagnostics".into(),
    ]);
    v
}

/// Replays every test meeting through the CLI (module docs), writing
/// `<out>/earshot/<meeting>.json`. Returns per-meeting facts for the report.
fn replay_meetings(
    earshot: &Path,
    meetings: &[String],
    fixtures: &Path,
    out: &Path,
    cuda: bool,
    rerun: bool,
) -> Result<Vec<Value>> {
    let dir = out.join("earshot");
    std::fs::create_dir_all(&dir)?;
    let ann = fixtures.join(AMI_ANNOTATIONS_DIR);
    let xml: String = std::fs::read(ann.join(AMI_MEETINGS_XML))
        .context("run `cargo xtask fetch-fixtures --set ami-sum`")?
        .into_iter()
        .map(char::from)
        .collect();
    let mut path_dirs = Vec::new();
    if cuda && let Some(d) = cudnn_bin() {
        path_dirs.push(d);
    }
    let path = path_with(&path_dirs)?;
    let mut facts = Vec::new();
    for meeting in meetings {
        let input = dir.join(format!("{meeting}.json"));
        let log = dir.join(format!("{meeting}.log"));
        if input.is_file() && !rerun {
            eprintln!("sum-eval: {meeting}: reusing {}", input.display());
        } else {
            let (mic, far) = call_channels(&ami_headset_roles(&xml, meeting)?)?;
            let wav = |c: u8| fixtures.join(format!("ami/audio/{meeting}.Headset-{c}.wav"));
            let mut tracks = Vec::new();
            for c in &far {
                let p = wav(*c);
                let bytes = std::fs::read(&p).with_context(|| {
                    format!(
                        "{}: run `cargo xtask fetch-fixtures --set ami-sum`",
                        p.display()
                    )
                })?;
                tracks.push(read_wav_16k_mono(&bytes).with_context(|| p.display().to_string())?);
            }
            let reference = dir.join(format!("{meeting}.far.wav"));
            std::fs::write(&reference, wav_bytes(&mix(&tracks)))?;
            let data = dir.join(format!("data-{meeting}"));
            if data.exists() {
                std::fs::remove_dir_all(&data)?;
            }
            let mut cmd = Command::new(earshot);
            cmd.arg("replay")
                .arg("--mic")
                .arg(wav(mic))
                .arg("--ref")
                .arg(&reference)
                .arg("--out")
                .arg(&data)
                .args(["--model", ASR_MODEL])
                // Only the summary input is wanted; the candidates summarise it later.
                .arg("--no-summary")
                .arg("--summary-input")
                .arg(&input)
                .env("PATH", &path);
            eprintln!("sum-eval: {meeting}: replaying (mic Headset-{mic}, far end {far:?})");
            let started = std::time::Instant::now();
            let r = run_with_timeout(&mut cmd, REPLAY_TIMEOUT)
                .with_context(|| format!("{meeting}: replay did not finish within the deadline"))?;
            std::fs::write(&log, format!("{}\n--- stderr ---\n{}", r.stdout, r.stderr))?;
            if !r.success {
                bail!(
                    "{meeting}: replay failed (log {}): {}",
                    log.display(),
                    r.describe()
                );
            }
            r.require_complete("earshot-cli replay")?;
            std::fs::write(
                dir.join(format!("{meeting}.seconds")),
                format!("{:.1}", started.elapsed().as_secs_f64()),
            )?;
            // The audio copies are large and reproducible.
            let _ = std::fs::remove_file(&reference);
            let _ = std::fs::remove_dir_all(data.join("audio"));
        }
        let text = std::fs::read_to_string(&log).unwrap_or_default();
        let pick = |prefix: &str| -> Vec<String> {
            text.lines()
                .filter(|l| l.trim_start().starts_with(prefix))
                .map(|l| l.trim().to_owned())
                .collect()
        };
        facts.push(json!({
            "meeting": meeting,
            "replay_seconds": std::fs::read_to_string(dir.join(format!("{meeting}.seconds")))
                .ok()
                .and_then(|s| s.trim().parse::<f64>().ok()),
            "asr": pick("live asr"),
            "post": pick("post: final pass ran"),
            "echo": pick("post: echo"),
        }));
    }
    Ok(facts)
}

/// The input arguments of the `sum_eval` example: annotations, Earshot inputs, jobs and
/// meetings.
#[must_use]
pub fn input_args(
    annotations: &Path,
    earshot_inputs: &Path,
    jobs: &[(String, String)],
    meetings: &[(String, Vec<String>)],
) -> Vec<OsString> {
    let mut a: Vec<OsString> = vec![
        "--annotations".into(),
        annotations.into(),
        "--earshot-inputs".into(),
        earshot_inputs.into(),
    ];
    for (split, source) in jobs {
        a.push("--job".into());
        a.push(format!("{split}:{source}").into());
    }
    for (split, list) in meetings {
        a.push("--meetings".into());
        a.push(format!("{split}={}", list.join(",")).into());
    }
    a
}

/// The arguments of one `sum_eval` run.
#[allow(clippy::too_many_arguments)]
#[must_use]
pub fn eval_args(
    annotations: &Path,
    earshot_inputs: &Path,
    jobs: &[(String, String)],
    meetings: &[(String, Vec<String>)],
    model: Option<(&Path, &Candidate)>,
    gpu: bool,
    opts: &Options,
    out: &Path,
) -> Vec<OsString> {
    let mut a = input_args(annotations, earshot_inputs, jobs, meetings);
    match model {
        Some((path, c)) => {
            a.extend([
                "--model".into(),
                path.into(),
                "--model-id".into(),
                c.id.into(),
            ]);
            if !c.prefix.is_empty() {
                a.extend(["--assistant-prefix".into(), c.prefix.into()]);
            }
            if gpu {
                a.push("--gpu".into());
            }
        }
        None => a.extend(["--remote".into(), "--model-id".into(), "remote".into()]),
    }
    if let Some(t) = opts.threads {
        a.extend(["--threads".into(), t.to_string().into()]);
    }
    if let Some(s) = opts.chunk_seconds {
        a.extend(["--chunk-seconds".into(), s.to_string().into()]);
    }
    if !opts.prompt_variant.is_current() {
        a.extend(["--prompt-variant".into(), opts.prompt_variant.name().into()]);
    }
    if !opts.style_preset.is_standard() {
        a.extend(["--style-preset".into(), opts.style_preset.name().into()]);
    }
    if opts.two_track {
        a.push("--two-track".into());
    }
    if opts.two_call {
        a.push("--two-call".into());
    }
    if let Some(cap) = opts.thinking {
        a.extend(["--thinking".into(), cap.to_string().into()]);
    }
    a.extend([
        "--context".into(),
        opts.context.to_string().into(),
        "--max-output".into(),
        opts.max_output.to_string().into(),
        "--out".into(),
        out.into(),
    ]);
    a
}

fn fmt_opt(v: &Value, digits: usize) -> String {
    v.as_f64()
        .map_or_else(|| "-".to_owned(), |x| format!("{x:.digits$}"))
}

/// Whether an earlier candidate report `v` holds exactly these jobs and meetings with
/// these settings (then it is reused instead of run again; a run that was stopped writes
/// no report, so a partial one is never reused).
#[must_use]
pub fn covers(
    v: &Value,
    jobs: &[(String, String)],
    meetings: &[(String, Vec<String>)],
    opts: &Options,
) -> bool {
    let Some(have) = v["jobs"].as_array() else {
        return false;
    };
    let (context, max_output) = opts.budget();
    let settings = v["grammar"] == opts.format_key().as_str()
        && v["context_tokens"].as_u64() == Some(context)
        && v["max_output_tokens"].as_u64() == Some(max_output);
    settings
        && have.len() == jobs.len()
        && jobs.iter().zip(have).all(|((split, source), j)| {
            let want = meetings
                .iter()
                .find(|(s, _)| s == split)
                .map(|(_, m)| m.clone())
                .unwrap_or_default();
            let got: Vec<String> = j["meetings"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|m| m["meeting"].as_str().map(str::to_owned))
                .collect();
            j["split"] == split.as_str() && j["source"] == source.as_str() && got == want
        })
}

/// The Markdown table of a report: one row per candidate and job.
#[must_use]
pub fn table(report: &Value) -> String {
    let mut s = String::from(
        "| candidate | backend | split | source | ok | ROUGE-L F1 | ROUGE-Lsum F1 | ROUGE-1 F1 | ROUGE-2 F1 | decisions caught, strict F1 (recall / precision) | actions caught, strict F1 | decisions caught, reference recall* | actions caught, reference recall* | decisions covered** | actions covered** | s/meeting | reply tok/s | prompt tok/s | repairs | truncated | looping |\n|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|\n",
    );
    for c in report["candidates"].as_array().into_iter().flatten() {
        for j in c["jobs"].as_array().into_iter().flatten() {
            let g = &j["aggregate"];
            let items = |k: &str| {
                format!(
                    "{}/{} ({} / {})",
                    g[k]["caught"],
                    g[k]["reference"],
                    fmt_opt(&g[k]["recall"], 2),
                    fmt_opt(&g[k]["precision"], 2)
                )
            };
            // Many-to-one: only caught / reference is meaningful.
            let covered = |k: &str| {
                format!(
                    "{}/{} ({})",
                    g[k]["caught"],
                    g[k]["reference"],
                    fmt_opt(&g[k]["recall"], 2)
                )
            };
            let _ = writeln!(
                s,
                "| {} | {} | {} | {} | {}/{} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} |",
                candidate_label(c),
                c["backend"].as_str().unwrap_or("?"),
                j["split"].as_str().unwrap_or("?"),
                j["source"].as_str().unwrap_or("?"),
                g["ok"],
                g["meetings"],
                fmt_opt(&g["rougeL_f"], 3),
                fmt_opt(&g["rougeLsum_f"], 3),
                fmt_opt(&g["rouge1_f"], 3),
                fmt_opt(&g["rouge2_f"], 3),
                items("decisions"),
                items("actions"),
                items("decisions_rr"),
                items("actions_rr"),
                covered("decisions_cov"),
                covered("actions_cov"),
                fmt_opt(&g["seconds_per_meeting"], 1),
                fmt_opt(&g["generated_tokens_per_s"], 1),
                fmt_opt(&g["prompt_tokens_per_s"], 0),
                g["repairs"],
                g["truncated"],
                g["looping"],
            );
        }
    }
    s.push_str(
        "\n* Reference recall: a reference item counts as caught when some summary item holds \
         at least half of its content tokens (defined after seeing dev data; \
         `earshot_core::metrics::items`).\n\
         ** Coverage: a reference item counts as caught when all of the summary's items of \
         that list together hold at least half of its content tokens (many-to-one; defined \
         after seeing data; `items::covered_items`).\n",
    );
    length_table(report, &mut s);
    let chunked: Vec<&Value> = report["candidates"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|c| !c["chunk_seconds"].is_null())
        .collect();
    if !chunked.is_empty() {
        s.push_str(
            "\n| candidate | split | source | chunk s | map steps | map s | reduce s | at hang-up s (last map + reduce) | at hang-up s per meeting | reduce s per meeting | audio s |\n|---|---|---|---|---|---|---|---|---|---|---|\n",
        );
        for c in chunked {
            for j in c["jobs"].as_array().into_iter().flatten() {
                let t = &j["aggregate"]["timing"];
                let steps: u64 = j["meetings"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(|m| m["report"]["timing"]["map_steps"].as_u64().unwrap_or(0))
                    .sum();
                let _ = writeln!(
                    s,
                    "| {} | {} | {} | {} | {steps} | {} | {} | {} | {} | {} | {} |",
                    c["candidate"].as_str().unwrap_or("?"),
                    j["split"].as_str().unwrap_or("?"),
                    j["source"].as_str().unwrap_or("?"),
                    c["chunk_seconds"],
                    fmt_opt(&t["map_s"], 1),
                    fmt_opt(&t["reduce_s"], 1),
                    fmt_opt(&t["hangup_s"], 1),
                    fmt_opt(&t["hangup_s_per_meeting"], 1),
                    fmt_opt(&t["reduce_s_per_meeting"], 1),
                    fmt_opt(&t["audio_s"], 0),
                );
            }
        }
    }
    s
}

/// A report's candidate name, with its prompt variant when that is not the production one
/// and "two-call" for a T6.20c run.
/// The T12.17 length table (docs/summary-eval.md): words, notes and items written, summed
/// over a job's meetings, and lists at the local grammar's item cap. Only for reports whose
/// aggregates carry the length block.
fn length_table(report: &Value, s: &mut String) {
    let rows: Vec<(&Value, &Value)> = report["candidates"]
        .as_array()
        .into_iter()
        .flatten()
        .flat_map(|c| {
            c["jobs"]
                .as_array()
                .into_iter()
                .flatten()
                .map(move |j| (c, j))
        })
        .filter(|(_, j)| j["aggregate"]["length"].is_object())
        .collect();
    if rows.is_empty() {
        return;
    }
    s.push_str(
        "\n| candidate | split | source | ok | words | notes words (tldr, headings, bullets) | sections | bullets | decisions written | actions written | lists at the cap | decision or action lists at the cap | meetings with one |\n|---|---|---|---|---|---|---|---|---|---|---|---|---|\n",
    );
    for (c, j) in rows {
        let g = &j["aggregate"];
        let l = &g["length"];
        let _ = writeln!(
            s,
            "| {} | {} | {} | {}/{} | {} | {} | {} | {} | {} | {} | {} | {} | {} |",
            candidate_label(c),
            j["split"].as_str().unwrap_or("?"),
            j["source"].as_str().unwrap_or("?"),
            g["ok"],
            g["meetings"],
            l["words"],
            l["notes_words"],
            l["sections"],
            l["bullets"],
            l["decisions"],
            l["actions"],
            l["lists_at_cap"],
            l["item_lists_at_cap"],
            l["meetings_with_item_list_at_cap"],
        );
    }
}

fn candidate_label(c: &Value) -> String {
    let mut name = c["candidate"].as_str().unwrap_or("?").to_owned();
    if c["two_call"] == true {
        name.push_str(" (two-call)");
    }
    if c["two_track"] == true {
        name.push_str(" (two-track)");
    }
    if let Some(cap) = c["thinking_cap"].as_u64() {
        name.push_str(&format!(" (thinking {cap})"));
    }
    match c["prompt_variant"].as_str() {
        Some(v) if v != PromptVariant::Current.name() => format!("{name} (prompt {v})"),
        _ => name,
    }
}

/// Only the meetings of `wanted` (empty: all), each of which must be in one of `meetings`'
/// lists; a split left with no meeting is dropped.
///
/// # Errors
/// A wanted meeting that is in no list.
pub fn select_meetings(
    meetings: Vec<(String, Vec<String>)>,
    wanted: &[String],
) -> Result<Vec<(String, Vec<String>)>> {
    if wanted.is_empty() {
        return Ok(meetings);
    }
    for w in wanted {
        ensure!(
            meetings.iter().any(|(_, list)| list.contains(w)),
            "meeting {w} is not in the selected splits"
        );
    }
    Ok(meetings
        .into_iter()
        .map(|(split, list)| {
            (
                split,
                list.into_iter().filter(|m| wanted.contains(m)).collect(),
            )
        })
        .filter(|(_, list): &(String, Vec<String>)| !list.is_empty())
        .collect())
}

/// The meetings of a run: each split of `jobs` (dev before test) with its `ami-sum`
/// meetings, narrowed to `wanted` ([`select_meetings`], checked against the whole lists),
/// then cut to the first `limit` of each split.
///
/// # Errors
/// A wanted meeting that is in no selected split.
pub fn run_meetings(
    jobs: &[(String, String)],
    wanted: &[String],
    limit: Option<usize>,
) -> Result<Vec<(String, Vec<String>)>> {
    let mut meetings = Vec::new();
    for split in ["dev", "test"] {
        if jobs.iter().any(|(s, _)| s == split) {
            meetings.push((split.to_owned(), ami_sum_meetings(split)));
        }
    }
    Ok(select_meetings(meetings, wanted)?
        .into_iter()
        .map(|(split, list)| match limit {
            Some(n) => (split, list.into_iter().take(n).collect()),
            None => (split, list),
        })
        .collect())
}

/// Arguments of the `sum_eval` example that re-score one finished result `file`.
#[must_use]
pub fn rescore_args(annotations: &Path, file: &Path) -> Vec<OsString> {
    vec![
        "--annotations".into(),
        annotations.as_os_str().to_owned(),
        "--rescore".into(),
        file.as_os_str().to_owned(),
    ]
}

/// The feature the `sum_eval` example is built with: `cuda` or `vulkan` for those runs, none
/// for a CPU run. A CUDA build with no layers offloaded still sends large prompt batches to
/// the GPU (llama.cpp's op offload), so a CPU timing needs a build without CUDA.
#[must_use]
pub fn example_feature(backend: &str) -> Option<&'static str> {
    match backend {
        "cuda" => Some("cuda"),
        "vulkan" => Some("vulkan"),
        _ => None,
    }
}

/// Refuses a Vulkan result that llama.cpp did not load on Vulkan (its recorded `backend`,
/// from `earshot_llm_local::log::classify`). Other backends pass: a CPU run is checked by
/// [`check_cpu_only`] and a CUDA load is refused by the loader when it lands elsewhere.
///
/// # Errors
/// A `vulkan` run whose result does not record the Vulkan backend.
pub fn check_gpu_backend(backend: &str, result: &Value) -> Result<()> {
    if backend != "vulkan" {
        return Ok(());
    }
    let ran = result["backend"].as_str().unwrap_or("none recorded");
    ensure!(ran == "vulkan", "a Vulkan sum-eval loaded on {ran}");
    Ok(())
}

/// Refuses a CPU result whose llama.cpp build has a CUDA backend (its `system_info`).
///
/// # Errors
/// A `cpu` run from a build that lists CUDA, or a result without `system_info`.
pub fn check_cpu_only(backend: &str, result: &Value) -> Result<()> {
    if backend != "cpu" {
        return Ok(());
    }
    let info = result["system_info"]
        .as_str()
        .context("sum_eval result has no system_info")?;
    ensure!(
        !info.contains("CUDA"),
        "a CPU sum-eval ran on a llama.cpp build with CUDA (op offload would put prompt \
         batches on the GPU): {info}"
    );
    Ok(())
}

/// Whether `id` names the Claude Code candidate (`claude-code` or `claude-code/<model>`).
#[must_use]
pub fn is_claude_code_id(id: &str) -> bool {
    AgentCli::of_id(id) == Some(AgentCli::ClaudeCode)
}

/// Whether `id` names the Codex candidate (`codex` or `codex/<model>`).
#[must_use]
pub fn is_codex_id(id: &str) -> bool {
    AgentCli::of_id(id) == Some(AgentCli::Codex)
}

/// The `-m` Codex is asked for by candidate `id` (`codex`: its default, `None`;
/// `codex/<model>`: `<model>`, checked with `earshot_llm::codex::check_model`).
///
/// # Errors
/// Not a Codex id, or a model name the provider refuses.
pub fn codex_model(id: &str) -> Result<Option<String>> {
    AgentCli::Codex.model(id)
}

/// The `--model` Claude Code is asked for by candidate `id` (`claude-code`: its default,
/// `None`; `claude-code/<model>`: `<model>`, checked with
/// `earshot_llm::claude_code::check_model`, the production rule).
///
/// # Errors
/// Not a Claude Code id, or a model name the provider refuses.
pub fn claude_code_model(id: &str) -> Result<Option<String>> {
    AgentCli::ClaudeCode.model(id)
}

/// The result file name of a candidate on a backend: `<id>-<backend>.json`, with every
/// character of `id` that is not ASCII alphanumeric, `-`, `.` or `_` written `_` (a
/// `claude-code/<model>` id holds `/`, and a model name may hold `:`).
#[must_use]
pub fn result_file_name(id: &str, backend: &str) -> String {
    let stem: String = id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '.' | '_') {
                c
            } else {
                '_'
            }
        })
        .collect();
    format!("{stem}-{backend}.json")
}

/// Refuses a Claude Code run that is not the T7.4gb shape: the candidate alone, only the
/// public AMI reference transcripts (`--source reference`: Earshot's replays and private
/// data are never handed to Claude Code), the production plan and Claude Code's own
/// budget (no `--two-call`, `--two-track`, `--context`,
/// `--max-output`, `--threads` or `--backend`), and no replay or prompt export/import.
/// `--chunk-seconds` is allowed: the "chunks during the meeting" condition.
///
/// # Errors
/// What the run would do that a Claude Code run must not.
pub fn check_claude_code(opts: &Options) -> Result<()> {
    check_agent_cli(opts, AgentCli::ClaudeCode)
}

/// Refuses an agent-CLI run that is not the T7.4gb shape ([`check_claude_code`] for Claude
/// Code; the same rules for Codex, T7.4gc). `--codex-cli` only with `--model codex`.
///
/// # Errors
/// What the run would do that an agent-CLI run must not.
pub fn check_agent_cli(opts: &Options, agent: AgentCli) -> Result<()> {
    let id = agent.id();
    ensure!(
        opts.models.len() == 1,
        "--model {id} runs alone (not with {:?})",
        opts.models
    );
    agent.model(&opts.models[0])?;
    ensure!(
        !opts.sources.is_empty() && opts.sources.iter().all(|s| s == "reference"),
        "--model {id} summarises only the public AMI reference transcripts: pass \
         --source reference"
    );
    let defaults = Options::default();
    // A prompt variant is allowed (T12.17, evaluation only): the rules change, the plan
    // does not, and the format key names the variant.
    ensure!(
        !opts.two_call && !opts.two_track,
        "--model {id} runs the production plan (no --two-call or --two-track)"
    );
    ensure!(
        opts.context == defaults.context
            && opts.max_output == defaults.max_output
            && opts.threads.is_none()
            && opts.backend.is_none(),
        "--context, --max-output, --threads and --backend do not apply to --model {id} \
         (the provider's production budget, backend {id})"
    );
    ensure!(
        !opts.stored_only() && !opts.replay_only && !opts.rerun_earshot,
        "--model {id} does not replay, export or import"
    );
    ensure!(
        opts.codex_cli.is_none() || agent == AgentCli::Codex,
        "--codex-cli is for --model codex only"
    );
    ensure!(
        opts.thinking.is_none(),
        "--thinking is for local Qwen candidates only, not --model {id}"
    );
    Ok(())
}

/// The arguments of the `sum_eval` example for the Claude Code candidate `id`.
#[must_use]
#[expect(
    clippy::too_many_arguments,
    reason = "each is one example argument; a struct would only rename them"
)]
pub fn claude_code_args(
    annotations: &Path,
    jobs: &[(String, String)],
    meetings: &[(String, Vec<String>)],
    id: &str,
    model: Option<&str>,
    chunk_seconds: Option<u32>,
    prompt_variant: PromptVariant,
    out: &Path,
) -> Vec<OsString> {
    agent_cli_args(
        AgentCli::ClaudeCode,
        annotations,
        jobs,
        meetings,
        id,
        model,
        chunk_seconds,
        prompt_variant,
        StylePreset::Standard,
        None,
        out,
    )
}

/// The arguments of the `sum_eval` example for the agent-CLI candidate `id` of `agent`;
/// `codex_cli` names the Codex CLI (`--codex-cli`, Codex only).
#[must_use]
#[expect(
    clippy::too_many_arguments,
    reason = "each is one example argument; a struct would only rename them"
)]
pub fn agent_cli_args(
    agent: AgentCli,
    annotations: &Path,
    jobs: &[(String, String)],
    meetings: &[(String, Vec<String>)],
    id: &str,
    model: Option<&str>,
    chunk_seconds: Option<u32>,
    prompt_variant: PromptVariant,
    style_preset: StylePreset,
    codex_cli: Option<&Path>,
    out: &Path,
) -> Vec<OsString> {
    let mut a: Vec<OsString> = vec!["--annotations".into(), annotations.into()];
    for (split, source) in jobs {
        a.push("--job".into());
        a.push(format!("{split}:{source}").into());
    }
    for (split, list) in meetings {
        a.push("--meetings".into());
        a.push(format!("{split}={}", list.join(",")).into());
    }
    a.extend([agent.engine_flag().into(), "--model-id".into(), id.into()]);
    if let Some(m) = model {
        a.extend([agent.model_flag().into(), m.into()]);
    }
    if let (AgentCli::Codex, Some(p)) = (agent, codex_cli) {
        a.extend(["--codex-cli".into(), p.into()]);
    }
    if let Some(s) = chunk_seconds {
        a.extend(["--chunk-seconds".into(), s.to_string().into()]);
    }
    if !prompt_variant.is_current() {
        a.extend(["--prompt-variant".into(), prompt_variant.name().into()]);
    }
    if !style_preset.is_standard() {
        a.extend(["--style-preset".into(), style_preset.name().into()]);
    }
    a.extend(["--out".into(), out.into()]);
    a
}

/// Refuses to overwrite a finished result file `stored` that this run's report would
/// replace while it holds another format key ([`other_format`]), other settings
/// ([`other_settings`]) or meetings the run would drop ([`dropped_meetings`]).
fn check_overwrite(
    file: &Path,
    stored: &Value,
    jobs: &[(String, String)],
    meetings: &[(String, Vec<String>)],
    opts: &Options,
) -> Result<()> {
    if let Some(other) = other_format(stored, opts) {
        bail!(
            "{} holds a run with format key {other}, not {}; use another --out",
            file.display(),
            opts.format_key()
        );
    }
    if let Some(other) = other_settings(stored, opts) {
        bail!(
            "{} holds a run with other settings ({other}); use another --out",
            file.display()
        );
    }
    let dropped = dropped_meetings(stored, jobs, meetings);
    ensure!(
        dropped.is_empty(),
        "{} holds meetings this run would drop ({}); use another --out",
        file.display(),
        dropped.join(", ")
    );
    Ok(())
}

/// The Claude Code run (T7.4gb, [`check_claude_code`]): builds the `sum_eval` example
/// (plain release build: llama.cpp is linked but not used), runs it once with
/// `--claude-code` and writes `<out>/report.json` and `<out>/table.md` as [`run`] does. A
/// finished result file is reused, re-scored with `--rescore`, or protected as for the
/// local candidates. Backend label `claude-code`.
///
/// # Errors
/// A refused option, missing fixtures, a failed build or evaluation.
pub fn run_claude_code(root: &Path, opts: &Options) -> Result<()> {
    run_agent_cli(root, opts, AgentCli::ClaudeCode)
}

/// The run of an agent-CLI candidate ([`run_claude_code`] for Claude Code; the same for
/// Codex, T7.4gc, backend label `codex`, with `--codex-cli` passed on).
///
/// # Errors
/// A refused option, missing fixtures, a failed build or evaluation.
pub fn run_agent_cli(root: &Path, opts: &Options, agent: AgentCli) -> Result<()> {
    check_agent_cli(opts, agent)?;
    let id = opts.models[0].clone();
    let model = agent.model(&id)?;
    let backend = agent.id();
    let jobs = jobs(&opts.splits, &opts.sources)?;
    let fixtures = crate::fixtures::cache_dir()?;
    let annotations = fixtures.join(AMI_ANNOTATIONS_DIR);
    ensure!(
        annotations.join("abstractive").is_dir(),
        "{} has no abstractive summaries; run `cargo xtask fetch-fixtures --set ami-sum`",
        annotations.display()
    );
    let out = opts
        .out
        .clone()
        .unwrap_or_else(|| root.join("target").join("sum-eval"));
    std::fs::create_dir_all(&out)?;
    let meetings = run_meetings(&jobs, &opts.meetings, opts.limit)?;
    let target_dir = crate::build::short_target_dir(
        root,
        opts.target_dir.as_deref(),
        std::env::var_os("SystemDrive"),
        None,
    );
    let msgs = build(
        root,
        &build_args(
            "earshot-llm-local",
            &["--example", "sum_eval"],
            None,
            &target_dir,
        ),
        "the sum_eval example",
    )?;
    let built = executable(&msgs, "sum_eval").context("cargo reported no sum_eval")?;
    let exe = out.join(format!("sum_eval-{}.exe", std::process::id()));
    std::fs::copy(&built, &exe)
        .with_context(|| format!("copy {} to {}", built.display(), exe.display()))?;
    let dll = llama_bin_dir(&msgs).context("cargo reported no llama-cpp-sys-2 build output")?;
    let path = path_with(&[dll])?;
    let file = out.join(result_file_name(&id, backend));
    let result = (|| -> Result<Value> {
        let stored = std::fs::read(&file)
            .ok()
            .and_then(|b| serde_json::from_slice::<Value>(&b).ok());
        if opts.rescore {
            ensure!(file.is_file(), "no {} to re-score", file.display());
            let status = Command::new(&exe)
                .args(rescore_args(&annotations, &file))
                .env("PATH", &path)
                .status()?;
            ensure!(
                status.success(),
                "re-scoring {} failed ({status})",
                file.display()
            );
            return Ok(serde_json::from_slice(&std::fs::read(&file)?)?);
        }
        if let Some(v) = stored
            .as_ref()
            .filter(|v| covers(v, &jobs, &meetings, opts))
        {
            eprintln!("sum-eval: {id}: reusing {}", file.display());
            return Ok(v.clone());
        }
        if let Some(v) = &stored {
            check_overwrite(&file, v, &jobs, &meetings, opts)?;
        }
        let status = Command::new(&exe)
            .args(agent_cli_args(
                agent,
                &annotations,
                &jobs,
                &meetings,
                &id,
                model.as_deref(),
                opts.chunk_seconds,
                opts.prompt_variant,
                opts.style_preset,
                opts.codex_cli.as_deref(),
                &file,
            ))
            .env("PATH", &path)
            .status()
            .with_context(|| format!("start {}", exe.display()))?;
        ensure!(status.success(), "sum_eval failed for {id} ({status})");
        let v: Value = serde_json::from_slice(&std::fs::read(&file)?)?;
        ensure!(
            v["backend"] == backend,
            "{} records backend {}, not {backend}",
            file.display(),
            v["backend"]
        );
        Ok(v)
    })();
    let _ = std::fs::remove_file(&exe);
    let mut v = result?;
    v["name"] = json!(agent.display_name(model.as_deref()));
    let report = json!({
        "task": agent.task(),
        "backend": backend,
        "context_tokens": v["context_tokens"],
        "max_output_tokens": v["max_output_tokens"],
        "meetings": meetings.iter().map(|(s, m)| json!({"split": s, "meetings": m})).collect::<Vec<_>>(),
        "replays": [],
        "candidates": [v],
        "remote": format!("not run ({id} run)"),
    });
    let report_file = out.join("report.json");
    std::fs::write(&report_file, serde_json::to_vec_pretty(&report)?)?;
    let t = table(&report);
    std::fs::write(out.join("table.md"), &t)?;
    println!("{t}");
    eprintln!("sum-eval: report in {}", report_file.display());
    Ok(())
}

/// Refuses a `--thinking` run (T12.10) that is not a local summary run of candidates whose
/// prompt ends with the empty thinking block (the Qwen 3 and 3.5 candidates), or that adds
/// `--two-call` or `--two-track`. The cap's own range is checked by the example.
///
/// # Errors
/// What the run would do that a thinking run must not.
pub fn check_thinking(opts: &Options) -> Result<()> {
    if opts.thinking.is_none() {
        return Ok(());
    }
    ensure!(
        !(opts.stored_only() || opts.rescore || opts.replay_only),
        "--thinking applies to local summary runs only"
    );
    ensure!(
        !opts.two_call && !opts.two_track,
        "--thinking is not combined with --two-call or --two-track"
    );
    let candidates = select_candidates(&opts.models)?;
    let others: Vec<&str> = candidates
        .iter()
        .filter(|c| c.prefix != QWEN_NO_THINK)
        .map(|c| c.id)
        .collect();
    ensure!(
        others.is_empty(),
        "--thinking needs candidates whose prompt ends with the empty thinking block (Qwen 3 \
         and 3.5); pass --model with those only, not {}",
        others.join(", ")
    );
    Ok(())
}

/// `cargo xtask sum-eval`.
///
/// # Errors
/// Backend selection, missing fixtures or models, a failed build, replay or evaluation.
pub fn run(root: &Path, opts: &Options) -> Result<()> {
    if let Some(agent) = opts.agent_cli() {
        return run_agent_cli(root, opts, agent);
    }
    ensure!(
        opts.codex_cli.is_none(),
        "--codex-cli is for --model codex only"
    );
    ensure!(
        opts.chunk_seconds.is_none() || !(opts.stored_only() || opts.rescore),
        "--chunk-seconds applies to local summary runs only"
    );
    ensure!(
        opts.prompt_variant.is_current() || !(opts.stored_only() || opts.rescore),
        "--prompt-variant applies to local summary runs only"
    );
    ensure!(
        opts.style_preset.is_standard() || !(opts.stored_only() || opts.rescore),
        "--style-preset applies to summary runs only"
    );
    ensure!(
        !opts.two_call || (opts.chunk_seconds.is_some() && !(opts.stored_only() || opts.rescore)),
        "--two-call needs --chunk-seconds and applies to local summary runs only"
    );
    ensure!(
        !opts.two_track
            || (opts.chunk_seconds.is_some()
                && !opts.two_call
                && !(opts.stored_only() || opts.rescore)
                && opts.sources.iter().all(|s| s == "reference")
                && !opts.sources.is_empty()),
        "--two-track needs --chunk-seconds and --source reference, not --two-call, and applies to local summary runs only"
    );
    check_thinking(opts)?;
    let report = crate::doctor::collect();
    let cuda = crate::doctor::cuda_for_backend_choice(&report, opts.backend.as_deref())?;
    // Vulkan (T8.3h) applies to the summaries only: Earshot-source replays are built as
    // before (CUDA when the host has it).
    let backend = match opts.backend.as_deref() {
        Some("vulkan") => "vulkan",
        b => crate::accept::m2::select_backend(b, cuda)?,
    };
    let gpu = backend != "cpu";
    let jobs = jobs(&opts.splits, &opts.sources)?;
    let candidates = select_candidates(&opts.models)?;
    let fixtures = crate::fixtures::cache_dir()?;
    let annotations = fixtures.join(AMI_ANNOTATIONS_DIR);
    ensure!(
        annotations.join("abstractive").is_dir(),
        "{} has no abstractive summaries; run `cargo xtask fetch-fixtures --set ami-sum`",
        annotations.display()
    );
    let out = opts
        .out
        .clone()
        .unwrap_or_else(|| root.join("target").join("sum-eval"));
    std::fs::create_dir_all(&out)?;
    let models_dir = crate::models::cache_dir()?;
    let manifest = Manifest::load(&crate::models::manifest_path(root))?;
    let mut paths = Vec::new();
    for c in &candidates {
        let a = manifest
            .artefacts
            .iter()
            .find(|a| a.id == c.id)
            .with_context(|| format!("{} is not in models/manifest.toml", c.id))?;
        ensure!(
            a.sets.iter().any(|s| s == "llm-eval"),
            "{} is not in the llm-eval set",
            c.id
        );
        let p = models_dir.join(&a.path);
        let size = std::fs::metadata(&p).map(|m| m.len()).ok();
        ensure!(
            opts.rescore || opts.stored_only() || size == Some(a.size),
            "{} missing or incomplete; run `cargo xtask fetch-models --set llm-eval`",
            p.display()
        );
        paths.push(p);
    }
    let meetings = run_meetings(&jobs, &opts.meetings, opts.limit)?;
    // A short target directory: CUDA builds of whisper.cpp fail CMake's probe in a deep
    // worktree path (AGENTS.md).
    let target_dir = crate::build::short_target_dir(
        root,
        opts.target_dir.as_deref(),
        std::env::var_os("SystemDrive"),
        None,
    );

    let mut replays = Vec::new();
    let earshot_dir = out.join("earshot");
    if jobs.iter().any(|(_, s)| s == "earshot") && !opts.rescore && !opts.stored_only() {
        let msgs = build(
            root,
            &build_args(
                "earshot-cli",
                &["--bin", "earshot-cli"],
                cuda.then_some("cuda"),
                &target_dir,
            ),
            "the earshot CLI",
        )?;
        let built =
            executable(&msgs, "earshot-cli").context("cargo reported no earshot-cli binary")?;
        let exe = out.join(format!("earshot-{}.exe", std::process::id()));
        std::fs::copy(&built, &exe)
            .with_context(|| format!("copy {} to {}", built.display(), exe.display()))?;
        // The copy runs outside cargo: llama.cpp's DLLs go beside it (T6.21b).
        crate::llama_dlls::install_beside(&msgs, &exe)?;
        let test = meetings
            .iter()
            .find(|(s, _)| s == "test")
            .map(|(_, m)| m.clone())
            .unwrap_or_default();
        replays = replay_meetings(&exe, &test, &fixtures, &out, cuda, opts.rerun_earshot)?;
        let _ = std::fs::remove_file(&exe);
    }
    if opts.replay_only {
        eprintln!(
            "sum-eval: {} replay(s) ready in {}",
            replays.len(),
            earshot_dir.display()
        );
        return Ok(());
    }

    let msgs = build(
        root,
        &build_args(
            "earshot-llm-local",
            &["--example", "sum_eval"],
            example_feature(backend),
            &target_dir,
        ),
        "the sum_eval example",
    )?;
    let built = executable(&msgs, "sum_eval").context("cargo reported no sum_eval")?;
    // Run a copy: a later build (another sum-eval in parallel) can then relink the example
    // while this evaluation runs.
    let exe = out.join(format!("sum_eval-{}.exe", std::process::id()));
    std::fs::copy(&built, &exe)
        .with_context(|| format!("copy {} to {}", built.display(), exe.display()))?;
    let dll = llama_bin_dir(&msgs).context("cargo reported no llama-cpp-sys-2 build output")?;
    let path = path_with(&[dll])?;
    let mut results = Vec::new();
    if let Some(dir) = &opts.export_prompts {
        let mut args = input_args(&annotations, &earshot_dir, &jobs, &meetings);
        args.extend(["--export-prompts".into(), dir.as_os_str().to_owned()]);
        let status = Command::new(&exe).args(args).env("PATH", &path).status()?;
        let _ = std::fs::remove_file(&exe);
        ensure!(status.success(), "exporting the prompts failed ({status})");
        eprintln!("sum-eval: prompts in {}", dir.display());
        return Ok(());
    }
    if let Some(dir) = &opts.import_replies {
        let label = opts
            .label
            .as_deref()
            .context("--import-replies needs --label")?;
        let file = out.join(format!("{label}.json"));
        let mut args = input_args(&annotations, &earshot_dir, &jobs, &meetings);
        args.extend([
            "--import-replies".into(),
            dir.as_os_str().to_owned(),
            "--label".into(),
            label.into(),
            "--out".into(),
            file.as_os_str().to_owned(),
        ]);
        let status = Command::new(&exe).args(args).env("PATH", &path).status()?;
        ensure!(status.success(), "importing the replies failed ({status})");
        let mut v: Value = serde_json::from_slice(&std::fs::read(&file)?)?;
        v["name"] = json!(label);
        results.push(v);
    }
    for (c, p) in candidates
        .iter()
        .zip(&paths)
        .filter(|_| opts.import_replies.is_none())
    {
        let file = out.join(result_file_name(c.id, backend));
        if opts.rescore {
            // Stored summaries only: a candidate without a finished file is left out.
            if !file.is_file() {
                eprintln!("sum-eval: {}: no {} to re-score", c.id, file.display());
                continue;
            }
            let status = Command::new(&exe)
                .args(rescore_args(&annotations, &file))
                .env("PATH", &path)
                .status()
                .with_context(|| format!("start {}", exe.display()))?;
            ensure!(
                status.success(),
                "re-scoring {} failed ({status})",
                file.display()
            );
            let mut v: Value = serde_json::from_slice(&std::fs::read(&file)?)?;
            v["name"] = json!(c.name);
            results.push(v);
            continue;
        }
        if let Some(mut v) = std::fs::read(&file)
            .ok()
            .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
            .filter(|v| covers(v, &jobs, &meetings, opts))
        {
            eprintln!("sum-eval: {}: reusing {}", c.id, file.display());
            v["name"] = json!(c.name);
            results.push(v);
            continue;
        }
        if let Some(stored) = std::fs::read(&file)
            .ok()
            .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
        {
            check_overwrite(&file, &stored, &jobs, &meetings, opts)?;
        }
        let status = Command::new(&exe)
            .args(eval_args(
                &annotations,
                &earshot_dir,
                &jobs,
                &meetings,
                Some((p, c)),
                gpu,
                opts,
                &file,
            ))
            .env("PATH", &path)
            .status()
            .with_context(|| format!("start {}", exe.display()))?;
        ensure!(status.success(), "sum_eval failed for {} ({status})", c.id);
        let mut v: Value = serde_json::from_slice(&std::fs::read(&file)?)?;
        check_cpu_only(backend, &v)?;
        check_gpu_backend(backend, &v)?;
        v["name"] = json!(c.name);
        results.push(v);
    }
    let remote_file = out.join("remote.json");
    let remote = if opts.rescore {
        if remote_file.is_file() {
            let status = Command::new(&exe)
                .args(rescore_args(&annotations, &remote_file))
                .env("PATH", &path)
                .status()?;
            ensure!(
                status.success(),
                "re-scoring {} failed",
                remote_file.display()
            );
            serde_json::from_slice(&std::fs::read(&remote_file)?)?
        } else {
            json!("not run (EARSHOT_LLM_URL is not set)")
        }
    } else if std::env::var_os("EARSHOT_LLM_URL").is_some_and(|v| !v.is_empty()) {
        let file = out.join("remote.json");
        let status = Command::new(&exe)
            .args(eval_args(
                &annotations,
                &earshot_dir,
                &jobs,
                &meetings,
                None,
                false,
                opts,
                &file,
            ))
            .env("PATH", &path)
            .status()?;
        ensure!(
            status.success(),
            "sum_eval failed for the remote endpoint ({status})"
        );
        serde_json::from_slice(&std::fs::read(&file)?)?
    } else {
        json!("not run (EARSHOT_LLM_URL is not set)")
    };
    // The per-run copy of the example is no longer needed.
    let _ = std::fs::remove_file(&exe);
    let report = json!({
        "task": "T6.20",
        "backend": backend,
        "context_tokens": opts.context,
        "max_output_tokens": opts.max_output,
        "asr_model": ASR_MODEL,
        "meetings": meetings.iter().map(|(s, m)| json!({"split": s, "meetings": m})).collect::<Vec<_>>(),
        "replays": replays,
        "candidates": results,
        "remote": remote,
    });
    // Imported replies get their own report beside the local candidates' one.
    let stem = match (&opts.import_replies, &opts.label) {
        (Some(_), Some(label)) => format!("{label}."),
        _ => String::new(),
    };
    let report_file = out.join(format!("{stem}report.json"));
    std::fs::write(&report_file, serde_json::to_vec_pretty(&report)?)?;
    let t = table(&report);
    std::fs::write(out.join(format!("{stem}table.md")), &t)?;
    println!("{t}");
    eprintln!("sum-eval: report in {}", report_file.display());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sum_eval_jobs_put_the_earshot_source_on_the_test_split_only() {
        let all = jobs(&[], &[]).unwrap();
        assert_eq!(
            all,
            [
                ("dev".to_string(), "reference".to_string()),
                ("test".into(), "reference".into()),
                ("test".into(), "earshot".into())
            ]
        );
        assert!(jobs(&["dev".into()], &["earshot".into()]).is_err());
        assert!(jobs(&["train".into()], &[]).is_err());
        assert!(jobs(&[], &["asr".into()]).is_err());
    }

    #[test]
    fn sum_eval_gemma4_prefix_does_not_repeat_the_formatters_thought_channel() {
        let gemma = CANDIDATES
            .iter()
            .find(|c| c.id.starts_with("gemma-4"))
            .expect("a Gemma 4 candidate");
        assert_eq!(
            gemma.prefix, "",
            "chat_format::gemma4 already opens the thought channel"
        );
        for c in &CANDIDATES {
            assert!(!c.prefix.contains("<|channel>"), "{}: {:?}", c.id, c.prefix);
        }
    }

    #[test]
    fn sum_eval_granite_candidates_run_with_thinking_off() {
        // Granite 4.2 thinks by default; its template with thinking off opens the reply
        // with an empty think block, which is the screened condition (T12.27).
        for id in ["granite-4.2-3b-q4_k_m", "granite-4.2-8b-q4_k_m"] {
            let c = CANDIDATES
                .iter()
                .find(|c| c.id == id)
                .unwrap_or_else(|| panic!("{id} is a candidate"));
            assert_eq!(c.prefix, "<think></think>", "{id}");
        }
    }

    #[test]
    fn sum_eval_ornith_runs_with_qwens_thinking_off_block() {
        let c = CANDIDATES
            .iter()
            .find(|c| c.id == "ornith-1.5-9b-q4_k_m")
            .expect("Ornith is a candidate");
        assert_eq!(c.prefix, "<think>\n\n</think>\n\n");
    }

    #[test]
    fn sum_eval_instruct_2507_candidates_have_no_thinking_prefix() {
        // The Qwen3 2507 Instruct releases are non-thinking: their ChatML template renders
        // nothing after the assistant header, so an empty think block would be extra text.
        for id in [
            "qwen3-4b-instruct-2507-q4_k_m",
            "qwen3-30b-a3b-instruct-2507-q4_k_m",
        ] {
            let c = CANDIDATES
                .iter()
                .find(|c| c.id == id)
                .unwrap_or_else(|| panic!("{id} is a candidate"));
            assert_eq!(c.prefix, "", "{id}");
        }
    }

    #[test]
    fn sum_eval_candidates_are_the_llm_eval_set() {
        let m = Manifest::load(&crate::models::manifest_path(&crate::repo_root())).unwrap();
        let mut set: Vec<&str> = m
            .artefacts
            .iter()
            .filter(|a| a.sets.iter().any(|s| s == "llm-eval"))
            .map(|a| a.id.as_str())
            .collect();
        set.sort_unstable();
        let mut ours: Vec<&str> = CANDIDATES.iter().map(|c| c.id).collect();
        ours.sort_unstable();
        assert_eq!(set, ours);
        assert_eq!(select_candidates(&[]).unwrap().len(), CANDIDATES.len());
        assert_eq!(
            select_candidates(&["qwen3-8b-q4_k_m".into()]).unwrap()[0].name,
            "Qwen3 8B"
        );
        assert!(select_candidates(&["llama".into()]).is_err());
    }

    #[test]
    fn sum_eval_the_project_manager_is_the_mic() {
        let xml = r#"<meeting nite:id="meet_49" observation="ES2004a" duration="1186">
      <speaker nite:id="ES2004a_2" channel="1" nxt_agent="B" role="PM"/>
      <speaker nite:id="ES2004a_3" channel="2" nxt_agent="C" role="ID"/>
      <speaker nite:id="ES2004a_1" channel="0" nxt_agent="A" role="UI"/>
      <speaker nite:id="ES2004a_4" channel="3" nxt_agent="D" role="ME"/>
   </meeting>"#;
        let roles = ami_headset_roles(xml, "ES2004a").unwrap();
        assert_eq!(roles[1], (1, "PM".to_string()));
        assert_eq!(call_channels(&roles).unwrap(), (1, vec![0, 2, 3]));
        let two_pm = xml.replace("role=\"ID\"", "role=\"PM\"");
        assert!(call_channels(&ami_headset_roles(&two_pm, "ES2004a").unwrap()).is_err());
        assert!(ami_headset_roles(xml, "IS1009a").is_err());
    }

    #[test]
    fn sum_eval_wav_mix_round_trips_and_clips() {
        let a = vec![1000i16, -2000, 30_000];
        let b = vec![500i16, 500, 30_000, 7];
        let m = mix(&[a, b]);
        assert_eq!(m, [1500, -1500, i16::MAX, 7]);
        let bytes = wav_bytes(&m);
        assert_eq!(read_wav_16k_mono(&bytes).unwrap(), m);
        // Wrong rate or channel count is refused.
        let mut stereo = bytes.clone();
        stereo[22] = 2;
        assert!(read_wav_16k_mono(&stereo).is_err());
        let mut rate = bytes;
        rate[24..28].copy_from_slice(&8000u32.to_le_bytes());
        assert!(read_wav_16k_mono(&rate).is_err());
        assert!(read_wav_16k_mono(b"RIFF").is_err());
    }

    #[test]
    fn sum_eval_finds_executables_and_the_llama_dlls() {
        let msgs = concat!(
            r#"{"reason":"build-script-executed","package_id":"registry+https://github.com/rust-lang/crates.io-index#llama-cpp-sys-2@0.1.160","out_dir":"C:/t/build/llama-cpp-sys-2-abc/out"}"#,
            "\n",
            r#"{"reason":"compiler-artifact","target":{"name":"sum_eval"},"executable":"C:/t/sum_eval.exe"}"#,
            "\n",
        );
        assert_eq!(
            executable(msgs, "sum_eval"),
            Some(PathBuf::from("C:/t/sum_eval.exe"))
        );
        assert_eq!(executable(msgs, "earshot"), None);
        assert_eq!(
            llama_bin_dir(msgs),
            Some(PathBuf::from("C:/t/build/llama-cpp-sys-2-abc/out").join("bin"))
        );
        assert!(
            build_args(
                "earshot-cli",
                &["--bin", "earshot-cli"],
                Some("cuda"),
                Path::new("C:/b")
            )
            .windows(2)
            .any(|w| w == ["--features", "cuda"])
        );
        assert!(
            !build_args("earshot-cli", &[], None, Path::new("C:/b"))
                .iter()
                .any(|a| a == "cuda" || a == "--features")
        );
    }

    #[test]
    fn sum_eval_cpu_runs_need_a_build_without_cuda() {
        assert_eq!(example_feature("cuda"), Some("cuda"));
        assert_eq!(example_feature("cpu"), None);
        let cuda_build = json!({"system_info": "CUDA : ARCHS = 1200 | CPU : AVX2 = 1 |"});
        let cpu_build = json!({"system_info": "CPU : SSE3 = 1 | AVX2 = 1 |"});
        assert!(check_cpu_only("cpu", &cuda_build).is_err());
        assert!(check_cpu_only("cpu", &cpu_build).is_ok());
        assert!(check_cpu_only("cuda", &cuda_build).is_ok());
        assert!(check_cpu_only("cpu", &json!({})).is_err());
    }

    #[test]
    fn sum_eval_vulkan_runs_build_vulkan_and_must_load_on_vulkan() {
        // T8.3h: llama.cpp on the Snapdragon's Adreno GPU through Vulkan.
        assert_eq!(example_feature("vulkan"), Some("vulkan"));
        assert!(
            build_args(
                "earshot-llm-local",
                &[],
                example_feature("vulkan"),
                Path::new("C:/b")
            )
            .windows(2)
            .any(|w| w == ["--features", "vulkan"])
        );
        assert!(check_gpu_backend("vulkan", &json!({"backend": "vulkan"})).is_ok());
        assert!(check_gpu_backend("vulkan", &json!({"backend": "cpu"})).is_err());
        assert!(check_gpu_backend("vulkan", &json!({})).is_err());
        // Other backends are judged elsewhere (`check_cpu_only`, the loader's own check).
        assert!(check_gpu_backend("cpu", &json!({"backend": "cpu"})).is_ok());
        assert!(check_gpu_backend("cuda", &json!({})).is_ok());
    }

    #[test]
    fn sum_eval_reuses_only_a_report_with_the_same_jobs_and_settings() {
        let opts = Options::default();
        let jobs = [("dev".to_string(), "reference".to_string())];
        let meetings = [("dev".to_string(), vec!["ES2003a".to_string()])];
        let v = json!({
            "grammar": "indented-numbered-r5",
            "context_tokens": 16384,
            "max_output_tokens": 8192,
            "jobs": [{"split": "dev", "source": "reference", "meetings": [{"meeting": "ES2003a"}]}],
        });
        assert!(covers(&v, &jobs, &meetings, &opts));
        let more = [(
            "dev".to_string(),
            vec!["ES2003a".to_string(), "ES2003b".to_string()],
        )];
        assert!(!covers(&v, &jobs, &more, &opts));
        let other = Options {
            context: 8192,
            ..Options::default()
        };
        assert!(!covers(&v, &jobs, &meetings, &other));
        let two = [jobs[0].clone(), ("test".into(), "earshot".into())];
        assert!(!covers(&v, &two, &meetings, &opts));
        assert!(!covers(&json!({}), &jobs, &meetings, &opts));
        // A finished report without the grammar field (older scoring) is not reused: its
        // rows are re-scored through the checkpoint instead.
        let mut old = v.clone();
        old.as_object_mut().unwrap().remove("grammar");
        assert!(!covers(&old, &jobs, &meetings, &opts));
        // Chunked and unchunked reports are never reused for each other.
        let chunked = Options {
            chunk_seconds: Some(600),
            ..Options::default()
        };
        assert!(!covers(&v, &jobs, &meetings, &chunked));
        let mut c = v.clone();
        c["grammar"] = json!("indented-numbered-r5-chunk600");
        assert!(covers(&c, &jobs, &meetings, &chunked));
        assert!(!covers(&c, &jobs, &meetings, &opts));
        let other = Options {
            chunk_seconds: Some(300),
            ..Options::default()
        };
        assert!(!covers(&c, &jobs, &meetings, &other));
    }

    #[test]
    fn sum_eval_prompt_variants_have_their_own_format_key_and_never_share_reports() {
        assert_eq!(
            format_key(None, PromptVariant::Current, false, false, None, None),
            "indented-numbered-r5"
        );
        assert_eq!(
            format_key(Some(600), PromptVariant::Current, false, false, None, None),
            "indented-numbered-r5-chunk600"
        );
        assert_eq!(
            format_key(Some(600), PromptVariant::Items, false, false, None, None),
            "indented-numbered-r5-chunk600-prompt-items"
        );
        let keys: std::collections::BTreeSet<String> = PromptVariant::ALL
            .into_iter()
            .map(|v| format_key(Some(600), v, false, false, None, None))
            .collect();
        assert_eq!(keys.len(), PromptVariant::ALL.len());

        let jobs = [("dev".to_string(), "reference".to_string())];
        let meetings = [("dev".to_string(), vec!["ES2003a".to_string()])];
        let current = Options {
            chunk_seconds: Some(600),
            ..Options::default()
        };
        let lean = Options {
            prompt_variant: PromptVariant::Lean,
            ..current.clone()
        };
        let control = json!({
            "grammar": "indented-numbered-r5-chunk600",
            "context_tokens": 16384,
            "max_output_tokens": 8192,
            "jobs": [{"split": "dev", "source": "reference", "meetings": [{"meeting": "ES2003a"}]}],
        });
        assert!(covers(&control, &jobs, &meetings, &current));
        assert!(!covers(&control, &jobs, &meetings, &lean));
        let mut variant = control.clone();
        variant["grammar"] = json!(format_key(
            Some(600),
            PromptVariant::Lean,
            false,
            false,
            None,
            None
        ));
        assert!(covers(&variant, &jobs, &meetings, &lean));
        assert!(!covers(&variant, &jobs, &meetings, &current));
        // A variant run never overwrites the control's file, nor the reverse.
        assert_eq!(other_format(&control, &current), None);
        assert_eq!(
            other_format(&control, &lean).as_deref(),
            Some("indented-numbered-r5-chunk600")
        );
        assert!(other_format(&variant, &current).is_some());
        assert_eq!(other_format(&json!({}), &lean), None);
    }

    #[test]
    fn sum_eval_two_call_has_its_own_format_key_args_and_label() {
        assert_eq!(
            format_key(Some(600), PromptVariant::Current, true, false, None, None),
            "indented-numbered-r5-chunk600-twocall"
        );
        assert_eq!(
            format_key(Some(600), PromptVariant::Lean, true, false, None, None),
            "indented-numbered-r5-chunk600-twocall-prompt-lean"
        );
        let one = Options {
            chunk_seconds: Some(600),
            ..Options::default()
        };
        let two = Options {
            two_call: true,
            ..one.clone()
        };
        assert_eq!(two.format_key(), "indented-numbered-r5-chunk600-twocall");
        // A two-call run never reuses or overwrites a one-call file, nor the reverse.
        let jobs = [("dev".to_string(), "reference".to_string())];
        let meetings = [("dev".to_string(), vec!["ES2003c".to_string()])];
        let stored = |key: String| {
            json!({
                "grammar": key, "context_tokens": 16384, "max_output_tokens": 8192,
                "jobs": [{"split": "dev", "source": "reference", "meetings": [{"meeting": "ES2003c"}]}],
            })
        };
        let one_file = stored(one.format_key());
        let two_file = stored(two.format_key());
        assert!(covers(&two_file, &jobs, &meetings, &two));
        assert!(!covers(&one_file, &jobs, &meetings, &two));
        assert!(!covers(&two_file, &jobs, &meetings, &one));
        assert!(other_format(&one_file, &two).is_some());
        assert!(other_format(&two_file, &one).is_some());
        let c = CANDIDATES[2];
        let args = |opts: &Options| -> Vec<String> {
            eval_args(
                Path::new("a"),
                Path::new("e"),
                &[],
                &[],
                Some((Path::new("m.gguf"), &c)),
                true,
                opts,
                Path::new("o.json"),
            )
            .into_iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect()
        };
        assert!(args(&two).iter().any(|a| a == "--two-call"));
        assert!(!args(&one).iter().any(|a| a == "--two-call"));
        let report = json!({"candidates": [
            {"candidate": "qwen3.5-9b-q4_k_m", "two_call": true, "prompt_variant": "current", "jobs": [{"split": "dev", "source": "reference", "aggregate": {}}]},
        ]});
        assert!(table(&report).contains("| qwen3.5-9b-q4_k_m (two-call) |"));
    }

    #[test]
    fn sum_eval_two_track_has_its_own_format_key_args_and_label() {
        let one = Options {
            chunk_seconds: Some(600),
            sources: vec!["reference".into()],
            ..Options::default()
        };
        let two = Options {
            two_track: true,
            ..one.clone()
        };
        assert_eq!(two.format_key(), "indented-numbered-r5-chunk600-twotrack");
        assert_eq!(
            format_key(Some(600), PromptVariant::Lean, false, true, None, None),
            "indented-numbered-r5-chunk600-twotrack-prompt-lean"
        );
        // Neither reuses nor overwrites the other's file.
        let file = |key: String| {
            json!({
                "grammar": key, "context_tokens": 16384, "max_output_tokens": 8192,
                "jobs": [{"split": "dev", "source": "reference", "meetings": [{"meeting": "ES2003c"}]}],
            })
        };
        assert!(other_format(&file(one.format_key()), &two).is_some());
        assert!(other_format(&file(two.format_key()), &one).is_some());
        let c = CANDIDATES[2];
        let args = |opts: &Options| -> Vec<String> {
            eval_args(
                Path::new("a"),
                Path::new("e"),
                &[],
                &[],
                Some((Path::new("m.gguf"), &c)),
                true,
                opts,
                Path::new("o.json"),
            )
            .into_iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect()
        };
        assert!(args(&two).iter().any(|a| a == "--two-track"));
        assert!(!args(&one).iter().any(|a| a == "--two-track"));
        let report = json!({"candidates": [
            {"candidate": "qwen3.5-4b-q4_k_m", "two_track": true, "prompt_variant": "current", "jobs": [{"split": "dev", "source": "reference", "aggregate": {}}]},
        ]});
        assert!(table(&report).contains("| qwen3.5-4b-q4_k_m (two-track) |"));
    }

    #[test]
    fn sum_eval_thinking_has_its_own_format_key_args_label_and_refusals() {
        let off = Options {
            models: vec!["qwen3.5-9b-q4_k_m".into(), "qwen3.5-4b-q4_k_m".into()],
            splits: vec!["dev".into()],
            sources: vec!["reference".into()],
            chunk_seconds: Some(600),
            ..Options::default()
        };
        let on = Options {
            thinking: Some(2048),
            ..off.clone()
        };
        // Off keeps today's key; on appends `-think<cap>` after the agent suffix and before
        // the prompt variant, as the example does.
        assert_eq!(off.format_key(), "indented-numbered-r5-chunk600");
        assert_eq!(on.format_key(), "indented-numbered-r5-chunk600-think2048");
        assert_eq!(
            format_key(None, PromptVariant::Lean, false, false, None, Some(512)),
            "indented-numbered-r5-think512-prompt-lean"
        );
        assert_ne!(
            Options {
                thinking: Some(1024),
                ..off.clone()
            }
            .format_key(),
            on.format_key(),
            "another cap is another key"
        );
        // Neither reuses nor overwrites the other's file.
        let file = |key: String| {
            json!({
                "grammar": key, "context_tokens": 16384, "max_output_tokens": 8192,
                "jobs": [{"split": "dev", "source": "reference", "meetings": [{"meeting": "ES2003c"}]}],
            })
        };
        let jobs = [("dev".to_string(), "reference".to_string())];
        let meetings = [("dev".to_string(), vec!["ES2003c".to_string()])];
        assert!(covers(&file(on.format_key()), &jobs, &meetings, &on));
        assert!(!covers(&file(off.format_key()), &jobs, &meetings, &on));
        assert!(!covers(&file(on.format_key()), &jobs, &meetings, &off));
        let p = Path::new("r.json");
        let refused = check_overwrite(p, &file(off.format_key()), &jobs, &meetings, &on)
            .unwrap_err()
            .to_string();
        assert!(refused.contains("format key"), "{refused}");
        assert!(check_overwrite(p, &file(on.format_key()), &jobs, &meetings, &off).is_err());
        assert!(check_overwrite(p, &file(on.format_key()), &jobs, &meetings, &on).is_ok());
        // The cap reaches the example only when set.
        let c = CANDIDATES[7];
        assert_eq!(c.id, "qwen3.5-9b-q4_k_m");
        let args = |opts: &Options| -> Vec<String> {
            eval_args(
                Path::new("a"),
                Path::new("e"),
                &[],
                &[],
                Some((Path::new("m.gguf"), &c)),
                true,
                opts,
                Path::new("o.json"),
            )
            .into_iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect()
        };
        assert!(args(&on).windows(2).any(|w| w == ["--thinking", "2048"]));
        assert!(!args(&off).iter().any(|a| a == "--thinking"));
        let report = json!({"candidates": [
            {"candidate": "qwen3.5-9b-q4_k_m", "thinking_cap": 2048, "prompt_variant": "current", "jobs": [{"split": "dev", "source": "reference", "aggregate": {}}]},
        ]});
        assert!(table(&report).contains("| qwen3.5-9b-q4_k_m (thinking 2048) |"));
        // Refused: candidates without the empty thinking block (here Phi-4-mini, and all
        // candidates when none is named), stored-only runs, two-call and two-track.
        check_thinking(&off).unwrap();
        check_thinking(&on).unwrap();
        for bad in [
            Options {
                models: vec!["phi-4-mini-instruct-q4_k_m".into()],
                ..on.clone()
            },
            Options {
                models: Vec::new(),
                ..on.clone()
            },
            Options {
                rescore: true,
                ..on.clone()
            },
            Options {
                two_call: true,
                ..on.clone()
            },
            Options {
                two_track: true,
                ..on.clone()
            },
        ] {
            assert!(check_thinking(&bad).is_err(), "{bad:?}");
        }
        let claude = Options {
            models: vec!["claude-code".into()],
            sources: vec!["reference".into()],
            thinking: Some(2048),
            ..Options::default()
        };
        assert!(check_agent_cli(&claude, AgentCli::ClaudeCode).is_err());
    }

    #[test]
    fn sum_eval_selected_meetings_must_exist_and_keep_split_order() {
        let all = vec![
            ("dev".to_string(), ami_sum_meetings("dev")),
            ("test".to_string(), ami_sum_meetings("test")),
        ];
        assert_eq!(select_meetings(all.clone(), &[]).unwrap(), all);
        let wanted: Vec<String> = ["TS3004c", "ES2003c"].map(String::from).to_vec();
        assert_eq!(
            select_meetings(all.clone(), &wanted).unwrap(),
            [(
                "dev".to_string(),
                vec!["ES2003c".to_string(), "TS3004c".to_string()]
            )]
        );
        assert!(select_meetings(all, &["XX0000a".to_string()]).is_err());
    }

    #[test]
    fn sum_eval_a_finished_file_without_a_format_key_is_protected() {
        let current = Options::default();
        let lean = Options {
            prompt_variant: PromptVariant::Lean,
            ..Options::default()
        };
        // A finished result from older scoring (jobs, no `grammar`) is not overwritten by
        // any run, the production format included.
        let older = json!({"jobs": [{"split": "dev", "source": "reference", "meetings": []}]});
        assert!(other_format(&older, &lean).is_some());
        assert!(other_format(&older, &current).is_some());
        // Something that is not a finished result is not a reason to refuse.
        assert_eq!(other_format(&json!({}), &current), None);
    }

    #[test]
    fn sum_eval_a_run_with_other_context_settings_never_overwrites_a_finished_file() {
        let opts = Options::default();
        let stored = |context: u64, max_output: u64| {
            json!({
                "grammar": opts.format_key(), "context_tokens": context,
                "max_output_tokens": max_output, "jobs": [],
            })
        };
        let same = stored(u64::from(opts.context), opts.max_output as u64);
        assert_eq!(other_settings(&same, &opts), None);
        // Same format key and meetings, another context or reply ceiling: `covers` misses,
        // and the run would replace the stored aggregate.
        let ctx = stored(u64::from(opts.context) * 2, opts.max_output as u64);
        assert!(other_settings(&ctx, &opts).is_some_and(|s| s.contains("context")));
        let out = stored(u64::from(opts.context), opts.max_output as u64 / 2);
        assert!(other_settings(&out, &opts).is_some_and(|s| s.contains("max_output")));
        // A finished file that records no settings is refused too; a non-result is not.
        assert!(other_settings(&json!({"jobs": []}), &opts).is_some());
        assert_eq!(other_settings(&json!({}), &opts), None);
    }

    #[test]
    fn sum_eval_a_run_never_drops_meetings_of_a_finished_file() {
        let jobs = [("dev".to_string(), "reference".to_string())];
        let m = |list: &[&str]| -> Vec<(String, Vec<String>)> {
            vec![(
                "dev".to_string(),
                list.iter().map(|s| (*s).to_string()).collect(),
            )]
        };
        let stored = |list: &[&str]| {
            json!({"grammar": "indented-numbered-r5", "jobs": [{
                "split": "dev", "source": "reference",
                "meetings": list.iter().map(|s| json!({"meeting": s})).collect::<Vec<_>>(),
            }]})
        };
        let full = stored(&["ES2003a", "ES2003b", "ES2003c"]);
        // A `--meeting` subset (or a smaller `--limit`) would replace the full file's
        // aggregate with its own.
        assert_eq!(
            dropped_meetings(&full, &jobs, &m(&["ES2003b"])),
            ["dev:reference:ES2003a", "dev:reference:ES2003c"]
        );
        // The same or more meetings drop nothing.
        assert!(dropped_meetings(&full, &jobs, &m(&["ES2003a", "ES2003b", "ES2003c"])).is_empty());
        let subset = stored(&["ES2003b"]);
        assert!(dropped_meetings(&subset, &jobs, &m(&["ES2003a", "ES2003b"])).is_empty());
        // A job this run does not have loses all its meetings.
        let other_job = [("dev".to_string(), "earshot".to_string())];
        assert_eq!(
            dropped_meetings(&subset, &other_job, &m(&["ES2003b"])),
            ["dev:reference:ES2003b"]
        );
        // No finished file, nothing dropped.
        assert!(dropped_meetings(&json!({}), &jobs, &m(&["ES2003b"])).is_empty());
    }

    #[test]
    fn sum_eval_meeting_selection_comes_before_the_limit() {
        let dev = ami_sum_meetings("dev");
        let last = dev.last().unwrap().clone();
        let jobs = [("dev".to_string(), "reference".to_string())];
        // `--meeting <last dev meeting> --limit 1`: the meeting is in the split, so it is
        // selected, not reported as missing because the limit cut it first.
        assert_eq!(
            run_meetings(&jobs, std::slice::from_ref(&last), Some(1)).unwrap(),
            [("dev".to_string(), vec![last.clone()])]
        );
        // The limit still cuts the selection and the whole lists.
        let two = [dev[2].clone(), last];
        assert_eq!(
            run_meetings(&jobs, &two, Some(1)).unwrap(),
            [("dev".to_string(), vec![dev[2].clone()])]
        );
        assert_eq!(
            run_meetings(&jobs, &[], Some(2)).unwrap(),
            [("dev".to_string(), dev[..2].to_vec())]
        );
        assert_eq!(
            run_meetings(&jobs, &[], None).unwrap(),
            [("dev".to_string(), dev.clone())]
        );
        // A meeting of a split that is not run is still refused.
        let test = ami_sum_meetings("test");
        assert!(run_meetings(&jobs, &test[..1], None).is_err());
    }

    #[test]
    fn sum_eval_args_pass_the_prompt_variant_only_when_set() {
        let c = CANDIDATES[2];
        let args = |opts: &Options| -> Vec<String> {
            eval_args(
                Path::new("a"),
                Path::new("e"),
                &[],
                &[],
                Some((Path::new("m.gguf"), &c)),
                true,
                opts,
                Path::new("o.json"),
            )
            .into_iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect()
        };
        assert!(
            !args(&Options::default())
                .iter()
                .any(|a| a == "--prompt-variant")
        );
        let owners = Options {
            prompt_variant: PromptVariant::Owners,
            ..Options::default()
        };
        assert!(
            args(&owners)
                .windows(2)
                .any(|w| w == ["--prompt-variant", "owners"])
        );
    }

    #[test]
    fn sum_eval_table_names_the_prompt_variant() {
        let report = json!({"candidates": [
            {"candidate": "qwen3.5-9b-q4_k_m", "prompt_variant": "lean", "jobs": [{"split": "dev", "source": "reference", "aggregate": {}}]},
            {"candidate": "qwen3.5-9b-q4_k_m", "prompt_variant": "current", "jobs": [{"split": "dev", "source": "reference", "aggregate": {}}]},
        ]});
        let t = table(&report);
        assert!(t.contains("| qwen3.5-9b-q4_k_m (prompt lean) |"), "{t}");
        assert!(t.contains("| qwen3.5-9b-q4_k_m | ? |"), "{t}");
        // No length block in these aggregates: no length table.
        assert!(!t.contains("notes words"), "{t}");
    }

    #[test]
    fn sum_eval_style_preset_comes_last_in_the_format_key_and_reaches_the_example() {
        let o = Options {
            models: vec!["claude-code".into()],
            sources: vec!["reference".into()],
            prompt_variant: PromptVariant::Short,
            style_preset: StylePreset::Brief,
            ..Options::default()
        };
        assert_eq!(
            o.format_key(),
            "indented-numbered-r5-claude-code-prompt-short-template-brief"
        );
        check_claude_code(&o).unwrap();
        let standard = Options {
            style_preset: StylePreset::Standard,
            ..o.clone()
        };
        assert_eq!(
            standard.format_key(),
            "indented-numbered-r5-claude-code-prompt-short"
        );
        let args: Vec<String> = agent_cli_args(
            AgentCli::ClaudeCode,
            Path::new("a"),
            &[],
            &[],
            "claude-code",
            None,
            None,
            PromptVariant::Current,
            StylePreset::Detailed,
            None,
            Path::new("o.json"),
        )
        .into_iter()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
        assert!(args.windows(2).any(|w| w == ["--style-preset", "detailed"]));
    }

    #[test]
    fn sum_eval_table_lists_the_t1217_lengths() {
        let length = json!({
            "words": 812, "notes_words": 500, "sections": 9, "bullets": 31,
            "decisions": 14, "actions": 7, "lists_at_cap": 2, "item_lists_at_cap": 1,
            "meetings_with_item_list_at_cap": 1,
        });
        let report = json!({"candidates": [
            {"candidate": "claude-code", "prompt_variant": "short", "jobs": [{"split": "dev", "source": "reference", "aggregate": {"ok": 20, "meetings": 20, "length": length}}]},
        ]});
        let t = table(&report);
        assert!(t.contains("| words |"), "{t}");
        assert!(
            t.contains("| claude-code (prompt short) | dev | reference | 20/20 | 812 | 500 | 9 | 31 | 14 | 7 | 2 | 1 | 1 |"),
            "{t}"
        );
    }

    #[test]
    fn sum_eval_args_pass_the_prefix_and_backend() {
        let c = CANDIDATES[2];
        let args: Vec<String> = eval_args(
            Path::new("a"),
            Path::new("e"),
            &[("dev".into(), "reference".into())],
            &[("dev".into(), vec!["ES2003a".into(), "ES2003b".into()])],
            Some((Path::new("m.gguf"), &c)),
            true,
            &Options::default(),
            Path::new("o.json"),
        )
        .into_iter()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
        let at = args.iter().position(|a| a == "--assistant-prefix").unwrap();
        assert_eq!(args[at + 1], QWEN_NO_THINK);
        assert!(args.iter().any(|a| a == "--gpu"));
        assert!(
            args.windows(2)
                .any(|w| w == ["--meetings", "dev=ES2003a,ES2003b"])
        );
        assert!(args.windows(2).any(|w| w == ["--context", "16384"]));
        assert!(!args.iter().any(|a| a == "--chunk-seconds"));
        let chunked: Vec<String> = eval_args(
            Path::new("a"),
            Path::new("e"),
            &[],
            &[],
            Some((Path::new("m.gguf"), &c)),
            true,
            &Options {
                chunk_seconds: Some(600),
                ..Options::default()
            },
            Path::new("o.json"),
        )
        .into_iter()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
        assert!(chunked.windows(2).any(|w| w == ["--chunk-seconds", "600"]));
        let remote: Vec<String> = eval_args(
            Path::new("a"),
            Path::new("e"),
            &[],
            &[],
            None,
            false,
            &Options::default(),
            Path::new("o.json"),
        )
        .into_iter()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
        assert!(remote.iter().any(|a| a == "--remote"));
        assert!(!remote.iter().any(|a| a == "--model"));
        let rescore: Vec<String> = rescore_args(Path::new("a"), Path::new("o.json"))
            .into_iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(rescore, ["--annotations", "a", "--rescore", "o.json"]);
    }

    #[test]
    fn sum_eval_table_shows_all_three_item_rules_in_aligned_columns() {
        let counts = |c: u64| json!({"caught": c, "reference": 10, "summary": 4, "recall": c as f64 / 10.0, "precision": c as f64 / 4.0});
        let report = json!({"candidates": [{
            "candidate": "m", "backend": "cuda",
            "jobs": [{"split": "dev", "source": "reference", "aggregate": {
                "ok": 1, "meetings": 1, "rougeL_f": 0.1, "rougeLsum_f": 0.2, "rouge1_f": 0.3,
                "rouge2_f": 0.04, "decisions": counts(1), "actions": counts(2),
                "decisions_rr": counts(3), "actions_rr": counts(4),
                "decisions_cov": counts(5), "actions_cov": counts(6),
                "seconds_per_meeting": 1.0, "generated_tokens_per_s": 2.0,
                "prompt_tokens_per_s": 3.0, "repairs": 0, "truncated": 0, "looping": 0,
            }}],
        }]});
        let t = table(&report);
        let lines: Vec<&str> = t.lines().take(3).collect();
        let cells = |l: &str| l.matches('|').count();
        assert_eq!(cells(lines[0]), cells(lines[1]));
        assert_eq!(cells(lines[0]), cells(lines[2]), "{t}");
        assert!(lines[0].contains("decisions covered**"));
        // Coverage shows caught / reference and recall only (no precision).
        assert!(lines[2].contains("| 5/10 (0.50) | 6/10 (0.60) |"), "{t}");
        assert!(t.contains("** Coverage"), "{t}");
    }

    fn claude(models: &[&str]) -> Options {
        Options {
            models: models.iter().map(|m| (*m).to_owned()).collect(),
            splits: vec!["dev".into()],
            sources: vec!["reference".into()],
            ..Options::default()
        }
    }

    #[test]
    fn sum_eval_claude_code_has_its_own_format_key_file_and_budget() {
        let after = claude(&["claude-code"]);
        let chunked = Options {
            chunk_seconds: Some(600),
            ..after.clone()
        };
        assert!(after.is_claude_code() && !Options::default().is_claude_code());
        assert_eq!(after.format_key(), "indented-numbered-r5-claude-code");
        assert_eq!(
            chunked.format_key(),
            "indented-numbered-r5-chunk600-claude-code"
        );
        assert_eq!(
            format_key(
                Some(600),
                PromptVariant::Current,
                false,
                false,
                Some(AgentCli::ClaudeCode),
                None
            ),
            chunked.format_key()
        );
        // Claude Code's production budget, not --context / --max-output.
        let b = earshot_llm::claude_code::CLAUDE_CODE_BUDGET;
        assert_eq!(
            after.budget(),
            (b.context_window as u64, b.max_output_tokens as u64)
        );
        assert_eq!(Options::default().budget(), (16_384, 8192));
        // The two conditions and the local candidates never reuse or overwrite each
        // other's files.
        let jobs = [("dev".to_string(), "reference".to_string())];
        let meetings = [("dev".to_string(), vec!["ES2003c".to_string()])];
        let stored = |o: &Options| {
            let (c, m) = o.budget();
            json!({
                "grammar": o.format_key(), "context_tokens": c, "max_output_tokens": m,
                "jobs": [{"split": "dev", "source": "reference", "meetings": [{"meeting": "ES2003c"}]}],
            })
        };
        let local = Options {
            chunk_seconds: Some(600),
            ..Options::default()
        };
        assert!(covers(&stored(&after), &jobs, &meetings, &after));
        assert!(covers(&stored(&chunked), &jobs, &meetings, &chunked));
        assert!(!covers(&stored(&after), &jobs, &meetings, &chunked));
        assert!(!covers(&stored(&chunked), &jobs, &meetings, &local));
        assert!(other_format(&stored(&after), &chunked).is_some());
        assert!(other_format(&stored(&chunked), &local).is_some());
        assert!(other_format(&stored(&local), &chunked).is_some());
        assert_eq!(other_settings(&stored(&after), &after), None);
        // A file of a local run with the same key would still be refused by its budget.
        let mut with_local_budget = stored(&after);
        with_local_budget["context_tokens"] = json!(16_384);
        assert!(other_settings(&with_local_budget, &after).is_some());
        // Result files: by candidate and backend, safe on Windows.
        assert_eq!(
            result_file_name("claude-code", "claude-code"),
            "claude-code-claude-code.json"
        );
        assert_eq!(
            result_file_name("claude-code/claude-opus-4-1:x[1m]", "claude-code"),
            "claude-code_claude-opus-4-1_x_1m_-claude-code.json"
        );
        assert_eq!(
            result_file_name("qwen3.5-4b-q4_k_m", "cpu"),
            "qwen3.5-4b-q4_k_m-cpu.json"
        );
    }

    #[test]
    fn sum_eval_claude_code_model_names_follow_the_provider_rule() {
        assert_eq!(claude_code_model("claude-code").unwrap(), None);
        assert_eq!(
            claude_code_model("claude-code/opus").unwrap().as_deref(),
            Some("opus")
        );
        assert!(claude_code_model("claude-code/").is_err());
        assert!(claude_code_model("claude-code/--tools").is_err());
        assert!(claude_code_model("claude-code/op us").is_err());
        assert!(claude_code_model("qwen3.5-4b-q4_k_m").is_err());
        assert!(is_claude_code_id("claude-code/sonnet") && !is_claude_code_id("claude-codex"));
    }

    #[test]
    fn sum_eval_claude_code_refuses_private_sources_and_other_settings() {
        check_claude_code(&claude(&["claude-code"])).unwrap();
        check_claude_code(&Options {
            chunk_seconds: Some(600),
            ..claude(&["claude-code"])
        })
        .unwrap();
        let refused = |o: Options, needle: &str| {
            let e = check_claude_code(&o).unwrap_err().to_string();
            assert!(e.contains(needle), "{e}");
        };
        refused(claude(&["claude-code", "qwen3.5-4b-q4_k_m"]), "alone");
        refused(claude(&["claude-code/--tools"]), "claude-code model");
        // Only the public AMI reference transcripts: the default (both sources) and the
        // Earshot source are refused.
        for sources in [
            vec![],
            vec!["earshot".to_string()],
            vec!["reference".into(), "earshot".into()],
        ] {
            refused(
                Options {
                    sources,
                    ..claude(&["claude-code"])
                },
                "reference transcripts",
            );
        }
        let c = || claude(&["claude-code"]);
        // A prompt variant is allowed (T12.17, evaluation only).
        check_claude_code(&Options {
            prompt_variant: PromptVariant::Lean,
            ..c()
        })
        .unwrap();
        refused(
            Options {
                two_call: true,
                chunk_seconds: Some(600),
                ..c()
            },
            "production plan",
        );
        refused(
            Options {
                context: 32_768,
                ..c()
            },
            "do not apply",
        );
        refused(
            Options {
                max_output: 16_384,
                ..c()
            },
            "do not apply",
        );
        refused(
            Options {
                threads: Some(4),
                ..c()
            },
            "do not apply",
        );
        refused(
            Options {
                backend: Some("cpu".into()),
                ..c()
            },
            "do not apply",
        );
        refused(
            Options {
                export_prompts: Some("p".into()),
                ..c()
            },
            "does not replay",
        );
        refused(
            Options {
                replay_only: true,
                ..c()
            },
            "does not replay",
        );
    }

    #[test]
    fn sum_eval_claude_code_args_name_the_engine_model_and_chunks() {
        let s_with =
            |model: Option<&str>, chunk: Option<u32>, variant: PromptVariant| -> Vec<String> {
                claude_code_args(
                    Path::new("a"),
                    &[("dev".into(), "reference".into())],
                    &[("dev".into(), vec!["ES2003c".into()])],
                    "claude-code",
                    model,
                    chunk,
                    variant,
                    Path::new("o.json"),
                )
                .into_iter()
                .map(|a| a.to_string_lossy().into_owned())
                .collect()
            };
        let s =
            |model: Option<&str>, chunk: Option<u32>| s_with(model, chunk, PromptVariant::Current);
        let after = s(None, None);
        assert!(after.iter().any(|a| a == "--claude-code"));
        assert!(after.windows(2).any(|w| w == ["--job", "dev:reference"]));
        assert!(after.windows(2).any(|w| w == ["--meetings", "dev=ES2003c"]));
        assert!(after.windows(2).any(|w| w == ["--model-id", "claude-code"]));
        // Never the Earshot inputs, a local model or the remote endpoint.
        for flag in [
            "--earshot-inputs",
            "--model",
            "--remote",
            "--claude-model",
            "--chunk-seconds",
        ] {
            assert!(!after.iter().any(|a| a == flag), "{flag}");
        }
        let chunked = s(Some("opus"), Some(600));
        assert!(chunked.windows(2).any(|w| w == ["--chunk-seconds", "600"]));
        assert!(chunked.windows(2).any(|w| w == ["--claude-model", "opus"]));
        assert!(!after.iter().any(|a| a == "--prompt-variant"));
        // A prompt variant is passed on (T12.17, evaluation only).
        let short = s_with(None, None, PromptVariant::Short);
        assert!(short.windows(2).any(|w| w == ["--prompt-variant", "short"]));
    }

    #[test]
    fn sum_eval_codex_has_its_own_format_key_file_and_budget() {
        let after = claude(&["codex"]);
        let chunked = Options {
            chunk_seconds: Some(600),
            ..after.clone()
        };
        assert!(after.is_codex() && !after.is_claude_code());
        assert_eq!(after.agent_cli(), Some(AgentCli::Codex));
        assert_eq!(after.format_key(), "indented-numbered-r5-codex");
        assert_eq!(chunked.format_key(), "indented-numbered-r5-chunk600-codex");
        // Codex's production budget, not --context / --max-output.
        let b = earshot_llm::codex::CODEX_BUDGET;
        assert_eq!(
            after.budget(),
            (b.context_window as u64, b.max_output_tokens as u64)
        );
        // Never reused for or overwriting a Claude Code or local file.
        let claude_after = claude(&["claude-code"]);
        let stored = |o: &Options| {
            let (c, m) = o.budget();
            json!({
                "grammar": o.format_key(), "context_tokens": c, "max_output_tokens": m,
                "jobs": [{"split": "dev", "source": "reference", "meetings": [{"meeting": "ES2003c"}]}],
            })
        };
        let jobs = [("dev".to_string(), "reference".to_string())];
        let meetings = [("dev".to_string(), vec!["ES2003c".to_string()])];
        assert!(covers(&stored(&after), &jobs, &meetings, &after));
        assert!(!covers(&stored(&claude_after), &jobs, &meetings, &after));
        assert!(other_format(&stored(&claude_after), &after).is_some());
        assert!(other_format(&stored(&after), &chunked).is_some());
        assert_eq!(result_file_name("codex", "codex"), "codex-codex.json");
        assert_eq!(
            result_file_name("codex/gpt-5.6-luna", "codex"),
            "codex_gpt-5.6-luna-codex.json"
        );
    }

    #[test]
    fn sum_eval_codex_model_names_follow_the_provider_rule() {
        assert_eq!(codex_model("codex").unwrap(), None);
        assert_eq!(
            codex_model("codex/gpt-5.6-luna").unwrap().as_deref(),
            Some("gpt-5.6-luna")
        );
        assert!(codex_model("codex/").is_err());
        assert!(codex_model("codex/-s").is_err());
        assert!(codex_model("codex/gpt 5").is_err());
        assert!(codex_model("claude-code").is_err());
        assert!(is_codex_id("codex/o3") && !is_codex_id("codexx") && !is_codex_id("claude-code"));
        assert!(!is_claude_code_id("codex") && is_claude_code_id("claude-code"));
    }

    #[test]
    fn sum_eval_codex_refuses_private_sources_other_settings_and_a_stray_cli_path() {
        let c = || claude(&["codex"]);
        check_agent_cli(&c(), AgentCli::Codex).unwrap();
        check_agent_cli(
            &Options {
                chunk_seconds: Some(600),
                codex_cli: Some("C:\\x\\codex.exe".into()),
                ..c()
            },
            AgentCli::Codex,
        )
        .unwrap();
        let refused = |o: Options, needle: &str| {
            let e = check_agent_cli(&o, AgentCli::Codex)
                .unwrap_err()
                .to_string();
            assert!(e.contains(needle), "{e}");
        };
        refused(claude(&["codex", "claude-code"]), "alone");
        refused(claude(&["codex/-s"]), "codex model");
        refused(
            Options {
                sources: vec!["earshot".into()],
                ..c()
            },
            "reference transcripts",
        );
        check_agent_cli(
            &Options {
                prompt_variant: PromptVariant::Lean,
                ..c()
            },
            AgentCli::Codex,
        )
        .unwrap();
        refused(
            Options {
                context: 32_768,
                ..c()
            },
            "do not apply to --model codex",
        );
        refused(
            Options {
                replay_only: true,
                ..c()
            },
            "does not replay",
        );
        // --codex-cli belongs to the Codex candidate only.
        let e = check_agent_cli(
            &Options {
                codex_cli: Some("codex.exe".into()),
                ..claude(&["claude-code"])
            },
            AgentCli::ClaudeCode,
        )
        .unwrap_err();
        assert!(e.to_string().contains("--codex-cli"), "{e}");
    }

    #[test]
    fn sum_eval_codex_args_name_the_engine_model_chunks_and_cli() {
        let s = |model: Option<&str>, chunk: Option<u32>, cli: Option<&Path>| -> Vec<String> {
            agent_cli_args(
                AgentCli::Codex,
                Path::new("a"),
                &[("dev".into(), "reference".into())],
                &[("dev".into(), vec!["ES2003c".into()])],
                "codex",
                model,
                chunk,
                PromptVariant::Current,
                StylePreset::Standard,
                cli,
                Path::new("o.json"),
            )
            .into_iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect()
        };
        let after = s(None, None, None);
        assert!(after.iter().any(|a| a == "--codex"));
        assert!(after.windows(2).any(|w| w == ["--model-id", "codex"]));
        for flag in [
            "--claude-code",
            "--claude-model",
            "--codex-model",
            "--codex-cli",
            "--chunk-seconds",
            "--remote",
            "--earshot-inputs",
        ] {
            assert!(!after.iter().any(|a| a == flag), "{flag}");
        }
        let chunked = s(
            Some("gpt-5.6-luna"),
            Some(600),
            Some(Path::new("C:\\x\\codex.exe")),
        );
        assert!(chunked.windows(2).any(|w| w == ["--chunk-seconds", "600"]));
        assert!(
            chunked
                .windows(2)
                .any(|w| w == ["--codex-model", "gpt-5.6-luna"])
        );
        assert!(
            chunked
                .windows(2)
                .any(|w| w == ["--codex-cli", "C:\\x\\codex.exe"])
        );
        // The Claude Code arguments never carry a Codex CLI.
        let claude = agent_cli_args(
            AgentCli::ClaudeCode,
            Path::new("a"),
            &[],
            &[],
            "claude-code",
            None,
            None,
            PromptVariant::Current,
            StylePreset::Standard,
            Some(Path::new("codex.exe")),
            Path::new("o.json"),
        );
        assert!(!claude.iter().any(|a| a == "--codex-cli"));
    }

    /// T6.20a: the committed atomic facts hold every meeting of both splits, each sentence
    /// with at least one fact, ids in annotation order (`sum_eval --judge-packets --facts`
    /// checks the sentences against the annotations themselves).
    #[test]
    fn sum_eval_atomic_facts_file_holds_both_splits_whole() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .join("fixtures/ami/atomic_facts.json");
        let v: Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        assert_eq!(v["format"], 1);
        assert_eq!(v["split_by"], "qwen3.5-9b-q4_k_m");
        assert!(v["licence"].as_str().unwrap().contains("CC BY 4.0"));
        let mut want = ami_sum_meetings("dev");
        want.extend(ami_sum_meetings("test"));
        let meetings = v["meetings"].as_object().unwrap();
        assert_eq!(meetings.len(), want.len());
        let mut facts = 0;
        for meeting in &want {
            let m = &v["meetings"][meeting.as_str()];
            for (list, prefix) in [("decisions", "D"), ("action_items", "A")] {
                let items = m[list]
                    .as_array()
                    .unwrap_or_else(|| panic!("{meeting}: no {list}"));
                for (i, item) in items.iter().enumerate() {
                    assert_eq!(item["id"], format!("{prefix}{}", i + 1), "{meeting}");
                    assert!(!item["text"].as_str().unwrap().trim().is_empty());
                    let list = item["facts"].as_array().unwrap();
                    assert!(!list.is_empty(), "{meeting} {}", item["id"]);
                    for fact in list {
                        assert!(!fact.as_str().unwrap().trim().is_empty());
                        facts += 1;
                    }
                }
            }
        }
        assert!(
            facts >= 248,
            "fewer facts than annotated sentences: {facts}"
        );
    }
}
