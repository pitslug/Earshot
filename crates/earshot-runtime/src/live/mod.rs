//! A live meeting session: capture -> core pipeline -> FLAC + live transcript, on one
//! session thread (PLAN.md 3.3, T6.4a).
//!
//! [`start_live`] validates the pipeline, builds live ASR from a [`LiveAsrFactory`],
//! opens one FLAC writer per track, and only then commits the `Meeting` and `Track` rows
//! (before capture, so segments reference real tracks) and starts the session thread.
//! A failure before that point leaves no rows behind. The returned [`LiveSession`]
//! carries the [`RuntimeEvent`] stream for the UI and the controls
//! ([`LiveSession::stop`], [`LiveSession::wait`]); dropping it stops the session too
//! (the recording is still finalised).
//!
//! # Session thread
//!
//! The thread first sends [`RuntimeEvent::Started`]. Its loop pumps the source into the
//! pipeline (never blocking), then polls the pipeline, and sleeps
//! [`LiveConfig::idle_wait`] when the source has nothing. Live decodes never run here:
//! each transcribed track has a worker thread (`worker::LiveWorker`, T6.10a) that the
//! session feeds with the track's `Audio` and whose transcript events it handles like the
//! pipeline's, so a slow decode never delays capture or the recording. By default each
//! worker decodes speech stretches, each once as soon as it is settled, with the
//! whole-track pass's result ([`LiveTranscription`], T6.13b); the live chunker (partials
//! and finals) stays available. The workers are finished (rest decoded, open segments
//! closed; a stretch worker always finishes its queue) before the pipeline's final-pass
//! request and `Ended` are handled. A stretch worker's pieces, exactly as decoded (before
//! the echo filter), and how far they reach are recorded device-locally at the end
//! ([`crate::stretches`], T6.13c), so the final pass transcribes only what they do not
//! cover; the final-pass request asks for the stretches' own decoding for those tracks.
//! A track whose [`TrackAsr`] carries [`LiveSpeakers`] also has its speaker windows
//! computed during the meeting (T6.13d2), on the session's one speaker thread at
//! below-normal priority, never on the transcription workers (T6.13d3, `speaker_thread`
//! docs); the session sends it the track's `Audio` too, stops it before finishing the
//! workers and waits at most [`LiveConfig::speaker_stop_wait`] for it. The windows are
//! recorded at the end ([`crate::speakers`]) so the final pass only finishes the tail and
//! clusters. The speaker detection setting ([`LiveConfig::speaker_timing`], T6.13d4) can
//! keep speaker work off from the start (no speaker thread, no model loaded; each track gets
//! a reason-only record) or stop it on battery power (the session thread reads the supply
//! every [`LiveConfig::power_poll`]; the record keeps the windows finished before and the
//! reason); [`RuntimeEvent::LiveSpeakersOff`] reports either.
//! A stop request calls [`AudioSource::stop`] once; the source
//! then delivers what it still has and its end, the pipeline flushes, and the thread
//! finalises the recording. [`RuntimeEvent::Ended`] is always the last event.
//!
//! Event handling:
//! - `Audio`: appended to the track's FLAC writer (`mic.flac` cleaned, `loopback.flac`)
//!   and sent to the track's live worker.
//! - `Transcript`: forwarded; a `Final` is stored as a [`TranscriptPass::Live`] segment.
//! - `Levels`, `Stats`, `Discontinuity`: forwarded; the last stats snapshot of each poll
//!   drives the clock policy below.
//! - `Error`: forwarded as [`RuntimeWarning::Pipeline`] (timestamp jumps, live ASR stops).
//! - `FinalPassRequested` / `Ended`: kept for the [`LiveOutcome`].
//!
//! # Runtime policies
//!
//! - **Clock re-sync.** When the latest stats snapshot shows the mic skew beyond
//!   [`LiveConfig::mic_skew_budget`], or the reference-minus-mic skew beyond
//!   [`LiveConfig::track_offset_budget`] (which keeps the echo path inside the
//!   canceller's 600 ms delay search even when drift steering saturates), the session
//!   sends [`DiscontinuityKind::DataLost`] to the mic and then the reference, back to
//!   back before the next pump (pipeline module docs, Clock drift), and reports the
//!   routine event [`RuntimeEvent::Resynced`]. Re-syncs are at least
//!   [`LiveConfig::resync_cooldown`] apart.
//! - **Device loss.** A `DeviceChanged` marker without a device name reports
//!   [`RuntimeWarning::DeviceUnavailable`]; the track stays unavailable (silence is
//!   recorded) until a marker with a name reports [`RuntimeWarning::DeviceRestored`].
//!   Tracks still unavailable at the end are listed in [`LiveOutcome::unavailable_at_end`].
//!   A marker carrying [`EndpointWarning::BluetoothHandsFree`] (a switch onto a
//!   hands-free mic) also reports [`RuntimeWarning::BluetoothHandsFreeMic`], right before
//!   the marker's `Discontinuity` event.
//! - **Capture errors** from the source are reported as [`RuntimeWarning::Capture`]
//!   and pumping continues (a WASAPI source ends itself after a fatal error). Repeats of
//!   the same message are counted, not re-sent; the count is reported when another
//!   message arrives or the session ends.
//! - **Persistence errors** during the meeting (a FLAC write, a live segment) are
//!   reported and do not stop capture. A failed FLAC write never deletes audio (T9.17): that
//!   track stops recording and its file is finished at once with the audio before the error
//!   ([`RuntimeWarning::RecordingStopped`]); the other track goes on, and when every track has
//!   stopped the session stops the meeting itself ([`RuntimeWarning::EveryRecordingStopped`]).
//!   Tracks of different lengths are fine: the final pass reads each file as it is.
//!
//! # Finalising
//!
//! At the end every remaining FLAC writer is finalised into its `AudioFile` row, each
//! independently (a failure is reported in [`LiveOutcome::problems`] and the other
//! tracks still finish); then the meeting's duration is set. The meeting row is re-read
//! under the store lock and only `duration_ms` changes: edits made during the meeting
//! (a rename, tags, a sync merge) are kept. A synced row is never written back from a
//! copy held across the session.
//!
//! The session never exports to Obsidian (on demand only, decided by the owner) and never
//! touches the network.
//!
//! # Holding the live work
//!
//! [`LiveConfig::hold`] inverts the start order (T9.1l, `hold` module docs): the source, the rows
//! and the FLAC writers come first and live transcription and speaker work start later, off the
//! session thread, so a slow or failed model load never costs the recording. Every recording
//! finishes the same way: when capture ends, the FLAC files and the duration are saved first
//! ([`RuntimeEvent::RecordingSaved`]) and the live workers are finished after that, so a stop (or
//! the app quitting) never waits for a backlog to decode.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use earshot_core::asr::{
    AsrError, BackendInfo, BackendKind, CancelToken, MeetingTime, TranscriptEvent,
};
use earshot_core::capture::{
    AudioSource, CaptureSession, CaptureSink, Discontinuity, DiscontinuityKind, EndOfStream,
    EndReason, EndpointWarning, SourceStatus, TrackId,
};
use earshot_core::diar::{Diarizer, SpeakerEmbedder, SpeakerSegmenter};
use earshot_core::identity::EntityId;
use earshot_core::model::{
    ClockDomain, Meeting, MeetingRepo, SegmentRow, Stamp, Track, TranscriptPass,
};
use earshot_core::pipeline::{
    FinalPassRequest, Levels, LiveAsr, LiveTrack, Pipeline, PipelineConfig, PipelineEnd,
    PipelineError, PipelineEvent, PipelineStats,
};
use earshot_core::prompt::{
    MeetingInfo, PromptSegment, SummaryInput, Template, Vocabulary, time_chunks,
};
use earshot_store::{AudioProvenance, FlacTrackWriter};

use crate::live_summaries::{LiveSummaries, LiveSummaryRecord};
use crate::power::{PowerProbe, PowerSupply};
use crate::speaker_timing::{SpeakerTiming, UNPLUGGED_REASON};
use crate::speakers::{SpeakerHeader, SpeakerRecord};
use crate::stretches::StretchRecord;
use crate::{BoxRng, RuntimeError, Services};

pub mod echo;
mod gaps;
pub use gaps::REPORTED_GAP;
mod hold;
mod preview;
pub(crate) mod speaker_thread;
mod summary_thread;
#[cfg(test)]
pub(crate) mod tests;
mod voice;
mod worker;

use echo::{LiveEchoFilter, Released};
pub use hold::{
    CAUGHT_UP_BACKLOG, HOLD_LOAD_STALL, HeldSpeakerFactory, HoldGates, HoldNotes, HoldReason,
    LiveHold, LiveTranscriptState, SPEAKER_HANDOVER_MARGIN,
};
use hold::{HoldState, held_entry};
use preview::PreviewState;
pub use preview::{
    LivePreviews, PreviewLoader, PreviewOffReason, PreviewRefusal, PreviewReport, PreviewRule,
    StreamLoader, StreamingPreview,
};
pub use speaker_thread::SPEAKER_MAX_BUFFER;
use speaker_thread::{SpeakerThread, SpeakerTrack};
use summary_thread::{Job, SummaryThread};
use voice::{FarEndChange, FarEndWatch, VoiceTracker};
pub use voice::{VoiceActivityConfig, transcript_lag};
use worker::LiveWorker;

/// Version recorded with every recorded track ([`AudioProvenance::pipeline_version`]).
pub const PIPELINE_VERSION: &str = concat!("earshot-pipeline ", env!("CARGO_PKG_VERSION"));

/// The pipeline's timestamp-gap limit in live sessions (see [`LiveConfig::pipeline`]):
/// ten times the WASAPI channel cap (30 s).
pub const LIVE_MAX_GAP: Duration = Duration::from_secs(300);

/// Version recorded for the cleaned mic when a reference was cancelled.
pub const AEC_VERSION: &str = concat!("earshot-aec ", env!("CARGO_PKG_VERSION"));

/// Builds live ASR for a track, given the id of its `Track` row; `Ok(None)` leaves the
/// track without live transcription. Called once per track before capture starts.
pub type LiveAsrFactory =
    Box<dyn FnMut(TrackId, EntityId) -> Result<Option<TrackAsr>, RuntimeError> + Send>;

/// Better backends refused for a model, best first, each with the reason.
pub type Fallbacks = Vec<(BackendKind, AsrError)>;

/// Live ASR for one track, with the better backends that were refused for it (each with
/// the reason), for [`RuntimeEvent::LiveBackend`].
#[derive(Debug)]
pub struct TrackAsr {
    /// The pipeline's live ASR.
    pub asr: LiveAsr,
    /// Better backends tried first and refused (best first).
    pub fallbacks: Vec<(BackendKind, AsrError)>,
    /// Speaker models for the track's speaker windows during the meeting (T6.13d2);
    /// `None`: the final pass diarises the track whole.
    pub speakers: Option<LiveSpeakers>,
    /// How the transcription backend was proven, for reports: ONNX Runtime's node
    /// placement for a Parakeet model (`backends::AnyTranscriber::onnx_placement`); `None`
    /// for whisper, whose evidence is whisper.cpp's log, re-checked on every call.
    pub evidence: Option<String>,
}

impl From<LiveAsr> for TrackAsr {
    fn from(asr: LiveAsr) -> Self {
        Self {
            asr,
            fallbacks: Vec::new(),
            speakers: None,
            evidence: None,
        }
    }
}

/// The most pipeline frames a live worker of `cfg` may queue before it stops ("fell
/// behind"): `max_asr_backlog`, raised to [`STRETCH_MAX_BACKLOG`] for speech stretches.
#[must_use]
pub fn live_asr_backlog(cfg: &LiveConfig) -> u64 {
    let backlog = match cfg.transcription {
        LiveTranscription::Stretches(_) => cfg.pipeline.max_asr_backlog.max(STRETCH_MAX_BACKLOG),
        LiveTranscription::Chunker => cfg.pipeline.max_asr_backlog,
    };
    earshot_core::audio::PIPELINE_RATE.frames_for(backlog) as u64
}

/// The speaker models a live worker runs ([`LiveSpeakers`]).
pub type LiveDiarizer = Diarizer<Box<dyn SpeakerSegmenter + Send>, Box<dyn SpeakerEmbedder + Send>>;

/// Loads a track's speaker models ([`LiveSpeakers::models`]).
pub type SpeakerLoader = Box<dyn FnOnce() -> Result<LiveDiarizer, RuntimeError> + Send>;

/// Speaker work for a track during the meeting (T6.13d2; on the session's speaker thread,
/// T6.13d3): the diarizer whose windows (segmentation and embeddings, [`Diarizer::advance`])
/// are computed while the meeting runs, with the same window settings as the final pass's
/// diarizer, and the models' ONNX Runtime thread count (recorded; the final pass reuses the
/// windows only with the same count, [`crate::speakers`]).
pub struct LiveSpeakers {
    /// Loads the models and settings. [`start_live`] calls it before capture starts, and
    /// only when speaker work runs during the meeting ([`LiveConfig::speaker_timing`],
    /// T6.13d4): with speaker work off from the start no model is loaded.
    pub models: SpeakerLoader,
    /// ONNX Runtime intra-op threads of the models (`None` for models without one).
    pub threads: Option<usize>,
    /// If set, the number of windows computed so far, updated after each finished batch
    /// (an interrupted batch does not count): for status display and tests. Never written
    /// after the session's `Ended`.
    pub progress: Option<Arc<std::sync::atomic::AtomicUsize>>,
    /// Whether the speaker thread runs at below-normal OS priority (T6.13d3; always true
    /// from [`crate::backends::with_live_speakers`], whose ONNX sessions are background
    /// ones too). The session's one speaker thread is lowered if any track asks for it.
    /// Tests of what speaker work computes set false, so that a host whose cores are all
    /// busy with normal-priority work (a loaded test run) still gives the thread time.
    pub background: bool,
}

impl std::fmt::Debug for LiveSpeakers {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LiveSpeakers")
            .field("threads", &self.threads)
            .field("background", &self.background)
            .finish()
    }
}

/// Live session settings.
#[derive(Debug, Clone, PartialEq)]
pub struct LiveConfig {
    /// Meeting title (may be empty; the summary can fill it later).
    pub title: String,
    /// Summary template.
    pub template: Template,
    /// The user's UTC offset at the start, for display.
    pub utc_offset_minutes: i32,
    /// Clock domain of the capture timestamps (QPC for WASAPI, replay for a file).
    pub clock_domain: ClockDomain,
    /// Core pipeline settings. The default raises `max_gap` to
    /// [`LIVE_MAX_GAP`]: a consumer stall makes the WASAPI channel drop frames (30 s
    /// queue cap) and then resume with a `DataLost` marker and a timestamp gap as long as
    /// the stall, which the pipeline must fill as silence rather than report as a
    /// `TimestampJump` (that would keep the track on sample time and lose the wall-clock
    /// position). Stalls beyond it are still reported.
    pub pipeline: PipelineConfig,
    /// Sleep when the source has nothing (default 10 ms, one WASAPI period).
    pub idle_wait: Duration,
    /// Re-sync when the mic skew exceeds this (default 100 ms: transcript times drift
    /// no further than that from the device clock).
    pub mic_skew_budget: Duration,
    /// Re-sync when reference and mic skews differ by more than this (default 100 ms).
    pub track_offset_budget: Duration,
    /// Minimum meeting time between re-syncs (default 5 s).
    pub resync_cooldown: Duration,
    /// At the end, a track whose live transcription is further behind than this stops
    /// decoding instead of making the end wait (default 30 s; T6.10b): the cleanup pass
    /// transcribes the whole recording anyway. Applies to the live chunker only: speech
    /// stretches always finish their queue (they are the transcript, D20).
    pub max_stop_backlog: Duration,
    /// How the meeting is transcribed while it runs (default: speech stretches, T6.13b).
    pub transcription: LiveTranscription,
    /// At the end, how long the session waits for live speaker work to stop after the
    /// transcription workers finished (default 2 s; T6.13d3). Stopping interrupts the
    /// batch in progress, which ends within one model call; a thread still running after
    /// this is left to end on its own and its tracks are recorded with the batches
    /// finished before (what an interrupted batch leaves too), so speaker work never
    /// delays the end of a meeting by more than this. A thread left like that writes
    /// nothing after `Ended` and holds its models and audio only until its model call
    /// returns (`speaker_thread` docs).
    pub speaker_stop_wait: Duration,
    /// When speakers are detected (T6.13d4, [`crate::speaker_timing`]; default during the
    /// call). Applies to tracks whose [`TrackAsr`] carries [`LiveSpeakers`].
    pub speaker_timing: SpeakerTiming,
    /// The power supply, read for [`SpeakerTiming::AfterCallOnBattery`] only (default: the
    /// OS's, [`crate::power`]).
    pub power: PowerProbe,
    /// With [`SpeakerTiming::AfterCallOnBattery`] on a machine with a battery, how often
    /// the session thread reads the power supply while speaker work runs (default 5 s).
    /// The read is a non-blocking OS call (microseconds), so its cost is negligible; the
    /// interval bounds how much speaker work runs on battery after an unplug.
    pub power_poll: Duration,
    /// The voice-activity indicator of each track ([`RuntimeEvent::VoiceActivity`],
    /// `voice` module docs).
    pub voice_activity: VoiceActivityConfig,
    /// Summarise each finished window while the meeting runs (T8.3j,
    /// [`crate::live_summaries`]; default `None`: off). Needs live transcription; off on
    /// battery with [`SpeakerTiming::AfterCallOnBattery`], as speaker work is. At the end the
    /// session waits at most [`LiveConfig::speaker_stop_wait`] for the window in progress
    /// to stop.
    pub live_summaries: Option<LiveSummaries>,
    /// Start live transcription and speaker work after capture, when the hold's gates open
    /// (`hold` module docs; default `None`: both load before capture starts).
    pub hold: Option<LiveHold>,
    /// Show provisional words of the unfinished stretch (`preview` module docs; default `None`:
    /// words appear when their stretch lands).
    pub previews: Option<LivePreviews>,
    /// Keep the meeting's summary on this computer (T12.13, [`crate::keep_local`]; default
    /// off): the record is written in the transaction that creates the meeting's rows, so the
    /// meeting never exists without it.
    pub keep_summary_local: bool,
    /// At the end, a track's live catch-up with no sign of life for this long (its decoded
    /// frontier, its decode's cancel-token polls and its messages all unchanged) is given up
    /// (T9.22, `worker::StallWatch`): the decode is asked to stop, its thread is left, never
    /// joined, and [`RuntimeWarning::LiveCatchUpStalled`] says so. Default 2 minutes, a judgement.
    /// The rule it rests on: no single call between two polls of the decode's token may take this
    /// long. Whisper polls inside its graph compute (the abort callback), Parakeet only once per
    /// piece and per chunk (an ONNX encoder run is one call, seconds at most), the piece loop once
    /// per piece and per VAD frame. A backend or a piece length that breaks that rule must poll more
    /// often or raise this bound, never let a slow decode be cut.
    pub catch_up_stall: Duration,
}

/// How a live session transcribes.
#[derive(Debug, Clone, PartialEq)]
pub enum LiveTranscription {
    /// Each speech stretch once, as soon as it is settled, with the result of the
    /// whole-track pass (`asr::PieceStream`, D20): finals only, no partials. The queue may
    /// grow to [`STRETCH_MAX_BACKLOG`] and is always finished at the end.
    Stretches(earshot_core::asr::PieceConfig),
    /// The streaming chunker: partials and finals per utterance, a draft for the cleanup
    /// pass to replace (the T6.10 behaviour, kept as an option).
    Chunker,
}

impl Default for LiveTranscription {
    fn default() -> Self {
        Self::Stretches(earshot_core::asr::PieceConfig::default())
    }
}

/// How far behind a speech-stretch worker may fall before it stops (D20: nothing is
/// deferred later than 30 minutes).
pub const STRETCH_MAX_BACKLOG: Duration = Duration::from_secs(30 * 60);

impl Default for LiveConfig {
    fn default() -> Self {
        Self {
            title: String::new(),
            template: Template::default(),
            utc_offset_minutes: 0,
            clock_domain: ClockDomain::Qpc,
            pipeline: PipelineConfig {
                max_gap: LIVE_MAX_GAP,
                ..PipelineConfig::default()
            },
            idle_wait: Duration::from_millis(10),
            mic_skew_budget: Duration::from_millis(100),
            track_offset_budget: Duration::from_millis(100),
            resync_cooldown: Duration::from_secs(5),
            max_stop_backlog: Duration::from_secs(30),
            transcription: LiveTranscription::default(),
            speaker_stop_wait: Duration::from_secs(2),
            speaker_timing: SpeakerTiming::default(),
            power: PowerProbe::default(),
            power_poll: Duration::from_secs(5),
            live_summaries: None,
            voice_activity: VoiceActivityConfig::default(),
            hold: None,
            previews: None,
            keep_summary_local: false,
            catch_up_stall: Duration::from_secs(120),
        }
    }
}

/// What a live session reports to the UI, in order. `Started` is first and `Ended`
/// last; the channel then closes.
#[derive(Debug, Clone, PartialEq)]
pub enum RuntimeEvent {
    /// Rows exist and the session thread is running (with [`LiveConfig::hold`], before any model
    /// has loaded).
    Started {
        /// The meeting.
        meeting: EntityId,
        /// Its tracks (mic first).
        tracks: Vec<(TrackId, EntityId)>,
    },
    /// The backend that runs a track's live transcription (reports must say which
    /// backend ran), sent right after `Started`, or when a held session's live transcription
    /// starts; it is also stored device-locally ([`crate::backends::pass_backend`]).
    LiveBackend {
        /// Track.
        track: TrackId,
        /// What actually runs (verified by the loader).
        backend: BackendInfo,
        /// Better backends refused, with the reason.
        fallbacks: Vec<(BackendKind, AsrError)>,
    },
    /// Level meters.
    Levels(Levels),
    /// A track's voice activity changed ("You" on the cleaned mic, "Them" on the
    /// reference): an energy VAD over the audio as it is pumped, with onset and release
    /// ([`LiveConfig::voice_activity`], `voice` module docs), never the transcription
    /// workers' VAD (which lags with the decodes). Sent only on a change; every track
    /// starts silent, and a track still speaking at the end is switched off before
    /// `Ended`.
    VoiceActivity {
        /// Track.
        track: TrackId,
        /// Whether someone is speaking on it now.
        speaking: bool,
        /// When the change happened in the audio: the start of the first speech frame for
        /// an "on", the end of the last one for an "off" (sent after the release, so about
        /// [`VoiceActivityConfig::release`] later).
        at: MeetingTime,
    },
    /// How far a transcribed track's transcript is behind its audio: the audio sent to
    /// its worker minus what is transcribed ([`transcript_lag`], `voice` module docs).
    /// Sent for each track with live transcription after each session poll that carried a
    /// [`RuntimeEvent::Stats`], so at most once per `PipelineConfig::stats_interval` of
    /// meeting time (1 s by default).
    TranscriptLag {
        /// Track.
        track: TrackId,
        /// Audio not yet transcribed.
        behind: Duration,
    },
    /// Periodic diagnostics (AEC state, clock skews, intake counters, ASR backlog).
    Stats(PipelineStats),
    /// A live transcript event ("Me" on the mic, "Them" on the reference).
    ///
    /// Mic finals pass the echo filter ([`echo`]) before they are sent: a mic final may
    /// therefore arrive after partials of later mic segments, and a mic segment whose
    /// partials showed words may end in `Discarded` (all its words were echo). Mic partials
    /// are shown without words that already repeat the far end.
    Transcript {
        /// Track.
        track: TrackId,
        /// Partial, final or discarded segment.
        event: TranscriptEvent,
    },
    /// A capture discontinuity (device change, lost data, or a runtime re-sync).
    Discontinuity {
        /// Track.
        track: TrackId,
        /// When.
        at: MeetingTime,
        /// What.
        kind: DiscontinuityKind,
    },
    /// Both tracks were re-synchronised with the device clock (routine; informational).
    Resynced {
        /// Mic skew that triggered it (ms).
        mic_skew_ms: f64,
        /// Reference-minus-mic skew (ms), with a reference.
        track_offset_ms: Option<f64>,
    },
    /// A track's live chunker fell behind and decodes finals only (partial decodes off),
    /// or caught up and shows partials again (routine; T6.10b). Speech stretches have no
    /// partials and never send it.
    LivePartials {
        /// Track.
        track: TrackId,
        /// Whether partials are shown again.
        enabled: bool,
        /// How far behind the track was.
        behind: Duration,
    },
    /// Live speaker work does not run (from the start) or stopped for the rest of the
    /// meeting because of the speaker detection setting (T6.13d4,
    /// [`LiveConfig::speaker_timing`]); the cleanup finds the speakers after the call.
    /// Informational; the reason is also in each track's speaker record.
    LiveSpeakersOff {
        /// Why ([`crate::speaker_timing`]'s reasons).
        reason: String,
    },
    /// The words to show now for the track's unfinished stretch, replacing any shown before: display
    /// only, and nothing of it is ever stored or logged (`preview` module docs). Empty: none.
    LivePreview {
        /// Track.
        track: TrackId,
        /// In time order; each is shown until settled text reaches it.
        words: Vec<earshot_core::asr::Word>,
    },
    /// A track's previews run now (`off` is `None`) or do not, and why (T12.33): once per change,
    /// as it happens, only for a track that has previews ([`LiveConfig::previews`]). A kind, never
    /// text: a host shows its own words.
    LivePreviewState {
        /// Track.
        track: TrackId,
        /// Why there are none now; `None` once they run.
        off: Option<PreviewOffReason>,
    },
    /// What a track's previews did, counts and times only: once per track that had previews
    /// ([`PreviewReport::track`]), after the live transcript finished, in a session configured
    /// with [`LiveConfig::previews`].
    LivePreviewReport(PreviewReport),
    /// Summarising during the meeting ([`LiveConfig::live_summaries`]) does not run (from
    /// the start) or stopped for the rest of the meeting; the summary after the meeting
    /// maps every window it did not. Informational.
    LiveSummariesOff {
        /// Why ([`crate::live_summaries`]'s reasons).
        reason: String,
    },
    /// A window of the meeting was summarised during it ([`crate::live_summaries`]).
    LiveSummaryMapped {
        /// The window: segments starting in `[window * seconds, (window + 1) * seconds)`.
        window: u64,
        /// How long it took (model load included).
        took: Duration,
        /// Why there is no partial (the summary after the meeting maps the window);
        /// `None` when there is one.
        error: Option<String>,
    },
    /// Something the user may need to know; capture continues.
    Warning(RuntimeWarning),
    /// A held or lagging session's live transcript changed state (T9.1l).
    LiveTranscript(LiveTranscriptState),
    /// Capture has ended and the FLAC files and the meeting's duration are saved: nothing more
    /// can be lost, whatever happens to the live work still finishing. Sent once, before
    /// [`RuntimeEvent::LiveTranscriptFinished`] (T9.1l).
    RecordingSaved {
        /// Why capture ended.
        reason: EndReason,
        /// What failed while saving it. It travels here because a new meeting may start from this
        /// event on, long before [`RuntimeEvent::Ended`]. [`LiveOutcome::problems`] stays the whole
        /// record, for whoever reads only the outcome, so a host reading both shows these once.
        problems: Vec<RuntimeWarning>,
    },
    /// Every live transcription worker has finished its queue: the live transcript is
    /// complete (sent once at the end, before the speaker and live-summary threads are
    /// waited for; T7.4c measures catch-up to it). Also sent after a catch-up that was cancelled or
    /// given up (T9.22), when the transcript is not complete: then the outcome's problems hold
    /// [`RuntimeWarning::LiveCatchUpStalled`] or the host cancelled it, and the time is no catch-up.
    LiveTranscriptFinished {
        /// How long after the capture source ended, measured by the session when it sends
        /// this, so it does not depend on when a host reads the event.
        after_capture_end: Duration,
    },
    /// The session ended: the recording is finalised and the live work finished. Always the last
    /// event.
    Ended(LiveOutcome),
}

/// A recoverable problem during a live session.
#[derive(Debug, Clone, PartialEq)]
pub enum RuntimeWarning {
    /// Reported by the pipeline (timestamp jump, live ASR stopped, ...).
    Pipeline(PipelineError),
    /// The capture source reported an error.
    Capture(String),
    /// The previous capture error was repeated this many more times.
    CaptureRepeated {
        /// The message.
        message: String,
        /// Repeats after the first report.
        times: u64,
    },
    /// A track's device is gone; silence is recorded until it is restored.
    DeviceUnavailable {
        /// Track.
        track: TrackId,
    },
    /// A track's device is back (possibly another endpoint).
    DeviceRestored {
        /// Track.
        track: TrackId,
        /// The endpoint now captured.
        device_name: String,
    },
    /// Writing or finalising a track's recording failed; that track has no audio file (its
    /// `.part` is left on disk).
    Recording {
        /// Track.
        track: TrackId,
        /// What failed.
        error: String,
    },
    /// Writing a track's recording failed during the meeting, so that track stopped recording
    /// at `at` (T9.17): the audio before it is kept and finished as a readable file (if even
    /// that fails, [`RuntimeWarning::Recording`] follows), and the other track goes on. Sent once
    /// per track, during the meeting and again in [`LiveOutcome::problems`].
    RecordingStopped {
        /// Track.
        track: TrackId,
        /// Where the kept audio ends.
        at: MeetingTime,
        /// The disk (or the user's quota on it) is full.
        disk_full: bool,
        /// What failed.
        error: String,
    },
    /// Every track of the meeting stopped recording ([`RuntimeWarning::RecordingStopped`]), so
    /// the session stopped the meeting itself (the owner, 2026-11-12). Sent once, after the
    /// last track's `RecordingStopped`, and again in [`LiveOutcome::problems`].
    EveryRecordingStopped {
        /// The last failure was a full disk.
        disk_full: bool,
    },
    /// The audio device (or Windows' audio service) stopped answering when capture was asked
    /// to stop, so the source ended without it and the recording was saved with everything
    /// captured before (T9.20, [`EndpointWarning::StoppedAnswering`]). At most once.
    DeviceStoppedAnswering,
    /// At the end a track's live catch-up showed no sign of life for
    /// [`LiveConfig::catch_up_stall`], so it was given up (T9.22): the recording is saved and the
    /// track's live transcript stops where it got to. Hosts leave the meeting unprocessed: whatever
    /// hung may hang the final pass too.
    LiveCatchUpStalled {
        /// Track.
        track: TrackId,
    },
    /// At the end a held session's model load had not returned within [`LiveHold::load_stall`],
    /// so its thread was left, never joined (T9.22b): the recording is saved and the session ends
    /// without its live transcript. Hosts leave the meeting unprocessed, as for
    /// [`RuntimeWarning::LiveCatchUpStalled`]: the final pass loads the same models.
    LiveLoadStalled,
    /// Storing a row failed (a live segment, or the meeting's duration at the end).
    Persistence(String),
    /// A held session's live transcription could not start (T9.1l). The meeting goes on
    /// recording and the final pass transcribes it.
    LiveLoadFailed {
        /// What failed.
        error: String,
    },
    /// Something a host's own loading found while a session was held (T9.1l), shown as it is.
    HostNote {
        /// The condition (one banner per key).
        key: String,
        /// Plain copy.
        message: String,
    },
    /// A track's live transcription runs on a slower backend than the best one tried
    /// (for example CUDA was refused and the CPU runs it).
    BackendFallback {
        /// Track.
        track: TrackId,
        /// What runs.
        backend: BackendKind,
        /// What was refused and why.
        refused: Vec<(BackendKind, AsrError)>,
    },
    /// At the end a track's live chunker was this far behind and stopped there
    /// ([`LiveConfig::max_stop_backlog`]); the cleanup pass transcribes the rest. Speech
    /// stretches always finish their queue and never send it.
    LiveCutAtStop {
        /// Track.
        track: TrackId,
        /// Audio left undecoded.
        behind: Duration,
    },
    /// The mic is a Bluetooth headset in hands-free mode (T6.24): Windows records it at
    /// 8-16 kHz and lowers the headset's playback while it is open. Sent at the start
    /// (judged on the endpoint opened then), and for a mid-meeting switch onto such a mic
    /// right before that switch's `Discontinuity` event (when the `DeviceChanged` marker
    /// carries [`EndpointWarning::BluetoothHandsFree`]); the computer's own mic with the
    /// headset for listening is the better choice.
    BluetoothHandsFreeMic {
        /// The endpoint's name.
        device_name: String,
        /// Its rate (Hz), when known.
        sample_rate: Option<u32>,
    },
    /// The mic has been speaking and nothing has been heard from the far end's track
    /// (T6.14a, the `voice` module docs): a call's other people may reach the meeting only
    /// through the mic. At most once per session.
    FarEndSilent,
    /// The far end is heard after [`RuntimeWarning::FarEndSilent`]. At most once per session.
    FarEndHeard,
    /// The mic stopped delivering audio for at least [`REPORTED_GAP`] at a time, so speech was
    /// lost (T9.16; the `gaps` module docs), usually because the computer went to sleep. Sent
    /// each time the total grows, and once more in [`LiveOutcome::problems`].
    RecordingGap {
        /// All the audio lost so far in this meeting.
        total: Duration,
    },
    /// A chosen microphone or output (T7.4i, the Audio page) could not be opened when the
    /// meeting started, so the Windows default is recorded instead. Sent at the start with
    /// the other start warnings. (One lost during the meeting is
    /// [`RuntimeWarning::DeviceUnavailable`], then [`RuntimeWarning::ChosenDeviceGivenUp`].)
    ChosenDeviceUnavailable {
        /// [`TrackId::Mic`] for the microphone, [`TrackId::Reference`] for the output.
        track: TrackId,
        /// The chosen endpoint's name when it was chosen (may be empty).
        chosen: String,
        /// The default endpoint recorded instead.
        used: String,
        /// Why it was not used: `None` when it is not connected, else the open error.
        error: Option<String>,
    },
    /// A chosen microphone or output (T7.4i) was lost during the meeting and could not be
    /// opened again within the capture layer's grace period, so the track records the
    /// Windows default for the rest of the meeting. Sent instead of
    /// [`RuntimeWarning::DeviceRestored`] when the default opens (after the
    /// [`RuntimeWarning::DeviceUnavailable`] of the loss).
    ChosenDeviceGivenUp {
        /// The track.
        track: TrackId,
        /// The chosen endpoint's name when it was chosen (may be empty).
        chosen: String,
        /// The default endpoint recorded from now on.
        used: String,
    },
    /// The call plays on another output, which could not be opened, so the output recorded did
    /// not change and the far end may be missing (T6.15).
    CallOutputRefused {
        /// The output the call plays on.
        wanted: String,
        /// The output still recorded, if it has a stream.
        used: Option<String>,
    },
}

/// Result of a live session.
#[derive(Debug, Clone, PartialEq)]
pub struct LiveOutcome {
    /// The meeting row.
    pub meeting: EntityId,
    /// Its tracks (mic first).
    pub tracks: Vec<(TrackId, EntityId)>,
    /// Meeting duration (end of the recorded audio).
    pub end: MeetingTime,
    /// Why capture ended.
    pub reason: EndReason,
    /// What the final pass should transcribe (T6.4b runs it).
    pub final_pass: FinalPassRequest,
    /// The `AudioFile` rows written (one per track whose recording succeeded).
    pub audio_files: Vec<(TrackId, EntityId)>,
    /// Tracks whose device was unavailable when capture ended.
    pub unavailable_at_end: Vec<TrackId>,
    /// Failures while finalising (a track's audio file, the meeting row) and any
    /// [`RuntimeWarning::RecordingGap`]; empty when everything was recorded and stored.
    pub problems: Vec<RuntimeWarning>,
    /// Live "Me" words removed as echo of the far end ([`echo`]).
    pub echo_words_removed: usize,
    /// The stretches in which the echo canceller held a delay lock (empty: never; no
    /// reference, headphones); the final pass cleans echo in each with its delay.
    pub echo_locks: Vec<crate::echo::EchoLock>,
}

/// A running live session; see the module docs. Dropping it requests a stop (without
/// waiting); the session thread still finalises the recording.
#[derive(Debug)]
pub struct LiveSession {
    meeting: EntityId,
    events: Option<Receiver<RuntimeEvent>>,
    stop: Arc<AtomicBool>,
    /// Cancels the live catch-up after the recording ([`LiveSession::cancel_catch_up`]).
    catch_up: CancelToken,
    thread: Option<JoinHandle<Result<LiveOutcome, RuntimeError>>>,
}

impl LiveSession {
    /// The meeting being recorded.
    #[must_use]
    pub fn meeting(&self) -> EntityId {
        self.meeting
    }

    /// Takes the event stream (once), for example to hand it to the UI thread while
    /// this handle keeps the controls. The stream closes after [`RuntimeEvent::Ended`];
    /// events are buffered until received.
    pub fn take_events(&mut self) -> Option<Receiver<RuntimeEvent>> {
        self.events.take()
    }

    /// Requests the end of capture without waiting: the session thread stops the source,
    /// finalises the recording and sends [`RuntimeEvent::Ended`]; then [`LiveSession::wait`]
    /// returns the outcome. Repeating it has no further effect.
    pub fn request_stop(&self) {
        self.stop.store(true, Ordering::Release);
    }

    /// Stops the live transcript's catch-up after the recording (T9.22): every worker's decode is
    /// asked to stop and the session finishes without waiting for it, so `Ended` comes at once. The
    /// recording is unaffected; call it once the recording is saved (before, it ends the catch-up
    /// before it begins). Repeating it has no further effect.
    pub fn cancel_catch_up(&self) {
        self.catch_up.cancel();
    }

    /// Requests the end of capture and waits until the recording is finalised. Do not
    /// hold [`Services::store`] while calling this.
    ///
    /// # Errors
    /// As [`LiveSession::wait`].
    pub fn stop(self) -> Result<LiveOutcome, RuntimeError> {
        self.stop.store(true, Ordering::Release);
        self.wait()
    }

    /// Waits until the source ends by itself (a replay) and the recording is finalised.
    /// Do not hold [`Services::store`] while calling this.
    ///
    /// # Errors
    /// [`RuntimeError::Poisoned`] if the session thread panicked;
    /// [`RuntimeError::Internal`] if the pipeline broke its end-of-session contract.
    pub fn wait(mut self) -> Result<LiveOutcome, RuntimeError> {
        let thread = self
            .thread
            .take()
            .ok_or(RuntimeError::Internal("session already joined"))?;
        thread.join().map_err(|_| RuntimeError::Poisoned)?
    }
}

impl Drop for LiveSession {
    fn drop(&mut self) {
        if self.thread.is_some() {
            self.stop.store(true, Ordering::Release);
        }
    }
}

/// A track's live transcription, ready to start on its worker.
struct BuiltTrack {
    track: TrackId,
    live: LiveTrack,
    backend: BackendInfo,
    fallbacks: Fallbacks,
    /// How the stretches transcribe, for the final pass (T6.13c); `None` for the chunker.
    record: Option<StretchRecord>,
    speakers: Option<LiveSpeakers>,
}

/// The [`LiveTrack`] of `asr`.
fn build_track(
    track: TrackId,
    asr: TrackAsr,
    transcription: &LiveTranscription,
) -> Result<BuiltTrack, RuntimeError> {
    let TrackAsr {
        asr,
        fallbacks,
        speakers,
        evidence: _,
    } = asr;
    let backend = asr.transcriber.backend_info();
    let (live, record) = match transcription {
        LiveTranscription::Stretches(pieces) => {
            let record = StretchRecord {
                backend: backend.clone(),
                options: asr.config.options.clone(),
                pieces: pieces.clone(),
                vad: asr.vad.id(),
                vad_frame: asr.vad.frame_len(),
                decoded: earshot_core::asr::DecodedPieces::default(),
                stopped: None,
            };
            (LiveTrack::with_stretches(track, asr, pieces)?, Some(record))
        }
        LiveTranscription::Chunker => (LiveTrack::new(track, asr)?, None),
    };
    Ok(BuiltTrack {
        track,
        live,
        backend,
        fallbacks,
        record,
        speakers,
    })
}

/// Starts a live session recording `source`; see the module docs.
///
/// `rng` is the session's id generator (production: [`crate::os_seeded_rng`]).
///
/// # Errors
/// [`RuntimeError::Pipeline`] for an invalid pipeline config or an unusable capture
/// rate; errors from `live_asr`; store errors creating the FLAC files or the rows;
/// [`RuntimeError::Spawn`]. In every case no meeting row is left behind.
pub fn start_live<S>(
    source: S,
    services: Services,
    cfg: LiveConfig,
    live_asr: Option<LiveAsrFactory>,
    rng: BoxRng,
) -> Result<LiveSession, RuntimeError>
where
    S: AudioSource + Send + 'static,
    S::Error: std::fmt::Debug,
{
    start_live_with_warnings(source, services, cfg, live_asr, rng, Vec::new())
}

/// [`start_live`] with `start_warnings` about the capture known before it started (a
/// Bluetooth hands-free mic, T6.24): each is sent as a [`RuntimeEvent::Warning`] right
/// after `Started` and the `LiveBackend` events, before any audio event.
///
/// # Errors
/// As [`start_live`].
pub fn start_live_with_warnings<S>(
    source: S,
    services: Services,
    cfg: LiveConfig,
    mut live_asr: Option<LiveAsrFactory>,
    mut rng: BoxRng,
    start_warnings: Vec<RuntimeWarning>,
) -> Result<LiveSession, RuntimeError>
where
    S: AudioSource + Send + 'static,
    S::Error: std::fmt::Debug,
{
    let capture = source.session().clone();
    let mut pipeline = Pipeline::new(capture.clone(), cfg.pipeline.clone())?;
    let clock = Arc::clone(&services.clock);
    let mut voice: [Option<VoiceTracker>; 2] = [None, None];
    for track in capture.tracks() {
        voice[slot(track)] =
            Some(VoiceTracker::new(&cfg.voice_activity).map_err(RuntimeError::Asr)?);
    }
    let far_end = capture
        .tracks()
        .any(|t| t == TrackId::Reference)
        .then(|| FarEndWatch::new(&cfg.voice_activity));

    // The rows, built but not yet committed (ids are needed for live ASR and the files).
    let (meeting, tracks, summary_vocabulary) = {
        let mut db = services.store();
        // The user's own words for the summary windows (T12.15a), read once at the start;
        // a setting never costs a recording, so one that cannot be read is no words.
        let summary_vocabulary = if cfg.live_summaries.is_some() {
            crate::vocabulary::vocabulary(&db).map_or_else(|_| Vocabulary::default(), |(v, _)| v)
        } else {
            Vocabulary::default()
        };
        let (_, hlc) = db.parts();
        let mut stamp = Stamp {
            clock: &*clock,
            hlc,
            rng: &mut *rng,
        };
        let meeting = Meeting::new(
            cfg.title.clone(),
            clock.now_unix_ms(),
            cfg.utc_offset_minutes,
            cfg.template,
            &mut stamp,
        );
        let tracks: Vec<Track> = capture
            .tracks()
            .map(|id| {
                let device = capture.track(id).and_then(|t| t.device_name.clone());
                Track::new(meeting.id, id, device, cfg.clock_domain, &mut stamp)
            })
            .collect();
        (meeting, tracks, summary_vocabulary)
    };
    // Held by the session thread until it ends: the meeting cannot be deleted meanwhile.
    let lease = services.lease_meeting_for(meeting.id, crate::MeetingJob::Recording)?;

    let mut live_backends = Vec::new();
    let mut workers = Vec::new();
    let mut stretch_records = Vec::new();
    let mut speaker_threads = Vec::new();
    let mut speaker_setups = Vec::new();
    // With a hold the factory runs later, on the hold's thread (`hold` module docs).
    let held = cfg.hold.clone().filter(|_| live_asr.is_some());
    let previews = LivePreviews::states(
        cfg.previews.clone().filter(|_| live_asr.is_some()),
        capture.reference.is_some(),
    );
    let worker_cap = live_asr_backlog(&cfg);
    if held.is_none()
        && let Some(factory) = live_asr.as_mut()
    {
        for track in &tracks {
            if let Some(asr) = factory(track.kind, track.id)? {
                let built = build_track(track.kind, asr, &cfg.transcription)?;
                live_backends.push((track.kind, built.backend, built.fallbacks));
                if let Some(record) = built.record {
                    stretch_records.push((track.kind, record));
                }
                if let Some(s) = built.speakers {
                    speaker_setups.push((track.kind, s));
                }
                // Live ASR runs on the track's own worker thread (T6.10a).
                let worker =
                    LiveWorker::spawn(track.kind, built.live, cfg.pipeline.partials, worker_cap)?;
                // The pipeline reports the worker's backlog and final-pass entry as its own.
                pipeline.register_external_live(worker.final_pass(), worker.backlog_source())?;
                workers.push(worker);
            }
        }
    }
    // The speaker detection setting (T6.13d4): the power supply is read only when it
    // decides something. Speaker work off from the start loads no model and leaves each
    // track a reason-only record (`crate::speakers::record_speakers_off`), so the cleanup
    // reports why it diarised the whole track.
    // Live summaries need live transcription (the windows are of its rows).
    let live_summaries = cfg
        .live_summaries
        .clone()
        .filter(|_| !workers.is_empty() || held.is_some());
    let held_speakers = held.as_ref().is_some_and(|h| h.speakers.is_some());
    let supply = (cfg.speaker_timing == SpeakerTiming::AfterCallOnBattery
        && (!speaker_setups.is_empty() || held_speakers || live_summaries.is_some()))
    .then(|| cfg.power.supply());
    // Summaries follow the battery rule of speaker detection (T8.3j).
    let summaries_off = (live_summaries.is_some()
        && cfg.speaker_timing == SpeakerTiming::AfterCallOnBattery
        && supply == Some(PowerSupply::Battery))
    .then(|| crate::live_summaries::ON_BATTERY_REASON.to_owned());
    let speakers_off = if speaker_setups.is_empty() && !held_speakers {
        None
    } else {
        cfg.speaker_timing
            .at_start(supply.unwrap_or(PowerSupply::Mains))
            .err()
    };
    let mut speaker_tracks = Vec::new();
    // Every track of a held session would have had speaker work.
    let mut speakers_off_tracks: Vec<TrackId> = if held_speakers && speakers_off.is_some() {
        capture.tracks().collect()
    } else {
        Vec::new()
    };
    for (track, s) in speaker_setups {
        if speakers_off.is_some() {
            speakers_off_tracks.push(track);
            continue;
        }
        speaker_threads.push((track, s.threads));
        speaker_tracks.push(SpeakerTrack {
            track,
            diarizer: (s.models)()?,
            threads: s.threads,
            progress: s.progress,
            background: s.background,
        });
    }
    // A held session's summary worker starts with its live transcript.
    let (summaries, pending_summaries) = match (&live_summaries, &summaries_off, &held) {
        (Some(l), None, Some(_)) => (None, Some(l.clone())),
        (Some(l), None, None) => (Some(SummaryThread::spawn(l)?), None),
        _ => (None, None),
    };
    let watches = supply.is_some_and(|s| cfg.speaker_timing.watches_power(s));
    let watch_power =
        (speakers_off.is_none() && !speaker_tracks.is_empty() || summaries.is_some()) && watches;
    // Speaker work runs on its own thread, never on the transcription workers (T6.13d3).
    let speakers = if speaker_tracks.is_empty() {
        None
    } else {
        Some(SpeakerThread::spawn(speaker_tracks)?)
    };
    let mut writers: [Option<FlacTrackWriter>; 2] = [None, None];
    let abort = |writers: &mut [Option<FlacTrackWriter>; 2]| {
        for w in writers.iter_mut().filter_map(Option::take) {
            let _ = w.abort();
        }
    };
    for track in &tracks {
        match services.data.track_writer(
            meeting.id,
            track.kind,
            earshot_core::audio::PIPELINE_RATE.hz(),
        ) {
            Ok(w) => {
                writers[slot(track.kind)] = Some(with_test_fault(&services.data, track.kind, w));
            }
            Err(e) => {
                abort(&mut writers);
                return Err(e.into());
            }
        }
    }

    // Commit the rows last, then start the thread (which announces Started).
    let committed = {
        let (m, t) = (meeting.clone(), tracks.clone());
        let keep_local = cfg.keep_summary_local;
        services
            .store()
            .transaction(|tx| -> Result<(), RuntimeError> {
                let id = m.id;
                tx.put_meeting(m)?;
                for track in t {
                    tx.put_track(track)?;
                }
                if keep_local {
                    crate::keep_local::mark_kept_local_tx(tx, id)?;
                }
                Ok(())
            })
    };
    if let Err(e) = committed {
        abort(&mut writers);
        return Err(e);
    }
    // Which backend runs the live pass, per track (device-local provenance).
    for (track, backend, _) in &live_backends {
        let stored = crate::backends::record_pass_backend(
            &mut services.store(),
            meeting.id,
            TranscriptPass::Live,
            Some(*track),
            backend,
        );
        if let Err(e) = stored {
            abort(&mut writers);
            withdraw(&services, meeting.id);
            return Err(e);
        }
    }

    // A held session holds its tracks' final-pass entries until its live work starts.
    let hold = match (held, live_asr.take()) {
        (Some(hold), Some(factory)) => {
            let ids = tracks.iter().map(|t| (t.kind, t.id)).collect();
            let mut state = HoldState::new(
                hold,
                factory,
                ids,
                cfg.transcription.clone(),
                speakers_off.is_none(),
            );
            state.watch_power = watches;
            for track in &tracks {
                let entry = held_entry(track.kind, track.id, &cfg.transcription);
                let registered =
                    pipeline.register_held_live(entry, state.backlog_source(track.kind));
                if let Err(e) = registered {
                    abort(&mut writers);
                    withdraw(&services, meeting.id);
                    return Err(e.into());
                }
            }
            Some(state)
        }
        _ => None,
    };

    let (tx, rx) = channel();
    let stop = Arc::new(AtomicBool::new(false));
    let catch_up = CancelToken::new();
    let meeting_id = meeting.id;
    let session = Session {
        source,
        pipeline,
        capture,
        services: services.clone(),
        rng,
        meeting_id,
        tracks,
        writers,
        events: tx,
        policy: cfg,
        unavailable: [false, false],
        final_pass: None,
        ended: None,
        last_resync: None,
        repeated_capture: None,
        live_backends,
        echo: LiveEchoFilter::new(),
        workers,
        speakers,
        deferred: Vec::new(),
        stretch_records,
        speaker_threads,
        speaker_records: Vec::new(),
        _lease: lease,
        speakers_off_tracks: speakers_off_tracks
            .into_iter()
            .map(|t| (t, speakers_off.clone().unwrap_or_default()))
            .collect(),
        speakers_off,
        next_power_read: watch_power.then(Instant::now),
        summaries,
        capture_ended: None,
        summaries_off,
        summary_rows: Vec::new(),
        summary_vocabulary,
        next_window: 0,
        summary_record: None,
        start_warnings,
        voice,
        far_end,
        sent: [0, 0],
        hold,
        pending_summaries,
        worker_cap,
        transcript_state: None,
        capture_end: None,
        audio_files: Vec::new(),
        problems: Vec::new(),
        gaps: gaps::GapWatch::default(),
        previews,
        preview_said: [None, None],
        next_preview_power: None,
        preview_rule_allowed: [false, false],
        waiting_power: None,
        speaking: [false, false],
        recording_stopped: [false, false],
        stop_self: false,
        recording_saved: false,
        catch_up: catch_up.clone(),
    };
    let flag = Arc::clone(&stop);
    let spawned = std::thread::Builder::new()
        .name("earshot-session".into())
        .spawn(move || {
            // A bug on this thread must not cost the recording (T9.19): the writers are finished
            // as far as they got and the meeting ends Failed, `RecordingSaved` and `Ended` each
            // sent once; `wait` still reports the panic.
            let mut session = session;
            match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| session.run(&flag))) {
                Ok(result) => result,
                Err(_) => {
                    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        session.after_panic();
                    }));
                    Err(RuntimeError::Poisoned)
                }
            }
        });
    let thread = match spawned {
        Ok(thread) => thread,
        Err(e) => {
            // The writers went down with the closure; take the rows back.
            withdraw(&services, meeting_id);
            return Err(RuntimeError::Spawn(e));
        }
    };
    Ok(LiveSession {
        meeting: meeting_id,
        events: Some(rx),
        stop,
        catch_up,
        thread: Some(thread),
    })
}

/// `w` with the write failure a test set for `track` ([`write_faults`]).
#[cfg(test)]
fn with_test_fault(
    data: &earshot_store::DataDir,
    track: TrackId,
    mut w: FlacTrackWriter,
) -> FlacTrackWriter {
    if let Some(fault) = write_faults::take(data, track) {
        w.inject_fault(fault);
    }
    w
}

/// `w` unchanged: production never injects a failure.
#[cfg(not(test))]
fn with_test_fault(
    _data: &earshot_store::DataDir,
    _track: TrackId,
    w: FlacTrackWriter,
) -> FlacTrackWriter {
    w
}

/// Tombstones a meeting whose session could not start.
fn withdraw(services: &Services, meeting: EntityId) {
    let clock = Arc::clone(&services.clock);
    let mut db = services.store();
    if let Ok(Some(mut m)) = db.meeting(meeting) {
        let (store, hlc) = db.parts();
        m.meta.mark_deleted(hlc, &*clock);
        let _ = store.put_meeting(m);
    }
}

/// The speaker models' thread count of `track` ([`LiveSpeakers::threads`]).
fn threads_of(threads: &[(TrackId, Option<usize>)], track: TrackId) -> Option<usize> {
    threads
        .iter()
        .find(|(t, _)| *t == track)
        .and_then(|(_, n)| *n)
}

fn slot(track: TrackId) -> usize {
    match track {
        TrackId::Mic => 0,
        TrackId::Reference => 1,
    }
}

/// State owned by the session thread.
struct Session<S> {
    source: S,
    pipeline: Pipeline,
    capture: CaptureSession,
    services: Services,
    rng: BoxRng,
    meeting_id: EntityId,
    tracks: Vec<Track>,
    writers: [Option<FlacTrackWriter>; 2],
    events: Sender<RuntimeEvent>,
    policy: LiveConfig,
    unavailable: [bool; 2],
    final_pass: Option<FinalPassRequest>,
    ended: Option<PipelineEnd>,
    last_resync: Option<MeetingTime>,
    /// The last capture error and how often it repeated since it was reported.
    repeated_capture: Option<(String, u64)>,
    /// The live backend of each transcribed track, announced after `Started`.
    live_backends: Vec<(TrackId, BackendInfo, Fallbacks)>,
    /// Echo cleaning of the live "Me" transcript.
    echo: LiveEchoFilter,
    /// Live ASR workers, one per transcribed track (empty once finished).
    workers: Vec<LiveWorker>,
    /// Live speaker work of every track with speaker models (`None` without, or once
    /// finished).
    speakers: Option<SpeakerThread>,
    /// The pipeline's `FinalPassRequested` and `Ended`, held until the workers finish.
    deferred: Vec<PipelineEvent>,
    /// Per stretch track, the record written at the end ([`crate::stretches`]): how the
    /// stretches transcribe from the start, what they decoded once the workers finished.
    stretch_records: Vec<(TrackId, StretchRecord)>,
    /// Per track with live speaker work, its models' thread count (T6.13d2).
    speaker_threads: Vec<(TrackId, Option<usize>)>,
    /// Per track with live speaker work, the record written at the end
    /// ([`crate::speakers`]), built once the workers finished.
    speaker_records: Vec<(TrackId, SpeakerRecord)>,
    /// The session's claim on its meeting (released when the session thread ends).
    _lease: crate::MeetingLease,
    /// The tracks whose live speaker work was off from the start, with the reason, recorded
    /// at the end (reason-only records, [`crate::speakers::record_speakers_off`]).
    speakers_off_tracks: Vec<(TrackId, String)>,
    /// Why live speaker work is off from the start (T6.13d4), announced after `Started`.
    speakers_off: Option<String>,
    /// When the session next reads the power supply (`None`: it does not watch it,
    /// [`LiveConfig::power_poll`]).
    next_power_read: Option<Instant>,
    /// The map worker of live summaries (`None` when off, or once finished).
    summaries: Option<SummaryThread>,
    /// When the capture source ended.
    capture_ended: Option<Instant>,
    /// Why live summaries are off from the start, announced after `Started`.
    summaries_off: Option<String>,
    /// The live rows stored so far, as the map worker reads them (kept only with a map
    /// worker).
    summary_rows: Vec<PromptSegment>,
    /// The user's own words, given to every summary window (T12.15a; read at the start).
    summary_vocabulary: Vocabulary,
    /// The next window not yet sent to the map worker.
    next_window: u64,
    /// What the map worker left, written at the end.
    summary_record: Option<LiveSummaryRecord>,
    /// Warnings known before capture started, sent after the backend events.
    start_warnings: Vec<RuntimeWarning>,
    /// Each track's voice-activity indicator (by [`slot`]; `None` for a track the session
    /// does not have).
    voice: [Option<VoiceTracker>; 2],
    /// Compares the tracks' voice activity (`None` for a session with no reference track).
    far_end: Option<FarEndWatch>,
    /// Pipeline frames sent to each track's transcription worker (by [`slot`]), for
    /// [`RuntimeEvent::TranscriptLag`].
    sent: [u64; 2],
    /// The live work not yet started (`hold` module docs); `None` once it all has.
    hold: Option<HoldState>,
    /// A held session's summary worker, until its live transcript starts.
    pending_summaries: Option<LiveSummaries>,
    /// The most a worker may queue ([`live_asr_backlog`]).
    worker_cap: u64,
    /// The live transcript's last reported state.
    transcript_state: Option<LiveTranscriptState>,
    /// The pipeline's end, as soon as it is known.
    capture_end: Option<PipelineEnd>,
    /// The `AudioFile` rows written when the recording was saved.
    audio_files: Vec<(TrackId, EntityId)>,
    /// Failures while finalising.
    problems: Vec<RuntimeWarning>,
    /// The audio the mic did not deliver.
    gaps: gaps::GapWatch,
    /// Each track's previews (by [`slot`], `preview` module docs).
    previews: [PreviewState; 2],
    /// What each track last said of its previews in [`RuntimeEvent::LivePreviewState`].
    preview_said: [Option<Option<PreviewOffReason>>; 2],
    /// When the running previews next read the power supply.
    next_preview_power: Option<Instant>,
    /// Each track whose previews the rule ([`LivePreviews::rule`]) allowed: asked once, because a
    /// track's backend and model never change once its stretch record exists (T12.33).
    preview_rule_allowed: [bool; 2],
    /// The power supply as previews waiting for a hold last read it, and when they read it next
    /// (at most every [`LiveConfig::power_poll`], T12.33).
    waiting_power: Option<(PowerSupply, Instant)>,
    /// Each track's voice activity now (by [`slot`]).
    speaking: [bool; 2],
    /// Each track whose recording stopped on a write error (by [`slot`], T9.17).
    recording_stopped: [bool; 2],
    /// Every track stopped recording, so the session stops the meeting itself.
    stop_self: bool,
    /// [`RuntimeEvent::RecordingSaved`] was sent (T9.19: a panic after it does not send it
    /// again).
    recording_saved: bool,
    /// The live catch-up's cancel ([`LiveSession::cancel_catch_up`]).
    catch_up: CancelToken,
}

impl<S> Session<S>
where
    S: AudioSource,
    S::Error: std::fmt::Debug,
{
    fn emit(&self, event: RuntimeEvent) {
        // The UI may have gone away; the recording still completes.
        let _ = self.events.send(event);
    }

    fn warn(&self, warning: RuntimeWarning) {
        self.emit(RuntimeEvent::Warning(warning));
    }

    fn run(&mut self, stop: &AtomicBool) -> Result<LiveOutcome, RuntimeError> {
        self.emit(RuntimeEvent::Started {
            meeting: self.meeting_id,
            tracks: self.tracks.iter().map(|t| (t.kind, t.id)).collect(),
        });
        for (track, backend, fallbacks) in std::mem::take(&mut self.live_backends) {
            if !fallbacks.is_empty() {
                self.warn(RuntimeWarning::BackendFallback {
                    track,
                    backend: backend.kind,
                    refused: fallbacks.clone(),
                });
            }
            self.emit(RuntimeEvent::LiveBackend {
                track,
                backend,
                fallbacks,
            });
        }
        if let Some(reason) = self.speakers_off.take() {
            self.emit(RuntimeEvent::LiveSpeakersOff { reason });
        }
        if let Some(reason) = self.summaries_off.take() {
            self.emit(RuntimeEvent::LiveSummariesOff { reason });
        }
        for warning in std::mem::take(&mut self.start_warnings) {
            self.warn(warning);
        }
        self.begin_hold();
        let mut stop_sent = false;
        while !self.pipeline.is_ended() {
            if !stop_sent && (self.stop_self || stop.load(Ordering::Acquire)) {
                self.source.stop();
                stop_sent = true;
            }
            let status = match self.source.pump(&mut self.pipeline) {
                Ok(status) => status,
                Err(e) => {
                    self.capture_error(format!("{e:?}"));
                    SourceStatus::Idle
                }
            };
            let events = self.pipeline.poll();
            self.handle(events);
            self.handle_workers();
            self.check_hold();
            self.check_catch_up();
            self.check_previews();
            self.check_power();
            match status {
                SourceStatus::Delivered => {}
                SourceStatus::Idle => std::thread::sleep(self.policy.idle_wait),
                SourceStatus::Ended => {
                    self.capture_ended.get_or_insert_with(Instant::now);
                    if !self.pipeline.is_ended() {
                        // A source that claims to have ended without delivering its end
                        // broke its contract: an error end.
                        self.pipeline.end(EndOfStream {
                            at: self.capture.origin,
                            reason: EndReason::Failed,
                        });
                    }
                }
            }
        }
        let events = self.pipeline.poll();
        self.handle(events);
        // Every sample is written: the recording is saved before anything that may take long.
        self.save_recording();
        self.stop_previews();
        self.end_hold();
        self.finish_workers();
        self.flush_capture_repeats();
        let end = self.ended.map(|e| e.end);
        for track in self.tracks.iter().map(|t| t.kind).collect::<Vec<_>>() {
            let off = match (self.voice[slot(track)].as_mut(), end) {
                (Some(v), Some(end)) => v.finish(end),
                _ => None,
            };
            if let Some((speaking, at)) = off {
                self.emit(RuntimeEvent::VoiceActivity {
                    track,
                    speaking,
                    at,
                });
            }
        }
        let result = self.finish();
        if let Ok(outcome) = &result {
            self.emit(RuntimeEvent::Ended(outcome.clone()));
        }
        result
    }

    /// Reports a capture error, counting identical repeats instead of re-sending them.
    fn capture_error(&mut self, message: String) {
        if let Some((last, times)) = &mut self.repeated_capture
            && *last == message
        {
            *times += 1;
            return;
        }
        self.flush_capture_repeats();
        self.warn(RuntimeWarning::Capture(message.clone()));
        self.repeated_capture = Some((message, 0));
    }

    fn flush_capture_repeats(&mut self) {
        if let Some((message, times)) = self.repeated_capture.take()
            && times > 0
        {
            self.warn(RuntimeWarning::CaptureRepeated { message, times });
        }
    }

    fn handle(&mut self, events: Vec<PipelineEvent>) {
        let mut latest_stats = None;
        for event in events {
            if let PipelineEvent::Ended(end) = &event {
                self.capture_end = Some(*end);
            }
            match event {
                PipelineEvent::Audio(audio) => {
                    self.record(audio.track, &audio.samples);
                    self.voice_activity(audio.track, audio.start, &audio.samples);
                    self.preview_audio(audio.track, audio.start, &audio.samples);
                    let mut over_cap = false;
                    if let Some(w) = self.workers.iter().find(|w| w.track() == audio.track) {
                        w.send(&audio.samples);
                        self.sent[slot(audio.track)] = audio.start + audio.samples.len() as u64;
                        over_cap = w.backlog() > self.worker_cap;
                    }
                    if let Some(s) = &self.speakers {
                        s.send(audio.track, &audio.samples);
                    }
                    if self.holding() {
                        self.hold_audio(audio.track, &audio.samples);
                    }
                    // The worker stops itself when it next takes its queue (`worker` docs).
                    if over_cap {
                        self.set_transcript_state(LiveTranscriptState::Stopped);
                    }
                }
                PipelineEvent::Levels(l) => {
                    self.echo.progress(l.end);
                    self.emit(RuntimeEvent::Levels(l));
                }
                PipelineEvent::Stats(s) => {
                    if let Some(total) = self.gaps.stats(&s.mic) {
                        self.warn(RuntimeWarning::RecordingGap { total });
                    }
                    self.echo.stats(&s);
                    latest_stats = Some(s.clone());
                    self.emit(RuntimeEvent::Stats(s));
                }
                PipelineEvent::Transcript {
                    track: TrackId::Mic,
                    event,
                } => match event {
                    // Mic finals go through the echo filter before they are shown or stored.
                    TranscriptEvent::Final(segment) => self.echo.mic_final(segment),
                    TranscriptEvent::Partial(p) => self.emit(RuntimeEvent::Transcript {
                        track: TrackId::Mic,
                        event: TranscriptEvent::Partial(self.echo.mic_partial(p)),
                    }),
                    event @ TranscriptEvent::Discarded { .. } => {
                        self.emit(RuntimeEvent::Transcript {
                            track: TrackId::Mic,
                            event,
                        });
                    }
                },
                PipelineEvent::Transcript { track, event } => {
                    self.echo.reference(&event);
                    if let TranscriptEvent::Final(segment) = &event {
                        self.preview_settled(track, segment.end);
                        self.store_live(segment.clone());
                    }
                    self.emit(RuntimeEvent::Transcript { track, event });
                }
                PipelineEvent::Discontinuity { track, at, kind } => {
                    self.device_state(track, &kind);
                    self.emit(RuntimeEvent::Discontinuity { track, at, kind });
                }
                PipelineEvent::Error(e) => {
                    if let PipelineError::LiveAsr { track, error } = &e
                        && let Some((_, r)) =
                            self.stretch_records.iter_mut().find(|(t, _)| t == track)
                    {
                        r.stopped.get_or_insert_with(|| error.to_string());
                    }
                    if let PipelineError::TimestampJump {
                        track: TrackId::Mic,
                        gap,
                        backwards: false,
                    } = e
                    {
                        let total = self.gaps.jump(gap);
                        self.warn(RuntimeWarning::RecordingGap { total });
                    }
                    self.warn(RuntimeWarning::Pipeline(e));
                }
                event @ (PipelineEvent::FinalPassRequested(_) | PipelineEvent::Ended(_))
                    if !self.workers.is_empty() =>
                {
                    self.deferred.push(event);
                }
                PipelineEvent::FinalPassRequested(req) => self.final_pass = Some(req),
                PipelineEvent::Ended(end) => self.ended = Some(end),
            }
        }
        // Mic finals the echo filter lets through (all of them once the pipeline ended).
        for released in self.echo.release(self.ended.is_some()) {
            let event = match released {
                Released::Kept(segment) => {
                    self.preview_settled(TrackId::Mic, segment.end);
                    self.store_live(segment.clone());
                    TranscriptEvent::Final(segment)
                }
                Released::Dropped(id) => TranscriptEvent::Discarded { id },
            };
            self.emit(RuntimeEvent::Transcript {
                track: TrackId::Mic,
                event,
            });
        }
        if latest_stats.is_some() {
            self.transcript_lags();
        }
        // One decision per poll, on the newest snapshot (older ones in the same poll
        // describe a state the pipeline has already left).
        if let Some(stats) = latest_stats
            && !self.pipeline.is_ended()
        {
            self.check_clocks(&stats);
        }
    }

    /// Runs `track`'s voice-activity indicator over its next pipeline audio and sends the
    /// changes.
    fn voice_activity(&mut self, track: TrackId, start: u64, samples: &[f32]) {
        let Some(v) = self.voice[slot(track)].as_mut() else {
            return;
        };
        for (speaking, at) in v.push(start, samples) {
            self.speaking[slot(track)] = speaking;
            self.preview_voice(track, speaking, at);
            if let Some(w) = self.far_end.as_mut() {
                w.change(track == TrackId::Reference, speaking, at);
            }
            self.emit(RuntimeEvent::VoiceActivity {
                track,
                speaking,
                at,
            });
        }
        let now = MeetingTime::from_pipeline_frames(start + samples.len() as u64);
        match self.far_end.as_mut().and_then(|w| w.check(now)) {
            Some(FarEndChange::Silent) => self.warn(RuntimeWarning::FarEndSilent),
            Some(FarEndChange::Heard) => self.warn(RuntimeWarning::FarEndHeard),
            None => {}
        }
    }

    /// Sends each transcribed track's [`RuntimeEvent::TranscriptLag`].
    fn transcript_lags(&self) {
        for w in &self.workers {
            let behind = transcript_lag(self.sent[slot(w.track())], w.decoded_through());
            self.emit(RuntimeEvent::TranscriptLag {
                track: w.track(),
                behind,
            });
        }
    }

    /// The battery rule of the speaker detection setting (T6.13d4): on battery, speaker
    /// work stops for the rest of the meeting (the batches finished before stay valid) and
    /// the session stops watching. Plugging in again does not restart it: speaker windows
    /// are computed from the start of the track.
    fn check_power(&mut self) {
        let Some(due) = self.next_power_read else {
            return;
        };
        let now = Instant::now();
        if now < due {
            return;
        }
        if self.speakers.is_none() && self.summaries.is_none() {
            self.next_power_read = None;
            return;
        }
        if self.policy.power.supply() != PowerSupply::Battery {
            self.next_power_read = Some(now + self.policy.power_poll);
            return;
        }
        self.next_power_read = None;
        if let Some(speakers) = &self.speakers
            && speakers.stop_work(UNPLUGGED_REASON)
        {
            self.emit(RuntimeEvent::LiveSpeakersOff {
                reason: UNPLUGGED_REASON.to_owned(),
            });
        }
        let why = crate::live_summaries::UNPLUGGED_REASON;
        if let Some(summaries) = &mut self.summaries
            && summaries.stop_work(why)
        {
            self.emit(RuntimeEvent::LiveSummariesOff {
                reason: why.to_owned(),
            });
        }
    }

    /// Handles the transcript events the live workers produced since the last call.
    fn handle_workers(&mut self) {
        // Mic finals wait for the reference worker's progress (live echo hold).
        if let Some(r) = self
            .workers
            .iter()
            .find(|w| w.track() == TrackId::Reference)
        {
            self.echo.reference_progress(r.decoded_through());
        }
        if let Some(m) = self.workers.iter().find(|w| w.track() == TrackId::Mic) {
            self.echo.mic_progress(m.decoded_through());
        }
        // Each track's decoded frontier, read before its events (a worker sends a decode's
        // events before it publishes the frontier), for the live summary windows.
        let frontiers: Vec<(TrackId, MeetingTime)> = self
            .workers
            .iter()
            .map(|w| (w.track(), w.decoded_through()))
            .collect();
        let mut events = Vec::new();
        for w in &self.workers {
            let (e, switches) = w.drain();
            events.extend(e);
            for p in switches {
                self.emit(RuntimeEvent::LivePartials {
                    track: w.track(),
                    enabled: p.enabled,
                    behind: p.behind,
                });
            }
        }
        if !events.is_empty() {
            self.handle(events);
        }
        self.feed_summaries(&frontiers);
        self.mapped_events();
    }

    /// Sends the map worker every window that every transcribed track has settled past
    /// (`crate::live_summaries` docs): its worker decoded past the window's end, and for
    /// the mic no final starting before it is still held by the echo filter.
    fn feed_summaries(&mut self, frontiers: &[(TrackId, MeetingTime)]) {
        let Some(thread) = &self.summaries else {
            return;
        };
        let held = self.echo.held_from();
        let Some(settled) = frontiers
            .iter()
            .map(|(track, d)| match (track, held) {
                (TrackId::Mic, Some(h)) => (*d).min(h),
                _ => *d,
            })
            .min()
        else {
            return;
        };
        let window_ms = u64::from(thread.seconds()) * 1000;
        while (self.next_window + 1) * window_ms <= settled.as_millis() {
            let k = self.next_window;
            self.next_window += 1;
            let end = (k + 1) * window_ms;
            let mut segments: Vec<PromptSegment> = self
                .summary_rows
                .iter()
                .filter(|s| s.start.as_millis() < end)
                .cloned()
                .collect();
            segments.sort_by_key(|s| (s.start, s.id));
            let input = SummaryInput {
                // As the meeting reads during it (`crate::live_summaries` docs).
                meeting: MeetingInfo {
                    title: None,
                    started: None,
                    duration_ms: 0,
                    attendees: Vec::new(),
                },
                segments,
                notes: Vec::new(),
                template: self.policy.template,
                prompt_variant: earshot_core::prompt::PromptVariant::Current,
                rules: Default::default(),
                // Map windows never carry the user's style (T7.4b), but do carry the user's
                // own words: spelling help belongs wherever the transcript is (T12.15a).
                style: Default::default(),
                style_preset: Default::default(),
                vocabulary: self.summary_vocabulary.clone(),
                user_name: None,
            };
            let Ok(ranges) = time_chunks(&input, thread.seconds()) else {
                continue;
            };
            // A window nobody spoke in has nothing to map.
            let in_window = ranges.last().is_some_and(|r| {
                crate::live_summaries::window_of(&input, r, thread.seconds()) == k
            });
            if in_window {
                thread.send(Job {
                    window: k,
                    input,
                    ranges,
                });
            }
        }
    }

    /// Reports the windows the map worker finished since the last call.
    fn mapped_events(&self) {
        let Some(thread) = &self.summaries else {
            return;
        };
        for m in thread.drain() {
            self.emit(RuntimeEvent::LiveSummaryMapped {
                window: m.window,
                took: m.took,
                error: m.error,
            });
        }
    }

    /// Finishes every live worker (reference first, so the mic's last finals meet the far
    /// end's), then handles the held end events (the pipeline put each worker's
    /// final-pass entry in the request, `Pipeline::register_external_live`).
    fn finish_workers(&mut self) {
        // Speaker work stops first: the final pass does the tail (`speaker_thread` docs),
        // and the thread has until the workers finished to end.
        if let Some(s) = &self.speakers {
            s.request_stop();
        }
        // Summaries too: the summary after the meeting maps the windows not done. The
        // thread is told now and waited for after the workers, so its wait never delays
        // the live transcript (T7.4c).
        let mut summaries = self.summaries.take();
        if let Some(s) = summaries.as_mut() {
            s.request_stop();
        }
        let mut workers = std::mem::take(&mut self.workers);
        workers.sort_by_key(|w| w.track() == TrackId::Mic);
        // A chunker track far behind stops decoding instead of making the end wait (T6.10b);
        // speech stretches are the transcript and always finish (D20).
        let cap = earshot_core::audio::PIPELINE_RATE.frames_for(self.policy.max_stop_backlog);
        for w in workers.iter().filter(|w| w.has_partials()) {
            let backlog = w.backlog();
            if backlog > cap as u64 {
                w.cancel();
                self.warn(RuntimeWarning::LiveCutAtStop {
                    track: w.track(),
                    behind: earshot_core::audio::PIPELINE_RATE
                        .duration_of(usize::try_from(backlog).unwrap_or(usize::MAX)),
                });
            }
        }
        let logs: Vec<_> = workers
            .iter()
            .filter_map(|w| w.stretch_log().map(|log| (w.track(), log)))
            .collect();
        for w in workers {
            let track = w.track();
            let (events, switches, end) =
                w.finish_watched(self.policy.catch_up_stall, &self.catch_up);
            match end {
                worker::CatchUpEnd::Finished => {}
                worker::CatchUpEnd::Cancelled => self.stop_stretches(track, "catch-up cancelled"),
                worker::CatchUpEnd::Stalled => {
                    self.stop_stretches(track, "catch-up stalled");
                    let w = RuntimeWarning::LiveCatchUpStalled { track };
                    self.warn(w.clone());
                    self.problems.push(w);
                }
            }
            for p in switches {
                self.emit(RuntimeEvent::LivePartials {
                    track,
                    enabled: p.enabled,
                    behind: p.behind,
                });
            }
            self.handle(events);
        }
        let capture_ended = *self.capture_ended.get_or_insert_with(Instant::now);
        self.emit(RuntimeEvent::LiveTranscriptFinished {
            after_capture_end: capture_ended.elapsed(),
        });
        self.finish_previews();
        if let Some(s) = summaries {
            let finished = s.finish(self.policy.speaker_stop_wait);
            for m in finished.mapped {
                self.emit(RuntimeEvent::LiveSummaryMapped {
                    window: m.window,
                    took: m.took,
                    error: m.error,
                });
            }
            self.summary_record = finished.record;
            // It stopped by itself (no lower priority, a crash), perhaps before any window:
            // no record says so then.
            if let Some(reason) = finished.stopped_itself {
                self.emit(RuntimeEvent::LiveSummariesOff { reason });
            }
        }
        // What each stretch worker decoded, also when it failed or crashed (T6.13c).
        for (track, log) in logs {
            let log = log
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone();
            if let Some((_, r)) = self.stretch_records.iter_mut().find(|(t, _)| *t == track) {
                r.decoded = log.decoded;
                // The backend that proved itself on the last decodes (a per-call GPU
                // fallback reports the CPU), not the one reported when the session started.
                if let Some(backend) = log.backend {
                    r.backend = backend;
                }
            }
        }
        // The speaker windows computed during the meeting (T6.13d2): every finished batch
        // (a crash or a late stop loses only the batch in progress). A track whose
        // snapshot cannot be read gets no record; the final pass diarises it whole.
        let speakers = self
            .speakers
            .take()
            .map(|s| s.finish(self.policy.speaker_stop_wait))
            .unwrap_or_default();
        for (track, state) in speakers {
            let Some((results, stopped)) = state else {
                continue;
            };
            let threads = threads_of(&self.speaker_threads, track);
            self.speaker_records.push((
                track,
                SpeakerRecord {
                    header: SpeakerHeader {
                        params: results.params().clone(),
                        threads,
                        stopped,
                    },
                    results,
                },
            ));
        }
        for event in std::mem::take(&mut self.deferred) {
            self.handle(vec![event]);
        }
    }

    /// Marks `track`'s stretch record as stopped (the final pass transcribes what it did not
    /// decode).
    fn stop_stretches(&mut self, track: TrackId, why: &str) {
        if let Some((_, r)) = self.stretch_records.iter_mut().find(|(t, _)| *t == track) {
            r.stopped.get_or_insert_with(|| why.to_owned());
        }
    }

    fn record(&mut self, track: TrackId, samples: &[f32]) {
        let Some(writer) = self.writers[slot(track)].as_mut() else {
            return;
        };
        if let Err(e) = writer.write(samples) {
            self.recording_failed(track, &e);
        }
    }

    /// A write to `track`'s recording failed (T9.17): the track stops recording and its file
    /// is finished at once with the audio written before the error (a later crash then loses
    /// nothing more), the user hears it once, and when no track records any more the session
    /// stops the meeting itself.
    fn recording_failed(&mut self, track: TrackId, error: &earshot_store::StoreError) {
        let Some(writer) = self.writers[slot(track)].take() else {
            return;
        };
        self.recording_stopped[slot(track)] = true;
        let at = MeetingTime::from_pipeline_frames(writer.whole_frames());
        let disk_full = error.is_disk_full();
        // Nothing whole was written: there is no audio to keep, and an empty file would only
        // read as a recording to re-transcribe. The `.part` holds no frame, so removing it loses
        // nothing (a failed removal leaves it to the orphan sweep).
        let finished = if writer.whole_frames() == 0 {
            let _ = writer.abort();
            None
        } else {
            self.finalize(track, writer)
        };
        let stopped = RuntimeWarning::RecordingStopped {
            track,
            at,
            disk_full,
            error: error.to_string(),
        };
        self.warn(stopped.clone());
        self.problems.push(stopped);
        match finished {
            Some(Ok(file)) => self.audio_files.push((track, file.id)),
            Some(Err(e)) => {
                let w = RuntimeWarning::Recording {
                    track,
                    error: e.to_string(),
                };
                self.warn(w.clone());
                self.problems.push(w);
            }
            None => {}
        }
        let all = self
            .tracks
            .iter()
            .all(|t| self.recording_stopped[slot(t.kind)]);
        if all && !self.stop_self {
            self.stop_self = true;
            let w = RuntimeWarning::EveryRecordingStopped { disk_full };
            self.warn(w.clone());
            self.problems.push(w);
        }
    }

    /// Finishes `writer` (whole, or as far as it got after a failed write) into `track`'s
    /// `AudioFile` row; `None` when the session has no such track. The store is locked only for
    /// the track check and the row: the file work (header, sync, rename) runs without it.
    fn finalize(
        &mut self,
        track: TrackId,
        writer: FlacTrackWriter,
    ) -> Option<Result<earshot_core::model::AudioFile, earshot_store::StoreError>> {
        let row = self.tracks.iter().find(|t| t.kind == track)?.clone();
        #[cfg(test)]
        finalize_panics::check(&self.services.data, track);
        let provenance = AudioProvenance {
            pipeline_version: PIPELINE_VERSION.to_owned(),
            aec_version: (track == TrackId::Mic && self.capture.reference.is_some())
                .then(|| AEC_VERSION.to_owned()),
        };
        let clock = Arc::clone(&self.services.clock);
        let services = self.services.clone();
        Some(earshot_store::finalize_track_audio_outside_lock(
            writer,
            &self.services.data,
            &row,
            provenance,
            &*clock,
            &mut *self.rng,
            &mut |f| f(&mut services.store()),
        ))
    }

    fn store_live(&mut self, segment: earshot_core::asr::Segment) {
        if self.summaries.is_some()
            && let Some(track) = self.tracks.iter().find(|t| t.id == segment.track)
        {
            self.summary_rows.push(PromptSegment::from_segment(
                &segment,
                crate::view::track_label(track.kind),
            ));
        }
        let clock = Arc::clone(&self.services.clock);
        let result = {
            let mut db = self.services.store();
            let (store, hlc) = db.parts();
            let row = SegmentRow::new(self.meeting_id, TranscriptPass::Live, segment, hlc, &*clock);
            store.put_segment(row)
        };
        if let Err(e) = result {
            self.warn(RuntimeWarning::Persistence(e.to_string()));
        }
    }

    fn device_state(&mut self, track: TrackId, kind: &DiscontinuityKind) {
        // Only something to say: the device state did not change.
        if let DiscontinuityKind::Note {
            warning: EndpointWarning::CallOutputRefused { wanted, used },
        } = kind
        {
            self.warn(RuntimeWarning::CallOutputRefused {
                wanted: wanted.clone(),
                used: used.clone(),
            });
            return;
        }
        if let DiscontinuityKind::Note {
            warning: EndpointWarning::StoppedAnswering,
        } = kind
        {
            self.warn(RuntimeWarning::DeviceStoppedAnswering);
            self.problems.push(RuntimeWarning::DeviceStoppedAnswering);
            return;
        }
        let DiscontinuityKind::DeviceChanged {
            device_name,
            warning,
            ..
        } = kind
        else {
            return;
        };
        let was = std::mem::replace(&mut self.unavailable[slot(track)], device_name.is_none());
        // A chosen endpoint given up for the default (T7.4i) is reported as such, never as
        // the device being back; the default's own warning follows.
        let (gave_up, warning) = match warning {
            Some(EndpointWarning::ChosenEndpointGone {
                chosen,
                new_endpoint,
            }) => (Some(chosen), new_endpoint.as_deref()),
            other => (None, other.as_ref()),
        };
        match (device_name, gave_up) {
            (None, _) => self.warn(RuntimeWarning::DeviceUnavailable { track }),
            (Some(name), Some(chosen)) => self.warn(RuntimeWarning::ChosenDeviceGivenUp {
                track,
                chosen: chosen.clone(),
                used: name.clone(),
            }),
            (Some(name), None) if was => self.warn(RuntimeWarning::DeviceRestored {
                track,
                device_name: name.clone(),
            }),
            (Some(_), None) => {}
        }
        // A switch onto a Bluetooth hands-free mic (T6.24): the source decides when; a
        // warning on any other track is ignored, the loopback is never a mic.
        if let (
            TrackId::Mic,
            Some(name),
            Some(EndpointWarning::BluetoothHandsFree { sample_rate }),
        ) = (track, device_name, warning)
        {
            self.warn(RuntimeWarning::BluetoothHandsFreeMic {
                device_name: name.clone(),
                sample_rate: *sample_rate,
            });
        }
    }

    /// The clock re-sync policy (module docs).
    fn check_clocks(&mut self, stats: &PipelineStats) {
        let ms = |d: Duration| d.as_secs_f64() * 1000.0;
        let mic = stats.mic.skew_ms;
        let offset = stats.reference.as_ref().map(|r| r.skew_ms - mic);
        let due = mic.abs() > ms(self.policy.mic_skew_budget)
            || offset.is_some_and(|o| o.abs() > ms(self.policy.track_offset_budget));
        let cooled = self
            .last_resync
            .is_none_or(|last| stats.end.saturating_sub(last) >= self.policy.resync_cooldown);
        if !due || !cooled {
            return;
        }
        self.last_resync = Some(stats.end);
        let at = self
            .capture
            .device_time(Duration::from_millis(stats.end.as_millis()));
        for track in self.capture.tracks() {
            self.pipeline.discontinuity(
                track,
                Discontinuity {
                    at,
                    kind: DiscontinuityKind::DataLost,
                },
            );
        }
        self.emit(RuntimeEvent::Resynced {
            mic_skew_ms: mic,
            track_offset_ms: offset,
        });
    }

    /// Finalises every track's recording, then the meeting's duration (module docs), before the
    /// live work ends, and says so. From here on the meeting reads as being processed.
    fn save_recording(&mut self) {
        let Some(ended) = self.capture_end else {
            return;
        };
        let clock = Arc::clone(&self.services.clock);
        let kinds: Vec<TrackId> = self.tracks.iter().map(|t| t.kind).collect();
        for track in kinds {
            let Some(writer) = self.writers[slot(track)].take() else {
                continue;
            };
            match self.finalize(track, writer) {
                Some(Ok(file)) => self.audio_files.push((track, file.id)),
                Some(Err(e)) => self.problems.push(RuntimeWarning::Recording {
                    track,
                    error: e.to_string(),
                }),
                None => {}
            }
        }
        let mut db = self.services.store();
        // Re-read: only the duration changes, edits made during the meeting are kept.
        match db.meeting(self.meeting_id) {
            Ok(Some(mut meeting)) => {
                let (store, hlc) = db.parts();
                meeting.duration_ms = Some(ended.end.as_millis());
                meeting.meta.touch(hlc, &*clock);
                if let Err(e) = store.put_meeting(meeting) {
                    self.problems
                        .push(RuntimeWarning::Persistence(e.to_string()));
                }
            }
            Ok(None) => self.problems.push(RuntimeWarning::Persistence(
                "the meeting row disappeared during the session".to_owned(),
            )),
            Err(e) => self
                .problems
                .push(RuntimeWarning::Persistence(e.to_string())),
        }
        drop(db);
        // No gap in the claim: the session holds the meeting until its thread ends.
        if let Ok(lease) = self
            .services
            .lease_meeting_for(self.meeting_id, crate::MeetingJob::Processing)
        {
            self._lease = lease;
        }
        let lost = self.gaps.total();
        if !lost.is_zero() {
            self.problems
                .push(RuntimeWarning::RecordingGap { total: lost });
        }
        self.recording_saved = true;
        self.emit(RuntimeEvent::RecordingSaved {
            reason: ended.reason,
            problems: self.problems.clone(),
        });
    }

    /// What is left to do after a panic on the session thread (T9.19), whatever the session was
    /// doing: capture is stopped, every track still recording is finished as far as it got
    /// (T9.17's path) and the meeting gets its duration, [`RuntimeEvent::RecordingSaved`] goes
    /// out unless it already did, then [`RuntimeEvent::Ended`] with [`EndReason::Failed`]. Live
    /// work is abandoned (no records for the final pass: it transcribes the tracks whole). Never
    /// called on a normal end.
    fn after_panic(&mut self) {
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| self.source.stop()));
        // Where the recording got to: the pipeline's own end when it had one, else the longest
        // track taken by its writer.
        let taken = self
            .writers
            .iter()
            .flatten()
            .map(FlacTrackWriter::frames_taken)
            .max()
            .unwrap_or(0);
        let end = self
            .capture_end
            .map_or_else(|| MeetingTime::from_pipeline_frames(taken), |e| e.end);
        let failed = PipelineEnd {
            end,
            reason: EndReason::Failed,
        };
        if !self.recording_saved {
            self.capture_end = Some(failed);
            // A track whose file a panic left unfinished (in its finish) is said so: start-up
            // recovery saves it at the next start (T9.18).
            for t in self.tracks.iter().map(|t| t.kind).collect::<Vec<_>>() {
                let finished = self.audio_files.iter().any(|(k, _)| *k == t);
                if self.writers[slot(t)].is_none() && !finished && !self.recording_stopped[slot(t)]
                {
                    self.problems.push(RuntimeWarning::Recording {
                        track: t,
                        error: "the recording was not finished; it is saved at the next start"
                            .to_owned(),
                    });
                }
            }
            self.save_recording();
        }
        let final_pass = self.final_pass.clone().unwrap_or_else(|| FinalPassRequest {
            end,
            tracks: self
                .tracks
                .iter()
                .map(|t| earshot_core::pipeline::FinalPassTrack {
                    track: t.kind,
                    track_entity: Some(t.id),
                    options: earshot_core::asr::TranscribeOptions::batch(),
                })
                .collect(),
        });
        let outcome = LiveOutcome {
            meeting: self.meeting_id,
            tracks: self.tracks.iter().map(|t| (t.kind, t.id)).collect(),
            end,
            reason: EndReason::Failed,
            final_pass,
            audio_files: std::mem::take(&mut self.audio_files),
            unavailable_at_end: Vec::new(),
            problems: std::mem::take(&mut self.problems),
            echo_words_removed: self.echo.removed(),
            echo_locks: self.echo.locks().to_vec(),
        };
        self.emit(RuntimeEvent::Ended(outcome));
    }

    /// What is left at the end of the session, once the live work has finished: the records for
    /// the final pass (module docs).
    fn finish(&mut self) -> Result<LiveOutcome, RuntimeError> {
        let ended = self
            .ended
            .ok_or(RuntimeError::Internal("pipeline ended without Ended"))?;
        let final_pass = self.final_pass.clone().ok_or(RuntimeError::Internal(
            "pipeline ended without a final-pass request",
        ))?;
        let mut problems = std::mem::take(&mut self.problems);
        let audio_files = std::mem::take(&mut self.audio_files);
        let mut db = self.services.store();
        // Echo-cleaning provenance: the lock stretches the final pass will clean with, and
        // the live pass's report.
        let locks = self.echo.locks().to_vec();
        let report = crate::echo::EchoReport {
            locks: locks.clone(),
            text_removed: self.echo.removed(),
            ..crate::echo::EchoReport::default()
        };
        let stored =
            crate::echo::record_echo_locks(&mut db, self.meeting_id, &locks).and_then(|()| {
                crate::echo::record_echo_report(
                    &mut db,
                    self.meeting_id,
                    TranscriptPass::Live,
                    &report,
                )
            });
        if let Err(e) = stored {
            problems.push(RuntimeWarning::Persistence(e.to_string()));
        }
        // What the speech stretches cover, for the final pass to reuse (T6.13c). Without a
        // record the final pass transcribes the track whole.
        for (track, record) in &self.stretch_records {
            if let Err(e) =
                crate::stretches::record_stretches(&mut db, self.meeting_id, *track, record)
            {
                problems.push(RuntimeWarning::Persistence(e.to_string()));
            }
        }
        // The speaker windows computed during the meeting, for the final pass to finish
        // (T6.13d2). Without a record the final pass diarises the track whole.
        for (track, record) in &self.speaker_records {
            if let Err(e) =
                crate::speakers::record_speakers(&mut db, self.meeting_id, *track, record)
            {
                problems.push(RuntimeWarning::Persistence(e.to_string()));
            }
        }
        for (track, why) in &self.speakers_off_tracks {
            if let Err(e) =
                crate::speakers::record_speakers_off(&mut db, self.meeting_id, *track, why)
            {
                problems.push(RuntimeWarning::Persistence(e.to_string()));
            }
        }
        // The windows summarised during the meeting, for the summary after it (T8.3j).
        if let Some(record) = &self.summary_record
            && let Err(e) =
                crate::live_summaries::record_live_summaries(&mut db, self.meeting_id, record)
        {
            problems.push(RuntimeWarning::Persistence(e.to_string()));
        }
        drop(db);
        Ok(LiveOutcome {
            meeting: self.meeting_id,
            tracks: self.tracks.iter().map(|t| (t.kind, t.id)).collect(),
            end: ended.end,
            reason: ended.reason,
            final_pass,
            audio_files,
            unavailable_at_end: self
                .capture
                .tracks()
                .filter(|t| self.unavailable[slot(*t)])
                .collect(),
            problems,
            echo_words_removed: self.echo.removed(),
            echo_locks: self.echo.locks().to_vec(),
        })
    }
}

/// Panics injected by tests into a track's finish (T9.19 with T9.18), keyed by the data
/// directory so tests running at once never see each other's.
#[cfg(test)]
pub(crate) mod finalize_panics {
    use std::path::PathBuf;
    use std::sync::Mutex;

    use earshot_core::capture::TrackId;
    use earshot_store::DataDir;

    static PANICS: Mutex<Vec<(PathBuf, TrackId)>> = Mutex::new(Vec::new());

    /// The next session on `data` panics in `track`'s finish, its writer dropped unfinished.
    pub(crate) fn set(data: &DataDir, track: TrackId) {
        PANICS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push((data.root().to_owned(), track));
    }

    pub(super) fn check(data: &DataDir, track: TrackId) {
        let hit = {
            let mut p = PANICS
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            p.iter()
                .position(|(root, t)| root == data.root() && *t == track)
                .map(|i| p.remove(i))
        };
        assert!(hit.is_none(), "an invented bug in a track's finish");
    }
}

/// Write failures injected by tests into a session's FLAC writers (T9.17), keyed by the data
/// directory so tests running at once never see each other's.
#[cfg(test)]
pub(crate) mod write_faults {
    use std::path::PathBuf;
    use std::sync::Mutex;

    use earshot_core::capture::TrackId;
    use earshot_store::{DataDir, WriteFault};

    static FAULTS: Mutex<Vec<(PathBuf, TrackId, WriteFault)>> = Mutex::new(Vec::new());

    /// The next session on `data` gets `fault` on `track`'s writer.
    pub(crate) fn set(data: &DataDir, track: TrackId, fault: WriteFault) {
        let mut f = FAULTS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        f.push((data.root().to_owned(), track, fault));
    }

    pub(super) fn take(data: &DataDir, track: TrackId) -> Option<WriteFault> {
        let mut f = FAULTS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let i = f
            .iter()
            .position(|(root, t, _)| root == data.root() && *t == track)?;
        Some(f.remove(i).2)
    }
}
