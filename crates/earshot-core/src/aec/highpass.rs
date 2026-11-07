//! High-pass filtering around the echo canceller (T6.23b).
//!
//! A microphone in a real room picks up energy below about 100 Hz that is not echo (desk
//! and building rumble, handling, ventilation), while desktop loudspeakers reproduce almost
//! nothing there. On a real recording the canceller removed far less of the mic's power on
//! far-end-only blocks than it did above 100 Hz: every broadband statistic of the
//! canceller (the linear stage's ERLE that sets `converged`, the mono double-talk witness
//! gated on it, the adaptation error) was dominated by the low band the echo does not
//! reach, so the linear stage was never declared converged in talk-over, the witness never
//! called, and the cold-phase postfilter cut the near end. WebRTC's audio processing
//! high-passes the capture signal (about 80 Hz) before its echo controller
//! (`modules/audio_processing/high_pass_filter.cc`, applied in
//! `modules/audio_processing/audio_processing_impl.cc` ahead of the echo controller); AEC3
//! has an option to high-pass the render reference as well (`EchoCanceller3Config`,
//! `api/audio/echo_canceller3_config.h`).
//!
//! [`HighPass`] chooses how the canceller uses the filter:
//!
//! - [`HighPass::Audio`]: mic and reference are high-passed before the canceller; its
//!   output, the app's copy of the audio, is high-passed too.
//! - [`HighPass::Internal`]: the filter is used only inside the canceller. The linear stage
//!   adapts on the filtered mic and reference, so its echo estimate `y` estimates the
//!   filtered echo `H(echo)`; every detector, ERLE accumulator and postfilter model is
//!   taken on the filtered signals. The output is built on the full-band mic. Subtracting
//!   `y` from the mic itself would leave `echo - H(echo)`, which falls only 6 dB per octave
//!   above the cutoff (-19 dB at 1 kHz for an 80 Hz cutoff; measured, a 7 dB linear-stage
//!   ERLE against 12.5 dB). Instead the output is built on `A(mic)`, where `A` is the
//!   first-order all-pass that splits the high-pass exactly: `H = A + L` with `L` a
//!   third-order low-pass. For the normalised Butterworth `H(s) = s^2 / (s^2 + sqrt(2) s + 1)`
//!   and `A(s) = (s - a) / (s + a)` with `a = 1 / sqrt(2)`,
//!   `H(s) - A(s) = (1 / sqrt(2)) / ((s^2 + sqrt(2) s + 1)(s + 1 / sqrt(2)))`, and the same
//!   bilinear transform keeps the identity for the digital filters. The linear output is
//!   then `A(mic) - y = A(near) - L(echo) + (H(echo) - y)`: the near end with its magnitude
//!   unchanged at every frequency (only an all-pass phase, group delay at most
//!   `2 sqrt(2) / (2 pi fc)`, 5.6 ms for 80 Hz, near DC), minus the echo below the cutoff
//!   that the estimate lacks, and the linear stage's own misadjustment. The postfilter
//!   learns its models and estimates the residual echo on the filtered signals (so non-echo
//!   energy below the cutoff never inflates them), divides the estimate per bin by the
//!   filter's power response (floored at -20 dB, `postfilter::MIN_ANALYSIS_RESPONSE`) to
//!   get the residual echo in the full band, and computes the gain from that and the
//!   full-band signal's own power: a bin below the cutoff is suppressed only as far as it
//!   holds predicted echo, and non-echo energy there passes. Without an echo path the
//!   output is `A(mic)` delayed.
//!
//! - [`HighPass::Decisions`] (design D, T6.23b): the linear stage adapts and the output is
//!   built exactly as without a filter; the high-passed mic, echo estimate, error and
//!   aligned reference (each through its own filter instance; the error is the filtered mic
//!   minus the filtered estimate, which is the filtered error) feed only the canceller's
//!   decisions: the convergence and ERLE measurement, the double-talk witness and its
//!   freeze, the coherence statistic and veto, the mismatch bypass and the noise floor.
//!   With `postfilter: true` the postfilter's models and residual estimates are also taken
//!   on the filtered signals and the estimate is divided per bin by the filter's power
//!   response (the base is the unfiltered linear output, whose filtered copy the analysis
//!   is); with `postfilter: false` the postfilter sees the unfiltered signals and only its
//!   control flags come from the filtered decisions.
//!
//! The filter is a second-order Butterworth high-pass (bilinear transform with
//! pre-warping, R. Bristow-Johnson, "Cookbook formulae for audio EQ biquad filter
//! coefficients", `Q = 1/sqrt(2)`), run in f64 as a transposed direct form II, one
//! instance per signal, so mic and reference see the same filter.
//!
//! The candidates, the selection rule (fixed before measuring) and the measurements are in
//! private notes.

use rustfft::num_complex::Complex;

use super::{ConfigError, require, within};

/// How the echo canceller uses a high-pass filter (see the module docs).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum HighPass {
    /// No filter.
    Off,
    /// Mic and reference high-passed before the canceller; the output is high-passed.
    Audio {
        /// Cutoff (-3 dB), Hz.
        cutoff_hz: f32,
    },
    /// The filter is used only inside the canceller; the output keeps the full-band mic.
    Internal {
        /// Cutoff (-3 dB), Hz.
        cutoff_hz: f32,
    },
    /// The filter feeds only the canceller's decisions; adaptation and output are the
    /// unfiltered canceller's.
    Decisions {
        /// Cutoff (-3 dB), Hz.
        cutoff_hz: f32,
        /// The postfilter's models and residual estimates also use the filtered signals
        /// (label `D<hz>`); false leaves the postfilter's inputs unfiltered (`D<hz>w`).
        postfilter: bool,
    },
}

/// Lowest accepted cutoff (Hz).
pub const MIN_CUTOFF_HZ: f32 = 10.0;
/// Highest accepted cutoff (Hz).
pub const MAX_CUTOFF_HZ: f32 = 1_000.0;

impl HighPass {
    /// The cutoff, if a filter is used.
    #[must_use]
    pub fn cutoff_hz(self) -> Option<f32> {
        match self {
            Self::Off => None,
            Self::Audio { cutoff_hz }
            | Self::Internal { cutoff_hz }
            | Self::Decisions { cutoff_hz, .. } => Some(cutoff_hz),
        }
    }

    /// Checks the cutoff.
    ///
    /// # Errors
    /// [`ConfigError`] when the cutoff is not within [`MIN_CUTOFF_HZ`]..=[`MAX_CUTOFF_HZ`].
    pub fn validate(self) -> Result<(), ConfigError> {
        match self.cutoff_hz() {
            None => Ok(()),
            Some(f) => require(
                within(
                    f64::from(f),
                    f64::from(MIN_CUTOFF_HZ),
                    f64::from(MAX_CUTOFF_HZ),
                ),
                "highpass cutoff_hz must be in [10, 1000]",
            ),
        }
    }

    /// Parses an evaluation label: `off`, `A<hz>` (audio), `I<hz>` (internal), `D<hz>`
    /// (decisions and postfilter) or `D<hz>w` (decisions only), for example `D80w`. Used by
    /// the evaluation harnesses to select a variant.
    ///
    /// # Errors
    /// A message when the label is not one of those forms or the cutoff is out of range.
    pub fn parse(label: &str) -> Result<Self, String> {
        let bad =
            || format!("high-pass variant {label:?}: expected off, A<hz>, I<hz>, D<hz> or D<hz>w");
        let hp = if label.eq_ignore_ascii_case("off") {
            Self::Off
        } else {
            let (kind, hz) = label.split_at_checked(1).ok_or_else(bad)?;
            let (hz, w) = match hz.strip_suffix('w') {
                Some(h) => (h, true),
                None => (hz, false),
            };
            let cutoff_hz: f32 = hz.parse().map_err(|_| bad())?;
            match (kind, w) {
                ("A" | "a", false) => Self::Audio { cutoff_hz },
                ("I" | "i", false) => Self::Internal { cutoff_hz },
                ("D" | "d", w) => Self::Decisions {
                    cutoff_hz,
                    postfilter: !w,
                },
                _ => return Err(bad()),
            }
        };
        hp.validate().map_err(|e| e.to_string())?;
        Ok(hp)
    }

    /// The evaluation label ([`HighPass::parse`] reads it back).
    #[must_use]
    pub fn label(self) -> String {
        match self {
            Self::Off => "off".to_owned(),
            Self::Audio { cutoff_hz } => format!("A{cutoff_hz}"),
            Self::Internal { cutoff_hz } => format!("I{cutoff_hz}"),
            Self::Decisions {
                cutoff_hz,
                postfilter,
            } => format!("D{cutoff_hz}{}", if postfilter { "" } else { "w" }),
        }
    }
}

/// First-order all-pass `A` with `H = A + L` for the high-pass `H` of the same cutoff
/// (`L` a third-order low-pass; see the module docs), streaming.
#[derive(Debug, Clone, Copy)]
pub struct AllPassFilter {
    g: f64,
    z: f64,
}

impl AllPassFilter {
    /// The all-pass matching [`HighPassFilter::new`]`(cutoff_hz, rate_hz)`.
    ///
    /// # Panics
    /// As [`HighPassFilter::new`].
    #[must_use]
    pub fn matching(cutoff_hz: f32, rate_hz: u32) -> Self {
        let f = f64::from(cutoff_hz);
        let fs = f64::from(rate_hz);
        assert!(
            f.is_finite() && f > 0.0 && f < fs / 2.0,
            "all-pass cutoff out of range"
        );
        // Bilinear transform s = (1 - z^-1) / (K (1 + z^-1)), K = tan(w0 / 2) (the
        // pre-warping of the cookbook high-pass), of (s - a) / (s + a), a = 1 / sqrt(2):
        // A(z) = (g - z^-1) / (1 - g z^-1), g = (1 - c) / (1 + c), c = K a.
        let c = (std::f64::consts::PI * f / fs).tan() / std::f64::consts::SQRT_2;
        Self {
            g: (1.0 - c) / (1.0 + c),
            z: 0.0,
        }
    }

    /// Filters `x` in place, continuing from the previous call.
    pub fn process(&mut self, x: &mut [f32]) {
        for v in x {
            // y[n] = g x[n] - x[n-1] + g y[n-1], transposed direct form.
            let input = f64::from(*v);
            let y = self.g * input + self.z;
            self.z = self.g * y - input;
            *v = y as f32;
        }
    }

    /// Frequency response `A(f)` at `freq_hz` (unit magnitude).
    #[must_use]
    pub fn response(&self, freq_hz: f64, rate_hz: u32) -> Complex<f64> {
        let z1 = Complex::from_polar(1.0, -std::f64::consts::TAU * freq_hz / f64::from(rate_hz));
        (self.g - z1) / (1.0 - self.g * z1)
    }

    /// Like [`AllPassFilter::process`] into a new buffer.
    #[must_use]
    pub fn filtered(&mut self, x: &[f32]) -> Vec<f32> {
        let mut out = x.to_vec();
        self.process(&mut out);
        out
    }
}

/// Second-order Butterworth high-pass (see the module docs), streaming.
#[derive(Debug, Clone, Copy)]
pub struct HighPassFilter {
    b: [f64; 3],
    a: [f64; 2],
    z: [f64; 2],
}

impl HighPassFilter {
    /// A filter with its -3 dB point at `cutoff_hz` for audio at `rate_hz`.
    ///
    /// # Panics
    /// If the cutoff is not positive and below half the rate.
    #[must_use]
    pub fn new(cutoff_hz: f32, rate_hz: u32) -> Self {
        let f = f64::from(cutoff_hz);
        let fs = f64::from(rate_hz);
        assert!(
            f.is_finite() && f > 0.0 && f < fs / 2.0,
            "high-pass cutoff out of range"
        );
        let w0 = std::f64::consts::TAU * f / fs;
        let alpha = w0.sin() / std::f64::consts::SQRT_2;
        let c = w0.cos();
        let a0 = 1.0 + alpha;
        Self {
            b: [(1.0 + c) / 2.0 / a0, -(1.0 + c) / a0, (1.0 + c) / 2.0 / a0],
            a: [-2.0 * c / a0, (1.0 - alpha) / a0],
            z: [0.0; 2],
        }
    }

    /// Frequency response `H(f)` at `freq_hz`.
    #[must_use]
    pub fn response(&self, freq_hz: f64, rate_hz: u32) -> Complex<f64> {
        let w = std::f64::consts::TAU * freq_hz / f64::from(rate_hz);
        let z1 = Complex::from_polar(1.0, -w);
        let z2 = z1 * z1;
        (self.b[0] + self.b[1] * z1 + self.b[2] * z2) / (1.0 + self.a[0] * z1 + self.a[1] * z2)
    }

    /// Power response `|H(f)|^2` at `freq_hz`.
    #[must_use]
    pub fn power_response(&self, freq_hz: f64, rate_hz: u32) -> f64 {
        self.response(freq_hz, rate_hz).norm_sqr()
    }

    /// Filters `x` in place, continuing from the previous call.
    pub fn process(&mut self, x: &mut [f32]) {
        for v in x {
            let input = f64::from(*v);
            let y = self.b[0] * input + self.z[0];
            self.z[0] = self.b[1] * input - self.a[0] * y + self.z[1];
            self.z[1] = self.b[2] * input - self.a[1] * y;
            *v = y as f32;
        }
    }

    /// Like [`HighPassFilter::process`] into a new buffer.
    #[must_use]
    pub fn filtered(&mut self, x: &[f32]) -> Vec<f32> {
        let mut out = x.to_vec();
        self.process(&mut out);
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tone_gain_db(cutoff: f32, freq: f64) -> f64 {
        let n = 32_000;
        let x: Vec<f32> = (0..n)
            .map(|i| (std::f64::consts::TAU * freq * i as f64 / 16_000.0).sin() as f32)
            .collect();
        let y = HighPassFilter::new(cutoff, 16_000).filtered(&x);
        // Skip the transient (first second).
        let p = |v: &[f32]| {
            v[16_000..]
                .iter()
                .map(|s| f64::from(*s).powi(2))
                .sum::<f64>()
        };
        10.0 * (p(&y) / p(&x)).log10()
    }

    #[test]
    fn aec_highpass_is_a_second_order_butterworth() {
        for cutoff in [80.0f32, 100.0, 120.0] {
            let at = tone_gain_db(cutoff, f64::from(cutoff));
            assert!(
                (at + 3.01).abs() < 0.1,
                "{cutoff} Hz: {at} dB at the cutoff"
            );
            // 12 dB per octave: an octave below the cutoff is about -12.3 dB.
            let below = tone_gain_db(cutoff, f64::from(cutoff) / 2.0);
            assert!(
                (below + 12.3).abs() < 0.3,
                "{cutoff} Hz: {below} dB an octave below"
            );
            let pass = tone_gain_db(cutoff, 1_000.0);
            assert!(pass.abs() < 0.05, "{cutoff} Hz: {pass} dB at 1 kHz");
            let f = HighPassFilter::new(cutoff, 16_000);
            for freq in [f64::from(cutoff) / 2.0, f64::from(cutoff), 1_000.0] {
                let analytic = 10.0 * f.power_response(freq, 16_000).log10();
                let measured = tone_gain_db(cutoff, freq);
                assert!((analytic - measured).abs() < 0.05, "{cutoff} Hz at {freq}");
            }
        }
    }

    #[test]
    fn aec_highpass_minus_its_allpass_is_a_low_pass() {
        // H = A + L: the difference of the two filters' outputs keeps low frequencies and
        // falls 18 dB per octave above the cutoff; A has unit gain everywhere.
        let gain_db = |cutoff: f32, freq: f64, diff: bool| {
            let x: Vec<f32> = (0..48_000)
                .map(|i| (std::f64::consts::TAU * freq * i as f64 / 16_000.0).sin() as f32)
                .collect();
            let a = AllPassFilter::matching(cutoff, 16_000).filtered(&x);
            let y: Vec<f32> = if diff {
                let h = HighPassFilter::new(cutoff, 16_000).filtered(&x);
                h.iter().zip(&a).map(|(h, a)| h - a).collect()
            } else {
                a
            };
            let p = |v: &[f32]| {
                v[16_000..]
                    .iter()
                    .map(|s| f64::from(*s).powi(2))
                    .sum::<f64>()
            };
            10.0 * (p(&y) / p(&x)).log10()
        };
        for cutoff in [80.0f32, 100.0] {
            let c = f64::from(cutoff);
            for freq in [c / 4.0, c, 4.0 * c, 1_000.0] {
                assert!(
                    gain_db(cutoff, freq, false).abs() < 0.01,
                    "all-pass gain at {freq}"
                );
            }
            // |L| at a quarter of the cutoff is 0.94 (-0.5 dB).
            assert!(gain_db(cutoff, c / 4.0, true).abs() < 1.0, "low band kept");
            let at4 = gain_db(cutoff, 4.0 * c, true);
            let at8 = gain_db(cutoff, 8.0 * c, true);
            assert!(at4 < -30.0, "{cutoff} Hz: {at4} dB at 4 x cutoff");
            assert!(
                (at4 - at8 - 18.0).abs() < 1.0,
                "{cutoff} Hz: slope {}",
                at4 - at8
            );
            let (h, a) = (
                HighPassFilter::new(cutoff, 16_000),
                AllPassFilter::matching(cutoff, 16_000),
            );
            let l = |f: f64| (h.response(f, 16_000) - a.response(f, 16_000)).norm_sqr();
            assert!((10.0 * l(4.0 * c).log10() - at4).abs() < 0.1, "analytic L");
        }
    }

    #[test]
    fn aec_highpass_streams_like_one_call() {
        let x: Vec<f32> = (0..5_000)
            .map(|i| ((i * 7919) % 101) as f32 / 50.0 - 1.0)
            .collect();
        let whole = HighPassFilter::new(80.0, 16_000).filtered(&x);
        let mut f = HighPassFilter::new(80.0, 16_000);
        let mut parts = Vec::new();
        for c in x.chunks(333) {
            parts.extend(f.filtered(c));
        }
        assert_eq!(whole, parts);
    }

    #[test]
    fn aec_highpass_labels_round_trip_and_validate() {
        for l in ["off", "A80", "A100", "A120", "I80", "I100", "D80", "D100w"] {
            let hp = HighPass::parse(l).unwrap();
            assert_eq!(HighPass::parse(&hp.label()).unwrap(), hp);
        }
        assert_eq!(
            HighPass::parse("I80").unwrap(),
            HighPass::Internal { cutoff_hz: 80.0 }
        );
        assert_eq!(
            HighPass::parse("D80w").unwrap(),
            HighPass::Decisions {
                cutoff_hz: 80.0,
                postfilter: false
            }
        );
        for l in [
            "", "X80", "A", "Ifoo", "A5", "I2000", "Anan", "A80w", "I80w", "Dw", "D5w",
        ] {
            assert!(HighPass::parse(l).is_err(), "{l}");
        }
        assert!(
            HighPass::Audio {
                cutoff_hz: f32::NAN
            }
            .validate()
            .is_err()
        );
        assert!(HighPass::Off.validate().is_ok());
    }
}
