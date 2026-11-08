//! Models page IPC tests (T7.4e) over a temp store and models directory.

use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use earshot_runtime::earshot_hardware::{Cpu, Gpu, Hardware, Npu};
use earshot_runtime::model_bench::record_check;
use earshot_runtime::summary_model::LOW_MEMORY_SUMMARY_MODEL;

use super::*;
use crate::model_jobs::{CheckDone, CheckStep, DownloadEnd, ModelSink, ModelWork};
use crate::testutil::{TempDir, services};

const GIB: u64 = 1 << 30;
const SURFACE: &[Accelerator] = &[Accelerator::Qnn, Accelerator::Cpu];

fn surface() -> Hardware {
    Hardware {
        cpu: Cpu {
            name: Some("Snapdragon(R) X2 Elite - X2E88100 - Qualcomm(R) Oryon(TM) CPU".into()),
            physical_cores: Some(12),
            logical_cores: Some(12),
        },
        gpus: vec![Gpu {
            name: "Qualcomm(R) Adreno(TM) X2-90 GPU".into(),
            vendor_id: 0x4d4f_4351,
            dedicated_memory: Some(512 << 20),
            driver_version: Some("31.0.112.0".into()),
            integrated: Some(true),
            compute_capability: None,
        }],
        npus: vec![Npu {
            name: "Snapdragon(R) X2 Elite NPU".into(),
            driver_version: Some("30.0.143.0".into()),
        }],
        total_memory: Some(15 * GIB + GIB / 2),
    }
}

/// Model jobs that never run (the views only ask whether a model downloads).
struct Idle;

impl ModelWork for Idle {
    fn download(
        &self,
        _: &Services,
        _: &str,
        _: &earshot_core::asr::CancelToken,
        _: &mut dyn FnMut(u64, u64),
    ) -> Result<(), DownloadEnd> {
        Ok(())
    }

    fn check(
        &self,
        _: &Services,
        _: &earshot_core::asr::CancelToken,
        _: &mut dyn FnMut(CheckStep),
        _: &mut dyn FnMut(String),
    ) -> Result<CheckDone, IpcError> {
        Ok(CheckDone {
            answer: None,
            trials: 0,
        })
    }
}

fn idle_jobs() -> ModelJobs {
    let sink: ModelSink = Arc::new(|_| {});
    ModelJobs::new(Arc::new(Idle), sink, crate::activity::WorkGate::new())
}

#[test]
fn models_hardware_view_keeps_every_fact_and_the_build_order() {
    let v = hardware_view(&surface(), SURFACE);
    assert_eq!(
        v.processor.as_deref(),
        Some("Snapdragon(R) X2 Elite - X2E88100 - Qualcomm(R) Oryon(TM) CPU")
    );
    assert_eq!((v.physical_cores, v.logical_cores), (Some(12), Some(12)));
    assert_eq!(v.gpus[0].dedicated_memory, Some(512 << 20));
    assert_eq!(v.gpus[0].integrated, Some(true));
    assert_eq!(v.npus[0].name, "Snapdragon(R) X2 Elite NPU");
    assert_eq!(v.accelerators, [RunsOn::Npu, RunsOn::Processor]);
    // A missing fact stays missing.
    let empty = hardware_view(&Hardware::default(), &[Accelerator::Cpu]);
    assert_eq!(empty.processor, None);
    assert_eq!(empty.total_memory, None);
    assert!(empty.gpus.is_empty() && empty.npus.is_empty());
}

#[test]
fn models_view_offers_the_speech_helpers_with_their_size_and_never_removes_them() {
    use earshot_runtime::catalogue::{HELPERS_MODEL, display_name};
    let dir = TempDir::new("models-helpers");
    let svc = services(&dir);
    let models = ModelDir::new(dir.0.join("models"));
    let hw = surface();
    let downloading = |m: &str| m == HELPERS_MODEL;
    let v = models_view(&svc, &models, &hw, SURFACE, &downloading).unwrap();
    let h = &v.helpers;
    assert_eq!(h.name, HELPERS_MODEL);
    assert_eq!(h.title, display_name(HELPERS_MODEL));
    assert_eq!(h.kind, ModelUse::Helpers);
    assert_eq!(h.installed, Installed::No);
    assert_eq!(h.held, 0);
    assert!(h.size > 0);
    assert_eq!(
        h.size,
        Catalogue::embedded()
            .unwrap()
            .download_size(HELPERS_MODEL)
            .unwrap()
    );
    assert!(h.protected);
    assert!(h.downloading);
    assert!(v.transcription.iter().all(|m| m.name != HELPERS_MODEL));
    assert!(v.summary.iter().all(|m| m.name != HELPERS_MODEL));
    // The refusal goes through the page's one removal path.
    let e = remove(&svc, &models, &hw, SURFACE, &idle_jobs(), HELPERS_MODEL).unwrap_err();
    assert_eq!(e.kind, "model_in_use");
    assert_eq!(earshot_runtime::catalogue::manifest_id_in(&e.message), None);
}

#[test]
fn models_view_lists_the_catalogue_choices_and_what_is_in_use() {
    let dir = TempDir::new("models-view");
    let svc = services(&dir);
    let models = ModelDir::new(dir.0.join("models"));
    let hw = surface();
    let downloading = |m: &str| m == "parakeet-tdt-110m";
    let v = models_view(&svc, &models, &hw, SURFACE, &downloading).unwrap();
    assert_eq!(v.languages, ["en"]);
    let rec = v.recommended.clone().unwrap();
    assert_eq!(
        (rec.model.as_str(), rec.runs_on),
        ("parakeet-tdt-0.6b-v2", RunsOn::Npu)
    );
    let roles: Vec<(ChoiceKind, &str)> = v
        .choices
        .iter()
        .map(|c| (c.role, c.model.as_str()))
        .collect();
    assert_eq!(
        roles,
        [
            (ChoiceKind::Recommended, "parakeet-tdt-0.6b-v2"),
            (ChoiceKind::Lighter, "parakeet-tdt-110m"),
            (ChoiceKind::European, "parakeet-tdt-0.6b-v3"),
            (ChoiceKind::EveryLanguage, "large-v3-turbo"),
        ]
    );
    // Every choice is a listed transcription model, with its size, place and state.
    for c in &v.choices {
        assert!(v.transcription.iter().any(|m| m.name == c.model), "{c:?}");
    }
    let item = |n: &str| v.transcription.iter().find(|m| m.name == n).unwrap();
    let v2 = item("parakeet-tdt-0.6b-v2");
    assert_eq!(v2.kind, ModelUse::Transcription);
    assert_eq!(v2.size, 2_512_987_040);
    assert_eq!(v2.installed, Installed::No);
    // Without its NPU version the model runs on the processor, as a meeting would (T8.4g).
    assert_eq!(
        (v2.held, v2.runs_on, v2.check),
        (0, RunsOn::Processor, Verdict::Unknown)
    );
    assert!(!v2.downloading && !v2.protected);
    assert!(item("parakeet-tdt-110m").downloading);
    assert_eq!(item("parakeet-tdt-0.6b-v3").runs_on, RunsOn::Processor);
    assert!(
        item("parakeet-tdt-0.6b-v3")
            .languages
            .contains(&"de".to_owned())
    );
    // The fallback model is always kept.
    assert!(item(host::FALLBACK_MODEL).protected);
    // Summary models: the shipped two, on the processor here.
    assert_eq!(v.summary.len(), 2);
    assert!(v.summary.iter().all(|m| m.kind == ModelUse::Summary));
    assert!(v.summary.iter().all(|m| m.runs_on == RunsOn::Processor));
    assert_eq!(v.summary_recommended.model, LOW_MEMORY_SUMMARY_MODEL);
    // Nothing installed and no check: the fallback model is what a meeting would use.
    assert_eq!(v.check, None);
    assert_eq!(v.in_use.transcription, host::FALLBACK_MODEL);
    assert!(
        v.in_use
            .transcription_source
            .starts_with("the fallback model")
    );
    assert_eq!(v.in_use.summary, LOW_MEMORY_SUMMARY_MODEL);
    assert!(v.in_use.summaries_here);
    assert!(v.in_use.notes.is_empty());
}

#[test]
fn models_view_says_npu_only_for_a_model_whose_npu_version_is_installed() {
    use earshot_runtime::backends::{AsrModel, asr_model};
    use earshot_runtime::catalogue::NPU_OFFERED;
    let dir = TempDir::new("models-npu");
    let svc = services(&dir);
    let models = ModelDir::new(dir.0.join("models"));
    let hw = surface();
    let runs_on = |n: &str| {
        let v = models_view(&svc, &models, &hw, SURFACE, &|_| false).unwrap();
        v.transcription
            .iter()
            .find(|m| m.name == n)
            .unwrap()
            .runs_on
    };
    assert_eq!(runs_on("parakeet-tdt-0.6b-v2"), RunsOn::Processor);
    let Ok(AsrModel::Parakeet(p)) = asr_model("parakeet-tdt-0.6b-v2") else {
        panic!("a Parakeet model");
    };
    for rel in p.qnn_files() {
        let f = models.root().join(rel);
        std::fs::create_dir_all(f.parent().unwrap()).unwrap();
        std::fs::write(f, b"x").unwrap();
    }
    assert_eq!(runs_on("parakeet-tdt-0.6b-v2"), RunsOn::Npu);
    assert_eq!(runs_on("parakeet-tdt-110m"), RunsOn::Processor);
    // An x64 build lists no NPU version; an ARM64 build lists one for each of these two.
    let v = models_view(&svc, &models, &hw, SURFACE, &|_| false).unwrap();
    assert_eq!(v.npu.is_empty(), !NPU_OFFERED);
    assert!(v.npu.iter().all(|m| m.kind == ModelUse::NpuVersion));
    assert!(
        v.transcription
            .iter()
            .all(|m| m.kind == ModelUse::Transcription)
    );
}

#[test]
fn models_view_follows_the_settings_and_the_stored_check() {
    let dir = TempDir::new("models-view");
    let svc = services(&dir);
    let models = ModelDir::new(dir.0.join("models"));
    let hw = surface();
    let check = StoredCheck {
        fingerprint: "an older machine".into(),
        model: "parakeet-tdt-110m".into(),
        backend: BackendKind::Qnn,
        fallback: Some("tiny.en".into()),
        trials: Vec::new(),
    };
    record_check(&mut svc.store(), &check).unwrap();
    let value = ModelSettingsValue {
        transcription: Some("parakeet-tdt-0.6b-v3".into()),
        transcription_part: PartChoice::Npu,
        summary: None,
        summary_part: PartChoice::Processor,
        languages: vec!["en".into(), "de-AT".into()],
    };
    let stored = set_settings(&svc, &value).unwrap();
    assert_eq!(stored.value.languages, ["en", "de"]);
    assert_eq!(settings_view(&svc).unwrap(), stored);
    let v = models_view(&svc, &models, &hw, SURFACE, &|_| false).unwrap();
    assert_eq!(v.languages, ["en", "de"]);
    assert_eq!(v.recommended.unwrap().model, "parakeet-tdt-0.6b-v3");
    // The stored check is listed, out of date here.
    let c = v.check.unwrap();
    assert_eq!(
        (
            c.model.as_str(),
            c.runs_on,
            c.fallback.as_deref(),
            c.current
        ),
        ("parakeet-tdt-110m", RunsOn::Npu, Some("tiny.en"), false)
    );
    // The chosen model on the NPU it has no version for: the processor, with the note.
    assert_eq!(v.in_use.transcription, "parakeet-tdt-0.6b-v3");
    assert_eq!(v.in_use.transcription_runs_on, RunsOn::Processor);
    assert_eq!(v.in_use.transcription_source, "chosen in Settings");
    assert_eq!(
        v.in_use.notes,
        ["Parakeet 0.6B v3 has no NPU version, so it runs on the processor."]
    );
    let protected: Vec<&str> = v
        .transcription
        .iter()
        .filter(|m| m.protected)
        .map(|m| m.name.as_str())
        .collect();
    for m in [
        "parakeet-tdt-0.6b-v3",
        "parakeet-tdt-110m",
        "tiny.en",
        host::FALLBACK_MODEL,
    ] {
        assert!(protected.contains(&m), "{m}: {protected:?}");
    }
}

#[test]
fn models_settings_refusals_are_invalid_setting_and_store_nothing() {
    let dir = TempDir::new("models-settings");
    let svc = services(&dir);
    let before = settings_view(&svc).unwrap();
    assert_eq!(before.value.transcription, None);
    assert_eq!(before.value.transcription_part, PartChoice::Automatic);
    assert_eq!(before.value.languages, ["en"]);
    let bad = [
        ModelSettingsValue {
            transcription: Some("base.en".into()),
            languages: vec!["en".into(), "ja".into()],
            ..before.value.clone()
        },
        ModelSettingsValue {
            summary_part: PartChoice::Npu,
            ..before.value.clone()
        },
        ModelSettingsValue {
            languages: vec![],
            ..before.value.clone()
        },
        ModelSettingsValue {
            summary: Some("qwen9-70b".into()),
            ..before.value.clone()
        },
    ];
    for v in &bad {
        let e = set_settings(&svc, v).unwrap_err();
        assert_eq!(e.kind, "invalid_setting", "{v:?}");
        assert!(!e.retryable);
    }
    assert_eq!(
        set_settings(&svc, &bad[0]).unwrap_err().message,
        "Whisper base.en does not transcribe ja; choose a multilingual model"
    );
    assert_eq!(settings_view(&svc).unwrap(), before);
    // An unknown stored value is reported once read.
    {
        let mut db = svc.store();
        db.set_local_value(
            earshot_runtime::model_settings::TRANSCRIPTION_PART_KEY,
            b"tpu",
        )
        .unwrap();
    }
    let v = settings_view(&svc).unwrap();
    assert_eq!(v.value.transcription_part, PartChoice::Automatic);
    assert_eq!(v.unknown.len(), 1);
}

#[test]
fn models_remove_refuses_kept_models_and_frees_others() {
    let dir = TempDir::new("models-remove");
    let svc = services(&dir);
    let models = ModelDir::new(dir.0.join("models"));
    let jobs = idle_jobs();
    let catalogue = Catalogue::embedded().unwrap();
    // A partly downloaded model to remove: its `.part` goes.
    let a = &catalogue.artefacts_of("tiny.en").unwrap()[0];
    let part = earshot_runtime::download::part_path(&a.file(&models).unwrap());
    std::fs::create_dir_all(part.parent().unwrap()).unwrap();
    std::fs::write(&part, b"12345").unwrap();
    let freed = remove(&svc, &models, &surface(), SURFACE, &jobs, "tiny.en").unwrap();
    assert_eq!(freed.bytes_freed, 5);
    assert!(!part.exists());
    let e = remove(
        &svc,
        &models,
        &surface(),
        SURFACE,
        &jobs,
        host::FALLBACK_MODEL,
    )
    .unwrap_err();
    assert_eq!(e.kind, "model_in_use");
    set_settings(
        &svc,
        &ModelSettingsValue {
            transcription: Some("small.en".into()),
            transcription_part: PartChoice::Automatic,
            summary: Some(LOW_MEMORY_SUMMARY_MODEL.into()),
            summary_part: PartChoice::Automatic,
            languages: vec!["en".into()],
        },
    )
    .unwrap();
    for m in ["small.en", LOW_MEMORY_SUMMARY_MODEL] {
        assert_eq!(
            remove(&svc, &models, &surface(), SURFACE, &jobs, m)
                .unwrap_err()
                .kind,
            "model_in_use"
        );
    }
    assert_eq!(
        remove(&svc, &models, &surface(), SURFACE, &jobs, "nope")
            .unwrap_err()
            .kind,
        "unknown_model"
    );
}

/// Model jobs whose downloads wait for `go`.
struct Waiting(Mutex<mpsc::Receiver<()>>);

impl ModelWork for Waiting {
    fn download(
        &self,
        _: &Services,
        _: &str,
        _: &earshot_core::asr::CancelToken,
        _: &mut dyn FnMut(u64, u64),
    ) -> Result<(), DownloadEnd> {
        let _ = self.0.lock().unwrap().recv_timeout(Duration::from_secs(30));
        Ok(())
    }

    fn check(
        &self,
        _: &Services,
        _: &earshot_core::asr::CancelToken,
        _: &mut dyn FnMut(CheckStep),
        _: &mut dyn FnMut(String),
    ) -> Result<CheckDone, IpcError> {
        unreachable!("not used")
    }
}

#[test]
fn models_remove_and_start_download_are_one_critical_section() {
    let dir = TempDir::new("models-race");
    let svc = services(&dir);
    let models = ModelDir::new(dir.0.join("models"));
    let (go, rx) = mpsc::channel();
    let sink: ModelSink = Arc::new(|_| {});
    let jobs = ModelJobs::new(
        Arc::new(Waiting(Mutex::new(rx))),
        sink,
        crate::activity::WorkGate::new(),
    );
    // The removal runs with the downloads lock held, so a start waits for it.
    let seen = jobs
        .without_download("tiny.en", || Ok(jobs.downloads_locked()))
        .unwrap();
    assert!(seen, "the removal holds the downloads lock");
    // While the model downloads, its removal is refused and touches nothing.
    jobs.start_download(&svc, "tiny.en").unwrap();
    let e = remove(&svc, &models, &surface(), SURFACE, &jobs, "tiny.en").unwrap_err();
    assert_eq!((e.kind.as_str(), e.retryable), ("busy", true));
    let ran = std::cell::Cell::new(false);
    assert!(
        jobs.without_download("tiny.en", || {
            ran.set(true);
            Ok(())
        })
        .is_err()
    );
    assert!(!ran.get());
    go.send(()).unwrap();
    jobs.join();
    remove(&svc, &models, &surface(), SURFACE, &jobs, "tiny.en").unwrap();
}

#[test]
fn models_view_texts_name_models_only_by_their_display_names() {
    use earshot_runtime::catalogue::{display_name, manifest_id_in};
    let dir = TempDir::new("models-view");
    let svc = services(&dir);
    let models = ModelDir::new(dir.0.join("models"));
    let small_card = Hardware {
        gpus: vec![Gpu {
            name: "NVIDIA GeForce RTX 3050".into(),
            vendor_id: 0x10de,
            dedicated_memory: Some(4 * GIB),
            driver_version: None,
            integrated: Some(false),
            compute_capability: None,
        }],
        npus: Vec::new(),
        total_memory: Some(32 * GIB),
        ..surface()
    };
    let desktop: &[Accelerator] = &[Accelerator::Cuda, Accelerator::Cpu];
    let parts = [
        PartChoice::Automatic,
        PartChoice::GraphicsCard,
        PartChoice::Npu,
        PartChoice::Processor,
    ];
    let mut views = 0;
    for (hw, order) in [(surface(), SURFACE), (small_card, desktop)] {
        for transcription in [
            None,
            Some("parakeet-tdt-0.6b-v2"),
            Some("parakeet-tdt-0.6b-v3"),
            Some("large-v3-turbo"),
        ] {
            for transcription_part in parts {
                for summary_part in parts {
                    for summary in [None, Some(LOW_MEMORY_SUMMARY_MODEL)] {
                        let value = ModelSettingsValue {
                            transcription: transcription.map(str::to_owned),
                            transcription_part,
                            summary: summary.map(str::to_owned),
                            summary_part,
                            languages: vec!["en".into()],
                        };
                        if let Err(e) = set_settings(&svc, &value) {
                            assert_eq!(manifest_id_in(&e.message), None, "{}", e.message);
                            continue;
                        }
                        let v = models_view(&svc, &models, &hw, order, &|_| false).unwrap();
                        views += 1;
                        let mut texts = vec![
                            v.summary_recommended.reason.clone(),
                            v.in_use.transcription_source.clone(),
                        ];
                        texts.extend(v.recommended.iter().map(|r| r.reason.clone()));
                        texts.extend(v.in_use.notes.iter().cloned());
                        for t in &texts {
                            assert_eq!(manifest_id_in(t), None, "{t}");
                        }
                        for m in v.transcription.iter().chain(&v.summary) {
                            assert_eq!(m.title, display_name(&m.name));
                            assert_ne!(m.title, m.name);
                        }
                    }
                }
            }
        }
    }
    assert!(views > 0);
}

#[test]
fn models_view_says_when_this_computer_does_not_write_the_summaries() {
    use earshot_runtime::summary_settings::{SummarySettings, SummaryWriter, set_summary_settings};
    let dir = TempDir::new("models-view-writer");
    let svc = services(&dir);
    let models = ModelDir::new(dir.0.join("models"));
    let hw = surface();
    for (writer, here) in [
        (SummaryWriter::NoSummary, false),
        (SummaryWriter::ClaudeCode, false),
        (SummaryWriter::Codex, false),
        (SummaryWriter::OnThisComputer, true),
    ] {
        {
            let mut db = svc.store();
            set_summary_settings(
                &mut db,
                &SummarySettings {
                    writer,
                    ..SummarySettings::default()
                },
            )
            .unwrap();
        }
        let v = models_view(&svc, &models, &hw, SURFACE, &|_| false).unwrap();
        assert_eq!(v.in_use.summaries_here, here, "{writer:?}");
    }
}
