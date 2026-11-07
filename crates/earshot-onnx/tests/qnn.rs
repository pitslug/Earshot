//! The QNN execution provider through Qualcomm's plugin (T8.3), run on this host: on x64 the
//! plugin's HTP simulator (`cargo xtask fetch-models --set qnn-x64`), on Windows ARM64 the
//! NPU (`--set qnn`). Built only with `--features qnn`.
//!
//! These tests prove the provider path and the evidence rule on real ONNX Runtime placement,
//! not NPU speed or accuracy (GATE-03a).

#![cfg(feature = "qnn")]

mod common;

use earshot_core::asr::BackendKind;
use earshot_onnx::qnn::{QNN_HOST, QNN_HOST_SET, QnnHardware, register_qnn, registered_qnn};
use earshot_onnx::{CPU_EP, OnnxSession, Provider, QNN_EP, SessionConfig};

fn registered() -> &'static earshot_onnx::qnn::QnnDevice {
    common::init_runtime();
    let models = common::models_dir().expect("EARSHOT_MODELS or LOCALAPPDATA");
    register_qnn(&models, &QNN_HOST).unwrap_or_else(|e| {
        panic!("QNN plugin: {e} (run `cargo xtask fetch-models --set {QNN_HOST_SET}`)")
    })
}

#[test]
fn qnn_plugin_registers_the_host_device() {
    let device = registered();
    let want = if cfg!(target_arch = "aarch64") {
        QnnHardware::Npu
    } else {
        QnnHardware::Simulator
    };
    assert_eq!(device.hardware, want);
    assert!(device.backend_path.ends_with("QnnHtp.dll"));
    assert!(device.backend_path.is_absolute());
    assert_eq!(registered_qnn(), Some(device));
    // Registering the same package again is the same registration.
    let models = common::models_dir().unwrap();
    assert_eq!(register_qnn(&models, &QNN_HOST).unwrap(), device);
}

#[test]
fn qnn_a_failed_registration_is_not_remembered() {
    common::init_runtime();
    // A package that is not there fails; the real one still registers afterwards (a failure
    // is never cached for the life of the process).
    let missing = earshot_onnx::qnn::QnnPackage {
        dir: "no-such-qnn-package",
        ..QNN_HOST
    };
    let models = common::models_dir().unwrap();
    assert!(register_qnn(&models, &missing).is_err());
    registered();
    assert!(registered_qnn().is_some());
}

#[test]
fn qnn_registration_leaves_cpu_sessions_on_the_cpu() {
    registered();
    // VAD, speaker models and Parakeet's per-token steps keep running on the CPU EP in a
    // process that registered the plugin.
    let session =
        OnnxSession::load(&common::model(common::SILERO), &SessionConfig::cpu(1)).unwrap();
    let backend = session.backend();
    assert_eq!(backend.requested, Provider::Cpu);
    assert_eq!(backend.placement.nodes_on(QNN_EP), 0, "{backend}");
    assert!(backend.check_partition(0).is_ok(), "{backend}");
}

#[test]
#[ignore = "needs the NPU conversion and is slow on the x64 HTP simulator (about 2 min to load): cargo xtask qnn-convert --model parakeet-tdt-110m, then cargo test --release -p earshot-onnx --features qnn --test qnn qnn_parakeet -- --include-ignored"]
fn qnn_parakeet_conversion_runs_on_qnn_with_its_documented_partition() {
    registered();
    let models = common::models_dir().unwrap();
    let dir = models.join("parakeet-qnn").join("parakeet-tdt-110m");
    assert!(
        dir.join("encoder.onnx").is_file(),
        "{} missing; run `cargo xtask qnn-convert --model parakeet-tdt-110m`",
        dir.display()
    );
    let encoder = SessionConfig {
        provider: Provider::Qnn,
        intra_threads: Some(2),
        ..SessionConfig::default()
    };
    let t = earshot_onnx::parakeet::ParakeetTdt::load_split(
        &dir,
        &encoder,
        &SessionConfig::cpu(2),
        "parakeet-tdt-110m (NPU conversion)",
        true,
    )
    .unwrap();
    assert!(t.is_static());
    let sessions = t.backends();
    let enc = sessions[0];
    assert_eq!(enc.kind(), Some(BackendKind::Qnn), "{enc}");
    enc.check_partition_ops(&earshot_onnx::parakeet::QNN_ENCODER_CPU_OPS)
        .unwrap_or_else(|e| panic!("{e}"));
    // Without its documented partition the encoder is refused: the boundary is real.
    assert!(enc.check_partition(0).is_err(), "{enc}");
    for s in &sessions[1..] {
        assert!(s.check_partition(0).is_ok(), "{s}");
        assert_eq!(s.requested, Provider::Cpu);
    }
}

#[test]
fn qnn_float_dynamic_model_is_mostly_left_on_the_cpu_and_refused() {
    registered();
    let config = SessionConfig {
        provider: Provider::Qnn,
        intra_threads: Some(1),
        ..SessionConfig::default()
    };
    // Silero VAD is a float model with dynamic shapes: the HTP backend takes almost nothing.
    let session = OnnxSession::load(&common::model(common::SILERO), &config).unwrap();
    let backend = session.backend();
    assert_eq!(backend.requested, Provider::Qnn);
    let (qnn, cpu) = (
        backend.placement.nodes_on(QNN_EP),
        backend.placement.nodes_on(CPU_EP),
    );
    assert!(qnn > 0, "the QNN EP took part: {backend}");
    assert!(cpu > qnn, "most nodes stay on the CPU: {backend}");
    assert_eq!(backend.kind(), Some(BackendKind::Qnn));
    let err = backend.check_partition(0).unwrap_err();
    assert!(err.contains("CPUExecutionProvider="), "{err}");
}
