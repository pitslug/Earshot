//! T14.47: the final pass leaves no replaced text (docs/sync-spec.md 15.2), keeps a side it
//! found nothing on (15.11 c) and cleans echo against a kept reference (15.11 d). Every name
//! contains `post`. Every word is invented.

use super::*;

/// A new seed for every run: a re-run with the seed and the fixed test clock of the run before
/// would give its new lines the ids of that run's lines.
fn fresh_rng() -> ChaCha8Rng {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEED: AtomicU64 = AtomicU64::new(9_000);
    ChaCha8Rng::seed_from_u64(SEED.fetch_add(1, Ordering::SeqCst))
}

/// Puts a live draft row of `text` on `track` at `start` ms.
fn put_draft(svc: &Services, meeting: EntityId, track: &Track, text: &str, start: u64) -> EntityId {
    let clock = Arc::clone(&svc.clock);
    let mut db = svc.store();
    let (store, hlc) = db.parts();
    let words: Vec<Word> = text
        .split(' ')
        .enumerate()
        .map(|(i, w)| Word::new(w, start + 300 * i as u64, start + 300 * i as u64 + 250, 0.9))
        .collect();
    let s = earshot_core::asr::Segment::from_words(
        EntityId::from_bytes({
            let mut b = [0x5Au8; 16];
            b[0] = (start / 1000) as u8;
            b[1] = u8::from(track.kind == TrackId::Reference);
            b
        }),
        track.id,
        words,
        earshot_core::asr::MeetingTime::ZERO,
    );
    let id = s.id;
    let row = SegmentRow::new(meeting, TranscriptPass::Live, s, hlc, &*clock);
    store.put_segment(row).unwrap();
    id
}

fn rows(svc: &Services, meeting: EntityId) -> Vec<SegmentRow> {
    svc.store().segments(meeting).unwrap()
}

fn input_of(
    meeting: EntityId,
    mic: &Track,
    them: &Track,
    locks: Vec<crate::echo::EchoLock>,
) -> PostInput {
    let (mic_audio, them_audio) = scene();
    PostInput {
        echo_locks: locks,
        echo_locks_unreadable: false,
        meeting,
        tracks: vec![track_input(mic, mic_audio), track_input(them, them_audio)],
    }
}

fn run_with(
    svc: &Services,
    asr: FakeTranscriber,
    input: PostInput,
) -> Result<PostOutcome, RuntimeError> {
    let mut post =
        PostProcessor::new(asr, FakeSegmenter::segmentation_3_0(), embedder(), config()).unwrap();
    post.run(svc, input, &mut fresh_rng(), &CancelToken::new())
}

/// The mic as `transcriber()` hears it, and the reference empty (it heard nothing).
fn mic_only() -> FakeTranscriber {
    FakeTranscriber::scripted([
        Ok(vec![
            raw(&[("hello", 1_000, 1_400), ("all", 1_500, 1_900)]),
            raw(&[("thanks", 31_000, 31_500)]),
        ]),
        Ok(vec![]),
    ])
}

fn nothing() -> FakeTranscriber {
    FakeTranscriber::scripted([Ok(vec![]), Ok(vec![])])
}

/// Every device-local value, for byte comparisons.
fn local_values(svc: &Services) -> Vec<(String, Vec<u8>)> {
    let db = svc.store();
    db.local_keys()
        .unwrap()
        .into_iter()
        .map(|k| {
            let v = db.local_value(&k).unwrap().unwrap();
            (k, v)
        })
        .collect()
}

fn assert_no_replaced_text(svc: &Services, meeting: EntityId) {
    let all = rows(svc, meeting);
    for r in &all {
        assert!(
            !(r.meta.deleted && r.holds_text()),
            "a tombstone holds text: {r:?}"
        );
    }
    let final_tracks: BTreeSet<EntityId> = all
        .iter()
        .filter(|r| r.pass == TranscriptPass::Final && !r.meta.deleted)
        .map(|r| r.segment.track)
        .collect();
    for r in &all {
        if r.pass == TranscriptPass::Live && final_tracks.contains(&r.segment.track) {
            assert!(
                r.is_cleared(),
                "a draft row of a track with final lines: {r:?}"
            );
        }
    }
}

/// A first pass and a re-transcription leave no tombstone that holds text and no draft on a
/// track with final lines; every clearing stamp is later than every final line's.
#[test]
fn post_replaced_first_pass_and_rerun_leave_no_replaced_text() {
    let dir = TempDir::new();
    let svc = services(&dir);
    let (meeting, mic, them) = meeting_rows(&svc);
    put_draft(&svc, meeting, &mic, "hello all", 1_000);
    put_draft(&svc, meeting, &them, "hi see you", 13_000);
    run_with(
        &svc,
        transcriber(),
        input_of(meeting, &mic, &them, Vec::new()),
    )
    .unwrap();
    assert_no_replaced_text(&svc, meeting);
    let all = rows(&svc, meeting);
    let top_line = all
        .iter()
        .filter(|r| r.pass == TranscriptPass::Final && !r.meta.deleted)
        .map(|r| r.meta.updated_at)
        .max()
        .unwrap();
    for r in all.iter().filter(|r| r.is_cleared()) {
        assert!(r.meta.updated_at > top_line, "{r:?}");
    }
    // The lines go out before the cleared rows.
    for t in [&mic, &them] {
        assert!(svc.store().lines_go_first(meeting, t.id).unwrap());
    }
    // A re-transcription clears what it replaced.
    run_with(
        &svc,
        transcriber(),
        input_of(meeting, &mic, &them, Vec::new()),
    )
    .unwrap();
    assert_no_replaced_text(&svc, meeting);
    let cleared = rows(&svc, meeting)
        .iter()
        .filter(|r| r.is_cleared())
        .count();
    assert!(
        cleared >= 2 + 6,
        "drafts and the first run's lines: {cleared}"
    );
}

/// A track without final lines keeps its draft; a pass with no final line at all leaves the
/// draft shown.
#[test]
fn post_replaced_a_track_without_final_lines_keeps_its_draft() {
    let dir = TempDir::new();
    let svc = services(&dir);
    let (meeting, mic, them) = meeting_rows(&svc);
    let mic_draft = put_draft(&svc, meeting, &mic, "hello all", 1_000);
    let them_draft = put_draft(&svc, meeting, &them, "far end words", 13_000);
    run_with(&svc, mic_only(), input_of(meeting, &mic, &them, Vec::new())).unwrap();
    let find = |id| {
        rows(&svc, meeting)
            .into_iter()
            .find(|r| r.segment.id == id)
            .unwrap()
    };
    assert!(find(mic_draft).is_cleared());
    let kept = find(them_draft);
    assert!(kept.holds_text() && !kept.meta.deleted, "{kept:?}");

    let dir = TempDir::new();
    let svc = services(&dir);
    let (meeting, mic, them) = meeting_rows(&svc);
    put_draft(&svc, meeting, &mic, "hello all", 1_000);
    put_draft(&svc, meeting, &them, "far end words", 13_000);
    let outcome = run_with(&svc, nothing(), input_of(meeting, &mic, &them, Vec::new())).unwrap();
    assert_eq!(outcome.final_segments, 0);
    assert!(
        !outcome
            .warnings
            .iter()
            .any(|w| matches!(w, PostWarning::EarlierLinesKept { .. })),
        "nothing to keep: {:?}",
        outcome.warnings
    );
    let shown = svc.store().shown_lines(meeting).unwrap();
    assert_eq!(shown.pass, Some(TranscriptPass::Live));
    assert_eq!(shown.lines.len(), 2);
}

/// A re-transcription that finds nothing for one track keeps that track's lines and speakers,
/// replaces the other's and warns; with the reference kept and the mic replaced the policy has
/// no reference voices, and no reference speaker row is tombstoned.
#[test]
fn post_replaced_a_rerun_that_finds_nothing_on_one_track_keeps_its_lines() {
    let dir = TempDir::new();
    let svc = services(&dir);
    let (meeting, mic, them) = meeting_rows(&svc);
    run_with(
        &svc,
        transcriber(),
        input_of(meeting, &mic, &them, Vec::new()),
    )
    .unwrap();
    let them_before: Vec<SegmentRow> = rows(&svc, meeting)
        .into_iter()
        .filter(|r| r.segment.track == them.id && !r.meta.deleted)
        .collect();
    let speakers_before: Vec<earshot_core::model::Speaker> = live_speakers(&svc, meeting)
        .into_iter()
        .filter(|s| s.role == SpeakerRole::Remote)
        .collect();
    assert!(!them_before.is_empty() && !speakers_before.is_empty());
    let outcome = run_with(&svc, mic_only(), input_of(meeting, &mic, &them, Vec::new())).unwrap();
    assert!(
        outcome.warnings.contains(&PostWarning::EarlierLinesKept {
            track: TrackId::Reference
        }),
        "{:?}",
        outcome.warnings
    );
    let them_after: Vec<SegmentRow> = rows(&svc, meeting)
        .into_iter()
        .filter(|r| r.segment.track == them.id && !r.meta.deleted)
        .collect();
    assert_eq!(
        them_after, them_before,
        "the reference lines are kept as they were"
    );
    for s in &speakers_before {
        let now = svc
            .store()
            .speakers(meeting)
            .unwrap()
            .into_iter()
            .find(|x| x.id == s.id)
            .unwrap();
        assert!(!now.meta.deleted, "{now:?}");
    }
    // The mic was replaced.
    assert_eq!(mic_finals(&svc, meeting, &mic), ["hello", "all", "thanks"]);
    assert_no_replaced_text(&svc, meeting);
}

/// The reverse: the mic kept, the reference replaced; the kept mic lines are unchanged.
#[test]
fn post_replaced_a_kept_mic_is_unchanged_beside_a_replaced_reference() {
    let dir = TempDir::new();
    let svc = services(&dir);
    let (meeting, mic, them) = meeting_rows(&svc);
    run_with(
        &svc,
        transcriber(),
        input_of(meeting, &mic, &them, Vec::new()),
    )
    .unwrap();
    let mic_before: Vec<SegmentRow> = rows(&svc, meeting)
        .into_iter()
        .filter(|r| r.segment.track == mic.id && !r.meta.deleted)
        .collect();
    let asr = FakeTranscriber::scripted([Ok(vec![]), Ok(vec![raw(&[("hi", 13_000, 13_300)])])]);
    let outcome = run_with(&svc, asr, input_of(meeting, &mic, &them, Vec::new())).unwrap();
    assert!(outcome.warnings.contains(&PostWarning::EarlierLinesKept {
        track: TrackId::Mic
    }));
    let mic_after: Vec<SegmentRow> = rows(&svc, meeting)
        .into_iter()
        .filter(|r| r.segment.track == mic.id && !r.meta.deleted)
        .collect();
    assert_eq!(mic_after, mic_before, "{:#?}", rows(&svc, meeting));
}

/// A re-transcription that finds nothing at all changes no line and no speaker row and leaves
/// every device-local record byte for byte as it was, deleting only the live records.
#[test]
fn post_replaced_a_rerun_that_finds_nothing_at_all_changes_nothing() {
    let dir = TempDir::new();
    let svc = services(&dir);
    let (meeting, mic, them) = meeting_rows(&svc);
    run_with(
        &svc,
        transcriber(),
        input_of(meeting, &mic, &them, Vec::new()),
    )
    .unwrap();
    // Live records the run must delete.
    for kind in [TrackId::Mic, TrackId::Reference] {
        let mut db = svc.store();
        db.set_local_value(crate::stretches::record_key(meeting, kind).as_str(), b"x")
            .unwrap();
        db.set_local_value(crate::speakers::record_key(meeting, kind).as_str(), b"x")
            .unwrap();
    }
    let rows_before = rows(&svc, meeting);
    let speakers_before = svc.store().speakers(meeting).unwrap();
    let local_before: Vec<(String, Vec<u8>)> = local_values(&svc)
        .into_iter()
        .filter(|(k, _)| !k.starts_with("live-stretches/") && !k.starts_with("live-speakers/"))
        .collect();
    let outcome = run_with(&svc, nothing(), input_of(meeting, &mic, &them, Vec::new())).unwrap();
    assert_eq!(rows(&svc, meeting), rows_before);
    assert_eq!(svc.store().speakers(meeting).unwrap(), speakers_before);
    assert_eq!(
        local_values(&svc),
        local_before,
        "only the live records are gone"
    );
    assert_eq!(
        outcome
            .warnings
            .iter()
            .filter(|w| matches!(w, PostWarning::EarlierLinesKept { .. }))
            .count(),
        2
    );
    assert_eq!(outcome.final_segments, 6);
}

/// A track with one edited line and two unedited lines where the run finds nothing is kept
/// whole; its control (one recogniser line outside the edited line) replaces the two unedited
/// lines and keeps the edited one. New words only under the edited line do not count.
#[test]
fn post_replaced_words_under_an_edited_line_never_count_as_found() {
    for case in ["nothing", "under_the_edit", "one_more_word"] {
        let dir = TempDir::new();
        let svc = services(&dir);
        let (meeting, mic, them) = meeting_rows(&svc);
        run_with(
            &svc,
            transcriber(),
            input_of(meeting, &mic, &them, Vec::new()),
        )
        .unwrap();
        // The reference has three lines: hi, see, you (and bye). Edit "hi".
        let edited_id = rows(&svc, meeting)
            .into_iter()
            .find(|r| r.segment.track == them.id && r.segment.text == "hi" && !r.meta.deleted)
            .unwrap()
            .segment
            .id;
        crate::edit::edit_segment(&svc, meeting, edited_id, "hi there").unwrap();
        let reference = match case {
            "nothing" => vec![],
            "under_the_edit" => vec![raw(&[("high", 13_000, 13_300)])],
            _ => vec![
                raw(&[("high", 13_000, 13_300)]),
                raw(&[("later", 25_000, 25_300)]),
            ],
        };
        let asr = FakeTranscriber::scripted([
            Ok(vec![
                raw(&[("hello", 1_000, 1_400), ("all", 1_500, 1_900)]),
                raw(&[("thanks", 31_000, 31_500)]),
            ]),
            Ok(reference),
        ]);
        let before: Vec<SegmentRow> = rows(&svc, meeting)
            .into_iter()
            .filter(|r| r.segment.track == them.id && !r.meta.deleted)
            .collect();
        let outcome = run_with(&svc, asr, input_of(meeting, &mic, &them, Vec::new())).unwrap();
        let after: Vec<SegmentRow> = rows(&svc, meeting)
            .into_iter()
            .filter(|r| r.segment.track == them.id && !r.meta.deleted)
            .collect();
        let kept = outcome.warnings.contains(&PostWarning::EarlierLinesKept {
            track: TrackId::Reference,
        });
        if case == "one_more_word" {
            assert!(!kept, "{case}");
            let texts: Vec<&str> = after.iter().map(|r| r.segment.text.as_str()).collect();
            assert_eq!(texts, ["hi there", "later"], "{case}");
        } else {
            assert!(kept, "{case}: {:?}", outcome.warnings);
            assert_eq!(after, before, "{case}");
        }
        assert_no_replaced_text(&svc, meeting);
    }
}

/// A mic whose recogniser heard words that echo cleaning removed in full replaces the mic's
/// earlier lines with none (15.11, provisional answer 1); a mic that heard nothing keeps them.
#[test]
fn post_replaced_a_mic_that_heard_only_echo_is_replaced_and_one_that_heard_nothing_is_kept() {
    for heard_echo in [true, false] {
        let dir = TempDir::new();
        let svc = services(&dir);
        let (meeting, mic, them) = meeting_rows(&svc);
        let (mic_audio, them_audio, asr) = echo_scene();
        let mut post = PostProcessor::new(
            asr,
            FakeSegmenter::segmentation_3_0(),
            echo_embedder(MODEL),
            echo_config(MODEL),
        )
        .unwrap();
        let locks = whole_call(150);
        post.run(
            &svc,
            PostInput {
                meeting,
                tracks: vec![
                    track_input(&mic, mic_audio.clone()),
                    track_input(&them, them_audio.clone()),
                ],
                echo_locks: locks.clone(),
                echo_locks_unreadable: false,
            },
            &mut fresh_rng(),
            &CancelToken::new(),
        )
        .unwrap();
        assert!(!mic_finals(&svc, meeting, &mic).is_empty());
        let mic_words = if heard_echo {
            vec![raw(&[
                ("we", 8_650, 8_900),
                ("should", 8_950, 9_200),
                ("ship", 9_250, 9_500),
                ("it", 9_550, 9_800),
            ])]
        } else {
            vec![]
        };
        let asr = FakeTranscriber::scripted([
            Ok(mic_words),
            Ok(vec![raw(&[
                ("we", 8_500, 8_750),
                ("should", 8_800, 9_050),
                ("ship", 9_100, 9_350),
                ("it", 9_400, 9_650),
            ])]),
        ]);
        let mut post = PostProcessor::new(
            asr,
            FakeSegmenter::segmentation_3_0(),
            echo_embedder(MODEL),
            echo_config(MODEL),
        )
        .unwrap();
        let before = mic_finals(&svc, meeting, &mic);
        let outcome = post
            .run(
                &svc,
                PostInput {
                    meeting,
                    tracks: vec![track_input(&mic, mic_audio), track_input(&them, them_audio)],
                    echo_locks: locks,
                    echo_locks_unreadable: false,
                },
                &mut fresh_rng(),
                &CancelToken::new(),
            )
            .unwrap();
        let kept = outcome.warnings.contains(&PostWarning::EarlierLinesKept {
            track: TrackId::Mic,
        });
        if heard_echo {
            assert!(!kept);
            assert!(
                mic_finals(&svc, meeting, &mic).is_empty(),
                "replaced with none"
            );
        } else {
            assert!(kept);
            assert_eq!(mic_finals(&svc, meeting, &mic), before);
        }
        assert_no_replaced_text(&svc, meeting);
    }
}

/// With the reference kept, new mic words that repeat the kept reference lines' words at the
/// lock delay are removed and words not repeated there are kept; with the kept lines' words
/// withheld (their text edited, which clears the words) they are left in.
#[test]
fn post_replaced_echo_is_cleaned_against_a_kept_reference() {
    for withheld in [false, true] {
        let dir = TempDir::new();
        let svc = services(&dir);
        let (meeting, mic, them) = meeting_rows(&svc);
        let (mic_audio, them_audio, asr) = echo_scene();
        let locks = whole_call(150);
        let mut post = PostProcessor::new(
            asr,
            FakeSegmenter::segmentation_3_0(),
            echo_embedder(MODEL),
            echo_config(MODEL),
        )
        .unwrap();
        let input = |mic_audio: MonoBuffer, them_audio: MonoBuffer, locks| PostInput {
            meeting,
            tracks: vec![track_input(&mic, mic_audio), track_input(&them, them_audio)],
            echo_locks: locks,
            echo_locks_unreadable: false,
        };
        post.run(
            &svc,
            input(mic_audio.clone(), them_audio.clone(), locks.clone()),
            &mut fresh_rng(),
            &CancelToken::new(),
        )
        .unwrap();
        if withheld {
            for r in rows(&svc, meeting) {
                if r.segment.track == them.id && !r.meta.deleted && r.pass == TranscriptPass::Final
                {
                    let text = format!("{} edited", r.segment.text);
                    crate::edit::edit_segment(&svc, meeting, r.segment.id, &text).unwrap();
                }
            }
        }
        // The reference hears nothing now (kept); the mic hears the user and the echo again.
        let asr = FakeTranscriber::scripted([
            Ok(vec![
                raw(&[("hello", 1_000, 1_400), ("all", 1_500, 1_900)]),
                raw(&[
                    ("we", 8_650, 8_900),
                    ("should", 8_950, 9_200),
                    ("ship", 9_250, 9_500),
                    ("it", 9_550, 9_800),
                ]),
            ]),
            Ok(vec![]),
        ]);
        let mut post = PostProcessor::new(
            asr,
            FakeSegmenter::segmentation_3_0(),
            echo_embedder(MODEL),
            echo_config(MODEL),
        )
        .unwrap();
        let outcome = post
            .run(
                &svc,
                input(mic_audio, them_audio, locks),
                &mut fresh_rng(),
                &CancelToken::new(),
            )
            .unwrap();
        assert!(
            outcome.warnings.contains(&PostWarning::EarlierLinesKept {
                track: TrackId::Reference
            }),
            "{:?}",
            outcome.warnings
        );
        let words = mic_finals(&svc, meeting, &mic);
        if withheld {
            assert_eq!(words, ["hello", "all", "we", "should", "ship", "it"]);
        } else {
            assert_eq!(words, ["hello", "all"]);
            assert_eq!(outcome.echo.text_removed, 4);
        }
    }
}

/// What decided the kept tracks changing between the early read and the write so that a
/// decision flips fails the pass with a conflict and writes nothing: an edit that puts the
/// run's only new words under an edited line, and a track losing its last live final row. Its
/// control, a change that flips no decision, passes.
#[test]
fn post_replaced_a_kept_decision_that_changed_before_the_write_is_a_conflict() {
    for case in ["edit_flips", "lines_gone", "no_flip"] {
        let dir = TempDir::new();
        let svc = services(&dir);
        let (meeting, mic, them) = meeting_rows(&svc);
        run_with(
            &svc,
            transcriber(),
            input_of(meeting, &mic, &them, Vec::new()),
        )
        .unwrap();
        let them_id = them.id;
        let mic_id = mic.id;
        let hook: crate::post::BeforeWriteHook = Box::new(move |svc: &Services| {
            let clock = Arc::clone(&svc.clock);
            let mut db = svc.store();
            let (store, hlc) = db.parts();
            for mut r in store.segments(meeting).unwrap() {
                if r.meta.deleted || r.pass != TranscriptPass::Final {
                    continue;
                }
                match case {
                    "edit_flips" if r.segment.track == them_id && r.segment.text == "hi" => {
                        r.segment.text = "hi there".into();
                        r.segment.words.clear();
                        r.meta.touch(hlc, &*clock);
                        r.text_edited_at = Some(r.meta.updated_at);
                        store.put_segment(r).unwrap();
                    }
                    "lines_gone" if r.segment.track == them_id => {
                        r.meta.mark_deleted(hlc, &*clock);
                        store.put_segment(r).unwrap();
                    }
                    "no_flip" if r.segment.track == mic_id && r.segment.text == "thanks" => {
                        r.segment.text = "thanks a lot".into();
                        r.segment.words.clear();
                        r.meta.touch(hlc, &*clock);
                        r.text_edited_at = Some(r.meta.updated_at);
                        store.put_segment(r).unwrap();
                    }
                    _ => {}
                }
            }
        });
        // The reference's only new words lie under "hi" (found at the early read; under an
        // edit after it), or it hears nothing (kept at the early read; nothing to keep after).
        let reference = match case {
            "edit_flips" => vec![raw(&[("high", 13_000, 13_300)])],
            _ => vec![],
        };
        let asr = FakeTranscriber::scripted([
            Ok(vec![raw(&[("hello", 1_000, 1_400), ("all", 1_500, 1_900)])]),
            Ok(reference),
        ]);
        let mut post =
            PostProcessor::new(asr, FakeSegmenter::segmentation_3_0(), embedder(), config())
                .unwrap()
                .with_before_write(hook);
        let before_hook = rows(&svc, meeting);
        let result = post.run(
            &svc,
            input_of(meeting, &mic, &them, Vec::new()),
            &mut fresh_rng(),
            &CancelToken::new(),
        );
        if case == "no_flip" {
            assert!(result.is_ok(), "{case}: {result:?}");
        } else {
            assert!(
                matches!(result, Err(RuntimeError::Conflict(KEPT_DECISION_CHANGED))),
                "{case}: {result:?}"
            );
            // Nothing written but the hook's own change.
            let now = rows(&svc, meeting);
            assert_eq!(now.len(), before_hook.len(), "{case}");
            assert!(
                now.iter()
                    .filter(|r| r.segment.track == mic.id)
                    .eq(before_hook.iter().filter(|r| r.segment.track == mic.id)),
                "{case}: the mic is as it was"
            );
        }
    }
}

/// Whether `needle` occurs in the database file or its write-ahead log.
fn on_disk(svc: &Services, needle: &[u8]) -> bool {
    let db = svc.data.db_path();
    ["", "-wal"].iter().any(|suffix| {
        std::fs::read(format!("{}{suffix}", db.display()))
            .is_ok_and(|bytes| bytes.windows(needle.len()).any(|w| w == needle))
    })
}

/// On disk (15.8): a needle in a draft row and in a replaced final line is gone from the
/// database file and its write-ahead log after the pass, and one an edit removed after the
/// debounced flush, with a second connection held open.
#[test]
fn post_replaced_text_leaves_the_file_and_its_log() {
    let dir = TempDir::new();
    let svc = services(&dir);
    let _reader = rusqlite::Connection::open(svc.data.db_path()).unwrap();
    let (meeting, mic, them) = meeting_rows(&svc);
    put_draft(&svc, meeting, &mic, "zebradraftword here", 1_000);
    let asr = FakeTranscriber::scripted([
        Ok(vec![
            raw(&[("hello", 1_000, 1_400), ("all", 1_500, 1_900)]),
            raw(&[("quokkalineword", 31_000, 31_500)]),
        ]),
        Ok(vec![raw(&[("hi", 13_000, 13_300)])]),
    ]);
    run_with(&svc, asr, input_of(meeting, &mic, &them, Vec::new())).unwrap();
    assert!(
        !on_disk(&svc, b"zebradraftword"),
        "the draft's word is gone"
    );
    assert!(
        on_disk(&svc, b"quokkalineword"),
        "the positive control: the line is there"
    );
    // A re-transcription replaces the line.
    let asr = FakeTranscriber::scripted([
        Ok(vec![raw(&[("hello", 1_000, 1_400), ("all", 1_500, 1_900)])]),
        Ok(vec![raw(&[("hi", 13_000, 13_300)])]),
    ]);
    run_with(&svc, asr, input_of(meeting, &mic, &them, Vec::new())).unwrap();
    assert!(
        !on_disk(&svc, b"quokkalineword"),
        "the replaced line's word is gone"
    );
    // An edit removes a word; the debounced flush takes it out of the log.
    svc.set_flush_delay(Duration::from_millis(500));
    let line = rows(&svc, meeting)
        .into_iter()
        .find(|r| r.segment.track == mic.id && !r.meta.deleted && r.pass == TranscriptPass::Final)
        .unwrap();
    crate::edit::edit_segment(&svc, meeting, line.segment.id, "wombateditword").unwrap();
    crate::edit::edit_segment(&svc, meeting, line.segment.id, "hello everyone").unwrap();
    let start = std::time::Instant::now();
    while svc.flushes_run() == 0 && start.elapsed() < Duration::from_secs(30) {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(svc.flushes_run(), 1, "one flush for both edits");
    assert!(
        !on_disk(&svc, b"wombateditword"),
        "the edited-away word is gone"
    );
}

/// A cancelled pass changes nothing: the draft keeps its text.
#[test]
fn post_replaced_a_cancelled_pass_clears_nothing() {
    let dir = TempDir::new();
    let svc = services(&dir);
    let (meeting, mic, them) = meeting_rows(&svc);
    let draft = put_draft(&svc, meeting, &mic, "hello all", 1_000);
    let mut post = PostProcessor::new(
        transcriber(),
        FakeSegmenter::segmentation_3_0(),
        embedder(),
        config(),
    )
    .unwrap();
    let cancel = CancelToken::new();
    cancel.cancel();
    let result = post.run(
        &svc,
        input_of(meeting, &mic, &them, Vec::new()),
        &mut fresh_rng(),
        &cancel,
    );
    assert!(result.is_err());
    let row = rows(&svc, meeting)
        .into_iter()
        .find(|r| r.segment.id == draft)
        .unwrap();
    assert!(row.holds_text() && !row.meta.deleted);
}
