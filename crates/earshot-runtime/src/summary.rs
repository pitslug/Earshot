//! Meeting summaries (PLAN.md 3.7, T6.4c): from a local model in process (llama.cpp,
//! D19, T6.21a; [`SummaryService::local`]), or through the configured endpoint, an
//! OpenAI-compatible server or the Anthropic Messages API when the user opts in to Claude
//! with their own key (D19, T6.22; [`SummaryService::new`], [`SummaryConfig::anthropic`]), or
//! through the user's own Claude Code CLI (T7.4ga; [`SummaryService::claude_code`]) or Codex
//! CLI (T7.4gc; [`SummaryService::codex`]).
//!
//! # Privacy
//!
//! - A local summary makes no connection: the service has no [`NetClient`] and its
//!   [`AuditLog`] stays empty. The model is a file on disk, run by `earshot-llm-local`
//!   (llama.cpp built without its HTTP client).
//! - For a remote summary all HTTP goes through `earshot-net`. A [`SummaryService`] builds
//!   its own [`NetClient`] whose allowlist holds exactly one entry, the configured
//!   endpoint's host and port (PLAN.md 3.9), and records every attempt in its [`AuditLog`]
//!   ([`SummaryService::audit_log`]). Plain `http` is accepted only for loopback and LAN
//!   hosts (`earshot-net` URL policy). The API key travels only in its request header
//!   (redacted by `earshot-net`, never formatted).
//! - Claude Code (PLAN.md 3.9 exception): no `NetClient`; the prompt text is handed to the
//!   user's CLI on standard input and each run is recorded in the service's
//!   [`HandoverLog`] ([`SummaryService::handovers`]: when, how many characters, never the
//!   text); the CLI's own request to Anthropic is not visible to Earshot
//!   ([`crate::claude_code`]). Codex (T7.4gc) is the same, its request going to OpenAI
//!   ([`crate::codex`]).
//! - What is sent (or given to the local model) is a [`SummaryInput`]: meeting title and
//!   start, attendee names, the transcript text with speaker names, and the user's notes.
//!   The type holds no audio, and nothing in this module reads recordings.
//! - A summary is made only by an explicit [`SummaryService::summarize`] call; the live
//!   session and the post-meeting job never call it. With live summaries on
//!   ([`crate::live_summaries`]) a live session maps windows with the local model only
//!   ([`SummaryService::live_mapper`]); a remote service has no live mapper.
//!
//! # Local summaries
//!
//! Each [`SummaryService::summarize`] call loads the model on its own thread (llama.cpp's
//! model is `!Send`), summarises and frees it before returning, so the model's memory is
//! held only during the call. The transcript is cut into [`time_chunks`] of
//! [`LocalSummaryConfig::chunk_seconds`] (T6.20: 600 s windows, one map request each, then
//! the usual reduce; one window is the ordinary single request). The backend reported in
//! [`LocalRunReport::backend`] is the one llama.cpp's load log shows, never the request;
//! partials made during the meeting ([`crate::live_summaries`]) are re-cited and replace
//! their windows' map requests ([`LocalRunReport::live`]);
//! a GPU request that ends on the CPU fails the load. Cancelling stops the run between
//! requests and between generated tokens. A model load cannot be interrupted (llama.cpp
//! has no cancel hook for it; seconds for a 9B GGUF, twice with a CPU retry): a cancel
//! during the load takes effect when the load returns, before the first request.
//!
//! In the app a run on the graphics card is in a child process instead
//! ([`crate::summary_child`], T9.3): a stall or a crash costs only that summary, which runs on the
//! CPU here and says so, and cancelling kills the child at once.
//!
//! # What is stored
//!
//! On success a `Summary` row (template, model name, the validated [`SummaryV1`]); older
//! summaries of the meeting become tombstones in the same transaction. The model call runs
//! without the store lock, so the transaction first re-reads the meeting: if it was
//! deleted meanwhile, or its transcript's segment ids changed (live draft replaced by the
//! final pass, a re-transcription), nothing is stored and the call returns
//! [`RuntimeError::Conflict`]; the caller summarises again (the citations would point at
//! tombstoned segments otherwise). A meeting without a title takes the summary's title; a
//! title the user gave is never replaced (the meeting row is re-read under the lock, only
//! `title` changes). On failure nothing is written. Local and remote summaries share this
//! step.
//!
//! The transcript is the final pass when there is one, else the live draft
//! ([`MeetingView`]).

use std::fmt;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use earshot_core::asr::CancelToken;
use earshot_core::identity::EntityId;
use earshot_core::model::{MeetingRepo, Stamp, Summary};
use earshot_core::prompt::{
    ContextBudget, MeetingInfo, PromptSegment, SummaryInput, SummaryStyle, TimeChunkError,
    UserNote, Vocabulary, time_chunks,
};
use earshot_core::summary::SummaryV1;
use earshot_llm::{
    ApiKey, ClaudeCli, ClaudeCodeClient, ClaudeCodeConfig, ClaudeCodeError, CodexCli, CodexClient,
    CodexConfig, CodexError, HandoverLog, LlmClient, LlmConfig, LlmError, LlmSummarizer, Provider,
    SummaryReport,
};

use crate::claude_code::{HandoverCli, HandoverEntry};
use earshot_llm_local::grammar::SummaryLimits;
use earshot_llm_local::summarize::{
    Generate, SummaryFailure, SummaryOptions, map_last_chunk, summarize_chunks_with,
};
use earshot_llm_local::{Heartbeat, LlamaBackend, LoadOptions, LocalLlm};
use earshot_net::{Allowlist, AuditLog, NetClient, NetConfig};
use rand_core::Rng;

use crate::live_summaries::{LiveReuse, MapChunk, StoredLiveSummaries};
use crate::summary_child::{ChildEnd, ChildJob, ChildSummary, ChildWork, GpuChild, Watch};
use crate::view::{MeetingView, format_local};

#[cfg(test)]
#[path = "summary_tests.rs"]
mod tests;
use crate::{RuntimeError, Services, SharedClock};

/// Where and how summaries are requested.
#[derive(Debug, Clone)]
pub struct SummaryConfig {
    /// Which API the endpoint speaks.
    pub provider: Provider,
    /// The base URL; the only host the service may contact. OpenAI-compatible: for
    /// example `http://127.0.0.1:8080/v1`. Anthropic: the API root, for example
    /// `https://api.anthropic.com`.
    pub endpoint: String,
    /// Model name sent with each request and stored with the summary.
    pub model: String,
    /// API key, if the endpoint needs one (bearer or `x-api-key` header; never logged).
    pub api_key: Option<ApiKey>,
    /// Context budget of the model (default: `earshot-llm`'s for the provider).
    pub budget: Option<ContextBudget>,
    /// Per-request timeout (default: `earshot-llm`'s).
    pub request_timeout: Option<Duration>,
}

impl SummaryConfig {
    /// An OpenAI-compatible `endpoint` and `model` with the defaults.
    #[must_use]
    pub fn new(endpoint: impl Into<String>, model: impl Into<String>) -> Self {
        Self::for_provider(Provider::OpenAi, endpoint, model)
    }

    /// The Anthropic Messages API at `endpoint` (for example
    /// [`earshot_llm::ANTHROPIC_DEFAULT_URL`]) with `model` and the user's `api_key`.
    #[must_use]
    pub fn anthropic(
        endpoint: impl Into<String>,
        model: impl Into<String>,
        api_key: Option<ApiKey>,
    ) -> Self {
        Self {
            api_key,
            ..Self::for_provider(Provider::Anthropic, endpoint, model)
        }
    }

    fn for_provider(
        provider: Provider,
        endpoint: impl Into<String>,
        model: impl Into<String>,
    ) -> Self {
        Self {
            provider,
            endpoint: endpoint.into(),
            model: model.into(),
            api_key: None,
            budget: None,
            request_timeout: None,
        }
    }
}

/// Context of a local summary model in tokens (T6.20 default "S16": 16384 tokens).
pub const LOCAL_CONTEXT_TOKENS: u32 = 16_384;
/// Largest reply budget of a local summary request (T6.20 default: 8192 tokens).
pub const LOCAL_MAX_OUTPUT_TOKENS: usize = 8192;
/// Meeting time per map request of a local summary (T6.20 default: 600 s).
pub const LOCAL_CHUNK_SECONDS: u32 = 600;

/// No progress from the GPU for this long moves a summary to the CPU (T8.4ab, a judgement: an
/// hour of meeting takes about 94 s in all on the RTX 5080, and every prompt batch and reply
/// token counts as progress).
pub const GPU_STALL_LIMIT: Duration = Duration::from_secs(180);

/// The limit for a GPU load, a judgement: it reports nothing until it ends and may first read
/// 5 GB from a slow disk.
pub const GPU_LOAD_LIMIT: Duration = Duration::from_secs(600);

/// How long a cancelled run may stay silent before it is left behind, so a stop does not wait
/// for the whole stall limit.
const STALL_CANCEL_GRACE: Duration = Duration::from_secs(30);

/// How often a watched run is looked at.
const STALL_POLL: Duration = Duration::from_millis(100);

/// A GPU stalled in this app session: a stuck call cannot be interrupted and holds the GPU, so
/// every later summary of the session runs on the CPU and says so (T8.4ab).
static GPU_STALLED: AtomicBool = AtomicBool::new(false);

/// The text of [`LocalSummaryError::GpuChildStalled`].
const CHILD_STALLED: &str = "the GPU's summary process stopped responding and was stopped";

/// The folder of llama.cpp's DLLs this process runs from: [`earshot_llm_local::loaded_folder`], a
/// test's own in tests.
type LoadedFolder = &'static (dyn Fn() -> Option<PathBuf> + Send + Sync);

/// A local summary model ([`SummaryService::local`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalSummaryConfig {
    /// The GGUF file.
    pub model_path: PathBuf,
    /// The model's name, stored with each summary (for example the manifest id).
    pub model_id: String,
    /// Offload every layer to this build's GPU backend; `false` keeps the model on the
    /// CPU. Either way the load fails when llama.cpp does something else.
    pub gpu: bool,
    /// CPU threads.
    pub threads: i32,
    /// Context size in tokens.
    pub context_tokens: u32,
    /// The largest reply budget of any request.
    pub max_output_tokens: usize,
    /// Text where the assistant's reply starts (for example Qwen 3.5's empty think block
    /// `"<think>\n\n</think>\n\n"`); empty for most models.
    pub assistant_prefix: String,
    /// Meeting time per map request ([`time_chunks`]); above 0.
    pub chunk_seconds: u32,
    /// With `gpu`: when llama.cpp refuses the GPU load, load once more on the CPU and
    /// report the refusal ([`LocalRunReport::gpu_refused`]). Off: the refusal is the error.
    pub cpu_fallback: bool,
    /// GPUs a load must not end up on (the machine's integrated ones, D23): a load naming one
    /// counts as refused.
    pub refuse_gpus: Vec<String>,
    /// With `gpu` and `cpu_fallback`: a run with no progress for this long is left behind and
    /// the summary runs on the CPU ([`GPU_STALL_LIMIT`]).
    pub stall_limit: Duration,
    /// The same for the load itself, which reports no progress ([`GPU_LOAD_LIMIT`]).
    pub load_stall_limit: Duration,
    /// With `gpu`: the program that runs the model, so a stall or a crash costs only this
    /// summary ([`crate::summary_child`]). `None`: in this process.
    pub gpu_child: Option<GpuChild>,
    /// The models folder, for the check of the CUDA files before a child uses them
    /// ([crate::cuda::child_folder_problem]).
    pub models: Option<crate::backends::ModelDir>,
}

impl LocalSummaryConfig {
    /// `model_path` on the CPU with the T6.20 defaults ([`LOCAL_CONTEXT_TOKENS`],
    /// [`LOCAL_MAX_OUTPUT_TOKENS`], [`LOCAL_CHUNK_SECONDS`]), `earshot-llm-local`'s
    /// default thread count and no assistant prefix.
    #[must_use]
    pub fn new(model_path: impl Into<PathBuf>, model_id: impl Into<String>) -> Self {
        Self {
            model_path: model_path.into(),
            model_id: model_id.into(),
            gpu: false,
            threads: earshot_llm_local::default_threads(),
            context_tokens: LOCAL_CONTEXT_TOKENS,
            max_output_tokens: LOCAL_MAX_OUTPUT_TOKENS,
            assistant_prefix: String::new(),
            chunk_seconds: LOCAL_CHUNK_SECONDS,
            cpu_fallback: false,
            refuse_gpus: Vec::new(),
            stall_limit: GPU_STALL_LIMIT,
            load_stall_limit: GPU_LOAD_LIMIT,
            gpu_child: None,
            models: None,
        }
    }
}

/// Why a local summary failed ([`RuntimeError::LocalSummary`]).
#[derive(Debug, Clone, PartialEq)]
pub enum LocalSummaryError {
    /// llama.cpp could not load the model, or loaded it on another backend than asked.
    Load(earshot_llm_local::LlmError),
    /// The GPU load failed and the CPU retry ([`LocalSummaryConfig::cpu_fallback`]) failed
    /// too (for example a corrupt file): both errors, neither dropped.
    LoadRetried {
        /// Why the GPU load failed.
        gpu: Box<LocalSummaryError>,
        /// Why the CPU retry failed.
        cpu: Box<LocalSummaryError>,
    },
    /// The GPU stopped making progress, in this summary or an earlier one of the session: the
    /// stuck call is left behind and the summary runs on the CPU (T8.4ab).
    GpuStalled,
    /// The GPU's child process stopped making progress and was killed, which freed the card:
    /// this summary runs on the CPU and the next one tries the card again (T9.3).
    GpuChildStalled,
    /// The GPU's child process ended without finishing (a driver crash, llama.cpp's abort): this
    /// summary runs on the CPU and the next one tries the card again (T9.3).
    GpuChildDied {
        /// Fixed words and at most an exit code.
        why: String,
    },
    /// llama.cpp's CUDA folder no longer looks as it did at the start, so no child may load it:
    /// this summary runs on the CPU and a restart checks the folder in full (T9.3).
    CudaFilesChanged {
        /// Fixed words.
        why: String,
    },
    /// The run in the GPU's child process failed: its [`SummaryFailure`]'s text.
    ChildRun(String),
    /// The transcript could not be cut into time chunks (a zero window).
    Chunks(TimeChunkError),
    /// The run failed: no plan fits, a model error, still invalid after its repairs, or
    /// [`SummaryFailure::Cancelled`].
    Run(SummaryFailure),
}

impl LocalSummaryError {
    /// Whether the run was cancelled.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        matches!(self, Self::Run(SummaryFailure::Cancelled))
    }
}

impl fmt::Display for LocalSummaryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Load(e) => write!(f, "{e}"),
            Self::LoadRetried { gpu, cpu } => {
                write!(
                    f,
                    "the GPU load failed ({gpu}), then the CPU retry failed ({cpu})"
                )
            }
            Self::GpuStalled => f.write_str("the GPU stopped making progress"),
            Self::GpuChildStalled => f.write_str(CHILD_STALLED),
            Self::GpuChildDied { why } => write!(f, "the GPU's summary process died: {why}"),
            Self::CudaFilesChanged { why } => {
                write!(f, "the CUDA files changed since the start: {why}")
            }
            Self::ChildRun(message) => f.write_str(message),
            Self::Chunks(e) => write!(f, "{e}"),
            Self::Run(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for LocalSummaryError {}

/// What a local summary did.
#[derive(Debug, Clone, PartialEq)]
pub struct LocalRunReport {
    /// Where llama.cpp put the model, from its load log.
    pub backend: LlamaBackend,
    /// Why the GPU load was refused, when the model then ran on the CPU
    /// ([`LocalSummaryConfig::cpu_fallback`]).
    pub gpu_refused: Option<Box<LocalSummaryError>>,
    /// Model load time.
    pub load_time: Duration,
    /// Time chunks of the transcript (one: the ordinary single request).
    pub chunks: usize,
    /// Requests, repairs and the time of every step.
    pub run: earshot_llm_local::summarize::SummaryReport,
    /// What happened to the partials made during the meeting (all zero without a record).
    pub live: Box<LiveReuse>,
}

/// What a summary run did, by backend.
#[derive(Debug, Clone, PartialEq)]
pub enum SummaryRunReport {
    /// A remote endpoint: requests, repairs and fallbacks.
    Remote(SummaryReport),
    /// Claude Code: runs (`requests`) and repairs.
    ClaudeCode(SummaryReport),
    /// Codex (T7.4gc): runs (`requests`) and repairs.
    Codex(SummaryReport),
    /// The local model.
    Local(LocalRunReport),
}

/// Result of a summary.
#[derive(Debug, Clone, PartialEq)]
pub struct SummaryOutcome {
    /// The stored `Summary` row.
    pub summary_id: EntityId,
    /// The validated summary.
    pub summary: SummaryV1,
    /// What the run did.
    pub report: SummaryRunReport,
    /// The meeting had no title and now carries the summary's.
    pub title_set: bool,
}

/// A loaded local model and where it runs.
pub(crate) struct LoadedModel {
    /// Counts the model's progress.
    pub(crate) heartbeat: Heartbeat,
    pub(crate) model: Box<dyn Generate>,
    pub(crate) backend: LlamaBackend,
    pub(crate) load_time: Duration,
}

/// Loads the local model on the summary thread (llama.cpp in production, a fake in tests).
pub(crate) trait LoadLocal: Send + Sync {
    fn load(&self, config: &LocalSummaryConfig) -> Result<LoadedModel, RuntimeError>;
}

/// llama.cpp in production. `LocalLlm::load` serialises loads itself (each reads its own
/// window of the process-wide load log).
pub(crate) struct LlamaLoader;

impl LoadLocal for LlamaLoader {
    fn load(&self, config: &LocalSummaryConfig) -> Result<LoadedModel, RuntimeError> {
        crate::cpu::require_native_builds()?;
        if !config.model_path.is_file() {
            return Err(RuntimeError::ModelMissing {
                path: config.model_path.clone(),
                fetch_set: None,
            });
        }
        let (llm, report) = LocalLlm::load(
            &config.model_path,
            &LoadOptions {
                gpu: config.gpu,
                context_tokens: config.context_tokens,
                threads: config.threads,
            },
        )
        .map_err(|e| RuntimeError::LocalSummary(LocalSummaryError::Load(e)))?;
        if let Some(device) = refused_gpu(
            report.backend,
            report.device.as_deref(),
            &config.refuse_gpus,
        ) {
            return Err(RuntimeError::LocalSummary(LocalSummaryError::Load(
                earshot_llm_local::LlmError::Backend(format!(
                    "llama.cpp loaded the model on {device}, which summaries do not use"
                )),
            )));
        }
        Ok(LoadedModel {
            heartbeat: llm.heartbeat(),
            model: Box::new(llm),
            backend: report.backend,
            load_time: report.load_time,
        })
    }
}

/// The GPU of a load that must not have used it: `device` when the load is on a GPU and
/// `refuse` holds it.
fn refused_gpu<'a>(
    backend: LlamaBackend,
    device: Option<&'a str>,
    refuse: &[String],
) -> Option<&'a str> {
    device.filter(|d| backend != LlamaBackend::Cpu && refuse.iter().any(|r| r == d))
}

enum Backend {
    Remote {
        runtime: Box<tokio::runtime::Runtime>,
        summarizer: LlmSummarizer,
        model: String,
        /// Through an agent CLI, Claude Code or Codex (the report says so).
        cli: Option<HandoverCli>,
    },
    Local {
        config: LocalSummaryConfig,
        loader: Arc<dyn LoadLocal>,
        /// [`GPU_STALLED`]; a test's own flag in tests.
        stalled: &'static AtomicBool,
        folder: LoadedFolder,
    },
}

/// The client settings of a remote `config` and the allowlist its `earshot-net` client gets:
/// exactly the endpoint's host and port. Refuses now what earshot-net would refuse on every
/// request (plain http to a public host), so a bad setting fails when it is saved, not
/// mid-meeting. The one rule of [`SummaryService::new`] and of the Summaries settings
/// (`crate::summary_settings`).
///
/// # Errors
/// As [`SummaryService::new`]: [`RuntimeError::Llm`], [`RuntimeError::Endpoint`],
/// [`RuntimeError::EndpointDenied`].
fn endpoint_config(config: &SummaryConfig) -> Result<(LlmConfig, Allowlist), RuntimeError> {
    let mut llm = LlmConfig::for_provider(config.provider, &config.endpoint, &config.model)
        .map_err(RuntimeError::Llm)?;
    llm.api_key = config.api_key.clone();
    if let Some(budget) = config.budget {
        llm.budget = budget;
    }
    if let Some(timeout) = config.request_timeout {
        llm.request_timeout = timeout;
    }
    let mut allow = Allowlist::new();
    allow
        .allow_url(&llm.endpoint)
        .map_err(RuntimeError::Endpoint)?;
    allow
        .check(&llm.request_url())
        .map_err(RuntimeError::EndpointDenied)?;
    Ok((llm, allow))
}

/// The allowlist a [`SummaryService`] for `config` would get (exactly one entry, the
/// endpoint), without building the client.
///
/// # Errors
/// As [`SummaryService::new`] for the endpoint.
pub fn summary_allowlist(config: &SummaryConfig) -> Result<Allowlist, RuntimeError> {
    endpoint_config(config).map(|(_, allow)| allow)
}

/// The summary client. Remote: one `earshot-net` client allowlisting only the configured
/// endpoint, the provider's client over it, and a current-thread tokio runtime to drive
/// it. Local: the model's settings (the model is loaded per call). Create one per
/// configuration and reuse it.
pub struct SummaryService {
    backend: Backend,
    audit: AuditLog,
    handovers: Handovers,
    record_failures: Arc<std::sync::Mutex<Vec<String>>>,
}

/// The handover log of an agent-CLI service.
enum Handovers {
    None,
    ClaudeCode(HandoverLog),
    Codex(HandoverLog<CodexError>),
}

/// Clears an agent-CLI client's handover sink when dropped (the end of a summary run).
struct ClearSink<'a>(&'a LlmClient);

impl Drop for ClearSink<'_> {
    fn drop(&mut self) {
        match self.0 {
            LlmClient::ClaudeCode(c) => c.set_sink(None),
            LlmClient::Codex(c) => c.set_sink(None),
            LlmClient::OpenAi(_) | LlmClient::Anthropic(_) => {}
        }
    }
}

impl fmt::Debug for SummaryService {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (kind, model) = match &self.backend {
            Backend::Remote { model, .. } => ("remote", model),
            Backend::Local { config, .. } => ("local", &config.model_id),
        };
        f.debug_struct("SummaryService")
            .field("backend", &kind)
            .field("model", model)
            .finish_non_exhaustive()
    }
}

impl SummaryService {
    /// A service for the remote endpoint of `config`, timestamping its audit log with
    /// `clock`.
    ///
    /// # Errors
    /// [`RuntimeError::Llm`] for an invalid endpoint or model;
    /// [`RuntimeError::Endpoint`] if the endpoint cannot be allowlisted;
    /// [`RuntimeError::EndpointDenied`] if `earshot-net` would refuse it (plain `http` to
    /// a host that is not loopback or LAN); [`RuntimeError::Net`] if the client cannot be
    /// built; [`RuntimeError::Spawn`] if the tokio runtime cannot
    /// start.
    pub fn new(config: &SummaryConfig, clock: SharedClock) -> Result<Self, RuntimeError> {
        let (llm, allow) = endpoint_config(config)?;
        let audit = AuditLog::new();
        let net = NetClient::new(NetConfig::new(allow, clock).audit(audit.clone()))
            .map_err(RuntimeError::Net)?;
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(RuntimeError::Spawn)?;
        Ok(Self {
            backend: Backend::Remote {
                runtime: Box::new(runtime),
                summarizer: LlmSummarizer::new(LlmClient::new(net, llm)),
                model: config.model.clone(),
                cli: None,
            },
            audit,
            handovers: Handovers::None,
            record_failures: Arc::default(),
        })
    }

    /// A service summarising through the user's Claude Code CLI ([`crate::claude_code`]):
    /// the CLI of `config` (found now when it names none), its handovers timestamped with
    /// `clock`. No `NetClient`: the audit log stays empty.
    ///
    /// # Errors
    /// [`RuntimeError::Llm`] with [`ClaudeCodeError::NotInstalled`] when no CLI is found, or
    /// a refused model name; [`RuntimeError::Spawn`] if the tokio runtime cannot start.
    pub fn claude_code(
        config: &crate::claude_code::ClaudeCodeSummaryConfig,
        clock: SharedClock,
    ) -> Result<Self, RuntimeError> {
        let cli = match &config.cli {
            Some(cli) => cli.clone(),
            None => ClaudeCli::find().ok_or(RuntimeError::Llm(LlmError::ClaudeCode(
                ClaudeCodeError::NotInstalled,
            )))?,
        };
        let mut cc = ClaudeCodeConfig::new(cli, config.model.clone()).map_err(RuntimeError::Llm)?;
        cc.effort = config.effort;
        if let Some(timeout) = config.request_timeout {
            cc.request_timeout = timeout;
        }
        let client = ClaudeCodeClient::new(cc, clock);
        let handovers = client.handovers();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(RuntimeError::Spawn)?;
        Ok(Self {
            backend: Backend::Remote {
                runtime: Box::new(runtime),
                summarizer: LlmSummarizer::new(client),
                model: crate::claude_code::stored_model_name(config.model.as_deref()),
                cli: Some(HandoverCli::ClaudeCode),
            },
            audit: AuditLog::new(),
            handovers: Handovers::ClaudeCode(handovers),
            record_failures: Arc::default(),
        })
    }

    /// A service summarising through the user's Codex CLI ([`crate::codex`]): the CLI of
    /// `config` (found now when it names none), its handovers timestamped with `clock`. No
    /// `NetClient`: the audit log stays empty.
    ///
    /// # Errors
    /// [`RuntimeError::Llm`] with [`CodexError::NotInstalled`] when no CLI is found, or a
    /// refused model name; [`RuntimeError::Spawn`] if the tokio runtime cannot start.
    pub fn codex(
        config: &crate::codex::CodexSummaryConfig,
        clock: SharedClock,
    ) -> Result<Self, RuntimeError> {
        let cli = match &config.cli {
            Some(cli) => cli.clone(),
            None => CodexCli::find()
                .ok_or(RuntimeError::Llm(LlmError::Codex(CodexError::NotInstalled)))?,
        };
        let mut cc = CodexConfig::new(cli, config.model.clone()).map_err(RuntimeError::Llm)?;
        if let Some(timeout) = config.request_timeout {
            cc.request_timeout = timeout;
        }
        let client = CodexClient::new(cc, clock);
        let handovers = client.handovers();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(RuntimeError::Spawn)?;
        Ok(Self {
            backend: Backend::Remote {
                runtime: Box::new(runtime),
                summarizer: LlmSummarizer::new(client),
                model: crate::codex::stored_model_name(config.model.as_deref()),
                cli: Some(HandoverCli::Codex),
            },
            audit: AuditLog::new(),
            handovers: Handovers::Codex(handovers),
            record_failures: Arc::default(),
        })
    }

    /// A service summarising with the local model of `config` (module docs). Nothing is
    /// loaded until [`SummaryService::summarize`]; a missing model file is reported there.
    ///
    /// # Errors
    /// [`RuntimeError::LocalSummary`] with [`LocalSummaryError::Chunks`] for a zero
    /// [`LocalSummaryConfig::chunk_seconds`].
    pub fn local(config: &LocalSummaryConfig) -> Result<Self, RuntimeError> {
        Self::local_with(config, Box::new(LlamaLoader))
    }

    fn local_with(
        config: &LocalSummaryConfig,
        loader: Box<dyn LoadLocal>,
    ) -> Result<Self, RuntimeError> {
        Self::local_flagged(config, loader, &GPU_STALLED)
    }

    pub(crate) fn local_flagged(
        config: &LocalSummaryConfig,
        loader: Box<dyn LoadLocal>,
        stalled: &'static AtomicBool,
    ) -> Result<Self, RuntimeError> {
        Self::local_on_folder(config, loader, stalled, &earshot_llm_local::loaded_folder)
    }

    pub(crate) fn local_on_folder(
        config: &LocalSummaryConfig,
        loader: Box<dyn LoadLocal>,
        stalled: &'static AtomicBool,
        folder: LoadedFolder,
    ) -> Result<Self, RuntimeError> {
        if config.chunk_seconds == 0 {
            return Err(RuntimeError::LocalSummary(LocalSummaryError::Chunks(
                TimeChunkError::ZeroWindow,
            )));
        }
        Ok(Self {
            backend: Backend::Local {
                config: config.clone(),
                loader: Arc::from(loader),
                stalled,
                folder,
            },
            audit: AuditLog::new(),
            handovers: Handovers::None,
            record_failures: Arc::default(),
        })
    }

    /// The model name stored with each summary (the endpoint's model, or the local
    /// model's id).
    #[must_use]
    pub fn model(&self) -> &str {
        match &self.backend {
            Backend::Remote { model, .. } => model,
            Backend::Local { config, .. } => &config.model_id,
        }
    }

    /// The map worker of a live session ([`crate::live_summaries`]) for this local model,
    /// with `threads` CPU threads (the meeting's share,
    /// [`crate::live_summaries::live_summary_threads`]); `None` for a remote service (a
    /// live session never makes a connection).
    #[must_use]
    pub fn live_mapper(&self, threads: i32) -> Option<Arc<dyn MapChunk>> {
        match &self.backend {
            Backend::Remote { .. } => None,
            Backend::Local {
                config,
                loader,
                stalled,
                folder,
            } => Some(Arc::new(LocalMapper {
                stalled,
                folder: *folder,
                config: LocalSummaryConfig {
                    threads,
                    ..config.clone()
                },
                loader: Arc::clone(loader),
            })),
        }
    }

    /// Every request attempt made so far (the privacy record); always empty for a local
    /// service.
    #[must_use]
    pub fn audit_log(&self) -> AuditLog {
        self.audit.clone()
    }

    /// Every time text was handed to Claude Code or Codex so far, in the stored form (when,
    /// how many characters, which CLI, how the run ended; never the text); always empty for
    /// other services.
    #[must_use]
    pub fn handovers(&self) -> Vec<HandoverEntry> {
        match &self.handovers {
            Handovers::None => Vec::new(),
            Handovers::ClaudeCode(log) => log.entries().iter().map(HandoverEntry::of).collect(),
            Handovers::Codex(log) => log.entries().iter().map(HandoverEntry::of).collect(),
        }
    }

    /// Why storing a handover of this service failed, one store error text per failure (never
    /// meeting text); empty when every handover was stored.
    #[must_use]
    pub fn handover_record_failures(&self) -> Vec<String> {
        self.record_failures
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// The agent-CLI client of this service (Claude Code or Codex), if it runs one.
    fn agent_cli_client(&self) -> Option<&LlmClient> {
        match &self.backend {
            Backend::Remote { summarizer, .. } => match summarizer.client() {
                c @ (LlmClient::ClaudeCode(_) | LlmClient::Codex(_)) => Some(c),
                _ => None,
            },
            Backend::Local { .. } => None,
        }
    }

    /// The agent CLI this service hands text to (Claude Code or Codex), if any.
    #[must_use]
    pub fn agent_cli(&self) -> Option<HandoverCli> {
        match &self.backend {
            Backend::Remote { cli, .. } => *cli,
            Backend::Local { .. } => None,
        }
    }

    /// Whether this service runs Claude Code.
    #[must_use]
    pub fn is_claude_code(&self) -> bool {
        self.agent_cli() == Some(HandoverCli::ClaudeCode)
    }

    /// Whether this service's Claude Code ran at its own default effort although an effort was
    /// chosen, because its `--help` could not be read (T12.20; an older CLI that does not list
    /// `--effort` is said on the Summaries page instead). False before its first run.
    #[must_use]
    pub fn claude_effort_unread(&self) -> bool {
        matches!(
            self.agent_cli_client(),
            Some(LlmClient::ClaudeCode(c))
                if c.effort_probe() == Some(earshot_llm::claude_code::EffortProbe::Unreadable)
        )
    }

    /// Whether this service runs Codex.
    #[must_use]
    pub fn is_codex(&self) -> bool {
        self.agent_cli() == Some(HandoverCli::Codex)
    }

    /// The prompt input for `meeting` (what would be sent).
    ///
    /// # Errors
    /// [`RuntimeError::Internal`] if the meeting does not exist; store errors.
    pub fn input(services: &Services, meeting: EntityId) -> Result<SummaryInput, RuntimeError> {
        let (view, settings) = load_input(services, meeting)?;
        Ok(input_of(&view, &settings))
    }

    /// Summarises `meeting` and stores the result (module docs). Blocks until the
    /// endpoint answered or the local model finished; cancel through `cancel`.
    ///
    /// # Errors
    /// Remote: [`RuntimeError::Llm`] (endpoint, validation or cancellation). Local:
    /// [`RuntimeError::ModelMissing`], [`RuntimeError::LocalSummary`] (load, plan, model,
    /// still invalid, cancelled), [`RuntimeError::Spawn`]. Both:
    /// [`RuntimeError::Conflict`] (module docs), store errors, [`RuntimeError::Internal`]
    /// if the meeting does not exist. Nothing is written then. A service that is not local:
    /// [`RuntimeError::SummaryKeptLocal`] for a meeting kept on this computer, before anything
    /// is read or sent.
    pub fn summarize<R: Rng + ?Sized>(
        &self,
        services: &Services,
        meeting: EntityId,
        rng: &mut R,
        cancel: &CancelToken,
    ) -> Result<SummaryOutcome, RuntimeError> {
        // A meeting kept on this computer (T12.13) is summarised only by a local model: any
        // other service is refused before the transcript is read (`crate::keep_local`).
        if !matches!(self.backend, Backend::Local { .. })
            && crate::keep_local::summary_privacy(&services.store(), meeting)
                == crate::keep_local::SummaryPrivacy::KeptLocal
        {
            return Err(RuntimeError::SummaryKeptLocal(meeting));
        }
        let (view, settings) = load_input(services, meeting)?;
        let input = input_of(&view, &settings);
        // Claude Code and Codex: every handover of this run is stored for the meeting as it
        // happens (`crate::claude_code::MeetingHandovers`); the sink is cleared when the run ends.
        let _sink = self.agent_cli_client().map(|c| {
            let sink = Arc::new(crate::claude_code::MeetingHandovers {
                services: services.clone(),
                meeting,
                failures: Arc::clone(&self.record_failures),
            });
            match c {
                LlmClient::ClaudeCode(c) => c.set_sink(Some(sink)),
                LlmClient::Codex(c) => c.set_sink(Some(sink)),
                LlmClient::OpenAi(_) | LlmClient::Anthropic(_) => {}
            }
            ClearSink(c)
        });
        let (summary, report, model) = match &self.backend {
            Backend::Remote {
                runtime,
                summarizer,
                model,
                cli,
            } => {
                let run = runtime.block_on(summarizer.summarize_with_report(&input, cancel));
                let summary = run.result.map_err(RuntimeError::Llm)?;
                let report = match cli {
                    Some(HandoverCli::ClaudeCode) => SummaryRunReport::ClaudeCode(run.report),
                    Some(HandoverCli::Codex) => SummaryRunReport::Codex(run.report),
                    None => SummaryRunReport::Remote(run.report),
                };
                (summary, report, model.clone())
            }
            Backend::Local {
                config,
                loader,
                stalled,
                folder,
            } => {
                let live = live_record(services, &view)?;
                let (summary, report) = run_local(
                    config,
                    loader,
                    (stalled, *folder),
                    &input,
                    live.as_ref(),
                    cancel,
                )?;
                (
                    summary,
                    SummaryRunReport::Local(report),
                    config.model_id.clone(),
                )
            }
        };
        let (summary_id, title_set) = store_summary(services, &view, model, &summary, rng)?;
        Ok(SummaryOutcome {
            summary_id,
            summary,
            report,
            title_set,
        })
    }
}

/// The user's settings a summary request carries, as stored when it is written.
struct PromptSettings {
    /// The summary style ([`crate::summary_style`]).
    style: SummaryStyle,
    /// The summary template ([`crate::summary_style::summary_template`], T12.17).
    style_preset: earshot_core::prompt::StylePreset,
    /// The user's own words ([`crate::vocabulary`]).
    vocabulary: Vocabulary,
}

/// The meeting's view with the summary style and the user's own words stored now
/// ([`crate::summary_style`], [`crate::vocabulary`]; a stored value this version refuses is
/// none, reported on the Summaries and Dictionary pages and in the meeting's start notes by
/// `host::summary_from_settings`), read under one store lock.
fn load_input(
    services: &Services,
    meeting: EntityId,
) -> Result<(MeetingView, PromptSettings), RuntimeError> {
    let db = services.store();
    let view =
        MeetingView::load(&**db, meeting)?.ok_or(RuntimeError::Internal("no such meeting"))?;
    let (style, _refused) = crate::summary_style::summary_style(&db)?;
    let (style_preset, _refused) = crate::summary_style::summary_template(&db)?;
    let (vocabulary, _refused) = crate::vocabulary::vocabulary(&db)?;
    Ok((
        view,
        PromptSettings {
            style,
            style_preset,
            vocabulary,
        },
    ))
}

/// The partials a live session recorded for the meeting of `view`, with the live segments
/// they cite and the segments of `view` (the transcript being summarised); `None` without
/// a record.
fn live_record(services: &Services, view: &MeetingView) -> Result<Option<LiveParts>, RuntimeError> {
    let db = services.store();
    let Some(record) = crate::live_summaries::live_summary_record(&db, view.meeting.id)? else {
        return Ok(None);
    };
    let mut rows: Vec<earshot_core::asr::Segment> = earshot_core::model::live_segments(
        &**db,
        view.meeting.id,
        earshot_core::model::TranscriptPass::Live,
    )?
    .into_iter()
    .map(|r| r.segment)
    .collect();
    drop(db);
    rows.sort_by_key(|s| (s.start, s.id));
    Ok(Some(LiveParts {
        record,
        live: crate::live_summaries::timed(view, &rows),
        fin: crate::live_summaries::timed(view, &view.segments),
    }))
}

/// [`live_record`]'s answer.
struct LiveParts {
    record: StoredLiveSummaries,
    live: Vec<earshot_core::summary::TimedSegment>,
    fin: Vec<earshot_core::summary::TimedSegment>,
}

/// The run options of `config`.
pub(crate) fn options_of(config: &LocalSummaryConfig, cancel: &CancelToken) -> SummaryOptions {
    SummaryOptions {
        max_output_tokens: config.max_output_tokens,
        assistant_prefix: config.assistant_prefix.clone(),
        limits: SummaryLimits::default(),
        cancel: cancel.clone(),
    }
}

/// Summarises `input` with the local model on a thread of its own (module docs), reusing
/// the re-cited partials of `live`. The model is dropped on that thread before it returns.
fn run_local(
    config: &LocalSummaryConfig,
    loader: &Arc<dyn LoadLocal>,
    (stalled, folder): (&'static AtomicBool, LoadedFolder),
    input: &SummaryInput,
    live: Option<&LiveParts>,
    cancel: &CancelToken,
) -> Result<(SummaryV1, LocalRunReport), RuntimeError> {
    let local = RuntimeError::LocalSummary;
    let ranges = time_chunks(input, config.chunk_seconds)
        .map_err(|e| local(LocalSummaryError::Chunks(e)))?;
    if cancel.is_cancelled() {
        return Err(local(LocalSummaryError::Run(SummaryFailure::Cancelled)));
    }
    let (done, reuse) = match live {
        Some(parts) => crate::live_summaries::reusable(
            &parts.record,
            &config.model_id,
            config.chunk_seconds,
            &parts.live,
            &parts.fin,
            input,
            &ranges,
        ),
        None => (vec![None; ranges.len()], LiveReuse::default()),
    };
    let job = |config: &LocalSummaryConfig| LocalJob {
        config: config.clone(),
        loader: Arc::clone(loader),
        stalled,
        input: input.clone(),
        ranges: ranges.clone(),
        done: done.clone(),
    };
    // The GPU was given up: the same run on the CPU, carrying why.
    let on_cpu = |why: LocalSummaryError| -> Result<Finished, RuntimeError> {
        if cancel.is_cancelled() {
            return Err(local(LocalSummaryError::Run(SummaryFailure::Cancelled)));
        }
        let cpu = LocalSummaryConfig {
            gpu: false,
            ..config.clone()
        };
        match attempt_local(job(&cpu), cancel)? {
            Attempt::Finished(finished) => {
                let (run, backend, load_time, _) = (*finished)?;
                Ok((run, backend, load_time, Some(Box::new(why))))
            }
            Attempt::Stalled => Err(RuntimeError::Internal("a CPU run is never left behind")),
        }
    };
    let (run, backend, load_time, gpu_refused) = if let Some(child) = usable_child(config, stalled)
    {
        let work = ChildWork::Whole {
            ranges: ranges.clone(),
            done: done.clone(),
        };
        let attempt = ChildTry {
            child,
            config,
            stalled,
            folder,
            background: false,
        };
        match run_in_child(&attempt, work, input, cancel)? {
            InChild::Finished(s) => {
                let ChildSummary {
                    summary,
                    backend,
                    load_time,
                    report,
                } = *s;
                let run = earshot_llm_local::summarize::SummaryRun {
                    result: Ok(summary),
                    report,
                };
                (run, backend, load_time, None)
            }
            InChild::GaveUp(why) if config.cpu_fallback => {
                on_cpu(why.clone()).map_err(|e| match e {
                    RuntimeError::LocalSummary(cpu @ LocalSummaryError::Load(_)) => {
                        local(LocalSummaryError::LoadRetried {
                            gpu: Box::new(why),
                            cpu: Box::new(cpu),
                        })
                    }
                    e => e,
                })?
            }
            InChild::GaveUp(why) => return Err(local(why)),
        }
    } else {
        match attempt_local(job(config), cancel)? {
            Attempt::Finished(finished) => (*finished)?,
            Attempt::Stalled => {
                stalled.store(true, Ordering::Release);
                on_cpu(LocalSummaryError::GpuStalled)?
            }
        }
    };
    let summary = run.result.map_err(|e| local(LocalSummaryError::Run(e)))?;
    Ok((
        summary,
        LocalRunReport {
            backend,
            gpu_refused,
            load_time,
            chunks: ranges.len(),
            run: run.report,
            live: Box::new(reuse),
        },
    ))
}

/// The child that runs `config`'s GPU summary: none for a CPU summary, without a registered
/// program, or after a child that would not die (it may still hold the card).
fn usable_child<'a>(config: &'a LocalSummaryConfig, stalled: &AtomicBool) -> Option<&'a GpuChild> {
    config
        .gpu_child
        .as_ref()
        .filter(|_| config.gpu && !stalled.load(Ordering::Acquire))
}

/// How a child's try ended, for the caller.
enum InChild {
    /// The summary.
    Finished(Box<ChildSummary>),
    /// Not from this child: why, to run on the CPU or report.
    GaveUp(LocalSummaryError),
}

/// What a stalled child means: only one that will not die keeps the card off for the session.
pub(crate) fn after_child_stall(gone: bool, stalled: &AtomicBool) -> LocalSummaryError {
    if gone {
        LocalSummaryError::GpuChildStalled
    } else {
        stalled.store(true, Ordering::Release);
        LocalSummaryError::GpuStalled
    }
}

/// Who runs a try in a child.
struct ChildTry<'a> {
    child: &'a GpuChild,
    config: &'a LocalSummaryConfig,
    stalled: &'a AtomicBool,
    folder: LoadedFolder,
    /// A live window: the whole child below normal priority.
    background: bool,
}

/// One try in a child ([`crate::summary_child`]). A missing model file and a failed run or a
/// cancel are the caller's errors; anything else about the card is [`InChild::GaveUp`].
fn run_in_child(
    attempt: &ChildTry<'_>,
    work: ChildWork,
    input: &SummaryInput,
    cancel: &CancelToken,
) -> Result<InChild, RuntimeError> {
    let config = attempt.config;
    // As `LlamaLoader`: a missing file is never a matter of the GPU.
    if !config.model_path.is_file() {
        return Err(RuntimeError::ModelMissing {
            path: config.model_path.clone(),
            fetch_set: None,
        });
    }
    // The app may still be choosing its DLLs; the child runs on the set chosen (module docs).
    earshot_llm_local::wait_for_llama();
    let llama_folder = (attempt.folder)();
    // The start verified this folder in full and ran a trial; a child loads it again now, so a
    // library dropped in since must not reach it. Sizes and names only: not a hash per summary.
    if let Some(folder) = &llama_folder
        && let Some(why) = crate::cuda::child_folder_problem(folder, config.models.as_ref())
    {
        return Ok(InChild::GaveUp(LocalSummaryError::CudaFilesChanged {
            why: why.to_owned(),
        }));
    }
    let job = ChildJob {
        model_path: config.model_path.clone(),
        model_id: config.model_id.clone(),
        threads: config.threads,
        context_tokens: config.context_tokens,
        max_output_tokens: config.max_output_tokens,
        assistant_prefix: config.assistant_prefix.clone(),
        refuse_gpus: config.refuse_gpus.clone(),
        llama_folder,
        work,
        input: input.clone(),
    };
    let watch = Watch {
        load_limit: config.load_stall_limit,
        run_limit: config.stall_limit,
        background: attempt.background,
    };
    match crate::summary_child::run_job(attempt.child, &job, watch, cancel) {
        ChildEnd::Finished(summary) => Ok(InChild::Finished(summary)),
        ChildEnd::RunFailed(message) => Err(RuntimeError::LocalSummary(
            LocalSummaryError::ChildRun(message),
        )),
        ChildEnd::Cancelled => Err(RuntimeError::LocalSummary(LocalSummaryError::Run(
            SummaryFailure::Cancelled,
        ))),
        ChildEnd::LoadRefused(message) => Ok(InChild::GaveUp(LocalSummaryError::Load(
            earshot_llm_local::LlmError::Backend(message),
        ))),
        ChildEnd::Died(why) => Ok(InChild::GaveUp(LocalSummaryError::GpuChildDied { why })),
        ChildEnd::Stalled { gone } => Ok(InChild::GaveUp(after_child_stall(gone, attempt.stalled))),
    }
}

/// What one try at a local summary needs, owned: a stalled try's thread outlives the call.
struct LocalJob {
    config: LocalSummaryConfig,
    loader: Arc<dyn LoadLocal>,
    stalled: &'static AtomicBool,
    input: SummaryInput,
    ranges: Vec<std::ops::Range<usize>>,
    done: Vec<Option<SummaryV1>>,
}

/// A finished run: the run, where it ran, the load time and any refusal.
type Finished = (
    earshot_llm_local::summarize::SummaryRun,
    LlamaBackend,
    Duration,
    Option<Box<LocalSummaryError>>,
);

/// How a try ended.
enum Attempt {
    /// The thread returned.
    Finished(Box<Result<Finished, RuntimeError>>),
    /// The thread is stuck and was left behind.
    Stalled,
}

/// What the thread tells the watcher.
enum Progress {
    /// The model is loaded on `backend`.
    Loaded(LlamaBackend, Heartbeat),
    /// The run ended.
    Finished(Box<Result<Finished, RuntimeError>>),
}

/// Whether a watched run that has been silent for `silent` has stalled.
fn has_stalled(silent: Duration, limit: Duration, cancelled: bool) -> bool {
    silent >= limit || (cancelled && silent >= STALL_CANCEL_GRACE)
}

/// One try on a thread of its own. A GPU try that can fall back is watched: with no progress
/// for its limit the thread is left behind (`Attempt::Stalled`) with its token cancelled, so
/// it stops and frees the model if the call ever returns, and its answer goes nowhere. Nothing
/// on that thread touches the store. Only the GPU is watched: a slow CPU is not a stall.
fn attempt_local(job: LocalJob, cancel: &CancelToken) -> Result<Attempt, RuntimeError> {
    let (tx, rx) = std::sync::mpsc::channel();
    let token = CancelToken::new();
    let options = options_of(&job.config, &token);
    let mut limit =
        (job.config.gpu && job.config.cpu_fallback).then_some(job.config.load_stall_limit);
    let run_limit = job.config.stall_limit;
    let worker = std::thread::Builder::new()
        .name("earshot-summary".into())
        .spawn(move || {
            let finished = (|| -> Result<Finished, RuntimeError> {
                let (loaded, gpu_refused) =
                    load_with_fallback(&job.config, &*job.loader, job.stalled)?;
                let LoadedModel {
                    heartbeat,
                    mut model,
                    backend,
                    load_time,
                } = loaded;
                let _ = tx.send(Progress::Loaded(backend, heartbeat));
                let run = summarize_chunks_with(
                    model.as_mut(),
                    &job.input,
                    &options,
                    &job.ranges,
                    job.done,
                );
                drop(model);
                Ok((run, backend, load_time, gpu_refused))
            })();
            let _ = tx.send(Progress::Finished(Box::new(finished)));
        })
        .map_err(RuntimeError::Spawn)?;
    let mut heartbeat: Option<Heartbeat> = None;
    let mut last_count = 0;
    let mut since = std::time::Instant::now();
    loop {
        if cancel.is_cancelled() {
            token.cancel();
        }
        match rx.recv_timeout(STALL_POLL) {
            Ok(Progress::Loaded(backend, beat)) => {
                // Refused and retried on the CPU, or a CPU try: nothing to watch from here.
                limit = (limit.is_some() && backend != LlamaBackend::Cpu).then_some(run_limit);
                last_count = beat.count();
                heartbeat = Some(beat);
                since = std::time::Instant::now();
            }
            Ok(Progress::Finished(finished)) => {
                let _ = worker.join();
                return Ok(Attempt::Finished(finished));
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                return match worker.join() {
                    Err(panic) => std::panic::resume_unwind(panic),
                    Ok(()) => Err(RuntimeError::Internal("the summary thread sent no result")),
                };
            }
        }
        if let Some(count) = heartbeat.as_ref().map(Heartbeat::count)
            && count != last_count
        {
            last_count = count;
            since = std::time::Instant::now();
        }
        if let Some(limit) = limit
            && has_stalled(since.elapsed(), limit, cancel.is_cancelled())
        {
            token.cancel();
            return Ok(Attempt::Stalled);
        }
    }
}

/// The local model as a live session's map worker ([`SummaryService::live_mapper`]): each
/// window loads the model on the worker's thread, maps it and frees it, so the model's
/// memory is held only while a window is mapped (`docs/live-summaries.md`).
struct LocalMapper {
    config: LocalSummaryConfig,
    loader: Arc<dyn LoadLocal>,
    stalled: &'static AtomicBool,
    folder: LoadedFolder,
}

impl MapChunk for LocalMapper {
    fn model_id(&self) -> &str {
        &self.config.model_id
    }

    fn seconds(&self) -> u32 {
        self.config.chunk_seconds
    }

    fn map(
        &self,
        input: &SummaryInput,
        ranges: &[std::ops::Range<usize>],
        cancel: &CancelToken,
    ) -> Result<SummaryV1, String> {
        // On the card in a child (T9.3), so a crash during a meeting costs one window and the
        // summary after the meeting maps it. The whole child runs below normal, as this thread.
        // A window's failure is reported and the next window tries again; only a refused load
        // moves to the CPU, as it does below.
        let mut on_cpu = None;
        if let Some(child) = usable_child(&self.config, self.stalled) {
            let work = ChildWork::LastChunk {
                ranges: ranges.to_vec(),
            };
            let attempt = ChildTry {
                child,
                config: &self.config,
                stalled: self.stalled,
                folder: self.folder,
                background: true,
            };
            match run_in_child(&attempt, work, input, cancel).map_err(|e| e.to_string())? {
                InChild::Finished(s) => return Ok(s.summary),
                InChild::GaveUp(LocalSummaryError::Load(_)) => {
                    on_cpu = Some(LocalSummaryConfig {
                        gpu: false,
                        ..self.config.clone()
                    });
                }
                InChild::GaveUp(why) => return Err(why.to_string()),
            }
        }
        let config = on_cpu.as_ref().unwrap_or(&self.config);
        let (loaded, _) =
            load_with_fallback(config, &*self.loader, self.stalled).map_err(|e| e.to_string())?;
        let mut model = loaded.model;
        let options = options_of(&self.config, cancel);
        let run = map_last_chunk(model.as_mut(), input, &options, ranges);
        drop(model);
        run.result.map_err(|e| e.to_string())
    }
}

/// Loads the model of `config`; a refused GPU load (llama.cpp's own refusal, never a
/// missing file) is retried once on the CPU when [`LocalSummaryConfig::cpu_fallback`],
/// returning the refusal with the model. llama.cpp's errors do not tell a GPU refusal
/// (out of memory, a backend that cannot start) from a file no backend can load, so every
/// load error is retried; when the retry fails too both errors are returned
/// ([`LocalSummaryError::LoadRetried`]).
///
/// After a stall in this app session the GPU is not tried: the CPU, with that as the reason.
fn load_with_fallback(
    config: &LocalSummaryConfig,
    loader: &dyn LoadLocal,
    stalled: &AtomicBool,
) -> Result<(LoadedModel, Option<Box<LocalSummaryError>>), RuntimeError> {
    if config.gpu && config.cpu_fallback && stalled.load(Ordering::Acquire) {
        let cpu = LocalSummaryConfig {
            gpu: false,
            ..config.clone()
        };
        return loader
            .load(&cpu)
            .map(|m| (m, Some(Box::new(LocalSummaryError::GpuStalled))));
    }
    match loader.load(config) {
        Err(RuntimeError::LocalSummary(refusal @ LocalSummaryError::Load(_)))
            if config.gpu && config.cpu_fallback =>
        {
            let cpu = LocalSummaryConfig {
                gpu: false,
                ..config.clone()
            };
            match loader.load(&cpu) {
                Ok(m) => Ok((m, Some(Box::new(refusal)))),
                Err(RuntimeError::LocalSummary(cpu @ LocalSummaryError::Load(_))) => {
                    Err(RuntimeError::LocalSummary(LocalSummaryError::LoadRetried {
                        gpu: Box::new(refusal),
                        cpu: Box::new(cpu),
                    }))
                }
                Err(e) => Err(e),
            }
        }
        r => r.map(|m| (m, None)),
    }
}

/// Stores `summary` of the meeting of `view` under `model` in one transaction (module
/// docs: the meeting must still exist with the same segment ids; older summaries become
/// tombstones; only an empty title is filled; the partials made during the meeting are
/// deleted, [`crate::live_summaries`]). Returns the row id and whether the title was set.
pub(crate) fn store_summary<R: Rng + ?Sized>(
    services: &Services,
    view: &MeetingView,
    model: String,
    summary: &SummaryV1,
    rng: &mut R,
) -> Result<(EntityId, bool), RuntimeError> {
    let meeting = view.meeting.id;
    let clock = Arc::clone(&services.clock);
    let mut db = services.store();
    let (store, hlc) = db.parts();
    let template = view.meeting.template;
    let stored = store.transaction(|tx| -> Result<_, RuntimeError> {
        let mut stamp = Stamp {
            clock: &*clock,
            hlc,
            rng: &mut *rng,
        };
        // The meeting and its transcript must be what was summarised: a summary of a
        // deleted meeting, or one citing segments a re-transcription has replaced, is
        // not stored (the caller summarises again).
        let now = MeetingView::load(&*tx, meeting)?.ok_or(RuntimeError::Conflict(
            "the meeting was deleted during the summary",
        ))?;
        let ids = |v: &MeetingView| -> Vec<EntityId> {
            let mut ids: Vec<EntityId> = v.segments.iter().map(|s| s.id).collect();
            ids.sort();
            ids
        };
        // A text edit (T12.16) keeps the ids but changes what was summarised.
        if ids(&now) != ids(view) || now.edited != view.edited {
            return Err(RuntimeError::Conflict(
                "the transcript changed during the summary",
            ));
        }
        for mut old in tx.summaries(meeting)? {
            if !old.meta.deleted {
                old.meta.mark_deleted(stamp.hlc, stamp.clock);
                tx.put_summary(old)?;
            }
        }
        let row = Summary::new(meeting, template, Some(model), summary.clone(), &mut stamp);
        let id = row.id;
        tx.put_summary(row)?;
        // Re-read: only an empty title is filled, nothing else changes.
        let mut title_set = false;
        if let Some(mut m) = tx.meeting(meeting)?
            && m.title.trim().is_empty()
            && !summary.title.trim().is_empty()
        {
            m.title = summary.title.clone();
            m.meta.touch(stamp.hlc, stamp.clock);
            tx.put_meeting(m)?;
            title_set = true;
        }
        // The live partials are content; a summary now exists, so they go.
        tx.delete_local_value(crate::live_summaries::record_key(meeting).as_str())?;
        Ok((id, title_set))
    });
    if stored.is_ok() {
        // Out of the WAL too; a reader blocking the checkpoint makes it fail retryably and
        // the store's closing checkpoint finishes it.
        let _ = store.flush_erasures();
    }
    stored
}

/// The prompt input of a meeting view with the user's summary style and own words.
fn input_of(view: &MeetingView, settings: &PromptSettings) -> SummaryInput {
    let m = &view.meeting;
    SummaryInput {
        meeting: MeetingInfo {
            title: (!m.title.trim().is_empty()).then(|| m.title.clone()),
            started: Some(format_local(m.started_unix_ms, m.utc_offset_minutes)),
            duration_ms: m.duration_ms.unwrap_or(0),
            attendees: view.attendees(),
        },
        segments: view
            .segments
            .iter()
            .map(|s| PromptSegment::from_segment(s, &view.speaker_name(s)))
            .collect(),
        notes: view
            .notes
            .iter()
            .map(|n| UserNote {
                at: n.at,
                text: n.text.clone(),
            })
            .collect(),
        template: m.template,
        prompt_variant: earshot_core::prompt::PromptVariant::Current,
        style: settings.style.clone(),
        style_preset: settings.style_preset,
        vocabulary: settings.vocabulary.clone(),
        user_name: view.user_name(),
    }
}
