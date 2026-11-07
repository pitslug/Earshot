//! `earshot devices` and `earshot record`: live capture from the default devices
//! (Windows WASAPI).
//!
//! - `devices` lists the default console and communications microphones and the default
//!   render device (the loopback), each with its native mix format, and the render audio
//!   sessions (the processes `--loopback-process` can name). A device that cannot be
//!   opened is listed with the reason; nothing is recorded.
//! - `record` runs one live meeting through the runtime ([`start_wasapi_session`]) and the
//!   shared driver ([`crate::session`]): it stops after `--seconds` of capture or on the
//!   first Ctrl+C (a second Ctrl+C ends the process at once, without finalising), then
//!   finalises every recording before the post pass, summary and export. With `--in-room`
//!   (T6.27) it records the mic alone: no loopback, no echo canceller, one track, speakers
//!   found on the mic with the in-person policy ("Me" only from the user's voiceprint).

use std::io::Write;
use std::time::{Duration, Instant};

use anyhow::Result;
use clap::Args;
use earshot_capture_wasapi::{CaptureMode, LoopbackRequest, MicRole};
use earshot_core::identity::EntityId;

use crate::MicRoleArg;
use crate::session::MeetingOptions;

/// `earshot record` arguments.
#[derive(Debug, Clone, Args)]
pub struct RecordArgs {
    /// Stop this many seconds after the session is running (default: record until
    /// Ctrl+C). Capture starts before the models load, so the recording is longer by the
    /// load time.
    #[arg(long)]
    pub seconds: Option<f64>,
    /// Which default capture device to use as the mic.
    #[arg(long, value_enum, default_value_t = MicRoleArg::Console)]
    pub mic_role: MicRoleArg,
    /// Loopback source: `default-device` (the whole speaker output, the default and the
    /// reliable choice), `auto` (a call app with an active audio session, else the
    /// default speakers), `follow-call` (the default speakers, then the output a known call app
    /// plays on: on trial, T6.15), or a process name (`zoom.exe`) or pid. Per-app capture is not
    /// yet proven on call apps (T6.15).
    #[arg(
        long,
        default_value = "default-device",
        value_name = "auto|NAME|PID|default-device|follow-call"
    )]
    pub loopback_process: LoopbackRequest,
    /// A meeting in the room with no call audio: record the mic only (no loopback, no
    /// echo canceller); speakers are found on the mic and "Me" only from your voiceprint.
    #[arg(long, conflicts_with = "loopback_process")]
    pub in_room: bool,
    /// Data, model, post-pass, summary and export options.
    #[command(flatten)]
    pub meeting: MeetingOptions,
}

impl RecordArgs {
    /// What to capture: the mic alone with `--in-room`, else the mic and the loopback.
    #[must_use]
    pub fn capture_mode(&self) -> CaptureMode {
        if self.in_room {
            CaptureMode::InRoom
        } else {
            CaptureMode::Call(self.loopback_process.clone())
        }
    }
}

/// When a recording stops: after `limit` measured from the first check (the session has
/// started by then), or once `stop_requested` is true.
#[derive(Debug)]
pub struct StopRule {
    limit: Option<Duration>,
    started: Option<Instant>,
}

impl StopRule {
    /// A rule for `--seconds` (`None`: only on request).
    ///
    /// # Errors
    /// A limit that is not a positive, finite number of seconds.
    pub fn new(seconds: Option<f64>) -> Result<Self> {
        let limit = match seconds {
            Some(s) => {
                crate::record_raw::check_seconds(s)?;
                Some(Duration::try_from_secs_f64(s)?)
            }
            None => None,
        };
        Ok(Self {
            limit,
            started: None,
        })
    }

    /// Whether to stop now.
    pub fn should_stop(&mut self, now: Instant, stop_requested: bool) -> bool {
        let started = *self.started.get_or_insert(now);
        stop_requested || self.limit.is_some_and(|l| now.duration_since(started) >= l)
    }
}

/// Lists the default devices and the render sessions to `out`.
///
/// # Errors
/// COM cannot be initialised, or output errors.
#[cfg(windows)]
pub fn devices(out: &mut dyn Write) -> Result<()> {
    use earshot_capture_wasapi::recording::DeviceDescription;
    use earshot_capture_wasapi::wasapi::{
        CaptureStream, ComApartment, Endpoint, list_render_sessions,
    };

    let com = ComApartment::init_mta()?;
    for (label, endpoint) in [
        ("mic (console)", Endpoint::Microphone(MicRole::Console)),
        (
            "mic (communications)",
            Endpoint::Microphone(MicRole::Communications),
        ),
        ("loopback (default render)", Endpoint::Loopback),
    ] {
        // Opened (not started) only to read the endpoint's name and mix format.
        match CaptureStream::open_default(&com, endpoint) {
            Ok(stream) => {
                let d = DeviceDescription {
                    name: stream.device_name().to_owned(),
                    format: stream.device_format(),
                };
                writeln!(out, "{}", crate::record_raw::describe_device(label, &d))?;
                // T6.24: a Bluetooth hands-free mic degrades recording and playback.
                if matches!(endpoint, Endpoint::Microphone(_))
                    && let Some(w) = stream
                        .endpoint_properties()
                        .and_then(earshot_capture_wasapi::hands_free_warning)
                {
                    writeln!(out, "  warning: {w}")?;
                }
            }
            Err(e) => writeln!(out, "{label}: unavailable ({e})")?,
        }
    }
    match list_render_sessions(&com) {
        Ok(sessions) if sessions.is_empty() => writeln!(out, "render sessions: none")?,
        Ok(sessions) => {
            writeln!(out, "render sessions (--loopback-process NAME or PID):")?;
            for s in sessions {
                let name = if s.process_name.is_empty() {
                    "?"
                } else {
                    &s.process_name
                };
                let state = if s.active { "active" } else { "inactive" };
                writeln!(out, "  {name} (pid {}) {state}", s.pid)?;
            }
        }
        Err(e) => writeln!(out, "render sessions: unavailable ({e})")?,
    }
    Ok(())
}

/// Not available off Windows.
///
/// # Errors
/// Always.
#[cfg(not(windows))]
pub fn devices(_out: &mut dyn Write) -> Result<()> {
    anyhow::bail!("devices needs Windows (WASAPI)")
}

/// Records one live meeting (module docs).
///
/// # Errors
/// Bad options, capture that cannot start, and as [`crate::session::run_meeting`].
#[cfg(windows)]
pub fn run(args: &RecordArgs, out: &mut dyn Write) -> Result<EntityId> {
    use std::cell::RefCell;
    use std::sync::atomic::Ordering;

    use earshot_core::model::ClockDomain;
    use earshot_runtime::host::wasapi_starter;
    use earshot_runtime::live::LiveConfig;

    let rule = RefCell::new(StopRule::new(args.seconds)?);
    let ctrl_c = earshot_capture_wasapi::wasapi::install_ctrl_c_handler()?;
    // T6.18: after the press that stops the recording (if any), one more press cancels only
    // the cleanup; a press after that ends the process.
    let cancel_at = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(u32::MAX));
    let armed = std::sync::Arc::clone(&cancel_at);
    let cleanup = crate::session::CleanupInterrupt {
        arm: Box::new(move || {
            armed.store(ctrl_c.presses() + 1, Ordering::SeqCst);
            ctrl_c.allow_one_more();
        }),
        requested: std::sync::Arc::new(move || {
            ctrl_c.presses() >= cancel_at.load(Ordering::SeqCst)
        }),
        key: "Ctrl+C",
    };
    let mic_role = MicRole::from(args.mic_role);
    let mode = args.capture_mode();
    let config = LiveConfig {
        title: args.meeting.title.clone(),
        clock_domain: ClockDomain::Qpc,
        ..LiveConfig::default()
    };
    if mode == CaptureMode::InRoom {
        writeln!(out, "in the room: mic only, no loopback or echo canceller")?;
    }
    match args.seconds {
        Some(s) => writeln!(out, "recording for {s} s (Ctrl+C stops earlier)")?,
        None => writeln!(out, "recording until Ctrl+C")?,
    }
    crate::session::run_meeting(
        &args.meeting,
        config,
        wasapi_starter(mode, mic_role),
        &|| {
            rule.borrow_mut()
                .should_stop(Instant::now(), ctrl_c.presses() >= 1)
        },
        Some(&cleanup),
        None,
        out,
    )
}

/// Not available off Windows.
///
/// # Errors
/// Always.
#[cfg(not(windows))]
pub fn run(_args: &RecordArgs, _out: &mut dyn Write) -> Result<EntityId> {
    anyhow::bail!("record needs Windows (WASAPI)")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_stops_after_the_limit_from_the_first_check() {
        let t0 = Instant::now();
        let mut rule = StopRule::new(Some(2.0)).unwrap();
        assert!(!rule.should_stop(t0, false));
        assert!(!rule.should_stop(t0 + Duration::from_millis(1_999), false));
        assert!(rule.should_stop(t0 + Duration::from_secs(2), false));
    }

    #[test]
    fn record_stops_on_request_with_or_without_a_limit() {
        let t0 = Instant::now();
        let mut rule = StopRule::new(None).unwrap();
        assert!(!rule.should_stop(t0, false));
        assert!(
            !rule.should_stop(t0 + Duration::from_secs(86_400), false),
            "no limit: only a request stops it"
        );
        assert!(rule.should_stop(t0 + Duration::from_secs(1), true));
        let mut rule = StopRule::new(Some(60.0)).unwrap();
        assert!(rule.should_stop(t0, true));
    }

    #[test]
    fn record_rejects_unusable_limits() {
        for bad in [0.0, -1.0, f64::NAN, f64::INFINITY] {
            assert!(StopRule::new(Some(bad)).is_err(), "{bad}");
        }
    }
}
