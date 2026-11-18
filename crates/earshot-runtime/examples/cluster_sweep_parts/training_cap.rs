//! Every embedding in the speaker grouping, with and without T12.66's regrouping (T12.85), for
//! `cluster_sweep --training-cap`: the data, candidates, measures and rule are
//! `docs/far-speakers/training-cap.md`, committed before this module was written. Counts and
//! times only.

use std::path::Path;

use earshot_core::audio::{MonoBuffer, PIPELINE_RATE};
use earshot_core::diar::{Diarization, DiarizationConfig, Regroup};
use earshot_core::metrics::der::Turn;
use serde_json::json;

use super::short_turns::ShortItems;
use super::{
    Embedder, HELD, Item, Score, Segmenter, clip, diarize_at, fail, floors, pooled, round4, score,
};

/// The fifteen AMI dev meetings: every meeting of each dev series.
pub const AMI_DEV_ALL: [&str; 15] = [
    "ES2003a", "ES2003b", "ES2003c", "ES2011a", "ES2011b", "ES2011c", "IS1008a", "IS1008b",
    "IS1008c", "TS3004a", "TS3004b", "TS3004c", "TS3006a", "TS3006b", "TS3006c",
];
/// The rule's weight of a merge or a lost speaker, in extra voices.
pub const MERGE_WEIGHT: usize = 4;
/// Rule 3: how much worse one AMI dev meeting's DER may get.
pub const MEETING_DER_SLACK: f64 = 0.02;
/// The baseline's training cap.
pub const BASELINE_CAP: usize = 1000;

/// One candidate of the rule doc.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Candidate {
    /// Today's pipeline.
    B,
    /// Every embedding trains.
    C,
    /// T12.66's regrouping.
    R,
    /// Both.
    Cr,
}

/// The candidates in the rule's tie-break order (B first, as the reference).
pub const CANDIDATES: [Candidate; 4] = [Candidate::B, Candidate::R, Candidate::C, Candidate::Cr];

impl Candidate {
    pub fn name(self) -> &'static str {
        match self {
            Candidate::B => "B",
            Candidate::C => "C",
            Candidate::R => "R",
            Candidate::Cr => "CR",
        }
    }

    fn cap(self) -> usize {
        match self {
            Candidate::B | Candidate::R => BASELINE_CAP,
            Candidate::C | Candidate::Cr => usize::MAX,
        }
    }

    fn regroup(self) -> Option<Regroup> {
        match self {
            Candidate::B | Candidate::C => None,
            // T12.66's pick: f 0.60, d 0.80, m 8 s, plain.
            Candidate::R | Candidate::Cr => Some(Regroup {
                floor: 0.60,
                distance: 0.80,
                min_speech_s: 8.0,
                weighted: false,
            }),
        }
    }

    /// The diarisation settings of this candidate over `config`.
    pub fn config(self, config: &DiarizationConfig) -> DiarizationConfig {
        let mut c = config.clone();
        c.clustering.max_train_embeddings = self.cap();
        c.clustering.regroup = self.regroup();
        c
    }
}

/// The rule doc's cost of one item: merges and lost speakers weigh `MERGE_WEIGHT` extra voices.
pub fn cost(s: &Score) -> usize {
    MERGE_WEIGHT * (s.merges + s.lost) + s.extras
}

/// Whether some found speaker of `hyp` is owned by `talker`: the true speaker whose reference
/// speech it overlaps the longest inside `uem` (ties to the lower label), T12.66's measure.
pub fn talker_found(reference: &[Turn], hyp: &[Turn], uem: (f64, f64), talker: &str) -> bool {
    let mut found: Vec<&str> = hyp.iter().map(|t| t.speaker.as_str()).collect();
    found.sort_unstable();
    found.dedup();
    found.iter().any(|f| {
        let mut by: Vec<(&str, f64)> = Vec::new();
        for h in hyp.iter().filter(|h| h.speaker == *f) {
            let Some((hs, he)) = clip(h, uem) else {
                continue;
            };
            for r in reference {
                let Some((rs, re)) = clip(r, uem) else {
                    continue;
                };
                let o = re.min(he) - rs.max(hs);
                if o > 0.0 {
                    match by.iter_mut().find(|(n, _)| *n == r.speaker) {
                        Some((_, t)) => *t += o,
                        None => by.push((r.speaker.as_str(), o)),
                    }
                }
            }
        }
        by.sort_by(|a, b| a.0.cmp(b.0));
        let owner = by
            .iter()
            .fold(None::<(&str, f64)>, |best, &(n, o)| match best {
                Some((_, bo)) if bo >= o => best,
                _ => Some((n, o)),
            });
        owner.is_some_and(|(n, _)| n == talker)
    })
}

/// One candidate over every item.
#[derive(Debug, Clone)]
pub struct CapPoint {
    pub candidate: Candidate,
    pub cost: usize,
    /// Merges + lost per item.
    pub merges: Vec<usize>,
    /// DER per item.
    pub der: Vec<f64>,
    pub talkers: usize,
    /// Merges + lost summed over the AMI meetings.
    pub a_merges: usize,
    pub a_der: f64,
}

/// The rule's four conditions for `p` against the baseline `b` (`n_ami` AMI meetings first).
pub fn conditions(p: &CapPoint, b: &CapPoint, n_ami: usize) -> [bool; 4] {
    [
        p.cost < b.cost,
        (0..n_ami).all(|i| p.merges[i] <= b.merges[i]),
        (0..n_ami).all(|i| p.der[i] <= b.der[i] + MEETING_DER_SLACK),
        p.talkers >= b.talkers && (n_ami..p.merges.len()).all(|i| p.merges[i] <= b.merges[i]),
    ]
}

/// The rule's pick among the passing candidates (`points[0]` is the baseline; the rest in
/// tie-break order), or `None`.
pub fn pick(points: &[CapPoint], n_ami: usize) -> Option<Candidate> {
    let b = &points[0];
    points[1..]
        .iter()
        .enumerate()
        .filter(|(_, p)| conditions(p, b, n_ami).iter().all(|c| *c))
        .min_by(|(i, x), (j, y)| {
            x.cost
                .cmp(&y.cost)
                .then(x.a_merges.cmp(&y.a_merges))
                .then(round4(x.a_der).total_cmp(&round4(y.a_der)))
                .then(i.cmp(j))
        })
        .map(|(_, p)| p.candidate)
}

fn diarize_cand(
    parts: (Segmenter, Embedder),
    config: &DiarizationConfig,
    c: Candidate,
    audio: &MonoBuffer,
) -> (Diarization, (Segmenter, Embedder)) {
    diarize_at(parts, &c.config(config), HELD, floors::OFF, audio)
}

/// `--training-cap`: `items` are the fifteen AMI dev meetings then the long scenes; the short
/// scenes are added after them.
pub fn run(
    mut items: Vec<Item>,
    short: ShortItems,
    mut parts: (Segmenter, Embedder),
    config: &DiarizationConfig,
    model: &str,
    out: Option<&Path>,
) {
    let n_ami = AMI_DEV_ALL.len();
    let n_long = items.len();
    if config.clustering.max_train_embeddings != BASELINE_CAP || config.clustering.regroup.is_some()
    {
        fail("the shipped clustering is not the baseline this rule was written for");
    }
    items.extend(short.items);
    // rows[item][candidate]
    let mut rows: Vec<Vec<(Score, bool)>> = Vec::new();
    let mut embeddings: Vec<usize> = Vec::new();
    for (i, item) in items.iter().enumerate() {
        eprintln!("cluster_sweep --training-cap: {}", item.name);
        let buffer = MonoBuffer::new(PIPELINE_RATE, item.audio.clone());
        parts.0.replay = false;
        parts.1.replay = false;
        parts.1.outputs = 0;
        let (direct, p) = diarize_cand(parts, config, Candidate::B, &buffer);
        parts = p;
        embeddings.push(parts.1.outputs);
        parts.0.replay = true;
        parts.1.replay = true;
        let talker = (i >= n_long).then(|| short.talkers[i - n_long].as_str());
        let mut row = Vec::new();
        for c in CANDIDATES {
            let (d, p) = diarize_cand(parts, config, c, &buffer);
            parts = p;
            if c == Candidate::B && d != direct {
                fail(format!(
                    "{}: the replayed baseline differs from the direct run",
                    item.name
                ));
            }
            let found =
                talker.is_some_and(|t| talker_found(&item.reference, &d.der_turns(), item.uem, t));
            row.push((score(item, &d), found));
        }
        parts.0.log.clear();
        parts.1.log.clear();
        rows.push(row);
    }

    let points: Vec<CapPoint> = CANDIDATES
        .iter()
        .enumerate()
        .map(|(g, &c)| {
            let col: Vec<&Score> = rows.iter().map(|r| &r[g].0).collect();
            CapPoint {
                candidate: c,
                cost: col.iter().map(|s| cost(s)).sum(),
                merges: col.iter().map(|s| s.merges + s.lost).collect(),
                der: col.iter().map(|s| s.der.der()).collect(),
                talkers: rows[n_long..].iter().filter(|r| r[g].1).count(),
                a_merges: col[..n_ami].iter().map(|s| s.merges + s.lost).sum(),
                a_der: pooled(&col[..n_ami]),
            }
        })
        .collect();
    let picked = pick(&points, n_ami);

    println!(
        "embeddings per item: {}",
        embeddings
            .iter()
            .map(|e| e.to_string())
            .collect::<Vec<_>>()
            .join(",")
    );
    println!(
        "cand | cost | A merges+lost extras DER | L merges+lost extras | S talkers DER | conditions"
    );
    for (g, p) in points.iter().enumerate() {
        let col: Vec<&Score> = rows.iter().map(|r| &r[g].0).collect();
        let sum = |r: std::ops::Range<usize>, f: fn(&Score) -> usize| -> usize {
            col[r].iter().map(|s| f(s)).sum()
        };
        println!(
            "{} | {} | {} {} {:.4} | {} {} | {} {:.4} | {:?}",
            p.candidate.name(),
            p.cost,
            p.a_merges,
            sum(0..n_ami, |s| s.extras),
            p.a_der,
            sum(n_ami..n_long, |s| s.merges + s.lost),
            sum(n_ami..n_long, |s| s.extras),
            p.talkers,
            pooled(&col[n_long..]),
            conditions(p, &points[0], n_ami),
        );
    }
    for (i, item) in items.iter().enumerate().take(n_ami) {
        let cells: Vec<String> = rows[i]
            .iter()
            .map(|(s, _)| {
                format!(
                    "found {} m{} l{} x{} der {:.4}",
                    s.found,
                    s.merges,
                    s.lost,
                    s.extras,
                    s.der.der()
                )
            })
            .collect();
        println!("  {} | {}", item.name, cells.join(" | "));
    }
    println!("rule: picks {}", picked.map_or("nothing", Candidate::name));

    let report = json!({
        "model": model,
        "rule": "docs/far-speakers/training-cap.md",
        "merge_weight": MERGE_WEIGHT,
        "embeddings": items.iter().zip(&embeddings).map(|(i, e)| json!({
            "name": i.name, "kind": i.kind, "embeddings": e,
        })).collect::<Vec<_>>(),
        "candidates": points.iter().enumerate().map(|(g, p)| json!({
            "candidate": p.candidate.name(),
            "cost": p.cost,
            "a_merges_lost": p.a_merges,
            "a_der": round4(p.a_der),
            "talkers": p.talkers,
            "conditions": conditions(p, &points[0], n_ami),
            "items": items.iter().zip(&rows).map(|(i, r)| {
                let (s, found) = &r[g];
                json!({
                    "name": i.name,
                    "kind": i.kind,
                    "true": i.true_k,
                    "found": s.found,
                    "merges": s.merges,
                    "lost": s.lost,
                    "extras": s.extras,
                    "extra_speech_s": round4(s.extra_speech_s),
                    "der": round4(s.der.der()),
                    "talker_found": found,
                })
            }).collect::<Vec<_>>(),
        })).collect::<Vec<_>>(),
        "pick": picked.map(Candidate::name),
    });
    let text = serde_json::to_string_pretty(&report).unwrap_or_else(|e| fail(e));
    if let Some(out) = out {
        std::fs::write(out, &text).unwrap_or_else(|e| fail(format!("{}: {e}", out.display())));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn point(
        c: Candidate,
        cost: usize,
        merges: Vec<usize>,
        der: Vec<f64>,
        talkers: usize,
    ) -> CapPoint {
        CapPoint {
            candidate: c,
            cost,
            a_merges: merges[..2].iter().sum(),
            merges,
            der,
            talkers,
            a_der: 0.1,
        }
    }

    #[test]
    fn cluster_sweep_training_cap_rule_needs_every_condition() {
        // Two AMI meetings, one scene.
        let b = point(Candidate::B, 10, vec![1, 0, 0], vec![0.10, 0.10, 0.2], 5);
        let ok = point(Candidate::R, 8, vec![1, 0, 0], vec![0.11, 0.12, 0.2], 5);
        assert_eq!(conditions(&ok, &b, 2), [true; 4]);
        let dearer = point(Candidate::R, 10, vec![1, 0, 0], vec![0.1, 0.1, 0.2], 5);
        assert!(!conditions(&dearer, &b, 2)[0]);
        let merges_one = point(Candidate::C, 5, vec![0, 1, 0], vec![0.1, 0.1, 0.2], 5);
        assert!(
            !conditions(&merges_one, &b, 2)[1],
            "a meeting gains a merge"
        );
        let worse = point(Candidate::C, 5, vec![1, 0, 0], vec![0.1, 0.121, 0.2], 5);
        assert!(!conditions(&worse, &b, 2)[2]);
        let fewer = point(Candidate::Cr, 5, vec![1, 0, 0], vec![0.1, 0.1, 0.2], 4);
        assert!(!conditions(&fewer, &b, 2)[3]);
        let scene_merge = point(Candidate::Cr, 5, vec![1, 0, 1], vec![0.1, 0.1, 0.2], 5);
        assert!(!conditions(&scene_merge, &b, 2)[3]);
        assert_eq!(pick(&[b.clone(), dearer, merges_one], 2), None);
        // Lowest cost wins; on a tie R comes before C.
        let c_same = point(Candidate::C, 8, vec![1, 0, 0], vec![0.1, 0.1, 0.2], 5);
        let cr_cheaper = point(Candidate::Cr, 7, vec![1, 0, 0], vec![0.1, 0.1, 0.2], 5);
        assert_eq!(
            pick(&[b.clone(), ok.clone(), c_same.clone()], 2),
            Some(Candidate::R)
        );
        assert_eq!(pick(&[b, ok, c_same, cr_cheaper], 2), Some(Candidate::Cr));
    }

    #[test]
    fn cluster_sweep_training_cap_cost_weighs_a_merge_as_four_extras() {
        let s = Score {
            der: earshot_core::metrics::der::DerResult {
                total: 1.0,
                missed: 0.0,
                false_alarm: 0.0,
                confusion: 0.0,
                mapping: Vec::new(),
            },
            found: 5,
            merges: 1,
            extras: 2,
            extra_speech_s: 3.0,
            lost: 1,
        };
        assert_eq!(cost(&s), 4 * 2 + 2);
    }

    #[test]
    fn cluster_sweep_training_cap_finds_a_talker_by_its_owner() {
        let r = vec![
            Turn::new(0.0, 10.0, "A".to_owned()),
            Turn::new(10.0, 12.0, "T".to_owned()),
        ];
        let merged = vec![Turn::new(0.0, 12.0, "S0".to_owned())];
        assert!(!talker_found(&r, &merged, (0.0, 12.0), "T"));
        let apart = vec![
            Turn::new(0.0, 10.0, "S0".to_owned()),
            Turn::new(10.0, 12.0, "S1".to_owned()),
        ];
        assert!(talker_found(&r, &apart, (0.0, 12.0), "T"));
    }

    #[test]
    fn cluster_sweep_training_cap_candidates_set_only_their_own_changes() {
        let base = DiarizationConfig::default();
        assert_eq!(
            Candidate::B.config(&base).clustering.max_train_embeddings,
            BASELINE_CAP
        );
        assert!(Candidate::B.config(&base).clustering.regroup.is_none());
        assert_eq!(
            Candidate::C.config(&base).clustering.max_train_embeddings,
            usize::MAX
        );
        assert!(Candidate::C.config(&base).clustering.regroup.is_none());
        let r = Candidate::R.config(&base).clustering;
        assert_eq!(r.max_train_embeddings, BASELINE_CAP);
        assert_eq!(
            r.regroup
                .map(|g| (g.floor, g.distance, g.min_speech_s, g.weighted)),
            Some((0.60, 0.80, 8.0, false))
        );
        assert_eq!(
            Candidate::Cr.config(&base).clustering.max_train_embeddings,
            usize::MAX
        );
        assert!(Candidate::Cr.config(&base).clustering.regroup.is_some());
    }
}
