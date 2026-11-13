//! A finished raw recording (mic + loopback) and its `timestamps.json` description.
//! Platform independent.

use serde::Serialize;

#[cfg(test)]
use crate::endpoint::hands_free_warning;
use crate::endpoint::{EndpointProperties, HandsFreeFollower, HandsFreeMic, MicProcessing};
use crate::follow::ReopenReason;
use crate::format::MixFormat;
use crate::observer::{ReopenOutcome, TrackObserver, reopen_is_reported};
use crate::timeline::{PacketInfo, PacketRecord, QPC_UNITS_PER_SEC, TrackTimeline};

/// Version of the `timestamps.json` layout. v2 adds `loopback_source` and per-track
/// `device_changes`. v3: per-track `wav_channels` is the WAV's real channel count
/// (mic 1, loopback the device's channels, normally 2, interleaved); every frame
/// count and position stays in frames. v4: a mic `device_changes` entry carries
/// `hands_free` when the re-open switched onto a Bluetooth hands-free mic (absent
/// otherwise).
pub const TIMESTAMPS_SCHEMA_VERSION: u32 = 4;

/// The Bluetooth hands-free warning of a mic re-open (schema v4), judged by the same
/// [`HandsFreeFollower`] rule as the live mic track.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct HandsFreeChange {
    /// The new mic's mix-format rate (Hz), when known.
    pub sample_rate: Option<u32>,
}

/// A stream re-open during a recording (device switch, device loss or recovery).
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct DeviceChange {
    /// QPC position when the old stream was closed (100 ns units).
    pub qpc: u64,
    /// Track frame where the discontinuity starts; the gap up to the new stream's
    /// first packet is filled with silence.
    pub track_frame: u64,
    /// Why the stream was re-opened.
    pub reason: ReopenReason,
    /// New endpoint name, `None` when no device could be opened.
    pub device_name: Option<String>,
    /// Native mix format of the new endpoint (the audio engine converts it to the
    /// track rate when it differs), `None` when no device could be opened.
    pub device_format: Option<MixFormat>,
    /// Why the new device could not be opened, if it could not.
    pub error: Option<String>,
    /// Set when this re-open switched the mic onto a Bluetooth hands-free mic (schema
    /// v4; left out of the JSON otherwise).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hands_free: Option<HandsFreeChange>,
}

impl DeviceChange {
    /// The warning to show for this change: a switch onto a Bluetooth hands-free mic.
    #[must_use]
    pub fn hands_free_warning(&self) -> Option<HandsFreeMic> {
        let hf = self.hands_free?;
        Some(HandsFreeMic {
            device_name: self.device_name.clone().unwrap_or_default(),
            sample_rate: hf.sample_rate,
        })
    }
}

/// How the loopback track was captured.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LoopbackMode {
    /// Process loopback of one process tree (device independent).
    Process,
    /// Loopback of the default console render endpoint.
    DefaultDevice,
}

/// The loopback source decision, recorded in `timestamps.json`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct LoopbackSource {
    /// The `--loopback-process` value as requested (`auto`, `default-device`, a name or a pid).
    pub requested: String,
    /// Chosen capture mode.
    pub mode: LoopbackMode,
    /// Target process id (process mode only).
    pub pid: Option<u32>,
    /// Target executable name, when known (process mode only).
    pub process_name: Option<String>,
    /// Known call app label, when recognised (process mode only).
    pub app: Option<String>,
    /// Why the default device is used although it was not requested.
    pub fallback_reason: Option<String>,
}

/// One captured track: samples plus everything needed to describe it.
#[derive(Clone, Debug)]
pub struct TrackRecording {
    /// Endpoint friendly name.
    pub device_name: String,
    /// Native mix format the samples were converted from.
    pub format: MixFormat,
    /// Assembled timeline (samples and packet log).
    pub timeline: TrackTimeline,
    /// Stream re-opens, in order.
    pub device_changes: Vec<DeviceChange>,
}

impl TrackRecording {
    /// f32 samples at the native rate, interleaved with [`channels`](Self::channels)
    /// per frame.
    pub fn samples(&self) -> &[f32] {
        self.timeline.samples()
    }

    /// Channels per frame (also the WAV channel count): 1 for the mic, the device
    /// channels for the loopback.
    pub fn channels(&self) -> u16 {
        self.timeline.channels()
    }

    /// Native sample rate (also the WAV rate).
    pub fn sample_rate(&self) -> u32 {
        self.format.sample_rate
    }

    /// Serializable description of this track.
    pub fn report(&self) -> TrackReport {
        let packets = self.timeline.packets();
        TrackReport {
            device_name: self.device_name.clone(),
            format: self.format,
            wav_sample_rate: self.format.sample_rate,
            wav_channels: self.timeline.channels(),
            wav_start_qpc: self.timeline.start_qpc(),
            frames_written: self.timeline.frames(),
            synthesized_silence_frames: self.timeline.synthesized_frames(),
            silence_gaps: self.timeline.silence_gaps(),
            discontinuities: packets.iter().filter(|p| p.discontinuity).count() as u32,
            timestamp_errors: packets.iter().filter(|p| p.timestamp_error).count() as u32,
            packet_count: packets.len() as u64,
            device_changes: self.device_changes.clone(),
            packets: packets.to_vec(),
        }
    }
}

/// A track being recorded by `record_raw`: its timeline and re-open log, fed by the
/// capture loop as a [`TrackObserver`].
#[derive(Clone, Debug)]
pub struct RecordingTrack {
    first: DeviceDescription,
    timeline: TrackTimeline,
    changes: Vec<DeviceChange>,
    /// Outputs the call plays on that could not be opened (T6.15), each with why.
    refused: Vec<String>,
    /// Judges mic re-opens for the Bluetooth hands-free warning (the mic track only).
    hands_free: Option<HandsFreeFollower>,
}

impl RecordingTrack {
    /// Track first opened on `first`, assembled into `timeline`.
    pub fn new(first: DeviceDescription, timeline: TrackTimeline) -> Self {
        Self {
            first,
            timeline,
            changes: Vec::new(),
            refused: Vec::new(),
            hands_free: None,
        }
    }

    /// The outputs the call plays on that could not be opened (T6.15), each with why. No device
    /// change happened, so `timestamps.json` does not list them.
    #[must_use]
    pub fn refused_call_outputs(&self) -> &[String] {
        &self.refused
    }

    /// A mic track that opened `start` (its properties): each re-open that switches onto
    /// a Bluetooth hands-free mic is logged with [`DeviceChange::hands_free`], once per
    /// switch ([`HandsFreeFollower`], the rule the live mic track uses). The start mic
    /// is the caller's to warn about.
    #[must_use]
    pub fn with_mic_warnings(mut self, start: Option<&EndpointProperties>) -> Self {
        self.hands_free = Some(HandsFreeFollower::new(start));
        self
    }

    /// Pads the timeline up to `end_qpc` (capture stop) and returns the recording.
    pub fn finish(mut self, end_qpc: u64) -> TrackRecording {
        self.timeline.finish(end_qpc);
        TrackRecording {
            device_name: self.first.name,
            format: self.first.format,
            timeline: self.timeline,
            device_changes: self.changes,
        }
    }
}

impl TrackObserver for RecordingTrack {
    fn packet(&mut self, info: PacketInfo, mono: &[f32]) {
        self.timeline.push(info, mono);
    }

    /// Marks the discontinuity in the timeline (every attempt) and logs reported
    /// re-opens at the track frame where it starts.
    fn reopened(&mut self, qpc: u64, reason: ReopenReason, outcome: ReopenOutcome<'_>) {
        let track_frame = self.timeline.mark_discontinuity();
        if !reopen_is_reported(reason, outcome.is_opened()) {
            return;
        }
        let (device_name, device_format, error, hands_free) = match outcome {
            ReopenOutcome::Opened {
                device_name,
                device_format,
                endpoint,
                ..
            } => {
                let hands_free = self
                    .hands_free
                    .as_mut()
                    .and_then(|f| f.opened(endpoint))
                    .map(|w| HandsFreeChange {
                        sample_rate: w.sample_rate,
                    });
                (
                    Some(device_name.to_owned()),
                    Some(device_format),
                    None,
                    hands_free,
                )
            }
            ReopenOutcome::Failed { error } => (None, None, Some(error.to_owned()), None),
        };
        self.changes.push(DeviceChange {
            qpc,
            track_frame,
            reason,
            device_name,
            device_format,
            error,
            hands_free,
        });
    }

    fn call_output_refused(&mut self, _qpc: u64, wanted: &str, _used: Option<&str>, error: &str) {
        self.refused.push(format!("{wanted}: {error}"));
    }

    /// Logs the end as a change with no device and the error only as a number
    /// ([`follow::track_ended_detail`](crate::follow::track_ended_detail)); the timeline is
    /// padded with silence to the end of the recording by [`finish`](Self::finish).
    fn track_ended(&mut self, qpc: u64, hresult: i32) {
        let track_frame = self.timeline.mark_discontinuity();
        self.changes.push(DeviceChange {
            qpc,
            track_frame,
            reason: ReopenReason::StreamError,
            device_name: None,
            device_format: None,
            error: Some(crate::follow::track_ended_detail(hresult)),
            hands_free: None,
        });
    }
}

/// A [`RecordingTrack`] that also hands each Bluetooth hands-free warning it logs to
/// `notify` at once, so `record_raw`'s caller can show it during the recording.
pub struct NotifyingTrack<F> {
    track: RecordingTrack,
    notify: F,
}

impl<F: FnMut(&HandsFreeMic)> NotifyingTrack<F> {
    /// Wraps `track` (normally built [`with_mic_warnings`](RecordingTrack::with_mic_warnings)).
    pub fn new(track: RecordingTrack, notify: F) -> Self {
        Self { track, notify }
    }

    /// The wrapped track.
    pub fn into_track(self) -> RecordingTrack {
        self.track
    }
}

impl<F: FnMut(&HandsFreeMic)> TrackObserver for NotifyingTrack<F> {
    fn packet(&mut self, info: PacketInfo, samples: &[f32]) {
        self.track.packet(info, samples);
    }

    fn reopened(&mut self, qpc: u64, reason: ReopenReason, outcome: ReopenOutcome<'_>) {
        let before = self.track.changes.len();
        self.track.reopened(qpc, reason, outcome);
        if let Some(w) = self.track.changes[before..]
            .iter()
            .find_map(DeviceChange::hands_free_warning)
        {
            (self.notify)(&w);
        }
    }

    fn track_ended(&mut self, qpc: u64, hresult: i32) {
        self.track.track_ended(qpc, hresult);
    }
}

/// Which default capture endpoint is used as the microphone. Default-device loopback
/// always uses the console default render endpoint, so `Console` pairs devices of the
/// same role.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MicRole {
    /// Default console capture device (`eConsole`).
    #[default]
    Console,
    /// Default communications capture device (`eCommunications`).
    Communications,
}

/// Endpoint name and mix format, reported before capture starts.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeviceDescription {
    /// Endpoint friendly name.
    pub name: String,
    /// Native mix format.
    pub format: MixFormat,
}

/// Mic and loopback captured over the same interval.
#[derive(Clone, Debug)]
pub struct RawRecording {
    /// Role of the capture endpoint used for the mic track.
    pub mic_role: MicRole,
    /// How the mic stream was opened (T12.32): with Windows' default signal processing, or
    /// raw. Not written to `timestamps.json` (schema v4 has no field for it).
    pub mic_processing: MicProcessing,
    /// Requested duration in seconds.
    pub requested_seconds: f64,
    /// The outputs the call played on that could not be opened (T6.15), each with why.
    pub refused_call_outputs: Vec<String>,
    /// Origin QPC (100 ns units), taken just before the streams were started. Frame 0 of
    /// both WAVs is at this position.
    pub start_qpc: u64,
    /// QPC position when capture stopped.
    pub end_qpc: u64,
    /// How the loopback track was captured.
    pub loopback_source: LoopbackSource,
    /// Microphone track.
    pub mic: TrackRecording,
    /// Loopback track (a process tree or the default render endpoint).
    pub loopback: TrackRecording,
}

impl RawRecording {
    /// The `timestamps.json` document.
    pub fn timestamps(&self) -> TimestampsFile {
        TimestampsFile {
            schema_version: TIMESTAMPS_SCHEMA_VERSION,
            qpc_units_per_second: QPC_UNITS_PER_SEC,
            mic_role: self.mic_role,
            requested_seconds: self.requested_seconds,
            start_qpc: self.start_qpc,
            end_qpc: self.end_qpc,
            loopback_source: self.loopback_source.clone(),
            mic: self.mic.report(),
            loopback: self.loopback.report(),
        }
    }
}

/// Root of `timestamps.json`.
#[derive(Clone, Debug, Serialize)]
pub struct TimestampsFile {
    /// Layout version.
    pub schema_version: u32,
    /// Units of every `*qpc` field per second (QPC positions are 100 ns).
    pub qpc_units_per_second: u64,
    /// Role of the capture endpoint used for the mic track.
    pub mic_role: MicRole,
    /// Requested duration.
    pub requested_seconds: f64,
    /// Origin: frame 0 of both WAVs (taken just before the streams were started).
    pub start_qpc: u64,
    /// Capture stop.
    pub end_qpc: u64,
    /// How the loopback track was captured.
    pub loopback_source: LoopbackSource,
    /// `mic.wav` description.
    pub mic: TrackReport,
    /// `loopback.wav` description.
    pub loopback: TrackReport,
}

/// Per-track section of `timestamps.json`.
#[derive(Clone, Debug, Serialize)]
pub struct TrackReport {
    /// Endpoint friendly name.
    pub device_name: String,
    /// Native mix format delivered by the device.
    pub format: MixFormat,
    /// Sample rate of the WAV file (the native rate; no resampling).
    pub wav_sample_rate: u32,
    /// WAV channel count: 1 for the mic (downmixed), the device channels for the
    /// loopback (interleaved; schema v3).
    pub wav_channels: u16,
    /// QPC position of WAV frame 0.
    pub wav_start_qpc: u64,
    /// Frames in the WAV file (samples per channel).
    pub frames_written: u64,
    /// Frames of synthesized silence (gaps without packets).
    pub synthesized_silence_frames: u64,
    /// Number of silence insertions.
    pub silence_gaps: u32,
    /// Packets flagged as data discontinuities.
    pub discontinuities: u32,
    /// Packets flagged with a timestamp error.
    pub timestamp_errors: u32,
    /// Number of packets received.
    pub packet_count: u64,
    /// Stream re-opens (device switches and recoveries), in order.
    pub device_changes: Vec<DeviceChange>,
    /// Every packet received.
    pub packets: Vec<PacketRecord>,
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::format::SampleFormat;

    /// Canned recording: 1 s at 48 kHz, mic with 100 packets (one discontinuity),
    /// stereo loopback with no packets at all.
    pub(crate) fn canned() -> RawRecording {
        let t0 = 5_000_000;
        let fmt = MixFormat {
            sample_rate: 48_000,
            channels: 2,
            block_align: 8,
            bits_per_sample: 32,
            valid_bits_per_sample: 32,
            sample_format: SampleFormat::F32,
        };
        let mut mic = TrackTimeline::new(48_000, t0);
        for i in 0..100u64 {
            let info = PacketInfo {
                qpc: t0 + i * 100_000,
                device_position: i * 480,
                frames: 480,
                discontinuity: i == 0,
                ..Default::default()
            };
            mic.push(info, &[0.25; 480]);
        }
        let end = t0 + QPC_UNITS_PER_SEC;
        mic.finish(end);
        let mut lb = TrackTimeline::new(48_000, t0).with_channels(2);
        lb.finish(end);
        RawRecording {
            mic_role: MicRole::Console,
            mic_processing: MicProcessing::Default,
            requested_seconds: 1.0,
            refused_call_outputs: Vec::new(),
            start_qpc: t0,
            end_qpc: end,
            loopback_source: LoopbackSource {
                requested: "auto".into(),
                mode: LoopbackMode::DefaultDevice,
                pid: None,
                process_name: None,
                app: None,
                fallback_reason: Some("no call app with an active audio session".into()),
            },
            mic: TrackRecording {
                device_name: "Mic (Test)".into(),
                format: fmt,
                timeline: mic,
                device_changes: vec![
                    DeviceChange {
                        qpc: t0 + 500_000,
                        track_frame: 2_400,
                        reason: ReopenReason::DefaultDeviceChanged,
                        device_name: Some("Headset (Test)".into()),
                        device_format: Some(fmt),
                        error: None,
                        hands_free: None,
                    },
                    DeviceChange {
                        qpc: t0 + 700_000,
                        track_frame: 3_360,
                        reason: ReopenReason::DefaultDeviceChanged,
                        device_name: Some("Headset (Hands-Free)".into()),
                        device_format: Some(fmt),
                        error: None,
                        hands_free: Some(HandsFreeChange {
                            sample_rate: Some(16_000),
                        }),
                    },
                ],
            },
            loopback: TrackRecording {
                device_name: "Speakers (Test)".into(),
                format: fmt,
                timeline: lb,
                device_changes: Vec::new(),
            },
        }
    }

    fn fmt48() -> MixFormat {
        MixFormat {
            sample_rate: 48_000,
            channels: 2,
            block_align: 8,
            bits_per_sample: 32,
            valid_bits_per_sample: 32,
            sample_format: SampleFormat::F32,
        }
    }

    #[test]
    fn recording_track_logs_reported_reopens_at_the_track_frame() {
        let t0 = 5_000_000;
        let first = DeviceDescription {
            name: "Mic".into(),
            format: fmt48(),
        };
        let mut t = RecordingTrack::new(first, TrackTimeline::new(48_000, t0));
        let pkt = |qpc| PacketInfo {
            qpc,
            frames: 480,
            ..Default::default()
        };
        t.packet(pkt(t0), &[0.5; 480]);
        t.reopened(
            t0 + 200_000,
            ReopenReason::StreamError,
            ReopenOutcome::Failed { error: "lost" },
        );
        // Periodic retry failures are not logged, but still re-anchor the timeline.
        t.reopened(
            t0 + 300_000,
            ReopenReason::Retry,
            ReopenOutcome::Failed {
                error: "still lost",
            },
        );
        t.reopened(
            t0 + 400_000,
            ReopenReason::Retry,
            ReopenOutcome::Opened {
                device_name: "Headset",
                device_format: fmt48(),
                rate: 48_000,
                endpoint: None,
                gave_up: None,
            },
        );
        // 3 ms after the old stream ended: filled exactly after a discontinuity.
        t.packet(pkt(t0 + 130_000), &[0.5; 480]);
        let r = t.finish(t0 + 230_000);
        assert_eq!(r.device_name, "Mic");
        assert_eq!(r.timeline.packets()[1].silence_before, 144);
        let changes: Vec<_> = r
            .device_changes
            .iter()
            .map(|c| {
                (
                    c.qpc,
                    c.track_frame,
                    c.reason,
                    c.device_name.clone(),
                    c.error.clone(),
                )
            })
            .collect();
        assert_eq!(
            changes,
            vec![
                (
                    t0 + 200_000,
                    480,
                    ReopenReason::StreamError,
                    None,
                    Some("lost".to_owned())
                ),
                (
                    t0 + 400_000,
                    480,
                    ReopenReason::Retry,
                    Some("Headset".to_owned()),
                    None
                ),
            ]
        );
        assert_eq!(r.device_changes[1].device_format, Some(fmt48()));
        assert_eq!(r.samples().len(), 480 + 144 + 480);
    }

    fn hands_free_mic() -> EndpointProperties {
        EndpointProperties {
            name: "Headset (Hands-Free)".into(),
            enumerator: Some("BTHHFENUM".into()),
            sample_rate: Some(16_000),
            ..Default::default()
        }
    }

    fn usb_mic() -> EndpointProperties {
        EndpointProperties {
            name: "USB".into(),
            enumerator: Some("USB".into()),
            sample_rate: Some(48_000),
            ..Default::default()
        }
    }

    fn opened(p: &EndpointProperties) -> ReopenOutcome<'_> {
        ReopenOutcome::Opened {
            device_name: &p.name,
            device_format: fmt48(),
            rate: 48_000,
            endpoint: Some(p),
            gave_up: None,
        }
    }

    #[test]
    fn recording_track_records_each_switch_onto_a_hands_free_mic() {
        let t0 = 5_000_000;
        let (hfp, usb) = (hands_free_mic(), usb_mic());
        let first = DeviceDescription {
            name: usb.name.clone(),
            format: fmt48(),
        };
        let switch = ReopenReason::DefaultDeviceChanged;
        let mut t = RecordingTrack::new(first, TrackTimeline::new(48_000, t0))
            .with_mic_warnings(Some(&usb));
        t.reopened(t0 + 1, switch, opened(&hfp));
        t.reopened(t0 + 2, ReopenReason::StreamError, opened(&hfp));
        // Lost and back on the same hands-free mic: not a new switch.
        let lost = ReopenOutcome::Failed { error: "gone" };
        t.reopened(t0 + 3, ReopenReason::StreamError, lost);
        t.reopened(t0 + 4, ReopenReason::Retry, opened(&hfp));
        t.reopened(t0 + 5, switch, opened(&usb));
        t.reopened(t0 + 6, switch, opened(&hfp));
        let r = t.finish(t0 + 100_000);
        let warned: Vec<_> = r.device_changes.iter().map(|c| c.hands_free).collect();
        let hf = Some(HandsFreeChange {
            sample_rate: Some(16_000),
        });
        assert_eq!(warned, vec![hf, None, None, None, None, hf]);
        assert_eq!(
            r.device_changes[0].hands_free_warning(),
            hands_free_warning(&hfp),
            "the warning the live emitter would give"
        );
        assert_eq!(r.device_changes[1].hands_free_warning(), None);

        // Without mic warnings (the loopback track) nothing is ever recorded.
        let first = DeviceDescription {
            name: "Speakers".into(),
            format: fmt48(),
        };
        let mut lb = RecordingTrack::new(first, TrackTimeline::new(48_000, t0));
        lb.reopened(t0 + 1, switch, opened(&hfp));
        let r = lb.finish(t0 + 100_000);
        assert_eq!(r.device_changes[0].hands_free, None);
    }

    #[test]
    fn recording_track_started_on_a_hands_free_mic_leaves_that_warning_to_the_start() {
        let t0 = 5_000_000;
        let hfp = hands_free_mic();
        let first = DeviceDescription {
            name: hfp.name.clone(),
            format: fmt48(),
        };
        let mut t = RecordingTrack::new(first, TrackTimeline::new(48_000, t0))
            .with_mic_warnings(Some(&hfp));
        t.reopened(t0 + 1, ReopenReason::StreamError, opened(&hfp));
        assert_eq!(t.finish(t0 + 100_000).device_changes[0].hands_free, None);
    }

    #[test]
    fn notifying_track_hands_each_new_warning_to_the_caller_as_it_is_recorded() {
        let t0 = 5_000_000;
        let (hfp, usb) = (hands_free_mic(), usb_mic());
        let first = DeviceDescription {
            name: usb.name.clone(),
            format: fmt48(),
        };
        let mut heard = Vec::new();
        let track = RecordingTrack::new(first, TrackTimeline::new(48_000, t0))
            .with_mic_warnings(Some(&usb));
        let mut n = NotifyingTrack::new(track, |w: &HandsFreeMic| heard.push(w.clone()));
        let switch = ReopenReason::DefaultDeviceChanged;
        n.reopened(t0 + 1, switch, opened(&hfp));
        n.reopened(t0 + 2, ReopenReason::StreamError, opened(&hfp));
        n.reopened(t0 + 3, switch, opened(&usb));
        let r = n.into_track().finish(t0 + 100_000);
        assert_eq!(r.device_changes.len(), 3);
        assert_eq!(heard, vec![hands_free_warning(&hfp).unwrap()]);
    }

    #[test]
    fn recording_track_logs_an_ended_track_with_the_error_as_a_number_only() {
        let t0 = 5_000_000;
        let first = DeviceDescription {
            name: "Mic".into(),
            format: fmt48(),
        };
        let track = RecordingTrack::new(first, TrackTimeline::new(48_000, t0));
        let mut n = NotifyingTrack::new(track, |_: &HandsFreeMic| {});
        n.track_ended(t0 + 20_000_000, 0x8889_0017_u32 as i32);
        let r = n.into_track().finish(t0 + 30_000_000);
        assert_eq!(r.device_changes.len(), 1);
        let change = &r.device_changes[0];
        assert_eq!(change.reason, ReopenReason::StreamError);
        assert_eq!(change.device_name, None);
        assert_eq!(
            change.error.as_deref(),
            Some("stream error 0x88890017; the track ended")
        );
        // Silence up to the end of the recording.
        assert_eq!(r.timeline.frames(), 3 * 48_000);
    }

    #[test]
    fn recording_track_keeps_stereo_frames_across_a_reopen() {
        let t0 = 5_000_000;
        let first = DeviceDescription {
            name: "Speakers".into(),
            format: fmt48(),
        };
        let timeline = TrackTimeline::new(48_000, t0).with_channels(2);
        let mut t = RecordingTrack::new(first, timeline);
        let pkt = |qpc| PacketInfo {
            qpc,
            frames: 480,
            ..Default::default()
        };
        let lr: Vec<f32> = (0..480).flat_map(|_| [0.5, -0.5]).collect();
        t.packet(pkt(t0), &lr);
        t.reopened(
            t0 + 100_000,
            ReopenReason::DefaultDeviceChanged,
            ReopenOutcome::Opened {
                device_name: "Headphones",
                device_format: fmt48(),
                rate: 48_000,
                endpoint: None,
                gave_up: None,
            },
        );
        t.packet(pkt(t0 + 130_000), &lr);
        let r = t.finish(t0 + 230_000);
        assert_eq!(r.channels(), 2);
        assert_eq!(r.device_changes[0].track_frame, 480, "in frames");
        assert_eq!(r.timeline.packets()[1].silence_before, 144);
        assert_eq!(r.timeline.frames(), 480 + 144 + 480);
        assert_eq!(r.samples().len(), 2 * (480 + 144 + 480));
        let report = r.report();
        assert_eq!((report.wav_channels, report.frames_written), (2, 1_104));
    }

    #[test]
    fn call_output_refused_is_kept_apart_from_the_device_changes() {
        let first = DeviceDescription {
            name: "Speakers".into(),
            format: fmt48(),
        };
        let mut t = RecordingTrack::new(first, TrackTimeline::new(48_000, 1_000));
        t.call_output_refused(2_000, "Headset", Some("Speakers"), "in use");
        assert_eq!(t.refused_call_outputs(), ["Headset: in use"]);
        assert!(t.finish(3_000).device_changes.is_empty());
    }

    #[test]
    fn timestamps_json_shape() {
        let v = serde_json::to_value(canned().timestamps()).unwrap();
        assert_eq!(v["schema_version"], 4);
        assert_eq!(v["qpc_units_per_second"], 10_000_000);
        assert_eq!(v["start_qpc"], 5_000_000);
        assert_eq!(v["end_qpc"], 15_000_000);
        assert_eq!(v["requested_seconds"], 1.0);
        assert_eq!(v["mic_role"], "console");
        assert_eq!(v["mic"]["wav_start_qpc"], v["start_qpc"]);
        assert_eq!(v["loopback"]["wav_start_qpc"], v["start_qpc"]);
        // Frame 0 of the WAV is the packet stamped at the origin, not just a copied field.
        assert_eq!(v["mic"]["packets"][0]["qpc"], v["start_qpc"]);
        assert_eq!(v["mic"]["packets"][0]["track_frame"], 0);
        assert_eq!(v["mic"]["packets"][0]["silence_before"], 0);
        assert_eq!(v["mic"]["packets"][0]["trimmed_frames"], 0);
        for track in ["mic", "loopback"] {
            let t = &v[track];
            for key in [
                "device_name",
                "format",
                "wav_sample_rate",
                "wav_channels",
                "wav_start_qpc",
                "frames_written",
                "synthesized_silence_frames",
                "silence_gaps",
                "discontinuities",
                "timestamp_errors",
                "packet_count",
                "device_changes",
                "packets",
            ] {
                assert!(t.get(key).is_some(), "{track}.{key} missing");
            }
            assert_eq!(t["wav_sample_rate"], 48_000);
            assert_eq!(t["frames_written"], 48_000, "frames, not samples");
            assert_eq!(t["format"]["sample_format"], "f32");
            assert_eq!(t["format"]["channels"], 2);
        }
        // v3: the mic is downmixed, the loopback keeps its device channels.
        assert_eq!(v["mic"]["wav_channels"], 1);
        assert_eq!(v["loopback"]["wav_channels"], 2);
        let mic = &v["mic"];
        assert_eq!(mic["device_name"], "Mic (Test)");
        assert_eq!(mic["packet_count"], 100);
        assert_eq!(mic["discontinuities"], 1);
        assert_eq!(mic["synthesized_silence_frames"], 0);
        let p = &mic["packets"][1];
        for key in [
            "qpc",
            "device_position",
            "frames",
            "track_frame",
            "silence_before",
            "trimmed_frames",
            "silent",
            "discontinuity",
            "timestamp_error",
        ] {
            assert!(p.get(key).is_some(), "packet.{key} missing");
        }
        assert_eq!(p["qpc"], 5_100_000);
        assert_eq!(p["track_frame"], 480);
        let src = &v["loopback_source"];
        assert_eq!(src["requested"], "auto");
        assert_eq!(src["mode"], "default_device");
        assert!(src["pid"].is_null());
        assert_eq!(
            src["fallback_reason"],
            "no call app with an active audio session"
        );
        let ch = &mic["device_changes"][0];
        assert_eq!(ch["reason"], "default_device_changed");
        assert_eq!(ch["track_frame"], 2_400);
        assert_eq!(ch["qpc"], 5_500_000);
        assert_eq!(ch["device_name"], "Headset (Test)");
        assert_eq!(ch["device_format"]["sample_rate"], 48_000);
        assert!(ch["error"].is_null());
        // v4: `hands_free` only on a switch onto a Bluetooth hands-free mic.
        assert!(ch.get("hands_free").is_none(), "absent without a warning");
        let hf = &mic["device_changes"][1];
        assert_eq!(
            hf["hands_free"],
            serde_json::json!({ "sample_rate": 16_000 })
        );
        let lb = &v["loopback"];
        assert_eq!(lb["device_changes"].as_array().unwrap().len(), 0);
        assert_eq!(lb["packet_count"], 0);
        assert_eq!(lb["synthesized_silence_frames"], 48_000);
        assert_eq!(lb["silence_gaps"], 1);
    }
}
