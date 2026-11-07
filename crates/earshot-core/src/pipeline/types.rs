//! Pipeline settings, events and errors.

use std::fmt;
use std::time::Duration;

use crate::aec::EchoCancellerConfig;
use crate::asr::{
    AsrError, ChunkerConfig, MeetingTime, SegmentIds, TranscribeOptions, Transcriber,
    TranscriptEvent, Vad,
};
use crate::audio::{DEFAULT_MAX_DEVIATION, PIPELINE_RATE, Sample};
use crate::capture::{DiscontinuityKind, EndReason, TrackId};
use crate::identity::EntityId;

/// Upper bound for [`PipelineConfig::max_gap`] (keeps every derived sample count far
/// inside `i64`).
pub const MAX_GAP_LIMIT: Duration = Duration::from_secs(3600);

/// Pipeline settings. [`Default`] suits a desktop call (WASAPI, 10 ms packets).
#[derive(Debug, Clone, PartialEq)]
pub struct PipelineConfig {
    /// Echo canceller settings.
    pub aec: EchoCancellerConfig,
    /// Warm-start echo delay (samples at 16 kHz), for example from the audio check. A
    /// hint only seeds the delay search (AGENTS.md invariant).
    pub delay_hint: Option<f64>,
    /// A packet whose timestamp differs from the previous packet's end by more than this
    /// starts a new run (gap or overlap); smaller differences are device jitter and the
    /// packet is taken as contiguous. Default 4 ms; must be below `max_track_lag`.
    pub gap_tolerance: Duration,
    /// A timestamp more than this ahead of or behind the track's end is not believed:
    /// the track continues where it was and [`PipelineError::TimestampJump`] is
    /// reported. It bounds the silence a single packet can make the pipeline synthesise
    /// and keeps a clock that went back from discarding audio. Default 30 s; must be at
    /// least `max_track_lag` and at most [`MAX_GAP_LIMIT`].
    pub max_gap: Duration,
    /// Neither track waits for the other longer than this: when one track is ahead by
    /// more, the other is filled with silence up to `leader - max_track_lag` (an idle
    /// loopback delivers no packets at all). Audio arriving later for filled positions is
    /// dropped. Default 400 ms; it is the worst-case extra latency of the cleaned mic.
    pub max_track_lag: Duration,
    /// Steer the reference resampler so that the reference keeps the mic's position
    /// relative to the shared device clock (PLAN.md 3.3: drift is compensated on the
    /// reference, never the mic). Default on. Off, a 16 kHz reference bypasses the
    /// resampler.
    pub drift_correction: bool,
    /// Time constant of the drift steering: the correction is the measured
    /// reference-minus-mic skew divided by this. A constant drift `d` leaves a residual
    /// offset of `d * drift_time_constant` (200 ppm x 20 s = 4 ms). Default 20 s.
    pub drift_time_constant: Duration,
    /// Largest steering correction (ppm, at most 10 000). Default 1 000.
    pub max_drift_correction_ppm: f64,
    /// Smoothing time of the per-track skew measurement (device jitter). Default 1 s.
    pub skew_smoothing: Duration,
    /// Low-pass time constant of the reference-minus-mic skew that drives the drift
    /// steering (the reported skews keep `skew_smoothing`). Default 20 s: WASAPI loopback
    /// timestamps can wander on a cycle of seconds while the samples
    /// keep the device clock (T6.17, seen on real recordings), and steering on the 1 s skew copied that wander into the reference
    /// audio (+-170 ppm). With the 20 s `drift_time_constant` the loop has damping 0.5
    /// (1 / (2 sqrt(smoothing / time constant))), still tracks 200 ppm to within 6 ms in a
    /// minute (`pipeline_reference_drift_is_steered_and_bounded`), and the residual offset
    /// of a constant drift is unchanged.
    pub drift_smoothing: Duration,
    /// Level meter period on the meeting timeline. Default 100 ms.
    pub meter_interval: Duration,
    /// [`PipelineStats`] period on the meeting timeline. Default 1 s.
    pub stats_interval: Duration,
    /// How live ASR treats partial decodes (backpressure). Default
    /// [`PartialPolicy::Coalesce`]; changeable with
    /// [`super::Pipeline::set_partial_policy`].
    pub partials: PartialPolicy,
    /// Largest live-ASR backlog per track. A track whose undecoded audio exceeds this
    /// (a decoder slower than real time never catches up; the queue costs about 230 MB
    /// per hour) stops its live ASR with [`PipelineError::LiveAsr`] ("fell behind"); the
    /// final pass still transcribes the recording. Default 10 min; must be > 0.
    pub max_asr_backlog: Duration,
}

impl Default for PipelineConfig {
    fn default() -> Self {
        Self {
            aec: EchoCancellerConfig::default(),
            delay_hint: None,
            gap_tolerance: Duration::from_millis(4),
            max_gap: Duration::from_secs(30),
            max_track_lag: Duration::from_millis(400),
            drift_correction: true,
            drift_time_constant: Duration::from_secs(20),
            max_drift_correction_ppm: 1_000.0,
            skew_smoothing: Duration::from_secs(1),
            drift_smoothing: Duration::from_secs(20),
            meter_interval: Duration::from_millis(100),
            stats_interval: Duration::from_secs(1),
            partials: PartialPolicy::Coalesce,
            max_asr_backlog: Duration::from_secs(600),
        }
    }
}

impl PipelineConfig {
    /// Checks the constraints documented on each field.
    ///
    /// # Errors
    /// [`PipelineError::InvalidConfig`] naming the first violated constraint.
    pub fn validate(&self) -> Result<(), PipelineError> {
        let fail = |m: &str| Err(PipelineError::InvalidConfig(m.to_owned()));
        let frames = |d: Duration| PIPELINE_RATE.frames_for(d);
        if frames(self.meter_interval) == 0 || frames(self.stats_interval) == 0 {
            return fail("meter_interval and stats_interval must be at least one sample");
        }
        if frames(self.max_track_lag) == 0 || self.gap_tolerance >= self.max_track_lag {
            return fail("max_track_lag must be > 0 and above gap_tolerance");
        }
        if self.max_gap < self.max_track_lag || self.max_gap > MAX_GAP_LIMIT {
            return fail("max_gap must be at least max_track_lag and at most 1 h");
        }
        if frames(self.max_asr_backlog) == 0 {
            return fail("max_asr_backlog must be at least one sample");
        }
        if self.drift_time_constant.is_zero()
            || self.skew_smoothing.is_zero()
            || self.drift_smoothing.is_zero()
        {
            return fail("drift_time_constant, skew_smoothing and drift_smoothing must be > 0");
        }
        let ppm = self.max_drift_correction_ppm;
        if !(ppm.is_finite() && (0.0..=DEFAULT_MAX_DEVIATION * 1e6).contains(&ppm)) {
            return fail("max_drift_correction_ppm must be in [0, 10000]");
        }
        if self.delay_hint.is_some_and(|d| !d.is_finite() || d < 0.0) {
            return fail("delay_hint must be finite and non-negative");
        }
        if let Err(e) = self.aec.validate() {
            return fail(&e.to_string());
        }
        Ok(())
    }
}

/// Something the pipeline produced; drained with [`super::Pipeline::poll`].
#[derive(Debug, Clone, PartialEq)]
pub enum PipelineEvent {
    /// New 16 kHz mono audio of a track, contiguous with the previous event of that track
    /// (the first starts at pipeline frame 0, meeting time zero). The mic is post-AEC
    /// ("Me", `mic.flac`); the reference is the aligned mono mix ("Them",
    /// `loopback.flac`, PLAN.md 3.6). This is what live ASR and the recorder consume.
    Audio(TrackAudio),
    /// Level meters for the last [`PipelineConfig::meter_interval`].
    Levels(Levels),
    /// Periodic diagnostics (every [`PipelineConfig::stats_interval`], and once at the
    /// end).
    Stats(PipelineStats),
    /// A capture discontinuity was received (informational; the timeline is kept).
    Discontinuity {
        /// Track.
        track: TrackId,
        /// Meeting time of the break (zero if before the origin).
        at: MeetingTime,
        /// What happened.
        kind: DiscontinuityKind,
    },
    /// Input was rejected or reinterpreted; the pipeline keeps running.
    Error(PipelineError),
    /// A live transcript event of a track (partial, final or discarded segment). The
    /// segment's `track` is the [`LiveAsr::track_entity`] given for that track.
    Transcript {
        /// Track.
        track: TrackId,
        /// The event.
        event: TranscriptEvent,
    },
    /// Capture has ended and every live segment is closed: the host should run the
    /// final pass (accurate model, full context) over the recorded tracks, then
    /// diarisation and the summary (PLAN.md 3.3). Emitted once, right before
    /// [`PipelineEvent::Ended`]. Obsidian export is never part of it.
    FinalPassRequested(FinalPassRequest),
    /// The session ended: every track's audio has been emitted up to `end`. Only
    /// [`PipelineError::AfterEnd`] errors can follow it.
    Ended(PipelineEnd),
}

/// New audio of one track; see [`PipelineEvent::Audio`].
#[derive(Debug, Clone, PartialEq)]
pub struct TrackAudio {
    /// [`TrackId::Mic`] (cleaned) or [`TrackId::Reference`] (mono mix).
    pub track: TrackId,
    /// Pipeline frame (16 kHz sample index from meeting time zero) of `samples[0]`.
    pub start: u64,
    /// Mono samples at [`PIPELINE_RATE`].
    pub samples: Vec<Sample>,
}

impl TrackAudio {
    /// Meeting time of the first sample.
    #[must_use]
    pub fn start_time(&self) -> MeetingTime {
        MeetingTime::from_pipeline_frames(self.start)
    }
}

/// RMS levels (dBFS, floored at -120) over one meter interval ending at `end`. With a
/// reference, the mic and reference are metered at the canceller input, which runs up
/// to one block plus the canceller latency (24 ms) ahead of the cleaned window.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Levels {
    /// End of the interval.
    pub end: MeetingTime,
    /// Microphone before echo cancellation.
    pub mic_dbfs: f32,
    /// Microphone after echo cancellation (equal to `mic_dbfs` without a reference).
    pub cleaned_dbfs: f32,
    /// Reference (mono mix), if the session has one.
    pub reference_dbfs: Option<f32>,
}

/// Periodic diagnostics.
#[derive(Debug, Clone, PartialEq)]
pub struct PipelineStats {
    /// Meeting time the stats refer to (the cleaned mic's position).
    pub end: MeetingTime,
    /// Echo canceller state, if the session has a reference.
    pub aec: Option<AecStats>,
    /// Microphone intake.
    pub mic: TrackStats,
    /// Reference intake, if the session has one.
    pub reference: Option<TrackStats>,
    /// Current drift steering of the reference resampler (ppm; positive = the reference
    /// is stretched because its device clock runs slow).
    pub reference_correction_ppm: f64,
}

/// Echo canceller diagnostics (from [`crate::aec::BlockStatus`] and the delay estimator).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AecStats {
    /// Tracked echo delay (ms), when locked or hinted.
    pub delay_ms: Option<f64>,
    /// The delay estimator has locked on an echo path.
    pub locked: bool,
    /// Double talk in the last block.
    pub double_talk: bool,
    /// Mono reference: the double-talk call of the last block was overridden by the
    /// coherence veto ([`crate::aec::BlockStatus::vetoed`]); an effective call is
    /// `double_talk && !vetoed`.
    pub vetoed: bool,
    /// Mean suppression gain over bins of the last block (1 = none;
    /// [`crate::aec::BlockStatus::mean_gain`]).
    pub suppression_gain: f32,
    /// The mic passes through unsuppressed (no echo path evidence, or mismatch).
    pub bypass: bool,
    /// The linear stage has converged.
    pub converged: bool,
    /// Residual render-vs-capture drift the delay estimator still sees (ppm).
    pub residual_drift_ppm: Option<f64>,
    /// Mic power over cleaned power in the last stats interval (dB); `None` when either
    /// is silent. ERLE only while the far end talks alone.
    pub attenuation_db: Option<f64>,
}

/// Per-track intake counters (cumulative) and clock skew.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct TrackStats {
    /// Smoothed position of the device timestamps ahead of where the track's samples are
    /// placed (ms; positive = the device delivers fewer samples than its nominal rate
    /// against the device clock). A run restarted while the other track has a run starts
    /// with that track's skew (module docs, Clock drift); otherwise with zero.
    pub skew_ms: f64,
    /// Non-finite input samples replaced by zero.
    pub non_finite: u64,
    /// Finite input samples clamped to [`super::SAMPLE_LIMIT`].
    pub clamped: u64,
    /// Trailing partial frames (fewer samples than channels) dropped.
    pub partial_frames: u64,
    /// Runs restarted at a packet's own timestamp (gap or overlap beyond
    /// [`PipelineConfig::gap_tolerance`], a marker, or a timestamp jump).
    pub restarts: u64,
    /// Pipeline samples of silence inserted (gaps, waiting for the other track, end).
    pub zero_filled: u64,
    /// Pipeline samples dropped because their position had already been emitted or filled
    /// (overlaps, backwards timestamps, late audio).
    pub late_dropped: u64,
    /// Input frames dropped while the track is muted (unsupported rate).
    pub muted_frames: u64,
    /// Pipeline samples queued for live ASR, not yet decoded (0 without live ASR). The
    /// queue is drained by [`super::Pipeline::poll`], or by the host for a track it
    /// registered ([`super::Pipeline::register_external_live`]), which reports it.
    pub asr_backlog: u64,
}

/// End of the session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PipelineEnd {
    /// Meeting time every track was emitted up to.
    pub end: MeetingTime,
    /// Why capture ended.
    pub reason: EndReason,
}

/// How live ASR runs partial decodes; see [`super::Pipeline::poll`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum PartialPolicy {
    /// Every partial decode the chunker schedules (one per `partial_interval` of audio),
    /// however far behind the host is.
    All,
    /// When a poll finds more audio queued than [`super::COALESCE_TAIL`], partial decodes
    /// are skipped for all but its last [`super::COALESCE_TAIL`]: one coalesced partial
    /// instead of one per second of backlog. A host that keeps up sees no difference.
    #[default]
    Coalesce,
    /// No partial decodes: only finals (lowest CPU, no live hypothesis).
    Off,
}

/// Live transcription of one track: a batch [`Transcriber`] and a [`Vad`] driven by a
/// [`crate::asr::StreamingChunker`] that the pipeline builds and runs inside
/// [`super::Pipeline::poll`] (core spawns no thread; the host chooses the thread that
/// polls).
pub struct LiveAsr {
    /// Live model (fast).
    pub transcriber: Box<dyn Transcriber + Send>,
    /// Voice activity detector.
    pub vad: Box<dyn Vad + Send>,
    /// Chunker settings (validated when attached).
    pub config: ChunkerConfig,
    /// The `Track` row this track's segments belong to (created by the host before
    /// capture starts, PLAN.md 3.5).
    pub track_entity: EntityId,
    /// Segment id source.
    pub segment_ids: SegmentIds,
}

impl fmt::Debug for LiveAsr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LiveAsr")
            .field("backend", &self.transcriber.backend_info())
            .field("config", &self.config)
            .field("track_entity", &self.track_entity)
            .finish_non_exhaustive()
    }
}

/// What the final pass should transcribe; see [`PipelineEvent::FinalPassRequested`].
#[derive(Debug, Clone, PartialEq)]
pub struct FinalPassRequest {
    /// End of the recorded audio (every track was emitted up to here).
    pub end: MeetingTime,
    /// The session's tracks, mic first.
    pub tracks: Vec<FinalPassTrack>,
}

/// One track of a [`FinalPassRequest`].
#[derive(Debug, Clone, PartialEq)]
pub struct FinalPassTrack {
    /// Which track (its audio is the [`PipelineEvent::Audio`] stream of that track).
    pub track: TrackId,
    /// The track's `Track` row, when live ASR was attached to it or registered for it.
    pub track_entity: Option<EntityId>,
    /// Decoding for the final pass: [`TranscribeOptions::batch`] in the live language.
    pub options: TranscribeOptions,
}

/// Pipeline errors: [`super::Pipeline::new`] fails with them, and at run time they are
/// reported as [`PipelineEvent::Error`] while the pipeline keeps going.
#[derive(Debug, Clone, PartialEq)]
pub enum PipelineError {
    /// A [`PipelineConfig`] constraint is violated.
    InvalidConfig(String),
    /// The resampler cannot take this rate: the track is muted until a device change
    /// announces a usable rate (its time is filled with silence).
    UnsupportedRate {
        /// Track.
        track: TrackId,
        /// The rejected rate.
        rate_hz: u32,
    },
    /// A timestamp jump (either direction) beyond [`PipelineConfig::max_gap`] from the
    /// track's current end: the packet was placed right after the track's previous audio
    /// instead.
    TimestampJump {
        /// Track.
        track: TrackId,
        /// Size of the jump that was not believed.
        gap: Duration,
        /// The stamp lay before the track's end (a clock that went back).
        backwards: bool,
    },
    /// Frames or a marker for a track the session does not have; ignored.
    MissingTrack {
        /// Track.
        track: TrackId,
    },
    /// Input after the end of the session; ignored.
    AfterEnd,
    /// Live ASR of a track failed (or was cancelled): its chunker is dropped and the
    /// track gets no more live transcript events. Capture, recording and the final-pass
    /// request are unaffected. Also returned by [`super::Pipeline::attach_live_asr`]
    /// for an invalid chunker config.
    LiveAsr {
        /// Track.
        track: TrackId,
        /// What failed.
        error: AsrError,
    },
}

impl fmt::Display for PipelineError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidConfig(m) => write!(f, "invalid pipeline config: {m}"),
            Self::UnsupportedRate { track, rate_hz } => {
                write!(f, "{track:?} track: unsupported sample rate {rate_hz} Hz")
            }
            Self::TimestampJump {
                track,
                gap,
                backwards,
            } => write!(
                f,
                "{track:?} track: {} timestamp jump of {:.3} s ignored",
                if *backwards { "backward" } else { "forward" },
                gap.as_secs_f64()
            ),
            Self::MissingTrack { track } => write!(f, "session has no {track:?} track"),
            Self::AfterEnd => write!(f, "input after the end of the session"),
            Self::LiveAsr { track, error } => {
                write!(f, "{track:?} track: live ASR stopped: {error}")
            }
        }
    }
}

impl std::error::Error for PipelineError {}
