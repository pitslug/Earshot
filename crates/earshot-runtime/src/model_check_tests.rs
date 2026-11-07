//! Model check rule tests. Every name contains `model_check`.

use earshot_core::asr::BackendKind;

use super::*;

const GB: u64 = 1_000_000_000;

fn desktop() -> HardwareFacts {
    HardwareFacts {
        accelerators: vec![Accelerator {
            backend: BackendKind::Cuda,
            free_memory: Some(14 * GB),
        }],
        cores: 32,
        memory: 64 * GB,
        drivers: Some(Vec::new()),
    }
}

fn laptop(cores: usize, memory_gb: u64) -> HardwareFacts {
    HardwareFacts {
        accelerators: Vec::new(),
        cores,
        memory: memory_gb * GB,
        drivers: Some(Vec::new()),
    }
}

fn trial(model: &'static str, backend: BackendKind, rtf: f64, wer: f64) -> Trial {
    Trial {
        model,
        backend,
        rtf,
        wer_percent: wer,
    }
}

/// Runs the search to the end with `measure` standing in for the timed run.
fn search(
    facts: &HardwareFacts,
    languages: &[&str],
    installed: &dyn Fn(&str) -> bool,
    measure: &dyn Fn(&'static str, BackendKind) -> (f64, f64),
) -> (Option<Recommendation>, Vec<(&'static str, BackendKind)>) {
    let mut s = ModelSearch::new(facts, languages, installed);
    let mut tested = Vec::new();
    for _ in 0..20 {
        match s.next() {
            Step::Test { model, backend } => {
                tested.push((model, backend));
                let (rtf, wer) = measure(model, backend);
                s.record(trial(model, backend, rtf, wer));
            }
            Step::Done(r) => return (r, tested),
        }
    }
    panic!("the search did not settle: {tested:?}");
}

fn all(_: &str) -> bool {
    true
}

#[test]
fn model_check_ladders_and_models_match_the_manifest() {
    let manifest: toml::Table = include_str!("../../../models/manifest.toml")
        .parse()
        .unwrap();
    let sizes: std::collections::BTreeMap<&str, u64> = manifest["artefact"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| {
            (
                a["id"].as_str().unwrap(),
                u64::try_from(a["size"].as_integer().unwrap()).unwrap(),
            )
        })
        .collect();
    let models = manifest["model"].as_array().unwrap();
    assert_eq!(models.len(), MODELS.len());
    for m in models {
        let name = m["name"].as_str().unwrap();
        let facts = model_facts(name).unwrap_or_else(|| panic!("{name} missing"));
        let langs: Vec<&str> = m["languages"]
            .as_array()
            .unwrap()
            .iter()
            .map(|l| l.as_str().unwrap())
            .collect();
        assert_eq!(facts.languages, langs.as_slice(), "{name} languages");
        let size: u64 = m["artefacts"]
            .as_array()
            .unwrap()
            .iter()
            .map(|a| sizes[a.as_str().unwrap()])
            .sum();
        assert_eq!(facts.size, size, "{name} size");
    }
    let ladders = manifest["ladder"].as_array().unwrap();
    assert_eq!(ladders.len(), LADDERS.len());
    for l in ladders {
        let backend = match l["backend"].as_str().unwrap() {
            "cuda" => BackendKind::Cuda,
            "vulkan" => BackendKind::Vulkan,
            "qnn" => BackendKind::Qnn,
            "cpu" => BackendKind::Cpu,
            other => panic!("ladder backend {other} has no mirror here yet"),
        };
        let scope = match l["scope"].as_str().unwrap() {
            "english" => Scope::English,
            "multilingual" => Scope::Multilingual,
            other => panic!("unknown ladder scope {other}"),
        };
        let names: Vec<&str> = l["models"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m.as_str().unwrap())
            .collect();
        let mirrors: Vec<&Ladder> = LADDERS
            .iter()
            .filter(|x| x.backend == backend && x.scope == scope)
            .collect();
        assert_eq!(mirrors.len(), 1, "one mirror per backend and scope");
        assert_eq!(mirrors[0].models, names.as_slice());
    }
    // Every ladder model loads by name in the runtime.
    for l in LADDERS {
        for m in l.models {
            assert!(crate::backends::asr_model(m).is_ok(), "{m}");
        }
    }
}

#[test]
fn model_check_languages_filter_the_candidates() {
    assert_eq!(
        candidates(BackendKind::Cuda, &[]),
        ["parakeet-tdt-0.6b-v2", "parakeet-tdt-110m", "base.en"]
    );
    assert_eq!(
        candidates(BackendKind::Cuda, &["en"]),
        candidates(BackendKind::Cuda, &[])
    );
    // German: both multilingual GPU models cover it.
    assert_eq!(
        candidates(BackendKind::Cuda, &["de"]),
        ["parakeet-tdt-0.6b-v3", "large-v3-turbo"]
    );
    // English and German together: the multilingual ladder.
    assert_eq!(
        candidates(BackendKind::Cuda, &["en", "de"]),
        ["parakeet-tdt-0.6b-v3", "large-v3-turbo"]
    );
    // Japanese is outside Parakeet v3: only turbo.
    assert_eq!(candidates(BackendKind::Cuda, &["ja"]), ["large-v3-turbo"]);
    // On the CPU nothing measured covers Japanese.
    assert!(candidates(BackendKind::Cpu, &["ja"]).is_empty());
    // A language no model covers.
    assert!(candidates(BackendKind::Cuda, &["xx"]).is_empty());
    // Vulkan runs whisper only (T6.10d).
    assert_eq!(
        candidates(BackendKind::Vulkan, &[]),
        ["small.en", "base.en"]
    );
    assert_eq!(candidates(BackendKind::Vulkan, &["de"]), ["large-v3-turbo"]);
    assert_eq!(candidates(BackendKind::Vulkan, &["ja"]), ["large-v3-turbo"]);
    // The NPU runs the converted Parakeet models, English only (T8.3e).
    assert_eq!(
        candidates(BackendKind::Qnn, &[]),
        ["parakeet-tdt-0.6b-v2", "parakeet-tdt-110m"]
    );
    assert!(candidates(BackendKind::Qnn, &["de"]).is_empty());
}

#[test]
fn model_check_hardware_picks_the_starting_rung() {
    let en = candidates(BackendKind::Cpu, &[]);
    // The desktop GPU starts at the top.
    let gpu = candidates(BackendKind::Cuda, &[]);
    assert_eq!(starting_rung(&desktop(), BackendKind::Cuda, &gpu), Some(0));
    // A small GPU (2 GB free) cannot hold the 0.6B model with headroom: start at 110M.
    let small_gpu = HardwareFacts {
        accelerators: vec![Accelerator {
            backend: BackendKind::Cuda,
            free_memory: Some(2 * GB),
        }],
        ..desktop()
    };
    assert_eq!(starting_rung(&small_gpu, BackendKind::Cuda, &gpu), Some(1));
    // A 12-core 16 GB laptop (the Surface: 3 threads per track) starts at the top on the CPU.
    assert_eq!(
        starting_rung(&laptop(12, 16), BackendKind::Cpu, &en),
        Some(0)
    );
    // A 4-core laptop (1 thread per track) starts below the large model.
    assert_eq!(
        starting_rung(&laptop(4, 16), BackendKind::Cpu, &en),
        Some(1)
    );
    // 4 GB of memory cannot hold the 0.6B model twice over either.
    assert_eq!(
        starting_rung(&laptop(12, 4), BackendKind::Cpu, &en),
        Some(1)
    );
    // Nothing fits: the last rung.
    assert_eq!(starting_rung(&laptop(2, 0), BackendKind::Cpu, &en), Some(2));
    assert_eq!(starting_rung(&laptop(8, 8), BackendKind::Cpu, &[]), None);
    // Backends: the GPU first, the CPU always last; an accelerator without a ladder is skipped.
    assert_eq!(
        backend_order(&desktop()),
        [BackendKind::Cuda, BackendKind::Cpu]
    );
    let unladdered = HardwareFacts {
        accelerators: vec![Accelerator {
            backend: BackendKind::Fake,
            free_memory: None,
        }],
        ..laptop(12, 16)
    };
    assert_eq!(backend_order(&unladdered), [BackendKind::Cpu]);
}

#[test]
fn model_check_a_pair_tested_twice_keeps_its_first_verdict() {
    let t = |m: &'static str, b, r: u8| (m, b, r);
    let trials = [
        t("base.en", BackendKind::Cpu, 1),
        t("base.en", BackendKind::Cuda, 2),
        t("base.en", BackendKind::Cpu, 3),
    ];
    assert_eq!(pair_result(trials, "base.en", BackendKind::Cpu), Some(1));
    assert_eq!(pair_result(trials, "base.en", BackendKind::Cuda), Some(2));
    assert_eq!(pair_result(trials, "small.en", BackendKind::Cpu), None);

    // The search judges by the first trial too: a later slow re-run of the passing rung
    // does not undo the answer.
    let facts = laptop(4, 16);
    let mut s = ModelSearch::new(&facts, &[], &all);
    let Step::Test { model, backend } = s.next() else {
        panic!("a first test")
    };
    s.record(trial(model, backend, 0.3, 5.0));
    let before = s.next();
    s.record(trial(model, backend, 0.9, 5.0));
    assert_eq!(s.next(), before);
    assert_eq!(
        s.verdict(backend, model),
        Some(Verdict::Pass { comfortable: false })
    );
}

#[test]
fn model_check_verdicts_follow_the_fixed_limits() {
    let t = |rtf, wer| judge(&trial("base.en", BackendKind::Cpu, rtf, wer));
    assert_eq!(t(0.1, 5.0), Verdict::Pass { comfortable: true });
    assert_eq!(t(RTF_COMFORTABLE, 5.0), Verdict::Pass { comfortable: true });
    assert_eq!(t(0.3, 5.0), Verdict::Pass { comfortable: false });
    assert_eq!(t(RTF_LIMIT, 5.0), Verdict::Pass { comfortable: false });
    assert_eq!(t(0.6, 5.0), Verdict::TooSlow);
    assert_eq!(t(0.1, 30.0), Verdict::Wrong);
    // Wrong wins over slow: garbage is a broken backend whatever the speed.
    assert_eq!(t(0.9, 90.0), Verdict::Wrong);
    assert_eq!(t(f64::NAN, 5.0), Verdict::TooSlow);
    assert_eq!(t(0.1, f64::NAN), Verdict::Wrong);
}

#[test]
fn model_check_desktop_settles_on_the_top_gpu_rung_in_one_test() {
    // The top rung passes comfortably and there is nothing above it: one test.
    let (r, tested) = search(&desktop(), &[], &all, &|_, _| (0.01, 3.0));
    assert_eq!(tested, [("parakeet-tdt-0.6b-v2", BackendKind::Cuda)]);
    let r = r.unwrap();
    assert_eq!(
        (r.model, r.backend, r.fallback),
        (
            "parakeet-tdt-0.6b-v2",
            BackendKind::Cuda,
            Some("parakeet-tdt-110m")
        )
    );
    assert!(r.missing.is_empty());
}

#[test]
fn model_check_steps_up_while_comfortable_and_down_when_too_slow() {
    // A 4-core laptop starts at the 110M rung; it is comfortable, so the 0.6B is tried; that is
    // too slow, so the 110M is the answer and base.en its fallback.
    let speed = |m: &str, _| match m {
        "parakeet-tdt-0.6b-v2" => (0.8, 3.0),
        "parakeet-tdt-110m" => (0.1, 5.0),
        _ => (0.05, 8.0),
    };
    let (r, tested) = search(&laptop(4, 16), &[], &all, &speed);
    assert_eq!(
        tested,
        [
            ("parakeet-tdt-110m", BackendKind::Cpu),
            ("parakeet-tdt-0.6b-v2", BackendKind::Cpu)
        ]
    );
    let r = r.unwrap();
    assert_eq!(
        (r.model, r.fallback),
        ("parakeet-tdt-110m", Some("base.en"))
    );
    // Passing but not comfortable: settled at once, no step up.
    let (r, tested) = search(&laptop(4, 16), &[], &all, &|_, _| (0.4, 5.0));
    assert_eq!(tested.len(), 1);
    assert_eq!(r.unwrap().model, "parakeet-tdt-110m");
    // Too slow everywhere above base.en: steps down to it.
    let slow = |m: &str, _| {
        if m == "base.en" {
            (0.3, 8.0)
        } else {
            (0.9, 4.0)
        }
    };
    let (r, tested) = search(&laptop(12, 16), &[], &all, &slow);
    assert_eq!(tested.len(), 3);
    let r = r.unwrap();
    assert_eq!((r.model, r.fallback), ("base.en", None));
}

#[test]
fn model_check_wrong_output_moves_to_the_next_backend() {
    // CUDA produces garbage (a broken driver): the search does not try smaller CUDA models
    // but goes to the CPU ladder, which starts at the top on the desktop.
    let measure = |_: &str, b| {
        if b == BackendKind::Cuda {
            (0.01, 95.0)
        } else {
            (0.05, 3.0)
        }
    };
    let (r, tested) = search(&desktop(), &[], &all, &measure);
    assert_eq!(
        tested,
        [
            ("parakeet-tdt-0.6b-v2", BackendKind::Cuda),
            ("parakeet-tdt-0.6b-v2", BackendKind::Cpu)
        ]
    );
    let r = r.unwrap();
    assert_eq!(
        (r.model, r.backend),
        ("parakeet-tdt-0.6b-v2", BackendKind::Cpu)
    );
    // Nothing works anywhere: no recommendation.
    let (r, _) = search(&desktop(), &[], &all, &|_, _| (0.01, 99.0));
    assert!(r.is_none());
    // A backend where every rung is too slow also hands over to the next one.
    let measure = |_: &str, b| {
        if b == BackendKind::Cuda {
            (0.9, 3.0)
        } else {
            (0.1, 3.0)
        }
    };
    let (r, tested) = search(&desktop(), &[], &all, &measure);
    assert_eq!(tested.len(), 3 + 1);
    assert_eq!(r.unwrap().backend, BackendKind::Cpu);
}

#[test]
fn model_check_tests_only_installed_models_and_offers_better_ones() {
    // Only base.en is installed (the shippable default today): it is tested and recommended,
    // and the two better Parakeet models are offered with their sizes.
    let only_base = |m: &str| m == "base.en";
    let (r, tested) = search(&desktop(), &[], &only_base, &|_, _| (0.02, 5.0));
    assert_eq!(tested, [("base.en", BackendKind::Cuda)]);
    let r = r.unwrap();
    assert_eq!((r.model, r.fallback), ("base.en", None));
    let offered: Vec<(&str, u64)> = r.missing.iter().map(|m| (m.model, m.size)).collect();
    assert_eq!(
        offered,
        [
            ("parakeet-tdt-0.6b-v2", 2_512_987_040),
            ("parakeet-tdt-110m", 477_410_591)
        ]
    );
    // The fallback skips a model that is not installed.
    let no_110m = |m: &str| m != "parakeet-tdt-110m";
    let (r, _) = search(&desktop(), &[], &no_110m, &|_, _| (0.01, 3.0));
    let r = r.unwrap();
    assert_eq!(
        (r.model, r.fallback),
        ("parakeet-tdt-0.6b-v2", Some("base.en"))
    );
    // Nothing installed: no recommendation.
    let (r, tested) = search(&desktop(), &[], &|_| false, &|_, _| (0.01, 3.0));
    assert!(r.is_none() && tested.is_empty());
}

#[test]
fn model_check_meeting_languages_choose_the_multilingual_models() {
    // German on the desktop: Parakeet v3 first; turbo is its fallback.
    let (r, tested) = search(&desktop(), &["de"], &all, &|_, _| (0.02, 4.0));
    assert_eq!(tested, [("parakeet-tdt-0.6b-v3", BackendKind::Cuda)]);
    let r = r.unwrap();
    assert_eq!(
        (r.model, r.fallback),
        ("parakeet-tdt-0.6b-v3", Some("large-v3-turbo"))
    );
    // Japanese on a machine without a GPU: nothing measured covers it on the CPU.
    let (r, tested) = search(&laptop(12, 16), &["ja"], &all, &|_, _| (0.02, 4.0));
    assert!(r.is_none() && tested.is_empty());
}

#[test]
fn model_check_garbage_from_a_bigger_model_keeps_the_passing_one() {
    // A 4-core laptop passes the 110M comfortably, steps up, and the 0.6B returns garbage
    // (for example out of memory): the 110M stands; the CPU backend is not abandoned.
    let measure = |m: &str, _| {
        if m == "parakeet-tdt-0.6b-v2" {
            (0.3, 80.0)
        } else {
            (0.1, 5.0)
        }
    };
    let (r, tested) = search(&laptop(4, 16), &[], &all, &measure);
    assert_eq!(tested.len(), 2);
    let r = r.unwrap();
    assert_eq!(
        (r.model, r.backend, r.fallback),
        ("parakeet-tdt-110m", BackendKind::Cpu, Some("base.en"))
    );
}

#[test]
fn model_check_language_codes_are_normalised() {
    assert_eq!(language_code("en-US"), "en");
    assert_eq!(language_code("EN"), "en");
    assert_eq!(language_code("de_AT"), "de");
    assert_eq!(
        candidates(BackendKind::Cuda, &["en-GB", "EN"]),
        candidates(BackendKind::Cuda, &["en"])
    );
    assert_eq!(
        candidates(BackendKind::Cuda, &["DE"]),
        candidates(BackendKind::Cuda, &["de"])
    );
}

#[test]
fn model_check_starts_at_the_first_installed_rung_at_or_below_the_guess() {
    // The desktop would start at the 0.6B, which is not installed: the 110M is tested first,
    // and the 0.6B is offered.
    let no_big = |m: &str| m != "parakeet-tdt-0.6b-v2";
    let (r, tested) = search(&desktop(), &[], &no_big, &|_, _| (0.05, 3.0));
    assert_eq!(tested, [("parakeet-tdt-110m", BackendKind::Cuda)]);
    let r = r.unwrap();
    assert_eq!(r.model, "parakeet-tdt-110m");
    assert_eq!(r.missing.len(), 1);
    // A 4-core laptop would start at the 110M; only the 0.6B is installed: it is still tried
    // (the guess only chooses where to start, the test decides).
    let only_big = |m: &str| m == "parakeet-tdt-0.6b-v2";
    let (r, tested) = search(&laptop(4, 16), &[], &only_big, &|_, _| (0.3, 3.0));
    assert_eq!(tested, [("parakeet-tdt-0.6b-v2", BackendKind::Cpu)]);
    assert_eq!(r.unwrap().model, "parakeet-tdt-0.6b-v2");
    // A CPU entry among the accelerators does not duplicate the CPU backend.
    let cpu_listed = HardwareFacts {
        accelerators: vec![Accelerator {
            backend: BackendKind::Cpu,
            free_memory: None,
        }],
        ..laptop(8, 16)
    };
    assert_eq!(backend_order(&cpu_listed), [BackendKind::Cpu]);
}

/// A laptop with integrated graphics and a Vulkan build (T6.10d): 8 cores, 16 GB.
fn igpu_laptop() -> HardwareFacts {
    HardwareFacts {
        accelerators: vec![Accelerator {
            backend: BackendKind::Vulkan,
            free_memory: None,
        }],
        ..laptop(8, 16)
    }
}

#[test]
fn model_check_vulkan_ladder_runs_whisper_only_after_cuda() {
    for l in LADDERS.iter().filter(|l| l.backend == BackendKind::Vulkan) {
        for m in l.models {
            assert!(
                matches!(
                    crate::backends::asr_model(m),
                    Ok(crate::backends::AsrModel::Whisper(_))
                ),
                "{m}: ONNX Runtime has no Vulkan provider"
            );
        }
    }
    assert_eq!(
        backend_order(&igpu_laptop()),
        [BackendKind::Vulkan, BackendKind::Cpu]
    );
    let both = HardwareFacts {
        accelerators: vec![
            Accelerator {
                backend: BackendKind::Cuda,
                free_memory: Some(14 * GB),
            },
            Accelerator {
                backend: BackendKind::Vulkan,
                free_memory: None,
            },
        ],
        ..desktop()
    };
    assert_eq!(
        backend_order(&both),
        [BackendKind::Cuda, BackendKind::Vulkan, BackendKind::Cpu]
    );
    // CUDA still settles on its own: Vulkan and the CPU are not tried.
    let (r, tested) = search(&both, &[], &all, &|_, _| (0.01, 3.0));
    assert_eq!(tested, [("parakeet-tdt-0.6b-v2", BackendKind::Cuda)]);
    assert_eq!(r.unwrap().backend, BackendKind::Cuda);
}

/// The Surface with a `qnn` build (GATE-03a): the NPU, 12 cores, 16 GB.
fn npu_laptop() -> HardwareFacts {
    HardwareFacts {
        accelerators: vec![Accelerator {
            backend: BackendKind::Qnn,
            free_memory: None,
        }],
        ..laptop(12, 16)
    }
}

#[test]
fn model_check_npu_ladder_runs_converted_parakeet_first() {
    for l in LADDERS.iter().filter(|l| l.backend == BackendKind::Qnn) {
        for m in l.models {
            assert!(
                matches!(
                    crate::backends::asr_model(m),
                    Ok(crate::backends::AsrModel::Parakeet(_))
                ),
                "{m}: only ONNX models run on the NPU"
            );
        }
    }
    assert_eq!(
        backend_order(&npu_laptop()),
        [BackendKind::Qnn, BackendKind::Cpu]
    );
    // The NPU settles on its own, as CUDA does: the CPU is not tried.
    let (r, tested) = search(&npu_laptop(), &[], &all, &|_, _| (0.02, 3.0));
    assert_eq!(tested, [("parakeet-tdt-0.6b-v2", BackendKind::Qnn)]);
    let r = r.unwrap();
    assert_eq!(
        (r.model, r.backend),
        ("parakeet-tdt-0.6b-v2", BackendKind::Qnn)
    );
    // An NPU that gives wrong output moves on to the CPU ladder.
    let (r, tested) = search(&npu_laptop(), &[], &all, &|_, b| {
        if b == BackendKind::Qnn {
            (0.02, 95.0)
        } else {
            (0.04, 3.0)
        }
    });
    assert_eq!(tested[0].1, BackendKind::Qnn);
    assert_eq!(r.unwrap().backend, BackendKind::Cpu);
    // Multilingual meetings have no NPU model: straight to the CPU's.
    let (r, tested) = search(&npu_laptop(), &["de"], &all, &|_, _| (0.04, 3.0));
    assert!(
        tested.iter().all(|(_, b)| *b == BackendKind::Cpu),
        "{tested:?}"
    );
    assert_eq!(r.unwrap().model, "parakeet-tdt-0.6b-v3");
}

#[test]
fn model_check_quality_order_is_the_measured_meeting_wer() {
    // QUALITY_ORDER is the greedy AMI (IHM) WER order of docs/model-eval/cuda.json.
    let report: serde_json::Value =
        serde_json::from_str(include_str!("../../../docs/model-eval/cuda.json")).unwrap();
    let mut rows: Vec<(String, f64)> = report["rows"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|r| r["decoding"] == "greedy")
        .map(|r| {
            (
                r["model"].as_str().unwrap().to_owned(),
                r["corpora"]["ami"]["wer"]["wer_percent"].as_f64().unwrap(),
            )
        })
        .collect();
    rows.sort_by(|a, b| a.1.total_cmp(&b.1));
    let measured: Vec<&str> = rows.iter().map(|(m, _)| m.as_str()).collect();
    assert_eq!(QUALITY_ORDER, measured.as_slice());
    // Every manifest model is ranked, and every ladder runs best first.
    for m in MODELS {
        assert!(quality_rank(m.name) < QUALITY_ORDER.len(), "{}", m.name);
    }
    for l in LADDERS {
        assert!(
            l.models
                .windows(2)
                .all(|w| quality_rank(w[0]) < quality_rank(w[1])),
            "{:?} {:?}: {:?}",
            l.backend,
            l.scope,
            l.models
        );
    }
    assert_eq!(quality_rank("no-such-model"), usize::MAX);
}

#[test]
fn model_check_vulkan_is_kept_only_when_it_beats_the_cpu() {
    use BackendKind::{Cpu, Vulkan};
    // Everything installed: Vulkan settles on small.en, but the CPU runs Parakeet 110M, a
    // better model, so the CPU answer is used. Both backends were tested.
    let measure = |m: &str, b| match (m, b) {
        ("small.en", Vulkan) => (0.15, 4.0),
        ("parakeet-tdt-0.6b-v2", Cpu) => (0.8, 3.0),
        (_, Cpu) => (0.1, 5.0),
        _ => (0.1, 6.0),
    };
    let (r, tested) = search(&igpu_laptop(), &[], &all, &measure);
    assert_eq!(
        tested,
        [
            ("small.en", Vulkan),
            ("parakeet-tdt-110m", Cpu),
            ("parakeet-tdt-0.6b-v2", Cpu)
        ]
    );
    let r = r.unwrap();
    assert_eq!(
        (r.model, r.backend, r.fallback),
        ("parakeet-tdt-110m", Cpu, Some("base.en"))
    );

    // Only base.en installed (the shippable model today): the same model on both, so Vulkan
    // must be faster to be kept.
    let only_base = |m: &str| m == "base.en";
    let fast_gpu = |_: &str, b| {
        if b == Vulkan { (0.05, 5.0) } else { (0.3, 5.0) }
    };
    let (r, tested) = search(&igpu_laptop(), &[], &only_base, &fast_gpu);
    assert_eq!(tested, [("base.en", Vulkan), ("base.en", Cpu)]);
    let r = r.unwrap();
    assert_eq!((r.model, r.backend), ("base.en", Vulkan));
    let slow_gpu = |_: &str, b| {
        if b == Vulkan { (0.3, 5.0) } else { (0.1, 5.0) }
    };
    let (r, _) = search(&igpu_laptop(), &[], &only_base, &slow_gpu);
    assert_eq!(r.unwrap().backend, Cpu);
    // A tie is not a win: Vulkan must beat the CPU.
    let (r, _) = search(&igpu_laptop(), &[], &only_base, &|_, _| (0.1, 5.0));
    assert_eq!(r.unwrap().backend, Cpu);

    // Whisper models only: Vulkan runs small.en, which no installed CPU model can beat
    // (only base.en is on the CPU ladder), so the CPU is not even tested.
    let whisper = |m: &str| m == "small.en" || m == "base.en";
    let (r, tested) = search(&igpu_laptop(), &[], &whisper, &|_, _| (0.15, 4.0));
    assert_eq!(tested, [("small.en", Vulkan)]);
    let r = r.unwrap();
    assert_eq!(
        (r.model, r.backend, r.fallback),
        ("small.en", Vulkan, Some("base.en"))
    );

    // Vulkan passes, the CPU has no passing model: Vulkan stands.
    let cpu_broken = |_: &str, b| {
        if b == Vulkan { (0.1, 5.0) } else { (0.1, 90.0) }
    };
    let (r, _) = search(&igpu_laptop(), &[], &only_base, &cpu_broken);
    assert_eq!(r.unwrap().backend, Vulkan);
    // Vulkan produces garbage: the CPU, as for any broken backend.
    let gpu_broken = |_: &str, b| {
        if b == Vulkan { (0.1, 90.0) } else { (0.1, 5.0) }
    };
    let (r, tested) = search(&igpu_laptop(), &[], &only_base, &gpu_broken);
    assert_eq!(tested, [("base.en", Vulkan), ("base.en", Cpu)]);
    assert_eq!(r.unwrap().backend, Cpu);
}

#[test]
fn model_check_vulkan_for_languages_outside_the_cpu_models() {
    // Japanese: turbo on Vulkan; nothing on the CPU covers it, so the CPU is not tested.
    let (r, tested) = search(&igpu_laptop(), &["ja"], &all, &|_, _| (0.3, 4.0));
    assert_eq!(tested, [("large-v3-turbo", BackendKind::Vulkan)]);
    let r = r.unwrap();
    assert_eq!(
        (r.model, r.backend),
        ("large-v3-turbo", BackendKind::Vulkan)
    );
}
