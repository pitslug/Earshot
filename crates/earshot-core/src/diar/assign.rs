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
    let mut picker = Picker::new(turns);
    let mut prev = None;
    words
        .iter()
        .map(|w| {
            let k = picker.pick(w.start, w.end, prev);
            prev = k.or(prev);
            k
        })
        .collect()
}

/// The rule of the module docs for one span after another (T14.48): the turns are indexed once,
/// by start and by end, and spans whose starts do not go backwards are answered by one sweep
/// (a pointer into the turns by start and the set of turns still open), so labelling a track is
/// close to linear in its words and turns and allocates nothing per word. A span that starts
/// before the previous one restarts the sweep; the answer never depends on the order asked.
///
/// For a span `[s, e)` (`e` at least `s`): a turn adds to its speaker's overlap only if it
/// starts before `e` (at or before `s` for a zero-length span) and ends after `s`, so the open
/// set (turns starting at or before the largest `e` asked since the restart, ending after `s`)
/// holds every turn that overlaps. In a gap every turn either ended by `s` (distance `s - end`,
/// the largest end wins: binary search in the turns by end) or ends after it (distance
/// `start - e`, saturating: the smallest start among them, kept as a suffix minimum over the
/// turns by end; the ones at distance 0 all start at or before `e` and so are in the open set).
struct Picker<'a> {
    turns: &'a [SpeakerTurn],
    /// Number of speakers (largest index + 1); 0 with no turns.
    speakers: usize,
    /// Turn indices by start (stable).
    by_start: Vec<usize>,
    /// Turn indices by end (stable).
    by_end: Vec<usize>,
    /// `min_start[k]`: the smallest start (ms) among `by_end[k..]`; `u64::MAX` past the end.
    min_start: Vec<u64>,
    /// Next turn of `by_start` to open.
    next: usize,
    /// Open turns: started at or before the largest end asked, not yet ended.
    open: Vec<usize>,
    /// Start of the last span asked (the sweep's position).
    last_start: Option<u64>,
    /// Overlap per speaker, all zero between calls.
    overlap: Vec<u64>,
}

impl<'a> Picker<'a> {
    fn new(turns: &'a [SpeakerTurn]) -> Self {
        let speakers = turns.iter().map(|t| t.speaker + 1).max().unwrap_or(0);
        let mut by_start: Vec<usize> = (0..turns.len()).collect();
        by_start.sort_by_key(|&i| turns[i].start);
        let mut by_end: Vec<usize> = (0..turns.len()).collect();
        by_end.sort_by_key(|&i| turns[i].end);
        let mut min_start = vec![u64::MAX; turns.len() + 1];
        for k in (0..turns.len()).rev() {
            min_start[k] = min_start[k + 1].min(turns[by_end[k]].start.as_millis());
        }
        Picker {
            turns,
            speakers,
            by_start,
            by_end,
            min_start,
            next: 0,
            open: Vec::new(),
            last_start: None,
            overlap: vec![0; speakers],
        }
    }

    /// Speaker for the span `start..end` given the previous word's speaker (module docs).
    fn pick(&mut self, start: MeetingTime, end: MeetingTime, prev: Option<usize>) -> Option<usize> {
        if self.turns.is_empty() {
            return None;
        }
        let turns = self.turns;
        let n = self.speakers;
        let (s, e) = (start.as_millis(), end.as_millis().max(start.as_millis()));
        if self.last_start.is_some_and(|l| s < l) {
            self.next = 0;
            self.open.clear();
        }
        self.last_start = Some(s);
        while self.next < turns.len() && turns[self.by_start[self.next]].start.as_millis() <= e {
            self.open.push(self.by_start[self.next]);
            self.next += 1;
        }
        self.open.retain(|&i| turns[i].end.as_millis() > s);

        // Overlap, as the module docs count it, over the open turns only.
        for &i in &self.open {
            let t = &turns[i];
            let (ts, te) = (t.start.as_millis(), t.end.as_millis());
            self.overlap[t.speaker] += if e > s {
                e.min(te).saturating_sub(s.max(ts))
            } else {
                u64::from(ts <= s && s < te)
            };
        }
        let mut best = 0;
        let mut first_tied = usize::MAX;
        for &i in &self.open {
            let k = turns[i].speaker;
            let o = self.overlap[k];
            if o > best || (o == best && o > 0 && k < first_tied) {
                if o > best {
                    best = o;
                }
                first_tied = k;
            }
        }
        let prev_tied = prev.is_some_and(|p| p < n && best > 0 && self.overlap[p] == best);
        for &i in &self.open {
            self.overlap[turns[i].speaker] = 0;
        }
        if best > 0 {
            return Some(match prev {
                Some(p) if prev_tied => p,
                _ => first_tied,
            });
        }

        // In a gap: the nearest turn; on equal distance the previous speaker, then the
        // earlier turn.
        let ended = self
            .by_end
            .partition_point(|&i| turns[i].end.as_millis() <= s);
        let before = (ended > 0).then(|| s - turns[self.by_end[ended - 1]].end.as_millis());
        let first_after = self.min_start[ended];
        let after = (ended < turns.len()).then(|| first_after.saturating_sub(e));
        let nearest = match (before, after) {
            (Some(b), Some(a)) => b.min(a),
            (Some(b), None) => b,
            (None, Some(a)) => a,
            (None, None) => return None,
        };
        let mut prev_found = false;
        let mut earliest: Option<(MeetingTime, usize)> = None;
        let mut visit = |t: &SpeakerTurn| {
            prev_found |= prev == Some(t.speaker);
            let key = (t.start, t.speaker);
            if earliest.is_none_or(|k| key < k) {
                earliest = Some(key);
            }
        };
        if before == Some(nearest) {
            let last_end = turns[self.by_end[ended - 1]].end;
            for &i in self.by_end[..ended].iter().rev() {
                if turns[i].end != last_end {
                    break;
                }
                visit(&turns[i]);
            }
        }
        if after == Some(nearest) {
            if first_after <= e {
                // Distance 0: every turn ending after `s` and starting at or before `e`, all
                // of them open.
                for &i in &self.open {
                    if turns[i].start.as_millis() <= e {
                        visit(&turns[i]);
                    }
                }
            } else {
                let from = self
                    .by_start
                    .partition_point(|&i| turns[i].start.as_millis() < first_after);
                for &i in &self.by_start[from..] {
                    if turns[i].start.as_millis() != first_after {
                        break;
                    }
                    if turns[i].end.as_millis() > s {
                        visit(&turns[i]);
                    }
                }
            }
        }
        if prev_found {
            return prev;
        }
        earliest.map(|(_, k)| k)
    }
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
    let mut picker = Picker::new(turns);
    let mut prev: Option<usize> = None;
    let mut per_segment: Vec<Vec<Option<usize>>> = Vec::with_capacity(segments.len());
    let mut wordless: Vec<Option<usize>> = Vec::with_capacity(segments.len());
    for seg in &segments {
        if seg.words.is_empty() {
            let k = picker.pick(seg.start, seg.end, prev);
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
                    let k = picker.pick(w.start, w.end, prev);
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

#[cfg(test)]
mod tests {
    //! The sweep of [`Picker`] (T14.48) against the per-word scan it replaced, kept here as the
    //! reference: the same speaker for every span and the same labelled parts.

    use super::*;

    /// The rule as it was before T14.48: every turn scanned for every word.
    fn reference_pick(
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

    fn reference_assign(words: &[Word], turns: &[SpeakerTurn]) -> Vec<Option<usize>> {
        let mut prev = None;
        words
            .iter()
            .map(|w| {
                let k = reference_pick(w.start, w.end, turns, prev);
                prev = k.or(prev);
                k
            })
            .collect()
    }

    /// `label_segment_speakers_keeping` as it was before T14.48 (with the reference pick).
    fn reference_label(
        segments: Vec<Segment>,
        turns: &[SpeakerTurn],
        new_id: &mut dyn FnMut() -> EntityId,
        keep: Option<usize>,
    ) -> Vec<(Segment, Option<usize>)> {
        let mut prev: Option<usize> = None;
        let mut per_segment: Vec<Vec<Option<usize>>> = Vec::with_capacity(segments.len());
        let mut wordless: Vec<Option<usize>> = Vec::with_capacity(segments.len());
        for seg in &segments {
            if seg.words.is_empty() {
                let k = reference_pick(seg.start, seg.end, turns, prev);
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
                        let k = reference_pick(w.start, w.end, turns, prev);
                        prev = k.or(prev);
                        k
                    })
                    .collect(),
            );
        }
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

    /// A small deterministic generator (xorshift64*), so a failing case can be re-run.
    struct Gen(u64);

    impl Gen {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 >> 12;
            self.0 ^= self.0 << 25;
            self.0 ^= self.0 >> 27;
            self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
        }

        fn below(&mut self, n: u64) -> u64 {
            self.next() % n.max(1)
        }

        fn count(&mut self, n: u64) -> usize {
            usize::try_from(self.below(n)).unwrap()
        }
    }

    fn ms(v: u64) -> MeetingTime {
        MeetingTime::from_millis(v)
    }

    /// Turns on a coarse grid (so ties of overlap and distance are common), overlapping, some
    /// sharing a start or an end, some of zero length and a few ending before they start.
    fn gen_turns(g: &mut Gen, speakers: u64, count: usize, span: u64) -> Vec<SpeakerTurn> {
        (0..count)
            .map(|_| {
                let start = g.below(span / 50) * 50;
                let len = match g.below(10) {
                    0 => 0,
                    1 => g.below(4) * 50,
                    _ => 50 + g.below(40) * 50,
                };
                let end = if g.below(25) == 0 {
                    start.saturating_sub(50 + g.below(4) * 50)
                } else {
                    start + len
                };
                SpeakerTurn {
                    speaker: g.count(speakers),
                    start: ms(start),
                    end: ms(end),
                }
            })
            .collect()
    }

    /// Words mostly in time order, some of zero length, some in gaps or past every turn, with
    /// sentence ends and pauses for the snap rule; `shuffled` puts some out of order.
    fn gen_words(g: &mut Gen, count: usize, shuffled: bool) -> Vec<Word> {
        let mut at = g.below(200);
        let mut words: Vec<Word> = (0..count)
            .map(|i| {
                at += match g.below(8) {
                    0 => 300 + g.below(800),
                    1 => 0,
                    _ => g.below(120),
                };
                let len = match g.below(7) {
                    0 => 0,
                    1 => 600 + g.below(1500),
                    2 => 50 * (1 + g.below(6)),
                    _ => 10 * (1 + g.below(40)),
                };
                // Some words on the turns' grid, so they start or end on a turn boundary.
                if g.below(3) == 0 {
                    at = at / 50 * 50;
                }
                let text = match g.below(6) {
                    0 => format!("word{i}."),
                    1 => format!("word{i}?\""),
                    2 => format!("word{i},"),
                    _ => format!("word{i}"),
                };
                Word {
                    text,
                    start: ms(at),
                    end: ms(at + len),
                    probability: 0.9,
                }
            })
            .collect();
        if shuffled {
            for i in 0..words.len() {
                if g.below(5) == 0 {
                    let j = g.count(words.len() as u64);
                    words.swap(i, j);
                }
            }
        }
        words
    }

    fn numbered_ids() -> impl FnMut() -> EntityId {
        let mut n: u64 = 0;
        move || {
            n += 1;
            let mut bytes = [0u8; 16];
            bytes[8..].copy_from_slice(&n.to_be_bytes());
            EntityId::from_bytes(bytes)
        }
    }

    #[test]
    fn diar_assign_sweep_gives_the_scan_s_speaker_for_every_span() {
        let mut g = Gen(0x9E37_79B9_7F4A_7C15);
        let (mut gaps, mut zero, mut touching, mut checked) = (0, 0, 0, 0);
        let (mut ties, mut prev_decides, mut restarts, mut inverted) = (0, 0, 0, 0);
        for case in 0..1500 {
            let span = 2000 + g.below(20_000);
            let speakers = 1 + g.below(5);
            let count = g.count(30);
            let turns = gen_turns(&mut g, speakers, count, span);
            let count = g.count(80);
            let words = gen_words(&mut g, count, case % 3 == 0);
            let expected = reference_assign(&words, &turns);
            assert_eq!(assign_words(&words, &turns), expected, "case {case}");
            checked += words.len();
            // Words judged beside a turn that ends before it starts.
            if turns.iter().any(|t| t.end < t.start) {
                inverted += words.len();
            }
            // How often the harder paths came up, so the generator is known to reach them.
            for (i, w) in words.iter().enumerate() {
                let (s, e) = (w.start.as_millis(), w.end.as_millis());
                // A span starting before the one before it restarts the sweep.
                restarts += usize::from(i > 0 && w.start < words[i - 1].start);
                // Overlap ties between speakers, and those the previous word's speaker
                // decides (it is tied and is not the lowest tied index).
                let n = turns.iter().map(|t| t.speaker + 1).max().unwrap_or(0);
                let mut overlap = vec![0u64; n];
                for t in &turns {
                    let (ts, te) = (t.start.as_millis(), t.end.as_millis());
                    overlap[t.speaker] += if e > s {
                        e.min(te).saturating_sub(s.max(ts))
                    } else {
                        u64::from(ts <= s && s < te)
                    };
                }
                let best = overlap.iter().copied().max().unwrap_or(0);
                let tied: Vec<usize> = (0..n).filter(|&k| best > 0 && overlap[k] == best).collect();
                if tied.len() > 1 {
                    ties += 1;
                    let prev = expected[..i].iter().rev().find_map(|k| *k);
                    prev_decides +=
                        usize::from(prev.is_some_and(|p| tied.contains(&p) && p != tied[0]));
                }
                let inside = turns.iter().any(|t| {
                    let (ts, te) = (t.start.as_millis(), t.end.as_millis());
                    if e > s {
                        e.min(te) > s.max(ts)
                    } else {
                        ts <= s && s < te
                    }
                });
                gaps += usize::from(!inside && !turns.is_empty());
                zero += usize::from(e == s);
                touching += usize::from(turns.iter().any(|t| t.end == w.start || t.start == w.end));
            }
        }
        assert!(checked > 40_000, "only {checked} words");
        assert!(gaps > 1000, "only {gaps} words in gaps");
        assert!(zero > 1000, "only {zero} zero-length words");
        assert!(
            touching > 500,
            "only {touching} words touching a turn boundary"
        );
        assert!(ties > 1000, "only {ties} overlap ties between speakers");
        assert!(
            prev_decides > 400,
            "only {prev_decides} ties the previous speaker decides"
        );
        assert!(restarts > 2500, "only {restarts} sweep restarts");
        assert!(
            inverted > 7000,
            "only {inverted} words beside an inverted turn"
        );
    }

    #[test]
    fn diar_assign_sweep_labels_segments_as_the_scan_did() {
        let mut g = Gen(0xD1B5_4A32_D192_ED03);
        let (mut split, mut wordless) = (0, 0);
        for case in 0..600 {
            let span = 3000 + g.below(30_000);
            let speakers = 1 + g.below(4);
            let count = g.count(25);
            let turns = gen_turns(&mut g, speakers, count, span);
            let count = g.count(90);
            let words = gen_words(&mut g, count, case % 5 == 0);
            // The words cut into segments, some of them left wordless.
            let track = EntityId::from_bytes([7; 16]);
            let mut ids = numbered_ids();
            let mut segments = Vec::new();
            let mut rest = words.as_slice();
            while !rest.is_empty() {
                let take = (1 + g.count(12)).min(rest.len());
                let (part, tail) = rest.split_at(take);
                rest = tail;
                let id = ids();
                if g.below(6) == 0 {
                    let mut empty = Segment::from_words(id, track, Vec::new(), part[0].start);
                    empty.end = part[part.len() - 1].end.max(part[0].start);
                    segments.push(empty);
                    wordless += 1;
                } else {
                    segments.push(Segment::from_words(id, track, part.to_vec(), part[0].start));
                }
            }
            let keep = (g.below(3) == 0).then(|| g.count(4));
            let ours =
                label_segment_speakers_keeping(segments.clone(), &turns, &mut numbered_ids(), keep);
            let theirs = reference_label(segments.clone(), &turns, &mut numbered_ids(), keep);
            split += ours.len().saturating_sub(segments.len());
            assert_eq!(ours, theirs, "case {case}");
        }
        assert!(split > 100, "only {split} segment parts from splits");
        assert!(wordless > 100, "only {wordless} wordless segments");
    }
}
