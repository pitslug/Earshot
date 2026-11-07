//! Test-only helpers: load the committed smoke fixtures and the AEC scenario file.
//! Compiled only for `cfg(test)`; the core purity contract covers the lib target, and core
//! tests may read fixtures (PLAN.md 3.2).

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};

use rand_chacha::ChaCha8Rng;
use rand_core::SeedableRng;

use crate::sim::{Scenario, ScenarioFile, SimOutput, SpeechPool, simulate};

/// Repository root (two levels above this crate).
pub fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn decode_flac(path: &std::path::Path) -> Vec<f32> {
    let mut reader = claxon::FlacReader::open(path)
        .unwrap_or_else(|e| panic!("cannot open {}: {e}", path.display()));
    let info = reader.streaminfo();
    assert_eq!(info.sample_rate, 16_000, "{}", path.display());
    assert_eq!(info.channels, 1, "{}", path.display());
    let scale = 1.0 / f64::from(1u32 << (info.bits_per_sample - 1));
    reader
        .samples()
        .map(|s| (f64::from(s.expect("flac sample")) * scale) as f32)
        .collect()
}

/// Far-end and near-end utterance lists.
pub type SpeakerUtterances = (Vec<Vec<f32>>, Vec<Vec<f32>>);

/// Smoke utterances split by speaker: far end = speaker 61, near end = speaker 121.
pub fn smoke_speech() -> &'static SpeakerUtterances {
    static SPEECH: OnceLock<SpeakerUtterances> = OnceLock::new();
    SPEECH.get_or_init(|| {
        let dir = repo_root().join("fixtures/smoke");
        let mut names: Vec<_> = std::fs::read_dir(&dir)
            .unwrap_or_else(|e| panic!("cannot list {}: {e}", dir.display()))
            .map(|e| {
                e.expect("dir entry")
                    .file_name()
                    .into_string()
                    .expect("utf-8")
            })
            .filter(|n| n.ends_with(".flac"))
            .collect();
        names.sort();
        let (mut far, mut near) = (Vec::new(), Vec::new());
        for name in names {
            let audio = decode_flac(&dir.join(&name));
            if name.starts_with("61-") {
                far.push(audio);
            } else if name.starts_with("121-") {
                near.push(audio);
            }
        }
        assert!(
            !far.is_empty() && !near.is_empty(),
            "smoke speakers missing"
        );
        (far, near)
    })
}

/// One dev-wide speaker pair of the echo-canceller evaluation (T1.6c).
pub struct DevWidePair {
    /// Far-end speaker id.
    pub far: u32,
    /// Near-end speaker id.
    pub near: u32,
    /// Far-end and near-end utterances.
    pub speech: SpeakerUtterances,
}

/// Fixture cache: `EARSHOT_FIXTURES`, else `%LOCALAPPDATA%\earshot-dev\fixtures` (mirrors
/// `xtask::fixtures::cache_dir`).
fn fixture_cache() -> PathBuf {
    if let Some(d) = std::env::var_os("EARSHOT_FIXTURES").filter(|v| !v.is_empty()) {
        return PathBuf::from(d);
    }
    PathBuf::from(std::env::var_os("LOCALAPPDATA").unwrap_or_default())
        .join("earshot-dev")
        .join("fixtures")
}

/// The dev-wide speech of `fixtures/librispeech/aec_dev.toml` (dev-clean voices, read from
/// the fixture cache), one entry per listed pair in file order. Missing audio fails with the
/// fetch instruction (a fetchable fixture is never a silent skip).
pub fn dev_wide_speech() -> &'static [DevWidePair] {
    static PAIRS: OnceLock<Vec<DevWidePair>> = OnceLock::new();
    PAIRS.get_or_init(|| {
        let path = repo_root().join("fixtures/librispeech/aec_dev.toml");
        let text = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()));
        let table: toml::Table =
            toml::from_str(&text).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
        let cache = fixture_cache();
        let mut by_speaker: HashMap<i64, Vec<Vec<f32>>> = HashMap::new();
        for u in table["utterance"].as_array().expect("utterance list") {
            let id = u["id"].as_str().expect("utterance id");
            let parts: Vec<&str> = id.split('-').collect();
            let file = cache
                .join("librispeech/dev-clean/LibriSpeech/dev-clean")
                .join(parts[0])
                .join(parts[1])
                .join(format!("{id}.flac"));
            assert!(
                file.is_file(),
                "dev-wide AEC speech {} is missing: run `cargo xtask fetch-fixtures --set librispeech` (or set EARSHOT_FIXTURES)",
                file.display()
            );
            // Size only: core has no SHA-256 helper (sha2 is not a core dependency);
            // `cargo xtask fetch-fixtures --verify` and `accept m1` check the hashes.
            let size = std::fs::metadata(&file).map_or(0, |m| m.len());
            let pinned = u["size"].as_integer().and_then(|v| u64::try_from(v).ok());
            assert_eq!(
                Some(size),
                pinned,
                "{} differs from its pinned size in aec_dev.toml: run `cargo xtask fetch-fixtures --set librispeech --verify`",
                file.display()
            );
            by_speaker
                .entry(u["speaker"].as_integer().expect("speaker"))
                .or_default()
                .push(decode_flac(&file));
        }
        table["pair"]
            .as_array()
            .expect("pair list")
            .iter()
            .map(|p| {
                let (far, near) = (
                    p["far"].as_integer().expect("far"),
                    p["near"].as_integer().expect("near"),
                );
                DevWidePair {
                    far: u32::try_from(far).expect("speaker id"),
                    near: u32::try_from(near).expect("speaker id"),
                    speech: (by_speaker[&far].clone(), by_speaker[&near].clone()),
                }
            })
            .collect()
    })
}

/// Simulates `scn` with the speech of a dev-wide pair at the scenario's own seed plus
/// [`crate::sim::DEV_WIDE_SEED_OFFSET`] (as `aec_eval --devwide` does).
pub fn run_dev_wide(base: &Scenario, pair: &DevWidePair) -> SimOutput {
    let scn = Scenario {
        seed: base
            .seed
            .checked_add(crate::sim::DEV_WIDE_SEED_OFFSET)
            .expect("dev-wide seed overflows"),
        ..base.clone()
    };
    let (far, near) = &pair.speech;
    let pool = SpeechPool { far, near };
    let mut rng = ChaCha8Rng::seed_from_u64(scn.seed);
    simulate(&scn, &scenarios().setup, &pool, &mut rng).expect("scenario simulates")
}

/// `acceptance/aec_scenarios.toml`, parsed.
pub fn scenarios() -> &'static ScenarioFile {
    static FILE: OnceLock<ScenarioFile> = OnceLock::new();
    FILE.get_or_init(|| {
        let path = repo_root().join("acceptance/aec_scenarios.toml");
        let text = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()));
        toml::from_str(&text).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
    })
}

/// Simulates the named scenario with its own seed (cached per test binary).
pub fn run_scenario(name: &str) -> SimOutput {
    static CACHE: OnceLock<Mutex<HashMap<String, Arc<SimOutput>>>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    if let Some(hit) = cache.lock().expect("cache lock").get(name) {
        return (**hit).clone();
    }
    let scn = scenarios()
        .get(name)
        .unwrap_or_else(|| panic!("no scenario {name}"));
    let out = Arc::new(run(scn));
    cache
        .lock()
        .expect("cache lock")
        .insert(name.to_string(), Arc::clone(&out));
    (*out).clone()
}

/// The value of threshold `id` in `acceptance/thresholds.toml`.
pub fn threshold(id: &str) -> f64 {
    static FILE: OnceLock<toml::Table> = OnceLock::new();
    let table = FILE.get_or_init(|| {
        let path = repo_root().join("acceptance/thresholds.toml");
        let text = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()));
        toml::from_str(&text).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
    });
    table["thresholds"][id]["value"]
        .as_float()
        .unwrap_or_else(|| panic!("threshold {id} has no numeric value"))
}

/// Simulates `scn` (for example a robustness variant) with its own seed.
pub fn run(scn: &Scenario) -> SimOutput {
    let (far, near) = smoke_speech();
    let pool = SpeechPool { far, near };
    let mut rng = ChaCha8Rng::seed_from_u64(scn.seed);
    simulate(scn, &scenarios().setup, &pool, &mut rng).expect("scenario simulates")
}

/// Stereo copy of `base` for the T1.5b tests (not a scenario of the acceptance file):
/// the right loudspeaker stands about 0.9 m further from the mic than the left (a
/// multipath spacing T1.2a found on a real recording), and the right
/// channel is the left one 1 ms later at weight `correlation` plus independent far-end
/// speech; `image_change_s` changes the far-end stereo image mid-call.
pub fn stereo_variant(base: &Scenario, correlation: f64, image_change_s: Option<f64>) -> Scenario {
    Scenario {
        name: format!("{}-stereo{correlation}", base.name),
        stereo: Some(crate::sim::Stereo {
            right_speaker: [2.9, 2.7, 0.9],
            correlation,
            lag_ms: 1.0,
            image_change_s,
        }),
        ..base.clone()
    }
}

/// Copies of `signals` with non-finite samples written into four consecutive blocks of
/// 128 from `at`: NaN, +inf, -inf, then NaN and infinity in one block (sample positions
/// spread over each block).
pub fn with_non_finite(signals: &[&[f32]], at: usize) -> Vec<Vec<f32>> {
    signals
        .iter()
        .map(|s| {
            let mut v = s.to_vec();
            for (b, bad) in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY, f32::NAN]
                .into_iter()
                .enumerate()
            {
                for i in [0, 17, 64, 127] {
                    v[at + b * 128 + i] = bad;
                }
                if b == 3 {
                    v[at + b * 128 + 5] = f32::INFINITY;
                }
            }
            v
        })
        .collect()
}
