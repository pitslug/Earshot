//! asr.wer_smoke_base on Vulkan (T6.10d): the same corpus WER check as `cpu_wer`, on the
//! first Vulkan device.
//!
//! Only built with `--features vulkan` (use `cargo xtask build`'s short target directory:
//! ggml's nested Vulkan shader build fails under a deep path). Vulkan use is proven from
//! whisper.cpp's own log (`whisper_backend_init_gpu: using Vulkan<n> backend` plus model
//! weights in a `Vulkan<n>` buffer) and must match `backend_info()`. A machine without a
//! Vulkan device fails here: a missing device is never a silent CPU run.
#![cfg(feature = "vulkan")]

mod common;

use std::time::Instant;

use earshot_asr_whisper::{WhisperConfig, WhisperTranscriber, evidence};
use earshot_core::asr::{BackendKind, Transcriber};

#[test]
fn base_en_meets_wer_smoke_base_on_vulkan() {
    common::capture_whisper_logs();
    eprintln!(
        "whisper.cpp GPUs: {:#?}",
        earshot_asr_whisper::gpu_devices()
    );
    let config = WhisperConfig::for_backend(common::base_en(), BackendKind::Vulkan)
        .expect("this build and machine have a Vulkan device");
    let started = Instant::now();
    let mark = evidence::log_mark();
    let mut transcriber = WhisperTranscriber::load(&config).expect("load base.en");
    let load = started.elapsed();
    let info = transcriber.backend_info();
    let logs = evidence::logs_since(mark);
    let all = logs.join("\n");
    eprintln!("whisper.cpp log of the Vulkan load:\n{all}");

    assert_eq!(info.kind, BackendKind::Vulkan, "{info:?}\n{all}");
    assert_eq!(info.model, "ggml-base.en");
    assert!(
        logs.iter().any(|l| l.contains("whisper_backend_init_gpu")
            && l.contains("using Vulkan")
            && l.contains("backend")),
        "whisper.cpp did not select a Vulkan device:\n{all}"
    );
    assert!(
        logs.iter().any(|l| l.contains("whisper_model_load")
            && l.contains("Vulkan")
            && l.contains("total size")),
        "model weights were not placed in a Vulkan buffer:\n{all}"
    );

    // The shared rule (CLI, runtime): backend_info() and the log classification agree.
    let (log, lines) = evidence::classify_whisper_log(&logs);
    assert_eq!(log, evidence::LogBackend::Vulkan, "{lines:?}");
    evidence::check_backend(BackendKind::Vulkan, &info, log)
        .unwrap_or_else(|e| panic!("{e}: {lines:?}"));

    let run = common::run_smoke_set(&mut transcriber);
    let limit = common::wer_smoke_base_limit();
    eprintln!("{} [{info:?}]", run.summary("vulkan", load));
    assert!(
        run.wer_percent() <= limit,
        "{} = {:.2}% exceeds {limit}% on {info:?}",
        common::WER_SMOKE_BASE,
        run.wer_percent()
    );
}
