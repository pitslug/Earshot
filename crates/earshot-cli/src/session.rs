//! The meeting driver shared by `replay` (and the live `record`): one meeting through the
//! runtime with the production backends, printed as it happens.
//!
//! 1. Before anything is recorded: the data directory and store are opened, the whisper
//!    model is located (a missing model names the `cargo xtask fetch-models` set), ONNX
//!    Runtime is initialised, the summary endpoint (if any) is allowlisted, and the export
//!    vault (if any) must exist. A bad setting fails here, with no meeting row. The speaker
//!    detection setting (T6.13d4) is `--speakers` if given (stored with `--save-speakers`),
//!    else the one stored in the data directory, else "during the call".
//! 2. The live session runs with whisper on the best verified accelerator per track
//!    ([`EvidenceLoader`]) and Silero VAD. Its events are printed: the backend of each
//!    track with the refused ones, AEC and clock stats (every [`STATS_EVERY`] of meeting
//!    time and the last one), discontinuities, warnings and live finals ("Me" for the
//!    mic, "Them" for the loopback).
//!    Echo of the far end is cleaned from the live "Me" finals with the canceller's lock;
//!    an isolated echo word may still show in this live draft.
//! 3. Unless `--no-post`: the final pass, diarisation and voiceprint naming
//!    ([`post_processor`]), and the backend that ran it. The final pass takes the live
//!    speech stretches where they are the same transcription and transcribes only what
//!    they do not cover (T6.13c; `--whole-pass` transcribes and diarises every track
//!    whole instead, for comparison); one line per track says which. It cleans echo
//!    again with the text rule and the speaker check: the stored transcript is the one
//!    that must be clean. The words removed (live and final) are printed.
//! 4. The summary. With `--llm-url` (OpenAI-compatible) or `--llm-provider anthropic`
//!    (Claude, key from `ANTHROPIC_API_KEY`): through `earshot-net` to that endpoint
//!    alone, every request in the audit log printed (`network: ...`). Otherwise on this
//!    machine (T6.21b): the local model of `--summary-model`, else the one this machine's
//!    memory picks (`earshot_runtime::summary_model`), when it is installed; a default
//!    model that is not installed is one line naming `cargo xtask fetch-models --set
//!    summary` and no summary, never a network endpoint. `--no-summary` skips it. A local
//!    summary needs llama.cpp's DLLs next to the exe (`cargo xtask build` puts them there;
//!    `llama.dll` is delay-loaded, so the CLI checks for them first and says so), and a
//!    `cuda` build also the CUDA runtime's cuBLAS DLLs on PATH (checked the same way). Without
//!    an endpoint the run makes no network request at all.
//! 5. The Obsidian export, only with `--export-vault` (export is on demand only).
//! 6. The transcript as stored (the final pass when it ran, else the live draft).

use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use clap::{Args, ValueEnum};
use earshot_core::asr::{BackendInfo, BackendKind, CancelToken, MeetingTime, TranscriptEvent};
use earshot_core::audio::PIPELINE_RATE;
use earshot_core::capture::TrackId;
use earshot_core::export::ExportOptions;
use earshot_core::identity::EntityId;
use earshot_core::model::{AudioFileRepo, MeetingRepo, TranscriptPass};
use earshot_core::pipeline::{AecStats, PipelineStats};
use earshot_llm::{ANTHROPIC_API_KEY_ENV, ANTHROPIC_DEFAULT_MODEL, ANTHROPIC_DEFAULT_URL, ApiKey};
use earshot_runtime::backends::{AsrModel, ModelDir, accelerator_order, asr_model};
use earshot_runtime::echo::{EchoLock, EchoReport};
use earshot_runtime::host::{
    self, AfterMeeting, HostPost, HostStep, ModelSource, SkipReason, SummaryPlanError,
    SummaryRequest, SummaryResult, SummarySetupError, TimingSource, after_meeting, init_onnx_for,
    live_factory, prepare_summary, skip_reason, start_meeting,
};
pub use earshot_runtime::host::{FALLBACK_MODEL, ModelChoice, Starter, SummaryPlan, model_dir};
// The DLL checks moved to the runtime (T7.2a); the CLI's tests still reach them here.
#[cfg(test)]
pub use earshot_runtime::host::{
    CUDA_RUNTIME_DLL_PREFIXES, LLAMA_DLLS, missing_dll_prefixes, missing_llama_dlls,
};
use earshot_runtime::live::{LiveConfig, LiveSession, RuntimeEvent, RuntimeWarning};
use earshot_runtime::post::{
    CleanupWork, PostOutcome, PostStage, PostWarning, SpeakerWindowUse, StretchUse,
};
use earshot_runtime::speaker_timing::{AFTER_CALL_WARNING, SpeakerTiming};
use earshot_runtime::summary::{SummaryConfig, SummaryRunReport, SummaryService};
use earshot_runtime::summary_model::{SUMMARY_MODELS, summary_gpu_planned};
use earshot_runtime::view::MeetingView;
use earshot_runtime::{RuntimeError, Services, SharedClock, SystemClock, os_seeded_rng};
use earshot_store::{DataDir, Store};

use crate::events::EventLog;
use crate::transcribe::format_time;

// Used only by the tests below (they were in scope here before T7.2a moved the host logic
// to `earshot_runtime::host`).
#[cfg(test)]
use earshot_runtime::backends::{Accelerator, SILERO};
#[cfg(test)]
use earshot_runtime::live::LiveOutcome;
#[cfg(test)]
use earshot_runtime::model_bench::{check_fingerprint, hardware_facts};
#[cfg(test)]
use earshot_runtime::speaker_timing::speaker_timing;

/// Stats are printed at most once per this much meeting time (and once at the end).
pub const STATS_EVERY: Duration = Duration::from_secs(5);

/// How often the driver checks its stop condition while waiting for events.
const EVENT_WAIT: Duration = Duration::from_millis(100);

/// Options shared by `replay` and `record`.
#[derive(Debug, Clone, Args)]
pub struct MeetingOptions {
    /// Earshot data directory for this run (database and recordings; created if missing).
    #[arg(long)]
    pub out: PathBuf,
    /// Transcription model for the live and final passes: a whisper model (tiny.en,
    /// base.en, small.en, small.en-q5_1, large-v3-turbo, large-v3-turbo-q5_0) or a Parakeet
    /// model (parakeet-ctc-110m, parakeet-tdt-110m, parakeet-tdt-0.6b-v2,
    /// parakeet-tdt-0.6b-v3; `cargo xtask fetch-models --set ci` fetches the defaults,
    /// `--set choice` and `--set eval` the others). Default: the model
    /// `earshot-cli benchmark` recorded in this data directory when it is current for this
    /// machine, else base.en. Models come from EARSHOT_MODELS (default
    /// %LOCALAPPDATA%\Earshot\models).
    #[arg(long)]
    pub model: Option<String>,
    /// Meeting title (default: none; a summary may set it).
    #[arg(long, default_value = "")]
    pub title: String,
    /// Skip the post-meeting pass (final transcript, diarisation, voiceprints).
    #[arg(long)]
    pub no_post: bool,
    /// Summary provider: openai (any OpenAI-compatible endpoint; needs --llm-url and
    /// --llm-model), anthropic (Claude with your own key, read from the
    /// ANTHROPIC_API_KEY environment variable, never from the command line) or claude-code
    /// (Claude through your installed Claude Code on its Claude.ai sign-in; the prompt text is
    /// handed to Claude Code, which sends it to Anthropic; --llm-model picks its model, for
    /// example opus) or codex (OpenAI through your installed Codex on its ChatGPT sign-in; the
    /// prompt text is handed to Codex, which sends it to OpenAI; --llm-model picks its model).
    /// Without
    /// --llm-url or --llm-provider the summary is made on this machine (--summary-model).
    #[arg(long, value_enum, value_name = "PROVIDER")]
    pub llm_provider: Option<LlmProviderArg>,
    /// Summary endpoint: the OpenAI-compatible base URL (for example
    /// http://127.0.0.1:8080/v1), or with --llm-provider anthropic the API root (default
    /// https://api.anthropic.com). The only host contacted, and only the transcript text
    /// is sent.
    #[arg(long, value_name = "URL")]
    pub llm_url: Option<String>,
    /// Model name for the summary endpoint (anthropic default: claude-sonnet-5-5).
    #[arg(long, value_name = "NAME")]
    pub llm_model: Option<String>,
    /// Make no summary (no local model is loaded and nothing is sent anywhere).
    #[arg(long, conflicts_with_all = ["llm_provider", "llm_url", "llm_model", "summary_model"])]
    pub no_summary: bool,
    /// The local summary model, used when no --llm-url or --llm-provider is given:
    /// qwen3.5-9b-q4_k_m or qwen3.5-4b-q4_k_m (`cargo xtask fetch-models --set summary`).
    /// Default: the 9B on machines that report more than 12 GiB of memory (a 16 GB machine
    /// does), else the 4B. Without the default model installed, no summary is made.
    #[arg(long, value_name = "ID", conflicts_with_all = ["llm_provider", "llm_url", "llm_model"])]
    pub summary_model: Option<String>,
    /// Export the meeting note into this Obsidian vault after processing (never without
    /// this option).
    #[arg(long, value_name = "DIR")]
    pub export_vault: Option<PathBuf>,
    /// Write the summary input (what a summary request would be built from: meeting
    /// details and the stored transcript with segment ids and speaker names, JSON) to this
    /// file after processing. A local file only; used by `cargo xtask sum-eval`.
    #[arg(long, value_name = "FILE")]
    pub summary_input: Option<PathBuf>,
    /// When speakers are detected: `during` the call (the default), `after` the call, or
    /// `after-on-battery` (laptops: during the call on mains power, after it on battery;
    /// a machine without a battery treats it as `during`). After the call: the final
    /// transcript and summary take longer after hang-up, but no cores are used for speaker
    /// detection during the meeting. Overrides the setting stored in --out for this run.
    #[arg(long, value_enum, value_name = "WHEN")]
    pub speakers: Option<SpeakersArg>,
    /// Store the --speakers value as the setting in --out, for later runs without it.
    #[arg(long, requires = "speakers")]
    pub save_speakers: bool,
    /// Summarise during the meeting: `on` or `off` (the default). Each finished 10-minute
    /// window is summarised by the local summary model while the meeting runs, so only the
    /// last window and the merge are left at hang-up; the summary is ready sooner but may
    /// miss some points. Needs a local summary model; off on battery with
    /// `--speakers after-on-battery`. Overrides the setting stored in --out for this run.
    #[arg(long, value_enum, value_name = "ON_OFF")]
    pub live_summaries: Option<OnOff>,
    /// Store the --live-summaries value as the setting in --out, for later runs without it.
    #[arg(long, requires = "live_summaries")]
    pub save_live_summaries: bool,
    /// Cleanup: ignore the speech stretches and speaker windows computed during the meeting
    /// and transcribe and diarise every track whole with the same models (the fallback
    /// pass), for comparing the two (T6.13). Off by default.
    #[arg(long, conflicts_with = "no_post")]
    pub whole_pass: bool,
}

/// `--live-summaries` values.
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum OnOff {
    /// On.
    On,
    /// Off.
    Off,
}

/// The live summaries setting for this run (T8.3j): `flag` (`--live-summaries`) if given,
/// stored as the setting with `save`; else the setting stored in `store`; else off. Prints
/// which.
///
/// # Errors
/// Store or output errors.
pub fn choose_live_summaries(
    store: &mut Store,
    flag: Option<bool>,
    save: bool,
    out: &mut dyn Write,
) -> Result<bool> {
    let (on, source) = host::choose_live_summaries(store, flag, save).map_err(runtime)?;
    let source = match source {
        TimingSource::RequestedAndSaved => "--live-summaries, saved as the setting".to_owned(),
        TimingSource::Requested => "--live-summaries, for this run".to_owned(),
        TimingSource::Stored => "stored setting".to_owned(),
        TimingSource::Default => "default".to_owned(),
        TimingSource::UnknownStored(name) => {
            format!("off: the stored setting {name:?} is not known to this build")
        }
    };
    writeln!(
        out,
        "summarise during the meeting: {} ({source})",
        if on { "on" } else { "off" }
    )?;
    Ok(on)
}

/// `--speakers` values ([`SpeakerTiming`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum SpeakersArg {
    /// During the call.
    During,
    /// After the call.
    After,
    /// After the call on battery, during it on mains power.
    AfterOnBattery,
}

impl From<SpeakersArg> for SpeakerTiming {
    fn from(a: SpeakersArg) -> Self {
        match a {
            SpeakersArg::During => SpeakerTiming::DuringCall,
            SpeakersArg::After => SpeakerTiming::AfterCall,
            SpeakersArg::AfterOnBattery => SpeakerTiming::AfterCallOnBattery,
        }
    }
}

/// The speaker detection setting for this run (T6.13d4): `flag` (`--speakers`) if given,
/// stored as the setting with `save`; else the setting stored in `store`; else the
/// default. Prints which, and the warning for "after the call".
///
/// # Errors
/// Store or output errors.
pub fn choose_speaker_timing(
    store: &mut Store,
    flag: Option<SpeakerTiming>,
    save: bool,
    out: &mut dyn Write,
) -> Result<SpeakerTiming> {
    let (timing, source) = host::choose_speaker_timing(store, flag, save).map_err(runtime)?;
    let source = match source {
        TimingSource::RequestedAndSaved => "--speakers, saved as the setting".to_owned(),
        TimingSource::Requested => "--speakers, for this run".to_owned(),
        TimingSource::Stored => "stored setting".to_owned(),
        TimingSource::Default => "default".to_owned(),
        TimingSource::UnknownStored(name) => {
            format!("default: the stored setting {name:?} is not known to this build")
        }
    };
    writeln!(out, "speakers: {} ({source})", timing.label())?;
    if timing == SpeakerTiming::AfterCall {
        writeln!(out, "  {AFTER_CALL_WARNING}")?;
    }
    Ok(timing)
}

/// `--llm-provider` values.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum LlmProviderArg {
    /// An OpenAI-compatible endpoint (`--llm-url` and `--llm-model`).
    Openai,
    /// The Anthropic Messages API (Claude), key from `ANTHROPIC_API_KEY`.
    Anthropic,
    /// Claude through the installed Claude Code CLI (T7.4ga), model from `--llm-model`.
    ClaudeCode,
    /// OpenAI through the installed Codex CLI (T7.4gc), model from `--llm-model`.
    Codex,
}

/// The summary settings of `opts`, or `None` for no summary (and no network). `env`
/// looks up an environment variable; the Anthropic key is read only from
/// [`ANTHROPIC_API_KEY_ENV`], never from an argument (it would land in shell history).
///
/// # Errors
/// OpenAI-compatible: only one of `--llm-url` and `--llm-model`, or
/// `--llm-provider openai` without them. Anthropic: no key in the environment.
pub fn summary_config(
    opts: &MeetingOptions,
    env: impl Fn(&str) -> Option<String>,
) -> Result<Option<SummaryConfig>> {
    match opts.llm_provider {
        None | Some(LlmProviderArg::Openai) => match (&opts.llm_url, &opts.llm_model) {
            (Some(url), Some(model)) => Ok(Some(SummaryConfig::new(url, model))),
            (None, None) if opts.llm_provider.is_none() => Ok(None),
            _ => bail!("--llm-url and --llm-model go together"),
        },
        Some(LlmProviderArg::Anthropic) => {
            let Some(key) = env(ANTHROPIC_API_KEY_ENV).and_then(ApiKey::new) else {
                bail!(
                    "--llm-provider anthropic needs your API key in the {ANTHROPIC_API_KEY_ENV} \
                     environment variable (it is never taken from the command line)"
                );
            };
            Ok(Some(SummaryConfig::anthropic(
                opts.llm_url.as_deref().unwrap_or(ANTHROPIC_DEFAULT_URL),
                opts.llm_model.as_deref().unwrap_or(ANTHROPIC_DEFAULT_MODEL),
                Some(key),
            )))
        }
        // No endpoint: see [`claude_code_config`].
        Some(LlmProviderArg::ClaudeCode | LlmProviderArg::Codex) => Ok(None),
    }
}

/// The Claude Code summary of `--llm-provider claude-code` (`--llm-model` picks the model,
/// none: Claude Code's default), `None` for any other provider.
///
/// # Errors
/// `--llm-url` (Claude Code has no endpoint); a model name Claude Code cannot take.
pub fn claude_code_config(
    opts: &MeetingOptions,
) -> Result<Option<earshot_runtime::claude_code::ClaudeCodeSummaryConfig>> {
    if opts.llm_provider != Some(LlmProviderArg::ClaudeCode) {
        return Ok(None);
    }
    if opts.llm_url.is_some() {
        bail!("--llm-provider claude-code takes no --llm-url (Claude Code connects by itself)");
    }
    if let Some(m) = &opts.llm_model {
        earshot_runtime::claude_code::check_model(m)
            .map_err(|why| anyhow::anyhow!("--llm-model cannot be passed to Claude Code: {why}"))?;
    }
    Ok(Some(
        earshot_runtime::claude_code::ClaudeCodeSummaryConfig {
            model: opts.llm_model.clone(),
            ..Default::default()
        },
    ))
}

/// The Codex summary of `--llm-provider codex` (`--llm-model` picks the model, none: Codex's
/// default), `None` for any other provider.
///
/// # Errors
/// `--llm-url` (Codex has no endpoint); a model name Codex cannot take.
pub fn codex_config(
    opts: &MeetingOptions,
) -> Result<Option<earshot_runtime::codex::CodexSummaryConfig>> {
    if opts.llm_provider != Some(LlmProviderArg::Codex) {
        return Ok(None);
    }
    if opts.llm_url.is_some() {
        bail!("--llm-provider codex takes no --llm-url (Codex connects by itself)");
    }
    if let Some(m) = &opts.llm_model {
        earshot_runtime::codex::check_model(m)
            .map_err(|why| anyhow::anyhow!("--llm-model cannot be passed to Codex: {why}"))?;
    }
    Ok(Some(earshot_runtime::codex::CodexSummaryConfig {
        model: opts.llm_model.clone(),
        ..Default::default()
    }))
}

/// The summary step of `opts` (module docs, step 4; [`host::summary_plan`]): the endpoint
/// of [`summary_config`] when one is given; else, unless `--no-summary`, the local model of
/// `--summary-model` or the one `memory` (reported physical bytes) picks, if installed in
/// `models`.
///
/// # Errors
/// As [`summary_config`]; an unknown `--summary-model`; a `--summary-model` that is not
/// installed (it was asked for by name).
pub fn summary_plan(
    opts: &MeetingOptions,
    env: impl Fn(&str) -> Option<String>,
    memory: Option<u64>,
    gpu: bool,
    models: &ModelDir,
) -> Result<SummaryPlan> {
    let request = SummaryRequest {
        remote: summary_config(opts, env)?,
        skip: opts.no_summary,
        local_model: opts.summary_model.clone(),
        // The CLI does not read the hardware for this: a model too big for the card is refused
        // by the load and retried on the CPU.
        graphics_memory: None,
        // The CLI's key comes from ANTHROPIC_API_KEY, checked in `summary_config`.
        no_key: None,
        claude_code: claude_code_config(opts)?,
        codex: codex_config(opts)?,
        // The CLI summarises only the meeting it has just recorded, never one kept on this
        // computer (T12.13: asked only on the app's new-meeting screen); `SummaryService`
        // refuses a remote service for a kept meeting anyway.
        kept_local: false,
    };
    host::summary_plan(&request, memory, gpu, models).map_err(|e| match e {
        SummaryPlanError::UnknownModel(e) => anyhow::anyhow!(
            "--summary-model: {e} (shipped local summary models: {})",
            SUMMARY_MODELS
                .iter()
                .map(|m| m.id)
                .collect::<Vec<_>>()
                .join(", ")
        ),
        SummaryPlanError::NotInstalled(e) => anyhow::anyhow!("--summary-model: {e}"),
    })
}

/// The lines for a plan that runs no summary (`--no-summary`, or no local model
/// installed): the one printed before the meeting, and the reason in the closing
/// `network: none (..)` line. `None` for a plan that runs a summary.
#[must_use]
pub fn no_summary_lines(plan: &SummaryPlan) -> Option<(String, &'static str)> {
    match plan {
        SummaryPlan::Skip => Some(("summary: skipped (--no-summary)".into(), "summary skipped")),
        // `why` already names the `cargo xtask fetch-models --set summary` command.
        SummaryPlan::NotInstalled(choice, why) => Some((
            format!(
                "summary: none, no local summary model is installed ({}: {why})",
                choice.model.id
            ),
            "no local summary model installed",
        )),
        // The CLI reads its key from the environment and never plans this; told anyway.
        SummaryPlan::NoApiKey(e) => Some((format!("summary: none ({e})"), "no API key")),
        SummaryPlan::ClaudeCodeUnavailable(e) => Some((
            format!("summary: none ({e}; open Claude Code and sign in, or install it)"),
            "Claude Code cannot run",
        )),
        SummaryPlan::CodexUnavailable(e) => Some((
            format!("summary: none ({e}; sign in to Codex with `codex login`, or install it)"),
            "Codex cannot run",
        )),
        // The CLI never plans this (`kept_local: false` above); told anyway.
        SummaryPlan::KeptLocalNoModel(choice) => Some((
            format!(
                "summary: none, the meeting is kept on this computer and its local summary model is not installed ({})",
                choice.model.id
            ),
            "kept on this computer, no local summary model",
        )),
        SummaryPlan::Remote(_)
        | SummaryPlan::Local(_)
        | SummaryPlan::ClaudeCode(_)
        | SummaryPlan::Codex(_) => None,
    }
}

/// The speaker label of a live track.
#[must_use]
pub fn speaker(track: TrackId) -> &'static str {
    match track {
        TrackId::Mic => "Me",
        TrackId::Reference => "Them",
    }
}

fn track_name(track: TrackId) -> &'static str {
    match track {
        TrackId::Mic => "mic",
        TrackId::Reference => "reference",
    }
}

fn at(t: MeetingTime) -> String {
    format!("[{}]", format_time(t.as_millis()))
}

/// One line for a backend: kind, device and model.
#[must_use]
pub fn format_backend(b: &BackendInfo) -> String {
    format!("{:?} ({}, {})", b.kind, b.device, b.model)
}

fn format_aec(aec: Option<&AecStats>) -> String {
    let Some(a) = aec else {
        return "aec off (no reference)".into();
    };
    let mut s = format!("aec {}", if a.locked { "locked" } else { "searching" });
    if let Some(d) = a.delay_ms {
        s += &format!(" delay {d:.1} ms");
    }
    if let Some(att) = a.attenuation_db {
        s += &format!(" attenuation {att:.1} dB");
    }
    for (on, name) in [
        (a.converged, "converged"),
        (a.double_talk, "double-talk"),
        (a.bypass, "bypass"),
    ] {
        if on {
            s += " ";
            s += name;
        }
    }
    if let Some(drift) = a.residual_drift_ppm {
        s += &format!(" residual drift {drift:.1} ppm");
    }
    s
}

/// One line of pipeline stats: AEC state, clock skews and steering, ASR backlog, intake
/// repairs.
#[must_use]
pub fn format_stats(stats: &PipelineStats) -> String {
    let secs = |samples: u64| samples as f64 / f64::from(PIPELINE_RATE.hz());
    let mut s = format!(
        "{} {} | skew mic {:.1} ms",
        at(stats.end),
        format_aec(stats.aec.as_ref()),
        stats.mic.skew_ms
    );
    if let Some(r) = &stats.reference {
        s += &format!(
            ", reference {:.1} ms, steering {:.1} ppm",
            r.skew_ms, stats.reference_correction_ppm
        );
    }
    s += &format!(" | asr backlog mic {:.1} s", secs(stats.mic.asr_backlog));
    if let Some(r) = &stats.reference {
        s += &format!(", reference {:.1} s", secs(r.asr_backlog));
    }
    let tracks = std::iter::once((TrackId::Mic, &stats.mic))
        .chain(stats.reference.as_ref().map(|r| (TrackId::Reference, r)));
    for (track, t) in tracks {
        let repaired = t.non_finite + t.clamped + t.partial_frames + t.late_dropped;
        if repaired + t.restarts + t.muted_frames > 0 {
            s += &format!(
                " | {}: {} restarts, {} late dropped, {} muted frames, {} repaired samples",
                track_name(track),
                t.restarts,
                t.late_dropped,
                t.muted_frames,
                t.non_finite + t.clamped + t.partial_frames
            );
        }
    }
    s
}

pub(crate) fn format_warning(w: &RuntimeWarning) -> String {
    match w {
        RuntimeWarning::Pipeline(e) => format!("warning: pipeline: {e:?}"),
        RuntimeWarning::Capture(m) => format!("warning: capture: {m}"),
        RuntimeWarning::FarEndSilent => "warning: the mic is speaking but the far end's track \
             has heard nothing; check that the call plays through the captured output"
            .to_owned(),
        RuntimeWarning::FarEndHeard => "the far end's track now hears sound".to_owned(),
        RuntimeWarning::RecordingGap { total } => format!(
            "warning: the mic delivered no audio for {:.1} s in total (did the computer sleep?); \
             speech in that time is lost",
            total.as_secs_f64()
        ),
        RuntimeWarning::CaptureRepeated { message, times } => {
            format!("warning: capture: {message} (repeated {times} more times)")
        }
        RuntimeWarning::DeviceUnavailable { track } => format!(
            "warning: {} device unavailable; recording silence",
            track_name(*track)
        ),
        RuntimeWarning::DeviceRestored { track, device_name } => {
            format!(
                "warning: {} device restored: {device_name}",
                track_name(*track)
            )
        }
        RuntimeWarning::Recording { track, error } => {
            format!("warning: {} recording failed: {error}", track_name(*track))
        }
        RuntimeWarning::RecordingStopped {
            track,
            at,
            disk_full,
            error,
        } => {
            let why = if *disk_full {
                "the disk is full".to_owned()
            } else {
                error.clone()
            };
            format!(
                "warning: {} recording stopped at {:.1} s ({why}); the audio before it is kept",
                track_name(*track),
                at.as_millis() as f64 / 1000.0
            )
        }
        RuntimeWarning::EveryRecordingStopped { disk_full } => format!(
            "warning: every track stopped recording{}, so the meeting was stopped",
            if *disk_full {
                " because the disk is full"
            } else {
                ""
            }
        ),
        RuntimeWarning::LiveCatchUpStalled { track } => format!(
            "warning: the {} live transcript stopped answering after the recording and was left",
            track_name(*track)
        ),
        RuntimeWarning::DeviceStoppedAnswering => "warning: the audio device stopped answering \
             when capture was stopped; the recording was saved without it"
            .to_owned(),
        RuntimeWarning::Persistence(m) => format!("warning: storing failed: {m}"),
        RuntimeWarning::LiveLoadFailed { error } => {
            format!("warning: live transcription did not start: {error}")
        }
        RuntimeWarning::HostNote { message, .. } => format!("note: {message}"),
        RuntimeWarning::BackendFallback {
            track,
            backend,
            refused,
        } => {
            let refused: Vec<String> = refused.iter().map(|(k, e)| format!("{k:?}: {e}")).collect();
            format!(
                "warning: {} live asr runs on {backend:?}; refused {}",
                track_name(*track),
                refused.join("; ")
            )
        }
        RuntimeWarning::BluetoothHandsFreeMic {
            device_name,
            sample_rate,
        } => {
            let rate = sample_rate.map_or(String::new(), |r| {
                format!(" ({} kHz)", f64::from(r) / 1000.0)
            });
            format!(
                "warning: the mic {device_name:?} is a Bluetooth headset in hands-free mode{rate}: \
                 Windows records it at phone quality and lowers the headset's playback while it \
                 is open. Use the computer's own mic and keep the headset for listening."
            )
        }
        RuntimeWarning::LiveCutAtStop { track, behind } => format!(
            "warning: {} live transcript stopped {:.0} s behind at the end; the cleanup pass covers the rest",
            track_name(*track),
            behind.as_secs_f64()
        ),
        RuntimeWarning::CallOutputRefused { wanted, used } => format!(
            "warning: the call plays on {wanted:?}, which could not be opened; still recording {}",
            used.as_deref().unwrap_or("no output")
        ),
        // The CLI records the default devices (only the app chooses endpoints, T7.4i).
        RuntimeWarning::ChosenDeviceGivenUp {
            track,
            chosen,
            used,
        } => format!(
            "warning: {} device {chosen:?} was lost; recording the default {used:?} for the rest of the meeting",
            track_name(*track)
        ),
        RuntimeWarning::ChosenDeviceUnavailable {
            track,
            chosen,
            used,
            error,
        } => format!(
            "warning: {} device {chosen:?} {}; recording the default {used:?}",
            track_name(*track),
            error.as_ref().map_or_else(
                || "is not connected".to_owned(),
                |e| format!("could not be opened ({e})")
            )
        ),
    }
}

/// The printed line(s) for an event; `None` for level meters, partials and stats (stats
/// go through [`StatsPrinter`]).
#[must_use]
pub fn format_event(event: &RuntimeEvent) -> Option<String> {
    match event {
        RuntimeEvent::Started { meeting, tracks } => Some(format!(
            "meeting {meeting}: recording {}",
            tracks
                .iter()
                .map(|(t, _)| track_name(*t))
                .collect::<Vec<_>>()
                .join(" + ")
        )),
        RuntimeEvent::LiveBackend {
            track,
            backend,
            fallbacks,
        } => {
            let mut s = format!(
                "live asr {}: {}",
                track_name(*track),
                format_backend(backend)
            );
            for (kind, e) in fallbacks {
                s += &format!("\n  refused {kind:?}: {e}");
            }
            Some(s)
        }
        RuntimeEvent::Levels(_)
        | RuntimeEvent::Stats(_)
        | RuntimeEvent::VoiceActivity { .. }
        | RuntimeEvent::TranscriptLag { .. }
        | RuntimeEvent::RecordingSaved { .. }
        | RuntimeEvent::LiveTranscript(_)
        | RuntimeEvent::LivePreview { .. }
        | RuntimeEvent::LivePreviewReport(_)
        | RuntimeEvent::LiveTranscriptFinished { .. } => None,
        RuntimeEvent::Transcript { track, event } => match event {
            TranscriptEvent::Final(seg) => Some(format!(
                "{} {}: {}",
                at(seg.start),
                speaker(*track),
                seg.text
            )),
            TranscriptEvent::Partial(_) | TranscriptEvent::Discarded { .. } => None,
        },
        RuntimeEvent::Discontinuity { track, at: t, kind } => Some(format!(
            "{} {}: discontinuity {kind:?}",
            at(*t),
            track_name(*track)
        )),
        RuntimeEvent::LivePartials {
            track,
            enabled,
            behind,
        } => Some(if *enabled {
            format!(
                "live asr {}: caught up ({:.1} s behind), partials back on",
                track_name(*track),
                behind.as_secs_f64()
            )
        } else {
            format!(
                "live asr {}: {:.1} s behind, partials off until it catches up",
                track_name(*track),
                behind.as_secs_f64()
            )
        }),
        RuntimeEvent::Resynced {
            mic_skew_ms,
            track_offset_ms,
        } => Some(match track_offset_ms {
            Some(o) => format!(
                "re-synced with the device clock (mic skew {mic_skew_ms:.1} ms, reference offset {o:.1} ms)"
            ),
            None => format!("re-synced with the device clock (mic skew {mic_skew_ms:.1} ms)"),
        }),
        RuntimeEvent::LiveSpeakersOff { reason } => Some(format!(
            "speakers: {reason}; the cleanup finds them after the call"
        )),
        RuntimeEvent::LiveSummariesOff { reason } => Some(format!(
            "summary: {reason}; the summary after the call does every window"
        )),
        RuntimeEvent::LiveSummaryMapped {
            window,
            took,
            error,
        } => Some(match error {
            None => format!(
                "summary: window {window} summarised during the meeting ({:.1} s)",
                took.as_secs_f64()
            ),
            Some(e) => format!(
                "summary: window {window} could not be summarised during the meeting ({e}); the summary after the call does it"
            ),
        }),
        RuntimeEvent::Warning(w) => Some(format_warning(w)),
        RuntimeEvent::Ended(o) => {
            let mut s = format!(
                "{} ended ({:?}); {} recording(s) stored",
                at(o.end),
                o.reason,
                o.audio_files.len()
            );
            s += &if o.echo_locks.is_empty() {
                "\n  echo: no canceller lock, nothing removed".to_owned()
            } else {
                format!(
                    "\n  echo: {} live Me word(s) removed ({})",
                    o.echo_words_removed,
                    format_locks(&o.echo_locks)
                )
            };
            for t in &o.unavailable_at_end {
                s += &format!("\n  {} device unavailable at the end", track_name(*t));
            }
            for p in &o.problems {
                s += &format!("\n  {}", format_warning(p));
            }
            Some(s)
        }
    }
}

/// Prints stats at most once per [`STATS_EVERY`] of meeting time, and the last snapshot
/// at the end if it was not printed.
#[derive(Debug, Default)]
pub struct StatsPrinter {
    next_ms: u64,
    pending: Option<PipelineStats>,
}

impl StatsPrinter {
    /// The line to print for `stats` now, if any.
    pub fn push(&mut self, stats: &PipelineStats) -> Option<String> {
        let ms = stats.end.as_millis();
        if ms >= self.next_ms {
            self.next_ms = ms + STATS_EVERY.as_millis() as u64;
            self.pending = None;
            Some(format_stats(stats))
        } else {
            self.pending = Some(stats.clone());
            None
        }
    }

    /// The last snapshot, if it was held back.
    pub fn finish(&mut self) -> Option<String> {
        self.pending.take().map(|s| format_stats(&s))
    }
}

fn print_event(
    out: &mut dyn Write,
    stats: &mut StatsPrinter,
    event: &RuntimeEvent,
    log: Option<&EventLog>,
) -> Result<()> {
    if let RuntimeEvent::Ended(_) = event
        && let Some(line) = stats.finish()
    {
        writeln!(out, "{line}")?;
    }
    if let Some(log) = log {
        log.event(event);
    }
    let line = match event {
        RuntimeEvent::Stats(s) => {
            let line = stats.push(s);
            if let (Some(log), Some(_)) = (log, &line) {
                log.stats(s);
            }
            line
        }
        other => format_event(other),
    };
    if let Some(line) = line {
        writeln!(out, "{line}")?;
    }
    Ok(())
}

/// The transcription model of a run ([`host::choose_model`]): `requested`, else the model
/// check recorded in `data` when it is current for this machine (T6.10c), else
/// [`FALLBACK_MODEL`]; says which on `out` (nothing for a requested model).
///
/// # Errors
/// The database cannot be opened or its model check does not parse; output errors.
pub fn choose_model(
    requested: Option<&str>,
    data: &DataDir,
    models: &ModelDir,
    out: &mut dyn Write,
) -> Result<ModelChoice> {
    let store = Store::open(&data.db_path())
        .with_context(|| format!("cannot open the database in {}", data.root().display()))?;
    let (choice, source) =
        host::choose_model(requested, &store, models, host::DEFAULT_LANGUAGES).map_err(runtime)?;
    match source {
        ModelSource::Requested => {}
        ModelSource::ModelCheck => {
            // The check always records the backend it kept.
            if let Some(backend) = choice.backend {
                writeln!(
                    out,
                    "model: {} on {backend:?} (from the model check)",
                    choice.model
                )?;
            }
        }
        ModelSource::Fallback(why) => writeln!(
            out,
            "model: {FALLBACK_MODEL} ({why}; run `earshot-cli benchmark --out {}`)",
            data.root().display()
        )?,
    }
    Ok(choice)
}

fn runtime(e: RuntimeError) -> anyhow::Error {
    anyhow::anyhow!("{e}")
}

/// Fails (with the fetch instruction) unless every model the run will load is present
/// ([`host::require_models`]). Checked before recording, so a missing model never costs a
/// recorded meeting its post pass.
///
/// # Errors
/// An unknown model name, or the first missing model.
pub fn require_models(models: &ModelDir, model: &str, post: bool) -> Result<()> {
    host::require_models(models, model, post).map_err(runtime)
}

/// Prints the session's tracks and events until it ends, requesting the stop as soon as
/// `should_stop` says so (see [`pump_events`]).
fn follow_session(
    svc: &Services,
    session: &mut LiveSession,
    should_stop: &dyn Fn() -> bool,
    log: Option<&EventLog>,
    out: &mut dyn Write,
) -> Result<()> {
    let events = session
        .take_events()
        .context("the session's events were already taken")?;
    {
        let db = svc.store();
        for t in db.tracks(session.meeting())? {
            if let Some(device) = &t.device_name {
                writeln!(out, "{} device: {device}", track_name(t.kind))?;
            }
        }
    }
    pump_events(&events, should_stop, &|| session.request_stop(), log, out)
}

/// Prints events until [`RuntimeEvent::Ended`] (or the stream closes). `should_stop` is
/// checked before every event and every wait, so a stop request is seen within one event
/// or [`EVENT_WAIT`] however busy the stream is; the first time it is true,
/// `request_stop` runs (once) and the events keep being printed until the end, after the
/// recording is finalised.
///
/// # Errors
/// Output errors.
pub fn pump_events(
    events: &Receiver<RuntimeEvent>,
    should_stop: &dyn Fn() -> bool,
    request_stop: &dyn Fn(),
    log: Option<&EventLog>,
    out: &mut dyn Write,
) -> Result<()> {
    let mut stats = StatsPrinter::default();
    let mut stop_sent = false;
    loop {
        if !stop_sent && should_stop() {
            writeln!(out, "stopping (finalising the recording)")?;
            request_stop();
            stop_sent = true;
        }
        match events.recv_timeout(EVENT_WAIT) {
            Ok(event) => {
                print_event(out, &mut stats, &event, log)?;
                if let RuntimeEvent::Ended(_) = event {
                    return Ok(());
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return Ok(()),
        }
    }
}

/// Runs one meeting (module docs). `should_stop` is polled while the session runs; when
/// it returns true the session is stopped (a replay also ends by itself). Every event and
/// cleanup step is also written to `log` when given (`--events-json`).
///
/// # Errors
/// Setup failures (before any row is written), a session that cannot start, a post
/// pass, summary or export that fails, or output errors.
pub fn run_meeting(
    opts: &MeetingOptions,
    mut config: LiveConfig,
    start: Starter,
    should_stop: &dyn Fn() -> bool,
    cleanup: Option<&CleanupInterrupt>,
    log: Option<&EventLog>,
    out: &mut dyn Write,
) -> Result<EntityId> {
    // 1. Everything that can be checked before recording.
    let data = DataDir::new(&opts.out);
    data.create()
        .with_context(|| format!("cannot create {}", opts.out.display()))?;
    let models = model_dir(&data);
    let choice = choose_model(opts.model.as_deref(), &data, &models, out)?;
    let model = choice.model.clone();
    require_models(&models, &model, !opts.no_post)?;
    let clock: SharedClock = Arc::new(SystemClock);
    let plan = summary_plan(
        opts,
        |name| std::env::var(name).ok(),
        earshot_store::total_memory(),
        summary_gpu_planned(&accelerator_order()),
        &models,
    )?;
    // Claude Code: installed and signed in? (Never a sign-in; no other provider instead.)
    let plan = host::check_claude_code(
        plan,
        &earshot_runtime::claude_code::InstalledClaudeCode::meeting_start(),
    );
    // Codex: the same check (T7.4gc).
    let plan = host::check_codex(
        plan,
        &earshot_runtime::codex::InstalledCodex::meeting_start(),
    );
    // The reason in the closing `network: none (..)` line when no summary runs.
    let mut no_summary_network = "";
    if let Some((line, network)) = no_summary_lines(&plan) {
        writeln!(out, "{line}")?;
        no_summary_network = network;
    }
    let summary = match prepare_summary(
        &plan,
        &models,
        Arc::clone(&clock),
        &earshot_runtime::host::dll_search_dirs(),
    ) {
        Ok(s) => s,
        Err(SummarySetupError::LlamaDllsMissing(missing)) => bail!(
            "a local summary needs llama.cpp's DLLs next to earshot-cli.exe; not found: {} (build with `cargo xtask build`, which copies them, or pass --no-summary)",
            missing.join(", ")
        ),
        Err(SummarySetupError::CudaDllsMissing(missing)) => bail!(
            "a local summary in this CUDA build needs the CUDA runtime's DLLs on PATH; not found: {} (install the CUDA toolkit or runtime this build was made with, or pass --no-summary)",
            missing.join(", ")
        ),
        Err(SummarySetupError::Runtime(e)) => return Err(runtime(e)),
    };
    if let Some((choice, gpu)) = summary.as_ref().and_then(|s| s.local.as_ref()) {
        writeln!(
            out,
            "summary: local model {}, requested on the {}",
            choice.describe(),
            if *gpu { "GPU" } else { "CPU" }
        )?;
    }
    if let Some(vault) = &opts.export_vault
        && !vault.is_dir()
    {
        bail!("export vault {} is not a directory", vault.display());
    }
    let order = choice.order(&accelerator_order());
    // VAD, segmentation and speaker embeddings run on the CPU (T6.11); the CUDA library of
    // ONNX Runtime is loaded only for a Parakeet model that may run on CUDA. If it cannot be
    // loaded, the CPU library is (Parakeet then refuses CUDA and runs on the CPU, reported).
    let mut onnx_note = Ok(());
    let ort = init_onnx_for(&models, &model, &order, &mut |library, e| {
        onnx_note = writeln!(
            out,
            "onnx runtime: the {library:?} library is unavailable ({e})"
        );
    })
    .map_err(runtime)?;
    onnx_note?;
    writeln!(out, "data: {}", opts.out.display())?;
    writeln!(out, "models: {}", models.root().display())?;
    writeln!(
        out,
        "onnx runtime: {} (VAD and speakers on the CPU)",
        ort.display()
    )?;
    let mut store = Store::open(&data.db_path())
        .with_context(|| format!("cannot open the database in {}", opts.out.display()))?;
    config.speaker_timing = choose_speaker_timing(
        &mut store,
        opts.speakers.map(SpeakerTiming::from),
        opts.save_speakers,
        out,
    )?;
    let live_summaries = choose_live_summaries(
        &mut store,
        opts.live_summaries.map(|v| v == OnOff::On),
        opts.save_live_summaries,
        out,
    )?;
    if live_summaries {
        match host::live_summaries_for(summary.as_ref(), &order) {
            Ok(l) => config.live_summaries = Some(l),
            Err(why) => writeln!(out, "summarise during the meeting: off ({why})")?,
        }
    }
    let svc = Services::new(store, data, Arc::clone(&clock)).map_err(runtime)?;

    // 2. The live session: live whisper threads follow the machine (T6.10b), and the
    // speaker windows are computed during the meeting (T6.13d2) only when there is a
    // cleanup to finish from them.
    let factory = live_factory(&models, &model, &order, Arc::clone(&clock), !opts.no_post);
    let mut session = start_meeting(&svc, config, start, factory).map_err(runtime)?;
    // From here on the recording must be finalised whatever happens: on any error the
    // session is stopped and joined before the error is returned.
    let outcome = match follow_session(&svc, &mut session, should_stop, log, out) {
        Ok(()) => session.wait().map_err(runtime)?,
        Err(e) => {
            let _ = session.stop();
            return Err(e);
        }
    };
    // Hang-up: the recording is finalised; the summary's wait is timed from here.
    let hung_up = std::time::Instant::now();
    let meeting = outcome.meeting;
    {
        let db = svc.store();
        for f in db.audio_files(meeting)? {
            let path = svc
                .data
                .resolve(&f.path)
                .map_err(|e| anyhow::anyhow!("{e}"))?;
            writeln!(out, "audio: {}", path.display())?;
        }
    }

    // 3 and 4. The post-meeting pass, then the summary (network only there, only to the
    // configured endpoint; a local model makes no connection).
    let post_cancel = CancelToken::new();
    let after = AfterMeeting {
        models: &models,
        model: &model,
        order: &order,
        post: !opts.no_post,
        whole_pass: opts.whole_pass,
        summary: summary.as_ref().map(|s| &s.service),
        no_summary: skip_reason(&plan).unwrap_or(SkipReason::NotRequested),
        post_cancel: &post_cancel,
        summary_cancel: &CancelToken::new(),
    };
    let mut printer = PostPrinter {
        out: &mut *out,
        log,
        cleanup,
        whole_pass: opts.whole_pass,
        model: &model,
        token: post_cancel.clone(),
        done: Arc::new(AtomicBool::new(false)),
        watcher: None,
        error: None,
    };
    let host = after_meeting(&svc, &outcome, &after, &mut |step| printer.step(step));
    if let Some(e) = printer.error.take() {
        return Err(e.into());
    }
    host.post.map_err(runtime)?;
    // PLAN.md 3.9's agent-CLI exception (Claude Code, Codex): when and how much was handed
    // over, never the text.
    let cli_name = |cli: earshot_runtime::claude_code::HandoverCli| cli.name().to_lowercase();
    for h in &host.handovers {
        writeln!(
            out,
            "{}: handed {} characters at {} ms ({:?})",
            cli_name(h.cli),
            h.characters,
            h.at_unix_ms,
            h.outcome
        )?;
    }
    if let Some(first) = host.handovers.first()
        && host.handover_record_failures.is_empty()
    {
        writeln!(
            out,
            "{}: stored in this meeting's local handover record (when and how many characters, never the text)",
            cli_name(first.cli)
        )?;
    }
    for why in &host.handover_record_failures {
        writeln!(
            out,
            "handover: a handover could not be stored in the local record ({why})"
        )?;
    }
    if host.claude_effort_unread {
        writeln!(
            out,
            "claude-code: ran at its own default effort (its --help could not be read)"
        )?;
    }
    if summary.is_some() {
        for e in &host.network {
            writeln!(
                out,
                "network: {} {}://{}:{}{} status {:?} ({:?})",
                e.method, e.scheme, e.host, e.port, e.path, e.status, e.outcome
            )?;
        }
    }
    match host.summary {
        SummaryResult::Stored { model, outcome, .. } => {
            if let SummaryRunReport::ClaudeCode(r) = &outcome.report {
                writeln!(
                    out,
                    "summary model: {model} through Claude Code ({} run(s), {} repair(s))",
                    r.requests, r.repairs
                )?;
                writeln!(
                    out,
                    "network: none of Earshot's own (Claude Code sent the handed-over text to Anthropic itself)"
                )?;
            }
            if let SummaryRunReport::Codex(r) = &outcome.report {
                writeln!(
                    out,
                    "summary model: {model} through Codex ({} run(s), {} repair(s))",
                    r.requests, r.repairs
                )?;
                writeln!(
                    out,
                    "network: none of Earshot's own (Codex sent the handed-over text to OpenAI itself)"
                )?;
            }
            if let SummaryRunReport::Local(r) = &outcome.report {
                if let Some(refusal) = &r.gpu_refused {
                    writeln!(
                        out,
                        "summary: the GPU load was refused ({refusal}); the model ran on the CPU"
                    )?;
                }
                writeln!(
                    out,
                    "summary model: {model} on {} (load {:.1} s, {} time chunk(s), {} request(s))",
                    r.backend.name(),
                    r.load_time.as_secs_f64(),
                    r.chunks,
                    r.run.requests
                )?;
                if r.live.recorded > 0 || r.live.not_used.is_some() {
                    let mut line = format!(
                        "summary: {} of {} window(s) summarised during the meeting reused",
                        r.live.reused, r.live.recorded
                    );
                    if r.live.recite_failed + r.live.unplaced > 0 {
                        line += &format!(", {} redone", r.live.recite_failed + r.live.unplaced);
                    }
                    if let Some(why) = &r.live.not_used {
                        line += &format!(" (not used: {why})");
                    }
                    writeln!(out, "{line}")?;
                }
                writeln!(
                    out,
                    "network: none (local summary, {} audit entries)",
                    host.network.len()
                )?;
            }
            writeln!(
                out,
                "summary: {} - {}",
                outcome.summary.title, outcome.summary.tldr
            )?;
            writeln!(
                out,
                "summary: stored {:.1} s after the recording ended",
                hung_up.elapsed().as_secs_f64()
            )?;
        }
        SummaryResult::Failed { error, .. } => return Err(runtime(error)),
        // The CLI never cancels the summary.
        SummaryResult::Cancelled => bail!("summary: cancelled"),
        SummaryResult::Skipped { .. } => {
            writeln!(out, "network: none ({no_summary_network})")?;
        }
    }

    // 5. Export, on request only.
    if let Some(vault) = &opts.export_vault {
        let r = earshot_runtime::export::export_meeting(
            &svc,
            vault,
            meeting,
            &ExportOptions::default(),
            &mut *os_seeded_rng(),
        )
        .map_err(runtime)?;
        writeln!(out, "export: {} ({:?})", r.path, r.action)?;
    }

    // 5b. The summary input, on request only (a local file).
    if let Some(path) = &opts.summary_input {
        let input = SummaryService::input(&svc, meeting).map_err(runtime)?;
        std::fs::write(path, serde_json::to_vec_pretty(&input)?)
            .with_context(|| format!("cannot write {}", path.display()))?;
        writeln!(
            out,
            "summary input: {} ({} segments)",
            path.display(),
            input.segments.len()
        )?;
    }

    // 6. The stored transcript.
    let view = {
        let db = svc.store();
        MeetingView::load(&**db, meeting)?
    };
    if let Some(view) = view {
        let pass = match view.pass {
            Some(TranscriptPass::Final) => "final pass",
            Some(TranscriptPass::Live) => "live draft, not the final transcript",
            None => "none",
        };
        writeln!(out, "transcript ({pass}):")?;
        for seg in &view.segments {
            writeln!(
                out,
                "{} {}: {}",
                at(seg.start),
                view.speaker_name(seg),
                seg.text
            )?;
        }
    }
    Ok(meeting)
}

/// The canceller's lock stretches: `canceller lock 120 ms` for one, else the count and
/// the delays.
#[must_use]
pub fn format_locks(locks: &[EchoLock]) -> String {
    match locks {
        [one] => format!("canceller lock {} ms", one.delay_ms),
        many => format!(
            "canceller locked in {} stretches: {} ms",
            many.len(),
            many.iter()
                .map(|l| l.delay_ms.to_string())
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

/// The post pass's echo-cleaning line.
#[must_use]
pub fn format_post_echo(e: &EchoReport) -> String {
    if e.locks.is_empty() {
        return "post: echo: no canceller lock, nothing removed".to_owned();
    }
    // T6.23a: words of a mostly-echo cluster that sounds like the far end, named only when
    // there are any.
    let cluster = if e.cluster_removed > 0 {
        format!(", echo cluster {}", e.cluster_removed)
    } else {
        String::new()
    };
    let mut s = format!(
        "post: echo removed from Me: {} word(s) (text {}, speaker check {}{cluster}; {})",
        e.removed(),
        e.text_removed,
        e.speaker_removed,
        format_locks(&e.locks)
    );
    if !e.bridged_gaps.is_empty() {
        let gaps: Vec<String> = e
            .bridged_gaps
            .iter()
            .map(|&(a, b)| format!("{}-{}", format_time(a), format_time(b)))
            .collect();
        s += &format!(
            "; also cleaned in {} lock gap(s) at the same delay: {}",
            gaps.len(),
            gaps.join(", ")
        );
    }
    if let Some(why) = &e.speaker_check_skipped {
        s += &format!("; speaker check skipped: {why}");
    }
    s
}
/// How a host lets the user cancel the post-meeting cleanup (T6.18): `record` allows one
/// more Ctrl+C after the one that stopped the recording.
pub struct CleanupInterrupt {
    /// Called once just before the cleanup starts.
    pub arm: Box<dyn Fn()>,
    /// True once the user asked to cancel the cleanup.
    pub requested: Arc<dyn Fn() -> bool + Send + Sync>,
    /// What the user presses, for the message ("Ctrl+C").
    pub key: &'static str,
}

/// Final-pass transcription time over audio time for `model` on `backend`: a rough upper
/// bound, a few times the measured speed (docs/asr-models.md: Silero pieces, 2 threads on
/// a GPU and 4 per track on the CPU of the desktop; beam search, which a live-chunker
/// meeting's final pass uses, takes about 2.5x greedy), so a slower host or a busy machine
/// still finishes within it. Measured (greedy, desktop): Parakeet 0.01 on CUDA and 0.03 on
/// the CPU; base.en 0.017 on CUDA and 0.31 on the CPU; small.en 0.025 and 1.03;
/// large-v3-turbo 0.023 on CUDA, too slow on the CPU; Vulkan small.en 0.142 for two tracks.
fn transcription_factor(model: &str, backend: BackendKind) -> f64 {
    let gpu = match backend {
        BackendKind::Cuda | BackendKind::Qnn => Some(1.0),
        BackendKind::Vulkan => Some(3.0),
        _ => None,
    };
    match asr_model(model) {
        Ok(AsrModel::Parakeet(_)) => gpu.map_or(0.1, |slow| 0.03 * slow),
        // Whisper by size: tiny and base, small, then large (and any other name).
        _ => {
            let (on_gpu, on_cpu) = if model.starts_with("tiny") || model.starts_with("base") {
                (0.1, 1.0)
            } else if model.starts_with("small") {
                (0.15, 3.0)
            } else {
                (0.2, 5.0)
            };
            gpu.map_or(on_cpu, |slow| on_gpu * slow)
        }
    }
}

/// Speaker work time over audio time on the CPU, a rough upper bound: segmentation and
/// CAM++ embeddings took 0.03 of the audio on the desktop (private notes, ONNX on
/// the CPU, T6.11), slower on a smaller CPU.
const DIARISATION_FACTOR: f64 = 0.1;

/// Rough upper bound on the cleanup time: the audio the final pass still has to
/// transcribe ([`PostProcessor::cleanup_work`]: after the live stretches it can reuse) at
/// `model`'s speed on `backend` ([`transcription_factor`]), plus the audio it still has
/// to diarise (after the reusable live speaker windows) at [`DIARISATION_FACTOR`], rounded
/// up to 10 s. Transcription and speaker work run side by side (T6.12) and skipped
/// silence (T6.16) is not decoded, so the real time is usually shorter.
#[must_use]
pub fn cleanup_estimate(work: &[CleanupWork], model: &str, backend: BackendKind) -> Duration {
    let factor = transcription_factor(model, backend);
    let secs: f64 = work
        .iter()
        .map(|w| {
            w.to_transcribe.as_secs_f64() * factor + w.to_diarise.as_secs_f64() * DIARISATION_FACTOR
        })
        .sum();
    Duration::from_secs_f64(((secs / 10.0).ceil() * 10.0).max(10.0))
}

fn rough(d: Duration) -> String {
    let s = d.as_secs();
    if s < 90 {
        format!("about {s} s")
    } else {
        format!("about {} min", s.div_ceil(60))
    }
}

/// The cleanup warnings worth a line of their own: an unreadable stretch record is already
/// on that track's stretch line ([`format_stretch_use`], same reason), so it is not printed
/// twice.
fn printed_warnings(warnings: &[PostWarning]) -> impl Iterator<Item = &PostWarning> {
    warnings
        .iter()
        .filter(|w| !matches!(w, PostWarning::StretchRecordUnreadable { .. }))
}

/// Whether the cleanup took a track's live speech stretches (T6.13c) or transcribed it whole,
/// and why.
#[must_use]
pub fn format_stretch_use(track: TrackId, u: &StretchUse) -> String {
    match u {
        StretchUse::Reused {
            through,
            pieces,
            backend,
        } => format!(
            "post: {}: took {pieces} live speech piece(s) up to {} (decoded on {}), transcribed only the rest",
            track_name(track),
            format_time(through.as_millis()),
            format_backend(backend)
        ),
        StretchUse::Transcribed { reason } => format!(
            "post: {}: transcribed the whole track ({reason})",
            track_name(track)
        ),
    }
}

/// The user cancelled the cleanup: no final pass will finish from the live speaker windows
/// (speaker embeddings, biometric data), so they are deleted at once
/// ([`host::cleanup_cancelled`]); the recordings and the live transcript are kept.
#[cfg(test)]
fn cleanup_cancelled(svc: &Services, meeting: EntityId, out: &mut dyn Write) -> Result<()> {
    Ok(print_cleanup_cancelled(
        &host::cleanup_cancelled(svc, meeting),
        out,
    )?)
}

/// The lines for a cancelled cleanup, given what deleting the speaker windows did.
fn print_cleanup_cancelled(
    deleted: &Result<usize, RuntimeError>,
    out: &mut dyn Write,
) -> std::io::Result<()> {
    writeln!(
        out,
        "post: cleanup cancelled; the recordings and the live transcript are kept"
    )?;
    match deleted {
        Ok(0) => {}
        Ok(n) => writeln!(
            out,
            "post: deleted the speaker windows computed during the meeting ({n} track(s))"
        )?,
        Err(e) => writeln!(
            out,
            "post: warning: the speaker windows computed during the meeting could not be \
             deleted: {e}"
        )?,
    }
    Ok(())
}

/// Whether the cleanup finished a track's speakers from the live speaker windows (T6.13d2)
/// or diarised it whole, and why.
#[must_use]
pub fn format_speaker_window_use(track: TrackId, u: &SpeakerWindowUse) -> String {
    match u {
        // Nothing was taken (a meeting shorter than one batch, or speaker work that stopped
        // before its first batch): the whole track was diarised here.
        SpeakerWindowUse::Reused {
            windows: 0,
            stopped,
        } => {
            let mut why = "no speaker windows were finished during the meeting".to_owned();
            if let Some(stopped) = stopped {
                why.push_str("; ");
                why.push_str(stopped);
            }
            format!(
                "post: {}: found the speakers over the whole track ({why})",
                track_name(track)
            )
        }
        SpeakerWindowUse::Reused { windows, stopped } => {
            let mut line = format!(
                "post: {}: took {windows} speaker window(s) from the meeting, computed only the rest",
                track_name(track)
            );
            if let Some(why) = stopped {
                line.push_str(&format!(" ({why})"));
            }
            line
        }
        SpeakerWindowUse::Diarised { reason } => format!(
            "post: {}: found the speakers over the whole track ({reason})",
            track_name(track)
        ),
    }
}

/// One progress line for a finished cleanup step.
#[must_use]
pub fn format_stage(stage: &PostStage) -> String {
    let secs = |d: &Duration| format!("{:.1} s", d.as_secs_f64());
    match stage {
        PostStage::Transcribed { track, elapsed } => format!(
            "post: re-transcribed the {} track ({})",
            track_name(*track),
            secs(elapsed)
        ),
        PostStage::Diarised {
            track,
            speakers,
            elapsed,
        } => format!(
            "post: found {speakers} speaker(s) on the {} track ({})",
            track_name(*track),
            secs(elapsed)
        ),
        PostStage::EchoChecked { elapsed } => format!("post: echo check done ({})", secs(elapsed)),
        PostStage::Stored { elapsed } => format!(
            "post: speakers named and transcript stored ({})",
            secs(elapsed)
        ),
    }
}

/// Cancels `token` once `requested` turns true, polling every 50 ms until `done`; returns
/// whether it cancelled.
fn watch_for_cancel(
    requested: Arc<dyn Fn() -> bool + Send + Sync>,
    token: CancelToken,
    done: Arc<AtomicBool>,
) -> std::thread::JoinHandle<bool> {
    std::thread::spawn(move || {
        while !done.load(Ordering::SeqCst) {
            if requested() {
                token.cancel();
                return true;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        false
    })
}

/// Prints the post-meeting run's steps ([`HostStep`]) as they happen and runs the host's
/// cleanup interrupt: armed when the final pass starts, watched until it ends.
struct PostPrinter<'a> {
    out: &'a mut dyn Write,
    log: Option<&'a EventLog>,
    cleanup: Option<&'a CleanupInterrupt>,
    whole_pass: bool,
    model: &'a str,
    /// The post pass's cancel token, cancelled by the watcher.
    token: CancelToken,
    done: Arc<AtomicBool>,
    watcher: Option<std::thread::JoinHandle<bool>>,
    /// The first output error (the run is not interrupted for it).
    error: Option<std::io::Error>,
}

impl PostPrinter<'_> {
    fn step(&mut self, step: HostStep<'_>) {
        if let Err(e) = self.print(step) {
            self.error.get_or_insert(e);
        }
    }

    fn print(&mut self, step: HostStep<'_>) -> std::io::Result<()> {
        let out = &mut *self.out;
        match step {
            HostStep::PostLoaded { missing } => {
                for t in missing {
                    writeln!(out, "post: no recording for the {} track", track_name(*t))?;
                }
            }
            HostStep::PostStarting {
                fallbacks,
                work,
                asr_backend,
            } => {
                if self.whole_pass {
                    writeln!(
                        out,
                        "post: the whole-meeting pass was requested (--whole-pass): the live \
                         stretches and speaker windows are not used"
                    )?;
                }
                for (kind, e) in fallbacks {
                    writeln!(out, "post: refused {kind:?}: {e}")?;
                }
                let mut start = format!(
                    "post: cleaning up the transcript (re-transcribe, find speakers, echo check), up to {}",
                    rough(cleanup_estimate(work, self.model, asr_backend))
                );
                if let Some(c) = self.cleanup {
                    start.push_str(&format!(
                        "; {key} cancels the cleanup only (the recordings and the live transcript \
                         are kept), a second {key} quits at once",
                        key = c.key
                    ));
                }
                writeln!(out, "{start}")?;
                out.flush()?;
                if let Some(log) = self.log {
                    log.post_start();
                }
                if let Some(c) = self.cleanup {
                    (c.arm)();
                    self.watcher = Some(watch_for_cancel(
                        Arc::clone(&c.requested),
                        self.token.clone(),
                        Arc::clone(&self.done),
                    ));
                }
            }
            HostStep::Post(stage) => {
                if let Some(log) = self.log {
                    log.stage(&stage);
                }
                // Progress lines never stop the cleanup.
                let _ = writeln!(out, "{}", format_stage(&stage));
                let _ = out.flush();
            }
            HostStep::PostFinished(result) => {
                self.done.store(true, Ordering::SeqCst);
                if let Some(w) = self.watcher.take() {
                    let _ = w.join();
                }
                match result {
                    Ok(HostPost::Skipped) => writeln!(out, "post: skipped (--no-post)")?,
                    Ok(HostPost::NothingToProcess) => writeln!(out, "post: nothing to process")?,
                    Ok(HostPost::Cancelled {
                        speaker_windows_deleted,
                    }) => print_cleanup_cancelled(speaker_windows_deleted, out)?,
                    Ok(HostPost::Done(result)) => print_post_result(result, self.log, out)?,
                    // The caller returns the error.
                    Err(_) => {}
                }
            }
            HostStep::SummaryStarting => {}
        }
        Ok(())
    }
}

/// The lines for a finished final pass.
fn print_post_result(
    result: &PostOutcome,
    log: Option<&EventLog>,
    out: &mut dyn Write,
) -> std::io::Result<()> {
    if let Some(log) = log {
        log.post_done(
            result.final_segments,
            result.speakers.len(),
            &format!("{:?}", result.backend.kind),
        );
    }
    writeln!(
        out,
        "post: final pass ran on {}; {} segments, {} speakers",
        format_backend(&result.backend),
        result.final_segments,
        result.speakers.len()
    )?;
    for (track, u) in &result.stretches {
        writeln!(out, "{}", format_stretch_use(*track, u))?;
    }
    for (track, u) in &result.speaker_windows {
        writeln!(out, "{}", format_speaker_window_use(*track, u))?;
    }
    writeln!(out, "{}", format_post_echo(&result.echo))?;
    for w in printed_warnings(&result.warnings) {
        writeln!(out, "post: warning: {w:?}")?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use earshot_core::asr::{AsrError, BackendKind, PartialSegment, Segment, Word};
    use earshot_core::capture::{DiscontinuityKind, EndReason};
    use earshot_core::pipeline::{FinalPassRequest, Levels, TrackStats};

    fn info(kind: BackendKind) -> BackendInfo {
        BackendInfo {
            kind,
            device: "cpu".into(),
            model: "ggml-tiny.en.bin".into(),
        }
    }

    fn stats(end_ms: u64, aec: Option<AecStats>) -> PipelineStats {
        PipelineStats {
            end: MeetingTime::from_millis(end_ms),
            aec,
            mic: TrackStats {
                skew_ms: 1.25,
                asr_backlog: 8_000,
                ..TrackStats::default()
            },
            reference: aec.map(|_| TrackStats {
                skew_ms: -0.5,
                ..TrackStats::default()
            }),
            reference_correction_ppm: 12.0,
        }
    }

    fn aec() -> AecStats {
        AecStats {
            delay_ms: Some(42.0),
            locked: true,
            double_talk: false,
            vetoed: false,
            suppression_gain: 1.0,
            bypass: false,
            converged: true,
            residual_drift_ppm: None,
            attenuation_db: Some(18.5),
        }
    }

    #[test]
    fn session_backend_lines_name_what_runs_and_what_was_refused() {
        let line = format_event(&RuntimeEvent::LiveBackend {
            track: TrackId::Reference,
            backend: info(BackendKind::Cpu),
            fallbacks: vec![(BackendKind::Cuda, AsrError::Backend("no GPU found".into()))],
        })
        .unwrap();
        assert!(
            line.starts_with("live asr reference: Cpu (cpu, ggml-tiny.en.bin)"),
            "{line}"
        );
        assert!(line.contains("refused Cuda"), "{line}");
        assert!(line.contains("no GPU found"), "{line}");
    }

    #[test]
    fn session_live_finals_are_printed_with_speaker_and_time() {
        let seg = Segment::from_words(
            EntityId::from_bytes([1; 16]),
            EntityId::from_bytes([2; 16]),
            vec![
                Word::new("hello", 61_000, 61_400, 0.9),
                Word::new("there", 61_500, 61_900, 0.9),
            ],
            MeetingTime::from_millis(0),
        );
        let line = format_event(&RuntimeEvent::Transcript {
            track: TrackId::Mic,
            event: TranscriptEvent::Final(seg),
        })
        .unwrap();
        assert_eq!(line, "[01:01.000] Me: hello there");
    }

    #[test]
    fn session_levels_partials_and_stats_are_not_event_lines() {
        let s = RuntimeEvent::Stats(stats(0, None));
        assert!(format_event(&s).is_none());
        let levels = RuntimeEvent::Levels(Levels {
            end: MeetingTime::from_millis(100),
            mic_dbfs: -20.0,
            cleaned_dbfs: -20.0,
            reference_dbfs: None,
        });
        assert!(format_event(&levels).is_none());
        let partial = RuntimeEvent::Transcript {
            track: TrackId::Mic,
            event: TranscriptEvent::Partial(PartialSegment {
                segment: Segment::from_words(
                    EntityId::from_bytes([1; 16]),
                    EntityId::from_bytes([2; 16]),
                    vec![Word::new("hel", 0, 300, 0.5)],
                    MeetingTime::from_millis(0),
                ),
                committed_words: 0,
            }),
        };
        assert!(format_event(&partial).is_none());
        let discarded = RuntimeEvent::Transcript {
            track: TrackId::Mic,
            event: TranscriptEvent::Discarded {
                id: EntityId::from_bytes([1; 16]),
            },
        };
        assert!(format_event(&discarded).is_none());
    }

    /// A stream that sends `Levels` every millisecond (so a receive never times out)
    /// until a stop is requested, then `Ended`.
    fn busy_stream() -> (
        Receiver<RuntimeEvent>,
        Arc<std::sync::atomic::AtomicBool>,
        std::thread::JoinHandle<u64>,
    ) {
        use std::sync::atomic::{AtomicBool, Ordering};
        let (tx, rx) = std::sync::mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&stop);
        let feeder = std::thread::spawn(move || {
            let mut sent = 0u64;
            let deadline = std::time::Instant::now() + Duration::from_secs(20);
            while !flag.load(Ordering::SeqCst) && std::time::Instant::now() < deadline {
                let levels = Levels {
                    end: MeetingTime::from_millis(sent * 100),
                    mic_dbfs: -30.0,
                    cleaned_dbfs: -30.0,
                    reference_dbfs: None,
                };
                if tx.send(RuntimeEvent::Levels(levels)).is_err() {
                    return sent;
                }
                sent += 1;
                std::thread::sleep(Duration::from_millis(1));
            }
            let _ = tx.send(RuntimeEvent::Ended(LiveOutcome {
                meeting: EntityId::from_bytes([3; 16]),
                tracks: vec![],
                end: MeetingTime::from_millis(sent * 100),
                reason: EndReason::Stopped,
                final_pass: FinalPassRequest {
                    tracks: vec![],
                    end: MeetingTime::from_millis(sent * 100),
                },
                audio_files: vec![],
                unavailable_at_end: vec![],
                problems: vec![],
                echo_words_removed: 0,
                echo_locks: Vec::new(),
            }));
            sent
        });
        (rx, stop, feeder)
    }

    #[test]
    fn session_stop_is_seen_while_events_never_pause() {
        use std::cell::Cell;
        use std::sync::atomic::Ordering;
        let (rx, stop, feeder) = busy_stream();
        let checks = Cell::new(0u32);
        let requests = Cell::new(0u32);
        let mut out = Vec::new();
        pump_events(
            &rx,
            &|| {
                checks.set(checks.get() + 1);
                checks.get() >= 50
            },
            &|| {
                requests.set(requests.get() + 1);
                stop.store(true, Ordering::SeqCst);
            },
            None,
            &mut out,
        )
        .unwrap();
        let sent = feeder.join().unwrap();
        assert_eq!(requests.get(), 1, "the stop is requested exactly once");
        assert!(
            sent < 1_000,
            "stopped promptly, not by the feeder's deadline: {sent}"
        );
        let text = String::from_utf8(out).unwrap();
        assert!(
            text.contains("stopping (finalising the recording)"),
            "{text}"
        );
        assert!(text.contains("ended (Stopped)"), "{text}");
    }

    #[test]
    fn session_stop_requested_before_any_event_is_honoured_at_once() {
        use std::cell::Cell;
        use std::sync::atomic::Ordering;
        let (rx, stop, feeder) = busy_stream();
        let requests = Cell::new(0u32);
        let mut out = Vec::new();
        pump_events(
            &rx,
            &|| true,
            &|| {
                requests.set(requests.get() + 1);
                stop.store(true, Ordering::SeqCst);
            },
            None,
            &mut out,
        )
        .unwrap();
        feeder.join().unwrap();
        assert_eq!(requests.get(), 1);
        let text = String::from_utf8(out).unwrap();
        assert!(text.starts_with("stopping"), "{text}");
    }

    /// A temp models directory with the given files (empty) under it.
    struct Models(std::path::PathBuf);

    impl Models {
        fn with(tag: &str, files: &[String]) -> Self {
            let dir = std::env::temp_dir()
                .join(format!("earshot-cli-models-{tag}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            for f in files {
                let p = dir.join(f);
                std::fs::create_dir_all(p.parent().unwrap()).unwrap();
                std::fs::write(p, b"").unwrap();
            }
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }
    }

    impl Drop for Models {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn session_requires_the_post_models_before_recording_unless_no_post() {
        let whisper = asr_model("tiny.en").unwrap().files().remove(0);
        let live_only = Models::with("live", &[whisper.clone(), SILERO.into()]);
        let dir = ModelDir::new(&live_only.0);
        require_models(&dir, "tiny.en", false).unwrap();
        let e = require_models(&dir, "tiny.en", true)
            .unwrap_err()
            .to_string();
        assert!(e.contains("fetch-models --set ci"), "{e}");
        assert!(e.contains("segmentation") || e.contains("campplus"), "{e}");

        let no_vad = Models::with("novad", &[whisper]);
        let e = require_models(&ModelDir::new(&no_vad.0), "tiny.en", false)
            .unwrap_err()
            .to_string();
        assert!(e.contains("silero"), "{e}");
        assert!(require_models(&dir, "no-such-model", false).is_err());
    }

    #[test]
    fn session_model_comes_from_a_current_model_check() {
        let dir = std::env::temp_dir().join(format!("earshot-cli-choose-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let data = DataDir::new(&dir);
        data.create().unwrap();
        let models = ModelDir::new(dir.join("models"));
        let mut out = Vec::new();
        // --model wins without looking.
        assert_eq!(
            choose_model(Some("tiny.en"), &data, &models, &mut out).unwrap(),
            ModelChoice {
                model: "tiny.en".into(),
                backend: None
            }
        );
        // No check yet: the fallback, and the hint names the command.
        assert_eq!(
            choose_model(None, &data, &models, &mut out).unwrap(),
            ModelChoice {
                model: FALLBACK_MODEL.into(),
                backend: None
            }
        );
        assert!(String::from_utf8_lossy(&out).contains("earshot-cli benchmark"));
        // A current check is used.
        let fingerprint = check_fingerprint(&models, &hardware_facts(), &["en"]);
        let check = |fingerprint: String| earshot_runtime::model_bench::StoredCheck {
            fingerprint,
            model: "parakeet-tdt-110m".into(),
            backend: earshot_core::asr::BackendKind::Cpu,
            fallback: Some("base.en".into()),
            trials: Vec::new(),
        };
        {
            let mut store = Store::open(&data.db_path()).unwrap();
            earshot_runtime::model_bench::record_check(&mut store, &check(fingerprint)).unwrap();
        }
        // ... with the backend it kept: a CPU answer is not overruled by a GPU.
        let choice = choose_model(None, &data, &models, &mut out).unwrap();
        assert_eq!(
            choice,
            ModelChoice {
                model: "parakeet-tdt-110m".into(),
                backend: Some(earshot_core::asr::BackendKind::Cpu)
            }
        );
        assert_eq!(
            choice.order(&[Accelerator::Cuda, Accelerator::Vulkan, Accelerator::Cpu]),
            [Accelerator::Cpu]
        );
        let vulkan = ModelChoice {
            backend: Some(earshot_core::asr::BackendKind::Vulkan),
            ..choice
        };
        assert_eq!(
            vulkan.order(&[Accelerator::Vulkan, Accelerator::Cpu]),
            [Accelerator::Vulkan, Accelerator::Cpu]
        );
        let explicit = ModelChoice {
            backend: None,
            ..vulkan
        };
        assert_eq!(
            explicit.order(&[Accelerator::Cuda, Accelerator::Cpu]),
            [Accelerator::Cuda, Accelerator::Cpu]
        );
        // An out-of-date check (another machine, other models) is not.
        {
            let mut store = Store::open(&data.db_path()).unwrap();
            earshot_runtime::model_bench::record_check(&mut store, &check("old".into())).unwrap();
        }
        out.clear();
        assert_eq!(
            choose_model(None, &data, &models, &mut out).unwrap().model,
            FALLBACK_MODEL
        );
        assert!(String::from_utf8_lossy(&out).contains("out of date"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn session_requires_every_file_of_a_parakeet_model() {
        let files = asr_model("parakeet-tdt-0.6b-v2").unwrap().files();
        assert!(
            files.iter().any(|f| f.ends_with("encoder.weights")),
            "{files:?}"
        );
        let mut all = files.clone();
        all.push(SILERO.into());
        let full = Models::with("parakeet", &all);
        require_models(&ModelDir::new(&full.0), "parakeet-tdt-0.6b-v2", false).unwrap();
        // Any one file missing fails with the model's fetch set (ci for the default v2).
        for missing in &files {
            let rest: Vec<String> = all.iter().filter(|f| *f != missing).cloned().collect();
            let partial = Models::with("parakeet-partial", &rest);
            let e = require_models(&ModelDir::new(&partial.0), "parakeet-tdt-0.6b-v2", false)
                .unwrap_err()
                .to_string();
            assert!(e.contains("fetch-models --set ci"), "{missing}: {e}");
        }
    }

    #[test]
    fn session_stats_show_aec_skews_and_backlog() {
        let line = format_stats(&stats(5_000, Some(aec())));
        assert!(
            line.starts_with("[00:05.000] aec locked delay 42.0 ms"),
            "{line}"
        );
        assert!(line.contains("attenuation 18.5 dB converged"), "{line}");
        assert!(
            line.contains("skew mic 1.2 ms, reference -0.5 ms, steering 12.0 ppm"),
            "{line}"
        );
        assert!(
            line.contains("asr backlog mic 0.5 s, reference 0.0 s"),
            "{line}"
        );
        let mono = format_stats(&stats(0, None));
        assert!(mono.contains("aec off (no reference)"), "{mono}");
        assert!(!mono.contains("steering"), "{mono}");
    }

    #[test]
    fn session_stats_are_rate_limited_and_the_last_one_is_kept() {
        let mut p = StatsPrinter::default();
        assert!(p.push(&stats(0, None)).is_some());
        assert!(p.push(&stats(1_000, None)).is_none());
        assert!(p.push(&stats(4_999, None)).is_none());
        assert!(p.push(&stats(5_000, None)).is_some());
        assert!(p.finish().is_none(), "the last one was printed");
        assert!(p.push(&stats(6_000, None)).is_none());
        let last = p.finish().unwrap();
        assert!(last.starts_with("[00:06.000]"), "{last}");
        assert!(p.finish().is_none());
    }

    #[test]
    fn session_end_prints_the_held_back_stats_first() {
        let mut p = StatsPrinter::default();
        let mut out = Vec::new();
        print_event(&mut out, &mut p, &RuntimeEvent::Stats(stats(0, None)), None).unwrap();
        print_event(
            &mut out,
            &mut p,
            &RuntimeEvent::Stats(stats(2_000, None)),
            None,
        )
        .unwrap();
        let ended = RuntimeEvent::Ended(LiveOutcome {
            meeting: EntityId::from_bytes([3; 16]),
            tracks: vec![],
            end: MeetingTime::from_millis(2_000),
            reason: EndReason::Finished,
            final_pass: FinalPassRequest {
                tracks: vec![],
                end: MeetingTime::from_millis(2_000),
            },
            audio_files: vec![],
            unavailable_at_end: vec![TrackId::Reference],
            problems: vec![RuntimeWarning::Persistence("disk full".into())],
            echo_words_removed: 3,
            echo_locks: vec![EchoLock {
                from_ms: 0,
                to_ms: 2_000,
                delay_ms: 120,
            }],
        });
        print_event(&mut out, &mut p, &ended, None).unwrap();
        let text = String::from_utf8(out).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert!(lines[0].starts_with("[00:00.000]"), "{text}");
        assert!(lines[1].starts_with("[00:02.000] aec off"), "{text}");
        assert!(
            lines[2].starts_with("[00:02.000] ended (Finished)"),
            "{text}"
        );
        assert!(
            text.contains("reference device unavailable at the end"),
            "{text}"
        );
        assert!(
            text.contains("warning: storing failed: disk full"),
            "{text}"
        );
        assert!(
            text.contains("echo: 3 live Me word(s) removed (canceller lock 120 ms)"),
            "{text}"
        );
    }

    #[test]
    fn session_post_echo_line_reports_counts_and_a_skipped_check() {
        let ran = EchoReport {
            locks: vec![EchoLock {
                from_ms: 0,
                to_ms: 9_000,
                delay_ms: 150,
            }],
            text_removed: 4,
            speaker_removed: 2,
            speaker_check_skipped: None,
            bridged_gaps: Vec::new(),
            cluster_removed: 0,
        };
        assert_eq!(
            format_post_echo(&ran),
            "post: echo removed from Me: 6 word(s) (text 4, speaker check 2; canceller lock 150 ms)"
        );
        // T6.23a: words removed with a mostly-echo cluster are counted and named.
        let cluster = EchoReport {
            cluster_removed: 1,
            ..ran.clone()
        };
        assert_eq!(
            format_post_echo(&cluster),
            "post: echo removed from Me: 7 word(s) (text 4, speaker check 2, echo cluster 1; canceller lock 150 ms)"
        );
        // T6.23: gaps the final pass bridged are named, so a reader sees where words were
        // removed outside a recorded lock.
        let bridged = EchoReport {
            bridged_gaps: vec![(147_000, 154_000)],
            ..ran.clone()
        };
        let line = format_post_echo(&bridged);
        assert!(
            line.ends_with(&format!(
                "; also cleaned in 1 lock gap(s) at the same delay: {}-{}",
                format_time(147_000),
                format_time(154_000)
            )),
            "{line}"
        );
        let skipped = EchoReport {
            speaker_check_skipped: Some("no calibrated margin".into()),
            speaker_removed: 0,
            ..ran
        };
        assert!(
            format_post_echo(&skipped).ends_with("speaker check skipped: no calibrated margin"),
            "{}",
            format_post_echo(&skipped)
        );
        assert_eq!(
            format_post_echo(&EchoReport::default()),
            "post: echo: no canceller lock, nothing removed"
        );
        let two = [
            EchoLock {
                from_ms: 0,
                to_ms: 9_000,
                delay_ms: 150,
            },
            EchoLock {
                from_ms: 12_000,
                to_ms: 20_000,
                delay_ms: 310,
            },
        ];
        assert_eq!(
            format_locks(&two),
            "canceller locked in 2 stretches: 150, 310 ms"
        );
    }

    #[test]
    fn session_discontinuities_and_warnings_are_reported() {
        let d = format_event(&RuntimeEvent::Discontinuity {
            track: TrackId::Mic,
            at: MeetingTime::from_millis(1_500),
            kind: DiscontinuityKind::DataLost,
        })
        .unwrap();
        assert_eq!(d, "[00:01.500] mic: discontinuity DataLost");
        let w = format_event(&RuntimeEvent::Warning(RuntimeWarning::DeviceUnavailable {
            track: TrackId::Reference,
        }))
        .unwrap();
        assert!(
            w.starts_with("warning: reference device unavailable"),
            "{w}"
        );
    }

    /// Two 6-minute tracks with `transcribe` and `diarise` seconds left on each.
    fn two_tracks(transcribe: u64, diarise: u64) -> Vec<CleanupWork> {
        [TrackId::Mic, TrackId::Reference]
            .into_iter()
            .map(|track| CleanupWork {
                track,
                to_transcribe: Duration::from_secs(transcribe),
                to_diarise: Duration::from_secs(diarise),
            })
            .collect()
    }

    #[test]
    fn session_bluetooth_hands_free_mic_warning_suggests_the_computers_own_mic() {
        let w = format_event(&RuntimeEvent::Warning(
            RuntimeWarning::BluetoothHandsFreeMic {
                device_name: "Headset (Acme Headset Hands-Free)".into(),
                sample_rate: Some(16_000),
            },
        ))
        .unwrap();
        assert_eq!(
            w,
            "warning: the mic \"Headset (Acme Headset Hands-Free)\" is a Bluetooth headset in \
             hands-free mode (16 kHz): Windows records it at phone quality and lowers the \
             headset's playback while it is open. Use the computer's own mic and keep the \
             headset for listening."
        );
        let unknown_rate = format_event(&RuntimeEvent::Warning(
            RuntimeWarning::BluetoothHandsFreeMic {
                device_name: "Headset".into(),
                sample_rate: None,
            },
        ))
        .unwrap();
        assert!(
            unknown_rate.contains("hands-free mode: Windows"),
            "{unknown_rate}"
        );
    }

    #[test]
    fn session_cleanup_estimate_follows_the_model_backend_and_reuse() {
        let whole = two_tracks(360, 360);
        let est = |model, backend| cleanup_estimate(&whole, model, backend).as_secs();
        // whisper base.en: 720 s x 0.1 on CUDA, x 1.0 on the CPU, plus 72 s of speakers.
        assert_eq!(est("base.en", BackendKind::Cuda), 150);
        assert_eq!(est("base.en", BackendKind::Cpu), 800);
        // Parakeet on the CPU is about ten times faster than base.en there (T6.10e3: a
        // 13-minute meeting was estimated at 13 min and took about 1 min).
        assert_eq!(est("parakeet-tdt-0.6b-v2", BackendKind::Cpu), 150);
        assert_eq!(est("parakeet-tdt-0.6b-v2", BackendKind::Cuda), 100);
        // Larger whisper models are slower; Vulkan is slower than CUDA.
        assert!(est("small.en", BackendKind::Cpu) > est("base.en", BackendKind::Cpu));
        assert!(est("large-v3-turbo", BackendKind::Cpu) > est("small.en", BackendKind::Cpu));
        assert!(est("small.en", BackendKind::Vulkan) > est("small.en", BackendKind::Cuda));
        // Reused stretches and speaker windows leave little: only the rest counts.
        assert_eq!(
            cleanup_estimate(&two_tracks(0, 30), "base.en", BackendKind::Cpu),
            Duration::from_secs(10)
        );
        assert_eq!(
            cleanup_estimate(&two_tracks(60, 60), "base.en", BackendKind::Cpu),
            Duration::from_secs(140)
        );
        assert_eq!(
            cleanup_estimate(&[], "base.en", BackendKind::Cuda),
            Duration::from_secs(10)
        );
        assert_eq!(rough(Duration::from_secs(40)), "about 40 s");
        assert_eq!(rough(Duration::from_secs(220)), "about 4 min");
    }

    #[test]
    fn session_cleanup_progress_lines_name_the_step_and_track() {
        let d = Duration::from_millis(12_340);
        assert_eq!(
            format_stage(&PostStage::Transcribed {
                track: TrackId::Mic,
                elapsed: d
            }),
            "post: re-transcribed the mic track (12.3 s)"
        );
        assert_eq!(
            format_stage(&PostStage::Diarised {
                track: TrackId::Reference,
                speakers: 3,
                elapsed: d
            }),
            "post: found 3 speaker(s) on the reference track (12.3 s)"
        );
        assert!(format_stage(&PostStage::EchoChecked { elapsed: d }).contains("echo check"));
        assert!(format_stage(&PostStage::Stored { elapsed: d }).contains("stored"));
    }

    #[test]
    fn session_cleanup_prints_an_unreadable_stretch_record_once() {
        let reason = "the stretch record cannot be read: stretch record version 3".to_owned();
        let line = format_stretch_use(
            TrackId::Mic,
            &StretchUse::Transcribed {
                reason: reason.clone(),
            },
        );
        assert!(line.contains(&reason), "{line}");
        let warnings = [
            PostWarning::StretchRecordUnreadable {
                track: TrackId::Mic,
                reason,
            },
            PostWarning::SkippedVoiceprints(1),
        ];
        let printed: Vec<_> = printed_warnings(&warnings).collect();
        assert_eq!(printed, [&PostWarning::SkippedVoiceprints(1)]);
    }

    #[test]
    fn session_cleanup_says_whether_the_live_stretches_were_reused() {
        assert_eq!(
            format_stretch_use(
                TrackId::Mic,
                &StretchUse::Reused {
                    through: MeetingTime::from_millis(754_200),
                    pieces: 31,
                    backend: BackendInfo {
                        kind: BackendKind::Cuda,
                        device: "RTX 5080".into(),
                        model: "ggml-base.en".into(),
                    },
                }
            ),
            "post: mic: took 31 live speech piece(s) up to 12:34.200 (decoded on Cuda (RTX 5080, \
             ggml-base.en)), transcribed only the rest"
        );
        assert_eq!(
            format_stretch_use(
                TrackId::Reference,
                &StretchUse::Transcribed {
                    reason: "no stretch record".into()
                }
            ),
            "post: reference: transcribed the whole track (no stretch record)"
        );
    }

    #[test]
    fn session_cancelled_cleanup_deletes_the_live_speaker_windows() {
        let dir = std::env::temp_dir().join(format!("earshot-cli-forget-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let data = DataDir::new(&dir);
        data.create().unwrap();
        let db = data.db_path();
        let store = Store::open(&db).unwrap();
        let svc = Services::new(
            store,
            data,
            Arc::new(earshot_core::identity::ManualClock::new(1_760_000_000_000)),
        )
        .unwrap();
        let meeting = EntityId::from_bytes([7; 16]);
        let other = EntityId::from_bytes([8; 16]);
        // Distinctive bytes standing in for the stored embeddings.
        let secret: Vec<u8> = (0..32u8).map(|i| 0xC3 ^ i.wrapping_mul(29)).collect();
        {
            let mut store = svc.store();
            for (m, t) in [(meeting, "mic"), (meeting, "reference"), (other, "mic")] {
                store
                    .set_local_value(&format!("live-speakers/{m}/{t}"), &secret)
                    .unwrap();
            }
            store
                .set_local_value(&format!("live-stretches/{meeting}/mic"), b"words")
                .unwrap();
            store.checkpoint().unwrap();
        }
        let mut out = Vec::new();
        cleanup_cancelled(&svc, meeting, &mut out).unwrap();
        let text = String::from_utf8_lossy(&out);
        assert!(text.contains("cleanup cancelled"), "{text}");
        assert!(text.contains("deleted the speaker windows"), "{text}");
        {
            let store = svc.store();
            for t in ["mic", "reference"] {
                let key = format!("live-speakers/{meeting}/{t}");
                assert_eq!(store.local_value(&key).unwrap(), None, "{key}");
            }
            // Another meeting's windows and the live transcript's stretches stay.
            let kept = format!("live-speakers/{other}/mic");
            assert!(store.local_value(&kept).unwrap().is_some());
            let stretches = format!("live-stretches/{meeting}/mic");
            assert!(store.local_value(&stretches).unwrap().is_some());
        }
        // Gone from the file and the WAL, as far as this meeting is concerned: the other
        // meeting's copy is the only one left.
        let on_disk = |needle: &[u8]| -> usize {
            ["", "-wal"]
                .iter()
                .filter_map(|s| std::fs::read(format!("{}{s}", db.display())).ok())
                .map(|b| b.windows(needle.len()).filter(|w| *w == needle).count())
                .sum()
        };
        assert_eq!(on_disk(&secret), 1);
        drop(svc);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn session_cleanup_says_whether_the_live_speaker_windows_were_reused() {
        assert_eq!(
            format_speaker_window_use(
                TrackId::Mic,
                &SpeakerWindowUse::Reused {
                    windows: 3552,
                    stopped: None,
                }
            ),
            "post: mic: took 3552 speaker window(s) from the meeting, computed only the rest"
        );
        assert_eq!(
            format_speaker_window_use(
                TrackId::Mic,
                &SpeakerWindowUse::Reused {
                    windows: 64,
                    stopped: Some("live speaker work fell more than 180 s behind".into()),
                }
            ),
            "post: mic: took 64 speaker window(s) from the meeting, computed only the rest \
             (live speaker work fell more than 180 s behind)"
        );
        assert_eq!(
            format_speaker_window_use(
                TrackId::Reference,
                &SpeakerWindowUse::Diarised {
                    reason: "no speaker record".into()
                }
            ),
            "post: reference: found the speakers over the whole track (no speaker record)"
        );
        // Nothing to take (a meeting shorter than one batch): diarised wording.
        assert_eq!(
            format_speaker_window_use(
                TrackId::Mic,
                &SpeakerWindowUse::Reused {
                    windows: 0,
                    stopped: None,
                }
            ),
            "post: mic: found the speakers over the whole track (no speaker windows were \
             finished during the meeting)"
        );
        assert_eq!(
            format_speaker_window_use(
                TrackId::Mic,
                &SpeakerWindowUse::Reused {
                    windows: 0,
                    stopped: Some("live speaker work failed: cpu lost".into()),
                }
            ),
            "post: mic: found the speakers over the whole track (no speaker windows were \
             finished during the meeting; live speaker work failed: cpu lost)"
        );
    }

    #[test]
    fn session_cleanup_cancel_request_cancels_the_token() {
        let asked = Arc::new(AtomicBool::new(false));
        let token = CancelToken::new();
        let done = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&asked);
        let w = watch_for_cancel(
            Arc::new(move || flag.load(Ordering::SeqCst)),
            token.clone(),
            Arc::clone(&done),
        );
        asked.store(true, Ordering::SeqCst);
        assert!(w.join().unwrap(), "the watcher cancelled");
        assert!(token.is_cancelled());
        // Finishing first: nothing is cancelled.
        let token = CancelToken::new();
        let done = Arc::new(AtomicBool::new(true));
        let w = watch_for_cancel(Arc::new(|| false), token.clone(), done);
        assert!(!w.join().unwrap());
        assert!(!token.is_cancelled());
    }

    #[test]
    fn session_live_summaries_flag_overrides_the_stored_setting_for_one_run() {
        let dir =
            std::env::temp_dir().join(format!("earshot-cli-live-summaries-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mut store = Store::open(&dir.join("earshot.db")).unwrap();
        let choose = |store: &mut Store, flag, save| {
            let mut out = Vec::new();
            let on = choose_live_summaries(store, flag, save, &mut out).unwrap();
            (on, String::from_utf8(out).unwrap())
        };
        assert_eq!(
            choose(&mut store, None, false),
            (
                false,
                "summarise during the meeting: off (default)\n".to_owned()
            )
        );
        let (on, text) = choose(&mut store, Some(true), false);
        assert!(on && text.contains("for this run"), "{text}");
        assert!(!choose(&mut store, None, false).0, "not saved");
        let (on, text) = choose(&mut store, Some(true), true);
        assert!(on && text.contains("saved as the setting"), "{text}");
        assert_eq!(
            choose(&mut store, None, false),
            (
                true,
                "summarise during the meeting: on (stored setting)\n".to_owned()
            )
        );
        store
            .set_local_value(
                earshot_runtime::live_summaries::LIVE_SUMMARIES_KEY,
                b"later",
            )
            .unwrap();
        let (on, text) = choose(&mut store, None, false);
        assert!(!on && text.contains("not known to this build"), "{text}");
        drop(store);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn session_live_summary_events_say_what_the_summary_after_the_call_does() {
        let line = format_event(&RuntimeEvent::LiveSummaryMapped {
            window: 2,
            took: Duration::from_millis(41_500),
            error: None,
        })
        .unwrap();
        assert_eq!(
            line,
            "summary: window 2 summarised during the meeting (41.5 s)"
        );
        let line = format_event(&RuntimeEvent::LiveSummaryMapped {
            window: 0,
            took: Duration::ZERO,
            error: Some("load failed".into()),
        })
        .unwrap();
        assert!(
            line.contains("load failed") && line.contains("does it"),
            "{line}"
        );
        let line = format_event(&RuntimeEvent::LiveSummariesOff {
            reason: "on battery".into(),
        })
        .unwrap();
        assert!(
            line.contains("on battery") && line.contains("every window"),
            "{line}"
        );
    }

    #[test]
    fn session_speakers_flag_overrides_the_stored_setting_for_one_run() {
        let dir = std::env::temp_dir().join(format!("earshot-cli-speakers-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mut store = Store::open(&dir.join("earshot.db")).unwrap();
        let choose = |store: &mut Store, flag, save| {
            let mut out = Vec::new();
            let t = choose_speaker_timing(store, flag, save, &mut out).unwrap();
            (t, String::from_utf8(out).unwrap())
        };
        // Nothing stored: the default.
        let (t, text) = choose(&mut store, None, false);
        assert_eq!(t, SpeakerTiming::DuringCall);
        assert_eq!(text, "speakers: During the call (default)\n");
        // Saved: used by later runs without the flag, with the warning for "after".
        let (t, _) = choose(&mut store, Some(SpeakerTiming::AfterCall), true);
        assert_eq!(t, SpeakerTiming::AfterCall);
        let (t, text) = choose(&mut store, None, false);
        assert_eq!(t, SpeakerTiming::AfterCall);
        assert!(
            text.starts_with("speakers: After the call (stored setting)\n"),
            "{text}"
        );
        assert!(text.contains(AFTER_CALL_WARNING), "{text}");
        // The flag without saving: this run only.
        let (t, text) = choose(&mut store, Some(SpeakerTiming::DuringCall), false);
        assert_eq!(t, SpeakerTiming::DuringCall);
        assert!(text.contains("for this run"), "{text}");
        assert!(!text.contains(AFTER_CALL_WARNING), "{text}");
        assert_eq!(
            speaker_timing(&store).unwrap().timing(),
            SpeakerTiming::AfterCall,
            "not saved"
        );
        let (t, _) = choose(&mut store, Some(SpeakerTiming::AfterCallOnBattery), true);
        assert_eq!(t, SpeakerTiming::AfterCallOnBattery);
        assert_eq!(
            choose(&mut store, None, false).0,
            SpeakerTiming::AfterCallOnBattery
        );
        // A value this build does not know: the default, said.
        store
            .set_local_value(
                earshot_runtime::speaker_timing::SPEAKER_TIMING_KEY,
                b"after-on-solar",
            )
            .unwrap();
        let (t, text) = choose(&mut store, None, false);
        assert_eq!(t, SpeakerTiming::DuringCall);
        assert_eq!(
            text,
            "speakers: During the call (default: the stored setting \"after-on-solar\" is not \
             known to this build)\n"
        );
        drop(store);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn session_prints_why_speakers_wait_for_the_cleanup() {
        use earshot_runtime::speaker_timing::{AFTER_CALL_REASON, UNPLUGGED_REASON};
        let line = format_event(&RuntimeEvent::LiveSpeakersOff {
            reason: UNPLUGGED_REASON.into(),
        })
        .unwrap();
        assert_eq!(
            line,
            format!("speakers: {UNPLUGGED_REASON}; the cleanup finds them after the call")
        );
        // The cleanup line says why the whole track was diarised.
        let line = format_speaker_window_use(
            TrackId::Mic,
            &SpeakerWindowUse::Reused {
                windows: 0,
                stopped: Some(AFTER_CALL_REASON.into()),
            },
        );
        assert!(
            line.starts_with("post: mic: found the speakers over the whole track")
                && line.contains(AFTER_CALL_REASON),
            "{line}"
        );
    }
}
