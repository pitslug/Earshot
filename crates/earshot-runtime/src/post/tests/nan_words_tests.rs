//! T14.23: a recogniser that hands out a word probability that is no number never costs the
//! final pass its write. Every name contains `post`.

use super::*;

/// A scripted segment whose words are built field by field, as a faulty recogniser hands them
/// over: nothing has looked at their probabilities.
fn raw_as_given(words: &[(&str, u64, u64, f32)]) -> RawSegment {
    let words: Vec<Word> = words
        .iter()
        .map(|&(text, start, end, probability)| Word {
            text: text.into(),
            start: MeetingTime::from_millis(start),
            end: MeetingTime::from_millis(end),
            probability,
        })
        .collect();
    RawSegment {
        start: words[0].start,
        end: words[words.len() - 1].end,
        text: words
            .iter()
            .map(|w| w.text.as_str())
            .collect::<Vec<_>>()
            .join(" "),
        words,
    }
}

#[test]
fn post_a_word_probability_that_is_no_number_is_stored_as_no_confidence() {
    let dir = TempDir::new();
    let svc = services(&dir);
    let (meeting, mic, them) = meeting_rows(&svc);
    let (mic_audio, them_audio) = scene();
    // The script of `transcriber()`, with NaN and infinities among the probabilities.
    let script = FakeTranscriber::scripted([
        Ok(vec![
            raw_as_given(&[
                ("hello", 1_000, 1_400, f32::NAN),
                ("all", 1_500, 1_900, 0.8),
            ]),
            raw_as_given(&[("thanks", 31_000, 31_500, f32::NAN)]),
        ]),
        Ok(vec![
            raw_as_given(&[("hi", 13_000, 13_300, f32::INFINITY)]),
            raw_as_given(&[
                ("see", 19_000, 19_300, 0.6),
                ("you", 21_000, 21_300, f32::NEG_INFINITY),
            ]),
            raw_as_given(&[("bye", 41_000, 41_300, 0.9)]),
        ]),
    ]);
    let mut post = PostProcessor::new(
        script,
        FakeSegmenter::segmentation_3_0(),
        embedder(),
        config(),
    )
    .unwrap();
    let input = PostInput {
        echo_locks: Vec::new(),
        echo_locks_unreadable: false,
        meeting,
        tracks: vec![track_input(&mic, mic_audio), track_input(&them, them_audio)],
    };
    let mut rng = ChaCha8Rng::seed_from_u64(3);
    post.run(&svc, input, &mut rng, &CancelToken::new())
        .expect("the final pass stores");

    // Read back from the store: every word with its text and times, the ones without a number
    // at no confidence, the others as given.
    let mut words: Vec<(String, u64, u64, f32)> = finals_of(&svc, meeting)
        .iter()
        .flat_map(|s| s.words.clone())
        .map(|w| {
            (
                w.text,
                w.start.as_millis(),
                w.end.as_millis(),
                w.probability,
            )
        })
        .collect();
    words.sort_by_key(|w| w.1);
    let want = [
        ("hello", 1_000, 1_400, 0.0),
        ("all", 1_500, 1_900, 0.8),
        ("hi", 13_000, 13_300, 0.0),
        ("see", 19_000, 19_300, 0.6),
        ("you", 21_000, 21_300, 0.0),
        ("thanks", 31_000, 31_500, 0.0),
        ("bye", 41_000, 41_300, 0.9),
    ];
    assert_eq!(words.len(), want.len(), "{words:?}");
    for (got, want) in words.iter().zip(want) {
        assert_eq!(
            (got.0.as_str(), got.1, got.2, got.3),
            want,
            "all of {words:?}"
        );
    }
    for segment in finals_of(&svc, meeting) {
        assert!(
            segment.confidence.is_finite() && (0.0..=1.0).contains(&segment.confidence),
            "{segment:?}"
        );
    }
}
