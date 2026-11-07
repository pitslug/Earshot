//! Open-set voiceprint threshold calibration for CAM++ on LibriSpeech **dev-clean only**
//! (PLAN.md 4.0, T3.4): `cargo test -p earshot-onnx voiceprint_calibration`.
//!
//! Protocol (fixed before any acceptance run; test-clean is never read here):
//! - the 40 dev-clean speakers of `fixtures/librispeech/subset.toml`, sorted by id;
//! - two folds: the first 20 enrolled and the last 20 unknown, then swapped;
//! - enrolment: every `enrol` utterance (>= 10 s per speaker) embedded whole and added to the
//!   speaker's voiceprint with core `voiceprint::enroll` (running centroid);
//! - test clips: the centre 2.5 s of every `test` utterance (3 per speaker, each >= 3 s);
//! - impostor scores: for each unknown speaker's clip, its best cosine score over the
//!   enrolled voiceprints; threshold = `calibrate_threshold(impostors,
//!   DEV_TARGET_FALSE_ACCEPT)`.
//!
//! The test fails if the committed `CAMPPLUS_DEV_CLEAN_THRESHOLD` differs from the fresh
//! calibration, and reports the dev-clean top-1 / false-reject numbers at that threshold.
//!
//! `voiceprint_calibration_highpass_leaves_voiceprints_unchanged` (T6.23b, ignored: run it
//! with `cargo test --release -p earshot-onnx voiceprint_calibration_highpass --
//! --include-ignored`) runs the same protocol with every utterance passed through each
//! echo-canceller high-pass candidate (private notes rule 1d) and asserts the rule for
//! the production default.

mod common;

use std::path::PathBuf;
use std::time::Duration;

use earshot_core::aec::{BLOCK, EchoCanceller, EchoCancellerConfig, HighPass, HighPassFilter};
use earshot_core::asr::CancelToken;
use earshot_core::diar::{Embedding, SpeakerEmbedder};
use earshot_core::identity::{DeviceId, EntityId, Hlc, ManualClock};
use earshot_core::voiceprint::{
    CAMPPLUS_DEV_CLEAN_THRESHOLD, DEV_TARGET_FALSE_ACCEPT, EnrolmentConfig, InMemoryVoiceprintRepo,
    MatchOutcome, Person, Stamp, VoiceprintRepo, calibrate_threshold, calibrated_threshold, enroll,
    match_embedding,
};
use earshot_onnx::SessionConfig;
use earshot_onnx::embed::CamppEmbedder;
use rand_chacha::ChaCha8Rng;
use rand_core::SeedableRng;
use serde::Deserialize;

/// Test clip length (PLAN.md 4.0: 2-3 s test clips).
const CLIP: usize = 40_000;
const SPLIT: &str = "dev-clean";

#[derive(Deserialize)]
struct Subset {
    utterance: Vec<Utt>,
}

#[derive(Deserialize)]
struct Utt {
    split: String,
    speaker: u32,
    id: String,
    role: String,
    seconds: f64,
}

/// Fixture directory: `EARSHOT_FIXTURES`, else `%LOCALAPPDATA%\earshot-dev\fixtures`
/// (mirrors `xtask::fixtures`).
fn fixtures_dir() -> PathBuf {
    if let Some(d) = std::env::var_os("EARSHOT_FIXTURES").filter(|v| !v.is_empty()) {
        return PathBuf::from(d);
    }
    PathBuf::from(std::env::var_os("LOCALAPPDATA").expect("LOCALAPPDATA"))
        .join("earshot-dev")
        .join("fixtures")
}

fn load_utt(u: &Utt) -> Vec<f32> {
    let parts: Vec<&str> = u.id.split('-').collect();
    let path = fixtures_dir()
        .join("librispeech")
        .join(&u.split)
        .join("LibriSpeech")
        .join(&u.split)
        .join(parts[0])
        .join(parts[1])
        .join(format!("{}.flac", u.id));
    assert!(
        path.is_file(),
        "{} missing; run `cargo xtask fetch-fixtures --set librispeech`",
        path.display()
    );
    let mut r = claxon::FlacReader::open(&path).expect("flac");
    assert_eq!(r.streaminfo().sample_rate, 16_000);
    let scale = 1.0 / f64::from(1u32 << (r.streaminfo().bits_per_sample - 1));
    r.samples()
        .map(|s| (f64::from(s.expect("sample")) * scale) as f32)
        .collect()
}

fn centre_clip(x: &[f32]) -> &[f32] {
    assert!(x.len() >= CLIP, "test utterance shorter than the clip");
    let start = (x.len() - CLIP) / 2;
    &x[start..start + CLIP]
}

struct Speaker {
    id: u32,
    enrol: Vec<Embedding>,
    enrol_seconds: f64,
    tests: Vec<Embedding>,
}

fn embed_dev_clean() -> Vec<Speaker> {
    embed_dev_clean_with(&|x: &[f32]| x.to_vec())
}

/// Embeds every dev-clean utterance after `transform` (applied to the whole utterance,
/// before the test clip is cut, as a stream would be filtered).
fn embed_dev_clean_with(transform: &dyn Fn(&[f32]) -> Vec<f32>) -> Vec<Speaker> {
    let path = common::repo_root().join("fixtures/librispeech/subset.toml");
    let subset: Subset =
        toml::from_str(&std::fs::read_to_string(&path).expect("subset.toml")).expect("parse");
    common::init_runtime();
    let mut e = CamppEmbedder::load(&common::model(common::CAMPPLUS), &SessionConfig::cpu(1))
        .expect("CAM++");
    let cancel = CancelToken::new();
    let mut ids: Vec<u32> = subset
        .utterance
        .iter()
        .filter(|u| u.split == SPLIT)
        .map(|u| u.speaker)
        .collect();
    ids.sort_unstable();
    ids.dedup();
    assert_eq!(ids.len(), 40, "dev-clean subset has 40 speakers");
    ids.iter()
        .map(|&id| {
            let mut s = Speaker {
                id,
                enrol: Vec::new(),
                enrol_seconds: 0.0,
                tests: Vec::new(),
            };
            for u in subset
                .utterance
                .iter()
                .filter(|u| u.split == SPLIT && u.speaker == id)
            {
                let audio = transform(&load_utt(u));
                match u.role.as_str() {
                    "enrol" => {
                        s.enrol.extend(e.embed(&[&audio], &cancel).unwrap());
                        s.enrol_seconds += u.seconds;
                    }
                    "test" => s
                        .tests
                        .extend(e.embed(&[centre_clip(&audio)], &cancel).unwrap()),
                    other => panic!("role {other}"),
                }
            }
            assert!(
                s.enrol_seconds >= 10.0 && s.tests.len() == 3,
                "speaker {id}"
            );
            s
        })
        .collect()
}

/// Scores of one run of the protocol.
struct Scores {
    /// Each unknown clip's best score over the enrolled voiceprints.
    impostor: Vec<f32>,
    /// Each known clip: (correct top-1, best score).
    known: Vec<(bool, f32)>,
    /// Every (test clip, enrolled voiceprint) score of both folds, in a fixed order.
    pairs: Vec<f32>,
}

fn protocol(speakers: &[Speaker]) -> Scores {
    let mut impostor = Vec::new();
    let mut known: Vec<(bool, f32)> = Vec::new();
    let mut pairs = Vec::new();
    for fold in 0..2 {
        let (enrolled, unknown) = if fold == 0 {
            speakers.split_at(20)
        } else {
            let (a, b) = speakers.split_at(20);
            (b, a)
        };
        let clock = ManualClock::new(1_800_000_000_000);
        let mut rng = ChaCha8Rng::seed_from_u64(fold as u64);
        let mut hlc = Hlc::new(DeviceId::generate(&clock, &mut rng));
        let mut repo = InMemoryVoiceprintRepo::new();
        let mut person_of: Vec<(u32, EntityId)> = Vec::new();
        for s in enrolled {
            let mut stamp = Stamp {
                clock: &clock,
                hlc: &mut hlc,
                rng: &mut rng,
            };
            let p = Person::new(format!("dev-{}", s.id), false, &mut stamp);
            repo.put_person(p.clone()).unwrap();
            enroll(
                &mut repo,
                p.id,
                &s.enrol,
                Duration::from_secs_f64(s.enrol_seconds),
                &EnrolmentConfig::default(),
                &mut stamp,
            )
            .unwrap();
            person_of.push((s.id, p.id));
        }
        // Scores only; the threshold is applied after calibration (-inf accepts all).
        for s in unknown {
            for t in &s.tests {
                match match_embedding(&repo, t, f32::NEG_INFINITY).unwrap() {
                    MatchOutcome::Known { score, .. } => impostor.push(score),
                    MatchOutcome::Unknown { .. } => unreachable!(),
                }
            }
        }
        for s in enrolled {
            let me = person_of.iter().find(|(id, _)| *id == s.id).unwrap().1;
            for t in &s.tests {
                match match_embedding(&repo, t, f32::NEG_INFINITY).unwrap() {
                    MatchOutcome::Known { person, score } => known.push((person == me, score)),
                    MatchOutcome::Unknown { .. } => unreachable!(),
                }
            }
        }
        for (_, person) in &person_of {
            let prints = repo.voiceprints_of(*person).unwrap();
            for s in speakers {
                for t in &s.tests {
                    for v in &prints {
                        pairs.push(v.score(t).unwrap());
                    }
                }
            }
        }
    }
    Scores {
        impostor,
        known,
        pairs,
    }
}

#[test]
fn voiceprint_calibration_campplus_dev_clean_threshold_is_reproducible() {
    let speakers = embed_dev_clean();
    let Scores {
        impostor, known, ..
    } = protocol(&speakers);
    assert_eq!(impostor.len(), 120);
    let t = calibrate_threshold(&impostor, DEV_TARGET_FALSE_ACCEPT);
    let fa = impostor.iter().filter(|&&s| s >= t).count();
    let top1 = known.iter().filter(|(ok, _)| *ok).count();
    let accepted_correct = known.iter().filter(|(ok, s)| *ok && *s >= t).count();
    println!(
        "voiceprint calibration (dev-clean, CAM++): threshold {t:?}; unknown false accepts \
         {fa}/{} ({:.2}%); known clips top-1 {top1}/{} ({:.2}%), top-1 and accepted \
         {accepted_correct}/{} ({:.2}%)",
        impostor.len(),
        100.0 * fa as f64 / impostor.len() as f64,
        known.len(),
        100.0 * top1 as f64 / known.len() as f64,
        known.len(),
        100.0 * accepted_correct as f64 / known.len() as f64
    );
    assert!(
        (t - CAMPPLUS_DEV_CLEAN_THRESHOLD).abs() <= 1e-4,
        "committed CAMPPLUS_DEV_CLEAN_THRESHOLD {CAMPPLUS_DEV_CLEAN_THRESHOLD} differs from \
         the dev-clean calibration {t:.6}"
    );
    // The constant is registered for the exact model id the embedder reports.
    let e = CamppEmbedder::load(&common::model(common::CAMPPLUS), &SessionConfig::cpu(1))
        .expect("CAM++");
    assert_eq!(
        calibrated_threshold(&e.info().model).unwrap(),
        CAMPPLUS_DEV_CLEAN_THRESHOLD
    );
}

/// Largest change of a (test clip, voiceprint) score a high-pass may cause (private notes
/// rule 1d, fixed before measuring).
const MAX_PAIR_SCORE_CHANGE: f32 = 0.05;

/// The audio a speaker embedding sees under `highpass`: the high-pass itself (Audio), or the
/// canceller with a silent reference (Internal: the matching all-pass; Decisions: the mic
/// itself; latency removed).
fn through(highpass: HighPass, x: &[f32]) -> Vec<f32> {
    match highpass {
        HighPass::Off => x.to_vec(),
        HighPass::Audio { cutoff_hz } => HighPassFilter::new(cutoff_hz, 16_000).filtered(x),
        HighPass::Internal { .. } | HighPass::Decisions { .. } => {
            let mut aec = EchoCanceller::new(EchoCancellerConfig {
                highpass,
                ..EchoCancellerConfig::default()
            });
            let lat = aec.output_latency();
            let n = (x.len() + lat).div_ceil(BLOCK) * BLOCK;
            let mut mic = x.to_vec();
            mic.resize(n, 0.0);
            let out = aec.process_vec(&mic, &vec![0.0; n]);
            out[lat..lat + x.len()].to_vec()
        }
    }
}

#[test]
#[ignore = "T6.23b dev-data check (embeds dev-clean once per variant): cargo test --release -p earshot-onnx voiceprint_calibration_highpass -- --include-ignored"]
fn voiceprint_calibration_highpass_leaves_voiceprints_unchanged() {
    let t = CAMPPLUS_DEV_CLEAN_THRESHOLD;
    let counts = |s: &Scores| {
        (
            s.known.iter().filter(|(ok, _)| *ok).count(),
            s.impostor.iter().filter(|&&x| x >= t).count(),
        )
    };
    let base = protocol(&embed_dev_clean());
    let (top1, fa) = counts(&base);
    println!(
        "voiceprint highpass: unfiltered top-1 {top1}/{}, false accepts {fa}/{}",
        base.known.len(),
        base.impostor.len()
    );
    let default = EchoCancellerConfig::default().highpass;
    let mut checked_default = default == HighPass::Off;
    for label in [
        "A80", "A100", "A120", "I80", "I100", "D80", "D100", "D80w", "D100w", "A20", "A30", "A40",
        "D30w",
    ] {
        let hp = HighPass::parse(label).unwrap();
        let s = protocol(&embed_dev_clean_with(&|x: &[f32]| through(hp, x)));
        let (v_top1, v_fa) = counts(&s);
        assert_eq!(s.pairs.len(), base.pairs.len());
        let d: Vec<f32> = s
            .pairs
            .iter()
            .zip(&base.pairs)
            .map(|(a, b)| (a - b).abs())
            .collect();
        let max = d.iter().copied().fold(0.0f32, f32::max);
        let mean = d.iter().sum::<f32>() / d.len() as f32;
        let ok = v_top1 == top1 && v_fa == fa && max <= MAX_PAIR_SCORE_CHANGE;
        println!(
            "voiceprint highpass: {label}: top-1 {v_top1}/{}, false accepts {v_fa}/{}, pair score change max {max:.4} mean {mean:.5} over {} pairs -> {}",
            s.known.len(),
            s.impostor.len(),
            d.len(),
            if ok { "unchanged" } else { "CHANGED" }
        );
        if hp == default {
            checked_default = true;
            assert!(ok, "the default high-pass {label} changes voiceprints");
        }
    }
    assert!(
        checked_default,
        "the default high-pass {} is not a measured candidate",
        default.label()
    );
}
