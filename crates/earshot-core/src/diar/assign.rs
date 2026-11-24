//! Word-to-speaker assignment and speaker labelling of transcript segments.
//!
//! Each [`Word`] goes to the speaker whose turns overlap its span the most (the rule used
//! by WhisperX `assign_word_speakers` and pyannote-whisper style aligners). Ties go to the
//! previous word's speaker when it is among the tied ones, otherwise to the lowest speaker
//! index. A word that overlaps no turn (a gap, or ASR timestamps drifting past a turn
//! boundary) goes to the speaker of the nearest turn; on equal distance, to the previous
//! word's speaker if tied, otherwise to the speaker of the earlier turn. A zero-length
//! word counts as overlapping the turns that contain its instant.
//!
//! [`label_segment_speakers`] (and so [`label_segments`]) then moves each speaker change of a
//! track that falls inside a sentence, with no pause, to the nearest sentence end or pause at
//! most two words away ([`snap_changes`] with [`EDGE_SNAP`], T12.88): the overlap rule alone
//! gave a sentence's last word to the next speaker wherever the diarisation drew the turn
//! boundary a little early.

use super::pipeline::SpeakerTurn;

/// What a final transcript line with no speaker is called in summaries, exports and the app
/// (T12.65: speech diarisation could not give to any speaker it found).
pub const UNKNOWN_SPEAKER: &str = "Unknown speaker";
use crate::asr::{MeetingTime, Segment, Word};
use crate::identity::EntityId;

/// Speaker of each word (index into the diarisation's speakers), `None` only when there
/// are no turns. `words` should be in time order; the tie rule uses the previous word.
#[must_use]
pub fn assign_words(words: &[Word], turns: &[SpeakerTurn]) -> Vec<Option<usize>> {
    let mut prev = None;
    words
        .iter()
        .map(|w| {
            let k = pick(w.start, w.end, turns, prev);
            prev = k.or(prev);
            k
        })
        .collect()
}

/// Speaker for the span `start..end` given the previous word's speaker (see module docs).
fn pick(
    start: MeetingTime,
    end: MeetingTime,
    turns: &[SpeakerTurn],
    prev: Option<usize>,
) -> Option<usize> {
    let n = turns.iter().map(|t| t.speaker + 1).max()?;
    let (s, e) = (start.as_millis(), end.as_millis().max(start.as_millis()));
    let mut overlap = vec![0u64; n];
    for t in turns {
        let (ts, te) = (t.start.as_millis(), t.end.as_millis());
        overlap[t.speaker] += if e > s {
            e.min(te).saturating_sub(s.max(ts))
        } else {
            u64::from(ts <= s && s < te)
        };
    }
    let best = overlap.iter().copied().max().unwrap_or(0);
    if best > 0 {
        let tied = |k: usize| overlap[k] == best;
        return Some(match prev {
            Some(p) if p < n && tied(p) => p,
            _ => (0..n).find(|&k| tied(k)).unwrap_or(0),
        });
    }
    // In a gap: the nearest turn; on equal distance the previous speaker, then the
    // earlier turn.
    let distance = |t: &SpeakerTurn| {
        let (ts, te) = (t.start.as_millis(), t.end.as_millis());
        if te <= s {
            s - te
        } else {
            ts.saturating_sub(e)
        }
    };
    let nearest = turns.iter().map(distance).min()?;
    let tied: Vec<&SpeakerTurn> = turns.iter().filter(|t| distance(t) == nearest).collect();
    if let Some(p) = prev
        && tied.iter().any(|t| t.speaker == p)
    {
        return Some(p);
    }
    tied.iter()
        .min_by_key(|t| (t.start, t.speaker))
        .map(|t| t.speaker)
}

/// A pause of at least this between two words supports a speaker change there (ms; T6.9's
/// `NO_PAUSE_MS`, the rule of [`snap_changes`]).
pub const SNAP_PAUSE_MS: u64 = 300;

/// The snap the final pass applies (T12.88 stage 2, picked by the rule of
/// `docs/far-speakers/edge-words.md` and judged on ten held-out AMI dev meetings: edge errors
/// 434 to 267, words given the wrong speaker 5.70% to 5.22%).
pub const EDGE_SNAP: Snap = Snap {
    reach: 2,
    clause_ends: false,
};

/// How far [`snap_changes`] may move a speaker change, and which word ends support one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Snap {
    /// At most this many words either way.
    pub reach: usize,
    /// A word ending in `,` supports a change too (else only `.`, `?` and `!`).
    pub clause_ends: bool,
}

/// Does `text` end a sentence (`.`, `?`, `!`; with `clause_ends` also `,`), ignoring closing
/// quotes and brackets?
fn ends_unit(text: &str, clause_ends: bool) -> bool {
    let t = text.trim_end_matches(['"', '\'', ')', ']', '\u{201D}', '\u{2019}']);
    t.ends_with(['.', '?', '!']) || (clause_ends && t.ends_with(','))
}

/// Is the boundary between words `i - 1` and `i` supported (a unit end or a pause)?
fn supported(words: &[Word], i: usize, clause_ends: bool) -> bool {
    ends_unit(&words[i - 1].text, clause_ends)
        || words[i]
            .start
            .as_millis()
            .saturating_sub(words[i - 1].end.as_millis())
            >= SNAP_PAUSE_MS
}

/// Moves each speaker change that sits on an unsupported boundary (no sentence end before it
/// and no pause of [`SNAP_PAUSE_MS`]) to the nearest supported boundary at most
/// [`Snap::reach`] words away, the earlier on equal distance, when one lies inside the two runs
/// it separates and leaves both non-empty (T12.88 stage 2, `docs/far-speakers/edge-words.md`;
/// T6.9a's step 3 in `word-changes.md`). Changes are taken left to right. No run is merged and
/// no change is added or removed. `labels` are `words`' speakers in time order, as
/// [`assign_words`] gives them; the result has the same length.
#[must_use]
pub fn snap_changes(words: &[Word], labels: &[Option<usize>], snap: Snap) -> Vec<Option<usize>> {
    snap_changes_except(words, labels, snap, None)
}

/// [`snap_changes`], leaving every change into or out of label `keep` where it is (the
/// unassigned speech of [`label_segment_speakers_unassigned`]: the rule was measured between
/// real speakers only).
fn snap_changes_except(
    words: &[Word],
    labels: &[Option<usize>],
    snap: Snap,
    keep: Option<usize>,
) -> Vec<Option<usize>> {
    let mut out = labels.to_vec();
    if snap.reach == 0 || words.len() != labels.len() {
        return out;
    }
    let ok = |i: usize| supported(words, i, snap.clause_ends);
    let changes: Vec<usize> = (1..out.len()).filter(|&i| out[i] != out[i - 1]).collect();
    for c in changes {
        if out[c] == out[c - 1] || ok(c) {
            continue;
        }
        if keep.is_some() && (out[c] == keep || out[c - 1] == keep) {
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
        let target = (1..=snap.reach).find_map(|d| {
            let back = c.checked_sub(d).filter(|&b| b > run_start && ok(b));
            let forward = Some(c + d).filter(|&f| f < run_end && ok(f));
            back.or(forward)
        });
        match target {
            Some(t) if t < c => out[t..c].iter_mut().for_each(|l| *l = after),
            Some(t) => out[c..t].iter_mut().for_each(|l| *l = before),
            None => {}
        }
    }
    out
}

/// Sets [`Segment::speaker_id`] from `turns`, splitting a segment wherever its words
/// change speaker after the changes snap to sentence ends and pauses (module docs).
///
/// - `speaker_id(k)` maps diarisation speaker `k` to the id to store (for example the
///   matched person's speaker id); `None` leaves the field unset.
/// - A segment whose words all go to one speaker is returned unchanged apart from
///   `speaker_id` (its text is kept as the recogniser produced it).
/// - A split segment is rebuilt part by part with [`Segment::from_words`], so every part,
///   the first included, takes its text, span and confidence from its own words (the
///   recogniser's original text is not kept). The first part keeps the segment's id;
///   later parts get ids from `new_id`.
/// - A segment without words is assigned as if it were one word spanning the segment.
///
/// Segments should be in time order (the tie rule carries the previous word's speaker
/// across segment boundaries).
pub fn label_segments(
    segments: Vec<Segment>,
    turns: &[SpeakerTurn],
    speaker_id: impl Fn(usize) -> Option<EntityId>,
    new_id: &mut dyn FnMut() -> EntityId,
) -> Vec<Segment> {
    label_segment_speakers(segments, turns, new_id)
        .into_iter()
        .map(|(mut seg, k)| {
            seg.speaker_id = k.and_then(&speaker_id);
            seg
        })
        .collect()
}

/// [`label_segment_speakers`] with the speech diarisation left without a speaker (T12.65,
/// [`super::Diarization::unassigned`]): the unassigned spans take part in the rule of the
/// module docs as one more speaker, ranked after every real one (so a tie goes to a real
/// speaker), and a word or segment that goes to them gets `None`. A segment is split where
/// its words move between a speaker and unassigned speech. With no unassigned span the parts
/// are exactly [`label_segment_speakers`]'s.
pub fn label_segment_speakers_unassigned(
    segments: Vec<Segment>,
    turns: &[SpeakerTurn],
    unassigned: &[super::UnassignedSpan],
    new_id: &mut dyn FnMut() -> EntityId,
) -> UnassignedLabelling {
    if unassigned.is_empty() {
        return UnassignedLabelling {
            parts: label_segment_speakers(segments, turns, new_id),
            unassigned: Vec::new(),
            words: 0,
        };
    }
    let pseudo = turns.iter().map(|t| t.speaker + 1).max().unwrap_or(0);
    let mut all = turns.to_vec();
    all.extend(unassigned.iter().map(|s| SpeakerTurn {
        speaker: pseudo,
        start: s.start,
        end: s.end,
    }));
    let (mut words, mut ids) = (0, Vec::new());
    let parts = label_segment_speakers_keeping(segments, &all, new_id, Some(pseudo))
        .into_iter()
        .map(|(seg, k)| {
            if k == Some(pseudo) {
                words += seg.words.len().max(1);
                ids.push(seg.id);
                (seg, None)
            } else {
                (seg, k)
            }
        })
        .collect();
    UnassignedLabelling {
        parts,
        unassigned: ids,
        words,
    }
}

/// What [`label_segment_speakers_unassigned`] returns.
#[derive(Debug, Clone, PartialEq)]
pub struct UnassignedLabelling {
    /// Each segment part with its diarisation speaker (`None`: no speaker).
    pub parts: Vec<(Segment, Option<usize>)>,
    /// Ids of the parts that went to unassigned speech (a part with no speaker because its
    /// track has no turns at all is not among them).
    pub unassigned: Vec<EntityId>,
    /// Words of those parts (a wordless segment counts as one).
    pub words: usize,
}

/// [`label_segments`] before the ids are set: each segment part (split, text and ids
/// exactly as [`label_segments`] makes them, `speaker_id` left as it came in) with its
/// diarisation speaker, so a caller can see which speakers own any part before it creates
/// their rows.
pub fn label_segment_speakers(
    segments: Vec<Segment>,
    turns: &[SpeakerTurn],
    new_id: &mut dyn FnMut() -> EntityId,
) -> Vec<(Segment, Option<usize>)> {
    label_segment_speakers_keeping(segments, turns, new_id, None)
}

/// [`label_segment_speakers`], whose snapping leaves changes into or out of `keep` alone.
fn label_segment_speakers_keeping(
    segments: Vec<Segment>,
    turns: &[SpeakerTurn],
    new_id: &mut dyn FnMut() -> EntityId,
    keep: Option<usize>,
) -> Vec<(Segment, Option<usize>)> {
    // Every word's speaker by the overlap rule, segment by segment (a wordless segment's own
    // label carries the tie rule on, as before).
    let mut prev: Option<usize> = None;
    let mut per_segment: Vec<Vec<Option<usize>>> = Vec::with_capacity(segments.len());
    let mut wordless: Vec<Option<usize>> = Vec::with_capacity(segments.len());
    for seg in &segments {
        if seg.words.is_empty() {
            let k = pick(seg.start, seg.end, turns, prev);
            prev = k.or(prev);
            wordless.push(k);
            per_segment.push(Vec::new());
            continue;
        }
        wordless.push(None);
        per_segment.push(
            seg.words
                .iter()
                .map(|w| {
                    let k = pick(w.start, w.end, turns, prev);
                    prev = k.or(prev);
                    k
                })
                .collect(),
        );
    }
    // Then the track's speaker changes snap to sentence ends and pauses across segments
    // (T12.88 stage 2, [`EDGE_SNAP`]).
    let words: Vec<Word> = segments
        .iter()
        .flat_map(|s| s.words.iter().cloned())
        .collect();
    let flat: Vec<Option<usize>> = per_segment.iter().flatten().copied().collect();
    let mut snapped = snap_changes_except(&words, &flat, EDGE_SNAP, keep).into_iter();
    for labels in &mut per_segment {
        for l in labels.iter_mut() {
            *l = snapped.next().flatten();
        }
    }
    let mut out = Vec::with_capacity(segments.len());
    for ((seg, labels), k) in segments.into_iter().zip(per_segment).zip(wordless) {
        if seg.words.is_empty() {
            out.push((seg, k));
            continue;
        }
        if labels.iter().all(|l| *l == labels[0]) {
            out.push((seg, labels[0]));
            continue;
        }
        let mut runs: Vec<(Option<usize>, Vec<Word>)> = Vec::new();
        for (w, l) in seg.words.iter().zip(&labels) {
            match runs.last_mut() {
                Some((rl, ws)) if rl == l => ws.push(w.clone()),
                _ => runs.push((*l, vec![w.clone()])),
            }
        }
        for (i, (l, words)) in runs.into_iter().enumerate() {
            let id = if i == 0 { seg.id } else { new_id() };
            let mut part = Segment::from_words(id, seg.track, words, seg.start);
            part.speaker_id = seg.speaker_id;
            out.push((part, l));
        }
    }
    out
}
