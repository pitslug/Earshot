//! The hidden replay mode behind `accept m7` (T7.6): a recorded two-track meeting through the
//! app's own path, without a window.
//!
//! `earshot-desktop --replay <dir> --llm-url <loopback url> --data <dir> --vault <dir> --model
//! <name> --exit-after-export` is handled first in `main`, before Tauri is touched, so no window
//! or webview is ever created. `<dir>` holds `mic.wav` and `loopback.wav`.
//!
//! Everything after the capture source is what a real meeting runs: the settings are stored
//! through the settings modules, [`MeetingDriver`] runs [`HostPrepare`] (only its capture source
//! differs), and the export is `detail::export`. The results are printed as lines on stdout;
//! transcript text never is.
//!
//! Three refusals keep this mode from touching anything of the user's. `--data` is required and
//! may not overlap the app's own data directory. `--vault` must be an empty folder, so a real
//! vault never gets a fixture note. `--llm-url` must be on loopback: a shipped binary holds no
//! LLM mock (AGENTS.md), so the mock lives in xtask, and this mode can send a transcript nowhere
//! else. There is no default for any path.

use std::ffi::OsString;
use std::io::{BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use earshot_core::audio::read_wav;
use earshot_core::capture::{DeviceTime, ReplayBuilder, TrackBuffer};
use earshot_core::model::ClockDomain;
use earshot_runtime::Services;
use earshot_runtime::backends::ModelDir;
use earshot_runtime::host::{HostOutcome, HostPost, HostStep, SummaryResult};
use earshot_runtime::live::{LiveAsrFactory, LiveOutcome, start_live};
use earshot_runtime::model_settings::{self, ModelPick, ModelSettings};
use earshot_runtime::summary::SummaryRunReport;
use earshot_runtime::summary_settings::{self, EndpointProvider, SummarySettings, SummaryWriter};
use earshot_store::DataDir;

use crate::activity::WorkGate;
use crate::ipc::{AppState, IpcError};
use crate::meeting::{
    HostPrepare, MeetingDriver, MeetingSource, Prepare, Prepared, Sink, StartRequest,
};
use crate::meeting_events::{CaptureMode, LiveTrack, MeetingEvent, SummaryEvent, backend_label};

/// The flag that selects this mode.
pub const REPLAY_ARG: &str = "--replay";
/// Model name configured for the endpoint (the mock ignores it).
const ENDPOINT_MODEL: &str = "m7-mock";
/// Device time of the first replayed frame (any value works; this is 1 s).
const ORIGIN: DeviceTime = DeviceTime::from_ticks(10_000_000);
/// How long the whole meeting may take.
const MEETING_DEADLINE: Duration = Duration::from_secs(2 * 60 * 60);

/// The mode's arguments.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Args {
    /// Holds `mic.wav` and `loopback.wav`.
    pub replay: PathBuf,
    /// The summary endpoint, on loopback; `None` writes the summary with the local model
    /// (`--local-summary`, which T8.4ada's proof needs).
    pub llm_url: Option<String>,
    /// A folder of llama.cpp DLLs to run from (`--llama-dir`).
    pub llama_dir: Option<PathBuf>,
    /// Use CUDA for transcription this run (`--cuda-transcription`), as the app says it at start.
    pub cuda_transcription: bool,
    /// Store both CUDA switches on and run the app's own start (`cuda_settings::begin`, the real
    /// `CudaFiles`) from the models folder (`--cuda-switch`), printing what the Models page would say.
    pub cuda_switch: bool,
    /// With `--cuda-switch`: start the meeting at once, without waiting for that start
    /// (`--cuda-meeting-first`), as someone pressing record while it works.
    pub cuda_meeting_first: bool,
    /// The data directory of this run.
    pub data: PathBuf,
    /// The vault the note goes to.
    pub vault: PathBuf,
    /// The transcription model, by manifest name.
    pub model: String,
}

/// Whether `url` names a loopback address on plain `http` or `https`.
#[must_use]
pub fn is_loopback_url(url: &str) -> bool {
    let Some(rest) = url
        .strip_prefix("http://")
        .or_else(|| url.strip_prefix("https://"))
    else {
        return false;
    };
    let authority = rest.split('/').next().unwrap_or_default();
    // Userinfo could hide another host in front of an `@`.
    if authority.contains('@') {
        return false;
    }
    let host = authority.rsplit_once(':').map_or(authority, |(h, _)| h);
    host == "127.0.0.1" || host == "localhost"
}

/// Whether `args` ask for this mode.
#[must_use]
pub fn asked(args: &[OsString]) -> bool {
    args.iter().any(|a| a == REPLAY_ARG)
}

/// The mode's arguments, `None` when `args` does not ask for it.
///
/// # Errors
/// Why the arguments cannot be used, with nothing run.
pub fn parse(args: &[OsString]) -> Option<Result<Args, String>> {
    if !asked(args) {
        return None;
    }
    Some(parse_replay(args))
}

fn parse_replay(args: &[OsString]) -> Result<Args, String> {
    let (mut replay, mut llm, mut data, mut vault, mut model) = (None, None, None, None, None);
    let mut llama_dir: Option<OsString> = None;
    let (mut exit_after_export, mut local_summary, mut cuda_transcription) = (false, false, false);
    let (mut cuda_switch, mut cuda_meeting_first) = (false, false);
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        let arg = arg
            .to_str()
            .ok_or_else(|| "an argument is not valid text".to_owned())?;
        let slot = match arg {
            "--exit-after-export" => {
                exit_after_export = true;
                continue;
            }
            "--local-summary" => {
                local_summary = true;
                continue;
            }
            "--cuda-transcription" => {
                cuda_transcription = true;
                continue;
            }
            "--cuda-switch" => {
                cuda_switch = true;
                continue;
            }
            "--cuda-meeting-first" => {
                cuda_meeting_first = true;
                continue;
            }
            "--llama-dir" => &mut llama_dir,
            REPLAY_ARG => &mut replay,
            "--llm-url" => &mut llm,
            "--data" => &mut data,
            "--vault" => &mut vault,
            "--model" => &mut model,
            other => return Err(format!("unknown argument {other}")),
        };
        *slot = Some(
            it.next()
                .ok_or_else(|| format!("{arg} needs a value"))?
                .clone(),
        );
    }
    if cuda_switch && (cuda_transcription || llama_dir.is_some()) {
        return Err(
            "--cuda-switch makes the choices --cuda-transcription and --llama-dir make".into(),
        );
    }
    if cuda_meeting_first && !cuda_switch {
        return Err("--cuda-meeting-first needs --cuda-switch".into());
    }
    let need = |v: Option<OsString>, name: &str| v.ok_or_else(|| format!("{name} is required"));
    if !exit_after_export {
        return Err("--exit-after-export is required: this mode always exits".into());
    }
    let text = |v: OsString, name: &str| {
        v.into_string()
            .map_err(|_| format!("{name} is not valid text"))
    };
    let llm_url = match (llm, local_summary) {
        (Some(_), true) => return Err("--llm-url and --local-summary are one or the other".into()),
        (None, true) => None,
        (url, false) => {
            let url = text(need(url, "--llm-url")?, "--llm-url")?;
            if !is_loopback_url(&url) {
                return Err("--llm-url must be on loopback (127.0.0.1 or localhost)".into());
            }
            Some(url)
        }
    };
    Ok(Args {
        replay: need(replay, REPLAY_ARG)?.into(),
        llm_url,
        llama_dir: llama_dir.map(Into::into),
        cuda_transcription,
        cuda_switch,
        cuda_meeting_first,
        data: need(data, "--data")?.into(),
        vault: need(vault, "--vault")?.into(),
        model: text(need(model, "--model")?, "--model")?,
    })
}

/// Runs the mode when `args` asks for it, and returns the process exit code.
#[must_use]
pub fn main_if_asked(args: &[OsString]) -> Option<i32> {
    let parsed = parse(args)?;
    let mut out = std::io::stdout();
    let result = parsed.and_then(|a| run(&a, &mut out));
    Some(match result {
        Ok(()) => 0,
        Err(why) => {
            let _ = writeln!(out, "error: {why}");
            1
        }
    })
}

/// `path` with links, short names and relative parts resolved where it exists.
fn resolved(path: &Path) -> PathBuf {
    std::fs::canonicalize(path)
        .or_else(|_| std::path::absolute(path))
        .unwrap_or_else(|_| path.to_path_buf())
}

/// The path's parts in lower case (Windows paths compare without case; a `\\\\?\\` prefix is one
/// part only on a resolved path, so both sides are resolved first).
fn parts(path: &Path) -> Vec<String> {
    path.components()
        .map(|c| c.as_os_str().to_string_lossy().to_lowercase())
        .collect()
}

/// Refuses a data directory that is the app's own, inside it or holding it. Both are resolved
/// paths ([`resolved`]).
fn check_scratch(data: &Path, app_data: Option<&Path>) -> Result<(), String> {
    let Some(own) = app_data else {
        return Ok(());
    };
    let (data, own) = (parts(data), parts(own));
    let overlap = data.iter().zip(&own).all(|(a, b)| a == b);
    if overlap {
        return Err("--data may not be, hold or be inside the app's own data directory".into());
    }
    Ok(())
}

/// Refuses a vault that holds anything: a run writes a note and temp files into it, so it must
/// not be the user's own.
fn check_vault(vault: &Path) -> Result<(), String> {
    let mut entries = std::fs::read_dir(vault).map_err(|e| format!("cannot read --vault: {e}"))?;
    if entries.next().is_some() {
        return Err("--vault must be an empty folder".into());
    }
    Ok(())
}

fn read_wav_track(path: &Path) -> Result<TrackBuffer, String> {
    let file =
        std::fs::File::open(path).map_err(|e| format!("cannot open {}: {e}", path.display()))?;
    let wav = read_wav(BufReader::new(file))
        .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    TrackBuffer::try_from(wav).map_err(|e| format!("{}: unusable audio ({e:?})", path.display()))
}

/// `inner`, keeping each track's proof of its backend (ONNX Runtime's node placement).
fn recording_placement(
    mut inner: LiveAsrFactory,
    record: Arc<Mutex<AfterRecord>>,
) -> LiveAsrFactory {
    Box::new(move |track, entity| {
        let asr = inner(track, entity)?;
        if let Some(asr) = &asr {
            let mut r = record.lock().unwrap_or_else(PoisonError::into_inner);
            if let Some(evidence) = &asr.evidence {
                r.live_placement.push(format!("{track:?}: {evidence}"));
            }
            // Model and library messages only: nothing has been transcribed yet.
            for (backend, why) in &asr.fallbacks {
                r.live_placement
                    .push(format!("{track:?}: refused {backend:?}: {why}"));
            }
        }
        Ok(asr)
    })
}

/// The replayed capture source: `mic.wav` and `loopback.wav`, as fast as the session reads them.
fn replay_source(dir: &Path, record: Arc<Mutex<AfterRecord>>) -> Result<MeetingSource, IpcError> {
    let bad = |message: String| IpcError {
        kind: "replay".into(),
        message,
        retryable: false,
    };
    let mic = read_wav_track(&dir.join("mic.wav")).map_err(bad)?;
    let reference = read_wav_track(&dir.join("loopback.wav")).map_err(bad)?;
    let source = ReplayBuilder::new(ORIGIN, mic)
        .reference(reference)
        .build()
        .map_err(|e| bad(format!("cannot replay these files: {e:?}")))?;
    Ok(MeetingSource {
        starter: Box::new(move |svc, cfg, asr, rng| {
            let asr = asr.map(|factory| recording_placement(factory, record));
            start_live(source, svc, cfg, asr, rng)
        }),
        clock_domain: ClockDomain::Replay,
        notices: Vec::new(),
        // A replay has no call app to watch.
        call_signal: None,
    })
}

/// What the work after the meeting did, for the printed lines.
#[derive(Default)]
struct AfterRecord {
    /// The final pass's backend label.
    final_backend: Option<String>,
    /// One printed line per network request of the summary.
    network: Vec<String>,
    /// Where a local summary ran, from its load's evidence.
    summary_backend: Option<String>,
    /// Each live track's ONNX Runtime node placement (none for whisper).
    live_placement: Vec<String>,
}

/// The audit line of one request (the format `accept m6` and `m7` read).
fn network_line(host: &HostOutcome) -> Vec<String> {
    host.network
        .iter()
        .map(|e| {
            format!(
                "network: {} {}://{}:{}{} status {:?} ({:?})",
                e.method, e.scheme, e.host, e.port, e.path, e.status, e.outcome
            )
        })
        .collect()
}

/// [`HostPrepare`] with what its `after` step did kept for the report.
struct RecordingPrepare {
    inner: HostPrepare,
    record: Arc<Mutex<AfterRecord>>,
}

impl Prepare for RecordingPrepare {
    fn prepare(
        &self,
        services: &Services,
        request: &StartRequest,
        ticket: &Arc<earshot_runtime::meeting_queue::QueueTicket>,
    ) -> Result<Prepared, IpcError> {
        let prepared = self.inner.prepare(services, request, ticket)?;
        let record = Arc::clone(&self.record);
        let after = prepared.after;
        Ok(Prepared {
            start: prepared.start,
            notices: prepared.notices,
            call_signal: prepared.call_signal,
            after: Box::new(
                move |svc: &Services,
                      outcome: &LiveOutcome,
                      cancel: &earshot_core::asr::CancelToken,
                      progress: &mut dyn FnMut(HostStep<'_>)| {
                    let host = after(svc, outcome, cancel, progress);
                    let mut r = record.lock().unwrap_or_else(PoisonError::into_inner);
                    r.network = network_line(&host);
                    if let SummaryResult::Stored { outcome, .. } = &host.summary
                        && let SummaryRunReport::Local(local) = &outcome.report
                    {
                        r.summary_backend = Some(local.backend.name().to_owned());
                    }
                    if let Ok(HostPost::Done(done)) = &host.post {
                        r.final_backend = Some(backend_label(done.backend.kind).to_owned());
                    }
                    host
                },
            ),
        })
    }
}

fn ipc_text(e: &IpcError) -> String {
    format!("{} ({})", e.message, e.kind)
}

/// Stores the settings a user would choose on the pages.
fn store_settings(services: &Services, args: &Args) -> Result<(), String> {
    {
        let mut db = services.store();
        model_settings::set_model_settings(
            &mut db,
            &ModelSettings {
                transcription: ModelPick::Named(args.model.clone()),
                ..ModelSettings::default()
            },
        )
        .map_err(|e| format!("the model settings were refused: {e}"))?;
        let summary = match &args.llm_url {
            Some(url) => SummarySettings {
                writer: SummaryWriter::OwnEndpoint,
                provider: EndpointProvider::OpenAiCompatible,
                url: url.clone(),
                model: ENDPOINT_MODEL.into(),
                ..SummarySettings::default()
            },
            None => SummarySettings {
                writer: SummaryWriter::OnThisComputer,
                ..SummarySettings::default()
            },
        };
        summary_settings::set_summary_settings(&mut db, &summary)
            .map_err(|e| format!("the summary settings were refused: {e}"))?;
    }
    let vault = args
        .vault
        .to_str()
        .ok_or_else(|| "--vault is not valid text".to_owned())?;
    earshot_runtime::export_settings::set_obsidian_settings(
        services,
        vault,
        earshot_runtime::export_settings::DEFAULT_FOLDER,
    )
    .map_err(|e| format!("the Obsidian settings were refused: {e}"))?;
    Ok(())
}

/// A printed name for `track` (the lines `accept m6` reads).
fn track_name(track: LiveTrack) -> &'static str {
    match track {
        LiveTrack::You => "mic",
        LiveTrack::Them => "reference",
    }
}

/// Tries `dir` in a child, then runs from it if that worked; what happened, as one line.
fn use_llama_folder(dir: &Path, models: &earshot_runtime::backends::ModelDir) -> String {
    use crate::llama_probe::{Verdict, try_folder};
    const STAY: &str = "staying beside the exe";
    let Some(model) = earshot_runtime::host::smallest_installed_summary_model(models) else {
        return format!("llama folder: refused (no summary model to try it with); {STAY}");
    };
    match try_folder(dir, &model) {
        Verdict::Refused { why } => format!("llama folder: refused ({why}); {STAY}"),
        Verdict::Usable { backend } => match earshot_runtime::host::load_llama_folder(dir) {
            Ok(()) => format!("llama folder: tried on {backend}, loaded"),
            Err(e) => format!("llama folder: refused ({e}); {STAY}"),
        },
    }
}

/// Tries the CUDA transcription folder in a child, then chooses it at start if that worked; what
/// happened, as one line.
fn use_cuda_transcription(models: &ModelDir) -> String {
    use crate::llama_probe::Verdict;
    const STAY: &str = "staying off CUDA";
    match crate::transcription_probe::try_transcription_folder(models) {
        Verdict::Refused { why } => format!("cuda transcription: refused ({why}); {STAY}"),
        Verdict::Usable { backend } => {
            match earshot_runtime::cuda::use_for_transcription(models, true) {
                Ok(()) => format!("cuda transcription: tried on {backend}, chosen"),
                Err(e) => format!("cuda transcription: refused ({e}); {STAY}"),
            }
        }
    }
}

/// Stores both switches as the page would, runs the app's start over the real files and prints
/// what the Models page would then say (backends from evidence, notes in full; never a path).
fn begin_cuda_switch(services: &Services) -> Result<Arc<crate::cuda_settings::CudaLatch>, String> {
    use earshot_runtime::cuda_switch::{CudaSwitch, set_cuda_switch};
    let both = CudaSwitch {
        summaries: true,
        transcription: true,
    };
    set_cuda_switch(&mut services.store(), both).map_err(|e| e.to_string())?;
    let files = crate::cuda_files::ManifestCudaFiles::new(Arc::clone(&services.clock));
    Ok(crate::cuda_settings::begin(services, files, || {}))
}

/// What the start found and what the Models page says about it.
fn report_cuda_switch(
    services: &Services,
    latch: &crate::cuda_settings::CudaLatch,
    began: Instant,
    out: &mut dyn Write,
) -> Result<(), String> {
    let started = began;
    let start = latch.wait();
    let mut line = |text: String| writeln!(out, "{text}").map_err(|e| format!("cannot print: {e}"));
    line(format!(
        "cuda start: {:.1} s, summaries {:?}, transcription {:?}",
        started.elapsed().as_secs_f64(),
        start.summaries,
        start.transcription
    ))?;
    let models = earshot_runtime::host::model_dir(&services.data);
    let order = earshot_runtime::backends::accelerator_order();
    let mut view = crate::models::models_view(
        services,
        &models,
        &earshot_runtime::hardware(),
        &order,
        &|_| false,
    )
    .map_err(|e| ipc_text(&e))?;
    crate::cuda_settings::apply_start(&mut view, &start);
    line(format!(
        "in use: summaries {:?}, transcription {:?}",
        view.in_use.summary_runs_on, view.in_use.transcription_runs_on
    ))?;
    for note in &view.in_use.notes {
        line(format!("note: {note}"))?;
    }
    // The check the app runs after this start, by its own step (`check_once_started`: the same
    // decision, gate and model jobs), recorded as the app records it: does the parent read its
    // answer?
    let (events, heard) = std::sync::mpsc::channel();
    let events = std::sync::Mutex::new(events);
    let checks = crate::model_jobs::ModelJobs::new(
        Arc::new(crate::model_jobs::HostModelWork),
        Arc::new(move |e| {
            let _ = events
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .send(e);
        }),
        crate::activity::WorkGate::for_app(),
    );
    let refusals = Arc::new(std::sync::Mutex::new(Vec::new()));
    let sink: crate::cuda_settings::CudaSink = {
        let refusals = Arc::clone(&refusals);
        Arc::new(move |e| {
            if let crate::cuda_settings::CudaEvent::Warning { message, .. } = e {
                refusals
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .push(message);
            }
        })
    };
    if !crate::cuda_settings::check_once_started(latch, services, &checks, &sink) {
        let refused = refusals
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        line(match refused.first() {
            Some(why) => format!("model check: not started ({why})"),
            None => "model check: not needed for this run".into(),
        })?;
        return Ok(());
    }
    let end = loop {
        use crate::model_jobs::ModelJobEvent as E;
        match heard.recv() {
            Ok(e @ (E::Checked { .. } | E::Failed { .. } | E::CheckGaveWay)) => break e,
            Ok(_) => {}
            Err(_) => return Err("the model check ended without a result".into()),
        }
    };
    let (_, current) =
        crate::cuda_settings::check_state(services, &models).map_err(|e| ipc_text(&e))?;
    line(format!(
        "model check: {end:?}, current for this run: {current}"
    ))?;
    Ok(())
}

/// Where the speech helpers' sessions were placed, loaded as a meeting loads them.
fn helper_placement(models: &ModelDir) -> Vec<String> {
    use earshot_runtime::backends::{load_diarisation, load_silero};
    use earshot_runtime::host::ONNX_ACCELERATOR;
    let vad = match load_silero(models, ONNX_ACCELERATOR) {
        Ok(vad) => format!("silero: {}", vad.backend()),
        Err(e) => format!("silero: {e}"),
    };
    let speakers = match load_diarisation(models, ONNX_ACCELERATOR) {
        Ok((segmenter, embedder)) => vec![
            format!("segmentation: {}", segmenter.backend()),
            format!("cam++: {}", embedder.backend()),
        ],
        Err(e) => vec![format!("speakers: {e}")],
    };
    std::iter::once(vad).chain(speakers).collect()
}

/// The meeting, its summary and its export.
fn run(args: &Args, out: &mut dyn Write) -> Result<(), String> {
    let own = DataDir::from_local_app_data().map(|d| resolved(d.root()));
    check_scratch(&resolved(&args.data), own.as_deref())?;
    check_vault(&args.vault)?;
    let data = DataDir::new(&args.data);
    // Before any llama call. A refusal stays on the set beside the exe, and says so.
    if let Some(dir) = &args.llama_dir {
        let line = use_llama_folder(dir, &earshot_runtime::backends::ModelDir::from_env(&data));
        writeln!(out, "{line}").map_err(|e| format!("cannot print: {e}"))?;
    }
    if args.cuda_transcription {
        // Before anything loads ONNX Runtime. A refusal stays off CUDA, and says so.
        let line = use_cuda_transcription(&earshot_runtime::host::model_dir(&data));
        writeln!(out, "{line}").map_err(|e| format!("cannot print: {e}"))?;
    }
    crate::crash::install(crate::crash::logs_dir(data.root()));
    let services = AppState::services_in(&data).map_err(|e| ipc_text(&e))?;
    store_settings(&services, args)?;
    let cuda_began = Instant::now();
    let cuda_latch = if args.cuda_switch {
        Some(begin_cuda_switch(&services)?)
    } else {
        None
    };
    if !args.cuda_meeting_first
        && let Some(latch) = &cuda_latch
    {
        report_cuda_switch(&services, latch, cuda_began, out)?;
    }

    let record = Arc::new(Mutex::new(AfterRecord::default()));
    let dir = args.replay.clone();
    let source_record = Arc::clone(&record);
    let prepare = RecordingPrepare {
        inner: HostPrepare::with_source(Box::new(move |_, _| {
            replay_source(&dir, Arc::clone(&source_record))
        })),
        record: Arc::clone(&record),
    };
    let (tx, rx) = mpsc::channel::<MeetingEvent>();
    let tx = Mutex::new(tx);
    let sink: Sink = Arc::new(move |e: MeetingEvent| {
        // Only what the report prints; text stays out.
        let reported = matches!(
            e,
            MeetingEvent::Backend { .. } | MeetingEvent::Summary { .. }
        );
        if reported {
            let _ = tx.lock().unwrap_or_else(PoisonError::into_inner).send(e);
        }
    });
    // Without the switch nothing was begun: CUDA is off for this run.
    let cuda_off = crate::cuda_settings::CudaLatch::resolved_off();
    let driver = MeetingDriver::new(
        Arc::new(prepare),
        sink,
        Arc::new(Instant::now),
        WorkGate::new(),
        cuda_latch.as_deref().unwrap_or(&cuda_off),
    );
    let meeting = driver
        .start(
            &services,
            StartRequest {
                mode: CaptureMode::Call,
                title: String::new(),
                keep_local: false,
            },
        )
        .map_err(|e| ipc_text(&e))?;
    let write = |out: &mut dyn Write, line: &str| {
        writeln!(out, "{line}").map_err(|e| format!("cannot print: {e}"))
    };
    write(out, &format!("meeting: {meeting}"))?;
    if args.cuda_meeting_first {
        write(
            out,
            &format!(
                "meeting started {:.2} s after the CUDA start began, which had finished: {}",
                cuda_began.elapsed().as_secs_f64(),
                cuda_latch.as_ref().is_some_and(|l| !l.current().checking()),
            ),
        )?;
    }
    let result = loop {
        match rx.recv_timeout(MEETING_DEADLINE) {
            Ok(MeetingEvent::Backend { track, label }) => {
                write(out, &format!("live asr {}: {label}", track_name(track)))?
            }
            Ok(MeetingEvent::Summary { result, .. }) => break result,
            Ok(_) => {}
            Err(e) => return Err(format!("the meeting did not finish: {e}")),
        }
    };
    let finished = {
        let mut r = record.lock().unwrap_or_else(PoisonError::into_inner);
        std::mem::take(&mut *r)
    };
    for line in &finished.live_placement {
        write(out, &format!("live placement {line}"))?;
    }
    if let Some(label) = &finished.final_backend {
        write(out, &format!("post: final pass ran on {label}"))?;
    }
    for line in &finished.network {
        write(out, line)?;
    }
    if let Some(backend) = &finished.summary_backend {
        write(out, &format!("summary backend: {backend}"))?;
    }
    match result {
        SummaryEvent::Stored { .. } => write(out, "summary: stored")?,
        other => return Err(format!("no summary was stored: {other:?}")),
    }
    // The meeting thread holds a `Services` clone: joined, so its last store handle closes.
    driver.join();
    if args.cuda_meeting_first
        && let Some(latch) = &cuda_latch
    {
        report_cuda_switch(&services, latch, cuda_began, out)?;
    }
    if args.cuda_transcription || args.cuda_switch {
        for line in helper_placement(&earshot_runtime::host::model_dir(&data)) {
            write(out, &format!("helper placement {line}"))?;
        }
    }
    let id = crate::detail::parse_id(&meeting).map_err(|e| ipc_text(&e))?;
    crate::detail::export(&services, id).map_err(|e| ipc_text(&e))?;
    // Not the note's path: it holds the summary's title.
    write(out, "exported")?;
    Ok(())
}

#[cfg(test)]
#[path = "headless_tests.rs"]
mod tests;
