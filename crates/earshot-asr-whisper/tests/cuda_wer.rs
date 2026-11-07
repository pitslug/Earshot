//! asr.wer_smoke_base on CUDA: the same corpus WER check as `cpu_wer`, on the GPU.
//!
//! Only built with `--features cuda`. GPU use is proven from whisper.cpp's own log
//! (`whisper_backend_init_gpu: using CUDA<n> backend` plus model weights in a `CUDA<n>`
//! buffer) and must match `backend_info()`.
#![cfg(feature = "cuda")]

mod common;

use earshot_asr_whisper::evidence;
use earshot_core::asr::{BackendKind, Transcriber};

#[test]
fn base_en_meets_wer_smoke_base_on_cuda() {
    common::capture_whisper_logs();
    let (mut transcriber, load) = common::load_base_en(true);
    let info = transcriber.backend_info();
    let logs = common::captured_logs();
    let all = logs.join("\n");

    assert_eq!(info.kind, BackendKind::Cuda, "{info:?}\n{all}");
    assert_eq!(info.model, "ggml-base.en");
    assert!(
        logs.iter()
            .any(|l| l.contains("whisper_backend_init_gpu") && l.contains("using CUDA0 backend")),
        "whisper.cpp did not select CUDA0:\n{all}"
    );
    assert!(
        logs.iter().any(|l| l.contains("whisper_model_load")
            && l.contains("CUDA0")
            && l.contains("total size")),
        "model weights were not placed in a CUDA buffer:\n{all}"
    );
    // ggml-cuda logs `Device 0: <name>, compute capability ...`; backend_info() must name
    // the same device.
    assert!(
        logs.iter()
            .any(|l| l.contains("Device 0:") && l.contains(info.device.as_str())),
        "backend_info device {:?} is not CUDA device 0 in the log:\n{all}",
        info.device
    );

    // The shared rule (CLI, accept m2): backend_info() and the log classification agree.
    let (log, lines) = evidence::classify_whisper_log(&logs);
    evidence::check_backend(BackendKind::Cuda, &info, log)
        .unwrap_or_else(|e| panic!("{e}: {lines:?}"));

    let run = common::run_smoke_set(&mut transcriber);
    let limit = common::wer_smoke_base_limit();
    eprintln!("{} [{info:?}]", run.summary("cuda", load));
    assert!(
        run.wer_percent() <= limit,
        "{} = {:.2}% exceeds {limit}% on {info:?}",
        common::WER_SMOKE_BASE,
        run.wer_percent()
    );
}
