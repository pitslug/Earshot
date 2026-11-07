//! asr.wer_smoke_base on the CPU: base.en over the whole smoke set, corpus WER after Whisper
//! normalisation, against the limit in `acceptance/thresholds.toml`. The CPU backend is
//! proven from whisper.cpp's own log and must match `backend_info()`.

mod common;

use earshot_asr_whisper::evidence;
use earshot_core::asr::{BackendKind, Transcriber};

#[test]
fn smoke_set_has_twenty_16k_mono_clips_with_references() {
    let set = common::smoke_set();
    assert_eq!(set.len(), common::SMOKE_CLIPS);
    for (id, reference) in &set {
        assert!(!reference.trim().is_empty(), "{id} has no reference");
        let samples = common::load_smoke_clip(id);
        let seconds = samples.len() as f32 / common::SAMPLE_RATE as f32;
        assert!((0.5..35.0).contains(&seconds), "{id} is {seconds} s");
        assert!(samples.iter().any(|s| s.abs() > 0.01), "{id} is silent");
    }
}

#[test]
fn threshold_is_read_from_thresholds_toml() {
    let limit = common::wer_smoke_base_limit();
    assert!(limit > 0.0 && limit < 100.0, "{limit}");
}

#[test]
fn base_en_meets_wer_smoke_base_on_cpu() {
    common::capture_whisper_logs();
    let (mut transcriber, load) = common::load_base_en(false);
    let info = transcriber.backend_info();
    assert_eq!(info.kind, BackendKind::Cpu, "{info:?}");
    assert_eq!(info.device, "cpu");
    assert_eq!(info.model, "ggml-base.en");

    // whisper.cpp logs `whisper_backend_init_gpu: no GPU found` when it computes on the CPU
    // and `whisper_backend_init_gpu: using <dev> backend` for any GPU backend, so this holds
    // for every feature combination.
    let logs = common::captured_logs();
    let all = logs.join("\n");
    assert!(
        logs.iter()
            .any(|l| l.contains("whisper_backend_init_gpu: no GPU found")),
        "whisper.cpp did not report the CPU fallback:\n{all}"
    );
    assert!(
        !logs
            .iter()
            .any(|l| l.contains("whisper_backend_init_gpu: using")),
        "use_gpu = false must not pick a GPU backend:\n{all}"
    );

    // The shared rule (CLI, accept m2): backend_info() and the log classification agree.
    let (log, lines) = evidence::classify_whisper_log(&logs);
    evidence::check_backend(BackendKind::Cpu, &info, log)
        .unwrap_or_else(|e| panic!("{e}: {lines:?}"));

    let run = common::run_smoke_set(&mut transcriber);
    let limit = common::wer_smoke_base_limit();
    eprintln!("{} [{info:?}]", run.summary("cpu", load));
    assert!(
        run.wer_percent() <= limit,
        "{} = {:.2}% exceeds {limit}% on {info:?}",
        common::WER_SMOKE_BASE,
        run.wer_percent()
    );
}
