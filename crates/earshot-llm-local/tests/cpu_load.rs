//! A CPU load (`gpu: false`) is read as a CPU run on every build, including CUDA and Vulkan
//! builds, where llama.cpp logs `using device` for each GPU it finds even when nothing is
//! offloaded.

mod common;

use earshot_llm_local::{LlamaBackend, LoadOptions, LocalLlm};

#[test]
#[ignore = "needs the summary model set: cargo xtask fetch-models --set summary; run: cargo test -p earshot-llm-local -- --include-ignored"]
fn cpu_load_is_cpu_on_every_build() {
    let options = LoadOptions {
        gpu: false,
        context_tokens: 2048,
        ..LoadOptions::default()
    };
    let (_llm, report) = LocalLlm::load(&common::model_path(common::MODEL, "summary"), &options)
        .unwrap_or_else(|e| {
            let log = earshot_llm_local::log::since(0).unwrap_or_default();
            let evidence: Vec<&String> = log
                .iter()
                .filter(|l| {
                    l.contains("using device")
                        || l.contains("layers to GPU")
                        || l.contains("model buffer size")
                })
                .collect();
            panic!("{e}; load log evidence lines: {evidence:#?}")
        });
    eprintln!("evidence: {:?}", report.evidence);
    assert_eq!(report.backend, LlamaBackend::Cpu);
    if common::gpu_build() {
        // The case this test exists for: a GPU was listed and still nothing was offloaded.
        assert!(
            report.evidence.iter().any(|l| l.contains("using device")),
            "a GPU build lists its devices: {:?}",
            report.evidence
        );
    }
}
