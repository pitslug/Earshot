//! Which local summary model runs on this machine (T6.21b, D19; chosen by T6.20).
//!
//! The shipped models are [`SUMMARY_MODELS`] (manifest set `summary`): Qwen 3.5 9B (Q4_K_M)
//! by default, Qwen 3.5 4B (Q4_K_M) where memory is short. Both need Qwen 3.5's empty think
//! block as the assistant prefix ([`QWEN_NO_THINK`], the setup `cargo xtask sum-eval`
//! measured).
//!
//! # The CPU rule (T8.3f)
//!
//! A summary that runs on the CPU gets the 4B whatever the memory: T6.20
//! measured the 9B at 16.4 minutes per hour of meeting on 8 desktop threads against 11.2 for
//! the 4B and named the 4B for CPU-only machines, and on the Surface the 9B took about 4.5
//! minutes for a 6-minute replay (GATE-03a). Where it runs is decided before llama.cpp is
//! loaded ([`summary_gpu_planned`]).
//!
//! # The memory rule (on the GPU)
//!
//! The 9B runs when the physical memory Windows reports ([`earshot_store::total_memory`],
//! `GlobalMemoryStatusEx` `ullTotalPhys`) is more than [`LARGE_MODEL_MIN_MEMORY`] (12 GiB);
//! otherwise, and when the memory cannot be read, the 4B (reported as such). The plan's
//! rule is "the 9B on machines with at least 16 GB": `ullTotalPhys` is the installed memory
//! minus what firmware and an integrated GPU reserve, so a machine sold as 16 GB reports
//! less than 16 GiB (the desktop's 64 GiB reports 61.6 GiB, 2.4 GiB reserved, measured
//! 2026-10-26). Memory comes in 8, 12, 16, 24, 32 GB sizes, so "more than 12 GiB reported"
//! is every machine with more than 12 GB installed, and a 16 GB machine (the Surface target)
//! qualifies unless it reserves 4 GiB or more.
//!
//! # Where it runs
//!
//! [`summary_gpu`]: on the GPU when this build's llama.cpp has a GPU backend
//! (`earshot_llm_local::supports_gpu`) and the runtime's backend order puts CUDA or Vulkan
//! first; else on the CPU. On Vulkan a discrete GPU with room for the model is needed as well
//! (D23, [`crate::model_settings::summary_placement`]). A GPU load that fails is retried once
//! on the CPU and the refusal is reported ([`crate::summary::LocalRunReport::gpu_refused`]).
//!
//! # The graphics card's memory
//!
//! The memory rule picks the 9B only if it also fits the card
//! ([`crate::model_check::MEMORY_FACTOR`], a judgement): a 16 GB machine with an 8 GiB card
//! runs the 4B on the card, which is faster than either model on the CPU.

use crate::RuntimeError;
use crate::backends::{Accelerator, ModelDir};
use crate::model_check::MEMORY_FACTOR;
use crate::summary::LocalSummaryConfig;

#[cfg(test)]
#[path = "summary_model_tests.rs"]
mod tests;

/// Qwen 3.5's assistant prefix with an empty think block (no thinking; T6.20).
pub const QWEN_NO_THINK: &str = "<think>\n\n</think>\n\n";

/// The manifest set holding the shipped summary models (`cargo xtask fetch-models --set
/// summary`).
pub const SUMMARY_FETCH_SET: &str = "summary";

/// One GiB.
const GIB: u64 = 1 << 30;

/// The 9B runs when the reported physical memory is more than this (module docs).
pub const LARGE_MODEL_MIN_MEMORY: u64 = 12 * GIB;

/// A shipped local summary model.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SummaryModel {
    /// The manifest id, stored with each summary.
    pub id: &'static str,
    /// The GGUF file, relative to the models directory.
    pub file: &'static str,
    /// Its size in bytes (the manifest's).
    pub size: u64,
    /// Where the assistant's reply starts.
    pub assistant_prefix: &'static str,
}

/// The default model (machines with enough memory).
pub const DEFAULT_SUMMARY_MODEL: &str = "qwen3.5-9b-q4_k_m";
/// The model for machines with less memory.
pub const LOW_MEMORY_SUMMARY_MODEL: &str = "qwen3.5-4b-q4_k_m";

/// The shipped local summary models, keyed by exact manifest id (pinned to manifest set
/// `summary` by a test).
pub const SUMMARY_MODELS: &[SummaryModel] = &[
    SummaryModel {
        id: DEFAULT_SUMMARY_MODEL,
        file: "llm/qwen3.5-9b-q4_k_m.gguf",
        size: 5_680_522_464,
        assistant_prefix: QWEN_NO_THINK,
    },
    SummaryModel {
        id: LOW_MEMORY_SUMMARY_MODEL,
        file: "llm/qwen3.5-4b-q4_k_m.gguf",
        size: 2_740_937_888,
        assistant_prefix: QWEN_NO_THINK,
    },
];

/// The shipped model `id`, matched exactly.
///
/// # Errors
/// [`RuntimeError::UnknownModel`] for any other id.
pub fn summary_model(id: &str) -> Result<&'static SummaryModel, RuntimeError> {
    SUMMARY_MODELS
        .iter()
        .find(|m| m.id == id)
        .ok_or_else(|| RuntimeError::UnknownModel(id.to_owned()))
}

/// Whether `rel` is a shipped summary model's file (for the fetch instruction).
#[must_use]
pub fn is_summary_file(rel: &str) -> bool {
    SUMMARY_MODELS.iter().any(|m| m.file == rel)
}

/// Why a model was chosen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChoiceReason {
    /// The caller named it.
    Requested,
    /// The summary runs on the CPU: the low-memory model, whatever the memory (module docs).
    OnTheCpu,
    /// From the reported physical memory (bytes).
    Memory(u64),
    /// The larger model does not fit the graphics card, with this much dedicated memory.
    GraphicsMemory(u64),
    /// The memory could not be read: the low-memory model.
    MemoryUnknown,
}

/// The model for this machine and why.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SummaryModelChoice {
    /// The model.
    pub model: &'static SummaryModel,
    /// Why.
    pub reason: ChoiceReason,
}

impl SummaryModelChoice {
    /// One line for the user: the model and why.
    #[must_use]
    pub fn describe(&self) -> String {
        let why = match self.reason {
            ChoiceReason::Requested => "requested".to_owned(),
            ChoiceReason::OnTheCpu => "runs on the CPU, where the 9B is too slow".to_owned(),
            ChoiceReason::Memory(m) => format!(
                "{:.1} GiB of memory, the 9B needs more than {} GiB",
                m as f64 / GIB as f64,
                LARGE_MODEL_MIN_MEMORY / GIB
            ),
            ChoiceReason::GraphicsMemory(m) => format!(
                "{:.1} GiB of graphics memory is too little for the 9B",
                m as f64 / GIB as f64
            ),
            ChoiceReason::MemoryUnknown => "the memory could not be read".to_owned(),
        };
        format!("{} ({why})", self.model.id)
    }

    /// Why this model, for the app's Models page: models by their display names
    /// ([`crate::catalogue::display_name`]), never by manifest id (the CLI prints
    /// [`Self::describe`]).
    #[must_use]
    pub fn user_reason(&self) -> String {
        let large = crate::catalogue::display_name(DEFAULT_SUMMARY_MODEL);
        match self.reason {
            ChoiceReason::Requested => "chosen in Settings".to_owned(),
            ChoiceReason::OnTheCpu => format!("runs on the processor, where {large} is too slow"),
            ChoiceReason::Memory(m) => format!(
                "{:.1} GiB of memory; {large} needs more than {} GiB",
                m as f64 / GIB as f64,
                LARGE_MODEL_MIN_MEMORY / GIB
            ),
            ChoiceReason::GraphicsMemory(m) => format!(
                "{:.1} GiB of graphics memory; {large} needs more",
                m as f64 / GIB as f64
            ),
            ChoiceReason::MemoryUnknown => "the memory could not be read".to_owned(),
        }
    }
}

/// The model for a summary on the GPU (`gpu`, [`summary_gpu_planned`]) or the CPU and
/// `memory` (reported physical bytes, module docs), or `requested` when given.
///
/// # Errors
/// [`RuntimeError::UnknownModel`] for a requested id not in [`SUMMARY_MODELS`].
pub fn choose_summary_model(
    requested: Option<&str>,
    memory: Option<u64>,
    gpu: bool,
) -> Result<SummaryModelChoice, RuntimeError> {
    choose_summary_model_on(requested, memory, gpu, None)
}

/// [`choose_summary_model`] on a graphics card with `graphics` bytes of dedicated memory
/// (`None`: unknown, not held against a model; module docs).
///
/// # Errors
/// [`RuntimeError::UnknownModel`] for a requested id not in [`SUMMARY_MODELS`].
pub fn choose_summary_model_on(
    requested: Option<&str>,
    memory: Option<u64>,
    gpu: bool,
    graphics: Option<u64>,
) -> Result<SummaryModelChoice, RuntimeError> {
    if let Some(id) = requested {
        return Ok(SummaryModelChoice {
            model: summary_model(id)?,
            reason: ChoiceReason::Requested,
        });
    }
    let (id, reason) = match memory {
        _ if !gpu => (LOW_MEMORY_SUMMARY_MODEL, ChoiceReason::OnTheCpu),
        // On the GPU: the memory rule, then the card.
        Some(m) if m > LARGE_MODEL_MIN_MEMORY => match graphics {
            Some(g)
                if summary_model(DEFAULT_SUMMARY_MODEL)?
                    .size
                    .saturating_mul(MEMORY_FACTOR)
                    > g =>
            {
                (LOW_MEMORY_SUMMARY_MODEL, ChoiceReason::GraphicsMemory(g))
            }
            _ => (DEFAULT_SUMMARY_MODEL, ChoiceReason::Memory(m)),
        },
        Some(m) => (LOW_MEMORY_SUMMARY_MODEL, ChoiceReason::Memory(m)),
        None => (LOW_MEMORY_SUMMARY_MODEL, ChoiceReason::MemoryUnknown),
    };
    Ok(SummaryModelChoice {
        model: summary_model(id)?,
        reason,
    })
}

/// Whether the summary model goes on the GPU: this build's llama.cpp has a GPU backend and
/// `order` (the runtime's backend order) starts with CUDA or Vulkan. On Vulkan the device rule
/// comes on top ([`crate::model_settings::summary_placement`]).
#[must_use]
pub fn summary_gpu(llama_gpu: bool, order: &[Accelerator]) -> bool {
    llama_gpu && matches!(order.first(), Some(Accelerator::Cuda | Accelerator::Vulkan))
}

/// [`summary_gpu`] decided without loading llama.cpp (the model is chosen before its DLLs are
/// checked): the runtime's `cuda` feature builds llama.cpp's CUDA backend and
/// `vulkan-summaries` its Vulkan backend, so they stand for `llama_gpu`, each for the
/// accelerator it builds. `order` is the runtime's backend order
/// ([`crate::backends::accelerator_order`]).
#[must_use]
pub fn summary_gpu_planned(order: &[Accelerator]) -> bool {
    let order = &llama_order(order);
    let llama_gpu = match order.first() {
        Some(Accelerator::Cuda) => true,
        Some(Accelerator::Vulkan) => cfg!(feature = "vulkan-summaries"),
        _ => false,
    };
    summary_gpu(llama_gpu, order)
}

/// `order` as llama.cpp sees it: only a `cuda` build's llama.cpp has CUDA. In the shipped build
/// CUDA in the order is ONNX Runtime's (`crate::cuda`) and says nothing about summaries, which
/// stay on what follows it.
#[must_use]
pub fn llama_order(order: &[Accelerator]) -> Vec<Accelerator> {
    order
        .iter()
        .copied()
        .filter(|a| *a != Accelerator::Cuda || cfg!(feature = "cuda"))
        .collect()
}

/// [`summary_gpu`] for this build and machine: asks llama.cpp whether it has a GPU backend
/// (this loads llama.cpp's library) and reads [`crate::backends::accelerator_order`].
#[must_use]
pub fn summary_gpu_here() -> bool {
    summary_gpu(
        earshot_llm_local::supports_gpu(),
        &llama_order(&crate::backends::accelerator_order()),
    )
}

/// The installed file of `model` in `models`, checked against the manifest size.
///
/// # Errors
/// [`RuntimeError::ModelMissing`] (fetch set `summary`) for a missing file;
/// [`RuntimeError::ModelIncomplete`] for a file of another size.
pub fn installed(
    models: &ModelDir,
    model: &SummaryModel,
) -> Result<std::path::PathBuf, RuntimeError> {
    let path = models.require(model.file)?;
    let found = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
    if found != model.size {
        return Err(RuntimeError::ModelIncomplete {
            path,
            expected: model.size,
            found,
            fetch_set: SUMMARY_FETCH_SET,
        });
    }
    Ok(path)
}

/// The local summary settings for `choice` (its installed file, prefix and id, the T6.20
/// defaults), on the GPU when `gpu` ([`summary_gpu`]) with one CPU retry.
///
/// # Errors
/// As [`installed`].
pub fn local_summary_config(
    models: &ModelDir,
    choice: &SummaryModelChoice,
    gpu: bool,
) -> Result<LocalSummaryConfig, RuntimeError> {
    let path = installed(models, choice.model)?;
    let mut config = LocalSummaryConfig::new(path, choice.model.id);
    config.assistant_prefix = choice.model.assistant_prefix.to_owned();
    config.gpu = gpu;
    config.cpu_fallback = gpu;
    // The app's own exe in its hidden mode, when it registered one (T9.3).
    config.gpu_child = gpu.then(crate::summary_child::registered).flatten();
    config.models = Some(models.clone());
    Ok(config)
}
