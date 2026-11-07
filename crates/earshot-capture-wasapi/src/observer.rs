//! The seam between the WASAPI capture loop and what is done with its packets.
//!
//! The Windows capture loop (`wasapi`) owns streams, waits, device following and
//! re-opens; per track it hands every packet and every re-open to a
//! [`TrackObserver`]. `record_raw` observes with a timeline
//! ([`RecordingTrack`](crate::recording::RecordingTrack)), the live `AudioSource` with
//! an event emitter ([`EmitterTrack`](crate::live::EmitterTrack)). Platform independent.

use crate::endpoint::EndpointProperties;
use crate::follow::ReopenReason;
use crate::format::MixFormat;
use crate::timeline::PacketInfo;

/// Result of one stream re-open attempt.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReopenOutcome<'a> {
    /// A new stream is open and started.
    Opened {
        /// Endpoint friendly name (or process loopback label).
        device_name: &'a str,
        /// Native mix format of the new endpoint.
        device_format: MixFormat,
        /// Rate of the packets the new stream delivers (the track's rate: a re-open asks
        /// the audio engine to convert, and to keep the track's channel count).
        rate: u32,
        /// The new endpoint's properties (`None` for process loopback).
        endpoint: Option<&'a EndpointProperties>,
        /// The name of the chosen endpoint (T7.4i) this default one replaces for the rest of
        /// the capture, when the track has just given it up; `None` otherwise.
        gave_up: Option<&'a str>,
    },
    /// No stream could be opened; the track has no device until a later retry.
    Failed {
        /// Why.
        error: &'a str,
    },
}

impl ReopenOutcome<'_> {
    /// True for [`ReopenOutcome::Opened`].
    pub fn is_opened(&self) -> bool {
        matches!(self, Self::Opened { .. })
    }
}

/// Whether a re-open is reported (logged in `timestamps.json`, announced as a
/// discontinuity): every successful re-open, and every failure except periodic
/// retries of an already lost device.
pub fn reopen_is_reported(reason: ReopenReason, opened: bool) -> bool {
    opened || reason != ReopenReason::Retry
}

/// Receives one track's packets and re-opens, in capture order, on the capture thread.
pub trait TrackObserver {
    /// One packet: `samples` holds its converted frames, interleaved at the track's
    /// channel count (1 for the mic, the device channels for the loopback; empty when
    /// `info.silent`).
    fn packet(&mut self, info: PacketInfo, samples: &[f32]);

    /// The stream was closed at `qpc` (its last packets were already delivered) and a
    /// re-open was attempted for `reason`. Called for every attempt, reported or not
    /// (see [`reopen_is_reported`]).
    fn reopened(&mut self, qpc: u64, reason: ReopenReason, outcome: ReopenOutcome<'_>);

    /// The call plays on `wanted`, which could not be opened at `qpc` (`error`), so the track goes
    /// on recording `used` undisturbed (T6.15). Default: ignored.
    fn call_output_refused(&mut self, qpc: u64, wanted: &str, used: Option<&str>, error: &str) {
        let _ = (qpc, wanted, used, error);
    }

    /// Packets drained after this call were captured before `qpc` (a "now" plus a
    /// margin); a later timestamp is bogus. Default: ignored.
    fn set_horizon(&mut self, qpc: u64) {
        let _ = qpc;
    }

    /// False once the consumer is gone and capture can stop. Default: true.
    fn wants_more(&self) -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retry_failures_are_the_only_unreported_reopens() {
        for reason in [
            ReopenReason::DefaultDeviceChanged,
            ReopenReason::StreamError,
            ReopenReason::Retry,
            ReopenReason::CallOutputMoved,
        ] {
            assert!(reopen_is_reported(reason, true), "{reason:?}");
        }
        assert!(reopen_is_reported(
            ReopenReason::DefaultDeviceChanged,
            false
        ));
        assert!(reopen_is_reported(ReopenReason::StreamError, false));
        assert!(!reopen_is_reported(ReopenReason::Retry, false));
    }
}
