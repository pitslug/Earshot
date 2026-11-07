//! Prints an ONNX model's inputs, outputs and custom metadata (a development aid).
//!
//! `cargo run -p earshot-onnx --example onnx_probe -- <model.onnx>...` (CPU ONNX Runtime from
//! the models directory, as the tests load it).

use std::path::PathBuf;

use earshot_onnx::{OnnxSession, SessionConfig};

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
    for path in std::env::args().skip(1) {
        let s = OnnxSession::load(&PathBuf::from(&path), &SessionConfig::cpu(1)).expect("load");
        println!("== {path}");
        println!("inputs:  {:?}", s.input_names());
        println!("outputs: {:?}", s.output_names());
        for (k, v) in s.custom_metadata().expect("metadata") {
            if k != "doc" {
                println!("meta {k} = {v}");
            }
        }
    }
}
