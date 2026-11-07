//! Two-track call simulator for echo-canceller tests and acceptance (PLAN.md T1.2, 4.0).
//!
//! A far-end talker is rendered through a loudspeaker into a shoebox room and picked up by
//! the microphone together with a near-end talker and optional background noise:
//!
//! ```text
//! reference x (loopback, render clock)
//!   -> clock drift (resampled by 1 / (1 + ppm 1e-6) onto the capture clock)
//!   -> loudspeaker soft clip (optional)
//!   -> bulk delay (device and OS latency; may jump mid-call)
//!   -> room impulse response (image source; may change mid-call)
//!   -> echo, switched on/off for headphone scenarios
//! mic = echo + near-end (dry speech through its own room response) + noise
//! ```
//!
//! Stereo scenarios ([`Stereo`], T1.5b) render a left and a right channel through two
//! loudspeakers at different positions; the right channel is the left one (delayed by an
//! inter-channel lag) mixed with independent far-end speech, so the channels are partly
//! correlated as in a real stereo call. Their echoes add at the mic, and `reference` is
//! the mono downmix while `reference_lr` holds the two channels.
//!
//! Everything runs at the 16 kHz pipeline rate. Speech is passed in as buffers (tests and
//! the acceptance runner load the committed smoke fixtures); randomness (utterance order,
//! gaps, noise) comes from the caller's seeded RNG, so a scenario is deterministic.

mod rir;
pub mod rumble;

#[cfg(test)]
mod tests;

use std::fmt;
use std::ops::Range;

use rand_core::Rng;
use serde::Deserialize;

pub use rir::{Room, SPEED_OF_SOUND, direct_delay_samples, distance, image_source_rir};

use crate::audio::{PIPELINE_RATE, Resampler, SampleRate};
use crate::dsp::{fft_filter, power};

/// Active RMS the far-end reference is normalised to (-20 dBFS).
pub const REFERENCE_RMS: f64 = 0.1;

/// Parsed `acceptance/aec_scenarios.toml`.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScenarioFile {
    /// Room and positions shared by all scenarios.
    pub setup: Setup,
    /// Delay and drift sweep for `aec.robustness`.
    pub robustness: Robustness,
    /// The scenarios.
    pub scenario: Vec<Scenario>,
    /// Held-out evaluation set (T1.6b); `None` when the file has no `[heldout]` block.
    #[serde(default)]
    pub heldout: Option<HeldOut>,
}

/// Seed offset of the informational second-seed sweep of `accept m1` (T6.6a). Its runs have
/// been printed since T6.6a, so they are dev data (docs/aec-dev-split.md).
pub const SECOND_SEED_OFFSET: u64 = 1000;

/// Seed offset of the dev-wide sweep (T1.6c): every gating scenario rendered at this offset
/// with each speaker pair of `fixtures/librispeech/aec_dev.toml` (dev-clean voices). Dev data:
/// used for tuning and diagnosis. [`ScenarioFile::dev_seeds`] includes its seeds, so a
/// held-out offset can never render one.
pub const DEV_WIDE_SEED_OFFSET: u64 = 2000;

/// Held-out evaluation set of the echo canceller (T1.6b, adopted by the owner;
/// docs/aec-dev-split.md): every gating scenario re-rendered at each of `seed_offsets`, with
/// speech from a speaker pair the smoke set does not contain (pinned in
/// `fixtures/librispeech/aec_heldout.toml`). Never used for tuning.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HeldOut {
    /// Offsets added to each gating scenario's seed; one held-out run per offset.
    pub seed_offsets: Vec<u64>,
}

/// One held-out rendering of a gating scenario.
#[derive(Debug, Clone, PartialEq)]
pub struct HeldOutRun {
    /// Name of the gating scenario it re-renders.
    pub base: String,
    /// Seed offset applied.
    pub offset: u64,
    /// The scenario with the held-out seed (name `<base>@heldout+<offset>`).
    pub scenario: Scenario,
}

impl ScenarioFile {
    /// The scenario called `name`.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&Scenario> {
        self.scenario.iter().find(|s| s.name == name)
    }

    /// Every seed the dev set renders: each scenario's own seed (base runs and the
    /// robustness grid), that seed plus [`SECOND_SEED_OFFSET`] and plus
    /// [`DEV_WIDE_SEED_OFFSET`].
    #[must_use]
    pub fn dev_seeds(&self) -> std::collections::BTreeSet<u64> {
        self.scenario
            .iter()
            .flat_map(|s| {
                [
                    Some(s.seed),
                    s.seed.checked_add(SECOND_SEED_OFFSET),
                    s.seed.checked_add(DEV_WIDE_SEED_OFFSET),
                ]
            })
            .flatten()
            .collect()
    }

    /// The held-out runs: every gating scenario (selected by its `informational` field) at
    /// every held-out seed offset, in file order then offset order.
    ///
    /// # Errors
    /// [`SimError::InvalidScenario`] when the file has no `[heldout]` block, it lists no or
    /// duplicate offsets, a seed overflows, or a held-out seed is also a dev seed
    /// ([`Self::dev_seeds`]).
    pub fn heldout_runs(&self) -> Result<Vec<HeldOutRun>, SimError> {
        let bad = |m: String| Err(SimError::InvalidScenario(format!("heldout: {m}")));
        let Some(h) = &self.heldout else {
            return bad("the scenario file has no [heldout] block".into());
        };
        if h.seed_offsets.is_empty() {
            return bad("no seed offsets".into());
        }
        let mut seen = std::collections::BTreeSet::new();
        if let Some(d) = h.seed_offsets.iter().find(|o| !seen.insert(**o)) {
            return bad(format!("seed offset {d} listed twice"));
        }
        let dev = self.dev_seeds();
        let mut runs = Vec::new();
        for scn in self.scenario.iter().filter(|s| !s.informational) {
            for &offset in &h.seed_offsets {
                let Some(seed) = scn.seed.checked_add(offset) else {
                    return bad(format!("{} seed + {offset} overflows", scn.name));
                };
                if dev.contains(&seed) {
                    return bad(format!(
                        "{} at offset {offset} renders seed {seed}, which the dev set uses",
                        scn.name
                    ));
                }
                runs.push(HeldOutRun {
                    base: scn.name.clone(),
                    offset,
                    scenario: Scenario {
                        name: format!("{}@heldout+{offset}", scn.name),
                        seed,
                        ..scn.clone()
                    },
                });
            }
        }
        Ok(runs)
    }
}

/// Room geometry shared by all scenarios.
#[derive(Debug, Clone, Copy, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Setup {
    /// The room.
    pub room: Room,
    /// Microphone position (m).
    pub mic: [f64; 3],
    /// Loudspeaker position (m).
    pub speaker: [f64; 3],
    /// Loudspeaker position after a mid-call echo-path change (m).
    pub speaker_moved: [f64; 3],
    /// Near-end talker position (m).
    pub talker: [f64; 3],
    /// Room impulse response length (ms).
    pub rir_ms: f64,
}

/// The delay/drift grid over which `aec.robustness` re-runs the listed scenarios.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Robustness {
    /// Bulk delays (ms).
    pub delays_ms: Vec<f64>,
    /// Clock drifts (ppm).
    pub drifts_ppm: Vec<f64>,
    /// Scenario names to sweep.
    pub scenarios: Vec<String>,
}

/// A bulk-delay jump, for example when the output switches to Bluetooth.
#[derive(Debug, Clone, Copy, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DelayJump {
    /// Time of the jump (s).
    pub at_s: f64,
    /// Bulk delay after the jump (ms).
    pub delay_ms: f64,
}

/// A second loudspeaker and stereo far-end content (T1.5b).
#[derive(Debug, Clone, Copy, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Stereo {
    /// Right loudspeaker position (m); the left one is [`Setup::speaker`]. A mid-call
    /// echo-path change moves it by the same offset as the left one.
    pub right_speaker: [f64; 3],
    /// Weight of the left content in the right channel, in [0, 1]: 1 makes the right
    /// channel the delayed left channel, 0 makes it independent far-end speech.
    pub correlation: f64,
    /// Lag of the correlated part of the right channel behind the left (ms, >= 0).
    pub lag_ms: f64,
    /// Time (s) from which the far-end stereo image changes: the correlated part then
    /// lags by twice `lag_ms` at half the weight (a talker moving in the far-end room).
    #[serde(default)]
    pub image_change_s: Option<f64>,
}

fn yes() -> bool {
    true
}

fn default_echo_level() -> f64 {
    -30.0
}

/// One simulated call.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Scenario {
    /// Unique name.
    pub name: String,
    /// What the scenario exercises.
    #[serde(default)]
    pub description: String,
    /// Informational stress scenario: runners report it but never gate on it.
    #[serde(default)]
    pub informational: bool,
    /// Length (s).
    pub duration_s: f64,
    /// Seed for the caller's RNG (utterance order, gaps, noise).
    pub seed: u64,
    /// Far-end speech present (otherwise the reference is silent).
    #[serde(default = "yes")]
    pub far_speech: bool,
    /// Near-end speech present.
    #[serde(default)]
    pub near_speech: bool,
    /// When near-end speech starts (s).
    #[serde(default)]
    pub near_start_s: f64,
    /// Echo path exists (false for headphones).
    #[serde(default = "yes")]
    pub echo: bool,
    /// Echo appears at this time (s), for a headphones-to-speakers switch.
    #[serde(default)]
    pub echo_from_s: Option<f64>,
    /// Echo disappears at this time (s), for a speakers-to-headphones switch.
    #[serde(default)]
    pub echo_until_s: Option<f64>,
    /// Bulk delay from reference to loudspeaker output (ms).
    #[serde(default)]
    pub delay_ms: f64,
    /// Optional bulk-delay jump.
    #[serde(default)]
    pub delay_jump: Option<DelayJump>,
    /// Render clock relative to capture clock (ppm); positive = render runs fast.
    #[serde(default)]
    pub drift_ppm: f64,
    /// Loudspeaker soft clip `c tanh(v / c)` with `c` this many times the active RMS of
    /// the played signal; `None` = linear.
    #[serde(default)]
    pub softclip: Option<f64>,
    /// Active RMS of the echo at the mic (dBFS).
    #[serde(default = "default_echo_level")]
    pub echo_level_dbfs: f64,
    /// Signal-to-echo ratio: near-end active power over echo active power (dB).
    #[serde(default)]
    pub ser_db: f64,
    /// Background noise relative to the near-end (or echo, without near-end) level (dB).
    #[serde(default)]
    pub noise_snr_db: Option<f64>,
    /// Time of a loudspeaker move that changes the echo path (s).
    #[serde(default)]
    pub path_change_s: Option<f64>,
    /// Intervals `[from, to]` (s) in which the near end stays silent: an utterance that
    /// would overlap one starts at its end instead. Used to give every echo-path event an
    /// echo-only stretch ([`RECOVERY_WINDOW_S`]).
    #[serde(default)]
    pub near_silent_s: Vec<[f64; 2]>,
    /// Stereo far end through two loudspeakers; `None` = mono.
    #[serde(default)]
    pub stereo: Option<Stereo>,
}

/// Length of the echo-only stretch (far end active, near end silent) every echo-path event
/// (device switch, path change, delay jump) must be followed by: the 2 s recovery bound of
/// `aec.path_change` plus the 0.3 s tail mask of the time-to-15-dB metric, so recovery is
/// measurable.
pub const RECOVERY_WINDOW_S: f64 = 2.3;

impl Scenario {
    /// Times (s) of the echo-path events: echo appearing or disappearing (device switch),
    /// loudspeaker move, bulk-delay jump.
    #[must_use]
    pub fn events(&self) -> Vec<f64> {
        [
            self.echo_from_s,
            self.echo_until_s,
            self.path_change_s,
            self.delay_jump.map(|j| j.at_s),
        ]
        .into_iter()
        .flatten()
        .collect()
    }

    /// Copy for the robustness sweep whose bulk delay never drops below `delay_ms` and
    /// whose drift is `drift_ppm`. A fast render clock (positive ppm) shrinks the delay
    /// over the call, so that variant starts `drift_ppm * duration` later and reaches
    /// `delay_ms` at the end; otherwise it starts at `delay_ms`.
    #[must_use]
    pub fn with_delay_and_drift(&self, delay_ms: f64, drift_ppm: f64) -> Self {
        let shrink_ms = drift_ppm.max(0.0) * 1e-6 * self.duration_s * 1_000.0;
        Self {
            name: format!("{}@{delay_ms}ms{drift_ppm:+}ppm", self.name),
            delay_ms: delay_ms + shrink_ms,
            drift_ppm,
            ..self.clone()
        }
    }
}

/// Utterances to draw speech from, at the 16 kHz pipeline rate.
#[derive(Debug, Clone, Copy)]
pub struct SpeechPool<'a> {
    /// Far-end talker utterances.
    pub far: &'a [Vec<f32>],
    /// Near-end talker utterances.
    pub near: &'a [Vec<f32>],
}

/// Where the echo's main peak is, over time.
#[derive(Debug, Clone, PartialEq)]
pub struct DelayTruth {
    /// Render-vs-capture clock drift (ppm).
    pub drift_ppm: f64,
    /// Piecewise base delay (bulk + direct path), sorted by start sample.
    pub segments: Vec<DelaySegment>,
}

/// A stretch of constant bulk delay and echo path.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DelaySegment {
    /// First mic sample of the segment.
    pub start: usize,
    /// Bulk delay plus direct-path delay (samples).
    pub base: f64,
}

impl DelayTruth {
    /// Delay in samples between the reference and the echo's direct-path peak at mic
    /// sample `m`: echo sample `m` carries reference sample `(m - base)(1 + drift)`.
    #[must_use]
    pub fn delay_at(&self, m: usize) -> f64 {
        let base = self
            .segments
            .iter()
            .rev()
            .find(|s| s.start <= m)
            .or(self.segments.first())
            .map_or(0.0, |s| s.base);
        let eps = self.drift_ppm * 1e-6;
        let m = m as f64;
        m - (m - base) * (1.0 + eps)
    }
}

/// One utterance placed by the simulator.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlacedUtterance {
    /// Index of the clip in its speech pool.
    pub clip: usize,
    /// Samples it occupies (reference time for the far end, mic time for the near end).
    pub span: Range<usize>,
    /// The whole clip fits (false: cut off by the end of the call).
    pub complete: bool,
    /// Far-end channel: 0 for the mono or left reference, 1 for a stereo scenario's
    /// independent right-channel speech; 0 for the near end.
    pub channel: u8,
}

/// Simulator output; every signal has the same length.
#[derive(Debug, Clone, PartialEq)]
pub struct SimOutput {
    /// Sample rate of all signals (16 kHz).
    pub rate: SampleRate,
    /// Microphone: `echo + near_clean + noise`.
    pub mic: Vec<f32>,
    /// Far-end reference (loopback); the mono downmix `(L + R) / 2` for stereo scenarios.
    pub reference: Vec<f32>,
    /// Left and right reference channels of a stereo scenario.
    pub reference_lr: Option<[Vec<f32>; 2]>,
    /// Near-end speech as it reaches the mic (with its room response): the target.
    pub near_clean: Vec<f32>,
    /// Echo component of the mic.
    pub echo: Vec<f32>,
    /// Noise component of the mic.
    pub noise: Vec<f32>,
    /// Far-end speech regions (reference time, samples).
    pub far_regions: Vec<Range<usize>>,
    /// Near-end speech regions (mic time, samples; the dry utterance spans).
    pub near_regions: Vec<Range<usize>>,
    /// Every far-end utterance placed (reference time), in placement order per channel:
    /// which clip of [`SpeechPool::far`] and where (ground truth for transcript metrics).
    pub far_utterances: Vec<PlacedUtterance>,
    /// Every near-end utterance placed (mic time), in placement order.
    pub near_utterances: Vec<PlacedUtterance>,
    /// Intervals in which an echo path exists (mic time).
    pub echo_active: Vec<Range<usize>>,
    /// Ground-truth echo delay.
    pub delay: DelayTruth,
}

/// Simulation errors.
#[derive(Debug, Clone, PartialEq)]
pub enum SimError {
    /// A parameter is out of range.
    InvalidScenario(String),
    /// The scenario needs speech the pool does not have.
    EmptyPool(&'static str),
}

impl fmt::Display for SimError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidScenario(m) => write!(f, "invalid scenario: {m}"),
            Self::EmptyPool(which) => write!(f, "no {which} utterances in the speech pool"),
        }
    }
}

impl std::error::Error for SimError {}

fn uniform<R: Rng + ?Sized>(rng: &mut R) -> f64 {
    (rng.next_u64() >> 11) as f64 / (1u64 << 53) as f64
}

fn gaussian<R: Rng + ?Sized>(rng: &mut R) -> f64 {
    // Box-Muller.
    let u1 = 1.0 - uniform(rng);
    let u2 = uniform(rng);
    (-2.0 * u1.ln()).sqrt() * (2.0 * std::f64::consts::PI * u2).cos()
}

fn secs(rate: SampleRate, s: f64) -> usize {
    (s * f64::from(rate.hz())).round().max(0.0) as usize
}

/// Active RMS of `x` over `regions` (clipped to its length).
#[must_use]
pub fn active_rms(x: &[f32], regions: &[Range<usize>]) -> f64 {
    let (mut sum, mut count) = (0.0f64, 0usize);
    for r in regions {
        let r = r.start.min(x.len())..r.end.min(x.len());
        sum += x[r.clone()]
            .iter()
            .map(|&v| f64::from(v) * f64::from(v))
            .sum::<f64>();
        count += r.len();
    }
    if count == 0 {
        0.0
    } else {
        (sum / count as f64).sqrt()
    }
}

/// Concatenates utterances (each normalised to the same RMS) from a random start, with
/// random 0.2-0.8 s gaps, from `start` until `n` samples are filled.
fn place<R: Rng + ?Sized>(
    utts: &[Vec<f32>],
    start: usize,
    n: usize,
    silent: &[Range<usize>],
    rng: &mut R,
    channel: u8,
) -> (Vec<f32>, Vec<Range<usize>>, Vec<PlacedUtterance>) {
    let mut out = vec![0.0f32; n];
    let mut regions = Vec::new();
    let mut placed = Vec::new();
    let mut idx = (rng.next_u64() % utts.len() as u64) as usize;
    let mut pos = start;
    while pos < n {
        let u = &utts[idx % utts.len()];
        let end = pos + u.len().min(n - pos);
        if let Some(s) = silent.iter().find(|s| s.start < end && pos < s.end) {
            pos = s.end;
            continue;
        }
        let rms = power(u).sqrt();
        let g = if rms > 0.0 {
            (REFERENCE_RMS / rms) as f32
        } else {
            0.0
        };
        let take = u.len().min(n - pos);
        for (o, &v) in out[pos..pos + take].iter_mut().zip(u) {
            *o = v * g;
        }
        if take > 0 {
            regions.push(pos..pos + take);
            placed.push(PlacedUtterance {
                clip: idx % utts.len(),
                span: pos..pos + take,
                complete: take == u.len(),
                channel,
            });
        }
        let gap = secs(PIPELINE_RATE, 0.2 + 0.6 * uniform(rng));
        pos += take + gap;
        idx += 1;
    }
    (out, regions, placed)
}

fn scale(x: &mut [f32], g: f64) {
    for v in x {
        *v = (f64::from(*v) * g) as f32;
    }
}

fn shift(x: &[f32], by: usize) -> Vec<f32> {
    let mut y = vec![0.0; x.len()];
    if by < x.len() {
        y[by..].copy_from_slice(&x[..x.len() - by]);
    }
    y
}

/// Sorted union of two region lists.
fn merge_regions(a: Vec<Range<usize>>, b: Vec<Range<usize>>) -> Vec<Range<usize>> {
    let mut all: Vec<Range<usize>> = a.into_iter().chain(b).collect();
    all.sort_by_key(|r| r.start);
    let mut out: Vec<Range<usize>> = Vec::with_capacity(all.len());
    for r in all {
        match out.last_mut() {
            Some(last) if r.start <= last.end => last.end = last.end.max(r.end),
            _ => out.push(r),
        }
    }
    out
}

/// One loudspeaker channel on the capture clock: clock drift, then the optional soft clip
/// (its level from the channel's own active RMS over `regions`).
fn render(scn: &Scenario, x: &[f32], regions: &[Range<usize>]) -> Result<Vec<f32>, SimError> {
    let n = x.len();
    let rate = PIPELINE_RATE;
    let mut played = if scn.drift_ppm == 0.0 {
        x.to_vec()
    } else {
        let eps = scn.drift_ppm * 1e-6;
        let mut r = Resampler::new(rate, rate).map_err(|e| {
            SimError::InvalidScenario(format!("{}: drift resampler: {e}", scn.name))
        })?;
        r.set_relative_ratio(1.0 / (1.0 + eps))
            .map_err(|e| SimError::InvalidScenario(format!("{}: drift: {e}", scn.name)))?;
        let mut out = Vec::with_capacity(n + 1024);
        r.process(x, &mut out);
        r.flush(&mut out);
        out.resize(n, 0.0);
        out
    };
    if let Some(c) = scn.softclip {
        let level = c * active_rms(&played, regions);
        if level > 0.0 {
            for v in &mut played {
                *v = (level * (f64::from(*v) / level).tanh()) as f32;
            }
        }
    }
    Ok(played)
}

/// Far-end regions (reference time) as they arrive at the mic: within each delay segment
/// of `delay`, shifted by that segment's delay (drift ignored), clipped to the segment and
/// to `n` samples.
#[must_use]
pub fn arrived_regions(
    far_regions: &[Range<usize>],
    delay: &DelayTruth,
    n: usize,
) -> Vec<Range<usize>> {
    let mut out = Vec::new();
    for (i, seg) in delay.segments.iter().enumerate() {
        let end = delay.segments.get(i + 1).map_or(n, |s| s.start).min(n);
        let base = seg.base.round().max(0.0) as usize;
        for r in far_regions {
            let a = (r.start + base).max(seg.start);
            let b = (r.end + base).min(end);
            if a < b {
                out.push(a..b);
            }
        }
    }
    out
}

/// Checks the scenario validity rule on a simulated call: every echo-path event of `scn`
/// ([`Scenario::events`]) is followed by an echo-only stretch of [`RECOVERY_WINDOW_S`] in
/// which the far end talks (far-end speech covers at least half of it) and the near end is
/// silent (its signal at the mic, room tail included, stays 60 dB below its active level).
/// Returns one message per violation; empty when the scenario is valid.
#[must_use]
pub fn recovery_window_violations(scn: &Scenario, out: &SimOutput) -> Vec<String> {
    let rate = out.rate;
    let n = out.mic.len();
    let near_level = active_rms(&out.near_clean, &out.near_regions).powi(2);
    let mut bad = Vec::new();
    for t in scn.events() {
        let w = secs(rate, t).min(n)..secs(rate, t + RECOVERY_WINDOW_S).min(n);
        if w.len() < secs(rate, RECOVERY_WINDOW_S) {
            bad.push(format!(
                "{}: event at {t} s leaves less than the window",
                scn.name
            ));
            continue;
        }
        // Far-end speech as it arrives at the mic (each delay segment's bulk delay).
        let far: usize = arrived_regions(&out.far_regions, &out.delay, n)
            .iter()
            .map(|r| r.end.min(w.end).saturating_sub(r.start.max(w.start)))
            .sum();
        if 2 * far < w.len() {
            bad.push(format!(
                "{}: far end talks only {:.0}% of {t}..{:.1} s",
                scn.name,
                100.0 * far as f64 / w.len() as f64,
                t + RECOVERY_WINDOW_S
            ));
        }
        if power(&out.near_clean[w.clone()]) > near_level * 1e-6 {
            bad.push(format!(
                "{}: near end is not silent in {t}..{:.1} s",
                scn.name,
                t + RECOVERY_WINDOW_S
            ));
        }
    }
    bad
}

/// Runs `scn` in `setup` with speech from `pool` and randomness from `rng`.
///
/// # Errors
/// [`SimError::InvalidScenario`] for a non-positive duration, a negative delay, drift
/// beyond the resampler's range, event times outside the call, or a fast render clock
/// that would drive the bulk delay below zero;
/// [`SimError::EmptyPool`] if speech is required but the pool is empty.
pub fn simulate<R: Rng + ?Sized>(
    scn: &Scenario,
    setup: &Setup,
    pool: &SpeechPool<'_>,
    rng: &mut R,
) -> Result<SimOutput, SimError> {
    let rate = PIPELINE_RATE;
    let bad = |m: String| Err(SimError::InvalidScenario(format!("{}: {m}", scn.name)));
    if !(scn.duration_s > 0.0 && scn.duration_s.is_finite()) {
        return bad("duration must be positive".into());
    }
    let mut delays = vec![scn.delay_ms];
    if let Some(j) = scn.delay_jump {
        delays.push(j.delay_ms);
    }
    if delays.iter().any(|d| !(d.is_finite() && *d >= 0.0)) {
        return bad("delays must be >= 0 ms".into());
    }
    if scn.drift_ppm.is_nan() || scn.drift_ppm.abs() > 5_000.0 {
        return bad("drift must be within +/-5000 ppm".into());
    }
    if scn
        .near_silent_s
        .iter()
        .any(|[a, b]| !(*a >= 0.0 && a < b && *b <= scn.duration_s))
    {
        return bad("near-silent intervals must be ordered and lie within the call".into());
    }
    if scn
        .events()
        .iter()
        .any(|&t| !(t >= 0.0 && t <= scn.duration_s))
    {
        return bad("event times must lie within the call".into());
    }
    if scn.far_speech && pool.far.is_empty() {
        return Err(SimError::EmptyPool("far-end"));
    }
    if scn.near_speech && pool.near.is_empty() {
        return Err(SimError::EmptyPool("near-end"));
    }
    if let Some(st) = scn.stereo {
        if !((0.0..=1.0).contains(&st.correlation) && st.lag_ms >= 0.0 && st.lag_ms < 50.0) {
            return bad("stereo correlation must be in [0, 1] and lag in [0, 50) ms".into());
        }
        if st
            .image_change_s
            .is_some_and(|t| !(t >= 0.0 && t <= scn.duration_s))
        {
            return bad("event times must lie within the call".into());
        }
    }

    let n = secs(rate, scn.duration_s);
    let near_silent: Vec<Range<usize>> = scn
        .near_silent_s
        .iter()
        .map(|[a, b]| secs(rate, *a)..secs(rate, *b))
        .collect();
    let (left, mut far_regions, mut far_utterances) = if scn.far_speech {
        place(pool.far, 0, n, &[], rng, 0)
    } else {
        (vec![0.0; n], Vec::new(), Vec::new())
    };
    let (near_dry, near_regions, near_utterances) = if scn.near_speech {
        place(
            pool.near,
            secs(rate, scn.near_start_s),
            n,
            &near_silent,
            rng,
            0,
        )
    } else {
        (vec![0.0; n], Vec::new(), Vec::new())
    };

    // Right channel of a stereo far end: the (lagged) left content plus independent
    // far-end speech.
    let right = match scn.stereo {
        Some(st) if scn.far_speech => {
            let (other, other_regions, other_utterances) = place(pool.far, 0, n, &[], rng, 1);
            far_utterances.extend(other_utterances);
            let lag = secs(rate, st.lag_ms / 1_000.0);
            let change = st.image_change_s.map_or(n, |t| secs(rate, t).min(n));
            let (near_lag, far_lag) = (shift(&left, lag), shift(&left, 2 * lag));
            let c = st.correlation;
            let d = (1.0 - c * c).sqrt();
            far_regions = merge_regions(far_regions, other_regions);
            Some(
                (0..n)
                    .map(|i| {
                        let corr = if i < change {
                            c * f64::from(near_lag[i])
                        } else {
                            0.5 * c * f64::from(far_lag[i])
                        };
                        (corr + d * f64::from(other[i])) as f32
                    })
                    .collect::<Vec<f32>>(),
            )
        }
        Some(_) => Some(vec![0.0; n]),
        None => None,
    };
    let reference = match &right {
        Some(r) => left.iter().zip(r).map(|(a, b)| 0.5 * (a + b)).collect(),
        None => left.clone(),
    };

    // Loudspeaker outputs on the capture clock.
    let played = render(scn, &left, &far_regions)?;
    let played_right = match &right {
        Some(r) => Some(render(scn, r, &far_regions)?),
        None => None,
    };

    // Piecewise-constant (bulk delay, echo path) segments in mic time.
    let rir_len = secs(rate, setup.rir_ms / 1_000.0).max(1);
    let rir_main = image_source_rir(&setup.room, setup.speaker, setup.mic, rate, rir_len);
    let rir_moved = image_source_rir(&setup.room, setup.speaker_moved, setup.mic, rate, rir_len);
    let right_rirs = scn.stereo.map(|st| {
        let moved: [f64; 3] = std::array::from_fn(|k| {
            st.right_speaker[k] + setup.speaker_moved[k] - setup.speaker[k]
        });
        (
            image_source_rir(&setup.room, st.right_speaker, setup.mic, rate, rir_len),
            image_source_rir(&setup.room, moved, setup.mic, rate, rir_len),
        )
    });
    let mut cuts = vec![0usize, n];
    if let Some(t) = scn.path_change_s {
        cuts.push(secs(rate, t).min(n));
    }
    if let Some(j) = scn.delay_jump {
        cuts.push(secs(rate, j.at_s).min(n));
    }
    cuts.sort_unstable();
    cuts.dedup();
    let mut echo = vec![0.0f32; n];
    let mut segments = Vec::new();
    for w in cuts.windows(2) {
        let (s0, s1) = (w[0], w[1]);
        if s0 == s1 {
            continue;
        }
        let t0 = s0 as f64 / f64::from(rate.hz());
        let moved = scn.path_change_s.is_some_and(|t| t0 >= t);
        let delay_ms = match scn.delay_jump {
            Some(j) if t0 >= j.at_s => j.delay_ms,
            _ => scn.delay_ms,
        };
        let bulk = secs(rate, delay_ms / 1_000.0);
        let (rir, spk) = if moved {
            (&rir_moved, setup.speaker_moved)
        } else {
            (&rir_main, setup.speaker)
        };
        let mut e = fft_filter(&shift(&played, bulk), rir);
        if let (Some(pr), Some((main, moved_r))) = (&played_right, &right_rirs) {
            let er = fft_filter(&shift(pr, bulk), if moved { moved_r } else { main });
            for (a, b) in e.iter_mut().zip(&er) {
                *a += b;
            }
        }
        echo[s0..s1].copy_from_slice(&e[s0..s1]);
        let base = bulk as f64 + direct_delay_samples(spk, setup.mic, rate);
        // With drift the bulk delay at mic sample m is bulk - eps (m - base); a fast render
        // clock must not drive it below zero (the echo cannot precede its reference).
        let bulk_at_end = bulk as f64 - scn.drift_ppm * 1e-6 * ((s1 - 1) as f64 - base);
        if bulk_at_end < -0.5 {
            return bad(format!(
                "drift makes the bulk delay negative ({:.2} ms at {:.1} s)",
                bulk_at_end * 1_000.0 / f64::from(rate.hz()),
                s1 as f64 / f64::from(rate.hz())
            ));
        }
        segments.push(DelaySegment { start: s0, base });
    }
    let delay = DelayTruth {
        drift_ppm: scn.drift_ppm,
        segments,
    };

    // Echo level: active RMS over the far-end regions as they arrive at the mic.
    let echo_target = 10f64.powf(scn.echo_level_dbfs / 20.0);
    let arrived = arrived_regions(&far_regions, &delay, n);
    let echo_rms = active_rms(&echo, &arrived);
    if echo_rms > 0.0 {
        scale(&mut echo, echo_target / echo_rms);
    }
    let on = if scn.echo {
        secs(rate, scn.echo_from_s.unwrap_or(0.0)).min(n)
            ..secs(rate, scn.echo_until_s.unwrap_or(scn.duration_s)).min(n)
    } else {
        0..0
    };
    for (i, v) in echo.iter_mut().enumerate() {
        if !on.contains(&i) {
            *v = 0.0;
        }
    }
    let echo_active = if on.is_empty() { Vec::new() } else { vec![on] };

    // Near-end through its own (short-range) room response.
    let rir_talker = image_source_rir(&setup.room, setup.talker, setup.mic, rate, rir_len);
    let mut near_clean = fft_filter(&near_dry, &rir_talker);
    let near_target = echo_target * 10f64.powf(scn.ser_db / 20.0);
    let near_rms = active_rms(&near_clean, &near_regions);
    if near_rms > 0.0 {
        scale(&mut near_clean, near_target / near_rms);
    }

    // Pink-ish background noise (white through a one-pole low-pass).
    let mut noise = vec![0.0f32; n];
    if let Some(snr) = scn.noise_snr_db {
        let mut state = 0.0f64;
        for v in &mut noise {
            state = 0.9 * state + gaussian(rng);
            *v = state as f32;
        }
        let level = if scn.near_speech {
            near_target
        } else {
            echo_target
        };
        let rms = power(&noise).sqrt();
        if rms > 0.0 {
            scale(&mut noise, level * 10f64.powf(-snr / 20.0) / rms);
        }
    }

    let mic = echo
        .iter()
        .zip(&near_clean)
        .zip(&noise)
        .map(|((e, s), w)| e + s + w)
        .collect();
    let reference_lr = right.map(|r| [left, r]);
    Ok(SimOutput {
        rate,
        mic,
        reference,
        reference_lr,
        near_clean,
        echo,
        noise,
        far_regions,
        near_regions,
        far_utterances,
        near_utterances,
        echo_active,
        delay,
    })
}
