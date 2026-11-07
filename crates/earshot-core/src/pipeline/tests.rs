//! Pipeline tests (fakes and simulator audio only). Every test name contains `pipeline`
//! so `cargo test -p earshot-core pipeline` selects them.

use std::num::NonZeroU16;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use rand_chacha::ChaCha8Rng;
use rand_core::{Rng, SeedableRng};

use super::*;
use crate::aec::{BLOCK, EchoCanceller, EchoCancellerConfig};
use crate::asr::{
    AsrError, BackendInfo, CancelToken, ChunkerConfig, EnergyVad, FakeTranscriber, Language,
    Segment, TranscribeOptions, Transcriber, TranscriptEvent, Transcription, Word,
};
use crate::audio::MonoBuffer;
use crate::audio::{PIPELINE_RATE, SampleRate, resample_all};
use crate::capture::{
    AudioSource, CaptureSession, CaptureSink, DeviceTime, Discontinuity, DiscontinuityKind,
    EndOfStream, EndReason, ReplayBuilder, SourceStatus, TrackBuffer, TrackId, TrackInfo,
};
use crate::identity::EntityId;
use crate::testutil;

const ORIGIN: DeviceTime = DeviceTime::from_ticks(5_000_000_000);
const STEREO: NonZeroU16 = NonZeroU16::new(2).unwrap();
const R16: SampleRate = SampleRate::HZ_16000;
const R48: SampleRate = SampleRate::HZ_48000;

fn noise(seed: u64, n: usize, amp: f32) -> Vec<f32> {
    let mut rng = ChaCha8Rng::seed_from_u64(seed);
    (0..n)
        .map(|_| {
            let u = (rng.next_u32() >> 8) as f32 / (1u32 << 24) as f32;
            (u * 2.0 - 1.0) * amp
        })
        .collect()
}

/// Config with drift steering off, so 16 kHz tracks bypass the resampler and outputs can
/// be compared bit for bit.
fn exact_config() -> PipelineConfig {
    PipelineConfig {
        drift_correction: false,
        ..PipelineConfig::default()
    }
}

/// Everything a run produced, with the audio of each track checked to be contiguous
/// from pipeline frame 0.
#[derive(Debug, Default)]
struct Collected {
    mic: Vec<f32>,
    them: Vec<f32>,
    levels: Vec<Levels>,
    stats: Vec<PipelineStats>,
    errors: Vec<PipelineError>,
    discontinuities: Vec<(TrackId, MeetingTime, DiscontinuityKind)>,
    transcripts: Vec<(TrackId, TranscriptEvent)>,
    final_pass: Option<FinalPassRequest>,
    ended: Option<PipelineEnd>,
}

impl Collected {
    fn absorb(&mut self, events: Vec<PipelineEvent>) {
        for e in events {
            assert!(
                self.ended.is_none() || e == PipelineEvent::Error(PipelineError::AfterEnd),
                "event after Ended: {e:?}"
            );
            match e {
                PipelineEvent::Audio(a) => {
                    let out = match a.track {
                        TrackId::Mic => &mut self.mic,
                        TrackId::Reference => &mut self.them,
                    };
                    assert_eq!(
                        a.start,
                        out.len() as u64,
                        "{:?} audio not contiguous",
                        a.track
                    );
                    assert!(!a.samples.is_empty(), "empty audio event");
                    assert!(a.samples.iter().all(|s| s.is_finite()), "non-finite output");
                    out.extend_from_slice(&a.samples);
                }
                PipelineEvent::Levels(l) => self.levels.push(l),
                PipelineEvent::Stats(s) => self.stats.push(s),
                PipelineEvent::Error(err) => self.errors.push(err),
                PipelineEvent::Discontinuity { track, at, kind } => {
                    self.discontinuities.push((track, at, kind));
                }
                PipelineEvent::Transcript { track, event } => {
                    assert!(
                        self.final_pass.is_none(),
                        "transcript after the final-pass request"
                    );
                    self.transcripts.push((track, event));
                }
                PipelineEvent::FinalPassRequested(req) => {
                    assert!(self.final_pass.is_none(), "two final-pass requests");
                    self.final_pass = Some(req);
                }
                PipelineEvent::Ended(end) => {
                    assert!(
                        self.final_pass.is_some(),
                        "Ended without a final-pass request"
                    );
                    self.ended = Some(end);
                }
            }
        }
    }

    fn last_stats(&self) -> &PipelineStats {
        self.stats.last().expect("stats emitted")
    }
}

/// Pumps `source` into a new pipeline until the end, polling after every pump.
fn run_source(mut source: impl AudioSource, cfg: PipelineConfig) -> Collected {
    let mut p = Pipeline::new(source.session().clone(), cfg).expect("pipeline");
    let mut got = Collected::default();
    loop {
        let status = source.pump(&mut p).expect("pump");
        got.absorb(p.poll());
        if status == SourceStatus::Ended {
            break;
        }
    }
    assert!(p.is_ended());
    assert!(got.ended.is_some(), "no Ended event");
    got
}

fn end(p: &mut Pipeline, at: DeviceTime) {
    p.end(EndOfStream {
        at,
        reason: EndReason::Stopped,
    });
}

/// Delivers `samples` (mono or interleaved) in packets of `packet` frames with exact
/// timestamps from `start`.
fn push_packets(
    p: &mut Pipeline,
    track: TrackId,
    samples: &[f32],
    channels: usize,
    rate: SampleRate,
    start: DeviceTime,
    packet: usize,
) {
    for (i, chunk) in samples.chunks(packet * channels).enumerate() {
        let ts = start.after_frames((i * packet) as u64, rate);
        p.frames(track, chunk, ts);
    }
}

fn dbfs(x: &[f32]) -> f64 {
    let e: f64 = x.iter().map(|&v| f64::from(v) * f64::from(v)).sum();
    10.0 * (e / x.len().max(1) as f64).max(1e-20).log10()
}

#[test]
fn pipeline_mic_only_16k_is_passed_through_exactly() {
    let mic = noise(1, 16_000 * 2 + 77, 0.3);
    let source = ReplayBuilder::new(ORIGIN, crate::audio::MonoBuffer::new(R16, mic.clone()))
        .build()
        .unwrap();
    let got = run_source(source, PipelineConfig::default());
    assert_eq!(
        got.mic, mic,
        "a mic-only 16 kHz session has no AEC and no resampler"
    );
    assert!(got.them.is_empty());
    assert!(got.errors.is_empty(), "{:?}", got.errors);
    let end = got.ended.unwrap();
    assert_eq!(end.end, MeetingTime::from_pipeline_frames(mic.len() as u64));
    assert_eq!(end.reason, EndReason::Finished);
}

#[test]
fn pipeline_mic_48k_matches_whole_clip_resampling() {
    let mic = noise(2, 48_000 * 2 + 5, 0.3);
    let source = ReplayBuilder::new(ORIGIN, crate::audio::MonoBuffer::new(R48, mic.clone()))
        .build()
        .unwrap();
    let got = run_source(source, PipelineConfig::default());
    let want = resample_all(&mic, R48, PIPELINE_RATE).unwrap();
    assert_eq!(
        got.mic, want,
        "streamed resampling must equal the whole-clip resampling"
    );
}

#[test]
fn pipeline_cleaned_mic_is_the_canceller_output_with_its_latency_removed() {
    let sim = testutil::run_scenario("double-talk-ser-0");
    let n = 16_000 * 12;
    let (mic, reference) = (&sim.mic[..n], &sim.reference[..n]);
    let source = ReplayBuilder::new(ORIGIN, crate::audio::MonoBuffer::new(R16, mic.to_vec()))
        .reference(crate::audio::MonoBuffer::new(R16, reference.to_vec()))
        .build()
        .unwrap();
    let got = run_source(source, exact_config());

    let mut aec = EchoCanceller::new(EchoCancellerConfig::default());
    let latency = aec.output_latency();
    let blocks = n / BLOCK;
    let direct = aec.process_vec(&mic[..blocks * BLOCK], &reference[..blocks * BLOCK]);

    assert_eq!(got.mic.len(), n, "cleaned mic covers the whole session");
    assert_eq!(got.them.len(), n);
    assert_eq!(
        &got.mic[..blocks * BLOCK - latency],
        &direct[latency..],
        "cleaned sample k must be the canceller output for mic sample k"
    );
    assert_eq!(
        got.them, reference,
        "the Them track is the aligned reference"
    );
    let aec_stats = got.last_stats().aec.expect("AEC stats with a reference");
    assert!(
        aec_stats.locked,
        "the canceller locks on the simulated echo"
    );
}

#[test]
fn pipeline_stats_report_the_cancellers_veto_and_suppression_gain() {
    // With one stats interval per block, every Stats carries the status of one of the
    // canceller blocks just processed (the cleaned mic lags by the canceller latency, and a
    // pump may process two blocks at once).
    let sim = testutil::run_scenario("double-talk-ser-0");
    let n = 16_000 * 12;
    let (mic, reference) = (&sim.mic[..n], &sim.reference[..n]);
    let source = ReplayBuilder::new(ORIGIN, crate::audio::MonoBuffer::new(R16, mic.to_vec()))
        .reference(crate::audio::MonoBuffer::new(R16, reference.to_vec()))
        .build()
        .unwrap();
    let cfg = PipelineConfig {
        stats_interval: Duration::from_millis(8),
        ..exact_config()
    };
    let got = run_source(source, cfg);

    let mut aec = EchoCanceller::new(EchoCancellerConfig::default());
    let mut out = vec![0.0; BLOCK];
    let direct: Vec<(bool, bool, u32)> = mic
        .as_chunks::<BLOCK>()
        .0
        .iter()
        .zip(reference.as_chunks::<BLOCK>().0)
        .map(|(m, r)| {
            aec.process(m, r, &mut out);
            let s = aec.status();
            (s.doubletalk, s.vetoed, s.mean_gain.to_bits())
        })
        .collect();
    let mut checked = 0;
    let (mut calls, mut vetoed, mut suppressed) = (0, 0, 0);
    for stats in &got.stats {
        let a = stats.aec.expect("AEC stats with a reference");
        let block = usize::try_from(stats.end.to_pipeline_frames()).unwrap() / BLOCK;
        if block + 3 >= direct.len() {
            continue; // the end drains the latency with silence the direct run never saw
        }
        let got_status = (a.double_talk, a.vetoed, a.suppression_gain.to_bits());
        assert!(
            direct[block + 1..=block + 3].contains(&got_status),
            "stats at block {block} report {got_status:?}, not a status of the blocks just processed"
        );
        checked += 1;
        calls += usize::from(a.double_talk);
        vetoed += usize::from(a.vetoed);
        suppressed += usize::from(a.suppression_gain < 1.0);
    }
    assert!(checked > 1000, "only {checked} stats checked");
    assert!(calls > 0, "the scenario has double-talk calls");
    // The direct run vetoes mono calls in this window, so the stats must show some: a
    // constant `vetoed: false` fails here.
    assert!(
        direct.iter().any(|&(_, v, _)| v),
        "the direct run has vetoed calls"
    );
    assert!(vetoed > 0, "the stats report vetoed calls");
    assert!(suppressed > 0, "the scenario has suppressed blocks");
}

#[test]
fn pipeline_stereo_reference_uses_the_stereo_canceller() {
    let base = testutil::scenarios().get("far-only").unwrap().clone();
    let sim = testutil::run(&testutil::stereo_variant(&base, 0.5, None));
    let [l, r] = sim.reference_lr.clone().expect("stereo scenario");
    let n = 16_000 * 8;
    let interleaved: Vec<f32> = (0..n).flat_map(|i| [l[i], r[i]]).collect();
    let source = ReplayBuilder::new(
        ORIGIN,
        crate::audio::MonoBuffer::new(R16, sim.mic[..n].to_vec()),
    )
    .reference(TrackBuffer::new(R16, STEREO, interleaved).unwrap())
    .build()
    .unwrap();
    let got = run_source(source, exact_config());

    let mut aec = EchoCanceller::new(EchoCancellerConfig::default());
    let latency = aec.output_latency();
    let m = (n / BLOCK) * BLOCK;
    let mut direct = vec![0.0; m];
    aec.process_stereo(&sim.mic[..m], &l[..m], &r[..m], &mut direct);
    assert_eq!(&got.mic[..m - latency], &direct[latency..]);
    let mono: Vec<f32> = (0..n).map(|i| (l[i] + r[i]) * 0.5).collect();
    assert_eq!(
        got.them, mono,
        "the Them track is the mono mix of the reference"
    );
}

#[test]
fn pipeline_resampled_48k_tracks_still_cancel_echo() {
    let sim = testutil::run_scenario("far-only");
    let mic48 = resample_all(&sim.mic, R16, R48).unwrap();
    let ref48 = resample_all(&sim.reference, R16, R48).unwrap();
    let source = ReplayBuilder::new(ORIGIN, crate::audio::MonoBuffer::new(R48, mic48))
        .reference(crate::audio::MonoBuffer::new(R48, ref48))
        .build()
        .unwrap();
    let got = run_source(source, PipelineConfig::default());
    assert_eq!(got.mic.len(), sim.mic.len());
    // Far end only: after convergence the cleaned mic must be far below the mic.
    let tail = 16_000 * 10..sim.mic.len();
    let attenuation = dbfs(&sim.mic[tail.clone()]) - dbfs(&got.mic[tail]);
    assert!(
        attenuation > 15.0,
        "echo attenuation through 48 kHz tracks {attenuation:.1} dB"
    );
}

#[test]
fn pipeline_reference_gap_is_zero_filled_and_alignment_kept() {
    let mic = noise(3, 16_000 * 3, 0.2);
    let reference = noise(4, 16_000 * 3, 0.2);
    let (gap_start, gap_len) = (16_000u64, 4_000u64);
    let source = ReplayBuilder::new(ORIGIN, crate::audio::MonoBuffer::new(R16, mic.clone()))
        .reference(crate::audio::MonoBuffer::new(R16, reference.clone()))
        .gap(TrackId::Reference, gap_start, gap_len)
        .unwrap()
        .build()
        .unwrap();
    let got = run_source(source, exact_config());
    let mut want = reference.clone();
    want[gap_start as usize..(gap_start + gap_len) as usize].fill(0.0);
    assert_eq!(got.them, want, "a timestamp gap is silence at its place");
    assert_eq!(got.mic.len(), mic.len());
    let stats = got.last_stats().reference.as_ref().unwrap();
    assert_eq!(stats.zero_filled, gap_len);
    assert_eq!(stats.restarts, 1);
}

#[test]
fn pipeline_reference_idle_past_the_lag_keeps_its_resampler_tail() {
    // A 48 kHz stereo loopback goes idle for 2 s (longer than max_track_lag) and resumes
    // at its true timestamps. The silence filled meanwhile must come after the samples the
    // resampler still held for the audio before the pause: none of them is dropped as
    // late, and the resumed audio is a single new run.
    let secs = 6u64;
    let mic = noise(40, 16_000 * secs as usize, 0.2);
    let far: Vec<f32> = noise(41, 48_000 * secs as usize, 0.2)
        .into_iter()
        .flat_map(|x| [x, x])
        .collect();
    let far = TrackBuffer::new(R48, STEREO, far).unwrap();
    let cfg = exact_config();
    assert!(cfg.max_track_lag < Duration::from_secs(2));
    let source = ReplayBuilder::new(ORIGIN, crate::audio::MonoBuffer::new(R16, mic.clone()))
        .reference(far)
        .gap(TrackId::Reference, 48_000 * 2, 48_000 * 2)
        .unwrap()
        .build()
        .unwrap();
    let got = run_source(source, cfg);
    assert_eq!(got.mic.len(), mic.len());
    let r = got.last_stats().reference.unwrap();
    assert_eq!((r.restarts, r.late_dropped), (1, 0), "{r:?}");
    let m = got.last_stats().mic;
    assert_eq!(
        (m.restarts, m.late_dropped, m.zero_filled),
        (0, 0, 0),
        "{m:?}"
    );
}
#[test]
fn pipeline_idle_reference_never_stalls_the_mic() {
    let session = CaptureSession::new(ORIGIN, TrackInfo::new(R16), Some(TrackInfo::new(R16)));
    let cfg = exact_config();
    let lag = PIPELINE_RATE.frames_for(cfg.max_track_lag);
    let mut p = Pipeline::new(session, cfg).unwrap();
    let mic = noise(5, 16_000 * 5, 0.2);
    // The loopback delivers one second, then nothing (an idle render endpoint).
    push_packets(
        &mut p,
        TrackId::Reference,
        &noise(6, 16_000, 0.2),
        1,
        R16,
        ORIGIN,
        160,
    );
    push_packets(&mut p, TrackId::Mic, &mic, 1, R16, ORIGIN, 160);
    let mut got = Collected::default();
    got.absorb(p.poll());
    let latency = EchoCanceller::new(EchoCancellerConfig::default()).output_latency();
    assert!(
        got.mic.len() + lag + latency + BLOCK >= mic.len(),
        "cleaned {} of {} mic samples while the reference is idle",
        got.mic.len(),
        mic.len()
    );
    // Late loopback audio for positions already given up is dropped, not misplaced.
    let late = ORIGIN.after_frames(16_000, R16);
    p.frames(TrackId::Reference, &noise(7, 160, 0.2), late);
    got.absorb(p.poll());
    end(&mut p, ORIGIN.after_frames(mic.len() as u64, R16));
    got.absorb(p.poll());
    assert_eq!(got.mic.len(), mic.len());
    assert_eq!(got.them.len(), mic.len());
    assert_eq!(
        got.last_stats().reference.as_ref().unwrap().late_dropped,
        160
    );
}

#[test]
fn pipeline_non_finite_and_out_of_range_samples_are_sanitised() {
    let session = CaptureSession::new(
        ORIGIN,
        TrackInfo::new(R48),
        Some(TrackInfo::new(R48).with_channels(STEREO)),
    );
    let mut p = Pipeline::new(session, PipelineConfig::default()).unwrap();
    let mut mic = noise(8, 48_000, 0.2);
    let mut reference = noise(9, 96_000, 0.2);
    for (i, bad) in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY, 1e30]
        .iter()
        .enumerate()
    {
        mic[1000 + i] = *bad;
        reference[3001 + 2 * i] = *bad;
    }
    push_packets(&mut p, TrackId::Mic, &mic, 1, R48, ORIGIN, 480);
    push_packets(&mut p, TrackId::Reference, &reference, 2, R48, ORIGIN, 480);
    end(&mut p, ORIGIN.after_frames(48_000, R48));
    let mut got = Collected::default();
    got.absorb(p.poll());
    let stats = got.last_stats();
    assert_eq!((stats.mic.non_finite, stats.mic.clamped), (3, 1));
    let r = stats.reference.as_ref().unwrap();
    assert_eq!((r.non_finite, r.clamped), (3, 1));
    assert_eq!(got.mic.len(), 16_000);
}

#[test]
fn pipeline_overlapping_and_backwards_timestamps_are_trimmed() {
    let session = CaptureSession::new(ORIGIN, TrackInfo::new(R16), None);
    let mut p = Pipeline::new(session, PipelineConfig::default()).unwrap();
    let a = noise(10, 1600, 0.2);
    let b = noise(11, 1600, 0.2);
    p.frames(TrackId::Mic, &a, ORIGIN);
    // Overlaps the first packet by half.
    p.frames(TrackId::Mic, &b, ORIGIN.after_frames(800, R16));
    // Entirely in the past.
    p.frames(
        TrackId::Mic,
        &noise(12, 400, 0.2),
        ORIGIN.after_frames(100, R16),
    );
    // Before the origin: only the part from frame 0 on could count, and it is past.
    p.frames(
        TrackId::Mic,
        &noise(13, 400, 0.2),
        DeviceTime::from_ticks(ORIGIN.ticks() - 10_000),
    );
    end(&mut p, ORIGIN);
    let mut got = Collected::default();
    got.absorb(p.poll());
    let mut want = a.clone();
    want.extend_from_slice(&b[800..]);
    assert_eq!(got.mic, want);
    assert_eq!(got.last_stats().mic.late_dropped, 800 + 400 + 400);
    assert!(got.errors.is_empty(), "{:?}", got.errors);
}

#[test]
fn pipeline_timestamp_jump_beyond_max_gap_is_an_error_and_stays_bounded() {
    let session = CaptureSession::new(ORIGIN, TrackInfo::new(R16), None);
    let mut p = Pipeline::new(session, PipelineConfig::default()).unwrap();
    let a = noise(14, 1600, 0.2);
    let b = noise(15, 1600, 0.2);
    p.frames(TrackId::Mic, &a, ORIGIN);
    let hour = ORIGIN + Duration::from_secs(3600);
    p.frames(TrackId::Mic, &b, hour);
    end(&mut p, hour.after_frames(1600, R16));
    let mut got = Collected::default();
    got.absorb(p.poll());
    assert!(
        matches!(
            got.errors.as_slice(),
            [PipelineError::TimestampJump {
                track: TrackId::Mic,
                ..
            }]
        ),
        "{:?}",
        got.errors
    );
    let mut want = a.clone();
    want.extend_from_slice(&b);
    assert_eq!(
        got.mic, want,
        "after a jump the track continues where it was"
    );
    assert_eq!(
        got.ended.unwrap().end,
        MeetingTime::from_pipeline_frames(3200)
    );
}

#[test]
fn pipeline_device_change_to_a_new_rate_keeps_the_timeline() {
    let session = CaptureSession::new(ORIGIN, TrackInfo::new(R48), None);
    let mut p = Pipeline::new(session, PipelineConfig::default()).unwrap();
    push_packets(
        &mut p,
        TrackId::Mic,
        &noise(16, 48_000, 0.2),
        1,
        R48,
        ORIGIN,
        480,
    );
    let r441 = SampleRate::new(44_100).unwrap();
    let t1 = ORIGIN + Duration::from_secs(1);
    p.discontinuity(
        TrackId::Mic,
        Discontinuity {
            at: t1,
            kind: DiscontinuityKind::DeviceChanged {
                device_name: Some("Headset".into()),
                rate: r441,
                warning: None,
            },
        },
    );
    push_packets(
        &mut p,
        TrackId::Mic,
        &noise(17, 44_100, 0.2),
        1,
        r441,
        t1,
        441,
    );
    end(&mut p, ORIGIN + Duration::from_secs(2));
    let mut got = Collected::default();
    got.absorb(p.poll());
    assert_eq!(got.mic.len(), 32_000);
    assert_eq!(got.discontinuities.len(), 1);
    assert_eq!(got.discontinuities[0].1, MeetingTime::from_millis(1000));
    assert!(got.errors.is_empty(), "{:?}", got.errors);
    // The new device's audio is placed right at 1 s (its stream starts there; no gap).
    assert!(dbfs(&got.mic[16_100..17_000]) > -30.0);
}

#[test]
fn pipeline_unsupported_rate_mutes_the_track_with_an_error() {
    let bad = SampleRate::new(500).unwrap();
    assert!(matches!(
        Pipeline::new(
            CaptureSession::new(ORIGIN, TrackInfo::new(bad), None),
            PipelineConfig::default()
        ),
        Err(PipelineError::UnsupportedRate {
            track: TrackId::Mic,
            rate_hz: 500
        })
    ));

    let session = CaptureSession::new(ORIGIN, TrackInfo::new(R16), None);
    let mut p = Pipeline::new(session, PipelineConfig::default()).unwrap();
    p.frames(TrackId::Mic, &noise(18, 1600, 0.2), ORIGIN);
    let t = ORIGIN.after_frames(1600, R16);
    let marker = |rate| Discontinuity {
        at: t,
        kind: DiscontinuityKind::DeviceChanged {
            device_name: None,
            rate,
            warning: None,
        },
    };
    p.discontinuity(TrackId::Mic, marker(bad));
    p.frames(TrackId::Mic, &[0.5; 50], t);
    end(&mut p, t + Duration::from_millis(100));
    let mut got = Collected::default();
    got.absorb(p.poll());
    assert!(matches!(
        got.errors.as_slice(),
        [PipelineError::UnsupportedRate {
            track: TrackId::Mic,
            rate_hz: 500
        }]
    ));
    assert_eq!(got.last_stats().mic.muted_frames, 50);
    assert_eq!(
        got.mic.len(),
        1600 + 1600,
        "muted time is silence up to the end"
    );
    assert!(got.mic[1600..].iter().all(|&s| s == 0.0));
}

#[test]
fn pipeline_untrusted_input_shapes_never_panic() {
    // No reference track: reference frames and markers are errors.
    let mut p = Pipeline::new(
        CaptureSession::new(ORIGIN, TrackInfo::new(R16), None),
        PipelineConfig::default(),
    )
    .unwrap();
    p.frames(TrackId::Reference, &[0.1; 160], ORIGIN);
    p.discontinuity(
        TrackId::Reference,
        Discontinuity {
            at: ORIGIN,
            kind: DiscontinuityKind::DataLost,
        },
    );
    p.frames(TrackId::Mic, &[], ORIGIN);
    // A marker from the far future: its meeting time must not overflow.
    p.discontinuity(
        TrackId::Mic,
        Discontinuity {
            at: DeviceTime::from_ticks(u64::MAX),
            kind: DiscontinuityKind::DataLost,
        },
    );
    p.frames(
        TrackId::Mic,
        &[0.1; 160],
        DeviceTime::from_ticks(u64::MAX - 10),
    );
    end(&mut p, DeviceTime::from_ticks(u64::MAX));
    end(&mut p, ORIGIN);
    p.frames(TrackId::Mic, &[0.1; 160], ORIGIN);
    let mut got = Collected::default();
    got.absorb(p.poll());
    let missing = PipelineError::MissingTrack {
        track: TrackId::Reference,
    };
    assert!(
        matches!(
            got.errors.as_slice(),
            [m1, m2, PipelineError::TimestampJump { track: TrackId::Mic, .. }, PipelineError::AfterEnd, PipelineError::AfterEnd]
                if *m1 == missing && *m2 == missing
        ),
        "{:?}",
        got.errors
    );
    // The jump was not believed and the absurd end time is beyond max_gap: one packet.
    assert_eq!(got.mic.len(), 160);

    // A stereo reference with a trailing partial frame.
    let mut p = Pipeline::new(
        CaptureSession::new(
            ORIGIN,
            TrackInfo::new(R16),
            Some(TrackInfo::new(R16).with_channels(STEREO)),
        ),
        PipelineConfig::default(),
    )
    .unwrap();
    p.frames(TrackId::Reference, &[0.1; 321], ORIGIN);
    p.frames(TrackId::Mic, &[0.1; 160], ORIGIN);
    end(&mut p, ORIGIN.after_frames(160, R16));
    let mut got = Collected::default();
    got.absorb(p.poll());
    assert_eq!(
        got.last_stats().reference.as_ref().unwrap().partial_frames,
        1
    );
    assert_eq!(got.them.len(), 160);
}

#[test]
fn pipeline_reference_drift_is_steered_and_bounded() {
    // The loopback device clock runs 200 ppm slow against QPC: its 48 000 frames span
    // 1.0002 s of device time. Without steering the reference falls 12 ms behind the mic
    // in a minute; the AEC delay search would follow it, but over an hour it would leave
    // the 600 ms window.
    let secs = 60;
    let run = |drift_correction: bool| {
        let session = CaptureSession::new(ORIGIN, TrackInfo::new(R16), Some(TrackInfo::new(R48)));
        let cfg = PipelineConfig {
            drift_correction,
            ..PipelineConfig::default()
        };
        let mut p = Pipeline::new(session, cfg).unwrap();
        let mut got = Collected::default();
        let mic = noise(19, 160, 0.1);
        let reference = noise(20, 480, 0.1);
        for i in 0..secs * 100u64 {
            p.frames(TrackId::Mic, &mic, ORIGIN.after_frames(i * 160, R16));
            let ticks = (i as f64 * 100_000.0 * 1.0002).round() as u64;
            p.frames(
                TrackId::Reference,
                &reference,
                DeviceTime::from_ticks(ORIGIN.ticks() + ticks),
            );
            got.absorb(p.poll());
        }
        got
    };
    let free = run(false);
    let steered = run(true);
    let offset = |g: &Collected| {
        let s = g.last_stats();
        s.reference.as_ref().unwrap().skew_ms - s.mic.skew_ms
    };
    assert!(
        (offset(&free) - 12.0).abs() < 1.0,
        "unsteered offset {:.2} ms",
        offset(&free)
    );
    assert!(
        offset(&steered).abs() < 6.0,
        "steered offset {:.2} ms",
        offset(&steered)
    );
    let ppm = steered.last_stats().reference_correction_ppm;
    assert!((ppm - 200.0).abs() < 60.0, "correction {ppm:.0} ppm");
    assert!(steered.errors.is_empty() && free.errors.is_empty());
    // Independent of the controller's own error signal: every reference sample was
    // placed exactly once (no silence inserted, nothing dropped) and the Them track keeps
    // up with the mic.
    let mic_len = (secs * 16_000) as usize;
    for g in [&free, &steered] {
        let r = g.last_stats().reference.unwrap();
        assert_eq!((r.restarts, r.zero_filled, r.late_dropped), (0, 0, 0));
        assert!(
            g.them.len() <= mic_len && g.them.len() + 1_000 >= mic_len,
            "Them {} of {mic_len}",
            g.them.len()
        );
    }
}

#[test]
fn pipeline_reference_restart_keeps_alignment_under_mic_skew() {
    // Both devices run 1000 ppm slow against the shared clock (stamps 10.01 ms apart for
    // 10 ms of audio), so after 20 s each track's placement lags its stamps by about
    // 20 ms. A restarted reference run anchored at its own stamp would sit 20 ms (320
    // samples) away from the mic; it must keep the mic's placement instead.
    let secs = 20;
    let session = CaptureSession::new(ORIGIN, TrackInfo::new(R16), Some(TrackInfo::new(R16)));
    let mut p = Pipeline::new(session, exact_config()).unwrap();
    let mic = noise(40, 160, 0.01);
    let reference = noise(41, secs * 16_000, 0.1);
    let stamp = |i: usize| DeviceTime::from_ticks(ORIGIN.ticks() + i as u64 * 100_100);
    let gap = (secs * 100 / 2)..(secs * 100 / 2 + 10); // 100 ms without loopback packets
    let mut got = Collected::default();
    for i in 0..secs * 100 {
        p.frames(TrackId::Mic, &mic, stamp(i));
        if !gap.contains(&i) {
            p.frames(
                TrackId::Reference,
                &reference[i * 160..(i + 1) * 160],
                stamp(i),
            );
        }
        got.absorb(p.poll());
    }
    let s = got.last_stats();
    let r = s.reference.unwrap();
    assert_eq!(r.restarts, 1);
    assert!(s.mic.skew_ms > 15.0, "mic skew {:.1} ms", s.mic.skew_ms);
    assert!(
        (r.skew_ms - s.mic.skew_ms).abs() < 2.0,
        "reference {:.1} ms vs mic {:.1} ms",
        r.skew_ms,
        s.mic.skew_ms
    );
    // Sample-count placement continues across the restart within the skew estimate's
    // lag (1000 ppm x 1 s smoothing = 16 samples); stamp placement would be 320 off.
    let first = gap.end * 160;
    let probe = &reference[first..first + 160];
    let found = (first - 400..first + 400)
        .find(|&k| got.them[k..k + 160] == *probe)
        .expect("restarted packet in the Them track");
    assert!(
        found.abs_diff(first) <= 32,
        "placed at {found}, sample count says {first}"
    );
}

#[test]
fn pipeline_backward_jump_beyond_max_gap_is_an_error_and_keeps_the_track_going() {
    let cfg = PipelineConfig {
        max_gap: Duration::from_secs(1),
        ..PipelineConfig::default()
    };
    let mut p = Pipeline::new(CaptureSession::new(ORIGIN, TrackInfo::new(R16), None), cfg).unwrap();
    let a = noise(42, 32_000, 0.2);
    push_packets(&mut p, TrackId::Mic, &a, 1, R16, ORIGIN, 160);
    // 0.5 s back: believed (an overlap), dropped as already written.
    let b = noise(43, 160, 0.2);
    p.frames(TrackId::Mic, &b, ORIGIN.after_frames(24_000, R16));
    // 1.5 s back: beyond max_gap, not believed; the packet continues the track.
    let c = noise(44, 160, 0.2);
    p.frames(TrackId::Mic, &c, ORIGIN.after_frames(8_000, R16));
    end(&mut p, ORIGIN);
    let mut got = Collected::default();
    got.absorb(p.poll());
    assert!(
        matches!(
            got.errors.as_slice(),
            [PipelineError::TimestampJump {
                track: TrackId::Mic,
                backwards: true,
                ..
            }]
        ),
        "{:?}",
        got.errors
    );
    let mut want = a.clone();
    want.extend_from_slice(&c);
    assert_eq!(got.mic, want);
    assert_eq!(got.last_stats().mic.late_dropped, 160);
}

#[test]
fn pipeline_usb_timestamp_jitter_is_not_a_gap() {
    // 10 ms packets at 48 kHz stamped alternately 1.0 ms early and 0.9 ms late against
    // the nominal grid (USB audio): every packet is within gap_tolerance of the previous
    // packet's end, so the stream is one run and resamples exactly as a whole clip.
    let audio = noise(45, 48_000 * 5, 0.2);
    let grid = ORIGIN.ticks();
    let stamp = |i: usize| {
        let jitter: i64 = if i.is_multiple_of(2) { -10_000 } else { 9_000 };
        DeviceTime::from_ticks((grid as i64 + i as i64 * 100_000 + jitter) as u64)
    };
    let origin = stamp(0);
    let mut p = Pipeline::new(
        CaptureSession::new(origin, TrackInfo::new(R48), None),
        PipelineConfig::default(),
    )
    .unwrap();
    for (i, chunk) in audio.chunks(480).enumerate() {
        p.frames(TrackId::Mic, chunk, stamp(i));
    }
    end(&mut p, origin);
    let mut got = Collected::default();
    got.absorb(p.poll());
    assert_eq!(got.mic, resample_all(&audio, R48, PIPELINE_RATE).unwrap());
    let m = got.last_stats().mic;
    assert_eq!((m.restarts, m.late_dropped, m.zero_filled), (0, 0, 0));
}

#[test]
fn pipeline_end_pads_both_tracks_to_the_end_time_and_ends_last() {
    let session = CaptureSession::new(ORIGIN, TrackInfo::new(R16), Some(TrackInfo::new(R16)));
    let mut p = Pipeline::new(session, exact_config()).unwrap();
    push_packets(
        &mut p,
        TrackId::Mic,
        &noise(21, 16_000, 0.2),
        1,
        R16,
        ORIGIN,
        160,
    );
    push_packets(
        &mut p,
        TrackId::Reference,
        &noise(22, 8_000, 0.2),
        1,
        R16,
        ORIGIN,
        160,
    );
    let mut got = Collected::default();
    got.absorb(p.poll());
    end(&mut p, ORIGIN + Duration::from_millis(1250));
    got.absorb(p.poll());
    assert_eq!(got.mic.len(), 20_000);
    assert_eq!(got.them.len(), 20_000);
    let end = got.ended.as_ref().unwrap();
    assert_eq!(end.end, MeetingTime::from_millis(1250));
    assert_eq!(end.reason, EndReason::Stopped);
    assert_eq!(got.last_stats().end, MeetingTime::from_millis(1250));
    assert!(p.poll().is_empty());
}

#[test]
fn pipeline_levels_and_stats_follow_the_meeting_timeline() {
    let n = 16_000 * 3 + 800;
    let mut mic = noise(23, n, 0.1);
    mic[16_000..17_600].fill(0.0);
    let source = ReplayBuilder::new(ORIGIN, crate::audio::MonoBuffer::new(R16, mic))
        .build()
        .unwrap();
    let got = run_source(source, PipelineConfig::default());
    assert_eq!(got.levels.len(), 30);
    let ends: Vec<u64> = got.stats.iter().map(|s| s.end.as_millis()).collect();
    assert_eq!(ends, [1000, 2000, 3000, 3050]);
    let l = &got.levels[10];
    assert_eq!(l.end, MeetingTime::from_millis(1100));
    assert!(l.mic_dbfs < -100.0, "silent window {:?}", l);
    assert!(
        (got.levels[0].mic_dbfs - -24.8).abs() < 1.0,
        "{:?}",
        got.levels[0]
    );
    assert_eq!(got.levels[0].mic_dbfs, got.levels[0].cleaned_dbfs);
    assert!(got.levels[0].reference_dbfs.is_none());
    assert!(got.stats[0].aec.is_none() && got.stats[0].reference.is_none());
}

#[test]
fn pipeline_is_send_so_the_runtime_can_own_it_on_its_thread() {
    fn assert_send<T: Send>() {}
    assert_send::<Pipeline>();
    assert_send::<PipelineEvent>();
}

#[test]
fn pipeline_config_validation_rejects_bad_values() {
    let session = CaptureSession::new(ORIGIN, TrackInfo::new(R16), None);
    let bad = [
        PipelineConfig {
            meter_interval: Duration::ZERO,
            ..PipelineConfig::default()
        },
        PipelineConfig {
            stats_interval: Duration::ZERO,
            ..PipelineConfig::default()
        },
        PipelineConfig {
            gap_tolerance: Duration::from_secs(1),
            ..PipelineConfig::default()
        },
        PipelineConfig {
            max_drift_correction_ppm: 20_000.0,
            ..PipelineConfig::default()
        },
        PipelineConfig {
            drift_time_constant: Duration::ZERO,
            ..PipelineConfig::default()
        },
        PipelineConfig {
            max_gap: Duration::from_millis(100),
            ..PipelineConfig::default()
        },
        PipelineConfig {
            max_gap: Duration::from_secs(2 * 3600),
            ..PipelineConfig::default()
        },
    ];
    for cfg in bad {
        assert!(
            matches!(
                Pipeline::new(session.clone(), cfg.clone()),
                Err(PipelineError::InvalidConfig(_))
            ),
            "{cfg:?}"
        );
    }
    assert!(PipelineConfig::default().validate().is_ok());
}

// --- Live transcription (T6.2b) ---

const SPEECH: f32 = 0.3;
const QUIET: f32 = 1e-4;

/// `n` words from `start_ms`, 300 ms long, one every 400 ms.
fn words(prefix: &str, start_ms: u64, n: usize) -> Vec<Word> {
    (0..n)
        .map(|i| {
            let s = start_ms + 400 * i as u64;
            Word::new(format!("{prefix}{i}"), s, s + 300, 0.9)
        })
        .collect()
}

/// Non-repeating audio of `total_ms`: loud inside the word spans, quiet elsewhere.
fn speech_audio(words: &[Word], total_ms: u64, seed: u64) -> Vec<f32> {
    let n = (total_ms * 16) as usize;
    let base = noise(seed, n, 1.0);
    let loud = |i: usize| {
        let ms = i as u64 / 16;
        words
            .iter()
            .any(|w| (w.start.as_millis()..w.end.as_millis()).contains(&ms))
    };
    (0..n)
        .map(|i| base[i] * if loud(i) { SPEECH } else { QUIET })
        .collect()
}

/// A transcriber counting its calls through a shared counter.
struct Counting {
    inner: FakeTranscriber,
    calls: Arc<AtomicUsize>,
}

impl Transcriber for Counting {
    fn transcribe(
        &mut self,
        audio: &MonoBuffer,
        options: &TranscribeOptions,
        cancel: &CancelToken,
    ) -> Result<Transcription, AsrError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.inner.transcribe(audio, options, cancel)
    }
    fn backend_info(&self) -> BackendInfo {
        self.inner.backend_info()
    }
}

fn entity(n: u8) -> EntityId {
    EntityId::from_bytes([n; 16])
}

fn live(fake: FakeTranscriber, track_entity: EntityId) -> (LiveAsr, Arc<AtomicUsize>) {
    let calls = Arc::new(AtomicUsize::new(0));
    let mut n = 0u8;
    let asr = LiveAsr {
        transcriber: Box::new(Counting {
            inner: fake,
            calls: Arc::clone(&calls),
        }),
        vad: Box::new(EnergyVad::default()),
        config: ChunkerConfig::default(),
        track_entity,
        segment_ids: Box::new(move || {
            n = n.wrapping_add(1);
            EntityId::from_bytes([0xA0, n, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0])
        }),
    };
    (asr, calls)
}

fn final_segments(got: &Collected, track: TrackId) -> Vec<Segment> {
    got.transcripts
        .iter()
        .filter(|(t, _)| *t == track)
        .filter_map(|(_, e)| match e {
            TranscriptEvent::Final(s) => Some(s.clone()),
            _ => None,
        })
        .collect()
}

fn word_texts(segments: &[Segment]) -> Vec<String> {
    segments
        .iter()
        .flat_map(|s| s.words.iter().map(|w| w.text.clone()))
        .collect()
}

fn partial_count(got: &Collected) -> usize {
    got.transcripts
        .iter()
        .filter(|(_, e)| matches!(e, TranscriptEvent::Partial(_)))
        .count()
}

/// Mic-only 16 kHz session (the mic passes through exactly, so the timeline fake finds
/// every buffer) with live ASR on the mic; `poll_every` packets between polls.
fn mic_session(
    audio: &[f32],
    words: &[Word],
    cfg: PipelineConfig,
    poll_every: usize,
) -> (Collected, usize) {
    let session = CaptureSession::new(ORIGIN, TrackInfo::new(R16), None);
    let mut p = Pipeline::new(session, cfg).unwrap();
    let (asr, calls) = live(
        FakeTranscriber::timeline(audio.to_vec(), words.to_vec()),
        entity(1),
    );
    p.attach_live_asr(TrackId::Mic, asr).unwrap();
    let mut got = Collected::default();
    for (i, chunk) in audio.chunks(160).enumerate() {
        p.frames(
            TrackId::Mic,
            chunk,
            ORIGIN.after_frames(i as u64 * 160, R16),
        );
        if (i + 1) % poll_every == 0 {
            got.absorb(p.poll());
        }
    }
    end(&mut p, ORIGIN.after_frames(audio.len() as u64, R16));
    got.absorb(p.poll());
    (got, calls.load(Ordering::SeqCst))
}

/// T9.1l: a host that starts live ASR after capture registers it late, and its last entry is what
/// the final pass reads; before input only the first form is for a pipeline-run track.
#[test]
fn pipeline_held_live_registers_after_input_and_its_last_entry_counts() {
    let session = CaptureSession::new(ORIGIN, TrackInfo::new(SampleRate::HZ_16000), None);
    let mut p = Pipeline::new(session, PipelineConfig::default()).unwrap();
    let entry = |n: u8, options: TranscribeOptions| FinalPassTrack {
        track: TrackId::Mic,
        track_entity: Some(entity(n)),
        options,
    };
    assert!(matches!(
        p.register_held_live(
            FinalPassTrack {
                track: TrackId::Reference,
                track_entity: None,
                options: TranscribeOptions::batch(),
            },
            Box::new(|| 0)
        ),
        Err(PipelineError::MissingTrack { .. })
    ));
    p.register_held_live(entry(1, TranscribeOptions::batch()), Box::new(|| 0))
        .unwrap();
    p.frames(TrackId::Mic, &vec![0.0; 1600], ORIGIN);
    // Too late for the other way, not for this one.
    assert!(
        p.register_external_live(entry(2, TranscribeOptions::batch()), Box::new(|| 0))
            .is_err()
    );
    p.register_held_live(entry(3, TranscribeOptions::live()), Box::new(|| 7))
        .unwrap();
    p.end(EndOfStream {
        at: ORIGIN.after_frames(1600, SampleRate::HZ_16000),
        reason: EndReason::Stopped,
    });
    let request = p
        .poll()
        .into_iter()
        .find_map(|e| match e {
            PipelineEvent::FinalPassRequested(r) => Some(r),
            _ => None,
        })
        .unwrap();
    assert_eq!(request.tracks, [entry(3, TranscribeOptions::live())]);
    assert!(
        p.register_held_live(entry(4, TranscribeOptions::batch()), Box::new(|| 0))
            .is_err(),
        "the request is made"
    );
}

#[test]
fn pipeline_live_asr_transcribes_the_mic_on_the_meeting_timeline() {
    let mut w = words("a", 1000, 5);
    w.extend(words("b", 5000, 4));
    let audio = speech_audio(&w, 8000, 30);
    let (got, calls) = mic_session(&audio, &w, PipelineConfig::default(), 1);
    let finals = final_segments(&got, TrackId::Mic);
    assert_eq!(finals.len(), 2);
    assert_eq!(
        word_texts(&finals),
        w.iter().map(|x| x.text.clone()).collect::<Vec<_>>()
    );
    assert!(finals.iter().all(|s| s.track == entity(1)));
    assert_eq!(finals[0].start, MeetingTime::from_millis(1000));
    assert_eq!(finals[1].end, MeetingTime::from_millis(6500));
    assert!(partial_count(&got) > 0 && calls > 2);
    let req = got.final_pass.clone().unwrap();
    assert_eq!(req.end, MeetingTime::from_millis(8000));
    assert_eq!(
        req.tracks,
        [FinalPassTrack {
            track: TrackId::Mic,
            track_entity: Some(entity(1)),
            options: TranscribeOptions::batch(),
        }]
    );
    assert!(got.errors.is_empty(), "{:?}", got.errors);
}

#[test]
fn pipeline_live_asr_transcribes_the_reference_as_them() {
    let w = words("t", 600, 6);
    let reference = speech_audio(&w, 4000, 31);
    let mic = noise(32, reference.len(), QUIET);
    let session = CaptureSession::new(ORIGIN, TrackInfo::new(R16), Some(TrackInfo::new(R16)));
    let mut p = Pipeline::new(session, exact_config()).unwrap();
    let (asr, _) = live(
        FakeTranscriber::timeline(reference.clone(), w.clone()),
        entity(2),
    );
    p.attach_live_asr(TrackId::Reference, asr).unwrap();
    let mut got = Collected::default();
    for i in 0..reference.len() / 160 {
        let ts = ORIGIN.after_frames(i as u64 * 160, R16);
        p.frames(TrackId::Mic, &mic[i * 160..(i + 1) * 160], ts);
        p.frames(TrackId::Reference, &reference[i * 160..(i + 1) * 160], ts);
        got.absorb(p.poll());
    }
    end(&mut p, ORIGIN.after_frames(reference.len() as u64, R16));
    got.absorb(p.poll());
    let finals = final_segments(&got, TrackId::Reference);
    assert_eq!(
        word_texts(&finals),
        w.iter().map(|x| x.text.clone()).collect::<Vec<_>>()
    );
    assert!(finals.iter().all(|s| s.track == entity(2)));
    assert!(final_segments(&got, TrackId::Mic).is_empty());
    let tracks = &got.final_pass.as_ref().unwrap().tracks;
    assert_eq!(tracks.len(), 2);
    assert_eq!(
        (tracks[0].track, tracks[0].track_entity),
        (TrackId::Mic, None)
    );
    assert_eq!(
        (tracks[1].track, tracks[1].track_entity),
        (TrackId::Reference, Some(entity(2)))
    );
}

#[test]
fn pipeline_partials_coalesce_when_the_host_falls_behind() {
    // 20 s of continuous speech (one max_window cut), then a second utterance.
    let mut w = words("a", 500, 50);
    w.extend(words("b", 23_000, 5));
    let audio = speech_audio(&w, 26_000, 33);
    // A host that keeps up polls every packet; a host whose decoder is stuck polls once.
    let (prompt, prompt_calls) = mic_session(&audio, &w, PipelineConfig::default(), 1);
    let (stuck, stuck_calls) = mic_session(&audio, &w, PipelineConfig::default(), usize::MAX);
    let spans = |g: &Collected| -> Vec<(Vec<String>, MeetingTime, MeetingTime)> {
        final_segments(g, TrackId::Mic)
            .iter()
            .map(|s| (word_texts(std::slice::from_ref(s)), s.start, s.end))
            .collect()
    };
    assert_eq!(
        spans(&prompt),
        spans(&stuck),
        "finals do not depend on the host's pace"
    );
    assert!(prompt_calls > 20, "{prompt_calls}");
    assert!(
        stuck_calls <= 4,
        "one cut, two closes, at most one coalesced partial: {stuck_calls}"
    );
    assert!(partial_count(&stuck) <= 1);

    let off = PipelineConfig {
        partials: PartialPolicy::Off,
        ..PipelineConfig::default()
    };
    let (quiet, quiet_calls) = mic_session(&audio, &w, off, 1);
    assert_eq!(partial_count(&quiet), 0);
    assert_eq!(quiet_calls, 3);
    assert_eq!(spans(&quiet), spans(&prompt));
}

#[test]
fn pipeline_end_finishes_open_segments_before_the_final_pass_request() {
    // Speech runs to the very end: only `end` can close the segment.
    let w = words("z", 500, 8);
    let audio = speech_audio(&w, 3600, 34);
    let (got, _) = mic_session(&audio, &w, PipelineConfig::default(), 1);
    let finals = final_segments(&got, TrackId::Mic);
    assert_eq!(word_texts(&finals).len(), 8);
    // Every partial id is closed (the collector already checked ordering: transcripts,
    // then the request, then Ended).
    for (_, e) in &got.transcripts {
        if let TranscriptEvent::Partial(p) = e {
            assert!(finals.iter().any(|f| f.id == p.segment.id));
        }
    }
}

#[test]
fn pipeline_live_asr_failure_stops_only_that_track() {
    let w = words("a", 500, 10);
    let audio = speech_audio(&w, 6000, 35);
    let session = CaptureSession::new(ORIGIN, TrackInfo::new(R16), None);
    let mut p = Pipeline::new(session, PipelineConfig::default()).unwrap();
    let (asr, calls) = live(
        FakeTranscriber::scripted([Err(AsrError::Backend("gpu lost".into()))]),
        entity(3),
    );
    p.attach_live_asr(TrackId::Mic, asr).unwrap();
    let mut got = Collected::default();
    for (i, chunk) in audio.chunks(160).enumerate() {
        p.frames(
            TrackId::Mic,
            chunk,
            ORIGIN.after_frames(i as u64 * 160, R16),
        );
        got.absorb(p.poll());
    }
    end(&mut p, ORIGIN.after_frames(audio.len() as u64, R16));
    got.absorb(p.poll());
    assert_eq!(
        got.errors,
        [PipelineError::LiveAsr {
            track: TrackId::Mic,
            error: AsrError::Backend("gpu lost".into()),
        }]
    );
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "no decode after the failure"
    );
    assert_eq!(got.mic, audio, "capture and recording continue");
    assert_eq!(got.last_stats().mic.asr_backlog, 0);
    let req = got.final_pass.unwrap();
    assert_eq!(
        req.tracks[0].track_entity,
        Some(entity(3)),
        "the final pass still runs"
    );
}

#[test]
fn pipeline_live_asr_can_be_cancelled_from_the_host() {
    let w = words("a", 500, 10);
    let audio = speech_audio(&w, 6000, 36);
    let session = CaptureSession::new(ORIGIN, TrackInfo::new(R16), None);
    let mut p = Pipeline::new(session, PipelineConfig::default()).unwrap();
    let (asr, calls) = live(
        FakeTranscriber::timeline(audio.clone(), w.clone()),
        entity(4),
    );
    p.attach_live_asr(TrackId::Mic, asr).unwrap();
    p.live_cancel_token(TrackId::Mic).unwrap().cancel();
    push_packets(&mut p, TrackId::Mic, &audio, 1, R16, ORIGIN, 160);
    end(&mut p, ORIGIN.after_frames(audio.len() as u64, R16));
    let mut got = Collected::default();
    got.absorb(p.poll());
    assert!(matches!(
        got.errors.as_slice(),
        [PipelineError::LiveAsr {
            track: TrackId::Mic,
            error: AsrError::Cancelled
        }]
    ));
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert!(p.live_cancel_token(TrackId::Mic).is_none());
}

#[test]
fn pipeline_live_asr_attachment_is_checked() {
    let mic_only = || {
        Pipeline::new(
            CaptureSession::new(ORIGIN, TrackInfo::new(R16), None),
            PipelineConfig::default(),
        )
        .unwrap()
    };
    let fake = || FakeTranscriber::scripted([]);
    let mut p = mic_only();
    assert_eq!(
        p.attach_live_asr(TrackId::Reference, live(fake(), entity(5)).0),
        Err(PipelineError::MissingTrack {
            track: TrackId::Reference
        })
    );
    let (mut bad, _) = live(fake(), entity(5));
    bad.config.partial_interval = Duration::ZERO;
    assert!(matches!(
        p.attach_live_asr(TrackId::Mic, bad),
        Err(PipelineError::LiveAsr {
            track: TrackId::Mic,
            error: AsrError::Config(_)
        })
    ));
    p.attach_live_asr(TrackId::Mic, live(fake(), entity(5)).0)
        .unwrap();
    assert!(matches!(
        p.attach_live_asr(TrackId::Mic, live(fake(), entity(5)).0),
        Err(PipelineError::InvalidConfig(_))
    ));
    let mut late = mic_only();
    late.frames(TrackId::Mic, &[0.0; 160], ORIGIN);
    assert!(matches!(
        late.attach_live_asr(TrackId::Mic, live(fake(), entity(5)).0),
        Err(PipelineError::InvalidConfig(_))
    ));
}

#[test]
fn pipeline_external_live_track_reports_the_hosts_backlog_and_final_pass_entry() {
    // A host running the mic's LiveTrack on its own thread registers it: the pipeline's
    // stats carry the host's backlog and the final-pass request the host's entry.
    let session = CaptureSession::new(ORIGIN, TrackInfo::new(R16), None);
    let mut p = Pipeline::new(session, PipelineConfig::default()).unwrap();
    let (asr, _) = live(FakeTranscriber::scripted([]), entity(9));
    let host =
        LiveTrack::with_stretches(TrackId::Mic, asr, &crate::asr::PieceConfig::default()).unwrap();
    let entry = host.final_pass();
    // The host's entry is not the pipeline's default for a track without live ASR.
    assert_eq!(entry.track_entity, Some(entity(9)));
    assert_ne!(entry.options, TranscribeOptions::batch());
    let queued = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let reported = Arc::clone(&queued);
    p.register_external_live(
        entry.clone(),
        Box::new(move || reported.load(Ordering::SeqCst)),
    )
    .unwrap();
    let mut got = Collected::default();
    for i in 0..300u64 {
        queued.store(1_000 + i, Ordering::SeqCst);
        p.frames(TrackId::Mic, &[0.0; 160], ORIGIN.after_frames(i * 160, R16));
        got.absorb(p.poll());
    }
    let backlogs: Vec<u64> = got.stats.iter().map(|s| s.mic.asr_backlog).collect();
    assert!(!backlogs.is_empty());
    assert!(
        backlogs.iter().all(|&b| (1_000..1_300).contains(&b)),
        "{backlogs:?}"
    );
    end(&mut p, ORIGIN.after_frames(300 * 160, R16));
    got.absorb(p.poll());
    assert_eq!(got.final_pass.unwrap().tracks, vec![entry]);
    // No live ASR runs inside the pipeline for it.
    assert!(p.live_cancel_token(TrackId::Mic).is_none());
    assert!(got.transcripts.is_empty());
}

#[test]
fn pipeline_external_live_registration_is_checked() {
    let mic_only = || {
        Pipeline::new(
            CaptureSession::new(ORIGIN, TrackInfo::new(R16), None),
            PipelineConfig::default(),
        )
        .unwrap()
    };
    let entry = |track| FinalPassTrack {
        track,
        track_entity: Some(entity(5)),
        options: TranscribeOptions::batch(),
    };
    let none = || -> Box<dyn Fn() -> u64 + Send> { Box::new(|| 0) };
    let mut p = mic_only();
    assert_eq!(
        p.register_external_live(entry(TrackId::Reference), none()),
        Err(PipelineError::MissingTrack {
            track: TrackId::Reference
        })
    );
    p.register_external_live(entry(TrackId::Mic), none())
        .unwrap();
    // Neither a second registration nor live ASR inside the pipeline on the same track.
    assert!(matches!(
        p.register_external_live(entry(TrackId::Mic), none()),
        Err(PipelineError::InvalidConfig(_))
    ));
    assert!(matches!(
        p.attach_live_asr(
            TrackId::Mic,
            live(FakeTranscriber::scripted([]), entity(5)).0
        ),
        Err(PipelineError::InvalidConfig(_))
    ));
    let mut attached = mic_only();
    attached
        .attach_live_asr(
            TrackId::Mic,
            live(FakeTranscriber::scripted([]), entity(5)).0,
        )
        .unwrap();
    assert!(matches!(
        attached.register_external_live(entry(TrackId::Mic), none()),
        Err(PipelineError::InvalidConfig(_))
    ));
    let mut late = mic_only();
    late.frames(TrackId::Mic, &[0.0; 160], ORIGIN);
    assert!(matches!(
        late.register_external_live(entry(TrackId::Mic), none()),
        Err(PipelineError::InvalidConfig(_))
    ));
}

#[test]
fn pipeline_snapshots_stay_bounded_when_the_host_does_not_poll() {
    // 60 s without a poll: 600 meter intervals and 60 stats intervals were produced.
    let session = CaptureSession::new(ORIGIN, TrackInfo::new(R16), None);
    let mut p = Pipeline::new(session, PipelineConfig::default()).unwrap();
    let audio = noise(46, 16_000 * 60, 0.1);
    push_packets(&mut p, TrackId::Mic, &audio, 1, R16, ORIGIN, 160);
    let mut got = Collected::default();
    got.absorb(p.poll());
    assert_eq!(got.mic, audio, "audio is never dropped");
    assert_eq!(got.levels.len(), MAX_PENDING_LEVELS);
    assert_eq!(got.stats.len(), MAX_PENDING_STATS);
    assert_eq!(
        got.levels.last().unwrap().end,
        MeetingTime::from_millis(60_000)
    );
    assert_eq!(got.last_stats().end, MeetingTime::from_millis(60_000));
    // The most recent ones, contiguous.
    assert_eq!(
        got.levels[0].end,
        MeetingTime::from_millis(60_000 - 63 * 100)
    );
    assert_eq!(
        got.stats[0].end,
        MeetingTime::from_millis(60_000 - 15 * 1000)
    );
}

#[test]
fn pipeline_live_asr_that_falls_too_far_behind_stops_and_leaves_the_final_pass() {
    // A host that never polls (or a decoder far slower than real time): once more than
    // max_asr_backlog of audio waits, that track's live ASR stops; recording continues.
    let w = words("a", 500, 10);
    let audio = speech_audio(&w, 6000, 37);
    let cfg = PipelineConfig {
        max_asr_backlog: Duration::from_secs(2),
        ..PipelineConfig::default()
    };
    let session = CaptureSession::new(ORIGIN, TrackInfo::new(R16), None);
    let mut p = Pipeline::new(session, cfg).unwrap();
    let (asr, calls) = live(
        FakeTranscriber::timeline(audio.clone(), w.clone()),
        entity(6),
    );
    p.attach_live_asr(TrackId::Mic, asr).unwrap();
    push_packets(&mut p, TrackId::Mic, &audio, 1, R16, ORIGIN, 160);
    end(&mut p, ORIGIN.after_frames(audio.len() as u64, R16));
    let mut got = Collected::default();
    got.absorb(p.poll());
    assert!(
        matches!(
            got.errors.as_slice(),
            [PipelineError::LiveAsr { track: TrackId::Mic, error: AsrError::Backend(m) }]
                if m.contains("fell behind")
        ),
        "{:?}",
        got.errors
    );
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert!(got.transcripts.is_empty());
    assert_eq!(got.mic, audio);
    assert_eq!(got.last_stats().mic.asr_backlog, 0);
    let req = got.final_pass.unwrap();
    assert_eq!(req.tracks[0].track_entity, Some(entity(6)));

    // Exactly at the cap is still live.
    let cfg = PipelineConfig {
        max_asr_backlog: Duration::from_millis(6000),
        ..PipelineConfig::default()
    };
    let session = CaptureSession::new(ORIGIN, TrackInfo::new(R16), None);
    let mut p = Pipeline::new(session, cfg).unwrap();
    let (asr, _) = live(
        FakeTranscriber::timeline(audio.clone(), w.clone()),
        entity(6),
    );
    p.attach_live_asr(TrackId::Mic, asr).unwrap();
    push_packets(&mut p, TrackId::Mic, &audio, 1, R16, ORIGIN, 160);
    let mut got = Collected::default();
    got.absorb(p.poll());
    assert!(got.errors.is_empty(), "{:?}", got.errors);
}

#[test]
fn pipeline_final_pass_uses_the_live_language() {
    let session = CaptureSession::new(ORIGIN, TrackInfo::new(R16), None);
    let mut p = Pipeline::new(session, PipelineConfig::default()).unwrap();
    let (mut asr, _) = live(FakeTranscriber::scripted([]), entity(7));
    asr.config.options.language = Language::Code("de".into());
    p.attach_live_asr(TrackId::Mic, asr).unwrap();
    end(&mut p, ORIGIN);
    let mut got = Collected::default();
    got.absorb(p.poll());
    let track = &got.final_pass.unwrap().tracks[0];
    assert_eq!(track.options.language, Language::Code("de".into()));
    assert_eq!(track.options.decoding, TranscribeOptions::batch().decoding);
    assert_ne!(
        TranscribeOptions::batch().language,
        Language::Code("de".into())
    );
}

#[test]
fn pipeline_live_mic_words_are_on_the_meeting_timeline_with_a_reference() {
    // A silent reference: no echo path, so the canceller passes the mic through, delayed
    // by its latency, which the pipeline removes. Word times from the mic's live ASR
    // must therefore be exactly the mic-only ones (pins the pre-roll alignment).
    let mut w = words("a", 1000, 5);
    w.extend(words("b", 5000, 4));
    let audio = speech_audio(&w, 8000, 38);
    let session = CaptureSession::new(ORIGIN, TrackInfo::new(R16), Some(TrackInfo::new(R16)));
    let mut p = Pipeline::new(session, exact_config()).unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let (mut asr, _) = live(FakeTranscriber::scripted([]), entity(8));
    asr.transcriber = Box::new(Locating {
        audio: audio.clone(),
        inner: Counting {
            inner: FakeTranscriber::timeline(audio.clone(), w.clone()),
            calls: Arc::clone(&calls),
        },
    });
    p.attach_live_asr(TrackId::Mic, asr).unwrap();
    let silent = vec![0.0; 160];
    let mut got = Collected::default();
    for (i, chunk) in audio.chunks(160).enumerate() {
        let ts = ORIGIN.after_frames(i as u64 * 160, R16);
        p.frames(TrackId::Mic, chunk, ts);
        p.frames(TrackId::Reference, &silent, ts);
        got.absorb(p.poll());
    }
    end(&mut p, ORIGIN.after_frames(audio.len() as u64, R16));
    got.absorb(p.poll());
    assert_eq!(got.mic.len(), audio.len());
    // The passed-through mic is the canceller's input: after its audio high-pass when one
    // is configured (T6.23b: 30 Hz by default).
    let input = match exact_config().aec.highpass {
        crate::aec::HighPass::Audio { cutoff_hz } => {
            crate::aec::HighPassFilter::new(cutoff_hz, 16_000).filtered(&audio)
        }
        _ => audio.clone(),
    };
    let worst = got
        .mic
        .iter()
        .zip(&input)
        .map(|(a, b)| (a - b).abs())
        .fold(0.0f32, f32::max);
    assert!(worst < 1e-3, "bypassed canceller output differs by {worst}");
    assert!(got.errors.is_empty(), "{:?}", got.errors);
    let finals = final_segments(&got, TrackId::Mic);
    let times: Vec<(String, u64, u64)> = finals
        .iter()
        .flat_map(|s| &s.words)
        .map(|x| (x.text.clone(), x.start.as_millis(), x.end.as_millis()))
        .collect();
    let want: Vec<(String, u64, u64)> = w
        .iter()
        .map(|x| (x.text.clone(), x.start.as_millis(), x.end.as_millis()))
        .collect();
    assert_eq!(times, want);
    assert!(calls.load(Ordering::SeqCst) > 2);
}

/// Finds where a (nearly equal) buffer sits in `audio` by least absolute difference over
/// its first 64 samples, then transcribes the exact slice with `inner`. For streams that
/// went through the canceller in bypass (equal to the mic up to rounding): a misaligned
/// stream would be located elsewhere and shift every word time.
struct Locating {
    audio: Vec<f32>,
    inner: Counting,
}

impl Transcriber for Locating {
    fn transcribe(
        &mut self,
        buf: &MonoBuffer,
        options: &TranscribeOptions,
        cancel: &CancelToken,
    ) -> Result<Transcription, AsrError> {
        let n = buf.samples.len().min(64);
        let last = self.audio.len() - buf.samples.len();
        let at = (0..=last)
            .min_by(|&a, &b| {
                let cost = |o: usize| -> f32 {
                    (0..n)
                        .map(|i| (self.audio[o + i] - buf.samples[i]).abs())
                        .sum()
                };
                cost(a).total_cmp(&cost(b))
            })
            .unwrap();
        let exact = MonoBuffer::new(buf.rate, self.audio[at..at + buf.samples.len()].to_vec());
        self.inner.transcribe(&exact, options, cancel)
    }
    fn backend_info(&self) -> BackendInfo {
        self.inner.backend_info()
    }
}

#[test]
fn pipeline_loopback_timestamp_wander_is_not_steered_into_the_reference() {
    // T6.17: on the desktop, WASAPI loopback timestamps wander on a cycle of seconds
    // (seen on real recordings) while
    // mic and loopback share one device clock, so the true drift is 0. Steering must not
    // follow the wander: a correction that does moves the reference audio by milliseconds
    // every few seconds, and the canceller has to chase it.
    let secs = 240u64;
    let session = CaptureSession::new(ORIGIN, TrackInfo::new(R16), Some(TrackInfo::new(R48)));
    let mut p = Pipeline::new(session, PipelineConfig::default()).unwrap();
    let mut got = Collected::default();
    let mic = noise(29, 160, 0.1);
    let reference = noise(30, 480, 0.1);
    let mut worst_after_settling: f64 = 0.0;
    for i in 0..secs * 100 {
        p.frames(TrackId::Mic, &mic, ORIGIN.after_frames(i * 160, R16));
        let t = i as f64 / 100.0;
        let wander_ticks = 4e-3 * (2.0 * std::f64::consts::PI * t / 10.0).sin() * 1e7;
        let ticks = (i as f64 * 100_000.0 + wander_ticks).round() as u64;
        p.frames(
            TrackId::Reference,
            &reference,
            DeviceTime::from_ticks(ORIGIN.ticks() + ticks),
        );
        got.absorb(p.poll());
        if t >= 60.0
            && let Some(s) = got.stats.last()
        {
            worst_after_settling = worst_after_settling.max(s.reference_correction_ppm.abs());
        }
    }
    assert!(
        worst_after_settling < 25.0,
        "steering followed the timestamp wander: up to {worst_after_settling:.0} ppm"
    );
    let r = got.last_stats().reference.unwrap();
    assert_eq!((r.zero_filled, r.late_dropped), (0, 0));
    assert!(got.errors.is_empty(), "{:?}", got.errors);
}

// --- Speech stretches keep each piece as it is decoded (T6.13c) ---

/// Answers like `inner` but fails (or panics) on call `on` (1-based).
struct FailOn {
    inner: FakeTranscriber,
    on: usize,
    panic: bool,
    calls: usize,
}

impl Transcriber for FailOn {
    fn transcribe(
        &mut self,
        audio: &MonoBuffer,
        options: &TranscribeOptions,
        cancel: &CancelToken,
    ) -> Result<Transcription, AsrError> {
        self.calls += 1;
        if self.calls == self.on {
            assert!(!self.panic, "simulated transcriber crash");
            return Err(AsrError::Backend("gpu lost".into()));
        }
        self.inner.transcribe(audio, options, cancel)
    }
    fn backend_info(&self) -> BackendInfo {
        self.inner.backend_info()
    }
}

/// A stretch track over two turns (two pieces) whose transcriber fails or panics on the
/// second piece, given all its audio at once (so both pieces are decoded in one core
/// call); the run's outcome (Err for a panic), its events and the track.
fn stretch_run_failing_second(
    panic: bool,
) -> (std::thread::Result<()>, Vec<PipelineEvent>, LiveTrack) {
    let mut w = words("a", 1000, 5);
    w.extend(words("b", 9000, 5));
    let audio = speech_audio(&w, 20_000, 41);
    let (mut asr, _) = live(
        FakeTranscriber::timeline(audio.clone(), w.clone()),
        entity(7),
    );
    asr.transcriber = Box::new(FailOn {
        inner: FakeTranscriber::timeline(audio.clone(), w),
        on: 2,
        panic,
        calls: 0,
    });
    let mut track =
        LiveTrack::with_stretches(TrackId::Mic, asr, &crate::asr::PieceConfig::default()).unwrap();
    let mut events = Vec::new();
    track.feed(&audio, u64::MAX, &mut events);
    let ran = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        track.run(PartialPolicy::All, &mut events);
    }));
    (ran, events, track)
}

fn final_texts(events: &[PipelineEvent]) -> Vec<String> {
    events
        .iter()
        .filter_map(|e| match e {
            PipelineEvent::Transcript {
                event: TranscriptEvent::Final(s),
                ..
            } => Some(s.words.iter().map(|w| w.text.clone()).collect::<Vec<_>>()),
            _ => None,
        })
        .flatten()
        .collect()
}

#[test]
fn pipeline_live_stretch_keeps_the_pieces_before_a_failed_decode_of_the_same_call() {
    for panic in [false, true] {
        let (ran, events, mut track) = stretch_run_failing_second(panic);
        assert_eq!(ran.is_err(), panic);
        // The first piece was decoded in the same core call as the failure: it is kept,
        // shown, and covered by the frontier, which stops before the second turn.
        let pieces = track.take_decoded_pieces();
        assert_eq!(pieces.len(), 1, "panic {panic}: {pieces:?}");
        let through = track.stretches_through().unwrap();
        assert!(through >= pieces[0].span.end as u64, "{through}");
        assert!(through <= 9 * 16_000, "{through}");
        assert_eq!(final_texts(&events), ["a0", "a1", "a2", "a3", "a4"]);
        let stopped = events.iter().any(|e| {
            matches!(
                e,
                PipelineEvent::Error(PipelineError::LiveAsr {
                    track: TrackId::Mic,
                    ..
                })
            )
        });
        // A failure stops the track; a panic leaves that to the host that caught it.
        assert_eq!(stopped, !panic, "{events:?}");
    }
}

#[test]
fn pipeline_a_note_changes_nothing_about_the_audio() {
    let run = |noted: bool| {
        let session = CaptureSession::new(ORIGIN, TrackInfo::new(R16), Some(TrackInfo::new(R16)));
        let mut p = Pipeline::new(session, PipelineConfig::default()).unwrap();
        let mut got = Collected::default();
        for k in 0..20u64 {
            let at = ORIGIN.after_frames(k * 1600, R16);
            if noted && k == 10 {
                p.discontinuity(
                    TrackId::Reference,
                    Discontinuity {
                        at,
                        kind: DiscontinuityKind::Note {
                            warning: crate::capture::EndpointWarning::CallOutputRefused {
                                wanted: "Headset".into(),
                                used: None,
                            },
                        },
                    },
                );
            }
            p.frames(TrackId::Mic, &noise(18, 1600, 0.2), at);
            p.frames(TrackId::Reference, &noise(19, 1600, 0.2), at);
            got.absorb(p.poll());
        }
        end(&mut p, ORIGIN.after_frames(20 * 1600, R16));
        got.absorb(p.poll());
        let restarts = got.last_stats().reference.as_ref().map(|r| r.restarts);
        (got.mic, got.them, restarts)
    };
    assert_eq!(run(true), run(false));
}
