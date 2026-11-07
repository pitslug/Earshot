//! Tests of the two halves of the diarisation pipeline (T6.13d): the incremental half
//! ([`DiarStream`], [`Diarizer::advance`]) and the final half ([`Diarizer::diarize_from`]).
//! The standard is equality with [`Diarizer::diarize`] on the same audio. Every test path
//! contains `diar`, so `cargo test -p earshot-core diar` selects them.

use std::time::Duration;

use super::*;
use crate::asr::{CancelToken, MeetingTime};
use crate::audio::{MonoBuffer, Sample};

/// xorshift64* in [-1, 1).
struct Noise(u64);

impl Noise {
    fn next(&mut self) -> f32 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        let x = self.0.wrapping_mul(0x2545_F491_4F6C_DD1D);
        ((x >> 40) as f32 / (1u64 << 24) as f32) * 2.0 - 1.0
    }
}

const DIM: usize = 32;

fn centroids() -> Vec<Vec<f32>> {
    let mut n = Noise(99);
    (0..MAX_SCENE_SPEAKERS)
        .map(|_| {
            let v: Vec<f32> = (0..DIM).map(|_| n.next()).collect();
            let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
            v.into_iter().map(|x| x / norm).collect()
        })
        .collect()
}

/// Fakes whose output depends on the order of calls (a running noise generator, jitter
/// and slot rotation by window count): equal results mean the same calls in the same order.
fn stateful(config: DiarizationConfig) -> Diarizer<FakeSegmenter, FakeEmbedder> {
    Diarizer::new(
        FakeSegmenter::segmentation_3_0()
            .with_boundary_jitter(Duration::from_millis(120))
            .with_rotation(1),
        FakeEmbedder::new("fake-emb@1", centroids(), 0.05, 7),
        config,
    )
    .unwrap()
}

/// Fakes whose output is a function of the input alone, like real models: a fresh
/// instance can take over from another.
fn stateless(config: DiarizationConfig) -> Diarizer<FakeSegmenter, FakeEmbedder> {
    Diarizer::new(
        FakeSegmenter::segmentation_3_0()
            .with_boundary_jitter(Duration::from_millis(120))
            .with_rotation(1)
            .content_seeded(),
        FakeEmbedder::new("fake-emb@1", centroids(), 0.05, 7).content_seeded(),
        config,
    )
    .unwrap()
}

/// Round-robin scene of `n` speakers with a few overlaps, about `rounds * n * 5` s long.
fn scene(n: usize, rounds: usize) -> (Vec<SceneTurn>, u64) {
    let mut t = 700;
    let mut turns = Vec::new();
    for r in 0..rounds {
        for k in 0..n {
            let len = 3000 + 700 * ((r + k) as u64 % 4);
            turns.push(SceneTurn::new(k, t, t + len));
            // Every third turn is overlapped by the next speaker's first second.
            t = if (r + k) % 3 == 0 {
                t + len - 900
            } else {
                t + len + 450
            };
        }
    }
    (turns, t + 1300)
}

/// Feeds `samples` to a stream in pieces of `push` samples, advancing after each.
fn feed(
    dz: &mut Diarizer<FakeSegmenter, FakeEmbedder>,
    stream: &mut DiarStream,
    samples: &[Sample],
    push: usize,
) {
    for piece in samples.chunks(push.max(1)) {
        stream.append(piece);
        dz.advance(stream, usize::MAX, &CancelToken::new()).unwrap();
    }
}

fn configs() -> Vec<(&'static str, DiarizationConfig)> {
    vec![
        ("default", DiarizationConfig::default()),
        (
            "batch 5",
            DiarizationConfig {
                batch_size: 5,
                ..DiarizationConfig::default()
            },
        ),
        (
            "batch 1, overlap kept, constrained",
            DiarizationConfig {
                batch_size: 1,
                exclude_overlap: false,
                constrained_assignment: true,
                ..DiarizationConfig::default()
            },
        ),
        (
            "step 2.5 s, batch 3",
            DiarizationConfig {
                window_step: Duration::from_millis(2500),
                batch_size: 3,
                ..DiarizationConfig::default()
            },
        ),
    ]
}

#[test]
fn diar_stream_then_final_half_equals_the_whole_track_with_the_same_calls() {
    let (turns, total) = scene(4, 7);
    let audio = scene_audio(&turns, total);
    assert!(
        audio.samples.len() > 100 * 16_000,
        "the scene spans several batches"
    );
    let origin = MeetingTime::from_millis(1234);
    for (name, config) in configs() {
        let mut whole = stateful(config.clone());
        let expected = whole.diarize(&audio, origin, &CancelToken::new()).unwrap();
        assert!(expected.speakers.len() >= 3, "{name}: {expected:?}");
        let (whole_seg, whole_emb) = whole.into_parts();
        for push in [1usize, 4999, 160_000, audio.samples.len()] {
            if push == 1 && config.batch_size != 5 {
                continue; // one-sample pushes are slow; one config is enough
            }
            let ctx = format!("{name}, push {push}");
            let mut dz = stateful(config.clone());
            let mut stream = dz.stream();
            feed(&mut dz, &mut stream, &audio.samples, push);
            // The incremental half did real work: every whole batch of windows whose
            // audio is known, which for this scene is most of them.
            let done = stream.windows_done();
            assert!(
                done > 0 && done.is_multiple_of(config.batch_size),
                "{ctx}: {done}"
            );
            assert_eq!(stream.received(), audio.samples.len(), "{ctx}");
            let got = dz
                .diarize_from(stream.into_results(), &audio, origin, &CancelToken::new())
                .unwrap();
            assert_eq!(got, expected, "{ctx}");
            let (seg, emb) = dz.into_parts();
            assert_eq!(seg.windows_seen(), whole_seg.windows_seen(), "{ctx}");
            assert_eq!(emb.regions(), whole_emb.regions(), "{ctx}");
        }
    }
}

#[test]
fn diar_stream_stopped_anywhere_resumes_on_a_fresh_diarizer() {
    let (turns, total) = scene(3, 8);
    let audio = scene_audio(&turns, total);
    let n = audio.samples.len();
    // The grid and batching variants (the other settings do not touch resuming).
    for (name, config) in configs()
        .into_iter()
        .filter(|(name, _)| !name.contains("overlap"))
    {
        let expected = stateless(config.clone())
            .diarize(&audio, MeetingTime::ZERO, &CancelToken::new())
            .unwrap();
        // Stop points: nothing, around the end of the first window and of the first
        // default batch (41 s), odd points in the middle, the edges of the end.
        let cuts = [
            0,
            159_999,
            160_000,
            656_000,
            656_001,
            n / 2 + 12_345,
            n - 7 * 16_000 + 3,
            n - 1,
            n,
        ];
        let mut resumed_with_work = 0;
        for cut in cuts {
            let ctx = format!("{name}, stopped at {cut}");
            let mut live = stateless(config.clone());
            let mut stream = live.stream();
            feed(&mut live, &mut stream, &audio.samples[..cut], 7_777);
            if stream.windows_done() > 0 {
                resumed_with_work += 1;
            }
            // Through the stored form, as the runtime keeps it between the meeting and
            // the final pass.
            let bytes = stream.into_results().to_bytes();
            let results = WindowResults::from_bytes(&bytes).unwrap();
            let got = stateless(config.clone())
                .diarize_from(results, &audio, MeetingTime::ZERO, &CancelToken::new())
                .unwrap();
            assert_eq!(got, expected, "{ctx}");
        }
        assert!(resumed_with_work >= 5, "{name}: {resumed_with_work}");
    }
}

#[test]
fn diar_stream_segments_a_batch_only_once_all_its_audio_is_known() {
    let config = DiarizationConfig {
        batch_size: 4,
        ..DiarizationConfig::default()
    };
    let (turns, total) = scene(3, 6);
    let audio = scene_audio(&turns, total);
    let mut dz = stateful(config);
    let mut stream = dz.stream();
    // Windows 0..4 start at 0, 1, 2, 3 s and the last ends at 13 s.
    let batch_end = 13 * 16_000;
    stream.append(&audio.samples[..batch_end - 1]);
    assert_eq!(stream.ready_batches(), 0);
    assert_eq!(
        dz.advance(&mut stream, usize::MAX, &CancelToken::new()),
        Ok(0)
    );
    assert_eq!(dz.into_parts().0.windows_seen(), 0);

    let mut dz = stateful(DiarizationConfig {
        batch_size: 4,
        ..DiarizationConfig::default()
    });
    let mut stream = dz.stream();
    stream.append(&audio.samples[..batch_end]);
    assert_eq!(stream.ready_batches(), 1);
    // max_batches bounds the work of one call.
    assert_eq!(dz.advance(&mut stream, 0, &CancelToken::new()), Ok(0));
    assert_eq!(dz.advance(&mut stream, 1, &CancelToken::new()), Ok(4));
    assert_eq!(stream.windows_done(), 4);
    stream.append(&audio.samples[batch_end..batch_end + 3 * 16_000]);
    assert_eq!(stream.ready_batches(), 0);
    stream.append(&audio.samples[batch_end + 3 * 16_000..batch_end + 4 * 16_000]);
    assert_eq!(stream.ready_batches(), 1);
    // Audio no unfinished window needs is not kept: less than the span of the next batch
    // (13 s) plus the earlier batch whose locals may still wait to be embedded (4 s).
    stream.append(&audio.samples[batch_end + 4 * 16_000..60 * 16_000]);
    dz.advance(&mut stream, usize::MAX, &CancelToken::new())
        .unwrap();
    assert_eq!(stream.windows_done(), 48);
    assert!(
        stream.buffered() < 17 * 16_000,
        "buffered {}",
        stream.buffered()
    );
}

#[test]
fn diar_stream_short_and_empty_tracks_are_left_to_the_final_half() {
    let config = DiarizationConfig::default();
    for total in [0u64, 4000, 10_000, 30_000] {
        let turns = [SceneTurn::new(0, 500, 3500), SceneTurn::new(1, 3900, 9000)];
        let audio = scene_audio(&turns, total);
        let expected = stateful(config.clone())
            .diarize(&audio, MeetingTime::ZERO, &CancelToken::new())
            .unwrap();
        let mut dz = stateful(config.clone());
        let mut stream = dz.stream();
        feed(&mut dz, &mut stream, &audio.samples, 16_000);
        assert_eq!(stream.windows_done(), 0, "total {total}");
        let got = dz
            .diarize_from(
                stream.into_results(),
                &audio,
                MeetingTime::ZERO,
                &CancelToken::new(),
            )
            .unwrap();
        assert_eq!(got, expected, "total {total}");
    }
}

/// An embedder that fails while `fail` is set.
struct Flaky {
    inner: FakeEmbedder,
    fail: bool,
}

impl SpeakerEmbedder for Flaky {
    fn info(&self) -> &EmbedderInfo {
        self.inner.info()
    }

    fn embed(
        &mut self,
        speech: &[&[Sample]],
        cancel: &CancelToken,
    ) -> Result<Vec<Embedding>, DiarError> {
        if self.fail {
            return Err(DiarError::Backend("flaky".to_owned()));
        }
        self.inner.embed(speech, cancel)
    }
}

#[test]
fn diar_stream_failed_batches_leave_the_results_as_they_were() {
    let config = DiarizationConfig {
        batch_size: 4,
        ..DiarizationConfig::default()
    };
    let (turns, total) = scene(3, 4);
    let audio = scene_audio(&turns, total);
    let expected = stateless(config.clone())
        .diarize(&audio, MeetingTime::ZERO, &CancelToken::new())
        .unwrap();
    let mut dz = Diarizer::new(
        FakeSegmenter::segmentation_3_0()
            .with_boundary_jitter(Duration::from_millis(120))
            .with_rotation(1)
            .content_seeded(),
        Flaky {
            inner: FakeEmbedder::new("fake-emb@1", centroids(), 0.05, 7).content_seeded(),
            fail: false,
        },
        config.clone(),
    )
    .unwrap();
    let mut stream = dz.stream();
    stream.append(&audio.samples[..30 * 16_000]);
    assert!(dz.advance(&mut stream, 2, &CancelToken::new()).unwrap() > 0);
    let before = stream.results().clone();
    let received = stream.received();
    stream.append(&audio.samples[30 * 16_000..]);
    // A model failure and a cancellation in the middle of the work: nothing is kept of
    // the failed batch (some of its locals were queued, some embedded).
    dz.embedder_mut().fail = true;
    assert_eq!(
        dz.advance(&mut stream, usize::MAX, &CancelToken::new()),
        Err(DiarError::Backend("flaky".to_owned()))
    );
    assert_eq!(stream.results(), &before);
    dz.embedder_mut().fail = false;
    let cancel = CancelToken::new();
    cancel.cancel();
    assert_eq!(
        dz.advance(&mut stream, usize::MAX, &cancel),
        Err(DiarError::Cancelled)
    );
    assert_eq!(stream.results(), &before);
    assert!(stream.received() > received);
    // The stream carries on from where it was, and the final half gives the whole-track
    // answer.
    dz.advance(&mut stream, usize::MAX, &CancelToken::new())
        .unwrap();
    assert!(stream.windows_done() > before.windows());
    let got = stateless(config)
        .diarize_from(
            stream.into_results(),
            &audio,
            MeetingTime::ZERO,
            &CancelToken::new(),
        )
        .unwrap();
    assert_eq!(got, expected);
}

#[test]
fn diar_window_results_are_refused_for_other_settings_or_audio() {
    let (turns, total) = scene(2, 4);
    let audio = scene_audio(&turns, total);
    let config = DiarizationConfig {
        batch_size: 4,
        ..DiarizationConfig::default()
    };
    let mut dz = stateless(config.clone());
    let mut stream = dz.stream();
    feed(&mut dz, &mut stream, &audio.samples, 16_000);
    let results = stream.into_results();
    assert!(results.windows() > 4);
    assert_eq!(results.params(), &dz.window_params());

    // Another setting that changes the per-window work.
    for other in [
        DiarizationConfig {
            batch_size: 5,
            ..config.clone()
        },
        DiarizationConfig {
            onset: 0.4,
            ..config.clone()
        },
        DiarizationConfig {
            window_step: Duration::from_millis(1500),
            ..config.clone()
        },
        DiarizationConfig {
            exclude_overlap: false,
            ..config.clone()
        },
    ] {
        let mut dz = stateless(other.clone());
        assert_ne!(results.params(), &dz.window_params(), "{other:?}");
        assert!(
            matches!(
                dz.diarize_from(
                    results.clone(),
                    &audio,
                    MeetingTime::ZERO,
                    &CancelToken::new()
                ),
                Err(DiarError::State(_))
            ),
            "{other:?}"
        );
    }
    // Another embedding model or segmenter.
    let mut other_emb = Diarizer::new(
        FakeSegmenter::segmentation_3_0().content_seeded(),
        FakeEmbedder::new("fake-emb@2", centroids(), 0.05, 7).content_seeded(),
        config.clone(),
    )
    .unwrap();
    assert!(matches!(
        other_emb.diarize_from(
            results.clone(),
            &audio,
            MeetingTime::ZERO,
            &CancelToken::new()
        ),
        Err(DiarError::State(_))
    ));
    // Settings that only the final half uses do not matter.
    let mut later = stateless(DiarizationConfig {
        constrained_assignment: true,
        merge_gap: Duration::from_millis(300),
        ..config.clone()
    });
    assert_eq!(results.params(), &later.window_params());
    later
        .diarize_from(
            results.clone(),
            &audio,
            MeetingTime::ZERO,
            &CancelToken::new(),
        )
        .unwrap();

    // A track shorter than the audio the windows were computed from.
    let short = MonoBuffer::new(
        audio.rate,
        audio.samples[..(results.windows() - 1) * 16_000 + 159_999].to_vec(),
    );
    assert!(matches!(
        stateless(config.clone()).diarize_from(
            results.clone(),
            &short,
            MeetingTime::ZERO,
            &CancelToken::new()
        ),
        Err(DiarError::State(_))
    ));

    // The stored form round-trips exactly and refuses damaged bytes.
    let bytes = results.to_bytes();
    assert_eq!(WindowResults::from_bytes(&bytes).unwrap(), results);
    for cut in [0, 3, 8, bytes.len() / 2, bytes.len() - 1] {
        assert!(
            matches!(
                WindowResults::from_bytes(&bytes[..cut]),
                Err(DiarError::State(_))
            ),
            "cut {cut}"
        );
    }
    let mut longer = bytes.clone();
    longer.push(0);
    assert!(matches!(
        WindowResults::from_bytes(&longer),
        Err(DiarError::State(_))
    ));
    let mut other_magic = bytes;
    other_magic[0] ^= 1;
    assert!(matches!(
        WindowResults::from_bytes(&other_magic),
        Err(DiarError::State(_))
    ));
}

#[test]
fn diar_stream_needs_the_settings_it_was_made_with() {
    let a = stateless(DiarizationConfig::default());
    let mut b = stateless(DiarizationConfig {
        onset: 0.3,
        ..DiarizationConfig::default()
    });
    let mut stream = a.stream();
    stream.append(&vec![0.0; 50 * 16_000]);
    assert!(matches!(
        b.advance(&mut stream, usize::MAX, &CancelToken::new()),
        Err(DiarError::State(_))
    ));
    assert_eq!(stream.windows_done(), 0);
}
