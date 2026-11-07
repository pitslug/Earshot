//! The workload of `cargo xtask qnn-etw` (T8.3): loads ONNX Runtime with telemetry off,
//! registers Qualcomm's QNN plugin (`earshot_onnx::qnn::register_qnn`), runs Silero VAD on
//! the QNN EP over the frames of a real speech clip, prints `ready <pid>` and stays alive for
//! `<hold seconds>` so the ETW providers it registered can be listed, then exits. On x64 the
//! QNN device is Qualcomm's HTP simulator; on Windows ARM64 the NPU.
//!
//! `qnn_etw_probe <models dir> <hold seconds> <qnn|cpu> <clip.flac>` (built with
//! `--features qnn`); `cpu` is the baseline: the same work on the CPU EP without the plugin.
//!
//! `qnn_etw_probe <models dir> <hold seconds> transcribe <clip.flac> <hypothesis file>`: the
//! QNN run plus a transcription of the clip. When the NPU conversion of Parakeet TDT 110M
//! (`<models>/parakeet-qnn/parakeet-tdt-110m`, `cargo xtask qnn-convert`) is there, its encoder
//! runs on the QNN EP (prediction network and joint on the CPU, the NPU partition) and the
//! probe prints `transcribe-backend qnn`; otherwise the float model runs on the CPU EP and it
//! prints `transcribe-backend cpu`. Either line is printed only after ONNX Runtime's placement
//! passed: the production NPU partition check (`parakeet::check_qnn_partition`) or every
//! session wholly on the CPU; the probe fails otherwise. The transcript goes to the hypothesis file for the content
//! check and is never printed.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use earshot_core::asr::{BackendKind, CancelToken, TranscribeOptions, Transcriber, Vad};
use earshot_core::audio::{MonoBuffer, PIPELINE_RATE};
use earshot_onnx::parakeet::{ParakeetTdt, check_qnn_partition};
use earshot_onnx::qnn::{QNN_HOST, register_qnn};
use earshot_onnx::silero::SileroVad;
use earshot_onnx::{Provider, SessionConfig};

fn fail(msg: impl std::fmt::Display) -> ! {
    eprintln!("qnn_etw_probe: {msg}");
    std::process::exit(1);
}

fn read_flac(path: &Path) -> Vec<f32> {
    let mut reader =
        claxon::FlacReader::open(path).unwrap_or_else(|e| fail(format!("{}: {e}", path.display())));
    let info = reader.streaminfo();
    if info.sample_rate != 16_000 || info.channels != 1 {
        fail(format!("{}: not 16 kHz mono", path.display()));
    }
    let scale = 1.0 / f64::from(1u32 << (info.bits_per_sample - 1));
    reader
        .samples()
        .map(|s| (f64::from(s.unwrap_or_else(|e| fail(e))) * scale) as f32)
        .collect()
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (models, hold, mode, clip, out) = match args.as_slice() {
        [m, h, mode, clip] if mode == "qnn" || mode == "cpu" => (m, h, mode.as_str(), clip, None),
        [m, h, mode, clip, out] if mode == "transcribe" => (m, h, "transcribe", clip, Some(out)),
        _ => fail(
            "usage: qnn_etw_probe <models dir> <hold seconds> <qnn|cpu> <clip> \
             | <models dir> <hold seconds> transcribe <clip> <hypothesis file>",
        ),
    };
    let qnn = mode != "cpu";
    let models = PathBuf::from(models);
    let hold: u64 = hold.parse().unwrap_or_else(|e| fail(e));
    let samples = read_flac(Path::new(clip));
    earshot_onnx::init(
        &models.join(earshot_onnx::ORT_CPU_DYLIB),
        earshot_onnx::ORT_CPU_DYLIB_SHA256,
    )
    .unwrap_or_else(|e| fail(e));
    let device = qnn.then(|| register_qnn(&models, &QNN_HOST).unwrap_or_else(|e| fail(e)));
    let config = SessionConfig {
        provider: if qnn { Provider::Qnn } else { Provider::Cpu },
        intra_threads: Some(1),
        ..SessionConfig::default()
    };
    let mut vad = SileroVad::load(&models.join("silero/silero_vad.onnx"), &config)
        .unwrap_or_else(|e| fail(e));
    let frame = vad.frame_len();
    let mut frames = 0;
    for chunk in samples.chunks_exact(frame) {
        vad.speech_probability(chunk).unwrap_or_else(|e| fail(e));
        frames += 1;
    }
    println!(
        "qnn_etw_probe: {:?} device, {frames} VAD frames of the clip, placement {}",
        device.map(|d| d.hardware),
        vad.backend()
    );
    if let Some(out) = out {
        let npu_dir = models.join("parakeet-qnn/parakeet-tdt-110m");
        let (mut t, backend) = if npu_dir.join("encoder.onnx").is_file() {
            let encoder = SessionConfig {
                provider: Provider::Qnn,
                intra_threads: Some(2),
                ..SessionConfig::default()
            };
            let t = ParakeetTdt::load_split(
                &npu_dir,
                &encoder,
                &SessionConfig::cpu(2),
                "parakeet-tdt-110m (NPU conversion)",
                true,
            )
            .unwrap_or_else(|e| fail(e));
            if !t.is_static() {
                fail(format!(
                    "{} is not a static NPU conversion",
                    npu_dir.display()
                ));
            }
            (t, "qnn")
        } else {
            let t = ParakeetTdt::load(
                &models.join("parakeet/tdt-110m"),
                &SessionConfig::cpu(2),
                "parakeet-tdt-110m",
                true,
            )
            .unwrap_or_else(|e| fail(format!("{e} (needs `cargo xtask fetch-models --set ci`)")));
            (t, "cpu")
        };
        for b in t.backends() {
            println!("qnn_etw_probe: transcriber session {b}");
        }
        // The backend line is printed only for the placement ONNX Runtime reported, never
        // for the request alone: on the NPU the production partition check (the encoder on
        // QNN with its documented float boundary, the steps on the CPU), else every session
        // wholly on the CPU.
        let sessions = t.backends();
        let placed = if backend == "qnn" {
            check_qnn_partition(&sessions)
        } else {
            match sessions
                .iter()
                .find(|b| b.kind() != Some(BackendKind::Cpu) || !b.fully_on_requested())
            {
                Some(b) => Err(format!("a session did not run wholly on the CPU ({b})")),
                None => Ok(()),
            }
        };
        placed.unwrap_or_else(|e| fail(format!("transcriber placement: {e}")));
        let started = Instant::now();
        let audio = MonoBuffer::new(PIPELINE_RATE, samples);
        let result = t
            .transcribe(&audio, &TranscribeOptions::live(), &CancelToken::new())
            .unwrap_or_else(|e| fail(e));
        let text: Vec<String> = result
            .segments
            .iter()
            .flat_map(|s| s.timed_words())
            .map(|w| w.text)
            .collect();
        std::fs::write(out, text.join(" ")).unwrap_or_else(|e| fail(e));
        println!(
            "qnn_etw_probe: transcribed {} words in {:.0} s (not printed)",
            text.len(),
            started.elapsed().as_secs_f64()
        );
        println!("transcribe-backend {backend} {}", result.backend.device);
    }
    println!("ready {}", std::process::id());
    let _ = std::io::stdout().flush();
    std::thread::sleep(Duration::from_secs(hold));
}
