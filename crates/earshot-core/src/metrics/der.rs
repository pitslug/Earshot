//! Diarisation error rate (DER).
//!
//! Follows the NIST RT / `md-eval` definition as implemented by `pyannote.metrics`
//! (`DiarizationErrorRate`, <https://github.com/pyannote/pyannote-metrics>), the
//! reference used by the pyannote AMI-diarization-setup (PLAN.md 4.0):
//!
//! - the scored region is the UEM (or, without one, the extent of reference and
//!   hypothesis), minus a collar around every reference turn boundary;
//! - overlapped speech is scored: in every elementary interval of duration `d` with
//!   `N_ref` active reference speakers and `N_hyp` active hypothesis speakers,
//!   `total += d N_ref`, `missed += d max(0, N_ref - N_hyp)`,
//!   `false alarm += d max(0, N_hyp - N_ref)`,
//!   `confusion += d (min(N_ref, N_hyp) - N_correct)`, where `N_correct` counts active
//!   reference speakers whose mapped hypothesis speaker is also active;
//! - the speaker mapping is the one-to-one mapping maximising total co-occurrence time,
//!   found with the Hungarian algorithm ([`super::hungarian`]).
//!
//! `DER = (missed + false alarm + confusion) / total`.

use super::hungarian::max_weight_assignment;

/// One speaker turn (seconds).
#[derive(Debug, Clone, PartialEq)]
pub struct Turn {
    /// Start time in seconds.
    pub start: f64,
    /// End time in seconds (exclusive).
    pub end: f64,
    /// Speaker label; labels are compared within one side only.
    pub speaker: String,
}

impl Turn {
    /// A turn from `start` to `end` seconds spoken by `speaker`.
    pub fn new(start: f64, end: f64, speaker: impl Into<String>) -> Self {
        Turn {
            start,
            end,
            speaker: speaker.into(),
        }
    }
}

/// DER scoring options. The fields are private so a collar can only be given through a
/// constructor that names its convention: [`DerConfig::nist`] (per side, `md-eval -c`) or
/// [`DerConfig::pyannote`] (total width).
#[derive(Debug, Clone, PartialEq)]
pub struct DerConfig {
    /// Collar in seconds, `pyannote.metrics` convention: the total width removed around
    /// each reference boundary, i.e. `collar / 2` on each side.
    collar: f64,
    /// Scored regions as `(start, end)` seconds. `None` scores the extent of reference
    /// and hypothesis together.
    uem: Option<Vec<(f64, f64)>>,
}

/// The NIST / `md-eval -c 0.25` forgiveness collar used by the AMI diarisation setup
/// (PLAN.md 4.0): 0.25 s removed on **each side** of every reference boundary. Pass it to
/// [`DerConfig::nist`].
pub const AMI_COLLAR_PER_SIDE_S: f64 = 0.25;

impl DerConfig {
    /// Collar given the NIST way, as the time removed on each side of every reference
    /// boundary (`md-eval -c`). Stores the total width `2 * per_side_s` (pyannote
    /// convention), so `nist(0.25)` scores like `md-eval -c 0.25` and like pyannote's
    /// `collar=0.5`. No UEM. `nist(0.0)` scores without a collar.
    #[must_use]
    pub const fn nist(per_side_s: f64) -> Self {
        DerConfig {
            collar: 2.0 * per_side_s,
            uem: None,
        }
    }

    /// Collar given the `pyannote.metrics` way, as the total width removed around each
    /// reference boundary (`collar=0.5` removes +/-0.25 s). No UEM.
    #[must_use]
    pub const fn pyannote(total_width_s: f64) -> Self {
        DerConfig {
            collar: total_width_s,
            uem: None,
        }
    }

    /// The same configuration restricted to the scored regions `uem` (`(start, end)` s).
    #[must_use]
    pub fn with_uem(mut self, uem: Vec<(f64, f64)>) -> Self {
        self.uem = Some(uem);
        self
    }

    /// The collar's total width in seconds (pyannote convention).
    #[must_use]
    pub fn collar_total_s(&self) -> f64 {
        self.collar
    }

    /// The scored regions, if restricted.
    #[must_use]
    pub fn uem(&self) -> Option<&[(f64, f64)]> {
        self.uem.as_deref()
    }
}

/// DER components, all in seconds.
#[derive(Debug, Clone, PartialEq)]
pub struct DerResult {
    /// Scored reference speech (overlap counted once per speaker).
    pub total: f64,
    /// Reference speech with no hypothesis speaker to cover it.
    pub missed: f64,
    /// Hypothesis speech beyond the number of reference speakers.
    pub false_alarm: f64,
    /// Speech attributed to the wrong speaker under the optimal mapping.
    pub confusion: f64,
    /// Reference label -> mapped hypothesis label (unmapped labels are absent).
    pub mapping: Vec<(String, String)>,
}

impl DerResult {
    /// `(missed + false_alarm + confusion) / total`. With no scored reference speech this
    /// is 0 when there are no errors and `+inf` otherwise.
    pub fn der(&self) -> f64 {
        let err = self.missed + self.false_alarm + self.confusion;
        if self.total > 0.0 {
            err / self.total
        } else if err > 0.0 {
            f64::INFINITY
        } else {
            0.0
        }
    }
}

/// Sorts and merges overlapping or touching intervals, dropping empty ones.
fn merge(mut iv: Vec<(f64, f64)>) -> Vec<(f64, f64)> {
    iv.retain(|(s, e)| e > s);
    iv.sort_by(|a, b| a.0.total_cmp(&b.0));
    let mut out: Vec<(f64, f64)> = Vec::with_capacity(iv.len());
    for (s, e) in iv {
        match out.last_mut() {
            Some(last) if s <= last.1 => last.1 = last.1.max(e),
            _ => out.push((s, e)),
        }
    }
    out
}

/// `a` minus `b`; both sorted and merged.
fn subtract(a: &[(f64, f64)], b: &[(f64, f64)]) -> Vec<(f64, f64)> {
    let mut out = Vec::new();
    for &(s, e) in a {
        let mut cur = s;
        for &(bs, be) in b {
            if be <= cur || bs >= e {
                continue;
            }
            if bs > cur {
                out.push((cur, bs));
            }
            cur = cur.max(be);
            if cur >= e {
                break;
            }
        }
        if cur < e {
            out.push((cur, e));
        }
    }
    out
}

/// Distinct labels of `turns` and, per turn, the label index.
fn labels(turns: &[Turn]) -> (Vec<String>, Vec<usize>) {
    let mut names: Vec<String> = Vec::new();
    let idx = turns
        .iter()
        .map(|t| match names.iter().position(|n| *n == t.speaker) {
            Some(i) => i,
            None => {
                names.push(t.speaker.clone());
                names.len() - 1
            }
        })
        .collect();
    (names, idx)
}

/// Distinct label indices of turns active at time `t`.
fn active(turns: &[Turn], idx: &[usize], t: f64) -> Vec<usize> {
    let mut a: Vec<usize> = turns
        .iter()
        .zip(idx)
        .filter(|(turn, _)| turn.start <= t && t < turn.end)
        .map(|(_, &i)| i)
        .collect();
    a.sort_unstable();
    a.dedup();
    a
}

/// Scores `hypothesis` turns against `reference` turns.
pub fn der(reference: &[Turn], hypothesis: &[Turn], cfg: &DerConfig) -> DerResult {
    // Scored region.
    let uem = match &cfg.uem {
        Some(u) => merge(u.clone()),
        None => {
            let all = reference.iter().chain(hypothesis);
            let lo = all.clone().map(|t| t.start).fold(f64::INFINITY, f64::min);
            let hi = all.map(|t| t.end).fold(f64::NEG_INFINITY, f64::max);
            if lo < hi { vec![(lo, hi)] } else { Vec::new() }
        }
    };
    let half = cfg.collar.max(0.0) / 2.0;
    let scored = if half > 0.0 {
        let collars = merge(
            reference
                .iter()
                .flat_map(|t| [t.start, t.end])
                .map(|b| (b - half, b + half))
                .collect(),
        );
        subtract(&uem, &collars)
    } else {
        uem
    };

    let (ref_names, ref_idx) = labels(reference);
    let (hyp_names, hyp_idx) = labels(hypothesis);

    // Elementary intervals inside the scored region.
    let mut points: Vec<f64> = reference
        .iter()
        .chain(hypothesis)
        .flat_map(|t| [t.start, t.end])
        .chain(scored.iter().flat_map(|&(s, e)| [s, e]))
        .collect();
    points.sort_by(f64::total_cmp);
    points.dedup();

    struct Piece {
        dur: f64,
        r: Vec<usize>,
        h: Vec<usize>,
    }
    let mut pieces = Vec::new();
    let mut cooc = vec![vec![0.0f64; hyp_names.len()]; ref_names.len()];
    for w in points.windows(2) {
        let (a, b) = (w[0], w[1]);
        let mid = 0.5 * (a + b);
        if !scored.iter().any(|&(s, e)| s <= mid && mid < e) {
            continue;
        }
        let r = active(reference, &ref_idx, mid);
        let h = active(hypothesis, &hyp_idx, mid);
        let dur = b - a;
        for &ri in &r {
            for &hi in &h {
                cooc[ri][hi] += dur;
            }
        }
        pieces.push(Piece { dur, r, h });
    }

    let map = max_weight_assignment(&cooc);
    let mut res = DerResult {
        total: 0.0,
        missed: 0.0,
        false_alarm: 0.0,
        confusion: 0.0,
        mapping: map
            .iter()
            .enumerate()
            .filter_map(|(ri, h)| h.map(|hi| (ref_names[ri].clone(), hyp_names[hi].clone())))
            .collect(),
    };
    for p in &pieces {
        let (nr, nh) = (p.r.len(), p.h.len());
        let correct =
            p.r.iter()
                .filter(|&&ri| map[ri].is_some_and(|hi| p.h.contains(&hi)))
                .count();
        res.total += p.dur * nr as f64;
        res.missed += p.dur * nr.saturating_sub(nh) as f64;
        res.false_alarm += p.dur * nh.saturating_sub(nr) as f64;
        res.confusion += p.dur * (nr.min(nh) - correct) as f64;
    }
    res
}
