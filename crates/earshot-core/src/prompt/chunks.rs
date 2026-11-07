//! Time chunks of a transcript: the chunk rule of the T6.20 chunking experiment (fixed
//! before measuring, `docs/summary-eval.md`), used by local summaries (T6.21a) and the
//! `sum_eval` example.
//!
//! Window `k` is meeting time `[k * seconds, (k + 1) * seconds)` from the meeting start. A
//! transcript segment belongs to the window that holds its start time, so chunks are cut
//! only at segment boundaries. The chunks are the non-empty windows in order (the last one
//! is whatever is left at hang-up, however short). A meeting with one non-empty window is
//! one chunk: the ordinary unchunked request.

use std::fmt;
use std::ops::Range;

use super::SummaryInput;

/// Why [`time_chunks`] cannot cut a transcript.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimeChunkError {
    /// The window is zero seconds long.
    ZeroWindow,
    /// Segment `segment` (0-based) starts before the segment before it, so the windows
    /// would not be consecutive index ranges.
    OutOfOrder {
        /// The segment that starts too early.
        segment: usize,
    },
}

impl fmt::Display for TimeChunkError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TimeChunkError::ZeroWindow => f.write_str("the chunk window must be above 0 seconds"),
            TimeChunkError::OutOfOrder { segment } => {
                write!(f, "segment {} starts before segment {segment}", segment + 1)
            }
        }
    }
}

impl std::error::Error for TimeChunkError {}

/// The chunks of `input` for windows of `seconds` (module docs), as consecutive segment
/// index ranges that together cover every segment, ready for
/// [`plan_chunks`](super::plan_chunks). No segments give no chunks.
///
/// # Errors
/// [`TimeChunkError::ZeroWindow`] for `seconds == 0`; [`TimeChunkError::OutOfOrder`] when
/// the segments are not in meeting-time order.
pub fn time_chunks(
    input: &SummaryInput,
    seconds: u32,
) -> Result<Vec<Range<usize>>, TimeChunkError> {
    if seconds == 0 {
        return Err(TimeChunkError::ZeroWindow);
    }
    let window = u64::from(seconds) * 1000;
    let mut out: Vec<Range<usize>> = Vec::new();
    let mut current = None;
    let mut last = 0;
    for (i, s) in input.segments.iter().enumerate() {
        let ms = s.start.as_millis();
        if ms < last {
            return Err(TimeChunkError::OutOfOrder { segment: i });
        }
        last = ms;
        let k = ms / window;
        match out.last_mut() {
            Some(r) if current == Some(k) => r.end = i + 1,
            _ => {
                current = Some(k);
                out.push(i..i + 1);
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::asr::MeetingTime;
    use crate::prompt::{MeetingInfo, PromptSegment, Template};

    fn input(starts_s: &[u64]) -> SummaryInput {
        SummaryInput {
            meeting: MeetingInfo {
                title: None,
                started: None,
                duration_ms: 0,
                attendees: vec![],
            },
            segments: starts_s
                .iter()
                .enumerate()
                .map(|(i, s)| PromptSegment {
                    id: format!("0193a5b0-1c00-7000-8000-{:012x}", i + 1)
                        .parse()
                        .unwrap(),
                    start: MeetingTime::from_millis(s * 1000),
                    speaker: "A".into(),
                    text: "x".into(),
                })
                .collect(),
            notes: vec![],
            template: Template::General,
            prompt_variant: Default::default(),
            style: Default::default(),
            user_name: None,
        }
    }

    #[test]
    fn prompt_time_chunks_cut_at_the_first_segment_of_each_window() {
        // 599 s stays in window 0; 600 s opens window 1; window 2 is empty; 1800 s is 3.
        let i = input(&[0, 30, 599, 600, 1190, 1800, 1805]);
        assert_eq!(time_chunks(&i, 600).unwrap(), [0..3, 3..5, 5..7]);
        // The first segment need not start at 0.
        let late = time_chunks(&input(&[650, 700]), 600).unwrap();
        assert_eq!(late, std::slice::from_ref(&(0..2)));
        // Segments starting at the same time stay together.
        let same = time_chunks(&input(&[5, 5, 5]), 600).unwrap();
        assert_eq!(same, std::slice::from_ref(&(0..3)));
        assert_eq!(
            time_chunks(&input(&[]), 600).unwrap(),
            Vec::<Range<usize>>::new()
        );
    }

    #[test]
    fn prompt_time_chunks_refuse_a_zero_window_and_unordered_segments() {
        assert_eq!(
            time_chunks(&input(&[10, 5]), 600),
            Err(TimeChunkError::OutOfOrder { segment: 1 })
        );
        assert_eq!(
            time_chunks(&input(&[1]), 0),
            Err(TimeChunkError::ZeroWindow)
        );
    }

    #[test]
    fn prompt_time_chunks_are_what_plan_chunks_accepts() {
        let i = input(&[0, 10, 700, 1300, 1310]);
        let ranges = time_chunks(&i, 600).unwrap();
        let budget = crate::prompt::ContextBudget {
            context_window: 16_384,
            max_output_tokens: 8192,
        };
        let chunks = crate::prompt::plan_chunks(&i, budget, &ranges).unwrap();
        assert_eq!(
            chunks
                .iter()
                .map(|c| c.segments.clone())
                .collect::<Vec<_>>(),
            ranges
        );
    }
}
