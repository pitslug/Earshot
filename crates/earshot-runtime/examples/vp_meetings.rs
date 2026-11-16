//! Voiceprint matches on meeting audio (T12.67). Counts and scores only.
//!
//! ```text
//! cargo run --release -p earshot-runtime --example vp_meetings -- <repo-root> \
//!     --models <dir> [--out <file.json>]
//! ```
//!
//! The data, measures, candidates and rule are `docs/voiceprint-meetings.md`, committed before
//! this tool was written and before any run. Dev data only: AMI dev series meetings of fixture
//! set `ami-dev` (enrolment from each series' first meeting, trials on its second and third).
//! Nothing here changes a shipped threshold. No word text, name or speaker identity is printed
//! or written: speakers are compared by identity inside the tool only.

#[path = "cluster_sweep_parts/ami.rs"]
mod ami;

use std::collections::BTreeMap;
use std::path::PathBuf;

use earshot_core::asr::{CancelToken, MeetingTime};
use earshot_core::audio::{MonoBuffer, PIPELINE_RATE};
use earshot_core::diar::{Diarization, Diarizer, Embedding, SpeakerEmbedder};
use earshot_core::identity::{DeviceId, EntityId, Hlc, ManualClock};
use earshot_core::metrics::der::Turn;
use earshot_core::voiceprint::{
    InMemoryVoiceprintRepo, Person, Stamp, VoiceprintRepo, enroll, match_embedding,
};
use earshot_runtime::backends::{Accelerator, ModelDir, init_onnx, load_diarisation};
use earshot_runtime::post::PostConfig;
use rand_chacha::ChaCha8Rng;
use rand_core::SeedableRng;
use serde_json::{Value, json};

/// The AMI dev series.
const SERIES: [&str; 5] = ["ES2003", "ES2011", "IS1008", "TS3004", "TS3006"];
/// Enrolment meeting letter.
const ENROL: &str = "a";
/// Test meeting letters.
const TESTS: [&str; 2] = ["b", "c"];
/// The rule's false-accept requirement (FA and W).
const FA_LIMIT: f64 = 0.05;
/// The rule's miss cost.
const MISS_COST: f64 = 0.10;

fn fail(msg: impl std::fmt::Display) -> ! {
    eprintln!("vp_meetings: {msg}");
    std::process::exit(1);
}

// ---- who is who ----------------------------------------------------------------------------

fn attr<'a>(tag: &'a str, name: &str) -> Option<&'a str> {
    let key = format!(" {name}=\"");
    let at = tag.find(&key)? + key.len();
    let rest = &tag[at..];
    Some(&rest[..rest.find('"')?])
}

/// `nxt_agent` letter -> `global_name` for `meeting`, from `corpusResources/meetings.xml`.
fn identities(meetings_xml: &str, meeting: &str) -> BTreeMap<String, String> {
    let key = format!("observation=\"{meeting}\"");
    let Some(at) = meetings_xml.find(&key) else {
        fail(format!("{meeting} is not in meetings.xml"));
    };
    let block = &meetings_xml[at..];
    let block = &block[..block.find("</meeting>").unwrap_or(block.len())];
    let mut out = BTreeMap::new();
    let mut rest = block;
    while let Some(i) = rest.find("<speaker ") {
        rest = &rest[i..];
        let close = rest.find('>').unwrap_or(rest.len());
        let tag = &rest[..close];
        if let (Some(l), Some(g)) = (attr(tag, "nxt_agent"), attr(tag, "global_name")) {
            out.insert(l.to_owned(), g.to_owned());
        }
        rest = &rest[close.min(rest.len())..];
        if rest.len() <= 1 {
            break;
        }
        rest = &rest[1..];
    }
    out
}

// ---- owners and homes ----------------------------------------------------------------------

/// `overlap[t][f]`: seconds true speaker `truth[t]` and found speaker `found[f]` overlap inside
/// `uem` (no collar).
fn overlaps(
    reference: &[Turn],
    hyp: &[Turn],
    uem: (f64, f64),
) -> (Vec<String>, Vec<String>, Vec<Vec<f64>>) {
    let clip = |t: &Turn| {
        let (s, e) = (t.start.max(uem.0), t.end.min(uem.1));
        (e > s).then_some((s, e))
    };
    let mut truth: Vec<String> = reference.iter().map(|t| t.speaker.clone()).collect();
    truth.sort();
    truth.dedup();
    let mut found: Vec<String> = hyp.iter().map(|t| t.speaker.clone()).collect();
    found.sort();
    found.dedup();
    let mut m = vec![vec![0.0; found.len()]; truth.len()];
    for r in reference {
        let Some((rs, re)) = clip(r) else { continue };
        let ti = truth
            .binary_search(&r.speaker)
            .unwrap_or_else(|_| unreachable!());
        for h in hyp {
            let Some((hs, he)) = clip(h) else { continue };
            let o = re.min(he) - rs.max(hs);
            if o > 0.0 {
                let fi = found
                    .binary_search(&h.speaker)
                    .unwrap_or_else(|_| unreachable!());
                m[ti][fi] += o;
            }
        }
    }
    (truth, found, m)
}

/// Index of the largest positive value (ties to the lower index), if any.
fn argmax(v: impl Iterator<Item = f64>) -> Option<usize> {
    let mut best: Option<(usize, f64)> = None;
    for (i, x) in v.enumerate() {
        if x > 0.0 && best.is_none_or(|(_, b)| x > b) {
            best = Some((i, x));
        }
    }
    best.map(|(i, _)| i)
}

/// Per found speaker (`"S<k>"` order of `d.speakers`): its owner's letter, if any.
fn owners_of(d: &Diarization, reference: &[Turn], uem: (f64, f64)) -> Vec<Option<String>> {
    let hyp = d.der_turns();
    let (truth, found, m) = overlaps(reference, &hyp, uem);
    (0..d.speakers.len())
        .map(|k| {
            let f = found.iter().position(|x| *x == format!("S{k}"))?;
            argmax(m.iter().map(|row| row[f])).map(|t| truth[t].clone())
        })
        .collect()
}

/// Per true speaker letter: its home found speaker index.
fn homes_of(d: &Diarization, reference: &[Turn], uem: (f64, f64)) -> BTreeMap<String, usize> {
    let hyp = d.der_turns();
    let (truth, found, m) = overlaps(reference, &hyp, uem);
    let mut out = BTreeMap::new();
    for (t, row) in truth.iter().zip(&m) {
        if let Some(f) = argmax(row.iter().copied()) {
            let k: usize = found[f][1..].parse().unwrap_or_else(|_| unreachable!());
            out.insert(t.clone(), k);
        }
    }
    out
}

// ---- trials and rules ----------------------------------------------------------------------

/// One found speaker of a test meeting.
#[derive(Debug, Clone)]
struct Trial {
    meeting: usize,
    series: usize,
    /// Owner identity, if any.
    owner: Option<String>,
    centroid: Embedding,
    /// Score against every enrolled person: (identity, series, score).
    scores: Vec<(String, usize, f32)>,
}

/// Which condition: every person stored, or the test meeting's series removed.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Condition {
    Known,
    Unknown,
}

/// A naming rule.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Rule {
    /// Best person at or above the threshold (the shipped rule at 0.462).
    Threshold(f32),
    /// The shipped threshold and a margin over the runner-up.
    Margin(f32, f32),
    /// The shipped threshold, then two voices of one meeting named one person keep it only if
    /// they match each other.
    TwoVoices(f32),
}

/// The person each trial is named under `rule` and `cond` (identity), `None` if unnamed.
fn names(trials: &[Trial], rule: Rule, cond: Condition) -> Vec<Option<String>> {
    let pick = |t: &Trial| -> Option<(String, f32)> {
        let mut s: Vec<(String, f32)> = t
            .scores
            .iter()
            .filter(|(_, series, _)| cond == Condition::Known || *series != t.series)
            .map(|(p, _, s)| (p.clone(), *s))
            .collect();
        s.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
        let (best, score) = s.first()?.clone();
        let threshold = match rule {
            Rule::Threshold(t) | Rule::Margin(t, _) | Rule::TwoVoices(t) => t,
        };
        if score < threshold {
            return None;
        }
        if let Rule::Margin(_, m) = rule
            && s.get(1).is_some_and(|(_, second)| score - second < m)
        {
            return None;
        }
        Some((best, score))
    };
    let mut out: Vec<Option<(String, f32)>> = trials.iter().map(pick).collect();
    if let Rule::TwoVoices(threshold) = rule {
        let mut by: BTreeMap<(usize, String), Vec<usize>> = BTreeMap::new();
        for (i, n) in out.iter().enumerate() {
            if let Some((p, _)) = n {
                by.entry((trials[i].meeting, p.clone()))
                    .or_default()
                    .push(i);
            }
        }
        for idx in by.values().filter(|v| v.len() > 1) {
            let keeper = *idx
                .iter()
                .max_by(|&&a, &&b| {
                    let (sa, sb) = (
                        out[a].as_ref().map_or(0.0, |x| x.1),
                        out[b].as_ref().map_or(0.0, |x| x.1),
                    );
                    sa.total_cmp(&sb).then(b.cmp(&a))
                })
                .unwrap_or_else(|| unreachable!());
            for &i in idx {
                if i == keeper {
                    continue;
                }
                let like = trials[i]
                    .centroid
                    .cosine_similarity(&trials[keeper].centroid)
                    .unwrap_or(f32::NEG_INFINITY);
                if like < threshold {
                    out[i] = None;
                }
            }
        }
    }
    out.into_iter().map(|n| n.map(|(p, _)| p)).collect()
}

/// What one rule does in one condition.
#[derive(Debug, Clone, Default, PartialEq)]
struct Outcome {
    trials: usize,
    correct: usize,
    wrong: usize,
    miss: usize,
    /// Trials whose owner is enrolled.
    owner_enrolled: usize,
    split_events: usize,
    mixed_events: usize,
}

impl Outcome {
    fn rate(n: usize, d: usize) -> f64 {
        if d == 0 { 0.0 } else { n as f64 / d as f64 }
    }
    fn wrong_rate(&self) -> f64 {
        Self::rate(self.wrong, self.trials)
    }
    fn miss_rate(&self) -> f64 {
        Self::rate(self.miss, self.owner_enrolled)
    }
}

fn outcome(trials: &[Trial], named: &[Option<String>], enrolled: &[String]) -> Outcome {
    let mut o = Outcome {
        trials: trials.len(),
        ..Outcome::default()
    };
    for (t, n) in trials.iter().zip(named) {
        let owner_in = t.owner.as_ref().is_some_and(|w| enrolled.contains(w));
        if owner_in {
            o.owner_enrolled += 1;
        }
        match n {
            Some(p) if t.owner.as_ref() == Some(p) => o.correct += 1,
            Some(_) => o.wrong += 1,
            None if owner_in => o.miss += 1,
            None => {}
        }
    }
    let mut by: BTreeMap<(usize, &String), Vec<&Option<String>>> = BTreeMap::new();
    for (t, n) in trials.iter().zip(named) {
        if let Some(p) = n {
            by.entry((t.meeting, p)).or_default().push(&t.owner);
        }
    }
    for owners in by.values().filter(|v| v.len() > 1) {
        if owners[0].is_some() && owners.iter().all(|w| *w == owners[0]) {
            o.split_events += 1;
        } else {
            o.mixed_events += 1;
        }
    }
    o
}

/// A rule in both conditions: (known, unknown).
fn judge(trials: &[Trial], rule: Rule, enrolled: &[String]) -> (Outcome, Outcome) {
    let known = outcome(trials, &names(trials, rule, Condition::Known), enrolled);
    // In the unknown condition no owner is stored, so every name is a false accept.
    let unknown = outcome(trials, &names(trials, rule, Condition::Unknown), &[]);
    (known, unknown)
}

/// The false-accept requirement and the miss cost against the shipped rule.
fn meets(c: &(Outcome, Outcome), shipped: &(Outcome, Outcome)) -> bool {
    c.1.wrong_rate() <= FA_LIMIT
        && c.0.wrong_rate() <= FA_LIMIT
        && c.0.miss_rate() <= shipped.0.miss_rate() + MISS_COST
}

// ---- main ------------------------------------------------------------------------------------

fn fixtures_dir() -> PathBuf {
    if let Some(d) = std::env::var_os("EARSHOT_FIXTURES").filter(|v| !v.is_empty()) {
        return PathBuf::from(d);
    }
    PathBuf::from(std::env::var_os("LOCALAPPDATA").unwrap_or_else(|| fail("LOCALAPPDATA")))
        .join("earshot-dev")
        .join("fixtures")
}

fn outcome_json(o: &Outcome) -> Value {
    json!({
        "trials": o.trials,
        "correct": o.correct,
        "wrong": o.wrong,
        "miss": o.miss,
        "owner_enrolled": o.owner_enrolled,
        "wrong_rate": o.wrong_rate(),
        "miss_rate": o.miss_rate(),
        "split_events": o.split_events,
        "mixed_events": o.mixed_events,
    })
}

fn main() {
    let mut it = std::env::args().skip(1);
    let _root = PathBuf::from(
        it.next()
            .unwrap_or_else(|| fail("usage: vp_meetings <repo-root> --models <dir>")),
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
    let fixtures = fixtures_dir();
    let meetings_xml: String =
        std::fs::read(fixtures.join("ami/annotations/corpusResources/meetings.xml"))
            .unwrap_or_else(|e| fail(format!("meetings.xml: {e}")))
            .iter()
            .map(|&b| char::from(b))
            .collect();

    init_onnx(&models, Accelerator::Cpu).unwrap_or_else(|e| fail(format!("ONNX Runtime: {e}")));
    let (segmenter, embedder) =
        load_diarisation(&models, Accelerator::Cpu).unwrap_or_else(|e| fail(e));
    let model = embedder.info().model.clone();
    let post = PostConfig::default();
    let mut config = post.diarization.clone();
    config.clustering.threshold = post
        .clustering_threshold(&model)
        .unwrap_or_else(|e| fail(e));
    config.clustering.unassigned_floor = post.unassigned_floor(&model).unwrap_or_else(|e| fail(e));
    let shipped_threshold = post
        .voiceprint_threshold(&model)
        .unwrap_or_else(|e| fail(e));
    let mut diarizer = Diarizer::new(segmenter, embedder, config).unwrap_or_else(|e| fail(e));

    let mut diarize = |meeting: &str| -> (Diarization, ami::Reference, BTreeMap<String, String>) {
        eprintln!("vp_meetings: {meeting}");
        let reference = ami::reference(&fixtures, meeting).unwrap_or_else(|e| fail(e));
        let audio = ami::mix(&fixtures, meeting).unwrap_or_else(|e| fail(e));
        let d = diarizer
            .diarize(
                &MonoBuffer::new(PIPELINE_RATE, audio),
                MeetingTime::ZERO,
                &CancelToken::new(),
            )
            .unwrap_or_else(|e| fail(format!("{meeting}: {e}")));
        (d, reference, identities(&meetings_xml, meeting))
    };

    // Enrolment, as the app enrols one meeting's named voices.
    let clock = ManualClock::new(1_800_000_000_000);
    let mut rng = ChaCha8Rng::seed_from_u64(12_670);
    let mut hlc = Hlc::new(DeviceId::generate(&clock, &mut rng));
    let mut repo = InMemoryVoiceprintRepo::new();
    let mut persons: Vec<(String, usize, EntityId)> = Vec::new();
    let mut not_enrolled = 0usize;
    for (si, s) in SERIES.iter().enumerate() {
        let meeting = format!("{s}{ENROL}");
        let (d, r, who) = diarize(&meeting);
        let owners = owners_of(&d, &r.turns, r.uem);
        let homes = homes_of(&d, &r.turns, r.uem);
        for (letter, identity) in &who {
            let home = homes.get(letter).copied();
            let mutual = home.filter(|&k| owners[k].as_deref() == Some(letter.as_str()));
            let Some(k) = mutual else {
                not_enrolled += 1;
                continue;
            };
            let mut stamp = Stamp {
                clock: &clock,
                hlc: &mut hlc,
                rng: &mut rng,
            };
            let p = Person::new(format!("person-{}", persons.len()), false, &mut stamp);
            repo.put_person(p.clone()).unwrap_or_else(|e| fail(e));
            match enroll(
                &mut repo,
                p.id,
                std::slice::from_ref(&d.speakers[k].centroid),
                d.speakers[k].speech,
                &post.enrolment,
                &mut stamp,
            ) {
                Ok(_) => persons.push((identity.clone(), si, p.id)),
                Err(_) => not_enrolled += 1,
            }
        }
    }
    let enrolled: Vec<String> = persons.iter().map(|(g, _, _)| g.clone()).collect();

    // Trials.
    let mut trials: Vec<Trial> = Vec::new();
    let mut meeting_ids: Vec<String> = Vec::new();
    let mut reference_speakers = 0usize;
    for (si, s) in SERIES.iter().enumerate() {
        for l in TESTS {
            let meeting = format!("{s}{l}");
            let (d, r, who) = diarize(&meeting);
            reference_speakers += r.speakers;
            let owners = owners_of(&d, &r.turns, r.uem);
            let mi = meeting_ids.len();
            meeting_ids.push(meeting.clone());
            for (k, sp) in d.speakers.iter().enumerate() {
                let scores = persons
                    .iter()
                    .map(|(g, ps, id)| {
                        let v = repo
                            .voiceprints_of(*id)
                            .unwrap_or_else(|e| fail(e))
                            .into_iter()
                            .find(|v| !v.meta.deleted)
                            .unwrap_or_else(|| fail("an enrolled person has no voiceprint"));
                        (
                            g.clone(),
                            *ps,
                            v.score(&sp.centroid).unwrap_or_else(|e| fail(e)),
                        )
                    })
                    .collect();
                trials.push(Trial {
                    meeting: mi,
                    series: si,
                    owner: owners[k]
                        .as_ref()
                        .and_then(|letter| who.get(letter).cloned()),
                    centroid: sp.centroid.clone(),
                    scores,
                });
                // The tool's naming at the shipped threshold is the production match.
                let named = names(
                    std::slice::from_ref(trials.last().unwrap_or_else(|| unreachable!())),
                    Rule::Threshold(shipped_threshold),
                    Condition::Known,
                );
                let production = match_embedding(&repo, &sp.centroid, shipped_threshold)
                    .unwrap_or_else(|e| fail(e));
                let production_named = match production {
                    earshot_core::voiceprint::MatchOutcome::Known { person, .. } => persons
                        .iter()
                        .find(|(_, _, id)| *id == person)
                        .map(|(g, _, _)| g.clone()),
                    earshot_core::voiceprint::MatchOutcome::Unknown { .. } => None,
                };
                if named[0] != production_named {
                    fail(format!(
                        "{meeting}: the tool's naming differs from match_embedding"
                    ));
                }
            }
        }
    }

    let shipped = judge(&trials, Rule::Threshold(shipped_threshold), &enrolled);
    let shipped_meets = meets(&shipped, &shipped);
    // A.
    let mut grid_a: Vec<f32> = (0..=20).map(|i| 0.40 + 0.02 * i as f32).collect();
    grid_a.push(shipped_threshold);
    grid_a.sort_by(f32::total_cmp);
    let a: Vec<(f32, (Outcome, Outcome))> = grid_a
        .iter()
        .map(|&t| (t, judge(&trials, Rule::Threshold(t), &enrolled)))
        .collect();
    let pick_a = if shipped_meets {
        None
    } else {
        a.iter()
            .filter(|(t, c)| *t > shipped_threshold && meets(c, &shipped))
            .map(|(t, _)| *t)
            .next()
    };
    // B.
    let b: Vec<(f32, (Outcome, Outcome))> = (0..=10)
        .map(|i| 0.02 * i as f32)
        .map(|m| {
            (
                m,
                judge(&trials, Rule::Margin(shipped_threshold, m), &enrolled),
            )
        })
        .collect();
    let pick_b = if shipped_meets {
        None
    } else {
        b.iter()
            .filter(|(m, c)| *m > 0.0 && meets(c, &shipped))
            .map(|(m, _)| *m)
            .next()
    };
    // C.
    let c = judge(&trials, Rule::TwoVoices(shipped_threshold), &enrolled);
    let shipped_names = names(
        &trials,
        Rule::Threshold(shipped_threshold),
        Condition::Known,
    );
    let c_names = names(
        &trials,
        Rule::TwoVoices(shipped_threshold),
        Condition::Known,
    );
    let (mut right_removed, mut wrong_removed) = (0usize, 0usize);
    for ((t, before), after) in trials.iter().zip(&shipped_names).zip(&c_names) {
        if before.is_some() && after.is_none() {
            if before == &t.owner {
                right_removed += 1;
            } else {
                wrong_removed += 1;
            }
        }
    }
    let c_passes = shipped.0.mixed_events > 0
        && c.0.mixed_events < shipped.0.mixed_events
        && right_removed <= wrong_removed
        && c.0.miss_rate() <= shipped.0.miss_rate() + MISS_COST;

    println!(
        "persons {} (not enrolled {not_enrolled}), trials {}, shipped threshold {shipped_threshold}",
        persons.len(),
        trials.len()
    );
    let line = |label: String, o: &(Outcome, Outcome)| {
        println!(
            "{label}: FA {:.3} ({}/{}) | W {:.3} ({}) M {:.3} ({}/{}) correct {} | split {} mixed {}",
            o.1.wrong_rate(),
            o.1.wrong,
            o.1.trials,
            o.0.wrong_rate(),
            o.0.wrong,
            o.0.miss_rate(),
            o.0.miss,
            o.0.owner_enrolled,
            o.0.correct,
            o.0.split_events,
            o.0.mixed_events,
        );
    };
    line("shipped".into(), &shipped);
    for (t, o) in &a {
        line(format!("A t={t:.3}"), o);
    }
    for (m, o) in &b {
        line(format!("B m={m:.2}"), o);
    }
    line("C".into(), &c);
    println!(
        "rule: shipped meets the requirement {shipped_meets}; A picks {pick_a:?}; B picks {pick_b:?}; C passes {c_passes} (right removed {right_removed}, wrong removed {wrong_removed})"
    );

    let report = json!({
        "model": model.to_string(),
        "rule": "docs/voiceprint-meetings.md",
        "shipped_threshold": shipped_threshold,
        "enrolment_meetings": SERIES.iter().map(|s| format!("{s}{ENROL}")).collect::<Vec<_>>(),
        "test_meetings": meeting_ids,
        "persons": persons.len(),
        "not_enrolled": not_enrolled,
        "trials": trials.len(),
        "reference_speakers": reference_speakers,
        "shipped": { "known": outcome_json(&shipped.0), "unknown": outcome_json(&shipped.1) },
        "a": a.iter().map(|(t, o)| json!({
            "threshold": t, "known": outcome_json(&o.0), "unknown": outcome_json(&o.1),
        })).collect::<Vec<_>>(),
        "b": b.iter().map(|(m, o)| json!({
            "margin": m, "known": outcome_json(&o.0), "unknown": outcome_json(&o.1),
        })).collect::<Vec<_>>(),
        "c": { "known": outcome_json(&c.0), "unknown": outcome_json(&c.1),
               "right_removed": right_removed, "wrong_removed": wrong_removed },
        "verdict": {
            "shipped_meets_requirement": shipped_meets,
            "a_pick": pick_a,
            "b_pick": pick_b,
            "c_passes": c_passes,
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
    use earshot_core::diar::EmbeddingModelId;

    fn emb(v: &[f32]) -> Embedding {
        Embedding::new(EmbeddingModelId::new("fake@1"), v.to_vec())
    }

    fn trial(
        meeting: usize,
        series: usize,
        owner: Option<&str>,
        c: &[f32],
        scores: &[(&str, usize, f32)],
    ) -> Trial {
        Trial {
            meeting,
            series,
            owner: owner.map(str::to_owned),
            centroid: emb(c),
            scores: scores
                .iter()
                .map(|(p, s, x)| ((*p).to_owned(), *s, *x))
                .collect(),
        }
    }

    #[test]
    fn vp_meetings_threshold_and_margin_name_or_leave_a_voice() {
        let t = [trial(
            0,
            0,
            Some("A"),
            &[1.0, 0.0],
            &[("A", 0, 0.60), ("B", 1, 0.55)],
        )];
        assert_eq!(
            names(&t, Rule::Threshold(0.5), Condition::Known),
            [Some("A".to_owned())]
        );
        assert_eq!(names(&t, Rule::Threshold(0.7), Condition::Known), [None]);
        assert_eq!(
            names(&t, Rule::Margin(0.5, 0.04), Condition::Known),
            [Some("A".to_owned())]
        );
        assert_eq!(names(&t, Rule::Margin(0.5, 0.06), Condition::Known), [None]);
        // Unknown: the meeting's own series is removed, so B (another series) is a false accept.
        assert_eq!(
            names(&t, Rule::Threshold(0.5), Condition::Unknown),
            [Some("B".to_owned())]
        );
    }

    #[test]
    fn vp_meetings_two_voices_keep_a_split_and_drop_an_unlike_second() {
        // Two voices of one meeting both best match A. Unlike each other: the lower loses it.
        // Like each other (a split of one voice): both keep it.
        let unlike = [
            trial(0, 0, Some("A"), &[1.0, 0.0], &[("A", 0, 0.8)]),
            trial(0, 0, Some("X"), &[0.0, 1.0], &[("A", 0, 0.6)]),
        ];
        assert_eq!(
            names(&unlike, Rule::TwoVoices(0.5), Condition::Known),
            [Some("A".to_owned()), None]
        );
        let split = [
            trial(0, 0, Some("A"), &[1.0, 0.0], &[("A", 0, 0.8)]),
            trial(0, 0, Some("A"), &[0.9, 0.1], &[("A", 0, 0.6)]),
        ];
        assert_eq!(
            names(&split, Rule::TwoVoices(0.5), Condition::Known),
            [Some("A".to_owned()), Some("A".to_owned())]
        );
        // Another meeting is never compared.
        let apart = [
            trial(0, 0, Some("A"), &[1.0, 0.0], &[("A", 0, 0.8)]),
            trial(1, 0, Some("X"), &[0.0, 1.0], &[("A", 0, 0.6)]),
        ];
        assert_eq!(
            names(&apart, Rule::TwoVoices(0.5), Condition::Known),
            [Some("A".to_owned()), Some("A".to_owned())]
        );
    }

    #[test]
    fn vp_meetings_outcomes_count_correct_wrong_misses_and_events() {
        let t = [
            trial(0, 0, Some("A"), &[1.0, 0.0], &[]),
            trial(0, 0, Some("X"), &[0.0, 1.0], &[]),
            trial(0, 0, Some("B"), &[0.0, 1.0], &[]),
            trial(0, 0, None, &[0.0, 1.0], &[]),
        ];
        let named = [Some("A".to_owned()), Some("A".to_owned()), None, None];
        let o = outcome(&t, &named, &["A".to_owned(), "B".to_owned()]);
        assert_eq!((o.correct, o.wrong, o.miss, o.owner_enrolled), (1, 1, 1, 2));
        assert_eq!((o.split_events, o.mixed_events), (0, 1));
        let both_a = [Some("A".to_owned()), Some("A".to_owned())];
        let split = [
            trial(0, 0, Some("A"), &[1.0, 0.0], &[]),
            trial(0, 0, Some("A"), &[1.0, 0.0], &[]),
        ];
        let o = outcome(&split, &both_a, &["A".to_owned()]);
        assert_eq!((o.split_events, o.mixed_events), (1, 0));
    }

    #[test]
    fn vp_meetings_requirement_and_miss_cost() {
        let o = |wrong: usize, fa: usize, miss: usize| {
            (
                Outcome {
                    trials: 40,
                    wrong,
                    miss,
                    owner_enrolled: 40,
                    ..Outcome::default()
                },
                Outcome {
                    trials: 40,
                    wrong: fa,
                    ..Outcome::default()
                },
            )
        };
        let shipped = o(4, 4, 4);
        assert!(!meets(&shipped, &shipped), "W 0.10 fails");
        assert!(meets(&o(2, 2, 8), &shipped), "W, FA 0.05; M +0.10");
        assert!(!meets(&o(2, 2, 9), &shipped), "M +0.125");
        assert!(!meets(&o(2, 3, 4), &shipped), "FA 0.075");
    }

    #[test]
    fn vp_meetings_reads_who_is_who_from_meetings_xml() {
        let xml = r#"<meeting nite:id="m1" observation="XX1a" duration="9">
      <speaker nite:id="XX1a_1" channel="0" nxt_agent="A" global_name="G1" role="PM"/>
      <speaker nite:id="XX1a_2" channel="1" nxt_agent="B" global_name="G2" role="ID"/>
   </meeting>
<meeting nite:id="m2" observation="XX1b" duration="9">
      <speaker nite:id="XX1b_1" channel="0" nxt_agent="A" global_name="G2" role="PM"/>
   </meeting>"#;
        let a = identities(xml, "XX1a");
        assert_eq!(a.get("A").map(String::as_str), Some("G1"));
        assert_eq!(a.get("B").map(String::as_str), Some("G2"));
        assert_eq!(a.len(), 2);
        let b = identities(xml, "XX1b");
        assert_eq!(b.get("A").map(String::as_str), Some("G2"));
        assert_eq!(b.len(), 1);
    }
}
