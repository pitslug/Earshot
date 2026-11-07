//! Voice activity detection: the [`Vad`] trait and an energy-based implementation.

use std::collections::VecDeque;
use std::time::Duration;

use super::transcriber::AsrError;
use crate::audio::{PIPELINE_RATE, Sample};

/// Frame-based voice activity detector at [`crate::audio::PIPELINE_RATE`].
///
/// Callers feed consecutive frames of exactly [`Vad::frame_len`] samples. Implementations
/// may keep state across frames (Silero keeps an RNN state); [`Vad::reset`] clears it.
pub trait Vad {
    /// What decides this detector's probabilities: the algorithm and its settings, or the
    /// model with a hash of its file (Silero: `silero-vad@<first 12 hex digits of the
    /// SHA-256>`). Two detectors with the same id give the same probabilities for the same
    /// frames, so work recorded with one (the live speech stretches, T6.13c) is reused only
    /// with a detector of the same id.
    fn id(&self) -> String;

    /// Samples per frame at the pipeline rate (energy VAD: 480 = 30 ms; Silero: 512).
    fn frame_len(&self) -> usize;

    /// Speech probability in `[0, 1]` for the next frame.
    ///
    /// # Errors
    /// [`AsrError::Config`] if `frame.len() != self.frame_len()`; [`AsrError::Backend`] if
    /// a model-based detector fails.
    fn speech_probability(&mut self, frame: &[Sample]) -> Result<f32, AsrError>;

    /// Forgets all state (start of a new stream).
    fn reset(&mut self);
}

impl<V: Vad + ?Sized> Vad for Box<V> {
    fn id(&self) -> String {
        (**self).id()
    }
    fn frame_len(&self) -> usize {
        (**self).frame_len()
    }
    fn speech_probability(&mut self, frame: &[Sample]) -> Result<f32, AsrError> {
        (**self).speech_probability(frame)
    }
    fn reset(&mut self) {
        (**self).reset();
    }
}

/// Settings for [`EnergyVad`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct EnergyVadConfig {
    /// Samples per frame (default 480 = 30 ms at 16 kHz).
    pub frame_len: usize,
    /// Absolute gate: frames quieter than this RMS level (dBFS) are never speech.
    pub min_level_dbfs: f32,
    /// Relative gate: speech must be this many dB above the tracked noise floor.
    pub margin_db: f32,
    /// How fast the noise floor may rise, in dB per second (it falls instantly).
    pub floor_rise_db_per_s: f32,
    /// The floor never exceeds the minimum frame level within this window (default 1.5 s).
    pub min_window: Duration,
}

impl Default for EnergyVadConfig {
    fn default() -> Self {
        EnergyVadConfig {
            frame_len: 480,
            min_level_dbfs: -50.0,
            margin_db: 10.0,
            floor_rise_db_per_s: 3.0,
            min_window: Duration::from_millis(1500),
        }
    }
}

/// Energy VAD with an adaptive noise floor (minimum tracking).
///
/// A frame is speech (probability 1) when its RMS level is at least
/// `max(min_level_dbfs, noise_floor + margin_db)`, otherwise 0.
///
/// The noise floor follows minimum statistics (R. Martin, "Noise power spectral density
/// estimation based on optimal smoothing and minimum statistics", IEEE Trans. Speech and
/// Audio Processing 9(5), 2001), reduced to broadband frame levels: it is the minimum
/// frame level over the last `min_window` (1.5 s, Martin's window is of that order), so
/// the pauses of ordinary speech hold it down and a talker never lifts it into speech.
/// On top of that it rises by at most `floor_rise_db_per_s`, so a sustained loud signal
/// with no pauses becomes background only gradually; it drops instantly.
/// Cheap and model-free; Silero (T2.3) is the accurate detector.
#[derive(Debug, Clone, Default)]
pub struct EnergyVad {
    config: EnergyVadConfig,
    floor_db: Option<f32>,
    /// Frame levels (dBFS) within the minimum window, oldest first.
    recent: VecDeque<f32>,
}

impl EnergyVad {
    /// A detector with `config`.
    ///
    /// # Errors
    /// [`AsrError::Config`] if `frame_len` is 0 or a level is not finite.
    pub fn new(config: EnergyVadConfig) -> Result<Self, AsrError> {
        if config.frame_len == 0 {
            return Err(AsrError::Config("energy VAD frame_len must be > 0".into()));
        }
        let levels = [
            config.min_level_dbfs,
            config.margin_db,
            config.floor_rise_db_per_s,
        ];
        if levels.iter().any(|v| !v.is_finite()) || config.floor_rise_db_per_s < 0.0 {
            return Err(AsrError::Config(
                "energy VAD levels must be finite and the floor rise non-negative".into(),
            ));
        }
        if config.min_window.is_zero() {
            return Err(AsrError::Config("energy VAD min_window must be > 0".into()));
        }
        Ok(EnergyVad {
            config,
            floor_db: None,
            recent: VecDeque::new(),
        })
    }

    /// Frames in the minimum window (at least 1).
    fn window_frames(&self) -> usize {
        let frame_s = self.config.frame_len as f64 / f64::from(PIPELINE_RATE.hz());
        ((self.config.min_window.as_secs_f64() / frame_s).ceil() as usize).max(1)
    }

    /// The current noise-floor estimate in dBFS, once a frame has been seen.
    #[must_use]
    pub fn noise_floor_dbfs(&self) -> Option<f32> {
        self.floor_db
    }
}

impl Vad for EnergyVad {
    fn id(&self) -> String {
        // Every setting changes the output; Debug prints each exactly.
        format!("energy-vad {:?}", self.config)
    }

    fn frame_len(&self) -> usize {
        self.config.frame_len
    }

    fn speech_probability(&mut self, frame: &[Sample]) -> Result<f32, AsrError> {
        if frame.len() != self.config.frame_len {
            return Err(AsrError::Config(format!(
                "VAD frame must be {} samples, got {}",
                self.config.frame_len,
                frame.len()
            )));
        }
        let level = rms_dbfs(frame);
        let frame_s = frame.len() as f32 / PIPELINE_RATE.hz() as f32;
        self.recent.push_back(level);
        let window = self.window_frames();
        while self.recent.len() > window {
            self.recent.pop_front();
        }
        let window_min = self.recent.iter().copied().fold(f32::INFINITY, f32::min);
        let floor = match self.floor_db {
            None => window_min,
            Some(f) => (f + self.config.floor_rise_db_per_s * frame_s).min(window_min),
        };
        self.floor_db = Some(floor);
        let gate = self
            .config
            .min_level_dbfs
            .max(floor + self.config.margin_db);
        Ok(if level >= gate { 1.0 } else { 0.0 })
    }

    fn reset(&mut self) {
        self.floor_db = None;
        self.recent.clear();
    }
}

/// RMS level of `frame` in dBFS (full scale 1.0), floored at -120 dB.
fn rms_dbfs(frame: &[Sample]) -> f32 {
    let energy: f64 = frame.iter().map(|&x| f64::from(x) * f64::from(x)).sum();
    let mean = energy / frame.len().max(1) as f64;
    (10.0 * mean.max(1e-12).log10()) as f32
}
