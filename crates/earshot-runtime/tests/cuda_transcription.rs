//! T8.4ad: saying "use CUDA for transcription this run". Its own binary, with one test, because
//! the choice is one per process. A build with `onnx-cuda` exercises the choice itself; every other
//! build proves the choice is refused, never a silent CPU run reported as CUDA. A `cuda` build is
//! a developer's, where the choice changes nothing. The files are a test manifest's: the choice
//! reads every byte against the manifest it is given.
//!
//! Default ci runs only the refusal half. The whole test, needing no GPU or NVIDIA file:
//! `cargo test -p earshot-runtime --features onnx-cuda --test cuda_transcription`

#![cfg(not(feature = "cuda"))]

use std::path::PathBuf;

use earshot_runtime::RuntimeError;
use earshot_runtime::backends::{
    Accelerator, ModelDir, accelerator_order, asr_model, init_onnx, model_can_run_on, model_order,
    onnx_session_config,
};
use earshot_runtime::catalogue::Catalogue;
use earshot_runtime::cuda::{
    CudaPart, folder, transcription_from_folder, use_for_transcription_in, verify_part_in,
};
use earshot_runtime::model_bench::{check_fingerprint, hardware_facts};

fn scratch() -> PathBuf {
    let d = std::env::temp_dir().join(format!("earshot-cuda-choice-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// The bytes of a test manifest's file `name`.
fn bytes_of(name: &str) -> Vec<u8> {
    format!("the bytes of {name}").into_bytes()
}

/// A manifest whose parts pin every required file, with [`bytes_of`] as their bytes.
fn test_manifest() -> Catalogue {
    let mut text = "sets = [\"s\"]\nhosts = [\"example.org\"]\n\n".to_owned();
    for (i, (id, part)) in [("a", "summaries"), ("b", "transcription")]
        .into_iter()
        .enumerate()
    {
        text.push_str(&format!(
            "[[artefact]]\nid = \"{id}\"\nsets = [\"s\"]\nlocation = \"cache\"\n\
             url = \"https://example.org/{id}\"\npath = \"archives/{id}.zip\"\n\
             sha256 = \"{}\"\nsize = 1\nlicence = \"x\"\nattribution = \"x\"\n\n",
            "0".repeat(64)
        ));
        text.push_str(&format!(
            "[[cuda_archive]]\npart = \"{part}\"\nartefact = \"{id}\"\nformat = \"zip\"\n\n"
        ));
        for name in CudaPart::ALL[i].required_files() {
            let bytes = bytes_of(name);
            text.push_str(&format!(
                "[[cuda_archive.file]]\nname = \"{name}\"\nmember = \"bin/{name}\"\n\
                 sha256 = \"{}\"\nsize = {}\n\n",
                sha256_hex(&bytes),
                bytes.len()
            ));
        }
    }
    Catalogue::parse(&text).unwrap_or_else(|e| panic!("{e}\n{text}"))
}

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn is_unavailable(r: Result<(), RuntimeError>) -> bool {
    matches!(r, Err(RuntimeError::Unavailable(_)))
}

#[test]
fn cuda_transcription_choice_is_refused_or_changes_only_parakeet() {
    let root = scratch();
    let models = ModelDir::new(&root);
    let dir = folder(&models);
    std::fs::create_dir_all(&dir).unwrap();
    let before = accelerator_order();

    // Not chosen: CUDA is refused, whatever the build holds.
    assert!(!before.contains(&Accelerator::Cuda));
    assert!(is_unavailable(
        init_onnx(&models, Accelerator::Cuda).map(|_| ())
    ));
    assert!(is_unavailable(
        onnx_session_config(Accelerator::Cuda, 1).map(|_| ())
    ));

    let manifest = test_manifest();
    let fingerprint_before = check_fingerprint(&models, &hardware_facts(), &["en"]);

    // One file short: refused and not recorded.
    let files: Vec<&str> = CudaPart::ALL
        .iter()
        .flat_map(|p| p.required_files().iter().copied())
        .collect();
    for f in &files[1..] {
        std::fs::write(dir.join(f), bytes_of(f)).unwrap();
    }
    assert!(is_unavailable(use_for_transcription_in(
        &manifest, &models, true
    )));
    assert!(!transcription_from_folder());
    assert_eq!(accelerator_order(), before);

    // Every file there with its right size and one with the wrong bytes: its SHA-256 refuses it.
    let wrong = vec![b'x'; bytes_of(files[0]).len()];
    std::fs::write(dir.join(files[0]), wrong).unwrap();
    let refused = verify_part_in(&manifest, &models, CudaPart::Transcription).unwrap_err();
    assert!(
        refused.to_string().contains("failed its check"),
        "{refused}"
    );
    assert!(is_unavailable(use_for_transcription_in(
        &manifest, &models, true
    )));
    assert!(!transcription_from_folder());
    assert_eq!(accelerator_order(), before);

    std::fs::write(dir.join(files[0]), bytes_of(files[0])).unwrap();
    let chosen = use_for_transcription_in(&manifest, &models, true);
    if !cfg!(feature = "onnx-cuda") {
        // No provider in this build, with every file there.
        assert!(is_unavailable(chosen));
        assert!(!transcription_from_folder());
        assert_eq!(accelerator_order(), before);
        std::fs::remove_dir_all(&root).unwrap();
        return;
    }
    chosen.unwrap();
    assert!(transcription_from_folder());

    // The check's fingerprint follows the choice and nothing else, so a parent and a child that
    // made the same choice agree (the folder's own state is not in it).
    let chosen_fingerprint = check_fingerprint(&models, &hardware_facts(), &["en"]);
    assert_ne!(chosen_fingerprint, fingerprint_before);
    assert_eq!(
        hardware_facts().accelerators[0].backend,
        earshot_core::asr::BackendKind::Cuda
    );
    std::fs::write(dir.join("a-note.txt"), b"not a library").unwrap();
    assert_eq!(
        check_fingerprint(&models, &hardware_facts(), &["en"]),
        chosen_fingerprint
    );

    // CUDA leads the order, for Parakeet only.
    let order = accelerator_order();
    assert_eq!(order.first(), Some(&Accelerator::Cuda));
    assert_eq!(order[1..], before);
    let parakeet = asr_model("parakeet-tdt-0.6b-v2").unwrap();
    let whisper = asr_model("base.en").unwrap();
    assert_eq!(model_order(&parakeet, &order), order);
    assert_eq!(model_order(&whisper, &order), before);
    assert!(model_can_run_on("parakeet-tdt-0.6b-v2", Accelerator::Cuda));
    assert!(!model_can_run_on("base.en", Accelerator::Cuda));
    assert!(!model_can_run_on("large-v3-turbo", Accelerator::Cuda));
    // Neither the check nor a recommendation tries Whisper on CUDA.
    use earshot_core::asr::BackendKind;
    use earshot_runtime::model_check::candidates;
    assert_eq!(
        candidates(BackendKind::Cuda, &["en"]),
        ["parakeet-tdt-0.6b-v2", "parakeet-tdt-110m"]
    );
    assert_eq!(
        candidates(BackendKind::Cuda, &["de"]),
        ["parakeet-tdt-0.6b-v3"]
    );

    // A forced graphics card takes Whisper to Vulkan, never to CUDA.
    let placed = earshot_runtime::model_settings::transcription_placement(
        "base.en",
        earshot_runtime::model_settings::Part::GraphicsCard,
        vec![Accelerator::Cuda, Accelerator::Vulkan, Accelerator::Cpu],
        &[Accelerator::Cuda, Accelerator::Vulkan, Accelerator::Cpu],
        &earshot_hardware::Hardware::default(),
    );
    assert_eq!(placed.order, [Accelerator::Vulkan, Accelerator::Cpu]);

    // Summaries ignore ONNX Runtime's CUDA: what follows it decides.
    assert_eq!(
        earshot_runtime::summary_model::summary_gpu_planned(&order),
        earshot_runtime::summary_model::summary_gpu_planned(&before)
    );

    // Without Vulkan, a Whisper model forced to the card is told why, not that there is no card.
    let placed = earshot_runtime::model_settings::transcription_placement(
        "base.en",
        earshot_runtime::model_settings::Part::GraphicsCard,
        vec![Accelerator::Cuda, Accelerator::Cpu],
        &[Accelerator::Cuda, Accelerator::Cpu],
        &earshot_hardware::Hardware::default(),
    );
    assert_eq!(placed.order, [Accelerator::Cpu]);
    let note = placed.note.unwrap();
    assert!(note.contains("no CUDA version"), "{note}");

    // The provider is asked for. Files that do not load are refused before ONNX Runtime loads, and
    // the one caller that heard it reports it.
    assert!(onnx_session_config(Accelerator::Cuda, 1).is_ok());
    let mut heard = Vec::new();
    let result = earshot_runtime::host::init_onnx_for(
        &models,
        "parakeet-tdt-0.6b-v2",
        &order,
        &mut |library, e| heard.push((library, matches!(e, RuntimeError::Onnx(_)))),
    );
    assert_eq!(heard, [(Accelerator::Cuda, true)]);
    // The retry asked for the CPU library, which this scratch folder does not hold.
    assert!(matches!(result, Err(RuntimeError::ModelMissing { .. })));
    assert_eq!(earshot_onnx::loaded_library(), None);

    // CUDA is off for the rest of the process: the order, the sessions and a setup read earlier.
    assert!(earshot_runtime::cuda::load_failed());
    assert!(!transcription_from_folder());
    assert_eq!(accelerator_order(), before);
    assert_eq!(model_order(&parakeet, &order), before);
    assert!(is_unavailable(
        onnx_session_config(Accelerator::Cuda, 1).map(|_| ())
    ));
    assert!(is_unavailable(use_for_transcription_in(
        &manifest, &models, true
    )));
    assert_eq!(
        check_fingerprint(&models, &hardware_facts(), &["en"]),
        fingerprint_before
    );
    // So a later meeting hears nothing.
    let mut later = 0;
    let _ = earshot_runtime::host::init_onnx_for(
        &models,
        "parakeet-tdt-0.6b-v2",
        &accelerator_order(),
        &mut |_, _| later += 1,
    );
    assert_eq!(later, 0);
    std::fs::remove_dir_all(&root).unwrap();
}
