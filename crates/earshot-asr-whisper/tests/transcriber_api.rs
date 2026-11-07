//! `WhisperTranscriber` behaviour on the CPU with base.en: options, languages, prompts,
//! threads, cancellation and errors. Scoring lives in `cpu_wer` / `cuda_wer`.

mod common;

use std::num::NonZeroUsize;
use std::time::{Duration, Instant};

use earshot_asr_whisper::{WhisperConfig, WhisperTranscriber};
use earshot_core::asr::{
    AsrError, CancelToken, DecodingOptions, DecodingStrategy, Language, TranscribeOptions,
    Transcriber, normalize_token,
};
use earshot_core::audio::{MonoBuffer, PIPELINE_RATE, SampleRate};
use earshot_core::metrics::wer::word_errors_normalized;

const CLIP: &str = "61-70968-0000";

fn cpu() -> WhisperTranscriber {
    common::load_base_en(false).0
}

fn single_thread() -> WhisperTranscriber {
    let mut config = WhisperConfig::new(common::base_en());
    config.use_gpu = false;
    config.threads = NonZeroUsize::MIN;
    WhisperTranscriber::load(&config).unwrap()
}

fn text(t: &earshot_core::asr::Transcription) -> String {
    t.segments
        .iter()
        .map(|s| s.text.as_str())
        .collect::<Vec<_>>()
        .join(" ")
}

#[test]
fn live_and_batch_decoding_both_transcribe_the_clip() {
    let mut asr = cpu();
    let audio = common::smoke_buffer(CLIP);
    let reference = common::reference(CLIP);
    for options in [TranscribeOptions::live(), TranscribeOptions::batch()] {
        let t = asr
            .transcribe(&audio, &options, &CancelToken::new())
            .unwrap();
        common::assert_well_formed(&t, &asr, &audio);
        let counts = word_errors_normalized(&reference, &text(&t));
        assert!(
            counts.wer().unwrap() <= 0.25,
            "{:?}: {counts:?}\n ref: {reference}\n hyp: {}",
            options.decoding,
            text(&t)
        );
    }
}

/// Temperature 0 (a clean clip, so the fallback does not fire): repeated calls give the
/// same words, timestamps and probabilities. One thread, because multi-threaded ggml CPU reductions are not
/// bit-reproducible (observed: 0.7123 vs 0.7122), which can also flip a beam's text.
#[test]
fn temperature_zero_is_deterministic() {
    let mut asr = single_thread();
    let audio = common::smoke_buffer(CLIP);
    let options = TranscribeOptions::batch();
    let a = asr
        .transcribe(&audio, &options, &CancelToken::new())
        .unwrap();
    let b = asr
        .transcribe(&audio, &options, &CancelToken::new())
        .unwrap();
    // The premise: no window fell back to a sampled (non-deterministic) temperature.
    let stats = asr.fallback_stats();
    assert!(stats.windows >= 2, "{stats:?}");
    assert_eq!(stats.fallback_windows, 0, "{stats:?}");
    assert_eq!(text(&a), text(&b));
    let (wa, wb) = (a.words(), b.words());
    assert_eq!(wa.len(), wb.len());
    for (x, y) in wa.iter().zip(&wb) {
        assert_eq!(x, y);
    }
}

#[test]
fn word_timestamps_follow_the_speech() {
    let mut asr = cpu();
    let audio = common::smoke_buffer(CLIP);
    let t = asr
        .transcribe(&audio, &TranscribeOptions::live(), &CancelToken::new())
        .unwrap();
    let words = t.words();
    let duration_ms = audio.samples.len() as u64 * 1000 / u64::from(common::SAMPLE_RATE);
    assert!(words.len() >= 5, "{words:?}");
    let first = &words[0];
    let last = words.last().unwrap();
    assert!(first.start.as_millis() < 2_000, "{first:?}");
    assert!(
        last.end.as_millis() + 3_000 >= duration_ms,
        "{last:?} ends long before {duration_ms} ms"
    );
    let distinct_starts = words.windows(2).filter(|p| p[0].start < p[1].start).count();
    assert!(
        distinct_starts * 2 >= words.len(),
        "word starts are not spread over the clip: {words:?}"
    );
    for segment in &t.segments {
        for w in &segment.words {
            assert!(
                segment.start <= w.start && w.end <= segment.end,
                "{w:?} in {segment:?}"
            );
        }
    }
}

#[test]
fn language_default_auto_and_names_resolve_to_english_on_base_en() {
    let mut asr = cpu();
    let audio = common::smoke_buffer(CLIP);
    let default = asr
        .transcribe(&audio, &TranscribeOptions::live(), &CancelToken::new())
        .unwrap();
    assert_eq!(default.language.as_deref(), Some("en"));
    for language in [Language::Auto, Language::Code("english".into())] {
        let options = TranscribeOptions {
            language: language.clone(),
            ..TranscribeOptions::live()
        };
        let t = asr
            .transcribe(&audio, &options, &CancelToken::new())
            .unwrap();
        assert_eq!(t.language.as_deref(), Some("en"), "{language:?}");
        assert_eq!(text(&t), text(&default), "{language:?}");
    }
}

#[test]
fn unsupported_languages_and_decoding_settings_are_config_errors() {
    let mut asr = cpu();
    let audio = common::smoke_buffer(CLIP);
    let german = TranscribeOptions {
        language: Language::Code("de".into()),
        ..TranscribeOptions::live()
    };
    let unknown = TranscribeOptions {
        language: Language::Code("klingon".into()),
        ..TranscribeOptions::live()
    };
    let beam = TranscribeOptions {
        decoding: DecodingOptions {
            strategy: DecodingStrategy::BeamSearch { beam_size: 0 },
            temperature: 0.0,
            fallback: None,
        },
        ..TranscribeOptions::live()
    };
    for options in [german, unknown, beam] {
        let err = asr
            .transcribe(&audio, &options, &CancelToken::new())
            .unwrap_err();
        assert!(matches!(err, AsrError::Config(_)), "{options:?}: {err:?}");
    }
}

#[test]
fn prompt_is_accepted_and_nul_prompt_is_rejected() {
    let mut asr = cpu();
    let audio = common::smoke_buffer(CLIP);
    let options = TranscribeOptions {
        prompt: Some("A story about a boy on a bridge.".into()),
        ..TranscribeOptions::live()
    };
    let t = asr
        .transcribe(&audio, &options, &CancelToken::new())
        .unwrap();
    common::assert_well_formed(&t, &asr, &audio);

    let long = TranscribeOptions {
        prompt: Some("word ".repeat(2_000)),
        ..TranscribeOptions::live()
    };
    let t = asr.transcribe(&audio, &long, &CancelToken::new()).unwrap();
    common::assert_well_formed(&t, &asr, &audio);

    let nul = TranscribeOptions {
        prompt: Some("bad\0prompt".into()),
        ..TranscribeOptions::live()
    };
    let err = asr
        .transcribe(&audio, &nul, &CancelToken::new())
        .unwrap_err();
    assert!(matches!(err, AsrError::Config(_)), "{err:?}");
}

#[test]
fn single_thread_config_transcribes() {
    let mut config = WhisperConfig::new(common::base_en());
    config.use_gpu = false;
    config.threads = NonZeroUsize::MIN;
    let mut asr = WhisperTranscriber::load(&config).unwrap();
    let audio = common::smoke_buffer(CLIP);
    let t = asr
        .transcribe(&audio, &TranscribeOptions::live(), &CancelToken::new())
        .unwrap();
    common::assert_well_formed(&t, &asr, &audio);
}

#[test]
fn wrong_rate_and_empty_audio() {
    let mut asr = cpu();
    let wrong = MonoBuffer::new(SampleRate::HZ_48000, vec![0.0; 48_000]);
    let err = asr
        .transcribe(&wrong, &TranscribeOptions::live(), &CancelToken::new())
        .unwrap_err();
    assert!(matches!(err, AsrError::WrongSampleRate { .. }), "{err:?}");

    let empty = MonoBuffer::new(PIPELINE_RATE, Vec::new());
    let t = asr
        .transcribe(&empty, &TranscribeOptions::live(), &CancelToken::new())
        .unwrap();
    assert!(t.segments.is_empty());
    assert_eq!(t.backend, asr.backend_info());
}

#[test]
fn a_cancelled_token_stops_before_decoding() {
    let mut asr = cpu();
    let cancel = CancelToken::new();
    cancel.cancel();
    let err = asr
        .transcribe(
            &common::smoke_buffer(CLIP),
            &TranscribeOptions::batch(),
            &cancel,
        )
        .unwrap_err();
    assert_eq!(err, AsrError::Cancelled);
}

#[test]
fn cancelling_from_another_thread_aborts_a_long_decode() {
    let mut asr = single_thread();
    // The whole smoke set four times (about 5 min of audio, 9 whisper windows): far longer
    // than the cancellation delay on one thread.
    let mut samples = Vec::new();
    for _ in 0..4 {
        for (id, _) in common::smoke_set() {
            samples.extend(common::load_smoke_clip(&id));
        }
    }
    let audio = MonoBuffer::new(PIPELINE_RATE, samples);

    // Calibration on this host under its current load: one whisper window (30 s) of the
    // same audio, decoded to the end. The abort callback is polled after every encoder and
    // decoder graph, so a cancelled call must return within about one window's work; a
    // fixed bound in seconds fails on a loaded or slower host (it did under full ci).
    let window = MonoBuffer::new(PIPELINE_RATE, audio.samples[..30 * 16_000].to_vec());
    let started = Instant::now();
    asr.transcribe(&window, &TranscribeOptions::batch(), &CancelToken::new())
        .unwrap();
    let one_window = started.elapsed();
    let full_windows = audio.samples.len() / (30 * 16_000);
    assert!(full_windows >= 8, "audio must be many windows long");

    let cancel = CancelToken::new();
    let remote = cancel.clone();
    let canceller = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(300));
        remote.cancel();
        Instant::now()
    });
    let result = asr.transcribe(&audio, &TranscribeOptions::batch(), &cancel);
    let returned = Instant::now();
    let cancelled_at = canceller.join().unwrap();
    assert_eq!(result.unwrap_err(), AsrError::Cancelled);
    let lag = returned.saturating_duration_since(cancelled_at);
    // Both bounds hold: the original fixed one and the one derived from this host's
    // window time (AGENTS.md 11: a harder condition keeps the original assertion).
    assert!(
        lag < Duration::from_secs(5),
        "returned {lag:?} after cancel"
    );
    assert!(
        lag < one_window * 2,
        "returned {lag:?} after cancel; one 30 s window takes {one_window:?} \
         ({full_windows} windows in the audio)"
    );

    // The transcriber stays usable after a cancelled call.
    let clip = common::smoke_buffer(CLIP);
    let t = asr
        .transcribe(&clip, &TranscribeOptions::live(), &CancelToken::new())
        .unwrap();
    common::assert_well_formed(&t, &asr, &clip);
}

/// Hard audio for the history test: three smoke clips under seeded white noise (about
/// 0 dB SNR), so the beam has real alternatives to choose from.
fn noisy_smoke() -> MonoBuffer {
    let mut samples = Vec::new();
    for id in ["61-70968-0001", "121-121726-0002", "61-70968-0003"] {
        samples.extend(common::load_smoke_clip(id));
    }
    let rms = (samples.iter().map(|s| s * s).sum::<f32>() / samples.len() as f32).sqrt();
    let mut state: u64 = 0x9e37_79b9_7f4a_7c15;
    for s in &mut samples {
        state = state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        let u = (state >> 40) as f32 / (1u64 << 24) as f32 - 0.5;
        *s = (*s + u * rms * 3.46).clamp(-1.0, 1.0);
    }
    MonoBuffer::new(PIPELINE_RATE, samples)
}

/// whisper.cpp's beam search draws its candidates with the first decoder's RNG, which a
/// whisper state seeds once and never resets (whisper.cpp 1.8.3, `whisper_init_state` /
/// `whisper_sample_token_topk`). A batch decode must not depend on earlier calls (T2.5:
/// AMI WER differed by 21 points depending on what the transcriber had decoded before).
#[test]
fn batch_decoding_does_not_depend_on_earlier_calls() {
    let load = || {
        let mut config = WhisperConfig::new(common::base_en());
        config.use_gpu = false;
        config.threads = NonZeroUsize::MIN; // bit-reproducible CPU arithmetic
        WhisperTranscriber::load(&config).unwrap()
    };
    let audio = noisy_smoke();
    let options = TranscribeOptions::batch();
    let fresh = text(
        &load()
            .transcribe(&audio, &options, &CancelToken::new())
            .unwrap(),
    );
    let mut used = load();
    for call in 1..=3 {
        let t = text(
            &used
                .transcribe(&audio, &options, &CancelToken::new())
                .unwrap(),
        );
        assert_eq!(t, fresh, "call {call} differs from a fresh transcriber");
    }
}

/// The smoke clip whose six copies loop base.en at temperature 0 (see the test).
const LOOP_CLIP: &str = "121-121726-0012";
/// A decode of six copies with at least this many repeats of one trigram looped (measured
/// 27 on base.en; a clean decode gives 6, one per copy).
const LOOP_MIN_REPEATS: usize = 20;

/// Largest number of times one 3-word sequence occurs in `text` (a repetition loop scores
/// far above the number of copies of the speech, normal speech about one per copy).
fn max_trigram_repeats(text: &str) -> usize {
    let words: Vec<String> = text.split_whitespace().map(normalize_token).collect();
    let mut counts = std::collections::HashMap::new();
    for run in words.windows(3) {
        *counts.entry(run.join(" ")).or_insert(0usize) += 1;
    }
    counts.into_values().max().unwrap_or(0)
}

/// T2.4d: one smoke clip six times in a row sends the greedy decoder into a repetition loop
/// at temperature 0 (dev data, found by probing smoke-set inputs). Whisper's temperature
/// fallback detects the window and retries it at higher temperatures, which breaks the
/// loop; the transcriber counts it.
///
/// Since PLAN D22 this runs on base.en (tiny.en before, which looped on 121-121726-0005 x6
/// with 50+ repeats of one trigram). Probed on base.en, one thread, 2026-11-02 (every smoke
/// clip repeated 3 to 9 times): base.en loops only on 121-121726-0012 x6, 27 repeats of one
/// trigram in 137 words where the speech holds about 30; every other input stays at about
/// one repeat per copy of the clip (at most 10, for a clip repeated 9 times). The loop bound
/// is re-derived from that measurement: [`LOOP_MIN_REPEATS`] (20), more than three times
/// the 6 a clean decode of six copies gives and below the 27 measured; the bound on the
/// fixed text is unchanged.
#[test]
fn temperature_fallback_breaks_a_repetition_loop() {
    let load = || {
        let mut config = WhisperConfig::new(common::base_en());
        config.use_gpu = false;
        config.threads = NonZeroUsize::MIN; // bit-reproducible CPU arithmetic
        WhisperTranscriber::load(&config).unwrap()
    };
    let clip = common::load_smoke_clip(LOOP_CLIP);
    let audio = MonoBuffer::new(PIPELINE_RATE, clip.repeat(6));

    let mut without = TranscribeOptions::live();
    without.decoding.fallback = None;
    let mut asr = load();
    let looped = text(
        &asr.transcribe(&audio, &without, &CancelToken::new())
            .unwrap(),
    );
    let stats = asr.fallback_stats();
    eprintln!(
        "without fallback: {} repeats; {looped}",
        max_trigram_repeats(&looped)
    );
    assert!(
        max_trigram_repeats(&looped) >= LOOP_MIN_REPEATS,
        "expected a loop at temperature 0 without fallback: {looped}"
    );
    assert_eq!((stats.fallback_windows, stats.retries), (0, 0), "{stats:?}");

    let mut asr = load();
    let options = TranscribeOptions::live();
    assert!(options.decoding.fallback.is_some());
    let fixed = text(
        &asr.transcribe(&audio, &options, &CancelToken::new())
            .unwrap(),
    );
    let stats = asr.fallback_stats();
    eprintln!(
        "with fallback: {} repeats, {stats:?}; {fixed}",
        max_trigram_repeats(&fixed)
    );
    assert!(
        max_trigram_repeats(&fixed) < 10,
        "the fallback did not break the loop: {fixed}"
    );
    assert!(
        stats.fallback_windows >= 1 && stats.retries >= 1,
        "{stats:?}"
    );
    assert!(stats.windows >= stats.fallback_windows, "{stats:?}");
}
