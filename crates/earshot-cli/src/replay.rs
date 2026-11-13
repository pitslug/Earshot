//! `earshot-cli replay`: a recorded mic (and optionally the far-end loopback) through the
//! runtime as if it were captured live, then the post pass, summary and export of
//! [`crate::session`].
//!
//! The files are read at their own rate; the mic is mixed to mono (capture mics are mono)
//! and the reference keeps its channels (the echo canceller takes a stereo loopback). Both
//! start at the same instant and are delivered in 10 ms packets with exact timestamps
//! ([`ReplayBuilder`]), as fast as the session consumes them. With `--timeline` the
//! packets carry the device timestamps a raw recording saved (`timestamps.json` of
//! `earshot-cli record-raw`, [`ReplayBuilder::from_raw_timeline`]), so clock skew, drift
//! steering and jitter behave as they did live. No audio device is used.

use std::io::Write;
use std::num::NonZeroU16;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Result, anyhow};
use clap::Args;
use earshot_core::capture::{
    DeviceTime, RawProcessing, RawTimeline, ReplayBuilder, TrackBuffer, mix_to_mono,
};
use earshot_core::model::ClockDomain;
use earshot_runtime::live::{LiveConfig, start_live};
use earshot_runtime::replay::{PaceState, Paced, WallClock};

use crate::events::EventLog;
use crate::session::{MeetingOptions, run_meeting};
use crate::transcribe::read_track;

/// Device time of the first replayed frame (any value works; this is 1 s).
const ORIGIN: DeviceTime = DeviceTime::from_ticks(10_000_000);

/// `earshot-cli replay` arguments.
#[derive(Debug, Clone, Args)]
pub struct ReplayArgs {
    /// The microphone recording (WAV or FLAC; mixed to mono).
    #[arg(long)]
    pub mic: PathBuf,
    /// The far-end loopback recording (WAV or FLAC, any channel count), the echo
    /// canceller's reference. Without it the meeting has the mic track only.
    #[arg(long = "ref", value_name = "REF")]
    pub reference: Option<PathBuf>,
    /// Deliver the audio at real-time speed (wall clock) instead of as fast as the session
    /// consumes it, so the printed live ASR backlog shows whether this host keeps up.
    #[arg(long)]
    pub realtime: bool,
    /// The `timestamps.json` of an `earshot-cli record-raw` recording whose `mic.wav` and
    /// `loopback.wav` are `--mic` and `--ref`: packets carry the recorded device
    /// timestamps instead of exact ones.
    #[arg(long, requires = "reference")]
    pub timeline: Option<PathBuf>,
    /// Write the session's events and cleanup steps as timestamped JSON lines to this file
    /// (no transcript text; [`crate::events`]), for timing harnesses. With `--realtime` it
    /// marks when the audio started.
    #[arg(long, value_name = "PATH")]
    pub events_json: Option<PathBuf>,
    /// Data, model, post-pass, summary and export options.
    #[command(flatten)]
    pub meeting: MeetingOptions,
}

/// WASAPI's live queue: past this much waiting audio, a real device drops frames
/// (`earshot_capture_wasapi::LIVE_QUEUE_CAP`, 30 s).
const DEVICE_QUEUE: Duration = Duration::from_secs(30);

/// `track` mixed to one channel (the mean of each frame).
fn to_mono(track: TrackBuffer) -> Result<TrackBuffer> {
    if track.channels() == NonZeroU16::MIN {
        return Ok(track);
    }
    let mut mono = Vec::with_capacity(track.samples().len());
    mix_to_mono(track.samples(), track.channels(), &mut mono);
    TrackBuffer::new(track.rate(), NonZeroU16::MIN, mono).map_err(|e| anyhow!("{e:?}"))
}

/// The line saying a `--timeline` take's mic was recorded raw (schema v5, T12.32), or `None`
/// for the default stream. The replay itself is the same either way.
fn processing_line(timeline: &RawTimeline) -> Option<&'static str> {
    match timeline.mic.processing {
        RawProcessing::Default => None,
        RawProcessing::Raw => Some(
            "replay: this take's mic was recorded raw (without the signal processing Windows or the device applies by default)",
        ),
    }
}

/// Runs the replay, printing to `out`.
///
/// # Errors
/// Unreadable input, and as [`run_meeting`].
pub fn run(args: &ReplayArgs, out: &mut dyn Write) -> Result<earshot_core::identity::EntityId> {
    let builder = match (&args.timeline, &args.reference) {
        (Some(timeline), Some(reference)) => {
            let bytes = std::fs::read(timeline)
                .map_err(|e| anyhow!("cannot read {}: {e}", timeline.display()))?;
            let timeline: RawTimeline = serde_json::from_slice(&bytes)
                .map_err(|e| anyhow!("{} is not a raw timeline: {e}", timeline.display()))?;
            if let Some(line) = processing_line(&timeline) {
                writeln!(out, "{line}")?;
            }
            ReplayBuilder::from_raw_timeline(
                &timeline,
                read_track(&args.mic)?,
                read_track(reference)?,
            )
            .map_err(|e| anyhow!("the recording does not match its timeline: {e:?}"))?
        }
        _ => {
            let mic = to_mono(read_track(&args.mic)?)?;
            let mut builder = ReplayBuilder::new(ORIGIN, mic);
            if let Some(path) = &args.reference {
                builder = builder.reference(read_track(path)?);
            }
            builder
        }
    };
    let source = builder
        .build()
        .map_err(|e| anyhow!("cannot replay these files: {e:?}"))?;
    let config = LiveConfig {
        title: args.meeting.title.clone(),
        clock_domain: ClockDomain::Replay,
        ..LiveConfig::default()
    };
    let pace = Arc::new(PaceState::default());
    let log = match &args.events_json {
        Some(path) => Some(Arc::new(EventLog::create(path)?)),
        None => None,
    };
    let start: crate::session::Starter = if args.realtime {
        let mut paced = Paced::new(source, Arc::new(WallClock::new()), true, Arc::clone(&pace));
        if let Some(log) = log.clone() {
            paced = paced.on_start(move || log.audio_origin());
        }
        Box::new(move |svc, cfg, asr, rng| start_live(paced, svc, cfg, asr, rng))
    } else {
        Box::new(move |svc, cfg, asr, rng| start_live(source, svc, cfg, asr, rng))
    };
    let meeting = run_meeting(
        &args.meeting,
        config,
        start,
        &|| false,
        None,
        log.as_deref(),
        out,
    )?;
    if args.realtime {
        writeln!(out, "{}", format_lag(pace.max_lag()))?;
    }
    Ok(meeting)
}

/// The `--realtime` capture-lag line (T6.10b).
fn format_lag(lag: Duration) -> String {
    let verdict = if lag >= DEVICE_QUEUE {
        "a live device would have DROPPED audio"
    } else {
        "no audio would have been dropped"
    };
    format!(
        "realtime: largest capture lag {:.2} s (a live device queues {} s; {verdict})",
        lag.as_secs_f64(),
        DEVICE_QUEUE.as_secs()
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use earshot_core::audio::SampleRate;

    #[test]
    fn replay_mic_is_mixed_to_mono_and_mono_is_unchanged() {
        let rate = SampleRate::new(48_000).unwrap();
        let stereo = TrackBuffer::new(
            rate,
            NonZeroU16::new(2).unwrap(),
            vec![1.0, 0.0, 0.5, 0.5, -1.0, 1.0],
        )
        .unwrap();
        let mono = to_mono(stereo).unwrap();
        assert_eq!(mono.channels().get(), 1);
        assert_eq!(mono.rate(), rate);
        assert_eq!(mono.samples(), &[0.5, 0.5, 0.0]);
        let same = TrackBuffer::new(rate, NonZeroU16::MIN, vec![0.25, -0.25]).unwrap();
        assert_eq!(to_mono(same.clone()).unwrap(), same);
    }

    #[test]
    fn replay_says_when_the_takes_mic_was_recorded_raw() {
        let json = |processing: &str| {
            format!(
                r#"{{"schema_version": 5, "qpc_units_per_second": 10000000, "start_qpc": 0,
                   "end_qpc": 1, "mic": {{"device_name": "m", "wav_sample_rate": 48000,
                   {processing} "frames_written": 0, "synthesized_silence_frames": 0,
                   "packets": []}}, "loopback": {{"device_name": "l",
                   "wav_sample_rate": 48000, "frames_written": 0,
                   "synthesized_silence_frames": 0, "packets": []}}}}"#
            )
        };
        let read = |p: &str| serde_json::from_str::<RawTimeline>(&json(p)).unwrap();
        assert_eq!(processing_line(&read("")), None);
        let line = processing_line(&read(r#""processing": "raw","#)).expect("a line");
        assert!(
            line.starts_with("replay: this take's mic was recorded raw"),
            "{line}"
        );
    }

    #[test]
    fn replay_realtime_lag_line_says_whether_a_device_would_drop() {
        let ok = format_lag(Duration::from_millis(1_250));
        assert!(
            ok.contains("1.25 s") && ok.contains("no audio would have been dropped"),
            "{ok}"
        );
        let bad = format_lag(Duration::from_secs(31));
        assert!(bad.contains("DROPPED"), "{bad}");
    }
}
