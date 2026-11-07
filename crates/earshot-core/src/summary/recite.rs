//! Moving a partial summary made from the live transcript onto the final transcript
//! (T8.3j, option A of `docs/live-summaries.md`).
//!
//! A chunk summarised during the meeting cites live segments, and its action items may be
//! owned by a track label ("Me", "Them"). The final pass writes new segments with new ids
//! and diarised speaker names. [`recite`] rewrites the partial's citations and owners onto
//! the final segments:
//!
//! - every cited live segment becomes the final segments on the same track that overlap it
//!   in time, largest overlap first;
//! - a list item's citations are the union of its live segments' final segments; above
//!   [`MAX_CITATIONS`] the ones with the most total overlap are kept; the kept ids are in
//!   meeting order;
//! - an action item whose owner is a live speaker label gets the final speaker of its
//!   re-cited segments when they all have one speaker, else keeps the label;
//! - a cited live segment that is unknown or overlaps no final segment on its track makes
//!   the whole partial fail: the caller then maps that chunk again from the final
//!   transcript.
//!
//! Free text (title, tldr, bullets, decisions, tasks) is kept as written: a partial may still
//! say "Them" in prose. Measured by T8.3ja before the feature can be turned on.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use super::{MAX_CITATIONS, SummaryV1};
use crate::identity::EntityId;

/// A transcript segment with its time span, track and speaker, as [`recite`] needs it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TimedSegment {
    /// Segment id.
    pub id: EntityId,
    /// Which recorded track it came from (an index the caller assigns; live and final
    /// segments of one recording use the same numbering).
    pub track: u32,
    /// Start, ms from the meeting start.
    pub start_ms: u64,
    /// End, ms from the meeting start.
    pub end_ms: u64,
    /// Speaker display name ("Me" / "Them" live, the diarised name in the final pass).
    pub speaker: String,
}

/// Why a partial cannot be re-cited.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReciteError {
    /// The partial cites a segment that is not among the live segments.
    UnknownSegment(EntityId),
    /// A cited live segment overlaps no final segment on its track.
    NoOverlap(EntityId),
}

impl fmt::Display for ReciteError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ReciteError::UnknownSegment(id) => write!(f, "cites unknown live segment {id}"),
            ReciteError::NoOverlap(id) => {
                write!(
                    f,
                    "live segment {id} overlaps no final segment on its track"
                )
            }
        }
    }
}

impl std::error::Error for ReciteError {}

/// A re-cited partial.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Recited {
    /// The partial with final segment ids and owners.
    pub summary: SummaryV1,
    /// Action items whose label owner became a final speaker.
    pub owners_renamed: usize,
    /// Action items whose owner is a live label that could not be renamed (their re-cited
    /// segments have more than one speaker).
    pub owners_kept: usize,
}

/// Moves `partial` (citing `live` segments) onto the `fin` segments (module docs).
///
/// # Errors
/// [`ReciteError`] for a citation that cannot be moved.
pub fn recite(
    partial: &SummaryV1,
    live: &[TimedSegment],
    fin: &[TimedSegment],
) -> Result<Recited, ReciteError> {
    let live_by_id: BTreeMap<EntityId, &TimedSegment> = live.iter().map(|s| (s.id, s)).collect();
    let labels: BTreeSet<&str> = live.iter().map(|s| s.speaker.as_str()).collect();
    let fin_by_id: BTreeMap<EntityId, &TimedSegment> = fin.iter().map(|s| (s.id, s)).collect();
    let mut cache: BTreeMap<EntityId, Vec<(EntityId, u64)>> = BTreeMap::new();
    let mut moved = |ids: &[EntityId]| -> Result<Vec<EntityId>, ReciteError> {
        let mut overlap: BTreeMap<EntityId, u64> = BTreeMap::new();
        for id in ids {
            if !cache.contains_key(id) {
                let l = live_by_id.get(id).ok_or(ReciteError::UnknownSegment(*id))?;
                let hits: Vec<(EntityId, u64)> = fin
                    .iter()
                    .filter(|f| f.track == l.track)
                    .filter_map(|f| {
                        let o = l
                            .end_ms
                            .min(f.end_ms)
                            .saturating_sub(l.start_ms.max(f.start_ms));
                        (o > 0).then_some((f.id, o))
                    })
                    .collect();
                if hits.is_empty() {
                    return Err(ReciteError::NoOverlap(*id));
                }
                cache.insert(*id, hits);
            }
            for (f, o) in &cache[id] {
                *overlap.entry(*f).or_default() += o;
            }
        }
        let mut ranked: Vec<(EntityId, u64)> = overlap.into_iter().collect();
        ranked.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        ranked.truncate(MAX_CITATIONS);
        let mut kept: Vec<EntityId> = ranked.into_iter().map(|(id, _)| id).collect();
        kept.sort_by_key(|id| (fin_by_id[id].start_ms, *id));
        Ok(kept)
    };

    let mut summary = partial.clone();
    for section in &mut summary.enhanced_notes {
        for b in &mut section.bullets {
            b.source_segment_ids = moved(&b.source_segment_ids)?;
        }
    }
    for d in &mut summary.decisions {
        d.source_segment_ids = moved(&d.source_segment_ids)?;
    }
    let (mut owners_renamed, mut owners_kept) = (0, 0);
    for a in &mut summary.action_items {
        a.source_segment_ids = moved(&a.source_segment_ids)?;
        if labels.contains(a.owner.as_str()) {
            let speakers: BTreeSet<&str> = a
                .source_segment_ids
                .iter()
                .map(|id| fin_by_id[id].speaker.as_str())
                .collect();
            match speakers.into_iter().collect::<Vec<_>>()[..] {
                [one] => {
                    if one != a.owner {
                        a.owner = one.to_owned();
                    }
                    owners_renamed += 1;
                }
                _ => owners_kept += 1,
            }
        }
    }
    Ok(Recited {
        summary,
        owners_renamed,
        owners_kept,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::summary::{ActionItem, Decision, NoteBullet, NoteSection};

    fn id(n: u64) -> EntityId {
        format!("0193a5b0-1c00-7000-8000-{n:012x}").parse().unwrap()
    }

    fn seg(n: u64, track: u32, start: u64, end: u64, speaker: &str) -> TimedSegment {
        TimedSegment {
            id: id(n),
            track,
            start_ms: start,
            end_ms: end,
            speaker: speaker.into(),
        }
    }

    // Live: one mic segment (Me) and one long loopback segment (Them) that the final
    // pass split between two people.
    fn live() -> Vec<TimedSegment> {
        vec![seg(1, 0, 0, 4_000, "Me"), seg(2, 1, 4_000, 20_000, "Them")]
    }

    fn fin() -> Vec<TimedSegment> {
        vec![
            seg(101, 0, 0, 4_000, "Me"),
            seg(102, 1, 4_000, 9_000, "Alice"),
            seg(103, 1, 9_000, 20_000, "Bob"),
            // Same time as the loopback, other track: never a candidate for live 2.
            seg(104, 0, 10_000, 12_000, "Me"),
        ]
    }

    fn action(owner: &str, ids: &[u64]) -> ActionItem {
        ActionItem {
            owner: owner.into(),
            task: "send the report".into(),
            due: None,
            source_segment_ids: ids.iter().map(|n| id(*n)).collect(),
        }
    }

    fn partial() -> SummaryV1 {
        SummaryV1 {
            title: "Sync".into(),
            tldr: "Them agreed to send it.".into(),
            enhanced_notes: vec![NoteSection {
                heading: "Report".into(),
                bullets: vec![
                    NoteBullet {
                        text: "Report is late".into(),
                        source_segment_ids: vec![id(2)],
                    },
                    NoteBullet {
                        text: "From my notes".into(),
                        source_segment_ids: vec![],
                    },
                ],
            }],
            decisions: vec![Decision {
                text: "Send on Friday".into(),
                source_segment_ids: vec![id(1), id(2)],
            }],
            action_items: vec![action("Me", &[1]), action("Them", &[2])],
            open_questions: vec![],
            topics: vec!["report".into()],
        }
    }

    #[test]
    fn summary_recite_moves_citations_onto_overlapping_final_segments_on_the_same_track() {
        let r = recite(&partial(), &live(), &fin()).unwrap();
        // The long Them segment maps to both final segments it overlaps, never to the mic
        // segment at the same time, in meeting order.
        assert_eq!(
            r.summary.enhanced_notes[0].bullets[0].source_segment_ids,
            [id(102), id(103)]
        );
        assert!(
            r.summary.enhanced_notes[0].bullets[1]
                .source_segment_ids
                .is_empty()
        );
        assert_eq!(
            r.summary.decisions[0].source_segment_ids,
            [id(101), id(102), id(103)]
        );
        // Free text is kept as written.
        assert_eq!(r.summary.tldr, "Them agreed to send it.");
    }

    #[test]
    fn summary_recite_renames_label_owners_only_when_one_final_speaker_owns_the_citations() {
        let r = recite(&partial(), &live(), &fin()).unwrap();
        assert_eq!(r.summary.action_items[0].owner, "Me");
        // Live 2 is Alice and Bob: the owner stays a label and is counted.
        assert_eq!(r.summary.action_items[1].owner, "Them");
        assert_eq!((r.owners_renamed, r.owners_kept), (1, 1));

        // Only Bob's part overlaps a shorter live segment: renamed to Bob.
        let live = vec![seg(1, 0, 0, 4_000, "Me"), seg(2, 1, 10_000, 20_000, "Them")];
        let mut p = partial();
        p.action_items = vec![action("Them", &[2])];
        p.decisions.clear();
        p.enhanced_notes.clear();
        let r = recite(&p, &live, &fin()).unwrap();
        assert_eq!(r.summary.action_items[0].owner, "Bob");
        assert_eq!(r.summary.action_items[0].source_segment_ids, [id(103)]);
        assert_eq!((r.owners_renamed, r.owners_kept), (1, 0));

        // An owner that is not a live label (the model wrote a name) is left alone.
        p.action_items = vec![action("Unassigned", &[2])];
        let r = recite(&p, &live, &fin()).unwrap();
        assert_eq!(r.summary.action_items[0].owner, "Unassigned");
        assert_eq!((r.owners_renamed, r.owners_kept), (0, 0));
    }

    #[test]
    fn summary_recite_keeps_at_most_max_citations_with_the_most_overlap() {
        // One live segment over five final ones of different lengths.
        let live = vec![seg(1, 1, 0, 50_000, "Them")];
        let fin: Vec<TimedSegment> = [
            (0, 2_000),
            (2_000, 20_000),
            (20_000, 21_000),
            (21_000, 40_000),
            (40_000, 50_000),
        ]
        .iter()
        .enumerate()
        .map(|(i, (s, e))| seg(200 + i as u64, 1, *s, *e, "Alice"))
        .collect();
        let p = SummaryV1 {
            decisions: vec![Decision {
                text: "x".into(),
                source_segment_ids: vec![id(1)],
            }],
            ..SummaryV1::default()
        };
        let r = recite(&p, &live, &fin).unwrap();
        // The expectation below assumes three citations at most.
        const _: () = assert!(MAX_CITATIONS == 3);
        assert_eq!(
            r.summary.decisions[0].source_segment_ids,
            [id(201), id(203), id(204)]
        );
    }

    #[test]
    fn summary_recite_fails_on_an_unknown_or_unmatched_live_segment() {
        let mut p = partial();
        p.decisions[0].source_segment_ids = vec![id(9)];
        assert_eq!(
            recite(&p, &live(), &fin()),
            Err(ReciteError::UnknownSegment(id(9)))
        );
        // The final pass has nothing on the loopback track where live 2 was.
        let fin: Vec<TimedSegment> = fin().into_iter().filter(|s| s.track == 0).collect();
        assert_eq!(
            recite(&partial(), &live(), &fin),
            Err(ReciteError::NoOverlap(id(2)))
        );
        // Touching end to start is not an overlap.
        let fin = vec![
            seg(101, 0, 4_000, 8_000, "Me"),
            seg(102, 1, 20_000, 30_000, "A"),
        ];
        assert_eq!(
            recite(&partial(), &live(), &fin),
            Err(ReciteError::NoOverlap(id(2)))
        );
    }
}
