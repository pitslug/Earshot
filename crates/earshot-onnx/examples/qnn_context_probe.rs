//! T12.12: compiled QNN context models for the Parakeet NPU encoder (the app loads them since
//! T12.12a, `earshot_runtime::npu_compiled`; this probe measures them by hand).
//!
//! `compile` loads the NPU conversion's `encoder.onnx` once with
//! [`OnnxSession::load_writing_context`], so the QNN plugin writes `<work>/encoder_ctx.onnx` and
//! its context binary. `qdq` and `ctx` then load `--sessions` whole Parakeet models one after
//! another, the encoder from the conversion (`qdq`, the load before T12.12a) or from the context
//! model (`ctx`), and print each load's time, this process's private
//! memory and the machine's free memory, then transcribe the FLAC files on every session and
//! say whether each session's words equal the first `qdq`-style reference printed. Counts and
//! times only, plus the words of the public smoke clips.
//!
//! `cargo run --release -p earshot-onnx --features qnn --example qnn_context_probe --
//! <model dir> <work dir> compile|qdq|ctx [--sessions N] [<file.flac>...]`
//! (ARM64, models folder with sets `qnn` and `qnn-models`). Run each mode in its own process.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Instant;

use earshot_core::asr::{CancelToken, TranscribeOptions, Transcriber};
use earshot_core::audio::{MonoBuffer, PIPELINE_RATE};
use earshot_onnx::parakeet::ParakeetTdt;
use earshot_onnx::qnn::{QNN_HOST, register_qnn};
use earshot_onnx::{OnnxSession, Provider, SessionConfig};

const CTX_MODEL: &str = "encoder_ctx.onnx";

fn main() {
    let models = std::env::var_os("EARSHOT_MODELS")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("LOCALAPPDATA")
                .map(|b| PathBuf::from(b).join("Earshot").join("models"))
        })
        .expect("EARSHOT_MODELS or LOCALAPPDATA");
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let mut sessions = 3usize;
    if let Some(i) = args.iter().position(|a| a == "--sessions") {
        sessions = args[i + 1].parse().expect("--sessions N");
        args.drain(i..i + 2);
    }
    let [dir, work, mode, files @ ..] = &args[..] else {
        panic!("usage: <model dir> <work dir> compile|qdq|ctx [--sessions N] [flac...]");
    };
    let (dir, work) = (PathBuf::from(dir), PathBuf::from(work));
    std::fs::create_dir_all(&work).expect("work dir");
    println!("memory before ONNX Runtime: {}", memory());
    earshot_onnx::init(
        &models.join(earshot_onnx::ORT_CPU_DYLIB),
        earshot_onnx::ORT_CPU_DYLIB_SHA256,
    )
    .expect("ONNX Runtime");
    register_qnn(&models, &QNN_HOST).expect("QNN plugin");
    println!("memory after the plugin: {}", memory());
    let qnn = SessionConfig {
        provider: Provider::Qnn,
        intra_threads: Some(4),
        ..SessionConfig::default()
    };
    if mode == "compile" {
        let out = work.join(CTX_MODEL);
        let out_text = out.display().to_string();
        let started = Instant::now();
        let session = OnnxSession::load_writing_context(&dir.join("encoder.onnx"), &qnn, &out)
            .expect("compile");
        println!("writing {out_text}");
        println!(
            "compiled in {:.1} s; {}",
            started.elapsed().as_secs_f64(),
            session.backend()
        );
        drop(session);
        for entry in std::fs::read_dir(&work).expect("read work dir").flatten() {
            let len = entry.metadata().map_or(0, |m| m.len());
            println!(
                "  {} {:.1} MiB",
                entry.file_name().to_string_lossy(),
                len as f64 / 1048576.0
            );
        }
        return;
    }
    if !matches!(mode.as_str(), "qdq" | "ctx") {
        panic!("unknown mode {mode}");
    }
    let audio: Vec<(String, MonoBuffer)> = files
        .iter()
        .map(|f| {
            let (rate, samples) = read_flac(Path::new(f));
            assert_eq!(rate, 16_000);
            (f.clone(), MonoBuffer::new(PIPELINE_RATE, samples))
        })
        .collect();
    let mut loaded: Vec<ParakeetTdt> = Vec::new();
    for n in 1..=sessions {
        let started = Instant::now();
        let encoder = if mode == "qdq" {
            OnnxSession::load(&dir.join("encoder.onnx"), &qnn)
        } else {
            OnnxSession::load(&work.join(CTX_MODEL), &qnn)
        }
        .unwrap_or_else(|e| panic!("session {n}: {e}"));
        let t =
            ParakeetTdt::load_with_encoder(&dir, encoder, &SessionConfig::cpu(4), "probe", true)
                .unwrap_or_else(|e| panic!("session {n}: {e}"));
        let load_s = started.elapsed().as_secs_f64();
        let partition = earshot_onnx::parakeet::check_qnn_partition(&t.backends());
        println!(
            "{mode} session {n}: loaded in {load_s:.1} s; partition {partition:?}; {}",
            memory()
        );
        loaded.push(t);
    }
    let options = TranscribeOptions::live();
    let cancel = CancelToken::new();
    for (name, buffer) in &audio {
        let mut first: Option<Vec<String>> = None;
        for (i, t) in loaded.iter_mut().enumerate() {
            let started = Instant::now();
            let out = t.transcribe(buffer, &options, &cancel).expect("transcribe");
            let words: Vec<String> = out
                .segments
                .iter()
                .flat_map(|s| s.timed_words())
                .map(|w| {
                    // Text, times and probability: compared exactly across sessions and modes.
                    format!(
                        "{}@{}-{}:{:.6}",
                        w.text,
                        w.start.as_millis(),
                        w.end.as_millis(),
                        w.probability
                    )
                })
                .collect();
            let same = first.as_ref().map(|f| *f == words);
            println!(
                "  {name} session {}: {:.2} s, {} words{}",
                i + 1,
                started.elapsed().as_secs_f64(),
                words.len(),
                match same {
                    None => format!(": {}", words.join(" ")),
                    Some(true) => ", same as session 1".to_owned(),
                    Some(false) => format!(", DIFFERENT: {}", words.join(" ")),
                }
            );
            first.get_or_insert(words);
        }
    }
    println!("after decoding: {}", memory());
}

/// This process's private memory and the machine's free memory, GiB.
fn memory() -> String {
    let script = format!(
        "$p = Get-Process -Id {}; $f = (Get-CimInstance Win32_OperatingSystem).FreePhysicalMemory; \
         '{{0:N2}} {{1:N2}}' -f ($p.PrivateMemorySize64 / 1GB), ($f / 1MB)",
        std::process::id()
    );
    let out = Command::new("powershell")
        .args(["-NoProfile", "-NonInteractive", "-Command", &script])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned())
        .unwrap_or_default();
    let mut parts = out.split_whitespace();
    format!(
        "private {} GiB, system free {} GiB",
        parts.next().unwrap_or("?"),
        parts.next().unwrap_or("?")
    )
}

fn read_flac(path: &Path) -> (u32, Vec<f32>) {
    let mut reader = claxon::FlacReader::open(path).expect("flac");
    let info = reader.streaminfo();
    let scale = 1.0 / f64::from(1u32 << (info.bits_per_sample - 1));
    let samples = reader
        .samples()
        .map(|s| (f64::from(s.expect("sample")) * scale) as f32)
        .collect();
    (info.sample_rate, samples)
}
