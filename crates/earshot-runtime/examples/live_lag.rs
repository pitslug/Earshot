//! How long words take from being spoken to reaching the screen (T9.1m): a real-time replay of
//! public audio through a production live session, reporting counts and times only, never text.
//!
//! ```text
//! cargo run --release -p earshot-runtime --example live_lag -- <repo-root>
//!     [--audio bundled|continuous] [--backend cpu|cuda|qnn] [--models <dir>] [--speakers on|off]
//!     [--seconds <n>] [--previews off|on|everyone] [--force-previews] [--card-memory-gib <n>]
//!     [--window <s>] [--interval <s>] [--out <file.json>]
//! cargo run --release -p earshot-runtime --example live_lag -- <repo-root> --memory
//!     [--backend cpu|cuda|qnn] [--card-memory-gib <n>] [--out <file.json>]
//! cargo run --release -p earshot-runtime --features onnx-cuda --example live_lag -- <repo-root>
//!     --backend cuda --contention alone|own|shared [--audio bundled|continuous] [--out <file.json>]
//! ```
//!
//! `--backend qnn` needs an ARM64 build with `--features qnn` (the NPU, T12.4). The app refuses
//! previews there (`backends::previews_refused`); `--force-previews` (evaluation only) loads the
//! preview sessions through `backends::load_preview_unrefused` instead, which skips only that
//! refusal. The report's `preview_refusal` says what the app would decide on this machine and
//! whether this run went past it; `preview_sessions` lists every preview load with the backend
//! the loaded session reports and, when forced, each ONNX Runtime session's placement (from ONNX
//! Runtime's placement log, already checked by the load), never the request. `--memory` loads a
//! track's session and then two preview sessions (always past the refusal): the first preview
//! session comes with "on", the second only with "everyone".
//!
//! `--card-memory-gib <n>` (evaluation only, T12.3c) simulates a smaller graphics card: the preview
//! rule reads this machine's hardware facts with every non-integrated adapter's dedicated memory set
//! to `n` GiB, and the preview sessions load through `backends::load_preview_on_hardware`, the app's
//! rule and loader on that machine (refusals included). Nothing else reads the simulated value:
//! transcription runs where `--backend` asks and the card's real memory is untouched. The report
//! records it (`simulated_card_gib`). With `--memory` the two preview sessions load where the rule
//! puts them on that machine (Everyone), not past the refusal.
//!
//! `--contention alone|own|shared` (evaluation only, T12.3b, CUDA) replays nothing: it times a
//! track's real decodes of the reference track while previews run at the app's cadence on another
//! thread, with no preview (`alone`), the preview on its own session (`own`, today) or borrowing
//! the track's encoder through `backends::load_preview_sharing_encoder` (`shared`). It reports
//! every real decode's time, its wait for the shared encoder's lock and a hash of its words.
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
use earshot_runtime::backends::{
    Accelerator, ModelDir, PreviewSlot, init_onnx, load_preview_on_hardware,
    load_preview_unrefused, onnx_library, preview_backend, previews_refused,
    previews_refused_second, second_preview_backend,
};
use earshot_runtime::live::{
    LiveAsrFactory, LiveConfig, LivePreviews, PreviewLoader, RuntimeEvent, start_live,
};
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
    /// Evaluation only (T12.4): load the preview sessions with
    /// `backends::load_preview_unrefused`, past the app's refusal (`previews_refused`,
    /// `previews_refused_second`), so the NPU can be measured. The report records it.
    force_previews: bool,
    /// Evaluation only (T12.3c): the preview rule reads a graphics card of this many GiB
    /// ([`simulated_machine`]), through `backends::load_preview_on_hardware`.
    card_memory_gib: Option<u64>,
    /// Evaluation only (T12.3b): measure real decodes beside previews instead of replaying
    /// ([`contention_report`]).
    contention: Option<Contention>,
}

fn parse_args() -> Args {
    parse_from(std::env::args().skip(1)).unwrap_or_else(|e| fail(e))
}

/// [`Args`] from the arguments after the program name.
fn parse_from(mut raw: impl Iterator<Item = String>) -> Result<Args, String> {
    let repo = raw
        .next()
        .ok_or_else(|| "usage: live_lag <repo-root> [options]".to_owned())?;
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
        force_previews: false,
        card_memory_gib: None,
        contention: None,
    };
    while let Some(flag) = raw.next() {
        let mut value = || raw.next().ok_or_else(|| format!("{flag} needs a value"));
        match flag.as_str() {
            "--audio" => args.audio = value()?,
            "--backend" => {
                args.backend = match value()?.as_str() {
                    "cpu" => BackendKind::Cpu,
                    "cuda" => BackendKind::Cuda,
                    "qnn" => BackendKind::Qnn,
                    other => return Err(format!("unknown backend {other}")),
                }
            }
            "--models" => args.models = Some(PathBuf::from(value()?)),
            "--speakers" => args.speakers = value()? == "on",
            "--seconds" => {
                args.seconds = value()?.parse().map_err(|e| format!("--seconds: {e}"))?;
            }
            "--out" => args.out = Some(PathBuf::from(value()?)),
            "--memory" => args.memory = true,
            "--previews" => {
                args.previews = match value()?.as_str() {
                    "off" => None,
                    "on" => Some(PreviewChoice::Automatic),
                    "everyone" => Some(PreviewChoice::Everyone),
                    other => return Err(format!("unknown --previews {other}")),
                }
            }
            "--window" => args.window = value()?.parse().ok(),
            "--interval" => args.interval = value()?.parse().ok(),
            "--force-previews" => args.force_previews = true,
            "--card-memory-gib" => {
                let gib: u64 = value()?
                    .parse()
                    .map_err(|e| format!("--card-memory-gib: {e}"))?;
                if gib == 0 {
                    return Err("--card-memory-gib must be at least 1".to_owned());
                }
                args.card_memory_gib = Some(gib);
            }
            "--contention" => {
                args.contention = Some(match value()?.as_str() {
                    "alone" => Contention::Alone,
                    "own" => Contention::Own,
                    "shared" => Contention::Shared,
                    other => return Err(format!("unknown --contention {other}")),
                });
            }
            other => return Err(format!("unknown flag {other}")),
        }
    }
    if args.force_previews && (args.memory || args.previews.is_none()) {
        return Err(
            "--force-previews needs --previews on|everyone and no --memory (the memory report \
             always loads its preview sessions past the refusal)"
                .to_owned(),
        );
    }
    if args.force_previews && args.card_memory_gib.is_some() {
        return Err(
            "--card-memory-gib applies the app's rule to a simulated card; --force-previews skips \
             the rule: use one"
                .to_owned(),
        );
    }
    if args.contention.is_some()
        && (args.memory
            || args.previews.is_some()
            || args.force_previews
            || args.card_memory_gib.is_some()
            || args.backend != BackendKind::Cuda)
    {
        return Err(
            "--contention measures one CUDA track's decodes beside previews: it needs \
             --backend cuda and no --memory, --previews, --force-previews or --card-memory-gib"
                .to_owned(),
        );
    }
    Ok(args)
}

/// Bytes in a GiB.
const GIB: u64 = 1024 * 1024 * 1024;

/// Evaluation only (`--card-memory-gib`, T12.3c): `machine` with every non-integrated graphics
/// adapter's dedicated memory set to `gib` GiB and nothing else changed, the facts the preview rule
/// reads for the card.
fn simulated_machine(
    mut machine: earshot_hardware::Hardware,
    gib: u64,
) -> Result<earshot_hardware::Hardware, String> {
    let mut any = false;
    for gpu in machine
        .gpus
        .iter_mut()
        .filter(|g| g.integrated != Some(true))
    {
        gpu.dedicated_memory = Some(gib * GIB);
        any = true;
    }
    if any {
        Ok(machine)
    } else {
        Err("--card-memory-gib: this machine lists no graphics card to simulate".to_owned())
    }
}

/// How a run's preview sessions are loaded.
#[derive(Clone)]
enum PreviewLoads {
    /// The app's loaders (`host::live_previews`), on this machine.
    App,
    /// `--force-previews` (T12.4): past the app's refusal.
    Forced,
    /// `--card-memory-gib` (T12.3c): the app's rule and loader on a simulated machine.
    Simulated(earshot_hardware::Hardware),
}

/// What one preview loader did: the session asked for (`requested`, the track's backend) and,
/// when it loaded, the backend the loaded session reports (from ONNX Runtime's placement) and,
/// on the evaluation path, every session's placement; or why it did not load.
fn preview_record(
    slot: &str,
    forced: bool,
    requested: &BackendInfo,
    result: Result<(BackendInfo, Option<Vec<String>>), &str>,
) -> Value {
    let mut record = json!({
        "slot": slot,
        "refusal_bypassed": forced,
        "requested": { "kind": format!("{:?}", requested.kind), "model": requested.model },
    });
    match result {
        Ok((ran, placement)) => {
            record["loaded"] = json!({
                "kind": format!("{:?}", ran.kind),
                "device": ran.device,
                "model": ran.model,
                "placement": placement,
            });
        }
        Err(why) => record["not_loaded"] = json!(why),
    }
    record
}

/// `load` with every outcome written to `log` ([`preview_record`]).
fn recorded_loader(
    slot: &'static str,
    load: PreviewLoader,
    log: Arc<Mutex<Vec<Value>>>,
) -> PreviewLoader {
    Arc::new(move |backend: &BackendInfo| {
        let result = load(backend);
        let record = preview_record(
            slot,
            false,
            backend,
            result
                .as_ref()
                .map(|t| (t.backend_info(), None))
                .map_err(String::as_str),
        );
        log.lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(record);
        result
    })
}

/// Evaluation only (`--force-previews`, T12.4): a loader past the app's refusal, through
/// `backends::load_preview_unrefused`, with the placement of every session written to `log`.
fn forced_loader(
    slot: &'static str,
    models: ModelDir,
    log: Arc<Mutex<Vec<Value>>>,
) -> PreviewLoader {
    Arc::new(move |backend: &BackendInfo| {
        let result = load_preview_unrefused(&models, backend);
        let record = preview_record(
            slot,
            true,
            backend,
            result
                .as_ref()
                .map(|p| (p.transcriber.backend_info(), Some(p.placement.clone())))
                .map_err(String::as_str),
        );
        log.lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(record);
        result.map(|p| p.transcriber)
    })
}

/// Evaluation only (`--card-memory-gib`, T12.3c): the app's rule and loader for `slot` on the
/// simulated `machine`, through `backends::load_preview_on_hardware`, with the placement of every
/// session written to `log`.
fn simulated_loader(
    slot: PreviewSlot,
    models: ModelDir,
    choice: PreviewChoice,
    machine: earshot_hardware::Hardware,
    log: Arc<Mutex<Vec<Value>>>,
) -> PreviewLoader {
    Arc::new(move |backend: &BackendInfo| {
        let result = load_preview_on_hardware(&models, backend, choice, slot, &machine);
        let mut record = preview_record(
            match slot {
                PreviewSlot::First => "first",
                PreviewSlot::OwnVoice => "own_voice",
            },
            false,
            backend,
            result
                .as_ref()
                .map(|p| (p.transcriber.backend_info(), Some(p.placement.clone())))
                .map_err(String::as_str),
        );
        record["simulated_card_gib"] = json!(simulated_card_gib(&machine));
        log.lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(record);
        result.map(|p| p.transcriber)
    })
}

/// The card memory a simulated machine shows, in GiB.
fn simulated_card_gib(machine: &earshot_hardware::Hardware) -> Option<u64> {
    machine
        .gpus
        .iter()
        .filter(|g| g.integrated != Some(true))
        .filter_map(|g| g.dedicated_memory)
        .max()
        .map(|m| m / GIB)
}

/// The session's previews for `choice`: production's (`host::live_previews`), or the evaluation
/// loaders of `loads`; either way every load is recorded in `log`.
fn previews_for(
    models: &ModelDir,
    choice: PreviewChoice,
    loads: &PreviewLoads,
    log: &Arc<Mutex<Vec<Value>>>,
) -> LivePreviews {
    match loads {
        PreviewLoads::App => {}
        PreviewLoads::Forced => {
            let mut p = LivePreviews::new(forced_loader("first", models.clone(), Arc::clone(log)));
            if choice == PreviewChoice::Everyone {
                p.own_voice = Some(forced_loader("own_voice", models.clone(), Arc::clone(log)));
            }
            return p;
        }
        PreviewLoads::Simulated(machine) => {
            let loader = |slot| {
                simulated_loader(
                    slot,
                    models.clone(),
                    choice,
                    machine.clone(),
                    Arc::clone(log),
                )
            };
            let mut p = LivePreviews::new(loader(PreviewSlot::First));
            if choice == PreviewChoice::Everyone {
                p.own_voice = Some(loader(PreviewSlot::OwnVoice));
            }
            return p;
        }
    }
    let mut p = host::live_previews(models, choice);
    p.load = recorded_loader("first", p.load, Arc::clone(log));
    p.own_voice = p
        .own_voice
        .map(|l| recorded_loader("own_voice", l, Arc::clone(log)));
    p
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

/// The machine's free physical memory in bytes (`Win32_OperatingSystem.FreePhysicalMemory`, KiB).
fn system_free_bytes() -> Option<f64> {
    let out = Command::new("powershell")
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            "(Get-CimInstance Win32_OperatingSystem).FreePhysicalMemory",
        ])
        .output()
        .ok()?;
    let kib: f64 = String::from_utf8_lossy(&out.stdout).trim().parse().ok()?;
    Some(kib * 1024.0)
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
fn memory_report(
    models: &ModelDir,
    model: &str,
    accel: Accelerator,
    audio: &[f32],
    simulated: Option<&earshot_hardware::Hardware>,
) -> Value {
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
    // The machine's free memory too: what an NPU driver holds for a session may not be counted in
    // this process's private bytes.
    let memory = || {
        host_reading().map(|r| {
            json!({
                "private_gib": gib(r.2),
                "peak_working_set_gib": gib(r.3),
                "system_free_gib": system_free_bytes().map(gib),
            })
        })
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
    let mut sessions: Vec<Box<dyn Transcriber + Send>> = Vec::new();
    // Session 1 is a track's own (a normal session); 2 and 3 are preview sessions loaded as the
    // app loads them (background threads), past the app's refusal (evaluation only, T12.4), or on
    // a simulated machine where the app's rule puts them under Everyone (T12.3c): the first
    // preview comes with "on", the second only with "everyone".
    let requested = BackendInfo {
        kind: accel.backend(),
        device: String::new(),
        model: model.to_owned(),
    };
    for (n, role) in [
        (1, "a track's own session"),
        (2, "first preview session (on, everyone)"),
        (3, "second preview session (everyone only)"),
    ] {
        let began = Instant::now();
        let (session, placement): (Box<dyn Transcriber + Send>, Vec<String>) = if n == 1 {
            let s =
                earshot_runtime::backends::load_parakeet(models, parakeet, accel, Some(threads))
                    .unwrap_or_else(|e| fail(e));
            let placement = s.backends().iter().map(ToString::to_string).collect();
            (Box::new(s), placement)
        } else if let Some(machine) = simulated {
            let slot = if n == 2 {
                PreviewSlot::First
            } else {
                PreviewSlot::OwnVoice
            };
            let p = load_preview_on_hardware(
                models,
                &requested,
                PreviewChoice::Everyone,
                slot,
                machine,
            )
            .unwrap_or_else(|e| fail(e));
            (p.transcriber, p.placement)
        } else {
            let p = load_preview_unrefused(models, &requested).unwrap_or_else(|e| fail(e));
            (p.transcriber, p.placement)
        };
        let load_s = began.elapsed().as_secs_f64();
        let ran = session.backend_info();
        stages.push(json!({
            "stage": format!("session {n} loaded"),
            "role": role,
            "preview_session": n > 1,
            "refusal_bypassed": n > 1 && simulated.is_none(),
            "simulated_card_gib": simulated.and_then(simulated_card_gib),
            "backend": { "kind": format!("{:?}", ran.kind), "device": ran.device },
            "placement": placement,
            "load_s": load_s,
            "memory": memory(),
        }));
        let mut session = session;
        let took: Vec<f64> = (0..3).map(|_| decode(&mut *session, 20.0, 12.0)).collect();
        stages.push(json!({ "stage": format!("session {n}, 3 decodes of 12 s"), "took_s": took, "memory": memory() }));
        sessions.push(session);
    }
    let session = &mut **sessions.first_mut().unwrap_or_else(|| fail("no session"));
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
        "simulated_card_gib": simulated.and_then(simulated_card_gib),
        "threads": threads,
        "stages": stages,
        "window_lengths": lengths,
        "new_lengths": new_lengths,
        "machine_load_percent": host_reading().map(|r| r.0),
    })
}

/// The set-ups `--contention` compares (T12.3b, evaluation only).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Contention {
    /// The track's session alone: the reference times.
    Alone,
    /// Today: a preview on its own session, loaded as the app loads one.
    Own,
    /// The preview borrowing the track's encoder (`backends::load_preview_sharing_encoder`).
    Shared,
}

/// Length of a track's real decode in `--contention` (a speech stretch's piece).
const CONTENTION_PIECE_S: f64 = 15.0;
/// Times the real decodes go over the audio after the untimed warm-up pass.
const CONTENTION_PASSES: usize = 3;
/// Pause between two real decodes, so previews fall between and during them.
const CONTENTION_GAP: Duration = Duration::from_millis(500);
/// The preview window and interval of the app (docs/live-previews.md, "What was chosen").
const CONTENTION_WINDOW_S: f64 = 12.0;
const CONTENTION_INTERVAL: Duration = Duration::from_secs(1);

/// A decode that returns its words' hash and how long it waited for a shared encoder's lock.
type ContentionDecode = Box<dyn FnMut(&MonoBuffer) -> (String, Vec<Duration>) + Send>;

/// The words of a transcription (text and times), hashed: never the text itself.
fn words_sha256(t: &Transcription) -> String {
    let mut h = Sha256::new();
    for w in t.segments.iter().flat_map(|s| s.words.iter()) {
        h.update(w.text.as_bytes());
        h.update(w.start.as_millis().to_le_bytes());
        h.update(w.end.as_millis().to_le_bytes());
    }
    h.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

fn plain_decode(mut t: Box<dyn Transcriber + Send>) -> ContentionDecode {
    let options = TranscribeOptions::live();
    let cancel = CancelToken::new();
    Box::new(move |audio| {
        let words = t
            .transcribe(audio, &options, &cancel)
            .unwrap_or_else(|e| fail(e));
        (words_sha256(&words), Vec::new())
    })
}

fn sharing_decode(mut t: earshot_onnx::parakeet::ParakeetTdt) -> ContentionDecode {
    let options = TranscribeOptions::live();
    let cancel = CancelToken::new();
    Box::new(move |audio| {
        let words = t
            .transcribe(audio, &options, &cancel)
            .unwrap_or_else(|e| fail(e));
        (words_sha256(&words), t.take_encoder_waits())
    })
}

/// Evaluation only (T12.3b): how long a track's real decodes take while previews run at the app's
/// cadence on another thread, with the preview on its own session (`own`), on the track's encoder
/// (`shared`) or absent (`alone`). Real decodes are consecutive [`CONTENTION_PIECE_S`] pieces of
/// `audio`, [`CONTENTION_PASSES`] times after an untimed warm-up pass, [`CONTENTION_GAP`] apart;
/// previews decode a [`CONTENTION_WINDOW_S`] window moving 1 s at a time and wait the longer of
/// [`CONTENTION_INTERVAL`] and their own decode. Counts, times and word hashes only.
fn contention_report(
    models: &ModelDir,
    model: &str,
    accel: Accelerator,
    audio: &[f32],
    setup: Contention,
) -> Value {
    let threads = earshot_runtime::backends::onnx_cpu_threads(
        std::thread::available_parallelism().map_or(1, std::num::NonZero::get),
        2,
    );
    let gib = |v: f64| v / 1024.0 / 1024.0 / 1024.0;
    let memory = || host_reading().map(|r| json!({ "private_gib": gib(r.2) }));
    let frames = |seconds: f64| (seconds * f64::from(PIPELINE_RATE.hz())) as usize;
    let window = |start_s: f64, length_s: f64| {
        let start = frames(start_s).min(audio.len());
        let end = (start + frames(length_s)).min(audio.len());
        MonoBuffer::new(PIPELINE_RATE, audio[start..end].to_vec())
    };
    let requested = BackendInfo {
        kind: accel.backend(),
        device: String::new(),
        model: model.to_owned(),
    };
    let mut placements = serde_json::Map::new();
    let mut encoder_users = None;
    let began = Instant::now();
    let (mut track, preview): (ContentionDecode, Option<ContentionDecode>) = match setup {
        Contention::Alone | Contention::Own => {
            let earshot_runtime::backends::AsrModel::Parakeet(parakeet) =
                earshot_runtime::backends::asr_model(model).unwrap_or_else(|e| fail(e))
            else {
                fail("the contention report is for Parakeet")
            };
            let t =
                earshot_runtime::backends::load_parakeet(models, parakeet, accel, Some(threads))
                    .unwrap_or_else(|e| fail(e));
            placements.insert(
                "track".into(),
                json!(
                    t.backends()
                        .iter()
                        .map(ToString::to_string)
                        .collect::<Vec<_>>()
                ),
            );
            let preview = (setup == Contention::Own).then(|| {
                let p = load_preview_unrefused(models, &requested).unwrap_or_else(|e| fail(e));
                placements.insert("preview".into(), json!(p.placement));
                plain_decode(p.transcriber)
            });
            (plain_decode(Box::new(t)), preview)
        }
        Contention::Shared => {
            let pair = earshot_runtime::backends::load_preview_sharing_encoder(
                models,
                model,
                accel,
                Some(threads),
            )
            .unwrap_or_else(|e| fail(e));
            let names = |t: &earshot_onnx::parakeet::ParakeetTdt| {
                t.backends()
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
            };
            placements.insert("track".into(), json!(names(&pair.track)));
            placements.insert("preview".into(), json!(names(&pair.preview)));
            placements.insert(
                "shared_encoder".into(),
                json!(pair.encoder.backend().to_string()),
            );
            encoder_users = Some(pair.encoder.users());
            (
                sharing_decode(pair.track),
                Some(sharing_decode(pair.preview)),
            )
        }
    };
    let load_s = began.elapsed().as_secs_f64();
    let loaded_memory = memory();

    // Point 2's fixed window: the preview's words for it against the track's.
    let fixed = window(20.0, CONTENTION_WINDOW_S);
    let (track_fixed, _) = track(&fixed);
    let mut preview = preview;
    let preview_fixed = preview.as_mut().map(|p| p(&fixed).0);

    let length_s = audio.len() as f64 / f64::from(PIPELINE_RATE.hz());
    let pieces: Vec<f64> = (0..)
        .map(|k| f64::from(k) * CONTENTION_PIECE_S)
        .take_while(|s| *s + 1.0 < length_s)
        .collect();
    // Warm-up: every piece length once, untimed (a CUDA session sets up per new input length).
    for start in &pieces {
        let _ = track(&window(*start, CONTENTION_PIECE_S));
    }

    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let preview_thread = preview.map(|mut p| {
        let stop = Arc::clone(&stop);
        let audio_windows: Vec<MonoBuffer> = (0..)
            .map(f64::from)
            .take_while(|s| *s + CONTENTION_WINDOW_S < length_s)
            .map(|s| window(s, CONTENTION_WINDOW_S))
            .collect();
        std::thread::spawn(move || {
            let mut took = Vec::new();
            let mut waits = Vec::new();
            let mut k = 0;
            while !stop.load(std::sync::atomic::Ordering::SeqCst) && !audio_windows.is_empty() {
                let began = Instant::now();
                let (_, w) = p(&audio_windows[k % audio_windows.len()]);
                let t = began.elapsed();
                took.push(t.as_secs_f64());
                waits.extend(w.iter().map(Duration::as_secs_f64));
                k += 1;
                std::thread::sleep(t.max(CONTENTION_INTERVAL));
            }
            (took, waits)
        })
    });

    let mut decodes = Vec::new();
    let (mut took_all, mut waited_all) = (Vec::new(), Vec::new());
    for pass in 0..CONTENTION_PASSES {
        for (piece, start) in pieces.iter().enumerate() {
            let buffer = window(*start, CONTENTION_PIECE_S);
            let began = Instant::now();
            let (words, waits) = track(&buffer);
            let took = began.elapsed().as_secs_f64();
            let waited: f64 = waits.iter().map(Duration::as_secs_f64).sum();
            took_all.push(took);
            waited_all.push(waited);
            decodes.push(json!({
                "pass": pass,
                "piece": piece,
                "start_s": start,
                "took_s": took,
                "encoder_wait_s": waited,
                "encoder_calls": waits.len(),
                "words_sha256": words,
            }));
            std::thread::sleep(CONTENTION_GAP);
        }
    }
    stop.store(true, std::sync::atomic::Ordering::SeqCst);
    let previews = preview_thread.map(|h| {
        let (took, waits) = h
            .join()
            .unwrap_or_else(|_| fail("the preview thread panicked"));
        json!({
            "decodes": took.len(),
            "took_s": spread(&took),
            "encoder_wait_s": spread(&waits),
        })
    });
    json!({
        "contention": format!("{setup:?}").to_lowercase(),
        "model": model,
        "threads": threads,
        "placements": placements,
        "encoder_users": encoder_users,
        "load_s": load_s,
        "memory_after_load": loaded_memory,
        "fixed_window": {
            "track_words_sha256": track_fixed,
            "preview_words_sha256": preview_fixed,
            "preview_matches_track": preview_fixed.as_ref().map(|p| *p == track_fixed),
        },
        "piece_s": CONTENTION_PIECE_S,
        "passes": CONTENTION_PASSES,
        "real_decodes": decodes,
        "real_took_s": spread(&took_all),
        "real_encoder_wait_s": spread(&waited_all),
        "previews": previews,
        "memory_at_end": memory(),
        "machine_load_percent": host_reading().map(|r| r.0),
    })
}

fn main() {
    // NPU compiles in a helper, as in the app (T12.12b).
    let raw: Vec<std::ffi::OsString> = std::env::args_os().collect();
    if let Some(code) = earshot_runtime::npu_compiled::child_main(&raw) {
        std::process::exit(code);
    }
    earshot_runtime::npu_compiled::use_this_exe();
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

    // Evaluation only (T12.3c): the machine the preview rule reads, when a card is simulated.
    let simulated = args
        .card_memory_gib
        .map(|gib| simulated_machine(earshot_runtime::hardware(), gib).unwrap_or_else(|e| fail(e)));

    if let Some(setup) = args.contention {
        let report = contention_report(&models, model, accel, &reference, setup);
        let text = serde_json::to_string_pretty(&report).unwrap_or_else(|e| fail(e));
        println!("{text}");
        if let Some(out) = &args.out {
            std::fs::write(out, &text).unwrap_or_else(|e| fail(e));
        }
        return;
    }

    if args.memory {
        let report = memory_report(&models, model, accel, &reference, simulated.as_ref());
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
    let preview_sessions = Arc::new(Mutex::new(Vec::new()));
    // What the app would decide on this machine, whatever this run does.
    let refusal = args.previews.map(|choice| {
        let machine = simulated.clone().unwrap_or_else(earshot_runtime::hardware);
        json!({
            "forced_past_it": args.force_previews,
            "simulated_card_gib": simulated.as_ref().and_then(simulated_card_gib),
            "app_first": previews_refused(args.backend, &machine, choice),
            // Where the app would run the first session (T12.3c: the CPU beside a small card).
            "app_first_on": preview_backend(args.backend, &machine, choice)
                .ok()
                .map(|k| format!("{k:?}")),
            "app_own_voice": (choice == PreviewChoice::Everyone)
                .then(|| previews_refused_second(args.backend, &machine, choice)),
            // Where the app would run the user's own track's session (T12.3a: the CPU beside CUDA).
            "app_own_voice_on": (choice == PreviewChoice::Everyone)
                .then(|| second_preview_backend(args.backend, &machine, choice).ok())
                .flatten()
                .map(|k| format!("{k:?}")),
        })
    });
    let previews = args.previews.map(|choice| {
        let loads = match (&simulated, args.force_previews) {
            (Some(machine), _) => PreviewLoads::Simulated(machine.clone()),
            (None, true) => PreviewLoads::Forced,
            (None, false) => PreviewLoads::App,
        };
        let mut p = previews_for(&models, choice, &loads, &preview_sessions);
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
                    "backend": r.backend.map(|k| format!("{k:?}")),
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
        "simulated_card_gib": simulated.as_ref().and_then(simulated_card_gib),
        "live_backends": backends,
        "onnx_placement": *evidence.lock().unwrap_or_else(PoisonError::into_inner),
        "speakers": args.speakers,
        "warnings": problems,
        "stopped_with": format!("{:?}", outcome.reason),
        "capture_max_lag_s": state.max_lag().as_secs_f64(),
        "catch_up_after_end_s": catch_up,
        "previews": preview_reports,
        "preview_refusal": refusal,
        "preview_sessions": *preview_sessions.lock().unwrap_or_else(PoisonError::into_inner),
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

    fn parse(args: &[&str]) -> Result<Args, String> {
        parse_from(
            std::iter::once("repo")
                .chain(args.iter().copied())
                .map(str::to_owned),
        )
    }

    #[test]
    fn live_lag_parses_the_npu_backend_and_the_forced_previews() {
        let a = parse(&[
            "--backend",
            "qnn",
            "--previews",
            "everyone",
            "--force-previews",
        ])
        .unwrap();
        assert_eq!(a.backend, BackendKind::Qnn);
        assert_eq!(a.previews, Some(PreviewChoice::Everyone));
        assert!(a.force_previews);
        let a = parse(&["--backend", "qnn", "--previews", "on"]).unwrap();
        assert_eq!(a.previews, Some(PreviewChoice::Automatic));
        assert!(!a.force_previews, "never forced unless asked");
        let a = parse(&["--backend", "qnn", "--memory"]).unwrap();
        assert!(a.memory);
        assert_eq!(parse(&[]).unwrap().backend, BackendKind::Cpu);
        assert!(parse(&["--backend", "npu"]).is_err());
        assert!(parse(&["--backend"]).is_err());
        assert!(parse(&["--previews", "some"]).is_err());
        assert!(parse(&["--share-npu"]).is_err(), "no such flag");
    }

    #[test]
    fn live_lag_forced_previews_need_previews_and_no_memory_mode() {
        assert!(parse(&["--force-previews"]).is_err());
        assert!(parse(&["--force-previews", "--previews", "off"]).is_err());
        assert!(parse(&["--force-previews", "--previews", "on", "--memory"]).is_err());
        assert!(parse(&["--previews", "on", "--force-previews"]).is_ok());
    }

    fn npu_track() -> BackendInfo {
        BackendInfo {
            kind: BackendKind::Qnn,
            device: "npu".to_owned(),
            model: "parakeet-tdt-0.6b-v2".to_owned(),
        }
    }

    fn empty_models() -> ModelDir {
        ModelDir::new(std::env::temp_dir().join(format!("live-lag-preview-{}", std::process::id())))
    }

    /// Without `--force-previews` the app's loaders run unchanged: the NPU is refused, recorded.
    #[test]
    fn live_lag_preview_loads_are_the_apps_unless_forced() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let p = previews_for(
            &empty_models(),
            PreviewChoice::Everyone,
            &PreviewLoads::App,
            &log,
        );
        let own = p.own_voice.clone().expect("everyone has a second loader");
        for load in [p.load, own] {
            assert_eq!(
                load(&npu_track()).err().as_deref(),
                Some("not measured on the NPU yet")
            );
        }
        let log = log.lock().unwrap();
        assert_eq!(log.len(), 2);
        for (record, slot) in log.iter().zip(["first", "own_voice"]) {
            assert_eq!(record["slot"], slot);
            assert_eq!(record["refusal_bypassed"], false);
            assert_eq!(record["not_loaded"], "not measured on the NPU yet");
            assert_eq!(record["requested"]["kind"], "Qnn");
        }
        let on = previews_for(
            &empty_models(),
            PreviewChoice::Automatic,
            &PreviewLoads::App,
            &Arc::new(Mutex::new(Vec::new())),
        );
        assert!(on.own_voice.is_none());
    }

    /// With `--force-previews` the refusal is skipped (the load goes on and fails here for a
    /// load reason, on an empty models folder) and the record says it was bypassed.
    #[test]
    fn live_lag_forced_preview_loads_skip_only_the_refusal() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let p = previews_for(
            &empty_models(),
            PreviewChoice::Everyone,
            &PreviewLoads::Forced,
            &log,
        );
        let own = p.own_voice.clone().expect("everyone has a second loader");
        for load in [p.load, own] {
            let why = load(&npu_track()).err().expect("no models here");
            assert!(!why.contains("not measured"), "{why}");
        }
        let log = log.lock().unwrap();
        assert_eq!(log.len(), 2);
        assert!(log.iter().all(|r| r["refusal_bypassed"] == true));
        let on = previews_for(
            &empty_models(),
            PreviewChoice::Automatic,
            &PreviewLoads::Forced,
            &Arc::new(Mutex::new(Vec::new())),
        );
        assert!(
            on.own_voice.is_none(),
            "forcing never adds the second session"
        );
    }

    #[test]
    fn live_lag_parses_the_simulated_card() {
        let a = parse(&[
            "--backend",
            "cuda",
            "--card-memory-gib",
            "12",
            "--previews",
            "on",
        ])
        .unwrap();
        assert_eq!(a.card_memory_gib, Some(12));
        let a = parse(&["--backend", "cuda", "--card-memory-gib", "12", "--memory"]).unwrap();
        assert_eq!(a.card_memory_gib, Some(12));
        assert_eq!(
            parse(&[]).unwrap().card_memory_gib,
            None,
            "never simulated unless asked"
        );
        assert!(parse(&["--card-memory-gib"]).is_err());
        assert!(parse(&["--card-memory-gib", "twelve"]).is_err());
        assert!(parse(&["--card-memory-gib", "0"]).is_err());
        assert!(
            parse(&[
                "--card-memory-gib",
                "12",
                "--previews",
                "on",
                "--force-previews"
            ])
            .is_err(),
            "the simulation applies the rule, forcing skips it"
        );
    }

    fn a_card(memory: Option<u64>, integrated: Option<bool>) -> earshot_hardware::Gpu {
        earshot_hardware::Gpu {
            name: "A card".to_owned(),
            vendor_id: 0x10de,
            dedicated_memory: memory,
            driver_version: None,
            integrated,
            compute_capability: None,
        }
    }

    /// The simulation changes only the dedicated memory of the adapters the rule reads as the card.
    #[test]
    fn live_lag_simulated_machine_changes_only_the_cards_memory() {
        let machine = earshot_hardware::Hardware {
            total_memory: Some(64 * GIB),
            gpus: vec![
                a_card(Some(16 * GIB), Some(false)),
                a_card(Some(GIB / 2), Some(true)),
                a_card(None, None),
            ],
            ..earshot_hardware::Hardware::default()
        };
        let sim = simulated_machine(machine.clone(), 12).unwrap();
        assert_eq!(sim.total_memory, machine.total_memory);
        assert_eq!(sim.gpus[0].dedicated_memory, Some(12 * GIB));
        assert_eq!(sim.gpus[1].dedicated_memory, Some(GIB / 2), "integrated");
        assert_eq!(sim.gpus[2].dedicated_memory, Some(12 * GIB));
        assert_eq!(sim.gpus[0].name, machine.gpus[0].name);
        assert_eq!(simulated_card_gib(&sim), Some(12));
        // The rule sees a small card: a CUDA track's previews move to the CPU.
        assert_eq!(
            preview_backend(BackendKind::Cuda, &sim, PreviewChoice::Automatic),
            Ok(BackendKind::Cpu)
        );
        assert_eq!(
            preview_backend(BackendKind::Cuda, &machine, PreviewChoice::Automatic),
            Ok(BackendKind::Cuda)
        );
        let integrated_only = earshot_hardware::Hardware {
            gpus: vec![a_card(Some(GIB), Some(true))],
            ..earshot_hardware::Hardware::default()
        };
        assert!(simulated_machine(integrated_only, 12).is_err());
    }

    /// With a simulated card the app's rule runs on that machine: a CUDA track's previews are
    /// asked for on the CPU (the load fails here for a load reason, on an empty models folder), the
    /// NPU is still refused, and every record names the simulated card and no bypass.
    #[test]
    fn live_lag_simulated_preview_loads_apply_the_apps_rule_to_the_simulated_card() {
        let machine = earshot_hardware::Hardware {
            total_memory: Some(64 * GIB),
            gpus: vec![a_card(Some(12 * GIB), Some(false))],
            ..earshot_hardware::Hardware::default()
        };
        let log = Arc::new(Mutex::new(Vec::new()));
        let p = previews_for(
            &empty_models(),
            PreviewChoice::Everyone,
            &PreviewLoads::Simulated(machine),
            &log,
        );
        let own = p.own_voice.clone().expect("everyone has a second loader");
        let cuda = BackendInfo {
            kind: BackendKind::Cuda,
            device: "CUDA0".to_owned(),
            model: "parakeet-tdt-0.6b-v2".to_owned(),
        };
        for load in [&p.load, &own] {
            let why = load(&cuda).err().expect("no models here");
            assert!(!why.contains("card") && !why.contains("memory"), "{why}");
            assert_eq!(
                load(&npu_track()).err().as_deref(),
                Some("not measured on the NPU yet")
            );
        }
        let log = log.lock().unwrap();
        assert_eq!(log.len(), 4);
        assert!(log.iter().all(|r| r["refusal_bypassed"] == false));
        assert!(log.iter().all(|r| r["simulated_card_gib"] == 12));
    }

    #[test]
    fn live_lag_preview_record_reports_the_loaded_backend_not_the_request() {
        let ran = BackendInfo {
            kind: BackendKind::Cpu,
            device: "cpu".to_owned(),
            model: "parakeet-tdt-0.6b-v2".to_owned(),
        };
        let placement = vec!["requested qnn, placed: CPUExecutionProvider=12".to_owned()];
        let r = preview_record(
            "first",
            true,
            &npu_track(),
            Ok((ran, Some(placement.clone()))),
        );
        assert_eq!(r["requested"]["kind"], "Qnn");
        assert_eq!(r["loaded"]["kind"], "Cpu");
        assert_eq!(r["loaded"]["placement"], json!(placement));
        assert!(r.get("not_loaded").is_none());
    }

    /// T12.3b: `--contention` is CUDA only and stands alone.
    #[test]
    fn live_lag_contention_needs_cuda_and_no_other_mode() {
        for (value, setup) in [
            ("alone", Contention::Alone),
            ("own", Contention::Own),
            ("shared", Contention::Shared),
        ] {
            let a = parse(&["--backend", "cuda", "--contention", value]).unwrap();
            assert_eq!(a.contention, Some(setup));
        }
        assert!(parse(&["--contention", "shared"]).is_err(), "the CPU");
        assert!(parse(&["--backend", "cuda", "--contention", "both"]).is_err());
        for extra in [
            &["--memory"][..],
            &["--previews", "on"],
            &["--card-memory-gib", "12"],
        ] {
            let mut a = vec!["--backend", "cuda", "--contention", "shared"];
            a.extend_from_slice(extra);
            assert!(parse(&a).is_err(), "{extra:?}");
        }
        assert_eq!(parse(&[]).unwrap().contention, None);
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
