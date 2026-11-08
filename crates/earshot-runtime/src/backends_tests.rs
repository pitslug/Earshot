//! Backend tests. Every name contains `backends`. Tests that load ONNX models need the
//! `ci` model set and fail with the fetch instruction when it is missing (AGENTS.md); the
//! whisper loader is a fake until `earshot_asr_whisper::evidence` is on main.

use std::cell::RefCell;
use std::ffi::OsString;
use std::path::{Path, PathBuf};

use earshot_core::asr::{
    AsrError, BackendInfo, BackendKind, CancelToken, FakeTranscriber, Transcriber, Vad,
};
use earshot_core::audio::{MonoBuffer, PIPELINE_RATE};
use earshot_core::capture::TrackId;
use earshot_core::diar::{SpeakerEmbedder, SpeakerSegmenter};
use earshot_core::identity::{Clock, EntityId, ManualClock};
use earshot_core::model::MeetingRepo;
use earshot_core::model::TranscriptPass;
use earshot_store::{DataDir, Store};
use rand_chacha::ChaCha8Rng;
use rand_core::SeedableRng;

use super::*;
use crate::post::PostConfig;

/// A loader that refuses the backends in `fail` (as `load_verified` does when the log and
/// `backend_info` disagree) and records what it was asked for.
struct FakeLoader {
    fail: Vec<BackendKind>,
    asked: RefCell<Vec<(PathBuf, BackendKind)>>,
}

impl FakeLoader {
    fn failing(fail: &[BackendKind]) -> Self {
        Self {
            fail: fail.to_vec(),
            asked: RefCell::new(Vec::new()),
        }
    }
}

impl WhisperLoader for FakeLoader {
    type Transcriber = FakeTranscriber;
    fn load_verified(
        &self,
        model: &Path,
        expected: BackendKind,
    ) -> Result<FakeTranscriber, AsrError> {
        self.asked
            .borrow_mut()
            .push((model.to_path_buf(), expected));
        if self.fail.contains(&expected) {
            return Err(refused(expected));
        }
        Ok(FakeTranscriber::scripted([]))
    }
}

fn refused(expected: BackendKind) -> AsrError {
    AsrError::Backend(format!(
        "{expected:?} requested but whisper.cpp fell back to the CPU"
    ))
}

/// A thread-safe fake loader for factories: refuses `fail`, then loads.
struct SendLoader(Vec<BackendKind>);

impl WhisperLoader for SendLoader {
    type Transcriber = FakeTranscriber;
    fn load_verified(&self, _: &Path, expected: BackendKind) -> Result<FakeTranscriber, AsrError> {
        if self.0.contains(&expected) {
            return Err(refused(expected));
        }
        Ok(FakeTranscriber::scripted([]))
    }
}

/// The production models directory: `EARSHOT_MODELS`, else `%LOCALAPPDATA%\Earshot\models`.
fn models() -> ModelDir {
    let data = DataDir::from_local_app_data()
        .expect("LOCALAPPDATA is not set; set EARSHOT_MODELS to the models directory");
    ModelDir::from_env(&data)
}

/// Tests that load whisper through the evidence loader run one at a time: the evidence of
/// a load is the process-wide log captured during it (AGENTS.md: tests touching a
/// process-wide buffer hold a shared test lock). Every test of the crate that loads
/// whisper takes it ([`whisper_loads`]).
static WHISPER_LOADS: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// The lock of [`WHISPER_LOADS`], also after a test panicked while holding it.
pub(crate) fn whisper_loads() -> std::sync::MutexGuard<'static, ()> {
    WHISPER_LOADS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Unwraps, panicking with the error's message (the fetch instruction for a missing
/// model).
fn ok<T>(r: Result<T, RuntimeError>) -> T {
    r.unwrap_or_else(|e| panic!("{e}"))
}

#[test]
fn backends_model_dir_prefers_earshot_models_and_names_the_fetch_set() {
    let data = DataDir::new("C:/data");
    assert_eq!(ModelDir::resolve(None, &data).root(), data.models_dir());
    assert_eq!(
        ModelDir::resolve(Some(OsString::new()), &data).root(),
        data.models_dir()
    );
    assert_eq!(
        ModelDir::resolve(Some(OsString::from("D:/models")), &data).root(),
        Path::new("D:/models")
    );
    let empty = ModelDir::new(std::env::temp_dir().join("earshot-runtime-no-models"));
    let err = empty.require(SILERO).unwrap_err();
    let text = err.to_string();
    assert!(matches!(err, RuntimeError::ModelMissing { .. }));
    assert!(text.contains("cargo xtask fetch-models --set ci"), "{text}");
    assert!(text.contains("silero_vad.onnx"), "{text}");
    let missing = |name| {
        empty
            .require(&whisper_model(name).unwrap())
            .unwrap_err()
            .to_string()
    };
    // PLAN D22: defaults in ci and desktop, the models offered but never tested in choice.
    assert!(missing("base.en").contains("--set ci"));
    assert!(missing("small.en").contains("--set desktop"));
    assert!(missing("large-v3-turbo").contains("--set choice"));
    assert!(missing("tiny.en").contains("--set choice"));
    // A file that no manifest entry provides says so instead of naming a set.
    let other = empty
        .require("whisper/ggml-mine.bin")
        .unwrap_err()
        .to_string();
    assert!(other.contains("not in models/manifest.toml"), "{other}");
}

#[test]
fn backends_whisper_names_must_be_manifest_models() {
    assert_eq!(
        whisper_model("base.en").unwrap(),
        "whisper/ggml-base.en.bin"
    );
    for bad in ["medium.en", "../../secrets", "base.en/../x", "", "C:\\x"] {
        assert!(
            matches!(whisper_model(bad), Err(RuntimeError::UnknownModel(_))),
            "{bad:?}"
        );
    }
}

#[test]
fn backends_order_follows_the_build() {
    use Accelerator::{Cpu, Cuda, Vulkan};
    let order = accelerator_order();
    assert_eq!(order.last(), Some(&Cpu), "{order:?}");
    assert_eq!(order.contains(&Cuda), cfg!(feature = "cuda"), "{order:?}");
    if cfg!(feature = "vulkan") {
        // Vulkan only when whisper.cpp sees a Vulkan device.
        let device = earshot_asr_whisper::gpu_devices()
            .iter()
            .any(|d| d.backend == BackendKind::Vulkan);
        assert_eq!(order.contains(&Vulkan), device, "{order:?}");
    } else {
        assert!(!order.contains(&Vulkan), "{order:?}");
    }
    // Vulkan, when present, comes after CUDA and right before the CPU.
    if let Some(v) = order.iter().position(|a| *a == Vulkan) {
        assert!(
            order[..v]
                .iter()
                .all(|a| *a == Cuda || *a == Accelerator::Qnn),
            "{order:?}"
        );
        assert_eq!(order[v + 1..], [Cpu], "{order:?}");
    }
    // QNN only in ARM64 `qnn` builds, right after CUDA (T8.3, `with_npu`).
    assert_eq!(
        order.contains(&Accelerator::Qnn),
        cfg!(all(feature = "qnn", target_arch = "aarch64")),
        "{order:?}"
    );
    if !cfg!(feature = "cuda")
        && !cfg!(feature = "vulkan")
        && !cfg!(all(feature = "qnn", target_arch = "aarch64"))
    {
        assert_eq!(order, [Cpu]);
    }
}

#[test]
fn backends_qnn_comes_first_only_on_arm64() {
    // PLAN.md 3.4 / D20: NPU > Vulkan > CPU on the Surface (an ARM64 `qnn` build).
    assert_eq!(
        with_npu(order_for(false, false), true),
        [Accelerator::Qnn, Accelerator::Cpu]
    );
    assert_eq!(
        with_npu(order_for(false, true), true),
        [Accelerator::Qnn, Accelerator::Vulkan, Accelerator::Cpu]
    );
    assert_eq!(
        with_npu(order_for(true, false), true),
        [Accelerator::Cuda, Accelerator::Qnn, Accelerator::Cpu]
    );
    // Without the NPU (x64 builds: the QNN package is the HTP simulator) nothing changes.
    assert_eq!(
        with_npu(order_for(true, true), false),
        order_for(true, true)
    );
    assert_eq!(Accelerator::Qnn.backend(), BackendKind::Qnn);
    assert_eq!(Accelerator::of(BackendKind::Qnn), Some(Accelerator::Qnn));
}

#[test]
fn backends_qnn_is_offered_to_parakeet_models_only() {
    let order = [Accelerator::Qnn, Accelerator::Vulkan, Accelerator::Cpu];
    let whisper = asr_model("base.en").unwrap();
    let parakeet = asr_model("parakeet-tdt-0.6b-v2").unwrap();
    assert_eq!(
        model_order(&whisper, &order),
        [Accelerator::Vulkan, Accelerator::Cpu]
    );
    assert_eq!(model_order(&parakeet, &order), order);
}

#[test]
fn backends_cuda_is_offered_to_whisper_only_where_whisper_has_it() {
    let order = [Accelerator::Cuda, Accelerator::Vulkan, Accelerator::Cpu];
    let whisper = asr_model("base.en").unwrap();
    let parakeet = asr_model("parakeet-tdt-0.6b-v2").unwrap();
    assert_eq!(model_order_with(&whisper, &order, true), order);
    assert_eq!(
        model_order_with(&whisper, &order, false),
        [Accelerator::Vulkan, Accelerator::Cpu]
    );
    assert_eq!(model_order_with(&parakeet, &order, false), order);
    assert_eq!(model_order_with(&parakeet, &order, true), order);
}

#[test]
#[cfg_attr(feature = "qnn", ignore = "checks a build without the `qnn` feature")]
fn backends_qnn_request_without_the_feature_is_an_error_not_a_cpu_fallback() {
    let models = models();
    for result in [
        load_silero(&models, Accelerator::Qnn).map(|_| ()),
        init_onnx(&models, Accelerator::Qnn).map(|_| ()),
        onnx_session_config(Accelerator::Qnn, 1).map(|_| ()),
    ] {
        assert!(
            matches!(result, Err(RuntimeError::Unavailable(_))),
            "{:?}",
            result.err()
        );
    }
}

#[test]
fn backends_qnn_files_name_their_fetch_set() {
    let empty = ModelDir::new(std::env::temp_dir().join("earshot-runtime-no-qnn"));
    let files = qnn_files();
    let text = empty.require(&files[0]).unwrap_err().to_string();
    assert!(
        text.contains(&format!(
            "fetch-models --set {}",
            earshot_onnx::qnn::QNN_HOST_SET
        )),
        "{text}"
    );
    assert!(
        files
            .iter()
            .any(|f| f.ends_with(earshot_onnx::qnn::QNN_PROVIDER_LIB))
    );
    assert!(
        files
            .iter()
            .any(|f| f.ends_with(earshot_onnx::qnn::QNN_HTP_LIB))
    );
    let ort = empty
        .require(earshot_onnx::ORT_CPU_DYLIB_ARM64)
        .unwrap_err()
        .to_string();
    assert!(ort.contains("--set arm64"), "{ort}");
}

#[test]
fn backends_qnn_files_cover_every_pinned_library_of_the_package() {
    let package = earshot_onnx::qnn::QNN_HOST;
    let files = qnn_files();
    assert_eq!(files.len(), 2 + package.libraries.len());
    for (name, _) in package.libraries {
        let rel = format!("{}/{name}", package.dir);
        assert!(files.contains(&rel), "{rel}");
        // A missing companion names its fetch set like the plugin does.
        assert_eq!(fetch_set(&rel), Some(earshot_onnx::qnn::QNN_HOST_SET));
    }
}

#[test]
#[cfg_attr(
    feature = "qnn-installed",
    ignore = "checks a build that may take the plugin from the models folder"
)]
fn backends_qnn_package_is_the_models_folder_unless_the_exe_folder_holds_the_plugin() {
    let models = ModelDir::new(std::env::temp_dir().join("earshot-runtime-qnn-dir"));
    // A test binary's folder holds no plugin, as in development and the CLI.
    let exe = earshot_onnx::qnn::exe_dir().unwrap();
    assert!(!exe.join(earshot_onnx::qnn::QNN_PROVIDER_LIB).exists());
    assert_eq!(
        qnn_package_dir(&models),
        Some(models.root().join(earshot_onnx::qnn::QNN_HOST.dir))
    );
}

#[test]
fn backends_qnn_install_only_takes_the_plugin_from_beside_the_exe_alone() {
    let root = std::env::temp_dir().join(format!("earshot-qnn-install-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let models = ModelDir::new(root.join("models"));
    let install = root.join("install");
    let empty = root.join("empty");
    std::fs::create_dir_all(&install).unwrap();
    std::fs::create_dir_all(&empty).unwrap();
    std::fs::write(install.join(earshot_onnx::qnn::QNN_PROVIDER_LIB), b"plugin").unwrap();
    // The models folder's copy exists too: a shipped build never reads it.
    let in_models = models.root().join(earshot_onnx::qnn::QNN_HOST.dir);
    std::fs::create_dir_all(&in_models).unwrap();
    std::fs::write(
        in_models.join(earshot_onnx::qnn::QNN_PROVIDER_LIB),
        b"plugin",
    )
    .unwrap();
    // A shipped app: the install's folder, else nothing.
    assert_eq!(
        qnn_package_dir_in(&models, Some(&install), true),
        Some(install.clone())
    );
    assert_eq!(qnn_package_dir_in(&models, Some(&empty), true), None);
    assert_eq!(qnn_package_dir_in(&models, None, true), None);
    // Development and the CLI: the install's folder when it holds the plugin, else the models
    // folder.
    assert_eq!(
        qnn_package_dir_in(&models, Some(&install), false),
        Some(install.clone())
    );
    assert_eq!(
        qnn_package_dir_in(&models, Some(&empty), false),
        Some(in_models.clone())
    );
    assert_eq!(qnn_package_dir_in(&models, None, false), Some(in_models));
    // Only the desktop's `qnn` feature turns it on.
    assert_eq!(QNN_INSTALL_ONLY, cfg!(feature = "qnn-installed"));
    std::fs::remove_dir_all(&root).unwrap();
}

/// Writes every file of `model`'s NPU conversion (empty: only their presence is read here).
fn put_npu_conversion(models: &ModelDir, model: &str) {
    let Ok(AsrModel::Parakeet(p)) = asr_model(model) else {
        panic!("{model} is no Parakeet model");
    };
    for rel in p.qnn_files() {
        let f = models.root().join(rel);
        std::fs::create_dir_all(f.parent().unwrap()).unwrap();
        std::fs::write(f, b"").unwrap();
    }
}

#[test]
fn backends_npu_is_left_out_for_a_model_without_its_npu_conversion() {
    let root = std::env::temp_dir().join(format!("earshot-npu-order-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let models = ModelDir::new(root.clone());
    let surface = vec![Accelerator::Qnn, Accelerator::Cpu];
    // No conversion: the processor, and the user is told.
    let (order, note) = npu_order(&models, "parakeet-tdt-0.6b-v2", surface.clone());
    assert_eq!(order, [Accelerator::Cpu]);
    let note = note.unwrap();
    assert_eq!(
        note,
        format!(
            "{} has no NPU version installed, so it runs on the processor.",
            crate::catalogue::display_name("parakeet-tdt-0.6b-v2")
        )
    );
    assert_eq!(crate::catalogue::manifest_id_in(&note), None, "{note}");
    // One file short is still missing.
    put_npu_conversion(&models, "parakeet-tdt-0.6b-v2");
    let Ok(AsrModel::Parakeet(p)) = asr_model("parakeet-tdt-0.6b-v2") else {
        unreachable!()
    };
    let last = models.root().join(p.qnn_files().last().unwrap());
    std::fs::remove_file(&last).unwrap();
    assert_eq!(
        npu_order(&models, "parakeet-tdt-0.6b-v2", surface.clone()).0,
        [Accelerator::Cpu]
    );
    // With every file the NPU stays first (its files are checked when the model loads).
    std::fs::write(&last, b"").unwrap();
    assert_eq!(
        npu_order(&models, "parakeet-tdt-0.6b-v2", surface.clone()),
        (surface.clone(), None)
    );
    // A whisper model is never offered the NPU, so nothing is said; an order without the NPU is
    // left alone.
    assert_eq!(
        npu_order(&models, "base.en", surface.clone()),
        (surface.clone(), None)
    );
    assert_eq!(
        npu_order(&models, "parakeet-tdt-110m", vec![Accelerator::Cpu]),
        (vec![Accelerator::Cpu], None)
    );
    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn backends_transcription_setup_without_the_npu_conversion_runs_on_the_processor_and_says_so() {
    use crate::model_settings::{ModelPick, ModelSettings, Part, set_model_settings};
    let root = std::env::temp_dir().join(format!("earshot-npu-setup-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    let mut db = Store::open(&root.join("earshot.db")).unwrap();
    let models = ModelDir::new(root.join("models"));
    let surface = [Accelerator::Qnn, Accelerator::Cpu];
    let hw = earshot_hardware::Hardware::default();
    for part in [Part::Automatic, Part::Npu] {
        let s = ModelSettings {
            transcription: ModelPick::Named("parakeet-tdt-0.6b-v2".into()),
            transcription_part: part,
            ..ModelSettings::default()
        };
        set_model_settings(&mut db, &s).unwrap();
        let t = crate::host::transcription_setup(&db, &models, &surface, &hw).unwrap();
        assert_eq!(t.order, [Accelerator::Cpu], "{part:?}");
        assert_eq!(
            t.notes,
            [format!(
                "{} has no NPU version installed, so it runs on the processor.",
                crate::catalogue::display_name("parakeet-tdt-0.6b-v2")
            )],
            "{part:?}"
        );
    }
    // Once the conversion is there the NPU is tried first, and nothing is said.
    put_npu_conversion(&models, "parakeet-tdt-0.6b-v2");
    let t = crate::host::transcription_setup(&db, &models, &surface, &hw).unwrap();
    assert_eq!(t.order, surface);
    assert!(t.notes.is_empty(), "{:?}", t.notes);
    drop(db);
    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn backends_whisper_is_never_offered_the_npu() {
    // whisper.cpp has no QNN backend: QNN is skipped without a load and without a refusal
    // (so an ARM64 qnn build shows no fallback warning for whisper).
    let model = Path::new("whisper/ggml-base.en.bin");
    let loader = FakeLoader::failing(&[]);
    let choice = select_whisper(&loader, model, &[Accelerator::Qnn, Accelerator::Cpu]).unwrap();
    assert_eq!(choice.backend, BackendKind::Cpu);
    assert!(choice.rejected.is_empty(), "{:?}", choice.rejected);
    assert_eq!(
        *loader.asked.borrow(),
        [(model.to_path_buf(), BackendKind::Cpu)]
    );
}

#[test]
fn backends_order_is_cuda_then_vulkan_then_cpu() {
    // PLAN.md 3.4 and D20: CUDA > Vulkan > CPU on x86-64; Vulkan only with a Vulkan device.
    use Accelerator::{Cpu, Cuda, Vulkan};
    assert_eq!(order_for(true, true), [Cuda, Vulkan, Cpu]);
    assert_eq!(order_for(true, false), [Cuda, Cpu]);
    assert_eq!(order_for(false, true), [Vulkan, Cpu]);
    assert_eq!(order_for(false, false), [Cpu]);
    for a in [Cpu, Cuda, Vulkan] {
        assert_eq!(Accelerator::of(a.backend()), Some(a));
    }
    // T8.3: QNN is an accelerator (ARM64 `qnn` builds), placed by `with_npu`.
    assert_eq!(Accelerator::of(BackendKind::Qnn), Some(Accelerator::Qnn));
    assert_eq!(Accelerator::of(BackendKind::Fake), None);
}

#[test]
fn backends_order_from_a_recommended_backend_never_climbs_back_up() {
    use Accelerator::{Cpu, Cuda, Vulkan};
    let full = [Cuda, Vulkan, Cpu];
    // The model check kept the CPU over Vulkan: Vulkan is not tried.
    assert_eq!(order_from(&full, BackendKind::Cpu), [Cpu]);
    assert_eq!(order_from(&full, BackendKind::Vulkan), [Vulkan, Cpu]);
    assert_eq!(order_from(&full, BackendKind::Cuda), full);
    // A backend this build does not have: the build's whole order.
    assert_eq!(order_from(&[Cpu], BackendKind::Vulkan), [Cpu]);
    assert_eq!(order_from(&[Cuda, Cpu], BackendKind::Vulkan), [Cuda, Cpu]);
}

#[test]
fn backends_vulkan_is_whisper_only_and_never_a_silent_cpu_onnx_session() {
    // ONNX Runtime has no Vulkan provider here: Parakeet, Silero and the speaker models
    // refuse it.
    let models = models();
    for result in [
        load_silero(&models, Accelerator::Vulkan).map(|_| ()),
        load_diarisation(&models, Accelerator::Vulkan).map(|_| ()),
        init_onnx(&models, Accelerator::Vulkan).map(|_| ()),
        onnx_session_config(Accelerator::Vulkan, 1).map(|_| ()),
    ] {
        assert!(
            matches!(result, Err(RuntimeError::Unavailable(_))),
            "{:?}",
            result.err()
        );
    }
    let p = PARAKEET_MODELS[0];
    let e = load_parakeet(&models, &p, Accelerator::Vulkan, None).unwrap_err();
    assert!(e.to_string().contains("Vulkan"), "{e}");
    assert_eq!(
        onnx_library(
            "parakeet-tdt-0.6b-v2",
            &[Accelerator::Vulkan, Accelerator::Cpu]
        ),
        Accelerator::Cpu
    );
}

#[test]
#[cfg_attr(
    feature = "vulkan",
    ignore = "checks a build without the `vulkan` feature"
)]
fn backends_vulkan_request_without_the_feature_is_refused_not_run_on_the_cpu() {
    let _whisper = whisper_loads();
    let models = models();
    let path = ok(models.require(&ok(whisper_model("base.en"))));
    let e = EvidenceLoader::default()
        .load_verified(&path, BackendKind::Vulkan)
        .unwrap_err();
    assert!(e.to_string().contains("no Vulkan device"), "{e}");
    let choice = ok(select_whisper(
        &EvidenceLoader::default(),
        &path,
        &[Accelerator::Vulkan, Accelerator::Cpu],
    ));
    assert_eq!(choice.backend, BackendKind::Cpu);
    assert_eq!(choice.rejected.len(), 1);
    assert_eq!(choice.rejected[0].0, BackendKind::Vulkan);
}

/// Needs a `vulkan` build (short target dir) and a Vulkan device:
/// `cargo test --release -p earshot-runtime --features vulkan --target-dir C:\eb\vk --lib
/// backends_base_en_loads_verified_on_vulkan` (base.en: the Vulkan ladder's last rung).
#[test]
#[cfg_attr(
    not(feature = "vulkan"),
    ignore = "needs a `vulkan` build and a Vulkan device"
)]
fn backends_base_en_loads_verified_on_vulkan() {
    let _whisper = whisper_loads();
    let models = models();
    let path = ok(models.require(&ok(whisper_model("base.en"))));
    let choice = ok(select_whisper(
        &EvidenceLoader::for_live(2),
        &path,
        &[Accelerator::Vulkan, Accelerator::Cpu],
    ));
    assert_eq!(choice.backend, BackendKind::Vulkan, "{:?}", choice.rejected);
    let mut whisper = choice.transcriber;
    let ev = whisper.evidence().clone();
    assert_eq!(ev.log, earshot_asr_whisper::evidence::LogBackend::Vulkan);
    assert!(
        ev.lines.iter().any(|l| l.contains("using Vulkan")),
        "{:?}",
        ev.lines
    );
    let t = whisper
        .transcribe(
            &MonoBuffer::new(PIPELINE_RATE, vec![0.0; 16_000]),
            &earshot_core::asr::TranscribeOptions::live(),
            &CancelToken::new(),
        )
        .unwrap();
    assert_eq!(t.backend.kind, BackendKind::Vulkan);
    assert_eq!(whisper.backend_info().kind, BackendKind::Vulkan);
}

#[test]
#[cfg_attr(feature = "cuda", ignore = "checks a build without the `cuda` feature")]
fn backends_cuda_request_without_the_feature_is_an_error_not_a_cpu_fallback() {
    let models = models();
    for result in [
        load_silero(&models, Accelerator::Cuda).map(|_| ()),
        load_diarisation(&models, Accelerator::Cuda).map(|_| ()),
        init_onnx(&models, Accelerator::Cuda).map(|_| ()),
    ] {
        assert!(
            matches!(result, Err(RuntimeError::Unavailable(_))),
            "{:?}",
            result.err()
        );
    }
}

#[test]
fn backends_whisper_selection_tries_in_order_and_keeps_the_reasons() {
    let model = Path::new("whisper/ggml-base.en.bin");
    let loader = FakeLoader::failing(&[BackendKind::Cuda]);
    let choice = select_whisper(&loader, model, &[Accelerator::Cuda, Accelerator::Cpu]).unwrap();
    assert_eq!(choice.backend, BackendKind::Cpu);
    assert_eq!(choice.rejected.len(), 1);
    assert_eq!(choice.rejected[0].0, BackendKind::Cuda);
    assert_eq!(
        *loader.asked.borrow(),
        [
            (model.to_path_buf(), BackendKind::Cuda),
            (model.to_path_buf(), BackendKind::Cpu)
        ]
    );
    // Nothing verifies: an error naming every attempt, never an unverified load.
    let loader = FakeLoader::failing(&[BackendKind::Cuda, BackendKind::Cpu]);
    let err = select_whisper(&loader, model, &[Accelerator::Cuda, Accelerator::Cpu]).unwrap_err();
    match err {
        RuntimeError::NoBackend(rejected) => assert_eq!(rejected.len(), 2),
        other => panic!("{other:?}"),
    }
}

#[test]
#[cfg_attr(feature = "cuda", ignore = "a cuda build loads the GPU ONNX Runtime")]
fn backends_onnx_models_load_on_the_cpu_from_the_models_directory() {
    let models = models();
    let accel = Accelerator::Cpu;
    ok(init_onnx(&models, accel));
    let mut vad = ok(load_silero(&models, accel));
    assert_eq!(vad.frame_len(), 512);
    let p = vad.speech_probability(&[0.0; 512]).unwrap();
    assert!((0.0..=1.0).contains(&p));
    let (mut segmenter, mut embedder) = ok(load_diarisation(&models, accel));
    let window = vec![0.01f32; segmenter.info().window];
    assert_eq!(
        segmenter
            .segment(&[&window], &CancelToken::new())
            .unwrap()
            .len(),
        1
    );
    let speech: Vec<f32> = (0..32_000)
        .map(|i| ((i as f32) * 0.05).sin() * 0.1)
        .collect();
    let e = embedder.embed(&[&speech], &CancelToken::new()).unwrap();
    assert_eq!(e.len(), 1);
    assert_eq!(
        embedder.info().model.as_str(),
        "campplus-en-voxceleb@357a834f702b"
    );
    // The models report the backend that actually ran.
    assert_eq!(embedder.backend().kind(), Some(BackendKind::Cpu));
    // The shipped calibrations cover this embedder: a post-processor builds with the
    // default config, and reports how its whisper was placed.
    let setup = ok(post_processor(
        &SendLoader(vec![BackendKind::Cuda]),
        &models,
        "base.en",
        &[Accelerator::Cuda, accel],
        accel,
        PostConfig::default(),
    ));
    assert_eq!(setup.asr_backend, BackendKind::Cpu);
    assert_eq!(setup.fallbacks.len(), 1);
    // The production final pass decodes in Silero speech pieces, never one whole-track
    // call (T6.16).
    assert!(setup.processor.decodes_in_pieces());
}

#[test]
#[cfg_attr(feature = "cuda", ignore = "a cuda build loads the GPU ONNX Runtime")]
fn backends_live_asr_factory_ids_are_real_uuidv7_and_fallbacks_are_kept() {
    let models = models();
    ok(init_onnx(&models, Accelerator::Cpu));
    let clock = std::sync::Arc::new(ManualClock::new(1_792_294_800_000));
    let mut factory = live_asr_factory(
        std::sync::Arc::new(SendLoader(vec![BackendKind::Cuda])),
        models,
        "base.en".into(),
        vec![Accelerator::Cuda, Accelerator::Cpu],
        Accelerator::Cpu,
        clock.clone(),
        Box::new(ChaCha8Rng::seed_from_u64(70)),
    );
    let track = EntityId::from_bytes([4; 16]);
    let mut track_asr = ok(factory(TrackId::Mic, track)).expect("live ASR for the mic");
    assert_eq!(track_asr.fallbacks.len(), 1);
    assert_eq!(track_asr.fallbacks[0].0, BackendKind::Cuda);
    let asr = &mut track_asr.asr;
    assert_eq!(asr.track_entity, track);
    assert_eq!(asr.vad.frame_len(), 512);
    // More than 4096 ids: every one a well-formed UUIDv7 carrying the clock's time.
    let mut seen = std::collections::BTreeSet::new();
    for i in 0..5000u64 {
        clock.advance_ms(i % 2);
        let id = (asr.segment_ids)();
        let u = id.as_uuid();
        assert_eq!(u.get_version_num(), 7, "{u}");
        assert_eq!((u.as_u128() >> 62) & 0b11, 0b10, "RFC 4122 variant: {u}");
        let ms = (u.as_u128() >> 80) as u64;
        assert_eq!(ms, clock.now_unix_ms(), "{u}");
        assert!(seen.insert(id), "duplicate {u}");
    }
    let t = asr.transcriber.transcribe(
        &MonoBuffer::new(PIPELINE_RATE, vec![0.0; 1600]),
        &earshot_core::asr::TranscribeOptions::live(),
        &CancelToken::new(),
    );
    assert!(
        t.is_err(),
        "the scripted fake is empty: proves it is the loader's transcriber"
    );
}

#[test]
fn backends_pass_backend_is_stored_per_pass_and_track() {
    let mut store = Store::open_in_memory().unwrap();
    let meeting = EntityId::from_bytes([1; 16]);
    let info = FakeTranscriber::scripted([]).backend_info();
    assert_eq!(
        pass_backend(&store, meeting, TranscriptPass::Live, Some(TrackId::Mic)).unwrap(),
        None
    );
    record_pass_backend(
        &mut store,
        meeting,
        TranscriptPass::Live,
        Some(TrackId::Mic),
        &info,
    )
    .unwrap();
    assert_eq!(
        pass_backend(&store, meeting, TranscriptPass::Live, Some(TrackId::Mic)).unwrap(),
        Some(info.clone())
    );
    assert_eq!(
        pass_backend(&store, meeting, TranscriptPass::Final, None).unwrap(),
        None
    );
}

#[test]
fn backends_are_send_for_the_runtime_threads() {
    fn send<T: Send>() {}
    send::<earshot_onnx::silero::SileroVad>();
    send::<earshot_onnx::segment::PyannoteSegmenter>();
    send::<earshot_onnx::embed::CamppEmbedder>();
    send::<earshot_asr_whisper::WhisperTranscriber>();
}

#[cfg(windows)]
#[test]
fn backends_live_pipeline_gap_limit_exceeds_the_capture_drop_horizon() {
    // A consumer stall of up to the capture queue cap ends in one DataLost marker and a
    // timestamp gap of about the stall; the pipeline must take that gap as silence, not
    // as a TimestampJump.
    let gap = crate::live::LiveConfig::default().pipeline.max_gap;
    assert!(gap >= 2 * earshot_capture_wasapi::LIVE_QUEUE_CAP, "{gap:?}");
}

#[cfg(windows)]
#[test]
#[ignore = "needs a microphone and a render endpoint (GATE-01)"]
fn backends_wasapi_session_records_from_the_default_devices() {
    use std::time::Duration;

    let dir = std::env::temp_dir().join(format!("earshot-runtime-wasapi-{}", std::process::id()));
    let data = DataDir::new(&dir);
    data.create().unwrap();
    let store = earshot_store::Store::open(&data.db_path()).unwrap();
    let svc = crate::Services::new(store, data, std::sync::Arc::new(crate::SystemClock)).unwrap();
    let session = start_wasapi_session(
        svc.clone(),
        crate::live::LiveConfig::default(),
        earshot_capture_wasapi::MicRole::Console,
        &earshot_capture_wasapi::CaptureMode::Call(
            earshot_capture_wasapi::LoopbackRequest::DefaultDevice,
        ),
        None,
        crate::os_seeded_rng(),
    )
    .unwrap();
    std::thread::sleep(Duration::from_secs(2));
    let outcome = session.stop().unwrap();
    assert_eq!(outcome.audio_files.len(), 2);
    assert!(outcome.end.as_millis() >= 1_500);
    let _ = std::fs::remove_dir_all(dir);
}

#[cfg(windows)]
#[test]
fn backends_a_bluetooth_hands_free_mic_becomes_a_session_warning() {
    use earshot_capture_wasapi::{EndpointProperties, hands_free_warning as classify};

    let headset = EndpointProperties {
        name: "Headset (Hands-Free)".into(),
        enumerator: Some("BTHHFENUM".into()),
        form_factor: Some(earshot_capture_wasapi::endpoint::form_factor::HEADSET),
        filter_path: None,
        sample_rate: Some(16_000),
        id: None,
    };
    let found = classify(&headset).expect("a hands-free mic");
    assert_eq!(
        hands_free_warning(found),
        crate::live::RuntimeWarning::BluetoothHandsFreeMic {
            device_name: "Headset (Hands-Free)".into(),
            sample_rate: Some(16_000),
        }
    );
    let usb = EndpointProperties {
        enumerator: Some("USB".into()),
        ..headset
    };
    assert_eq!(classify(&usb), None, "no warning for a USB mic");
}

#[cfg(windows)]
#[test]
#[ignore = "needs a microphone"]
fn backends_wasapi_in_room_session_records_the_mic_only() {
    use std::time::Duration;

    use earshot_core::capture::{EndReason, TrackId};
    use earshot_core::model::MeetingRepo;

    let dir = std::env::temp_dir().join(format!(
        "earshot-runtime-wasapi-room-{}",
        std::process::id()
    ));
    let data = DataDir::new(&dir);
    data.create().unwrap();
    let store = earshot_store::Store::open(&data.db_path()).unwrap();
    let svc = crate::Services::new(store, data, std::sync::Arc::new(crate::SystemClock)).unwrap();
    let session = start_wasapi_session(
        svc.clone(),
        crate::live::LiveConfig::default(),
        earshot_capture_wasapi::MicRole::Console,
        &earshot_capture_wasapi::CaptureMode::InRoom,
        None,
        crate::os_seeded_rng(),
    )
    .unwrap();
    std::thread::sleep(Duration::from_secs(2));
    let outcome = session.stop().unwrap();
    assert_eq!(outcome.reason, EndReason::Stopped);
    assert_eq!(
        outcome.tracks.iter().map(|(k, _)| *k).collect::<Vec<_>>(),
        [TrackId::Mic]
    );
    assert_eq!(outcome.audio_files.len(), 1);
    assert!(outcome.echo_locks.is_empty(), "no canceller, no lock");
    let rows = svc.store().tracks(outcome.meeting).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].kind, TrackId::Mic);
    assert!(outcome.end.as_millis() >= 1_500);
    drop(svc);
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn backends_a_call_that_fell_back_degrades_the_reported_backend() {
    let cuda = BackendInfo {
        kind: BackendKind::Cuda,
        device: "CUDA0".into(),
        model: "ggml-base.en".into(),
    };
    // A call that re-initialised its backend and failed on the GPU ran on the CPU.
    let lines = vec![
        "whisper_backend_init_gpu: using CUDA0 backend".to_owned(),
        "whisper_backend_init_gpu: failed to initialize CUDA0 backend".to_owned(),
    ];
    let degraded = degraded_backend(&cuda, BackendKind::Cuda, &lines)
        .unwrap()
        .expect("a fallback");
    assert_eq!(degraded.kind, BackendKind::Cpu);
    assert!(degraded.device.contains("CUDA0"), "{}", degraded.device);
    assert_eq!(degraded.model, cuda.model);
    let no_gpu = vec!["whisper_backend_init_gpu: no GPU found".to_owned()];
    assert_eq!(
        degraded_backend(&cuda, BackendKind::Cuda, &no_gpu)
            .unwrap()
            .map(|b| b.kind),
        Some(BackendKind::Cpu)
    );
    // A call that initialised nothing (greedy, same state) is fine.
    assert_eq!(
        degraded_backend(&cuda, BackendKind::Cuda, &[]).unwrap(),
        None
    );
}

#[test]
fn backends_an_unknown_call_log_is_an_error_never_the_cpu() {
    let cuda = BackendInfo {
        kind: BackendKind::Cuda,
        device: "CUDA0".into(),
        model: "ggml-base.en".into(),
    };
    // Re-initialised without naming a backend: unknown.
    let unknown = vec!["whisper_backend_init_gpu: something unexpected".to_owned()];
    assert!(degraded_backend(&cuda, BackendKind::Cuda, &unknown).is_err());
    // A CPU model whose call selected a GPU: contradictory, not a "CPU fallback".
    let cpu = BackendInfo {
        kind: BackendKind::Cpu,
        ..cuda.clone()
    };
    let gpu = vec!["whisper_backend_init_gpu: using CUDA0 backend".to_owned()];
    assert!(degraded_backend(&cpu, BackendKind::Cpu, &gpu).is_err());
}

#[test]
fn backends_live_transcription_must_decode_greedily() {
    let live = earshot_core::asr::ChunkerConfig::default();
    assert!(check_live_options(&live).is_ok());
    let beam = earshot_core::asr::ChunkerConfig {
        options: earshot_core::asr::TranscribeOptions::batch(),
        ..earshot_core::asr::ChunkerConfig::default()
    };
    assert!(matches!(
        check_live_options(&beam),
        Err(RuntimeError::Unavailable(_))
    ));
}

#[test]
fn backends_base_en_loads_verified_on_the_cpu_and_transcribes() {
    let _whisper = whisper_loads();
    let models = models();
    let path = ok(models.require(&ok(whisper_model("base.en"))));
    let choice = ok(select_whisper(
        &EvidenceLoader::default(),
        &path,
        &[Accelerator::Cpu],
    ));
    assert_eq!(choice.backend, BackendKind::Cpu);
    let mut whisper = choice.transcriber;
    assert_eq!(whisper.backend_info().kind, BackendKind::Cpu);
    let t = whisper
        .transcribe(
            &MonoBuffer::new(PIPELINE_RATE, vec![0.0; 16_000]),
            &earshot_core::asr::TranscribeOptions::batch(),
            &CancelToken::new(),
        )
        .unwrap();
    assert_eq!(t.backend.kind, BackendKind::Cpu);
    // The per-call check found nothing: still the CPU, undegraded.
    assert_eq!(whisper.backend_info().kind, BackendKind::Cpu);
    assert!(!whisper.backend_info().device.contains("fell back"));
}

#[test]
#[cfg_attr(feature = "cuda", ignore = "a cuda build loads the GPU ONNX Runtime")]
fn backends_production_loader_feeds_the_live_factory_and_the_post_processor() {
    let _whisper = whisper_loads();
    let models = models();
    ok(init_onnx(&models, Accelerator::Cpu));
    let factory = live_asr_factory(
        std::sync::Arc::new(EvidenceLoader::default()),
        models.clone(),
        "base.en".into(),
        vec![Accelerator::Cpu],
        Accelerator::Cpu,
        std::sync::Arc::new(ManualClock::new(1_792_294_800_000)),
        Box::new(ChaCha8Rng::seed_from_u64(80)),
    );
    // Speaker windows during the meeting (T6.13d2), with the cleanup's window settings.
    let mut factory = with_live_speakers(
        factory,
        models.clone(),
        Accelerator::Cpu,
        PostConfig::default().diarization,
    );
    let live = ok(factory(TrackId::Mic, EntityId::from_bytes([8; 16]))).unwrap();
    assert_eq!(live.asr.transcriber.backend_info().kind, BackendKind::Cpu);
    // The models load only when the session runs speaker work (T6.13d4).
    let mut live = live;
    let speakers = live.speakers.take().expect("live speaker models");
    let diarizer = ok((speakers.models)());
    let live_key = (diarizer.window_params(), speakers.threads);
    assert_eq!(live_key.1, Some(speaker_threads(cores())));
    // Production speaker work runs below normal priority (T6.13d3).
    assert!(speakers.background);
    assert!(
        live_key.0.segmenter.model.ends_with("@220ad67ca923"),
        "{}",
        live_key.0.segmenter.model
    );
    assert_eq!(
        live.asr.config.options.decoding,
        earshot_core::asr::DecodingOptions::LIVE
    );
    assert!(live.fallbacks.is_empty());

    // One batch of the production speaker models through the speaker thread as background
    // work (T6.13d3): the thread and the models' pool threads lowered, the batch finished,
    // no pool thread failed to lower itself.
    let progress = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let thread = crate::live::speaker_thread::SpeakerThread::spawn(vec![
        crate::live::speaker_thread::SpeakerTrack {
            track: TrackId::Mic,
            threads: None,
            diarizer,
            progress: Some(std::sync::Arc::clone(&progress)),
            background: speakers.background,
        },
    ])
    .unwrap();
    // 41 s: one batch of 32 windows. Alternating 1 s noise bursts and silence.
    let mut seed = 0x2545_f491_u32;
    let audio: Vec<f32> = (0..41 * 16_000)
        .map(|i| {
            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            let noise = (f64::from(seed >> 8) / f64::from(1u32 << 24) - 0.5) as f32;
            if (i / 16_000) % 2 == 0 {
                noise * 0.4
            } else {
                0.0
            }
        })
        .collect();
    for p in audio.chunks(160) {
        thread.send(TrackId::Mic, p);
    }
    let start = std::time::Instant::now();
    while progress.load(std::sync::atomic::Ordering::SeqCst) < 32
        && start.elapsed() < std::time::Duration::from_secs(300)
    {
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    let out = thread.finish(std::time::Duration::from_secs(60));
    let (results, stopped) = out[0].1.clone().expect("the windows");
    assert_eq!(stopped, None, "speaker work ran as background work");
    assert_eq!(results.windows(), 32, "one batch finished");
    assert!(!crate::priority::pool_failed());

    // The final pass of a (silent) meeting through the production stack, with the
    // backend recorded.
    let dir = crate::testutil::TempDir::new("backends");
    let (svc, _) = crate::testutil::services(&dir);
    let meeting = crate::testutil::meeting(&svc, "Silence", 81);
    let tracks: Vec<_> = svc.store().tracks(meeting).unwrap();
    let mut setup = ok(post_processor(
        &EvidenceLoader::default(),
        &models,
        "base.en",
        &[Accelerator::Cpu],
        Accelerator::Cpu,
        PostConfig::default(),
    ));
    let input = crate::post::PostInput {
        echo_locks: Vec::new(),
        meeting,
        tracks: tracks
            .iter()
            .map(|t| crate::post::PostTrack {
                track: t.clone(),
                audio: MonoBuffer::new(PIPELINE_RATE, vec![0.0; 32_000]),
                options: earshot_core::asr::TranscribeOptions::batch(),
                stretches: None,
                speakers: None,
            })
            .collect(),
    };
    let outcome = setup
        .processor
        .run(
            &svc,
            input,
            &mut ChaCha8Rng::seed_from_u64(82),
            &CancelToken::new(),
        )
        .unwrap();
    assert_eq!(outcome.backend.kind, BackendKind::Cpu);
    assert!(outcome.warnings.is_empty(), "{:?}", outcome.warnings);
    // The final pass could finish from those live windows: same window settings, models
    // and thread count.
    assert_eq!(setup.processor.speaker_window_key(), live_key);
    let stored = pass_backend(&svc.store(), meeting, TranscriptPass::Final, None).unwrap();
    assert_eq!(stored.map(|b| b.kind), Some(BackendKind::Cpu));
}

#[test]
fn backends_live_whisper_threads_leave_the_machine_room() {
    // T6.10b (the owner): GPU 2; CPU at most half the cores, split per track, at most 4 each.
    let t = |b, cores, tracks| live_whisper_threads(b, cores, tracks).get();
    assert_eq!(t(BackendKind::Cuda, 32, 2), 2);
    assert_eq!(t(BackendKind::Vulkan, 32, 2), 2); // a GPU too (T6.10d)
    assert_eq!(t(BackendKind::Cpu, 32, 2), 4); // desktop CPU: 8 of 32
    assert_eq!(t(BackendKind::Cpu, 12, 2), 3); // Snapdragon X Elite class: 6 of 12
    assert_eq!(t(BackendKind::Cpu, 8, 2), 2); // 10th-gen Intel 4c/8t: 4 of 8
    assert_eq!(t(BackendKind::Cpu, 8, 1), 4); // phone, one track
    assert_eq!(t(BackendKind::Cpu, 2, 2), 1); // never zero
    assert_eq!(t(BackendKind::Cpu, 1, 1), 1);
}

#[test]
fn backends_asr_models_name_whisper_and_parakeet_exactly() {
    let names = asr_model_names();
    for n in [
        "tiny.en",
        "base.en",
        "large-v3-turbo",
        "parakeet-ctc-110m",
        "parakeet-tdt-110m",
        "parakeet-tdt-0.6b-v2",
        "parakeet-tdt-0.6b-v3",
    ] {
        assert!(names.contains(&n), "{n}");
        assert!(asr_model(n).is_ok(), "{n}");
    }
    assert_eq!(names.len(), WHISPER_MODELS.len() + PARAKEET_MODELS.len());
    for bad in [
        "parakeet",
        "Parakeet-tdt-0.6b-v2",
        "parakeet-tdt-0.6b-v2 ",
        "../x",
    ] {
        assert!(
            matches!(asr_model(bad), Err(RuntimeError::UnknownModel(_))),
            "{bad}"
        );
    }
    assert_eq!(
        asr_model("base.en").unwrap(),
        AsrModel::Whisper(whisper_model("base.en").unwrap())
    );
    let v2 = asr_model("parakeet-tdt-0.6b-v2").unwrap().files();
    assert_eq!(
        v2,
        [
            "parakeet/tdt-0.6b-v2/encoder.onnx",
            "parakeet/tdt-0.6b-v2/encoder.weights",
            "parakeet/tdt-0.6b-v2/decoder.onnx",
            "parakeet/tdt-0.6b-v2/joiner.onnx",
            "parakeet/tdt-0.6b-v2/tokens.txt",
        ]
    );
    assert_eq!(
        asr_model("parakeet-ctc-110m").unwrap().files(),
        [
            "parakeet/tdt_ctc-110m/model.onnx",
            "parakeet/tdt_ctc-110m/tokens.txt"
        ]
    );
    // Only v3 detects the language.
    let multilingual: Vec<&str> = PARAKEET_MODELS
        .iter()
        .filter(|p| !p.english_only)
        .map(|p| p.name)
        .collect();
    assert_eq!(multilingual, ["parakeet-tdt-0.6b-v3"]);
}

#[test]
fn backends_parakeet_files_come_from_the_manifest_eval_set() {
    // Every file is a cache path of an `eval` artefact of models/manifest.toml that is also
    // in the model's fetch set (PLAN D22: the defaults in ci and desktop, v3 in choice).
    let manifest: toml::Table = include_str!("../../../models/manifest.toml")
        .parse()
        .unwrap();
    let artefacts = manifest["artefact"].as_array().unwrap();
    for p in PARAKEET_MODELS {
        for f in p.files() {
            let a = artefacts
                .iter()
                .find(|a| a["path"].as_str() == Some(f.as_str()))
                .unwrap_or_else(|| panic!("{f} is not in models/manifest.toml"));
            let sets = a["sets"].as_array().unwrap();
            assert!(sets.contains(&toml::Value::from("eval")), "{f}: {sets:?}");
            assert!(
                sets.contains(&toml::Value::from(p.fetch_set)),
                "{f}: {sets:?} lacks {}",
                p.fetch_set
            );
            let default = p.fetch_set == "ci";
            assert_eq!(
                sets.contains(&toml::Value::from("desktop")),
                default,
                "{f}: {sets:?}"
            );
        }
    }
    let set = |n: &str| model_fetch_set(n).unwrap();
    assert_eq!(set("parakeet-tdt-0.6b-v2"), "ci");
    assert_eq!(set("parakeet-tdt-110m"), "ci");
    assert_eq!(set("parakeet-tdt-0.6b-v3"), "choice");
    assert_eq!(set("parakeet-ctc-110m"), "eval");
    assert_eq!(set("base.en"), "ci");
    assert_eq!(set("small.en"), "desktop");
    assert_eq!(set("large-v3-turbo"), "choice");
    assert_eq!(model_fetch_set("no-such-model"), None);
    // A missing file names the model's fetch set.
    let empty = ModelDir::new(std::env::temp_dir().join("earshot-no-models-here"));
    let e = select_asr(
        &FakeLoader::failing(&[]),
        &empty,
        "parakeet-tdt-110m",
        &[Accelerator::Cpu],
        &|_| None,
    )
    .unwrap_err()
    .to_string();
    assert!(e.contains("fetch-models --set ci"), "{e}");
}

fn placement(lines: &[&str], requested: Provider) -> SessionBackend {
    let mut p = earshot_onnx::Placement::default();
    for l in lines {
        p.observe(l);
    }
    SessionBackend {
        requested,
        placement: p,
    }
}

#[test]
fn backends_parakeet_placement_must_show_the_requested_backend() {
    let cuda = Provider::Cuda { device_id: 0 };
    let encoder = placement(
        &[
            "Node(s) placed on [CUDAExecutionProvider]. Number of nodes: 1515",
            "Node(s) placed on [CPUExecutionProvider]. Number of nodes: 482",
        ],
        cuda,
    );
    let small = placement(
        &["All nodes placed on [CUDAExecutionProvider]. Number of nodes: 13"],
        cuda,
    );
    let on_cpu = placement(
        &["All nodes placed on [CPUExecutionProvider]. Number of nodes: 13"],
        cuda,
    );
    let silent = placement(&[], cuda);
    assert!(check_placement(&[&encoder, &small, &small], BackendKind::Cuda).is_ok());
    // One session on the CPU when CUDA was asked for: refused, never a CPU session.
    assert!(check_placement(&[&encoder, &on_cpu, &small], BackendKind::Cuda).is_err());
    // No placement logged: unknown, refused.
    assert!(check_placement(&[&silent], BackendKind::Cuda).is_err());
    assert!(check_placement(&[], BackendKind::Cpu).is_err());
    let cpu = placement(
        &["All nodes placed on [CPUExecutionProvider]. Number of nodes: 13"],
        Provider::Cpu,
    );
    assert!(check_placement(&[&cpu], BackendKind::Cpu).is_ok());
    assert!(check_placement(&[&encoder], BackendKind::Cpu).is_err());
}

#[test]
fn backends_parakeet_npu_conversions_are_local_builds_of_the_tdt_models() {
    for p in PARAKEET_MODELS {
        match p.decoder {
            ParakeetDecoder::Ctc => assert_eq!(p.qnn_dir(), None, "{}", p.name),
            ParakeetDecoder::Tdt => {
                let dir = p.qnn_dir().unwrap();
                assert_eq!(dir, format!("parakeet-qnn/{}", p.name));
                let files = p.qnn_files();
                for f in [
                    "encoder.onnx",
                    "encoder.weights",
                    "decoder.onnx",
                    "joiner.onnx",
                    "tokens.txt",
                ] {
                    assert!(files.contains(&format!("{dir}/{f}")), "{f}");
                }
                // The NPU ladder's conversions are published (T8.4g, set qnn-models); any
                // other is a local build only, never a manifest artefact.
                let published = ["parakeet-tdt-0.6b-v2", "parakeet-tdt-110m"].contains(&p.name);
                let want = published.then_some(crate::catalogue::NPU_SET);
                assert!(files.iter().all(|f| fetch_set(f) == want), "{files:?}");
            }
        }
    }
    let empty = ModelDir::new(std::env::temp_dir().join("earshot-runtime-no-npu-models"));
    let p = PARAKEET_MODELS
        .iter()
        .find(|p| p.name == "parakeet-tdt-110m")
        .unwrap();
    let e = require_qnn_model(&empty, p).unwrap_err().to_string();
    assert!(
        e.contains("cargo xtask qnn-convert --model parakeet-tdt-110m"),
        "{e}"
    );
    let ctc = PARAKEET_MODELS
        .iter()
        .find(|p| p.decoder == ParakeetDecoder::Ctc)
        .unwrap();
    let e = require_qnn_model(&empty, ctc).unwrap_err().to_string();
    assert!(e.contains("no NPU conversion"), "{e}");
}

#[test]
fn backends_parakeet_npu_partition_is_the_encoder_on_qnn_and_the_steps_on_the_cpu() {
    // Recorded on the desktop's HTP simulator (2026-10-24): the QDQ 110M encoder converted by
    // `cargo xtask qnn-convert` is one fused QNN node plus its float boundary on the CPU.
    let encoder_lines = [
        " Node(s) placed on [CPUExecutionProvider]. Number of nodes: 2",
        "  QuantizeLinear (audio_signal_QuantizeLinear)",
        "  DequantizeLinear (outputs_DequantizeLinear)",
        " Node(s) placed on [QNNExecutionProvider]. Number of nodes: 1",
        "  QNNExecutionProvider_9_1 (QNNExecutionProvider_QNNExecutionProvider_9_1_0)",
    ];
    let encoder = placement(&encoder_lines, Provider::Qnn);
    let decoder = placement(
        &["All nodes placed on [CPUExecutionProvider]. Number of nodes: 7"],
        Provider::Cpu,
    );
    let joiner = placement(
        &["All nodes placed on [CPUExecutionProvider]. Number of nodes: 10"],
        Provider::Cpu,
    );
    assert!(check_qnn_parakeet(&[&encoder, &decoder, &joiner]).is_ok());
    // `check_placement` for QNN is the same rule: the encoder alone is not a Parakeet model.
    assert!(check_placement(&[&encoder], BackendKind::Qnn).is_err());
    assert!(check_placement(&[&encoder, &decoder, &joiner], BackendKind::Qnn).is_ok());
    // Anything else left on the CPU is refused.
    let mut lines = encoder_lines.to_vec();
    lines[0] = " Node(s) placed on [CPUExecutionProvider]. Number of nodes: 3";
    lines.insert(3, "  LayerNormalization (/layers.0/norm)");
    let leaky = placement(&lines, Provider::Qnn);
    let e = check_qnn_parakeet(&[&leaky, &decoder, &joiner]).unwrap_err();
    assert!(e.to_string().contains("LayerNormalization"), "{e}");
    // A float (dynamic) encoder that QNN barely touches is refused.
    let float = placement(
        &[
            " Node(s) placed on [CPUExecutionProvider]. Number of nodes: 3800",
            " Node(s) placed on [QNNExecutionProvider]. Number of nodes: 4",
        ],
        Provider::Qnn,
    );
    assert!(check_qnn_parakeet(&[&float, &decoder, &joiner]).is_err());
    // The steps must stay on the CPU, and all three sessions are needed.
    assert!(check_qnn_parakeet(&[&encoder, &encoder, &joiner]).is_err());
    assert!(check_qnn_parakeet(&[&encoder]).is_err());
}

#[test]
fn backends_npu_conversion_files_are_checked_against_their_record() {
    let dir = std::env::temp_dir().join(format!("earshot-qnn-record-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let files = [
        "encoder.onnx",
        "encoder.weights",
        "decoder.onnx",
        "joiner.onnx",
        "tokens.txt",
    ];
    let mut outputs = serde_json::Map::new();
    for f in files {
        std::fs::write(dir.join(f), f.as_bytes()).unwrap();
        outputs.insert(
            f.to_owned(),
            serde_json::json!({
                "sha256": file_sha256(&dir.join(f)).unwrap(),
                "size": f.len(),
            }),
        );
    }
    let mut record = serde_json::json!({
        "task": "T8.3c",
        "recipe": "a16w8pc-minmax-j2",
        "outputs": outputs,
    });
    std::fs::write(dir.join("conversion.json"), record.to_string()).unwrap();
    assert!(verify_qnn_conversion(&dir).is_ok());
    // A T8.3 conversion (no recipe, the clipped calibration) is refused with the rebuild
    // command, however intact its files are.
    for (task, recipe) in [
        (Some("T8.3"), None),
        (None, None),
        (Some("T8.3c"), None),
        (Some("T8.3"), Some("a16w8pc-minmax-j2")),
    ] {
        record["task"] = task.map_or(serde_json::Value::Null, Into::into);
        record["recipe"] = recipe.map_or(serde_json::Value::Null, Into::into);
        std::fs::write(dir.join("conversion.json"), record.to_string()).unwrap();
        let e = verify_qnn_conversion(&dir).unwrap_err().to_string();
        assert!(
            e.contains("rebuild it with `cargo xtask qnn-convert --model"),
            "{e}"
        );
    }
    record["task"] = "T8.3c".into();
    record["recipe"] = "a16w8pc-minmax-j2".into();
    std::fs::write(dir.join("conversion.json"), record.to_string()).unwrap();
    assert!(verify_qnn_conversion(&dir).is_ok());
    // A swapped file of the same size is refused by its hash.
    std::fs::write(dir.join("tokens.txt"), b"TOKENS.TXT").unwrap();
    let e = verify_qnn_conversion(&dir).unwrap_err().to_string();
    assert!(e.contains("tokens.txt has SHA-256"), "{e}");
    // A truncated file is refused by its size.
    std::fs::write(dir.join("tokens.txt"), b"x").unwrap();
    let e = verify_qnn_conversion(&dir).unwrap_err().to_string();
    assert!(e.contains("bytes"), "{e}");
    // No record, no load.
    std::fs::remove_file(dir.join("conversion.json")).unwrap();
    assert!(verify_qnn_conversion(&dir).is_err());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
#[cfg_attr(feature = "qnn", ignore = "checks a build without the `qnn` feature")]
fn backends_parakeet_on_qnn_without_the_feature_is_refused() {
    let p = &PARAKEET_MODELS[1];
    let e = load_parakeet(&models(), p, Accelerator::Qnn, Some(1)).unwrap_err();
    assert!(e.to_string().contains("`qnn` feature"), "{e}");
}

#[test]
fn backends_qnn_placement_allows_no_cpu_nodes_outside_the_partition() {
    let qnn = Provider::Qnn;
    let whole = placement(
        &[" Node(s) placed on [QNNExecutionProvider]. Number of nodes: 1"],
        qnn,
    );
    // Recorded on the desktop's HTP simulator (2026-10-24): Silero VAD, float with dynamic
    // shapes, keeps 211 of 212 nodes on the CPU.
    let silero = placement(
        &[
            " Node(s) placed on [CPUExecutionProvider]. Number of nodes: 211",
            " Node(s) placed on [QNNExecutionProvider]. Number of nodes: 1",
        ],
        qnn,
    );
    // A few CPU nodes are tolerated for CUDA (shape nodes) but not for the NPU.
    let mostly = placement(
        &[
            " Node(s) placed on [QNNExecutionProvider]. Number of nodes: 300",
            " Node(s) placed on [CPUExecutionProvider]. Number of nodes: 2",
        ],
        qnn,
    );
    let steps = placement(
        &["All nodes placed on [CPUExecutionProvider]. Number of nodes: 7"],
        Provider::Cpu,
    );
    // `check_placement` for QNN is the documented Parakeet partition: one rule, not two.
    assert!(check_placement(&[&whole, &steps, &steps], BackendKind::Qnn).is_ok());
    assert!(check_qnn_parakeet(&[&whole, &steps, &steps]).is_ok());
    let err = check_placement(&[&silero, &steps, &steps], BackendKind::Qnn).unwrap_err();
    assert!(
        err.to_string().contains("CPUExecutionProvider=211"),
        "{err}"
    );
    assert!(check_placement(&[&mostly, &steps, &steps], BackendKind::Qnn).is_err());
    let unlogged = placement(&[], qnn);
    assert!(check_placement(&[&unlogged, &steps, &steps], BackendKind::Qnn).is_err());
    // Not a three-session Parakeet model: refused.
    assert!(check_placement(&[&whole, &whole], BackendKind::Qnn).is_err());
    assert!(check_placement(&[&whole], BackendKind::Cpu).is_err());
}

/// The fixture cache: `EARSHOT_FIXTURES`, else `%LOCALAPPDATA%\earshot-dev\fixtures`.
fn fixtures_dir() -> PathBuf {
    if let Some(d) = std::env::var_os("EARSHOT_FIXTURES").filter(|v| !v.is_empty()) {
        return PathBuf::from(d);
    }
    PathBuf::from(std::env::var_os("LOCALAPPDATA").expect("LOCALAPPDATA or EARSHOT_FIXTURES"))
        .join("earshot-dev")
        .join("fixtures")
}

/// Stream 19 of `asr.stream_determinism` (m2_prod: the pinned subset's test-clean
/// utterances in subset order, grouped into ~60 s streams): its audio, end to end as the
/// metric concatenates it, and its reference.
fn librispeech_stream_19() -> (Vec<f32>, String) {
    const IDS: [&str; 9] = [
        "5683-32865-0006",
        "6829-68769-0000",
        "6829-68769-0001",
        "6829-68769-0002",
        "6829-68769-0003",
        "6930-75918-0000",
        "6930-75918-0001",
        "6930-75918-0002",
        "6930-75918-0003",
    ];
    let root = fixtures_dir().join("librispeech/test-clean/LibriSpeech/test-clean");
    let (mut audio, mut reference) = (Vec::new(), Vec::new());
    for id in IDS {
        let parts: Vec<&str> = id.split('-').collect();
        let dir = root.join(parts[0]).join(parts[1]);
        let flac = dir.join(format!("{id}.flac"));
        assert!(
            flac.is_file(),
            "{} is missing; run `cargo xtask fetch-fixtures --set librispeech`",
            flac.display()
        );
        audio.extend(earshot_store::read_flac(&flac).unwrap().1);
        let trans =
            std::fs::read_to_string(dir.join(format!("{}-{}.trans.txt", parts[0], parts[1])))
                .unwrap();
        let line = trans
            .lines()
            .find_map(|l| l.strip_prefix(id).map(|r| r.trim().to_owned()))
            .unwrap_or_else(|| panic!("no transcript for {id}"));
        reference.push(line);
    }
    (audio, reference.join(" "))
}

#[test]
#[ignore = "needs the ci model set and the LibriSpeech fixtures (cargo xtask fetch-models --set ci; cargo xtask fetch-fixtures --set librispeech) and a build without `cuda`: cargo test --release -p earshot-runtime backends_parakeet_v2_decodes_the_stream_it_skipped -- --include-ignored"]
fn backends_parakeet_v2_decodes_the_stream_it_skipped() {
    // T2.5d: on this stream the production default parakeet-tdt-0.6b-v2 returned no words
    // for a whole 25 s piece (24.4-49.4 s): 78 errors of 227 words, live and final alike.
    // Bounds fixed before measuring the fix: at most 11 errors (5%; the model's corpus WER
    // over the subset's single utterances is 1.75%) with uncovered speech decoded again,
    // and the failure still reproduced without it (at least 50 errors), so the test keeps
    // exercising the case it was written for.
    let _whisper = whisper_loads();
    let models = models();
    ok(init_onnx(&models, Accelerator::Cpu));
    let choice = ok(select_asr(
        &EvidenceLoader::default(),
        &models,
        "parakeet-tdt-0.6b-v2",
        &[Accelerator::Cpu],
        &|_| Some(4),
    ));
    let mut t = choice.transcriber;
    let mut vad = ok(load_silero(&models, Accelerator::Cpu));
    let (audio, reference) = librispeech_stream_19();
    let buffer = MonoBuffer::new(PIPELINE_RATE, audio);
    let mut run = |config: &earshot_core::asr::PieceConfig| {
        let (tr, report) = earshot_core::asr::transcribe_in_pieces(
            &mut t,
            &mut vad,
            &buffer,
            &earshot_core::asr::TranscribeOptions::live(),
            config,
            &CancelToken::new(),
        )
        .unwrap();
        let text = earshot_core::asr::join_words(&tr.words());
        let errors = earshot_core::metrics::wer::word_errors_normalized(&reference, &text);
        (errors, report, text)
    };
    let (errors, report, text) = run(&earshot_core::asr::PieceConfig::default());
    assert_eq!(errors.ref_words, 227);
    assert!(report.uncovered >= 1, "{report:?}");
    assert!(errors.errors() <= 11, "{} errors: {text}", errors.errors());
    let off = earshot_core::asr::PieceConfig {
        recover_speech: None,
        ..earshot_core::asr::PieceConfig::default()
    };
    let (errors, report, _) = run(&off);
    assert_eq!(report.uncovered, 0);
    assert!(
        errors.errors() >= 50,
        "the skipped piece no longer reproduces ({} errors): this test no longer covers it",
        errors.errors()
    );
}

#[test]
fn backends_any_transcriber_forwards_may_skip_speech() {
    // T2.5d: the piece decoders read it through the wrappers (the Parakeet arm is checked
    // on the real model in `backends_parakeet_loads_verified_on_the_cpu_through_the_factories`).
    let plain: AnyTranscriber<FakeTranscriber> =
        AnyTranscriber::Whisper(FakeTranscriber::scripted([]));
    assert!(!plain.may_skip_speech());
    let skipping: AnyTranscriber<FakeTranscriber> =
        AnyTranscriber::Whisper(FakeTranscriber::scripted([]).skipping_speech());
    assert!(skipping.may_skip_speech());
    let boxed: Box<dyn Transcriber> = Box::new(skipping);
    assert!(boxed.may_skip_speech());
}

#[test]
#[ignore = "needs the ci model set (cargo xtask fetch-models --set ci) and a build without `cuda`: cargo test --release -p earshot-runtime backends_parakeet_loads -- --include-ignored"]
fn backends_parakeet_loads_verified_on_the_cpu_through_the_factories() {
    let _whisper = whisper_loads();
    let models = models();
    ok(init_onnx(&models, Accelerator::Cpu));
    let choice = ok(select_asr(
        &EvidenceLoader::default(),
        &models,
        "parakeet-tdt-110m",
        &[Accelerator::Cpu],
        &|_| Some(2),
    ));
    assert_eq!(choice.backend, BackendKind::Cpu);
    assert!(choice.rejected.is_empty());
    let mut t = choice.transcriber;
    assert!(matches!(t, AnyTranscriber::Parakeet(_)));
    // A TDT model reports that it may skip speech (T2.5d), through every wrapper.
    assert!(t.may_skip_speech());
    let clip = earshot_store::read_flac(
        &Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/smoke/121-121726-0002.flac"),
    )
    .unwrap()
    .1;
    let out = t
        .transcribe(
            &MonoBuffer::new(PIPELINE_RATE, clip),
            &earshot_core::asr::TranscribeOptions::live(),
            &CancelToken::new(),
        )
        .unwrap();
    assert_eq!(out.backend.kind, BackendKind::Cpu);
    // Reference "ANGOR PAIN PAINFUL TO HEAR" (fixtures/smoke/transcripts.txt); "angor" is a
    // rare word, so one substitution is allowed.
    let text: Vec<&str> = out.segments.iter().map(|s| s.text.as_str()).collect();
    let errors = earshot_core::metrics::wer::word_errors_normalized(
        "ANGOR PAIN PAINFUL TO HEAR",
        &text.join(" "),
    );
    assert!(errors.errors() <= 1, "{out:?}");

    // The live factory and the post processor take the Parakeet name too.
    let clock: SharedClock = std::sync::Arc::new(ManualClock::new(1_700_000_000_000));
    let mut factory = live_asr_factory(
        std::sync::Arc::new(EvidenceLoader::default()),
        models.clone(),
        "parakeet-tdt-110m".into(),
        vec![Accelerator::Cpu],
        Accelerator::Cpu,
        clock,
        Box::new(ChaCha8Rng::seed_from_u64(7)),
    );
    let track = ok(factory(TrackId::Mic, EntityId::from_bytes([1; 16]))).expect("live asr");
    assert_eq!(track.asr.transcriber.backend_info().kind, BackendKind::Cpu);
    assert!(track.asr.transcriber.may_skip_speech());
    let setup = ok(post_processor(
        &EvidenceLoader::default(),
        &models,
        "parakeet-tdt-110m",
        &[Accelerator::Cpu],
        Accelerator::Cpu,
        PostConfig::default(),
    ));
    assert_eq!(setup.asr_backend, BackendKind::Cpu);
}

#[test]
fn backends_onnx_sessions_get_the_capped_thread_count() {
    // The T6.10b rule: at most half the cores in total, at most 4 per job, at least 1.
    assert_eq!(onnx_cpu_threads(32, 1), 4);
    assert_eq!(onnx_cpu_threads(32, 2), 4);
    assert_eq!(onnx_cpu_threads(8, 1), 4);
    assert_eq!(onnx_cpu_threads(8, 2), 2);
    assert_eq!(onnx_cpu_threads(4, 1), 2);
    assert_eq!(onnx_cpu_threads(2, 2), 1);
    assert_eq!(onnx_cpu_threads(1, 1), 1);
    let cfg = ok(onnx_session_config(Accelerator::Cpu, 3));
    assert_eq!(cfg.intra_threads, Some(3));
    assert_eq!(cfg.provider, Provider::Cpu);
    assert_eq!(
        ok(onnx_session_config(Accelerator::Cpu, 0)).intra_threads,
        Some(1)
    );
    assert_eq!(VAD_THREADS, 1);
}

#[test]
fn backends_onnx_library_is_cuda_whenever_cuda_is_in_the_order() {
    let gpu = [Accelerator::Cuda, Accelerator::Cpu];
    let cpu = [Accelerator::Cpu];
    assert_eq!(
        onnx_library("parakeet-tdt-0.6b-v2", &gpu),
        Accelerator::Cuda
    );
    assert_eq!(onnx_library("parakeet-tdt-0.6b-v2", &cpu), Accelerator::Cpu);
    // One library for the whole process: a Whisper model on a CUDA build
    // loads the CUDA library too, so a later Parakeet meeting in the same app session finds
    // the library it needs.
    assert_eq!(onnx_library("large-v3-turbo", &gpu), Accelerator::Cuda);
    assert_eq!(onnx_library("base.en", &gpu), Accelerator::Cuda);
    assert_eq!(onnx_library("no-such-model", &gpu), Accelerator::Cuda);
    assert_eq!(onnx_library("base.en", &cpu), Accelerator::Cpu);
    // Every model on one CUDA build agrees, so the library never changes within a process.
    for m in [
        "parakeet-tdt-0.6b-v2",
        "parakeet-tdt-110m",
        "large-v3-turbo",
        "base.en",
    ] {
        assert_eq!(onnx_library(m, &gpu), onnx_library_for_all(&gpu), "{m}");
    }
    // On ARM64 the QNN plugin is registered only for a Parakeet model that may use it.
    let npu = [Accelerator::Qnn, Accelerator::Cpu];
    assert_eq!(onnx_library("parakeet-tdt-0.6b-v2", &npu), Accelerator::Qnn);
    assert_eq!(onnx_library("base.en", &npu), Accelerator::Cpu);
}

#[test]
fn backends_speaker_models_get_one_thread_count_live_and_after() {
    // T6.13d2: CAM++ output changes with the thread count, so live and final-pass speaker
    // sessions share one: the two-copy budget.
    assert_eq!(speaker_threads(32), 4);
    assert_eq!(speaker_threads(12), 3);
    assert_eq!(speaker_threads(8), 2);
    assert_eq!(speaker_threads(2), 1);
    for cores in [1, 2, 4, 8, 12, 16, 32, 64] {
        assert_eq!(speaker_threads(cores), onnx_cpu_threads(cores, 2));
    }
}

#[test]
fn backends_parallel_diarisation_needs_two_threads_per_copy() {
    assert!(parallel_diarisation(32));
    assert!(parallel_diarisation(8));
    assert!(!parallel_diarisation(6));
    assert!(!parallel_diarisation(4));
    assert!(!parallel_diarisation(1));
}

/// T9.14: the second preview session (the user's own track in a call) runs only when chosen, only
/// where the first one runs, only on the CPU, and only with the memory for both.
#[test]
fn backends_previews_second_session_only_for_everyone_on_a_big_cpu_machine() {
    use crate::preview_settings::PreviewChoice;
    use earshot_hardware::{Gpu, Hardware};
    let machine = |memory: Option<u64>, card: Option<u64>| Hardware {
        total_memory: memory,
        gpus: vec![Gpu {
            name: "A card".to_owned(),
            vendor_id: 0x10de,
            dedicated_memory: card,
            driver_version: None,
            integrated: Some(false),
            compute_capability: None,
        }],
        ..Hardware::default()
    };
    let big = machine(Some(64 * 1024 * 1024 * 1024), Some(PREVIEW_MIN_CARD_MEMORY));
    assert_eq!(
        previews_refused_second(BackendKind::Cpu, &big, PreviewChoice::Everyone),
        None
    );
    let at_limit = machine(Some(PREVIEW_BOTH_MIN_MEMORY), None);
    assert_eq!(
        previews_refused_second(BackendKind::Cpu, &at_limit, PreviewChoice::Everyone),
        None
    );
    let below = machine(Some(PREVIEW_BOTH_MIN_MEMORY - 1), None);
    assert!(previews_refused_second(BackendKind::Cpu, &below, PreviewChoice::Everyone).is_some());
    let unknown = machine(None, None);
    assert!(previews_refused_second(BackendKind::Cpu, &unknown, PreviewChoice::Everyone).is_some());
    // The first session would run on CUDA here; the second is not measured there.
    assert_eq!(
        previews_refused(BackendKind::Cuda, &big, PreviewChoice::Everyone),
        None
    );
    let cuda = previews_refused_second(BackendKind::Cuda, &big, PreviewChoice::Everyone);
    assert_eq!(cuda.as_deref(), Some(PREVIEW_SECOND_NOT_ON_CARD));
    for kind in [BackendKind::Qnn, BackendKind::Vulkan, BackendKind::Fake] {
        assert!(
            previews_refused_second(kind, &big, PreviewChoice::Everyone).is_some(),
            "{kind:?}"
        );
    }
    for choice in [
        PreviewChoice::Off,
        PreviewChoice::Automatic,
        PreviewChoice::On,
    ] {
        assert!(
            previews_refused_second(BackendKind::Cpu, &big, choice).is_some(),
            "{choice:?}"
        );
    }
}

/// Everyone lifts the memory limit of the first session exactly as "on" does.
#[test]
fn backends_previews_everyone_lifts_the_first_sessions_memory_limit_like_on() {
    use crate::preview_settings::PreviewChoice;
    use earshot_hardware::Hardware;
    for memory in [None, Some(PREVIEW_MIN_MEMORY - 1), Some(PREVIEW_MIN_MEMORY)] {
        let m = Hardware {
            total_memory: memory,
            ..Hardware::default()
        };
        assert_eq!(
            previews_refused(BackendKind::Cpu, &m, PreviewChoice::Everyone),
            previews_refused(BackendKind::Cpu, &m, PreviewChoice::On),
            "{memory:?}"
        );
        assert_eq!(
            previews_refused(BackendKind::Cpu, &m, PreviewChoice::Everyone),
            None
        );
    }
}
