//! Summaries through the user's installed Claude Code CLI on their Claude.ai sign-in
//! (T7.4ga, PLAN.md 3.9 exception chosen by the owner). No HTTP here: Earshot hands
//! the prompt text to the CLI, which connects to Anthropic itself; Earshot cannot audit that
//! request, so every run records a [`Handover`] (when, how many characters, how it ended;
//! never the text).
//!
//! # Finding the CLI ([`ClaudeCli::find`])
//!
//! `claude.exe` (or `claude` off Windows) in the native installer's `%USERPROFILE%\.local\bin`,
//! then npm's global folders (`%APPDATA%\npm`, `%LOCALAPPDATA%\npm`), then an absolute `PATH`
//! directory: the known install folders come first, so a `claude` earlier on `PATH` (a wrapper
//! someone else put there) is not run when a real install exists (T9.25). An npm `claude.cmd` shim is never run (it would route the
//! arguments through `cmd.exe`): it is resolved to the `claude.exe` it wraps
//! (`node_modules\@anthropic-ai\claude-code\bin\claude.exe`, Claude Code 2 and later) or to
//! `node` plus `cli.js` (older releases), as another desktop app does.
//!
//! # One run ([`ClaudeCodeClient::complete`])
//!
//! `claude --print --output-format json --tools "" --setting-sources ""
//! --strict-mcp-config --disable-slash-commands --no-session-persistence --safe-mode
//! --system-prompt <fixed text> [--model <model>] [--effort <level>]` (every flag checked
//! against `claude --help` of Claude Code 2.1.290; `--effort` against 2.1.292, T12.18):
//! - no tools at all (`--tools ""`), no MCP servers (`--strict-mcp-config` without
//!   `--mcp-config`), no skills (`--disable-slash-commands`), no user, project or local
//!   settings files (`--setting-sources ""`: hooks, plugins, permissions and `env` of the
//!   user are not loaded), and `--safe-mode` (no CLAUDE.md, skills, plugins, hooks, MCP
//!   servers, custom commands or agents, output styles). Measured on 2.1.290 (2026-11-04,
//!   signed in, a CLAUDE.md in the working folder's parent telling the model to answer
//!   "PINEAPPLE"): `--setting-sources project` without `--safe-mode` loaded it (the reply was
//!   PINEAPPLE); `--setting-sources project --safe-mode`, `--setting-sources ""` alone and both
//!   together did not, and the sign-in kept working with `--setting-sources ""`. What can still
//!   apply: admin-managed (policy) settings, which `--safe-mode` and `--restricted` say always
//!   apply, and the user's environment variables except the ones removed below. `--bare` is
//!   not used: it never reads the OAuth sign-in this provider runs on.
//! - `--no-session-persistence`: Claude Code keeps no transcript of the run on disk.
//! - The prompt goes only on standard input ([`stdin_text`]); the command line holds only
//!   fixed text, the model name ([`check_model`]) and an [`Effort`] word from a fixed list.
//!   `--effort` is passed only when the installed CLI's `claude --help` lists it
//!   ([`help_lists_effort`], asked once per client, stopped by the run's cancel token): an
//!   older CLI or an unreadable help runs without it, at its own default effort, and
//!   [`ClaudeCodeClient::effort_probe`] tells the two apart. Claude Code's own system prompt is
//!   replaced by [`SYSTEM_PROMPT`] (fixed; the summary rules travel on standard input).
//! - The working folder is a new, empty temporary folder, deleted afterwards.
//! - `CLAUDECODE` (the marker of a session started from inside Claude Code) and
//!   `CLAUDE_CODE_SIMPLE` (what `--bare` sets: no OAuth) are removed from the child's
//!   environment, and so is every variable Claude Code documents for another API address,
//!   another provider (Bedrock, Vertex, Foundry, Claude Platform on AWS), a credential other
//!   than the stored sign-in, custom headers, a proxy or OpenTelemetry export
//!   ([`CLEARED_ENV`], T9.25): the CLI talks only to its signed-in default. A user signed in only through such a variable (an
//!   `ANTHROPIC_API_KEY` or `CLAUDE_CODE_OAUTH_TOKEN`) reads as not signed in, so the summary
//!   is skipped with a banner. Admin-managed settings can still set any of them (an `env`
//!   block there is outside Earshot's reach); CA and client-certificate variables are kept
//!   (they name no address). `DISABLE_TELEMETRY`, `DISABLE_ERROR_REPORTING` and `DISABLE_AUTOUPDATER`
//!   are set (an earlier app's opt-outs).
//! - Before any text is written to the CLI, [`HandoverSink::started`] is told (the runtime
//!   stores a `started` entry then); if it cannot record, the CLI is stopped unread
//!   ([`ClaudeCodeError::NotRecorded`]). Its end goes to [`HandoverSink::ended`].
//! - The run goes through `earshot_proc::run_with`: no console window, a kill-on-close job object,
//!   the request timeout as its deadline, the run's `CancelToken` polled, and a cut-off
//!   output is a failure, never parsed.
//! - Claude Code's JSON result (`type: "result"`, `is_error`, `result`, `stop_reason`) is the
//!   reply; the summary JSON in `result` is validated and repaired by
//!   [`crate::LlmSummarizer`] exactly as for the HTTP providers (each repair round is
//!   another run with the same restrictions). `--json-schema` exists in this version but is
//!   not used: how it combines with `--tools ""` could not be checked on a working sign-in
//!   (T7.4ga), so the schema travels in the prompt, as in the `text` response mode.
//!
//! # Errors
//!
//! Known CLI messages map to [`ClaudeCodeError`] variants with fixed wording (the CLI's
//! output can carry model text, so it never enters an error), each from a real capture:
//! "Failed to authenticate" (this host, 2026-11-04, 2.1.290, in an `is_error` result),
//! the OAuth refresh clash and "Prompt is too long" (an earlier app's fixtures, 2.1.283), the 10 MB
//! standard input limit (an earlier app, 2.1.283) and commander's "unknown option" (2.1.290). Any
//! other failure asks `claude auth status --json` once: `loggedIn: false` is
//! [`ClaudeCodeError::NotSignedIn`].

use std::ffi::OsString;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use earshot_core::asr::CancelToken;
use earshot_core::identity::Clock;
use earshot_core::prompt::{ChatMessage, ContextBudget, Role};
use earshot_proc::{ChildError, ChildOutput, ChildSpec};
use serde_json::Value;

use crate::client::Completion;
use crate::config::DEFAULT_REQUEST_TIMEOUT;
use crate::error::LlmError;
use crate::handover::TempFolder;
pub use crate::handover::{Handover, HandoverLog, HandoverOutcome, HandoverSink};

/// Prefix of the working folders.
const TEMP_PREFIX: &str = "earshot-claude";

/// Context budget of a Claude Code summary: current Claude models have 200k-token windows
/// (as [`crate::ANTHROPIC_DEFAULT_BUDGET`]); Claude Code chooses the reply length itself, so
/// the reply budget only sizes the request plan.
pub const CLAUDE_CODE_BUDGET: ContextBudget = ContextBudget {
    context_window: 200_000,
    max_output_tokens: 16_000,
};

/// Deadline of `claude --version` and `claude auth status`.
pub const STATUS_TIMEOUT: Duration = Duration::from_secs(20);

/// Most bytes of each CLI output kept; a longer output is a cut-off failure.
pub const MAX_CLI_OUTPUT: usize = 8 * 1024 * 1024;

/// Longest model name passed to `--model`.
pub const MAX_MODEL_NAME: usize = 200;

/// Claude Code's system prompt for a summary run (replaces its coding-assistant prompt). Fixed
/// text: the summary rules and the meeting travel on standard input.
pub const SYSTEM_PROMPT: &str = "You write meeting summaries for Earshot, a meeting notes app. \
The input is a request in parts: a system part with the rules, a user part with the meeting, \
and sometimes your earlier reply with a correction. Follow the system part exactly and answer \
the last user part with only the JSON object it asks for, nothing before or after it.";

/// Variables removed from the CLI's environment (module docs; T9.25, the owner's decision of
/// 2026-11-12: the CLI talks only to its signed-in default, never to an address, account or
/// provider a variable of the user's names). Every name comes from Claude Code's own
/// documentation, read 2026-11-12 for Claude Code 2.1.292: the environment variables
/// reference <https://code.claude.com/docs/en/env-vars> unless marked otherwise.
/// `CLAUDE_CONFIG_DIR` is kept on purpose: it chooses which of the user's own stored sign-ins
/// runs (as `CODEX_HOME` does for Codex). A deny list over a vendor table that grows each
/// release can fall behind; re-read the reference on every Claude Code version change.
pub const CLEARED_ENV: [&str; 61] = [
    // Not about the network (T7.4ga): the marker of a session started from inside Claude
    // Code, and what `--bare` sets (no OAuth; named by `claude --help`).
    "CLAUDECODE",
    "CLAUDE_CODE_SIMPLE",
    // Another address for the API.
    "ANTHROPIC_BASE_URL",
    "ANTHROPIC_BEDROCK_BASE_URL",
    "ANTHROPIC_BEDROCK_MANTLE_BASE_URL",
    "ANTHROPIC_AWS_BASE_URL",
    "ANTHROPIC_FOUNDRY_BASE_URL",
    "ANTHROPIC_FOUNDRY_RESOURCE",
    "ANTHROPIC_VERTEX_BASE_URL",
    // Another provider (Amazon Bedrock, its Mantle endpoint, Claude Platform on AWS,
    // Google Cloud's Agent Platform (Vertex), Microsoft Foundry) and its skipped sign-in.
    "CLAUDE_CODE_USE_BEDROCK",
    "CLAUDE_CODE_USE_MANTLE",
    "CLAUDE_CODE_USE_ANTHROPIC_AWS",
    "CLAUDE_CODE_USE_VERTEX",
    "CLAUDE_CODE_USE_FOUNDRY",
    "CLAUDE_CODE_SKIP_BEDROCK_AUTH",
    "CLAUDE_CODE_SKIP_MANTLE_AUTH",
    "CLAUDE_CODE_SKIP_ANTHROPIC_AWS_AUTH",
    "CLAUDE_CODE_SKIP_VERTEX_AUTH",
    "CLAUDE_CODE_SKIP_FOUNDRY_AUTH",
    // Credentials other than the stored sign-in (another account, or a gateway's), and
    // headers that can carry one.
    "ANTHROPIC_API_KEY",
    "ANTHROPIC_AUTH_TOKEN",
    "ANTHROPIC_CUSTOM_HEADERS",
    "CLAUDE_CODE_OAUTH_TOKEN",
    "ANTHROPIC_PROFILE",
    "ANTHROPIC_FEDERATION_RULE_ID",
    "ANTHROPIC_ORGANIZATION_ID",
    "ANTHROPIC_WORKSPACE_ID",
    "ANTHROPIC_AWS_API_KEY",
    "ANTHROPIC_AWS_WORKSPACE_ID",
    "ANTHROPIC_FOUNDRY_API_KEY",
    "ANTHROPIC_FOUNDRY_AUTH_TOKEN",
    "AWS_BEARER_TOKEN_BEDROCK",
    // Used only by `claude auth login`, which Earshot never runs; cleared with the token
    // above so no alternative credential reaches the child.
    "CLAUDE_CODE_OAUTH_REFRESH_TOKEN",
    "CLAUDE_CODE_OAUTH_SCOPES",
    // OpenTelemetry export, switched on and aimed from the environment, which can carry the
    // prompt (`OTEL_LOG_USER_PROMPTS`, `OTEL_LOG_RAW_API_BODIES`) to a collector of the
    // user's; `DISABLE_TELEMETRY` does not cover it (found by review: T9.25). Names from the
    // reference and <https://code.claude.com/docs/en/monitoring-usage>.
    "CLAUDE_CODE_ENABLE_TELEMETRY",
    "CLAUDE_CODE_ENHANCED_TELEMETRY_BETA",
    "ENABLE_ENHANCED_TELEMETRY_BETA",
    "ENABLE_BETA_TRACING_DETAILED",
    "BETA_TRACING_ENDPOINT",
    "OTEL_METRICS_EXPORTER",
    "OTEL_LOGS_EXPORTER",
    "OTEL_TRACES_EXPORTER",
    "OTEL_EXPORTER_OTLP_ENDPOINT",
    "OTEL_EXPORTER_OTLP_METRICS_ENDPOINT",
    "OTEL_EXPORTER_OTLP_LOGS_ENDPOINT",
    "OTEL_EXPORTER_OTLP_TRACES_ENDPOINT",
    "OTEL_EXPORTER_OTLP_HEADERS",
    "OTEL_EXPORTER_OTLP_METRICS_HEADERS",
    "OTEL_EXPORTER_OTLP_LOGS_HEADERS",
    "OTEL_EXPORTER_OTLP_TRACES_HEADERS",
    "OTEL_LOG_USER_PROMPTS",
    "OTEL_LOG_ASSISTANT_RESPONSES",
    "OTEL_LOG_TOOL_DETAILS",
    "OTEL_LOG_TOOL_CONTENT",
    "OTEL_LOG_RAW_API_BODIES",
    // Proxies: the reference and <https://code.claude.com/docs/en/network-config>, which says
    // the lower-case forms work too (Windows names ignore case; the lower-case entries are
    // for other systems).
    "HTTPS_PROXY",
    "HTTP_PROXY",
    "NO_PROXY",
    "https_proxy",
    "http_proxy",
    "no_proxy",
];

/// Variables set in the CLI's environment (module docs).
const SET_ENV: [(&str, &str); 3] = [
    ("DISABLE_TELEMETRY", "1"),
    ("DISABLE_ERROR_REPORTING", "1"),
    ("DISABLE_AUTOUPDATER", "1"),
];

/// A known CLI message: every needle must occur (ASCII case ignored).
struct Known {
    needles: &'static [&'static str],
    error: fn() -> ClaudeCodeError,
}

/// The CLI messages this provider recognises (module docs; first match wins, so the
/// refresh clash, which ends "or sign in again", comes before the sign-in failure).
const KNOWN: &[Known] = &[
    // Earlier app's fixture oneshot_oauth_refresh_race (2026-10-05): on stdout, exit 1.
    Known {
        needles: &[
            "failed to refresh oauth token",
            "another claude code process is refreshing it",
        ],
        error: || ClaudeCodeError::SignInBusy,
    },
    // This host, 2026-11-04, 2.1.290: `is_error` result "Failed to authenticate: OAuth
    // session expired and could not be refreshed", exit 1.
    Known {
        needles: &["failed to authenticate"],
        error: || ClaudeCodeError::SignInExpired,
    },
    // Earlier app's fixtures oneshot_prompt_too_long* (2.1.283): "Prompt is too long", exit 1.
    Known {
        needles: &["prompt is too long"],
        error: || ClaudeCodeError::PromptTooLong,
    },
    // Earlier app's fixture oneshot_stdin_over_10mb (2.1.283): "Error: piped stdin input exceeds
    // 10MB. ..." on stderr, exit 1.
    Known {
        needles: &["piped stdin input", "exceeds 10mb"],
        error: || ClaudeCodeError::PromptTooLong,
    },
    // This host, 2026-11-04, 2.1.290: "error: unknown option '--bogus-flag'" on stderr,
    // exit 1 (commander).
    Known {
        needles: &["error: unknown option"],
        error: || ClaudeCodeError::UnsupportedVersion,
    },
];

/// Why a Claude Code run gave no reply. The wording is fixed: no CLI output is ever part of
/// it (it can carry model text).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClaudeCodeError {
    /// No Claude Code CLI was found ([`ClaudeCli::find`]).
    NotInstalled,
    /// Claude Code is not signed in (`claude auth status`: `loggedIn: false`).
    NotSignedIn,
    /// Claude Code is signed in but its sign-in could not be used ("Failed to
    /// authenticate", for example an expired session that could not be refreshed).
    SignInExpired,
    /// Another Claude Code process was refreshing the sign-in at the same moment (retry in a
    /// minute).
    SignInBusy,
    /// The meeting is longer than Claude Code accepts.
    PromptTooLong,
    /// This Claude Code version refuses an option Earshot uses.
    UnsupportedVersion,
    /// The CLI could not be started (the reason from the operating system).
    Start(String),
    /// The CLI did not finish within the deadline and was stopped.
    TimedOut(Duration),
    /// The CLI's output was cut off (or longer than [`MAX_CLI_OUTPUT`]).
    OutputIncomplete,
    /// Earshot could not record the handover locally, so the CLI was stopped before any
    /// text was handed to it.
    NotRecorded,
    /// The CLI's output is not its JSON result (what was wrong, never the output).
    BadOutput(&'static str),
    /// Any other failure: the exit code and Claude Code's own error kind when it gave one.
    Failed {
        /// The exit code, if any.
        exit_code: Option<i32>,
        /// `terminal_reason` or `subtype` of the result (a lower-case word), if any.
        reason: Option<String>,
    },
}

impl ClaudeCodeError {
    /// Whether the user has to sign in to Claude Code for a run to work.
    #[must_use]
    pub fn needs_sign_in(&self) -> bool {
        matches!(self, Self::NotSignedIn | Self::SignInExpired)
    }

    /// A fixed lower-case name of the variant (for records that must hold no text):
    /// `not_installed`, `not_signed_in`, `sign_in_expired`, `sign_in_busy`,
    /// `prompt_too_long`, `unsupported_version`, `start`, `timed_out`, `output_incomplete`,
    /// `bad_output`, `failed`.
    #[must_use]
    pub fn kind(&self) -> &'static str {
        match self {
            Self::NotInstalled => "not_installed",
            Self::NotSignedIn => "not_signed_in",
            Self::SignInExpired => "sign_in_expired",
            Self::SignInBusy => "sign_in_busy",
            Self::PromptTooLong => "prompt_too_long",
            Self::UnsupportedVersion => "unsupported_version",
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
        matches!(self, Self::SignInBusy | Self::TimedOut(_))
    }
}

impl fmt::Display for ClaudeCodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotInstalled => f.write_str("Claude Code is not installed on this computer"),
            Self::NotSignedIn => f.write_str("Claude Code is not signed in"),
            Self::SignInExpired => {
                f.write_str("Claude Code could not use its sign-in (it may have expired)")
            }
            Self::SignInBusy => f.write_str(
                "another Claude Code window was renewing the sign-in at the same moment; try again in a minute",
            ),
            Self::PromptTooLong => f.write_str("the meeting is longer than Claude Code accepts"),
            Self::UnsupportedVersion => f.write_str(
                "this Claude Code version does not accept an option Earshot uses; update Claude Code",
            ),
            Self::Start(e) => write!(f, "Claude Code could not be started: {e}"),
            Self::TimedOut(d) => {
                write!(f, "Claude Code did not answer within {} s", d.as_secs())
            }
            Self::OutputIncomplete => f.write_str("Claude Code's answer was cut off"),
            Self::NotRecorded => f.write_str(
                "Earshot could not record the handover on this computer, so nothing was handed to Claude Code",
            ),
            Self::BadOutput(why) => write!(f, "Claude Code's answer is not usable: {why}"),
            Self::Failed { exit_code, reason } => {
                f.write_str("Claude Code reported an error")?;
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

impl std::error::Error for ClaudeCodeError {}

impl From<ClaudeCodeError> for LlmError {
    fn from(e: ClaudeCodeError) -> Self {
        LlmError::ClaudeCode(e)
    }
}

/// Why `name` cannot be passed to `--model` (empty is allowed: the CLI's own default is
/// used, and no `--model` is passed). Letters, digits and `-._:[]` only (aliases such as
/// `opus`, full ids such as `claude-opus-4-1-20250805`, the `[1m]` suffix), at most
/// [`MAX_MODEL_NAME`] characters, never starting with `-` (it would read as an option).
///
/// # Errors
/// Why the name is refused (plain words, never the name).
pub fn check_model(name: &str) -> Result<(), &'static str> {
    if name.len() > MAX_MODEL_NAME {
        return Err("it is longer than 200 characters");
    }
    if name.starts_with('-') {
        return Err("it must not start with a hyphen");
    }
    if !name
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b"-._:[]".contains(&b))
    {
        return Err("use only letters, digits, hyphens, dots, colons and square brackets");
    }
    Ok(())
}

/// A directly runnable Claude Code CLI: the executable and any arguments it needs before
/// Earshot's own (`cli.js` for an older npm install run by `node`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClaudeCli {
    /// The executable.
    pub program: PathBuf,
    /// Arguments before Earshot's own.
    pub leading_args: Vec<OsString>,
}

/// Where [`ClaudeCli::find_with`] looks (the process environment for [`ClaudeCli::find`]).
#[derive(Debug, Clone, Default)]
pub struct CliSearch {
    /// `PATH`.
    pub path: Option<OsString>,
    /// The user's home folder (`USERPROFILE` on Windows, `HOME` elsewhere).
    pub home: Option<PathBuf>,
    /// `APPDATA` (Windows).
    pub app_data: Option<PathBuf>,
    /// `LOCALAPPDATA` (Windows).
    pub local_app_data: Option<PathBuf>,
}

impl CliSearch {
    /// The search of this process's environment.
    #[must_use]
    pub fn from_env() -> Self {
        let dir = |name: &str| std::env::var_os(name).map(PathBuf::from);
        Self {
            path: std::env::var_os("PATH"),
            home: if cfg!(windows) {
                dir("USERPROFILE")
            } else {
                dir("HOME")
            },
            app_data: dir("APPDATA"),
            local_app_data: dir("LOCALAPPDATA"),
        }
    }

    /// The directories searched, in order (module docs): the known install folders, then
    /// `PATH`.
    #[must_use]
    pub fn dirs(&self) -> Vec<PathBuf> {
        let mut dirs = Vec::new();
        if let Some(home) = &self.home {
            dirs.push(home.join(".local").join("bin"));
        }
        dirs.extend(self.npm_dirs());
        dirs.extend(self.path_dirs());
        dirs
    }

    /// npm's global folders (`%APPDATA%\npm`, then `%LOCALAPPDATA%\npm`), on Windows only.
    #[must_use]
    pub fn npm_dirs(&self) -> Vec<PathBuf> {
        if !cfg!(windows) {
            return Vec::new();
        }
        [&self.app_data, &self.local_app_data]
            .into_iter()
            .flatten()
            .map(|d| d.join("npm"))
            .collect()
    }

    /// The absolute `PATH` entries, in order. Relative entries are left out (they would
    /// resolve against the current folder).
    #[must_use]
    pub fn path_dirs(&self) -> Vec<PathBuf> {
        self.path
            .as_ref()
            .map(|p| {
                std::env::split_paths(p)
                    .filter(|d| d.is_absolute())
                    .collect()
            })
            .unwrap_or_default()
    }
}

impl ClaudeCli {
    /// The CLI found from this process's environment, `None` when there is none.
    #[must_use]
    pub fn find() -> Option<Self> {
        Self::find_with(&CliSearch::from_env())
    }

    /// The CLI found in `search` (module docs).
    #[must_use]
    pub fn find_with(search: &CliSearch) -> Option<Self> {
        let dirs = search.dirs();
        let exe = if cfg!(windows) {
            "claude.exe"
        } else {
            "claude"
        };
        for dir in &dirs {
            let path = dir.join(exe);
            if path.is_file() {
                return Some(Self {
                    program: path,
                    leading_args: Vec::new(),
                });
            }
            if cfg!(windows) {
                let shim = dir.join("claude.cmd");
                if shim.is_file()
                    && let Some(cli) = Self::from_npm_shim(&shim, &dirs)
                {
                    return Some(cli);
                }
            }
        }
        None
    }

    /// What an npm `claude.cmd` shim in `shim`'s folder runs: the package's `claude.exe`, or
    /// `node` (from `dirs`) with its `cli.js`; `None` when neither is there.
    fn from_npm_shim(shim: &Path, dirs: &[PathBuf]) -> Option<Self> {
        let package = shim
            .parent()?
            .join("node_modules")
            .join("@anthropic-ai")
            .join("claude-code");
        let exe = package.join("bin").join("claude.exe");
        if exe.is_file() {
            return Some(Self {
                program: exe,
                leading_args: Vec::new(),
            });
        }
        let script = package.join("cli.js");
        let node = dirs
            .iter()
            .map(|d| d.join("node.exe"))
            .find(|p| p.is_file())?;
        script.is_file().then(|| Self {
            program: node,
            leading_args: vec![script.into_os_string()],
        })
    }

    /// A child run of the CLI with `args` after the leading ones (module docs: environment,
    /// no window), in `dir`, with `stdin` and `deadline`.
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
            env_set: SET_ENV
                .iter()
                .map(|(k, v)| (OsString::from(k), OsString::from(v)))
                .collect(),
            stdin,
            deadline,
            max_output: MAX_CLI_OUTPUT,
        }
    }

    /// A quick CLI query (`--version`, `auth status`) in an empty temporary folder.
    fn query(&self, args: &[&str], timeout: Duration) -> Result<ChildOutput, ClaudeCodeError> {
        self.query_until(args, timeout, &|| false)
            .map_err(|e| e.unwrap_or(ClaudeCodeError::OutputIncomplete))
    }

    /// [`Self::query`], stopped as soon as `cancelled` says so: `Err(None)` then.
    fn query_until(
        &self,
        args: &[&str],
        timeout: Duration,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<ChildOutput, Option<ClaudeCodeError>> {
        let dir = TempFolder::new(&std::env::temp_dir(), TEMP_PREFIX)
            .map_err(|e| Some(ClaudeCodeError::Start(e.to_string())))?;
        let spec = self.spec(
            args.iter().map(OsString::from).collect(),
            dir.path(),
            Vec::new(),
            timeout,
        );
        let out = match earshot_proc::run(&spec, cancelled) {
            Ok(out) => out,
            Err(ChildError::Cancelled) => return Err(None),
            Err(e) => return Err(Some(child_error(e))),
        };
        out.require_complete()
            .map_err(|_| Some(ClaudeCodeError::OutputIncomplete))
    }

    /// The CLI's version (`claude --version`, first line, for example
    /// `2.1.290 (Claude Code)`).
    ///
    /// # Errors
    /// The CLI cannot be run, fails or prints nothing.
    pub fn version(&self, timeout: Duration) -> Result<String, ClaudeCodeError> {
        let out = self.query(&["--version"], timeout)?;
        let text = String::from_utf8_lossy(&out.stdout);
        let line = text.lines().next().unwrap_or("").trim();
        if !out.status.success() || line.is_empty() {
            return Err(ClaudeCodeError::Failed {
                exit_code: out.status.code(),
                reason: None,
            });
        }
        // A version line is short; anything else is not shown.
        Ok(line.chars().take(80).collect())
    }

    /// Whether the CLI takes `--effort` (`claude --help` lists it, [`help_lists_effort`]);
    /// `None` when the help could not be read (exit code not 0, cut off, not started).
    #[must_use]
    pub fn accepts_effort(&self, timeout: Duration) -> Option<bool> {
        match self.effort_probe(timeout, &|| false)? {
            EffortProbe::Listed => Some(true),
            EffortProbe::NotListed => Some(false),
            EffortProbe::Unreadable => None,
        }
    }

    /// What `claude --help` says of `--effort`, stopped as soon as `cancelled` says so (`None`
    /// then: no answer).
    #[must_use]
    pub fn effort_probe(
        &self,
        timeout: Duration,
        cancelled: &dyn Fn() -> bool,
    ) -> Option<EffortProbe> {
        match self.query_until(&["--help"], timeout, cancelled) {
            Err(None) => None,
            Err(Some(_)) => Some(EffortProbe::Unreadable),
            Ok(out) if !out.status.success() => Some(EffortProbe::Unreadable),
            Ok(out) if help_lists_effort(&String::from_utf8_lossy(&out.stdout)) => {
                Some(EffortProbe::Listed)
            }
            Ok(_) => Some(EffortProbe::NotListed),
        }
    }

    /// Whether the CLI is signed in (`claude auth status --json`, which never signs in).
    #[must_use]
    pub fn sign_in(&self, timeout: Duration) -> SignIn {
        match self.query(&["auth", "status", "--json"], timeout) {
            Ok(out) => sign_in_of(&out.stdout),
            Err(e) => SignIn::Unknown(e.to_string()),
        }
    }
}

/// Claude Code's sign-in state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SignIn {
    /// Signed in (`authMethod`, for example `claude.ai`, when given).
    SignedIn {
        /// How (`authMethod`).
        method: Option<String>,
    },
    /// Not signed in.
    NotSignedIn,
    /// The check failed (why).
    Unknown(String),
}

/// The sign-in of `claude auth status --json` output (`loggedIn`, `authMethod`; captured on
/// this host with 2.1.290, which exits 1 with `loggedIn: false` when not signed in, so the
/// exit code is not read).
#[must_use]
pub fn sign_in_of(stdout: &[u8]) -> SignIn {
    let Ok(v) = serde_json::from_slice::<Value>(stdout) else {
        return SignIn::Unknown("Claude Code's sign-in status is not JSON".into());
    };
    match v.get("loggedIn").and_then(Value::as_bool) {
        Some(true) => SignIn::SignedIn {
            method: v
                .get("authMethod")
                .and_then(Value::as_str)
                .filter(|m| m.len() <= 40 && m.bytes().all(|b| b.is_ascii_graphic()))
                .map(str::to_owned),
        },
        Some(false) => SignIn::NotSignedIn,
        None => SignIn::Unknown("Claude Code's sign-in status has no loggedIn field".into()),
    }
}

fn child_error(e: ChildError) -> ClaudeCodeError {
    match e {
        ChildError::Start(e) | ChildError::Job(e) | ChildError::Wait(e) => {
            ClaudeCodeError::Start(e.to_string())
        }
        ChildError::TimedOut(d) => ClaudeCodeError::TimedOut(d),
        ChildError::OutputIncomplete { .. } => ClaudeCodeError::OutputIncomplete,
        ChildError::Aborted => ClaudeCodeError::NotRecorded,
        // Mapped by the caller (it knows whether the run's token was set).
        ChildError::Cancelled => ClaudeCodeError::Failed {
            exit_code: None,
            reason: Some("cancelled".into()),
        },
    }
}

/// How hard Claude Code works on a reply (`--effort <level>`, T12.18): one of the fixed words
/// `claude --help` lists for Claude Code 2.1.292 (`low, medium, high, xhigh, max`), never free
/// text. A real run on this host (2026-11-10, 2.1.292, signed in) answered with each of the
/// five and no warning; an unknown word is ignored by the CLI with a warning, so only these
/// are ever passed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Effort {
    /// `low`.
    Low,
    /// `medium`.
    Medium,
    /// `high`.
    High,
    /// `xhigh` ("Extra high" on the page).
    ExtraHigh,
    /// `max`.
    Max,
}

impl Effort {
    /// Every level, lowest first.
    pub const ALL: [Self; 5] = [
        Self::Low,
        Self::Medium,
        Self::High,
        Self::ExtraHigh,
        Self::Max,
    ];

    /// The word passed to `--effort` (and stored): `low`, `medium`, `high`, `xhigh`, `max`.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::ExtraHigh => "xhigh",
            Self::Max => "max",
        }
    }

    /// Reads [`Effort::as_str`] exactly.
    #[must_use]
    pub fn parse(word: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|e| e.as_str() == word)
    }
}

/// What the installed CLI's `claude --help` said of `--effort` (T12.18, T12.20).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EffortProbe {
    /// It lists `--effort`: a chosen effort is passed.
    Listed,
    /// It does not (an older Claude Code): runs go at its own default effort.
    NotListed,
    /// The help could not be read (did not start, failed, timed out or was cut off): runs go at
    /// its own default effort, so an older CLI never sees an option it would refuse.
    Unreadable,
}

/// Whether `claude --help` output lists the `--effort` option: a line that starts (after its
/// indent) with `--effort` followed by a space, `<` or the end of the line. Older Claude Code
/// versions have no such line; a run there omits the flag (an unknown option fails the run).
#[must_use]
pub fn help_lists_effort(help: &str) -> bool {
    help.lines().any(|line| {
        line.trim_start()
            .strip_prefix("--effort")
            .is_some_and(|rest| rest.is_empty() || rest.starts_with([' ', '<', '\t']))
    })
}

/// The arguments of a summary run after the CLI's leading ones (module docs): `model`
/// `None` uses the CLI's default.
#[must_use]
pub fn run_args(model: Option<&str>) -> Vec<OsString> {
    run_args_with(model, None)
}

/// [`run_args`] with `--effort <level>` after `--model` when `effort` is given (only for a
/// CLI whose `--help` lists it: [`ClaudeCodeClient`] checks once).
#[must_use]
pub fn run_args_with(model: Option<&str>, effort: Option<Effort>) -> Vec<OsString> {
    let mut args: Vec<OsString> = [
        "--print",
        "--output-format",
        "json",
        "--tools",
        "",
        "--setting-sources",
        "",
        "--strict-mcp-config",
        "--disable-slash-commands",
        "--no-session-persistence",
        "--safe-mode",
        "--system-prompt",
        SYSTEM_PROMPT,
    ]
    .iter()
    .map(OsString::from)
    .collect();
    if let Some(m) = model {
        args.push("--model".into());
        args.push(m.into());
    }
    if let Some(e) = effort {
        args.push("--effort".into());
        args.push(e.as_str().into());
    }
    args
}

/// The request as Claude Code reads it on standard input: each message under a line naming
/// its part (`system`, `user`, `your earlier reply`).
#[must_use]
pub fn stdin_text(messages: &[ChatMessage]) -> String {
    let mut out = String::from(
        "An Earshot summary request follows, in parts. Each part starts with a line naming it.\n",
    );
    for m in messages {
        let name = match m.role {
            Role::System => "system",
            Role::User => "user",
            Role::Assistant => "your earlier reply",
        };
        out.push_str("\n===== ");
        out.push_str(name);
        out.push_str(" =====\n");
        out.push_str(&m.content);
        out.push('\n');
    }
    out
}

/// Settings of [`ClaudeCodeClient`].
#[derive(Debug, Clone)]
pub struct ClaudeCodeConfig {
    /// The CLI.
    pub cli: ClaudeCli,
    /// `--model` (checked with [`check_model`]); `None`: the CLI's default.
    pub model: Option<String>,
    /// `--effort` (T12.18); `None`: the CLI's default. Passed only when the CLI's `--help`
    /// lists the option, asked once per client before its first run.
    pub effort: Option<Effort>,
    /// Deadline of each run.
    pub request_timeout: Duration,
    /// Context budget for the request plan.
    pub budget: ContextBudget,
    /// Where the empty working folders are made (default: the system temp folder).
    pub temp_root: PathBuf,
}

impl ClaudeCodeConfig {
    /// `cli` with `model` and the defaults ([`DEFAULT_REQUEST_TIMEOUT`],
    /// [`CLAUDE_CODE_BUDGET`], the system temp folder).
    ///
    /// # Errors
    /// A model name [`check_model`] refuses: [`LlmError::ClaudeCode`] with
    /// [`ClaudeCodeError::Start`] (nothing is run).
    pub fn new(cli: ClaudeCli, model: Option<String>) -> Result<Self, LlmError> {
        let model = model.filter(|m| !m.is_empty());
        if let Some(m) = &model {
            check_model(m).map_err(|why| {
                ClaudeCodeError::Start(format!("the model name cannot be used: {why}"))
            })?;
        }
        Ok(Self {
            cli,
            model,
            effort: None,
            request_timeout: DEFAULT_REQUEST_TIMEOUT,
            budget: CLAUDE_CODE_BUDGET,
            temp_root: std::env::temp_dir(),
        })
    }
}

/// Summary requests through the Claude Code CLI (module docs). Cheap to clone; clones share
/// the [`HandoverLog`].
#[derive(Clone)]
pub struct ClaudeCodeClient {
    config: ClaudeCodeConfig,
    clock: Arc<dyn Clock + Send + Sync>,
    log: HandoverLog,
    sink: Arc<Mutex<Option<Arc<dyn HandoverSink>>>>,
    /// What the CLI's help said of `--effort`, asked once (only when an effort is set); a help
    /// stopped by a cancel leaves it unasked.
    effort_probe: Arc<Mutex<Option<EffortProbe>>>,
}

impl fmt::Debug for ClaudeCodeClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ClaudeCodeClient")
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

impl ClaudeCodeClient {
    /// A client for `config`, timestamping its handovers with `clock`.
    pub fn new(config: ClaudeCodeConfig, clock: Arc<dyn Clock + Send + Sync>) -> Self {
        Self {
            config,
            clock,
            log: HandoverLog::new(),
            sink: Arc::new(Mutex::new(None)),
            effort_probe: Arc::new(Mutex::new(None)),
        }
    }

    /// The effort to pass: the configured one when the CLI's `--help` lists `--effort`
    /// (asked once per client and its clones, within [`STATUS_TIMEOUT`]; a help that cannot be
    /// read counts as not listed, so an older CLI never sees an option it would refuse, and
    /// [`Self::effort_probe`] says so). The help stops on `cancel` and is then asked again by
    /// the next run.
    ///
    /// # Errors
    /// [`LlmError::Cancelled`] when `cancel` stopped the help.
    fn effort(&self, cancel: &CancelToken) -> Result<Option<Effort>, LlmError> {
        let Some(effort) = self.config.effort else {
            return Ok(None);
        };
        // Held across the help so clones ask once; a run waits at most the help's deadline.
        let mut probe = self
            .effort_probe
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let answer = match *probe {
            Some(p) => p,
            None => {
                let p = self
                    .config
                    .cli
                    .effort_probe(STATUS_TIMEOUT, &|| cancel.is_cancelled())
                    .ok_or(LlmError::Cancelled)?;
                *probe = Some(p);
                p
            }
        };
        Ok((answer == EffortProbe::Listed).then_some(effort))
    }

    /// What the CLI's help said of `--effort` for the chosen effort: `None` when no effort is
    /// chosen or no run has asked yet. [`EffortProbe::Unreadable`] means the runs went at
    /// Claude Code's own default effort without the help saying the CLI is too old (T12.20).
    #[must_use]
    pub fn effort_probe(&self) -> Option<EffortProbe> {
        self.config.effort?;
        *self
            .effort_probe
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// Sets (or clears) where handovers are reported as they happen; shared by clones.
    pub fn set_sink(&self, sink: Option<Arc<dyn HandoverSink>>) {
        *self.sink.lock().unwrap_or_else(PoisonError::into_inner) = sink;
    }

    /// The settings.
    pub fn config(&self) -> &ClaudeCodeConfig {
        &self.config
    }

    /// A handle to the handover log (the privacy record).
    pub fn handovers(&self) -> HandoverLog {
        self.log.clone()
    }

    /// One run with `messages` (module docs). Blocks until the CLI ends; returns
    /// [`LlmError::Cancelled`] as soon as `cancel` is set (the CLI is stopped).
    ///
    /// # Errors
    /// [`LlmError::ClaudeCode`]; [`LlmError::Cancelled`].
    pub fn complete(
        &self,
        messages: &[ChatMessage],
        cancel: &CancelToken,
    ) -> Result<Completion, LlmError> {
        if cancel.is_cancelled() {
            return Err(LlmError::Cancelled);
        }
        let effort = self.effort(cancel)?;
        let text = stdin_text(messages);
        let characters = text.chars().count();
        let dir = TempFolder::new(&self.config.temp_root, TEMP_PREFIX)
            .map_err(|e| ClaudeCodeError::Start(format!("no working folder: {e}")))?;
        let spec = self.config.cli.spec(
            run_args_with(self.config.model.as_deref(), effort),
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
                return Err(ClaudeCodeError::Start(e.to_string()).into());
            }
            Err(ChildError::Aborted) => return Err(ClaudeCodeError::NotRecorded.into()),
            Err(ChildError::Cancelled) => Err(LlmError::Cancelled),
            Err(e) => Err(LlmError::ClaudeCode(child_error(e))),
            Ok(out) => self.reply_of(out),
        };
        let handover = Handover {
            timestamp_ms,
            characters,
            outcome: match &result {
                Ok(c) => HandoverOutcome::Answered {
                    characters: c.content.chars().count(),
                },
                Err(LlmError::Cancelled) => HandoverOutcome::Cancelled,
                Err(LlmError::ClaudeCode(e)) => HandoverOutcome::Failed(e.clone()),
                // `complete` returns only Claude Code errors and cancellation.
                Err(_) => HandoverOutcome::Failed(ClaudeCodeError::Failed {
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

    /// The reply of a finished run, or why there is none (a failure the known messages do
    /// not explain asks for the sign-in state once).
    fn reply_of(&self, out: ChildOutput) -> Result<Completion, LlmError> {
        match parse_output(&out) {
            Ok(c) => Ok(c),
            Err(Parsed::Known(e)) => Err(e.into()),
            Err(Parsed::Unexplained(e)) => {
                if self.config.cli.sign_in(STATUS_TIMEOUT) == SignIn::NotSignedIn {
                    Err(ClaudeCodeError::NotSignedIn.into())
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
    /// A known message, or a cut-off output (nothing to ask about).
    Known(ClaudeCodeError),
    /// A failure no known message explains (the sign-in may be the reason).
    Unexplained(ClaudeCodeError),
}

/// The first known message in `texts`.
fn known_message(texts: &[&str]) -> Option<ClaudeCodeError> {
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

/// A short lower-case word from the result (`terminal_reason`, else `subtype`), never free
/// text.
fn reason_word(v: &Value) -> Option<String> {
    ["terminal_reason", "subtype"]
        .iter()
        .filter_map(|k| v.get(*k).and_then(Value::as_str))
        .find(|w| {
            !w.is_empty() && w.len() <= 40 && w.bytes().all(|b| b.is_ascii_lowercase() || b == b'_')
        })
        .map(str::to_owned)
}

/// The reply in a finished run's output: complete output, exit 0, one JSON result object
/// with `is_error: false` and a `result` string. Exposed for tests.
///
/// # Errors
/// [`Parsed`].
pub fn parse_output(out: &ChildOutput) -> Result<Completion, Parsed> {
    if !out.complete() {
        return Err(Parsed::Known(ClaudeCodeError::OutputIncomplete));
    }
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    let envelope = serde_json::from_str::<Value>(stdout.trim())
        .ok()
        .filter(|v| v.get("type").and_then(Value::as_str) == Some("result"));
    let exit_code = out.status.code();
    let Some(v) = envelope else {
        if let Some(e) = known_message(&[&stdout, &stderr]) {
            return Err(Parsed::Known(e));
        }
        if out.status.success() {
            return Err(Parsed::Known(ClaudeCodeError::BadOutput(
                "it is not Claude Code's JSON result",
            )));
        }
        return Err(Parsed::Unexplained(ClaudeCodeError::Failed {
            exit_code,
            reason: None,
        }));
    };
    let result = v.get("result").and_then(Value::as_str);
    let is_error = v.get("is_error").and_then(Value::as_bool);
    match (is_error, result) {
        (Some(false), Some(text)) if out.status.success() => Ok(Completion {
            content: text.to_owned(),
            finish_reason: v
                .get("stop_reason")
                .and_then(Value::as_str)
                .map(str::to_owned),
        }),
        (None, _) | (Some(false), None) if out.status.success() => Err(Parsed::Known(
            ClaudeCodeError::BadOutput("the result has no reply text"),
        )),
        _ => {
            if let Some(e) = known_message(&[result.unwrap_or(""), &stderr]) {
                return Err(Parsed::Known(e));
            }
            Err(Parsed::Unexplained(ClaudeCodeError::Failed {
                exit_code,
                reason: reason_word(&v),
            }))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claude_code_model_names_are_checked() {
        for ok in [
            "",
            "opus",
            "sonnet",
            "claude-opus-4-1-20250805",
            "sonnet[1m]",
            "a.b:c",
        ] {
            assert!(check_model(ok).is_ok(), "{ok}");
        }
        for bad in [
            "-p",
            "--tools",
            "opus sonnet",
            "op\"us",
            "op\nus",
            "opus;x",
            "\u{e9}",
        ] {
            assert!(check_model(bad).is_err(), "{bad:?}");
        }
        assert!(check_model(&"a".repeat(MAX_MODEL_NAME)).is_ok());
        assert!(check_model(&"a".repeat(MAX_MODEL_NAME + 1)).is_err());
    }

    #[test]
    fn claude_code_run_args_hold_only_fixed_text_and_the_model() {
        let args = run_args(Some("opus"));
        let s: Vec<String> = args
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        let pos = |flag: &str| s.iter().position(|a| a == flag).unwrap();
        assert_eq!(s[0], "--print");
        assert_eq!(s[pos("--output-format") + 1], "json");
        assert_eq!(s[pos("--tools") + 1], "", "no tools at all");
        assert_eq!(s[pos("--setting-sources") + 1], "", "no settings files");
        assert_eq!(s[pos("--system-prompt") + 1], SYSTEM_PROMPT);
        assert_eq!(s[pos("--model") + 1], "opus");
        for flag in [
            "--strict-mcp-config",
            "--disable-slash-commands",
            "--no-session-persistence",
            "--safe-mode",
        ] {
            assert!(s.contains(&flag.to_owned()), "{flag}");
        }
        assert!(!s.iter().any(|a| a == "--bare" || a == "--mcp-config"));
        assert!(!run_args(None).iter().any(|a| a == "--model"));
        assert!(
            !s.iter().any(|a| a == "--effort"),
            "no --effort without one asked"
        );
    }

    #[test]
    fn claude_code_run_args_pass_the_effort_as_a_fixed_word() {
        for (effort, word) in [
            (Effort::Low, "low"),
            (Effort::Medium, "medium"),
            (Effort::High, "high"),
            (Effort::ExtraHigh, "xhigh"),
            (Effort::Max, "max"),
        ] {
            let s: Vec<String> = run_args_with(Some("opus"), Some(effort))
                .iter()
                .map(|a| a.to_string_lossy().into_owned())
                .collect();
            let at = s.iter().position(|a| a == "--effort").unwrap();
            assert_eq!(s[at + 1], word);
            assert_eq!(s.iter().filter(|a| *a == "--effort").count(), 1);
            // Everything else is the run without an effort, in order.
            let mut without = s.clone();
            without.drain(at..at + 2);
            let plain: Vec<String> = run_args(Some("opus"))
                .iter()
                .map(|a| a.to_string_lossy().into_owned())
                .collect();
            assert_eq!(without, plain);
            assert_eq!(Effort::parse(word), Some(effort));
            assert_eq!(effort.as_str(), word);
        }
        assert_eq!(Effort::ALL.len(), 5);
        for bad in ["", "Low", "extra-high", "xhigh ", "--model", "ultra"] {
            assert_eq!(Effort::parse(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn claude_code_help_lists_effort_only_as_an_option() {
        // `claude --help` of Claude Code 2.1.292 (this host, 2026-11-10), the lines around it.
        let new = "Options:\n  --disable-slash-commands              Disable all skills\n  \
                   --effort <level>                      Effort level for the current session\n                                        \
                   (low, medium, high, xhigh, max)\n  --model <model>                       Model for the current session.\n";
        assert!(help_lists_effort(new));
        let old = "Options:\n  --model <model>   Model for the current session (the effort of the \
                   default model)\n  --print   Print response and exit\n";
        assert!(!help_lists_effort(old));
        assert!(!help_lists_effort("  --effort-level <x>  something else\n"));
        assert!(!help_lists_effort("  Use --effort to change it\n"));
        assert!(help_lists_effort("--effort\n"));
        assert!(!help_lists_effort(""));
    }

    #[test]
    fn claude_code_sign_in_status_reads_logged_in() {
        assert_eq!(
            sign_in_of(br#"{"loggedIn": true, "authMethod": "claude.ai", "email": "x"}"#),
            SignIn::SignedIn {
                method: Some("claude.ai".into())
            }
        );
        assert_eq!(sign_in_of(br#"{"loggedIn": false}"#), SignIn::NotSignedIn);
        assert!(matches!(sign_in_of(b"{}"), SignIn::Unknown(_)));
        assert!(matches!(sign_in_of(b"Logged in"), SignIn::Unknown(_)));
    }

    #[test]
    #[cfg_attr(not(windows), ignore = "Windows paths")]
    fn claude_code_search_order_skips_relative_path_entries() {
        let search = CliSearch {
            path: Some(std::env::join_paths(["relative", "C:\\abs"]).unwrap()),
            home: Some(PathBuf::from("C:\\home")),
            app_data: Some(PathBuf::from("C:\\appdata")),
            local_app_data: Some(PathBuf::from("C:\\local")),
        };
        // The known install folders first, then PATH (T9.25).
        assert_eq!(
            search.dirs(),
            [
                PathBuf::from("C:\\home\\.local\\bin"),
                PathBuf::from("C:\\appdata\\npm"),
                PathBuf::from("C:\\local\\npm"),
                PathBuf::from("C:\\abs"),
            ]
        );
    }
}
