//! Provisional words while someone is talking (T9.1m): a display-only guess at the unfinished
//! speech stretch, which the real stretch decode replaces when it returns.
//!
//! **Display only.** Nothing here reaches the store, a [`crate::stretches::StretchRecord`], the
//! echo filter, a summary, an export or the final pass: its one way out is
//! [`super::RuntimeEvent::LivePreview`]. It reads two counters of the track's worker, and writes
//! to nothing the worker, the pipeline or the recording uses. Provisional text is never logged.
//!
//! **Its own thread and its own model session.** A track's [`earshot_core::asr::Transcriber`]
//! takes `&mut self` (ONNX Runtime's Rust binding too, `ort::Session::run`), so sharing one means
//! a lock a real decode could wait on; a preview therefore never shares a track's transcriber.
//! It runs below normal priority, on background ONNX Runtime threads, with a session of its own
//! (docs/live-previews.md has the memory this costs). It loads there too, so a slow or failed load
//! costs only the previews.
//!
//! **Which tracks.** Each track has previews of its own ([`PreviewState`] by track), each with its
//! own thread and model session: the far end in a call and the mic in the room
//! ([`LivePreviews::load`]), plus the mic in a call when the user chose it for their own voice
//! ([`LivePreviews::own_voice`], T9.14; on the CPU even when the track runs on CUDA, T12.3a). A
//! track's previews stopping never stops the other's, and each track sends its own [`PreviewReport`]. Without `own_voice` a mic in a call shows nothing:
//! far-end words leak into it until the echo filter removes them, and that filter works on settled
//! text (`live_preview_mic_in_a_call_shows_no_far_end_word`).
//!
//! **What is decoded.** About every [`LivePreviews::interval`], while the track's voice activity says
//! there is unfinished speech, the last [`LivePreviews::window`] of the audio after the worker's
//! decoded frontier. A preview goes after a real decode: while the worker's queue shows a decode in
//! progress ([`REAL_DECODE_QUEUE`]) it is skipped, one already running is cancelled, and it is never
//! queued: the next one starts from the audio of that moment, at least as long after the last one
//! as that one took.
//!
//! **What is shown.** Windows are joined and unstable words held back by the live chunker's own
//! [`LocalAgreement`]: a word is shown once two decodes agree on it, and a shown word stays until
//! the real decode covers it ([`PreviewWords`]). So a decode that stays on blank
//! (`Transcriber::may_skip_speech`) shows nothing new and takes nothing away. Words the real decode
//! covers leave as it lands; ones it does not hold are gone with them.
//!
//! **A streaming model instead (evaluation only, T12.23).** With [`LivePreviews::stream`] set, a
//! track's thread feeds every packet of its audio, once, to a [`StreamingPreview`] and shows the
//! words it finished, rather than decoding windows: nothing else changes (when previews start and
//! stop, what settled text removes, the events and the report). The app never sets it; only
//! evaluation loaders build one (docs/live-previews.md).
//!
//! **When it is off.** On battery; during a hold and its catch-up (T9.1l); for a track that falls
//! behind; for a chunker session, which has partials; for a backend the loader refuses. Each is
//! reported in [`PreviewReport::off`], and none affects recording or stored transcripts. Previews
//! that take too long pause and try again ([`PreviewReport::slow_pauses`]).
//!
//! **Telling the user (T12.33).** As it happens, each track with previews says whether they run
//! or why not, as a kind ([`PreviewOffReason`]) in [`super::RuntimeEvent::LivePreviewState`], once
//! per change: waiting for the transcript to catch up, running once the model is loaded, off for
//! the meeting. A track without previews (the mic in a call without `own_voice`) says nothing.
//! [`LivePreviews::rule`] is asked before a wait or the battery is said, once per track (its
//! backend and model never change once known), so neither is said for previews this computer
//! would not run anyway: turned off, off by default, a part without them, and in the app a whisper
//! model (`backends::preview_rule`, the loader's own rule). Under the automatic choice what the
//! rule refuses is said as off by default ([`LivePreviews::user_asked`]). While a hold keeps
//! previews waiting, the power supply is read at most every [`super::LiveConfig::power_poll`],
//! and a computer on battery then says the battery rather than the wait.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, SyncSender, TrySendError, channel};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use earshot_core::asr::{
    AsrError, BackendInfo, CancelToken, LocalAgreement, MeetingTime, TranscribeOptions,
    Transcriber, Word,
};
use earshot_core::audio::{MonoBuffer, PIPELINE_RATE, Sample, quantise_pcm16};
use earshot_core::capture::{AudioSource, TrackId};
use earshot_core::identity::EntityId;

use super::hold::{CAUGHT_UP_BACKLOG, LiveTranscriptState};
use super::{LiveTranscription, RuntimeEvent, Session, slot};
use crate::power::PowerSupply;

/// A worker queue longer than this means a real decode is in progress: the session hands it about
/// 10 ms at a time and it takes them at once otherwise. A judgement.
pub const REAL_DECODE_QUEUE: Duration = Duration::from_millis(200);

/// The least audio decoded for a preview: a shorter one gives words nobody could read yet.
pub const MIN_PREVIEW_AUDIO: Duration = Duration::from_secs(1);

/// A preview word starting up to this long before the end of settled text is kept: word times
/// move by this much between decodes (`LocalAgreement`'s `DUPLICATE_SLACK`). So a settled last word
/// shorter than this can show twice until the next preview.
pub const COVERED_SLACK: Duration = Duration::from_millis(100);

/// Why previews stop on battery.
pub const ON_BATTERY_REASON: &str = "off on battery";

/// What a report says of a preview thread that panicked.
const PREVIEW_THREAD_CRASHED: &str = "the preview thread crashed";

/// Why previews stop for a slow track.
pub const FELL_BEHIND_REASON: &str = "the live transcript fell behind";

fn on_battery() -> PreviewRefusal {
    PreviewRefusal::new(PreviewOffReason::OnBattery, ON_BATTERY_REASON)
}

/// Previews this slow in a row pause them ([`LivePreviews::slow_pause`]).
pub const SLOW_PREVIEWS: u32 = 3;

/// Audio messages the session may have queued for the thread (about 40 s of 10 ms packets); past
/// this it is stuck, and audio is dropped rather than held.
const AUDIO_QUEUE: usize = 4096;

/// Why a track has no provisional words now, as a kind (T12.33): a host picks its own words for
/// each, and never shows the text beside it ([`PreviewRefusal::text`], [`PreviewReport::off`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PreviewOffReason {
    /// The computer runs on battery.
    OnBattery,
    /// The live transcript is catching up after a hold (T9.1l); previews start once it is up to
    /// date. Said only once the track's backend is known and nothing else would keep them off.
    CatchingUp,
    /// The track's live transcript fell behind ([`LivePreviews::fell_behind`]).
    FellBehind,
    /// The live transcript stopped for this meeting.
    TranscriptStopped,
    /// On the NPU a preview loads only the model compiled on this computer, and it did not load.
    NeedsCompiledModel,
    /// The user's own voice has no previews beside the NPU yet.
    OwnVoiceOnNpu,
    /// The user asked for their own voice too, and this computer has too little memory for it.
    NotEnoughMemory,
    /// Off by default here, and the user did not turn them on: a computer with this little memory,
    /// or under the automatic choice anything the rule refuses ([`LivePreviews::user_asked`]).
    OffByDefault,
    /// The meeting's transcription model or the part it runs on has no previews.
    Model,
    /// Turned off in Settings.
    TurnedOff,
    /// The track is one the user did not ask previews for.
    NotAsked,
    /// The session has nothing a preview could run beside (a chunker session, which has partials;
    /// no live transcript on the track).
    NotApplicable,
    /// The preview model did not load.
    LoadFailed,
    /// Previews stopped working (a failed decode, a crashed thread, a priority that cannot be
    /// lowered).
    Failed,
    /// The meeting ended before previews started.
    MeetingEnded,
}

/// Why a track's previews are off: the kind for a host's own words, and the runtime's text for
/// reports and developer tools ([`PreviewReport::off`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreviewRefusal {
    /// The kind.
    pub reason: PreviewOffReason,
    /// The runtime's words: never shown in a window.
    pub text: String,
}

impl PreviewRefusal {
    /// A refusal of kind `reason` in the runtime's words `text`.
    #[must_use]
    pub fn new(reason: PreviewOffReason, text: impl Into<String>) -> Self {
        Self {
            reason,
            text: text.into(),
        }
    }
}

impl std::fmt::Display for PreviewRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.text)
    }
}

/// Reads as its text, so code that reported the reason as a string still does.
impl std::ops::Deref for PreviewRefusal {
    type Target = str;

    fn deref(&self) -> &str {
        &self.text
    }
}

impl PartialEq<&str> for PreviewRefusal {
    fn eq(&self, other: &&str) -> bool {
        self.text == *other
    }
}

/// A loader's plain words are a load that failed.
impl From<String> for PreviewRefusal {
    fn from(text: String) -> Self {
        Self::new(PreviewOffReason::LoadFailed, text)
    }
}

impl From<&str> for PreviewRefusal {
    fn from(text: &str) -> Self {
        Self::new(PreviewOffReason::LoadFailed, text)
    }
}

/// The runtime's text alone, for callers that report a reason and need no kind.
impl From<PreviewRefusal> for String {
    fn from(refusal: PreviewRefusal) -> Self {
        refusal.text
    }
}

/// Loads a track's preview model (its own session, never the track's) for the backend the track
/// runs on. An `Err` says why this meeting has no previews.
pub type PreviewLoader =
    Arc<dyn Fn(&BackendInfo) -> Result<Box<dyn Transcriber + Send>, PreviewRefusal> + Send + Sync>;

/// Whether this computer runs a track's previews on the backend the track runs on, without
/// loading anything: `Some` says why not ([`LivePreviews::rule`], T12.33). The same answer the
/// loader would give, known before the load, so the session never tells the user about a wait or
/// a battery for previews this computer would not run anyway.
pub type PreviewRule = Arc<dyn Fn(&BackendInfo) -> Option<PreviewRefusal> + Send + Sync>;

/// Evaluation only (T12.23): a streaming model fed a track's audio once, as it comes
/// ([`LivePreviews::stream`]).
pub trait StreamingPreview {
    /// Where the model runs, as it reports itself.
    fn backend_info(&self) -> BackendInfo;
    /// Forgets everything heard: the audio that follows does not follow what came before.
    fn restart(&mut self);
    /// Adds 16 kHz samples that follow the last ones and decodes all the model can; returns how
    /// many model steps that took (0: not enough audio for one yet).
    ///
    /// # Errors
    /// Why the model cannot go on.
    fn push(&mut self, samples: &[Sample]) -> Result<u64, String>;
    /// The words heard since the last restart that can no longer change, times from the
    /// restart, without those before [`StreamingPreview::forget_before`]'s time.
    fn finished_words(&self) -> Vec<Word>;
    /// Words starting before `time` (from the restart) are no longer needed.
    fn forget_before(&mut self, time: MeetingTime);
}

/// Loads a track's streaming model (its own session) for the backend the track runs on
/// ([`LivePreviews::stream`]). An `Err` says why this meeting has no previews.
pub type StreamLoader = Arc<
    dyn Fn(&BackendInfo) -> Result<Box<dyn StreamingPreview + Send>, PreviewRefusal> + Send + Sync,
>;

/// Previews of a session (module docs); [`super::LiveConfig::previews`].
#[derive(Clone)]
pub struct LivePreviews {
    /// Loads the model, on the preview thread: for the far end in a call, the mic in the room.
    pub load: PreviewLoader,
    /// Loads a second session for the user's own track (the mic) in a call (T9.14; `None`: the
    /// other people only). Ignored in the room, where `load` serves the mic.
    pub own_voice: Option<PreviewLoader>,
    /// What `load` would refuse, asked first and without loading ([`PreviewRule`]; `None`: only
    /// the loader says).
    pub rule: Option<PreviewRule>,
    /// The same for `own_voice`.
    pub own_voice_rule: Option<PreviewRule>,
    /// Whether the user turned provisional words on themselves (default true). When false, under
    /// the automatic choice, what `rule` or `own_voice_rule` refuses is said as
    /// [`PreviewOffReason::OffByDefault`]: nobody asked for them, so a computer, model or part
    /// without them needs no word. The report keeps the rule's own text; what happens to previews
    /// that were to run (the battery, a wait, a failure) is said whatever this is.
    pub user_asked: bool,
    /// How often the unfinished stretch is decoded (a starting value, default 1 s).
    pub interval: Duration,
    /// How much of it (a starting value, default 12 s).
    pub window: Duration,
    /// A preview taking longer than this is too late to be worth showing (default 3 s, the target
    /// for words on screen; a judgement). [`SLOW_PREVIEWS`] in a row usually mean other work has the
    /// machine, because a preview runs below normal priority: it never stops real decodes.
    pub slow: Duration,
    /// How long previews then pause, before trying again (default 30 s; a judgement).
    pub slow_pause: Duration,
    /// A track's queue longer than this turns previews off for the meeting (default
    /// [`CAUGHT_UP_BACKLOG`]). Tests that replay faster than real time and are not about this rule
    /// set it high: there a brief stall of a busy host is many seconds of audio.
    pub fell_behind: Duration,
    /// Whether the thread runs below normal priority (always true in production; tests of what
    /// previews compute set false, so that a host whose cores are all busy still gives them time).
    pub background: bool,
    /// Evaluation only (T12.23, module docs): each previewed track's words come from a streaming
    /// model this loads, instead of window decodes of `load`'s or `own_voice`'s session; those
    /// still say which tracks have previews. `None` in the app.
    pub stream: Option<StreamLoader>,
}

impl LivePreviews {
    /// Previews from `load` with the starting values.
    #[must_use]
    pub fn new(load: PreviewLoader) -> Self {
        Self {
            load,
            own_voice: None,
            rule: None,
            own_voice_rule: None,
            user_asked: true,
            interval: Duration::from_secs(1),
            window: Duration::from_secs(12),
            slow: Duration::from_secs(3),
            slow_pause: Duration::from_secs(30),
            fell_behind: CAUGHT_UP_BACKLOG,
            background: true,
            stream: None,
        }
    }
}

impl std::fmt::Debug for LivePreviews {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LivePreviews")
            .field("interval", &self.interval)
            .field("window", &self.window)
            .field("background", &self.background)
            .field("own_voice", &self.own_voice.is_some())
            .field("stream", &self.stream.is_some())
            .finish_non_exhaustive()
    }
}

impl PartialEq for LivePreviews {
    fn eq(&self, other: &Self) -> bool {
        let same = |a: &PreviewLoader, b: &PreviewLoader| {
            std::ptr::addr_eq(Arc::as_ptr(a), Arc::as_ptr(b))
        };
        let same_rule = |a: &Option<PreviewRule>, b: &Option<PreviewRule>| match (a, b) {
            (None, None) => true,
            (Some(a), Some(b)) => std::ptr::addr_eq(Arc::as_ptr(a), Arc::as_ptr(b)),
            _ => false,
        };
        same(&self.load, &other.load)
            && match (&self.own_voice, &other.own_voice) {
                (None, None) => true,
                (Some(a), Some(b)) => same(a, b),
                _ => false,
            }
            && same_rule(&self.rule, &other.rule)
            && same_rule(&self.own_voice_rule, &other.own_voice_rule)
            && self.user_asked == other.user_asked
            && match (&self.stream, &other.stream) {
                (None, None) => true,
                (Some(a), Some(b)) => std::ptr::addr_eq(Arc::as_ptr(a), Arc::as_ptr(b)),
                _ => false,
            }
    }
}

/// What a track's previews did, counts and times only; sent once per track at the session's end.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PreviewReport {
    /// The track they ran on.
    pub track: Option<TrackId>,
    /// Decodes that returned.
    pub decodes: u64,
    /// Seconds of audio those decodes held.
    pub audio_s: f64,
    /// Seconds they took.
    pub decode_s: f64,
    /// The longest one, in seconds.
    pub slowest_s: f64,
    /// Previews due while a real decode was in progress: skipped, not queued.
    pub skipped_busy: u64,
    /// Decodes cancelled because a real decode started.
    pub cancelled: u64,
    /// Pauses after previews that took too long ([`LivePreviews::slow_pause`]).
    pub slow_pauses: u64,
    /// Audio packets dropped because the thread was stuck.
    pub audio_dropped: u64,
    /// Words shown.
    pub words_shown: u64,
    /// Decodes that showed nothing new (two decodes disagreeing, or a decode on blank).
    pub decodes_adding_nothing: u64,
    /// Seconds the model took to load, on the preview thread.
    pub load_s: f64,
    /// Where the loaded session runs, as it reports itself (ONNX Runtime's placement for a real
    /// model; `None`: no session loaded). It can differ from the track's: beside CUDA the user's own
    /// track runs on the CPU (T12.3a).
    pub backend: Option<earshot_core::asr::BackendKind>,
    /// Why previews stopped or never ran (`None`: they ran to the end).
    pub off: Option<String>,
    /// The kind of [`PreviewReport::off`] (`None` exactly when it is).
    pub reason: Option<PreviewOffReason>,
}

/// The words shown for one unfinished stretch (module docs). Only grows, except for what the real
/// decode has covered.
#[derive(Debug, Default)]
pub(super) struct PreviewWords {
    agreement: LocalAgreement,
    shown: Vec<Word>,
    /// The frontier last seen.
    frontier: MeetingTime,
}

impl PreviewWords {
    /// The words shown now.
    pub(super) fn shown(&self) -> &[Word] {
        &self.shown
    }

    /// A new decode's words (meeting time); words before `frontier` are the real decode's. Returns
    /// how many words it added.
    pub(super) fn hypothesis(&mut self, words: Vec<Word>, frontier: MeetingTime) -> usize {
        let words: Vec<Word> = words.into_iter().filter(|w| w.start >= frontier).collect();
        let tail = self.agreement.accept(&words);
        let new = self.agreement.agree(tail);
        let added = new.len();
        self.shown.extend(new);
        added
    }

    /// The real decode has covered everything before `frontier`: those words leave. True when any
    /// did. Once nothing is left the next stretch starts afresh.
    pub(super) fn settled(&mut self, frontier: MeetingTime) -> bool {
        if frontier <= self.frontier {
            return false;
        }
        self.frontier = frontier;
        let before = self.shown.len();
        self.shown.retain(|w| w.start + COVERED_SLACK >= frontier);
        if self.shown.is_empty() {
            self.agreement.clear_history();
        }
        self.shown.len() != before
    }
}

/// The last stretch of a track's audio.
#[derive(Debug, Default)]
struct RecentAudio {
    samples: VecDeque<Sample>,
    /// Pipeline frame of the first sample kept.
    start: u64,
}

impl RecentAudio {
    fn end(&self) -> u64 {
        self.start + self.samples.len() as u64
    }

    /// Adds audio starting at frame `at`, keeping at most `keep` samples. A gap (packets dropped,
    /// or the first packet) restarts it there: a preview is only ever a guess at the newest audio.
    fn push(&mut self, at: u64, samples: &[Sample], keep: usize) {
        if self.samples.is_empty() || at != self.end() {
            self.samples.clear();
            self.start = at;
        }
        self.samples.extend(samples.iter().copied());
        let excess = self.samples.len().saturating_sub(keep);
        self.samples.drain(..excess);
        self.start += excess as u64;
    }

    /// The samples from frame `from` on, and where they start.
    fn since(&self, from: u64) -> (u64, Vec<Sample>) {
        let start = from.max(self.start);
        let skip = usize::try_from(start - self.start).unwrap_or(usize::MAX);
        (start, self.samples.iter().skip(skip).copied().collect())
    }
}

enum ToPreview {
    Audio(u64, Vec<Sample>),
    Voice { speaking: bool, at: u64 },
}

enum FromPreview {
    /// The words to show now.
    Words(Vec<Word>),
    /// The model is loaded: previews run from here on.
    Running,
    /// Previews stopped for the meeting, and why.
    Off(PreviewRefusal),
    Done,
}

/// What the preview thread reads from the session and the track's worker.
struct PreviewWork {
    previews: LivePreviews,
    backend: BackendInfo,
    /// The worker's decoded frontier (pipeline frames).
    frontier: Arc<AtomicU64>,
    /// The worker's queue (samples).
    queued: Arc<AtomicU64>,
    busy: u64,
    inbox: Receiver<ToPreview>,
    out: Sender<FromPreview>,
    stop: Arc<AtomicBool>,
    decode: Arc<Mutex<CancelToken>>,
    report: Arc<Mutex<PreviewReport>>,
}

impl PreviewWork {
    fn add(&self, f: impl FnOnce(&mut PreviewReport)) {
        f(&mut self.report.lock().unwrap_or_else(PoisonError::into_inner));
    }

    fn run(self) {
        if let Err(why) = self.decode_until_stopped() {
            let _ = self.out.send(FromPreview::Off(why));
        }
        let _ = self.out.send(FromPreview::Done);
    }

    fn decode_until_stopped(&self) -> Result<(), PreviewRefusal> {
        let failed = |text: String| PreviewRefusal::new(PreviewOffReason::Failed, text);
        if self.previews.background {
            crate::priority::lower_current_thread().map_err(failed)?;
        }
        if let Some(load) = &self.previews.stream {
            return self.stream_until_stopped(load);
        }
        let loading = Instant::now();
        let mut transcriber = (self.previews.load)(&self.backend)?;
        let ran = transcriber.backend_info().kind;
        self.add(|r| {
            r.load_s = loading.elapsed().as_secs_f64();
            r.backend = Some(ran);
        });
        let _ = self.out.send(FromPreview::Running);
        let keep = PIPELINE_RATE.frames_for(self.previews.window);
        let min = PIPELINE_RATE.frames_for(MIN_PREVIEW_AUDIO) as u64;
        let options = TranscribeOptions::live();
        let mut audio = RecentAudio::default();
        let mut words = PreviewWords::default();
        let mut speaking = false;
        let mut speech_until = 0u64;
        let mut due = Instant::now();
        let mut slow = 0u32;
        let wait = self.previews.interval.min(Duration::from_millis(20));
        while !self.stop.load(Ordering::Acquire) {
            match self.inbox.recv_timeout(wait) {
                Ok(message) => {
                    let mut next = Some(message);
                    while let Some(message) = next.take() {
                        match message {
                            ToPreview::Audio(at, samples) => audio.push(at, &samples, keep),
                            ToPreview::Voice { speaking: on, at } => {
                                speaking = on;
                                speech_until = speech_until.max(at);
                            }
                        }
                        next = self.inbox.try_recv().ok();
                    }
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return Ok(()),
            }
            let frontier = self.frontier.load(Ordering::SeqCst);
            let settled_to = MeetingTime::from_pipeline_frames(frontier);
            if words.settled(settled_to) {
                let _ = self.out.send(FromPreview::Words(words.shown().to_vec()));
            }
            if Instant::now() < due {
                continue;
            }
            // Unfinished speech: someone is speaking, or spoke after what the worker has decoded.
            if !speaking && speech_until <= frontier {
                continue;
            }
            let (start, samples) = audio.since(frontier);
            if (samples.len() as u64) < min {
                continue;
            }
            if self.queued.load(Ordering::SeqCst) > self.busy {
                self.add(|r| r.skipped_busy += 1);
                due = Instant::now() + self.previews.interval;
                continue;
            }
            let cancel = CancelToken::new();
            *self.decode.lock().unwrap_or_else(PoisonError::into_inner) = cancel.clone();
            let began = Instant::now();
            let buffer = MonoBuffer::new(PIPELINE_RATE, quantise_pcm16(&samples));
            let result = transcriber.transcribe(&buffer, &options, &cancel);
            let took = began.elapsed();
            due = Instant::now() + took.max(self.previews.interval);
            let transcription = match result {
                Ok(t) => t,
                Err(AsrError::Cancelled) => {
                    self.add(|r| r.cancelled += 1);
                    continue;
                }
                Err(e) => return Err(failed(e.to_string())),
            };
            self.add(|r| {
                r.decodes += 1;
                r.audio_s += PIPELINE_RATE.duration_of(samples.len()).as_secs_f64();
                r.decode_s += took.as_secs_f64();
                r.slowest_s = r.slowest_s.max(took.as_secs_f64());
            });
            slow = if took > self.previews.slow {
                slow + 1
            } else {
                0
            };
            if slow >= SLOW_PREVIEWS {
                slow = 0;
                self.add(|r| r.slow_pauses += 1);
                due = Instant::now() + self.previews.slow_pause;
            }
            let placeholder = EntityId::from_bytes([0; 16]);
            let heard: Vec<Word> = transcription
                .into_segments(
                    placeholder,
                    MeetingTime::from_pipeline_frames(start),
                    &mut || placeholder,
                )
                .into_iter()
                .flat_map(|s| s.words)
                .collect();
            // The worker may have moved on during the decode.
            let frontier = MeetingTime::from_pipeline_frames(self.frontier.load(Ordering::SeqCst));
            let added = words.hypothesis(heard, frontier);
            if added > 0 {
                self.add(|r| r.words_shown += added as u64);
                let _ = self.out.send(FromPreview::Words(words.shown().to_vec()));
            } else {
                self.add(|r| r.decodes_adding_nothing += 1);
            }
        }
        Ok(())
    }

    /// Evaluation only (T12.23, module docs): every packet of the track's audio goes to the
    /// streaming model once, and the words it finished are shown until settled text covers them
    /// ([`StreamFeed`]). Covered words leave on every turn of the loop, during a slow pause too.
    fn stream_until_stopped(&self, load: &StreamLoader) -> Result<(), PreviewRefusal> {
        let loading = Instant::now();
        let model = load(&self.backend)?;
        let ran = model.backend_info().kind;
        self.add(|r| {
            r.load_s = loading.elapsed().as_secs_f64();
            r.backend = Some(ran);
        });
        let _ = self.out.send(FromPreview::Running);
        let mut feed = StreamFeed::new(model);
        let mut slow = 0u32;
        let mut paused_until: Option<Instant> = None;
        let wait = Duration::from_millis(20);
        while !self.stop.load(Ordering::Acquire) {
            if paused_until.is_some_and(|until| Instant::now() >= until) {
                paused_until = None;
            }
            let paused = paused_until.is_some();
            match self.inbox.recv_timeout(wait) {
                Ok(message) => {
                    let mut next = Some(message);
                    while let Some(message) = next.take() {
                        // Paused: the audio of the pause is not heard.
                        if let ToPreview::Audio(at, samples) = message
                            && !paused
                        {
                            feed.audio(at, &samples);
                        }
                        next = self.inbox.try_recv().ok();
                    }
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return Ok(()),
            }
            if paused {
                // The model starts over after the pause (it keeps the words it finished).
                feed.pause();
            } else if let Some(audio) = feed.take_pending() {
                let audio = quantise_pcm16(&audio);
                let began = Instant::now();
                let steps = feed
                    .push(&audio)
                    .map_err(|e| PreviewRefusal::new(PreviewOffReason::Failed, e))?;
                let took = began.elapsed();
                if steps > 0 {
                    self.add(|r| {
                        r.decodes += steps;
                        r.decode_s += took.as_secs_f64();
                        r.slowest_s = r.slowest_s.max(took.as_secs_f64());
                    });
                    slow = if took > self.previews.slow {
                        slow + 1
                    } else {
                        0
                    };
                    if slow >= SLOW_PREVIEWS {
                        slow = 0;
                        self.add(|r| r.slow_pauses += 1);
                        paused_until = Some(Instant::now() + self.previews.slow_pause);
                    }
                }
                self.add(|r| r.audio_s += PIPELINE_RATE.duration_of(audio.len()).as_secs_f64());
            }
            let frontier = MeetingTime::from_pipeline_frames(self.frontier.load(Ordering::SeqCst));
            if let Some((words, added)) = feed.show(frontier) {
                if added > 0 {
                    self.add(|r| r.words_shown += added as u64);
                }
                let _ = self.out.send(FromPreview::Words(words));
            }
        }
        Ok(())
    }
}

/// Evaluation only (T12.23): what a track's streaming model has been given and what it shows.
/// Audio that does not follow the last packet (a gap: dropped packets) starts the model over;
/// the words it had finished are kept, in meeting time, and the unfinished last word and any
/// audio not yet pushed are dropped, so no word spans the gap.
pub(super) struct StreamFeed {
    model: Box<dyn StreamingPreview + Send>,
    /// Pipeline frame of the model's time zero (`None`: nothing heard since the last restart).
    origin: Option<u64>,
    /// Pipeline frame just after the last packet given.
    end: u64,
    pending: Vec<Sample>,
    /// Finished words of earlier runs of the model, in meeting time.
    carried: Vec<Word>,
    /// What was last shown.
    shown: Vec<Word>,
}

impl StreamFeed {
    pub(super) fn new(model: Box<dyn StreamingPreview + Send>) -> Self {
        Self {
            model,
            origin: None,
            end: 0,
            pending: Vec::new(),
            carried: Vec::new(),
            shown: Vec::new(),
        }
    }

    /// A packet of the track's audio at pipeline frame `at`.
    pub(super) fn audio(&mut self, at: u64, samples: &[Sample]) {
        if self.origin.is_some() && at != self.end {
            self.restart();
        }
        if self.origin.is_none() {
            self.origin = Some(at);
        }
        self.end = at + samples.len() as u64;
        self.pending.extend_from_slice(samples);
    }

    /// The audio given and not yet pushed, if any.
    pub(super) fn take_pending(&mut self) -> Option<Vec<Sample>> {
        (!self.pending.is_empty()).then(|| std::mem::take(&mut self.pending))
    }

    /// Pushes `audio` (what [`StreamFeed::take_pending`] returned) to the model.
    ///
    /// # Errors
    /// The model's.
    pub(super) fn push(&mut self, audio: &[Sample]) -> Result<u64, String> {
        self.model.push(audio)
    }

    /// Previews are paused: nothing pending is pushed and the model starts over (once).
    pub(super) fn pause(&mut self) {
        self.pending.clear();
        if self.origin.is_some() {
            self.restart();
        }
    }

    fn restart(&mut self) {
        if let Some(zero) = self.origin.take() {
            let zero = MeetingTime::from_pipeline_frames(zero);
            let finished = self.model.finished_words();
            self.carried
                .extend(finished.into_iter().map(|w| at_time(w, zero)));
        }
        self.model.restart();
        self.pending.clear();
    }

    /// The words to show now that settled text reaches `frontier`, and how many of them are new,
    /// when that differs from what was last shown.
    pub(super) fn show(&mut self, frontier: MeetingTime) -> Option<(Vec<Word>, usize)> {
        let covered = |w: &Word| w.start + COVERED_SLACK < frontier;
        self.carried.retain(|w| !covered(w));
        let mut now = self.carried.clone();
        if let Some(zero) = self.origin {
            let zero = MeetingTime::from_pipeline_frames(zero);
            let relative =
                MeetingTime::from_millis(frontier.as_millis().saturating_sub(zero.as_millis()));
            self.model
                .forget_before(relative.saturating_sub_duration(COVERED_SLACK));
            now.extend(
                self.model
                    .finished_words()
                    .into_iter()
                    .map(|w| at_time(w, zero))
                    .filter(|w| !covered(w)),
            );
        }
        if now == self.shown {
            return None;
        }
        let added = now
            .iter()
            .filter(|w| {
                !self
                    .shown
                    .iter()
                    .any(|s| s.start == w.start && s.text == w.text)
            })
            .count();
        self.shown.clone_from(&now);
        Some((now, added))
    }
}

/// `w`, whose times are from `zero`, in meeting time.
fn at_time(w: Word, zero: MeetingTime) -> Word {
    Word {
        start: w.start + zero,
        end: w.end + zero,
        ..w
    }
}

/// The session's end of a running preview thread.
pub(super) struct PreviewThread {
    to: SyncSender<ToPreview>,
    from: Receiver<FromPreview>,
    stop: Arc<AtomicBool>,
    decode: Arc<Mutex<CancelToken>>,
    report: Arc<Mutex<PreviewReport>>,
    queued: Arc<AtomicU64>,
    busy: u64,
    /// [`LivePreviews::fell_behind`], in frames.
    fell_behind: u64,
    /// The end of the settled text so far.
    settled: MeetingTime,
    /// The words last sent to the UI.
    sent: Vec<Word>,
    thread: Option<JoinHandle<()>>,
}

impl PreviewThread {
    fn spawn(
        previews: LivePreviews,
        track: TrackId,
        backend: BackendInfo,
        frontier: Arc<AtomicU64>,
        queued: Arc<AtomicU64>,
    ) -> std::io::Result<Self> {
        let (to, inbox) = std::sync::mpsc::sync_channel(AUDIO_QUEUE);
        let (out, from) = channel();
        let stop = Arc::new(AtomicBool::new(false));
        let decode = Arc::new(Mutex::new(CancelToken::new()));
        let report = Arc::new(Mutex::new(PreviewReport {
            track: Some(track),
            ..PreviewReport::default()
        }));
        let busy = PIPELINE_RATE.frames_for(REAL_DECODE_QUEUE) as u64;
        let fell_behind = PIPELINE_RATE.frames_for(previews.fell_behind) as u64;
        let work = PreviewWork {
            previews,
            backend,
            frontier,
            queued: Arc::clone(&queued),
            busy,
            inbox,
            out,
            stop: Arc::clone(&stop),
            decode: Arc::clone(&decode),
            report: Arc::clone(&report),
        };
        let thread = std::thread::Builder::new()
            .name("earshot-live-preview".into())
            .spawn(move || work.run())?;
        Ok(Self {
            to,
            from,
            stop,
            decode,
            report,
            queued,
            busy,
            fell_behind,
            settled: MeetingTime::ZERO,
            sent: Vec::new(),
            thread: Some(thread),
        })
    }

    /// Never waits: a thread that stopped taking audio loses it, and the session goes on.
    fn send(&self, message: ToPreview) {
        if let Err(TrySendError::Full(_)) = self.to.try_send(message) {
            self.report
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .audio_dropped += 1;
        }
    }

    /// A real decode has started: the preview in progress gives way.
    fn yield_to_real_decode(&self) {
        if self.queued.load(Ordering::SeqCst) > self.busy
            && let Ok(token) = self.decode.try_lock()
        {
            token.cancel();
        }
    }

    /// `words` without what settled text covers, when that differs from what was sent.
    fn changed_show(&mut self, mut words: Vec<Word>) -> Option<Vec<Word>> {
        let settled = self.settled;
        words.retain(|w| w.start + COVERED_SLACK >= settled);
        if words == self.sent {
            return None;
        }
        self.sent.clone_from(&words);
        Some(words)
    }

    fn finish(mut self, wait: Duration) -> PreviewReport {
        self.stop.store(true, Ordering::Release);
        self.decode
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .cancel();
        let until = Instant::now() + wait;
        // A decode in progress ends on its own: this never makes the end of a meeting wait longer.
        let done = loop {
            let left = until.saturating_duration_since(Instant::now());
            match self.from.recv_timeout(left) {
                Ok(FromPreview::Done) | Err(RecvTimeoutError::Disconnected) => break true,
                Ok(_) => {}
                Err(RecvTimeoutError::Timeout) => break false,
            }
        };
        let crashed = done
            && self
                .thread
                .take()
                .is_some_and(|thread| thread.join().is_err());
        let mut report = self
            .report
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        if crashed {
            report.off = Some(PREVIEW_THREAD_CRASHED.to_owned());
            report.reason = Some(PreviewOffReason::Failed);
        }
        report
    }
}

/// A track's previews: not started, running, or off and why.
#[derive(Default)]
pub(super) enum PreviewState {
    /// This track has none.
    #[default]
    None,
    Pending(LivePreviews),
    Running(Box<PreviewThread>),
    /// Stopped or never run; what it did, and why.
    Off(PreviewReport),
}

/// The tracks in [`slot`] order.
const TRACKS: [TrackId; 2] = [TrackId::Mic, TrackId::Reference];

impl LivePreviews {
    /// Each track's previews (by [`slot`]) in a session with or without a far end (module docs).
    pub(super) fn states(previews: Option<Self>, has_reference: bool) -> [PreviewState; 2] {
        let Some(previews) = previews else {
            return [PreviewState::None, PreviewState::None];
        };
        let own_voice = previews.own_voice.clone();
        let own_voice_rule = previews.own_voice_rule.clone();
        let first = Self {
            own_voice: None,
            own_voice_rule: None,
            ..previews
        };
        if has_reference {
            let mic = own_voice.map_or(PreviewState::None, |load| {
                PreviewState::Pending(Self {
                    load,
                    rule: own_voice_rule,
                    ..first.clone()
                })
            });
            [mic, PreviewState::Pending(first)]
        } else {
            [PreviewState::Pending(first), PreviewState::None]
        }
    }
}

impl<S> Session<S>
where
    S: AudioSource,
    S::Error: std::fmt::Debug,
{
    /// Hands the track's running preview thread its next audio.
    pub(super) fn preview_audio(&self, track: TrackId, start: u64, samples: &[Sample]) {
        if let PreviewState::Running(p) = &self.previews[slot(track)] {
            p.send(ToPreview::Audio(start, samples.to_vec()));
        }
    }

    /// A track's voice-activity change.
    pub(super) fn preview_voice(&self, track: TrackId, speaking: bool, at: MeetingTime) {
        if let PreviewState::Running(p) = &self.previews[slot(track)] {
            let at = at.to_pipeline_frames();
            p.send(ToPreview::Voice { speaking, at });
        }
    }

    /// Settled text of `track` now reaches `end`.
    pub(super) fn preview_settled(&mut self, track: TrackId, end: MeetingTime) {
        if let PreviewState::Running(p) = &mut self.previews[slot(track)] {
            p.settled = p.settled.max(end);
        }
    }

    /// Tells the host that `track`'s previews run (`None`) or why they do not, once per change
    /// (T12.33).
    fn say_previews(&mut self, track: TrackId, off: Option<PreviewOffReason>) {
        if self.preview_said[slot(track)] != Some(off) {
            self.preview_said[slot(track)] = Some(off);
            self.emit(RuntimeEvent::LivePreviewState { track, off });
        }
    }

    /// Stops `track`'s previews for the meeting; the other track's go on.
    fn previews_off(&mut self, track: TrackId, why: impl Into<PreviewRefusal>) {
        let why = why.into();
        let report = match std::mem::take(&mut self.previews[slot(track)]) {
            PreviewState::Running(p) => {
                let shown = !p.sent.is_empty();
                let report = p.finish(Duration::ZERO);
                if shown {
                    self.emit(RuntimeEvent::LivePreview {
                        track,
                        words: Vec::new(),
                    });
                }
                report
            }
            PreviewState::Off(report) => report,
            PreviewState::None | PreviewState::Pending(_) => PreviewReport {
                track: Some(track),
                ..PreviewReport::default()
            },
        };
        self.previews[slot(track)] = PreviewState::Off(PreviewReport {
            off: Some(why.text),
            reason: Some(why.reason),
            ..report
        });
        self.say_previews(track, Some(why.reason));
    }

    /// Between two pumps, never waiting: starts each track's previews once nothing holds them
    /// back, gives way to a real decode, passes on what the threads found, and stops a track's
    /// previews when they should stop.
    pub(super) fn check_previews(&mut self) {
        let mut active = [false; 2];
        for track in TRACKS {
            match &self.previews[slot(track)] {
                PreviewState::None | PreviewState::Off(_) => continue,
                PreviewState::Pending(_) => self.start_previews(track),
                PreviewState::Running(_) => {}
            }
            active[slot(track)] = true;
        }
        if active == [false; 2] {
            return;
        }
        if matches!(self.transcript_state, Some(LiveTranscriptState::Stopped)) {
            for track in TRACKS {
                if active[slot(track)] {
                    self.previews_off(
                        track,
                        PreviewRefusal::new(
                            PreviewOffReason::TranscriptStopped,
                            "the live transcript stopped",
                        ),
                    );
                }
            }
            return;
        }
        for track in TRACKS {
            if let PreviewState::Running(p) = &self.previews[slot(track)] {
                let limit = p.fell_behind;
                let behind = self
                    .workers
                    .iter()
                    .find(|w| w.track() == track)
                    .is_some_and(|w| w.backlog() > limit);
                if behind {
                    self.previews_off(
                        track,
                        PreviewRefusal::new(PreviewOffReason::FellBehind, FELL_BEHIND_REASON),
                    );
                }
            }
        }
        self.check_preview_power();
        let mut messages: [Vec<FromPreview>; 2] = [Vec::new(), Vec::new()];
        let mut crashed = [false; 2];
        for track in TRACKS {
            if let PreviewState::Running(p) = &mut self.previews[slot(track)] {
                p.yield_to_real_decode();
                // Read before the messages: a thread that ended has sent all it will.
                let ended = p.thread.as_ref().is_some_and(JoinHandle::is_finished);
                messages[slot(track)] = p.from.try_iter().collect();
                // A thread that ended is joined now, which does not wait: only a panic is a crash,
                // and the user hears it now rather than never (the report at the end says the
                // same). One that returned said why itself, in the messages above.
                if ended && let Some(thread) = p.thread.take() {
                    crashed[slot(track)] = thread.join().is_err();
                }
            }
        }
        // A thread trims words after the worker's frontier moved, and a worker sends a decode's
        // finals before it publishes the frontier: so the finals behind these messages are queued
        // now, and go out first. Settled text always lands before the words it replaces leave.
        if messages.iter().any(|m| !m.is_empty()) {
            self.handle_workers();
        }
        for track in TRACKS {
            let messages = std::mem::take(&mut messages[slot(track)]);
            let PreviewState::Running(p) = &mut self.previews[slot(track)] else {
                continue;
            };
            let mut shows = Vec::new();
            let mut off = None;
            let mut running = false;
            for message in messages {
                match message {
                    FromPreview::Running => running = true,
                    FromPreview::Words(words) => {
                        if let Some(words) = p.changed_show(words) {
                            shows.push(words);
                        }
                    }
                    FromPreview::Off(why) => off = Some(why),
                    FromPreview::Done => {}
                }
            }
            if running {
                self.say_previews(track, None);
            }
            for words in shows {
                self.emit(RuntimeEvent::LivePreview { track, words });
            }
            if crashed[slot(track)] {
                off = Some(PreviewRefusal::new(
                    PreviewOffReason::Failed,
                    PREVIEW_THREAD_CRASHED,
                ));
            }
            if let Some(why) = off {
                self.previews_off(track, why);
            }
        }
    }

    fn check_preview_power(&mut self) {
        let now = Instant::now();
        if self.next_preview_power.is_some_and(|due| now < due) {
            return;
        }
        self.next_preview_power = Some(now + self.policy.power_poll);
        let running: Vec<TrackId> = TRACKS
            .into_iter()
            .filter(|t| matches!(self.previews[slot(*t)], PreviewState::Running(_)))
            .collect();
        if !running.is_empty() && self.policy.power.supply() == PowerSupply::Battery {
            for track in running {
                self.previews_off(track, on_battery());
            }
        }
    }

    /// What the session's rule ([`LivePreviews::rule`]) refuses for `track`'s waiting previews
    /// on `backend`, if it was given one; under the automatic choice as
    /// [`PreviewOffReason::OffByDefault`] ([`LivePreviews::user_asked`]). Asked at most once per
    /// track: a refusal turns them off, and an answer that allows them is kept.
    fn preview_rule_refusal(
        &mut self,
        track: TrackId,
        backend: &BackendInfo,
    ) -> Option<PreviewRefusal> {
        if self.preview_rule_allowed[slot(track)] {
            return None;
        }
        let PreviewState::Pending(previews) = &self.previews[slot(track)] else {
            return None;
        };
        let Some(why) = previews.rule.as_ref().and_then(|rule| rule(backend)) else {
            self.preview_rule_allowed[slot(track)] = true;
            return None;
        };
        Some(if previews.user_asked {
            why
        } else {
            PreviewRefusal {
                reason: PreviewOffReason::OffByDefault,
                ..why
            }
        })
    }

    /// The power supply for previews waiting for a hold: read at most every
    /// [`super::LiveConfig::power_poll`], as running previews read it, never on every loop.
    fn waiting_supply(&mut self) -> PowerSupply {
        let now = Instant::now();
        if let Some((supply, due)) = self.waiting_power
            && now < due
        {
            return supply;
        }
        let supply = self.policy.power.supply();
        self.waiting_power = Some((supply, now + self.policy.power_poll));
        supply
    }

    /// Starts `track`'s thread when nothing holds it back any more (module docs).
    fn start_previews(&mut self, track: TrackId) {
        // Holding, or catching up after a hold: the transcript is not live yet.
        if self.hold.as_ref().is_some_and(|h| h.transcript_held)
            || matches!(
                self.transcript_state,
                Some(LiveTranscriptState::Held(_) | LiveTranscriptState::CatchingUp)
            )
        {
            // The wait is said only where previews would start after it: once the track's
            // backend is known and where the computer's rule runs them on it. On battery the
            // battery is said instead, as soon as it is read: the start after the hold reads the
            // supply again and decides.
            let backend = self
                .stretch_records
                .iter()
                .find(|(t, _)| *t == track)
                .map(|(_, record)| record.backend.clone());
            if let Some(backend) = backend
                && matches!(self.policy.transcription, LiveTranscription::Stretches(_))
            {
                if let Some(why) = self.preview_rule_refusal(track, &backend) {
                    self.previews_off(track, why);
                } else if self.waiting_supply() == PowerSupply::Battery {
                    self.say_previews(track, Some(PreviewOffReason::OnBattery));
                } else {
                    self.say_previews(track, Some(PreviewOffReason::CatchingUp));
                }
            }
            return;
        }
        let not_applicable =
            |text: &str| PreviewRefusal::new(PreviewOffReason::NotApplicable, text);
        if !matches!(self.policy.transcription, LiveTranscription::Stretches(_)) {
            self.previews_off(track, not_applicable("the live chunker has partials"));
            return;
        }
        let Some(worker) = self.workers.iter().find(|w| w.track() == track) else {
            self.previews_off(track, not_applicable("no live transcript on the track"));
            return;
        };
        let Some((_, record)) = self.stretch_records.iter().find(|(t, _)| *t == track) else {
            self.previews_off(track, not_applicable("no live stretches on the track"));
            return;
        };
        let backend = record.backend.clone();
        let (frontier, queued) = (worker.frontier_source(), worker.queue_source());
        // First what this computer would refuse anyway: the battery is not why then.
        if let Some(why) = self.preview_rule_refusal(track, &backend) {
            self.previews_off(track, why);
            return;
        }
        if self.policy.power.supply() == PowerSupply::Battery {
            self.previews_off(track, on_battery());
            return;
        }
        let speaking = self.speaking[slot(track)];
        let PreviewState::Pending(previews) = std::mem::take(&mut self.previews[slot(track)])
        else {
            return;
        };
        match PreviewThread::spawn(previews, track, backend, frontier, queued) {
            Ok(thread) => {
                if speaking {
                    thread.send(ToPreview::Voice {
                        speaking,
                        at: self.sent[slot(track)],
                    });
                }
                self.previews[slot(track)] = PreviewState::Running(Box::new(thread));
            }
            Err(e) => self.previews_off(
                track,
                PreviewRefusal::new(PreviewOffReason::Failed, e.to_string()),
            ),
        }
    }

    /// The recording is over: no more decodes. What is shown stays until the real decodes land.
    pub(super) fn stop_previews(&self) {
        for state in &self.previews {
            if let PreviewState::Running(p) = state {
                p.stop.store(true, Ordering::Release);
            }
        }
    }

    /// The live transcript is complete: nothing provisional is left, and each track's report
    /// goes out.
    pub(super) fn finish_previews(&mut self) {
        for track in TRACKS {
            let report = match std::mem::take(&mut self.previews[slot(track)]) {
                PreviewState::None => continue,
                PreviewState::Pending(_) => PreviewReport {
                    track: Some(track),
                    off: Some("the meeting ended before previews started".to_owned()),
                    reason: Some(PreviewOffReason::MeetingEnded),
                    ..PreviewReport::default()
                },
                PreviewState::Off(report) => report,
                PreviewState::Running(p) => {
                    let shown = !p.sent.is_empty();
                    let report = p.finish(self.policy.speaker_stop_wait);
                    if shown {
                        self.emit(RuntimeEvent::LivePreview {
                            track,
                            words: Vec::new(),
                        });
                    }
                    report
                }
            };
            self.emit(RuntimeEvent::LivePreviewReport(report));
        }
    }
}
