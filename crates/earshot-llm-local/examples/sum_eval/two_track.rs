//! Two-track chunk summaries (T8.3ja): option A of `docs/live-summaries.md` measured on the
//! AMI reference transcript. Rules fixed before measuring:
//!
//! - the two tracks: the Project Manager is "Me" (the user who records the meeting; every
//!   AMI scenario meeting has one), the other three participants are "Them";
//! - live segments: a track's turns joined while each turn starts at most [`JOIN_GAP_MS`]
//!   after the joined segment ends and the joined segment spans at most [`JOIN_MAX_MS`] (the
//!   turn rule of `ami.rs` applied per track, as Earshot cuts its live stretches); text in
//!   start order; ids of their own;
//! - the live meeting header as during a meeting: no title, no duration, no attendees, no
//!   notes;
//! - every 600 s window before the last is mapped from the live transcript so far (the
//!   segments up to that window, `map_last_chunk`), re-cited onto the reference transcript
//!   (`earshot_core::summary::recite`) and reused for that window; a failed map or re-cite
//!   is redone from the reference transcript at hang-up; the last window and the reduce run
//!   at hang-up, as in production.

use std::ops::Range;
use std::time::Duration;

use earshot_core::asr::MeetingTime;
use earshot_core::identity::EntityId;
use earshot_core::prompt::{MeetingInfo, PromptSegment, SummaryInput, time_chunks};
use earshot_core::summary::{SummaryV1, TimedSegment, recite};
use earshot_llm_local::summarize::{
    Generate, SummaryFailure, SummaryOptions, SummaryReport, map_last_chunk, summarize_chunks_with,
};
use serde_json::{Value, json};

/// The speaker who is "Me" (module docs).
pub const ME_ROLE: &str = "Project Manager";
/// Live track labels.
pub const ME: &str = "Me";
/// Live track labels.
pub const THEM: &str = "Them";
/// Longest pause inside one live segment (ms).
pub const JOIN_GAP_MS: u64 = 1_000;
/// Longest live segment (ms).
pub const JOIN_MAX_MS: u64 = 30_000;

/// A live segment id (a namespace apart from the reference turns' ids).
fn live_id(n: usize) -> EntityId {
    format!("0193a5b0-1c00-7000-9000-{n:012x}")
        .parse()
        .expect("a valid id")
}

/// The two-track copy of a meeting.
#[derive(Debug, Clone)]
pub struct TwoTrack {
    /// The live transcript as a summary input.
    pub live: SummaryInput,
    /// The live segments with times and tracks.
    pub live_timed: Vec<TimedSegment>,
    /// The reference segments with times and tracks.
    pub fin_timed: Vec<TimedSegment>,
}

/// The two-track copy of `input` (module docs); `ends_ms[i]` is the end of segment `i`.
#[must_use]
pub fn two_track(input: &SummaryInput, ends_ms: &[u64], me: &str) -> TwoTrack {
    assert_eq!(ends_ms.len(), input.segments.len(), "one end per segment");
    let track = |speaker: &str| u32::from(speaker != me);
    let fin_timed: Vec<TimedSegment> = input
        .segments
        .iter()
        .zip(ends_ms)
        .map(|(s, &end)| TimedSegment {
            id: s.id,
            track: track(&s.speaker),
            start_ms: s.start.as_millis(),
            end_ms: end.max(s.start.as_millis()),
            speaker: s.speaker.clone(),
        })
        .collect();
    // (start, end, track, text) per live segment; one open segment per track.
    let mut closed: Vec<(u64, u64, u32, String)> = Vec::new();
    let mut open: [Option<(u64, u64, u32, String)>; 2] = [None, None];
    for (s, t) in input.segments.iter().zip(&fin_timed) {
        let slot = &mut open[t.track as usize];
        match slot {
            Some(o) if t.start_ms <= o.1 + JOIN_GAP_MS && t.end_ms - o.0 <= JOIN_MAX_MS => {
                o.1 = o.1.max(t.end_ms);
                o.3.push(' ');
                o.3.push_str(&s.text);
            }
            _ => {
                if let Some(o) = slot.take() {
                    closed.push(o);
                }
                *slot = Some((t.start_ms, t.end_ms, t.track, s.text.clone()));
            }
        }
    }
    closed.extend(open.into_iter().flatten());
    closed.sort_by_key(|c| (c.0, c.2));
    let label = |track: u32| if track == 0 { ME } else { THEM };
    let live_timed: Vec<TimedSegment> = closed
        .iter()
        .enumerate()
        .map(|(i, c)| TimedSegment {
            id: live_id(i + 1),
            track: c.2,
            start_ms: c.0,
            end_ms: c.1,
            speaker: label(c.2).to_owned(),
        })
        .collect();
    let live = SummaryInput {
        meeting: MeetingInfo {
            title: None,
            started: None,
            duration_ms: 0,
            attendees: Vec::new(),
        },
        segments: closed
            .iter()
            .zip(&live_timed)
            .map(|(c, t)| PromptSegment {
                id: t.id,
                start: MeetingTime::from_millis(c.0),
                speaker: t.speaker.clone(),
                text: c.3.clone(),
            })
            .collect(),
        notes: Vec::new(),
        template: input.template,
        prompt_variant: input.prompt_variant,
        rules: input.rules,
        style: input.style.clone(),
        style_preset: input.style_preset,
        vocabulary: input.vocabulary.clone(),
        user_name: None,
    };
    TwoTrack {
        live,
        live_timed,
        fin_timed,
    }
}

/// What happened to the windows before the last one.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Counts {
    /// Windows mapped from the live transcript.
    pub live_maps: usize,
    /// Live maps that failed (redone at hang-up).
    pub map_failed: usize,
    /// Live partials that could not be re-cited (redone at hang-up).
    pub recite_failed: usize,
    /// Re-cited partials with no reference window to go to (dropped; the window is
    /// redone).
    pub unplaced: usize,
    /// Re-cited partials reused.
    pub reused: usize,
    /// Action items whose label owner became a reference speaker.
    pub owners_renamed: usize,
    /// Action items left with a label owner.
    pub owners_kept: usize,
}

/// Runs the two-track flow (module docs) on `input` with windows of `seconds`: the summary
/// (or the failure), the report of the hang-up run, the live steps' time and the counts.
///
/// # Errors
/// A transcript that cannot be cut into windows.
pub fn summarise(
    model: &mut dyn Generate,
    input: &SummaryInput,
    ends_ms: &[u64],
    seconds: u32,
    options: &SummaryOptions,
) -> Result<
    (
        Result<SummaryV1, SummaryFailure>,
        SummaryReport,
        Duration,
        Counts,
    ),
    String,
> {
    let tt = two_track(input, ends_ms, ME_ROLE);
    let fin_ranges = time_chunks(input, seconds).map_err(|e| e.to_string())?;
    let live_ranges = time_chunks(&tt.live, seconds).map_err(|e| e.to_string())?;
    let window = |inp: &SummaryInput, r: &Range<usize>| {
        inp.segments[r.start].start.as_millis() / (u64::from(seconds) * 1000)
    };
    let last = fin_ranges.last().map_or(0, |r| window(input, r));
    let mut done: Vec<Option<SummaryV1>> = vec![None; fin_ranges.len()];
    let mut counts = Counts::default();
    let mut live_time = Duration::ZERO;
    if fin_ranges.len() > 1 {
        for (i, r) in live_ranges.iter().enumerate() {
            let k = window(&tt.live, r);
            if k >= last {
                break;
            }
            let so_far = SummaryInput {
                segments: tt.live.segments[..r.end].to_vec(),
                ..tt.live.clone()
            };
            let run = map_last_chunk(model, &so_far, options, &live_ranges[..=i]);
            live_time += run.report.steps.iter().map(|s| s.wall).sum::<Duration>();
            counts.live_maps += 1;
            let Ok(partial) = run.result else {
                counts.map_failed += 1;
                continue;
            };
            let Ok(r) = recite(&partial, &tt.live_timed, &tt.fin_timed) else {
                counts.recite_failed += 1;
                continue;
            };
            match fin_ranges.iter().position(|f| window(input, f) == k) {
                Some(j) => {
                    done[j] = Some(r.summary);
                    counts.reused += 1;
                    counts.owners_renamed += r.owners_renamed;
                    counts.owners_kept += r.owners_kept;
                }
                None => counts.unplaced += 1,
            }
        }
    }
    let run = summarize_chunks_with(model, input, options, &fin_ranges, done);
    Ok((run.result, run.report, live_time, counts))
}

/// The two-track part of a meeting's report.
#[must_use]
pub fn report_json(report: &SummaryReport, live_time: Duration, counts: &Counts) -> Value {
    let hangup: f64 = report.steps.iter().map(|s| s.wall.as_secs_f64()).sum();
    json!({
        "live_maps": counts.live_maps,
        "live_map_s": live_time.as_secs_f64(),
        "map_failed": counts.map_failed,
        "recite_failed": counts.recite_failed,
        "unplaced": counts.unplaced,
        "reused": counts.reused,
        "redone": report.map_parts.saturating_sub(report.reused_parts + 1),
        "owners_renamed": counts.owners_renamed,
        "owners_kept": counts.owners_kept,
        "hangup_s": hangup,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use earshot_core::prompt::{PromptVariant, Template};

    fn id(n: usize) -> EntityId {
        format!("0193a5b0-1c00-7000-8000-{n:012x}").parse().unwrap()
    }

    fn input(turns: &[(u64, &str, &str)]) -> SummaryInput {
        SummaryInput {
            meeting: MeetingInfo {
                title: Some("t".into()),
                started: None,
                duration_ms: 99_000,
                attendees: vec!["Project Manager".into(), "Marketing Expert".into()],
            },
            segments: turns
                .iter()
                .enumerate()
                .map(|(i, (s, who, text))| PromptSegment {
                    id: id(i + 1),
                    start: MeetingTime::from_millis(*s),
                    speaker: (*who).into(),
                    text: (*text).into(),
                })
                .collect(),
            notes: Vec::new(),
            template: Template::General,
            prompt_variant: PromptVariant::Current,
            rules: Default::default(),
            style: Default::default(),
            style_preset: Default::default(),
            vocabulary: Default::default(),
            user_name: None,
        }
    }

    #[test]
    fn sum_eval_two_track_joins_each_tracks_turns_and_hides_names() {
        let pm = ME_ROLE;
        let inp = input(&[
            (0, pm, "hello"),
            (1_000, "Marketing Expert", "hi"),
            (2_500, "User Interface Designer", "morning"),
            (3_000, pm, "agenda"),
            // A pause above JOIN_GAP_MS on the Them track: a new live segment.
            (6_000, "Marketing Expert", "first"),
            (40_000, pm, "later"),
        ]);
        let ends = [800, 2_000, 3_200, 3_900, 7_000, 41_000];
        let tt = two_track(&inp, &ends, pm);
        let live: Vec<(u64, u64, &str, &str)> = tt
            .live
            .segments
            .iter()
            .zip(&tt.live_timed)
            .map(|(s, t)| (t.start_ms, t.end_ms, s.speaker.as_str(), s.text.as_str()))
            .collect();
        assert_eq!(
            live,
            [
                // Me: 0-800 and 3000-3900 are 2.2 s apart: two segments.
                (0, 800, "Me", "hello"),
                // Them: 1000-2000 and 2500-3200 join (0.5 s apart), 6000 starts anew.
                (1_000, 3_200, "Them", "hi morning"),
                (3_000, 3_900, "Me", "agenda"),
                (6_000, 7_000, "Them", "first"),
                (40_000, 41_000, "Me", "later"),
            ]
        );
        // As during a meeting: no names, duration or title; ids of their own.
        assert!(tt.live.meeting.attendees.is_empty());
        assert_eq!(
            (
                tt.live.meeting.duration_ms,
                tt.live.meeting.title.as_deref()
            ),
            (0, None)
        );
        assert!(
            tt.live
                .segments
                .iter()
                .all(|s| !inp.segment_ids().contains(&s.id))
        );
        // The reference keeps its speakers, on the right tracks.
        assert_eq!(tt.fin_timed[1].speaker, "Marketing Expert");
        assert_eq!(
            tt.fin_timed.iter().map(|t| t.track).collect::<Vec<_>>(),
            [0, 1, 1, 0, 1, 0]
        );
    }

    #[test]
    fn sum_eval_two_track_caps_a_joined_segment_at_thirty_seconds() {
        let inp = input(&[
            (0, "Marketing Expert", "a"),
            (10_000, "Marketing Expert", "b"),
            (20_000, "Marketing Expert", "c"),
            (29_000, "Marketing Expert", "d"),
        ]);
        let ends = [10_000, 20_000, 29_000, 31_000];
        let tt = two_track(&inp, &ends, ME_ROLE);
        let spans: Vec<(u64, u64)> = tt
            .live_timed
            .iter()
            .map(|t| (t.start_ms, t.end_ms))
            .collect();
        assert_eq!(spans, [(0, 29_000), (29_000, 31_000)]);
    }
}
