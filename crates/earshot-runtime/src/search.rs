//! Finding meetings by their text (T9.7), on this computer only.
//!
//! What is searched is what a meeting shows ([`MeetingView`]): its title, its transcript (the
//! final pass, else the live draft) and its current summary. Tombstoned rows hold replaced text
//! and are never read. There is no index: each search reads the meetings newest first, one short
//! store lock per meeting, and matches with the lock free. The text searched for is never logged,
//! stored or put in an error.

use earshot_core::identity::EntityId;
use earshot_core::model::MeetingRepo;
use earshot_core::summary::SummaryV1;

use crate::RuntimeError;
use crate::services::Services;
use crate::view::MeetingView;

/// The most meetings one search returns.
pub const MAX_RESULTS: usize = 50;

/// Characters a snippet keeps at most before the match.
pub const SNIPPET_BEFORE: usize = 30;

/// Characters a snippet keeps at most after the match.
pub const SNIPPET_AFTER: usize = 70;

/// What marks a snippet cut short.
const ELLIPSIS: &str = "...";

/// Where a meeting matched. A meeting that matches in several places gives the first of title,
/// transcript, summary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MatchedIn {
    /// The meeting's title.
    Title,
    /// A line of the transcript the meeting shows.
    Transcript,
    /// The meeting's current summary.
    Summary,
}

/// One meeting found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchHit {
    /// The meeting.
    pub meeting: EntityId,
    /// Its title; empty until the user or a summary gives one.
    pub title: String,
    /// Its start (Unix ms).
    pub started_unix_ms: u64,
    /// Where it matched.
    pub matched: MatchedIn,
    /// The text around the first match, on one line.
    pub snippet: String,
    /// When the matching line starts (ms from the meeting's start); only for a transcript match.
    pub line_ms: Option<u64>,
}

/// What a search found.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SearchResults {
    /// The meetings found, newest first, at most [`MAX_RESULTS`].
    pub hits: Vec<SearchHit>,
    /// Whether more meetings matched than `hits` holds.
    pub capped: bool,
}

/// The live meetings whose title, transcript or summary contains `query`: literal text, case
/// ignored, surrounding white space ignored. An empty query finds nothing.
///
/// # Errors
/// Storage errors.
pub fn search_meetings(services: &Services, query: &str) -> Result<SearchResults, RuntimeError> {
    let query = Query::new(query.trim());
    let mut results = SearchResults::default();
    if query.is_empty() {
        return Ok(results);
    }
    let mut meetings = services.store().meetings()?;
    meetings.retain(|m| !m.meta.deleted);
    meetings.sort_by_key(|m| std::cmp::Reverse((m.started_unix_ms, m.id)));
    for meeting in meetings {
        // One short lock per meeting; matching runs without it. `None`: deleted since the read
        // above.
        let Some(view) = MeetingView::load(&**services.store(), meeting.id)? else {
            continue;
        };
        let Some(hit) = hit_in(&view, &query) else {
            continue;
        };
        if results.hits.len() == MAX_RESULTS {
            results.capped = true;
            break;
        }
        results.hits.push(hit);
    }
    Ok(results)
}

/// The first match in `view`, if any.
fn hit_in(view: &MeetingView, query: &Query) -> Option<SearchHit> {
    let (matched, snippet, line_ms) = if let Some(snippet) = query.snippet(&view.meeting.title) {
        (MatchedIn::Title, snippet, None)
    } else if let Some((snippet, at)) = view
        .segments
        .iter()
        .find_map(|s| Some((query.snippet(&s.text)?, s.start.as_millis())))
    {
        (MatchedIn::Transcript, snippet, Some(at))
    } else {
        let summary = &view.summary.as_ref()?.summary;
        let snippet = summary_texts(summary).find_map(|text| query.snippet(text))?;
        (MatchedIn::Summary, snippet, None)
    };
    Some(SearchHit {
        meeting: view.meeting.id,
        title: view.meeting.title.clone(),
        started_unix_ms: view.meeting.started_unix_ms,
        matched,
        snippet,
        line_ms,
    })
}

/// The summary's text as the meeting screen shows it, which leaves out its topics.
fn summary_texts(s: &SummaryV1) -> impl Iterator<Item = &str> {
    std::iter::once(s.title.as_str())
        .chain(std::iter::once(s.tldr.as_str()))
        .chain(s.decisions.iter().map(|d| d.text.as_str()))
        .chain(s.action_items.iter().flat_map(|a| {
            [a.task.as_str(), a.owner.as_str()]
                .into_iter()
                .chain(a.due.as_deref())
        }))
        .chain(s.enhanced_notes.iter().flat_map(|n| {
            std::iter::once(n.heading.as_str()).chain(n.bullets.iter().map(|b| b.text.as_str()))
        }))
        .chain(s.open_questions.iter().map(String::as_str))
}

/// Query text with its case folded ([`fold_case`]), one character at a time so a position in folded text can be
/// traced back to the text's own characters (some letters lower to two).
struct Query(Vec<char>);

/// `text` with its case folded one character at a time: the one case rule of search and of
/// folder names ([`crate::folders`]).
pub(crate) fn fold_case(text: &str) -> impl Iterator<Item = char> + '_ {
    text.chars().flat_map(char::to_lowercase)
}

impl Query {
    fn new(query: &str) -> Self {
        Self(fold_case(query).collect())
    }

    fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// The range of `text`'s characters holding the first match.
    fn position(&self, chars: &[char]) -> Option<(usize, usize)> {
        if self.is_empty() {
            return None;
        }
        let folded: Vec<(char, usize)> = chars
            .iter()
            .enumerate()
            .flat_map(|(at, c)| c.to_lowercase().map(move |l| (l, at)))
            .collect();
        let found = folded
            .windows(self.0.len())
            .find(|window| window.iter().map(|(c, _)| c).eq(self.0.iter()))?;
        Some((found.first()?.1, found.last()?.1 + 1))
    }

    /// The text around the first match in `text`, on one line.
    fn snippet(&self, text: &str) -> Option<String> {
        let chars: Vec<char> = text.chars().collect();
        let (start, end) = self.position(&chars)?;
        let from = cut_before(&chars, start);
        let to = cut_after(&chars, end);
        let window: String = chars[from..to].iter().collect();
        let line = window.split_whitespace().collect::<Vec<_>>().join(" ");
        let before = if from > 0 { ELLIPSIS } else { "" };
        let after = if to < chars.len() { ELLIPSIS } else { "" };
        Some(format!("{before}{line}{after}"))
    }
}

/// Where a snippet starts for a match at `start`: at most [`SNIPPET_BEFORE`] characters back, on
/// a word when there is one to stop at.
fn cut_before(chars: &[char], start: usize) -> usize {
    let from = start.saturating_sub(SNIPPET_BEFORE);
    if from == 0 || chars[from - 1].is_whitespace() {
        return from;
    }
    chars[from..start]
        .iter()
        .position(|c| c.is_whitespace())
        .map_or(from, |space| from + space + 1)
}

/// Where a snippet ends for a match ending at `end`: at most [`SNIPPET_AFTER`] characters on, on
/// a word when there is one to stop at.
fn cut_after(chars: &[char], end: usize) -> usize {
    let to = end.saturating_add(SNIPPET_AFTER).min(chars.len());
    if to == chars.len() || chars[to].is_whitespace() {
        return to;
    }
    chars[end..to]
        .iter()
        .rposition(|c| c.is_whitespace())
        .map_or(to, |space| end + space)
}

#[cfg(test)]
#[path = "search_tests.rs"]
mod tests;
