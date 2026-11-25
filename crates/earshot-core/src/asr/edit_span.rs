//! The words a user's edited line covers (T12.16, T12.92): one rule for the final pass
//! (`earshot_runtime::post`, a new decode giving way to edited lines) and for sync (a
//! re-transcription made on another computer giving way to an edit made here,
//! docs/sync-spec.md section 7 rule 4b). Moved here from earshot-runtime unchanged.

use super::{Segment, Word};

/// `segments` without the words `mask` marks (in order over all their words); a segment
/// left without words is dropped. Returns the segments and the number of words removed.
#[must_use]
pub fn drop_masked(segments: &[Segment], mask: &[bool]) -> (Vec<Segment>, usize) {
    let mut out = Vec::new();
    let (mut k, mut removed) = (0, 0);
    for s in segments {
        let mut kept = Vec::new();
        for w in &s.words {
            if mask.get(k).copied().unwrap_or(false) {
                removed += 1;
            } else {
                kept.push(w.clone());
            }
            k += 1;
        }
        if kept.len() == s.words.len() {
            out.push(s.clone());
        } else if !kept.is_empty() {
            let mut seg = Segment::from_words(s.id, s.track, kept, s.start);
            seg.speaker_id = s.speaker_id;
            out.push(seg);
        }
    }
    (out, removed)
}

/// Whether `w` lies under the span of the edited segment `e` (same track checked by the
/// caller): their times overlap by more than nothing, or, for an untimed word (`start ==
/// end`), it starts inside `[e.start, e.end)`. An edited segment with no length covers
/// nothing.
#[must_use]
pub fn under_edit(w: &Word, e: &Segment) -> bool {
    if e.end <= e.start {
        return false;
    }
    if w.end <= w.start {
        return e.start <= w.start && w.start < e.end;
    }
    w.start < e.end && w.end > e.start
}

/// A new decode giving way to the user's edited segments (T12.16): every word of `segments`
/// under the span of an `edited` segment on the same track ([`under_edit`]) is left out; a
/// segment that loses some words is rebuilt from the rest (same id, track and speaker, span
/// and text from the words that stay), one that loses all is dropped. A new segment with no
/// words is judged by its own span, as one word. The rule is per word, not per segment,
/// because a re-run cuts segments differently: a new segment that runs past the edited one
/// keeps the words outside it. A word straddling the edited span's edge goes, because the
/// edited text already covers that stretch of audio. Returns the segments and how many words
/// were left out.
#[must_use]
pub fn drop_words_under_edits(segments: Vec<Segment>, edited: &[Segment]) -> (Vec<Segment>, usize) {
    if edited.is_empty() {
        return (segments, 0);
    }
    let mut kept_segments = Vec::with_capacity(segments.len());
    let mut mask = Vec::new();
    let mut wordless_dropped = 0;
    for s in segments {
        let edits: Vec<&Segment> = edited.iter().filter(|e| e.track == s.track).collect();
        if s.words.is_empty() {
            let span = Word {
                text: String::new(),
                start: s.start,
                end: s.end,
                probability: 0.0,
            };
            if edits.iter().any(|e| under_edit(&span, e)) {
                wordless_dropped += 1;
            } else {
                kept_segments.push(s);
            }
            continue;
        }
        mask.extend(
            s.words
                .iter()
                .map(|w| edits.iter().any(|e| under_edit(w, e))),
        );
        kept_segments.push(s);
    }
    let (out, removed) = drop_masked(&kept_segments, &mask);
    (out, removed + wordless_dropped)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::asr::MeetingTime;
    use crate::identity::EntityId;

    fn id(n: u8) -> EntityId {
        EntityId::from_bytes([0x01, 0x90, 0, 0, 0, 0, 0x70, 0, 0x80, 0, 0, 0, 0, 0, n, 1])
    }

    fn line(n: u8, track: u8, words: &[(&str, u64, u64)]) -> Segment {
        let words = words
            .iter()
            .map(|(t, s, e)| Word::new(*t, *s, *e, 0.9))
            .collect();
        let mut s = Segment::from_words(id(n), id(track), words, MeetingTime::ZERO);
        s.speaker_id = Some(id(50 + n));
        s
    }

    fn span(n: u8, track: u8, start: u64, end: u64) -> Segment {
        let mut s = line(n, track, &[]);
        s.start = MeetingTime::from_millis(start);
        s.end = MeetingTime::from_millis(end);
        s
    }

    #[test]
    fn asr_edit_span_under_edit_overlap_untimed_and_empty_span() {
        let e = span(1, 9, 1_000, 2_000);
        assert!(under_edit(&Word::new("a", 1_900, 2_100, 0.9), &e));
        assert!(under_edit(&Word::new("a", 900, 1_001, 0.9), &e));
        // Touching edges do not overlap.
        assert!(!under_edit(&Word::new("a", 2_000, 2_300, 0.9), &e));
        assert!(!under_edit(&Word::new("a", 700, 1_000, 0.9), &e));
        // An untimed word counts by its start in [start, end).
        assert!(under_edit(&Word::new("a", 1_000, 1_000, 0.9), &e));
        assert!(!under_edit(&Word::new("a", 2_000, 2_000, 0.9), &e));
        // An edited segment with no length covers nothing.
        let empty = span(2, 9, 1_500, 1_500);
        assert!(!under_edit(&Word::new("a", 1_000, 2_000, 0.9), &empty));
    }

    #[test]
    fn asr_edit_span_drops_words_on_the_same_track_only_and_keeps_ids() {
        let edited = span(1, 9, 1_000, 2_000);
        let mixed = line(
            2,
            9,
            &[
                ("before", 200, 600),
                ("under", 1_200, 1_500),
                ("after", 2_100, 2_500),
            ],
        );
        let gone = line(3, 9, &[("all", 1_100, 1_300), ("under", 1_400, 1_900)]);
        let other = line(4, 8, &[("other", 1_200, 1_500)]);
        let wordless = span(5, 9, 1_200, 1_400);
        let (out, dropped) = drop_words_under_edits(
            vec![mixed.clone(), gone, other.clone(), wordless],
            std::slice::from_ref(&edited),
        );
        assert_eq!(dropped, 4, "one word, two words and one wordless line");
        assert_eq!(out.len(), 2, "{out:?}");
        assert_eq!(out[0].id, mixed.id);
        assert_eq!(out[0].speaker_id, mixed.speaker_id);
        assert_eq!(out[0].text, "before after");
        assert_eq!(out[1], other, "another track is untouched");
        // No edits: everything stays, exactly.
        assert_eq!(
            drop_words_under_edits(vec![mixed.clone()], &[]),
            (vec![mixed], 0)
        );
    }
}
