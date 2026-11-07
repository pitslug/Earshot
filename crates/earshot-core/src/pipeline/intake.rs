//! Per-track intake: sanitising, runs, resampling to the pipeline rate, clock skew.
//!
//! A track's packets form **runs**: a run starts at a packet's own timestamp, and every
//! following packet whose timestamp is within `gap_tolerance` of the previous packet's
//! end (packet-relative, never the run start, so device clock drift is never mistaken for
//! a gap) continues it, its output placed by sample count. Anything else (a gap, an
//! overlap, a backwards timestamp, a marker) flushes the run's resampler and starts a new
//! run at the packet's timestamp. Placement into [`Lanes`] fills gaps with silence and
//! drops audio for positions already written.

use super::types::{PipelineError, PipelineEvent, TrackStats};
use super::{Derived, SAMPLE_LIMIT};
use crate::audio::{PIPELINE_RATE, Resampler, Sample, SampleRate};
use crate::capture::{DeviceTime, TrackId, TrackInfo};

/// Pipeline samples per device tick.
const SAMPLES_PER_TICK: f64 = PIPELINE_RATE.hz() as f64 / DeviceTime::TICKS_PER_SECOND as f64;

/// Pipeline-rate audio of one track not yet consumed: one `Vec` per lane (1 or 2), all of
/// equal length, `data[k][0]` at pipeline frame `start`.
#[derive(Debug)]
pub(super) struct Lanes {
    pub(super) data: Vec<Vec<Sample>>,
    pub(super) start: u64,
}

impl Lanes {
    fn new(lanes: usize) -> Self {
        Self {
            data: vec![Vec::new(); lanes],
            start: 0,
        }
    }

    /// Pipeline frame after the last written sample.
    pub(super) fn end(&self) -> u64 {
        self.start + self.data[0].len() as u64
    }

    /// Appends silence up to `target`; returns the samples added.
    pub(super) fn fill_to(&mut self, target: u64) -> u64 {
        let add = target.saturating_sub(self.end());
        for lane in &mut self.data {
            lane.resize(lane.len() + add as usize, 0.0);
        }
        add
    }

    /// Moves the first `n` samples of every lane into `out` (one reused buffer per lane;
    /// `out` is resized to the lane count).
    pub(super) fn take_into(&mut self, n: usize, out: &mut Vec<Vec<Sample>>) {
        self.start += n as u64;
        out.resize_with(self.data.len(), Vec::new);
        for (lane, dst) in self.data.iter_mut().zip(out.iter_mut()) {
            dst.clear();
            dst.extend(lane.drain(..n));
        }
    }
}

/// A run of contiguous packets.
#[derive(Debug, Clone, Copy)]
struct Run {
    /// Pipeline frame of the run's first output sample.
    start: i64,
    /// Output samples placed so far.
    emitted: u64,
    /// Output samples owed for the input consumed (integrates the resampling ratio).
    owed: f64,
    /// Placement minus timestamp position at the run start (non-zero after a jump).
    ts_offset: f64,
    /// Where the next packet is expected (`None` after a timestamp overflow).
    next_ts: Option<DeviceTime>,
}

/// Intake state of one track.
#[derive(Debug)]
pub(super) struct Intake {
    pub(super) id: TrackId,
    /// Device channels per input frame.
    channels: usize,
    /// The reference is steered for drift, so it always runs through a resampler.
    steerable: bool,
    /// Current input rate; `None` while muted (unsupported rate).
    rate: Option<SampleRate>,
    /// One per lane; empty when the input is already at the pipeline rate (bypass).
    resamplers: Vec<Resampler>,
    pub(super) ratio: f64,
    run: Option<Run>,
    started: bool,
    /// Smoothed timestamp-minus-placement skew of the current run (pipeline samples).
    skew: f64,
    pub(super) lanes: Lanes,
    pub(super) stats: TrackStats,
    scratch_in: Vec<Vec<Sample>>,
    scratch_out: Vec<Vec<Sample>>,
}

impl Intake {
    /// An intake producing `lanes` lanes (1, or 2 for a stereo reference).
    pub(super) fn new(
        id: TrackId,
        info: &TrackInfo,
        lanes: usize,
        steerable: bool,
    ) -> Result<Self, PipelineError> {
        let mut intake = Self {
            id,
            channels: usize::from(info.channels.get()),
            steerable,
            rate: None,
            resamplers: Vec::new(),
            ratio: 1.0,
            run: None,
            started: false,
            skew: 0.0,
            lanes: Lanes::new(lanes),
            stats: TrackStats::default(),
            scratch_in: vec![Vec::new(); lanes],
            scratch_out: vec![Vec::new(); lanes],
        };
        intake.configure(info.rate)?;
        Ok(intake)
    }

    /// Sets the input rate, building resamplers unless the input can bypass them. On
    /// error the track is muted.
    fn configure(&mut self, rate: SampleRate) -> Result<(), PipelineError> {
        self.rate = None;
        self.resamplers.clear();
        if rate != PIPELINE_RATE || self.steerable {
            for _ in 0..self.lanes.data.len() {
                let mut r = Resampler::new(rate, PIPELINE_RATE).map_err(|_| {
                    PipelineError::UnsupportedRate {
                        track: self.id,
                        rate_hz: rate.hz(),
                    }
                })?;
                // Within DEFAULT_MAX_DEVIATION by construction (config validation).
                let _ = r.set_relative_ratio(self.ratio);
                self.resamplers.push(r);
            }
        }
        self.rate = Some(rate);
        Ok(())
    }

    /// A run is in progress (the track is delivering).
    pub(super) fn has_run(&self) -> bool {
        self.run.is_some()
    }

    /// Smoothed skew in pipeline samples (see [`TrackStats::skew_ms`]).
    pub(super) fn skew(&self) -> f64 {
        self.skew
    }

    /// Counters plus the current skew.
    pub(super) fn track_stats(&self) -> TrackStats {
        TrackStats {
            skew_ms: self.skew * 1000.0 / f64::from(PIPELINE_RATE.hz()),
            ..self.stats
        }
    }

    /// Sets the relative resampling ratio (reference drift steering).
    pub(super) fn set_ratio(&mut self, ratio: f64) {
        self.ratio = ratio;
        for r in &mut self.resamplers {
            // The caller clamps to max_drift_correction_ppm (<= DEFAULT_MAX_DEVIATION).
            let _ = r.set_relative_ratio(ratio);
        }
    }

    /// Fills the lanes with silence up to `target`, counting it.
    pub(super) fn fill_to(&mut self, target: u64) {
        self.stats.zero_filled += self.lanes.fill_to(target);
    }

    /// The track has delivered nothing while the other one ran `max_track_lag` ahead:
    /// silence up to `target`. A run still open ends first, so the samples its resamplers
    /// still hold (owed for input already consumed) are placed where they belong, before
    /// the silence. Filling past an open run instead would leave its tail behind the lanes'
    /// end, to be dropped as late when the run flushes or continues.
    pub(super) fn fill_idle_to(&mut self, target: u64) {
        if target > self.lanes.end() {
            self.flush();
            self.fill_to(target);
        }
    }

    /// Ends the current run: the resamplers' tails are placed.
    pub(super) fn flush(&mut self) {
        let Some(run) = self.run.take() else {
            return;
        };
        if self.resamplers.is_empty() {
            return;
        }
        for (r, out) in self.resamplers.iter_mut().zip(&mut self.scratch_out) {
            out.clear();
            r.flush(out);
        }
        self.place(run.start + run.emitted as i64);
    }

    /// A device change: the run ends, the clock is new (ratio and skew reset) and the
    /// rate may change.
    pub(super) fn device_changed(&mut self, rate: SampleRate) -> Result<(), PipelineError> {
        self.flush();
        self.ratio = 1.0;
        self.skew = 0.0;
        self.configure(rate)
    }

    /// Places `scratch_out` at pipeline frame `pos`: silence before it, drop what lies
    /// before the lanes' end.
    fn place(&mut self, pos: i64) {
        let n = self.scratch_out[0].len();
        if n == 0 {
            return;
        }
        let end = self.lanes.end();
        let skip = if pos < 0 || (pos as u64) < end {
            let behind = (end as i128 - i128::from(pos)).min(n as i128) as usize;
            self.stats.late_dropped += behind as u64;
            behind
        } else {
            self.fill_to(pos as u64);
            0
        };
        for (lane, src) in self.lanes.data.iter_mut().zip(&self.scratch_out) {
            lane.extend_from_slice(&src[skip..]);
        }
    }

    /// Takes one packet: `frames` interleaved at the device's channel count, the first
    /// stamped `ts`. A run started by this packet is anchored `bias` pipeline samples
    /// before its stamp (the other track's current skew, so both tracks keep the same
    /// placement relative to the shared clock). Errors are queued as events.
    pub(super) fn push(
        &mut self,
        frames: &[Sample],
        ts: DeviceTime,
        origin: DeviceTime,
        bias: f64,
        d: &Derived,
        events: &mut Vec<PipelineEvent>,
    ) {
        let ch = self.channels;
        let whole = frames.len() / ch;
        if !frames.len().is_multiple_of(ch) {
            self.stats.partial_frames += 1;
        }
        let Some(rate) = self.rate else {
            self.stats.muted_frames += whole as u64;
            return;
        };
        if whole == 0 {
            return;
        }
        self.sanitise(&frames[..whole * ch]);

        let ts_pos =
            (i128::from(ts.ticks()) - i128::from(origin.ticks())) as f64 * SAMPLES_PER_TICK;
        if let Some(run) = &self.run {
            let contiguous = run.next_ts.is_some_and(|next| {
                (i128::from(ts.ticks()) - i128::from(next.ticks())).unsigned_abs()
                    <= u128::from(d.tolerance_ticks)
            });
            if !contiguous {
                self.flush();
            }
        }
        let mut run = match self.run {
            Some(run) => run,
            None => {
                // Anchor `bias` samples before the stamp: the other track's placement
                // lags its stamps by its skew, and alignment between the tracks (not
                // with the stamps) is what the canceller needs.
                let end = self.lanes.end() as i64;
                let mut pos = (ts_pos - bias).round() as i64;
                // max_gap <= MAX_GAP_LIMIT (validated), so this cannot wrap.
                let max_gap = d.max_gap as i64;
                let jump = pos.saturating_sub(end);
                if jump.unsigned_abs() > max_gap.unsigned_abs() {
                    events.push(PipelineEvent::Error(PipelineError::TimestampJump {
                        track: self.id,
                        gap: PIPELINE_RATE.duration_of(
                            usize::try_from(jump.unsigned_abs()).unwrap_or(usize::MAX),
                        ),
                        backwards: jump < 0,
                    }));
                    pos = end;
                }
                if self.started {
                    self.stats.restarts += 1;
                }
                self.started = true;
                // The run starts with the skew it inherits: measured below as `bias`.
                self.skew = bias;
                Run {
                    start: pos,
                    emitted: 0,
                    owed: 0.0,
                    ts_offset: pos as f64 + bias - ts_pos,
                    next_ts: Some(ts),
                }
            }
        };

        // Skew: where the timestamp says this packet belongs against where it is placed.
        let measured = ts_pos + run.ts_offset - (run.start as f64 + run.owed);
        let secs = whole as f64 / f64::from(rate.hz());
        let alpha = (secs / d.skew_smoothing_s).min(1.0);
        self.skew += alpha * (measured - self.skew);

        run.next_ts = ts.checked_after_frames(whole as u64, rate);
        run.owed += secs * self.ratio * f64::from(PIPELINE_RATE.hz());
        if self.resamplers.is_empty() {
            std::mem::swap(&mut self.scratch_in, &mut self.scratch_out);
        } else {
            for ((r, input), out) in self
                .resamplers
                .iter_mut()
                .zip(&self.scratch_in)
                .zip(&mut self.scratch_out)
            {
                out.clear();
                r.process(input, out);
            }
        }
        let pos = run.start + run.emitted as i64;
        run.emitted += self.scratch_out[0].len() as u64;
        self.run = Some(run);
        self.place(pos);
    }

    /// Deinterleaves whole frames into `scratch_in` (one lane per channel, or the mean of
    /// all channels for a single lane), replacing non-finite samples by zero and clamping
    /// the rest to [`SAMPLE_LIMIT`].
    fn sanitise(&mut self, frames: &[Sample]) {
        let ch = self.channels;
        let lanes = self.scratch_in.len();
        for lane in &mut self.scratch_in {
            lane.clear();
        }
        let (mut non_finite, mut clamped) = (0u64, 0u64);
        let mut clean = |x: Sample| {
            if !x.is_finite() {
                non_finite += 1;
                0.0
            } else if x.abs() > SAMPLE_LIMIT {
                clamped += 1;
                x.clamp(-SAMPLE_LIMIT, SAMPLE_LIMIT)
            } else {
                x
            }
        };
        let scale = 1.0 / ch as f32;
        for frame in frames.chunks_exact(ch) {
            if lanes == ch {
                for (lane, &x) in self.scratch_in.iter_mut().zip(frame) {
                    lane.push(clean(x));
                }
            } else {
                let sum: f32 = frame.iter().map(|&x| clean(x)).sum();
                self.scratch_in[0].push(if ch == 1 { sum } else { sum * scale });
            }
        }
        self.stats.non_finite += non_finite;
        self.stats.clamped += clamped;
    }
}
