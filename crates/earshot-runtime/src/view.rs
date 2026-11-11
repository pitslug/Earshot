//! A meeting as summaries and exports see it: the live (not tombstoned) rows, with the
//! final transcript preferred over the live draft.

use earshot_core::asr::Segment;
use earshot_core::capture::TrackId;
use earshot_core::diar::SpeakerRole;
use std::collections::{BTreeMap, BTreeSet};

use earshot_core::identity::{EntityId, HlcTimestamp};
use earshot_core::model::{
    Meeting, MeetingRepo, Note, RepoError, Speaker, Summary, Track, TranscriptPass, live_segments,
};
use earshot_core::voiceprint::DEFAULT_ME_NAME;

/// The name of a segment without a speaker on `track`: "Me" for the mic, "Them" for the
/// loopback (PLAN.md 3.3).
#[must_use]
pub fn track_label(track: TrackId) -> &'static str {
    match track {
        TrackId::Mic => "Me",
        TrackId::Reference => "Them",
    }
}

/// One meeting's current content.
#[derive(Debug, Clone, PartialEq)]
pub struct MeetingView {
    /// The meeting row.
    pub meeting: Meeting,
    /// Its tracks.
    pub tracks: Vec<Track>,
    /// Live speakers, in creation order.
    pub speakers: Vec<Speaker>,
    /// The transcript in meeting order: the final pass when it exists, else the live
    /// draft.
    pub segments: Vec<Segment>,
    /// Which pass `segments` came from (`None` without any transcript).
    pub pass: Option<TranscriptPass>,
    /// The segments of `segments` whose text the user edited (T12.16), with when
    /// (`SegmentRow::text_edited_at`).
    pub edited: BTreeMap<EntityId, HlcTimestamp>,
    /// The user's notes in meeting order.
    pub notes: Vec<Note>,
    /// The current summary: the most recently updated live one.
    pub summary: Option<Summary>,
}

impl MeetingView {
    /// Loads `meeting`; `None` if it does not exist or is deleted.
    ///
    /// # Errors
    /// Storage errors.
    pub fn load<R: MeetingRepo + ?Sized>(
        repo: &R,
        meeting: EntityId,
    ) -> Result<Option<Self>, RepoError> {
        let Some(m) = repo.meeting(meeting)?.filter(|m| !m.meta.deleted) else {
            return Ok(None);
        };
        let all_tracks = repo.tracks(meeting)?;
        let all_speakers = repo.speakers(meeting)?;
        let all_notes = repo.notes(meeting)?;
        let all_summaries = repo.summaries(meeting)?;
        let tracks: Vec<Track> = all_tracks.into_iter().filter(|t| !t.meta.deleted).collect();
        let speakers: Vec<Speaker> = all_speakers
            .into_iter()
            .filter(|s| !s.meta.deleted)
            .collect();
        let mut pass = None;
        let mut segments = Vec::new();
        let mut edited = BTreeMap::new();
        for p in [TranscriptPass::Final, TranscriptPass::Live] {
            let rows = live_segments(repo, meeting, p)?;
            if !rows.is_empty() {
                pass = Some(p);
                edited = rows
                    .iter()
                    .filter_map(|r| r.text_edited_at.map(|t| (r.segment.id, t)))
                    .collect();
                segments = rows.into_iter().map(|r| r.segment).collect();
                break;
            }
        }
        segments.sort_by_key(|s| (s.start, s.id));
        let mut notes: Vec<Note> = all_notes.into_iter().filter(|n| !n.meta.deleted).collect();
        notes.sort_by_key(|n| (n.at, n.id));
        let summary = all_summaries
            .into_iter()
            .filter(|s| !s.meta.deleted)
            .max_by_key(|s| s.meta.updated_at);
        Ok(Some(Self {
            meeting: m,
            tracks,
            speakers,
            segments,
            pass,
            edited,
            notes,
            summary,
        }))
    }

    /// Whether the current summary no longer matches the transcript, so the host offers to
    /// summarise again (nothing is sent by itself): it cites a segment the transcript no
    /// longer has (a re-transcription replaced it), or the user edited a segment's text after
    /// the summary was written (T12.16: an edit later than the summary's creation; a summary
    /// restored by an undo keeps its creation time, so an older one stays out of date).
    /// `false` without a summary.
    #[must_use]
    pub fn summary_out_of_date(&self) -> bool {
        let Some(summary) = &self.summary else {
            return false;
        };
        let ids: BTreeSet<EntityId> = self.segments.iter().map(|s| s.id).collect();
        !cites_only(&summary.summary, &ids)
            || self.edited.values().any(|at| *at > summary.meta.created_at)
    }

    /// The name shown for `segment`: its speaker's display name after diarisation,
    /// otherwise "Me" for the mic and "Them" for the loopback (PLAN.md 3.3).
    ///
    /// Names come from `Speaker.display_name` only, never from the linked `Person`: a
    /// person rename must rewrite the display name of that person's speaker rows (the
    /// rename command does this), so summaries and exports, which compare rendered
    /// content, pick it up.
    #[must_use]
    pub fn speaker_name(&self, segment: &Segment) -> String {
        if let Some(id) = segment.speaker_id
            && let Some((_, label)) = self.labels().into_iter().find(|(s, _)| *s == id)
        {
            return label;
        }
        self.tracks
            .iter()
            .find(|t| t.id == segment.track)
            .map_or_else(String::new, |t| track_label(t.kind).to_owned())
    }

    /// Attendee names: the speakers' display names, first appearance first, without
    /// duplicates.
    #[must_use]
    pub fn attendees(&self) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        for (_, label) in self.labels() {
            if !label.is_empty() && !out.contains(&label) {
                out.push(label);
            }
        }
        out
    }

    /// Each speaker row's name as this meeting shows it ([`distinct_names`]), in row order.
    #[must_use]
    pub fn labels(&self) -> Vec<(EntityId, String)> {
        let people: Vec<(EntityId, &str)> = self
            .speakers
            .iter()
            .map(|s| (s.person_id.unwrap_or(s.id), s.display_name.as_str()))
            .collect();
        self.speakers
            .iter()
            .map(|s| s.id)
            .zip(distinct_names(&people))
            .collect()
    }

    /// How the user appears in this meeting once they have given a name (`None`: not in it,
    /// or still the default, T9.1h).
    #[must_use]
    pub fn user_name(&self) -> Option<String> {
        let me = self
            .speakers
            .iter()
            .find(|s| s.role == SpeakerRole::Me && s.display_name != DEFAULT_ME_NAME)?;
        self.labels()
            .into_iter()
            .find(|(id, _)| *id == me.id)
            .map(|(_, label)| label)
    }
}

/// Names that tell people apart within one meeting (T9.1h), for `people` as
/// (who, stored name) in order; one row per entry, and entries with one `who` are one person.
/// A first name only one person has keeps the name as stored. People sharing a first name
/// (ASCII case ignored) get the first name and last initial when that separates them ("Casey
/// P"), else a number in order of appearance ("Casey (1)"). Stored names never change.
#[must_use]
pub fn distinct_names(people: &[(EntityId, &str)]) -> Vec<String> {
    let first = |n: &str| {
        n.split_whitespace()
            .next()
            .unwrap_or("")
            .to_ascii_lowercase()
    };
    let initialled = |n: &str| -> Option<String> {
        let mut words = n.split_whitespace();
        let first = words.next()?;
        let initial = words.next_back()?.chars().next()?;
        Some(format!("{first} {}", initial.to_uppercase()))
    };
    let mut who: Vec<(EntityId, &str)> = Vec::new();
    for &(id, name) in people {
        if !who.iter().any(|(w, _)| *w == id) {
            who.push((id, name));
        }
    }
    people
        .iter()
        .map(|&(id, name)| {
            let (_, name) = *who.iter().find(|(w, _)| *w == id).unwrap_or(&(id, name));
            let group: Vec<&str> = who
                .iter()
                .filter(|(_, n)| !first(n).is_empty() && first(n) == first(name))
                .map(|(_, n)| *n)
                .collect();
            if group.len() < 2 {
                return name.to_owned();
            }
            let unique = |candidate: &String| {
                group
                    .iter()
                    .filter(|n| initialled(n).as_ref() == Some(candidate))
                    .count()
                    == 1
            };
            if let Some(label) = initialled(name).filter(unique) {
                return label;
            }
            // Numbered among those the initial does not separate.
            let numbered: Vec<&&str> = group
                .iter()
                .filter(|n| !initialled(n).is_some_and(|c| unique(&c)))
                .collect();
            let at = who
                .iter()
                .filter(|(_, n)| first(n) == first(name))
                .filter(|(_, n)| numbered.contains(&n))
                .position(|(w, _)| *w == id)
                .unwrap_or(0);
            let shown = name.split_whitespace().next().unwrap_or(name);
            format!("{shown} ({})", at + 1)
        })
        .collect()
}

/// `unix_ms` shifted by `utc_offset_minutes`, as `YYYY-MM-DD HH:MM` (proleptic Gregorian;
/// days-to-civil after H. Hinnant, "chrono-Compatible Low-Level Date Algorithms").
#[must_use]
pub fn format_local(unix_ms: u64, utc_offset_minutes: i32) -> String {
    let minutes =
        i64::try_from(unix_ms / 60_000).unwrap_or(i64::MAX) + i64::from(utc_offset_minutes);
    let (days, minute_of_day) = (minutes.div_euclid(1440), minutes.rem_euclid(1440));
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02} {:02}:{:02}",
        minute_of_day / 60,
        minute_of_day % 60
    )
}

/// Whether every segment `summary` cites (note bullets, decisions, action items) is in `ids`.
/// Only the citations are checked: the summary passed validation when it was stored.
#[must_use]
pub fn cites_only(summary: &earshot_core::summary::SummaryV1, ids: &BTreeSet<EntityId>) -> bool {
    summary
        .enhanced_notes
        .iter()
        .flat_map(|n| &n.bullets)
        .flat_map(|b| &b.source_segment_ids)
        .chain(summary.decisions.iter().flat_map(|d| &d.source_segment_ids))
        .chain(
            summary
                .action_items
                .iter()
                .flat_map(|a| &a.source_segment_ids),
        )
        .all(|id| ids.contains(id))
}

#[cfg(test)]
mod tests {
    use super::format_local;

    #[test]
    fn summary_and_export_local_start_time_formatting() {
        assert_eq!(format_local(0, 0), "1970-01-01 00:00");
        // 2026-10-18 03:40 UTC, then with a local offset.
        assert_eq!(format_local(1_792_294_800_000, 0), "2026-10-18 03:40");
        assert_eq!(format_local(1_792_294_800_000, 600), "2026-10-18 13:40");
        assert_eq!(format_local(1_792_294_800_000, -300), "2026-10-17 22:40");
        // Leap day.
        assert_eq!(format_local(951_782_400_000, 0), "2000-02-29 00:00");
    }
}

#[cfg(test)]
mod name_tests {
    use super::*;

    fn id(n: u8) -> EntityId {
        EntityId::from_bytes([n; 16])
    }

    fn names(people: &[(u8, &str)]) -> Vec<String> {
        let people: Vec<(EntityId, &str)> = people.iter().map(|(n, s)| (id(*n), *s)).collect();
        distinct_names(&people)
    }

    #[test]
    fn view_a_first_name_one_person_has_keeps_the_stored_name() {
        assert_eq!(
            names(&[
                (1, "Casey Parker"),
                (2, "Robin"),
                (3, "Speaker 1"),
                (4, "Speaker 2")
            ]),
            ["Casey Parker", "Robin", "Speaker 1", "Speaker 2"]
        );
        assert!(names(&[]).is_empty());
    }

    #[test]
    fn view_people_sharing_a_first_name_get_a_last_initial() {
        assert_eq!(
            names(&[(1, "Casey Parker"), (2, "Casey Mason"), (3, "Robin")]),
            ["Casey P", "Casey M", "Robin"]
        );
        // Case does not separate first names, and the last word gives the initial.
        assert_eq!(
            names(&[(1, "casey van dyk"), (2, "Casey Mason")]),
            ["casey D", "Casey M"]
        );
    }

    #[test]
    fn view_without_a_separating_initial_people_are_numbered_in_order() {
        assert_eq!(
            names(&[(1, "Casey"), (2, "Casey")]),
            ["Casey (1)", "Casey (2)"]
        );
        assert_eq!(
            names(&[(1, "Casey Parker"), (2, "Casey Pan")]),
            ["Casey (1)", "Casey (2)"]
        );
        // One with a last name keeps the initial; the one without is numbered.
        assert_eq!(
            names(&[(1, "Casey"), (2, "Casey Mason")]),
            ["Casey (1)", "Casey M"]
        );
    }

    #[test]
    fn view_one_person_on_several_rows_is_one_name() {
        assert_eq!(names(&[(1, "Casey"), (1, "Casey")]), ["Casey", "Casey"]);
        assert_eq!(
            names(&[(1, "Casey"), (2, "Casey"), (1, "Casey")]),
            ["Casey (1)", "Casey (2)", "Casey (1)"]
        );
    }
}
