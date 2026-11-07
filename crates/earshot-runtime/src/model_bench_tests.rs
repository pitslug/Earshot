//! Model check run tests. Every name contains `model_check`.

use earshot_core::asr::BackendKind;
use earshot_store::{DataDir, Store};

use super::*;
use crate::model_check::{Accelerator, HardwareFacts};

fn power(x: &[f32]) -> f64 {
    x.iter().map(|v| f64::from(*v) * f64::from(*v)).sum::<f64>() / x.len().max(1) as f64
}

#[test]
fn model_check_bundled_clip_takes_turns_with_a_cancelled_echo() {
    let clip = TestClip::bundled().unwrap();
    let secs = clip.duration().as_secs_f64();
    assert!((50.0..100.0).contains(&secs), "{secs} s");
    assert_eq!(clip.mic.samples.len(), clip.reference.samples.len());
    assert_eq!(clip.raw_mic.samples.len(), clip.mic.samples.len());
    // Ten utterances each, every transcript word kept, in turn order.
    let words = |t: &str| t.split_whitespace().count();
    let total: usize = TRANSCRIPTS.lines().map(|l| words(l) - 1).sum();
    assert_eq!(words(&clip.mic_text) + words(&clip.reference_text), total);
    let of = |speaker: &str| {
        TRANSCRIPTS
            .lines()
            .filter(|l| l.starts_with(speaker))
            .filter_map(|l| l.split_once(' ').map(|(_, t)| t))
            .collect::<Vec<_>>()
            .join(" ")
    };
    assert_eq!(clip.mic_text, of("61-"));
    assert_eq!(clip.reference_text, of("121-"));
    // Where only the far end talks (second half, after the canceller has converged), the raw
    // mic carries the echo and the cleaned mic carries much less: at least 6 dB (a sanity
    // bound fixed before measuring, far below what the canceller reaches in accept m1).
    let half = clip.mic.samples.len() / 2;
    let far_only: Vec<usize> = (half..clip.reference.samples.len())
        .filter(|&i| clip.reference.samples[i].abs() > 0.02)
        .collect();
    assert!(
        far_only.len() > 16_000,
        "far-end samples {}",
        far_only.len()
    );
    let pick = |x: &[f32]| far_only.iter().map(|&i| x[i]).collect::<Vec<f32>>();
    let raw = power(&pick(&clip.raw_mic.samples));
    let cleaned = power(&pick(&clip.mic.samples));
    let erle = 10.0 * (raw / cleaned.max(1e-20)).log10();
    assert!(raw > 1e-6, "no echo in the raw mic");
    assert!(erle >= 6.0, "echo reduced by only {erle:.1} dB");
    // The canceller keeps the user's speech: the cleaned mic is far from silent where the
    // user talks.
    let near: Vec<f32> = (0..clip.mic.samples.len())
        .filter(|&i| clip.reference.samples[i] == 0.0 && clip.raw_mic.samples[i].abs() > 0.05)
        .map(|i| clip.mic.samples[i])
        .collect();
    assert!(power(&near) > 1e-4, "the user's speech was removed");
}

#[test]
fn model_check_hardware_read_reports_cores_memory_and_the_build() {
    let facts = hardware_facts();
    assert!(facts.cores >= 1);
    if cfg!(windows) {
        assert!(facts.memory > 1 << 30 && facts.memory < u64::MAX);
    }
    let backends: Vec<BackendKind> = facts.accelerators.iter().map(|a| a.backend).collect();
    if cfg!(feature = "cuda") {
        assert_eq!(backends, [BackendKind::Cuda]);
    } else {
        assert!(backends.is_empty());
    }
}

#[test]
fn model_check_driver_versions_are_parsed_from_reg_query() {
    let out = "\r\nHKEY_LOCAL_MACHINE\\SYSTEM\\CurrentControlSet\\Control\\Class\\{4d36e968-e325-11ce-bfc1-08002be10318}\\0001\r\n    DriverVersion    REG_SZ    32.0.13018.6\r\n\r\nHKEY_LOCAL_MACHINE\\SYSTEM\\CurrentControlSet\\Control\\Class\\{4d36e968-e325-11ce-bfc1-08002be10318}\\0000\r\n    DriverVersion    REG_SZ    32.0.16.1714\r\n\r\nEnd of search: 2 match(es) found.\r\n";
    assert_eq!(
        parse_driver_versions(out),
        ["0000=32.0.16.1714", "0001=32.0.13018.6"]
    );
    assert!(parse_driver_versions("ERROR: The system was unable to find").is_empty());
    assert!(parse_driver_versions("").is_empty());
}

#[test]
#[cfg_attr(not(windows), ignore = "the driver probe reads the Windows registry")]
fn model_check_driver_probe_reads_this_machine() {
    // Every Windows machine has a display adapter (a virtual one at least).
    let drivers = gpu_drivers().expect("reg query answered");
    assert!(!drivers.is_empty());
    assert!(drivers.iter().all(|d| d.contains('=')), "{drivers:?}");
    // Read once per process: the second call is the first answer.
    assert_eq!(gpu_drivers(), Some(drivers));
}

#[test]
fn model_check_probe_output_in_a_localized_code_page_still_parses() {
    // A German console writes the trailer in code page 850 ("Suchvorgang beendet: 1
    // Uebereinstimmung(en) gefunden." with a 0x9A for the umlaut), which is not UTF-8.
    let mut bytes = br"HKEY_LOCAL_MACHINE\SYSTEM\CurrentControlSet\Control\Class\{4d36e968-e325-11ce-bfc1-08002be10318}\0000".to_vec();
    bytes.extend_from_slice(
        b"\r\n    DriverVersion    REG_SZ    32.0.16.1714\r\n\r\nSuchvorgang beendet: 1 ",
    );
    bytes.push(0x9A);
    bytes.extend_from_slice(b"bereinstimmung(en) gefunden.\r\n");
    assert!(std::str::from_utf8(&bytes).is_err());
    assert_eq!(
        parse_driver_versions(&probe_text(&bytes)),
        ["0000=32.0.16.1714"]
    );
}

#[test]
fn model_check_fingerprint_changes_with_what_the_answer_depends_on() {
    let empty = ModelDir::new(std::env::temp_dir().join("earshot-model-check-empty"));
    let facts = HardwareFacts {
        accelerators: vec![Accelerator {
            backend: BackendKind::Cuda,
            free_memory: None,
        }],
        cores: 32,
        memory: 64 << 30,
        drivers: Some(vec!["0000=32.0.16.1714".into(), "0001=32.0.13018.6".into()]),
    };
    let base = check_fingerprint(&empty, &facts, &["en"]);
    assert_eq!(base, check_fingerprint(&empty, &facts, &["EN"]));
    assert_eq!(
        check_fingerprint(&empty, &facts, &["de", "en"]),
        check_fingerprint(&empty, &facts, &["en", "de", "de"])
    );
    assert_ne!(base, check_fingerprint(&empty, &facts, &["de"]));
    let fewer = HardwareFacts {
        cores: 8,
        ..facts.clone()
    };
    assert_ne!(base, check_fingerprint(&empty, &fewer, &["en"]));
    let cpu = HardwareFacts {
        accelerators: Vec::new(),
        ..facts.clone()
    };
    assert_ne!(base, check_fingerprint(&empty, &cpu, &["en"]));
    // A GPU driver update changes it (PLAN T6.10c).
    let updated = HardwareFacts {
        drivers: Some(vec!["0000=32.0.16.1720".into(), "0001=32.0.13018.6".into()]),
        ..facts.clone()
    };
    let after_update = check_fingerprint(&empty, &updated, &["en"]);
    assert_ne!(base, after_update);
    let stored = |fingerprint: &str| StoredCheck {
        fingerprint: fingerprint.into(),
        model: "base.en".into(),
        backend: BackendKind::Cuda,
        fallback: None,
        trials: Vec::new(),
    };
    assert!(!stored(&base).current(&after_update));
    // A failed driver probe is not a change: the stored check stays current, but only
    // when everything else matches.
    let unread = HardwareFacts {
        drivers: None,
        ..facts.clone()
    };
    let probe_failed = check_fingerprint(&empty, &unread, &["en"]);
    assert_ne!(base, probe_failed);
    assert!(stored(&base).current(&probe_failed));
    assert!(stored(&after_update).current(&probe_failed));
    let unread_fewer = HardwareFacts {
        drivers: None,
        cores: 8,
        ..facts.clone()
    };
    assert!(!stored(&base).current(&check_fingerprint(&empty, &unread_fewer, &["en"])));
    assert!(!stored(&base).current(&check_fingerprint(&empty, &unread, &["de"])));
    // Installing a model changes it.
    let dir = crate::testutil::TempDir::new("model-check-fp");
    let models = ModelDir::new(dir.0.as_path());
    let before = check_fingerprint(&models, &facts, &["en"]);
    let file = dir
        .0
        .as_path()
        .join(crate::backends::whisper_model("base.en").unwrap());
    std::fs::create_dir_all(file.parent().unwrap()).unwrap();
    std::fs::write(&file, b"").unwrap();
    assert!(installed(&models, "base.en"));
    assert!(!installed(&models, "parakeet-tdt-110m"));
    assert_ne!(before, check_fingerprint(&models, &facts, &["en"]));
}

#[test]
fn model_check_answer_is_stored_device_locally() {
    let dir = crate::testutil::TempDir::new("model-check-store");
    let data = DataDir::new(dir.0.as_path());
    data.create().unwrap();
    let mut store = Store::open(&data.db_path()).unwrap();
    assert_eq!(stored_check(&store).unwrap(), None);
    let check = StoredCheck {
        fingerprint: "fp".into(),
        model: "parakeet-tdt-0.6b-v2".into(),
        backend: BackendKind::Cuda,
        fallback: Some("parakeet-tdt-110m".into()),
        trials: Vec::new(),
    };
    record_check(&mut store, &check).unwrap();
    let back = stored_check(&store).unwrap().unwrap();
    assert_eq!(back, check);
    assert!(back.current("fp"));
    assert!(!back.current("other"));
}

#[test]
fn model_check_record_of_a_pair_tested_twice_reads_its_first_verdict() {
    // The rule ModelSearch acts on (`model_check::pair_result`): the first trial counts.
    let stored = |model: &str, verdict| StoredTrial {
        model: model.into(),
        backend: BackendKind::Cpu,
        verdict,
    };
    let check = StoredCheck {
        fingerprint: "fp".into(),
        model: "parakeet-tdt-110m".into(),
        backend: BackendKind::Cpu,
        fallback: None,
        trials: vec![
            stored("parakeet-tdt-0.6b-v2", StoredVerdict::TooSlow),
            stored("parakeet-tdt-110m", StoredVerdict::Pass),
            stored("parakeet-tdt-0.6b-v2", StoredVerdict::Pass),
        ],
    };
    assert_eq!(
        check.verdict("parakeet-tdt-0.6b-v2", BackendKind::Cpu),
        Some(StoredVerdict::TooSlow)
    );
}

#[test]
fn model_check_record_keeps_every_trial_verdict() {
    let report = |model: &'static str, backend, rtf, wer_percent| TrialReport {
        trial: Trial {
            model,
            backend,
            rtf,
            wer_percent,
        },
        failure: None,
        load: std::time::Duration::ZERO,
        evidence: None,
    };
    let trials = [
        (
            report("parakeet-tdt-0.6b-v2", BackendKind::Cpu, 0.9, 5.0),
            Verdict::TooSlow,
        ),
        (
            report("parakeet-tdt-110m", BackendKind::Cpu, 0.3, 6.0),
            Verdict::Pass { comfortable: false },
        ),
        (
            report("small.en", BackendKind::Vulkan, 0.1, 80.0),
            Verdict::Wrong,
        ),
    ];
    let answer = Recommendation {
        model: "parakeet-tdt-110m",
        backend: BackendKind::Cpu,
        fallback: Some("base.en"),
        missing: Vec::new(),
    };
    let check = StoredCheck::new("fp".into(), &answer, &trials);
    assert_eq!(check.model, "parakeet-tdt-110m");
    assert_eq!(check.fallback.as_deref(), Some("base.en"));
    let v = |m, b| check.verdict(m, b);
    assert_eq!(
        v("parakeet-tdt-0.6b-v2", BackendKind::Cpu),
        Some(StoredVerdict::TooSlow)
    );
    assert_eq!(
        v("parakeet-tdt-110m", BackendKind::Cpu),
        Some(StoredVerdict::Pass)
    );
    assert_eq!(
        v("small.en", BackendKind::Vulkan),
        Some(StoredVerdict::Wrong)
    );
    // Another backend, or a model never tested: unknown.
    assert_eq!(v("parakeet-tdt-0.6b-v2", BackendKind::Cuda), None);
    assert_eq!(v("base.en", BackendKind::Cpu), None);
    // A record from before T7.4d (no trials) still reads; only its answer is known.
    let old: StoredCheck = serde_json::from_str(
        r#"{"fingerprint":"fp","model":"base.en","backend":"cpu","fallback":null}"#,
    )
    .unwrap();
    assert!(old.trials.is_empty());
    assert_eq!(
        old.verdict("base.en", BackendKind::Cpu),
        Some(StoredVerdict::Pass)
    );
    assert_eq!(old.verdict("small.en", BackendKind::Cpu), None);
    let back: StoredCheck = serde_json::from_slice(&serde_json::to_vec(&check).unwrap()).unwrap();
    assert_eq!(back, check);
}

#[test]
#[ignore = "needs the ci model set and the CPU ONNX Runtime (cargo xtask fetch-models): cargo test --release -p earshot-runtime --lib model_check_trial -- --include-ignored"]
fn model_check_trial_of_base_en_on_the_cpu_is_correct() {
    // Whisper loads capture the process-wide log: one at a time with the backends tests.
    let _whisper = crate::backends::tests::whisper_loads();
    let data = DataDir::from_local_app_data().unwrap();
    let models = ModelDir::from_env(&data);
    crate::backends::init_onnx(&models, crate::backends::Accelerator::Cpu).unwrap();
    let clip = TestClip::bundled().unwrap();
    let r = run_trial(&models, "base.en", BackendKind::Cpu, &clip);
    assert_eq!(r.failure, None);
    // base.en scores about 4% on clean read speech; the clip must be well inside the
    // garbage limit for a working model.
    assert!(
        r.trial.wer_percent < crate::model_check::WER_LIMIT_PERCENT / 2.0,
        "{r:?}"
    );
    assert!(r.trial.rtf.is_finite() && r.trial.rtf > 0.0, "{r:?}");
    // A model that is not installed fails as a trial, not as a panic.
    let empty = ModelDir::new(std::env::temp_dir().join("earshot-model-check-none"));
    let missing = run_trial(&empty, "base.en", BackendKind::Cpu, &clip);
    assert!(missing.failure.is_some());
    assert!(missing.trial.wer_percent.is_nan());
}
