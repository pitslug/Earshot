//! Where CAM++ spends its time on CUDA (T6.11): per-call cost with one repeated input length
//! against a new length on every call, on the CPU and on CUDA, plus ONNX Runtime's node
//! placement.
//!
//! ```text
//! cargo run --release -p earshot-onnx [--features cuda] --example campp_probe -- cpu|cuda
//! ```
//!
//! Models (and ONNX Runtime) come from EARSHOT_MODELS (default %LOCALAPPDATA%\Earshot\models).
//! Input is deterministic noise: only timing is measured.

use std::path::PathBuf;
use std::time::Instant;

use earshot_core::asr::CancelToken;
use earshot_core::diar::SpeakerEmbedder;
use earshot_onnx::embed::CamppEmbedder;
use earshot_onnx::{Provider, SessionConfig};

const CAMPPLUS: &str = "embedding/3dspeaker_speech_campplus_sv_en_voxceleb_16k.onnx";
/// Calls per condition.
const CALLS: usize = 40;

fn main() {
    let provider = match std::env::args().nth(1).as_deref() {
        Some("cuda") => Provider::Cuda { device_id: 0 },
        Some("cpu") | None => Provider::Cpu,
        Some(other) => panic!("cpu|cuda, not {other}"),
    };
    let models = std::env::var_os("EARSHOT_MODELS")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("LOCALAPPDATA")
                .map(|b| PathBuf::from(b).join("Earshot").join("models"))
        })
        .expect("EARSHOT_MODELS or LOCALAPPDATA");
    let (lib, sha) = if provider == Provider::Cpu {
        (
            earshot_onnx::ORT_CPU_DYLIB,
            earshot_onnx::ORT_CPU_DYLIB_SHA256,
        )
    } else {
        (
            earshot_onnx::ORT_CUDA_DYLIB,
            earshot_onnx::ORT_CUDA_DYLIB_SHA256,
        )
    };
    earshot_onnx::init(&models.join(lib), sha).expect("ONNX Runtime");
    let config = SessionConfig {
        provider,
        intra_threads: Some(4),
        background: None,
    };
    let started = Instant::now();
    let mut e = CamppEmbedder::load(&models.join(CAMPPLUS), &config).expect("CAM++");
    println!("{provider}: load {:.2} s", started.elapsed().as_secs_f64());
    println!("placement: {}", e.backend());
    let mut state = 0x1234_5678u32;
    let mut noise = |n: usize| -> Vec<f32> {
        (0..n)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 17;
                state ^= state << 5;
                (f64::from(state) / f64::from(u32::MAX) - 0.5) as f32 * 0.1
            })
            .collect()
    };
    let cancel = CancelToken::new();
    let mut time = |label: &str, lengths: &[usize]| {
        let clips: Vec<Vec<f32>> = lengths.iter().map(|&n| noise(n)).collect();
        let mut first = 0.0;
        let started = Instant::now();
        for (i, c) in clips.iter().enumerate() {
            let t = Instant::now();
            e.embed(&[c.as_slice()], &cancel).expect("embed");
            if i == 0 {
                first = t.elapsed().as_secs_f64() * 1000.0;
            }
        }
        let total = started.elapsed().as_secs_f64() * 1000.0;
        println!(
            "{label}: first call {first:.1} ms, mean of the rest {:.1} ms",
            (total - first) / (lengths.len() - 1) as f64
        );
    };
    // 1.5 s every time (one shape), then a different length every call (1.0 to 3.0 s in
    // 10 ms steps, none repeated), then the first shape again.
    time("same length", &[24_000; CALLS]);
    let varied: Vec<usize> = (0..CALLS).map(|i| 16_000 + i * 160 * 5).collect();
    time("new length every call", &varied);
    time("same length again", &[24_000; CALLS]);
    time("lengths seen before", &varied);
}
