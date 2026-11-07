//! M3 acceptance evaluation (T3.5): diarisation and voiceprints measured on the production
//! setup, run by `cargo xtask accept m3`.
//!
//! ```text
//! cargo run --release -p earshot-runtime --example diar_eval -- \
//!     --root <repo> --models <dir> --fixtures <dir>
//! ```
//!
//! Prints one JSON report on stdout (progress on stderr). Missing models or fixtures fail with
//! the `cargo xtask fetch-models` / `fetch-fixtures` command to run; nothing is skipped.
//!
//! Production setup: ONNX Runtime's CPU library (`backends::init_onnx`), pyannote
//! segmentation-3.0 and CAM++ loaded by `backends::load_diarisation` on the CPU (as the final
//! pass, T6.11) with `backends::speaker_threads` ONNX threads, the core `Diarizer` with
//! `PostConfig::default().diarization` and the clustering threshold the final pass uses
//! (`PostConfig::clustering_threshold`: the dev-clean calibration keyed by the embedder's
//! exact model id), and the voiceprint threshold the final pass uses
//! (`PostConfig::voiceprint_threshold`, dev-clean calibration). Nothing is tuned here. The
//! backend is proven by ONNX Runtime's node placement of both sessions.
//!
//! Definitions (PLAN.md 4.0 and 4.2, fixed before the first measurement):
//! - `dia.der_ami_ihm` / `dia.der_ami_sdm`: the AMI meetings pinned for M2
//!   (`asr_eval/refs.rs` `AMI_MEETINGS`: IS1009a and ES2004a, the first two of the pyannote
//!   AMI-diarization-setup `test.mini` list; no overlap with calibration data, which was
//!   LibriSpeech dev-clean only). IHM-mix is `<meeting>.Mix-Headset.wav`, SDM is
//!   `<meeting>.Array1-01.wav`. Each recording is diarised whole from time 0, as the final
//!   pass diarises a track. Reference: the AMI-diarization-setup `only_words` RTTM; scored
//!   region: its UEM; NIST collar 0.25 s per side (`DerConfig::nist(AMI_COLLAR_PER_SIDE_S)`),
//!   overlap scored (core `metrics::der`, the pyannote.metrics definition). RTTM lines of zero
//!   duration are skipped, as pyannote's annotation loader drops empty segments. The value is
//!   the corpus DER over both meetings: summed missed + false alarm + confusion over summed
//!   scored reference time (pyannote.metrics accumulation), in percent.
//! - `dia.count_error`: the median of |found - true| speaker counts over every synthetic scene
//!   and the four AMI diarisations above (two meetings x two conditions; AMI true count = the
//!   distinct RTTM speakers, found = the diarizer's speakers). The median of an even number
//!   of items is the mean of the middle two. Synthetic scenes: [`SCENES_PER_COUNT`] per
//!   speaker count k in 1..=6, built from the 40 LibriSpeech **test-clean** speakers of
//!   `fixtures/librispeech/subset.toml` (never a dev-clean speaker: asserted) exactly as the
//!   T3.2b calibration scenes (`earshot-onnx/tests/diar_calibration.rs`), with seed
//!   `SCENE_SEED_BASE + 100 k + s`: each speaker's stream is its subset utterances
//!   concatenated in id order; the first k speakers of a ChaCha8 shuffle take part; the first
//!   k turns visit every speaker once, later turns pick another speaker than the previous one
//!   with >= 1.5 s left (for k = 1 the same speaker continues); each turn is the next 1.5-8 s
//!   of the stream (capped at what is left, no audio used twice); a turn change overlaps the
//!   previous turn by 0.2-1.0 s (at most half of it) with probability 0.2, else follows a
//!   0.1-0.8 s silence (k = 1: always a silence); turns until 15 s x k of speech; the sum is
//!   scaled down only if it would clip. Their DER (reference = turn spans, no UEM, same
//!   collar) is reported for information only.
//! - `dia.count_error_large` (T3.5a): the mean of |found - true| over the synthetic scenes
//!   above with k in [`LARGE_COUNTS`] (4, 5 and 6 speakers; the same scenes and seeds, no AMI
//!   item), so a diarizer that merges larger meetings is seen even when the median over every
//!   item is not.
//! - `vp.top1` / `vp.false_accept`: the 40 test-clean speakers sorted by id in two folds (the
//!   first 20 enrolled and the last 20 unknown, then swapped), the protocol of the dev-clean
//!   calibration (`earshot-onnx/tests/voiceprint_calibration.rs`): enrolment embeds every
//!   `enrol` utterance (>= 10 s per speaker) whole and enrols them with core
//!   `voiceprint::enroll` (`EnrolmentConfig::default()`); test clips are the centre 2.5 s of
//!   each of a speaker's 3 `test` utterances. Each clip is matched open-set with core
//!   `voiceprint::match_embedding` at the calibrated threshold, as production matches.
//!   `vp.top1` = enrolled speakers' clips matched `Known` as their own person / all enrolled
//!   speakers' clips (a correct best match below the threshold is a miss); the closed-set
//!   top-1 (best score only) is reported for information. `vp.false_accept` = unknown
//!   speakers' clips matched `Known` as anyone / all unknown speakers' clips.

#[allow(dead_code)] // shared with asr_eval; only the AMI meeting list and UEM parser are used
#[path = "../../../earshot-asr-whisper/examples/asr_eval/refs.rs"]
mod refs;

use std::fs::File;
use std::io::BufReader;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use earshot_core::asr::{CancelToken, MeetingTime};
use earshot_core::audio::{MonoBuffer, PIPELINE_RATE, read_wav};
use earshot_core::diar::{Diarizer, Embedding, SpeakerEmbedder, SpeakerSegmenter};
use earshot_core::identity::{DeviceId, EntityId, Hlc, ManualClock};
use earshot_core::metrics::der::{AMI_COLLAR_PER_SIDE_S, DerConfig, DerResult, Turn, der};
use earshot_core::voiceprint::{
    InMemoryVoiceprintRepo, MatchOutcome, Person, Stamp, VoiceprintRepo, enroll, match_embedding,
};
use earshot_onnx::embed::CamppEmbedder;
use earshot_onnx::segment::PyannoteSegmenter;
use earshot_runtime::backends::{
    Accelerator, ModelDir, init_onnx, load_diarisation, speaker_threads,
};
use earshot_runtime::post::PostConfig;
use rand_chacha::ChaCha8Rng;
use rand_core::{Rng, SeedableRng};
use serde_json::{Value, json};

/// Synthetic scenes per speaker count.
pub const SCENES_PER_COUNT: u64 = 5;
/// Speaker counts of the synthetic scenes.
pub const SCENE_COUNTS: std::ops::RangeInclusive<usize> = 1..=6;
/// Speaker counts of the scenes scored by `dia.count_error_large`.
pub const LARGE_COUNTS: std::ops::RangeInclusive<u64> = 4..=6;
/// Scene seeds are `SCENE_SEED_BASE + 100 k + s` (the calibration used `100 k + s` on
/// dev-clean; a distinct base keeps the two apart).
pub const SCENE_SEED_BASE: u64 = 35_000;
/// Speech per speaker in a scene (seconds), as the calibration.
pub const SPEECH_PER_SPEAKER_S: f64 = 15.0;
/// Voiceprint test clip (PLAN.md 4.0: 2-3 s), the centre 2.5 s of a test utterance.
pub const CLIP: usize = 40_000;
/// The two AMI conditions: (condition, audio file suffix, metric id).
pub const AMI_CONDITIONS: [(&str, &str, &str); 2] = [
    ("ihm_mix", "Mix-Headset", "dia.der_ami_ihm"),
    ("sdm", "Array1-01", "dia.der_ami_sdm"),
];

const HZ: f64 = 16_000.0;
const FETCH_LIBRISPEECH: &str = "run `cargo xtask fetch-fixtures --set librispeech`";

fn fail(msg: impl std::fmt::Display) -> ! {
    eprintln!("diar_eval: {msg}");
    std::process::exit(1);
}

struct Args {
    root: PathBuf,
    models: ModelDir,
    fixtures: PathBuf,
}

fn parse_args() -> Args {
    let mut it = std::env::args().skip(1);
    let (mut root, mut models, mut fixtures) = (None, None, None);
    while let Some(flag) = it.next() {
        let value = it
            .next()
            .unwrap_or_else(|| fail(format!("{flag} needs a value")));
        match flag.as_str() {
            "--root" => root = Some(PathBuf::from(value)),
            "--models" => models = Some(ModelDir::new(value)),
            "--fixtures" => fixtures = Some(PathBuf::from(value)),
            other => fail(format!("unknown argument {other}")),
        }
    }
    Args {
        root: root.unwrap_or_else(|| fail("--root is required")),
        models: models.unwrap_or_else(|| fail("--models is required")),
        fixtures: fixtures.unwrap_or_else(|| fail("--fixtures is required")),
    }
}

// ---- pure helpers (unit-tested) ---------------------------------------------------------

/// Parses an RTTM of `meeting`: `SPEAKER <file> <chan> <start> <dur> <NA> <NA> <name> ...`.
/// Zero-duration turns are skipped (pyannote drops empty segments).
///
/// # Errors
/// A line that is not a SPEAKER line of `meeting`, a non-finite or negative time, or no turn.
pub fn parse_rttm(text: &str, meeting: &str) -> Result<Vec<Turn>, String> {
    let mut out = Vec::new();
    for line in text.lines().filter(|l| !l.trim().is_empty()) {
        let f: Vec<&str> = line.split_whitespace().collect();
        if f.len() < 8 || f[0] != "SPEAKER" {
            return Err(format!("RTTM line `{line}` is not a SPEAKER line"));
        }
        if f[1] != meeting {
            return Err(format!("RTTM line `{line}` is not for {meeting}"));
        }
        let start: f64 = f[3]
            .parse()
            .map_err(|e| format!("RTTM start in `{line}`: {e}"))?;
        let dur: f64 = f[4]
            .parse()
            .map_err(|e| format!("RTTM duration in `{line}`: {e}"))?;
        if !start.is_finite() || !dur.is_finite() || start < 0.0 || dur < 0.0 {
            return Err(format!("RTTM line `{line}` has a bad span"));
        }
        if dur > 0.0 {
            out.push(Turn::new(start, start + dur, f[7]));
        }
    }
    if out.is_empty() {
        return Err(format!("RTTM for {meeting} has no turns"));
    }
    Ok(out)
}

/// Distinct speaker labels of `turns`.
pub fn speaker_count(turns: &[Turn]) -> usize {
    let mut names: Vec<&str> = turns.iter().map(|t| t.speaker.as_str()).collect();
    names.sort_unstable();
    names.dedup();
    names.len()
}

/// The median (mean of the middle two for an even count).
///
/// # Panics
/// On an empty slice or a NaN.
pub fn median(values: &[f64]) -> f64 {
    assert!(!values.is_empty(), "median of nothing");
    let mut v = values.to_vec();
    v.sort_by(|a, b| a.partial_cmp(b).expect("no NaN"));
    let n = v.len();
    if n % 2 == 1 {
        v[n / 2]
    } else {
        0.5 * (v[n / 2 - 1] + v[n / 2])
    }
}

/// The arithmetic mean.
///
/// # Panics
/// On an empty slice.
pub fn mean(values: &[f64]) -> f64 {
    assert!(!values.is_empty(), "mean of nothing");
    values.iter().sum::<f64>() / values.len() as f64
}

/// DER components summed over results (pyannote.metrics accumulation): (missed, false alarm,
/// confusion, total), all in seconds.
pub fn accumulate<'a>(results: impl IntoIterator<Item = &'a DerResult>) -> (f64, f64, f64, f64) {
    results.into_iter().fold((0.0, 0.0, 0.0, 0.0), |a, r| {
        (
            a.0 + r.missed,
            a.1 + r.false_alarm,
            a.2 + r.confusion,
            a.3 + r.total,
        )
    })
}

/// `100 (missed + false alarm + confusion) / total`.
pub fn der_percent(missed: f64, false_alarm: f64, confusion: f64, total: f64) -> f64 {
    100.0 * (missed + false_alarm + confusion) / total
}

fn der_json(r: &DerResult) -> Value {
    json!({
        "total_s": r.total,
        "missed_s": r.missed,
        "false_alarm_s": r.false_alarm,
        "confusion_s": r.confusion,
        "der_percent": der_percent(r.missed, r.false_alarm, r.confusion, r.total),
        "missed_percent": 100.0 * r.missed / r.total,
        "false_alarm_percent": 100.0 * r.false_alarm / r.total,
        "confusion_percent": 100.0 * r.confusion / r.total,
    })
}

/// A uniform draw in `[lo, hi)`.
fn uniform(rng: &mut ChaCha8Rng, lo: f64, hi: f64) -> f64 {
    lo + (hi - lo) * (rng.next_u64() >> 11) as f64 / (1u64 << 53) as f64
}

/// One synthetic scene.
pub struct Scene {
    /// True speaker count.
    pub speakers: usize,
    /// Seed it was built with.
    pub seed: u64,
    /// The mix at 16 kHz.
    pub audio: Vec<f32>,
    /// Reference turns (speaker labels are LibriSpeech speaker ids).
    pub reference: Vec<Turn>,
    /// The sample range of its speaker's stream each turn of `reference` plays.
    pub sources: Vec<std::ops::Range<usize>>,
}

impl Scene {
    /// Two turns (indices into `reference`) of one speaker that play overlapping audio of
    /// its stream, if any: the scene rule never uses audio twice.
    pub fn reused_audio(&self) -> Option<(usize, usize)> {
        for (i, (ti, ri)) in self.reference.iter().zip(&self.sources).enumerate() {
            for (j, (tj, rj)) in self
                .reference
                .iter()
                .zip(&self.sources)
                .enumerate()
                .skip(i + 1)
            {
                if ti.speaker == tj.speaker && ri.start < rj.end && rj.start < ri.end {
                    return Some((i, j));
                }
            }
        }
        None
    }
}

/// The seed of scene `s` with `k` speakers.
pub fn scene_seed(k: usize, s: u64) -> u64 {
    SCENE_SEED_BASE + 100 * k as u64 + s
}

/// Builds scene `(k, s)` from `streams` (speaker id, concatenated audio), by the rule of the
/// module docs (the T3.2b calibration scene rule, extended to k = 1).
///
/// # Panics
/// If `k` is 0 or more than the number of streams, or a stream is shorter than 1.5 s.
pub fn build_scene(streams: &[(u32, Vec<f32>)], k: usize, s: u64) -> Scene {
    assert!(k >= 1 && k <= streams.len(), "bad speaker count {k}");
    let seed = scene_seed(k, s);
    let mut rng = ChaCha8Rng::seed_from_u64(seed);
    let mut order: Vec<usize> = (0..streams.len()).collect();
    for i in (1..order.len()).rev() {
        let j = (rng.next_u64() % (i as u64 + 1)) as usize;
        order.swap(i, j);
    }
    let cast: Vec<usize> = order[..k].to_vec();
    let mut cursor = vec![0usize; k];
    let min_turn = (1.5 * HZ) as usize;
    let mut audio: Vec<f32> = Vec::new();
    let mut reference = Vec::new();
    let mut sources = Vec::new();
    let mut speech = 0.0;
    let mut prev: Option<(usize, usize)> = None; // (speaker slot, turn length in samples)
    let mut turn = 0usize;
    while speech < SPEECH_PER_SPEAKER_S * k as f64 || turn < k {
        let slot = if turn < k {
            turn
        } else {
            let eligible: Vec<usize> = (0..k)
                .filter(|&c| k == 1 || Some(c) != prev.map(|p| p.0))
                .filter(|&c| streams[cast[c]].1.len() - cursor[c] >= min_turn)
                .collect();
            if eligible.is_empty() {
                break;
            }
            eligible[(rng.next_u64() % eligible.len() as u64) as usize]
        };
        let stream = &streams[cast[slot]].1;
        let left = stream.len() - cursor[slot];
        assert!(left >= min_turn, "every speaker has well over 1.5 s");
        let len = ((uniform(&mut rng, 1.5, 8.0) * HZ) as usize).min(left);
        let source = cursor[slot]..cursor[slot] + len;
        let piece = &stream[source.clone()];
        sources.push(source);
        cursor[slot] += len;
        let start = match prev {
            None => 0,
            Some((_, prev_len)) if k > 1 && uniform(&mut rng, 0.0, 1.0) < 0.2 => {
                let ov = ((uniform(&mut rng, 0.2, 1.0) * HZ) as usize).min(prev_len / 2);
                audio.len() - ov
            }
            Some(_) => audio.len() + (uniform(&mut rng, 0.1, 0.8) * HZ) as usize,
        };
        if audio.len() < start + len {
            audio.resize(start + len, 0.0);
        }
        for (a, p) in audio[start..start + len].iter_mut().zip(piece) {
            *a += p;
        }
        reference.push(Turn::new(
            start as f64 / HZ,
            (start + len) as f64 / HZ,
            streams[cast[slot]].0.to_string(),
        ));
        speech += len as f64 / HZ;
        prev = Some((slot, len));
        turn += 1;
    }
    let peak = audio.iter().fold(0.0f32, |m, x| m.max(x.abs()));
    if peak > 0.99 {
        let g = 0.99 / peak;
        audio.iter_mut().for_each(|x| *x *= g);
    }
    Scene {
        speakers: k,
        seed,
        audio,
        reference,
        sources,
    }
}

/// The centre [`CLIP`] samples of `x`.
///
/// # Panics
/// If `x` is shorter than the clip.
pub fn centre_clip(x: &[f32]) -> &[f32] {
    assert!(x.len() >= CLIP, "test utterance shorter than the clip");
    let start = (x.len() - CLIP) / 2;
    &x[start..start + CLIP]
}

/// Voiceprint counts over the open-set decisions of every test clip.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct VpTally {
    /// Enrolled speakers' clips.
    pub known: usize,
    /// ... matched `Known` as their own person (`vp.top1` numerator).
    pub known_correct: usize,
    /// ... whose best-scoring person is their own, whatever the score (closed set).
    pub known_closed_set_top1: usize,
    /// ... matched `Known` as someone else.
    pub known_wrong_person: usize,
    /// ... matched `Unknown`.
    pub known_rejected: usize,
    /// Unknown speakers' clips.
    pub unknown: usize,
    /// ... matched `Known` as anyone (`vp.false_accept` numerator).
    pub unknown_accepted: usize,
}

impl VpTally {
    /// Adds an enrolled speaker's clip: `truth` is their person, `outcome` the match.
    pub fn add_known(&mut self, truth: EntityId, outcome: &MatchOutcome) {
        self.known += 1;
        match outcome {
            MatchOutcome::Known { person, .. } if *person == truth => {
                self.known_correct += 1;
                self.known_closed_set_top1 += 1;
            }
            MatchOutcome::Known { .. } => self.known_wrong_person += 1,
            MatchOutcome::Unknown { best } => {
                self.known_rejected += 1;
                if best.is_some_and(|(p, _)| p == truth) {
                    self.known_closed_set_top1 += 1;
                }
            }
        }
    }

    /// Adds an unknown speaker's clip.
    pub fn add_unknown(&mut self, outcome: &MatchOutcome) {
        self.unknown += 1;
        if matches!(outcome, MatchOutcome::Known { .. }) {
            self.unknown_accepted += 1;
        }
    }

    /// `vp.top1` in percent.
    pub fn top1_percent(&self) -> f64 {
        100.0 * self.known_correct as f64 / self.known as f64
    }

    /// `vp.false_accept` in percent.
    pub fn false_accept_percent(&self) -> f64 {
        100.0 * self.unknown_accepted as f64 / self.unknown as f64
    }
}

// ---- data -------------------------------------------------------------------------------

struct Utt {
    split: String,
    speaker: u32,
    id: String,
    role: String,
    seconds: f64,
}

fn subset(root: &Path) -> Vec<Utt> {
    let path = root.join("fixtures/librispeech/subset.toml");
    let text =
        std::fs::read_to_string(&path).unwrap_or_else(|e| fail(format!("{}: {e}", path.display())));
    let doc: toml::Table = text
        .parse()
        .unwrap_or_else(|e| fail(format!("{}: {e}", path.display())));
    let entries = doc["utterance"]
        .as_array()
        .unwrap_or_else(|| fail("subset.toml has no utterances"));
    entries
        .iter()
        .map(|e| {
            let s = |k: &str| {
                e[k].as_str()
                    .unwrap_or_else(|| fail(format!("subset.toml entry without {k}")))
                    .to_owned()
            };
            Utt {
                split: s("split"),
                speaker: e["speaker"]
                    .as_integer()
                    .and_then(|v| u32::try_from(v).ok())
                    .unwrap_or_else(|| fail("subset.toml entry without speaker")),
                id: s("id"),
                role: s("role"),
                seconds: e["seconds"]
                    .as_float()
                    .unwrap_or_else(|| fail("subset.toml entry without seconds")),
            }
        })
        .collect()
}

fn load_utt(fixtures: &Path, u: &Utt) -> Vec<f32> {
    let parts: Vec<&str> = u.id.split('-').collect();
    let [speaker, chapter, _] = parts.as_slice() else {
        fail(format!("bad LibriSpeech id {}", u.id));
    };
    let path = fixtures
        .join("librispeech")
        .join(&u.split)
        .join("LibriSpeech")
        .join(&u.split)
        .join(speaker)
        .join(chapter)
        .join(format!("{}.flac", u.id));
    if !path.is_file() {
        fail(format!(
            "{} is missing; {FETCH_LIBRISPEECH}",
            path.display()
        ));
    }
    let (rate, samples) = earshot_store::read_flac(&path)
        .unwrap_or_else(|e| fail(format!("{}: {e}", path.display())));
    if rate != PIPELINE_RATE.hz() {
        fail(format!("{}: expected 16 kHz", path.display()));
    }
    samples
}

/// Test-clean speakers sorted by id, after checking none is a dev-clean (calibration)
/// speaker.
fn test_speakers(utts: &[Utt]) -> Vec<u32> {
    let ids = |split: &str| {
        let mut v: Vec<u32> = utts
            .iter()
            .filter(|u| u.split == split)
            .map(|u| u.speaker)
            .collect();
        v.sort_unstable();
        v.dedup();
        v
    };
    let (test, dev) = (ids("test-clean"), ids("dev-clean"));
    if test.len() != 40 {
        fail(format!(
            "subset.toml has {} test-clean speakers, expected 40",
            test.len()
        ));
    }
    if dev.is_empty() {
        fail("subset.toml has no dev-clean speakers to check against");
    }
    if let Some(s) = test.iter().find(|s| dev.contains(s)) {
        fail(format!(
            "speaker {s} is in both test-clean and the dev-clean calibration set"
        ));
    }
    test
}

fn read_ami_wav(fixtures: &Path, meeting: &str, suffix: &str) -> Vec<f32> {
    let path = fixtures.join(format!("ami/audio/{meeting}.{suffix}.wav"));
    let file = File::open(&path)
        .unwrap_or_else(|e| fail(format!("{}: {e}; {}", path.display(), refs::FETCH_AMI)));
    let wav =
        read_wav(BufReader::new(file)).unwrap_or_else(|e| fail(format!("{}: {e}", path.display())));
    if wav.rate() != PIPELINE_RATE {
        fail(format!("{}: expected 16 kHz", path.display()));
    }
    wav.to_mono()
}

fn read_ami_text(fixtures: &Path, rel: &str) -> String {
    let path = fixtures.join(rel);
    let bytes = std::fs::read(&path)
        .unwrap_or_else(|e| fail(format!("{}: {e}; {}", path.display(), refs::FETCH_AMI)));
    refs::latin1(&bytes)
}

// ---- evaluation -------------------------------------------------------------------------

type ProdDiarizer = Diarizer<PyannoteSegmenter, CamppEmbedder>;

fn diarize(d: &mut ProdDiarizer, audio: Vec<f32>) -> (earshot_core::diar::Diarization, f64) {
    let started = Instant::now();
    let buf = MonoBuffer::new(PIPELINE_RATE, audio);
    let out = d
        .diarize(&buf, MeetingTime::ZERO, &CancelToken::new())
        .unwrap_or_else(|e| fail(format!("diarize: {e}")));
    (out, started.elapsed().as_secs_f64())
}

/// One AMI diarisation: (report row, DER result, |count error|).
fn eval_ami(d: &mut ProdDiarizer, fixtures: &Path) -> Vec<(String, Value, DerResult, f64)> {
    let mut rows = Vec::new();
    for &(condition, suffix, id) in &AMI_CONDITIONS {
        for &meeting in &refs::AMI_MEETINGS {
            let uem_text = read_ami_text(fixtures, &format!("ami/uem/{meeting}.uem"));
            let uem = refs::parse_uem(&uem_text, meeting).unwrap_or_else(|e| fail(e));
            let rttm = read_ami_text(fixtures, &format!("ami/rttm/{meeting}.rttm"));
            let reference = parse_rttm(&rttm, meeting).unwrap_or_else(|e| fail(e));
            let audio = read_ami_wav(fixtures, meeting, suffix);
            let audio_s = audio.len() as f64 / HZ;
            eprintln!("diar_eval: {meeting} {suffix} ({audio_s:.0} s)");
            let (out, dt) = diarize(d, audio);
            let cfg = DerConfig::nist(AMI_COLLAR_PER_SIDE_S).with_uem(vec![uem]);
            let r = der(&reference, &out.der_turns(), &cfg);
            let true_count = speaker_count(&reference);
            let found = out.speakers.len();
            let count_error = (found as f64 - true_count as f64).abs();
            let row = json!({
                "metric": id,
                "meeting": meeting,
                "condition": condition,
                "audio": format!("ami/audio/{meeting}.{suffix}.wav"),
                "audio_s": audio_s,
                "uem": [uem.0, uem.1],
                "reference_turns": reference.len(),
                "true_speakers": true_count,
                "found_speakers": found,
                "count_error": count_error,
                "der": der_json(&r),
                "mapping": r.mapping,
                "seconds": dt,
            });
            rows.push((id.to_owned(), row, r, count_error));
        }
    }
    rows
}

fn eval_synthetic(d: &mut ProdDiarizer, streams: &[(u32, Vec<f32>)]) -> (Vec<Value>, Vec<f64>) {
    let collar = DerConfig::nist(AMI_COLLAR_PER_SIDE_S);
    let mut rows = Vec::new();
    let mut errors = Vec::new();
    for k in SCENE_COUNTS {
        for s in 0..SCENES_PER_COUNT {
            let scene = build_scene(streams, k, s);
            if let Some((a, b)) = scene.reused_audio() {
                fail(format!(
                    "scene k={k} s={s} reused audio in turns {a} and {b}"
                ));
            }
            let audio_s = scene.audio.len() as f64 / HZ;
            let (out, dt) = diarize(d, scene.audio);
            let r = der(&scene.reference, &out.der_turns(), &collar);
            let err = (out.speakers.len() as f64 - k as f64).abs();
            errors.push(err);
            rows.push(json!({
                "speakers": k,
                "scene": s,
                "seed": scene.seed,
                "audio_s": audio_s,
                "found_speakers": out.speakers.len(),
                "count_error": err,
                "der": der_json(&r),
                "seconds": dt,
            }));
        }
        eprintln!("diar_eval: synthetic scenes with {k} speakers done");
    }
    (rows, errors)
}

struct VpSpeaker {
    id: u32,
    enrol: Vec<Embedding>,
    enrol_seconds: f64,
    tests: Vec<Embedding>,
}

fn eval_voiceprints(
    emb: &mut CamppEmbedder,
    fixtures: &Path,
    utts: &[Utt],
    speakers: &[u32],
    threshold: f32,
) -> (VpTally, Vec<Value>) {
    let cancel = CancelToken::new();
    let embedded: Vec<VpSpeaker> = speakers
        .iter()
        .map(|&id| {
            let mut s = VpSpeaker {
                id,
                enrol: Vec::new(),
                enrol_seconds: 0.0,
                tests: Vec::new(),
            };
            for u in utts
                .iter()
                .filter(|u| u.split == "test-clean" && u.speaker == id)
            {
                let audio = load_utt(fixtures, u);
                let embed = |e: &mut CamppEmbedder, x: &[f32]| {
                    e.embed(&[x], &cancel)
                        .unwrap_or_else(|e| fail(format!("embed {}: {e}", u.id)))
                };
                match u.role.as_str() {
                    "enrol" => {
                        s.enrol.extend(embed(emb, &audio));
                        s.enrol_seconds += u.seconds;
                    }
                    "test" => s.tests.extend(embed(emb, centre_clip(&audio))),
                    other => fail(format!("subset.toml role {other}")),
                }
            }
            if s.enrol_seconds < 10.0 || s.tests.len() != 3 {
                fail(format!(
                    "speaker {id}: {:.1} s enrolment and {} test clips (need >= 10 s and 3)",
                    s.enrol_seconds,
                    s.tests.len()
                ));
            }
            s
        })
        .collect();
    let mut tally = VpTally::default();
    let mut folds = Vec::new();
    for fold in 0..2u64 {
        let (a, b) = embedded.split_at(20);
        let (enrolled, unknown) = if fold == 0 { (a, b) } else { (b, a) };
        let clock = ManualClock::new(1_800_000_000_000);
        let mut rng = ChaCha8Rng::seed_from_u64(fold);
        let mut hlc = Hlc::new(DeviceId::generate(&clock, &mut rng));
        let mut repo = InMemoryVoiceprintRepo::new();
        let mut person_of: Vec<(u32, EntityId)> = Vec::new();
        let enrolment = PostConfig::default().enrolment;
        for s in enrolled {
            let mut stamp = Stamp {
                clock: &clock,
                hlc: &mut hlc,
                rng: &mut rng,
            };
            let p = Person::new(format!("test-{}", s.id), false, &mut stamp);
            repo.put_person(p.clone())
                .unwrap_or_else(|e| fail(format!("put person: {e}")));
            enroll(
                &mut repo,
                p.id,
                &s.enrol,
                Duration::from_secs_f64(s.enrol_seconds),
                &enrolment,
                &mut stamp,
            )
            .unwrap_or_else(|e| fail(format!("enrol {}: {e}", s.id)));
            person_of.push((s.id, p.id));
        }
        let before = tally;
        for s in enrolled {
            let me = person_of
                .iter()
                .find(|(id, _)| *id == s.id)
                .map(|(_, p)| *p)
                .unwrap_or_else(|| fail("enrolled person missing"));
            for t in &s.tests {
                let o = match_embedding(&repo, t, threshold)
                    .unwrap_or_else(|e| fail(format!("match: {e}")));
                tally.add_known(me, &o);
            }
        }
        for s in unknown {
            for t in &s.tests {
                let o = match_embedding(&repo, t, threshold)
                    .unwrap_or_else(|e| fail(format!("match: {e}")));
                tally.add_unknown(&o);
            }
        }
        folds.push(json!({
            "fold": fold,
            "enrolled": enrolled.iter().map(|s| s.id).collect::<Vec<_>>(),
            "unknown": unknown.iter().map(|s| s.id).collect::<Vec<_>>(),
            "enrolment_s": enrolled.iter().map(|s| s.enrol_seconds).collect::<Vec<_>>(),
            "known_correct": tally.known_correct - before.known_correct,
            "known": tally.known - before.known,
            "unknown_accepted": tally.unknown_accepted - before.unknown_accepted,
            "unknown_clips": tally.unknown - before.unknown,
        }));
    }
    (tally, folds)
}

fn main() {
    let args = parse_args();
    let started = Instant::now();
    let library = init_onnx(&args.models, Accelerator::Cpu).unwrap_or_else(|e| fail(e));
    let cores = std::thread::available_parallelism().map_or(1, std::num::NonZeroUsize::get);
    let threads = speaker_threads(cores);
    let (segmenter, embedder) =
        load_diarisation(&args.models, Accelerator::Cpu).unwrap_or_else(|e| fail(e));
    let placement = [
        ("segmentation", segmenter.backend().clone()),
        ("embedding", embedder.backend().clone()),
    ];
    for (name, b) in &placement {
        if b.kind() != Some(earshot_core::asr::BackendKind::Cpu) || !b.fully_on_requested() {
            fail(format!(
                "{name}: ONNX Runtime placed the session as {b}, expected the CPU"
            ));
        }
    }
    let segmenter_id = segmenter.info().model.to_string();
    let model = embedder.info().model.clone();
    let post = PostConfig::default();
    let cluster_threshold = post
        .clustering_threshold(&model)
        .unwrap_or_else(|e| fail(e));
    let vp_threshold = post
        .voiceprint_threshold(&model)
        .unwrap_or_else(|e| fail(e));
    let mut config = post.diarization.clone();
    config.clustering.threshold = cluster_threshold;
    let mut d = Diarizer::new(segmenter, embedder, config.clone()).unwrap_or_else(|e| fail(e));

    // AMI DER and count.
    let ami = eval_ami(&mut d, &args.fixtures);
    let mut metrics = Vec::new();
    for &(_, _, id) in &AMI_CONDITIONS {
        let results: Vec<&DerResult> = ami.iter().filter(|r| r.0 == id).map(|r| &r.2).collect();
        let (m, fa, c, t) = accumulate(results.iter().copied());
        metrics.push(json!({
            "id": id, "unit": "%", "value": der_percent(m, fa, c, t),
            "missed_s": m, "false_alarm_s": fa, "confusion_s": c, "total_s": t,
            "meetings": results.len(),
        }));
    }

    // Synthetic scenes from test-clean speakers.
    let utts = subset(&args.root);
    let speakers = test_speakers(&utts);
    let mut streams: Vec<(u32, Vec<f32>)> = Vec::new();
    let mut ordered: Vec<&Utt> = utts.iter().filter(|u| u.split == "test-clean").collect();
    ordered.sort_by(|a, b| (a.speaker, &a.id).cmp(&(b.speaker, &b.id)));
    for u in ordered {
        let audio = load_utt(&args.fixtures, u);
        match streams.last_mut() {
            Some((id, s)) if *id == u.speaker => s.extend(audio),
            _ => streams.push((u.speaker, audio)),
        }
    }
    let (synthetic, synthetic_errors) = eval_synthetic(&mut d, &streams);

    let mut count_items: Vec<Value> = synthetic
        .iter()
        .map(|r| {
            json!({
                "source": format!("synthetic k={} s={}", r["speakers"], r["scene"]),
                "true": r["speakers"], "found": r["found_speakers"], "error": r["count_error"],
            })
        })
        .collect();
    let mut errors = synthetic_errors;
    for (_, row, _, err) in &ami {
        count_items.push(json!({
            "source": format!("AMI {} {}", row["meeting"].as_str().unwrap_or("?"),
                row["condition"].as_str().unwrap_or("?")),
            "true": row["true_speakers"], "found": row["found_speakers"], "error": err,
        }));
        errors.push(*err);
    }
    let large: Vec<Value> = count_items
        .iter()
        .filter(|it| {
            it["source"]
                .as_str()
                .is_some_and(|s| s.starts_with("synthetic"))
                && it["true"]
                    .as_u64()
                    .is_some_and(|k| LARGE_COUNTS.contains(&k))
        })
        .cloned()
        .collect();
    let large_errors: Vec<f64> = large.iter().filter_map(|it| it["error"].as_f64()).collect();
    assert_eq!(
        large_errors.len(),
        large.len(),
        "every large-scene item has an error"
    );
    metrics.push(json!({
        "id": "dia.count_error", "unit": "count", "value": median(&errors),
        "items": count_items,
    }));
    metrics.push(json!({
        "id": "dia.count_error_large", "unit": "count", "value": mean(&large_errors),
        "items": large,
    }));

    // Voiceprints.
    let (tally, folds) = eval_voiceprints(
        d.embedder_mut(),
        &args.fixtures,
        &utts,
        &speakers,
        vp_threshold,
    );
    let tally_json = json!({
        "known": tally.known, "known_correct": tally.known_correct,
        "known_closed_set_top1": tally.known_closed_set_top1,
        "known_wrong_person": tally.known_wrong_person, "known_rejected": tally.known_rejected,
        "unknown": tally.unknown, "unknown_accepted": tally.unknown_accepted,
    });
    metrics.push(json!({
        "id": "vp.top1", "unit": "%", "value": tally.top1_percent(), "tally": tally_json,
    }));
    metrics.push(json!({
        "id": "vp.false_accept", "unit": "%", "value": tally.false_accept_percent(),
        "tally": tally_json,
    }));

    let synthetic_results: Vec<(f64, f64, f64, f64)> = synthetic
        .iter()
        .map(|r| {
            let g = |k: &str| r["der"][k].as_f64().unwrap_or(f64::NAN);
            (
                g("missed_s"),
                g("false_alarm_s"),
                g("confusion_s"),
                g("total_s"),
            )
        })
        .collect();
    let s = synthetic_results.iter().fold((0.0, 0.0, 0.0, 0.0), |a, r| {
        (a.0 + r.0, a.1 + r.1, a.2 + r.2, a.3 + r.3)
    });

    let report = json!({
        "backend": "cpu",
        "onnx_library": library.display().to_string(),
        "speaker_threads": threads,
        "cores": cores,
        "placement": placement.iter().map(|(n, b)| json!({
            "session": n,
            "placement": b.to_string(),
            "kind": b.kind(),
            "fully_on_requested": b.fully_on_requested(),
        })).collect::<Vec<_>>(),
        "segmentation_model": segmenter_id,
        "embedding_model": model.as_str(),
        "cluster_threshold": cluster_threshold,
        "voiceprint_threshold": vp_threshold,
        "diarization_config": format!("{config:?}"),
        "ami": ami.iter().map(|r| r.1.clone()).collect::<Vec<_>>(),
        "synthetic": {
            "scenes_per_count": SCENES_PER_COUNT,
            "seed_base": SCENE_SEED_BASE,
            "speakers": streams.iter().map(|s| s.0).collect::<Vec<_>>(),
            "der_percent": der_percent(s.0, s.1, s.2, s.3),
            "scenes": synthetic,
        },
        "voiceprints": {"folds": folds, "clip_samples": CLIP},
        "metrics": metrics,
        "seconds": started.elapsed().as_secs_f64(),
    });
    println!("{report}");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diar_eval_rttm_is_parsed_and_checked() {
        let text = "SPEAKER IS1009a 1 54.95 5.9 <NA> <NA> FIE088 <NA> <NA>\n\
                    SPEAKER IS1009a 1 55.7 0.33 <NA> <NA> FIO089 <NA> <NA>\n\
                    SPEAKER IS1009a 1 60.0 0 <NA> <NA> FIO089 <NA> <NA>\n\n";
        let t = parse_rttm(text, "IS1009a").unwrap();
        // The zero-duration turn is dropped, as pyannote does.
        assert_eq!(t.len(), 2);
        assert_eq!(t[0], Turn::new(54.95, 54.95 + 5.9, "FIE088"));
        assert_eq!(t[1].speaker, "FIO089");
        assert_eq!(speaker_count(&t), 2);
        assert!(parse_rttm(text, "ES2004a").is_err());
        assert!(parse_rttm("SPKR IS1009a 1 1 1 x x A", "IS1009a").is_err());
        assert!(parse_rttm("SPEAKER IS1009a 1 x 1 x x A", "IS1009a").is_err());
        assert!(parse_rttm("SPEAKER IS1009a 1 1 -1 x x A", "IS1009a").is_err());
        assert!(parse_rttm("SPEAKER IS1009a 1 1 0 x x A", "IS1009a").is_err());
        assert!(parse_rttm("", "IS1009a").is_err());
    }

    #[test]
    fn diar_eval_median_of_odd_and_even_counts() {
        assert_eq!(median(&[3.0, 0.0, 1.0]), 1.0);
        assert_eq!(median(&[0.0, 2.0, 1.0, 0.0]), 0.5);
        assert_eq!(median(&[5.0]), 5.0);
    }

    #[test]
    fn diar_eval_der_is_accumulated_over_meetings() {
        let r = |m: f64, fa: f64, c: f64, t: f64| DerResult {
            total: t,
            missed: m,
            false_alarm: fa,
            confusion: c,
            mapping: Vec::new(),
        };
        let a = r(1.0, 2.0, 3.0, 10.0);
        let b = r(0.0, 0.0, 1.0, 90.0);
        let (m, fa, c, t) = accumulate([&a, &b]);
        assert_eq!((m, fa, c, t), (1.0, 2.0, 4.0, 100.0));
        // Summed components, not the mean of per-meeting rates (60% and 1.1%).
        assert!((der_percent(m, fa, c, t) - 7.0).abs() < 1e-12);
    }

    fn streams() -> Vec<(u32, Vec<f32>)> {
        // 8 speakers of 20 s, each a distinct constant level so turns are traceable.
        (0..8u32)
            .map(|i| (100 + i, vec![0.01 * (i + 1) as f32; 20 * 16_000]))
            .collect()
    }

    #[test]
    fn diar_eval_scenes_have_the_true_count_and_never_reuse_audio() {
        // Acceptance scene seeds never equal a calibration seed (`100 k + s`, k <= 6).
        const { assert!(SCENE_SEED_BASE > 100 * 6 + SCENES_PER_COUNT) };
        let st = streams();
        for k in SCENE_COUNTS {
            for s in 0..SCENES_PER_COUNT {
                let sc = build_scene(&st, k, s);
                assert_eq!(sc.speakers, k);
                assert_eq!(sc.seed, SCENE_SEED_BASE + 100 * k as u64 + s);
                assert_eq!(speaker_count(&sc.reference), k, "k={k} s={s}");
                assert_eq!(sc.reused_audio(), None, "k={k} s={s}");
                assert_eq!(sc.sources.len(), sc.reference.len());
                let speech: f64 = sc.reference.iter().map(|t| t.end - t.start).sum();
                // 15 s per speaker unless streams run out (20 s each here: they do not).
                assert!(speech >= SPEECH_PER_SPEAKER_S * k as f64, "k={k}: {speech}");
                assert!(sc.reference.iter().all(|t| t.end - t.start >= 1.5 - 1e-9));
                // Reproducible from the seed.
                let again = build_scene(&st, k, s);
                assert_eq!(again.audio, sc.audio);
                assert_eq!(again.reference, sc.reference);
            }
        }
    }

    #[test]
    fn diar_eval_one_speaker_scenes_have_no_overlap() {
        let st = streams();
        for s in 0..SCENES_PER_COUNT {
            let sc = build_scene(&st, 1, s);
            assert!(sc.reference.len() >= 2, "a 15 s scene needs several turns");
            for w in sc.reference.windows(2) {
                assert!(w[1].start > w[0].end, "{w:?}");
            }
        }
    }

    #[test]
    fn diar_eval_reused_audio_finds_two_turns_playing_the_same_samples() {
        let st = streams();
        let mut sc = build_scene(&st, 2, 0);
        assert_eq!(sc.reused_audio(), None);
        // Make the second turn of the first speaker replay its first turn's samples.
        let first = sc.reference[0].speaker.clone();
        let again = (1..sc.reference.len())
            .find(|&j| sc.reference[j].speaker == first)
            .expect("the first speaker talks twice");
        sc.sources[again] = sc.sources[0].start + 1..sc.sources[0].end + 1;
        assert_eq!(sc.reused_audio(), Some((0, again)));
        // The same samples of another speaker's stream are not a reuse.
        let mut other = build_scene(&st, 2, 0);
        other.sources[1] = other.sources[0].clone();
        assert_ne!(other.reference[0].speaker, other.reference[1].speaker);
        assert_eq!(other.reused_audio(), None);
    }

    #[test]
    fn diar_eval_centre_clip_is_2_5_s() {
        let x: Vec<f32> = (0..50_000).map(|i| i as f32).collect();
        let c = centre_clip(&x);
        assert_eq!(c.len(), CLIP);
        assert_eq!(c[0], 5_000.0);
        assert!((2.0..=3.0).contains(&(CLIP as f64 / HZ)));
    }

    #[test]
    fn diar_eval_voiceprint_tally_is_open_set() {
        let clock = ManualClock::new(1_800_000_000_000);
        let mut rng = ChaCha8Rng::seed_from_u64(1);
        let a = EntityId::generate(&clock, &mut rng);
        let b = EntityId::generate(&clock, &mut rng);
        let mut t = VpTally::default();
        t.add_known(
            a,
            &MatchOutcome::Known {
                person: a,
                score: 0.9,
            },
        );
        t.add_known(
            a,
            &MatchOutcome::Known {
                person: b,
                score: 0.8,
            },
        );
        // Right person but below the threshold: a top-1 miss, a closed-set hit.
        t.add_known(
            a,
            &MatchOutcome::Unknown {
                best: Some((a, 0.3)),
            },
        );
        t.add_known(
            a,
            &MatchOutcome::Unknown {
                best: Some((b, 0.3)),
            },
        );
        t.add_unknown(&MatchOutcome::Known {
            person: b,
            score: 0.7,
        });
        t.add_unknown(&MatchOutcome::Unknown {
            best: Some((a, 0.2)),
        });
        t.add_unknown(&MatchOutcome::Unknown { best: None });
        t.add_unknown(&MatchOutcome::Unknown { best: None });
        assert_eq!(
            t,
            VpTally {
                known: 4,
                known_correct: 1,
                known_closed_set_top1: 2,
                known_wrong_person: 1,
                known_rejected: 2,
                unknown: 4,
                unknown_accepted: 1,
            }
        );
        assert_eq!(t.top1_percent(), 25.0);
        assert_eq!(t.false_accept_percent(), 25.0);
    }
}
