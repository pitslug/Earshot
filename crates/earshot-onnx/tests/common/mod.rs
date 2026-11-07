//! Shared helpers for the earshot-onnx integration tests: model paths (a missing model is a
//! failed precondition, never a skip), the committed smoke speech and the simulator scenarios.

#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use earshot_core::sim::{ScenarioFile, SimOutput, SpeechPool, simulate};
use rand_chacha::ChaCha8Rng;
use rand_core::SeedableRng;

/// Silero VAD (models manifest id `silero-vad`).
pub const SILERO: &str = "silero/silero_vad.onnx";
/// pyannote segmentation-3.0 (manifest id `pyannote-segmentation-3.0`, extracted member).
pub const SEGMENTATION: &str =
    "pyannote-segmentation-3.0/sherpa-onnx-pyannote-segmentation-3-0/model.onnx";
/// CAM++ VoxCeleb (manifest id `campplus-en-voxceleb`).
pub const CAMPPLUS: &str = "embedding/3dspeaker_speech_campplus_sv_en_voxceleb_16k.onnx";

/// Parakeet TDT exports (manifest ids `parakeet-tdt-<tag>-*`): the default English models,
/// set `ci` (PLAN D22: tests load only the default models; the CTC head and the
/// multilingual v3 are never loaded by a test).
pub const PARAKEET_TDT_110M_DIR: &str = "parakeet/tdt-110m";
/// Parakeet TDT 0.6B v2 (English), set `ci`.
pub const PARAKEET_TDT_V2_DIR: &str = "parakeet/tdt-0.6b-v2";
/// onnx-asr's NeMo 128-bin preprocessor (manifest id `onnx-asr-nemo128`, set `eval`; the
/// reference front end, not a transcription model).
pub const NEMO128: &str = "parakeet/onnx-asr/nemo128.onnx";

/// Path of a file or directory of manifest set `set` at `rel`; panics with the fetch
/// instruction when missing.
pub fn fetched(rel: &str, set: &str) -> PathBuf {
    match models_dir().map(|dir| dir.join(rel)) {
        Some(p) if p.exists() => p,
        other => panic!(
            "model not found at {}; run `cargo xtask fetch-models --set {set}`",
            other.map_or_else(
                || "<no EARSHOT_MODELS or LOCALAPPDATA>".to_string(),
                |p| p.display().to_string()
            )
        ),
    }
}

pub fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// Models directory: `EARSHOT_MODELS`, else `%LOCALAPPDATA%\Earshot\models`.
///
/// Mirrors `xtask::models::cache_dir` (xtask is not a dependency of this crate); keep the two
/// rules identical.
pub fn models_dir() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("EARSHOT_MODELS").filter(|v| !v.is_empty()) {
        return Some(PathBuf::from(dir));
    }
    std::env::var_os("LOCALAPPDATA")
        .filter(|v| !v.is_empty())
        .map(|base| PathBuf::from(base).join("Earshot").join("models"))
}

/// Path of the model at `rel` inside the models directory. Panics with the fetch
/// instruction when it is missing.
pub fn model(rel: &str) -> PathBuf {
    match models_dir().map(|dir| dir.join(rel)) {
        Some(p) if p.is_file() => p,
        other => panic!(
            "model not found at {}; run `cargo xtask fetch-models --set ci` \
             (or point EARSHOT_MODELS at a directory that contains {rel})",
            other.map_or_else(
                || "<no EARSHOT_MODELS or LOCALAPPDATA>".to_string(),
                |p| p.display().to_string()
            )
        ),
    }
}

/// Loads the official ONNX Runtime build from the models directory: the GPU (CUDA 13) build
/// with the `cuda` feature, the CPU build otherwise. Panics with the fetch instruction when
/// it is missing.
pub fn init_runtime() {
    let (rel, sha) = if cfg!(feature = "cuda") {
        (
            earshot_onnx::ORT_CUDA_DYLIB,
            earshot_onnx::ORT_CUDA_DYLIB_SHA256,
        )
    } else {
        (
            earshot_onnx::ORT_CPU_DYLIB,
            earshot_onnx::ORT_CPU_DYLIB_SHA256,
        )
    };
    let lib = match models_dir().map(|dir| dir.join(rel)) {
        Some(p) if p.is_file() => p,
        other => panic!(
            "ONNX Runtime not found at {}; run `cargo xtask fetch-models --set {}`",
            other.map_or_else(
                || "<no EARSHOT_MODELS or LOCALAPPDATA>".to_string(),
                |p| p.display().to_string()
            ),
            if cfg!(feature = "cuda") { "cuda" } else { "ci" }
        ),
    };
    earshot_onnx::init(&lib, sha).expect("load ONNX Runtime");
}

/// The provider tests run on: CUDA with the `cuda` feature, else the CPU.
pub fn test_provider() -> earshot_onnx::Provider {
    if cfg!(feature = "cuda") {
        earshot_onnx::Provider::Cuda { device_id: 0 }
    } else {
        earshot_onnx::Provider::Cpu
    }
}

pub fn decode_flac(path: &Path) -> Vec<f32> {
    let mut reader = claxon::FlacReader::open(path)
        .unwrap_or_else(|e| panic!("cannot open {}: {e}", path.display()));
    let info = reader.streaminfo();
    assert_eq!(info.sample_rate, 16_000, "{}", path.display());
    assert_eq!(info.channels, 1, "{}", path.display());
    let scale = 1.0 / f64::from(1u32 << (info.bits_per_sample - 1));
    reader
        .samples()
        .map(|s| (f64::from(s.expect("flac sample")) * scale) as f32)
        .collect()
}

/// Utterances of speaker 61 and of speaker 121.
pub type SpeakerUtterances = (Vec<Vec<f32>>, Vec<Vec<f32>>);

/// Smoke utterances split by speaker: (speaker 61, speaker 121), as the core test helpers.
pub fn smoke_speech() -> &'static SpeakerUtterances {
    static SPEECH: OnceLock<SpeakerUtterances> = OnceLock::new();
    SPEECH.get_or_init(|| {
        let dir = repo_root().join("fixtures/smoke");
        let mut names: Vec<_> = std::fs::read_dir(&dir)
            .unwrap_or_else(|e| panic!("cannot list {}: {e}", dir.display()))
            .map(|e| {
                e.expect("dir entry")
                    .file_name()
                    .into_string()
                    .expect("utf-8")
            })
            .filter(|n| n.ends_with(".flac"))
            .collect();
        names.sort();
        let (mut a, mut b) = (Vec::new(), Vec::new());
        for name in names {
            let audio = decode_flac(&dir.join(&name));
            if name.starts_with("61-") {
                a.push(audio);
            } else if name.starts_with("121-") {
                b.push(audio);
            }
        }
        assert!(
            a.len() == 10 && b.len() == 10,
            "smoke set must have 10 clips of each speaker"
        );
        (a, b)
    })
}

/// `acceptance/aec_scenarios.toml`, parsed.
pub fn scenarios() -> &'static ScenarioFile {
    static FILE: OnceLock<ScenarioFile> = OnceLock::new();
    FILE.get_or_init(|| {
        let path = repo_root().join("acceptance/aec_scenarios.toml");
        let text = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()));
        toml::from_str(&text).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
    })
}

/// Simulates the named scenario with its own seed (as `earshot_core::testutil::run_scenario`).
pub fn run_scenario(name: &str) -> SimOutput {
    let file = scenarios();
    let scn = file
        .get(name)
        .unwrap_or_else(|| panic!("no scenario {name}"));
    let (far, near) = smoke_speech();
    let pool = SpeechPool { far, near };
    let mut rng = ChaCha8Rng::seed_from_u64(scn.seed);
    simulate(scn, &file.setup, &pool, &mut rng).expect("scenario simulates")
}
