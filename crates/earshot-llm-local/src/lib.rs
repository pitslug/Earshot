//! Local LLM on llama.cpp, in process (PLAN D19, T6.19 spike).
//!
//! llama.cpp comes from llama-cpp-sys-2 (pinned with `=`, like whisper-rs). Both llama.cpp
//! and whisper.cpp vendor ggml, at different versions, and statically linking both into one
//! binary fails with hundreds of duplicate ggml symbols. So llama.cpp and its ggml are built
//! as DLLs (`dynamic-link`: `llama.dll`, `ggml.dll`, `ggml-base.dll`, `ggml-cpu.dll`, plus
//! `ggml-cuda.dll` or `ggml-vulkan.dll`) while whisper.cpp's ggml stays static in the
//! executable. The Windows loader binds `llama.dll` to its own `ggml.dll` by DLL name, so the
//! two copies never see each other; the executable imports only `llama_*` from `llama.dll`.
//! The one way to break that is a `ggml_*` call from Rust (it would bind to whisper's static
//! copy), which `ffi.rs` never makes (`ffi_calls_only_llama_functions`). Details, the build
//! matrix and measured speeds: `docs/local-llm.md`.
//!
//! llama.cpp's `common` library is not built: it links an HTTP client (cpp-httplib) for
//! model downloads. The JSON Schema -> grammar conversion is [`grammar`] instead.
//!
//! No network, no telemetry: llama.cpp is built without curl or `common`, and this crate
//! opens no sockets.

#![deny(unsafe_code)]

mod chat_format;
#[allow(unsafe_code)]
mod dll_folder;
#[allow(unsafe_code)]
mod ffi;
mod gate;
#[cfg(test)]
mod gbnf_match;
pub mod grammar;
pub mod log;
pub mod summarize;
pub mod thinking;

pub use dll_folder::{FolderRefused, load_from_folder, loaded_folder};
pub use gate::{LlamaHold, callers_slept_at_the_gate, hold_llama, llama_held, wait_for_llama};

use std::ffi::CString;
use std::fmt;
use std::path::Path;
use std::time::{Duration, Instant};

use earshot_core::asr::CancelToken;
use earshot_core::prompt::{ChatMessage, Role};

pub use log::LlamaBackend;

/// Errors of this crate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LlmError {
    /// llama.cpp could not load the model or create a context.
    Load(String),
    /// Invalid input (a NUL byte in text, an unknown chat template, an unparsable grammar).
    Input(String),
    /// The prompt plus the reply budget does not fit in the context.
    ContextTooSmall {
        /// Prompt tokens.
        prompt: usize,
        /// Reply budget.
        max_tokens: usize,
        /// Context size.
        context: usize,
    },
    /// `llama_decode` failed.
    Decode(i32),
    /// The reply is not UTF-8.
    Utf8,
    /// The backend that ran is not the one asked for, or the evidence is unclear.
    Backend(String),
    /// The generation's [`CancelToken`] was cancelled.
    Cancelled,
}

impl fmt::Display for LlmError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LlmError::Load(e) => write!(f, "llama.cpp could not load the model: {e}"),
            LlmError::Input(e) => write!(f, "invalid input: {e}"),
            LlmError::ContextTooSmall {
                prompt,
                max_tokens,
                context,
            } => write!(
                f,
                "prompt of {prompt} tokens plus {max_tokens} reply tokens exceeds the {context}-token context"
            ),
            LlmError::Decode(c) => write!(f, "llama_decode failed with {c}"),
            LlmError::Utf8 => f.write_str("the reply is not valid UTF-8"),
            LlmError::Backend(e) => write!(f, "backend: {e}"),
            LlmError::Cancelled => f.write_str("generation cancelled"),
        }
    }
}

impl std::error::Error for LlmError {}

/// How to load a model.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LoadOptions {
    /// Offload every layer to the GPU backend this build has (CUDA or Vulkan); `false`
    /// keeps the model on the CPU.
    pub gpu: bool,
    /// Context size in tokens (prompt plus reply).
    pub context_tokens: u32,
    /// CPU threads for prompt and generation.
    pub threads: i32,
}

impl Default for LoadOptions {
    fn default() -> Self {
        LoadOptions {
            gpu: false,
            context_tokens: 8192,
            threads: default_threads(),
        }
    }
}

/// Half the logical cores, 1 to 8 (the CPU share PLAN T6.10b leaves the rest of the app).
#[must_use]
pub fn default_threads() -> i32 {
    let cores = std::thread::available_parallelism().map_or(2, std::num::NonZero::get);
    i32::try_from((cores / 2).clamp(1, 8)).unwrap_or(1)
}

/// What a load did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadReport {
    /// Where the weights went, from llama.cpp's own log.
    pub backend: LlamaBackend,
    /// The log lines the backend was read from.
    pub evidence: Vec<String>,
    /// The GPU llama.cpp named for this load ([`log::gpu_device`]; a CPU load of a GPU build
    /// names one too, so read it with [`Self::backend`]).
    pub device: Option<String>,
    /// llama.cpp's description of the model.
    pub description: String,
    /// Context size allocated.
    pub context_tokens: u32,
    /// Load time.
    pub load_time: Duration,
}

/// Why generation stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stop {
    /// The model ended its reply (the grammar was complete).
    EndOfGeneration,
    /// The reply budget ran out; the text is truncated.
    MaxTokens,
    /// The reply started repeating itself ([`is_looping`]) and was stopped; the text is
    /// incomplete. Distinct from [`Stop::MaxTokens`]: the reply was not long, it looped.
    Looping,
}

/// Bytes at the end of a reply the loop guard looks at.
pub const LOOP_WINDOW_BYTES: usize = 1_200;

/// The loop guard runs every this many reply tokens.
pub const LOOP_CHECK_TOKENS: usize = 32;

/// zlib compression ratio of the last [`LOOP_WINDOW_BYTES`] above which a reply loops
/// (`earshot_core::asr::compression_ratio`, the guard the transcription path uses with 2.4
/// on 30 s windows of text). A summary reply holds JSON with distinct sentences: measured
/// on the AMI dev summaries (T6.20), the last 400 bytes of replies that ran to the reply
/// limit compressed 1.5 to 2.8 when they were long but distinct, 3.3 to 5.5 when they
/// repeated.
pub const LOOP_MAX_COMPRESSION_RATIO: f32 = 4.0;

/// Whether the end of `reply` repeats itself: its last [`LOOP_WINDOW_BYTES`] (none for a
/// shorter reply) compress more than [`LOOP_MAX_COMPRESSION_RATIO`].
#[must_use]
pub fn is_looping(reply: &[u8]) -> bool {
    if reply.len() < LOOP_WINDOW_BYTES {
        return false;
    }
    let tail = String::from_utf8_lossy(&reply[reply.len() - LOOP_WINDOW_BYTES..]);
    earshot_core::asr::compression_ratio(&tail) > LOOP_MAX_COMPRESSION_RATIO
}

/// One reply.
#[derive(Debug, Clone, PartialEq)]
pub struct Generation {
    /// The reply text.
    pub text: String,
    /// Why it stopped.
    pub stop: Stop,
    /// Prompt tokens after the chat template.
    pub prompt_tokens: usize,
    /// Reply tokens sampled (the end-of-generation token not counted).
    pub generated_tokens: usize,
    /// Time to process the prompt.
    pub prompt_time: Duration,
    /// Time to sample the reply.
    pub generate_time: Duration,
}

impl Generation {
    /// Reply tokens per second.
    #[must_use]
    pub fn tokens_per_second(&self) -> f64 {
        rate(self.generated_tokens, self.generate_time)
    }

    /// Prompt tokens per second.
    #[must_use]
    pub fn prompt_tokens_per_second(&self) -> f64 {
        rate(self.prompt_tokens, self.prompt_time)
    }
}

fn rate(tokens: usize, time: Duration) -> f64 {
    let s = time.as_secs_f64();
    if s > 0.0 { tokens as f64 / s } else { 0.0 }
}

/// `llama_print_system_info()` of llama.cpp's own ggml (CPU features it was built with).
#[must_use]
pub fn system_info() -> String {
    ffi::system_info()
}

/// Loads `model` on the GPU and decodes a few tokens, so the GPU really runs (T8.4ada): a CUDA set
/// on a card it holds no code for ends the process here. Returns where the model ran, from its
/// load's evidence.
///
/// # Errors
/// As [`LocalLlm::load`] and [`LocalLlm::generate_cancellable`].
pub fn probe_gpu(model: &Path) -> Result<LlamaBackend, LlmError> {
    let options = LoadOptions {
        gpu: true,
        context_tokens: 512,
        ..LoadOptions::default()
    };
    let (mut llm, report) = LocalLlm::load(model, &options)?;
    let messages = [ChatMessage::new(
        earshot_core::prompt::Role::User,
        "Say hello.",
    )];
    llm.generate_cancellable(&messages, "", "root ::= [a-z ]+", 4, &CancelToken::new())?;
    Ok(report.backend)
}

/// Whether this build of llama.cpp has a GPU backend (the `cuda` or `vulkan` feature).
#[must_use]
pub fn supports_gpu() -> bool {
    ffi::supports_gpu_offload()
}

/// [`LocalLlm::format_chat`] with llama.cpp's template call passed in as `apply`
/// (`ffi::Engine::apply_template` in production, a stub in tests): `apply`'s prompt when it
/// formats the template, else [`chat_format`]'s when the template is Gemma 4's, else an
/// error.
fn format_chat_with(
    template: &std::ffi::CStr,
    messages: &[ChatMessage],
    apply: impl Fn(&std::ffi::CStr, &[(CString, CString)]) -> Option<String>,
) -> Result<String, LlmError> {
    let msgs = messages
        .iter()
        .map(|m| {
            let role = match m.role {
                Role::System => c"system",
                Role::User => c"user",
                Role::Assistant => c"assistant",
            };
            CString::new(m.content.as_bytes())
                .map(|c| (role.to_owned(), c))
                .map_err(|_| LlmError::Input("message contains NUL".into()))
        })
        .collect::<Result<Vec<_>, _>>()?;
    if let Some(prompt) = apply(template, &msgs) {
        return Ok(prompt);
    }
    let template = template.to_string_lossy();
    if chat_format::is_gemma4(&template) {
        let turns: Vec<(&str, &str)> = messages
            .iter()
            .map(|m| {
                let role = match m.role {
                    Role::System => "system",
                    Role::User => "user",
                    Role::Assistant => "assistant",
                };
                (role, m.content.as_str())
            })
            .collect();
        return Ok(chat_format::gemma4(&turns));
    }
    Err(LlmError::Input(
        "llama.cpp does not support this model's chat template".into(),
    ))
}

/// Prompt tokens per `llama_decode` call; chunks end on multiples of this from position 0.
const PROMPT_CHUNK: usize = 2048;

/// The saved-state slot of the boundary a prompt shares with the one before it (T12.25): the
/// shared rules of a run's requests. A later "last" state never replaces it.
const SHARED_SLOT: usize = 0;

/// The saved-state slot of a prompt's last boundary (for a repair round, which repeats it).
const LAST_SLOT: usize = 1;

/// The state of sequence 0 after decoding exactly `tokens` (a multiple of the micro-batch).
struct Snapshot {
    tokens: Vec<i32>,
    state: Vec<u8>,
}

/// A boundary of `prompt` shared with `cached` (T12.25 prefix reuse): their common prefix,
/// less the last prompt token (whose logits the reply needs), rounded down to a multiple of
/// `ubatch`, so the rest of the prompt is computed in the same micro-batches as a fresh
/// prompt would be (docs/summary-eval.md, T12.25 part 1).
#[must_use]
fn reusable_prefix(cached: &[i32], prompt: &[i32], ubatch: usize) -> usize {
    if ubatch == 0 {
        return 0;
    }
    let common = cached
        .iter()
        .zip(prompt)
        .take_while(|(a, b)| a == b)
        .count()
        .min(prompt.len().saturating_sub(1));
    common / ubatch * ubatch
}

/// The `llama_decode` chunks of prompt positions `start..len`: the first ends at the next
/// multiple of `chunk` (or `len`), every later one is `chunk` long, so for `start` = 0 they
/// are `len` cut every `chunk`, exactly as without reuse.
#[must_use]
fn prompt_chunks(start: usize, len: usize, chunk: usize) -> Vec<std::ops::Range<usize>> {
    let mut out = Vec::new();
    let mut at = start;
    while at < len {
        let end = ((at / chunk + 1) * chunk).min(len);
        out.push(at..end);
        at = end;
    }
    out
}

/// [`prompt_chunks`] also cut at every `save` point inside `start..len`, where prefix reuse
/// saves the state. Cuts fall on micro-batch boundaries, so llama.cpp still computes the same
/// micro-batches; only the call that ends at a cut also returns that token's logits, which
/// nothing reads.
#[must_use]
fn decode_ranges(
    start: usize,
    len: usize,
    chunk: usize,
    save: &[usize],
) -> Vec<std::ops::Range<usize>> {
    let mut out = Vec::new();
    for r in prompt_chunks(start, len, chunk) {
        let mut cuts: Vec<usize> = save
            .iter()
            .copied()
            .filter(|&p| p > r.start && p < r.end)
            .collect();
        cuts.sort_unstable();
        cuts.dedup();
        let mut at = r.start;
        for c in cuts {
            out.push(at..c);
            at = c;
        }
        out.push(at..r.end);
    }
    out
}

/// Prompt token counts of the requests since the last [`LocalLlm::take_reuse_stats`] (T12.25,
/// evaluation): how many prompt tokens there were and how many of them the context already
/// held, so were not decoded again.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ReuseStats {
    /// Requests.
    pub requests: usize,
    /// Prompt tokens of those requests.
    pub prompt_tokens: usize,
    /// Of those, kept from the context instead of decoded.
    pub reused_tokens: usize,
}

impl ReuseStats {
    /// Prompt tokens llama.cpp decoded.
    #[must_use]
    pub fn decoded_tokens(&self) -> usize {
        self.prompt_tokens - self.reused_tokens
    }
}

/// A loaded GGUF model with one context. Not `Send`: use it on the thread that loaded it.
pub struct LocalLlm {
    engine: ffi::Engine,
    template: CString,
    heartbeat: Heartbeat,
    /// Keep the shared start of consecutive prompts in the context (T12.25); off by default.
    prefix_reuse: bool,
    /// Counts since the last [`LocalLlm::take_reuse_stats`].
    reuse: ReuseStats,
    /// Saved states: [`SHARED_SLOT`] and [`LAST_SLOT`].
    snapshots: [Option<Snapshot>; 2],
}

/// Counts a run's progress (prompt batches and reply tokens), readable from another thread: a
/// count that stops moving means the run is stuck inside one llama.cpp call, which nothing can
/// interrupt.
#[derive(Debug, Clone, Default)]
pub struct Heartbeat(std::sync::Arc<std::sync::atomic::AtomicU64>);

impl Heartbeat {
    /// One more step of progress.
    pub fn beat(&self) {
        self.0.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }

    /// The steps so far.
    #[must_use]
    pub fn count(&self) -> u64 {
        self.0.load(std::sync::atomic::Ordering::Relaxed)
    }
}

impl LocalLlm {
    /// Loads `path`. The backend is read from llama.cpp's load log; asking for the GPU on a
    /// build or machine without one, or getting a partial offload, is an error, never a
    /// silent CPU run. Loads in one process run one at a time (each reads its own window of
    /// the process-wide load log), so a concurrent call waits.
    ///
    /// # Errors
    /// [`LlmError::Load`], [`LlmError::Backend`], or [`LlmError::Input`] when the model has
    /// no chat template.
    pub fn load(path: &Path, options: &LoadOptions) -> Result<(Self, LoadReport), LlmError> {
        let c_path = CString::new(path.to_string_lossy().as_bytes())
            .map_err(|_| LlmError::Input("model path contains NUL".into()))?;
        ffi::init();
        let started = Instant::now();
        // Loads are serialised here, so every caller's log window is its own load's.
        let (engine, lines) = log::load_window(|| {
            ffi::Engine::load(
                &c_path,
                &ffi::EngineParams {
                    gpu_layers: if options.gpu { 999 } else { 0 },
                    n_ctx: options.context_tokens,
                    n_batch: options.context_tokens.min(2048),
                    threads: options.threads,
                },
            )
        })
        .map_err(|e| LlmError::Backend(e.to_owned()))?;
        let load_time = started.elapsed();
        let lines =
            lines.ok_or_else(|| LlmError::Backend("the load log overflowed the capture".into()))?;
        let Some(engine) = engine else {
            let tail: Vec<&String> = lines.iter().rev().take(5).collect();
            return Err(LlmError::Load(format!(
                "{} (last log lines: {tail:?})",
                path.display()
            )));
        };
        let backend = log::classify(&lines).map_err(LlmError::Backend)?;
        let device = log::gpu_device(&lines);
        let gpu_ran = backend != LlamaBackend::Cpu;
        if gpu_ran != options.gpu {
            return Err(LlmError::Backend(format!(
                "asked for {} but llama.cpp loaded the model on {}",
                if options.gpu { "the GPU" } else { "the CPU" },
                backend.name()
            )));
        }
        let template = engine
            .chat_template()
            .ok_or_else(|| LlmError::Input("the model has no chat template".into()))?;
        let evidence = lines
            .into_iter()
            .filter(|l| {
                l.contains("using device")
                    || l.contains("model buffer size")
                    || l.contains("layers to GPU")
            })
            .collect();
        let report = LoadReport {
            backend,
            evidence,
            device,
            description: engine.description(),
            context_tokens: engine.n_ctx(),
            load_time,
        };
        Ok((
            LocalLlm {
                engine,
                template,
                heartbeat: Heartbeat::default(),
                prefix_reuse: false,
                reuse: ReuseStats::default(),
                snapshots: [None, None],
            },
            report,
        ))
    }

    /// Turns prompt prefix reuse on or off (T12.25 part 1; off after a load). On, a request
    /// keeps the start it shares with what the context holds ([`reusable_prefix`]) and
    /// decodes only the rest; greedy replies are meant to be byte-identical to a fresh
    /// prompt's, which docs/summary-eval.md proves on the dev meetings. Evaluation only until
    /// the owner turns it on in the app.
    pub fn set_prefix_reuse(&mut self, on: bool) {
        self.prefix_reuse = on;
    }

    /// Forgets what the context holds, so the next request starts fresh (a new meeting).
    pub fn forget_prefix(&mut self) {
        self.engine.clear();
        self.snapshots = [None, None];
    }

    /// The prompt counts since the last call ([`ReuseStats`]), reset.
    pub fn take_reuse_stats(&mut self) -> ReuseStats {
        std::mem::take(&mut self.reuse)
    }

    /// Readies the context for `tokens` and decodes them, checking `cancel` before every
    /// call. Without prefix reuse: from position 0 after clearing the memory, in
    /// [`prompt_chunks`], as before T12.25. With it: from the longest saved [`Snapshot`]
    /// whose tokens start the prompt (shorter than it), loaded with `llama_state_seq_set_data`,
    /// in [`decode_ranges`]; on the way the state is saved at the boundary this prompt shares
    /// with what the context held before it (into [`SHARED_SLOT`]) and at its last boundary
    /// (into [`LAST_SLOT`]), both from [`reusable_prefix`], unless a slot already holds it.
    /// Returns the tokens kept.
    pub(crate) fn decode_prompt(
        &mut self,
        tokens: &mut [i32],
        cancel: &CancelToken,
    ) -> Result<usize, LlmError> {
        if !self.prefix_reuse {
            self.engine.clear();
            self.count_request(tokens.len(), 0);
            for range in prompt_chunks(0, tokens.len(), PROMPT_CHUNK) {
                self.decode_range(&mut tokens[range], cancel)?;
            }
            return Ok(0);
        }
        let ubatch = self.engine.ubatch();
        let shared = reusable_prefix(self.engine.cached(), tokens, ubatch);
        let last = reusable_prefix(tokens, tokens, ubatch);
        let usable = |s: &Snapshot| s.tokens.len() < tokens.len() && tokens.starts_with(&s.tokens);
        let best = self
            .snapshots
            .iter()
            .flatten()
            .filter(|s| usable(s))
            .max_by_key(|s| s.tokens.len());
        let mut start = 0;
        match best {
            Some(snap) => {
                if self.engine.load_state(&snap.state, &snap.tokens) {
                    start = snap.tokens.len();
                }
            }
            None => self.engine.clear(),
        }
        let held = |b: usize, slots: &[Option<Snapshot>; 2]| {
            slots
                .iter()
                .flatten()
                .any(|s| s.tokens.len() == b && tokens.starts_with(&s.tokens))
        };
        let mut save: Vec<(usize, usize)> = Vec::new();
        for (b, slot) in [(shared, SHARED_SLOT), (last, LAST_SLOT)] {
            if b > start && !held(b, &self.snapshots) && !save.iter().any(|(p, _)| *p == b) {
                save.push((b, slot));
            }
        }
        let points: Vec<usize> = save.iter().map(|(p, _)| *p).collect();
        self.count_request(tokens.len(), start);
        for range in decode_ranges(start, tokens.len(), PROMPT_CHUNK, &points) {
            let end = range.end;
            self.decode_range(&mut tokens[range], cancel)?;
            if let Some(&(_, slot)) = save.iter().find(|(p, _)| *p == end)
                && let Some(state) = self.engine.save_state()
            {
                self.snapshots[slot] = Some(Snapshot {
                    tokens: tokens[..end].to_vec(),
                    state,
                });
            }
        }
        Ok(start)
    }

    /// Adds one request of `prompt` tokens, `kept` of them from a saved state, to the counts.
    fn count_request(&mut self, prompt: usize, kept: usize) {
        self.reuse.requests += 1;
        self.reuse.prompt_tokens += prompt;
        self.reuse.reused_tokens += kept;
    }

    /// One `llama_decode` call of prompt tokens, after a cancellation check.
    fn decode_range(&mut self, tokens: &mut [i32], cancel: &CancelToken) -> Result<(), LlmError> {
        if cancel.is_cancelled() {
            return Err(LlmError::Cancelled);
        }
        let rc = self.engine.decode(tokens);
        if rc != 0 {
            return Err(LlmError::Decode(rc));
        }
        self.heartbeat.beat();
        Ok(())
    }

    /// This model's progress counter ([`Heartbeat`]).
    #[must_use]
    pub fn heartbeat(&self) -> Heartbeat {
        self.heartbeat.clone()
    }

    /// The chat prompt for `messages` with the model's template, ending where the
    /// assistant's reply starts.
    ///
    /// # Errors
    /// [`LlmError::Input`] for a NUL byte or a template neither llama.cpp nor
    /// [`chat_format`] supports.
    pub fn format_chat(&self, messages: &[ChatMessage]) -> Result<String, LlmError> {
        format_chat_with(&self.template, messages, ffi::Engine::apply_template)
    }

    /// Prompt tokens of `messages` with the chat template and `assistant_prefix`, as
    /// [`LocalLlm::generate_with_prefix`] would decode them.
    ///
    /// # Errors
    /// [`LlmError::Input`] for a NUL byte, an unsupported template or a failed
    /// tokenization.
    pub fn prompt_tokens(
        &self,
        messages: &[ChatMessage],
        assistant_prefix: &str,
    ) -> Result<usize, LlmError> {
        let mut prompt = self.format_chat(messages)?;
        prompt.push_str(assistant_prefix);
        self.engine
            .tokenize(&prompt)
            .map(|t| t.len())
            .ok_or_else(|| LlmError::Input("tokenization failed".into()))
    }

    /// The allocated context size in tokens.
    #[must_use]
    pub fn context_tokens(&self) -> usize {
        self.engine.n_ctx() as usize
    }

    /// Generates a reply to `messages` constrained by `grammar` (GBNF, start rule `root`,
    /// for example from [`grammar::summary_grammar`]), greedily (deterministic for a given
    /// build and backend), up to `max_tokens` tokens. The context is cleared first.
    ///
    /// # Errors
    /// [`LlmError::Input`] (bad message or grammar), [`LlmError::ContextTooSmall`],
    /// [`LlmError::Decode`], [`LlmError::Utf8`].
    pub fn generate(
        &mut self,
        messages: &[ChatMessage],
        grammar: &str,
        max_tokens: usize,
    ) -> Result<Generation, LlmError> {
        self.generate_with_prefix(messages, "", grammar, max_tokens)
    }

    /// [`LocalLlm::generate`] with `assistant_prefix` appended to the chat prompt, where the
    /// assistant's reply starts. The prefix is part of the prompt, not of the reply; the
    /// grammar constrains only what follows it. Used for chat templates whose own rendering
    /// adds text there, for example Qwen3's empty `<think>` block when thinking is off.
    ///
    /// # Errors
    /// As [`LocalLlm::generate`].
    pub fn generate_with_prefix(
        &mut self,
        messages: &[ChatMessage],
        assistant_prefix: &str,
        grammar: &str,
        max_tokens: usize,
    ) -> Result<Generation, LlmError> {
        self.generate_cancellable(
            messages,
            assistant_prefix,
            grammar,
            max_tokens,
            &CancelToken::new(),
        )
    }

    /// [`LocalLlm::generate_with_prefix`] that stops when `cancel` is cancelled: checked
    /// before the prompt, between prompt batches and before every reply token.
    ///
    /// # Errors
    /// As [`LocalLlm::generate`], and [`LlmError::Cancelled`].
    pub fn generate_cancellable(
        &mut self,
        messages: &[ChatMessage],
        assistant_prefix: &str,
        grammar: &str,
        max_tokens: usize,
        cancel: &CancelToken,
    ) -> Result<Generation, LlmError> {
        let mut prompt = self.format_chat(messages)?;
        prompt.push_str(assistant_prefix);
        let c_grammar =
            CString::new(grammar).map_err(|_| LlmError::Input("grammar contains NUL".into()))?;
        let mut sampler = self.engine.grammar_sampler(&c_grammar).ok_or_else(|| {
            LlmError::Input("llama.cpp rejected the grammar (see its log)".into())
        })?;
        let mut tokens = self
            .engine
            .tokenize(&prompt)
            .ok_or_else(|| LlmError::Input("tokenization failed".into()))?;
        let context = self.engine.n_ctx() as usize;
        if tokens.len() + max_tokens > context {
            return Err(LlmError::ContextTooSmall {
                prompt: tokens.len(),
                max_tokens,
                context,
            });
        }
        let started = Instant::now();
        self.decode_prompt(&mut tokens, cancel)?;
        let prompt_time = started.elapsed();
        let started = Instant::now();
        let mut bytes = Vec::new();
        let mut generated = 0;
        let mut stop = Stop::MaxTokens;
        while generated < max_tokens {
            if cancel.is_cancelled() {
                return Err(LlmError::Cancelled);
            }
            let token = self
                .engine
                .sample_greedy(&mut sampler)
                .ok_or_else(|| LlmError::Input("the grammar allows no token here".into()))?;
            if self.engine.is_eog(token) {
                stop = Stop::EndOfGeneration;
                break;
            }
            self.engine.piece(token, &mut bytes);
            generated += 1;
            if generated.is_multiple_of(LOOP_CHECK_TOKENS) && is_looping(&bytes) {
                stop = Stop::Looping;
                break;
            }
            let rc = self.engine.decode(&mut [token]);
            if rc != 0 {
                return Err(LlmError::Decode(rc));
            }
            self.heartbeat.beat();
        }
        let generate_time = started.elapsed();
        Ok(Generation {
            text: String::from_utf8(bytes).map_err(|_| LlmError::Utf8)?,
            stop,
            prompt_tokens: tokens.len(),
            generated_tokens: generated,
            prompt_time,
            generate_time,
        })
    }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    /// ggml/gguf names the crate may use. Only types: they carry no symbol, so they cannot
    /// bind to the wrong ggml. Add one only after review (AGENTS.md).
    const GGML_TYPES_ALLOWED: &[&str] = &[concat!("gg", "ml_log_level")];

    /// Every identifier matching `\b(ggml|gguf)_[A-Za-z0-9_]+` in `text`, except the
    /// allowed types. Matching the bare identifier (not a `sys::` path) also catches brace
    /// imports, `as` renames, glob imports with bare calls, and names in comments.
    fn foreign_names(text: &str) -> Vec<String> {
        let is_ident = |c: u8| c.is_ascii_alphanumeric() || c == b'_';
        let bytes = text.as_bytes();
        let mut found = Vec::new();
        let mut i = 0;
        while i < bytes.len() {
            let rest = &bytes[i..];
            let starts = rest.starts_with(b"ggml_") || rest.starts_with(b"gguf_");
            if !starts || (i > 0 && is_ident(bytes[i - 1])) {
                i += 1;
                continue;
            }
            let end = i + 5 + rest[5..].iter().take_while(|&&c| is_ident(c)).count();
            // The prefix and every identifier byte are ASCII, so both ends are char
            // boundaries.
            let name = &text[i..end];
            if end > i + 5 && !GGML_TYPES_ALLOWED.contains(&name) {
                found.push(name.to_string());
            }
            i = end;
        }
        found
    }

    /// Whether `dir` is build output: `<root>/target`, or any directory cargo marked with
    /// `CACHEDIR.TAG` (a `--target-dir`). Its generated bindings name every ggml item. A
    /// module directory called `target` deeper in the tree is still scanned.
    fn is_build_output(root: &Path, dir: &Path) -> bool {
        *dir == root.join("target") || dir.join("CACHEDIR.TAG").is_file()
    }

    /// Every `.rs` file under `dir`, recursively, sorted, skipping build output.
    fn rust_files(dir: &Path) -> Vec<PathBuf> {
        let mut out = Vec::new();
        let mut stack = vec![dir.to_path_buf()];
        while let Some(d) = stack.pop() {
            let entries = std::fs::read_dir(&d).unwrap_or_else(|e| panic!("{}: {e}", d.display()));
            for entry in entries {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    if !is_build_output(dir, &path) {
                        stack.push(path);
                    }
                } else if path.extension().is_some_and(|e| e == "rs") {
                    out.push(path);
                }
            }
        }
        out.sort();
        out
    }

    /// `root` joined with a `/`-separated relative path, one component at a time (so it
    /// compares equal to the paths `read_dir` returns).
    fn under(root: &Path, rel: &str) -> PathBuf {
        rel.split('/').fold(root.to_path_buf(), |p, c| p.join(c))
    }

    /// The `.rs` files under `root` and `"<file>: <name>"` for every foreign name in them.
    fn scan(root: &Path) -> (Vec<PathBuf>, Vec<String>) {
        let files = rust_files(root);
        let mut found = Vec::new();
        for path in &files {
            let text = std::fs::read_to_string(path).unwrap();
            for name in foreign_names(&text) {
                found.push(format!("{}: {name}", path.display()));
            }
        }
        (files, found)
    }

    /// The guard for the one rule that keeps the two ggml copies apart (crate docs): Rust
    /// names no `ggml_*`/`gguf_*` item except reviewed types. An un-imported reference binds
    /// to whisper.cpp's static ggml instead of `ggml.dll` (measured: docs/local-llm.md).
    /// Scans every `.rs` file of the crate: `src/` recursively, `tests/`, `examples/` and
    /// `build.rs`, comments included.
    /// Every way into `ffi.rs` that does not need an `Engine` starts at `init`, and `init` starts
    /// at the gate; an `Engine` exists only after that. `greedy_with_grammar` calls nothing itself:
    /// it works on what it is given.
    #[test]
    fn ffi_calls_pass_the_gate() {
        let src = include_str!("ffi.rs");
        let lines: Vec<&str> = src.lines().collect();
        let mut doors = Vec::new();
        for (i, line) in lines.iter().enumerate() {
            let t = line.trim_start();
            let Some(rest) = t.strip_prefix("pub(crate) fn ") else {
                continue;
            };
            let name = rest.split(['(', '<']).next().unwrap();
            let open = (i..lines.len())
                .find(|&k| lines[k].trim_end().ends_with('{'))
                .unwrap();
            let signature = lines[i..=open].join(" ");
            if signature.contains("self") || name == "greedy_with_grammar" {
                continue;
            }
            let first = lines[open + 1].trim();
            let wanted = if name == "init" {
                "crate::gate::wait_for_llama();"
            } else {
                "init();"
            };
            assert_eq!(first, wanted, "{name} must start at the gate");
            doors.push(name.to_owned());
        }
        doors.sort();
        assert_eq!(
            doors,
            [
                "apply_template",
                "init",
                "load",
                "supports_gpu_offload",
                "system_info"
            ]
        );
        // A llama call anywhere else does not compile: every one is `unsafe`, and the crate denies
        // that outside these two modules.
        let lib = include_str!("lib.rs");
        assert!(lib.starts_with("//!") && lib.contains(concat!("\n#![deny(unsafe", "_code)]\n")));
        let allows: Vec<&str> = lib
            .lines()
            .zip(lib.lines().skip(1))
            .filter(|(line, _)| line.trim() == concat!("#[allow(unsafe", "_code)]"))
            .map(|(_, next)| next.trim())
            .collect();
        assert_eq!(allows, ["mod dll_folder;", "mod ffi;"]);
    }

    #[test]
    fn ffi_calls_only_llama_functions() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"));
        let (files, found) = scan(root);
        assert!(
            found.is_empty(),
            "ggml/gguf names used from Rust: {found:?}"
        );
        for must in [
            "build.rs",
            "src/lib.rs",
            "src/ffi.rs",
            "src/log.rs",
            "src/grammar.rs",
            "tests/coexist.rs",
            "tests/common/mod.rs",
        ] {
            assert!(
                files.contains(&under(root, must)),
                "{must} not scanned: {files:?}"
            );
        }
    }

    #[test]
    fn ffi_guard_finds_every_form_of_ggml_reference() {
        // Split with concat! so this file's own scan does not see the examples.
        let cases: &[(&str, &[&str])] = &[
            // A path call through the alias or the crate name.
            (
                concat!("let n = unsafe { sys::gg", "ml_backend_dev_count() };"),
                &[concat!("gg", "ml_backend_dev_count")],
            ),
            (
                concat!("let g = llama_cpp_sys_2::gg", "ml_abort(x);"),
                &[concat!("gg", "ml_abort")],
            ),
            // A function passed on, and a call with a space before the parenthesis.
            (
                concat!(
                    "let f = sys::gg",
                    "ml_time_us; sys::gg",
                    "uf_init_from_file (p, q);"
                ),
                &[
                    concat!("gg", "ml_time_us"),
                    concat!("gg", "uf_init_from_file"),
                ],
            ),
            // A brace-list import.
            (
                concat!("use llama_cpp_sys_2::{llama_free, gg", "ml_time_us};"),
                &[concat!("gg", "ml_time_us")],
            ),
            // A renamed import.
            (
                concat!("use llama_cpp_sys_2::gg", "ml_abort as stop;"),
                &[concat!("gg", "ml_abort")],
            ),
            // A glob import and a bare call.
            (
                concat!(
                    "use llama_cpp_sys_2::*;\nlet n = unsafe { gg",
                    "ml_backend_dev_count() };"
                ),
                &[concat!("gg", "ml_backend_dev_count")],
            ),
            // At the start of the text and after punctuation.
            (
                concat!("gg", "ml_a();(gg", "uf_b)"),
                &[concat!("gg", "ml_a"), concat!("gg", "uf_b")],
            ),
        ];
        for (text, want) in cases {
            assert_eq!(foreign_names(text), *want, "in {text:?}");
        }
        // Reviewed types, wildcards in prose and longer identifiers are not references.
        for text in [
            concat!("fn cb(level: sys::gg", "ml_log_level, t: *const c_char) {}"),
            concat!("never call a `gg", "ml_*` function"),
            concat!("let myg", "gml_count = x_gg", "ml_y + 1;"),
        ] {
            assert!(
                foreign_names(text).is_empty(),
                "{text:?}: {:?}",
                foreign_names(text)
            );
        }
    }

    #[test]
    fn ffi_guard_scans_every_rust_file_of_the_crate() {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        let root =
            std::env::temp_dir().join(format!("earshot-llm-guard-{}-{nanos}", std::process::id()));
        let call = concat!("fn f() { unsafe { gg", "ml_abort() } }\n");
        let files = [
            "build.rs",
            "src/lib.rs",
            "src/nested/deep/mod.rs",
            "src/target/mod.rs",
            "tests/t.rs",
            "tests/common/mod.rs",
            "examples/e.rs",
        ];
        for f in files {
            let path = under(&root, f);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, call).unwrap();
        }
        // Not Rust: not scanned.
        std::fs::write(root.join("notes.md"), call).unwrap();
        // Build output (generated bindings name every ggml item): not scanned, whether in
        // `target/` or in any directory cargo marks with CACHEDIR.TAG (`--target-dir`).
        for (dir, tag) in [
            ("target/debug/build/x/out", false),
            ("build-out/debug", true),
        ] {
            let path = under(&root, dir);
            std::fs::create_dir_all(&path).unwrap();
            std::fs::write(path.join("bindings.rs"), call).unwrap();
            if tag {
                std::fs::write(under(&root, "build-out").join("CACHEDIR.TAG"), "").unwrap();
            }
        }
        let (scanned, found) = scan(&root);
        std::fs::remove_dir_all(&root).unwrap();
        assert_eq!(scanned.len(), files.len(), "{scanned:?}");
        assert_eq!(found.len(), files.len(), "{found:?}");
        for f in files {
            let p = under(&root, f).display().to_string();
            assert!(
                found.iter().any(|l| l.starts_with(&p)),
                "{f} missed: {found:?}"
            );
        }
    }

    #[test]
    fn loop_guard_stops_repeating_replies_and_spares_distinct_ones() {
        // A reply that repeats one bullet.
        let bullet =
            r#"{"text":"The team discussed the use of flash memory.","source_segment_ids":[12]},"#;
        let mut looping = String::from(r#"{"title":"T","enhanced_notes":["#);
        while looping.len() < 3 * super::LOOP_WINDOW_BYTES {
            looping.push_str(bullet);
        }
        assert!(super::is_looping(looping.as_bytes()));
        // Distinct sentences (different words and numbers) do not trip it.
        let words = [
            "budget", "remote", "battery", "yellow", "rubber", "screen", "button", "cost",
            "market", "survey", "kinetic", "solar", "plastic", "titanium", "voice", "locator",
        ];
        let mut distinct = String::from(r#"{"title":"T","enhanced_notes":["#);
        let mut i = 0usize;
        while distinct.len() < 3 * super::LOOP_WINDOW_BYTES {
            let (a, b, c) = (
                words[i % 16],
                words[(i * 7 + 3) % 16],
                words[(i * 5 + 1) % 16],
            );
            distinct.push_str(&format!(
                r#"{{"text":"Point {i}: the {a} affects the {b} and the {c} plan by {} euro.","source_segment_ids":[{},{}]}},"#,
                i * 3 + 7,
                i + 1,
                i * 2 + 5
            ));
            i += 1;
        }
        assert!(!super::is_looping(distinct.as_bytes()));
        // Short replies are never judged.
        assert!(!super::is_looping(
            &looping.as_bytes()[..super::LOOP_WINDOW_BYTES - 1]
        ));
    }

    #[test]
    fn format_chat_falls_back_to_gemma4_only_when_llama_cpp_refuses_a_gemma4_template() {
        use earshot_core::prompt::{ChatMessage, Role};
        use std::cell::RefCell;
        use std::ffi::{CStr, CString};

        // Gemma 4's markers in a template llama.cpp's built-in families do not know.
        let gemma = CString::new(
            "{%- if messages[0]['role'] == 'system' -%}{{ '<|turn>system\\n' }}{%- endif -%}\
             {%- for m in messages -%}{{ '<|turn>' + role + '\\n' }}{{ m['content'] }}\
             {{ '<turn|>\\n' }}{%- endfor -%}\
             {%- if add_generation_prompt -%}{{ '<|turn>model\\n' }}\
             {{ '<|channel>thought\\n<channel|>' }}{%- endif -%}",
        )
        .unwrap();
        let other = CString::new("{{ messages }} <|im_start|>").unwrap();
        let messages = [
            ChatMessage::new(Role::System, "Rules."),
            ChatMessage::new(Role::User, "Transcript."),
            ChatMessage::new(Role::Assistant, "{}"),
        ];
        // The stub stands in for `llama_chat_apply_template` and records what it was given.
        let seen: RefCell<Vec<(Vec<u8>, Vec<String>)>> = RefCell::new(Vec::new());
        let refuse = |t: &CStr, m: &[(CString, CString)]| -> Option<String> {
            seen.borrow_mut().push((
                t.to_bytes().to_vec(),
                m.iter()
                    .map(|(r, c)| format!("{}:{}", r.to_str().unwrap(), c.to_str().unwrap()))
                    .collect(),
            ));
            None
        };

        let prompt = super::format_chat_with(&gemma, &messages, refuse).unwrap();
        assert_eq!(
            prompt,
            super::chat_format::gemma4(&[
                ("system", "Rules."),
                ("user", "Transcript."),
                ("assistant", "{}"),
            ])
        );
        assert!(
            prompt.starts_with("<|turn>system\nRules.<turn|>\n"),
            "{prompt}"
        );
        // llama.cpp was asked first, with the model's template and the plain roles.
        assert_eq!(
            seen.borrow()[0],
            (
                gemma.to_bytes().to_vec(),
                vec![
                    "system:Rules.".to_string(),
                    "user:Transcript.".to_string(),
                    "assistant:{}".to_string()
                ]
            )
        );
        // A template llama.cpp formats is never re-formatted here.
        let llama = |_: &CStr, _: &[(CString, CString)]| Some("llama.cpp's prompt".to_string());
        assert_eq!(
            super::format_chat_with(&gemma, &messages, llama).unwrap(),
            "llama.cpp's prompt"
        );
        // A refused template without every Gemma 4 marker is an error, not a guess.
        assert!(matches!(
            super::format_chat_with(&other, &messages, refuse),
            Err(super::LlmError::Input(_))
        ));
        // A NUL byte fails before llama.cpp is asked.
        let before = seen.borrow().len();
        let nul = [ChatMessage::new(Role::User, "a\0b")];
        assert!(super::format_chat_with(&gemma, &nul, refuse).is_err());
        assert_eq!(seen.borrow().len(), before);
    }

    #[test]
    fn default_threads_is_bounded() {
        let t = super::default_threads();
        assert!((1..=8).contains(&t), "{t}");
    }

    #[test]
    fn prefix_reuse_keeps_whole_micro_batches_and_always_decodes_the_last_token() {
        use super::reusable_prefix;
        let seq: Vec<i32> = (0..2000).collect();
        // Nothing cached, or reuse off by a zero micro-batch: nothing kept.
        assert_eq!(reusable_prefix(&[], &seq, 512), 0);
        assert_eq!(reusable_prefix(&seq, &seq, 0), 0);
        // A shared start of 1300 tokens keeps two whole micro-batches.
        let mut other = seq.clone();
        other[1300] = -5;
        assert_eq!(reusable_prefix(&seq, &other, 512), 1024);
        // Shorter than one micro-batch: nothing.
        assert_eq!(reusable_prefix(&seq[..511], &seq, 512), 0);
        // The same prompt again: the last token is always decoded, so 1999 rounds to 1536.
        assert_eq!(reusable_prefix(&seq, &seq, 512), 1536);
        // A prompt of exactly 1024 shared tokens keeps 512, never all of it.
        assert_eq!(reusable_prefix(&seq, &seq[..1024], 512), 512);
        // A cache longer than the prompt (an earlier reply) changes nothing.
        let mut longer = seq.clone();
        longer.extend(5000..6000);
        assert_eq!(reusable_prefix(&longer, &seq[..1600], 512), 1536);
    }

    #[test]
    fn prefix_reuse_chunks_end_where_a_fresh_prompt_chunks_end() {
        use super::prompt_chunks;
        assert_eq!(
            prompt_chunks(0, 0, 2048),
            Vec::<std::ops::Range<usize>>::new()
        );
        // From 0: the fresh prompt's `chunks_mut(2048)`.
        assert_eq!(
            prompt_chunks(0, 5000, 2048),
            [0..2048, 2048..4096, 4096..5000]
        );
        // From a kept prefix: the first chunk ends on the same boundary.
        assert_eq!(
            prompt_chunks(1536, 5000, 2048),
            [1536..2048, 2048..4096, 4096..5000]
        );
        let one = |start, len| {
            let c = prompt_chunks(start, len, 2048);
            assert_eq!(c.len(), 1, "{c:?}");
            c[0].clone()
        };
        assert_eq!(one(2048, 3000), 2048..3000);
        assert_eq!(one(512, 900), 512..900);
        // Save points cut a chunk; nothing else moves.
        assert_eq!(
            super::decode_ranges(0, 5000, 2048, &[1536, 4608, 1536, 9000]),
            [0..1536, 1536..2048, 2048..4096, 4096..4608, 4608..5000]
        );
        // A save point at a chunk end or at the start cuts nothing.
        assert_eq!(
            super::decode_ranges(1024, 5000, 2048, &[1024, 2048]),
            [1024..2048, 2048..4096, 4096..5000]
        );
        // Every chunk boundary of a run from any start is one of the fresh run's.
        for start in [0, 512, 1024, 2560, 4608] {
            for r in prompt_chunks(start, 9000, 2048) {
                assert!(r.end == 9000 || r.end % 2048 == 0, "{start}: {r:?}");
            }
        }
    }
}
