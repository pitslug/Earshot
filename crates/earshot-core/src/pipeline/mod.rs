//! The step-driven, sans-IO capture pipeline (PLAN.md 3.3, T6.2).
//!
//! ```text
//! frames(mic) ---> intake: sanitise, runs, resample 16 kHz --\            /--> Audio(Mic): cleaned "Me"
//!                                                              > align --> AEC
//! frames(ref) ---> intake: ... + drift steering (ref only) ---/   (lag fill) \--> Audio(Reference): mono "Them"
//!
//! poll():  Audio(track) -> live ASR backlog -> StreamingChunker -> Transcript(track)
//! end():   flush everything, finish live segments -> FinalPassRequested -> Ended
//! ```
//!
//! [`Pipeline`] implements [`CaptureSink`], so an [`crate::capture::AudioSource`] pumps
//! straight into it; [`Pipeline::poll`] drains the [`PipelineEvent`]s produced so far. It
//! owns no thread, clock or I/O: all time comes from the capture's [`DeviceTime`]s.
//!
//! # State machine
//!
//! The pipeline has two phases:
//!
//! - **Running**: [`CaptureSink::frames`] and [`CaptureSink::discontinuity`] are accepted;
//!   after each call the aligned audio available on every track is processed at once.
//! - **Ended**: entered by [`CaptureSink::end`], which flushes every resampler, pads every
//!   track with silence to a common end, drains the echo canceller's latency, decodes
//!   every live-ASR backlog and closes the open segments, and queues a final
//!   [`PipelineEvent::Stats`], one [`PipelineEvent::FinalPassRequested`] and then
//!   [`PipelineEvent::Ended`]. Later input is ignored and reported as
//!   [`PipelineError::AfterEnd`], the only event that can follow `Ended`.
//!
//! Live ASR ([`Pipeline::attach_live_asr`], optional per track) is **live** or
//! **stopped**: a decode error or cancellation drops that track's chunker and reports
//! [`PipelineError::LiveAsr`] once; capture, recording and the final-pass request carry
//! on. Decodes run only inside [`Pipeline::poll`] and `end`, on the caller's thread
//! (core spawns none); [`PartialPolicy`] lets a slow host coalesce or skip partial
//! decodes (backpressure).
//!
//! Each track's intake is in one of three states: **waiting** (no run: the next packet
//! starts one at its own timestamp), **in a run** (packets placed by sample count while
//! their timestamps agree with the previous packet's end within
//! [`PipelineConfig::gap_tolerance`]) or **muted** (the device rate is unusable; frames
//! are counted and dropped until a [`DiscontinuityKind::DeviceChanged`] brings a usable
//! rate). The `intake` module documents the run rules.
//!
//! # Timeline
//!
//! Every output is on one 16 kHz timeline whose frame 0 is the session origin (meeting
//! time zero). Per track: a gap in timestamps is silence at its place; audio for a
//! position already written (overlap, backwards timestamp, late data) is dropped; a
//! jump of more than [`PipelineConfig::max_gap`] from the track's end, in either
//! direction, is reported and the track continues where it was (the silence one packet
//! can create is bounded, and a clock that went back does not discard audio). All
//! meeting-time conversions of untrusted stamps are overflow-free. Neither track waits for
//! the other by more than [`PipelineConfig::max_track_lag`]: an idle loopback (WASAPI
//! sends no packets for silence) is filled with silence, so the mic never stalls.
//!
//! Untrusted input never panics: non-finite samples become zero and others are clamped
//! to [`SAMPLE_LIMIT`] (counted in [`TrackStats`]), trailing partial frames are dropped,
//! unknown tracks and unusable rates are [`PipelineEvent::Error`]s.
//!
//! # Echo cancellation and latency
//!
//! With a reference, the canceller runs on whole [`BLOCK`]s once both tracks have them
//! ([`EchoCanceller::process`], or [`EchoCanceller::process_stereo`] for a two-channel
//! reference; more channels are mixed to mono). Its output lags by
//! [`EchoCanceller::output_latency`] samples; the pipeline drops that pre-roll, so
//! cleaned sample `k` is emitted at pipeline frame `k` (timestamps need no further
//! correction), and at the end it feeds silence to push the last samples out. Without
//! a reference (Android, in-room mode) the mic passes through unchanged. The canceller
//! is never reset on a discontinuity: its own lock tracking handles echo-path changes
//! (M1 device-switch scenarios), and alignment is restored from the timestamps.
//!
//! # Clock drift
//!
//! Mic and loopback run on different device clocks; their timestamps share one (QPC).
//! Each intake measures its **skew**: where its packets' timestamps say they belong
//! minus where sample counting placed them, smoothed over
//! [`PipelineConfig::skew_smoothing`]. The reference resampler's ratio is steered by
//! `(skew_ref - skew_mic) / drift_time_constant` (a proportional controller, clamped to
//! [`PipelineConfig::max_drift_correction_ppm`]) on the skew difference low-passed over
//! [`PipelineConfig::drift_smoothing`] (loopback timestamps wander by milliseconds within
//! seconds, T6.17), so the reference keeps the mic's
//! position on the device clock with a residual offset of `drift x time constant`
//! (4 ms at 200 ppm), far inside the canceller's 600 ms delay search. The mic is never
//! resampled for drift (PLAN.md 3.3); its own skew against the device clock (which
//! shifts transcript times by up to 360 ms per hour at 100 ppm) is reported in
//! [`TrackStats::skew_ms`].
//!
//! A run that restarts while the other track has a run is anchored on the other
//! track's placement, not on its own stamp: it starts `skew_other` samples before its
//! stamp and with that skew, so the restart keeps the tracks aligned (the controller
//! sees no step). Only a run that starts while the other track has none is anchored on
//! its stamp. A host re-synchronises both placements with the device clock by sending
//! [`DiscontinuityKind::DataLost`] to both tracks at once (both runs end, the next
//! ones start on their stamps).
//!
//! # What the runtime (T6.4) must do
//!
//! - Call [`Pipeline::poll`] after delivering capture events and consume every event;
//!   audio accumulates in the pipeline until polled.
//! - Pump continuously: the cleaned mic is held back while the reference is behind by
//!   up to `max_track_lag`, and data later than that is dropped.
//! - Watch [`TrackStats::skew_ms`] of the mic: if it grows beyond the transcript timing
//!   budget, re-synchronise by sending [`DiscontinuityKind::DataLost`] to both tracks at
//!   the same moment (restarting the mic alone keeps the reference's skew, by design);
//!   the core never drops or inserts mic samples on its own. Deliver the two markers
//!   back to back, before either track's next frames: a track whose new run starts
//!   while the other still has its old run inherits that run's skew again.
//! - Treat [`PipelineError::TimestampJump`] (sleep/resume, broken driver clock) as a
//!   reason to warn the user; the pipeline keeps the tracks contiguous.
//! - Create the `Track` rows before capture and attach live ASR with their ids; poll
//!   from a thread that may block on decodes (capture keeps queueing in the meantime).
//!   Lower [`PartialPolicy`] to `Off` when [`TrackStats::asr_backlog`] keeps growing.
//!   A host that runs a track's [`LiveTrack`] on a thread of its own registers it with
//!   [`Pipeline::register_external_live`] instead: the pipeline then reports that track's
//!   backlog from the host and puts the track's own [`FinalPassTrack`] into the
//!   [`FinalPassRequest`].
//! - Write the [`PipelineEvent::Audio`] streams to `mic.flac` / `loopback.flac`; on
//!   [`PipelineEvent::FinalPassRequested`] run the final pass over them, then
//!   diarisation and the summary. Obsidian export is never part of stop.

mod intake;
mod live;
pub use live::LiveTrack;
mod types;

#[cfg(test)]
mod tests;

use std::collections::VecDeque;
use std::mem;
use std::time::Duration;

pub use types::{
    AecStats, FinalPassRequest, FinalPassTrack, Levels, LiveAsr, PartialPolicy, PipelineConfig,
    PipelineEnd, PipelineError, PipelineEvent, PipelineStats, TrackAudio, TrackStats,
};

use self::intake::Intake;
use crate::aec::{BLOCK, EchoCanceller};
use crate::asr::{CancelToken, MeetingTime, TranscribeOptions};
use crate::audio::{PIPELINE_RATE, Sample};
use crate::capture::{
    CaptureSession, CaptureSink, DeviceTime, Discontinuity, DiscontinuityKind, EndOfStream, TrackId,
};

/// Input samples beyond `+/-SAMPLE_LIMIT` are clamped (full scale is 1.0; this only stops
/// absurd values from a broken driver reaching the filters).
pub const SAMPLE_LIMIT: f32 = 16.0;

/// Under [`PartialPolicy::Coalesce`], the last this-much of a poll's queued audio is
/// decoded with partials on (the rest with partials off).
pub const COALESCE_TAIL: Duration = Duration::from_millis(100);

/// Index of a track in per-track arrays.
fn slot(track: TrackId) -> usize {
    match track {
        TrackId::Mic => 0,
        TrackId::Reference => 1,
    }
}

/// Config values in the units the stages use.
#[derive(Debug, Clone, Copy)]
struct Derived {
    tolerance_ticks: u64,
    /// Pipeline samples.
    max_gap: u64,
    max_lag: u64,
    meter: u64,
    stats: u64,
    skew_smoothing_s: f64,
    /// Pipeline samples.
    max_asr_backlog: u64,
    drift_time_constant_s: f64,
    drift_smoothing_s: f64,
}

impl Derived {
    fn new(cfg: &PipelineConfig) -> Self {
        let frames = |d| PIPELINE_RATE.frames_for(d) as u64;
        Self {
            tolerance_ticks: u64::try_from(cfg.gap_tolerance.as_nanos() / 100).unwrap_or(u64::MAX),
            max_gap: frames(cfg.max_gap),
            max_lag: frames(cfg.max_track_lag),
            meter: frames(cfg.meter_interval),
            stats: frames(cfg.stats_interval),
            skew_smoothing_s: cfg.skew_smoothing.as_secs_f64(),
            max_asr_backlog: frames(cfg.max_asr_backlog),
            drift_time_constant_s: cfg.drift_time_constant.as_secs_f64(),
            drift_smoothing_s: cfg.drift_smoothing.as_secs_f64(),
        }
    }
}

/// Pipeline phase; see the module docs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    Running,
    Ended,
}

/// Echo canceller and its alignment state.
#[derive(Debug)]
struct AecStage {
    canceller: EchoCanceller,
    /// Pipeline frames fed to the canceller (both tracks' lanes start here).
    processed: u64,
    /// Output samples still to drop (the canceller's latency pre-roll).
    preroll: usize,
}

/// Audio of one output track not yet handed out by `poll`.
#[derive(Debug)]
struct Pending {
    track: TrackId,
    /// Pipeline frame of `samples[0]`.
    start: u64,
    samples: Vec<Sample>,
}

impl Pending {
    fn new(track: TrackId) -> Self {
        Self {
            track,
            start: 0,
            samples: Vec::new(),
        }
    }

    fn end(&self) -> u64 {
        self.start + self.samples.len() as u64
    }
}

/// Reused per-call buffers of [`Pipeline::process`].
#[derive(Debug, Default)]
struct Scratch {
    mic: Vec<Vec<Sample>>,
    reference: Vec<Vec<Sample>>,
    out: Vec<Sample>,
    them: Vec<Sample>,
}

/// At most this many [`PipelineEvent::Levels`] wait for a poll; older ones are dropped
/// (meters are snapshots, only the recent ones matter).
pub const MAX_PENDING_LEVELS: usize = 64;

/// At most this many [`PipelineEvent::Stats`] wait for a poll; older ones are dropped
/// (their counters are cumulative, so the latest carries the totals).
pub const MAX_PENDING_STATS: usize = 16;

/// Energy accumulators for meters and stats.
#[derive(Debug, Default, Clone, Copy)]
struct Energy {
    mic: f64,
    mic_n: u64,
    cleaned: f64,
    cleaned_n: u64,
    reference: f64,
    reference_n: u64,
}

fn energy(x: &[Sample]) -> f64 {
    x.iter().map(|&v| f64::from(v) * f64::from(v)).sum()
}

fn dbfs(e: f64, n: u64) -> f32 {
    (10.0 * (e / n.max(1) as f64).max(1e-12).log10()) as f32
}

/// Live ASR of a track that the host runs ([`Pipeline::register_external_live`]).
struct ExternalLive {
    entry: FinalPassTrack,
    backlog: Box<dyn Fn() -> u64 + Send>,
}

impl std::fmt::Debug for ExternalLive {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ExternalLive")
            .field("entry", &self.entry)
            .field("backlog", &(self.backlog)())
            .finish()
    }
}

/// The capture pipeline; see the module docs.
#[derive(Debug)]
pub struct Pipeline {
    session: CaptureSession,
    cfg: PipelineConfig,
    d: Derived,
    phase: Phase,
    mic: Intake,
    reference: Option<Intake>,
    /// Present iff the session has a reference.
    aec: Option<AecStage>,
    cleaned: Pending,
    them: Pending,
    /// Outputs are cut at this pipeline frame (set at the end).
    limit: Option<u64>,
    meter: Energy,
    next_meter: u64,
    interval: Energy,
    next_stats: u64,
    last_stats: Option<u64>,
    /// Live ASR per track (mic, reference).
    live: [Option<LiveTrack>; 2],
    /// Live ASR a host runs per track ([`Pipeline::register_external_live`]).
    external: [Option<ExternalLive>; 2],
    /// Some capture input has been accepted (live ASR can no longer be attached).
    started: bool,
    scratch: Scratch,
    /// Snapshots waiting for a poll (bounded, oldest dropped).
    pending_levels: VecDeque<Levels>,
    pending_stats: VecDeque<PipelineStats>,
    /// Every other event, in order (unbounded: errors, markers, transcripts, the end).
    queue: Vec<PipelineEvent>,
    /// Low-passed reference-minus-mic skew (pipeline samples) and the mic lane end it was
    /// last updated at; `None` while either track has no run.
    steer_skew: Option<(f64, u64)>,
}

impl Pipeline {
    /// A pipeline for `session`.
    ///
    /// # Errors
    /// [`PipelineError::InvalidConfig`] for an invalid `cfg`;
    /// [`PipelineError::UnsupportedRate`] if a track's rate cannot be resampled.
    pub fn new(session: CaptureSession, cfg: PipelineConfig) -> Result<Self, PipelineError> {
        cfg.validate()?;
        let d = Derived::new(&cfg);
        let mic = Intake::new(TrackId::Mic, &session.mic, 1, false)?;
        let reference = match &session.reference {
            Some(info) => {
                let lanes = if info.channels.get() == 2 { 2 } else { 1 };
                Some(Intake::new(
                    TrackId::Reference,
                    info,
                    lanes,
                    cfg.drift_correction,
                )?)
            }
            None => None,
        };
        let aec = reference.as_ref().map(|_| {
            let mut canceller = EchoCanceller::new(cfg.aec);
            if let Some(hint) = cfg.delay_hint {
                canceller.set_delay_hint(hint);
            }
            AecStage {
                preroll: canceller.output_latency(),
                canceller,
                processed: 0,
            }
        });
        Ok(Self {
            session,
            next_meter: d.meter,
            next_stats: d.stats,
            cfg,
            d,
            phase: Phase::Running,
            mic,
            reference,
            aec,
            cleaned: Pending::new(TrackId::Mic),
            them: Pending::new(TrackId::Reference),
            limit: None,
            meter: Energy::default(),
            interval: Energy::default(),
            last_stats: None,
            live: [None, None],
            external: [None, None],
            started: false,
            scratch: Scratch::default(),
            pending_levels: VecDeque::new(),
            pending_stats: VecDeque::new(),
            queue: Vec::new(),
            steer_skew: None,
        })
    }

    /// The session this pipeline was built for.
    #[must_use]
    pub fn session(&self) -> &CaptureSession {
        &self.session
    }

    /// True once [`CaptureSink::end`] has been processed.
    #[must_use]
    pub fn is_ended(&self) -> bool {
        self.phase == Phase::Ended
    }

    /// Attaches live transcription to `track`. Must happen before any capture input.
    ///
    /// # Errors
    /// [`PipelineError::MissingTrack`] for an absent reference;
    /// [`PipelineError::InvalidConfig`] after input was accepted or if `track` already
    /// has live ASR; [`PipelineError::LiveAsr`] for an invalid chunker config.
    pub fn attach_live_asr(&mut self, track: TrackId, asr: LiveAsr) -> Result<(), PipelineError> {
        self.check_live_slot(track)?;
        self.live[slot(track)] = Some(LiveTrack::new(track, asr)?);
        Ok(())
    }

    /// Registers live ASR that the host runs for `entry.track` (a [`LiveTrack`] on a
    /// thread of its own, fed with this pipeline's [`PipelineEvent::Audio`]), so the
    /// pipeline reports it as its own: [`TrackStats::asr_backlog`] is `backlog()` (the
    /// samples the host has received and not yet decoded), and the track's
    /// [`FinalPassRequest`] entry is `entry` (the host's [`LiveTrack::final_pass`]). Must
    /// happen before any capture input.
    ///
    /// # Errors
    /// As [`Pipeline::attach_live_asr`].
    pub fn register_external_live(
        &mut self,
        entry: FinalPassTrack,
        backlog: Box<dyn Fn() -> u64 + Send>,
    ) -> Result<(), PipelineError> {
        let track = entry.track;
        self.check_live_slot(track)?;
        self.external[slot(track)] = Some(ExternalLive { entry, backlog });
        Ok(())
    }

    /// [`Pipeline::register_external_live`] for a host that starts its live ASR after capture
    /// (T9.1l): it may happen after capture input and before the end, and a later call replaces
    /// the earlier one, because the host keeps the track's audio from frame 0 itself and feeds
    /// it to its [`LiveTrack`] when that starts. A host that never starts it keeps the `entry`
    /// it first registered.
    ///
    /// # Errors
    /// [`PipelineError::MissingTrack`] for an absent reference;
    /// [`PipelineError::InvalidConfig`] after the end or if the pipeline itself runs `track`'s
    /// live ASR.
    pub fn register_held_live(
        &mut self,
        entry: FinalPassTrack,
        backlog: Box<dyn Fn() -> u64 + Send>,
    ) -> Result<(), PipelineError> {
        let track = entry.track;
        if track == TrackId::Reference && self.reference.is_none() {
            return Err(PipelineError::MissingTrack { track });
        }
        if self.phase == Phase::Ended || self.live[slot(track)].is_some() {
            return Err(PipelineError::InvalidConfig(format!(
                "{track:?} track cannot hold its live ASR now"
            )));
        }
        self.external[slot(track)] = Some(ExternalLive { entry, backlog });
        Ok(())
    }

    /// Whether live ASR may be attached to or registered for `track` now.
    fn check_live_slot(&self, track: TrackId) -> Result<(), PipelineError> {
        if track == TrackId::Reference && self.reference.is_none() {
            return Err(PipelineError::MissingTrack { track });
        }
        if self.started || self.phase == Phase::Ended {
            return Err(PipelineError::InvalidConfig(
                "attach live ASR before any capture input".into(),
            ));
        }
        if self.live[slot(track)].is_some() || self.external[slot(track)].is_some() {
            return Err(PipelineError::InvalidConfig(format!(
                "{track:?} track already has live ASR"
            )));
        }
        Ok(())
    }

    /// Samples queued for `track`'s live ASR, in the pipeline or at the host.
    fn asr_backlog(&self, track: TrackId) -> u64 {
        match (&self.live[slot(track)], &self.external[slot(track)]) {
            (Some(live), _) => live.backlog(),
            (None, Some(external)) => (external.backlog)(),
            (None, None) => 0,
        }
    }

    /// Changes how partial decodes are treated from the next poll on.
    pub fn set_partial_policy(&mut self, policy: PartialPolicy) {
        self.cfg.partials = policy;
    }

    /// A token that cancels `track`'s live decoding from any thread (for example when
    /// the user stops a meeting during a long decode). After cancellation the track's
    /// live ASR stops with [`PipelineError::LiveAsr`]. `None` without live ASR attached to
    /// the pipeline (a host running [`LiveTrack`]s on its own threads uses their
    /// [`LiveTrack::cancel_token`]).
    #[must_use]
    pub fn live_cancel_token(&self, track: TrackId) -> Option<CancelToken> {
        self.live[slot(track)]
            .as_ref()
            .and_then(LiveTrack::cancel_token)
    }

    /// Runs live ASR over the queued audio of each track (blocking decodes on the
    /// caller's thread, partials per [`PartialPolicy`]), then drains the events produced
    /// since the last call: the new audio of each track first (at most one
    /// [`PipelineEvent::Audio`] per track), then the pending [`PipelineEvent::Levels`]
    /// (at most [`MAX_PENDING_LEVELS`], the most recent) and [`PipelineEvent::Stats`] (at
    /// most [`MAX_PENDING_STATS`]), then every other event in the order it occurred.
    /// Levels and Stats are therefore not time-ordered against the Error, Discontinuity
    /// and Transcript events of the same poll; use their `end` times.
    /// After [`PipelineEvent::Ended`] only `Error(`[`PipelineError::AfterEnd`]`)` can be
    /// returned. A host that does not poll holds the unconsumed audio and the other
    /// events in memory; the snapshots are bounded, and a live-ASR backlog beyond
    /// [`PipelineConfig::max_asr_backlog`] stops that track's live ASR.
    ///
    /// Backpressure: audio that arrives while the host is busy decoding waits in the
    /// queue; the next poll decodes it with partials coalesced (or skipped, see
    /// [`PartialPolicy`]). Every word still reaches a final, because the decodes that
    /// finalise text (cuts and closes) always run; with an exact decoder the finals are
    /// unchanged, but with a real one (whisper) their text can differ slightly, since
    /// LocalAgreement commits words from fewer partial hypotheses.
    /// [`TrackStats::asr_backlog`] shows the queue.
    pub fn poll(&mut self) -> Vec<PipelineEvent> {
        let policy = self.cfg.partials;
        for stage in self.live.iter_mut().flatten() {
            stage.run(policy, &mut self.queue);
        }
        let mut out = Vec::with_capacity(
            self.queue.len() + self.pending_levels.len() + self.pending_stats.len() + 2,
        );
        for pending in [&mut self.cleaned, &mut self.them] {
            if !pending.samples.is_empty() {
                let samples = mem::take(&mut pending.samples);
                let start = pending.start;
                pending.start += samples.len() as u64;
                out.push(PipelineEvent::Audio(TrackAudio {
                    track: pending.track,
                    start,
                    samples,
                }));
            }
        }
        out.extend(self.pending_levels.drain(..).map(PipelineEvent::Levels));
        out.extend(self.pending_stats.drain(..).map(PipelineEvent::Stats));
        out.append(&mut self.queue);
        out
    }

    fn intake_mut(&mut self, track: TrackId) -> Option<&mut Intake> {
        match track {
            TrackId::Mic => Some(&mut self.mic),
            TrackId::Reference => self.reference.as_mut(),
        }
    }

    /// Rejects input after the end and for absent tracks.
    fn accepts(&mut self, track: TrackId) -> bool {
        let err = if self.phase == Phase::Ended {
            PipelineError::AfterEnd
        } else if track == TrackId::Reference && self.reference.is_none() {
            PipelineError::MissingTrack { track }
        } else {
            self.started = true;
            return true;
        };
        self.queue.push(PipelineEvent::Error(err));
        false
    }

    /// Pipeline frame of `ts` (zero before the origin, saturating).
    fn position(&self, ts: DeviceTime) -> u64 {
        self.session
            .meeting_time(ts)
            .map_or(0, |d| PIPELINE_RATE.frames_for(d) as u64)
    }

    /// Steers the reference resampler towards the mic's skew (module docs, Clock drift).
    fn steer(&mut self) {
        let Some(r) = self.reference.as_mut() else {
            return;
        };
        if !self.cfg.drift_correction || !self.mic.has_run() || !r.has_run() {
            self.steer_skew = None;
            return;
        }
        // Low-pass the skew difference (`drift_smoothing`): loopback timestamps wander by
        // milliseconds within seconds while the drift itself changes over minutes (T6.17).
        let diff = r.skew() - self.mic.skew();
        let now = self.mic.lanes.end();
        let filtered = match self.steer_skew {
            None => diff,
            Some((prev, at)) => {
                let secs = now.saturating_sub(at) as f64 / f64::from(PIPELINE_RATE.hz());
                let alpha = (secs / self.d.drift_smoothing_s).min(1.0);
                prev + alpha * (diff - prev)
            }
        };
        self.steer_skew = Some((filtered, now));
        let diff_s = filtered / f64::from(PIPELINE_RATE.hz());
        let max = self.cfg.max_drift_correction_ppm;
        let ppm = (diff_s / self.d.drift_time_constant_s * 1e6).clamp(-max, max);
        let ratio = 1.0 + ppm * 1e-6;
        if (ratio - r.ratio).abs() > 1e-7 {
            r.set_ratio(ratio);
        }
    }

    /// Processes everything that is aligned and available.
    fn process(&mut self) {
        // Scratch buffers are taken out of `self` for the call and put back, so steady
        // state processing does not allocate.
        let mut s = mem::take(&mut self.scratch);
        self.process_with(&mut s);
        self.scratch = s;
    }

    fn process_with(&mut self, s: &mut Scratch) {
        let Some(reference) = self.reference.as_mut() else {
            let n = self.mic.lanes.data[0].len();
            self.mic.lanes.take_into(n, &mut s.mic);
            self.emit_cleaned(&s.mic[0], Some(&s.mic[0]));
            return;
        };
        let (me, re) = (self.mic.lanes.end(), reference.lanes.end());
        if me > re + self.d.max_lag {
            reference.fill_idle_to(me - self.d.max_lag);
        } else if re > me + self.d.max_lag {
            self.mic.fill_idle_to(re - self.d.max_lag);
        }
        let Some(aec) = self.aec.as_mut() else {
            return;
        };
        let ready = self.mic.lanes.end().min(reference.lanes.end()) - aec.processed;
        let n = (ready as usize / BLOCK) * BLOCK;
        if n == 0 {
            return;
        }
        self.mic.lanes.take_into(n, &mut s.mic);
        reference.lanes.take_into(n, &mut s.reference);
        let mic = &s.mic[0];
        s.out.clear();
        s.out.resize(n, 0.0);
        s.them.clear();
        if let [l, r] = s.reference.as_slice() {
            aec.canceller.process_stereo(mic, l, r, &mut s.out);
            s.them.extend(l.iter().zip(r).map(|(a, b)| (a + b) * 0.5));
        } else {
            aec.canceller.process(mic, &s.reference[0], &mut s.out);
            s.them.extend_from_slice(&s.reference[0]);
        }
        let them = &s.them;
        aec.processed += n as u64;
        let skip = aec.preroll.min(n);
        aec.preroll -= skip;

        let mic_e = energy(mic);
        for acc in [&mut self.meter, &mut self.interval] {
            acc.mic += mic_e;
            acc.mic_n += n as u64;
        }
        let keep = self.cut(self.them.end(), them.len());
        let them_e = energy(&them[..keep]);
        self.meter.reference += them_e;
        self.meter.reference_n += keep as u64;
        self.them.samples.extend_from_slice(&them[..keep]);
        if let Some(stage) = &mut self.live[slot(TrackId::Reference)] {
            stage.feed(&them[..keep], self.d.max_asr_backlog, &mut self.queue);
        }
        self.emit_cleaned(&s.out[skip..], None);
    }

    /// How many of `n` samples starting at pipeline frame `at` lie before the limit.
    fn cut(&self, at: u64, n: usize) -> usize {
        self.limit
            .map_or(n, |l| (l.saturating_sub(at) as usize).min(n))
    }

    /// Appends cleaned mic audio, emitting meters and stats at their boundaries. `raw` is
    /// the uncancelled mic for the same samples, when it is not metered at the input.
    fn emit_cleaned(&mut self, cleaned: &[Sample], mut raw: Option<&[Sample]>) {
        let mut rest = &cleaned[..self.cut(self.cleaned.end(), cleaned.len())];
        while !rest.is_empty() {
            let pos = self.cleaned.end();
            let to_boundary = self.next_meter.min(self.next_stats) - pos;
            let k = rest.len().min(to_boundary as usize);
            let (piece, tail) = rest.split_at(k);
            let e = energy(piece);
            for acc in [&mut self.meter, &mut self.interval] {
                acc.cleaned += e;
                acc.cleaned_n += k as u64;
            }
            if let Some(r) = raw {
                let (rp, rt) = r.split_at(k);
                let e = energy(rp);
                for acc in [&mut self.meter, &mut self.interval] {
                    acc.mic += e;
                    acc.mic_n += k as u64;
                }
                raw = Some(rt);
            }
            self.cleaned.samples.extend_from_slice(piece);
            if let Some(stage) = &mut self.live[slot(TrackId::Mic)] {
                stage.feed(piece, self.d.max_asr_backlog, &mut self.queue);
            }
            let end = pos + k as u64;
            if end == self.next_meter {
                self.push_levels(end);
                self.next_meter += self.d.meter;
            }
            if end == self.next_stats {
                self.push_stats(end);
                self.next_stats += self.d.stats;
            }
            rest = tail;
        }
    }

    fn push_levels(&mut self, end: u64) {
        let m = mem::take(&mut self.meter);
        if self.pending_levels.len() == MAX_PENDING_LEVELS {
            self.pending_levels.pop_front();
        }
        self.pending_levels.push_back(Levels {
            end: MeetingTime::from_pipeline_frames(end),
            mic_dbfs: dbfs(m.mic, m.mic_n),
            cleaned_dbfs: dbfs(m.cleaned, m.cleaned_n),
            reference_dbfs: self
                .reference
                .as_ref()
                .map(|_| dbfs(m.reference, m.reference_n)),
        });
    }

    fn push_stats(&mut self, end: u64) {
        let acc = mem::take(&mut self.interval);
        let floor = 1e-10;
        let attenuation_db = (acc.mic > floor * acc.mic_n as f64
            && acc.cleaned > floor * acc.cleaned_n as f64)
            .then(|| {
                let mic = acc.mic / acc.mic_n as f64;
                let cleaned = acc.cleaned / acc.cleaned_n as f64;
                10.0 * (mic / cleaned).log10()
            });
        let aec = self.aec.as_ref().map(|a| {
            let s = a.canceller.status();
            let ms_per_sample = 1000.0 / f64::from(PIPELINE_RATE.hz());
            AecStats {
                delay_ms: s.delay.delay().map(|d| d * ms_per_sample),
                locked: s.delay.is_locked(),
                double_talk: s.doubletalk,
                vetoed: s.vetoed,
                suppression_gain: s.mean_gain,
                bypass: s.bypass,
                converged: s.converged,
                residual_drift_ppm: a.canceller.linear().delay_estimator().drift_ppm(),
                attenuation_db,
            }
        });
        let reference_correction_ppm = self
            .reference
            .as_ref()
            .map_or(0.0, |r| (r.ratio - 1.0) * 1e6);
        if self.pending_stats.len() == MAX_PENDING_STATS {
            self.pending_stats.pop_front();
        }
        self.pending_stats.push_back(PipelineStats {
            end: MeetingTime::from_pipeline_frames(end),
            aec,
            mic: TrackStats {
                asr_backlog: self.asr_backlog(TrackId::Mic),
                ..self.mic.track_stats()
            },
            reference: self.reference.as_ref().map(|r| TrackStats {
                asr_backlog: self.asr_backlog(TrackId::Reference),
                ..r.track_stats()
            }),
            reference_correction_ppm,
        });
        self.last_stats = Some(end);
    }
}

impl CaptureSink for Pipeline {
    fn frames(&mut self, track: TrackId, frames: &[Sample], device_ts: DeviceTime) {
        if !self.accepts(track) {
            return;
        }
        let (origin, d) = (self.session.origin, self.d);
        // A new run on this track is anchored on the other track's placement (module
        // docs, Clock drift); without an active partner it is anchored on its stamp.
        let partner = match track {
            TrackId::Mic => self.reference.as_ref(),
            TrackId::Reference => Some(&self.mic),
        };
        let bias = partner.filter(|p| p.has_run()).map_or(0.0, Intake::skew);
        let mut events = mem::take(&mut self.queue);
        if let Some(intake) = self.intake_mut(track) {
            intake.push(frames, device_ts, origin, bias, &d, &mut events);
        }
        self.queue = events;
        if track == TrackId::Reference {
            self.steer();
        }
        self.process();
    }

    fn discontinuity(&mut self, track: TrackId, discontinuity: Discontinuity) {
        if !self.accepts(track) {
            return;
        }
        let at = MeetingTime::from_pipeline_frames(self.position(discontinuity.at));
        let result = match (&discontinuity.kind, self.intake_mut(track)) {
            (DiscontinuityKind::DataLost, Some(intake)) => {
                intake.flush();
                Ok(())
            }
            (DiscontinuityKind::DeviceChanged { rate, .. }, Some(intake)) => {
                intake.device_changed(*rate)
            }
            // A note changes nothing about the audio: the run and its drift estimate go on.
            (DiscontinuityKind::Note { .. }, _) | (_, None) => Ok(()),
        };
        self.queue.push(PipelineEvent::Discontinuity {
            track,
            at,
            kind: discontinuity.kind,
        });
        if let Err(e) = result {
            self.queue.push(PipelineEvent::Error(e));
        }
        self.process();
    }

    fn end(&mut self, end: EndOfStream) {
        if self.phase == Phase::Ended {
            self.queue
                .push(PipelineEvent::Error(PipelineError::AfterEnd));
            return;
        }
        self.mic.flush();
        let mut last = self.mic.lanes.end();
        if let Some(r) = self.reference.as_mut() {
            r.flush();
            last = last.max(r.lanes.end());
        }
        let at = self.position(end.at);
        if at > last && at - last <= self.d.max_gap {
            last = at;
        }
        self.mic.fill_to(last);
        if let Some(r) = self.reference.as_mut() {
            r.fill_to(last);
        }
        self.process();
        if let Some(aec) = &self.aec {
            // Feed silence (uncounted) until the canceller has output every sample: output
            // for mic frame `k` appears once frame `k + latency` has been fed.
            let latency = aec.canceller.output_latency() as u64;
            let target = (last + latency).div_ceil(BLOCK as u64) * BLOCK as u64;
            let target = target.max(aec.processed);
            self.limit = Some(last);
            self.mic.lanes.fill_to(target);
            if let Some(r) = self.reference.as_mut() {
                r.lanes.fill_to(target);
            }
            self.process();
        }
        let policy = self.cfg.partials;
        for stage in self.live.iter_mut().flatten() {
            stage.finish(policy, &mut self.queue);
        }
        if self.last_stats != Some(last) {
            self.push_stats(last);
        }
        let tracks = self
            .session
            .tracks()
            .map(
                |track| match (&self.live[slot(track)], &self.external[slot(track)]) {
                    (Some(stage), _) => stage.final_pass(),
                    (None, Some(external)) => external.entry.clone(),
                    (None, None) => FinalPassTrack {
                        track,
                        track_entity: None,
                        options: TranscribeOptions::batch(),
                    },
                },
            )
            .collect();
        self.queue
            .push(PipelineEvent::FinalPassRequested(FinalPassRequest {
                end: MeetingTime::from_pipeline_frames(last),
                tracks,
            }));
        self.queue.push(PipelineEvent::Ended(PipelineEnd {
            end: MeetingTime::from_pipeline_frames(last),
            reason: end.reason,
        }));
        self.phase = Phase::Ended;
    }
}
