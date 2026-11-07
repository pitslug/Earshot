use earshot_hardware::{Cpu, Hardware};

use super::*;
use crate::model_bench::{StoredTrial, hardware_facts};
use crate::summary_model::{ChoiceReason, LOW_MEMORY_SUMMARY_MODEL, choose_summary_model};

const GIB: u64 = 1 << 30;

fn machine(memory: u64, cores: usize) -> Hardware {
    Hardware {
        cpu: Cpu {
            name: Some("Test CPU".into()),
            physical_cores: Some(cores),
            logical_cores: Some(cores),
        },
        total_memory: Some(memory),
        ..Hardware::default()
    }
}

const CPU: &[Accelerator] = &[Accelerator::Cpu];
const SURFACE: &[Accelerator] = &[Accelerator::Qnn, Accelerator::Cpu];
const DESKTOP: &[Accelerator] = &[Accelerator::Cuda, Accelerator::Cpu];
const VULKAN: &[Accelerator] = &[Accelerator::Vulkan, Accelerator::Cpu];

#[test]
fn hardware_recommend_english_starts_at_the_best_rung_that_fits() {
    let r = recommend_transcription(&machine(16 * GIB, 12), CPU, &[]).unwrap();
    assert_eq!(r.model, "parakeet-tdt-0.6b-v2");
    assert_eq!(r.backend, BackendKind::Cpu);
    assert_eq!(
        r.reason,
        "the most accurate English model for the processor"
    );
    // 4 GiB cannot hold the 0.6B twice over: the next rung, and the reason says why.
    let r = recommend_transcription(&machine(4 * GIB, 12), CPU, &["en"]).unwrap();
    assert_eq!(r.model, "parakeet-tdt-110m");
    assert!(
        r.reason.contains("Parakeet 0.6B v2 needs more memory"),
        "{}",
        r.reason
    );
    // Two cores give one thread per track: too few for a model over 1 GB.
    let r = recommend_transcription(&machine(16 * GIB, 2), CPU, &[]).unwrap();
    assert_eq!(r.model, "parakeet-tdt-110m");
    // Unknown facts are not held against a model.
    let r = recommend_transcription(&Hardware::default(), CPU, &[]).unwrap();
    assert_eq!(r.model, "parakeet-tdt-110m", "unknown cores count as one");
    // The NPU leads on the Surface, CUDA on the desktop.
    let r = recommend_transcription(&machine(16 * GIB, 12), SURFACE, &[]).unwrap();
    assert_eq!(
        (r.model, r.backend),
        ("parakeet-tdt-0.6b-v2", BackendKind::Qnn)
    );
    assert_eq!(r.reason, "the most accurate English model for the NPU");
    let r = recommend_transcription(&machine(64 * GIB, 32), DESKTOP, &["en-GB"]).unwrap();
    assert_eq!(
        (r.model, r.backend),
        ("parakeet-tdt-0.6b-v2", BackendKind::Cuda)
    );
}

#[test]
fn hardware_recommend_follows_the_meeting_languages() {
    let hw = machine(16 * GIB, 12);
    // Any European language, English included: Parakeet v3 (T7.4 round 3).
    let r = recommend_transcription(&hw, SURFACE, &["en", "de"]).unwrap();
    assert_eq!((r.model, r.backend), (EUROPEAN_MODEL, BackendKind::Cpu));
    assert_eq!(
        r.reason,
        "the most accurate multilingual model for the processor"
    );
    // Another language: no ladder here covers it, so Whisper large-v3-turbo.
    let r = recommend_transcription(&hw, SURFACE, &["ja"]).unwrap();
    assert_eq!(
        (r.model, r.backend),
        (EVERY_LANGUAGE_MODEL, BackendKind::Cpu)
    );
    assert_eq!(r.reason, "the most accurate model that transcribes ja");
    // On CUDA the multilingual ladder holds it.
    let r = recommend_transcription(&machine(64 * GIB, 32), DESKTOP, &["ja", "en"]).unwrap();
    assert_eq!(
        (r.model, r.backend),
        (EVERY_LANGUAGE_MODEL, BackendKind::Cuda)
    );
    // A language no model covers.
    assert_eq!(recommend_transcription(&hw, SURFACE, &["xx"]), None);
}

#[test]
fn hardware_recommend_keeps_vulkan_only_for_a_more_accurate_model() {
    // Vulkan's English ladder starts at small.en; the processor's Parakeet is more accurate.
    let r = recommend_transcription(&machine(16 * GIB, 12), VULKAN, &[]).unwrap();
    assert_eq!(
        (r.model, r.backend),
        ("parakeet-tdt-0.6b-v2", BackendKind::Cpu)
    );
    assert!(
        r.reason
            .contains("more accurate than Whisper small.en on the graphics card"),
        "{}",
        r.reason
    );
    // The multilingual ladders: large-v3-turbo on Vulkan, v3 on the CPU (more accurate).
    let r = recommend_transcription(&machine(16 * GIB, 12), VULKAN, &["fr"]).unwrap();
    assert_eq!((r.model, r.backend), (EUROPEAN_MODEL, BackendKind::Cpu));
}

#[test]
fn hardware_runs_on_follows_the_build_and_the_ladders() {
    let on = |m, order| runs_on(m, order);
    assert_eq!(on("parakeet-tdt-0.6b-v2", DESKTOP), BackendKind::Cuda);
    assert_eq!(on(EVERY_LANGUAGE_MODEL, DESKTOP), BackendKind::Cuda);
    assert_eq!(on(EUROPEAN_MODEL, DESKTOP), BackendKind::Cuda);
    // The NPU runs only the converted English Parakeet models.
    assert_eq!(on("parakeet-tdt-0.6b-v2", SURFACE), BackendKind::Qnn);
    assert_eq!(on("parakeet-tdt-110m", SURFACE), BackendKind::Qnn);
    assert_eq!(on(EUROPEAN_MODEL, SURFACE), BackendKind::Cpu);
    assert_eq!(on(EVERY_LANGUAGE_MODEL, SURFACE), BackendKind::Cpu);
    assert_eq!(on("base.en", SURFACE), BackendKind::Cpu);
    // Vulkan runs whisper only.
    assert_eq!(on("small.en", VULKAN), BackendKind::Vulkan);
    assert_eq!(on(EVERY_LANGUAGE_MODEL, VULKAN), BackendKind::Vulkan);
    assert_eq!(on("parakeet-tdt-0.6b-v2", VULKAN), BackendKind::Cpu);
    assert_eq!(on("not-a-model", DESKTOP), BackendKind::Cpu);
    assert_eq!(on("parakeet-tdt-0.6b-v2", CPU), BackendKind::Cpu);
}

#[test]
fn hardware_choices_are_the_four_of_t7_4() {
    let hw = machine(16 * GIB, 12);
    let choices = transcription_choices(&hw, SURFACE, None);
    let got: Vec<(ChoiceRole, &str, BackendKind, CheckVerdict)> = choices
        .iter()
        .map(|c| (c.role, c.model, c.runs_on, c.check))
        .collect();
    assert_eq!(
        got,
        [
            (
                ChoiceRole::Recommended,
                "parakeet-tdt-0.6b-v2",
                BackendKind::Qnn,
                CheckVerdict::Unknown
            ),
            (
                ChoiceRole::Lighter,
                "parakeet-tdt-110m",
                BackendKind::Qnn,
                CheckVerdict::Unknown
            ),
            (
                ChoiceRole::European,
                EUROPEAN_MODEL,
                BackendKind::Cpu,
                CheckVerdict::Unknown
            ),
            (
                ChoiceRole::EveryLanguage,
                EVERY_LANGUAGE_MODEL,
                BackendKind::Cpu,
                CheckVerdict::Unknown
            ),
        ]
    );
    for c in &choices {
        let f = model_check::model_facts(c.model).unwrap();
        assert_eq!(c.download_size, f.size);
        assert_eq!(c.languages, f.languages);
    }
    // A Vulkan build: the recommendation keeps the processor's Parakeet over Vulkan's
    // small.en, and the choices agree with it (the lighter one is the processor's next rung).
    let vulkan: Vec<(ChoiceRole, &str, BackendKind)> = transcription_choices(&hw, VULKAN, None)
        .iter()
        .map(|c| (c.role, c.model, c.runs_on))
        .collect();
    let r = recommend_transcription(&hw, VULKAN, &[]).unwrap();
    assert_eq!(
        vulkan[0],
        (ChoiceRole::Recommended, r.model, BackendKind::Cpu)
    );
    assert_eq!(
        vulkan[..2],
        [
            (
                ChoiceRole::Recommended,
                "parakeet-tdt-0.6b-v2",
                BackendKind::Cpu
            ),
            (ChoiceRole::Lighter, "parakeet-tdt-110m", BackendKind::Cpu),
        ]
    );
    // Every build's recommended choice is the recommendation itself.
    for order in [CPU, SURFACE, DESKTOP, VULKAN] {
        let first = &transcription_choices(&hw, order, None)[0];
        let r = recommend_transcription(&hw, order, &[]).unwrap();
        assert_eq!(
            (first.model, first.runs_on),
            (r.model, r.backend),
            "{order:?}"
        );
    }
    // At the bottom of the ladder there is no lighter model.
    let tiny = transcription_choices(&machine(GIB / 4, 12), CPU, None);
    assert_eq!(tiny[0].model, "base.en");
    assert!(tiny.iter().all(|c| c.role != ChoiceRole::Lighter));
    assert_eq!(tiny.len(), 3);
}

#[test]
fn hardware_too_slow_comes_only_from_a_stored_trial() {
    let hw = machine(16 * GIB, 12);
    let trial = |model: &str, backend, verdict| StoredTrial {
        model: model.into(),
        backend,
        verdict,
    };
    let check = StoredCheck {
        fingerprint: "fp".into(),
        model: "parakeet-tdt-110m".into(),
        backend: BackendKind::Qnn,
        fallback: None,
        trials: vec![
            trial(
                "parakeet-tdt-0.6b-v2",
                BackendKind::Qnn,
                StoredVerdict::TooSlow,
            ),
            trial("parakeet-tdt-110m", BackendKind::Qnn, StoredVerdict::Pass),
            trial(EUROPEAN_MODEL, BackendKind::Qnn, StoredVerdict::Wrong),
        ],
    };
    let choices = transcription_choices(&hw, SURFACE, Some(&check));
    let verdicts: Vec<CheckVerdict> = choices.iter().map(|c| c.check).collect();
    // v3 failed on the NPU, but here it runs on the processor, where it was not tested.
    assert_eq!(
        verdicts,
        [
            CheckVerdict::TooSlow,
            CheckVerdict::KeepsUp,
            CheckVerdict::Unknown,
            CheckVerdict::Unknown
        ]
    );
    assert_eq!(
        check_verdict(Some(&check), EUROPEAN_MODEL, BackendKind::Qnn),
        CheckVerdict::Fails
    );
    assert_eq!(
        check_verdict(None, "parakeet-tdt-0.6b-v2", BackendKind::Qnn),
        CheckVerdict::Unknown
    );
    // The recommendation itself does not change: the check only confirms or steps down.
    let r = recommend_transcription(&hw, SURFACE, &[]).unwrap();
    assert_eq!(r.model, "parakeet-tdt-0.6b-v2");
}

#[test]
fn hardware_summary_follows_the_memory_rule_and_where_it_runs() {
    let hw = machine(16 * GIB, 12);
    let advice = advise(&hw, CPU, &[], None).unwrap();
    assert!(!advice.summary_on_gpu);
    assert_eq!(advice.summary.model.id, LOW_MEMORY_SUMMARY_MODEL);
    assert_eq!(advice.summary.reason, ChoiceReason::OnTheCpu);
    for order in [CPU, SURFACE, DESKTOP] {
        let advice = advise(&hw, order, &[], None).unwrap();
        let gpu = summary_model::summary_gpu_planned(order);
        assert_eq!(advice.summary_on_gpu, gpu);
        assert_eq!(
            advice.summary,
            choose_summary_model(None, hw.total_memory, gpu).unwrap()
        );
    }
    let advice = advise(&hw, SURFACE, &["de"], None).unwrap();
    assert_eq!(advice.transcription.unwrap().model, EUROPEAN_MODEL);
    assert_eq!(advice.choices.len(), 4);
}

#[test]
fn hardware_facts_of_this_host_feed_the_recommendation() {
    let hw = earshot_hardware::read();
    let order = crate::backends::accelerator_order();
    println!("hardware: {hw:#?}");
    println!("accelerator order of this build: {order:?}");
    let facts = facts_for(&hw, &order);
    let check = hardware_facts();
    // The same accelerators and memory reading as the model check's own facts.
    assert_eq!(facts.accelerators, check.accelerators);
    // One core count for the advice and the check (T7.4d review).
    assert_eq!(facts.cores, check.cores, "one core count");
    assert_eq!(
        hw.total_memory,
        earshot_store::total_memory(),
        "one memory reading"
    );
    let advice = advise(&hw, &order, &[], None).unwrap();
    println!("recommendation (English): {:?}", advice.transcription);
    for c in &advice.choices {
        println!(
            "choice {:?}: {} on {:?}, {} bytes",
            c.role, c.model, c.runs_on, c.download_size
        );
    }
    println!(
        "summary: {} (on the GPU: {})",
        advice.summary.describe(),
        advice.summary_on_gpu
    );
    assert!(advice.transcription.is_some());
}

#[test]
#[cfg_attr(
    not(windows),
    ignore = "earshot-hardware reads real facts only on Windows"
)]
fn hardware_facts_of_this_windows_host_have_cores_and_memory() {
    let hw = earshot_hardware::read();
    assert!(hw.cpu.logical_cores.is_some_and(|n| n > 0), "{hw:?}");
    assert!(hw.total_memory.is_some(), "{hw:?}");
}

#[test]
fn hardware_recommend_reasons_name_models_by_display_name() {
    let languages: [&[&str]; 4] = [&[], &["en"], &["de"], &["ja"]];
    for order in [CPU, SURFACE, DESKTOP, VULKAN] {
        for memory in [2 * GIB, 4 * GIB, 8 * GIB, 16 * GIB, 64 * GIB] {
            for cores in [1, 2, 4, 12] {
                for langs in languages {
                    let hw = machine(memory, cores);
                    if let Some(r) = recommend_transcription(&hw, order, langs) {
                        assert_eq!(
                            crate::catalogue::manifest_id_in(&r.reason),
                            None,
                            "{}",
                            r.reason
                        );
                    }
                    for gpu in [false, true] {
                        let c = choose_summary_model(None, Some(memory), gpu).unwrap();
                        let why = c.user_reason();
                        assert_eq!(crate::catalogue::manifest_id_in(&why), None, "{why}");
                    }
                }
            }
        }
    }
    let unknown = choose_summary_model(None, None, false)
        .unwrap()
        .user_reason();
    assert_eq!(
        unknown,
        "runs on the processor, where Qwen 3.5 9B is too slow"
    );
    let big = choose_summary_model(None, Some(64 * GIB), true)
        .unwrap()
        .user_reason();
    assert_eq!(
        big,
        "64.0 GiB of memory; Qwen 3.5 9B needs more than 12 GiB"
    );
}
