//! T8.4f: an NPU whose QNN libraries refuse to load. Its own binary, with one test, because the
//! refusal is kept for the rest of the process. The failure is injected by an empty models folder:
//! a build without `qnn` refuses the NPU outright, a `qnn` build finds no library to load. Either
//! way the one caller that heard it reports it, the NPU leaves every order, and nothing later
//! repeats the warning or reports a QNN run.
//!
//! `cargo test -p earshot-runtime --test qnn_fallback` (also with `--features qnn`).

use earshot_runtime::RuntimeError;
use earshot_runtime::backends::{
    Accelerator, ModelDir, accelerator_order, asr_model, model_can_run_on, model_order,
    onnx_library, onnx_library_for_all, qnn_load_failed,
};

#[test]
fn backends_qnn_library_that_does_not_load_is_reported_once_and_left_off() {
    let root = std::env::temp_dir().join(format!("earshot-qnn-fallback-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    let models = ModelDir::new(root.clone());
    let surface = [Accelerator::Qnn, Accelerator::Cpu];
    let parakeet = asr_model("parakeet-tdt-0.6b-v2").unwrap();
    // Before: the NPU is offered to Parakeet.
    assert!(!qnn_load_failed());
    assert_eq!(model_order(&parakeet, &surface), surface);
    assert_eq!(
        onnx_library("parakeet-tdt-0.6b-v2", &surface),
        Accelerator::Qnn
    );

    let mut heard = Vec::new();
    let result = earshot_runtime::host::init_onnx_for(
        &models,
        "parakeet-tdt-0.6b-v2",
        &surface,
        &mut |library, e| heard.push((library, e.to_string())),
    );
    assert_eq!(heard.len(), 1, "{heard:?}");
    assert_eq!(heard[0].0, Accelerator::Qnn);
    // The retry asked for the CPU library, which this scratch folder does not hold.
    assert!(
        matches!(result, Err(RuntimeError::ModelMissing { .. })),
        "{result:?}"
    );

    // The NPU is off for the rest of the process: the build's order, a setup read earlier, the
    // library choice and the Models page's "can it run there".
    assert!(qnn_load_failed());
    assert!(!accelerator_order().contains(&Accelerator::Qnn));
    assert_eq!(model_order(&parakeet, &surface), [Accelerator::Cpu]);
    assert!(!model_can_run_on("parakeet-tdt-0.6b-v2", Accelerator::Qnn));
    assert_eq!(
        onnx_library("parakeet-tdt-0.6b-v2", &surface),
        Accelerator::Cpu
    );
    assert_eq!(onnx_library_for_all(&surface), Accelerator::Cpu);

    // So a later meeting, re-transcription or model check hears nothing: it goes straight to the
    // CPU library.
    let mut later = 0;
    let again = earshot_runtime::host::init_onnx_for(
        &models,
        "parakeet-tdt-0.6b-v2",
        &surface,
        &mut |_, _| later += 1,
    );
    assert!(matches!(again, Err(RuntimeError::ModelMissing { .. })));
    let check = earshot_runtime::host::init_onnx_for_check(&models, &surface, &mut |_, _| {
        later += 1;
    });
    assert!(matches!(check, Err(RuntimeError::ModelMissing { .. })));
    assert_eq!(later, 0);
    std::fs::remove_dir_all(&root).unwrap();
}
