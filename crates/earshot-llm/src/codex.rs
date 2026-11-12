//! Summaries through the user's installed OpenAI Codex CLI on their ChatGPT sign-in
//! (T7.4gc, PLAN.md 3.9 exception chosen by the owner). No HTTP here: Earshot hands
//! the prompt text to the CLI, which connects to OpenAI itself; Earshot cannot audit that
//! request, so every run records a [`Handover`] (when, how many characters, how it ended;
//! never the text), exactly as for Claude Code ([`crate::claude_code`]).
//!
//! # Finding the CLI ([`CodexCli::find`])
//!
//! Earshot does not ship Codex: it looks for the user's own. `codex.exe` in the standalone
//! installer's default folder `%LOCALAPPDATA%\Programs\OpenAI\Codex\bin` (`$defaultVisibleBinDir`
//! in openai/codex `scripts/install/install.ps1`, read 2026-11-12; a junction to the current
//! release), then npm's global folders (`%APPDATA%\npm`, `%LOCALAPPDATA%\npm`), then an
//! absolute `PATH` directory (`codex` off Windows, where only `PATH` is searched): the known
//! install folders come first, so a `codex` earlier on `PATH` is not run when a real install
//! exists (T9.25). An npm `codex.cmd` shim is never run (it would route the arguments
//! through `cmd.exe`): it is resolved to the native `codex.exe` of the platform package npm
//! installed beside it, `node_modules\@openai\codex\node_modules\@openai\codex-win32-<arch>\
//! vendor\<triple>\bin\codex.exe` (a global install) or the hoisted
//! `node_modules\@openai\codex-win32-<arch>\vendor\<triple>\bin\codex.exe` (the two layouts
//! another desktop app's publish script locates for codex-cli 0.155.1; `<arch>`/`<triple>` are
//! `x64`/`x86_64-pc-windows-msvc`, and by the same naming `arm64`/`aarch64-pc-windows-msvc` on
//! ARM64, which is not measured). Relative `PATH` entries are skipped. Codex documents no
//! other Windows install folder, so none is guessed (a `CODEX_INSTALL_DIR` install is found
//! through `PATH`, which the installer updates).
//!
//! # One run ([`CodexClient::complete`])
//!
//! `codex exec --json --ephemeral --skip-git-repo-check --ignore-user-config --ignore-rules
//! --color never -s read-only -c approval_policy="never" -c web_search="disabled"
//! -c project_doc_max_bytes=0 -c skills.max_context_tokens=1 -c history.persistence="none"
//! --disable <feature>... [-m <model>] -` (every flag from `codex --help` and
//! `codex exec --help` of codex-cli 0.155.1, 2026-11-04; [`run_args`]):
//! - `exec`: one non-interactive turn; `-` reads the prompt from standard input only.
//! - `-s read-only` (no writes even if a tool ran), `approval_policy="never"` (nothing ever
//!   waits for a person; an enterprise-managed ChatGPT workspace may require `on-request`
//!   instead: Codex then logs a warning item and uses that, measured 2026-11-04, harmless
//!   because no tool that asks is enabled), `web_search="disabled"`.
//! - No tools: `--disable` for every tool-providing feature that `codex features list`
//!   showed enabled or available on 0.155.1 ([`DISABLED_FEATURES`]: the shell and unified
//!   exec, apps (connectors), plugins, hooks, memories, multi-agent, image generation and
//!   viewing, goals, sleep, tool suggestions, skill search and installs, browsers, computer
//!   use, workspace dependencies, shell snapshots, MCP elicitation). A feature name this
//!   version does not know fails the run before any connection ("Unknown feature flag",
//!   measured), so every listed name was accepted by 0.155.1. The list is pinned to
//!   codex-cli 0.155.1: on every Codex version change, re-check it against
//!   `codex features list` (a new tool-providing feature must be added, a renamed one fixed).
//! - Tools that run anyway: any `item.started`/`item.completed` whose item type is not an
//!   agent message, reasoning or an error (for example `command_execution`, `file_change`,
//!   `mcp_tool_call`, `web_search`, or a type this version adds) fails the run with
//!   [`CodexError::ToolRan`], whatever the turn's outcome: its reply is never used.
//! - Events Earshot does not know: a JSON event whose `type` is not one the captures show
//!   ([`KNOWN_EVENTS`]), or an `item.*` event without an `item` object, fails the run with
//!   [`CodexError::UnknownEvent`] (a new Codex event could be a tool by another name). Lines
//!   that are not JSON are not events and are skipped (Codex prints only JSON on stdout with
//!   `--json`; its logs go to stderr).
//! - No user configuration: `--ignore-user-config` (no `config.toml`, so none of the user's
//!   MCP servers, profiles or model providers; the sign-in in `CODEX_HOME` still works),
//!   `--ignore-rules` (no execpolicy `.rules`), `project_doc_max_bytes=0` (no `AGENTS.md`),
//!   `skills.max_context_tokens=1` (skill descriptions left out, as an earlier app measured). What can
//!   still apply: enterprise-managed requirements of the user's ChatGPT workspace (measured:
//!   they override the approval policy) and the user's environment variables.
//! - `--ephemeral`: Codex keeps no session files of the run; `history.persistence="none"`
//!   keeps no prompt history either (belt and braces: after a real run there was no
//!   `history.jsonl` and no session file; a bad value of this key is refused by 0.155.1, so
//!   the key exists, while an unknown `-c` key would be ignored silently).
//! - The child's environment loses every variable that names another address, a credential
//!   other than the stored ChatGPT sign-in, or a proxy ([`CLEARED_ENV`]: `OPENAI_BASE_URL`,
//!   `OPENAI_API_BASE`, `OPENAI_API_KEY`, `CODEX_API_KEY`, `CODEX_ACCESS_TOKEN`, workload
//!   identity federation, `HTTP(S)_PROXY`, `ALL_PROXY`, `NO_PROXY`; T9.25), so no variable of
//!   the user's can send the prompt to another endpoint or bill another account. On a ChatGPT
//!   sign-in 0.155.1 ignored the two base URLs (measured 2026-11-04: both pointed at a closed
//!   loopback port, the run still answered). A user signed in only through such a variable
//!   reads as not signed in, so the summary is skipped with a banner. `CODEX_HOME` (where the
//!   sign-in lives) and the CA variables (`CODEX_CA_CERTIFICATE`, `SSL_CERT_FILE`: they name
//!   no address) are kept.
//! - Codex keeps its own coding-agent instructions (exec has no flag to replace them), so
//!   the stdin text starts with [`PREAMBLE`] (fixed) before the request parts.
//! - The working folder is a new, empty temporary folder, deleted afterwards. Nothing is
//!   written to disk by Earshot: the reply is the last `agent_message` item of the stream
//!   (no `-o` file; real runs carry the reply there).
//! - Before any text is written to the CLI, [`HandoverSink::started`] is told; if it cannot
//!   record, the CLI is stopped unread ([`CodexError::NotRecorded`]).
//! - The run goes through `earshot_proc::run_with` (no console window, a kill-on-close job
//!   object, the request timeout as its deadline, the run's `CancelToken` polled), and a
//!   cut-off output is a failure, never parsed.
//! - The reply is accepted only after a `turn.completed` event without `turn.failed`, from
//!   the last `agent_message` item; it is validated and repaired
//!   by [`crate::LlmSummarizer`] exactly as for the other providers. `--output-schema` exists
//!   but is not used: how it behaves on a ChatGPT sign-in could not be checked (T7.4gc), so
//!   the schema travels in the prompt, as for Claude Code.
//!
//! # Errors
//!
//! Known CLI messages map to [`CodexError`] variants with fixed wording (the CLI's output can
//! carry model text, URLs and request ids, so it never enters an error), each from a real
//! capture: the workspace refusal `codex_workspace_access_denied` (this host, 2026-11-04,
//! 0.155.1, a 403 in `turn.failed`), `401 Unauthorized` (this host, an empty `CODEX_HOME`),
//! "ran out of room in the model's context window" and "Input exceeds the maximum length"
//! (an earlier app's fixtures, 0.155.1), and "Unknown feature flag", clap's "unexpected argument" and
//! "Error loading config.toml" for an option or value this version refuses (this host). A
//! sign-in failure or any failure no known message explains asks `codex login status` once:
//! "Not logged in" is [`CodexError::NotSignedIn`].

use std::ffi::OsString;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use earshot_core::asr::CancelToken;
use earshot_core::identity::Clock;
use earshot_core::prompt::{ChatMessage, ContextBudget};
use earshot_proc::{ChildError, ChildOutput, ChildSpec};
use serde_json::Value;

use crate::claude_code::CliSearch;
use crate::client::Completion;
use crate::config::DEFAULT_REQUEST_TIMEOUT;
use crate::error::LlmError;
use crate::handover::{Handover, HandoverLog, HandoverOutcome, HandoverSink, TempFolder};

/// Context budget of a Codex summary: sized like Claude Code's (200k tokens); Codex's own
/// input cap is 1,048,576 characters (an earlier app's capture), above what this budget plans. Codex
/// chooses the reply length itself, so the reply budget only sizes the request plan.
pub const CODEX_BUDGET: ContextBudget = ContextBudget {
    context_window: 200_000,
    max_output_tokens: 16_000,
};

/// Deadline of `codex --version` and `codex login status`.
pub const STATUS_TIMEOUT: Duration = Duration::from_secs(20);

/// Most bytes of each CLI output kept; longer is a cut-off failure.
pub const MAX_CLI_OUTPUT: usize = 8 * 1024 * 1024;

/// Longest model name passed to `-m`.
pub const MAX_MODEL_NAME: usize = 64;

/// The fixed text at the start of a run's standard input (Codex keeps its own instructions).
pub const PREAMBLE: &str = "You write meeting summaries for Earshot, a meeting notes app. \
You have no tools in this session: do not run commands, read files or search the web. The \
input is a request in parts: a system part with the rules, a user part with the meeting, and \
sometimes your earlier reply with a correction. Follow the system part exactly and answer the \
last user part with only the JSON object it asks for, nothing before or after it.";

/// Features turned off for every run (module docs; names from `codex features list` of
/// codex-cli 0.155.1). Pinned to that version: re-check against `codex features list` on
/// every Codex version change.
pub const DISABLED_FEATURES: [&str; 21] = [
    "shell_tool",
    "unified_exec",
    "apps",
    "plugins",
    "remote_plugin",
    "hooks",
    "memories",
    "multi_agent",
    "image_generation",
    "view_image",
    "goals",
    "sleep_tool",
    "tool_suggest",
    "skill_search",
    "skill_mcp_dependency_install",
    "in_app_browser",
    "browser_use",
    "computer_use",
    "workspace_dependencies",
    "shell_snapshot",
    "tool_call_mcp_elicitation",
];

/// Prefix of the working folders.
const TEMP_PREFIX: &str = "earshot-codex";

/// Variables removed from the CLI's environment (module docs; T9.25, the owner's decision of
/// 2026-11-12: the CLI talks only to its signed-in default, never to an address, account or
/// proxy a variable of the user's names, as [`crate::claude_code::CLEARED_ENV`]). Where each
/// name comes from is beside it (read 2026-11-12; every name is also present as text in the
/// codex-cli 0.166.0 `codex.exe`, checked 2026-11-12). Codex has no variable that switches the
/// provider: `model_provider` and `model_providers` are config keys, and
/// `--ignore-user-config` leaves the user's `config.toml` unread.
pub const CLEARED_ENV: [&str; 17] = [
    // Another address for the built-in OpenAI provider (T7.4gc, measured on 0.155.1; the
    // config reference names the matching key `openai_base_url`).
    "OPENAI_BASE_URL",
    "OPENAI_API_BASE",
    // Credentials other than the stored ChatGPT sign-in: `OPENAI_API_KEY` (T7.4gc; the
    // advanced config page's `env_key = "OPENAI_API_KEY"`), and the rest from the
    // environment variables page
    // <https://learn.chatgpt.com/docs/config-file/environment-variables> (formerly
    // developers.openai.com/codex/environment-variables): an API key or access token for a
    // non-interactive run, and workload identity federation.
    "OPENAI_API_KEY",
    "CODEX_API_KEY",
    "CODEX_ACCESS_TOKEN",
    "OPENAI_FEDERATION_RULE_ID",
    "OPENAI_IDENTITY_TOKEN_FILE",
    "OPENAI_WORKLOAD_IDENTITY_CONTEXT",
    // Proxies. Codex's documentation names none (its config reference says an upstream proxy
    // is taken "from the environment"); these are the names reqwest, the HTTP client Codex is
    // built on, reads: the crate docs' Proxies section (<https://docs.rs/reqwest>: `HTTP_PROXY`,
    // `HTTPS_PROXY`, `ALL_PROXY` and their lower-case forms) and `NoProxy::from_env`
    // (`NO_PROXY`, then `no_proxy`). Windows names ignore case; the lower-case entries are for
    // other systems.
    "HTTPS_PROXY",
    "HTTP_PROXY",
    "ALL_PROXY",
    "NO_PROXY",
    "https_proxy",
    "http_proxy",
    "all_proxy",
    "no_proxy",
    // Another address for refreshing the stored sign-in's token. Not in Codex's documentation:
    // found as text in the codex-cli 0.166.0 `codex.exe` by a review. Cleared on the owner's
    // decision of 2026-11-13, because it can send the sign-in's refresh token elsewhere.
    "CODEX_REFRESH_TOKEN_URL_OVERRIDE",
];

/// Top-level event types of `codex exec --json` (every one seen in the captures: this host
/// 2026-11-04 and an earlier app's fixtures, codex-cli 0.155.1; `item.updated` from an earlier app's event
/// parser). Any other type fails the run ([`CodexError::UnknownEvent`]).
pub const KNOWN_EVENTS: [&str; 8] = [
    "thread.started",
    "turn.started",
    "turn.completed",
    "turn.failed",
    "item.started",
    "item.updated",
    "item.completed",
    "error",
];

/// Item types that are not a tool: the reply, the model's reasoning and Codex's own warnings
/// (module docs; every other item type fails the run).
const NON_TOOL_ITEMS: [&str; 3] = ["agent_message", "reasoning", "error"];

/// A known CLI message: every needle must occur (ASCII case ignored).
struct Known {
    needles: &'static [&'static str],
    error: fn() -> CodexError,
}

/// The CLI messages this provider recognises (module docs; first match wins: the workspace
/// refusal's own text says "Unauthorized", so it comes before the sign-in failure).
const KNOWN: &[Known] = &[
    // This host, 2026-11-04, 0.155.1: `turn.failed` "unexpected status 403 Forbidden:
    // {"detail":{"message":"Unauthorized: Contact your ChatGPT workspace administrator for
    // access.","code":"codex_workspace_access_denied"}}, ...", exit 1.
    Known {
        needles: &["codex_workspace_access_denied"],
        error: || CodexError::AccessDenied,
    },
    // This host, 2026-11-04, 0.155.1, an empty CODEX_HOME: `turn.failed` "unexpected status
    // 401 Unauthorized: Missing bearer or basic authentication in header, ...", exit 1.
    Known {
        needles: &["401 unauthorized"],
        error: || CodexError::SignInExpired,
    },
    // Earlier app's fixture oneshot_context_window.jsonl (0.155.1): `turn.failed` "Codex ran out of
    // room in the model's context window. ...".
    Known {
        needles: &["context window"],
        error: || CodexError::PromptTooLong,
    },
    // Earlier app's fixture stderr_input_too_large.txt (0.155.1): "Error: turn/start: turn/start
    // failed: Input exceeds the maximum length of 1048576 characters. ..." on stderr.
    Known {
        needles: &["input exceeds the maximum length"],
        error: || CodexError::PromptTooLong,
    },
    // This host, 2026-11-04, 0.155.1: "Error: Unknown feature flag: <name>", exit 1.
    Known {
        needles: &["unknown feature flag"],
        error: || CodexError::UnsupportedVersion,
    },
    // This host, 2026-11-04, 0.155.1: "error: unexpected argument '--x' found", exit 2 (clap).
    Known {
        needles: &["error: unexpected argument"],
        error: || CodexError::UnsupportedVersion,
    },
    // This host, 2026-11-04, 0.155.1: "Error loading config.toml: unknown variant `x`, ...
    // in `web_search`" for a `-c` value this version refuses, exit 1.
    Known {
        needles: &["error loading config.toml"],
        error: || CodexError::UnsupportedVersion,
    },
];

/// Why a Codex run gave no reply. The wording is fixed: no CLI output is ever part of it (it
/// can carry model text).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CodexError {
    /// No Codex CLI was found ([`CodexCli::find`]).
    NotInstalled,
    /// Codex is not signed in (`codex login status`: "Not logged in").
    NotSignedIn,
    /// Codex is signed in but OpenAI refused its sign-in (401).
    SignInExpired,
    /// The user's ChatGPT workspace does not allow Codex (its administrator decides).
    AccessDenied,
    /// The meeting is longer than Codex accepts.
    PromptTooLong,
    /// This Codex version refuses an option Earshot uses.
    UnsupportedVersion,
    /// Codex used a tool (ran a command, changed a file, called a server or searched): the
    /// run's reply is not used.
    ToolRan,
    /// Codex printed an event Earshot does not know (a type outside [`KNOWN_EVENTS`], or an
    /// item event without its item): the run's reply is not used.
    UnknownEvent,
    /// The CLI could not be started (the reason from the operating system).
    Start(String),
    /// The CLI did not finish within the deadline and was stopped.
    TimedOut(Duration),
    /// The CLI's output was cut off (or longer than [`MAX_CLI_OUTPUT`]).
    OutputIncomplete,
    /// Earshot could not record the handover locally, so the CLI was stopped before any
    /// text was handed to it.
    NotRecorded,
    /// The CLI's output is not a completed turn with a reply (what was wrong, never the
    /// output).
    BadOutput(&'static str),
    /// Any other failure: the exit code and a fixed word for what was seen (`turn_failed`,
    /// `turn_not_completed`).
    Failed {
        /// The exit code, if any.
        exit_code: Option<i32>,
        /// A fixed lower-case word, if any.
        reason: Option<&'static str>,
    },
}

impl CodexError {
    /// Whether the user has to sign in to Codex for a run to work.
    #[must_use]
    pub fn needs_sign_in(&self) -> bool {
        matches!(self, Self::NotSignedIn | Self::SignInExpired)
    }

    /// A fixed lower-case name of the variant (for records that must hold no text):
    /// `not_installed`, `not_signed_in`, `sign_in_expired`, `access_denied`,
    /// `prompt_too_long`, `unsupported_version`, `tool_ran`, `unknown_event`, `start`,
    /// `timed_out`,
    /// `output_incomplete`, `not_recorded`, `bad_output`, `failed`.
    #[must_use]
    pub fn kind(&self) -> &'static str {
        match self {
            Self::NotInstalled => "not_installed",
            Self::NotSignedIn => "not_signed_in",
            Self::SignInExpired => "sign_in_expired",
            Self::AccessDenied => "access_denied",
            Self::PromptTooLong => "prompt_too_long",
            Self::UnsupportedVersion => "unsupported_version",
            Self::ToolRan => "tool_ran",
            Self::UnknownEvent => "unknown_event",
            Self::Start(_) => "start",
            Self::TimedOut(_) => "timed_out",
            Self::OutputIncomplete => "output_incomplete",
            Self::NotRecorded => "not_recorded",
            Self::BadOutput(_) => "bad_output",
            Self::Failed { .. } => "failed",
        }
    }

    /// Whether retrying later can work without the user doing anything.
    #[must_use]
    pub fn is_retryable(&self) -> bool {
        matches!(self, Self::TimedOut(_))
    }
}

impl fmt::Display for CodexError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotInstalled => f.write_str("Codex is not installed on this computer"),
            Self::NotSignedIn => f.write_str("Codex is not signed in"),
            Self::SignInExpired => {
                f.write_str("OpenAI did not accept Codex's sign-in (it may have expired)")
            }
            Self::AccessDenied => f.write_str(
                "your ChatGPT workspace does not allow Codex; ask its administrator for access",
            ),
            Self::PromptTooLong => f.write_str("the meeting is longer than Codex accepts"),
            Self::UnsupportedVersion => {
                f.write_str("this Codex version and Earshot do not agree on an option")
            }
            Self::UnknownEvent => f.write_str(
                "Codex sent something Earshot does not recognise, so its answer was not used",
            ),
            Self::ToolRan => f.write_str(
                "Codex used a tool, which Earshot does not allow, so its answer was not used",
            ),
            Self::Start(e) => write!(f, "Codex could not be started: {e}"),
            Self::TimedOut(d) => write!(f, "Codex did not answer within {} s", d.as_secs()),
            Self::OutputIncomplete => f.write_str("Codex's answer was cut off"),
            Self::NotRecorded => f.write_str(
                "Earshot could not record the handover on this computer, so nothing was handed to Codex",
            ),
            Self::BadOutput(why) => write!(f, "Codex's answer is not usable: {why}"),
            Self::Failed { exit_code, reason } => {
                f.write_str("Codex reported an error")?;
                match (exit_code, reason) {
                    (Some(c), Some(r)) => write!(f, " (exit code {c}, {r})"),
                    (Some(c), None) => write!(f, " (exit code {c})"),
                    (None, Some(r)) => write!(f, " ({r})"),
                    (None, None) => Ok(()),
                }
            }
        }
    }
}

impl std::error::Error for CodexError {}

impl From<CodexError> for LlmError {
    fn from(e: CodexError) -> Self {
        LlmError::Codex(e)
    }
}

/// Why `name` cannot be passed to `-m` (empty is allowed: Codex's own default is used, and no
/// `-m` is passed). A letter or digit, then up to 63 letters, digits and `._-` (for example
/// `gpt-5.6-luna`, the rule of an earlier app's argument builder): never a leading `-`, a space,
/// a quote or an `=`.
///
/// # Errors
/// Why the name is refused (plain words, never the name).
pub fn check_model(name: &str) -> Result<(), &'static str> {
    if name.is_empty() {
        return Ok(());
    }
    if name.len() > MAX_MODEL_NAME {
        return Err("it is longer than 64 characters");
    }
    if !name.as_bytes()[0].is_ascii_alphanumeric() {
        return Err("it must start with a letter or digit");
    }
    if !name
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b"-._".contains(&b))
    {
        return Err("use only letters, digits, hyphens, dots and underscores");
    }
    Ok(())
}

/// A directly runnable Codex CLI: the executable and any arguments it needs before Earshot's
/// own (none for an installed Codex; tests run a fake this way).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodexCli {
    /// The executable.
    pub program: PathBuf,
    /// Arguments before Earshot's own.
    pub leading_args: Vec<OsString>,
}

/// The npm platform package and Rust target of this build's Windows architecture.
fn npm_platform() -> (&'static str, &'static str) {
    if cfg!(target_arch = "aarch64") {
        ("codex-win32-arm64", "aarch64-pc-windows-msvc")
    } else {
        ("codex-win32-x64", "x86_64-pc-windows-msvc")
    }
}

impl CodexCli {
    /// The directories searched, in order (module docs): on Windows the standalone
    /// installer's default folder and npm's global folders, then the absolute `PATH` entries.
    #[must_use]
    pub fn search_dirs(search: &CliSearch) -> Vec<PathBuf> {
        let mut dirs = Vec::new();
        if cfg!(windows)
            && let Some(local) = &search.local_app_data
        {
            dirs.push(
                local
                    .join("Programs")
                    .join("OpenAI")
                    .join("Codex")
                    .join("bin"),
            );
        }
        dirs.extend(search.npm_dirs());
        dirs.extend(search.path_dirs());
        dirs
    }

    /// The CLI found from this process's environment, `None` when there is none.
    #[must_use]
    pub fn find() -> Option<Self> {
        Self::find_with(&CliSearch::from_env())
    }

    /// The CLI found in `search` (module docs).
    #[must_use]
    pub fn find_with(search: &CliSearch) -> Option<Self> {
        let exe = if cfg!(windows) { "codex.exe" } else { "codex" };
        for dir in Self::search_dirs(search) {
            let path = dir.join(exe);
            if path.is_file() {
                return Some(Self::at(path));
            }
            if cfg!(windows)
                && dir.join("codex.cmd").is_file()
                && let Some(native) = Self::npm_native(&dir)
            {
                return Some(Self::at(native));
            }
        }
        None
    }

    /// The native `codex.exe` npm installed beside a `codex.cmd` shim in `npm_dir` (module
    /// docs), when there is one.
    #[must_use]
    pub fn npm_native(npm_dir: &Path) -> Option<PathBuf> {
        let (package, triple) = npm_platform();
        let openai = npm_dir.join("node_modules").join("@openai");
        [
            openai
                .join("codex")
                .join("node_modules")
                .join("@openai")
                .join(package),
            openai.join(package),
        ]
        .into_iter()
        .map(|p| p.join("vendor").join(triple).join("bin").join("codex.exe"))
        .find(|p| p.is_file())
    }

    /// The CLI at `program`, run directly.
    #[must_use]
    pub fn at(program: PathBuf) -> Self {
        Self {
            program,
            leading_args: Vec::new(),
        }
    }

    /// A child run of the CLI with `args` after the leading ones, in `dir`, with `stdin` and
    /// `deadline`, without [`CLEARED_ENV`] (the rest of the environment stays: Codex finds
    /// its sign-in through it).
    fn spec(
        &self,
        args: Vec<OsString>,
        dir: &Path,
        stdin: Vec<u8>,
        deadline: Duration,
    ) -> ChildSpec {
        let mut all = self.leading_args.clone();
        all.extend(args);
        ChildSpec {
            program: self.program.clone(),
            args: all,
            current_dir: dir.to_path_buf(),
            env_remove: CLEARED_ENV.iter().map(OsString::from).collect(),
            env_set: Vec::new(),
            stdin,
            deadline,
            max_output: MAX_CLI_OUTPUT,
        }
    }

    /// A quick CLI query (`--version`, `login status`) in an empty temporary folder; the
    /// output need not be complete-checked by the caller.
    fn query(&self, args: &[&str], timeout: Duration) -> Result<ChildOutput, CodexError> {
        let dir = TempFolder::new(&std::env::temp_dir(), TEMP_PREFIX)
            .map_err(|e| CodexError::Start(e.to_string()))?;
        let spec = self.spec(
            args.iter().map(OsString::from).collect(),
            dir.path(),
            Vec::new(),
            timeout,
        );
        let out = earshot_proc::run(&spec, &|| false).map_err(child_error)?;
        out.require_complete()
            .map_err(|_| CodexError::OutputIncomplete)
    }

    /// The CLI's version (`codex --version`, first line, for example `codex-cli 0.155.1`).
    ///
    /// # Errors
    /// The CLI cannot be run, fails or prints nothing.
    pub fn version(&self, timeout: Duration) -> Result<String, CodexError> {
        let out = self.query(&["--version"], timeout)?;
        let text = String::from_utf8_lossy(&out.stdout);
        let line = text.lines().next().unwrap_or("").trim();
        if !out.status.success() || line.is_empty() {
            return Err(CodexError::Failed {
                exit_code: out.status.code(),
                reason: None,
            });
        }
        Ok(line.chars().take(80).collect())
    }

    /// Whether the CLI is signed in (`codex login status`, which never signs in).
    #[must_use]
    pub fn sign_in(&self, timeout: Duration) -> CodexSignIn {
        match self.query(&["login", "status"], timeout) {
            Ok(out) => sign_in_of(&out),
            Err(e) => CodexSignIn::Unknown(e.to_string()),
        }
    }
}

/// Codex's sign-in state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CodexSignIn {
    /// Signed in (`chatgpt` or `api_key`, when the status line says which).
    SignedIn {
        /// How (a fixed word, never the status text).
        method: Option<&'static str>,
    },
    /// Not signed in.
    NotSignedIn,
    /// The check failed (why).
    Unknown(String),
}

/// The sign-in of a `codex login status` run (captured on this host with 0.155.1: "Logged in
/// using ChatGPT", exit 0; with an empty `CODEX_HOME` "Not logged in", exit 1). Only fixed
/// words are kept (an API-key sign-in prints part of the key).
#[must_use]
pub fn sign_in_of(out: &ChildOutput) -> CodexSignIn {
    let text = format!(
        "{}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
    .to_ascii_lowercase();
    if text.contains("not logged in") {
        return CodexSignIn::NotSignedIn;
    }
    if out.status.success() && text.contains("logged in") {
        let method = if text.contains("chatgpt") {
            Some("chatgpt")
        } else if text.contains("api key") {
            Some("api_key")
        } else {
            None
        };
        return CodexSignIn::SignedIn { method };
    }
    CodexSignIn::Unknown("Codex's sign-in status was not recognised".into())
}

fn child_error(e: ChildError) -> CodexError {
    match e {
        ChildError::Start(e) | ChildError::Job(e) | ChildError::Wait(e) => {
            CodexError::Start(e.to_string())
        }
        ChildError::TimedOut(d) => CodexError::TimedOut(d),
        ChildError::OutputIncomplete { .. } => CodexError::OutputIncomplete,
        ChildError::Aborted => CodexError::NotRecorded,
        // Mapped by the caller (it knows whether the run's token was set).
        ChildError::Cancelled => CodexError::Failed {
            exit_code: None,
            reason: Some("cancelled"),
        },
    }
}

/// The arguments of a summary run after the CLI's leading ones (module docs): `model`
/// `None` uses Codex's default.
#[must_use]
pub fn run_args(model: Option<&str>) -> Vec<OsString> {
    let mut args: Vec<OsString> = [
        "exec",
        "--json",
        "--ephemeral",
        "--skip-git-repo-check",
        "--ignore-user-config",
        "--ignore-rules",
        "--color",
        "never",
        "-s",
        "read-only",
        "-c",
        "approval_policy=\"never\"",
        "-c",
        "web_search=\"disabled\"",
        "-c",
        "project_doc_max_bytes=0",
        "-c",
        "skills.max_context_tokens=1",
        "-c",
        "history.persistence=\"none\"",
    ]
    .iter()
    .map(OsString::from)
    .collect();
    for f in DISABLED_FEATURES {
        args.push("--disable".into());
        args.push(f.into());
    }
    if let Some(m) = model {
        args.push("-m".into());
        args.push(m.into());
    }
    args.push("-".into());
    args
}

/// The request as Codex reads it on standard input: [`PREAMBLE`], then the parts as Claude
/// Code reads them ([`crate::claude_code::stdin_text`]).
#[must_use]
pub fn stdin_text(messages: &[ChatMessage]) -> String {
    format!("{PREAMBLE}\n\n{}", crate::claude_code::stdin_text(messages))
}

/// Settings of [`CodexClient`].
#[derive(Debug, Clone)]
pub struct CodexConfig {
    /// The CLI.
    pub cli: CodexCli,
    /// `-m` (checked with [`check_model`]); `None`: Codex's default.
    pub model: Option<String>,
    /// Deadline of each run.
    pub request_timeout: Duration,
    /// Context budget for the request plan.
    pub budget: ContextBudget,
    /// Where the empty working folders are made (default: the system temp folder).
    pub temp_root: PathBuf,
}

impl CodexConfig {
    /// `cli` with `model` and the defaults ([`DEFAULT_REQUEST_TIMEOUT`], [`CODEX_BUDGET`],
    /// the system temp folder).
    ///
    /// # Errors
    /// A model name [`check_model`] refuses: [`LlmError::Codex`] with [`CodexError::Start`]
    /// (nothing is run).
    pub fn new(cli: CodexCli, model: Option<String>) -> Result<Self, LlmError> {
        let model = model.filter(|m| !m.is_empty());
        if let Some(m) = &model {
            check_model(m).map_err(|why| {
                CodexError::Start(format!("the model name cannot be used: {why}"))
            })?;
        }
        Ok(Self {
            cli,
            model,
            request_timeout: DEFAULT_REQUEST_TIMEOUT,
            budget: CODEX_BUDGET,
            temp_root: std::env::temp_dir(),
        })
    }
}

/// Summary requests through the Codex CLI (module docs). Cheap to clone; clones share the
/// [`HandoverLog`].
#[derive(Clone)]
pub struct CodexClient {
    config: CodexConfig,
    clock: Arc<dyn Clock + Send + Sync>,
    log: HandoverLog<CodexError>,
    sink: Arc<Mutex<Option<Arc<dyn HandoverSink<CodexError>>>>>,
}

impl fmt::Debug for CodexClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CodexClient")
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

impl CodexClient {
    /// A client for `config`, timestamping its handovers with `clock`.
    pub fn new(config: CodexConfig, clock: Arc<dyn Clock + Send + Sync>) -> Self {
        Self {
            config,
            clock,
            log: HandoverLog::new(),
            sink: Arc::new(Mutex::new(None)),
        }
    }

    /// Sets (or clears) where handovers are reported as they happen; shared by clones.
    pub fn set_sink(&self, sink: Option<Arc<dyn HandoverSink<CodexError>>>) {
        *self.sink.lock().unwrap_or_else(PoisonError::into_inner) = sink;
    }

    /// The settings.
    pub fn config(&self) -> &CodexConfig {
        &self.config
    }

    /// A handle to the handover log (the privacy record).
    pub fn handovers(&self) -> HandoverLog<CodexError> {
        self.log.clone()
    }

    /// One run with `messages` (module docs). Blocks until the CLI ends; returns
    /// [`LlmError::Cancelled`] as soon as `cancel` is set (the CLI is stopped).
    ///
    /// # Errors
    /// [`LlmError::Codex`]; [`LlmError::Cancelled`].
    pub fn complete(
        &self,
        messages: &[ChatMessage],
        cancel: &CancelToken,
    ) -> Result<Completion, LlmError> {
        if cancel.is_cancelled() {
            return Err(LlmError::Cancelled);
        }
        let text = stdin_text(messages);
        let characters = text.chars().count();
        let dir = TempFolder::new(&self.config.temp_root, TEMP_PREFIX)
            .map_err(|e| CodexError::Start(format!("no working folder: {e}")))?;
        let spec = self.config.cli.spec(
            run_args(self.config.model.as_deref()),
            dir.path(),
            text.into_bytes(),
            self.config.request_timeout,
        );
        let timestamp_ms = self.clock.now_unix_ms();
        let sink = self
            .sink
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        let run = earshot_proc::run_with(&spec, &|| cancel.is_cancelled(), &mut |_| {
            sink.as_ref()
                .is_none_or(|s| s.started(timestamp_ms, characters).is_ok())
        });
        drop(dir);
        let result = match run {
            // Nothing was handed over: the CLI never started, or was stopped unread.
            Err(ChildError::Start(e) | ChildError::Job(e)) => {
                return Err(CodexError::Start(e.to_string()).into());
            }
            Err(ChildError::Aborted) => return Err(CodexError::NotRecorded.into()),
            Err(ChildError::Cancelled) => Err(LlmError::Cancelled),
            Err(e) => Err(LlmError::Codex(child_error(e))),
            Ok(out) => self.reply_of(&out),
        };
        let handover = Handover {
            timestamp_ms,
            characters,
            outcome: match &result {
                Ok(c) => HandoverOutcome::Answered {
                    characters: c.content.chars().count(),
                },
                Err(LlmError::Cancelled) => HandoverOutcome::Cancelled,
                Err(LlmError::Codex(e)) => HandoverOutcome::Failed(e.clone()),
                // `complete` returns only Codex errors and cancellation.
                Err(_) => HandoverOutcome::Failed(CodexError::Failed {
                    exit_code: None,
                    reason: None,
                }),
            },
        };
        if let Some(s) = &sink {
            s.ended(&handover);
        }
        self.log.record(handover);
        result
    }

    /// The reply of a finished run, or why there is none (a sign-in failure, or a failure the
    /// known messages do not explain, asks for the sign-in state once).
    fn reply_of(&self, out: &ChildOutput) -> Result<Completion, LlmError> {
        match parse_output(out) {
            Ok(c) => Ok(c),
            Err(Parsed::Known(e)) if e != CodexError::SignInExpired => Err(e.into()),
            Err(Parsed::Known(e) | Parsed::Unexplained(e)) => {
                if self.config.cli.sign_in(STATUS_TIMEOUT) == CodexSignIn::NotSignedIn {
                    Err(CodexError::NotSignedIn.into())
                } else {
                    Err(e.into())
                }
            }
        }
    }
}

/// Why [`parse_output`] gave no reply.
#[derive(Debug, PartialEq, Eq)]
pub enum Parsed {
    /// A known message, or a cut-off output (nothing to ask about, except a sign-in failure).
    Known(CodexError),
    /// A failure no known message explains (the sign-in may be the reason).
    Unexplained(CodexError),
}

/// The first known message in `texts`.
fn known_message(texts: &[&str]) -> Option<CodexError> {
    let lower: Vec<String> = texts.iter().map(|t| t.to_ascii_lowercase()).collect();
    KNOWN
        .iter()
        .find(|k| {
            lower
                .iter()
                .any(|t| k.needles.iter().all(|n| t.contains(n)))
        })
        .map(|k| (k.error)())
}

/// What a run's JSONL events said.
#[derive(Default)]
struct Events {
    completed: bool,
    failed: bool,
    last_agent_message: Option<String>,
    /// An item that is a tool ran (module docs).
    tool_ran: bool,
    /// An event Earshot does not know (module docs).
    unknown_event: bool,
    /// `turn.failed` messages, top-level `error` lines and `error` items (warnings when the
    /// turn completes; only read for a failed run).
    errors: Vec<String>,
}

fn events_of(stdout: &str) -> Events {
    let mut ev = Events::default();
    let text = |v: &Value, k: &str| v.get(k).and_then(Value::as_str).map(str::to_owned);
    for line in stdout.lines() {
        let Ok(v) = serde_json::from_str::<Value>(line.trim()) else {
            continue;
        };
        let kind = v.get("type").and_then(Value::as_str);
        if !kind.is_some_and(|k| KNOWN_EVENTS.contains(&k)) {
            ev.unknown_event = true;
            continue;
        }
        match kind {
            Some("turn.completed") => ev.completed = true,
            Some("turn.failed") => {
                ev.failed = true;
                if let Some(m) = v.get("error").and_then(|e| text(e, "message")) {
                    ev.errors.push(m);
                }
            }
            Some("error") => ev.errors.extend(text(&v, "message")),
            Some(kind @ ("item.started" | "item.updated" | "item.completed")) => {
                let Some(item) = v.get("item").filter(|i| i.is_object()) else {
                    ev.unknown_event = true;
                    continue;
                };
                let item_type = item.get("type").and_then(Value::as_str);
                if !item_type.is_some_and(|t| NON_TOOL_ITEMS.contains(&t)) {
                    ev.tool_ran = true;
                    continue;
                }
                if kind != "item.completed" {
                    continue;
                }
                match item_type {
                    // The latest agent message is the answer; an empty one means none.
                    Some("agent_message") => {
                        ev.last_agent_message = text(item, "text").filter(|t| !t.trim().is_empty());
                    }
                    Some("error") => ev.errors.extend(text(item, "message")),
                    _ => {}
                }
            }
            _ => {}
        }
    }
    ev
}

/// The reply in a finished run's output: complete output, no tool item, a `turn.completed`
/// event and no `turn.failed`, and a non-empty last `agent_message`. Exposed for tests.
///
/// # Errors
/// [`Parsed`].
pub fn parse_output(out: &ChildOutput) -> Result<Completion, Parsed> {
    if !out.complete() {
        return Err(Parsed::Known(CodexError::OutputIncomplete));
    }
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    let ev = events_of(&stdout);
    if ev.tool_ran {
        return Err(Parsed::Known(CodexError::ToolRan));
    }
    if ev.unknown_event {
        return Err(Parsed::Known(CodexError::UnknownEvent));
    }
    let reply = ev.last_agent_message.clone();
    if ev.completed && !ev.failed {
        // A completed turn wins over the exit code (as an earlier app measured); its error items were
        // warnings.
        return reply
            .map(|content| Completion {
                content,
                finish_reason: None,
            })
            .ok_or(Parsed::Known(CodexError::BadOutput(
                "the turn ended without a reply",
            )));
    }
    let mut texts: Vec<&str> = ev.errors.iter().map(String::as_str).collect();
    texts.push(&stderr);
    texts.push(&stdout);
    if let Some(e) = known_message(&texts) {
        return Err(Parsed::Known(e));
    }
    let exit_code = out.status.code();
    if out.status.success() && !ev.failed {
        return Err(Parsed::Known(CodexError::BadOutput(
            "the turn did not complete",
        )));
    }
    Err(Parsed::Unexplained(CodexError::Failed {
        exit_code,
        reason: Some(if ev.failed {
            "turn_failed"
        } else {
            "turn_not_completed"
        }),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codex_model_names_are_checked() {
        for ok in ["", "gpt-5.6-luna", "o3", "gpt_5", "a.b-c"] {
            assert!(check_model(ok).is_ok(), "{ok}");
        }
        for bad in [
            "-m",
            "--sandbox",
            ".x",
            "gpt 5",
            "gp\"t",
            "a=b",
            "a\nb",
            "a:b",
            "\u{e9}",
        ] {
            assert!(check_model(bad).is_err(), "{bad:?}");
        }
        assert!(check_model(&"a".repeat(MAX_MODEL_NAME)).is_ok());
        assert!(check_model(&"a".repeat(MAX_MODEL_NAME + 1)).is_err());
    }

    #[test]
    fn codex_run_args_hold_only_fixed_text_and_the_model() {
        let s: Vec<String> = run_args(Some("gpt-5.6-luna"))
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        let pos = |flag: &str| s.iter().position(|a| a == flag).unwrap();
        assert_eq!(s[0], "exec");
        assert_eq!(s.last().unwrap(), "-", "the prompt only on standard input");
        assert_eq!(s[pos("-s") + 1], "read-only");
        assert_eq!(s[pos("-m") + 1], "gpt-5.6-luna");
        assert_eq!(s[pos("--color") + 1], "never");
        for flag in [
            "--json",
            "--ephemeral",
            "--skip-git-repo-check",
            "--ignore-user-config",
            "--ignore-rules",
        ] {
            assert!(s.contains(&flag.to_owned()), "{flag}");
        }
        for c in [
            "approval_policy=\"never\"",
            "web_search=\"disabled\"",
            "project_doc_max_bytes=0",
            "skills.max_context_tokens=1",
            "history.persistence=\"none\"",
        ] {
            assert!(s.windows(2).any(|w| w[0] == "-c" && w[1] == c), "{c}");
        }
        for f in [
            "shell_tool",
            "unified_exec",
            "apps",
            "plugins",
            "hooks",
            "memories",
        ] {
            assert!(
                s.windows(2).any(|w| w[0] == "--disable" && w[1] == f),
                "{f}"
            );
        }
        assert_eq!(
            s.iter().filter(|a| *a == "--disable").count(),
            DISABLED_FEATURES.len()
        );
        for never in [
            "-o",
            "--output-last-message",
            "--search",
            "--dangerously-bypass-approvals-and-sandbox",
            "--approve-for-me",
            "--add-dir",
            "--enable",
            "workspace-write",
            "danger-full-access",
        ] {
            assert!(!s.iter().any(|a| a == never), "{never}");
        }
        assert!(!run_args(None).iter().any(|a| a == "-m"));
    }

    fn output(stdout: &str, stderr: &str, ok: bool) -> ChildOutput {
        #[cfg(windows)]
        let status = {
            use std::os::windows::process::ExitStatusExt;
            std::process::ExitStatus::from_raw(u32::from(!ok))
        };
        #[cfg(not(windows))]
        let status = {
            use std::os::unix::process::ExitStatusExt;
            std::process::ExitStatus::from_raw(if ok { 0 } else { 256 })
        };
        ChildOutput {
            status,
            stdout: stdout.as_bytes().to_vec(),
            stderr: stderr.as_bytes().to_vec(),
            stdout_complete: true,
            stderr_complete: true,
        }
    }

    #[test]
    fn codex_sign_in_status_reads_the_status_line() {
        assert_eq!(
            sign_in_of(&output("Logged in using ChatGPT\n", "", true)),
            CodexSignIn::SignedIn {
                method: Some("chatgpt")
            }
        );
        assert_eq!(
            sign_in_of(&output(
                "",
                "Logged in using an API key - sk-proj-***abc\n",
                true
            )),
            CodexSignIn::SignedIn {
                method: Some("api_key")
            }
        );
        assert_eq!(
            sign_in_of(&output("Not logged in\n", "", false)),
            CodexSignIn::NotSignedIn
        );
        assert!(matches!(
            sign_in_of(&output("something else", "", true)),
            CodexSignIn::Unknown(_)
        ));
        assert!(matches!(
            sign_in_of(&output("Logged in using ChatGPT", "", false)),
            CodexSignIn::Unknown(_)
        ));
    }

    #[test]
    fn codex_output_needs_a_completed_turn_and_a_reply() {
        let completed = r#"{"type":"thread.started","thread_id":"t"}
{"type":"turn.started"}
{"type":"item.completed","item":{"id":"item_0","type":"error","message":"Exceeded skills context budget."}}
{"type":"item.completed","item":{"id":"item_1","type":"reasoning","text":"thinking"}}
{"type":"item.completed","item":{"id":"item_2","type":"agent_message","text":"from the stream"}}
{"type":"turn.completed","usage":{"input_tokens":1}}"#;
        let c = parse_output(&output(completed, "", true)).unwrap();
        assert_eq!(c.content, "from the stream");
        let c = parse_output(&output(completed, "", false)).unwrap();
        assert_eq!(
            c.content, "from the stream",
            "a completed turn wins over the exit"
        );
        // No completed turn: never a reply, even with an agent message.
        let partial =
            r#"{"type":"item.completed","item":{"id":"item_1","type":"agent_message","text":"x"}}"#;
        assert_eq!(
            parse_output(&output(partial, "", true)),
            Err(Parsed::Known(CodexError::BadOutput(
                "the turn did not complete"
            )))
        );
        // A completed turn whose last agent message is empty has no reply.
        let empty = r#"{"type":"item.completed","item":{"id":"item_1","type":"agent_message","text":"x"}}
{"type":"item.completed","item":{"id":"item_2","type":"agent_message","text":"  "}}
{"type":"turn.completed"}"#;
        assert!(matches!(
            parse_output(&output(empty, "", true)),
            Err(Parsed::Known(CodexError::BadOutput(_)))
        ));
        // A cut-off output.
        let mut cut = output(completed, "", true);
        cut.stdout_complete = false;
        assert_eq!(
            parse_output(&cut),
            Err(Parsed::Known(CodexError::OutputIncomplete))
        );
    }

    #[test]
    fn codex_any_tool_item_fails_the_run_whatever_the_turn_says() {
        let with = |item: &str| {
            format!(
                "{{\"type\":\"turn.started\"}}\n{item}\n\
                 {{\"type\":\"item.completed\",\"item\":{{\"id\":\"item_9\",\"type\":\"agent_message\",\"text\":\"{{}}\"}}}}\n\
                 {{\"type\":\"turn.completed\"}}"
            )
        };
        for item in [
            r#"{"type":"item.started","item":{"id":"item_1","type":"command_execution","command":"dir"}}"#,
            r#"{"type":"item.completed","item":{"id":"item_1","type":"file_change","changes":[]}}"#,
            r#"{"type":"item.completed","item":{"id":"item_1","type":"mcp_tool_call"}}"#,
            r#"{"type":"item.started","item":{"id":"item_1","type":"web_search","query":"x"}}"#,
            r#"{"type":"item.updated","item":{"id":"item_1","type":"todo_list"}}"#,
            r#"{"type":"item.completed","item":{"id":"item_1"}}"#,
        ] {
            assert_eq!(
                parse_output(&output(&with(item), "", true)),
                Err(Parsed::Known(CodexError::ToolRan)),
                "{item}"
            );
        }
        assert_eq!(CodexError::ToolRan.kind(), "tool_ran");
        assert!(!CodexError::ToolRan.is_retryable());
    }

    #[test]
    fn codex_unknown_events_fail_the_run_closed() {
        let with = |event: &str| {
            format!(
                "{{\"type\":\"thread.started\",\"thread_id\":\"t\"}}\n{{\"type\":\"turn.started\"}}\n{event}\n\
                 {{\"type\":\"item.completed\",\"item\":{{\"id\":\"item_9\",\"type\":\"agent_message\",\"text\":\"{{}}\"}}}}\n\
                 {{\"type\":\"turn.completed\"}}"
            )
        };
        for event in [
            // An item event without its item object.
            r#"{"type":"item.completed"}"#,
            r#"{"type":"item.started","item":"command_execution"}"#,
            r#"{"type":"item.updated","item":null}"#,
            // A top-level type outside the known set, and none at all.
            r#"{"type":"exec_command.begin","command":"dir"}"#,
            r#"{"type":"turn.diff","diff":""}"#,
            r#"{"id":"x"}"#,
            r#"[1,2]"#,
        ] {
            assert_eq!(
                parse_output(&output(&with(event), "", true)),
                Err(Parsed::Known(CodexError::UnknownEvent)),
                "{event}"
            );
        }
        // Every known type with its usual shape still passes; a line that is not JSON is
        // not an event.
        let known = with(
            r#"{"type":"item.updated","item":{"id":"item_1","type":"reasoning","text":"x"}}
{"type":"error","message":"Reconnecting... 1/5"}
not json at all"#,
        );
        assert!(parse_output(&output(&known, "", true)).is_ok());
        assert_eq!(CodexError::UnknownEvent.kind(), "unknown_event");
        assert!(!CodexError::UnknownEvent.is_retryable());
    }

    #[test]
    fn codex_known_failures_come_from_real_captures() {
        // Every text below is verbatim from a capture: this host 2026-11-04 (codex-cli
        // 0.155.1) or an earlier app's 0.155.1 fixtures, as the KNOWN table says.
        let failed = |msg: &str| {
            format!(
                "{{\"type\":\"turn.started\"}}\n{{\"type\":\"turn.failed\",\"error\":{{\"message\":{}}}}}",
                serde_json::to_string(msg).unwrap()
            )
        };
        let cases = [
            (
                failed(
                    r#"unexpected status 403 Forbidden: {"detail":{"message":"Unauthorized: Contact your ChatGPT workspace administrator for access.","code":"codex_workspace_access_denied"}}, url: https://chatgpt.com/backend-api/codex/responses, cf-ray: a5512d89fd720201-SYD, request id: 07e7f263-654a-4e06-bfe5-cb53d893d5b2"#,
                ),
                String::new(),
                CodexError::AccessDenied,
            ),
            (
                failed(
                    "unexpected status 401 Unauthorized: Missing bearer or basic authentication in header, url: https://api.openai.com/v1/responses, cf-ray: a5512c602c26aeb5-SYD, request id: req_6140b00352114b7a8a1521381e91add0",
                ),
                String::new(),
                CodexError::SignInExpired,
            ),
            (
                failed(
                    "Codex ran out of room in the model's context window. Start a new thread or clear earlier history before retrying.",
                ),
                String::new(),
                CodexError::PromptTooLong,
            ),
            (
                String::new(),
                r#"Error: turn/start: turn/start failed: Input exceeds the maximum length of 1048576 characters. (code -32602), data: {"input_error_code":"input_too_large","max_chars":1048576,"actual_chars":9000000}"#.to_owned(),
                CodexError::PromptTooLong,
            ),
            (
                String::new(),
                "Error: Unknown feature flag: bogus_feature_xyz".to_owned(),
                CodexError::UnsupportedVersion,
            ),
            (
                String::new(),
                "error: unexpected argument '--bogus-flag' found\n\n  tip: to pass '--bogus-flag' as a value, use '-- --bogus-flag'".to_owned(),
                CodexError::UnsupportedVersion,
            ),
            (
                String::new(),
                "Error loading config.toml: unknown variant `bogus`, expected one of `disabled`, `cached`, `indexed`, `live`\nin `web_search`".to_owned(),
                CodexError::UnsupportedVersion,
            ),
        ];
        for (stdout, stderr, want) in cases {
            assert_eq!(
                parse_output(&output(&stdout, &stderr, false)),
                Err(Parsed::Known(want.clone())),
                "{want:?}"
            );
            assert!(!want.to_string().contains("http"), "{want}");
        }
        // Anything else: unexplained, with a fixed word and the exit code only (synthetic
        // text, standing for any message no capture explains).
        let e = parse_output(&output(&failed("Point 0: we ship the beta"), "", false));
        let Err(Parsed::Unexplained(e)) = e else {
            panic!("{e:?}")
        };
        assert_eq!(
            e.to_string(),
            "Codex reported an error (exit code 1, turn_failed)"
        );
    }

    #[test]
    fn codex_unsupported_version_copy_is_plain() {
        assert_eq!(
            CodexError::UnsupportedVersion.to_string(),
            "this Codex version and Earshot do not agree on an option"
        );
    }

    #[test]
    #[cfg_attr(not(windows), ignore = "Windows paths")]
    fn codex_search_order_skips_relative_path_entries() {
        let search = CliSearch {
            path: Some(std::env::join_paths(["relative", "C:\\abs"]).unwrap()),
            home: Some(PathBuf::from("C:\\home")),
            app_data: Some(PathBuf::from("C:\\appdata")),
            local_app_data: Some(PathBuf::from("C:\\local")),
        };
        // The known install folders first, then PATH (T9.25).
        assert_eq!(
            CodexCli::search_dirs(&search),
            [
                PathBuf::from("C:\\local\\Programs\\OpenAI\\Codex\\bin"),
                PathBuf::from("C:\\appdata\\npm"),
                PathBuf::from("C:\\local\\npm"),
                PathBuf::from("C:\\abs"),
            ]
        );
    }
}
