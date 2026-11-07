//! Acoustic echo cancellation (PLAN.md 3.3, M1).
//!
//! - [`delay`]: GCC-PHAT bulk delay tracking and clock drift estimation.
//! - [`linear`]: partitioned-block frequency-domain adaptive filter.
//! - [`highpass`]: high-pass filtering around the canceller (T6.23b).
//! - [`coherence`]: double-talk statistic independent of the linear echo estimate.
//! - [`EchoCanceller`]: the public canceller (linear stage, double-talk detection,
//!   residual echo suppression, optional comfort noise): `process(mic, reference) -> out`.
//! - [`eval`]: scoring of the M1 metrics on simulated calls (shared by tests and
//!   `accept m1`).

mod canceller;
pub mod coherence;
pub mod delay;
pub mod eval;
pub mod highpass;
pub mod linear;
pub mod postfilter;

#[cfg(test)]
mod tests;

pub use canceller::{BlockStatus, DEFAULT_HIGHPASS, EchoCanceller, EchoCancellerConfig};
pub use highpass::{AllPassFilter, HighPass, HighPassFilter};

/// An echo-canceller setting is out of range (`*Config::validate`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigError(pub String);

impl std::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "invalid echo canceller setting: {}", self.0)
    }
}

impl std::error::Error for ConfigError {}

/// `Err(ConfigError)` with `msg` unless `ok`.
pub(crate) fn require(ok: bool, msg: &str) -> Result<(), ConfigError> {
    if ok {
        Ok(())
    } else {
        Err(ConfigError(msg.to_owned()))
    }
}

/// Finite and within `[lo, hi]`.
pub(crate) fn within(x: f64, lo: f64, hi: f64) -> bool {
    x.is_finite() && x >= lo && x <= hi
}
pub use linear::BLOCK;
