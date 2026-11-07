//! Shared helpers: the model path, a short meeting, and one schema-constrained summary run.
//!
//! Each test that loads a model is its own integration-test binary: llama.cpp's log, from
//! which the backend is read, is process-wide.

#![allow(dead_code)]

use std::path::PathBuf;

use earshot_core::asr::MeetingTime;
use earshot_core::identity::EntityId;
use earshot_core::prompt::{
    ContextBudget, MeetingInfo, PromptPlan, PromptSegment, SummaryInput, Template, plan,
    segment_numbers,
};
use earshot_core::summary::{SummaryV1, validate_reply};
use earshot_llm_local::grammar::{SummaryLimits, summary_grammar};
use earshot_llm_local::{Generation, LlamaBackend, LoadOptions, LoadReport, LocalLlm};

/// The model these tests load, relative to the models directory: the shipped low-memory
/// summary model (`models/manifest.toml`, set `summary`; PLAN D22: tests load only default
/// models, so not the Qwen2.5 1.5B spike model any more).
pub const MODEL: &str = "llm/qwen3.5-4b-q4_k_m.gguf";

/// Context and reply budget of the test request.
pub const BUDGET: ContextBudget = ContextBudget {
    context_window: 8192,
    max_output_tokens: 1536,
};

/// Models directory: `EARSHOT_MODELS`, else `%LOCALAPPDATA%\Earshot\models` (the rule of
/// `xtask::models::cache_dir`; keep them identical).
pub fn models_dir() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("EARSHOT_MODELS").filter(|v| !v.is_empty()) {
        return Some(PathBuf::from(dir));
    }
    std::env::var_os("LOCALAPPDATA")
        .filter(|v| !v.is_empty())
        .map(|base| PathBuf::from(base).join("Earshot").join("models"))
}

/// Path of a model file. Panics with the fetch instruction when it is missing (a missing
/// model is a failed precondition, never a skip).
pub fn model_path(rel: &str, set: &str) -> PathBuf {
    match models_dir().map(|d| d.join(rel)) {
        Some(p) if p.is_file() => p,
        other => panic!(
            "model not found at {}; run `cargo xtask fetch-models --set {set}` (or point \
             EARSHOT_MODELS at a directory that contains {rel})",
            other.map_or_else(
                || "<no EARSHOT_MODELS or LOCALAPPDATA>".into(),
                |p| p.display().to_string()
            )
        ),
    }
}

/// Whether this build offloads to a GPU (the `cuda` or `vulkan` feature).
pub fn gpu_build() -> bool {
    cfg!(any(feature = "cuda", feature = "vulkan"))
}

/// The llama.cpp backend this build must run on.
pub fn expected_backend() -> LlamaBackend {
    if cfg!(feature = "cuda") {
        LlamaBackend::Cuda
    } else if cfg!(feature = "vulkan") {
        LlamaBackend::Vulkan
    } else {
        LlamaBackend::Cpu
    }
}

/// Loads [`MODEL`] on the GPU when the build has one, else the CPU, and checks from
/// llama.cpp's load log that it runs on [`expected_backend`].
pub fn load_model() -> (LocalLlm, LoadReport) {
    let options = LoadOptions {
        gpu: gpu_build(),
        context_tokens: u32::try_from(BUDGET.context_window).unwrap(),
        ..LoadOptions::default()
    };
    let (llm, report) =
        LocalLlm::load(&model_path(MODEL, "summary"), &options).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(
        report.backend,
        expected_backend(),
        "evidence: {:?}",
        report.evidence
    );
    (llm, report)
}

fn id(n: u32) -> EntityId {
    format!("0193a5b0-1c00-7000-8000-{n:012x}").parse().unwrap()
}

/// A short project meeting with one decision and one action item.
pub fn meeting() -> SummaryInput {
    let lines = [
        (
            "Alice",
            "Morning all. The goal today is to decide when we ship the Android beta.",
        ),
        (
            "Bob",
            "The crash rate on the internal build is down to 0.2 percent after Tuesday's fix.",
        ),
        (
            "Alice",
            "That is under our 0.5 percent bar, so I think we can ship the beta on November 14.",
        ),
        (
            "Carol",
            "Marketing needs the store screenshots a week before, so by November 7.",
        ),
        ("Bob", "Agreed, November 14 works for engineering."),
        (
            "Alice",
            "Then it is decided: the Android beta ships on November 14.",
        ),
        (
            "Carol",
            "I will prepare the store screenshots and send them to Alice by November 7.",
        ),
        (
            "Bob",
            "One open point: do we support tablets in the beta? Nobody has tested them.",
        ),
        (
            "Alice",
            "Let's leave tablets open for now and revisit next week.",
        ),
    ];
    let segments = lines
        .iter()
        .enumerate()
        .map(|(i, (speaker, text))| PromptSegment {
            id: id(u32::try_from(i).unwrap() + 1),
            start: MeetingTime::from_millis(u64::try_from(i).unwrap() * 9_000),
            speaker: (*speaker).to_string(),
            text: (*text).to_string(),
        })
        .collect();
    SummaryInput {
        meeting: MeetingInfo {
            title: None,
            started: Some("2026-10-23 10:00".into()),
            duration_ms: 90_000,
            attendees: vec!["Alice".into(), "Bob".into(), "Carol".into()],
        },
        segments,
        notes: Vec::new(),
        template: Template::General,
        prompt_variant: Default::default(),
        style: Default::default(),
        user_name: None,
    }
}

/// One schema-constrained summary of [`meeting`]: the generation and the parsed summary
/// (`Err` holds the validation errors and the raw text).
pub fn summarise(llm: &mut LocalLlm) -> (Generation, Result<SummaryV1, String>) {
    let input = meeting();
    let PromptPlan::Single(prompt) = plan(&input, BUDGET).expect("plan") else {
        panic!("the test meeting must fit in one request");
    };
    let grammar = summary_grammar(&prompt.cites, SummaryLimits::default()).expect("grammar");
    // The ceiling, not the request's content-sized budget: the test checks that the
    // grammar completes on its own.
    let generation = llm
        .generate(&prompt.messages, &grammar, BUDGET.max_output_tokens)
        .unwrap_or_else(|e| panic!("{e}"));
    let parsed = serde_json::from_str::<serde_json::Value>(&generation.text)
        .map_err(|e| vec![e.to_string()])
        .and_then(|v| {
            validate_reply(&v, &segment_numbers(&input), &prompt.cites)
                .map(|checked| checked.summary)
                .map_err(|errors| errors.iter().map(ToString::to_string).collect())
        })
        .map_err(|list: Vec<String>| format!("{list:?} in:\n{}", generation.text));
    (generation, parsed)
}

/// One-line speed report.
pub fn speed(label: &str, report: &LoadReport, g: &Generation) -> String {
    format!(
        "{label}: backend {} | load {:.2} s | prompt {} tok in {:.2} s ({:.0} tok/s) | reply {} tok in {:.2} s ({:.1} tok/s) | stop {:?} | {}",
        report.backend.name(),
        report.load_time.as_secs_f64(),
        g.prompt_tokens,
        g.prompt_time.as_secs_f64(),
        g.prompt_tokens_per_second(),
        g.generated_tokens,
        g.generate_time.as_secs_f64(),
        g.tokens_per_second(),
        g.stop,
        report.description,
    )
}
