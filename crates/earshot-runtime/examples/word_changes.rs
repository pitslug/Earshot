//! Does a word-boundary rule make word-level speaker attribution better (T6.9a)? Counts only.
//!
//! ```text
//! cargo run --release -p earshot-runtime --example word_changes -- <repo-root> \
//!     --models <dir> [--pairs <n>] [--meetings <m,m,..>] [--out <file.json>]
//! ```
//!
//! The rule, data, measures and success criterion are `docs/far-speakers/word-changes.md`,
//! written before the first run. Dev data only: LibriSpeech dev-clean scenes and AMI dev-series
//! meetings. No word text is printed or written; word text is read only to ask whether a word
//! ends a sentence.

#[allow(dead_code)] // shared with asr_eval; only `kept_words` is used here
#[path = "../../earshot-asr-whisper/examples/asr_eval/refs.rs"]
mod refs;

use std::fs::File;
use std::io::BufReader;
use std::path::{Path, PathBuf};

use earshot_core::asr::{
    BackendKind, CancelToken, MeetingTime, PieceConfig, TranscribeOptions, Word,
    transcribe_in_pieces,
};
use earshot_core::audio::{MonoBuffer, PIPELINE_RATE, quantise_pcm16, read_wav};
use earshot_core::diar::{Diarizer, SpeakerEmbedder, SpeakerTurn, assign_words};
use earshot_runtime::backends::{
    Accelerator, EvidenceLoader, ModelDir, final_pass_asr, init_onnx, load_diarisation,
    load_silero, onnx_library,
};
use earshot_runtime::post::PostConfig;
use serde_json::{Value, json};

/// The production default English model on the desktop.
const DEFAULT_MODEL: &str = "parakeet-tdt-0.6b-v2";
/// A pause of at least this supports a speaker change (ms; T6.9's `NO_PAUSE_MS`).
const PAUSE_MS: u64 = 300;
/// A flip is a run of at most this many words with one speaker on both sides (T6.9's count).
const FLIP_WORDS: usize = 3;
/// Length of one dev scene (s).
const SCENE_S: usize = 240;
/// Pause between the utterances of a turn (ms).
const UTTERANCE_GAP_MS: usize = 200;
/// Scene sequence seed, plus the pair's index.
const SCENE_SEED: u64 = 6_930;
/// The first meeting of each AMI dev series.
const AMI_DEV: [&str; 5] = ["ES2003a", "ES2011a", "IS1008a", "TS3004a", "TS3006a"];
/// Letters a speaker's words file can have.
const AMI_LETTERS: [&str; 5] = ["A", "B", "C", "D", "E"];

fn fail(msg: impl std::fmt::Display) -> ! {
    eprintln!("word_changes: {msg}");
    std::process::exit(1);
}

// ---- the rule ----------------------------------------------------------------------------

/// One variant of the rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Variant {
    /// Runs shorter than this may be merged.
    n: usize,
    /// How far a change may snap (words); 0 is no snapping.
    s: usize,
}

/// Every variant tried, in the tie-break order of the success criterion.
const VARIANTS: [Variant; 6] = [
    Variant { n: 2, s: 0 },
    Variant { n: 2, s: 2 },
    Variant { n: 3, s: 0 },
    Variant { n: 3, s: 2 },
    Variant { n: 5, s: 0 },
    Variant { n: 5, s: 2 },
];

fn ends_sentence(word: &str) -> bool {
    word.trim_end_matches(['"', '\'', ')'])
        .ends_with(['.', '?', '!'])
}

/// Is the boundary between words `i - 1` and `i` supported?
fn supported(words: &[Word], i: usize) -> bool {
    ends_sentence(&words[i - 1].text)
        || words[i]
            .start
            .as_millis()
            .saturating_sub(words[i - 1].end.as_millis())
            >= PAUSE_MS
}

/// `(start, end)` of the runs of equal labels.
fn runs_of(labels: &[usize]) -> Vec<(usize, usize)> {
    let mut runs: Vec<(usize, usize)> = Vec::new();
    for (i, l) in labels.iter().enumerate() {
        match runs.last_mut() {
            Some(r) if labels[r.0] == *l => r.1 = i + 1,
            _ => runs.push((i, i + 1)),
        }
    }
    runs
}

/// The rule of `docs/far-speakers/word-changes.md` over the labels of `words`.
fn apply(words: &[Word], labels: &[usize], v: Variant) -> Vec<usize> {
    let mut out = labels.to_vec();
    // Steps 1 and 2, left to right: a run already merged extends its neighbour.
    let mut at = 0;
    while at < out.len() {
        let mut end = at + 1;
        while end < out.len() && out[end] == out[at] {
            end += 1;
        }
        if at > 0 && end - at < v.n {
            let previous = out[at - 1];
            let start_supported = supported(words, at);
            let end_supported = end == out.len() || supported(words, end);
            let flip =
                end < out.len() && out[end] == previous && !(start_supported && end_supported);
            if !start_supported || flip {
                out[at..end].iter_mut().for_each(|l| *l = previous);
                // Read the merged run again from its own start, with what follows it. Each
                // merge removes a run, so this ends.
                while at > 0 && out[at - 1] == previous {
                    at -= 1;
                }
                continue;
            }
        }
        at = end;
    }
    // Step 3: changes left on an unsupported boundary move to the nearest supported one.
    if v.s > 0 {
        let changes: Vec<usize> = (1..out.len()).filter(|&i| out[i] != out[i - 1]).collect();
        for c in changes {
            if out[c] == out[c - 1] || supported(words, c) {
                continue;
            }
            let (before, after) = (out[c - 1], out[c]);
            let run_start = (0..c)
                .rev()
                .take_while(|&j| out[j] == before)
                .last()
                .unwrap_or(c);
            let run_end = (c..out.len())
                .take_while(|&j| out[j] == after)
                .last()
                .map_or(c, |j| j + 1);
            // The nearest supported boundary inside the two runs that leaves both non-empty;
            // on equal distance the earlier one.
            let target = (1..=v.s).find_map(|d| {
                let back = c
                    .checked_sub(d)
                    .filter(|&b| b > run_start && supported(words, b));
                let forward = Some(c + d).filter(|&f| f < run_end && supported(words, f));
                back.or(forward)
            });
            if let Some(t) = target {
                if t < c {
                    out[t..c].iter_mut().for_each(|l| *l = after);
                } else {
                    out[c..t].iter_mut().for_each(|l| *l = before);
                }
            }
        }
    }
    out
}

// ---- scoring -----------------------------------------------------------------------------

/// A scored data item: labels of the baseline, truth (None: left out), words.
struct Item {
    words: Vec<Word>,
    baseline: Vec<usize>,
    truth: Vec<Option<usize>>,
    /// Diarised speakers.
    k: usize,
    /// True speakers.
    true_k: usize,
    left_out: usize,
}

/// The one-to-one map of diarised onto true speakers with the most baseline agreement;
/// `None` for a diarised speaker left unmapped.
fn best_map(item: &Item) -> Vec<Option<usize>> {
    let mut counts = vec![vec![0u64; item.true_k]; item.k];
    for (l, t) in item.baseline.iter().zip(&item.truth) {
        if let Some(t) = t {
            counts[*l][*t] += 1;
        }
    }
    fn search(
        k: usize,
        counts: &[Vec<u64>],
        used: &mut Vec<bool>,
        map: &mut Vec<Option<usize>>,
        score: u64,
        best: &mut (u64, Vec<Option<usize>>),
    ) {
        if k == counts.len() {
            if score > best.0 || best.1.is_empty() {
                *best = (score, map.clone());
            }
            return;
        }
        map[k] = None;
        search(k + 1, counts, used, map, score, best);
        for t in 0..used.len() {
            if !used[t] {
                used[t] = true;
                map[k] = Some(t);
                search(k + 1, counts, used, map, score + counts[k][t], best);
                used[t] = false;
                map[k] = None;
            }
        }
    }
    let mut best = (0, Vec::new());
    search(
        0,
        &counts,
        &mut vec![false; item.true_k],
        &mut vec![None; item.k],
        0,
        &mut best,
    );
    best.1
}

/// Counts over one or more items for one set of labels.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
struct Score {
    scored: u64,
    wrong: u64,
    true_changes: u64,
    found_changes: u64,
    label_changes: u64,
    unsupported_changes: u64,
    flips: u64,
    short_true_run_words: u64,
    short_true_run_words_right: u64,
}

impl Score {
    fn add(&mut self, o: &Score) {
        self.scored += o.scored;
        self.wrong += o.wrong;
        self.true_changes += o.true_changes;
        self.found_changes += o.found_changes;
        self.label_changes += o.label_changes;
        self.unsupported_changes += o.unsupported_changes;
        self.flips += o.flips;
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
            "words_with_truth": self.scored,
            "wrong": self.wrong,
            "wder_percent": round3(100.0 * self.wder()),
            "true_changes": self.true_changes,
            "change_recall_percent": round3(100.0 * self.recall()),
            "label_changes": self.label_changes,
            "unsupported_changes": self.unsupported_changes,
            "flips_of_at_most_3_words": self.flips,
            "words_in_true_runs_of_at_most_3": self.short_true_run_words,
            "of_those_right": self.short_true_run_words_right,
        })
    }
}

fn round3(x: f64) -> f64 {
    (x * 1000.0).round() / 1000.0
}

fn score(item: &Item, labels: &[usize], map: &[Option<usize>]) -> Score {
    let mut s = Score::default();
    let right = |i: usize| item.truth[i].is_some() && map[labels[i]] == item.truth[i];
    for i in 0..labels.len() {
        if item.truth[i].is_some() {
            s.scored += 1;
            s.wrong += u64::from(!right(i));
        }
    }
    let n = labels.len();
    for i in 1..n {
        if let (Some(a), Some(b)) = (item.truth[i - 1], item.truth[i])
            && a != b
        {
            s.true_changes += 1;
            let lo = i.saturating_sub(1).max(1);
            let hi = (i + 1).min(n - 1);
            s.found_changes += u64::from((lo..=hi).any(|j| labels[j] != labels[j - 1]));
        }
        if labels[i] != labels[i - 1] {
            s.label_changes += 1;
            s.unsupported_changes += u64::from(!supported(&item.words, i));
        }
    }
    let runs = runs_of(labels);
    s.flips = runs
        .windows(3)
        .filter(|w| labels[w[0].0] == labels[w[2].0] && w[1].1 - w[1].0 <= FLIP_WORDS)
        .count() as u64;
    // True runs over the words with truth, in order.
    let known: Vec<usize> = (0..n).filter(|&i| item.truth[i].is_some()).collect();
    let mut at = 0;
    while at < known.len() {
        let mut end = at + 1;
        while end < known.len() && item.truth[known[end]] == item.truth[known[at]] {
            end += 1;
        }
        if end - at <= FLIP_WORDS {
            for &i in &known[at..end] {
                s.short_true_run_words += 1;
                s.short_true_run_words_right += u64::from(right(i));
            }
        }
        at = end;
    }
    s
}

/// Baseline and every variant, pooled over `items`.
fn pooled(items: &[&Item]) -> (Score, Vec<Score>) {
    let mut base = Score::default();
    let mut rows = vec![Score::default(); VARIANTS.len()];
    for item in items {
        let map = best_map(item);
        base.add(&score(item, &item.baseline, &map));
        for (row, v) in rows.iter_mut().zip(VARIANTS) {
            row.add(&score(item, &apply(&item.words, &item.baseline, v), &map));
        }
    }
    (base, rows)
}

fn table(name: &str, items: &[&Item], rows: &(Score, Vec<Score>)) -> Value {
    let (base, variants) = rows;
    json!({
        "set": name,
        "items": items.len(),
        "words_left_out": items.iter().map(|i| i.left_out).sum::<usize>(),
        "found_speakers": items.iter().map(|i| i.k).collect::<Vec<_>>(),
        "true_speakers": items.iter().map(|i| i.true_k).collect::<Vec<_>>(),
        "baseline": base.json(),
        "variants": VARIANTS.iter().zip(variants).map(|(v, s)| {
            json!({"n": v.n, "s": v.s, "score": s.json()})
        }).collect::<Vec<_>>(),
    })
}

/// The success criterion of the report, on the variant split A picks.
fn verdict(
    a: &(Score, Vec<Score>),
    b: &(Score, Vec<Score>),
    ami: Option<&(Score, Vec<Score>)>,
) -> Value {
    let pick = pick_on_a(a);
    let v = VARIANTS[pick];
    let (ab, av) = (&a.0, &a.1[pick]);
    let (bb, bv) = (&b.0, &b.1[pick]);
    let c1 = bv.wder() <= bb.wder() * 0.95 && bb.wrong >= bv.wrong + 5;
    let falls = |base: &Score, s: &Score| {
        s.unsupported_changes < base.unsupported_changes && s.flips < base.flips
    };
    let keeps = |base: &Score, s: &Score| base.recall() - s.recall() <= 0.01;
    let c5 = av.wder() < ab.wder();
    let mut c3 = falls(bb, bv);
    let mut c4 = keeps(bb, bv);
    let c2 = match ami {
        Some((base, rows)) => {
            let s = &rows[pick];
            c3 = c3 && falls(base, s);
            c4 = c4 && keeps(base, s);
            s.wder() <= base.wder()
        }
        None => false,
    };
    json!({
        "picked_on_split_a": {"n": v.n, "s": v.s},
        "1_split_b_wder_5_percent_lower_and_5_words": c1,
        "2_ami_wder_not_higher": c2,
        "3_unsupported_changes_and_flips_fall": c3,
        "4_change_recall_within_1_point": c4,
        "5_split_a_wder_lower": c5,
        "all_hold": c1 && c2 && c3 && c4 && c5,
    })
}

/// Lowest WDER on split A; [`VARIANTS`] is in the tie-break order.
fn pick_on_a(a: &(Score, Vec<Score>)) -> usize {
    let mut best = 0;
    for (i, s) in a.1.iter().enumerate() {
        if s.wder() < a.1[best].wder() {
            best = i;
        }
    }
    best
}

// ---- scenes ------------------------------------------------------------------------------

fn sorted_entries(dir: &Path) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = std::fs::read_dir(dir)
        .unwrap_or_else(|e| fail(format!("{}: {e}", dir.display())))
        .map(|e| e.unwrap_or_else(|e| fail(e)).path())
        .collect();
    v.sort();
    v
}

fn read_flac(path: &Path) -> Vec<f32> {
    let mut r =
        claxon::FlacReader::open(path).unwrap_or_else(|e| fail(format!("{}: {e}", path.display())));
    let scale = 1.0 / f64::from(1u32 << (r.streaminfo().bits_per_sample - 1));
    r.samples()
        .map(|s| {
            (f64::from(s.unwrap_or_else(|e| fail(format!("{}: {e}", path.display())))) * scale)
                as f32
        })
        .collect()
}

/// A speaker's utterances in path order, up to about `want` samples.
fn utterances(dir: &Path, want: usize) -> Vec<Vec<f32>> {
    let mut out = Vec::new();
    let mut held = 0;
    for chapter in sorted_entries(dir).into_iter().filter(|p| p.is_dir()) {
        for flac in sorted_entries(&chapter)
            .into_iter()
            .filter(|p| p.extension().is_some_and(|e| e == "flac"))
        {
            let u = read_flac(&flac);
            held += u.len();
            out.push(u);
            if held >= want {
                return out;
            }
        }
    }
    out
}

/// A deterministic draw in `[0, 1)`, independent of the audio.
fn draw(state: &mut u64) -> f64 {
    *state = state
        .wrapping_mul(6_364_136_223_846_793_005)
        .wrapping_add(1_442_695_040_888_963_407);
    (*state >> 11) as f64 / (1u64 << 53) as f64
}

fn between(state: &mut u64, lo: f64, hi: f64) -> f64 {
    lo + (hi - lo) * draw(state)
}

/// One true turn, in samples.
#[derive(Debug, Clone, Copy, PartialEq)]
struct TrueTurn {
    speaker: usize,
    start: usize,
    end: usize,
}

/// The two-speaker scene of the report.
fn build_scene(pair: [&[Vec<f32>]; 2], seed: u64, want: usize) -> (Vec<f32>, Vec<TrueTurn>) {
    let hz = PIPELINE_RATE.hz() as f64;
    let inner_gap = PIPELINE_RATE.hz() as usize * UTTERANCE_GAP_MS / 1000;
    let mut state = seed;
    let mut next = [0usize, 0];
    let mut audio: Vec<f32> = Vec::with_capacity(want);
    let mut turns: Vec<TrueTurn> = Vec::new();
    let mut speaker = 0;
    while audio.len() < want {
        let target = (between(&mut state, 5.0, 30.0) * hz) as usize;
        let interrupted = draw(&mut state) < 0.2;
        let cut = between(&mut state, 0.3, 0.7);
        let overlaps = draw(&mut state) < 0.2;
        let overlap_s = between(&mut state, 0.2, 1.0);
        let silence_s = between(&mut state, 0.1, 0.8);
        let mut piece: Vec<f32> = Vec::new();
        while piece.len() < target && next[speaker] < pair[speaker].len() {
            if !piece.is_empty() {
                piece.resize(piece.len() + inner_gap, 0.0);
            }
            piece.extend_from_slice(&pair[speaker][next[speaker]]);
            next[speaker] += 1;
        }
        if piece.is_empty() {
            break;
        }
        if interrupted {
            let last = pair[speaker][next[speaker] - 1].len();
            let keep = ((last as f64) * cut) as usize;
            piece.truncate(piece.len() - last + keep);
        }
        let start = match turns.last() {
            None => 0,
            Some(prev) if overlaps => {
                let ov = ((overlap_s * hz) as usize).min((prev.end - prev.start) / 2);
                prev.end - ov
            }
            Some(prev) => prev.end + (silence_s * hz) as usize,
        };
        let end = start + piece.len();
        if audio.len() < end {
            audio.resize(end, 0.0);
        }
        for (a, p) in audio[start..end].iter_mut().zip(&piece) {
            *a += p;
        }
        turns.push(TrueTurn {
            speaker,
            start,
            end,
        });
        speaker = 1 - speaker;
    }
    audio.truncate(want);
    for t in &mut turns {
        t.end = t.end.min(want);
    }
    turns.retain(|t| t.start < t.end);
    let peak = audio.iter().fold(0.0f32, |m, x| m.max(x.abs()));
    if peak > 0.99 {
        let g = 0.99 / peak;
        audio.iter_mut().for_each(|x| *x *= g);
    }
    (audio, turns)
}

/// The true speaker of a word: the one whose turn holds its midpoint; `None` in an overlap or
/// in no turn.
fn scene_truth(word: &Word, turns: &[TrueTurn]) -> Option<usize> {
    let mid_ms = (word.start.as_millis() + word.end.as_millis()) as f64 / 2.0;
    let mid = (mid_ms * f64::from(PIPELINE_RATE.hz()) / 1000.0) as usize;
    let mut on = turns
        .iter()
        .filter(|t| t.start <= mid && mid < t.end)
        .map(|t| t.speaker);
    let first = on.next()?;
    if on.any(|s| s != first) {
        None
    } else {
        Some(first)
    }
}

// ---- AMI ---------------------------------------------------------------------------------

fn attr<'a>(tag: &'a str, name: &str) -> Option<&'a str> {
    let key = format!(" {name}=\"");
    let at = tag.find(&key)? + key.len();
    let rest = &tag[at..];
    Some(&rest[..rest.find('"')?])
}

/// One speaker's words file under the report's rules; times in ms.
fn ami_words(xml: &str) -> Vec<Word> {
    let mut words: Vec<Word> = Vec::new();
    let mut rest = xml;
    while let Some(at) = rest.find("<w ") {
        rest = &rest[at..];
        let close = rest.find('>').unwrap_or_else(|| fail("unterminated <w>"));
        let tag = &rest[..close];
        if tag.ends_with('/') {
            rest = &rest[close + 1..];
            continue;
        }
        let body_end = rest
            .find("</w>")
            .unwrap_or_else(|| fail("<w> without </w>"));
        let body = rest[close + 1..body_end].trim().to_owned();
        let tag = tag.to_owned();
        rest = &rest[body_end + 4..];
        if attr(&tag, "punc") == Some("true") {
            if ends_sentence(&body)
                && let Some(last) = words.last_mut()
                && !ends_sentence(&last.text)
            {
                last.text.push('.');
            }
            continue;
        }
        if attr(&tag, "trunc") == Some("true") || body.is_empty() {
            continue;
        }
        let time = |name: &str| -> Option<u64> {
            attr(&tag, name)?
                .parse::<f64>()
                .ok()
                .map(|s| (s * 1000.0).round() as u64)
        };
        let (Some(start), Some(end)) = (time("starttime"), time("endtime")) else {
            continue;
        };
        words.push(Word {
            text: body,
            start: MeetingTime::from_millis(start),
            end: MeetingTime::from_millis(end.max(start)),
            probability: 1.0,
        });
    }
    words
}

/// A meeting's words across speakers in start-time order, with the speaker's position as truth.
fn ami_meeting_words(annotations: &Path, meeting: &str) -> (Vec<Word>, Vec<Option<usize>>, usize) {
    let mut all: Vec<(Word, usize)> = Vec::new();
    let mut speakers = 0;
    for letter in AMI_LETTERS {
        let p = annotations
            .join("words")
            .join(format!("{meeting}.{letter}.words.xml"));
        let Ok(bytes) = std::fs::read(&p) else {
            continue;
        };
        let xml = refs::latin1(&bytes);
        all.extend(ami_words(&xml).into_iter().map(|w| (w, speakers)));
        speakers += 1;
    }
    if speakers == 0 {
        fail(format!(
            "{meeting}: no words files; run `cargo xtask fetch-fixtures --set ami-sum`"
        ));
    }
    all.sort_by_key(|(w, s)| (w.start.as_millis(), *s));
    let (words, truth): (Vec<Word>, Vec<usize>) = all.into_iter().unzip();
    (words, truth.into_iter().map(Some).collect(), speakers)
}

fn read_mono_wav(path: &Path) -> Vec<f32> {
    let file = File::open(path).unwrap_or_else(|e| {
        fail(format!(
            "{}: {e}; run `cargo xtask fetch-fixtures --set ami-dev`",
            path.display()
        ))
    });
    let wav =
        read_wav(BufReader::new(file)).unwrap_or_else(|e| fail(format!("{}: {e}", path.display())));
    if wav.rate() != PIPELINE_RATE || wav.channels() != 1 {
        fail(format!("{}: expected 16 kHz mono", path.display()));
    }
    wav.into_samples()
}

fn fixtures_dir() -> PathBuf {
    if let Some(d) = std::env::var_os("EARSHOT_FIXTURES").filter(|v| !v.is_empty()) {
        return PathBuf::from(d);
    }
    PathBuf::from(std::env::var_os("LOCALAPPDATA").unwrap_or_else(|| fail("LOCALAPPDATA")))
        .join("earshot-dev")
        .join("fixtures")
}

fn labels_of(words: &[Word], turns: &[SpeakerTurn]) -> Vec<usize> {
    assign_words(words, turns)
        .into_iter()
        .map(|l| l.unwrap_or_else(|| fail("a diarisation with no turns")))
        .collect()
}

fn main() {
    let mut it = std::env::args().skip(1);
    let _root = it
        .next()
        .unwrap_or_else(|| fail("usage: word_changes <repo-root> --models <dir>"));
    let (mut models, mut out) = (None, None);
    let mut pairs = 20usize;
    let mut meetings: Vec<String> = AMI_DEV.iter().map(|m| (*m).to_owned()).collect();
    while let Some(flag) = it.next() {
        let value = it
            .next()
            .unwrap_or_else(|| fail(format!("{flag} needs a value")));
        match flag.as_str() {
            "--models" => models = Some(ModelDir::new(value)),
            "--out" => out = Some(PathBuf::from(value)),
            "--pairs" => {
                pairs = value
                    .parse()
                    .unwrap_or_else(|e| fail(format!("--pairs: {e}")))
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
    let threshold = config.clustering.threshold;
    let mut diarizer = Diarizer::new(segmenter, embedder, config).unwrap_or_else(|e| fail(e));
    let cancel = CancelToken::new();

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
    let mut transcriber = choice.transcriber;
    let mut vad =
        load_silero(&models, Accelerator::Cpu).unwrap_or_else(|e| fail(format!("Silero: {e}")));

    let fixtures = fixtures_dir();
    let root = fixtures.join("librispeech/dev-clean/LibriSpeech/dev-clean");
    let speakers: Vec<PathBuf> = sorted_entries(&root)
        .into_iter()
        .filter(|p| p.is_dir())
        .collect();
    if speakers.len() != 40 {
        fail(format!(
            "{}: expected 40 dev-clean speakers",
            root.display()
        ));
    }
    let want = SCENE_S * PIPELINE_RATE.hz() as usize;
    let mut items = Vec::new();
    for p in 0..pairs.min(20) {
        eprintln!("word_changes: scene {p}");
        let a = utterances(&speakers[2 * p], want);
        let b = utterances(&speakers[2 * p + 1], want);
        let (audio, turns) = build_scene([&a, &b], SCENE_SEED + p as u64, want);
        let audio = quantise_pcm16(&audio);
        let buffer = MonoBuffer::new(PIPELINE_RATE, audio);
        let diar = diarizer
            .diarize(&buffer, MeetingTime::ZERO, &cancel)
            .unwrap_or_else(|e| fail(format!("diarize: {e}")));
        let (t, _) = transcribe_in_pieces(
            &mut transcriber,
            &mut vad,
            &buffer,
            &TranscribeOptions::live(),
            &PieceConfig::default(),
            &cancel,
        )
        .unwrap_or_else(|e| fail(format!("pieces: {e}")));
        let words = refs::kept_words(&t);
        let truth: Vec<Option<usize>> = words.iter().map(|w| scene_truth(w, &turns)).collect();
        let left_out = truth.iter().filter(|t| t.is_none()).count();
        let baseline = labels_of(&words, &diar.turns);
        items.push(Item {
            words,
            baseline,
            truth,
            k: diar.speakers.len(),
            true_k: 2,
            left_out,
        });
    }
    let split_a: Vec<&Item> = items.iter().take(10).collect();
    let split_b: Vec<&Item> = items.iter().skip(10).collect();
    let (a, b) = (pooled(&split_a), pooled(&split_b));

    let annotations = fixtures.join("ami/annotations");
    let mut ami_items = Vec::new();
    for m in &meetings {
        eprintln!("word_changes: {m}");
        let audio = read_mono_wav(&fixtures.join(format!("ami/audio/{m}.Mix-Headset.wav")));
        let buffer = MonoBuffer::new(PIPELINE_RATE, quantise_pcm16(&audio));
        let diar = diarizer
            .diarize(&buffer, MeetingTime::ZERO, &cancel)
            .unwrap_or_else(|e| fail(format!("diarize {m}: {e}")));
        let (words, truth, true_k) = ami_meeting_words(&annotations, m);
        let baseline = labels_of(&words, &diar.turns);
        ami_items.push(Item {
            words,
            baseline,
            truth,
            k: diar.speakers.len(),
            true_k,
            left_out: 0,
        });
    }
    let ami_refs: Vec<&Item> = ami_items.iter().collect();
    let ami = (!ami_refs.is_empty()).then(|| pooled(&ami_refs));

    let report = json!({
        "model": model.to_string(),
        "cluster_threshold": threshold,
        "transcription_model": DEFAULT_MODEL,
        "split_a": table("scenes split A", &split_a, &a),
        "split_b": table("scenes split B", &split_b, &b),
        "ami_dev": ami.as_ref().map(|r| table("AMI dev", &ami_refs, r)),
        "ami_meetings": meetings,
        "verdict": verdict(&a, &b, ami.as_ref()),
    });
    let text = serde_json::to_string_pretty(&report).unwrap_or_else(|e| fail(e));
    if let Some(out) = &out {
        std::fs::write(out, &text).unwrap_or_else(|e| fail(format!("{}: {e}", out.display())));
    }
    println!("{text}");
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

    /// `(text, gap before in ms)`, 100 ms long each.
    fn line(spec: &[(&str, u64)]) -> Vec<Word> {
        let mut t = 0;
        spec.iter()
            .map(|(text, gap)| {
                t += gap;
                let word = w(text, t, t + 100);
                t += 100;
                word
            })
            .collect()
    }

    fn v(n: usize, s: usize) -> Variant {
        Variant { n, s }
    }

    #[test]
    fn a_short_run_inside_a_sentence_takes_the_previous_speaker() {
        let words = line(&[("a", 0), ("b", 0), ("c", 0), ("d", 0), ("e", 0)]);
        assert_eq!(apply(&words, &[0, 0, 1, 0, 0], v(2, 0)), vec![0; 5]);
        // A run at the limit stays.
        assert_eq!(
            apply(&words, &[0, 0, 1, 1, 1], v(3, 0)),
            vec![0, 0, 1, 1, 1]
        );
        assert_eq!(apply(&words, &[0, 0, 0, 1, 1], v(3, 0)), vec![0; 5]);
    }

    #[test]
    fn a_change_at_a_sentence_end_or_a_pause_stays() {
        let words = line(&[("a", 0), ("b.", 0), ("c", 0), ("d", 400), ("e", 0)]);
        // A one-word turn after a sentence end, with its own end supported by the pause.
        assert_eq!(
            apply(&words, &[0, 0, 1, 0, 0], v(5, 0)),
            vec![0, 0, 1, 0, 0]
        );
        // A short run after a pause with a different speaker next stays.
        assert_eq!(apply(&words, &[0, 0, 0, 1, 2], v(2, 0))[3], 1);
    }

    #[test]
    fn a_flip_with_one_unsupported_side_goes_even_after_a_sentence_end() {
        let words = line(&[("a.", 0), ("b", 0), ("c", 0), ("d", 0)]);
        assert_eq!(apply(&words, &[0, 1, 0, 0], v(3, 0)), vec![0; 4]);
        // Not a flip: the speaker after it is someone else.
        assert_eq!(apply(&words, &[0, 1, 2, 2], v(2, 0)), vec![0, 1, 2, 2]);
    }

    #[test]
    fn a_change_snaps_to_the_nearest_supported_boundary_and_never_empties_a_run() {
        // Sentence end after word 2; the labels change one word late.
        let words = line(&[("a", 0), ("b.", 0), ("c", 0), ("d", 0), ("e", 0), ("f", 0)]);
        assert_eq!(
            apply(&words, &[0, 0, 0, 1, 1, 1], v(2, 2)),
            vec![0, 0, 1, 1, 1, 1]
        );
        // One word early.
        assert_eq!(
            apply(&words, &[0, 1, 1, 1, 1, 1], v(1, 2)),
            vec![0, 0, 1, 1, 1, 1]
        );
        // Out of reach: stays.
        assert_eq!(
            apply(&words, &[0, 0, 0, 0, 0, 1], v(1, 2)),
            vec![0, 0, 0, 0, 0, 1]
        );
        // A supported boundary at the run's own start would empty it.
        let words = line(&[("a.", 0), ("b", 0), ("c", 0), ("d", 0)]);
        assert_eq!(apply(&words, &[0, 1, 1, 2], v(1, 2)), vec![0, 1, 1, 2]);
    }

    #[test]
    fn labels_without_a_change_and_empty_input_pass_through() {
        assert!(apply(&[], &[], v(3, 2)).is_empty());
        let words = line(&[("a", 0), ("b", 0)]);
        assert_eq!(apply(&words, &[1, 1], v(3, 2)), vec![1, 1]);
    }

    #[test]
    fn scene_turns_hold_their_speakers_audio_and_never_repeat_it() {
        let hz = PIPELINE_RATE.hz() as usize;
        let a: Vec<Vec<f32>> = (0..40).map(|_| vec![0.25; 6 * hz]).collect();
        let b: Vec<Vec<f32>> = (0..40).map(|_| vec![0.125; 6 * hz]).collect();
        let want = 120 * hz;
        let (audio, turns) = build_scene([&a, &b], 3, want);
        assert_eq!(audio.len(), want);
        assert!(turns.len() > 3);
        for t in turns.windows(2) {
            assert_ne!(t[0].speaker, t[1].speaker);
        }
        // Outside any overlap, a turn holds its speaker's level or the pauses between its
        // utterances, and mostly the level.
        for (i, t) in turns.iter().enumerate() {
            let level = if t.speaker == 0 { 0.25 } else { 0.125 };
            let (mut own, mut alone) = (0usize, 0usize);
            for (at, &sample) in audio.iter().enumerate().take(t.end).skip(t.start) {
                let overlapped = turns
                    .iter()
                    .enumerate()
                    .any(|(j, o)| j != i && o.start <= at && at < o.end);
                if !overlapped {
                    alone += 1;
                    own += usize::from(sample == level);
                    assert!(sample == level || sample == 0.0, "{t:?} at {at}");
                }
            }
            assert!(own * 10 >= alone * 9, "{t:?}: {own} of {alone}");
        }
        // Silence between turns has no truth, an overlap has none either.
        let ms = |s: usize| (s * 1000 / hz) as u64;
        let word = |s: usize| w("x", ms(s), ms(s));
        assert_eq!(scene_truth(&word(1), &turns), Some(turns[0].speaker));
        for pair in turns.windows(2) {
            if pair[1].start < pair[0].end {
                let mid = (pair[1].start + pair[0].end) / 2;
                assert_eq!(scene_truth(&word(mid), &turns), None);
            }
        }
    }

    #[test]
    fn ami_words_follow_the_reference_rules_and_mark_sentence_ends() {
        let xml = r#"
          <w nite:id="x0" starttime="1.0" endtime="1.5">So</w>
          <w nite:id="x1" starttime="1.5" endtime="1.5" punc="true">.</w>
          <w nite:id="x2" starttime="1.6" endtime="1.7" trunc="true">wh</w>
          <w nite:id="x3" starttime="1.8" endtime="2.0">well</w>
          <w nite:id="x4" starttime="2.0" endtime="2.0" punc="true">,</w>
          <vocalsound nite:id="x5" starttime="2.0" endtime="2.2" type="laugh"/>
          <w nite:id="x6" starttime="2.2" endtime="2.4">yes</w>
          <w nite:id="x7" starttime="2.4" endtime="2.4" punc="true">?</w>
        "#;
        let words = ami_words(xml);
        let texts: Vec<&str> = words.iter().map(|w| w.text.as_str()).collect();
        assert_eq!(texts, vec!["So.", "well", "yes."]);
        assert_eq!(words[1].start.as_millis(), 1800);
        assert_eq!(words[1].end.as_millis(), 2000);
    }

    #[test]
    fn scoring_counts_wrong_words_changes_and_flips_under_the_baseline_map() {
        let words = line(&[("a", 0), ("b", 0), ("c", 0), ("d", 0), ("e", 0), ("f", 0)]);
        let item = Item {
            baseline: vec![1, 1, 0, 1, 0, 0],
            truth: vec![Some(0), Some(0), Some(0), Some(0), Some(1), None],
            words,
            k: 2,
            true_k: 2,
            left_out: 1,
        };
        let map = best_map(&item);
        assert_eq!(map, vec![Some(1), Some(0)]);
        let s = score(&item, &item.baseline, &map);
        assert_eq!(s.scored, 5);
        assert_eq!(s.wrong, 1);
        assert_eq!(s.true_changes, 1);
        assert_eq!(s.found_changes, 1);
        assert_eq!(s.label_changes, 3);
        assert_eq!(s.unsupported_changes, 3);
        assert_eq!(s.flips, 2);
        assert_eq!(
            (s.short_true_run_words, s.short_true_run_words_right),
            (1, 1)
        );
    }

    #[test]
    fn the_verdict_needs_every_condition() {
        let good = Score {
            scored: 1000,
            wrong: 50,
            true_changes: 100,
            found_changes: 90,
            label_changes: 150,
            unsupported_changes: 60,
            flips: 20,
            ..Score::default()
        };
        let better = Score {
            wrong: 40,
            unsupported_changes: 30,
            flips: 5,
            ..good
        };
        let rows = |s: Score| (good, vec![s; 6]);
        let v = verdict(&rows(better), &rows(better), Some(&rows(better)));
        assert_eq!(v["all_hold"], json!(true));
        // No AMI run proves nothing.
        assert_eq!(
            verdict(&rows(better), &rows(better), None)["all_hold"],
            json!(false)
        );
        // AMI gets worse.
        let worse = Score {
            wrong: 51,
            ..better
        };
        assert_eq!(
            verdict(&rows(better), &rows(better), Some(&rows(worse)))["2_ami_wder_not_higher"],
            json!(false)
        );
        // Recall lost.
        let lost = Score {
            found_changes: 88,
            ..better
        };
        assert_eq!(
            verdict(&rows(better), &rows(lost), Some(&rows(better)))["4_change_recall_within_1_point"],
            json!(false)
        );
    }
}
