//! Input shorter than whisper.cpp's minimum (10 mel frames, `MIN_INPUT_SAMPLES`) is padded
//! by `WhisperTranscriber` itself: whisper.cpp 1.8.3 would otherwise skip the input with only
//! an "input is too short" warning and return no text. Own binary: it captures the
//! process-wide whisper log.

mod common;

use earshot_asr_whisper::{MIN_INPUT_SAMPLES, evidence};
use earshot_core::asr::{CancelToken, TranscribeOptions, Transcriber};
use earshot_core::audio::{MonoBuffer, PIPELINE_RATE};

const CLIP: &str = "61-70968-0000";

#[test]
fn input_shorter_than_whisper_minimum_is_decoded_not_skipped() {
    common::capture_whisper_logs();
    let (mut asr, _) = common::load_base_en(false);
    let speech = common::load_smoke_clip(CLIP);
    // 100 ms (whisper.cpp's stated minimum, still one frame short), 62.5 ms, a length below
    // whisper.cpp's 200-sample reflective pad, and the minimum itself; greedy and beam.
    for len in [1_600, 1_000, 100, MIN_INPUT_SAMPLES] {
        for options in [TranscribeOptions::live(), TranscribeOptions::batch()] {
            // A stretch of the clip with speech in it (its loudest region).
            let at = loudest(&speech, len);
            let audio = MonoBuffer::new(PIPELINE_RATE, speech[at..at + len].to_vec());
            let before = asr.fallback_stats();
            let mark = evidence::log_mark();
            let t = asr
                .transcribe(&audio, &options, &CancelToken::new())
                .unwrap_or_else(|e| panic!("{len} samples: {e}"));
            let logs = evidence::logs_since(mark);
            assert!(
                !logs.iter().any(|l| l.contains("input is too short")),
                "{len} samples were skipped by whisper.cpp:\n{}",
                logs.join("\n")
            );
            // whisper.cpp ran its encoder on the input (a skipped input starts no window).
            assert!(
                asr.fallback_stats().windows > before.windows,
                "{len} samples: no decoding window started"
            );
            assert_eq!(t.backend, asr.backend_info());
            // Times of a padded call never point into the padding: they end with the
            // caller's input. (Unpadded calls keep whisper.cpp's own segment ends, which can
            // run to the end of its 10 s window.)
            let end_ms = (len as u64 * 1000).div_ceil(16_000);
            for s in t.segments.iter().filter(|_| len < MIN_INPUT_SAMPLES) {
                assert!(s.end.as_millis() <= end_ms, "{len} samples: {s:?}");
                for w in &s.words {
                    assert!(w.end.as_millis() <= end_ms, "{len} samples: {w:?}");
                }
            }
        }
    }
}

/// Start of the `len`-sample window of `samples` with the most energy.
fn loudest(samples: &[f32], len: usize) -> usize {
    (0..=samples.len() - len)
        .step_by(len.max(1))
        .max_by(|&a, &b| {
            let e = |s: usize| samples[s..s + len].iter().map(|x| x * x).sum::<f32>();
            e(a).total_cmp(&e(b))
        })
        .unwrap_or(0)
}
