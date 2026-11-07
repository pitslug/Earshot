//! Loads a Parakeet TDT directory with the encoder on the QNN execution provider and the
//! prediction network and joint on the CPU (the NPU partition, T8.3), prints where ONNX
//! Runtime placed every session and transcribes FLAC files (a development aid; on x64 QNN is
//! Qualcomm's HTP simulator).
//!
//! `cargo run --release -p earshot-onnx --features qnn --example qnn_probe -- <model dir>
//! [<file.flac>...]` (needs `cargo xtask fetch-models --set qnn-x64`, or `--set qnn` on ARM64).

use std::path::PathBuf;
use std::time::Instant;

use earshot_core::asr::{CancelToken, TranscribeOptions, Transcriber};
use earshot_core::audio::{MonoBuffer, PIPELINE_RATE};
use earshot_onnx::parakeet::ParakeetTdt;
use earshot_onnx::qnn::{QNN_HOST, register_qnn};
use earshot_onnx::{Provider, SessionConfig};

fn main() {
    let models = std::env::var_os("EARSHOT_MODELS")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("LOCALAPPDATA")
                .map(|b| PathBuf::from(b).join("Earshot").join("models"))
        })
        .expect("EARSHOT_MODELS or LOCALAPPDATA");
    earshot_onnx::init(
        &models.join(earshot_onnx::ORT_CPU_DYLIB),
        earshot_onnx::ORT_CPU_DYLIB_SHA256,
    )
    .expect("ONNX Runtime");
    let device = register_qnn(&models, &QNN_HOST).expect("QNN plugin");
    println!("qnn device: {device:?}");
    let mut args = std::env::args().skip(1);
    let dir = PathBuf::from(args.next().expect("model dir"));
    let encoder = SessionConfig {
        provider: Provider::Qnn,
        intra_threads: Some(4),
        ..SessionConfig::default()
    };
    let started = Instant::now();
    let mut t = ParakeetTdt::load_split(&dir, &encoder, &SessionConfig::cpu(4), "probe", false)
        .expect("load");
    println!(
        "loaded in {:.1} s, static encoder: {}",
        started.elapsed().as_secs_f64(),
        t.is_static()
    );
    for b in t.backends() {
        println!("  {b}; partition(0): {:?}", b.check_partition(0));
        for (ep, ops) in &b.placement.ops {
            if ep != earshot_onnx::QNN_EP {
                println!("    {ep}: {ops:?}");
            }
        }
    }
    for f in args {
        let (rate, samples) = read_flac(&PathBuf::from(&f));
        assert_eq!(rate, 16_000);
        let audio = MonoBuffer::new(PIPELINE_RATE, samples);
        let started = Instant::now();
        let out = t
            .transcribe(&audio, &TranscribeOptions::live(), &CancelToken::new())
            .expect("transcribe");
        let text: Vec<String> = out
            .segments
            .iter()
            .flat_map(|s| s.timed_words())
            .map(|w| w.text)
            .collect();
        println!(
            "{f} ({:.1} s): {}",
            started.elapsed().as_secs_f64(),
            text.join(" ")
        );
    }
}

fn read_flac(path: &std::path::Path) -> (u32, Vec<f32>) {
    let mut reader = claxon::FlacReader::open(path).expect("flac");
    let info = reader.streaminfo();
    let scale = 1.0 / f64::from(1u32 << (info.bits_per_sample - 1));
    let samples = reader
        .samples()
        .map(|s| (f64::from(s.expect("sample")) * scale) as f32)
        .collect();
    (info.sample_rate, samples)
}
