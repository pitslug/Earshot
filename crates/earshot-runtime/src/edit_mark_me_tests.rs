//! "This is me" and "This isn't me" (T9.12). Every name contains `edit`.

use std::sync::Arc;
use std::time::Duration;

use earshot_core::asr::{MeetingTime, Segment, Word};
use earshot_core::capture::TrackId;
use earshot_core::diar::{Embedding, EmbeddingModelId, SpeakerRole};
use earshot_core::identity::EntityId;
use earshot_core::model::{
    ClockDomain, Meeting, MeetingRepo, SegmentRow, Speaker, Stamp, Track, TranscriptPass,
};
use earshot_core::prompt::Template;
use earshot_core::voiceprint::{DEFAULT_ME_NAME, EnrolmentConfig, VoiceprintRepo, enroll};
use rand_chacha::ChaCha8Rng;
use rand_core::SeedableRng;

use super::*;
use crate::identities::{IdentityRecord, SpeakerIdentity, StoredIdentities, decode, encode};
use crate::testutil::{TempDir, meeting, services};

/// An in-room meeting as the final pass leaves it: one mic track, two unnamed voices with a
/// final segment and an identity entry each. Returns the meeting and the two speaker rows.
fn in_room_meeting(svc: &Services, seed: u64) -> (EntityId, EntityId, EntityId) {
    let clock = Arc::clone(&svc.clock);
    let mut rng = ChaCha8Rng::seed_from_u64(seed);
    let mut db = svc.store();
    let (store, hlc) = db.parts();
    let mut stamp = Stamp {
        clock: &*clock,
        hlc,
        rng: &mut rng,
    };
    let m = Meeting::new(
        "Workshop",
        1_792_294_800_000,
        0,
        Template::General,
        &mut stamp,
    );
    store.put_meeting(m.clone()).unwrap();
    let mic = Track::new(m.id, TrackId::Mic, None, ClockDomain::Replay, &mut stamp);
    store.put_track(mic.clone()).unwrap();
    let mut rows = Vec::new();
    let mut entries = Vec::new();
    for (n, text) in [
        (1u64, "the pump arrives monday"),
        (2, "the crane needs a permit"),
    ] {
        let row = Speaker::new(
            m.id,
            SpeakerRole::Room,
            format!("Speaker {n}"),
            None,
            &mut stamp,
        );
        store.put_speaker(row.clone()).unwrap();
        let start = n * 10_000;
        let words: Vec<Word> = text
            .split(' ')
            .enumerate()
            .map(|(i, w)| Word::new(w, start + 400 * i as u64, start + 400 * i as u64 + 300, 0.9))
            .collect();
        let mut seg = Segment::from_words(
            EntityId::generate(stamp.clock, &mut *stamp.rng),
            mic.id,
            words,
            MeetingTime::from_millis(start),
        );
        seg.speaker_id = Some(row.id);
        store
            .put_segment(SegmentRow::new(
                m.id,
                TranscriptPass::Final,
                seg,
                stamp.hlc,
                stamp.clock,
            ))
            .unwrap();
        entries.push(SpeakerIdentity {
            speaker: row.id,
            track: TrackId::Mic,
            centroid: Embedding::new(
                EmbeddingModelId::new("level@1"),
                if n == 1 {
                    vec![1.0, 0.0]
                } else {
                    vec![0.0, 1.0]
                },
            ),
            given_name: row.display_name.clone(),
            given_person: None,
            given_stamp: Some(row.meta.updated_at),
            user_edited: false,
            enrolled: false,
            ignored: false,
        });
        rows.push(row.id);
    }
    // Test set-up only: production writes this record through the runtime's registry.
    store
        .set_local_value(
            record_key(m.id).as_str(),
            &encode(&IdentityRecord { speakers: entries }),
        )
        .unwrap();
    (m.id, rows[0], rows[1])
}

fn row_of(svc: &Services, meeting: EntityId, id: EntityId) -> Speaker {
    svc.store()
        .speakers(meeting)
        .unwrap()
        .into_iter()
        .find(|s| s.id == id)
        .unwrap()
}

fn me_rows(svc: &Services, meeting: EntityId) -> Vec<EntityId> {
    view(svc, meeting)
        .speakers
        .iter()
        .filter(|s| s.role == SpeakerRole::Me)
        .map(|s| s.id)
        .collect()
}

fn entries_of(svc: &Services, meeting: EntityId) -> Vec<SpeakerIdentity> {
    match svc
        .store()
        .local_value(record_key(meeting).as_str())
        .unwrap()
        .map(|b| decode(&b))
    {
        None => Vec::new(),
        Some(StoredIdentities::Record(r)) => r.speakers,
        Some(other) => panic!("{other:?}"),
    }
}

fn entry_of(svc: &Services, meeting: EntityId, speaker: EntityId) -> SpeakerIdentity {
    entries_of(svc, meeting)
        .into_iter()
        .find(|i| i.speaker == speaker)
        .expect("the voice has an entry")
}

#[test]
fn edit_mark_me_makes_the_voice_the_users_row_under_their_name() {
    let dir = TempDir::new("edit");
    let (svc, _) = services(&dir);
    let user = new_person(&svc, "Casey", true, 901);
    let (m, voice, other) = in_room_meeting(&svc, 902);
    let before = row_of(&svc, m, voice);

    assert_eq!(mark_me(&svc, m, voice).unwrap(), user);
    let row = row_of(&svc, m, voice);
    assert_eq!(row.role, SpeakerRole::Me);
    assert_eq!(row.person_id, Some(user));
    assert_eq!(row.display_name, "Casey");
    assert!(row.meta.updated_at > before.meta.updated_at, "re-stamped");
    assert_eq!(me_rows(&svc, m), [voice]);
    // The other voice is untouched.
    assert_eq!(row_of(&svc, m, other).role, SpeakerRole::Room);
    // Its centroid stays for a re-run to match, and reads as the user's edit.
    let entry = entry_of(&svc, m, voice);
    assert!(entry.user_edited);
    assert!(entry.edited(&row));
    assert!(lines(&svc, m).contains(&("Casey".into(), "the pump arrives monday".into())));
    // A later rename of the user reaches the row, as for any Me row.
    assert_eq!(rename_person(&svc, user, "Casey Lane").unwrap(), 1);
    assert_eq!(row_of(&svc, m, voice).display_name, "Casey Lane");
}

#[test]
fn edit_mark_me_creates_the_users_person_when_there_is_none() {
    let dir = TempDir::new("edit");
    let (svc, _) = services(&dir);
    let (m, voice, _) = in_room_meeting(&svc, 905);
    assert!(list_persons(&svc).unwrap().is_empty());
    let user = mark_me(&svc, m, voice).unwrap();
    let persons = list_persons(&svc).unwrap();
    assert_eq!(persons.len(), 1);
    assert!(persons[0].is_me);
    assert_eq!(persons[0].id, user);
    assert_eq!(persons[0].name, DEFAULT_ME_NAME);
    assert_eq!(row_of(&svc, m, voice).display_name, DEFAULT_ME_NAME);
    // A second meeting uses the same person, and undoing keeps it.
    let (second, theirs, _) = in_room_meeting(&svc, 906);
    assert_eq!(mark_me(&svc, second, theirs).unwrap(), user);
    unmark_me(&svc, second, theirs).unwrap();
    assert_eq!(list_persons(&svc).unwrap().len(), 1);
}

#[test]
fn edit_mark_me_refuses_a_call_meeting_and_writes_nothing() {
    let dir = TempDir::new("edit");
    let (svc, _) = services(&dir);
    let m = meeting(&svc, "Release", 907);
    let alice = speaker(&svc, m, "Alice");
    let me = speaker(&svc, m, "Me");
    assert_eq!(refused(mark_me(&svc, m, alice.id)), EditError::NotInRoom(m));
    assert_eq!(refused(mark_me(&svc, m, me.id)), EditError::NotInRoom(m));
    assert_eq!(refused(unmark_me(&svc, m, me.id)), EditError::NotInRoom(m));
    assert_eq!(speaker(&svc, m, "Alice").meta, alice.meta);
    assert_eq!(speaker(&svc, m, "Me").meta, me.meta);
    assert!(list_persons(&svc).unwrap().is_empty());
    // Nothing is offered either.
    let v = view(&svc, m);
    for row in &v.speakers {
        assert!(!can_mark_me(&v.tracks, &v.speakers, row));
        assert!(!can_unmark_me(&v.tracks, row, None, false));
    }
}

#[test]
fn edit_mark_me_is_decided_from_the_track_rows() {
    let dir = TempDir::new("edit");
    let (svc, _) = services(&dir);
    let (m, voice, _) = in_room_meeting(&svc, 908);
    let live = view(&svc, m);
    assert!(records_in_room(&live.tracks));
    assert!(can_mark_me(&live.tracks, &live.speakers, &live.speakers[0]));
    // Computer audio alone is the far end: nobody there is the user.
    let clock = Arc::clone(&svc.clock);
    let mut rng = ChaCha8Rng::seed_from_u64(909);
    {
        let mut db = svc.store();
        let mut track = db.tracks(m).unwrap().remove(0);
        let (store, hlc) = db.parts();
        track.meta.mark_deleted(hlc, &*clock);
        store.put_track(track).unwrap();
        let mut stamp = Stamp {
            clock: &*clock,
            hlc,
            rng: &mut rng,
        };
        let t = Track::new(m, TrackId::Reference, None, ClockDomain::Replay, &mut stamp);
        store.put_track(t).unwrap();
    }
    let live = view(&svc, m);
    assert!(!records_in_room(&live.tracks));
    assert!(!can_mark_me(
        &live.tracks,
        &live.speakers,
        &live.speakers[0]
    ));
    assert_eq!(refused(mark_me(&svc, m, voice)), EditError::NotInRoom(m));
}

#[test]
fn edit_mark_me_keeps_one_me_row_per_meeting() {
    let dir = TempDir::new("edit");
    let (svc, _) = services(&dir);
    let (m, voice, other) = in_room_meeting(&svc, 910);
    mark_me(&svc, m, voice).unwrap();
    let other_before = row_of(&svc, m, other);
    let voice_before = row_of(&svc, m, voice);
    assert_eq!(
        refused(mark_me(&svc, m, other)),
        EditError::MeetingHasMe(voice)
    );
    assert_eq!(
        refused(mark_me(&svc, m, voice)),
        EditError::MeetingHasMe(voice),
        "an already-Me row is refused"
    );
    assert_eq!(me_rows(&svc, m), [voice]);
    assert_eq!(row_of(&svc, m, other).meta, other_before.meta);
    assert_eq!(row_of(&svc, m, voice).meta, voice_before.meta);
    // Linking the other voice to the user stays refused too.
    let user = row_of(&svc, m, voice).person_id.unwrap();
    assert_eq!(
        refused(link_speaker(&svc, m, other, &PersonChoice::Existing(user))),
        EditError::NotTheUser(other)
    );
    // After the undo the other voice may be marked.
    unmark_me(&svc, m, voice).unwrap();
    mark_me(&svc, m, other).unwrap();
    assert_eq!(me_rows(&svc, m), [other]);
    let v = view(&svc, m);
    let can = |id| {
        let row = v.speakers.iter().find(|s| s.id == id).unwrap();
        (
            can_mark_me(&v.tracks, &v.speakers, row),
            can_unmark_me(&v.tracks, row, None, false),
        )
    };
    assert_eq!(can(other), (false, true));
    assert_eq!(can(voice), (false, false));
}

#[test]
fn edit_unmark_me_puts_the_row_back_as_an_unnamed_voice() {
    let dir = TempDir::new("edit");
    let (svc, _) = services(&dir);
    let user = new_person(&svc, "Casey", true, 911);
    let (m, voice, other) = in_room_meeting(&svc, 912);
    assert_eq!(
        refused(unmark_me(&svc, m, voice)),
        EditError::NotMe(voice),
        "nothing to undo yet"
    );
    mark_me(&svc, m, voice).unwrap();
    let marked = row_of(&svc, m, voice);
    unmark_me(&svc, m, voice).unwrap();
    let row = row_of(&svc, m, voice);
    assert_eq!(row.role, SpeakerRole::Room);
    assert_eq!(row.person_id, None);
    assert_eq!(row.display_name, "Speaker 1");
    assert!(row.meta.updated_at > marked.meta.updated_at);
    assert!(me_rows(&svc, m).is_empty());
    // Back to what a pass would give: the next pass names it, a later edit is an edit again.
    let entry = entry_of(&svc, m, voice);
    assert!(!entry.user_edited);
    assert_eq!(entry.given_name, "Speaker 1");
    assert_eq!(entry.given_person, None);
    assert!(!entry.edited(&row));
    assert!(lines(&svc, m).contains(&("Speaker 1".into(), "the pump arrives monday".into())));
    // The user's person is still there.
    assert!(list_persons(&svc).unwrap().iter().any(|p| p.id == user));

    // The number is the smallest one free: the other voice took "Speaker 1" meanwhile.
    mark_me(&svc, m, voice).unwrap();
    assert!(rename_speaker(&svc, m, other, "Speaker 1").unwrap());
    unmark_me(&svc, m, voice).unwrap();
    assert_eq!(row_of(&svc, m, voice).display_name, "Speaker 2");
}

#[test]
fn edit_mark_me_adds_no_biometric_data() {
    let dir = TempDir::new("edit");
    let (svc, _) = services(&dir);
    let (m, voice, _) = in_room_meeting(&svc, 913);
    let centroids = |svc: &Services| {
        entries_of(svc, m)
            .into_iter()
            .map(|i| (i.speaker, i.centroid))
            .collect::<Vec<_>>()
    };
    let before = centroids(&svc);
    let user = mark_me(&svc, m, voice).unwrap();
    assert_eq!(print_count(&svc, user), None, "no print from a mark");
    assert!(svc.store().voiceprints_of(user).unwrap().is_empty());
    assert_eq!(centroids(&svc), before, "the record gains no centroid");
    // The stored centroid of a marked voice is never enrolled from.
    let any = calibrated(true);
    assert_eq!(
        refused(remember_voices_with(&svc, m, &[voice], &any)),
        EditError::MeVoice(voice)
    );
    unmark_me(&svc, m, voice).unwrap();
    assert!(svc.store().voiceprints_of(user).unwrap().is_empty());
    assert_eq!(centroids(&svc), before);

    // A user who already has a voiceprint: its count does not move.
    let (second, theirs, _) = in_room_meeting(&svc, 914);
    {
        let clock = Arc::clone(&svc.clock);
        let mut rng = ChaCha8Rng::seed_from_u64(915);
        let mut db = svc.store();
        let (store, hlc) = db.parts();
        let mut stamp = Stamp {
            clock: &*clock,
            hlc,
            rng: &mut rng,
        };
        enroll(
            &mut *store,
            user,
            &[Embedding::new(
                EmbeddingModelId::new("level@1"),
                vec![1.0, 0.0],
            )],
            Duration::from_secs(60),
            &EnrolmentConfig::default(),
            &mut stamp,
        )
        .unwrap();
    }
    assert_eq!(print_count(&svc, user), Some(1));
    mark_me(&svc, second, theirs).unwrap();
    unmark_me(&svc, second, theirs).unwrap();
    assert_eq!(print_count(&svc, user), Some(1));
}

#[test]
fn edit_mark_me_shows_the_user_in_the_view_and_export_and_keeps_written_summaries() {
    let dir = TempDir::new("edit");
    let (svc, _) = services(&dir);
    let user = new_person(&svc, "Casey Lane", true, 916);
    let (m, voice, _) = in_room_meeting(&svc, 917);
    let cited = view(&svc, m).segments[0].id;
    store(&svc, m, "The pump arrives on Monday.", 918);
    let written = view(&svc, m).summary.unwrap();

    assert_eq!(mark_me(&svc, m, voice).unwrap(), user);
    let v = view(&svc, m);
    assert_eq!(v.user_name().as_deref(), Some("Casey Lane"));
    assert!(
        v.attendees().iter().any(|a| a == "Casey Lane"),
        "{:?}",
        v.attendees()
    );
    let seg = v.segments.iter().find(|s| s.id == cited).unwrap();
    assert_eq!(v.speaker_name(seg), "Casey Lane");
    // The written summary is as it was (as with renames).
    assert_eq!(v.summary.as_ref().unwrap(), &written);
    assert_eq!(summary_history(&svc, m).unwrap().current.unwrap(), written);

    let note = exported(&svc, &dir, m, 919);
    assert!(note.contains("Casey Lane"), "{note}");
    assert!(!note.contains("Speaker 1"), "{note}");
}

#[test]
fn edit_mark_me_refuses_a_merged_away_row_a_deleted_meeting_and_a_stranger() {
    let dir = TempDir::new("edit");
    let (svc, _) = services(&dir);
    let (m, voice, other) = in_room_meeting(&svc, 920);
    merge_speakers(&svc, m, voice, other).unwrap();
    assert_eq!(
        refused(mark_me(&svc, m, other)),
        EditError::SpeakerNotFound(other)
    );
    assert_eq!(
        refused(unmark_me(&svc, m, other)),
        EditError::SpeakerNotFound(other)
    );
    let nobody = EntityId::from_bytes([8; 16]);
    assert_eq!(
        refused(mark_me(&svc, m, nobody)),
        EditError::SpeakerNotFound(nobody)
    );
    // Another meeting's row.
    let (second, theirs, _) = in_room_meeting(&svc, 921);
    assert_eq!(
        refused(mark_me(&svc, m, theirs)),
        EditError::SpeakerNotFound(theirs)
    );
    delete(&svc, second);
    assert_eq!(
        refused(mark_me(&svc, second, theirs)),
        EditError::MeetingNotFound(second)
    );
    assert!(list_persons(&svc).unwrap().is_empty(), "nothing created");
}

#[test]
fn edit_mark_me_leaves_a_person_the_user_named_after_themselves() {
    let dir = TempDir::new("edit");
    let (svc, _) = services(&dir);
    let (m, voice, _) = in_room_meeting(&svc, 922);
    let twin = link_speaker(
        &svc,
        m,
        voice,
        &PersonChoice::New {
            name: "Casey".into(),
        },
    )
    .unwrap();
    let user = mark_me(&svc, m, voice).unwrap();
    assert_ne!(user, twin);
    assert_eq!(row_of(&svc, m, voice).person_id, Some(user));
    let persons = list_persons(&svc).unwrap();
    let separate = persons.iter().find(|p| p.id == twin).unwrap();
    assert_eq!(separate.name, "Casey");
    assert!(!separate.is_me);
    // Removing that other person leaves the marked row alone; the user cannot be removed.
    remove_person(&svc, twin).unwrap();
    let row = row_of(&svc, m, voice);
    assert_eq!(row.person_id, Some(user));
    assert_eq!(row.role, SpeakerRole::Me);
    assert_eq!(
        refused(remove_person(&svc, user)),
        EditError::PersonIsTheUser(user)
    );
}

/// Gives `voice`'s row and identity entry person `person`, as a pass naming it by
/// voiceprint leaves them.
fn named_by_the_pass(svc: &Services, meeting: EntityId, voice: EntityId, person: EntityId) {
    let clock = Arc::clone(&svc.clock);
    let mut db = svc.store();
    let mut row = db
        .speakers(meeting)
        .unwrap()
        .into_iter()
        .find(|s| s.id == voice)
        .unwrap();
    let (store, hlc) = db.parts();
    row.person_id = Some(person);
    row.display_name = "Robin".into();
    row.meta.touch(hlc, &*clock);
    store.put_speaker(row.clone()).unwrap();
    let key = record_key(meeting);
    let Some(StoredIdentities::Record(mut record)) =
        store.local_value(key.as_str()).unwrap().map(|b| decode(&b))
    else {
        panic!("a record");
    };
    for i in &mut record.speakers {
        if i.speaker == voice {
            i.given_person = Some(person);
            i.given_name.clone_from(&row.display_name);
            i.given_stamp = Some(row.meta.updated_at);
        }
    }
    // Test set-up only: production writes this record through the runtime's registry.
    store
        .set_local_value(key.as_str(), &encode(&record))
        .unwrap();
}

#[test]
fn edit_mark_me_entry_is_the_users_not_the_person_the_pass_named() {
    let dir = TempDir::new("edit");
    let (svc, _) = services(&dir);
    let user = new_person(&svc, "Casey", true, 930);
    let robin = new_person(&svc, "Robin", false, 931);
    let (m, voice, other) = in_room_meeting(&svc, 932);
    named_by_the_pass(&svc, m, voice, robin);
    assert_eq!(mark_me(&svc, m, voice).unwrap(), user);
    assert_eq!(entry_of(&svc, m, voice).given_person, Some(user));
    // Removing the person the pass had named keeps the mark's entry: it is the user's now.
    remove_person(&svc, robin).unwrap();
    let entry = entry_of(&svc, m, voice);
    assert!(entry.user_edited);
    assert!(entry.edited(&row_of(&svc, m, voice)));
    assert_eq!(me_rows(&svc, m), [voice]);
    // Undo reads as the pass's own naming again, linked to nobody.
    unmark_me(&svc, m, voice).unwrap();
    assert_eq!(entry_of(&svc, m, voice).given_person, None);
    assert!(entries_of(&svc, m).iter().any(|i| i.speaker == other));
}

#[test]
fn edit_mark_me_entry_goes_when_the_user_forgets_their_voice() {
    let dir = TempDir::new("edit");
    let (svc, _) = services(&dir);
    let user = new_person(&svc, "Casey", true, 933);
    let (m, voice, other) = in_room_meeting(&svc, 934);
    assert_eq!(mark_me(&svc, m, voice).unwrap(), user);
    crate::my_voice::forget_my_voice(&svc).unwrap();
    let left: Vec<EntityId> = entries_of(&svc, m).iter().map(|i| i.speaker).collect();
    assert_eq!(
        left,
        [other],
        "the marked voice's centroid is biometric and goes"
    );
    // The mark itself, a row edit, stays.
    assert_eq!(me_rows(&svc, m), [voice]);
}

/// Enrols one voiceprint for `person` (test set-up: the explicit enrolment paths).
fn enrol_print(svc: &Services, person: EntityId, seed: u64) {
    let clock = Arc::clone(&svc.clock);
    let mut rng = ChaCha8Rng::seed_from_u64(seed);
    let mut db = svc.store();
    let (store, hlc) = db.parts();
    let mut stamp = Stamp {
        clock: &*clock,
        hlc,
        rng: &mut rng,
    };
    enroll(
        &mut *store,
        person,
        &[Embedding::new(
            EmbeddingModelId::new("level@1"),
            vec![1.0, 0.0],
        )],
        Duration::from_secs(60),
        &EnrolmentConfig::default(),
        &mut stamp,
    )
    .unwrap();
}

/// Makes `voice` the meeting's "Me" row as a final pass naming it by the user's voiceprint
/// leaves it: role Me, the user's person and name, no identity entry ("Me" is never recorded
/// by the pass) and, with `evidence`, the pass's voiceprint-named "Me" record (a pass from
/// before T12.44 left none).
fn me_by_voiceprint(
    svc: &Services,
    meeting: EntityId,
    voice: EntityId,
    user: EntityId,
    evidence: bool,
) {
    let clock = Arc::clone(&svc.clock);
    let mut db = svc.store();
    let name = db.person(user).unwrap().unwrap().display_name;
    let mut row = db
        .speakers(meeting)
        .unwrap()
        .into_iter()
        .find(|s| s.id == voice)
        .unwrap();
    let (store, hlc) = db.parts();
    row.role = SpeakerRole::Me;
    row.person_id = Some(user);
    row.display_name = name;
    row.meta.touch(hlc, &*clock);
    store.put_speaker(row).unwrap();
    let key = record_key(meeting);
    let Some(StoredIdentities::Record(mut record)) =
        store.local_value(key.as_str()).unwrap().map(|b| decode(&b))
    else {
        panic!("a record");
    };
    record.speakers.retain(|i| i.speaker != voice);
    // Test set-up only: production writes these records through the runtime's registry.
    store
        .set_local_value(key.as_str(), &encode(&record))
        .unwrap();
    if evidence {
        store
            .set_local_value(
                crate::identities::voiceprint_me_key(meeting).as_str(),
                &crate::identities::encode_voiceprint_me(voice),
            )
            .unwrap();
    }
}

fn ids(persons: &[PersonInfo]) -> Vec<EntityId> {
    persons.iter().map(|p| p.id).collect()
}

/// `recognised_me` and `can_unmark_me` of `voice`, from the store as the meeting detail
/// reads them.
fn unmark_offer(svc: &Services, meeting: EntityId, voice: EntityId) -> (bool, bool) {
    let v = view(svc, meeting);
    let row = v.speakers.iter().find(|s| s.id == voice).unwrap().clone();
    let db = svc.store();
    let store: &earshot_store::Store = &db;
    let named = voiceprint_me_in(store, meeting).unwrap();
    let has = user_has_voiceprint(store).unwrap();
    (
        recognised_me(&v.tracks, &row, named, has),
        can_unmark_me(&v.tracks, &row, named, has),
    )
}

#[test]
fn edit_mark_me_offers_a_person_named_after_the_user() {
    let dir = TempDir::new("edit");
    let (svc, _) = services(&dir);
    let (m, voice, _) = in_room_meeting(&svc, 940);
    // A user without a name of their own ("Me") has nobody named after them, even a person
    // called "Me".
    let called_me = new_person(&svc, DEFAULT_ME_NAME, false, 941);
    let user = mark_me(&svc, m, voice).unwrap();
    assert!(named_after_me(&svc).unwrap().is_empty());
    remove_person(&svc, called_me).unwrap();

    crate::my_voice::set_my_name(&svc, "Casey Lane").unwrap();
    let twin = new_person(&svc, "casey LANE", false, 942);
    new_person(&svc, "Robin", false, 943);
    new_person(&svc, "Casey", false, 944);
    let offered = named_after_me(&svc).unwrap();
    assert_eq!(ids(&offered), [twin], "the whole name, case ignored");
    assert!(offered.iter().all(|p| p.id != user && !p.is_me));
    // Merged away, it is offered no more.
    merge_into_me(&svc, twin).unwrap();
    assert!(named_after_me(&svc).unwrap().is_empty());
}

#[test]
fn edit_merge_into_me_merges_the_person_and_keeps_their_voiceprints_as_the_users() {
    let dir = TempDir::new("edit");
    let (svc, _) = services(&dir);
    let user = new_person(&svc, "Casey", true, 945);
    let (m, voice, other) = in_room_meeting(&svc, 946);
    // Before marking existed, the user named a voice after themselves and saved its voice.
    let twin = link_speaker(
        &svc,
        m,
        other,
        &PersonChoice::New {
            name: "Casey".into(),
        },
    )
    .unwrap();
    enrol_print(&svc, twin, 947);
    mark_me(&svc, m, voice).unwrap();
    assert_eq!(ids(&named_after_me(&svc).unwrap()), [twin]);

    assert_eq!(merge_into_me(&svc, twin).unwrap(), user);
    let gone = svc.store().person(twin).unwrap().unwrap();
    assert_eq!(gone.merged_into, Some(user), "merged by the identity rule");
    assert!(!gone.meta.deleted);
    let persons = list_persons(&svc).unwrap();
    assert!(persons.iter().all(|p| p.id != twin), "no longer listed");
    // The existing merge rule: the print stays on the merged person and counts as the user's.
    assert_eq!(print_count(&svc, twin), Some(1));
    assert!(svc.store().voiceprints_of(user).unwrap().is_empty());
    assert!(persons.iter().any(|p| p.id == user && p.has_voiceprint));
    // Nothing was enrolled from the marked row, and it is still the user's row.
    let row = row_of(&svc, m, voice);
    assert_eq!(row.role, SpeakerRole::Me);
    assert_eq!(row.person_id, Some(user));
    // The other row keeps its link and name; written text is not rewritten.
    let linked = row_of(&svc, m, other);
    assert_eq!(linked.person_id, Some(twin));
    assert_eq!(linked.display_name, "Casey");
    // That link stands for the user now, and renaming the user reaches the row.
    assert_eq!(resolved_person(&**svc.store(), twin).unwrap(), Some(user));
    crate::my_voice::set_my_name(&svc, "Casey Lane").unwrap();
    assert_eq!(row_of(&svc, m, other).display_name, "Casey Lane");
    // Forgetting the user's voice forgets the merged person's print too.
    assert_eq!(crate::my_voice::forget_my_voice(&svc).unwrap(), 1);
    assert_eq!(print_count(&svc, twin), None);
}

#[test]
fn edit_merge_into_me_refuses_anyone_not_named_after_the_user() {
    let dir = TempDir::new("edit");
    let (svc, _) = services(&dir);
    let robin = new_person(&svc, "Robin", false, 948);
    // No user yet: nobody is named after them.
    assert_eq!(
        refused(merge_into_me(&svc, robin)),
        EditError::NotNamedAfterUser(robin)
    );
    let user = new_person(&svc, "Casey", true, 949);
    assert_eq!(
        refused(merge_into_me(&svc, robin)),
        EditError::NotNamedAfterUser(robin)
    );
    assert_eq!(
        refused(merge_into_me(&svc, user)),
        EditError::NotNamedAfterUser(user)
    );
    let nobody = EntityId::from_bytes([9; 16]);
    assert_eq!(
        refused(merge_into_me(&svc, nobody)),
        EditError::PersonNotFound(nobody)
    );
    let twin = new_person(&svc, "Casey", false, 950);
    merge_into_me(&svc, twin).unwrap();
    assert_eq!(
        refused(merge_into_me(&svc, twin)),
        EditError::PersonNotFound(twin),
        "already merged"
    );
    assert!(svc.store().person(robin).unwrap().unwrap().is_live());
}

#[test]
fn edit_unmark_me_refuses_a_me_row_the_users_voiceprint_named() {
    let dir = TempDir::new("edit");
    let (svc, _) = services(&dir);
    let user = new_person(&svc, "Casey", true, 951);
    enrol_print(&svc, user, 952);
    let (m, voice, other) = in_room_meeting(&svc, 953);
    me_by_voiceprint(&svc, m, voice, user, true);
    assert_eq!(unmark_offer(&svc, m, voice), (true, false));
    let before = row_of(&svc, m, voice);
    assert_eq!(
        refused(unmark_me(&svc, m, voice)),
        EditError::RecognisedMe(voice)
    );
    assert_eq!(row_of(&svc, m, voice).meta, before.meta, "nothing written");

    // Once the user's voice is forgotten, no voiceprint names it again: undo is allowed, and
    // the row is the user's from then on (re-recording the voice does not take it back).
    crate::my_voice::forget_my_voice(&svc).unwrap();
    assert_eq!(unmark_offer(&svc, m, voice), (false, true));
    unmark_me(&svc, m, voice).unwrap();
    assert_eq!(row_of(&svc, m, voice).role, SpeakerRole::Room);
    enrol_print(&svc, user, 954);
    mark_me(&svc, m, voice).unwrap();
    assert_eq!(unmark_offer(&svc, m, voice), (false, true));
    unmark_me(&svc, m, voice).unwrap();

    // A voice the user marked is theirs to undo, voiceprint or not.
    mark_me(&svc, m, other).unwrap();
    assert_eq!(unmark_offer(&svc, m, other), (false, true));
    unmark_me(&svc, m, other).unwrap();
}

#[test]
fn edit_mark_then_forget_then_record_the_voice_again_is_not_recognised() {
    let dir = TempDir::new("edit");
    let (svc, _) = services(&dir);
    let user = new_person(&svc, "Casey", true, 955);
    let (m, voice, _) = in_room_meeting(&svc, 956);
    mark_me(&svc, m, voice).unwrap();
    // Forgetting drops the mark's identity entry (biometric); the mark stays.
    crate::my_voice::forget_my_voice(&svc).unwrap();
    assert!(entries_of(&svc, m).iter().all(|i| i.speaker != voice));
    enrol_print(&svc, user, 957);
    assert_eq!(unmark_offer(&svc, m, voice), (false, true));
    unmark_me(&svc, m, voice).unwrap();
}

#[test]
fn edit_recognised_me_fails_open_without_readable_evidence() {
    let dir = TempDir::new("edit");
    let (svc, _) = services(&dir);
    let user = new_person(&svc, "Casey", true, 958);
    enrol_print(&svc, user, 959);
    // A voiceprint "Me" from a pass that left no record (before T12.44): allowed.
    let (m, voice, _) = in_room_meeting(&svc, 960);
    me_by_voiceprint(&svc, m, voice, user, false);
    assert_eq!(unmark_offer(&svc, m, voice), (false, true));
    // An unreadable record: allowed too.
    let (second, theirs, _) = in_room_meeting(&svc, 961);
    me_by_voiceprint(&svc, second, theirs, user, false);
    // Test set-up only: production writes this record through the runtime's registry.
    svc.store()
        .set_local_value(
            crate::identities::voiceprint_me_key(second).as_str(),
            b"\x09\x00\x00\x00garbage",
        )
        .unwrap();
    assert_eq!(unmark_offer(&svc, second, theirs), (false, true));
    unmark_me(&svc, second, theirs).unwrap();
    unmark_me(&svc, m, voice).unwrap();
}
