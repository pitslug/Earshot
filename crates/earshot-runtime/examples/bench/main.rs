//! `cargo xtask bench` (T8.1): how fast and how large one part of the production path is on
//! this machine: real-time factor, latency of live chunks, peak memory.
//!
//! ```text
//! cargo build --release -p earshot-runtime [--features cuda|vulkan|qnn] --example bench
//! bench asr|diar|aec --root <repo> --backend <b> --models <dir> --scratch <dir> [--model <name>]
//! ```
//!
//! Prints one JSON object on stdout, progress on stderr; one process per part, so its peak
//! memory is that part's. Run through `cargo xtask bench`, which writes the results file.
//! A timing is only a performance figure on an idle host: the runner records a `note`.
//!
//! The audio of every part is the benchmark meeting the app ships
//! (`apps/desktop/src-tauri/resources/benchmark`, `cargo xtask bench-meeting`, public AMI
//! speech), so the figures need no fixtures. Definitions, fixed before measuring:
//!
//! - **`asr`** on the production default model for English meetings on `--backend` (the first
//!   rung of the model check's English ladder, or `--model`). `live`: a real
//!   [`start_live`] session on a throwaway store, replayed in real time through
//!   [`Paced`], speaker work and live summaries off. A chunk is one live final (one speech
//!   stretch since T6.13b); its latency is the wall time the session reported it minus the
//!   wall time its end was delivered. `rtf`: the final pass's transcriber with
//!   `asr::transcribe_in_pieces` over each track, one after the other, as `model_eval`'s
//!   `speed` corpus: decode seconds over audio seconds.
//! - **`diar`** on the CPU, the only place production runs it (T6.11): the final pass's
//!   models and settings over each track in the order the live path takes them
//!   (`Diarizer::advance`, one batch at a time, then `diarize_from` for the rest). A chunk is
//!   one batch. `rtf`: total over audio seconds, tracks one after the other.
//! - **`aec`** on the CPU on this one thread, with the default canceller. The mic is the
//!   meeting's mic plus the reference delayed by [`ECHO_DELAY_MS`] at [`ECHO_GAIN`] (the
//!   meeting has no echo, and a canceller with no echo path to follow does less work). A
//!   chunk is one `aec::BLOCK` (8 ms of audio, so its budget is 8 ms). `rtf`: as `aec.rtf`.
//!
//! The backend of `asr` is proven by the runtime's loaders (whisper.cpp's log or ONNX
//! Runtime's node placement, checked after every call) and `diar`'s by node placement; a run
//! on any other backend fails. Peak memory is `earshot_proc::peak_memory` at the end of the
//! run: system memory only, so a GPU model's video memory is not in it (whisper's own log of
//! its buffer sizes is in the evidence). No transcript text is printed.

use std::path::{Path, PathBuf};
use std::sync::mpsc::RecvTimeoutError;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use earshot_core::aec::{BLOCK, EchoCanceller, EchoCancellerConfig};
use earshot_core::asr::{
    BackendKind, CancelToken, MeetingTime, PieceConfig, TranscribeOptions, Transcriber,
    TranscriptEvent, transcribe_in_pieces,
};
use earshot_core::audio::{MonoBuffer, PIPELINE_RATE, Sample};
use earshot_core::capture::{DeviceTime, ReplayBuilder};
use earshot_core::diar::{Diarizer, SpeakerEmbedder, WindowResults};
use earshot_core::pipeline::PipelineError;
use earshot_runtime::backends::{
    Accelerator, EvidenceLoader, ModelDir, final_pass_asr, init_onnx, live_asr_factory,
    load_diarisation, load_silero, onnx_library, speaker_threads,
};
use earshot_runtime::benchmark::{CATCH_UP_LIMIT, Scratch};
use earshot_runtime::live::{LiveConfig, RuntimeEvent, RuntimeWarning, start_live};
use earshot_runtime::model_check;
use earshot_runtime::post::PostConfig;
use earshot_runtime::replay::{PaceClock, PaceState, Paced, WallClock};
use earshot_runtime::{Services, SystemClock, os_seeded_rng};
use earshot_store::{DataDir, read_flac};
use serde_json::{Value, json};

/// The meeting's folder, from the repo root.
const MEETING_DIR: &str = "apps/desktop/src-tauri/resources/benchmark";
/// Device time of the replay's first frame (any value works).
const ORIGIN: DeviceTime = DeviceTime::from_ticks(10_000_000);
/// The echo added for `aec`: its delay (ms), within the canceller's search range.
const ECHO_DELAY_MS: usize = 120;
/// The echo added for `aec`: its gain.
const ECHO_GAIN: f32 = 0.3;
/// How long the event loop waits for an event.
const EVENT_WAIT: Duration = Duration::from_millis(100);
/// What the peak memory leaves out.
const MEMORY_SCOPE: &str = "system memory of this process; video memory is not included";

fn fail(msg: impl std::fmt::Display) -> ! {
    eprintln!("bench: {msg}");
    std::process::exit(1);
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Args {
    part: String,
    root: PathBuf,
    backend: BackendKind,
    models: PathBuf,
    scratch: PathBuf,
    model: Option<String>,
}

fn parse_backend(name: &str) -> Result<BackendKind, String> {
    match name {
        "cpu" => Ok(BackendKind::Cpu),
        "cuda" => Ok(BackendKind::Cuda),
        "vulkan" => Ok(BackendKind::Vulkan),
        "qnn" => Ok(BackendKind::Qnn),
        other => Err(format!("unsupported backend {other}")),
    }
}

fn parse_args(args: impl IntoIterator<Item = String>) -> Result<Args, String> {
    let mut it = args.into_iter();
    let part = it.next().ok_or("usage: bench asr|diar|aec ...")?;
    if !matches!(part.as_str(), "asr" | "diar" | "aec") {
        return Err(format!("unknown part {part}"));
    }
    let (mut root, mut backend, mut models, mut scratch, mut model) =
        (None, None, None, None, None);
    while let Some(flag) = it.next() {
        let value = it.next().ok_or(format!("{flag} needs a value"))?;
        match flag.as_str() {
            "--root" => root = Some(PathBuf::from(value)),
            "--backend" => backend = Some(parse_backend(&value)?),
            "--models" => models = Some(PathBuf::from(value)),
            "--scratch" => scratch = Some(PathBuf::from(value)),
            "--model" => model = Some(value),
            other => return Err(format!("unknown argument {other}")),
        }
    }
    let backend = backend.ok_or("--backend is required")?;
    if part != "asr" && backend != BackendKind::Cpu {
        return Err(format!("{part} runs on the CPU only"));
    }
    if part != "asr" && model.is_some() {
        return Err(format!("{part} has no --model"));
    }
    Ok(Args {
        part,
        root: root.ok_or("--root is required")?,
        backend,
        models: models.ok_or("--models is required")?,
        scratch: scratch.ok_or("--scratch is required")?,
        model,
    })
}

/// Nearest-rank percentile of a sorted list (`0 < p <= 1`).
fn percentile(sorted: &[f64], p: f64) -> Option<f64> {
    let last = sorted.len().checked_sub(1)?;
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::cast_precision_loss
    )]
    let rank = ((sorted.len() as f64 * p).ceil() as usize).saturating_sub(1);
    sorted.get(rank.min(last)).copied()
}

/// Count, p50, p95 and largest of `values`; the percentiles are null for none.
fn spread(values: &[f64]) -> Value {
    let mut s = values.to_vec();
    s.sort_by(f64::total_cmp);
    json!({
        "count": s.len(),
        "p50": percentile(&s, 0.5),
        "p95": percentile(&s, 0.95),
        "max": s.last(),
    })
}

fn cores() -> usize {
    std::thread::available_parallelism().map_or(1, std::num::NonZero::get)
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

/// Real-time factor of `decode` over `samples` of 16 kHz audio.
fn rtf(decode: Duration, samples: usize) -> f64 {
    decode.as_secs_f64() / PIPELINE_RATE.duration_of(samples).as_secs_f64()
}

fn read_track(root: &Path, name: &str) -> Vec<Sample> {
    let path = root.join(MEETING_DIR).join(name);
    let (rate, samples) =
        read_flac(&path).unwrap_or_else(|e| fail(format!("read {}: {e}", path.display())));
    if rate != PIPELINE_RATE.hz() {
        fail(format!("{} is not 16 kHz", path.display()));
    }
    samples
}

fn accelerator(kind: BackendKind) -> Accelerator {
    Accelerator::of(kind).unwrap_or_else(|| fail(format!("no accelerator for {kind:?}")))
}

/// `--model`, or the first rung of the model check's English ladder on `backend`.
fn asr_model(args: &Args) -> String {
    args.model.clone().unwrap_or_else(|| {
        model_check::candidates(args.backend, &[])
            .first()
            .map_or_else(
                || fail(format!("no English ladder for {:?}", args.backend)),
                |m| (*m).to_owned(),
            )
    })
}

/// The live half of `asr` (module docs).
#[allow(clippy::too_many_lines)]
fn asr_live(args: &Args, name: &str, mic: &[Sample], reference: &[Sample]) -> Value {
    let order = vec![accelerator(args.backend)];
    let models = ModelDir::new(&args.models);
    let factory = live_asr_factory(
        Arc::new(EvidenceLoader::for_live(2)),
        models,
        name.to_owned(),
        order,
        Accelerator::Cpu,
        Arc::new(SystemClock),
        os_seeded_rng(),
    );
    let scratch = Scratch::create(&args.scratch).unwrap_or_else(|e| fail(format!("scratch: {e}")));
    let data = DataDir::new(scratch.path());
    data.create().unwrap_or_else(|e| fail(format!("data: {e}")));
    let store = data
        .open_store()
        .unwrap_or_else(|e| fail(format!("store: {e}")));
    let svc = Services::new(store, data, Arc::new(SystemClock))
        .unwrap_or_else(|e| fail(format!("services: {e}")));
    let source = ReplayBuilder::new(ORIGIN, MonoBuffer::new(PIPELINE_RATE, mic.to_vec()))
        .reference(MonoBuffer::new(PIPELINE_RATE, reference.to_vec()))
        .build()
        .unwrap_or_else(|e| fail(format!("replay: {e:?}")));
    let clock: Arc<dyn PaceClock> = Arc::new(WallClock::new());
    let state = Arc::new(PaceState::default());
    let started = Arc::new(Mutex::new(None));
    let paced = {
        let (clock, started) = (Arc::clone(&clock), Arc::clone(&started));
        Paced::new(source, Arc::clone(&clock), true, Arc::clone(&state)).on_start(move || {
            *started.lock().unwrap_or_else(PoisonError::into_inner) = Some(clock.now());
        })
    };
    let mut session = start_live(
        paced,
        svc.clone(),
        LiveConfig::default(),
        Some(factory),
        os_seeded_rng(),
    )
    .unwrap_or_else(|e| fail(format!("start the session: {e}")));
    let events = session
        .take_events()
        .unwrap_or_else(|| fail("the session has no events"));
    let (mut latencies, mut lags, mut backends) = (Vec::new(), Vec::new(), Vec::new());
    let (mut words, mut stopped, mut catch_up) = (0usize, false, None);
    loop {
        match events.recv_timeout(EVENT_WAIT) {
            Ok(RuntimeEvent::Ended(_)) | Err(RecvTimeoutError::Disconnected) => break,
            Ok(RuntimeEvent::Transcript {
                event: TranscriptEvent::Final(s),
                ..
            }) => {
                let now = clock.now();
                let origin = started
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .unwrap_or_else(|| fail("a final before the replay started"));
                latencies.push(ms(chunk_latency(now, origin, s.end)));
                words += s.words.len();
            }
            Ok(RuntimeEvent::LiveBackend {
                track,
                backend,
                fallbacks,
            }) => {
                if backend.kind != args.backend || !fallbacks.is_empty() {
                    fail(format!(
                        "live transcription of {track:?} ran on {:?} (refused: {fallbacks:?})",
                        backend.kind
                    ));
                }
                backends.push(json!({ "track": format!("{track:?}"), "backend": backend }));
            }
            Ok(RuntimeEvent::TranscriptLag { behind, .. }) => lags.push(ms(behind)),
            Ok(RuntimeEvent::LiveTranscriptFinished { after_capture_end }) => {
                catch_up = Some(after_capture_end);
            }
            Ok(RuntimeEvent::Warning(
                RuntimeWarning::LiveCutAtStop { .. }
                | RuntimeWarning::Pipeline(PipelineError::LiveAsr { .. }),
            )) => stopped = true,
            Ok(_) | Err(RecvTimeoutError::Timeout) => {}
        }
    }
    session
        .wait()
        .unwrap_or_else(|e| fail(format!("the session failed: {e}")));
    if backends.len() != 2 {
        fail(format!("{} live tracks reported a backend", backends.len()));
    }
    if latencies.is_empty() {
        fail("the session produced no live finals");
    }
    let catch_up = catch_up.unwrap_or_else(|| fail("the live transcript did not finish"));
    json!({
        "chunk": "one live final (a speech stretch)",
        "latency_ms": spread(&latencies),
        "words": words,
        "transcript_lag_ms": spread(&lags),
        "catch_up_ms": ms(catch_up),
        "kept_up": !stopped && catch_up <= CATCH_UP_LIMIT,
        "largest_capture_lag_ms": ms(state.max_lag()),
        "backends": backends,
    })
}

/// How long after `end` of the meeting (delivered at `origin` plus `end`) the wall clock `now`
/// is. A final cannot come before its audio; zero if the clocks say it did.
fn chunk_latency(now: Duration, origin: Duration, end: MeetingTime) -> Duration {
    now.saturating_sub(origin + Duration::from_millis(end.as_millis()))
}

fn bench_asr(args: &Args) -> Value {
    let name = asr_model(args);
    let accel = accelerator(args.backend);
    let order = vec![accel];
    let models = ModelDir::new(&args.models);
    let library = onnx_library(&name, &order);
    init_onnx(&models, library).unwrap_or_else(|e| fail(format!("ONNX Runtime: {e}")));
    let (mic, reference) = (
        read_track(&args.root, "mic.flac"),
        read_track(&args.root, "reference.flac"),
    );
    eprintln!(
        "bench: asr: {name} on {:?}: the live session, in real time",
        args.backend
    );
    let live = asr_live(args, &name, &mic, &reference);
    eprintln!("bench: asr: the final pass's transcriber over both tracks");
    let load = Instant::now();
    let mut choice = final_pass_asr(&EvidenceLoader::default(), &models, &name, &order)
        .unwrap_or_else(|e| fail(format!("load {name}: {e}")));
    if choice.backend != args.backend || !choice.rejected.is_empty() {
        fail(format!(
            "{name} loaded on {:?} (refused {:?})",
            choice.backend, choice.rejected
        ));
    }
    let mut vad =
        load_silero(&models, Accelerator::Cpu).unwrap_or_else(|e| fail(format!("Silero: {e}")));
    let load_s = load.elapsed().as_secs_f64();
    let (mut decode, mut samples, mut pieces) = (Duration::ZERO, 0, 0);
    for audio in [&mic, &reference] {
        let buffer = MonoBuffer::new(PIPELINE_RATE, audio.clone());
        let start = Instant::now();
        let (t, report) = transcribe_in_pieces(
            &mut choice.transcriber,
            &mut vad,
            &buffer,
            &TranscribeOptions::live(),
            &PieceConfig::default(),
            &CancelToken::new(),
        )
        .unwrap_or_else(|e| fail(format!("decode: {e}")));
        decode += start.elapsed();
        samples += audio.len();
        pieces += report.pieces;
        if t.backend.kind != args.backend {
            fail(format!("the decode ran on {:?}", t.backend.kind));
        }
    }
    json!({
        "model": name,
        "backend": choice.transcriber.backend_info(),
        "evidence": choice.transcriber.backend_evidence(),
        "audio_s": PIPELINE_RATE.duration_of(samples).as_secs_f64(),
        "rtf": rtf(decode, samples),
        "load_s": load_s,
        "pieces": pieces,
        "live": live,
    })
}

fn bench_diar(args: &Args) -> Value {
    let models = ModelDir::new(&args.models);
    init_onnx(&models, Accelerator::Cpu).unwrap_or_else(|e| fail(format!("ONNX Runtime: {e}")));
    let load = Instant::now();
    let (segmenter, embedder) = load_diarisation(&models, Accelerator::Cpu)
        .unwrap_or_else(|e| fail(format!("speaker models: {e}")));
    let load_s = load.elapsed().as_secs_f64();
    let placement = json!({
        "segmentation": segmenter.backend().to_string(),
        "embedding": embedder.backend().to_string(),
    });
    let model_id = embedder.info().model.clone();
    let mut config = PostConfig::default();
    config.diarization.clustering.threshold = config
        .clustering_threshold(&model_id)
        .unwrap_or_else(|e| fail(format!("clustering threshold: {e}")));
    let mut diarizer = Diarizer::new(segmenter, embedder, config.diarization)
        .unwrap_or_else(|e| fail(format!("diarizer: {e:?}")));
    let cancel = CancelToken::new();
    let (mut batches, mut total, mut samples, mut speakers) =
        (Vec::new(), Duration::ZERO, 0, Vec::new());
    for name in ["mic.flac", "reference.flac"] {
        let audio = read_track(&args.root, name);
        eprintln!("bench: diar: {name}");
        let start = Instant::now();
        let mut stream = diarizer.stream();
        stream.append(&audio);
        while stream.ready_batches() > 0 {
            let batch = Instant::now();
            diarizer
                .advance(&mut stream, 1, &cancel)
                .unwrap_or_else(|e| fail(format!("advance: {e:?}")));
            batches.push(ms(batch.elapsed()));
        }
        let results: WindowResults = stream.into_results();
        let buffer = MonoBuffer::new(PIPELINE_RATE, audio);
        let d = diarizer
            .diarize_from(results, &buffer, MeetingTime::ZERO, &cancel)
            .unwrap_or_else(|e| fail(format!("diarize: {e:?}")));
        total += start.elapsed();
        samples += buffer.samples.len();
        speakers.push(d.speakers.len());
    }
    json!({
        "model": model_id,
        "backend": { "kind": "cpu" },
        "evidence": placement,
        "threads": speaker_threads(cores()),
        "audio_s": PIPELINE_RATE.duration_of(samples).as_secs_f64(),
        "rtf": rtf(total, samples),
        "load_s": load_s,
        "speakers_per_track": speakers,
        "live": {
            "chunk": "one batch of speaker windows",
            "latency_ms": spread(&batches),
        },
    })
}

/// The mic with the reference's echo added (module docs), both cut to whole blocks.
fn with_echo(mic: &[Sample], reference: &[Sample]) -> Vec<Sample> {
    let delay = ECHO_DELAY_MS * PIPELINE_RATE.hz() as usize / 1000;
    mic.iter()
        .enumerate()
        .map(|(i, &m)| {
            let echo = i
                .checked_sub(delay)
                .and_then(|j| reference.get(j))
                .copied()
                .unwrap_or(0.0);
            m + ECHO_GAIN * echo
        })
        .collect()
}

fn bench_aec(args: &Args) -> Value {
    let (mic, reference) = (
        read_track(&args.root, "mic.flac"),
        read_track(&args.root, "reference.flac"),
    );
    let mic = with_echo(&mic, &reference);
    let mut canceller = EchoCanceller::new(EchoCancellerConfig::default());
    let mut out = vec![0.0; BLOCK];
    let mut latencies = Vec::with_capacity(mic.len() / BLOCK);
    let (mut total, mut converged, mut samples) = (Duration::ZERO, 0usize, 0usize);
    for (m, r) in mic
        .as_chunks::<BLOCK>()
        .0
        .iter()
        .zip(reference.as_chunks::<BLOCK>().0)
    {
        let start = Instant::now();
        canceller.process(m, r, &mut out);
        let took = start.elapsed();
        std::hint::black_box(&out);
        total += took;
        samples += BLOCK;
        latencies.push(ms(took));
        converged += usize::from(canceller.status().converged);
    }
    json!({
        "backend": { "kind": "cpu" },
        "threads": 1,
        "audio_s": PIPELINE_RATE.duration_of(samples).as_secs_f64(),
        "rtf": rtf(total, samples),
        "converged_blocks": converged,
        "live": {
            "chunk": "one 8 ms block",
            "latency_ms": spread(&latencies),
        },
    })
}

fn main() {
    let args = parse_args(std::env::args().skip(1)).unwrap_or_else(|e| fail(e));
    let mut report = match args.part.as_str() {
        "asr" => bench_asr(&args),
        "diar" => bench_diar(&args),
        _ => bench_aec(&args),
    };
    let peak = earshot_proc::peak_memory();
    report["peak_memory_mb"] = json!({
        "working_set": peak.map(|p| p.working_set / (1024 * 1024)),
        "committed": peak.map(|p| p.committed / (1024 * 1024)),
        "scope": MEMORY_SCOPE,
    });
    report["cores"] = json!(cores());
    println!("{report}");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(s: &str) -> Vec<String> {
        s.split(' ').map(str::to_owned).collect()
    }

    #[test]
    fn bench_args_read_every_flag() {
        let a = parse_args(strings(
            "asr --root r --backend vulkan --models m --scratch s --model base.en",
        ))
        .unwrap();
        assert_eq!(a.part, "asr");
        assert_eq!(a.backend, BackendKind::Vulkan);
        assert_eq!(a.model.as_deref(), Some("base.en"));
    }

    #[test]
    fn bench_diar_and_aec_refuse_a_backend_production_never_runs_them_on() {
        for part in ["diar", "aec"] {
            let line = format!("{part} --root r --backend vulkan --models m --scratch s");
            assert!(parse_args(strings(&line)).is_err(), "{part}");
            let line = format!("{part} --root r --backend cpu --models m --scratch s");
            assert!(parse_args(strings(&line)).is_ok(), "{part}");
            let line = format!("{part} --root r --backend cpu --models m --scratch s --model x");
            assert!(parse_args(strings(&line)).is_err(), "{part}");
        }
        assert!(parse_args(strings("nope --root r")).is_err());
    }

    #[test]
    fn bench_percentile_is_nearest_rank() {
        let ten: Vec<f64> = (1..=10).map(f64::from).collect();
        assert_eq!(percentile(&ten, 0.5), Some(5.0));
        assert_eq!(percentile(&ten, 0.95), Some(10.0));
        assert_eq!(percentile(&[3.0], 0.95), Some(3.0));
        assert_eq!(percentile(&[], 0.5), None);
        let s = spread(&[]);
        assert_eq!(s["count"], 0);
        assert!(s["p50"].is_null());
        let s = spread(&[2.0, 1.0]);
        assert_eq!(s["p50"], 1.0);
        assert_eq!(s["max"], 2.0);
    }

    #[test]
    fn bench_chunk_latency_counts_from_when_its_end_was_delivered() {
        let origin = Duration::from_secs(5);
        let end = MeetingTime::from_millis(2_000);
        assert_eq!(
            chunk_latency(Duration::from_millis(7_500), origin, end),
            Duration::from_millis(500)
        );
        assert_eq!(
            chunk_latency(Duration::from_secs(6), origin, end),
            Duration::ZERO
        );
    }

    #[test]
    fn bench_echo_is_the_reference_delayed() {
        let delay = ECHO_DELAY_MS * 16;
        let mut reference = vec![0.0; delay + 3];
        reference[0] = 1.0;
        let mic = vec![0.0; delay + 3];
        let out = with_echo(&mic, &reference);
        assert_eq!(out[0], 0.0);
        assert!((out[delay] - ECHO_GAIN).abs() < 1e-6);
    }

    #[test]
    fn bench_rtf_is_decode_over_audio() {
        assert!((rtf(Duration::from_secs(1), 32_000) - 0.5).abs() < 1e-9);
    }
}
