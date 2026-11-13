//! Unit tests for the capture abstractions and the replay source. Every test path
//! contains `capture`, so `cargo test -p earshot-core capture` selects them all.

use std::convert::Infallible;
use std::num::NonZeroU16;
use std::time::Duration;

use super::*;
use crate::audio::{MonoBuffer, SampleRate};

const T0: u64 = 1_000_000_000;
const SR48: SampleRate = SampleRate::HZ_48000;
const SR16: SampleRate = SampleRate::HZ_16000;

fn ramp(n: usize, offset: f32) -> Vec<f32> {
    (0..n).map(|k| offset + k as f32).collect()
}

fn origin() -> DeviceTime {
    DeviceTime::from_ticks(T0)
}

fn run(source: &mut ReplaySource) -> Vec<CaptureEvent> {
    let mut events = Vec::new();
    loop {
        match source.pump(&mut events).unwrap() {
            SourceStatus::Ended => return events,
            SourceStatus::Delivered => {}
            SourceStatus::Idle => panic!("replay is never idle"),
        }
    }
}

/// Frames delivered on `track`, concatenated, with their timestamps.
fn frames_of(events: &[CaptureEvent], track: TrackId) -> Vec<(DeviceTime, Vec<f32>)> {
    events
        .iter()
        .filter_map(|e| match e {
            CaptureEvent::Frames {
                track: t,
                device_ts,
                frames,
            } if *t == track => Some((*device_ts, frames.clone())),
            _ => None,
        })
        .collect()
}

fn concat(packets: &[(DeviceTime, Vec<f32>)]) -> Vec<f32> {
    packets
        .iter()
        .flat_map(|(_, f)| f.iter().copied())
        .collect()
}

fn markers_of(events: &[CaptureEvent], track: TrackId) -> Vec<Discontinuity> {
    events
        .iter()
        .filter_map(|e| match e {
            CaptureEvent::Discontinuity {
                track: t,
                discontinuity,
            } if *t == track => Some(discontinuity.clone()),
            _ => None,
        })
        .collect()
}

fn event_ts(e: &CaptureEvent) -> DeviceTime {
    match e {
        CaptureEvent::Frames { device_ts, .. } => *device_ts,
        CaptureEvent::Discontinuity { discontinuity, .. } => discontinuity.at,
        CaptureEvent::End(end) => end.at,
    }
}

/// The sink contract every source must keep: exactly one `End`, and it is last.
fn assert_ends_once_last(events: &[CaptureEvent]) -> EndOfStream {
    let ends: Vec<_> = events
        .iter()
        .enumerate()
        .filter(|(_, e)| matches!(e, CaptureEvent::End(_)))
        .collect();
    assert_eq!(ends.len(), 1, "exactly one End event");
    assert_eq!(ends[0].0, events.len() - 1, "End is the last event");
    match events.last() {
        Some(CaptureEvent::End(end)) => end.clone(),
        _ => unreachable!(),
    }
}

// ---- time ----

#[test]
fn capture_device_time_units_are_100_ns() {
    assert_eq!(DeviceTime::TICKS_PER_SECOND, 10_000_000);
    assert_eq!(DeviceTime::from_nanos(1_000), DeviceTime::from_ticks(10));
    assert_eq!(DeviceTime::from_nanos(149), DeviceTime::from_ticks(1));
    assert_eq!(DeviceTime::from_nanos(150), DeviceTime::from_ticks(2));
    assert_eq!(DeviceTime::from_ticks(12_345).ticks(), 12_345);
    assert_eq!(
        DeviceTime::from_ticks(3) + Duration::from_micros(1),
        DeviceTime::from_ticks(13)
    );
}

#[test]
fn capture_device_time_frame_math_rounds_to_nearest_tick() {
    assert_eq!(origin().after_frames(480, SR48).ticks(), T0 + 100_000);
    let r441 = SampleRate::new(44_100).unwrap();
    assert_eq!(origin().after_frames(441, r441).ticks(), T0 + 100_000);
    // 1 frame at 48 kHz is 208.33 ticks.
    assert_eq!(origin().after_frames(1, SR48).ticks(), T0 + 208);
    assert_eq!(DeviceTime::frames_in_ticks(100_000, SR48), 480);
    assert_eq!(DeviceTime::frames_in_ticks(104, SR48), 0);
    assert_eq!(DeviceTime::frames_in_ticks(105, SR48), 1);
}

#[test]
fn capture_session_maps_device_time_to_meeting_time() {
    let s = CaptureSession::new(origin(), TrackInfo::new(SR48), Some(TrackInfo::new(SR16)));
    assert_eq!(s.meeting_time(origin()), Some(Duration::ZERO));
    assert_eq!(
        s.meeting_time(DeviceTime::from_ticks(T0 + 15_000_000)),
        Some(Duration::from_millis(1_500))
    );
    assert_eq!(s.meeting_time(DeviceTime::from_ticks(T0 - 1)), None);
    assert_eq!(
        s.device_time(Duration::from_millis(1_500)),
        DeviceTime::from_ticks(T0 + 15_000_000)
    );
    assert_eq!(
        s.meeting_frame(DeviceTime::from_ticks(T0 + 100_000), SR48),
        Some(480)
    );
    assert_eq!(s.meeting_frame(DeviceTime::from_ticks(T0 - 1), SR48), None);
    assert_eq!(s.track(TrackId::Mic).unwrap().rate, SR48);
    assert_eq!(s.track(TrackId::Reference).unwrap().rate, SR16);
    let mic_only = CaptureSession::new(origin(), TrackInfo::new(SR48), None);
    assert!(mic_only.track(TrackId::Reference).is_none());
    assert_eq!(mic_only.tracks().collect::<Vec<_>>(), vec![TrackId::Mic]);
    assert_eq!(
        s.tracks().collect::<Vec<_>>(),
        vec![TrackId::Mic, TrackId::Reference]
    );
}

// ---- buffer replay ----

fn two_track(mic_frames: usize, ref_frames: usize) -> ReplayBuilder {
    ReplayBuilder::new(origin(), MonoBuffer::new(SR48, ramp(mic_frames, 0.0)))
        .reference(MonoBuffer::new(SR16, ramp(ref_frames, 100_000.0)))
}

#[test]
fn capture_replay_delivers_both_tracks_exactly_with_timestamps() {
    let mut src = two_track(4_800 + 100, 1_600 + 50).build().unwrap();
    assert_eq!(src.session().origin, origin());
    assert_eq!(src.session().mic.rate, SR48);
    assert_eq!(src.session().reference.as_ref().unwrap().rate, SR16);
    let events = run(&mut src);

    let mic = frames_of(&events, TrackId::Mic);
    let rf = frames_of(&events, TrackId::Reference);
    assert_eq!(concat(&mic), ramp(4_900, 0.0));
    assert_eq!(concat(&rf), ramp(1_650, 100_000.0));
    // Default 10 ms packets at each track's own rate, last one partial.
    assert!(mic[..10].iter().all(|(_, f)| f.len() == 480));
    assert_eq!(mic[10].1.len(), 100);
    assert!(rf[..10].iter().all(|(_, f)| f.len() == 160));
    assert_eq!(rf[10].1.len(), 50);
    // Timestamps: origin + frames already delivered on that track.
    let mut pos = 0;
    for (ts, f) in &mic {
        assert_eq!(*ts, origin().after_frames(pos, SR48));
        pos += f.len() as u64;
    }
    // Global order is by timestamp; mic before reference on ties.
    for w in events.windows(2) {
        assert!(event_ts(&w[0]) <= event_ts(&w[1]), "events out of order");
    }
    assert!(matches!(
        &events[0],
        CaptureEvent::Frames {
            track: TrackId::Mic,
            ..
        }
    ));
    let end = assert_ends_once_last(&events);
    assert_eq!(end.reason, EndReason::Finished);
    // End is at the later of the two track ends (reference: 1650 frames at 16 kHz).
    assert!(origin().after_frames(1_650, SR16) > origin().after_frames(4_900, SR48));
    assert_eq!(end.at, origin().after_frames(1_650, SR16));
    assert!(markers_of(&events, TrackId::Mic).is_empty());
}

#[test]
fn capture_replay_packet_duration_is_configurable() {
    let mut src = two_track(4_800, 1_600)
        .packet_duration(Duration::from_millis(20))
        .build()
        .unwrap();
    let events = run(&mut src);
    assert!(
        frames_of(&events, TrackId::Mic)
            .iter()
            .all(|(_, f)| f.len() == 960)
    );
    assert!(
        frames_of(&events, TrackId::Reference)
            .iter()
            .all(|(_, f)| f.len() == 320)
    );
}

#[test]
fn capture_replay_gap_omits_frames_and_resumes_at_true_timestamp() {
    // 250 ms of reference missing from 100 ms on (like idle loopback): no marker,
    // the resumed packet is stamped where it belongs.
    let mut src = two_track(48_000, 16_000)
        .gap(TrackId::Reference, 1_600, 4_000)
        .unwrap()
        .build()
        .unwrap();
    let events = run(&mut src);
    let rf = frames_of(&events, TrackId::Reference);
    let delivered = concat(&rf);
    assert_eq!(delivered.len(), 16_000 - 4_000);
    let mut expected = ramp(1_600, 100_000.0);
    expected.extend(ramp(16_000 - 5_600, 100_000.0 + 5_600.0));
    assert_eq!(delivered, expected);
    let resume = rf
        .iter()
        .find(|(_, f)| f[0] == 100_000.0 + 5_600.0)
        .expect("resumed packet");
    assert_eq!(resume.0, origin().after_frames(5_600, SR16));
    assert!(markers_of(&events, TrackId::Reference).is_empty());
    // The mic is untouched.
    assert_eq!(concat(&frames_of(&events, TrackId::Mic)), ramp(48_000, 0.0));
    assert_ends_once_last(&events);
}

#[test]
fn capture_replay_gap_at_start_delays_first_packet() {
    let mut src = two_track(4_800, 1_600)
        .gap(TrackId::Reference, 0, 800)
        .unwrap()
        .build()
        .unwrap();
    let events = run(&mut src);
    let rf = frames_of(&events, TrackId::Reference);
    assert_eq!(rf[0].0, origin().after_frames(800, SR16));
    assert_eq!(rf[0].1[0], 100_800.0);
}

#[test]
fn capture_replay_injects_discontinuity_mid_packet() {
    let kind = DiscontinuityKind::DataLost;
    let mut src = two_track(4_800, 1_600)
        .discontinuity(TrackId::Mic, 1_000, kind.clone())
        .unwrap()
        .build()
        .unwrap();
    let events = run(&mut src);
    let markers = markers_of(&events, TrackId::Mic);
    assert_eq!(markers.len(), 1);
    assert_eq!(markers[0].kind, kind);
    assert_eq!(markers[0].at, origin().after_frames(1_000, SR48));
    // The marker sits between frame 999 and frame 1000 of the mic stream.
    let mut before = Vec::new();
    let mut seen_marker = false;
    for e in &events {
        match e {
            CaptureEvent::Discontinuity {
                track: TrackId::Mic,
                ..
            } => seen_marker = true,
            CaptureEvent::Frames {
                track: TrackId::Mic,
                frames,
                device_ts,
            } => {
                if seen_marker {
                    assert_eq!(frames[0], 1_000.0, "first frame after the marker");
                    assert_eq!(*device_ts, markers[0].at);
                    break;
                }
                before.extend_from_slice(frames);
            }
            _ => {}
        }
    }
    assert_eq!(before, ramp(1_000, 0.0));
    assert_eq!(concat(&frames_of(&events, TrackId::Mic)), ramp(4_800, 0.0));
    assert_ends_once_last(&events);
}

#[test]
fn capture_replay_marked_gap_and_device_change() {
    let change = DiscontinuityKind::DeviceChanged {
        device_name: Some("Headset".into()),
        rate: SR48,
        warning: None,
    };
    let mut src = two_track(48_000, 16_000)
        .gap(TrackId::Mic, 9_600, 2_400)
        .unwrap()
        .discontinuity(TrackId::Mic, 9_600, change.clone())
        .unwrap()
        .build()
        .unwrap();
    let events = run(&mut src);
    let markers = markers_of(&events, TrackId::Mic);
    assert_eq!(markers.len(), 1);
    assert_eq!(markers[0].kind, change);
    // The marker is stamped where audio resumes, after the gap.
    assert_eq!(markers[0].at, origin().after_frames(12_000, SR48));
    let mic = frames_of(&events, TrackId::Mic);
    let after = mic.iter().find(|(ts, _)| *ts >= markers[0].at).unwrap();
    assert_eq!(after.1[0], 12_000.0);
}

#[test]
fn capture_replay_stop_ends_early_and_stays_ended() {
    let mut src = two_track(48_000, 16_000).build().unwrap();
    let mut events = Vec::new();
    for _ in 0..5 {
        assert_eq!(src.pump(&mut events).unwrap(), SourceStatus::Delivered);
    }
    src.stop();
    assert_eq!(src.pump(&mut events).unwrap(), SourceStatus::Ended);
    let end = assert_ends_once_last(&events);
    assert_eq!(end.reason, EndReason::Stopped);
    let last_frames_ts = events
        .iter()
        .rev()
        .find_map(|e| match e {
            CaptureEvent::Frames { device_ts, .. } => Some(*device_ts),
            _ => None,
        })
        .unwrap();
    assert!(
        end.at > last_frames_ts,
        "End follows the last delivered frames"
    );
    let n = events.len();
    for _ in 0..3 {
        assert_eq!(src.pump(&mut events).unwrap(), SourceStatus::Ended);
    }
    assert_eq!(events.len(), n, "nothing after End");
}

#[test]
fn capture_replay_mic_only_session() {
    let mut src = ReplayBuilder::new(origin(), MonoBuffer::new(SR16, ramp(1_000, 0.0)))
        .build()
        .unwrap();
    assert!(src.session().reference.is_none());
    let events = run(&mut src);
    assert!(frames_of(&events, TrackId::Reference).is_empty());
    assert_eq!(concat(&frames_of(&events, TrackId::Mic)), ramp(1_000, 0.0));
    assert_eq!(
        assert_ends_once_last(&events).at,
        origin().after_frames(1_000, SR16)
    );
}

#[test]
fn capture_replay_empty_tracks_end_at_origin() {
    let mut src = ReplayBuilder::new(origin(), MonoBuffer::new(SR16, Vec::new()))
        .build()
        .unwrap();
    let events = run(&mut src);
    assert_eq!(events.len(), 1);
    assert_eq!(assert_ends_once_last(&events).at, origin());
}

#[test]
fn capture_replay_builder_rejects_bad_edits() {
    assert_eq!(
        two_track(100, 100).gap(TrackId::Mic, 50, 51).unwrap_err(),
        ReplayError::EditOutOfRange {
            track: TrackId::Mic,
            frame: 101,
            len: 100
        }
    );
    assert!(matches!(
        two_track(100, 100).discontinuity(TrackId::Reference, 101, DiscontinuityKind::DataLost),
        Err(ReplayError::EditOutOfRange { .. })
    ));
    // A marker at the very end of a track is allowed.
    assert!(
        two_track(100, 100)
            .discontinuity(TrackId::Mic, 100, DiscontinuityKind::DataLost)
            .is_ok()
    );
    let mic_only = ReplayBuilder::new(origin(), MonoBuffer::new(SR16, ramp(10, 0.0)));
    assert_eq!(
        mic_only.gap(TrackId::Reference, 0, 1).unwrap_err(),
        ReplayError::NoReferenceTrack
    );
    assert_eq!(
        two_track(10, 10)
            .packet_duration(Duration::from_micros(10))
            .build()
            .unwrap_err(),
        ReplayError::PacketTooShort
    );
}

#[test]
fn capture_audio_source_is_object_safe() {
    let mut replay = two_track(480, 160).build().unwrap();
    let source: &mut dyn AudioSource<Error = Infallible> = &mut replay;
    let mut events: Vec<CaptureEvent> = Vec::new();
    let sink: &mut dyn CaptureSink = &mut events;
    while source.pump(sink).unwrap() != SourceStatus::Ended {}
    assert_eq!(events.len(), 3);
}

// ---- raw-recording timeline replay ----

/// A schema-v1 `timestamps.json` covering the cases the WASAPI timeline produces:
/// leading pad, a trimmed first packet on the loopback, a mid-stream gap, a
/// data-discontinuity flag, a silent packet and a timestamp-error packet.
fn raw_json() -> String {
    // 48 kHz, 10 ms packets = 480 frames = 100_000 ticks.
    format!(
        r#"{{
  "schema_version": 1,
  "qpc_units_per_second": 10000000,
  "mic_role": "console",
  "requested_seconds": 0.1,
  "start_qpc": {t0},
  "end_qpc": {end},
  "mic": {{
    "device_name": "Mic (Test)",
    "format": {{"sample_rate": 48000, "channels": 2, "block_align": 8,
                "bits_per_sample": 32, "valid_bits_per_sample": 32, "sample_format": "f32"}},
    "wav_sample_rate": 48000, "wav_channels": 1, "wav_start_qpc": {t0},
    "frames_written": 2000, "synthesized_silence_frames": 80,
    "silence_gaps": 1, "discontinuities": 2, "timestamp_errors": 1, "packet_count": 4,
    "packets": [
      {{"qpc": {p0}, "device_position": 0, "frames": 480, "track_frame": 80,
        "silence_before": 80, "trimmed_frames": 0, "silent": false,
        "discontinuity": true, "timestamp_error": false}},
      {{"qpc": {p1}, "device_position": 480, "frames": 480, "track_frame": 560,
        "silence_before": 0, "trimmed_frames": 0, "silent": true,
        "discontinuity": false, "timestamp_error": false}},
      {{"qpc": 1, "device_position": 960, "frames": 480, "track_frame": 1040,
        "silence_before": 0, "trimmed_frames": 0, "silent": false,
        "discontinuity": false, "timestamp_error": true}},
      {{"qpc": {p3}, "device_position": 1440, "frames": 480, "track_frame": 1520,
        "silence_before": 0, "trimmed_frames": 0, "silent": false,
        "discontinuity": true, "timestamp_error": false}}
    ]
  }},
  "loopback": {{
    "device_name": "Speakers (Test)",
    "format": {{"sample_rate": 48000, "channels": 2, "block_align": 8,
                "bits_per_sample": 32, "valid_bits_per_sample": 32, "sample_format": "f32"}},
    "wav_sample_rate": 48000, "wav_channels": 1, "wav_start_qpc": {t0},
    "frames_written": 2000, "synthesized_silence_frames": 1280,
    "silence_gaps": 2, "discontinuities": 0, "timestamp_errors": 0, "packet_count": 2,
    "packets": [
      {{"qpc": {l0}, "device_position": 0, "frames": 480, "track_frame": 0,
        "silence_before": 0, "trimmed_frames": 240, "silent": false,
        "discontinuity": false, "timestamp_error": false}},
      {{"qpc": {l1}, "device_position": 480, "frames": 480, "track_frame": 1000,
        "silence_before": 760, "trimmed_frames": 0, "silent": false,
        "discontinuity": false, "timestamp_error": false}}
    ]
  }}
}}"#,
        t0 = T0,
        end = T0 + 2_000 * 10_000_000 / 48_000 + 1,
        p0 = T0 + 16_667,
        p1 = T0 + 116_667 + 3,
        p3 = T0 + 316_667 + 7,
        l0 = T0 - 50_000,
        l1 = T0 + 208_333,
    )
}

fn raw_timeline() -> RawTimeline {
    serde_json::from_str(&raw_json()).unwrap()
}

#[test]
fn capture_raw_timeline_parses_schema_v1() {
    let t = raw_timeline();
    assert_eq!(t.schema_version, 1);
    assert_eq!(t.start_qpc, T0);
    assert_eq!(t.mic.wav_sample_rate, 48_000);
    assert_eq!(t.mic.packets.len(), 4);
    assert!(t.mic.packets[0].discontinuity);
    assert_eq!(t.loopback.packets[0].trimmed_frames, 240);
    assert!(t.mic.device_changes.is_empty());
}

#[test]
fn capture_replay_from_raw_timeline_reproduces_packets() {
    let t = raw_timeline();
    let mic = MonoBuffer::new(SR48, ramp(2_000, 0.0));
    let lb = MonoBuffer::new(SR48, ramp(2_000, 10_000.0));
    let mut src = ReplayBuilder::from_raw_timeline(&t, mic, lb)
        .unwrap()
        .build()
        .unwrap();
    assert_eq!(src.session().origin, origin());
    assert_eq!(src.session().mic.device_name.as_deref(), Some("Mic (Test)"));
    assert_eq!(
        src.session()
            .reference
            .as_ref()
            .unwrap()
            .device_name
            .as_deref(),
        Some("Speakers (Test)")
    );
    let events = run(&mut src);

    let m = frames_of(&events, TrackId::Mic);
    assert_eq!(m.len(), 4);
    // Packets are replayed at their recorded QPC, with the WAV samples at track_frame.
    assert_eq!(m[0].0, DeviceTime::from_ticks(T0 + 16_667));
    assert_eq!(m[0].1, ramp(480, 80.0));
    assert_eq!(m[1].0, DeviceTime::from_ticks(T0 + 116_670));
    assert_eq!(
        m[1].1,
        ramp(480, 560.0),
        "silent packets replay the WAV zeros as-is"
    );
    // A timestamp-error packet has no usable qpc: it is stamped at the end of the
    // previous packet on its track, so timestamps never go backwards.
    assert_eq!(m[2].0, m[1].0.after_frames(480, SR48));
    assert_eq!(m[3].0, DeviceTime::from_ticks(T0 + 316_674));
    let delivered: usize = m.iter().map(|(_, f)| f.len()).sum();
    assert_eq!(
        delivered as u64 + t.mic.synthesized_silence_frames,
        t.mic.frames_written
    );
    // A discontinuity flag becomes a DataLost marker just before its packet, except on
    // the stream's first packet, which WASAPI flags on start (as the live source).
    let markers = markers_of(&events, TrackId::Mic);
    assert_eq!(markers.len(), 1, "{markers:?}");
    assert_eq!(markers[0].kind, DiscontinuityKind::DataLost);
    assert_eq!(markers[0].at, m[3].0);
    let mic_events: Vec<_> = events
        .iter()
        .filter(|e| match e {
            CaptureEvent::Frames { track, .. } | CaptureEvent::Discontinuity { track, .. } => {
                *track == TrackId::Mic
            }
            CaptureEvent::End(_) => false,
        })
        .collect();
    assert!(matches!(mic_events[0], CaptureEvent::Frames { .. }));
    assert!(matches!(mic_events[3], CaptureEvent::Discontinuity { .. }));
    assert!(
        matches!(mic_events[4], CaptureEvent::Frames { device_ts, .. } if *device_ts == m[3].0)
    );

    let l = frames_of(&events, TrackId::Reference);
    assert_eq!(l.len(), 2);
    // The trimmed first packet keeps 240 frames, stamped at the origin.
    assert_eq!(l[0].0, DeviceTime::from_ticks(T0 - 50_000 + 50_000));
    assert_eq!(l[0].1, ramp(240, 10_000.0));
    assert_eq!(l[1].0, DeviceTime::from_ticks(T0 + 208_333));
    assert_eq!(l[1].1, ramp(480, 11_000.0));
    let delivered: usize = l.iter().map(|(_, f)| f.len()).sum();
    assert_eq!(
        delivered as u64 + t.loopback.synthesized_silence_frames,
        t.loopback.frames_written
    );

    let end = assert_ends_once_last(&events);
    assert_eq!(end.at, DeviceTime::from_ticks(t.end_qpc));
    assert_eq!(end.reason, EndReason::Finished);
}

#[test]
fn capture_raw_timeline_v2_device_changes_become_markers() {
    let json = raw_json().replacen(
        "\"packets\": [",
        r#""device_changes": [
      {"qpc": 1000210000, "track_frame": 1040, "reason": "default_device_changed",
       "device_name": "Headset Mic", "device_format": null, "error": null}
    ],
    "packets": ["#,
        1,
    );
    let json = json.replacen("\"schema_version\": 1", "\"schema_version\": 2", 1);
    // The new stream's first packet (track_frame 1040) is flagged on start, as WASAPI
    // does: no DataLost for it.
    let flagged_1040 = json.replacen(
        "\"discontinuity\": false, \"timestamp_error\": true",
        "\"discontinuity\": true, \"timestamp_error\": true",
        1,
    );
    assert_ne!(flagged_1040, json);
    let json = flagged_1040;
    let t: RawTimeline = serde_json::from_str(&json).unwrap();
    assert_eq!(t.mic.device_changes.len(), 1);
    assert!(t.mic.packets[2].discontinuity);
    let mic = MonoBuffer::new(SR48, ramp(2_000, 0.0));
    let lb = MonoBuffer::new(SR48, ramp(2_000, 10_000.0));
    let mut src = ReplayBuilder::from_raw_timeline(&t, mic, lb)
        .unwrap()
        .build()
        .unwrap();
    let events = run(&mut src);
    // The re-open, then the later packet's DataLost (track_frame 1520); neither the
    // track's first packet nor the new stream's first packet makes a DataLost.
    let markers = markers_of(&events, TrackId::Mic);
    assert_eq!(markers.len(), 2, "{markers:?}");
    assert_eq!(markers[1].kind, DiscontinuityKind::DataLost);
    assert_eq!(
        markers[0],
        Discontinuity {
            at: DeviceTime::from_ticks(1_000_210_000),
            kind: DiscontinuityKind::DeviceChanged {
                device_name: Some("Headset Mic".into()),
                rate: SR48,
                warning: None,
            },
        }
    );
    // It precedes the packet at track_frame 1040 and follows the one at 560.
    let pos = |pred: &dyn Fn(&CaptureEvent) -> bool| events.iter().position(pred).unwrap();
    let marker_pos = pos(&|e| {
        matches!(e, CaptureEvent::Discontinuity { discontinuity, .. }
            if matches!(discontinuity.kind, DiscontinuityKind::DeviceChanged { .. }))
    });
    let pkt_1040 = pos(
        &|e| matches!(e, CaptureEvent::Frames { track: TrackId::Mic, frames, .. } if frames[0] == 1_040.0),
    );
    let pkt_560 = pos(
        &|e| matches!(e, CaptureEvent::Frames { track: TrackId::Mic, frames, .. } if frames[0] == 560.0),
    );
    assert!(pkt_560 < marker_pos && marker_pos < pkt_1040);
}

#[test]
fn capture_replay_from_raw_timeline_rejects_mismatched_buffers() {
    let t = raw_timeline();
    let short = MonoBuffer::new(SR48, ramp(1_999, 0.0));
    let ok = MonoBuffer::new(SR48, ramp(2_000, 0.0));
    assert_eq!(
        ReplayBuilder::from_raw_timeline(&t, short, ok.clone()).unwrap_err(),
        ReplayError::LengthMismatch {
            track: TrackId::Mic,
            expected: 2_000,
            actual: 1_999
        }
    );
    let wrong_rate = MonoBuffer::new(SR16, ramp(2_000, 0.0));
    assert_eq!(
        ReplayBuilder::from_raw_timeline(&t, ok.clone(), wrong_rate).unwrap_err(),
        ReplayError::RateMismatch {
            track: TrackId::Reference,
            expected: 48_000,
            actual: 16_000
        }
    );
    let mut bad = raw_timeline();
    bad.mic.packets[3].track_frame = 1_600; // runs past frames_written
    assert_eq!(
        ReplayBuilder::from_raw_timeline(&bad, ok.clone(), ok.clone()).unwrap_err(),
        ReplayError::PacketOutOfRange {
            track: TrackId::Mic,
            packet: 3
        }
    );
    let mut bad = raw_timeline();
    bad.qpc_units_per_second = 0;
    assert_eq!(
        ReplayBuilder::from_raw_timeline(&bad, ok.clone(), ok).unwrap_err(),
        ReplayError::BadTimeline
    );
}

#[test]
fn capture_raw_timeline_converts_other_qpc_units() {
    // Same timeline in microsecond units: 100 ns ticks are recovered exactly.
    let mut t = raw_timeline();
    t.qpc_units_per_second = 1_000_000;
    t.start_qpc = T0 / 10;
    t.end_qpc = T0 / 10 + 4_200;
    t.mic.packets[0].qpc = T0 / 10 + 2_000;
    let ok = MonoBuffer::new(SR48, ramp(2_000, 0.0));
    let mut src = ReplayBuilder::from_raw_timeline(&t, ok.clone(), ok)
        .unwrap()
        .build()
        .unwrap();
    assert_eq!(src.session().origin, origin());
    let events = run(&mut src);
    assert_eq!(
        frames_of(&events, TrackId::Mic)[0].0,
        DeviceTime::from_ticks(T0 + 20_000)
    );
    assert_eq!(
        assert_ends_once_last(&events).at,
        DeviceTime::from_ticks(T0 + 42_000)
    );
}

#[test]
fn capture_replay_edits_apply_to_raw_timelines_too() {
    let t = raw_timeline();
    let ok = MonoBuffer::new(SR48, ramp(2_000, 0.0));
    let mut src = ReplayBuilder::from_raw_timeline(&t, ok.clone(), ok)
        .unwrap()
        .gap(TrackId::Mic, 1_520, 480)
        .unwrap()
        .build()
        .unwrap();
    let events = run(&mut src);
    assert_eq!(frames_of(&events, TrackId::Mic).len(), 3);
}

/// A raw timeline whose mic has `packets` 10 ms packets, each stamped `late` ticks
/// later than the previous one's end (drift below the gap tolerance, so the WASAPI
/// timeline placed them back to back), then one timestamp-error packet.
fn drifted_timeline(packets: u64, late: u64) -> RawTimeline {
    let mut log: Vec<RawPacket> = (0..packets)
        .map(|k| RawPacket {
            qpc: T0 + k * (100_000 + late),
            frames: 480,
            track_frame: k * 480,
            trimmed_frames: 0,
            silent: false,
            discontinuity: false,
            timestamp_error: false,
        })
        .collect();
    log.push(RawPacket {
        qpc: 7,
        frames: 480,
        track_frame: packets * 480,
        trimmed_frames: 0,
        silent: false,
        discontinuity: false,
        timestamp_error: true,
    });
    let track = |packets: Vec<RawPacket>, frames: u64| RawTrackTimeline {
        device_name: "Test".into(),
        wav_sample_rate: 48_000,
        wav_channels: 1,
        frames_written: frames,
        synthesized_silence_frames: 0,
        packets,
        device_changes: Vec::new(),
        processing: RawProcessing::Default,
    };
    RawTimeline {
        schema_version: 1,
        qpc_units_per_second: 10_000_000,
        start_qpc: T0,
        end_qpc: T0 + (packets + 1) * (100_000 + late),
        mic: track(log, (packets + 1) * 480),
        loopback: track(Vec::new(), 0),
    }
}

#[test]
fn capture_raw_timestamp_error_packet_follows_drifted_previous_packet() {
    // 20 packets each 3 ms late: by packet 19 the device clock is 57 ms ahead of the
    // WAV position, so stamping the error packet from its track_frame would go back.
    let t = drifted_timeline(20, 30_000);
    let mic = MonoBuffer::new(SR48, ramp(21 * 480, 0.0));
    let lb = MonoBuffer::new(SR48, Vec::new());
    let mut src = ReplayBuilder::from_raw_timeline(&t, mic, lb)
        .unwrap()
        .build()
        .unwrap();
    let events = run(&mut src);
    let m = frames_of(&events, TrackId::Mic);
    assert_eq!(m.len(), 21);
    assert!(markers_of(&events, TrackId::Mic).is_empty());
    for w in m.windows(2) {
        assert!(w[1].0 >= w[0].0, "mic timestamps must not go backwards");
    }
    // Stamped at the previous packet's end on the same track.
    assert_eq!(m[20].0, m[19].0.after_frames(480, SR48));
    assert!(m[20].0 > origin().after_frames(20 * 480, SR48));
}

#[test]
fn capture_raw_packet_at_or_before_the_previous_start_follows_the_previous_end() {
    // The WASAPI timeline placed these packets back to back; their stamps are unusable
    // (one repeats the previous start, one goes back to the origin). Like the live
    // source, replay stamps them at the previous packet's end instead of going back.
    let mut t = drifted_timeline(4, 0);
    t.mic.packets[2].qpc = t.mic.packets[1].qpc;
    t.mic.packets[3].qpc = T0;
    let mic = MonoBuffer::new(SR48, ramp(5 * 480, 0.0));
    let lb = MonoBuffer::new(SR48, Vec::new());
    let mut src = ReplayBuilder::from_raw_timeline(&t, mic, lb)
        .unwrap()
        .build()
        .unwrap();
    let events = run(&mut src);
    let m = frames_of(&events, TrackId::Mic);
    let ts: Vec<_> = m.iter().map(|f| f.0).collect();
    let expected: Vec<_> = (0..5)
        .map(|k| origin().after_frames(k * 480, SR48))
        .collect();
    assert_eq!(ts, expected);
    assert!(markers_of(&events, TrackId::Mic).is_empty());
}

#[test]
fn capture_raw_timestamp_error_first_packet_uses_track_position() {
    let mut t = drifted_timeline(0, 0);
    t.mic.packets[0].track_frame = 0;
    let mic = MonoBuffer::new(SR48, ramp(480, 0.0));
    let lb = MonoBuffer::new(SR48, Vec::new());
    let mut src = ReplayBuilder::from_raw_timeline(&t, mic, lb)
        .unwrap()
        .build()
        .unwrap();
    let events = run(&mut src);
    assert_eq!(frames_of(&events, TrackId::Mic)[0].0, origin());
}

#[test]
fn capture_raw_timeline_rejects_unknown_schema_versions() {
    let ok = MonoBuffer::new(SR48, ramp(2_000, 0.0));
    for v in [0, 6, 99] {
        let mut t = raw_timeline();
        t.schema_version = v;
        assert_eq!(
            ReplayBuilder::from_raw_timeline(&t, ok.clone(), ok.clone()).unwrap_err(),
            ReplayError::UnsupportedSchema(v)
        );
    }
    assert_eq!(SUPPORTED_SCHEMAS, [1, 2, 3, 4, 5]);
    for v in [2, 3, 4, 5] {
        let mut t = raw_timeline();
        t.schema_version = v;
        assert!(ReplayBuilder::from_raw_timeline(&t, ok.clone(), ok.clone()).is_ok());
    }
}

#[test]
fn capture_raw_timeline_rejects_qpcs_that_overflow() {
    let ok = MonoBuffer::new(SR48, ramp(2_000, 0.0));
    // A packet whose end would pass u64::MAX ticks.
    let mut t = raw_timeline();
    t.mic.packets[3].qpc = u64::MAX - 10;
    assert_eq!(
        ReplayBuilder::from_raw_timeline(&t, ok.clone(), ok.clone()).unwrap_err(),
        ReplayError::BadTimeline
    );
    // An origin so late that the track end overflows.
    let mut t = raw_timeline();
    t.start_qpc = u64::MAX - 1_000;
    assert_eq!(
        ReplayBuilder::from_raw_timeline(&t, ok.clone(), ok.clone()).unwrap_err(),
        ReplayError::BadTimeline
    );
    // A device change stamped beyond the tick range in coarser units.
    let mut t = raw_timeline();
    t.qpc_units_per_second = 1;
    t.start_qpc = 0;
    t.end_qpc = 1;
    for p in t
        .mic
        .packets
        .iter_mut()
        .chain(t.loopback.packets.iter_mut())
    {
        p.qpc = 0;
    }
    t.mic.device_changes.push(RawDeviceChange {
        qpc: u64::MAX,
        track_frame: 0,
        device_name: None,
        hands_free: None,
    });
    assert_eq!(
        ReplayBuilder::from_raw_timeline(&t, ok.clone(), ok).unwrap_err(),
        ReplayError::BadTimeline
    );
}

#[test]
fn capture_device_time_arithmetic_is_checked() {
    let near_max = DeviceTime::from_ticks(u64::MAX - 10);
    assert_eq!(near_max.checked_after_frames(480, SR48), None);
    assert_eq!(
        near_max.checked_after_frames(0, SR48),
        Some(near_max),
        "zero frames is fine"
    );
    assert_eq!(near_max.checked_add(Duration::from_secs(1)), None);
    assert_eq!(
        DeviceTime::from_ticks(5).checked_add(Duration::from_micros(1)),
        Some(DeviceTime::from_ticks(15))
    );
    // The infallible forms saturate rather than wrap or panic.
    assert_eq!(
        near_max.after_frames(480, SR48),
        DeviceTime::from_ticks(u64::MAX)
    );
    assert_eq!(near_max + Duration::MAX, DeviceTime::from_ticks(u64::MAX));
    assert_eq!(
        DeviceTime::from_ticks(0).checked_after_frames(u64::MAX, SampleRate::new(1).unwrap()),
        None
    );
}

// ---- multichannel (stereo reference, T6.3c) ----

const STEREO: NonZeroU16 = NonZeroU16::new(2).unwrap();

/// `frames` interleaved stereo frames: left `offset + k`, right `-(offset + k)`.
fn stereo_ramp(frames: usize, offset: f32) -> Vec<f32> {
    (0..frames)
        .flat_map(|k| [offset + k as f32, -(offset + k as f32)])
        .collect()
}

#[test]
fn capture_track_info_is_mono_unless_given_channels() {
    let mono = TrackInfo::new(SR48);
    assert_eq!(mono.channels, MONO);
    let stereo = TrackInfo::new(SR48)
        .with_channels(STEREO)
        .with_device_name("Speakers");
    assert_eq!(stereo.channels.get(), 2);
    assert_eq!(stereo.rate, SR48);
    assert_eq!(stereo.device_name.as_deref(), Some("Speakers"));
    assert_eq!(stereo.frames_in(960), 480);
    assert_eq!(mono.frames_in(960), 960);
}

#[test]
fn capture_mix_to_mono_is_the_mean_of_the_channels() {
    let mut out = vec![9.0];
    let n = mix_to_mono(&[0.5, -0.5, 1.0, 0.0, -0.25, -0.75], STEREO, &mut out);
    assert_eq!(n, 3);
    assert_eq!(out, [9.0, 0.0, 0.5, -0.5], "appends, does not clear");
    let mut out = Vec::new();
    assert_eq!(mix_to_mono(&[0.25, 0.5], MONO, &mut out), 2);
    assert_eq!(out, [0.25, 0.5], "mono passes through");
    let three = NonZeroU16::new(3).unwrap();
    let mut out = Vec::new();
    assert_eq!(mix_to_mono(&[0.3, 0.6, 0.9], three, &mut out), 1);
    assert!((out[0] - 0.6).abs() < 1e-6);
}

#[test]
fn capture_track_buffer_rejects_partial_frames() {
    assert_eq!(
        TrackBuffer::new(SR48, STEREO, vec![0.0; 3]).unwrap_err(),
        ReplayError::PartialFrame {
            samples: 3,
            channels: 2
        }
    );
    let b = TrackBuffer::new(SR48, STEREO, vec![0.0; 4]).unwrap();
    assert_eq!((b.frames(), b.channels(), b.rate()), (2, STEREO, SR48));
    let m: TrackBuffer = MonoBuffer::new(SR16, vec![0.0; 5]).into();
    assert_eq!((m.frames(), m.channels()), (5, MONO));
    let wav = crate::audio::WavAudio::new(SR48, 2, vec![0.0; 6]).unwrap();
    assert_eq!(TrackBuffer::try_from(wav).unwrap().frames(), 3);
    // A zero-channel WavAudio can no longer be built, so TryFrom cannot see one.
    assert!(crate::audio::WavAudio::new(SR48, 0, Vec::new()).is_err());
}

#[test]
fn capture_replay_delivers_an_interleaved_stereo_reference() {
    let reference = TrackBuffer::new(SR48, STEREO, stereo_ramp(1_000, 0.0)).unwrap();
    let mut src = ReplayBuilder::new(origin(), MonoBuffer::new(SR48, ramp(1_000, 0.0)))
        .reference(reference)
        .gap(TrackId::Reference, 500, 100)
        .unwrap()
        .build()
        .unwrap();
    let info = src.session().reference.clone().unwrap();
    assert_eq!(info.channels, STEREO);
    assert_eq!(src.session().mic.channels, MONO);
    let events = run(&mut src);
    let rf = frames_of(&events, TrackId::Reference);
    // 10 ms packets are 480 frames = 960 interleaved samples, cut by the gap.
    let lens: Vec<_> = rf.iter().map(|(_, f)| f.len()).collect();
    assert_eq!(lens, vec![960, 40, 720, 80]);
    // Timestamps advance by frames, not samples.
    assert_eq!(rf[1].0, origin().after_frames(480, SR48));
    assert_eq!(rf[2].0, origin().after_frames(600, SR48));
    assert_eq!(rf[3].0, origin().after_frames(960, SR48));
    let mut expect = stereo_ramp(500, 0.0);
    expect.extend(stereo_ramp(400, 600.0));
    assert_eq!(concat(&rf), expect);
    assert_eq!(concat(&frames_of(&events, TrackId::Mic)), ramp(1_000, 0.0));
    // The end follows the reference's frame count (1000 frames at 48 kHz).
    assert_eq!(
        assert_ends_once_last(&events).at,
        origin().after_frames(1_000, SR48)
    );
    // A stopped replay ends after the last delivered frames, also counted in frames.
    let mut src = ReplayBuilder::new(origin(), MonoBuffer::new(SR48, Vec::new()))
        .reference(TrackBuffer::new(SR48, STEREO, stereo_ramp(480, 0.0)).unwrap())
        .build()
        .unwrap();
    let mut events = Vec::new();
    src.pump(&mut events).unwrap();
    src.stop();
    src.pump(&mut events).unwrap();
    assert_eq!(
        assert_ends_once_last(&events).at,
        origin().after_frames(480, SR48)
    );
}

/// [`raw_json`] as schema v3 with a stereo `loopback.wav`.
fn raw_json_v3_stereo() -> String {
    let json = raw_json().replacen("\"schema_version\": 1", "\"schema_version\": 3", 1);
    // The loopback section is the second `wav_channels`.
    let at = json.rfind("\"wav_channels\": 1").unwrap();
    let mut json = json;
    json.replace_range(at..at + "\"wav_channels\": 1".len(), "\"wav_channels\": 2");
    json
}

#[test]
fn capture_raw_timeline_v3_replays_a_stereo_loopback() {
    let t: RawTimeline = serde_json::from_str(&raw_json_v3_stereo()).unwrap();
    assert_eq!(t.schema_version, 3);
    assert_eq!((t.mic.wav_channels, t.loopback.wav_channels), (1, 2));
    let mic = MonoBuffer::new(SR48, ramp(2_000, 0.0));
    let lb = TrackBuffer::new(SR48, STEREO, stereo_ramp(2_000, 10_000.0)).unwrap();
    let mut src = ReplayBuilder::from_raw_timeline(&t, mic, lb)
        .unwrap()
        .build()
        .unwrap();
    assert_eq!(src.session().reference.as_ref().unwrap().channels, STEREO);
    assert_eq!(src.session().mic.channels, MONO);
    let events = run(&mut src);
    let l = frames_of(&events, TrackId::Reference);
    assert_eq!(l.len(), 2);
    // Same packets as the mono case, each frame carrying both channels.
    assert_eq!(l[0].0, origin());
    assert_eq!(l[0].1, stereo_ramp(240, 10_000.0));
    assert_eq!(l[1].0, DeviceTime::from_ticks(T0 + 208_333));
    assert_eq!(l[1].1, stereo_ramp(480, 11_000.0));
    // Mixing this anti-phase reference to mono cancels it.
    let mut mixed = Vec::new();
    mix_to_mono(&l[1].1, STEREO, &mut mixed);
    assert_eq!(mixed, vec![0.0; 480]);
    assert_eq!(
        assert_ends_once_last(&events).at,
        DeviceTime::from_ticks(t.end_qpc)
    );
}

/// [`raw_json`] at `version` with two mic device changes: the first `extra` (JSON
/// members appended to the change), the second plain.
fn raw_json_with_changes(version: u32, extra: &str) -> String {
    raw_json()
        .replacen(
            "\"schema_version\": 1",
            &format!("\"schema_version\": {version}"),
            1,
        )
        .replacen(
            "\"packets\": [",
            &format!(
                r#""device_changes": [
      {{"qpc": 1000210000, "track_frame": 1040, "reason": "default_device_changed",
       "device_name": "Headset (Hands-Free)", "device_format": null, "error": null{extra}}},
      {{"qpc": 1000310000, "track_frame": 1520, "reason": "default_device_changed",
       "device_name": "USB Mic", "device_format": null, "error": null}}
    ],
    "packets": ["#
            ),
            1,
        )
}

#[test]
fn capture_raw_timeline_v3_device_changes_carry_no_hands_free_warning() {
    let t: RawTimeline = serde_json::from_str(&raw_json_with_changes(3, "")).unwrap();
    assert_eq!(t.schema_version, 3);
    let warned: Vec<_> = t.mic.device_changes.iter().map(|c| c.hands_free).collect();
    assert_eq!(warned, vec![None, None]);
}

#[test]
fn capture_raw_timeline_v4_reads_the_hands_free_warning_and_ignores_unknown_fields() {
    let json = raw_json_with_changes(
        4,
        r#", "hands_free": {"sample_rate": 16000, "later": 1}, "unknown": [1, 2]"#,
    )
    .replacen(
        "\"qpc_units_per_second\"",
        "\"added_later\": {\"x\": true}, \"qpc_units_per_second\"",
        1,
    );
    let t: RawTimeline = serde_json::from_str(&json).unwrap();
    assert_eq!(t.schema_version, 4);
    let warned: Vec<_> = t.mic.device_changes.iter().map(|c| c.hands_free).collect();
    assert_eq!(
        warned,
        vec![
            Some(RawHandsFree {
                sample_rate: Some(16_000)
            }),
            None
        ]
    );
    // A warning with an unknown rate.
    let json = raw_json_with_changes(4, r#", "hands_free": {"sample_rate": null}"#);
    let t: RawTimeline = serde_json::from_str(&json).unwrap();
    assert_eq!(
        t.mic.device_changes[0].hands_free,
        Some(RawHandsFree { sample_rate: None })
    );
}

#[test]
fn capture_raw_replay_of_a_v4_hands_free_change_keeps_the_warning_off() {
    // The replay never warns (AGENTS: the raw replay always sets `warning: None`).
    let json = raw_json_with_changes(4, r#", "hands_free": {"sample_rate": 16000}"#);
    let t: RawTimeline = serde_json::from_str(&json).unwrap();
    let mic = MonoBuffer::new(SR48, ramp(2_000, 0.0));
    let lb = MonoBuffer::new(SR48, ramp(2_000, 10_000.0));
    let mut src = ReplayBuilder::from_raw_timeline(&t, mic, lb)
        .unwrap()
        .build()
        .unwrap();
    let events = run(&mut src);
    let changed: Vec<_> = markers_of(&events, TrackId::Mic)
        .into_iter()
        .filter_map(|m| match m.kind {
            DiscontinuityKind::DeviceChanged {
                device_name,
                warning,
                ..
            } => Some((device_name, warning)),
            DiscontinuityKind::DataLost | DiscontinuityKind::Note { .. } => None,
        })
        .collect();
    assert_eq!(
        changed,
        vec![
            (Some("Headset (Hands-Free)".into()), None),
            (Some("USB Mic".into()), None)
        ]
    );
}

/// [`raw_json`] at `version`, its mic section carrying `processing` (a JSON value) when given.
fn raw_json_with_processing(version: u32, processing: Option<&str>) -> String {
    let json = raw_json().replacen(
        "\"schema_version\": 1",
        &format!("\"schema_version\": {version}"),
        1,
    );
    match processing {
        Some(value) => json.replacen(
            "\"device_name\": \"Mic (Test)\",",
            &format!("\"device_name\": \"Mic (Test)\", \"processing\": {value},"),
            1,
        ),
        None => json,
    }
}

#[test]
fn capture_raw_timeline_v5_reads_whether_the_mic_was_opened_raw() {
    let raw: RawTimeline =
        serde_json::from_str(&raw_json_with_processing(5, Some("\"raw\""))).unwrap();
    assert_eq!(raw.schema_version, 5);
    assert_eq!(raw.mic.processing, RawProcessing::Raw);
    assert_eq!(raw.loopback.processing, RawProcessing::Default);
    // Absent: the default stream, in v5 and in every older version.
    for version in [1, 2, 3, 4, 5] {
        let t: RawTimeline =
            serde_json::from_str(&raw_json_with_processing(version, None)).unwrap();
        assert_eq!(t.mic.processing, RawProcessing::Default, "v{version}");
        assert_eq!(t.loopback.processing, RawProcessing::Default, "v{version}");
    }
    let v4: RawTimeline = serde_json::from_str(&raw_json_with_changes(4, "")).unwrap();
    assert_eq!(v4.mic.processing, RawProcessing::Default);
    // The default written out reads as the default.
    let named: RawTimeline =
        serde_json::from_str(&raw_json_with_processing(5, Some("\"default\""))).unwrap();
    assert_eq!(named.mic.processing, RawProcessing::Default);
}

#[test]
fn capture_raw_timeline_refuses_an_unknown_processing_of_the_mic() {
    // A take whose mic was opened some other way cannot be read as either kind.
    for value in ["\"agc\"", "\"Raw\"", "true", "null", "1"] {
        let json = raw_json_with_processing(5, Some(value));
        assert!(
            serde_json::from_str::<RawTimeline>(&json).is_err(),
            "{value} was accepted"
        );
    }
}

#[test]
fn capture_raw_replay_of_a_raw_take_equals_the_default_take() {
    // The field is informational: the replay of a raw take is the replay of the same take
    // recorded with the default stream, event for event.
    let replay = |processing: Option<&str>| {
        let t: RawTimeline =
            serde_json::from_str(&raw_json_with_processing(5, processing)).unwrap();
        let mic = MonoBuffer::new(SR48, ramp(2_000, 0.0));
        let lb = MonoBuffer::new(SR48, ramp(2_000, 10_000.0));
        let mut src = ReplayBuilder::from_raw_timeline(&t, mic, lb)
            .unwrap()
            .build()
            .unwrap();
        let session = src.session().clone();
        (session, run(&mut src))
    };
    let raw = replay(Some("\"raw\""));
    assert!(!raw.1.is_empty());
    assert_eq!(raw, replay(None));
}

#[test]
fn capture_raw_timeline_v1_and_v2_tracks_are_mono() {
    let mut t = raw_timeline();
    assert_eq!((t.mic.wav_channels, t.loopback.wav_channels), (1, 1));
    // `wav_channels` was always written, but a missing one still means mono.
    let json = raw_json().replace("\"wav_channels\": 1, ", "");
    let bare: RawTimeline = serde_json::from_str(&json).unwrap();
    assert_eq!((bare.mic.wav_channels, bare.loopback.wav_channels), (1, 1));
    t.schema_version = 2;
    let ok = MonoBuffer::new(SR48, ramp(2_000, 0.0));
    let src = ReplayBuilder::from_raw_timeline(&t, ok.clone(), ok)
        .unwrap()
        .build()
        .unwrap();
    assert_eq!(src.session().reference.as_ref().unwrap().channels, MONO);
}

#[test]
fn capture_raw_timeline_rejects_mismatched_channels() {
    let t: RawTimeline = serde_json::from_str(&raw_json_v3_stereo()).unwrap();
    let mic = MonoBuffer::new(SR48, ramp(2_000, 0.0));
    // A mono loopback buffer for a stereo timeline.
    assert_eq!(
        ReplayBuilder::from_raw_timeline(&t, mic.clone(), mic.clone()).unwrap_err(),
        ReplayError::ChannelMismatch {
            track: TrackId::Reference,
            expected: 2,
            actual: 1
        }
    );
    // Zero channels is a malformed timeline.
    let mut bad = t.clone();
    bad.mic.wav_channels = 0;
    let lb = TrackBuffer::new(SR48, STEREO, stereo_ramp(2_000, 0.0)).unwrap();
    assert_eq!(
        ReplayBuilder::from_raw_timeline(&bad, mic, lb).unwrap_err(),
        ReplayError::BadTimeline
    );
}
