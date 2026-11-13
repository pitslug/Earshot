//! Search across meetings (T9.7). Every text here is invented.

use std::sync::Arc;

use earshot_core::asr::{MeetingTime, Segment, Word};
use earshot_core::capture::TrackId;
use earshot_core::model::{ClockDomain, Meeting, SegmentRow, Stamp, Track, TranscriptPass};
use earshot_core::prompt::Template;
use earshot_core::summary::{ActionItem, Decision, NoteBullet, NoteSection};
use rand_chacha::ChaCha8Rng;
use rand_core::SeedableRng;

use super::*;
use crate::testutil::{T0, TempDir, services};

/// A meeting with one track and no text, started `started` ms after [`T0`].
fn meeting_at(svc: &Services, title: &str, started: u64, seed: u64) -> (EntityId, EntityId) {
    let clock = Arc::clone(&svc.clock);
    let mut rng = ChaCha8Rng::seed_from_u64(seed);
    let mut db = svc.store();
    let (store, hlc) = db.parts();
    let mut stamp = Stamp {
        clock: &*clock,
        hlc,
        rng: &mut rng,
    };
    let m = Meeting::new(title, T0 + started, 600, Template::General, &mut stamp);
    let track = Track::new(m.id, TrackId::Mic, None, ClockDomain::Replay, &mut stamp);
    let ids = (m.id, track.id);
    store.put_meeting(m).unwrap();
    store.put_track(track).unwrap();
    ids
}

/// A line of `pass`. The words carry the text, as a real segment's do.
fn line(
    svc: &Services,
    (meeting, track): (EntityId, EntityId),
    pass: TranscriptPass,
    text: &str,
    start: u64,
    seed: u64,
) -> EntityId {
    let clock = Arc::clone(&svc.clock);
    let mut rng = ChaCha8Rng::seed_from_u64(seed);
    let mut db = svc.store();
    let (store, hlc) = db.parts();
    let words: Vec<Word> = text
        .split(' ')
        .enumerate()
        .map(|(i, w)| Word::new(w, start + 400 * i as u64, start + 400 * i as u64 + 300, 0.9))
        .collect();
    let segment = Segment::from_words(
        EntityId::generate(&*clock, &mut rng),
        track,
        words,
        MeetingTime::from_millis(start),
    );
    let id = segment.id;
    store
        .put_segment(SegmentRow::new(meeting, pass, segment, hlc, &*clock))
        .unwrap();
    id
}

fn summary_saying(tldr: &str) -> SummaryV1 {
    SummaryV1 {
        title: "Written title".into(),
        tldr: tldr.into(),
        enhanced_notes: vec![],
        decisions: vec![],
        action_items: vec![],
        open_questions: vec![],
        topics: vec![],
    }
}

fn store_summary(svc: &Services, meeting: EntityId, summary: &SummaryV1, seed: u64) {
    let view = MeetingView::load(&**svc.store(), meeting).unwrap().unwrap();
    crate::summary::store_summary(
        svc,
        &view,
        "test-model".into(),
        summary,
        &mut ChaCha8Rng::seed_from_u64(seed),
    )
    .unwrap();
}

fn snippet_of(text: &str, query: &str) -> Option<String> {
    Query::new(query).snippet(text)
}

fn found(svc: &Services, query: &str) -> Vec<EntityId> {
    search_meetings(svc, query)
        .unwrap()
        .hits
        .into_iter()
        .map(|h| h.meeting)
        .collect()
}

#[test]
fn search_an_empty_query_finds_nothing() {
    let dir = TempDir::new("search-empty");
    let (svc, _) = services(&dir);
    meeting_at(&svc, "Anything at all", 0, 1);
    for query in ["", "   ", "\t\n "] {
        let results = search_meetings(&svc, query).unwrap();
        assert_eq!(results, SearchResults::default(), "{query:?}");
    }
}

#[test]
fn search_finds_a_title_case_ignored_and_trimmed() {
    let dir = TempDir::new("search-title");
    let (svc, _) = services(&dir);
    let (id, _) = meeting_at(&svc, "Orchard Planning", 0, 1);
    meeting_at(&svc, "Something else", 1, 2);
    let results = search_meetings(&svc, "  oRCHARD pl ").unwrap();
    assert!(!results.capped);
    assert_eq!(results.hits.len(), 1);
    let hit = &results.hits[0];
    assert_eq!(hit.meeting, id);
    assert_eq!(hit.title, "Orchard Planning");
    assert_eq!(hit.started_unix_ms, T0);
    assert_eq!(hit.matched, MatchedIn::Title);
    assert_eq!(hit.snippet, "Orchard Planning");
    assert_eq!(hit.line_ms, None);
}

#[test]
fn search_a_transcript_match_gives_the_first_matching_line_and_its_time() {
    let dir = TempDir::new("search-line");
    let (svc, _) = services(&dir);
    let m = meeting_at(&svc, "Weekly", 0, 1);
    // Written out of order: "first" is by the transcript's own order.
    line(
        &svc,
        m,
        TranscriptPass::Final,
        "later the marigold again",
        9_000,
        2,
    );
    line(&svc, m, TranscriptPass::Final, "nothing here", 1_000, 3);
    line(
        &svc,
        m,
        TranscriptPass::Final,
        "early marigold talk",
        4_000,
        4,
    );
    let results = search_meetings(&svc, "MARIGOLD").unwrap();
    assert_eq!(results.hits.len(), 1);
    let hit = &results.hits[0];
    assert_eq!(hit.matched, MatchedIn::Transcript);
    assert_eq!(hit.snippet, "early marigold talk");
    assert_eq!(hit.line_ms, Some(4_000));
}

#[test]
fn search_reports_the_first_place_of_title_then_transcript_then_summary() {
    let dir = TempDir::new("search-order");
    let (svc, _) = services(&dir);
    let m = meeting_at(&svc, "Marigold review", 0, 1);
    line(
        &svc,
        m,
        TranscriptPass::Final,
        "the marigold line",
        1_000,
        2,
    );
    store_summary(&svc, m.0, &summary_saying("A marigold summary."), 3);
    let hits = search_meetings(&svc, "marigold").unwrap().hits;
    assert_eq!(hits[0].matched, MatchedIn::Title);

    let n = meeting_at(&svc, "Other", 1, 4);
    line(&svc, n, TranscriptPass::Final, "the thistle line", 1_000, 5);
    store_summary(&svc, n.0, &summary_saying("A thistle summary."), 6);
    let hits = search_meetings(&svc, "thistle").unwrap().hits;
    assert_eq!(hits[0].matched, MatchedIn::Transcript);
}

#[test]
fn search_reads_every_part_of_the_summary_the_meeting_shows() {
    let dir = TempDir::new("search-summary");
    let (svc, _) = services(&dir);
    let (id, _) = meeting_at(&svc, "Plain", 0, 1);
    let summary = SummaryV1 {
        title: "Heron title".into(),
        tldr: "Ibis in short.".into(),
        enhanced_notes: vec![NoteSection {
            heading: "Crane heading".into(),
            bullets: vec![NoteBullet {
                text: "Egret bullet".into(),
                source_segment_ids: vec![],
            }],
        }],
        decisions: vec![Decision {
            text: "Stork decision".into(),
            source_segment_ids: vec![],
        }],
        action_items: vec![ActionItem {
            owner: "Plover owner".into(),
            task: "Avocet task".into(),
            due: Some("Curlew day".into()),
            source_segment_ids: vec![],
        }],
        open_questions: vec!["Godwit question".into()],
        topics: vec!["Bittern topic".into()],
    };
    store_summary(&svc, id, &summary, 2);
    for word in [
        "heron", "ibis", "crane", "egret", "stork", "plover", "avocet", "curlew", "godwit",
    ] {
        let hits = search_meetings(&svc, word).unwrap().hits;
        assert_eq!(hits.len(), 1, "{word}");
        assert_eq!(hits[0].matched, MatchedIn::Summary, "{word}");
        assert_eq!(hits[0].line_ms, None, "{word}");
        assert!(hits[0].snippet.to_lowercase().contains(word), "{word}");
    }
    // The meeting screen does not show topics: nothing the user could not then find there.
    assert_eq!(found(&svc, "bittern"), []);
}

#[test]
fn search_a_replaced_summary_in_its_tombstone_does_not_match() {
    let dir = TempDir::new("search-tombstone");
    let (svc, _) = services(&dir);
    let (id, _) = meeting_at(&svc, "Plain", 0, 1);
    store_summary(&svc, id, &summary_saying("The old zebrafinch text."), 2);
    assert_eq!(found(&svc, "zebrafinch"), [id]);
    store_summary(&svc, id, &summary_saying("The new kestrel text."), 3);
    assert_eq!(found(&svc, "zebrafinch"), []);
    assert_eq!(found(&svc, "kestrel"), [id]);
}

#[test]
fn search_a_deleted_meeting_does_not_match() {
    let dir = TempDir::new("search-deleted");
    let (svc, _) = services(&dir);
    let m = meeting_at(&svc, "Marigold review", 0, 1);
    line(&svc, m, TranscriptPass::Final, "the thistle line", 1_000, 2);
    store_summary(&svc, m.0, &summary_saying("A kestrel summary."), 3);
    let kept = meeting_at(&svc, "Marigold kept", 1, 4);
    assert_eq!(found(&svc, "marigold"), [kept.0, m.0]);

    svc.delete_meeting(m.0).unwrap();
    assert_eq!(found(&svc, "marigold"), [kept.0]);
    assert_eq!(found(&svc, "thistle"), []);
    assert_eq!(found(&svc, "kestrel"), []);
}

#[test]
fn search_the_final_transcript_replaces_the_draft() {
    let dir = TempDir::new("search-final");
    let (svc, _) = services(&dir);
    let m = meeting_at(&svc, "Plain", 0, 1);
    line(
        &svc,
        m,
        TranscriptPass::Live,
        "a draft mishearing",
        1_000,
        2,
    );
    // A draft is what a meeting without a final pass shows.
    assert_eq!(found(&svc, "mishearing"), [m.0]);

    line(&svc, m, TranscriptPass::Final, "a final hearing", 1_000, 3);
    assert_eq!(found(&svc, "mishearing"), []);
    assert_eq!(found(&svc, "final hearing"), [m.0]);
}

#[test]
fn search_a_tombstoned_line_does_not_match() {
    let dir = TempDir::new("search-line-gone");
    let (svc, _) = services(&dir);
    let m = meeting_at(&svc, "Plain", 0, 1);
    let gone = line(
        &svc,
        m,
        TranscriptPass::Final,
        "a replaced thistle",
        1_000,
        2,
    );
    line(&svc, m, TranscriptPass::Final, "a kept kestrel", 2_000, 3);
    assert_eq!(found(&svc, "thistle"), [m.0]);

    crate::edit::write(&svc, |tx, stamp| {
        for mut row in tx.segments(m.0)? {
            if row.segment.id == gone {
                row.meta.mark_deleted(stamp.hlc, stamp.clock);
                tx.put_segment(row)?;
            }
        }
        Ok(())
    })
    .unwrap();
    assert_eq!(found(&svc, "thistle"), []);
    assert_eq!(found(&svc, "kestrel"), [m.0]);
}

#[test]
fn search_percent_underscore_quotes_and_backslashes_are_literal_text() {
    let dir = TempDir::new("search-literal");
    let (svc, _) = services(&dir);
    let (percent, _) = meeting_at(&svc, "Up 50% today", 0, 1);
    let (under, _) = meeting_at(&svc, "file_name review", 1, 2);
    let (quoted, _) = meeting_at(&svc, r#"It's a "quoted" one"#, 2, 3);
    let (slash, _) = meeting_at(&svc, r"C:\temp folder", 3, 4);
    let (_plain, _) = meeting_at(&svc, "Up 50 today filename", 4, 5);

    assert_eq!(found(&svc, "%"), [percent]);
    assert_eq!(found(&svc, "50%"), [percent]);
    assert_eq!(found(&svc, "_"), [under]);
    assert_eq!(found(&svc, "file_name"), [under]);
    assert_eq!(found(&svc, "'"), [quoted]);
    assert_eq!(found(&svc, "\""), [quoted]);
    assert_eq!(found(&svc, "' OR 1=1 --"), []);
    assert_eq!(found(&svc, r"\"), [slash]);
    assert_eq!(found(&svc, r"\%"), []);
    assert_eq!(found(&svc, "*"), []);
}

#[test]
fn search_ignores_case_in_any_alphabet() {
    let dir = TempDir::new("search-case");
    let (svc, _) = services(&dir);
    let (id, _) = meeting_at(&svc, "ÉTÉ à MÜNCHEN Привет", 0, 1);
    assert_eq!(found(&svc, "été"), [id]);
    assert_eq!(found(&svc, "münchen"), [id]);
    assert_eq!(found(&svc, "ПРИВЕТ"), [id]);
}

#[test]
fn search_snippet_never_splits_a_character_at_either_edge() {
    let dir = TempDir::new("search-edge");
    let (svc, _) = services(&dir);
    // Multi-byte text of every width on both sides, with no space to stop at, and the match
    // moved through the window so each edge falls inside each width.
    let filler = ["é", "€", "🦉"];
    for (i, before) in filler.iter().enumerate() {
        for (j, after) in filler.iter().enumerate() {
            for extra in 0..4 {
                let text = format!(
                    "{}needle{}",
                    before.repeat(SNIPPET_BEFORE + 1 + extra),
                    after.repeat(SNIPPET_AFTER + 1 + extra)
                );
                let hit = snippet_of(&text, "needle").expect("found");
                let want = format!(
                    "...{}needle{}...",
                    before.repeat(SNIPPET_BEFORE),
                    after.repeat(SNIPPET_AFTER)
                );
                assert_eq!(hit, want, "{i} {j} {extra}");
            }
        }
    }
    // Through the whole path too.
    let long = format!("{}needle", "€".repeat(200));
    let (id, _) = meeting_at(&svc, &long, 0, 1);
    let hits = search_meetings(&svc, "NEEDLE").unwrap().hits;
    assert_eq!(hits[0].meeting, id);
    assert!(hits[0].snippet.ends_with("needle"));
    assert!(hits[0].snippet.starts_with("..."));
}

#[test]
fn search_snippet_of_a_short_text_is_the_whole_text_without_an_ellipsis() {
    assert_eq!(snippet_of("a short line", "short").unwrap(), "a short line");
    assert_eq!(snippet_of("a short line", "absent"), None);
}

#[test]
fn search_snippet_prefers_to_cut_at_a_word_and_stays_on_one_line() {
    let before = "alpha ".repeat(10);
    let after = " omega".repeat(20);
    let snippet = snippet_of(&format!("{before}needle{after}"), "needle").unwrap();
    assert!(snippet.starts_with("...alpha "), "{snippet}");
    assert!(snippet.ends_with("omega..."), "{snippet}");
    assert!(snippet.contains("needle"));
    // A cut at a word never gives more than the limits.
    assert!(snippet.chars().count() <= SNIPPET_BEFORE + "needle".len() + SNIPPET_AFTER + 6);

    assert_eq!(
        snippet_of("first line\n\n  needle\tthen more", "needle").unwrap(),
        "first line needle then more"
    );
}

#[test]
fn search_a_letter_that_lowers_to_two_keeps_the_snippet_in_place() {
    // 'İ' lowers to two characters, so folded positions run ahead of the text's own.
    let text = format!("{} needle tail", "İ".repeat(10));
    assert_eq!(
        snippet_of(&text, "needle").unwrap(),
        format!("{} needle tail", "İ".repeat(10))
    );
    assert!(snippet_of(&text, "İ").is_some());
    assert!(snippet_of("plain i", "İ").is_none());
    // A query longer than the text.
    assert_eq!(snippet_of("ab", "abc"), None);
}

#[test]
fn search_stops_at_the_cap_newest_first_and_says_so() {
    let dir = TempDir::new("search-cap");
    let (svc, _) = services(&dir);
    let mut ids = Vec::new();
    for i in 0..MAX_RESULTS as u64 {
        ids.push(meeting_at(&svc, &format!("Marigold {i}"), i + 1, i + 1).0);
    }
    ids.reverse();

    // Exactly the cap: nothing was left out.
    let results = search_meetings(&svc, "marigold").unwrap();
    assert!(!results.capped);
    assert_eq!(results.hits.len(), MAX_RESULTS);

    let older = meeting_at(&svc, "Marigold oldest", 0, 1_000).0;
    let results = search_meetings(&svc, "marigold").unwrap();
    assert!(results.capped);
    assert_eq!(results.hits.len(), MAX_RESULTS);
    let got: Vec<EntityId> = results.hits.iter().map(|h| h.meeting).collect();
    assert_eq!(got, ids);
    assert!(!got.contains(&older));
}

#[test]
fn search_with_a_cancelled_token_gives_no_answer() {
    let dir = TempDir::new("search-cancelled");
    let (svc, _) = services(&dir);
    for i in 0..3 {
        meeting_at(&svc, "Bramble review", i * 1_000, 40 + i);
    }
    let cancel = CancelToken::new();
    cancel.cancel();
    assert_eq!(
        search_meetings_unless(&svc, "bramble", &cancel).unwrap(),
        None
    );
}

#[test]
fn search_checks_its_token_before_every_meeting_and_answers_as_before() {
    let dir = TempDir::new("search-token-polls");
    let (svc, _) = services(&dir);
    for i in 0..5 {
        meeting_at(&svc, &format!("Juniper {i}"), i * 1_000, 50 + i);
    }
    let cancel = CancelToken::new();
    let answer = search_meetings_unless(&svc, "juniper", &cancel).unwrap();
    assert_eq!(answer, Some(search_meetings(&svc, "juniper").unwrap()));
    assert_eq!(answer.unwrap().hits.len(), 5);
    // Once per meeting at least, so a cancel stops it between two meetings.
    assert!(cancel.polls() >= 5, "{} polls", cancel.polls());
}

#[test]
fn search_a_newer_search_supersedes_the_one_before() {
    let searches = Searches::default();
    let first = searches.begin();
    assert!(!first.is_cancelled());
    let second = searches.begin();
    assert!(first.is_cancelled(), "the older search goes");
    assert!(!second.is_cancelled());
    let third = searches.begin();
    assert!(second.is_cancelled());
    assert!(!third.is_cancelled());
}
