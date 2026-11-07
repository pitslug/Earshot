//! whisper.cpp (ggml linked statically) and llama.cpp (ggml in DLLs) in one process: both
//! load, both run on the backend the build asks for, concurrently, and whisper's transcript
//! is the same before, during and after llama.cpp ran.

mod common;

use std::path::Path;

use earshot_asr_whisper::{WhisperConfig, evidence};
use earshot_core::asr::{BackendKind, CancelToken, TranscribeOptions, Transcriber};
use earshot_core::audio::{MonoBuffer, PIPELINE_RATE};

const BASE_EN: &str = "whisper/ggml-base.en.bin";
const CLIP: &str = "fixtures/smoke/61-70968-0000.flac";

fn smoke_clip() -> MonoBuffer {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join(CLIP);
    let mut reader =
        claxon::FlacReader::open(&path).unwrap_or_else(|e| panic!("open {}: {e}", path.display()));
    let info = reader.streaminfo();
    assert_eq!(
        (info.sample_rate, info.channels, info.bits_per_sample),
        (16_000, 1, 16)
    );
    let samples = reader
        .samples()
        .map(|s| s.expect("flac sample") as f32 / 32768.0)
        .collect();
    MonoBuffer::new(PIPELINE_RATE, samples)
}

fn transcript(t: &mut impl Transcriber, audio: &MonoBuffer) -> String {
    let out = t
        .transcribe(audio, &TranscribeOptions::live(), &CancelToken::new())
        .unwrap_or_else(|e| panic!("{e}"));
    out.segments
        .iter()
        .map(|s| s.text.trim())
        .collect::<Vec<_>>()
        .join(" ")
}

#[test]
#[ignore = "needs the summary model set: cargo xtask fetch-models --set summary (and --set ci); run: cargo test -p earshot-llm-local -- --include-ignored"]
fn coexist_whisper_and_llama_run_in_one_process() {
    let audio = smoke_clip();
    let mut config = WhisperConfig::new(common::model_path(BASE_EN, "ci"));
    // whisper.cpp on CUDA in a CUDA build. In a Vulkan build whisper runs on the CPU (its
    // Vulkan backend is linked but unused): whisper.cpp's Vulkan log has no evidence rule
    // yet (earshot_asr_whisper::evidence, T6.10d), and a backend is never taken on trust.
    // llama.cpp still runs on Vulkan.
    config.use_gpu = cfg!(feature = "cuda");
    let expected = if cfg!(feature = "cuda") {
        BackendKind::Cuda
    } else {
        BackendKind::Cpu
    };
    let whisper_mark = evidence::log_mark();
    let (mut whisper, ev) =
        evidence::load_verified(&config, expected).unwrap_or_else(|e| panic!("{e}"));
    eprintln!(
        "whisper backend {:?}: {:?}",
        whisper.backend_info(),
        ev.lines
    );
    let before = transcript(&mut whisper, &audio);
    assert!(
        before.split_whitespace().count() > 5,
        "whisper heard almost nothing: {before:?}"
    );

    // llama.cpp on its own thread while whisper decodes on this one.
    let llama = std::thread::spawn(|| {
        let (mut llm, report) = common::load_model();
        let (generation, parsed) = common::summarise(&mut llm);
        (
            common::speed("coexist", &report, &generation),
            parsed.map(|_| ()),
        )
    });
    let during = transcript(&mut whisper, &audio);
    let (speed, parsed) = llama.join().expect("the llama thread panicked");
    eprintln!("{speed}");
    parsed.unwrap_or_else(|e| {
        panic!("llama.cpp output alongside whisper is not a valid SummaryV1: {e}")
    });
    let after = transcript(&mut whisper, &audio);

    eprintln!("whisper: {before}");
    assert_eq!(
        during, before,
        "whisper's transcript changed while llama.cpp ran"
    );
    assert_eq!(
        after, before,
        "whisper's transcript changed after llama.cpp ran"
    );
    // Both loggers are separate: llama.cpp's lines reached only its own ring and whisper.cpp's
    // only whisper's capture, and each capture saw its own library.
    let whisper_lines = evidence::logs_since_complete(whisper_mark)
        .expect("the whisper capture dropped lines during the test");
    let llama_lines = earshot_llm_local::log::since(0).expect("the llama ring dropped lines");
    logs_are_separate(&whisper_lines, &llama_lines).unwrap_or_else(|e| panic!("{e}"));
}

/// A line written by llama.cpp itself (not its ggml, whose lines look like whisper's ggml).
fn is_llama_line(line: &str) -> bool {
    let l = line.trim_start();
    l.starts_with("llama_") || l.starts_with("load_tensors:") || l.starts_with("print_info:")
}

/// A line written by whisper.cpp itself.
fn is_whisper_line(line: &str) -> bool {
    line.trim_start().starts_with("whisper_")
}

/// `Ok` when each capture holds lines of its own library and none of the other's.
fn logs_are_separate(whisper: &[String], llama: &[String]) -> Result<(), String> {
    if !whisper.iter().any(|l| is_whisper_line(l)) {
        return Err(format!(
            "the whisper capture saw no whisper.cpp line ({} lines)",
            whisper.len()
        ));
    }
    if !llama.iter().any(|l| is_llama_line(l)) {
        return Err(format!(
            "the llama ring saw no llama.cpp line ({} lines)",
            llama.len()
        ));
    }
    let crossed: Vec<&String> = whisper.iter().filter(|l| is_llama_line(l)).collect();
    if !crossed.is_empty() {
        return Err(format!(
            "llama.cpp lines reached the whisper capture: {crossed:?}"
        ));
    }
    let crossed: Vec<&String> = llama.iter().filter(|l| is_whisper_line(l)).collect();
    if !crossed.is_empty() {
        return Err(format!(
            "whisper.cpp lines reached the llama ring: {crossed:?}"
        ));
    }
    Ok(())
}

/// The negative control for the logger check of `coexist_whisper_and_llama_run_in_one_process`.
#[test]
fn coexist_logger_check_detects_crossed_or_missing_lines() {
    let own = |l: &[&str]| l.iter().map(|s| (*s).to_string()).collect::<Vec<_>>();
    let whisper = own(&[
        "whisper_init_from_file_with_params_no_state: loading model from 'x'",
        concat!("gg", "ml_cuda_init: found 1 CUDA devices"),
    ]);
    let llama = own(&[
        "llama_model_loader: loaded meta data with 26 key-value pairs",
        "load_tensors:   CPU_Mapped model buffer size =  1059.89 MiB",
        concat!("gg", "ml_cuda_init: found 1 CUDA devices"),
    ]);
    assert_eq!(logs_are_separate(&whisper, &llama), Ok(()));

    let mut leaked = whisper.clone();
    leaked.push("load_tensors: offloaded 29/29 layers to GPU".into());
    assert!(logs_are_separate(&leaked, &llama).is_err());
    let mut leaked = llama.clone();
    leaked.push("whisper_backend_init_gpu: using CUDA0 backend".into());
    assert!(logs_are_separate(&whisper, &leaked).is_err());
    // An empty capture proves nothing.
    assert!(logs_are_separate(&[], &llama).is_err());
    assert!(logs_are_separate(&whisper, &[]).is_err());
}
