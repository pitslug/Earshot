//! DTW word times on a real model (T2.2d): base.en on the CPU, against token-level
//! timestamps on the same audio. Own test binary: it reads whisper.cpp's process-wide log.

mod common;

use std::num::NonZeroUsize;

use earshot_asr_whisper::{WhisperConfig, WhisperTranscriber, WordTiming};
use earshot_core::asr::{AsrError, CancelToken, TranscribeOptions, Transcriber, Transcription};

/// Smoke clips decoded by these tests (the first ones of `transcripts.txt`).
const CLIPS: usize = 4;

/// Held by every test that loads a model: whisper.cpp's log window is process-wide, so a
/// concurrent load would land in another test's window.
static LOADS: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn loads() -> std::sync::MutexGuard<'static, ()> {
    LOADS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn load(timing: WordTiming) -> WhisperTranscriber {
    let mut config = WhisperConfig::new(common::base_en());
    config.use_gpu = false;
    // One thread: multithreaded whisper.cpp CPU runs are not bit-reproducible, and the text
    // of the two runs is compared exactly.
    config.threads = NonZeroUsize::MIN;
    config.word_timing = timing;
    WhisperTranscriber::load(&config).unwrap_or_else(|e| panic!("load base.en {timing:?}: {e}"))
}

fn decode(t: &mut WhisperTranscriber, id: &str) -> Transcription {
    let buffer = common::smoke_buffer(id);
    let out = t
        .transcribe(&buffer, &TranscribeOptions::live(), &CancelToken::new())
        .unwrap_or_else(|e| panic!("{id}: {e}"));
    common::assert_well_formed(&out, t, &buffer);
    out
}

#[test]
fn dtw_word_times_run_on_base_en_and_keep_the_text() {
    let _loads = loads();
    common::capture_whisper_logs();
    let mark = earshot_asr_whisper::evidence::log_mark();
    let mut tokens = load(WordTiming::TokenTimestamps);
    let token_log = earshot_asr_whisper::evidence::logs_since(mark);
    let mark = earshot_asr_whisper::evidence::log_mark();
    let mut dtw = load(WordTiming::Dtw);
    let dtw_log = earshot_asr_whisper::evidence::logs_since(mark);
    assert_eq!(dtw.word_timing(), WordTiming::Dtw);
    // whisper.cpp's own record of the context parameters (`whisper_init_with_params_no_state`).
    let dtw_flag = |log: &[String]| {
        log.iter()
            .find(|l| l.contains("dtw        ="))
            .map(|l| l.trim_end().ends_with("= 1"))
            .unwrap_or_else(|| panic!("no dtw line in {log:?}"))
    };
    assert!(!dtw_flag(&token_log), "{token_log:?}");
    assert!(dtw_flag(&dtw_log), "{dtw_log:?}");
    assert!(
        !dtw_log
            .iter()
            .any(|l| l.contains("not supported with flash_attn")),
        "{dtw_log:?}"
    );

    let (mut words, mut moved) = (0usize, 0usize);
    for (id, _) in common::smoke_set().into_iter().take(CLIPS) {
        let a = decode(&mut tokens, &id);
        let b = decode(&mut dtw, &id);
        let text = |t: &Transcription| {
            t.segments
                .iter()
                .map(|s| s.text.clone())
                .collect::<Vec<_>>()
        };
        assert_eq!(text(&a), text(&b), "{id}: DTW changed the decoded text");
        for (sa, sb) in a.segments.iter().zip(&b.segments) {
            assert_eq!(
                (sa.start, sa.end),
                (sb.start, sb.end),
                "{id}: segment times"
            );
            assert_eq!(sa.words.len(), sb.words.len(), "{id}");
            for (wa, wb) in sa.words.iter().zip(&sb.words) {
                assert_eq!(wa.text, wb.text, "{id}");
                assert!(
                    sb.start <= wb.start && wb.end <= sb.end,
                    "{id}: {wb:?} outside {sb:?}"
                );
                words += 1;
                moved += usize::from(wa.start != wb.start);
            }
            // DTW ends a word where the next one starts.
            for pair in sb.words.windows(2) {
                assert_eq!(pair[0].end, pair[1].start, "{id}: {pair:?}");
            }
        }
    }
    eprintln!(
        "dtw: {moved} of {words} word starts differ from token timestamps over {CLIPS} clips"
    );
    assert!(words > 0);
    // DTW times are its own (20 ms frames), not the token timestamps passed through.
    assert!(
        moved * 2 > words,
        "only {moved} of {words} word starts changed"
    );
}

#[test]
fn dtw_refuses_alignment_heads_of_another_model() {
    // base.en under tiny.en's name: tiny.en's heads are for a 4 x 6 text decoder, base.en
    // has 6 x 8. The pinned shape (or whisper.cpp's own head check) refuses it.
    let _loads = loads();
    let dir = std::env::temp_dir().join(format!("earshot-dtw-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("temp dir");
    let renamed = dir.join("ggml-tiny.en.bin");
    let _ = std::fs::remove_file(&renamed);
    if std::fs::hard_link(common::base_en(), &renamed).is_err() {
        std::fs::copy(common::base_en(), &renamed).expect("copy base.en");
    }
    let mut config = WhisperConfig::new(&renamed);
    config.use_gpu = false;
    config.word_timing = WordTiming::Dtw;
    let result = WhisperTranscriber::load(&config);
    let _ = std::fs::remove_dir_all(&dir);
    eprintln!("dtw: renamed base.en: {:?}", result.as_ref().err());
    match result {
        Err(AsrError::Config(m)) => assert!(m.contains("tiny.en"), "{m}"),
        Err(AsrError::Backend(m)) => assert!(m.contains("load"), "{m}"),
        other => panic!("base.en ran with tiny.en's alignment heads: {other:?}"),
    }
}
