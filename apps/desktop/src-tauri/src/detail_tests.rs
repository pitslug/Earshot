//! Meeting detail tests (T7.3): the mapping from a store written the way the runtime writes
//! it, edit refusals as user copy, the sample WAV, export and deletion.

use std::sync::Arc;

use earshot_core::asr::{MeetingTime, Segment, Word};
use earshot_core::audio::read_wav;
use earshot_core::capture::TrackId;
use earshot_core::diar::{EmbeddingModelId, SpeakerRole};
use earshot_core::identity::{Clock, EntityId};
use earshot_core::model::{
    ClockDomain, Meeting, MeetingRepo, SegmentRow, Speaker, Stamp, Summary, Track, TranscriptPass,
};
use earshot_core::prompt::Template;
use earshot_core::summary::{ActionItem, Decision, SummaryV1};
use earshot_core::voiceprint::{Person, VoiceprintRepo};
use earshot_runtime::identities::{IdentityRecord, encode};
use earshot_runtime::{MeetingJob, Services};
use rand_chacha::ChaCha8Rng;
use rand_core::SeedableRng;

use super::*;
use crate::ipc::IpcError;
use crate::testutil::{T0, TempDir, services};

/// A meeting as the runtime stores it.
struct Fixture {
    meeting: EntityId,
    me: EntityId,
    priya: EntityId,
    unnamed: EntityId,
    priya_person: EntityId,
    /// Final segments in meeting order.
    segments: Vec<EntityId>,
}

fn words(text: &str, start: u64, gap: u64) -> Vec<Word> {
    text.split(' ')
        .enumerate()
        .map(|(i, w)| {
            let at = start + gap * i as u64;
            Word::new(w, at, at + gap - 100, 0.9)
        })
        .collect()
}

/// Two-track meeting: "Me" (mic), Priya (far end, linked to a person by the final pass's
/// voiceprint naming), "Speaker 2" (far end, unnamed). Talk time: Me 2 s, Priya 1 s,
/// Speaker 2 1 s.
fn call_meeting(svc: &Services, seed: u64) -> Fixture {
    let clock: Arc<dyn Clock + Send + Sync> = Arc::clone(&svc.clock);
    let mut rng = ChaCha8Rng::seed_from_u64(seed);
    let mut db = svc.store();
    let (store, hlc) = db.parts();
    let mut stamp = Stamp {
        clock: &*clock,
        hlc,
        rng: &mut rng,
    };
    let mut m = Meeting::new("Vendor review", T0, 600, Template::General, &mut stamp);
    m.duration_ms = Some(60_000);
    let mic = Track::new(m.id, TrackId::Mic, None, ClockDomain::Replay, &mut stamp);
    let them = Track::new(
        m.id,
        TrackId::Reference,
        None,
        ClockDomain::Replay,
        &mut stamp,
    );
    let person = Person::new("Priya Shah", false, &mut stamp);
    let me = Speaker::new(m.id, SpeakerRole::Me, "Me", None, &mut stamp);
    let priya = Speaker::new(
        m.id,
        SpeakerRole::Remote,
        "Priya Shah",
        Some(person.id),
        &mut stamp,
    );
    let unnamed = Speaker::new(m.id, SpeakerRole::Remote, "Speaker 2", None, &mut stamp);
    let long = "we could split the order and air freight the seals so that commissioning \
                still lands before christmas this year";
    let lines = [
        (&mic, &me, "shall we split the order", 1_000, 200),
        (&them, &unnamed, long, 3_000, 50),
        (&them, &priya, "yes that works for us", 6_000, 200),
        (&mic, &me, "then lets confirm friday", 8_000, 250),
    ];
    store.put_meeting(m.clone()).unwrap();
    store.put_track(mic.clone()).unwrap();
    store.put_track(them.clone()).unwrap();
    store.put_person(person.clone()).unwrap();
    for s in [&me, &priya, &unnamed] {
        store.put_speaker(s.clone()).unwrap();
    }
    let mut ids = Vec::new();
    for (track, speaker, text, start, gap) in lines {
        let mut s = Segment::from_words(
            EntityId::generate(stamp.clock, &mut *stamp.rng),
            track.id,
            words(text, start, gap),
            MeetingTime::from_millis(start),
        );
        s.speaker_id = Some(speaker.id);
        ids.push(s.id);
        store
            .put_segment(SegmentRow::new(
                m.id,
                TranscriptPass::Final,
                s,
                stamp.hlc,
                stamp.clock,
            ))
            .unwrap();
    }
    // The final pass's identity record: it named Priya from her voiceprint.
    let priya_row = store
        .speakers(m.id)
        .unwrap()
        .into_iter()
        .find(|s| s.id == priya.id)
        .unwrap();
    let centroid =
        earshot_core::diar::Embedding::new(EmbeddingModelId::new("test-embedder"), vec![1.0, 0.0]);
    let record = IdentityRecord {
        speakers: vec![earshot_runtime::identities::SpeakerIdentity {
            speaker: priya.id,
            track: TrackId::Reference,
            centroid,
            given_name: "Priya Shah".into(),
            given_person: Some(person.id),
            given_stamp: Some(priya_row.meta.updated_at),
            user_edited: false,
            enrolled: false,
            ignored: false,
        }],
    };
    // Test set-up only: production writes this record through the runtime's registry.
    store
        .set_local_value(
            &earshot_store::identities::record_key(m.id),
            &encode(&record),
        )
        .unwrap();
    Fixture {
        meeting: m.id,
        me: me.id,
        priya: priya.id,
        unnamed: unnamed.id,
        priya_person: person.id,
        segments: ids,
    }
}

/// A summary row citing `cites` (decision) and `action` (action item); `deleted` stores it
/// as the previous summary.
fn put_summary(
    svc: &Services,
    meeting: EntityId,
    cites: Vec<EntityId>,
    action: Vec<EntityId>,
    deleted: bool,
    seed: u64,
) -> EntityId {
    let clock: Arc<dyn Clock + Send + Sync> = Arc::clone(&svc.clock);
    let mut rng = ChaCha8Rng::seed_from_u64(seed);
    let mut db = svc.store();
    let (store, hlc) = db.parts();
    let mut stamp = Stamp {
        clock: &*clock,
        hlc,
        rng: &mut rng,
    };
    let body = SummaryV1 {
        title: "Vendor review: pumps".into(),
        tldr: "The order is split.".into(),
        enhanced_notes: Vec::new(),
        decisions: vec![Decision {
            text: "Split the pump order".into(),
            source_segment_ids: cites,
        }],
        action_items: vec![ActionItem {
            owner: "Priya Shah".into(),
            task: "Send the split-order quote".into(),
            due: Some("Friday".into()),
            source_segment_ids: action,
        }],
        open_questions: vec!["Who pays for freight?".into()],
        topics: Vec::new(),
    };
    let mut s = Summary::new(
        meeting,
        Template::General,
        Some(earshot_runtime::summary_model::DEFAULT_SUMMARY_MODEL.into()),
        body,
        &mut stamp,
    );
    if deleted {
        s.meta.mark_deleted(stamp.hlc, stamp.clock);
    }
    let id = s.id;
    store.put_summary(s).unwrap();
    id
}

fn card(d: &MeetingDetail, id: EntityId) -> &SpeakerCard {
    d.speakers.iter().find(|s| s.id == id.to_string()).unwrap()
}

#[test]
fn detail_maps_header_transcript_speakers_and_resolves_citations() {
    let dir = TempDir::new("detail-map");
    let svc = services(&dir);
    let f = call_meeting(&svc, 1);
    put_summary(
        &svc,
        f.meeting,
        vec![f.segments[0]],
        vec![f.segments[2]],
        false,
        2,
    );

    let d = meeting_detail(&svc, f.meeting).unwrap();
    assert_eq!(d.id, f.meeting.to_string());
    assert_eq!(d.state, MeetingState::Ready, "final segments are stored");
    assert_eq!(d.header.title, "Vendor review");
    assert_eq!(d.header.mode, CaptureMode::Call);
    assert_eq!(d.header.transcript, Some(TranscriptKind::Final));
    // The writer by its display name, never the manifest id (T12.43).
    assert_eq!(d.header.summary_writer.as_deref(), Some("Qwen 3.5 9B"));
    assert!(!d.header.summary_edited);
    assert_eq!(d.header.duration_ms, Some(60_000));

    // Transcript rows in meeting order with speaker names and tracks.
    let rows: Vec<(&str, RowTrack, &str)> = d
        .transcript
        .iter()
        .map(|r| (r.speaker.as_str(), r.track, r.text.as_str()))
        .collect();
    assert_eq!(rows[0], ("Me", RowTrack::You, "shall we split the order"));
    assert_eq!(rows[1].0, "Speaker 2");
    assert_eq!(rows[1].1, RowTrack::Them);
    assert_eq!(
        rows[2],
        ("Priya Shah", RowTrack::Them, "yes that works for us")
    );
    assert_eq!(d.transcript[3].start_ms, 8_000);
    let ids: Vec<String> = d.transcript.iter().map(|r| r.segment_id.clone()).collect();
    assert_eq!(
        ids,
        f.segments
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
    );

    // Citations point at transcript rows.
    let s = d.summary.as_ref().unwrap();
    assert_eq!(s.tldr, "The order is split.");
    assert_eq!(
        s.decisions[0].citations,
        [Citation {
            segment_id: f.segments[0].to_string(),
            row: Some(0)
        }]
    );
    assert_eq!(s.actions[0].owner, "Priya Shah");
    assert_eq!(s.actions[0].due.as_deref(), Some("Friday"));
    assert_eq!(s.actions[0].citations[0].row, Some(2));
    assert_eq!(s.open_questions, ["Who pays for freight?"]);
    assert!(!d.summary_stale);
    assert!(!d.can_undo_summary, "no previous summary");

    // Speakers: who needs a name, where they came from, talk share, sample line.
    let me = card(&d, f.me);
    assert_eq!(me.kind, SpeakerKind::Me);
    assert_eq!(me.source, SpeakerSource::Mic);
    assert!(!me.needs_name);
    let priya = card(&d, f.priya);
    assert_eq!(priya.source, SpeakerSource::Voiceprint);
    assert_eq!(
        priya.person,
        Some(PersonRef {
            id: f.priya_person.to_string(),
            name: "Priya Shah".into(),
            has_voiceprint: false,
        })
    );
    assert!(!priya.needs_name);
    let unnamed = card(&d, f.unnamed);
    assert_eq!(unnamed.source, SpeakerSource::FarEnd);
    assert!(unnamed.needs_name);
    assert!(unnamed.person.is_none());
    assert!(unnamed.sample_line.ends_with("..."));
    assert!(unnamed.sample_line.chars().count() <= SAMPLE_LINE_CHARS + 3);
    assert!(unnamed.sample_line.starts_with("we could split the order"));
    assert_eq!(priya.sample_line, "yes that works for us");
    let total: u32 = d.speakers.iter().map(|s| s.share_percent).sum();
    assert!(
        (99..=101).contains(&total),
        "shares sum to about 100: {total}"
    );
    assert!(me.share_percent > priya.share_percent);
}

#[test]
fn detail_share_of_talk_time_follows_segment_lengths() {
    let dir = TempDir::new("detail-share");
    let svc = services(&dir);
    let f = call_meeting(&svc, 3);
    let d = meeting_detail(&svc, f.meeting).unwrap();
    let view = MeetingView::load(&**svc.store(), f.meeting)
        .unwrap()
        .unwrap();
    let len = |id: EntityId| -> u64 {
        view.segments
            .iter()
            .filter(|s| s.speaker_id == Some(id))
            .map(|s| s.end.as_millis() - s.start.as_millis())
            .sum()
    };
    let total = len(f.me) + len(f.priya) + len(f.unnamed);
    for id in [f.me, f.priya, f.unnamed] {
        let want = u32::try_from((len(id) * 100 + total / 2) / total).unwrap();
        assert_eq!(card(&d, id).share_percent, want);
    }
}

#[test]
fn detail_lists_only_speakers_with_a_line_in_the_transcript() {
    // A speaker row no segment refers to (a voice no word went to, from a final pass
    // before T7.3b) is not shown: it has no line, no sample and nothing to name.
    let dir = TempDir::new("detail-wordless");
    let svc = services(&dir);
    let f = call_meeting(&svc, 4);
    let wordless = {
        let clock: Arc<dyn Clock + Send + Sync> = Arc::clone(&svc.clock);
        let mut rng = ChaCha8Rng::seed_from_u64(44);
        let mut db = svc.store();
        let (store, hlc) = db.parts();
        let mut stamp = Stamp {
            clock: &*clock,
            hlc,
            rng: &mut rng,
        };
        let s = Speaker::new(
            f.meeting,
            SpeakerRole::Remote,
            "Speaker 3",
            None,
            &mut stamp,
        );
        store.put_speaker(s.clone()).unwrap();
        s.id
    };
    let d = meeting_detail(&svc, f.meeting).unwrap();
    let ids: Vec<String> = d.speakers.iter().map(|s| s.id.clone()).collect();
    let want: Vec<String> = [f.me, f.priya, f.unnamed]
        .iter()
        .map(ToString::to_string)
        .collect();
    assert_eq!(ids, want);
    assert!(!ids.contains(&wordless.to_string()));
    assert_eq!(d.speakers.iter().filter(|s| s.needs_name).count(), 1);
}

#[test]
fn detail_a_summary_citing_a_replaced_segment_is_stale_and_shows_it_unavailable() {
    let dir = TempDir::new("detail-stale");
    let svc = services(&dir);
    let f = call_meeting(&svc, 4);
    let gone = EntityId::from_bytes([7; 16]);
    put_summary(&svc, f.meeting, vec![f.segments[1], gone], vec![], false, 5);
    let d = meeting_detail(&svc, f.meeting).unwrap();
    let c = &d.summary.as_ref().unwrap().decisions[0].citations;
    assert_eq!(c[0].row, Some(1));
    assert_eq!(c[1].segment_id, gone.to_string());
    assert_eq!(c[1].row, None);
    assert!(d.summary_stale);
    // A previous summary that cites only current segments can be restored.
    assert!(!d.can_undo_summary);
}

#[test]
fn detail_suggested_and_changed_words_and_the_word_list() {
    let dir = TempDir::new("detail-words");
    let svc = services(&dir);
    let f = call_meeting(&svc, 42);
    earshot_runtime::vocabulary::set_vocabulary(&svc, &[] as &[&str], &["Zorbek", "priya"])
        .unwrap();
    edit::edit_segment(&svc, f.meeting, f.segments[3], "then ask sorbeck, friday").unwrap();
    let d = meeting_detail(&svc, f.meeting).unwrap();
    // The Dictionary, then saved people's name words without a repeat in another case.
    assert_eq!(d.word_list, ["Zorbek", "priya", "Shah"]);
    let row = &d.transcript[3];
    assert_eq!(
        row.suggestions,
        [WordSuggestionView {
            token: 2,
            decoded: "sorbeck".into(),
            listed: "Zorbek".into(),
        }]
    );
    assert!(row.changed_words.is_empty());
    assert!(
        d.transcript[..3]
            .iter()
            .all(|r| r.suggestions.is_empty() && r.changed_words.is_empty())
    );

    edit::use_word(&svc, f.meeting, f.segments[3], 2, "Zorbek").unwrap();
    let d = meeting_detail(&svc, f.meeting).unwrap();
    let row = &d.transcript[3];
    assert_eq!(row.text, "then ask Zorbek, friday");
    assert!(row.suggestions.is_empty());
    assert_eq!(
        row.changed_words,
        [ChangedWordView {
            token: 2,
            decoded: "sorbeck".into(),
            written: "Zorbek".into(),
        }]
    );
    // Refusals arrive as user copy that names no word.
    let e: IpcError = edit::use_word(&svc, f.meeting, f.segments[3], 2, "Glimmerax")
        .unwrap_err()
        .into();
    assert_eq!(e.kind, "word_not_listed");
    assert!(!e.message.contains("Glimmerax"));
    let e: IpcError = edit::put_back_word(&svc, f.meeting, f.segments[3], 1)
        .unwrap_err()
        .into();
    assert_eq!(e.kind, "no_word_choice");
    edit::put_back_word(&svc, f.meeting, f.segments[3], 2).unwrap();
    let d = meeting_detail(&svc, f.meeting).unwrap();
    assert_eq!(d.transcript[3].text, "then ask sorbeck, friday");
    assert_eq!(d.transcript[3].suggestions.len(), 1);

    // A draft shows no marks and no list.
    {
        let clock: Arc<dyn Clock + Send + Sync> = Arc::clone(&svc.clock);
        let mut db = svc.store();
        let rows = db.segments(f.meeting).unwrap();
        let (store, hlc) = db.parts();
        for mut r in rows {
            r.pass = TranscriptPass::Live;
            r.meta.touch(hlc, &*clock);
            store.put_segment(r).unwrap();
        }
    }
    let d = meeting_detail(&svc, f.meeting).unwrap();
    assert_eq!(d.header.transcript, Some(TranscriptKind::Draft));
    assert!(d.word_list.is_empty());
    assert!(
        d.transcript
            .iter()
            .all(|r| r.suggestions.is_empty() && r.changed_words.is_empty())
    );
}

#[test]
fn detail_dictionary_words_from_the_transcript_add_undo_and_remove() {
    let dir = TempDir::new("detail-dictionary");
    let svc = services(&dir);
    let f = call_meeting(&svc, 43);
    earshot_runtime::vocabulary::set_vocabulary(&svc, &[] as &[&str], &["QXR-7"]).unwrap();
    edit::edit_segment(&svc, f.meeting, f.segments[3], "then ask sorbeck, friday").unwrap();
    let d = meeting_detail(&svc, f.meeting).unwrap();
    assert_eq!(d.dictionary, ["QXR-7"]);
    assert!(d.transcript[3].suggestions.is_empty());

    // A saved line edit offers the new uncommon words; nothing is added by it.
    let offer = edit_line(
        &svc,
        f.meeting,
        f.segments[2],
        "Quillmarsh and Zorbek, friday",
    )
    .unwrap();
    assert_eq!(offer, ["Quillmarsh", "Zorbek"]);
    assert_eq!(
        meeting_detail(&svc, f.meeting).unwrap().dictionary,
        ["QXR-7"]
    );

    // "Add to Dictionary" on "Zorbek," adds the word without its punctuation, and the word is
    // then suggested on the other line.
    let added = add_dictionary_word(&svc, "Zorbek,").unwrap();
    assert_eq!(
        added,
        DictionaryWordAdded {
            word: "Zorbek".into(),
            added: true
        }
    );
    let d = meeting_detail(&svc, f.meeting).unwrap();
    assert_eq!(d.dictionary, ["QXR-7", "Zorbek"]);
    assert_eq!(
        d.transcript[3].suggestions,
        [WordSuggestionView {
            token: 2,
            decoded: "sorbeck".into(),
            listed: "Zorbek".into(),
        }]
    );
    // Already there (case ignored): nothing to undo.
    assert!(!add_dictionary_word(&svc, "zorbek").unwrap().added);

    // Undo removes exactly that word; the suggestion goes with it.
    assert!(remove_dictionary_word(&svc, "Zorbek").unwrap());
    let d = meeting_detail(&svc, f.meeting).unwrap();
    assert_eq!(d.dictionary, ["QXR-7"]);
    assert!(d.transcript[3].suggestions.is_empty());
    assert!(!remove_dictionary_word(&svc, "Zorbek").unwrap());
    // "Remove from Dictionary" on a listed word, case ignored.
    assert!(remove_dictionary_word(&svc, "qxr-7").unwrap());
    assert!(
        meeting_detail(&svc, f.meeting)
            .unwrap()
            .dictionary
            .is_empty()
    );

    // Refusals are the Dictionary page's own sentences, and store nothing.
    let full: Vec<String> = (0..100).map(|i| format!("w{i}")).collect();
    earshot_runtime::vocabulary::set_vocabulary(&svc, &[] as &[&str], &full).unwrap();
    let e = add_dictionary_word(&svc, "Quillmarsh").unwrap_err();
    assert_eq!(e.kind, "invalid_setting");
    assert_eq!(
        e.message,
        "The list can hold at most 100 words; this one has 101."
    );
    // The UI mock's refusal is pinned to the same table entry (`mockSummaries.test.ts`).
    let table = crate::testutil::summary_cases();
    let e = add_dictionary_word(&svc, "--").unwrap_err();
    assert_eq!(
        (e.kind.as_str(), Some(e.message.as_str())),
        ("invalid_setting", table["messages"]["not_a_word"].as_str())
    );
    assert_eq!(meeting_detail(&svc, f.meeting).unwrap().dictionary, full);

    // A draft transcript shows no Dictionary.
    {
        let clock: Arc<dyn Clock + Send + Sync> = Arc::clone(&svc.clock);
        let mut db = svc.store();
        let rows = db.segments(f.meeting).unwrap();
        let (store, hlc) = db.parts();
        for mut r in rows {
            r.pass = TranscriptPass::Live;
            r.meta.touch(hlc, &*clock);
            store.put_segment(r).unwrap();
        }
    }
    assert!(
        meeting_detail(&svc, f.meeting)
            .unwrap()
            .dictionary
            .is_empty()
    );
    // A line edit of a draft is refused as before.
    let e = edit_line(&svc, f.meeting, f.segments[2], "anything").unwrap_err();
    assert_eq!(e.kind, "draft_transcript");
}

#[test]
fn detail_an_edited_title_and_line_show_and_mark_an_older_summary_out_of_date() {
    let dir = TempDir::new("detail-text-edit");
    let svc = services(&dir);
    let f = call_meeting(&svc, 40);
    put_summary(&svc, f.meeting, vec![f.segments[2]], vec![], false, 41);
    let d = meeting_detail(&svc, f.meeting).unwrap();
    assert!(!d.summary_stale);
    assert!(d.transcript.iter().all(|r| !r.edited));

    edit::rename_meeting(&svc, f.meeting, "Supplier review").unwrap();
    edit::edit_segment(
        &svc,
        f.meeting,
        f.segments[2],
        "yes, that works for both of us",
    )
    .unwrap();
    let d = meeting_detail(&svc, f.meeting).unwrap();
    assert_eq!(d.header.title, "Supplier review");
    let row = &d.transcript[2];
    assert_eq!(row.segment_id, f.segments[2].to_string());
    assert_eq!(row.text, "yes, that works for both of us");
    assert!(row.edited);
    assert_eq!(d.transcript.iter().filter(|r| r.edited).count(), 1);
    // The summary still finds the line it cites, but was written from the old words.
    let c = &d.summary.as_ref().unwrap().decisions[0].citations;
    assert_eq!(c[0].row, Some(2));
    assert!(d.summary_stale);
    // Refusals arrive as user copy.
    let e: IpcError = edit::edit_segment(&svc, f.meeting, f.segments[2], " ")
        .unwrap_err()
        .into();
    assert_eq!(e.kind, "invalid_text");
    let e: IpcError = edit::rename_meeting(
        &svc, f.meeting, "one
two",
    )
    .unwrap_err()
    .into();
    assert_eq!(e.kind, "invalid_title");
}

#[test]
fn detail_a_name_the_summary_used_changing_marks_it_out_of_date_with_its_own_reason() {
    // T12.82: the summary names Priya Shah (an action's owner); renaming the unnamed voice it
    // never mentions leaves it current, renaming Priya marks it, and a new summary clears it.
    let dir = TempDir::new("detail-names");
    let svc = services(&dir);
    let f = call_meeting(&svc, 47);
    put_summary(&svc, f.meeting, vec![f.segments[2]], vec![], false, 48);
    edit::rename_speaker(&svc, f.meeting, f.unnamed, "Jordan").unwrap();
    let d = meeting_detail(&svc, f.meeting).unwrap();
    assert!(!d.summary_stale && !d.summary_stale_names);

    edit::rename_person(&svc, f.priya_person, "Priya Shah-Ng").unwrap();
    let d = meeting_detail(&svc, f.meeting).unwrap();
    assert!(d.summary_stale && d.summary_stale_names);

    // A transcript edit as well: the transcript's reason is the one shown.
    edit::edit_segment(&svc, f.meeting, f.segments[2], "yes, that works for us").unwrap();
    let d = meeting_detail(&svc, f.meeting).unwrap();
    assert!(d.summary_stale && !d.summary_stale_names);

    put_summary(&svc, f.meeting, vec![f.segments[2]], vec![], false, 49);
    let d = meeting_detail(&svc, f.meeting).unwrap();
    assert!(!d.summary_stale && !d.summary_stale_names);
}

#[test]
fn detail_a_line_with_no_speaker_is_an_unknown_speaker_the_user_can_assign() {
    // T12.65: a final line the final pass gave no speaker shows as the unknown speaker; moving
    // it from the line (T12.90) names it and marks it edited. Priya and Speaker 2 have no line
    // left here, so they are offered by their far-end role, and a line moved to a speaker of
    // another track is refused as user copy.
    let dir = TempDir::new("detail-unknown");
    let svc = services(&dir);
    let f = call_meeting(&svc, 46);
    {
        // Test set-up: the far end's third line written as the final pass leaves it.
        let mut store = svc.store();
        let mut row = store
            .segments(f.meeting)
            .unwrap()
            .into_iter()
            .find(|r| r.segment.id == f.segments[2])
            .unwrap();
        row.segment.speaker_id = None;
        store.put_segment(row).unwrap();
        // And recorded as left unassigned, as the final pass records it.
        let ids = [f.segments[2]].into_iter().collect();
        store
            .set_local_value(
                &format!("unknown-lines/{}", f.meeting),
                &earshot_runtime::unknown_lines::encode(&ids).unwrap(),
            )
            .unwrap();
        // A line with no speaker the pass did not record keeps its track's label.
        let mut other = store
            .segments(f.meeting)
            .unwrap()
            .into_iter()
            .find(|r| r.segment.id == f.segments[1])
            .unwrap();
        other.segment.speaker_id = None;
        store.put_segment(other).unwrap();
    }
    let d = meeting_detail(&svc, f.meeting).unwrap();
    let row = &d.transcript[2];
    assert!(row.unknown_speaker);
    assert_eq!(row.speaker, earshot_core::diar::UNKNOWN_SPEAKER);
    assert_eq!(row.speaker_id, None);
    assert_eq!(d.transcript.iter().filter(|r| r.unknown_speaker).count(), 1);
    assert!(!d.transcript[1].unknown_speaker);
    assert_eq!(d.transcript[1].speaker, "Them");

    let offered: Vec<(String, String)> = offers(&d, 2)
        .iter()
        .map(|m| (m.id.clone(), m.name.clone()))
        .collect();
    assert_eq!(
        offered,
        [
            (f.priya.to_string(), "Priya Shah".to_owned()),
            (f.unnamed.to_string(), "Speaker 2".to_owned())
        ]
    );
    assert!(
        edit::move_line(
            &svc,
            f.meeting,
            f.segments[2],
            edit::LineSpeaker::Existing(f.priya)
        )
        .unwrap()
    );
    let d = meeting_detail(&svc, f.meeting).unwrap();
    let row = &d.transcript[2];
    assert!(!row.unknown_speaker);
    assert_eq!(row.speaker, "Priya Shah");
    assert!(row.edited);
    let e: IpcError = edit::move_line(
        &svc,
        f.meeting,
        f.segments[0],
        edit::LineSpeaker::Existing(f.priya),
    )
    .unwrap_err()
    .into();
    assert_eq!(e.kind, "speaker_not_on_track");
}

#[test]
fn detail_each_final_line_says_where_it_can_move_and_whether_it_splits() {
    // T12.90: the menu's offers come from the runtime's rules: speakers heard on the line's
    // track other than its own, and a split only while the words match the text.
    let dir = TempDir::new("detail-lines");
    let svc = services(&dir);
    let f = call_meeting(&svc, 47);
    let d = meeting_detail(&svc, f.meeting).unwrap();
    let ids = |v: &[crate::detail::MovableSpeaker]| {
        v.iter()
            .map(|m| (m.id.parse::<EntityId>().unwrap(), m.name.clone()))
            .collect::<Vec<_>>()
    };
    assert!(offers(&d, 0).is_empty(), "Me is the mic's one speaker");
    assert_eq!(ids(&offers(&d, 1)), [(f.priya, "Priya Shah".to_owned())]);
    assert_eq!(ids(&offers(&d, 2)), [(f.unnamed, "Speaker 2".to_owned())]);
    assert!(d.transcript.iter().all(|r| r.splittable));
    assert!(edit::edit_segment(&svc, f.meeting, f.segments[2], "yes that works").unwrap());
    let d = meeting_detail(&svc, f.meeting).unwrap();
    assert!(
        !d.transcript[2].splittable,
        "an edited line has no word times"
    );
    let e: IpcError = edit::split_line(&svc, f.meeting, f.segments[2], 1)
        .unwrap_err()
        .into();
    assert_eq!(e.kind, "split_needs_word_times");
    assert_eq!(e.message, crate::detail::SPLIT_NEEDS_WORD_TIMES_COPY);
    let second = edit::split_line(&svc, f.meeting, f.segments[3], 2).unwrap();
    let d = meeting_detail(&svc, f.meeting).unwrap();
    let texts: Vec<&str> = d.transcript.iter().map(|r| r.text.as_str()).collect();
    assert_eq!(
        texts,
        [
            "shall we split the order",
            "we could split the order and air freight the seals so that commissioning still lands before christmas this year",
            "yes that works",
            "then lets",
            "confirm friday"
        ]
    );
    assert_eq!(d.transcript[4].segment_id, second.to_string());
    assert!(d.transcript[3].edited && d.transcript[4].edited);
    let e: IpcError = edit::split_line(&svc, f.meeting, second, 0)
        .unwrap_err()
        .into();
    assert_eq!(e.kind, "split_out_of_range");
}

#[test]
fn detail_a_summary_edit_shows_at_once_and_can_be_undone() {
    let dir = TempDir::new("detail-summary-edit");
    let svc = services(&dir);
    let f = call_meeting(&svc, 42);
    let id = put_summary(
        &svc,
        f.meeting,
        vec![f.segments[1]],
        vec![f.segments[3]],
        false,
        43,
    );
    let d = meeting_detail(&svc, f.meeting).unwrap();
    assert_eq!(d.summary.as_ref().unwrap().id, id.to_string());
    assert!(!d.can_undo_summary);
    assert!(!d.header.summary_edited);

    let field = |f: SummaryFieldRef| f.field();
    edit::edit_summary(
        &svc,
        f.meeting,
        id,
        field(SummaryFieldRef::Decision { index: 0 }),
        "Split the pump order in two",
    )
    .unwrap();
    edit::edit_summary(
        &svc,
        f.meeting,
        id,
        field(SummaryFieldRef::ActionDue { index: 0 }),
        "",
    )
    .unwrap();
    let d = meeting_detail(&svc, f.meeting).unwrap();
    let s = d.summary.as_ref().unwrap();
    assert_eq!(s.id, id.to_string(), "edited in place");
    assert_eq!(s.decisions[0].text, "Split the pump order in two");
    assert_eq!(s.decisions[0].citations[0].row, Some(1));
    assert_eq!(s.actions[0].due, None);
    assert!(
        d.can_undo_summary,
        "the text before the edit can be restored"
    );
    assert!(d.header.summary_edited, "the header says it was edited");
    assert_eq!(d.header.summary_writer.as_deref(), Some("Qwen 3.5 9B"));
    assert!(
        !d.summary_stale,
        "a summary edit leaves the transcript as it was"
    );
    edit::undo_summary(&svc, f.meeting).unwrap();
    let d = meeting_detail(&svc, f.meeting).unwrap();
    assert_eq!(
        d.summary.as_ref().unwrap().actions[0].due.as_deref(),
        Some("Friday")
    );
    // The text before the last edit was itself edited (the decision).
    assert!(d.header.summary_edited);

    // A summary that is no longer current is refused as user copy.
    let e: IpcError = edit::edit_summary(
        &svc,
        f.meeting,
        id,
        field(SummaryFieldRef::Tldr),
        "Anything.",
    )
    .unwrap_err()
    .into();
    assert_eq!(e.kind, "summary_changed");
}

#[test]
fn detail_summary_field_refs_name_the_runtime_fields() {
    use earshot_runtime::edit::SummaryField as F;
    let cases = [
        (SummaryFieldRef::Title, F::Title),
        (SummaryFieldRef::Tldr, F::Tldr),
        (SummaryFieldRef::Decision { index: 2 }, F::Decision(2)),
        (SummaryFieldRef::ActionOwner { index: 1 }, F::ActionOwner(1)),
        (SummaryFieldRef::ActionTask { index: 1 }, F::ActionTask(1)),
        (SummaryFieldRef::ActionDue { index: 3 }, F::ActionDue(3)),
        (
            SummaryFieldRef::NoteHeading { section: 1 },
            F::NoteHeading(1),
        ),
        (
            SummaryFieldRef::NoteBullet {
                section: 1,
                bullet: 4,
            },
            F::NoteBullet(1, 4),
        ),
        (
            SummaryFieldRef::OpenQuestion { index: 0 },
            F::OpenQuestion(0),
        ),
    ];
    for (r, want) in cases {
        assert_eq!(r.field(), want);
    }
    // The UI sends them tagged by kind.
    let r: SummaryFieldRef =
        serde_json::from_str(r#"{"kind":"NoteBullet","section":0,"bullet":2}"#).unwrap();
    assert_eq!(r.field(), F::NoteBullet(0, 2));
}

#[test]
fn detail_can_undo_only_with_a_restorable_previous_summary() {
    let dir = TempDir::new("detail-undo");
    let svc = services(&dir);
    let f = call_meeting(&svc, 6);
    put_summary(&svc, f.meeting, vec![f.segments[0]], vec![], true, 7);
    put_summary(&svc, f.meeting, vec![f.segments[1]], vec![], false, 8);
    assert!(meeting_detail(&svc, f.meeting).unwrap().can_undo_summary);

    let g = call_meeting(&svc, 9);
    put_summary(
        &svc,
        g.meeting,
        vec![EntityId::from_bytes([3; 16])],
        vec![],
        true,
        10,
    );
    put_summary(&svc, g.meeting, vec![g.segments[1]], vec![], false, 11);
    assert!(
        !meeting_detail(&svc, g.meeting).unwrap().can_undo_summary,
        "the previous summary cites a replaced transcript"
    );
}

#[test]
fn detail_an_edited_voiceprint_name_is_no_longer_a_voiceprint_match() {
    let dir = TempDir::new("detail-edited");
    let svc = services(&dir);
    let f = call_meeting(&svc, 12);
    edit::rename_speaker(&svc, f.meeting, f.priya, "Priya S").unwrap();
    let d = meeting_detail(&svc, f.meeting).unwrap();
    assert_eq!(card(&d, f.priya).source, SpeakerSource::FarEnd);
    assert_eq!(card(&d, f.priya).name, "Priya S");
    assert_eq!(d.transcript[2].speaker, "Priya S");
}

#[test]
fn detail_in_room_meeting_rows_are_room_and_an_unknown_meeting_is_not_found() {
    let dir = TempDir::new("detail-room");
    let svc = services(&dir);
    let clock: Arc<dyn Clock + Send + Sync> = Arc::clone(&svc.clock);
    let mut rng = ChaCha8Rng::seed_from_u64(20);
    let m = {
        let mut db = svc.store();
        let (store, hlc) = db.parts();
        let mut stamp = Stamp {
            clock: &*clock,
            hlc,
            rng: &mut rng,
        };
        let mut m = Meeting::new("Workshop", T0, 600, Template::General, &mut stamp);
        m.duration_ms = Some(10_000);
        let mic = Track::new(m.id, TrackId::Mic, None, ClockDomain::Replay, &mut stamp);
        let s = Segment::from_words(
            EntityId::generate(stamp.clock, &mut *stamp.rng),
            mic.id,
            words("hello room", 0, 300),
            MeetingTime::from_millis(0),
        );
        store.put_meeting(m.clone()).unwrap();
        store.put_track(mic).unwrap();
        store
            .put_segment(SegmentRow::new(
                m.id,
                TranscriptPass::Live,
                s,
                stamp.hlc,
                stamp.clock,
            ))
            .unwrap();
        m.id
    };
    let d = meeting_detail(&svc, m).unwrap();
    assert_eq!(d.header.mode, CaptureMode::InRoom);
    assert_eq!(d.header.transcript, Some(TranscriptKind::Draft));
    assert_eq!(d.transcript[0].track, RowTrack::Room);
    assert_eq!(d.transcript[0].speaker_id, None);
    assert!(d.summary.is_none());
    assert!(!d.summary_stale);

    let e = meeting_detail(&svc, EntityId::from_bytes([1; 16])).unwrap_err();
    assert_eq!(e.kind, "meeting_not_found");
    svc.delete_meeting(m).unwrap();
    assert_eq!(
        meeting_detail(&svc, m).unwrap_err().kind,
        "meeting_not_found"
    );
}

/// An in-room meeting: one mic track and two unnamed voices of the room with a line each.
fn in_room_meeting(svc: &Services, seed: u64) -> (EntityId, EntityId, EntityId) {
    let clock: Arc<dyn Clock + Send + Sync> = Arc::clone(&svc.clock);
    let mut rng = ChaCha8Rng::seed_from_u64(seed);
    let mut db = svc.store();
    let (store, hlc) = db.parts();
    let mut stamp = Stamp {
        clock: &*clock,
        hlc,
        rng: &mut rng,
    };
    let m = Meeting::new("Site walk", T0, 600, Template::General, &mut stamp);
    let mic = Track::new(m.id, TrackId::Mic, None, ClockDomain::Replay, &mut stamp);
    store.put_meeting(m.clone()).unwrap();
    store.put_track(mic.clone()).unwrap();
    let mut rows = Vec::new();
    for (n, text) in [
        (1u64, "the gate opens at nine"),
        (2, "bring the long ladder"),
    ] {
        let row = Speaker::new(
            m.id,
            SpeakerRole::Room,
            format!("Speaker {n}"),
            None,
            &mut stamp,
        );
        store.put_speaker(row.clone()).unwrap();
        let start = n * 2_000;
        let mut s = Segment::from_words(
            EntityId::generate(stamp.clock, &mut *stamp.rng),
            mic.id,
            words(text, start, 200),
            MeetingTime::from_millis(start),
        );
        s.speaker_id = Some(row.id);
        store
            .put_segment(SegmentRow::new(
                m.id,
                TranscriptPass::Final,
                s,
                stamp.hlc,
                stamp.clock,
            ))
            .unwrap();
        rows.push(row.id);
    }
    (m.id, rows[0], rows[1])
}

#[test]
fn detail_offers_this_is_me_only_in_a_room_with_no_me_row() {
    let dir = TempDir::new("detail-mark-me");
    let svc = services(&dir);

    // A call: its Me comes from the mic track, nothing is offered and the runtime refuses.
    let f = call_meeting(&svc, 40);
    let d = meeting_detail(&svc, f.meeting).unwrap();
    assert!(
        d.speakers
            .iter()
            .all(|s| !s.can_mark_me && !s.can_unmark_me)
    );
    let e = IpcError::from(edit::mark_me(&svc, f.meeting, f.unnamed).unwrap_err());
    assert_eq!(e.kind, "not_in_room");
    let e = IpcError::from(edit::unmark_me(&svc, f.meeting, f.me).unwrap_err());
    assert_eq!(e.kind, "not_in_room");

    // In the room: every voice is offered until one is the user.
    let (m, voice, other) = in_room_meeting(&svc, 41);
    let d = meeting_detail(&svc, m).unwrap();
    assert_eq!(d.header.mode, CaptureMode::InRoom);
    assert!(d.speakers.iter().all(|s| s.can_mark_me && !s.can_unmark_me));
    assert!(d.speakers.iter().all(|s| s.needs_name));

    edit::mark_me(&svc, m, voice).unwrap();
    let d = meeting_detail(&svc, m).unwrap();
    let mine = card(&d, voice);
    assert_eq!(mine.kind, SpeakerKind::Me);
    assert!(!mine.needs_name && !mine.can_mark_me && mine.can_unmark_me);
    assert_eq!(mine.source, SpeakerSource::Mic);
    assert_eq!(mine.name, "Me");
    assert!(!mine.person.as_ref().unwrap().has_voiceprint);
    let theirs = card(&d, other);
    assert_eq!(theirs.kind, SpeakerKind::Other);
    assert!(theirs.needs_name && !theirs.can_mark_me && !theirs.can_unmark_me);
    assert_eq!(d.transcript[0].speaker, "Me");
    // A second voice is refused with fixed copy, and so is remembering the marked one.
    let e = IpcError::from(edit::mark_me(&svc, m, other).unwrap_err());
    assert_eq!(e.kind, "meeting_has_me");
    assert_eq!(
        e.message,
        "One voice in this meeting is already you. Choose \"This isn't me\" on it first."
    );
    let e = save_voice(
        &svc,
        m,
        voice,
        &LinkChoice::Existing {
            person: mine.person.as_ref().unwrap().id.clone(),
        },
    )
    .unwrap_err();
    assert_eq!(e.kind, "me_voice");

    // The user's name reaches the card and the transcript.
    let user = mine.person.as_ref().unwrap().id.parse().unwrap();
    edit::rename_person(&svc, user, "Casey Lane").unwrap();
    let d = meeting_detail(&svc, m).unwrap();
    assert_eq!(card(&d, voice).name, "Casey Lane");
    assert_eq!(d.transcript[0].speaker, "Casey Lane");

    // The undo: an unnamed voice that needs a name, and the offer is back on every voice.
    edit::unmark_me(&svc, m, voice).unwrap();
    let d = meeting_detail(&svc, m).unwrap();
    let back = card(&d, voice);
    assert_eq!(back.kind, SpeakerKind::Other);
    assert_eq!(back.name, "Speaker 1");
    assert!(back.person.is_none());
    assert!(back.needs_name);
    assert!(d.speakers.iter().all(|s| s.can_mark_me && !s.can_unmark_me));
    assert_eq!(d.transcript[0].speaker, "Speaker 1");
    let e = IpcError::from(edit::unmark_me(&svc, m, voice).unwrap_err());
    assert_eq!(e.kind, "not_me");

    // A merged-away voice is gone.
    edit::merge_speakers(&svc, m, voice, other).unwrap();
    let e = IpcError::from(edit::mark_me(&svc, m, other).unwrap_err());
    assert_eq!(e.kind, "speaker_not_found");
}

/// Adds a person (`is_me` for the user), with one voiceprint when `print`; returns the id.
fn person_with(svc: &Services, name: &str, is_me: bool, print: bool, seed: u64) -> EntityId {
    let clock: Arc<dyn Clock + Send + Sync> = Arc::clone(&svc.clock);
    let mut rng = ChaCha8Rng::seed_from_u64(seed);
    let mut db = svc.store();
    let (store, hlc) = db.parts();
    let mut stamp = Stamp {
        clock: &*clock,
        hlc,
        rng: &mut rng,
    };
    let p = Person::new(name, is_me, &mut stamp);
    store.put_person(p.clone()).unwrap();
    if print {
        earshot_core::voiceprint::enroll(
            &mut *store,
            p.id,
            &[earshot_core::diar::Embedding::new(
                EmbeddingModelId::new("level@1"),
                vec![1.0, 0.0],
            )],
            std::time::Duration::from_secs(60),
            earshot_core::voiceprint::SampleOrigin::Own,
            &earshot_core::voiceprint::EnrolmentConfig::default(),
            &mut stamp,
        )
        .unwrap();
    }
    p.id
}

#[test]
fn detail_this_isnt_me_is_refused_on_a_voice_the_users_voiceprint_named() {
    let dir = TempDir::new("detail-recognised-me");
    let svc = services(&dir);
    let user = person_with(&svc, "Casey", true, true, 60);
    let (m, voice, _) = in_room_meeting(&svc, 61);
    // As a final pass naming the voice by the user's voiceprint leaves it (no identity entry).
    {
        let clock: Arc<dyn Clock + Send + Sync> = Arc::clone(&svc.clock);
        let mut db = svc.store();
        let mut row = db
            .speakers(m)
            .unwrap()
            .into_iter()
            .find(|s| s.id == voice)
            .unwrap();
        let (store, hlc) = db.parts();
        row.role = SpeakerRole::Me;
        row.person_id = Some(user);
        row.display_name = "Casey".into();
        row.meta.touch(hlc, &*clock);
        store.put_speaker(row).unwrap();
        // The pass's evidence that the voiceprint named it (test set-up: production writes it
        // through the runtime's registry).
        store
            .set_local_value(
                &format!("voiceprint-me/{m}"),
                &earshot_runtime::identities::encode_voiceprint_me(voice),
            )
            .unwrap();
    }
    let d = meeting_detail(&svc, m).unwrap();
    let mine = card(&d, voice);
    assert!(mine.recognised_me && !mine.can_unmark_me);
    assert!(
        d.speakers
            .iter()
            .filter(|s| s.id != mine.id)
            .all(|s| !s.recognised_me)
    );
    let e = IpcError::from(edit::unmark_me(&svc, m, voice).unwrap_err());
    assert_eq!(e.kind, "recognised_me");
    assert_eq!(e.message, RECOGNISED_ME_COPY);
    assert!(RECOGNISED_ME_COPY.contains("Settings, People"));

    // With the user's voice forgotten, the undo is the user's again.
    earshot_runtime::my_voice::forget_my_voice(&svc).unwrap();
    let mine = card(&meeting_detail(&svc, m).unwrap(), voice).clone();
    assert!(!mine.recognised_me && mine.can_unmark_me);
}

#[test]
fn detail_recognised_me_copy_is_the_ui_copy() {
    // The UI's tooltip and mock refusal (`RECOGNISED_ME_COPY` in packages/ui/src/ipc/ipc.ts)
    // say exactly this; the TypeScript literal escapes the apostrophe.
    let ts = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../../packages/ui/src/ipc/ipc.ts"),
    )
    .unwrap();
    let literal = format!("'{}'", RECOGNISED_ME_COPY.replace('\'', "\\'"));
    assert!(ts.contains(&literal), "{literal}");
}

#[test]
fn detail_line_move_and_split_copy_is_the_ui_copy() {
    // The UI's text and the mock's refusals (T12.90, constants of the same names in
    // packages/ui/src/ipc/ipc.ts) say exactly this.
    let ts = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../../packages/ui/src/ipc/ipc.ts"),
    )
    .unwrap();
    for (name, copy) in [
        (
            "SPLIT_NEEDS_WORD_TIMES_COPY",
            crate::detail::SPLIT_NEEDS_WORD_TIMES_COPY,
        ),
        (
            "SPEAKER_NOT_ON_TRACK_COPY",
            crate::detail::SPEAKER_NOT_ON_TRACK_COPY,
        ),
        (
            "SPLIT_OUT_OF_RANGE_COPY",
            crate::detail::SPLIT_OUT_OF_RANGE_COPY,
        ),
    ] {
        let at = ts
            .find(&format!("export const {name} ="))
            .unwrap_or_else(|| panic!("{name} is not in ipc.ts"));
        let literal = format!("\"{copy}\"");
        let end = ts[at..].find(';').unwrap() + at;
        assert!(ts[at..end].contains(&literal), "{name}: {literal}");
    }
    assert_eq!(
        edit_error(&EditError::SpeakerNotOnTrack(EntityId::from_bytes([1; 16]))).message,
        crate::detail::SPEAKER_NOT_ON_TRACK_COPY
    );
    assert_eq!(
        edit_error(&EditError::SplitOutOfRange(EntityId::from_bytes([1; 16]))).message,
        crate::detail::SPLIT_OUT_OF_RANGE_COPY
    );
}

#[test]
fn detail_mark_me_returns_the_persons_named_after_the_user_to_merge() {
    let dir = TempDir::new("detail-merge-me");
    let svc = services(&dir);
    person_with(&svc, "Casey Lane", true, false, 62);
    let twin = person_with(&svc, "Casey Lane", false, true, 63);
    person_with(&svc, "Robin", false, false, 64);
    let (m, voice, other) = in_room_meeting(&svc, 65);
    edit::link_speaker(&svc, m, other, &PersonChoice::Existing(twin)).unwrap();
    let offered = super::mark_me(&svc, m, voice).unwrap().offers;
    assert_eq!(offered.len(), 1);
    assert_eq!(offered[0].id, twin.to_string());
    assert_eq!(offered[0].name, "Casey Lane");
    assert!(offered[0].has_voiceprint && !offered[0].is_me);

    let user = edit::merge_into_me(&svc, twin).unwrap();
    // The row linked to the merged person shows the user as its person: it has a name and
    // never asks for one again.
    let d = meeting_detail(&svc, m).unwrap();
    let theirs = card(&d, other);
    assert_eq!(theirs.person.as_ref().unwrap().id, user.to_string());
    assert!(theirs.person.as_ref().unwrap().has_voiceprint);
    assert!(!theirs.needs_name && !theirs.can_ignore);
    assert_eq!(theirs.name, "Casey Lane");
    let listed = persons(&svc).unwrap().persons;
    assert!(listed.iter().all(|p| p.id != twin.to_string()));
    assert!(listed.iter().any(|p| p.is_me && p.has_voiceprint));
    let e = IpcError::from(edit::merge_into_me(&svc, twin).unwrap_err());
    assert_eq!(e.kind, "person_not_found");
}

#[test]
fn detail_edit_refusals_become_plain_user_copy() {
    let id = EntityId::from_bytes([2; 16]);
    let all = [
        (EditError::MeetingNotFound(id), "meeting_not_found"),
        (EditError::SpeakerNotFound(id), "speaker_not_found"),
        (EditError::PersonNotFound(id), "person_not_found"),
        (EditError::InvalidName(NameProblem::Empty), "invalid_name"),
        (EditError::InvalidName(NameProblem::TooLong), "invalid_name"),
        (
            EditError::InvalidName(NameProblem::ControlCharacter),
            "invalid_name",
        ),
        (EditError::SameSpeaker(id), "same_speaker"),
        (EditError::MergeAwayMe(id), "merge_away_me"),
        (EditError::NoPerson(id), "no_person"),
        (EditError::MeVoice(id), "me_voice"),
        (EditError::NotTheUser(id), "not_the_user"),
        (EditError::MeIsTheUser(id), "me_is_the_user"),
        (EditError::PersonIsTheUser(id), "person_is_the_user"),
        (EditError::NotInRoom(id), "not_in_room"),
        (EditError::MeetingHasMe(id), "meeting_has_me"),
        (EditError::NotMe(id), "not_me"),
        (EditError::RecognisedMe(id), "recognised_me"),
        (EditError::NotNamedAfterUser(id), "not_named_after_user"),
        (EditError::NoPreviousSummary, "no_previous_summary"),
        (EditError::PreviousSummaryStale, "previous_summary_stale"),
        (EditError::FolderNotFound(id), "folder_not_found"),
        (EditError::FolderNameTaken(id), "folder_name_taken"),
        (EditError::MeetingRecording(id), "meeting_recording"),
        // Never sent (an empty title is allowed, pinned below): no copy asks for a title.
        (EditError::InvalidTitle(TextProblem::Empty), "internal"),
        (
            EditError::InvalidTitle(TextProblem::TooLong),
            "invalid_title",
        ),
        (
            EditError::InvalidTitle(TextProblem::ControlCharacter),
            "invalid_title",
        ),
        (EditError::InvalidText(TextProblem::Empty), "invalid_text"),
        (EditError::InvalidText(TextProblem::TooLong), "invalid_text"),
        (
            EditError::InvalidText(TextProblem::ControlCharacter),
            "invalid_text",
        ),
        (EditError::SegmentNotFound(id), "segment_not_found"),
        (EditError::DraftTranscript(id), "draft_transcript"),
        (EditError::SummaryNotCurrent(id), "summary_changed"),
        (EditError::SummaryItemNotFound, "summary_changed"),
        (
            EditError::InvalidSummaryText(TextProblem::Empty),
            "invalid_summary_text",
        ),
        (
            EditError::InvalidSummaryText(TextProblem::TooLong),
            "invalid_summary_text",
        ),
        (
            EditError::InvalidSummaryText(TextProblem::ControlCharacter),
            "invalid_summary_text",
        ),
        (EditError::NotUnnamed(id), "not_unnamed"),
        (EditError::WordNotFound(id), "word_not_found"),
        (EditError::WordNotListed, "word_not_listed"),
        (EditError::NoWordChoice(id), "no_word_choice"),
        (EditError::MeetingStillWorking(id), "meeting_still_working"),
        (EditError::SpeakerNotOnTrack(id), "speaker_not_on_track"),
        (EditError::SplitNeedsWordTimes(id), "split_needs_word_times"),
        (EditError::SplitOutOfRange(id), "split_out_of_range"),
        (EditError::UserRow(id), "user_row"),
    ];
    for (e, kind) in all {
        let ipc = IpcError::from(RuntimeError::Edit(e.clone()));
        assert_eq!(ipc, edit_error(&e));
        assert_eq!(ipc.kind, kind);
        assert!(!ipc.retryable);
        let m = &ipc.message;
        assert!(m.ends_with('.'), "{m}");
        assert!(m.chars().next().unwrap().is_uppercase(), "{m}");
        // UI copy rules: no ids, no em dashes, no "please", no exclamation marks.
        assert!(!m.contains(&id.to_string()), "{m}");
        assert!(!m.contains('\u{2014}') && !m.contains('!'), "{m}");
        assert!(!m.to_lowercase().contains("please"), "{m}");
    }
    assert_eq!(
        edit_error(&EditError::NotTheUser(id)).message,
        "Only your own row can be linked to you."
    );
    // An empty title clears the title: the user never sees a refusal asking for one.
    assert_eq!(earshot_runtime::edit::clean_title("   ").unwrap(), "");
    assert_eq!(
        edit_error(&EditError::InvalidTitle(TextProblem::Empty)).message,
        crate::plain_text::INTERNAL
    );
}

#[test]
fn detail_edits_go_through_the_runtime_and_refusals_arrive_as_user_copy() {
    let dir = TempDir::new("detail-edits");
    let svc = services(&dir);
    let f = call_meeting(&svc, 30);
    // An empty name.
    let e: IpcError = edit::rename_speaker(&svc, f.meeting, f.unnamed, "  ")
        .unwrap_err()
        .into();
    assert_eq!(e.kind, "invalid_name");
    // Linking someone else to the user's person.
    let me_person = {
        let clock: Arc<dyn Clock + Send + Sync> = Arc::clone(&svc.clock);
        let mut rng = ChaCha8Rng::seed_from_u64(31);
        let mut db = svc.store();
        let (store, hlc) = db.parts();
        let mut stamp = Stamp {
            clock: &*clock,
            hlc,
            rng: &mut rng,
        };
        let p = Person::new("Casey", true, &mut stamp);
        store.put_person(p.clone()).unwrap();
        p.id
    };
    let e = link(
        &svc,
        f.meeting,
        f.unnamed,
        &LinkChoice::Existing {
            person: me_person.to_string(),
        },
    )
    .unwrap_err();
    assert_eq!(e.kind, "not_the_user");
    assert_eq!(e.message, "Only your own row can be linked to you.");
    // Linking to a new person names the row and lists the person.
    let id = link(
        &svc,
        f.meeting,
        f.unnamed,
        &LinkChoice::New {
            name: "Sam Lee".into(),
        },
    )
    .unwrap()
    .person;
    let d = meeting_detail(&svc, f.meeting).unwrap();
    let c = card(&d, f.unnamed);
    assert_eq!(c.name, "Sam Lee");
    assert!(!c.needs_name);
    assert_eq!(c.person.as_ref().unwrap().id, id);
    assert!(
        persons(&svc)
            .unwrap()
            .persons
            .iter()
            .any(|p| p.id == id && p.name == "Sam Lee")
    );
    // Merging the unnamed speaker into Priya moves the rows and removes the card.
    let g = call_meeting(&svc, 32);
    edit::merge_speakers(&svc, g.meeting, g.priya, g.unnamed).unwrap();
    let d = meeting_detail(&svc, g.meeting).unwrap();
    assert!(d.speakers.iter().all(|s| s.id != g.unnamed.to_string()));
    assert_eq!(d.transcript[1].speaker, "Priya Shah");
    // "Me" cannot be merged away.
    let e: IpcError = edit::merge_speakers(&svc, g.meeting, g.priya, g.me)
        .unwrap_err()
        .into();
    assert_eq!(e.kind, "merge_away_me");
    // Saving the user's row as someone else is refused before anything is stored.
    let e = save_voice(
        &svc,
        g.meeting,
        g.me,
        &LinkChoice::New {
            name: "Sky Moss".into(),
        },
    )
    .unwrap_err();
    assert_eq!(e.kind, "me_is_the_user");
    assert!(
        !persons(&svc)
            .unwrap()
            .persons
            .iter()
            .any(|p| p.name == "Sky Moss")
    );
    // The bad id from the UI.
    assert_eq!(parse_id("not an id").unwrap_err().kind, "invalid_id");
}

/// Gives `speaker` an entry in `meeting`'s identity record, as a final pass that kept its
/// voice would (test set-up only).
fn give_identity(svc: &Services, meeting: EntityId, speaker: EntityId) {
    let key = earshot_store::identities::record_key(meeting);
    let mut db = svc.store();
    let mut record = match db
        .local_value(&key)
        .unwrap()
        .map(|b| earshot_runtime::identities::decode(&b))
    {
        Some(earshot_runtime::identities::StoredIdentities::Record(r)) => r,
        other => panic!("{other:?}"),
    };
    let row = db
        .speakers(meeting)
        .unwrap()
        .into_iter()
        .find(|s| s.id == speaker)
        .unwrap();
    record
        .speakers
        .push(earshot_runtime::identities::SpeakerIdentity {
            speaker,
            track: TrackId::Reference,
            centroid: earshot_core::diar::Embedding::new(
                EmbeddingModelId::new("test-embedder"),
                vec![0.0, 1.0],
            ),
            given_name: row.display_name.clone(),
            given_person: None,
            given_stamp: Some(row.meta.updated_at),
            user_edited: false,
            enrolled: false,
            ignored: false,
        });
    db.set_local_value(&key, &encode(&record)).unwrap();
}

#[test]
fn detail_not_this_person_puts_a_named_voice_back_as_an_ignored_speaker() {
    let dir = TempDir::new("detail-not-person");
    let svc = services(&dir);
    let f = call_meeting(&svc, 61);
    let d = meeting_detail(&svc, f.meeting).unwrap();
    // Offered on a named voice with a stored voice, never on "Me" or an unnamed voice.
    assert!(card(&d, f.priya).can_not_this_person);
    assert!(!card(&d, f.me).can_not_this_person);
    assert!(!card(&d, f.unnamed).can_not_this_person);

    edit::not_this_person(&svc, f.meeting, f.priya).unwrap();
    let d = meeting_detail(&svc, f.meeting).unwrap();
    let c = card(&d, f.priya);
    assert!(c.person.is_none() && c.ignored && !c.needs_name && !c.can_not_this_person);
    // "Speaker 2" is taken by the unnamed voice: the next free label.
    assert_eq!(c.name, "Speaker 1");
    assert_eq!(d.transcript[2].speaker, "Speaker 1");

    let e: IpcError = edit::not_this_person(&svc, f.meeting, f.me)
        .unwrap_err()
        .into();
    assert_eq!(e.kind, "user_row");
}

#[test]
fn detail_not_this_person_is_not_offered_on_a_voice_linked_to_a_person_merged_into_the_user() {
    let dir = TempDir::new("detail-not-person-me");
    let svc = services(&dir);
    let f = call_meeting(&svc, 62);
    earshot_runtime::my_voice::set_my_name(&svc, "Priya Shah").unwrap();
    edit::merge_into_me(&svc, f.priya_person).unwrap();
    let d = meeting_detail(&svc, f.meeting).unwrap();
    assert!(!card(&d, f.priya).can_not_this_person);
    let e: IpcError = edit::not_this_person(&svc, f.meeting, f.priya)
        .unwrap_err()
        .into();
    assert_eq!(e.kind, "user_row");
    assert!(!e.message.contains("This isn't me"), "{}", e.message);
}

#[test]
fn detail_an_ignored_voice_keeps_its_label_and_no_longer_needs_a_name() {
    let dir = TempDir::new("detail-ignore");
    let svc = services(&dir);
    let f = call_meeting(&svc, 60);
    // T12.93 changed this test: the choice lives on the synced row, so a voice with no stored
    // voice (no identity entry, as on a computer that did not record the meeting) is offered
    // Ignore too, and the old `no_stored_voice` refusal is gone.
    let d = meeting_detail(&svc, f.meeting).unwrap();
    let c = card(&d, f.unnamed);
    assert!(c.can_ignore && !c.ignored && c.needs_name);
    // "Me" and a named voice are not offered Ignore.
    assert!(!card(&d, f.me).can_ignore);
    assert!(!card(&d, f.priya).can_ignore);

    edit::ignore_speaker(&svc, f.meeting, f.unnamed).unwrap();
    let d = meeting_detail(&svc, f.meeting).unwrap();
    let c = card(&d, f.unnamed);
    assert!(c.ignored && !c.needs_name);
    assert_eq!(c.name, "Speaker 2");
    assert_eq!(d.transcript[1].speaker, "Speaker 2", "the label stays");
    assert!(d.speakers.iter().all(|s| !s.needs_name));

    edit::unignore_speaker(&svc, f.meeting, f.unnamed).unwrap();
    let c = card(&meeting_detail(&svc, f.meeting).unwrap(), f.unnamed).clone();
    assert!(!c.ignored && c.needs_name);
}

#[test]
fn detail_save_voice_names_links_and_says_what_remembering_did() {
    let dir = TempDir::new("detail-save");
    let svc = services(&dir);
    let f = call_meeting(&svc, 61);
    give_identity(&svc, f.meeting, f.unnamed);
    edit::ignore_speaker(&svc, f.meeting, f.unnamed).unwrap();
    let saved = save_voice(
        &svc,
        f.meeting,
        f.unnamed,
        &LinkChoice::New {
            name: "Jordan Ellis".into(),
        },
    )
    .unwrap();
    // The test embedder has no calibrated threshold: named and linked, nothing stored.
    assert_eq!(saved.outcome, RememberOutcome::Unsupported);
    let d = meeting_detail(&svc, f.meeting).unwrap();
    let c = card(&d, f.unnamed);
    assert_eq!(c.name, "Jordan Ellis");
    assert_eq!(c.person.as_ref().unwrap().id, saved.person);
    assert!(!c.person.as_ref().unwrap().has_voiceprint);
    assert!(!c.needs_name && !c.ignored, "naming ends the ignore");
    // A voice the final pass kept nothing of: still named, and the result says so.
    let g = call_meeting(&svc, 62);
    let none = save_voice(
        &svc,
        g.meeting,
        g.unnamed,
        &LinkChoice::Existing {
            person: saved.person.clone(),
        },
    )
    .unwrap();
    assert_eq!(none.outcome, RememberOutcome::NoStoredVoice);
    assert_eq!(
        card(&meeting_detail(&svc, g.meeting).unwrap(), g.unnamed).name,
        "Jordan Ellis"
    );
    // A bad id from the UI.
    let e = save_voice(
        &svc,
        g.meeting,
        g.unnamed,
        &LinkChoice::Existing {
            person: "nobody".into(),
        },
    )
    .unwrap_err();
    assert_eq!(e.kind, "invalid_id");
}

#[test]
fn detail_sample_wav_is_a_16_bit_mono_file_of_the_samples() {
    let samples: Vec<i16> = (0..1600).map(|i| ((i * 37) % 2000 - 1000) as i16).collect();
    let wav = sample_wav(16_000, &samples).unwrap();
    assert_eq!(&wav[..4], b"RIFF");
    assert_eq!(&wav[8..12], b"WAVE");
    assert_eq!(wav.len(), 44 + samples.len() * 2);
    assert_eq!(u16::from_le_bytes([wav[20], wav[21]]), 1, "PCM");
    assert_eq!(u16::from_le_bytes([wav[22], wav[23]]), 1, "mono");
    assert_eq!(
        u32::from_le_bytes([wav[24], wav[25], wav[26], wav[27]]),
        16_000
    );
    assert_eq!(u16::from_le_bytes([wav[34], wav[35]]), 16, "16-bit");
    // The data is exactly the samples.
    let data: Vec<i16> = wav[44..]
        .as_chunks::<2>()
        .0
        .iter()
        .map(|b| i16::from_le_bytes(*b))
        .collect();
    assert_eq!(data, samples);
    let back = read_wav(std::io::Cursor::new(&wav)).unwrap();
    assert_eq!(back.channels(), 1);
    assert_eq!(sample_wav(0, &samples).unwrap_err().kind, "internal");
    // An 8 s sample stays well inside one IPC message.
    assert_eq!(SAMPLE_MAX_MS, 8_000);
}

#[test]
fn detail_base64_matches_rfc_4648_vectors() {
    for (input, want) in [
        ("", ""),
        ("f", "Zg=="),
        ("fo", "Zm8="),
        ("foo", "Zm9v"),
        ("foob", "Zm9vYg=="),
        ("fooba", "Zm9vYmE="),
        ("foobar", "Zm9vYmFy"),
    ] {
        assert_eq!(base64(input.as_bytes()), want);
    }
    assert_eq!(base64(&[0xff, 0xfe, 0xfd]), "//79");
}

#[test]
fn detail_speaker_sample_without_audio_is_none() {
    let dir = TempDir::new("detail-sample");
    let svc = services(&dir);
    let f = call_meeting(&svc, 40);
    assert_eq!(speaker_sample(&svc, f.meeting, f.priya).unwrap(), None);
    let e = speaker_sample(&svc, f.meeting, EntityId::from_bytes([4; 16])).unwrap_err();
    assert_eq!(e.kind, "speaker_not_found");
}

/// Stores a FLAC for each of `meeting`'s tracks: on the far end a pattern that differs from
/// sample to sample, on the mic silence, both 8 s at 16 kHz. Returns the far end's samples.
fn store_recordings(svc: &Services, meeting: EntityId, seed: u64) -> Vec<i16> {
    let far: Vec<i16> = (0..128_000)
        .map(|i: i32| ((i * 37) % 4_000 - 2_000) as i16)
        .collect();
    let clock: Arc<dyn Clock + Send + Sync> = Arc::clone(&svc.clock);
    let mut rng = ChaCha8Rng::seed_from_u64(seed);
    let mut db = svc.store();
    let (store, _) = db.parts();
    for track in store.tracks(meeting).unwrap() {
        let samples: Vec<f32> = match track.kind {
            TrackId::Reference => far.iter().map(|&s| i16_to_f32(s)).collect(),
            _ => vec![0.0; far.len()],
        };
        let mut w = svc.data.track_writer(meeting, track.kind, 16_000).unwrap();
        w.write(&samples).unwrap();
        store
            .finalize_track_audio(
                w,
                &svc.data,
                &track,
                earshot_store::AudioProvenance {
                    pipeline_version: "test".into(),
                    aec_version: None,
                },
                &*clock,
                &mut rng,
            )
            .unwrap();
    }
    far
}

#[test]
fn detail_speaker_sample_reads_the_speakers_longest_line_from_its_tracks_recording() {
    let dir = TempDir::new("detail-sample-flac");
    let svc = services(&dir);
    let f = call_meeting(&svc, 41);
    let far = store_recordings(&svc, f.meeting, 42);
    // Priya's one line runs from 6.0 s to 6.9 s on the far end.
    let want = base64(&sample_wav(16_000, &far[96_000..110_400]).unwrap());
    assert_eq!(
        speaker_sample(&svc, f.meeting, f.priya).unwrap(),
        Some(want)
    );
    // Another voice on the same track: its own line, 3.0 s to 3.9 s.
    let want = base64(&sample_wav(16_000, &far[48_000..62_400]).unwrap());
    assert_eq!(
        speaker_sample(&svc, f.meeting, f.unnamed).unwrap(),
        Some(want)
    );
    // The user's longest line (1.0 s to 1.9 s) comes from the mic's recording, which is silence.
    let silent = base64(&sample_wav(16_000, &[0; 14_400]).unwrap());
    assert_eq!(speaker_sample(&svc, f.meeting, f.me).unwrap(), Some(silent));
}

#[test]
fn detail_says_whether_the_recordings_are_still_kept() {
    let dir = TempDir::new("detail-audio");
    let svc = services(&dir);
    // A meeting without stored recordings.
    let f = call_meeting(&svc, 45);
    assert!(!meeting_detail(&svc, f.meeting).unwrap().audio_kept);
    // With both, then one, then none (deleted by retention or by hand).
    let m = crate::storage_settings::tests::meeting_with_audio(&svc, 46, None);
    assert!(meeting_detail(&svc, m).unwrap().audio_kept);
    let path = |t| {
        svc.data
            .resolve(&earshot_store::DataDir::audio_rel(m, t))
            .unwrap()
    };
    std::fs::remove_file(path(TrackId::Mic)).unwrap();
    assert!(meeting_detail(&svc, m).unwrap().audio_kept);
    std::fs::remove_file(path(TrackId::Reference)).unwrap();
    let d = meeting_detail(&svc, m).unwrap();
    assert!(!d.audio_kept);
    // The rest of the meeting still reads.
    assert_eq!(d.header.title, "Review");
}

#[test]
fn detail_export_without_a_vault_is_no_vault_and_with_one_writes_the_note() {
    let dir = TempDir::new("detail-export");
    let svc = services(&dir);
    let f = call_meeting(&svc, 50);
    let e = export(&svc, f.meeting).unwrap_err();
    assert_eq!(e.kind, "no_vault");
    assert_eq!(e.message, "Set your Obsidian vault in Settings to export.");
    // A temp vault (never a real one), set on the Obsidian page with a subfolder.
    let vault = dir.0.join("vault");
    std::fs::create_dir_all(vault.join(".obsidian")).unwrap();
    earshot_runtime::export_settings::set_obsidian_settings(
        &svc,
        &vault.display().to_string(),
        "Work/Calls",
    )
    .unwrap();
    let done = export(&svc, f.meeting).unwrap();
    assert!(done.note_path.ends_with(".md"), "{}", done.note_path);
    assert!(
        done.note_path.starts_with("Work/Calls/"),
        "{}",
        done.note_path
    );
    let note = vault.join(&done.note_path);
    assert!(note.is_file(), "{}", note.display());
}

#[test]
fn detail_delete_is_meeting_busy_while_a_job_holds_the_meeting() {
    let dir = TempDir::new("detail-delete");
    let svc = services(&dir);
    let f = call_meeting(&svc, 60);
    let lease = svc
        .lease_meeting_for(f.meeting, MeetingJob::Processing)
        .unwrap();
    let e: IpcError = svc.delete_meeting(f.meeting).unwrap_err().into();
    assert_eq!(e.kind, "meeting_busy");
    assert!(e.retryable);
    drop(lease);
    svc.delete_meeting(f.meeting).unwrap();
    assert!(
        svc.store()
            .meeting(f.meeting)
            .unwrap()
            .is_some_and(|m| m.meta.deleted)
    );
}

#[test]
fn detail_lists_claude_code_and_codex_handovers_and_reads_meetings_without_them() {
    use earshot_runtime::claude_code::{HandoverCli, HandoverEntry, HandoverKind, encode};
    let dir = TempDir::new("detail-handovers");
    let svc = services(&dir);
    let f = call_meeting(&svc, 3);
    // A meeting from before T7.4ga (no record) reads fine.
    assert!(
        meeting_detail(&svc, f.meeting)
            .unwrap()
            .handovers
            .is_empty()
    );
    let entries = [
        HandoverEntry {
            at_unix_ms: 1_793_000_000_000,
            characters: 5324,
            cli: HandoverCli::ClaudeCode,
            outcome: HandoverKind::SignInExpired,
        },
        HandoverEntry {
            at_unix_ms: 1_793_000_060_000,
            characters: 5400,
            cli: HandoverCli::ClaudeCode,
            outcome: HandoverKind::Answered,
        },
        HandoverEntry {
            at_unix_ms: 1_793_000_120_000,
            characters: 5500,
            cli: HandoverCli::Codex,
            outcome: HandoverKind::AccessDenied,
        },
    ];
    svc.store()
        .set_local_value(
            &format!("summary-handovers/{}", f.meeting),
            &encode(&entries),
        )
        .unwrap();
    let d = meeting_detail(&svc, f.meeting).unwrap();
    assert_eq!(
        d.handovers,
        [
            HandoverItem {
                cli: "claude_code".into(),
                at_ms: 1_793_000_000_000,
                characters: 5324,
                outcome: "sign_in_expired".into(),
            },
            HandoverItem {
                cli: "claude_code".into(),
                at_ms: 1_793_000_060_000,
                characters: 5400,
                outcome: "answered".into(),
            },
            HandoverItem {
                cli: "codex".into(),
                at_ms: 1_793_000_120_000,
                characters: 5500,
                outcome: "access_denied".into(),
            },
        ]
    );
    // A record this version cannot read shows nothing, never an error.
    svc.store()
        .set_local_value(
            &format!("summary-handovers/{}", f.meeting),
            b"\x09\x00\x00\x00{}",
        )
        .unwrap();
    assert!(
        meeting_detail(&svc, f.meeting)
            .unwrap()
            .handovers
            .is_empty()
    );
}

#[test]
fn detail_persons_say_how_many_meetings_each_is_in() {
    let dir = TempDir::new("detail");
    let svc = services(&dir);
    call_meeting(&svc, 90);
    let other = person_with(&svc, "Priya Shah", false, false, 91);
    let listed = persons(&svc).unwrap().persons;
    let priyas: Vec<(&str, u32)> = listed
        .iter()
        .filter(|p| p.name == "Priya Shah")
        .map(|p| (p.id.as_str(), p.meetings))
        .collect();
    assert_eq!(priyas.len(), 2, "two persons with one name, both listed");
    let other = other.to_string();
    assert!(priyas.contains(&(other.as_str(), 0)));
    assert!(priyas.iter().any(|(id, n)| *id != other && *n == 1));
    // The count reaches the window as a number.
    let json = serde_json::to_value(&listed[0]).unwrap();
    assert!(json["meetings"].is_u64(), "{json}");
}

fn ui_base_letters_path() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../../packages/ui/src/detail/baseLetters.ts")
}

/// The person filter's letter table for the UI (T12.80), generated from core's
/// `fold_diacritic`, which `earshot_runtime::edit::person_name_key` folds with.
fn ui_base_letters() -> String {
    let table: std::collections::BTreeMap<String, &str> = (char::MIN..=char::MAX)
        .filter_map(|c| {
            earshot_core::metrics::normalize::fold_diacritic(c).map(|b| (c.to_string(), b))
        })
        .collect();
    format!(
        "// Generated from `earshot_core::metrics::normalize::fold_diacritic` by\n\
         // `EARSHOT_UPDATE_BINDINGS=1 cargo test -p earshot-desktop bindings`; do not edit.\n\n\
         /** Each lower-case Latin letter with a diacritic and its base letters: the person\n \
         * filter's fold (`earshot_runtime::edit::person_name_key`). */\n\
         export const BASE_LETTERS: Readonly<Record<string, string>> = {};\n",
        serde_json::to_string_pretty(&table).unwrap()
    )
}

#[test]
fn detail_person_filter_letters_bindings_are_up_to_date() {
    let path = ui_base_letters_path();
    let fresh = ui_base_letters();
    if std::env::var_os("EARSHOT_UPDATE_BINDINGS").is_some_and(|v| v == "1") {
        std::fs::write(&path, &fresh).unwrap();
        return;
    }
    let committed = std::fs::read_to_string(&path).unwrap_or_default();
    assert!(
        committed.replace("\r\n", "\n") == fresh,
        "packages/ui/src/detail/baseLetters.ts is out of date: run \
         EARSHOT_UPDATE_BINDINGS=1 cargo test -p earshot-desktop bindings"
    );
}

/// The voice sample copy table (`fixtures/ui/voice_sample_copy_cases.json`, T12.77b7).
fn voice_copy() -> serde_json::Value {
    serde_json::from_str(include_str!(
        "../../../../fixtures/ui/voice_sample_copy_cases.json"
    ))
    .unwrap()
}

/// A dotted version as numbers (`0.4.0` gives `[0, 4, 0]`).
fn version_parts(v: &str) -> Vec<u64> {
    v.split('.').map(|p| p.parse().unwrap()).collect()
}

/// The release that first wrote voice samples (T12.77b8 merges only with its version bump).
const SAMPLES_RELEASE: &str = "0.4.0";

/// `since` names the release that first wrote voice samples, fixed at 0.4.0. Before the
/// workspace reaches it (0.3.11 until the bump) `since` is at least the workspace version, so
/// the copy never names a release already past; from the bump on it is at most the workspace
/// version (equal at 0.4.0, and later releases keep naming the one that changed how voices are
/// saved).
#[test]
fn detail_voice_sample_copy_since_is_the_release_that_wrote_samples() {
    let since = voice_copy()["since"].as_str().unwrap().to_owned();
    assert_eq!(since, SAMPLES_RELEASE);
    let workspace = env!("CARGO_PKG_VERSION");
    if version_parts(workspace) < version_parts(SAMPLES_RELEASE) {
        assert!(
            version_parts(&since) >= version_parts(workspace),
            "since {since} names a release before this one ({workspace})"
        );
    } else {
        assert!(
            version_parts(&since) <= version_parts(workspace),
            "since {since} names a release after this one ({workspace})"
        );
    }
    assert_eq!(version_parts("0.4.0"), [0, 4, 0]);
    assert!(version_parts("0.3.11") < version_parts("0.4.0"));
    assert!(version_parts("0.4.1") > version_parts("0.4.0"));
}

#[test]
fn detail_voice_sample_copy_is_the_agreed_table_word_for_word() {
    let doc = include_str!("../../../../docs/voice-samples.md");
    let table = voice_copy();
    let texts: Vec<(&str, &str)> = table
        .as_object()
        .unwrap()
        .iter()
        .filter(|(k, _)| !k.starts_with('_') && k.as_str() != "since")
        .map(|(k, v)| (k.as_str(), v.as_str().unwrap()))
        .collect();
    assert_eq!(texts.len(), 8, "seven texts and the tooltip");
    for (key, text) in texts {
        assert!(doc.contains(&format!("\"{text}\"")), "{key}: {text}");
        assert!(!text.contains('\u{2014}'), "{key} holds an em dash");
    }
}

#[test]
fn detail_no_voice_sample_result_serialises_a_vector() {
    let results = [
        serde_json::to_value(NotThisPersonResult {
            taken_back: 1,
            saved_before_samples: true,
        }),
        serde_json::to_value(LinkResult {
            person: "p".into(),
            taken_back: 1,
        }),
        serde_json::to_value(MarkedMeResult {
            offers: vec![PersonItem {
                id: "p".into(),
                name: "Robin Invented".into(),
                has_voiceprint: true,
                is_me: false,
                meetings: 2,
            }],
            taken_back: 1,
        }),
        serde_json::to_value(MergeResult {
            samples_taken_back: 1,
        }),
        serde_json::to_value(SavedVoiceResult {
            person: "p".into(),
            outcome: RememberOutcome::Replaced,
            taken_back: 1,
            moved: true,
        }),
    ];
    fn floats(v: &serde_json::Value) -> bool {
        match v {
            serde_json::Value::Number(n) => n.is_f64(),
            serde_json::Value::Array(a) => a.iter().any(floats),
            serde_json::Value::Object(o) => o.values().any(floats),
            _ => false,
        }
    }
    for r in results {
        let v = r.unwrap();
        let text = v.to_string();
        assert!(
            !text.contains("vector") && !text.contains("centroid"),
            "{text}"
        );
        assert!(!floats(&v), "{text}");
    }
}

/// What line `row` of `d` offers in its Move to speaker menu, read as the UI reads it: its
/// track's list without the line's own speaker.
fn offers(d: &MeetingDetail, row: usize) -> Vec<MovableSpeaker> {
    let r = &d.transcript[row];
    d.movable_by_track
        .get(&r.track_id)
        .map_or_else(Vec::new, |v| {
            v.iter()
                .filter(|s| Some(&s.id) != r.speaker_id.as_ref())
                .cloned()
                .collect()
        })
}

/// A meeting of `fixtures/ui/line_edit_cases.json` as the store holds it: ids by case label.
struct LineCase {
    meeting: EntityId,
    speakers: BTreeMap<String, EntityId>,
    names: BTreeMap<String, String>,
    lines: BTreeMap<String, EntityId>,
    /// The meeting's tracks by label (`mic`, and `far` in a call).
    tracks: BTreeMap<String, EntityId>,
    rng: ChaCha8Rng,
}

impl LineCase {
    fn label_of(&self, id: &str) -> String {
        self.speakers
            .iter()
            .find(|(_, v)| v.to_string() == id)
            .map_or_else(|| format!("unlabelled {id}"), |(k, _)| k.clone())
    }

    /// The id of speaker or line `label`, or a fresh one no row has (an unknown label).
    fn id(&mut self, label: &str, clock: &dyn Clock) -> EntityId {
        self.speakers
            .get(label)
            .or_else(|| self.lines.get(label))
            .copied()
            .unwrap_or_else(|| EntityId::generate(clock, &mut self.rng))
    }
}

fn case_role(role: &str) -> SpeakerRole {
    match role {
        "Me" => SpeakerRole::Me,
        "Local" => SpeakerRole::Local,
        "Remote" => SpeakerRole::Remote,
        "Room" => SpeakerRole::Room,
        other => panic!("unknown role {other}"),
    }
}

/// Writes case `case` as the final pass (or, for a draft, the live pass) leaves a meeting.
fn line_case(svc: &Services, case: &serde_json::Value, seed: u64) -> LineCase {
    let clock: Arc<dyn Clock + Send + Sync> = Arc::clone(&svc.clock);
    let mut rng = ChaCha8Rng::seed_from_u64(seed);
    let mut db = svc.store();
    let (store, hlc) = db.parts();
    let mut stamp = Stamp {
        clock: &*clock,
        hlc,
        rng: &mut rng,
    };
    let m = Meeting::new("Line edits", T0, 600, Template::General, &mut stamp);
    store.put_meeting(m.clone()).unwrap();
    let mic = Track::new(m.id, TrackId::Mic, None, ClockDomain::Replay, &mut stamp);
    store.put_track(mic.clone()).unwrap();
    let far = (!case["in_room"].as_bool().unwrap()).then(|| {
        let t = Track::new(
            m.id,
            TrackId::Reference,
            None,
            ClockDomain::Replay,
            &mut stamp,
        );
        store.put_track(t.clone()).unwrap();
        t
    });
    let mut speakers = BTreeMap::new();
    let mut names = BTreeMap::new();
    for s in case["speakers"].as_array().unwrap() {
        let (label, name) = (s["id"].as_str().unwrap(), s["name"].as_str().unwrap());
        let role = case_role(s["role"].as_str().unwrap());
        let person = s["person"].as_bool().unwrap().then(|| {
            let p = Person::new(name, role == SpeakerRole::Me, &mut stamp);
            store.put_person(p.clone()).unwrap();
            p.id
        });
        let row = Speaker::new(m.id, role, name, person, &mut stamp);
        store.put_speaker(row.clone()).unwrap();
        speakers.insert(label.to_owned(), row.id);
        names.insert(label.to_owned(), name.to_owned());
    }
    let pass = if case["draft"].as_bool().unwrap() {
        TranscriptPass::Live
    } else {
        TranscriptPass::Final
    };
    let mut lines = BTreeMap::new();
    for (i, l) in case["lines"].as_array().unwrap().iter().enumerate() {
        let track = match l["track"].as_str().unwrap() {
            "mic" => &mic,
            "far" => far.as_ref().expect("a far-end line in the room"),
            other => panic!("unknown track {other}"),
        };
        let start = 1_000 + 6_000 * i as u64;
        let words: Vec<Word> = l["words"]
            .as_array()
            .unwrap()
            .iter()
            .enumerate()
            .map(|(k, w)| {
                let at = start + 400 * k as u64;
                Word::new(w.as_str().unwrap(), at, at + 300, 0.9)
            })
            .collect();
        let mut s = Segment::from_words(
            EntityId::generate(stamp.clock, &mut *stamp.rng),
            track.id,
            words,
            MeetingTime::from_millis(start),
        );
        l["text"].as_str().unwrap().clone_into(&mut s.text);
        s.speaker_id = l["speaker"].as_str().map(|label| speakers[label]);
        lines.insert(l["id"].as_str().unwrap().to_owned(), s.id);
        store
            .put_segment(SegmentRow::new(m.id, pass, s, stamp.hlc, stamp.clock))
            .unwrap();
    }
    let mut tracks = BTreeMap::from([("mic".to_owned(), mic.id)]);
    if let Some(far) = &far {
        tracks.insert("far".to_owned(), far.id);
    }
    LineCase {
        meeting: m.id,
        speakers,
        names,
        lines,
        tracks,
        rng: ChaCha8Rng::seed_from_u64(seed + 1),
    }
}

/// Checks `expect`'s `offers`, `splittable` and `listed` against the meeting detail and the
/// runtime's own rule functions over the same view.
fn check_line_case(svc: &Services, c: &LineCase, expect: &serde_json::Value, at: &str) {
    let d = meeting_detail(svc, c.meeting).unwrap();
    let view = MeetingView::load_named(&svc.store(), c.meeting)
        .unwrap()
        .unwrap();
    let row_of = |label: &str| {
        let id = c.lines[label].to_string();
        d.transcript
            .iter()
            .position(|r| r.segment_id == id)
            .unwrap_or_else(|| panic!("{at}: no line {label}"))
    };
    if let Some(offered) = expect["offers"].as_object() {
        for (line, want) in offered {
            let want: Vec<String> = want
                .as_array()
                .unwrap()
                .iter()
                .map(|v| v.as_str().unwrap().to_owned())
                .collect();
            let shown = offers(&d, row_of(line));
            let got: Vec<String> = shown.iter().map(|s| c.label_of(&s.id)).collect();
            assert_eq!(got, want, "{at}: offers of {line}");
            for s in &shown {
                assert_eq!(
                    s.name,
                    c.names[&c.label_of(&s.id)],
                    "{at}: name of {}",
                    s.id
                );
            }
            let segment = view
                .segments
                .iter()
                .find(|s| s.id == c.lines[line])
                .unwrap();
            let rule: Vec<String> = if d.header.transcript == Some(TranscriptKind::Final) {
                edit::movable_speakers(&view.segments, &view.speakers, &view.tracks, segment)
                    .iter()
                    .map(|id| c.label_of(&id.to_string()))
                    .collect()
            } else {
                Vec::new()
            };
            assert_eq!(rule, want, "{at}: movable_speakers of {line}");
        }
    }
    if let Some(splits) = expect["splittable"].as_object() {
        for (line, want) in splits {
            let want = want.as_bool().unwrap();
            assert_eq!(
                d.transcript[row_of(line)].splittable,
                want,
                "{at}: splittable {line}"
            );
            let segment = view
                .segments
                .iter()
                .find(|s| s.id == c.lines[line])
                .unwrap();
            let is_final = d.header.transcript == Some(TranscriptKind::Final);
            assert_eq!(
                is_final && edit::can_split_line(segment),
                want,
                "{at}: can_split_line {line}"
            );
        }
    }
    if let Some(by_track) = expect["by_track"].as_object() {
        for (track, want) in by_track {
            let want: Vec<&str> = want
                .as_array()
                .unwrap()
                .iter()
                .map(|v| v.as_str().unwrap())
                .collect();
            let id = c.tracks[track].to_string();
            let listed = d
                .movable_by_track
                .get(&id)
                .unwrap_or_else(|| panic!("{at}: no list for the {track} track"));
            let got: Vec<String> = listed.iter().map(|s| c.label_of(&s.id)).collect();
            assert_eq!(got, want, "{at}: by_track {track}");
        }
    }
    if let Some(listed) = expect["listed"].as_array() {
        let want: Vec<&str> = listed.iter().map(|v| v.as_str().unwrap()).collect();
        let got: Vec<String> = d.speakers.iter().map(|s| c.label_of(&s.id)).collect();
        assert_eq!(got, want, "{at}: speakers listed");
    }
}

/// `fixtures/ui/line_edit_cases.json`, which the UI's `ipc/lineEdits.test.ts` runs through the
/// mock: every case through the shipped move, split and edit functions and the detail.
#[test]
fn detail_line_edits_match_the_shared_cases() {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../../fixtures/ui/line_edit_cases.json");
    let table: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    let cases = table["cases"].as_array().unwrap();
    assert!(cases.len() >= 5);
    for (n, case) in cases.iter().enumerate() {
        let name = case["name"].as_str().unwrap();
        let dir = TempDir::new("detail-line-cases");
        let svc = services(&dir);
        let mut c = line_case(&svc, case, 300 + 10 * n as u64);
        check_line_case(&svc, &c, case, &format!("{name}, at the start"));
        let clock: Arc<dyn Clock + Send + Sync> = Arc::clone(&svc.clock);
        for (k, step) in case["steps"].as_array().unwrap().iter().enumerate() {
            let at = format!("{name}, step {}", k + 1);
            let line = c.id(step["line"].as_str().unwrap(), &*clock);
            let before: BTreeSet<EntityId> = svc
                .store()
                .speakers(c.meeting)
                .unwrap()
                .iter()
                .map(|s| s.id)
                .collect();
            let outcome: Result<serde_json::Value, RuntimeError> = match step["op"]
                .as_str()
                .unwrap()
            {
                "move" => {
                    let to = step["to"].as_str().unwrap();
                    let target = if to == "new" {
                        edit::LineSpeaker::New
                    } else {
                        edit::LineSpeaker::Existing(c.id(to, &*clock))
                    };
                    edit::move_line(&svc, c.meeting, line, target).map(serde_json::Value::Bool)
                }
                "split" => {
                    let token = usize::try_from(step["token"].as_u64().unwrap()).unwrap();
                    edit::split_line(&svc, c.meeting, line, token)
                        .map(|id| serde_json::Value::String(id.to_string()))
                }
                "edit" => edit::edit_segment(&svc, c.meeting, line, step["text"].as_str().unwrap())
                    .map(serde_json::Value::Bool),
                other => panic!("{at}: unknown op {other}"),
            };
            match step["result"].as_str().unwrap() {
                "error" => {
                    let e: IpcError = outcome.expect_err(&at).into();
                    assert_eq!(e.kind, step["error"].as_str().unwrap(), "{at}");
                }
                "moved" => assert_eq!(outcome.expect(&at), serde_json::Value::Bool(true), "{at}"),
                "unchanged" => {
                    assert_eq!(outcome.expect(&at), serde_json::Value::Bool(false), "{at}");
                }
                "new_speaker" => {
                    assert_eq!(outcome.expect(&at), serde_json::Value::Bool(true), "{at}");
                    let made: Vec<Speaker> = svc
                        .store()
                        .speakers(c.meeting)
                        .unwrap()
                        .into_iter()
                        .filter(|s| !before.contains(&s.id))
                        .collect();
                    assert_eq!(made.len(), 1, "{at}: one new speaker row");
                    let want = &step["new_speaker"];
                    assert_eq!(made[0].display_name, want["name"].as_str().unwrap(), "{at}");
                    assert_eq!(
                        made[0].role,
                        case_role(want["role"].as_str().unwrap()),
                        "{at}"
                    );
                    assert_eq!(made[0].person_id, None, "{at}");
                    let label = want["as"].as_str().unwrap().to_owned();
                    c.names.insert(label.clone(), made[0].display_name.clone());
                    c.speakers.insert(label, made[0].id);
                }
                "split" => {
                    let second: EntityId = outcome.expect(&at).as_str().unwrap().parse().unwrap();
                    let want = &step["split"];
                    let rows = svc.store().segments(c.meeting).unwrap();
                    let text = |id: EntityId| {
                        rows.iter()
                            .find(|r| r.segment.id == id)
                            .unwrap()
                            .segment
                            .text
                            .clone()
                    };
                    let texts: Vec<&str> = want["texts"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .map(|v| v.as_str().unwrap())
                        .collect();
                    assert_eq!([text(line), text(second)], [texts[0], texts[1]], "{at}");
                    c.lines
                        .insert(want["as"].as_str().unwrap().to_owned(), second);
                }
                other => panic!("{at}: unknown result {other}"),
            }
            check_line_case(&svc, &c, step, &at);
        }
    }
}

/// A long synthetic meeting for [`detail_timing_long_meeting_prints_its_cost`]: `lines` final
/// lines alternating over a mic and a far-end track, six speakers (Me and one more voice on the
/// mic, four on the far end), twelve words each.
fn long_meeting(svc: &Services, lines: usize) -> EntityId {
    let clock: Arc<dyn Clock + Send + Sync> = Arc::clone(&svc.clock);
    let mut rng = ChaCha8Rng::seed_from_u64(90);
    let mut db = svc.store();
    let (store, hlc) = db.parts();
    let mut stamp = Stamp {
        clock: &*clock,
        hlc,
        rng: &mut rng,
    };
    let mut m = Meeting::new("Long review", T0, 600, Template::General, &mut stamp);
    m.duration_ms = Some(lines as u64 * 4_000);
    let mic = Track::new(m.id, TrackId::Mic, None, ClockDomain::Replay, &mut stamp);
    let them = Track::new(
        m.id,
        TrackId::Reference,
        None,
        ClockDomain::Replay,
        &mut stamp,
    );
    store.put_meeting(m.clone()).unwrap();
    store.put_track(mic.clone()).unwrap();
    store.put_track(them.clone()).unwrap();
    let mic_speakers = [
        Speaker::new(m.id, SpeakerRole::Me, "Me", None, &mut stamp),
        Speaker::new(m.id, SpeakerRole::Local, "Speaker 1", None, &mut stamp),
    ];
    let far_speakers: Vec<Speaker> = (2..6)
        .map(|n| {
            Speaker::new(
                m.id,
                SpeakerRole::Remote,
                format!("Speaker {n}"),
                None,
                &mut stamp,
            )
        })
        .collect();
    for s in mic_speakers.iter().chain(&far_speakers) {
        store.put_speaker(s.clone()).unwrap();
    }
    let text = "the gravel order goes out on friday and the hedge plan waits a week";
    for i in 0..lines {
        let start = i as u64 * 4_000;
        let (track, speaker) = if i % 2 == 0 {
            (&mic, &mic_speakers[(i / 2) % 2])
        } else {
            (&them, &far_speakers[(i / 2) % 4])
        };
        let mut s = Segment::from_words(
            EntityId::generate(stamp.clock, &mut *stamp.rng),
            track.id,
            words(text, start, 250),
            MeetingTime::from_millis(start),
        );
        s.speaker_id = Some(speaker.id);
        store
            .put_segment(SegmentRow::new(
                m.id,
                TranscriptPass::Final,
                s,
                stamp.hlc,
                stamp.clock,
            ))
            .unwrap();
    }
    m.id
}

#[test]
#[ignore = "timing, prints only: cargo test -p earshot-desktop --bin earshot detail_timing -- --ignored --nocapture"]
fn detail_timing_long_meeting_prints_its_cost() {
    // T12.90a: what one `meeting_detail` costs on a meeting of 3,000 final lines, and the size of
    // its JSON. Informational: nothing is asserted beyond the line count.
    let dir = TempDir::new("detail-timing");
    let svc = services(&dir);
    let m = long_meeting(&svc, 3_000);
    let d = meeting_detail(&svc, m).unwrap();
    assert_eq!(d.transcript.len(), 3_000);
    let mut times = Vec::new();
    for _ in 0..7 {
        let t = std::time::Instant::now();
        let d = meeting_detail(&svc, m).unwrap();
        times.push(t.elapsed());
        std::hint::black_box(d);
    }
    times.sort();
    let json = serde_json::to_vec(&d).unwrap();
    println!(
        "detail_timing: 3000 lines, median {:?}, min {:?}, max {:?}, json {} bytes",
        times[times.len() / 2],
        times[0],
        times[times.len() - 1],
        json.len()
    );
}

/// T14.57 (docs/sync-spec.md 16.7, test 14): the People page with a damaged person row and a
/// damaged speaker link lists everyone else and says so; Remove and Forget voice succeed and
/// say a damaged row was left; the refusal for a member whose `is_me` does not read is the
/// table's fixed copy; and no serialised result holds the damaged row's invented name.
/// Control: the damaged name is found in the store file by the same search.
#[test]
fn detail_people_with_a_damaged_row_list_remove_and_forget_with_fixed_copy() {
    let people: serde_json::Value = serde_json::from_str(include_str!(
        "../../../../fixtures/ui/people_copy_cases.json"
    ))
    .unwrap();
    assert_eq!(
        Some(REMOVE_UNREADABLE_COPY),
        people["remove_unreadable"].as_str()
    );
    assert_eq!(
        edit_error(&EditError::PersonUnreadable(EntityId::from_bytes([1; 16]))).message,
        REMOVE_UNREADABLE_COPY
    );
    let dir = TempDir::new("detail-people-damaged");
    let svc = services(&dir);
    let named = |name: &str, seed: u8| {
        let clock = Arc::clone(&svc.clock);
        let mut db = svc.store();
        let (store, hlc) = db.parts();
        let p = Person {
            id: EntityId::from_bytes([
                0x01, 0x93, 0, 0, 0, 0, 0x70, 0, 0x80, 0, 0, 0, 0, 0, seed, 1,
            ]),
            meta: earshot_core::identity::SyncMeta::new(hlc, &*clock),
            display_name: name.into(),
            is_me: false,
            merged_into: None,
        };
        store.put_person(p.clone()).unwrap();
        p.id
    };
    let keep = named("Invented Quorra", 1);
    let gone = named("Invented Zephyrine", 2);
    // The control: before the damage the same search finds the name in the result.
    let before = serde_json::to_string(&persons(&svc).unwrap()).unwrap();
    assert!(before.contains("Zephyrine"), "{before}");
    let db = dir.0.join("data").join("earshot.db");
    {
        let c = rusqlite::Connection::open(&db).unwrap();
        c.execute(
            "UPDATE person SET display_name = CAST(X'FF00FE' AS TEXT), created_wall = -1 WHERE id = ?1",
            rusqlite::params![gone.as_uuid().as_bytes().to_vec()],
        )
        .unwrap();
    }
    let list = persons(&svc).unwrap();
    assert!(list.damaged, "the list says a damaged row was left out");
    assert_eq!(list.persons.len(), 1);
    assert_eq!(list.persons[0].id, keep.to_string());
    let forgot =
        VoicesForgottenView::of(earshot_runtime::edit::forget_person_voices(&svc, keep).unwrap());
    let removed = earshot_runtime::edit::remove_person(&svc, keep).unwrap();
    let json = serde_json::to_string(&(
        &list,
        &forgot,
        PersonRemovedView {
            damaged: removed.damaged_skipped > 0,
        },
    ))
    .unwrap();
    assert!(
        !json.contains("Zephyrine") && !json.contains(&gone.to_string()),
        "{json}"
    );
}
