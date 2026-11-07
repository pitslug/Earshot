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
