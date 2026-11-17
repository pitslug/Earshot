//! The unassigned-floor measures and rule of `docs/far-speakers/unassigned-floor.md`
//! (T12.65), for `cluster_sweep --floors`. Counts and times only.

use earshot_core::diar::Diarization;
use earshot_core::metrics::der::Turn;

/// Floors tried after -1 (today), in hundredths: 0.00, 0.05, ..., 0.60.
pub const FLOOR_GRID: [i32; 13] = [0, 5, 10, 15, 20, 25, 30, 35, 40, 45, 50, 55, 60];
/// Today's floor, in hundredths.
pub const OFF: i32 = -100;
/// Frame of the moved-speech measure (s).
pub const FRAME_S: f64 = 0.01;
/// Rule 2: loss at most this share of the gain.
pub const LOSS_PER_GAIN: f64 = 0.25;
/// Rule 3: loss at most this share of all reference speech.
pub const LOSS_SHARE: f64 = 0.005;
/// Rule 4: how much worse one AMI dev meeting's DER may get.
pub const MEETING_DER_SLACK: f64 = 0.005;

pub fn floor(f: i32) -> f32 {
    f as f32 / 100.0
}

/// Frames moved to unassigned at a floor, against the same item at -1.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Moved {
    /// Seconds of right speech taken away.
    pub loss_s: f64,
    /// Seconds of wrong speech taken away.
    pub gain_s: f64,
    /// Seconds of non-speech taken away.
    pub non_speech_s: f64,
    /// All unassigned speech inside the scored region (s).
    pub unassigned_s: f64,
}

pub fn frames(uem: (f64, f64)) -> usize {
    ((uem.1 - uem.0) / FRAME_S).ceil().max(0.0) as usize
}

/// Frame `i` of the scored region covers `[uem.0 + i FRAME_S, ...)`; its centre decides.
pub fn centre(uem: (f64, f64), i: usize) -> f64 {
    uem.0 + (i as f64 + 0.5) * FRAME_S
}

/// For each frame, the labels of the turns covering its centre.
pub fn paint(turns: &[Turn], uem: (f64, f64)) -> Vec<Vec<String>> {
    let n = frames(uem);
    let mut out: Vec<Vec<String>> = vec![Vec::new(); n];
    for t in turns {
        let lo = ((t.start - uem.0) / FRAME_S - 0.5).ceil().max(0.0) as usize;
        let mut i = lo;
        while i < n && centre(uem, i) < t.end {
            if centre(uem, i) >= t.start && !out[i].contains(&t.speaker) {
                out[i].push(t.speaker.clone());
            }
            i += 1;
        }
    }
    out
}

/// Owners: per found speaker, the true speaker whose reference speech it overlaps the longest
/// inside `uem` (ties to the lower label in sort order); a found speaker overlapping no
/// reference speech has none.
pub fn owners(reference: &[Turn], hyp: &[Turn], uem: (f64, f64)) -> Vec<(String, String)> {
    let clip = |t: &Turn| {
        let (s, e) = (t.start.max(uem.0), t.end.min(uem.1));
        (e > s).then_some((s, e))
    };
    let mut truth: Vec<&str> = reference.iter().map(|t| t.speaker.as_str()).collect();
    truth.sort_unstable();
    truth.dedup();
    let mut found: Vec<&str> = hyp.iter().map(|t| t.speaker.as_str()).collect();
    found.sort_unstable();
    found.dedup();
    let mut out = Vec::new();
    for f in found {
        let mut best: Option<(&str, f64)> = None;
        for who in &truth {
            let mut o = 0.0;
            for r in reference.iter().filter(|r| r.speaker == *who) {
                let Some((rs, re)) = clip(r) else { continue };
                for h in hyp.iter().filter(|h| h.speaker == f) {
                    let Some((hs, he)) = clip(h) else { continue };
                    o += (re.min(he) - rs.max(hs)).max(0.0);
                }
            }
            if o > 0.0 && best.is_none_or(|(_, bo)| o > bo) {
                best = Some((who, o));
            }
        }
        if let Some((who, _)) = best {
            out.push((f.to_owned(), who.to_owned()));
        }
    }
    out
}
/// The unassigned spans of `d` as turns labelled `"unassigned"`.
pub fn unassigned_turns(d: &Diarization) -> Vec<Turn> {
    d.unassigned
        .iter()
        .map(|s| {
            Turn::new(
                s.start.as_millis() as f64 / 1000.0,
                s.end.as_millis() as f64 / 1000.0,
                "unassigned".to_owned(),
            )
        })
        .collect()
}

/// The moved-speech measures of the rule doc: `base` (floor -1) and `at` (floor f) turns,
/// `unassigned` the turns left unassigned at f.
pub fn moved(reference: &[Turn], base: &[Turn], unassigned: &[Turn], uem: (f64, f64)) -> Moved {
    let owner = owners(reference, base, uem);
    let refs = paint(reference, uem);
    let was = paint(base, uem);
    let left = paint(unassigned, uem);
    let mut m = Moved::default();
    for i in 0..frames(uem) {
        if left[i].is_empty() {
            continue;
        }
        m.unassigned_s += FRAME_S;
        if was[i].is_empty() {
            continue;
        }
        if refs[i].is_empty() {
            m.non_speech_s += FRAME_S;
            continue;
        }
        let right = refs[i]
            .iter()
            .any(|r| owner.iter().any(|(f, t)| t == r && was[i].contains(f)));
        if right {
            m.loss_s += FRAME_S;
        } else {
            m.gain_s += FRAME_S;
        }
    }
    m
}

/// One floor pooled over every item.
#[derive(Debug, Clone, PartialEq)]
pub struct FloorPoint {
    /// Floor in hundredths.
    pub f: i32,
    pub gain_s: f64,
    pub loss_s: f64,
    /// Each AMI dev meeting's DER.
    pub meeting_der: Vec<f64>,
}

/// Which rule conditions a floor meets: (gain > 0, loss <= share of gain, loss <= share of
/// speech, no meeting much worse).
pub fn conditions(p: &FloorPoint, off: &FloorPoint, reference_s: f64) -> [bool; 4] {
    [
        p.gain_s > 0.0,
        p.loss_s <= LOSS_PER_GAIN * p.gain_s,
        p.loss_s <= LOSS_SHARE * reference_s,
        p.meeting_der
            .iter()
            .zip(&off.meeting_der)
            .all(|(d, d0)| *d <= *d0 + MEETING_DER_SLACK),
    ]
}

/// The rule's pick: the highest passing floor, or [`OFF`] when none passes.
pub fn pick(points: &[FloorPoint], reference_s: f64) -> i32 {
    let off = points
        .iter()
        .find(|p| p.f == OFF)
        .expect("the grid holds -1");
    points
        .iter()
        .filter(|p| p.f != OFF && conditions(p, off, reference_s).iter().all(|c| *c))
        .map(|p| p.f)
        .max()
        .unwrap_or(OFF)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn turn(s: f64, e: f64, who: &str) -> Turn {
        Turn::new(s, e, who.to_owned())
    }

    #[test]
    fn cluster_sweep_floors_moved_speech_counts_right_wrong_and_non_speech() {
        // Truth: A 0-10, B 10-12. At -1, S0 holds 0-12 (owned by A, so B's speech there
        // has a wrong name, though S0 is B's main speaker). Unassigned at f: 9-13. Loss
        // 9-10, gain 10-12, non-speech 12-13 is not moved (S0 did not cover it).
        let reference = [turn(0.0, 10.0, "A"), turn(10.0, 12.0, "B")];
        let base = [turn(0.0, 12.0, "S0")];
        let left = [turn(9.0, 13.0, "unassigned")];
        let m = moved(&reference, &base, &left, (0.0, 14.0));
        assert!((m.loss_s - 1.0).abs() < 0.011, "{m:?}");
        assert!((m.gain_s - 2.0).abs() < 0.011, "{m:?}");
        assert!(m.non_speech_s.abs() < 1e-9, "{m:?}");
        assert!((m.unassigned_s - 4.0).abs() < 0.011, "{m:?}");
        // A false alarm taken away is non-speech.
        let base = [turn(0.0, 13.0, "S0")];
        let m = moved(&reference, &base, &left, (0.0, 14.0));
        assert!((m.non_speech_s - 1.0).abs() < 0.011, "{m:?}");
    }

    #[test]
    fn cluster_sweep_floors_owner_is_the_longest_overlap() {
        let reference = [turn(0.0, 10.0, "A"), turn(10.0, 20.0, "B")];
        let hyp = [turn(0.0, 6.0, "S1"), turn(6.0, 20.0, "S0")];
        let h = owners(&reference, &hyp, (0.0, 20.0));
        assert_eq!(
            h,
            vec![
                ("S0".to_owned(), "B".to_owned()),
                ("S1".to_owned(), "A".to_owned())
            ]
        );
    }

    fn point(f: i32, gain: f64, loss: f64, ders: &[f64]) -> FloorPoint {
        FloorPoint {
            f,
            gain_s: gain,
            loss_s: loss,
            meeting_der: ders.to_vec(),
        }
    }

    #[test]
    fn cluster_sweep_floors_rule_picks_the_highest_passing_floor() {
        let r = 1000.0;
        let pts = [
            point(OFF, 0.0, 0.0, &[0.10, 0.20]),
            point(0, 0.0, 0.0, &[0.10, 0.20]),
            point(10, 8.0, 1.0, &[0.101, 0.20]),
            point(20, 12.0, 2.9, &[0.102, 0.204]),
            point(30, 20.0, 4.0, &[0.103, 0.206]),
            point(40, 40.0, 6.0, &[0.103, 0.20]),
        ];
        // 0: no gain. 10, 20 pass. 30: meeting 2 is 0.006 worse. 40: loss 6 > 0.5% of 1000.
        assert_eq!(pick(&pts, r), 20);
        let off = &pts[0];
        assert_eq!(conditions(&pts[1], off, r), [false, true, true, true]);
        assert_eq!(conditions(&pts[4], off, r), [true, true, true, false]);
        assert_eq!(conditions(&pts[5], off, r), [true, true, false, true]);
        // Loss above a quarter of the gain fails.
        let greedy = point(50, 10.0, 3.0, &[0.10, 0.20]);
        assert_eq!(conditions(&greedy, off, r), [true, false, true, true]);
    }

    #[test]
    fn cluster_sweep_floors_rule_picks_minus_one_when_nothing_passes() {
        let pts = [
            point(OFF, 0.0, 0.0, &[0.1]),
            point(0, 0.0, 0.0, &[0.1]),
            point(30, 1.0, 1.0, &[0.1]),
        ];
        assert_eq!(pick(&pts, 1000.0), OFF);
    }
}
