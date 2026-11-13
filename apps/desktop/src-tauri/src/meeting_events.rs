//! What the meeting screen hears (T7.2): the runtime's [`RuntimeEvent`]s and the host's
//! [`HostStep`]s and [`HostOutcome`], mapped to one typed [`MeetingEvent`] stream for the UI.
//!
//! Pure functions and small state machines only (no threads, no clock of their own: the
//! level throttle is given the time). Transcript text appears only in
//! [`MeetingEvent::Landed`] (settled) and [`MeetingEvent::Provisional`] (a guess at the unfinished
//! stretch, never stored, T9.1m), which go to the local webview and nowhere else; every other event
//! carries counts, levels, states and fixed copy.

use std::time::{Duration, Instant};

use earshot_core::asr::{BackendInfo, BackendKind, TranscriptEvent};
use earshot_core::capture::{EndReason, TrackId};
use earshot_core::diar::SpeakerRole;
use earshot_core::pipeline::PipelineError;
use earshot_core::summary::SummaryV1;
use earshot_runtime::RuntimeError;
use earshot_runtime::host::{HostOutcome, HostPost, HostStep, SkipReason, SummaryResult};
use earshot_runtime::live::{
    HoldReason, LiveTranscriptState, PreviewOffReason, RuntimeEvent, RuntimeWarning,
};
use earshot_runtime::post::{PostSpeaker, PostStage};
use earshot_runtime::summary::{LocalSummaryError, SummaryRunReport};
use serde::Serialize;
use specta::Type;

/// What a meeting records.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, serde::Deserialize, Type)]
pub enum CaptureMode {
    /// A call: the mic and the computer's audio (two tracks, echo canceller).
    Call,
    /// In the room: the mic only.
    InRoom,
}

/// A live track as the meeting screen names it: speaker names exist only after hang-up.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Type)]
pub enum LiveTrack {
    /// The mic (after echo cancellation).
    You,
    /// The computer's audio.
    Them,
}

impl LiveTrack {
    /// The UI name of a runtime track.
    #[must_use]
    pub fn of(track: TrackId) -> Self {
        match track {
            TrackId::Mic => Self::You,
            TrackId::Reference => Self::Them,
        }
    }

    pub(crate) fn device(self) -> &'static str {
        match self {
            Self::You => "your microphone",
            Self::Them => "computer audio",
        }
    }

    pub(crate) fn key(self) -> &'static str {
        match self {
            Self::You => "you",
            Self::Them => "them",
        }
    }
}

/// Why the recording ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Type)]
pub enum EndedReason {
    /// Stopped on request.
    Stopped,
    /// The source ran out (a replay).
    Finished,
    /// Capture failed; what was captured until then is kept.
    Failed,
}

/// Which condition a warning banner is about (the UI picks the banner's look from it).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Type)]
pub enum WarningKind {
    /// The mic is a Bluetooth headset in hands-free mode.
    BluetoothHandsFree,
    /// A device is gone; silence is recorded.
    DeviceUnavailable,
    /// A device is back.
    DeviceRestored,
    /// A device chosen on the Audio page could not be used; the default is recorded.
    DeviceFallback,
    /// A track's live transcript stopped or fell behind.
    TrackBehind,
    /// Live transcription runs on a slower backend than the best one tried.
    Backend,
    /// Live speaker work does not run during the meeting.
    SpeakersOff,
    /// Summarising during the meeting is off or stopped; the summary is made after it.
    SummariesOff,
    /// A track's recording failed.
    Recording,
    /// Capture or audio timing trouble.
    Audio,
    /// Saving to the database failed.
    Storage,
    /// The recording stopped by itself because the call seemed to have ended.
    CallEnded,
    /// Provisional words are off or waiting (T12.33).
    ProvisionalWords,
}

/// A step of the work after the meeting.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Type)]
#[serde(tag = "kind")]
pub enum PostStep {
    /// The recordings are being read back.
    Loading,
    /// The final pass is starting (models loaded).
    Starting,
    /// A track's transcript was finished.
    Transcribed {
        /// The track.
        track: LiveTrack,
    },
    /// A track's speakers were found.
    Diarised {
        /// The track.
        track: LiveTrack,
    },
    /// Echo of the far end was cleaned from the mic transcript.
    EchoChecked,
    /// Speakers were named and the final transcript stored.
    Stored,
    /// The summary is being written.
    Summarising,
}

/// What the summary step did.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Type)]
#[serde(tag = "kind")]
pub enum SummaryEvent {
    /// A summary was stored.
    Stored {
        /// Its title.
        title: String,
        /// Action items.
        actions: u32,
        /// Decisions.
        decisions: u32,
        /// Voices found after the meeting that are not yet named.
        voices_to_name: u32,
    },
    /// No summary was made (plain copy).
    Skipped {
        /// Why.
        reason: String,
    },
    /// The summary failed; nothing was stored. The final transcript stays, so the screen offers
    /// Re-summarise.
    Failed {
        /// What failed (fixed copy, `plain_text::runtime_text`).
        message: String,
        /// Whether trying again can succeed without changing anything.
        retryable: bool,
    },
    /// The meeting ended in an error before its recording was saved (T9.23): no final pass or
    /// summary ran, and nothing here says what was kept, so the screen offers only Open.
    RecordingFailed {
        /// What happened (fixed copy, `plain_text::runtime_text`).
        message: String,
    },
    /// The final transcript could not be finished (T9.23), so no summary ran: the recording and
    /// any draft stay, the meeting reads as `Unprocessed`, and the screen offers Re-transcribe.
    PostFailed {
        /// Why (fixed copy, `plain_text::runtime_text`).
        message: String,
        /// Whether trying again can succeed without changing anything.
        retryable: bool,
    },
    /// The summary was cancelled; nothing was stored. A final transcript stays.
    Cancelled,
    /// The final pass was cancelled, so no summary ran either: the recording and any draft
    /// transcript stay and the meeting reads as `Unprocessed`.
    PostCancelled,
}

/// Where the live transcript is, for the banner at the top of the meeting's transcript.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Type)]
pub enum TranscriptState {
    /// Recording; the previous meeting's final pass or summary comes first.
    WaitingForPreviousMeeting,
    /// Recording; the app is still deciding what its models run on.
    WaitingForStart,
    /// Working through the audio queued so far.
    CatchingUp,
    /// Keeping up: no banner.
    Live,
    /// The live transcript stopped for this meeting; the full one is made after it.
    Stopped,
}

/// A word of [`MeetingEvent::Provisional`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Type)]
pub struct ProvisionalWord {
    /// Start in meeting time (ms).
    pub start_ms: u64,
    /// The word.
    pub text: String,
}

/// One event of the meeting screen, in order: `Started` first; `Ended` once the recording
/// is saved; then `PostProgress` steps; `Summary` last. A meeting's work goes on after a newer
/// meeting started: `Ended`, `PostProgress` and `Summary` say whose they are, and it sends no
/// other event from then on.
#[derive(Debug, Clone, PartialEq, Serialize, Type, tauri_specta::Event)]
#[serde(tag = "type")]
pub enum MeetingEvent {
    /// The meeting is recording.
    Started {
        /// Meeting id (hyphenated UUID).
        meeting: String,
        /// What it records.
        mode: CaptureMode,
        /// Its tracks (You first).
        tracks: Vec<LiveTrack>,
    },
    /// The backend that runs a track's transcription, as a short label ("GPU (CUDA)").
    Backend {
        /// The track.
        track: LiveTrack,
        /// The label.
        label: String,
    },
    /// Level meters, at most [`LEVELS_PER_SECOND`] a second.
    Levels {
        /// The mic after echo cancellation (dBFS, at least [`FLOOR_DBFS`]).
        you_dbfs: f32,
        /// The computer's audio (dBFS); `None` in the room.
        them_dbfs: Option<f32>,
    },
    /// Someone started or stopped speaking on a track.
    VoiceActivity {
        /// The track.
        track: LiveTrack,
        /// Whether someone speaks now.
        speaking: bool,
    },
    /// How far the transcript is behind the audio (the slowest track).
    Lag {
        /// Seconds of audio not yet transcribed.
        seconds: f64,
    },
    /// Words transcribed so far (both tracks).
    Words {
        /// The total.
        total: u32,
    },
    /// A stretch of transcript landed (finals only, never partials).
    Landed {
        /// The track.
        track: LiveTrack,
        /// Start in meeting time (ms).
        start_ms: u64,
        /// End in meeting time (ms): provisional words before it are covered by this text.
        end_ms: u64,
        /// The text.
        text: String,
    },
    /// What to show for the track's unfinished stretch, replacing what was shown (T9.1m). A guess:
    /// it is never stored, and [`MeetingEvent::Landed`] stays the only event for settled text.
    Provisional {
        /// The track.
        track: LiveTrack,
        /// The words, in time order; empty when none is left.
        words: Vec<ProvisionalWord>,
    },
    /// The live transcript changed state (T9.1l).
    Transcript {
        /// Its state.
        state: TranscriptState,
    },
    /// Something the user may need to know; recording continues. A later warning with the
    /// same `key` replaces the earlier one (a device that is back replaces "unavailable").
    Warning {
        /// The condition (one banner per key).
        key: String,
        /// What kind of condition.
        kind: WarningKind,
        /// Plain copy.
        message: String,
    },
    /// The condition of the banner `key` is over: the banner goes away (T12.33). Nothing happens
    /// when no such banner is shown.
    WarningCleared {
        /// The banner's key.
        key: String,
    },
    /// The question "the call seems to have ended: stop recording?" appears or goes away (T9.4).
    CallEndOffer {
        /// Whether it is being asked.
        open: bool,
    },
    /// Nobody answered that question in time, and the recording is being stopped.
    CallEndStopped,
    /// The recording is saved.
    Ended {
        /// The meeting.
        meeting: String,
        /// Why it ended.
        reason: EndedReason,
    },
    /// The work after the meeting progressed.
    PostProgress {
        /// The meeting.
        meeting: String,
        /// The step that just started or finished.
        step: PostStep,
        /// Final-pass steps done.
        done: u32,
        /// Final-pass steps in all, once known.
        total: Option<u32>,
    },
    /// The summary step's result. Always the last event of a meeting.
    Summary {
        /// The meeting.
        meeting: String,
        /// What happened.
        result: SummaryEvent,
    },
}

/// The most level events a second.
pub const LEVELS_PER_SECOND: u32 = 10;

/// The lowest level reported: silence (-inf dBFS) and anything quieter read as this.
pub const FLOOR_DBFS: f32 = -100.0;

/// A level fit for the UI (finite, in `FLOOR_DBFS..=0`; JSON has no infinity).
fn level(dbfs: f32) -> f32 {
    if dbfs.is_nan() {
        FLOOR_DBFS
    } else {
        dbfs.clamp(FLOOR_DBFS, 0.0)
    }
}

/// A short label for the backend that runs: "CPU", "GPU (CUDA)", "GPU (Vulkan)", "NPU".
#[must_use]
pub fn backend_label(kind: BackendKind) -> &'static str {
    match kind {
        BackendKind::Cpu => "CPU",
        BackendKind::Cuda => "GPU (CUDA)",
        BackendKind::Vulkan => "GPU (Vulkan)",
        BackendKind::Qnn => "NPU",
        BackendKind::Fake => "Test backend",
    }
}

fn backend_info_label(info: &BackendInfo) -> String {
    backend_label(info.kind).to_owned()
}

/// Lets an event through at most once per `min_gap`.
#[derive(Debug, Clone)]
pub struct Throttle {
    min_gap: Duration,
    last: Option<Instant>,
}

impl Throttle {
    /// A throttle for `per_second` events a second.
    #[must_use]
    pub fn per_second(per_second: u32) -> Self {
        Self {
            min_gap: Duration::from_secs(1) / per_second.max(1),
            last: None,
        }
    }

    /// Whether an event at `now` goes through (and is counted).
    pub fn allow(&mut self, now: Instant) -> bool {
        match self.last {
            Some(last) if now.saturating_duration_since(last) < self.min_gap => false,
            _ => {
                self.last = Some(now);
                true
            }
        }
    }
}

/// Maps a live session's events, keeping the counts the UI shows.
#[derive(Debug)]
pub struct EventMapper {
    mode: CaptureMode,
    /// The meeting, from `Started`.
    meeting: String,
    /// The problems told with the save, which `Ended`'s outcome holds too.
    told: Vec<RuntimeWarning>,
    words: u32,
    lag: [Option<Duration>; 2],
    levels: Throttle,
    /// What each track last said of its provisional words (`None`: nothing, so nobody asked for
    /// them there; `Some(None)`: they run).
    previews: [Option<Option<PreviewOffReason>>; 2],
    /// The provisional-words banner now shown.
    preview_banner: Option<String>,
}

fn slot(track: TrackId) -> usize {
    match track {
        TrackId::Mic => 0,
        TrackId::Reference => 1,
    }
}

impl EventMapper {
    /// A mapper for a meeting in `mode`.
    #[must_use]
    pub fn new(mode: CaptureMode) -> Self {
        Self {
            mode,
            meeting: String::new(),
            told: Vec::new(),
            words: 0,
            lag: [None, None],
            levels: Throttle::per_second(LEVELS_PER_SECOND),
            previews: [None, None],
            preview_banner: None,
        }
    }

    /// Words of the finals seen so far.
    #[cfg(test)]
    #[must_use]
    pub fn words(&self) -> u32 {
        self.words
    }

    /// The UI events for `event`, received at `now` (for the level throttle).
    pub fn map(&mut self, event: &RuntimeEvent, now: Instant) -> Vec<MeetingEvent> {
        match event {
            RuntimeEvent::Started { meeting, tracks } => {
                self.meeting = meeting.to_string();
                vec![MeetingEvent::Started {
                    meeting: meeting.to_string(),
                    mode: self.mode,
                    tracks: tracks.iter().map(|(t, _)| LiveTrack::of(*t)).collect(),
                }]
            }
            RuntimeEvent::LiveBackend { track, backend, .. } => vec![MeetingEvent::Backend {
                track: LiveTrack::of(*track),
                label: backend_info_label(backend),
            }],
            RuntimeEvent::Levels(l) => {
                if self.levels.allow(now) {
                    vec![MeetingEvent::Levels {
                        you_dbfs: level(l.cleaned_dbfs),
                        them_dbfs: l.reference_dbfs.map(level),
                    }]
                } else {
                    Vec::new()
                }
            }
            RuntimeEvent::VoiceActivity {
                track, speaking, ..
            } => vec![MeetingEvent::VoiceActivity {
                track: LiveTrack::of(*track),
                speaking: *speaking,
            }],
            RuntimeEvent::TranscriptLag { track, behind } => {
                self.lag[slot(*track)] = Some(*behind);
                let worst = self.lag.iter().flatten().max().copied().unwrap_or_default();
                vec![MeetingEvent::Lag {
                    seconds: worst.as_secs_f64(),
                }]
            }
            RuntimeEvent::Transcript {
                track,
                event: TranscriptEvent::Final(seg),
            } => {
                let count = if seg.words.is_empty() {
                    seg.text.split_whitespace().count()
                } else {
                    seg.words.len()
                };
                self.words = self
                    .words
                    .saturating_add(u32::try_from(count).unwrap_or(u32::MAX));
                vec![
                    MeetingEvent::Landed {
                        track: LiveTrack::of(*track),
                        start_ms: seg.start.as_millis(),
                        end_ms: seg.end.as_millis(),
                        text: seg.text.clone(),
                    },
                    MeetingEvent::Words { total: self.words },
                ]
            }
            RuntimeEvent::LivePreview { track, words } => vec![MeetingEvent::Provisional {
                track: LiveTrack::of(*track),
                words: words
                    .iter()
                    .map(|w| ProvisionalWord {
                        start_ms: w.start.as_millis(),
                        text: w.text.clone(),
                    })
                    .collect(),
            }],
            RuntimeEvent::LivePreviewState { track, off } => {
                self.previews[slot(*track)] = Some(*off);
                let banner = provisional_words_banner(self.previews);
                if banner == self.preview_banner {
                    return Vec::new();
                }
                self.preview_banner.clone_from(&banner);
                vec![match banner {
                    Some(message) => MeetingEvent::Warning {
                        key: PROVISIONAL_WORDS_KEY.into(),
                        kind: WarningKind::ProvisionalWords,
                        message,
                    },
                    None => MeetingEvent::WarningCleared {
                        key: PROVISIONAL_WORDS_KEY.into(),
                    },
                }]
            }
            RuntimeEvent::LiveSpeakersOff { reason } => vec![MeetingEvent::Warning {
                key: "speakers".into(),
                kind: WarningKind::SpeakersOff,
                message: format!("Speakers are found after the meeting ({reason})."),
            }],
            RuntimeEvent::LiveSummariesOff { reason } => vec![MeetingEvent::Warning {
                key: "live-summaries".into(),
                kind: WarningKind::SummariesOff,
                message: format!("{}.", sentence_start(reason.trim_end_matches('.'))),
            }],
            RuntimeEvent::Warning(w) => warning_event(w).into_iter().collect(),
            RuntimeEvent::LiveTranscript(state) => vec![MeetingEvent::Transcript {
                state: match state {
                    LiveTranscriptState::Held(HoldReason::PreviousMeeting) => {
                        TranscriptState::WaitingForPreviousMeeting
                    }
                    LiveTranscriptState::Held(HoldReason::Starting) => {
                        TranscriptState::WaitingForStart
                    }
                    LiveTranscriptState::CatchingUp => TranscriptState::CatchingUp,
                    LiveTranscriptState::UpToDate => TranscriptState::Live,
                    LiveTranscriptState::Stopped => TranscriptState::Stopped,
                },
            }],
            // The stop never waits for the rest of a live transcript to decode.
            RuntimeEvent::RecordingSaved { reason, problems } => {
                self.told.clone_from(problems);
                let is_gap = |w: &&RuntimeWarning| matches!(w, RuntimeWarning::RecordingGap { .. });
                let mut out: Vec<MeetingEvent> = problems
                    .iter()
                    .filter(|w| !is_gap(w))
                    .filter_map(warning_event)
                    .collect();
                out.push(MeetingEvent::Ended {
                    meeting: self.meeting.clone(),
                    reason: match reason {
                        EndReason::Stopped => EndedReason::Stopped,
                        EndReason::Finished => EndedReason::Finished,
                        EndReason::Failed => EndedReason::Failed,
                    },
                });
                // After `Ended`: the screen after a meeting shows only what follows it, and the
                // person who was not watching during the meeting still has to hear this.
                out.extend(problems.iter().filter(is_gap).filter_map(warning_event));
                out
            }
            // The outcome is the whole record: what the save already said is not said again.
            RuntimeEvent::Ended(o) => o
                .problems
                .iter()
                .filter(|p| !self.told.contains(p))
                .filter_map(warning_event)
                .collect(),
            // Partials and discarded segments never reach the screen (D20 shows what
            // landed); stats, re-syncs and partial switches are diagnostics; device
            // discontinuities arrive as warnings (unavailable, restored, hands-free).
            RuntimeEvent::Transcript { .. }
            | RuntimeEvent::Stats(_)
            | RuntimeEvent::Discontinuity { .. }
            | RuntimeEvent::Resynced { .. }
            | RuntimeEvent::LivePartials { .. }
            | RuntimeEvent::LiveTranscriptFinished { .. }
            // Counts for a report; `LivePreviewState` said why there were none, as it happened.
            | RuntimeEvent::LivePreviewReport(_)
            // A mapped window is reported by the summary after the meeting.
            | RuntimeEvent::LiveSummaryMapped { .. } => Vec::new(),
        }
    }
}

/// The one banner key for provisional words that are off or waiting (T12.33).
pub const PROVISIONAL_WORDS_KEY: &str = "provisional-words";

/// The words of the provisional-words banner (T12.33) for what each track last said (mic, then
/// the computer's audio), `None` when there is nothing to say: fixed copy by kind
/// ([`crate::plain_text::provisional_words_copy`]). Both tracks off for one reason is one
/// sentence about all of them; otherwise each track's sentence says whose words it is about,
/// other people first. A refusal of the user's own voice says other people's words are shown only
/// while their track says so; while theirs are off for a reason the banner gives, it is said
/// alone, and before their track has said anything it is not said yet. No sentence contradicts
/// another.
#[must_use]
pub fn provisional_words_banner(said: [Option<Option<PreviewOffReason>>; 2]) -> Option<String> {
    use crate::plain_text::{PreviewScope, own_voice_alone_copy, provisional_words_copy};
    let [you, them] = said;
    type Said = Option<Option<PreviewOffReason>>;
    let copy = |reason: Said, other: Said, scope| {
        let reason = reason??;
        // A track nobody asked provisional words for takes no part: the sentence is about all.
        let scope = if other.is_none() || other == Some(Some(reason)) {
            PreviewScope::All
        } else {
            scope
        };
        provisional_words_copy(reason, scope)
    };
    let them_sentence = copy(them, you, PreviewScope::Them);
    let you_sentence = match you {
        Some(Some(reason)) if own_voice_alone_copy(reason).is_some() => match them {
            // Other people's words are shown.
            Some(None) => provisional_words_copy(reason, PreviewScope::You),
            // Theirs are off and the banner says why: the user's own voice alone.
            Some(Some(_)) if them_sentence.is_some() => {
                own_voice_alone_copy(reason).map(str::to_owned)
            }
            // Not heard from yet, or off for a reason that needs no word: nothing yet.
            _ => None,
        },
        _ => copy(you, them, PreviewScope::You),
    };
    let mut sentences: Vec<String> = Vec::new();
    for sentence in [them_sentence, you_sentence].into_iter().flatten() {
        if !sentences.contains(&sentence) {
            sentences.push(sentence);
        }
    }
    (!sentences.is_empty()).then(|| sentences.join(" "))
}

/// A recording gap in words: whole seconds under a minute, then minutes and seconds.
fn gap_length(total: Duration) -> String {
    let secs = total.as_secs_f64().round() as u64;
    match (secs / 60, secs % 60) {
        (0, 1) => "about 1 second".to_owned(),
        (0, s) => format!("about {s} seconds"),
        (1, 0) => "about 1 minute".to_owned(),
        (m, 0) => format!("about {m} minutes"),
        (1, s) => format!("about 1 minute {s} seconds"),
        (m, s) => format!("about {m} minutes {s} seconds"),
    }
}

/// A time into the meeting as a clock reads it: `m:ss`, or `h:mm:ss` from an hour.
fn meeting_clock(ms: u64) -> String {
    let s = ms / 1000;
    let (h, m, s) = (s / 3600, s / 60 % 60, s % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
}

/// The banner key of [`RuntimeWarning::EveryRecordingStopped`]; the shell notifies on it
/// whether or not the window is watched (T9.17).
pub const RECORDING_STOPPED_KEY: &str = "recording-stopped";

/// What the user is told when every track stopped recording and the meeting stopped itself
/// (T9.17): fixed copy, also the notification's body.
#[must_use]
pub fn every_recording_stopped_message(disk_full: bool) -> &'static str {
    if disk_full {
        "Earshot stopped this meeting because the disk is full. What was recorded before that is \
         kept. Free some space before the next meeting."
    } else {
        "Earshot stopped this meeting because it could not save any more audio. What was \
         recorded before that is kept."
    }
}

/// The banner after a recording stopped by itself (T9.4): the user may want to know it was cut.
#[must_use]
pub fn call_end_stopped_warning() -> MeetingEvent {
    MeetingEvent::Warning {
        key: "call-end-stopped".into(),
        kind: WarningKind::CallEnded,
        message: "Earshot stopped this recording because the call seemed to have ended and nobody \
                  answered."
            .into(),
    }
}

fn warning(key: String, kind: WarningKind, message: String) -> Option<MeetingEvent> {
    Some(MeetingEvent::Warning { key, kind, message })
}

pub(crate) fn sentence_start(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        Some(f) => f.to_uppercase().chain(c).collect(),
        None => String::new(),
    }
}

/// What a track records from the default device `used`, and the device's kind.
fn default_recorded(t: LiveTrack, used: &str) -> (&'static str, String) {
    match t {
        LiveTrack::You => ("microphone", format!("the default microphone ({used})")),
        LiveTrack::Them => (
            "output",
            format!("computer audio from the default output ({used})"),
        ),
    }
}

/// The chosen device's name in a sentence ("The chosen microphone" when it has none).
fn chosen_label(what: &str, chosen: &str) -> String {
    if chosen.trim().is_empty() {
        format!("The chosen {what}")
    } else {
        chosen.to_owned()
    }
}

/// The banner text of a chosen device (T7.4i) given up for the default during the meeting.
fn chosen_device_given_up_message(t: LiveTrack, chosen: &str, used: &str) -> String {
    let (what, recorded) = default_recorded(t, used);
    format!(
        "{} is still not connected, so Earshot records {recorded} for the rest of this \
         meeting.",
        chosen_label(what, chosen)
    )
}

/// The banner text of a chosen device (T7.4i) replaced by the default at the start.
fn chosen_device_message(t: LiveTrack, chosen: &str, used: &str, error: Option<&str>) -> String {
    let (what, recorded) = default_recorded(t, used);
    let name = chosen_label(what, chosen);
    let why = match error {
        None => "is not connected".to_owned(),
        Some(e) => format!("could not be opened ({e})"),
    };
    format!(
        "{name} {why}, so Earshot records {recorded}. You can choose another {what} in \
         Settings, Audio."
    )
}

/// The banner for a runtime warning (`None`: nothing the user needs to see).
#[must_use]
pub fn warning_event(w: &RuntimeWarning) -> Option<MeetingEvent> {
    match w {
        RuntimeWarning::BluetoothHandsFreeMic { device_name, .. } => warning(
            "bluetooth".into(),
            WarningKind::BluetoothHandsFree,
            format!(
                "{device_name} is a Bluetooth headset in hands-free mode, so Windows records \
                 it at phone quality. Use the computer's own mic and keep the headset for \
                 listening."
            ),
        ),
        RuntimeWarning::FarEndSilent => warning(
            "far-end".into(),
            WarningKind::Audio,
            "Earshot hears your microphone but nothing from this computer's sound output, so \
             other people on a call may only be recorded through the microphone. Check that the \
             call app plays through the output in Settings, Audio. In a room with no call, you \
             can ignore this."
                .into(),
        ),
        // The same key as the warning above, which it replaces.
        RuntimeWarning::FarEndHeard => warning(
            "far-end".into(),
            WarningKind::DeviceRestored,
            "Earshot now hears this computer's sound output.".into(),
        ),
        // One banner, with the total so far.
        RuntimeWarning::RecordingGap { total } => warning(
            "recording-gap".into(),
            WarningKind::Audio,
            format!(
                "This recording has gaps totalling {}, and speech in them is lost. The microphone \
                 stopped sending sound, which also happens when the computer goes to sleep: keep \
                 the lid open and leave it switched on while recording.",
                gap_length(*total)
            ),
        ),
        RuntimeWarning::ChosenDeviceUnavailable {
            track,
            chosen,
            used,
            error,
        } => {
            let t = LiveTrack::of(*track);
            warning(
                format!("chosen-device:{}", t.key()),
                WarningKind::DeviceFallback,
                chosen_device_message(t, chosen, used, error.as_deref()),
            )
        }
        // The same key as the loss's "unavailable" banner, which it replaces.
        RuntimeWarning::ChosenDeviceGivenUp {
            track,
            chosen,
            used,
        } => {
            let t = LiveTrack::of(*track);
            warning(
                format!("device:{}", t.key()),
                WarningKind::DeviceFallback,
                chosen_device_given_up_message(t, chosen, used),
            )
        }
        RuntimeWarning::CallOutputRefused { wanted, used } => warning(
            "call-output".into(),
            WarningKind::DeviceFallback,
            match used {
                Some(used) => format!(
                    "The call plays on {wanted}, which Earshot could not open, so it goes on \
                     recording computer audio from {used}."
                ),
                None => format!("The call plays on {wanted}, which Earshot could not open."),
            },
        ),
        RuntimeWarning::DeviceUnavailable { track } => {
            let t = LiveTrack::of(*track);
            warning(
                format!("device:{}", t.key()),
                WarningKind::DeviceUnavailable,
                format!(
                    "{} is unavailable. Earshot records silence until it is back.",
                    sentence_start(t.device())
                ),
            )
        }
        RuntimeWarning::DeviceRestored { track, device_name } => {
            let t = LiveTrack::of(*track);
            warning(
                format!("device:{}", t.key()),
                WarningKind::DeviceRestored,
                format!("{} is back ({device_name}).", sentence_start(t.device())),
            )
        }
        // The live transcript's own banner says it stopped; a final pass that cannot run either
        // says why in its result.
        RuntimeWarning::LiveLoadFailed { .. } => None,
        RuntimeWarning::HostNote { key, message } => {
            warning(key.clone(), WarningKind::Backend, message.clone())
        }
        RuntimeWarning::Pipeline(PipelineError::LiveAsr { track, .. }) => {
            let t = LiveTrack::of(*track);
            warning(
                format!("behind:{}", t.key()),
                WarningKind::TrackBehind,
                format!(
                    "The live transcript for {} stopped. Earshot transcribes the rest after \
                     the meeting.",
                    t.device()
                ),
            )
        }
        RuntimeWarning::LiveCutAtStop { track, behind } => {
            let t = LiveTrack::of(*track);
            warning(
                format!("behind:{}", t.key()),
                WarningKind::TrackBehind,
                format!(
                    "The live transcript for {} stopped {:.0} s behind. Earshot transcribes \
                     the rest after the meeting.",
                    t.device(),
                    behind.as_secs_f64()
                ),
            )
        }
        RuntimeWarning::Pipeline(e) => warning(
            "audio".into(),
            WarningKind::Audio,
            format!("Earshot repaired a problem in the audio ({e})."),
        ),
        RuntimeWarning::Capture(m) => warning(
            "audio".into(),
            WarningKind::Audio,
            format!("Audio capture reported a problem: {m}"),
        ),
        RuntimeWarning::CaptureRepeated { message, times } => warning(
            "audio".into(),
            WarningKind::Audio,
            format!("Audio capture reported a problem: {message} (repeated {times} more times)"),
        ),
        // Fixed copy (T9.23): the error can name a local file. Its file is left for start-up
        // recovery (T9.18), which finishes it at the next start.
        RuntimeWarning::Recording { track, error: _ } => {
            let t = LiveTrack::of(*track);
            warning(
                format!("recording:{}", t.key()),
                WarningKind::Recording,
                format!(
                    "Earshot could not finish saving the recording of {}. What was recorded is \
                     kept and finished the next time Earshot starts.",
                    t.device()
                ),
            )
        }
        // The same key as the failure above: a file that could not even be finished replaces it.
        RuntimeWarning::RecordingStopped {
            track,
            at,
            disk_full,
            error: _,
        } => {
            let t = LiveTrack::of(*track);
            // Fixed copy by kind (T9.23): the error can name a local file.
            let why = if *disk_full {
                "the disk is full"
            } else {
                "Earshot could not save it"
            };
            warning(
                format!("recording:{}", t.key()),
                WarningKind::Recording,
                format!(
                    "The recording of {} stopped at {} because {why}. What was recorded before \
                     that is kept.",
                    t.device(),
                    meeting_clock(at.as_millis())
                ),
            )
        }
        RuntimeWarning::EveryRecordingStopped { disk_full } => warning(
            RECORDING_STOPPED_KEY.into(),
            WarningKind::Recording,
            every_recording_stopped_message(*disk_full).into(),
        ),
        RuntimeWarning::LiveCatchUpStalled { track } => {
            let t = LiveTrack::of(*track);
            warning(
                format!("behind:{}", t.key()),
                WarningKind::TrackBehind,
                format!(
                    "The live transcript for {} stopped answering after the recording, so Earshot \
                     left this meeting unprocessed. The recording is kept.",
                    t.device()
                ),
            )
        }
        RuntimeWarning::LiveLoadStalled => warning(
            "live-load-stalled".into(),
            WarningKind::Backend,
            "The live transcription model did not finish loading after the recording, so Earshot \
             left this meeting unprocessed. The recording is kept."
                .into(),
        ),
        RuntimeWarning::DeviceStoppedAnswering => warning(
            "device-stopped-answering".into(),
            WarningKind::Audio,
            "The audio device stopped answering when the recording was stopped, so Earshot saved \
             the recording without waiting for it. Everything recorded before that is kept."
                .into(),
        ),
        // The same key as the loss's "unavailable" banner, which it replaces: it is not coming back.
        RuntimeWarning::TrackEnded { track } => {
            let t = LiveTrack::of(*track);
            warning(
                format!("device:{}", t.key()),
                WarningKind::DeviceUnavailable,
                format!(
                    "{} kept failing, so Earshot stopped recording it for this meeting. What was \
                     recorded before is kept.",
                    sentence_start(t.device())
                ),
            )
        }
        RuntimeWarning::Persistence(m) => warning(
            "storage".into(),
            WarningKind::Storage,
            format!("Saving to the database failed: {m}"),
        ),
        RuntimeWarning::BackendFallback {
            track,
            backend,
            refused,
        } => {
            let t = LiveTrack::of(*track);
            let mut tried: Vec<&str> = refused.iter().map(|(k, _)| backend_label(*k)).collect();
            tried.dedup();
            let message = if tried.is_empty() {
                format!(
                    "Live transcription for {} runs on the {}.",
                    t.device(),
                    backend_label(*backend)
                )
            } else {
                format!(
                    "Live transcription for {} runs on the {} because the {} could not be used.",
                    t.device(),
                    backend_label(*backend),
                    tried.join(" or ")
                )
            };
            warning(
                format!("backend:{}", t.key()),
                WarningKind::Backend,
                message,
            )
        }
    }
}

/// Maps the host's post-meeting progress.
#[derive(Debug, Default)]
pub struct PostProgressMapper {
    meeting: String,
    done: u32,
    total: Option<u32>,
}

impl PostProgressMapper {
    /// A mapper for `meeting`.
    #[must_use]
    pub fn of(meeting: String) -> Self {
        Self {
            meeting,
            ..Self::default()
        }
    }

    /// The UI event for `step` (`None` for steps the UI does not show).
    pub fn step(&mut self, step: &HostStep<'_>) -> Option<MeetingEvent> {
        let shown = match step {
            HostStep::PostLoaded { .. } => PostStep::Loading,
            HostStep::PostStarting { work, .. } => {
                // Per track one transcription and one diarisation step, then echo and store.
                let tracks = u32::try_from(work.len()).unwrap_or(u32::MAX);
                self.total = Some(tracks.saturating_mul(2).saturating_add(2));
                PostStep::Starting
            }
            HostStep::Post(stage) => {
                self.done = self.done.saturating_add(1);
                if let Some(t) = self.total {
                    self.total = Some(t.max(self.done));
                }
                match stage {
                    PostStage::Transcribed { track, .. } => PostStep::Transcribed {
                        track: LiveTrack::of(*track),
                    },
                    PostStage::Diarised { track, .. } => PostStep::Diarised {
                        track: LiveTrack::of(*track),
                    },
                    PostStage::EchoChecked { .. } => PostStep::EchoChecked,
                    PostStage::Stored { .. } => PostStep::Stored,
                }
            }
            HostStep::PostFinished(_) => return None,
            HostStep::SummaryStarting => PostStep::Summarising,
        };
        Some(MeetingEvent::PostProgress {
            meeting: self.meeting.clone(),
            step: shown,
            done: self.done,
            total: self.total,
        })
    }
}

/// Whether a speaker still needs a name: not the user, not linked to a person and not
/// ignored by the user (T12.19, `earshot_runtime::edit::is_ignored`). The one rule behind
/// the summary-ready count, the meeting detail and a re-summary's count.
#[must_use]
pub fn needs_name(role: SpeakerRole, has_person: bool, ignored: bool) -> bool {
    role != SpeakerRole::Me && !has_person && !ignored
}

/// Speakers found after the meeting that still need a name ([`needs_name`]).
#[must_use]
pub fn voices_to_name(speakers: &[PostSpeaker]) -> u32 {
    let n = speakers
        .iter()
        .filter(|s| needs_name(s.role, s.person.is_some(), s.ignored))
        .count();
    u32::try_from(n).unwrap_or(u32::MAX)
}

fn count(n: usize) -> u32 {
    u32::try_from(n).unwrap_or(u32::MAX)
}

/// The "summary ready" event of a stored summary.
#[must_use]
pub fn stored_event(summary: &SummaryV1, voices_to_name: u32) -> SummaryEvent {
    SummaryEvent::Stored {
        title: summary.title.clone(),
        actions: count(summary.action_items.len()),
        decisions: count(summary.decisions.len()),
        voices_to_name,
    }
}

/// Plain copy for a skipped summary.
#[must_use]
pub fn skip_copy(reason: &SkipReason) -> String {
    match reason {
        SkipReason::NotRequested => "Summaries are turned off.".into(),
        // `why` is the runtime's developer text, for the CLI.
        SkipReason::NotInstalled { .. } => {
            "No summary model is installed on this computer. You can download one in Settings, \
             Summaries."
                .into()
        }
        SkipReason::ChosenNotInstalled { model } => crate::plain_text::chosen_summary_model(model),
        SkipReason::PostFailed => "The cleanup after the meeting failed.".into(),
        SkipReason::NoApiKey(e) => no_key_copy(e, false),
        SkipReason::ClaudeCodeUnavailable(e) => claude_code_copy(e, false),
        SkipReason::CodexUnavailable(e) => codex_copy(e, false),
        SkipReason::KeptLocalNoModel { model } => {
            crate::plain_text::kept_local_no_model(model, false)
        }
    }
}

/// Why a summary through Claude Code does not run (T7.4ga), and what to do: `ahead` for the
/// banner at the start ("will not run"), else for the summary result ("did not run").
#[must_use]
pub fn claude_code_copy(e: &earshot_runtime::claude_code::ClaudeCodeError, ahead: bool) -> String {
    use earshot_runtime::claude_code::ClaudeCodeError;
    let lead = if ahead {
        "The summary will not run"
    } else {
        "The summary did not run"
    };
    let fix = match e {
        ClaudeCodeError::NotInstalled => {
            "Install Claude Code and sign in, then re-summarise this meeting."
        }
        ClaudeCodeError::NotSignedIn | ClaudeCodeError::SignInExpired => {
            "Open Claude Code and sign in, then re-summarise this meeting."
        }
        _ => "Check Claude Code, then re-summarise this meeting.",
    };
    format!("{lead}: {e}. {fix}")
}

/// The warning that Earshot could not store its local record of a handover to Claude Code
/// (key `summary-handover-record`; `why` is the store's error, never meeting text).
#[must_use]
pub fn handover_record_warning(why: &str) -> MeetingEvent {
    MeetingEvent::Warning {
        key: "summary-handover-record".into(),
        kind: WarningKind::Backend,
        message: handover_record_copy(why),
    }
}

/// Claude Code wrote the summary at its own default effort although one was chosen: its help
/// could not be read, so Earshot could not tell whether it takes the choice (T12.20). Shown as
/// banner `summary-effort` after a meeting and as a re-summary job's warning.
pub const CLAUDE_EFFORT_UNREAD: &str = "Claude Code wrote this summary at its own default effort: Earshot could not read whether this Claude Code takes the effort you chose.";

/// The banner of [`CLAUDE_EFFORT_UNREAD`].
#[must_use]
pub fn claude_effort_unread_warning() -> MeetingEvent {
    MeetingEvent::Warning {
        key: "summary-effort".into(),
        kind: WarningKind::Backend,
        message: CLAUDE_EFFORT_UNREAD.into(),
    }
}

/// The copy of [`handover_record_warning`] (also a re-summary job's warning).
#[must_use]
pub fn handover_record_copy(why: &str) -> String {
    format!("Earshot could not store its record of what it handed to Claude Code or Codex ({why}).")
}

/// Why a summary through Codex does not run (T7.4gc), and what to do: `ahead` for the banner
/// at the start ("will not run"), else for the summary result ("did not run").
#[must_use]
pub fn codex_copy(e: &earshot_runtime::codex::CodexError, ahead: bool) -> String {
    use earshot_runtime::codex::CodexError;
    let lead = if ahead {
        "The summary will not run"
    } else {
        "The summary did not run"
    };
    let fix = match e {
        CodexError::NotInstalled => "Install Codex and sign in, then re-summarise this meeting.",
        CodexError::NotSignedIn | CodexError::SignInExpired => {
            "Sign in to Codex (run codex login), then re-summarise this meeting."
        }
        CodexError::AccessDenied => {
            "Choose another way to write summaries on the Summaries page, or ask your ChatGPT workspace administrator to allow Codex."
        }
        CodexError::UnsupportedVersion => "Check that Earshot supports your Codex version.",
        CodexError::ToolRan => {
            "Earshot does not let Codex run commands or tools; re-summarise this meeting, and if it happens again choose another way to write summaries."
        }
        _ => "Check Codex, then re-summarise this meeting.",
    };
    format!("{lead}: {e}. {fix}")
}

/// The banner at the start of a meeting whose summary will not run because Codex is not
/// installed or not signed in (key `summary-codex`), `None` for any other skip.
#[must_use]
pub fn codex_banner(reason: &SkipReason) -> Option<MeetingEvent> {
    match reason {
        SkipReason::CodexUnavailable(e) => Some(MeetingEvent::Warning {
            key: "summary-codex".into(),
            kind: WarningKind::Backend,
            message: codex_copy(e, true),
        }),
        _ => None,
    }
}

/// The banner at the start of a meeting whose summary will not run because the summary model
/// chosen in Settings is not downloaded (key `summary-model`), `None` for any other skip.
#[must_use]
pub fn chosen_model_banner(reason: &SkipReason) -> Option<MeetingEvent> {
    match reason {
        SkipReason::ChosenNotInstalled { model } => Some(MeetingEvent::Warning {
            key: "summary-model".into(),
            kind: WarningKind::Backend,
            message: format!(
                "The summary will not run. {}",
                crate::plain_text::chosen_summary_model(model)
            ),
        }),
        _ => None,
    }
}

/// The banner at the start of a meeting kept on this computer (T12.13) whose summary will not
/// run because no summary model here is downloaded (key `summary-kept-local`), `None` for any
/// other skip.
#[must_use]
pub fn kept_local_banner(reason: &SkipReason) -> Option<MeetingEvent> {
    match reason {
        SkipReason::KeptLocalNoModel { model } => Some(MeetingEvent::Warning {
            key: "summary-kept-local".into(),
            kind: WarningKind::Backend,
            message: crate::plain_text::kept_local_no_model(model, true),
        }),
        _ => None,
    }
}

/// What an explicit summary of a meeting kept on this computer (T12.13) fails with when no
/// summary model here is downloaded (kind `summary_kept_local`); never another writer.
#[must_use]
pub fn kept_local_error(model: &str) -> crate::ipc::IpcError {
    crate::ipc::IpcError {
        kind: "summary_kept_local".into(),
        message: crate::plain_text::kept_local_no_model(model, false),
        retryable: false,
    }
}

/// The banner at the start of a meeting whose summary will not run because Claude Code is
/// not installed or not signed in (key `summary-claude-code`), `None` for any other skip.
#[must_use]
pub fn claude_code_banner(reason: &SkipReason) -> Option<MeetingEvent> {
    match reason {
        SkipReason::ClaudeCodeUnavailable(e) => Some(MeetingEvent::Warning {
            key: "summary-claude-code".into(),
            kind: WarningKind::Backend,
            message: claude_code_copy(e, true),
        }),
        _ => None,
    }
}

/// Why the summary does not run without its API key, and what to do: `ahead` for the banner
/// at the start ("will not run"), else for the summary result ("did not run").
#[must_use]
pub fn no_key_copy(e: &earshot_runtime::summary_settings::ApiKeyError, ahead: bool) -> String {
    use earshot_runtime::summary_settings::ApiKeyError;
    let lead = if ahead {
        "The summary will not run"
    } else {
        "The summary did not run"
    };
    let why = match e {
        ApiKeyError::Missing => "no Anthropic API key is saved".to_owned(),
        // The store's own words are Windows' text: never shown (T9.23).
        ApiKeyError::Store(_) => "Windows Credential Manager could not be read".to_owned(),
        ApiKeyError::Unsupported | ApiKeyError::Invalid(_) => e.to_string(),
    };
    let fix = if matches!(e, ApiKeyError::Missing) {
        "Add it in Settings, Summaries, then re-summarise this meeting."
    } else {
        "Check the key in Settings, Summaries, then re-summarise this meeting."
    };
    format!("{lead}: {why}. {fix}")
}

/// The banner at the start of a meeting whose summary will not run for want of its API key
/// (key `summary-key`), `None` for any other skip.
#[must_use]
pub fn no_key_banner(reason: &SkipReason) -> Option<MeetingEvent> {
    match reason {
        SkipReason::NoApiKey(e) => Some(MeetingEvent::Warning {
            key: "summary-key".into(),
            kind: WarningKind::Backend,
            message: no_key_copy(e, true),
        }),
        _ => None,
    }
}

/// What the user is told when the final transcript could not be finished: what happened and
/// the way forward, then why in fixed copy.
#[must_use]
pub fn post_failed_copy(e: &RuntimeError) -> String {
    format!(
        "The transcript could not be finished. The recording and the draft transcript are saved. {}",
        crate::plain_text::runtime_text(e)
    )
}

/// What the summary step did, from the host's outcome. A failed final pass is
/// [`SummaryEvent::PostFailed`] (the summary never ran); a cancelled one
/// [`SummaryEvent::PostCancelled`].
#[must_use]
pub fn summary_event(outcome: &HostOutcome) -> SummaryEvent {
    if let Err(e) = &outcome.post {
        return SummaryEvent::PostFailed {
            message: post_failed_copy(e),
            retryable: e.is_retryable(),
        };
    }
    // With one token for both (the app's), nothing was summarised. A summary kept going on the draft
    // has its own result to report.
    if matches!(outcome.post, Ok(HostPost::Cancelled { .. }))
        && !matches!(outcome.summary, SummaryResult::Stored { .. })
    {
        return SummaryEvent::PostCancelled;
    }
    let voices = match &outcome.post {
        Ok(HostPost::Done(post)) => voices_to_name(&post.speakers),
        _ => 0,
    };
    summary_result_event(&outcome.summary, voices)
}

/// What the user is told when a summary meant for the graphics card ran on the processor
/// (never silent, T8.4ab); `None` otherwise.
#[must_use]
pub fn summary_fallback_note(result: &SummaryResult) -> Option<String> {
    let SummaryResult::Stored { outcome, .. } = result else {
        return None;
    };
    let SummaryRunReport::Local(report) = &outcome.report else {
        return None;
    };
    report.gpu_refused.as_deref().map(gpu_fallback_copy)
}

/// The words of [`summary_fallback_note`] for why the card was not used.
fn gpu_fallback_copy(why: &LocalSummaryError) -> String {
    match why {
        LocalSummaryError::GpuStalled => {
            "Your graphics card stopped responding, so this summary ran on the processor and took \
             longer. Summaries stay on the processor until you restart Earshot."
                .to_owned()
        }
        LocalSummaryError::GpuChildStalled => {
            "Your graphics card stopped responding, so this summary ran on the processor and took \
             longer. Earshot will try the card again next time."
                .to_owned()
        }
        LocalSummaryError::GpuChildDied { .. } => {
            "The summary stopped working on your graphics card, so it ran on the processor and \
             took longer. Your recording and transcript are safe."
                .to_owned()
        }
        LocalSummaryError::CudaFilesChanged { .. } => {
            "The CUDA files changed since Earshot started, so this summary ran on the processor and \
             took longer. Restart Earshot to check them again."
                .to_owned()
        }
        _ => "Your graphics card could not load the summary model, so this summary ran on the \
              processor and took longer."
            .to_owned(),
    }
}

/// What a summary run did, with `voices_to_name` for a stored summary (also a re-summary
/// of a stored meeting, `crate::jobs`).
#[must_use]
pub fn summary_result_event(result: &SummaryResult, voices_to_name: u32) -> SummaryEvent {
    match result {
        SummaryResult::Stored { outcome: s, .. } => stored_event(&s.summary, voices_to_name),
        SummaryResult::Skipped { reason } => SummaryEvent::Skipped {
            reason: skip_copy(reason),
        },
        SummaryResult::Failed { error, retryable } => SummaryEvent::Failed {
            message: crate::plain_text::runtime_text(error),
            retryable: *retryable,
        },
        SummaryResult::Cancelled => SummaryEvent::Cancelled,
    }
}

#[cfg(test)]
#[path = "meeting_events_tests.rs"]
mod tests;
