//! The Benchmark section of Settings (T7.4c): like a game's benchmark, the
//! current settings (transcription model and where it runs, summary model and where it
//! runs, summarising during the meeting, speaker detection) run against a short meeting
//! shipped with the app, and the timings are shown in plain words next to what the model
//! check found (`earshot_runtime::benchmark`).
//!
//! The meeting is an AMI excerpt ([`MEETING_ATTRIBUTION`], `resources/benchmark/`, made by
//! `cargo xtask bench-meeting`), embedded in the exe. The run replays it in real time
//! through the production replay path on a throwaway store under
//! `<data dir>\benchmark\` (deleted afterwards on every path); nothing from it is stored as
//! a meeting and nothing goes over the network. Only the latest result is kept (device
//! record `benchmark`: timings, settings and backends, never content).
//!
//! The run happens in the app's process, as a meeting does (it loads exactly what a meeting
//! loads), on a thread of its own ([`BenchmarkJobs`]) holding a
//! [`crate::activity::BenchmarkPass`] for its whole life, so it never overlaps a meeting, a
//! meeting job, the model check, the voice sample or the audio check (its timings would be
//! distorted, or theirs). It sends [`BenchmarkEvent`]s: `Progress`, then exactly one of
//! `Done`, `Cancelled` or `Failed` (a panic in the work becomes `Failed`). Tests drive it
//! through [`BenchmarkWork`] fakes.

use std::panic::AssertUnwindSafe;
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::JoinHandle;

use earshot_core::asr::CancelToken;
use earshot_runtime::benchmark::{
    self as bench, BenchSkip, BenchmarkProgress, BenchmarkRecord, LiveSummariesRun, RecordBackend,
    RecordSummary,
};
use earshot_runtime::catalogue::display_name;
use earshot_runtime::model_bench::StoredVerdict;
use earshot_runtime::speaker_timing::SpeakerTiming;
use earshot_runtime::{RuntimeError, Services};
use serde::Serialize;
use specta::Type;
use tauri_specta::Event;

use crate::activity::WorkGate;
use crate::ipc::IpcError;

/// The benchmark meeting's mic track (the Project Manager's headset).
const MIC_FLAC: &[u8] = include_bytes!("../resources/benchmark/mic.flac");
/// The benchmark meeting's reference track (the other three headsets).
const REFERENCE_FLAC: &[u8] = include_bytes!("../resources/benchmark/reference.flac");

/// The meeting's attribution (CC BY 4.0; `resources/benchmark/NOTICE.md`).
pub const MEETING_ATTRIBUTION: &str = "The benchmark meeting is an excerpt (9:30 to 12:00) of \
meeting ES2004a of the AMI Meeting Corpus, University of Edinburgh and the AMI Consortium, \
licensed under CC BY 4.0 (groups.inf.ed.ac.uk/ami/corpus).";

/// The bundled meeting.
///
/// # Errors
/// The embedded files do not decode (a broken build).
pub fn bundled_meeting() -> Result<bench::BenchmarkMeeting, RuntimeError> {
    bench::BenchmarkMeeting::from_flac(MIC_FLAC, REFERENCE_FLAC)
}

/// One line of a result: what, and its value in words.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Type)]
pub struct BenchmarkRow {
    /// What it is ("Final transcript").
    pub label: String,
    /// The value in words ("Ready 9 seconds after the meeting ended").
    pub value: String,
}

/// A stored result as the page shows it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Type)]
pub struct BenchmarkResultView {
    /// When it ran (Unix ms).
    pub ran_at_ms: u64,
    /// The app version that ran it.
    pub app_version: String,
    /// Live transcription kept up.
    pub kept_up: bool,
    /// The result in one sentence.
    pub headline: String,
    /// The settings it ran with.
    pub settings: Vec<BenchmarkRow>,
    /// What it measured, in order.
    pub timings: Vec<BenchmarkRow>,
    /// What the user is told about the run's settings (a part not used, a stored setting this
    /// version does not know, a library that could not load).
    pub notes: Vec<String>,
}

/// The Benchmark page.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Type)]
pub struct BenchmarkView {
    /// The latest result (`null`: none yet).
    pub last: Option<BenchmarkResultView>,
    /// A stored result this version cannot read (the next run replaces it).
    pub unreadable: bool,
    /// The benchmark meeting's length (ms).
    pub meeting_ms: u64,
    /// The meeting's attribution.
    pub attribution: String,
    /// The run under way (`null`: none), so a page opened during it shows it.
    pub running: Option<BenchmarkRunning>,
}

/// A run under way.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Type)]
pub struct BenchmarkRunning {
    /// The phase.
    pub phase: BenchmarkPhase,
    /// Replayed so far (ms).
    pub heard_ms: u64,
    /// The meeting's length (ms).
    pub total_ms: u64,
}

/// Where a run is, for the page.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Type)]
pub enum BenchmarkPhase {
    /// The meeting is being played to Earshot.
    Replaying,
    /// The meeting ended; the live transcript is being finished.
    FinishingLive,
    /// The final transcript is being made.
    Transcribing,
    /// The summary is being written.
    Summarising,
}

/// Progress and the end of a benchmark run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Type, Event)]
#[serde(tag = "type")]
pub enum BenchmarkEvent {
    /// Where the run is; `heard_ms` of `total_ms` of the meeting replayed so far.
    Progress {
        /// The phase.
        phase: BenchmarkPhase,
        /// Replayed so far (ms).
        heard_ms: u64,
        /// The meeting's length (ms).
        total_ms: u64,
    },
    /// Finished (terminal): the result, stored.
    Done {
        /// The page now.
        view: BenchmarkView,
    },
    /// Cancelled; nothing was stored (terminal).
    Cancelled,
    /// Failed; nothing was stored (terminal).
    Failed {
        /// Why, in the page's words.
        message: String,
    },
}

fn ms_u64(d: std::time::Duration) -> u64 {
    u64::try_from(d.as_millis()).unwrap_or(u64::MAX)
}

/// "4.2 seconds" (one decimal under 10 s, whole seconds above; minutes past 2 minutes),
/// rounded once so the words match the value.
#[must_use]
pub fn seconds_text(ms: u64) -> String {
    let tenths = (ms + 50) / 100;
    if tenths < 100 {
        if tenths == 10 {
            return "1 second".into();
        }
        return format!("{}.{} seconds", tenths / 10, tenths % 10);
    }
    let s = (ms + 500) / 1000;
    if s >= 120 {
        return format!("{} min {} s", s / 60, s % 60);
    }
    format!("{s} seconds")
}

/// Where a backend ran, in words ("the processor", "the graphics card (NVIDIA ...)").
#[must_use]
pub fn part_text(b: &RecordBackend) -> String {
    match b.kind.as_str() {
        "cpu" => "the processor".into(),
        "cuda" | "vulkan" if b.device.is_empty() || b.device == "cpu" => "the graphics card".into(),
        "cuda" | "vulkan" => format!("the graphics card ({})", b.device),
        "qnn" => "the NPU".into(),
        other => other.to_owned(),
    }
}

/// Where llama.cpp ran a summary (its backend name), in words.
fn llama_part_text(name: &str) -> String {
    match name {
        "cpu" => "the processor".into(),
        "cuda" | "vulkan" | "metal" => "the graphics card".into(),
        other => other.to_owned(),
    }
}

/// The parts live transcription ran on, in words (one when both tracks agree).
fn live_parts(r: &BenchmarkRecord) -> String {
    let mut parts: Vec<String> = r.live_backends.iter().map(part_text).collect();
    parts.dedup();
    match parts.as_slice() {
        [] => "not known".into(),
        [one] => one.clone(),
        many => many.join(" and "),
    }
}

fn row(label: &str, value: impl Into<String>) -> BenchmarkRow {
    BenchmarkRow {
        label: label.into(),
        value: value.into(),
    }
}

/// The page's words for a stored result.
#[must_use]
pub fn result_view(r: &BenchmarkRecord) -> BenchmarkResultView {
    let model = display_name(&r.settings.transcription_model);
    let live = live_parts(r);
    let mut settings = vec![row("Transcription", format!("{model} on {live}"))];
    settings.push(row(
        "Summary",
        match &r.summary {
            RecordSummary::Stored { model, backend, .. } => {
                format!("{} on {}", display_name(model), llama_part_text(backend))
            }
            RecordSummary::Skipped { reason } => match reason {
                BenchSkip::NotRequested => "None (Summaries is set to no summary)".into(),
                BenchSkip::NotInstalled { model } => format!(
                    "None ({} is not downloaded; see Models)",
                    display_name(model)
                ),
                BenchSkip::NotOnThisComputer { .. } => "Not run: summaries are written \
                    outside this computer, and the benchmark makes no network connection"
                    .into(),
            },
        },
    ));
    settings.push(row(
        "Summarising during the meeting",
        match &r.settings.live_summaries {
            LiveSummariesRun::Off => "Off".to_owned(),
            LiveSummariesRun::On => "On".to_owned(),
            LiveSummariesRun::NotRun => {
                "On, but not run: it needs a summary on this computer".to_owned()
            }
            // The runtime's reason, as the meeting banner shows it.
            LiveSummariesRun::Stopped { reason } => format!(
                "{}.",
                crate::meeting_events::sentence_start(reason.trim_end_matches('.'))
            ),
        },
    ));
    settings.push(row(
        "Speaker detection",
        SpeakerTiming::parse(&r.settings.speaker_timing).map_or_else(
            || r.settings.speaker_timing.clone(),
            |t| t.label().to_owned(),
        ),
    ));

    let mut timings = Vec::new();
    timings.push(row(
        "Transcript lag during the meeting (both tracks)",
        match (r.lag_max_ms, r.lag_median_ms) {
            (Some(max), Some(median)) => format!(
                "Usually {} behind, at most {}",
                seconds_text(median),
                seconds_text(max)
            ),
            _ => "Not measured".into(),
        },
    ));
    timings.push(row(
        "Live transcript",
        if r.kept_up {
            format!(
                "Kept up: complete {} after the meeting ended",
                seconds_text(r.catch_up_ms)
            )
        } else {
            format!(
                "Fell behind: complete {} after the meeting ended",
                seconds_text(r.catch_up_ms)
            )
        },
    ));
    let final_part = r
        .final_backend
        .as_ref()
        .map(|b| format!(" on {}", part_text(b)))
        .unwrap_or_default();
    timings.push(row(
        "Final transcript",
        format!(
            "Ready {} after the meeting ended{final_part}",
            seconds_text(r.transcript_ready_ms)
        ),
    ));
    timings.push(row(
        "Summary",
        match &r.summary {
            RecordSummary::Stored { ready_ms, .. } => {
                format!("Ready {} after the meeting ended", seconds_text(*ready_ms))
            }
            RecordSummary::Skipped { .. } => "Not run".into(),
        },
    ));
    timings.push(row(
        "Model check",
        match r.check {
            Some(StoredVerdict::Pass) => format!("Found {model} fast enough on {live}"),
            Some(StoredVerdict::TooSlow) => format!("Found {model} too slow on {live}"),
            Some(StoredVerdict::Wrong) => format!("Found that {model} does not work on {live}"),
            None => format!("Has not tried {model} on {live}"),
        },
    ));
    let headline = match (&r.summary, r.kept_up) {
        (RecordSummary::Stored { ready_ms, .. }, true) => format!(
            "Live transcription kept up, and the summary was ready {} after the meeting ended.",
            seconds_text(*ready_ms)
        ),
        (RecordSummary::Skipped { .. }, true) => "Live transcription kept up.".into(),
        (_, false) => "Live transcription fell behind. A lighter model, or another part on the \
            Models page, may keep up."
            .into(),
    };
    BenchmarkResultView {
        ran_at_ms: r.ran_at_ms,
        app_version: r.app_version.clone(),
        kept_up: r.kept_up,
        headline,
        settings,
        timings,
        notes: r.notes.clone(),
    }
}

/// The page: the latest stored result and the run under way.
///
/// # Errors
/// Store errors; the embedded meeting does not decode.
pub fn benchmark_view(
    services: &Services,
    running: Option<BenchmarkRunning>,
) -> Result<BenchmarkView, IpcError> {
    let stored = bench::stored_benchmark(&services.store())?;
    Ok(BenchmarkView {
        last: stored.record.as_ref().map(result_view),
        unreadable: stored.unreadable,
        meeting_ms: meeting_ms()?,
        attribution: MEETING_ATTRIBUTION.into(),
        running,
    })
}

/// The bundled meeting's length (ms), decoded once.
fn meeting_ms() -> Result<u64, RuntimeError> {
    static LENGTH: std::sync::OnceLock<Result<u64, &'static str>> = std::sync::OnceLock::new();
    (*LENGTH.get_or_init(|| {
        bundled_meeting()
            .map(|m| ms_u64(m.duration()))
            .map_err(|_| "the benchmark meeting does not decode")
    }))
    .map_err(RuntimeError::Internal)
}

/// The work of a run (the installed models, or fakes in tests).
pub trait BenchmarkWork: Send + Sync {
    /// Runs the benchmark with the settings of `services` and stores the result there;
    /// `None` when cancelled (nothing stored). Runs on the benchmark thread.
    ///
    /// # Errors
    /// As `earshot_runtime::benchmark::run_from_settings`.
    fn run(
        &self,
        services: &Services,
        cancel: &CancelToken,
        progress: &mut dyn FnMut(BenchmarkProgress),
    ) -> Result<Option<BenchmarkRecord>, RuntimeError>;
}

/// Where benchmark events go.
pub type BenchmarkSink = Arc<dyn Fn(BenchmarkEvent) + Send + Sync>;

struct Running {
    cancel: CancelToken,
    thread: JoinHandle<()>,
}

struct Inner {
    work: Arc<dyn BenchmarkWork>,
    sink: BenchmarkSink,
    gate: WorkGate,
    slot: Mutex<Option<Running>>,
    /// Where the run under way is (`None`: none).
    current: Mutex<Option<BenchmarkRunning>>,
}

/// Runs at most one benchmark (cheap to clone; clones share it).
#[derive(Clone)]
pub struct BenchmarkJobs(Arc<Inner>);

/// What a run that panicked reports.
pub const BENCHMARK_PANIC_MESSAGE: &str = "The benchmark stopped unexpectedly.";

impl BenchmarkJobs {
    /// Jobs of `work`, sending events to `sink`; a run holds a benchmark pass of `gate`.
    pub fn new(work: Arc<dyn BenchmarkWork>, sink: BenchmarkSink, gate: WorkGate) -> Self {
        Self(Arc::new(Inner {
            work,
            sink,
            gate,
            slot: Mutex::new(None),
            current: Mutex::new(None),
        }))
    }

    fn slot(&self) -> std::sync::MutexGuard<'_, Option<Running>> {
        self.0.slot.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Where the run under way is, for a page opened during it.
    #[must_use]
    pub fn current(&self) -> Option<BenchmarkRunning> {
        *self
            .0
            .current
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// Starts a run (events follow).
    ///
    /// # Errors
    /// `busy` (retryable) while a run, a meeting, a meeting job, the model check, the voice
    /// sample or the audio check runs.
    pub fn start(&self, services: &Services) -> Result<(), IpcError> {
        let mut slot = self.slot();
        if let Some(old) = slot.take() {
            if !old.thread.is_finished() {
                *slot = Some(old);
                return Err(IpcError {
                    kind: "busy".into(),
                    message: crate::activity::BENCHMARK_ALREADY_RUNNING.into(),
                    retryable: true,
                });
            }
            let _ = old.thread.join();
        }
        let pass = self.0.gate.enter_benchmark()?;
        *self
            .0
            .current
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(BenchmarkRunning {
            phase: BenchmarkPhase::Replaying,
            heard_ms: 0,
            total_ms: meeting_ms().unwrap_or(0),
        });
        let cancel = CancelToken::new();
        let inner = Arc::clone(&self.0);
        let svc = services.clone();
        let token = cancel.clone();
        let thread = std::thread::Builder::new()
            .name("earshot-benchmark".into())
            .spawn(move || {
                let event =
                    std::panic::catch_unwind(AssertUnwindSafe(|| run(&inner, &svc, &token)))
                        .unwrap_or_else(|_| BenchmarkEvent::Failed {
                            message: BENCHMARK_PANIC_MESSAGE.into(),
                        });
                drop(pass);
                *inner.current.lock().unwrap_or_else(PoisonError::into_inner) = None;
                (inner.sink)(event);
            })
            .map_err(|e| IpcError {
                kind: "internal".into(),
                message: format!("cannot start the benchmark thread: {e}"),
                retryable: false,
            })?;
        *slot = Some(Running { cancel, thread });
        Ok(())
    }

    /// Cancels the running benchmark (a `Cancelled` follows unless it already ended).
    pub fn cancel(&self) {
        if let Some(r) = self.slot().as_ref() {
            r.cancel.cancel();
        }
    }

    /// Waits for the running benchmark.
    #[cfg(test)]
    pub fn join(&self) {
        if let Some(r) = self.slot().take() {
            let _ = r.thread.join();
        }
    }
}

/// A run, to its terminal event (returned, sent by the caller after the pass is released).
fn run(inner: &Inner, svc: &Services, cancel: &CancelToken) -> BenchmarkEvent {
    let sink = Arc::clone(&inner.sink);
    let mut progress = |p: BenchmarkProgress| {
        let mut current = inner.current.lock().unwrap_or_else(PoisonError::into_inner);
        let total = current.map_or(0, |c| c.total_ms);
        let now = match p {
            BenchmarkProgress::Replaying { heard, total } => BenchmarkRunning {
                phase: BenchmarkPhase::Replaying,
                heard_ms: ms_u64(heard),
                total_ms: ms_u64(total),
            },
            BenchmarkProgress::FinishingLive => after_replay(BenchmarkPhase::FinishingLive, total),
            BenchmarkProgress::Transcribing => after_replay(BenchmarkPhase::Transcribing, total),
            BenchmarkProgress::Summarising => after_replay(BenchmarkPhase::Summarising, total),
        };
        *current = Some(now);
        drop(current);
        sink(BenchmarkEvent::Progress {
            phase: now.phase,
            heard_ms: now.heard_ms,
            total_ms: now.total_ms,
        });
    };
    match inner.work.run(svc, cancel, &mut progress) {
        Ok(None) => BenchmarkEvent::Cancelled,
        Ok(Some(_)) => match benchmark_view(svc, None) {
            Ok(view) => BenchmarkEvent::Done { view },
            Err(_) => BenchmarkEvent::Failed {
                message: STORE_FAILED.to_owned(),
            },
        },
        Err(e) => BenchmarkEvent::Failed {
            message: failure_copy(&e),
        },
    }
}

/// A phase after the replay: the whole meeting has been played.
fn after_replay(phase: BenchmarkPhase, total_ms: u64) -> BenchmarkRunning {
    BenchmarkRunning {
        phase,
        heard_ms: total_ms,
        total_ms,
    }
}

/// The store could not be read or written.
pub const STORE_FAILED: &str =
    "Earshot could not save the benchmark to its database. Run the benchmark again.";

/// A model is not downloaded.
pub const MODEL_MISSING: &str = "A model these settings use is not downloaded. Download it on the \
    Models page, then run the benchmark again.";
/// The settings cannot run here.
pub const CANNOT_RUN_HERE: &str = "These settings cannot run on this computer. Choose another \
    model or part on the Models page, then run the benchmark again.";
/// Transcribing failed.
pub const TRANSCRIPTION_FAILED: &str =
    "Transcribing the benchmark meeting failed. Run the benchmark again.";
/// Speaker detection failed.
pub const SPEAKERS_FAILED: &str =
    "Finding the speakers of the benchmark meeting failed. Run the benchmark again.";
/// The summary failed.
pub const SUMMARY_FAILED: &str =
    "Summarising the benchmark meeting failed. Run the benchmark again.";
/// Anything else.
pub const OTHER_FAILED: &str = "The benchmark did not finish. Run it again.";

/// The page's words for a failed run: fixed copy for every failure, so no error text (a model
/// id, a path) reaches the page.
#[must_use]
pub fn failure_copy(e: &RuntimeError) -> String {
    match e {
        RuntimeError::Store(_) | RuntimeError::Repo(_) => STORE_FAILED,
        RuntimeError::ModelMissing { .. }
        | RuntimeError::ModelIncomplete { .. }
        | RuntimeError::UnknownModel(_) => MODEL_MISSING,
        RuntimeError::Unavailable(_) | RuntimeError::NoBackend(_) => CANNOT_RUN_HERE,
        RuntimeError::Asr(_) | RuntimeError::Onnx(_) => TRANSCRIPTION_FAILED,
        RuntimeError::Diar(_) | RuntimeError::Uncalibrated { .. } => SPEAKERS_FAILED,
        RuntimeError::Llm(_) | RuntimeError::LocalSummary(_) => SUMMARY_FAILED,
        _ => OTHER_FAILED,
    }
    .to_owned()
}

/// The production work: the installed models, the bundled meeting in real time.
pub struct HostBenchmarkWork;

impl BenchmarkWork for HostBenchmarkWork {
    fn run(
        &self,
        services: &Services,
        cancel: &CancelToken,
        progress: &mut dyn FnMut(BenchmarkProgress),
    ) -> Result<Option<BenchmarkRecord>, RuntimeError> {
        let meeting = bundled_meeting()?;
        let models = earshot_runtime::host::model_dir(&services.data);
        let scratch = services.data.root().join("benchmark");
        bench::run_from_settings(
            services,
            &models,
            &meeting,
            &earshot_runtime::host::dll_search_dirs(),
            &scratch,
            env!("CARGO_PKG_VERSION"),
            cancel,
            progress,
        )
    }
}

/// The Benchmark page: the latest result, the meeting's length and the run under way.
#[tauri::command]
#[specta::specta]
pub async fn benchmark(
    state: tauri::State<'_, crate::ipc::AppState>,
    jobs: tauri::State<'_, BenchmarkJobs>,
) -> Result<BenchmarkView, IpcError> {
    let services = state.services()?;
    let running = jobs.current();
    tauri::async_runtime::spawn_blocking(move || benchmark_view(&services, running))
        .await
        .map_err(|_| IpcError {
            kind: "internal".into(),
            message: "the task stopped".into(),
            retryable: false,
        })?
}

/// Starts the benchmark with the current settings (`BenchmarkEvent`s follow). `busy` while a
/// meeting, a meeting job, the model check, the voice sample, the audio check or another
/// benchmark runs.
#[tauri::command]
#[specta::specta]
pub fn start_benchmark(
    state: tauri::State<'_, crate::ipc::AppState>,
    jobs: tauri::State<'_, BenchmarkJobs>,
) -> Result<(), IpcError> {
    let services = state.services()?;
    jobs.start(&services)
}

/// Cancels the benchmark; nothing is stored.
#[tauri::command]
#[specta::specta]
pub fn cancel_benchmark(jobs: tauri::State<'_, BenchmarkJobs>) {
    jobs.cancel();
}

#[cfg(test)]
#[path = "benchmark_tests.rs"]
mod tests;
