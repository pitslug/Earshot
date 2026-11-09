//! The clustering threshold with AMI dev meetings in the data (T12.9). Counts and times only.
//!
//! ```text
//! cargo run --release -p earshot-runtime --example cluster_sweep -- <repo-root> \
//!     --models <dir> [--out <file.json>]
//! ```
//!
//! The data, measures and selection rule are `docs/far-speakers/ami-dev-calibration.md`,
//! committed before this tool was written and before any run. Dev data only: the AMI dev mixes
//! of fixture set `ami-dev` and LibriSpeech dev-clean scenes in which every speaker plays once.
//! Measurement only: nothing here changes the committed threshold. No word text is printed or
//! written; the annotation words are read only for their times.

use std::collections::HashMap;
use std::fs::File;
use std::io::BufReader;
use std::path::{Path, PathBuf};

use earshot_core::asr::{CancelToken, MeetingTime};
use earshot_core::audio::{MonoBuffer, PIPELINE_RATE, Sample, quantise_pcm16, read_wav};
use earshot_core::diar::{
    DiarError, Diarization, DiarizationConfig, Diarizer, EmbedderInfo, Embedding, LocalActivity,
    SegmenterInfo, SpeakerEmbedder, SpeakerSegmenter,
};
use earshot_core::metrics::der::{AMI_COLLAR_PER_SIDE_S, DerConfig, DerResult, Turn, der};
use earshot_runtime::backends::{Accelerator, ModelDir, init_onnx, load_diarisation};
use earshot_runtime::post::PostConfig;
use serde_json::{Value, json};

/// The held constant.
const HELD: u32 = 68;
/// Thresholds tried, in hundredths.
const GRID: std::ops::RangeInclusive<u32> = 55..=100;
/// The first meeting of each AMI dev series.
const AMI_DEV: [&str; 5] = ["ES2003a", "ES2011a", "IS1008a", "TS3004a", "TS3006a"];
/// Letters a speaker's words file can have.
const AMI_LETTERS: [&str; 5] = ["A", "B", "C", "D", "E"];
/// One speaker's reference intervals closer than this are joined (s).
const JOIN_GAP_S: f64 = 0.5;
/// Scene length (s).
const SCENE_S: usize = 360;
/// Silence between utterances of a stream and at a change in `even` and `dominant` (ms).
const GAP_MS: usize = 200;
/// Scene draws are seeded with this plus the scene index.
const SCENE_SEED: u64 = 12_900;
/// Rule step 1: the T3.2b valley height.
const VALLEY_HEIGHT: f64 = 0.011;
/// Rule step 3: the AMI dev DER a move must win by.
const AMI_DER_MARGIN: f64 = 0.005;
/// Rule step 3: the summed count error a move with no AMI DER loss must win by.
const COUNT_MARGIN: usize = 2;
/// Rule step 3: how much worse the long scenes may get.
const SCENE_DER_SLACK: f64 = 0.002;
/// Rule step 3: how much worse one AMI dev meeting may get.
const MEETING_DER_SLACK: f64 = 0.02;
const HZ: usize = 16_000;

fn fail(msg: impl std::fmt::Display) -> ! {
    eprintln!("cluster_sweep: {msg}");
    std::process::exit(1);
}

fn threshold(t: u32) -> f32 {
    t as f32 / 100.0
}

// ---- record and replay -------------------------------------------------------------------

/// FNV-1a over a batch: each input's length and its samples' bit patterns, in order.
fn input_hash(inputs: &[&[Sample]]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    let mut eat = |v: u64| {
        for b in v.to_le_bytes() {
            h ^= u64::from(b);
            h = h.wrapping_mul(0x0000_0100_0000_01b3);
        }
    };
    for x in inputs {
        eat(x.len() as u64);
        for s in *x {
            eat(u64::from(s.to_bits()));
        }
    }
    h
}

/// Keeps the wrapped model's outputs by input hash while recording and answers from them
/// while replaying; a replayed call with inputs never recorded fails.
struct Cached<T, O> {
    inner: T,
    log: HashMap<u64, Vec<O>>,
    replay: bool,
}

impl<T, O: Clone> Cached<T, O> {
    fn new(inner: T) -> Self {
        Cached {
            inner,
            log: HashMap::new(),
            replay: false,
        }
    }

    fn call(
        &mut self,
        inputs: &[&[Sample]],
        run: impl FnOnce(&mut T) -> Result<Vec<O>, DiarError>,
    ) -> Result<Vec<O>, DiarError> {
        let key = input_hash(inputs);
        if self.replay {
            let out = self
                .log
                .get(&key)
                .unwrap_or_else(|| fail("a replayed call has inputs that were never recorded"));
            if out.len() != inputs.len() {
                fail("a replayed call has another batch size");
            }
            return Ok(out.clone());
        }
        let out = run(&mut self.inner)?;
        self.log.entry(key).or_insert_with(|| out.clone());
        Ok(out)
    }
}

impl<T: SpeakerSegmenter> SpeakerSegmenter for Cached<T, LocalActivity> {
    fn info(&self) -> &SegmenterInfo {
        self.inner.info()
    }
    fn segment(
        &mut self,
        windows: &[&[Sample]],
        cancel: &CancelToken,
    ) -> Result<Vec<LocalActivity>, DiarError> {
        self.call(windows, |m| m.segment(windows, cancel))
    }
}

impl<T: SpeakerEmbedder> SpeakerEmbedder for Cached<T, Embedding> {
    fn info(&self) -> &EmbedderInfo {
        self.inner.info()
    }
    fn embed(
        &mut self,
        speech: &[&[Sample]],
        cancel: &CancelToken,
    ) -> Result<Vec<Embedding>, DiarError> {
        self.call(speech, |m| m.embed(speech, cancel))
    }
}

// ---- measures ----------------------------------------------------------------------------

/// One meeting or scene with its truth.
struct Item {
    name: String,
    kind: &'static str,
    audio: Vec<f32>,
    /// Reference speech (seconds), labelled by true speaker.
    reference: Vec<Turn>,
    true_k: usize,
    /// Scored region (seconds).
    uem: (f64, f64),
}

/// What one item scores at one threshold.
#[derive(Debug, Clone)]
struct Score {
    der: DerResult,
    found: usize,
    merges: usize,
    extras: usize,
    extra_speech_s: f64,
    lost: usize,
}

impl Score {
    fn count_error(&self, true_k: usize) -> usize {
        self.found.abs_diff(true_k)
    }
}

fn clip(t: &Turn, uem: (f64, f64)) -> Option<(f64, f64)> {
    let (s, e) = (t.start.max(uem.0), t.end.min(uem.1));
    (e > s).then_some((s, e))
}

/// Found speakers, merges, extras and lost speakers of `hyp` against `reference` inside `uem`
/// (the definitions of the rule doc).
fn speakers_of(
    reference: &[Turn],
    hyp: &[Turn],
    uem: (f64, f64),
) -> (usize, usize, usize, f64, usize) {
    let mut found: Vec<&str> = hyp
        .iter()
        .filter(|t| clip(t, uem).is_some())
        .map(|t| t.speaker.as_str())
        .collect();
    found.sort_unstable();
    found.dedup();
    let mut truth: Vec<&str> = reference.iter().map(|t| t.speaker.as_str()).collect();
    truth.sort_unstable();
    truth.dedup();
    let mut overlap = vec![vec![0.0f64; found.len()]; truth.len()];
    for r in reference {
        let Some((rs, re)) = clip(r, uem) else {
            continue;
        };
        let ri = truth
            .binary_search(&r.speaker.as_str())
            .unwrap_or_else(|_| unreachable!());
        for h in hyp {
            let Some((hs, he)) = clip(h, uem) else {
                continue;
            };
            let o = re.min(he) - rs.max(hs);
            if o > 0.0 {
                let hi = found
                    .binary_search(&h.speaker.as_str())
                    .unwrap_or_else(|_| unreachable!());
                overlap[ri][hi] += o;
            }
        }
    }
    let mut homes: Vec<usize> = Vec::new();
    let mut lost = 0;
    for row in &overlap {
        // Longest overlap; ties to the lower index.
        let best = row.iter().enumerate().filter(|(_, o)| **o > 0.0).fold(
            None::<(usize, f64)>,
            |b, (i, o)| match b {
                Some((_, bo)) if bo >= *o => b,
                _ => Some((i, *o)),
            },
        );
        match best {
            Some((i, _)) => homes.push(i),
            None => lost += 1,
        }
    }
    let homed = homes.len();
    homes.sort_unstable();
    homes.dedup();
    let merges = homed - homes.len();
    let mut extras = 0;
    let mut extra_speech = 0.0;
    for (i, name) in found.iter().enumerate() {
        if !homes.contains(&i) {
            extras += 1;
            extra_speech += hyp
                .iter()
                .filter(|t| t.speaker == *name)
                .filter_map(|t| clip(t, uem))
                .map(|(s, e)| e - s)
                .sum::<f64>();
        }
    }
    (found.len(), merges, extras, extra_speech, lost)
}

fn score(item: &Item, d: &Diarization) -> Score {
    let hyp = d.der_turns();
    let cfg = DerConfig::nist(AMI_COLLAR_PER_SIDE_S).with_uem(vec![item.uem]);
    let der = der(&item.reference, &hyp, &cfg);
    let (found, merges, extras, extra_speech_s, lost) =
        speakers_of(&item.reference, &hyp, item.uem);
    Score {
        der,
        found,
        merges,
        extras,
        extra_speech_s,
        lost,
    }
}

// ---- the rule ----------------------------------------------------------------------------

/// One threshold over every item (AMI meetings first, then scenes).
#[derive(Debug, Clone)]
struct Point {
    t: u32,
    /// Pooled AMI dev DER.
    a: f64,
    /// Pooled scene DER.
    l: f64,
    /// Summed count error over every item.
    c: usize,
    /// Merges per item.
    merges: Vec<usize>,
    /// Each AMI dev meeting's DER.
    meeting_der: Vec<f64>,
}

#[derive(Debug, Clone, PartialEq)]
struct Verdict {
    admissible: Vec<u32>,
    winner: u32,
    pick: u32,
    /// Step 3's conditions at the winner: AMI win (DER or count), scenes, meetings.
    move_conditions: (bool, bool, bool),
    unconstrained_ami_best: u32,
}

fn rounded(x: f64) -> f64 {
    (x * 1000.0).round()
}

fn select(points: &[Point], short: &HashMap<u32, f64>) -> Verdict {
    let at = |t: u32| {
        points
            .iter()
            .find(|p| p.t == t)
            .unwrap_or_else(|| fail(format!("{t} is not on the grid")))
    };
    let held = at(HELD);
    let min_s = points
        .iter()
        .map(|p| {
            short
                .get(&p.t)
                .copied()
                .unwrap_or_else(|| fail(format!("no S({})", p.t)))
        })
        .fold(f64::INFINITY, f64::min);
    let admissible: Vec<&Point> = points
        .iter()
        .filter(|p| short[&p.t] <= min_s + VALLEY_HEIGHT + 1e-12)
        .filter(|p| p.merges.iter().zip(&held.merges).all(|(m, h)| m <= h))
        .collect();
    if !admissible.iter().any(|p| p.t == HELD) {
        fail("the held constant is not admissible");
    }
    let winner = *admissible
        .iter()
        .min_by(|x, y| {
            rounded(x.a)
                .total_cmp(&rounded(y.a))
                .then(x.c.cmp(&y.c))
                .then(rounded(x.l).total_cmp(&rounded(y.l)))
                .then(x.t.abs_diff(HELD).cmp(&y.t.abs_diff(HELD)))
                .then(x.t.cmp(&y.t))
        })
        .unwrap_or_else(|| unreachable!());
    let ami_win = held.a - winner.a >= AMI_DER_MARGIN - 1e-12
        || (winner.a <= held.a && held.c >= winner.c + COUNT_MARGIN);
    let scenes = winner.l <= held.l + SCENE_DER_SLACK + 1e-12;
    let meetings = winner
        .meeting_der
        .iter()
        .zip(&held.meeting_der)
        .all(|(w, h)| *w <= h + MEETING_DER_SLACK + 1e-12);
    let unconstrained = points
        .iter()
        .min_by(|x, y| {
            rounded(x.a)
                .total_cmp(&rounded(y.a))
                .then(x.t.abs_diff(HELD).cmp(&y.t.abs_diff(HELD)))
                .then(x.t.cmp(&y.t))
        })
        .unwrap_or_else(|| unreachable!());
    Verdict {
        admissible: admissible.iter().map(|p| p.t).collect(),
        winner: winner.t,
        pick: if ami_win && scenes && meetings {
            winner.t
        } else {
            HELD
        },
        move_conditions: (ami_win, scenes, meetings),
        unconstrained_ami_best: unconstrained.t,
    }
}

/// S(t) from the committed T6.9b curve (`threshold S(t) ...` lines).
fn short_curve(text: &str) -> HashMap<u32, f64> {
    let mut out = HashMap::new();
    for line in text.lines() {
        let mut f = line.split_whitespace();
        let (Some(t), Some(s)) = (f.next(), f.next()) else {
            continue;
        };
        let (Ok(t), Ok(s)) = (t.parse::<f64>(), s.parse::<f64>()) else {
            continue;
        };
        out.insert((t * 100.0).round() as u32, s);
    }
    out
}

// ---- AMI ---------------------------------------------------------------------------------

fn attr<'a>(tag: &'a str, name: &str) -> Option<&'a str> {
    let key = format!(" {name}=\"");
    let at = tag.find(&key)? + key.len();
    let rest = &tag[at..];
    Some(&rest[..rest.find('"')?])
}

/// One speaker's word intervals (s): every `<w>` that is not punctuation and has times with
/// end > start, in file order.
fn word_intervals(xml: &str) -> Vec<(f64, f64)> {
    let mut out = Vec::new();
    let mut rest = xml;
    while let Some(at) = rest.find("<w ") {
        rest = &rest[at..];
        let close = rest.find('>').unwrap_or_else(|| fail("unterminated <w>"));
        let tag = &rest[..close];
        rest = &rest[close + 1..];
        if attr(tag, "punc") == Some("true") {
            continue;
        }
        let time = |n: &str| attr(tag, n).and_then(|v| v.parse::<f64>().ok());
        if let (Some(s), Some(e)) = (time("starttime"), time("endtime"))
            && e > s
        {
            out.push((s, e));
        }
    }
    out
}

/// Joins intervals closer than [`JOIN_GAP_S`] (sorted first).
fn join(mut v: Vec<(f64, f64)>) -> Vec<(f64, f64)> {
    v.sort_by(|a, b| a.0.total_cmp(&b.0));
    let mut out: Vec<(f64, f64)> = Vec::new();
    for (s, e) in v {
        match out.last_mut() {
            Some(last) if s - last.1 < JOIN_GAP_S => last.1 = last.1.max(e),
            _ => out.push((s, e)),
        }
    }
    out
}

fn ami_item(fixtures: &Path, meeting: &str) -> Item {
    let mut reference = Vec::new();
    let mut true_k = 0;
    for letter in AMI_LETTERS {
        let p = fixtures
            .join("ami/annotations/words")
            .join(format!("{meeting}.{letter}.words.xml"));
        let Ok(bytes) = std::fs::read(&p) else {
            continue;
        };
        let xml: String = bytes.iter().map(|&b| char::from(b)).collect();
        let speech = join(word_intervals(&xml));
        if speech.is_empty() {
            continue;
        }
        true_k += 1;
        reference.extend(speech.into_iter().map(|(s, e)| Turn::new(s, e, letter)));
    }
    if true_k == 0 {
        fail(format!(
            "{meeting}: no words files; run `cargo xtask fetch-fixtures --set ami-sum`"
        ));
    }
    let uem = (
        reference
            .iter()
            .map(|t| t.start)
            .fold(f64::INFINITY, f64::min),
        reference.iter().map(|t| t.end).fold(0.0, f64::max),
    );
    let path = fixtures.join(format!("ami/audio/{meeting}.Mix-Headset.wav"));
    let file = File::open(&path).unwrap_or_else(|e| {
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
    Item {
        name: meeting.to_owned(),
        kind: "ami",
        audio: quantise_pcm16(&wav.into_samples()),
        reference,
        true_k,
        uem,
    }
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

/// A speaker's utterances in path order joined with [`GAP_MS`] of silence, up to `want`.
fn stream(dir: &Path, want: usize) -> Vec<f32> {
    let gap = vec![0f32; HZ * GAP_MS / 1000];
    let mut out = Vec::new();
    for chapter in sorted_entries(dir).into_iter().filter(|p| p.is_dir()) {
        for flac in sorted_entries(&chapter)
            .into_iter()
            .filter(|p| p.extension().is_some_and(|e| e == "flac"))
        {
            out.extend(read_flac(&flac));
            out.extend_from_slice(&gap);
            if out.len() >= want {
                out.truncate(want);
                return out;
            }
        }
    }
    out
}

/// The next value of the scene LCG (`far_speakers`'s constants).
fn next(state: &mut u64) -> u64 {
    *state = state
        .wrapping_mul(6_364_136_223_846_793_005)
        .wrapping_add(1_442_695_040_888_963_407);
    *state >> 33
}

/// A draw in `[lo, hi)`.
fn draw(state: &mut u64, lo: usize, hi: usize) -> usize {
    lo + (next(state) as usize) % (hi - lo)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    One,
    Even,
    Dominant,
    Four,
}

impl Kind {
    fn name(self) -> &'static str {
        match self {
            Kind::One => "one",
            Kind::Even => "even",
            Kind::Dominant => "dominant",
            Kind::Four => "four",
        }
    }
}

/// The 16 scenes: kind and the speaker positions each uses.
fn scene_plan() -> Vec<(Kind, Vec<usize>)> {
    let mut plan: Vec<(Kind, Vec<usize>)> = (0..4).map(|i| (Kind::One, vec![i])).collect();
    plan.extend((0..3).map(|i| (Kind::Even, vec![4 + 2 * i, 5 + 2 * i])));
    plan.extend((0..3).map(|i| (Kind::Dominant, vec![10 + 2 * i, 11 + 2 * i])));
    plan.extend((0..6).map(|i| (Kind::Four, (16 + 4 * i..20 + 4 * i).collect())));
    plan
}

/// One turn: its slot in the scene, its span in the mix and in its speaker's stream (samples).
#[derive(Debug, Clone, PartialEq)]
struct SceneTurn {
    slot: usize,
    start: usize,
    source: std::ops::Range<usize>,
}

/// Builds a scene from its speakers' streams under the rule doc. Returns the mix and turns.
fn build_scene(kind: Kind, streams: &[&[f32]], seed: u64) -> (Vec<f32>, Vec<SceneTurn>) {
    let ms = |m: usize| m * HZ / 1000;
    let end = SCENE_S * HZ;
    let mut state = seed;
    let mut cursor = vec![0usize; streams.len()];
    let mut turns: Vec<SceneTurn> = Vec::new();
    let mut prev_end = 0usize;
    let mut index = 0usize;
    loop {
        let slot = match kind {
            Kind::One => 0,
            Kind::Even | Kind::Dominant => index % 2,
            Kind::Four if index < 4 => index,
            Kind::Four => {
                let previous = turns.last().map_or(0, |t| t.slot);
                let others: Vec<usize> = (0..4).filter(|&s| s != previous).collect();
                others[draw(&mut state, 0, 3)]
            }
        };
        let (lo, hi) = match (kind, slot) {
            (Kind::One, _) => (SCENE_S * 1000, SCENE_S * 1000 + 1),
            (Kind::Even, _) => (5_000, 30_000),
            (Kind::Dominant, 0) => (40_000, 90_000),
            (Kind::Dominant, _) => (4_000, 12_000),
            (Kind::Four, _) => (2_000, 20_000),
        };
        let want = ms(draw(&mut state, lo, hi));
        let start = if index == 0 {
            0
        } else {
            match kind {
                Kind::One => unreachable!(),
                Kind::Even | Kind::Dominant => prev_end + ms(GAP_MS),
                Kind::Four => {
                    if draw(&mut state, 0, 5) == 0 {
                        let previous = turns.last().map_or(0, |t| t.source.len());
                        prev_end - ms(draw(&mut state, 200, 1_000)).min(previous / 2)
                    } else {
                        prev_end + ms(draw(&mut state, 100, 800))
                    }
                }
            }
        };
        if start >= end {
            break;
        }
        let left = streams[slot].len() - cursor[slot];
        // `one` is its speaker's stream up to the scene length, however long that is.
        let min_len = if kind == Kind::One {
            1
        } else {
            ms(lo).min(end - start)
        };
        if left < min_len {
            break;
        }
        let len = want.min(left).min(end - start);
        let source = cursor[slot]..cursor[slot] + len;
        cursor[slot] += len;
        prev_end = start + len;
        turns.push(SceneTurn {
            slot,
            start,
            source,
        });
        index += 1;
        if kind == Kind::One {
            break;
        }
    }
    let total = turns
        .iter()
        .map(|t| t.start + t.source.len())
        .max()
        .unwrap_or(0);
    let mut audio = vec![0f32; total];
    for t in &turns {
        for (a, s) in audio[t.start..]
            .iter_mut()
            .zip(&streams[t.slot][t.source.clone()])
        {
            *a += s;
        }
    }
    let peak = audio.iter().fold(0.0f32, |m, x| m.max(x.abs()));
    if peak > 0.99 {
        let g = 0.99 / peak;
        audio.iter_mut().for_each(|x| *x *= g);
    }
    (audio, turns)
}

/// Two turns of one slot that play overlapping samples of its stream, if any.
fn reused(turns: &[SceneTurn]) -> Option<(usize, usize)> {
    for (i, a) in turns.iter().enumerate() {
        for (j, b) in turns.iter().enumerate().skip(i + 1) {
            if a.slot == b.slot && a.source.start < b.source.end && b.source.start < a.source.end {
                return Some((i, j));
            }
        }
    }
    None
}

fn scene_items(fixtures: &Path) -> Vec<Item> {
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
    let plan = scene_plan();
    let mut used: Vec<usize> = plan.iter().flat_map(|(_, s)| s.clone()).collect();
    used.sort_unstable();
    if used != (0..40).collect::<Vec<_>>() {
        fail("a speaker plays in two scenes or in none");
    }
    let mut items = Vec::new();
    for (index, (kind, positions)) in plan.into_iter().enumerate() {
        let streams: Vec<Vec<f32>> = positions
            .iter()
            .map(|&p| stream(&speakers[p], SCENE_S * HZ))
            .collect();
        let refs: Vec<&[f32]> = streams.iter().map(Vec::as_slice).collect();
        let (audio, turns) = build_scene(kind, &refs, SCENE_SEED + index as u64);
        if let Some((i, j)) = reused(&turns) {
            fail(format!("scene {index}: turns {i} and {j} reuse audio"));
        }
        let names: Vec<String> = positions
            .iter()
            .map(|&p| {
                speakers[p]
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default()
            })
            .collect();
        let reference: Vec<Turn> = turns
            .iter()
            .map(|t| {
                Turn::new(
                    t.start as f64 / HZ as f64,
                    (t.start + t.source.len()) as f64 / HZ as f64,
                    names[t.slot].clone(),
                )
            })
            .collect();
        let true_k = {
            let mut s: Vec<usize> = turns.iter().map(|t| t.slot).collect();
            s.sort_unstable();
            s.dedup();
            s.len()
        };
        let len = audio.len() as f64 / HZ as f64;
        items.push(Item {
            name: format!("{}-{index}", kind.name()),
            kind: kind.name(),
            audio: quantise_pcm16(&audio),
            reference,
            true_k,
            uem: (0.0, len),
        });
    }
    items
}

fn fixtures_dir() -> PathBuf {
    if let Some(d) = std::env::var_os("EARSHOT_FIXTURES").filter(|v| !v.is_empty()) {
        return PathBuf::from(d);
    }
    PathBuf::from(std::env::var_os("LOCALAPPDATA").unwrap_or_else(|| fail("LOCALAPPDATA")))
        .join("earshot-dev")
        .join("fixtures")
}

// ---- main --------------------------------------------------------------------------------

type Segmenter = Cached<earshot_onnx::segment::PyannoteSegmenter, LocalActivity>;
type Embedder = Cached<earshot_onnx::embed::CamppEmbedder, Embedding>;

fn diarize(
    parts: (Segmenter, Embedder),
    config: &DiarizationConfig,
    t: u32,
    audio: &MonoBuffer,
) -> (Diarization, (Segmenter, Embedder)) {
    let mut config = config.clone();
    config.clustering.threshold = threshold(t);
    let mut d = Diarizer::new(parts.0, parts.1, config).unwrap_or_else(|e| fail(e));
    let out = d
        .diarize(audio, MeetingTime::ZERO, &CancelToken::new())
        .unwrap_or_else(|e| fail(format!("diarize: {e}")));
    (out, d.into_parts())
}

fn pooled(scores: &[&Score]) -> f64 {
    let (e, t) = scores.iter().fold((0.0, 0.0), |(e, t), s| {
        (
            e + s.der.missed + s.der.false_alarm + s.der.confusion,
            t + s.der.total,
        )
    });
    e / t
}

fn round4(x: f64) -> f64 {
    (x * 10_000.0).round() / 10_000.0
}

fn main() {
    let mut it = std::env::args().skip(1);
    let root = PathBuf::from(
        it.next()
            .unwrap_or_else(|| fail("usage: cluster_sweep <repo-root> --models <dir>")),
    );
    let (mut models, mut out) = (None, None);
    while let Some(flag) = it.next() {
        let value = it
            .next()
            .unwrap_or_else(|| fail(format!("{flag} needs a value")));
        match flag.as_str() {
            "--models" => models = Some(ModelDir::new(value)),
            "--out" => out = Some(PathBuf::from(value)),
            other => fail(format!("unknown argument {other}")),
        }
    }
    let models = models.unwrap_or_else(|| fail("--models is required"));
    let short = short_curve(
        &std::fs::read_to_string(root.join("docs/far-speakers/long-scenes-calibration.txt"))
            .unwrap_or_else(|e| fail(format!("long-scenes-calibration.txt: {e}"))),
    );
    let fixtures = fixtures_dir();
    let mut items: Vec<Item> = AMI_DEV.iter().map(|m| ami_item(&fixtures, m)).collect();
    items.extend(scene_items(&fixtures));

    init_onnx(&models, Accelerator::Cpu).unwrap_or_else(|e| fail(format!("ONNX Runtime: {e}")));
    let (segmenter, embedder) =
        load_diarisation(&models, Accelerator::Cpu).unwrap_or_else(|e| fail(e));
    let model = embedder.info().model.clone();
    let post = PostConfig::default();
    let calibrated = post
        .clustering_threshold(&model)
        .unwrap_or_else(|e| fail(e));
    if (calibrated - threshold(HELD)).abs() > 1e-6 {
        fail(format!("the committed threshold is {calibrated}, not 0.68"));
    }
    let config = post.diarization.clone();
    let mut parts = (Cached::new(segmenter), Cached::new(embedder));

    // scores[item][grid index]
    let grid: Vec<u32> = GRID.collect();
    let mut scores: Vec<Vec<Score>> = Vec::new();
    for item in &items {
        eprintln!("cluster_sweep: {}", item.name);
        let buffer = MonoBuffer::new(PIPELINE_RATE, item.audio.clone());
        parts.0.replay = false;
        parts.1.replay = false;
        let (direct, p) = diarize(parts, &config, HELD, &buffer);
        parts = p;
        parts.0.replay = true;
        parts.1.replay = true;
        let mut row = Vec::new();
        for &t in &grid {
            let (d, p) = diarize(parts, &config, t, &buffer);
            parts = p;
            if t == HELD && d != direct {
                fail(format!(
                    "{}: the replay at 0.68 differs from the direct run",
                    item.name
                ));
            }
            row.push(score(item, &d));
        }
        // The cache serves one item only.
        parts.0.log.clear();
        parts.1.log.clear();
        scores.push(row);
    }

    let n_ami = AMI_DEV.len();
    let points: Vec<Point> = grid
        .iter()
        .enumerate()
        .map(|(g, &t)| {
            let col: Vec<&Score> = scores.iter().map(|r| &r[g]).collect();
            Point {
                t,
                a: pooled(&col[..n_ami]),
                l: pooled(&col[n_ami..]),
                c: col
                    .iter()
                    .zip(&items)
                    .map(|(s, i)| s.count_error(i.true_k))
                    .sum(),
                merges: col.iter().map(|s| s.merges).collect(),
                meeting_der: col[..n_ami].iter().map(|s| s.der.der()).collect(),
            }
        })
        .collect();
    let verdict = select(&points, &short);

    println!(
        "threshold S(t) A(t) L(t) C(t) | AMI merges extras | scene merges extras | found per meeting"
    );
    for (g, p) in points.iter().enumerate() {
        let col: Vec<&Score> = scores.iter().map(|r| &r[g]).collect();
        let sum = |r: std::ops::Range<usize>, f: fn(&Score) -> usize| -> usize {
            col[r].iter().map(|s| f(s)).sum()
        };
        let found: Vec<String> = col[..n_ami].iter().map(|s| s.found.to_string()).collect();
        println!(
            "{:.2} {:.4} {:.4} {:.4} {} | {} {} | {} {} | {}",
            threshold(p.t),
            short[&p.t],
            p.a,
            p.l,
            p.c,
            sum(0..n_ami, |s| s.merges),
            sum(0..n_ami, |s| s.extras),
            sum(n_ami..col.len(), |s| s.merges),
            sum(n_ami..col.len(), |s| s.extras),
            found.join(","),
        );
    }
    println!(
        "rule: admissible {:?}, winner {:.2}, picks {:.2} (held 0.68); move conditions {:?}; unconstrained AMI best {:.2}",
        verdict.admissible,
        threshold(verdict.winner),
        threshold(verdict.pick),
        verdict.move_conditions,
        threshold(verdict.unconstrained_ami_best),
    );

    let item_json = |i: usize, g: usize| -> Value {
        let s = &scores[i][g];
        json!({
            "name": items[i].name,
            "kind": items[i].kind,
            "true": items[i].true_k,
            "found": s.found,
            "merges": s.merges,
            "extras": s.extras,
            "extra_speech_s": round4(s.extra_speech_s),
            "lost": s.lost,
            "der": round4(s.der.der()),
            "missed": round4(s.der.missed),
            "false_alarm": round4(s.der.false_alarm),
            "confusion": round4(s.der.confusion),
            "total": round4(s.der.total),
        })
    };
    let report = json!({
        "model": model.to_string(),
        "rule": "docs/far-speakers/ami-dev-calibration.md",
        "items": items.iter().map(|i| json!({
            "name": i.name,
            "kind": i.kind,
            "true": i.true_k,
            "seconds": round4(i.audio.len() as f64 / HZ as f64),
            "scored": [round4(i.uem.0), round4(i.uem.1)],
        })).collect::<Vec<_>>(),
        "curve": points.iter().enumerate().map(|(g, p)| json!({
            "threshold": threshold(p.t),
            "short_scene_der": short[&p.t],
            "ami_der": round4(p.a),
            "scene_der": round4(p.l),
            "count_error": p.c,
            "items": (0..items.len()).map(|i| item_json(i, g)).collect::<Vec<_>>(),
        })).collect::<Vec<_>>(),
        "verdict": {
            "admissible": verdict.admissible.iter().map(|&t| threshold(t)).collect::<Vec<_>>(),
            "winner": threshold(verdict.winner),
            "pick": threshold(verdict.pick),
            "held": threshold(HELD),
            "move_conditions": {
                "ami_win": verdict.move_conditions.0,
                "scenes_not_worse": verdict.move_conditions.1,
                "no_meeting_much_worse": verdict.move_conditions.2,
            },
            "unconstrained_ami_best": threshold(verdict.unconstrained_ami_best),
        },
    });
    let text = serde_json::to_string_pretty(&report).unwrap_or_else(|e| fail(e));
    if let Some(out) = &out {
        std::fs::write(out, &text).unwrap_or_else(|e| fail(format!("{}: {e}", out.display())));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn turn(s: f64, e: f64, who: &str) -> Turn {
        Turn::new(s, e, who)
    }

    #[test]
    fn cluster_sweep_counts_merges_extras_and_lost_speakers() {
        let reference = [
            turn(0.0, 10.0, "a"),
            turn(10.0, 20.0, "b"),
            turn(20.0, 30.0, "c"),
            turn(40.0, 50.0, "d"),
        ];
        // a and b collapse into S0, c is S1, S2 is an extra, d overlaps nothing.
        let hyp = [
            turn(0.0, 20.0, "S0"),
            turn(20.0, 30.0, "S1"),
            turn(30.0, 32.0, "S2"),
        ];
        let (found, merges, extras, extra_speech, lost) =
            speakers_of(&reference, &hyp, (0.0, 60.0));
        assert_eq!((found, merges, extras, lost), (3, 1, 1, 1));
        assert!((extra_speech - 2.0).abs() < 1e-9);
        // Only what lies in the scored region counts.
        let (found, _, extras, _, _) = speakers_of(&reference, &hyp, (0.0, 29.0));
        assert_eq!((found, extras), (2, 0));
    }

    #[test]
    fn cluster_sweep_joins_close_intervals_only() {
        assert_eq!(
            join(vec![(2.2, 3.0), (0.0, 1.0), (1.4, 1.6), (3.6, 4.0)]),
            vec![(0.0, 1.6), (2.2, 3.0), (3.6, 4.0)]
        );
    }

    #[test]
    fn cluster_sweep_reads_word_times_and_skips_punctuation() {
        let xml = r#"<w nite:id="x" starttime="1.0" endtime="1.5">hi</w>
<w nite:id="y" starttime="1.5" endtime="1.5" punc="true">.</w>
<w nite:id="z" starttime="2.0" endtime="2.4" trunc="true">wh</w>
<w nite:id="q" starttime="3.0" endtime="2.0">bad</w>"#;
        assert_eq!(word_intervals(xml), vec![(1.0, 1.5), (2.0, 2.4)]);
    }

    fn streams(n: usize, seconds: usize) -> Vec<Vec<f32>> {
        (0..n)
            .map(|k| {
                (0..seconds * HZ)
                    .map(|i| ((i + k) as f32 * 1e-4).sin() * 0.1)
                    .collect()
            })
            .collect()
    }

    #[test]
    fn cluster_sweep_scenes_use_every_speaker_once_and_never_replay_audio() {
        let plan = scene_plan();
        assert_eq!(plan.len(), 16);
        let mut used: Vec<usize> = plan.iter().flat_map(|(_, s)| s.clone()).collect();
        used.sort_unstable();
        assert_eq!(used, (0..40).collect::<Vec<_>>());
        for (index, (kind, positions)) in plan.into_iter().enumerate() {
            let st = streams(positions.len(), 480);
            let refs: Vec<&[f32]> = st.iter().map(Vec::as_slice).collect();
            let (audio, turns) = build_scene(kind, &refs, SCENE_SEED + index as u64);
            assert_eq!(reused(&turns), None, "scene {index}");
            assert!(audio.len() <= SCENE_S * HZ, "scene {index}");
            assert!(
                audio.len() > (SCENE_S - 30) * HZ,
                "scene {index}: {}",
                audio.len()
            );
            let mut slots: Vec<usize> = turns.iter().map(|t| t.slot).collect();
            slots.sort_unstable();
            slots.dedup();
            assert_eq!(slots.len(), positions.len(), "scene {index}");
            // Every turn's source is the next stretch of its stream.
            for s in 0..positions.len() {
                let mut at = 0;
                for t in turns.iter().filter(|t| t.slot == s) {
                    assert_eq!(t.source.start, at);
                    at = t.source.end;
                }
            }
        }
    }

    #[test]
    fn cluster_sweep_scene_ends_when_a_stream_runs_out() {
        let st = streams(2, 100);
        let refs: Vec<&[f32]> = st.iter().map(Vec::as_slice).collect();
        let (audio, turns) = build_scene(Kind::Dominant, &refs, SCENE_SEED);
        assert!(audio.len() < SCENE_S * HZ);
        assert_eq!(reused(&turns), None);
    }

    fn point(t: u32, a: f64, l: f64, c: usize, merges: Vec<usize>) -> Point {
        Point {
            t,
            a,
            l,
            c,
            meeting_der: vec![a],
            merges,
        }
    }

    fn valley() -> HashMap<u32, f64> {
        [(67, 0.20), (68, 0.178), (70, 0.179), (83, 0.181)]
            .into_iter()
            .collect()
    }

    #[test]
    fn cluster_sweep_rule_moves_only_on_a_clear_ami_win() {
        let short = valley();
        let base = |a70: f64| {
            vec![
                point(67, 0.10, 0.10, 4, vec![0, 0]),
                point(68, 0.20, 0.10, 4, vec![1, 0]),
                point(70, a70, 0.10, 4, vec![1, 0]),
                point(83, 0.30, 0.09, 4, vec![2, 0]),
            ]
        };
        // 0.67 is outside the valley, 0.83 merges more: only 0.68 and 0.70 are admissible.
        let v = select(&base(0.196), &short);
        assert_eq!(v.admissible, vec![68, 70]);
        assert_eq!((v.winner, v.pick), (70, 68));
        assert_eq!(v.unconstrained_ami_best, 67);
        let v = select(&base(0.195), &short);
        assert_eq!((v.winner, v.pick), (70, 70));
    }

    #[test]
    fn cluster_sweep_rule_count_branch_needs_a_count_to_fall_by_two() {
        let short = valley();
        let pts = |c68: usize, c70: usize| {
            vec![
                point(68, 0.20, 0.10, c68, vec![0]),
                point(70, 0.20, 0.10, c70, vec![0]),
            ]
        };
        assert_eq!(select(&pts(3, 1), &short).pick, 70);
        assert_eq!(select(&pts(2, 1), &short).pick, 68);
        // Scenes worse by more than the slack stop a move.
        let mut p = pts(3, 1);
        p[1].l = 0.103;
        assert_eq!(select(&p, &short).pick, 68);
    }

    #[test]
    fn cluster_sweep_reads_the_committed_short_scene_curve() {
        let text = "header line\nthreshold S(t) L(t)\n0.68 0.1782 0.1301 0.075 | x\n0.83 0.1812 0.1249 0.000\nrule: winner 0.83";
        let c = short_curve(text);
        assert_eq!(c.len(), 2);
        assert_eq!(c[&68], 0.1782);
        assert_eq!(c[&83], 0.1812);
    }
}
