//! The meeting driver (T7.2): one meeting recording at a time, each on a thread of its own from
//! pre-flight to summary, its progress sent to the UI as [`MeetingEvent`]s; older meetings finish
//! behind it, in order (T9.1l, [`earshot_runtime::meeting_queue`]).
//!
//! The order is the CLI's `record` through `earshot_runtime::host` (module docs there),
//! minus the printing, with the models after capture:
//!
//! 1. Pre-flight, before any capture starts ([`HostPrepare`]): the model choice and every
//!    model file for what this computer can use now, the summary plan and its setup (llama.cpp's
//!    DLLs for a local model), the speaker timing setting. Each failure is an [`IpcError`] and
//!    nothing is recorded.
//! 2. The live session is created on the meeting thread (the WASAPI starter runs on the thread
//!    that starts it; COM state is per thread), and its events are drained there: the stop flag is
//!    checked before every event, the stop is requested once, and the events are drained until
//!    `Ended`. The session records from here on; ONNX Runtime, the live models and live summaries
//!    wait for the meeting's turn on a thread of the session's (`LiveHold`).
//! 3. The recording saved ([`RuntimeEvent::RecordingSaved`]): a new meeting may start. This one
//!    waits for its turn in the queue and runs `host::after_meeting` with its progress, then the
//!    summary result, the last event.
//!
//! Only the newest meeting talks to the meeting screen: an older one sends what the screen and the
//! tray need to know how it ended ([`MeetingEvent::Ended`], [`MeetingEvent::PostProgress`],
//! [`MeetingEvent::Summary`], each with its meeting id), and nothing else once a newer one started.
//!
//! The driver is generic over [`Prepare`], so tests run it on a replay and fake post work.

use std::panic::AssertUnwindSafe;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use earshot_core::asr::CancelToken;
use earshot_core::capture::TrackId;
use earshot_runtime::backends::ModelDir;
use earshot_runtime::call_end::{CallEndChange, CallEndConfig, CallEndWatch, CallSignal};
use earshot_runtime::claude_code::ClaudeCodeProbe;
use earshot_runtime::codex::CodexProbe;
use earshot_runtime::host::{
    self, HostOutcome, HostStep, PreparedSummary, SkipReason, SummaryPlanError, SummarySetupError,
};
use earshot_runtime::live::{LiveOutcome, LiveSession, RuntimeEvent};
use earshot_runtime::meeting_queue::{MeetingQueue, QueueTicket};
use earshot_runtime::summary_settings::{ApiKeyStore, SummaryWriter};
use earshot_runtime::{MeetingJob, RuntimeError, Services};

use crate::ipc::IpcError;
use crate::meeting_events::{
    CaptureMode, EventMapper, MeetingEvent, PostProgressMapper, SummaryEvent, summary_event,
};

/// How long the meeting thread waits for an event before it checks the stop flag again.
pub const EVENT_WAIT: Duration = Duration::from_millis(100);

/// What the user asked for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StartRequest {
    /// What the meeting records.
    pub mode: CaptureMode,
    /// The title (empty: the summary may give one).
    pub title: String,
}

/// Starts the live session (runs on the meeting thread).
pub type StartFn = Box<dyn FnOnce(&Services) -> Result<LiveSession, RuntimeError>>;

/// The work after the meeting: the host's post-meeting runner with its progress callback.
pub type AfterFn =
    Box<dyn FnOnce(&Services, &LiveOutcome, &mut dyn FnMut(HostStep<'_>)) -> HostOutcome>;

/// A meeting ready to start: everything the pre-flight checked.
pub struct Prepared {
    /// Starts the live session.
    pub start: StartFn,
    /// Runs the final pass and the summary.
    pub after: AfterFn,
    /// Banners found during the pre-flight (a stored setting not used), sent after `Started`.
    pub notices: Vec<MeetingEvent>,
    /// What the call app's render sessions do (T9.4); `None`: this meeting has no offer to stop.
    pub call_signal: Option<CallSignal>,
}

/// The pre-flight of a meeting. Runs on the meeting thread, before any capture starts.
pub trait Prepare: Send + Sync {
    /// Checks everything and returns the meeting ready to start; its live work waits for
    /// `ticket`'s turn.
    ///
    /// # Errors
    /// Any pre-flight failure (nothing has been recorded).
    fn prepare(
        &self,
        services: &Services,
        request: &StartRequest,
        ticket: &Arc<QueueTicket>,
    ) -> Result<Prepared, IpcError>;
}

/// Where the UI events go.
pub type Sink = Arc<dyn Fn(MeetingEvent) + Send + Sync>;

/// The clock of the level throttle.
pub type Now = Arc<dyn Fn() -> Instant + Send + Sync>;

struct Running {
    /// Set by `stop`; read by the meeting thread before every event.
    stop: Arc<AtomicBool>,
    /// Set by `keep_recording`; taken by the meeting thread.
    keep: Arc<AtomicBool>,
    /// True from the start request until the recording is saved.
    recording: Arc<AtomicBool>,
    thread: JoinHandle<()>,
}

/// The meeting threads: the newest, and older ones finishing.
#[derive(Default)]
struct Threads {
    newest: Option<Running>,
    finishing: Vec<JoinHandle<()>>,
}

impl Threads {
    fn any_unfinished(&self) -> bool {
        self.newest
            .as_ref()
            .is_some_and(|r| !r.thread.is_finished())
            || self.finishing.iter().any(|t| !t.is_finished())
    }

    /// Whether the newest meeting is recording: its thread has not ended, or not by a panic's path,
    /// which clears the flag first.
    fn recording(&self) -> bool {
        self.newest
            .as_ref()
            .is_some_and(|r| r.recording.load(Ordering::SeqCst) && !r.thread.is_finished())
    }
}

struct Inner {
    prepare: Arc<dyn Prepare>,
    sink: Sink,
    now: Now,
    threads: Mutex<Threads>,
    gate: crate::activity::WorkGate,
    queue: MeetingQueue,
    /// Counts the meetings started.
    started: AtomicU64,
    /// The meeting the screen follows: the newest that started recording.
    shown: AtomicU64,
}

impl Inner {
    /// Sends `event` of meeting number `seq` (module docs).
    fn send(&self, seq: u64, event: MeetingEvent) {
        let shown = self.shown.load(Ordering::SeqCst) == seq;
        if shown
            || matches!(
                event,
                MeetingEvent::Ended { .. }
                    | MeetingEvent::PostProgress { .. }
                    | MeetingEvent::Summary { .. }
            )
        {
            (self.sink)(event);
        }
    }
}

/// Where the driver's meetings are.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DriverPhase {
    /// No meeting.
    Idle,
    /// From the start request until the recording is saved.
    Recording,
    /// Nothing records; a final pass or a summary runs or waits.
    Processing,
}

/// How often [`MeetingDriver::stop_and_wait`] looks at the recording.
const STOP_POLL: Duration = Duration::from_millis(50);

/// Records at most one meeting at a time (cheap to clone; clones share the meetings).
#[derive(Clone)]
pub struct MeetingDriver(Arc<Inner>);

impl MeetingDriver {
    /// A driver with `prepare`, sending events to `sink`, levels throttled on `now`; a
    /// meeting holds a pass of `gate` for its thread's life (no model check meanwhile).
    pub fn new(
        prepare: Arc<dyn Prepare>,
        sink: Sink,
        now: Now,
        gate: crate::activity::WorkGate,
    ) -> Self {
        let sink: Sink = Arc::new(move |e| sink(crate::plain_text::plain_meeting_event(e)));
        Self(Arc::new(Inner {
            prepare,
            sink,
            now,
            threads: Mutex::default(),
            gate,
            queue: MeetingQueue::new(),
            started: AtomicU64::new(0),
            shown: AtomicU64::new(0),
        }))
    }

    fn threads(&self) -> std::sync::MutexGuard<'_, Threads> {
        self.0
            .threads
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// Whether a meeting is recording or being processed.
    #[cfg(test)]
    #[must_use]
    pub fn busy(&self) -> bool {
        self.threads().any_unfinished()
    }

    /// Starts a meeting and returns its id once it records. Waits for the pre-flight and the
    /// capture's start, never for a model or an older meeting; the meeting then runs on its own
    /// thread until its summary result.
    ///
    /// # Errors
    /// `busy` while another meeting records or the model check runs; any pre-flight or start
    /// failure, with nothing recorded.
    pub fn start(&self, services: &Services, request: StartRequest) -> Result<String, IpcError> {
        let (tx, rx) = mpsc::sync_channel::<Result<String, IpcError>>(1);
        {
            let mut threads = self.threads();
            if threads.recording() {
                return Err(IpcError {
                    kind: "busy".into(),
                    message: "a meeting is already recording".into(),
                    retryable: true,
                });
            }
            let pass = self.0.gate.enter_meeting()?;
            if let Some(old) = threads.newest.take() {
                threads.finishing.push(old.thread);
            }
            // Those that ended have nothing left to wait for.
            let (ended, finishing): (Vec<_>, Vec<_>) = std::mem::take(&mut threads.finishing)
                .into_iter()
                .partition(JoinHandle::is_finished);
            threads.finishing = finishing;
            for thread in ended {
                let _ = thread.join();
            }
            let stop = Arc::new(AtomicBool::new(false));
            let recording = Arc::new(AtomicBool::new(true));
            let inner = Arc::clone(&self.0);
            let svc = services.clone();
            let keep = Arc::new(AtomicBool::new(false));
            let flags = RunFlags {
                stop: Arc::clone(&stop),
                keep: Arc::clone(&keep),
                recording: Arc::clone(&recording),
            };
            let seq = self.0.started.fetch_add(1, Ordering::SeqCst) + 1;
            let thread = std::thread::Builder::new()
                .name("earshot-meeting".into())
                .spawn(move || {
                    run_guarded(&inner, &svc, &request, &flags, &tx, seq);
                    drop(pass);
                })
                .map_err(|e| IpcError {
                    kind: "internal".into(),
                    message: format!("cannot start the meeting thread: {e}"),
                    retryable: false,
                })?;
            threads.newest = Some(Running {
                stop,
                keep,
                recording,
                thread,
            });
        }
        let started = rx.recv().unwrap_or_else(|_| {
            Err(IpcError {
                kind: "internal".into(),
                message: "the meeting thread stopped before the meeting started".into(),
                retryable: false,
            })
        });
        if started.is_err() {
            // The thread ends right after reporting a failed start; join it so the next
            // start is not refused as busy.
            let mut threads = self.threads();
            if let Some(r) = threads.newest.take() {
                let _ = r.thread.join();
            }
        }
        started
    }

    /// Asks the recording meeting to stop; the recording is saved and the work after the
    /// meeting takes its turn.
    ///
    /// # Errors
    /// `not_recording` when no meeting records.
    pub fn stop(&self) -> Result<(), IpcError> {
        let threads = self.threads();
        match threads.newest.as_ref() {
            Some(r) if r.recording.load(Ordering::SeqCst) => {
                r.stop.store(true, Ordering::SeqCst);
                Ok(())
            }
            _ => Err(IpcError {
                kind: "not_recording".into(),
                message: "no meeting is recording".into(),
                retryable: false,
            }),
        }
    }

    /// The user answered "Keep recording" to the offer to stop (T9.4). It changes nothing about
    /// the recording: the question is settled until the call app's session returns and ends.
    /// Nothing to answer (the recording has just ended) is not an error.
    pub fn keep_recording(&self) {
        if let Some(r) = self.threads().newest.as_ref() {
            r.keep.store(true, Ordering::SeqCst);
        }
    }

    /// Where the meetings are.
    #[must_use]
    pub fn phase(&self) -> DriverPhase {
        let threads = self.threads();
        if threads.recording() {
            DriverPhase::Recording
        } else if threads.any_unfinished() {
            DriverPhase::Processing
        } else {
            DriverPhase::Idle
        }
    }

    /// Stops a recording meeting and returns once its recording is saved (at once when
    /// none records). The work after the meeting goes on; the caller decides about that.
    pub fn stop_and_wait(&self) {
        let _ = self.stop();
        while self.phase() == DriverPhase::Recording {
            std::thread::sleep(STOP_POLL);
        }
    }

    /// Waits for every meeting's thread to finish.
    pub fn join(&self) {
        let (newest, finishing) = {
            let mut threads = self.threads();
            (
                threads.newest.take(),
                std::mem::take(&mut threads.finishing),
            )
        };
        for thread in finishing {
            let _ = thread.join();
        }
        if let Some(r) = newest {
            let _ = r.thread.join();
        }
    }
}

/// What the UI hears when the meeting thread panics after the meeting started.
pub const MEETING_PANIC_MESSAGE: &str =
    "The meeting stopped unexpectedly. What was recorded until then is kept.";

/// What the UI hears when the session ends in an error. Once the recording is saved that error can
/// only be about the live work, so it never says the recording failed.
fn session_failure_message(saved: bool, e: &RuntimeError) -> String {
    if saved {
        "The recording is saved, but the live transcript did not finish. Re-transcribe the \
         meeting from its page to get the full transcript."
            .to_owned()
    } else {
        format!("The recording could not be finished: {e}")
    }
}

/// The flags a meeting's thread shares with the driver.
struct RunFlags {
    stop: Arc<AtomicBool>,
    keep: Arc<AtomicBool>,
    recording: Arc<AtomicBool>,
}

/// Runs the offer to stop for a call (T9.4, [`earshot_runtime::call_end`]). It says what changes;
/// the meeting's thread is what stops the session on [`MeetingEvent::CallEndStopped`].
struct CallEndOffer {
    watch: CallEndWatch,
    signal: CallSignal,
    started: Instant,
}

impl CallEndOffer {
    fn new(signal: CallSignal, config: CallEndConfig, started: Instant) -> Self {
        Self {
            watch: CallEndWatch::new(config),
            signal,
            started,
        }
    }

    /// The far end's voice activity, from the meeting's events.
    fn event(&mut self, event: &RuntimeEvent, now: Instant) {
        if let RuntimeEvent::VoiceActivity {
            track: TrackId::Reference,
            speaking,
            ..
        } = event
        {
            self.watch
                .far_end(*speaking, now.saturating_duration_since(self.started));
        }
    }

    /// One look at the sessions and the user's answer: the event to send, if the question changes.
    fn tick(&mut self, keep: &AtomicBool, now: Instant) -> Option<MeetingEvent> {
        let at = now.saturating_duration_since(self.started);
        if let Some(reading) = (self.signal)() {
            self.watch.sessions(reading, at);
        }
        if keep.swap(false, Ordering::SeqCst) {
            self.watch.keep_recording();
            return Some(MeetingEvent::CallEndOffer { open: false });
        }
        self.watch.check(at).map(|change| match change {
            CallEndChange::Offer | CallEndChange::OfferAgain => {
                MeetingEvent::CallEndOffer { open: true }
            }
            CallEndChange::Withdraw => MeetingEvent::CallEndOffer { open: false },
            CallEndChange::Stop => MeetingEvent::CallEndStopped,
        })
    }
}

/// [`run`] with a panic turned into the meeting's last event: a failed start before the
/// meeting records, else a failed [`MeetingEvent::Summary`]. The thread then ends, so the
/// driver is free again (a live session dropped during the unwind is stopped by its `Drop`, and
/// the meeting's place in the queue ends with the unwind).
fn run_guarded(
    inner: &Inner,
    svc: &Services,
    request: &StartRequest,
    flags: &RunFlags,
    started: &mpsc::SyncSender<Result<String, IpcError>>,
    seq: u64,
) {
    let meeting = OnceLock::new();
    let result = std::panic::catch_unwind(AssertUnwindSafe(|| {
        run(inner, svc, request, flags, started, &meeting, seq);
    }));
    if result.is_err() {
        flags.recording.store(false, Ordering::SeqCst);
        if let Some(meeting) = meeting.get() {
            inner.send(
                seq,
                MeetingEvent::Summary {
                    meeting: meeting.clone(),
                    result: SummaryEvent::Failed {
                        message: MEETING_PANIC_MESSAGE.into(),
                        retryable: false,
                    },
                },
            );
        } else {
            let _ = started.send(Err(IpcError {
                kind: "internal".into(),
                message: "The meeting stopped unexpectedly before it started.".into(),
                retryable: false,
            }));
        }
    }
}

/// The meeting thread: pre-flight, start, events until `Ended`, then the work after the
/// meeting in its turn, and the summary result. `meeting` is set once the start was reported.
fn run(
    inner: &Inner,
    svc: &Services,
    request: &StartRequest,
    flags: &RunFlags,
    started: &mpsc::SyncSender<Result<String, IpcError>>,
    meeting: &OnceLock<String>,
    seq: u64,
) {
    let (stop, recording) = (&*flags.stop, &*flags.recording);
    let send = |e: MeetingEvent| inner.send(seq, e);
    // The meeting's place: everything it loads and runs after capture goes in this order.
    let ticket = Arc::new(inner.queue.enter());
    let prepared = match inner.prepare.prepare(svc, request, &ticket) {
        Ok(p) => p,
        Err(e) => {
            recording.store(false, Ordering::SeqCst);
            let _ = started.send(Err(e));
            return;
        }
    };
    let mut session = match (prepared.start)(svc) {
        Ok(s) => s,
        Err(e) => {
            recording.store(false, Ordering::SeqCst);
            let _ = started.send(Err(IpcError::from(e)));
            return;
        }
    };
    let id = session.meeting();
    let _ = meeting.set(id.to_string());
    // From here on the screen follows this meeting.
    inner.shown.store(seq, Ordering::SeqCst);
    let _ = started.send(Ok(id.to_string()));

    // Taken when the recording is saved, so the meeting never reads as idle while it waits.
    let mut waiting = None;
    let mut saved_seen = false;
    let mut mapper = EventMapper::new(request.mode);
    let mut notices = Some(prepared.notices);
    // Calls only: in the room nothing ends when a call app does.
    let mut call_end = prepared
        .call_signal
        .filter(|_| request.mode == CaptureMode::Call)
        .map(|signal| CallEndOffer::new(signal, CallEndConfig::default(), (inner.now)()));
    if let Some(events) = session.take_events() {
        let mut stop_sent = false;
        let mut stopped_by_call_end = false;
        loop {
            if !stop_sent && stop.load(Ordering::SeqCst) {
                session.request_stop();
                stop_sent = true;
                // Answered: the polling ends with the question.
                call_end = None;
            }
            if let Some(event) = call_end
                .as_mut()
                .and_then(|offer| offer.tick(&flags.keep, (inner.now)()))
            {
                let stops = matches!(event, MeetingEvent::CallEndStopped);
                send(event);
                if stops {
                    session.request_stop();
                    stop_sent = true;
                    stopped_by_call_end = true;
                    call_end = None;
                }
            }
            match events.recv_timeout(EVENT_WAIT) {
                Ok(event) => {
                    let is_end = matches!(event, RuntimeEvent::Ended(_));
                    let saved = matches!(event, RuntimeEvent::RecordingSaved { .. });
                    if let Some(offer) = call_end.as_mut() {
                        offer.event(&event, (inner.now)());
                    }
                    if saved {
                        call_end = None;
                        saved_seen = true;
                        waiting = svc.lease_meeting_for(id, MeetingJob::Waiting).ok();
                    }
                    let mut mapped = mapper.map(&event, (inner.now)());
                    // The UI's "saved" event goes last, so what it hears can start a new meeting at
                    // once; the banners go before the flag, so a newer meeting cannot start before
                    // they are out.
                    let last = mapped.pop();
                    for e in mapped {
                        send(e);
                    }
                    if saved {
                        recording.store(false, Ordering::SeqCst);
                    }
                    if let Some(e) = last {
                        send(e);
                    }
                    // After the end, so it shows with the work after the recording.
                    if saved && stopped_by_call_end {
                        send(crate::meeting_events::call_end_stopped_warning());
                    }
                    if matches!(event, RuntimeEvent::Started { .. })
                        && let Some(n) = notices.take()
                    {
                        n.into_iter().for_each(send);
                    }
                    if is_end {
                        break;
                    }
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => break,
            }
        }
    }
    let outcome = session.wait();
    recording.store(false, Ordering::SeqCst);
    let outcome = match outcome {
        Ok(o) => o,
        Err(e) => {
            send(MeetingEvent::Summary {
                meeting: id.to_string(),
                result: SummaryEvent::Failed {
                    message: session_failure_message(saved_seen, &e),
                    retryable: false,
                },
            });
            return;
        }
    };
    // Its turn: uninterruptible, because nothing can be lost by waiting and quitting ends the
    // process.
    ticket.wait_turn(&CancelToken::new());
    let mut progress = PostProgressMapper::of(id.to_string());
    let host = (prepared.after)(svc, &outcome, &mut |step| {
        if matches!(step, HostStep::PostFinished(_)) {
            ticket.final_pass_done();
        }
        if let Some(e) = progress.step(&step) {
            send(e);
        }
    });
    if let Ok(earshot_runtime::host::HostPost::Done(post)) = &host.post {
        for (key, message) in crate::plain_text::post_warning_copy(&post.warnings) {
            send(MeetingEvent::Warning {
                key,
                kind: crate::meeting_events::WarningKind::Backend,
                message,
            });
        }
    }
    // The local record of what was handed to Claude Code could not be stored (the summary's
    // own result follows unchanged).
    for why in &host.handover_record_failures {
        send(crate::meeting_events::handover_record_warning(why));
    }
    if let Some(message) = crate::meeting_events::summary_fallback_note(&host.summary) {
        send(MeetingEvent::Warning {
            key: "summary-processor".into(),
            kind: crate::meeting_events::WarningKind::Backend,
            message,
        });
    }
    // The next meeting goes once this one's summary event is out, so events stay in queue order.
    send(MeetingEvent::Summary {
        meeting: id.to_string(),
        result: summary_event(&host),
    });
    drop(waiting);
}

fn setup_err(kind: &str, message: String) -> IpcError {
    IpcError {
        kind: kind.into(),
        message,
        retryable: false,
    }
}

/// The banner text of an ONNX Runtime library that could not load (the CPU one is used).
#[must_use]
pub(crate) fn onnx_fallback_message(
    library: earshot_runtime::backends::Accelerator,
    e: &RuntimeError,
) -> String {
    let why = crate::plain_text::library_problem(e);
    format!(
        "The {library:?} library of ONNX Runtime is unavailable, so speech models run on the CPU \
         ({why})."
    )
}

/// The UI error of a summary setup failure.
#[must_use]
pub(crate) fn summary_setup_error(e: SummarySetupError) -> IpcError {
    match e {
        SummarySetupError::LlamaDllsMissing(missing) => setup_err(
            "summary_dlls",
            format!(
                "A summary on this computer needs llama.cpp's DLLs next to Earshot; not found: {}",
                missing.join(", ")
            ),
        ),
        SummarySetupError::CudaDllsMissing(missing) => setup_err(
            "summary_dlls",
            format!(
                "A summary on this computer needs the CUDA runtime's DLLs on PATH; not found: {}",
                missing.join(", ")
            ),
        ),
        SummarySetupError::Runtime(e) => IpcError::from(e),
    }
}

/// The summary step of a meeting (and of a re-summary), from the Summaries and Models page
/// settings (`host::summary_from_settings`): no summary, the local model and where it
/// runs, or the user's own endpoint (an Anthropic key read from `keys` now); its service,
/// or why none runs, with the notes the user is told (a forced graphics card not used, a
/// stored setting this version does not know).
///
/// An Anthropic endpoint whose key cannot be read is not an error (a
/// summary setting never costs a recording): no service, `no_summary` is
/// `SkipReason::NoApiKey` (the start banner is `meeting_events::no_key_banner`); never a
/// local model or another provider. The key store is read once. Likewise Claude Code (T7.4ga)
/// that `claude` reports not installed or not signed in (`host::check_claude_code`, run
/// here without the store lock): `SkipReason::ClaudeCodeUnavailable`; and Codex (T7.4gc)
/// that `codex` reports so (`host::check_codex`): `SkipReason::CodexUnavailable`; and a summary
/// model chosen by name and not installed (the owner, T9.1q): `SkipReason::ChosenNotInstalled`.
///
/// # Errors
/// An unknown requested model; llama.cpp's or CUDA's DLLs missing; a service that cannot be
/// built.
pub(crate) fn summary_setup(
    svc: &Services,
    models: &ModelDir,
    keys: &dyn ApiKeyStore,
    claude: &dyn ClaudeCodeProbe,
    codex: &dyn CodexProbe,
) -> Result<SummarySetup, IpcError> {
    let settings = {
        let db = svc.store();
        host::summary_from_settings(
            &db,
            &earshot_runtime::backends::accelerator_order(),
            &earshot_runtime::hardware(),
            keys,
        )?
    };
    let plan = match host::summary_plan(
        &settings.request,
        earshot_store::total_memory(),
        settings.placement.gpu,
        models,
    ) {
        Ok(plan) => plan,
        Err(SummaryPlanError::UnknownModel(e)) => return Err(IpcError::from(e)),
        Err(SummaryPlanError::NotInstalled(e)) => {
            let message = crate::plain_text::summary_model_not_installed(&e);
            let error = IpcError {
                message,
                ..IpcError::from(e)
            };
            let chosen = settings
                .request
                .local_model
                .as_deref()
                .and_then(|id| earshot_runtime::summary_model::summary_model(id).ok());
            let Some(chosen) = chosen else {
                return Err(error);
            };
            return Ok(SummarySetup {
                summary: None,
                no_summary: SkipReason::ChosenNotInstalled { model: chosen.id },
                notes: settings.notes,
                live: settings.live,
                local_chosen: settings.writer == SummaryWriter::OnThisComputer,
                chosen_missing: Some(error),
            });
        }
    };
    let plan = host::check_claude_code(plan, claude);
    let plan = host::check_codex(plan, codex);
    let summary = host::prepare_summary_on(
        &plan,
        models,
        Arc::clone(&svc.clock),
        &host::dll_search_dirs(),
        settings.placement.gpu,
    )
    .map_err(summary_setup_error)?;
    Ok(SummarySetup {
        no_summary: host::skip_reason(&plan).unwrap_or(SkipReason::NotRequested),
        summary,
        notes: settings.notes,
        live: settings.live,
        local_chosen: settings.writer == SummaryWriter::OnThisComputer,
        chosen_missing: None,
    })
}

/// What [`summary_setup`] decided.
pub(crate) struct SummarySetup {
    /// The service, `None` when no summary runs.
    pub summary: Option<PreparedSummary>,
    /// Why none runs (when `summary` is `None`).
    pub no_summary: SkipReason,
    /// What the user is told.
    pub notes: Vec<String>,
    /// "Summarise during the meeting" is on.
    pub live: bool,
    /// The summary is to be written on this computer.
    pub local_chosen: bool,
    /// The summary model chosen by name is not installed: what an explicit re-summarise fails
    /// with (a meeting skips the summary instead, `SkipReason::ChosenNotInstalled`).
    pub chosen_missing: Option<IpcError>,
}

/// The live summaries of a meeting with `setup` on `order` (T8.3jc): only with the setting
/// on and a local summary model (`host::live_summaries_for`). The second value is the
/// banner when the setting is on with a summary on this computer that cannot run during
/// the meeting (the model is not installed); with another choice the Summaries page already
/// shows the switch as unavailable, so nothing is said.
#[must_use]
pub(crate) fn live_summaries(
    setup: &SummarySetup,
    order: &[earshot_runtime::backends::Accelerator],
) -> (
    Option<earshot_runtime::live_summaries::LiveSummaries>,
    Option<String>,
) {
    if !setup.live {
        return (None, None);
    }
    match host::live_summaries_for(setup.summary.as_ref(), order) {
        Ok(l) => (Some(l), None),
        Err(_) if !setup.local_chosen => (None, None),
        Err(why) => (
            None,
            Some(format!(
                "Summarising during the meeting is off for this meeting: {why}."
            )),
        ),
    }
}

/// The banners of the pre-flight's setup notes (a "where it runs" choice not used, a stored
/// setting this version does not know), one per note: keys `settings-0`, `settings-1`, ...
#[must_use]
pub(crate) fn setup_notices(notes: Vec<String>) -> Vec<MeetingEvent> {
    notes
        .into_iter()
        .enumerate()
        .map(|(i, message)| MeetingEvent::Warning {
            key: format!("settings-{i}"),
            kind: crate::meeting_events::WarningKind::Backend,
            message,
        })
        .collect()
}

/// What a meeting captures (T7.4i): the capture mode the meeting asked for and the
/// microphone and output chosen on the Audio page.
#[cfg(windows)]
#[derive(Debug)]
pub(crate) struct MeetingCapture {
    /// A call (with automatic loopback: a call app's audio, else the output's) or the room.
    pub mode: earshot_capture_wasapi::CaptureMode,
    /// The chosen endpoints (a chosen one that is gone is replaced by the default with a
    /// warning when capture starts).
    pub devices: earshot_capture_wasapi::DeviceSelection,
    /// A banner per stored Audio setting this version cannot use (keys `audio-settings-0`,
    /// ...).
    pub notices: Vec<MeetingEvent>,
}

/// The capture of `request` from the Audio page settings (`host::audio_setup`). The stored
/// capture mode only preselects the new-meeting screen; the meeting records the mode it asks
/// for.
///
/// # Errors
/// Store errors.
#[cfg(windows)]
pub(crate) fn meeting_capture(
    svc: &Services,
    request: &StartRequest,
) -> Result<MeetingCapture, IpcError> {
    let (audio, notes) = {
        let db = svc.store();
        host::audio_setup(&db)?
    };
    let mode = match request.mode {
        // The whole output, as T6.14 decided after a Teams call: `Auto` attaches to
        // ms-teams.exe, which delivers digital silence on real calls because Windows hides
        // its Communications streams from process loopback (T6.15a). `FollowCallOutput` (T6.15)
        // becomes the app's default only after GATE-01a on a real call on a non-default output.
        CaptureMode::Call => earshot_capture_wasapi::CaptureMode::Call(
            earshot_capture_wasapi::LoopbackRequest::DefaultDevice,
        ),
        CaptureMode::InRoom => earshot_capture_wasapi::CaptureMode::InRoom,
    };
    let notices = notes
        .into_iter()
        .enumerate()
        .map(|(i, message)| MeetingEvent::Warning {
            key: format!("audio-settings-{i}"),
            kind: crate::meeting_events::WarningKind::Audio,
            message,
        })
        .collect();
    Ok(MeetingCapture {
        mode,
        devices: audio.devices(),
        notices,
    })
}

/// Where a meeting's audio comes from.
pub struct MeetingSource {
    /// Starts the capture.
    pub starter: host::Starter,
    /// Its clock.
    pub clock_domain: earshot_core::model::ClockDomain,
    /// Banners about it.
    pub notices: Vec<MeetingEvent>,
    /// What the call app's sessions do, for the offer to stop (T9.4); `None`: no offer.
    pub call_signal: Option<CallSignal>,
}

/// Chooses a meeting's capture source during the pre-flight.
pub type SourceFn =
    Box<dyn Fn(&Services, &StartRequest) -> Result<MeetingSource, IpcError> + Send + Sync>;

/// The production pre-flight (module docs, step 1): the CLI's `record` checks through
/// `earshot_runtime::host`; the capture source is its one variable, WASAPI on the Audio page's
/// devices in the app ([`HostPrepare::wasapi`]) and a replay in `accept m7`
/// (`crate::headless`).
pub struct HostPrepare {
    source: SourceFn,
}

impl HostPrepare {
    /// The app's meetings: live capture.
    #[must_use]
    pub fn wasapi() -> Self {
        Self::with_source(Box::new(wasapi_source))
    }

    /// Everything of a real meeting except where the audio comes from.
    #[must_use]
    pub fn with_source(source: SourceFn) -> Self {
        Self { source }
    }
}

/// Live capture on the Audio page's devices (T7.4i).
#[cfg(windows)]
fn wasapi_source(svc: &Services, request: &StartRequest) -> Result<MeetingSource, IpcError> {
    let capture = meeting_capture(svc, request)?;
    Ok(MeetingSource {
        starter: host::wasapi_starter_on(
            capture.mode,
            earshot_capture_wasapi::MicRole::default(),
            capture.devices,
        ),
        clock_domain: earshot_core::model::ClockDomain::Qpc,
        notices: capture.notices,
        call_signal: call_signal(request.mode),
    })
}

/// The real sessions behind the offer to stop, for a call. A poller that cannot start costs only
/// the offer: never a recording.
#[cfg(windows)]
fn call_signal(mode: CaptureMode) -> Option<CallSignal> {
    use earshot_capture_wasapi::{CALL_POLL_INTERVAL, RenderSessionPoller, call_app_active};
    use earshot_runtime::call_end::CallSessions;

    if mode != CaptureMode::Call {
        return None;
    }
    let mut poller = RenderSessionPoller::start(CALL_POLL_INTERVAL).ok()?;
    Some(Box::new(move || {
        poller.latest().map(|reading| match reading {
            Ok(sessions) if call_app_active(&sessions) => CallSessions::Active,
            Ok(_) => CallSessions::NoneActive,
            Err(_) => CallSessions::Unknown,
        })
    }))
}

#[cfg(not(windows))]
fn wasapi_source(_: &Services, _: &StartRequest) -> Result<MeetingSource, IpcError> {
    Err(IpcError {
        kind: "unsupported".into(),
        message: "live capture needs Windows (WASAPI)".into(),
        retryable: false,
    })
}

mod production {
    use std::sync::Arc;

    use earshot_core::asr::CancelToken;
    use earshot_runtime::Services;
    use earshot_runtime::host::{self, AfterMeeting, HostOutcome, MeetingModels};
    use earshot_runtime::live::{LiveConfig, LiveHold};
    use earshot_runtime::meeting_queue::{MeetingHold, QueueTicket};

    use super::{HostPrepare, Prepare, Prepared, StartRequest};
    use crate::ipc::IpcError;

    impl Prepare for HostPrepare {
        fn prepare(
            &self,
            svc: &Services,
            request: &StartRequest,
            ticket: &Arc<QueueTicket>,
        ) -> Result<Prepared, IpcError> {
            let models = host::model_dir(&svc.data);
            // What this computer can use now, without waiting for the CUDA start: enough to refuse
            // a meeting whose files are missing. What it runs on is read when its turn comes
            // (`MeetingModels`), so it keeps what the start decides.
            let build = earshot_runtime::backends::accelerator_order();
            let setup = {
                let db = svc.store();
                host::transcription_setup(&db, &models, &build, &earshot_runtime::hardware())?
            };
            host::require_models(&models, &setup.choice.model, true)?;
            let checked = host::ResolvedModels {
                model: setup.choice.model.clone(),
                order: setup.order.clone(),
            };
            let summary_setup = super::summary_setup(
                svc,
                &models,
                &crate::credentials::CredentialManager::anthropic(),
                &earshot_runtime::claude_code::InstalledClaudeCode::meeting_start(),
                &earshot_runtime::codex::InstalledCodex::meeting_start(),
            )?;
            let (live_summaries, live_note) = super::live_summaries(&summary_setup, &setup.order);
            // A setting never costs a recording: one that cannot be read is automatic.
            let previews = earshot_runtime::preview_settings::preview_choice(&svc.store())
                .map_or_else(|_| Default::default(), |(choice, _)| choice);
            let super::SummarySetup {
                summary,
                no_summary,
                notes: summary_notes,
                ..
            } = summary_setup;
            // "Where it runs" not honoured, or a stored setting this version does not know.
            let mut notices = super::setup_notices(
                setup
                    .notes
                    .into_iter()
                    .chain(summary_notes)
                    .chain(live_note)
                    .collect(),
            );
            // An endpoint without its API key: the meeting runs, the summary is skipped.
            notices.extend(crate::meeting_events::no_key_banner(&no_summary));
            // Claude Code chosen but not installed or not signed in: the same rule.
            notices.extend(crate::meeting_events::claude_code_banner(&no_summary));
            // Codex likewise (T7.4gc).
            notices.extend(crate::meeting_events::codex_banner(&no_summary));
            // A summary model chosen by name and not downloaded: the same rule.
            notices.extend(crate::meeting_events::chosen_model_banner(&no_summary));
            let (timing, _) = {
                let mut db = svc.store();
                host::choose_speaker_timing(&mut db, None, false)?
            };
            // The capture source: where the audio comes from is its only say.
            let source = (self.source)(svc, request)?;
            notices.extend(source.notices);
            let mut hold = LiveHold::new(Arc::new(MeetingHold::new(Arc::clone(ticket))));
            let meeting_models = Arc::new(MeetingModels::new(
                svc,
                models.clone(),
                (build, checked),
                Arc::clone(&hold.notes),
                Box::new(super::onnx_fallback_message),
            ));
            hold.speakers = Some(meeting_models.speaker_factory());
            let config = LiveConfig {
                title: request.title.clone(),
                clock_domain: source.clock_domain,
                speaker_timing: timing,
                live_summaries,
                hold: Some(hold),
                previews: Some(host::live_previews(&models, previews)),
                ..LiveConfig::default()
            };
            let factory = meeting_models.live_factory();
            let starter = source.starter;
            let call_signal = source.call_signal;
            let start =
                Box::new(move |svc: &Services| host::start_meeting(svc, config, starter, factory));
            let after = Box::new(
                move |svc: &Services,
                      outcome: &earshot_runtime::live::LiveOutcome,
                      progress: &mut dyn FnMut(host::HostStep<'_>)| {
                    // What the live work ran on; decided now if its turn never came.
                    let resolved = match meeting_models.resolve() {
                        Ok(r) => r,
                        Err(e) => return HostOutcome::not_started(e),
                    };
                    // The app has no cancel control yet: the tokens are never set.
                    let (post_cancel, summary_cancel) = (CancelToken::new(), CancelToken::new());
                    let run = AfterMeeting {
                        models: &models,
                        model: &resolved.model,
                        order: &resolved.order,
                        post: true,
                        whole_pass: false,
                        summary: summary.as_ref().map(|s| &s.service),
                        no_summary,
                        post_cancel: &post_cancel,
                        summary_cancel: &summary_cancel,
                    };
                    host::after_meeting(svc, outcome, &run, progress)
                },
            );
            Ok(Prepared {
                start,
                after,
                notices,
                call_signal,
            })
        }
    }
}

#[cfg(test)]
#[path = "meeting_tests.rs"]
mod tests;
