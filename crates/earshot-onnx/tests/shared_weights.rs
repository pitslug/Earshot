//! T9.1ma: sessions sharing one copy of Parakeet 0.6B's weights decode exactly as ordinary ones.
//! Its own test binary: [`set_enabled`] is process-wide.

mod common;

use earshot_core::asr::{CancelToken, TranscribeOptions, Transcriber};
use earshot_core::audio::{MonoBuffer, PIPELINE_RATE};
use earshot_onnx::parakeet::ParakeetTdt;
use earshot_onnx::runtime::OnnxSession;
use earshot_onnx::shared_weights::{for_model, set_enabled};
use earshot_onnx::{Provider, SessionConfig};
use std::sync::Arc;

const NAME: &str = "parakeet-tdt-0.6b-v2";

fn config() -> SessionConfig {
    common::init_runtime();
    SessionConfig {
        provider: Provider::Cpu,
        intra_threads: Some(4),
        background: None,
    }
}

/// Every smoke clip's words, as (text, start ms, end ms, probability).
fn decode(asr: &mut ParakeetTdt) -> Vec<Vec<(String, u64, u64, f32)>> {
    let dir = common::repo_root().join("fixtures/smoke");
    let mut clips: Vec<_> = std::fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|e| e == "flac"))
        .collect();
    clips.sort();
    assert!(!clips.is_empty(), "no smoke clips");
    clips
        .iter()
        .map(|clip| {
            let audio = MonoBuffer::new(PIPELINE_RATE, common::decode_flac(clip));
            asr.transcribe(&audio, &TranscribeOptions::live(), &CancelToken::new())
                .unwrap()
                .words()
                .into_iter()
                .map(|w| {
                    (
                        w.text,
                        w.start.as_millis(),
                        w.end.as_millis(),
                        w.probability,
                    )
                })
                .collect()
        })
        .collect()
}

#[test]
#[ignore = "needs the ci model set (cargo xtask fetch-models --set ci): cargo test --release -p earshot-onnx --test shared_weights -- --include-ignored"]
fn shared_weights_decode_as_an_ordinary_load_and_are_freed_with_the_last_session() {
    let dir = common::fetched(common::PARAKEET_TDT_V2_DIR, "ci");
    let encoder = dir.join("encoder.onnx");
    let config = config();

    set_enabled(false);
    let plain = OnnxSession::load(&encoder, &config).unwrap();
    assert!(!plain.shares_weights());
    drop(plain);
    let mut asr = ParakeetTdt::load(&dir, &config, NAME, true).unwrap();
    let ordinary = decode(&mut asr);
    drop(asr);

    set_enabled(true);
    let mut first = ParakeetTdt::load(&dir, &config, NAME, true).unwrap();
    let mut second = ParakeetTdt::load(&dir, &config, NAME, true).unwrap();
    let held = for_model(&encoder).expect("the sessions hold the weights");
    assert_eq!(Arc::strong_count(&held), 3, "two sessions and this handle");

    let shared = decode(&mut first);
    assert_eq!(decode(&mut second), shared, "two sharing sessions agree");
    // Turning prepacking off changes which kernels run, so a probability may round differently;
    // the words and their times may not.
    assert_eq!(ordinary.len(), shared.len());
    for (a, b) in ordinary.iter().zip(&shared) {
        assert_eq!(a.len(), b.len(), "{a:?} {b:?}");
        for (x, y) in a.iter().zip(b) {
            assert_eq!((&x.0, x.1, x.2), (&y.0, y.1, y.2));
            assert!((x.3 - y.3).abs() <= 1e-3, "{x:?} {y:?}");
        }
    }

    // The weights go with the last session, so a finished meeting gives the memory back.
    let weak = Arc::downgrade(&held);
    drop((first, second, held));
    assert!(
        weak.upgrade().is_none(),
        "the weights outlived their sessions"
    );
}
