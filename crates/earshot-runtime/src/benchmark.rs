//! The benchmark in Settings (T7.4c): like a game's benchmark, the
//! current settings run against a short bundled meeting and the timings are shown next to
//! what the model check found.
//!
//! - **Meeting** ([`BenchmarkMeeting`]): two 16 kHz mono FLAC tracks the host bundles (the
//!   desktop app ships an AMI excerpt made by `cargo xtask bench-meeting`), replayed as a
//!   call: the mic track and the reference ("the other side of the call").
//! - **Settings** ([`benchmark_setup`]): read from the user's store and never changed, through
//!   the functions a meeting uses: [`host::transcription_setup`] (model and where it runs),
//!   [`host::summary_from_settings`] and [`host::summary_plan`] (the local summary model and
//!   where it runs), the speaker detection setting and "summarise during the meeting". No
//!   network: a summary written by the user's endpoint, Claude Code or Codex is skipped and
//!   said so ([`BenchSkip::NotOnThisComputer`]); no API key is read.
//! - **Run** ([`run_benchmark`]): the production replay path on a throwaway [`Services`] in a
//!   folder of its own under the host's scratch folder (its own store and audio), deleted on
//!   every path, failure and cancel included ([`Scratch`]). The replay is paced in real time
//!   ([`crate::replay::Paced`], as `earshot-cli replay --realtime`); the live session runs with the meeting's
//!   live transcription ([`host::live_factory`]), speaker timing and live summaries, then the
//!   final pass and the summary ([`host::after_meeting`]) with the user's summary style copied
//!   into the throwaway store. Nothing is written to the user's store except the result
//!   ([`record_benchmark`]). A run that made a network request or handed text to another
//!   program is an internal error (second line of defence).
//! - **Measurements** ([`BenchmarkRecord`]): the largest and median transcript lag while the
//!   replay ran, over both tracks' reports ([`RuntimeEvent::TranscriptLag`]), how long after
//!   the replay ended the live transcript was complete
//!   ([`RuntimeEvent::LiveTranscriptFinished`], so the waits for the speaker and summary
//!   threads and finalising the recording do not count) and whether live transcription kept
//!   up ([`CATCH_UP_LIMIT`]), what "summarise during the meeting" actually did
//!   ([`LiveSummariesRun`]),
//!   the time from the replay's end to the final transcript and to the stored summary, and
//!   the backend each step reported (from its evidence: [`RuntimeEvent::LiveBackend`], the
//!   final pass's recorded backend, the summary's report; never the request). The model
//!   check's verdict for the same model and backend ([`StoredCheck::verdict`]) is kept beside
//!   them. Never content: no transcript or summary text.
//!
//! It runs in the host's process, like a meeting (it loads exactly the backends a meeting
//! loads, so ONNX Runtime's one library per process and whisper's process-wide evidence log
//! are no different from a meeting's); the host keeps it from running beside a meeting, a
//! meeting job or the model check.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::RecvTimeoutError;
use std::time::Duration;

use earshot_core::asr::{BackendInfo, CancelToken};
use earshot_core::audio::{MonoBuffer, PIPELINE_RATE};
use earshot_core::capture::{DeviceTime, ReplayBuilder, TrackId};
use earshot_core::model::{ClockDomain, TranscriptPass};
use earshot_core::pipeline::PipelineError;
use earshot_core::prompt::SummaryStyle;
use earshot_store::{DataDir, Store};
use serde::{Deserialize, Serialize};

use crate::backends::{Accelerator, ModelDir};
use crate::host::{
    self, AfterMeeting, HostOutcome, HostPost, HostStep, PreparedSummary, SettingsSummary,
    SkipReason, SummaryBackend, SummaryPlan, SummaryPlanError, SummaryResult, TranscriptionSetup,
};
use crate::live::{
    LiveAsrFactory, LiveConfig, LiveOutcome, LiveSession, RuntimeEvent, RuntimeWarning,
};
use crate::live_summaries::LiveSummaries;
use crate::local::DeviceRecord;
use crate::model_bench::StoredVerdict;
use crate::replay::{PaceClock, PaceState, Paced, WallClock};
use crate::speaker_timing::SpeakerTiming;
use crate::summary_settings::SummaryWriter;
use crate::{RuntimeError, Services, SharedClock, os_seeded_rng};

/// The device-local key of the latest result.
pub const BENCHMARK_KEY: &str = "benchmark";
/// The record's format version (outside the JSON payload).
pub const RECORD_VERSION: u32 = 1;
/// Live transcription kept up when every track ran to the end and the live transcript was
/// complete at most this long after the replay ended (a judgement: the last speech piece,
/// up to 30 s of audio, is decoded after the end, which takes a few seconds on a machine
/// that keeps up; a machine that falls behind is late by a share of the whole meeting).
pub const CATCH_UP_LIMIT: Duration = Duration::from_secs(15);
/// How long the event loop waits for an event before it checks the cancel token again.
pub const EVENT_WAIT: Duration = Duration::from_millis(100);
/// Progress is reported when the replay has moved on by this much.
pub const PROGRESS_STEP: Duration = Duration::from_secs(1);
/// Why a local summary cannot run: llama.cpp's DLLs are not beside the exe or on `PATH`.
pub const LLAMA_DLLS_MISSING: &str =
    "a summary on this computer needs llama.cpp's DLLs next to Earshot";
/// Why a local summary on the graphics card cannot run: the CUDA runtime's DLLs are missing.
pub const CUDA_DLLS_MISSING: &str = "a summary on this computer needs the CUDA runtime's DLLs";
/// The title of the throwaway meeting.
pub const MEETING_TITLE: &str = "Benchmark";

/// Device time of the replay's first frame (any value works; this is 1 s).
const ORIGIN: DeviceTime = DeviceTime::from_ticks(10_000_000);

/// The bundled two-track meeting.
#[derive(Debug, Clone)]
pub struct BenchmarkMeeting {
    mic: Vec<f32>,
    reference: Vec<f32>,
}

/// Decodes a 16 kHz mono 16-bit FLAC file held in memory.
fn decode_flac(bytes: &[u8]) -> Result<Vec<f32>, RuntimeError> {
    let bad = |_| RuntimeError::Internal("the benchmark meeting does not decode");
    let mut reader = claxon::FlacReader::new(std::io::Cursor::new(bytes)).map_err(bad)?;
    let info = reader.streaminfo();
    if info.sample_rate != PIPELINE_RATE.hz() || info.channels != 1 || info.bits_per_sample != 16 {
        return Err(RuntimeError::Internal(
            "the benchmark meeting is not 16 kHz mono 16-bit",
        ));
    }
    reader
        .samples()
        .map(|s| {
            s.map_err(bad).and_then(|v| {
                i16::try_from(v)
                    .map(earshot_core::audio::i16_to_f32)
                    .map_err(|_| RuntimeError::Internal("the benchmark meeting does not decode"))
            })
        })
        .collect()
}

impl BenchmarkMeeting {
    /// The meeting from its two FLAC files (16 kHz mono 16-bit).
    ///
    /// # Errors
    /// [`RuntimeError::Internal`] when a file does not decode or is not 16 kHz mono 16-bit.
    pub fn from_flac(mic: &[u8], reference: &[u8]) -> Result<Self, RuntimeError> {
        Ok(Self::new(decode_flac(mic)?, decode_flac(reference)?))
    }

    /// The meeting from its samples at 16 kHz (the shorter track is padded with silence).
    #[must_use]
    pub fn new(mut mic: Vec<f32>, mut reference: Vec<f32>) -> Self {
        let n = mic.len().max(reference.len());
        mic.resize(n, 0.0);
        reference.resize(n, 0.0);
        Self { mic, reference }
    }

    /// Its length.
    #[must_use]
    pub fn duration(&self) -> Duration {
        PIPELINE_RATE.duration_of(self.mic.len())
    }
}

/// The throwaway data folder of one run: created empty, deleted when dropped (every path,
/// failure, cancel and panic included).
#[derive(Debug)]
pub struct Scratch {
    dir: PathBuf,
}

/// Leftover run folders older than this are deleted when a run starts (a crash).
pub const STALE_SCRATCH: Duration = Duration::from_secs(6 * 3600);

impl Scratch {
    /// A new empty folder under `root`, after deleting run folders under `root` that are
    /// older than [`STALE_SCRATCH`] (left by a crash).
    ///
    /// # Errors
    /// I/O errors creating the folder.
    pub fn create(root: &Path) -> Result<Self, RuntimeError> {
        if let Ok(entries) = std::fs::read_dir(root) {
            for e in entries.flatten() {
                let old = e
                    .metadata()
                    .and_then(|m| m.modified())
                    .ok()
                    .and_then(|t| t.elapsed().ok())
                    .is_some_and(|age| age > STALE_SCRATCH);
                if old && e.file_name().to_string_lossy().starts_with("run-") {
                    let _ = std::fs::remove_dir_all(e.path());
                }
            }
        }
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        let dir = root.join(format!("run-{}-{nanos}", std::process::id()));
        std::fs::create_dir_all(&dir).map_err(|e| RuntimeError::Store(e.into()))?;
        Ok(Self { dir })
    }

    /// The folder.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.dir
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        // The store is closed by now; a file still held open (a slow virus scanner) gets a
        // few short retries.
        for _ in 0..5 {
            match std::fs::remove_dir_all(&self.dir) {
                Ok(()) => return,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => return,
                Err(_) => std::thread::sleep(Duration::from_millis(100)),
            }
        }
    }
}

/// Why the benchmark makes no summary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum BenchSkip {
    /// The Summaries page says no summary.
    NotRequested,
    /// The summary is written by the user's endpoint, Claude Code or Codex: the benchmark
    /// makes no network connection and hands nothing to another program.
    NotOnThisComputer {
        /// Who writes the summary (`SummaryWriter::as_str`).
        writer: String,
    },
    /// The local summary model is not installed.
    NotInstalled {
        /// Its id.
        model: String,
    },
}

/// What "summarise during the meeting" did in a run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum LiveSummariesRun {
    /// The setting is off.
    Off,
    /// It ran to the end of the replay.
    On,
    /// The setting is on, but it did not run: it needs a local summary model.
    NotRun,
    /// It never started or stopped early (on battery with the battery speaker setting, no
    /// lower priority, a crash).
    Stopped {
        /// The runtime's reason ([`RuntimeEvent::LiveSummariesOff`]; settings text, no
        /// content).
        reason: String,
    },
}

/// The settings of a benchmark as read from the user's store ([`read_settings`]).
#[derive(Debug)]
pub struct BenchmarkSettings {
    /// The transcription model and where it runs.
    pub transcription: TranscriptionSetup,
    /// The summary step (no API key read).
    pub summary: SettingsSummary,
    /// The speaker detection setting.
    pub speaker_timing: SpeakerTiming,
    /// The model check's record, for its verdict beside the result.
    pub check: Option<crate::model_bench::StoredCheck>,
    /// The user's summary style (a refused stored one is empty, with a note in `summary`).
    pub style: SummaryStyle,
}

/// Reads the settings of a benchmark from the user's `store` (nothing is changed): the
/// meeting's own choices on `build` and `hardware`. No API key is read.
///
/// # Errors
/// Store errors; a stored model check that does not parse; a stored summary model that is
/// not shipped.
pub fn read_settings(
    store: &Store,
    models: &ModelDir,
    build: &[Accelerator],
    hardware: &earshot_hardware::Hardware,
) -> Result<BenchmarkSettings, RuntimeError> {
    Ok(BenchmarkSettings {
        transcription: host::transcription_setup(store, models, build, hardware)?,
        summary: host::summary_from_settings(
            store,
            build,
            hardware,
            &crate::summary_settings::NoApiKeys,
        )?,
        speaker_timing: crate::speaker_timing::speaker_timing(store)?.timing(),
        check: crate::model_bench::stored_check(store)?,
        style: crate::summary_style::summary_style(store)?.0,
    })
}

/// What the summary step of a benchmark is.
#[derive(Debug)]
pub enum BenchSummary {
    /// The local model of the settings, ready to run.
    Local(Box<PreparedSummary>),
    /// None, with why.
    Skipped(BenchSkip),
}

/// The settings a benchmark runs with ([`benchmark_setup`]).
#[derive(Debug)]
pub struct BenchmarkSetup {
    /// The transcription model.
    pub model: String,
    /// Where transcription may run, best first (the "where it runs" setting applied).
    pub order: Vec<Accelerator>,
    /// The summary step.
    pub summary: BenchSummary,
    /// The speaker detection setting.
    pub speaker_timing: SpeakerTiming,
    /// The "summarise during the meeting" setting (honoured only with a local summary:
    /// [`live_summaries_start`]).
    pub live_summaries: bool,
    /// The model check's record, for its verdict beside the result.
    pub check: Option<crate::model_bench::StoredCheck>,
    /// The user's summary style.
    pub style: SummaryStyle,
    /// What the user is told with the result (a "where it runs" choice not used, an unknown
    /// stored value, a refused style, a library that could not load).
    pub notes: Vec<String>,
}

/// Checks the model files and prepares the summary step of `settings`; run this without the
/// store lock (it reads the disk and looks for DLLs). `dll_dirs`:
/// [`host::dll_search_dirs`].
///
/// # Errors
/// A model that is missing; a chosen local model that is not installed or not shipped;
/// llama.cpp's DLLs missing for a local summary.
pub fn benchmark_setup(
    settings: BenchmarkSettings,
    models: &ModelDir,
    clock: SharedClock,
    dll_dirs: &[PathBuf],
) -> Result<BenchmarkSetup, RuntimeError> {
    let model = settings.transcription.choice.model.clone();
    host::require_models(models, &model, true)?;
    let summary = prepare_bench_summary(&settings.summary, models, clock, dll_dirs)?;
    let mut notes = settings.transcription.notes;
    notes.extend(settings.summary.notes);
    Ok(BenchmarkSetup {
        model,
        order: settings.transcription.order,
        summary,
        speaker_timing: settings.speaker_timing,
        live_summaries: settings.summary.live,
        check: settings.check,
        style: settings.style,
        notes,
    })
}

/// The summary step of a benchmark: the local model prepared, or why there is none. Nothing
/// is prepared for a summary written elsewhere.
///
/// # Errors
/// A chosen local model that is not installed or not shipped; llama.cpp's DLLs missing.
pub fn prepare_bench_summary(
    settings: &SettingsSummary,
    models: &ModelDir,
    clock: SharedClock,
    dll_dirs: &[PathBuf],
) -> Result<BenchSummary, RuntimeError> {
    Ok(match settings.writer {
        SummaryWriter::NoSummary => BenchSummary::Skipped(BenchSkip::NotRequested),
        SummaryWriter::OnThisComputer => {
            let plan = host::summary_plan(
                &settings.request,
                earshot_store::total_memory(),
                settings.placement.gpu,
                models,
            )
            .map_err(|e| match e {
                SummaryPlanError::UnknownModel(e) | SummaryPlanError::NotInstalled(e) => e,
            })?;
            match plan {
                SummaryPlan::Local(_) => {
                    match host::prepare_summary_on(
                        &plan,
                        models,
                        clock,
                        dll_dirs,
                        settings.placement.gpu,
                    ) {
                        Ok(Some(s)) => BenchSummary::Local(Box::new(s)),
                        Ok(None) => {
                            return Err(RuntimeError::Internal("a local summary plan ran none"));
                        }
                        Err(host::SummarySetupError::Runtime(e)) => return Err(e),
                        Err(host::SummarySetupError::LlamaDllsMissing(_)) => {
                            return Err(RuntimeError::Unavailable(LLAMA_DLLS_MISSING));
                        }
                        Err(host::SummarySetupError::CudaDllsMissing(_)) => {
                            return Err(RuntimeError::Unavailable(CUDA_DLLS_MISSING));
                        }
                    }
                }
                SummaryPlan::NotInstalled(choice, _) => {
                    BenchSummary::Skipped(BenchSkip::NotInstalled {
                        model: choice.model.id.to_owned(),
                    })
                }
                _ => BenchSummary::Skipped(BenchSkip::NotRequested),
            }
        }
        writer
        @ (SummaryWriter::OwnEndpoint | SummaryWriter::ClaudeCode | SummaryWriter::Codex) => {
            BenchSummary::Skipped(BenchSkip::NotOnThisComputer {
                writer: writer.as_str().to_owned(),
            })
        }
    })
}

/// The live summaries of a run and what is recorded about them: with the setting (`on`) and a
/// local summary, the worker on `order`; with the setting and none, [`LiveSummariesRun::NotRun`].
#[must_use]
pub fn live_summaries_start(
    summary: &BenchSummary,
    on: bool,
    order: &[Accelerator],
) -> (Option<LiveSummaries>, LiveSummariesRun) {
    if !on {
        return (None, LiveSummariesRun::Off);
    }
    let prepared = match summary {
        BenchSummary::Local(s) => Some(&**s),
        BenchSummary::Skipped(_) => None,
    };
    match host::live_summaries_for(prepared, order) {
        Ok(l) => (Some(l), LiveSummariesRun::On),
        Err(_) => (None, LiveSummariesRun::NotRun),
    }
}

/// Where a run is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BenchmarkProgress {
    /// The meeting is being replayed: this much of `total` so far.
    Replaying {
        /// Replayed so far.
        heard: Duration,
        /// The meeting's length.
        total: Duration,
    },
    /// The replay ended; the live transcript is being finished.
    FinishingLive,
    /// The final transcript is being made.
    Transcribing,
    /// The summary is being written.
    Summarising,
}

/// The work after the live session: the final pass and the summary, reporting
/// [`HostStep`]s; the token cancels both (production: [`host::after_meeting`]).
pub type AfterFn<'a> = Box<
    dyn FnOnce(&Services, &LiveOutcome, &CancelToken, &mut dyn FnMut(HostStep<'_>)) -> HostOutcome
        + 'a,
>;

/// One run's inputs ([`run_benchmark`]).
pub struct BenchmarkRun<'a> {
    /// The meeting.
    pub meeting: &'a BenchmarkMeeting,
    /// The live session's configuration (speaker timing, live summaries).
    pub config: LiveConfig,
    /// Live transcription.
    pub factory: Option<LiveAsrFactory>,
    /// The final pass and the summary.
    pub after: AfterFn<'a>,
    /// What "summarise during the meeting" starts as ([`live_summaries_start`]).
    pub live_summaries: LiveSummariesRun,
    /// The user's summary style, copied into the throwaway store.
    pub style: SummaryStyle,
    /// Timings and pacing.
    pub clock: Arc<dyn PaceClock>,
    /// Replay in real time (production); off, the replay goes as fast as the session takes
    /// it.
    pub realtime: bool,
    /// The throwaway [`Services`]' clock.
    pub services_clock: SharedClock,
    /// The folder under which the run's throwaway data folder is made.
    pub scratch_root: &'a Path,
}

/// What one run measured ([`run_benchmark`]).
#[derive(Debug, Clone, PartialEq)]
pub struct Measured {
    /// The meeting's length.
    pub meeting: Duration,
    /// Every transcript lag the session reported (with its Stats, during the replay).
    pub lags: Vec<Duration>,
    /// The backend that ran each track's live transcription.
    pub live_backends: Vec<(TrackId, BackendInfo)>,
    /// A track's live transcription stopped (failed or fell behind) or was cut at the end.
    pub live_stopped: bool,
    /// From the replay's end to the live transcript being complete.
    pub catch_up: Duration,
    /// From the replay's end to the final transcript being stored.
    pub transcript_ready: Duration,
    /// The backend the final pass recorded.
    pub final_backend: Option<BackendInfo>,
    /// What "summarise during the meeting" did.
    pub live_summaries: LiveSummariesRun,
    /// The summary.
    pub summary: MeasuredSummary,
}

/// What the summary step of a run did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MeasuredSummary {
    /// Stored.
    Stored {
        /// From the replay's end.
        ready: Duration,
        /// The model.
        model: String,
        /// Where it ran (llama.cpp's backend name).
        backend: String,
    },
    /// Not made (the setting, or not installed).
    Skipped(BenchSkip),
}

/// Runs the benchmark (module docs). `None` when cancelled through `cancel`.
///
/// # Errors
/// The throwaway store cannot be made; the session cannot start or finish; the final pass
/// or the summary failed.
pub fn run_benchmark(
    run: BenchmarkRun<'_>,
    skip: Option<&BenchSkip>,
    cancel: &CancelToken,
    progress: &mut dyn FnMut(BenchmarkProgress),
) -> Result<Option<Measured>, RuntimeError> {
    let scratch = Scratch::create(run.scratch_root)?;
    let result = run_in(run, skip, &scratch, cancel, progress);
    drop(scratch);
    result
}

/// What a skipped summary is recorded as: the result's own reason. `skip` is the setup's reason
/// for running none (the host's [`SkipReason::NotRequested`] then stands for it); a summary
/// that was prepared (`skip` is `None`) and did not run is an error, as is one skipped by a
/// failed final pass (reported as that failure before this is reached).
fn skip_of(reason: &SkipReason, skip: Option<&BenchSkip>) -> Result<BenchSkip, RuntimeError> {
    match reason {
        SkipReason::NotRequested => skip.cloned().ok_or(RuntimeError::Internal(
            "the benchmark summary was prepared and did not run",
        )),
        SkipReason::NotInstalled { model, .. } | SkipReason::ChosenNotInstalled { model } => {
            Ok(BenchSkip::NotInstalled {
                model: (*model).to_owned(),
            })
        }
        SkipReason::NoApiKey(_) => Ok(BenchSkip::NotOnThisComputer {
            writer: SummaryWriter::OwnEndpoint.as_str().to_owned(),
        }),
        SkipReason::ClaudeCodeUnavailable(_) => Ok(BenchSkip::NotOnThisComputer {
            writer: SummaryWriter::ClaudeCode.as_str().to_owned(),
        }),
        SkipReason::CodexUnavailable(_) => Ok(BenchSkip::NotOnThisComputer {
            writer: SummaryWriter::Codex.as_str().to_owned(),
        }),
        SkipReason::PostFailed => Err(RuntimeError::Internal(
            "the benchmark summary was skipped after a failed final pass",
        )),
    }
}

/// A live session stopped and joined when dropped, so a panic never leaves its thread holding
/// the throwaway store or FLAC writers while the scratch folder is deleted.
struct JoinedSession(Option<LiveSession>);

impl Drop for JoinedSession {
    fn drop(&mut self) {
        if let Some(session) = self.0.take() {
            let _ = session.stop();
        }
    }
}

fn run_in(
    run: BenchmarkRun<'_>,
    skip: Option<&BenchSkip>,
    scratch: &Scratch,
    cancel: &CancelToken,
    progress: &mut dyn FnMut(BenchmarkProgress),
) -> Result<Option<Measured>, RuntimeError> {
    let data = DataDir::new(scratch.path());
    data.create().map_err(|e| RuntimeError::Store(e.into()))?;
    let store = data.open_store()?;
    let svc = Services::new(store, data, run.services_clock)?;
    crate::summary_style::set_summary_style(&mut svc.store(), run.style.as_str())
        .map_err(|_| RuntimeError::Internal("the summary style cannot be copied"))?;
    let total = run.meeting.duration();
    let source = ReplayBuilder::new(
        ORIGIN,
        MonoBuffer::new(PIPELINE_RATE, run.meeting.mic.clone()),
    )
    .reference(MonoBuffer::new(
        PIPELINE_RATE,
        run.meeting.reference.clone(),
    ))
    .build()
    .map_err(|_| RuntimeError::Internal("the benchmark meeting cannot be replayed"))?;
    let state = Arc::new(PaceState::default());
    let paced = Paced::new(
        source,
        Arc::clone(&run.clock),
        run.realtime,
        Arc::clone(&state),
    );
    let clock = run.clock;
    // Declared after `svc`, so an unwind joins the session before the store closes.
    let mut session = JoinedSession(Some(crate::live::start_live(
        paced,
        svc.clone(),
        run.config,
        run.factory,
        os_seeded_rng(),
    )?));
    let mut lags = Vec::new();
    let mut live_backends = Vec::new();
    let mut live_stopped = false;
    let mut live_summaries = run.live_summaries;
    let mut reported = Duration::ZERO;
    let mut finishing = false;
    let mut catch_up = None;
    progress(BenchmarkProgress::Replaying {
        heard: Duration::ZERO,
        total,
    });
    let events = session.0.as_mut().and_then(LiveSession::take_events);
    if let Some(events) = events {
        let mut stop_sent = false;
        loop {
            if !stop_sent
                && cancel.is_cancelled()
                && let Some(s) = &session.0
            {
                s.request_stop();
                stop_sent = true;
            }
            let replay_over = state.ended().is_some();
            if replay_over && !finishing {
                finishing = true;
                progress(BenchmarkProgress::FinishingLive);
            } else if !replay_over {
                let heard = state.heard().min(total);
                if heard >= reported + PROGRESS_STEP {
                    reported = heard;
                    progress(BenchmarkProgress::Replaying { heard, total });
                }
            }
            match events.recv_timeout(EVENT_WAIT) {
                Ok(RuntimeEvent::Ended(_)) | Err(RecvTimeoutError::Disconnected) => break,
                Ok(RuntimeEvent::LiveTranscriptFinished { after_capture_end }) => {
                    catch_up = Some(after_capture_end);
                }
                // Sent with the pipeline's Stats (at most once per second of meeting
                // audio), so every one is from the replay, the last at its end.
                Ok(RuntimeEvent::TranscriptLag { behind, .. }) => lags.push(behind),
                Ok(RuntimeEvent::LiveBackend { track, backend, .. }) => {
                    live_backends.push((track, backend));
                }
                Ok(RuntimeEvent::LiveSummariesOff { reason }) => {
                    if !matches!(live_summaries, LiveSummariesRun::Stopped { .. }) {
                        live_summaries = LiveSummariesRun::Stopped { reason };
                    }
                }
                Ok(RuntimeEvent::Warning(
                    RuntimeWarning::LiveCutAtStop { .. }
                    | RuntimeWarning::Pipeline(PipelineError::LiveAsr { .. }),
                )) => live_stopped = true,
                Ok(_) | Err(RecvTimeoutError::Timeout) => {}
            }
        }
    }
    let outcome = session
        .0
        .take()
        .ok_or(RuntimeError::Internal("the benchmark session is gone"))?
        .wait()?;
    if cancel.is_cancelled() {
        return Ok(None);
    }
    let replay_end = state
        .ended()
        .ok_or(RuntimeError::Internal("the benchmark replay did not end"))?;
    let catch_up = catch_up.ok_or(RuntimeError::Internal(
        "the benchmark session did not finish its live transcript",
    ))?;
    let mut transcript_ready = None;
    let host = (run.after)(&svc, &outcome, cancel, &mut |step| match step {
        HostStep::PostStarting { .. } => progress(BenchmarkProgress::Transcribing),
        HostStep::PostFinished(_) => transcript_ready = Some(clock.now()),
        HostStep::SummaryStarting => progress(BenchmarkProgress::Summarising),
        _ => {}
    });
    let summary_done = clock.now();
    // Second line of defence: the setup prepares only a local summary.
    if !host.network.is_empty() || !host.handovers.is_empty() {
        return Err(RuntimeError::Internal(
            "the benchmark reached outside this computer",
        ));
    }
    if cancel.is_cancelled() {
        return Ok(None);
    }
    match host.post {
        Ok(HostPost::Done(_)) => {}
        Ok(HostPost::Cancelled { .. }) => return Ok(None),
        Ok(HostPost::Skipped | HostPost::NothingToProcess) => {
            return Err(RuntimeError::Internal(
                "the benchmark meeting had nothing to transcribe",
            ));
        }
        Err(e) => return Err(e),
    }
    let summary = match host.summary {
        SummaryResult::Stored { model, backend, .. } => MeasuredSummary::Stored {
            ready: summary_done.saturating_sub(replay_end),
            model,
            backend: match backend {
                SummaryBackend::Local(name) => name,
                SummaryBackend::Remote | SummaryBackend::ClaudeCode | SummaryBackend::Codex => {
                    return Err(RuntimeError::Internal(
                        "the benchmark reached outside this computer",
                    ));
                }
            },
        },
        SummaryResult::Skipped { reason } => MeasuredSummary::Skipped(skip_of(&reason, skip)?),
        SummaryResult::Cancelled => return Ok(None),
        SummaryResult::Failed { error, .. } => return Err(error),
    };
    let final_backend = {
        let db = svc.store();
        crate::backends::pass_backend(&db, outcome.meeting, TranscriptPass::Final, None)?
    };
    Ok(Some(Measured {
        meeting: total,
        lags,
        live_backends,
        live_stopped,
        catch_up,
        transcript_ready: transcript_ready
            .unwrap_or(summary_done)
            .saturating_sub(replay_end),
        final_backend,
        live_summaries,
        summary,
    }))
}

/// The settings a result was measured with.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecordSettings {
    /// The transcription model (manifest id).
    pub transcription_model: String,
    /// The speaker detection setting (`SpeakerTiming::as_str`).
    pub speaker_timing: String,
    /// What "summarise during the meeting" did.
    pub live_summaries: LiveSummariesRun,
}

/// A backend as recorded.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecordBackend {
    /// `cpu`, `cuda`, `vulkan`, `qnn`, `fake` (`BackendKind`'s serde name).
    pub kind: String,
    /// The device the backend reported (a GPU's name, or `cpu`).
    pub device: String,
}

impl From<&BackendInfo> for RecordBackend {
    fn from(b: &BackendInfo) -> Self {
        Self {
            kind: serde_json::to_value(b.kind)
                .ok()
                .and_then(|v| v.as_str().map(str::to_owned))
                .unwrap_or_default(),
            device: b.device.clone(),
        }
    }
}

/// The summary step as recorded.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RecordSummary {
    /// Stored by the local model.
    Stored {
        /// From the replay's end (ms).
        ready_ms: u64,
        /// The model id.
        model: String,
        /// Where it ran (llama.cpp's backend name: `cpu`, `cuda`, ...).
        backend: String,
    },
    /// Not made.
    Skipped {
        /// Why.
        reason: BenchSkip,
    },
}

/// The latest benchmark result (device record [`BENCHMARK_KEY`]): timings, settings and
/// backends only, never content.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BenchmarkRecord {
    /// When it ran (Unix ms).
    pub ran_at_ms: u64,
    /// The app version that ran it.
    pub app_version: String,
    /// The settings.
    pub settings: RecordSettings,
    /// The meeting's length (ms).
    pub meeting_ms: u64,
    /// Live transcription's backend per track (mic first), as the session reported it.
    pub live_backends: Vec<RecordBackend>,
    /// The largest transcript lag while the replay ran, over both tracks (ms); `None`: none
    /// reported.
    pub lag_max_ms: Option<u64>,
    /// The median of both tracks' transcript lag reports while the replay ran (ms).
    pub lag_median_ms: Option<u64>,
    /// From the replay's end to the live transcript being complete (ms).
    pub catch_up_ms: u64,
    /// Live transcription kept up (every track ran to the end and caught up within
    /// [`CATCH_UP_LIMIT`]).
    pub kept_up: bool,
    /// From the replay's end to the final transcript (ms).
    pub transcript_ready_ms: u64,
    /// The final pass's backend.
    pub final_backend: Option<RecordBackend>,
    /// The summary.
    pub summary: RecordSummary,
    /// The model check's verdict for the transcription model on live transcription's
    /// backend; `None`: no check, or it did not test that pair.
    pub check: Option<StoredVerdict>,
    /// What the user is told about the run's settings ([`BenchmarkSetup::notes`]).
    pub notes: Vec<String>,
}

fn ms(d: Duration) -> u64 {
    u64::try_from(d.as_millis()).unwrap_or(u64::MAX)
}

/// The median of `values` (the lower middle for an even count); `None` when empty.
#[must_use]
pub fn median(values: &[Duration]) -> Option<Duration> {
    let mut v = values.to_vec();
    v.sort_unstable();
    v.get(v.len().saturating_sub(1) / 2).copied()
}

impl BenchmarkRecord {
    /// The record of `measured` with `setup`, run at `ran_at_ms` by `app_version`.
    #[must_use]
    pub fn new(
        measured: &Measured,
        setup: &BenchmarkSetup,
        ran_at_ms: u64,
        app_version: &str,
    ) -> Self {
        let mut tracks = measured.live_backends.clone();
        tracks.sort_by_key(|(t, _)| *t != TrackId::Mic);
        let check = setup.check.as_ref().and_then(|c| {
            let backend = tracks.first().map(|(_, b)| b.kind)?;
            c.verdict(&setup.model, backend)
        });
        Self {
            ran_at_ms,
            app_version: app_version.to_owned(),
            settings: RecordSettings {
                transcription_model: setup.model.clone(),
                speaker_timing: setup.speaker_timing.as_str().to_owned(),
                live_summaries: measured.live_summaries.clone(),
            },
            meeting_ms: ms(measured.meeting),
            live_backends: tracks.iter().map(|(_, b)| RecordBackend::from(b)).collect(),
            lag_max_ms: measured.lags.iter().max().map(|d| ms(*d)),
            lag_median_ms: median(&measured.lags).map(ms),
            catch_up_ms: ms(measured.catch_up),
            kept_up: !measured.live_stopped
                && !measured.live_backends.is_empty()
                && measured.catch_up <= CATCH_UP_LIMIT,
            transcript_ready_ms: ms(measured.transcript_ready),
            final_backend: measured.final_backend.as_ref().map(RecordBackend::from),
            summary: match &measured.summary {
                MeasuredSummary::Stored {
                    ready,
                    model,
                    backend,
                } => RecordSummary::Stored {
                    ready_ms: ms(*ready),
                    model: model.clone(),
                    backend: backend.clone(),
                },
                MeasuredSummary::Skipped(reason) => RecordSummary::Skipped {
                    reason: reason.clone(),
                },
            },
            check,
            notes: setup.notes.clone(),
        }
    }
}

/// The stored result ([`stored_benchmark`]).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct StoredBenchmark {
    /// The latest result, if any readable one is stored.
    pub record: Option<BenchmarkRecord>,
    /// A stored result this version cannot read (the next run replaces it).
    pub unreadable: bool,
}

/// Stores `record` as the latest result (replacing any earlier one).
///
/// # Errors
/// Store errors.
pub fn record_benchmark(store: &mut Store, record: &BenchmarkRecord) -> Result<(), RuntimeError> {
    let json = serde_json::to_vec(record)
        .map_err(|_| RuntimeError::Internal("the benchmark result does not serialise"))?;
    let mut value = RECORD_VERSION.to_le_bytes().to_vec();
    value.extend_from_slice(&json);
    crate::local::set(store, &DeviceRecord::Benchmark.key(), &value)?;
    Ok(())
}

/// The latest stored result; one this version cannot read is reported, never an error.
///
/// # Errors
/// Store errors.
pub fn stored_benchmark(store: &Store) -> Result<StoredBenchmark, RuntimeError> {
    let Some(value) = store.local_value(BENCHMARK_KEY)? else {
        return Ok(StoredBenchmark::default());
    };
    let record = value
        .split_first_chunk::<4>()
        .filter(|(v, _)| u32::from_le_bytes(**v) == RECORD_VERSION)
        .and_then(|(_, json)| serde_json::from_slice(json).ok());
    Ok(StoredBenchmark {
        unreadable: record.is_none(),
        record,
    })
}

/// What the user is told when an accelerator's ONNX Runtime library could not load.
#[must_use]
pub fn onnx_fallback_note(library: Accelerator) -> String {
    let part = match library {
        Accelerator::Cpu => "processor",
        Accelerator::Cuda | Accelerator::Vulkan => "graphics card",
        Accelerator::Qnn => "NPU",
    };
    format!(
        "The {part} library of ONNX Runtime could not be loaded, so speech models ran on the \
         processor."
    )
}

/// The production run (module docs): the settings in `user` (read, never changed), the
/// models in `models`, `meeting` replayed in real time on a throwaway store under
/// `scratch_root`; the result is stored in `user` and returned. `None` when cancelled
/// (nothing is stored). `progress` hears where the run is.
///
/// # Errors
/// As [`read_settings`], [`benchmark_setup`] and [`run_benchmark`]; ONNX Runtime cannot load.
#[allow(clippy::too_many_arguments)]
pub fn run_from_settings(
    user: &Services,
    models: &ModelDir,
    meeting: &BenchmarkMeeting,
    dll_dirs: &[PathBuf],
    scratch_root: &Path,
    app_version: &str,
    cancel: &CancelToken,
    progress: &mut dyn FnMut(BenchmarkProgress),
) -> Result<Option<BenchmarkRecord>, RuntimeError> {
    let order = host::settled_order();
    let settings = {
        let db = user.store();
        read_settings(&db, models, &order, &crate::hardware())?
    };
    // Reads the disk and looks for DLLs: with the store lock released.
    let mut setup = benchmark_setup(settings, models, Arc::clone(&user.clock), dll_dirs)?;
    let mut library_note = None;
    host::init_onnx_for(models, &setup.model, &setup.order, &mut |library, _| {
        library_note = Some(onnx_fallback_note(library));
    })?;
    setup.notes.extend(library_note);
    let (summary, skip) = match &setup.summary {
        BenchSummary::Local(s) => (Some(&**s), None),
        BenchSummary::Skipped(why) => (None, Some(why)),
    };
    let (live_summaries, live_run) =
        live_summaries_start(&setup.summary, setup.live_summaries, &setup.order);
    let config = LiveConfig {
        title: MEETING_TITLE.into(),
        clock_domain: ClockDomain::Replay,
        speaker_timing: setup.speaker_timing,
        live_summaries,
        ..LiveConfig::default()
    };
    let factory = host::live_factory(
        models,
        &setup.model,
        &setup.order,
        Arc::clone(&user.clock),
        true,
    );
    let (model, order) = (setup.model.clone(), setup.order.clone());
    let no_summary = SkipReason::NotRequested;
    let after: AfterFn<'_> = Box::new(move |svc, outcome, cancel, progress| {
        let run = AfterMeeting {
            models,
            model: &model,
            order: &order,
            post: true,
            whole_pass: false,
            summary: summary.map(|s| &s.service),
            no_summary,
            post_cancel: cancel,
            summary_cancel: cancel,
        };
        host::after_meeting(svc, outcome, &run, progress)
    });
    let measured = run_benchmark(
        BenchmarkRun {
            meeting,
            config,
            factory: Some(factory),
            after,
            live_summaries: live_run,
            style: setup.style.clone(),
            clock: Arc::new(WallClock::new()),
            realtime: true,
            services_clock: Arc::clone(&user.clock),
            scratch_root,
        },
        skip,
        cancel,
        progress,
    )?;
    let Some(measured) = measured else {
        return Ok(None);
    };
    let record = BenchmarkRecord::new(&measured, &setup, user.clock.now_unix_ms(), app_version);
    record_benchmark(&mut user.store(), &record)?;
    Ok(Some(record))
}

#[cfg(test)]
#[path = "benchmark_tests.rs"]
mod tests;
