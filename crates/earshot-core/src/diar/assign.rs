//! Word-to-speaker assignment and speaker labelling of transcript segments.
//!
//! Each [`Word`] goes to the speaker whose turns overlap its span the most (the rule used
//! by WhisperX `assign_word_speakers` and pyannote-whisper style aligners). Ties go to the
//! previous word's speaker when it is among the tied ones, otherwise to the lowest speaker
//! index. A word that overlaps no turn (a gap, or ASR timestamps drifting past a turn
//! boundary) goes to the speaker of the nearest turn; on equal distance, to the previous
//! word's speaker if tied, otherwise to the speaker of the earlier turn. A zero-length
//! word counts as overlapping the turns that contain its instant.

use super::pipeline::SpeakerTurn;
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

/// Sets [`Segment::speaker_id`] from `turns`, splitting a segment wherever its words
/// change speaker.
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

/// [`label_segments`] before the ids are set: each segment part (split, text and ids
/// exactly as [`label_segments`] makes them, `speaker_id` left as it came in) with its
/// diarisation speaker, so a caller can see which speakers own any part before it creates
/// their rows.
pub fn label_segment_speakers(
    segments: Vec<Segment>,
    turns: &[SpeakerTurn],
    new_id: &mut dyn FnMut() -> EntityId,
) -> Vec<(Segment, Option<usize>)> {
    let mut prev: Option<usize> = None;
    let mut out = Vec::with_capacity(segments.len());
    for seg in segments {
        if seg.words.is_empty() {
            let k = pick(seg.start, seg.end, turns, prev);
            prev = k.or(prev);
            out.push((seg, k));
            continue;
        }
        let labels: Vec<Option<usize>> = seg
            .words
            .iter()
            .map(|w| {
                let k = pick(w.start, w.end, turns, prev);
                prev = k.or(prev);
                k
            })
            .collect();
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
