//! The audio check (T7.4j): optional, from Settings (and later first run), never during a
//! meeting (the app's `WorkGate` keeps it apart from meetings, the model check and the voice
//! sample). It plays the chirps of `earshot_core::audio_check` through the chosen output
//! while the chosen microphone and that output's loopback are captured on one device clock,
//! then judges the speaker-to-mic delay, the echo level, headphones (no echo), a too quiet
//! mic or echo, and clipping.
//!
//! - [`measure`] drives any [`AudioSource`] with a reference track (the WASAPI capture in the
//!   app, a replay in tests): both tracks are placed on the session's timeline by their
//!   device timestamps (a loopback delivers nothing while nothing plays, so it starts late),
//!   resampled to 16 kHz and handed to `audio_check::analyse`. The audio is held in memory
//!   on the calling thread only and dropped there.
//! - [`run_check`] (Windows) is the production run: the capture on the Audio page's devices
//!   (a device loopback of the chosen output) and a render thread playing the chirps on the
//!   output that loopback opened.
//! - The result is stored device-locally per microphone + output pair, by the endpoint ids
//!   actually opened (a "default" choice is stored under the device Windows used then), in
//!   the one record `audio-check` (`version u32 LE | JSON list` in the order written, at
//!   most [`MAX_PAIRS`], the most recently written kept whatever the wall clock did; never
//!   synced). A record this version cannot read is reported and treated
//!   as empty, never an error.
//! - At meeting start ([`delay_hint_for`], used by `backends::start_wasapi_session_on`) the
//!   stored delay of the pair the meeting opened seeds the delay estimator through
//!   `PipelineConfig::delay_hint` (a hint only seeds the search; the canceller never cancels
//!   without live evidence). With process loopback (a call app) the pair's output is the
//!   output the Audio page names (the chosen one when connected, else the Windows default),
//!   which is not necessarily the device the audio engine mixes the call app into (the app
//!   may play on another output of its own choosing), and the app's stream may reach the
//!   process loopback a few ms apart from the device mix the check measured. Either way the
//!   hint then does not match a live candidate and the search runs as it would without one.

use std::num::NonZeroU16;
use std::time::Duration;

use earshot_core::asr::CancelToken;
use earshot_core::audio::{PIPELINE_RATE, SampleRate, resample_all};
use earshot_core::audio_check::{self as core_check, CheckResult, ClipDetector, Outcome};
use earshot_core::capture::{
    AudioSource, CaptureSink, DeviceTime, Discontinuity, EndOfStream, SourceStatus, TrackId,
    mix_to_mono,
};
use earshot_store::Store;
use serde::{Deserialize, Serialize};

use crate::RuntimeError;
use crate::local::DeviceRecord;

#[cfg(test)]
#[path = "audio_check_tests.rs"]
mod tests;

/// `local_state` key of the stored results.
pub const AUDIO_CHECK_KEY: &str = "audio-check";
/// Version of the stored record (outside the payload).
pub const RECORD_VERSION: u32 = 1;
/// Pairs kept (the newest).
pub const MAX_PAIRS: usize = 16;
/// Capture continues this long after the playback ended, so the last chirp's echo arrives
/// whatever the delay up to `audio_check::MAX_LAG`.
pub const AFTER_PLAYBACK: Duration = Duration::from_millis(1_300);
/// A capture stops after this much mic audio whatever the playback does (a playback that
/// never ends); a stamp beyond it is not believed.
pub const MAX_CAPTURE: Duration = Duration::from_secs(12);
/// Packets within this of the previous packet's end continue it (device jitter), as the
/// pipeline's intake does.
const GAP_TOLERANCE_TICKS: u64 = 40_000;

/// One endpoint of a checked pair.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckedEndpoint {
    /// `IMMDevice::GetId`.
    pub id: String,
    /// Its name when checked.
    pub name: String,
}

/// What a stored check found.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum StoredOutcome {
    /// The mic heard the chirps.
    Echo {
        /// Delay (samples at 16 kHz).
        delay_samples: f64,
        /// Echo over played power (dB).
        echo_db: f64,
        /// Echo level at the mic (dBFS).
        echo_dbfs: f64,
    },
    /// Played, but no echo heard (headphones).
    NoEcho,
    /// Nothing was played on the output.
    NotPlayed,
    /// The mic delivered silence.
    MicSilent,
}

/// One stored result.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StoredCheck {
    /// The microphone opened.
    pub microphone: CheckedEndpoint,
    /// The output played on (and whose loopback was recorded).
    pub output: CheckedEndpoint,
    /// When it ran (Unix ms).
    pub checked_at_ms: u64,
    /// What it found.
    pub outcome: StoredOutcome,
    /// Too quiet (`audio_check::CheckResult::too_quiet`).
    pub too_quiet: bool,
    /// The mic clipped.
    pub clipping: bool,
}

impl StoredCheck {
    /// The stored form of `result` for the pair, run at `at_ms`.
    #[must_use]
    pub fn new(
        microphone: CheckedEndpoint,
        output: CheckedEndpoint,
        at_ms: u64,
        result: &CheckResult,
    ) -> Self {
        let outcome = match result.outcome {
            Outcome::Echo {
                delay_samples,
                echo_db,
                echo_dbfs,
            } => StoredOutcome::Echo {
                delay_samples,
                echo_db,
                echo_dbfs,
            },
            Outcome::NoEcho => StoredOutcome::NoEcho,
            Outcome::NotPlayed => StoredOutcome::NotPlayed,
            Outcome::MicSilent => StoredOutcome::MicSilent,
        };
        Self {
            microphone,
            output,
            checked_at_ms: at_ms,
            outcome,
            too_quiet: result.too_quiet,
            clipping: result.clipping,
        }
    }

    /// The delay in ms when an echo was heard.
    #[must_use]
    pub fn delay_ms(&self) -> Option<f64> {
        match self.outcome {
            StoredOutcome::Echo { delay_samples, .. } => {
                Some(delay_samples * 1000.0 / f64::from(PIPELINE_RATE.hz()))
            }
            _ => None,
        }
    }

    /// The warm-start hint for a canceller searching up to `max_delay` samples
    /// (`audio_check::usable_hint`).
    #[must_use]
    pub fn delay_hint(&self, max_delay: usize) -> Option<f64> {
        match self.outcome {
            StoredOutcome::Echo { delay_samples, .. } => {
                core_check::usable_hint(delay_samples, self.too_quiet, max_delay)
            }
            _ => None,
        }
    }

    /// Whether this is the result of the pair `mic` + `output` (endpoint ids).
    #[must_use]
    pub fn is_pair(&self, mic: &str, output: &str) -> bool {
        self.microphone.id == mic && self.output.id == output
    }
}

/// The stored results.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct StoredChecks {
    /// Every readable result, in the order written (oldest first).
    pub checks: Vec<StoredCheck>,
    /// A record was stored but this version cannot read it (treated as empty).
    pub unreadable: bool,
}

impl StoredChecks {
    /// The result of the pair `mic` + `output`, if any.
    #[must_use]
    pub fn pair(&self, mic: &str, output: &str) -> Option<&StoredCheck> {
        // A pair is stored at most once; the last match is the latest write.
        self.checks.iter().rev().find(|c| c.is_pair(mic, output))
    }
}

fn decode(bytes: &[u8]) -> Option<Vec<StoredCheck>> {
    let (version, payload) = bytes.split_first_chunk::<4>()?;
    if u32::from_le_bytes(*version) != RECORD_VERSION {
        return None;
    }
    serde_json::from_slice(payload).ok()
}

fn encode(checks: &[StoredCheck]) -> Result<Vec<u8>, RuntimeError> {
    let mut out = RECORD_VERSION.to_le_bytes().to_vec();
    serde_json::to_writer(&mut out, checks)
        .map_err(|_| RuntimeError::Internal("cannot encode the audio check record"))?;
    Ok(out)
}

/// The stored results.
///
/// # Errors
/// Store errors (an unreadable record is [`StoredChecks::unreadable`], never an error).
pub fn stored_checks(store: &Store) -> Result<StoredChecks, RuntimeError> {
    let Some(bytes) = store.local_value(AUDIO_CHECK_KEY)? else {
        return Ok(StoredChecks::default());
    };
    Ok(match decode(&bytes) {
        Some(checks) => StoredChecks {
            checks,
            unreadable: false,
        },
        None => StoredChecks {
            checks: Vec::new(),
            unreadable: true,
        },
    })
}

/// Stores `check`, replacing the earlier result of its pair, and keeps the [`MAX_PAIRS`]
/// most recently written (by write order, never by `checked_at_ms`, so `check` itself is
/// always kept even when the wall clock stepped back; an unreadable earlier record is
/// replaced).
///
/// # Errors
/// Store errors.
pub fn record_check(store: &mut Store, check: StoredCheck) -> Result<(), RuntimeError> {
    let mut checks = stored_checks(store)?.checks;
    checks.retain(|c| !c.is_pair(&check.microphone.id, &check.output.id));
    checks.push(check);
    let excess = checks.len().saturating_sub(MAX_PAIRS);
    checks.drain(..excess);
    crate::local::set(store, &DeviceRecord::AudioCheck.key(), &encode(&checks)?)?;
    Ok(())
}

/// The stored delay hint of the pair `mic` + `output` (endpoint ids) for a canceller
/// searching up to `max_delay` samples; `None` without a usable result (module docs).
///
/// # Errors
/// Store errors.
pub fn delay_hint_for(
    store: &Store,
    mic: &str,
    output: &str,
    max_delay: usize,
) -> Result<Option<f64>, RuntimeError> {
    Ok(stored_checks(store)?
        .pair(mic, output)
        .and_then(|c| c.delay_hint(max_delay)))
}

/// One track placed on the session timeline by its device timestamps, mono.
struct Placed {
    rate: SampleRate,
    channels: NonZeroU16,
    samples: Vec<f32>,
    /// Device time just past the last frame placed.
    end: Option<DeviceTime>,
    scratch: Vec<f32>,
}

impl Placed {
    fn new(rate: SampleRate, channels: NonZeroU16) -> Self {
        Self {
            rate,
            channels,
            samples: Vec::new(),
            end: None,
            scratch: Vec::new(),
        }
    }

    /// Places `frames` stamped `ts`, keeping at most `cap` frames; `false` when the stamp
    /// lies past `cap` frames (not believed).
    fn push(&mut self, origin: DeviceTime, frames: &[f32], ts: DeviceTime, cap: usize) -> bool {
        self.scratch.clear();
        let n = mix_to_mono(frames, self.channels, &mut self.scratch);
        let continuing = self
            .end
            .is_some_and(|e| e.ticks().abs_diff(ts.ticks()) <= GAP_TOLERANCE_TICKS);
        let mut skip = 0usize;
        if !continuing {
            let ticks = i128::from(ts.ticks()) - i128::from(origin.ticks());
            let pos = (ticks * i128::from(self.rate.hz()) + 5_000_000).div_euclid(10_000_000);
            if pos > cap as i128 {
                return false;
            }
            let len = self.samples.len() as i128;
            if pos >= len {
                self.samples
                    .resize(usize::try_from(pos).unwrap_or(cap), 0.0);
            } else {
                // Overlap (or a stamp before the origin): positions already written win.
                skip = usize::try_from(len - pos).unwrap_or(usize::MAX).min(n);
            }
        }
        let room = cap.saturating_sub(self.samples.len());
        let take = (n - skip).min(room);
        self.samples
            .extend_from_slice(&self.scratch[skip..skip + take]);
        self.end = Some(ts.after_frames(n as u64, self.rate));
        true
    }
}

struct CheckSink {
    origin: DeviceTime,
    mic: Placed,
    reference: Placed,
    clip: ClipDetector,
    mic_cap: usize,
    reference_cap: usize,
    /// A discontinuity or an unbelievable stamp: the alignment cannot be trusted.
    broken: bool,
    ended: bool,
}

impl CaptureSink for CheckSink {
    fn frames(&mut self, track: TrackId, frames: &[f32], device_ts: DeviceTime) {
        if self.broken {
            return;
        }
        let ok = match track {
            TrackId::Mic => {
                self.clip.push(frames);
                self.mic.push(self.origin, frames, device_ts, self.mic_cap)
            }
            TrackId::Reference => {
                self.reference
                    .push(self.origin, frames, device_ts, self.reference_cap)
            }
        };
        if !ok {
            self.broken = true;
        }
    }

    fn discontinuity(&mut self, _: TrackId, _: Discontinuity) {
        // Lost data or another device: the delay measured across it would mean nothing.
        self.broken = true;
    }

    fn end(&mut self, _: EndOfStream) {
        self.ended = true;
    }
}

fn to_pipeline_rate(x: &[f32], rate: SampleRate) -> Result<Vec<f32>, RuntimeError> {
    if rate == PIPELINE_RATE {
        return Ok(x.to_vec());
    }
    resample_all(x, rate, PIPELINE_RATE)
        .map_err(|e| RuntimeError::Capture(format!("cannot resample the check's audio: {e}")))
}

/// Captures `source` while the chirps play, then judges it. `playback_done` says whether
/// the playback has ended (capture then continues for [`AFTER_PLAYBACK`] of mic audio);
/// `idle` is called when the source has nothing ready (production: a short sleep);
/// `progress` after each delivery with the mic audio captured so far. A source that ends by
/// itself (a replay) is judged on what it delivered. Returns `None` when `cancel` fired (the
/// source is stopped and drained first, and the audio dropped).
///
/// # Errors
/// [`RuntimeError::Capture`] for a source error, a source without a reference (output)
/// track, a break in either track (a device changed or lost data: run the check again) or a
/// resampling error.
pub fn measure<A: AudioSource>(
    source: &mut A,
    playback_done: &mut dyn FnMut() -> bool,
    cancel: &CancelToken,
    idle: &mut dyn FnMut(),
    progress: &mut dyn FnMut(Duration),
) -> Result<Option<CheckResult>, RuntimeError> {
    measure_up_to(source, MAX_CAPTURE, playback_done, cancel, idle, progress)
}

/// [`measure`] keeping up to `max_capture` of each track (tests over longer recordings).
pub(crate) fn measure_up_to<A: AudioSource>(
    source: &mut A,
    max_capture: Duration,
    playback_done: &mut dyn FnMut() -> bool,
    cancel: &CancelToken,
    idle: &mut dyn FnMut(),
    progress: &mut dyn FnMut(Duration),
) -> Result<Option<CheckResult>, RuntimeError> {
    let session = source.session().clone();
    let reference = session.reference.clone().ok_or_else(|| {
        RuntimeError::Capture("the check needs the output's sound and none was captured".into())
    })?;
    let mic_rate = session.mic.rate;
    let mut sink = CheckSink {
        origin: session.origin,
        mic: Placed::new(mic_rate, session.mic.channels),
        reference: Placed::new(reference.rate, reference.channels),
        clip: ClipDetector::new(),
        mic_cap: mic_rate.frames_for(max_capture),
        reference_cap: reference.rate.frames_for(max_capture),
        broken: false,
        ended: false,
    };
    let after = mic_rate.frames_for(AFTER_PLAYBACK);
    let mut stop_at: Option<usize> = None;
    let mut stopping = false;
    let mut cancelled = false;
    while !sink.ended {
        if !stopping {
            let held = sink.mic.samples.len();
            if stop_at.is_none() && playback_done() {
                stop_at = Some(held + after);
            }
            if cancel.is_cancelled() {
                cancelled = true;
            }
            let full = held >= sink.mic_cap;
            if cancelled || sink.broken || full || stop_at.is_some_and(|s| held >= s) {
                source.stop();
                stopping = true;
            }
        }
        match source.pump(&mut sink) {
            Ok(SourceStatus::Delivered) => {
                progress(mic_rate.duration_of(sink.mic.samples.len()));
            }
            Ok(SourceStatus::Idle) => idle(),
            Ok(SourceStatus::Ended) => break,
            Err(e) => {
                source.stop();
                return Err(RuntimeError::Capture(format!("the capture failed: {e:?}")));
            }
        }
    }
    if cancelled {
        return Ok(None);
    }
    if sink.broken {
        return Err(RuntimeError::Capture(
            "the microphone or the output changed or lost sound during the check; run it again"
                .into(),
        ));
    }
    let mic = to_pipeline_rate(&sink.mic.samples, mic_rate)?;
    let played = to_pipeline_rate(&sink.reference.samples, reference.rate)?;
    Ok(Some(core_check::analyse(
        &played,
        &mic,
        sink.clip.clipped(),
    )))
}

/// The playback thread of a check: [`finish`](Self::finish) stops and joins it; dropped
/// without that (a panic while measuring) it still sets the stop flag and joins, so the
/// sounds never keep playing past the check.
pub(crate) struct Playback {
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    thread: Option<std::thread::JoinHandle<Result<(), String>>>,
}

impl Playback {
    /// Runs `play` on a thread named `earshot-audio-check-play`; `play` must return soon
    /// after the flag it is given is set.
    pub(crate) fn spawn(
        play: impl FnOnce(&std::sync::atomic::AtomicBool) -> Result<(), String> + Send + 'static,
    ) -> Result<Self, RuntimeError> {
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = std::sync::Arc::clone(&stop);
        let thread = std::thread::Builder::new()
            .name("earshot-audio-check-play".into())
            .spawn(move || play(&flag))
            .map_err(RuntimeError::Spawn)?;
        Ok(Self {
            stop,
            thread: Some(thread),
        })
    }

    /// Whether the playback has ended by itself.
    pub(crate) fn is_finished(&self) -> bool {
        self.thread
            .as_ref()
            .is_none_or(std::thread::JoinHandle::is_finished)
    }

    /// Stops the playback and waits for it; its result.
    pub(crate) fn finish(mut self) -> Result<(), String> {
        self.stop_and_join()
    }

    fn stop_and_join(&mut self) -> Result<(), String> {
        self.stop.store(true, std::sync::atomic::Ordering::Release);
        match self.thread.take() {
            Some(t) => t
                .join()
                .unwrap_or_else(|_| Err("the playback thread stopped unexpectedly".into())),
            None => Ok(()),
        }
    }
}

impl Drop for Playback {
    fn drop(&mut self) {
        let _ = self.stop_and_join();
    }
}

/// A finished production run: the pair the capture opened and what was found.
#[derive(Debug, Clone)]
pub struct Measured {
    /// The microphone opened.
    pub microphone: CheckedEndpoint,
    /// The output played on.
    pub output: CheckedEndpoint,
    /// The verdict.
    pub result: CheckResult,
    /// Chosen endpoints that were not connected and were replaced by the default.
    #[cfg(windows)]
    pub fallbacks: Vec<earshot_capture_wasapi::DeviceFallback>,
}

impl Measured {
    /// A run of `microphone` and `output` that found `result`, with no fallback (hosts that
    /// measure through [`measure`] themselves, tests).
    #[must_use]
    pub fn new(microphone: CheckedEndpoint, output: CheckedEndpoint, result: CheckResult) -> Self {
        Self {
            microphone,
            output,
            result,
            #[cfg(windows)]
            fallbacks: Vec::new(),
        }
    }

    /// The stored form, run at `at_ms`.
    #[must_use]
    pub fn stored(&self, at_ms: u64) -> StoredCheck {
        StoredCheck::new(
            self.microphone.clone(),
            self.output.clone(),
            at_ms,
            &self.result,
        )
    }
}

/// The production check (module docs): plays the chirps on the output of `devices` and
/// captures it and the microphone. Returns `None` when cancelled (playback stops at once).
///
/// # Errors
/// [`RuntimeError::Capture`] when a device cannot be opened, identified or played on, or as
/// [`measure`].
#[cfg(windows)]
pub fn run_check(
    devices: &earshot_capture_wasapi::DeviceSelection,
    cancel: &CancelToken,
    progress: &mut dyn FnMut(Duration),
) -> Result<Option<Measured>, RuntimeError> {
    use earshot_capture_wasapi::wasapi::{ComApartment, RenderStream, WasapiSource};
    use earshot_capture_wasapi::{CaptureMode, LoopbackRequest, MicRole};

    let capture = |e: String| RuntimeError::Capture(e);
    let mut source = WasapiSource::start_on(
        MicRole::default(),
        &CaptureMode::Call(LoopbackRequest::DefaultDevice),
        devices,
    )
    .map_err(|e| crate::capture_start_error(&e))?;
    let mic_id = source.mic_endpoint().and_then(|p| p.id.clone());
    let output_id = source.output_endpoint_id().map(str::to_owned);
    let (Some(mic_id), Some(output_id)) = (mic_id, output_id) else {
        source.stop();
        return Err(capture(
            "Windows gave no id for the microphone or the output".into(),
        ));
    };
    let microphone = CheckedEndpoint {
        id: mic_id,
        name: source.mic_device().name.clone(),
    };
    let output = CheckedEndpoint {
        id: output_id.clone(),
        name: source
            .loopback_device()
            .map(|d| d.name.clone())
            .unwrap_or_default(),
    };
    let fallbacks = source.fallbacks().to_vec();
    // Held across `measure`: a panic there drops it, which stops and joins the playback.
    let player = Playback::spawn(move |flag| {
        let com = ComApartment::init_mta().map_err(|e| e.to_string())?;
        let mut render = RenderStream::open(&com, Some(&output_id)).map_err(|e| e.to_string())?;
        let rate = SampleRate::new(render.rate()).ok_or("the output reports 0 Hz")?;
        render
            .play(&core_check::test_signal(rate), flag)
            .map_err(|e| e.to_string())
    })?;
    let measured = measure(
        &mut source,
        &mut || player.is_finished(),
        cancel,
        &mut || std::thread::sleep(Duration::from_millis(10)),
        progress,
    );
    let played = player.finish();
    let Some(result) = measured? else {
        return Ok(None);
    };
    played.map_err(|e| capture(format!("the sounds could not be played: {e}")))?;
    Ok(Some(Measured {
        microphone,
        output,
        result,
        fallbacks,
    }))
}
