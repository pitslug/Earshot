//! Replays a real `earshot-cli record-raw` recording (GATE-10) through the core replay
//! source. The recording is private (`fixtures/private/`, never committed): the test
//! looks in `EARSHOT_RAW_RECORDING` (a directory with `mic.wav`, `loopback.wav` and
//! `timestamps.json`; a missing directory there is an error), then in `gate10.dir` of the
//! private settings file (`.earshot-private/gate12.toml` in the home folder) under the nearest
//! ancestor of this crate that holds it. The test is `#[ignore]`d (a visible skip); run it
//! with `-- --ignored` on a host with the data.
//! Any supported schema works: v1/v2 recordings are mono, a v3 `loopback.wav` keeps
//! its device channels (stereo).

use std::fs::File;
use std::io::BufReader;
use std::path::{Path, PathBuf};

use earshot_core::audio::read_wav;
use earshot_core::capture::{
    AudioSource, CaptureEvent, DeviceTime, DiscontinuityKind, EndReason, RawTimeline,
    ReplayBuilder, SourceStatus, TrackBuffer, TrackId,
};

#[path = "../../earshot-asr-whisper/examples/asr_eval/gate12_private.rs"]
mod gate12_private;

fn recording_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("EARSHOT_RAW_RECORDING") {
        return PathBuf::from(dir);
    }
    let settings = gate12_private::load(gate12_private::default_path().as_deref())
        .unwrap_or_else(|e| panic!("private settings: {e}"))
        .unwrap_or_else(|| {
            panic!(
                "no raw recording: {}, and EARSHOT_RAW_RECORDING is not set",
                gate12_private::NOT_HERE
            )
        });
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .map(|a| a.join(&settings.gate10.dir))
        .find(|d| d.join("timestamps.json").is_file())
        .unwrap_or_else(|| {
            panic!("no raw recording at `gate10.dir` of the private settings under any ancestor")
        })
}

fn load_wav(path: &Path, channels: u16) -> TrackBuffer {
    let wav = read_wav(BufReader::new(File::open(path).unwrap())).unwrap();
    assert_eq!(wav.channels(), channels, "{} channels", path.display());
    TrackBuffer::try_from(wav).unwrap()
}

#[test]
#[ignore = "needs a private raw recording: gate10.dir in .earshot-private/gate12.toml or EARSHOT_RAW_RECORDING"]
fn capture_replays_gate10_raw_recording() {
    let dir = recording_dir();
    let timeline: RawTimeline = serde_json::from_reader(BufReader::new(
        File::open(dir.join("timestamps.json")).unwrap(),
    ))
    .unwrap();
    assert_eq!(timeline.mic.wav_channels, 1, "the mic is always mono");
    let mic = load_wav(&dir.join("mic.wav"), 1);
    let lb = load_wav(&dir.join("loopback.wav"), timeline.loopback.wav_channels);
    let wavs = [mic.samples().to_vec(), lb.samples().to_vec()];
    let channels = [1, usize::from(timeline.loopback.wav_channels)];

    let mut src = ReplayBuilder::from_raw_timeline(&timeline, mic, lb)
        .unwrap()
        .build()
        .unwrap();
    assert_eq!(
        src.session().origin,
        DeviceTime::from_ticks(timeline.start_qpc)
    );

    // Each track's delivered packets correspond, in order, to the packet-log entries
    // that kept any frames. Place each at its logged track_frame: the rebuilt track must
    // equal the WAV sample for sample (synthesized silence is zeros in both).
    let logs = [&timeline.mic, &timeline.loopback];
    let mut expected = logs.map(|t| {
        t.packets
            .iter()
            .filter(|p| u64::from(p.frames) > p.trimmed_frames)
            .map(|p| {
                (
                    p.track_frame as usize,
                    (u64::from(p.frames) - p.trimmed_frames) as usize,
                )
            })
            .collect::<Vec<_>>()
            .into_iter()
    });
    let mut rebuilt = [vec![0.0f32; wavs[0].len()], vec![0.0f32; wavs[1].len()]];
    let mut delivered = [0u64; 2];
    let mut data_lost = [0u32; 2];
    let mut last_ts: [Option<DeviceTime>; 2] = [None, None];
    let mut end = None;
    let mut events = Vec::new();
    loop {
        let status = src.pump(&mut events).unwrap();
        for e in events.drain(..) {
            match e {
                CaptureEvent::Frames {
                    track,
                    device_ts,
                    frames,
                } => {
                    let i = usize::from(track == TrackId::Reference);
                    let (slot, len) = expected[i]
                        .next()
                        .unwrap_or_else(|| panic!("{track:?}: more packets than the log"));
                    // Interleaved: `len` frames are `len * ch` samples from `slot * ch`.
                    let ch = channels[i];
                    let range = slot * ch..(slot + len) * ch;
                    assert_eq!(frames.len(), len * ch, "{track:?} packet at {slot}: length");
                    assert!(
                        frames[..] == wavs[i][range.clone()],
                        "{track:?} packet at {slot}: samples differ from the WAV"
                    );
                    if let Some(prev) = last_ts[i] {
                        assert!(
                            device_ts >= prev,
                            "{track:?} packet at {slot}: timestamp went backwards without a marker"
                        );
                    }
                    last_ts[i] = Some(device_ts);
                    rebuilt[i][range].copy_from_slice(&frames);
                    delivered[i] += len as u64;
                }
                CaptureEvent::Discontinuity {
                    track,
                    discontinuity,
                } => {
                    let i = usize::from(track == TrackId::Reference);
                    last_ts[i] = None;
                    if discontinuity.kind == DiscontinuityKind::DataLost {
                        data_lost[i] += 1;
                    }
                }
                CaptureEvent::End(e) => end = Some(e),
            }
        }
        if status == SourceStatus::Ended {
            break;
        }
    }
    let end = end.expect("End event");
    assert_eq!(end.reason, EndReason::Finished);
    assert_eq!(end.at, DeviceTime::from_ticks(timeline.end_qpc));

    for (i, track) in logs.into_iter().enumerate() {
        assert!(expected[i].next().is_none(), "track {i}: packets missing");
        assert_eq!(
            delivered[i] + track.synthesized_silence_frames,
            track.frames_written,
            "track {i}: delivered + synthesized == written"
        );
        let flagged = track.packets.iter().filter(|p| p.discontinuity).count() as u32;
        assert_eq!(data_lost[i], flagged);
    }
    assert!(rebuilt[0] == wavs[0], "mic WAV reconstructed from replay");
    assert!(
        rebuilt[1] == wavs[1],
        "loopback WAV reconstructed from replay"
    );
    eprintln!(
        "replayed {} (schema v{}): mic {} frames in {} packets, loopback {} frames x {} ch in {} packets",
        dir.display(),
        timeline.schema_version,
        delivered[0],
        timeline.mic.packets.len(),
        delivered[1],
        channels[1],
        timeline.loopback.packets.len()
    );
}
