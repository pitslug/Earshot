//! Tests of the Models page settings (T7.4e): stored values, validation, "where it runs" and
//! the host's use of them.

use earshot_hardware::{Cpu, Gpu, Hardware};

use super::*;
use crate::backends::ModelDir;
use crate::catalogue::CatalogueError;
use crate::host::{self, FALLBACK_MODEL, TranscriptionSource};
use crate::model_bench::{StoredCheck, check_fingerprint, hardware_facts, record_check};
use crate::summary_model::{DEFAULT_SUMMARY_MODEL, LOW_MEMORY_SUMMARY_MODEL};
use crate::summary_settings::NoApiKeys;
use crate::testutil::TempDir;

const GIB: u64 = 1 << 30;
const CPU: &[Accelerator] = &[Accelerator::Cpu];
const SURFACE: &[Accelerator] = &[Accelerator::Qnn, Accelerator::Cpu];
const DESKTOP: &[Accelerator] = &[Accelerator::Cuda, Accelerator::Cpu];
const VULKAN: &[Accelerator] = &[Accelerator::Vulkan, Accelerator::Cpu];

fn store(dir: &TempDir) -> Store {
    Store::open(&dir.0.join("earshot.db")).unwrap()
}

fn named(m: &str) -> ModelPick {
    ModelPick::Named(m.into())
}

fn machine(memory: u64, gpu_memory: Option<u64>) -> Hardware {
    Hardware {
        cpu: Cpu {
            name: Some("Test CPU".into()),
            physical_cores: Some(12),
            logical_cores: Some(12),
        },
        gpus: gpu_memory
            .map(|m| Gpu {
                name: "Test GPU".into(),
                vendor_id: 0x10de,
                dedicated_memory: Some(m),
                driver_version: None,
                integrated: Some(false),
                compute_capability: None,
            })
            .into_iter()
            .collect(),
        total_memory: Some(memory),
        ..Hardware::default()
    }
}

#[test]
fn settings_round_trip_in_the_store() {
    let dir = TempDir::new("settings");
    let mut db = store(&dir);
    let (s, unknown) = model_settings(&db).unwrap();
    assert_eq!(s, ModelSettings::default());
    assert_eq!(s.languages, ["en"]);
    assert!(unknown.is_empty());
    let want = ModelSettings {
        transcription: named("parakeet-tdt-0.6b-v3"),
        transcription_part: Part::Npu,
        summary: named(LOW_MEMORY_SUMMARY_MODEL),
        summary_part: Part::Processor,
        languages: vec!["en".into(), "de".into()],
    };
    assert_eq!(set_model_settings(&mut db, &want).unwrap(), want);
    assert_eq!(model_settings(&db).unwrap(), (want.clone(), Vec::new()));
    // Kept across a reopen, as text under the registry keys.
    drop(db);
    let mut db = store(&dir);
    assert_eq!(model_settings(&db).unwrap().0, want);
    assert_eq!(
        db.local_value(MEETING_LANGUAGES_KEY).unwrap().unwrap(),
        b"en,de"
    );
    assert_eq!(
        db.local_value(TRANSCRIPTION_PART_KEY).unwrap().unwrap(),
        b"npu"
    );
    // Back to automatic.
    set_model_settings(&mut db, &ModelSettings::default()).unwrap();
    assert_eq!(model_settings(&db).unwrap().0, ModelSettings::default());
    assert_eq!(
        db.local_value(TRANSCRIPTION_MODEL_KEY).unwrap().unwrap(),
        b"automatic"
    );
    for p in Part::ALL {
        assert_eq!(Part::parse(p.as_str()), Some(p));
    }
}

#[test]
fn settings_keys_are_device_records() {
    for (record, key) in [
        (DeviceRecord::TranscriptionModel, TRANSCRIPTION_MODEL_KEY),
        (DeviceRecord::TranscriptionPart, TRANSCRIPTION_PART_KEY),
        (DeviceRecord::SummaryModel, SUMMARY_MODEL_KEY),
        (DeviceRecord::SummaryPart, SUMMARY_PART_KEY),
        (DeviceRecord::MeetingLanguages, MEETING_LANGUAGES_KEY),
    ] {
        assert_eq!(record.key().as_str(), key);
        assert!(DeviceRecord::ALL.contains(&record));
    }
}

#[test]
fn settings_unknown_stored_values_read_as_defaults_and_are_reported() {
    let dir = TempDir::new("settings");
    let mut db = store(&dir);
    for (key, value) in [
        (TRANSCRIPTION_MODEL_KEY, "whisper-v9"),
        (TRANSCRIPTION_PART_KEY, "tpu"),
        (SUMMARY_MODEL_KEY, "qwen9-70b"),
        (SUMMARY_PART_KEY, "npu"),
        (MEETING_LANGUAGES_KEY, "en,xx"),
    ] {
        db.set_local_value(key, value.as_bytes()).unwrap();
    }
    let (s, unknown) = model_settings(&db).unwrap();
    assert_eq!(s, ModelSettings::default());
    let got: Vec<(&str, &str)> = unknown.iter().map(|u| (u.key, u.stored.as_str())).collect();
    assert_eq!(
        got,
        [
            (TRANSCRIPTION_MODEL_KEY, "whisper-v9"),
            (TRANSCRIPTION_PART_KEY, "tpu"),
            (SUMMARY_MODEL_KEY, "qwen9-70b"),
            (SUMMARY_PART_KEY, "npu"),
            (MEETING_LANGUAGES_KEY, "en,xx"),
        ]
    );
    assert!(unknown[0].to_string().contains("the default is used"));
    // A known model that does not cover the stored languages is not used either.
    db.set_local_value(MEETING_LANGUAGES_KEY, b"de").unwrap();
    db.set_local_value(TRANSCRIPTION_MODEL_KEY, b"base.en")
        .unwrap();
    let (s, unknown) = model_settings(&db).unwrap();
    assert_eq!(s.transcription, ModelPick::Automatic);
    assert_eq!(s.languages, ["de"]);
    assert!(
        unknown
            .iter()
            .any(|u| u.key == TRANSCRIPTION_MODEL_KEY && u.stored == "base.en")
    );
}

#[test]
fn settings_validation_refuses_and_stores_nothing() {
    let dir = TempDir::new("settings");
    let mut db = store(&dir);
    let with = |f: &dyn Fn(&mut ModelSettings)| {
        let mut s = ModelSettings::default();
        f(&mut s);
        s
    };
    let refused = [
        with(&|s| s.transcription = named("whisper-v9")),
        // A helper model is not a transcription model.
        with(&|s| s.transcription = named("silero")),
        with(&|s| s.summary = named("qwen9-70b")),
        with(&|s| s.languages = vec![]),
        with(&|s| s.languages = vec![" ".into()]),
        with(&|s| s.languages = vec!["en".into(), "xx".into()]),
        with(&|s| {
            s.transcription = named("base.en");
            s.languages = vec!["en".into(), "de".into()];
        }),
        with(&|s| s.summary_part = Part::Npu),
    ];
    for s in &refused {
        assert!(set_model_settings(&mut db, s).is_err(), "{s:?}");
    }
    assert!(matches!(
        validate(&refused[5]),
        Err(SettingsError::UnknownLanguage(l)) if l == "xx"
    ));
    match validate(&refused[6]) {
        Err(e @ SettingsError::ModelLanguages { .. }) => {
            assert_eq!(
                e.to_string(),
                "Whisper base.en does not transcribe de; choose a multilingual model"
            );
        }
        other => panic!("{other:?}"),
    }
    assert!(matches!(
        validate(&refused[7]),
        Err(SettingsError::NpuForSummaries)
    ));
    for key in [
        TRANSCRIPTION_MODEL_KEY,
        SUMMARY_MODEL_KEY,
        SUMMARY_PART_KEY,
        MEETING_LANGUAGES_KEY,
    ] {
        assert_eq!(db.local_value(key).unwrap(), None, "{key}");
    }
    // Languages are normalised (region dropped, lower case, no repeats, order kept).
    let s = with(&|s| s.languages = vec!["en-GB".into(), "DE".into(), "en".into()]);
    assert_eq!(validate(&s).unwrap().languages, ["en", "de"]);
}

#[test]
fn settings_forced_part_falls_back_to_the_processor_with_the_reason() {
    let hw = machine(16 * GIB, Some(16 * GIB));
    let auto = |order: &[Accelerator]| order.to_vec();
    let place = |model: &str, part, build: &[Accelerator], hw: &Hardware| {
        transcription_placement(model, part, auto(build), build, hw)
    };
    // Automatic keeps the order it is given; the processor is the processor alone.
    let p = place("parakeet-tdt-0.6b-v2", Part::Automatic, DESKTOP, &hw);
    assert_eq!((p.order, p.note), (DESKTOP.to_vec(), None));
    let p = place("parakeet-tdt-0.6b-v2", Part::Processor, DESKTOP, &hw);
    assert_eq!((p.order, p.note), (vec![Accelerator::Cpu], None));
    // A part that is there and fits: that part, then the processor.
    let p = place("parakeet-tdt-0.6b-v2", Part::GraphicsCard, DESKTOP, &hw);
    assert_eq!(p.order, [Accelerator::Cuda, Accelerator::Cpu]);
    assert_eq!(p.note, None);
    let p = place("parakeet-tdt-110m", Part::Npu, SURFACE, &hw);
    assert_eq!(p.order, [Accelerator::Qnn, Accelerator::Cpu]);
    // No NPU version (T7.4 round 3 text).
    let p = place("parakeet-tdt-0.6b-v3", Part::Npu, SURFACE, &hw);
    assert_eq!(p.order, [Accelerator::Cpu]);
    assert_eq!(
        p.note.as_deref(),
        Some("Parakeet 0.6B v3 has no NPU version, so it runs on the processor.")
    );
    let p = place("large-v3-turbo", Part::Npu, SURFACE, &hw);
    assert_eq!(p.order, [Accelerator::Cpu]);
    // A build or computer without the part.
    let p = place("parakeet-tdt-0.6b-v2", Part::Npu, DESKTOP, &hw);
    assert_eq!(p.order, [Accelerator::Cpu]);
    assert!(p.note.unwrap().contains("no NPU Earshot can use"));
    let p = place("parakeet-tdt-0.6b-v2", Part::GraphicsCard, SURFACE, &hw);
    assert_eq!(p.order, [Accelerator::Cpu]);
    assert!(p.note.unwrap().contains("no graphics card Earshot can use"));
    // Vulkan runs whisper only.
    let p = place("parakeet-tdt-0.6b-v2", Part::GraphicsCard, VULKAN, &hw);
    assert_eq!(p.order, [Accelerator::Cpu]);
    assert!(p.note.unwrap().contains("has no graphics card version"));
    let p = place("small.en", Part::GraphicsCard, VULKAN, &hw);
    assert_eq!(p.order, [Accelerator::Vulkan, Accelerator::Cpu]);
    // Too big for the card (twice its size must fit); unknown memory is not held against it.
    let small_card = machine(16 * GIB, Some(2 * GIB));
    let p = place(
        "parakeet-tdt-0.6b-v2",
        Part::GraphicsCard,
        DESKTOP,
        &small_card,
    );
    assert_eq!(p.order, [Accelerator::Cpu]);
    assert_eq!(
        p.note.as_deref(),
        Some("Parakeet 0.6B v2 is too big for your graphics card, so it runs on the processor.")
    );
    let p = place(
        "parakeet-tdt-110m",
        Part::GraphicsCard,
        DESKTOP,
        &small_card,
    );
    assert_eq!(p.order, [Accelerator::Cuda, Accelerator::Cpu]);
    let p = place(
        "parakeet-tdt-0.6b-v2",
        Part::GraphicsCard,
        DESKTOP,
        &machine(16 * GIB, None),
    );
    assert_eq!(p.order, [Accelerator::Cuda, Accelerator::Cpu]);
}

#[test]
fn settings_summary_placement_follows_the_part_and_the_card() {
    let hw = machine(16 * GIB, Some(16 * GIB));
    let auto = ModelPick::Automatic;
    let planned = summary_model::summary_gpu_planned(DESKTOP);
    let s = summary_placement(&auto, Part::Automatic, DESKTOP, &hw).unwrap();
    assert_eq!((s.requested, s.gpu, s.note), (None, planned, None));
    let s =
        summary_placement(&named(DEFAULT_SUMMARY_MODEL), Part::Processor, DESKTOP, &hw).unwrap();
    assert_eq!(s.requested.as_deref(), Some(DEFAULT_SUMMARY_MODEL));
    assert!(!s.gpu);
    assert_eq!(s.note, None);
    // The graphics card where this build cannot use one: the processor, with the reason.
    let s = summary_placement(&auto, Part::GraphicsCard, SURFACE, &hw).unwrap();
    assert!(!s.gpu);
    assert!(s.note.unwrap().contains("run on the processor"));
    if planned {
        // A CUDA build: the card is used when the model fits it.
        let s = summary_placement(&auto, Part::GraphicsCard, DESKTOP, &hw).unwrap();
        assert!(s.gpu);
        assert_eq!(s.note, None);
        let small_card = machine(16 * GIB, Some(4 * GIB));
        let s = summary_placement(
            &named(DEFAULT_SUMMARY_MODEL),
            Part::GraphicsCard,
            DESKTOP,
            &small_card,
        )
        .unwrap();
        assert!(!s.gpu);
        assert_eq!(
            s.note.as_deref(),
            Some(
                "Qwen 3.5 9B is too big for your graphics card, so summaries run on the \
                 processor and take longer."
            )
        );
    } else {
        let s = summary_placement(&auto, Part::GraphicsCard, DESKTOP, &hw).unwrap();
        assert!(!s.gpu);
        assert!(s.note.is_some());
    }
    assert!(summary_placement(&named("nope"), Part::Processor, CPU, &hw).is_err());
}

#[test]
fn settings_host_transcription_setup_uses_the_settings() {
    let dir = TempDir::new("settings-host");
    let mut db = store(&dir);
    let models = ModelDir::new(dir.0.join("models"));
    let tiny = machine(GIB / 4, None);
    // Nothing stored, no check, nothing installed: the fallback, the build's order.
    let t = host::transcription_setup(&db, &models, SURFACE, &tiny).unwrap();
    assert_eq!(t.choice.model, FALLBACK_MODEL);
    assert_eq!(
        t.source,
        TranscriptionSource::Fallback("no model check yet")
    );
    assert_eq!(t.order, SURFACE);
    assert_eq!(t.languages, ["en"]);
    assert!(t.notes.is_empty());
    // The recommendation for this machine (base.en here) once it is installed.
    let base = crate::catalogue::Catalogue::embedded()
        .unwrap()
        .artefacts_of("base.en")
        .unwrap();
    for a in base {
        let f = a.file(&models).unwrap();
        std::fs::create_dir_all(f.parent().unwrap()).unwrap();
        std::fs::File::create(&f).unwrap().set_len(a.size).unwrap();
    }
    let t = host::transcription_setup(&db, &models, CPU, &tiny).unwrap();
    assert_eq!(t.choice.model, "base.en");
    assert!(
        matches!(t.source, TranscriptionSource::Recommended(ref why) if why.starts_with("no model check yet; "))
    );
    // A current check for the meeting languages wins over the recommendation.
    let langs = ["en", "de"];
    let check = StoredCheck {
        fingerprint: check_fingerprint(&models, &hardware_facts(), &langs),
        model: "parakeet-tdt-0.6b-v3".into(),
        backend: BackendKind::Cpu,
        fallback: None,
        trials: Vec::new(),
    };
    record_check(&mut db, &check).unwrap();
    let mut s = ModelSettings {
        languages: vec!["en".into(), "de".into()],
        ..ModelSettings::default()
    };
    set_model_settings(&mut db, &s).unwrap();
    let t = host::transcription_setup(&db, &models, DESKTOP, &tiny).unwrap();
    assert_eq!(t.source, TranscriptionSource::ModelCheck);
    assert_eq!(t.choice.model, "parakeet-tdt-0.6b-v3");
    assert_eq!(t.order, [Accelerator::Cpu], "from the check's backend down");
    // A chosen model, forced onto the NPU it has no version for: the processor, said.
    s.transcription = named("large-v3-turbo");
    s.transcription_part = Part::Npu;
    set_model_settings(&mut db, &s).unwrap();
    let t = host::transcription_setup(&db, &models, SURFACE, &tiny).unwrap();
    assert_eq!(t.source, TranscriptionSource::Setting);
    assert_eq!(t.choice.model, "large-v3-turbo");
    assert_eq!(t.order, [Accelerator::Cpu]);
    assert_eq!(
        t.notes,
        ["Whisper large-v3-turbo has no NPU version, so it runs on the processor."]
    );
    // An unknown stored value is told too.
    db.set_local_value(SUMMARY_PART_KEY, b"tpu").unwrap();
    let t = host::transcription_setup(&db, &models, SURFACE, &tiny).unwrap();
    assert!(t.notes.iter().any(|n| n.contains(SUMMARY_PART_KEY)));
}

/// What `transcription_setup` reads from the store is read in one go, so a page can let the
/// store go before the model files are looked at: the setup from those reads is the setup of
/// the store as it was when they were taken, whatever the store says after.
#[test]
fn settings_host_transcription_setup_from_reads_needs_no_store() {
    let dir = TempDir::new("settings-host-reads");
    let mut db = store(&dir);
    let models = ModelDir::new(dir.0.join("models"));
    let tiny = machine(GIB / 4, None);
    let langs = ["en"];
    let check = StoredCheck {
        fingerprint: check_fingerprint(&models, &hardware_facts(), &langs),
        model: "parakeet-tdt-0.6b-v3".into(),
        backend: BackendKind::Cpu,
        fallback: None,
        trials: Vec::new(),
    };
    record_check(&mut db, &check).unwrap();
    let before = host::transcription_setup(&db, &models, DESKTOP, &tiny).unwrap();
    assert_eq!(before.source, TranscriptionSource::ModelCheck);
    let reads = host::TranscriptionReads::read(&db).unwrap();
    // The store changes and goes away; the reads do not.
    set_model_settings(
        &mut db,
        &ModelSettings {
            transcription: named("large-v3-turbo"),
            ..ModelSettings::default()
        },
    )
    .unwrap();
    drop(db);
    let from = host::transcription_setup_from(reads, &models, DESKTOP, &tiny).unwrap();
    assert_eq!(from, before);
}

#[test]
fn settings_host_summary_request_follows_the_settings() {
    let dir = TempDir::new("settings-host");
    let mut db = store(&dir);
    let hw = machine(16 * GIB, Some(16 * GIB));
    let summary = host::summary_from_settings(
        &db,
        CPU,
        &hw,
        &NoApiKeys,
        crate::keep_local::SummaryPrivacy::Writer,
    )
    .unwrap();
    let (request, placement) = (summary.request, summary.placement);
    assert!(request.remote.is_none() && !request.skip);
    assert_eq!(request.local_model, None);
    assert!(!placement.gpu);
    let s = ModelSettings {
        summary: named(DEFAULT_SUMMARY_MODEL),
        summary_part: Part::Processor,
        ..ModelSettings::default()
    };
    set_model_settings(&mut db, &s).unwrap();
    let summary = host::summary_from_settings(
        &db,
        DESKTOP,
        &hw,
        &NoApiKeys,
        crate::keep_local::SummaryPrivacy::Writer,
    )
    .unwrap();
    let (request, placement) = (summary.request, summary.placement);
    assert_eq!(request.local_model.as_deref(), Some(DEFAULT_SUMMARY_MODEL));
    assert!(!placement.gpu, "forced to the processor");
}

/// Puts every file of `model` in `models` with its manifest size (contents not read).
fn install(models: &ModelDir, model: &str) {
    for a in crate::catalogue::Catalogue::embedded()
        .unwrap()
        .artefacts_of(model)
        .unwrap()
    {
        let f = a.file(models).unwrap();
        std::fs::create_dir_all(f.parent().unwrap()).unwrap();
        std::fs::File::create(&f).unwrap().set_len(a.size).unwrap();
    }
}

/// Asserts `m` is kept and its removal refused.
fn assert_kept(models: &ModelDir, keep: &[String], m: &str) {
    assert!(keep.contains(&m.to_owned()), "{m}: {keep:?}");
    assert_eq!(
        host::remove_model(models, m, keep),
        Err(CatalogueError::Chosen(m.into()))
    );
}

#[test]
fn settings_protected_models_keep_the_fallback_model() {
    let dir = TempDir::new("settings-remove");
    let db = store(&dir);
    let models = ModelDir::new(dir.0.join("models"));
    let hw = machine(16 * GIB, None);
    let keep = host::protected_models(&db, &models, CPU, &hw).unwrap();
    assert_kept(&models, &keep, FALLBACK_MODEL);
    // Kept even when no one passes it.
    assert_eq!(
        host::remove_model(&models, FALLBACK_MODEL, &[]),
        Err(CatalogueError::Chosen(FALLBACK_MODEL.into()))
    );
    // Any model no run would use goes (nothing installed: nothing freed).
    assert_eq!(
        host::remove_model(&models, "large-v3-turbo", &keep)
            .unwrap()
            .bytes_freed,
        0
    );
}

#[test]
fn settings_protected_models_keep_the_chosen_transcription_model() {
    let dir = TempDir::new("settings-remove");
    let mut db = store(&dir);
    let models = ModelDir::new(dir.0.join("models"));
    let hw = machine(16 * GIB, None);
    let s = ModelSettings {
        transcription: named("small.en"),
        ..ModelSettings::default()
    };
    set_model_settings(&mut db, &s).unwrap();
    let keep = host::protected_models(&db, &models, CPU, &hw).unwrap();
    assert_kept(&models, &keep, "small.en");
}

#[test]
fn settings_protected_models_keep_the_installed_recommendation() {
    let dir = TempDir::new("settings-remove");
    let db = store(&dir);
    let models = ModelDir::new(dir.0.join("models"));
    // 4 GiB on the processor: the recommendation is parakeet-tdt-110m.
    let hw = machine(4 * GIB, None);
    let used = host::transcription_setup(&db, &models, CPU, &hw).unwrap();
    assert_eq!(used.choice.model, FALLBACK_MODEL, "not installed yet");
    install(&models, "parakeet-tdt-110m");
    let used = host::transcription_setup(&db, &models, CPU, &hw).unwrap();
    assert_eq!(used.choice.model, "parakeet-tdt-110m");
    let keep = host::protected_models(&db, &models, CPU, &hw).unwrap();
    assert_kept(&models, &keep, "parakeet-tdt-110m");
    assert!(models.root().exists());
}

#[test]
fn settings_protected_models_keep_the_stored_checks_model_and_fallback() {
    let dir = TempDir::new("settings-remove");
    let mut db = store(&dir);
    let models = ModelDir::new(dir.0.join("models"));
    let hw = machine(16 * GIB, None);
    // Out of date for this machine, still kept (a meeting may use it once it holds again).
    let check = StoredCheck {
        fingerprint: "old".into(),
        model: "parakeet-tdt-0.6b-v2".into(),
        backend: BackendKind::Cpu,
        fallback: Some("tiny.en".into()),
        trials: Vec::new(),
    };
    record_check(&mut db, &check).unwrap();
    let keep = host::protected_models(&db, &models, CPU, &hw).unwrap();
    assert_kept(&models, &keep, "parakeet-tdt-0.6b-v2");
    assert_kept(&models, &keep, "tiny.en");
}

#[test]
fn settings_protected_models_keep_the_summary_model_a_run_would_use() {
    let dir = TempDir::new("settings-remove");
    let mut db = store(&dir);
    let models = ModelDir::new(dir.0.join("models"));
    // A desktop-like machine: lots of memory and a 16 GiB card.
    let hw = machine(64 * GIB, Some(16 * GIB));
    let keep = host::protected_models(&db, &models, DESKTOP, &hw).unwrap();
    let placement = host::local_summary_placement(&db, DESKTOP, &hw).unwrap();
    let used = host::summary_model_for(&placement, &hw).unwrap().model.id;
    assert_kept(&models, &keep, used);
    let other = if used == DEFAULT_SUMMARY_MODEL {
        LOW_MEMORY_SUMMARY_MODEL
    } else {
        DEFAULT_SUMMARY_MODEL
    };
    assert!(!keep.contains(&other.to_owned()), "{keep:?}");
    // Summaries forced to the processor use the 4B: that one is kept, not the 9B.
    let s = ModelSettings {
        summary_part: Part::Processor,
        ..ModelSettings::default()
    };
    set_model_settings(&mut db, &s).unwrap();
    let keep = host::protected_models(&db, &models, DESKTOP, &hw).unwrap();
    assert_kept(&models, &keep, LOW_MEMORY_SUMMARY_MODEL);
    assert!(
        !keep.contains(&DEFAULT_SUMMARY_MODEL.to_owned()),
        "{keep:?}"
    );
    // A chosen summary model is kept.
    let s = ModelSettings {
        summary: named(DEFAULT_SUMMARY_MODEL),
        summary_part: Part::Processor,
        ..ModelSettings::default()
    };
    set_model_settings(&mut db, &s).unwrap();
    let keep = host::protected_models(&db, &models, DESKTOP, &hw).unwrap();
    assert_kept(&models, &keep, DEFAULT_SUMMARY_MODEL);
}

#[test]
fn settings_one_graphics_card_fit_rule_for_transcription_and_summaries() {
    // Twice the file size must fit the card's dedicated memory, for both tasks.
    let card = |gib: u64| machine(64 * GIB, Some(gib * GIB));
    let size = 3 * GIB;
    assert!(fits_graphics_card(size, &card(6)));
    assert!(!fits_graphics_card(size, &card(5)));
    assert!(
        fits_graphics_card(size, &machine(64 * GIB, None)),
        "unknown memory"
    );
    // The 4B summary model (2.74 GB) needs more than a 4 GiB card under the shared rule.
    let four_b = summary_model::summary_model(LOW_MEMORY_SUMMARY_MODEL)
        .unwrap()
        .size;
    assert!(!fits_graphics_card(four_b, &card(4)));
    assert!(fits_graphics_card(four_b, &card(6)));
    // Transcription's placement follows the same function.
    let v2 = model_check::model_facts("parakeet-tdt-0.6b-v2")
        .unwrap()
        .size;
    for gib in [4, 5, 6] {
        let p = transcription_placement(
            "parakeet-tdt-0.6b-v2",
            Part::GraphicsCard,
            DESKTOP.to_vec(),
            DESKTOP,
            &card(gib),
        );
        assert_eq!(
            p.order == [Accelerator::Cuda, Accelerator::Cpu],
            fits_graphics_card(v2, &card(gib)),
            "{gib} GiB"
        );
    }
    if summary_model::summary_gpu_planned(DESKTOP) {
        let s = summary_placement(
            &named(LOW_MEMORY_SUMMARY_MODEL),
            Part::GraphicsCard,
            DESKTOP,
            &card(4),
        )
        .unwrap();
        assert!(!s.gpu);
        let s = summary_placement(
            &named(LOW_MEMORY_SUMMARY_MODEL),
            Part::GraphicsCard,
            DESKTOP,
            &card(6),
        )
        .unwrap();
        assert!(s.gpu);
    }
}

#[test]
fn settings_notes_and_refusals_name_models_by_display_name() {
    let hardware = [
        machine(16 * GIB, Some(16 * GIB)),
        machine(16 * GIB, Some(2 * GIB)),
        machine(8 * GIB, None),
    ];
    let parts = [
        Part::Automatic,
        Part::GraphicsCard,
        Part::Npu,
        Part::Processor,
    ];
    let mut notes = 0;
    for hw in &hardware {
        for build in [CPU, SURFACE, DESKTOP, VULKAN] {
            for m in MODELS {
                for part in parts {
                    let p = transcription_placement(m.name, part, build.to_vec(), build, hw);
                    if let Some(n) = p.note {
                        notes += 1;
                        assert_eq!(crate::catalogue::manifest_id_in(&n), None, "{n}");
                    }
                }
            }
            for pick in [
                ModelPick::Automatic,
                named(DEFAULT_SUMMARY_MODEL),
                named(LOW_MEMORY_SUMMARY_MODEL),
            ] {
                for part in parts {
                    if let Some(n) = summary_placement(&pick, part, build, hw).unwrap().note {
                        notes += 1;
                        assert_eq!(crate::catalogue::manifest_id_in(&n), None, "{n}");
                    }
                }
            }
        }
    }
    assert!(notes > 0, "the sweep produced notes");
    let refused = SettingsError::ModelLanguages {
        model: "parakeet-tdt-110m".into(),
        missing: vec!["de".into()],
    }
    .to_string();
    assert_eq!(
        crate::catalogue::manifest_id_in(&refused),
        None,
        "{refused}"
    );
}

/// The desktop as DXCore reports it: the integrated Radeon first, then the RTX 5080.
fn desktop_gpus() -> Hardware {
    let mut hw = machine(64 * GIB, Some(16 * GIB));
    hw.gpus[0].name = "NVIDIA GeForce RTX 5080".to_owned();
    hw.gpus[0].integrated = Some(false);
    let mut radeon = hw.gpus[0].clone();
    radeon.name = "AMD Radeon(TM) Graphics".to_owned();
    radeon.dedicated_memory = Some(512 << 20);
    radeon.integrated = Some(true);
    hw.gpus.insert(0, radeon);
    hw
}

const VULKAN_FIRST: &[Accelerator] = &[Accelerator::Vulkan, Accelerator::Cpu];

fn on_vulkan(pick: &ModelPick, part: Part, hw: &Hardware) -> SummaryPlacement {
    placement_when(true, pick, part, VULKAN_FIRST, hw).unwrap()
}

#[test]
fn settings_summary_on_vulkan_takes_the_discrete_gpu_of_the_desktop() {
    let hw = desktop_gpus();
    assert!(has_discrete_gpu(&hw));
    assert_eq!(integrated_gpu_names(&hw), ["AMD Radeon(TM) Graphics"]);
    let s = on_vulkan(&ModelPick::Automatic, Part::Automatic, &hw);
    assert_eq!((s.gpu, s.note), (true, None));
    let s = on_vulkan(&ModelPick::Automatic, Part::GraphicsCard, &hw);
    assert_eq!((s.gpu, s.note), (true, None));
    // The memory that counts is the discrete card's, not the Radeon's.
    assert_eq!(graphics_memory(&hw), Some(16 * GIB));
}

#[test]
fn settings_summary_on_vulkan_stays_on_the_processor_without_a_discrete_gpu() {
    // An integrated GPU only, in every way it can be reported.
    let mut igpu = desktop_gpus();
    igpu.gpus.remove(1);
    let mut unknown_no_memory = igpu.clone();
    unknown_no_memory.gpus[0].integrated = None;
    unknown_no_memory.gpus[0].dedicated_memory = None;
    let mut unreadable = igpu.clone();
    unreadable.gpus.clear();
    for hw in [&igpu, &unknown_no_memory, &unreadable] {
        assert!(!has_discrete_gpu(hw), "{hw:?}");
        let s = on_vulkan(&ModelPick::Automatic, Part::Automatic, hw);
        assert_eq!((s.gpu, s.note), (false, None), "{hw:?}");
        let s = on_vulkan(&ModelPick::Automatic, Part::GraphicsCard, hw);
        assert!(!s.gpu);
        assert!(s.note.unwrap().contains("no separate graphics card"));
    }
    // An adapter of unknown kind with memory of its own counts.
    let mut unknown = desktop_gpus();
    unknown.gpus.remove(0);
    unknown.gpus[0].integrated = None;
    assert!(has_discrete_gpu(&unknown));
    assert!(on_vulkan(&ModelPick::Automatic, Part::Automatic, &unknown).gpu);
}

#[test]
fn settings_summary_on_vulkan_needs_room_on_the_card() {
    let mut hw = desktop_gpus();
    // 8 GiB: automatic takes the 4B on the card.
    hw.gpus[1].dedicated_memory = Some(8 * GIB);
    assert!(on_vulkan(&ModelPick::Automatic, Part::Automatic, &hw).gpu);
    // The 9B by name does not fit: the processor, and the reason is shown.
    let nine = named(DEFAULT_SUMMARY_MODEL);
    let s = on_vulkan(&nine, Part::Automatic, &hw);
    assert!(!s.gpu);
    assert!(s.note.unwrap().contains("too big"));
    // 4 GiB: neither fits, and choosing for the user says nothing.
    hw.gpus[1].dedicated_memory = Some(4 * GIB);
    let s = on_vulkan(&ModelPick::Automatic, Part::Automatic, &hw);
    assert_eq!((s.gpu, s.note), (false, None));
    // Unknown memory is not held against a model.
    hw.gpus[1].dedicated_memory = None;
    assert!(has_discrete_gpu(&hw));
    assert!(on_vulkan(&ModelPick::Automatic, Part::Automatic, &hw).gpu);
}

#[test]
fn settings_summary_on_cuda_keeps_its_rule_and_a_processor_choice_always_wins() {
    let cuda = &[Accelerator::Cuda, Accelerator::Cpu];
    // Automatic on CUDA was never held to the device rule.
    let mut igpu = desktop_gpus();
    igpu.gpus.remove(1);
    let s = placement_when(true, &ModelPick::Automatic, Part::Automatic, cuda, &igpu).unwrap();
    assert_eq!((s.gpu, s.note), (true, None));
    let hw = desktop_gpus();
    let s = on_vulkan(&ModelPick::Automatic, Part::Processor, &hw);
    assert_eq!((s.gpu, s.note), (false, None));
    let s = placement_when(
        false,
        &ModelPick::Automatic,
        Part::Automatic,
        VULKAN_FIRST,
        &hw,
    )
    .unwrap();
    assert_eq!((s.gpu, s.note), (false, None));
}
