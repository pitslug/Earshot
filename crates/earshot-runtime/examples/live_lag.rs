//! How long words take from being spoken to reaching the screen (T9.1m): a real-time replay of
//! public audio through a production live session, reporting counts and times only, never text.
//!
//! ```text
//! cargo run --release -p earshot-runtime --example live_lag -- <repo-root>
//!     [--audio bundled|continuous] [--backend cpu|cuda] [--models <dir>] [--speakers on|off]
//!     [--seconds <n>] [--previews off|on|everyone] [--window <s>] [--interval <s>] [--out <file.json>]
//! cargo run --release -p earshot-runtime --example live_lag -- <repo-root> --memory
//!     [--backend cpu|cuda] [--out <file.json>]
//! ```
//!
//! Audio (public only): `bundled` is the Settings benchmark's meeting (AMI ES2004a, two tracks);
//! `continuous` is `fixtures/smoke` joined with 200 ms gaps on the reference track, the mic
//! silent: one person who never pauses, the worst case for a stretch.
//!
//! Setup: `host::live_factory` with the production default model of `--backend`, `LiveConfig`'s
//! defaults (speech stretches) and the real-time pacing of the Settings benchmark
//! (`replay::Paced`). A word "reaches the UI" when its `RuntimeEvent::Transcript` final reaches
//! this host loop; the desktop adds only the mapping to `MeetingEvent::Landed` and the webview.
//! The event's wall time since the first pump, minus the word's end in meeting time, is its lag.
//! Stretches are timed by a transcriber wrapper, so the report separates the wait for a stretch to
//! end from the decode.
//!
//! With previews the report adds, per word that settled, when it first reached the screen in either
//! form (`on_screen_lag_s`), each preview word's own lag, how many preview words differed from the
//! settled text they became (`changed`) and how many left before settled text reached them
//! (`vanished`). Two runs store the same words when their `stored_sha256` match.
//!
//! The report names the backend the session announced and the host's load: the percentage of the
//! whole machine in use, read about every 30 s, and this process's own CPU time and memory. A
//! number measured with a loaded host is reported as such, never as an idle-host result.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use earshot_core::asr::{
    AsrError, BackendInfo, BackendKind, CancelToken, TranscribeOptions, Transcriber,
    TranscriptEvent, Transcription,
};
use earshot_core::audio::{MonoBuffer, PIPELINE_RATE};
use earshot_core::capture::{DeviceTime, ReplayBuilder, TrackId};
use earshot_core::model::ClockDomain;
use earshot_runtime::backends::{Accelerator, ModelDir, init_onnx, onnx_library};
use earshot_runtime::live::{LiveAsrFactory, LiveConfig, RuntimeEvent, start_live};
use earshot_runtime::preview_settings::PreviewChoice;
use earshot_runtime::replay::{PaceClock, PaceState, Paced, WallClock};
use earshot_runtime::{Services, host, model_check, os_seeded_rng};
use earshot_store::DataDir;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

/// Device time of the replay's first frame (any value works).
const ORIGIN: DeviceTime = DeviceTime::from_ticks(10_000_000);
/// How often the host's load is read.
const LOAD_EVERY: Duration = Duration::from_secs(30);

fn fail(message: impl std::fmt::Display) -> ! {
    eprintln!("live_lag: {message}");
    std::process::exit(1);
}

struct Args {
    repo: PathBuf,
    audio: String,
    backend: BackendKind,
    models: Option<PathBuf>,
    speakers: bool,
    seconds: u64,
    out: Option<PathBuf>,
    /// Measure what more sessions of the model cost instead of replaying (`memory_report`).
    memory: bool,
    /// Run previews (production's, `host::live_previews`) with this choice: `on` the other people
    /// only, `everyone` also the user's own track in a call (T9.14); `None` (`off`) none.
    previews: Option<PreviewChoice>,
    /// Their window and interval, when not the starting values.
    window: Option<f64>,
    interval: Option<f64>,
}

fn parse_args() -> Args {
    let mut raw = std::env::args().skip(1);
    let repo = raw
        .next()
        .unwrap_or_else(|| fail("usage: live_lag <repo-root> [options]"));
    let mut args = Args {
        repo: PathBuf::from(repo),
        audio: "bundled".to_owned(),
        backend: BackendKind::Cpu,
        models: None,
        speakers: true,
        seconds: 120,
        out: None,
        memory: false,
        previews: None,
        window: None,
        interval: None,
    };
    while let Some(flag) = raw.next() {
        let mut value = || {
            raw.next()
                .unwrap_or_else(|| fail(format!("{flag} needs a value")))
        };
        match flag.as_str() {
            "--audio" => args.audio = value(),
            "--backend" => {
                args.backend = match value().as_str() {
                    "cpu" => BackendKind::Cpu,
                    "cuda" => BackendKind::Cuda,
                    other => fail(format!("unknown backend {other}")),
                }
            }
            "--models" => args.models = Some(PathBuf::from(value())),
            "--speakers" => args.speakers = value() == "on",
            "--seconds" => {
                args.seconds = value()
                    .parse()
                    .unwrap_or_else(|e| fail(format!("--seconds: {e}")));
            }
            "--out" => args.out = Some(PathBuf::from(value())),
            "--memory" => args.memory = true,
            "--previews" => {
                args.previews = match value().as_str() {
                    "off" => None,
                    "on" => Some(PreviewChoice::Automatic),
                    "everyone" => Some(PreviewChoice::Everyone),
                    other => fail(format!("unknown --previews {other}")),
                }
            }
            "--window" => args.window = value().parse().ok(),
            "--interval" => args.interval = value().parse().ok(),
            other => fail(format!("unknown flag {other}")),
        }
    }
    args
}

/// A 16 kHz mono FLAC file's samples.
fn read_flac(path: &Path) -> Vec<f32> {
    let mut reader =
        claxon::FlacReader::open(path).unwrap_or_else(|e| fail(format!("{}: {e}", path.display())));
    let info = reader.streaminfo();
    if info.sample_rate != PIPELINE_RATE.hz() || info.channels != 1 || info.bits_per_sample != 16 {
        fail(format!("{}: not 16 kHz mono 16-bit", path.display()));
    }
    reader
        .samples()
        .map(|s| {
            let v = s.unwrap_or_else(|e| fail(format!("{}: {e}", path.display())));
            i16::try_from(v).map_or(0.0, earshot_core::audio::i16_to_f32)
        })
        .collect()
}

/// The two tracks of `args.audio` (module docs).
fn load_audio(args: &Args) -> (Vec<f32>, Vec<f32>) {
    match args.audio.as_str() {
        "bundled" => {
            let dir = args.repo.join("apps/desktop/src-tauri/resources/benchmark");
            let mut mic = read_flac(&dir.join("mic.flac"));
            let mut reference = read_flac(&dir.join("reference.flac"));
            let n = mic.len().max(reference.len());
            mic.resize(n, 0.0);
            reference.resize(n, 0.0);
            (mic, reference)
        }
        "continuous" => {
            let dir = args.repo.join("fixtures/smoke");
            let mut files: Vec<PathBuf> = std::fs::read_dir(&dir)
                .unwrap_or_else(|e| fail(format!("{}: {e}", dir.display())))
                .filter_map(|e| e.ok().map(|e| e.path()))
                .filter(|p| p.extension().is_some_and(|x| x == "flac"))
                .collect();
            files.sort();
            let want = PIPELINE_RATE.frames_for(Duration::from_secs(args.seconds));
            let gap = PIPELINE_RATE.frames_for(Duration::from_millis(200));
            let mut reference = Vec::with_capacity(want);
            for file in files.iter().cycle() {
                if reference.len() >= want {
                    break;
                }
                reference.extend(read_flac(file));
                reference.resize(reference.len() + gap, 0.0);
            }
            reference.truncate(want);
            (vec![0.0; reference.len()], reference)
        }
        other => fail(format!("unknown audio {other}")),
    }
}

/// `sorted`'s value at fraction `p` (nearest rank); `None` when empty.
fn percentile(sorted: &[f64], p: f64) -> Option<f64> {
    if sorted.is_empty() {
        return None;
    }
    let at = ((sorted.len() - 1) as f64 * p).round() as usize;
    Some(sorted[at.min(sorted.len() - 1)])
}

/// Median, 95th percentile and largest of `values` (seconds), and how many.
fn spread(values: &[f64]) -> Value {
    let mut sorted = values.to_vec();
    sorted.sort_by(f64::total_cmp);
    json!({
        "n": sorted.len(),
        "median": percentile(&sorted, 0.5),
        "p95": percentile(&sorted, 0.95),
        "max": sorted.last(),
    })
}

/// One decode call's times.
#[derive(Debug, Clone, Copy)]
struct Decode {
    audio_s: f64,
    took_s: f64,
}

/// Times every decode of a track; otherwise the track's own transcriber.
struct Timed {
    inner: Box<dyn Transcriber + Send>,
    log: Arc<Mutex<Vec<Decode>>>,
}

impl Transcriber for Timed {
    fn transcribe(
        &mut self,
        audio: &MonoBuffer,
        options: &TranscribeOptions,
        cancel: &CancelToken,
    ) -> Result<Transcription, AsrError> {
        let started = Instant::now();
        let result = self.inner.transcribe(audio, options, cancel);
        self.log
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(Decode {
                audio_s: PIPELINE_RATE.duration_of(audio.samples.len()).as_secs_f64(),
                took_s: started.elapsed().as_secs_f64(),
            });
        result
    }

    fn backend_info(&self) -> BackendInfo {
        self.inner.backend_info()
    }

    fn may_skip_speech(&self) -> bool {
        self.inner.may_skip_speech()
    }
}

/// `factory` with every track's transcriber timed into `logs`.
fn timed(
    mut factory: LiveAsrFactory,
    logs: [Arc<Mutex<Vec<Decode>>>; 2],
    evidence: Arc<Mutex<Vec<String>>>,
) -> LiveAsrFactory {
    Box::new(move |track, entity| {
        let Some(mut asr) = factory(track, entity)? else {
            return Ok(None);
        };
        if let Some(e) = &asr.evidence {
            evidence
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(format!("{track:?}: {e}"));
        }
        asr.asr.transcriber = Box::new(Timed {
            inner: asr.asr.transcriber,
            log: Arc::clone(&logs[index(track)]),
        });
        Ok(Some(asr))
    })
}

/// `words` as (normalised text, start ms, end ms): held in memory only, to compare; never printed.
fn named_words(words: &[earshot_core::asr::Word]) -> Vec<(String, u64, u64)> {
    words
        .iter()
        .map(|w| {
            (
                earshot_core::asr::normalize_token(&w.text),
                w.start.as_millis(),
                w.end.as_millis(),
            )
        })
        .collect()
}

fn index(track: TrackId) -> usize {
    match track {
        TrackId::Mic => 0,
        TrackId::Reference => 1,
    }
}

/// The whole machine's CPU use (percent) and this process's CPU seconds, private bytes and
/// working set, read through PowerShell (a child, about once per [`LOAD_EVERY`]).
fn host_reading() -> Option<(f64, f64, f64, f64)> {
    let pid = std::process::id();
    let script = format!(
        "$p=Get-Process -Id {pid};\
         $l=(Get-CimInstance Win32_Processor | Measure-Object LoadPercentage -Average).Average;\
         \"$l $($p.CPU) $($p.PrivateMemorySize64) $($p.PeakWorkingSet64)\""
    );
    let out = Command::new("powershell")
        .args(["-NoProfile", "-NonInteractive", "-Command", &script])
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    let mut parts = text.split_whitespace().map(str::parse::<f64>);
    let load = parts.next()?.ok()?;
    let cpu = parts.next()?.ok()?;
    let private = parts.next()?.ok()?;
    let peak = parts.next()?.ok()?;
    Some((load, cpu, private, peak))
}

/// What one track's previews did, compared with the finals that replaced them.
#[derive(Debug, Default)]
struct PreviewLags {
    /// The words on screen now: (normalised text, start ms, seconds after its end it appeared).
    shown: Vec<(String, u64, f64)>,
    /// Per preview word: the lag when it appeared.
    word_s: Vec<f64>,
    /// Preview words replaced by settled text holding the same word near the same time.
    kept: u64,
    /// Preview words the settled text said otherwise.
    changed: u64,
    /// Preview words that left with no settled text at them.
    vanished: u64,
    /// Sends.
    updates: u64,
}

impl PreviewLags {
    fn update(&mut self, at: Duration, words: &[(String, u64, u64)]) {
        self.updates += 1;
        let now = at.as_secs_f64();
        let mut next = Vec::with_capacity(words.len());
        for (text, start, end) in words {
            let known = self.shown.iter().find(|(t, s, _)| t == text && s == start);
            if let Some(word) = known {
                next.push(word.clone());
            } else {
                let lag = now - *end as f64 / 1000.0;
                self.word_s.push(lag);
                next.push((text.clone(), *start, lag));
            }
        }
        let gone = self
            .shown
            .iter()
            .filter(|(t, s, _)| !words.iter().any(|(wt, ws, _)| wt == t && ws == s))
            .count();
        // What a final took away has been counted by `settled`.
        self.vanished += gone as u64;
        self.shown = next;
    }

    /// A final holding `words` (normalised text, start, end): the preview words it covers leave.
    /// Returns, per word of the final, the lag of the preview word it replaced.
    fn settled(&mut self, words: &[(String, u64, u64)]) -> Vec<Option<f64>> {
        let Some(end) = words.last().map(|w| w.2) else {
            return Vec::new();
        };
        let mut matched = vec![None; words.len()];
        let mut left = Vec::new();
        for (text, start, lag) in self.shown.drain(..) {
            // The runtime's rule for what settled text covers (`preview::COVERED_SLACK`).
            if start + 100 >= end {
                left.push((text, start, lag));
                continue;
            }
            let found = words.iter().enumerate().position(|(i, (t, s, _))| {
                matched[i].is_none() && *t == text && s.abs_diff(start) <= 1000
            });
            if let Some(i) = found {
                matched[i] = Some(lag);
                self.kept += 1;
            } else {
                self.changed += 1;
            }
        }
        self.shown = left;
        matched
    }
}

/// What one track's finals took.
#[derive(Debug, Default)]
struct TrackLags {
    /// Per word: wall time on reaching this loop, minus the word's end.
    word_s: Vec<f64>,
    /// Per stretch: the same for its first word and its last.
    first_word_s: Vec<f64>,
    last_word_s: Vec<f64>,
    /// Per stretch: from its first word's start to its last word's end.
    stretch_s: Vec<f64>,
    /// Per word: from its end to the first time it reached the screen, provisional or settled.
    on_screen_s: Vec<f64>,
    /// Hashes every stored word's text and times, in order.
    stored: Sha256,
    previews: PreviewLags,
}

impl TrackLags {
    fn add(&mut self, at: Duration, words: &[(u64, u64)]) {
        let (Some(first), Some(last)) = (words.first(), words.last()) else {
            return;
        };
        let lag = |end_ms: u64| at.as_secs_f64() - end_ms as f64 / 1000.0;
        self.word_s.extend(words.iter().map(|(_, e)| lag(*e)));
        self.first_word_s.push(lag(first.1));
        self.last_word_s.push(lag(last.1));
        self.stretch_s
            .push(last.1.saturating_sub(first.0) as f64 / 1000.0);
    }

    fn json(&self) -> Value {
        json!({
            "stretches": self.first_word_s.len(),
            "words": self.word_s.len(),
            "word_lag_s": spread(&self.word_s),
            "first_word_lag_s": spread(&self.first_word_s),
            "last_word_lag_s": spread(&self.last_word_s),
            "stretch_length_s": spread(&self.stretch_s),
            "on_screen_lag_s": spread(&self.on_screen_s),
            "stored_sha256": self.stored.clone().finalize().iter().map(|b| format!("{b:02x}")).collect::<String>(),
            "previews": {
                "updates": self.previews.updates,
                "word_lag_s": spread(&self.previews.word_s),
                "kept": self.previews.kept,
                "changed": self.previews.changed,
                "vanished": self.previews.vanished,
            },
        })
    }
}

/// What the model costs per session, and decoding a preview window: this process's memory after
/// each load and after three decodes on it, and each decode's time at two window lengths and at
/// lengths not seen before (ONNX Runtime may set up per new input length). Numbers only.
fn memory_report(models: &ModelDir, model: &str, accel: Accelerator, audio: &[f32]) -> Value {
    let earshot_runtime::backends::AsrModel::Parakeet(parakeet) =
        earshot_runtime::backends::asr_model(model).unwrap_or_else(|e| fail(e))
    else {
        fail("the memory report is for Parakeet")
    };
    let threads = earshot_runtime::backends::onnx_cpu_threads(
        std::thread::available_parallelism().map_or(1, std::num::NonZero::get),
        2,
    );
    let gib = |v: f64| v / 1024.0 / 1024.0 / 1024.0;
    let memory = || {
        host_reading().map(|r| json!({ "private_gib": gib(r.2), "peak_working_set_gib": gib(r.3) }))
    };
    let options = TranscribeOptions::live();
    let cancel = CancelToken::new();
    let frames = |seconds: f64| (seconds * f64::from(PIPELINE_RATE.hz())) as usize;
    let decode = |t: &mut dyn Transcriber, start_s: f64, length_s: f64| -> f64 {
        let start = frames(start_s);
        let buffer = MonoBuffer::new(
            PIPELINE_RATE,
            audio[start..start + frames(length_s)].to_vec(),
        );
        let began = Instant::now();
        t.transcribe(&buffer, &options, &cancel)
            .unwrap_or_else(|e| fail(e));
        began.elapsed().as_secs_f64()
    };
    let mut stages = vec![json!({ "stage": "no session", "memory": memory() })];
    let mut sessions = Vec::new();
    for n in 1..=3 {
        let began = Instant::now();
        let mut session =
            earshot_runtime::backends::load_parakeet(models, parakeet, accel, Some(threads))
                .unwrap_or_else(|e| fail(e));
        let load_s = began.elapsed().as_secs_f64();
        stages.push(
            json!({ "stage": format!("session {n} loaded"), "load_s": load_s, "memory": memory() }),
        );
        let took: Vec<f64> = (0..3).map(|_| decode(&mut session, 20.0, 12.0)).collect();
        stages.push(json!({ "stage": format!("session {n}, 3 decodes of 12 s"), "took_s": took, "memory": memory() }));
        sessions.push(session);
    }
    let session = sessions.first_mut().unwrap_or_else(|| fail("no session"));
    let mut lengths = Vec::new();
    for length in [3.0, 6.0, 9.0, 12.0, 15.0] {
        let took: Vec<f64> = (0..3).map(|_| decode(session, 20.0, length)).collect();
        lengths.push(json!({ "length_s": length, "took_s": took }));
    }
    // Lengths this session has not seen, each decoded twice: a cost per new length shows as a
    // first decode slower than the second.
    let mut new_lengths = Vec::new();
    for k in 0..6 {
        let length = 11.0 + 0.27 * f64::from(k);
        let first = decode(session, 20.0, length);
        let second = decode(session, 20.0, length);
        new_lengths.push(json!({ "length_s": length, "first_s": first, "second_s": second }));
    }
    json!({
        "model": model,
        "threads": threads,
        "stages": stages,
        "window_lengths": lengths,
        "new_lengths": new_lengths,
        "machine_load_percent": host_reading().map(|r| r.0),
    })
}

fn main() {
    let args = parse_args();
    let models = args.models.clone().map_or_else(
        || {
            let root = std::env::var_os("EARSHOT_MODELS")
                .filter(|v| !v.is_empty())
                .map(PathBuf::from)
                .or_else(|| {
                    std::env::var_os("LOCALAPPDATA")
                        .map(|d| PathBuf::from(d).join("Earshot").join("models"))
                })
                .unwrap_or_else(|| fail("no models folder"));
            ModelDir::new(root)
        },
        ModelDir::new,
    );
    let (mic, reference) = load_audio(&args);
    let length = PIPELINE_RATE.duration_of(mic.len());

    let model = model_check::candidates(args.backend, &[])
        .first()
        .copied()
        .unwrap_or_else(|| fail("no default model for the backend"));
    let accel =
        Accelerator::of(args.backend).unwrap_or_else(|| fail("no accelerator for the backend"));
    let order = vec![accel];
    // The shipped x64 build's CUDA: Parakeet from the optional download's folder.
    if args.backend == BackendKind::Cuda {
        earshot_runtime::cuda::use_for_transcription(&models, true)
            .unwrap_or_else(|e| fail(format!("CUDA: {e}")));
    }
    init_onnx(&models, onnx_library(model, &order))
        .unwrap_or_else(|e| fail(format!("ONNX Runtime: {e}")));

    if args.memory {
        let report = memory_report(&models, model, accel, &reference);
        let text = serde_json::to_string_pretty(&report).unwrap_or_else(|e| fail(e));
        println!("{text}");
        if let Some(out) = &args.out {
            std::fs::write(out, &text).unwrap_or_else(|e| fail(e));
        }
        return;
    }

    let scratch = args
        .repo
        .join("target")
        .join("live-lag")
        .join(format!("run-{}", std::process::id()));
    let data = DataDir::new(&scratch);
    data.create().unwrap_or_else(|e| fail(e));
    let store = data.open_store().unwrap_or_else(|e| fail(e));
    let clock: earshot_runtime::SharedClock = Arc::new(earshot_runtime::SystemClock);
    let services = Services::new(store, data, Arc::clone(&clock)).unwrap_or_else(|e| fail(e));

    let logs = [
        Arc::new(Mutex::new(Vec::new())),
        Arc::new(Mutex::new(Vec::new())),
    ];
    let evidence = Arc::new(Mutex::new(Vec::new()));
    let factory = timed(
        host::live_factory(&models, model, &order, clock, args.speakers),
        logs.clone(),
        Arc::clone(&evidence),
    );

    let source = ReplayBuilder::new(ORIGIN, MonoBuffer::new(PIPELINE_RATE, mic))
        .reference(MonoBuffer::new(PIPELINE_RATE, reference))
        .build()
        .unwrap_or_else(|e| fail(format!("replay: {e:?}")));
    let pace: Arc<dyn PaceClock> = Arc::new(WallClock::new());
    let state = Arc::new(PaceState::default());
    let started: Arc<Mutex<Option<Instant>>> = Arc::new(Mutex::new(None));
    let start_flag = Arc::clone(&started);
    let paced = Paced::new(source, pace, true, Arc::clone(&state)).on_start(move || {
        *start_flag.lock().unwrap_or_else(PoisonError::into_inner) = Some(Instant::now());
    });
    let previews = args.previews.map(|choice| {
        let mut p = host::live_previews(&models, choice);
        if let Some(s) = args.window {
            p.window = Duration::from_secs_f64(s);
        }
        if let Some(s) = args.interval {
            p.interval = Duration::from_secs_f64(s);
        }
        p
    });
    let config = LiveConfig {
        title: "live_lag".into(),
        clock_domain: ClockDomain::Replay,
        previews,
        ..LiveConfig::default()
    };
    let mut session = start_live(paced, services, config, Some(factory), os_seeded_rng())
        .unwrap_or_else(|e| fail(format!("session: {e}")));
    let events = session
        .take_events()
        .unwrap_or_else(|| fail("no event stream"));

    let mut lags = [TrackLags::default(), TrackLags::default()];
    let mut backends = Vec::new();
    let mut problems = 0usize;
    let mut catch_up = None;
    let mut preview_reports = Vec::new();
    let mut loads = Vec::new();
    let mut last = host_reading();
    let first_cpu = last.map(|r| r.1);
    let mut next_load = Instant::now() + LOAD_EVERY;
    let mut peak_private = 0.0f64;
    let mut peak_ws = 0.0f64;
    loop {
        if Instant::now() >= next_load {
            next_load = Instant::now() + LOAD_EVERY;
            if let Some(r) = host_reading() {
                loads.push(r.0);
                peak_private = peak_private.max(r.2);
                peak_ws = peak_ws.max(r.3);
                last = Some(r);
            }
        }
        match events.recv_timeout(Duration::from_millis(200)) {
            Ok(RuntimeEvent::Ended(_)) => break,
            Ok(RuntimeEvent::LiveBackend { track, backend, .. }) => {
                backends.push(json!({
                    "track": format!("{track:?}"),
                    "kind": format!("{:?}", backend.kind),
                    "device": backend.device,
                    "model": backend.model,
                }));
            }
            Ok(RuntimeEvent::Transcript {
                track,
                event: TranscriptEvent::Final(segment),
            }) => {
                let at = started
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .map_or(Duration::ZERO, |s| s.elapsed());
                let words: Vec<(u64, u64)> = segment
                    .words
                    .iter()
                    .map(|w| (w.start.as_millis(), w.end.as_millis()))
                    .collect();
                let lag = &mut lags[index(track)];
                lag.add(at, &words);
                let named = named_words(&segment.words);
                for (text, start, end) in &named {
                    lag.stored.update(text.as_bytes());
                    lag.stored.update(start.to_le_bytes());
                    lag.stored.update(end.to_le_bytes());
                }
                let shown = lag.previews.settled(&named);
                for ((_, _, end), preview) in named.iter().zip(shown) {
                    let settled = at.as_secs_f64() - *end as f64 / 1000.0;
                    lag.on_screen_s
                        .push(preview.map_or(settled, |p| p.min(settled)));
                }
            }
            Ok(RuntimeEvent::LivePreview { track, words }) => {
                let at = started
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .map_or(Duration::ZERO, |s| s.elapsed());
                lags[index(track)].previews.update(at, &named_words(&words));
            }
            Ok(RuntimeEvent::LivePreviewReport(r)) => {
                preview_reports.push(json!({
                    "track": r.track.map(|t| format!("{t:?}")),
                    "decodes": r.decodes,
                    "audio_s": r.audio_s,
                    "decode_s": r.decode_s,
                    "slowest_s": r.slowest_s,
                    "rtf": (r.audio_s > 0.0).then(|| r.decode_s / r.audio_s),
                    "skipped_busy": r.skipped_busy,
                    "cancelled": r.cancelled,
                    "slow_pauses": r.slow_pauses,
                    "audio_dropped": r.audio_dropped,
                    "words_shown": r.words_shown,
                    "decodes_adding_nothing": r.decodes_adding_nothing,
                    "load_s": r.load_s,
                    "off": r.off,
                }));
            }
            Ok(RuntimeEvent::LiveTranscriptFinished { after_capture_end }) => {
                catch_up = Some(after_capture_end.as_secs_f64());
            }
            Ok(RuntimeEvent::Warning(w)) => {
                problems += 1;
                // The kind only; a warning here never holds transcript text.
                eprintln!(
                    "live_lag: warning {}",
                    format!("{w:?}").split(['(', ' ']).next().unwrap_or("?")
                );
            }
            Ok(_) | Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    let outcome = session
        .wait()
        .unwrap_or_else(|e| fail(format!("session: {e}")));
    let end_reading = host_reading().or(last);
    if let Some(r) = end_reading {
        peak_private = peak_private.max(r.2);
        peak_ws = peak_ws.max(r.3);
    }

    let decodes = |log: &Arc<Mutex<Vec<Decode>>>| {
        let log = log.lock().unwrap_or_else(PoisonError::into_inner);
        let took: Vec<f64> = log.iter().map(|d| d.took_s).collect();
        let audio: f64 = log.iter().map(|d| d.audio_s).sum();
        let total: f64 = took.iter().sum();
        json!({
            "calls": log.len(),
            "audio_s": audio,
            "took_s": spread(&took),
            "rtf": (audio > 0.0).then(|| total / audio),
        })
    };
    let report = json!({
        "audio": args.audio,
        "length_s": length.as_secs_f64(),
        "requested_backend": format!("{:?}", args.backend),
        "live_backends": backends,
        "onnx_placement": *evidence.lock().unwrap_or_else(PoisonError::into_inner),
        "speakers": args.speakers,
        "warnings": problems,
        "stopped_with": format!("{:?}", outcome.reason),
        "capture_max_lag_s": state.max_lag().as_secs_f64(),
        "catch_up_after_end_s": catch_up,
        "previews": preview_reports,
        "mic": lags[0].json(),
        "reference": lags[1].json(),
        "decodes_mic": decodes(&logs[0]),
        "decodes_reference": decodes(&logs[1]),
        "host": {
            "machine_load_percent": spread(&loads),
            "process_cpu_s": match (first_cpu, end_reading) {
                (Some(a), Some(b)) => Some(b.1 - a),
                _ => None,
            },
            "peak_private_gib": peak_private / 1024.0 / 1024.0 / 1024.0,
            "peak_working_set_gib": peak_ws / 1024.0 / 1024.0 / 1024.0,
            "logical_cores": std::thread::available_parallelism().map_or(0, std::num::NonZero::get),
        },
    });
    let text = serde_json::to_string_pretty(&report).unwrap_or_else(|e| fail(e));
    println!("{text}");
    if let Some(out) = &args.out {
        if let Some(dir) = out.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        std::fs::write(out, &text).unwrap_or_else(|e| fail(e));
    }
    let _ = std::fs::remove_dir_all(&scratch);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn live_lag_percentile_is_the_nearest_rank() {
        assert_eq!(percentile(&[], 0.5), None);
        assert_eq!(percentile(&[1.0], 0.95), Some(1.0));
        assert_eq!(percentile(&[1.0, 2.0, 3.0], 0.5), Some(2.0));
        assert_eq!(percentile(&[1.0, 2.0, 3.0], 1.0), Some(3.0));
    }

    #[test]
    fn live_lag_measures_each_word_from_its_end() {
        let mut lags = TrackLags::default();
        lags.add(Duration::from_secs(10), &[(1000, 2000), (2000, 4000)]);
        assert_eq!(lags.word_s, vec![8.0, 6.0]);
        assert_eq!(lags.first_word_s, vec![8.0]);
        assert_eq!(lags.last_word_s, vec![6.0]);
        assert_eq!(lags.stretch_s, vec![3.0]);
        lags.add(Duration::from_secs(11), &[]);
        assert_eq!(
            lags.first_word_s.len(),
            1,
            "a final without words is no stretch"
        );
    }
}
