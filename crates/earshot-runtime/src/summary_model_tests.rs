use super::*;

const GIB_: u64 = 1 << 30;

/// A temp directory removed on drop (std only, AGENTS.md).
struct TempDir(std::path::PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "earshot-summary-model-{tag}-{}-{}",
            std::process::id(),
            N.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn summary_model_table_is_the_manifest_summary_set() {
    let manifest: toml::Table = include_str!("../../../models/manifest.toml")
        .parse()
        .unwrap();
    let artefacts = manifest["artefact"].as_array().unwrap();
    let in_set: Vec<&str> = artefacts
        .iter()
        .filter(|a| {
            a["sets"]
                .as_array()
                .unwrap()
                .iter()
                .any(|s| s.as_str() == Some(SUMMARY_FETCH_SET))
        })
        .map(|a| a["id"].as_str().unwrap())
        .collect();
    let mut table: Vec<&str> = SUMMARY_MODELS.iter().map(|m| m.id).collect();
    let mut set = in_set.clone();
    table.sort_unstable();
    set.sort_unstable();
    assert_eq!(
        table, set,
        "the table is exactly the manifest's summary set"
    );
    for m in SUMMARY_MODELS {
        let a = artefacts
            .iter()
            .find(|a| a["id"].as_str() == Some(m.id))
            .unwrap();
        assert_eq!(a["path"].as_str(), Some(m.file), "{}", m.id);
        assert_eq!(
            a["size"].as_integer(),
            Some(i64::try_from(m.size).unwrap()),
            "{}",
            m.id
        );
        assert_eq!(a["licence"].as_str(), Some("Apache-2.0"), "{}", m.id);
        // The measured T6.20 setup: Qwen 3.5 without thinking.
        assert_eq!(m.assistant_prefix, QWEN_NO_THINK, "{}", m.id);
        assert!(is_summary_file(m.file));
    }
    assert!(!is_summary_file("llm/qwen2.5-1.5b-instruct-q4_k_m.gguf"));
}

#[test]
fn summary_model_choice_on_the_gpu_follows_the_memory_rule() {
    let pick = |m| choose_summary_model(None, m, true).unwrap();
    // A machine sold as 16 GB reports less than 16 GiB: it still gets the 9B.
    for reported in [
        16 * GIB_,
        15 * GIB_ + GIB_ / 2,
        13 * GIB_,
        12 * GIB_ + 1,
        64 * GIB_,
    ] {
        let c = pick(Some(reported));
        assert_eq!(c.model.id, DEFAULT_SUMMARY_MODEL, "{reported}");
        assert_eq!(c.reason, ChoiceReason::Memory(reported));
    }
    // 12 GiB reported or less (a 12 GB or 8 GB machine): the 4B.
    for reported in [12 * GIB_, 12 * GIB_ - 1, 8 * GIB_, 1] {
        let c = pick(Some(reported));
        assert_eq!(c.model.id, LOW_MEMORY_SUMMARY_MODEL, "{reported}");
        assert_eq!(c.reason, ChoiceReason::Memory(reported));
    }
    // Unknown memory: the 4B, and the reason says so.
    let c = pick(None);
    assert_eq!(c.model.id, LOW_MEMORY_SUMMARY_MODEL);
    assert_eq!(c.reason, ChoiceReason::MemoryUnknown);
    assert!(
        c.describe().contains("could not be read"),
        "{}",
        c.describe()
    );
    assert!(
        pick(Some(61 * GIB_)).describe().contains("61.0 GiB"),
        "{}",
        pick(Some(61 * GIB_)).describe()
    );
}

#[test]
fn summary_model_on_the_cpu_is_the_4b_whatever_the_memory() {
    // T8.3f: the 9B is too slow on a CPU (T6.20: 16.4 min per hour of
    // meeting on 8 desktop threads against 11.2 for the 4B; the Surface took about 4.5 min
    // for a 6-minute replay).
    for memory in [
        Some(64 * GIB_),
        Some(15 * GIB_ + GIB_ / 2),
        Some(12 * GIB_ + 1),
        Some(8 * GIB_),
        None,
    ] {
        let c = choose_summary_model(None, memory, false).unwrap();
        assert_eq!(c.model.id, LOW_MEMORY_SUMMARY_MODEL, "{memory:?}");
        assert_eq!(c.reason, ChoiceReason::OnTheCpu, "{memory:?}");
        assert!(c.describe().contains("on the CPU"), "{}", c.describe());
    }
    // A model asked for by name still wins on the CPU.
    let c = choose_summary_model(Some(DEFAULT_SUMMARY_MODEL), Some(8 * GIB_), false).unwrap();
    assert_eq!(
        (c.model.id, c.reason),
        (DEFAULT_SUMMARY_MODEL, ChoiceReason::Requested)
    );
}

#[test]
fn summary_model_gpu_is_planned_from_the_build_and_the_order() {
    use Accelerator::{Cpu, Cuda, Vulkan};
    // Without llama.cpp: the runtime's `cuda` feature is what builds llama.cpp's CUDA backend.
    assert_eq!(summary_gpu_planned(&[Cuda, Cpu]), cfg!(feature = "cuda"));
    // `vulkan-summaries` builds its Vulkan backend; a plain `vulkan` build (whisper only) or a
    // CUDA build does not.
    assert_eq!(
        summary_gpu_planned(&[Vulkan, Cpu]),
        cfg!(feature = "vulkan-summaries")
    );
    assert!(!summary_gpu_planned(&[Cpu]));
    assert!(!summary_gpu_planned(&[]));
    // CUDA in the order of a build whose llama.cpp has none is ONNX Runtime's: what follows decides.
    if !cfg!(feature = "cuda") {
        assert_eq!(llama_order(&[Cuda, Vulkan, Cpu]), [Vulkan, Cpu]);
        assert_eq!(
            summary_gpu_planned(&[Cuda, Vulkan, Cpu]),
            cfg!(feature = "vulkan-summaries")
        );
    }
}

#[test]
fn summary_model_requested_by_exact_id() {
    let c = choose_summary_model(Some(LOW_MEMORY_SUMMARY_MODEL), Some(64 * GIB_), true).unwrap();
    assert_eq!(
        (c.model.id, c.reason),
        (LOW_MEMORY_SUMMARY_MODEL, ChoiceReason::Requested)
    );
    for bad in [
        "qwen3.5-9b",
        "QWEN3.5-9B-Q4_K_M",
        "qwen2.5-1.5b-instruct-q4_k_m",
        "",
    ] {
        assert!(
            matches!(
                choose_summary_model(Some(bad), None, true),
                Err(RuntimeError::UnknownModel(_))
            ),
            "{bad}"
        );
    }
}

#[test]
fn summary_model_gpu_only_when_llama_has_one_and_a_gpu_leads() {
    use Accelerator::{Cpu, Cuda, Qnn, Vulkan};
    assert!(summary_gpu(true, &[Cuda, Cpu]));
    assert!(!summary_gpu(false, &[Cuda, Cpu]));
    assert!(summary_gpu(true, &[Vulkan, Cpu]));
    assert!(!summary_gpu(false, &[Vulkan, Cpu]));
    assert!(summary_gpu(true, &[Cuda, Vulkan, Cpu]));
    // The Surface: the NPU leads, and summaries never use it.
    assert!(!summary_gpu(true, &[Qnn, Vulkan, Cpu]));
    assert!(!summary_gpu(true, &[Cpu]));
    assert!(!summary_gpu(true, &[]));
}

#[test]
fn summary_model_respects_the_graphics_cards_memory() {
    let on = |graphics| choose_summary_model_on(None, Some(32 * GIB_), true, graphics).unwrap();
    // The 9B needs twice its 5.3 GiB file.
    let c = on(Some(8 * GIB_));
    assert_eq!(c.model.id, LOW_MEMORY_SUMMARY_MODEL);
    assert_eq!(c.reason, ChoiceReason::GraphicsMemory(8 * GIB_));
    assert!(c.describe().contains("8.0 GiB of graphics memory"));
    assert!(c.user_reason().contains("8.0 GiB of graphics memory"));
    assert_eq!(on(Some(16 * GIB_)).model.id, DEFAULT_SUMMARY_MODEL);
    // Unknown memory is not held against the model.
    assert_eq!(on(None).model.id, DEFAULT_SUMMARY_MODEL);
    // The exact edge of the rule.
    let size = summary_model(DEFAULT_SUMMARY_MODEL).unwrap().size;
    assert_eq!(on(Some(size * 2)).model.id, DEFAULT_SUMMARY_MODEL);
    assert_eq!(on(Some(size * 2 - 1)).model.id, LOW_MEMORY_SUMMARY_MODEL);
    // Without room in main memory or on the CPU, the card changes nothing.
    let c = choose_summary_model_on(None, Some(8 * GIB_), true, Some(16 * GIB_)).unwrap();
    assert_eq!(c.reason, ChoiceReason::Memory(8 * GIB_));
    let c = choose_summary_model_on(None, Some(32 * GIB_), false, Some(4 * GIB_)).unwrap();
    assert_eq!(c.reason, ChoiceReason::OnTheCpu);
    // A named model is the user's choice; the placement checks whether it fits.
    let c = choose_summary_model_on(Some(DEFAULT_SUMMARY_MODEL), None, true, Some(GIB_)).unwrap();
    assert_eq!(c.reason, ChoiceReason::Requested);
}

#[test]
fn summary_model_config_needs_the_whole_installed_file() {
    let dir = TempDir::new("installed");
    let models = ModelDir::new(&dir.0);
    let choice = choose_summary_model(Some(LOW_MEMORY_SUMMARY_MODEL), None, false).unwrap();
    // Missing: names the summary fetch set.
    let e = local_summary_config(&models, &choice, false).unwrap_err();
    assert!(
        matches!(
            &e,
            RuntimeError::ModelMissing {
                fetch_set: Some("summary"),
                ..
            }
        ),
        "{e:?}"
    );
    assert!(
        e.to_string()
            .contains("cargo xtask fetch-models --set summary"),
        "{e}"
    );
    // Present but not the manifest size (an interrupted copy): refused.
    let path = dir.0.join(choice.model.file);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, b"short").unwrap();
    let e = local_summary_config(&models, &choice, false).unwrap_err();
    assert!(
        matches!(&e, RuntimeError::ModelIncomplete { found: 5, .. }),
        "{e:?}"
    );
    assert!(
        e.to_string()
            .contains("cargo xtask fetch-models --set summary"),
        "{e}"
    );
    // The right size (a sparse file is enough for the check).
    let f = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
    f.set_len(choice.model.size).unwrap();
    drop(f);
    let cpu = local_summary_config(&models, &choice, false).unwrap();
    assert_eq!(cpu.model_path, path);
    assert_eq!(cpu.model_id, LOW_MEMORY_SUMMARY_MODEL);
    assert_eq!(cpu.assistant_prefix, QWEN_NO_THINK);
    assert!(!cpu.gpu && !cpu.cpu_fallback);
    // The T6.20 defaults.
    assert_eq!(
        (cpu.context_tokens, cpu.max_output_tokens, cpu.chunk_seconds),
        (16_384, 8192, 600)
    );
    // On the GPU, with one CPU retry.
    let gpu = local_summary_config(&models, &choice, true).unwrap();
    assert!(gpu.gpu && gpu.cpu_fallback);
}

/// T9.23: on a processor without the native builds' baseline llama.cpp is never asked (its
/// first call starts ggml's AVX2 CPU backend): summaries run on the CPU.
#[test]
fn summary_gpu_here_never_asks_llama_on_a_processor_without_the_baseline() {
    assert!(!summary_gpu_here_with(
        false,
        || panic!("llama.cpp asked on a processor without AVX2"),
        || panic!("whisper.cpp asked on a processor without AVX2"),
    ));
    assert!(summary_gpu_here_with(
        true,
        || true,
        || vec![Accelerator::Vulkan, Accelerator::Cpu]
    ));
    assert!(!summary_gpu_here_with(
        true,
        || false,
        || vec![Accelerator::Vulkan, Accelerator::Cpu]
    ));
}
