//! M1 evaluation for `cargo xtask accept m1` (PLAN.md T1.6).
//!
//! `cargo run --release -p earshot-core --example aec_eval -- <repo-root> [--private <dir>]
//! [--heldout <fixture-cache>] [--heldout-only]`
//!
//! Simulates every scenario of `acceptance/aec_scenarios.toml` with the committed smoke
//! speech, runs the echo canceller, scores the `aec.*` metrics with
//! `earshot_core::aec::eval` against the values in `acceptance/thresholds.toml`, re-runs
//! the robustness grid, times the canceller (one thread, this release build) for
//! `aec.rtf`, and, when the GATE-10 recording exists in `<dir>`, reports its real-world
//! ERLE as informational output; likewise the GATE-10b stereo recording in `<dir>-stereo`
//! through the stereo canceller (T1.5b). Without `--private`, `<dir>` is `gate10.dir` of the
//! private settings file (`gate12_private`), under the repo root; with no file both reports
//! are a visible skip. Prints one JSON object on stdout.
//!
//! With `--heldout`, also renders the held-out set (T1.6b, `ScenarioFile::heldout_runs`)
//! with the speech pinned in `fixtures/librispeech/aec_heldout.toml`, read from the fixture
//! cache (the caller verifies its checksums), and reports every run's scores keyed by
//! metric id under `heldout`. `--heldout-only` prints only that object (the baseline
//! generator, `cargo xtask aec-heldout-baseline`). Held-out runs are scored by the same
//! code as the dev runs.
//!
//! With `--devwide`, also runs the dev-wide sweep (T1.6c): every gating scenario at
//! seed + `DEV_WIDE_SEED_OFFSET` with each speaker pair of `fixtures/librispeech/aec_dev.toml`
//! (dev-clean voices, read from the fixture cache), reported under `devwide`; dev data, never
//! gated. `--devwide-only` prints only that object (the tuning loop).
//!
//! `--rumble <set.toml>` (T6.23b dev mode) renders only the base scenarios of the rumble dev
//! set (`acceptance/aec_rumble_dev.toml`) with the smoke (dev) speech and their own seeds,
//! adds the measured low-frequency rumble (`sim::rumble::add_rumble`) and prints their scores
//! under `rumble`; never gating, never held-out.
//!
//! `--highpass off|A<hz>|I<hz>` runs every canceller with that `EchoCancellerConfig::highpass`
//! (T6.23b variant evaluation on dev data); the report names the variant under `highpass`.
//!
//! File access lives here, not in the core library (PLAN.md 3.2).

use std::fs::File;
use std::io::BufReader;
use std::path::{Path, PathBuf};
use std::time::Instant;

use earshot_core::aec::eval::{
    Limits, Run, Scores, classify, compensate, grid_convergence_informational, is_informational,
    score_for, worst,
};
use earshot_core::aec::{BLOCK, EchoCanceller, EchoCancellerConfig, HighPass};
use earshot_core::audio::{SampleRate, WavAudio, read_wav, resample_all};
use earshot_core::metrics::erle::{ErleConfig, segmental_erle};
use earshot_core::sim::rumble::{RumbleSet, add_rumble};
use earshot_core::sim::{
    DEV_WIDE_SEED_OFFSET, SECOND_SEED_OFFSET, Scenario, ScenarioFile, SimOutput, SpeechPool,
    recovery_window_violations, simulate,
};
use rand_chacha::ChaCha8Rng;
use rand_core::SeedableRng;
use serde_json::{Value, json};

#[path = "../../earshot-asr-whisper/examples/asr_eval/gate12_private.rs"]
mod gate12_private;

type Speech = (Vec<Vec<f32>>, Vec<Vec<f32>>);

/// Smoke (dev) speakers: far end, near end.
const DEV_SPEAKERS: [u32; 2] = [61, 121];

/// The canceller every run uses: the default, or with `--highpass <variant>` (T6.23b
/// evaluation of high-pass variants on dev data; `accept m1` never passes it).
static CONFIG: std::sync::OnceLock<EchoCancellerConfig> = std::sync::OnceLock::new();

fn canceller_config() -> EchoCancellerConfig {
    *CONFIG.get_or_init(EchoCancellerConfig::default)
}

fn fail(msg: String) -> ! {
    eprintln!("aec_eval: {msg}");
    std::process::exit(2);
}

fn read_flac(path: &Path) -> Vec<f32> {
    let mut reader = claxon::FlacReader::open(path)
        .unwrap_or_else(|e| fail(format!("cannot open {}: {e}", path.display())));
    let info = reader.streaminfo();
    if info.sample_rate != 16_000 || info.channels != 1 {
        fail(format!("{}: expected 16 kHz mono", path.display()));
    }
    let scale = 1.0 / f64::from(1u32 << (info.bits_per_sample - 1));
    reader
        .samples()
        .map(|s| {
            let v = s.unwrap_or_else(|e| fail(format!("{}: {e}", path.display())));
            (f64::from(v) * scale) as f32
        })
        .collect()
}

/// Smoke utterances: far end = speaker 61, near end = speaker 121 (as the core tests).
fn smoke_speech(root: &Path) -> Speech {
    let dir = root.join("fixtures").join("smoke");
    let mut names: Vec<String> = std::fs::read_dir(&dir)
        .unwrap_or_else(|e| fail(format!("cannot list {}: {e}", dir.display())))
        .filter_map(|e| e.ok()?.file_name().into_string().ok())
        .filter(|n| n.ends_with(".flac"))
        .collect();
    names.sort();
    let (mut far, mut near) = (Vec::new(), Vec::new());
    for name in names {
        let audio = read_flac(&dir.join(&name));
        if name.starts_with(&format!("{}-", DEV_SPEAKERS[0])) {
            far.push(audio);
        } else if name.starts_with(&format!("{}-", DEV_SPEAKERS[1])) {
            near.push(audio);
        }
    }
    if far.is_empty() || near.is_empty() {
        fail(format!(
            "{}: smoke speakers 61 and 121 missing",
            dir.display()
        ));
    }
    (far, near)
}

/// Held-out speech (T1.6b): the utterances of `fixtures/librispeech/aec_heldout.toml` in
/// list order, read from the fixture cache. Returns the speech and a JSON description.
fn heldout_speech(root: &Path, cache: &Path) -> (Speech, Value) {
    let path = root
        .join("fixtures")
        .join("librispeech")
        .join("aec_heldout.toml");
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| fail(format!("cannot read {}: {e}", path.display())));
    let table: toml::Table =
        toml::from_str(&text).unwrap_or_else(|e| fail(format!("{}: {e}", path.display())));
    let utts = table
        .get("utterance")
        .and_then(toml::Value::as_array)
        .unwrap_or_else(|| fail(format!("{}: no utterances", path.display())));
    let (mut far, mut near) = (Vec::new(), Vec::new());
    let (mut far_ids, mut near_ids) = (Vec::new(), Vec::new());
    let (mut far_spk, mut near_spk) = (Vec::new(), Vec::new());
    for u in utts {
        let field = |k: &str| {
            u.get(k)
                .unwrap_or_else(|| fail(format!("{}: utterance without {k}", path.display())))
        };
        let id = field("id").as_str().unwrap_or_default().to_string();
        let speaker = field("speaker").as_integer().unwrap_or(-1);
        let parts: Vec<&str> = id.split('-').collect();
        if parts.len() != 3
            || parts
                .iter()
                .any(|p| p.is_empty() || !p.bytes().all(|b| b.is_ascii_digit()))
            || parts[0] != speaker.to_string()
        {
            fail(format!("{}: bad utterance id {id}", path.display()));
        }
        if DEV_SPEAKERS.iter().any(|&d| i64::from(d) == speaker) {
            fail(format!(
                "held-out speaker {speaker} is a dev (smoke) speaker"
            ));
        }
        let file = cache
            .join("librispeech")
            .join("test-clean")
            .join("LibriSpeech")
            .join("test-clean")
            .join(parts[0])
            .join(parts[1])
            .join(format!("{id}.flac"));
        let audio = read_flac(&file);
        match field("end").as_str() {
            Some("far") => {
                far.push(audio);
                far_ids.push(id);
                far_spk.push(speaker);
            }
            Some("near") => {
                near.push(audio);
                near_ids.push(id);
                near_spk.push(speaker);
            }
            _ => fail(format!("{}: {id} has no far/near end", path.display())),
        }
    }
    far_spk.dedup();
    near_spk.dedup();
    if far.is_empty() || near.is_empty() || far_spk.len() != 1 || near_spk.len() != 1 {
        fail(format!(
            "{}: need one far and one near speaker",
            path.display()
        ));
    }
    let desc = json!({
        "far_speaker": far_spk[0], "near_speaker": near_spk[0],
        "far": far_ids, "near": near_ids,
    });
    ((far, near), desc)
}

/// One dev-wide speaker pair (T1.6c): its far and near speaker ids and speech.
struct DevPair {
    far: u32,
    near: u32,
    speech: Speech,
}

/// Dev-wide speech (T1.6c): the pairs of `fixtures/librispeech/aec_dev.toml`, each end's
/// utterances in list order, read from the fixture cache (the caller verifies checksums).
fn devwide_speech(root: &Path, cache: &Path) -> Vec<DevPair> {
    let path = root
        .join("fixtures")
        .join("librispeech")
        .join("aec_dev.toml");
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| fail(format!("cannot read {}: {e}", path.display())));
    let table: toml::Table =
        toml::from_str(&text).unwrap_or_else(|e| fail(format!("{}: {e}", path.display())));
    let heldout = heldout_speakers(root);
    let list = |k: &str| {
        table
            .get(k)
            .and_then(toml::Value::as_array)
            .unwrap_or_else(|| fail(format!("{}: no {k} entries", path.display())))
    };
    let int = |v: &toml::Value, k: &str| -> u32 {
        v.get(k)
            .and_then(toml::Value::as_integer)
            .and_then(|x| u32::try_from(x).ok())
            .unwrap_or_else(|| fail(format!("{}: entry without {k}", path.display())))
    };
    let mut by_speaker: std::collections::BTreeMap<u32, Vec<Vec<f32>>> =
        std::collections::BTreeMap::new();
    for u in list("utterance") {
        let speaker = int(u, "speaker");
        let id = u
            .get("id")
            .and_then(toml::Value::as_str)
            .unwrap_or_default();
        let parts: Vec<&str> = id.split('-').collect();
        if parts.len() != 3
            || parts
                .iter()
                .any(|p| p.is_empty() || !p.bytes().all(|b| b.is_ascii_digit()))
            || parts[0] != speaker.to_string()
        {
            fail(format!("{}: bad utterance id {id}", path.display()));
        }
        if DEV_SPEAKERS.contains(&speaker) {
            fail(format!("dev-wide speaker {speaker} is a smoke speaker"));
        }
        if heldout.contains(&i64::from(speaker)) {
            fail(format!("dev-wide speaker {speaker} is a held-out speaker"));
        }
        let file = cache
            .join("librispeech")
            .join("dev-clean")
            .join("LibriSpeech")
            .join("dev-clean")
            .join(parts[0])
            .join(parts[1])
            .join(format!("{id}.flac"));
        by_speaker
            .entry(speaker)
            .or_default()
            .push(read_flac(&file));
    }
    let speech = |s: u32| {
        by_speaker
            .get(&s)
            .cloned()
            .unwrap_or_else(|| fail(format!("{}: speaker {s} has no utterances", path.display())))
    };
    list("pair")
        .iter()
        .map(|p| {
            let (far, near) = (int(p, "far"), int(p, "near"));
            DevPair {
                far,
                near,
                speech: (speech(far), speech(near)),
            }
        })
        .collect()
}

/// Speakers of the held-out speech (`fixtures/librispeech/aec_heldout.toml`), which the
/// dev-wide set must never use.
fn heldout_speakers(root: &Path) -> Vec<i64> {
    let path = root
        .join("fixtures")
        .join("librispeech")
        .join("aec_heldout.toml");
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| fail(format!("cannot read {}: {e}", path.display())));
    let table: toml::Table =
        toml::from_str(&text).unwrap_or_else(|e| fail(format!("{}: {e}", path.display())));
    let speakers: Vec<i64> = table
        .get("utterance")
        .and_then(toml::Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|u| u.get("speaker").and_then(toml::Value::as_integer))
        .collect();
    if speakers.is_empty() {
        fail(format!("{}: no held-out speakers", path.display()));
    }
    speakers
}

/// The dev-wide sweep (T1.6c): every gating scenario at seed + [`DEV_WIDE_SEED_OFFSET`] with
/// each dev-wide speaker pair, scored by the same code as the base runs. Reports every run,
/// the worst value per metric for each scenario and pair, and the overall worst. Dev data:
/// reported, never gated. A rendering that breaks the scenario validity rule fails.
fn devwide_eval(file: &ScenarioFile, pairs: &[DevPair], lim: &Limits) -> Value {
    let mut runs = Vec::new();
    let mut all = Vec::new();
    let mut by_scenario: std::collections::BTreeMap<String, Vec<Scores>> =
        std::collections::BTreeMap::new();
    let mut by_pair: std::collections::BTreeMap<String, Vec<Scores>> =
        std::collections::BTreeMap::new();
    for p in pairs {
        let pair = format!("{}/{}", p.far, p.near);
        for base in file.scenario.iter().filter(|s| !is_informational(s)) {
            let scn = Scenario {
                name: format!("{}@dev+{DEV_WIDE_SEED_OFFSET}/{pair}", base.name),
                seed: base
                    .seed
                    .checked_add(DEV_WIDE_SEED_OFFSET)
                    .unwrap_or_else(|| fail(format!("{}: dev-wide seed overflows", base.name))),
                ..base.clone()
            };
            let s = sim(&scn, file, &p.speech);
            let bad = recovery_window_violations(&scn, &s);
            if !bad.is_empty() {
                fail(format!("dev-wide rendering is invalid: {}", bad.join("; ")));
            }
            let (run, _) = timed_run(&s);
            let sc = score_for(&scn, &s, &run, &canceller_config());
            runs.push(json!({
                "base": base.name,
                "pair": pair,
                "seed": scn.seed,
                "scores": coded_scores_json(&sc),
                "doubletalk_sisdr_below_ser0": sc.doubletalk_sisdr_informational.map_or(Value::Null, finite_or_str),
                "doubletalk_highpassed": doubletalk_highpassed_json(&sc),
                "would_fail": sc.failures(lim),
            }));
            by_scenario.entry(base.name.clone()).or_default().push(sc);
            by_pair.entry(pair.clone()).or_default().push(sc);
            all.push(sc);
        }
    }
    let fold = |m: std::collections::BTreeMap<String, Vec<Scores>>| -> Value {
        m.into_iter()
            .map(|(k, v)| (k, coded_scores_json(&worst(&v))))
            .collect::<serde_json::Map<String, Value>>()
            .into()
    };
    let dt_highpassed = doubletalk_highpassed_worst(&by_scenario);
    json!({
        "seed_offset": DEV_WIDE_SEED_OFFSET,
        "pairs": pairs.iter().map(|p| json!({ "far": p.far, "near": p.near })).collect::<Vec<_>>(),
        "doubletalk_highpassed_by_scenario": dt_highpassed,
        "worst": coded_scores_json(&worst(&all)),
        "worst_by_scenario": fold(by_scenario),
        "worst_by_pair": fold(by_pair),
        "runs": runs,
    })
}

/// The rumble dev set (T6.23b): each listed base scenario with its own seed and the smoke
/// speech, plus the measured rumble.
fn rumble_eval(file: &ScenarioFile, path: &Path, speech: &Speech, lim: &Limits) -> Value {
    let text = std::fs::read_to_string(path)
        .unwrap_or_else(|e| fail(format!("cannot read {}: {e}", path.display())));
    let set: RumbleSet =
        toml::from_str(&text).unwrap_or_else(|e| fail(format!("{}: {e}", path.display())));
    let mut runs = Vec::new();
    for name in &set.scenarios {
        let scn = file
            .get(name)
            .unwrap_or_else(|| fail(format!("rumble set names unknown scenario {name}")));
        let mut s = sim(scn, file, speech);
        let mut rng = ChaCha8Rng::seed_from_u64(scn.seed.wrapping_add(set.seed_offset));
        let ms = add_rumble(&mut s, &set, &mut rng);
        let (run, _) = timed_run(&s);
        let sc = score_for(scn, &s, &run, &canceller_config());
        runs.push(json!({
            "name": name,
            "stereo": s.reference_lr.is_some(),
            "rumble_mean_square": ms,
            "scores": coded_scores_json(&sc),
            "would_fail": sc.failures(lim),
        }));
    }
    json!({ "set": path.display().to_string(), "runs": runs })
}

fn limits(root: &Path) -> Limits {
    let path = root.join("acceptance").join("thresholds.toml");
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| fail(format!("cannot read {}: {e}", path.display())));
    let table: toml::Table =
        toml::from_str(&text).unwrap_or_else(|e| fail(format!("{}: {e}", path.display())));
    let v = |id: &str| {
        table["thresholds"][id]["value"]
            .as_float()
            .unwrap_or_else(|| fail(format!("threshold {id} has no value")))
    };
    Limits {
        erle_farend: v("aec.erle_farend"),
        erle_linear: v("aec.erle_linear"),
        convergence: v("aec.convergence"),
        path_change: v("aec.path_change"),
        doubletalk_sisdr: v("aec.doubletalk_sisdr"),
        passthrough: v("aec.passthrough"),
    }
}

fn sim(scn: &Scenario, file: &ScenarioFile, speech: &Speech) -> SimOutput {
    let pool = SpeechPool {
        far: &speech.0,
        near: &speech.1,
    };
    let mut rng = ChaCha8Rng::seed_from_u64(scn.seed);
    simulate(scn, &file.setup, &pool, &mut rng)
        .unwrap_or_else(|e| fail(format!("simulate {}: {e}", scn.name)))
}

/// Runs the canceller, returning the run and the processing time (s).
fn timed_run(s: &SimOutput) -> (Run, f64) {
    let n = s.mic.len() / BLOCK * BLOCK;
    let mut aec = EchoCanceller::new(canceller_config());
    let mut out = vec![0.0f32; n];
    let mut linear = vec![0.0f32; n];
    let t0 = Instant::now();
    match &s.reference_lr {
        // Stereo scenarios run the stereo canceller (T6.6a), as `eval::run_canceller` does.
        Some([l, r]) => {
            aec.process_stereo_with_linear(&s.mic[..n], &l[..n], &r[..n], &mut out, &mut linear);
        }
        None => aec.process_with_linear(&s.mic[..n], &s.reference[..n], &mut out, &mut linear),
    }
    let dt = t0.elapsed().as_secs_f64();
    (compensate(out, linear, aec.output_latency()), dt)
}

fn scores_json(s: &Scores) -> Value {
    let f = |v: Option<f64>| v.map_or(Value::Null, |x| json!(finite_or_str(x)));
    json!({
        "erle_farend": f(s.erle_farend),
        "erle_linear": f(s.erle_linear),
        "convergence": f(s.convergence),
        "path_change": f(s.path_change),
        "doubletalk_sisdr": f(s.doubletalk_sisdr),
        "passthrough": f(s.passthrough),
    })
}

/// Double-talk SI-SDR against the clean near end (official, at any SER) beside the same
/// output scored against the high-passed near end (T6.23c step 1, informational only);
/// `null` when the run is not double talk.
fn doubletalk_highpassed_json(s: &Scores) -> Value {
    let official = s.doubletalk_sisdr.or(s.doubletalk_sisdr_informational);
    match official {
        None => Value::Null,
        Some(v) => json!({
            "official": finite_or_str(v),
            "gated": s.doubletalk_sisdr.is_some(),
            "highpassed_near": s.doubletalk_sisdr_highpassed.map_or(Value::Null, finite_or_str),
        }),
    }
}

/// Worst (lowest) official and high-passed double-talk SI-SDR per base scenario.
fn doubletalk_highpassed_worst(
    by_scenario: &std::collections::BTreeMap<String, Vec<Scores>>,
) -> Value {
    by_scenario
        .iter()
        .filter_map(|(k, v)| {
            let w = worst(v);
            let j = doubletalk_highpassed_json(&w);
            (!j.is_null()).then(|| (k.clone(), j))
        })
        .collect::<serde_json::Map<String, Value>>()
        .into()
}

/// Scores keyed by their threshold id (`null` when the metric does not apply).
fn coded_scores_json(s: &Scores) -> Value {
    let f = |v: Option<f64>| v.map_or(Value::Null, finite_or_str);
    json!({
        "aec.erle_farend": f(s.erle_farend),
        "aec.erle_linear": f(s.erle_linear),
        "aec.convergence": f(s.convergence),
        "aec.path_change": f(s.path_change),
        "aec.doubletalk_sisdr": f(s.doubletalk_sisdr),
        "aec.passthrough": f(s.passthrough),
    })
}

/// The held-out set (T1.6b): every run of `ScenarioFile::heldout_runs` with the held-out
/// speech, scored by the same code as the dev runs. A rendering that breaks the scenario
/// validity rule (`recovery_window_violations`) fails the evaluation: its event metrics
/// would not be measurable.
fn heldout_eval(file: &ScenarioFile, speech: &Speech, desc: Value, lim: &Limits) -> Value {
    let runs = file
        .heldout_runs()
        .unwrap_or_else(|e| fail(format!("held-out set: {e}")));
    let offsets: Vec<u64> = file
        .heldout
        .as_ref()
        .map(|h| h.seed_offsets.clone())
        .unwrap_or_default();
    let mut out = Vec::new();
    let mut by_scenario: std::collections::BTreeMap<String, Vec<Scores>> =
        std::collections::BTreeMap::new();
    for r in &runs {
        let s = sim(&r.scenario, file, speech);
        let bad = recovery_window_violations(&r.scenario, &s);
        if !bad.is_empty() {
            fail(format!("held-out rendering is invalid: {}", bad.join("; ")));
        }
        let (run, _) = timed_run(&s);
        let sc = score_for(&r.scenario, &s, &run, &canceller_config());
        out.push(json!({
            "base": r.base,
            "offset": r.offset,
            "seed": r.scenario.seed,
            "scores": coded_scores_json(&sc),
            // Double-talk SI-SDR below SER 0: reported, never gated or baselined (PLAN 4.0).
            "doubletalk_sisdr_below_ser0": sc.doubletalk_sisdr_informational.map_or(Value::Null, finite_or_str),
            // Informational (T6.23c step 1): never baselined or gated.
            "doubletalk_highpassed": doubletalk_highpassed_json(&sc),
            "would_fail": sc.failures(lim),
        }));
        by_scenario.entry(r.base.clone()).or_default().push(sc);
    }
    // Everything a held-out run depends on besides the speech and the canceller: the room
    // setup and every gating scenario's parameters, in their exact (round-trip) `Debug`
    // form. The baseline stores its digest, so editing a scenario forces a regeneration.
    let gating: Vec<&Scenario> = file.scenario.iter().filter(|s| !s.informational).collect();
    let scenarios = format!("{:?}\n{gating:?}", file.setup);
    json!({
        "seed_offsets": offsets,
        "speech": desc,
        "scenarios": scenarios,
        "runs": out,
        "doubletalk_highpassed_by_scenario": doubletalk_highpassed_worst(&by_scenario),
    })
}

/// JSON cannot hold infinities: they become the strings "inf" / "-inf".
fn finite_or_str(x: f64) -> Value {
    if x.is_finite() {
        json!(x)
    } else if x > 0.0 {
        json!("inf")
    } else if x < 0.0 {
        json!("-inf")
    } else {
        json!("nan")
    }
}

/// A real recording's mic and loopback channels at 16 kHz, cut to whole blocks.
struct Recording {
    mic: Vec<f32>,
    /// Loopback downmix (mixed before resampling) and the individual channels.
    downmix: Vec<f32>,
    loopback: Vec<Vec<f32>>,
}

/// Loads `mic.wav` and `loopback.wav` from `dir`; `Ok(None)` when the pair is absent.
fn load_recording(dir: &Path) -> Result<Option<Recording>, String> {
    let mic_path = dir.join("mic.wav");
    let ref_path = dir.join("loopback.wav");
    if !mic_path.is_file() || !ref_path.is_file() {
        return Ok(None);
    }
    // A report never names the folder (`real_recordings`).
    let load = |p: &Path| -> Result<WavAudio, String> {
        let name = p.file_name().unwrap_or_default().display();
        let f = File::open(p).map_err(|e| format!("{name}: {e}"))?;
        read_wav(BufReader::new(f)).map_err(|e| format!("{name}: {e}"))
    };
    let to16 = |x: &[f32], rate: SampleRate| {
        resample_all(x, rate, SampleRate::HZ_16000).map_err(|e| e.to_string())
    };
    let m = load(&mic_path)?;
    let l = load(&ref_path)?;
    let mic = to16(&m.to_mono(), m.rate())?;
    let downmix = to16(&l.to_mono(), l.rate())?;
    let loopback = (0..l.channels())
        .map(|c| to16(&l.channel(c).unwrap_or_default(), l.rate()))
        .collect::<Result<Vec<_>, _>>()?;
    let n = loopback
        .iter()
        .map(Vec::len)
        .fold(mic.len().min(downmix.len()), usize::min)
        / BLOCK
        * BLOCK;
    Ok(Some(Recording {
        mic: mic[..n].to_vec(),
        downmix: downmix[..n].to_vec(),
        loopback: loopback.into_iter().map(|c| c[..n].to_vec()).collect(),
    }))
}

/// Median segmental ERLE of a latency-compensated canceller output against the mic.
fn real_erle(mic: &[f32], out: &[f32], latency: usize) -> Option<f64> {
    let out = &out[latency..];
    segmental_erle(&mic[..out.len()], out, 16_000, &ErleConfig::default()).map(|r| r.median_db)
}

const REAL_DETAIL: &str =
    "real call on speakers with talk-over: ERLE includes near-end speech, informational only";

const NO_PAIR: &str = "the recording's folder has no mic.wav/loopback.wav pair";

fn gate10(dir: &Path) -> Value {
    let result = (|| -> Result<Value, String> {
        let Some(rec) = load_recording(dir)? else {
            return Ok(json!({ "status": "skipped", "detail": NO_PAIR }));
        };
        // The mono canceller gets the loopback downmix.
        let mut aec = EchoCanceller::new(canceller_config());
        let out = aec.process_vec(&rec.mic, &rec.downmix);
        let erle = real_erle(&rec.mic, &out, aec.output_latency());
        Ok(json!({
            "status": "measured",
            "erle_db": erle.map(finite_or_str),
            "seconds": (rec.mic.len() - aec.output_latency()) as f64 / 16_000.0,
            "final_delay_ms": aec.status().delay.delay().map(|d| d / 16.0),
            "detail": REAL_DETAIL,
        }))
    })();
    result.unwrap_or_else(|e| json!({ "status": "error", "detail": e }))
}

/// GATE-10b: the stereo recording through the stereo canceller, with the mono canceller
/// on the downmix for comparison (T1.5b; informational).
fn gate10b(dir: &Path) -> Value {
    let result = (|| -> Result<Value, String> {
        let Some(rec) = load_recording(dir)? else {
            return Ok(json!({ "status": "skipped", "detail": NO_PAIR }));
        };
        if rec.loopback.len() != 2 {
            return Ok(json!({
                "status": "skipped",
                "detail": format!("loopback.wav has {} channel(s), not 2", rec.loopback.len()),
            }));
        }
        let n = rec.mic.len();
        let mut stereo = EchoCanceller::new(canceller_config());
        let mut out = vec![0.0f32; n];
        stereo.process_stereo(&rec.mic, &rec.loopback[0], &rec.loopback[1], &mut out);
        let erle = real_erle(&rec.mic, &out, stereo.output_latency());
        let mut mono = EchoCanceller::new(canceller_config());
        let out_mono = mono.process_vec(&rec.mic, &rec.downmix);
        let erle_mono = real_erle(&rec.mic, &out_mono, mono.output_latency());
        Ok(json!({
            "status": "measured",
            "erle_db": erle.map(finite_or_str),
            "erle_db_mono_downmix": erle_mono.map(finite_or_str),
            "seconds": (n - stereo.output_latency()) as f64 / 16_000.0,
            "final_delay_ms": stereo.status().delay.delay().map(|d| d / 16.0),
            "detail": REAL_DETAIL,
        }))
    })();
    result.unwrap_or_else(|e| json!({ "status": "error", "detail": e }))
}

/// The GATE-10b stereo recording sits next to the GATE-10 one, with a `-stereo` suffix.
fn stereo_dir(private: &Path) -> PathBuf {
    let mut name = private.file_name().unwrap_or_default().to_os_string();
    name.push("-stereo");
    private.with_file_name(name)
}

/// The `gate10` and `gate10b` reports; without a folder (no `--private`, no private settings)
/// each is a visible skip. A report never names the folder: `accept m1` commits these details.
fn real_recordings(private: Option<&Path>) -> (Value, Value) {
    match private {
        Some(dir) => (gate10(dir), gate10b(&stereo_dir(dir))),
        None => {
            let skip = json!({ "status": "skipped", "detail": gate12_private::NOT_HERE });
            (skip.clone(), skip)
        }
    }
}

fn main() {
    const USAGE: &str = "usage: aec_eval <repo-root> [--private <dir>] [--heldout <fixture-cache>] [--heldout-only] [--devwide <fixture-cache>] [--devwide-only] [--highpass off|A<hz>|I<hz>|D<hz>[w]] [--rumble <set.toml>]";
    let mut args = std::env::args().skip(1);
    let root = PathBuf::from(args.next().unwrap_or_else(|| fail(USAGE.into())));
    let mut private: Option<PathBuf> = None;
    let mut heldout_cache: Option<PathBuf> = None;
    let mut heldout_only = false;
    let mut devwide_cache: Option<PathBuf> = None;
    let mut devwide_only = false;
    let mut rumble: Option<PathBuf> = None;
    while let Some(a) = args.next() {
        match a.as_str() {
            "--private" => {
                private = Some(PathBuf::from(
                    args.next().unwrap_or_else(|| fail(USAGE.into())),
                ));
            }
            "--heldout" => {
                heldout_cache = Some(PathBuf::from(
                    args.next().unwrap_or_else(|| fail(USAGE.into())),
                ));
            }
            "--heldout-only" => heldout_only = true,
            "--devwide" => {
                devwide_cache = Some(PathBuf::from(
                    args.next().unwrap_or_else(|| fail(USAGE.into())),
                ));
            }
            "--devwide-only" => devwide_only = true,
            "--rumble" => {
                rumble = Some(PathBuf::from(
                    args.next().unwrap_or_else(|| fail(USAGE.into())),
                ));
            }
            "--highpass" => {
                let label = args.next().unwrap_or_else(|| fail(USAGE.into()));
                let highpass = HighPass::parse(&label).unwrap_or_else(|e| fail(e));
                CONFIG
                    .set(EchoCancellerConfig {
                        highpass,
                        ..EchoCancellerConfig::default()
                    })
                    .unwrap_or_else(|_| fail("--highpass given twice".into()));
            }
            _ => fail(USAGE.into()),
        }
    }
    let path = root.join("acceptance").join("aec_scenarios.toml");
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| fail(format!("cannot read {}: {e}", path.display())));
    let file: ScenarioFile =
        toml::from_str(&text).unwrap_or_else(|e| fail(format!("{}: {e}", path.display())));
    let lim = limits(&root);
    if let Some(path) = &rumble {
        println!(
            "{}",
            json!({ "rumble": rumble_eval(&file, path, &smoke_speech(&root), &lim), "highpass": canceller_config().highpass.label() })
        );
        return;
    }
    // Held-out speech is loaded (and checked) first; the held-out runs go after every dev
    // loop, so `aec.rtf` and the dev results are measured exactly as before T1.6b.
    let heldout_input = heldout_cache.as_ref().map(|c| heldout_speech(&root, c));
    if heldout_only && heldout_input.is_none() {
        fail("--heldout-only needs --heldout <fixture-cache>".into());
    }
    let run_heldout = || match &heldout_input {
        Some((speech, desc)) => heldout_eval(&file, speech, desc.clone(), &lim),
        None => Value::Null,
    };
    if heldout_only {
        println!(
            "{}",
            json!({ "heldout": run_heldout(), "highpass": canceller_config().highpass.label() })
        );
        return;
    }
    let devwide_input = devwide_cache.as_ref().map(|c| devwide_speech(&root, c));
    let run_devwide = || match &devwide_input {
        Some(pairs) => devwide_eval(&file, pairs, &lim),
        None => Value::Null,
    };
    if devwide_only {
        if devwide_input.is_none() {
            fail("--devwide-only needs --devwide <fixture-cache>".into());
        }
        println!(
            "{}",
            json!({ "devwide": run_devwide(), "highpass": canceller_config().highpass.label() })
        );
        return;
    }
    let speech = smoke_speech(&root);
    // A damaged settings file fails here, before the long runs; a missing one is a skip.
    let private = private.or_else(|| {
        gate12_private::load(gate12_private::default_path().as_deref())
            .unwrap_or_else(|e| fail(e))
            .map(|s| root.join(s.gate10.dir))
    });

    // Base scenarios: headline metrics (worst case per metric) and the RTF benchmark.
    let mut base_scores = Vec::new();
    let mut per_scenario = Vec::new();
    let mut informational = Vec::new();
    // Double-talk SI-SDR below SER 0: reported, never gated (PLAN.md 4.0).
    let mut below_ser0 = Vec::new();
    let mut failures = Vec::new();
    let (mut cpu_s, mut audio_s) = (0.0f64, 0.0f64);
    for scn in &file.scenario {
        let s = sim(scn, &file, &speech);
        let (run, dt) = timed_run(&s);
        cpu_s += dt;
        audio_s += run.out.len() as f64 / 16_000.0;
        let sc = score_for(scn, &s, &run, &canceller_config());
        if let Some(v) = sc.doubletalk_sisdr_informational {
            below_ser0.push(json!({ "name": scn.name, "doubletalk_sisdr": finite_or_str(v) }));
        }
        if is_informational(scn) {
            // Informational scenario: reported, never measured or used for tuning.
            informational.push(json!({
                "name": scn.name,
                "class": format!("{:?}", classify(scn).0),
                "scores": scores_json(&sc),
                "would_fail": sc.failures(&lim),
            }));
            continue;
        }
        for f in sc.failures(&lim) {
            failures.push(format!("{}: {f}", scn.name));
        }
        per_scenario.push(json!({
            "name": scn.name,
            "class": format!("{:?}", classify(scn).0),
            "scores": scores_json(&sc),
            "doubletalk_highpassed": doubletalk_highpassed_json(&sc),
        }));
        base_scores.push(sc);
    }
    let headline = worst(&base_scores);

    // Robustness grid: every listed scenario at every delay x drift.
    let mut grid_runs = 0usize;
    let mut grid = Vec::new();
    // Stereo convergence at 500 ms bulk delay: reported, not gated (aec.robustness).
    let mut grid_informational = Vec::new();
    for name in &file.robustness.scenarios {
        let base = file
            .get(name)
            .unwrap_or_else(|| fail(format!("robustness names unknown scenario {name}")));
        if is_informational(base) {
            continue;
        }
        for &d in &file.robustness.delays_ms {
            for &p in &file.robustness.drifts_ppm {
                let scn = base.with_delay_and_drift(d, p);
                let s = sim(&scn, &file, &speech);
                let (run, _) = timed_run(&s);
                grid_runs += 1;
                let mut sc = score_for(&scn, &s, &run, &canceller_config());
                grid.push(json!({ "name": scn.name, "scores": scores_json(&sc) }));
                if grid_convergence_informational(&scn, d)
                    && let Some(c) = sc.convergence.take()
                {
                    grid_informational.push(json!({
                        "name": scn.name,
                        "convergence": finite_or_str(c),
                        "would_fail": c.is_nan() || c > lim.convergence,
                    }));
                }
                if let Some(v) = sc.doubletalk_sisdr_informational {
                    below_ser0
                        .push(json!({ "name": scn.name, "doubletalk_sisdr": finite_or_str(v) }));
                }
                for f in sc.failures(&lim) {
                    failures.push(format!("{}: {f}", scn.name));
                }
            }
        }
    }

    // Informational second-seed sweep (T6.6a): every gating scenario with another seed, to
    // show the tuning has not overfit the fixed seeds. Reported only, never gated.
    let mut second_seed = Vec::new();
    for scn in file.scenario.iter().filter(|s| !is_informational(s)) {
        let scn = Scenario {
            name: format!("{}@seed+{SECOND_SEED_OFFSET}", scn.name),
            seed: scn.seed + SECOND_SEED_OFFSET,
            ..scn.clone()
        };
        let s = sim(&scn, &file, &speech);
        let (run, _) = timed_run(&s);
        let sc = score_for(&scn, &s, &run, &canceller_config());
        second_seed.push(json!({
            "name": scn.name,
            "scores": scores_json(&sc),
            "would_fail": sc.failures(&lim),
        }));
    }

    let devwide = run_devwide();
    let heldout = run_heldout();
    let (gate10, gate10b) = real_recordings(private.as_deref());
    let report = json!({
        "highpass": canceller_config().highpass.label(),
        "headline": scores_json(&headline),
        "scenarios": per_scenario,
        "informational": informational,
        "doubletalk_below_ser0": below_ser0,
        "robustness": {
            "pass": failures.is_empty(),
            "base_runs": file.scenario.len(),
            "grid_runs": grid_runs,
            "runs": grid,
            "informational": grid_informational,
            "failures": failures,
        },
        "rtf": { "value": cpu_s / audio_s, "cpu_s": cpu_s, "audio_s": audio_s },
        "gate10": gate10,
        "gate10b": gate10b,
        "second_seed": second_seed,
        "devwide": devwide,
        "heldout": heldout,
    });
    println!("{report}");
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An invented folder, so the assertions can look for its name in what `accept m1` keeps.
    fn folder(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "aec-eval-{name}-private-folder-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn aec_eval_real_recordings_without_settings_are_a_visible_skip() {
        let (mono, stereo) = real_recordings(None);
        for report in [&mono, &stereo] {
            assert_eq!(report["status"], "skipped");
            let detail = report["detail"].as_str().unwrap();
            assert!(detail.contains("gate12.toml"), "{detail}");
        }
    }

    #[test]
    fn aec_eval_real_recording_reports_never_name_the_folder() {
        // `accept m1` commits these details, and a private recording's folder is private.
        let empty = folder("empty");
        let (mono, stereo) = real_recordings(Some(&empty));
        for report in [&mono, &stereo] {
            assert_eq!(report["status"], "skipped");
            let detail = report["detail"].as_str().unwrap();
            assert!(detail.contains("mic.wav"), "{detail}");
            assert!(!detail.contains("private-folder"), "{detail}");
        }

        let damaged = folder("damaged");
        std::fs::write(damaged.join("mic.wav"), b"not audio").unwrap();
        std::fs::write(damaged.join("loopback.wav"), b"not audio").unwrap();
        let report = gate10(&damaged);
        assert_eq!(report["status"], "error");
        let detail = report["detail"].as_str().unwrap();
        assert!(detail.contains("mic.wav"), "{detail}");
        assert!(!detail.contains("private-folder"), "{detail}");

        let mono_loopback = folder("mono");
        for name in ["mic.wav", "loopback.wav"] {
            let mut bytes = Vec::new();
            earshot_core::audio::write_wav(
                &mut bytes,
                SampleRate::HZ_16000,
                1,
                &[0.0; 1_600],
                earshot_core::audio::WavFormat::Pcm16,
            )
            .unwrap();
            std::fs::write(mono_loopback.join(name), bytes).unwrap();
        }
        let report = gate10b(&mono_loopback);
        assert_eq!(report["status"], "skipped");
        let detail = report["detail"].as_str().unwrap();
        assert!(detail.contains("loopback.wav"), "{detail}");
        assert!(!detail.contains("private-folder"), "{detail}");

        for dir in [empty, damaged, mono_loopback] {
            let _ = std::fs::remove_dir_all(dir);
        }
    }
}
