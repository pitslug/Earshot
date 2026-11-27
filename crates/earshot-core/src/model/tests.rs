//! Data model tests (`cargo test -p earshot-core model`).

use rand_chacha::ChaCha8Rng;
use rand_core::SeedableRng;

use super::*;
use crate::asr::{Segment, Word};
use crate::capture::TrackId;
use crate::diar::SpeakerRole;
use crate::identity::{
    ClassMarker, DeviceId, EntityId, Hlc, ManualClock, SyncClass, SyncClassKind, sync_envelope,
};
use crate::prompt::Template;
use crate::summary::SummaryV1;
use crate::voiceprint::Stamp;

struct World {
    clock: ManualClock,
    hlc: Hlc,
    rng: ChaCha8Rng,
}

impl World {
    fn new() -> Self {
        let clock = ManualClock::new(1_800_000_000_000);
        let mut rng = ChaCha8Rng::seed_from_u64(7);
        let hlc = Hlc::new(DeviceId::generate(&clock, &mut rng));
        World { clock, hlc, rng }
    }

    fn stamp(&mut self) -> Stamp<'_, ManualClock, ChaCha8Rng> {
        self.clock.advance_ms(1);
        Stamp {
            clock: &self.clock,
            hlc: &mut self.hlc,
            rng: &mut self.rng,
        }
    }
}

fn kind<T: SyncClass>() -> SyncClassKind {
    <T::Class as ClassMarker>::KIND
}

fn empty_summary() -> SummaryV1 {
    SummaryV1 {
        title: "t".into(),
        tldr: "x".into(),
        enhanced_notes: vec![],
        decisions: vec![],
        action_items: vec![],
        open_questions: vec![],
        topics: vec![],
    }
}

#[test]
fn model_entities_have_the_sync_classes_of_plan_3_5() {
    assert_eq!(kind::<Meeting>(), SyncClassKind::Syncable);
    assert_eq!(kind::<Track>(), SyncClassKind::Syncable);
    assert_eq!(kind::<SegmentRow>(), SyncClassKind::Syncable);
    assert_eq!(kind::<Speaker>(), SyncClassKind::Syncable);
    assert_eq!(kind::<Note>(), SyncClassKind::Syncable);
    assert_eq!(kind::<Summary>(), SyncClassKind::Syncable);
    assert_eq!(kind::<Folder>(), SyncClassKind::Syncable);
    assert_eq!(kind::<SummaryRequest>(), SyncClassKind::Syncable);
    assert_eq!(kind::<Person>(), SyncClassKind::Syncable);
    assert_eq!(kind::<Voiceprint>(), SyncClassKind::SyncableOptIn);
    assert_eq!(kind::<AudioFile>(), SyncClassKind::NeverSync);
    assert_eq!(kind::<ExportRecord>(), SyncClassKind::NeverSync);
}

#[test]
fn model_syncable_rows_go_through_the_sync_envelope_with_their_entity_names() {
    let mut w = World::new();
    let m = Meeting::new(
        "Weekly",
        1_800_000_000_000,
        60,
        Template::General,
        &mut w.stamp(),
    );
    let t = Track::new(
        m.id,
        TrackId::Mic,
        Some("Mic".into()),
        ClockDomain::Qpc,
        &mut w.stamp(),
    );
    let sp = Speaker::new(m.id, SpeakerRole::Me, "Me", None, &mut w.stamp());
    let n = Note::new(
        m.id,
        MeetingTime::from_millis(1500),
        "hello",
        &mut w.stamp(),
    );
    let s = Summary::new(
        m.id,
        Template::General,
        None,
        empty_summary(),
        &mut w.stamp(),
    );
    let seg = Segment::from_words(
        EntityId::generate(&w.clock, &mut w.rng),
        t.id,
        vec![Word::new("hi", 0, 300, 0.9)],
        MeetingTime::ZERO,
    );
    let row = SegmentRow::new(
        m.id,
        TranscriptPass::Live,
        seg.clone(),
        &mut w.hlc,
        &w.clock,
    );

    assert_eq!(sync_envelope(&m).entity, "meeting");
    assert_eq!(sync_envelope(&t).entity, "track");
    assert_eq!(sync_envelope(&sp).entity, "speaker");
    assert_eq!(sync_envelope(&n).entity, "note");
    assert_eq!(sync_envelope(&s).entity, "summary");
    let folder = Folder::new("Projects", &mut w.stamp());
    assert_eq!(sync_envelope(&folder).entity, "folder");
    let request = SummaryRequest::new(
        m.id,
        DeviceId::from_bytes([2; 16]),
        DeviceId::from_bytes([1; 16]),
        12,
        &mut w.stamp(),
    );
    assert_eq!(sync_envelope(&request).entity, "summary_request");
    assert_eq!(request.state, RequestState::Requested);
    assert!(request.reason.is_empty());
    let env = sync_envelope(&row);
    assert_eq!(env.entity, "segment");
    assert_eq!(env.id, seg.id, "a segment row keeps the ASR segment's id");
    assert_eq!(env.class, SyncClassKind::Syncable);
}

#[test]
fn model_note_anchor_is_meeting_relative_milliseconds() {
    let mut w = World::new();
    let m = Meeting::new("", 1_800_000_000_000, 0, Template::General, &mut w.stamp());
    let n = Note::new(m.id, MeetingTime::from_millis(61_250), "x", &mut w.stamp());
    let json = serde_json::to_value(&n).unwrap();
    assert_eq!(
        json["at"],
        serde_json::json!(61_250),
        "anchor is ms from meeting start"
    );
    assert_eq!(n.meeting_id, m.id);
}

#[test]
fn model_meeting_time_is_reexported_from_its_single_definition() {
    // MeetingTime lives in `asr` (where ASR produces it); `model` re-exports the same type.
    let a: crate::asr::MeetingTime = MeetingTime::from_millis(5);
    assert_eq!(a.as_millis(), 5);
}

#[test]
fn model_vault_path_accepts_relative_forward_slash_paths() {
    let p = VaultPath::new("Meetings/2026-10-17 14-00 Weekly.md").unwrap();
    assert_eq!(p.as_str(), "Meetings/2026-10-17 14-00 Weekly.md");
    // Backslashes are normalised to forward slashes.
    let q = VaultPath::new("Meetings\\Sub\\a.md").unwrap();
    assert_eq!(q.as_str(), "Meetings/Sub/a.md");
    assert_eq!(q.file_name(), "a.md");
}

#[test]
fn model_vault_path_rejects_escapes_and_absolute_paths() {
    for bad in [
        "",
        "/abs.md",
        "\\abs.md",
        "C:/x.md",
        "c:x.md",
        "../x.md",
        "a/../../x.md",
        "a/./x.md",
        "a//x.md",
        "a/",
        "//server/share/x.md",
        "a/b\u{0}.md",
    ] {
        assert!(VaultPath::new(bad).is_err(), "{bad:?} must be rejected");
    }
}

#[test]
fn model_audio_file_and_export_record_carry_their_provenance() {
    let mut w = World::new();
    let m = Meeting::new("", 1_800_000_000_000, 0, Template::General, &mut w.stamp());
    let t = Track::new(m.id, TrackId::Mic, None, ClockDomain::Qpc, &mut w.stamp());
    let f = AudioFile {
        id: EntityId::generate(&w.clock, &mut w.rng),
        meeting_id: m.id,
        track_id: t.id,
        path: "audio/x/mic.flac".into(),
        format: AudioFormat::Flac,
        sample_rate_hz: 16_000,
        frames: 16_000,
        size_bytes: 1234,
        pipeline_version: "earshot-0.1.0".into(),
        aec_version: Some("aec-1".into()),
        created_unix_ms: 1,
    };
    assert_eq!(f.duration_ms(), 1000);
    let r = ExportRecord {
        id: EntityId::generate(&w.clock, &mut w.rng),
        meeting_id: m.id,
        device_id: w.hlc.node(),
        path: VaultPath::new("M/a.md").unwrap(),
        exported_unix_ms: 2,
    };
    assert_eq!(r.device_id, w.hlc.node());
}

// ---- repository contract on the in-memory repo ----

#[test]
fn model_in_memory_repo_meeting_contract() {
    repo_contract::meeting_repo(InMemoryRepo::new);
}

#[test]
fn model_in_memory_repo_audio_file_contract() {
    repo_contract::audio_file_repo(InMemoryRepo::new);
}

#[test]
fn model_in_memory_repo_meeting_deletion_contract() {
    repo_contract::meeting_deletion(InMemoryRepo::new);
}

#[test]
fn model_in_memory_repo_folder_contract() {
    repo_contract::folder_repo(InMemoryRepo::new);
}

#[test]
fn model_in_memory_repo_summary_request_contract() {
    repo_contract::summary_request_repo(InMemoryRepo::new);
}

#[test]
fn model_in_memory_repo_dictionary_contract() {
    repo_contract::dictionary_repo(InMemoryRepo::new);
}

#[test]
fn model_dictionary_word_is_opt_in_and_goes_through_the_sync_envelope() {
    let mut w = World::new();
    assert_eq!(kind::<DictionaryWord>(), SyncClassKind::SyncableOptIn);
    let word = DictionaryWord::new("Quillmarsh", &mut w.stamp());
    let env = sync_envelope(&word);
    assert_eq!(env.entity, "dictionary_word");
    assert_eq!(env.class, SyncClassKind::SyncableOptIn);
    assert!(word.is_live());
}

#[test]
fn model_summary_request_states_have_stable_names() {
    for s in RequestState::ALL {
        assert_eq!(RequestState::from_name(s.name()), Some(s));
        assert_eq!(
            serde_json::to_value(s).unwrap(),
            serde_json::Value::String(s.name().into())
        );
    }
    assert_eq!(RequestState::from_name("lost"), None);
    let open: Vec<RequestState> = RequestState::ALL
        .into_iter()
        .filter(|s| s.is_open())
        .collect();
    assert_eq!(open, [RequestState::Requested, RequestState::Claimed]);
}

#[test]
fn model_meeting_json_without_a_folder_reads_as_no_folder() {
    let mut w = World::new();
    let mut m = Meeting::new("Weekly", 1, 60, Template::General, &mut w.stamp());
    let mut json = serde_json::to_value(&m).unwrap();
    assert!(
        json.as_object_mut().unwrap().remove("folder_id").is_some(),
        "the field is written"
    );
    let read: Meeting = serde_json::from_value(json).unwrap();
    assert_eq!(read, m, "older JSON (no folder_id) reads as no folder");
    m.folder_id = Some(EntityId::generate(&w.clock, &mut w.rng));
    let back: Meeting = serde_json::from_str(&serde_json::to_string(&m).unwrap()).unwrap();
    assert_eq!(back, m, "a folder round-trips");
}

#[test]
fn model_new_folder_and_meeting_start_unfiled() {
    let mut w = World::new();
    let m = Meeting::new("Weekly", 1, 60, Template::General, &mut w.stamp());
    assert_eq!(m.folder_id, None);
    let f = Folder::new("Clients", &mut w.stamp());
    assert_eq!(f.name, "Clients");
    assert!(!f.meta.deleted);
}

#[test]
fn model_in_memory_repo_export_contract() {
    repo_contract::export_repo(InMemoryRepo::new);
}

#[test]
fn model_replace_segments_tombstones_the_previous_pass_only() {
    let mut w = World::new();
    let mut repo = InMemoryRepo::new();
    let m = Meeting::new("", 1_800_000_000_000, 0, Template::General, &mut w.stamp());
    repo.put_meeting(m.clone()).unwrap();
    let t = Track::new(m.id, TrackId::Mic, None, ClockDomain::Qpc, &mut w.stamp());
    repo.put_track(t.clone()).unwrap();
    let mk = |w: &mut World, start: u64| {
        Segment::from_words(
            EntityId::generate(&w.clock, &mut w.rng),
            t.id,
            vec![Word::new("w", start, start + 100, 1.0)],
            MeetingTime::ZERO,
        )
    };
    let live = vec![mk(&mut w, 0), mk(&mut w, 200)];
    for s in &live {
        let row = SegmentRow::new(m.id, TranscriptPass::Live, s.clone(), &mut w.hlc, &w.clock);
        repo.put_segment(row).unwrap();
    }
    let finals = vec![mk(&mut w, 0)];
    let n = replace_segments(
        &mut repo,
        m.id,
        TranscriptPass::Final,
        finals.clone(),
        &mut w.hlc,
        &w.clock,
    )
    .unwrap();
    assert_eq!(n, 0, "no earlier final pass to tombstone");
    // Live segments are untouched by a final pass.
    assert_eq!(
        live_segments(&repo, m.id, TranscriptPass::Live)
            .unwrap()
            .len(),
        2
    );
    // A re-transcribe replaces the final pass: old rows become tombstones.
    let again = vec![mk(&mut w, 10), mk(&mut w, 300)];
    let n = replace_segments(
        &mut repo,
        m.id,
        TranscriptPass::Final,
        again.clone(),
        &mut w.hlc,
        &w.clock,
    )
    .unwrap();
    assert_eq!(n, 1);
    let current: Vec<EntityId> = live_segments(&repo, m.id, TranscriptPass::Final)
        .unwrap()
        .iter()
        .map(|r| r.segment.id)
        .collect();
    assert_eq!(current, again.iter().map(|s| s.id).collect::<Vec<_>>());
    let old = repo
        .segments(m.id)
        .unwrap()
        .into_iter()
        .find(|r| r.segment.id == finals[0].id)
        .unwrap();
    assert!(
        old.meta.deleted,
        "replaced rows stay as tombstones so the deletion syncs"
    );
    assert!(old.meta.updated_at > old.meta.created_at);
}

#[test]
fn model_replace_segments_rejects_a_segment_of_another_meetings_track() {
    let mut w = World::new();
    let mut repo = InMemoryRepo::new();
    let a = Meeting::new("", 1, 0, Template::General, &mut w.stamp());
    let b = Meeting::new("", 2, 0, Template::General, &mut w.stamp());
    repo.put_meeting(a.clone()).unwrap();
    repo.put_meeting(b.clone()).unwrap();
    let tb = Track::new(b.id, TrackId::Mic, None, ClockDomain::Qpc, &mut w.stamp());
    repo.put_track(tb.clone()).unwrap();
    let seg = Segment::from_words(
        EntityId::generate(&w.clock, &mut w.rng),
        tb.id,
        vec![],
        MeetingTime::ZERO,
    );
    let err = replace_segments(
        &mut repo,
        a.id,
        TranscriptPass::Final,
        vec![seg],
        &mut w.hlc,
        &w.clock,
    )
    .unwrap_err();
    assert!(matches!(err, RepoError::MissingParent { .. }), "{err:?}");
}

/// T12.92: two copies of one final line are ordered by the user's edit first (absent lowest),
/// then by `updated_at`; the order is total and antisymmetric, and copies that are not both
/// final have none (plain `updated_at` order applies).
#[test]
fn model_segment_edit_order_puts_the_edit_first_then_the_stamp() {
    use crate::identity::{HlcTimestamp, SyncMeta};
    use core::cmp::Ordering::{Equal, Greater, Less};
    let node = |b: u8| DeviceId::from_bytes([b; 16]);
    let ts = |wall_ms: u64, logical: u32, n: u8| HlcTimestamp {
        wall_ms,
        logical,
        node: node(n),
    };
    let row = |updated: HlcTimestamp, edited: Option<HlcTimestamp>| {
        let seg = Segment::from_words(
            EntityId::from_bytes([1; 16]),
            EntityId::from_bytes([2; 16]),
            vec![Word::new("invented", 0, 300, 0.9)],
            crate::asr::MeetingTime::ZERO,
        );
        let mut r = SegmentRow {
            meta: SyncMeta {
                created_at: ts(1, 0, 1),
                updated_at: updated,
                deleted: false,
                origin_device_id: node(1),
            },
            meeting_id: EntityId::from_bytes([3; 16]),
            pass: TranscriptPass::Final,
            segment: seg,
            text_edited_at: edited,
            pass_run: None,
        };
        r.meta.deleted = edited.is_none();
        r
    };
    let edit = row(ts(100, 0, 1), Some(ts(100, 0, 1)));
    let later_tomb = row(ts(200, 0, 2), None);
    // The edit wins over a later tombstone, from either side.
    assert_eq!(SegmentRow::edit_order(&later_tomb, &edit), Some(Greater));
    assert_eq!(SegmentRow::edit_order(&edit, &later_tomb), Some(Less));
    // Of two edits the later wins; ties on the edit go to `updated_at`, then the node.
    let later_edit = row(ts(150, 0, 3), Some(ts(150, 0, 3)));
    assert_eq!(SegmentRow::edit_order(&edit, &later_edit), Some(Greater));
    let same_edit_later_write = row(ts(300, 0, 4), Some(ts(100, 0, 1)));
    assert_eq!(
        SegmentRow::edit_order(&edit, &same_edit_later_write),
        Some(Greater)
    );
    let tie_a = row(ts(300, 0, 4), Some(ts(100, 0, 1)));
    let tie_b = row(ts(300, 0, 5), Some(ts(100, 0, 1)));
    assert_eq!(SegmentRow::edit_order(&tie_a, &tie_b), Some(Greater));
    assert_eq!(SegmentRow::edit_order(&tie_b, &tie_a), Some(Less));
    assert_eq!(SegmentRow::edit_order(&tie_a, &tie_a.clone()), Some(Equal));
    // Without edits it is `updated_at` order.
    let older_tomb = row(ts(50, 0, 2), None);
    assert_eq!(
        SegmentRow::edit_order(&older_tomb, &later_tomb),
        Some(Greater)
    );
    // Antisymmetric over every pair.
    let all = [
        &edit,
        &later_tomb,
        &later_edit,
        &same_edit_later_write,
        &tie_a,
        &tie_b,
        &older_tomb,
    ];
    for a in all {
        for b in all {
            assert_eq!(
                SegmentRow::edit_order(a, b).map(core::cmp::Ordering::reverse),
                SegmentRow::edit_order(b, a)
            );
        }
    }
    // A meeting deletion's erasure (no text, words, confidence or speaker) has no edit order
    // with any copy, so plain stamp order decides; a pass tombstone keeps its text and has one.
    let mut erasure = later_tomb.clone();
    erasure.segment.text.clear();
    erasure.segment.words.clear();
    erasure.segment.confidence = 0.0;
    erasure.segment.speaker_id = None;
    assert!(erasure.is_erasure() && !later_tomb.is_erasure() && !edit.is_erasure());
    assert_eq!(SegmentRow::edit_order(&edit, &erasure), None);
    assert_eq!(SegmentRow::edit_order(&erasure, &edit), None);
    // Not both final: no edit order.
    let mut live = edit.clone();
    live.pass = TranscriptPass::Live;
    assert_eq!(SegmentRow::edit_order(&live, &later_tomb), None);
    assert_eq!(SegmentRow::edit_order(&later_tomb, &live), None);
}

/// A final line with invented words, for the T14.47 tests.
fn t1447_row(
    updated: crate::identity::HlcTimestamp,
    edited: Option<crate::identity::HlcTimestamp>,
    deleted: bool,
) -> SegmentRow {
    use crate::identity::SyncMeta;
    let seg = Segment::from_words(
        EntityId::from_bytes([1; 16]),
        EntityId::from_bytes([2; 16]),
        vec![Word::new("invented", 0, 300, 0.9)],
        crate::asr::MeetingTime::ZERO,
    );
    SegmentRow {
        meta: SyncMeta {
            created_at: updated,
            updated_at: updated,
            deleted,
            origin_device_id: updated.node,
        },
        meeting_id: EntityId::from_bytes([3; 16]),
        pass: TranscriptPass::Final,
        segment: seg,
        text_edited_at: edited,
        pass_run: None,
    }
}

fn t1447_ts(wall_ms: u64, n: u8) -> crate::identity::HlcTimestamp {
    crate::identity::HlcTimestamp {
        wall_ms,
        logical: 0,
        node: DeviceId::from_bytes([n; 16]),
    }
}

/// T14.47 (docs/sync-spec.md 15.1): a row holds text when its text or its words are not empty;
/// a cleared row and an erasure hold none.
#[test]
fn model_segment_holds_text_when_its_text_or_its_words_are_not_empty() {
    let row = t1447_row(t1447_ts(1, 1), None, false);
    assert!(row.holds_text());
    let mut words_only = row.clone();
    words_only.segment.text.clear();
    assert!(words_only.holds_text());
    let mut text_only = row.clone();
    text_only.segment.words.clear();
    assert!(text_only.holds_text());
    let cleared = row.cleared();
    assert!(!cleared.holds_text() && cleared.is_cleared());
    let mut erasure = cleared.clone();
    erasure.segment.confidence = 0.0;
    assert!(!erasure.holds_text() && erasure.is_erasure() && !erasure.is_cleared());
    // The cleared form keeps what rule 4 compares and the row's identity.
    assert_eq!(cleared.segment.id, row.segment.id);
    assert_eq!(cleared.segment.track, row.segment.track);
    assert_eq!(
        (cleared.segment.start, cleared.segment.end),
        (row.segment.start, row.segment.end)
    );
    assert_eq!(
        (cleared.pass, cleared.text_edited_at, cleared.pass_run),
        (row.pass, row.text_edited_at, row.pass_run)
    );
    assert_eq!(cleared.meta.created_at, row.meta.created_at);
    assert!(cleared.meta.deleted && cleared.segment.speaker_id.is_none());
}

/// T14.47: a cleared row is never an erasure, whatever its own confidence was (0, -0, NaN) and
/// with or without a speaker; an erasure is one.
#[test]
fn model_segment_cleared_row_is_never_an_erasure() {
    for confidence in [0.0_f32, -0.0, f32::NAN, 0.42, 1.0] {
        for speaker in [None, Some(EntityId::from_bytes([9; 16]))] {
            let mut row = t1447_row(t1447_ts(5, 1), None, true);
            row.segment.confidence = confidence;
            row.segment.speaker_id = speaker;
            let cleared = row.cleared();
            assert!(!cleared.is_erasure(), "{confidence} {speaker:?}");
            assert!(cleared.is_cleared(), "{confidence} {speaker:?}");
            assert_eq!(cleared.segment.confidence.to_bits(), 1.0_f32.to_bits());
        }
    }
    let mut erasure = t1447_row(t1447_ts(5, 1), None, true).cleared();
    erasure.segment.confidence = 0.0;
    assert!(erasure.is_erasure());
}

/// T14.47: rule 4a's order of a cleared tombstone against an edit, a live line, another tombstone
/// and an erasure, both ways.
#[test]
fn model_segment_edit_order_of_a_cleared_tombstone() {
    use core::cmp::Ordering::{Greater, Less};
    let cleared = t1447_row(t1447_ts(200, 2), None, true).cleared();
    // An edit made elsewhere that did not see the replacement wins, whatever the stamps.
    let edit = t1447_row(t1447_ts(100, 1), Some(t1447_ts(100, 1)), false);
    assert_eq!(SegmentRow::edit_order(&cleared, &edit), Some(Greater));
    assert_eq!(SegmentRow::edit_order(&edit, &cleared), Some(Less));
    // Against an unedited live copy of the line, the later stamp wins.
    let live_older = t1447_row(t1447_ts(150, 3), None, false);
    assert_eq!(SegmentRow::edit_order(&live_older, &cleared), Some(Greater));
    assert_eq!(SegmentRow::edit_order(&cleared, &live_older), Some(Less));
    let live_later = t1447_row(t1447_ts(250, 3), None, false);
    assert_eq!(SegmentRow::edit_order(&cleared, &live_later), Some(Greater));
    // Against another tombstone (one that kept its text), the later stamp.
    let tomb = t1447_row(t1447_ts(120, 4), None, true);
    assert_eq!(SegmentRow::edit_order(&tomb, &cleared), Some(Greater));
    assert_eq!(SegmentRow::edit_order(&cleared, &tomb), Some(Less));
    // Against an erasure there is no edit order: plain stamp order decides.
    let mut erasure = cleared.clone();
    erasure.segment.confidence = 0.0;
    erasure.meta.updated_at = t1447_ts(300, 5);
    assert_eq!(SegmentRow::edit_order(&cleared, &erasure), None);
    assert_eq!(SegmentRow::edit_order(&erasure, &cleared), None);
}
