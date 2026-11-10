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
        Some("qwen3.5-9b".into()),
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
    assert_eq!(d.header.summary_model.as_deref(), Some("qwen3.5-9b"));
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
    let e = remember(&svc, m, &[voice]).unwrap_err();
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
        (EditError::NoPreviousSummary, "no_previous_summary"),
        (EditError::PreviousSummaryStale, "previous_summary_stale"),
        (EditError::FolderNotFound(id), "folder_not_found"),
        (EditError::FolderNameTaken(id), "folder_name_taken"),
        (EditError::MeetingRecording(id), "meeting_recording"),
        (EditError::InvalidTitle(TextProblem::Empty), "invalid_title"),
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
    .unwrap();
    let d = meeting_detail(&svc, f.meeting).unwrap();
    let c = card(&d, f.unnamed);
    assert_eq!(c.name, "Sam Lee");
    assert!(!c.needs_name);
    assert_eq!(c.person.as_ref().unwrap().id, id);
    assert!(
        persons(&svc)
            .unwrap()
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
    // Remembering an unlinked voice is refused before anything is stored.
    let e = remember(&svc, g.meeting, &[g.me]).unwrap_err();
    assert_eq!(e.kind, "me_voice");
    // The bad id from the UI.
    assert_eq!(parse_id("not an id").unwrap_err().kind, "invalid_id");
    assert_eq!(
        parse_ids(&[g.me.to_string(), g.me.to_string()]).unwrap(),
        [g.me]
    );
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
