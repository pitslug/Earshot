//! Does a sentence's edge word stay with its speaker (T12.88)? Counts only.
//!
//! ```text
//! cargo run --release -p earshot-runtime --example edge_words -- <repo-root> \
//!     --models <dir> [--baseline <report.json>] [--pairs <n>] [--meetings <m,m,..>] \
//!     [--out <file.json>]
//! ```
//!
//! The rule, data, measures and decision are `docs/far-speakers/edge-words.md`, written before
//! the first run. Run once on the build before the candidate (the baseline report), then on the
//! candidate with `--baseline`, which scores with the baseline's speaker maps and gives the
//! verdict. Dev data only: LibriSpeech dev-clean scenes and AMI dev meetings (`Mix-Headset` and
//! `Array1-01`). No word text is printed or written; word text is read to match recognised
//! words to the reference, to ask whether a word ends in punctuation and to count its letters.

#[allow(dead_code)] // shared with asr_eval; only `kept_words` and `latin1` are used here
#[path = "../../earshot-asr-whisper/examples/asr_eval/refs.rs"]
mod refs;

#[allow(dead_code)] // shared with word_changes
#[path = "word_changes_parts/data.rs"]
mod data;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use data::{
    AMI_DEV, SCENE_S, SCENE_SEED, TrueTurn, ami_meeting_words, best_map, build_scene,
    dev_clean_speakers, fixtures_dir, read_mono_wav, turn_at, utterances,
};
use earshot_core::asr::{
    BackendKind, CancelToken, CtcToken, MeetingTime, PieceConfig, TranscribeOptions, Vocab, Word,
    transcribe_in_pieces, words_from_tokens,
};
use earshot_core::audio::{MonoBuffer, PIPELINE_RATE, quantise_pcm16};
use earshot_core::diar::{
    Diarizer, Snap, SpeakerEmbedder, SpeakerTurn, assign_words, snap_changes,
};
use earshot_core::metrics::normalize::normalize_english;
use earshot_runtime::backends::{
    Accelerator, EvidenceLoader, ModelDir, final_pass_asr, init_onnx, load_diarisation,
    load_silero, onnx_library,
};
use earshot_runtime::post::PostConfig;
use serde_json::{Value, json};

/// The production default English model on the desktop.
const DEFAULT_MODEL: &str = "parakeet-tdt-0.6b-v2";
/// A recognised word matches a reference word starting at most this far away (ms).
const MATCH_WINDOW_MS: u64 = 1_000;
/// A true turn of at most this many words is a short one (the rule's "short true turns").
const SHORT_TRUE_RUN: usize = 2;
/// Words of at most this many letters count in `durations`.
const SHORT_WORD_LETTERS: usize = 4;
/// The sets of the rule, in report order.
const SETS: [&str; 3] = ["scenes", "ami_mix", "ami_sdm"];
/// The full data of the rule: scene pairs.
const FULL_PAIRS: usize = 20;
/// Stage 2's held-out meetings: the second and third of each AMI dev series (set `ami-dev`).
const AMI_HELD: [&str; 10] = [
    "ES2003b", "ES2003c", "ES2011b", "ES2011c", "IS1008b", "IS1008c", "TS3004b", "TS3004c",
    "TS3006b", "TS3006c",
];
/// Stage 2's sets, in report order.
const SETS2: [&str; 4] = ["scenes", "ami_mix", "ami_sdm", "ami_held"];
/// Stage 2's variants, in the pick's tie-break order (S = 1 first, then `sentence`).
const VARIANTS2: [Snap; 4] = [
    Snap {
        reach: 1,
        clause_ends: false,
    },
    Snap {
        reach: 1,
        clause_ends: true,
    },
    Snap {
        reach: 2,
        clause_ends: false,
    },
    Snap {
        reach: 2,
        clause_ends: true,
    },
];
/// Stage 2's allowance for short true turns kept: this share of a set's words in them.
const SHORT_TURN_ALLOWANCE: f64 = 0.02;

fn fail(msg: impl std::fmt::Display) -> ! {
    eprintln!("edge_words: {msg}");
    std::process::exit(1);
}

// ---- which word ends this build makes -----------------------------------------------------

/// `"punctuation"` when a trailing punctuation token moves a word's end (the baseline),
/// `"spoken"` when the end stays at the last spoken token (the candidate). Decided on a fixed
/// token list through the production `words_from_tokens`.
fn word_ends() -> &'static str {
    let vocab = Vocab::parse("<unk> 0\n\u{2581}yes 1\n. 2\n<blk> 3\n")
        .unwrap_or_else(|e| fail(format!("probe vocabulary: {e}")));
    let token = |id, first_frame, end_frame| CtcToken {
        id,
        first_frame,
        end_frame,
        log_prob: 0.0,
    };
    let words = words_from_tokens(
        &[token(1, 0, 2), token(2, 6, 7)],
        &vocab,
        Duration::from_millis(80),
    );
    match words.as_slice() {
        [w] if w.text == "yes." && w.end.as_millis() == 560 => "punctuation",
        [w] if w.text == "yes." && w.end.as_millis() == 160 => "spoken",
        _ => fail("words_from_tokens gives neither known end"),
    }
}

// ---- truth ---------------------------------------------------------------------------------

/// A word's text as the matcher compares it: Whisper's English normaliser over the whole word.
fn match_key(text: &str) -> String {
    normalize_english(text)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// Truth of each recognised word: the speaker of the reference word it matches (equal
/// non-empty key, start within [`MATCH_WINDOW_MS`], one to one in time order, the nearest
/// unused start first, the earlier on a tie); `None` when it matches none.
fn match_truth(hyp: &[Word], reference: &[Word], truth: &[Option<usize>]) -> Vec<Option<usize>> {
    let mut by_key: HashMap<String, Vec<usize>> = HashMap::new();
    for (i, w) in reference.iter().enumerate() {
        let k = match_key(&w.text);
        if !k.is_empty() {
            by_key.entry(k).or_default().push(i);
        }
    }
    for v in by_key.values_mut() {
        v.sort_by_key(|&i| (reference[i].start.as_millis(), i));
    }
    let mut used = vec![false; reference.len()];
    let mut order: Vec<usize> = (0..hyp.len()).collect();
    order.sort_by_key(|&i| (hyp[i].start.as_millis(), i));
    let mut out = vec![None; hyp.len()];
    for h in order {
        let Some(cands) = by_key.get(&match_key(&hyp[h].text)) else {
            continue;
        };
        let t = hyp[h].start.as_millis();
        let best = cands
            .iter()
            .copied()
            .filter(|&r| !used[r] && reference[r].start.as_millis().abs_diff(t) <= MATCH_WINDOW_MS)
            .min_by_key(|&r| {
                (
                    reference[r].start.as_millis().abs_diff(t),
                    reference[r].start,
                )
            });
        if let Some(r) = best {
            used[r] = true;
            out[h] = truth[r];
        }
    }
    out
}

/// A scene word's truth: the speaker whose true turn holds its start.
fn scene_truth(word: &Word, turns: &[TrueTurn]) -> Option<usize> {
    turn_at(word.start.as_millis() as f64, turns)
}

// ---- scoring -------------------------------------------------------------------------------

/// One scored item (a scene or a meeting recording).
struct Item {
    name: String,
    set: &'static str,
    words: Vec<Word>,
    labels: Vec<usize>,
    truth: Vec<Option<usize>>,
    k: usize,
    true_k: usize,
    turns_hash: String,
}

#[derive(Debug, Clone, Copy, Default, PartialEq)]
struct Score {
    words: u64,
    scored: u64,
    wrong: u64,
    edge_errors: u64,
    true_changes: u64,
    found_changes: u64,
    short_true_run_words: u64,
    short_true_run_words_right: u64,
}

impl Score {
    fn add(&mut self, o: &Score) {
        self.words += o.words;
        self.scored += o.scored;
        self.wrong += o.wrong;
        self.edge_errors += o.edge_errors;
        self.true_changes += o.true_changes;
        self.found_changes += o.found_changes;
        self.short_true_run_words += o.short_true_run_words;
        self.short_true_run_words_right += o.short_true_run_words_right;
    }
    fn wder(&self) -> f64 {
        self.wrong as f64 / self.scored.max(1) as f64
    }
    fn recall(&self) -> f64 {
        self.found_changes as f64 / self.true_changes.max(1) as f64
    }
    fn json(&self) -> Value {
        json!({
            "words": self.words,
            "words_with_truth": self.scored,
            "wrong": self.wrong,
            "wder_percent": round3(100.0 * self.wder()),
            "edge_errors": self.edge_errors,
            "true_changes": self.true_changes,
            "found_changes": self.found_changes,
            "change_recall_percent": round3(100.0 * self.recall()),
            "words_in_true_runs_of_at_most_2": self.short_true_run_words,
            "of_those_right": self.short_true_run_words_right,
        })
    }
    fn from_json(v: &Value) -> Score {
        let n = |k: &str| {
            v[k].as_u64()
                .unwrap_or_else(|| fail(format!("baseline report: no {k}")))
        };
        Score {
            words: n("words"),
            scored: n("words_with_truth"),
            wrong: n("wrong"),
            edge_errors: n("edge_errors"),
            true_changes: n("true_changes"),
            found_changes: n("found_changes"),
            short_true_run_words: n("words_in_true_runs_of_at_most_2"),
            short_true_run_words_right: n("of_those_right"),
        }
    }
}

fn round3(x: f64) -> f64 {
    (x * 1000.0).round() / 1000.0
}

/// The rule's measures for `labels` against `truth` under `map`.
fn score(labels: &[usize], truth: &[Option<usize>], map: &[Option<usize>]) -> Score {
    let n = labels.len();
    let mut s = Score {
        words: n as u64,
        ..Score::default()
    };
    let mapped = |i: usize| map.get(labels[i]).copied().flatten();
    let right = |i: usize| truth[i].is_some() && mapped(i) == truth[i];
    for i in 0..n {
        let Some(t) = truth[i] else {
            continue;
        };
        s.scored += 1;
        if right(i) {
            continue;
        }
        s.wrong += 1;
        let first = i == 0 || labels[i - 1] != labels[i];
        let last = i + 1 == n || labels[i + 1] != labels[i];
        let before = first && i > 0 && mapped(i - 1) == Some(t);
        let after = last && i + 1 < n && mapped(i + 1) == Some(t);
        s.edge_errors += u64::from(before || after);
    }
    for i in 1..n {
        if let (Some(a), Some(b)) = (truth[i - 1], truth[i])
            && a != b
        {
            s.true_changes += 1;
            let lo = i.saturating_sub(1).max(1);
            let hi = (i + 1).min(n - 1);
            s.found_changes += u64::from((lo..=hi).any(|j| labels[j] != labels[j - 1]));
        }
    }
    let known: Vec<usize> = (0..n).filter(|&i| truth[i].is_some()).collect();
    let mut at = 0;
    while at < known.len() {
        let mut end = at + 1;
        while end < known.len() && truth[known[end]] == truth[known[at]] {
            end += 1;
        }
        if end - at <= SHORT_TRUE_RUN {
            for &i in &known[at..end] {
                s.short_true_run_words += 1;
                s.short_true_run_words_right += u64::from(right(i));
            }
        }
        at = end;
    }
    s
}

/// Median and 90th percentile (sorted value at index round((n - 1) p)) of `v`, ms.
fn spread(mut v: Vec<u64>) -> Value {
    if v.is_empty() {
        return json!({"words": 0});
    }
    v.sort_unstable();
    let at = |p: f64| v[((v.len() - 1) as f64 * p).round() as usize];
    json!({"words": v.len(), "median_ms": at(0.5), "p90_ms": at(0.9)})
}

/// Lengths of words of at most [`SHORT_WORD_LETTERS`] letters, with and without trailing
/// punctuation.
fn durations<'a>(words: impl Iterator<Item = &'a Word>) -> Value {
    let (mut with, mut without) = (Vec::new(), Vec::new());
    for w in words {
        let core = w.text.trim_end_matches(|c: char| c.is_ascii_punctuation());
        if core.chars().filter(|c| c.is_alphabetic()).count() > SHORT_WORD_LETTERS
            || core.is_empty()
        {
            continue;
        }
        let len = w.end.as_millis().saturating_sub(w.start.as_millis());
        if core.len() < w.text.len() {
            with.push(len);
        } else {
            without.push(len);
        }
    }
    json!({"with_punctuation": spread(with), "without_punctuation": spread(without)})
}

/// FNV-1a over the turns, so two runs can show they diarised identically.
fn turns_hash(turns: &[SpeakerTurn]) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for t in turns {
        for v in [t.speaker as u64, t.start.as_millis(), t.end.as_millis()] {
            for b in v.to_le_bytes() {
                h ^= u64::from(b);
                h = h.wrapping_mul(0x0100_0000_01b3);
            }
        }
    }
    format!("{h:016x}")
}

// ---- the report ----------------------------------------------------------------------------

/// The map each item is scored with: the baseline report's when given, else its own.
fn maps(items: &[Item], baseline: Option<&Value>) -> Vec<Vec<Option<usize>>> {
    items
        .iter()
        .map(|it| match baseline {
            None => best_map(&it.labels, &it.truth, it.k, it.true_k),
            Some(b) => {
                let found = b["items"]
                    .as_array()
                    .and_then(|a| a.iter().find(|x| x["name"] == json!(it.name)))
                    .unwrap_or_else(|| fail(format!("baseline report: no item {}", it.name)));
                let map: Vec<Option<usize>> = serde_json::from_value(found["map"].clone())
                    .unwrap_or_else(|e| fail(format!("baseline map of {}: {e}", it.name)));
                if map.len() != it.k {
                    fail(format!(
                        "{}: baseline found {} speakers",
                        it.name,
                        map.len()
                    ));
                }
                map
            }
        })
        .collect()
}

/// The decision of `docs/far-speakers/edge-words.md`, from the two reports' pooled scores.
fn verdict(
    base: &HashMap<&str, Score>,
    cand: &HashMap<&str, Score>,
    same_turns: bool,
    full: bool,
    ends_right: bool,
) -> Value {
    let get = |m: &HashMap<&str, Score>, s: &str| -> Option<Score> { m.get(s).copied() };
    let pair = |s: &str| get(base, s).zip(get(cand, s));
    let c1 = ["ami_mix", "ami_sdm"].iter().all(|s| {
        pair(s).is_some_and(|(b, c)| c.edge_errors < b.edge_errors && c.wder() < b.wder())
    });
    let c2 =
        pair("scenes").is_some_and(|(b, c)| c.edge_errors <= b.edge_errors && c.wder() <= b.wder());
    let c3 = SETS.iter().all(|s| {
        pair(s).is_some_and(|(b, c)| c.short_true_run_words_right >= b.short_true_run_words_right)
    });
    let c4 = SETS
        .iter()
        .all(|s| pair(s).is_some_and(|(b, c)| b.recall() - c.recall() <= 0.01));
    json!({
        "1_ami_mix_and_sdm_fewer_edge_errors_and_lower_wder": c1,
        "2_scenes_edge_errors_and_wder_not_higher": c2,
        "3_short_true_turns_kept_not_fewer": c3,
        "4_change_recall_within_1_point": c4,
        "5_diarisation_identical_in_both_runs": same_turns,
        "full_data_and_both_word_ends": full && ends_right,
        "ships": c1 && c2 && c3 && c4 && same_turns && full && ends_right,
    })
}

fn labels_of(words: &[Word], turns: &[SpeakerTurn]) -> Vec<usize> {
    assign_words(words, turns)
        .into_iter()
        .map(|l| l.unwrap_or_else(|| fail("a diarisation with no turns")))
        .collect()
}

struct Tools<S, E, T, V> {
    diarizer: Diarizer<S, E>,
    transcriber: T,
    vad: V,
    cancel: CancelToken,
}

impl<S, E, T, V> Tools<S, E, T, V>
where
    S: earshot_core::diar::SpeakerSegmenter,
    E: SpeakerEmbedder,
    T: earshot_core::asr::Transcriber,
    V: earshot_core::asr::Vad,
{
    /// Diarisation turns and kept words of `audio` (quantised as the stored FLAC).
    fn run(&mut self, audio: &[f32], what: &str) -> (Vec<SpeakerTurn>, usize, Vec<Word>) {
        let buffer = MonoBuffer::new(PIPELINE_RATE, quantise_pcm16(audio));
        let diar = self
            .diarizer
            .diarize(&buffer, MeetingTime::ZERO, &self.cancel)
            .unwrap_or_else(|e| fail(format!("diarize {what}: {e}")));
        let (t, _) = transcribe_in_pieces(
            &mut self.transcriber,
            &mut self.vad,
            &buffer,
            &TranscribeOptions::live(),
            &PieceConfig::default(),
            &self.cancel,
        )
        .unwrap_or_else(|e| fail(format!("pieces {what}: {e}")));
        (diar.turns, diar.speakers.len(), refs::kept_words(&t))
    }
}

fn main() {
    let mut it = std::env::args().skip(1);
    let _root = it
        .next()
        .unwrap_or_else(|| fail("usage: edge_words <repo-root> --models <dir>"));
    let (mut models, mut out, mut baseline_path) = (None, None, None);
    let mut stage2 = false;
    let mut pairs = FULL_PAIRS;
    let mut meetings: Vec<String> = AMI_DEV.iter().map(|m| (*m).to_owned()).collect();
    while let Some(flag) = it.next() {
        if flag == "--stage2" {
            stage2 = true;
            continue;
        }
        let value = it
            .next()
            .unwrap_or_else(|| fail(format!("{flag} needs a value")));
        match flag.as_str() {
            "--models" => models = Some(ModelDir::new(value)),
            "--out" => out = Some(PathBuf::from(value)),
            "--baseline" => baseline_path = Some(PathBuf::from(value)),
            "--pairs" => {
                pairs = value
                    .parse::<usize>()
                    .unwrap_or_else(|e| fail(format!("--pairs: {e}")))
                    .min(FULL_PAIRS)
            }
            "--meetings" => {
                meetings = value
                    .split(',')
                    .filter(|m| !m.is_empty())
                    .map(str::to_owned)
                    .collect();
            }
            other => fail(format!("unknown argument {other}")),
        }
    }
    for m in &meetings {
        if !AMI_DEV.contains(&m.as_str()) {
            fail(format!("{m} is not an AMI dev meeting of this report"));
        }
    }
    let ends = word_ends();
    if stage2 && baseline_path.is_some() {
        fail("--stage2 scores its own baseline; it takes no --baseline");
    }
    let baseline: Option<Value> = baseline_path.as_deref().map(read_report);
    let models = models.unwrap_or_else(|| fail("--models is required"));
    init_onnx(&models, onnx_library(DEFAULT_MODEL, &[Accelerator::Cpu]))
        .unwrap_or_else(|e| fail(format!("ONNX Runtime: {e}")));
    let (segmenter, embedder) =
        load_diarisation(&models, Accelerator::Cpu).unwrap_or_else(|e| fail(e));
    let model = embedder.info().model.clone();
    let post = PostConfig::default();
    let mut config = post.diarization.clone();
    config.clustering.threshold = post
        .clustering_threshold(&model)
        .unwrap_or_else(|e| fail(e));
    config.clustering.regroup = post.regroup(&model);
    let threshold = config.clustering.threshold;
    let diarizer = Diarizer::new(segmenter, embedder, config).unwrap_or_else(|e| fail(e));
    let choice = final_pass_asr(
        &EvidenceLoader::default(),
        &models,
        DEFAULT_MODEL,
        &[Accelerator::Cpu],
    )
    .unwrap_or_else(|e| fail(format!("final-pass model: {e}")));
    if choice.backend != BackendKind::Cpu {
        fail("the final-pass model is not on the CPU");
    }
    let vad =
        load_silero(&models, Accelerator::Cpu).unwrap_or_else(|e| fail(format!("Silero: {e}")));
    let mut tools = Tools {
        diarizer,
        transcriber: choice.transcriber,
        vad,
        cancel: CancelToken::new(),
    };

    let fixtures = fixtures_dir();
    let mut items: Vec<Item> = Vec::new();
    let speakers = dev_clean_speakers(&fixtures);
    let want = SCENE_S * PIPELINE_RATE.hz() as usize;
    for p in 0..pairs {
        eprintln!("edge_words: scene {p}");
        let a = utterances(&speakers[2 * p], want);
        let b = utterances(&speakers[2 * p + 1], want);
        let (audio, true_turns) = build_scene([&a, &b], SCENE_SEED + p as u64, want);
        let (turns, k, words) = tools.run(&audio, &format!("scene {p}"));
        let truth = words.iter().map(|w| scene_truth(w, &true_turns)).collect();
        items.push(Item {
            name: format!("scene-{p:02}"),
            set: "scenes",
            labels: labels_of(&words, &turns),
            words,
            truth,
            k,
            true_k: 2,
            turns_hash: turns_hash(&turns),
        });
    }
    let annotations = fixtures.join("ami/annotations");
    for (set, mic, fixture_set) in [
        ("ami_mix", "Mix-Headset", "ami-dev"),
        ("ami_sdm", "Array1-01", "ami-dev-sdm"),
    ] {
        for m in &meetings {
            eprintln!("edge_words: {m} {mic}");
            let audio = read_mono_wav(
                &fixtures.join(format!("ami/audio/{m}.{mic}.wav")),
                fixture_set,
            );
            let (turns, k, words) = tools.run(&audio, &format!("{m} {mic}"));
            let (reference, ref_truth, true_k) = ami_meeting_words(&annotations, m);
            let truth = match_truth(&words, &reference, &ref_truth);
            items.push(Item {
                name: format!("{m}.{mic}"),
                set,
                labels: labels_of(&words, &turns),
                words,
                truth,
                k,
                true_k,
                turns_hash: turns_hash(&turns),
            });
        }
    }
    if stage2 {
        for m in AMI_HELD {
            eprintln!("edge_words: {m} Mix-Headset (held out)");
            let audio = read_mono_wav(
                &fixtures.join(format!("ami/audio/{m}.Mix-Headset.wav")),
                "ami-dev",
            );
            let (turns, k, words) = tools.run(&audio, &format!("{m} Mix-Headset"));
            let (reference, ref_truth, true_k) = ami_meeting_words(&annotations, m);
            let truth = match_truth(&words, &reference, &ref_truth);
            items.push(Item {
                name: format!("{m}.Mix-Headset"),
                set: "ami_held",
                labels: labels_of(&words, &turns),
                words,
                truth,
                k,
                true_k,
                turns_hash: turns_hash(&turns),
            });
        }
        let full = pairs == FULL_PAIRS && meetings.len() == AMI_DEV.len();
        let report = stage2_report(&items, ends, full);
        let report = json!({
            "task": "T12.88 stage 2",
            "word_ends": ends,
            "transcription_model": DEFAULT_MODEL,
            "embedding_model": model.to_string(),
            "cluster_threshold": threshold,
            "pairs": pairs,
            "ami_meetings": meetings,
            "ami_held": AMI_HELD,
            "full_data": full,
            "stage2": report,
        });
        let text = serde_json::to_string_pretty(&report).unwrap_or_else(|e| fail(e));
        if let Some(out) = &out {
            std::fs::write(out, &text).unwrap_or_else(|e| fail(format!("{}: {e}", out.display())));
        }
        println!("{text}");
        return;
    }

    let maps = maps(&items, baseline.as_ref());
    let mut pooled: HashMap<&str, Score> = HashMap::new();
    let mut item_rows = Vec::new();
    for (it, map) in items.iter().zip(&maps) {
        let s = score(&it.labels, &it.truth, map);
        pooled.entry(it.set).or_default().add(&s);
        item_rows.push(json!({
            "name": it.name,
            "set": it.set,
            "turns_hash": it.turns_hash,
            "found_speakers": it.k,
            "true_speakers": it.true_k,
            "map": map,
            "score": s.json(),
        }));
    }
    let sets: Vec<Value> = SETS
        .iter()
        .filter_map(|s| {
            let score = pooled.get(s)?;
            Some(json!({
                "set": s,
                "score": score.json(),
                "durations": durations(items.iter().filter(|i| i.set == *s).flat_map(|i| &i.words)),
            }))
        })
        .collect();
    let full = pairs == FULL_PAIRS && meetings.len() == AMI_DEV.len();
    let verdict = baseline.as_ref().map(|b| {
        let base: HashMap<&str, Score> = SETS
            .iter()
            .filter_map(|s| {
                let row = b["sets"]
                    .as_array()?
                    .iter()
                    .find(|r| r["set"] == json!(s))?;
                Some((*s, Score::from_json(&row["score"])))
            })
            .collect();
        let same_turns = items.iter().all(|it| {
            b["items"].as_array().is_some_and(|a| {
                a.iter()
                    .any(|x| x["name"] == json!(it.name) && x["turns_hash"] == json!(it.turns_hash))
            })
        });
        let base_full = b["full_data"].as_bool() == Some(true);
        let ends_right = b["word_ends"] == json!("punctuation") && ends == "spoken";
        verdict(&base, &pooled, same_turns, full && base_full, ends_right)
    });
    let report = json!({
        "task": "T12.88",
        "word_ends": ends,
        "transcription_model": DEFAULT_MODEL,
        "embedding_model": model.to_string(),
        "cluster_threshold": threshold,
        "pairs": pairs,
        "ami_meetings": meetings,
        "full_data": full,
        "baseline": baseline_path.as_ref().map(|p| p.display().to_string()),
        "sets": sets,
        "items": item_rows,
        "verdict": verdict,
    });
    let text = serde_json::to_string_pretty(&report).unwrap_or_else(|e| fail(e));
    if let Some(out) = &out {
        std::fs::write(out, &text).unwrap_or_else(|e| fail(format!("{}: {e}", out.display())));
    }
    println!("{text}");
}

// ---- stage 2 -------------------------------------------------------------------------------

fn variant_json(v: Snap) -> Value {
    json!({"reach": v.reach, "support": if v.clause_ends { "clause" } else { "sentence" }})
}

/// Baseline and every variant's score per set, each item scored with its baseline map.
fn stage2_scores(items: &[Item]) -> HashMap<&'static str, (Score, Vec<Score>)> {
    let mut out: HashMap<&'static str, (Score, Vec<Score>)> = HashMap::new();
    for it in items {
        let map = best_map(&it.labels, &it.truth, it.k, it.true_k);
        let entry = out
            .entry(it.set)
            .or_insert_with(|| (Score::default(), vec![Score::default(); VARIANTS2.len()]));
        entry.0.add(&score(&it.labels, &it.truth, &map));
        let base: Vec<Option<usize>> = it.labels.iter().map(|&l| Some(l)).collect();
        for (row, v) in entry.1.iter_mut().zip(VARIANTS2) {
            let snapped: Vec<usize> = snap_changes(&it.words, &base, v)
                .into_iter()
                .map(|l| l.unwrap_or_else(|| fail("a snapped word lost its speaker")))
                .collect();
            row.add(&score(&snapped, &it.truth, &map));
        }
    }
    out
}

/// The variant the rule picks on `ami_mix` and `ami_sdm` pooled: fewest edge errors, then
/// lowest WDER, then the earlier in [`VARIANTS2`].
fn stage2_pick(scores: &HashMap<&str, (Score, Vec<Score>)>) -> usize {
    let pooled = |i: usize| {
        let mut s = Score::default();
        for set in ["ami_mix", "ami_sdm"] {
            if let Some((_, rows)) = scores.get(set) {
                s.add(&rows[i]);
            }
        }
        s
    };
    let mut best = 0;
    for i in 1..VARIANTS2.len() {
        let (a, b) = (pooled(i), pooled(best));
        if a.edge_errors < b.edge_errors || (a.edge_errors == b.edge_errors && a.wder() < b.wder())
        {
            best = i;
        }
    }
    best
}

/// Stage 2's decision for variant `pick` (`docs/far-speakers/edge-words.md`).
fn stage2_verdict(
    scores: &HashMap<&str, (Score, Vec<Score>)>,
    pick: usize,
    full: bool,
    ends_today: bool,
) -> Value {
    let pair = |s: &str| scores.get(s).map(|(b, rows)| (*b, rows[pick]));
    let c1 =
        pair("ami_held").is_some_and(|(b, c)| c.edge_errors < b.edge_errors && c.wder() < b.wder());
    let c2 = {
        let (mix, sdm) = (pair("ami_mix"), pair("ami_sdm"));
        match (mix, sdm) {
            (Some((mb, mc)), Some((sb, sc))) => {
                mc.edge_errors + sc.edge_errors < mb.edge_errors + sb.edge_errors
                    && mc.wder() <= mb.wder()
                    && sc.wder() <= sb.wder()
            }
            _ => false,
        }
    };
    let c3 =
        pair("scenes").is_some_and(|(b, c)| c.edge_errors <= b.edge_errors && c.wder() <= b.wder());
    let c4 = SETS2.iter().all(|s| {
        pair(s).is_some_and(|(b, c)| {
            let lost = b.short_true_run_words_right as f64 - c.short_true_run_words_right as f64;
            lost <= SHORT_TURN_ALLOWANCE * b.short_true_run_words as f64
        })
    });
    let c5 = SETS2
        .iter()
        .all(|s| pair(s).is_some_and(|(b, c)| b.recall() - c.recall() <= 0.01));
    json!({
        "picked": variant_json(VARIANTS2[pick]),
        "1_ami_held_fewer_edge_errors_and_lower_wder": c1,
        "2_ami_mix_and_sdm_fewer_edge_errors_wder_not_higher": c2,
        "3_scenes_not_worse": c3,
        "4_short_true_turns_kept_within_2_percent": c4,
        "5_change_recall_within_1_point": c5,
        "full_data_and_todays_word_ends": full && ends_today,
        "ships": c1 && c2 && c3 && c4 && c5 && full && ends_today,
    })
}

fn stage2_report(items: &[Item], ends: &str, full: bool) -> Value {
    let scores = stage2_scores(items);
    let pick = stage2_pick(&scores);
    let sets: Vec<Value> = SETS2
        .iter()
        .filter_map(|s| {
            let (base, rows) = scores.get(s)?;
            Some(json!({
                "set": s,
                "items": items.iter().filter(|i| i.set == *s).count(),
                "baseline": base.json(),
                "variants": VARIANTS2.iter().zip(rows).map(|(v, r)| {
                    json!({"variant": variant_json(*v), "score": r.json()})
                }).collect::<Vec<_>>(),
            }))
        })
        .collect();
    let item_rows: Vec<Value> = items
        .iter()
        .map(|it| {
            json!({
                "name": it.name,
                "set": it.set,
                "turns_hash": it.turns_hash,
                "found_speakers": it.k,
                "true_speakers": it.true_k,
            })
        })
        .collect();
    json!({
        "sets": sets,
        "items": item_rows,
        "verdict": stage2_verdict(&scores, pick, full, ends == "punctuation"),
    })
}

fn read_report(path: &Path) -> Value {
    let text =
        std::fs::read_to_string(path).unwrap_or_else(|e| fail(format!("{}: {e}", path.display())));
    serde_json::from_str(&text).unwrap_or_else(|e| fail(format!("{}: {e}", path.display())))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn w(text: &str, start: u64, end: u64) -> Word {
        Word {
            text: text.to_owned(),
            start: MeetingTime::from_millis(start),
            end: MeetingTime::from_millis(end),
            probability: 1.0,
        }
    }

    #[test]
    fn edge_words_this_build_makes_a_known_word_end() {
        assert!(["punctuation", "spoken"].contains(&word_ends()));
    }

    #[test]
    fn edge_words_match_by_text_and_nearest_start_one_to_one() {
        let reference = vec![
            w("Hello", 1000, 1300),
            w("there.", 1300, 1600),
            w("hello", 2500, 2800),
            w("um", 3000, 3100),
        ];
        let truth = vec![Some(0), Some(0), Some(1), Some(1)];
        let hyp = vec![
            w("hello,", 1100, 1300), // nearest is the first "Hello"
            w("there", 1350, 1700),
            w("hello", 2000, 2300), // the first is used; the second is 500 ms away
            w("hello", 2600, 2700), // both used
            w("there", 5000, 5100), // outside the window
        ];
        assert_eq!(
            match_truth(&hyp, &reference, &truth),
            vec![Some(0), Some(0), Some(1), None, None]
        );
    }

    #[test]
    fn edge_words_an_error_at_a_run_edge_toward_its_true_neighbour_is_an_edge_error() {
        let map = vec![Some(0), Some(1)];
        // Speaker 0's last word went to speaker 1's run: an edge error.
        let s = score(
            &[0, 0, 1, 1, 1],
            &[Some(0), Some(0), Some(0), Some(1), Some(1)],
            &map,
        );
        assert_eq!((s.wrong, s.edge_errors), (1, 1));
        // A wrong word in the middle of a run is wrong but no edge error.
        let s = score(
            &[0, 0, 0, 0, 0],
            &[Some(0), Some(0), Some(1), Some(0), Some(0)],
            &map,
        );
        assert_eq!((s.wrong, s.edge_errors), (1, 0));
        // The last word of a run whose truth is the next run's speaker.
        let s = score(
            &[0, 0, 0, 1, 1],
            &[Some(0), Some(0), Some(1), Some(1), Some(1)],
            &map,
        );
        assert_eq!((s.wrong, s.edge_errors), (1, 1));
        // Words without truth are not scored, but their labels still mark a run's edge.
        let s = score(&[0, 1], &[None, Some(0)], &map);
        assert_eq!((s.scored, s.wrong, s.edge_errors), (1, 1, 1));
        let s = score(&[1, 1], &[None, Some(0)], &map);
        assert_eq!((s.scored, s.wrong, s.edge_errors), (1, 1, 0));
    }

    #[test]
    fn edge_words_short_true_turns_and_recall_are_counted() {
        let map = vec![Some(0), Some(1)];
        let t = |v: [usize; 7]| v.map(Some).to_vec();
        // A one-word true turn of speaker 1 between runs of three, labelled right.
        let s = score(&[0, 0, 0, 1, 0, 0, 0], &t([0, 0, 0, 1, 0, 0, 0]), &map);
        assert_eq!(
            (s.short_true_run_words, s.short_true_run_words_right),
            (1, 1)
        );
        assert_eq!((s.true_changes, s.found_changes), (2, 2));
        // Missed: labelled with its neighbours.
        let s = score(&[0; 7], &t([0, 0, 0, 1, 0, 0, 0]), &map);
        assert_eq!((s.short_true_run_words_right, s.found_changes), (0, 0));
        // A two-word true turn is short too.
        let s = score(&[0; 7], &t([0, 0, 0, 1, 1, 0, 0]), &map);
        assert_eq!(s.short_true_run_words, 4);
    }

    #[test]
    fn edge_words_durations_split_short_words_by_trailing_punctuation() {
        let words = [
            w("yes.", 0, 400),
            w("yes", 0, 160),
            w("no", 0, 120),
            w("because", 0, 900),
        ];
        let d = durations(words.iter());
        assert_eq!(d["with_punctuation"]["words"], json!(1));
        assert_eq!(d["with_punctuation"]["median_ms"], json!(400));
        assert_eq!(d["without_punctuation"]["words"], json!(2));
        assert_eq!(d["without_punctuation"]["p90_ms"], json!(160));
    }

    fn score_with(edge_errors: u64, wrong: u64, right_short: u64, found: u64) -> Score {
        Score {
            words: 1000,
            scored: 1000,
            wrong,
            edge_errors,
            true_changes: 100,
            found_changes: found,
            short_true_run_words: 50,
            short_true_run_words_right: right_short,
        }
    }

    fn stage2_with(
        base: Score,
        variant: Score,
        set_override: Option<(&'static str, Score)>,
    ) -> HashMap<&'static str, (Score, Vec<Score>)> {
        SETS2
            .iter()
            .map(|s| {
                let v = match set_override {
                    Some((name, o)) if name == *s => o,
                    _ => variant,
                };
                (*s, (base, vec![v; VARIANTS2.len()]))
            })
            .collect()
    }

    #[test]
    fn edge_words_stage2_pick_takes_fewest_edge_errors_then_wder_then_order() {
        let base = score_with(20, 100, 30, 50);
        let mut scores = stage2_with(base, base, None);
        // Equal everywhere: the first variant (reach 1, sentence).
        assert_eq!(stage2_pick(&scores), 0);
        // Fewer edge errors on one AMI set for variant 2.
        scores.get_mut("ami_sdm").unwrap().1[2] = score_with(15, 100, 30, 50);
        assert_eq!(stage2_pick(&scores), 2);
        // Variant 3 ties on edge errors with a lower WDER.
        scores.get_mut("ami_sdm").unwrap().1[3] = score_with(15, 99, 30, 50);
        assert_eq!(stage2_pick(&scores), 3);
        // The held-out set never decides the pick.
        scores.get_mut("ami_held").unwrap().1[1] = score_with(0, 0, 30, 50);
        assert_eq!(stage2_pick(&scores), 3);
    }

    #[test]
    fn edge_words_stage2_verdict_needs_every_condition() {
        let base = score_with(20, 100, 30, 50);
        let better = score_with(10, 90, 30, 50);
        let ok = |s: &HashMap<&str, (Score, Vec<Score>)>| {
            stage2_verdict(s, 0, true, true)["ships"].clone()
        };
        assert_eq!(ok(&stage2_with(base, better, None)), json!(true));
        assert_eq!(
            stage2_verdict(&stage2_with(base, better, None), 0, false, true)["ships"],
            json!(false)
        );
        assert_eq!(
            stage2_verdict(&stage2_with(base, better, None), 0, true, false)["ships"],
            json!(false)
        );
        // Held-out not better.
        assert_eq!(
            ok(&stage2_with(base, better, Some(("ami_held", base)))),
            json!(false)
        );
        // One AMI pick set with a higher WDER.
        let worse_wder = score_with(10, 101, 30, 50);
        assert_eq!(
            ok(&stage2_with(base, better, Some(("ami_mix", worse_wder)))),
            json!(false)
        );
        // Scenes worse.
        assert_eq!(
            ok(&stage2_with(
                base,
                better,
                Some(("scenes", score_with(21, 90, 30, 50)))
            )),
            json!(false)
        );
        // Short true turns: 1 of 50 lost is inside 2%, 2 is not.
        assert_eq!(
            ok(&stage2_with(
                base,
                better,
                Some(("scenes", score_with(10, 90, 29, 50)))
            )),
            json!(true)
        );
        assert_eq!(
            ok(&stage2_with(
                base,
                better,
                Some(("scenes", score_with(10, 90, 28, 50)))
            )),
            json!(false)
        );
        // Recall down by more than a point.
        assert_eq!(
            ok(&stage2_with(
                base,
                better,
                Some(("ami_held", score_with(10, 90, 30, 48)))
            )),
            json!(false)
        );
        // A missing set.
        let mut missing = stage2_with(base, better, None);
        missing.remove("ami_held");
        assert_eq!(ok(&missing), json!(false));
    }

    #[test]
    fn edge_words_the_verdict_needs_every_condition() {
        let base: HashMap<&str, Score> = SETS
            .iter()
            .map(|s| (*s, score_with(20, 100, 30, 50)))
            .collect();
        let better: HashMap<&str, Score> = SETS
            .iter()
            .map(|s| (*s, score_with(10, 90, 30, 50)))
            .collect();
        assert_eq!(
            verdict(&base, &better, true, true, true)["ships"],
            json!(true)
        );
        assert_eq!(
            verdict(&base, &better, false, true, true)["ships"],
            json!(false)
        );
        assert_eq!(
            verdict(&base, &better, true, false, true)["ships"],
            json!(false)
        );
        assert_eq!(
            verdict(&base, &better, true, true, false)["ships"],
            json!(false)
        );
        // No change on the AMI sets is not enough.
        assert_eq!(
            verdict(&base, &base, true, true, true)["ships"],
            json!(false)
        );
        // A lost short true turn on any set fails.
        let mut lost = better.clone();
        lost.insert("scenes", score_with(10, 90, 29, 50));
        assert_eq!(
            verdict(&base, &lost, true, true, true)["ships"],
            json!(false)
        );
        // Recall down by more than a point fails.
        let mut recall = better.clone();
        recall.insert("ami_sdm", score_with(10, 90, 30, 48));
        assert_eq!(
            verdict(&base, &recall, true, true, true)["ships"],
            json!(false)
        );
        // A missing set fails.
        let mut missing = better.clone();
        missing.remove("ami_sdm");
        assert_eq!(
            verdict(&base, &missing, true, true, true)["ships"],
            json!(false)
        );
    }
}
