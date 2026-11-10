//! Live summary tests: the setting, the record, the thread rule and the reuse rule. Every
//! name contains `live_summar`.

use earshot_core::asr::MeetingTime;
use earshot_core::prompt::{MeetingInfo, PromptSegment, PromptVariant, Template};
use earshot_core::summary::{ActionItem, Decision};

use super::*;
use crate::testutil::{TempDir, services};

fn id(n: u8) -> EntityId {
    EntityId::from_bytes([
        0x01, 0x90, 0, 0, 0, 0, 0x70, 0, 0x80, 0, 0, 0, 0, 0, 0xA0, n,
    ])
}

fn partial(decision: &str, cites: &[EntityId], owner: &str) -> SummaryV1 {
    SummaryV1 {
        title: String::new(),
        tldr: "t".into(),
        enhanced_notes: Vec::new(),
        decisions: vec![Decision {
            text: decision.into(),
            source_segment_ids: cites.to_vec(),
        }],
        action_items: vec![ActionItem {
            owner: owner.into(),
            task: "send the notes".into(),
            due: None,
            source_segment_ids: cites.to_vec(),
        }],
        open_questions: Vec::new(),
        topics: Vec::new(),
    }
}

fn timed(n: u8, track: u32, start_ms: u64, end_ms: u64, speaker: &str) -> TimedSegment {
    TimedSegment {
        id: id(n),
        track,
        start_ms,
        end_ms,
        speaker: speaker.into(),
    }
}

/// The final transcript: two segments in window 0 (3 s windows), one in window 1.
fn fin() -> (Vec<TimedSegment>, SummaryInput) {
    let fin = vec![
        timed(10, 0, 1_000, 2_000, "Me"),
        timed(11, 1, 2_000, 2_900, "Alice"),
        timed(12, 1, 4_000, 5_000, "Alice"),
    ];
    let input = SummaryInput {
        meeting: MeetingInfo {
            title: None,
            started: None,
            duration_ms: 6_000,
            attendees: Vec::new(),
        },
        segments: fin
            .iter()
            .map(|t| PromptSegment {
                id: t.id,
                start: MeetingTime::from_millis(t.start_ms),
                speaker: t.speaker.clone(),
                text: "x".into(),
            })
            .collect(),
        notes: Vec::new(),
        template: Template::General,
        prompt_variant: PromptVariant::Current,
        style: Default::default(),
        vocabulary: Default::default(),
        user_name: None,
    };
    (fin, input)
}

fn record(partials: Vec<LivePartial>) -> StoredLiveSummaries {
    StoredLiveSummaries::Record(Box::new(LiveSummaryRecord {
        model: "m".into(),
        seconds: 3,
        partials,
        stopped: None,
    }))
}

#[test]
fn live_summaries_setting_is_off_by_default_and_round_trips() {
    let dir = TempDir::new("live-summaries");
    let (svc, _) = services(&dir);
    let mut db = svc.store();
    assert_eq!(live_summaries_setting(&db).unwrap(), StoredSwitch::Absent);
    assert!(!StoredSwitch::Absent.on());
    set_live_summaries(&mut db, true).unwrap();
    assert_eq!(
        live_summaries_setting(&db).unwrap(),
        StoredSwitch::Set(true)
    );
    assert!(StoredSwitch::Set(true).on());
    set_live_summaries(&mut db, false).unwrap();
    assert_eq!(
        live_summaries_setting(&db).unwrap(),
        StoredSwitch::Set(false)
    );
    // A value a newer build wrote reads as off, as stored.
    db.set_local_value(LIVE_SUMMARIES_KEY, b"later").unwrap();
    let stored = live_summaries_setting(&db).unwrap();
    assert_eq!(stored, StoredSwitch::Unknown("later".into()));
    assert!(!stored.on());
    assert!(!LIVE_SUMMARIES_HELP.contains('\u{2014}'));
}

#[test]
fn live_summary_record_round_trips_and_refuses_other_versions() {
    let r = LiveSummaryRecord {
        model: "qwen".into(),
        seconds: 600,
        partials: vec![LivePartial {
            window: 2,
            summary: partial("ship", &[id(1)], "Me"),
        }],
        stopped: Some("why".into()),
    };
    let bytes = encode(&r).unwrap();
    assert_eq!(
        decode(&bytes),
        StoredLiveSummaries::Record(Box::new(r.clone()))
    );
    let mut other = bytes.clone();
    other[0] = 9;
    assert!(
        matches!(decode(&other), StoredLiveSummaries::Unreadable(m) if m.contains("version 9"))
    );
    assert!(matches!(
        decode(&bytes[..2]),
        StoredLiveSummaries::Unreadable(_)
    ));
    let mut damaged = bytes;
    damaged.truncate(20);
    assert!(matches!(
        decode(&damaged),
        StoredLiveSummaries::Unreadable(_)
    ));
}

#[test]
fn live_summary_threads_leave_live_transcription_its_cores() {
    // The Surface: 12 cores, live CPU transcription on 6 (3 per track).
    assert_eq!(live_summary_threads(12, 6, 6), 4);
    // Never above the post-meeting count, never below one.
    assert_eq!(live_summary_threads(32, 4, 8), 8);
    assert_eq!(live_summary_threads(4, 4, 4), 1);
    assert_eq!(live_summary_threads(0, 0, 0), 1);
}

#[test]
fn live_summary_partials_are_recited_onto_their_window() {
    let (fin, input) = fin();
    let ranges = earshot_core::prompt::time_chunks(&input, 3).unwrap();
    assert_eq!(ranges.len(), 2);
    // Live: one Me segment and one Them segment covering the first window.
    let live = vec![
        timed(1, 0, 900, 2_000, "Me"),
        timed(2, 1, 2_000, 3_000, "Them"),
    ];
    let rec = record(vec![LivePartial {
        window: 0,
        summary: partial("ship on friday", &[id(1)], "Me"),
    }]);
    let (done, reuse) = reusable(&rec, "m", 3, &live, &fin, &input, &ranges);
    assert_eq!(
        reuse,
        LiveReuse {
            recorded: 1,
            reused: 1,
            ..LiveReuse::default()
        }
    );
    let got = done[0].as_ref().expect("window 0 reused");
    assert!(done[1].is_none(), "window 1 is mapped at hang-up");
    // Cited onto the final segment of the same track, owner renamed to its speaker.
    assert_eq!(got.decisions[0].source_segment_ids, [id(10)]);
    assert_eq!(got.decisions[0].text, "ship on friday");
    assert_eq!(got.action_items[0].owner, "Me");
    let them = record(vec![LivePartial {
        window: 0,
        summary: partial("ask alice", &[id(2)], "Them"),
    }]);
    let (done, _) = reusable(&them, "m", 3, &live, &fin, &input, &ranges);
    assert_eq!(done[0].as_ref().unwrap().action_items[0].owner, "Alice");
}

#[test]
fn live_summary_partials_that_cannot_be_used_are_redone() {
    let (fin, input) = fin();
    let ranges = earshot_core::prompt::time_chunks(&input, 3).unwrap();
    let live = vec![timed(1, 0, 900, 2_000, "Me")];
    // A citation of an unknown live segment: the window is mapped again.
    let unknown = record(vec![LivePartial {
        window: 0,
        summary: partial("x", &[id(99)], "Me"),
    }]);
    let (done, reuse) = reusable(&unknown, "m", 3, &live, &fin, &input, &ranges);
    assert!(done.iter().all(Option::is_none));
    assert_eq!(
        (reuse.recorded, reuse.recite_failed, reuse.reused),
        (1, 1, 0)
    );
    // A window the transcript does not have (nobody spoke there in the end).
    let gone = record(vec![LivePartial {
        window: 7,
        summary: partial("x", &[id(1)], "Me"),
    }]);
    let (done, reuse) = reusable(&gone, "m", 3, &live, &fin, &input, &ranges);
    assert!(done.iter().all(Option::is_none));
    assert_eq!(reuse.unplaced, 1);
    let ok = vec![LivePartial {
        window: 0,
        summary: partial("x", &[id(1)], "Me"),
    }];
    // Another model or window length: nothing is used.
    let (done, reuse) = reusable(
        &record(ok.clone()),
        "other",
        3,
        &live,
        &fin,
        &input,
        &ranges,
    );
    assert!(done.iter().all(Option::is_none));
    assert!(reuse.not_used.unwrap().contains("made with m"));
    let (done, reuse) = reusable(&record(ok), "m", 600, &live, &fin, &input, &ranges);
    assert!(done.iter().all(Option::is_none));
    assert!(reuse.not_used.unwrap().contains("3 s windows"));
    let unreadable = StoredLiveSummaries::Unreadable("version 9".into());
    let (done, reuse) = reusable(&unreadable, "m", 3, &live, &fin, &input, &ranges);
    assert!(done.iter().all(Option::is_none));
    assert_eq!(reuse.not_used.as_deref(), Some("version 9"));
}
