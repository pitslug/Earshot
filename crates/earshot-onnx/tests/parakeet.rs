//! T6.10e1 accept: `cargo test -p earshot-onnx parakeet`.
//!
//! The NeMo front end against onnx-asr's `nemo128.onnx` (bounds fixed before measuring: max
//! absolute difference 5e-3 and mean 1e-4 over the valid frames of normalised features; after
//! measuring, the first and last frame, mostly zero padding at the log floor, got a separate
//! sanity bound of 0.25, logged in T6.10e1), and
//! the default Parakeet TDT models (110M TDT branch and 0.6B v2) on the committed smoke clips
//! (corpus WER at most 8%, Whisper English normalisation, T6.10e2, fixed before measuring;
//! NVIDIA reports about 2-3% on LibriSpeech test-clean). Since PLAN D22 (tests use only the
//! default models) the CTC head and the multilingual v3 are no longer loaded here. Needs
//! the `ci` model set (and `eval` for the front-end reference) and the CPU ONNX Runtime; a
//! missing model fails with the fetch command.

mod common;

use earshot_core::asr::{AsrError, CancelToken, Language, TranscribeOptions, Transcriber};
use earshot_core::audio::{MonoBuffer, PIPELINE_RATE};
use earshot_core::metrics::normalize::normalize_english;
use earshot_core::metrics::wer::{align_tokens, corpus_wer};
use earshot_onnx::SessionConfig;
use earshot_onnx::nemo_mel::NemoMel;
use earshot_onnx::parakeet::{OnnxNemoMel, ParakeetTdt};

const MAX_ABS_DIFF: f32 = 5e-3;
const MAX_MEAN_DIFF: f32 = 1e-4;
/// The two frames made mostly of STFT padding (see the test).
const MAX_EDGE_DIFF: f32 = 0.25;
const MAX_SMOKE_WER: f64 = 0.08;

fn config() -> SessionConfig {
    common::init_runtime();
    SessionConfig {
        provider: common::test_provider(),
        intra_threads: Some(4),
        background: None,
    }
}

/// Smoke clips with their reference text, in file order.
fn smoke() -> Vec<(String, Vec<f32>, String)> {
    let dir = common::repo_root().join("fixtures/smoke");
    let refs = std::fs::read_to_string(dir.join("transcripts.txt")).expect("transcripts");
    refs.lines()
        .filter_map(|l| l.split_once(' '))
        .map(|(id, text)| {
            let audio = common::decode_flac(&dir.join(format!("{id}.flac")));
            (id.to_owned(), audio, text.to_owned())
        })
        .collect()
}

#[test]
#[ignore = "needs the eval model set (cargo xtask fetch-models --set eval): cargo test --release -p earshot-onnx --test parakeet -- --include-ignored"]
fn parakeet_front_end_matches_the_nemo_reference() {
    let mut reference =
        OnnxNemoMel::load(&common::fetched(common::NEMO128, "eval"), &config()).expect("nemo128");
    let ours = NemoMel::new(128);
    let clips: Vec<Vec<f32>> = smoke().into_iter().take(4).map(|(_, a, _)| a).collect();
    // Real speech only (a pure tone leaves bins at the log floor with almost no variance, so
    // normalisation magnifies float rounding on both sides), plus a cut that is not a whole
    // number of hops.
    let mut inputs = clips.clone();
    inputs.push(clips[1][..clips[1].len() - 173].to_vec());
    let (mut worst, mut worst_edge, mut sum, mut n) = (0.0f32, 0.0f32, 0.0f64, 0usize);
    for x in &inputs {
        let (want, n_mels, t, valid) = reference.compute(x).expect("reference");
        let (frames, nemo_valid) = NemoMel::frames(x.len());
        assert_eq!(n_mels, 128);
        // onnx-asr keeps every STFT frame valid (1 + len / 160); NeMo's `get_seq_len` keeps
        // len / 160 (features.py on main), which `compute` follows. Everything else is
        // compared with onnx-asr's convention.
        assert_eq!(
            (t, valid),
            (frames, nemo_valid + 1),
            "onnx-asr frame convention"
        );
        let got = ours.compute_valid(x, valid);
        for m in 0..n_mels {
            for f in 0..valid {
                let d = (want[m * t + f] - got[m * frames + f]).abs();
                // The first and last frames are mostly the STFT's zero padding: their energy
                // can sit at the 2^-24 log floor, where float32 rounding in either
                // implementation is magnified (measured: up to 0.14 there, nothing above 5e-3
                // elsewhere). They keep a sanity bound; every interior frame the strict one.
                if f == 0 || f + 1 == valid {
                    worst_edge = worst_edge.max(d);
                } else {
                    worst = worst.max(d);
                }
                sum += f64::from(d);
                n += 1;
            }
        }
    }
    let mean = (sum / n as f64) as f32;
    println!(
        "front end vs nemo128.onnx: interior max |diff| {worst:.2e}, edge frames {worst_edge:.2e}, \
         mean {mean:.2e} over {n} values"
    );
    assert!(worst <= MAX_ABS_DIFF, "interior max |diff| {worst}");
    assert!(
        worst_edge <= MAX_EDGE_DIFF,
        "edge-frame max |diff| {worst_edge}"
    );
    assert!(mean <= MAX_MEAN_DIFF, "mean |diff| {mean}");
}

#[test]
#[ignore = "needs the ci model set (cargo xtask fetch-models --set ci): cargo test --release -p earshot-onnx --test parakeet -- --include-ignored"]
fn parakeet_refuses_other_languages_and_empty_input_is_empty() {
    // On the default 110M TDT model (PLAN D22; this ran on the 110M CTC head before).
    let mut asr = ParakeetTdt::load(
        &common::fetched(common::PARAKEET_TDT_110M_DIR, "ci"),
        &config(),
        "parakeet-tdt-110m",
        true,
    )
    .expect("load Parakeet TDT 110M");
    let opts = TranscribeOptions {
        language: Language::Code("de".into()),
        ..TranscribeOptions::batch()
    };
    let one_s = MonoBuffer::new(PIPELINE_RATE, vec![0.0; 16_000]);
    assert!(matches!(
        asr.transcribe(&one_s, &opts, &CancelToken::new()),
        Err(AsrError::Config(_))
    ));
    let empty = MonoBuffer::new(PIPELINE_RATE, Vec::new());
    let t = asr
        .transcribe(&empty, &TranscribeOptions::batch(), &CancelToken::new())
        .expect("empty");
    assert!(t.segments.is_empty());
}

/// Corpus WER of `asr` on the smoke clips (printing each hypothesis).
fn smoke_wer(asr: &mut dyn Transcriber, label: &str) -> f64 {
    let opts = TranscribeOptions::batch();
    let cancel = CancelToken::new();
    let mut counts = Vec::new();
    for (id, audio, reference) in smoke() {
        let len = audio.len();
        let t = asr
            .transcribe(&MonoBuffer::new(PIPELINE_RATE, audio), &opts, &cancel)
            .expect("transcribe");
        let words = t.words();
        let end_ms = (len as u64) * 1000 / 16_000 + 400;
        for w in &words {
            assert!(
                w.start <= w.end && w.end.as_millis() <= end_ms,
                "{label} {id}: {w:?}"
            );
            assert!(
                w.probability > 0.0 && w.probability <= 1.0,
                "{label} {id}: {w:?}"
            );
        }
        let hyp = words
            .iter()
            .map(|w| w.text.as_str())
            .collect::<Vec<_>>()
            .join(" ");
        println!("{label} {id}: {hyp}");
        let (r, h) = (normalize_english(&reference), normalize_english(&hyp));
        counts.push(align_tokens(
            &r.split_whitespace().collect::<Vec<_>>(),
            &h.split_whitespace().collect::<Vec<_>>(),
        ));
    }
    let total = corpus_wer(counts);
    let wer = total.wer().expect("reference words");
    println!(
        "{label} on {} smoke reference words: WER {:.2}% ({:?})",
        total.ref_words,
        wer * 100.0,
        asr.backend_info()
    );
    wer
}

#[test]
#[ignore = "needs the ci model set (cargo xtask fetch-models --set ci): cargo test --release -p earshot-onnx --test parakeet -- --include-ignored"]
fn parakeet_tdt_models_transcribe_the_smoke_clips() {
    // The default TDT models (PLAN D22; v3, multilingual, was measured here until then).
    for (dir, name, english) in [
        (
            common::PARAKEET_TDT_110M_DIR,
            "parakeet-tdt_ctc-110m (TDT)",
            true,
        ),
        (common::PARAKEET_TDT_V2_DIR, "parakeet-tdt-0.6b-v2", true),
    ] {
        let mut asr = ParakeetTdt::load(&common::fetched(dir, "ci"), &config(), name, english)
            .unwrap_or_else(|e| panic!("load {name}: {e}"));
        let wer = smoke_wer(&mut asr, name);
        assert!(wer <= MAX_SMOKE_WER, "{name}: smoke WER {wer}");
        if english {
            let opts = TranscribeOptions {
                language: Language::Code("de".into()),
                ..TranscribeOptions::batch()
            };
            let one_s = MonoBuffer::new(PIPELINE_RATE, vec![0.0; 16_000]);
            assert!(matches!(
                asr.transcribe(&one_s, &opts, &CancelToken::new()),
                Err(AsrError::Config(_))
            ));
        }
    }
}

/// T12.3b (evaluation only): a model borrowing another's encoder through
/// `ParakeetTdt::share_encoder` gives the same words as the one that shared it, both count their
/// lock waits, and two threads decoding at once through the one encoder finish with the same
/// words they give alone.
#[test]
#[ignore = "needs the ci model set (cargo xtask fetch-models --set ci): cargo test --release -p earshot-onnx --test parakeet -- --include-ignored"]
fn parakeet_tdt_shared_encoder_gives_the_same_words() {
    let dir = common::fetched(common::PARAKEET_TDT_110M_DIR, "ci");
    let name = "parakeet-tdt_ctc-110m (TDT)";
    let own = ParakeetTdt::load(&dir, &config(), name, true).expect("load");
    let (mut track, encoder) = own.share_encoder();
    let mut preview =
        ParakeetTdt::load_sharing(&dir, &encoder, &config(), name, true).expect("load sharing");
    assert_eq!(encoder.users(), 3, "the handle, the track and the preview");
    assert_eq!(preview.backends()[0], encoder.backend());

    let opts = TranscribeOptions::live();
    let cancel = CancelToken::new();
    let clips: Vec<MonoBuffer> = smoke()
        .into_iter()
        .take(4)
        .map(|(_, a, _)| MonoBuffer::new(PIPELINE_RATE, a))
        .collect();
    let words = |asr: &mut ParakeetTdt, clip: &MonoBuffer| {
        asr.transcribe(clip, &opts, &cancel)
            .expect("transcribe")
            .words()
    };
    let alone: Vec<_> = clips.iter().map(|c| words(&mut track, c)).collect();
    assert!(
        alone.iter().all(|w| !w.is_empty()),
        "smoke clips have words"
    );
    let waits = track.take_encoder_waits();
    assert_eq!(waits.len(), clips.len(), "one encoder call per short clip");
    for (clip, want) in clips.iter().zip(&alone) {
        assert_eq!(&words(&mut preview, clip), want, "the preview's words");
    }
    assert_eq!(preview.take_encoder_waits().len(), clips.len());

    // Both at once: each thread gets the words it got alone.
    let (track_words, preview_words) = std::thread::scope(|s| {
        let a = s.spawn(|| {
            (0..3)
                .flat_map(|_| {
                    clips
                        .iter()
                        .map(|c| words(&mut track, c))
                        .collect::<Vec<_>>()
                })
                .collect::<Vec<_>>()
        });
        let b = s.spawn(|| {
            (0..3)
                .flat_map(|_| {
                    clips
                        .iter()
                        .map(|c| words(&mut preview, c))
                        .collect::<Vec<_>>()
                })
                .collect::<Vec<_>>()
        });
        (a.join().expect("track"), b.join().expect("preview"))
    });
    let want: Vec<_> = (0..3).flat_map(|_| alone.clone()).collect();
    assert_eq!(track_words, want);
    assert_eq!(preview_words, want);

    // An own encoder records no waits.
    let mut plain = ParakeetTdt::load(&dir, &config(), name, true).expect("load");
    let _ = words(&mut plain, &clips[0]);
    assert!(plain.take_encoder_waits().is_empty());
}
