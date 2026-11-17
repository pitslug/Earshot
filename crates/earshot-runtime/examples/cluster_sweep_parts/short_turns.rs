//! The short-turn talker measures and rule of `docs/far-speakers/short-turns.md` (T12.66), for
//! `cluster_sweep --short-turns`: 10 dev-clean short-turn scenes beside the AMI dev mixes and
//! the long scenes, the small-cluster regrouping of `earshot_core::diar::cluster` (step 6)
//! swept over its grid, and the rule's pick. Counts and times only.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use earshot_core::audio::{MonoBuffer, PIPELINE_RATE, quantise_pcm16};
use earshot_core::diar::{Diarization, DiarizationConfig, Regroup};
use earshot_core::metrics::der::Turn;
use serde_json::json;

use super::{
    AMI_DEV, Embedder, FileSpan, HELD, HZ, Item, SceneTurn, Score, Segmenter, dev_clean_speakers,
    diarize_at, draw, fail, files_used, floors, mix, pooled, reused, round4, score, speaker_name,
    stream_files,
};

/// Short-turn scene draws are seeded with this plus the scene index.
pub const SHORT_SEED: u64 = 16_600;
/// Short-turn scenes.
pub const SHORT_SCENES: usize = 10;
/// Short-turn scene length (s).
pub const SHORT_SCENE_S: usize = 300;
/// Long turns (ms, inclusive).
pub const LONG_TURN_MS: (usize, usize) = (8_000, 20_000);
/// The talker's turns (ms, inclusive).
pub const SHORT_TURN_MS: (usize, usize) = (1_000, 3_500);
/// Gaps between turns (ms, inclusive).
pub const TURN_GAP_MS: (usize, usize) = (100, 800);
/// The talker stays quiet after a long turn with probability one in this (speaks otherwise).
pub const SHORT_SKIP_ONE_IN: usize = 3;
/// The least talker speech an S scene may hold (s): the top of the m grid, so every
/// minimum on the grid can be met in every scene.
pub const MIN_TALKER_SPEECH_S: f64 = 16.0;
/// The long scenes.
pub const LONG_SCENES: usize = 16;

/// Candidate floors (cosine, hundredths).
pub const F_GRID: [i32; 8] = [0, 10, 20, 30, 40, 50, 60, 100];
/// Candidate distances (hundredths).
pub const D_GRID: [u32; 5] = [68, 80, 90, 100, 110];
/// Candidate minimum speech (s).
pub const M_GRID: [u32; 3] = [4, 8, 16];
/// Rule 1: talkers found on the short scenes at least this many.
pub const MIN_TALKERS: usize = 5;
/// Rule 2: at most this many false long scenes.
pub const MAX_FALSE_L_SCENES: usize = 1;
/// Rule 3: at most this many false new speakers over the short scenes.
pub const MAX_FALSE_NEW_S: usize = 2;
/// Rule 4: how much worse one AMI dev meeting's DER may get.
pub const MEETING_DER_SLACK: f64 = 0.005;

/// One grid setting.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Setting {
    /// Floor, hundredths.
    pub f: i32,
    /// Distance, hundredths.
    pub d: u32,
    /// Minimum speech, seconds.
    pub m: u32,
    pub weighted: bool,
}

impl Setting {
    pub fn regroup(self) -> Regroup {
        Regroup {
            floor: self.f as f32 / 100.0,
            distance: self.d as f32 / 100.0,
            min_speech_s: self.m as f32,
            weighted: self.weighted,
        }
    }
}

/// The 240 settings: floor, then distance, then minimum speech, then plain before weighted.
pub fn grid() -> Vec<Setting> {
    let mut out = Vec::new();
    for f in F_GRID {
        for d in D_GRID {
            for m in M_GRID {
                for weighted in [false, true] {
                    out.push(Setting { f, d, m, weighted });
                }
            }
        }
    }
    out
}

// ---- the short-turn scenes ---------------------------------------------------------------

/// Speaker positions of short scene `i`: the talker first, then the three long-turn speakers.
pub fn short_positions(i: usize) -> [usize; 4] {
    [i, 10 + 3 * i, 11 + 3 * i, 12 + 3 * i]
}

/// Builds a short-turn scene from four streams (slot 0 the talker, 1-3 the long-turn
/// speakers) under the rule doc. Long-turn speakers take turns in a random order, never the
/// same one in two long turns running; after each long turn the talker speaks one turn with
/// probability 2/3 (unless a draw of one in [`SHORT_SKIP_ONE_IN`] says skip); gaps of 100-800 ms, no overlap. A turn that the scene's end or its stream
/// would cut below its shortest length ends the scene instead.
pub fn build_short_scene(streams: &[&[f32]], seed: u64) -> (Vec<f32>, Vec<SceneTurn>) {
    let ms = |m: usize| m * HZ / 1000;
    let end = SHORT_SCENE_S * HZ;
    let mut state = seed;
    let mut cursor = [0usize; 4];
    let mut turns: Vec<SceneTurn> = Vec::new();
    let mut prev_end: Option<usize> = None;
    let mut prev_long: Option<usize> = None;
    // Places one turn; false when it does not fit.
    let mut place = |turns: &mut Vec<SceneTurn>,
                     prev_end: &mut Option<usize>,
                     slot: usize,
                     want: usize,
                     start: usize,
                     min: usize| {
        if start >= end {
            return false;
        }
        let left = streams[slot].len() - cursor[slot];
        let len = want.min(left).min(end - start);
        if len < min {
            return false;
        }
        let source = cursor[slot]..cursor[slot] + len;
        cursor[slot] += len;
        *prev_end = Some(start + len);
        turns.push(SceneTurn {
            slot,
            start,
            source,
        });
        true
    };
    loop {
        let others: Vec<usize> = (1..4).filter(|&s| Some(s) != prev_long).collect();
        let slot = others[draw(&mut state, 0, others.len())];
        let want = ms(draw(&mut state, LONG_TURN_MS.0, LONG_TURN_MS.1 + 1));
        let start = prev_end.map_or(0, |e| {
            e + ms(draw(&mut state, TURN_GAP_MS.0, TURN_GAP_MS.1 + 1))
        });
        if !place(
            &mut turns,
            &mut prev_end,
            slot,
            want,
            start,
            ms(LONG_TURN_MS.0),
        ) {
            break;
        }
        prev_long = Some(slot);
        if draw(&mut state, 0, SHORT_SKIP_ONE_IN) != 0 {
            let want = ms(draw(&mut state, SHORT_TURN_MS.0, SHORT_TURN_MS.1 + 1));
            let start =
                prev_end.unwrap_or(0) + ms(draw(&mut state, TURN_GAP_MS.0, TURN_GAP_MS.1 + 1));
            if !place(
                &mut turns,
                &mut prev_end,
                0,
                want,
                start,
                ms(SHORT_TURN_MS.0),
            ) {
                break;
            }
        }
    }
    (mix(streams, &turns), turns)
}

/// The short scenes with their talkers.
pub struct ShortItems {
    pub items: Vec<Item>,
    /// Each scene's short-turn talker (reference label).
    pub talkers: Vec<String>,
    /// Each scene's talker speech (s).
    pub talker_speech_s: Vec<f64>,
}

/// Builds the 10 short-turn scenes. Fails unless every speaker plays in exactly one, no turn
/// within a scene replays samples, and no speaker's utterance file is played by both its long
/// scene (`long_files`, per speaker position) and its short scene.
pub fn short_items(fixtures: &Path, long_files: &[BTreeSet<PathBuf>]) -> ShortItems {
    let speakers = dev_clean_speakers(fixtures);
    let mut used: Vec<usize> = (0..SHORT_SCENES).flat_map(short_positions).collect();
    used.sort_unstable();
    if used != (0..speakers.len()).collect::<Vec<_>>() {
        fail("a speaker plays in two short scenes or in none");
    }
    let mut out = ShortItems {
        items: Vec::new(),
        talkers: Vec::new(),
        talker_speech_s: Vec::new(),
    };
    for i in 0..SHORT_SCENES {
        let positions = short_positions(i);
        let (streams, files): (Vec<Vec<f32>>, Vec<Vec<FileSpan>>) = positions
            .iter()
            .map(|&p| stream_files(&speakers[p], SHORT_SCENE_S * HZ, true))
            .unzip();
        let refs: Vec<&[f32]> = streams.iter().map(Vec::as_slice).collect();
        let (audio, turns) = build_short_scene(&refs, SHORT_SEED + i as u64);
        if let Some((a, b)) = reused(&turns) {
            fail(format!("short scene {i}: turns {a} and {b} reuse audio"));
        }
        for (slot, &p) in positions.iter().enumerate() {
            let short = files_used(&turns, slot, &files[slot]);
            if short.intersection(&long_files[p]).next().is_some() {
                fail(format!(
                    "short scene {i}: a speaker plays an utterance its long scene played"
                ));
            }
        }
        let names: Vec<String> = positions
            .iter()
            .map(|&p| speaker_name(&speakers[p]))
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
        let talker_speech: usize = turns
            .iter()
            .filter(|t| t.slot == 0)
            .map(|t| t.source.len())
            .sum();
        let true_k = {
            let mut s: Vec<usize> = turns.iter().map(|t| t.slot).collect();
            s.sort_unstable();
            s.dedup();
            s.len()
        };
        let len = audio.len() as f64 / HZ as f64;
        out.items.push(Item {
            name: format!("short-{i}"),
            kind: "short",
            audio: quantise_pcm16(&audio),
            reference,
            true_k,
            uem: (0.0, len),
        });
        if (talker_speech as f64 / HZ as f64) < MIN_TALKER_SPEECH_S {
            fail(format!(
                "short scene {i}: the talker speaks less than {MIN_TALKER_SPEECH_S} s"
            ));
        }
        out.talkers.push(names[0].clone());
        out.talker_speech_s.push(talker_speech as f64 / HZ as f64);
    }
    out
}

// ---- measures ----------------------------------------------------------------------------

/// What one item measures at one setting.
#[derive(Debug, Clone)]
pub struct ItemMeasure {
    pub score: Score,
    /// The item's short-turn talker owns a found speaker (short scenes only).
    pub talker_found: bool,
    /// New speakers inside the scored region.
    pub new_speakers: usize,
    /// New speakers not owned by the item's short-turn talker.
    pub false_new: usize,
    /// Seconds that left a baseline speaker for a new one, the reference holding the new
    /// speaker's owner there.
    pub moved_right_s: f64,
    /// The other moved seconds.
    pub moved_wrong_s: f64,
    /// Seconds in which a baseline speaker holds a frame it did not hold in the baseline (the
    /// regrouping promises 0).
    pub gained_s: f64,
    /// A speaker of the setting whose cluster holds no frame in the baseline (it gained every
    /// frame it holds), if any.
    pub unmatched: Option<usize>,
}

fn label_index(label: &str) -> usize {
    label
        .strip_prefix('S')
        .and_then(|n| n.parse().ok())
        .unwrap_or_else(|| fail(format!("unexpected speaker label {label}")))
}

/// What a speaker of a regrouped run is in the baseline.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kept {
    /// A new speaker of the regrouping.
    New,
    /// The baseline speaker from the same cluster.
    Base(usize),
    /// A baseline cluster that holds no frame in the baseline.
    Missing,
}

/// Per speaker of `at`, the speaker of `base` from the same cluster of the clustering (both runs
/// cluster the same embeddings the same way), a new speaker, or a cluster the baseline gave no
/// frame.
pub fn old_speakers(base: &Diarization, at: &Diarization) -> Vec<Kept> {
    if base.speaker_clusters.len() != base.speakers.len()
        || at.speaker_clusters.len() != at.speakers.len()
    {
        fail("a diarisation does not say which cluster each speaker came from");
    }
    (0..at.speakers.len())
        .map(|i| {
            if at.new_speakers.contains(&i) {
                return Kept::New;
            }
            base.speaker_clusters
                .iter()
                .position(|&c| c == at.speaker_clusters[i])
                .map_or(Kept::Missing, Kept::Base)
        })
        .collect()
}

/// The rule doc's measures of `at` against the baseline `base` (both of `item`), `talker` the
/// short-turn talker of a short scene.
pub fn measure(
    item: &Item,
    base: &Diarization,
    at: &Diarization,
    talker: Option<&str>,
) -> ItemMeasure {
    let uem = item.uem;
    let hyp = at.der_turns();
    let owner = floors::owners(&item.reference, &hyp, uem);
    let owner_of = |label: &str| {
        owner
            .iter()
            .find(|(f, _)| f == label)
            .map(|(_, t)| t.as_str())
    };
    let talker_found = talker.is_some_and(|t| owner.iter().any(|(_, o)| o == t));
    let in_uem: BTreeSet<&str> = hyp
        .iter()
        .filter(|t| t.end.min(uem.1) > t.start.max(uem.0))
        .map(|t| t.speaker.as_str())
        .collect();
    let new_labels: Vec<String> = at
        .new_speakers
        .iter()
        .map(|i| format!("S{i}"))
        .filter(|l| in_uem.contains(l.as_str()))
        .collect();
    let false_new = new_labels
        .iter()
        .filter(|l| talker.is_none() || owner_of(l) != talker)
        .count();

    let map = old_speakers(base, at);
    let unmatched = map.iter().position(|k| *k == Kept::Missing);
    let refs = floors::paint(&item.reference, uem);
    let was = floors::paint(&base.der_turns(), uem);
    let now = floors::paint(&hyp, uem);
    let (mut right, mut wrong, mut gained) = (0.0, 0.0, 0.0);
    for i in 0..floors::frames(uem) {
        let was_here: Vec<usize> = was[i].iter().map(|l| label_index(l)).collect();
        // A baseline speaker on here that was not on here in the baseline.
        if now[i].iter().any(|l| match map[label_index(l)] {
            Kept::New => false,
            Kept::Base(b) => !was_here.contains(&b),
            Kept::Missing => true,
        }) {
            gained += floors::FRAME_S;
        }
        let new_here: Vec<&String> = now[i]
            .iter()
            .filter(|l| map[label_index(l)] == Kept::New)
            .collect();
        if new_here.is_empty() {
            continue;
        }
        let kept: Vec<usize> = now[i]
            .iter()
            .filter_map(|l| match map[label_index(l)] {
                Kept::Base(b) => Some(b),
                _ => None,
            })
            .collect();
        let lost = was_here.iter().any(|b| !kept.contains(b));
        if !lost {
            continue;
        }
        let ok = new_here
            .iter()
            .any(|l| owner_of(l).is_some_and(|o| refs[i].iter().any(|r| r == o)));
        if ok {
            right += floors::FRAME_S;
        } else {
            wrong += floors::FRAME_S;
        }
    }
    ItemMeasure {
        score: score(item, at),
        talker_found,
        new_speakers: new_labels.len(),
        false_new,
        moved_right_s: right,
        moved_wrong_s: wrong,
        gained_s: gained,
        unmatched,
    }
}

// ---- the rule ----------------------------------------------------------------------------

/// One setting (or the baseline) pooled over every item.
#[derive(Debug, Clone, PartialEq)]
pub struct ShortPoint {
    pub setting: Setting,
    /// Talkers found on the short scenes.
    pub talkers: usize,
    /// Long scenes with at least one new speaker.
    pub false_l_scenes: usize,
    /// False new speakers summed over the short scenes.
    pub false_new_s: usize,
    /// False new speakers over every item.
    pub false_new_all: usize,
    /// Each AMI dev meeting's DER.
    pub meeting_der: Vec<f64>,
    /// Pooled AMI dev DER.
    pub a_der: f64,
    /// Pooled short-scene DER.
    pub s_der: f64,
}

/// The baseline's values the rule compares with.
#[derive(Debug, Clone, PartialEq)]
pub struct Baseline {
    pub talkers: usize,
    pub meeting_der: Vec<f64>,
    pub s_der: f64,
}

/// The five rule conditions at `p`: finds the talker, few false long scenes, few false short
/// speakers, no AMI dev meeting worse, short scenes not worse.
pub fn conditions(p: &ShortPoint, base: &Baseline) -> [bool; 5] {
    [
        p.talkers >= MIN_TALKERS && p.talkers > base.talkers,
        p.false_l_scenes <= MAX_FALSE_L_SCENES,
        p.false_new_s <= MAX_FALSE_NEW_S,
        p.meeting_der
            .iter()
            .zip(&base.meeting_der)
            .all(|(d, d0)| *d <= *d0 + MEETING_DER_SLACK),
        p.s_der <= base.s_der,
    ]
}

/// The rule's pick (an index into `points`), or `None` when no setting passes: most talkers
/// found, then fewest false new speakers, then lowest pooled AMI dev DER, then the highest
/// floor, the lowest distance, the highest minimum speech, plain before weighted.
pub fn pick(points: &[ShortPoint], base: &Baseline) -> Option<usize> {
    (0..points.len())
        .filter(|&i| conditions(&points[i], base).iter().all(|c| *c))
        .min_by(|&x, &y| {
            let (a, b) = (&points[x], &points[y]);
            b.talkers
                .cmp(&a.talkers)
                .then(a.false_new_all.cmp(&b.false_new_all))
                .then(a.a_der.total_cmp(&b.a_der))
                .then(b.setting.f.cmp(&a.setting.f))
                .then(a.setting.d.cmp(&b.setting.d))
                .then(b.setting.m.cmp(&a.setting.m))
                .then(a.setting.weighted.cmp(&b.setting.weighted))
        })
}

// ---- the run -----------------------------------------------------------------------------

fn diarize_with(
    parts: (Segmenter, Embedder),
    config: &DiarizationConfig,
    regroup: Option<Regroup>,
    audio: &MonoBuffer,
) -> (Diarization, (Segmenter, Embedder)) {
    let mut config = config.clone();
    config.clustering.regroup = regroup;
    diarize_at(parts, &config, HELD, floors::OFF, audio)
}

/// `--short-turns`: `items` are the AMI dev meetings then the long scenes; the short scenes
/// are added after them. Records each item's model outputs once in a direct baseline run,
/// checks the replayed baseline equals it, replays every setting, prints the table and writes
/// the report to `out`.
pub fn run(
    mut items: Vec<Item>,
    short: ShortItems,
    mut parts: (Segmenter, Embedder),
    config: &DiarizationConfig,
    model: &str,
    out: Option<&Path>,
) {
    let n_ami = AMI_DEV.len();
    if items.len() != n_ami + LONG_SCENES {
        fail("the short-turn run needs the AMI dev meetings and the 16 long scenes");
    }
    let n_long = n_ami + LONG_SCENES;
    items.extend(short.items);
    let talker = |i: usize| (i >= n_long).then(|| short.talkers[i - n_long].as_str());
    let settings = grid();
    let max_train = config.clustering.max_train_embeddings;
    // baseline[item], rows[item][setting], embeddings[item]
    let mut baseline: Vec<ItemMeasure> = Vec::new();
    let mut rows: Vec<Vec<ItemMeasure>> = Vec::new();
    let mut embeddings: Vec<usize> = Vec::new();
    for (i, item) in items.iter().enumerate() {
        eprintln!("cluster_sweep --short-turns: {}", item.name);
        let buffer = MonoBuffer::new(PIPELINE_RATE, item.audio.clone());
        parts.0.replay = false;
        parts.1.replay = false;
        parts.1.outputs = 0;
        let (direct, p) = diarize_with(parts, config, None, &buffer);
        parts = p;
        embeddings.push(parts.1.outputs);
        parts.0.replay = true;
        parts.1.replay = true;
        let (again, p) = diarize_with(parts, config, None, &buffer);
        parts = p;
        if again != direct {
            fail(format!(
                "{}: the replayed baseline differs from the direct run",
                item.name
            ));
        }
        if !direct.new_speakers.is_empty() {
            fail(format!("{}: the baseline made a new speaker", item.name));
        }
        baseline.push(measure(item, &direct, &direct, talker(i)));
        let mut row = Vec::with_capacity(settings.len());
        for s in &settings {
            let (d, p) = diarize_with(parts, config, Some(s.regroup()), &buffer);
            parts = p;
            let m = measure(item, &direct, &d, talker(i));
            if let Some(k) = m.unmatched {
                fail(format!(
                    "{} at {s:?}: speaker S{k} comes from a cluster that holds no frame in the \
                     baseline, so a baseline speaker gained frames",
                    item.name
                ));
            }
            if m.gained_s > 0.0 {
                fail(format!(
                    "{} at {s:?}: baseline speakers hold {:.2} s they did not hold in the \
                     baseline",
                    item.name, m.gained_s
                ));
            }
            row.push(m);
        }
        parts.0.log.clear();
        parts.1.log.clear();
        rows.push(row);
    }

    let point_of = |col: &[&ItemMeasure], setting: Setting| {
        let scores: Vec<&Score> = col.iter().map(|m| &m.score).collect();
        ShortPoint {
            setting,
            talkers: col[n_long..].iter().filter(|m| m.talker_found).count(),
            false_l_scenes: col[n_ami..n_long]
                .iter()
                .filter(|m| m.new_speakers > 0)
                .count(),
            false_new_s: col[n_long..].iter().map(|m| m.false_new).sum(),
            false_new_all: col.iter().map(|m| m.false_new).sum(),
            meeting_der: scores[..n_ami].iter().map(|s| s.der.der()).collect(),
            a_der: pooled(&scores[..n_ami]),
            s_der: pooled(&scores[n_long..]),
        }
    };
    let base_col: Vec<&ItemMeasure> = baseline.iter().collect();
    let base_point = point_of(
        &base_col,
        Setting {
            f: floors::OFF,
            d: 0,
            m: 0,
            weighted: false,
        },
    );
    let base = Baseline {
        talkers: base_point.talkers,
        meeting_der: base_point.meeting_der.clone(),
        s_der: base_point.s_der,
    };
    let columns: Vec<Vec<&ItemMeasure>> = (0..settings.len())
        .map(|g| rows.iter().map(|r| &r[g]).collect())
        .collect();
    let points: Vec<ShortPoint> = settings
        .iter()
        .zip(&columns)
        .map(|(s, col)| point_of(col, *s))
        .collect();
    let picked = pick(&points, &base);
    let l_der = |col: &[&ItemMeasure]| {
        let s: Vec<&Score> = col[n_ami..n_long].iter().map(|m| &m.score).collect();
        pooled(&s)
    };
    let moved = |col: &[&ItemMeasure]| {
        (
            col.iter().map(|m| m.moved_right_s).sum::<f64>(),
            col.iter().map(|m| m.moved_wrong_s).sum::<f64>(),
        )
    };
    let gained = |col: &[&ItemMeasure]| col.iter().map(|m| m.gained_s).sum::<f64>();

    println!(
        "talker speech per short scene (s): {}",
        short
            .talker_speech_s
            .iter()
            .map(|s| format!("{s:.1}"))
            .collect::<Vec<_>>()
            .join(",")
    );
    println!(
        "embeddings per item: {}",
        embeddings
            .iter()
            .map(|e| e.to_string())
            .collect::<Vec<_>>()
            .join(",")
    );
    println!(
        "f d m w | talkers falseL falseS falseAll | moved right wrong gained | A L S | conditions"
    );
    println!(
        "baseline | {} - - - | - - - | {:.4} {:.4} {:.4} | -",
        base_point.talkers,
        base_point.a_der,
        l_der(&base_col),
        base_point.s_der
    );
    for (p, col) in points.iter().zip(&columns) {
        let (right, wrong) = moved(col);
        println!(
            "{:.2} {:.2} {} {} | {} {} {} {} | {:.1} {:.1} {:.2} | {:.4} {:.4} {:.4} | {:?}",
            p.setting.f as f64 / 100.0,
            p.setting.d as f64 / 100.0,
            p.setting.m,
            if p.setting.weighted { "w" } else { "p" },
            p.talkers,
            p.false_l_scenes,
            p.false_new_s,
            p.false_new_all,
            right,
            wrong,
            gained(col),
            p.a_der,
            l_der(col),
            p.s_der,
            conditions(p, &base),
        );
    }
    match picked {
        Some(i) => {
            let s = points[i].setting;
            println!(
                "rule: picks f {:.2} d {:.2} m {} {}",
                s.f as f64 / 100.0,
                s.d as f64 / 100.0,
                s.m,
                if s.weighted { "weighted" } else { "plain" }
            );
        }
        None => println!("rule: no setting passes; the regrouping stays off"),
    }

    let item_json = |i: usize, m: &ItemMeasure| {
        json!({
            "name": items[i].name,
            "kind": items[i].kind,
            "der": round4(m.score.der.der()),
            "missed": round4(m.score.der.missed),
            "false_alarm": round4(m.score.der.false_alarm),
            "confusion": round4(m.score.der.confusion),
            "total": round4(m.score.der.total),
            "found": m.score.found,
            "talker_found": m.talker_found,
            "new_speakers": m.new_speakers,
            "false_new": m.false_new,
            "moved_s": round4(m.moved_right_s + m.moved_wrong_s),
            "moved_right_s": round4(m.moved_right_s),
            "moved_wrong_s": round4(m.moved_wrong_s),
            "baseline_gained_s": round4(m.gained_s),
        })
    };
    let setting_json = |s: &Setting| {
        json!({
            "floor": s.f as f64 / 100.0,
            "distance": s.d as f64 / 100.0,
            "min_speech_s": s.m,
            "weighting": if s.weighted { "weighted" } else { "plain" },
        })
    };
    let report = json!({
        "model": model,
        "rule": "docs/far-speakers/short-turns.md",
        "threshold": super::threshold(HELD),
        "unassigned_floor": floors::floor(floors::OFF),
        "max_train_embeddings": max_train,
        "items": items.iter().zip(&embeddings).map(|(i, &e)| json!({
            "name": i.name,
            "kind": i.kind,
            "seconds": round4(i.audio.len() as f64 / HZ as f64),
            "embeddings": e,
            "over_max_train_embeddings": e > max_train,
        })).collect::<Vec<_>>(),
        "short_scenes": (0..SHORT_SCENES).map(|i| json!({
            "name": items[n_long + i].name,
            "talker_speech_s": round4(short.talker_speech_s[i]),
            "seconds": round4(items[n_long + i].audio.len() as f64 / HZ as f64),
        })).collect::<Vec<_>>(),
        "baseline": {
            "talkers_found": base_point.talkers,
            "ami_der": round4(base_point.a_der),
            "long_der": round4(l_der(&base_col)),
            "short_der": round4(base_point.s_der),
            "items": baseline.iter().enumerate().map(|(i, m)| item_json(i, m)).collect::<Vec<_>>(),
        },
        "settings": points.iter().zip(&columns).map(|(p, col)| {
            let (right, wrong) = moved(col);
            json!({
                "setting": setting_json(&p.setting),
                "talkers_found": p.talkers,
                "false_long_scenes": p.false_l_scenes,
                "false_new_short": p.false_new_s,
                "false_new_all": p.false_new_all,
                "moved_s": round4(right + wrong),
                "moved_right_s": round4(right),
                "moved_wrong_s": round4(wrong),
                "baseline_gained_s": round4(gained(col)),
                "ami_der": round4(p.a_der),
                "long_der": round4(l_der(col)),
                "short_der": round4(p.s_der),
                "conditions": conditions(p, &base),
                "items": col.iter().enumerate().map(|(i, m)| item_json(i, m)).collect::<Vec<_>>(),
            })
        }).collect::<Vec<_>>(),
        "pick": picked.map(|i| setting_json(&points[i].setting)),
    });
    let text = serde_json::to_string_pretty(&report).unwrap_or_else(|e| fail(e));
    if let Some(out) = out {
        std::fs::write(out, &text).unwrap_or_else(|e| fail(format!("{}: {e}", out.display())));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn cluster_sweep_short_scenes_use_every_speaker_once_and_keep_their_bounds() {
        let mut used: Vec<usize> = (0..SHORT_SCENES).flat_map(short_positions).collect();
        used.sort_unstable();
        assert_eq!(used, (0..40).collect::<Vec<_>>());
        let ms = |m: usize| m * HZ / 1000;
        let st = streams(4, SHORT_SCENE_S);
        let refs: Vec<&[f32]> = st.iter().map(Vec::as_slice).collect();
        for i in 0..SHORT_SCENES {
            let (audio, turns) = build_short_scene(&refs, SHORT_SEED + i as u64);
            assert_eq!(reused(&turns), None, "scene {i}");
            assert!(audio.len() <= SHORT_SCENE_S * HZ, "scene {i}");
            assert!(
                audio.len() + ms(LONG_TURN_MS.0 + TURN_GAP_MS.1 + SHORT_TURN_MS.1)
                    > SHORT_SCENE_S * HZ,
                "scene {i}: {}",
                audio.len()
            );
            let mut prev_long: Option<usize> = None;
            let mut talker = 0;
            for (k, t) in turns.iter().enumerate() {
                let len = t.source.len();
                if t.slot == 0 {
                    talker += len;
                    assert!(
                        (ms(SHORT_TURN_MS.0)..=ms(SHORT_TURN_MS.1)).contains(&len),
                        "scene {i} turn {k}: {len}"
                    );
                    // The talker only follows a long turn.
                    assert!(k > 0 && turns[k - 1].slot != 0, "scene {i} turn {k}");
                } else {
                    assert!(
                        (ms(LONG_TURN_MS.0)..=ms(LONG_TURN_MS.1)).contains(&len),
                        "scene {i} turn {k}: {len}"
                    );
                    assert_ne!(prev_long, Some(t.slot), "scene {i} turn {k}");
                    prev_long = Some(t.slot);
                }
                if k > 0 {
                    let gap = t.start - (turns[k - 1].start + turns[k - 1].source.len());
                    assert!(
                        (ms(TURN_GAP_MS.0)..=ms(TURN_GAP_MS.1)).contains(&gap),
                        "scene {i} turn {k}: gap {gap}"
                    );
                } else {
                    assert_eq!(t.start, 0);
                }
            }
            // Every long-turn speaker and the talker speak, and every minimum on the grid can be met.
            assert!(
                talker as f64 / HZ as f64 >= MIN_TALKER_SPEECH_S,
                "scene {i}: talker {talker}"
            );
            for slot in 0..4 {
                assert!(
                    turns.iter().any(|t| t.slot == slot),
                    "scene {i} slot {slot}"
                );
            }
            eprintln!("scene {i}: talker {:.2} s", talker as f64 / HZ as f64);
            // Every turn's source is the next stretch of its stream.
            for s in 0..4 {
                let mut at = 0;
                for t in turns.iter().filter(|t| t.slot == s) {
                    assert_eq!(t.source.start, at);
                    at = t.source.end;
                }
            }
        }
    }

    #[test]
    fn cluster_sweep_short_scene_ends_when_a_stream_runs_out() {
        let mut st = streams(4, SHORT_SCENE_S);
        st[0].truncate(5 * HZ);
        let refs: Vec<&[f32]> = st.iter().map(Vec::as_slice).collect();
        let (audio, turns) = build_short_scene(&refs, SHORT_SEED);
        assert!(audio.len() < SHORT_SCENE_S * HZ - 30 * HZ);
        assert_eq!(reused(&turns), None);
    }

    #[test]
    fn cluster_sweep_short_files_used_are_the_files_a_turn_plays() {
        let files: Vec<FileSpan> = vec![
            (PathBuf::from("a"), 0..100),
            (PathBuf::from("b"), 110..200),
            (PathBuf::from("c"), 210..300),
        ];
        let turns = vec![
            SceneTurn {
                slot: 0,
                start: 0,
                source: 100..110,
            },
            SceneTurn {
                slot: 0,
                start: 500,
                source: 150..215,
            },
            SceneTurn {
                slot: 1,
                start: 900,
                source: 0..300,
            },
        ];
        // The first turn plays only the gap after "a".
        let used: Vec<String> = files_used(&turns, 0, &files)
            .into_iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect();
        assert_eq!(used, vec!["b", "c"]);
    }

    fn setting(f: i32, d: u32, m: u32, weighted: bool) -> Setting {
        Setting { f, d, m, weighted }
    }

    fn passing(s: Setting) -> ShortPoint {
        ShortPoint {
            setting: s,
            talkers: 6,
            false_l_scenes: 1,
            false_new_s: 2,
            false_new_all: 4,
            meeting_der: vec![0.20, 0.30],
            a_der: 0.25,
            s_der: 0.10,
        }
    }

    fn baseline() -> Baseline {
        Baseline {
            talkers: 2,
            meeting_der: vec![0.20, 0.30],
            s_der: 0.10,
        }
    }

    #[test]
    fn cluster_sweep_short_rule_fails_each_condition_alone() {
        let base = baseline();
        let ok = passing(setting(30, 80, 8, false));
        assert_eq!(conditions(&ok, &base), [true; 5]);
        assert_eq!(pick(std::slice::from_ref(&ok), &base), Some(0));
        let fails = |p: ShortPoint, which: usize| {
            let mut want = [true; 5];
            want[which] = false;
            assert_eq!(conditions(&p, &base), want, "{p:?}");
            assert_eq!(pick(&[p], &base), None);
        };
        fails(
            ShortPoint {
                talkers: 4,
                ..ok.clone()
            },
            0,
        );
        // At least 5 is not enough when the baseline already finds as many.
        let found_already = Baseline {
            talkers: 6,
            ..baseline()
        };
        assert!(!conditions(&ok, &found_already)[0]);
        fails(
            ShortPoint {
                false_l_scenes: 2,
                ..ok.clone()
            },
            1,
        );
        fails(
            ShortPoint {
                false_new_s: 3,
                ..ok.clone()
            },
            2,
        );
        fails(
            ShortPoint {
                meeting_der: vec![0.2051, 0.30],
                ..ok.clone()
            },
            3,
        );
        // Within the slack passes.
        assert!(
            conditions(
                &ShortPoint {
                    meeting_der: vec![0.2049, 0.30],
                    ..ok.clone()
                },
                &base
            )[3]
        );
        fails(
            ShortPoint {
                s_der: 0.1001,
                ..ok.clone()
            },
            4,
        );
    }

    #[test]
    fn cluster_sweep_short_rule_breaks_ties_in_the_doc_order() {
        let base = baseline();
        let s = setting(30, 80, 8, false);
        let p = |f: fn(&mut ShortPoint)| {
            let mut x = passing(s);
            f(&mut x);
            x
        };
        // Most talkers first, even over fewer false speakers.
        let pts = [p(|x| x.false_new_all = 0), p(|x| x.talkers = 7)];
        assert_eq!(pick(&pts, &base), Some(1));
        // Then fewest false new speakers.
        let pts = [p(|x| x.false_new_all = 5), p(|x| x.false_new_all = 3)];
        assert_eq!(pick(&pts, &base), Some(1));
        // Then the lowest pooled AMI DER.
        let pts = [p(|x| x.a_der = 0.26), p(|x| x.a_der = 0.24)];
        assert_eq!(pick(&pts, &base), Some(1));
        // Then the highest floor, the lowest distance, the highest minimum, plain first.
        let pts = [
            passing(setting(20, 80, 8, false)),
            passing(setting(40, 80, 8, false)),
        ];
        assert_eq!(pick(&pts, &base), Some(1));
        let pts = [
            passing(setting(40, 90, 8, false)),
            passing(setting(40, 80, 8, false)),
        ];
        assert_eq!(pick(&pts, &base), Some(1));
        let pts = [
            passing(setting(40, 80, 4, false)),
            passing(setting(40, 80, 16, false)),
        ];
        assert_eq!(pick(&pts, &base), Some(1));
        let pts = [
            passing(setting(40, 80, 16, true)),
            passing(setting(40, 80, 16, false)),
        ];
        assert_eq!(pick(&pts, &base), Some(1));
        // A failing point never wins, however it ranks.
        let pts = [
            ShortPoint {
                talkers: 10,
                false_l_scenes: 5,
                ..passing(s)
            },
            passing(s),
        ];
        assert_eq!(pick(&pts, &base), Some(1));
    }

    #[test]
    fn cluster_sweep_short_grid_has_240_settings_in_order() {
        let g = grid();
        assert_eq!(g.len(), 240);
        assert_eq!(g[0], setting(0, 68, 4, false));
        assert_eq!(g[1], setting(0, 68, 4, true));
        assert_eq!(g[239], setting(100, 110, 16, true));
        for s in &g {
            assert!(s.regroup().validate().is_ok(), "{s:?}");
        }
    }

    fn turn(s: f64, e: f64, who: &str) -> Turn {
        Turn::new(s, e, who.to_owned())
    }

    #[test]
    fn cluster_sweep_short_measures_new_speakers_and_moved_speech() {
        use earshot_core::asr::MeetingTime;
        use earshot_core::diar::{DiarSpeaker, Embedding, EmbeddingModelId, SpeakerTurn};
        use std::time::Duration;
        let model = EmbeddingModelId::new("m@1");
        let speaker = |v: [f32; 2]| DiarSpeaker {
            centroid: Embedding::new(model.clone(), v.to_vec()),
            speech: Duration::from_secs(1),
        };
        let st = |speaker: usize, s: u64, e: u64| SpeakerTurn {
            speaker,
            start: MeetingTime::from_millis(s),
            end: MeetingTime::from_millis(e),
        };
        // Truth: A 0-10 s, T (the talker) 10-12 s, B 12-20 s. Baseline: S0 (A) 0-12, S1 (B)
        // 12-20. Regrouped: S0 (A) 0-10, S1 new 10-13, S2 (B) 13-20.
        let item = Item {
            name: "short-x".to_owned(),
            kind: "short",
            audio: Vec::new(),
            reference: vec![
                turn(0.0, 10.0, "A"),
                turn(10.0, 12.0, "T"),
                turn(12.0, 20.0, "B"),
            ],
            true_k: 3,
            uem: (0.0, 20.0),
        };
        let base = Diarization {
            speakers: vec![speaker([1.0, 0.0]), speaker([0.0, 1.0])],
            turns: vec![st(0, 0, 12_000), st(1, 12_000, 20_000)],
            unassigned: Vec::new(),
            new_speakers: Vec::new(),
            speaker_clusters: vec![3, 5],
            embedding_model: model.clone(),
        };
        // Matched by cluster, not centroid: B's centroid differs a little in this run.
        let at = Diarization {
            speakers: vec![
                speaker([1.0, 0.0]),
                speaker([0.6, 0.8]),
                speaker([0.0, 0.9]),
            ],
            turns: vec![
                st(0, 0, 10_000),
                st(1, 10_000, 13_000),
                st(2, 13_000, 20_000),
            ],
            unassigned: Vec::new(),
            new_speakers: vec![1],
            speaker_clusters: vec![3, 7, 5],
            embedding_model: model.clone(),
        };
        assert_eq!(
            old_speakers(&base, &at),
            vec![Kept::Base(0), Kept::New, Kept::Base(1)]
        );
        let m = measure(&item, &base, &at, Some("T"));
        assert!(m.talker_found);
        assert!(!measure(&item, &base, &base, Some("T")).talker_found);
        assert_eq!((m.new_speakers, m.false_new), (1, 0));
        // 10-12 moved from S0 to the talker's new speaker (right); 12-13 from B (wrong).
        assert!((m.moved_right_s - 2.0).abs() < 0.011, "{m:?}");
        assert!((m.moved_wrong_s - 1.0).abs() < 0.011, "{m:?}");
        assert_eq!((m.gained_s, m.unmatched), (0.0, None));
        // B holding 11-12 s, which S0 held in the baseline, is a gained second.
        let mut grabby = at.clone();
        grabby.turns = vec![
            st(0, 0, 10_000),
            st(1, 10_000, 11_000),
            st(2, 11_000, 20_000),
        ];
        let g = measure(&item, &base, &grabby, Some("T"));
        assert!((g.gained_s - 1.0).abs() < 0.011, "{g:?}");
        // A speaker from a cluster with no frame in the baseline gains everything it holds.
        let mut stranger = at.clone();
        stranger.speaker_clusters = vec![3, 7, 9];
        let x = measure(&item, &base, &stranger, Some("T"));
        assert_eq!(x.unmatched, Some(2));
        assert!((x.gained_s - 7.0).abs() < 0.011, "{x:?}");
        // Not a short scene: every new speaker is false.
        let m = measure(&item, &base, &at, None);
        assert_eq!((m.new_speakers, m.false_new), (1, 1));
        assert!(!m.talker_found);
    }
}
