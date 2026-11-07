//! On a real machine with Vulkan: with no device setting, a GPU load lands on a discrete GPU,
//! never the integrated one Vulkan may list first (T8.4aa; on the desktop `Vulkan0` for whisper
//! is the integrated Radeon).
#![cfg(feature = "vulkan")]

use earshot_llm_local::LlamaBackend;

mod common;

#[test]
#[ignore = "needs a discrete GPU, the Vulkan SDK and the summary model set: cargo xtask fetch-models --set summary; run: cargo test -p earshot-llm-local --features vulkan --test vulkan_device -- --include-ignored (Ninja and the Visual Studio x64 environment)"]
fn vulkan_load_lands_on_a_discrete_gpu() {
    assert!(
        std::env::var_os("GGML_VK_VISIBLE_DEVICES").is_none(),
        "this test is about the choice made without that setting"
    );
    let hardware = earshot_hardware::read();
    let discrete: Vec<String> = hardware
        .gpus
        .iter()
        .filter(|g| g.integrated == Some(false))
        .map(|g| g.name.clone())
        .collect();
    assert!(!discrete.is_empty(), "no discrete GPU: {:?}", hardware.gpus);

    let (_llm, report) = common::load_model();
    assert_eq!(
        report.backend,
        LlamaBackend::Vulkan,
        "{:?}",
        report.evidence
    );
    let device = report.device.expect("the load names its device");
    assert!(
        discrete.contains(&device),
        "{device} is not one of {discrete:?}: {:?}",
        report.evidence
    );
    // Exactly one device: the model is never split across two.
    let used = report
        .evidence
        .iter()
        .filter(|l| l.contains("using device"))
        .count();
    assert_eq!(used, 1, "{:?}", report.evidence);
    println!("loaded on {device}: {:?}", report.evidence);
}
