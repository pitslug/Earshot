//! T3.2 accept: `cargo test -p earshot-onnx diar`.
//!
//! pyannote segmentation-3.0 and CAM++ on ONNX Runtime, run through the core `Diarizer` on a
//! two-speaker mix of the committed smoke utterances (speakers 61 and 121, alternating).

mod common;

use std::ops::Range;

use earshot_core::asr::{CancelToken, MeetingTime};
use earshot_core::audio::{MonoBuffer, PIPELINE_RATE};
use earshot_core::diar::{
    ClusteringConfig, DiarError, Diarization, DiarizationConfig, Diarizer, SpeakerEmbedder,
    SpeakerSegmenter,
};
use earshot_core::metrics::der::{AMI_COLLAR_PER_SIDE_S, DerConfig, Turn, der};
use earshot_onnx::SessionConfig;
use earshot_onnx::embed::{CAMPPLUS_DIM, CamppEmbedder};
use earshot_onnx::segment::PyannoteSegmenter;

fn config() -> SessionConfig {
    common::init_runtime();
    SessionConfig {
        provider: common::test_provider(),
        intra_threads: None,
        background: None,
    }
}

fn segmenter() -> PyannoteSegmenter {
    PyannoteSegmenter::load(&common::model(common::SEGMENTATION), &config(), 8)
        .expect("load segmentation")
}

fn embedder() -> CamppEmbedder {
    CamppEmbedder::load(&common::model(common::CAMPPLUS), &config()).expect("load CAM++")
}

/// The two-speaker mix: utterances of speaker 61 and 121 alternating, 0.5 s apart, and the
/// reference turns (utterance spans, labelled "61" / "121").
fn two_speaker_mix() -> (Vec<f32>, Vec<(String, Range<usize>)>) {
    let (a, b) = common::smoke_speech();
    let gap = PIPELINE_RATE.hz() as usize / 2;
    let mut audio = Vec::new();
    let mut turns = Vec::new();
    for i in 0..a.len().max(b.len()) {
        for (spk, utts) in [("61", a), ("121", b)] {
            if let Some(u) = utts.get(i) {
                let start = audio.len();
                audio.extend_from_slice(u);
                turns.push((spk.to_owned(), start..audio.len()));
                audio.extend(std::iter::repeat_n(0.0, gap));
            }
        }
    }
    (audio, turns)
}

fn secs(s: usize) -> f64 {
    s as f64 / f64::from(PIPELINE_RATE.hz())
}

fn diarize(audio: &[f32], clustering: ClusteringConfig) -> Diarization {
    let cfg = DiarizationConfig {
        clustering,
        ..DiarizationConfig::default()
    };
    let mut d = Diarizer::new(segmenter(), embedder(), cfg).expect("diarizer");
    let buf = MonoBuffer::new(PIPELINE_RATE, audio.to_vec());
    d.diarize(&buf, MeetingTime::ZERO, &CancelToken::new())
        .expect("diarize")
}

/// Per reference utterance, the hypothesis speaker holding most of its speech (None if none).
fn majority_speakers(d: &Diarization, turns: &[(String, Range<usize>)]) -> Vec<Option<usize>> {
    turns
        .iter()
        .map(|(_, r)| {
            let (s, e) = (secs(r.start), secs(r.end));
            let mut per = vec![0.0f64; d.speakers.len()];
            for t in &d.turns {
                let (ts, te) = (
                    t.start.as_millis() as f64 / 1000.0,
                    t.end.as_millis() as f64 / 1000.0,
                );
                per[t.speaker] += (te.min(e) - ts.max(s)).max(0.0);
            }
            (0..per.len())
                .filter(|&k| per[k] > 0.0)
                .max_by(|&x, &y| per[x].total_cmp(&per[y]))
        })
        .collect()
}

fn report(name: &str, d: &Diarization, turns: &[(String, Range<usize>)]) -> Vec<Option<usize>> {
    let reference: Vec<Turn> = turns
        .iter()
        .map(|(s, r)| Turn::new(secs(r.start), secs(r.end), s.clone()))
        .collect();
    let hyp = d.der_turns();
    let nist = der(&reference, &hyp, &DerConfig::nist(AMI_COLLAR_PER_SIDE_S));
    let none = der(&reference, &hyp, &DerConfig::nist(0.0));
    let majority = majority_speakers(d, turns);
    println!(
        "diar {name}: speakers {}; DER (0.25 s collar) {:.4} [miss {:.2} s, fa {:.2} s, \
         confusion {:.2} s of {:.2} s]; DER (no collar) {:.4}; utterance majority {:?}",
        d.speakers.len(),
        nist.der(),
        nist.missed,
        nist.false_alarm,
        nist.confusion,
        nist.total,
        none.der(),
        majority
    );
    majority
}

/// Accept: the default pipeline (pyannote 3.1 parameters; the clustering threshold is
/// WeSpeaker-tuned and is recalibrated for CAM++ on dev-clean in T3.5) finds exactly two
/// speakers, and every utterance's speech goes mostly to the hypothesis speaker of its own
/// reference speaker, with the two reference speakers on distinct hypothesis speakers.
#[test]
fn diar_two_speaker_smoke_mix_is_split_by_the_default_pipeline() {
    let (audio, turns) = two_speaker_mix();
    let d = diarize(&audio, ClusteringConfig::default());
    let majority = report("default pipeline", &d, &turns);
    assert!(
        d.embedding_model
            .as_str()
            .starts_with("campplus-en-voxceleb@")
    );
    assert_eq!(d.speakers.len(), 2, "exactly two speakers");
    let label_of = |spk: &str| {
        turns
            .iter()
            .zip(&majority)
            .find(|((s, _), _)| s == spk)
            .and_then(|(_, m)| *m)
            .expect("first utterance attributed")
    };
    let (h61, h121) = (label_of("61"), label_of("121"));
    assert_ne!(h61, h121, "the speakers must map to distinct clusters");
    for ((spk, r), m) in turns.iter().zip(&majority) {
        let want = if spk == "61" { h61 } else { h121 };
        assert_eq!(
            *m,
            Some(want),
            "utterance of {spk} at {:.1} s attributed to {m:?}",
            secs(r.start)
        );
    }
}

#[test]
fn diar_campplus_separates_the_smoke_speakers() {
    let (a, b) = common::smoke_speech();
    let mut e = embedder();
    println!("campplus min_samples {}", e.info().min_samples);
    let cancel = CancelToken::new();
    let ea = e
        .embed(&a.iter().map(Vec::as_slice).collect::<Vec<_>>(), &cancel)
        .unwrap();
    let eb = e
        .embed(&b.iter().map(Vec::as_slice).collect::<Vec<_>>(), &cancel)
        .unwrap();
    assert!(
        ea.iter()
            .chain(&eb)
            .all(|x| x.vector.len() == CAMPPLUS_DIM && x.is_valid())
    );
    let cos = |x: &earshot_core::diar::Embedding, y: &earshot_core::diar::Embedding| {
        x.cosine_similarity(y).unwrap()
    };
    let mut same = Vec::new();
    for g in [&ea, &eb] {
        for i in 0..g.len() {
            for j in i + 1..g.len() {
                same.push(cos(&g[i], &g[j]));
            }
        }
    }
    let cross: Vec<f32> = ea
        .iter()
        .flat_map(|x| eb.iter().map(|y| cos(x, y)))
        .collect();
    let min_same = same.iter().copied().fold(f32::INFINITY, f32::min);
    let max_cross = cross.iter().copied().fold(f32::NEG_INFINITY, f32::max);
    let mean = |v: &[f32]| v.iter().sum::<f32>() / v.len() as f32;
    println!(
        "campplus cosine: same-speaker min {min_same:.3} mean {:.3}, cross-speaker max {max_cross:.3} mean {:.3}",
        mean(&same),
        mean(&cross)
    );
    // Leave-one-out: every utterance is closer on average to the other utterances of its
    // own speaker than to the other speaker's.
    for (own, other) in [(&ea, &eb), (&eb, &ea)] {
        for (i, x) in own.iter().enumerate() {
            let s: Vec<f32> = own
                .iter()
                .enumerate()
                .filter(|&(j, _)| j != i)
                .map(|(_, y)| cos(x, y))
                .collect();
            let c: Vec<f32> = other.iter().map(|y| cos(x, y)).collect();
            assert!(
                mean(&s) > mean(&c),
                "utterance {i}: own {:.3} other {:.3}",
                mean(&s),
                mean(&c)
            );
        }
    }
}

#[test]
fn diar_campplus_reports_its_model_id_and_rejects_short_regions() {
    let mut e = embedder();
    let info = e.info().clone();
    assert_eq!(info.dimension, CAMPPLUS_DIM);
    let id = info.model.as_str();
    let hash = id.strip_prefix("campplus-en-voxceleb@").expect(id);
    assert!(
        hash.len() == 12 && hash.bytes().all(|b| b.is_ascii_hexdigit()),
        "{id}"
    );
    // The manifest pins sha256 357a834f702b...; the id carries its prefix.
    assert_eq!(hash, "357a834f702b");
    assert!(info.min_samples >= 400 && info.min_samples <= 16_000);
    let short = vec![0.01f32; info.min_samples - 1];
    assert!(matches!(
        e.embed(&[&short], &CancelToken::new()),
        Err(DiarError::Config(_))
    ));
    let cancel = CancelToken::new();
    cancel.cancel();
    let ok = vec![0.01f32; info.min_samples];
    assert_eq!(e.embed(&[&ok], &cancel).unwrap_err(), DiarError::Cancelled);
}

#[test]
fn diar_segmentation_marks_speech_and_not_silence() {
    let mut s = segmenter();
    let info = s.info().clone();
    let (a, _) = common::smoke_speech();
    let mut speech = a[0].clone();
    speech.resize(info.window, 0.0);
    let silence = vec![0.0f32; info.window];
    let out = s
        .segment(&[&speech, &silence], &CancelToken::new())
        .unwrap();
    assert_eq!(out.len(), 2);
    let active = |w: usize| {
        (0..out[w].frames())
            .filter(|&f| (0..out[w].speakers()).any(|k| out[w].get(f, k) > 0.5))
            .count()
    };
    println!(
        "segmentation active frames: speech {}, silence {}",
        active(0),
        active(1)
    );
    assert_eq!(active(1), 0, "silence has no speaker");
    assert!(active(0) > 0, "speech has a speaker");
    // No activity well after the clip ends (the rest of the window is zero padding).
    let end = a[0].len() + PIPELINE_RATE.hz() as usize / 2;
    for f in 0..out[0].frames() {
        if info.frame_start(f) > end {
            assert!(
                (0..out[0].speakers()).all(|k| out[0].get(f, k) == 0.0),
                "frame {f} active in the padding"
            );
        }
    }
    println!("segmentation backend: {}", s.backend());
}

#[test]
fn diar_segmenter_rejects_wrong_window_and_honours_cancel() {
    let mut s = segmenter();
    let short = vec![0.0f32; 100];
    assert!(matches!(
        s.segment(&[&short], &CancelToken::new()),
        Err(DiarError::Config(_))
    ));
    let w = vec![0.0f32; s.info().window];
    let cancel = CancelToken::new();
    cancel.cancel();
    assert_eq!(s.segment(&[&w], &cancel).unwrap_err(), DiarError::Cancelled);
}

#[test]
fn diar_segmenter_reports_a_model_id_with_the_file_hash() {
    let id = segmenter().info().model.clone();
    let hash = id.strip_prefix("pyannote-segmentation-3.0@").expect(&id);
    // The manifest pins model.onnx at sha256 220ad67ca923...; the id carries its prefix.
    assert_eq!(hash, "220ad67ca923", "{id}");
}

/// T6.13d2: the final pass reuses the live workers' speaker windows only if that cannot
/// change a result. Measured: segmentation activity is the same bit for bit at every thread
/// count tried, CAM++ embeddings are not (ONNX Runtime splits reductions by thread), so the
/// live and final-pass speaker sessions run with the same thread count
/// (`earshot_runtime::backends::speaker_threads`) and reuse requires it. This pins that a
/// fresh session with the same thread count gives the same output bit for bit, and prints
/// how far other thread counts move the embeddings.
#[test]
fn diar_cpu_outputs_are_identical_at_the_same_thread_count() {
    common::init_runtime();
    let load = |threads: usize| {
        let config = SessionConfig {
            provider: earshot_onnx::Provider::Cpu,
            intra_threads: Some(threads),
            background: None,
        };
        (
            PyannoteSegmenter::load(&common::model(common::SEGMENTATION), &config, 8)
                .expect("load segmentation"),
            CamppEmbedder::load(&common::model(common::CAMPPLUS), &config).expect("load CAM++"),
        )
    };
    let (mix, _) = two_speaker_mix();
    let window = load(1).0.info().window;
    // Four windows across the mix (zero-padded at the end) and regions of several lengths.
    let windows: Vec<Vec<f32>> = (0..4)
        .map(|k| {
            let start = (k * mix.len() / 4).min(mix.len());
            let mut w = mix[start..(start + window).min(mix.len())].to_vec();
            w.resize(window, 0.0);
            w
        })
        .collect();
    let regions: Vec<Vec<f32>> = [8_000usize, 23_456, 64_000, 160_000]
        .iter()
        .enumerate()
        .map(|(k, &n)| {
            let start = (k * 37_000).min(mix.len() - n);
            mix[start..start + n].to_vec()
        })
        .collect();
    let run = |threads: usize| {
        let (mut seg, mut emb) = load(threads);
        let w: Vec<&[f32]> = windows.iter().map(Vec::as_slice).collect();
        let acts = seg.segment(&w, &CancelToken::new()).expect("segment");
        let act_bits: Vec<Vec<u32>> = acts
            .iter()
            .map(|a| {
                (0..a.frames())
                    .flat_map(|f| (0..a.speakers()).map(move |k| (f, k)))
                    .map(|(f, k)| a.get(f, k).to_bits())
                    .collect()
            })
            .collect();
        let r: Vec<&[f32]> = regions.iter().map(Vec::as_slice).collect();
        let embs: Vec<Vec<f32>> = emb
            .embed(&r, &CancelToken::new())
            .expect("embed")
            .into_iter()
            .map(|e| e.vector)
            .collect();
        (act_bits, embs)
    };
    let bits = |e: &[Vec<f32>]| -> Vec<Vec<u32>> {
        e.iter()
            .map(|v| v.iter().map(|x| x.to_bits()).collect())
            .collect()
    };
    for threads in [1usize, 2, 3, 4] {
        let (a, ea) = run(threads);
        let (b, eb) = run(threads);
        assert!(
            a == b,
            "segmentation differs between two {threads}-thread sessions"
        );
        assert!(
            bits(&ea) == bits(&eb),
            "CAM++ differs between two {threads}-thread sessions"
        );
    }
    let (seg1, emb1) = run(1);
    for threads in [2usize, 3, 4, 8] {
        let (seg, emb) = run(threads);
        let max_diff = emb
            .iter()
            .zip(&emb1)
            .flat_map(|(x, y)| x.iter().zip(y).map(|(a, b)| (a - b).abs()))
            .fold(0.0f32, f32::max);
        println!(
            "diar threads {threads} vs 1: segmentation identical {}, CAM++ identical {}, largest component difference {max_diff:e}",
            seg == seg1,
            bits(&emb) == bits(&emb1)
        );
    }
}

/// Pool threads started by background sessions (`diar_background_sessions_*`).
static BACKGROUND_STARTS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

fn count_background_start() {
    BACKGROUND_STARTS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
}

/// Segmentation activity and CAM++ embeddings (as bits) of four windows and four regions
/// of the two-speaker mix, from sessions built with `config`.
fn speaker_output_bits(config: &SessionConfig) -> (Vec<Vec<u32>>, Vec<Vec<u32>>) {
    let mut seg = PyannoteSegmenter::load(&common::model(common::SEGMENTATION), config, 8)
        .expect("load segmentation");
    let mut emb = CamppEmbedder::load(&common::model(common::CAMPPLUS), config).expect("CAM++");
    let (mix, _) = two_speaker_mix();
    let window = seg.info().window;
    let windows: Vec<Vec<f32>> = (0..4)
        .map(|k| {
            let start = (k * mix.len() / 4).min(mix.len());
            let mut w = mix[start..(start + window).min(mix.len())].to_vec();
            w.resize(window, 0.0);
            w
        })
        .collect();
    let regions: Vec<Vec<f32>> = [8_000usize, 23_456, 64_000, 160_000]
        .iter()
        .enumerate()
        .map(|(k, &n)| {
            let start = (k * 37_000).min(mix.len() - n);
            mix[start..start + n].to_vec()
        })
        .collect();
    let w: Vec<&[f32]> = windows.iter().map(Vec::as_slice).collect();
    let acts = seg.segment(&w, &CancelToken::new()).expect("segment");
    let act_bits = acts
        .iter()
        .map(|a| {
            (0..a.frames())
                .flat_map(|f| (0..a.speakers()).map(move |k| (f, k)))
                .map(|(f, k)| a.get(f, k).to_bits())
                .collect()
        })
        .collect();
    let r: Vec<&[f32]> = regions.iter().map(Vec::as_slice).collect();
    let emb_bits = emb
        .embed(&r, &CancelToken::new())
        .expect("embed")
        .into_iter()
        .map(|e| e.vector.iter().map(|x| x.to_bits()).collect())
        .collect();
    (act_bits, emb_bits)
}

/// T6.13d3: live speaker models run as background sessions (pool threads lowered in
/// priority by a start function, no spin-waiting) while the final pass uses normal ones;
/// reuse is bit-exact only if that changes nothing. Pins that both give the same output bit
/// for bit at every speaker thread count, and that the start function runs on the pool
/// threads of a multi-threaded background session.
#[test]
fn diar_background_sessions_equal_normal_sessions_bit_for_bit() {
    use std::sync::atomic::Ordering::SeqCst;
    common::init_runtime();
    for threads in [1usize, 2, 3, 4] {
        let normal = SessionConfig {
            provider: earshot_onnx::Provider::Cpu,
            intra_threads: Some(threads),
            background: None,
        };
        let background = SessionConfig {
            background: Some(earshot_onnx::ThreadStart(count_background_start)),
            ..normal.clone()
        };
        let before = BACKGROUND_STARTS.load(SeqCst);
        let b = speaker_output_bits(&background);
        let started = BACKGROUND_STARTS.load(SeqCst) - before;
        let a = speaker_output_bits(&normal);
        assert!(a.0 == b.0, "segmentation differs at {threads} thread(s)");
        assert!(a.1 == b.1, "CAM++ differs at {threads} thread(s)");
        // ONNX Runtime runs an n-thread pool on n - 1 threads of its own plus the caller
        // (whose priority the caller sets): every one of them, in both sessions, ran it.
        assert_eq!(
            started,
            2 * (threads - 1),
            "pool threads that ran the start function at {threads} thread(s)"
        );
    }
}
