//! `earshot record-raw`: raw mic + loopback capture to WAV and `timestamps.json`
//! (schema v4). `mic.wav` is mono; `loopback.wav` keeps the loopback's channels
//! (stereo on a desktop and for process loopback), interleaved. No AEC, no resampling
//! (a re-opened device is asked for the track's rate and channel count).

// Off Windows only `run` (which bails) is reachable; the writer is still unit-tested.
#![cfg_attr(not(windows), allow(dead_code))]

use std::fs::{self, File};
use std::io::BufWriter;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use earshot_capture_wasapi::{
    DeviceDescription, LoopbackMode, LoopbackRequest, LoopbackSource, MicRole, RawRecording,
    SampleFormat, TrackRecording, TrackReport,
};

/// Mic track file name.
pub const MIC_WAV: &str = "mic.wav";
/// Loopback track file name.
pub const LOOPBACK_WAV: &str = "loopback.wav";
/// Packet timestamps file name.
pub const TIMESTAMPS_JSON: &str = "timestamps.json";

/// Paths written by [`write_outputs`].
#[derive(Debug)]
pub struct Outputs {
    /// `mic.wav`.
    pub mic: PathBuf,
    /// `loopback.wav`.
    pub loopback: PathBuf,
    /// `timestamps.json`.
    pub timestamps: PathBuf,
}

/// Writes a 32-bit float WAV at the track's native rate with the track's channels
/// (mic mono, loopback interleaved, normally stereo).
fn write_wav(path: &Path, track: &TrackRecording) -> Result<()> {
    let spec = hound::WavSpec {
        channels: track.channels(),
        sample_rate: track.sample_rate(),
        bits_per_sample: 32,
        sample_format: hound::SampleFormat::Float,
    };
    let ctx = || format!("cannot write {}", path.display());
    let mut w = hound::WavWriter::create(path, spec).with_context(ctx)?;
    for &s in track.samples() {
        w.write_sample(s).with_context(ctx)?;
    }
    w.finalize().with_context(ctx)
}

/// Writes `mic.wav`, `loopback.wav` and `timestamps.json` into `dir` (created if needed).
pub fn write_outputs(dir: &Path, rec: &RawRecording) -> Result<Outputs> {
    fs::create_dir_all(dir).with_context(|| format!("cannot create {}", dir.display()))?;
    let out = Outputs {
        mic: dir.join(MIC_WAV),
        loopback: dir.join(LOOPBACK_WAV),
        timestamps: dir.join(TIMESTAMPS_JSON),
    };
    write_wav(&out.mic, &rec.mic)?;
    write_wav(&out.loopback, &rec.loopback)?;
    let file = File::create(&out.timestamps)
        .with_context(|| format!("cannot create {}", out.timestamps.display()))?;
    serde_json::to_writer_pretty(BufWriter::new(file), &rec.timestamps())
        .with_context(|| format!("cannot write {}", out.timestamps.display()))?;
    Ok(out)
}

/// One stderr line naming a device and its mix format, printed before capture so the
/// mic/loopback pairing can be confirmed.
pub fn describe_device(label: &str, d: &DeviceDescription) -> String {
    let f = &d.format;
    let kind = match f.sample_format {
        SampleFormat::F32 => "f32",
        SampleFormat::I16 => "i16",
        SampleFormat::I24 => "i24",
        SampleFormat::I32 => "i32",
    };
    let valid = if f.valid_bits_per_sample != f.bits_per_sample {
        format!(" ({} valid bits)", f.valid_bits_per_sample)
    } else {
        String::new()
    };
    format!(
        "{label}: {} [{} Hz, {} ch, {kind}{valid}]",
        d.name, f.sample_rate, f.channels
    )
}

/// One stderr line naming the chosen loopback source and, for the default device,
/// why it was used.
pub fn describe_loopback_source(src: &LoopbackSource) -> String {
    match src.mode {
        LoopbackMode::Process => {
            let pid = src.pid.map_or_else(|| "?".to_owned(), |p| p.to_string());
            let target = match (&src.app, &src.process_name) {
                (Some(app), Some(exe)) => format!("{app} ({exe}, pid {pid})"),
                (None, Some(exe)) => format!("{exe} (pid {pid})"),
                (Some(app), None) => format!("{app} (pid {pid})"),
                (None, None) => format!("pid {pid}"),
            };
            format!(
                "loopback source: process loopback of {target} [requested: {}]",
                src.requested
            )
        }
        LoopbackMode::DefaultDevice => {
            let why = src.fallback_reason.as_deref().unwrap_or("requested");
            format!(
                "loopback source: default render device ({why}) [requested: {}]",
                src.requested
            )
        }
    }
}

/// One stderr line per track after capture: packet count, timestamp errors and
/// synthesized silence, so persistent bad timestamps or dropouts are visible.
pub fn describe_track_stats(label: &str, t: &TrackReport) -> String {
    let secs = t.synthesized_silence_frames as f64 / f64::from(t.wav_sample_rate.max(1));
    format!(
        "{label}: {} packets, {} timestamp errors, {} synthesized silence frames ({secs:.3} s), {} device changes",
        t.packet_count,
        t.timestamp_errors,
        t.synthesized_silence_frames,
        t.device_changes.len()
    )
}

/// The line printed for a Bluetooth hands-free mic (at the start or on a switch during
/// the recording): the same text as `earshot record`'s session warning.
#[cfg(windows)]
pub fn describe_mic_warning(w: &earshot_capture_wasapi::HandsFreeMic) -> String {
    crate::session::format_warning(&earshot_runtime::backends::hands_free_warning(w.clone()))
}

/// Validates the requested duration.
pub fn check_seconds(seconds: f64) -> Result<()> {
    anyhow::ensure!(
        seconds.is_finite() && seconds > 0.0,
        "--seconds must be a positive number, got {seconds}"
    );
    Ok(())
}

/// Records for `seconds` and writes the outputs to `dir`.
#[cfg(windows)]
pub fn run(
    seconds: f64,
    mic_role: MicRole,
    loopback: &LoopbackRequest,
    dir: &Path,
) -> Result<Outputs> {
    check_seconds(seconds)?;
    let rec = earshot_capture_wasapi::record_raw(
        seconds,
        mic_role,
        loopback,
        |mic, lb, src| {
            eprintln!("{}", describe_device("mic", mic));
            eprintln!("{}", describe_device("loopback", lb));
            eprintln!("{}", describe_loopback_source(src));
            eprintln!("record-raw: recording {seconds} s ...");
        },
        |w| eprintln!("{}", describe_mic_warning(w)),
    )
    .context("raw capture failed")?;
    for refused in &rec.refused_call_outputs {
        eprintln!("warning: the call plays on an output that could not be opened: {refused}");
    }
    for (label, track) in [("mic", &rec.mic), ("loopback", &rec.loopback)] {
        for c in &track.device_changes {
            let to = c.device_name.as_deref().unwrap_or("no device");
            eprintln!("{label}: re-opened ({:?}) -> {to}", c.reason);
        }
        eprintln!("{}", describe_track_stats(label, &track.report()));
    }
    write_outputs(dir, &rec)
}

/// Raw capture needs WASAPI.
#[cfg(not(windows))]
pub fn run(
    seconds: f64,
    mic_role: MicRole,
    loopback: &LoopbackRequest,
    dir: &Path,
) -> Result<Outputs> {
    check_seconds(seconds)?;
    let _ = (mic_role, loopback, dir);
    anyhow::bail!("record-raw is only supported on Windows (WASAPI)")
}

#[cfg(test)]
mod tests {
    use super::*;
    use earshot_capture_wasapi::{MixFormat, PacketInfo, TrackTimeline};

    fn fmt(rate: u32) -> MixFormat {
        MixFormat {
            sample_rate: rate,
            channels: 2,
            block_align: 8,
            bits_per_sample: 32,
            valid_bits_per_sample: 32,
            sample_format: SampleFormat::F32,
        }
    }

    /// 0.5 s: mic at 48 kHz with a ramp in 10 ms packets, stereo loopback at 44.1 kHz
    /// with no packets.
    fn canned() -> RawRecording {
        let t0 = 1_000_000;
        let end = t0 + 5_000_000;
        let mut mic = TrackTimeline::new(48_000, t0);
        for i in 0..50u64 {
            let s: Vec<f32> = (0..480).map(|k| (k as f32) / 480.0 - 0.5).collect();
            let info = PacketInfo {
                qpc: t0 + i * 100_000,
                device_position: i * 480,
                frames: 480,
                ..Default::default()
            };
            mic.push(info, &s);
        }
        mic.finish(end);
        let mut lb = TrackTimeline::new(44_100, t0).with_channels(2);
        lb.finish(end);
        RawRecording {
            mic_role: MicRole::Console,
            requested_seconds: 0.5,
            refused_call_outputs: Vec::new(),
            start_qpc: t0,
            end_qpc: end,
            loopback_source: LoopbackSource {
                requested: "zoom".into(),
                mode: LoopbackMode::Process,
                pid: Some(4321),
                process_name: Some("Zoom.exe".into()),
                app: Some("Zoom".into()),
                fallback_reason: None,
            },
            mic: TrackRecording {
                device_name: "Mic".into(),
                format: fmt(48_000),
                timeline: mic,
                device_changes: Vec::new(),
            },
            loopback: TrackRecording {
                device_name: "Speakers".into(),
                format: fmt(44_100),
                timeline: lb,
                device_changes: Vec::new(),
            },
        }
    }

    /// Unique scratch directory under the system temp dir, removed on drop.
    /// (std only: a `tempfile` dev-dependency widens `windows-sys` features under
    /// workspace unification, which relinks `xtask.exe` while `cargo xtask ci` runs.)
    struct ScratchDir(PathBuf);

    impl ScratchDir {
        fn new() -> Self {
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let p = std::env::temp_dir()
                .join(format!("earshot-cli-test-{}-{nanos}", std::process::id()));
            fs::create_dir_all(&p).unwrap();
            Self(p)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for ScratchDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn read_wav(path: &Path) -> (hound::WavSpec, Vec<f32>) {
        let mut r = hound::WavReader::open(path).unwrap();
        let spec = r.spec();
        let s = r.samples::<f32>().map(Result::unwrap).collect();
        (spec, s)
    }

    #[test]
    fn writes_float_wavs_at_native_rate_and_timestamps() {
        let dir = ScratchDir::new();
        let out_dir = dir.path().join("nested").join("take1");
        let rec = canned();
        let out = write_outputs(&out_dir, &rec).unwrap();
        assert_eq!(out.mic, out_dir.join(MIC_WAV));
        assert_eq!(out.loopback, out_dir.join(LOOPBACK_WAV));
        assert_eq!(out.timestamps, out_dir.join(TIMESTAMPS_JSON));

        let (spec, mic) = read_wav(&out.mic);
        assert_eq!(spec.channels, 1);
        assert_eq!(spec.sample_rate, 48_000);
        assert_eq!(spec.bits_per_sample, 32);
        assert_eq!(spec.sample_format, hound::SampleFormat::Float);
        assert_eq!(mic, rec.mic.samples());

        let (spec, lb) = read_wav(&out.loopback);
        assert_eq!(spec.sample_rate, 44_100);
        assert_eq!(spec.channels, 2, "the loopback stays stereo");
        assert_eq!(lb.len(), 2 * 22_050);
        assert!(lb.iter().all(|&s| s == 0.0));

        let json: serde_json::Value =
            serde_json::from_slice(&fs::read(&out.timestamps).unwrap()).unwrap();
        assert_eq!(json["schema_version"], 4);
        assert_eq!(json["mic"]["wav_channels"], 1);
        assert_eq!(json["loopback"]["wav_channels"], 2);
        assert_eq!(json["loopback"]["frames_written"], 22_050);
        assert_eq!(json["loopback_source"]["mode"], "process");
        assert_eq!(json["loopback_source"]["pid"], 4321);
        assert_eq!(json["loopback_source"]["requested"], "zoom");
        assert_eq!(json["mic"]["packet_count"], 50);
        assert_eq!(json["mic"]["frames_written"], 24_000);
        assert_eq!(json["loopback"]["synthesized_silence_frames"], 22_050);
        assert_eq!(json["loopback"]["device_name"], "Speakers");
    }

    /// [`canned`] with 10 ms of distinct left/right audio on the loopback.
    fn canned_with_stereo_audio() -> RawRecording {
        let mut rec = canned();
        let t0 = rec.start_qpc;
        let mut lb = TrackTimeline::new(44_100, t0).with_channels(2);
        let lr: Vec<f32> = (0..441)
            .flat_map(|k| [k as f32 / 1_000.0, -(k as f32) / 1_000.0])
            .collect();
        let info = PacketInfo {
            qpc: t0,
            frames: 441,
            ..Default::default()
        };
        lb.push(info, &lr);
        lb.finish(rec.end_qpc);
        rec.loopback.timeline = lb;
        rec
    }

    #[test]
    fn writes_the_stereo_loopback_interleaved() {
        let dir = ScratchDir::new();
        let rec = canned_with_stereo_audio();
        let out = write_outputs(dir.path(), &rec).unwrap();
        let (spec, lb) = read_wav(&out.loopback);
        assert_eq!(spec.channels, 2);
        assert_eq!(lb, rec.loopback.samples());
        assert_eq!(lb[2..4], [0.001, -0.001], "frame 1: left then right");
    }

    #[test]
    fn written_recording_replays_through_the_core_reader() {
        // Writer (this crate, capture-wasapi) and reader (earshot-core) agree on v4.
        use earshot_core::capture::{
            AudioSource, CaptureEvent, RawTimeline, ReplayBuilder, SourceStatus, TrackBuffer,
            TrackId,
        };
        let dir = ScratchDir::new();
        let rec = canned_with_stereo_audio();
        let out = write_outputs(dir.path(), &rec).unwrap();
        let timeline: RawTimeline =
            serde_json::from_slice(&fs::read(&out.timestamps).unwrap()).unwrap();
        let wav = |p: &Path| {
            let f = std::io::BufReader::new(fs::File::open(p).unwrap());
            TrackBuffer::try_from(earshot_core::audio::read_wav(f).unwrap()).unwrap()
        };
        let mut src =
            ReplayBuilder::from_raw_timeline(&timeline, wav(&out.mic), wav(&out.loopback))
                .unwrap()
                .build()
                .unwrap();
        let reference = src.session().reference.clone().unwrap();
        assert_eq!(reference.channels.get(), 2);
        let mut events = Vec::new();
        while src.pump(&mut events).unwrap() != SourceStatus::Ended {}
        let lb: Vec<f32> = events
            .iter()
            .filter_map(|e| match e {
                CaptureEvent::Frames {
                    track: TrackId::Reference,
                    frames,
                    ..
                } => Some(frames.clone()),
                _ => None,
            })
            .flatten()
            .collect();
        assert_eq!(lb, rec.loopback.samples()[..2 * 441]);
    }

    #[test]
    fn written_hands_free_switch_reads_back_through_the_core_reader() {
        // v4: the writer logs `hands_free` only on the switch onto a hands-free mic, and
        // the core reader returns it (replay itself never warns).
        use earshot_capture_wasapi::{DeviceChange, HandsFreeChange, ReopenReason};
        use earshot_core::capture::{RawHandsFree, RawTimeline};
        let dir = ScratchDir::new();
        let mut rec = canned();
        let change = |track_frame, name: &str, hands_free| DeviceChange {
            qpc: rec.start_qpc + track_frame * 10_000_000 / 48_000,
            track_frame,
            reason: ReopenReason::DefaultDeviceChanged,
            device_name: Some(name.into()),
            device_format: Some(fmt(48_000)),
            error: None,
            hands_free,
        };
        let hf = Some(HandsFreeChange {
            sample_rate: Some(16_000),
        });
        rec.mic.device_changes = vec![
            change(4_800, "Headset (Hands-Free)", hf),
            change(9_600, "Mic", None),
        ];
        let out = write_outputs(dir.path(), &rec).unwrap();
        let bytes = fs::read(&out.timestamps).unwrap();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json["schema_version"], 4);
        let changes = &json["mic"]["device_changes"];
        assert_eq!(changes[0]["hands_free"]["sample_rate"], 16_000);
        assert!(changes[1].get("hands_free").is_none());
        let timeline: RawTimeline = serde_json::from_slice(&bytes).unwrap();
        let read: Vec<_> = timeline
            .mic
            .device_changes
            .iter()
            .map(|c| c.hands_free)
            .collect();
        assert_eq!(
            read,
            vec![
                Some(RawHandsFree {
                    sample_rate: Some(16_000)
                }),
                None
            ]
        );
    }

    #[test]
    fn describes_device_name_and_format() {
        let d = DeviceDescription {
            name: "Microphone (Acme Webcam)".into(),
            format: fmt(48_000),
        };
        assert_eq!(
            describe_device("mic", &d),
            "mic: Microphone (Acme Webcam) [48000 Hz, 2 ch, f32]"
        );
        let d = DeviceDescription {
            name: "Speakers".into(),
            format: MixFormat {
                sample_rate: 44_100,
                channels: 6,
                block_align: 24,
                bits_per_sample: 32,
                valid_bits_per_sample: 24,
                sample_format: SampleFormat::I32,
            },
        };
        assert_eq!(
            describe_device("loopback", &d),
            "loopback: Speakers [44100 Hz, 6 ch, i32 (24 valid bits)]"
        );
    }

    fn source(mode: LoopbackMode) -> LoopbackSource {
        LoopbackSource {
            requested: "auto".into(),
            mode,
            pid: None,
            process_name: None,
            app: None,
            fallback_reason: None,
        }
    }

    #[test]
    fn describes_loopback_source() {
        let mut p = source(LoopbackMode::Process);
        p.pid = Some(1234);
        p.process_name = Some("ms-teams.exe".into());
        p.app = Some("Microsoft Teams".into());
        assert_eq!(
            describe_loopback_source(&p),
            "loopback source: process loopback of Microsoft Teams (ms-teams.exe, pid 1234) [requested: auto]"
        );
        p.app = None;
        assert_eq!(
            describe_loopback_source(&p),
            "loopback source: process loopback of ms-teams.exe (pid 1234) [requested: auto]"
        );
        p.process_name = None;
        p.requested = "1234".into();
        assert_eq!(
            describe_loopback_source(&p),
            "loopback source: process loopback of pid 1234 [requested: 1234]"
        );

        let mut d = source(LoopbackMode::DefaultDevice);
        d.fallback_reason = Some("no call app with an active audio session".into());
        assert_eq!(
            describe_loopback_source(&d),
            "loopback source: default render device (no call app with an active audio session) [requested: auto]"
        );
        d.fallback_reason = None;
        d.requested = "default-device".into();
        assert_eq!(
            describe_loopback_source(&d),
            "loopback source: default render device (requested) [requested: default-device]"
        );
    }

    #[test]
    fn summarises_track_stats() {
        let rec = canned();
        assert_eq!(
            describe_track_stats("mic", &rec.mic.report()),
            "mic: 50 packets, 0 timestamp errors, 0 synthesized silence frames (0.000 s), 0 device changes"
        );
        assert_eq!(
            describe_track_stats("loopback", &rec.loopback.report()),
            "loopback: 0 packets, 0 timestamp errors, 22050 synthesized silence frames (0.500 s), 0 device changes"
        );
    }

    #[cfg(windows)]
    #[test]
    fn describes_a_hands_free_mic_like_the_live_session_warning() {
        let w = earshot_capture_wasapi::HandsFreeMic {
            device_name: "Headset (Hands-Free)".into(),
            sample_rate: Some(16_000),
        };
        let line = describe_mic_warning(&w);
        assert!(
            line.starts_with("warning: the mic \"Headset (Hands-Free)\""),
            "{line}"
        );
        assert!(line.contains("(16 kHz)"), "{line}");
        assert!(line.contains("computer's own mic"), "{line}");
    }

    #[test]
    fn rejects_non_positive_seconds() {
        assert!(check_seconds(0.0).is_err());
        assert!(check_seconds(-1.0).is_err());
        assert!(check_seconds(f64::NAN).is_err());
        assert!(check_seconds(5.0).is_ok());
    }
}
